use crate::adapters::claude::ClaudeAdapter;
use crate::adapters::Adapter;
use crate::error::{CoreError, Result};
use crate::model::Tool;
use crate::paths::AppPaths;
use crate::platform::Platform;
use crate::registry::{HookSessionUpsert, Registry};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

const START_SUBCMD: &str = "hook claude session-start";
const END_SUBCMD: &str = "hook claude session-end";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
/// Kept deliberately short-lived (< 50 ms budget): single SQLite write
/// transaction, no full process list scan, no transcript parsing.
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

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let boot_id = platform.boot_id().unwrap_or_else(|_| "unknown".to_string());

    let mut registry = Registry::open(&paths.registry_db)?;

    match event {
        ClaudeHookEvent::SessionStart => {
            let (pid, pid_start_us) = find_claude_ancestor_pid(paths, platform);
            registry.upsert_hook_start(&HookSessionUpsert {
                tool: Tool::Claude,
                session_id: payload.session_id,
                cwd: payload.cwd,
                transcript: payload.transcript_path,
                pid,
                pid_start_us,
                boot_id,
                now,
            })?;
        }
        ClaudeHookEvent::SessionEnd => {
            registry.record_hook_end(
                Tool::Claude,
                &payload.session_id,
                &payload.cwd,
                payload.transcript_path.as_deref(),
                &boot_id,
                now,
            )?;
        }
    }

    Ok(())
}

/// Walk up to 3 ancestor processes from `getppid()` to find the invoking
/// Claude Code process (handles both direct exec and `/bin/sh -c` wrappers).
fn find_claude_ancestor_pid(
    paths: &AppPaths,
    platform: &dyn Platform,
) -> (Option<i64>, Option<i64>) {
    let adapter = ClaudeAdapter::new(paths.clone());
    let ppid = platform.parent_pid();
    if ppid <= 1 {
        return (None, None);
    }

    let mut fallback: Option<(i64, i64)> = None;
    let mut current = ppid;
    for _ in 0..3 {
        if current <= 1 {
            break;
        }
        if let Some(info) = platform.process_info(current) {
            if fallback.is_none() {
                fallback = Some((info.pid as i64, info.start_time_us));
            }
            if adapter.matches_process(&info) {
                return (Some(info.pid as i64), Some(info.start_time_us));
            }
            current = info.ppid;
        } else {
            break;
        }
    }

    match fallback {
        Some((pid, start_us)) => (Some(pid), Some(start_us)),
        None => (Some(ppid as i64), None),
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
                // Drop empty matcher wrapper objects that became empty after removing our hook.
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
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

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
        let platform = FakePlatform {
            boot: "1794307113.486877".to_string(),
            ppid: 500, // /bin/sh
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
                    exe_path: Some(PathBuf::from(
                        "/Users/u/.local/share/claude/versions/2.1.330",
                    )),
                    argv: vec!["claude".into()],
                    cwd: Some(PathBuf::from("/tmp/p1")),
                },
            ],
        };

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
}
