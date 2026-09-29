//! Search tab.

use super::*;

impl App {
    /// Most matches shown in the table (the largest ones).
    pub(super) const SEARCH_LIMIT: usize = 5000;
    /// Typing pause before the search runs.
    pub(super) const SEARCH_DEBOUNCE: Duration = Duration::from_millis(120);

    pub(super) fn search_changed(self: &Rc<Self>, _text: SharedString) {
        let weak = Rc::downgrade(self);
        self.search_timer.start(TimerMode::SingleShot, Self::SEARCH_DEBOUNCE, move || {
            if let Some(app) = weak.upgrade() {
                app.run_search();
            }
        });
    }

    pub(super) fn filters_changed(&self) {
        self.run_search();
    }

    pub(super) fn end_search(&self) {
        if let Some(ui) = self.ui() {
            ui.set_search_text(SharedString::new());
        }
        self.show_folder_view();
    }

    /// Translates the search box and filter bar into a [`Filter`].
    pub(super) fn current_filter(&self) -> Filter {
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
    pub(super) fn run_search(&self) {
        let _timed = crate::diag::Timed::new("run_search");
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
}
