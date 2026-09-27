//! NTFS Master File Table scanner (administrator only).
//!
//! Instead of listing hundreds of thousands of directories, this reads the
//! volume's `$MFT` – one record per file – sequentially and in large blocks,
//! bypassing the file cache:
//!
//! ```text
//! reader ──4 MiB blocks──► N parsers ──records + names──► coordinator ──► tree
//!    ▲                         │ (fixups, attributes,         (record table,
//!    └──── recycled buffers ───┘  names → WTF-8)               BFS layout)
//! ```
//!
//! Compared to the directory walker this is faster on cold caches, sees every
//! file regardless of ACLs, includes NTFS metadata files (`$MFT`, `$LogFile`, …)
//! and counts hard-linked files exactly once.
//!
//! Limitations: whole volumes only; an `$MFT` whose own record needs an
//! attribute list (extremely fragmented MFT) is reported as unsupported so the
//! caller falls back to the walker.

pub mod record;
pub mod volume;

use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Instant;

use crossbeam_channel::{bounded, unbounded};
use spacelens_core::{
    NodeId, RawEntry, ScanError, ScanOptions, ScanOutcome, ScanProgress, Scanner, Tree, TreeBuilder, flags,
};

use crate::path::Root;
use crate::walker::{filetime_to_unix, node_flags};
use record::{ROOT_RECORD, Summary};
use volume::{AlignedBuf, Volume};

/// Bytes per read request. Large sequential reads saturate NVMe and HDDs
/// alike; must be a multiple of every cluster size (max. 2 MiB).
const CHUNK_BYTES: u64 = 4 << 20;

/// Concurrent read requests on non-rotational storage. Measured on a PCIe 3
/// NVMe: 1, 2, 4 and 8 readers all hit the same ~2 GB/s (a 4 MiB request is
/// already split into parallel device I/O). Two keep a request queued while
/// the other completes, which may help faster drives; HDDs use one stream.
const SSD_READERS: usize = 2;

const KIND_NONE: u8 = 0;
const KIND_FILE: u8 = 1;
const KIND_DIR: u8 = 2;

#[derive(Clone, Copy, Debug, Default)]
pub struct MftScanner;

impl MftScanner {
    pub const NAME: &'static str = "MFT-Scan";
}

/// Whether the MFT scanner can be used for `root`: a whole NTFS volume and
/// an elevated process.
pub fn applicable(root: &Path) -> bool {
    let Ok(root) = Root::new(root) else { return false };
    root.drive.is_some()
        && root.display.len() == 3
        && crate::elevation::is_elevated()
        && crate::volumes::file_system(&root.display).is_some_and(|fs| fs.eq_ignore_ascii_case("NTFS"))
}

