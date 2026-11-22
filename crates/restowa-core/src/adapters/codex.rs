use crate::adapters::claude::{normalize_title, read_bounded_line};
use crate::adapters::sqlite_ro::{open_external_readonly, verify_table_columns};
use crate::adapters::Adapter;
use crate::config::ToolConfig;
use crate::doctor::{CheckStatus, DoctorCheck};
use crate::error::Result;
use crate::model::{ProcInfo, Session, SessionRef, Tool};
use crate::paths::AppPaths;
use crate::platform::Platform;
use crate::shell::expand_resume_template;
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const MAX_ROLLOUT_LINE_BYTES: usize = 64 * 1024;
const MAX_ROLLOUT_TITLE_LINES: usize = 100;

const NON_INTERACTIVE_SUBCOMMANDS: &[&str] = &[
    "app-server",
    "exec-server",
    "exec",
    "e",
    "review",
    "mcp",
    "sandbox",
    "remote-control",
    "debug",
    "doctor",
    "login",
    "logout",
    "update",
    "completion",
    "apply",
    "archive",
    "unarchive",
    "delete",
    "migrate-rollouts",
    "migrate-nfs-db",
    "cloud",
    "features",
    "help",
    "plugin",
    "queue",
    "app",
];

#[derive(Debug, Clone)]
pub struct CodexAdapter {
    paths: AppPaths,
}

#[derive(Debug, Clone)]
struct RolloutMeta {
    id: String,
    cwd: String,
    path: PathBuf,
}

#[derive(Debug, Clone)]
struct StateThreadRow {
    id: String,
    rollout_path: String,
    cwd: String,
    title: String,
    first_user_message: String,
    name: Option<String>,
}

impl CodexAdapter {
    pub fn new(paths: AppPaths) -> Self {
        Self { paths }
    }

    /// Extract `<session_id>` from `codex [opts...] resume [opts...] <session_id>`.
    fn extract_resume_arg(argv: &[String]) -> Option<String> {
        let resume_idx = argv.iter().skip(1).position(|a| a == "resume")? + 1;
        let mut iter = argv.iter().skip(resume_idx + 1);
        while let Some(arg) = iter.next() {
            if matches!(
                arg.as_str(),
                "-c" | "--config"
                    | "-m"
                    | "--model"
                    | "-p"
                    | "--profile"
                    | "-s"
                    | "--sandbox"
                    | "-C"
                    | "--cd"
                    | "-a"
                    | "--ask-for-approval"
                    | "-i"
                    | "--image"
                    | "--add-dir"
                    | "--enable"
                    | "--disable"
                    | "--remote"
                    | "--remote-auth-token-env"
                    | "--local-provider"
            ) {
                let _ = iter.next();
                continue;
            }
            if arg.starts_with('-') {
                continue;
            }
            if !arg.is_empty() {
                return Some(arg.clone());
            }
        }
        None
    }

