use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use restowa_core::config::Config;
use restowa_core::doctor::{run_doctor, CheckStatus};
use restowa_core::hooks::{
    handle_claude_hook, install_claude_hooks, uninstall_claude_hooks, ClaudeHookEvent,
};
use restowa_core::launchd::{
    install_launchd_agent, path_points_into_target, uninstall_launchd_agent,
};
use restowa_core::logging::init_file_logger;
use restowa_core::model::{Session, SessionStatus, Tool};
use restowa_core::paths::AppPaths;
use restowa_core::platform::{current_platform, Platform};
use restowa_core::registry::{resolve_key_in_slice, Registry};
use restowa_core::restore::{run_restore, RestoreOptions};
use restowa_core::scanner::{load_merged_sessions, run_scan};
use restowa_core::shell::shell_escape;
use restowa_core::terminal::select_backend;
use std::io::Read;
use std::process::{Command, ExitCode};
use std::time::{SystemTime, UNIX_EPOCH};

const EXIT_OK: u8 = 0;
const EXIT_ERR: u8 = 1;
const EXIT_NOTHING: u8 = 2;

#[derive(Debug, Parser)]
#[command(
    name = "restowa",
    version,
    about = "Keep a live registry of AI coding-agent sessions on macOS and restore them after reboot"
)]
struct Cli {
    /// Emit machine-readable JSON output
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// List recorded and live agent sessions
    List(ListArgs),
    /// Reopen sessions in terminal tabs
    Restore(RestoreArgs),
    /// Run one stateless scan pass
    Scan(ScanArgs),
    /// Handle lifecycle hook events from agent CLIs (reads JSON on stdin)
    Hook(HookArgs),
    /// Pin a session so it is always included in restore sets
    Pin { key: String },
    /// Unpin a previously pinned session
    Unpin { key: String },
    /// Set a custom title for a session
    Rename { key: String, title: String },
    /// Hide a session from restowa (never deletes tool data)
    Forget { key: String },
    /// Bring a running session's Ghostty or Terminal.app tab to the front
    Focus { key: String },
    /// Install hooks and/or launchd agent
    Install(InstallArgs),
    /// Reverse install (keeps registry unless --purge)
    Uninstall(UninstallArgs),
    /// Check tools, hooks, PID files, registry, and terminal backends
    Doctor,
    /// Print or edit config.toml
    Config(ConfigArgs),
}

#[derive(Debug, Args)]
struct ListArgs {
    /// Show only live sessions (default)
    #[arg(long, conflicts_with_all = ["last_boot", "pinned", "all"])]
    live: bool,
    /// Show sessions from the previous boot unioned with pinned sessions
    #[arg(long, conflicts_with_all = ["live", "pinned", "all"])]
    last_boot: bool,
    /// Show pinned sessions
    #[arg(long, conflicts_with_all = ["live", "last_boot", "all"])]
    pinned: bool,
    /// Show all non-hidden sessions
    #[arg(long, conflicts_with_all = ["live", "last_boot", "pinned"])]
    all: bool,
    /// Filter by tool (claude, codex, opencode, antigravity)
    #[arg(long)]
    tool: Option<Tool>,
}

#[derive(Debug, Args)]
struct RestoreArgs {
    /// Specific session keys or unique ID prefixes to restore
    keys: Vec<String>,
    /// Restore sessions that were live in the previous boot (+ pinned)
    #[arg(long)]
    last_boot: bool,
    /// Override terminal backend (ghostty, terminal)
    #[arg(long)]
    terminal: Option<String>,
    /// Print commands without opening terminal windows or tabs
    #[arg(long)]
    dry_run: bool,
}

#[derive(Debug, Args)]
struct ScanArgs {
    /// Print scan summary (silent by default)
    #[arg(short, long)]
    verbose: bool,
}

#[derive(Debug, Args)]
struct HookArgs {
    #[command(subcommand)]
    tool: HookTool,
}

