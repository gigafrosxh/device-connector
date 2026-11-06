# Device Connector

Ein Windows-PC und ein Mac fühlen sich an wie ein Rechner mit zwei Bildschirmen: Die Maus wandert am Bildschirmrand hinüber, die Tastatur kommt mit, die Zwischenablage ist auf beiden Geräten gleich, und Dateien lassen sich über den Rand ziehen.

> **Status: Alpha (v0.1.0-alpha.9).** Läuft unter Windows 11 und macOS 11+ (Apple Silicon und Intel). Bitte Fehler als Issue melden.

## Was geht und was nicht

| Funktion | Stand | Hinweis |
| --- | --- | --- |
| Maus am Bildschirmrand wechseln | ✅ | Anordnung per Ziehen: Menü → „Bildschirme anordnen …“ |
| Tastatur weiterleiten | ✅ | Physische Taste wird übertragen, beide Rechner sollten dasselbe Layout haben. Strg ⇄ Cmd wird getauscht (abschaltbar). |
| Zwischenablage (Text) in beide Richtungen | ✅ | bis 1 MB |
| Dateien über den Rand ziehen | 🧪 Prototyp | Drüben hängen sie am Zeiger und lassen sich in jeden Ordner fallen |
| Fenster-Übergabe (Strg+Alt+H) | 🧪 Prototyp | Öffnet Webseite bzw. Dokument des aktiven Fensters am anderen Rechner |
| Laufwerke des anderen Rechners | 🧪 Prototyp | Alle Platten, Partitionen und USB-Sticks von drüben als Netzlaufwerk im Finder bzw. Explorer, lesen und schreiben |
| Ton des anderen Rechners hier hören | 🧪 Prototyp | Systemton von drüben über das Standard-Ausgabegerät oder ein gewähltes Gerät, in beide Richtungen (eine zur Zeit). Mac als Quelle ab macOS 14.2 |
| Verbindung per Bluetooth (seriell/SPP) | ✅ | siehe Einrichtung |
| Verbindung per Netzwerk/USB-C-Kabel (TCP) | ✅ | WLAN, Ethernet, Thunderbolt-Bridge |

Ehrliche Grenzen:

* **Bluetooth ist langsam für Dateien**: real etwa 100–200 KB/s. Maus und Tastatur brauchen nur wenige Bytes und fühlen sich normal an; große Dateien besser über Netzwerk oder Kabel.
* **Fenster können nicht wirklich zwischen macOS und Windows wandern**, ein Programm läuft nur auf seinem eigenen System. Die Fenster-Übergabe öffnet deshalb den *Inhalt* drüben (die Webseite im Browser, das Dokument im passenden Programm).
* **HDMI und DisplayPort übertragen keine Daten zwischen zwei Rechnern**, nur Bild vom Rechner zum Monitor. Als Kabel taugen USB-C/Thunderbolt (Netzwerk über Kabel) oder Ethernet.

## Installation

