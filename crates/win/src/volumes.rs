//! Enumeration of drive letters.

use windows_sys::Win32::Storage::FileSystem::{
    GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW,
};
use windows_sys::Win32::System::WindowsProgramming::{
    DRIVE_CDROM, DRIVE_FIXED, DRIVE_RAMDISK, DRIVE_REMOTE, DRIVE_REMOVABLE,
};

use crate::path::wide_nul;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DriveKind {
    Fixed,
    Removable,
    Network,
    Optical,
    Ram,
    Unknown,
}

impl DriveKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Fixed => "Lokaler Datenträger",
            Self::Removable => "Wechseldatenträger",
            Self::Network => "Netzlaufwerk",
            Self::Optical => "Optisches Laufwerk",
            Self::Ram => "RAM-Disk",
            Self::Unknown => "Laufwerk",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Volume {
    /// Root path, e.g. `C:\`.
    pub root: String,
    pub label: String,
    /// File system name, e.g. `NTFS`.
    pub fs: String,
    pub total: u64,
    pub free: u64,
    pub kind: DriveKind,
}

impl Volume {
    pub fn used(&self) -> u64 {
        self.total.saturating_sub(self.free)
    }
}

/// Lists all mounted drive letters that currently have media.
pub fn list() -> Vec<Volume> {
    // SAFETY: no arguments; returns a bitmask.
    let mask = unsafe { GetLogicalDrives() };
    (0..26u8).filter(|i| mask & (1 << i) != 0).filter_map(|i| query(char::from(b'A' + i))).collect()
}

fn query(letter: char) -> Option<Volume> {
    let root = format!("{letter}:\\");
    let root_w = wide_nul(&root);
    // SAFETY: `root_w` is NUL-terminated and outlives the call.
    let kind = match unsafe { GetDriveTypeW(root_w.as_ptr()) } {
        DRIVE_FIXED => DriveKind::Fixed,
        DRIVE_REMOVABLE => DriveKind::Removable,
        DRIVE_REMOTE => DriveKind::Network,
        DRIVE_CDROM => DriveKind::Optical,
        DRIVE_RAMDISK => DriveKind::Ram,
        0 | 1 => return None, // unknown / no root directory
        _ => DriveKind::Unknown,
    };

    let mut label = [0u16; 261];
    let mut fs = [0u16; 261];
    // SAFETY: output buffers are live and their sizes are passed; unused out
    // parameters are null.
    let ok = unsafe {
        GetVolumeInformationW(
            root_w.as_ptr(),
            label.as_mut_ptr(),
            label.len() as u32,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            fs.as_mut_ptr(),
            fs.len() as u32,
        )
    };
    if ok == 0 {
        return None; // e.g. card reader or DVD drive without media
    }

    let (mut free, mut total) = (0u64, 0u64);
    // SAFETY: out pointers reference live locals; the third one may be null.
    unsafe { GetDiskFreeSpaceExW(root_w.as_ptr(), &mut free, &mut total, std::ptr::null_mut()) };

    Some(Volume { root, label: from_wide_z(&label), fs: from_wide_z(&fs), total, free, kind })
}

/// File system name of a volume root such as `C:\`, e.g. `NTFS`.
pub fn file_system(root: &str) -> Option<String> {
    let root_w = wide_nul(root);
    let mut fs = [0u16; 64];
    // SAFETY: `root_w` is NUL-terminated; the output buffer is live and its
    // size is passed; unused out parameters are null.
    let ok = unsafe {
        GetVolumeInformationW(
            root_w.as_ptr(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            fs.as_mut_ptr(),
            fs.len() as u32,
        )
    };
    (ok != 0).then(|| from_wide_z(&fs))
}

fn from_wide_z(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

#[cfg(test)]
mod tests {
    #[test]
    fn lists_system_drive() {
        let vols = super::list();
        let sys = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
        assert!(vols.iter().any(|v| v.root.starts_with(&sys)), "{vols:?}");
    }
}
