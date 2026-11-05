//! Der Dienst: verbindet Engine, Verbindung, Plattform und Oberfläche.
//!
//! Ein Thread („dc-service“) verarbeitet alle Ereignisse der Reihe nach, ein
//! zweiter („dc-link“) baut die Verbindung auf und hält sie, ein dritter
//! beobachtet die Zwischenablage.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use dc_core::files::{self, ReceiveEvent};
use dc_core::link::{self, Connection, Outbox};
use dc_core::protocol::{Handoff, PROTOCOL_VERSION};
use dc_core::transport::{SerialTransport, TcpTransport, Transport};
use dc_core::{pairing, Action, Engine, EngineConfig, Input, Message, Mode, Os, Side};

use crate::config::{Config, TransportChoice};
use crate::platform::{self, HandoffTarget};

#[derive(Debug, Clone, PartialEq)]
pub enum LinkState {
    /// Keine Verbindung ausgewählt.
    Idle,
    Connecting,
    Connected,
    Error(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Status {
    pub link: LinkState,
    pub via: String,
    pub peer_name: Option<String>,
    pub mode: Mode,
    pub peer_side: Side,
    /// Kurzer Hinweis, z. B. Fortschritt einer Übertragung.
    pub note: Option<String>,
    pub input_ok: bool,
}

#[derive(Debug, Clone)]
pub enum UiCommand {
    SetPeerSide(Side),
    SetTransport(TransportChoice),
    SetPairingCode(String),
    SetShareClipboard(bool),
    SetSwapCtrlCmd(bool),
    Quit,
}

enum Event {
    Input(Input),
    Net(u32, Message),
    LinkUp {
        id: u32,
        outbox: Arc<Outbox>,
        initiator: bool,
        via: String,
    },
    LinkDown {
        id: u32,
        reason: String,
    },
    LinkTrying(String),
    LinkFailed(String),
    Ui(UiCommand),
    DragFiles(Vec<PathBuf>),
    Handoff(HandoffTarget),
    Note(Option<String>),
}

/// Griff für die Oberfläche.
#[derive(Clone)]
pub struct ServiceHandle {
    tx: Sender<Event>,
}

impl ServiceHandle {
    pub fn command(&self, c: UiCommand) {
        let _ = self.tx.send(Event::Ui(c));
    }
}

struct LinkSettings {
    transport: TransportChoice,
    psk: [u8; 32],
    tcp_port: u16,
    generation: u64,
}

pub fn start(
    cfg: Config,
    on_status: impl Fn(Status) + Send + 'static,
) -> (ServiceHandle, thread::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel::<Event>();
    let stop = Arc::new(AtomicBool::new(false));
    let settings = Arc::new(Mutex::new(LinkSettings {
        transport: cfg.transport.clone(),
        psk: pairing::derive_psk(&cfg.pairing_code),
        tcp_port: cfg.tcp_port,
        generation: 0,
    }));

    {
        let tx = tx.clone();
        let settings = settings.clone();
        let stop = stop.clone();
        thread::Builder::new()
            .name("dc-link".into())
            .spawn(move || link_manager(settings, tx, stop))
            .expect("Thread");
    }
    {
        let tx = tx.clone();
        let stop = stop.clone();
        thread::Builder::new()
            .name("dc-clipboard".into())
            .spawn(move || clipboard_watcher(tx, stop))
            .expect("Thread");
    }

    let handle = ServiceHandle { tx: tx.clone() };
    let join = thread::Builder::new()
        .name("dc-service".into())
        .spawn(move || {
            let mut svc = Service::new(cfg, tx, settings, Box::new(on_status));
            svc.run(rx);
            stop.store(true, Ordering::SeqCst);
        })
        .expect("Thread");
    (handle, join)
}

// ---------------------------------------------------------------- Verbindungsaufbau

fn build_transport(choice: &TransportChoice, tcp_port: u16) -> Option<Box<dyn Transport>> {
    match choice {
        TransportChoice::None => None,
        TransportChoice::Serial { port } => Some(Box::new(SerialTransport { port: port.clone() })),
        TransportChoice::TcpListen => Some(Box::new(TcpTransport::listen(tcp_port))),
        TransportChoice::TcpConnect { addr } => Some(Box::new(TcpTransport::connect(addr.clone()))),
    }
}

fn link_manager(settings: Arc<Mutex<LinkSettings>>, tx: Sender<Event>, stop: Arc<AtomicBool>) {
    let next_id = AtomicU32::new(1);
    let mut current: Option<(u64, Box<dyn Transport>)> = None;
    while !stop.load(Ordering::SeqCst) {
        let (gen, choice, psk, port) = {
            let s = settings.lock().unwrap();
            (s.generation, s.transport.clone(), s.psk, s.tcp_port)
        };
        if current.as_ref().map(|c| c.0) != Some(gen) {
            current = build_transport(&choice, port).map(|t| (gen, t));
        }
        let Some((_, transport)) = current.as_mut() else {
            thread::sleep(Duration::from_millis(300));
            continue;
        };
        let via = transport.describe();
        let _ = tx.send(Event::LinkTrying(via.clone()));

        let mut stream = match transport.open() {
            Ok(s) => s,
            Err(e) => {
                if e.kind() != std::io::ErrorKind::TimedOut {
                    log::info!("{via}: {e}");
                    let _ = tx.send(Event::LinkFailed(format!("{via}: {e}")));
                    sleep_unless_changed(&settings, gen, &stop, Duration::from_secs(3));
                }
                continue;
            }
        };
        let secured = match link::handshake(stream.as_mut(), &psk) {
            Ok(s) => s,
            Err(link::LinkError::Timeout) => continue, // Gegenstelle (noch) nicht da
            Err(e) => {
                log::warn!("Handshake: {e}");
                let _ = tx.send(Event::LinkFailed(e.to_string()));
                sleep_unless_changed(&settings, gen, &stop, Duration::from_secs(3));
                continue;
            }
        };
        let id = next_id.fetch_add(1, Ordering::SeqCst);
        let initiator = secured.initiator;
        let conn = {
            let tx_msg = tx.clone();
            let tx_down = tx.clone();
            Connection::start(
                stream,
                secured,
                move |m| {
                    let _ = tx_msg.send(Event::Net(id, m));
                },
                move |reason| {
                    let _ = tx_down.send(Event::LinkDown { id, reason });
                },
            )
        };
        let conn = match conn {
            Ok(c) => c,
            Err(e) => {
                let _ = tx.send(Event::LinkFailed(e.to_string()));
                continue;
            }
        };
        log::info!("Verbunden über {via}");
        let _ = tx.send(Event::LinkUp {
            id,
            outbox: conn.outbox.clone(),
            initiator,
            via,
        });
        while conn.is_alive()
            && !stop.load(Ordering::SeqCst)
            && settings.lock().unwrap().generation == gen
        {
            thread::sleep(Duration::from_millis(200));
        }
        let was_alive = conn.is_alive();
        conn.shutdown();
        if was_alive {
            let _ = tx.send(Event::LinkDown {
                id,
                reason: "getrennt".into(),
            });
        }
        thread::sleep(Duration::from_millis(500));
    }
}

fn sleep_unless_changed(settings: &Mutex<LinkSettings>, gen: u64, stop: &AtomicBool, d: Duration) {
    let end = Instant::now() + d;
    while Instant::now() < end
        && !stop.load(Ordering::SeqCst)
        && settings.lock().unwrap().generation == gen
    {
        thread::sleep(Duration::from_millis(100));
    }
}

// ---------------------------------------------------------------- Zwischenablage

fn clipboard_watcher(tx: Sender<Event>, stop: Arc<AtomicBool>) {
    let mut cb = match arboard::Clipboard::new() {
        Ok(c) => c,
        Err(e) => {
            log::warn!("Zwischenablage nicht verfügbar: {e}");
            return;
        }
    };
    let mut last = cb.get_text().ok();
    while !stop.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(600));
        if let Ok(t) = cb.get_text() {
            if last.as_deref() != Some(t.as_str()) {
                last = Some(t.clone());
                let _ = tx.send(Event::Input(Input::LocalClipboard(t)));
            }
        }
    }
}

