use crate::error::{CoreError, Result};
use crate::model::{ProjectGroup, RestoreReport};
use crate::paths::AppPaths;

pub mod ghostty;
pub mod terminal_app;

pub trait TerminalBackend {
    fn name(&self) -> &'static str;
    fn available(&self) -> bool;
    /// Open sessions grouped by project: one window per project, one tab per session.
    fn open(&self, groups: &[ProjectGroup]) -> Result<RestoreReport>;
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
