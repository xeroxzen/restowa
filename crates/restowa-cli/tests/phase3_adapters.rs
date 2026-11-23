use restowa_core::adapters::antigravity::AntigravityAdapter;
use restowa_core::adapters::codex::CodexAdapter;
use restowa_core::adapters::opencode::OpenCodeAdapter;
use restowa_core::adapters::Adapter;
use restowa_core::config::Config;
use restowa_core::doctor::{run_doctor, CheckStatus};
use restowa_core::error::Result;
use restowa_core::model::{ProcInfo, Session, SessionSource, SessionStatus, Tool};
use restowa_core::paths::AppPaths;
use restowa_core::platform::Platform;
use restowa_core::registry::Registry;
use restowa_core::scanner::run_scan;
use rusqlite::Connection;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct IsolatedHome {
    root: PathBuf,
    home: PathBuf,
}

impl IsolatedHome {
    fn new(tag: &str) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("restowa-p3-{tag}-{}-{n}", std::process::id()));
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

fn fixtures_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures")
        .canonicalize()
        .unwrap()
}

struct MockPlatform {
    boot_id: String,
    procs: Vec<ProcInfo>,
    open_files: HashMap<i32, Vec<PathBuf>>,
}

impl Platform for MockPlatform {
    fn boot_id(&self) -> Result<String> {
        Ok(self.boot_id.clone())
    }
    fn list_processes(&self) -> Result<Vec<ProcInfo>> {
        Ok(self.procs.clone())
    }
    fn process_info(&self, pid: i32) -> Option<ProcInfo> {
        self.procs.iter().find(|p| p.pid == pid).cloned()
    }
    fn process_open_files(&self, pid: i32) -> Vec<PathBuf> {
        self.open_files.get(&pid).cloned().unwrap_or_default()
    }
    fn parent_pid(&self) -> i32 {
        1
    }
}