#[derive(Debug, Subcommand)]
enum HookTool {
    /// Handle Claude Code hooks
    Claude {
        #[arg(value_enum)]
        event: ClaudeHookArg,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ClaudeHookArg {
    SessionStart,
    SessionEnd,
}

#[derive(Debug, Args)]
struct InstallArgs {
    /// Merge Claude Code hooks into ~/.claude/settings.json (with backup)
    #[arg(long)]
    hooks: bool,
    /// Install and load the launchd scanner agent
    #[arg(long)]
    launchd: bool,
    /// Install both hooks and launchd agent
    #[arg(long)]
    all: bool,
    /// Override the restowa binary command written into hook configs
    #[arg(long)]
    bin: Option<String>,
}

#[derive(Debug, Args)]
struct UninstallArgs {
    /// Also delete the restowa registry database
    #[arg(long)]
    purge: bool,
}

#[derive(Debug, Args)]
struct ConfigArgs {
    #[command(subcommand)]
    action: Option<ConfigAction>,
}

#[derive(Debug, Subcommand)]
enum ConfigAction {
    /// Print path to config.toml
    Path,
    /// Open config.toml in $EDITOR
    Edit,
    /// Update config.toml values (used by the menu bar Settings window and scripts)
    Set {
        /// Default terminal backend (ghostty, terminal)
        #[arg(long)]
        terminal: Option<String>,
        /// Background scan interval in seconds (e.g. 60, 120, 300)
        #[arg(long)]
        scan_interval: Option<u64>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    // Hook commands must NEVER write to stdout and must ALWAYS exit 0 so they
    // never block or pollute Claude Code's context window.
    if let Commands::Hook(ref hook_args) = cli.command {
        run_hook_safe(hook_args);
        return ExitCode::from(EXIT_OK);
    }

    match run(cli) {
        Ok(code) => ExitCode::from(code),
        Err(err) => {
            log::error!("{err:#}");
            eprintln!("error: {err:#}");
            ExitCode::from(EXIT_ERR)
        }
    }
}

fn run_hook_safe(args: &HookArgs) {
    let Ok(paths) = AppPaths::resolve() else {
        return;
    };
    init_file_logger(paths.log_file.clone(), false);

    let mut stdin_buf = String::new();
    if let Err(err) = std::io::stdin().read_to_string(&mut stdin_buf) {
        log::error!("hook failed reading stdin: {err}");
        return;
    }

    let platform = current_platform();
    match args.tool {
        HookTool::Claude { event } => {
            let ev = match event {
                ClaudeHookArg::SessionStart => ClaudeHookEvent::SessionStart,
                ClaudeHookArg::SessionEnd => ClaudeHookEvent::SessionEnd,
            };
            if let Err(err) = handle_claude_hook(&paths, &platform, ev, &stdin_buf) {
                log::error!("claude hook ({ev:?}) error: {err}");
            }
        }
    }
}

fn run(cli: Cli) -> Result<u8> {
    let paths = AppPaths::resolve()?;
    let verbose = matches!(&cli.command, Commands::Scan(s) if s.verbose);
    init_file_logger(paths.log_file.clone(), verbose);
    let config = Config::load(&paths.config_file)?;
    let platform = current_platform();

    match cli.command {
        Commands::List(args) => cmd_list(&paths, &config, &platform, &args, cli.json),
        Commands::Restore(args) => cmd_restore(&paths, &config, &platform, &args, cli.json),
        Commands::Scan(args) => cmd_scan(&paths, &platform, &args, cli.json),
        Commands::Hook(_) => Ok(EXIT_OK),
        Commands::Pin { key } => cmd_mutate_key(&paths, &key, cli.json, |reg, k| {
            reg.set_pinned(k, true)?;
            Ok("pinned")
        }),
        Commands::Unpin { key } => cmd_mutate_key(&paths, &key, cli.json, |reg, k| {
            reg.set_pinned(k, false)?;
            Ok("unpinned")
        }),
        Commands::Rename { key, title } => cmd_mutate_key(&paths, &key, cli.json, |reg, k| {
            reg.set_title(k, &title)?;
            Ok("renamed")
        }),
        Commands::Forget { key } => cmd_mutate_key(&paths, &key, cli.json, |reg, k| {
            reg.set_hidden(k, true)?;
            Ok("forgotten")
        }),
        Commands::Focus { key } => cmd_focus(&paths, &platform, &key, cli.json),
        Commands::Install(args) => cmd_install(&paths, &config, &args, cli.json),
        Commands::Uninstall(args) => cmd_uninstall(&paths, &args, cli.json),
        Commands::Doctor => cmd_doctor(&paths, &platform, cli.json),
        Commands::Config(args) => cmd_config(&paths, &config, &args, cli.json),
    }
}

fn cmd_list(
    paths: &AppPaths,
    config: &Config,
    platform: &dyn Platform,
    args: &ListArgs,
    json: bool,
) -> Result<u8> {
    let mut sessions = if args.last_boot {
        match Registry::open_read_only(&paths.registry_db)? {
            Some(reg) => {
                let current_boot = platform.boot_id()?;
                let prev = reg.previous_boot_id(&current_boot, config.scan_interval_secs)?;
                reg.list_last_boot(prev.as_deref(), config.scan_interval_secs)?
            }
            None => Vec::new(),
        }
    } else if args.pinned {
        match Registry::open_read_only(&paths.registry_db)? {
            Some(reg) => reg.list_pinned()?,
            None => Vec::new(),
        }
    } else {
        let merged = load_merged_sessions(paths, platform, false)?;
        if args.all {
            merged
        } else {
            // Default: --live
            merged
                .into_iter()
                .filter(|s| s.status == SessionStatus::Live)
                .collect()
        }
    };

    if let Some(tool) = args.tool {
        sessions.retain(|s| s.tool == tool);
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&sessions)?);
        return Ok(EXIT_OK);
    }

    if sessions.is_empty() {
        println!("No sessions found.");
        return Ok(EXIT_NOTHING);
    }

    print_sessions_table(&sessions);
    Ok(EXIT_OK)
}

fn print_sessions_table(sessions: &[Session]) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    println!(
        "{:<10} {:<12} {:<20} {:<36} {:<8} SEEN",
        "KEY", "TOOL", "PROJECT", "TITLE", "STATUS"
    );
    for s in sessions {
        let project = truncate_cell(s.project_name(), 19);
        let title_raw = s.title.as_deref().unwrap_or("-");
        let title = truncate_cell(title_raw, 35);
        let seen_ts = s.last_seen_live.unwrap_or(s.first_seen);
        let seen = format_relative_age(now.saturating_sub(seen_ts));
        let mut status = s.status.to_string();
        if s.pinned {
            status.push('*');
        }
        if s.best_guess {
            status.push('?');
        }
        println!(
            "{:<10} {:<12} {:<20} {:<36} {:<8} {}",
            s.short_id(),
            s.tool,
            project,
            title,
            status,
            seen
        );
    }
}

