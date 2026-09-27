//! Contract between scanners and front-ends.
//!
//! A scanner runs on background threads and reports progress through a
//! shared [`ScanProgress`] made of atomics. Front-ends poll it at their own
//! pace (e.g. 10 Hz) instead of receiving an event per file, so progress
//! reporting costs the scanner practically nothing.

use std::fmt;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use crate::tree::Tree;

/// Live counters of a running scan plus its cancellation flag.
#[derive(Debug, Default)]
pub struct ScanProgress {
    files: AtomicU64,
    dirs: AtomicU64,
    bytes: AtomicU64,
    errors: AtomicU64,
    cancelled: AtomicBool,
    current: Mutex<String>,
}

/// Point-in-time copy of [`ScanProgress`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScanSnapshot {
    pub files: u64,
    pub dirs: u64,
    pub bytes: u64,
    pub errors: u64,
}

impl ScanProgress {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds the totals of one processed directory listing.
    #[inline]
    pub fn add(&self, files: u64, dirs: u64, bytes: u64) {
        self.files.fetch_add(files, Ordering::Relaxed);
        self.dirs.fetch_add(dirs, Ordering::Relaxed);
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    #[inline]
    pub fn add_error(&self) {
        self.errors.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> ScanSnapshot {
        ScanSnapshot {
            files: self.files.load(Ordering::Relaxed),
            dirs: self.dirs.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
        }
    }

    /// Requests cancellation; scanners stop at the next directory boundary.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    #[inline]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    /// Sets the path shown as "currently scanning". Callers should throttle.
    pub fn set_current(&self, path: &str) {
        if let Ok(mut cur) = self.current.lock() {
            cur.clear();
            cur.push_str(path);
        }
    }

    pub fn current(&self) -> String {
        self.current.lock().map(|c| c.clone()).unwrap_or_default()
    }
}

/// Tuning knobs for a scan.
#[derive(Clone, Debug, Default)]
pub struct ScanOptions {
    /// Worker threads; `0` lets the scanner choose based on the device.
    pub threads: usize,
}

/// Result of a finished (or cancelled) scan.
#[derive(Debug)]
pub struct ScanOutcome {
    pub tree: Tree,
    pub elapsed: Duration,
    /// `true` if the scan was cancelled; the tree then holds a consistent
    /// but incomplete snapshot.
    pub cancelled: bool,
    /// Human-readable name of the method used, e.g. "Verzeichnis-Scan".
    pub method: &'static str,
    pub threads: usize,
}

#[derive(Debug)]
pub enum ScanError {
    /// The root could not be opened as a directory.
    InvalidRoot { path: String, code: u32 },
    /// The tree grew beyond its 32-bit limits.
    Capacity,
    /// A bug (e.g. a panic) inside the scanner; the scan was aborted.
    Internal(String),
}

impl fmt::Display for ScanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRoot { path, code } => {
                write!(f, "\u{201E}{path}\u{201C} kann nicht gelesen werden (Fehler {code})")
            }
            Self::Capacity => f.write_str("Zu viele Einträge für einen Scan"),
            Self::Internal(msg) => write!(f, "Interner Fehler: {msg}"),
        }
    }
}

impl std::error::Error for ScanError {}

/// A strategy for turning a directory on disk into a [`Tree`].
pub trait Scanner: Send + Sync {
    fn name(&self) -> &'static str;

    fn scan(&self, root: &Path, options: &ScanOptions, progress: &ScanProgress) -> Result<ScanOutcome, ScanError>;
}
