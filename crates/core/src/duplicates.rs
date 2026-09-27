//! Duplicate detection, pure part: which files can be duplicates at all.
//!
//! Files can only be identical if they have the same size, which the scan
//! already knows – so this first stage needs no disk access and typically
//! rules out >95 % of all files. Hashing the remaining candidates happens in
//! the platform layer (`spacelens_win::dupes`).

use std::collections::HashMap;

use crate::safety;
use crate::tree::{NodeId, Tree, flags};

/// A set of files with identical content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DupGroup {
    pub size: u64,
    pub ids: Vec<NodeId>,
}

impl DupGroup {
    /// Space that removing all but one copy would free.
    pub fn wasted(&self) -> u64 {
        self.size * (self.ids.len() as u64).saturating_sub(1)
    }
}

/// Sorts groups by wasted space, largest first.
pub fn sort_groups(groups: &mut [DupGroup]) {
    groups.sort_by(|a, b| b.wasted().cmp(&a.wasted()).then(b.size.cmp(&a.size)));
}

/// Files below `root` that share their size with at least one other file,
/// grouped by size (largest first). Skipped: system areas and program data
/// ([`safety::is_system_area`]) unless `root` itself lies in one, hard links, cloud placeholders (reading
/// them would trigger a download) and files smaller than `min_size`.
pub fn candidates(tree: &Tree, root: NodeId, min_size: u64) -> Vec<Vec<NodeId>> {
    let min_size = min_size.max(1);
    // Searching *inside* a system area is a deliberate choice: then do not filter.
    let skip_areas = !safety::is_system_area(tree, root);
    let mut by_size: HashMap<u64, Vec<NodeId>> = HashMap::new();
    let mut stack = vec![root];
    while let Some(id) = stack.pop() {
        let node = tree.node(id);
        if node.is_dir() {
            if node.has(flags::NOT_FOLLOWED) || (skip_areas && id != root && safety::is_system_area(tree, id)) {
                continue;
            }
            stack.extend(tree.live_children(id));
            continue;
        }
        if node.size < min_size || node.has(flags::HARDLINK) || node.has(flags::CLOUD) || node.has(flags::SPARSE) {
            continue;
        }
        by_size.entry(node.size).or_default().push(id);
    }
    let mut groups: Vec<(u64, Vec<NodeId>)> = by_size.into_iter().filter(|(_, v)| v.len() > 1).collect();
    groups.sort_unstable_by_key(|g| std::cmp::Reverse(g.0));
    groups.into_iter().map(|(_, v)| v).collect()
}

/// Splits `ids` into groups of equal `key`, keeping only groups of two or
/// more. Items whose key is `None` (unreadable) are dropped.
pub fn regroup<K: std::hash::Hash + Eq>(ids: &[NodeId], keys: &[Option<K>]) -> Vec<Vec<NodeId>> {
    let mut map: HashMap<&K, Vec<NodeId>> = HashMap::new();
    for (id, key) in ids.iter().zip(keys) {
        if let Some(k) = key {
            map.entry(k).or_default().push(*id);
        }
    }
    let mut out: Vec<Vec<NodeId>> = map.into_values().filter(|v| v.len() > 1).collect();
    for g in &mut out {
        g.sort_unstable();
    }
    out.sort_unstable();
    out
}

/// The copy to keep in a group: the oldest file (most likely the original),
/// ties broken by the shorter path.
pub fn keeper(tree: &Tree, ids: &[NodeId]) -> Option<NodeId> {
    ids.iter().copied().min_by_key(|&id| {
        let mtime = tree.node(id).mtime;
        (if mtime == 0 { u32::MAX } else { mtime }, tree.ancestry(id).len(), id)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builder::{RawEntry, TreeBuilder};

    /// root: Users/Joe/{a.mkv 100 (t=5), b.mkv 100 (t=3), c.txt 7, AppData/x.bin 100},
    ///       Windows/y.dll 100, d.mkv 100 (hardlink), e.bin 100 (cloud)
    fn tree() -> Tree {
        let mut names = Vec::new();
        let mut e = |name: &str, size: u64, dir: bool, mtime: u32, extra: u16| {
            let off = names.len() as u32;
            names.extend_from_slice(name.as_bytes());
            RawEntry {
                size,
                alloc: size,
                mtime,
                name_off: off,
                name_len: name.len() as u16,
                flags: if dir { flags::DIR } else { 0 } | extra,
            }
        };
        let root = [
            e("Users", 0, true, 0, 0),
            e("Windows", 0, true, 0, 0),
            e("d.mkv", 100, false, 1, flags::HARDLINK),
            e("e.bin", 100, false, 1, flags::CLOUD),
        ];
        let users = [e("Joe", 0, true, 0, 0)];
        let joe = [
            e("a.mkv", 100, false, 5, 0),
            e("b.mkv", 100, false, 3, 0),
            e("c.txt", 7, false, 1, 0),
            e("AppData", 0, true, 0, 0),
        ];
        let appdata = [e("x.bin", 100, false, 1, 0)];
        let windows = [e("y.dll", 100, false, 1, 0)];
        let mut b = TreeBuilder::new("C:\\");
        let mut top = Vec::new();
        b.add_listing(0, &root, &names, |id| top.push(id)).unwrap();
        let mut u = Vec::new();
        b.add_listing(top[0], &users, &names, |id| u.push(id)).unwrap();
        b.add_listing(top[1], &windows, &names, |_| {}).unwrap();
        let mut j = Vec::new();
        b.add_listing(u[0], &joe, &names, |id| j.push(id)).unwrap();
        b.add_listing(j[0], &appdata, &names, |_| {}).unwrap();
        b.finish()
    }

    fn names(t: &Tree, ids: &[NodeId]) -> Vec<String> {
        let mut v: Vec<String> = ids.iter().map(|&i| t.name(i).into_owned()).collect();
        v.sort();
        v
    }

    #[test]
    fn candidates_skip_system_areas_links_and_cloud_files() {
        let t = tree();
        let groups = candidates(&t, 0, 1);
        assert_eq!(groups.len(), 1);
        assert_eq!(names(&t, &groups[0]), ["a.mkv", "b.mkv"]);
        assert!(candidates(&t, 0, 101).is_empty(), "min size");
    }

    #[test]
    fn regroup_splits_by_key_and_drops_singletons() {
        let groups = regroup(&[1, 2, 3, 4, 5], &[Some(9), Some(8), Some(9), None, Some(8)]);
        assert_eq!(groups, vec![vec![1, 3], vec![2, 5]]);
        assert!(regroup::<u8>(&[1, 2], &[Some(1), Some(2)]).is_empty());
    }

    #[test]
    fn keeper_is_the_oldest_and_wasted_counts_copies() {
        let t = tree();
        let group = candidates(&t, 0, 1).remove(0);
        let keep = keeper(&t, &group).unwrap();
        assert_eq!(t.name(keep), "b.mkv", "older than a.mkv");
        let g = DupGroup { size: 100, ids: group };
        assert_eq!(g.wasted(), 100);
    }
}
