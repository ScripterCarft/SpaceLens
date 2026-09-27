//! Duplicate detection, I/O part: hashing the size-matched candidates.
//!
//! Two passes, each dropping files that turn out to be unique:
//! 1. **Probe**: hash of the first and last 16 KiB (32 KiB read per file).
//!    Separates almost all same-sized but different files.
//! 2. **Full**: streaming XXH3-128 over the whole content, only for what is
//!    left after the probe.
//!
//! Files are opened read-only with full sharing (never blocks other programs)
//! and `FILE_FLAG_SEQUENTIAL_SCAN`. Unreadable files are simply left out.

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::os::windows::ffi::OsStringExt;
use std::os::windows::fs::OpenOptionsExt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use spacelens_core::duplicates::{self, DupGroup};
use spacelens_core::{NodeId, ScanProgress, Tree};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_SEQUENTIAL_SCAN, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
};
use xxhash_rust::xxh3::Xxh3;

const PROBE: u64 = 16 * 1024;
const CHUNK: usize = 1 << 20;

/// Finds groups of identical files among `candidates` (same-size groups from
/// [`duplicates::candidates`]). Reports files and bytes read to `progress`
/// and returns `None` if it was cancelled.
pub fn find(
    tree: &Tree,
    candidates: Vec<Vec<NodeId>>,
    progress: &ScanProgress,
    threads: usize,
) -> Option<Vec<DupGroup>> {
    let threads = threads.max(1);

    // Pass 1: probe (for small files the probe already covers everything).
    let flat: Vec<NodeId> = candidates.iter().flatten().copied().collect();
    progress.set_current("Vergleiche Anfang und Ende der Dateien …");
    let probes = hash_all(tree, &flat, progress, threads, Mode::Probe)?;
    let mut offset = 0;
    let mut after_probe = Vec::new();
    for group in &candidates {
        let keys = &probes[offset..offset + group.len()];
        offset += group.len();
        after_probe.extend(duplicates::regroup(group, keys));
    }

    // Pass 2: full content for files larger than the probe window.
    let (small, large): (Vec<Vec<NodeId>>, Vec<Vec<NodeId>>) =
        after_probe.into_iter().partition(|g| tree.node(g[0]).size <= 2 * PROBE);
    let flat: Vec<NodeId> = large.iter().flatten().copied().collect();
    progress.set_current("Vergleiche den vollständigen Inhalt …");
    let full = hash_all(tree, &flat, progress, threads, Mode::Full)?;
    let mut offset = 0;
    let mut groups: Vec<DupGroup> =
        small.into_iter().map(|ids| DupGroup { size: tree.node(ids[0]).size, ids }).collect();
    for group in &large {
        let keys = &full[offset..offset + group.len()];
        offset += group.len();
        for ids in duplicates::regroup(group, keys) {
            groups.push(DupGroup { size: tree.node(ids[0]).size, ids });
        }
    }
    duplicates::sort_groups(&mut groups);
    Some(groups)
}

#[derive(Clone, Copy)]
enum Mode {
    Probe,
    Full,
}

/// Hashes `ids` on `threads` workers; result index = input index.
fn hash_all(
    tree: &Tree,
    ids: &[NodeId],
    progress: &ScanProgress,
    threads: usize,
    mode: Mode,
) -> Option<Vec<Option<u128>>> {
    let next = AtomicUsize::new(0);
    let mut results = vec![None; ids.len()];
    let chunks: Vec<Vec<(usize, Option<u128>)>> = thread::scope(|s| {
        let handles: Vec<_> = (0..threads.min(ids.len().max(1)))
            .map(|_| {
                s.spawn(|| {
                    let mut out = Vec::new();
                    let mut buf = vec![0u8; CHUNK];
                    loop {
                        if progress.is_cancelled() {
                            break;
                        }
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(&id) = ids.get(i) else { break };
                        let size = tree.node(id).size;
                        let hash = open(tree, id).and_then(|f| match mode {
                            Mode::Probe => probe(f, size, &mut buf),
                            Mode::Full => full(f, &mut buf),
                        });
                        let read = match mode {
                            Mode::Probe => size.min(2 * PROBE),
                            Mode::Full => size,
                        };
                        progress.add(1, 0, read);
                        out.push((i, hash));
                    }
                    out
                })
            })
            .collect();
        handles.into_iter().filter_map(|h| h.join().ok()).collect()
    });
    if progress.is_cancelled() {
        return None;
    }
    for (i, hash) in chunks.into_iter().flatten() {
        results[i] = hash;
    }
    Some(results)
}