fn truncate_cell(s: &str, max_chars: usize) -> String {
    let mut chars = s.chars();
    let head: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() && max_chars > 1 {
        let mut out: String = head.chars().take(max_chars - 1).collect();
        out.push('…');
        out
    } else {
        head
    }
}

fn format_relative_age(secs: i64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

fn cmd_restore(
    paths: &AppPaths,
    config: &Config,
    platform: &dyn Platform,
    args: &RestoreArgs,
    json: bool,
) -> Result<u8> {
    let report = run_restore(
        paths,
        config,
        platform,
        &RestoreOptions {
            keys: args.keys.clone(),
            last_boot: args.last_boot,
            terminal: args.terminal.clone(),
            dry_run: args.dry_run,
        },
    )?;

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else if report.dry_run {
        if report.restored.is_empty() && report.failed.is_empty() {
            println!("Nothing to restore.");
        } else {
            println!(
                "Dry run (terminal: {}): {} session(s)",
                report.terminal,
                report.restored.len()
            );
            for item in &report.restored {
                let suffix = if item.best_guess { " (best guess)" } else { "" };
                println!("  [{}]{suffix} {}", item.key, item.shell_command);
            }
            for s in &report.skipped {
                println!("  note: {} ({})", s.key, s.reason);
            }
            for f in &report.failed {
                println!("  failed: {} ({}) — {}", f.key, f.cwd, f.reason);
            }
        }
    } else {
        println!(
            "Restored {} session(s) in {} (skipped: {}, failed: {})",
            report.restored.len(),
            report.terminal,
            report.skipped.len(),
            report.failed.len()
        );
        for item in &report.restored {
            let suffix = if item.best_guess { " (best guess)" } else { "" };
            println!("  restored: {} ({}){suffix}", item.key, item.cwd);
        }
        for s in &report.skipped {
            println!("  skipped:  {} — {}", s.key, s.reason);
        }
        for f in &report.failed {
            println!("  failed:   {} ({}) — {}", f.key, f.cwd, f.reason);
        }
    }

    if !report.failed.is_empty() && report.restored.is_empty() {
        Ok(EXIT_ERR)
    } else if report.restored.is_empty() {
        Ok(EXIT_NOTHING)
    } else {
        Ok(EXIT_OK)
    }
}

fn cmd_scan(paths: &AppPaths, platform: &dyn Platform, args: &ScanArgs, json: bool) -> Result<u8> {
    let report = run_scan(paths, platform)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else if args.verbose {
        println!(
            "scan complete: boot={} spool={} procs={} live={} closed={} missing={}",
            report.boot_id,
            report.spool_imported,
            report.processes_inspected,
            report.live_found,
            report.marked_closed,
            report.marked_missing
        );
    }
    Ok(EXIT_OK)
}

fn cmd_mutate_key<F>(paths: &AppPaths, key: &str, json: bool, op: F) -> Result<u8>
where
    F: FnOnce(&mut Registry, &str) -> restowa_core::error::Result<&'static str>,
{
    let mut reg = Registry::open(&paths.registry_db)?;
    let session = reg.resolve_key(key)?;
    let action = op(&mut reg, &session.key)?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "key": session.key,
                "action": action
            }))?
        );
    } else {
        println!("{action}: {}", session.key);
    }
    Ok(EXIT_OK)
}

