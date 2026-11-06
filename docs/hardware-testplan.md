# Hardware-Testplan (vor jedem Release)

Automatisch läuft in CI: Unit-Tests, die Zwei-Rechner-Simulation
(`crates/dc-core/tests/two_peers.rs`: Randwechsel, Rückkehr, gehaltene
Tasten, Notausgang, Trennung, Ziehen, Dateiübertragung mit Bestätigung über
echte TCP-Verbindungen) und Property-Tests für die Parser. Was nur mit echter
Hardware geht, steht hier. Ergebnis je Zeile mit ✓/✗ und Notiz im Release-PR
festhalten.

**Vorbereitung:** Auf beiden Rechnern im Menü „Diagnose: Mauswechsel
aufzeichnen“ einschalten. Bei Fehlern `handoff-trace.log` und
`device-connector.log` beider Rechner anhängen (Ordner siehe README).

## Matrix

Jeden Abschnitt mindestens einmal pro Kombination:

| Rechner A | Rechner B | Verbindung |
|---|---|---|
| Windows 11 (x64) | Mac Apple Silicon | Bluetooth (SPP) |
| Windows 11 (x64) | Mac Apple Silicon | TCP (WLAN/Ethernet) |
| Windows 11 (x64) | Mac Intel | TCP |

Zusätzlich je einmal: zweiter Monitor am PC (links und rechts vom
Hauptmonitor), Windows-Skalierung 150 %, Mac mit externem Monitor.

## 1. Mauswechsel (#2, #30)

