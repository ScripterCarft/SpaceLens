//! Parsing of NTFS file records (FILE records of the Master File Table).
//!
//! Pure, safe code over byte slices: every read is bounds-checked, so a
//! corrupt or unexpected record is skipped instead of causing a crash.
//!
//! Layout references: the on-disk format as documented by the Linux NTFS
//! driver (`layout.h`) and Microsoft's `FILE_RECORD_SEGMENT_HEADER`.

use spacelens_core::names::push_wtf8;

/// Record number of the root directory (`.`).
pub const ROOT_RECORD: u32 = 5;

/// Record header flag: record is in use.
const FLAG_IN_USE: u16 = 0x0001;
/// Record header flag: record has a directory index (`$I30`).
const FLAG_DIRECTORY: u16 = 0x0002;

pub const ATTR_STANDARD_INFORMATION: u32 = 0x10;
pub const ATTR_ATTRIBUTE_LIST: u32 = 0x20;
pub const ATTR_FILE_NAME: u32 = 0x30;
pub const ATTR_DATA: u32 = 0x80;
pub const ATTR_REPARSE_POINT: u32 = 0xC0;
pub const ATTR_BITMAP: u32 = 0xB0;
const ATTR_END: u32 = 0xFFFF_FFFF;

/// Attribute flags that make `total allocated` the real on-disk size.
const ATTR_FLAG_COMPRESSED: u16 = 0x0001;
const ATTR_FLAG_SPARSE: u16 = 0x8000;

/// `$FILE_NAME` namespace that holds only the 8.3 alias of another name.
const NAMESPACE_DOS: u8 = 2;

#[inline]
fn u16_at(b: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(off..off + 2)?.try_into().ok()?))
}

#[inline]
fn u32_at(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(off..off + 4)?.try_into().ok()?))
}

#[inline]
fn u64_at(b: &[u8], off: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(off..off + 8)?.try_into().ok()?))
}

/// Record number part (low 48 bits) of an NTFS file reference.
#[inline]
pub fn record_number(file_ref: u64) -> u64 {
    file_ref & 0x0000_FFFF_FFFF_FFFF
}

/// Applies the update sequence array ("fixups") in place.
///
/// NTFS stores the last two bytes of every 512-byte stride in the USA and
/// replaces them with a sequence number to detect torn writes. Returns `false`
/// for records that are not valid FILE records or were torn.
pub fn apply_fixups(rec: &mut [u8]) -> bool {
    if rec.get(0..4) != Some(b"FILE") {
        return false;
    }
    let (Some(usa_off), Some(usa_count)) = (u16_at(rec, 4), u16_at(rec, 6)) else { return false };
    let (usa_off, usa_count) = (usa_off as usize, usa_count as usize);
    if usa_count < 2 || usa_off + usa_count * 2 > rec.len() {
        return false;
    }
    let stride = rec.len() / (usa_count - 1);
    if stride < 2 || stride * (usa_count - 1) != rec.len() {
        return false;
    }
    let usn = [rec[usa_off], rec[usa_off + 1]];
    for i in 1..usa_count {
        let end = i * stride;
        if rec[end - 2..end] != usn {
            return false;
        }
        rec[end - 2] = rec[usa_off + 2 * i];
        rec[end - 1] = rec[usa_off + 2 * i + 1];
    }
    true
}

/// One attribute inside a record.
#[derive(Clone, Copy)]
pub struct Attr<'a> {
    pub kind: u32,
    raw: &'a [u8],
}

impl<'a> Attr<'a> {
    #[inline]
    pub fn non_resident(&self) -> bool {
        self.raw.get(8).is_some_and(|&b| b != 0)
    }

    #[inline]
    pub fn is_unnamed(&self) -> bool {
        self.raw.get(9) == Some(&0)
    }

    #[inline]
    fn flags(&self) -> u16 {
        u16_at(self.raw, 0x0C).unwrap_or(0)
    }

