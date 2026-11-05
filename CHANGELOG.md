# Changelog

Format nach [Keep a Changelog](https://keepachangelog.com/de/1.1.0/), Versionen nach [SemVer](https://semver.org/lang/de/).

## [0.1.0-alpha.5] – unveröffentlicht

### Behoben
- Windows: Vom Mac gezogene Dateien hingen nicht am Zeiger, sondern landeten im Empfangsordner, der sich dann öffnete. Die Shell lehnte die Pfade in der Schreibweise `\\?\C:\…` ab (E_INVALIDARG) (#3).
- Kommt das Ziehen erst an, nachdem man mit gehaltener Taste schon zurück ist, wird es trotzdem übernommen; ein altes, nie gestartetes Ziehen lebt beim nächsten Mal nicht mehr auf (#3).

## [0.1.0-alpha.4] – nicht veröffentlicht

### Neu
- Dateien ziehen: Die verdeckte Übertragung beginnt, sobald das Ziehen im Explorer/Finder beginnt, nicht erst nahe am Rand (#3).
- Dateien ziehen in Gegenrichtung: Wird am gesteuerten Rechner (z. B. im Finder mit der PC-Maus) gezogen und über den Rand zurückgegangen, bricht dieser das Ziehen ab, statt die Dateien abzulegen, und die Dateien hängen am Zeiger des anderen Rechners (#3).
- Der Zeiger des gesteuerten Rechners verschwindet, sobald die Steuerung zurückgeht, und erscheint wieder, wenn dort die eigene Maus benutzt wird oder die Steuerung zurückkommt (#35).

### Behoben
- Eine Taste, die gedrückt wurde, während der Zeiger drüben war, startete nach der Rückkehr ein falsches lokales Ziehen.
- macOS: Markierte Finder-Dateien werden ohne Umweg über System Events gelesen (eine Freigabe weniger).
- Windows: Verstecken des Zeigers meldete fälschlich Fehler.

## [0.1.0-alpha.3] – nicht veröffentlicht

### Behoben
- Windows: Beim Wechsel zum anderen Rechner sprang der Zeiger drüben vom Rand in die Bildschirmmitte. Der Hook maß Ereignisse, die noch mit dem Zeiger am Rand entstanden waren, schon gegen die Bildschirmmitte. Jetzt wird erst zentriert und dann gemessen, unmögliche Sprünge werden verworfen (#2).
- Beim Zurückkommen wird der Zeiger erst an die Einstiegsstelle gesetzt und dann freigegeben, statt kurz in der Mitte aufzutauchen (#2).

## [0.1.0-alpha.2] – nicht veröffentlicht

> **Protokoll v2:** nicht kompatibel mit 0.1.0-alpha.1. Beide Rechner müssen aktualisiert werden; eine alte Gegenstelle wird erkannt und im Menü gemeldet, statt halb zu funktionieren.

### Neu
- Bildschirmhöhe abgleichen: ganze Höhe oder Pixel 1:1 mit Ober-/Unterkanten oder Mitten bündig, für beide Rechner gleich (#4).
- Dateien werden schon beim Ziehen Richtung Rand vorab übertragen; lokales Ziehen wird beim Wechsel sauber beendet (Refs #2, #3).
- Diagnose-Aufzeichnung für den Mauswechsel (`handoff-trace.log`, im Menü schaltbar) (#30).
- Mehr Tasten (Pause, Medien- und Lautstärketasten u. a.); nicht unterstützte Tasten werden gemeldet (#28).
- Optionale Signatur (Authenticode) und Notarisierung (Developer ID) im Release-Workflow; unsignierte Dateien heißen `-unsigned` (#33).
- Zwei-Rechner-Simulation in CI, Hardware-Testplan (#31), Property-Tests und Fuzz-Ziele (#32).

### Geändert
- Dateiübertragung v2: Übertragungs-ID, Empfangsbestätigung, exakte Größen- und Anzahlprüfung, Größen- und Speicherplatzgrenzen, leere Ordner, sichtbare Fehler statt stiller Lücken (#7–#12, #15, #16).
- Sendewarteschlange begrenzt und fair: Eingaben haben Vorrang, Dateien kommen trotzdem voran (#19–#21).
- Schreib-Timeouts, IPv4 und IPv6 beim Warten und Verbinden, Versionsprüfung beim Verbinden (#22–#24).
- Dienst und Engine in kleinere Module aufgeteilt (#5).

### Behoben
- Einstellungen werden atomar gespeichert, Speicherfehler erscheinen im Menü (#6).
- Frame-Parser rekursiv: eine Gegenstelle konnte mit Müll den Stack sprengen (#18).
- Tasten wurden mit falscher Zuordnung losgelassen, wenn Strg/Cmd-Tausch dazwischen umgeschaltet wurde (#29); eingehende Zwischenablage begrenzt (#25).
- Fenster-Übergabe unter Windows überschrieb Bilder und Dateien in der Zwischenablage (#26).
- Maustasten, die beim Überqueren des Rands gedrückt waren, blieben lokal hängen (Refs #2).

### Sicherheit
- Empfangene Pfade strenger geprüft: alle Windows-Gerätenamen, verbotene Zeichen, leere Teile (#13); Schreiben folgt keinen Verknüpfungen, kein Überschreiben (#14).
- Webseiten und Dokumente vom anderen Rechner öffnen erst nach Rückfrage, abschaltbar im Menü (#27).
- Bereitstellungsordner für gezogene Dateien privat und pro Ziehen eigen (#17).

## [0.1.0-alpha.1] – nicht veröffentlicht

### Neu
- Tray-/Menüleisten-App für Windows 11 und macOS 11+ (Universal).
- Mauswechsel am Bildschirmrand, Anordnung links/rechts, Notausgang Strg+Alt+Esc.
- Tastaturweiterleitung über USB-HID-Codes, Strg ⇄ Cmd-Tausch.
- Zwischenablage-Sync (Text) in beide Richtungen.
- Verbindung über Bluetooth-Seriell (SPP) oder TCP (WLAN, Ethernet, USB-C/Thunderbolt).
- Verschlüsselung mit Noise (NNpsk0) und Kopplungscode.
- Prototyp: Dateien über den Rand ziehen (Explorer/Finder). Drüben hängen sie am Zeiger und lassen sich in jeden Ordner fallen; wer vor Ende der Übertragung loslässt, findet sie in Downloads/DeviceConnector.
- Menüpunkt „Dateien senden …“.
- Prototyp: Fenster-Übergabe Strg+Alt+H (Browser-Adresse; auf dem Mac auch Dokumente).
- Installer: Inno Setup (Windows), DMG (macOS), Release-Workflow für Tags `v*`.
