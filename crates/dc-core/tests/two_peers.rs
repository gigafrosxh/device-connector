//! Zwei Rechner im Test (#31): zwei Engines, dazwischen eine simulierte
//! Leitung, und je ein simuliertes Betriebssystem, das mitschreibt, welche
//! Tasten es für gedrückt hält und ob der Zeiger abgefangen ist.
//!
//! So lassen sich die Abläufe aus #2/#3 deterministisch nachstellen, ohne
//! echte Hardware. Dazu ein Durchlauf über echte, verschlüsselte TCP-
//! Verbindungen mit Dateiübertragung und Empfangsbestätigung.

use std::collections::{BTreeSet, VecDeque};

use dc_core::engine::Inject;
use dc_core::keymap::hid;
use dc_core::{Action, Button, Engine, EngineConfig, Input, Message, Mode, Os, Rect, Side};

/// Was das Betriebssystem eines Rechners „sieht“.
#[derive(Default, Debug)]
struct OsState {
    captured: bool,
    cursor: (f64, f64),
    buttons: BTreeSet<u8>,
    keys: BTreeSet<u16>,
}

struct Machine {
    engine: Engine,
    os: OsState,
    /// Physisch gedrückte Tasten am eigenen Gerät.
    physical_buttons: BTreeSet<u8>,
}

struct Sim {
    pc: Machine,
    mac: Machine,
    /// (an_mac, Nachricht)
    wire: VecDeque<(bool, Message)>,
}

impl Machine {
    fn new(os: Os, side: Side, screen: Rect) -> Machine {
        Machine {
            engine: Engine::new(
                EngineConfig {
                    peer_side: side,
                    swap_ctrl_cmd: true,
                    share_clipboard: true,
                    local_os: os,
                    align: Default::default(),
                },
                screen,
            ),
            os: OsState::default(),
            physical_buttons: BTreeSet::new(),
        }
    }
}

impl Sim {
    fn new() -> Sim {
        let mut s = Sim {
            pc: Machine::new(Os::Windows, Side::Right, Rect::new(0.0, 0.0, 1920.0, 1080.0)),
            mac: Machine::new(Os::MacOs, Side::Left, Rect::new(0.0, 0.0, 1440.0, 900.0)),
            wire: VecDeque::new(),
        };
        s.feed(true, Input::Connected { peer_os: Os::MacOs });
        s.feed(false, Input::Connected { peer_os: Os::Windows });
        s.pump();
        s
    }

    fn m(&mut self, pc: bool) -> &mut Machine {
        if pc {
            &mut self.pc
        } else {
            &mut self.mac
        }
    }

    /// Eingabe an die Engine von `pc` (true) oder Mac (false), Aktionen ausführen.
    fn feed(&mut self, pc: bool, input: Input) {
        let actions = self.m(pc).engine.handle(input);
        for a in actions {
            self.act(pc, a);
        }
    }

    fn act(&mut self, pc: bool, a: Action) {
        let m = self.m(pc);
        match a {
            Action::Send(msg) => self.wire.push_back((pc, msg)),
            Action::StartCapture => m.os.captured = true,
            Action::StopCapture { x, y } => {
                m.os.captured = false;
                m.os.cursor = (x, y);
            }
            Action::Inject(Inject::MoveTo { x, y, .. }) => m.os.cursor = (x, y),
            Action::Inject(Inject::Button { button, down, .. }) => {
                if down {
                    m.os.buttons.insert(button.bit());
                } else {
                    m.os.buttons.remove(&button.bit());
                }
            }
            Action::Inject(Inject::Key { hid, down }) => {
                if down {
                    m.os.keys.insert(hid);
                } else {
                    m.os.keys.remove(&hid);
                }
            }
            Action::ReleaseLocal { button, .. } => {
                m.os.buttons.remove(&button.bit());
            }
            _ => {}
        }
    }

    /// Alle Nachrichten zustellen, bis die Leitung leer ist.
    fn pump(&mut self) {
        let mut guard = 0;
        while let Some((from_pc, msg)) = self.wire.pop_front() {
            guard += 1;
            assert!(guard < 10_000, "Nachrichten-Pingpong ohne Ende");
            self.feed(!from_pc, Input::Remote(msg));
        }
    }

