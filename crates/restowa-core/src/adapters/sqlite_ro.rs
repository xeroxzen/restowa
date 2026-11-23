use rusqlite::{Connection, OpenFlags};
use std::path::Path;
use std::time::Duration;

const EXTERNAL_DB_TIMEOUT: Duration = Duration::from_millis(150);

pub enum ExternalDbOpen {
    Opened(Connection),
    NotFound,
    /// The database file exists, but could not be opened read-only under the
    /// current `allow_immutable` constraint (for example, a WAL database whose
    /// owning tool process is running, so `immutable=1` is forbidden).
    Skipped,
}

/// Open another tool's SQLite database strictly read-only, with a short busy
/// timeout, `PRAGMA query_only = ON`, no open transactions, and no writes.
///
/// `immutable=1` is ONLY permitted when `allow_immutable == true` (i.e. no
/// process of the owning tool is currently running). When `allow_immutable` is
/// `false` and a plain read-only open fails, returns `ExternalDbOpen::Skipped`
/// so the caller falls back to the next detection method and sets `best_guess`.
pub fn open_external_readonly(path: &Path, allow_immutable: bool) -> ExternalDbOpen {
    if !path.is_file() {
        return ExternalDbOpen::NotFound;
    }

    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    if let Ok(conn) = Connection::open_with_flags(path, flags) {
        let _ = conn.busy_timeout(EXTERNAL_DB_TIMEOUT);
        let _ = conn.pragma_update(None, "query_only", "ON");
        if conn
            .query_row("SELECT 1 FROM sqlite_master LIMIT 1", [], |_| Ok(()))
            .is_ok()
        {
            return ExternalDbOpen::Opened(conn);
        }
    }

    if !allow_immutable {
        return ExternalDbOpen::Skipped;
    }

    // Only allowed when no process of the owning tool is running.
    let escaped = percent_encode_uri_path(&path.to_string_lossy());
    let uri = format!("file:{escaped}?mode=ro&immutable=1");
    let uri_flags = OpenFlags::SQLITE_OPEN_READ_ONLY
        | OpenFlags::SQLITE_OPEN_URI
        | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    match Connection::open_with_flags(uri, uri_flags) {
        Ok(conn) => {
            let _ = conn.busy_timeout(EXTERNAL_DB_TIMEOUT);
            let _ = conn.pragma_update(None, "query_only", "ON");
            ExternalDbOpen::Opened(conn)
        }
        Err(_) => ExternalDbOpen::Skipped,
    }
}

/// Check that `table` exists and contains every column in `required_columns`.
/// Returns `Ok(())` if valid, or `Err(reason)` describing the schema mismatch.
pub fn verify_table_columns(
    conn: &Connection,
    table: &str,
    required_columns: &[&str],
) -> Result<(), String> {
    let exists: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
            [table],
            |r| r.get::<_, i64>(0),
        )
        .map(|v| v != 0)
        .map_err(|e| format!("failed querying sqlite_master: {e}"))?;

    if !exists {
        return Err(format!("missing table '{table}'"));
    }

    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info(\"{table}\")"))
        .map_err(|e| format!("failed reading schema for '{table}': {e}"))?;
    let cols: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(1))
        .map_err(|e| format!("failed iterating columns for '{table}': {e}"))?
        .filter_map(|r| r.ok())
        .collect();

    let missing: Vec<&str> = required_columns
        .iter()
        .copied()
        .filter(|req| !cols.iter().any(|c| c.eq_ignore_ascii_case(req)))
        .collect();

    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "table '{table}' missing expected column(s): {}",
            missing.join(", ")
        ))
    }
}

fn percent_encode_uri_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for b in path.bytes() {
        match b {
            b'?' => out.push_str("%3F"),
            b'#' => out.push_str("%23"),
            b'%' => out.push_str("%25"),
            b' ' => out.push_str("%20"),
            _ => out.push(b as char),
        }
    }
    out
}

/// Decode percent-encoded `file://` URI paths (e.g. `file:///Users/me/My%20Project`
/// -> `/Users/me/My Project`).
pub fn file_uri_to_path(uri: &str) -> Option<String> {
    let raw = uri.strip_prefix("file://")?;
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3]) {
                if let Ok(val) = u8::from_str_radix(hex, 16) {
                    out.push(val);
                    i += 3;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct ReadOnlyWalFixture {
        dir: PathBuf,
        db_path: PathBuf,
    }

    impl ReadOnlyWalFixture {
        fn new() -> Self {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir =
                std::env::temp_dir().join(format!("restowa-wal-ro-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let db_path = dir.join("tool.db");
            {
                let conn = Connection::open(&db_path).unwrap();
                conn.pragma_update(None, "journal_mode", "WAL").unwrap();
                conn.execute("CREATE TABLE items (id TEXT PRIMARY KEY)", [])
                    .unwrap();
                conn.execute("INSERT INTO items VALUES ('ok-1')", [])
                    .unwrap();
            }
            // Remove -shm/-wal if any and make directory read-only so plain
            // SQLITE_OPEN_READ_ONLY cannot create a -shm file.
            let _ = std::fs::remove_file(dir.join("tool.db-shm"));
            let _ = std::fs::remove_file(dir.join("tool.db-wal"));
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
            Self { dir, db_path }
        }
    }

    impl Drop for ReadOnlyWalFixture {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o755));
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn file_uri_to_path_decodes_spaces() {
        assert_eq!(
            file_uri_to_path("file:///Users/dev/Developer/Side%20Projects/demo-app").as_deref(),
            Some("/Users/dev/Developer/Side Projects/demo-app")
        );
    }

    #[test]
    fn immutable_allowed_when_no_tool_process_running_and_skipped_when_running() {
        let fix = ReadOnlyWalFixture::new();

        // When owning tool IS running (allow_immutable = false): must NOT use immutable=1
        assert!(matches!(
            open_external_readonly(&fix.db_path, false),
            ExternalDbOpen::Skipped
        ));

        // When NO owning tool process is running (allow_immutable = true): immutable=1 succeeds
        match open_external_readonly(&fix.db_path, true) {
            ExternalDbOpen::Opened(conn) => {
                let id: String = conn
                    .query_row("SELECT id FROM items LIMIT 1", [], |r| r.get(0))
                    .unwrap();
                assert_eq!(id, "ok-1");
            }
            _ => panic!("expected Opened with allow_immutable = true"),
        }
    }
}
