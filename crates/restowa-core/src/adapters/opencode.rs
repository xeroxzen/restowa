use crate::adapters::claude::normalize_title;
use crate::adapters::sqlite_ro::{open_external_readonly, verify_table_columns};
use crate::adapters::Adapter;
use crate::config::ToolConfig;
use crate::doctor::{CheckStatus, DoctorCheck};
use crate::error::Result;
use crate::model::{ProcInfo, Session, SessionRef, Tool};
use crate::paths::AppPaths;
use crate::platform::Platform;
use crate::shell::expand_resume_template;
use std::path::Path;

const NON_TUI_SUBCOMMANDS: &[&str] = &[
    "acp",
    "serve",
    "web",
    "run",
    "mcp",
    "db",
    "session",
    "export",
    "import",
    "stats",
    "models",
    "upgrade",
    "uninstall",
    "completion",
    "debug",
    "providers",
    "auth",
    "agent",
    "github",
    "pr",
    "plugin",
    "plug",
    "attach",
];

#[derive(Debug, Clone)]
pub struct OpenCodeAdapter {
    paths: AppPaths,
}

#[derive(Debug, Clone)]
struct OpenCodeSessionRow {
    id: String,
    directory: String,
    title: String,
}

impl OpenCodeAdapter {
    pub fn new(paths: AppPaths) -> Self {
        Self { paths }
    }

    fn extract_session_arg(argv: &[String]) -> Option<String> {
        let mut iter = argv.iter().skip(1);
        while let Some(arg) = iter.next() {
            if arg == "--session" || arg == "-s" {
                if let Some(val) = iter.next() {
                    if !val.is_empty() && !val.starts_with('-') {
                        return Some(val.clone());
                    }
                }
            } else if let Some(val) = arg.strip_prefix("--session=") {
                if !val.is_empty() {
                    return Some(val.to_string());
                }
            }
        }
        None
    }

    fn query_sessions_for_dir(&self, dir: &str) -> Option<Vec<OpenCodeSessionRow>> {
        let conn = open_external_readonly(&self.paths.opencode_db())?;
        if verify_table_columns(
            &conn,
            "session",
            &["id", "directory", "title", "time_updated"],
        )
        .is_err()
        {
            return None;
        }
        let mut stmt = conn
            .prepare(
                "SELECT id, directory, title
                 FROM session
                 WHERE directory = ?1
                   AND parent_id IS NULL
                   AND time_archived IS NULL
                 ORDER BY time_updated DESC, id DESC
                 LIMIT 2",
            )
            .ok()?;
        let rows = stmt
            .query_map([dir], |r| {
                Ok(OpenCodeSessionRow {
                    id: r.get(0)?,
                    directory: r.get(1)?,
                    title: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                })
            })
            .ok()?;
        let mut out = Vec::new();
        for r in rows.flatten() {
            out.push(r);
        }
        Some(out)
    }

    fn query_session_by_id(&self, session_id: &str) -> Option<OpenCodeSessionRow> {
        let conn = open_external_readonly(&self.paths.opencode_db())?;
        if verify_table_columns(&conn, "session", &["id", "directory", "title"]).is_err() {
            return None;
        }
        conn.query_row(
            "SELECT id, directory, title
             FROM session
             WHERE id = ?1 AND time_archived IS NULL
             LIMIT 1",
            [session_id],
            |r| {
                Ok(OpenCodeSessionRow {
                    id: r.get(0)?,
                    directory: r.get(1)?,
                    title: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                })
            },
        )
        .ok()
    }

    fn resolve_title(&self, row: &OpenCodeSessionRow) -> Option<String> {
        let trimmed = row.title.trim();
        if !trimmed.is_empty() && !trimmed.starts_with("New session - ") {
            return Some(normalize_title(trimmed));
        }
        self.first_user_part_text(&row.id)
    }

    fn first_user_part_text(&self, session_id: &str) -> Option<String> {
        let conn = open_external_readonly(&self.paths.opencode_db())?;
        if verify_table_columns(&conn, "message", &["id", "session_id", "data"]).is_err()
            || verify_table_columns(&conn, "part", &["message_id", "session_id", "data"]).is_err()
        {
            return None;
        }
        // Find the earliest user message for this session
        let mut msg_stmt = conn
            .prepare(
                "SELECT id, data FROM message
                 WHERE session_id = ?1
                 ORDER BY time_created ASC, id ASC
                 LIMIT 10",
            )
            .ok()?;
        let msg_rows = msg_stmt
            .query_map([session_id], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })
            .ok()?;

