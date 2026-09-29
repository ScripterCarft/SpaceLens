//! Cleanup tab and recycle bin.

use super::*;

impl App {
    /// Mirrors the current view into the UI's toggle flags.
    pub(super) fn sync_view_flags(&self) {
        let view = self.state.borrow().view;
        if let Some(ui) = self.ui() {
            ui.set_tab(view.tab());
            ui.set_cleanup_panel(view == View::Cleanup);
        }
    }

    pub(super) fn show_cleanup(&self) {
        if self.state.borrow().tree.is_none() {
            return;
        }
        self.state.borrow_mut().view = View::Cleanup;
        self.sync_view_flags();
        self.refresh_cleanup();
        if let Some(tree) = self.state.borrow().tree.clone() {
            self.update_location(&tree);
        }
    }

    /// Re-evaluates the catalogue against the (possibly changed) tree. Pure
    /// in-memory work: a few path lookups, no disk access.
    pub(super) fn refresh_cleanup(&self) {
        let _timed = crate::diag::Timed::new("refresh_cleanup");
        let Some(tree) = self.state.borrow().tree.clone() else { return };
        let findings = cleanup::evaluate(&tree, known::resolve);
        let rows: Vec<CleanupRow> = findings
            .iter()
            .map(|f| CleanupRow {
                title: f.rule.title.into(),
                description: f.rule.description.into(),
                size: format::bytes(f.size).into(),
                detail: format!("{} Dateien", format::count(f.files)).into(),
                caution: f.rule.level == Level::Caution,
                needs_admin: f.rule.needs_admin && !self.elevated,
                removable: f.is_removable(),
            })
            .collect();
        *self.findings.borrow_mut() = findings;

        let Some(ui) = self.ui() else { return };
        ui.set_cleanup_rows(ModelRc::new(VecModel::from(rows)));
        self.refresh_bin_info();

        let system_root = known::resolve(cleanup::Base::SystemDrive);
        let mut hints = Vec::new();
        if system_root.as_deref().is_some_and(|r| tree.find_path(r) != Some(Tree::ROOT)) {
            hints.push(
                "Nur Bereiche innerhalb des gescannten Ordners werden angezeigt – für alle das Systemlaufwerk scannen.",
            );
        }
        if !self.elevated {
            hints.push("Als Administrator werden zusätzlich Windows-Temp, Update-Downloads und Fehlerberichte erfasst und bereinigt.");
        }
        ui.set_cleanup_hint(hints.join("  ").into());
    }

    /// Updates the recycle-bin line. Windows counts the whole bin for this
    /// (≈180 ms with a few thousand items), so it runs on a worker thread and
    /// the tab opens immediately.
    pub(super) fn refresh_bin_info(&self) {
        let Some(ui) = self.ui() else { return };
        ui.set_bin_text("Papierkorb: Größe wird ermittelt …".into());
        let weak = ui.as_weak();
        let spawned = std::thread::Builder::new().name("recycle-bin-info".into()).spawn(move || {
            let info = recycle::bin_info();
            let _ = weak.upgrade_in_event_loop(move |ui| {
                let (text, empty) = bin_text(info);
                ui.set_bin_text(text.into());
                ui.set_bin_empty(empty);
            });
        });
        if spawned.is_err() {
            let (text, empty) = bin_text(recycle::bin_info());
            ui.set_bin_text(text.into());
            ui.set_bin_empty(empty);
        }
    }

    /// A single location opens directly; several are listed in the table.
    pub(super) fn cleanup_show(&self, index: i32) {
        let Some((title, locations)) =
            self.findings.borrow().get(index as usize).map(|f| (f.rule.title, f.locations.clone()))
        else {
            return;
        };
        let Some(tree) = self.state.borrow().tree.clone() else { return };
        if locations.len() > 1 {
            let (column, descending) = {
                let mut st = self.state.borrow_mut();
                st.view = View::Locations(title);
                (st.sort_column, st.sort_descending)
            };
            self.entry_model.show_list(locations, Tree::ROOT, sort_for_column(column, descending));
            self.sync_view_flags();
            if let Some(ui) = self.ui() {
                ui.set_entry_selected(-1);
                ui.invoke_focus_table();
            }
            self.update_location(&tree);
            return;
        }
        let Some(&loc) = locations.first() else { return };
        if tree.node(loc).is_dir() {
            self.navigate_to(loc, None);
        } else if let Some(parent) = tree.parent(loc) {
            self.navigate_to(parent, Some(loc));
        }
    }

