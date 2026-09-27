//! Search and filter over a scanned tree.
//!
//! Runs directly on the in-memory tree: one pass over the subtree, names are
//! compared as bytes without allocating. Fast enough to run on every
//! keystroke (≈600 000 entries in a few ms, see the benchmark in the tests
//! of `spacelens-win`).

use crate::filetype::{self, Category};
use crate::tree::{NodeId, Tree, flags};

/// What kind of entries to return.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Kind {
    /// Files and folders (folders only match by name).
    #[default]
    All,
    Files,
    Folders,
}

/// A search request. The default matches every file and folder.
#[derive(Clone, Debug, Default)]
pub struct Filter {
    /// Whitespace-separated terms; all must match the name. A term with `*`
    /// or `?` is a wildcard pattern for the whole name (`*.mp4`), otherwise a
    /// case-insensitive substring.
    pub query: String,
    pub kind: Kind,
    /// Only files of this category (implies files).
    pub category: Option<Category>,
    /// Minimum size in bytes (0 = any).
    pub min_size: u64,
    /// Only entries last modified before this Unix time (0 = any).
    pub modified_before: u32,
}

impl Filter {
    /// True if only the default "everything" filter is set.
    pub fn is_empty(&self) -> bool {
        self.query.trim().is_empty()
            && self.kind == Kind::All
            && self.category.is_none()
            && self.min_size == 0
            && self.modified_before == 0
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchResult {
    /// The largest matches (at most `limit`), largest first.
    pub ids: Vec<NodeId>,
    /// Number of all matches (may exceed `ids.len()`).
    pub matches: u64,
    /// Total size of all matches. Matches inside a matching folder are not
    /// counted again.
    pub size: u64,
}

/// One compiled query term.
enum Term {
    /// Lower-cased substring.
    Contains(Vec<u8>),
    /// Lower-cased wildcard pattern over the whole name.
    Glob(Vec<u8>),
    /// `*literal` – the most common pattern (`*.iso`), checked without backtracking.
    Suffix(Vec<u8>),
    /// `literal*` (`IMG_*`).
    Prefix(Vec<u8>),
}

fn compile(query: &str) -> Vec<Term> {
    query
        .split_whitespace()
        .map(|t| {
            let lower = t.to_lowercase().into_bytes();
            let wild = |b: &u8| *b == b'*' || *b == b'?';
            match lower.split_first() {
                Some((b'*', rest)) if !rest.is_empty() && !rest.iter().any(wild) => Term::Suffix(rest.to_vec()),
                _ if lower.last() == Some(&b'*') && lower.len() > 1 && !lower[..lower.len() - 1].iter().any(wild) => {
                    Term::Prefix(lower[..lower.len() - 1].to_vec())
                }
                _ if lower.iter().any(wild) => Term::Glob(lower),
                _ => Term::Contains(lower),
            }
        })
        .collect()
}

/// Lower-cases ASCII in place; non-ASCII names take the Unicode path.
fn lowered<'a>(name: &[u8], buf: &'a mut Vec<u8>) -> &'a [u8] {
    buf.clear();
    if name.is_ascii() {
        buf.extend(name.iter().map(u8::to_ascii_lowercase));
    } else {
        buf.extend_from_slice(String::from_utf8_lossy(name).to_lowercase().as_bytes());
    }
    buf
}

/// Substring test. With `fold`, `hay` is compared ASCII-case-insensitively
/// (the needle is already lower-case), avoiding a lower-cased copy per name.
fn contains(hay: &[u8], needle: &[u8], fold: bool) -> bool {
    let Some(&first) = needle.first() else { return true };
    if hay.len() < needle.len() {
        return false;
    }
    let norm = |b: u8| if fold { b.to_ascii_lowercase() } else { b };
    (0..=hay.len() - needle.len()).any(|i| {
        norm(hay[i]) == first && hay[i + 1..i + needle.len()].iter().zip(&needle[1..]).all(|(&h, &n)| norm(h) == n)
    })
}

