//! UI controller: owns the application state and wires Slint callbacks.
//!
//! Threading model: everything here runs on the UI thread. A scan runs on a
//! dedicated thread; the UI polls its atomic progress counters with a 10 Hz
//! timer and joins the thread once it has finished, so no closures or state
//! cross thread boundaries except the `Arc<ScanProgress>`.

use std::cell::RefCell;
use std::ffi::OsString;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use slint::{ComponentHandle, ModelRc, SharedString, Timer, TimerMode, VecModel};
use spacelens_core::{NodeId, ScanError, ScanOptions, ScanOutcome, ScanProgress, Tree, flags, format};
use spacelens_win::elevation::{self, RelaunchError};
use spacelens_win::volumes::{self, Volume};
use spacelens_win::{scanner_for, shell};

use crate::models::{COL_NAME, EntryModel, TreeModel, sort_for_column};
use crate::{MainWindow, NavKey};

const POLL_INTERVAL: Duration = Duration::from_millis(100);

struct ScanJob {
    progress: Arc<ScanProgress>,
    handle: JoinHandle<Result<ScanOutcome, ScanError>>,
    started: Instant,
}

struct State {
    tree: Option<Arc<Tree>>,
    job: Option<ScanJob>,
    volumes: Vec<Volume>,
    /// Directory shown in the contents table.
    current: NodeId,
    sort_column: i32,
    sort_descending: bool,
}

pub struct App {
    ui: slint::Weak<MainWindow>,
    tree_model: Rc<TreeModel>,
    entry_model: Rc<EntryModel>,
    poll_timer: Timer,
    elevated: bool,
    state: RefCell<State>,
}

/// Connects a Slint callback to an `App` method without creating an `Rc`
/// cycle (the UI holds only a weak reference to the controller).
macro_rules! bind {
    ($app:expr, $ui:expr, $on:ident => $method:ident($($arg:ident),*)) => {{
        let weak = Rc::downgrade($app);
        $ui.$on(move |$($arg),*| {
            if let Some(app) = weak.upgrade() {
                app.$method($($arg),*);
            }
        });
    }};
}

impl App {
    pub fn new(ui: &MainWindow, elevated: bool) -> Rc<Self> {
        let app = Rc::new(Self {
            ui: ui.as_weak(),
            tree_model: Rc::new(TreeModel::default()),
            entry_model: Rc::new(EntryModel::default()),
            poll_timer: Timer::default(),
            elevated,
            state: RefCell::new(State {
                tree: None,
                job: None,
                volumes: Vec::new(),
                current: Tree::ROOT,
                sort_column: 1,
                sort_descending: true,
            }),
        });

        ui.set_elevated(elevated);
        ui.set_tree_rows(ModelRc::from(app.tree_model.clone()));
        ui.set_entries(ModelRc::from(app.entry_model.clone()));
        ui.set_status(if elevated {
            "Bereit – Administratormodus: alle Ordner werden erfasst.".into()
        } else {
            "Bereit".into()
        });

        bind!(&app, ui, on_drive_selected => drive_selected(index));
        bind!(&app, ui, on_start_scan => start_scan());
        bind!(&app, ui, on_cancel_scan => cancel_scan());
        bind!(&app, ui, on_elevate => elevate());
        bind!(&app, ui, on_refresh_drives => refresh_drives());
        bind!(&app, ui, on_go_up => go_up());
        bind!(&app, ui, on_reveal_current => reveal_current());
        bind!(&app, ui, on_tree_toggle => tree_toggle(row));
        bind!(&app, ui, on_tree_select => tree_select(row));
        bind!(&app, ui, on_tree_navigate => tree_navigate(key));
        bind!(&app, ui, on_entry_select => entry_select(row));
        bind!(&app, ui, on_entry_open => entry_open(row));
        bind!(&app, ui, on_entry_reveal => entry_reveal(row));
        bind!(&app, ui, on_entry_navigate => entry_navigate(key));
        bind!(&app, ui, on_sort_by => sort_by(column));
        bind!(&app, ui, on_show_about => show_about());
        bind!(&app, ui, on_quit => quit());

        app.refresh_drives();
        app
    }

