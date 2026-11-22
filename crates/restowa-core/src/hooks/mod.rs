use crate::adapters::claude::ClaudeAdapter;
use crate::adapters::Adapter;
use crate::error::{CoreError, Result};
use crate::model::Tool;
use crate::paths::AppPaths;
use crate::platform::Platform;
use crate::registry::{HookSessionUpsert, Registry};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const START_SUBCMD: &str = "hook claude session-start";
const END_SUBCMD: &str = "hook claude session-end";
const HOOK_BUSY_TIMEOUT: Duration = Duration::from_millis(150);
const MAX_ANCESTOR_DEPTH: usize = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ClaudeHookEvent {
    SessionStart,
    SessionEnd,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ClaudeHookInput {
    pub session_id: String,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub transcript_path: Option<String>,
    #[serde(default)]
    pub hook_event_name: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

/// NDJSON event written to `~/Library/Application Support/restowa/spool.jsonl`
/// when the hook cannot acquire the SQLite write lock within `HOOK_BUSY_TIMEOUT`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpoolEvent {
    pub event: ClaudeHookEvent,
    pub tool: Tool,
    pub session_id: String,
    pub cwd: String,
    pub transcript_path: Option<String>,
    pub reason: Option<String>,
    pub pid: Option<i64>,
    pub pid_start_us: Option<i64>,
    pub boot_id: String,
    pub now: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct HookInstallReport {
    pub settings_path: String,
    pub backup_path: Option<String>,
    pub added_start: bool,
    pub added_end: bool,
    pub command_start: String,
    pub command_end: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct HookUninstallReport {
    pub settings_path: String,
    pub removed_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct HookStatus {
    pub settings_exists: bool,
    pub start_installed: bool,
    pub end_installed: bool,
    pub parse_error: Option<String>,
}

/// Handle a Claude Code lifecycle hook from stdin JSON.
///
/// - Skips headless invocations when any ancestor `argv` contains `-p` or `--print`.
/// - Uses a short ~150 ms SQLite `busy_timeout`; on write failure, appends the
///   event to `spool.jsonl` so `restowa scan` can replay it later.
/// - Never writes to stdout; callers always exit `0`.
pub fn handle_claude_hook(
    paths: &AppPaths,
    platform: &dyn Platform,
    event: ClaudeHookEvent,
    stdin_json: &str,
) -> Result<()> {
    let payload: ClaudeHookInput = serde_json::from_str(stdin_json)?;
    if payload.session_id.is_empty() {
        return Ok(());
    }

    let ancestor = inspect_ancestors(paths, platform);
    if ancestor.is_headless {
        log::debug!(
            "skipping claude hook for headless invocation (session_id={})",
            payload.session_id
        );
        return Ok(());
    }

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let boot_id = platform.boot_id().unwrap_or_else(|_| "unknown".to_string());

    let spool_event = SpoolEvent {
        event,
        tool: Tool::Claude,
        session_id: payload.session_id,
        cwd: payload.cwd,
        transcript_path: payload.transcript_path,
        reason: payload.reason,
        pid: ancestor.pid,
        pid_start_us: ancestor.pid_start_us,
        boot_id,
        now,
    };

    match apply_hook_event_direct(&paths.registry_db, &spool_event) {
        Ok(()) => Ok(()),
        Err(err) => {
            log::warn!("hook DB write failed ({err}); appending to spool.jsonl");
            append_spool_event(&paths.spool_file, &spool_event)?;
            Ok(())
        }
    }
}

fn apply_hook_event_direct(db_path: &Path, ev: &SpoolEvent) -> Result<()> {
    let mut registry = Registry::open_with_timeout(db_path, HOOK_BUSY_TIMEOUT)?;
    apply_spool_event_to_registry(&mut registry, ev)
}

fn apply_spool_event_to_registry(registry: &mut Registry, ev: &SpoolEvent) -> Result<()> {
    match ev.event {
        ClaudeHookEvent::SessionStart => {
            registry.upsert_hook_start(&HookSessionUpsert {
                tool: ev.tool,
                session_id: ev.session_id.clone(),
                cwd: ev.cwd.clone(),
                transcript: ev.transcript_path.clone(),
                pid: ev.pid,
                pid_start_us: ev.pid_start_us,
                boot_id: ev.boot_id.clone(),
                now: ev.now,
            })?;
        }
        ClaudeHookEvent::SessionEnd => {
            let exclude_from_last_boot = ev.reason.as_deref() == Some("clear");
            registry.record_hook_end(
                ev.tool,
                &ev.session_id,
                &ev.cwd,
                ev.transcript_path.as_deref(),
                &ev.boot_id,
                ev.now,
                exclude_from_last_boot,
            )?;
        }
    }
    Ok(())
}

fn append_spool_event(spool_path: &Path, ev: &SpoolEvent) -> Result<()> {
    AppPaths::ensure_parent(spool_path)?;
    let line = serde_json::to_string(ev)?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(spool_path)
        .map_err(|source| CoreError::IoPath {
            path: spool_path.to_path_buf(),
            source,
        })?;
    writeln!(file, "{line}").map_err(|source| CoreError::IoPath {
        path: spool_path.to_path_buf(),
        source,
    })?;
    Ok(())
}

/// Import all pending events from `spool.jsonl` into the registry and clear
/// the spool file. Uses an atomic rename before reading so concurrent hooks
/// appending to `spool.jsonl` are never dropped.
pub fn replay_spool(paths: &AppPaths, registry: &mut Registry) -> Result<usize> {
    if !paths.spool_file.exists() {
        return Ok(0);
    }

    let draining = paths
        .spool_file
        .with_extension(format!("jsonl.draining.{}", std::process::id()));
    if let Err(err) = std::fs::rename(&paths.spool_file, &draining) {
        if err.kind() == std::io::ErrorKind::NotFound {
            return Ok(0);
        }
        return Err(CoreError::IoPath {
            path: paths.spool_file.clone(),
            source: err,
        });
    }

    let raw = std::fs::read_to_string(&draining).map_err(|source| CoreError::IoPath {
        path: draining.clone(),
        source,
    })?;

    let mut imported = 0usize;
    let mut failed_lines: Vec<&str> = Vec::new();

    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<SpoolEvent>(trimmed) {
            Ok(ev) => match apply_spool_event_to_registry(registry, &ev) {
                Ok(()) => imported += 1,
                Err(err) => {
                    log::warn!("failed importing spool event: {err}");
                    failed_lines.push(trimmed);
                }
            },
            Err(err) => {
                log::warn!("discarding malformed spool line: {err}");
            }
        }
    }

    let _ = std::fs::remove_file(&draining);

    if !failed_lines.is_empty() {
        AppPaths::ensure_parent(&paths.spool_file)?;
        if let Ok(mut file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&paths.spool_file)
        {
            for line in failed_lines {
                let _ = writeln!(file, "{line}");
            }
        }
    }

    Ok(imported)
}

#[derive(Debug, Default)]
struct AncestorInspection {
    pid: Option<i64>,
    pid_start_us: Option<i64>,
    is_headless: bool,
}

/// Walk up ancestor processes from `getppid()` to:
/// 1. Detect headless `-p` / `--print` invocations on any ancestor `argv`.
/// 2. Locate the invoking Claude Code process PID + start time (walking past
///    any `/bin/sh -c` hook wrapper).
fn inspect_ancestors(paths: &AppPaths, platform: &dyn Platform) -> AncestorInspection {
    let adapter = ClaudeAdapter::new(paths.clone());
    let ppid = platform.parent_pid();
    if ppid <= 1 {
        return AncestorInspection::default();
    }

    let mut fallback: Option<(i64, i64)> = None;
    let mut matched_claude: Option<(i64, i64)> = None;
    let mut current = ppid;

    for _ in 0..MAX_ANCESTOR_DEPTH {
        if current <= 1 {
            break;
        }
        let Some(info) = platform.process_info(current) else {
            break;
        };

        if info
            .argv
            .iter()
            .skip(1)
            .any(|arg| arg == "-p" || arg == "--print")
        {
            return AncestorInspection {
                pid: None,
                pid_start_us: None,
                is_headless: true,
            };
        }

        if fallback.is_none() {
            fallback = Some((info.pid as i64, info.start_time_us));
        }
        if matched_claude.is_none() && adapter.matches_process(&info) {
            matched_claude = Some((info.pid as i64, info.start_time_us));
        }

        if info.ppid == current {
            break;
        }
        current = info.ppid;
    }

    let (pid, pid_start_us) = match matched_claude.or(fallback) {
        Some((pid, start_us)) => (Some(pid), Some(start_us)),
        None => (Some(ppid as i64), None),
    };

    AncestorInspection {
        pid,
        pid_start_us,
        is_headless: false,
    }
}

/// Inspect `~/.claude/settings.json` read-only to check whether restowa hooks
/// are installed.
pub fn inspect_claude_hooks(settings_path: &Path) -> HookStatus {
    if !settings_path.exists() {
        return HookStatus {
            settings_exists: false,
            start_installed: false,
            end_installed: false,
            parse_error: None,
        };
    }

    let raw = match std::fs::read_to_string(settings_path) {
        Ok(r) => r,
        Err(e) => {
            return HookStatus {
                settings_exists: true,
                start_installed: false,
                end_installed: false,
                parse_error: Some(e.to_string()),
            }
        }
    };

    let val: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            return HookStatus {
                settings_exists: true,
                start_installed: false,
                end_installed: false,
                parse_error: Some(e.to_string()),
            }
        }
    };

    let start_installed = has_hook_command(&val, "SessionStart", START_SUBCMD);
    let end_installed = has_hook_command(&val, "SessionEnd", END_SUBCMD);

    HookStatus {
        settings_exists: true,
        start_installed,
        end_installed,
        parse_error: None,
    }
}

/// Merge restowa's `SessionStart` and `SessionEnd` hooks into
/// `~/.claude/settings.json`, backing up the existing file first and
/// preserving all existing settings and hooks.
pub fn install_claude_hooks(
    settings_path: &Path,
    backup_path: &Path,
    command_bin: &str,
) -> Result<HookInstallReport> {
    let command_start = format!("{command_bin} {START_SUBCMD}");
    let command_end = format!("{command_bin} {END_SUBCMD}");

    let (mut root, backup_written) = if settings_path.exists() {
        let raw = std::fs::read_to_string(settings_path).map_err(|source| CoreError::IoPath {
            path: settings_path.to_path_buf(),
            source,
        })?;
        let val: Value = serde_json::from_str(&raw).map_err(|e| CoreError::InvalidSettings {
            path: settings_path.to_path_buf(),
            reason: e.to_string(),
        })?;
        if !val.is_object() {
            return Err(CoreError::InvalidSettings {
                path: settings_path.to_path_buf(),
                reason: "root JSON value must be an object".to_string(),
            });
        }
        std::fs::write(backup_path, &raw).map_err(|source| CoreError::IoPath {
            path: backup_path.to_path_buf(),
            source,
        })?;
        (val, Some(backup_path.to_string_lossy().into_owned()))
    } else {
        AppPaths::ensure_parent(settings_path)?;
        (Value::Object(Map::new()), None)
    };

    let added_start = ensure_hook_entry(&mut root, "SessionStart", START_SUBCMD, &command_start)?;
    let added_end = ensure_hook_entry(&mut root, "SessionEnd", END_SUBCMD, &command_end)?;

    write_json_atomic(settings_path, &root)?;

    Ok(HookInstallReport {
        settings_path: settings_path.to_string_lossy().into_owned(),
        backup_path: backup_written,
        added_start,
        added_end,
        command_start,
        command_end,
    })
}

/// Remove only restowa's hook entries from `~/.claude/settings.json`,
/// preserving all other hooks and settings.
pub fn uninstall_claude_hooks(settings_path: &Path) -> Result<HookUninstallReport> {
    if !settings_path.exists() {
        return Ok(HookUninstallReport {
            settings_path: settings_path.to_string_lossy().into_owned(),
            removed_count: 0,
        });
    }

    let raw = std::fs::read_to_string(settings_path).map_err(|source| CoreError::IoPath {
        path: settings_path.to_path_buf(),
        source,
    })?;
    let mut root: Value = serde_json::from_str(&raw).map_err(|e| CoreError::InvalidSettings {
        path: settings_path.to_path_buf(),
        reason: e.to_string(),
    })?;

    let mut removed = 0usize;
    if let Some(hooks_obj) = root.get_mut("hooks").and_then(|v| v.as_object_mut()) {
        for (event_key, marker) in [("SessionStart", START_SUBCMD), ("SessionEnd", END_SUBCMD)] {
            if let Some(arr) = hooks_obj.get_mut(event_key).and_then(|v| v.as_array_mut()) {
                for matcher_obj in arr.iter_mut() {
                    if let Some(inner_hooks) =
                        matcher_obj.get_mut("hooks").and_then(|v| v.as_array_mut())
                    {
                        let before = inner_hooks.len();
                        inner_hooks.retain(|h| !is_restowa_hook(h, marker));
                        removed += before - inner_hooks.len();
                    }
                }
                arr.retain(|m| {
                    m.get("hooks")
                        .and_then(|v| v.as_array())
                        .is_none_or(|a| !a.is_empty())
                });
            }
            if hooks_obj
                .get(event_key)
                .and_then(|v| v.as_array())
                .is_some_and(|a| a.is_empty())
            {
                hooks_obj.remove(event_key);
            }
        }
        if hooks_obj.is_empty() {
            if let Some(root_obj) = root.as_object_mut() {
                root_obj.remove("hooks");
            }
        }
    }

    if removed > 0 {
        write_json_atomic(settings_path, &root)?;
    }

    Ok(HookUninstallReport {
        settings_path: settings_path.to_string_lossy().into_owned(),
        removed_count: removed,
    })
}

fn is_restowa_hook(hook: &Value, marker: &str) -> bool {
    hook.get("type").and_then(|v| v.as_str()) == Some("command")
        && hook
            .get("command")
            .and_then(|v| v.as_str())
            .is_some_and(|cmd| cmd.contains(marker))
}

fn has_hook_command(root: &Value, event_key: &str, marker: &str) -> bool {
    root.pointer(&format!("/hooks/{event_key}"))
        .and_then(|v| v.as_array())
        .is_some_and(|arr| {
            arr.iter().any(|matcher| {
                matcher
                    .get("hooks")
                    .and_then(|v| v.as_array())
                    .is_some_and(|inner| inner.iter().any(|h| is_restowa_hook(h, marker)))
            })
        })
}

fn ensure_hook_entry(
    root: &mut Value,
    event_key: &str,
    marker: &str,
    command: &str,
) -> Result<bool> {
    if has_hook_command(root, event_key, marker) {
        return Ok(false);
    }

    let root_obj = root
        .as_object_mut()
        .ok_or_else(|| CoreError::InvalidSettings {
            path: std::path::PathBuf::from("settings.json"),
            reason: "root JSON value must be an object".to_string(),
        })?;

    let hooks_val = root_obj
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()));
    let hooks_obj = hooks_val
        .as_object_mut()
        .ok_or_else(|| CoreError::InvalidSettings {
            path: std::path::PathBuf::from("settings.json"),
            reason: "'hooks' must be a JSON object".to_string(),
        })?;

    let event_val = hooks_obj
        .entry(event_key)
        .or_insert_with(|| Value::Array(Vec::new()));
    let event_arr = event_val
        .as_array_mut()
        .ok_or_else(|| CoreError::InvalidSettings {
            path: std::path::PathBuf::from("settings.json"),
            reason: format!("'hooks.{event_key}' must be a JSON array"),
        })?;

    event_arr.push(json!({
        "matcher": "",
        "hooks": [
            {
                "type": "command",
                "command": command
            }
        ]
    }));
    Ok(true)
}

