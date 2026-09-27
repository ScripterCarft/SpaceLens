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
use spacelens_core::duplicates::{self, DupGroup};
use spacelens_core::filetype::{self, Category};
use spacelens_core::safety::{self, Guard};
use spacelens_core::search::{self, Filter, Kind};
use spacelens_core::treemap::{Tile, Treemap};
use spacelens_core::{NodeId, ScanError, ScanOptions, ScanOutcome, ScanProgress, Tree, flags, format, query};
use spacelens_win::elevation::{self, RelaunchError};
use spacelens_win::known;
use spacelens_win::recycle::{self, RecycleReport};
use spacelens_win::volumes::{self, Volume};
use spacelens_win::{scanner_for, shell};

use crate::models::{COL_NAME, EntryModel, TreeModel, sort_for_column};
use crate::{CleanupRow, Crumb, LegendItem, MainWindow, NavKey, TreemapLabel};

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
    /// All locations of one cleanup finding, listed in the table.
    Locations(&'static str),
    /// Search results below the current directory.
    Search,
    /// Duplicate groups below the current directory.
    Duplicates,
}

impl View {
    /// Index of the tab that represents this view (see `Tabs` in app.slint).
    fn tab(self) -> i32 {
        match self {
            View::Folder => 0,
            View::Largest => 1,
            View::Search => 2,
            View::Duplicates => 3,
            View::Cleanup | View::Locations(_) => 4,
        }
    }
}

/// Items being moved to the recycle bin on a worker thread.
struct DeleteJob {
    handle: JoinHandle<Result<RecycleReport, String>>,
    items: Vec<(NodeId, String)>,
    /// Table row to select afterwards (keeps the position for quick successive deletes).
    row: i32,
    /// Set for cleanup-view jobs (bulk, errors skipped silently).
    cleanup: Option<&'static str>,
    /// Tree generation the node ids belong to (see [`State::generation`]).
    generation: u64,
}

/// Duplicate search running on a worker thread (holds a clone of the tree).
struct DupJob {
    progress: Arc<ScanProgress>,
    handle: JoinHandle<Option<Vec<DupGroup>>>,
    root: NodeId,
    generation: u64,
    started: Instant,
}

/// Last duplicate result and the folder it belongs to.
#[derive(Default)]
struct DupResult {
    root: Option<NodeId>,
    groups: Vec<DupGroup>,
}

