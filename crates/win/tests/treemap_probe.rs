//! Manual performance probe: `cargo test --release -p spacelens-win --test treemap_probe -- --ignored --nocapture`

use std::path::Path;
use std::time::Instant;

use spacelens_core::treemap::Treemap;
use spacelens_core::{ScanOptions, ScanProgress, Scanner, Tree, filetype};

#[test]
#[ignore]
fn treemap_on_system_drive() {
    let out = spacelens_win::walker::WalkScanner
        .scan(Path::new("C:\\"), &ScanOptions::default(), &ScanProgress::new())
        .unwrap();
    let tree = out.tree;
    for (w, h) in [(1600u32, 450u32), (2560, 800)] {
        let t = Instant::now();
        let map = Treemap::layout(&tree, Tree::ROOT, w, h);
        let t_layout = t.elapsed();
        let mut buf = vec![0u8; (w * h * 3) as usize];
        let t = Instant::now();
        map.render(&mut buf, [30, 30, 30]);
        eprintln!("{w}x{h}: {} tiles, layout {t_layout:?}, render {:?}", map.tiles.len(), t.elapsed());
    }
    let t = Instant::now();
    let sums = filetype::totals(&tree, Tree::ROOT);
    eprintln!("category totals over {} nodes: {:?}", tree.len(), t.elapsed());
    for c in filetype::Category::ALL {
        eprintln!("  {:20} {}", c.label(), spacelens_core::format::bytes(sums[c as usize]));
    }
}

#[test]
#[ignore]
fn top_level_tiles() {
    let out = spacelens_win::walker::WalkScanner
        .scan(Path::new("C:\\"), &ScanOptions::default(), &ScanProgress::new())
        .unwrap();
    let tree = out.tree;
    let map = Treemap::layout(&tree, Tree::ROOT, 935, 250);
    for t in map.tiles.iter().filter(|t| t.depth == 0) {
        eprintln!(
            "{:30} x={:6.1} y={:6.1} w={:6.1} h={:6.1}",
            tree.name(t.node),
            t.rect.x,
            t.rect.y,
            t.rect.w,
            t.rect.h
        );
    }
}

#[test]
#[ignore]
fn search_speed_on_system_drive() {
    use spacelens_core::search::{Filter, Kind, search};
    let out = spacelens_win::walker::WalkScanner
        .scan(Path::new("C:\\"), &ScanOptions::default(), &ScanProgress::new())
        .unwrap();
    let tree = out.tree;
    let cases = [
        ("leer (alle Dateien)", Filter::default()),
        ("Text 'node_modules'", Filter { query: "node_modules".into(), ..Filter::default() }),
        ("Muster '*.dll'", Filter { query: "*.dll".into(), ..Filter::default() }),
        ("Zwei Wörter", Filter { query: "microsoft update".into(), ..Filter::default() }),
        ("Videos > 1 MB", Filter { category: Some(filetype::Category::Video), min_size: 1 << 20, ..Filter::default() }),
        ("Nur Ordner", Filter { kind: Kind::Folders, ..Filter::default() }),
    ];
    for (label, f) in cases {
        let t = Instant::now();
        let r = search(&tree, Tree::ROOT, &f, 5000);
        eprintln!(
            "{label:22} {:>8} Treffer  {:>10}  {:?}",
            r.matches,
            spacelens_core::format::bytes(r.size),
            t.elapsed()
        );
    }
}
