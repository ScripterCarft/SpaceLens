//! Parallel directory walker based on `GetFileInformationByHandleEx`.
//!
//! # Design
//!
//! * **Workers** (N threads) take a directory from the task queue, open it
//!   once and read its entries in 64 KiB batches with
//!   `FileIdExtdDirectoryInfo` (one syscall returns hundreds of entries incl.
//!   sizes, attributes and reparse tags). Names are converted to WTF-8 on the
//!   worker, so the expensive per-entry work is parallel.
//! * **One coordinator** (the calling thread) owns the [`TreeBuilder`]. It
//!   appends each finished listing as a contiguous block, assigns ids to the
//!   subdirectories and enqueues them as new tasks. Its work per entry is a
//!   `memcpy`, so a single coordinator keeps up with many workers and the tree
//!   needs no locks.
//! * Files are **never opened**: everything comes from the directory listing.
//!   This keeps the scan read-only, avoids sharing violations and never
//!   triggers downloads of cloud placeholders (OneDrive etc.).
//! * Name-surrogate reparse points (symlinks, junctions, mount points) are
//!   recorded but not followed, which prevents cycles and double counting.
//!
//! Hard links are counted once per link, like Explorer does; exact
//! de-duplication needs link counts from the MFT scanner.

use std::os::windows::io::{FromRawHandle, OwnedHandle};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, Instant};
use std::{mem, ptr, thread};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, bounded, unbounded};
use spacelens_core::names::push_wtf8;
use spacelens_core::{
    NodeId, RawEntry, ScanError, ScanOptions, ScanOutcome, ScanProgress, Scanner, TreeBuilder, flags,
};
use windows_sys::Win32::Foundation::{
    ERROR_INVALID_FUNCTION, ERROR_INVALID_PARAMETER, ERROR_NO_MORE_FILES, ERROR_NOT_SUPPORTED, GetLastError,
    INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_COMPRESSED, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_OFFLINE,
    FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS, FILE_ATTRIBUTE_RECALL_ON_OPEN, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_ATTRIBUTE_SPARSE_FILE, FILE_ATTRIBUTE_SYSTEM, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_FULL_DIR_INFO, FILE_ID_EXTD_DIR_INFO, FILE_INFO_BY_HANDLE_CLASS, FILE_LIST_DIRECTORY, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, FileFullDirectoryInfo, FileIdExtdDirectoryInfo, GetFileInformationByHandleEx,
    OPEN_EXISTING,
};

use crate::device;
use crate::path::Root;

/// Bytes per `GetFileInformationByHandleEx` call. 64 KiB is the documented
/// maximum for SMB and large enough for ~600 typical entries.
const BUF_BYTES: usize = 64 * 1024;

/// Error code recorded when a worker panicked on a directory.
const ERROR_INTERNAL: u32 = 0xE000_0001;

/// How often the coordinator checks for cancellation while waiting.
const POLL: Duration = Duration::from_millis(50);

/// How often the "current path" shown in the UI is refreshed.
const CURRENT_PATH_INTERVAL: Duration = Duration::from_millis(80);

/// Which information class the volume supports; shared by all workers.
const CLASS_UNKNOWN: u8 = 0;
const CLASS_EXTD: u8 = 1;
const CLASS_FULL: u8 = 2;

#[derive(Clone, Copy, Debug, Default)]
pub struct WalkScanner;

impl WalkScanner {
    pub const NAME: &'static str = "Verzeichnis-Scan";
}

