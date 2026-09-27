# Architektur

## Überblick

```
┌──────────────┐   ┌──────────────┐
│ SpaceLens.exe│   │spacelens-cli │      Frontends (UI-Thread / Konsole)
│  (Slint)     │   │              │
└──────┬───────┘   └──────┬───────┘
       │  Arc<Tree>, Arc<ScanProgress>
┌──────┴──────────────────┴───────┐
│ spacelens-win                    │    Win32: Scanner, Laufwerke, UAC, Explorer
│  mft (Admin) │ walker (Fallback) │
└──────┬───────────────────────────┘
       │  RawEntry-Listings
┌──────┴───────────────────────────┐
│ spacelens-core                   │    Tree, TreeBuilder, Aggregation, Queries
└──────────────────────────────────┘
```

## Datenmodell (`core/src/tree.rs`)

Ein Scan ergibt einen unveränderlichen `Tree`, geteilt per `Arc<Tree>`.

| Struktur  | Größe    | Inhalt |
|-----------|----------|--------|
| `Node`    | 40 Bytes | size, alloc, parent, name (Offset/Länge), dir-Index, mtime, flags |
| `DirInfo` | 16 Bytes | first_child, child_count, Dateien/Ordner im Teilbaum (nur Ordner) |
| Namen     | ~1 Byte/Zeichen | ein gemeinsamer WTF-8-Puffer |

Gemessen auf C: (≈600 000 Einträge): **≈77 Bytes pro Eintrag inklusive Namen**,
also ≈45 MB für den gesamten Baum.

**Invarianten**

1. `parent(id) < id` für alle Knoten außer der Wurzel (0).
2. Kinder eines Ordners liegen zusammenhängend: `first_child .. first_child + child_count`.
3. Nach `finish()` enthalten Ordner die Summen ihres Teilbaums.

Daraus folgt: Die Aggregation ist *ein* Rückwärtsdurchlauf über ein Array, ohne
Rekursion, Hashing oder Sortierung. Sortiert wird erst bei der Anzeige, und zwar
pro Ordner.

**Warum WTF-8?** Windows-Namen sind UTF-16 und dürfen ungepaarte Surrogates
enthalten. WTF-8 ist für jeden gültigen Namen identisch mit UTF-8 (Anzeige ohne
Kopie) und bleibt trotzdem verlustfrei. ASCII-Namen belegen halb so viel wie UTF-16.

## Scanner: paralleler Verzeichnis-Walker (`win/src/walker.rs`)

```
             Task{node, path}           Listing{entries, names, subdirs}
Coordinator ───────────────► N Worker ─────────────────────────────► Coordinator
 (TreeBuilder)  unbounded      │ CreateFileW(dir, BACKUP_SEMANTICS|OPEN_REPARSE_POINT)
     ▲                         │ GetFileInformationByHandleEx(FileIdExtdDirectoryInfo, 64 KiB)
     └── neue Unterordner ─────┘ Namen → WTF-8 (parallel)
```

- **Worker** öffnen jedes Verzeichnis einmal und lesen es in 64-KiB-Blöcken
  (hunderte Einträge pro Systemaufruf, inklusive Größe, belegter Größe, Attributen
  und Reparse-Tag). Dateien werden nie geöffnet.
- **Ein Koordinator** besitzt den `TreeBuilder`, hängt jedes Listing als Block an
  und verteilt die Unterordner als neue Tasks. Pro Eintrag ist das nur ein
  `memcpy`, deshalb braucht der Baum keine Locks.
- **Backpressure**: Der Ergebnis-Kanal ist begrenzt (`threads × 4`).
- **Threads**: Die Anzahl richtet sich nach dem Gerät. Bei HDD (Seek-Penalty per
  `IOCTL_STORAGE_QUERY_PROPERTY`) sind es 2, bei SSD/NVMe 4–12, bei Netzwerk 16.
- **Abbruch**: Ein Atomic-Flag, geprüft pro Verzeichnis. Der Koordinator pollt mit
  50 ms Timeout, der Teilbaum bleibt konsistent.
- **Fallback**: Unterstützt das Dateisystem `FileIdExtdDirectoryInfo` nicht
  (FAT/exFAT, manche SMB-Server), schaltet der Scanner einmalig auf
  `FileFullDirectoryInfo` um.

Messung auf C: (NVMe, 12 Threads, warmer Cache): ≈600 000 Einträge in ≈0,86 s
(als Standardbenutzer), 652 000 Einträge in 1,57 s als Administrator.

**Bekannte Grenzen (Walker)**
- Hardlinks werden pro Link gezählt, wie im Explorer. Exakt geht das nur mit
  Link-Zählern aus der MFT.
- Ohne Adminrechte fehlen geschützte Ordner. Sie werden als Fehler markiert und
  in der Statuszeile gezählt.

## Scanner: NTFS-MFT (`win/src/mft/`, nur Admin)

