//! Catalogue of well-known, safely removable data ("junk") and its
//! evaluation against a scanned tree.
//!
//! Evaluation needs no disk access: locations are looked up in the in-memory
//! tree, so the cleanup view opens instantly and shows exact sizes. Only
//! locations contained in the scan appear (scan `C:\` to see them all).

use crate::tree::{NodeId, Tree, flags, names_equal};

/// Well-known folders a target is relative to. Resolved by the platform
/// layer (e.g. via `SHGetKnownFolderPath`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Base {
    /// `%LOCALAPPDATA%`, e.g. `C:\Users\Joe\AppData\Local`
    LocalAppData,
    /// `%USERPROFILE%`, e.g. `C:\Users\Joe`
    Profile,
    /// `%WINDIR%`, e.g. `C:\Windows`
    Windows,
    /// `%PROGRAMDATA%`, e.g. `C:\ProgramData`
    ProgramData,
    /// Root of the system drive, e.g. `C:\`
    SystemDrive,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    /// Rebuilt automatically; removing it has no lasting effect.
    Safe,
    /// Removable, but with a noticeable consequence (re-download, no rollback, …).
    Caution,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Remove what is *inside* the matched folder; the folder itself stays,
    /// because programs expect it to exist.
    Contents,
    /// Remove the matched file or folder itself.
    Item,
    /// Only show it (removal must happen through Windows' own tools).
    InfoOnly,
}

/// One location of a rule. `pattern` is relative to `base`, separated by
/// `\`; a component may contain one `*` wildcard (e.g. `*` or `Brave-*`).
#[derive(Clone, Copy, Debug)]
pub struct Target {
    pub base: Base,
    pub pattern: &'static str,
    pub mode: Mode,
}

#[derive(Clone, Copy, Debug)]
pub struct Rule {
    pub title: &'static str,
    pub description: &'static str,
    pub level: Level,
    /// Usually only readable/removable with administrator rights.
    pub needs_admin: bool,
    pub targets: &'static [Target],
}

const fn contents(base: Base, pattern: &'static str) -> Target {
    Target { base, pattern, mode: Mode::Contents }
}

const fn item(base: Base, pattern: &'static str) -> Target {
    Target { base, pattern, mode: Mode::Item }
}

use Base::*;

