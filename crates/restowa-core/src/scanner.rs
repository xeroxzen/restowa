use crate::adapters::{adapters_for_paths, owning_tool_of_process, Adapter};
use crate::error::Result;
use crate::hooks::replay_spool;
use crate::model::{ProcInfo, Session, SessionRef, SessionSource, SessionStatus, Tool};
use crate::paths::AppPaths;
use crate::platform::Platform;
use crate::registry::{Registry, ScanSessionUpsert};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone)]
pub struct DiscoveredLiveSession {
    pub session_ref: SessionRef,
    pub pid: i32,
    pub pid_start_us: i64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ScanReport {
    pub boot_id: String,
    pub spool_imported: usize,
    pub processes_inspected: usize,
    pub live_found: usize,
    pub marked_closed: usize,
    pub marked_missing: usize,
}

pub struct LiveDiscoverySnapshot {
    pub processes_inspected: usize,
    pub proc_starts: HashMap<i32, i64>,
    pub running_tools: HashSet<Tool>,
    pub discovered: Vec<DiscoveredLiveSession>,
}

/// Discover all currently live sessions from running processes without writing
/// to the registry. Streams processes one-by-one to keep peak RSS well below
/// the 10 MB scan budget.
pub fn discover_live_sessions(
    paths: &AppPaths,
    platform: &dyn Platform,
) -> Result<LiveDiscoverySnapshot> {
    let adapters = adapters_for_paths(paths);
    let mut proc_starts = HashMap::new();
    let mut running_tools = HashSet::new();
    let mut matched_procs: Vec<(usize, ProcInfo)> = Vec::new();

    let processes_inspected = platform.for_each_process(&mut |proc_info| {
        proc_starts.insert(proc_info.pid, proc_info.start_time_us);
        if let Some(tool) = owning_tool_of_process(proc_info) {
            running_tools.insert(tool);
        }
        for (idx, adapter) in adapters.iter().enumerate() {
            if adapter.matches_process(proc_info) {
                running_tools.insert(adapter.tool());
                matched_procs.push((idx, proc_info.clone()));
                break;
            }
        }
    })?;

    // Resolve newest processes first so fallback matching pairs the newest
    // process with the newest unclaimed session in a shared folder.
    matched_procs.sort_by(|a, b| {
        b.1.start_time_us
            .cmp(&a.1.start_time_us)
            .then_with(|| b.1.pid.cmp(&a.1.pid))
    });

    let mut claimed_by_tool: HashMap<Tool, HashSet<String>> = HashMap::new();
    let mut discovered = Vec::with_capacity(matched_procs.len());
    for (idx, proc_info) in &matched_procs {
        let adapter = &adapters[*idx];
        let tool = adapter.tool();
        let claimed = claimed_by_tool.entry(tool).or_default();
        match adapter.session_for_process(proc_info, platform, claimed) {
            Ok(Some(session_ref)) => {
                claimed.insert(session_ref.session_id.clone());
                discovered.push(DiscoveredLiveSession {
                    session_ref,
                    pid: proc_info.pid,
                    pid_start_us: proc_info.start_time_us,
                });
            }
            Ok(None) => {}
            Err(err) => {
                log::warn!(
                    "adapter {} failed resolving pid {}: {}",
                    tool,
                    proc_info.pid,
                    err
                );
            }
        }
    }

    // If two live processes mapped to the same (tool, session_id) via fallback,
    // flag them as best_guess so ambiguity is never hidden silently.
    let mut key_counts: HashMap<String, usize> = HashMap::new();
    for item in &discovered {
        let key = Session::make_key(item.session_ref.tool, &item.session_ref.session_id);
        *key_counts.entry(key).or_insert(0) += 1;
    }
    for item in &mut discovered {
        let key = Session::make_key(item.session_ref.tool, &item.session_ref.session_id);
        if key_counts.get(&key).copied().unwrap_or(0) > 1 {
            item.session_ref.best_guess = true;
        }
    }

    Ok(LiveDiscoverySnapshot {
        processes_inspected,
        proc_starts,
        running_tools,
        discovered,
    })
}

/// Execute one complete stateless scan pass and persist updates to SQLite.
pub fn run_scan(paths: &AppPaths, platform: &dyn Platform) -> Result<ScanReport> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let boot_id = platform.boot_id()?;
    let mut registry = Registry::open(&paths.registry_db)?;
    registry.record_boot_scan(&boot_id, now)?;
    let spool_imported = replay_spool(paths, &mut registry)?;