fn write_json_atomic(path: &Path, val: &Value) -> Result<()> {
    AppPaths::ensure_parent(path)?;
    let mut formatted = serde_json::to_string_pretty(val)?;
    formatted.push('\n');
    let tmp = path.with_extension(format!("json.restowa.tmp.{}", std::process::id()));
    std::fs::write(&tmp, &formatted).map_err(|source| CoreError::IoPath {
        path: tmp.clone(),
        source,
    })?;
    std::fs::rename(&tmp, path).map_err(|source| CoreError::IoPath {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ProcInfo, SessionSource, SessionStatus};
    use crate::scanner::run_scan;
    use rusqlite::Connection;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Instant;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TestHome(PathBuf);
    impl TestHome {
        fn new() -> Self {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "restowa-hooks-test-{}-{}",
                std::process::id(),
                n
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }
    impl Drop for TestHome {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    struct FakePlatform {
        boot: String,
        procs: Vec<ProcInfo>,
        ppid: i32,
    }

    impl Platform for FakePlatform {
        fn boot_id(&self) -> Result<String> {
            Ok(self.boot.clone())
        }
        fn list_processes(&self) -> Result<Vec<ProcInfo>> {
            Ok(self.procs.clone())
        }
        fn process_info(&self, pid: i32) -> Option<ProcInfo> {
            self.procs.iter().find(|p| p.pid == pid).cloned()
        }
        fn process_open_files(&self, _pid: i32) -> Vec<PathBuf> {
            Vec::new()
        }
        fn parent_pid(&self) -> i32 {
            self.ppid
        }
    }

    fn claude_platform(boot: &str, claude_pid: i32, start_us: i64) -> FakePlatform {
        FakePlatform {
            boot: boot.to_string(),
            ppid: claude_pid + 100,
            procs: vec![
                ProcInfo {
                    pid: claude_pid + 100,
                    ppid: claude_pid,
                    start_time_us: start_us + 50,
                    exe_path: Some(PathBuf::from("/bin/sh")),
                    argv: vec![
                        "/bin/sh".into(),
                        "-c".into(),
                        "restowa hook claude session-start".into(),
                    ],
                    cwd: Some(PathBuf::from("/tmp/p1")),
                },
                ProcInfo {
                    pid: claude_pid,
                    ppid: 1,
                    start_time_us: start_us,
                    exe_path: Some(PathBuf::from(
                        "/Users/u/.local/share/claude/versions/2.1.330",
                    )),
                    argv: vec!["claude".into()],
                    cwd: Some(PathBuf::from("/tmp/p1")),
                },
            ],
        }
    }

    #[test]
    fn install_merges_without_touching_existing_hooks_and_writes_backup() {
        let home = TestHome::new();
        let paths = AppPaths::from_home(home.0.clone());
        std::fs::create_dir_all(paths.claude_dir()).unwrap();

        let existing = r#"{
  "model": "opus",
  "hooks": {
    "SessionStart": [
      {
        "matcher": "startup",
        "hooks": [
          { "type": "command", "command": "echo existing-hook" }
        ]
      }
    ],
    "PreToolUse": [
      {
        "matcher": "Bash",
        "hooks": [
          { "type": "command", "command": "echo keep-me" }
        ]
      }
    ]
  }
}
"#;
        std::fs::write(paths.claude_settings(), existing).unwrap();

        let report = install_claude_hooks(
            &paths.claude_settings(),
            &paths.claude_settings_backup(),
            "restowa",
        )
        .unwrap();
        assert!(report.added_start);
        assert!(report.added_end);
        assert!(paths.claude_settings_backup().is_file());
        assert_eq!(
            std::fs::read_to_string(paths.claude_settings_backup()).unwrap(),
            existing
        );

        let updated: Value =
            serde_json::from_str(&std::fs::read_to_string(paths.claude_settings()).unwrap())
                .unwrap();
        assert_eq!(updated["model"], "opus");
        assert_eq!(
            updated["hooks"]["SessionStart"].as_array().unwrap().len(),
            2
        );
        assert_eq!(updated["hooks"]["SessionEnd"].as_array().unwrap().len(), 1);
        assert_eq!(updated["hooks"]["PreToolUse"].as_array().unwrap().len(), 1);

        // Idempotent second install
        let second = install_claude_hooks(
            &paths.claude_settings(),
            &paths.claude_settings_backup(),
            "restowa",
        )
        .unwrap();
        assert!(!second.added_start);
        assert!(!second.added_end);

        // Uninstall removes only restowa hooks, preserving existing ones
        let un = uninstall_claude_hooks(&paths.claude_settings()).unwrap();
        assert_eq!(un.removed_count, 2);
        let after: Value =
            serde_json::from_str(&std::fs::read_to_string(paths.claude_settings()).unwrap())
                .unwrap();
        assert_eq!(after["hooks"]["SessionStart"].as_array().unwrap().len(), 1);
        assert!(after["hooks"].get("SessionEnd").is_none());
        assert_eq!(after["hooks"]["PreToolUse"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn handle_hook_walks_through_shell_wrapper_to_claude_pid() {
        let home = TestHome::new();
        let paths = AppPaths::from_home(home.0.clone());
        let platform = claude_platform("1794307113.486877", 400, 1000);

        let start_json = r#"{
            "session_id": "11111111-2222-3333-4444-555555555555",
            "hook_event_name": "SessionStart",
            "source": "startup",
            "cwd": "/tmp/p1",
            "transcript_path": "/tmp/p1.jsonl"
        }"#;
        handle_claude_hook(&paths, &platform, ClaudeHookEvent::SessionStart, start_json).unwrap();

        let reg = Registry::open(&paths.registry_db).unwrap();
        let s = reg
            .get_by_key("claude:11111111-2222-3333-4444-555555555555")
            .unwrap()
            .unwrap();
        assert_eq!(s.pid, Some(400));
        assert_eq!(s.pid_start_us, Some(1000));
        assert_eq!(s.status, SessionStatus::Live);
        assert_eq!(s.source, SessionSource::Hook);

        let end_json = r#"{
            "session_id": "11111111-2222-3333-4444-555555555555",
            "hook_event_name": "SessionEnd",
            "reason": "prompt_input_exit",
            "cwd": "/tmp/p1",
            "transcript_path": "/tmp/p1.jsonl"
        }"#;
        handle_claude_hook(&paths, &platform, ClaudeHookEvent::SessionEnd, end_json).unwrap();
        let s2 = reg
            .get_by_key("claude:11111111-2222-3333-4444-555555555555")
            .unwrap()
            .unwrap();
        assert_eq!(s2.status, SessionStatus::Closed);
        assert_eq!(s2.pid, None);
        assert_eq!(s2.last_boot_id.as_deref(), Some("1794307113.486877"));
    }

    #[test]
    fn session_start_source_clear_closes_old_row_and_excludes_from_last_boot() {
        let home = TestHome::new();
        let paths = AppPaths::from_home(home.0.clone());
        let platform = claude_platform("boot-1", 400, 1000);

        // Initial startup session
        let startup = r#"{
            "session_id": "old-session-1111",
            "hook_event_name": "SessionStart",
            "source": "startup",
            "cwd": "/tmp/p1",
            "transcript_path": "/tmp/old.jsonl"
        }"#;
        handle_claude_hook(&paths, &platform, ClaudeHookEvent::SessionStart, startup).unwrap();

        // /clear fires SessionStart with a NEW session_id on the SAME (pid, pid_start_us)
        let cleared = r#"{
            "session_id": "new-session-2222",
            "hook_event_name": "SessionStart",
            "source": "clear",
            "cwd": "/tmp/p1",
            "transcript_path": "/tmp/new.jsonl"
        }"#;
        handle_claude_hook(&paths, &platform, ClaudeHookEvent::SessionStart, cleared).unwrap();

        let reg = Registry::open(&paths.registry_db).unwrap();
        let old = reg.get_by_key("claude:old-session-1111").unwrap().unwrap();
        assert_eq!(old.status, SessionStatus::Closed);
        assert_eq!(old.pid, None);
        assert_eq!(old.pid_start_us, None);
        assert_eq!(old.last_boot_id, None);

        let new = reg.get_by_key("claude:new-session-2222").unwrap().unwrap();
        assert_eq!(new.status, SessionStatus::Live);
        assert_eq!(new.pid, Some(400));
        assert_eq!(new.last_boot_id.as_deref(), Some("boot-1"));

        // Verify old session is excluded from last-boot restore set
        let restore_set = reg.list_last_boot(Some("boot-1")).unwrap();
        let ids: Vec<&str> = restore_set.iter().map(|s| s.session_id.as_str()).collect();
        assert_eq!(ids, vec!["new-session-2222"]);
    }

    #[test]
    fn session_end_reason_clear_also_excludes_old_row_before_new_start() {
        let home = TestHome::new();
        let paths = AppPaths::from_home(home.0.clone());
        let platform = claude_platform("boot-1", 400, 1000);

        let startup = r#"{
            "session_id": "old-clear-seq",
            "hook_event_name": "SessionStart",
            "source": "startup",
            "cwd": "/tmp/p1"
        }"#;
        handle_claude_hook(&paths, &platform, ClaudeHookEvent::SessionStart, startup).unwrap();

        let end_clear = r#"{
            "session_id": "old-clear-seq",
            "hook_event_name": "SessionEnd",
            "reason": "clear",
            "cwd": "/tmp/p1"
        }"#;
        handle_claude_hook(&paths, &platform, ClaudeHookEvent::SessionEnd, end_clear).unwrap();

        let start_clear = r#"{
            "session_id": "new-clear-seq",
            "hook_event_name": "SessionStart",
            "source": "clear",
            "cwd": "/tmp/p1"
        }"#;
        handle_claude_hook(
            &paths,
            &platform,
            ClaudeHookEvent::SessionStart,
            start_clear,
        )
        .unwrap();

        let reg = Registry::open(&paths.registry_db).unwrap();
        let old = reg.get_by_key("claude:old-clear-seq").unwrap().unwrap();
        assert_eq!(old.status, SessionStatus::Closed);
        assert_eq!(old.last_boot_id, None);

        let restore_set = reg.list_last_boot(Some("boot-1")).unwrap();
        assert_eq!(restore_set.len(), 1);
        assert_eq!(restore_set[0].session_id, "new-clear-seq");
    }

    #[test]
    fn session_start_source_compact_keeps_same_id_live_and_preserves_metadata() {
        let home = TestHome::new();
        let paths = AppPaths::from_home(home.0.clone());
        let platform = claude_platform("boot-1", 400, 1000);

        let startup = r#"{
            "session_id": "compact-id-1",
            "hook_event_name": "SessionStart",
            "source": "startup",
            "cwd": "/tmp/p1",
            "transcript_path": "/tmp/c.jsonl"
        }"#;
        handle_claude_hook(&paths, &platform, ClaudeHookEvent::SessionStart, startup).unwrap();

        let mut reg = Registry::open(&paths.registry_db).unwrap();
        reg.set_title("claude:compact-id-1", "Keep Title").unwrap();
        reg.set_pinned("claude:compact-id-1", true).unwrap();
        drop(reg);

        // /compact keeps the same session_id on the same PID
        let compact = r#"{
            "session_id": "compact-id-1",
            "hook_event_name": "SessionStart",
            "source": "compact",
            "cwd": "/tmp/p1",
            "transcript_path": "/tmp/c.jsonl"
        }"#;
        handle_claude_hook(&paths, &platform, ClaudeHookEvent::SessionStart, compact).unwrap();

        let reg = Registry::open(&paths.registry_db).unwrap();
        let row = reg.get_by_key("claude:compact-id-1").unwrap().unwrap();
        assert_eq!(row.status, SessionStatus::Live);
        assert_eq!(row.pid, Some(400));
        assert_eq!(row.pid_start_us, Some(1000));
        assert_eq!(row.last_boot_id.as_deref(), Some("boot-1"));
        assert_eq!(row.title.as_deref(), Some("Keep Title"));
        assert!(row.pinned);
    }

    #[test]
    fn session_start_source_resume_keeps_same_id_and_updates_pid_and_boot() {
        let home = TestHome::new();
        let paths = AppPaths::from_home(home.0.clone());
        let boot1 = claude_platform("boot-1", 400, 1000);

        let startup = r#"{
            "session_id": "resume-id-1",
            "hook_event_name": "SessionStart",
            "source": "startup",
            "cwd": "/tmp/p1",
            "transcript_path": "/tmp/r.jsonl"
        }"#;
        handle_claude_hook(&paths, &boot1, ClaudeHookEvent::SessionStart, startup).unwrap();

        let end = r#"{
            "session_id": "resume-id-1",
            "hook_event_name": "SessionEnd",
            "reason": "prompt_input_exit",
            "cwd": "/tmp/p1"
        }"#;
        handle_claude_hook(&paths, &boot1, ClaudeHookEvent::SessionEnd, end).unwrap();

        // Resumed after reboot in a new Claude process
        let boot2 = claude_platform("boot-2", 900, 5000);
        let resume = r#"{
            "session_id": "resume-id-1",
            "hook_event_name": "SessionStart",
            "source": "resume",
            "cwd": "/tmp/p1",
            "transcript_path": "/tmp/r.jsonl"
        }"#;
        handle_claude_hook(&paths, &boot2, ClaudeHookEvent::SessionStart, resume).unwrap();

        let reg = Registry::open(&paths.registry_db).unwrap();
        let row = reg.get_by_key("claude:resume-id-1").unwrap().unwrap();
        assert_eq!(row.status, SessionStatus::Live);
        assert_eq!(row.pid, Some(900));
        assert_eq!(row.pid_start_us, Some(5000));
        assert_eq!(row.last_boot_id.as_deref(), Some("boot-2"));
    }

    #[test]
    fn headless_ancestor_with_p_or_print_skips_hook() {
        for flag in ["-p", "--print"] {
            let home = TestHome::new();
            let paths = AppPaths::from_home(home.0.clone());
            let platform = FakePlatform {
                boot: "boot-1".to_string(),
                ppid: 500,
                procs: vec![
                    ProcInfo {
                        pid: 500,
                        ppid: 400,
                        start_time_us: 2000,
                        exe_path: Some(PathBuf::from("/bin/sh")),
                        argv: vec![
                            "/bin/sh".into(),
                            "-c".into(),
                            "restowa hook claude session-start".into(),
                        ],
                        cwd: Some(PathBuf::from("/tmp/p1")),
                    },
                    ProcInfo {
                        pid: 400,
                        ppid: 1,
                        start_time_us: 1000,
                        exe_path: Some(PathBuf::from("/Users/u/.local/bin/claude")),
                        argv: vec!["claude".into(), flag.into(), "summarize this".into()],
                        cwd: Some(PathBuf::from("/tmp/p1")),
                    },
                ],
            };

            let start = r#"{
                "session_id": "headless-should-skip",
                "hook_event_name": "SessionStart",
                "source": "startup",
                "cwd": "/tmp/p1"
            }"#;
            handle_claude_hook(&paths, &platform, ClaudeHookEvent::SessionStart, start).unwrap();

            assert!(!paths.registry_db.exists());
            assert!(!paths.spool_file.exists());
        }
    }

    #[test]
    fn busy_db_falls_back_to_spool_within_timeout_and_scan_replays_and_clears() {
        let home = TestHome::new();
        let paths = AppPaths::from_home(home.0.clone());

        // Initialize DB first so WAL exists, then lock it exclusively.
        let reg = Registry::open(&paths.registry_db).unwrap();
        drop(reg);

        let locker = Connection::open(&paths.registry_db).unwrap();
        locker.execute_batch("BEGIN EXCLUSIVE;").unwrap();

        let platform = claude_platform("boot-1", 400, 1000);
        let start = r#"{
            "session_id": "spooled-session-999",
            "hook_event_name": "SessionStart",
            "source": "startup",
            "cwd": "/tmp/p1",
            "transcript_path": "/tmp/s.jsonl"
        }"#;

        let t0 = Instant::now();
        handle_claude_hook(&paths, &platform, ClaudeHookEvent::SessionStart, start).unwrap();
        let elapsed = t0.elapsed();

        // Must fail fast (~150ms busy_timeout) instead of waiting 2s
        assert!(
            elapsed < Duration::from_millis(900),
            "hook blocked too long on busy DB: {elapsed:?}"
        );
        assert!(paths.spool_file.is_file(), "expected spool.jsonl to exist");

        // Release exclusive lock and let `run_scan` replay and clear the spool
        locker.execute_batch("COMMIT;").unwrap();
        drop(locker);

        let scan_report = run_scan(&paths, &platform).unwrap();
        assert_eq!(scan_report.spool_imported, 1);
        assert!(!paths.spool_file.exists(), "spool.jsonl should be cleared");

        let reg = Registry::open(&paths.registry_db).unwrap();
        let row = reg
            .get_by_key("claude:spooled-session-999")
            .unwrap()
            .unwrap();
        assert_eq!(row.source, SessionSource::Hook);
        assert_eq!(row.status, SessionStatus::Live);
        assert_eq!(row.pid, Some(400));
    }
}