impl Scanner for WalkScanner {
    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn scan(&self, root: &Path, options: &ScanOptions, progress: &ScanProgress) -> Result<ScanOutcome, ScanError> {
        let start = Instant::now();
        let root = Root::new(root)
            .map_err(|e| ScanError::InvalidRoot { path: root.display().to_string(), code: os_code(&e) })?;

        // Fail fast with a clear error if the root itself is not listable.
        let mut nul = Vec::new();
        if let Err(code) = open_dir(&root.verbatim, &mut nul) {
            return Err(ScanError::InvalidRoot { path: root.display.clone(), code });
        }

        let threads = if options.threads > 0 { options.threads.min(64) } else { auto_threads(&root) };
        let mut builder = TreeBuilder::new(root.display.clone());

        let (task_tx, task_rx) = unbounded::<Task>();
        let (res_tx, res_rx) = bounded::<Listing>(threads * 4);
        let class = AtomicU8::new(CLASS_UNKNOWN);

        let _ = task_tx.send(Task { node: 0, path: root.verbatim.into_boxed_slice() });

        let result = thread::scope(|s| {
            for i in 0..threads {
                let (rx, tx, class) = (task_rx.clone(), res_tx.clone(), &class);
                let spawned = thread::Builder::new()
                    .name(format!("scan-worker-{i}"))
                    .stack_size(256 * 1024)
                    .spawn_scoped(s, move || worker(&rx, &tx, progress, class));
                if spawned.is_err() && i == 0 {
                    return Err(ScanError::Internal("Scan-Threads konnten nicht gestartet werden".into()));
                }
            }
            // Only workers may hold these ends, so the channels disconnect
            // exactly when the coordinator or all workers are gone.
            drop(task_rx);
            drop(res_tx);
            coordinate(&mut builder, task_tx, res_rx, progress)
        })?;

        Ok(ScanOutcome {
            tree: builder.finish(),
            elapsed: start.elapsed(),
            cancelled: result == Finished::Cancelled,
            method: Self::NAME,
            threads,
        })
    }
}

/// Chooses the worker count from the storage type: rotational disks thrash
/// with many concurrent seeks, SSDs/NVMe and network shares benefit from
/// parallel requests.
fn auto_threads(root: &Root) -> usize {
    let cpus = thread::available_parallelism().map_or(4, |n| n.get());
    match root.drive {
        None => 16, // network share: latency-bound
        Some(letter) => match device::seek_penalty(letter) {
            Some(true) => 2,
            _ => cpus.clamp(4, 12),
        },
    }
}

struct Task {
    node: NodeId,
    /// Verbatim path without NUL.
    path: Box<[u16]>,
}

#[derive(Default)]
struct Listing {
    node: NodeId,
    path: Box<[u16]>,
    entries: Vec<RawEntry>,
    names: Vec<u8>,
    /// Paths of descendable subdirectories, in entry order.
    subdirs: Vec<Box<[u16]>>,
    /// First OS error while listing (0 = none). Entries read before the
    /// error are kept.
    error: u32,
    files: u64,
    dirs: u64,
    bytes: u64,
}

#[derive(PartialEq, Eq)]
enum Finished {
    Complete,
    Cancelled,
}

fn coordinate(
    builder: &mut TreeBuilder,
    task_tx: Sender<Task>,
    res_rx: Receiver<Listing>,
    progress: &ScanProgress,
) -> Result<Finished, ScanError> {
    let mut pending: usize = 1;
    let mut last_current = Instant::now() - CURRENT_PATH_INTERVAL;

    while pending > 0 {
        if progress.is_cancelled() {
            return Ok(Finished::Cancelled);
        }
        let listing = match res_rx.recv_timeout(POLL) {
            Ok(l) => l,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => {
                return Err(ScanError::Internal("alle Scan-Threads wurden beendet".into()));
            }
        };
        pending -= 1;

        if listing.error != 0 {
            builder.mark_error(listing.node, listing.error);
            progress.add_error();
        }

        let mut subdirs = listing.subdirs.into_iter();
        builder
            .add_listing(listing.node, &listing.entries, &listing.names, |id| {
                if let Some(path) = subdirs.next() {
                    pending += 1;
                    // Cannot fail: workers keep the receiver alive while we run.
                    let _ = task_tx.send(Task { node: id, path });
                }
            })
            .map_err(|_| ScanError::Capacity)?;
        progress.add(listing.files, listing.dirs, listing.bytes);

        if last_current.elapsed() >= CURRENT_PATH_INTERVAL {
            last_current = Instant::now();
            progress.set_current(&String::from_utf16_lossy(strip_verbatim(&listing.path)));
        }
    }
    Ok(Finished::Complete)
}

fn worker(rx: &Receiver<Task>, tx: &Sender<Listing>, progress: &ScanProgress, class: &AtomicU8) {
    // u64 backing guarantees the 8-byte alignment the entry structs need.
    let mut buf = vec![0u64; BUF_BYTES / 8];
    let mut nul = Vec::with_capacity(512);
    while let Ok(task) = rx.recv() {
        if progress.is_cancelled() {
            return;
        }
        let node = task.node;
        let listing = catch_unwind(AssertUnwindSafe(|| list_directory(task, &mut buf, &mut nul, class)))
            .unwrap_or_else(|_| Listing { node, error: ERROR_INTERNAL, ..Listing::default() });
        if tx.send(listing).is_err() {
            return;
        }
    }
}

