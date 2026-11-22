use crate::config::ToolConfig;
use crate::doctor::DoctorCheck;
use crate::error::Result;
use crate::model::{ProcInfo, Session, SessionRef, Tool};
use crate::paths::AppPaths;
use crate::platform::Platform;

pub mod antigravity;
pub mod claude;
pub mod codex;
pub mod opencode;
pub mod sqlite_ro;

pub trait Adapter {
    fn tool(&self) -> Tool;
    /// Is this running process one of ours? Matches on `exe_path` and `argv`
    /// only — never on process name (`comm`).
    fn matches_process(&self, p: &ProcInfo) -> bool;
    /// Given a live process, return the session it is running, if knowable.
    fn session_for_process(
        &self,
        p: &ProcInfo,
        platform: &dyn Platform,
    ) -> Result<Option<SessionRef>>;
    /// Does the tool's session data still exist on disk?
    fn session_exists(&self, s: &Session) -> bool;
    /// Optional: title derived from the first user prompt (≤ 80 chars).
    fn title(&self, s: &Session) -> Option<String>;
    /// Shell-escaped resume command, run inside `s.cwd`.
    fn resume_command(&self, s: &Session, cfg: &ToolConfig) -> String;
    /// Optional adapter-specific health checks (such as external DB schema verification).
    fn doctor_checks(&self) -> Vec<DoctorCheck> {
        Vec::new()
    }
}

pub fn adapters_for_paths(paths: &AppPaths) -> Vec<Box<dyn Adapter>> {
    vec![
        Box::new(claude::ClaudeAdapter::new(paths.clone())),
        Box::new(codex::CodexAdapter::new(paths.clone())),
        Box::new(opencode::OpenCodeAdapter::new(paths.clone())),
        Box::new(antigravity::AntigravityAdapter::new(paths.clone())),
    ]
}