impl Scanner for MftScanner {
    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn scan(&self, root: &Path, options: &ScanOptions, progress: &ScanProgress) -> Result<ScanOutcome, ScanError> {
        let start = Instant::now();
        let unsupported = |why: String| ScanError::Unsupported(why);
        let root = Root::new(root).map_err(|e| unsupported(e.to_string()))?;
        let letter = root
            .drive
            .filter(|_| root.display.len() == 3)
            .ok_or_else(|| unsupported("nur für ganze Laufwerke".into()))?;

        let vol = Volume::open(letter).map_err(|c| unsupported(format!("Volume nicht lesbar (Fehler {c})")))?;
        let info = vol.ntfs_info().map_err(|c| unsupported(format!("kein NTFS (Fehler {c})")))?;
        let t_open = start.elapsed();
        let rs = info.bytes_per_record;
        if !(256..=65536).contains(&rs) || info.bytes_per_cluster == 0 {
            return Err(unsupported("unerwartete NTFS-Geometrie".into()));
        }
        let total_records = info.mft_valid_bytes / rs;
        if total_records >= u64::from(u32::MAX) {
            return Err(ScanError::Capacity);
        }

        let trace = std::env::var_os("SPACELENS_TRACE").is_some();
        // Rotational disks: one sequential stream. SSD/NVMe: several requests in flight.
        let readers = if crate::device::seek_penalty(letter) == Some(true) { 1 } else { SSD_READERS };
        let plan = read_plan(&vol, &info, total_records)?;
        let t_plan = start.elapsed();
        let parsers = if options.threads > 0 {
            options.threads.min(32)
        } else {
            thread::available_parallelism().map_or(4, |n| n.get()).clamp(2, 8)
        };

        let mut table = Table::new(total_records as usize);
        let job = Pipeline { vol: &vol, plan: &plan, rs: rs as usize, total_records, parsers, readers };
        read_and_parse(job, progress, &mut table)?;
        drop(vol);
        let t_read = start.elapsed();

        if progress.is_cancelled() {
            // Nothing meaningful can be shown from a partially read MFT.
            return Ok(ScanOutcome {
                tree: TreeBuilder::new(root.display).finish(),
                elapsed: start.elapsed(),
                cancelled: true,
                method: Self::NAME,
                threads: parsers,
            });
        }

        progress.set_current("Baum wird aufgebaut …");
        let tree = table.build_tree(root.display, progress)?;
        if trace {
            let read_mib: usize = plan.iter().map(|p| p.bytes).sum::<usize>() >> 20;
            eprintln!(
                "[mft] readers {readers} | open {t_open:?} | plan {:?} | read+parse {:?} ({read_mib} MiB of {} MiB, {} requests) | build {:?}",
                t_plan - t_open,
                t_read - t_plan,
                info.mft_valid_bytes >> 20,
                plan.len(),
                start.elapsed() - t_read
            );
        }
        Ok(ScanOutcome { tree, elapsed: start.elapsed(), cancelled: false, method: Self::NAME, threads: parsers })
    }
}

/// One read request: `bytes` at disk `offset`, containing `records` records
/// starting with record number `first_record`.
#[derive(Clone, Copy, Debug)]
struct Piece {
    offset: u64,
    bytes: usize,
    first_record: u64,
    records: usize,
}

/// Granularity at which unused MFT regions are skipped. Small enough to skip
/// holes, large enough to keep reads sequential; a multiple of every sector
/// and record size.
const BLOCK_BYTES: u64 = 64 << 10;

/// Reads the `$MFT` record itself and turns its data runs into read requests,
/// skipping regions whose records are all free according to `$MFT:$BITMAP`.
fn read_plan(vol: &Volume, info: &volume::NtfsInfo, total_records: u64) -> Result<Vec<Piece>, ScanError> {
    let unsupported = |why: &str| ScanError::Unsupported(why.to_string());
    let (cluster, rs) = (info.bytes_per_cluster, info.bytes_per_record);

    let trace = std::env::var_os("SPACELENS_TRACE").is_some();
    let t = std::time::Instant::now();
    let mut buf = AlignedBuf::new(rs as usize).ok_or_else(|| unsupported("Speicher"))?;
    vol.read_at(info.mft_start_lcn * cluster, &mut buf)
        .map_err(|c| ScanError::Unsupported(format!("MFT nicht lesbar (Fehler {c})")))?;
    let rec = &mut buf[..rs as usize];
    if !record::apply_fixups(rec) {
        return Err(unsupported("MFT-Eintrag 0 beschädigt"));
    }
    if record::has_attribute_list(rec) {
        return Err(unsupported("stark fragmentierte MFT"));
    }
    let runs = record::unnamed_attr(rec, record::ATTR_DATA)
        .and_then(|a| a.runlist())
        .and_then(record::decode_runlist)
        .ok_or_else(|| unsupported("MFT-Datenbereiche nicht lesbar"))?;

    // Optional: without the bitmap every record is read, which is still correct.
    let t_record0 = t.elapsed();
    let bitmap = read_bitmap(vol, rec, cluster, total_records);
    if trace {
        eprintln!(
            "[mft] record0 {t_record0:?} | bitmap {:?} ({} bytes)",
            t.elapsed() - t_record0,
            bitmap.as_ref().map_or(0, Vec::len)
        );
    }
    Ok(plan_pieces(&runs, cluster, rs, total_records, bitmap.as_deref()))
}

