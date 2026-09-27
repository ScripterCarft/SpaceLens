//! Administrator rights (UAC).
//!
//! SpaceLens follows the standard Windows model: the executable's manifest
//! requests `asInvoker`, so it starts without a UAC prompt. When the user
//! wants a complete scan, [`relaunch_elevated`] asks Windows to start a new
//! instance through the documented `runas` verb of `ShellExecuteExW`, which
//! shows the regular UAC consent dialog. There is no service, scheduled task
//! or other UAC bypass involved.

use std::ffi::OsString;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::{mem, ptr};

use windows_sys::Win32::Foundation::{ERROR_CANCELLED, ERROR_NOT_ALL_ASSIGNED, GetLastError, HANDLE, LUID};
use windows_sys::Win32::Security::{
    AdjustTokenPrivileges, GetTokenInformation, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW, SE_BACKUP_NAME,
    SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES, TOKEN_ELEVATION, TOKEN_PRIVILEGES, TOKEN_QUERY, TokenElevation,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows_sys::Win32::UI::Shell::{SEE_MASK_NOASYNC, SHELLEXECUTEINFOW, ShellExecuteExW};
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

use crate::path::wide_nul;

fn process_token(access: u32) -> Option<OwnedHandle> {
    let mut token: HANDLE = ptr::null_mut();
    // SAFETY: the pseudo handle of the current process is always valid and
    // `token` is a valid out pointer.
    let ok = unsafe { OpenProcessToken(GetCurrentProcess(), access, &mut token) };
    // SAFETY: on success we exclusively own the returned token handle.
    (ok != 0).then(|| unsafe { OwnedHandle::from_raw_handle(token) })
}

/// Whether the process runs with a full administrator token.
pub fn is_elevated() -> bool {
    let Some(token) = process_token(TOKEN_QUERY) else {
        return false;
    };
    let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut len = 0u32;
    // SAFETY: `elevation` is a live out buffer of the size passed.
    let ok = unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenElevation,
            ptr::from_mut(&mut elevation).cast(),
            mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        )
    };
    ok != 0 && elevation.TokenIsElevated != 0
}

/// Enables `SeBackupPrivilege`, which lets directory opens with backup
/// semantics bypass ACLs, so protected folders (`System Volume Information`,
/// other users' profiles) can be measured. Read-only; succeeds only when
/// elevated. Returns whether the privilege is now active.
pub fn enable_backup_privilege() -> bool {
    let Some(token) = process_token(TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY) else {
        return false;
    };
    let mut luid = LUID { LowPart: 0, HighPart: 0 };
    // SAFETY: SE_BACKUP_NAME is a static NUL-terminated string; `luid` is a
    // valid out pointer.
    if unsafe { LookupPrivilegeValueW(ptr::null(), SE_BACKUP_NAME, &mut luid) } == 0 {
        return false;
    }
    let privileges = TOKEN_PRIVILEGES {
        PrivilegeCount: 1,
        Privileges: [LUID_AND_ATTRIBUTES { Luid: luid, Attributes: SE_PRIVILEGE_ENABLED }],
    };
    // SAFETY: `privileges` is a fully initialised TOKEN_PRIVILEGES; we do not
    // request the previous state.
    let ok =
        unsafe { AdjustTokenPrivileges(token.as_raw_handle(), 0, &privileges, 0, ptr::null_mut(), ptr::null_mut()) };
    // AdjustTokenPrivileges "succeeds" even if the privilege is not held.
    // SAFETY: trivially safe; reads the calling thread's last error.
    ok != 0 && unsafe { GetLastError() } != ERROR_NOT_ALL_ASSIGNED
}

#[derive(Debug, PartialEq, Eq)]
pub enum RelaunchError {
    /// The user declined the UAC prompt.
    Cancelled,
    Failed(u32),
}

/// Starts a new elevated instance of the current executable with `args`.
/// The caller should exit after `Ok(())`.
pub fn relaunch_elevated(args: &[OsString]) -> Result<(), RelaunchError> {
    let exe = std::env::current_exe().map_err(|e| RelaunchError::Failed(e.raw_os_error().unwrap_or(0) as u32))?;
    let file = wide_nul(exe.as_os_str());
    let params = wide_nul(join_args(args));
    let verb = wide_nul("runas");

    // SAFETY: SHELLEXECUTEINFOW is plain-old-data; zero is valid for every field.
    let mut info: SHELLEXECUTEINFOW = unsafe { mem::zeroed() };
    info.cbSize = mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOASYNC;
    info.lpVerb = verb.as_ptr();
    info.lpFile = file.as_ptr();
    info.lpParameters = params.as_ptr();
    info.nShow = SW_SHOWNORMAL;

    // SAFETY: all string pointers reference NUL-terminated buffers that
    // outlive the call.
    if unsafe { ShellExecuteExW(&mut info) } != 0 {
        return Ok(());
    }
    // SAFETY: trivially safe; reads the calling thread's last error.
    match unsafe { GetLastError() } {
        ERROR_CANCELLED => Err(RelaunchError::Cancelled),
        code => Err(RelaunchError::Failed(code)),
    }
}

/// Joins arguments into one command line following the
/// `CommandLineToArgvW` quoting rules.
pub fn join_args(args: &[OsString]) -> OsString {
    let mut out = String::new();
    for (i, arg) in args.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        quote_arg(&arg.to_string_lossy(), &mut out);
    }
    OsString::from(out)
}

fn quote_arg(arg: &str, out: &mut String) {
    if !arg.is_empty() && !arg.contains([' ', '\t', '\n', '"']) {
        out.push_str(arg);
        return;
    }
    out.push('"');
    let mut backslashes = 0usize;
    for ch in arg.chars() {
        match ch {
            '\\' => backslashes += 1,
            '"' => {
                out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.extend(std::iter::repeat_n('\\', backslashes));
                out.push(ch);
                backslashes = 0;
            }
        }
    }
    // Backslashes before the closing quote must be doubled.
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    fn join(args: &[&str]) -> String {
        let v: Vec<OsString> = args.iter().map(OsString::from).collect();
        join_args(&v).to_string_lossy().into_owned()
    }

    #[test]
    fn quotes_like_command_line_to_argv() {
        assert_eq!(join(&["--scan", r"C:\"]), r"--scan C:\");
        assert_eq!(join(&["--scan", r"C:\Program Files\"]), r#"--scan "C:\Program Files\\""#);
        assert_eq!(join(&["a b", ""]), r#""a b" """#);
        assert_eq!(join(&[r#"say "hi""#]), r#""say \"hi\"""#);
    }

    #[test]
    fn elevation_query_does_not_fail() {
        // Value depends on how the tests are run; it must simply not panic.
        let _ = is_elevated();
    }
}
