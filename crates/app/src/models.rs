//! Lazy Slint models over a shared [`Tree`].
//!
//! Both models store only node ids; `row_data` formats a row when Slint asks
//! for it, i.e. only for the ~50 rows that are actually visible. Nothing is
//! copied or pre-formatted per entry, so showing a directory with 100 000
//! children costs one `Vec<u32>` and one sort.

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use slint::{Model, ModelNotify, ModelTracker, SharedString};
use spacelens_core::query::{self, Sort, SortKey};
use spacelens_core::{NodeId, Tree, flags, format};

use crate::{EntryRow, TreeRow};

// ---------------------------------------------------------------------------
// Folder tree
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
struct FlatRow {
    id: NodeId,
    depth: u16,
    expanded: bool,
}

/// Flattened, lazily expanded directory tree (directories only).
#[derive(Default)]
pub struct TreeModel {
    tree: RefCell<Option<Arc<Tree>>>,
    rows: RefCell<Vec<FlatRow>>,
    notify: ModelNotify,
}

impl TreeModel {
    pub fn set_tree(&self, tree: Option<Arc<Tree>>) {
        let mut rows = self.rows.borrow_mut();
        rows.clear();
        if tree.is_some() {
            rows.push(FlatRow { id: Tree::ROOT, depth: 0, expanded: false });
        }
        drop(rows);
        *self.tree.borrow_mut() = tree;
        self.notify.reset();
        self.expand(0);
    }

    /// Hands out the tree so the caller can mutate it (see [`Self::put_tree`]).
    pub fn take_tree(&self) -> Option<Arc<Tree>> {
        self.tree.borrow_mut().take()
    }

    /// Re-attaches a (modified) tree. Rows of removed directories and their
    /// expanded descendants disappear; the expansion state of the rest is kept.
    pub fn put_tree(&self, tree: Arc<Tree>) {
        let mut rows = self.rows.borrow_mut();
        let mut kept = Vec::with_capacity(rows.len());
        let mut skip_below: Option<u16> = None;
        for r in rows.drain(..) {
            if let Some(depth) = skip_below {
                if r.depth > depth {
                    continue;
                }
                skip_below = None;
            }
            if tree.node(r.id).has(flags::DELETED) {
                skip_below = Some(r.depth);
                continue;
            }
            kept.push(r);
        }
        *rows = kept;
        drop(rows);
        *self.tree.borrow_mut() = Some(tree);
        self.notify.reset();
    }

    pub fn id_at(&self, row: usize) -> Option<NodeId> {
        self.rows.borrow().get(row).map(|r| r.id)
    }

    pub fn len(&self) -> usize {
        self.rows.borrow().len()
    }

    pub fn is_expanded(&self, row: usize) -> bool {
        self.rows.borrow().get(row).is_some_and(|r| r.expanded)
    }

    pub fn toggle(&self, row: usize) {
        if self.is_expanded(row) { self.collapse(row) } else { self.expand(row) }
    }

    pub fn expand(&self, row: usize) {
        let Some(tree) = self.tree.borrow().clone() else { return };
        let mut rows = self.rows.borrow_mut();
        let Some(r) = rows.get(row).copied() else { return };
        if r.expanded {
            return;
        }
        let children = query::sorted_children(&tree, r.id, Sort::SIZE_DESC, true);
        rows[row].expanded = true;
        let depth = r.depth.saturating_add(1);
        let count = children.len();
        rows.splice(row + 1..row + 1, children.into_iter().map(|id| FlatRow { id, depth, expanded: false }));
        drop(rows);
        self.notify.row_changed(row);
        if count > 0 {
            self.notify.row_added(row + 1, count);
        }
    }

    pub fn collapse(&self, row: usize) {
        let mut rows = self.rows.borrow_mut();
        let Some(r) = rows.get(row).copied() else { return };
        if !r.expanded {
            return;
        }
        rows[row].expanded = false;
        let end = rows[row + 1..].iter().position(|c| c.depth <= r.depth).map_or(rows.len(), |p| row + 1 + p);
        rows.drain(row + 1..end);
        drop(rows);
        self.notify.row_changed(row);
        if end > row + 1 {
            self.notify.row_removed(row + 1, end - row - 1);
        }
    }

