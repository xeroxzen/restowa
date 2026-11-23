use restowa_core::adapters::claude::ClaudeAdapter;
use restowa_core::adapters::Adapter;
use restowa_core::model::ProcInfo;
use restowa_core::paths::AppPaths;
use restowa_core::platform::current_platform;
use serde_json::Value;
use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct IsolatedHome {
    root: PathBuf,
    home: PathBuf,
}

impl IsolatedHome {
    fn new(tag: &str) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("restowa-e2e-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let home = root.join("home");
        std::fs::create_dir_all(&home).unwrap();
        Self { root, home }
    }
}

impl Drop for IsolatedHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/claude")
        .canonicalize()
        .unwrap()
}

fn restowa_bin() -> &'static str {
    env!("CARGO_BIN_EXE_restowa")
}

#[test]
fn claude_adapter_reads_fixture_pid_file_and_transcript() {
    let env = IsolatedHome::new("fixture-adapter");
    let paths = AppPaths::from_home(env.home.clone());
    let fix = fixtures_dir();

    std::fs::create_dir_all(paths.claude_sessions_dir()).unwrap();
    std::fs::copy(
        fix.join("pid_session.json"),
        paths.claude_sessions_dir().join("3964.json"),
    )
    .unwrap();

    let encoded = ClaudeAdapter::encode_project_dir("/Users/dev/Projects/sample-app");
    let proj_dir = paths.claude_projects_dir().join(encoded);
    std::fs::create_dir_all(&proj_dir).unwrap();
    std::fs::copy(
        fix.join("transcript.jsonl"),
        proj_dir.join("5e45591c-0000-4000-8000-000000000001.jsonl"),
    )
    .unwrap();

    let adapter = ClaudeAdapter::new(paths);
    let proc_info = ProcInfo {
        pid: 3964,
        ppid: 1,
        start_time_us: 123_456,
        exe_path: None,
        argv: vec!["claude".into()],
        cwd: Some(PathBuf::from("/Users/dev/Projects/sample-app")),
    };

    assert!(adapter.matches_process(&proc_info));
    let platform = current_platform();
    let resolved = adapter
        .session_for_process(&proc_info, &platform, &HashSet::new())
        .unwrap()
        .unwrap();
    assert_eq!(resolved.session_id, "5e45591c-0000-4000-8000-000000000001");
    assert_eq!(resolved.cwd, "/Users/dev/Projects/sample-app");
    assert_eq!(
        resolved.title_hint.as_deref(),
        Some("Add audit event schema and wire up migration tests")
    );
}

