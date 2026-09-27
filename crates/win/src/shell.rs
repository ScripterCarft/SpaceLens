//! Integration with Windows Explorer.

use std::os::windows::process::CommandExt;
use std::process::Command;

/// Opens Explorer with `path` selected.
///
/// The argument is passed raw because Explorer expects `/select,"path"`,
/// which differs from the standard argument quoting. Paths cannot contain
/// `"` on Windows, so this cannot be abused to inject arguments.
pub fn reveal_in_explorer(path: &str) -> std::io::Result<()> {
    if path.contains('"') {
        return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
    }
    Command::new("explorer.exe").raw_arg(format!("/select,\"{path}\"")).spawn().map(drop)
}

/// Shows a native, application-modal message box (blocking).
pub fn message_box(title: &str, text: &str, error: bool) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_ICONINFORMATION, MB_OK, MessageBoxW};
    let title = crate::path::wide_nul(title);
    let text = crate::path::wide_nul(text);
    let icon = if error { MB_ICONERROR } else { MB_ICONINFORMATION };
    // SAFETY: both strings are NUL-terminated and outlive the call; a null
    // owner window is allowed.
    unsafe { MessageBoxW(std::ptr::null_mut(), text.as_ptr(), title.as_ptr(), MB_OK | icon) };
}
