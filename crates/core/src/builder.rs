//! Append-only construction of a [`Tree`].
//!
//! Scanners list one directory at a time and hand the complete listing to
//! [`TreeBuilder::add_listing`]. Because each listing is appended as a block,
//! children are contiguous and always have larger ids than their parent (see
//! the invariants in [`crate::tree`]). This makes the final aggregation a
//! single reverse pass without recursion, hashing or sorting.

use std::fmt;

use crate::names::NamePool;
use crate::tree::{DirInfo, NO_DIR, NO_NODE, Node, NodeId, ScanIssue, Tree, flags};

/// One entry of a directory listing as produced by a scanner.
///
/// `name_off`/`name_len` address WTF-8 bytes in the listing's name buffer
/// (see [`crate::names::push_wtf8`]). 32 bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RawEntry {
    pub size: u64,
    pub alloc: u64,
    pub mtime: u32,
    pub name_off: u32,
    pub name_len: u16,
    pub flags: u16,
}

impl RawEntry {
    /// Directory that the scanner has to descend into.
    #[inline]
    pub fn is_descendable_dir(&self) -> bool {
        self.flags & (flags::DIR | flags::NOT_FOLLOWED) == flags::DIR
    }
}

/// The tree would exceed its 32-bit index or name-pool limits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CapacityExceeded;

impl fmt::Display for CapacityExceeded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Baum überschreitet die maximale Kapazität (2³² Einträge bzw. 4 GiB Namen)")
    }
}

impl std::error::Error for CapacityExceeded {}

#[derive(Debug)]
pub struct TreeBuilder {
    nodes: Vec<Node>,
    dirs: Vec<DirInfo>,
    names: NamePool,
    issues: Vec<ScanIssue>,
    root_path: String,
}

impl TreeBuilder {
    /// Creates a builder whose root directory (id 0) is displayed as `root_path`.
    pub fn new(root_path: impl Into<String>) -> Self {
        Self::with_capacity(root_path, 0)
    }