#[test]
fn phase1_cli_install_hooks_three_sessions_list_dry_run_and_doctor() {
    let env = IsolatedHome::new("phase1-cli");
    let fix = fixtures_dir();

    // 1. Seed existing Claude settings.json from fixture
    let claude_dir = env.home.join(".claude");
    std::fs::create_dir_all(&claude_dir).unwrap();
    let settings_path = claude_dir.join("settings.json");
    std::fs::copy(fix.join("settings_existing.json"), &settings_path).unwrap();

    // 2. Run `restowa install --hooks --bin restowa`
    let install_out = Command::new(restowa_bin())
        .args(["install", "--hooks", "--bin", "restowa", "--json"])
        .env("HOME", &env.home)
        .output()
        .unwrap();
    assert!(
        install_out.status.success(),
        "install failed: {}",
        String::from_utf8_lossy(&install_out.stderr)
    );
    assert!(claude_dir.join("settings.json.restowa.bak").is_file());

    let merged: Value =
        serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
    assert_eq!(merged["model"], "opus");
    assert_eq!(merged["hooks"]["SessionStart"].as_array().unwrap().len(), 2);
    assert_eq!(merged["hooks"]["SessionEnd"].as_array().unwrap().len(), 1);
    assert_eq!(merged["hooks"]["PreToolUse"].as_array().unwrap().len(), 1);

    // 3. Create 2 project folders and record 3 Claude sessions via hook stdin
    let proj_a = env.root.join("project alpha");
    let proj_b = env.root.join("project'beta");
    std::fs::create_dir_all(&proj_a).unwrap();
    std::fs::create_dir_all(&proj_b).unwrap();

    let transcript_copy = env.root.join("transcript.jsonl");
    std::fs::copy(fix.join("transcript.jsonl"), &transcript_copy).unwrap();

    let sessions = [
        ("11111111-aaaa-4000-8000-000000000001", &proj_a),
        ("22222222-bbbb-4000-8000-000000000002", &proj_a),
        ("33333333-cccc-4000-8000-000000000003", &proj_b),
    ];

    for (sid, folder) in sessions {
        let payload = serde_json::json!({
            "session_id": sid,
            "hook_event_name": "SessionStart",
            "source": "startup",
            "cwd": folder.to_string_lossy(),
            "transcript_path": transcript_copy.to_string_lossy(),
        })
        .to_string();

        let t0 = Instant::now();
        let mut child = Command::new(restowa_bin())
            .args(["hook", "claude", "session-start"])
            .env("HOME", &env.home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        let elapsed = t0.elapsed();

        assert!(out.status.success());
        // Hook must NEVER write to stdout
        assert!(
            out.stdout.is_empty(),
            "hook wrote to stdout: {}",
            String::from_utf8_lossy(&out.stdout)
        );
        // Generous debug-build sanity check; release benchmark verified separately < 50ms
        assert!(
            elapsed.as_millis() < 1000,
            "hook took too long: {elapsed:?}"
        );
    }

    // 4. Verify `restowa list --all --json` shows all 3 exact hook-recorded session IDs
    // (Filter by source == "hook" so live `--resume` processes on the host machine
    // do not make this isolated HOME test flaky.)
    let list_out = Command::new(restowa_bin())
        .args(["list", "--all", "--json"])
        .env("HOME", &env.home)
        .output()
        .unwrap();
    assert!(list_out.status.success());
    let listed: Value = serde_json::from_slice(&list_out.stdout).unwrap();
    let hook_rows: Vec<&Value> = listed
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["source"].as_str() == Some("hook"))
        .collect();
    assert_eq!(
        hook_rows.len(),
        3,
        "expected 3 hook sessions, got: {listed}"
    );
    let ids: Vec<&str> = hook_rows
        .iter()
        .map(|s| s["session_id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&"11111111-aaaa-4000-8000-000000000001"));
    assert!(ids.contains(&"22222222-bbbb-4000-8000-000000000002"));
    assert!(ids.contains(&"33333333-cccc-4000-8000-000000000003"));
    assert!(hook_rows.iter().all(|s| {
        s["title"].as_str() == Some("Add audit event schema and wire up migration tests")
    }));

    // 5. Verify `restowa restore --dry-run --json` prints all 3 escaped commands
    let dry_out = Command::new(restowa_bin())
        .args([
            "restore",
            "11111111",
            "22222222",
            "33333333",
            "--dry-run",
            "--json",
        ])
        .env("HOME", &env.home)
        .output()
        .unwrap();
    assert!(dry_out.status.success());
    let report: Value = serde_json::from_slice(&dry_out.stdout).unwrap();
    let restored = report["restored"].as_array().unwrap();
    assert_eq!(restored.len(), 3);
    assert!(restored.iter().any(|item| {
        item["shell_command"]
            .as_str()
            .unwrap()
            .contains("project'\\''beta")
    }));

    // 6. Verify `restowa doctor --json` warns when ~/.claude/sessions is missing
    let doc_out = Command::new(restowa_bin())
        .args(["doctor", "--json"])
        .env("HOME", &env.home)
        .output()
        .unwrap();
    assert!(doc_out.status.success());
    let doc: Value = serde_json::from_slice(&doc_out.stdout).unwrap();
    let checks = doc["checks"].as_array().unwrap();
    let pid_check = checks
        .iter()
        .find(|c| c["name"] == "claude:pid-files")
        .unwrap();
    assert_eq!(pid_check["status"], "warn");
    let hook_check = checks.iter().find(|c| c["name"] == "claude:hooks").unwrap();
    assert_eq!(hook_check["status"], "ok");
}
