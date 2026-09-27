//! The immutable result of a scan: a flat, index-based file system tree.
//!
//! # Layout
//!
//! * All entries live in one `Vec<Node>` (40 bytes each). Nodes reference each
//!   other by `u32` index ([`NodeId`]) instead of pointers.
//! * Directories additionally own one [`DirInfo`] (16 bytes) in a separate
//!   vector, so the ~90 % of entries that are files do not pay for it.
//! * Names live in a shared [`NamePool`].
//!
//! # Invariants (relied upon by aggregation and queries)
//!
//! 1. `parent(id) < id` for every node except the root (id 0).
//! 2. The children of a directory occupy the contiguous id range
//!    `first_child .. first_child + child_count`.
//! 3. After [`TreeBuilder::finish`](crate::TreeBuilder::finish), `size`,
//!    `alloc` and the counters in [`DirInfo`] of a directory are the totals of
//!    its whole subtree.

use std::borrow::Cow;
use std::ops::Range;

use crate::names::{self, NamePool};

/// Index of a node inside a [`Tree`].
pub type NodeId = u32;

/// Sentinel for "no node" (parent of the root, unset links).
pub const NO_NODE: NodeId = u32::MAX;

/// Sentinel for [`Node::dir`] on files.
pub const NO_DIR: u32 = u32::MAX;

/// Bit flags stored in [`Node::flags`].
pub mod flags {
    /// Entry is a directory.
    pub const DIR: u16 = 1 << 0;
    /// Entry is a reparse point (symlink, junction, cloud placeholder, …).
    pub const REPARSE: u16 = 1 << 1;
    /// Directory was intentionally not descended (symlink / junction / mount
    /// point); its size does not contribute to the total.
    pub const NOT_FOLLOWED: u16 = 1 << 2;
    /// Listing this directory failed completely or partially.
    pub const ERROR: u16 = 1 << 3;
    pub const HIDDEN: u16 = 1 << 4;
    pub const SYSTEM: u16 = 1 << 5;
    pub const COMPRESSED: u16 = 1 << 6;
    pub const SPARSE: u16 = 1 << 7;
    /// Content is (partially) not stored locally (cloud file / offline).
    pub const CLOUD: u16 = 1 << 8;
    /// Additional hard link of a file whose size is already counted at
    /// another link (MFT scan only).
    pub const HARDLINK: u16 = 1 << 9;
    /// Removed after the scan (e.g. moved to the recycle bin). Its size has
    /// been subtracted from all ancestors; views skip it and its subtree.
    pub const DELETED: u16 = 1 << 10;
}

/// One file or directory. 40 bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct Node {
    /// Logical size in bytes (subtree total for directories).
    pub size: u64,
    /// Bytes allocated on disk (subtree total for directories).
    pub alloc: u64,
    pub parent: NodeId,
    pub name_off: u32,
    /// Index into [`Tree::dirs`] for directories, [`NO_DIR`] for files.
    pub dir: u32,
    /// Last write time as Unix seconds (0 = unknown).
    pub mtime: u32,
    pub name_len: u16,
    pub flags: u16,
}

const _: () = assert!(size_of::<Node>() == 40);

impl Node {
    #[inline]
    pub fn is_dir(&self) -> bool {
        self.flags & flags::DIR != 0
    }

    #[inline]
    pub fn has(&self, flag: u16) -> bool {
        self.flags & flag != 0
    }
}

/// Directory-only data. 16 bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct DirInfo {
    pub first_child: NodeId,
    pub child_count: u32,
    /// Number of files in the whole subtree.
    pub files: u32,
    /// Number of directories in the whole subtree (excluding itself).
    pub dirs: u32,
}

const _: () = assert!(size_of::<DirInfo>() == 16);

impl DirInfo {
    pub const EMPTY: Self = Self { first_child: NO_NODE, child_count: 0, files: 0, dirs: 0 };

    #[inline]
    pub fn children(&self) -> Range<NodeId> {
        if self.child_count == 0 {
            return 0..0;
        }
        self.first_child..self.first_child + self.child_count
    }
}

/// A problem encountered while scanning a directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScanIssue {
    pub node: NodeId,
    /// Raw OS error code (e.g. `ERROR_ACCESS_DENIED` = 5).
    pub code: u32,
}

/// Scan result. Shared via `Arc<Tree>`; only [`Tree::remove`] mutates it.
#[derive(Clone, Debug)]
pub struct Tree {
    pub(crate) nodes: Vec<Node>,
    pub(crate) dirs: Vec<DirInfo>,
    pub(crate) names: NamePool,
    pub(crate) issues: Vec<ScanIssue>,
    /// Display form of the scanned root, e.g. `C:\`.
    pub(crate) root_path: String,
}

impl Tree {
    pub const ROOT: NodeId = 0;

