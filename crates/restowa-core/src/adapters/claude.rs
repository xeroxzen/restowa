use crate::adapters::Adapter;
use crate::config::ToolConfig;
use crate::error::Result;
use crate::model::{ProcInfo, Session, SessionRef, Tool};
use crate::paths::AppPaths;
use crate::platform::Platform;
use crate::shell::expand_resume_template;
use serde::Deserialize;
use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

const MAX_TITLE_CHARS: usize = 80;
const MAX_TRANSCRIPT_SCAN_LINES: usize = 250;
const MAX_LINE_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone)]
pub struct ClaudeAdapter {
    paths: AppPaths,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PidSessionFile {
    pid: Option<i32>,
    session_id: String,
    cwd: String,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    entrypoint: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

impl ClaudeAdapter {
    pub fn new(paths: AppPaths) -> Self {
        Self { paths }
    }

    /// Claude Code replaces every character outside `[A-Za-z0-9-]` with `-`
    /// when deriving the project directory name under `~/.claude/projects/`.
    pub fn encode_project_dir(cwd: &str) -> String {
        cwd.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' {
                    c
                } else {
                    '-'
                }
            })
            .collect()
    }

    pub fn expected_transcript_path(&self, cwd: &str, session_id: &str) -> PathBuf {
        let encoded = Self::encode_project_dir(cwd);
        self.paths
            .claude_projects_dir()
            .join(encoded)
            .join(format!("{session_id}.jsonl"))
    }

    pub fn pid_session_path(&self, pid: i32) -> PathBuf {
        self.paths.claude_sessions_dir().join(format!("{pid}.json"))
    }

    fn read_pid_session(&self, pid: i32) -> Option<PidSessionFile> {
        let path = self.pid_session_path(pid);
        let raw = std::fs::read_to_string(&path).ok()?;
        let parsed: PidSessionFile = serde_json::from_str(&raw).ok()?;
        if let Some(file_pid) = parsed.pid {
            if file_pid != pid {
                return None;
            }
        }
        if parsed.session_id.is_empty() || parsed.cwd.is_empty() {
            return None;
        }
        // Ignore non-interactive helper entries if explicitly tagged otherwise.
        if let Some(kind) = parsed.kind.as_deref() {
            if kind != "interactive" && !kind.is_empty() {
                return None;
            }
        }
        let _ = parsed.entrypoint;
        Some(parsed)
    }

    fn extract_resume_arg(argv: &[String]) -> Option<String> {
        let mut iter = argv.iter().skip(1);
        while let Some(arg) = iter.next() {
            if arg == "--resume" || arg == "-r" {
                if let Some(val) = iter.next() {
                    if !val.is_empty() && !val.starts_with('-') {
                        return Some(val.clone());
                    }
                }
            } else if let Some(val) = arg.strip_prefix("--resume=") {
                if !val.is_empty() {
                    return Some(val.to_string());
                }
            }
        }
        None
    }

    /// Read a Claude Code JSONL transcript line-by-line (stopping early) and
    /// extract the first human user prompt as a single-line title ≤ 80 chars.
    ///
    /// Caps per-line memory to `MAX_LINE_BYTES` so huge lines (such as base64
    /// images or `file-history-snapshot` records) never spike RSS.
    pub fn extract_title_from_transcript(path: &Path) -> Option<String> {
        let file = File::open(path).ok()?;
        let mut reader = BufReader::with_capacity(8 * 1024, file);
        let mut line_buf: Vec<u8> = Vec::with_capacity(4096);

        for _ in 0..MAX_TRANSCRIPT_SCAN_LINES {
            line_buf.clear();
            let Some(complete) = read_bounded_line(&mut reader, &mut line_buf, MAX_LINE_BYTES)
            else {
                break;
            };
            if !complete {
                // Oversized line (e.g. screenshot/snapshot) — skip without parsing.
                continue;
            }
            let Ok(line) = std::str::from_utf8(&line_buf) else {
                continue;
            };
            if !line.contains("\"user\"") {
                continue;
            }
            let Ok(val) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if val.get("type").and_then(|v| v.as_str()) != Some("user") {
                continue;
            }
            if val.get("isSidechain").and_then(|v| v.as_bool()) == Some(true) {
                continue;
            }
            if let Some(content) = val.pointer("/message/content") {
                if let Some(text) = extract_content_text(content) {
                    let trimmed = text.trim_start();
                    if is_synthetic_claude_user_message(trimmed) {
                        continue;
                    }
                    let clean = normalize_title(trimmed);
                    if !clean.is_empty() {
                        return Some(clean);
                    }
                }
            }
        }
        None
    }

    pub fn has_pid_session_file(&self, pid: i32) -> bool {
        self.pid_session_path(pid).is_file()
    }
}

