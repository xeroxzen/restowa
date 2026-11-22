use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("I/O error at {path}: {source}")]
    IoPath {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("TOML deserialize error: {0}")]
    TomlDe(#[from] toml::de::Error),

    #[error("TOML serialize error: {0}")]
    TomlSer(#[from] toml::ser::Error),

    #[error("could not determine user home directory")]
    NoHomeDir,

    #[error("session not found: {0}")]
    SessionNotFound(String),

    #[error("ambiguous session key prefix '{prefix}' matches: {matches}")]
    AmbiguousKey { prefix: String, matches: String },

    #[error("invalid Claude settings file at {path}: {reason}")]
    InvalidSettings { path: PathBuf, reason: String },

    #[error("terminal backend '{name}' is not available")]
    TerminalUnavailable { name: String },

    #[error("terminal backend '{name}' failed: {message}")]
    TerminalFailed { name: String, message: String },

    #[error("platform error: {0}")]
    Platform(String),
}

pub type Result<T> = std::result::Result<T, CoreError>;