/// Loads `$MFT:$BITMAP` (one bit per record, 1 = in use).
fn read_bitmap(vol: &Volume, mft_record: &[u8], cluster: u64, total_records: u64) -> Option<Vec<u8>> {
    let attr = record::unnamed_attr(mft_record, record::ATTR_BITMAP)?;
    let needed = total_records.div_ceil(8) as usize;
    if let Some(v) = attr.value() {
        return Some(v.get(..needed)?.to_vec());
    }
    let runs = record::decode_runlist(attr.runlist()?)?;
    let total: u64 = runs.iter().map(|r| r.clusters * cluster).sum();
    if total < needed as u64 || total > 1 << 30 {
        return None;
    }
    let mut buf = AlignedBuf::new(total as usize)?;
    let mut pos = 0usize;
    for run in runs {
        let len = (run.clusters * cluster) as usize;
        if let Some(lcn) = run.lcn {
            vol.read_at(lcn * cluster, buf.get_mut(pos..pos + len)?).ok()?;
        }
        pos += len;
    }
    Some(buf[..needed].to_vec())
}

/// Splits the MFT data runs into sequential read requests of at most
/// [`CHUNK_BYTES`], leaving out [`BLOCK_BYTES`] blocks without any record in use.
fn plan_pieces(runs: &[record::Run], cluster: u64, rs: u64, total: u64, bitmap: Option<&[u8]>) -> Vec<Piece> {
    let in_use = |first: u64, count: u64| {
        bitmap.is_none_or(|bm| {
            (first..first + count).any(|r| bm.get((r / 8) as usize).is_none_or(|b| (b >> (r % 8)) & 1 == 1))
        })
    };
    let mut plan = Vec::new();
    let mut vcn_bytes = 0u64;
    for run in runs {
        let run_bytes = run.clusters * cluster;
        if let Some(lcn) = run.lcn {
            let mut current: Option<Piece> = None;
            let mut done = 0u64;
            while done < run_bytes {
                let first = (vcn_bytes + done) / rs;
                if first >= total {
                    break;
                }
                let bytes = BLOCK_BYTES.min(run_bytes - done);
                let count = (bytes / rs).min(total - first);
                if in_use(first, count) {
                    match current.as_mut() {
                        Some(p) if p.bytes as u64 + bytes <= CHUNK_BYTES => {
                            p.bytes += bytes as usize;
                            p.records += count as usize;
                        }
                        _ => {
                            plan.extend(current.take());
                            current = Some(Piece {
                                offset: lcn * cluster + done,
                                bytes: bytes as usize,
                                first_record: first,
                                records: count as usize,
                            });
                        }
                    }
                } else {
                    plan.extend(current.take());
                }
                done += bytes;
            }
            plan.extend(current);
        }
        vcn_bytes += run_bytes;
    }
    plan
}

struct Chunk {
    piece: Piece,
    buf: AlignedBuf,
}

/// Parser output for one chunk.
#[derive(Default)]
struct Parsed {
    records: Vec<(u32, Summary)>,
    names: Vec<NameRef>,
    bytes: Vec<u8>,
}

/// One `$FILE_NAME` = one directory entry (hard links have several).
#[derive(Clone, Copy, Debug)]
struct NameRef {
    /// Base record of the file.
    rec: u32,
    parent: u32,
    /// Offset into [`Table::names`].
    off: u32,
    len: u16,
}

/// Per-record data needed to build the tree. 24 bytes.
#[derive(Clone, Copy, Debug, Default)]
struct Rec {
    size: u64,
    alloc: u64,
    mtime: u32,
    flags: u16,
    kind: u8,
    has_data: bool,
}

