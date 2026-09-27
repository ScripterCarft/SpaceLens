//! Compiles the Slint UI and embeds the Windows resources (manifest, version info).

use std::path::PathBuf;
use std::{env, fs};

fn main() {
    let config = slint_build::CompilerConfiguration::new().with_style("fluent".into());
    slint_build::compile_with_config("ui/app.slint", config)
        .unwrap_or_else(|e| panic!("Slint-Kompilierung fehlgeschlagen: {e}"));

    embed_windows_resources();
}

/// Generates the `.rc` file (version from Cargo.toml) and links it in.
///
/// MSVC builds (CI, releases) *require* the resources so a published exe can
/// never ship without its manifest. Other toolchains (e.g. a local GNU setup
/// without `windres`) only warn.
fn embed_windows_resources() {
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap_or_default());
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap_or_default());
    let manifest = manifest_dir.join("res").join("SpaceLens.manifest");
    println!("cargo:rerun-if-changed={}", manifest.display());

    let version = env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".into());
    let mut parts: Vec<u16> = version.split(['.', '-', '+']).filter_map(|p| p.parse().ok()).take(3).collect();
    parts.resize(3, 0);
    let numeric = format!("{},{},{},0", parts[0], parts[1], parts[2]);

    let rc = format!(
        r#"1 24 "{manifest}"

1 VERSIONINFO
FILEVERSION {numeric}
PRODUCTVERSION {numeric}
FILEOS 0x40004
FILETYPE 0x1
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040704B0"
    BEGIN
      VALUE "FileDescription", "SpaceLens Disk Analyzer"
      VALUE "FileVersion", "{version}"
      VALUE "InternalName", "SpaceLens"
      VALUE "OriginalFilename", "SpaceLens.exe"
      VALUE "ProductName", "SpaceLens"
      VALUE "ProductVersion", "{version}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x407, 1200
  END
END
"#,
        manifest = manifest.display().to_string().replace('\\', "/"),
    );
    let rc_path = out_dir.join("spacelens.rc");
    fs::write(&rc_path, rc).unwrap_or_else(|e| panic!("{}: {e}", rc_path.display()));

    let result = embed_resource::compile(&rc_path, embed_resource::NONE);
    if env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        result.manifest_required().unwrap_or_else(|e| panic!("Ressourcen konnten nicht eingebettet werden: {e}"));
    } else if let Err(e) = result.manifest_optional() {
        println!("cargo:warning=Windows-Ressourcen nicht eingebettet: {e}");
    }
}
