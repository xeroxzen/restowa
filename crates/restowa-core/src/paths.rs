use crate::error::{CoreError, Result};
use directories::BaseDirs;
use std::path::{Path, PathBuf};

/// Resolved filesystem paths for restowa and supported tools.
///
/// Always derived from `home_dir` so tests can point `HOME` (or construct
/// `AppPaths::from_home`) at a temporary directory and never touch the user's
/// real home folder.
#[derive(Debug, Clone)]
pub struct AppPaths {
    pub home: PathBuf,
    pub data_dir: PathBuf,
    pub registry_db: PathBuf,
    pub spool_file: PathBuf,
    pub config_file: PathBuf,
    pub log_file: PathBuf,
}

impl AppPaths {
    pub fn resolve() -> Result<Self> {
        let home = std::env::var_os("HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| BaseDirs::new().map(|b| b.home_dir().to_path_buf()))
            .ok_or(CoreError::NoHomeDir)?;
        Ok(Self::from_home(home))
    }

    pub fn from_home(home: PathBuf) -> Self {
        let data_dir = home.join("Library/Application Support/restowa");
        let registry_db = data_dir.join("registry.db");
        let spool_file = data_dir.join("spool.jsonl");
        let config_file = data_dir.join("config.toml");
        let log_file = home.join("Library/Logs/restowa/restowa.log");
        Self {
            home,
            data_dir,
            registry_db,
            spool_file,
            config_file,
            log_file,
        }
    }

    pub fn claude_dir(&self) -> PathBuf {
        self.home.join(".claude")
    }

    pub fn claude_settings(&self) -> PathBuf {
        self.claude_dir().join("settings.json")
    }

    pub fn claude_settings_backup(&self) -> PathBuf {
        self.claude_dir().join("settings.json.restowa.bak")
    }

    pub fn claude_sessions_dir(&self) -> PathBuf {
        self.claude_dir().join("sessions")
    }

    pub fn claude_projects_dir(&self) -> PathBuf {
        self.claude_dir().join("projects")
    }

    pub fn codex_dir(&self) -> PathBuf {
        self.home.join(".codex")
    }

    pub fn codex_sessions_dir(&self) -> PathBuf {
        self.codex_dir().join("sessions")
    }

    /// Return the Codex SQLite state database path (`state_5.sqlite`, or the
    /// highest-numbered `state_*.sqlite` present in `~/.codex/`).
    pub fn codex_state_db(&self) -> PathBuf {
        let default = self.codex_dir().join("state_5.sqlite");
        if default.is_file() {
            return default;
        }
        if let Ok(entries) = std::fs::read_dir(self.codex_dir()) {
            let mut best: Option<(u32, PathBuf)> = None;
            for entry in entries.flatten() {
                let name = entry.file_name();
                let s = name.to_string_lossy();
                if let Some(num_str) = s
                    .strip_prefix("state_")
                    .and_then(|rest| rest.strip_suffix(".sqlite"))
                {
                    if let Ok(num) = num_str.parse::<u32>() {
                        if best.as_ref().is_none_or(|(b, _)| num > *b) {
                            best = Some((num, entry.path()));
                        }
                    }
                }
            }
            if let Some((_, path)) = best {
                return path;
            }
        }
        default
    }

    pub fn opencode_db(&self) -> PathBuf {
        self.home.join(".local/share/opencode/opencode.db")
    }

    pub fn antigravity_cli_dir(&self) -> PathBuf {
        self.home.join(".gemini/antigravity-cli")
    }

    pub fn antigravity_summaries_db(&self) -> PathBuf {
        self.antigravity_cli_dir().join("conversation_summaries.db")
    }

    pub fn antigravity_conversations_dir(&self) -> PathBuf {
        self.antigravity_cli_dir().join("conversations")
    }

    pub fn antigravity_last_conversations(&self) -> PathBuf {
        self.antigravity_cli_dir()
            .join("cache/last_conversations.json")
    }

    pub fn ghostty_config(&self) -> PathBuf {
        self.home.join(".config/ghostty/config")
    }

    pub fn launch_agent_plist(&self) -> PathBuf {
        self.home
            .join("Library/LaunchAgents/dev.restowa.scan.plist")
    }

    pub fn ensure_parent(path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| CoreError::IoPath {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        Ok(())
    }
}
