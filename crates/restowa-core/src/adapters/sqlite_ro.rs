use rusqlite::{Connection, OpenFlags};
use std::path::Path;
use std::time::Duration;

const EXTERNAL_DB_TIMEOUT: Duration = Duration::from_millis(150);

/// Open another tool's SQLite database strictly read-only, with a short busy
/// timeout, `PRAGMA query_only = ON`, no open transactions, and no writes.
///
/// Falls back to an `immutable=1` URI if opening a WAL-mode DB without `-shm`
/// would otherwise require write access to the tool's directory.
pub fn open_external_readonly(path: &Path) -> Option<Connection> {
    if !path.is_file() {
        return None;
    }

    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    if let Ok(conn) = Connection::open_with_flags(path, flags) {
        let _ = conn.busy_timeout(EXTERNAL_DB_TIMEOUT);
        let _ = conn.pragma_update(None, "query_only", "ON");
        // Verify we can actually read sqlite_master without triggering a WAL write error.
        if conn
            .query_row("SELECT 1 FROM sqlite_master LIMIT 1", [], |_| Ok(()))
            .is_ok()
        {
            return Some(conn);
        }
    }

    // Fallback: URI mode=ro&immutable=1 (never touches WAL/SHM or directory).
    let escaped = percent_encode_uri_path(&path.to_string_lossy());
    let uri = format!("file:{escaped}?mode=ro&immutable=1");
    let uri_flags = OpenFlags::SQLITE_OPEN_READ_ONLY
        | OpenFlags::SQLITE_OPEN_URI
        | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = Connection::open_with_flags(uri, uri_flags).ok()?;
    let _ = conn.busy_timeout(EXTERNAL_DB_TIMEOUT);
    let _ = conn.pragma_update(None, "query_only", "ON");
    Some(conn)
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

    #[test]
    fn file_uri_to_path_decodes_spaces() {
        assert_eq!(
            file_uri_to_path(
                "file:///Users/dev/Developer/Side%20Projects/demo-app"
            )
            .as_deref(),
            Some("/Users/dev/Developer/Side Projects/demo-app")
        );
    }
}
