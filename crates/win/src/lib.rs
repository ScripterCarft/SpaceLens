//! Windows platform layer of SpaceLens.
//!
//! All `unsafe` Win32 calls of the project live in this crate, each wrapped
//! in a small safe function. Handles are owned by [`std::os::windows::io::OwnedHandle`]
//! so they are closed on every path, including early returns and panics.

#![cfg(windows)]

pub mod device;
pub mod elevation;
pub mod mft;
pub mod path;
pub mod shell;
pub mod volumes;
pub mod walker;

use std::path::Path;

use spacelens_core::{ScanError, ScanOptions, ScanOutcome, ScanProgress, Scanner};

/// Which scanner to use.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Method {
    /// MFT when possible (elevated, whole NTFS volume), otherwise the walker.
    #[default]
    Auto,
    Walk,
    Mft,
}

/// Picks the best available scanner for `root`.
pub fn scanner_for(_root: &Path) -> Box<dyn Scanner> {
    Box::new(AutoScanner)
}

/// Returns the scanner for an explicitly chosen method.
pub fn scanner(method: Method) -> Box<dyn Scanner> {
    match method {
        Method::Auto => Box::new(AutoScanner),
        Method::Walk => Box::new(walker::WalkScanner),
        Method::Mft => Box::new(mft::MftScanner),
    }
}

/// Tries the MFT scanner and falls back to the directory walker whenever the
/// MFT path is not applicable or reports [`ScanError::Unsupported`].
#[derive(Clone, Copy, Debug, Default)]
pub struct AutoScanner;

impl Scanner for AutoScanner {
    fn name(&self) -> &'static str {
        "Automatisch"
    }

    fn scan(&self, root: &Path, options: &ScanOptions, progress: &ScanProgress) -> Result<ScanOutcome, ScanError> {
        if mft::applicable(root) {
            match mft::MftScanner.scan(root, options, progress) {
                Err(ScanError::Unsupported(_)) => {}
                other => return other,
            }
        }
        walker::WalkScanner.scan(root, options, progress)
    }
}
