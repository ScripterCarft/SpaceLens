//! Windows platform layer of SpaceLens.
//!
//! All `unsafe` Win32 calls of the project live in this crate, each wrapped
//! in a small safe function. Handles are owned by [`std::os::windows::io::OwnedHandle`]
//! so they are closed on every path, including early returns and panics.

#![cfg(windows)]

pub mod device;
pub mod elevation;
pub mod path;
pub mod shell;
pub mod volumes;
pub mod walker;

use std::path::Path;

use spacelens_core::Scanner;

/// Picks the best available scanner for `root`.
///
/// Currently always the parallel directory walker; the NTFS MFT reader will
/// be selected here for elevated scans of whole NTFS volumes.
pub fn scanner_for(_root: &Path) -> Box<dyn Scanner> {
    Box::new(walker::WalkScanner)
}
