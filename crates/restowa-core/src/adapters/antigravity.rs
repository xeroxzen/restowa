use crate::adapters::claude::normalize_title;
use crate::adapters::sqlite_ro::{
    file_uri_to_path, open_external_readonly, verify_table_columns, ExternalDbOpen,
};
use crate::adapters::Adapter;
use crate::config::ToolConfig;
use crate::doctor::{CheckStatus, DoctorCheck};
use crate::error::Result;
use crate::model::{ProcInfo, Session, SessionRef, Tool};
use crate::paths::AppPaths;
use crate::platform::Platform;
use crate::shell::expand_resume_template;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

const NON_INTERACTIVE_ARGS: &[&str] = &[
    "--hub",
    "acp",
    "-p",
    "--print",
    "--prompt",
    "remote-control",
    "mcp",
    "plugin",
    "plugins",
    "update",
    "install",
    "models",
    "agent",
    "agents",
    "changelog",
    "help",
    "mic-serve",
];

#[derive(Debug, Clone)]
pub struct AntigravityAdapter {
    paths: AppPaths,
}

#[derive(Debug, Clone)]
struct SummaryRow {
    conversation_id: String,
    title: String,
    preview: String,
    workspace_uris: String,
}

enum SummariesQueryResult {
    Rows(Vec<SummaryRow>),
    Skipped,
    NotFound,
}

impl AntigravityAdapter {
    pub fn new(paths: AppPaths) -> Self {
        Self { paths }
    }

    pub fn conversation_db_path(&self, id: &str) -> PathBuf {
        self.paths
            .antigravity_conversations_dir()
            .join(format!("{id}.db"))
    }

    fn extract_conversation_arg(argv: &[String]) -> Option<String> {
        let mut iter = argv.iter().skip(1);
        while let Some(arg) = iter.next() {
            if arg == "--conversation" {
                if let Some(val) = iter.next() {
                    if !val.is_empty() && !val.starts_with('-') {
                        return Some(val.clone());
                    }
                }
            } else if let Some(val) = arg.strip_prefix("--conversation=") {
                if !val.is_empty() {
                    return Some(val.to_string());
                }
            }
        }
        None
    }

    /// Extract exact conversation ID from open FDs held by a live `agy` process:
    /// - `~/.gemini/antigravity-cli/presence/<id>.lock`
    /// - `~/.gemini/antigravity-cli/conversations/<id>.db`
    fn extract_id_from_open_files(open_files: &[PathBuf]) -> Option<String> {
        for path in open_files {
            let s = path.to_string_lossy();
            if s.contains("antigravity-cli/presence/") {
                if let Some(id) = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .and_then(|n| n.strip_suffix(".lock"))
                {
                    if !id.is_empty() {
                        return Some(id.to_string());
                    }
                }
            }
        }
        for path in open_files {
            let s = path.to_string_lossy();
            if s.contains("antigravity-cli/conversations/") {
                if let Some(id) = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .and_then(|n| n.strip_suffix(".db"))
                {
                    if !id.is_empty() {
                        return Some(id.to_string());
                    }
                }
            }
        }
        None
    }

    fn read_last_conversations_map(&self) -> Option<HashMap<String, String>> {
        let raw = std::fs::read_to_string(self.paths.antigravity_last_conversations()).ok()?;
        serde_json::from_str(&raw).ok()
    }

    fn query_summary_by_id(&self, id: &str, allow_immutable: bool) -> Option<SummaryRow> {
        let ExternalDbOpen::Opened(conn) =
            open_external_readonly(&self.paths.antigravity_summaries_db(), allow_immutable)
        else {
            return None;
        };
        if verify_table_columns(
            &conn,
            "conversation_summaries",
            &["conversation_id", "title", "preview", "workspace_uris"],
        )
        .is_err()
        {
            return None;
        }
        conn.query_row(
            "SELECT conversation_id, title, preview, workspace_uris
             FROM conversation_summaries
             WHERE conversation_id = ?1
             LIMIT 1",
            [id],
            |r| {
                Ok(SummaryRow {
                    conversation_id: r.get(0)?,
                    title: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                    preview: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                    workspace_uris: r.get::<_, Option<String>>(3)?.unwrap_or_default(),
                })
            },
        )
        .ok()
    }

