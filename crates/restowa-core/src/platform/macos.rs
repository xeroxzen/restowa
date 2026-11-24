use crate::error::{CoreError, Result};
use crate::model::ProcInfo;
use crate::platform::Platform;
use libproc::bsd_info::BSDInfo;
use libproc::file_info::{ListFDs, ProcFDType};
use libproc::proc_pid::{listpidinfo, pidinfo, pidpath};
use libproc::processes::{pids_by_type, ProcFilter};
use std::ffi::{c_int, c_void, CStr};
use std::path::PathBuf;

const CTL_KERN: c_int = 1;
const KERN_BOOTTIME: c_int = 21;
const KERN_PROCARGS2: c_int = 49;

// Verified against macOS <sys/proc_info.h> on arm64/x86_64 Darwin ABI:
// sizeof(struct proc_vnodepathinfo) = 2352, offsetof(pvi_cdir.vip_path) = 152
// sizeof(struct vnode_fdinfowithpath) = 1200, offsetof(pvip.vip_path) = 176
const PROC_PIDVNODEPATHINFO: c_int = 9;
const PROC_PIDFDVNODEPATHINFO: c_int = 2;
const MAXPATHLEN: usize = 1024;
const PROC_VNODEPATHINFO_SIZE: usize = 2352;
const PROC_VNODEPATHINFO_CWD_OFFSET: usize = 152;
const VNODE_FDINFOWITHPATH_SIZE: usize = 1200;
const VNODE_FDINFOWITHPATH_PATH_OFFSET: usize = 176;

#[repr(C)]
struct Timeval {
    tv_sec: isize,
    tv_usec: i32,
}

#[repr(C, align(8))]
struct AlignedBuf<const N: usize>([u8; N]);

