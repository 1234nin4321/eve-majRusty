//! Leveled logging to `eve-maj.log` in the working directory, optionally mirrored to the console.
//!
//! Debug/info lines are buffered so the frequent debug-level scan tick costs a memcpy, not a write
//! syscall; warnings and errors flush immediately so they survive a crash right after.

use std::fmt::Arguments;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Mutex;
use std::time::Duration;

/// Serialized lowercase ("debug", "info", "warn", "err"), matching the Zig enum tag names in global.settings.json.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
#[repr(u8)]
pub enum LogLevel {
    Debug = 0,
    Info = 1,
    Warn = 2,
    Err = 3,
}

impl LogLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Debug => "DEBUG",
            Self::Info => "INFO",
            Self::Warn => "WARN",
            Self::Err => "ERROR",
        }
    }
}

pub const LOG_FILE_NAME: &str = "eve-maj.log";
const LOG_FILE_NAME_OLD: &str = "eve-maj.log.old";
// Rotated to .old at this size rather than trimmed, so a write never costs more than a size check plus (rarely) a rename.
const MAX_LOG_FILE_BYTES: u64 = 5 * 1024 * 1024;
const LOG_BUF_CAPACITY: usize = 16 * 1024;

static CURRENT_LEVEL: AtomicU8 = AtomicU8::new(LogLevel::Err as u8);
// Only mirror to stdout once main has confirmed a console exists (AllocConsole on Windows).
static CONSOLE_READY: AtomicBool = AtomicBool::new(false);

struct Sink {
    file: Option<File>,
    file_size: u64,
    buf: Vec<u8>,
}

static SINK: Mutex<Sink> = Mutex::new(Sink { file: None, file_size: 0, buf: Vec::new() });

pub fn set_level(level: LogLevel) {
    CURRENT_LEVEL.store(level as u8, Ordering::Relaxed);
}

/// Call once the console actually exists; before that, the console mirror is skipped entirely.
pub fn set_console_ready(ready: bool) {
    CONSOLE_READY.store(ready, Ordering::Relaxed);
}

fn should_log(level: LogLevel) -> bool {
    level as u8 >= CURRENT_LEVEL.load(Ordering::Relaxed)
}

fn timestamp() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f").to_string()
}

impl Sink {
    /// Lazily opens the log file so a session that never logs never touches disk.
    fn ensure_open(&mut self) -> bool {
        if self.file.is_some() {
            return true;
        }
        let Ok(file) = OpenOptions::new().create(true).append(true).open(LOG_FILE_NAME) else {
            return false;
        };
        self.file_size = file.metadata().map(|m| m.len()).unwrap_or(0);
        self.file = Some(file);
        true
    }

    /// Rotates to .old (discarding any previous .old) and starts fresh.
    fn rotate(&mut self) {
        self.file = None;
        let _ = fs::remove_file(LOG_FILE_NAME_OLD);
        let _ = fs::rename(LOG_FILE_NAME, LOG_FILE_NAME_OLD);
        self.file = File::create(LOG_FILE_NAME).ok();
        self.file_size = 0;
    }

    fn flush(&mut self) {
        if self.buf.is_empty() {
            return;
        }
        if self.file_size >= MAX_LOG_FILE_BYTES {
            self.rotate();
        }
        if let Some(file) = self.file.as_mut() {
            if file.write_all(&self.buf).is_ok() {
                self.file_size += self.buf.len() as u64;
            }
        }
        self.buf.clear();
    }

    fn write_line(&mut self, line: &str, flush_now: bool) {
        if !self.ensure_open() {
            return;
        }
        if line.len() > LOG_BUF_CAPACITY - self.buf.len() {
            self.flush();
        }
        self.buf.extend_from_slice(line.as_bytes());
        if flush_now {
            self.flush();
        }
    }
}

fn lock_sink() -> std::sync::MutexGuard<'static, Sink> {
    SINK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Used by the console control handler, since closing that window kills the process before any drop can run.
pub fn flush() {
    lock_sink().flush();
}

pub fn deinit_file() {
    let mut sink = lock_sink();
    sink.flush();
    sink.file = None;
}

/// Retries try_lock briefly to avoid missing the crash line, but bails instead of deadlocking if this thread already holds the lock (e.g. panicked inside the logger).
pub fn write_crash_line(args: Arguments<'_>) {
    const LOCK_RETRIES: u32 = 20;
    let mut attempt = 0;
    let mut sink = loop {
        match SINK.try_lock() {
            Ok(guard) => break guard,
            Err(std::sync::TryLockError::Poisoned(p)) => break p.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => {
                attempt += 1;
                if attempt >= LOCK_RETRIES {
                    return;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    };

    sink.flush();
    if sink.ensure_open() {
        let line = format!("[{}][CRASH] {}\n", timestamp(), args);
        if let Some(file) = sink.file.as_mut() {
            let _ = file.write_all(line.as_bytes());
        }
        sink.file_size += line.len() as u64;
    }
    sink.file = None;
}

/// A named logging scope; the Rust counterpart of Zig's `log.scoped("name")`.
#[derive(Clone, Copy)]
pub struct Scope(&'static str);

impl Scope {
    pub const fn new(name: &'static str) -> Self {
        Self(name)
    }

    pub fn log(self, level: LogLevel, args: Arguments<'_>) {
        if !should_log(level) {
            return;
        }
        let line = format!("[{}][{}][{}] {}\n", timestamp(), level.as_str(), self.0, args);
        lock_sink().write_line(&line, level >= LogLevel::Warn);
        if CONSOLE_READY.load(Ordering::Relaxed) {
            print!("{line}");
        }
    }

    pub fn debug(self, args: Arguments<'_>) {
        self.log(LogLevel::Debug, args)
    }

    pub fn info(self, args: Arguments<'_>) {
        self.log(LogLevel::Info, args)
    }

    pub fn warn(self, args: Arguments<'_>) {
        self.log(LogLevel::Warn, args)
    }

    pub fn err(self, args: Arguments<'_>) {
        self.log(LogLevel::Err, args)
    }
}
