//! Platform-independent core of SpaceLens.
//!
//! This crate owns the in-memory representation of a scanned file system
//! ([`Tree`]), the append-only [`TreeBuilder`] that scanners feed, the scan
//! contract ([`Scanner`], [`ScanProgress`]) and read-only queries used by
//! front-ends. It performs no I/O and contains no `unsafe` code, so it can be
//! unit-tested on any platform.
//!
//! See `docs/ARCHITECTURE.md` for the invariants the data model relies on.

#![forbid(unsafe_code)]

pub mod builder;
pub mod cleanup;
pub mod filetype;
pub mod format;
pub mod names;
pub mod query;
pub mod safety;
pub mod scan;
pub mod tree;
pub mod treemap;

pub use builder::{RawEntry, TreeBuilder};
pub use scan::{ScanError, ScanOptions, ScanOutcome, ScanProgress, ScanSnapshot, Scanner};
pub use tree::{DirInfo, Node, NodeId, Removed, ScanIssue, Tree, flags};
