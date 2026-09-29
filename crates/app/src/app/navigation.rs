//! Folder navigation, table selection, keyboard.

use super::*;

impl App {
    /// Shows `dir` in the table, selects it in the tree and optionally
    /// selects `select` in the table.
    pub(super) fn navigate_to(&self, dir: NodeId, select: Option<NodeId>) {
        let _timed = crate::diag::Timed::new("navigate_to");
        let Some(tree) = self.state.borrow().tree.clone() else { return };
        let (column, descending) = {
            let mut st = self.state.borrow_mut();
            st.current = dir;
            st.view = View::Folder;
            (st.sort_column, st.sort_descending)
        };
        self.entry_model.show(dir, sort_for_column(column, descending));
        let tree_row = self.tree_model.reveal(dir);

        let Some(ui) = self.ui() else { return };
        ui.set_sort_column(column);
        ui.set_sort_descending(descending);
        ui.set_tree_selected(tree_row.map_or(-1, |r| r as i32));
        ui.set_entry_selected(select.and_then(|id| self.entry_model.row_of(id)).map_or(-1, |r| r as i32));
        self.sync_view_flags();
        self.update_location(&tree);
        self.render_treemap();
    }

    /// Context bar (breadcrumb, prefix) and the totals in the status bar for
    /// the current view.
    pub(super) fn update_location(&self, tree: &Tree) {
        let Some(ui) = self.ui() else { return };
        let (dir, view) = {
            let st = self.state.borrow();
            (st.current, st.view)
        };
        ui.set_can_go_up(view != View::Folder || tree.parent(dir).is_some());

        let (prefix, show_crumbs, info) = match view {
            View::Cleanup => (
                "Bekannte Bereiche mit entbehrlichen Daten – alles landet im Papierkorb".to_string(),
                false,
                String::new(),
            ),
            View::Locations(title) => {
                let total: u64 = (0..self.entry_model.len())
                    .filter_map(|r| self.entry_model.id_at(r))
                    .map(|id| tree.node(id).size)
                    .sum();
                (
                    format!("{title} – {} Orte", self.entry_model.len()),
                    false,
                    format!("Zusammen {}", format::bytes(total)),
                )
            }
            View::Search => {
                let (matches, size) = self.search_summary.get();
                let shown = self.entry_model.len() as u64;
                let mut summary = format!("{} Treffer  ·  zusammen {}", format::count(matches), format::bytes(size));
                if shown < matches {
                    summary.push_str(&format!("  ·  die {} größten angezeigt", format::count(shown)));
                }
                ui.set_search_summary(summary.clone().into());
                ("Suche in".to_string(), true, summary)
            }
            View::Duplicates => {
                let dups = self.dups.borrow();
                let info = if dups.root == Some(dir) && !dups.groups.is_empty() {
                    format!(
                        "{} doppelt in {} Gruppen",
                        format::bytes(dups.groups.iter().map(DupGroup::wasted).sum()),
                        format::count(dups.groups.len() as u64)
                    )
                } else {
                    String::new()
                };
                ("Duplikate in".to_string(), true, info)
            }
            View::Largest => (
                "Größte Dateien in".to_string(),
                true,
                format!(
                    "Die {} größten von {} Dateien",
                    format::count(self.entry_model.len() as u64),
                    format::count(u64::from(tree.counts(dir).0))
                ),
            ),
            View::Folder => {
                let n = tree.node(dir);
                let (files, dirs) = tree.counts(dir);
                (
                    String::new(),
                    true,
                    format!(
                        "{} (belegt {})  ·  {} Dateien  ·  {} Ordner",
                        format::bytes(n.size),
                        format::bytes(n.alloc),
                        format::count(u64::from(files)),
                        format::count(u64::from(dirs))
                    ),
                )
            }
        };
        ui.set_crumb_prefix(prefix.into());
        ui.set_location_info(info.into());
        let crumbs = if show_crumbs { breadcrumb(tree, dir) } else { Vec::new() };
        ui.set_crumbs(ModelRc::new(VecModel::from(crumbs)));
    }

    pub(super) fn crumb_clicked(&self, id: i32) {
        if let Ok(id) = NodeId::try_from(id) {
            self.navigate_to(id, None);
        }
    }

    /// Number of entries listed in the "largest files" view.
    pub(super) const LARGEST_COUNT: usize = 1000;

    pub(super) fn show_largest(&self) {
        let _timed = crate::diag::Timed::new("show_largest");
        let (tree, dir, column, descending) = {
            let mut st = self.state.borrow_mut();
            st.view = View::Largest;
            (st.tree.clone(), st.current, st.sort_column, st.sort_descending)
        };
        let Some(tree) = tree else { return };
        let ids = query::largest_files(&tree, dir, Self::LARGEST_COUNT);
        self.entry_model.show_list(ids, dir, sort_for_column(column, descending));
        if let Some(ui) = self.ui() {
            ui.set_tab(View::Largest.tab());
            ui.set_cleanup_panel(false);
            ui.set_entry_selected(-1);
            ui.invoke_focus_table();
        }
        self.update_location(&tree);
    }

