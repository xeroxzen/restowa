use crate::adapters::claude::ClaudeAdapter;
use crate::adapters::{adapters_for_paths, owning_tool_of_process, Adapter};
use crate::hooks::inspect_claude_hooks;
use crate::launchd::inspect_launchd_agent;
use crate::model::Tool;
use crate::paths::AppPaths;
use crate::platform::Platform;
use crate::registry::Registry;
use crate::terminal::ghostty::GhosttyBackend;
use crate::terminal::terminal_app::TerminalAppBackend;
use crate::terminal::TerminalBackend;
use serde::Serialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckStatus {
    Ok,
    Warn,
    Error,
}

#[derive(Debug, Clone, Serialize)]
pub struct DoctorCheck {
    pub name: String,
    pub status: CheckStatus,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct DoctorReport {
    pub checks: Vec<DoctorCheck>,
    pub has_errors: bool,
    pub has_warnings: bool,
}

pub fn run_doctor(paths: &AppPaths, platform: &dyn Platform) -> DoctorReport {
    let mut checks = Vec::new();

    // 1. Check installed agent CLIs
    for (name, candidates) in [
        ("claude", vec![paths.home.join(".local/bin/claude")]),
        (
            "codex",
            vec![
                PathBuf::from("/opt/homebrew/bin/codex"),
                paths.home.join(".local/bin/codex"),
            ],
        ),
        ("opencode", vec![paths.home.join(".opencode/bin/opencode")]),
        ("agy", vec![paths.home.join(".local/bin/agy")]),
    ] {
        if let Some(found) = find_executable(name, &candidates) {
            checks.push(DoctorCheck {
                name: format!("tool:{name}"),
                status: CheckStatus::Ok,
                detail: found.to_string_lossy().into_owned(),
            });
        } else {
            checks.push(DoctorCheck {
                name: format!("tool:{name}"),
                status: CheckStatus::Warn,
                detail: format!("{name} not found in PATH or standard install paths"),
            });
        }
    }

    // 2. Claude Code hooks
    let hook_status = inspect_claude_hooks(&paths.claude_settings());
    if let Some(err) = hook_status.parse_error {
        checks.push(DoctorCheck {
            name: "claude:hooks".to_string(),
            status: CheckStatus::Error,
            detail: format!(
                "could not parse {}: {err}",
                paths.claude_settings().display()
            ),
        });
    } else if hook_status.start_installed && hook_status.end_installed {
        checks.push(DoctorCheck {
            name: "claude:hooks".to_string(),
            status: CheckStatus::Ok,
            detail: format!(
                "SessionStart and SessionEnd hooks installed in {}",
                paths.claude_settings().display()
            ),
        });
    } else {
        checks.push(DoctorCheck {
            name: "claude:hooks".to_string(),
            status: CheckStatus::Warn,
            detail: "Claude Code hooks not installed (run `restowa install --hooks`)".to_string(),
        });
    }

    // 3. Claude Code PID session files (~/.claude/sessions/<pid>.json)
    //    Also track which tools currently have any process running so external
    //    DB checks strictly honor the `immutable=1` guard.
    let claude_sessions_dir = paths.claude_sessions_dir();
    let claude_adapter = ClaudeAdapter::new(paths.clone());
    let mut live_claude_procs: Vec<(i32, i64)> = Vec::new();
    let mut running_tools: HashSet<Tool> = HashSet::new();
    let listed = platform.for_each_process(&mut |p| {
        if let Some(tool) = owning_tool_of_process(p) {
            running_tools.insert(tool);
        }
        if claude_adapter.matches_process(p) {
            running_tools.insert(Tool::Claude);
            live_claude_procs.push((p.pid, p.start_time_us));
        }
    });
    if let Err(e) = listed {
        // Which tools are running is unknown, so assume all are: the schema
        // checks below must never open a live database with `immutable=1`.
        running_tools.extend(Tool::ALL);
        checks.push(DoctorCheck {
            name: "processes".to_string(),
            status: CheckStatus::Error,
            detail: format!("could not list processes: {e}"),
        });
    }

    if !claude_sessions_dir.is_dir() {
        checks.push(DoctorCheck {
            name: "claude:pid-files".to_string(),
            status: CheckStatus::Warn,
            detail: format!(
                "{} is missing; pre-hook live discovery falls back to --resume argv only",
                claude_sessions_dir.display()
            ),
        });
    } else {
        let now_us = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_micros() as i64);
        let missing_pids = claude_pids_missing_session_files(
            &claude_adapter,
            platform,
            &live_claude_procs,
            now_us,
        );
        let any_json = std::fs::read_dir(&claude_sessions_dir)
            .ok()
            .into_iter()
            .flatten()
            .flatten()
            .any(|e| e.path().extension().and_then(|s| s.to_str()) == Some("json"));

        if !missing_pids.is_empty() {
            checks.push(DoctorCheck {
                name: "claude:pid-files".to_string(),
                status: CheckStatus::Warn,
                detail: format!(
                    "live Claude PIDs missing session files in {}: {:?}",
                    claude_sessions_dir.display(),
                    missing_pids
                ),
            });
        } else if !any_json && live_claude_procs.is_empty() {
            checks.push(DoctorCheck {
                name: "claude:pid-files".to_string(),
                status: CheckStatus::Warn,
                detail: format!(
                    "no PID session files found in {} (normal if no Claude Code sessions have run)",
                    claude_sessions_dir.display()
                ),
            });
        } else {
            checks.push(DoctorCheck {
                name: "claude:pid-files".to_string(),
                status: CheckStatus::Ok,
                detail: format!(
                    "{} present ({} live Claude process(es) matched)",
                    claude_sessions_dir.display(),
                    live_claude_procs.len()
                ),
            });
        }
    }