    pub(super) fn cleanup_clean(self: &Rc<Self>, index: i32) {
        if self.state.borrow().job.is_some()
            || self.state.borrow().delete_job.is_some()
            || self.state.borrow().dup_job.is_some()
        {
            return self.set_status("Bitte warten, bis der laufende Vorgang abgeschlossen ist.");
        }
        let Some(finding) = self.findings.borrow().get(index as usize).cloned() else { return };
        let Some(tree) = self.state.borrow().tree.clone() else { return };
        // The catalogue is vetted, but the hard guard rails still apply.
        let items: Vec<(NodeId, String)> = finding
            .targets
            .iter()
            .filter(|&&id| !matches!(safety::guard(&tree, id), Some(Guard::Blocked(_))))
            .map(|&id| (id, tree.path(id)))
            .collect();
        if items.is_empty() {
            return self.set_status("Nichts zu bereinigen.");
        }

        let text = format!(
            "{}\n\n{} in {} Dateien in den Papierkorb verschieben?\n\n{}\n\nDateien, die gerade verwendet werden, werden übersprungen. \
             Um den Platz freizugeben, danach den Papierkorb leeren.",
            finding.rule.title,
            format::bytes(finding.size),
            format::count(finding.files),
            finding.rule.description
        );
        let owner = self.owner_hwnd();
        if !shell::confirm(owner, "SpaceLens – Aufräumen", &text, finding.rule.level == Level::Caution) {
            return;
        }
        self.start_delete_job(items, -1, Some(finding.rule.title), true);
    }

    /// Runs the recycle operation on a worker thread and polls for completion.
    pub(super) fn start_delete_job(
        self: &Rc<Self>,
        items: Vec<(NodeId, String)>,
        row: i32,
        cleanup: Option<&'static str>,
        skip_errors: bool,
    ) {
        let owner = self.owner_hwnd();
        let paths: Vec<String> = items.iter().map(|(_, p)| p.clone()).collect();
        let spawned = std::thread::Builder::new()
            .name("recycle".into())
            .spawn(move || recycle::recycle(&paths, owner, skip_errors));
        let handle = match spawned {
            Ok(h) => h,
            Err(e) => return self.set_status(format!("Löschen konnte nicht gestartet werden: {e}")),
        };
        let generation = self.state.borrow().generation;
        self.state.borrow_mut().delete_job = Some(DeleteJob { handle, items, row, cleanup, generation });
        if let Some(ui) = self.ui() {
            ui.set_deleting(true);
        }
        self.set_status("Wird in den Papierkorb verschoben …");
        let weak = Rc::downgrade(self);
        self.delete_timer.start(TimerMode::Repeated, POLL_INTERVAL, move || {
            if let Some(app) = weak.upgrade() {
                app.poll_delete();
            }
        });
    }

    /// Empties the recycle bin (Windows asks for confirmation) and removes its
    /// contents from the tree, so the drive totals drop accordingly.
    pub(super) fn empty_bin(&self) {
        let _timed = crate::diag::Timed::new("empty_bin");
        if self.state.borrow().delete_job.is_some() {
            self.set_status("Bitte warten, bis das Verschieben in den Papierkorb abgeschlossen ist.");
            return;
        }
        if self.state.borrow().dup_job.is_some() {
            self.set_status("Bitte zuerst die Duplikat-Suche abbrechen oder abwarten.");
            return;
        }
        let before = recycle::bin_info().map_or(0, |(size, _)| size);
        match recycle::empty_bin(self.owner_hwnd()) {
            recycle::EmptyResult::Emptied => {
                let tree = self.state.borrow().tree.clone();
                if let Some(tree) = tree {
                    let bin = format!("{}\\$Recycle.Bin", tree.root_path().trim_end_matches('\\'));
                    if let Some(bin) = tree.find_path(&bin) {
                        let ids: Vec<NodeId> =
                            tree.live_children(bin).flat_map(|sid| tree.live_children(sid)).collect();
                        self.apply_removals(&ids);
                    }
                }
                self.set_status(format!("Papierkorb geleert – {} freigegeben.", format::bytes(before)));
            }
            recycle::EmptyResult::NotEmptied => self.set_status("Papierkorb wurde nicht geleert."),
            recycle::EmptyResult::Failed(e) => self.set_status(format!("Papierkorb leeren fehlgeschlagen: {e}")),
        }
        if self.state.borrow().view == View::Cleanup {
            self.refresh_cleanup();
        } else {
            self.refresh_bin_info();
        }
    }
}

/// Text for the recycle-bin line and whether the bin is empty.
fn bin_text(info: Option<(u64, u64)>) -> (String, bool) {
    match info {
        Some((size, items)) if items > 0 => (
            format!(
                "Papierkorb: {} in {} Elementen – dieser Platz ist erst nach dem Leeren wirklich frei.",
                format::bytes(size),
                format::count(items)
            ),
            false,
        ),
        Some(_) => ("Papierkorb ist leer.".to_string(), true),
        None => ("Papierkorb-Größe unbekannt.".to_string(), false),
    }
}
