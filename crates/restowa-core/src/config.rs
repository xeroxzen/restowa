use crate::error::{CoreError, Result};
use crate::model::Tool;
use crate::paths::AppPaths;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolConfig {
    pub resume: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolsConfig {
    #[serde(default = "default_claude_config")]
    pub claude: ToolConfig,
    #[serde(default = "default_codex_config")]
    pub codex: ToolConfig,
    #[serde(default = "default_opencode_config")]
    pub opencode: ToolConfig,
    #[serde(default = "default_antigravity_config")]
    pub antigravity: ToolConfig,
}

fn default_claude_config() -> ToolConfig {
    ToolConfig {
        resume: "claude --resume {id}".to_string(),
    }
}

fn default_codex_config() -> ToolConfig {
    ToolConfig {
        resume: "codex resume {id}".to_string(),
    }
}

fn default_opencode_config() -> ToolConfig {
    ToolConfig {
        resume: "opencode --session {id}".to_string(),
    }
}

fn default_antigravity_config() -> ToolConfig {
    ToolConfig {
        resume: "agy --conversation {id}".to_string(),
    }
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            claude: default_claude_config(),
            codex: default_codex_config(),
            opencode: default_opencode_config(),
            antigravity: default_antigravity_config(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_terminal")]
    pub terminal: String,
    #[serde(default)]
    pub tools: ToolsConfig,
}

fn default_terminal() -> String {
    "ghostty".to_string()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            terminal: default_terminal(),
            tools: ToolsConfig::default(),
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(path).map_err(|source| CoreError::IoPath {
            path: path.to_path_buf(),
            source,
        })?;
        let cfg: Config = toml::from_str(&raw)?;
        Ok(cfg)
    }

    pub fn to_toml_string(&self) -> Result<String> {
        Ok(toml::to_string_pretty(self)?)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        AppPaths::ensure_parent(path)?;
        let content = self.to_toml_string()?;
        std::fs::write(path, content).map_err(|source| CoreError::IoPath {
            path: path.to_path_buf(),
            source,
        })?;
        Ok(())
    }

    pub fn tool_config(&self, tool: Tool) -> &ToolConfig {
        match tool {
            Tool::Claude => &self.tools.claude,
            Tool::Codex => &self.tools.codex,
            Tool::Opencode => &self.tools.opencode,
            Tool::Antigravity => &self.tools.antigravity,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_roundtrips_toml() {
        let cfg = Config::default();
        let raw = toml::to_string_pretty(&cfg).unwrap();
        let parsed: Config = toml::from_str(&raw).unwrap();
        assert_eq!(cfg, parsed);
        assert_eq!(parsed.tools.claude.resume, "claude --resume {id}");
        assert_eq!(parsed.tools.antigravity.resume, "agy --conversation {id}");
    }

    #[test]
    fn partial_toml_fills_defaults() {
        let raw = r#"
[tools.claude]
resume = "claude --dangerously-skip-permissions --resume {id}"
"#;
        let parsed: Config = toml::from_str(raw).unwrap();
        assert_eq!(parsed.terminal, "ghostty");
        assert_eq!(
            parsed.tools.claude.resume,
            "claude --dangerously-skip-permissions --resume {id}"
        );
        assert_eq!(parsed.tools.codex.resume, "codex resume {id}");
    }
}