pub const RULES: &[Rule] = &[
    Rule {
        title: "Temporäre Dateien (Benutzer)",
        description: "Zwischendateien von Programmen und Installern, die oft liegen bleiben. Dateien, die gerade verwendet werden, werden übersprungen.",
        level: Level::Safe,
        needs_admin: false,
        targets: &[contents(LocalAppData, "Temp")],
    },
    Rule {
        title: "Temporäre Dateien (Windows)",
        description: "Zwischendateien von Windows-Diensten und Setups. Benutzte Dateien werden übersprungen.",
        level: Level::Safe,
        needs_admin: true,
        targets: &[contents(Windows, "Temp")],
    },
    Rule {
        title: "Windows-Update-Downloads",
        description: "Heruntergeladene, bereits installierte Update-Pakete. Windows lädt bei Bedarf neu herunter.",
        level: Level::Safe,
        needs_admin: true,
        targets: &[contents(Windows, r"SoftwareDistribution\Download")],
    },
    Rule {
        title: "Übermittlungsoptimierung",
        description: "Zwischenspeicher, mit dem Windows Updates an andere PCs weitergibt. Wird bei Bedarf neu gefüllt.",
        level: Level::Safe,
        needs_admin: true,
        targets: &[contents(
            Windows,
            r"ServiceProfiles\NetworkService\AppData\Local\Microsoft\Windows\DeliveryOptimization\Cache",
        )],
    },
    Rule {
        title: "Fehlerberichte",
        description: "Gesammelte Windows-Fehlerberichte (WER), die bereits gesendet wurden oder nicht mehr gebraucht werden.",
        level: Level::Safe,
        needs_admin: true,
        targets: &[
            contents(ProgramData, r"Microsoft\Windows\WER\ReportArchive"),
            contents(ProgramData, r"Microsoft\Windows\WER\ReportQueue"),
            contents(LocalAppData, r"Microsoft\Windows\WER\ReportArchive"),
            contents(LocalAppData, r"Microsoft\Windows\WER\ReportQueue"),
        ],
    },
    Rule {
        title: "Absturzabbilder",
        description: "Speicherabbilder nach Programm- oder Systemabstürzen. Nur für die Fehlersuche durch Entwickler nützlich.",
        level: Level::Safe,
        needs_admin: false,
        targets: &[
            contents(LocalAppData, "CrashDumps"),
            contents(Windows, "Minidump"),
            item(Windows, "MEMORY.DMP"),
            contents(Windows, "LiveKernelReports"),
        ],
    },
    Rule {
        title: "Browser-Cache",
        description: "Zwischengespeicherte Webseiten-Inhalte von Chrome, Edge, Brave und Firefox. Anmeldungen, Passwörter und Lesezeichen bleiben erhalten. Am besten den Browser vorher schließen.",
        level: Level::Safe,
        needs_admin: false,
        targets: &[
            contents(LocalAppData, r"Google\Chrome\User Data\*\Cache"),
            contents(LocalAppData, r"Google\Chrome\User Data\*\Code Cache"),
            contents(LocalAppData, r"Google\Chrome\User Data\*\GPUCache"),
            contents(LocalAppData, r"Microsoft\Edge\User Data\*\Cache"),
            contents(LocalAppData, r"Microsoft\Edge\User Data\*\Code Cache"),
            contents(LocalAppData, r"Microsoft\Edge\User Data\*\GPUCache"),
            contents(LocalAppData, r"BraveSoftware\*\User Data\*\Cache"),
            contents(LocalAppData, r"BraveSoftware\*\User Data\*\Code Cache"),
            contents(LocalAppData, r"BraveSoftware\*\User Data\*\GPUCache"),
            contents(LocalAppData, r"Mozilla\Firefox\Profiles\*\cache2"),
        ],
    },
    Rule {
        title: "Shader-Cache (Grafik)",
        description: "Vorübersetzte Grafik-Shader von Treibern und Spielen. Werden automatisch neu erzeugt; der erste Spielstart danach kann kurz ruckeln.",
        level: Level::Safe,
        needs_admin: false,
        targets: &[
            contents(LocalAppData, "D3DSCache"),
            contents(LocalAppData, r"NVIDIA\DXCache"),
            contents(LocalAppData, r"NVIDIA\GLCache"),
            contents(LocalAppData, r"AMD\DxCache"),
            contents(LocalAppData, r"AMD\DxcCache"),
            contents(LocalAppData, r"AMD\GLCache"),
            contents(LocalAppData, r"Intel\ShaderCache"),
        ],
    },
    Rule {
        title: "Entwickler-Caches",
        description: "Heruntergeladene Pakete von npm, NuGet, pip, Gradle und Cargo. Werden beim nächsten Build erneut heruntergeladen (Internet nötig, dauert länger).",
        level: Level::Caution,
        needs_admin: false,
        targets: &[
            contents(LocalAppData, "npm-cache"),
            contents(Profile, r".nuget\packages"),
            contents(LocalAppData, r"pip\Cache"),
            contents(Profile, r".gradle\caches"),
            contents(Profile, r".cargo\registry\cache"),
        ],
    },
    Rule {
        title: "Alte Windows-Installation",
        description: "Reste nach einem Windows-Upgrade (Windows.old). Danach ist keine Rückkehr zur alten Version mehr möglich. Entfernen über Einstellungen → System → Speicher → Temporäre Dateien.",
        level: Level::Caution,
        needs_admin: true,
        targets: &[
            Target { base: SystemDrive, pattern: "Windows.old", mode: Mode::InfoOnly },
            Target { base: SystemDrive, pattern: "$Windows.~BT", mode: Mode::InfoOnly },
        ],
    },
];