Wird automatisch gewählt (`AutoScanner`), wenn der Prozess Adminrechte hat und
ein ganzes NTFS-Laufwerk gescannt wird. Scheitert er mit `Unsupported`, übernimmt
der Walker.

```
read_plan: Datensatz 0 ($MFT) → Datenbereiche (Runlist) + $MFT:$BITMAP
           → Leseaufträge ≤ 4 MiB, freie 64-KiB-Blöcke werden übersprungen
2 Reader  ──AlignedBuf (NO_BUFFERING)──► N Parser (Fixups, Attribute, Namen→WTF-8)
    ▲                                         │
    └──────────── Puffer-Recycling ◄──────────┘──► Koordinator: Record-Tabelle
build_tree: CSR-Kinderlisten, pro Ordner nach Record sortiert, Breitensuche ab Record 5
```

- Parser (`record.rs`) ist reiner, sicherer Code mit Bounds-Checks und wird mit
  synthetischen Datensätzen getestet (Fixups, Hardlinks, Extension-Records,
  Sparse-Dateien, DOS-Namen, Junctions).
- **Hardlinks**: Die Größe zählt beim ersten Link in Breitensuche-Reihenfolge,
  alle weiteren Links bekommen 0 Bytes und das Flag `HARDLINK`. Die Summe
  entspricht damit dem tatsächlich belegten Platz.
- Enthält auch NTFS-Metadateien (`$MFT`, `$LogFile`, `$Bitmap` …).
- Gezählt wird nur der unbenannte Datenstrom (wie der Walker); Alternate Data
  Streams folgen später.

Messung auf C: (PCIe-3-NVMe, 1,3 GB MFT, 652 000 Einträge): **0,42 s**, davon
~70 ms beim ersten Lesen des Volumes (Windows), ~310 ms Lesen+Parsen von
633 MiB (≈2 GB/s, Grenze der SSD; 1–8 Reader gleich schnell) und ~40 ms
Baumaufbau. Der Walker braucht mit Adminrechten 1,57 s und zählt 12 GB
Hardlinks doppelt.

## Rechte (`win/src/elevation.rs`)

- Das Manifest (`app/res/SpaceLens.manifest`) setzt `asInvoker`: kein UAC-Dialog
  beim Start.
- „Als Administrator neu starten“ ruft `ShellExecuteExW` mit dem Verb `runas`
  auf (Standard-UAC-Dialog) und übergibt `--scan <Pfad>`, damit der Scan direkt
  weiterläuft.
- Mit Adminrechten wird `SeBackupPrivilege` aktiviert. Verzeichnisse mit
  Backup-Semantik lassen sich dann trotz ACL listen, nur lesend.

## UI (`app/`)

- `ui/*.slint` (Style `fluent`) definiert die Oberfläche: Toolbar, Ordnerbaum,
  Inhaltstabelle und Statuszeile.
- `models.rs` enthält lazy Slint-Modelle, die nur `NodeId`s speichern.
  `row_data` formatiert eine Zeile erst, wenn Slint sie anzeigt.
- `app.rs` ist der Controller. Er läuft komplett im UI-Thread; der Scan läuft in
  einem eigenen Thread, der Fortschritt wird per 10-Hz-Timer aus Atomics gelesen.
- **Layout** (`ui/app.slint`): jede Zeile hat eine Aufgabe – Scan-Leiste, Reiter +
  Suche, Brotkrumen-Pfad + Aktionen, (Filter), Inhalt, Treemap, Statusleiste mit
  festem Bereich für die Summen. Größen nur über min/preferred/max + Stretch
  (keine Bindungen an die Fenstergröße → keine Layout-Schleifen); geprüft bei
  1296×859 und bei der Mindestgröße 900×560.
- **Treemap** (`core/src/treemap.rs`, `ui/treemap.slint`): Squarified-Layout in
  Rust, gezeichnet direkt in einen RGB-Puffer mit Kissen-Schattierung und als ein
  einziges Bild an Slint übergeben. Beschriftungen, Hover-Rahmen und Legende sind
  leichte Overlays. Beim Größeändern wird nach 60 ms Ruhe neu gezeichnet. Gemessen
  auf C: (200 000 Kacheln, 1600×450): Layout ≈12 ms, Zeichnen ≈7 ms.
- **Dateitypen** (`core/src/filetype.rs`): ~130 Endungen → 10 Kategorien
  (Farbe + Legende). Summen pro Kategorie ≈15 ms über 600 000 Einträge, nur bei
  Ordnerwechsel neu berechnet.
- Renderer ist der Slint-Software-Renderer: ≈8 MB privater Speicher im Leerlauf
  gegenüber ≈80 MB mit OpenGL, und kein GPU-Treiber im Prozess.

## Löschen (`core/src/safety.rs`, `win/src/recycle.rs`, `app/src/app.rs`)

