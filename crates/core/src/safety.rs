//! Guard rails for deleting: which items must never be removed and which
//! deserve an explicit warning.
//!
//! Rules work on the full path (independent of where the scan started) and
//! are deliberately conservative: blocking a legitimate delete costs a click
//! in Explorer, deleting `System32` costs a Windows installation.

use crate::tree::{NodeId, Tree, flags};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Guard {
    /// Deleting is not offered; the text explains why.
    Blocked(&'static str),
    /// Allowed after confirmation; the text is shown in the dialog.
    Warn(&'static str),
}

const SYSTEM_TOP_DIRS: &[&str] = &[
    "windows",
    "program files",
    "program files (x86)",
    "programdata",
    "users",
    "system volume information",
    "recovery",
    "boot",
    "documents and settings",
    "config.msi",
];

const MANAGED_FILES: &[&str] = &["pagefile.sys", "hiberfil.sys", "swapfile.sys"];
const BOOT_FILES: &[&str] = &["bootmgr", "bootnxt"];

/// Parts of `C:\Windows` whose removal breaks Windows.
const CORE_WINDOWS_DIRS: &[&str] =
    &["system32", "syswow64", "winsxs", "servicing", "boot", "fonts", "systemapps", "systemresources"];

/// Parts of `C:\Windows` that are caches or logs and safe to clean.
const WINDOWS_CLEANABLE: &[&str] = &["temp", "logs", "prefetch", "softwaredistribution"];

const WARN_SYSTEM: &str =
    "Gehört zu Windows. Löschen kann das System beschädigen – nur entfernen, wenn Sie sicher wissen, was es ist.";
const WARN_PROGRAMS: &str = "Gehört zu einem installierten Programm. Besser über „Einstellungen → Apps“ deinstallieren, sonst funktioniert das Programm evtl. nicht mehr.";

/// Evaluates the path components after the drive, lower-cased
/// (e.g. `["windows", "system32"]` for `C:\Windows\System32`).
pub fn guard_for_components(comps: &[&str]) -> Option<Guard> {
    let (first, rest) = comps.split_first()?;
    if rest.is_empty() {
        if SYSTEM_TOP_DIRS.contains(first) || BOOT_FILES.contains(first) {
            return Some(Guard::Blocked("Systemordner von Windows – kann nicht gelöscht werden."));
        }
        if MANAGED_FILES.contains(first) {
            return Some(Guard::Blocked(
                "Wird von Windows verwaltet (Auslagerungs- bzw. Ruhezustandsdatei). Größe über die Systemeinstellungen ändern.",
            ));
        }
    }
    if first.starts_with('$') && *first != "$recycle.bin" {
        return Some(Guard::Blocked("NTFS- bzw. Windows-Systemdatei – kann nicht gelöscht werden."));
    }
    match *first {
        "$recycle.bin" if rest.len() <= 1 => {
            Some(Guard::Blocked("Papierkorb-Struktur – bitte den Papierkorb über Windows leeren."))
        }
        "users" if rest.len() == 1 => {
            Some(Guard::Blocked("Benutzerprofil – kann hier nicht gelöscht werden (Einstellungen → Konten)."))
        }
        "windows" => {
            let sub = rest.first().copied().unwrap_or_default();
            if CORE_WINDOWS_DIRS.contains(&sub) {
                Some(Guard::Blocked("Kernbestandteil von Windows – Löschen würde Windows beschädigen."))
            } else if WINDOWS_CLEANABLE.contains(&sub) {
                None
            } else {
                Some(Guard::Warn(WARN_SYSTEM))
            }
        }
        "program files" | "program files (x86)" | "programdata" => Some(Guard::Warn(WARN_PROGRAMS)),
        _ => None,
    }
}

/// Guard for a node of a scanned tree.
pub fn guard(tree: &Tree, id: NodeId) -> Option<Guard> {
    if id == Tree::ROOT {
        return Some(Guard::Blocked("Der Ausgangsordner des Scans kann nicht gelöscht werden."));
    }
    if tree.get(id)?.has(flags::DELETED) {
        return Some(Guard::Blocked("Wurde bereits entfernt."));
    }
    let path = tree.path(id).to_lowercase();
    let without_prefix = path.strip_prefix(r"\\?\").unwrap_or(&path);
    let mut parts = without_prefix.split('\\').filter(|p| !p.is_empty());
    // Skip the drive ("c:") or UNC server + share.
    if without_prefix.starts_with(r"\\") {
        parts.next();
        parts.next();
    } else {
        parts.next();
    }
    let comps: Vec<&str> = parts.collect();
    guard_for_components(&comps)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(path: &str) -> Option<Guard> {
        let lower = path.to_lowercase();
        let comps: Vec<&str> = lower.split('\\').filter(|p| !p.is_empty()).collect();
        guard_for_components(&comps)
    }

    fn blocked(path: &str) -> bool {
        matches!(g(path), Some(Guard::Blocked(_)))
    }

    fn warned(path: &str) -> bool {
        matches!(g(path), Some(Guard::Warn(_)))
    }

    #[test]
    fn blocks_system_locations() {
        for p in [
            "Windows",
            "Windows\\System32",
            "Windows\\System32\\drivers\\etc\\hosts",
            "Windows\\WinSxS\\amd64_foo",
            "Program Files",
            "Users",
            "Users\\Joe",
            "pagefile.sys",
            "hiberfil.sys",
            "$MFT",
            "$Extend\\$UsnJrnl",
            "System Volume Information",
            "$Recycle.Bin",
            "$Recycle.Bin\\S-1-5-21",
        ] {
            assert!(blocked(p), "{p} must be blocked");
        }
    }

    #[test]
    fn warns_for_installed_software_and_windows_internals() {
        for p in ["Program Files\\Foo", "ProgramData\\Bar\\cache.bin", "Windows\\Installer\\x.msi"] {
            assert!(warned(p), "{p} must warn");
        }
    }

    #[test]
    fn allows_user_data_and_known_junk() {
        for p in [
            "Users\\Joe\\Downloads",
            "Users\\Joe\\AppData\\Local\\Temp\\x.tmp",
            "Windows\\Temp\\foo.log",
            "Windows\\SoftwareDistribution\\Download",
            "VHDs\\Development.vhdx",
            "$Recycle.Bin\\S-1-5-21\\$R123.zip",
            "Games",
        ] {
            assert_eq!(g(p), None, "{p} must be allowed");
        }
    }

    #[test]
    fn guards_tree_nodes_by_full_path() {
        use crate::builder::{RawEntry, TreeBuilder};
        let names = b"WindowsDownloads";
        let e =
            |off: u32, len: u16| RawEntry { name_off: off, name_len: len, flags: flags::DIR, ..RawEntry::default() };
        let mut b = TreeBuilder::new("C:\\");
        b.add_listing(0, &[e(0, 7), e(7, 9)], names, |_| {}).unwrap();
        let t = b.finish();
        assert!(matches!(guard(&t, 0), Some(Guard::Blocked(_))));
        assert!(matches!(guard(&t, 1), Some(Guard::Blocked(_))));
        assert_eq!(guard(&t, 2), None);
    }
}
