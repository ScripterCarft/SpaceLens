# Roadmap

## 0.1 – Grundgerüst ✅
- Paralleler Verzeichnis-Scanner (Win32, lesend, Reparse-sicher, Cloud-sicher)
- Kompaktes Datenmodell (≈77 Bytes/Eintrag), Aggregation in einem Durchlauf
- Slint-UI: Laufwerke, Ordnerbaum, sortierbare Inhaltstabelle, Statuszeile, Tastatur
- UAC-Neustart über `runas`, SeBackupPrivilege, Manifest, statische CRT
- CLI-Benchmark, CI (fmt/clippy/test/build), Release-Workflow

## 0.2 – Geschwindigkeit ✅ (MFT)
- NTFS-MFT-Scanner (Admin): Bitmap-gestützt, parallel geparst, Hardlinks exakt,
  C: in 0,42 s. Automatische Auswahl mit Walker-Fallback.
- Offen: MFT auch für Unterordner (Teilbaum aus dem Volume-Scan), `$MFT` mit
  `$ATTRIBUTE_LIST` (extrem fragmentiert), Alternate Data Streams
- Offen: Größenanalyse der Exe (`cargo bloat`), Slint-Features weiter reduzieren

## 0.3 – Analyse
- ✅ Treemap (Squarified, in Rust gerendert, Hover/Klick/Doppelklick, Legende)
- ✅ Dateityp-Kategorien mit Summen pro Ordner
- ✅ Größte Dateien (aktueller Ordner, Top 1000)
- Filter/Suche, Ansicht „nach Endung“
- Fehlerliste (Ordner ohne Zugriff) als eigene Ansicht
- Export (CSV)

## 0.4 – Live & Aufräumen
- USN-Journal für inkrementelle Aktualisierung ohne Neuscan
- ✅ Löschen über Papierkorb (IFileOperation) mit Bestätigung und Schutzregeln, Baum-Update ohne Rescan
- Mehrfachauswahl (Strg/Umschalt) zum Löschen mehrerer Einträge
- Bekannte Aufräumziele (Temp, Caches, Windows Update, Shader-Caches) mit Erklärung
- Optional: getrennter Admin-Helfer-Prozess, UI ohne Adminrechte

## Später
- Einstellungen (Spalten, Einheiten, Sprache), Mehrfach-Tabs, Vergleich zweier Scans
- Code-Signierung der Releases
