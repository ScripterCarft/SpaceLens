//! Classification of files into coarse categories by extension.
//!
//! Used for treemap colours and the per-category statistics. Deliberately
//! extension-based: it needs no file access and costs one table lookup.

use std::cmp::Ordering;

use crate::tree::{NodeId, Tree, flags};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum Category {
    Video,
    Audio,
    Image,
    Archive,
    DiskImage,
    Program,
    Document,
    Code,
    System,
    Other,
}

impl Category {
    pub const ALL: [Category; 10] = [
        Category::Video,
        Category::Audio,
        Category::Image,
        Category::Archive,
        Category::DiskImage,
        Category::Program,
        Category::Document,
        Category::Code,
        Category::System,
        Category::Other,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Video => "Videos",
            Self::Audio => "Musik & Audio",
            Self::Image => "Bilder",
            Self::Archive => "Archive",
            Self::DiskImage => "Disk-Images & VMs",
            Self::Program => "Programme",
            Self::Document => "Dokumente",
            Self::Code => "Code & Daten",
            Self::System => "System",
            Self::Other => "Sonstiges",
        }
    }

    /// Base colour as RGB. Mid-saturation tones that stay readable in light
    /// and dark themes.
    pub fn rgb(self) -> [u8; 3] {
        match self {
            Self::Video => [0xD9, 0x5B, 0x5B],
            Self::Audio => [0xE0, 0x8E, 0x3C],
            Self::Image => [0xD8, 0xB9, 0x3B],
            Self::Archive => [0x8C, 0x6B, 0xC8],
            Self::DiskImage => [0xB0, 0x5C, 0xA8],
            Self::Program => [0x4F, 0x8F, 0xD6],
            Self::Document => [0x4B, 0xB0, 0x8E],
            Self::Code => [0x6F, 0xB5, 0x4A],
            Self::System => [0x7D, 0x86, 0x94],
            Self::Other => [0xA8, 0xA0, 0x94],
        }
    }
}

/// Lower-cased extension (without dot) of a WTF-8 name, if short enough to
/// be a real extension.
fn extension(name: &[u8]) -> Option<([u8; 8], usize)> {
    let dot = name.iter().rposition(|&b| b == b'.')?;
    let ext = &name[dot + 1..];
    if dot == 0 || ext.is_empty() || ext.len() > 8 {
        return None;
    }
    let mut buf = [0u8; 8];
    for (d, s) in buf.iter_mut().zip(ext) {
        *d = s.to_ascii_lowercase();
    }
    Some((buf, ext.len()))
}

/// Sorted table of extension → category (binary searched).
const TABLE: &[(&str, Category)] = {
    use Category::*;
    &[
        ("7z", Archive),
        ("aac", Audio),
        ("ai", Image),
        ("apk", Archive),
        ("appx", Program),
        ("asar", Program),
        ("avi", Video),
        ("avif", Image),
        ("bak", Archive),
        ("bat", Code),
        ("bin", DiskImage),
        ("bmp", Image),
        ("bz2", Archive),
        ("c", Code),
        ("cab", Archive),
        ("cat", System),
        ("cfg", Code),
        ("cpp", Code),
        ("cs", Code),
        ("csv", Document),
        ("cur", System),
        ("dat", System),
        ("db", Code),
        ("deb", Archive),
        ("dll", Program),
        ("dmg", DiskImage),
        ("doc", Document),
        ("docx", Document),
        ("drv", System),
        ("dylib", Program),
        ("edb", System),
        ("epub", Document),
        ("etl", System),
        ("evtx", System),
        ("exe", Program),
        ("flac", Audio),
        ("flv", Video),
        ("gif", Image),
        ("go", Code),
        ("gz", Archive),
        ("h", Code),
        ("heic", Image),
        ("hpp", Code),
        ("htm", Document),
        ("html", Document),
        ("ico", Image),
        ("img", DiskImage),
        ("ini", Code),
        ("iso", DiskImage),
        ("jar", Program),
        ("java", Code),
        ("jpeg", Image),
        ("jpg", Image),
        ("js", Code),
        ("json", Code),
        ("jxl", Image),
        ("key", Document),
        ("lib", Program),
        ("log", System),
        ("lua", Code),
        ("m4a", Audio),
        ("m4v", Video),
        ("md", Document),
        ("mid", Audio),
        ("mkv", Video),
        ("mov", Video),
        ("mp3", Audio),
        ("mp4", Video),
        ("mpeg", Video),
        ("mpg", Video),
        ("msi", Program),
        ("msix", Program),
        ("msp", Program),
        ("mui", System),
        ("mum", System),
        ("node", Program),
        ("o", Code),
        ("obj", Code),
        ("odp", Document),
        ("ods", Document),
        ("odt", Document),
        ("ogg", Audio),
        ("opus", Audio),
        ("otf", System),
        ("pak", Program),
        ("pdb", Code),
        ("pdf", Document),
        ("php", Code),
        ("png", Image),
        ("ppt", Document),
        ("pptx", Document),
        ("ps1", Code),
        ("psd", Image),
        ("py", Code),
        ("qcow2", DiskImage),
        ("rar", Archive),
        ("raw", Image),
        ("rlib", Code),
        ("rpm", Archive),
        ("rs", Code),
        ("rtf", Document),
        ("sh", Code),
        ("so", Program),
        ("sql", Code),
        ("sqlite", Code),
        ("svg", Image),
        ("sys", System),
        ("tar", Archive),
        ("tif", Image),
        ("tiff", Image),
        ("tmp", System),
        ("toml", Code),
        ("ts", Code),
        ("ttf", System),
        ("txt", Document),
        ("vdi", DiskImage),
        ("vhd", DiskImage),
        ("vhdx", DiskImage),
        ("vmdk", DiskImage),
        ("vob", Video),
        ("wasm", Program),
        ("wav", Audio),
        ("webm", Video),
        ("webp", Image),
        ("wim", DiskImage),
        ("wma", Audio),
        ("wmv", Video),
        ("woff", System),
        ("woff2", System),
        ("xls", Document),
        ("xlsx", Document),
        ("xml", Code),
        ("xz", Archive),
        ("yaml", Code),
        ("yml", Code),
        ("zip", Archive),
        ("zst", Archive),
    ]
};