struct Table {
    recs: Vec<Rec>,
    entries: Vec<NameRef>,
    names: Vec<u8>,
}

/// Parameters of the read → parse pipeline.
#[derive(Clone, Copy)]
struct Pipeline<'a> {
    vol: &'a Volume,
    plan: &'a [Piece],
    rs: usize,
    total_records: u64,
    parsers: usize,
    readers: usize,
}

fn read_and_parse(job: Pipeline<'_>, progress: &ScanProgress, table: &mut Table) -> Result<(), ScanError> {
    let Pipeline { vol, plan, rs, total_records, parsers, readers } = job;
    let pool = parsers + 2 * readers;
    let (free_tx, free_rx) = bounded::<AlignedBuf>(pool);
    for _ in 0..pool {
        let buf = AlignedBuf::new(CHUNK_BYTES as usize).ok_or(ScanError::Capacity)?;
        let _ = free_tx.send(buf);
    }
    let (work_tx, work_rx) = bounded::<Chunk>(pool);
    let (done_tx, done_rx) = unbounded::<Parsed>();
    let next_piece = AtomicUsize::new(0);

    thread::scope(|s| {
        // Several readers keep more than one request in flight (NVMe scales with
        // queue depth); they pull the next piece from a shared counter.
        let mut reader_handles = Vec::with_capacity(readers);
        for r in 0..readers {
            let (free_rx, work_tx, next_piece) = (free_rx.clone(), work_tx.clone(), &next_piece);
            let handle = thread::Builder::new()
                .name(format!("mft-reader-{r}"))
                .spawn_scoped(s, move || -> Result<(), u32> {
                    let total = plan.len().max(1);
                    loop {
                        let i = next_piece.fetch_add(1, Ordering::Relaxed);
                        let Some(piece) = plan.get(i) else { return Ok(()) };
                        if progress.is_cancelled() {
                            return Ok(());
                        }
                        let Ok(mut buf) = free_rx.recv() else { return Ok(()) };
                        let read = vol.read_at(piece.offset, &mut buf[..piece.bytes])?;
                        let piece = Piece { records: (read / rs).min(piece.records), ..*piece };
                        if work_tx.send(Chunk { piece, buf }).is_err() {
                            return Ok(());
                        }
                        if i % 16 == 0 {
                            progress.set_current(&format!("Master File Table wird gelesen … {} %", i * 100 / total));
                        }
                    }
                })
                .map_err(|e| ScanError::Internal(e.to_string()))?;
            reader_handles.push(handle);
        }
        drop((free_rx, work_tx));

        for i in 0..parsers {
            let (rx, free, done) = (work_rx.clone(), free_tx.clone(), done_tx.clone());
            thread::Builder::new()
                .name(format!("mft-parser-{i}"))
                .spawn_scoped(s, move || {
                    while let Ok(mut chunk) = rx.recv() {
                        let parsed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            parse_chunk(&mut chunk, rs, total_records)
                        }))
                        .unwrap_or_default();
                        let _ = free.send(chunk.buf);
                        if done.send(parsed).is_err() {
                            return;
                        }
                    }
                })
                .map_err(|e| ScanError::Internal(e.to_string()))?;
        }
        drop((work_rx, done_tx, free_tx));

        // The coordinator merges while the reader keeps the disk busy.
        for parsed in done_rx.iter() {
            table.merge(parsed)?;
        }
        for handle in reader_handles {
            match handle.join() {
                Ok(Ok(())) => {}
                Ok(Err(code)) => {
                    return Err(ScanError::Unsupported(format!("Lesefehler in der MFT (Fehler {code})")));
                }
                Err(_) => return Err(ScanError::Internal("MFT-Lesethread abgestürzt".into())),
            }
        }
        Ok(())
    })
}

