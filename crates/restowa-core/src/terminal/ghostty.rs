use crate::error::{CoreError, Result};
use crate::model::{ProjectGroup, RestoreReport};
use crate::paths::AppPaths;
use crate::shell::applescript_escape;
use crate::terminal::{run_osascript, TerminalBackend};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

#[derive(Debug, Clone)]
pub struct GhosttyBackend {
    paths: AppPaths,
}

impl GhosttyBackend {
    pub fn new(paths: AppPaths) -> Self {
        Self { paths }
    }

    pub fn app_installed() -> bool {
        Path::new("/Applications/Ghostty.app").is_dir()
    }

    /// Returns `true` if `~/.config/ghostty/config` explicitly sets
    /// `macos-applescript = false`.
    pub fn is_applescript_disabled(config_path: &Path) -> bool {
        let Ok(raw) = std::fs::read_to_string(config_path) else {
            return false;
        };
        let mut disabled = false;
        for line in raw.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            if let Some((key, val)) = trimmed.split_once('=') {
                if key.trim() == "macos-applescript" {
                    disabled = val.trim().eq_ignore_ascii_case("false");
                }
            }
        }
        disabled
    }

    /// Generate AppleScript using Ghostty's `Ghostty.sdef` primitives:
    /// one window per `ProjectGroup`, one tab per session inside that window,
    /// with `initial working directory` and `initial input` terminated by
    /// `linefeed`.
    pub fn build_applescript(groups: &[ProjectGroup]) -> String {
        let mut script = String::from("tell application \"Ghostty\"\n");
        for (g_idx, group) in groups.iter().enumerate() {
            for (i_idx, item) in group.items.iter().enumerate() {
                let cfg_var = format!("cfg_{g_idx}_{i_idx}");
                let win_var = format!("win_{g_idx}");
                let cwd_esc = applescript_escape(&item.cwd);
                let cmd_esc = applescript_escape(&item.shell_command);

                script.push_str(&format!("    set {cfg_var} to new surface configuration\n"));
                script.push_str(&format!(
                    "    set initial working directory of {cfg_var} to \"{cwd_esc}\"\n"
                ));
                script.push_str(&format!(
                    "    set initial input of {cfg_var} to \"{cmd_esc}\" & linefeed\n"
                ));
                if i_idx == 0 {
                    script.push_str(&format!(
                        "    set {win_var} to new window with configuration {cfg_var}\n"
                    ));
                } else {
                    script.push_str(&format!(
                        "    new tab in {win_var} with configuration {cfg_var}\n"
                    ));
                }
            }
        }
        script.push_str("end tell\n");
        script
    }

    pub fn build_focus_applescript(tty: &str) -> String {
        let tty = applescript_escape(tty);
        format!(
            "if application \"Ghostty\" is not running then return \"missing\"\n\
             tell application \"Ghostty\"\n\
             \x20   set matches to every terminal whose tty is \"{tty}\"\n\
             \x20   if (count of matches) is 0 then return \"missing\"\n\
             \x20   focus item 1 of matches\n\
             \x20   activate\n\
             end tell\n\
             return \"ok\"\n"
        )
    }
}

