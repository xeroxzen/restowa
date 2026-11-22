use crate::adapters::claude::ClaudeAdapter;
use crate::adapters::Adapter;
use crate::hooks::inspect_claude_hooks;
use crate::paths::AppPaths;
use crate::platform::Platform;
use crate::registry::Registry;
use crate::terminal::ghostty::GhosttyBackend;
use crate::terminal::terminal_app::TerminalAppBackend;
use crate::terminal::TerminalBackend;
use serde::Serialize;
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
    let claude_sessions_dir = paths.claude_sessions_dir();
    let claude_adapter = ClaudeAdapter::new(paths.clone());
    let live_claude_pids: Vec<i32> = platform
        .list_processes()
        .unwrap_or_default()
        .into_iter()
        .filter(|p| claude_adapter.matches_process(p))
        .map(|p| p.pid)
        .collect();

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
        let missing_pids: Vec<i32> = live_claude_pids
            .iter()
            .copied()
            .filter(|&pid| !claude_adapter.has_pid_session_file(pid))
            .collect();
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
        } else if !any_json && live_claude_pids.is_empty() {
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
                    live_claude_pids.len()
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

    // 5. Launchd agent (installed in Phase 2)
    let plist = paths.launch_agent_plist();
    if plist.is_file() {
        checks.push(DoctorCheck {
            name: "launchd".to_string(),
            status: CheckStatus::Ok,
            detail: format!("agent plist present at {}", plist.display()),
        });
    } else {
        checks.push(DoctorCheck {
            name: "launchd".to_string(),
            status: CheckStatus::Warn,
            detail: "launchd scan agent not installed yet (Phase 2)".to_string(),
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

    let has_errors = checks.iter().any(|c| c.status == CheckStatus::Error);
    let has_warnings = checks.iter().any(|c| c.status == CheckStatus::Warn);

    DoctorReport {
        checks,
        has_errors,
        has_warnings,
    }
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