struct State {
    dup_job: Option<DupJob>,
    tree: Option<Arc<Tree>>,
    /// Incremented whenever a different tree is shown, so results of work that
    /// started on an older tree (node ids!) are never applied to a newer one.
    generation: u64,
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
    search_timer: Timer,
    dup_timer: Timer,
    dups: RefCell<DupResult>,
    /// Matches and total size of the last search (for the header).
    search_summary: std::cell::Cell<(u64, u64)>,
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
            search_timer: Timer::default(),
            dup_timer: Timer::default(),
            dups: RefCell::new(DupResult::default()),
            search_summary: std::cell::Cell::new((0, 0)),
            findings: RefCell::new(Vec::new()),
            elevated,
            state: RefCell::new(State {
                tree: None,
                generation: 0,
                job: None,
                delete_job: None,
                dup_job: None,
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
        bind!(&app, ui, on_crumb_clicked => crumb_clicked(id));
        bind!(&app, ui, on_reveal_current => reveal_current());
        bind!(&app, ui, on_tree_toggle => tree_toggle(row));
        bind!(&app, ui, on_tree_select => tree_select(row));
        bind!(&app, ui, on_tree_navigate => tree_navigate(key));
        bind!(&app, ui, on_entry_click => entry_click(row, ctrl, shift));
        bind!(&app, ui, on_entry_select_all => entry_select_all());
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
        bind!(&app, ui, on_search_changed => search_changed(text));
        bind!(&app, ui, on_filters_changed => filters_changed());
        bind!(&app, ui, on_end_search => end_search());
        bind!(&app, ui, on_show_duplicates => show_duplicates());
        bind!(&app, ui, on_dupes_start => dupes_start());
        bind!(&app, ui, on_dupes_cancel => dupes_cancel());
        bind!(&app, ui, on_dupes_select_copies => dupes_select_copies());
        bind!(&app, ui, on_entry_navigate => entry_navigate(key, shift));
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

    // ------------------------------------------------------- elevation

    fn elevate(&self) {
        if self.state.borrow().delete_job.is_some() {
            self.set_status("Bitte warten, bis das Verschieben in den Papierkorb abgeschlossen ist.");
            return;
        }
        if self.state.borrow().dup_job.is_some() {
            self.set_status("Bitte zuerst die Duplikat-Suche abbrechen oder abwarten.");
            return;
        }
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

    /// Context bar (breadcrumb, prefix) and the totals in the status bar for
    /// the current view.
    fn update_location(&self, tree: &Tree) {
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

    fn crumb_clicked(&self, id: i32) {
        if let Ok(id) = NodeId::try_from(id) {
            self.navigate_to(id, None);
        }
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
            ui.set_tab(View::Largest.tab());
            ui.set_cleanup_panel(false);
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

    /// Plain selection of one row (keyboard, programmatic).
    fn entry_select(&self, row: i32) {
        self.entry_click(row, false, false);
    }

    /// Explorer-like selection: click = single, Ctrl = toggle, Shift = range
    /// from the anchor (Ctrl+Shift adds the range).
    fn entry_click(&self, row: i32, ctrl: bool, shift: bool) {
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

    fn entry_select_all(&self) {
        self.entry_model.select_all();
        let cursor = self.ui().map_or(0, |ui| ui.get_entry_selected().max(0) as usize);
        self.update_selection_status(cursor);
    }

    /// Status line: details of the row under the cursor, or the sum of a
    /// multi-selection.
    fn update_selection_status(&self, cursor: usize) {
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

    fn entry_open(&self, row: i32) {
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

    fn entry_reveal(&self, row: i32) {
        if let Some(id) = self.entry_model.id_at(row as usize) {
            self.reveal(id);
        }
    }

    fn entry_navigate(self: &Rc<Self>, key: NavKey, shift: bool) {
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

    fn sort_by(&self, column: i32) {
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
    fn delete_many(self: &Rc<Self>, tree: &Tree, selected: &[NodeId], row: i32) {
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
    fn owner_hwnd(&self) -> Option<isize> {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        let ui = self.ui()?;
        let handle = ui.window().window_handle();
        match handle.window_handle().ok()?.as_raw() {
            RawWindowHandle::Win32(h) => Some(h.hwnd.get()),
            _ => None,
        }
    }

    // ----------------------------------------------------------- search

    /// Most matches shown in the table (the largest ones).
    const SEARCH_LIMIT: usize = 5000;
    /// Typing pause before the search runs.
    const SEARCH_DEBOUNCE: Duration = Duration::from_millis(120);

    fn search_changed(self: &Rc<Self>, _text: SharedString) {
        let weak = Rc::downgrade(self);
        self.search_timer.start(TimerMode::SingleShot, Self::SEARCH_DEBOUNCE, move || {
            if let Some(app) = weak.upgrade() {
                app.run_search();
            }
        });
    }

    fn filters_changed(&self) {
        self.run_search();
    }

    fn end_search(&self) {
        if let Some(ui) = self.ui() {
            ui.set_search_text(SharedString::new());
        }
        self.show_folder_view();
    }

    /// Translates the search box and filter bar into a [`Filter`].
    fn current_filter(&self) -> Filter {
        let Some(ui) = self.ui() else { return Filter::default() };
        let type_index = ui.get_filter_type();
        let (kind, category) = match type_index {
            1..=10 => (Kind::Files, Category::ALL.get(type_index as usize - 1).copied()),
            11 => (Kind::Folders, None),
            _ => (Kind::All, None),
        };
        const SIZES: [u64; 6] = [0, 1 << 20, 10 << 20, 100 << 20, 1 << 30, 10 << 30];
        const DAYS: [u64; 6] = [0, 30, 182, 365, 730, 1825];
        let days = DAYS.get(ui.get_filter_age().max(0) as usize).copied().unwrap_or(0);
        let modified_before = if days == 0 {
            0
        } else {
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
            u32::try_from(now.saturating_sub(days * 86_400)).unwrap_or(u32::MAX)
        };
        Filter {
            query: ui.get_search_text().to_string(),
            kind,
            category,
            min_size: SIZES.get(ui.get_filter_size().max(0) as usize).copied().unwrap_or(0),
            modified_before,
        }
    }

    /// Searches below the current directory and shows the largest matches.
    fn run_search(&self) {
        let (tree, dir, column, descending) = {
            let st = self.state.borrow();
            (st.tree.clone(), st.current, st.sort_column, st.sort_descending)
        };
        let Some(tree) = tree else { return };
        let filter = self.current_filter();
        let started = Instant::now();
        let result = search::search(&tree, dir, &filter, Self::SEARCH_LIMIT);
        let elapsed = started.elapsed();

        self.state.borrow_mut().view = View::Search;
        self.search_summary.set((result.matches, result.size));
        self.entry_model.show_list(result.ids, dir, sort_for_column(column, descending));
        self.sync_view_flags();
        if let Some(ui) = self.ui() {
            ui.set_entry_selected(-1);
        }
        self.update_location(&tree);
        self.set_status(format!("Suche: {} Treffer in {} ms", format::count(result.matches), elapsed.as_millis()));
    }

    // ------------------------------------------------------- duplicates

    fn show_duplicates(&self) {
        if self.state.borrow().tree.is_none() {
            return;
        }
        self.state.borrow_mut().view = View::Duplicates;
        self.sync_view_flags();
        self.show_dup_result();
    }

    /// Shows the stored result if it belongs to the current folder.
    fn show_dup_result(&self) {
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

    fn dupes_start(self: &Rc<Self>) {
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
        const MIN: [u64; 4] = [100 << 10, 1 << 20, 10 << 20, 100 << 20];
        let min = MIN.get(ui.get_dupes_min().max(0) as usize).copied().unwrap_or(1 << 20);

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

    fn dupes_cancel(&self) {
        if let Some(job) = &self.state.borrow().dup_job {
            job.progress.cancel();
        }
    }

    fn poll_dupes(&self) {
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
    fn dupes_select_copies(&self) {
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
    fn prune_dup_result(&self, tree: &Tree) {
        let mut dups = self.dups.borrow_mut();
        for g in &mut dups.groups {
            g.ids.retain(|&id| !tree.node(id).has(flags::DELETED));
        }
        dups.groups.retain(|g| g.ids.len() > 1);
    }

    // ---------------------------------------------------------- cleanup

    /// Mirrors the current view into the UI's toggle flags.
    fn sync_view_flags(&self) {
        let view = self.state.borrow().view;
        if let Some(ui) = self.ui() {
            ui.set_tab(view.tab());
            ui.set_cleanup_panel(view == View::Cleanup);
        }
    }

    fn show_cleanup(&self) {
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

    /// A single location opens directly; several are listed in the table.
    fn cleanup_show(&self, index: i32) {
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

    fn cleanup_clean(self: &Rc<Self>, index: i32) {
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
    fn empty_bin(&self) {
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