fn parse_chunk(chunk: &mut Chunk, rs: usize, total_records: u64) -> Parsed {
    let mut out = Parsed {
        records: Vec::with_capacity(chunk.piece.records),
        names: Vec::with_capacity(chunk.piece.records),
        bytes: Vec::with_capacity(chunk.piece.records * 16),
    };
    for i in 0..chunk.piece.records {
        let number = chunk.piece.first_record + i as u64;
        if number >= total_records {
            break;
        }
        let Some(rec) = chunk.buf.get_mut(i * rs..(i + 1) * rs) else { break };
        if !record::apply_fixups(rec) {
            continue;
        }
        let number = number as u32;
        let (names, bytes) = (&mut out.names, &mut out.bytes);
        let mut pending = Vec::new();
        let Some(summary) = record::summarize(rec, |parent, name| {
            let off = bytes.len();
            record::push_utf16le_name(bytes, name);
            let len = u16::try_from(bytes.len() - off).unwrap_or(u16::MAX);
            pending.push((parent, off as u32, len));
        }) else {
            continue;
        };
        let owner = summary.base.map_or(Some(number), |b| u32::try_from(b).ok());
        if let Some(owner) = owner {
            for (parent, off, len) in pending {
                if let Ok(parent) = u32::try_from(parent) {
                    names.push(NameRef { rec: owner, parent, off, len });
                }
            }
            out.records.push((number, summary));
        }
    }
    out
}

impl Table {
    fn new(records: usize) -> Self {
        Self {
            recs: vec![Rec::default(); records],
            entries: Vec::with_capacity(records),
            names: Vec::with_capacity(records * 16),
        }
    }

    fn merge(&mut self, parsed: Parsed) -> Result<(), ScanError> {
        let base = u32::try_from(self.names.len()).map_err(|_| ScanError::Capacity)?;
        self.names.extend_from_slice(&parsed.bytes);
        self.entries.extend(parsed.names.into_iter().map(|n| NameRef { off: n.off + base, ..n }));

        for (number, s) in parsed.records {
            let target = s.base.map_or(number as usize, |b| b as usize);
            let Some(r) = self.recs.get_mut(target) else { continue };
            if let Some((size, alloc)) = s.data
                && !s.is_dir
            {
                r.size = size;
                r.alloc = alloc;
                r.has_data = true;
            }
            if s.base.is_none() {
                r.kind = if s.is_dir { KIND_DIR } else { KIND_FILE };
                r.mtime = s.mtime.map_or(0, filetime_to_unix);
                r.flags = node_flags(s.attrs, s.is_dir, s.reparse_tag);
                if s.is_dir {
                    (r.size, r.alloc) = (0, 0);
                }
            }
        }
        Ok(())
    }