    #[inline]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    #[inline]
    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id as usize]
    }

    #[inline]
    pub fn get(&self, id: NodeId) -> Option<&Node> {
        self.nodes.get(id as usize)
    }

    #[inline]
    pub fn dir_info(&self, id: NodeId) -> Option<&DirInfo> {
        self.dirs.get(self.nodes.get(id as usize)?.dir as usize)
    }

    /// Ids of the direct children (empty for files).
    #[inline]
    pub fn children(&self, id: NodeId) -> Range<NodeId> {
        self.dir_info(id).map_or(0..0, DirInfo::children)
    }

    /// Direct children that have not been removed.
    #[inline]
    pub fn live_children(&self, id: NodeId) -> impl Iterator<Item = NodeId> + '_ {
        self.children(id).filter(|&c| !self.nodes[c as usize].has(flags::DELETED))
    }

    #[inline]
    pub fn parent(&self, id: NodeId) -> Option<NodeId> {
        let p = self.nodes.get(id as usize)?.parent;
        (p != NO_NODE).then_some(p)
    }

    /// Raw WTF-8 name bytes.
    #[inline]
    pub fn name_bytes(&self, id: NodeId) -> &[u8] {
        let n = &self.nodes[id as usize];
        self.names.get(n.name_off, n.name_len)
    }

    /// Display name (zero-copy for valid UTF-8).
    #[inline]
    pub fn name(&self, id: NodeId) -> Cow<'_, str> {
        names::display(self.name_bytes(id))
    }

    pub fn root_path(&self) -> &str {
        &self.root_path
    }

    pub fn issues(&self) -> &[ScanIssue] {
        &self.issues
    }

    /// Chain of ids from the root down to `id` (inclusive).
    pub fn ancestry(&self, id: NodeId) -> Vec<NodeId> {
        let mut chain = Vec::with_capacity(32);
        let mut cur = id;
        while (cur as usize) < self.nodes.len() {
            chain.push(cur);
            cur = self.nodes[cur as usize].parent;
        }
        chain.reverse();
        chain
    }

    /// Full display path of a node, e.g. `C:\Users\Joe`.
    pub fn path(&self, id: NodeId) -> String {
        let chain = self.ancestry(id);
        let mut out = String::with_capacity(self.root_path.len() + chain.len() * 16);
        out.push_str(&self.root_path);
        for (i, &n) in chain.iter().enumerate().skip(1) {
            if i > 1 || !out.ends_with('\\') {
                out.push('\\');
            }
            out.push_str(&self.name(n));
        }
        out
    }

    /// Full path as exact UTF-16 (lossless, for OS calls).
    pub fn wide_path(&self, id: NodeId) -> Vec<u16> {
        let chain = self.ancestry(id);
        let mut out: Vec<u16> = self.root_path.encode_utf16().collect();
        for &n in chain.iter().skip(1) {
            if out.last() != Some(&u16::from(b'\\')) {
                out.push(u16::from(b'\\'));
            }
            names::wtf8_to_wide(self.name_bytes(n), &mut out);
        }
        out
    }

    /// Approximate heap memory held by this tree.
    pub fn memory_bytes(&self) -> usize {
        self.nodes.capacity() * size_of::<Node>()
            + self.dirs.capacity() * size_of::<DirInfo>()
            + self.names.capacity_bytes()
            + self.issues.capacity() * size_of::<ScanIssue>()
    }

    /// Number of files and directories below `id` (0/0 for files).
    pub fn counts(&self, id: NodeId) -> (u32, u32) {
        self.dir_info(id).map_or((0, 0), |d| (d.files, d.dirs))
    }
}

/// What [`Tree::remove`] took away.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Removed {
    pub size: u64,
    pub alloc: u64,
    pub files: u32,
    pub dirs: u32,
}

impl Tree {
    /// Marks `id` (and thereby its subtree) as removed and subtracts its
    /// totals from every ancestor, so all views stay correct without a
    /// rescan. Cost: O(depth). Returns `None` for the root or an item that
    /// is already removed.
    pub fn remove(&mut self, id: NodeId) -> Option<Removed> {
        if id == Self::ROOT {
            return None;
        }
        let node = *self.nodes.get(id as usize)?;
        if node.has(flags::DELETED) || self.ancestry(id).iter().any(|&a| self.nodes[a as usize].has(flags::DELETED)) {
            return None;
        }
        let (files, dirs) = match self.dir_info(id) {
            Some(d) => (d.files, d.dirs + 1),
            None => (1, 0),
        };
        let removed = Removed { size: node.size, alloc: node.alloc, files, dirs };

        self.nodes[id as usize].flags |= flags::DELETED;
        let mut cur = node.parent;
        while let Some(n) = self.nodes.get_mut(cur as usize) {
            n.size = n.size.saturating_sub(removed.size);
            n.alloc = n.alloc.saturating_sub(removed.alloc);
            let (parent, dir) = (n.parent, n.dir);
            if let Some(d) = self.dirs.get_mut(dir as usize) {
                d.files = d.files.saturating_sub(files);
                d.dirs = d.dirs.saturating_sub(dirs);
            }
            cur = parent;
        }
        Some(removed)
    }
}

impl Tree {
    /// Finds a node by absolute display path (case-insensitive like Windows),
    /// e.g. `C:\Users\Joe\AppData\Local\Temp`. Cost: one pass over the
    /// children of each directory on the way.
    pub fn find_path(&self, path: &str) -> Option<NodeId> {
        const SEP: char = '\\';
        let path = path.strip_prefix(r"\\?\").unwrap_or(path).trim_end_matches(SEP);
        let root = self.root_path.trim_end_matches(SEP);
        if path.len() < root.len() || !path.is_char_boundary(root.len()) {
            return None;
        }
        let (head, rest) = path.split_at(root.len());
        if !names_equal(head, root) {
            return None;
        }
        if !rest.is_empty() && !rest.starts_with(SEP) {
            return None; // "C:\Users2" must not match root "C:\Users"
        }
        let mut cur = Self::ROOT;
        for part in rest.split(SEP).filter(|p| !p.is_empty()) {
            cur = self.live_children(cur).find(|&c| names_equal(&self.name(c), part))?;
        }
        Some(cur)
    }
}

/// Case-insensitive name comparison (ASCII fast path, Unicode fallback).
pub fn names_equal(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b) || (!a.is_ascii() && a.to_lowercase() == b.to_lowercase())
}
