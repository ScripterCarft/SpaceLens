//! Treemap panel.

use super::*;

impl App {
    pub(super) fn treemap_resized(self: &Rc<Self>, width: f32, height: f32) {
        self.treemap.borrow_mut().logical = (width, height);
        let weak = Rc::downgrade(self);
        self.treemap_timer.start(TimerMode::SingleShot, TREEMAP_DEBOUNCE, move || {
            if let Some(app) = weak.upgrade() {
                app.render_treemap();
            }
        });
    }

    pub(super) fn clear_treemap(&self) {
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
    pub(super) fn render_treemap(&self) {
        let _timed = crate::diag::Timed::new("render_treemap");
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
    pub(super) fn treemap_hit(&self, x: f32, y: f32) -> Option<Tile> {
        let tm = self.treemap.borrow();
        tm.map.hit(x * tm.scale, y * tm.scale).copied()
    }

    pub(super) fn treemap_hovered(&self, x: f32, y: f32) {
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

    pub(super) fn treemap_left(&self) {
        self.treemap.borrow_mut().hovered = None;
        if let Some(ui) = self.ui() {
            ui.set_treemap_hover_visible(false);
        }
    }

    /// Click: select the table row (direct child of the current directory)
    /// that contains the clicked file.
    pub(super) fn treemap_clicked(&self, x: f32, y: f32) {
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
    pub(super) fn treemap_double_clicked(&self, x: f32, y: f32) {
        let Some(tile) = self.treemap_hit(x, y) else { return };
        let Some(tree) = self.state.borrow().tree.clone() else { return };
        let node = tree.node(tile.node);
        if node.is_dir() && !node.has(flags::NOT_FOLLOWED) {
            self.navigate_to(tile.node, None);
        } else if let Some(parent) = tree.parent(tile.node) {
            self.navigate_to(parent, Some(tile.node));
        }
    }
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
