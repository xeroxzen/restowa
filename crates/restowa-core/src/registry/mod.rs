use crate::error::{CoreError, Result};
use crate::model::{Session, SessionSource, SessionStatus, Tool};
use crate::paths::AppPaths;
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, Row};
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

const MIGRATION_0001: &str = include_str!("migrations/0001_init.sql");

pub struct Registry {
    conn: Connection,
}

#[derive(Debug, Clone)]
pub struct HookSessionUpsert {
    pub tool: Tool,
    pub session_id: String,
    pub cwd: String,
    pub transcript: Option<String>,
    pub pid: Option<i64>,
    pub pid_start_us: Option<i64>,
    pub boot_id: String,
    pub now: i64,
}

#[derive(Debug, Clone)]
pub struct ScanSessionUpsert {
    pub tool: Tool,
    pub session_id: String,
    pub cwd: String,
    pub transcript: Option<String>,
    pub title_hint: Option<String>,
    pub pid: i64,
    pub pid_start_us: i64,
    pub boot_id: String,
    pub now: i64,
}

impl Registry {
    pub fn open(path: &Path) -> Result<Self> {
        AppPaths::ensure_parent(path)?;
        let conn = Connection::open(path)?;
        Self::configure_and_migrate(conn)
    }

    pub fn open_read_only(path: &Path) -> Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let conn = Connection::open_with_flags(path, flags)?;
        conn.busy_timeout(Duration::from_secs(2))?;
        Ok(Some(Self { conn }))
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::configure_and_migrate(conn)
    }

    fn configure_and_migrate(conn: Connection) -> Result<Self> {
        conn.busy_timeout(Duration::from_secs(2))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version < 1 {
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch(MIGRATION_0001)?;
            tx.pragma_update(None, "user_version", 1i64)?;
            tx.commit()?;
        }
        Ok(Self { conn })
    }

    pub fn record_boot(&mut self, boot_id: &str, now: i64) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO boots (boot_id, first_seen) VALUES (?1, ?2)",
            params![boot_id, now],
        )?;
        Ok(())
    }

    /// Return the most recent boot_id strictly older than `current_boot_id`.
    pub fn previous_boot_id(&self, current_boot_id: &str) -> Result<Option<String>> {
        let boot: Option<String> = self
            .conn
            .query_row(
                "SELECT boot_id FROM boots
                 WHERE boot_id != ?1
                 ORDER BY first_seen DESC
                 LIMIT 1",
                params![current_boot_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(boot)
    }

    /// Upsert a session started via hook (`source = 'hook'`).
    /// Preserves user-controlled fields (`title`, `pinned`, `hidden`) and `first_seen`.
    pub fn upsert_hook_start(&mut self, input: &HookSessionUpsert) -> Result<()> {
        let key = Session::make_key(input.tool, &input.session_id);
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT OR IGNORE INTO boots (boot_id, first_seen) VALUES (?1, ?2)",
            params![&input.boot_id, input.now],
        )?;
        tx.execute(
            "INSERT INTO sessions (
                key, tool, session_id, cwd, title, transcript,
                pid, pid_start_us, status, source,
                first_seen, last_seen_live, last_boot_id, pinned, hidden
            ) VALUES (
                ?1, ?2, ?3, ?4, NULL, ?5,
                ?6, ?7, 'live', 'hook',
                ?8, ?8, ?9, 0, 0
            )
            ON CONFLICT(key) DO UPDATE SET
                cwd = excluded.cwd,
                transcript = COALESCE(excluded.transcript, sessions.transcript),
                pid = excluded.pid,
                pid_start_us = excluded.pid_start_us,
                status = 'live',
                source = 'hook',
                last_seen_live = excluded.last_seen_live,
                last_boot_id = excluded.last_boot_id",
            params![
                key,
                input.tool.as_str(),
                input.session_id,
                input.cwd,
                input.transcript,
                input.pid,
                input.pid_start_us,
                input.now,
                input.boot_id,
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Mark a session closed via hook (`SessionEnd`).
    /// Keeps `last_seen_live` and `last_boot_id` intact.
    pub fn record_hook_end(
        &mut self,
        tool: Tool,
        session_id: &str,
        cwd: &str,
        transcript: Option<&str>,
        boot_id: &str,
        now: i64,
    ) -> Result<()> {
        let key = Session::make_key(tool, session_id);
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT OR IGNORE INTO boots (boot_id, first_seen) VALUES (?1, ?2)",
            params![boot_id, now],
        )?;
        tx.execute(
            "INSERT INTO sessions (
                key, tool, session_id, cwd, title, transcript,
                pid, pid_start_us, status, source,
                first_seen, last_seen_live, last_boot_id, pinned, hidden
            ) VALUES (
                ?1, ?2, ?3, ?4, NULL, ?5,
                NULL, NULL, 'closed', 'hook',
                ?6, ?6, ?7, 0, 0
            )
            ON CONFLICT(key) DO UPDATE SET
                cwd = CASE WHEN excluded.cwd != '' THEN excluded.cwd ELSE sessions.cwd END,
                transcript = COALESCE(excluded.transcript, sessions.transcript),
                pid = NULL,
                pid_start_us = NULL,
                status = 'closed',
                source = 'hook'",
            params![
                key,
                tool.as_str(),
                session_id,
                cwd,
                transcript,
                now,
                boot_id
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Upsert a live session discovered by scanning.
    ///
    /// Never overwrites `source = 'hook'` or user-controlled fields (`title`, `pinned`, `hidden`).
    pub fn upsert_scan_live(&mut self, input: &ScanSessionUpsert) -> Result<()> {
        let key = Session::make_key(input.tool, &input.session_id);
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT OR IGNORE INTO boots (boot_id, first_seen) VALUES (?1, ?2)",
            params![&input.boot_id, input.now],
        )?;
        tx.execute(
            "INSERT INTO sessions (
                key, tool, session_id, cwd, title, transcript,
                pid, pid_start_us, status, source,
                first_seen, last_seen_live, last_boot_id, pinned, hidden
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6,
                ?7, ?8, 'live', 'scan',
                ?9, ?9, ?10, 0, 0
            )
            ON CONFLICT(key) DO UPDATE SET
                pid = excluded.pid,
                pid_start_us = excluded.pid_start_us,
                status = 'live',
                last_seen_live = excluded.last_seen_live,
                last_boot_id = excluded.last_boot_id,
                transcript = COALESCE(sessions.transcript, excluded.transcript)",
            params![
                key,
                input.tool.as_str(),
                input.session_id,
                input.cwd,
                input.title_hint,
                input.transcript,
                input.pid,
                input.pid_start_us,
                input.now,
                input.boot_id,
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn mark_closed(&mut self, key: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET status = 'closed', pid = NULL, pid_start_us = NULL WHERE key = ?1",
            params![key],
        )?;
        Ok(())
    }

    pub fn mark_missing(&mut self, key: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET status = 'missing', pid = NULL, pid_start_us = NULL WHERE key = ?1",
            params![key],
        )?;
        Ok(())
    }

    pub fn set_pinned(&mut self, key: &str, pinned: bool) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE sessions SET pinned = ?1 WHERE key = ?2",
            params![i64::from(pinned), key],
        )?;
        if changed == 0 {
            return Err(CoreError::SessionNotFound(key.to_string()));
        }
        Ok(())
    }

    pub fn set_title(&mut self, key: &str, title: &str) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE sessions SET title = ?1 WHERE key = ?2",
            params![title, key],
        )?;
        if changed == 0 {
            return Err(CoreError::SessionNotFound(key.to_string()));
        }
        Ok(())
    }

    pub fn set_hidden(&mut self, key: &str, hidden: bool) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE sessions SET hidden = ?1 WHERE key = ?2",
            params![i64::from(hidden), key],
        )?;
        if changed == 0 {
            return Err(CoreError::SessionNotFound(key.to_string()));
        }
        Ok(())
    }

    pub fn list_all(&self, include_hidden: bool) -> Result<Vec<Session>> {
        let sql = if include_hidden {
            "SELECT key, tool, session_id, cwd, title, transcript,
                    pid, pid_start_us, status, source,
                    first_seen, last_seen_live, last_boot_id, pinned, hidden
             FROM sessions
             ORDER BY COALESCE(last_seen_live, first_seen) DESC, key ASC"
        } else {
            "SELECT key, tool, session_id, cwd, title, transcript,
                    pid, pid_start_us, status, source,
                    first_seen, last_seen_live, last_boot_id, pinned, hidden
             FROM sessions
             WHERE hidden = 0
             ORDER BY COALESCE(last_seen_live, first_seen) DESC, key ASC"
        };
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map([], row_to_session)?;
        let mut sessions = Vec::new();
        for row in rows {
            sessions.push(row?);
        }
        Ok(sessions)
    }

    /// Restore set for a given previous boot ID:
    /// `status IN ('live','closed') AND last_boot_id = previous_boot_id AND hidden = 0`
    /// unioned with pinned non-missing, non-hidden sessions.
    pub fn list_last_boot(&self, previous_boot_id: Option<&str>) -> Result<Vec<Session>> {
        let mut stmt = self.conn.prepare(
            "SELECT key, tool, session_id, cwd, title, transcript,
                    pid, pid_start_us, status, source,
                    first_seen, last_seen_live, last_boot_id, pinned, hidden
             FROM sessions
             WHERE hidden = 0
               AND status IN ('live', 'closed')
               AND (pinned = 1 OR (?1 IS NOT NULL AND last_boot_id = ?1))
             ORDER BY COALESCE(last_seen_live, first_seen) DESC, key ASC",
        )?;
        let rows = stmt.query_map(params![previous_boot_id], row_to_session)?;
        let mut sessions = Vec::new();
        for row in rows {
            sessions.push(row?);
        }
        Ok(sessions)
    }

    pub fn get_by_key(&self, key: &str) -> Result<Option<Session>> {
        self.conn
            .query_row(
                "SELECT key, tool, session_id, cwd, title, transcript,
                        pid, pid_start_us, status, source,
                        first_seen, last_seen_live, last_boot_id, pinned, hidden
                 FROM sessions WHERE key = ?1",
                params![key],
                row_to_session,
            )
            .optional()
            .map_err(CoreError::from)
    }

    /// Resolve a user-provided key (`tool:session_id` or unique prefix of `session_id`).
    pub fn resolve_key(&self, input: &str) -> Result<Session> {
        let all = self.list_all(true)?;
        resolve_key_in_slice(&all, input)
    }
}

