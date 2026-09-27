# Roadmap

## 0.1 – Grundgerüst ✅
- Paralleler Verzeichnis-Scanner (Win32, lesend, Reparse-sicher, Cloud-sicher)
- Kompaktes Datenmodell (≈77 Bytes/Eintrag), Aggregation in einem Durchlauf
- Slint-UI: Laufwerke, Ordnerbaum, sortierbare Inhaltstabelle, Statuszeile, Tastatur
- UAC-Neustart über `runas`, SeBackupPrivilege, Manifest, statische CRT
- CLI-Benchmark, CI (fmt/clippy/test/build), Release-Workflow

## 0.2 – Geschwindigkeit
- **NTFS-MFT-Scanner** (Admin): `$MFT` direkt lesen (FSCTL_GET_NTFS_VOLUME_DATA,
  Retrieval Pointers, FILE-Records inkl. Fixups, `$ATTRIBUTE_LIST`,
  nicht-residente `$DATA`). Ziel: C: in < 3 s kalt. Exakte Hardlink-Deduplizierung.
- Automatische Auswahl in `scanner_for` (NTFS + Admin + Laufwerkswurzel → MFT)
- Größenanalyse der Exe (`cargo bloat`), Slint-Features weiter reduzieren

## 0.3 – Analyse
- Treemap (Squarified, in Rust berechnet, als Bild gerendert)
- Statistik nach Dateityp/Endung, größte Dateien global, Filter/Suche
- Fehlerliste (Ordner ohne Zugriff) als eigene Ansicht
- Export (CSV)

## 0.4 – Live & Aufräumen
- USN-Journal für inkrementelle Aktualisierung ohne Neuscan
- Löschen über Papierkorb (IFileOperation) mit Bestätigung, Baum-Update ohne Rescan
- Bekannte Aufräumziele (Temp, Caches, Windows Update, Shader-Caches) mit Erklärung
- Optional: getrennter Admin-Helfer-Prozess, UI ohne Adminrechte

## Später
- Einstellungen (Spalten, Einheiten, Sprache), Mehrfach-Tabs, Vergleich zweier Scans
- Code-Signierung der Releases
