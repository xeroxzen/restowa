use crate::adapters::{adapters_for_paths, Adapter};
use crate::error::Result;
use crate::model::{ProcInfo, Session, SessionRef, SessionSource, SessionStatus};
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
    pub processes_inspected: usize,
    pub live_found: usize,
    pub marked_closed: usize,
    pub marked_missing: usize,
}

/// Discover all currently live sessions from running processes without writing
/// to the registry. Used by both `restowa scan` and read-only `restowa list`
/// (so `restowa list` works even before hooks or launchd are installed).
pub fn discover_live_sessions(
    paths: &AppPaths,
    platform: &dyn Platform,
) -> Result<(Vec<ProcInfo>, Vec<DiscoveredLiveSession>)> {
    let procs = platform.list_processes()?;
    let adapters = adapters_for_paths(paths);
    let mut found = Vec::new();

    for proc_info in &procs {
        for adapter in &adapters {
            if adapter.matches_process(proc_info) {
                match adapter.session_for_process(proc_info) {
                    Ok(Some(session_ref)) => {
                        found.push(DiscoveredLiveSession {
                            session_ref,
                            pid: proc_info.pid,
                            pid_start_us: proc_info.start_time_us,
                        });
                    }
                    Ok(None) => {}
                    Err(err) => {
                        log::warn!(
                            "adapter {} failed resolving pid {}: {}",
                            adapter.tool(),
                            proc_info.pid,
                            err
                        );
                    }
                }
                break;
            }
        }
    }

    Ok((procs, found))
}

/// Execute one complete stateless scan pass and persist updates to SQLite.
pub fn run_scan(paths: &AppPaths, platform: &dyn Platform) -> Result<ScanReport> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let boot_id = platform.boot_id()?;
    let mut registry = Registry::open(&paths.registry_db)?;
    registry.record_boot(&boot_id, now)?;

    let (procs, discovered) = discover_live_sessions(paths, platform)?;
    let proc_starts: HashMap<i32, i64> = procs.iter().map(|p| (p.pid, p.start_time_us)).collect();

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
                if !adapter.session_exists(session) {
                    registry.mark_missing(&session.key)?;
                    marked_missing += 1;
                }
            }
        }
    }

    Ok(ScanReport {
        boot_id,
        processes_inspected: procs.len(),
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

    let db_sessions = match Registry::open_read_only(&paths.registry_db)? {
        Some(reg) => reg.list_all(include_hidden)?,
        None => Vec::new(),
    };

    let (procs, discovered) = discover_live_sessions(paths, platform)?;
    let proc_starts: HashMap<i32, i64> = procs.iter().map(|p| (p.pid, p.start_time_us)).collect();

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
                s.title = adapter.title(&s);
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
