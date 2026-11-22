use log::{Level, LevelFilter, Log, Metadata, Record};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_LOG_BYTES: u64 = 1_048_576; // 1 MB rotation budget

struct FileLogger {
    path: PathBuf,
    max_level: LevelFilter,
    lock: Mutex<()>,
}

impl FileLogger {
    fn rotate_if_needed(path: &Path) {
        if let Ok(meta) = fs::metadata(path) {
            if meta.len() >= MAX_LOG_BYTES {
                let rotated = path.with_extension("log.1");
                let _ = fs::rename(path, rotated);
            }
        }
    }
}

impl Log for FileLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() <= self.max_level
    }

    fn log(&self, record: &Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let _guard = match self.lock.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        if let Some(parent) = self.path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        Self::rotate_if_needed(&self.path);
        if let Ok(mut file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            let ts = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let _ = writeln!(
                file,
                "[{}] {:<5} {}: {}",
                ts,
                record.level(),
                record.target(),
                record.args()
            );
        }
    }

    fn flush(&self) {}
}

/// Initialize the tiny rotating file logger. Safe to call once per process;
/// failures are ignored so logging never breaks hook or CLI execution.
pub fn init_file_logger(path: PathBuf, verbose: bool) {
    let level = if verbose {
        LevelFilter::Debug
    } else {
        LevelFilter::Info
    };
    let logger: &'static FileLogger = Box::leak(Box::new(FileLogger {
        path,
        max_level: level,
        lock: Mutex::new(()),
    }));
    if log::set_logger(logger).is_ok() {
        log::set_max_level(if verbose {
            Level::Debug.to_level_filter()
        } else {
            Level::Info.to_level_filter()
        });
    }
}