/// Category of a file node. Directories are never passed in.
pub fn category_of(tree: &Tree, id: NodeId) -> Category {
    let node = tree.node(id);
    if let Some((ext, len)) = extension(tree.name_bytes(id)) {
        let ext = &ext[..len];
        if let Ok(i) = TABLE.binary_search_by(|(e, _)| cmp_bytes(e.as_bytes(), ext)) {
            return TABLE[i].1;
        }
    }
    if node.has(flags::SYSTEM) { Category::System } else { Category::Other }
}

fn cmp_bytes(a: &[u8], b: &[u8]) -> Ordering {
    a.cmp(b)
}

/// Bytes per category in the subtree of `id` (index = `Category as usize`).
/// Iterative DFS, touches every node of the subtree once.
pub fn totals(tree: &Tree, id: NodeId) -> [u64; Category::ALL.len()] {
    let mut sums = [0u64; Category::ALL.len()];
    let mut stack = vec![id];
    while let Some(n) = stack.pop() {
        let node = tree.node(n);
        if node.is_dir() {
            stack.extend(tree.live_children(n));
        } else if node.size > 0 {
            sums[category_of(tree, n) as usize] += node.size;
        }
    }
    sums
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builder::{RawEntry, TreeBuilder};

    #[test]
    fn table_is_sorted_and_unique() {
        for w in TABLE.windows(2) {
            assert!(w[0].0 < w[1].0, "{} >= {}", w[0].0, w[1].0);
        }
    }

    fn tree(names: &[(&str, u64, u16)]) -> Tree {
        let mut bytes = Vec::new();
        let entries: Vec<RawEntry> = names
            .iter()
            .map(|&(n, size, f)| {
                let off = bytes.len() as u32;
                bytes.extend_from_slice(n.as_bytes());
                RawEntry { size, alloc: size, mtime: 0, name_off: off, name_len: n.len() as u16, flags: f }
            })
            .collect();
        let mut b = TreeBuilder::new("X:\\");
        b.add_listing(0, &entries, &bytes, |_| {}).unwrap();
        b.finish()
    }

    #[test]
    fn classifies_by_extension_case_insensitive() {
        let t = tree(&[
            ("Film.MKV", 10, 0),
            ("song.mp3", 5, 0),
            ("setup.exe", 3, 0),
            ("README", 1, 0),
            ("pagefile.sys", 7, flags::SYSTEM),
            (".gitignore", 1, 0),
            ("disk.vhdx", 100, 0),
        ]);
        let cats: Vec<Category> = (1..t.len() as u32).map(|i| category_of(&t, i)).collect();
        assert_eq!(
            cats,
            vec![
                Category::Video,
                Category::Audio,
                Category::Program,
                Category::Other,
                Category::System,
                Category::Other,
                Category::DiskImage
            ]
        );
        let sums = totals(&t, 0);
        assert_eq!(sums[Category::Video as usize], 10);
        assert_eq!(sums[Category::DiskImage as usize], 100);
        assert_eq!(sums.iter().sum::<u64>(), 127);
    }
}