/// Read up to `max_bytes` of a single `\n`-terminated line into `out`.
///
/// Returns `None` at EOF, `Some(true)` if the entire line fit within
/// `max_bytes`, or `Some(false)` if the line exceeded `max_bytes` (in which
/// case the remainder of the line is drained without allocating).
pub(crate) fn read_bounded_line<R: BufRead>(
    reader: &mut R,
    out: &mut Vec<u8>,
    max_bytes: usize,
) -> Option<bool> {
    let mut saw_any = false;
    let mut exceeded = false;

    loop {
        let buf = match reader.fill_buf() {
            Ok([]) => {
                return if saw_any { Some(!exceeded) } else { None };
            }
            Ok(b) => b,
            Err(_) => return None,
        };
        saw_any = true;

        if let Some(nl) = buf.iter().position(|&b| b == b'\n') {
            if !exceeded {
                if out.len() + nl <= max_bytes {
                    out.extend_from_slice(&buf[..nl]);
                } else {
                    exceeded = true;
                }
            }
            reader.consume(nl + 1);
            return Some(!exceeded);
        }

        let len = buf.len();
        if !exceeded {
            if out.len() + len <= max_bytes {
                out.extend_from_slice(buf);
            } else {
                exceeded = true;
            }
        }
        reader.consume(len);
    }
}

fn is_synthetic_claude_user_message(s: &str) -> bool {
    s.starts_with("<local-command-caveat>")
        || s.starts_with("<local-command-stdout>")
        || s.starts_with("<command-name>")
        || s.starts_with("<system-reminder>")
}

fn extract_content_text(content: &serde_json::Value) -> Option<String> {
    if let Some(s) = content.as_str() {
        return Some(s.to_string());
    }
    if let Some(arr) = content.as_array() {
        for item in arr {
            if item.get("type").and_then(|v| v.as_str()) == Some("text") {
                if let Some(t) = item.get("text").and_then(|v| v.as_str()) {
                    if !t.trim().is_empty() {
                        return Some(t.to_string());
                    }
                }
            }
        }
    }
    None
}

pub fn normalize_title(raw: &str) -> String {
    let single_line: String = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = single_line.chars();
    let truncated: String = chars.by_ref().take(MAX_TITLE_CHARS).collect();
    if chars.next().is_some() {
        let mut t: String = truncated.chars().take(MAX_TITLE_CHARS - 1).collect();
        t.push('…');
        t
    } else {
        truncated
    }
}

impl Adapter for ClaudeAdapter {
    fn tool(&self) -> Tool {
        Tool::Claude
    }

    fn matches_process(&self, p: &ProcInfo) -> bool {
        // Never match Desktop app helpers or Chrome native host.
        if let Some(exe) = &p.exe_path {
            let exe_str = exe.to_string_lossy();
            if exe_str.contains("Claude.app/Contents/") {
                return false;
            }
        }

        if p.argv.iter().skip(1).any(|a| {
            matches!(
                a.as_str(),
                "--chrome-native-host" | "-p" | "--print" | "doctor" | "update" | "upgrade"
            )
        }) {
            return false;
        }

        let argv0_is_claude = p
            .argv
            .first()
            .and_then(|a| Path::new(a).file_name())
            .and_then(|n| n.to_str())
            .is_some_and(|n| n == "claude");

        let exe_is_claude = p.exe_path.as_ref().is_some_and(|exe| {
            let s = exe.to_string_lossy();
            s.contains("/claude/versions/")
                || exe
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n == "claude")
        });