    let snapshot = discover_live_sessions(paths, platform)?;
    let proc_starts = snapshot.proc_starts;
    let running_tools = snapshot.running_tools;
    let discovered = snapshot.discovered;
    let processes_inspected = snapshot.processes_inspected;

    let mut live_keys = HashSet::new();
    for item in &discovered {
        let key = Session::make_key(item.session_ref.tool, &item.session_ref.session_id);
        live_keys.insert(key);
        registry.upsert_scan_live(&ScanSessionUpsert {
            tool: item.session_ref.tool,
            session_id: item.session_ref.session_id.clone(),
            cwd: item.session_ref.cwd.clone(),
            transcript: item.session_ref.transcript.clone(),
            title_hint: item.session_ref.title_hint.clone(),
            pid: item.pid as i64,
            pid_start_us: item.pid_start_us,
            boot_id: boot_id.clone(),
            best_guess: item.session_ref.best_guess,
            now,
        })?;
    }

    let adapters = adapters_for_paths(paths);
    let all_sessions = registry.list_all(true)?;
    let mut marked_closed = 0usize;
    let mut marked_missing = 0usize;

    for session in &all_sessions {
        let mut is_currently_live = live_keys.contains(&session.key);

        // If a hook recorded a live PID whose PID file wasn't matched, check
        // whether that exact (pid, pid_start_us) is still alive without PID reuse.
        if !is_currently_live && session.status == SessionStatus::Live {
            if let Some(pid) = session.pid {
                if let Some(&actual_start) = proc_starts.get(&(pid as i32)) {
                    let same_process = session
                        .pid_start_us
                        .is_none_or(|recorded| recorded == actual_start);
                    if same_process {
                        is_currently_live = true;
                        // Keep live heartbeat updated with current boot ID.
                        registry.upsert_scan_live(&ScanSessionUpsert {
                            tool: session.tool,
                            session_id: session.session_id.clone(),
                            cwd: session.cwd.clone(),
                            transcript: session.transcript.clone(),
                            title_hint: session.title.clone(),
                            pid,
                            pid_start_us: actual_start,
                            boot_id: boot_id.clone(),
                            best_guess: session.best_guess,
                            now,
                        })?;
                    }
                }
            }
        }

        if session.status == SessionStatus::Live && !is_currently_live {
            registry.mark_closed(&session.key)?;
            marked_closed += 1;
        }

        if !is_currently_live && session.status != SessionStatus::Missing {
            if let Some(adapter) = find_adapter(&adapters, session.tool) {
                if !adapter.session_exists(session, running_tools.contains(&session.tool)) {
                    registry.mark_missing(&session.key)?;
                    marked_missing += 1;
                }
            }
        }
    }

    Ok(ScanReport {
        boot_id,
        spool_imported,
        processes_inspected,
        live_found: discovered.len(),
        marked_closed,
        marked_missing,
    })
}

