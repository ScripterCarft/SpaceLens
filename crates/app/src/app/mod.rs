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

use crate::models::{COL_NAME, EntryModel, TreeModel, column_from_key, column_key, sort_for_column};
use crate::{CleanupRow, Crumb, LegendItem, MainWindow, NavKey, TreemapLabel};
use spacelens_settings::Settings;

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
    settings: RefCell<Settings>,
    settings_path: Option<PathBuf>,
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

/// Minimum file sizes offered by the duplicate finder (combo box order).
const DUPE_MIN_SIZES: [u64; 4] = [100 << 10, 1 << 20, 10 << 20, 100 << 20];

impl App {
    pub fn new(ui: &MainWindow, elevated: bool, settings: Settings, settings_path: Option<PathBuf>) -> Rc<Self> {
        let sort_column = column_from_key(&settings.table.sort_column);
        let sort_descending = settings.table.sort_descending;
        Self::apply_settings(ui, &settings);
        let app = Rc::new(Self {
            settings: RefCell::new(settings),
            settings_path,
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
                sort_column,
                sort_descending,
            }),
        });

        // Closing via the title bar: persist the settings while the window
        // still has its final size.
        let weak = Rc::downgrade(&app);
        ui.window().on_close_requested(move || {
            if let Some(app) = weak.upgrade() {
                app.save_settings();
            }
            slint::CloseRequestResponse::HideWindow
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
}

mod cleanup_view;
mod delete;
mod drives;
mod duplicates_view;
mod lifecycle;
mod navigation;
mod scan;
mod search_view;
mod settings;
mod treemap;
