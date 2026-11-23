use crate::error::{CoreError, Result};
use crate::model::{Session, SessionSource, SessionStatus, Tool};
use crate::paths::AppPaths;
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, Row};
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

const MIGRATION_0001: &str = include_str!("migrations/0001_init.sql");
const MIGRATION_0002: &str = include_str!("migrations/0002_restore_set.sql");
const MIGRATION_0003: &str = include_str!("migrations/0003_clear_markup_titles.sql");

/// Grace margin added to `scan_interval` when checking whether a session was
/// still live at the previous boot's final scan.
pub const LAST_BOOT_GRACE_SECS: i64 = 30;

pub fn restore_window_secs(scan_interval_secs: u64) -> i64 {
    (scan_interval_secs as i64).saturating_add(LAST_BOOT_GRACE_SECS)
}

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
    pub best_guess: bool,
    pub now: i64,
}

impl Registry {
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with_timeout(path, Duration::from_secs(2))
    }

    pub fn open_with_timeout(path: &Path, busy_timeout: Duration) -> Result<Self> {
        AppPaths::ensure_parent(path)?;
        let conn = Connection::open(path)?;
        Self::configure_and_migrate(conn, busy_timeout)
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
        Self::configure_and_migrate(conn, Duration::from_secs(2))
    }

    fn configure_and_migrate(conn: Connection, busy_timeout: Duration) -> Result<Self> {
        conn.busy_timeout(busy_timeout)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "cache_size", -512i64)?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version < 1 {
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch(MIGRATION_0001)?;
            tx.pragma_update(None, "user_version", 1i64)?;
            tx.commit()?;
        }
        if version < 2 {
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch(MIGRATION_0002)?;
            tx.pragma_update(None, "user_version", 2i64)?;
            tx.commit()?;
        }
        if version < 3 {
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch(MIGRATION_0003)?;
            tx.pragma_update(None, "user_version", 3i64)?;
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

    /// Record a completed scan timestamp on `boots.last_scan_at`.
    pub fn record_boot_scan(&mut self, boot_id: &str, now: i64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO boots (boot_id, first_seen, last_scan_at)
             VALUES (?1, ?2, ?2)
             ON CONFLICT(boot_id) DO UPDATE SET
                 last_scan_at = CASE
                     WHEN boots.last_scan_at IS NULL OR excluded.last_scan_at >= boots.last_scan_at
                     THEN excluded.last_scan_at
                     ELSE boots.last_scan_at
                 END",
            params![boot_id, now],
        )?;
        Ok(())
    }

    /// Return the most recent earlier boot that had at least one eligible
    /// session still live at its final scan (and not ended by the user).
    pub fn previous_boot_id(
        &self,
        current_boot_id: &str,
        scan_interval_secs: u64,
    ) -> Result<Option<String>> {
        let window = restore_window_secs(scan_interval_secs);
        let boot: Option<String> = self
            .conn
            .query_row(
                "SELECT b.boot_id
                 FROM boots b
                 WHERE b.boot_id != ?1
                   AND EXISTS (
                       SELECT 1 FROM sessions s
                       WHERE s.last_boot_id = b.boot_id
                         AND s.hidden = 0
                         AND s.ended_by_user = 0
                         AND s.status IN ('live', 'closed')
                         AND (
                             b.last_scan_at IS NULL
                             OR (s.last_seen_live IS NOT NULL AND s.last_seen_live >= b.last_scan_at - ?2)
                         )
                   )
                 ORDER BY b.first_seen DESC
                 LIMIT 1",
                params![current_boot_id, window],
                |r| r.get(0),
            )
            .optional()?;
        Ok(boot)
    }

    /// Upsert a session started via hook (`source = 'hook'`).
    ///
    /// When a new `session_id` starts for the same `(pid, pid_start_us)` (e.g.
    /// after `/clear`), mark the old row `closed`, `ended_by_user = 1`, clear
    /// its PID and `last_boot_id` so it is excluded from the last-boot set.
    /// Preserves user-controlled fields (`title`, `pinned`, `hidden`) and `first_seen`.
    pub fn upsert_hook_start(&mut self, input: &HookSessionUpsert) -> Result<()> {
        let key = Session::make_key(input.tool, &input.session_id);
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT OR IGNORE INTO boots (boot_id, first_seen) VALUES (?1, ?2)",
            params![&input.boot_id, input.now],
        )?;
        if let (Some(pid), Some(pid_start_us)) = (input.pid, input.pid_start_us) {
            tx.execute(
                "UPDATE sessions
                 SET status = 'closed',
                     pid = NULL,
                     pid_start_us = NULL,
                     ended_by_user = 1,
                     last_boot_id = NULL
                 WHERE pid = ?1
                   AND pid_start_us = ?2
                   AND key != ?3",
                params![pid, pid_start_us, key],
            )?;
        }
        tx.execute(
            "INSERT INTO sessions (
                key, tool, session_id, cwd, title, transcript,
                pid, pid_start_us, status, source,
                first_seen, last_seen_live, last_boot_id,
                ended_by_user, best_guess, pinned, hidden
            ) VALUES (
                ?1, ?2, ?3, ?4, NULL, ?5,
                ?6, ?7, 'live', 'hook',
                ?8, ?8, ?9,
                0, 0, 0, 0
            )
            ON CONFLICT(key) DO UPDATE SET
                cwd = excluded.cwd,
                transcript = COALESCE(excluded.transcript, sessions.transcript),
                pid = excluded.pid,
                pid_start_us = excluded.pid_start_us,
                status = 'live',
                source = 'hook',
                ended_by_user = 0,
                best_guess = 0,
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
    ///
    /// When `ended_by_user` is true (`prompt_input_exit`, `logout`, `clear`),
    /// marks `ended_by_user = 1` so it is excluded from the last-boot restore
    /// set (unless pinned). For `reason = "other"` or missing reason,
    /// `ended_by_user` stays `0` so shutdown-killed sessions remain eligible.
    #[allow(clippy::too_many_arguments)]
    pub fn record_hook_end(
        &mut self,
        tool: Tool,
        session_id: &str,
        cwd: &str,
        transcript: Option<&str>,
        boot_id: &str,
        now: i64,
        ended_by_user: bool,
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
                first_seen, last_seen_live, last_boot_id,
                ended_by_user, best_guess, pinned, hidden
            ) VALUES (
                ?1, ?2, ?3, ?4, NULL, ?5,
                NULL, NULL, 'closed', 'hook',
                ?6, ?6, ?7,
                ?8, 0, 0, 0
            )
            ON CONFLICT(key) DO UPDATE SET
                cwd = CASE WHEN excluded.cwd != '' THEN excluded.cwd ELSE sessions.cwd END,
                transcript = COALESCE(excluded.transcript, sessions.transcript),
                pid = NULL,
                pid_start_us = NULL,
                status = 'closed',
                source = 'hook',
                ended_by_user = CASE WHEN ?8 = 1 THEN 1 ELSE sessions.ended_by_user END",
            params![
                key,
                tool.as_str(),
                session_id,
                cwd,
                transcript,
                now,
                boot_id,
                i64::from(ended_by_user),
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
                first_seen, last_seen_live, last_boot_id,
                ended_by_user, best_guess, pinned, hidden
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6,
                ?7, ?8, 'live', 'scan',
                ?9, ?9, ?10,
                0, ?11, 0, 0
            )
            ON CONFLICT(key) DO UPDATE SET
                pid = excluded.pid,
                pid_start_us = excluded.pid_start_us,
                status = 'live',
                ended_by_user = 0,
                best_guess = CASE
                    WHEN sessions.source = 'hook' THEN 0
                    ELSE excluded.best_guess
                END,
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
                i64::from(input.best_guess),
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
                    first_seen, last_seen_live, last_boot_id,
                    ended_by_user, best_guess, pinned, hidden
             FROM sessions
             ORDER BY COALESCE(last_seen_live, first_seen) DESC, key ASC"
        } else {
            "SELECT key, tool, session_id, cwd, title, transcript,
                    pid, pid_start_us, status, source,
                    first_seen, last_seen_live, last_boot_id,
                    ended_by_user, best_guess, pinned, hidden
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

    /// Last-boot restore set:
    /// `last_boot_id = prev AND hidden = 0 AND ended_by_user = 0 AND status IN ('live','closed')
    ///  AND last_seen_live >= prev.last_scan_at - (scan_interval + 30s)`,
    /// unioned with all non-hidden, non-missing pinned sessions.
    pub fn list_last_boot(
        &self,
        previous_boot_id: Option<&str>,
        scan_interval_secs: u64,
    ) -> Result<Vec<Session>> {
        let window = restore_window_secs(scan_interval_secs);
        let mut stmt = self.conn.prepare(
            "SELECT s.key, s.tool, s.session_id, s.cwd, s.title, s.transcript,
                    s.pid, s.pid_start_us, s.status, s.source,
                    s.first_seen, s.last_seen_live, s.last_boot_id,
                    s.ended_by_user, s.best_guess, s.pinned, s.hidden
             FROM sessions s
             LEFT JOIN boots b ON b.boot_id = s.last_boot_id
             WHERE s.hidden = 0
               AND s.status IN ('live', 'closed')
               AND (
                   s.pinned = 1
                   OR (
                       ?1 IS NOT NULL
                       AND s.last_boot_id = ?1
                       AND s.ended_by_user = 0
                       AND (
                           b.last_scan_at IS NULL
                           OR (s.last_seen_live IS NOT NULL AND s.last_seen_live >= b.last_scan_at - ?2)
                       )
                   )
               )
             ORDER BY COALESCE(s.last_seen_live, s.first_seen) DESC, s.key ASC",
        )?;
        let rows = stmt.query_map(params![previous_boot_id, window], row_to_session)?;
        let mut sessions = Vec::new();
        for row in rows {
            sessions.push(row?);
        }
        Ok(sessions)
    }

    /// All non-hidden, non-missing pinned sessions.
    pub fn list_pinned(&self) -> Result<Vec<Session>> {
        let mut stmt = self.conn.prepare(
            "SELECT key, tool, session_id, cwd, title, transcript,
                    pid, pid_start_us, status, source,
                    first_seen, last_seen_live, last_boot_id,
                    ended_by_user, best_guess, pinned, hidden
             FROM sessions
             WHERE hidden = 0
               AND pinned = 1
               AND status IN ('live', 'closed')
             ORDER BY COALESCE(last_seen_live, first_seen) DESC, key ASC",
        )?;
        let rows = stmt.query_map([], row_to_session)?;
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
                        first_seen, last_seen_live, last_boot_id,
                        ended_by_user, best_guess, pinned, hidden
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
        ended_by_user: row.get::<_, i64>(13)? != 0,
        best_guess: row.get::<_, i64>(14)? != 0,
        pinned: row.get::<_, i64>(15)? != 0,
        hidden: row.get::<_, i64>(16)? != 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

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

        reg.upsert_scan_live(&ScanSessionUpsert {
            tool: Tool::Claude,
            session_id: "3f9a1c2e-1111-2222-3333-444444444444".to_string(),
            cwd: "/tmp/proj-a".to_string(),
            transcript: None,
            title_hint: Some("Auto Title".to_string()),
            pid: 1234,
            pid_start_us: 999_000,
            boot_id: "100.000001".to_string(),
            best_guess: true,
            now: 1_700_000_120,
        })
        .unwrap();

        let s = reg.get_by_key(key).unwrap().unwrap();
        assert_eq!(s.source, SessionSource::Hook);
        assert!(!s.best_guess);
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
    fn restore_set_excludes_midday_exited_and_includes_live_shutdown_and_pinned() {
        let mut reg = Registry::open_in_memory().unwrap();
        let scan_interval = 120; // window = 150s
        let last_scan_t = 10_000;

        // Boot 0 (older): pinned session that was even /exited long ago -> ALWAYS included
        reg.record_boot_scan("boot-0", 1_000).unwrap();
        reg.upsert_hook_start(&HookSessionUpsert {
            tool: Tool::Claude,
            session_id: "pinned-forever".into(),
            cwd: "/tmp/pinned".into(),
            transcript: None,
            pid: Some(100),
            pid_start_us: Some(10),
            boot_id: "boot-0".into(),
            now: 900,
        })
        .unwrap();
        reg.record_hook_end(
            Tool::Claude,
            "pinned-forever",
            "/tmp/pinned",
            None,
            "boot-0",
            950,
            true, // prompt_input_exit
        )
        .unwrap();
        reg.set_pinned("claude:pinned-forever", true).unwrap();

        // Boot 1 (previous boot):
        // (1) Session /exited mid-day (ended_by_user=1, last_seen_live=5000) -> EXCLUDED
        reg.upsert_hook_start(&HookSessionUpsert {
            tool: Tool::Claude,
            session_id: "exited-midday".into(),
            cwd: "/tmp/a".into(),
            transcript: None,
            pid: Some(201),
            pid_start_us: Some(20),
            boot_id: "boot-1".into(),
            now: 5_000,
        })
        .unwrap();
        reg.record_hook_end(
            Tool::Claude,
            "exited-midday",
            "/tmp/a",
            None,
            "boot-1",
            5_100,
            true, // prompt_input_exit
        )
        .unwrap();

        // (2) Session /exited right before shutdown (within window, but ended_by_user=1) -> EXCLUDED
        reg.upsert_hook_start(&HookSessionUpsert {
            tool: Tool::Claude,
            session_id: "exited-late".into(),
            cwd: "/tmp/b".into(),
            transcript: None,
            pid: Some(202),
            pid_start_us: Some(21),
            boot_id: "boot-1".into(),
            now: last_scan_t - 20,
        })
        .unwrap();
        reg.record_hook_end(
            Tool::Claude,
            "exited-late",
            "/tmp/b",
            None,
            "boot-1",
            last_scan_t - 10,
            true, // logout / prompt_input_exit
        )
        .unwrap();

        // (3) Session closed mid-day without hook (stale last_seen_live outside 150s window) -> EXCLUDED
        reg.upsert_scan_live(&ScanSessionUpsert {
            tool: Tool::Codex,
            session_id: "stale-midday-codex".into(),
            cwd: "/tmp/c".into(),
            transcript: None,
            title_hint: None,
            pid: 203,
            pid_start_us: 22,
            boot_id: "boot-1".into(),
            best_guess: false,
            now: last_scan_t - 600,
        })
        .unwrap();
        reg.mark_closed("codex:stale-midday-codex").unwrap();

        // (4) Session live at the last scan (no SessionEnd) -> INCLUDED
        reg.upsert_scan_live(&ScanSessionUpsert {
            tool: Tool::Claude,
            session_id: "live-at-last-scan".into(),
            cwd: "/tmp/d".into(),
            transcript: None,
            title_hint: None,
            pid: 204,
            pid_start_us: 23,
            boot_id: "boot-1".into(),
            best_guess: false,
            now: last_scan_t,
        })
        .unwrap();

        // (5) Session killed by OS shutdown (SessionEnd with reason="other", ended_by_user=0) -> INCLUDED
        reg.upsert_hook_start(&HookSessionUpsert {
            tool: Tool::Claude,
            session_id: "killed-by-shutdown".into(),
            cwd: "/tmp/e".into(),
            transcript: None,
            pid: Some(205),
            pid_start_us: Some(24),
            boot_id: "boot-1".into(),
            now: last_scan_t - 30,
        })
        .unwrap();
        reg.record_hook_end(
            Tool::Claude,
            "killed-by-shutdown",
            "/tmp/e",
            None,
            "boot-1",
            last_scan_t + 5,
            false, // reason = "other"
        )
        .unwrap();

        reg.record_boot_scan("boot-1", last_scan_t).unwrap();
        // Empty intermediate boot-2
        reg.record_boot_scan("boot-2-empty", 15_000).unwrap();
        // Current boot-3
        reg.record_boot_scan("boot-3-current", 20_000).unwrap();

        let prev = reg
            .previous_boot_id("boot-3-current", scan_interval)
            .unwrap();
        assert_eq!(prev.as_deref(), Some("boot-1"));

        let restore_set = reg.list_last_boot(prev.as_deref(), scan_interval).unwrap();
        let ids: Vec<&str> = restore_set.iter().map(|s| s.session_id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["live-at-last-scan", "killed-by-shutdown", "pinned-forever"]
        );
    }

    #[test]
    fn migration_0003_clears_only_markup_titles() {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let db_path: PathBuf =
            std::env::temp_dir().join(format!("restowa-mig-v3-{}-{n}.db", std::process::id()));
        let _ = std::fs::remove_file(&db_path);
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch(MIGRATION_0001).unwrap();
            conn.execute_batch(MIGRATION_0002).unwrap();
            conn.pragma_update(None, "user_version", 2i64).unwrap();
            for (id, title) in [
                ("s1", "[Image: source: /tmp/a.png]"),
                ("s2", "<pasted_content id=\"x\"> Read CL…"),
                ("s3", "Fix the login bug"),
            ] {
                conn.execute(
                    "INSERT INTO sessions (key, tool, session_id, cwd, title, status, source,
                        first_seen, pinned, hidden)
                     VALUES (?1, 'claude', ?2, '/tmp', ?3, 'closed', 'scan', 100, 0, 0)",
                    params![format!("claude:{id}"), id, title],
                )
                .unwrap();
            }
        }

        let reg = Registry::open(&db_path).unwrap();
        let title = |k: &str| reg.get_by_key(k).unwrap().unwrap().title;
        assert_eq!(title("claude:s1"), None);
        assert_eq!(title("claude:s2"), None);
        assert_eq!(title("claude:s3").as_deref(), Some("Fix the login bug"));
        drop(reg);

        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(db_path.with_extension("db-wal"));
        let _ = std::fs::remove_file(db_path.with_extension("db-shm"));
    }

    #[test]
    fn migrates_existing_v1_database_to_v2_preserving_rows() {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let db_path: PathBuf =
            std::env::temp_dir().join(format!("restowa-mig-v1-v2-{}-{n}.db", std::process::id()));
        let _ = std::fs::remove_file(&db_path);

        // Create a v1 database manually
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch(MIGRATION_0001).unwrap();
            conn.pragma_update(None, "user_version", 1i64).unwrap();
            conn.execute(
                "INSERT INTO boots (boot_id, first_seen) VALUES ('boot-1', 100)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO sessions (
                    key, tool, session_id, cwd, status, source,
                    first_seen, last_seen_live, last_boot_id, pinned, hidden
                 ) VALUES (
                    'claude:s1', 'claude', 's1', '/tmp', 'live', 'hook',
                    100, 100, 'boot-1', 0, 0
                 )",
                [],
            )
            .unwrap();
        }

        // Opening via Registry runs migration 0002 cleanly
        let reg = Registry::open(&db_path).unwrap();
        let row = reg.get_by_key("claude:s1").unwrap().unwrap();
        assert_eq!(row.session_id, "s1");
        assert!(!row.ended_by_user);
        assert!(!row.best_guess);
        drop(reg);

        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(db_path.with_extension("db-wal"));
        let _ = std::fs::remove_file(db_path.with_extension("db-shm"));
    }
}
