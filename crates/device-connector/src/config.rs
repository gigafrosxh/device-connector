//! Einstellungen, gespeichert als JSON im Konfigurationsordner:
//! Windows `%APPDATA%\DeviceConnector\config\config.json`,
//! macOS `~/Library/Application Support/DeviceConnector/config.json`.

use std::fs;
use std::path::PathBuf;

use dc_core::transport::DEFAULT_TCP_PORT;
use dc_core::{pairing, Side};
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
        let path = config_path();
        match fs::read_to_string(&path) {
            Ok(s) => match serde_json::from_str::<Config>(&s) {
                Ok(mut c) => {
                    if !pairing::is_valid(&c.pairing_code) {
                        c.pairing_code = pairing::generate();
                        c.save();
                    }
                    c
                }
                Err(e) => {
                    log::warn!("Konfiguration unlesbar ({e}), nehme Standardwerte");
                    Config::default()
                }
            },
            Err(_) => {
                let c = Config::default();
                c.save();
                c
            }
        }
    }

    pub fn save(&self) {
        let path = config_path();
        if let Some(dir) = path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        match serde_json::to_string_pretty(self) {
            Ok(s) => {
                if let Err(e) = fs::write(&path, s) {
                    log::error!("Konfiguration nicht gespeichert: {e}");
                }
            }
            Err(e) => log::error!("Konfiguration nicht serialisierbar: {e}"),
        }
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
    }
}