/// Merge registry rows with live process discovery (read-only view).
///
/// Ensures `restowa list` shows currently running Claude Code sessions via
/// `~/.claude/sessions/<pid>.json` even before hooks or launchd are installed,
/// while preserving hook metadata (`source = hook`, user titles, pins) when
/// hooks are active.
pub fn load_merged_sessions(
    paths: &AppPaths,
    platform: &dyn Platform,
    include_hidden: bool,
) -> Result<Vec<Session>> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let boot_id = platform.boot_id().ok();

    // Hidden rows are loaded too: a forgotten session that is still running
    // would otherwise be rediscovered below as a new, visible session.
    let db_sessions = match Registry::open_read_only(&paths.registry_db)? {
        Some(reg) => reg.list_all(true)?,
        None => Vec::new(),
    };

    let snapshot = discover_live_sessions(paths, platform)?;
    let proc_starts = snapshot.proc_starts;
    let running_tools = snapshot.running_tools;
    let discovered = snapshot.discovered;

    let adapters = adapters_for_paths(paths);
    let mut by_key: HashMap<String, Session> = HashMap::new();

    for mut s in db_sessions {
        // Reconcile stale "live" DB rows in memory if the PID is gone/reused.
        if s.status == SessionStatus::Live {
            let still_alive = s.pid.is_some_and(|pid| {
                proc_starts.get(&(pid as i32)).is_some_and(|&actual_start| {
                    s.pid_start_us
                        .is_none_or(|recorded| recorded == actual_start)
                })
            });
            if !still_alive {
                s.status = SessionStatus::Closed;
                s.pid = None;
                s.pid_start_us = None;
            }
        }
        if s.title.is_none() {
            if let Some(adapter) = find_adapter(&adapters, s.tool) {
                s.title = adapter.title(&s, running_tools.contains(&s.tool));
            }
        }
        by_key.insert(s.key.clone(), s);
    }

    for live in discovered {
        let key = Session::make_key(live.session_ref.tool, &live.session_ref.session_id);
        if let Some(existing) = by_key.get_mut(&key) {
            existing.status = SessionStatus::Live;
            existing.pid = Some(live.pid as i64);
            existing.pid_start_us = Some(live.pid_start_us);
            existing.last_seen_live = Some(now);
            existing.ended_by_user = false;
            if existing.source != SessionSource::Hook {
                existing.best_guess = live.session_ref.best_guess;
            }
            if existing.title.is_none() {
                existing.title = live.session_ref.title_hint;
            }
            if existing.transcript.is_none() {
                existing.transcript = live.session_ref.transcript;
            }
        } else {
            by_key.insert(
                key.clone(),
                Session {
                    key,
                    tool: live.session_ref.tool,
                    session_id: live.session_ref.session_id,
                    cwd: live.session_ref.cwd,
                    title: live.session_ref.title_hint,
                    transcript: live.session_ref.transcript,
                    pid: Some(live.pid as i64),
                    pid_start_us: Some(live.pid_start_us),
                    status: SessionStatus::Live,
                    source: SessionSource::Scan,
                    first_seen: now,
                    last_seen_live: Some(now),
                    last_boot_id: boot_id.clone(),
                    ended_by_user: false,
                    best_guess: live.session_ref.best_guess,
                    pinned: false,
                    hidden: false,
                },
            );
        }
    }

    let mut sessions: Vec<Session> = by_key
        .into_values()
        .filter(|s| include_hidden || !s.hidden)
        .collect();
    sessions.sort_by(|a, b| {
        let a_ts = a.last_seen_live.unwrap_or(a.first_seen);
        let b_ts = b.last_seen_live.unwrap_or(b.first_seen);
        b_ts.cmp(&a_ts).then_with(|| a.key.cmp(&b.key))
    });
    Ok(sessions)
}

