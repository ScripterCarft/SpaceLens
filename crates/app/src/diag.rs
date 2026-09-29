//! Diagnostics: crash reports and a small event/performance log.
//!
//! Files live in `%LOCALAPPDATA%\SpaceLens\logs`:
//! * `crash-<unix time>.txt` – one per panic: version, thread, message,
//!   location and backtrace (symbolized when `SpaceLens.pdb` lies next to the exe).
//! * `spacelens.log` – notable events and every UI action slower than
//!   [`SLOW`]; rotated to `spacelens.log.1` at 1 MB so it never grows unbounded.
//!
//! Logging never fails the app: all I/O errors are ignored.

use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// UI actions slower than this are logged.
pub const SLOW: Duration = Duration::from_millis(30);
const MAX_LOG: u64 = 1 << 20;

static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
static WRITE: Mutex<()> = Mutex::new(());

/// Sets the log directory (call once at start-up).
pub fn init(dir: Option<PathBuf>) {
    let _ = DIR.set(dir);
}

pub fn dir() -> Option<&'static Path> {
    DIR.get().and_then(|d| d.as_deref())
}

fn now() -> (u64, String) {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let stamp = spacelens_core::format::unix_time(u32::try_from(secs).unwrap_or(u32::MAX));
    (secs, format!("{stamp}:{:02} UTC", secs % 60))
}

/// Appends one line to `spacelens.log`.
pub fn log(message: &str) {
    let Some(dir) = dir() else { return };
    let _guard = WRITE.lock();
    if fs::create_dir_all(dir).is_err() {
        return;
    }
    let file = dir.join("spacelens.log");
    if fs::metadata(&file).is_ok_and(|m| m.len() > MAX_LOG) {
        let _ = fs::rename(&file, dir.join("spacelens.log.1"));
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&file) {
        let _ = writeln!(f, "{}  {message}", now().1);
    }
}

/// Measures a UI action; logs it on drop if it took longer than [`SLOW`].
pub struct Timed {
    label: &'static str,
    start: Instant,
}

impl Timed {
    pub fn new(label: &'static str) -> Self {
        Self { label, start: Instant::now() }
    }
}

impl Drop for Timed {
    fn drop(&mut self) {
        let elapsed = self.start.elapsed();
        if elapsed >= SLOW {
            log(&format!("langsam: {} {} ms", self.label, elapsed.as_millis()));
        }
    }
}

/// Writes a crash report and returns its path.
pub fn crash_report(info: &std::panic::PanicHookInfo<'_>) -> Option<PathBuf> {
    let dir = dir()?;
    fs::create_dir_all(dir).ok()?;
    let (secs, stamp) = now();
    let thread = std::thread::current();
    let mut text = String::new();
    let _ = writeln!(text, "SpaceLens {} – Absturzbericht", env!("CARGO_PKG_VERSION"));
    let _ = writeln!(text, "Zeit:    {stamp}");
    let _ = writeln!(text, "Thread:  {}", thread.name().unwrap_or("?"));
    let _ = writeln!(text, "Fehler:  {info}");
    let _ = writeln!(text, "\nBacktrace:\n{}", std::backtrace::Backtrace::force_capture());
    let path = dir.join(format!("crash-{secs}.txt"));
    fs::write(&path, text).ok()?;
    log(&format!("Absturz, Bericht: {}", path.display()));
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logs_rotate_and_never_fail() {
        let dir = std::env::temp_dir().join(format!("spacelens-diag-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        init(Some(dir.clone()));
        log("hallo");
        let text = fs::read_to_string(dir.join("spacelens.log")).unwrap();
        assert!(text.contains("hallo"));
        fs::write(dir.join("spacelens.log"), vec![b'x'; (MAX_LOG + 1) as usize]).unwrap();
        log("nach Rotation");
        assert!(dir.join("spacelens.log.1").exists());
        assert!(fs::read_to_string(dir.join("spacelens.log")).unwrap().contains("nach Rotation"));
        let _ = fs::remove_dir_all(&dir);
    }
}
