//! Read-only queries over a [`Tree`] used by front-ends.
//!
//! Children are stored in scan order; sorting happens on demand per
//! directory, which is instant even for directories with 100k entries and
//! avoids keeping sorted copies of the whole tree.

use std::cmp::Ordering;

use crate::tree::{NodeId, Tree};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SortKey {
    Name,
    #[default]
    Size,
    Alloc,
    Files,
    Modified,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sort {
    pub key: SortKey,
    pub descending: bool,
}

impl Sort {
    pub const SIZE_DESC: Self = Self { key: SortKey::Size, descending: true };
}

/// Returns the children of `id` sorted by `sort`. With `dirs_only`, files are
/// skipped (used by the navigation tree).
pub fn sorted_children(tree: &Tree, id: NodeId, sort: Sort, dirs_only: bool) -> Vec<NodeId> {
    let mut ids: Vec<NodeId> = tree.children(id).filter(|&c| !dirs_only || tree.node(c).is_dir()).collect();
    sort_ids(tree, &mut ids, sort);
    ids
}

/// Sorts `ids` in place. Ties are broken by name so the order is stable
/// across refreshes.
pub fn sort_ids(tree: &Tree, ids: &mut [NodeId], sort: Sort) {
    let by_name = |a: &NodeId, b: &NodeId| cmp_names(&tree.name(*a), &tree.name(*b));
    let numeric = |f: fn(&Tree, NodeId) -> u64| {
        move |a: &NodeId, b: &NodeId| {
            let o = f(tree, *a).cmp(&f(tree, *b));
            if sort.descending { o.reverse() } else { o }
        }
    };
    match sort.key {
        SortKey::Name => {
            if ids.len() > 64 {
                // Pre-compute lowercase keys once instead of per comparison.
                let mut keyed: Vec<(String, NodeId)> = ids.iter().map(|&i| (tree.name(i).to_lowercase(), i)).collect();
                keyed.sort_unstable_by(|a, b| {
                    let o = a.0.cmp(&b.0);
                    if sort.descending { o.reverse() } else { o }
                });
                for (slot, (_, id)) in ids.iter_mut().zip(keyed) {
                    *slot = id;
                }
            } else {
                ids.sort_unstable_by(|a, b| {
                    let o = by_name(a, b);
                    if sort.descending { o.reverse() } else { o }
                });
            }
        }
        SortKey::Size => ids.sort_unstable_by(|a, b| numeric(|t, i| t.node(i).size)(a, b).then_with(|| by_name(a, b))),
        SortKey::Alloc => {
            ids.sort_unstable_by(|a, b| numeric(|t, i| t.node(i).alloc)(a, b).then_with(|| by_name(a, b)))
        }
        SortKey::Files => {
            ids.sort_unstable_by(|a, b| numeric(|t, i| u64::from(t.counts(i).0))(a, b).then_with(|| by_name(a, b)))
        }
        SortKey::Modified => {
            ids.sort_unstable_by(|a, b| numeric(|t, i| u64::from(t.node(i).mtime))(a, b).then_with(|| by_name(a, b)))
        }
    }
}

fn cmp_names(a: &str, b: &str) -> Ordering {
    let mut ai = a.chars().flat_map(char::to_lowercase);
    let mut bi = b.chars().flat_map(char::to_lowercase);
    loop {
        match (ai.next(), bi.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x != y => return x.cmp(&y),
            _ => {}
        }
    }
}

/// Share of `part` in `whole` in the range `0.0..=1.0`.
#[inline]
pub fn fraction(part: u64, whole: u64) -> f32 {
    if whole == 0 { 0.0 } else { (part as f64 / whole as f64) as f32 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builder::{RawEntry, TreeBuilder};
    use crate::tree::flags;

    fn tree() -> Tree {
        let names = b"bBaAc";
        let mk = |off: u32, size: u64, dir: bool| RawEntry {
            size,
            alloc: size,
            mtime: size as u32,
            name_off: off,
            name_len: 1,
            flags: if dir { flags::DIR } else { 0 },
        };
        let entries = [mk(0, 5, false), mk(1, 50, true), mk(2, 7, false), mk(3, 7, false), mk(4, 1, true)];
        let mut b = TreeBuilder::new("R:\\");
        b.add_listing(0, &entries, names, |_| {}).unwrap();
        b.finish()
    }

    fn names(t: &Tree, ids: &[NodeId]) -> String {
        ids.iter().map(|&i| t.name(i).into_owned()).collect()
    }

    #[test]
    fn sorts_by_size_with_name_tiebreak() {
        let t = tree();
        let ids = sorted_children(&t, 0, Sort::SIZE_DESC, false);
        assert_eq!(names(&t, &ids), "BaAbc");
    }

    #[test]
    fn sorts_by_name_case_insensitive() {
        let t = tree();
        let ids = sorted_children(&t, 0, Sort { key: SortKey::Name, descending: false }, false);
        assert_eq!(names(&t, &ids).to_lowercase(), "aabbc");
    }

    #[test]
    fn dirs_only_filter() {
        let t = tree();
        let ids = sorted_children(&t, 0, Sort::SIZE_DESC, true);
        assert_eq!(names(&t, &ids), "Bc");
    }

    #[test]
    fn fraction_handles_zero() {
        assert_eq!(fraction(5, 0), 0.0);
        assert!((fraction(1, 4) - 0.25).abs() < f32::EPSILON);
    }
}
