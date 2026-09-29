# Entwicklung

Wie an SpaceLens gearbeitet, gebaut und getestet wird – inklusive aller
Stolperfallen, die bisher aufgetreten sind.

## Bauen

**Maßgeblich ist die CI** (`.github/workflows/ci.yml`, windows-latest, MSVC):
jeder Push baut `SpaceLens.exe` + `spacelens-cli.exe` + `SpaceLens.pdb` (Artefakt
im Actions-Tab) und prüft fmt, clippy `-D warnings` und die Tests von core, win,
cli und settings. Ein Tag `v*` erzeugt ein GitHub-Release (`release.yml`).

Lokal beim Nutzer ist **kein** Rust/MSVC installiert. Für KI-Sitzungen gibt es eine
isolierte GNU-Toolchain (nichts am System verändert):

| Teil | Ort | Hinweis |
|---|---|---|
| rustup + stable-x86_64-pc-windows-gnu | Scratchpad der Sitzung | `%TEMP%` wird bereinigt → ggf. neu installieren |
| Cargo-Paketcache (`CARGO_HOME`) | `target/cargo-home` | liegt bewusst nicht in `%TEMP%` |
| `dlltool.exe`, `as.exe` | aus w64devkit, Scratchpad `bin/` | nötig für `raw-dylib` von windows-sys |
| `libshlwapi.a` | aus w64devkit, per `-L native=…` | fehlt in rust-mingw |

Nur `dlltool`/`as` aus w64devkit verwenden, **nicht** dessen `gcc` (inkompatibel,
`-lgcc_eh` fehlt). `llvm-ar` als dlltool-Ersatz erzeugt kaputte Import-Libs
(Absturz vor `main`).

## Testen

- `cargo test -p spacelens-core -p spacelens-win -p spacelens-cli -p spacelens-settings`
- Manuelle Messungen (ignorierte Tests, echtes C:):
  `cargo test --release -p spacelens-win --test treemap_probe -- --ignored --nocapture`
- MFT-Scanner/Admin-Pfade: nur mit UAC-Freigabe durch den Nutzer (vorher ankündigen).

**Oberfläche prüfen (ohne den Nutzer zu stören):**

- Screenshots nur des eigenen Fensters mit `PrintWindow` (nie den Bildschirm – es
  sind schon private Inhalte mit aufgenommen worden).
- Bedienen über **UI Automation** (Tabellen-/Baumzeilen, Buttons, Reiter und das
  Suchfeld sind beschriftet und steuerbar).
- **Simulierte Mausnachrichten (`PostMessage`) kommen bei winit nicht an** – Klicks
  und Ziehen lassen sich so nicht testen. Echte Maus nur mit Einverständnis.
- Tastatur per `PostMessage` kommt verzögert an; `WM_CLOSE` funktioniert.
- **Keine Aktion auslösen, die einen Dialog auf dem Bildschirm des Nutzers öffnet**,
  ohne es vorher anzukündigen (Rückfragen beim Löschen erscheinen bei ihm).
- Eigene Testinstanzen nur per PID beenden – der Nutzer hat oft selbst eine offen.

## Stolperfallen

- **Backslashes in Shell-Heredocs** gehen verloren (`'\\'` wird `'\'`). Code und
  Pfade mit Backslashes immer mit den Datei-Werkzeugen schreiben.
- **Commits sind per 1Password (SSH) signiert.** Ist 1Password gesperrt:
  „1Password: failed to fill whole buffer“ → Nutzer bitten zu entsperren. Signatur
  nie umgehen.
- **Slint-Layout:** keine Größen an `root.width/height` binden (Layout-Schleifen);
  Elemente mit `stretch: 0` schrumpfen nie – für „wächst bis X, schrumpft bis Y“
  `min/preferred/max` + `stretch > 0` verwenden. Fluent-Scrollbars liegen über dem
  Inhalt.
- **Elemente in `if`** sind von außen nicht ansprechbar – für Referenzen `visible`
  oder einen umgebenden Container nutzen.
- **PowerShell `Out-File -Encoding utf8`** schreibt ein BOM (der Einstellungslader
  akzeptiert es).
- CI-Tests der App-Crate würden Slint doppelt bauen – testbare Logik gehört in
  core/win/settings.

## Konventionen

- Code/Kommentare Englisch, UI-Texte Deutsch; Zahlen wie im Explorer.
- Jede Änderung: fmt, clippy `-D warnings`, Tests; Performance mit Zahlen belegen.
- Commit-Nachrichten: Was + Warum, gemessene Zahlen; `Co-Authored-By`-Zeile.
- Neue Funktionen bekommen einen geplanten Platz im Layout (Reiter, Leiste, Menü),
  und werden auch bei 900×560 geprüft.
- `docs/STATUS.md` und `CHANGELOG.md` am Ende jeder größeren Arbeit aktualisieren.