/// Opens a directory for listing. Returns the OS error code on failure.
fn open_dir(path: &[u16], nul: &mut Vec<u16>) -> Result<OwnedHandle, u32> {
    nul.clear();
    nul.extend_from_slice(path);
    nul.push(0);
    // SAFETY: `nul` is a valid NUL-terminated UTF-16 string that outlives the
    // call; all other pointer arguments are null, which the API permits.
    let h = unsafe {
        CreateFileW(
            nul.as_ptr(),
            FILE_LIST_DIRECTORY,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            ptr::null(),
            OPEN_EXISTING,
            // BACKUP_SEMANTICS is required to open directories and honours
            // SeBackupPrivilege when elevated. OPEN_REPARSE_POINT guarantees
            // we never traverse a link, even if one appears after listing.
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        // SAFETY: trivially safe; reads the calling thread's last error.
        return Err(unsafe { GetLastError() });
    }
    // SAFETY: `h` is a valid handle we exclusively own from here on.
    Ok(unsafe { OwnedHandle::from_raw_handle(h) })
}

/// Reads the next batch of entries into `buf`. `Ok(false)` = no more entries.
fn read_batch(dir: &OwnedHandle, class: FILE_INFO_BY_HANDLE_CLASS, buf: &mut [u64]) -> Result<bool, u32> {
    use std::os::windows::io::AsRawHandle;
    // SAFETY: the handle is valid for the duration of the call and `buf` is a
    // writable buffer of exactly the size passed.
    let ok = unsafe {
        GetFileInformationByHandleEx(dir.as_raw_handle(), class, buf.as_mut_ptr().cast(), mem::size_of_val(buf) as u32)
    };
    if ok != 0 {
        return Ok(true);
    }
    // SAFETY: trivially safe; reads the calling thread's last error.
    match unsafe { GetLastError() } {
        ERROR_NO_MORE_FILES => Ok(false),
        code => Err(code),
    }
}

fn list_directory(task: Task, buf: &mut [u64], nul: &mut Vec<u16>, class_state: &AtomicU8) -> Listing {
    let mut out = Listing { node: task.node, ..Listing::default() };
    let dir = match open_dir(&task.path, nul) {
        Ok(h) => h,
        Err(code) => {
            out.error = code;
            out.path = task.path;
            return out;
        }
    };

    let mut class = match class_state.load(Ordering::Relaxed) {
        CLASS_FULL => CLASS_FULL,
        _ => CLASS_EXTD,
    };
    let mut first = true;
    loop {
        let info_class = if class == CLASS_EXTD { FileIdExtdDirectoryInfo } else { FileFullDirectoryInfo };
        match read_batch(&dir, info_class, buf) {
            Ok(true) => {}
            Ok(false) => break,
            // FAT/exFAT and some network redirectors do not support the
            // extended class; fall back once and remember it for the volume.
            Err(ERROR_INVALID_PARAMETER | ERROR_INVALID_FUNCTION | ERROR_NOT_SUPPORTED)
                if first && class == CLASS_EXTD =>
            {
                class = CLASS_FULL;
                class_state.store(CLASS_FULL, Ordering::Relaxed);
                continue;
            }
            Err(code) => {
                out.error = code;
                break;
            }
        }
        if first {
            first = false;
            let _ = class_state.compare_exchange(CLASS_UNKNOWN, class, Ordering::Relaxed, Ordering::Relaxed);
        }
        parse_batch(buf, class == CLASS_EXTD, &task.path, &mut out);
    }
    out.path = task.path;
    out
}

/// Fields shared by both directory information classes.
struct EntryHeader {
    next: u32,
    attrs: u32,
    name_bytes: u32,
    end_of_file: i64,
    allocation: i64,
    last_write: i64,
    reparse_tag: u32,
}

fn parse_batch(buf: &[u64], extd: bool, dir_path: &[u16], out: &mut Listing) {
    let base = buf.as_ptr().cast::<u8>();
    let cap = mem::size_of_val(buf);
    let (header_size, name_offset) = if extd {
        (mem::size_of::<FILE_ID_EXTD_DIR_INFO>(), mem::offset_of!(FILE_ID_EXTD_DIR_INFO, FileName))
    } else {
        (mem::size_of::<FILE_FULL_DIR_INFO>(), mem::offset_of!(FILE_FULL_DIR_INFO, FileName))
    };

    let mut off = 0usize;
    loop {
        if off + header_size > cap {
            break;
        }
        // SAFETY: `off + header_size <= cap`, so the whole struct lies inside
        // `buf`; `read_unaligned` imposes no alignment requirement.
        let h = unsafe { read_header(base.add(off), extd) };
        let name_len = (h.name_bytes / 2) as usize;
        let name_start = off + name_offset;
        if name_start + name_len * 2 > cap || !name_start.is_multiple_of(2) {
            break; // malformed buffer from a buggy file system driver
        }
        // SAFETY: bounds and u16 alignment were checked above; the buffer is
        // initialised memory we own for the lifetime of the slice.
        let name = unsafe { std::slice::from_raw_parts(base.add(name_start).cast::<u16>(), name_len) };
        push_entry(&h, name, dir_path, out);
        if h.next == 0 {
            break;
        }
        off += h.next as usize;
    }
}

/// # Safety
/// `p` must point to at least `size_of` of the respective struct readable bytes.
unsafe fn read_header(p: *const u8, extd: bool) -> EntryHeader {
    if extd {
        // SAFETY: guaranteed by the caller.
        let e = unsafe { ptr::read_unaligned(p.cast::<FILE_ID_EXTD_DIR_INFO>()) };
        EntryHeader {
            next: e.NextEntryOffset,
            attrs: e.FileAttributes,
            name_bytes: e.FileNameLength,
            end_of_file: e.EndOfFile,
            allocation: e.AllocationSize,
            last_write: e.LastWriteTime,
            reparse_tag: e.ReparsePointTag,
        }
    } else {
        // SAFETY: guaranteed by the caller.
        let e = unsafe { ptr::read_unaligned(p.cast::<FILE_FULL_DIR_INFO>()) };
        EntryHeader {
            next: e.NextEntryOffset,
            attrs: e.FileAttributes,
            name_bytes: e.FileNameLength,
            end_of_file: e.EndOfFile,
            allocation: e.AllocationSize,
            last_write: e.LastWriteTime,
            // For reparse points this class stores the tag in EaSize.
            reparse_tag: e.EaSize,
        }
    }
}

fn push_entry(h: &EntryHeader, name: &[u16], dir_path: &[u16], out: &mut Listing) {
    const DOT: u16 = b'.' as u16;
    if name.is_empty() || name == [DOT] || name == [DOT, DOT] {
        return;
    }

    let is_dir = h.attrs & FILE_ATTRIBUTE_DIRECTORY != 0;
    let f = node_flags(h.attrs, is_dir, h.reparse_tag);

    let (size, alloc) = if is_dir {
        (0, 0)
    } else {
        (u64::try_from(h.end_of_file).unwrap_or(0), u64::try_from(h.allocation).unwrap_or(0))
    };

    let name_off = out.names.len() as u32;
    push_wtf8(&mut out.names, name);
    let name_len = u16::try_from(out.names.len() - name_off as usize).unwrap_or(u16::MAX);

    out.entries.push(RawEntry { size, alloc, mtime: filetime_to_unix(h.last_write), name_off, name_len, flags: f });

    if is_dir {
        out.dirs += 1;
        if f & flags::NOT_FOLLOWED == 0 {
            let sep = u16::from(b'\\');
            let needs_sep = dir_path.last() != Some(&sep);
            let mut child = Vec::with_capacity(dir_path.len() + name.len() + 1);
            child.extend_from_slice(dir_path);
            if needs_sep {
                child.push(sep);
            }
            child.extend_from_slice(name);
            out.subdirs.push(child.into_boxed_slice());
        }
    } else {
        out.files += 1;
        out.bytes += size;
    }
}

/// Maps Win32 file attributes to node flags. Shared with the MFT scanner,
/// whose `$STANDARD_INFORMATION` lacks the directory bit (hence `is_dir`).
pub(crate) fn node_flags(attrs: u32, is_dir: bool, reparse_tag: u32) -> u16 {
    let mut f = 0u16;
    if is_dir {
        f |= flags::DIR;
    }
    if attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        f |= flags::REPARSE;
        if is_dir && is_name_surrogate(reparse_tag) {
            f |= flags::NOT_FOLLOWED;
        }
    }
    for (attr, flag) in [
        (FILE_ATTRIBUTE_HIDDEN, flags::HIDDEN),
        (FILE_ATTRIBUTE_SYSTEM, flags::SYSTEM),
        (FILE_ATTRIBUTE_COMPRESSED, flags::COMPRESSED),
        (FILE_ATTRIBUTE_SPARSE_FILE, flags::SPARSE),
        (FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS | FILE_ATTRIBUTE_RECALL_ON_OPEN | FILE_ATTRIBUTE_OFFLINE, flags::CLOUD),
    ] {
        if attrs & attr != 0 {
            f |= flag;
        }
    }
    f
}

