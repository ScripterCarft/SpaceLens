# Changelog

Format: [Keep a Changelog](https://keepachangelog.com/de/1.1.0/). Versionen nach
[SemVer](https://semver.org/lang/de/).

## [Unveröffentlicht]

### Hinzugefügt
- Paralleler Verzeichnis-Scanner und NTFS-MFT-Scanner (Admin) mit automatischer Wahl.
- Oberfläche: Ordnerbaum, Tabelle, Reiter (Ordnerinhalt, Größte Dateien, Suche,
  Duplikate, Aufräumen), Brotkrumen-Pfad, Statusleiste mit Summen.
- Treemap mit Dateityp-Farben und Legende.
- Suche mit Platzhaltern und Filtern (Typ, Größe, Alter).
- Löschen in den Papierkorb mit Schutzregeln, Mehrfachauswahl.
- Aufräumen-Ansicht für bekannte Müllecken, Papierkorb leeren.
- Duplikat-Suche mit „Kopien auswählen“.
- Einstellungen in `%APPDATA%\SpaceLens\settings.json` (versioniert, kompatibel).
- Crash-Berichte und Log langsamer UI-Aktionen in `%LOCALAPPDATA%\SpaceLens\logs`.
- Symboldatei `SpaceLens.pdb` im Build für lesbare Crash-Berichte.

### Geändert
- Controller in Module aufgeteilt (Scan, Navigation, Löschen, Suche, Duplikate,
  Aufräumen, Treemap, Einstellungen, Lebenszyklus).
- Trenner rechnen ab dem Klickpunkt (Ziehen des Ordnerbaums war defekt).
- „Über SpaceLens“ beschreibt das Löschen korrekt.
- Beim Beenden wird auch eine laufende Duplikat-Suche gestoppt.
- Reiter „Aufräumen“ öffnet sofort (Papierkorb-Größe im Hintergrund; vorher 184 ms).
