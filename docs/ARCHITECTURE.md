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
│  walker (parallel)  [mft: TODO]  │
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

Messung auf C: (NVMe, 12 Threads, warmer Cache): ≈600 000 Einträge in ≈0,86 s.

**Bekannte Grenzen (Walker)**
- Hardlinks werden pro Link gezählt, wie im Explorer. Exakt geht das nur mit
  Link-Zählern aus der MFT.
- Ohne Adminrechte fehlen geschützte Ordner. Sie werden als Fehler markiert und
  in der Statuszeile gezählt.

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
- Renderer ist der Slint-Software-Renderer: ≈8 MB privater Speicher im Leerlauf
  gegenüber ≈80 MB mit OpenGL, und kein GPU-Treiber im Prozess.

## Build & Auslieferung

- Release-Profil: `lto = "fat"`, `codegen-units = 1`, `strip`, `panic = "unwind"`
  (für `catch_unwind` in den Workern).
- Die MSVC-Laufzeit wird statisch gelinkt (`.cargo/config.toml`), damit die Exe
  ohne VC-Redistributable läuft.
- `build.rs` erzeugt die Ressourcen: Manifest und Versionsinfo aus `Cargo.toml`.
  Bei MSVC-Builds sind sie Pflicht, der Build bricht sonst ab.
