//! Squarified treemap: layout and software rendering.
//!
//! The layout follows Bruls, Huizing & van Wijk, "Squarified Treemaps"
//! (2000): children are placed in rows whose aspect ratios are kept close to
//! 1, which makes tiles easy to compare and to hit with the mouse.
//!
//! Rendering writes straight into an RGB buffer (no GPU, no per-tile UI
//! elements), so even 100 000 visible tiles cost only a few milliseconds.
//! Every file gets a "cushion" – a radial brightness falloff – so adjacent
//! tiles of the same colour stay distinguishable without thick borders.

use crate::filetype::{self, Category};
use crate::tree::{NodeId, Tree};

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    #[inline]
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && y >= self.y && x < self.x + self.w && y < self.y + self.h
    }

    fn inset(&self, by: f32) -> Rect {
        Rect { x: self.x + by, y: self.y + by, w: (self.w - 2.0 * by).max(0.0), h: (self.h - 2.0 * by).max(0.0) }
    }
}

/// One placed node. `category` is `None` for directories.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tile {
    pub node: NodeId,
    pub rect: Rect,
    pub depth: u16,
    pub category: Option<Category>,
    /// Directory whose children were laid out inside it.
    pub expanded: bool,
}

/// Layout of one directory, tiles in pre-order (parents before children).
#[derive(Debug, Default)]
pub struct Treemap {
    pub tiles: Vec<Tile>,
    pub width: u32,
    pub height: u32,
}

/// Directories smaller than this (in pixels, either side) are not subdivided.
const MIN_EXPAND_PX: f32 = 6.0;
/// Upper bound on tiles; beyond it directories are drawn as solid blocks.
const MAX_TILES: usize = 400_000;
const MAX_DEPTH: u16 = 128;

impl Treemap {
    /// Lays out the subtree of `root` in a `width` × `height` pixel area.
    pub fn layout(tree: &Tree, root: NodeId, width: u32, height: u32) -> Self {
        let mut map = Treemap { tiles: Vec::new(), width, height };
        if width == 0 || height == 0 || tree.get(root).is_none() {
            return map;
        }
        let bounds = Rect { x: 0.0, y: 0.0, w: width as f32, h: height as f32 };
        if tree.node(root).is_dir() {
            map.place_children(tree, root, bounds, 0);
        } else {
            map.tiles.push(Tile {
                node: root,
                rect: bounds,
                depth: 0,
                category: Some(filetype::category_of(tree, root)),
                expanded: false,
            });
        }
        map
    }

    fn place_children(&mut self, tree: &Tree, dir: NodeId, rect: Rect, depth: u16) {
        let mut items: Vec<(f64, NodeId)> = tree
            .live_children(dir)
            .filter_map(|c| {
                let s = tree.node(c).size;
                (s > 0).then_some((s as f64, c))
            })
            .collect();
        if items.is_empty() || rect.w < 1.0 || rect.h < 1.0 {
            return;
        }
        items.sort_unstable_by(|a, b| b.0.total_cmp(&a.0));
        let total: f64 = items.iter().map(|i| i.0).sum();
        let scale = f64::from(rect.w) * f64::from(rect.h) / total;
        for item in &mut items {
            item.0 *= scale;
        }

        let mut placed = Vec::with_capacity(items.len());
        squarify(&items, rect, &mut placed);

        for (id, r) in placed {
            if r.w < 0.5 || r.h < 0.5 {
                continue; // invisible; not worth memory or hit-testing
            }
            let node = tree.node(id);
            let is_dir = node.is_dir();
            let expand = is_dir
                && depth < MAX_DEPTH
                && r.w >= MIN_EXPAND_PX
                && r.h >= MIN_EXPAND_PX
                && self.tiles.len() < MAX_TILES;
            self.tiles.push(Tile {
                node: id,
                rect: r,
                depth,
                category: (!is_dir).then(|| filetype::category_of(tree, id)),
                expanded: expand,
            });
            if expand {
                self.place_children(tree, id, r.inset(1.0), depth + 1);
            }
        }
    }

    /// Deepest tile under the point (pre-order ⇒ the last match is deepest).
    pub fn hit(&self, x: f32, y: f32) -> Option<&Tile> {
        self.tiles.iter().rev().find(|t| t.rect.contains(x, y))
    }

    /// Top-level tile (depth 0) that contains `node`'s tile, if visible.
    pub fn top_level_of(&self, x: f32, y: f32) -> Option<&Tile> {
        self.tiles.iter().find(|t| t.depth == 0 && t.rect.contains(x, y))
    }

