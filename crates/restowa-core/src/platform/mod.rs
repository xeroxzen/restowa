use crate::error::Result;
use crate::model::ProcInfo;
use std::path::PathBuf;

#[cfg(target_os = "macos")]
pub mod macos;

/// OS-specific process and boot inspection interface.
///
/// All macOS-specific `libproc` / `sysctl` logic lives behind this trait so
/// Linux support later is a new module rather than a rewrite, and unit tests
/// can inject deterministic snapshots.
pub trait Platform {
    fn boot_id(&self) -> Result<String>;
    fn list_processes(&self) -> Result<Vec<ProcInfo>>;
    fn process_info(&self, pid: i32) -> Option<ProcInfo>;
    fn process_open_files(&self, pid: i32) -> Vec<PathBuf>;
    fn parent_pid(&self) -> i32;
}

#[cfg(target_os = "macos")]
pub fn current_platform() -> macos::MacOsPlatform {
    macos::MacOsPlatform
}

#[cfg(not(target_os = "macos"))]
pub struct UnsupportedPlatform;

#[cfg(not(target_os = "macos"))]
impl Platform for UnsupportedPlatform {
    fn boot_id(&self) -> Result<String> {
        Err(crate::error::CoreError::Platform(
            "only macOS is supported in v1".to_string(),
        ))
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
        0
    }
}

#[cfg(not(target_os = "macos"))]
pub fn current_platform() -> UnsupportedPlatform {
    UnsupportedPlatform
}