extern "C" {
    fn sysctl(
        name: *const c_int,
        namelen: u32,
        oldp: *mut c_void,
        oldlenp: *mut usize,
        newp: *const c_void,
        newlen: usize,
    ) -> c_int;
    fn getppid() -> i32;
    fn devname(dev: i32, kind: u16) -> *const std::ffi::c_char;
    fn proc_pidinfo(
        pid: c_int,
        flavor: c_int,
        arg: u64,
        buffer: *mut c_void,
        buffersize: c_int,
    ) -> c_int;
    fn proc_pidfdinfo(
        pid: c_int,
        fd: c_int,
        flavor: c_int,
        buffer: *mut c_void,
        buffersize: c_int,
    ) -> c_int;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct MacOsPlatform;

impl Platform for MacOsPlatform {
    fn boot_id(&self) -> Result<String> {
        let mut bt = Timeval {
            tv_sec: 0,
            tv_usec: 0,
        };
        let mut size = std::mem::size_of::<Timeval>();
        let mib = [CTL_KERN, KERN_BOOTTIME];
        // SAFETY: `mib` has 2 valid elements, `bt` is properly aligned and sized.
        let rc = unsafe {
            sysctl(
                mib.as_ptr(),
                2,
                (&mut bt as *mut Timeval).cast::<c_void>(),
                &mut size,
                std::ptr::null(),
                0,
            )
        };
        if rc != 0 {
            return Err(CoreError::Platform(format!(
                "sysctl kern.boottime failed: {}",
                std::io::Error::last_os_error()
            )));
        }
        Ok(format!("{}.{:06}", bt.tv_sec, bt.tv_usec))
    }

    fn list_processes(&self) -> Result<Vec<ProcInfo>> {
        let mut out = Vec::new();
        self.for_each_process(&mut |info| out.push(info.clone()))?;
        Ok(out)
    }

    fn for_each_process(&self, f: &mut dyn FnMut(&ProcInfo)) -> Result<usize> {
        let pids = pids_by_type(ProcFilter::All)
            .map_err(|e| CoreError::Platform(format!("proc_listallpids failed: {e}")))?;

        let mut scratch = vec![0u8; 64 * 1024];
        let mut count = 0usize;
        for raw_pid in pids {
            if raw_pid == 0 {
                continue;
            }
            let pid = raw_pid as i32;
            if let Some(info) = inspect_pid_with_scratch(pid, &mut scratch) {
                count += 1;
                f(&info);
            }
        }
        Ok(count)
    }

    fn process_info(&self, pid: i32) -> Option<ProcInfo> {
        let mut scratch = vec![0u8; 64 * 1024];
        inspect_pid_with_scratch(pid, &mut scratch)
    }

    fn process_open_files(&self, pid: i32) -> Vec<PathBuf> {
        let Ok(bsd) = pidinfo::<BSDInfo>(pid, 0) else {
            return Vec::new();
        };
        let Ok(fds) = listpidinfo::<ListFDs>(pid, bsd.pbi_nfiles as usize) else {
            return Vec::new();
        };
        let mut paths = Vec::new();
        for fd in fds {
            if matches!(fd.proc_fdtype.into(), ProcFDType::VNode) {
                if let Some(path) = read_fd_vnode_path(pid, fd.proc_fd) {
                    paths.push(path);
                }
            }
        }
        paths
    }

    fn parent_pid(&self) -> i32 {
        // SAFETY: `getppid` is always safe to call.
        unsafe { getppid() }
    }

    fn process_tty(&self, pid: i32) -> Option<String> {
        const NODEV: u32 = u32::MAX;
        const S_IFCHR: u16 = 0o020000;
        let bsd = pidinfo::<BSDInfo>(pid, 0).ok()?;
        if bsd.e_tdev == NODEV {
            return None;
        }
        // SAFETY: `devname` returns NULL or a NUL-terminated string in a static
        // buffer that stays valid until the next call; it is copied right away.
        let raw = unsafe { devname(bsd.e_tdev as i32, S_IFCHR) };
        if raw.is_null() {
            return None;
        }
        // SAFETY: `raw` is non-null and NUL-terminated (see above).
        let name = unsafe { CStr::from_ptr(raw) }.to_str().ok()?;
        if name.is_empty() || name == "??" {
            return None;
        }
        Some(format!("/dev/{name}"))
    }
}

fn read_cwd(pid: i32) -> Option<PathBuf> {
    let mut buf = AlignedBuf([0u8; PROC_VNODEPATHINFO_SIZE]);
    // SAFETY: `buf` is 8-byte aligned and sized exactly to `sizeof(struct proc_vnodepathinfo)`.
    let rc = unsafe {
        proc_pidinfo(
            pid,
            PROC_PIDVNODEPATHINFO,
            0,
            buf.0.as_mut_ptr().cast::<c_void>(),
            PROC_VNODEPATHINFO_SIZE as c_int,
        )
    };
    if rc != PROC_VNODEPATHINFO_SIZE as c_int {
        return None;
    }
    bytes_to_path(&buf.0[PROC_VNODEPATHINFO_CWD_OFFSET..PROC_VNODEPATHINFO_CWD_OFFSET + MAXPATHLEN])
}

fn read_fd_vnode_path(pid: i32, fd: i32) -> Option<PathBuf> {
    let mut buf = AlignedBuf([0u8; VNODE_FDINFOWITHPATH_SIZE]);
    // SAFETY: `buf` is 8-byte aligned and sized exactly to `sizeof(struct vnode_fdinfowithpath)`.
    let rc = unsafe {
        proc_pidfdinfo(
            pid,
            fd,
            PROC_PIDFDVNODEPATHINFO,
            buf.0.as_mut_ptr().cast::<c_void>(),
            VNODE_FDINFOWITHPATH_SIZE as c_int,
        )
    };
    if rc != VNODE_FDINFOWITHPATH_SIZE as c_int {
        return None;
    }
    bytes_to_path(
        &buf.0[VNODE_FDINFOWITHPATH_PATH_OFFSET..VNODE_FDINFOWITHPATH_PATH_OFFSET + MAXPATHLEN],
    )
}

fn bytes_to_path(bytes: &[u8]) -> Option<PathBuf> {
    let nul = bytes.iter().position(|&b| b == 0)?;
    if nul == 0 {
        return None;
    }
    let cstr = CStr::from_bytes_with_nul(&bytes[..=nul]).ok()?;
    let s = cstr.to_str().ok()?;
    if s.is_empty() {
        None
    } else {
        Some(PathBuf::from(s))
    }
}

/// Inspect a single PID reusing `scratch` for `KERN_PROCARGS2` to avoid
/// per-process large buffer allocations.
fn inspect_pid_with_scratch(pid: i32, scratch: &mut Vec<u8>) -> Option<ProcInfo> {
    if pid <= 0 {
        return None;
    }
    let bsd = pidinfo::<BSDInfo>(pid, 0).ok()?;
    let start_time_us = (bsd.pbi_start_tvsec as i64)
        .saturating_mul(1_000_000)
        .saturating_add(bsd.pbi_start_tvusec as i64);

    let exe_path = pidpath(pid).ok().map(PathBuf::from);
    let argv = read_argv_with_scratch(pid, scratch).unwrap_or_default();

    // Skip processes where we cannot inspect either exe_path or argv
    // (e.g. root/system daemons owned by other users).
    if exe_path.is_none() && argv.is_empty() {
        return None;
    }

    let cwd = read_cwd(pid);

    Some(ProcInfo {
        pid,
        ppid: bsd.pbi_ppid as i32,
        start_time_us,
        exe_path,
        argv,
        cwd,
    })
}

/// Read `argv` of `pid` using `sysctl(CTL_KERN, KERN_PROCARGS2, pid)` into a
/// reused scratch buffer.
///
/// Buffer layout on macOS:
/// `[argc: i32][exec_path\0][\0...][argv[0]\0][argv[1]\0]...[envp...]`
fn read_argv_with_scratch(pid: i32, scratch: &mut Vec<u8>) -> Option<Vec<String>> {
    const MAX_ARG_LEN: usize = 1024;
    const MAX_ARGV_ITEMS: usize = 64;

    let mib = [CTL_KERN, KERN_PROCARGS2, pid];
    let mut size = scratch.len();
    // SAFETY: `scratch` has length `size`.
    let mut rc = unsafe {
        sysctl(
            mib.as_ptr(),
            3,
            scratch.as_mut_ptr().cast::<c_void>(),
            &mut size,
            std::ptr::null(),
            0,
        )
    };
    if rc != 0 && std::io::Error::last_os_error().raw_os_error() == Some(12 /* ENOMEM */) {
        let mut needed: usize = 0;
        // SAFETY: query required size with null oldp.
        let qrc = unsafe {
            sysctl(
                mib.as_ptr(),
                3,
                std::ptr::null_mut(),
                &mut needed,
                std::ptr::null(),
                0,
            )
        };
        if qrc != 0 || needed < std::mem::size_of::<i32>() || needed > 2 * 1024 * 1024 {
            return None;
        }
        scratch.resize(needed, 0);
        size = needed;
        // SAFETY: `scratch` has length `size`.
        rc = unsafe {
            sysctl(
                mib.as_ptr(),
                3,
                scratch.as_mut_ptr().cast::<c_void>(),
                &mut size,
                std::ptr::null(),
                0,
            )
        };
    }
    if rc != 0 || size < std::mem::size_of::<i32>() {
        return None;
    }

    let buf = &scratch[..size];
    let argc = i32::from_ne_bytes([buf[0], buf[1], buf[2], buf[3]]);
    if argc <= 0 {
        return Some(Vec::new());
    }

    let mut idx = std::mem::size_of::<i32>();
    // Skip exec_path
    while idx < buf.len() && buf[idx] != 0 {
        idx += 1;
    }
    // Skip trailing NUL padding after exec_path
    while idx < buf.len() && buf[idx] == 0 {
        idx += 1;
    }

    let limit = (argc as usize).min(MAX_ARGV_ITEMS);
    let mut argv = Vec::with_capacity(limit.min(16));
    for _ in 0..limit {
        if idx >= buf.len() {
            break;
        }
        let start = idx;
        while idx < buf.len() && buf[idx] != 0 {
            idx += 1;
        }
        let end = idx.min(start + MAX_ARG_LEN);
        argv.push(String::from_utf8_lossy(&buf[start..end]).into_owned());
        idx += 1; // skip NUL terminator
    }
    Some(argv)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_own_process_boot_id_cwd_argv_and_open_files() {
        let p = MacOsPlatform;
        let boot = p.boot_id().unwrap();
        assert!(boot.contains('.'));

        let my_pid = std::process::id() as i32;
        let info = p.process_info(my_pid).expect("self process_info");
        assert_eq!(info.pid, my_pid);
        assert!(info.start_time_us > 0);
        assert!(!info.argv.is_empty());
        assert!(info.exe_path.is_some());

        let expected_cwd = std::env::current_dir().unwrap().canonicalize().unwrap();
        let observed_cwd = info.cwd.expect("self cwd").canonicalize().unwrap();
        assert_eq!(observed_cwd, expected_cwd);

        // Verify open vnode FD enumeration finds a file we hold open
        let tmp_file = std::env::temp_dir().join(format!("restowa-fd-test-{my_pid}.txt"));
        let _handle = std::fs::File::create(&tmp_file).unwrap();
        let canonical_tmp = tmp_file.canonicalize().unwrap();
        let open_files = p.process_open_files(my_pid);
        let found = open_files
            .iter()
            .any(|f| f.canonicalize().ok().as_ref() == Some(&canonical_tmp));
        drop(_handle);
        let _ = std::fs::remove_file(&tmp_file);
        assert!(found, "expected open temp file in process_open_files");
    }
}