impl TerminalBackend for GhosttyBackend {
    fn name(&self) -> &'static str {
        "ghostty"
    }

    fn available(&self) -> bool {
        Self::app_installed() && !Self::is_applescript_disabled(&self.paths.ghostty_config())
    }

    fn open(&self, groups: &[ProjectGroup]) -> Result<RestoreReport> {
        if !self.available() {
            return Err(CoreError::TerminalUnavailable {
                name: self.name().to_string(),
            });
        }

        let mut report = RestoreReport {
            terminal: self.name().to_string(),
            dry_run: false,
            ..Default::default()
        };

        if groups.is_empty() {
            return Ok(report);
        }

        let script = Self::build_applescript(groups);
        let mut child = Command::new("/usr/bin/osascript")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| CoreError::TerminalFailed {
                name: self.name().to_string(),
                message: format!("failed to launch osascript: {e}"),
            })?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(script.as_bytes())
                .map_err(|e| CoreError::TerminalFailed {
                    name: self.name().to_string(),
                    message: format!("failed writing AppleScript to osascript: {e}"),
                })?;
        }

        let output = child
            .wait_with_output()
            .map_err(|e| CoreError::TerminalFailed {
                name: self.name().to_string(),
                message: format!("osascript wait failed: {e}"),
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(CoreError::TerminalFailed {
                name: self.name().to_string(),
                message: if stderr.is_empty() {
                    format!("osascript exited with status {}", output.status)
                } else {
                    stderr
                },
            });
        }

        for group in groups {
            report.restored.extend(group.items.iter().cloned());
        }
        Ok(report)
    }

    fn focus_tty(&self, tty: &str) -> Result<bool> {
        if !self.available() {
            return Ok(false);
        }
        let out = run_osascript(self.name(), &Self::build_focus_applescript(tty))?;
        Ok(out == "ok")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{RestoreItem, Tool};
    use crate::shell::{build_tab_command, expand_resume_template};

    #[test]
    fn focus_script_targets_the_tty_and_never_launches_ghostty() {
        let script = GhosttyBackend::build_focus_applescript("/dev/ttys004");
        assert!(script.starts_with("if application \"Ghostty\" is not running then return"));
        assert!(script.contains("every terminal whose tty is \"/dev/ttys004\""));
        assert!(script.contains("focus item 1 of matches"));
        assert!(GhosttyBackend::build_focus_applescript("a\"b").contains("tty is \"a\\\"b\""));
    }

    #[test]
    fn builds_one_window_per_group_and_tabs_for_extra_sessions() {
        let cmd1 = build_tab_command(
            "/Users/dev/Security Research/proj'1",
            &expand_resume_template("claude --resume {id}", "id-1"),
        );
        let cmd2 = build_tab_command(
            "/Users/dev/Security Research/proj'1",
            &expand_resume_template("claude --resume {id}", "id-2"),
        );
        let cmd3 = build_tab_command(
            "/Users/dev/other",
            &expand_resume_template("claude --resume {id}", "id-3"),
        );

        let groups = vec![
            ProjectGroup {
                cwd: "/Users/dev/Security Research/proj'1".to_string(),
                most_recent_activity: 200,
                items: vec![
                    RestoreItem {
                        key: "claude:id-1".into(),
                        tool: Tool::Claude,
                        session_id: "id-1".into(),
                        cwd: "/Users/dev/Security Research/proj'1".into(),
                        title: None,
                        best_guess: false,
                        resume_command: "claude --resume 'id-1'".into(),
                        shell_command: cmd1,
                    },
                    RestoreItem {
                        key: "claude:id-2".into(),
                        tool: Tool::Claude,
                        session_id: "id-2".into(),
                        cwd: "/Users/dev/Security Research/proj'1".into(),
                        title: None,
                        best_guess: false,
                        resume_command: "claude --resume 'id-2'".into(),
                        shell_command: cmd2,
                    },
                ],
            },
            ProjectGroup {
                cwd: "/Users/dev/other".to_string(),
                most_recent_activity: 100,
                items: vec![RestoreItem {
                    key: "claude:id-3".into(),
                    tool: Tool::Claude,
                    session_id: "id-3".into(),
                    cwd: "/Users/dev/other".into(),
                    title: None,
                    best_guess: false,
                    resume_command: "claude --resume 'id-3'".into(),
                    shell_command: cmd3,
                }],
            },
        ];

        let script = GhosttyBackend::build_applescript(&groups);
        assert!(script.contains("tell application \"Ghostty\""));
        assert!(script.contains("set win_0 to new window with configuration cfg_0_0"));
        assert!(script.contains("new tab in win_0 with configuration cfg_0_1"));
        assert!(script.contains("set win_1 to new window with configuration cfg_1_0"));
        // Every initial input ends with & linefeed
        assert_eq!(script.matches("& linefeed").count(), 3);
        // Shell single-quote escape preserved inside AppleScript string
        assert!(script.contains("proj'\\\\''1"));
    }
}