    /// Lays the records out breadth-first from the root directory so the
    /// result satisfies the tree invariants (children contiguous, parent < child).
    fn build_tree(self, root_display: String, progress: &ScanProgress) -> Result<Tree, ScanError> {
        let Table { recs, entries, names } = self;
        let n = recs.len();
        let valid = |e: &NameRef| {
            e.rec != ROOT_RECORD
                && recs.get(e.parent as usize).is_some_and(|p| p.kind == KIND_DIR)
                && recs.get(e.rec as usize).is_some_and(|r| r.kind != KIND_NONE)
        };

        // Children per directory in CSR form: start[d]..start[d + 1] indexes `order`.
        let mut start = vec![0u32; n + 1];
        for e in entries.iter().filter(|e| valid(e)) {
            start[e.parent as usize + 1] += 1;
        }
        for i in 0..n {
            start[i + 1] += start[i];
        }
        let mut cursor = start.clone();
        let mut order = vec![0u32; start[n] as usize];
        for (i, e) in entries.iter().enumerate().filter(|(_, e)| valid(e)) {
            let slot = &mut cursor[e.parent as usize];
            order[*slot as usize] = i as u32;
            *slot += 1;
        }
        drop(cursor);
        // Arrival order of parsed chunks is nondeterministic; sort each directory
        // by record number so layout and hard-link attribution are reproducible.
        let name_of = |i: u32| {
            let e = &entries[i as usize];
            names.get(e.off as usize..e.off as usize + e.len as usize).unwrap_or_default()
        };
        for d in 0..n {
            order[start[d] as usize..start[d + 1] as usize].sort_unstable_by(|&a, &b| {
                entries[a as usize].rec.cmp(&entries[b as usize].rec).then_with(|| name_of(a).cmp(name_of(b)))
            });
        }

        let mut builder = TreeBuilder::with_capacity(root_display, order.len() + 1);
        let mut counted = vec![false; n]; // hard links: size only at the first link
        let mut visited = vec![false; n]; // defensive: never descend a directory twice
        visited[ROOT_RECORD as usize] = true;
        let mut queue: VecDeque<(u32, NodeId)> = VecDeque::from([(ROOT_RECORD, Tree::ROOT)]);
        let mut raw = Vec::new();
        let mut subdirs = Vec::new();

        while let Some((dir, node)) = queue.pop_front() {
            if progress.is_cancelled() {
                break;
            }
            raw.clear();
            subdirs.clear();
            let (mut files, mut dirs, mut bytes) = (0u64, 0u64, 0u64);
            let range = start[dir as usize] as usize..start[dir as usize + 1] as usize;
            for &idx in &order[range] {
                let e = entries[idx as usize];
                let r = recs[e.rec as usize];
                let mut f = r.flags;
                let (mut size, mut alloc) = (0, 0);
                if r.kind == KIND_DIR {
                    dirs += 1;
                    if f & flags::NOT_FOLLOWED == 0 {
                        if visited[e.rec as usize] {
                            f |= flags::NOT_FOLLOWED;
                        } else {
                            visited[e.rec as usize] = true;
                            subdirs.push(e.rec);
                        }
                    }
                } else {
                    files += 1;
                    if counted[e.rec as usize] {
                        f |= flags::HARDLINK;
                    } else {
                        counted[e.rec as usize] = true;
                        (size, alloc) = (r.size, r.alloc);
                        bytes += size;
                    }
                }
                raw.push(RawEntry { size, alloc, mtime: r.mtime, name_off: e.off, name_len: e.len, flags: f });
            }
            let mut next = subdirs.iter();
            builder
                .add_listing(node, &raw, &names, |id| {
                    if let Some(&rec) = next.next() {
                        queue.push_back((rec, id));
                    }
                })
                .map_err(|_| ScanError::Capacity)?;
            progress.add(files, dirs, bytes);
        }
        Ok(builder.finish())
    }
}

#[cfg(test)]
mod tests {
    use super::record::tests::RecordBuilder;
    use super::*;

    /// Runs records (index = record number) through parse + merge + build.
    fn tree_from(records: &[Option<Vec<u8>>]) -> Tree {
        let rs = 1024;
        let mut buf = AlignedBuf::new(records.len() * rs).unwrap();
        for (i, r) in records.iter().enumerate() {
            if let Some(r) = r {
                buf[i * rs..(i + 1) * rs].copy_from_slice(r);
            }
        }
        let piece = Piece { offset: 0, bytes: buf.len(), first_record: 0, records: records.len() };
        let mut chunk = Chunk { piece, buf };
        let parsed = parse_chunk(&mut chunk, rs, records.len() as u64);
        let mut table = Table::new(records.len());
        table.merge(parsed).unwrap();
        table.build_tree("T:\\".into(), &ScanProgress::new()).unwrap()
    }