    fn query_summaries_for_cwd(&self, cwd: &str, allow_immutable: bool) -> SummariesQueryResult {
        let conn =
            match open_external_readonly(&self.paths.antigravity_summaries_db(), allow_immutable) {
                ExternalDbOpen::Opened(c) => c,
                ExternalDbOpen::NotFound => return SummariesQueryResult::NotFound,
                ExternalDbOpen::Skipped => return SummariesQueryResult::Skipped,
            };
        if verify_table_columns(
            &conn,
            "conversation_summaries",
            &[
                "conversation_id",
                "title",
                "preview",
                "workspace_uris",
                "last_modified_time",
            ],
        )
        .is_err()
        {
            return SummariesQueryResult::Skipped;
        }
        let Ok(mut stmt) = conn.prepare(
            "SELECT conversation_id, title, preview, workspace_uris
             FROM conversation_summaries
             WHERE parent_conversation_id = ''
             ORDER BY last_modified_time DESC
             LIMIT 200",
        ) else {
            return SummariesQueryResult::Skipped;
        };
        let Ok(rows) = stmt.query_map([], |r| {
            Ok(SummaryRow {
                conversation_id: r.get(0)?,
                title: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                preview: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                workspace_uris: r.get::<_, Option<String>>(3)?.unwrap_or_default(),
            })
        }) else {
            return SummariesQueryResult::Skipped;
        };

        let mut matched = Vec::new();
        for row in rows.flatten() {
            if workspace_uris_contains(&row.workspace_uris, cwd) {
                matched.push(row);
                if matched.len() >= 16 {
                    break;
                }
            }
        }
        SummariesQueryResult::Rows(matched)
    }

    fn title_from_summary(row: &SummaryRow) -> Option<String> {
        for candidate in [row.title.as_str(), row.preview.as_str()] {
            let clean = normalize_title(candidate);
            if !clean.is_empty() {
                return Some(clean);
            }
        }
        None
    }

    fn first_workspace_path(row: &SummaryRow) -> Option<String> {
        let uris: Vec<String> = serde_json::from_str(&row.workspace_uris).ok()?;
        uris.into_iter().find_map(|u| file_uri_to_path(&u))
    }
}

fn workspace_uris_contains(workspace_uris_json: &str, cwd: &str) -> bool {
    let Ok(uris) = serde_json::from_str::<Vec<String>>(workspace_uris_json) else {
        return false;
    };
    uris.iter()
        .filter_map(|u| file_uri_to_path(u))
        .any(|p| p == cwd)
}

fn is_agy_binary_name(name: &str) -> bool {
    name == "agy" || name == "agy-cli" || (name.starts_with("agy.") && name.ends_with(".old"))
}

impl Adapter for AntigravityAdapter {
    fn tool(&self) -> Tool {
        Tool::Antigravity
    }

    fn matches_process(&self, p: &ProcInfo) -> bool {
        if let Some(exe) = &p.exe_path {
            let s = exe.to_string_lossy();
            if s.contains("Antigravity.app/") || s.contains("Antigravity IDE.app/") {
                return false;
            }
        }

        if p.argv
            .iter()
            .skip(1)
            .any(|a| NON_INTERACTIVE_ARGS.contains(&a.as_str()) || a.starts_with("--hub"))
        {
            return false;
        }

        let argv0_is_agy = p.argv.first().is_some_and(|a| {
            Path::new(a)
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(is_agy_binary_name)
        });
        let exe_is_agy = p.exe_path.as_ref().is_some_and(|exe| {
            exe.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(is_agy_binary_name)
        });

        argv0_is_agy || exe_is_agy
    }

