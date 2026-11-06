//! Einstellungen, gespeichert als JSON im Konfigurationsordner:
//! Windows `%APPDATA%\DeviceConnector\config\config.json`,
//! macOS `~/Library/Application Support/DeviceConnector/config.json`.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use dc_core::transport::DEFAULT_TCP_PORT;
use dc_core::{pairing, Align, Side};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TransportChoice {
    None,
    Serial { port: String },
    TcpListen,
    TcpConnect { addr: String },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(default)]
pub struct Config {
    /// Anzeigename dieses Rechners.
    pub name: String,
    /// Auf welcher Seite steht der andere Rechner?
    pub peer_side: Side,
    pub transport: TransportChoice,
    pub pairing_code: String,
    pub share_clipboard: bool,
    pub swap_ctrl_cmd: bool,
    pub tcp_port: u16,
    /// Zielordner für empfangene Dateien (Standard: Downloads/DeviceConnector).
    pub receive_dir: Option<PathBuf>,
    /// Webseiten/Dokumente aus der Fenster-Übergabe ohne Rückfrage öffnen.
    /// Standard aus: Es erscheint erst eine Rückfrage.
    pub auto_open_from_peer: bool,
    /// Diagnose-Aufzeichnung für den Mauswechsel (handoff-trace.log).
    pub trace_handoff: bool,
    /// Höhenabgleich der Bildschirme (#4).
    pub screen_align: Align,
    /// Eigene Laufwerke für den anderen Rechner freigeben.
    pub share_drives: bool,
    /// Laufwerke des anderen Rechners beim Verbinden einbinden
    /// (Finder-Seitenleiste bzw. Laufwerksbuchstabe im Explorer).
    pub mount_peer_drives: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            name: default_name(),
            peer_side: if cfg!(target_os = "macos") {
                Side::Left
            } else {
                Side::Right
            },
            transport: TransportChoice::None,
            pairing_code: pairing::generate(),
            share_clipboard: true,
            swap_ctrl_cmd: true,
            tcp_port: DEFAULT_TCP_PORT,
            receive_dir: None,
            auto_open_from_peer: false,
            trace_handoff: false,
            screen_align: Align::Stretch,
            share_drives: true,
            mount_peer_drives: true,
        }
    }
}

fn default_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            if cfg!(target_os = "macos") {
                "Mac".into()
            } else {
                "PC".into()
            }
        })
}

pub fn config_dir() -> PathBuf {
    directories::ProjectDirs::from("", "", "DeviceConnector")
        .map(|d| d.config_dir().to_path_buf())
        .unwrap_or_else(|| std::env::temp_dir().join("DeviceConnector"))
}

fn config_path() -> PathBuf {
    config_dir().join("config.json")
}

impl Config {
    pub fn load() -> Config {
        Self::load_from(&config_path())
    }

    fn load_from(path: &Path) -> Config {
        match fs::read_to_string(path) {
            Ok(s) => match serde_json::from_str::<Config>(&s) {
                Ok(mut c) => {
                    if !pairing::is_valid(&c.pairing_code) {
                        c.pairing_code = pairing::generate();
                        let _ = c.save_to(path);
                    }
                    c
                }
                Err(e) => {
                    // Unlesbare Datei nicht stillschweigend überschreiben,
                    // sondern daneben aufheben (#6).
                    let backup = path.with_extension("json.defekt");
                    log::warn!(
                        "Konfiguration unlesbar ({e}), nehme Standardwerte; alte Datei: {}",
                        backup.display()
                    );
                    let _ = fs::rename(path, &backup);
                    Config::default()
                }
            },
            Err(_) => {
                let c = Config::default();
                let _ = c.save_to(path);
                c
            }
        }
    }

    /// Speichert atomar: erst in eine Zwischendatei im selben Ordner,
    /// dann ersetzen. Ein Abbruch hinterlässt nie eine halbe `config.json`.
    pub fn save(&self) -> io::Result<()> {
        let r = self.save_to(&config_path());
        if let Err(e) = &r {
            log::error!("Konfiguration nicht gespeichert: {e}");
        }
        r
    }

    fn save_to(&self, path: &Path) -> io::Result<()> {
        let dir = path
            .parent()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "kein Ordner"))?;
        fs::create_dir_all(dir)?;
        let text = serde_json::to_string_pretty(self).map_err(io::Error::other)?;
        let tmp = dir.join(format!(
            ".{}.{}.tmp",
            path.file_name().unwrap_or_default().to_string_lossy(),
            std::process::id()
        ));
        let res = (|| {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(text.as_bytes())?;
            f.sync_all()?;
            drop(f);
            // rename ersetzt atomar (Windows: MoveFileEx mit REPLACE_EXISTING).
            fs::rename(&tmp, path)
        })();
        if res.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        res
    }

    pub fn receive_dir(&self) -> PathBuf {
        self.receive_dir.clone().unwrap_or_else(|| {
            directories::UserDirs::new()
                .and_then(|u| u.download_dir().map(|d| d.to_path_buf()))
                .unwrap_or_else(std::env::temp_dir)
                .join("DeviceConnector")
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_roundtrip_and_defaults_for_missing_fields() {
        let c = Config {
            transport: TransportChoice::Serial {
                port: "COM5".into(),
            },
            ..Config::default()
        };
        let s = serde_json::to_string(&c).unwrap();
        assert_eq!(serde_json::from_str::<Config>(&s).unwrap(), c);
        // Alte/teilweise Dateien bleiben lesbar.
        let partial: Config = serde_json::from_str(r#"{"peer_side":"Left"}"#).unwrap();
        assert_eq!(partial.peer_side, Side::Left);
        assert!(partial.share_clipboard);
        assert!(!partial.auto_open_from_peer);
        assert!(partial.share_drives && partial.mount_peer_drives);
    }

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("dc-cfg-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn save_is_atomic_and_roundtrips() {
        let d = tmpdir("save");
        let path = d.join("config.json");
        let c = Config {
            name: "Test".into(),
            ..Config::default()
        };
        c.save_to(&path).unwrap();
        assert_eq!(Config::load_from(&path), c);
        // Keine Zwischendateien übrig.
        let names: Vec<_> = fs::read_dir(&d)
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(names.len(), 1, "{names:?}");
    }

    #[test]
    fn failed_save_keeps_previous_file() {
        let d = tmpdir("fail");
        let path = d.join("config.json");
        let c = Config::default();
        c.save_to(&path).unwrap();
        let before = fs::read_to_string(&path).unwrap();
        // Ziel ist ein Ordner → Ersetzen schlägt fehl, Fehler kommt beim Aufrufer an.
        let bad = d.join("sub");
        fs::create_dir_all(bad.join("x")).unwrap();
        assert!(c.save_to(&bad).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), before);
        assert_eq!(fs::read_dir(&d).unwrap().count(), 2);
    }

    #[test]
    fn unreadable_config_is_kept_as_backup() {
        let d = tmpdir("broken");
        let path = d.join("config.json");
        fs::write(&path, "{ kaputt").unwrap();
        let c = Config::load_from(&path);
        assert_eq!(c.peer_side, Config::default().peer_side);
        assert_eq!(
            fs::read_to_string(d.join("config.json.defekt")).unwrap(),
            "{ kaputt"
        );
    }
}
