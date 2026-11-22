use crate::error::{CoreError, Result};
use crate::paths::AppPaths;
use serde::Serialize;
use std::path::Path;
use std::process::Command;

pub const LAUNCHD_LABEL: &str = "dev.restowa.scan";
pub const DEFAULT_SCAN_INTERVAL_SECS: u64 = 120;

#[derive(Debug, Clone, Serialize)]
pub struct LaunchdInstallReport {
    pub plist_path: String,
    pub binary_path: String,
    pub interval_secs: u64,
    pub loaded: bool,
    pub points_into_target: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct LaunchdUninstallReport {
    pub plist_path: String,
    pub removed: bool,
    pub unloaded: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct LaunchdStatus {
    pub plist_exists: bool,
    pub plist_path: String,
    pub program_path: Option<String>,
    pub binary_exists: bool,
    pub points_into_target: bool,
    pub loaded: bool,
}

extern "C" {
    fn geteuid() -> u32;
}

fn current_uid() -> u32 {
    // SAFETY: `geteuid` is always safe to call on Unix.
    unsafe { geteuid() }
}

pub fn path_points_into_target(path: &str) -> bool {
    path.contains("/target/") || path.starts_with("target/")
}

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

fn xml_unescape(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
}

pub fn render_plist(binary_abs_path: &Path, interval_secs: u64) -> String {
    let escaped = xml_escape(&binary_abs_path.to_string_lossy());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{LAUNCHD_LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{escaped}</string>
    <string>scan</string>
  </array>
  <key>StartInterval</key>
  <integer>{interval_secs}</integer>
  <key>RunAtLoad</key>
  <true/>
  <key>ProcessType</key>
  <string>Background</string>
  <key>LowPriorityIO</key>
  <true/>
</dict>
</plist>
"#
    )
}

/// Extract `ProgramArguments[0]` from a launchd XML plist.
pub fn parse_plist_program_path(plist_xml: &str) -> Option<String> {
    let after_key = plist_xml.split("<key>ProgramArguments</key>").nth(1)?;
    let after_array = after_key.split("<array>").nth(1)?;
    let after_open = after_array.split("<string>").nth(1)?;
    let raw = after_open.split("</string>").next()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(xml_unescape(trimmed))
    }
}

pub fn install_launchd_agent(
    paths: &AppPaths,
    binary_abs_path: &Path,
    interval_secs: u64,
    load: bool,
) -> Result<LaunchdInstallReport> {
    if !binary_abs_path.is_absolute() {
        return Err(CoreError::Platform(format!(
            "launchd binary path must be absolute: {}",
            binary_abs_path.display()
        )));
    }

    let plist_path = paths.launch_agent_plist();
    AppPaths::ensure_parent(&plist_path)?;

    let content = render_plist(binary_abs_path, interval_secs);
    let tmp = plist_path.with_extension(format!("plist.restowa.tmp.{}", std::process::id()));
    std::fs::write(&tmp, &content).map_err(|source| CoreError::IoPath {
        path: tmp.clone(),
        source,
    })?;
    std::fs::rename(&tmp, &plist_path).map_err(|source| CoreError::IoPath {
        path: plist_path.clone(),
        source,
    })?;

    let mut loaded = false;
    if load {
        let domain = format!("gui/{}", current_uid());
        // Bootout any previous registration (ignore errors if not loaded yet).
        let _ = Command::new("/bin/launchctl")
            .args(["bootout", &domain])
            .arg(&plist_path)
            .output();

        let bootstrap = Command::new("/bin/launchctl")
            .args(["bootstrap", &domain])
            .arg(&plist_path)
            .output()?;

        if bootstrap.status.success() {
            loaded = true;
        } else {
            // Fallback to legacy `launchctl load -w` on older/edge environments.
            let legacy = Command::new("/bin/launchctl")
                .args(["load", "-w"])
                .arg(&plist_path)
                .output()?;
            if legacy.status.success() {
                loaded = true;
            } else {
                let stderr = String::from_utf8_lossy(&bootstrap.stderr)
                    .trim()
                    .to_string();
                return Err(CoreError::Platform(format!(
                    "launchctl bootstrap failed: {stderr}"
                )));
            }
        }
    }

    let bin_str = binary_abs_path.to_string_lossy().into_owned();
    let points_into_target = path_points_into_target(&bin_str);

    Ok(LaunchdInstallReport {
        plist_path: plist_path.to_string_lossy().into_owned(),
        binary_path: bin_str,
        interval_secs,
        loaded,
        points_into_target,
    })
}

pub fn uninstall_launchd_agent(paths: &AppPaths, unload: bool) -> Result<LaunchdUninstallReport> {
    let plist_path = paths.launch_agent_plist();
    let mut unloaded = false;
    let mut removed = false;

    if plist_path.exists() {
        if unload {
            let domain = format!("gui/{}", current_uid());
            let out = Command::new("/bin/launchctl")
                .args(["bootout", &domain])
                .arg(&plist_path)
                .output();
            if let Ok(o) = out {
                unloaded = o.status.success();
            }
        }
        std::fs::remove_file(&plist_path).map_err(|source| CoreError::IoPath {
            path: plist_path.clone(),
            source,
        })?;
        removed = true;
    }

    Ok(LaunchdUninstallReport {
        plist_path: plist_path.to_string_lossy().into_owned(),
        removed,
        unloaded,
    })
}

pub fn is_agent_loaded() -> bool {
    let target = format!("gui/{}/{LAUNCHD_LABEL}", current_uid());
    Command::new("/bin/launchctl")
        .args(["print", &target])
        .output()
        .is_ok_and(|o| o.status.success())
}

pub fn inspect_launchd_agent(paths: &AppPaths, check_launchctl: bool) -> LaunchdStatus {
    let plist_path = paths.launch_agent_plist();
    let plist_str = plist_path.to_string_lossy().into_owned();
    if !plist_path.is_file() {
        return LaunchdStatus {
            plist_exists: false,
            plist_path: plist_str,
            program_path: None,
            binary_exists: false,
            points_into_target: false,
            loaded: false,
        };
    }

    let program_path = std::fs::read_to_string(&plist_path)
        .ok()
        .and_then(|xml| parse_plist_program_path(&xml));

    let binary_exists = program_path
        .as_deref()
        .is_some_and(|p| Path::new(p).is_file());
    let points_into_target = program_path.as_deref().is_some_and(path_points_into_target);
    let loaded = if check_launchctl {
        is_agent_loaded()
    } else {
        false
    };

    LaunchdStatus {
        plist_exists: true,
        plist_path: plist_str,
        program_path,
        binary_exists,
        points_into_target,
        loaded,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TestHome(PathBuf);
    impl TestHome {
        fn new() -> Self {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "restowa-launchd-test-{}-{}",
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

    #[test]
    fn render_and_parse_plist_roundtrips_with_xml_escaping() {
        let bin = Path::new("/Users/me/bin & tools/restowa");
        let xml = render_plist(bin, 120);
        assert!(xml.contains("<string>/Users/me/bin &amp; tools/restowa</string>"));
        assert!(xml.contains("<integer>120</integer>"));
        assert_eq!(
            parse_plist_program_path(&xml).as_deref(),
            Some("/Users/me/bin & tools/restowa")
        );
    }

    #[test]
    fn install_writes_plist_and_detects_target_path() {
        let home = TestHome::new();
        let paths = AppPaths::from_home(home.0.clone());

        let fake_target_bin = home.0.join("repo/target/release/restowa");
        std::fs::create_dir_all(fake_target_bin.parent().unwrap()).unwrap();
        std::fs::write(&fake_target_bin, b"bin").unwrap();

        let report = install_launchd_agent(&paths, &fake_target_bin, 120, false).unwrap();
        assert!(report.points_into_target);
        assert!(paths.launch_agent_plist().is_file());

        let status = inspect_launchd_agent(&paths, false);
        assert!(status.plist_exists);
        assert!(status.binary_exists);
        assert!(status.points_into_target);

        let un = uninstall_launchd_agent(&paths, false).unwrap();
        assert!(un.removed);
        assert!(!paths.launch_agent_plist().exists());
    }
}