    fn session_for_process(
        &self,
        p: &ProcInfo,
        platform: &dyn Platform,
        claimed_ids: &HashSet<String>,
    ) -> Result<Option<SessionRef>> {
        let proc_cwd = p.cwd.as_ref().map(|c| c.to_string_lossy().into_owned());
        // An Antigravity process is running right now -> never allow `immutable=1`.
        let allow_immutable = false;

        // 1. Exact: open FD on `presence/<id>.lock` or `conversations/<id>.db`.
        let open_files = platform.process_open_files(p.pid);
        if let Some(id) = Self::extract_id_from_open_files(&open_files) {
            let summary = self.query_summary_by_id(&id, allow_immutable);
            let cwd = proc_cwd
                .clone()
                .or_else(|| summary.as_ref().and_then(Self::first_workspace_path))
                .unwrap_or_default();
            if !cwd.is_empty() {
                let title_hint = summary.as_ref().and_then(Self::title_from_summary);
                let transcript = self.conversation_db_path(&id);
                return Ok(Some(SessionRef {
                    tool: Tool::Antigravity,
                    session_id: id,
                    cwd,
                    transcript: Some(transcript.to_string_lossy().into_owned()),
                    title_hint,
                    best_guess: false,
                }));
            }
        }

        // 2. Exact: `--conversation <id>` in argv.
        if let Some(id) = Self::extract_conversation_arg(&p.argv) {
            let summary = self.query_summary_by_id(&id, allow_immutable);
            let cwd = proc_cwd
                .clone()
                .or_else(|| summary.as_ref().and_then(Self::first_workspace_path))
                .unwrap_or_default();
            if !cwd.is_empty() {
                let title_hint = summary.as_ref().and_then(Self::title_from_summary);
                let transcript = self.conversation_db_path(&id);
                return Ok(Some(SessionRef {
                    tool: Tool::Antigravity,
                    session_id: id,
                    cwd,
                    transcript: Some(transcript.to_string_lossy().into_owned()),
                    title_hint,
                    best_guess: false,
                }));
            }
        }

        let Some(cwd) = proc_cwd else {
            return Ok(None);
        };

        // 3. Fallback 1: `conversation_summaries.db` (`allow_immutable = false`).
        let mut db_skipped = false;
        match self.query_summaries_for_cwd(&cwd, allow_immutable) {
            SummariesQueryResult::Rows(rows) => {
                if let Some(chosen) = rows
                    .iter()
                    .find(|r| !claimed_ids.contains(&r.conversation_id))
                    .or_else(|| rows.first())
                {
                    let ambiguous = rows.len() > 1;
                    let title_hint = Self::title_from_summary(chosen);
                    let transcript = self.conversation_db_path(&chosen.conversation_id);
                    return Ok(Some(SessionRef {
                        tool: Tool::Antigravity,
                        session_id: chosen.conversation_id.clone(),
                        cwd,
                        transcript: Some(transcript.to_string_lossy().into_owned()),
                        title_hint,
                        best_guess: ambiguous,
                    }));
                }
            }
            SummariesQueryResult::Skipped => {
                db_skipped = true;
            }
            SummariesQueryResult::NotFound => {}
        }

        // 4. Fallback 2: `cache/last_conversations.json[cwd]`.
        if let Some(map) = self.read_last_conversations_map() {
            if let Some(id) = map.get(&cwd) {
                let title_hint = self
                    .query_summary_by_id(id, allow_immutable)
                    .as_ref()
                    .and_then(Self::title_from_summary);
                let transcript = self.conversation_db_path(id);
                return Ok(Some(SessionRef {
                    tool: Tool::Antigravity,
                    session_id: id.clone(),
                    cwd,
                    transcript: Some(transcript.to_string_lossy().into_owned()),
                    title_hint,
                    best_guess: db_skipped || claimed_ids.contains(id),
                }));
            }
        }

        Ok(None)
    }

    fn session_exists(&self, s: &Session, _tool_running: bool) -> bool {
        self.conversation_db_path(&s.session_id).is_file()
    }

    fn title(&self, s: &Session, tool_running: bool) -> Option<String> {
        let row = self.query_summary_by_id(&s.session_id, !tool_running)?;
        Self::title_from_summary(&row)
    }