    /// Like [`new`](Self::new) but pre-allocates for `expected` entries to
    /// avoid repeated reallocation of large vectors.
    pub fn with_capacity(root_path: impl Into<String>, expected: usize) -> Self {
        let root_path = root_path.into();
        let mut names = NamePool::with_capacity(expected.saturating_mul(16));
        let name_off = names.push_encoded(root_path.as_bytes()).unwrap_or(0);
        let name_len = u16::try_from(root_path.len()).unwrap_or(0);
        let mut nodes = Vec::with_capacity(expected.max(1));
        nodes.push(Node {
            size: 0,
            alloc: 0,
            parent: NO_NODE,
            name_off,
            dir: 0,
            mtime: 0,
            name_len,
            flags: flags::DIR,
        });
        let mut dirs = Vec::with_capacity((expected / 8).max(1));
        dirs.push(DirInfo::EMPTY);
        Self { nodes, dirs, names, issues: Vec::new(), root_path }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Appends the complete listing of directory `dir`.
    ///
    /// `on_subdir` is called with the id of every child directory that must be
    /// scanned next, in listing order. Listings for unknown ids, files or
    /// already populated directories are ignored (defensive; a correct scanner
    /// never produces them).
    pub fn add_listing(
        &mut self,
        dir: NodeId,
        entries: &[RawEntry],
        names: &[u8],
        mut on_subdir: impl FnMut(NodeId),
    ) -> Result<(), CapacityExceeded> {
        let Some(dir_idx) = self.nodes.get(dir as usize).map(|n| n.dir) else {
            return Ok(());
        };
        if dir_idx == NO_DIR || self.dirs[dir_idx as usize].child_count != 0 || entries.is_empty() {
            return Ok(());
        }

        let first = u32::try_from(self.nodes.len()).map_err(|_| CapacityExceeded)?;
        let count = u32::try_from(entries.len()).map_err(|_| CapacityExceeded)?;
        if first.checked_add(count).is_none_or(|end| end == NO_NODE) {
            return Err(CapacityExceeded);
        }
        self.nodes.reserve(entries.len());

        for (i, e) in entries.iter().enumerate() {
            let start = e.name_off as usize;
            let name = names.get(start..start + e.name_len as usize).unwrap_or_default();
            let name_off = self.names.push_encoded(name).ok_or(CapacityExceeded)?;
            let id = first + i as u32;
            let node_dir = if e.flags & flags::DIR != 0 {
                let idx = u32::try_from(self.dirs.len()).map_err(|_| CapacityExceeded)?;
                self.dirs.push(DirInfo::EMPTY);
                idx
            } else {
                NO_DIR
            };
            self.nodes.push(Node {
                size: e.size,
                alloc: e.alloc,
                parent: dir,
                name_off,
                dir: node_dir,
                mtime: e.mtime,
                name_len: name.len() as u16,
                flags: e.flags,
            });
            if e.is_descendable_dir() {
                on_subdir(id);
            }
        }

        let info = &mut self.dirs[dir_idx as usize];
        info.first_child = first;
        info.child_count = count;
        Ok(())
    }

    /// Records that listing `dir` failed (fully or partially) with `code`.
    pub fn mark_error(&mut self, dir: NodeId, code: u32) {
        if let Some(n) = self.nodes.get_mut(dir as usize) {
            n.flags |= flags::ERROR;
            self.issues.push(ScanIssue { node: dir, code });
        }
    }

    /// Aggregates subtree totals and freezes the tree.
    pub fn finish(mut self) -> Tree {
        aggregate(&mut self.nodes, &mut self.dirs);
        self.nodes.shrink_to_fit();
        self.dirs.shrink_to_fit();
        self.names.shrink_to_fit();
        self.issues.shrink_to_fit();
        Tree { nodes: self.nodes, dirs: self.dirs, names: self.names, issues: self.issues, root_path: self.root_path }
    }
}

/// Single reverse pass: since every child id is larger than its parent id,
/// visiting ids in descending order guarantees a child is complete before it
/// is added to its parent.
fn aggregate(nodes: &mut [Node], dirs: &mut [DirInfo]) {
    for i in (1..nodes.len()).rev() {
        let child = nodes[i];
        let parent = &mut nodes[child.parent as usize];
        parent.size += child.size;
        parent.alloc += child.alloc;
        let pdir = parent.dir as usize;
        if child.dir == NO_DIR {
            dirs[pdir].files += 1;
        } else {
            let c = dirs[child.dir as usize];
            let p = &mut dirs[pdir];
            p.files += c.files;
            p.dirs += c.dirs + 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::names::push_wtf8;

    /// Test helper: builds a listing from `(name, size, is_dir)` tuples.
    fn listing(items: &[(&str, u64, bool)]) -> (Vec<RawEntry>, Vec<u8>) {
        let mut names = Vec::new();
        let entries = items
            .iter()
            .map(|&(name, size, dir)| {
                let off = names.len() as u32;
                let wide: Vec<u16> = name.encode_utf16().collect();
                push_wtf8(&mut names, &wide);
                RawEntry {
                    size,
                    alloc: size.div_ceil(4096) * 4096,
                    mtime: 0,
                    name_off: off,
                    name_len: (names.len() as u32 - off) as u16,
                    flags: if dir { flags::DIR } else { 0 },
                }
            })
            .collect();
        (entries, names)
    }

    fn sample() -> Tree {
        let mut b = TreeBuilder::new("C:\\");
        let mut queue = Vec::new();
        let (e, n) = listing(&[("Windows", 0, true), ("pagefile.sys", 1000, false), ("Users", 0, true)]);
        b.add_listing(0, &e, &n, |id| queue.push(id)).unwrap();
        assert_eq!(queue, vec![1, 3]);
        let (e, n) = listing(&[("a.dll", 10, false), ("b.dll", 20, false)]);
        b.add_listing(1, &e, &n, |_| {}).unwrap();
        let (e, n) = listing(&[("Joe", 0, true)]);
        let mut sub = Vec::new();
        b.add_listing(3, &e, &n, |id| sub.push(id)).unwrap();
        let (e, n) = listing(&[("Übergröße.bin", 5, false)]);
        b.add_listing(sub[0], &e, &n, |_| {}).unwrap();
        b.mark_error(3, 5);
        b.finish()
    }

    #[test]
    fn aggregates_sizes_and_counts() {
        let t = sample();
        assert_eq!(t.len(), 8);
        assert_eq!(t.node(0).size, 1035);
        assert_eq!(t.node(1).size, 30);
        assert_eq!(t.node(3).size, 5);
        assert_eq!(t.counts(0), (4, 3));
        assert_eq!(t.counts(3), (1, 1));
        assert_eq!(t.counts(2), (0, 0));
    }

    #[test]
    fn upholds_layout_invariants() {
        let t = sample();
        for id in 1..t.len() as u32 {
            assert!(t.node(id).parent < id);
        }
        for id in 0..t.len() as u32 {
            for c in t.children(id) {
                assert_eq!(t.node(c).parent, id);
            }
        }
    }

    #[test]
    fn builds_paths() {
        let t = sample();
        assert_eq!(t.path(0), "C:\\");
        assert_eq!(t.path(1), "C:\\Windows");
        assert_eq!(t.path(7), "C:\\Users\\Joe\\Übergröße.bin");
        let wide = t.wide_path(7);
        assert_eq!(String::from_utf16(&wide).unwrap(), "C:\\Users\\Joe\\Übergröße.bin");
        assert_eq!(t.name(2), "pagefile.sys");
    }

    #[test]
    fn remove_updates_ancestors_and_hides_subtree() {
        let mut t = sample();
        let joe = 6;
        assert_eq!(t.name(joe), "Joe");
        let r = t.remove(joe).unwrap();
        assert_eq!(r, crate::Removed { size: 5, alloc: 4096, files: 1, dirs: 1 });
        assert_eq!(t.node(0).size, 1030);
        assert_eq!(t.node(3).size, 0);
        assert_eq!(t.counts(0), (3, 2));
        assert_eq!(t.counts(3), (0, 0));
        assert_eq!(t.live_children(3).count(), 0);
        assert_eq!(t.remove(joe), None, "already removed");
        assert_eq!(t.remove(7), None, "inside a removed subtree");
        assert_eq!(t.remove(0), None, "root");
        // Removing a single file.
        let r = t.remove(4).unwrap();
        assert_eq!((r.size, r.files, r.dirs), (10, 1, 0));
        assert_eq!((t.node(1).size, t.node(0).size), (20, 1020));
    }

    #[test]
    fn records_issues() {
        let t = sample();
        assert_eq!(t.issues(), &[ScanIssue { node: 3, code: 5 }]);
        assert!(t.node(3).has(flags::ERROR));
    }

    #[test]
    fn ignores_invalid_listings() {
        let mut b = TreeBuilder::new("X:\\");
        let (e, n) = listing(&[("f", 1, false)]);
        b.add_listing(0, &e, &n, |_| {}).unwrap();
        // Second listing for the same dir, listing for a file, unknown id.
        b.add_listing(0, &e, &n, |_| {}).unwrap();
        b.add_listing(1, &e, &n, |_| {}).unwrap();
        b.add_listing(99, &e, &n, |_| {}).unwrap();
        assert_eq!(b.finish().len(), 2);
    }

    #[test]
    fn not_followed_dirs_are_not_descended() {
        let mut b = TreeBuilder::new("X:\\");
        let (mut e, n) = listing(&[("link", 0, true), ("real", 0, true)]);
        e[0].flags |= flags::NOT_FOLLOWED | flags::REPARSE;
        let mut q = Vec::new();
        b.add_listing(0, &e, &n, |id| q.push(id)).unwrap();
        assert_eq!(q, vec![2]);
        assert_eq!(b.finish().counts(0), (0, 2));
    }
}
