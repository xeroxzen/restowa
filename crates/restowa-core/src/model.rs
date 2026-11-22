use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tool {
    Claude,
    Codex,
    Opencode,
    Antigravity,
}

impl Tool {
    pub fn as_str(self) -> &'static str {
        match self {
            Tool::Claude => "claude",
            Tool::Codex => "codex",
            Tool::Opencode => "opencode",
            Tool::Antigravity => "antigravity",
        }
    }

    pub fn all() -> &'static [Tool] {
        &[Tool::Claude, Tool::Codex, Tool::Opencode, Tool::Antigravity]
    }
}

impl fmt::Display for Tool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.as_str())
    }
}

impl FromStr for Tool {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "claude" => Ok(Tool::Claude),
            "codex" => Ok(Tool::Codex),
            "opencode" => Ok(Tool::Opencode),
            "antigravity" | "agy" => Ok(Tool::Antigravity),
            other => Err(format!(
                "unknown tool '{other}' (expected: claude, codex, opencode, antigravity)"
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionStatus {
    Live,
    Closed,
    Missing,
}

impl SessionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            SessionStatus::Live => "live",
            SessionStatus::Closed => "closed",
            SessionStatus::Missing => "missing",
        }
    }
}

impl fmt::Display for SessionStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.as_str())
    }
}

impl FromStr for SessionStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "live" => Ok(SessionStatus::Live),
            "closed" => Ok(SessionStatus::Closed),
            "missing" => Ok(SessionStatus::Missing),
            other => Err(format!("unknown session status '{other}'")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionSource {
    Hook,
    Scan,
}

impl SessionSource {
    pub fn as_str(self) -> &'static str {
        match self {
            SessionSource::Hook => "hook",
            SessionSource::Scan => "scan",
        }
    }
}

impl fmt::Display for SessionSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.as_str())
    }
}

impl FromStr for SessionSource {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "hook" => Ok(SessionSource::Hook),
            "scan" => Ok(SessionSource::Scan),
            other => Err(format!("unknown session source '{other}'")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub key: String,
    pub tool: Tool,
    pub session_id: String,
    pub cwd: String,
    pub title: Option<String>,
    pub transcript: Option<String>,
    pub pid: Option<i64>,
    pub pid_start_us: Option<i64>,
    pub status: SessionStatus,
    pub source: SessionSource,
    pub first_seen: i64,
    pub last_seen_live: Option<i64>,
    pub last_boot_id: Option<String>,
    pub ended_by_user: bool,
    pub best_guess: bool,
    pub pinned: bool,
    pub hidden: bool,
}

impl Session {
    pub fn make_key(tool: Tool, session_id: &str) -> String {
        format!("{}:{}", tool.as_str(), session_id)
    }

    pub fn short_id(&self) -> &str {
        let len = self.session_id.len().min(8);
        &self.session_id[..len]
    }

    pub fn project_name(&self) -> &str {
        std::path::Path::new(&self.cwd)
            .file_name()
            .and_then(|s| s.to_str())
            .filter(|s| !s.is_empty())
            .unwrap_or(&self.cwd)
    }
}

/// Process information collected from the OS.
///
/// Intentionally omits the short process name (`comm`) so adapters can only
/// identify processes by `argv` and `exe_path`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcInfo {
    pub pid: i32,
    pub ppid: i32,
    pub start_time_us: i64,
    pub exe_path: Option<PathBuf>,
    pub argv: Vec<String>,
    pub cwd: Option<PathBuf>,
}

/// Session resolved from a live process by an adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRef {
    pub tool: Tool,
    pub session_id: String,
    pub cwd: String,
    pub transcript: Option<String>,
    pub title_hint: Option<String>,
    pub best_guess: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreItem {
    pub key: String,
    pub tool: Tool,
    pub session_id: String,
    pub cwd: String,
    pub title: Option<String>,
    pub resume_command: String,
    pub shell_command: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectGroup {
    pub cwd: String,
    pub most_recent_activity: i64,
    pub items: Vec<RestoreItem>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedItem {
    pub key: String,
    pub cwd: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailedItem {
    pub key: String,
    pub cwd: String,
    pub reason: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreReport {
    pub terminal: String,
    pub dry_run: bool,
    pub restored: Vec<RestoreItem>,
    pub skipped: Vec<SkippedItem>,
    pub failed: Vec<FailedItem>,
}
