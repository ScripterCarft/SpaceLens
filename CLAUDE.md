# SpaceLens – Projektleitfaden

Hochperformanter Disk-Analyzer und Aufräum-Werkzeug für Windows. Rust-Backend,
native Slint-Oberfläche, kein WebView. Ziel: professionelles Werkzeug, schnell,
sparsam, stabil.

## Hier anfangen (neue Sitzung)

1. **`docs/STATUS.md`** – was fertig ist, was ungetestet ist, gemeldete Probleme,
   nächste Schritte. **Immer zuerst lesen.**
2. **`docs/DEVELOPMENT.md`** – bauen, testen, UI prüfen, alle bekannten Stolperfallen.
3. `docs/ARCHITECTURE.md` – Aufbau, Invarianten, Messwerte. `docs/ROADMAP.md`, `CHANGELOG.md`.
4. Bei Absturz-/Leistungsproblemen: Logs aus `%LOCALAPPDATA%\SpaceLens\logs`
   (`crash-*.txt`, `spacelens.log` mit `langsam: …`-Zeilen) anfordern.

## Arbeitsweise mit dem Nutzer

- Der Nutzer ist kein Entwickler und schreibt Deutsch. Vor und während der Arbeit in
  einfacher Sprache erklären: **was kommt, wie es in die App eingebaut wird, warum.**
- Er erwartet ein professionelles Werkzeug: Funktionen bekommen einen geplanten Platz
  im Layout, das Grundlegende (Laufwerk → Scannen → Sehen) bleibt schnell, alles
  funktioniert auch in kleinen Fenstern (900×560).
- Stabilität vor neuen Funktionen. Ehrlich berichten, was getestet ist und was nicht.
- Nichts auslösen, was auf seinem Bildschirm Dialoge öffnet oder seine Maus bewegt,
  ohne es anzukündigen; UAC-Abfragen vorher ankündigen.

## Aufbau

```
crates/
  core/     spacelens-core      Datenmodell (Tree), Builder, Aggregation, Abfragen, Suche,
                                Duplikat-Kandidaten, Aufräum-Katalog, Schutzregeln, Treemap.
                                Plattformunabhängig, #![forbid(unsafe_code)], keine I/O.
  win/      spacelens-win       Windows-Schicht: Scanner (mft/ = NTFS-MFT, walker),
                                Laufwerke, UAC, Papierkorb, Duplikat-Hashing, bekannte Ordner.
                                Einziger Ort mit `unsafe` (Win32 über windows-sys/windows).
  settings/ spacelens-settings  settings.json – versioniert, vorwärts-/rückwärtskompatibel.
  cli/      spacelens-cli       Headless-Scanner & Benchmark.
  app/      spacelens           Slint-GUI (SpaceLens.exe):
              ui/*.slint          app.slint (Fenster), table/tree/treemap/cleanup, widgets
              src/main.rs         Start, Einstellungen laden, Panic-Hook
              src/diag.rs         Crash-Berichte, Log langsamer UI-Aktionen
              src/models.rs       lazy Slint-Modelle (Baum, Tabelle)
              src/app/            Controller, ein Modul pro Bereich (scan, navigation,
                                  delete, search_view, duplicates_view, cleanup_view,
                                  treemap, settings, lifecycle, drives)
docs/       STATUS, DEVELOPMENT, ARCHITECTURE, ROADMAP
```

Neue UI-Bereiche: eigenes Modul unter `app/src/app/`, testbare Logik in `core`/`win`,
UI-Aktionen mit `crate::diag::Timed` messen.

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
- Neue Aufräum-Regeln (`core::cleanup::RULES`) nur mit ehrlicher Erklärung der Folgen
  und korrekter Einstufung; im Zweifel `Level::Caution` oder `Mode::InfoOnly`.
- Code mit Backslashes (Pfade) nie per Shell-Heredoc einfügen – die Shell verschluckt
  sie; Datei-Werkzeuge verwenden.
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
