use crate::config::ToolConfig;
use crate::error::Result;
use crate::model::{ProcInfo, Session, SessionRef, Tool};
use crate::paths::AppPaths;

pub mod claude;

pub trait Adapter {
    fn tool(&self) -> Tool;
    /// Is this running process one of ours? Matches on `exe_path` and `argv`
    /// only — never on process name (`comm`).
    fn matches_process(&self, p: &ProcInfo) -> bool;
    /// Given a live process, return the session it is running, if knowable.
    fn session_for_process(&self, p: &ProcInfo) -> Result<Option<SessionRef>>;
    /// Does the tool's session data still exist on disk?
    fn session_exists(&self, s: &Session) -> bool;
    /// Optional: title derived from the first user prompt (≤ 80 chars).
    fn title(&self, s: &Session) -> Option<String>;
    /// Shell-escaped resume command, run inside `s.cwd`.
    fn resume_command(&self, s: &Session, cfg: &ToolConfig) -> String;
}

pub fn adapters_for_paths(paths: &AppPaths) -> Vec<Box<dyn Adapter>> {
    vec![Box::new(claude::ClaudeAdapter::new(paths.clone()))]
}
