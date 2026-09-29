//! Moving items to the recycle bin.

use super::*;

impl App {
    pub(super) fn delete_selected(self: &Rc<Self>) {
        let row = self.ui().map_or(-1, |ui| ui.get_entry_selected());
        if row >= 0 {
            self.delete_row(row);
        }
    }

    pub(super) fn entry_delete(self: &Rc<Self>, row: i32) {
        self.delete_row(row);
    }

    /// Guard check → confirmation → recycle on a worker thread.
    pub(super) fn delete_row(self: &Rc<Self>, row: i32) {
        {
            let st = self.state.borrow();
            if st.job.is_some() || st.delete_job.is_some() || st.dup_job.is_some() {
                drop(st);
                self.set_status("Bitte warten, bis der laufende Vorgang abgeschlossen ist.");
                return;
            }
        }
        let tree = self.state.borrow().tree.clone();
        let (Some(tree), Some(id)) = (tree, self.entry_model.id_at(row.max(0) as usize)) else { return };

        // Explorer semantics: acting on a selected row acts on the whole
        // selection; acting on an unselected row acts on that row only.
        let selected = self.entry_model.selected_ids();
        if selected.len() > 1 && selected.contains(&id) {
            return self.delete_many(&tree, &selected, row);
        }

        let path = tree.path(id);
        let guard = safety::guard(&tree, id);
        if let Some(Guard::Blocked(why)) = guard {
            shell::message_box("SpaceLens – Nicht löschbar", &format!("{path}\n\n{why}"), false);
            return;
        }

        let node = tree.node(id);
        let what = if node.is_dir() {
            let (files, dirs) = tree.counts(id);
            format!(
                "Ordner mit {} Dateien in {} Unterordnern",
                format::count(u64::from(files)),
                format::count(u64::from(dirs))
            )
        } else {
            "Datei".to_string()
        };
        let mut text = format!("In den Papierkorb verschieben?\n\n{path}\n{what}  ·  {}", format::bytes(node.size));
        if node.has(flags::HARDLINK) {
            text.push_str("\n\nHinweis: Hardlink – Speicher wird erst frei, wenn alle Links entfernt sind.");
        }
        if let Some(Guard::Warn(warning)) = guard {
            text.push_str("\n\n⚠ ");
            text.push_str(warning);
        }
        let owner = self.owner_hwnd();
        if !shell::confirm(owner, "SpaceLens – In den Papierkorb", &text, guard.is_some()) {
            return;
        }

        self.start_delete_job(vec![(id, path)], row, None, false);
    }

    /// Several items: all-or-nothing guard check, one summary confirmation.
    pub(super) fn delete_many(self: &Rc<Self>, tree: &Tree, selected: &[NodeId], row: i32) {
        const SHOWN: usize = 8;
        let ids = query::outermost(tree, selected);

        let blocked: Vec<String> = ids
            .iter()
            .filter_map(|&id| match safety::guard(tree, id) {
                Some(Guard::Blocked(why)) => Some(format!("{}\n    {why}", tree.path(id))),
                _ => None,
            })
            .collect();
        if !blocked.is_empty() {
            let mut text = format!(
                "{} der ausgewählten Einträge dürfen nicht gelöscht werden – es wurde nichts verschoben.\n\n",
                blocked.len()
            );
            text.push_str(&blocked.iter().take(SHOWN).cloned().collect::<Vec<_>>().join("\n"));
            if blocked.len() > SHOWN {
                text.push_str(&format!("\n… und {} weitere", blocked.len() - SHOWN));
            }
            text.push_str("\n\nBitte diese Einträge aus der Auswahl nehmen (Strg + Klick).");
            shell::message_box("SpaceLens – Nicht löschbar", &text, false);
            return;
        }

        let warn = ids.iter().any(|&id| matches!(safety::guard(tree, id), Some(Guard::Warn(_))));
        let size: u64 = ids.iter().map(|&id| tree.node(id).size).sum();
        let files: u64 =
            ids.iter().map(|&id| if tree.node(id).is_dir() { u64::from(tree.counts(id).0) } else { 1 }).sum();
        let mut text = format!("{} Einträge in den Papierkorb verschieben?\n\n", format::count(ids.len() as u64));
        for &id in ids.iter().take(SHOWN) {
            text.push_str(&tree.path(id));
            text.push('\n');
        }
        if ids.len() > SHOWN {
            text.push_str(&format!("… und {} weitere\n", format::count((ids.len() - SHOWN) as u64)));
        }
        text.push_str(&format!("\nZusammen {} in {} Dateien.", format::bytes(size), format::count(files)));
        if warn {
            text.push_str(
                "\n\n⚠ Einige Einträge gehören zu Windows oder installierten Programmen. Löschen kann diese beschädigen.",
            );
        }
        if !shell::confirm(self.owner_hwnd(), "SpaceLens – In den Papierkorb", &text, warn) {
            return;
        }
        let items: Vec<(NodeId, String)> = ids.iter().map(|&id| (id, tree.path(id))).collect();
        self.start_delete_job(items, row, None, false);
    }