    pub fn row_of(&self, id: NodeId) -> Option<usize> {
        self.rows.borrow().iter().position(|r| r.id == id)
    }

    pub fn depth(&self, row: usize) -> Option<u16> {
        self.rows.borrow().get(row).map(|r| r.depth)
    }

    /// Row of the parent of `row` (for "left" navigation).
    pub fn parent_row(&self, row: usize) -> Option<usize> {
        let rows = self.rows.borrow();
        let depth = rows.get(row)?.depth;
        rows[..row].iter().rposition(|r| r.depth < depth)
    }

    /// Expands all ancestors of `id` and returns its row.
    pub fn reveal(&self, id: NodeId) -> Option<usize> {
        let tree = self.tree.borrow().clone()?;
        let chain = tree.ancestry(id);
        let mut row = 0usize;
        for (depth, &node) in chain.iter().enumerate().skip(1) {
            self.expand(row);
            let rows = self.rows.borrow();
            row = rows[row + 1..]
                .iter()
                .take_while(|r| r.depth as usize >= depth)
                .position(|r| r.id == node && r.depth as usize == depth)
                .map(|p| row + 1 + p)?;
        }
        Some(row)
    }
}

impl Model for TreeModel {
    type Data = TreeRow;

    fn row_count(&self) -> usize {
        self.rows.borrow().len()
    }

    fn row_data(&self, row: usize) -> Option<TreeRow> {
        let tree = self.tree.borrow();
        let tree = tree.as_ref()?;
        let r = *self.rows.borrow().get(row)?;
        let n = tree.node(r.id);
        let parent_size = tree.parent(r.id).map_or(n.size, |p| tree.node(p).size);
        let has_subdirs = tree.counts(r.id).1 > 0;
        Some(TreeRow {
            id: r.id as i32,
            depth: i32::from(r.depth),
            name: SharedString::from(&*tree.name(r.id)),
            size: format::bytes(n.size).into(),
            fraction: query::fraction(n.size, parent_size),
            expandable: has_subdirs,
            expanded: r.expanded,
            error: n.has(flags::ERROR),
            link: n.has(flags::NOT_FOLLOWED),
        })
    }

    fn model_tracker(&self) -> &dyn ModelTracker {
        &self.notify
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

// ---------------------------------------------------------------------------
// Directory contents
// ---------------------------------------------------------------------------

/// Column ids of the contents table (must match `table.slint`).
pub const COL_NAME: i32 = 0;
pub const COL_SIZE: i32 = 1;
pub const COL_ALLOC: i32 = 2;
pub const COL_SHARE: i32 = 3;
pub const COL_FILES: i32 = 4;
pub const COL_MODIFIED: i32 = 5;

/// Stable names for persisting the sort column (independent of UI order).
const COLUMN_KEYS: [(i32, &str); 6] = [
    (COL_NAME, "name"),
    (COL_SIZE, "size"),
    (COL_ALLOC, "alloc"),
    (COL_SHARE, "share"),
    (COL_FILES, "files"),
    (COL_MODIFIED, "modified"),
];

pub fn column_key(column: i32) -> &'static str {
    COLUMN_KEYS.iter().find(|(c, _)| *c == column).map_or("size", |(_, k)| k)
}

pub fn column_from_key(key: &str) -> i32 {
    COLUMN_KEYS.iter().find(|(_, k)| *k == key).map_or(COL_SIZE, |(c, _)| *c)
}

pub fn sort_for_column(column: i32, descending: bool) -> Sort {
    let key = match column {
        COL_NAME => SortKey::Name,
        COL_ALLOC => SortKey::Alloc,
        COL_FILES => SortKey::Files,
        COL_MODIFIED => SortKey::Modified,
        COL_SIZE | COL_SHARE => SortKey::Size,
        _ => SortKey::Size,
    };
    Sort { key, descending }
}

/// Children of one directory, sorted.
#[derive(Default)]
pub struct EntryModel {
    tree: RefCell<Option<Arc<Tree>>>,
    /// Directory shown, or the reference for the share column in list mode.
    dir: Cell<NodeId>,
    /// Flat list (e.g. largest files) showing full paths instead of names.
    list_mode: Cell<bool>,
    rows: RefCell<Vec<NodeId>>,
    /// Multi-selection (node ids, independent of sorting).
    selection: RefCell<HashSet<NodeId>>,
    /// Row where a Shift range starts (last plain or Ctrl click).
    anchor: Cell<Option<usize>>,
    /// Duplicate-group index per node (duplicates view only).
    groups: RefCell<HashMap<NodeId, u32>>,
    notify: ModelNotify,
}

impl EntryModel {
    pub fn set_tree(&self, tree: Option<Arc<Tree>>) {
        *self.tree.borrow_mut() = tree;
        self.rows.borrow_mut().clear();
        self.selection.borrow_mut().clear();
        self.groups.borrow_mut().clear();
        self.anchor.set(None);
        self.notify.reset();
    }