    /// Value of a resident attribute.
    pub fn value(&self) -> Option<&'a [u8]> {
        if self.non_resident() {
            return None;
        }
        let len = u32_at(self.raw, 0x10)? as usize;
        let off = u16_at(self.raw, 0x14)? as usize;
        self.raw.get(off..off.checked_add(len)?)
    }

    /// First VCN covered by a non-resident attribute segment.
    pub fn lowest_vcn(&self) -> Option<u64> {
        self.non_resident().then(|| u64_at(self.raw, 0x10)).flatten()
    }

    /// Mapping pairs (runlist) of a non-resident attribute.
    pub fn runlist(&self) -> Option<&'a [u8]> {
        if !self.non_resident() {
            return None;
        }
        let off = u16_at(self.raw, 0x20)? as usize;
        self.raw.get(off..)
    }

    /// `(logical size, size on disk)` of an unnamed `$DATA` attribute.
    ///
    /// Resident data is rounded up to 8 bytes, matching what NTFS reports as
    /// `AllocationSize` for such files (and therefore the directory walker).
    pub fn data_sizes(&self) -> Option<(u64, u64)> {
        if !self.non_resident() {
            let len = u32_at(self.raw, 0x10)? as u64;
            return Some((len, len.div_ceil(8) * 8));
        }
        if self.lowest_vcn()? != 0 {
            return None; // later segment of a split attribute; sizes live in the first
        }
        let alloc = u64_at(self.raw, 0x28)?;
        let size = u64_at(self.raw, 0x30)?;
        let on_disk = if self.flags() & (ATTR_FLAG_COMPRESSED | ATTR_FLAG_SPARSE) != 0 {
            u64_at(self.raw, 0x40).unwrap_or(alloc)
        } else {
            alloc
        };
        Some((size, on_disk))
    }
}

/// Iterates the attributes of a record whose fixups have been applied.
pub fn attributes(rec: &[u8]) -> impl Iterator<Item = Attr<'_>> {
    let used = u32_at(rec, 0x18).map_or(0, |u| (u as usize).min(rec.len()));
    let mut off = u16_at(rec, 0x14).map_or(usize::MAX, usize::from);
    std::iter::from_fn(move || {
        let kind = u32_at(rec, off)?;
        if kind == ATTR_END || kind == 0 {
            return None;
        }
        let len = u32_at(rec, off + 4)? as usize;
        if len < 0x18 || off + len > used {
            return None;
        }
        let attr = Attr { kind, raw: &rec[off..off + len] };
        off += len;
        Some(attr)
    })
}

/// Everything the scanner needs from one record.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    pub is_dir: bool,
    /// Record number of the base record for extension records.
    pub base: Option<u64>,
    /// `(size, size on disk)` of the unnamed data stream, if in this record.
    pub data: Option<(u64, u64)>,
    /// Last write time (FILETIME) from `$STANDARD_INFORMATION`.
    pub mtime: Option<i64>,
    /// Win32 file attributes from `$STANDARD_INFORMATION`.
    pub attrs: u32,
    pub reparse_tag: u32,
}

/// Parses a fixed-up, in-use record. `on_name(parent, name_utf16le)` is
/// called for every non-DOS `$FILE_NAME` (one per hard link).
/// Returns `None` for free or malformed records.
pub fn summarize(rec: &[u8], mut on_name: impl FnMut(u64, &[u8])) -> Option<Summary> {
    let flags = u16_at(rec, 0x16)?;
    if flags & FLAG_IN_USE == 0 {
        return None;
    }
    let base = record_number(u64_at(rec, 0x20)?);
    let mut s =
        Summary { is_dir: flags & FLAG_DIRECTORY != 0, base: (base != 0).then_some(base), ..Summary::default() };

    for attr in attributes(rec) {
        match attr.kind {
            ATTR_STANDARD_INFORMATION => {
                if let Some(v) = attr.value() {
                    s.mtime = u64_at(v, 0x08).map(|t| t as i64);
                    s.attrs = u32_at(v, 0x20).unwrap_or(0);
                }
            }
            ATTR_FILE_NAME => {
                let Some(v) = attr.value() else { continue };
                let (Some(parent), Some(&len), Some(&ns)) = (u64_at(v, 0), v.get(0x40), v.get(0x41)) else {
                    continue;
                };
                if ns == NAMESPACE_DOS {
                    continue;
                }
                if let Some(name) = v.get(0x42..0x42 + len as usize * 2) {
                    on_name(record_number(parent), name);
                }
            }
            ATTR_DATA if attr.is_unnamed() => {
                if let Some(d) = attr.data_sizes() {
                    s.data = Some(d);
                }
            }
            ATTR_REPARSE_POINT => {
                s.reparse_tag = attr.value().and_then(|v| u32_at(v, 0)).unwrap_or(0);
            }
            _ => {}
        }
    }
    Some(s)
}