fn cmd_focus(paths: &AppPaths, platform: &dyn Platform, key: &str, json: bool) -> Result<u8> {
    let sessions = load_merged_sessions(paths, platform, false)?;
    let session = resolve_key_in_slice(&sessions, key)?;
    if session.status != SessionStatus::Live {
        anyhow::bail!(
            "{} is not running; reopen it with `restowa restore {}`",
            session.key,
            session.key
        );
    }
    let pid = session
        .pid
        .context("running session has no recorded process")?;
    let tty = platform
        .process_tty(pid as i32)
        .with_context(|| format!("process {pid} has no terminal"))?;
    for name in ["ghostty", "terminal"] {
        let backend = select_backend(name, paths)?;
        if !backend.focus_tty(&tty)? {
            continue;
        }
        if json {
            let out = serde_json::json!({ "key": session.key, "terminal": name, "tty": tty });
            println!("{}", serde_json::to_string_pretty(&out)?);
        } else {
            println!("focused: {} ({name}, {tty})", session.key);
        }
        return Ok(EXIT_OK);
    }
    anyhow::bail!("no Ghostty or Terminal.app tab found for {tty}")
}

fn resolve_hook_command_bin(explicit: Option<&str>) -> String {
    if let Some(bin) = explicit {
        return maybe_escape_bin(bin);
    }
    // Prefer bare `restowa` when installed on PATH (e.g. Homebrew).
    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            if dir.join("restowa").is_file() {
                return "restowa".to_string();
            }
        }
    }
    // Fall back to the running binary path so local builds work before Homebrew install.
    if let Ok(exe) = std::env::current_exe() {
        return maybe_escape_bin(&exe.to_string_lossy());
    }
    "restowa".to_string()
}

fn maybe_escape_bin(bin: &str) -> String {
    if bin
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '+'))
    {
        bin.to_string()
    } else {
        shell_escape(bin)
    }
}

fn resolve_launchd_binary_path(explicit: Option<&str>) -> Result<std::path::PathBuf> {
    if let Some(bin) = explicit {
        return Ok(absolute_keeping_symlinks(std::path::Path::new(bin)));
    }
    if let Some(found) = std::env::var_os("PATH").and_then(|p| find_installed_on_path(&p)) {
        return Ok(found);
    }
    let exe = std::env::current_exe().context("failed to determine current executable path")?;
    Ok(exe.canonicalize().unwrap_or(exe))
}

