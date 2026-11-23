use crate::adapters::claude::normalize_title;
use crate::adapters::sqlite_ro::{open_external_readonly, verify_table_columns, ExternalDbOpen};
use crate::adapters::Adapter;
use crate::config::ToolConfig;
use crate::doctor::{CheckStatus, DoctorCheck};
use crate::error::Result;
use crate::model::{ProcInfo, Session, SessionRef, Tool};
use crate::paths::AppPaths;
use crate::platform::Platform;
use crate::shell::expand_resume_template;
use std::collections::HashSet;
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

    fn query_sessions_for_dir(
        &self,
        dir: &str,
        allow_immutable: bool,
    ) -> Option<Vec<OpenCodeSessionRow>> {
        let ExternalDbOpen::Opened(conn) =
            open_external_readonly(&self.paths.opencode_db(), allow_immutable)
        else {
            return None;
        };
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
                 LIMIT 16",
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

    fn query_session_by_id(
        &self,
        session_id: &str,
        allow_immutable: bool,
    ) -> Option<OpenCodeSessionRow> {
        let ExternalDbOpen::Opened(conn) =
            open_external_readonly(&self.paths.opencode_db(), allow_immutable)
        else {
            return None;
        };
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

    fn resolve_title(&self, row: &OpenCodeSessionRow, allow_immutable: bool) -> Option<String> {
        let trimmed = row.title.trim();
        if !trimmed.is_empty() && !trimmed.starts_with("New session - ") {
            return Some(normalize_title(trimmed));
        }
        self.first_user_part_text(&row.id, allow_immutable)
    }

    fn first_user_part_text(&self, session_id: &str, allow_immutable: bool) -> Option<String> {
        let ExternalDbOpen::Opened(conn) =
            open_external_readonly(&self.paths.opencode_db(), allow_immutable)
        else {
            return None;
        };
        if verify_table_columns(&conn, "message", &["id", "session_id", "data"]).is_err()
            || verify_table_columns(&conn, "part", &["message_id", "session_id", "data"]).is_err()
        {
            return None;
        }
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
        claimed_ids: &HashSet<String>,
    ) -> Result<Option<SessionRef>> {
        let proc_cwd = p.cwd.as_ref().map(|c| c.to_string_lossy().into_owned());
        let db_path_str = Some(self.paths.opencode_db().to_string_lossy().into_owned());
        // An OpenCode process is running right now -> never allow `immutable=1`.
        let allow_immutable = false;

        // 1. Exact: `--session <id>` or `-s <id>` in argv.
        if let Some(session_id) = Self::extract_session_arg(&p.argv) {
            if let Some(row) = self.query_session_by_id(&session_id, allow_immutable) {
                let cwd = if !row.directory.is_empty() {
                    row.directory.clone()
                } else {
                    proc_cwd.clone().unwrap_or_default()
                };
                if !cwd.is_empty() {
                    let title_hint = self.resolve_title(&row, allow_immutable);
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

        // 2. Fallback: newest unclaimed non-archived top-level session in opencode.db.
        let Some(cwd) = proc_cwd else {
            return Ok(None);
        };
        if let Some(rows) = self.query_sessions_for_dir(&cwd, allow_immutable) {
            if let Some(chosen) = rows
                .iter()
                .find(|r| !claimed_ids.contains(&r.id))
                .or_else(|| rows.first())
            {
                let ambiguous = rows.len() > 1;
                let title_hint = self.resolve_title(chosen, allow_immutable);
                return Ok(Some(SessionRef {
                    tool: Tool::Opencode,
                    session_id: chosen.id.clone(),
                    cwd: chosen.directory.clone(),
                    transcript: db_path_str,
                    title_hint,
                    best_guess: ambiguous,
                }));
            }
        }

        Ok(None)
    }

    fn session_exists(&self, s: &Session, tool_running: bool) -> bool {
        self.query_session_by_id(&s.session_id, !tool_running)
            .is_some()
    }

    fn title(&self, s: &Session, tool_running: bool) -> Option<String> {
        let allow_immutable = !tool_running;
        let row = self.query_session_by_id(&s.session_id, allow_immutable)?;
        self.resolve_title(&row, allow_immutable)
    }

    fn resume_command(&self, s: &Session, cfg: &ToolConfig) -> String {
        expand_resume_template(&cfg.resume, &s.session_id)
    }

    fn doctor_checks(&self, tool_running: bool) -> Vec<DoctorCheck> {
        let db_path = self.paths.opencode_db();
        if !db_path.is_file() {
            return Vec::new();
        }
        let check = match open_external_readonly(&db_path, !tool_running) {
            ExternalDbOpen::Opened(conn) => match verify_table_columns(
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
            ExternalDbOpen::Skipped | ExternalDbOpen::NotFound => DoctorCheck {
                name: "schema:opencode".to_string(),
                status: CheckStatus::Warn,
                detail: format!(
                    "skipped {} read-only check (immutable=1 disallowed while opencode is running)",
                    db_path.display()
                ),
            },
        };
        vec![check]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scanner::run_scan;
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

    struct MockPlatform {
        procs: Vec<ProcInfo>,
    }
    impl Platform for MockPlatform {
        fn boot_id(&self) -> Result<String> {
            Ok("1.0".into())
        }
        fn list_processes(&self) -> Result<Vec<ProcInfo>> {
            Ok(self.procs.clone())
        }
        fn process_info(&self, pid: i32) -> Option<ProcInfo> {
            self.procs.iter().find(|p| p.pid == pid).cloned()
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
    fn two_opencode_sessions_in_same_folder_both_resolve_and_flag_best_guess() {
        let home = TestHome::new();
        let paths = AppPaths::from_home(home.0.clone());
        seed_opencode_db(&paths.opencode_db());

        let platform = MockPlatform {
            procs: vec![
                ProcInfo {
                    pid: 201,
                    ppid: 1,
                    start_time_us: 200,
                    exe_path: Some(PathBuf::from("/Users/u/.opencode/bin/opencode")),
                    argv: vec!["opencode".into()],
                    cwd: Some(PathBuf::from("/Users/dev/multi")),
                },
                ProcInfo {
                    pid: 202,
                    ppid: 1,
                    start_time_us: 100,
                    exe_path: Some(PathBuf::from("/Users/u/.opencode/bin/opencode")),
                    argv: vec!["opencode".into()],
                    cwd: Some(PathBuf::from("/Users/dev/multi")),
                },
            ],
        };

        let scan = run_scan(&paths, &platform).unwrap();
        assert_eq!(scan.live_found, 2);

        let reg = crate::registry::Registry::open(&paths.registry_db).unwrap();
        let s1 = reg.get_by_key("opencode:ses_multi_1").unwrap().unwrap();
        let s2 = reg.get_by_key("opencode:ses_multi_2").unwrap().unwrap();
        assert!(s1.best_guess);
        assert!(s2.best_guess);
        assert_ne!(s1.pid, s2.pid);
    }

    #[test]
    fn resolves_exact_from_argv_and_flags_ambiguous_fallback_as_best_guess() {
        let home = TestHome::new();
        let paths = AppPaths::from_home(home.0.clone());
        seed_opencode_db(&paths.opencode_db());
        let adapter = OpenCodeAdapter::new(paths);
        let empty_platform = MockPlatform { procs: Vec::new() };
        let empty_claimed = HashSet::new();

        let single_proc = ProcInfo {
            pid: 10,
            ppid: 1,
            start_time_us: 1,
            exe_path: Some(PathBuf::from("/Users/u/.opencode/bin/opencode")),
            argv: vec!["opencode".into()],
            cwd: Some(PathBuf::from("/Users/dev/single")),
        };
        let s1 = adapter
            .session_for_process(&single_proc, &empty_platform, &empty_claimed)
            .unwrap()
            .unwrap();
        assert_eq!(s1.session_id, "ses_unique");
        assert_eq!(s1.title_hint.as_deref(), Some("Scaffold adapters"));
        assert!(!s1.best_guess);

        let explicit_proc = ProcInfo {
            pid: 12,
            ppid: 1,
            start_time_us: 3,
            exe_path: Some(PathBuf::from("/Users/u/.opencode/bin/opencode")),
            argv: vec!["opencode".into(), "--session".into(), "ses_multi_2".into()],
            cwd: Some(PathBuf::from("/Users/dev/multi")),
        };
        let s3 = adapter
            .session_for_process(&explicit_proc, &empty_platform, &empty_claimed)
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
        let empty_platform = MockPlatform { procs: Vec::new() };

        let proc_with_arg = ProcInfo {
            pid: 20,
            ppid: 1,
            start_time_us: 1,
            exe_path: Some(PathBuf::from("/Users/u/.opencode/bin/opencode")),
            argv: vec!["opencode".into(), "-s".into(), "ses_from_argv".into()],
            cwd: Some(PathBuf::from("/Users/dev/app")),
        };
        let s = adapter
            .session_for_process(&proc_with_arg, &empty_platform, &HashSet::new())
            .unwrap()
            .unwrap();
        assert_eq!(s.session_id, "ses_from_argv");

        let checks = adapter.doctor_checks(false);
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, CheckStatus::Warn);
        assert!(checks[0].detail.contains("missing table 'session'"));
    }
}
