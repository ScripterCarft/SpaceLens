//! Persistent user settings (`%APPDATA%\SpaceLens\settings.json`).
//!
//! Compatibility rules – so that old and new versions never break each other:
//!
//! 1. `schema` versions the file; `migrate` upgrades older layouts.
//! 2. Every field has a default (`#[serde(default)]`): newer versions read
//!    files written by older ones.
//! 3. Unknown fields are kept (`extra`, flattened) and written back: an older
//!    version saving a newer file does not destroy the newer settings. The
//!    `schema` number never goes down for the same reason.
//! 4. Values use stable names (`"size"`, bytes) instead of UI indices.
//! 5. Every value is range-checked after loading (`sanitize`).
//! 6. A corrupt file is moved aside (`settings.json.corrupt`), never fatal.
//! 7. Saving writes a temporary file and renames it over the old one, so an
//!    interrupted save never leaves a half-written file.

use std::fs;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Current layout of the file.
pub const SCHEMA: u32 = 1;
/// File name inside the settings directory (`%APPDATA%SpaceLens`).
pub const FILE: &str = "settings.json";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub schema: u32,
    pub window: WindowSettings,
    pub layout: LayoutSettings,
    pub table: TableSettings,
    pub scan: ScanSettings,
    pub duplicates: DuplicateSettings,
    /// Fields written by newer versions; preserved verbatim.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowSettings {
    /// Size of the restored (not maximized) window in logical pixels.
    pub width: f32,
    pub height: f32,
    pub maximized: bool,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LayoutSettings {
    pub tree_width: f32,
    pub treemap_height: f32,
    pub treemap_visible: bool,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TableSettings {
    /// Stable column key: name, size, alloc, share, files, modified.
    pub sort_column: String,
    pub sort_descending: bool,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ScanSettings {
    /// Last scanned path; pre-filled on start.
    pub last_path: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DuplicateSettings {
    /// Minimum file size in bytes.
    pub min_size: u64,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            schema: SCHEMA,
            window: WindowSettings::default(),
            layout: LayoutSettings::default(),
            table: TableSettings::default(),
            scan: ScanSettings::default(),
            duplicates: DuplicateSettings::default(),
            extra: Map::new(),
        }
    }
}

impl Default for WindowSettings {
    fn default() -> Self {
        Self { width: 1280.0, height: 800.0, maximized: false, extra: Map::new() }
    }
}

impl Default for LayoutSettings {
    fn default() -> Self {
        Self { tree_width: 320.0, treemap_height: 280.0, treemap_visible: true, extra: Map::new() }
    }
}

impl Default for TableSettings {
    fn default() -> Self {
        Self { sort_column: "size".into(), sort_descending: true, extra: Map::new() }
    }
}

impl Default for ScanSettings {
    fn default() -> Self {
        Self { last_path: String::new(), extra: Map::new() }
    }
}

impl Default for DuplicateSettings {
    fn default() -> Self {
        Self { min_size: 1 << 20, extra: Map::new() }
    }
}

/// Outcome of loading, for a status message.
#[derive(Debug, PartialEq, Eq)]
pub enum LoadNote {
    Loaded,
    Missing,
    /// The file could not be parsed and was moved aside.
    Corrupt,
}

impl Settings {
    /// Loads settings; never fails (falls back to defaults).
    pub fn load(path: &Path) -> (Self, LoadNote) {
        let text = match fs::read_to_string(path) {
            Ok(t) => t,
            Err(_) => return (Self::default(), LoadNote::Missing),
        };
        // Windows editors (Notepad, PowerShell) like to prepend a UTF-8 BOM.
        let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
        match serde_json::from_str::<Value>(text) {
            Ok(value) => {
                let value = migrate(value);
                match serde_json::from_value::<Settings>(value) {
                    Ok(mut s) => {
                        s.sanitize();
                        (s, LoadNote::Loaded)
                    }
                    Err(_) => (set_aside(path), LoadNote::Corrupt),
                }
            }
            Err(_) => (set_aside(path), LoadNote::Corrupt),
        }
    }

    /// Writes atomically (temporary file + rename).
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let mut out = self.clone();
        out.schema = out.schema.max(SCHEMA);
        let json = serde_json::to_string_pretty(&out).map_err(io::Error::other)?;
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, json)?;
        fs::rename(&tmp, path)
    }

    /// Clamps every value into a sane range.
    pub fn sanitize(&mut self) {
        let finite_or = |v: f32, d: f32| if v.is_finite() { v } else { d };
        self.window.width = finite_or(self.window.width, 1280.0).clamp(900.0, 10_000.0);
        self.window.height = finite_or(self.window.height, 800.0).clamp(540.0, 10_000.0);
        self.layout.tree_width = finite_or(self.layout.tree_width, 320.0).clamp(190.0, 2_000.0);
        self.layout.treemap_height = finite_or(self.layout.treemap_height, 280.0).clamp(100.0, 3_000.0);
        const COLUMNS: [&str; 6] = ["name", "size", "alloc", "share", "files", "modified"];
        if !COLUMNS.contains(&self.table.sort_column.as_str()) {
            self.table.sort_column = "size".into();
        }
        self.duplicates.min_size = self.duplicates.min_size.clamp(1, 1 << 40);
        if self.scan.last_path.len() > 32_767 {
            self.scan.last_path.clear();
        }
    }
}