fn find_adapter(adapters: &[Box<dyn Adapter>], tool: crate::model::Tool) -> Option<&dyn Adapter> {
    adapters
        .iter()
        .find(|a| a.tool() == tool)
        .map(|a| a.as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ProcInfo, Tool};
    use crate::registry::HookSessionUpsert;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TestHome(PathBuf);
    impl TestHome {
        fn new() -> Self {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "restowa-scan-test-{}-{}",
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
        boot_id: String,
        procs: Vec<ProcInfo>,
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
        fn process_open_files(&self, _pid: i32) -> Vec<PathBuf> {
            Vec::new()
        }
        fn parent_pid(&self) -> i32 {
            1
        }
    }

    #[test]
    fn forgotten_live_session_stays_hidden_in_merged_list() {
        let home = TestHome::new();
        let paths = AppPaths::from_home(home.0.clone());
        std::fs::create_dir_all(paths.claude_sessions_dir()).unwrap();
        let sid = "forget-me-live";
        std::fs::write(
            paths.claude_sessions_dir().join("5150.json"),
            format!(r#"{{"pid":5150,"sessionId":"{sid}","cwd":"/tmp/app","kind":"interactive"}}"#),
        )
        .unwrap();
        let platform = MockPlatform {
            boot_id: "boot-1".into(),
            procs: vec![ProcInfo {
                pid: 5150,
                ppid: 1,
                start_time_us: 100,
                exe_path: None,
                argv: vec!["claude".into()],
                cwd: Some(PathBuf::from("/tmp/app")),
            }],
        };
        run_scan(&paths, &platform).unwrap();
        let key = format!("claude:{sid}");
        let listed = |hidden: bool| {
            load_merged_sessions(&paths, &platform, hidden)
                .unwrap()
                .into_iter()
                .any(|s| s.key == key)
        };
        assert!(listed(false));

        let mut reg = Registry::open(&paths.registry_db).unwrap();
        reg.set_hidden(&key, true).unwrap();
        drop(reg);

        assert!(!listed(false));
        assert!(listed(true));
    }

    #[test]
    fn crash_marks_session_closed_and_keeps_it_in_last_boot_set() {
        let home = TestHome::new();
        let paths = AppPaths::from_home(home.0.clone());
        let transcript = home.0.join("crashed.jsonl");
        std::fs::write(&transcript, "{\"type\":\"mode\"}\n").unwrap();

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(1000);
        let mut reg = Registry::open(&paths.registry_db).unwrap();
        reg.upsert_hook_start(&HookSessionUpsert {
            tool: Tool::Claude,
            session_id: "crashed-session-1".into(),
            cwd: home.0.to_string_lossy().into_owned(),
            transcript: Some(transcript.to_string_lossy().into_owned()),
            pid: Some(7777),
            pid_start_us: Some(1_000_000),
            boot_id: "boot-1".into(),
            now,
        })
        .unwrap();
        drop(reg);

        // Terminal force-quit: PID 7777 vanished with no SessionEnd hook
        let crashed_platform = MockPlatform {
            boot_id: "boot-1".into(),
            procs: Vec::new(),
        };
        let report = run_scan(&paths, &crashed_platform).unwrap();
        assert_eq!(report.marked_closed, 1);
        assert_eq!(report.marked_missing, 0);

        let reg = Registry::open(&paths.registry_db).unwrap();
        let s = reg.get_by_key("claude:crashed-session-1").unwrap().unwrap();
        assert_eq!(s.status, SessionStatus::Closed);
        assert_eq!(s.pid, None);
        assert_eq!(s.pid_start_us, None);
        assert_eq!(s.last_boot_id.as_deref(), Some("boot-1"));

        // After reboot to boot-2, the crashed session is still in the last-boot set
        let prev = reg.previous_boot_id("boot-2", 120).unwrap();
        assert_eq!(prev.as_deref(), Some("boot-1"));
        let last_boot = reg.list_last_boot(prev.as_deref(), 120).unwrap();
        assert_eq!(last_boot.len(), 1);
        assert_eq!(last_boot[0].session_id, "crashed-session-1");
    }

    #[test]
    fn pid_reuse_does_not_mark_dead_session_live() {
        let home = TestHome::new();
        let paths = AppPaths::from_home(home.0.clone());
        let transcript = home.0.join("reused.jsonl");
        std::fs::write(&transcript, "{\"type\":\"mode\"}\n").unwrap();

        let mut reg = Registry::open(&paths.registry_db).unwrap();
        reg.upsert_hook_start(&HookSessionUpsert {
            tool: Tool::Claude,
            session_id: "dead-session-pid-reused".into(),
            cwd: home.0.to_string_lossy().into_owned(),
            transcript: Some(transcript.to_string_lossy().into_owned()),
            pid: Some(4242),
            pid_start_us: Some(1_000_000),
            boot_id: "boot-1".into(),
            now: 1000,
        })
        .unwrap();
        drop(reg);

        // Same PID 4242 is now held by an unrelated process with a DIFFERENT start_time_us
        let reused_platform = MockPlatform {
            boot_id: "boot-1".into(),
            procs: vec![ProcInfo {
                pid: 4242,
                ppid: 1,
                start_time_us: 9_999_999,
                exe_path: Some(PathBuf::from("/usr/bin/python3")),
                argv: vec!["python3".into(), "-m".into(), "http.server".into()],
                cwd: Some(PathBuf::from("/tmp")),
            }],
        };

        let report = run_scan(&paths, &reused_platform).unwrap();
        assert_eq!(report.marked_closed, 1);

        let reg = Registry::open(&paths.registry_db).unwrap();
        let s = reg
            .get_by_key("claude:dead-session-pid-reused")
            .unwrap()
            .unwrap();
        assert_eq!(s.status, SessionStatus::Closed);
        assert_eq!(s.pid, None);
        assert_eq!(s.pid_start_us, None);
    }

    #[test]
    fn four_live_sessions_before_reboot_all_appear_in_last_boot_restore_set() {
        let home = TestHome::new();
        let paths = AppPaths::from_home(home.0.clone());
        std::fs::create_dir_all(paths.claude_sessions_dir()).unwrap();

        let mut procs = Vec::new();
        for i in 1..=4 {
            let pid = 6000 + i;
            let sid = format!("boot1-sess-000{i}");
            let proj = home.0.join(format!("proj-{i}"));
            std::fs::create_dir_all(&proj).unwrap();
            let transcript = home.0.join(format!("t-{i}.jsonl"));
            std::fs::write(&transcript, "{\"type\":\"mode\"}\n").unwrap();

            std::fs::write(
                paths.claude_sessions_dir().join(format!("{pid}.json")),
                format!(
                    r#"{{"pid":{pid},"sessionId":"{sid}","cwd":"{}","kind":"interactive","entrypoint":"cli"}}"#,
                    proj.display()
                ),
            )
            .unwrap();
            procs.push(ProcInfo {
                pid,
                ppid: 1,
                start_time_us: 500_000 + (i as i64),
                exe_path: Some(PathBuf::from(
                    "/Users/u/.local/share/claude/versions/2.1.330",
                )),
                argv: vec!["claude".into()],
                cwd: Some(proj),
            });
        }

        // Scan on boot-1 discovers all 4 live sessions
        let boot1 = MockPlatform {
            boot_id: "boot-1".into(),
            procs,
        };
        let r1 = run_scan(&paths, &boot1).unwrap();
        assert_eq!(r1.live_found, 4);

        // Reboot to boot-2 (empty) then boot-3 (current)
        let boot2 = MockPlatform {
            boot_id: "boot-2".into(),
            procs: Vec::new(),
        };
        // Keep transcripts present by also copying to expected paths
        for i in 1..=4 {
            let proj = home.0.join(format!("proj-{i}"));
            let encoded =
                crate::adapters::claude::ClaudeAdapter::encode_project_dir(&proj.to_string_lossy());
            let dir = paths.claude_projects_dir().join(encoded);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join(format!("boot1-sess-000{i}.jsonl")),
                "{\"type\":\"mode\"}\n",
            )
            .unwrap();
        }
        let r2 = run_scan(&paths, &boot2).unwrap();
        assert_eq!(r2.marked_closed, 4);
        assert_eq!(r2.marked_missing, 0);

        let reg = Registry::open(&paths.registry_db).unwrap();
        let prev = reg.previous_boot_id("boot-3", 120).unwrap();
        assert_eq!(prev.as_deref(), Some("boot-1"));
        let restore_set = reg.list_last_boot(prev.as_deref(), 120).unwrap();
        assert_eq!(restore_set.len(), 4);
    }
}