    pub(super) fn show_folder_view(&self) {
        let current = self.state.borrow().current;
        self.navigate_to(current, None);
    }

    pub(super) fn go_up(&self) {
        match self.state.borrow().view {
            View::Folder => {}
            View::Locations(_) => return self.show_cleanup(),
            View::Largest | View::Cleanup | View::Search | View::Duplicates => return self.show_folder_view(),
        }
        let (tree, current) = {
            let st = self.state.borrow();
            (st.tree.clone(), st.current)
        };
        if let Some(parent) = tree.and_then(|t| t.parent(current)) {
            self.navigate_to(parent, Some(current));
        }
    }

    pub(super) fn tree_toggle(&self, row: i32) {
        let _timed = crate::diag::Timed::new("tree_toggle");
        self.tree_model.toggle(row as usize);
        self.sync_tree_selection();
    }

    pub(super) fn tree_select(&self, row: i32) {
        if let Some(id) = self.tree_model.id_at(row as usize) {
            self.navigate_to(id, None);
        }
    }

    /// Re-derives the selected tree row from the current directory after
    /// rows were inserted or removed.
    pub(super) fn sync_tree_selection(&self) {
        let current = self.state.borrow().current;
        if let Some(ui) = self.ui() {
            ui.set_tree_selected(self.tree_model.row_of(current).map_or(-1, |r| r as i32));
        }
    }

    pub(super) fn tree_navigate(&self, key: NavKey) {
        let Some(ui) = self.ui() else { return };
        let len = self.tree_model.len();
        if len == 0 {
            return;
        }
        let cur = usize::try_from(ui.get_tree_selected()).unwrap_or(0);
        let page = usize::try_from(ui.get_tree_page_rows()).unwrap_or(1).max(1);
        let target = match key {
            NavKey::Up => cur.saturating_sub(1),
            NavKey::Down => (cur + 1).min(len - 1),
            NavKey::Home => 0,
            NavKey::End => len - 1,
            NavKey::PageUp => cur.saturating_sub(page),
            NavKey::PageDown => (cur + page).min(len - 1),
            NavKey::Left => {
                if self.tree_model.is_expanded(cur) {
                    self.tree_model.collapse(cur);
                    self.sync_tree_selection();
                    return;
                }
                match self.tree_model.parent_row(cur) {
                    Some(p) => p,
                    None => return,
                }
            }
            NavKey::Right => {
                if !self.tree_model.is_expanded(cur) {
                    self.tree_model.expand(cur);
                    self.sync_tree_selection();
                    return;
                }
                let child = cur + 1;
                if child < len && self.tree_model.depth(child) > self.tree_model.depth(cur) { child } else { return }
            }
            NavKey::Activate => {
                self.tree_toggle(cur as i32);
                return;
            }
            NavKey::Back | NavKey::Delete => return,
        };
        self.tree_select(target as i32);
    }

    /// Plain selection of one row (keyboard, programmatic).
    pub(super) fn entry_select(&self, row: i32) {
        self.entry_click(row, false, false);
    }

    /// Explorer-like selection: click = single, Ctrl = toggle, Shift = range
    /// from the anchor (Ctrl+Shift adds the range).
    pub(super) fn entry_click(&self, row: i32, ctrl: bool, shift: bool) {
        let Some(ui) = self.ui() else { return };
        let Ok(r) = usize::try_from(row) else { return };
        if r >= self.entry_model.len() {
            return;
        }
        if shift {
            let anchor = self.entry_model.anchor().unwrap_or(r);
            self.entry_model.select_range(anchor, r, ctrl);
        } else {
            if ctrl {
                self.entry_model.toggle(r);
            } else {
                self.entry_model.select_range(r, r, false);
            }
            self.entry_model.set_anchor(r);
        }
        ui.set_entry_selected(row);
        self.update_selection_status(r);
    }

    pub(super) fn entry_select_all(&self) {
        self.entry_model.select_all();
        let cursor = self.ui().map_or(0, |ui| ui.get_entry_selected().max(0) as usize);
        self.update_selection_status(cursor);
    }

    /// Status line: details of the row under the cursor, or the sum of a
    /// multi-selection.
    pub(super) fn update_selection_status(&self, cursor: usize) {
        let Some(ui) = self.ui() else { return };
        let Some(tree) = self.state.borrow().tree.clone() else { return };
        let selected = self.entry_model.selected_ids();
        if selected.len() > 1 {
            let size: u64 = query::outermost(&tree, &selected).iter().map(|&id| tree.node(id).size).sum();
            ui.set_status(
                format!(
                    "{} ausgewählt  ·  zusammen {}  ·  Entf verschiebt alle in den Papierkorb",
                    format::count(selected.len() as u64),
                    format::bytes(size)
                )
                .into(),
            );
            return;
        }
        if let Some(id) = self.entry_model.id_at(cursor) {
            let n = tree.node(id);
            let mut info = tree.path(id);
            if n.has(flags::NOT_FOLLOWED) {
                info.push_str("  ·  Verknüpfung (Junction/Symlink) – nicht mitgezählt");
            } else if n.has(flags::ERROR) {
                info.push_str("  ·  Zugriff verweigert oder unvollständig gelesen");
            } else if n.has(flags::HARDLINK) {
                info.push_str("  ·  Weiterer Hardlink – Größe wird beim ersten Link gezählt");
            } else if n.has(flags::CLOUD) {
                info.push_str("  ·  Cloud-Datei (nicht vollständig lokal gespeichert)");
            }
            ui.set_status(info.into());
        }
    }

