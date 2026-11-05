# Device Connector

Ein Windows-PC und ein Mac fühlen sich an wie ein Rechner mit zwei Bildschirmen: Die Maus wandert am Bildschirmrand hinüber, die Tastatur kommt mit, die Zwischenablage ist auf beiden Geräten gleich, und Dateien lassen sich über den Rand ziehen.

> **Status: Alpha (v0.1.0-alpha.1).** Läuft unter Windows 11 und macOS 11+ (Apple Silicon und Intel). Bitte Fehler als Issue melden.

## Was geht und was nicht

| Funktion | Stand | Hinweis |
| --- | --- | --- |
| Maus am Bildschirmrand wechseln | ✅ | Anordnung links/rechts im Menü |
| Tastatur weiterleiten | ✅ | Physische Taste wird übertragen, beide Rechner sollten dasselbe Layout haben. Strg ⇄ Cmd wird getauscht (abschaltbar). |
| Zwischenablage (Text) in beide Richtungen | ✅ | bis 1 MB |
| Dateien über den Rand ziehen | 🧪 Prototyp | Dateien landen in `Downloads/DeviceConnector` |
| Fenster-Übergabe (Strg+Alt+H) | 🧪 Prototyp | Öffnet Webseite bzw. Dokument des aktiven Fensters am anderen Rechner |
| Verbindung per Bluetooth (seriell/SPP) | ✅ | siehe Einrichtung |
| Verbindung per Netzwerk/USB-C-Kabel (TCP) | ✅ | WLAN, Ethernet, Thunderbolt-Bridge |

Ehrliche Grenzen:

* **Bluetooth ist langsam für Dateien**: real etwa 100–200 KB/s. Maus und Tastatur brauchen nur wenige Bytes und fühlen sich normal an; große Dateien besser über Netzwerk oder Kabel.
* **Fenster können nicht wirklich zwischen macOS und Windows wandern**, ein Programm läuft nur auf seinem eigenen System. Die Fenster-Übergabe öffnet deshalb den *Inhalt* drüben (die Webseite im Browser, das Dokument im passenden Programm).
* **HDMI und DisplayPort übertragen keine Daten zwischen zwei Rechnern**, nur Bild vom Rechner zum Monitor. Als Kabel taugen USB-C/Thunderbolt (Netzwerk über Kabel) oder Ethernet.

## Installation

Installer gibt es unter [Releases](https://github.com/gigafrosxh/device-connector/releases):

* **Windows**: `DeviceConnector-<version>-windows-x64-setup.exe` (oder die portable `.zip`). SmartScreen warnt, weil die Datei nicht signiert ist: „Weitere Informationen“ → „Trotzdem ausführen“.
* **macOS**: `DeviceConnector-<version>-macos-universal.dmg` öffnen, App in „Programme“ ziehen. Beim ersten Start **Rechtsklick → Öffnen**, weil die App nicht notarisiert ist.

### macOS-Berechtigungen

Systemeinstellungen → Datenschutz & Sicherheit:

| Berechtigung | Wofür |
| --- | --- |
| **Bedienungshilfen** | Maus und Tastatur steuern, wenn der PC den Mac bedient; Eingaben abfangen |
| **Eingabeüberwachung** | Maus und Tastatur mitlesen, um sie an den PC zu geben |
| **Automation → Finder / System Events** | Markierte Dateien lesen (Ziehen über den Rand), aktives Fenster für die Übergabe |
| **Bluetooth** | Den seriellen Bluetooth-Port öffnen |

Device Connector fragt beim ersten Start danach. Nach dem Erlauben die App neu starten. Hinweis: Nach jedem Update fragt macOS erneut, weil die App ohne Entwicklerzertifikat signiert ist. Dann den alten Eintrag in der Liste entfernen und neu erlauben.

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
| Dateien übertragen | Im Explorer/Finder Dateien greifen und über den Rand ziehen, dann loslassen. Fortschritt steht im Menü. |
| Fenster übergeben | Im Browser oder Dokument **Strg+Alt+H** (Mac: Ctrl+Option+H) drücken |

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
& "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe" /DAppVersion=0.1.0-alpha.1 packaging\windows\DeviceConnector.iss
# → dist\DeviceConnector-0.1.0-alpha.1-windows-x64-setup.exe
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

Ablauf: Version in `Cargo.toml` (`[workspace.package]`) und `CHANGELOG.md` anpassen, mergen, dann `git tag v0.1.0-alpha.2 && git push origin v0.1.0-alpha.2`. Der Workflow `release.yml` prüft, dass Tag und `Cargo.toml` übereinstimmen, baut Installer für Windows und macOS und legt das GitHub-Release an (mit Bindestrich als Pre-release).

Geplant: v0.2 Dateien (fertig), v0.3 Fenster-Übergabe (fertig), v0.4 Kabel und natives Bluetooth-RFCOMM.

## Aufbau

```
crates/dc-core/            plattformunabhängig, mit Tests
  protocol.rs   Nachrichten (postcard, wenige Bytes pro Mausbewegung)
  engine.rs     Zustandsautomat: wer hat die Maus?
  layout.rs     Bildschirmrand, virtueller Cursor
  keymap.rs     USB-HID ⇄ Windows-Scancode ⇄ macOS-Keycode
  link.rs       Rahmen, Rollen-Sync, Noise-Verschlüsselung (NNpsk0), Vorrang für Eingaben
  transport.rs  Bluetooth-Seriell (SPP) und TCP; neuer Transport = ein Trait
  files.rs      Dateiübertragung mit Pfadprüfung
  pairing.rs    Kopplungscode
crates/device-connector/   die App
  service.rs    verbindet Engine, Verbindung, Zwischenablage, Dateien
  ui.rs         Tray-/Menüleisten-Menü
  platform/     windows.rs (Low-Level-Hooks, SendInput), macos.rs (CGEventTap, CGEventPost)
packaging/                 Inno-Setup-Skript, macOS-App-Bundle und DMG
```

Beide Rechner sind gleichberechtigt; welcher die Verbindung „aufbaut“, wird beim Verbinden ausgelost.

## Lizenz

MIT