    /// Physische Mausbewegung am PC (absolut, solange nicht abgefangen).
    fn pc_move_to(&mut self, x: f64, y: f64) {
        if self.pc.os.captured {
            let (cx, cy) = self.pc.os.cursor;
            self.feed(true, Input::LocalDelta { dx: x - cx, dy: y - cy });
        } else {
            let r = self.pc.engine.screen();
            let p = (x.clamp(r.x, r.right()), y.clamp(r.y, r.bottom()));
            self.pc.os.cursor = p;
            self.feed(true, Input::LocalMove { x: p.0, y: p.1 });
        }
        self.pump();
    }

    fn pc_delta(&mut self, dx: f64, dy: f64) {
        if self.pc.os.captured {
            self.feed(true, Input::LocalDelta { dx, dy });
        } else {
            let (x, y) = self.pc.os.cursor;
            let r = self.pc.engine.screen();
            let p = ((x + dx).clamp(r.x, r.right()), (y + dy).clamp(r.y, r.bottom()));
            self.pc.os.cursor = p;
            self.feed(true, Input::LocalMove { x: p.0, y: p.1 });
        }
        self.pump();
    }

    fn pc_button(&mut self, b: Button, down: bool) {
        if down {
            self.pc.physical_buttons.insert(b.bit());
        } else {
            self.pc.physical_buttons.remove(&b.bit());
        }
        // Nicht abgefangen: das System sieht die Taste selbst.
        if !self.pc.os.captured {
            if down {
                self.pc.os.buttons.insert(b.bit());
            } else {
                self.pc.os.buttons.remove(&b.bit());
            }
        }
        self.feed(true, Input::LocalButton { button: b, down });
        self.pump();
    }

    fn pc_key(&mut self, h: u16, down: bool) {
        if !self.pc.os.captured {
            if down {
                self.pc.os.keys.insert(h);
            } else {
                self.pc.os.keys.remove(&h);
            }
        }
        self.feed(true, Input::LocalKey { hid: h, down });
        self.pump();
    }

    /// Nichts darf hängen bleiben: Systemzustand passt zu den physischen Tasten.
    fn assert_nothing_stuck(&self) {
        // Das System darf keine Taste halten, die physisch los ist. (Umgekehrt
        // ist harmlos: drüben gedrückt und dann zurück – das System sieht
        // später nur ein Loslassen.)
        assert!(
            self.pc.os.buttons.is_subset(&self.pc.physical_buttons),
            "PC-System hält Maustasten, die physisch los sind: {:?} vs. {:?}",
            self.pc.os.buttons,
            self.pc.physical_buttons
        );
        assert!(self.mac.os.buttons.is_empty(), "Mac hält Maustasten: {:?}", self.mac.os.buttons);
        assert!(self.mac.os.keys.is_empty(), "Mac hält Tasten: {:?}", self.mac.os.keys);
    }

    fn cross_to_mac(&mut self, y: f64) {
        self.pc_move_to(1000.0, y);
        self.pc_move_to(1919.0, y);
        assert_eq!(self.pc.engine.mode(), Mode::Controlling);
        assert_eq!(self.mac.engine.mode(), Mode::Controlled);
        assert!(self.pc.os.captured);
    }

    fn return_to_pc(&mut self) {
        // Erst etwas nach rechts (Rückweg scharf machen), dann zurück.
        self.pc_delta(50.0, 0.0);
        self.pc_delta(-3000.0, 0.0);
        assert_eq!(self.pc.engine.mode(), Mode::Local);
        assert_eq!(self.mac.engine.mode(), Mode::Local);
        assert!(!self.pc.os.captured);
    }
}

#[test]
fn cross_move_and_return() {
    let mut s = Sim::new();
    s.cross_to_mac(540.0);
    // Einstieg auf halber Höhe am linken Mac-Rand.
    let (x, y) = s.mac.os.cursor;
    assert!(x <= 2.0 && (y - 449.5).abs() < 1.0, "Einstieg bei {x},{y}");
    s.pc_delta(300.0, -100.0);
    let (x, _) = s.mac.os.cursor;
    assert!(x > 290.0);
    s.return_to_pc();
    // Zurück am rechten PC-Rand, Höhe entsprechend der Mac-Höhe.
    let (x, _) = s.pc.os.cursor;
    assert!(x > 1900.0);
    s.assert_nothing_stuck();
}

