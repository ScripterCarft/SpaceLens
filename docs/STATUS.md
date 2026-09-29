# Projektstatus

Stand: 2026-09-29 · Version 0.1.0 (unveröffentlicht, Entwicklungsstand auf `main`)

Diese Datei ist die Übergabe zwischen Arbeitssitzungen: **was fertig ist, was nicht
getestet ist und was als Nächstes kommt.** Beim Abschluss jeder größeren Arbeit
aktualisieren.

## Fertig

| Bereich | Stand | Geprüft |
|---|---|---|
| Verzeichnis-Scanner (Walker) | parallel, lesend, Reparse-/Cloud-sicher | Tests + echtes C: (≈600 k Einträge in 0,86 s warm) |
| MFT-Scanner (Admin, NTFS) | Bitmap, Pipeline, Hardlinks exakt | echtes C: als Admin: 0,42 s |
| Datenmodell | 40-B-Nodes, WTF-8-Namen, Entfernen ohne Rescan | Tests |
| Oberfläche | Reiter, Brotkrumen, Statusleiste mit Summen, kleine Fenster | Screenshots 1296×859 und 900×560 |
| Treemap + Dateitypen | Squarified, Software-Rendering, Legende | Tests + Screenshot |
| Größte Dateien, Suche + Filter | < 30 ms auf C: | Tests + UI-Automation |
| Löschen (Papierkorb) | IFileOperation, Schutzregeln, Mehrfachauswahl | Tests; Einzel-Löschen im UI bestätigt |
| Aufräumen | Katalog bekannter Müllecken, Papierkorb leeren | Tests + Screenshot (ohne Bereinigen) |
| Duplikate | Größe → Probe → XXH3-128, Kopien auswählen | Tests + UI mit Testordner |
| Einstellungen | `%APPDATA%\SpaceLens\settings.json`, kompatibel | 7 Tests + Laden/Speichern am Fenster |
| Diagnose | Crash-Berichte, Log langsamer UI-Aktionen | Test (Rotation) |

## Nicht oder nur teilweise getestet

- **Trenner ziehen** (Baum | Tabelle, Tabelle | Treemap): neu umgebaut, am echten
  Fenster nicht prüfbar (simulierte Mausnachrichten erreichen winit nicht).
- **Mehrere Einträge löschen** im UI (nur Logik-Tests); ebenso *Aufräumen →
  In den Papierkorb* und *Papierkorb leeren* im UI.
- **Admin-Kategorien** der Aufräumen-Ansicht (Windows-Temp, Update-Downloads …).
- **MFT-Scanner** auf anderen Laufwerken, HDDs, sehr großen/fragmentierten MFTs.
- **MSVC-spezifisch** (nur in der CI gebaut): eingebettetes Manifest, PDB neben der Exe.
- Hoher DPI (125 %/150 %), Dunkelmodus, Screenreader.

## Bekannte Probleme (vom Nutzer gemeldet, 2026-09-29)

Noch ohne Details – **zuerst Logs/Beschreibungen einsammeln, dann gezielt beheben:**

- **Abstürze** → Crash-Berichte unter `%LOCALAPPDATA%\SpaceLens\logs\crash-*.txt`.
- **Viele UI-Fehler** → konkrete Schritte erfragen (Vorlage: `.github/ISSUE_TEMPLATE/bug.md`).
- **Langsames Laden der Tabs** → `spacelens.log` listet jede UI-Aktion > 30 ms
  (`langsam: <Aktion> <ms>`). Bereits gefunden und behoben: *Aufräumen* 184 ms →
  < 30 ms (Papierkorb-Größe wird jetzt im Hintergrund ermittelt). Suche auf ganz C:
  liegt bei ~40 ms (Grenzbereich).
- Fehlende Funktionen (erwartet, noch nicht spezifiziert).

## Nächste Schritte (vereinbart)

1. Gemeldete Abstürze, UI-Fehler und langsame Tabs anhand der Logs beheben –
   **Stabilität vor neuen Funktionen**.
2. Danach aus der Wunschliste: Scan-Vergleich, Live-Aktualisierung (USN-Journal),
   CSV-Export, signierte Exe (siehe `ROADMAP.md`).

## Entscheidungen (Kurzfassung, Details in ARCHITECTURE.md)

- Slint statt Tauri/WebView; Software-Renderer (≈8 MB RAM im Leerlauf).
- Adminrechte nur über Manifest `asInvoker` + „Als Administrator neu starten“ (`runas`).
- Löschen ausschließlich in den Papierkorb, immer mit Rückfrage und Schutzregeln.
- Duplikat-Suche und Aufräumen lassen Programm-/Systembereiche aus (inkl. `AppData`
  und Werkzeug-Ordner mit Punkt im Profil).
- Einstellungen: Fenstergröße ja, Fensterposition bewusst nein.
