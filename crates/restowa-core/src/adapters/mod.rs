use crate::config::ToolConfig;
use crate::doctor::DoctorCheck;
use crate::error::Result;
use crate::model::{ProcInfo, Session, SessionRef, Tool};
use crate::paths::AppPaths;
use crate::platform::Platform;
use std::collections::HashSet;

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
    /// `claimed_ids` holds session IDs already assigned to other live processes
    /// of the same tool in this scan pass so fallback resolution can assign
    /// distinct candidate sessions (marking them `best_guess`).
    fn session_for_process(
        &self,
        p: &ProcInfo,
        platform: &dyn Platform,
        claimed_ids: &HashSet<String>,
    ) -> Result<Option<SessionRef>>;
    /// Does the tool's session data still exist on disk?
    fn session_exists(&self, s: &Session, tool_running: bool) -> bool;
    /// Optional: title derived from the first user prompt (≤ 80 chars).
    fn title(&self, s: &Session, tool_running: bool) -> Option<String>;
    /// Shell-escaped resume command, run inside `s.cwd`.
    fn resume_command(&self, s: &Session, cfg: &ToolConfig) -> String;
    /// Optional adapter-specific health checks (such as external DB schema verification).
    fn doctor_checks(&self, tool_running: bool) -> Vec<DoctorCheck> {
        let _ = tool_running;
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

/// Detect whether `p` belongs to ANY process of a tracked tool (including
/// background helpers such as `codex app-server`, `opencode serve`, or
/// `agy acp`), matching strictly by `exe_path` and `argv` (never `comm`).
///
/// Used to enforce the `immutable=1` guard: SQLite `immutable=1` is permitted
/// only when zero processes of the owning tool are running.
pub fn owning_tool_of_process(p: &ProcInfo) -> Option<Tool> {
    let exe_name = p
        .exe_path
        .as_ref()
        .and_then(|e| e.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("");
    let argv0_name = p
        .argv
        .first()
        .and_then(|a| std::path::Path::new(a).file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("");

    let is_agy = |name: &str| name == "agy" || (name.starts_with("agy.") && name.ends_with(".old"));
    if is_agy(exe_name) || is_agy(argv0_name) {
        return Some(Tool::Antigravity);
    }

    if exe_name == "opencode" || argv0_name == "opencode" {
        return Some(Tool::Opencode);
    }

    let is_codex = exe_name == "codex"
        || argv0_name == "codex"
        || ((exe_name == "node" || argv0_name == "node")
            && p.argv.get(1).is_some_and(|a| {
                std::path::Path::new(a)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n == "codex" || n == "codex.js")
            }));
    if is_codex {
        return Some(Tool::Codex);
    }

    let is_claude = exe_name == "claude"
        || argv0_name == "claude"
        || p.exe_path
            .as_ref()
            .is_some_and(|e| e.to_string_lossy().contains("/claude/versions/"));
    if is_claude {
        return Some(Tool::Claude);
    }

    None
}