    /// Renders into `out` (RGB, `width * height * 3` bytes).
    pub fn render(&self, out: &mut [u8], background: [u8; 3]) {
        let (w, h) = (self.width as usize, self.height as usize);
        if out.len() < w * h * 3 {
            return;
        }
        for px in out.as_chunks_mut::<3>().0.iter_mut().take(w * h) {
            *px = background;
        }
        let border = shade(background, 0.55);
        let unresolved = [0x9A, 0x9A, 0x9A];
        let mut fx = Vec::new();
        let mut fy = Vec::new();

        for t in &self.tiles {
            let (x0, y0) = (t.rect.x.round().max(0.0) as usize, t.rect.y.round().max(0.0) as usize);
            let x1 = ((t.rect.x + t.rect.w).round() as usize).min(w);
            let y1 = ((t.rect.y + t.rect.h).round() as usize).min(h);
            if x1 <= x0 || y1 <= y0 {
                continue;
            }
            if t.expanded {
                // Only the 1px frame; children cover the interior.
                frame(out, w, x0, y0, x1, y1, border);
                continue;
            }
            let base = t.category.map_or(unresolved, Category::rgb);
            cushion(out, w, x0, y0, x1, y1, base, &mut fx, &mut fy);
        }
    }
}

/// Squarified layout of `items` (areas in px², sorted descending) into `rect`.
fn squarify(items: &[(f64, NodeId)], rect: Rect, out: &mut Vec<(NodeId, Rect)>) {
    let (mut x, mut y, mut w, mut h) = (f64::from(rect.x), f64::from(rect.y), f64::from(rect.w), f64::from(rect.h));
    let mut i = 0;
    while i < items.len() {
        let short = w.min(h).max(f64::MIN_POSITIVE);
        let mut sum = items[i].0;
        let mut worst = worst_ratio(sum, items[i].0, items[i].0, short);
        let mut j = i + 1;
        while j < items.len() {
            let next_sum = sum + items[j].0;
            let next_worst = worst_ratio(next_sum, items[i].0, items[j].0, short);
            if next_worst > worst {
                break;
            }
            sum = next_sum;
            worst = next_worst;
            j += 1;
        }
        let last_row = j == items.len();
        if w >= h {
            // Column on the left side.
            let col_w = if last_row { w } else { (sum / h).min(w) };
            let mut cy = y;
            for (k, &(area, id)) in items[i..j].iter().enumerate() {
                let ih = if k == j - i - 1 { y + h - cy } else { area / col_w.max(f64::MIN_POSITIVE) };
                out.push((id, Rect { x: x as f32, y: cy as f32, w: col_w as f32, h: ih as f32 }));
                cy += ih;
            }
            x += col_w;
            w -= col_w;
        } else {
            // Row along the top.
            let row_h = if last_row { h } else { (sum / w).min(h) };
            let mut cx = x;
            for (k, &(area, id)) in items[i..j].iter().enumerate() {
                let iw = if k == j - i - 1 { x + w - cx } else { area / row_h.max(f64::MIN_POSITIVE) };
                out.push((id, Rect { x: cx as f32, y: y as f32, w: iw as f32, h: row_h as f32 }));
                cx += iw;
            }
            y += row_h;
            h -= row_h;
        }
        i = j;
    }
}

/// Worst aspect ratio of a row with total area `sum` whose largest/smallest
/// items are `max`/`min`, laid along a side of length `short`.
#[inline]
fn worst_ratio(sum: f64, max: f64, min: f64, short: f64) -> f64 {
    let s2 = short * short;
    let sum2 = sum * sum;
    (s2 * max / sum2).max(sum2 / (s2 * min.max(f64::MIN_POSITIVE)))
}

#[inline]
fn shade(c: [u8; 3], f: f32) -> [u8; 3] {
    c.map(|v| (f32::from(v) * f).clamp(0.0, 255.0) as u8)
}

fn frame(out: &mut [u8], w: usize, x0: usize, y0: usize, x1: usize, y1: usize, c: [u8; 3]) {
    let mut put = |x: usize, y: usize| {
        let i = (y * w + x) * 3;
        out[i..i + 3].copy_from_slice(&c);
    };
    for x in x0..x1 {
        put(x, y0);
        put(x, y1 - 1);
    }
    for y in y0..y1 {
        put(x0, y);
        put(x1 - 1, y);
    }
}