    fn ui(&self) -> Option<MainWindow> {
        self.ui.upgrade()
    }

    fn set_status(&self, text: impl Into<SharedString>) {
        if let Some(ui) = self.ui() {
            ui.set_status(text.into());
        }
    }

    // ------------------------------------------------------------ drives

    fn refresh_drives(&self) {
        let vols = volumes::list();
        let labels: Vec<SharedString> = vols.iter().map(drive_label).collect();
        let Some(ui) = self.ui() else { return };
        ui.set_drives(ModelRc::new(VecModel::from(labels)));

        if ui.get_scan_path().is_empty() {
            let system = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
            let index = vols.iter().position(|v| v.root.starts_with(&system)).or((!vols.is_empty()).then_some(0));
            if let Some(i) = index {
                ui.set_drive_index(i as i32);
                ui.set_scan_path(vols[i].root.as_str().into());
            }
        }
        self.state.borrow_mut().volumes = vols;
    }

    fn drive_selected(&self, index: i32) {
        let root = self.state.borrow().volumes.get(index as usize).map(|v| v.root.clone());
        if let (Some(root), Some(ui)) = (root, self.ui()) {
            ui.set_scan_path(root.into());
        }
    }

    // -------------------------------------------------------------- scan

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

    fn poll_scan(&self) {
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

    fn finish_scan(&self) {
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

    fn cancel_scan(&self) {
        if let Some(job) = &self.state.borrow().job {
            job.progress.cancel();
            self.set_status("Scan wird abgebrochen …");
        }
    }

    fn show_tree(&self, tree: Option<Arc<Tree>>) {
        let has_tree = tree.is_some();
        self.state.borrow_mut().tree = tree.clone();
        self.entry_model.set_tree(tree.clone());
        self.tree_model.set_tree(tree);
        if let Some(ui) = self.ui() {
            ui.set_has_tree(has_tree);
            ui.set_tree_selected(-1);
            ui.set_entry_selected(-1);
        }
        if has_tree {
            self.navigate_to(Tree::ROOT, None);
            if let Some(ui) = self.ui() {
                ui.invoke_focus_table();
            }
        }
    }

    // ------------------------------------------------------- elevation

    fn elevate(&self) {
        let path = self.ui().map(|ui| ui.get_scan_path().trim().to_string()).unwrap_or_default();
        let mut args: Vec<OsString> = Vec::new();
        if !path.is_empty() {
            args.push("--scan".into());
            args.push(path.into());
        }
        match elevation::relaunch_elevated(&args) {
            Ok(()) => self.quit(),
            Err(RelaunchError::Cancelled) => self.set_status("Administratorrechte wurden nicht erteilt."),
            Err(RelaunchError::Failed(code)) => {
                self.set_status(format!("Neustart als Administrator fehlgeschlagen (Fehler {code})."));
            }
        }
    }

    // ------------------------------------------------------- navigation

    /// Shows `dir` in the table, selects it in the tree and optionally
    /// selects `select` in the table.
    fn navigate_to(&self, dir: NodeId, select: Option<NodeId>) {
        let Some(tree) = self.state.borrow().tree.clone() else { return };
        let (column, descending) = {
            let mut st = self.state.borrow_mut();
            st.current = dir;
            (st.sort_column, st.sort_descending)
        };
        self.entry_model.show(dir, sort_for_column(column, descending));
        let tree_row = self.tree_model.reveal(dir);

        let Some(ui) = self.ui() else { return };
        ui.set_sort_column(column);
        ui.set_sort_descending(descending);
        ui.set_tree_selected(tree_row.map_or(-1, |r| r as i32));
        ui.set_entry_selected(select.and_then(|id| self.entry_model.row_of(id)).map_or(-1, |r| r as i32));
        ui.set_can_go_up(tree.parent(dir).is_some());
        ui.set_location(tree.path(dir).into());

        let n = tree.node(dir);
        let (files, dirs) = tree.counts(dir);
        ui.set_location_info(
            format!(
                "{} (belegt {})  ·  {} Dateien  ·  {} Ordner  ·  {} Einträge hier",
                format::bytes(n.size),
                format::bytes(n.alloc),
                format::count(u64::from(files)),
                format::count(u64::from(dirs)),
                format::count(self.entry_model.len() as u64)
            )
            .into(),
        );
    }

    fn go_up(&self) {
        let (tree, current) = {
            let st = self.state.borrow();
            (st.tree.clone(), st.current)
        };
        if let Some(parent) = tree.and_then(|t| t.parent(current)) {
            self.navigate_to(parent, Some(current));
        }
    }

    fn tree_toggle(&self, row: i32) {
        self.tree_model.toggle(row as usize);
        self.sync_tree_selection();
    }

    fn tree_select(&self, row: i32) {
        if let Some(id) = self.tree_model.id_at(row as usize) {
            self.navigate_to(id, None);
        }
    }

    /// Re-derives the selected tree row from the current directory after
    /// rows were inserted or removed.
    fn sync_tree_selection(&self) {
        let current = self.state.borrow().current;
        if let Some(ui) = self.ui() {
            ui.set_tree_selected(self.tree_model.row_of(current).map_or(-1, |r| r as i32));
        }
    }

    fn tree_navigate(&self, key: NavKey) {
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
            NavKey::Back => return,
        };
        self.tree_select(target as i32);
    }

    fn entry_select(&self, row: i32) {
        let Some(ui) = self.ui() else { return };
        ui.set_entry_selected(row);
        let tree = self.state.borrow().tree.clone();
        if let (Some(tree), Some(id)) = (tree, self.entry_model.id_at(row as usize)) {
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

    fn entry_open(&self, row: i32) {
        let tree = self.state.borrow().tree.clone();
        let (Some(tree), Some(id)) = (tree, self.entry_model.id_at(row as usize)) else { return };
        let n = tree.node(id);
        if n.is_dir() && !n.has(flags::NOT_FOLLOWED) {
            self.navigate_to(id, None);
        } else {
            self.reveal(id);
        }
    }

    fn entry_reveal(&self, row: i32) {
        if let Some(id) = self.entry_model.id_at(row as usize) {
            self.reveal(id);
        }
    }

    fn entry_navigate(&self, key: NavKey) {
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
        };
        if len > 0 {
            self.entry_select(target);
        }
    }

    fn sort_by(&self, column: i32) {
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

    fn reveal_current(&self) {
        let Some(ui) = self.ui() else { return };
        let id = self.entry_model.id_at(ui.get_entry_selected().max(0) as usize);
        let id = if ui.get_entry_selected() >= 0 { id } else { None };
        let current = self.state.borrow().current;
        self.reveal(id.unwrap_or(current));
    }

    fn reveal(&self, id: NodeId) {
        let Some(tree) = self.state.borrow().tree.clone() else { return };
        if let Err(e) = shell::reveal_in_explorer(&tree.path(id)) {
            self.set_status(format!("Explorer konnte nicht geöffnet werden: {e}"));
        }
    }

    // ------------------------------------------------------------ misc

    fn show_about(&self) {
        shell::message_box(
            "Über SpaceLens",
            &format!(
                "SpaceLens {}\n\nSchneller Disk-Analyzer für Windows.\nNur lesend – es werden keine Dateien verändert.\n\nRechte: {}",
                env!("CARGO_PKG_VERSION"),
                if self.elevated { "Administrator" } else { "Standardbenutzer" }
            ),
            false,
        );
    }

    fn quit(&self) {
        self.shutdown();
        let _ = slint::quit_event_loop();
    }

    /// Stops a running scan so worker threads end promptly.
    pub fn shutdown(&self) {
        if let Some(job) = &self.state.borrow().job {
            job.progress.cancel();
        }
    }
}

fn drive_label(v: &Volume) -> SharedString {
    let name = if v.label.is_empty() { v.kind.label() } else { v.label.as_str() };
    format!(
        "{}  {}  ({})  –  {} von {} belegt",
        v.root.trim_end_matches('\\'),
        name,
        v.fs,
        format::bytes(v.used()),
        format::bytes(v.total)
    )
    .into()
}
