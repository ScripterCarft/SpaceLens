# SpaceLens

Schneller, sparsamer Disk-Analyzer für Windows 10/11, geschrieben in Rust mit
nativer [Slint](https://slint.dev)-Oberfläche. Kein WebView, keine Installation:
eine einzelne `SpaceLens.exe`.

- Als Administrator: liest die NTFS-Master-File-Table direkt – C: mit 650 000
  Einträgen in ≈0,4 s, Hardlinks exakt einmal gezählt
- Als Standardbenutzer: paralleler Verzeichnis-Scan, ≈600 000 Einträge in unter 1 s
- ≈77 Bytes RAM pro Datei, ≈8 MB Grundverbrauch der Oberfläche
- Scannen ist rein lesend: öffnet keine Dateien, folgt keinen Junctions/Symlinks
  und lädt keine OneDrive-Dateien herunter
- Aufräumen nur über den Windows-Papierkorb (wiederherstellbar), immer mit
  Rückfrage; Windows- und Systembereiche sind gesperrt bzw. mit Warnung versehen
- Standardbenutzer oder Administrator (vollständiger Scan inklusive geschützter
  Ordner über die normale UAC-Abfrage)

## Herunterladen

- **Neueste Entwicklungsversion**: Repository → *Actions* → letzter Lauf von
  „CI“ → Artefakt `SpaceLens-<commit>` (ZIP mit `SpaceLens.exe` und
  `spacelens-cli.exe`).
- **Releases**: Repository → *Releases*. Ein Release entsteht automatisch, wenn
  ein Tag `v*` gepusht wird (`git tag v0.1.0 && git push --tags`).

Die Exe ist nicht signiert. Beim ersten Start kann Windows SmartScreen warnen
(„Weitere Informationen“ → „Trotzdem ausführen“).

## Bedienung

| Aktion | Maus | Tastatur |
|---|---|---|
| Scannen | Laufwerk wählen oder Pfad eingeben → **Scannen** | `F5`, `Enter` im Pfadfeld |
| Ordner öffnen | Doppelklick in der Tabelle | `Enter` |
| Übergeordneter Ordner | **↑** | `Backspace` |
| Baum auf-/zuklappen | Pfeil oder Doppelklick | `→` / `←` |
| Sortieren | Klick auf die Spaltenüberschrift | |
| Im Explorer zeigen | Rechtsklick → *Im Explorer anzeigen* | |
| Treemap | Maus darüber: Pfad + Größe · Klick: Eintrag markieren · Doppelklick: direkt hinspringen | |
| Treemap ein-/ausblenden | *Ansicht* → *Treemap ausblenden* | |
| Größte Dateien (aktueller Ordner) | **Größte Dateien** über der Tabelle · Doppelklick springt zum Ordner | |
| Mehrere auswählen | Strg + Klick (einzeln), Umschalt + Klick (Bereich) | `Strg+A`, `Umschalt+↑/↓` |
| In den Papierkorb (auch mehrere) | **In den Papierkorb …**, Rechtsklick oder *Bearbeiten* | `Entf` |
| Aufräumen (bekannte Müllecken, Papierkorb leeren) | **Aufräumen** über der Tabelle | |
| Suchen (Name, `*.iso`, mehrere Wörter) + Filter Typ/Größe/Alter | Suchfeld über der Tabelle | `Strg+F` |
| Duplikate finden (identischer Inhalt), Kopien auswählen | Reiter **Duplikate** → *Duplikate suchen* → *Kopien auswählen* → `Entf` | |

Kommandozeile: `SpaceLens.exe --scan D:\` startet direkt einen Scan.

Benchmark ohne Oberfläche:

```
spacelens-cli.exe C:\ --top 10 [--method auto|walk|mft] [--threads N] [--quiet]
```

## Selbst bauen

Voraussetzungen: [Rust](https://rustup.rs) (stable, MSVC) und die „Visual Studio
Build Tools“ mit der Workload „Desktopentwicklung mit C++“.

```
cargo build --release
target\release\SpaceLens.exe
```

Aufbau, Designentscheidungen und Invarianten stehen in
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md), die Planung in
[docs/ROADMAP.md](docs/ROADMAP.md).