Installer gibt es unter [Releases](https://github.com/gigafrosxh/device-connector/releases):

* **Windows 11 x64** (Intel/AMD): `DeviceConnector-<version>-windows-x64-setup.exe` (oder die portable `.zip`).
* **Windows 11 ARM** (z. B. Snapdragon): `DeviceConnector-<version>-windows-arm64-setup.exe` (oder die portable `.zip`).
* **macOS** (Apple Silicon und Intel): `DeviceConnector-<version>-macos-universal.dmg` öffnen, App in „Programme“ ziehen.

### Signierte und unsignierte Builds

Jedes Release sagt oben in den Notizen, ob es signiert ist.

| | Signiert | Unsigniert (Dateiname endet auf `-unsigned`) |
| --- | --- | --- |
| Windows | Authenticode-Signatur, SmartScreen kennt den Herausgeber | SmartScreen warnt: „Weitere Informationen“ → „Trotzdem ausführen“ |
| macOS | Developer ID, notarisiert, startet per Doppelklick | Beim ersten Start **Rechtsklick → Öffnen**; nach jedem Update fragt macOS die Berechtigungen neu ab |

Unsignierte Builds sind der Entwicklerkanal: gleicher Code, nur ohne Zertifikat. Die Prüfsummen in `SHA256SUMS.txt` gelten für beide.

**Für Maintainer:** Signiert wird automatisch, sobald diese Repository-Secrets gesetzt sind (Settings → Secrets and variables → Actions):

| Secret | Inhalt |
| --- | --- |
| `WINDOWS_CERT_PFX` | Code-Signing-Zertifikat als `.pfx`, base64 (`base64 -w0 cert.pfx`) |
| `WINDOWS_CERT_PASSWORD` | Passwort der `.pfx` |
| `MACOS_CERT_P12` | „Developer ID Application“-Zertifikat mit Schlüssel als `.p12`, base64 |
| `MACOS_CERT_PASSWORD` | Passwort der `.p12` |
| `MACOS_SIGN_IDENTITY` | z. B. `Developer ID Application: Name (TEAMID)` |
| `APPLE_ID`, `APPLE_TEAM_ID`, `APPLE_APP_PASSWORD` | Apple-ID, Team-ID und app-spezifisches Passwort für `notarytool` |

Sind die Secrets einer Plattform nur teilweise gesetzt, bricht das Release ab. Mit der Repository-Variable `REQUIRE_SIGNING=true` darf überhaupt kein unsigniertes Release mehr entstehen. Bei gesetzten Secrets beendet jeder Signier-, Notarisierungs- oder Prüffehler den Lauf, bevor etwas veröffentlicht wird. Lokal: `MACOS_SIGN_IDENTITY=… APPLE_ID=… packaging/macos/build-dmg.sh` bzw. `packaging\windows\sign.ps1 <datei>` mit `WINDOWS_CERT_PFX`/`WINDOWS_CERT_PASSWORD` in der Umgebung.

### macOS-Berechtigungen

Systemeinstellungen → Datenschutz & Sicherheit:

| Berechtigung | Wofür |
| --- | --- |
| **Bedienungshilfen** | Maus und Tastatur steuern, wenn der PC den Mac bedient; Eingaben abfangen |
| **Eingabeüberwachung** | Maus und Tastatur mitlesen, um sie an den PC zu geben |
| **Automation → Finder / System Events** | Markierte Dateien lesen (Ziehen über den Rand), aktives Fenster für die Übergabe |
| **Bluetooth** | Den seriellen Bluetooth-Port öffnen |
| **Bildschirm- & Systemaudioaufnahme** (nur Systemaudio) | Nur wenn der PC den Ton des Macs abspielen soll. Ohne Freigabe kommt drüben nur Stille an |
| **Lokales Netzwerk** | Netzwerk- und Kabelverbindung (TCP). Ohne Freigabe meldet die App „No route to host“. |
| **Netzwerk → Firewall → Optionen** | Nur bei eingeschalteter Firewall: „Device Connector“ auf „Eingehende Verbindungen erlauben“, wenn der Mac auf Verbindungen wartet |

Device Connector fragt beim ersten Start danach. Maus und Tastatur werden nach dem Erlauben innerhalb weniger Sekunden aktiv, ein Neustart ist nicht nötig. Hinweis zu unsignierten Builds: Nach jedem Update fragt macOS erneut, weil die App ohne Entwicklerzertifikat signiert ist. Dann den alten Eintrag in der Liste entfernen und neu erlauben.

### Windows

Keine besonderen Berechtigungen. Programme, die als Administrator laufen (z. B. der Task-Manager), nehmen keine Eingaben vom Mac an, solange Device Connector nicht selbst als Administrator gestartet wird.

## Einrichtung

1. Auf beiden Rechnern starten. Ein Symbol mit zwei Bildschirmen erscheint im Tray (Windows) bzw. in der Menüleiste (Mac).
2. **Kopplungscode**: Auf einem Rechner „Kopplungscode: …“ anklicken, auf dem anderen „Kopplungscode eingeben …“ wählen und ihn eintippen. Der Code verschlüsselt die Verbindung, ohne ihn kann niemand mitlesen oder Eingaben einschleusen.
3. **Anordnung**: „Anderer Rechner steht … links/rechts“. Das reicht auf einem Rechner, der andere übernimmt es.
4. **Verbindung wählen**, eine von beiden:

**Bluetooth (seriell)**

1. Mac und PC in den Bluetooth-Einstellungen miteinander koppeln.
2. Windows: Einstellungen → Bluetooth und Geräte → Geräte → „Weitere Bluetooth-Einstellungen“ → Reiter **COM-Anschlüsse** → Hinzufügen → **Eingehend** → OK. Den neuen Port (z. B. `COM5`) im Menü unter „Verbindung“ wählen.
3. Mac: Unter „Verbindung“ den Port mit dem Namen des PCs wählen (z. B. `DESKTOP-ABC-…`). Fehlt er, „Bluetooth-Ports neu suchen“.

Neuere macOS-Versionen bieten den seriellen Bluetooth-Port nicht für jedes Gerät an. Wenn kein Port erscheint, die Netzwerk-Variante nehmen; ein nativer Bluetooth-Transport ist für v0.4 geplant.

**Netzwerk oder Kabel (TCP)**

1. Auf einem Rechner „Netzwerk: auf Verbindung warten“ wählen. Windows fragt einmal nach der Firewall, „Zulassen“ (privates Netz).
2. Auf dem anderen „Netzwerk: mit Adresse verbinden …“ und die IP-Adresse des ersten eintragen (Port 47812).
3. Per Kabel: Mac und PC mit einem USB-C/Thunderbolt-Kabel verbinden (Thunderbolt-Bridge bzw. USB-Netzwerk) oder per Ethernet, dann wie oben.

## Bedienung

| Aktion | So geht's |
| --- | --- |
| Maus rüberschicken | Maus über den Rand zum anderen Rechner bewegen |
| Maus zurückholen | Über denselben Rand zurück, oder **Strg+Alt+Esc** (Notausgang) |
| Kopieren und Einfügen | Ganz normal; die Zwischenablage gleicht sich nach etwa einer halben Sekunde ab |
| Dateien übertragen | Im Explorer/Finder Dateien greifen und mit gedrückter Taste über den Rand ziehen. Sobald sie drüben angekommen sind, hängen sie am Zeiger: über dem Zielordner loslassen. Wer früher loslässt oder daneben fallen lässt, findet sie in `Downloads/DeviceConnector`. Fortschritt steht im Menü. Alternativ: „Dateien senden …“ im Menü. |
| Fenster übergeben | Im Browser oder Dokument **Strg+Alt+H** (Mac: Ctrl+Option+H) drücken. Drüben fragt die App vor dem Öffnen nach, außer „Übergaben ohne Rückfrage öffnen“ ist im Menü an |
| Bildschirme anordnen | Menü → „Bildschirme anordnen …“ öffnet im Browser eine Ansicht wie in den Systemeinstellungen. Den grünen Bildschirm des anderen Rechners an die Seite und Höhe ziehen, an der er wirklich steht, dann „Übernehmen“. Beide Rechner übernehmen die Anordnung |
| Laufwerke des anderen Rechners | Nach dem Verbinden erscheinen sie von selbst: am Mac im Finder unter „Orte“ (Name des PCs, darin z. B. „Windows (C)“ und der USB-Stick), am PC im Explorer als eigener Laufwerksbuchstabe (Name des Macs, darin „Macintosh HD“ und externe Platten). Menü → „Laufwerke des anderen Rechners öffnen“ öffnet sie direkt; klappt das Einbinden nicht, erscheint eine Ordnerliste im Browser. Unter „Laufwerke“ lässt sich die eigene Freigabe und das Einbinden abschalten |
| Ton vom anderen Rechner hören | Am Rechner, an dem der Ton herauskommen soll: Menü → „Ton vom anderen Rechner“ → „Hier abspielen“. Darunter das Ausgabegerät wählen: „Standard-Ausgabegerät“ folgt dem Gerät, das das System gerade nutzt (z. B. eingesteckte Kopfhörer), oder ein festes Gerät. „Ausgabegeräte neu suchen“ holt neu angeschlossene Geräte in die Liste. Der andere Rechner schickt dann seinen gesamten Systemton |
| Unterschiedliche Bildschirmhöhen | Menü → „Bildschirmhöhe abgleichen“: ganze Höhe auf ganze Höhe (Standard) oder Pixel 1:1 mit Ober-, Unterkanten oder Mitten bündig. Beide Rechner übernehmen die Einstellung |

**Laufwerke unter Windows:** Windows lädt über diesen Weg normalerweise höchstens 50 MB pro Datei. Menü → „Laufwerke“ → „Große Dateien (über 50 MB) erlauben …“ hebt die Grenze auf 4 GB an (fragt einmal nach Administratorrechten). **Laufwerke am Mac:** Greift der PC zum ersten Mal auf Schreibtisch, Dokumente, Downloads oder einen USB-Stick des Macs zu, fragt macOS am Mac einmal nach der Erlaubnis. Wer alles ohne Nachfragen freigeben will, gibt Device Connector unter „Datenschutz & Sicherheit“ → „Festplattenvollzugriff“ frei. Freigegeben wird nur an den gekoppelten Rechner, verschlüsselt über die bestehende Verbindung; Netzlaufwerke werden nicht weitergereicht. Über Bluetooth ist der Zugriff sehr langsam, besser Netzwerk oder Kabel.

**Ton:** Der Ton läuft am anderen Rechner weiter; wer ihn nur hier hören will, stellt ihn drüben leise oder stumm. Verzögerung etwa 0,1–0,2 Sekunden, für Musik und Videos gut, für Spiele merklich. Über Netzwerk/Kabel in 48 kHz Stereo (rund 400 kbit/s), über Bluetooth in 24 kHz. Immer nur eine Richtung zugleich, sonst liefe der Ton im Kreis. Den Ton eines Macs aufnehmen geht erst ab macOS 14.2; macOS fragt dabei einmal nach „Systemaudioaufnahme“.

Tasten, die drüben nicht nachgebildet werden können, werden nicht still verschluckt: das Menü zeigt kurz, welche Taste es war.

**Hängt der Mauswechsel?** Menü → „Diagnose: Mauswechsel aufzeichnen“ auf *beiden* Rechnern einschalten, den Fehler auslösen und die Datei `handoff-trace.log` (neben dem Log) an das Issue hängen. Sie enthält nur Zeitpunkte, Zustände und Koordinaten, keine Tasten oder Inhalte.

Probleme? Das Log liegt unter `%APPDATA%\DeviceConnector\config\device-connector.log` (Windows) bzw. `~/Library/Application Support/DeviceConnector/device-connector.log` (Mac). Mit der Umgebungsvariable `DC_DEBUG=1` wird es ausführlicher.

## Selbst bauen

Voraussetzung: [Rust](https://rustup.rs) 1.80 oder neuer.

```sh
git clone https://github.com/gigafrosxh/device-connector
cd device-connector
cargo test --all            # Tests (laufen auch unter Linux)
cargo run -p device-connector   # Debug-Start mit Log in der Konsole
```

**Windows** (Visual Studio Build Tools mit „Desktopentwicklung mit C++“):

```powershell
cargo build --release -p device-connector
# Installer (Inno Setup 6 installiert):
& "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe" /DAppVersion=0.1.0-alpha.9 packaging\windows\DeviceConnector.iss
# → dist\DeviceConnector-0.1.0-alpha.9-windows-x64-setup.exe
```

**macOS** (Xcode-Befehlszeilentools):

```sh
packaging/macos/build-dmg.sh
# → dist/DeviceConnector-<version>-macos-universal.dmg  und  dist/Device Connector.app
```

Für Eingabe-Tests unter macOS die gebaute `.app` starten, nicht `cargo run`: Die Berechtigungen gelten sonst für das Terminal.

## Versionen und Releases

Versionen folgen [SemVer](https://semver.org/lang/de/): `MAJOR.MINOR.PATCH`, vor 1.0 kann sich noch alles ändern.

| Tag | Bedeutung |
| --- | --- |
| `v0.1.0-alpha.N` | Neue Funktionen, wenig getestet (Pre-release) |
| `v0.1.0-rc.N` | Release Candidate: Funktionen fest, nur noch Fehler beheben (Pre-release) |
| `v0.1.0` | Stabil |
| `v0.1.1` | Nur Fehlerbehebungen |

Ablauf: Version in `Cargo.toml` (`[workspace.package]`) und `CHANGELOG.md` anpassen, mergen, dann `git tag v0.1.0-alpha.9 && git push origin v0.1.0-alpha.9`. Der Workflow `release.yml` prüft, dass Tag und `Cargo.toml` übereinstimmen, baut Installer für Windows und macOS und legt das GitHub-Release an (mit Bindestrich als Pre-release).

Geplant: v0.2 Dateien (fertig), v0.3 Fenster-Übergabe (fertig), v0.4 Kabel und natives Bluetooth-RFCOMM.

## Aufbau

```
crates/dc-core/            plattformunabhängig, mit Tests
  protocol.rs   Nachrichten (postcard, wenige Bytes pro Mausbewegung)
  engine/       Zustandsautomat: wer hat die Maus? (local.rs: Rand und
                Weiterleiten, remote.rs: Abspielen, tests.rs)
  layout.rs     Bildschirmrand, virtueller Cursor
  keymap.rs     USB-HID ⇄ Windows-Scancode ⇄ macOS-Keycode
  link.rs       Rahmen, Rollen-Sync, Noise-Verschlüsselung (NNpsk0), Vorrang für Eingaben
  transport.rs  Bluetooth-Seriell (SPP) und TCP; neuer Transport = ein Trait
  files.rs      Dateiübertragung mit Pfadprüfung
  remotefs.rs   Laufwerke des anderen Rechners: Anfragen und Ausführung
  audio.rs      Ton des anderen Rechners: IMA-ADPCM, Umrechnung, Wiedergabepuffer
  pairing.rs    Kopplungscode
crates/device-connector/   die App
  service/      verbindet Engine, Verbindung, Zwischenablage, Dateien
                (mod.rs Ereignisschleife; connection, actions, messages,
                commands, transfer, drag, clipboard)
  trace.rs      Diagnose-Aufzeichnung für den Mauswechsel
  ui.rs         Tray-/Menüleisten-Menü
  arrange.rs    Fenster „Bildschirme anordnen“ (lokale Webseite)
  drives/       Laufwerke freigeben, WebDAV-Server auf 127.0.0.1 (dav.rs),
                Einbinden im Finder/Explorer (mount.rs), Laufwerksliste
  audio.rs      Ton aufnehmen (WASAPI-Loopback, Core-Audio-Tap) und abspielen (cpal)
  platform/     windows.rs (Low-Level-Hooks, SendInput), macos.rs (CGEventTap, CGEventPost)
crates/dc-core/tests/      Zwei-Rechner-Simulation, Property-Tests
fuzz/                      Fuzz-Ziele (cargo-fuzz, siehe fuzz/README.md)
packaging/                 Inno-Setup-Skript, macOS-App-Bundle und DMG, Signier-Skripte
docs/hardware-testplan.md  Prüfliste für echte Windows-↔-macOS-Tests vor einem Release
```

Beide Rechner sind gleichberechtigt; welcher die Verbindung „aufbaut“, wird beim Verbinden ausgelost.

## Lizenz

MIT