        argv0_is_claude || exe_is_claude
    }

    fn session_for_process(
        &self,
        p: &ProcInfo,
        _platform: &dyn Platform,
        _claimed_ids: &HashSet<String>,
    ) -> Result<Option<SessionRef>> {
        if let Some(pid_file) = self.read_pid_session(p.pid) {
            let transcript = self.expected_transcript_path(&pid_file.cwd, &pid_file.session_id);
            let transcript_str = Some(transcript.to_string_lossy().into_owned());
            let title_hint = Self::extract_title_from_transcript(&transcript).or(pid_file.name);
            return Ok(Some(SessionRef {
                tool: Tool::Claude,
                session_id: pid_file.session_id,
                cwd: pid_file.cwd,
                transcript: transcript_str,
                title_hint,
                best_guess: false,
            }));
        }

        // Fallback: check `--resume <id>` / `-r <id>` in argv.
        if let Some(session_id) = Self::extract_resume_arg(&p.argv) {
            if let Some(cwd_path) = &p.cwd {
                let cwd = cwd_path.to_string_lossy().into_owned();
                let transcript = self.expected_transcript_path(&cwd, &session_id);
                let title_hint = Self::extract_title_from_transcript(&transcript);
                return Ok(Some(SessionRef {
                    tool: Tool::Claude,
                    session_id,
                    cwd,
                    transcript: Some(transcript.to_string_lossy().into_owned()),
                    title_hint,
                    best_guess: false,
                }));
            }
        }

        Ok(None)
    }

    fn session_exists(&self, s: &Session, _tool_running: bool) -> bool {
        if let Some(transcript) = &s.transcript {
            if Path::new(transcript).is_file() {
                return true;
            }
        }
        let expected = self.expected_transcript_path(&s.cwd, &s.session_id);
        if expected.is_file() {
            return true;
        }
        if let Some(pid) = s.pid {
            if let Some(pid_file) = self.read_pid_session(pid as i32) {
                if pid_file.session_id == s.session_id {
                    return true;
                }
            }
        }
        false
    }

    fn title(&self, s: &Session, _tool_running: bool) -> Option<String> {
        if let Some(transcript) = &s.transcript {
            if let Some(t) = Self::extract_title_from_transcript(Path::new(transcript)) {
                return Some(t);
            }
        }
        let expected = self.expected_transcript_path(&s.cwd, &s.session_id);
        Self::extract_title_from_transcript(&expected)
    }

    fn resume_command(&self, s: &Session, cfg: &ToolConfig) -> String {
        expand_resume_template(&cfg.resume, &s.session_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TestHome(PathBuf);
    impl TestHome {
        fn new() -> Self {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "restowa-claude-test-{}-{}",
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

    #[test]
    fn encode_project_dir_matches_observed_claude_behavior() {
        assert_eq!(
            ClaudeAdapter::encode_project_dir(
                "/Users/dev/Developer/Personal/example.com"
            ),
            "-Users-dev-Developer-Personal-example-com"
        );
        assert_eq!(
            ClaudeAdapter::encode_project_dir(
                "/Users/dev/Developer/Side Projects/demo-app"
            ),
            "-Users-dev-Developer-Side-Projects-demo-app"
        );
    }

    #[test]
    fn matches_process_uses_argv_and_exe_and_excludes_helpers() {
        let home = TestHome::new();
        let adapter = ClaudeAdapter::new(AppPaths::from_home(home.0.clone()));

        // Auto-updated binary where proc_pidpath failed (exe_path = None), argv[0] = "claude"
        let auto_updated = ProcInfo {
            pid: 2849,
            ppid: 2017,
            start_time_us: 100,
            exe_path: None,
            argv: vec!["claude".into(), "--dangerously-skip-permissions".into()],
            cwd: Some(PathBuf::from("/tmp/proj")),
        };
        assert!(adapter.matches_process(&auto_updated));

        // Native versioned binary path
        let versioned = ProcInfo {
            pid: 43760,
            ppid: 2075,
            start_time_us: 200,
            exe_path: Some(PathBuf::from(
                "/Users/u/.local/share/claude/versions/2.1.328",
            )),
            argv: vec!["claude".into()],
            cwd: Some(PathBuf::from("/tmp/proj")),
        };
        assert!(adapter.matches_process(&versioned));

        // Chrome native host must be excluded
        let chrome_host = ProcInfo {
            pid: 80635,
            ppid: 80569,
            start_time_us: 300,
            exe_path: Some(PathBuf::from(
                "/Users/u/.local/share/claude/versions/2.1.328",
            )),
            argv: vec![
                "/Users/u/.local/bin/claude".into(),
                "--chrome-native-host".into(),
            ],
            cwd: Some(PathBuf::from("/Users/u/.claude/chrome")),
        };
        assert!(!adapter.matches_process(&chrome_host));

        // Claude Desktop app must be excluded
        let desktop = ProcInfo {
            pid: 53586,
            ppid: 1,
            start_time_us: 400,
            exe_path: Some(PathBuf::from(
                "/Applications/Claude.app/Contents/MacOS/Claude",
            )),
            argv: vec!["/Applications/Claude.app/Contents/MacOS/Claude".into()],
            cwd: None,
        };
        assert!(!adapter.matches_process(&desktop));

        // Non-interactive print mode must be excluded
        let print_mode = ProcInfo {
            pid: 999,
            ppid: 1,
            start_time_us: 500,
            exe_path: Some(PathBuf::from("/Users/u/.local/bin/claude")),
            argv: vec!["claude".into(), "-p".into(), "hello".into()],
            cwd: Some(PathBuf::from("/tmp")),
        };
        assert!(!adapter.matches_process(&print_mode));
    }

    #[test]
    fn resolves_session_from_pid_file_and_extracts_title_from_fixture() {
        let home = TestHome::new();
        let paths = AppPaths::from_home(home.0.clone());
        std::fs::create_dir_all(paths.claude_sessions_dir()).unwrap();
        let cwd = "/Users/dev/my-app";
        let sid = "5e45591c-0000-4000-8000-000000000001";

        std::fs::write(
            paths.claude_sessions_dir().join("3964.json"),
            format!(
                r#"{{"pid":3964,"sessionId":"{sid}","cwd":"{cwd}","kind":"interactive","entrypoint":"cli","name":"my-app-1"}}"#
            ),
        )
        .unwrap();

        let proj_dir = paths
            .claude_projects_dir()
            .join(ClaudeAdapter::encode_project_dir(cwd));
        std::fs::create_dir_all(&proj_dir).unwrap();
        std::fs::write(
            proj_dir.join(format!("{sid}.jsonl")),
            concat!(
                "{\"type\":\"mode\",\"mode\":\"normal\",\"sessionId\":\"5e45591c-0000-4000-8000-000000000001\"}\n",
                "{\"type\":\"user\",\"isSidechain\":false,\"message\":{\"role\":\"user\",\"content\":\"<local-command-caveat>The command output...</local-command-caveat>\"},\"cwd\":\"/Users/dev/my-app\"}\n",
                "{\"type\":\"user\",\"isSidechain\":false,\"message\":{\"role\":\"user\",\"content\":\"Add audit event schema\"},\"cwd\":\"/Users/dev/my-app\"}\n"
            ),
        )
        .unwrap();

        let adapter = ClaudeAdapter::new(paths);
        let proc_info = ProcInfo {
            pid: 3964,
            ppid: 1,
            start_time_us: 100,
            exe_path: None,
            argv: vec!["claude".into()],
            cwd: Some(PathBuf::from(cwd)),
        };

        let resolved = adapter
            .session_for_process(&proc_info, &EmptyPlatform, &HashSet::new())
            .unwrap()
            .unwrap();
        assert_eq!(resolved.session_id, sid);
        assert_eq!(resolved.cwd, cwd);
        assert_eq!(
            resolved.title_hint.as_deref(),
            Some("Add audit event schema")
        );
        assert!(!resolved.best_guess);
    }

    #[test]
    fn resolves_session_from_resume_argv_when_pid_file_missing() {
        let home = TestHome::new();
        let paths = AppPaths::from_home(home.0.clone());
        let adapter = ClaudeAdapter::new(paths);

        let proc_info = ProcInfo {
            pid: 2849,
            ppid: 1,
            start_time_us: 100,
            exe_path: None,
            argv: vec![
                "claude".into(),
                "--dangerously-skip-permissions".into(),
                "--resume".into(),
                "d9819a06-0000-4000-8000-000000000006".into(),
            ],
            cwd: Some(PathBuf::from("/Users/dev/sample-api")),
        };

        let resolved = adapter
            .session_for_process(&proc_info, &EmptyPlatform, &HashSet::new())
            .unwrap()
            .unwrap();
        assert_eq!(resolved.session_id, "d9819a06-0000-4000-8000-000000000006");
        assert_eq!(resolved.cwd, "/Users/dev/sample-api");
    }
}
