//! Plattformunabhängiger Kern von Device Connector.
//!
//! Alles, was nicht direkt mit dem Betriebssystem spricht, lebt hier und ist
//! unter Linux, macOS und Windows gleichermaßen testbar:
//!
//! * [`protocol`] – die Nachrichten zwischen den beiden Rechnern
//! * [`audio`]    – Ton des anderen Rechners (Kodierung, Umrechnung, Puffer)
//! * [`engine`]   – der Zustandsautomat (wer hat gerade die Maus?)
//! * [`layout`]   – Bildschirmrand-Erkennung und virtueller Cursor
//! * [`keymap`]   – Tastencodes (USB-HID ⇄ Windows-Scancode ⇄ macOS-Keycode)
//! * [`link`]     – Rahmen, Handshake und Verschlüsselung über einen beliebigen Transport
//! * [`transport`]– Bluetooth-Seriell (SPP) und TCP
//! * [`files`]    – Dateiübertragung
//! * [`pairing`]  – Kopplungscode
//! * [`remotefs`] – Laufwerke des anderen Rechners (Dateizugriff über die Verbindung)

pub mod audio;
pub mod engine;
pub mod files;
pub mod keymap;
pub mod layout;
pub mod link;
pub mod pairing;
pub mod protocol;
pub mod remotefs;
pub mod transport;

pub use engine::{Action, Engine, EngineConfig, Inject, Input, Mode};
pub use layout::{Align, Rect, Side};
pub use protocol::{Button, Message, Os, TransferOutcome};