#[test]
fn repeated_crossings_stay_consistent() {
    let mut s = Sim::new();
    for i in 0..50 {
        s.cross_to_mac(100.0 + i as f64 * 15.0);
        s.pc_delta(20.0, 5.0);
        s.return_to_pc();
        // Nach der Rückkehr kurz wegbewegen, dann ist der Rand wieder scharf.
        s.pc_delta(-20.0, 0.0);
    }
    s.assert_nothing_stuck();
}

#[test]
fn slow_push_against_edge_after_return_crosses_again() {
    let mut s = Sim::new();
    s.cross_to_mac(500.0);
    s.return_to_pc();
    // Am Rand weiter dagegen drücken (ohne wegzubewegen): nach ein paar
    // Bewegungen wechselt der Zeiger wieder (kein „Hängenbleiben“, #2).
    for _ in 0..10 {
        s.pc_delta(1.0, 0.0);
        if s.pc.engine.mode() == Mode::Controlling {
            break;
        }
    }
    assert_eq!(s.pc.engine.mode(), Mode::Controlling);
}

#[test]
fn button_held_while_crossing_is_not_stuck() {
    // #2: Rechte Taste über den Rand gehalten und drüben losgelassen.
    let mut s = Sim::new();
    s.pc_move_to(1000.0, 500.0);
    s.pc_button(Button::Right, true);
    s.pc_move_to(1919.0, 500.0);
    assert_eq!(s.pc.engine.mode(), Mode::Controlling);
    s.pc_button(Button::Right, false);
    s.pc_delta(100.0, 0.0);
    s.return_to_pc();
    s.assert_nothing_stuck();
}

#[test]
fn keys_and_buttons_released_on_return_and_disconnect() {
    let mut s = Sim::new();
    s.pc_key(hid::LEFT_SHIFT, true);
    s.cross_to_mac(300.0);
    // Shift kam mit rüber.
    assert!(!s.mac.os.keys.is_empty());
    s.pc_key(0x04, true);
    s.pc_button(Button::Left, true);
    // Zurück, während alles gehalten wird.
    s.return_to_pc();
    s.assert_nothing_stuck();

    s.pc_key(0x04, false);
    s.pc_button(Button::Left, false);
    s.pc_key(hid::LEFT_SHIFT, false);
    s.pc_delta(-20.0, 0.0);
    s.cross_to_mac(300.0);
    s.pc_key(0x05, true);
    s.pc_button(Button::Middle, true);
    // Verbindung bricht ab.
    s.feed(true, Input::Disconnected);
    s.feed(false, Input::Disconnected);
    s.wire.clear();
    assert_eq!(s.pc.engine.mode(), Mode::Local);
    assert!(!s.pc.os.captured);
    assert!(s.mac.os.keys.is_empty() && s.mac.os.buttons.is_empty());
}

#[test]
fn panic_hotkey_frees_mouse() {
    let mut s = Sim::new();
    s.cross_to_mac(500.0);
    s.pc_key(hid::LEFT_CTRL, true);
    s.pc_key(hid::LEFT_ALT, true);
    s.pc_key(hid::ESCAPE, true);
    assert_eq!(s.pc.engine.mode(), Mode::Local);
    assert_eq!(s.mac.engine.mode(), Mode::Local);
    assert!(!s.pc.os.captured);
    s.pc_key(hid::ESCAPE, false);
    s.pc_key(hid::LEFT_ALT, false);
    s.pc_key(hid::LEFT_CTRL, false);
    s.assert_nothing_stuck();
}

#[test]
fn file_drag_sequence_drop_and_return() {
    // #3: Ziehen Richtung Rand → Vorab-Übertragung, Überqueren → Abbruch
    // hier vor dem Versetzen, Loslassen drüben → Ablegen dort.
    let mut s = Sim::new();
    s.pc_move_to(800.0, 500.0);
    s.pc_button(Button::Left, true);
    let a = s.pc.engine.handle(Input::LocalMove { x: 1800.0, y: 500.0 });
    assert!(a.contains(&Action::PrefetchDrag));
    s.pc.os.cursor = (1800.0, 500.0);
    s.pc_move_to(1919.0, 500.0);
    assert_eq!(s.pc.engine.mode(), Mode::Controlling);
    // Loslassen drüben: Mac bekommt DragDrop (App legt ab), PC-System löst die Taste.
    s.pc_button(Button::Left, false);
    s.assert_nothing_stuck();
}