    pub(super) fn poll_delete(&self) {
        let finished = self.state.borrow().delete_job.as_ref().is_none_or(|j| j.handle.is_finished());
        if !finished {
            return;
        }
        self.delete_timer.stop();
        let Some(job) = self.state.borrow_mut().delete_job.take() else { return };
        if let Some(ui) = self.ui() {
            ui.set_deleting(false);
        }
        let report = match job.handle.join() {
            Ok(Ok(report)) => report,
            Ok(Err(e)) => return self.set_status(format!("Löschen fehlgeschlagen: {e}")),
            Err(_) => return self.set_status("Löschen fehlgeschlagen: interner Fehler."),
        };

        if job.generation != self.state.borrow().generation {
            // Defensive: a different tree is shown now; its node ids mean other files.
            self.set_status("In den Papierkorb verschoben. Die Anzeige wurde inzwischen neu geladen.");
            return;
        }
        // Trust the file system, not the dialog outcome: the user may have
        // skipped single items, or files may have been in use.
        let gone: Vec<NodeId> =
            job.items.iter().filter(|(_, path)| !recycle::exists(path)).map(|(id, _)| *id).collect();
        if gone.is_empty() {
            self.set_status(if report.aborted {
                "Abgebrochen – nichts wurde verschoben."
            } else {
                "Konnte nicht verschoben werden (in Verwendung oder keine Berechtigung)."
            });
            return;
        }
        let removed = self.apply_removals(&gone);
        if let Some(title) = job.cleanup {
            let skipped = job.items.len() - gone.len();
            let mut msg = format!("{title}: {} in den Papierkorb verschoben", format::bytes(removed.alloc));
            if skipped > 0 {
                msg.push_str(&format!("  ·  {} übersprungen (in Verwendung)", format::count(skipped as u64)));
            }
            msg.push_str("  ·  Papierkorb leeren, um den Platz freizugeben");
            self.set_status(msg);
            self.refresh_cleanup();
            return;
        }
        let what = match job.items.as_slice() {
            [(_, path)] => path.clone(),
            items => {
                let skipped = items.len() - gone.len();
                let mut s = format!("{} Einträge", format::count(gone.len() as u64));
                if skipped > 0 {
                    s.push_str(&format!(" ({} konnten nicht verschoben werden)", format::count(skipped as u64)));
                }
                s
            }
        };
        // Keep the cursor position so the next item can be deleted right away.
        let len = self.entry_model.len() as i32;
        if len > 0 {
            self.entry_select(job.row.clamp(0, len - 1));
        } else if let Some(ui) = self.ui() {
            ui.set_entry_selected(-1);
        }
        self.set_status(format!(
            "In den Papierkorb verschoben: {what}  ·  {} – der Platz wird frei, sobald der Papierkorb geleert wird",
            format::bytes(removed.alloc)
        ));
        self.refresh_bin_info();
        if let Some(ui) = self.ui() {
            ui.invoke_focus_table();
        }
    }

    /// Removes nodes from the tree in place and refreshes every view without
    /// rescanning or losing the tree's expansion state.
    pub(super) fn apply_removals(&self, ids: &[NodeId]) -> spacelens_core::Removed {
        let _timed = crate::diag::Timed::new("apply_removals");
        let mut total = spacelens_core::Removed::default();
        let Some(mut tree) = self.state.borrow_mut().tree.take() else { return total };
        // Release the models' references so the tree is uniquely owned and
        // can be edited without copying it.
        drop(self.tree_model.take_tree());
        drop(self.entry_model.take_tree());
        let editable = Arc::make_mut(&mut tree);
        for &id in ids {
            if let Some(r) = editable.remove(id) {
                total.size += r.size;
                total.alloc += r.alloc;
                total.files += r.files;
                total.dirs += r.dirs;
            }
        }
        self.state.borrow_mut().tree = Some(Arc::clone(&tree));
        self.tree_model.put_tree(Arc::clone(&tree));
        self.entry_model.put_tree(Arc::clone(&tree));
        self.treemap.borrow_mut().legend_dir = None;
        self.sync_tree_selection();
        if self.state.borrow().view == View::Search {
            self.run_search(); // keeps match count and total size correct
        }
        if self.dups.borrow().root.is_some() {
            self.prune_dup_result(&tree);
            if self.state.borrow().view == View::Duplicates {
                self.show_dup_result();
            }
        }
        self.update_location(&tree);
        self.render_treemap();
        total
    }

    /// HWND of the main window, for owner-modal Windows dialogs.
    pub(super) fn owner_hwnd(&self) -> Option<isize> {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        let ui = self.ui()?;
        let handle = ui.window().window_handle();
        match handle.window_handle().ok()?.as_raw() {
            RawWindowHandle::Win32(h) => Some(h.hwnd.get()),
            _ => None,
        }
    }
}