/// Upgrades older file layouts to [`SCHEMA`]. Schema 1 is the first layout,
/// so there is nothing to migrate yet; future versions add steps here, e.g.
/// `if schema < 2 { rename field … }`.
fn migrate(value: Value) -> Value {
    value
}

/// Moves an unreadable file out of the way and returns defaults.
fn set_aside(path: &Path) -> Settings {
    let _ = fs::rename(path, path.with_extension("json.corrupt"));
    Settings::default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_file(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("spacelens-settings-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir.join(FILE)
    }

    #[test]
    fn roundtrip_and_missing_file() {
        let path = temp_file("roundtrip");
        let (s, note) = Settings::load(&path);
        assert_eq!(note, LoadNote::Missing);
        assert_eq!(s, Settings::default());

        let mut s = Settings::default();
        s.layout.tree_width = 400.0;
        s.table.sort_column = "modified".into();
        s.scan.last_path = r"D:\Daten".into();
        s.save(&path).unwrap();
        let (loaded, note) = Settings::load(&path);
        assert_eq!(note, LoadNote::Loaded);
        assert_eq!(loaded, s);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn older_file_gets_defaults_for_new_fields() {
        let path = temp_file("older");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, r#"{ "schema": 1, "layout": { "tree_width": 250 } }"#).unwrap();
        let (s, _) = Settings::load(&path);
        assert_eq!(s.layout.tree_width, 250.0);
        assert!(s.layout.treemap_visible, "missing field -> default");
        assert_eq!(s.table.sort_column, "size");
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn newer_fields_survive_a_save_by_an_older_version() {
        let path = temp_file("newer");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, r#"{ "schema": 7, "theme": "dark", "layout": { "tree_width": 300, "columns": [1, 2] } }"#)
            .unwrap();
        let (s, _) = Settings::load(&path);
        s.save(&path).unwrap();
        let v: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["schema"], 7, "schema never goes down");
        assert_eq!(v["theme"], "dark");
        assert_eq!(v["layout"]["columns"], serde_json::json!([1, 2]));
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn utf8_bom_from_windows_editors_is_accepted() {
        let path = temp_file("bom");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "\u{feff}{ \"layout\": { \"tree_width\": 450 } }").unwrap();
        let (s, note) = Settings::load(&path);
        assert_eq!(note, LoadNote::Loaded);
        assert_eq!(s.layout.tree_width, 450.0);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn corrupt_file_is_moved_aside() {
        let path = temp_file("corrupt");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{ this is not json").unwrap();
        let (s, note) = Settings::load(&path);
        assert_eq!(note, LoadNote::Corrupt);
        assert_eq!(s, Settings::default());
        assert!(!path.exists());
        assert!(path.with_extension("json.corrupt").exists());
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn nonsense_values_are_clamped() {
        let path = temp_file("clamp");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"{ "window": { "width": 5, "height": 99999 }, "table": { "sort_column": "bogus" },
                 "layout": { "tree_width": -3 } }"#,
        )
        .unwrap();
        let (s, _) = Settings::load(&path);
        assert_eq!((s.window.width, s.window.height), (900.0, 10_000.0));
        assert_eq!(s.layout.tree_width, 190.0);
        assert_eq!(s.table.sort_column, "size");
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn wrong_types_count_as_corrupt_not_crash() {
        let path = temp_file("types");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, r#"{ "window": { "width": "wide" } }"#).unwrap();
        let (s, note) = Settings::load(&path);
        assert_eq!(note, LoadNote::Corrupt);
        assert_eq!(s, Settings::default());
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }
}