| # | Schritt | Erwartung |
|---|---|---|
| 1.1 | Langsam über den Rand zum anderen Rechner | Zeiger erscheint drüben an der Kante auf passender Höhe, hier verschwindet er sofort |
| 1.2 | Schnell über den Rand „werfen“ | wie 1.1, kein Ruckeln, kein Steckenbleiben |
| 1.3 | Zurück über die Gegenkante | Zeiger erscheint hier an der Kante, drüben verschwindet er |
| 1.4 | Direkt nach der Rückkehr weiter gegen den Rand drücken | nach kurzem Drücken wechselt er wieder rüber |
| 1.5 | 20× schnell hin und her | Zustand bleibt stimmig, kein Zeiger „halb hier, halb dort“ |
| 1.6 | Rechte Taste gedrückt über den Rand, drüben loslassen, zurück | hier ist keine Taste hängen geblieben (Kontextmenü/Markieren normal) |
| 1.7 | Strg+Alt+Esc während drüben | Kontrolle sofort zurück, keine Taste hängt drüben |
| 1.8 | Verbindung trennen (WLAN aus / Bluetooth aus), während drüben | Zeiger kommt hier zurück, drüben hängt nichts |
| 1.9 | Admin-Fenster (z. B. Task-Manager) vorne am PC, dann rüber | Zeiger verschwindet; falls nicht: Warnung im Log |
| 1.10 | Bildschirmhöhe abgleichen: jede der vier Einstellungen (#4) | Einstieg auf der eingestellten Höhe, nie blockiert |
| 1.11 | Auflösung/Skalierung ändern, während drüben | Zeiger bleibt im sichtbaren Bereich |

## 2. Tastatur (#28, #29)

| # | Schritt | Erwartung |
|---|---|---|
| 2.1 | Strg+C/V drüben (mit Strg ⇄ Cmd) | Kopieren/Einfügen funktioniert |
| 2.2 | Strg halten, im Menü Strg ⇄ Cmd umschalten, loslassen | keine Taste bleibt drüben gedrückt |
| 2.3 | F1–F12, F13–F19, Pos1/Ende, Bild auf/ab, Ziffernblock | kommen an |
| 2.4 | ISO-Taste `<>`, `^`, `´`, Umlaute (deutsches Layout auf beiden) | richtige Zeichen |
| 2.5 | Lautstärke-/Medientasten, Pause | kommen an oder Hinweis „wird nicht übertragen“ im Menü |
| 2.6 | IME (Japanisch/Koreanisch, falls verfügbar) | Umschalttasten kommen an oder klarer Hinweis |

## 3. Zwischenablage (#25, #26)

| # | Schritt | Erwartung |
|---|---|---|
| 3.1 | Text hier kopieren, drüben einfügen (beide Richtungen) | gleich |
| 3.2 | Text > 1 MB kopieren | wird nicht übertragen, drüben bleibt die alte Zwischenablage |
| 3.3 | Bild in die PC-Zwischenablage, dann Strg+Alt+H im Browser | nach der Übergabe ist das Bild wieder in der Zwischenablage |
| 3.4 | Während Strg+Alt+H läuft schnell etwas anderes kopieren | eigene Kopie wird nicht überschrieben |

## 4. Dateien (#3, #7–#17)

| # | Schritt | Erwartung |
|---|---|---|
| 4.1 | Datei aus Explorer/Finder über den Rand ziehen, drüben in Ordner fallen lassen | Datei liegt dort, hier nichts verschoben/abgelegt |
| 4.2 | Großen Ordner (> 500 MB) über den Rand ziehen | Dateien hängen am Zeiger, sobald da; Fortschritt im Menü |
| 4.3 | Datei Richtung Rand ziehen, aber hier wieder ablegen | normale lokale Aktion, drüben bleibt nichts liegen |
| 4.4 | Über den Rand, dann zurück, ohne loszulassen | hier nichts abgelegt; drüben liegt die Datei im Empfangsordner |
| 4.5 | „Dateien senden …“ mit Ordner inkl. leerer Unterordner | Struktur identisch, Prüfsummen gleich (`shasum -a 256` / `certutil -hashfile`) |
| 4.6 | Empfangsordner auf volles Laufwerk / schreibgeschützt | Fehlermeldung auf beiden Rechnern, keine Teil-Dateien |
| 4.7 | Während der Übertragung Verbindung trennen | Sender meldet Abbruch, drüben keine Reste |
| 4.8 | Zwei Übertragungen gleichzeitig starten | beide kommen vollständig an |
| 4.9 | Dateinamen `CON.txt`, `Datei.` , Emoji, Umlaute vom Mac zum PC | reservierte Namen abgelehnt, Rest korrekt |

## 5. Verbindung und Sicherheit (#19–#24, #27)

| # | Schritt | Erwartung |
|---|---|---|
| 5.1 | Falscher Kopplungscode | keine Verbindung, Hinweis |
| 5.2 | Ältere Version auf einer Seite | beide zeigen „Versionen passen nicht“ mit beiden Nummern; nach Update verbindet es ohne Neustart |
| 5.3 | Verbinden per Hostname, per IPv6-Adresse | klappt |
| 5.4 | Gegenstelle einfrieren (Prozess anhalten) | spätestens nach ~8 s „getrennt“, Beenden der App hängt nicht |
| 5.5 | Strg+Alt+H im Browser | drüben erscheint erst eine Rückfrage; mit „Übergaben ohne Rückfrage öffnen“ direkt |
| 5.6 | Während großer Übertragung Maus/Tastatur drüben nutzen | flüssig, Übertragung läuft weiter |

## 5a. Laufwerke des anderen Rechners

| # | Schritt | Erwartung |
|---|---|---|
| 5a.1 | Verbinden (Netzwerk) | Mac: Finder zeigt unter „Orte“ den PC, darin alle PC-Laufwerke (z. B. „Windows (C)“); PC: Explorer zeigt den Mac mit eigenem Buchstaben, darin „Macintosh HD“ und externe Platten |
| 5a.2 | USB-Stick am PC einstecken, im Finder neu laden | Stick erscheint innerhalb weniger Sekunden |
| 5a.3 | Datei (>100 MB) vom PC-Laufwerk auf den Mac-Schreibtisch kopieren und umgekehrt | Prüfsummen gleich, Maus bleibt während des Kopierens flüssig |
| 5a.4 | Im eingebundenen Laufwerk Ordner anlegen, umbenennen, löschen; Datei mit Umlaut im Namen | geht in beide Richtungen, Name drüben korrekt |
| 5a.5 | Windows: Datei über 50 MB öffnen, dann „Große Dateien erlauben …“ | vorher Fehler, danach klappt es |
| 5a.6 | Verbindung trennen | Laufwerk verschwindet auf beiden Seiten, Finder/Explorer hängen nicht |
| 5a.7 | „Meine Laufwerke freigeben“ auf einer Seite aus | drüben Hinweis „gibt keine Laufwerke frei“, kein Zugriff mehr |
| 5a.8 | App beenden / abstürzen lassen und neu starten | keine toten Laufwerke bleiben zurück |

## 6. Installation

| # | Schritt | Erwartung |
|---|---|---|
| 6.1 | Installer (Windows .exe) / DMG (macOS) installieren | startet; bei signierten Builds keine SmartScreen-/Gatekeeper-Warnung |
| 6.2 | Update über eine vorhandene Installation | Einstellungen bleiben erhalten |