    /// Parse the first line (`session_meta`) of a Codex `rollout-*.jsonl` file
    /// without reading the rest of the file.
    fn read_rollout_meta(path: &Path) -> Option<RolloutMeta> {
        let file = File::open(path).ok()?;
        let mut reader = BufReader::with_capacity(8 * 1024, file);
        let mut buf = Vec::with_capacity(2048);

        for _ in 0..5 {
            buf.clear();
            let complete = read_bounded_line(&mut reader, &mut buf, MAX_ROLLOUT_LINE_BYTES)?;
            if !complete {
                continue;
            }
            let Ok(line) = std::str::from_utf8(&buf) else {
                continue;
            };
            let Ok(val) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if val.get("type").and_then(|v| v.as_str()) == Some("session_meta") {
                let id = val
                    .pointer("/payload/id")
                    .or_else(|| val.pointer("/payload/session_id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let cwd = val
                    .pointer("/payload/cwd")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                if !id.is_empty() {
                    return Some(RolloutMeta {
                        id,
                        cwd,
                        path: path.to_path_buf(),
                    });
                }
            }
        }
        None
    }

    /// Extract the first user prompt from a Codex `rollout-*.jsonl` file.
    pub fn extract_title_from_rollout(path: &Path) -> Option<String> {
        let file = File::open(path).ok()?;
        let mut reader = BufReader::with_capacity(8 * 1024, file);
        let mut buf = Vec::with_capacity(2048);

        for _ in 0..MAX_ROLLOUT_TITLE_LINES {
            buf.clear();
            let complete = read_bounded_line(&mut reader, &mut buf, MAX_ROLLOUT_LINE_BYTES)?;
            if !complete {
                continue;
            }
            let Ok(line) = std::str::from_utf8(&buf) else {
                continue;
            };
            if !line.contains("\"user\"") {
                continue;
            }
            let Ok(val) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if val.get("type").and_then(|v| v.as_str()) != Some("response_item") {
                continue;
            }
            if val.pointer("/payload/role").and_then(|v| v.as_str()) != Some("user") {
                continue;
            }
            if let Some(arr) = val.pointer("/payload/content").and_then(|v| v.as_array()) {
                for item in arr {
                    if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
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

    /// Query `~/.codex/state_5.sqlite` read-only for recent CLI threads in `cwd`.
    /// Returns `None` if DB or expected schema is missing (never crashes).
    fn query_state_threads_for_cwd(&self, cwd: &str) -> Option<Vec<StateThreadRow>> {
        let db_path = self.paths.codex_state_db();
        let conn = open_external_readonly(&db_path)?;
        if verify_table_columns(&conn, "threads", &["id", "rollout_path", "cwd"]).is_err() {
            return None;
        }
        let mut stmt = conn
            .prepare(
                "SELECT id, rollout_path, cwd, title, first_user_message, name
                 FROM threads
                 WHERE archived = 0
                   AND cwd = ?1
                   AND (source = 'cli' OR source = '')
                 ORDER BY COALESCE(updated_at_ms, updated_at * 1000) DESC, id DESC
                 LIMIT 2",
            )
            .ok()?;
        let rows = stmt
            .query_map([cwd], |r| {
                Ok(StateThreadRow {
                    id: r.get(0)?,
                    rollout_path: r.get(1)?,
                    cwd: r.get(2)?,
                    title: r.get::<_, Option<String>>(3)?.unwrap_or_default(),
                    first_user_message: r.get::<_, Option<String>>(4)?.unwrap_or_default(),
                    name: r.get(5)?,
                })
            })
            .ok()?;
        let mut out = Vec::new();
        for r in rows.flatten() {
            out.push(r);
        }
        Some(out)
    }

    fn query_state_thread_by_id(&self, session_id: &str) -> Option<StateThreadRow> {
        let db_path = self.paths.codex_state_db();
        let conn = open_external_readonly(&db_path)?;
        if verify_table_columns(&conn, "threads", &["id", "rollout_path", "cwd"]).is_err() {
            return None;
        }
        conn.query_row(
            "SELECT id, rollout_path, cwd, title, first_user_message, name
             FROM threads
             WHERE id = ?1 AND archived = 0
             LIMIT 1",
            [session_id],
            |r| {
                Ok(StateThreadRow {
                    id: r.get(0)?,
                    rollout_path: r.get(1)?,
                    cwd: r.get(2)?,
                    title: r.get::<_, Option<String>>(3)?.unwrap_or_default(),
                    first_user_message: r.get::<_, Option<String>>(4)?.unwrap_or_default(),
                    name: r.get(5)?,
                })
            },
        )
        .ok()
    }

    fn best_title_from_row(row: &StateThreadRow) -> Option<String> {
        for candidate in [
            row.name.as_deref().unwrap_or(""),
            row.title.as_str(),
            row.first_user_message.as_str(),
        ] {
            let clean = normalize_title(candidate);
            if !clean.is_empty() {
                return Some(clean);
            }
        }
        None
    }

    fn list_rollout_files_newest_first(&self) -> Vec<PathBuf> {
        let root = self.paths.codex_sessions_dir();
        if !root.is_dir() {
            return Vec::new();
        }
        let mut files: Vec<(SystemTime, PathBuf)> = Vec::new();
        collect_rollout_files(&root, 0, &mut files);
        files.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
        files.into_iter().map(|(_, p)| p).collect()
    }

    fn find_rollout_by_id(&self, session_id: &str) -> Option<PathBuf> {
        let suffix = format!("{session_id}.jsonl");
        self.list_rollout_files_newest_first()
            .into_iter()
            .find(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("rollout-") && n.ends_with(&suffix))
            })
    }

    fn find_rollouts_for_cwd(&self, cwd: &str) -> Vec<RolloutMeta> {
        let mut matches = Vec::new();
        for path in self.list_rollout_files_newest_first().into_iter().take(200) {
            if let Some(meta) = Self::read_rollout_meta(&path) {
                if meta.cwd == cwd {
                    matches.push(meta);
                    if matches.len() >= 2 {
                        break;
                    }
                }
            }
        }
        matches
    }
}

fn collect_rollout_files(dir: &Path, depth: usize, out: &mut Vec<(SystemTime, PathBuf)>) {
    if depth > 4 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rollout_files(&path, depth + 1, out);
        } else if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("rollout-") && n.ends_with(".jsonl"))
        {
            let mtime = entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            out.push((mtime, path));
        }
    }
}

fn is_rollout_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with("rollout-") && n.ends_with(".jsonl"))
}