/// Whether the record carries an `$ATTRIBUTE_LIST` (attributes spread over
/// extension records).
pub fn has_attribute_list(rec: &[u8]) -> bool {
    attributes(rec).any(|a| a.kind == ATTR_ATTRIBUTE_LIST)
}

/// The unnamed attribute of type `kind` (e.g. `$DATA`, `$BITMAP`), if present.
pub fn unnamed_attr(rec: &[u8], kind: u32) -> Option<Attr<'_>> {
    attributes(rec).find(|a| a.kind == kind && a.is_unnamed())
}

/// One run of a runlist: `lcn == None` marks a sparse run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Run {
    pub lcn: Option<u64>,
    pub clusters: u64,
}

/// Decodes NTFS mapping pairs. Returns `None` on malformed input.
pub fn decode_runlist(mut b: &[u8]) -> Option<Vec<Run>> {
    let mut runs = Vec::new();
    let mut lcn: i64 = 0;
    loop {
        let (&header, rest) = b.split_first()?;
        if header == 0 {
            return Some(runs);
        }
        let len_size = (header & 0x0F) as usize;
        let off_size = (header >> 4) as usize;
        if len_size == 0 || len_size > 8 || off_size > 8 || rest.len() < len_size + off_size {
            return None;
        }
        let clusters = le_unsigned(&rest[..len_size]);
        let run = if off_size == 0 {
            Run { lcn: None, clusters }
        } else {
            lcn = lcn.checked_add(le_signed(&rest[len_size..len_size + off_size]))?;
            Run { lcn: Some(u64::try_from(lcn).ok()?), clusters }
        };
        runs.push(run);
        b = &rest[len_size + off_size..];
    }
}

fn le_unsigned(b: &[u8]) -> u64 {
    b.iter().rev().fold(0u64, |acc, &x| (acc << 8) | u64::from(x))
}

fn le_signed(b: &[u8]) -> i64 {
    let v = le_unsigned(b);
    let bits = b.len() * 8;
    if bits < 64 && v & (1 << (bits - 1)) != 0 { (v | (!0u64 << bits)) as i64 } else { v as i64 }
}