fn open(tree: &Tree, id: NodeId) -> Option<File> {
    let mut wide: Vec<u16> = r"\\?\".encode_utf16().collect();
    let path = tree.wide_path(id);
    if path.starts_with(&wide) {
        wide.clear();
    }
    wide.extend_from_slice(&path);
    OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_SEQUENTIAL_SCAN)
        .open(OsString::from_wide(&wide))
        .ok()
}

fn probe(mut f: File, size: u64, buf: &mut [u8]) -> Option<u128> {
    let mut h = Xxh3::new();
    h.update(&size.to_le_bytes());
    let head = size.min(PROBE) as usize;
    f.read_exact(&mut buf[..head]).ok()?;
    h.update(&buf[..head]);
    if size > PROBE {
        let tail_start = size.saturating_sub(PROBE).max(PROBE);
        let tail = (size - tail_start) as usize;
        f.seek(SeekFrom::Start(tail_start)).ok()?;
        f.read_exact(&mut buf[..tail]).ok()?;
        h.update(&buf[..tail]);
    }
    Some(h.digest128())
}

fn full(mut f: File, buf: &mut [u8]) -> Option<u128> {
    let mut h = Xxh3::new();
    loop {
        match f.read(buf) {
            Ok(0) => return Some(h.digest128()),
            Ok(n) => h.update(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spacelens_core::{ScanOptions, Scanner};
    use std::fs;

    #[test]
    fn finds_identical_files_only() {
        let root = std::env::temp_dir().join(format!("spacelens-dupes-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("a/b")).unwrap();
        let big: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let mut big_changed = big.clone();
        big_changed[100_000] ^= 0xFF; // same size, same head and tail, different middle
        fs::write(root.join("one.bin"), &big).unwrap();
        fs::write(root.join("a/two.bin"), &big).unwrap();
        fs::write(root.join("a/b/three.bin"), &big).unwrap();
        fs::write(root.join("near.bin"), &big_changed).unwrap();
        fs::write(root.join("small1.txt"), b"hello world").unwrap();
        fs::write(root.join("a/small2.txt"), b"hello world").unwrap();
        fs::write(root.join("other.txt"), b"hello there").unwrap();

        let out = crate::walker::WalkScanner.scan(&root, &ScanOptions::default(), &ScanProgress::new()).unwrap();
        let tree = out.tree;
        let cands = duplicates::candidates(&tree, Tree::ROOT, 1);
        let progress = ScanProgress::new();
        let groups = find(&tree, cands, &progress, 3).unwrap();
        let _ = fs::remove_dir_all(&root);

        let named: Vec<Vec<String>> = groups
            .iter()
            .map(|g| {
                let mut v: Vec<String> = g.ids.iter().map(|&i| tree.name(i).into_owned()).collect();
                v.sort();
                v
            })
            .collect();
        assert_eq!(named, vec![vec!["one.bin", "three.bin", "two.bin"], vec!["small1.txt", "small2.txt"]]);
        assert_eq!(groups[0].wasted(), 400_000);
    }

    #[test]
    fn cancellation_returns_none() {
        let progress = ScanProgress::new();
        progress.cancel();
        let tree = spacelens_core::TreeBuilder::new("X:\\").finish();
        assert!(find(&tree, vec![vec![0, 0]], &progress, 2).is_none());
    }
}
