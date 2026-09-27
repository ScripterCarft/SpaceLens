# SpaceLens – Projektleitfaden

Hochperformanter Disk-Analyzer für Windows (später: Aufräum-Tool). Rust-Backend,
native Slint-Oberfläche, kein WebView. Ziel: professionelles Werkzeug, schnell,
sparsam, stabil.

## Aufbau

```
crates/
  core/   spacelens-core  Datenmodell (Tree), Builder, Aggregation, Abfragen, Formatierung.
                          Plattformunabhängig, #![forbid(unsafe_code)], keine I/O.
  win/    spacelens-win   Windows-Schicht: Scanner (mft/ = NTFS-MFT, walker = Verzeichnisse),
                          Laufwerke, UAC/Rechte, Explorer.
                          Einziger Ort mit `unsafe` (Win32 über windows-sys).
  cli/    spacelens-cli   Headless-Scanner & Benchmark.
  app/    spacelens       Slint-GUI (SpaceLens.exe). ui/*.slint + src/{main,app,models}.rs
docs/     ARCHITECTURE.md (Details & Invarianten), ROADMAP.md
```

Abhängigkeitsrichtung: `app`/`cli` → `win` → `core`. `core` kennt niemanden.
Neue Scan-Methoden (z. B. MFT) implementieren `spacelens_core::Scanner` in `win`
und werden in `spacelens_win::scanner_for` ausgewählt.

## Befehle

```sh
cargo build --release                          # SpaceLens.exe + spacelens-cli.exe
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
target/release/spacelens-cli.exe C:\ --top 10   # Benchmark (Dauer, Einträge/s, Bytes/Eintrag)
target/release/spacelens-cli.exe C:\ --method mft # auto | walk | mft (mft braucht Admin)
SPACELENS_TRACE=1                               # Phasenzeiten des MFT-Scanners auf stderr
target/release/SpaceLens.exe --scan C:\
```

CI (`.github/workflows/ci.yml`) muss grün sein: fmt, clippy `-D warnings`, Tests,
Release-Build. Tags `v*` erzeugen ein GitHub-Release (`release.yml`).

## Harte Regeln

**Korrektheit & Sicherheit**
- Scanner sind **nur lesend** (Löschen ist eine eigene, explizite Nutzeraktion).
  Dateien werden nie geöffnet, nur Verzeichnisse gelistet (kein Sharing-Konflikt,
  kein Download von Cloud-Platzhaltern).
- Reparse-Points mit Name-Surrogate-Tag (Symlink, Junction, Mountpoint) werden
  **nie verfolgt** (Flag `NOT_FOLLOWED`). Cloud-Tags (OneDrive) sind keine Surrogates.
- Pfade für Win32 immer verbatim (`\\?\`), Namen verlustfrei (WTF-8 in `core::names`).
- MFT-Parser (`win/src/mft/record.rs`) bleibt frei von `unsafe`; jede Leseoperation
  bounds-geprüft, defekte Datensätze werden übersprungen.
- Fehler pro Verzeichnis werden gesammelt (`ScanIssue`), nie abbrechen.
- Kein `unwrap()` außerhalb von Tests (Clippy `unwrap_used = deny`). Worker fangen
  Panics pro Verzeichnis (`catch_unwind`).
- Jeder `unsafe`-Block hat einen `// SAFETY:`-Kommentar; Handles via `OwnedHandle`.
- Löschen **ausschließlich** in den Papierkorb (`recycle.rs`), nie ohne Rückfrage und
  nie an `core::safety` vorbei. Neue Schutzregeln immer mit Test.
- Adminrechte nur über den dokumentierten Weg: Manifest `asInvoker` +
  `ShellExecuteExW("runas")`. Keine UAC-Umgehungen, keine Dienste/Tasks.

**Performance & Speicher**
- Datenmodell-Invarianten (siehe `core/src/tree.rs`): `parent < id`, Kinder
  zusammenhängend, Aggregation = ein Rückwärtsdurchlauf. Nicht brechen.
- `Node` = 40 Bytes, `DirInfo` = 16 Bytes (per `const assert` geprüft). Neue Felder
  nur mit Begründung; lieber in Seitentabellen auslagern.
- Keine Allokation pro Datei im Hot Path; Worker verwenden Puffer wieder.
- UI: Modelle speichern nur `NodeId`s und formatieren in `row_data` (nur sichtbare
  Zeilen). Nie den ganzen Baum in Slint-Modelle kopieren.
- Fortschritt über Atomics + UI-Timer (10 Hz), keine Events pro Datei.
- Performance-Änderungen mit `spacelens-cli` messen (Zahlen in PR/Commit nennen).

**Stil**
- Code, Bezeichner und Kommentare: Englisch. UI-Texte: Deutsch.
- Zahlenformat wie Explorer: 1024er-Einheiten „KB/MB/GB“, Dezimalkomma.
- Rendering: Standard ist Slints Software-Renderer (≈8 MB statt ≈80 MB RAM im
  Leerlauf, RDP/VM-fest). OpenGL nur über Feature `gpu`.
- Slint-Style `fluent` (folgt Hell/Dunkel von Windows). Farben nur über `Theme`
  (`ui/widgets.slint`) bzw. `Palette`.

## Lokales Bauen

Standard: Rust (rustup, MSVC-Toolchain) + „Visual Studio Build Tools“ mit
Workload „Desktopentwicklung mit C++“. Alternativ baut die CI bei jedem Push
die Exe (Artefakt im Actions-Tab).