```
Entf / Button / Kontextmenü
  → safety::guard(tree, id)      Blocked → Erklärung, Ende · Warn → Hinweis im Dialog
  → Rückfrage (MessageBox, bei Warnung „Nein“ vorausgewählt)
  → Worker-Thread: IFileOperation (FOF_ALLOWUNDO | FOFX_RECYCLEONDELETE | FOF_WANTNUKEWARNING)
  → danach Dateisystem prüfen: nur was wirklich weg ist, wird entfernt
  → Tree::remove (Flag DELETED, Summen aller Vorfahren korrigiert, O(Tiefe))
  → Modelle behalten Aufklapp-Zustand, Treemap/Legende neu, nächste Zeile ausgewählt
```

- Kein endgültiges Löschen in der App; zu große Elemente meldet Windows selbst.
- Der Baum wird ohne Kopie bearbeitet: Modelle geben ihre `Arc<Tree>` kurz ab,
  damit `Arc::make_mut` den Baum exklusiv besitzt.
- Tabellen- und Baumzeilen sind für UI Automation/Screenreader beschriftet.

## Suche (`core/src/search.rs`)

- Ein Durchlauf über den Teilbaum des aktuellen Ordners, direkt auf dem Baum im
  Speicher. Günstige Prüfungen (Typ, Größe, Alter) zuerst, dann der Name:
  ASCII-Namen werden ohne Kopie verglichen, `*.ext` und `prefix*` haben eigene
  schnelle Wege, sonst ein allgemeiner Platzhalter-Vergleich.
- Passt ein Ordner, wird sein Inhalt nicht erneut gezählt (keine doppelte Summe).
- Nur die 5 000 größten Treffer landen in der Tabelle (Heap, O(n log k)).
- Gemessen auf C: (485 000 Dateien): alle Suchen 12–26 ms; die UI sucht 120 ms
  nach der letzten Eingabe.

## Duplikate (`core/src/duplicates.rs`, `win/src/dupes.rs`)

1. **Gleiche Größe** (aus dem Scan, keine Festplattenzugriffe) – schließt fast alles aus.
2. **Probe**: XXH3-128 über die ersten und letzten 16 KiB.
3. **Vollständige Prüfsumme** (XXH3-128, 1-MiB-Blöcke) nur für die Verbleibenden.

- Ausgelassen: System- und Programmbereiche inkl. `AppData` und Werkzeug-Ordner mit
  Punkt im Profil (`.nuget`, `.local` …) – außer man sucht gezielt darin; Hardlinks,
  Cloud-Platzhalter (würden sonst heruntergeladen), Sparse-Dateien.
- Dateien werden nur lesend mit voller Freigabe geöffnet; 4 Leser auf SSD, 1 auf HDD.
- „Kopien auswählen“ behält pro Gruppe die älteste Datei; gelöscht wird über den
  normalen Papierkorb-Weg mit allen Schutzregeln.
- Während der Suche sind Löschen/Scannen gesperrt (der Worker hält den Baum).
- Gemessen auf C: (vor dem Ausschluss der Werkzeug-Ordner): 349 Kandidaten, 1,9 GB
  gelesen in 1,9 s; Kandidatensuche 16 ms.

## Aufräumen (`core/src/cleanup.rs`, `win/src/known.rs`)

- Katalog `RULES`: Titel, Erklärung, Einstufung (Sicher/Mit Vorsicht), Admin-Bedarf
  und Ziele relativ zu bekannten Ordnern (`SHGetKnownFolderPath`), Muster mit `*`
  pro Pfadteil (z. B. alle Browser-Profile).
- Auswertung rein im Speicher über `Tree::find_path` + Mustervergleich: keine
  Festplattenzugriffe, die Ansicht öffnet sofort.
- Modus „Inhalt“ entfernt nur, was *in* einem Ordner liegt (der Ordner bleibt);
  „Nur Info“ (Windows.old) verweist auf die Windows-Einstellungen.
- Bereinigen = derselbe Papierkorb-Weg wie Löschen, aber mit `FOF_NOERRORUI`:
  benutzte Dateien werden ohne Dialog übersprungen (manueller Test in
  `recycle.rs` belegt das Verhalten). Harte Schutzregeln gelten trotzdem.
- „Papierkorb leeren“ nutzt `SHEmptyRecycleBinW` (Windows fragt selbst) und
  entfernt danach `$Recycle.Bin`-Inhalte aus dem Baum.

## Build & Auslieferung

- Release-Profil: `lto = "fat"`, `codegen-units = 1`, `strip`, `panic = "unwind"`
  (für `catch_unwind` in den Workern).
- Die MSVC-Laufzeit wird statisch gelinkt (`.cargo/config.toml`), damit die Exe
  ohne VC-Redistributable läuft.
- `build.rs` erzeugt die Ressourcen: Manifest und Versionsinfo aus `Cargo.toml`.
  Bei MSVC-Builds sind sie Pflicht, der Build bricht sonst ab.
