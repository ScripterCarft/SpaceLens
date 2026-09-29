//! Duplicates tab.

use super::*;

impl App {
    pub(super) fn show_duplicates(&self) {
        if self.state.borrow().tree.is_none() {
            return;
        }
        self.state.borrow_mut().view = View::Duplicates;
        self.sync_view_flags();
        self.show_dup_result();
    }

    /// Shows the stored result if it belongs to the current folder.
    pub(super) fn show_dup_result(&self) {
        let _timed = crate::diag::Timed::new("show_dup_result");
        let Some(tree) = self.state.borrow().tree.clone() else { return };
        let current = self.state.borrow().current;
        let dups = self.dups.borrow();
        let groups: &[DupGroup] = if dups.root == Some(current) { &dups.groups } else { &[] };
        let ids: Vec<Vec<NodeId>> = groups.iter().map(|g| g.ids.clone()).collect();
        let has_results = !groups.is_empty();
        let summary = if dups.root != Some(current) {
            "Findet Dateien mit identischem Inhalt in diesem Ordner (System- und Programmbereiche ausgenommen)."
                .to_string()
        } else if groups.is_empty() {
            "Keine Duplikate gefunden.".to_string()
        } else {
            let files: usize = groups.iter().map(|g| g.ids.len()).sum();
            let wasted: u64 = groups.iter().map(DupGroup::wasted).sum();
            format!(
                "{} Gruppen  ·  {} Dateien  ·  {} doppelt belegt",
                format::count(groups.len() as u64),
                format::count(files as u64),
                format::bytes(wasted)
            )
        };
        drop(dups);
        self.entry_model.show_groups(&ids, current);
        if let Some(ui) = self.ui() {
            ui.set_dupes_has_results(has_results);
            ui.set_dupes_summary(summary.into());
            ui.set_entry_selected(-1);
        }
        self.update_location(&tree);
    }

    pub(super) fn dupes_start(self: &Rc<Self>) {
        let _timed = crate::diag::Timed::new("dupes_start");
        {
            let st = self.state.borrow();
            if st.job.is_some() || st.delete_job.is_some() || st.dup_job.is_some() {
                return;
            }
        }
        let Some(ui) = self.ui() else { return };
        let (tree, root, generation) = {
            let st = self.state.borrow();
            (st.tree.clone(), st.current, st.generation)
        };
        let Some(tree) = tree else { return };
        let min = DUPE_MIN_SIZES.get(ui.get_dupes_min().max(0) as usize).copied().unwrap_or(1 << 20);

        // Stage 1 (same size) needs no I/O and is instant; hashing runs on a worker.
        let candidates = duplicates::candidates(&tree, root, min);
        let threads = match tree.root_path().chars().next().and_then(spacelens_win::device::seek_penalty) {
            Some(true) => 1, // HDD: one sequential reader
            _ => 4,
        };
        let progress = Arc::new(ScanProgress::new());
        let spawned = std::thread::Builder::new().name("duplicates".into()).spawn({
            let (tree, progress) = (Arc::clone(&tree), Arc::clone(&progress));
            move || spacelens_win::dupes::find(&tree, candidates, &progress, threads)
        });
        let handle = match spawned {
            Ok(h) => h,
            Err(e) => return self.set_status(format!("Duplikat-Suche konnte nicht gestartet werden: {e}")),
        };
        self.state.borrow_mut().dup_job = Some(DupJob { progress, handle, root, generation, started: Instant::now() });
        ui.set_dupes_running(true);
        ui.set_dupes_summary("Suche läuft …".into());

        let weak = Rc::downgrade(self);
        self.dup_timer.start(TimerMode::Repeated, POLL_INTERVAL, move || {
            if let Some(app) = weak.upgrade() {
                app.poll_dupes();
            }
        });
    }

    pub(super) fn dupes_cancel(&self) {
        if let Some(job) = &self.state.borrow().dup_job {
            job.progress.cancel();
        }
    }

    pub(super) fn poll_dupes(&self) {
        let finished = {
            let st = self.state.borrow();
            let Some(job) = &st.dup_job else {
                self.dup_timer.stop();
                return;
            };
            if let Some(ui) = self.ui() {
                let s = job.progress.snapshot();
                ui.set_dupes_summary(
                    format!(
                        "{}  {} Dateien geprüft  ·  {} gelesen  ·  {}",
                        job.progress.current(),
                        format::count(s.files),
                        format::bytes(s.bytes),
                        format::seconds(job.started.elapsed().as_secs_f64())
                    )
                    .into(),
                );
            }
            job.handle.is_finished()
        };
        if !finished {
            return;
        }
        self.dup_timer.stop();
        let Some(job) = self.state.borrow_mut().dup_job.take() else { return };
        if let Some(ui) = self.ui() {
            ui.set_dupes_running(false);
        }
        let result = job.handle.join();
        if job.generation != self.state.borrow().generation {
            return; // a new scan replaced the tree meanwhile
        }
        match result {
            Ok(Some(groups)) => {
                let elapsed = job.started.elapsed();
                *self.dups.borrow_mut() = DupResult { root: Some(job.root), groups };
                self.set_status(format!("Duplikat-Suche fertig in {}", format::seconds(elapsed.as_secs_f64())));
            }
            Ok(None) => self.set_status("Duplikat-Suche abgebrochen."),
            Err(_) => self.set_status("Duplikat-Suche fehlgeschlagen: interner Fehler."),
        }
        if self.state.borrow().view == View::Duplicates {
            self.show_dup_result();
        }
    }

    /// Selects every copy except the oldest file of each group.
    pub(super) fn dupes_select_copies(&self) {
        let Some(tree) = self.state.borrow().tree.clone() else { return };
        let copies: Vec<NodeId> = {
            let dups = self.dups.borrow();
            dups.groups
                .iter()
                .flat_map(|g| {
                    let keep = duplicates::keeper(&tree, &g.ids);
                    g.ids.iter().copied().filter(move |&id| Some(id) != keep)
                })
                .collect()
        };
        if copies.is_empty() {
            return;
        }
        self.entry_model.select_ids(&copies);
        let size: u64 = copies.iter().map(|&id| tree.node(id).size).sum();
        if let Some(ui) = self.ui() {
            ui.set_entry_selected(self.entry_model.row_of(copies[0]).map_or(-1, |r| r as i32));
            ui.invoke_focus_table();
        }
        self.set_status(format!(
            "{} Kopien ausgewählt ({}) – jeweils die älteste Datei bleibt. Entf verschiebt die Kopien in den Papierkorb.",
            format::count(copies.len() as u64),
            format::bytes(size)
        ));
    }

    /// Drops removed files from the stored groups (after deleting).
    pub(super) fn prune_dup_result(&self, tree: &Tree) {
        let mut dups = self.dups.borrow_mut();
        for g in &mut dups.groups {
            g.ids.retain(|&id| !tree.node(id).has(flags::DELETED));
        }
        dups.groups.retain(|g| g.ids.len() > 1);
    }
}