/// First `restowa` on `path_var` that does not live in a cargo `target/` dir.
fn find_installed_on_path(path_var: &std::ffi::OsStr) -> Option<std::path::PathBuf> {
    std::env::split_paths(path_var)
        .map(|dir| dir.join("restowa"))
        .filter(|candidate| candidate.is_file())
        .find(|candidate| {
            let resolved = candidate
                .canonicalize()
                .unwrap_or_else(|_| candidate.clone());
            !path_points_into_target(&resolved.to_string_lossy())
        })
        .map(|candidate| absolute_keeping_symlinks(&candidate))
}

/// Absolute paths are kept as given. Canonicalising would turn Homebrew's
/// stable `bin/restowa` symlink into a versioned Cellar path that the next
/// `brew upgrade` deletes, silently breaking the launchd agent. Relative
/// paths are canonicalised because launchd needs an absolute one.
fn absolute_keeping_symlinks(path: &std::path::Path) -> std::path::PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn cmd_install(paths: &AppPaths, config: &Config, args: &InstallArgs, json: bool) -> Result<u8> {
    let want_hooks = args.hooks || args.all || !args.launchd;
    let want_launchd = args.launchd || args.all;

    let hook_report = if want_hooks {
        let bin = resolve_hook_command_bin(args.bin.as_deref());
        Some(install_claude_hooks(
            &paths.claude_settings(),
            &paths.claude_settings_backup(),
            &bin,
        )?)
    } else {
        None
    };

    let launchd_report = if want_launchd {
        let abs_bin = resolve_launchd_binary_path(args.bin.as_deref())?;
        Some(install_launchd_agent(
            paths,
            &abs_bin,
            config.scan_interval_secs,
            true,
        )?)
    } else {
        None
    };

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "hooks": hook_report,
                "launchd": launchd_report
            }))?
        );
    } else {
        if let Some(r) = hook_report {
            if let Some(bak) = &r.backup_path {
                println!("Backed up existing settings to {bak}");
            }
            println!(
                "Claude Code hooks updated in {} (SessionStart: {}, SessionEnd: {})",
                r.settings_path,
                if r.added_start {
                    "added"
                } else {
                    "already present"
                },
                if r.added_end {
                    "added"
                } else {
                    "already present"
                }
            );
            println!("  SessionStart: {}", r.command_start);
            println!("  SessionEnd:   {}", r.command_end);
        }
        if let Some(lr) = launchd_report {
            println!(
                "Installed launchd agent at {} (binary: {}, interval: {}s, loaded: {})",
                lr.plist_path, lr.binary_path, lr.interval_secs, lr.loaded
            );
            if lr.points_into_target {
                eprintln!(
                    "warning: launchd plist points into target/ ({}); install restowa outside target/ and re-run `restowa install --launchd`",
                    lr.binary_path
                );
            }
        }
    }

    Ok(EXIT_OK)
}

fn cmd_uninstall(paths: &AppPaths, args: &UninstallArgs, json: bool) -> Result<u8> {
    let hook_report = uninstall_claude_hooks(&paths.claude_settings())?;
    let launchd_report = uninstall_launchd_agent(paths, true)?;
    let mut purged = false;
    if args.purge && paths.registry_db.exists() {
        std::fs::remove_file(&paths.registry_db)?;
        let _ = std::fs::remove_file(paths.registry_db.with_extension("db-wal"));
        let _ = std::fs::remove_file(paths.registry_db.with_extension("db-shm"));
        let _ = std::fs::remove_file(&paths.spool_file);
        purged = true;
    }

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "hooks": hook_report,
                "launchd": launchd_report,
                "registry_purged": purged
            }))?
        );
    } else {
        println!(
            "Removed {} restowa hook entry/entries from {}",
            hook_report.removed_count, hook_report.settings_path
        );
        if launchd_report.removed {
            println!(
                "Removed launchd agent {} (unloaded: {})",
                launchd_report.plist_path, launchd_report.unloaded
            );
        }
        if purged {
            println!("Purged registry at {}", paths.registry_db.display());
        }
    }
    Ok(EXIT_OK)
}