impl Adapter for CodexAdapter {
    fn tool(&self) -> Tool {
        Tool::Codex
    }

    fn matches_process(&self, p: &ProcInfo) -> bool {
        // Reject IDE extension daemons and Node wrapper scripts (match the real Rust binary).
        if let Some(exe) = &p.exe_path {
            let s = exe.to_string_lossy();
            if s.contains(".cursor/extensions/")
                || s.contains(".vscode/extensions/")
                || s.contains("/plugins/cache/")
            {
                return false;
            }
            if exe
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n == "node" || n == "bun")
            {
                return false;
            }
        }

        if let Some(argv0) = p.argv.first() {
            if Path::new(argv0)
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n == "node" || n == "bun")
            {
                return false;
            }
        }

        if p.argv
            .iter()
            .skip(1)
            .any(|a| NON_INTERACTIVE_SUBCOMMANDS.contains(&a.as_str()))
        {
            return false;
        }

        let exe_is_codex = p.exe_path.as_ref().is_some_and(|exe| {
            exe.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n == "codex")
        });
        let argv0_is_codex = p.argv.first().is_some_and(|a| {
            Path::new(a)
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n == "codex")
        });

        exe_is_codex || argv0_is_codex
    }

    fn session_for_process(
        &self,
        p: &ProcInfo,
        platform: &dyn Platform,
    ) -> Result<Option<SessionRef>> {
        let proc_cwd = p.cwd.as_ref().map(|c| c.to_string_lossy().into_owned());

        // 1. Exact: open `rollout-*.jsonl` file descriptor via libproc.
        //    Handles multiple Codex sessions running in the exact same folder.
        for fd_path in platform.process_open_files(p.pid) {
            if is_rollout_path(&fd_path) {
                if let Some(meta) = Self::read_rollout_meta(&fd_path) {
                    let cwd = if !meta.cwd.is_empty() {
                        meta.cwd
                    } else {
                        proc_cwd.clone().unwrap_or_default()
                    };
                    if !cwd.is_empty() {
                        let title_hint = Self::extract_title_from_rollout(&fd_path).or_else(|| {
                            self.query_state_thread_by_id(&meta.id)
                                .and_then(|r| Self::best_title_from_row(&r))
                        });
                        return Ok(Some(SessionRef {
                            tool: Tool::Codex,
                            session_id: meta.id,
                            cwd,
                            transcript: Some(meta.path.to_string_lossy().into_owned()),
                            title_hint,
                            best_guess: false,
                        }));
                    }
                }
            }
        }

        // 2. Exact: `codex resume <session_id>` in argv.
        if let Some(session_id) = Self::extract_resume_arg(&p.argv) {
            if let Some(row) = self.query_state_thread_by_id(&session_id) {
                let cwd = if !row.cwd.is_empty() {
                    row.cwd.clone()
                } else {
                    proc_cwd.clone().unwrap_or_default()
                };
                if !cwd.is_empty() {
                    let transcript = if !row.rollout_path.is_empty() {
                        Some(row.rollout_path.clone())
                    } else {
                        self.find_rollout_by_id(&session_id)
                            .map(|p| p.to_string_lossy().into_owned())
                    };
                    let title_hint = Self::best_title_from_row(&row).or_else(|| {
                        transcript
                            .as_deref()
                            .and_then(|t| Self::extract_title_from_rollout(Path::new(t)))
                    });
                    return Ok(Some(SessionRef {
                        tool: Tool::Codex,
                        session_id,
                        cwd,
                        transcript,
                        title_hint,
                        best_guess: false,
                    }));
                }
            } else if let Some(cwd) = proc_cwd.clone() {
                let transcript = self
                    .find_rollout_by_id(&session_id)
                    .map(|p| p.to_string_lossy().into_owned());
                let title_hint = transcript
                    .as_deref()
                    .and_then(|t| Self::extract_title_from_rollout(Path::new(t)));
                return Ok(Some(SessionRef {
                    tool: Tool::Codex,
                    session_id,
                    cwd,
                    transcript,
                    title_hint,
                    best_guess: false,
                }));
            }
        }

        let Some(cwd) = proc_cwd else {
            return Ok(None);
        };

        // 3. Fallback 1: read-only query against `state_5.sqlite` (threads table).
        if let Some(rows) = self.query_state_threads_for_cwd(&cwd) {
            if let Some(first) = rows.first() {
                let ambiguous = rows.len() > 1;
                let transcript = if !first.rollout_path.is_empty() {
                    Some(first.rollout_path.clone())
                } else {
                    self.find_rollout_by_id(&first.id)
                        .map(|p| p.to_string_lossy().into_owned())
                };
                let title_hint = Self::best_title_from_row(first).or_else(|| {
                    transcript
                        .as_deref()
                        .and_then(|t| Self::extract_title_from_rollout(Path::new(t)))
                });
                return Ok(Some(SessionRef {
                    tool: Tool::Codex,
                    session_id: first.id.clone(),
                    cwd: first.cwd.clone(),
                    transcript,
                    title_hint,
                    best_guess: ambiguous,
                }));
            }
        }

        // 4. Fallback 2: newest rollout file in `~/.codex/sessions` matching `cwd`.
        let rollouts = self.find_rollouts_for_cwd(&cwd);
        if let Some(first) = rollouts.first() {
            let ambiguous = rollouts.len() > 1;
            let title_hint = Self::extract_title_from_rollout(&first.path);
            return Ok(Some(SessionRef {
                tool: Tool::Codex,
                session_id: first.id.clone(),
                cwd: first.cwd.clone(),
                transcript: Some(first.path.to_string_lossy().into_owned()),
                title_hint,
                best_guess: ambiguous,
            }));
        }

        Ok(None)
    }

    fn session_exists(&self, s: &Session) -> bool {
        if let Some(transcript) = &s.transcript {
            if Path::new(transcript).is_file() {
                return true;
            }
        }
        if self.query_state_thread_by_id(&s.session_id).is_some() {
            return true;
        }
        self.find_rollout_by_id(&s.session_id).is_some()
    }

    fn title(&self, s: &Session) -> Option<String> {
        if let Some(row) = self.query_state_thread_by_id(&s.session_id) {
            if let Some(t) = Self::best_title_from_row(&row) {
                return Some(t);
            }
        }
        if let Some(transcript) = &s.transcript {
            if let Some(t) = Self::extract_title_from_rollout(Path::new(transcript)) {
                return Some(t);
            }
        }
        self.find_rollout_by_id(&s.session_id)
            .and_then(|p| Self::extract_title_from_rollout(&p))
    }

    fn resume_command(&self, s: &Session, cfg: &ToolConfig) -> String {
        expand_resume_template(&cfg.resume, &s.session_id)
    }

    fn doctor_checks(&self) -> Vec<DoctorCheck> {
        let db_path = self.paths.codex_state_db();
        if !db_path.is_file() {
            return Vec::new();
        }
        let check = match open_external_readonly(&db_path) {
            Some(conn) => {
                match verify_table_columns(&conn, "threads", &["id", "rollout_path", "cwd"]) {
                    Ok(()) => DoctorCheck {
                        name: "schema:codex".to_string(),
                        status: CheckStatus::Ok,
                        detail: format!("{} schema verified (read-only)", db_path.display()),
                    },
                    Err(reason) => DoctorCheck {
                        name: "schema:codex".to_string(),
                        status: CheckStatus::Warn,
                        detail: format!(
                            "{}: {reason}; falling back to rollout files",
                            db_path.display()
                        ),
                    },
                }
            }
            None => DoctorCheck {
                name: "schema:codex".to_string(),
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
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TestHome(PathBuf);
    impl TestHome {
        fn new() -> Self {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "restowa-codex-test-{}-{}",
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
    fn matches_process_excludes_node_wrapper_and_app_server() {
        let home = TestHome::new();
        let adapter = CodexAdapter::new(AppPaths::from_home(home.0.clone()));

        let real_codex = ProcInfo {
            pid: 100,
            ppid: 99,
            start_time_us: 1,
            exe_path: Some(PathBuf::from(
                "/opt/homebrew/lib/node_modules/@openai/codex/vendor/aarch64-apple-darwin/bin/codex",
            )),
            argv: vec!["codex".into()],
            cwd: Some(PathBuf::from("/tmp/p")),
        };
        assert!(adapter.matches_process(&real_codex));

        let node_wrapper = ProcInfo {
            pid: 99,
            ppid: 1,
            start_time_us: 1,
            exe_path: Some(PathBuf::from("/opt/homebrew/bin/node")),
            argv: vec!["node".into(), "/opt/homebrew/bin/codex".into()],
            cwd: Some(PathBuf::from("/tmp/p")),
        };
        assert!(!adapter.matches_process(&node_wrapper));

        let cursor_daemon = ProcInfo {
            pid: 21398,
            ppid: 21154,
            start_time_us: 1,
            exe_path: Some(PathBuf::from(
                "/Users/u/.cursor/extensions/openai.chatgpt/bin/macos-aarch64/codex",
            )),
            argv: vec!["codex".into(), "app-server".into()],
            cwd: Some(PathBuf::from("/")),
        };
        assert!(!adapter.matches_process(&cursor_daemon));
    }

    #[test]
    fn two_codex_sessions_in_same_folder_resolve_to_exact_distinct_ids_via_open_fds() {
        let home = TestHome::new();
        let paths = AppPaths::from_home(home.0.clone());
        let sess_dir = paths.codex_sessions_dir().join("2026/11/22");
        std::fs::create_dir_all(&sess_dir).unwrap();

        let cwd = "/Users/dev/sample-lib";
        let r1 =
            sess_dir.join("rollout-2026-11-22T10-00-00-019a0000-0000-7000-8000-000000000001.jsonl");
        let r2 =
            sess_dir.join("rollout-2026-11-22T10-05-00-019a0000-0000-7000-8000-000000000002.jsonl");

        std::fs::write(
            &r1,
            format!(
                "{{\"timestamp\":\"2026-11-22T10:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"019a0000-0000-7000-8000-000000000001\",\"cwd\":\"{cwd}\"}}}}\n\
                 {{\"timestamp\":\"2026-11-22T10:00:01Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"user\",\"content\":[{{\"type\":\"input_text\",\"text\":\"Port the TCP handshake\"}}]}}}}\n"
            ),
        )
        .unwrap();
        std::fs::write(
            &r2,
            format!(
                "{{\"timestamp\":\"2026-11-22T10:05:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"019a0000-0000-7000-8000-000000000002\",\"cwd\":\"{cwd}\"}}}}\n\
                 {{\"timestamp\":\"2026-11-22T10:05:01Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"user\",\"content\":[{{\"type\":\"input_text\",\"text\":\"Add framing fuzz tests\"}}]}}}}\n"
            ),
        )
        .unwrap();

        let mut open_files = HashMap::new();
        open_files.insert(501, vec![r1.clone()]);
        open_files.insert(502, vec![r2.clone()]);
        let platform = FakePlatform { open_files };

        let adapter = CodexAdapter::new(paths);
        let p1 = ProcInfo {
            pid: 501,
            ppid: 1,
            start_time_us: 100,
            exe_path: Some(PathBuf::from("/opt/homebrew/bin/codex")),
            argv: vec!["codex".into()],
            cwd: Some(PathBuf::from(cwd)),
        };
        let p2 = ProcInfo {
            pid: 502,
            ppid: 1,
            start_time_us: 200,
            exe_path: Some(PathBuf::from("/opt/homebrew/bin/codex")),
            argv: vec!["codex".into()],
            cwd: Some(PathBuf::from(cwd)),
        };

        let s1 = adapter
            .session_for_process(&p1, &platform)
            .unwrap()
            .unwrap();
        let s2 = adapter
            .session_for_process(&p2, &platform)
            .unwrap()
            .unwrap();

        assert_eq!(s1.session_id, "019a0000-0000-7000-8000-000000000001");
        assert_eq!(s1.title_hint.as_deref(), Some("Port the TCP handshake"));
        assert!(!s1.best_guess);

        assert_eq!(s2.session_id, "019a0000-0000-7000-8000-000000000002");
        assert_eq!(s2.title_hint.as_deref(), Some("Add framing fuzz tests"));
        assert!(!s2.best_guess);
    }

    #[test]
    fn missing_state_table_falls_back_to_rollout_and_doctor_warns_without_crashing() {
        let home = TestHome::new();
        let paths = AppPaths::from_home(home.0.clone());
        std::fs::create_dir_all(paths.codex_dir()).unwrap();

        // Create a state_5.sqlite with a completely unrelated table (missing `threads`)
        {
            let conn = Connection::open(paths.codex_state_db()).unwrap();
            conn.execute("CREATE TABLE other_table (x INTEGER)", [])
                .unwrap();
        }

        // Seed 2 rollouts in the same cwd so fallback is ambiguous (best_guess = true)
        let sess_dir = paths.codex_sessions_dir().join("2026/11/22");
        std::fs::create_dir_all(&sess_dir).unwrap();
        let cwd = "/Users/dev/same-dir";
        for i in 1..=2 {
            let f = sess_dir.join(format!("rollout-2026-11-22T10-0{i}-00-019a-id-{i}.jsonl"));
            std::fs::write(
                &f,
                format!(
                    "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"019a-id-{i}\",\"cwd\":\"{cwd}\"}}}}\n"
                ),
            )
            .unwrap();
        }

        let adapter = CodexAdapter::new(paths);
        let platform = FakePlatform {
            open_files: HashMap::new(),
        };
        let proc_info = ProcInfo {
            pid: 900,
            ppid: 1,
            start_time_us: 100,
            exe_path: Some(PathBuf::from("/opt/homebrew/bin/codex")),
            argv: vec!["codex".into()],
            cwd: Some(PathBuf::from(cwd)),
        };

        let resolved = adapter
            .session_for_process(&proc_info, &platform)
            .unwrap()
            .unwrap();
        assert!(
            resolved.best_guess,
            "ambiguous fallback should set best_guess"
        );

        let checks = adapter.doctor_checks();
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, CheckStatus::Warn);
        assert!(checks[0].detail.contains("missing table 'threads'"));
    }
}