// ---------------------------------------------------------------- Dienst

struct Service {
    cfg: Config,
    tx: Sender<Event>,
    settings: Arc<Mutex<LinkSettings>>,
    on_status: Box<dyn Fn(Status) + Send>,
    engine: Engine,
    input: Option<platform::InputHandle>,
    injector: Option<platform::Injector>,
    clipboard: Option<arboard::Clipboard>,
    receiver: files::Receiver,
    link: Option<(u32, Arc<Outbox>, bool)>,
    status: Status,
    next_file_id: Arc<AtomicU32>,
    last_screen_check: Instant,
}

impl Service {
    fn new(
        cfg: Config,
        tx: Sender<Event>,
        settings: Arc<Mutex<LinkSettings>>,
        on_status: Box<dyn Fn(Status) + Send>,
    ) -> Service {
        let engine = Engine::new(
            EngineConfig {
                peer_side: cfg.peer_side,
                swap_ctrl_cmd: cfg.swap_ctrl_cmd,
                share_clipboard: cfg.share_clipboard,
                local_os: Os::current(),
            },
            platform::screen_bounds(),
        );
        let input = {
            let tx = tx.clone();
            match platform::start_input(Box::new(move |i| {
                let _ = tx.send(Event::Input(i));
            })) {
                Ok(h) => Some(h),
                Err(e) => {
                    log::error!("Eingaben abgreifen nicht möglich: {e}");
                    None
                }
            }
        };
        let injector = match platform::Injector::new() {
            Ok(i) => Some(i),
            Err(e) => {
                log::error!("Eingaben abspielen nicht möglich: {e}");
                None
            }
        };
        let status = Status {
            link: if cfg.transport == TransportChoice::None {
                LinkState::Idle
            } else {
                LinkState::Connecting
            },
            via: String::new(),
            peer_name: None,
            mode: Mode::Local,
            peer_side: cfg.peer_side,
            note: None,
            input_ok: input.is_some(),
        };
        Service {
            receiver: files::Receiver::new(cfg.receive_dir()),
            cfg,
            tx,
            settings,
            on_status,
            engine,
            input,
            injector,
            clipboard: arboard::Clipboard::new().ok(),
            link: None,
            status,
            next_file_id: Arc::new(AtomicU32::new(1)),
            last_screen_check: Instant::now(),
        }
    }