    pub(super) fn entry_open(&self, row: i32) {
        let tree = self.state.borrow().tree.clone();
        let (Some(tree), Some(id)) = (tree, self.entry_model.id_at(row as usize)) else { return };
        let n = tree.node(id);
        if n.is_dir() && !n.has(flags::NOT_FOLLOWED) {
            self.navigate_to(id, None);
        } else if matches!(self.state.borrow().view, View::Largest | View::Search | View::Duplicates) {
            // From the list, jump to the file in its folder.
            if let Some(parent) = tree.parent(id) {
                self.navigate_to(parent, Some(id));
            }
        } else {
            self.reveal(id);
        }
    }

    pub(super) fn entry_reveal(&self, row: i32) {
        if let Some(id) = self.entry_model.id_at(row as usize) {
            self.reveal(id);
        }
    }

    pub(super) fn entry_navigate(self: &Rc<Self>, key: NavKey, shift: bool) {
        let Some(ui) = self.ui() else { return };
        let len = self.entry_model.len();
        let cur = ui.get_entry_selected();
        let page = ui.get_table_page_rows().max(1);
        let last = len as i32 - 1;
        let target = match key {
            NavKey::Up => (cur - 1).max(0),
            NavKey::Down => (cur + 1).min(last),
            NavKey::Home => 0,
            NavKey::End => last,
            NavKey::PageUp => (cur - page).max(0),
            NavKey::PageDown => (cur + page).min(last),
            NavKey::Activate => {
                if cur >= 0 {
                    self.entry_open(cur);
                }
                return;
            }
            NavKey::Back | NavKey::Left => {
                self.go_up();
                return;
            }
            NavKey::Right => return,
            NavKey::Delete => {
                if cur >= 0 {
                    self.delete_row(cur);
                }
                return;
            }
        };
        if len > 0 {
            self.entry_click(target, false, shift);
        }
    }

    pub(super) fn sort_by(&self, column: i32) {
        let _timed = crate::diag::Timed::new("sort_by");
        if self.state.borrow().view == View::Duplicates {
            return self.set_status("In der Duplikat-Ansicht bleiben die Gruppen zusammen – Sortieren ist hier aus.");
        }
        let Some(ui) = self.ui() else { return };
        let selected = self.entry_model.id_at(ui.get_entry_selected().max(0) as usize);
        let (column, descending) = {
            let mut st = self.state.borrow_mut();
            if st.sort_column == column {
                st.sort_descending = !st.sort_descending;
            } else {
                st.sort_column = column;
                st.sort_descending = column != COL_NAME;
            }
            (st.sort_column, st.sort_descending)
        };
        self.entry_model.resort(sort_for_column(column, descending));
        ui.set_sort_column(column);
        ui.set_sort_descending(descending);
        if ui.get_entry_selected() >= 0 {
            ui.set_entry_selected(selected.and_then(|id| self.entry_model.row_of(id)).map_or(-1, |r| r as i32));
        }
    }

    pub(super) fn reveal_current(&self) {
        let Some(ui) = self.ui() else { return };
        let id = self.entry_model.id_at(ui.get_entry_selected().max(0) as usize);
        let id = if ui.get_entry_selected() >= 0 { id } else { None };
        let current = self.state.borrow().current;
        self.reveal(id.unwrap_or(current));
    }

    pub(super) fn reveal(&self, id: NodeId) {
        let Some(tree) = self.state.borrow().tree.clone() else { return };
        if let Err(e) = shell::reveal_in_explorer(&tree.path(id)) {
            self.set_status(format!("Explorer konnte nicht geöffnet werden: {e}"));
        }
    }
}

/// Breadcrumb segments from the scan root to `dir`. Deep paths keep the root
/// and the last levels, with "…" (pointing to the hidden ancestor) between.
fn breadcrumb(tree: &Tree, dir: NodeId) -> Vec<Crumb> {
    const KEEP_LAST: usize = 4;
    let chain = tree.ancestry(dir);
    let crumb = |id: NodeId| Crumb {
        text: if id == Tree::ROOT {
            tree.root_path().trim_end_matches('\\').into()
        } else {
            tree.name(id).as_ref().into()
        },
        id: id as i32,
    };
    if chain.len() <= KEEP_LAST + 2 {
        return chain.into_iter().map(crumb).collect();
    }
    let hidden = chain[chain.len() - KEEP_LAST - 1];
    let mut out = vec![crumb(chain[0]), Crumb { text: "…".into(), id: hidden as i32 }];
    out.extend(chain[chain.len() - KEEP_LAST..].iter().map(|&id| crumb(id)));
    out
}