#[test]
fn phase3_codex_opencode_and_antigravity_fixture_scan_and_resume_commands() {
    let env = IsolatedHome::new("all-adapters");
    let paths = AppPaths::from_home(env.home.clone());
    let fix = fixtures_root();
    let cfg = Config::default();

    // 1. Seed 2 Codex rollout fixtures in the SAME folder (/Users/dev/sample-lib)
    let codex_day = paths.codex_sessions_dir().join("2026/11/22");
    std::fs::create_dir_all(&codex_day).unwrap();
    let r1_name = "rollout-2026-11-22T10-00-00-019a0000-0000-7000-8000-0000000000a1.jsonl";
    let r2_name = "rollout-2026-11-22T10-05-00-019a0000-0000-7000-8000-0000000000b2.jsonl";
    let r1_dst = codex_day.join(r1_name);
    let r2_dst = codex_day.join(r2_name);
    std::fs::copy(fix.join("codex").join(r1_name), &r1_dst).unwrap();
    std::fs::copy(fix.join("codex").join(r2_name), &r2_dst).unwrap();

    // 2. Seed OpenCode opencode.db from SQL fixture
    AppPaths::ensure_parent(&paths.opencode_db()).unwrap();
    {
        let conn = Connection::open(paths.opencode_db()).unwrap();
        let sql = std::fs::read_to_string(fix.join("opencode/schema_and_seed.sql")).unwrap();
        conn.execute_batch(&sql).unwrap();
    }

    // 3. Seed Antigravity conversation_summaries.db, conversation .db, and last_conversations.json
    std::fs::create_dir_all(paths.antigravity_conversations_dir()).unwrap();
    std::fs::create_dir_all(paths.antigravity_last_conversations().parent().unwrap()).unwrap();
    let agy_id = "8c257602-0000-4000-8000-000000000002";
    std::fs::write(
        paths
            .antigravity_conversations_dir()
            .join(format!("{agy_id}.db")),
        b"placeholder",
    )
    .unwrap();
    std::fs::copy(
        fix.join("antigravity/last_conversations.json"),
        paths.antigravity_last_conversations(),
    )
    .unwrap();
    {
        let conn = Connection::open(paths.antigravity_summaries_db()).unwrap();
        let sql = std::fs::read_to_string(fix.join("antigravity/summaries_seed.sql")).unwrap();
        conn.execute_batch(&sql).unwrap();
    }

    let mut open_files = HashMap::new();
    open_files.insert(1001, vec![r1_dst]);
    open_files.insert(1002, vec![r2_dst]);
    open_files.insert(
        3001,
        vec![paths
            .antigravity_cli_dir()
            .join(format!("presence/{agy_id}.lock"))],
    );

    let platform = MockPlatform {
        boot_id: "boot-p3".into(),
        procs: vec![
            // Two Codex processes in the SAME folder, each with its own open rollout FD
            ProcInfo {
                pid: 1001,
                ppid: 1,
                start_time_us: 100,
                exe_path: Some(PathBuf::from("/opt/homebrew/bin/codex")),
                argv: vec!["codex".into()],
                cwd: Some(PathBuf::from("/Users/dev/sample-lib")),
            },
            ProcInfo {
                pid: 1002,
                ppid: 1,
                start_time_us: 200,
                exe_path: Some(PathBuf::from("/opt/homebrew/bin/codex")),
                argv: vec!["codex".into()],
                cwd: Some(PathBuf::from("/Users/dev/sample-lib")),
            },
            // OpenCode in a multi-session folder without --session -> best_guess = true
            ProcInfo {
                pid: 2001,
                ppid: 1,
                start_time_us: 300,
                exe_path: Some(PathBuf::from("/Users/dev/.opencode/bin/opencode")),
                argv: vec!["opencode".into()],
                cwd: Some(PathBuf::from("/Users/dev/shared-repo")),
            },
            // OpenCode in a single-session folder -> exact (best_guess = false)
            ProcInfo {
                pid: 2002,
                ppid: 1,
                start_time_us: 350,
                exe_path: Some(PathBuf::from("/Users/dev/.opencode/bin/opencode")),
                argv: vec!["opencode".into()],
                cwd: Some(PathBuf::from("/Users/dev/restowa")),
            },
            // Antigravity CLI with open presence lock FD -> exact (best_guess = false)
            ProcInfo {
                pid: 3001,
                ppid: 1,
                start_time_us: 400,
                exe_path: Some(PathBuf::from("/Users/dev/.local/bin/agy.1794331593.old")),
                argv: vec!["agy".into()],
                cwd: Some(PathBuf::from("/Users/dev/Security Research/demo")),
            },
            // Background daemons that MUST be ignored
            ProcInfo {
                pid: 9001,
                ppid: 1,
                start_time_us: 500,
                exe_path: Some(PathBuf::from("/Users/dev/.local/bin/agy")),
                argv: vec!["agy".into(), "acp".into()],
                cwd: Some(PathBuf::from("/Users/dev/restowa")),
            },
            ProcInfo {
                pid: 9002,
                ppid: 1,
                start_time_us: 501,
                exe_path: Some(PathBuf::from("/opt/homebrew/bin/codex")),
                argv: vec!["codex".into(), "app-server".into()],
                cwd: Some(PathBuf::from("/")),
            },
        ],
        open_files,
    };

    let scan = run_scan(&paths, &platform).unwrap();
    assert_eq!(scan.live_found, 5);

    let reg = Registry::open(&paths.registry_db).unwrap();

    // Verify both Codex sessions in the exact same folder resolved distinctly & exactly
    let c1 = reg
        .get_by_key("codex:019a0000-0000-7000-8000-0000000000a1")
        .unwrap()
        .unwrap();
    let c2 = reg
        .get_by_key("codex:019a0000-0000-7000-8000-0000000000b2")
        .unwrap()
        .unwrap();
    assert_eq!(c1.pid, Some(1001));
    assert_eq!(c2.pid, Some(1002));
    assert!(!c1.best_guess);
    assert!(!c2.best_guess);
    assert_eq!(c1.title.as_deref(), Some("Port the TCP handshake"));
    assert_eq!(c2.title.as_deref(), Some("Write packet codec unit tests"));

    // Verify OpenCode ambiguous vs exact
    let oc_ambig = reg.get_by_key("opencode:ses_ambig_new").unwrap().unwrap();
    assert!(
        oc_ambig.best_guess,
        "ambiguous opencode match must be flagged"
    );
    assert_eq!(
        oc_ambig.title.as_deref(),
        Some("Audit SQLite WAL read-only flags")
    );

    let oc_exact = reg.get_by_key("opencode:ses_single_01").unwrap().unwrap();
    assert!(!oc_exact.best_guess);
    assert_eq!(oc_exact.title.as_deref(), Some("Scaffold adapters"));

    // Verify Antigravity exact match
    let agy = reg
        .get_by_key(&format!("antigravity:{agy_id}"))
        .unwrap()
        .unwrap();
    assert!(!agy.best_guess);
    assert_eq!(agy.cwd, "/Users/dev/Security Research/demo");
    assert_eq!(agy.title.as_deref(), Some("Inspect Security Research Repo"));

    // Verify resume commands from default config templates
    let codex_adapter = CodexAdapter::new(paths.clone());
    let opencode_adapter = OpenCodeAdapter::new(paths.clone());
    let agy_adapter = AntigravityAdapter::new(paths.clone());

    assert_eq!(
        codex_adapter.resume_command(&c1, cfg.tool_config(Tool::Codex)),
        "codex resume '019a0000-0000-7000-8000-0000000000a1'"
    );
    assert_eq!(
        opencode_adapter.resume_command(&oc_exact, cfg.tool_config(Tool::Opencode)),
        "opencode --session 'ses_single_01'"
    );
    assert_eq!(
        agy_adapter.resume_command(&agy, cfg.tool_config(Tool::Antigravity)),
        format!("agy --conversation '{agy_id}'")
    );

    // Verify doctor schema checks report OK for seeded DBs
    let doc = run_doctor(&paths, &platform);
    for schema_name in ["schema:opencode", "schema:antigravity"] {
        let chk = doc
            .checks
            .iter()
            .find(|c| c.name == schema_name)
            .expect(schema_name);
        assert_eq!(chk.status, CheckStatus::Ok);
    }

    // Verify session_exists returns false for nonexistent IDs
    let fake_agy = Session {
        key: "antigravity:does-not-exist".into(),
        tool: Tool::Antigravity,
        session_id: "does-not-exist".into(),
        cwd: "/tmp".into(),
        title: None,
        transcript: None,
        pid: None,
        pid_start_us: None,
        status: SessionStatus::Closed,
        source: SessionSource::Scan,
        first_seen: 1,
        last_seen_live: None,
        last_boot_id: None,
        ended_by_user: false,
        best_guess: false,
        pinned: false,
        hidden: false,
    };
    assert!(!agy_adapter.session_exists(&fake_agy, false));
}

