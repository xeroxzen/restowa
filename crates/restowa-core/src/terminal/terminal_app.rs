use crate::error::{CoreError, Result};
use crate::model::{ProjectGroup, RestoreReport};
use crate::shell::applescript_escape;
use crate::terminal::TerminalBackend;
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
}
