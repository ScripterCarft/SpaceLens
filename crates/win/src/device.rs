//! Storage device properties.

use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::{mem, ptr};

use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{
    DEVICE_SEEK_PENALTY_DESCRIPTOR, IOCTL_STORAGE_QUERY_PROPERTY, PropertyStandardQuery, STORAGE_PROPERTY_QUERY,
    StorageDeviceSeekPenaltyProperty,
};

use crate::path::wide_nul;

/// Whether the disk behind drive `letter` has a seek penalty (i.e. is a
/// rotational HDD). `None` if unknown (RAID, Storage Spaces, virtual disks).
///
/// Opens the volume with zero access rights, which needs no elevation.
pub fn seek_penalty(letter: char) -> Option<bool> {
    let name = wide_nul(format!(r"\\.\{letter}:"));
    // SAFETY: `name` is NUL-terminated and outlives the call; zero access
    // rights only allow querying device properties.
    let h = unsafe {
        CreateFileW(
            name.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            ptr::null(),
            OPEN_EXISTING,
            0,
            ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return None;
    }
    // SAFETY: `h` is a valid handle we exclusively own.
    let volume = unsafe { OwnedHandle::from_raw_handle(h) };

    let query = STORAGE_PROPERTY_QUERY {
        PropertyId: StorageDeviceSeekPenaltyProperty,
        QueryType: PropertyStandardQuery,
        AdditionalParameters: [0],
    };
    // SAFETY: plain-old-data struct, all-zero is a valid value.
    let mut desc: DEVICE_SEEK_PENALTY_DESCRIPTOR = unsafe { mem::zeroed() };
    let mut returned = 0u32;
    // SAFETY: input/output pointers reference live locals of the stated sizes.
    let ok = unsafe {
        DeviceIoControl(
            volume.as_raw_handle(),
            IOCTL_STORAGE_QUERY_PROPERTY,
            ptr::from_ref(&query).cast(),
            mem::size_of_val(&query) as u32,
            ptr::from_mut(&mut desc).cast(),
            mem::size_of_val(&desc) as u32,
            &mut returned,
            ptr::null_mut(),
        )
    };
    (ok != 0 && returned as usize >= mem::size_of_val(&desc)).then_some(desc.IncursSeekPenalty)
}
