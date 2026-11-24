use crate::error::{CoreError, Result};
use crate::model::{ProjectGroup, RestoreReport};
use crate::paths::AppPaths;
use std::io::Write;
use std::process::{Command, Stdio};

pub mod ghostty;
pub mod terminal_app;

pub trait TerminalBackend {
    fn name(&self) -> &'static str;
    fn available(&self) -> bool;
    /// Open sessions grouped by project: one window per project, one tab per session.
    fn open(&self, groups: &[ProjectGroup]) -> Result<RestoreReport>;
    /// Bring the tab whose terminal device is `tty` to the front. Returns
    /// `false` when the app isn't running or no tab has that tty. Never
    /// launches the app.
    fn focus_tty(&self, tty: &str) -> Result<bool>;
}

/// Runs `script` through osascript and returns its trimmed stdout.
pub(crate) fn run_osascript(terminal: &str, script: &str) -> Result<String> {
    let failed = |message: String| CoreError::TerminalFailed {
        name: terminal.to_string(),
        message,
    };
    let mut child = Command::new("/usr/bin/osascript")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| failed(format!("failed to launch osascript: {e}")))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(script.as_bytes())
            .map_err(|e| failed(format!("failed writing AppleScript to osascript: {e}")))?;
    }
    let output = child
        .wait_with_output()
        .map_err(|e| failed(format!("osascript wait failed: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(failed(if stderr.is_empty() {
            format!("osascript exited with status {}", output.status)
        } else {
            stderr
        }));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

pub fn select_backend(name: &str, paths: &AppPaths) -> Result<Box<dyn TerminalBackend>> {
    match name.to_ascii_lowercase().as_str() {
        "ghostty" => Ok(Box::new(ghostty::GhosttyBackend::new(paths.clone()))),
        "terminal" | "terminal.app" => Ok(Box::new(terminal_app::TerminalAppBackend)),
        other => Err(CoreError::TerminalUnavailable {
            name: other.to_string(),
        }),
    }
}