/// Wildcard match: `*` = any sequence, `?` = any single byte.
/// Iterative with backtracking to the last `*` (linear in practice).
/// `fold` as in [`contains`].
fn glob(name: &[u8], pat: &[u8], fold: bool) -> bool {
    let norm = |b: u8| if fold { b.to_ascii_lowercase() } else { b };
    let (mut n, mut p) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while n < name.len() {
        if p < pat.len() && (pat[p] == b'?' || pat[p] == norm(name[n])) {
            n += 1;
            p += 1;
        } else if p < pat.len() && pat[p] == b'*' {
            star = Some(p);
            mark = n;
            p += 1;
        } else if let Some(s) = star {
            p = s + 1;
            mark += 1;
            n = mark;
        } else {
            return false;
        }
    }
    pat[p..].iter().all(|&c| c == b'*')
}

/// All terms must match. ASCII names (the vast majority) are compared in
/// place; others are lower-cased with full Unicode rules first.
fn name_matches(raw: &[u8], terms: &[Term], buf: &mut Vec<u8>) -> bool {
    let (name, fold) = if raw.is_ascii() { (raw, true) } else { (lowered(raw, buf), false) };
    terms.iter().all(|t| match t {
        Term::Contains(s) => contains(name, s, fold),
        Term::Glob(g) => glob(name, g, fold),
        Term::Suffix(s) => name.len() >= s.len() && eq_fold(&name[name.len() - s.len()..], s, fold),
        Term::Prefix(p) => name.len() >= p.len() && eq_fold(&name[..p.len()], p, fold),
    })
}

fn eq_fold(a: &[u8], lower: &[u8], fold: bool) -> bool {
    if fold { a.eq_ignore_ascii_case(lower) } else { a == lower }
}

