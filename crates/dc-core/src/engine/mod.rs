//! Der Zustandsautomat: Wer hat gerade Maus und Tastatur?
//!
//! Die Engine kennt kein Betriebssystem. Die Plattform-Schicht füttert sie mit
//! [`Input`]s (lokale Eingaben, Nachrichten vom anderen Rechner) und führt die
//! zurückgegebenen [`Action`]s aus. Dadurch ist die ganze Logik testbar.
//!
//! Beide Rechner sind gleichberechtigt. Drei Zustände:
//!
//! * `Local`       – Maus und Tastatur wirken auf diesen Rechner.
//! * `Controlling` – Der Cursor ist drüben; lokale Eingaben werden abgefangen
//!   und gesendet.
//! * `Controlled`  – Der andere Rechner steuert uns; wir spielen seine
//!   Eingaben ab.

use std::collections::{BTreeMap, BTreeSet};

use crate::keymap::{self, hid};
use crate::layout::{Align, CursorStep, Rect, Side, VirtualCursor};
use crate::protocol::{Button, Message, Os};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Local,
    Controlling,
    Controlled,
}

#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Auf welcher Seite steht der andere Rechner?
    pub peer_side: Side,
    /// Strg ⇄ Cmd tauschen, wenn die Betriebssysteme verschieden sind.
    pub swap_ctrl_cmd: bool,
    pub share_clipboard: bool,
    pub local_os: Os,
    /// Höhenabgleich der Bildschirme (#4).
    pub align: Align,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Input {
    /// Absolute Cursorposition (nur wenn nicht abgefangen wird).
    LocalMove {
        x: f64,
        y: f64,
    },
    /// Relative Bewegung (nur während abgefangen wird).
    LocalDelta {
        dx: f64,
        dy: f64,
    },
    LocalButton {
        button: Button,
        down: bool,
    },
    LocalWheel {
        dx: i32,
        dy: i32,
    },
    LocalKey {
        hid: u16,
        down: bool,
    },
    LocalClipboard(String),
    /// Eine Taste, die (noch) nicht übertragen werden kann (#28).
    LocalUnsupportedKey(String),
    ScreenChanged(Rect),
    SetPeerSide(Side),
    SetAlign(Align),
    Connected {
        peer_os: Os,
    },
    Disconnected,
    Remote(Message),
}

