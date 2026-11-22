use crate::adapters::adapters_for_paths;
use crate::config::Config;
use crate::error::Result;
use crate::model::{
    FailedItem, ProjectGroup, RestoreItem, RestoreReport, Session, SessionStatus, SkippedItem,
};
use crate::paths::AppPaths;
use crate::platform::Platform;
use crate::registry::{resolve_key_in_slice, Registry};
use crate::scanner::load_merged_sessions;
use crate::shell::{build_tab_command, expand_resume_template};
use crate::terminal::select_backend;
use std::collections::{BTreeMap, HashSet};
use std::path::Path;

#[derive(Debug, Clone)]
pub struct RestoreOptions {
    pub keys: Vec<String>,
    pub last_boot: bool,
    pub terminal: Option<String>,
    pub dry_run: bool,
}

pub fn run_restore(
    paths: &AppPaths,
    config: &Config,
    platform: &dyn Platform,
    opts: &RestoreOptions,
) -> Result<RestoreReport> {
    let terminal_name = opts
        .terminal
        .clone()
        .unwrap_or_else(|| config.terminal.clone());

    let all_merged = load_merged_sessions(paths, platform, false)?;
    let live_keys: HashSet<String> = all_merged
        .iter()
        .filter(|s| s.status == SessionStatus::Live)
        .map(|s| s.key.clone())
        .collect();

    let candidates: Vec<Session> = if !opts.keys.is_empty() {
        let mut selected = Vec::with_capacity(opts.keys.len());
        let mut seen = HashSet::new();
        for key in &opts.keys {
            let s = resolve_key_in_slice(&all_merged, key)?;
            if seen.insert(s.key.clone()) {
                selected.push(s);
            }
        }
        selected
    } else if opts.last_boot {
        select_last_boot_sessions(paths, platform)?
    } else {
        // Default: prefer previous boot restore set if one exists with sessions;
        // otherwise select all non-missing, non-hidden sessions (e.g. Phase 1
        // same-boot workflow).
        let last_boot_set = select_last_boot_sessions(paths, platform)?;
        if !last_boot_set.is_empty() {
            last_boot_set
        } else {
            all_merged
                .into_iter()
                .filter(|s| s.status != SessionStatus::Missing && !s.hidden)
                .collect()
        }
    };

    let adapters = adapters_for_paths(paths);
    let mut skipped = Vec::new();
    let mut failed = Vec::new();
    let mut by_cwd: BTreeMap<String, (i64, Vec<RestoreItem>)> = BTreeMap::new();

    for session in candidates {
        if session.status == SessionStatus::Missing {
            skipped.push(SkippedItem {
                key: session.key,
                cwd: session.cwd,
                reason: "session data missing on disk".to_string(),
            });
            continue;
        }

        if !Path::new(&session.cwd).is_dir() {
            failed.push(FailedItem {
                key: session.key,
                cwd: session.cwd,
                reason: "working directory does not exist".to_string(),
            });
            continue;
        }

        let is_live = live_keys.contains(&session.key);
        if is_live && !opts.dry_run {
            skipped.push(SkippedItem {
                key: session.key,
                cwd: session.cwd,
                reason: "already live".to_string(),
            });
            continue;
        }

        let tool_cfg = config.tool_config(session.tool);
        let resume_command = adapters
            .iter()
            .find(|a| a.tool() == session.tool)
            .map(|a| a.resume_command(&session, tool_cfg))
            .unwrap_or_else(|| expand_resume_template(&tool_cfg.resume, &session.session_id));
        let shell_command = build_tab_command(&session.cwd, &resume_command);
        let activity = session.last_seen_live.unwrap_or(session.first_seen);

        let item = RestoreItem {
            key: session.key.clone(),
            tool: session.tool,
            session_id: session.session_id.clone(),
            cwd: session.cwd.clone(),
            title: session.title.clone(),
            resume_command,
            shell_command,
        };

        if is_live && opts.dry_run {
            skipped.push(SkippedItem {
                key: session.key,
                cwd: session.cwd.clone(),
                reason: "already live (would be skipped without --dry-run)".to_string(),
            });
        }

        let entry = by_cwd
            .entry(session.cwd)
            .or_insert_with(|| (activity, Vec::new()));
        if activity > entry.0 {
            entry.0 = activity;
        }
        entry.1.push(item);
    }

    let mut groups: Vec<ProjectGroup> = by_cwd
        .into_iter()
        .map(|(cwd, (most_recent_activity, items))| ProjectGroup {
            cwd,
            most_recent_activity,
            items,
        })
        .collect();
    groups.sort_by(|a, b| {
        b.most_recent_activity
            .cmp(&a.most_recent_activity)
            .then_with(|| a.cwd.cmp(&b.cwd))
    });

    if opts.dry_run {
        let restored = groups
            .into_iter()
            .flat_map(|g| g.items.into_iter())
            .collect();
        return Ok(RestoreReport {
            terminal: terminal_name,
            dry_run: true,
            restored,
            skipped,
            failed,
        });
    }

    if groups.is_empty() {
        return Ok(RestoreReport {
            terminal: terminal_name,
            dry_run: false,
            restored: Vec::new(),
            skipped,
            failed,
        });
    }

    let backend = select_backend(&terminal_name, paths)?;
    let mut report = backend.open(&groups)?;
    report.skipped.extend(skipped);
    report.failed.extend(failed);
    Ok(report)
}

