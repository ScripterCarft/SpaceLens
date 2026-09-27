//! Raw, unbuffered read access to an NTFS volume (requires administrator).

use std::alloc::{self, Layout};
use std::ops::{Deref, DerefMut};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::{mem, ptr};

use windows_sys::Win32::Foundation::{GENERIC_READ, GetLastError, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_NO_BUFFERING, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, ReadFile,
};
use windows_sys::Win32::System::IO::{DeviceIoControl, OVERLAPPED};
use windows_sys::Win32::System::Ioctl::{FSCTL_GET_NTFS_VOLUME_DATA, NTFS_VOLUME_DATA_BUFFER};

use crate::path::wide_nul;

/// Alignment satisfying `FILE_FLAG_NO_BUFFERING` for all sector sizes in use.
const ALIGN: usize = 4096;

/// Heap buffer aligned to [`ALIGN`], as unbuffered I/O requires.
pub struct AlignedBuf {
    ptr: ptr::NonNull<u8>,
    len: usize,
}

// SAFETY: the buffer exclusively owns its allocation; no interior sharing.
unsafe impl Send for AlignedBuf {}

impl AlignedBuf {
    /// Allocates `len` zeroed bytes (`len` is rounded up to [`ALIGN`]).
    pub fn new(len: usize) -> Option<Self> {
        let len = len.max(1).div_ceil(ALIGN) * ALIGN;
        let layout = Layout::from_size_align(len, ALIGN).ok()?;
        // SAFETY: `layout` has a non-zero size.
        let raw = unsafe { alloc::alloc_zeroed(layout) };
        Some(Self { ptr: ptr::NonNull::new(raw)?, len })
    }
}

impl Deref for AlignedBuf {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        // SAFETY: `ptr` points to `len` initialised (zeroed) bytes we own.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}

impl DerefMut for AlignedBuf {
    fn deref_mut(&mut self) -> &mut [u8] {
        // SAFETY: as above, and `&mut self` guarantees exclusive access.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        // SAFETY: allocated in `new` with exactly this layout.
        unsafe { alloc::dealloc(self.ptr.as_ptr(), Layout::from_size_align_unchecked(self.len, ALIGN)) };
    }
}

/// Geometry of an NTFS volume.
#[derive(Clone, Copy, Debug)]
pub struct NtfsInfo {
    pub bytes_per_cluster: u64,
    pub bytes_per_record: u64,
    pub mft_start_lcn: u64,
    pub mft_valid_bytes: u64,
}

pub struct Volume {
    handle: OwnedHandle,
}

impl Volume {
    /// Opens `\\.\X:` for raw reading. Fails with `ERROR_ACCESS_DENIED`
    /// unless the process is elevated.
    pub fn open(letter: char) -> Result<Self, u32> {
        let name = wide_nul(format!(r"\\.\{letter}:"));
        // SAFETY: `name` is NUL-terminated and outlives the call.
        let h = unsafe {
            CreateFileW(
                name.as_ptr(),
                GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                ptr::null(),
                OPEN_EXISTING,
                // Bypass the cache: the MFT is read once, sequentially, and
                // must not evict the user's working set from memory.
                FILE_FLAG_NO_BUFFERING,
                ptr::null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            // SAFETY: trivially safe.
            return Err(unsafe { GetLastError() });
        }
        // SAFETY: valid handle, exclusively owned from here on.
        Ok(Self { handle: unsafe { OwnedHandle::from_raw_handle(h) } })
    }

    /// Queries NTFS geometry; fails on non-NTFS volumes.
    pub fn ntfs_info(&self) -> Result<NtfsInfo, u32> {
        // SAFETY: plain-old-data; zero is a valid value.
        let mut d: NTFS_VOLUME_DATA_BUFFER = unsafe { mem::zeroed() };
        let mut returned = 0u32;
        // SAFETY: the output buffer is a live local of the size passed.
        let ok = unsafe {
            DeviceIoControl(
                self.handle.as_raw_handle(),
                FSCTL_GET_NTFS_VOLUME_DATA,
                ptr::null(),
                0,
                ptr::from_mut(&mut d).cast(),
                mem::size_of_val(&d) as u32,
                &mut returned,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            // SAFETY: trivially safe.
            return Err(unsafe { GetLastError() });
        }
        Ok(NtfsInfo {
            bytes_per_cluster: u64::from(d.BytesPerCluster),
            bytes_per_record: u64::from(d.BytesPerFileRecordSegment),
            mft_start_lcn: u64::try_from(d.MftStartLcn).unwrap_or(0),
            mft_valid_bytes: u64::try_from(d.MftValidDataLength).unwrap_or(0),
        })
    }

    /// Reads `buf.len()` bytes at `offset` (both must be sector-aligned).
    /// Safe to call from several threads: the offset travels in the
    /// `OVERLAPPED`, not in a shared file pointer.
    pub fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, u32> {
        // SAFETY: plain-old-data; zero is a valid value.
        let mut ov: OVERLAPPED = unsafe { mem::zeroed() };
        ov.Anonymous.Anonymous.Offset = offset as u32;
        ov.Anonymous.Anonymous.OffsetHigh = (offset >> 32) as u32;
        let mut read = 0u32;
        let len = u32::try_from(buf.len()).map_err(|_| 87u32)?;
        // SAFETY: `buf` is writable for `len` bytes and outlives the
        // (synchronous) call; `ov` lives until the call returns.
        let ok = unsafe { ReadFile(self.handle.as_raw_handle(), buf.as_mut_ptr(), len, &mut read, &mut ov) };
        if ok == 0 {
            // SAFETY: trivially safe.
            return Err(unsafe { GetLastError() });
        }
        Ok(read as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aligned_buffer_is_aligned_and_zeroed() {
        let mut b = AlignedBuf::new(5000).unwrap();
        assert_eq!(b.as_ptr() as usize % ALIGN, 0);
        assert_eq!(b.len(), 8192);
        assert!(b.iter().all(|&x| x == 0));
        b[8191] = 1;
    }
}
