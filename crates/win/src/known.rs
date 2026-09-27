//! Locations of well-known folders via `SHGetKnownFolderPath` (the documented
//! way; environment variables can be missing, redirected or contain 8.3 names).

use spacelens_core::cleanup::Base;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::{
    FOLDERID_LocalAppData, FOLDERID_Profile, FOLDERID_ProgramData, FOLDERID_Windows, KF_FLAG_DONT_VERIFY,
    SHGetKnownFolderPath,
};
use windows::core::GUID;

fn known_folder(id: &GUID) -> Option<String> {
    // SAFETY: valid KNOWNFOLDERID; the returned buffer is owned by us and
    // freed with CoTaskMemFree as documented, also on the error path (null).
    unsafe {
        let pwstr = SHGetKnownFolderPath(id, KF_FLAG_DONT_VERIFY, None).ok()?;
        let path = pwstr.to_string().ok();
        CoTaskMemFree(Some(pwstr.0 as *const _));
        path
    }
}

/// Absolute path of a cleanup [`Base`] for the current user.
pub fn resolve(base: Base) -> Option<String> {
    match base {
        Base::LocalAppData => known_folder(&FOLDERID_LocalAppData),
        Base::Profile => known_folder(&FOLDERID_Profile),
        Base::Windows => known_folder(&FOLDERID_Windows),
        Base::ProgramData => known_folder(&FOLDERID_ProgramData),
        Base::SystemDrive => known_folder(&FOLDERID_Windows).and_then(|w| w.get(..3).map(str::to_string)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_standard_folders() {
        let windows = resolve(Base::Windows).unwrap();
        assert!(windows.to_lowercase().ends_with("\\windows"), "{windows}");
        let local = resolve(Base::LocalAppData).unwrap();
        assert!(local.to_lowercase().ends_with("appdata\\local"), "{local}");
        assert_eq!(resolve(Base::SystemDrive).unwrap().len(), 3);
    }
}