pub fn resolve_key_in_slice(sessions: &[Session], input: &str) -> Result<Session> {
    if let Some(exact) = sessions
        .iter()
        .find(|s| s.key == input || s.session_id == input)
    {
        return Ok(exact.clone());
    }

    let matches: Vec<&Session> = sessions
        .iter()
        .filter(|s| s.session_id.starts_with(input) || s.key.starts_with(input))
        .collect();

    match matches.len() {
        0 => Err(CoreError::SessionNotFound(input.to_string())),
        1 => Ok(matches[0].clone()),
        _ => {
            let list = matches
                .iter()
                .map(|s| s.key.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            Err(CoreError::AmbiguousKey {
                prefix: input.to_string(),
                matches: list,
            })
        }
    }
}

fn row_to_session(row: &Row<'_>) -> rusqlite::Result<Session> {
    let tool_str: String = row.get(1)?;
    let status_str: String = row.get(8)?;
    let source_str: String = row.get(9)?;
    let tool = Tool::from_str(&tool_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            1,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
        )
    })?;
    let status = SessionStatus::from_str(&status_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            8,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
        )
    })?;
    let source = SessionSource::from_str(&source_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            9,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
        )
    })?;
    Ok(Session {
        key: row.get(0)?,
        tool,
        session_id: row.get(2)?,
        cwd: row.get(3)?,
        title: row.get(4)?,
        transcript: row.get(5)?,
        pid: row.get(6)?,
        pid_start_us: row.get(7)?,
        status,
        source,
        first_seen: row.get(10)?,
        last_seen_live: row.get(11)?,
        last_boot_id: row.get(12)?,
        pinned: row.get::<_, i64>(13)? != 0,
        hidden: row.get::<_, i64>(14)? != 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_upsert_preserves_user_fields_and_scan_never_downgrades_hook() {
        let mut reg = Registry::open_in_memory().unwrap();
        reg.upsert_hook_start(&HookSessionUpsert {
            tool: Tool::Claude,
            session_id: "3f9a1c2e-1111-2222-3333-444444444444".to_string(),
            cwd: "/tmp/proj-a".to_string(),
            transcript: Some("/tmp/t.jsonl".to_string()),
            pid: Some(1234),
            pid_start_us: Some(999_000),
            boot_id: "100.000001".to_string(),
            now: 1_700_000_000,
        })
        .unwrap();

        let key = "claude:3f9a1c2e-1111-2222-3333-444444444444";
        reg.set_title(key, "Custom Title").unwrap();
        reg.set_pinned(key, true).unwrap();

        // Scanner sees the same session later; must preserve source='hook', title, pinned, first_seen
        reg.upsert_scan_live(&ScanSessionUpsert {
            tool: Tool::Claude,
            session_id: "3f9a1c2e-1111-2222-3333-444444444444".to_string(),
            cwd: "/tmp/proj-a".to_string(),
            transcript: None,
            title_hint: Some("Auto Title".to_string()),
            pid: 1234,
            pid_start_us: 999_000,
            boot_id: "100.000001".to_string(),
            now: 1_700_000_120,
        })
        .unwrap();

        let s = reg.get_by_key(key).unwrap().unwrap();
        assert_eq!(s.source, SessionSource::Hook);
        assert_eq!(s.title.as_deref(), Some("Custom Title"));
        assert!(s.pinned);
        assert_eq!(s.first_seen, 1_700_000_000);
        assert_eq!(s.last_seen_live, Some(1_700_000_120));
        assert_eq!(s.pid_start_us, Some(999_000));
    }

    #[test]
    fn key_prefix_resolution_handles_unique_and_ambiguous() {
        let mut reg = Registry::open_in_memory().unwrap();
        for id in ["aaaa1111-0000", "aaaa2222-0000", "bbbb1111-0000"] {
            reg.upsert_hook_start(&HookSessionUpsert {
                tool: Tool::Claude,
                session_id: id.to_string(),
                cwd: "/tmp/p".to_string(),
                transcript: None,
                pid: Some(10),
                pid_start_us: Some(100),
                boot_id: "1.0".to_string(),
                now: 1000,
            })
            .unwrap();
        }

        assert_eq!(reg.resolve_key("bbbb").unwrap().session_id, "bbbb1111-0000");
        assert_eq!(
            reg.resolve_key("claude:aaaa1111-0000").unwrap().session_id,
            "aaaa1111-0000"
        );
        assert!(matches!(
            reg.resolve_key("aaaa"),
            Err(CoreError::AmbiguousKey { .. })
        ));
        assert!(matches!(
            reg.resolve_key("zzzz"),
            Err(CoreError::SessionNotFound(_))
        ));
    }

    #[test]
    fn last_boot_query_unions_previous_boot_and_pinned() {
        let mut reg = Registry::open_in_memory().unwrap();
        reg.record_boot("boot-1", 100).unwrap();
        reg.record_boot("boot-2", 200).unwrap();
        reg.record_boot("boot-3", 300).unwrap();

        // Session from boot-1 (pinned)
        reg.upsert_hook_start(&HookSessionUpsert {
            tool: Tool::Claude,
            session_id: "s-boot1-pinned".to_string(),
            cwd: "/tmp/a".to_string(),
            transcript: None,
            pid: None,
            pid_start_us: None,
            boot_id: "boot-1".to_string(),
            now: 110,
        })
        .unwrap();
        reg.set_pinned("claude:s-boot1-pinned", true).unwrap();

        // Session from boot-2 (previous boot)
        reg.upsert_hook_start(&HookSessionUpsert {
            tool: Tool::Claude,
            session_id: "s-boot2".to_string(),
            cwd: "/tmp/b".to_string(),
            transcript: None,
            pid: None,
            pid_start_us: None,
            boot_id: "boot-2".to_string(),
            now: 210,
        })
        .unwrap();

        // Session from boot-2 but missing -> excluded
        reg.upsert_hook_start(&HookSessionUpsert {
            tool: Tool::Claude,
            session_id: "s-boot2-missing".to_string(),
            cwd: "/tmp/c".to_string(),
            transcript: None,
            pid: None,
            pid_start_us: None,
            boot_id: "boot-2".to_string(),
            now: 220,
        })
        .unwrap();
        reg.mark_missing("claude:s-boot2-missing").unwrap();

        let prev = reg.previous_boot_id("boot-3").unwrap();
        assert_eq!(prev.as_deref(), Some("boot-2"));

        let restore_set = reg.list_last_boot(prev.as_deref()).unwrap();
        let ids: Vec<&str> = restore_set.iter().map(|s| s.session_id.as_str()).collect();
        assert_eq!(ids, vec!["s-boot2", "s-boot1-pinned"]);
    }
}