    // 4. Registry writable
    match Registry::open(&paths.registry_db) {
        Ok(mut reg) => {
            if let Ok(boot_id) = platform.boot_id() {
                let _ = reg.record_boot(&boot_id, 0);
            }
            checks.push(DoctorCheck {
                name: "registry".to_string(),
                status: CheckStatus::Ok,
                detail: format!("writable at {}", paths.registry_db.display()),
            });
        }
        Err(e) => {
            checks.push(DoctorCheck {
                name: "registry".to_string(),
                status: CheckStatus::Error,
                detail: format!("cannot open {}: {e}", paths.registry_db.display()),
            });
        }
    }

    // 5. Spool status
    if paths.spool_file.is_file() {
        let len = std::fs::metadata(&paths.spool_file)
            .map(|m| m.len())
            .unwrap_or(0);
        if len > 0 {
            checks.push(DoctorCheck {
                name: "spool".to_string(),
                status: CheckStatus::Warn,
                detail: format!(
                    "{} has pending spooled hook events ({len} bytes); run `restowa scan` to import",
                    paths.spool_file.display()
                ),
            });
        }
    }

    // 6. Launchd agent
    let launchd = inspect_launchd_agent(paths, true);
    if !launchd.plist_exists {
        checks.push(DoctorCheck {
            name: "launchd".to_string(),
            status: CheckStatus::Warn,
            detail: "launchd scan agent not installed (run `restowa install --launchd`)"
                .to_string(),
        });
    } else if launchd.points_into_target {
        let prog = launchd.program_path.as_deref().unwrap_or("<unknown>");
        checks.push(DoctorCheck {
            name: "launchd".to_string(),
            status: CheckStatus::Warn,
            detail: format!(
                "launchd plist points into target/ ({prog}); install binary outside target/ and re-run `restowa install --launchd`"
            ),
        });
    } else if !launchd.binary_exists {
        let prog = launchd.program_path.as_deref().unwrap_or("<missing>");
        checks.push(DoctorCheck {
            name: "launchd".to_string(),
            status: CheckStatus::Error,
            detail: format!("launchd plist references missing binary: {prog}"),
        });
    } else if !launchd.loaded {
        let prog = launchd.program_path.as_deref().unwrap_or("<unknown>");
        checks.push(DoctorCheck {
            name: "launchd".to_string(),
            status: CheckStatus::Warn,
            detail: format!(
                "plist exists ({prog}) but dev.restowa.scan is not loaded (run `restowa install --launchd`)"
            ),
        });
    } else {
        let prog = launchd.program_path.as_deref().unwrap_or("<unknown>");
        checks.push(DoctorCheck {
            name: "launchd".to_string(),
            status: CheckStatus::Ok,
            detail: format!("dev.restowa.scan loaded ({prog})"),
        });
    }

    // 6. Terminal backends
    let ghostty = GhosttyBackend::new(paths.clone());
    if GhosttyBackend::app_installed() {
        if GhosttyBackend::is_applescript_disabled(&paths.ghostty_config()) {
            checks.push(DoctorCheck {
                name: "terminal:ghostty".to_string(),
                status: CheckStatus::Warn,
                detail: "Ghostty.app installed, but `macos-applescript = false` in ~/.config/ghostty/config".to_string(),
            });
        } else if ghostty.available() {
            checks.push(DoctorCheck {
                name: "terminal:ghostty".to_string(),
                status: CheckStatus::Ok,
                detail: "Ghostty.app available with AppleScript tab support (first restore prompts for macOS Automation approval)".to_string(),
            });
        }
    } else {
        checks.push(DoctorCheck {
            name: "terminal:ghostty".to_string(),
            status: CheckStatus::Warn,
            detail: "/Applications/Ghostty.app not found".to_string(),
        });
    }

    if TerminalAppBackend.available() {
        checks.push(DoctorCheck {
            name: "terminal:terminal".to_string(),
            status: CheckStatus::Ok,
            detail: "Terminal.app fallback available".to_string(),
        });
    }

    // 7. Adapter-specific external database schema checks (read-only)
    for adapter in adapters_for_paths(paths) {
        let tool_running = running_tools.contains(&adapter.tool());
        checks.extend(adapter.doctor_checks(tool_running));
    }

    let has_errors = checks.iter().any(|c| c.status == CheckStatus::Error);
    let has_warnings = checks.iter().any(|c| c.status == CheckStatus::Warn);

