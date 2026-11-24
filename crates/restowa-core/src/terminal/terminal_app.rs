use crate::error::{CoreError, Result};
use crate::model::{ProjectGroup, RestoreReport};
use crate::shell::applescript_escape;
use crate::terminal::{run_osascript, TerminalBackend};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

#[derive(Debug, Default, Clone, Copy)]
pub struct TerminalAppBackend;

impl TerminalAppBackend {
    pub fn build_applescript(groups: &[ProjectGroup]) -> String {
        let mut script = String::from("tell application \"Terminal\"\n    activate\n");
        for (g_idx, group) in groups.iter().enumerate() {
            for (i_idx, item) in group.items.iter().enumerate() {
                let cmd_esc = applescript_escape(&item.shell_command);
                if i_idx == 0 {
                    script.push_str(&format!(
                        "    set tab_{g_idx}_0 to do script \"{cmd_esc}\"\n"
                    ));
                    script.push_str(&format!("    set win_{g_idx} to front window\n"));
                } else {
                    script.push_str(&format!("    do script \"{cmd_esc}\" in win_{g_idx}\n"));
                }
            }
        }
        script.push_str("end tell\n");
        script
    }

    /// Loops are bounded by the window and tab counts.
    pub fn build_focus_applescript(tty: &str) -> String {
        let tty = applescript_escape(tty);
        format!(
            "if application \"Terminal\" is not running then return \"missing\"\n\
             tell application \"Terminal\"\n\
             \x20   repeat with w in windows\n\
             \x20       repeat with t in tabs of w\n\
             \x20           if tty of t is \"{tty}\" then\n\
             \x20               set selected of t to true\n\
             \x20               set index of w to 1\n\
             \x20               activate\n\
             \x20               return \"ok\"\n\
             \x20           end if\n\
             \x20       end repeat\n\
             \x20   end repeat\n\
             end tell\n\
             return \"missing\"\n"
        )
    }
}

impl TerminalBackend for TerminalAppBackend {
    fn name(&self) -> &'static str {
        "terminal"
    }

    fn available(&self) -> bool {
        Path::new("/System/Applications/Utilities/Terminal.app").is_dir()
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

    #[test]
    fn focus_script_selects_matching_tab_and_never_launches_terminal() {
        let script = TerminalAppBackend::build_focus_applescript("/dev/ttys004");
        assert!(script.starts_with("if application \"Terminal\" is not running then return"));
        assert!(script.contains("if tty of t is \"/dev/ttys004\" then"));
        assert!(script.contains("set selected of t to true"));
        assert!(script.trim_end().ends_with("return \"missing\""));
    }
}