#[test]
fn restore_dry_run_displays_best_guess_and_list_pinned_filters_pinned() {
    let env = IsolatedHome::new("dry-run-best-guess");
    let paths = AppPaths::from_home(env.home.clone());
    let proj = env.root.join("proj");
    std::fs::create_dir_all(&proj).unwrap();

    let mut reg = Registry::open(&paths.registry_db).unwrap();
    reg.upsert_scan_live(&restowa_core::registry::ScanSessionUpsert {
        tool: Tool::Opencode,
        session_id: "ses_guess_1".into(),
        cwd: proj.to_string_lossy().into_owned(),
        transcript: None,
        title_hint: Some("Ambiguous OpenCode".into()),
        pid: 99991,
        pid_start_us: 100,
        boot_id: "boot-old".into(),
        best_guess: true,
        now: 1000,
    })
    .unwrap();
    reg.upsert_scan_live(&restowa_core::registry::ScanSessionUpsert {
        tool: Tool::Codex,
        session_id: "019a0000-0000-7000-8000-0000000000cc".into(),
        cwd: proj.to_string_lossy().into_owned(),
        transcript: None,
        title_hint: Some("Exact Codex".into()),
        pid: 99992,
        pid_start_us: 100,
        boot_id: "boot-old".into(),
        best_guess: false,
        now: 1001,
    })
    .unwrap();
    reg.set_pinned("codex:019a0000-0000-7000-8000-0000000000cc", true)
        .unwrap();
    drop(reg);

    let bin = env!("CARGO_BIN_EXE_restowa");

    // 1. Human-readable `restore --dry-run` shows `(best guess)` next to the best-guess item only
    let dry = std::process::Command::new(bin)
        .args([
            "restore",
            "opencode:ses_guess_1",
            "codex:019a0000-0000-7000-8000-0000000000cc",
            "--dry-run",
        ])
        .env("HOME", &env.home)
        .output()
        .unwrap();
    assert!(dry.status.success());
    let stdout = String::from_utf8_lossy(&dry.stdout);
    let guess_line = stdout
        .lines()
        .find(|l| l.contains("opencode:ses_guess_1"))
        .expect("opencode line present");
    let exact_line = stdout
        .lines()
        .find(|l| l.contains("codex:019a0000-0000-7000-8000-0000000000cc"))
        .expect("codex line present");
    assert!(
        guess_line.contains("(best guess)"),
        "expected `(best guess)` in dry-run line: {guess_line}"
    );
    assert!(
        !exact_line.contains("(best guess)"),
        "unexpected `(best guess)` on exact line: {exact_line}"
    );

    // 2. JSON `restore --dry-run --json` includes `"best_guess"` boolean
    let dry_json = std::process::Command::new(bin)
        .args([
            "restore",
            "opencode:ses_guess_1",
            "codex:019a0000-0000-7000-8000-0000000000cc",
            "--dry-run",
            "--json",
        ])
        .env("HOME", &env.home)
        .output()
        .unwrap();
    assert!(dry_json.status.success());
    let report: serde_json::Value = serde_json::from_slice(&dry_json.stdout).unwrap();
    let items = report["restored"].as_array().unwrap();
    let oc_item = items
        .iter()
        .find(|i| i["key"] == "opencode:ses_guess_1")
        .unwrap();
    let cx_item = items
        .iter()
        .find(|i| i["key"] == "codex:019a0000-0000-7000-8000-0000000000cc")
        .unwrap();
    assert_eq!(oc_item["best_guess"], true);
    assert_eq!(cx_item["best_guess"], false);

    // 3. `list --pinned --json` returns only the pinned session
    let pinned_out = std::process::Command::new(bin)
        .args(["list", "--pinned", "--json"])
        .env("HOME", &env.home)
        .output()
        .unwrap();
    assert!(pinned_out.status.success());
    let pinned: serde_json::Value = serde_json::from_slice(&pinned_out.stdout).unwrap();
    let arr = pinned.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["key"], "codex:019a0000-0000-7000-8000-0000000000cc");
}
