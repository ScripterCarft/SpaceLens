//! Running and finishing scans.

use super::*;

impl App {
    pub fn start_scan(self: &Rc<Self>) {
        let Some(ui) = self.ui() else { return };
        let path = ui.get_scan_path().trim().to_string();
        if path.is_empty() {
            self.set_status("Bitte ein Laufwerk oder einen Pfad angeben.");
            return;
        }
        if self.state.borrow().job.is_some() {
            return;
        }
        if self.state.borrow().delete_job.is_some() {
            self.set_status("Bitte warten, bis das Verschieben in den Papierkorb abgeschlossen ist.");
            return;
        }
        if self.state.borrow().dup_job.is_some() {
            self.set_status("Bitte zuerst die Duplikat-Suche abbrechen oder abwarten.");
            return;
        }

        // Release the previous tree before scanning to keep peak memory low.
        self.show_tree(None);

        let progress = Arc::new(ScanProgress::new());
        let root = PathBuf::from(&path);
        let scanner = scanner_for(&root);
        let spawned = std::thread::Builder::new().name("scan-coordinator".into()).spawn({
            let progress = Arc::clone(&progress);
            move || scanner.scan(&root, &ScanOptions::default(), &progress)
        });
        let handle = match spawned {
            Ok(h) => h,
            Err(e) => {
                self.set_status(format!("Scan konnte nicht gestartet werden: {e}"));
                return;
            }
        };

        self.state.borrow_mut().job = Some(ScanJob { progress, handle, started: Instant::now() });
        ui.set_scanning(true);
        ui.set_scan_info(SharedString::new());
        ui.set_progress_counts(SharedString::new());
        ui.set_progress_path(path.as_str().into());
        self.set_status(format!("Scanne {path} …"));

        let weak = Rc::downgrade(self);
        self.poll_timer.start(TimerMode::Repeated, POLL_INTERVAL, move || {
            if let Some(app) = weak.upgrade() {
                app.poll_scan();
            }
        });
    }

    pub(super) fn poll_scan(&self) {
        let finished = {
            let state = self.state.borrow();
            let Some(job) = &state.job else {
                self.poll_timer.stop();
                return;
            };
            if let Some(ui) = self.ui() {
                let s = job.progress.snapshot();
                ui.set_progress_counts(
                    format!(
                        "{} Dateien  ·  {} Ordner  ·  {}  ·  {}",
                        format::count(s.files),
                        format::count(s.dirs),
                        format::bytes(s.bytes),
                        format::seconds(job.started.elapsed().as_secs_f64())
                    )
                    .into(),
                );
                ui.set_progress_path(job.progress.current().into());
            }
            job.handle.is_finished()
        };
        if finished {
            self.finish_scan();
        }
    }

    pub(super) fn finish_scan(&self) {
        let _timed = crate::diag::Timed::new("finish_scan");
        self.poll_timer.stop();
        let Some(job) = self.state.borrow_mut().job.take() else { return };
        let Some(ui) = self.ui() else { return };
        ui.set_scanning(false);

        let outcome = match job.handle.join() {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(e)) => {
                self.set_status(format!("Scan fehlgeschlagen: {e}"));
                return;
            }
            Err(_) => {
                self.set_status("Scan fehlgeschlagen: interner Fehler (Panic im Scanner).");
                return;
            }
        };

        let tree = Arc::new(outcome.tree);
        let (files, dirs) = tree.counts(Tree::ROOT);
        let issues = tree.issues().len();
        crate::diag::log(&format!(
            "Scan fertig: {} · {} Einträge · {} ms · {} ohne Zugriff{}",
            outcome.method,
            tree.len(),
            outcome.elapsed.as_millis(),
            issues,
            if outcome.cancelled { " · abgebrochen" } else { "" }
        ));
        ui.set_scan_info(
            format!(
                "{} · {} Threads · {} · {} RAM",
                outcome.method,
                outcome.threads,
                format::seconds(outcome.elapsed.as_secs_f64()),
                format::bytes(tree.memory_bytes() as u64)
            )
            .into(),
        );

        let mut status = format!(
            "{} {} Dateien, {} Ordner, {}",
            if outcome.cancelled { "Abgebrochen – Teilergebnis:" } else { "Fertig:" },
            format::count(u64::from(files)),
            format::count(u64::from(dirs)),
            format::bytes(tree.node(Tree::ROOT).size)
        );
        if issues > 0 {
            status.push_str(&format!(" · {} Ordner ohne Zugriff", format::count(issues as u64)));
            if !self.elevated {
                status.push_str(" – für vollständige Ergebnisse als Administrator neu starten");
            }
        }
        self.set_status(status);
        self.show_tree(Some(tree));
    }

    pub(super) fn cancel_scan(&self) {
        if let Some(job) = &self.state.borrow().job {
            job.progress.cancel();
            self.set_status("Scan wird abgebrochen …");
        }
    }

    pub(super) fn show_tree(&self, tree: Option<Arc<Tree>>) {
        let has_tree = tree.is_some();
        {
            let mut st = self.state.borrow_mut();
            st.tree = tree.clone();
            st.generation += 1;
        }
        *self.dups.borrow_mut() = DupResult::default();
        self.findings.borrow_mut().clear();
        self.entry_model.set_tree(tree.clone());
        self.tree_model.set_tree(tree);
        if let Some(ui) = self.ui() {
            ui.set_has_tree(has_tree);
            ui.set_tree_selected(-1);
            ui.set_entry_selected(-1);
        }
        self.treemap.borrow_mut().legend_dir = None;
        if !has_tree {
            self.clear_treemap();
        }
        if has_tree {
            self.navigate_to(Tree::ROOT, None);
            if let Some(ui) = self.ui() {
                ui.invoke_focus_table();
            }
        }
    }
}