fn cmd_doctor(paths: &AppPaths, platform: &dyn Platform, json: bool) -> Result<u8> {
    let report = run_doctor(paths, platform);
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        for c in &report.checks {
            let badge = match c.status {
                CheckStatus::Ok => "[OK]  ",
                CheckStatus::Warn => "[WARN]",
                CheckStatus::Error => "[ERR] ",
            };
            println!("{badge} {:<20} {}", c.name, c.detail);
        }
    }
    if report.has_errors {
        Ok(EXIT_ERR)
    } else {
        Ok(EXIT_OK)
    }
}

fn cmd_config(paths: &AppPaths, config: &Config, args: &ConfigArgs, json: bool) -> Result<u8> {
    match args.action {
        Some(ConfigAction::Path) => {
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "path": paths.config_file
                    }))?
                );
            } else {
                println!("{}", paths.config_file.display());
            }
            Ok(EXIT_OK)
        }
        Some(ConfigAction::Edit) => {
            if !paths.config_file.exists() {
                config.save(&paths.config_file)?;
            }
            let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
            let status = Command::new(&editor)
                .arg(&paths.config_file)
                .status()
                .with_context(|| format!("failed to launch editor '{editor}'"))?;
            if status.success() {
                Ok(EXIT_OK)
            } else {
                Ok(EXIT_ERR)
            }
        }
        Some(ConfigAction::Set {
            ref terminal,
            scan_interval,
        }) => {
            let mut updated = config.clone();
            if let Some(term) = terminal {
                match term.as_str() {
                    "ghostty" | "terminal" => updated.terminal = term.clone(),
                    other => anyhow::bail!(
                        "unsupported terminal '{other}' (expected 'ghostty' or 'terminal')"
                    ),
                }
            }
            let interval_changed = if let Some(secs) = scan_interval {
                if secs < 30 {
                    anyhow::bail!("scan interval must be at least 30 seconds");
                }
                let changed = updated.scan_interval_secs != secs;
                updated.scan_interval_secs = secs;
                changed
            } else {
                false
            };
            // Reinstall the agent before saving so a launchd failure leaves config untouched.
            if interval_changed {
                let status = restowa_core::launchd::inspect_launchd_agent(paths, false);
                if status.plist_exists {
                    let bin_path = match status.program_path {
                        Some(p) => std::path::PathBuf::from(p),
                        None => resolve_launchd_binary_path(None)?,
                    };
                    install_launchd_agent(paths, &bin_path, updated.scan_interval_secs, true)
                        .context(
                        "failed to reload launchd agent with new scan interval; config not saved",
                    )?;
                }
            }
            updated.save(&paths.config_file)?;

            if json {
                println!("{}", serde_json::to_string_pretty(&updated)?);
            } else {
                println!("{}", updated.to_toml_string()?);
            }
            Ok(EXIT_OK)
        }
        None => {
            if json {
                println!("{}", serde_json::to_string_pretty(config)?);
            } else {
                println!("{}", config.to_toml_string()?);
            }
            Ok(EXIT_OK)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn scratch_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("restowa-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir); // Leftover from an earlier run, if any.
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn path_lookup_keeps_homebrew_symlink_instead_of_cellar_path() {
        let root = scratch_dir("brew");
        let cellar_bin = root.join("Cellar/restowa/0.1.0/bin");
        let brew_bin = root.join("bin");
        std::fs::create_dir_all(&cellar_bin).unwrap();
        std::fs::create_dir_all(&brew_bin).unwrap();
        std::fs::write(cellar_bin.join("restowa"), b"").unwrap();
        symlink(cellar_bin.join("restowa"), brew_bin.join("restowa")).unwrap();

        let found = find_installed_on_path(brew_bin.as_os_str());
        assert_eq!(found, Some(brew_bin.join("restowa")));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn path_lookup_skips_cargo_target_dirs() {
        let root = scratch_dir("target");
        let target_bin = root.join("target/release");
        let installed = root.join("installed");
        std::fs::create_dir_all(&target_bin).unwrap();
        std::fs::create_dir_all(&installed).unwrap();
        std::fs::write(target_bin.join("restowa"), b"").unwrap();
        std::fs::write(installed.join("restowa"), b"").unwrap();

        let path_var = std::env::join_paths([&target_bin, &installed]).unwrap();
        assert_eq!(
            find_installed_on_path(&path_var),
            Some(installed.join("restowa"))
        );
        std::fs::remove_dir_all(&root).unwrap();
    }
}