    #[test]
    fn builds_tree_with_hardlinks_extensions_and_junctions() {
        let dir = |parent: u64, name: &str| Some(RecordBuilder::new(true).file_name(parent, name, 1).build());
        let mut recs: Vec<Option<Vec<u8>>> = vec![None; 12];
        recs[5] = Some(RecordBuilder::new(true).file_name(5, ".", 3).build()); // root names itself
        recs[6] = dir(5, "Users");
        recs[7] = dir(6, "Joe");
        // File with two hard links (Users\a.bin and Users\Joe\b.bin), 1000 bytes.
        recs[8] = Some(
            RecordBuilder::new(false)
                .file_name(6, "a.bin", 1)
                .file_name(7, "b.bin", 1)
                .file_name(7, "B~1.BIN", 2)
                .resident_data(300)
                .build(),
        );
        // Base record without $DATA; the data lives in extension record 10.
        recs[9] = Some(RecordBuilder::new(false).file_name(7, "big.vhdx", 1).build());
        recs[10] = Some(RecordBuilder::new(false).base(9).nonresident_data(5 << 30, 5 << 30, &[0], None).build());
        // Junction: directory with a name-surrogate reparse tag.
        recs[11] = Some(
            RecordBuilder::new(true).std_info(0, 0x400 | 0x10).file_name(5, "Link", 1).build_with_reparse(0xA000_0003),
        );

        let t = tree_from(&recs);
        let (files, dirs) = t.counts(0);
        assert_eq!((files, dirs), (3, 3), "a.bin, b.bin, big.vhdx / Users, Joe, Link");
        assert_eq!(t.node(0).size, 300 + (5 << 30), "hard link counted once");
        let paths: Vec<String> = (0..t.len() as u32).map(|i| t.path(i)).collect();
        assert!(paths.contains(&r"T:\Users\Joe\big.vhdx".to_string()), "{paths:?}");
        assert!(!paths.iter().any(|p| p.contains("B~1")), "DOS names skipped");
        let link = (0..t.len() as u32).find(|&i| t.name(i) == "Link").unwrap();
        assert!(t.node(link).has(flags::NOT_FOLLOWED));
        let hard = (0..t.len() as u32).filter(|&i| t.node(i).has(flags::HARDLINK)).count();
        assert_eq!(hard, 1);
    }
}

#[cfg(test)]
mod plan_tests {
    use super::record::Run;
    use super::*;

    #[test]
    fn plans_sequential_chunks_without_bitmap() {
        // One run of 10 MiB, 4 KiB clusters, 1 KiB records, 10 240 records.
        let runs = [Run { lcn: Some(100), clusters: 2560 }];
        let plan = plan_pieces(&runs, 4096, 1024, 10_240, None);
        assert_eq!(plan.len(), 3);
        assert_eq!((plan[0].offset, plan[0].bytes, plan[0].first_record), (409_600, 4 << 20, 0));
        assert_eq!(plan[2].first_record, 8192);
        assert_eq!(plan.iter().map(|p| p.records).sum::<usize>(), 10_240);
    }

    #[test]
    fn skips_free_blocks_and_follows_runs() {
        // Two runs of 256 KiB each (4 blocks of 64 records); only record 70 and
        // records in the second run's last block are in use.
        let runs =
            [Run { lcn: Some(10), clusters: 64 }, Run { lcn: None, clusters: 0 }, Run { lcn: Some(500), clusters: 64 }];
        let mut bitmap = vec![0u8; 64];
        bitmap[70 / 8] |= 1 << (70 % 8);
        bitmap[500 / 8] |= 1 << (500 % 8);
        let plan = plan_pieces(&runs, 4096, 1024, 512, Some(&bitmap));
        assert_eq!(plan.len(), 2);
        assert_eq!((plan[0].offset, plan[0].first_record, plan[0].records), (10 * 4096 + 65_536, 64, 64));
        assert_eq!((plan[1].offset, plan[1].first_record, plan[1].records), (500 * 4096 + 3 * 65_536, 448, 64));
    }

    #[test]
    fn stops_at_valid_data_length() {
        let runs = [Run { lcn: Some(0), clusters: 1000 }];
        let plan = plan_pieces(&runs, 4096, 1024, 100, None);
        assert_eq!(plan.iter().map(|p| p.records).sum::<usize>(), 100);
    }
}