// ---------------------------------------------------------------- Über echte Verbindungen

mod over_tcp {
    use std::fs;
    use std::net::{TcpListener, TcpStream};
    use std::path::PathBuf;
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use dc_core::files::{self, ReceiveEvent, Receiver};
    use dc_core::link::{handshake, Connection};
    use dc_core::pairing::derive_psk;
    use dc_core::transport::Duplex;
    use dc_core::{Message, TransferOutcome};

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("dc-e2e-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn connect() -> (Connection, mpsc::Receiver<Message>, Connection, mpsc::Receiver<Message>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let a = TcpStream::connect(l.local_addr().unwrap()).unwrap();
        let (b, _) = l.accept().unwrap();
        let psk = derive_psk("K7QM-3XPA-9TRW");
        let tb = thread::spawn(move || {
            let mut s: Box<dyn Duplex> = Box::new(b);
            let sec = handshake(s.as_mut(), &psk).unwrap();
            (s, sec)
        });
        let mut sa: Box<dyn Duplex> = Box::new(a);
        let seca = handshake(sa.as_mut(), &psk).unwrap();
        let (sb, secb) = tb.join().unwrap();
        let (txa, rxa) = mpsc::channel();
        let (txb, rxb) = mpsc::channel();
        let ca = Connection::start(sa, seca, move |m| drop(txa.send(m)), |_| {}).unwrap();
        let cb = Connection::start(sb, secb, move |m| drop(txb.send(m)), |_| {}).unwrap();
        (ca, rxa, cb, rxb)
    }

    #[test]
    fn folder_transfer_with_ack_and_checksums() {
        let src = tmpdir("src");
        fs::create_dir_all(src.join("Projekt/leer")).unwrap();
        let data: Vec<u8> = (0..300_000u32).map(|i| (i * 7 % 251) as u8).collect();
        fs::write(src.join("Projekt/gross.bin"), &data).unwrap();
        fs::write(src.join("Projekt/klein.txt"), b"hallo").unwrap();
        let dst = tmpdir("dst");

        let (pc, pc_rx, mac, mac_rx) = connect();
        let c = files::collect(&[src.join("Projekt")]).unwrap();
        let cancel = AtomicBool::new(false);
        let sender = {
            let outbox = pc.outbox.clone();
            thread::spawn(move || files::send(&outbox, &c, 77, false, &cancel, |_, _| {}))
        };
        // Gleichzeitig Eingaben: dürfen nicht verloren gehen.
        for i in 0..200 {
            pc.send(Message::Key {
                hid: 4 + (i % 20) as u16,
                down: i % 2 == 0,
            });
        }

        let mut rx = Receiver::new(dst.clone());
        let mut keys = 0;
        loop {
            let m = mac_rx.recv_timeout(Duration::from_secs(10)).expect("Nachricht");
            match &m {
                Message::Key { .. } => keys += 1,
                _ => {}
            }
            match rx.handle(&m) {
                Some(ReceiveEvent::Done { transfer, files, .. }) => {
                    mac.send(Message::TransferResult {
                        transfer,
                        result: TransferOutcome::Saved {
                            files: files.len() as u32,
                        },
                    });
                    break;
                }
                Some(ReceiveEvent::Failed { error, .. }) => panic!("{error}"),
                _ => {}
            }
        }
        sender.join().unwrap().unwrap();
        // Bestätigung kommt beim Sender an.
        let ack = loop {
            match pc_rx.recv_timeout(Duration::from_secs(5)).expect("Bestätigung") {
                Message::TransferResult { transfer, result } => break (transfer, result),
                _ => continue,
            }
        };
        assert_eq!(ack, (77, TransferOutcome::Saved { files: 2 }));
        assert_eq!(fs::read(dst.join("Projekt/gross.bin")).unwrap(), data);
        assert_eq!(fs::read(dst.join("Projekt/klein.txt")).unwrap(), b"hallo");
        assert!(dst.join("Projekt/leer").is_dir());
        // Alle Eingaben kamen vor dem Ende der Übertragung bzw. dazwischen an.
        while let Ok(m) = mac_rx.try_recv() {
            if matches!(m, Message::Key { .. }) {
                keys += 1;
            }
        }
        assert_eq!(keys, 200);
        pc.shutdown();
        mac.shutdown();
    }
}