/// Converts a UTF-16LE name (max. 255 units) to WTF-8 without allocating.
pub fn push_utf16le_name(out: &mut Vec<u8>, name: &[u8]) {
    let mut units = [0u16; 256];
    let n = (name.len() / 2).min(units.len());
    for (i, unit) in units.iter_mut().take(n).enumerate() {
        *unit = u16::from_le_bytes([name[2 * i], name[2 * i + 1]]);
    }
    push_wtf8(out, &units[..n]);
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Builds a synthetic 1024-byte FILE record with fixups applied on disk.
    pub struct RecordBuilder {
        attrs: Vec<u8>,
        flags: u16,
        base: u64,
    }

    impl RecordBuilder {
        pub fn new(flags_dir: bool) -> Self {
            Self { attrs: Vec::new(), flags: FLAG_IN_USE | if flags_dir { FLAG_DIRECTORY } else { 0 }, base: 0 }
        }

        pub fn base(mut self, base: u64) -> Self {
            self.base = base;
            self
        }

        fn resident(mut self, kind: u32, name_len: u8, value: &[u8]) -> Self {
            let len = (0x18 + value.len()).div_ceil(8) * 8;
            let mut a = vec![0u8; len];
            a[0..4].copy_from_slice(&kind.to_le_bytes());
            a[4..8].copy_from_slice(&(len as u32).to_le_bytes());
            a[9] = name_len;
            a[0x10..0x14].copy_from_slice(&(value.len() as u32).to_le_bytes());
            a[0x14..0x16].copy_from_slice(&0x18u16.to_le_bytes());
            a[0x18..0x18 + value.len()].copy_from_slice(value);
            self.attrs.extend_from_slice(&a);
            self
        }

        pub fn std_info(self, mtime: u64, attrs: u32) -> Self {
            let mut v = vec![0u8; 0x30];
            v[0x08..0x10].copy_from_slice(&mtime.to_le_bytes());
            v[0x20..0x24].copy_from_slice(&attrs.to_le_bytes());
            self.resident(ATTR_STANDARD_INFORMATION, 0, &v)
        }

        pub fn file_name(self, parent: u64, name: &str, namespace: u8) -> Self {
            let units: Vec<u16> = name.encode_utf16().collect();
            let mut v = vec![0u8; 0x42 + units.len() * 2];
            v[0..8].copy_from_slice(&(parent | (1u64 << 48)).to_le_bytes()); // with sequence number
            v[0x40] = units.len() as u8;
            v[0x41] = namespace;
            for (i, u) in units.iter().enumerate() {
                v[0x42 + 2 * i..0x44 + 2 * i].copy_from_slice(&u.to_le_bytes());
            }
            self.resident(ATTR_FILE_NAME, 0, &v)
        }

        pub fn build_with_reparse(self, tag: u32) -> Vec<u8> {
            let mut v = tag.to_le_bytes().to_vec();
            v.extend_from_slice(&[0; 4]);
            self.resident(ATTR_REPARSE_POINT, 0, &v).build()
        }

        pub fn resident_data(self, bytes: usize) -> Self {
            self.resident(ATTR_DATA, 0, &vec![0xAB; bytes])
        }

        pub fn nonresident_data(mut self, size: u64, alloc: u64, runs: &[u8], sparse_total: Option<u64>) -> Self {
            let run_off = 0x48usize;
            let len = (run_off + runs.len()).div_ceil(8) * 8;
            let mut a = vec![0u8; len];
            a[0..4].copy_from_slice(&ATTR_DATA.to_le_bytes());
            a[4..8].copy_from_slice(&(len as u32).to_le_bytes());
            a[8] = 1;
            if sparse_total.is_some() {
                a[0x0C..0x0E].copy_from_slice(&ATTR_FLAG_SPARSE.to_le_bytes());
            }
            a[0x20..0x22].copy_from_slice(&(run_off as u16).to_le_bytes());
            a[0x28..0x30].copy_from_slice(&alloc.to_le_bytes());
            a[0x30..0x38].copy_from_slice(&size.to_le_bytes());
            a[0x40..0x48].copy_from_slice(&sparse_total.unwrap_or(0).to_le_bytes());
            a[run_off..run_off + runs.len()].copy_from_slice(runs);
            self.attrs.extend_from_slice(&a);
            self
        }

        /// Returns the record as stored on disk (sequence numbers in place).
        pub fn build(&self) -> Vec<u8> {
            let mut r = vec![0u8; 1024];
            r[0..4].copy_from_slice(b"FILE");
            r[4..6].copy_from_slice(&0x30u16.to_le_bytes()); // USA offset
            r[6..8].copy_from_slice(&3u16.to_le_bytes()); // 1 + 2 strides of 512
            r[0x14..0x16].copy_from_slice(&0x38u16.to_le_bytes());
            r[0x16..0x18].copy_from_slice(&self.flags.to_le_bytes());
            r[0x20..0x28].copy_from_slice(&self.base.to_le_bytes());
            let end = 0x38 + self.attrs.len();
            r[0x38..end].copy_from_slice(&self.attrs);
            r[end..end + 4].copy_from_slice(&ATTR_END.to_le_bytes());
            r[0x18..0x1C].copy_from_slice(&((end + 8) as u32).to_le_bytes());
            // Fixups: move the real stride tails into the USA, write the USN.
            let usn = [0x07, 0x00];
            r[0x30..0x32].copy_from_slice(&usn);
            for i in 1..3 {
                let e = i * 512;
                let (a, b) = (r[e - 2], r[e - 1]);
                r[0x30 + 2 * i] = a;
                r[0x31 + 2 * i] = b;
                r[e - 2..e].copy_from_slice(&usn);
            }
            r
        }
    }

    #[test]
    fn fixups_restore_and_detect_torn_records() {
        let mut rec = RecordBuilder::new(false).file_name(5, "a", 1).build();
        let mut torn = rec.clone();
        assert!(apply_fixups(&mut rec));
        torn[1022] = 0x99;
        assert!(!apply_fixups(&mut torn));
        let mut garbage = vec![0u8; 1024];
        assert!(!apply_fixups(&mut garbage));
    }

    #[test]
    fn summarizes_file_record() {
        let mut rec = RecordBuilder::new(false)
            .std_info(133_000_000_000_000_000, 0x22)
            .file_name(42, "LONGFI~1.TXT", NAMESPACE_DOS)
            .file_name(42, "Lange Datei.txt", 1)
            .file_name(77, "hardlink.txt", 0)
            .resident_data(100)
            .build();
        assert!(apply_fixups(&mut rec));
        let mut names = Vec::new();
        let s = summarize(&rec, |parent, n| {
            let mut out = Vec::new();
            push_utf16le_name(&mut out, n);
            names.push((parent, String::from_utf8(out).unwrap()));
        })
        .unwrap();
        assert_eq!(names, vec![(42, "Lange Datei.txt".to_string()), (77, "hardlink.txt".to_string())]);
        assert!(!s.is_dir);
        assert_eq!(s.data, Some((100, 104)));
        assert_eq!(s.attrs, 0x22);
        assert_eq!(s.mtime, Some(133_000_000_000_000_000));
        assert_eq!(s.base, None);
    }

    #[test]
    fn summarizes_nonresident_sparse_and_extension() {
        let mut rec = RecordBuilder::new(false).base(1234).nonresident_data(1 << 30, 1 << 30, &[0], Some(4096)).build();
        assert!(apply_fixups(&mut rec));
        let s = summarize(&rec, |_, _| {}).unwrap();
        assert_eq!(s.base, Some(1234));
        assert_eq!(s.data, Some((1 << 30, 4096)));
    }

    #[test]
    fn free_records_are_skipped() {
        let mut b = RecordBuilder::new(false);
        b.flags = 0;
        let mut rec = b.build();
        assert!(apply_fixups(&mut rec));
        assert_eq!(summarize(&rec, |_, _| {}), None);
    }

    #[test]
    fn decodes_runlists() {
        // 0x21: 1-byte length, 2-byte offset. Second run goes backwards (negative delta),
        // third run is sparse (offset size 0).
        let runs = decode_runlist(&[0x21, 0x10, 0x00, 0x01, 0x11, 0x05, 0xF0, 0x01, 0x08, 0x00]).unwrap();
        assert_eq!(
            runs,
            vec![
                Run { lcn: Some(0x100), clusters: 0x10 },
                Run { lcn: Some(0x100 - 0x10), clusters: 5 },
                Run { lcn: None, clusters: 8 },
            ]
        );
        assert_eq!(decode_runlist(&[0x21, 0x10]), None, "truncated");
        assert_eq!(decode_runlist(&[0x00]), Some(vec![]));
    }

    #[test]
    fn malformed_attributes_do_not_panic() {
        let mut rec = RecordBuilder::new(true).file_name(5, "x", 1).build();
        assert!(apply_fixups(&mut rec));
        // Corrupt the first attribute length to point past the record.
        rec[0x3C..0x40].copy_from_slice(&5000u32.to_le_bytes());
        let s = summarize(&rec, |_, _| panic!("no names expected")).unwrap();
        assert!(s.is_dir);
    }
}