/// Fills a tile with a cushion: brightness falls off quadratically from the
/// centre (separable, so the per-pixel cost is one add and three multiplies).
#[allow(clippy::too_many_arguments)]
fn cushion(
    out: &mut [u8],
    w: usize,
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
    base: [u8; 3],
    fx: &mut Vec<f32>,
    fy: &mut Vec<f32>,
) {
    let axis = |from: usize, to: usize, v: &mut Vec<f32>| {
        v.clear();
        let n = (to - from) as f32;
        v.extend((0..to - from).map(|i| {
            let t = (i as f32 + 0.5) / n * 2.0 - 1.0;
            t * t
        }));
    };
    axis(x0, x1, fx);
    axis(y0, y1, fy);
    let [r, g, b] = base.map(f32::from);
    for (row, &dy) in fy.iter().enumerate() {
        let line = ((y0 + row) * w + x0) * 3;
        for (col, &dx) in fx.iter().enumerate() {
            let f = 1.12 - 0.30 * (dx + dy) * 0.5 - 0.12 * dx.max(dy);
            let i = line + col * 3;
            out[i] = (r * f).clamp(0.0, 255.0) as u8;
            out[i + 1] = (g * f).clamp(0.0, 255.0) as u8;
            out[i + 2] = (b * f).clamp(0.0, 255.0) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builder::{RawEntry, TreeBuilder};
    use crate::tree::flags;

    fn sample() -> Tree {
        // root: big.mkv 600, docs/ (a.pdf 200, b.txt 100), small.zip 100
        let names = b"big.mkvdocssmall.zipa.pdfb.txt";
        let e = |off: u32, len: u16, size: u64, dir: bool| RawEntry {
            size,
            alloc: size,
            mtime: 0,
            name_off: off,
            name_len: len,
            flags: if dir { flags::DIR } else { 0 },
        };
        let mut b = TreeBuilder::new("X:\\");
        let mut sub = Vec::new();
        b.add_listing(0, &[e(0, 7, 600, false), e(7, 4, 0, true), e(11, 9, 100, false)], names, |id| sub.push(id))
            .unwrap();
        b.add_listing(sub[0], &[e(20, 5, 200, false), e(25, 5, 100, false)], names, |_| {}).unwrap();
        b.finish()
    }

    #[test]
    fn areas_are_proportional_and_cover_the_bounds() {
        let t = sample();
        let map = Treemap::layout(&t, 0, 200, 100);
        let top: Vec<&Tile> = map.tiles.iter().filter(|t| t.depth == 0).collect();
        assert_eq!(top.len(), 3);
        let area: f32 = top.iter().map(|t| t.rect.w * t.rect.h).sum();
        assert!((area - 20_000.0).abs() < 1.0, "{area}");
        let big = top.iter().find(|t| t.rect.w * t.rect.h > 11_000.0).unwrap();
        assert!((big.rect.w * big.rect.h - 12_000.0).abs() < 1.0, "600/1000 of the area");
        for t in &top {
            assert!(t.rect.x >= 0.0 && t.rect.y >= 0.0);
            assert!(t.rect.x + t.rect.w <= 200.01 && t.rect.y + t.rect.h <= 100.01);
        }
    }

    #[test]
    fn nests_directories_and_hits_deepest_tile() {
        let t = sample();
        let map = Treemap::layout(&t, 0, 200, 100);
        let docs = map.tiles.iter().find(|x| t.name(x.node) == "docs").unwrap();
        assert!(docs.expanded && docs.category.is_none());
        let pdf = map.tiles.iter().find(|x| t.name(x.node) == "a.pdf").unwrap();
        assert_eq!(pdf.depth, 1);
        assert_eq!(pdf.category, Some(Category::Document));
        let (cx, cy) = (pdf.rect.x + pdf.rect.w / 2.0, pdf.rect.y + pdf.rect.h / 2.0);
        assert_eq!(map.hit(cx, cy).map(|x| x.node), Some(pdf.node));
        assert_eq!(map.top_level_of(cx, cy).map(|x| x.node), Some(docs.node));
    }

    #[test]
    fn squarified_rows_keep_aspect_ratio_reasonable() {
        let items: Vec<(f64, NodeId)> = [6.0, 6.0, 4.0, 3.0, 2.0, 2.0, 1.0].iter().map(|&a| (a * 100.0, 0)).collect();
        let mut out = Vec::new();
        squarify(&items, Rect { x: 0.0, y: 0.0, w: 60.0, h: 40.0 }, &mut out);
        let worst = out.iter().map(|(_, r)| (r.w / r.h).max(r.h / r.w)).fold(0.0, f32::max);
        assert!(worst < 3.0, "worst aspect {worst}");
    }

    #[test]
    fn renders_without_panicking_on_odd_sizes() {
        let t = sample();
        for (w, h) in [(1, 1), (3, 200), (517, 13), (0, 10)] {
            let map = Treemap::layout(&t, 0, w, h);
            let mut buf = vec![0u8; (w * h * 3) as usize];
            map.render(&mut buf, [20, 20, 20]);
        }
    }
}
