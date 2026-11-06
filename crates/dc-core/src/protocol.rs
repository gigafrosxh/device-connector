//! Nachrichten zwischen den beiden Rechnern.
//!
//! Das Format ist bewusst kompakt (postcard, varint-kodiert): eine Mausbewegung
//! braucht nur wenige Bytes, das ist wichtig für Bluetooth.

use serde::{Deserialize, Serialize};

use crate::layout::{Align, Side};
use crate::remotefs::{FsData, FsError, FsOp};

/// Erhöhen, wenn sich [`Message`] inkompatibel ändert.
/// v2: Übertragungs-ID in allen Datei-Nachrichten, Ordner-Einträge und
/// Empfangsbestätigung ([`Message::TransferResult`]).
/// v3: Laufwerke des anderen Rechners ([`Message::FsRequest`], [`Message::FsReply`]).
pub const PROTOCOL_VERSION: u16 = 3;

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Windows,
    MacOs,
    Linux,
    Other,
}

impl Os {
    pub fn current() -> Os {
        if cfg!(target_os = "windows") {
            Os::Windows
        } else if cfg!(target_os = "macos") {
            Os::MacOs
        } else if cfg!(target_os = "linux") {
            Os::Linux
        } else {
            Os::Other
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Button {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

impl Button {
    pub const ALL: [Button; 5] = [
        Button::Left,
        Button::Right,
        Button::Middle,
        Button::Back,
        Button::Forward,
    ];

    pub fn bit(self) -> u8 {
        match self {
            Button::Left => 1,
            Button::Right => 2,
            Button::Middle => 4,
            Button::Back => 8,
            Button::Forward => 16,
        }
    }
}

/// Fenster-Übergabe (Phase 3): Inhalt des aktiven Fensters drüben öffnen.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum Handoff {
    /// Eine Webseite im Standardbrowser öffnen.
    OpenUrl(String),
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum TransferOutcome {
    /// Alle Dateien liegen fertig am Ziel.
    Saved { files: u32 },
    /// Abgelehnt oder fehlgeschlagen (z. B. Platte voll, Limit überschritten).
    Failed(String),
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum Message {
    Hello {
        protocol: u16,
        name: String,
        os: Os,
        app_version: String,
    },
    /// Der Absender sagt: „Ich stehe auf deiner Seite `side`.“
    /// So muss die Anordnung nur auf einem Rechner eingestellt werden.
    Layout {
        i_am_on_your: Side,
        align: Align,
    },
    /// Größe der Bildschirmfläche des Absenders (für [`Align`]).
    Screen {
        width: f64,
        height: f64,
    },
    /// Der Absender gibt die Kontrolle ab; der Cursor betritt den Empfänger
    /// an der Kante zum Absender, `y` ist die relative Höhe (0..=1).
    Enter {
        y: f32,
    },
    /// Der gesteuerte Rechner gibt die Kontrolle zurück (Cursor hat die Kante
    /// zum Steuernden überquert).
    Leave {
        y: f32,
    },
    /// Der Steuernde holt sich die Kontrolle per Tastenkürzel zurück.
    Release,
    MouseMove {
        dx: i32,
        dy: i32,
    },
    MouseButton {
        button: Button,
        down: bool,
    },
    /// In 1/120 Raststufen (wie Windows `WHEEL_DELTA`). Positiv = nach oben / rechts.
    Wheel {
        dx: i32,
        dy: i32,
    },
    /// USB-HID Usage-ID (Seite 0x07).
    Key {
        hid: u16,
        down: bool,
    },
    Clipboard {
        text: String,
    },
    /// `transfer` trennt gleichzeitige Vorgänge; `id` ist innerhalb eines
    /// Vorgangs eindeutig.
    FileStart {
        transfer: u32,
        id: u32,
        path: String,
        size: u64,
    },
    FileChunk {
        transfer: u32,
        id: u32,
        data: Vec<u8>,
    },
    FileEnd {
        transfer: u32,
        id: u32,
    },
    /// Ein (evtl. leerer) Ordner des Vorgangs; kommt vor den Dateien darin.
    Dir {
        transfer: u32,
        path: String,
    },
    /// Alle Dateien eines Vorgangs sind da. `open_first`: erste Datei öffnen
    /// statt nur den Ordner zu zeigen (Fenster-Übergabe eines Dokuments).
    BatchEnd {
        transfer: u32,
        files: u32,
        open_first: bool,
    },
    /// Antwort des Empfängers: Vorgang gespeichert oder abgelehnt. Kommt bei
    /// einem Fehler sofort (der Sender hört dann auf), sonst nach `BatchEnd`.
    TransferResult {
        transfer: u32,
        result: TransferOutcome,
    },
    Handoff(Handoff),
    Ping,
    Pong,
    /// Die folgenden Dateien gehören zu einem Ziehen über den Rand: nicht in
    /// Downloads ablegen, sondern verdeckt bereitstellen und an den Zeiger hängen.
    DragBegin {
        transfer: u32,
    },
    /// Linke Taste wurde losgelassen: Dateien dort fallen lassen, wo der Zeiger steht.
    DragDrop,
    /// Ziehen abgebrochen (Maus zurück oder Notausgang).
    DragCancel,
    /// Vorab übertragene Dateien eines Ziehens, das den Rand nie
    /// überquert hat: ohne Spuren verwerfen.
    DragDiscard {
        transfer: u32,
    },
    /// Zugriff auf die Laufwerke des Empfängers; Antwort mit derselben `id`.
    FsRequest {
        id: u32,
        op: FsOp,
    },
    FsReply {
        id: u32,
        result: Result<FsData, FsError>,
    },
}

impl Message {
    /// Nachrichten mit niedriger Priorität dürfen Eingaben nicht ausbremsen.
    pub fn is_bulk(&self) -> bool {
        matches!(
            self,
            Message::FileStart { .. }
                | Message::FileChunk { .. }
                | Message::FileEnd { .. }
                | Message::Dir { .. }
                | Message::BatchEnd { .. }
                | Message::FsRequest {
                    op: FsOp::Write { .. },
                    ..
                }
                | Message::FsReply {
                    result: Ok(FsData::Data(_)),
                    ..
                }
        )
    }

    pub fn encode(&self) -> Vec<u8> {
        postcard::to_stdvec(self).expect("Message ist immer serialisierbar")
    }

    pub fn decode(bytes: &[u8]) -> Result<Message, postcard::Error> {
        postcard::from_bytes(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let msgs = vec![
            Message::Hello {
                protocol: PROTOCOL_VERSION,
                name: "PC".into(),
                os: Os::Windows,
                app_version: "0.1.0".into(),
            },
            Message::MouseMove { dx: -3, dy: 7 },
            Message::Key {
                hid: 0x04,
                down: true,
            },
            Message::Clipboard {
                text: "Grüße 👋".into(),
            },
            Message::FileChunk {
                transfer: 7,
                id: 1,
                data: vec![1, 2, 3],
            },
            Message::Dir {
                transfer: 7,
                path: "Leer".into(),
            },
            Message::TransferResult {
                transfer: 7,
                result: TransferOutcome::Failed("voll".into()),
            },
            Message::Handoff(Handoff::OpenUrl("https://example.com".into())),
            Message::DragBegin { transfer: 7 },
            Message::DragDrop,
            Message::DragCancel,
            Message::DragDiscard { transfer: 7 },
            Message::FsRequest {
                id: 1,
                op: FsOp::Read {
                    path: "Macintosh HD/Users".into(),
                    offset: 5,
                    len: 16,
                },
            },
            Message::FsReply {
                id: 1,
                result: Ok(FsData::Data(vec![9; 4])),
            },
            Message::FsReply {
                id: 2,
                result: Err(FsError::new(crate::remotefs::FsErrorKind::NotFound, "weg")),
            },
        ];
        for m in msgs {
            assert_eq!(Message::decode(&m.encode()).unwrap(), m);
        }
    }

    #[test]
    fn mouse_move_is_tiny() {
        // Wichtig für Bluetooth: eine Bewegung passt in wenige Bytes.
        assert!(Message::MouseMove { dx: 5, dy: -2 }.encode().len() <= 4);
    }
}