/// A rule that matched something in the tree.
#[derive(Clone, Debug)]
pub struct Finding {
    pub rule: &'static Rule,
    /// Folders/files the rule matched (e.g. every browser profile's cache).
    pub locations: Vec<NodeId>,
    /// What would be removed: the contents of `Contents` locations and the
    /// `Item` locations themselves. Empty for info-only rules.
    pub targets: Vec<NodeId>,
    pub size: u64,
    pub alloc: u64,
    pub files: u64,
}

impl Finding {
    pub fn is_removable(&self) -> bool {
        !self.targets.is_empty()
    }
}

/// Evaluates all rules. `resolve` maps a [`Base`] to its absolute path.
/// Findings are sorted by size, largest first; empty ones are omitted.
pub fn evaluate(tree: &Tree, resolve: impl Fn(Base) -> Option<String>) -> Vec<Finding> {
    let mut out: Vec<Finding> = RULES
        .iter()
        .filter_map(|rule| {
            let mut f = Finding { rule, locations: Vec::new(), targets: Vec::new(), size: 0, alloc: 0, files: 0 };
            for t in rule.targets {
                let Some(base) = resolve(t.base).and_then(|p| tree.find_path(&p)) else { continue };
                for loc in match_pattern(tree, base, t.pattern) {
                    let node = tree.node(loc);
                    if node.has(flags::NOT_FOLLOWED) || f.locations.contains(&loc) {
                        continue;
                    }
                    f.locations.push(loc);
                    f.size += node.size;
                    f.alloc += node.alloc;
                    f.files += if node.is_dir() { u64::from(tree.counts(loc).0) } else { 1 };
                    match t.mode {
                        Mode::Contents => f.targets.extend(tree.live_children(loc)),
                        Mode::Item => f.targets.push(loc),
                        Mode::InfoOnly => {}
                    }
                }
            }
            (f.size > 0).then_some(f)
        })
        .collect();
    out.sort_by_key(|f| std::cmp::Reverse(f.size));
    out
}

/// All live nodes below `base` matching `pattern` (see [`Target`]).
pub fn match_pattern(tree: &Tree, base: NodeId, pattern: &str) -> Vec<NodeId> {
    let mut current = vec![base];
    for part in pattern.split('\\').filter(|p| !p.is_empty()) {
        let mut next = Vec::new();
        for &dir in &current {
            next.extend(tree.live_children(dir).filter(|&c| component_matches(&tree.name(c), part)));
        }
        if next.is_empty() {
            return next;
        }
        current = next;
    }
    current
}