        let mut first_user_msg_id = None;
        for (msg_id, data) in msg_rows.flatten() {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&data) {
                if val.get("role").and_then(|v| v.as_str()) == Some("user") {
                    first_user_msg_id = Some(msg_id);
                    break;
                }
            }
        }
        let msg_id = first_user_msg_id?;

        let mut part_stmt = conn
            .prepare(
                "SELECT data FROM part
                 WHERE message_id = ?1
                 ORDER BY time_created ASC, id ASC
                 LIMIT 10",
            )
            .ok()?;
        let part_rows = part_stmt
            .query_map([msg_id], |r| r.get::<_, String>(0))
            .ok()?;
        for data in part_rows.flatten() {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&data) {
                if val.get("type").and_then(|v| v.as_str()) == Some("text") {
                    if let Some(text) = val.get("text").and_then(|v| v.as_str()) {
                        let clean = normalize_title(text);
                        if !clean.is_empty() {
                            return Some(clean);
                        }
                    }
                }
            }
        }
        None
    }
}

impl Adapter for OpenCodeAdapter {
    fn tool(&self) -> Tool {
        Tool::Opencode
    }

    fn matches_process(&self, p: &ProcInfo) -> bool {
        if p.argv
            .iter()
            .skip(1)
            .any(|a| NON_TUI_SUBCOMMANDS.contains(&a.as_str()))
        {
            return false;
        }

        let exe_is_oc = p.exe_path.as_ref().is_some_and(|exe| {
            exe.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n == "opencode")
        });
        let argv0_is_oc = p.argv.first().is_some_and(|a| {
            Path::new(a)
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n == "opencode")
        });

        exe_is_oc || argv0_is_oc
    }

    fn session_for_process(
        &self,
        p: &ProcInfo,
        _platform: &dyn Platform,
    ) -> Result<Option<SessionRef>> {
        let proc_cwd = p.cwd.as_ref().map(|c| c.to_string_lossy().into_owned());
        let db_path_str = Some(self.paths.opencode_db().to_string_lossy().into_owned());

        // 1. Exact: `--session <id>` or `-s <id>` in argv.
        if let Some(session_id) = Self::extract_session_arg(&p.argv) {
            if let Some(row) = self.query_session_by_id(&session_id) {
                let cwd = if !row.directory.is_empty() {
                    row.directory.clone()
                } else {
                    proc_cwd.clone().unwrap_or_default()
                };
                if !cwd.is_empty() {
                    let title_hint = self.resolve_title(&row);
                    return Ok(Some(SessionRef {
                        tool: Tool::Opencode,
                        session_id,
                        cwd,
                        transcript: db_path_str,
                        title_hint,
                        best_guess: false,
                    }));
                }
            } else if let Some(cwd) = proc_cwd {
                return Ok(Some(SessionRef {
                    tool: Tool::Opencode,
                    session_id,
                    cwd,
                    transcript: db_path_str,
                    title_hint: None,
                    best_guess: false,
                }));
            }
            return Ok(None);
        }

        // 2. Fallback: newest non-archived top-level session in opencode.db matching `cwd`.
        let Some(cwd) = proc_cwd else {
            return Ok(None);
        };
        if let Some(rows) = self.query_sessions_for_dir(&cwd) {
            if let Some(first) = rows.first() {
                let ambiguous = rows.len() > 1;
                let title_hint = self.resolve_title(first);
                return Ok(Some(SessionRef {
                    tool: Tool::Opencode,
                    session_id: first.id.clone(),
                    cwd: first.directory.clone(),
                    transcript: db_path_str,
                    title_hint,
                    best_guess: ambiguous,
                }));
            }
        }

        Ok(None)
    }

    fn session_exists(&self, s: &Session) -> bool {
        self.query_session_by_id(&s.session_id).is_some()
    }

    fn title(&self, s: &Session) -> Option<String> {
        let row = self.query_session_by_id(&s.session_id)?;
        self.resolve_title(&row)
    }

    fn resume_command(&self, s: &Session, cfg: &ToolConfig) -> String {
        expand_resume_template(&cfg.resume, &s.session_id)
    }

    fn doctor_checks(&self) -> Vec<DoctorCheck> {
        let db_path = self.paths.opencode_db();
        if !db_path.is_file() {
            return Vec::new();
        }
        let check = match open_external_readonly(&db_path) {
            Some(conn) => match verify_table_columns(
                &conn,
                "session",
                &["id", "directory", "title", "time_updated"],
            ) {
                Ok(()) => DoctorCheck {
                    name: "schema:opencode".to_string(),
                    status: CheckStatus::Ok,
                    detail: format!("{} schema verified (read-only)", db_path.display()),
                },
                Err(reason) => DoctorCheck {
                    name: "schema:opencode".to_string(),
                    status: CheckStatus::Warn,
                    detail: format!(
                        "{}: {reason}; falling back to --session argv only",
                        db_path.display()
                    ),
                },
            },
            None => DoctorCheck {
                name: "schema:opencode".to_string(),
                status: CheckStatus::Warn,
                detail: format!("could not open {} read-only", db_path.display()),
            },
        };
        vec![check]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TestHome(PathBuf);
    impl TestHome {
        fn new() -> Self {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "restowa-opencode-test-{}-{}",
                std::process::id(),
                n
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }
    impl Drop for TestHome {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    struct EmptyPlatform;
    impl Platform for EmptyPlatform {
        fn boot_id(&self) -> Result<String> {
            Ok("1.0".into())
        }
        fn list_processes(&self) -> Result<Vec<ProcInfo>> {
            Ok(Vec::new())
        }
        fn process_info(&self, _pid: i32) -> Option<ProcInfo> {
            None
        }
        fn process_open_files(&self, _pid: i32) -> Vec<PathBuf> {
            Vec::new()
        }
        fn parent_pid(&self) -> i32 {
            1
        }
    }

    fn seed_opencode_db(db_path: &Path) {
        AppPaths::ensure_parent(db_path).unwrap();
        let conn = Connection::open(db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE session (
                id TEXT PRIMARY KEY,
                project_id TEXT NOT NULL,
                parent_id TEXT,
                directory TEXT NOT NULL,
                title TEXT NOT NULL,
                time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL,
                time_archived INTEGER
            );
            CREATE TABLE message (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                time_created INTEGER NOT NULL,
                data TEXT NOT NULL
            );
            CREATE TABLE part (
                id TEXT PRIMARY KEY,
                message_id TEXT NOT NULL,
                session_id TEXT NOT NULL,
                time_created INTEGER NOT NULL,
                data TEXT NOT NULL
            );
            INSERT INTO session VALUES ('ses_unique', 'global', NULL, '/Users/dev/single', 'Scaffold adapters', 100, 200, NULL);
            INSERT INTO session VALUES ('ses_multi_1', 'global', NULL, '/Users/dev/multi', 'New session - 2026-11-06T16:48:53Z', 100, 300, NULL);
            INSERT INTO session VALUES ('ses_multi_2', 'global', NULL, '/Users/dev/multi', 'Older session', 50, 150, NULL);
            INSERT INTO message VALUES ('msg_1', 'ses_multi_1', 101, '{\"role\":\"user\"}');
            INSERT INTO part VALUES ('prt_1', 'msg_1', 'ses_multi_1', 102, '{\"type\":\"text\",\"text\":\"Inspect auth middleware\"}');
            ",
        )
        .unwrap();
    }

    #[test]
    fn matches_process_accepts_tui_and_rejects_serve_acp_and_run() {
        let home = TestHome::new();
        let adapter = OpenCodeAdapter::new(AppPaths::from_home(home.0.clone()));

        let tui = ProcInfo {
            pid: 10,
            ppid: 1,
            start_time_us: 1,
            exe_path: Some(PathBuf::from("/Users/u/.opencode/bin/opencode")),
            argv: vec!["opencode".into(), "--session".into(), "ses_123".into()],
            cwd: Some(PathBuf::from("/tmp")),
        };
        assert!(adapter.matches_process(&tui));

        for sub in ["acp", "serve", "web", "run", "db"] {
            let helper = ProcInfo {
                pid: 11,
                ppid: 1,
                start_time_us: 1,
                exe_path: Some(PathBuf::from("/Users/u/.opencode/bin/opencode")),
                argv: vec!["opencode".into(), sub.into()],
                cwd: Some(PathBuf::from("/tmp")),
            };
            assert!(!adapter.matches_process(&helper), "should reject {sub}");
        }
    }

    #[test]
    fn resolves_exact_from_argv_and_flags_ambiguous_fallback_as_best_guess() {
        let home = TestHome::new();
        let paths = AppPaths::from_home(home.0.clone());
        seed_opencode_db(&paths.opencode_db());
        let adapter = OpenCodeAdapter::new(paths);

        // Single session in directory -> exact
        let single_proc = ProcInfo {
            pid: 10,
            ppid: 1,
            start_time_us: 1,
            exe_path: Some(PathBuf::from("/Users/u/.opencode/bin/opencode")),
            argv: vec!["opencode".into()],
            cwd: Some(PathBuf::from("/Users/dev/single")),
        };
        let s1 = adapter
            .session_for_process(&single_proc, &EmptyPlatform)
            .unwrap()
            .unwrap();
        assert_eq!(s1.session_id, "ses_unique");
        assert_eq!(s1.title_hint.as_deref(), Some("Scaffold adapters"));
        assert!(!s1.best_guess);

        // Two sessions in same directory without --session -> newest + best_guess = true + prompt title fallback
        let multi_proc = ProcInfo {
            pid: 11,
            ppid: 1,
            start_time_us: 2,
            exe_path: Some(PathBuf::from("/Users/u/.opencode/bin/opencode")),
            argv: vec!["opencode".into()],
            cwd: Some(PathBuf::from("/Users/dev/multi")),
        };
        let s2 = adapter
            .session_for_process(&multi_proc, &EmptyPlatform)
            .unwrap()
            .unwrap();
        assert_eq!(s2.session_id, "ses_multi_1");
        assert_eq!(s2.title_hint.as_deref(), Some("Inspect auth middleware"));
        assert!(s2.best_guess);

        // Explicit --session in argv -> exact even in multi-session folder
        let explicit_proc = ProcInfo {
            pid: 12,
            ppid: 1,
            start_time_us: 3,
            exe_path: Some(PathBuf::from("/Users/u/.opencode/bin/opencode")),
            argv: vec!["opencode".into(), "--session".into(), "ses_multi_2".into()],
            cwd: Some(PathBuf::from("/Users/dev/multi")),
        };
        let s3 = adapter
            .session_for_process(&explicit_proc, &EmptyPlatform)
            .unwrap()
            .unwrap();
        assert_eq!(s3.session_id, "ses_multi_2");
        assert!(!s3.best_guess);
    }

    #[test]
    fn missing_opencode_table_never_crashes_and_doctor_warns() {
        let home = TestHome::new();
        let paths = AppPaths::from_home(home.0.clone());
        AppPaths::ensure_parent(&paths.opencode_db()).unwrap();
        {
            let conn = Connection::open(paths.opencode_db()).unwrap();
            conn.execute("CREATE TABLE wrong_schema (id TEXT)", [])
                .unwrap();
        }
        let adapter = OpenCodeAdapter::new(paths);

        // Falls back cleanly to argv when DB schema is broken
        let proc_with_arg = ProcInfo {
            pid: 20,
            ppid: 1,
            start_time_us: 1,
            exe_path: Some(PathBuf::from("/Users/u/.opencode/bin/opencode")),
            argv: vec!["opencode".into(), "-s".into(), "ses_from_argv".into()],
            cwd: Some(PathBuf::from("/Users/dev/app")),
        };
        let s = adapter
            .session_for_process(&proc_with_arg, &EmptyPlatform)
            .unwrap()
            .unwrap();
        assert_eq!(s.session_id, "ses_from_argv");

        let checks = adapter.doctor_checks();
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, CheckStatus::Warn);
        assert!(checks[0].detail.contains("missing table 'session'"));
    }
}