    fn resume_command(&self, s: &Session, cfg: &ToolConfig) -> String {
        expand_resume_template(&cfg.resume, &s.session_id)
    }

    fn doctor_checks(&self, tool_running: bool) -> Vec<DoctorCheck> {
        let db_path = self.paths.antigravity_summaries_db();
        if !db_path.is_file() {
            return Vec::new();
        }
        let check = match open_external_readonly(&db_path, !tool_running) {
            ExternalDbOpen::Opened(conn) => match verify_table_columns(
                &conn,
                "conversation_summaries",
                &["conversation_id", "title", "preview", "workspace_uris"],
            ) {
                Ok(()) => DoctorCheck {
                    name: "schema:antigravity".to_string(),
                    status: CheckStatus::Ok,
                    detail: format!("{} schema verified (read-only)", db_path.display()),
                },
                Err(reason) => DoctorCheck {
                    name: "schema:antigravity".to_string(),
                    status: CheckStatus::Warn,
                    detail: format!(
                        "{}: {reason}; falling back to presence lock / cache files",
                        db_path.display()
                    ),
                },
            },
            ExternalDbOpen::Skipped | ExternalDbOpen::NotFound => DoctorCheck {
                name: "schema:antigravity".to_string(),
                status: CheckStatus::Warn,
                detail: format!(
                    "skipped {} read-only check (immutable=1 disallowed while agy is running)",
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
    use rusqlite::Connection;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TestHome(PathBuf);
    impl TestHome {
        fn new() -> Self {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir =
                std::env::temp_dir().join(format!("restowa-agy-test-{}-{}", std::process::id(), n));
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

    struct FakePlatform {
        open_files: HashMap<i32, Vec<PathBuf>>,
    }

    impl Platform for FakePlatform {
        fn boot_id(&self) -> Result<String> {
            Ok("1.0".into())
        }
        fn list_processes(&self) -> Result<Vec<ProcInfo>> {
            Ok(Vec::new())
        }
        fn process_info(&self, _pid: i32) -> Option<ProcInfo> {
            None
        }
        fn process_open_files(&self, pid: i32) -> Vec<PathBuf> {
            self.open_files.get(&pid).cloned().unwrap_or_default()
        }
        fn parent_pid(&self) -> i32 {
            1
        }
    }

    #[test]
    fn matches_process_accepts_auto_updated_agy_and_rejects_hub_acp_and_ide() {
        let home = TestHome::new();
        let adapter = AntigravityAdapter::new(AppPaths::from_home(home.0.clone()));

        let live_old = ProcInfo {
            pid: 15517,
            ppid: 2012,
            start_time_us: 1,
            exe_path: Some(PathBuf::from(
                "/Users/u/.local/bin/agy.1794331593374468000.old",
            )),
            argv: vec!["agy".into()],
            cwd: Some(PathBuf::from("/Users/u")),
        };
        assert!(adapter.matches_process(&live_old));

        let hub = ProcInfo {
            pid: 18225,
            ppid: 18065,
            start_time_us: 1,
            exe_path: Some(PathBuf::from("/Users/u/.gemini/bin/agy")),
            argv: vec!["agy".into(), "--hub".into(), "--hub-port=55437".into()],
            cwd: Some(PathBuf::from("/Users/u")),
        };
        assert!(!adapter.matches_process(&hub));

        let acp = ProcInfo {
            pid: 49742,
            ppid: 98543,
            start_time_us: 1,
            exe_path: Some(PathBuf::from("/Users/u/.local/bin/agy")),
            argv: vec!["/Users/u/.local/bin/agy".into(), "acp".into()],
            cwd: Some(PathBuf::from("/Users/u")),
        };
        assert!(!adapter.matches_process(&acp));

        let ide_helper = ProcInfo {
            pid: 18331,
            ppid: 1,
            start_time_us: 1,
            exe_path: Some(PathBuf::from(
                "/Applications/Antigravity IDE.app/Contents/MacOS/Antigravity IDE",
            )),
            argv: vec!["agy".into()],
            cwd: None,
        };
        assert!(!adapter.matches_process(&ide_helper));
    }

    #[test]
    fn resolves_exact_id_from_presence_lock_fd_and_reads_summary_title() {
        let home = TestHome::new();
        let paths = AppPaths::from_home(home.0.clone());
        std::fs::create_dir_all(paths.antigravity_conversations_dir()).unwrap();

        let id = "8c257602-0000-4000-8000-000000000002";
        std::fs::write(
            paths
                .antigravity_conversations_dir()
                .join(format!("{id}.db")),
            b"sqlite",
        )
        .unwrap();

        {
            let conn = Connection::open(paths.antigravity_summaries_db()).unwrap();
            conn.execute_batch(
                "CREATE TABLE conversation_summaries (
                    conversation_id TEXT PRIMARY KEY,
                    title TEXT NOT NULL DEFAULT '',
                    preview TEXT NOT NULL DEFAULT '',
                    workspace_uris TEXT NOT NULL DEFAULT '[]',
                    parent_conversation_id TEXT NOT NULL DEFAULT '',
                    last_modified_time TEXT NOT NULL DEFAULT ''
                );
                INSERT INTO conversation_summaries VALUES (
                    '8c257602-0000-4000-8000-000000000002',
                    'AWS Profiles Audit',
                    'show all aws profiles',
                    '[\"file:///Users/dev/cloud\"]',
                    '',
                    '2026-11-10T19:44:00Z'
                );",
            )
            .unwrap();
        }

        let mut open_files = HashMap::new();
        open_files.insert(
            15517,
            vec![paths
                .antigravity_cli_dir()
                .join(format!("presence/{id}.lock"))],
        );
        let platform = FakePlatform { open_files };
        let adapter = AntigravityAdapter::new(paths);

        let p = ProcInfo {
            pid: 15517,
            ppid: 1,
            start_time_us: 100,
            exe_path: Some(PathBuf::from("/Users/u/.local/bin/agy")),
            argv: vec!["agy".into()],
            cwd: Some(PathBuf::from("/Users/dev/cloud")),
        };

        let resolved = adapter
            .session_for_process(&p, &platform, &HashSet::new())
            .unwrap()
            .unwrap();
        assert_eq!(resolved.session_id, id);
        assert_eq!(resolved.title_hint.as_deref(), Some("AWS Profiles Audit"));
        assert!(!resolved.best_guess);
    }

    #[test]
    fn missing_summaries_table_falls_back_to_last_conversations_cache_with_best_guess_and_doctor_warns(
    ) {
        let home = TestHome::new();
        let paths = AppPaths::from_home(home.0.clone());
        std::fs::create_dir_all(paths.antigravity_last_conversations().parent().unwrap()).unwrap();

        {
            let conn = Connection::open(paths.antigravity_summaries_db()).unwrap();
            conn.execute("CREATE TABLE broken_schema (id TEXT)", [])
                .unwrap();
        }

        std::fs::write(
            paths.antigravity_last_conversations(),
            r#"{"/Users/dev/proj": "9226b940-0000-4000-8000-000000000004"}"#,
        )
        .unwrap();

        let adapter = AntigravityAdapter::new(paths);
        let platform = FakePlatform {
            open_files: HashMap::new(),
        };
        let p = ProcInfo {
            pid: 700,
            ppid: 1,
            start_time_us: 100,
            exe_path: Some(PathBuf::from("/Users/u/.local/bin/agy")),
            argv: vec!["agy".into()],
            cwd: Some(PathBuf::from("/Users/dev/proj")),
        };

        let resolved = adapter
            .session_for_process(&p, &platform, &HashSet::new())
            .unwrap()
            .unwrap();
        assert_eq!(resolved.session_id, "9226b940-0000-4000-8000-000000000004");
        assert!(resolved.best_guess);

        let checks = adapter.doctor_checks(false);
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, CheckStatus::Warn);
        assert!(checks[0]
            .detail
            .contains("missing table 'conversation_summaries'"));
    }
}