    pub fn show(&self, dir: NodeId, sort: Sort) {
        let Some(tree) = self.tree.borrow().clone() else { return };
        self.dir.set(dir);
        self.list_mode.set(false);
        self.groups.borrow_mut().clear();
        self.selection.borrow_mut().clear();
        self.anchor.set(None);
        *self.rows.borrow_mut() = query::sorted_children(&tree, dir, sort, false);
        self.notify.reset();
    }

    /// Shows an arbitrary list of nodes with full paths; shares are relative to `base`.
    pub fn show_list(&self, ids: Vec<NodeId>, base: NodeId, sort: Sort) {
        let Some(tree) = self.tree.borrow().clone() else { return };
        self.dir.set(base);
        self.list_mode.set(true);
        self.groups.borrow_mut().clear();
        self.selection.borrow_mut().clear();
        self.anchor.set(None);
        let mut ids = ids;
        query::sort_ids(&tree, &mut ids, sort);
        *self.rows.borrow_mut() = ids;
        self.notify.reset();
    }

    /// Shows duplicate groups in the given order (no sorting: groups stay
    /// together); shares are relative to `base`.
    pub fn show_groups(&self, groups: &[Vec<NodeId>], base: NodeId) {
        self.dir.set(base);
        self.list_mode.set(true);
        self.selection.borrow_mut().clear();
        self.anchor.set(None);
        let mut map = HashMap::new();
        let mut rows = Vec::new();
        for (g, ids) in groups.iter().enumerate() {
            for &id in ids {
                map.insert(id, g as u32);
                rows.push(id);
            }
        }
        *self.groups.borrow_mut() = map;
        *self.rows.borrow_mut() = rows;
        self.notify.reset();
    }

    /// Replaces the selection with `ids`.
    pub fn select_ids(&self, ids: &[NodeId]) {
        *self.selection.borrow_mut() = ids.iter().copied().collect();
        self.notify.reset();
    }

    /// Hands out the tree so the caller can mutate it (see [`Self::put_tree`]).
    pub fn take_tree(&self) -> Option<Arc<Tree>> {
        self.tree.borrow_mut().take()
    }

    /// Re-attaches a (modified) tree, dropping rows that were removed.
    pub fn put_tree(&self, tree: Arc<Tree>) {
        self.rows.borrow_mut().retain(|&id| !tree.node(id).has(flags::DELETED));
        self.selection.borrow_mut().retain(|&id| !tree.node(id).has(flags::DELETED));
        *self.tree.borrow_mut() = Some(tree);
        self.notify.reset();
    }

    pub fn resort(&self, sort: Sort) {
        let Some(tree) = self.tree.borrow().clone() else { return };
        query::sort_ids(&tree, &mut self.rows.borrow_mut(), sort);
        self.notify.reset();
    }

    pub fn id_at(&self, row: usize) -> Option<NodeId> {
        self.rows.borrow().get(row).copied()
    }

