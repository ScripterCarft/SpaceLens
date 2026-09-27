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

use slint::{
    Color, ComponentHandle, Image, ModelRc, Rgb8Pixel, SharedPixelBuffer, SharedString, Timer, TimerMode, VecModel,
};
use spacelens_core::cleanup::{self, Finding, Level};
use spacelens_core::filetype::{self, Category};
use spacelens_core::safety::{self, Guard};
use spacelens_core::treemap::{Tile, Treemap};
use spacelens_core::{NodeId, ScanError, ScanOptions, ScanOutcome, ScanProgress, Tree, flags, format, query};
use spacelens_win::elevation::{self, RelaunchError};
use spacelens_win::known;
use spacelens_win::recycle::{self, RecycleReport};
use spacelens_win::volumes::{self, Volume};
use spacelens_win::{scanner_for, shell};

use crate::models::{COL_NAME, EntryModel, TreeModel, sort_for_column};
use crate::{CleanupRow, LegendItem, MainWindow, NavKey, TreemapLabel};

const POLL_INTERVAL: Duration = Duration::from_millis(100);
/// Resizing re-renders the treemap once the size has been stable this long.
const TREEMAP_DEBOUNCE: Duration = Duration::from_millis(60);
/// Background (= border colour source) of the treemap image; matches treemap.slint.
const TREEMAP_BACKGROUND: [u8; 3] = [0x20, 0x20, 0x20];

/// Treemap of the current directory, kept for hit-testing.
#[derive(Default)]
struct TreemapState {
    map: Treemap,
    /// Directory the legend was computed for (it is independent of the size).
    legend_dir: Option<NodeId>,
    /// Drawing area in logical pixels and the scale factor used to render.
    logical: (f32, f32),
    scale: f32,
    hovered: Option<NodeId>,
}

struct ScanJob {
    progress: Arc<ScanProgress>,
    handle: JoinHandle<Result<ScanOutcome, ScanError>>,
    started: Instant,
}

/// What the area above the treemap shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum View {
    /// Children of the current directory.
    Folder,
    /// Largest files below the current directory.
    Largest,
    /// Known junk locations.
    Cleanup,
}

/// Items being moved to the recycle bin on a worker thread.
struct DeleteJob {
    handle: JoinHandle<Result<RecycleReport, String>>,
    items: Vec<(NodeId, String)>,
    /// Table row to select afterwards (keeps the position for quick successive deletes).
    row: i32,
    /// Set for cleanup-view jobs (bulk, errors skipped silently).
    cleanup: Option<&'static str>,
}