fn select_last_boot_sessions(paths: &AppPaths, platform: &dyn Platform) -> Result<Vec<Session>> {
    let Some(reg) = Registry::open_read_only(&paths.registry_db)? else {
        return Ok(Vec::new());
    };
    let current_boot = platform.boot_id()?;
    let prev_boot = reg.previous_boot_id(&current_boot)?;
    reg.list_last_boot(prev_boot.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ProcInfo, Tool};
    use crate::registry::HookSessionUpsert;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TestEnv {
        home: PathBuf,
        proj_a: PathBuf,
        proj_b: PathBuf,
    }

    impl TestEnv {
        fn new() -> Self {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "restowa-restore-test-{}-{}",
                std::process::id(),
                n
            ));
            let _ = std::fs::remove_dir_all(&root);
            let home = root.join("home");
            let proj_a = root.join("proj a");
            let proj_b = root.join("proj'b");
            std::fs::create_dir_all(&home).unwrap();
            std::fs::create_dir_all(&proj_a).unwrap();
            std::fs::create_dir_all(&proj_b).unwrap();
            Self {
                home,
                proj_a,
                proj_b,
            }
        }
    }

    impl Drop for TestEnv {
        fn drop(&mut self) {
            if let Some(root) = self.home.parent() {
                let _ = std::fs::remove_dir_all(root);
            }
        }
    }

    struct EmptyPlatform;
    impl Platform for EmptyPlatform {
        fn boot_id(&self) -> Result<String> {
            Ok("100.000001".into())
        }
        fn list_processes(&self) -> Result<Vec<ProcInfo>> {
            Ok(Vec::new())
        }
        fn process_info(&self, _pid: i32) -> Option<ProcInfo> {
            None
        }
        fn process_open_files(&self, _pid: i32) -> Vec<PathBuf> {
            Vec::new()
        }
        fn parent_pid(&self) -> i32 {
            1
        }
    }

    #[test]
    fn dry_run_groups_three_sessions_in_two_folders_with_escaped_commands() {
        let env = TestEnv::new();
        let paths = AppPaths::from_home(env.home.clone());
        let mut reg = Registry::open(&paths.registry_db).unwrap();

        let entries = [
            (
                "11111111-aaaa",
                env.proj_a.to_string_lossy().to_string(),
                100,
            ),
            (
                "22222222-bbbb",
                env.proj_a.to_string_lossy().to_string(),
                300,
            ),
            (
                "33333333-cccc",
                env.proj_b.to_string_lossy().to_string(),
                200,
            ),
        ];
        for (sid, cwd, ts) in entries {
            reg.upsert_hook_start(&HookSessionUpsert {
                tool: Tool::Claude,
                session_id: sid.to_string(),
                cwd,
                transcript: None,
                pid: Some(99999),
                pid_start_us: Some(123),
                boot_id: "100.000001".into(),
                now: ts,
            })
            .unwrap();
        }
        drop(reg);

        let report = run_restore(
            &paths,
            &Config::default(),
            &EmptyPlatform,
            &RestoreOptions {
                keys: Vec::new(),
                last_boot: false,
                terminal: Some("ghostty".into()),
                dry_run: true,
            },
        )
        .unwrap();

        assert!(report.dry_run);
        assert_eq!(report.restored.len(), 3);
        assert!(report.failed.is_empty());
        // Group A (max activity 300) comes before Group B (activity 200).
        assert_eq!(report.restored[0].cwd, env.proj_a.to_string_lossy());
        assert_eq!(report.restored[1].cwd, env.proj_a.to_string_lossy());
        assert_eq!(report.restored[2].cwd, env.proj_b.to_string_lossy());
        assert!(report.restored[2].shell_command.contains("proj'\\''b"));
        assert!(report.restored[2]
            .shell_command
            .ends_with("&& claude --resume '33333333-cccc'"));
    }

    #[test]
    fn missing_cwd_is_reported_failed_and_never_created() {
        let env = TestEnv::new();
        let paths = AppPaths::from_home(env.home.clone());
        let missing_dir = env.home.join("does-not-exist");

        let mut reg = Registry::open(&paths.registry_db).unwrap();
        reg.upsert_hook_start(&HookSessionUpsert {
            tool: Tool::Claude,
            session_id: "deadbeef-0000".into(),
            cwd: missing_dir.to_string_lossy().into_owned(),
            transcript: None,
            pid: None,
            pid_start_us: None,
            boot_id: "100.000001".into(),
            now: 100,
        })
        .unwrap();
        drop(reg);

        let report = run_restore(
            &paths,
            &Config::default(),
            &EmptyPlatform,
            &RestoreOptions {
                keys: Vec::new(),
                last_boot: false,
                terminal: Some("ghostty".into()),
                dry_run: true,
            },
        )
        .unwrap();

        assert!(report.restored.is_empty());
        assert_eq!(report.failed.len(), 1);
        assert!(!missing_dir.exists());
    }
}