    fn publish(&self) {
        (self.on_status)(self.status.clone());
    }

    fn run(&mut self, rx: Receiver<Event>) {
        self.publish();
        loop {
            match rx.recv_timeout(Duration::from_secs(1)) {
                Ok(Event::Ui(UiCommand::Quit)) => {
                    self.shutdown();
                    return;
                }
                Ok(ev) => self.on_event(ev),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
            if self.last_screen_check.elapsed() > Duration::from_secs(3) {
                self.last_screen_check = Instant::now();
                let r = platform::screen_bounds();
                self.apply(Input::ScreenChanged(r));
            }
        }
    }

    fn shutdown(&mut self) {
        if self.engine.mode() != Mode::Local {
            self.apply(Input::Disconnected);
        }
        if let Some(i) = &self.input {
            i.set_capturing(false);
        }
    }

    fn send(&self, m: Message) {
        if let Some((_, outbox, _)) = &self.link {
            outbox.push(m);
        }
    }

    fn apply(&mut self, input: Input) {
        let actions = self.engine.handle(input);
        for a in actions {
            self.execute(a);
        }
    }

    fn execute(&mut self, a: Action) {
        match a {
            Action::Send(m) => self.send(m),
            Action::StartCapture => {
                if let Some(i) = &self.input {
                    i.set_capturing(true);
                }
            }
            Action::StopCapture { x, y } => {
                if let Some(i) = &self.input {
                    i.set_capturing(false);
                    i.warp(x, y);
                }
            }
            Action::Inject(ev) => {
                if let Some(inj) = &mut self.injector {
                    inj.inject(&ev);
                }
            }
            Action::SetClipboard(text) => {
                if let Some(cb) = &mut self.clipboard {
                    if let Err(e) = cb.set_text(text) {
                        log::warn!("Zwischenablage setzen: {e}");
                    }
                }
            }
            Action::ResolveDrag => {
                let tx = self.tx.clone();
                thread::spawn(move || {
                    let files = platform::selected_files();
                    if !files.is_empty() {
                        let _ = tx.send(Event::DragFiles(files));
                    }
                });
            }
            Action::RequestHandoff => {
                let tx = self.tx.clone();
                thread::spawn(move || match platform::handoff_target() {
                    Some(t) => {
                        let _ = tx.send(Event::Handoff(t));
                    }
                    None => {
                        let _ = tx.send(Event::Note(Some(
                            "Fenster-Übergabe: für dieses Fenster nicht möglich".into(),
                        )));
                    }
                });
            }
            Action::PeerSideChanged(side) => {
                self.cfg.peer_side = side;
                self.cfg.save();
                self.status.peer_side = side;
                self.publish();
            }
            Action::ModeChanged(mode) => {
                self.status.mode = mode;
                self.publish();
            }
        }
    }

    fn set_note(&mut self, note: Option<String>) {
        if self.status.note != note {
            self.status.note = note;
            self.publish();
        }
    }

    fn on_event(&mut self, ev: Event) {
        match ev {
            Event::Input(i) => self.apply(i),
            Event::LinkTrying(via) => {
                if self.link.is_none() && self.status.via != via {
                    log::info!("Warte auf Gegenstelle über {via}");
                }
                if self.link.is_none() {
                    let changed =
                        !matches!(self.status.link, LinkState::Error(_)) || self.status.via != via;
                    if changed {
                        self.status.via = via;
                        if !matches!(self.status.link, LinkState::Error(_)) {
                            self.status.link = LinkState::Connecting;
                        }
                        self.publish();
                    }
                }
            }
            Event::LinkFailed(why) => {
                if self.link.is_none() {
                    self.status.link = LinkState::Error(why);
                    self.publish();
                }
            }
            Event::LinkUp {
                id,
                outbox,
                initiator,
                via,
            } => {
                self.link = Some((id, outbox, initiator));
                self.status.via = via;
                self.send(Message::Hello {
                    protocol: PROTOCOL_VERSION,
                    name: self.cfg.name.clone(),
                    os: Os::current(),
                    app_version: env!("CARGO_PKG_VERSION").into(),
                });
                if initiator {
                    // Nur eine Seite gibt die Anordnung vor, sonst tauschen beide.
                    self.send(Message::Layout {
                        i_am_on_your: self.cfg.peer_side.opposite(),
                    });
                }
            }
            Event::LinkDown { id, reason } => {
                if self.link.as_ref().map(|l| l.0) == Some(id) {
                    log::info!("Verbindung beendet: {reason}");
                    self.link = None;
                    self.receiver.abort();
                    self.apply(Input::Disconnected);
                    self.status.link = LinkState::Connecting;
                    self.status.peer_name = None;
                    self.status.note = Some(format!("Getrennt: {reason}"));
                    self.publish();
                }
            }
            Event::Net(id, m) => {
                if self.link.as_ref().map(|l| l.0) != Some(id) {
                    return;
                }
                self.on_message(m);
            }
            Event::Ui(c) => self.on_ui(c),
            Event::DragFiles(paths) => {
                if self.link.is_none() {
                    return;
                }
                platform::cancel_local_drag();
                self.send_files(paths, false);
            }
            Event::Handoff(t) => match t {
                HandoffTarget::Url(u) => {
                    self.send(Message::Handoff(Handoff::OpenUrl(u)));
                    self.set_note(Some("Webseite an den anderen Rechner übergeben".into()));
                }
                HandoffTarget::File(p) => self.send_files(vec![p], true),
            },
            Event::Note(n) => self.set_note(n),
        }
    }

    fn on_message(&mut self, m: Message) {
        match m {
            Message::Hello {
                protocol,
                name,
                os,
                app_version,
            } => {
                if protocol != PROTOCOL_VERSION {
                    self.status.link = LinkState::Error(format!(
                        "Andere Version am Gegenüber ({app_version}); bitte beide aktualisieren"
                    ));
                    self.publish();
                    return;
                }
                log::info!("Gegenstelle: {name} ({os:?}, v{app_version})");
                self.status.link = LinkState::Connected;
                self.status.peer_name = Some(name);
                self.status.note = None;
                self.apply(Input::Connected { peer_os: os });
                self.publish();
            }
            Message::Handoff(Handoff::OpenUrl(u)) => {
                if u.starts_with("https://") || u.starts_with("http://") {
                    if let Err(e) = opener::open(&u) {
                        log::warn!("URL öffnen: {e}");
                    }
                }
            }
            m @ (Message::FileStart { .. }
            | Message::FileChunk { .. }
            | Message::FileEnd { .. }
            | Message::BatchEnd { .. }) => match self.receiver.handle(&m) {
                Ok(Some(ReceiveEvent::Progress { received })) => {
                    // Nicht bei jedem Block neu zeichnen.
                    if received % (256 * 1024) < files::CHUNK as u64 {
                        self.set_note(Some(format!("Empfange Dateien … {}", human(received))));
                    }
                }
                Ok(Some(ReceiveEvent::Done { files, open_first })) => {
                    let n = files.len();
                    self.set_note(Some(format!("{n} Datei(en) empfangen")));
                    let target = if open_first {
                        files.first().cloned()
                    } else {
                        None
                    };
                    match target {
                        Some(f) => {
                            let _ = opener::open(&f);
                        }
                        None => {
                            if let Some(f) = files.first() {
                                let _ = opener::reveal(f);
                            }
                        }
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    log::warn!("Datei empfangen: {e}");
                    self.receiver.abort();
                    self.set_note(Some(format!("Dateiempfang abgebrochen: {e}")));
                }
            },
            Message::Pong => {}
            other => self.apply(Input::Remote(other)),
        }
    }

    fn send_files(&mut self, paths: Vec<PathBuf>, open_first: bool) {
        let Some((_, outbox, _)) = self.link.clone() else {
            return;
        };
        let tx = self.tx.clone();
        let ids = self.next_file_id.clone();
        thread::spawn(move || {
            let items = match files::collect(&paths) {
                Ok(i) if !i.is_empty() => i,
                Ok(_) => return,
                Err(e) => {
                    let _ = tx.send(Event::Note(Some(format!("Dateien lesen: {e}"))));
                    return;
                }
            };
            let first = ids.fetch_add(items.len() as u32, Ordering::SeqCst);
            let total: u64 = items.iter().map(|i| i.size).sum();
            let n = items.len();
            let mut last = Instant::now();
            let res = files::send(&outbox, &items, first, open_first, |done, total| {
                if last.elapsed() > Duration::from_millis(500) {
                    last = Instant::now();
                    let pct = (done * 100).checked_div(total).unwrap_or(100);
                    let _ = tx.send(Event::Note(Some(format!("Sende {n} Datei(en) … {pct} %"))));
                }
            });
            let note = match res {
                Ok(()) => format!("{n} Datei(en) gesendet ({})", human(total)),
                Err(e) => format!("Senden abgebrochen: {e}"),
            };
            let _ = tx.send(Event::Note(Some(note)));
        });
    }

    fn on_ui(&mut self, c: UiCommand) {
        match c {
            UiCommand::SetPeerSide(side) => {
                self.cfg.peer_side = side;
                self.cfg.save();
                self.status.peer_side = side;
                self.apply(Input::SetPeerSide(side));
                self.publish();
            }
            UiCommand::SetTransport(t) => {
                self.cfg.transport = t.clone();
                self.cfg.save();
                {
                    let mut s = self.settings.lock().unwrap();
                    s.transport = t.clone();
                    s.generation += 1;
                }
                self.status.link = if t == TransportChoice::None {
                    LinkState::Idle
                } else {
                    LinkState::Connecting
                };
                self.status.note = None;
                self.publish();
            }
            UiCommand::SetPairingCode(code) => {
                self.cfg.pairing_code = code.clone();
                self.cfg.save();
                let mut s = self.settings.lock().unwrap();
                s.psk = pairing::derive_psk(&code);
                s.generation += 1;
            }
            UiCommand::SetShareClipboard(on) => {
                self.cfg.share_clipboard = on;
                self.cfg.save();
                self.engine.set_share_clipboard(on);
            }
            UiCommand::SetSwapCtrlCmd(on) => {
                self.cfg.swap_ctrl_cmd = on;
                self.cfg.save();
                self.engine.set_swap_ctrl_cmd(on);
            }
            UiCommand::Quit => {}
        }
    }
}

fn human(bytes: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    if bytes as f64 >= MB {
        format!("{:.1} MB", bytes as f64 / MB)
    } else {
        format!("{} KB", bytes / 1024)
    }
}