    DoctorReport {
        checks,
        has_errors,
        has_warnings,
    }
}

/// Claude writes its PID file during startup, and short-lived `claude`
/// invocations can exit before writing one.
const PID_FILE_GRACE_US: i64 = 10_000_000;

/// Claude processes without a PID file, ignoring those younger than
/// `PID_FILE_GRACE_US` and those that exited (or whose PID was reused)
/// since `procs` was collected.
fn claude_pids_missing_session_files(
    adapter: &ClaudeAdapter,
    platform: &dyn Platform,
    procs: &[(i32, i64)],
    now_us: i64,
) -> Vec<i32> {
    procs
        .iter()
        .filter(|&&(_, start)| now_us.saturating_sub(start) >= PID_FILE_GRACE_US)
        .filter(|&&(pid, _)| !adapter.has_pid_session_file(pid))
        .filter(|&&(pid, start)| {
            platform
                .process_info(pid)
                .is_some_and(|p| p.start_time_us == start)
        })
        .map(|&(pid, _)| pid)
        .collect()
}

fn find_executable(name: &str, fallbacks: &[PathBuf]) -> Option<PathBuf> {
    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let candidate = dir.join(name);
            if is_executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    for candidate in fallbacks {
        if is_executable(candidate) {
            return Some(candidate.clone());
        }
    }
    None
}

fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Result;
    use crate::launchd::install_launchd_agent;
    use crate::model::ProcInfo;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TestHome(PathBuf);
    impl TestHome {
        fn new() -> Self {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "restowa-doctor-test-{}-{}",
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

    struct FixedPlatform(Vec<ProcInfo>);
    impl Platform for FixedPlatform {
        fn boot_id(&self) -> Result<String> {
            Ok("100.000001".into())
        }
        fn list_processes(&self) -> Result<Vec<ProcInfo>> {
            Ok(self.0.clone())
        }
        fn process_info(&self, pid: i32) -> Option<ProcInfo> {
            self.0.iter().find(|p| p.pid == pid).cloned()
        }
        fn process_open_files(&self, _pid: i32) -> Vec<PathBuf> {
            Vec::new()
        }
        fn parent_pid(&self) -> i32 {
            1
        }
    }

    struct FailingPlatform;
    impl Platform for FailingPlatform {
        fn boot_id(&self) -> Result<String> {
            Ok("100.000001".into())
        }
        fn list_processes(&self) -> Result<Vec<ProcInfo>> {
            Err(crate::error::CoreError::Platform("listing denied".into()))
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
    fn doctor_reports_an_error_when_processes_cannot_be_listed() {
        let home = TestHome::new();
        let paths = AppPaths::from_home(home.0.clone());
        let report = run_doctor(&paths, &FailingPlatform);
        let check = report
            .checks
            .iter()
            .find(|c| c.name == "processes")
            .expect("processes check");
        assert_eq!(check.status, CheckStatus::Error);
        assert!(check.detail.contains("listing denied"));
        assert!(report.has_errors);
    }

    #[test]
    fn pid_file_check_ignores_young_exited_and_reused_processes() {
        let home = TestHome::new();
        let paths = AppPaths::from_home(home.0.clone());
        std::fs::create_dir_all(paths.claude_sessions_dir()).unwrap();
        std::fs::write(paths.claude_sessions_dir().join("40.json"), "{}").unwrap();
        let adapter = ClaudeAdapter::new(paths);

        let now = 100_000_000;
        let old = now - PID_FILE_GRACE_US;
        let proc_at = |pid: i32, start: i64| ProcInfo {
            pid,
            ppid: 1,
            start_time_us: start,
            exe_path: None,
            argv: vec!["claude".into()],
            cwd: None,
        };
        // 10: old, running, no file. 20: just started. 30: exited.
        // 40: has a file. 50: PID reused by a newer process.
        let platform = FixedPlatform(vec![
            proc_at(10, old),
            proc_at(20, now - 1),
            proc_at(40, old),
            proc_at(50, old + 1),
        ]);
        let seen = [(10, old), (20, now - 1), (30, old), (40, old), (50, old)];

        let missing = claude_pids_missing_session_files(&adapter, &platform, &seen, now);
        assert_eq!(missing, vec![10]);
    }

    #[test]
    fn doctor_warns_when_launchd_plist_points_into_target() {
        let home = TestHome::new();
        let paths = AppPaths::from_home(home.0.clone());

        let target_bin = home.0.join("src/restowa/target/release/restowa");
        std::fs::create_dir_all(target_bin.parent().unwrap()).unwrap();
        std::fs::write(&target_bin, b"fake").unwrap();

        install_launchd_agent(&paths, &target_bin, 120, false).unwrap();

        let report = run_doctor(&paths, &EmptyPlatform);
        let check = report
            .checks
            .iter()
            .find(|c| c.name == "launchd")
            .expect("launchd check present");
        assert_eq!(check.status, CheckStatus::Warn);
        assert!(check.detail.contains("target/"));
    }
}