struct State {
    tree: Option<Arc<Tree>>,
    job: Option<ScanJob>,
    delete_job: Option<DeleteJob>,
    view: View,
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
    treemap: RefCell<TreemapState>,
    treemap_timer: Timer,
    delete_timer: Timer,
    /// Result of the last cleanup evaluation (indices match the view rows).
    findings: RefCell<Vec<Finding>>,
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
            treemap: RefCell::new(TreemapState::default()),
            treemap_timer: Timer::default(),
            delete_timer: Timer::default(),
            findings: RefCell::new(Vec::new()),
            elevated,
            state: RefCell::new(State {
                tree: None,
                job: None,
                delete_job: None,
                view: View::Folder,
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
        bind!(&app, ui, on_entry_delete => entry_delete(row));
        bind!(&app, ui, on_delete_selected => delete_selected());
        bind!(&app, ui, on_show_folder_view => show_folder_view());
        bind!(&app, ui, on_show_largest => show_largest());
        bind!(&app, ui, on_show_cleanup => show_cleanup());
        bind!(&app, ui, on_cleanup_show => cleanup_show(index));
        bind!(&app, ui, on_cleanup_clean => cleanup_clean(index));
        bind!(&app, ui, on_empty_bin => empty_bin());
        bind!(&app, ui, on_entry_navigate => entry_navigate(key));
        bind!(&app, ui, on_sort_by => sort_by(column));
        bind!(&app, ui, on_show_about => show_about());
        bind!(&app, ui, on_treemap_resized => treemap_resized(width, height));
        bind!(&app, ui, on_treemap_hovered => treemap_hovered(x, y));
        bind!(&app, ui, on_treemap_left => treemap_left());
        bind!(&app, ui, on_treemap_clicked => treemap_clicked(x, y));
        bind!(&app, ui, on_treemap_double_clicked => treemap_double_clicked(x, y));
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

    /// Header above the table (path + totals) for the current view.
    fn update_location(&self, tree: &Tree) {
        let Some(ui) = self.ui() else { return };
        if self.state.borrow().view == View::Cleanup {
            return; // header belongs to the cleanup view
        }
        let (dir, largest) = {
            let st = self.state.borrow();
            (st.current, st.view == View::Largest)
        };
        let n = tree.node(dir);
        let (files, dirs) = tree.counts(dir);
        ui.set_can_go_up(largest || tree.parent(dir).is_some());
        if largest {
            ui.set_location(format!("Größte Dateien in {}", tree.path(dir)).into());
            ui.set_location_info(
                format!(
                    "Die {} größten von {} Dateien  ·  Doppelklick springt zum Ordner der Datei",
                    format::count(self.entry_model.len() as u64),
                    format::count(u64::from(files))
                )
                .into(),
            );
            return;
        }
        ui.set_location(tree.path(dir).into());
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

    /// Number of entries listed in the "largest files" view.
    const LARGEST_COUNT: usize = 1000;

    fn show_largest(&self) {
        let (tree, dir, column, descending) = {
            let mut st = self.state.borrow_mut();
            st.view = View::Largest;
            (st.tree.clone(), st.current, st.sort_column, st.sort_descending)
        };
        let Some(tree) = tree else { return };
        let ids = query::largest_files(&tree, dir, Self::LARGEST_COUNT);
        self.entry_model.show_list(ids, dir, sort_for_column(column, descending));
        if let Some(ui) = self.ui() {
            ui.set_view_largest(true);
            ui.set_view_cleanup(false);
            ui.set_entry_selected(-1);
            ui.invoke_focus_table();
        }
        self.update_location(&tree);
    }

    fn show_folder_view(&self) {
        let current = self.state.borrow().current;
        self.navigate_to(current, None);
    }

    fn go_up(&self) {
        if self.state.borrow().view != View::Folder {
            return self.show_folder_view();
        }
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
            NavKey::Back | NavKey::Delete => return,
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
        } else if self.state.borrow().view == View::Largest {
            // From the list, jump to the file in its folder.
            if let Some(parent) = tree.parent(id) {
                self.navigate_to(parent, Some(id));
            }
        } else {
            self.reveal(id);
        }
    }

    fn entry_reveal(&self, row: i32) {
        if let Some(id) = self.entry_model.id_at(row as usize) {
            self.reveal(id);
        }
    }

    fn entry_navigate(self: &Rc<Self>, key: NavKey) {
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

    // ----------------------------------------------------------- delete

    fn delete_selected(self: &Rc<Self>) {
        let row = self.ui().map_or(-1, |ui| ui.get_entry_selected());
        if row >= 0 {
            self.delete_row(row);
        }
    }

    fn entry_delete(self: &Rc<Self>, row: i32) {
        self.delete_row(row);
    }

    /// Guard check → confirmation → recycle on a worker thread.
    fn delete_row(self: &Rc<Self>, row: i32) {
        {
            let st = self.state.borrow();
            if st.job.is_some() || st.delete_job.is_some() {
                drop(st);
                self.set_status("Bitte warten, bis der laufende Vorgang abgeschlossen ist.");
                return;
            }
        }
        let tree = self.state.borrow().tree.clone();
        let (Some(tree), Some(id)) = (tree, self.entry_model.id_at(row.max(0) as usize)) else { return };
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

    fn poll_delete(&self) {
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
        let name = job.items.first().map_or_else(String::new, |(_, p)| p.clone());
        self.set_status(format!(
            "In den Papierkorb verschoben: {name}  ·  {} – der Platz wird frei, sobald der Papierkorb geleert wird",
            format::bytes(removed.alloc)
        ));
        self.refresh_bin_info();
        if let Some(ui) = self.ui() {
            let len = self.entry_model.len() as i32;
            ui.set_entry_selected(if len == 0 { -1 } else { job.row.min(len - 1) });
            ui.invoke_focus_table();
        }
    }

    /// Removes nodes from the tree in place and refreshes every view without
    /// rescanning or losing the tree's expansion state.
    fn apply_removals(&self, ids: &[NodeId]) -> spacelens_core::Removed {
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
        self.update_location(&tree);
        self.render_treemap();
        total
    }

    /// HWND of the main window, for owner-modal Windows dialogs.
    fn owner_hwnd(&self) -> Option<isize> {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        let ui = self.ui()?;
        let handle = ui.window().window_handle();
        match handle.window_handle().ok()?.as_raw() {
            RawWindowHandle::Win32(h) => Some(h.hwnd.get()),
            _ => None,
        }
    }

    // ---------------------------------------------------------- cleanup

    /// Mirrors the current view into the UI's toggle flags.
    fn sync_view_flags(&self) {
        let view = self.state.borrow().view;
        if let Some(ui) = self.ui() {
            ui.set_view_largest(view == View::Largest);
            ui.set_view_cleanup(view == View::Cleanup);
        }
    }

    fn show_cleanup(&self) {
        if self.state.borrow().tree.is_none() {
            return;
        }
        self.state.borrow_mut().view = View::Cleanup;
        self.sync_view_flags();
        self.refresh_cleanup();
        if let Some(ui) = self.ui() {
            ui.set_location("Aufräumen".into());
            ui.set_location_info("Bekannte Bereiche mit entbehrlichen Daten – alles landet im Papierkorb".into());
            ui.set_can_go_up(true);
        }
    }

    /// Re-evaluates the catalogue against the (possibly changed) tree. Pure
    /// in-memory work: a few path lookups, no disk access.
    fn refresh_cleanup(&self) {
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

    fn refresh_bin_info(&self) {
        let Some(ui) = self.ui() else { return };
        match recycle::bin_info() {
            Some((size, items)) if items > 0 => {
                ui.set_bin_text(
                    format!(
                        "Papierkorb: {} in {} Elementen – dieser Platz ist erst nach dem Leeren wirklich frei.",
                        format::bytes(size),
                        format::count(items)
                    )
                    .into(),
                );
                ui.set_bin_empty(false);
            }
            Some(_) => {
                ui.set_bin_text("Papierkorb ist leer.".into());
                ui.set_bin_empty(true);
            }
            None => {
                ui.set_bin_text("Papierkorb-Größe unbekannt.".into());
                ui.set_bin_empty(false);
            }
        }
    }

    /// Shows the first location of a finding in the folder view.
    fn cleanup_show(&self, index: i32) {
        let loc = self.findings.borrow().get(index as usize).and_then(|f| f.locations.first().copied());
        let Some(tree) = self.state.borrow().tree.clone() else { return };
        let Some(loc) = loc else { return };
        if tree.node(loc).is_dir() {
            self.navigate_to(loc, None);
        } else if let Some(parent) = tree.parent(loc) {
            self.navigate_to(parent, Some(loc));
        }
    }

    fn cleanup_clean(self: &Rc<Self>, index: i32) {
        if self.state.borrow().job.is_some() || self.state.borrow().delete_job.is_some() {
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
    fn start_delete_job(
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
        self.state.borrow_mut().delete_job = Some(DeleteJob { handle, items, row, cleanup });
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
    fn empty_bin(&self) {
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

    // ---------------------------------------------------------- treemap

    fn treemap_resized(self: &Rc<Self>, width: f32, height: f32) {
        self.treemap.borrow_mut().logical = (width, height);
        let weak = Rc::downgrade(self);
        self.treemap_timer.start(TimerMode::SingleShot, TREEMAP_DEBOUNCE, move || {
            if let Some(app) = weak.upgrade() {
                app.render_treemap();
            }
        });
    }

    fn clear_treemap(&self) {
        let mut tm = self.treemap.borrow_mut();
        tm.map = Treemap::default();
        tm.hovered = None;
        if let Some(ui) = self.ui() {
            ui.set_treemap_image(Image::default());
            ui.set_treemap_labels(ModelRc::default());
            ui.set_treemap_legend(ModelRc::default());
            ui.set_treemap_hover_visible(false);
        }
    }

    /// Lays out and draws the current directory at the panel's physical size.
    fn render_treemap(&self) {
        let Some(ui) = self.ui() else { return };
        if !ui.get_treemap_visible() {
            return;
        }
        let (tree, dir) = {
            let st = self.state.borrow();
            (st.tree.clone(), st.current)
        };
        let Some(tree) = tree else { return self.clear_treemap() };
        let (w, h) = self.treemap.borrow().logical;
        if w < 1.0 || h < 1.0 {
            return;
        }
        let scale = ui.window().scale_factor().max(0.5);
        let (pw, ph) = ((w * scale).round() as u32, (h * scale).round() as u32);

        let map = Treemap::layout(&tree, dir, pw, ph);
        let mut pixels = SharedPixelBuffer::<Rgb8Pixel>::new(pw, ph);
        map.render(pixels.make_mut_bytes(), TREEMAP_BACKGROUND);
        ui.set_treemap_image(Image::from_rgb8(pixels));

        let labels: Vec<TreemapLabel> = map
            .tiles
            .iter()
            .filter(|t| t.depth == 0 && t.rect.w / scale >= 70.0 && t.rect.h / scale >= 24.0)
            .take(64)
            .map(|t| TreemapLabel {
                x: t.rect.x / scale,
                y: t.rect.y / scale,
                width: t.rect.w / scale,
                height: t.rect.h / scale,
                text: format!("{}  {}", tree.name(t.node), format::bytes(tree.node(t.node).size)).into(),
            })
            .collect();
        ui.set_treemap_labels(ModelRc::new(VecModel::from(labels)));
        ui.set_treemap_hover_visible(false);

        let mut tm = self.treemap.borrow_mut();
        tm.map = map;
        tm.scale = scale;
        tm.hovered = None;
        if tm.legend_dir != Some(dir) {
            tm.legend_dir = Some(dir);
            ui.set_treemap_legend(ModelRc::new(VecModel::from(legend(&tree, dir))));
        }
    }

    /// Deepest tile under a point given in logical pixels.
    fn treemap_hit(&self, x: f32, y: f32) -> Option<Tile> {
        let tm = self.treemap.borrow();
        tm.map.hit(x * tm.scale, y * tm.scale).copied()
    }

    fn treemap_hovered(&self, x: f32, y: f32) {
        let Some(tile) = self.treemap_hit(x, y) else { return self.treemap_left() };
        let scale = {
            let mut tm = self.treemap.borrow_mut();
            if tm.hovered == Some(tile.node) {
                return;
            }
            tm.hovered = Some(tile.node);
            tm.scale
        };
        let Some(tree) = self.state.borrow().tree.clone() else { return };
        let Some(ui) = self.ui() else { return };
        ui.set_treemap_hover_x(tile.rect.x / scale);
        ui.set_treemap_hover_y(tile.rect.y / scale);
        ui.set_treemap_hover_width(tile.rect.w / scale);
        ui.set_treemap_hover_height(tile.rect.h / scale);
        ui.set_treemap_hover_visible(true);
        let kind = tile.category.map_or("Ordner", Category::label);
        ui.set_status(
            format!("{}  ·  {}  ·  {kind}", tree.path(tile.node), format::bytes(tree.node(tile.node).size)).into(),
        );
    }

    fn treemap_left(&self) {
        self.treemap.borrow_mut().hovered = None;
        if let Some(ui) = self.ui() {
            ui.set_treemap_hover_visible(false);
        }
    }

    /// Click: select the table row (direct child of the current directory)
    /// that contains the clicked file.
    fn treemap_clicked(&self, x: f32, y: f32) {
        let Some(tile) = self.treemap_hit(x, y) else { return };
        let (tree, current) = {
            let st = self.state.borrow();
            (st.tree.clone(), st.current)
        };
        let Some(tree) = tree else { return };
        // The item itself (list view) or its ancestor that is a direct child
        // of the current directory (folder view).
        let chain = tree.ancestry(tile.node);
        let child = chain.iter().position(|&n| n == current).and_then(|pos| chain.get(pos + 1).copied());
        if let Some(row) = self.entry_model.row_of(tile.node).or_else(|| child.and_then(|c| self.entry_model.row_of(c)))
        {
            self.entry_select(row as i32);
        }
        if let Some(ui) = self.ui() {
            ui.set_status(format!("{}  ·  {}", tree.path(tile.node), format::bytes(tree.node(tile.node).size)).into());
            ui.invoke_focus_table();
        }
    }

    /// Double click: jump straight to the clicked item – into a folder, or to
    /// a file's folder with the file selected.
    fn treemap_double_clicked(&self, x: f32, y: f32) {
        let Some(tile) = self.treemap_hit(x, y) else { return };
        let Some(tree) = self.state.borrow().tree.clone() else { return };
        let node = tree.node(tile.node);
        if node.is_dir() && !node.has(flags::NOT_FOLLOWED) {
            self.navigate_to(tile.node, None);
        } else if let Some(parent) = tree.parent(tile.node) {
            self.navigate_to(parent, Some(tile.node));
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

/// Legend entries: categories present below `dir`, largest first.
fn legend(tree: &Tree, dir: NodeId) -> Vec<LegendItem> {
    let sums = filetype::totals(tree, dir);
    let total: u64 = sums.iter().sum();
    let mut cats: Vec<Category> = Category::ALL.into_iter().filter(|&c| sums[c as usize] > 0).collect();
    cats.sort_unstable_by_key(|&c| std::cmp::Reverse(sums[c as usize]));
    cats.into_iter()
        .map(|c| {
            let [r, g, b] = c.rgb();
            let bytes = sums[c as usize];
            LegendItem {
                color: Color::from_rgb_u8(r, g, b),
                text: format!(
                    "{} {} ({})",
                    c.label(),
                    format::bytes(bytes),
                    format::percent(query::fraction(bytes, total))
                )
                .into(),
            }
        })
        .collect()
}
