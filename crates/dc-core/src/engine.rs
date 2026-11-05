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
use crate::layout::{CursorStep, Rect, Side, VirtualCursor};
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
    ScreenChanged(Rect),
    SetPeerSide(Side),
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
    /// prüfen, ob Dateien gezogen werden (Phase 2).
    ResolveDrag,
    /// Tastenkürzel für die Fenster-Übergabe (Phase 3) wurde gedrückt.
    RequestHandoff,
    /// Anordnung hat sich geändert (vom anderen Rechner), speichern.
    PeerSideChanged(Side),
    ModeChanged(Mode),
}

pub struct Engine {
    cfg: EngineConfig,
    screen: Rect,
    mode: Mode,
    connected: bool,
    peer_os: Option<Os>,
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
    last_clipboard: Option<String>,
}

const EDGE_REARM_PX: f64 = 4.0;
const EDGE_PUSHES_TO_REARM: u32 = 6;
const RETURN_INSET_PX: f64 = 2.0;
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
            Input::ScreenChanged(r) => self.screen = r,
            Input::SetPeerSide(side) => {
                self.cfg.peer_side = side;
                if self.connected {
                    out.push(Action::Send(Message::Layout {
                        i_am_on_your: side.opposite(),
                    }));
                }
            }
            Input::Connected { peer_os } => {
                self.connected = true;
                self.peer_os = Some(peer_os);
                self.edge_armed = true;
            }
            Input::Disconnected => {
                self.connected = false;
                self.peer_os = None;
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
            Input::Remote(msg) => self.on_remote(msg, &mut out),
        }
        out
    }

    fn set_mode(&mut self, mode: Mode, out: &mut Vec<Action>) {
        if self.mode != mode {
            self.mode = mode;
            out.push(Action::ModeChanged(mode));
        }
    }

    fn on_local_move(&mut self, x: f64, y: f64, out: &mut Vec<Action>) {
        if self.mode != Mode::Local || !self.connected {
            return;
        }
        let side = self.cfg.peer_side;
        if !self.edge_armed {
            if self.screen.distance_from_edge(side, x) >= EDGE_REARM_PX {
                self.edge_armed = true;
                self.edge_pushes = 0;
            } else if self.screen.at_edge(side, x) {
                // Wer nach der Rückkehr am Rand weiter dagegen drückt, will
                // wieder rüber: nach ein paar Bewegungen ist der Rand scharf.
                self.edge_pushes += 1;
                if self.edge_pushes >= EDGE_PUSHES_TO_REARM {
                    self.edge_armed = true;
                    self.edge_pushes = 0;
                }
            }
            if !self.edge_armed {
                return;
            }
        }
        if self.screen.at_edge(side, x) && y >= self.screen.y && y <= self.screen.bottom() {
            log::debug!(
                "Rand überquert bei ({x:.0}, {y:.0}), gedrückte Maustasten {:#b}",
                self.local_buttons
            );
            self.set_mode(Mode::Controlling, out);
            out.push(Action::StartCapture);
            out.push(Action::Send(Message::Enter {
                y: self.screen.y_ratio(y),
            }));
            // Gedrückte Modifier mitnehmen (z. B. Shift beim Rüberziehen).
            for &k in self.local_keys.iter().filter(|k| keymap::is_modifier(**k)) {
                self.sent_keys.insert(k);
                out.push(Action::Send(Message::Key { hid: k, down: true }));
            }
            if self.local_buttons & Button::Left.bit() != 0 {
                self.carrying_drag = true;
                out.push(Action::ResolveDrag);
            }
        }
    }

    fn on_local_button(&mut self, button: Button, down: bool, out: &mut Vec<Action>) {
        if button == Button::Left {
            log::debug!(
                "Linke Taste {} ({:?})",
                if down { "gedrückt" } else { "los" },
                self.mode
            );
        }
        if down {
            self.local_buttons |= button.bit();
        } else {
            self.local_buttons &= !button.bit();
        }
        if self.mode != Mode::Controlling {
            return;
        }
        if self.carrying_drag && button == Button::Left {
            if !down {
                // Ende des Datei-Drags: drüben die Dateien fallen lassen. Die
                // Taste selbst hält dort die App, nicht wir.
                self.carrying_drag = false;
                out.push(Action::Send(Message::DragDrop));
            }
            return;
        }
        if down {
            self.sent_buttons |= button.bit();
        } else {
            if self.sent_buttons & button.bit() == 0 {
                return;
            }
            self.sent_buttons &= !button.bit();
        }
        out.push(Action::Send(Message::MouseButton { button, down }));
    }

    fn hotkey_base_held(&self) -> bool {
        let ctrl =
            self.local_keys.contains(&hid::LEFT_CTRL) || self.local_keys.contains(&hid::RIGHT_CTRL);
        let alt =
            self.local_keys.contains(&hid::LEFT_ALT) || self.local_keys.contains(&hid::RIGHT_ALT);
        ctrl && alt
    }

    fn on_local_key(&mut self, hid_code: u16, down: bool, out: &mut Vec<Action>) {
        if down {
            // Notausgang: Strg+Alt+Esc holt die Kontrolle zurück.
            if hid_code == hid::ESCAPE && self.hotkey_base_held() && self.mode == Mode::Controlling
            {
                out.push(Action::Send(Message::Release));
                self.return_home(out, None);
                self.set_mode(Mode::Local, out);
                self.local_keys.insert(hid_code);
                return;
            }
            if hid_code == hid::H
                && self.hotkey_base_held()
                && self.mode == Mode::Local
                && self.connected
            {
                out.push(Action::RequestHandoff);
            }
            self.local_keys.insert(hid_code);
        } else {
            self.local_keys.remove(&hid_code);
        }

        if self.mode == Mode::Controlling {
            if down {
                self.sent_keys.insert(hid_code);
            } else if !self.sent_keys.remove(&hid_code) {
                return;
            }
            out.push(Action::Send(Message::Key {
                hid: hid_code,
                down,
            }));
        }
    }

    /// Kontrolle zurück auf diesen Rechner. `y_ratio`: wo der Cursor
    /// zurückkommt (None = Bildschirmmitte).
    fn return_home(&mut self, out: &mut Vec<Action>, y_ratio: Option<f32>) {
        if self.mode != Mode::Controlling {
            return;
        }
        for k in std::mem::take(&mut self.sent_keys) {
            out.push(Action::Send(Message::Key {
                hid: k,
                down: false,
            }));
        }
        for b in Button::ALL {
            if self.sent_buttons & b.bit() != 0 {
                out.push(Action::Send(Message::MouseButton {
                    button: b,
                    down: false,
                }));
            }
        }
        self.sent_buttons = 0;
        if std::mem::take(&mut self.carrying_drag) {
            out.push(Action::Send(Message::DragCancel));
        }
        let (x, y) = match y_ratio {
            Some(r) => (
                self.screen.inset_x(self.cfg.peer_side, RETURN_INSET_PX),
                self.screen.y_from_ratio(r),
            ),
            None => self.screen.center(),
        };
        self.edge_armed = false;
        out.push(Action::StopCapture { x, y });
    }

    fn release_injected(&mut self, out: &mut Vec<Action>) {
        for (_, k) in std::mem::take(&mut self.injected_keys) {
            out.push(Action::Inject(Inject::Key {
                hid: k,
                down: false,
            }));
        }
        for b in Button::ALL {
            if self.injected_buttons & b.bit() != 0 {
                out.push(Action::Inject(Inject::Button {
                    button: b,
                    down: false,
                    x: self.cursor.x,
                    y: self.cursor.y,
                }));
            }
        }
        self.injected_buttons = 0;
    }

    fn translate_key(&self, hid_code: u16) -> u16 {
        let differs = self
            .peer_os
            .map(|p| p != self.cfg.local_os)
            .unwrap_or(false);
        if self.cfg.swap_ctrl_cmd && differs {
            keymap::swap_ctrl_gui(hid_code)
        } else {
            hid_code
        }
    }

    fn on_remote(&mut self, msg: Message, out: &mut Vec<Action>) {
        match msg {
            Message::Ping => out.push(Action::Send(Message::Pong)),
            Message::Layout { i_am_on_your } => {
                if self.cfg.peer_side != i_am_on_your {
                    self.cfg.peer_side = i_am_on_your;
                    out.push(Action::PeerSideChanged(i_am_on_your));
                }
            }
            Message::Enter { y } => {
                if self.mode == Mode::Controlling {
                    // Beide Seiten gleichzeitig am Rand: wir geben nach.
                    self.return_home(out, Some(y));
                }
                self.cursor = VirtualCursor::enter(&self.screen, self.cfg.peer_side, y);
                self.set_mode(Mode::Controlled, out);
                out.push(Action::Inject(Inject::MoveTo {
                    x: self.cursor.x,
                    y: self.cursor.y,
                    dx: 0.0,
                    dy: 0.0,
                }));
            }
            Message::Leave { y } => {
                if self.mode == Mode::Controlling {
                    self.return_home(out, Some(y));
                    self.set_mode(Mode::Local, out);
                }
            }
            Message::Release => {
                if self.mode == Mode::Controlled {
                    self.release_injected(out);
                    self.set_mode(Mode::Local, out);
                }
            }
            Message::MouseMove { dx, dy } => {
                if self.mode != Mode::Controlled {
                    return;
                }
                match self
                    .cursor
                    .step(&self.screen, self.cfg.peer_side, dx as f64, dy as f64)
                {
                    CursorStep::Moved { x, y } => out.push(Action::Inject(Inject::MoveTo {
                        x,
                        y,
                        dx: dx as f64,
                        dy: dy as f64,
                    })),
                    CursorStep::Exited { y_ratio } => {
                        self.release_injected(out);
                        // Cursor unsichtbar an den Rand parken.
                        out.push(Action::Inject(Inject::MoveTo {
                            x: self.screen.inset_x(self.cfg.peer_side, RETURN_INSET_PX),
                            y: self.cursor.y,
                            dx: 0.0,
                            dy: 0.0,
                        }));
                        out.push(Action::Send(Message::Leave { y: y_ratio }));
                        self.set_mode(Mode::Local, out);
                        self.edge_armed = false;
                    }
                }
            }
            Message::MouseButton { button, down } => {
                if self.mode != Mode::Controlled {
                    return;
                }
                if down {
                    self.injected_buttons |= button.bit();
                } else {
                    self.injected_buttons &= !button.bit();
                }
                out.push(Action::Inject(Inject::Button {
                    button,
                    down,
                    x: self.cursor.x,
                    y: self.cursor.y,
                }));
            }
            Message::Wheel { dx, dy } => {
                if self.mode == Mode::Controlled {
                    out.push(Action::Inject(Inject::Wheel { dx, dy }));
                }
            }
            Message::Key { hid: h, down } => {
                if self.mode != Mode::Controlled {
                    return;
                }
                let local = if down {
                    let t = self.translate_key(h);
                    *self.injected_keys.entry(h).or_insert(t)
                } else {
                    match self.injected_keys.remove(&h) {
                        Some(k) => k,
                        None => self.translate_key(h),
                    }
                };
                out.push(Action::Inject(Inject::Key { hid: local, down }));
            }
            Message::Clipboard { text } if self.cfg.share_clipboard => {
                // Auch eingehend begrenzen (#25); die eigene Zwischenablage
                // bleibt dann unverändert.
                if text.len() > MAX_CLIPBOARD_BYTES {
                    log::warn!(
                        "Zwischenablage vom anderen Rechner zu groß ({} Bytes, höchstens {MAX_CLIPBOARD_BYTES}), ignoriert",
                        text.len()
                    );
                    return;
                }
                if text.is_empty() {
                    return;
                }
                self.last_clipboard = Some(text.clone());
                out.push(Action::SetClipboard(text));
            }
            // Hello/Pong/Dateien/Übergabe behandelt die App-Schicht.
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine(side: Side, os: Os) -> Engine {
        let mut e = Engine::new(
            EngineConfig {
                peer_side: side,
                swap_ctrl_cmd: true,
                share_clipboard: true,
                local_os: os,
            },
            Rect::new(0.0, 0.0, 1920.0, 1080.0),
        );
        let peer = if os == Os::Windows {
            Os::MacOs
        } else {
            Os::Windows
        };
        e.handle(Input::Connected { peer_os: peer });
        e
    }

    fn sent(actions: &[Action]) -> Vec<Message> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::Send(m) => Some(m.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn crossing_the_edge_hands_over_control() {
        let mut pc = engine(Side::Right, Os::Windows);
        assert!(pc
            .handle(Input::LocalMove {
                x: 1000.0,
                y: 540.0
            })
            .is_empty());
        let a = pc.handle(Input::LocalMove {
            x: 1919.0,
            y: 540.0,
        });
        assert!(a.contains(&Action::StartCapture));
        assert!(matches!(sent(&a)[0], Message::Enter { y } if (y - 0.5).abs() < 0.01));
        assert_eq!(pc.mode(), Mode::Controlling);

        // Bewegungen und Tasten gehen jetzt rüber.
        let a = pc.handle(Input::LocalDelta { dx: 3.0, dy: -1.0 });
        assert_eq!(sent(&a), vec![Message::MouseMove { dx: 3, dy: -1 }]);
        let a = pc.handle(Input::LocalKey {
            hid: 0x04,
            down: true,
        });
        assert_eq!(
            sent(&a),
            vec![Message::Key {
                hid: 0x04,
                down: true
            }]
        );
    }

    #[test]
    fn wrong_edge_does_nothing() {
        let mut pc = engine(Side::Right, Os::Windows);
        assert!(pc.handle(Input::LocalMove { x: 0.0, y: 540.0 }).is_empty());
        assert_eq!(pc.mode(), Mode::Local);
    }

    #[test]
    fn not_connected_does_nothing() {
        let mut e = Engine::new(
            EngineConfig {
                peer_side: Side::Right,
                swap_ctrl_cmd: true,
                share_clipboard: true,
                local_os: Os::Windows,
            },
            Rect::new(0.0, 0.0, 1920.0, 1080.0),
        );
        assert!(e.handle(Input::LocalMove { x: 1919.0, y: 10.0 }).is_empty());
    }

    #[test]
    fn controlled_side_moves_cursor_and_returns() {
        // Mac: PC steht links.
        let mut mac = engine(Side::Left, Os::MacOs);
        let a = mac.handle(Input::Remote(Message::Enter { y: 0.5 }));
        assert_eq!(mac.mode(), Mode::Controlled);
        assert!(matches!(a.last(), Some(Action::Inject(Inject::MoveTo { x, .. })) if *x == 1.0));

        let a = mac.handle(Input::Remote(Message::MouseMove { dx: 100, dy: 0 }));
        assert!(matches!(a[0], Action::Inject(Inject::MoveTo { x, .. }) if x == 101.0));

        // Zurück über die linke Kante.
        let a = mac.handle(Input::Remote(Message::MouseMove { dx: -200, dy: 0 }));
        assert!(sent(&a).iter().any(|m| matches!(m, Message::Leave { .. })));
        assert_eq!(mac.mode(), Mode::Local);
    }

    #[test]
    fn jitter_right_after_entering_does_not_bounce_back() {
        let mut mac = engine(Side::Left, Os::MacOs);
        mac.handle(Input::Remote(Message::Enter { y: 0.5 }));
        mac.handle(Input::Remote(Message::MouseMove { dx: 1, dy: 0 }));
        mac.handle(Input::Remote(Message::MouseMove { dx: -3, dy: 0 }));
        assert_eq!(mac.mode(), Mode::Controlled);
        mac.handle(Input::Remote(Message::MouseMove { dx: 20, dy: 0 }));
        let a = mac.handle(Input::Remote(Message::MouseMove { dx: -30, dy: 0 }));
        assert!(sent(&a).iter().any(|m| matches!(m, Message::Leave { .. })));
    }

    #[test]
    fn slow_push_against_edge_still_returns() {
        let mut mac = engine(Side::Left, Os::MacOs);
        mac.handle(Input::Remote(Message::Enter { y: 0.5 }));
        let mut left = false;
        for _ in 0..10 {
            let a = mac.handle(Input::Remote(Message::MouseMove { dx: -1, dy: 0 }));
            left |= sent(&a).iter().any(|m| matches!(m, Message::Leave { .. }));
        }
        assert!(left, "langsames Drücken gegen die Kante muss zurückführen");
    }

    #[test]
    fn pushing_against_edge_after_return_rearms() {
        let mut pc = engine(Side::Right, Os::Windows);
        pc.handle(Input::LocalMove {
            x: 1919.0,
            y: 500.0,
        });
        pc.handle(Input::Remote(Message::Leave { y: 0.5 }));
        assert_eq!(pc.mode(), Mode::Local);
        let mut crossed = false;
        for _ in 0..10 {
            pc.handle(Input::LocalMove {
                x: 1919.0,
                y: 500.0,
            });
            crossed |= pc.mode() == Mode::Controlling;
        }
        assert!(crossed);
    }

    #[test]
    fn full_roundtrip_between_two_engines() {
        let mut pc = engine(Side::Right, Os::Windows);
        let mut mac = engine(Side::Left, Os::MacOs);

        let mut to_mac = sent(&pc.handle(Input::LocalMove {
            x: 1919.0,
            y: 270.0,
        }));
        for m in to_mac.drain(..) {
            mac.handle(Input::Remote(m));
        }
        assert_eq!(mac.mode(), Mode::Controlled);

        let back = sent(&mac.handle(Input::Remote(Message::MouseMove { dx: -50, dy: 0 })));
        let mut actions = Vec::new();
        for m in back {
            actions.extend(pc.handle(Input::Remote(m)));
        }
        assert_eq!(pc.mode(), Mode::Local);
        let stop = actions
            .iter()
            .find_map(|a| match a {
                Action::StopCapture { x, y } => Some((*x, *y)),
                _ => None,
            })
            .unwrap();
        assert_eq!(stop.0, 1917.0);
        assert!((stop.1 - 270.0).abs() < 1.0);

        // Direkt danach am Rand: noch nicht scharf, erst nach Entfernen.
        assert!(pc
            .handle(Input::LocalMove {
                x: 1919.0,
                y: 270.0
            })
            .is_empty());
        pc.handle(Input::LocalMove {
            x: 1900.0,
            y: 270.0,
        });
        assert!(!pc
            .handle(Input::LocalMove {
                x: 1919.0,
                y: 270.0
            })
            .is_empty());
    }

    #[test]
    fn ctrl_and_cmd_are_swapped_between_different_os() {
        let mut mac = engine(Side::Left, Os::MacOs);
        mac.handle(Input::Remote(Message::Enter { y: 0.5 }));
        let a = mac.handle(Input::Remote(Message::Key {
            hid: hid::LEFT_CTRL,
            down: true,
        }));
        assert_eq!(
            a,
            vec![Action::Inject(Inject::Key {
                hid: hid::LEFT_GUI,
                down: true
            })]
        );
    }

    #[test]
    fn key_release_uses_mapping_from_key_down() {
        // #29: Tausch-Einstellung ändert sich, während Strg gehalten wird.
        let mut mac = engine(Side::Left, Os::MacOs);
        mac.handle(Input::Remote(Message::Enter { y: 0.5 }));
        let down = Message::Key {
            hid: hid::LEFT_CTRL,
            down: true,
        };
        let up = Message::Key {
            hid: hid::LEFT_CTRL,
            down: false,
        };
        let a = mac.handle(Input::Remote(down.clone()));
        assert_eq!(
            a,
            vec![Action::Inject(Inject::Key {
                hid: hid::LEFT_GUI,
                down: true
            })]
        );
        mac.set_swap_ctrl_cmd(false);
        // Wiederholung (Auto-Repeat) behält die Zuordnung.
        let a = mac.handle(Input::Remote(down));
        assert_eq!(
            a,
            vec![Action::Inject(Inject::Key {
                hid: hid::LEFT_GUI,
                down: true
            })]
        );
        let a = mac.handle(Input::Remote(up.clone()));
        assert_eq!(
            a,
            vec![Action::Inject(Inject::Key {
                hid: hid::LEFT_GUI,
                down: false
            })]
        );
        // Doppeltes Loslassen: harmlos, nutzt die aktuelle Zuordnung.
        let a = mac.handle(Input::Remote(up));
        assert_eq!(
            a,
            vec![Action::Inject(Inject::Key {
                hid: hid::LEFT_CTRL,
                down: false
            })]
        );
        // Danach gilt die neue Einstellung, und Trennen lässt nichts hängen.
        mac.handle(Input::Remote(Message::Key {
            hid: hid::LEFT_CTRL,
            down: true,
        }));
        mac.set_swap_ctrl_cmd(true);
        let a = mac.handle(Input::Disconnected);
        assert!(a.contains(&Action::Inject(Inject::Key {
            hid: hid::LEFT_CTRL,
            down: false
        })));
    }

    #[test]
    fn panic_hotkey_returns_control_and_releases_keys() {
        let mut pc = engine(Side::Right, Os::Windows);
        pc.handle(Input::LocalMove { x: 1919.0, y: 10.0 });
        pc.handle(Input::LocalKey {
            hid: hid::LEFT_CTRL,
            down: true,
        });
        pc.handle(Input::LocalKey {
            hid: hid::LEFT_ALT,
            down: true,
        });
        let a = pc.handle(Input::LocalKey {
            hid: hid::ESCAPE,
            down: true,
        });
        assert_eq!(pc.mode(), Mode::Local);
        let msgs = sent(&a);
        assert_eq!(msgs[0], Message::Release);
        assert!(msgs.contains(&Message::Key {
            hid: hid::LEFT_CTRL,
            down: false
        }));
        assert!(msgs.contains(&Message::Key {
            hid: hid::LEFT_ALT,
            down: false
        }));
        assert!(a.iter().any(|x| matches!(x, Action::StopCapture { .. })));
    }

    #[test]
    fn dragging_across_edge_requests_drag_resolution_and_hides_release() {
        let mut pc = engine(Side::Right, Os::Windows);
        pc.handle(Input::LocalButton {
            button: Button::Left,
            down: true,
        });
        let a = pc.handle(Input::LocalMove {
            x: 1919.0,
            y: 500.0,
        });
        assert!(a.contains(&Action::ResolveDrag));
        let a = pc.handle(Input::LocalButton {
            button: Button::Left,
            down: false,
        });
        assert_eq!(sent(&a), vec![Message::DragDrop]);
    }

    #[test]
    fn returning_while_dragging_cancels_drag() {
        let mut pc = engine(Side::Right, Os::Windows);
        pc.handle(Input::LocalButton {
            button: Button::Left,
            down: true,
        });
        pc.handle(Input::LocalMove {
            x: 1919.0,
            y: 500.0,
        });
        let a = pc.handle(Input::Remote(Message::Leave { y: 0.5 }));
        assert!(sent(&a).contains(&Message::DragCancel));
        assert_eq!(pc.mode(), Mode::Local);
    }

    #[test]
    fn injected_left_is_released_on_exit() {
        let mut mac = engine(Side::Left, Os::MacOs);
        mac.handle(Input::Connected {
            peer_os: Os::Windows,
        });
        mac.handle(Input::Remote(Message::Enter { y: 0.5 }));
        assert_eq!(mac.inject_left(true).len(), 1);
        assert!(mac.inject_left(true).is_empty());
        let a = mac.handle(Input::Remote(Message::Release));
        assert!(a.iter().any(|x| matches!(
            x,
            Action::Inject(Inject::Button {
                button: Button::Left,
                down: false,
                ..
            })
        )));
    }

    #[test]
    fn clipboard_is_not_echoed_back() {
        let mut mac = engine(Side::Left, Os::MacOs);
        let a = mac.handle(Input::Remote(Message::Clipboard {
            text: "hallo".into(),
        }));
        assert_eq!(a, vec![Action::SetClipboard("hallo".into())]);
        // Unser Watcher sieht den neuen Inhalt – darf nicht zurückgeschickt werden.
        assert!(mac.handle(Input::LocalClipboard("hallo".into())).is_empty());
        let a = mac.handle(Input::LocalClipboard("neu".into()));
        assert_eq!(sent(&a), vec![Message::Clipboard { text: "neu".into() }]);
    }

    #[test]
    fn inbound_clipboard_is_limited() {
        // #25
        let mut mac = engine(Side::Left, Os::MacOs);
        // Genau an der Grenze, mehrbyte-Zeichen (ä = 2 Bytes).
        let at_limit = "ä".repeat(MAX_CLIPBOARD_BYTES / 2);
        assert_eq!(at_limit.len(), MAX_CLIPBOARD_BYTES);
        let a = mac.handle(Input::Remote(Message::Clipboard {
            text: at_limit.clone(),
        }));
        assert_eq!(a, vec![Action::SetClipboard(at_limit)]);
        let over = format!("x{}", "ä".repeat(MAX_CLIPBOARD_BYTES / 2));
        assert!(mac
            .handle(Input::Remote(Message::Clipboard { text: over }))
            .is_empty());
        assert!(mac
            .handle(Input::Remote(Message::Clipboard {
                text: String::new()
            }))
            .is_empty());
        // Teilen aus: nichts wird gesetzt.
        mac.set_share_clipboard(false);
        assert!(mac
            .handle(Input::Remote(Message::Clipboard { text: "a".into() }))
            .is_empty());
    }

    #[test]
    fn layout_is_mirrored_to_peer() {
        let mut pc = engine(Side::Right, Os::Windows);
        let a = pc.handle(Input::SetPeerSide(Side::Left));
        assert_eq!(
            sent(&a),
            vec![Message::Layout {
                i_am_on_your: Side::Right
            }]
        );
        let mut mac = engine(Side::Left, Os::MacOs);
        let a = mac.handle(Input::Remote(Message::Layout {
            i_am_on_your: Side::Right,
        }));
        assert_eq!(a, vec![Action::PeerSideChanged(Side::Right)]);
        assert_eq!(mac.peer_side(), Side::Right);
    }

    #[test]
    fn disconnect_while_controlling_frees_the_mouse() {
        let mut pc = engine(Side::Right, Os::Windows);
        pc.handle(Input::LocalMove { x: 1919.0, y: 10.0 });
        let a = pc.handle(Input::Disconnected);
        assert!(a.iter().any(|x| matches!(x, Action::StopCapture { .. })));
        assert_eq!(pc.mode(), Mode::Local);
    }

    #[test]
    fn disconnect_while_controlled_releases_injected_keys() {
        let mut mac = engine(Side::Left, Os::MacOs);
        mac.handle(Input::Remote(Message::Enter { y: 0.5 }));
        mac.handle(Input::Remote(Message::Key {
            hid: 0x04,
            down: true,
        }));
        let a = mac.handle(Input::Disconnected);
        assert!(a.contains(&Action::Inject(Inject::Key {
            hid: 0x04,
            down: false
        })));
    }

    #[test]
    fn handoff_hotkey() {
        let mut pc = engine(Side::Right, Os::Windows);
        pc.handle(Input::LocalKey {
            hid: hid::LEFT_CTRL,
            down: true,
        });
        pc.handle(Input::LocalKey {
            hid: hid::LEFT_ALT,
            down: true,
        });
        let a = pc.handle(Input::LocalKey {
            hid: hid::H,
            down: true,
        });
        assert!(a.contains(&Action::RequestHandoff));
    }
}