/// `IsReparseTagNameSurrogate`: the tag makes the entry an alias for another
/// name (symlink, junction, mount point). Cloud-file tags are *not* name
/// surrogates, so OneDrive folders are scanned normally.
#[inline]
fn is_name_surrogate(tag: u32) -> bool {
    tag & 0x2000_0000 != 0
}

/// FILETIME (100 ns since 1601) → Unix seconds, clamped to `u32`.
#[inline]
pub(crate) fn filetime_to_unix(ft: i64) -> u32 {
    const EPOCH_DIFF_SECS: i64 = 11_644_473_600;
    let secs = ft / 10_000_000 - EPOCH_DIFF_SECS;
    secs.clamp(0, i64::from(u32::MAX)) as u32
}

fn strip_verbatim(path: &[u16]) -> &[u16] {
    const PREFIX: [u16; 4] = [b'\\' as u16, b'\\' as u16, b'?' as u16, b'\\' as u16];
    path.strip_prefix(&PREFIX[..]).unwrap_or(path)
}

fn os_code(e: &std::io::Error) -> u32 {
    e.raw_os_error().map_or(0, |c| c as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn filetime_conversion() {
        assert_eq!(filetime_to_unix(0), 0);
        assert_eq!(filetime_to_unix(116_444_736_000_000_000), 0);
        assert_eq!(filetime_to_unix(116_444_736_000_000_000 + 10_000_000), 1);
    }

    #[test]
    fn name_surrogate_tags() {
        assert!(is_name_surrogate(0xA000_0003)); // IO_REPARSE_TAG_MOUNT_POINT (junction)
        assert!(is_name_surrogate(0xA000_000C)); // IO_REPARSE_TAG_SYMLINK
        assert!(!is_name_surrogate(0x9000_001A)); // IO_REPARSE_TAG_CLOUD
    }

    #[test]
    fn scans_a_real_directory() {
        let root = std::env::temp_dir().join(format!("spacelens-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::create_dir_all(root.join("empty")).unwrap();
        fs::write(root.join("top.bin"), vec![0u8; 1000]).unwrap();
        fs::write(root.join("a/one.txt"), b"12345").unwrap();
        fs::write(root.join("a/b/Übergröße 😀.dat"), vec![1u8; 4096]).unwrap();

        let progress = ScanProgress::new();
        let out = WalkScanner.scan(&root, &ScanOptions { threads: 3 }, &progress).unwrap();
        let _ = fs::remove_dir_all(&root);

        let t = &out.tree;
        assert!(!out.cancelled);
        assert_eq!(t.node(0).size, 1000 + 5 + 4096);
        assert_eq!(t.counts(0), (3, 3));
        assert!(t.issues().is_empty());
        let names: Vec<String> = (0..t.len() as u32).map(|i| t.path(i)).collect();
        assert!(names.iter().any(|p| p.ends_with(r"a\b\Übergröße 😀.dat")), "{names:?}");
        let snap = progress.snapshot();
        assert_eq!((snap.files, snap.dirs, snap.bytes), (3, 3, 5101));
    }

    #[test]
    fn reports_invalid_root() {
        let err = WalkScanner
            .scan(Path::new(r"Z:\definitely\missing\dir"), &ScanOptions::default(), &ScanProgress::new())
            .unwrap_err();
        assert!(matches!(err, ScanError::InvalidRoot { .. }));
    }

    #[test]
    fn cancellation_stops_scan() {
        let progress = ScanProgress::new();
        progress.cancel();
        let out = WalkScanner.scan(&std::env::temp_dir(), &ScanOptions { threads: 2 }, &progress).unwrap();
        assert!(out.cancelled);
    }
}
