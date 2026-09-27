//! Compact, lossless storage of file names.
//!
//! Windows file names are arbitrary sequences of UTF-16 code units and may
//! contain unpaired surrogates, so they cannot always be represented as UTF-8.
//! We store them as [WTF-8](https://simonsapin.github.io/wtf-8/): identical to
//! UTF-8 for every valid name (the overwhelming majority, so display needs no
//! conversion) and still able to round-trip the rare ill-formed ones. For
//! typical ASCII-heavy names this halves the memory compared to UTF-16.

use std::borrow::Cow;

/// Append-only byte arena holding all names of a tree.
///
/// Names are addressed by `(offset, len)` pairs stored in the nodes, which is
/// far cheaper than one heap allocation per name.
#[derive(Clone, Debug, Default)]
pub struct NamePool {
    bytes: Vec<u8>,
}

impl NamePool {
    pub fn with_capacity(bytes: usize) -> Self {
        Self { bytes: Vec::with_capacity(bytes) }
    }

    /// Appends already WTF-8 encoded bytes and returns their offset, or `None`
    /// if the pool would exceed the addressable 4 GiB.
    pub fn push_encoded(&mut self, name: &[u8]) -> Option<u32> {
        let off = u32::try_from(self.bytes.len()).ok()?;
        u32::try_from(self.bytes.len() + name.len()).ok()?;
        self.bytes.extend_from_slice(name);
        Some(off)
    }

    #[inline]
    pub fn get(&self, off: u32, len: u16) -> &[u8] {
        let start = off as usize;
        self.bytes.get(start..start + len as usize).unwrap_or_default()
    }

    pub fn len_bytes(&self) -> usize {
        self.bytes.len()
    }

    pub fn capacity_bytes(&self) -> usize {
        self.bytes.capacity()
    }

    pub fn shrink_to_fit(&mut self) {
        self.bytes.shrink_to_fit();
    }
}

/// Encodes UTF-16 code units as WTF-8 and appends them to `out`.
///
/// Well-formed surrogate pairs become 4-byte sequences; unpaired surrogates
/// are encoded as 3-byte sequences (the WTF-8 extension of UTF-8).
pub fn push_wtf8(out: &mut Vec<u8>, wide: &[u16]) {
    out.reserve(wide.len());
    let mut i = 0;
    while i < wide.len() {
        let unit = wide[i];
        // ASCII fast path: most names are plain ASCII.
        if unit < 0x80 {
            out.push(unit as u8);
            i += 1;
            continue;
        }
        let mut cp = u32::from(unit);
        i += 1;
        if (0xD800..0xDC00).contains(&unit)
            && let Some(&low) = wide.get(i)
            && (0xDC00..0xE000).contains(&low)
        {
            cp = 0x10000 + ((cp - 0xD800) << 10) + (u32::from(low) - 0xDC00);
            i += 1;
        }
        encode_code_point(out, cp);
    }
}

#[inline]
fn encode_code_point(out: &mut Vec<u8>, cp: u32) {
    if cp < 0x80 {
        out.push(cp as u8);
    } else if cp < 0x800 {
        out.extend_from_slice(&[0xC0 | (cp >> 6) as u8, 0x80 | (cp & 0x3F) as u8]);
    } else if cp < 0x10000 {
        out.extend_from_slice(&[0xE0 | (cp >> 12) as u8, 0x80 | ((cp >> 6) & 0x3F) as u8, 0x80 | (cp & 0x3F) as u8]);
    } else {
        out.extend_from_slice(&[
            0xF0 | (cp >> 18) as u8,
            0x80 | ((cp >> 12) & 0x3F) as u8,
            0x80 | ((cp >> 6) & 0x3F) as u8,
            0x80 | (cp & 0x3F) as u8,
        ]);
    }
}

/// Decodes WTF-8 produced by [`push_wtf8`] back into UTF-16 code units,
/// appending them to `out`. Used to rebuild exact paths for OS calls.
pub fn wtf8_to_wide(bytes: &[u8], out: &mut Vec<u16>) {
    out.reserve(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b0 = u32::from(bytes[i]);
        let (cp, width) = if b0 < 0x80 {
            (b0, 1)
        } else if b0 < 0xE0 {
            (((b0 & 0x1F) << 6) | cont(bytes, i + 1), 2)
        } else if b0 < 0xF0 {
            (((b0 & 0x0F) << 12) | (cont(bytes, i + 1) << 6) | cont(bytes, i + 2), 3)
        } else {
            (((b0 & 0x07) << 18) | (cont(bytes, i + 1) << 12) | (cont(bytes, i + 2) << 6) | cont(bytes, i + 3), 4)
        };
        i += width;
        if cp >= 0x10000 {
            let v = cp - 0x10000;
            out.push(0xD800 | (v >> 10) as u16);
            out.push(0xDC00 | (v & 0x3FF) as u16);
        } else {
            out.push(cp as u16);
        }
    }
}

#[inline]
fn cont(bytes: &[u8], i: usize) -> u32 {
    bytes.get(i).map_or(0, |b| u32::from(b & 0x3F))
}

/// Returns a displayable string; borrowed (zero-copy) for valid UTF-8, which
/// covers practically every real-world name.
#[inline]
pub fn display(bytes: &[u8]) -> Cow<'_, str> {
    String::from_utf8_lossy(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(wide: &[u16]) {
        let mut enc = Vec::new();
        push_wtf8(&mut enc, wide);
        let mut dec = Vec::new();
        wtf8_to_wide(&enc, &mut dec);
        assert_eq!(dec, wide);
    }

    #[test]
    fn ascii_and_unicode_roundtrip() {
        for s in ["", "a", "Program Files", "Übergröße.txt", "日本語", "emoji 😀 file", "ß\u{7FF}\u{800}\u{FFFF}"]
        {
            let wide: Vec<u16> = s.encode_utf16().collect();
            roundtrip(&wide);
            let mut enc = Vec::new();
            push_wtf8(&mut enc, &wide);
            assert_eq!(enc, s.as_bytes(), "valid UTF-16 must encode to plain UTF-8");
        }
    }

    #[test]
    fn unpaired_surrogates_roundtrip() {
        roundtrip(&[0xD800]);
        roundtrip(&[0xDC00, 0x41]);
        roundtrip(&[0x41, 0xD83D, 0x42]);
        roundtrip(&[0xDBFF, 0xDBFF, 0xDFFF]);
    }

    #[test]
    fn display_is_lossy_for_surrogates() {
        let mut enc = Vec::new();
        push_wtf8(&mut enc, &[0x61, 0xD800, 0x62]);
        let shown = display(&enc);
        assert!(shown.starts_with('a') && shown.ends_with('b'));
    }

    #[test]
    fn pool_addresses_names() {
        let mut pool = NamePool::default();
        let a = pool.push_encoded(b"alpha").unwrap();
        let b = pool.push_encoded(b"beta").unwrap();
        assert_eq!(pool.get(a, 5), b"alpha");
        assert_eq!(pool.get(b, 4), b"beta");
        assert_eq!(pool.get(100, 4), b"", "out of range must not panic");
    }
}