/// Searches the subtree below `root` (excluding `root` itself) and returns
/// the `limit` largest matches.
pub fn search(tree: &Tree, root: NodeId, filter: &Filter, limit: usize) -> SearchResult {
    use std::cmp::Reverse;
    use std::collections::BinaryHeap;

    let terms = compile(&filter.query);
    let has_name_terms = !terms.is_empty();
    let files_only = filter.kind == Kind::Files || filter.category.is_some();
    let folders_only = filter.kind == Kind::Folders;
    let mut buf = Vec::with_capacity(64);
    let mut heap = BinaryHeap::with_capacity(limit + 1);
    let mut result = SearchResult::default();

    let mut stack: Vec<NodeId> = tree.live_children(root).collect();
    while let Some(id) = stack.pop() {
        let node = tree.node(id);
        let is_dir = node.is_dir();

        // Folders match only when named explicitly or requested explicitly;
        // otherwise every folder would be a "match" of the empty query.
        let kind_ok = if is_dir {
            !files_only && (folders_only || has_name_terms)
        } else {
            !folders_only && filter.category.is_none_or(|c| filetype::category_of(tree, id) == c)
        };
        // Cheap checks first; the name comparison only runs for candidates.
        let is_match = kind_ok
            && node.size >= filter.min_size
            && (filter.modified_before == 0 || (node.mtime != 0 && node.mtime < filter.modified_before))
            && !node.has(flags::HARDLINK)
            && (!has_name_terms || name_matches(tree.name_bytes(id), &terms, &mut buf));

        if is_match {
            result.matches += 1;
            result.size += node.size;
            heap.push(Reverse((node.size, id)));
            if heap.len() > limit {
                heap.pop();
            }
            if is_dir {
                continue; // its contents are part of this match already
            }
        }
        if is_dir && !node.has(flags::NOT_FOLLOWED) {
            stack.extend(tree.live_children(id));
        }
    }

    let mut top: Vec<(u64, NodeId)> = heap.into_iter().map(|Reverse(x)| x).collect();
    top.sort_unstable_by(|a, b| b.cmp(a));
    result.ids = top.into_iter().map(|(_, id)| id).collect();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builder::{RawEntry, TreeBuilder};

    /// root: Videos/ (urlaub 2023.mp4 900, Urlaub 2024.MP4 500, clip.mov 50)
    ///       node_modules/ (a.js 10, node_modules/ (b.js 5))
    ///       Übergröße.iso 700, notes.txt 3
    fn tree() -> Tree {
        let mut names = Vec::new();
        let mut e = |name: &str, size: u64, dir: bool, mtime: u32| {
            let off = names.len() as u32;
            names.extend_from_slice(name.as_bytes());
            RawEntry {
                size,
                alloc: size,
                mtime,
                name_off: off,
                name_len: name.len() as u16,
                flags: if dir { flags::DIR } else { 0 },
            }
        };
        let root = [
            e("Videos", 0, true, 0),
            e("node_modules", 0, true, 0),
            e("Übergröße.iso", 700, false, 100),
            e("notes.txt", 3, false, 5000),
        ];
        let videos = [
            e("urlaub 2023.mp4", 900, false, 100),
            e("Urlaub 2024.MP4", 500, false, 5000),
            e("clip.mov", 50, false, 100),
        ];
        let nm = [e("a.js", 10, false, 100), e("node_modules", 0, true, 100)];
        let nm2 = [e("b.js", 5, false, 100)];
        let mut b = TreeBuilder::new("X:\\");
        let mut dirs = Vec::new();
        b.add_listing(0, &root, &names, |id| dirs.push(id)).unwrap();
        b.add_listing(dirs[0], &videos, &names, |_| {}).unwrap();
        let mut inner = Vec::new();
        b.add_listing(dirs[1], &nm, &names, |id| inner.push(id)).unwrap();
        b.add_listing(inner[0], &nm2, &names, |_| {}).unwrap();
        b.finish()
    }

    fn names(t: &Tree, r: &SearchResult) -> Vec<String> {
        r.ids.iter().map(|&i| t.name(i).into_owned()).collect()
    }

    fn run(t: &Tree, f: Filter) -> SearchResult {
        search(t, 0, &f, 100)
    }

    #[test]
    fn empty_filter_lists_files_largest_first() {
        let t = tree();
        let r = run(&t, Filter::default());
        assert_eq!(r.matches, 7);
        assert_eq!(names(&t, &r)[..3], ["urlaub 2023.mp4", "Übergröße.iso", "Urlaub 2024.MP4"]);
        assert!(Filter::default().is_empty());
    }

    #[test]
    fn terms_are_case_insensitive_and_combined() {
        let t = tree();
        let r = run(&t, Filter { query: "URLAUB 2024".into(), ..Filter::default() });
        assert_eq!(names(&t, &r), ["Urlaub 2024.MP4"]);
        let r = run(&t, Filter { query: "übergrö".into(), ..Filter::default() });
        assert_eq!(names(&t, &r), ["Übergröße.iso"]);
    }

    #[test]
    fn wildcards_match_whole_names() {
        let t = tree();
        let r = run(&t, Filter { query: "*.mp4".into(), ..Filter::default() });
        assert_eq!(r.matches, 2);
        assert_eq!(r.size, 1400);
        let r = run(&t, Filter { query: "?.js".into(), ..Filter::default() });
        assert_eq!(r.matches, 2);
        let r = run(&t, Filter { query: "URLAUB*".into(), ..Filter::default() });
        assert_eq!(r.matches, 2, "prefix pattern");
        let r = run(&t, Filter { query: "*.MOV".into(), ..Filter::default() });
        assert_eq!(names(&t, &r), ["clip.mov"], "suffix pattern");
        assert!(glob(b"abc", b"a*c", false) && glob(b"AC", b"a*c", true) && !glob(b"ab", b"a*c", false));
        assert!(glob(b"x", b"*", false) && contains(b"MyFile", b"file", true) && !contains(b"MyFile", b"file", false));
    }

    #[test]
    fn matching_folders_are_counted_once() {
        let t = tree();
        let r = run(&t, Filter { query: "node_modules".into(), ..Filter::default() });
        assert_eq!(r.matches, 1, "inner node_modules is part of the outer match");
        assert_eq!(r.size, 15);
    }

    #[test]
    fn filters_by_category_size_age_and_kind() {
        let t = tree();
        let r = run(&t, Filter { category: Some(Category::Video), ..Filter::default() });
        assert_eq!(r.matches, 3);
        let r = run(&t, Filter { category: Some(Category::Video), min_size: 100, ..Filter::default() });
        assert_eq!(r.matches, 2);
        let r = run(&t, Filter { modified_before: 1000, min_size: 500, ..Filter::default() });
        assert_eq!(names(&t, &r), ["urlaub 2023.mp4", "Übergröße.iso"]);
        let r = run(&t, Filter { kind: Kind::Folders, ..Filter::default() });
        assert_eq!(names(&t, &r).len(), 2, "Videos and the outer node_modules");
    }

    #[test]
    fn limit_keeps_the_largest() {
        let t = tree();
        let r = search(&t, 0, &Filter::default(), 2);
        assert_eq!(r.ids.len(), 2);
        assert_eq!(r.matches, 7);
        assert_eq!(names(&t, &r), ["urlaub 2023.mp4", "Übergröße.iso"]);
    }
}