/// Case-insensitive match of one path component with an optional single `*`.
fn component_matches(name: &str, pattern: &str) -> bool {
    match pattern.split_once('*') {
        None => names_equal(name, pattern),
        Some((prefix, suffix)) => {
            let lower = name.to_lowercase();
            lower.len() >= prefix.len() + suffix.len()
                && lower.starts_with(&prefix.to_lowercase())
                && lower.ends_with(&suffix.to_lowercase())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builder::{RawEntry, TreeBuilder};

    /// Builds a tree from `\`-separated file paths with sizes (dirs implied).
    fn tree_of(files: &[(&str, u64)]) -> Tree {
        use std::collections::BTreeMap;
        #[derive(Default)]
        struct Dir {
            dirs: BTreeMap<String, Dir>,
            files: Vec<(String, u64)>,
        }
        let mut root = Dir::default();
        for &(path, size) in files {
            let parts: Vec<&str> = path.split('\\').collect();
            let mut d = &mut root;
            for p in &parts[..parts.len() - 1] {
                d = d.dirs.entry((*p).to_string()).or_default();
            }
            d.files.push((parts[parts.len() - 1].to_string(), size));
        }
        let mut b = TreeBuilder::new("C:\\");
        let mut queue = vec![(0u32, &root)];
        while let Some((id, dir)) = queue.pop() {
            let mut names = Vec::new();
            let mut entries = Vec::new();
            let mut subdirs = Vec::new();
            for (name, sub) in &dir.dirs {
                let off = names.len() as u32;
                names.extend_from_slice(name.as_bytes());
                entries.push(RawEntry {
                    name_off: off,
                    name_len: name.len() as u16,
                    flags: flags::DIR,
                    ..RawEntry::default()
                });
                subdirs.push(sub);
            }
            for (name, size) in &dir.files {
                let off = names.len() as u32;
                names.extend_from_slice(name.as_bytes());
                entries.push(RawEntry {
                    size: *size,
                    alloc: *size,
                    name_off: off,
                    name_len: name.len() as u16,
                    ..RawEntry::default()
                });
            }
            let mut it = subdirs.into_iter();
            b.add_listing(id, &entries, &names, |child| {
                if let Some(s) = it.next() {
                    queue.push((child, s));
                }
            })
            .unwrap();
        }
        b.finish()
    }

    fn resolve(base: Base) -> Option<String> {
        Some(
            match base {
                Base::LocalAppData => r"C:\Users\Joe\AppData\Local",
                Base::Profile => r"C:\Users\Joe",
                Base::Windows => r"C:\Windows",
                Base::ProgramData => r"C:\ProgramData",
                Base::SystemDrive => r"C:\",
            }
            .to_string(),
        )
    }

    #[test]
    fn wildcards_match_components() {
        assert!(component_matches("Default", "*"));
        assert!(component_matches("Brave-Browser", "brave-*"));
        assert!(component_matches("cache2", "CACHE2"));
        assert!(!component_matches("Brave", "Brave-*"));
        assert!(component_matches("thumbcache_96.db", "thumbcache_*.db"));
    }

    #[test]
    fn finds_junk_across_profiles_and_sorts_by_size() {
        let t = tree_of(&[
            (r"Users\Joe\AppData\Local\Temp\a.tmp", 100),
            (r"Users\Joe\AppData\Local\Temp\sub\b.tmp", 50),
            (r"Users\Joe\AppData\Local\Google\Chrome\User Data\Default\Cache\Cache_Data\x", 400),
            (r"Users\Joe\AppData\Local\Google\Chrome\User Data\Profile 1\Cache\Cache_Data\y", 600),
            (r"Users\Joe\AppData\Local\Google\Chrome\User Data\Default\Bookmarks", 7),
            (r"Windows\MEMORY.DMP", 30),
            (r"Windows.old\Windows\big", 5000),
            (r"Users\Joe\Documents\keep.docx", 999),
        ]);
        let found = evaluate(&t, resolve);
        let titles: Vec<&str> = found.iter().map(|f| f.rule.title).collect();
        assert_eq!(
            titles,
            vec!["Alte Windows-Installation", "Browser-Cache", "Temporäre Dateien (Benutzer)", "Absturzabbilder"]
        );

        let browser = &found[1];
        assert_eq!(browser.size, 1000, "both profiles, bookmarks excluded");
        assert_eq!(browser.locations.len(), 2);
        assert_eq!(browser.targets.len(), 2, "the Cache_Data folders inside each Cache");

        let temp = &found[2];
        assert_eq!((temp.size, temp.files), (150, 2));
        assert_eq!(temp.targets.len(), 2, "a.tmp and sub, but not the Temp folder itself");

        let dump = &found[3];
        assert_eq!(dump.targets, dump.locations, "item mode removes the file itself");

        assert!(!found[0].is_removable(), "Windows.old is info only");
    }

    #[test]
    fn missing_bases_and_empty_rules_are_skipped() {
        let t = tree_of(&[(r"Data\x.bin", 10)]);
        assert!(evaluate(&t, resolve).is_empty());
        assert!(evaluate(&t, |_| None).is_empty());
    }
}
