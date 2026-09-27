//! Moving items to the recycle bin via the Shell's `IFileOperation` – the same
//! engine Explorer uses. Items can be restored from the recycle bin; items
//! too large for it trigger Windows' own "delete permanently?" warning
//! (`FOF_WANTNUKEWARNING`) instead of being deleted silently.

use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::Win32::UI::Shell::{
    FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_WANTNUKEWARNING, FOFX_RECYCLEONDELETE, FileOperation, IFileOperation,
    IShellItem, SHCreateItemFromParsingName,
};
use windows::core::HSTRING;

/// Result of a recycle request. Which items are really gone is determined
/// afterwards by checking the file system ([`exists`]), because the user may
/// skip or cancel individual items in the Windows dialogs.
#[derive(Debug, Default)]
pub struct RecycleReport {
    /// The user cancelled (at least part of) the operation.
    pub aborted: bool,
    /// Paths that could not even be queued (e.g. vanished, path too long).
    pub failed_to_queue: Vec<String>,
}

/// Balances `CoInitializeEx` on the current thread.
struct ComGuard;

impl Drop for ComGuard {
    fn drop(&mut self) {
        // SAFETY: only constructed after a successful CoInitializeEx on this thread.
        unsafe { CoUninitialize() };
    }
}

/// Moves `paths` to the recycle bin. Blocks until Windows has finished (a
/// progress dialog is shown for long operations), so call it from a
/// background thread. `owner` is the HWND of the main window (dialogs become
/// modal to it).
pub fn recycle(paths: &[String], owner: Option<isize>) -> Result<RecycleReport, String> {
    // SAFETY: initialises COM for this (worker) thread; balanced by ComGuard.
    let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };
    if hr.is_err() {
        return Err(format!("COM konnte nicht initialisiert werden ({hr:?})"));
    }
    let _com = ComGuard;

    // SAFETY: standard COM activation of the documented FileOperation class.
    let op: IFileOperation =
        unsafe { CoCreateInstance(&FileOperation, None, CLSCTX_ALL) }.map_err(|e| format!("IFileOperation: {e}"))?;
    // SAFETY: valid interface pointer; flags are documented constants.
    unsafe {
        op.SetOperationFlags(FOF_ALLOWUNDO | FOFX_RECYCLEONDELETE | FOF_NOCONFIRMATION | FOF_WANTNUKEWARNING)
            .map_err(|e| e.to_string())?;
        if let Some(hwnd) = owner {
            let _ = op.SetOwnerWindow(HWND(hwnd as *mut core::ffi::c_void));
        }
    }

    let mut report = RecycleReport::default();
    let mut queued = 0usize;
    for path in paths {
        // SAFETY: HSTRING provides a valid NUL-terminated wide string for the call.
        let item: windows::core::Result<IShellItem> =
            unsafe { SHCreateItemFromParsingName(&HSTRING::from(path.as_str()), None) };
        // SAFETY: `item` is a valid shell item owned by this scope.
        match item.and_then(|item| unsafe { op.DeleteItem(&item, None) }) {
            Ok(()) => queued += 1,
            Err(_) => report.failed_to_queue.push(path.clone()),
        }
    }
    if queued == 0 {
        return Ok(report);
    }

    // SAFETY: all items were queued on this interface in this thread.
    let performed = unsafe { op.PerformOperations() };
    // SAFETY: as above.
    report.aborted = unsafe { op.GetAnyOperationsAborted() }.is_ok_and(|b| b.as_bool());
    if let Err(e) = performed
        && !report.aborted
    {
        return Err(e.message());
    }
    Ok(report)
}

/// Whether a file or directory still exists (links are not followed).
pub fn exists(path: &str) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "moves a file into the real recycle bin of the current user"]
    fn recycles_a_temp_file() {
        let path = std::env::temp_dir().join(format!("spacelens-recycle-{}.txt", std::process::id()));
        std::fs::write(&path, b"x").unwrap();
        let p = path.to_string_lossy().into_owned();
        let report = recycle(std::slice::from_ref(&p), None).unwrap();
        assert!(!report.aborted);
        assert!(report.failed_to_queue.is_empty());
        assert!(!exists(&p), "file should be in the recycle bin now");
    }

    #[test]
    fn missing_paths_are_reported_not_fatal() {
        let report = recycle(&[r"C:\definitely\missing\spacelens.xyz".to_string()], None).unwrap();
        assert_eq!(report.failed_to_queue.len(), 1);
    }
}