/// Eingaben, die die Plattform-Schicht lokal abspielen soll.
#[derive(Debug, Clone, PartialEq)]
pub enum Inject {
    MoveTo {
        x: f64,
        y: f64,
        dx: f64,
        dy: f64,
    },
    Button {
        button: Button,
        down: bool,
        x: f64,
        y: f64,
    },
    Wheel {
        dx: i32,
        dy: i32,
    },
    Key {
        hid: u16,
        down: bool,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Send(Message),
    /// Lokale Eingaben ab jetzt abfangen und nicht mehr ans System geben.
    StartCapture,
    /// Abfangen beenden, Cursor an diese Stelle setzen.
    StopCapture {
        x: f64,
        y: f64,
    },
    Inject(Inject),
    SetClipboard(String),
    /// Beim Überqueren des Rands wurde die linke Maustaste gehalten:
    /// prüfen, ob Dateien gezogen werden (Phase 2). Kommt vor
    /// `StartCapture`, damit ein lokales Ziehen am Rand abgebrochen werden
    /// kann, bevor der Zeiger versetzt wird.
    ResolveDrag,
    /// Mit gehaltener linker Taste nahe am Rand: Dateien schon vorab
    /// übertragen, damit sie drüben sofort am Zeiger hängen (#3).
    PrefetchDrag,
    /// Linke Taste wurde losgelassen, ohne dass der Rand überquert wurde:
    /// eine Vorab-Übertragung verwerfen.
    DragEndedLocally,
    /// Eine Taste, die beim Überqueren gehalten und an dieses System schon
    /// als „gedrückt“ gemeldet war, wurde losgelassen, während abgefangen
    /// wird. Hier lokal loslassen, sonst bleibt sie für das System hängen (#2).
    ReleaseLocal {
        button: Button,
        x: f64,
        y: f64,
    },
    /// Tastenkürzel für die Fenster-Übergabe (Phase 3) wurde gedrückt.
    RequestHandoff,
    /// Anordnung hat sich geändert (vom anderen Rechner), speichern.
    PeerSideChanged(Side),
    /// Höhenabgleich hat sich geändert (vom anderen Rechner), speichern.
    AlignChanged(Align),
    ModeChanged(Mode),
    /// Kurzer Hinweis für den Nutzer (z. B. „Taste X wird nicht übertragen“).
    Notice(String),
}

pub struct Engine {
    cfg: EngineConfig,
    screen: Rect,
    mode: Mode,
    connected: bool,
    peer_os: Option<Os>,
    /// Bildschirmfläche des anderen Rechners, sobald bekannt.
    peer_screen: Option<(f64, f64)>,
    /// Lokal gedrückte Maustasten (Bitmaske), für die Drag-Erkennung.
    local_buttons: u8,
    /// Lokal gedrückte Tasten, für Tastenkürzel.
    local_keys: BTreeSet<u16>,
    /// Beim Controlling gesendete, noch gedrückte Tasten/Buttons.
    sent_keys: BTreeSet<u16>,
    sent_buttons: u8,
    /// Beim Controlled abgespielte, noch gedrückte Tasten: ferner Code →
    /// tatsächlich abgespielter Code. Das Loslassen nutzt immer die beim
    /// Drücken gewählte Übersetzung, auch wenn sich Strg/Cmd-Tausch
    /// zwischendurch ändert (#29).
    injected_keys: BTreeMap<u16, u16>,
    injected_buttons: u8,
    cursor: VirtualCursor,
    /// Rand ist erst wieder scharf, wenn der Cursor sich etwas entfernt hat.
    edge_armed: bool,
    /// Bewegungen gegen die Kante, während sie noch nicht scharf ist.
    edge_pushes: u32,
    /// Linke Taste wurde beim Überqueren gehalten (Datei-Drag).
    carrying_drag: bool,
    /// Für den aktuellen Druck der linken Taste wurde schon vorab übertragen.
    prefetch_sent: bool,
    /// Beim Überqueren gehaltene Maustasten (das System hier kennt sie als gedrückt).
    held_at_capture: u8,
    /// Wo der Rand überquert wurde.
    capture_origin: (f64, f64),
    last_clipboard: Option<String>,
}

const EDGE_REARM_PX: f64 = 4.0;
const EDGE_PUSHES_TO_REARM: u32 = 6;
const RETURN_INSET_PX: f64 = 2.0;
/// Ab diesem Abstand zum Rand (Anteil der Bildschirmbreite, mindestens
/// [`PREFETCH_MIN_PX`]) beginnt die Vorab-Übertragung gezogener Dateien.
const PREFETCH_ZONE: f64 = 0.12;
const PREFETCH_MIN_PX: f64 = 80.0;
pub const MAX_CLIPBOARD_BYTES: usize = 1024 * 1024;

impl Engine {
    pub fn new(cfg: EngineConfig, screen: Rect) -> Engine {
        let (cx, cy) = screen.center();
        Engine {
            cfg,
            screen,
            mode: Mode::Local,
            connected: false,
            peer_os: None,
            peer_screen: None,
            local_buttons: 0,
            local_keys: BTreeSet::new(),
            sent_keys: BTreeSet::new(),
            sent_buttons: 0,
            injected_keys: BTreeMap::new(),
            injected_buttons: 0,
            cursor: VirtualCursor {
                x: cx,
                y: cy,
                armed: false,
                push: 0.0,
            },
            edge_armed: true,
            edge_pushes: 0,
            carrying_drag: false,
            prefetch_sent: false,
            held_at_capture: 0,
            capture_origin: (0.0, 0.0),
            last_clipboard: None,
        }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn peer_side(&self) -> Side {
        self.cfg.peer_side
    }

    pub fn is_connected(&self) -> bool {
        self.connected
    }

    pub fn set_share_clipboard(&mut self, on: bool) {
        self.cfg.share_clipboard = on;
    }

    pub fn set_swap_ctrl_cmd(&mut self, on: bool) {
        self.cfg.swap_ctrl_cmd = on;
    }

    /// Wo zuletzt der Rand überquert wurde (Diagnose).
    pub fn capture_origin(&self) -> (f64, f64) {
        self.capture_origin
    }

    /// Fläche aller Bildschirme, mit der die Engine rechnet.
    pub fn screen(&self) -> Rect {
        self.screen
    }

    /// Position des (virtuellen) Zeigers, während dieser Rechner gesteuert wird.
    pub fn cursor(&self) -> (f64, f64) {
        (self.cursor.x, self.cursor.y)
    }

    /// Linke Taste hier drücken/loslassen, ohne dass sie vom anderen Rechner
    /// kam (Datei-Drag). Wird wie eine ferne Taste verwaltet: beim Verlassen
    /// oder Trennen automatisch losgelassen.
    pub fn inject_left(&mut self, down: bool) -> Vec<Action> {
        let bit = Button::Left.bit();
        if self.mode != Mode::Controlled || ((self.injected_buttons & bit != 0) == down) {
            return Vec::new();
        }
        if down {
            self.injected_buttons |= bit;
        } else {
            self.injected_buttons &= !bit;
        }
        vec![Action::Inject(Inject::Button {
            button: Button::Left,
            down,
            x: self.cursor.x,
            y: self.cursor.y,
        })]
    }

    pub fn handle(&mut self, input: Input) -> Vec<Action> {
        let mut out = Vec::new();
        match input {
            Input::ScreenChanged(r) => {
                if r != self.screen {
                    self.screen = r;
                    if self.mode == Mode::Controlled {
                        // Monitor/Auflösung geändert, während wir gesteuert
                        // werden: Zeiger in der neuen Fläche halten.
                        self.cursor.x = self.cursor.x.clamp(r.x, r.right());
                        self.cursor.y = self.cursor.y.clamp(r.y, r.bottom());
                    }
                    if self.connected {
                        out.push(self.screen_message());
                    }
                }
            }
            Input::SetPeerSide(side) => {
                self.cfg.peer_side = side;
                if self.connected {
                    out.push(self.layout_message());
                }
            }
            Input::SetAlign(align) => {
                self.cfg.align = align;
                if self.connected {
                    out.push(self.layout_message());
                }
            }
            Input::Connected { peer_os } => {
                self.connected = true;
                self.peer_os = Some(peer_os);
                self.edge_armed = true;
                out.push(self.screen_message());
            }
            Input::Disconnected => {
                self.connected = false;
                self.peer_os = None;
                self.peer_screen = None;
                self.return_home(&mut out, None);
                self.release_injected(&mut out);
                self.set_mode(Mode::Local, &mut out);
            }
            Input::LocalMove { x, y } => self.on_local_move(x, y, &mut out),
            Input::LocalDelta { dx, dy } => {
                if self.mode == Mode::Controlling {
                    let (dx, dy) = (dx.round() as i32, dy.round() as i32);
                    if dx != 0 || dy != 0 {
                        out.push(Action::Send(Message::MouseMove { dx, dy }));
                    }
                }
            }
            Input::LocalButton { button, down } => self.on_local_button(button, down, &mut out),
            Input::LocalWheel { dx, dy } => {
                if self.mode == Mode::Controlling {
                    out.push(Action::Send(Message::Wheel { dx, dy }));
                }
            }
            Input::LocalKey { hid, down } => self.on_local_key(hid, down, &mut out),
            Input::LocalClipboard(text) => {
                if self.connected
                    && self.cfg.share_clipboard
                    && !text.is_empty()
                    && text.len() <= MAX_CLIPBOARD_BYTES
                    && self.last_clipboard.as_deref() != Some(text.as_str())
                {
                    self.last_clipboard = Some(text.clone());
                    out.push(Action::Send(Message::Clipboard { text }));
                }
            }
            Input::LocalUnsupportedKey(name) => {
                if self.mode == Mode::Controlling {
                    out.push(Action::Notice(format!(
                        "Taste „{name}“ wird nicht zum anderen Rechner übertragen"
                    )));
                }
            }
            Input::Remote(msg) => self.on_remote(msg, &mut out),
        }
        out
    }

    fn layout_message(&self) -> Action {
        Action::Send(Message::Layout {
            i_am_on_your: self.cfg.peer_side.opposite(),
            align: self.cfg.align,
        })
    }

    fn screen_message(&self) -> Action {
        Action::Send(Message::Screen {
            width: self.screen.w,
            height: self.screen.h,
        })
    }

    /// Relative Höhe hier → relative Höhe auf dem anderen Bildschirm.
    fn to_peer_ratio(&self, ratio: f32) -> f32 {
        match self.peer_screen {
            Some((_, h)) => self.cfg.align.map(ratio, self.screen.h, h),
            None => ratio,
        }
    }

    pub fn align(&self) -> Align {
        self.cfg.align
    }

    fn set_mode(&mut self, mode: Mode, out: &mut Vec<Action>) {
        if self.mode != mode {
            self.mode = mode;
            out.push(Action::ModeChanged(mode));
        }
    }
}

mod local;
mod remote;
#[cfg(test)]
mod tests;
