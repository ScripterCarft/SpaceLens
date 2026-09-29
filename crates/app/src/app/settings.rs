//! Loading and saving user settings.

use super::*;

impl App {
    /// Applies persisted settings to a freshly created window.
    pub(super) fn apply_settings(ui: &MainWindow, s: &Settings) {
        ui.window().set_size(slint::LogicalSize::new(s.window.width, s.window.height));
        if s.window.maximized {
            ui.window().set_maximized(true);
        }
        ui.set_tree_width(s.layout.tree_width);
        ui.set_treemap_height(s.layout.treemap_height);
        ui.set_treemap_visible(s.layout.treemap_visible);
        let dupes = DUPE_MIN_SIZES
            .iter()
            .enumerate()
            .min_by_key(|(_, m)| m.abs_diff(s.duplicates.min_size))
            .map_or(1, |(i, _)| i as i32);
        ui.set_dupes_min(dupes);
        if !s.scan.last_path.is_empty() {
            ui.set_scan_path(s.scan.last_path.as_str().into());
        }
    }

    /// Collects the current state into the settings and writes them.
    /// Failures are silent: settings are a convenience, never a reason to
    /// block closing the app.
    pub(super) fn save_settings(&self) {
        let Some(ui) = self.ui() else { return };
        let mut s = self.settings.borrow_mut();
        let window = ui.window();
        let maximized = window.is_maximized();
        s.window.maximized = maximized;
        if !maximized && !window.is_minimized() {
            // Keep the restored size of a maximized window from the last run.
            let size = window.size().to_logical(window.scale_factor());
            s.window.width = size.width;
            s.window.height = size.height;
        }
        s.layout.tree_width = ui.get_tree_width();
        s.layout.treemap_height = ui.get_treemap_height();
        s.layout.treemap_visible = ui.get_treemap_visible();
        {
            let st = self.state.borrow();
            s.table.sort_column = column_key(st.sort_column).to_string();
            s.table.sort_descending = st.sort_descending;
        }
        let path = ui.get_scan_path().trim().to_string();
        if !path.is_empty() {
            s.scan.last_path = path;
        }
        if let Some(&min) = DUPE_MIN_SIZES.get(ui.get_dupes_min().max(0) as usize) {
            s.duplicates.min_size = min;
        }
        s.sanitize();
        if let Some(file) = &self.settings_path {
            let _ = s.save(file);
        }
    }
}