    pub fn row_of(&self, id: NodeId) -> Option<usize> {
        self.rows.borrow().iter().position(|&r| r == id)
    }

    pub fn len(&self) -> usize {
        self.rows.borrow().len()
    }

    // ----- selection -----

    pub fn anchor(&self) -> Option<usize> {
        self.anchor.get().filter(|&a| a < self.len())
    }

    pub fn set_anchor(&self, row: usize) {
        self.anchor.set(Some(row));
    }

    /// Replaces the selection with the rows in `range` (inclusive, any order).
    pub fn select_range(&self, a: usize, b: usize, add: bool) {
        let (lo, hi) = (a.min(b), a.max(b));
        {
            let rows = self.rows.borrow();
            let mut sel = self.selection.borrow_mut();
            if !add {
                sel.clear();
            }
            sel.extend(rows.iter().take(hi + 1).skip(lo).copied());
        }
        self.notify.reset();
    }

    pub fn toggle(&self, row: usize) {
        let Some(id) = self.id_at(row) else { return };
        {
            let mut sel = self.selection.borrow_mut();
            if !sel.remove(&id) {
                sel.insert(id);
            }
        }
        self.notify.row_changed(row);
    }

    pub fn select_all(&self) {
        let all: HashSet<NodeId> = self.rows.borrow().iter().copied().collect();
        *self.selection.borrow_mut() = all;
        self.notify.reset();
    }

    /// Selected ids in display order.
    pub fn selected_ids(&self) -> Vec<NodeId> {
        let sel = self.selection.borrow();
        self.rows.borrow().iter().copied().filter(|id| sel.contains(id)).collect()
    }
}

impl Model for EntryModel {
    type Data = EntryRow;

    fn row_count(&self) -> usize {
        self.rows.borrow().len()
    }

    fn row_data(&self, row: usize) -> Option<EntryRow> {
        let tree = self.tree.borrow();
        let tree = tree.as_ref()?;
        let id = *self.rows.borrow().get(row)?;
        let n = tree.node(id);
        let parent_size = tree.node(self.dir.get()).size;
        let fraction = query::fraction(n.size, parent_size);
        let is_dir = n.is_dir();
        Some(EntryRow {
            id: id as i32,
            name: if self.list_mode.get() {
                relative_path(tree, self.dir.get(), id).into()
            } else {
                SharedString::from(&*tree.name(id))
            },
            is_dir,
            size: format::bytes(n.size).into(),
            alloc: format::bytes(n.alloc).into(),
            fraction,
            percent: format::percent(fraction).into(),
            files: if is_dir { format::count(u64::from(tree.counts(id).0)).into() } else { SharedString::new() },
            modified: format::unix_time(n.mtime).into(),
            error: n.has(flags::ERROR),
            link: n.has(flags::NOT_FOLLOWED),
            dim: n.has(flags::HIDDEN) || n.has(flags::SYSTEM),
            selected: self.selection.borrow().contains(&id),
            group_tint: self.groups.borrow().get(&id).is_some_and(|g| g % 2 == 1),
        })
    }

    fn model_tracker(&self) -> &dyn ModelTracker {
        &self.notify
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Path of `id` relative to its ancestor `base`, e.g. `Spiele\Alt\game.pak`.
/// Name first, then the folder relative to `base`, e.g.
/// `game.pak  —  Spiele\Alt`. Long paths get cut at the end, so the part
/// that identifies the entry must come first.
fn relative_path(tree: &Tree, base: NodeId, id: NodeId) -> String {
    let chain = tree.ancestry(id);
    let start = chain.iter().position(|&n| n == base).map_or(0, |p| p + 1);
    let mut out = tree.name(id).into_owned();
    let folders = &chain[start.min(chain.len())..chain.len().saturating_sub(1)];
    if !folders.is_empty() {
        out.push_str("  —  ");
        for (i, &n) in folders.iter().enumerate() {
            if i > 0 {
                out.push('\\');
            }
            out.push_str(&tree.name(n));
        }
    }
    out
}
