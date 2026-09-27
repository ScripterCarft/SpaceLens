//! Path normalisation for Win32 calls.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

/// A scan root in the two forms we need.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Root {
    /// Human-readable absolute path, e.g. `C:\` or `C:\Users`.
    pub display: String,
    /// Verbatim (`\\?\`) UTF-16 path without terminating NUL. Bypasses
    /// `MAX_PATH` and Win32 path parsing so every legal name is reachable.
    pub verbatim: Vec<u16>,
    /// Drive letter if the root is on a lettered drive.
    pub drive: Option<char>,
}

impl Root {
    pub fn new(input: &Path) -> std::io::Result<Self> {
        let mut raw: Vec<u16> = input.as_os_str().encode_wide().collect();
        // "C:" alone would mean "current directory on C:" — users mean the drive.
        if raw.len() == 2 && raw[1] == u16::from(b':') {
            raw.push(u16::from(b'\\'));
        }
        let abs =
            std::path::absolute(Path::new(&String::from_utf16_lossy(&raw))).or_else(|_| std::path::absolute(input))?;
        let mut wide: Vec<u16> = abs.as_os_str().encode_wide().collect();

        let verbatim_prefix: Vec<u16> = r"\\?\".encode_utf16().collect();
        let is_verbatim = wide.starts_with(&verbatim_prefix);
        let is_unc = !is_verbatim && wide.starts_with(&[u16::from(b'\\'), u16::from(b'\\')]);

        trim_trailing_separators(&mut wide);

        let display = String::from_utf16_lossy(&wide);
        let verbatim = if is_verbatim {
            wide.clone()
        } else if is_unc {
            let mut v: Vec<u16> = r"\\?\UNC\".encode_utf16().collect();
            v.extend_from_slice(&wide[2..]);
            v
        } else {
            let mut v = verbatim_prefix;
            v.extend_from_slice(&wide);
            v
        };
        let drive = display
            .strip_prefix(r"\\?\")
            .unwrap_or(&display)
            .chars()
            .next()
            .filter(|c| c.is_ascii_alphabetic() && display.contains(':'))
            .map(|c| c.to_ascii_uppercase());
        Ok(Self { display, verbatim, drive })
    }
}

/// Removes trailing `\` except the one of a drive root (`C:\`).
fn trim_trailing_separators(wide: &mut Vec<u16>) {
    let sep = u16::from(b'\\');
    while wide.len() > 3 && wide.last() == Some(&sep) {
        wide.pop();
    }
    if wide.len() == 2 && wide[1] == u16::from(b':') {
        wide.push(sep);
    }
}

/// NUL-terminated UTF-16 copy of a string for Win32 `PCWSTR` parameters.
pub fn wide_nul(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    #[test]
    fn drive_roots() {
        for input in ["C:", "C:\\", "c:\\\\"] {
            let r = Root::new(Path::new(input)).unwrap();
            assert_eq!(r.display.to_uppercase(), "C:\\");
            assert_eq!(r.verbatim, v(&format!(r"\\?\{}", r.display)));
            assert_eq!(r.drive, Some('C'));
        }
    }

    #[test]
    fn folders_and_unc() {
        let r = Root::new(Path::new(r"D:\Data\Games\")).unwrap();
        assert_eq!(r.display, r"D:\Data\Games");
        assert_eq!(r.verbatim, v(r"\\?\D:\Data\Games"));

        let r = Root::new(Path::new(r"\\server\share\dir")).unwrap();
        assert_eq!(r.verbatim, v(r"\\?\UNC\server\share\dir"));
        assert_eq!(r.drive, None);
    }
}
