//! Der Dienst: verbindet Engine, Verbindung, Plattform und Oberfläche.
//!
//! Ein Thread („dc-service“) verarbeitet alle Ereignisse der Reihe nach, ein
//! zweiter („dc-link“) baut die Verbindung auf und hält sie, ein dritter
//! beobachtet die Zwischenablage.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use dc_core::files::{self, ReceiveEvent};
use dc_core::link::{self, Connection, Outbox};
use dc_core::protocol::{Handoff, TransferOutcome, PROTOCOL_VERSION};
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
    SetAutoOpen(bool),
    SendFiles(Vec<PathBuf>),
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
    /// Ziehen am Zeiger ist zu Ende; `dropped`: ein Ziel hat die Dateien genommen.
    DragEnded {
        dropped: bool,
        items: Vec<PathBuf>,
    },
    Handoff(HandoffTarget),
    Note(Option<String>),
    /// Sende-Thread fertig: alles in der Warteschlange (oder Fehler).
    TransferQueued {
        transfer: u32,
        result: Result<QueuedInfo, String>,
    },
    /// Antwort aus dem Bestätigungsdialog für eine Webseite/Datei von drüben.
    OpenApproved(PathOrUrl),
}

#[derive(Debug, Clone)]
struct QueuedInfo {
    files: usize,
    bytes: u64,
    skipped: usize,
}

#[derive(Debug, Clone)]
enum PathOrUrl {
    Path(PathBuf),
    Url(String),
}

/// Eigener Sendevorgang, der auf die Bestätigung des Empfängers wartet.
struct Pending {
    cancel: Arc<AtomicBool>,
    /// Gesetzt, sobald alles in der Warteschlange ist.
    queued: Option<(Instant, QueuedInfo)>,
}

/// So lange wartet der Sender nach dem letzten Block auf die Bestätigung.
const ACK_TIMEOUT: Duration = Duration::from_secs(60);

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
    // Zeitüberschreitungen pro Verbindungswahl nur einmal laut melden
    // (bei Bluetooth-Seriell kommen sie alle 10 s, solange drüben niemand ist).
    let mut timeout_reported_for: Option<u64> = None;
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
            Ok(s) => {
                log::debug!("{via}: Kanal offen, starte Handshake");
                s
            }
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
            Err(link::LinkError::Timeout) => {
                if timeout_reported_for != Some(gen) {
                    timeout_reported_for = Some(gen);
                    log::warn!(
                        "{via}: Verbindung offen, aber keine Antwort im Handshake (läuft Device Connector drüben, ist dort die Netzwerk-Freigabe erteilt?)"
                    );
                } else {
                    log::debug!("{via}: Handshake-Zeitüberschreitung");
                }
                continue;
            }
            Err(e) => {
                log::warn!("{via}: Handshake fehlgeschlagen: {e}");
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

fn start_input(tx: &Sender<Event>) -> Option<platform::InputHandle> {
    let tx = tx.clone();
    match platform::start_input(Box::new(move |i| {
        let _ = tx.send(Event::Input(i));
    })) {
        Ok(h) => Some(h),
        Err(e) => {
            log::debug!("Eingaben abgreifen nicht möglich: {e}");
            None
        }
    }
}

/// Abgebende Seite eines Datei-Drags.
#[derive(Debug, Clone, Copy, PartialEq)]
enum OutDrag {
    None,
    /// Markierte Dateien werden noch abgefragt.
    Resolving {
        released: bool,
        cancelled: bool,
    },
    /// DragBegin und Dateien sind unterwegs.
    Sent,
}

/// Empfangende Seite eines Datei-Drags.
struct InDrag {
    /// Übertragung, die zu diesem Ziehen gehört.
    transfer: u32,
    /// Eigener Bereitstellungsordner dieses Ziehens.
    dir: PathBuf,
    released: bool,
    cancelled: bool,
    /// Oberste bereitgestellte Einträge (sobald alle Dateien da sind).
    items: Vec<PathBuf>,
    /// Das Ziehen am Zeiger läuft.
    active: Option<platform::FileDrag>,
}

/// Elternordner für die verdeckten Bereitstellungsordner gezogener Dateien.
fn staging_root() -> PathBuf {
    std::env::temp_dir().join("DeviceConnector-drag")
}

/// Markierung, an der wir eigene Bereitstellungsordner erkennen.
const STAGING_MARKER: &str = ".device-connector-staging";

/// Neuen, privaten Bereitstellungsordner für ein Ziehen anlegen (#17).
fn create_staging_dir(transfer: u32) -> std::io::Result<PathBuf> {
    let root = staging_root();
    std::fs::create_dir_all(&root)?;
    if std::fs::symlink_metadata(&root)?.is_symlink() {
        return Err(std::io::Error::other(
            "Bereitstellungsordner ist eine Verknüpfung",
        ));
    }
    let dir = root.join(format!(
        "{}-{transfer}-{:08x}",
        std::process::id(),
        rand::random::<u32>()
    ));
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
    }
    #[cfg(not(unix))]
    std::fs::DirBuilder::new().create(&dir)?;
    std::fs::write(dir.join(STAGING_MARKER), b"")?;
    Ok(dir)
}

/// Eigene, liegen gebliebene Bereitstellungsordner älter als `age` löschen.
/// Fremde Ordner (ohne Markierung) und Verknüpfungen werden nie angefasst.
fn cleanup_staging(age: Duration) {
    let Ok(rd) = std::fs::read_dir(staging_root()) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        let Ok(meta) = std::fs::symlink_metadata(&p) else {
            continue;
        };
        if !meta.is_dir() || !p.join(STAGING_MARKER).is_file() {
            continue;
        }
        let old = meta
            .modified()
            .ok()
            .and_then(|m| m.elapsed().ok())
            .is_some_and(|el| el >= age);
        if old {
            log::debug!("Räume alten Bereitstellungsordner auf: {}", p.display());
            let _ = std::fs::remove_dir_all(&p);
        }
    }
}

/// Bereitstellungsordner entfernen, wenn nur noch die Markierung drin ist.
fn remove_staging_if_empty(dir: &Path) {
    let only_marker = std::fs::read_dir(dir)
        .map(|rd| rd.flatten().all(|e| e.file_name() == STAGING_MARKER))
        .unwrap_or(false);
    if only_marker {
        let _ = std::fs::remove_dir_all(dir);
    }
}

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
    next_transfer: u32,
    pending: HashMap<u32, Pending>,
    last_screen_check: Instant,
    input_warned: bool,
    early: Vec<(u32, Message)>,
    out_drag: OutDrag,
    in_drag: Option<InDrag>,
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
        let missing = platform::missing_permissions(false);
        if !missing.is_empty() {
            log::warn!("Fehlende Berechtigungen: {}", missing.join(", "));
        }
        cleanup_staging(Duration::from_secs(24 * 3600));
        let input = start_input(&tx);
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
            receiver: {
                let mut r = files::Receiver::new(cfg.receive_dir());
                r.set_free_space_probe(platform::free_space);
                r
            },
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
            next_transfer: rand::random::<u32>() >> 1,
            pending: HashMap::new(),
            last_screen_check: Instant::now(),
            input_warned: false,
            early: Vec::new(),
            out_drag: OutDrag::None,
            in_drag: None,
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
            self.check_ack_timeouts();
            if self.last_screen_check.elapsed() > Duration::from_secs(3) {
                self.last_screen_check = Instant::now();
                let r = platform::screen_bounds();
                self.apply(Input::ScreenChanged(r));
                // Berechtigung wurde evtl. inzwischen erteilt: erneut versuchen.
                if self.input.is_none() {
                    self.input = start_input(&self.tx);
                    if self.input.is_none() && !self.input_warned {
                        self.input_warned = true;
                        log::warn!(
                            "Maus/Tastatur weiterhin gesperrt; fehlt: {}",
                            platform::missing_permissions(false).join(", ")
                        );
                    }
                    if self.input.is_some() {
                        log::info!("Eingaben werden jetzt abgegriffen");
                        self.status.input_ok = true;
                        self.publish();
                    }
                }
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
            Action::Send(Message::DragDrop) => match self.out_drag {
                OutDrag::Resolving { cancelled, .. } => {
                    self.out_drag = OutDrag::Resolving {
                        released: true,
                        cancelled,
                    }
                }
                OutDrag::Sent => {
                    self.out_drag = OutDrag::None;
                    self.send(Message::DragDrop);
                }
                OutDrag::None => {}
            },
            Action::Send(Message::DragCancel) => match self.out_drag {
                OutDrag::Resolving { released, .. } => {
                    self.out_drag = OutDrag::Resolving {
                        released,
                        cancelled: true,
                    }
                }
                OutDrag::Sent => {
                    self.out_drag = OutDrag::None;
                    self.send(Message::DragCancel);
                }
                OutDrag::None => {}
            },
            Action::Send(m) => self.send(m),
            Action::Inject(
                ev @ dc_core::engine::Inject::Button {
                    button: dc_core::Button::Left,
                    down: false,
                    ..
                },
            ) if self.in_drag.as_ref().is_some_and(|d| d.active.is_some()) => {
                // Loslassen, das kein Ablegen ist (Maus verlässt den Rechner,
                // Notausgang, Trennung): Ziehen vorher abbrechen.
                self.cancel_incoming_drag("Ziehen abgebrochen");
                if let Some(inj) = &mut self.injector {
                    inj.inject(&ev);
                }
            }
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
                let fm = platform::file_manager_in_front();
                log::info!(
                    "Ziehen über den Rand erkannt (Dateimanager vorne: {fm}), frage markierte Dateien ab"
                );
                if fm {
                    // Ziehen hier sofort beenden, sonst bleiben Symbol und
                    // Drag-Zeiger sichtbar, während die Maus drüben ist.
                    platform::cancel_local_drag();
                }
                self.out_drag = OutDrag::Resolving {
                    released: false,
                    cancelled: false,
                };
                let tx = self.tx.clone();
                thread::spawn(move || {
                    let files = platform::selected_files();
                    if files.is_empty() {
                        log::info!("Keine markierten Dateien im vordersten Explorer/Finder-Fenster gefunden");
                        let _ = tx.send(Event::Note(Some(
                            "Ziehen: keine Dateien erkannt (nur aus Explorer-/Finder-Fenstern)"
                                .into(),
                        )));
                        let _ = tx.send(Event::DragFiles(Vec::new()));
                    } else {
                        log::info!("Ziehe {} Datei(en) rüber", files.len());
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
                self.save_config();
                self.status.peer_side = side;
                self.publish();
            }
            Action::ModeChanged(mode) => {
                log::info!("Modus: {mode:?}");
                self.status.mode = mode;
                self.publish();
            }
        }
    }

    /// Speichern; ein Fehler wird im Menü angezeigt statt nur geloggt (#6).
    fn save_config(&mut self) {
        if let Err(e) = self.cfg.save() {
            self.set_note(Some(format!("Einstellungen nicht gespeichert: {e}")));
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
                let early: Vec<Message> = std::mem::take(&mut self.early)
                    .into_iter()
                    .filter(|(i, _)| *i == id)
                    .map(|(_, m)| m)
                    .collect();
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
                for m in early {
                    self.on_message(m);
                }
            }
            Event::LinkDown { id, reason } => {
                if self.link.as_ref().map(|l| l.0) == Some(id) {
                    log::info!("Verbindung beendet: {reason}");
                    self.link = None;
                    self.receiver.abort();
                    for (_, p) in self.pending.drain() {
                        p.cancel.store(true, Ordering::SeqCst);
                    }
                    self.out_drag = OutDrag::None;
                    if self.in_drag.as_ref().is_some_and(|d| d.active.is_some()) {
                        self.cancel_incoming_drag("Getrennt während des Ziehens");
                    }
                    if let Some(d) = self.in_drag.take() {
                        remove_staging_if_empty(&d.dir);
                        let _ = std::fs::remove_dir_all(&d.dir);
                    }
                    self.apply(Input::Disconnected);
                    self.status.link = LinkState::Connecting;
                    self.status.peer_name = None;
                    self.status.note = Some(format!("Getrennt: {reason}"));
                    self.publish();
                }
            }
            Event::Net(id, m) => {
                match self.link.as_ref().map(|l| l.0) {
                    Some(cur) if cur == id => self.on_message(m),
                    // Der Lese-Thread kann schneller sein als die LinkUp-Meldung:
                    // Nachrichten der neuen Verbindung aufheben statt verwerfen.
                    cur if cur.is_none_or(|c| id > c) && self.early.len() < 1024 => {
                        self.early.push((id, m));
                    }
                    _ => {}
                }
            }
            Event::Ui(c) => self.on_ui(c),
            Event::DragFiles(paths) => {
                let state = std::mem::replace(&mut self.out_drag, OutDrag::None);
                if self.link.is_none() || paths.is_empty() {
                    return;
                }
                match state {
                    OutDrag::Resolving {
                        cancelled: true, ..
                    } => {
                        log::info!("Ziehen abgebrochen, bevor die Dateien bereit waren");
                    }
                    OutDrag::Resolving {
                        released: false, ..
                    } if self.engine.mode() == Mode::Controlling => {
                        // Drüben verdeckt bereitstellen und an den Zeiger hängen.
                        self.out_drag = OutDrag::Sent;
                        self.send_files(paths, false, true);
                    }
                    _ => {
                        // Schon losgelassen: normal in den Empfangsordner.
                        self.send_files(paths, false, false);
                    }
                }
            }
            Event::Handoff(t) => match t {
                HandoffTarget::Url(u) => {
                    self.send(Message::Handoff(Handoff::OpenUrl(u)));
                    self.set_note(Some("Webseite an den anderen Rechner übergeben".into()));
                }
                HandoffTarget::File(p) => self.send_files(vec![p], true, false),
            },
            Event::Note(n) => self.set_note(n),
            Event::TransferQueued { transfer, result } => match result {
                Ok(info) => {
                    if let Some(p) = self.pending.get_mut(&transfer) {
                        p.queued = Some((Instant::now(), info.clone()));
                        let mut note = format!(
                            "{} Datei(en) gesendet ({}), warte auf Bestätigung …",
                            info.files,
                            human(info.bytes)
                        );
                        if info.skipped > 0 {
                            note.push_str(&format!(" {} übersprungen", info.skipped));
                        }
                        self.set_note(Some(note));
                    }
                }
                Err(e) => {
                    let was_pending = self.pending.remove(&transfer).is_some();
                    if was_pending {
                        self.set_note(Some(format!("Senden abgebrochen: {e}")));
                    }
                }
            },
            Event::OpenApproved(what) => match what {
                PathOrUrl::Url(u) => {
                    if let Err(e) = opener::open(&u) {
                        log::warn!("URL öffnen: {e}");
                    }
                }
                PathOrUrl::Path(p) => {
                    if let Err(e) = opener::open(&p) {
                        log::warn!("{} öffnen: {e}", p.display());
                    }
                }
            },
            Event::DragEnded { dropped, items } => {
                let staging = items
                    .first()
                    .and_then(|p| p.parent())
                    .map(Path::to_path_buf);
                let left: Vec<PathBuf> = items.into_iter().filter(|p| p.exists()).collect();
                if dropped {
                    log::info!("Ziehen beendet: abgelegt");
                } else if !left.is_empty() {
                    log::info!("Ziehen beendet ohne Ziel");
                    self.store_drag_items(
                        &left,
                        true,
                        "Nicht abgelegt: Dateien liegen im Empfangsordner",
                    );
                }
                // Beim Ablegen kopiert das System evtl. noch im Hintergrund;
                // der Ordner wird dann später von cleanup_staging entfernt.
                if let Some(dir) = staging.filter(|d| d.join(STAGING_MARKER).is_file()) {
                    remove_staging_if_empty(&dir);
                }
            }
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
                if is_openable_url(&u) {
                    self.open_from_peer(PathOrUrl::Url(u));
                } else {
                    log::warn!("Fenster-Übergabe: ungültige Adresse verworfen");
                }
            }
            Message::TransferResult { transfer, result } => {
                let Some(p) = self.pending.remove(&transfer) else {
                    return;
                };
                p.cancel.store(true, Ordering::SeqCst);
                match result {
                    TransferOutcome::Saved { files } => {
                        log::info!("Gegenstelle hat {files} Datei(en) gespeichert");
                        let mut note = format!("{files} Datei(en) übertragen");
                        if let Some((_, info)) = &p.queued {
                            if info.skipped > 0 {
                                note.push_str(&format!(", {} übersprungen", info.skipped));
                            }
                        }
                        self.set_note(Some(note));
                    }
                    TransferOutcome::Failed(e) => {
                        log::warn!("Gegenstelle hat die Übertragung abgelehnt: {e}");
                        self.set_note(Some(format!("Übertragung fehlgeschlagen (drüben): {e}")));
                    }
                }
            }
            m @ (Message::FileStart { .. }
            | Message::FileChunk { .. }
            | Message::FileEnd { .. }
            | Message::Dir { .. }
            | Message::BatchEnd { .. }) => match self.receiver.handle(&m) {
                Some(ReceiveEvent::Progress { received, .. }) => {
                    // Nicht bei jedem Block neu zeichnen.
                    if received % (256 * 1024) < files::CHUNK as u64 {
                        self.set_note(Some(format!("Empfange Dateien … {}", human(received))));
                    }
                }
                Some(ReceiveEvent::Done {
                    transfer,
                    base,
                    files,
                    dirs,
                    open_first,
                }) => {
                    self.send(Message::TransferResult {
                        transfer,
                        result: TransferOutcome::Saved {
                            files: files.len() as u32,
                        },
                    });
                    if self
                        .in_drag
                        .as_ref()
                        .is_some_and(|d| d.transfer == transfer)
                    {
                        self.on_drag_files_ready(files, dirs, &base);
                        return;
                    }
                    let n = files.len();
                    log::info!("{n} Datei(en) empfangen in {}", base.display());
                    self.set_note(Some(format!("{n} Datei(en) empfangen")));
                    let first = files.first().or(dirs.first()).cloned();
                    match first {
                        Some(f) if open_first && files.len() == 1 => {
                            // Dokument aus der Fenster-Übergabe: erst nachfragen (#27).
                            self.open_from_peer(PathOrUrl::Path(f));
                        }
                        Some(f) => {
                            let _ = opener::reveal(&f);
                        }
                        None => {}
                    }
                }
                Some(ReceiveEvent::Failed { transfer, error }) => {
                    log::warn!(
                        "Datei empfangen ({}): {error}",
                        self.receiver.base().display()
                    );
                    self.send(Message::TransferResult {
                        transfer,
                        result: TransferOutcome::Failed(error.clone()),
                    });
                    if self
                        .in_drag
                        .as_ref()
                        .is_some_and(|d| d.transfer == transfer)
                    {
                        if let Some(mut d) = self.in_drag.take() {
                            if let Some(h) = d.active.take() {
                                h.cancel();
                            }
                            let _ = std::fs::remove_dir_all(&d.dir);
                        }
                    }
                    self.set_note(Some(format!("Dateiempfang abgebrochen: {error}")));
                }
                None => {}
            },
            Message::Pong => {}
            Message::DragBegin { transfer } => {
                cleanup_staging(Duration::from_secs(3600));
                if let Some(old) = self.in_drag.take() {
                    if old.active.is_none() {
                        let _ = std::fs::remove_dir_all(&old.dir);
                    } else {
                        self.in_drag = Some(old);
                        log::warn!("Neues Ziehen, während noch eines läuft: ignoriert");
                        return;
                    }
                }
                let dir = match create_staging_dir(transfer) {
                    Ok(d) => d,
                    Err(e) => {
                        log::warn!("Bereitstellungsordner: {e}");
                        return;
                    }
                };
                log::info!("Ziehen kommt an, Dateien werden verdeckt bereitgestellt");
                self.receiver.set_base_for(transfer, dir.clone());
                self.in_drag = Some(InDrag {
                    transfer,
                    dir,
                    released: false,
                    cancelled: false,
                    items: Vec::new(),
                    active: None,
                });
            }
            Message::DragDrop => {
                let active = self.in_drag.as_ref().is_some_and(|d| d.active.is_some());
                if active {
                    // Loslassen: das System legt die Dateien unter dem Zeiger ab.
                    self.in_drag = None;
                    log::info!("Dateien abgelegt");
                    self.set_note(None);
                    let acts = self.engine.inject_left(false);
                    for a in acts {
                        self.execute(a);
                    }
                } else if let Some(d) = &mut self.in_drag {
                    d.released = true;
                    self.set_note(Some(
                        "Losgelassen, Dateien kommen in den Empfangsordner …".into(),
                    ));
                }
            }
            Message::DragCancel => {
                let active = self.in_drag.as_ref().is_some_and(|d| d.active.is_some());
                if active {
                    self.cancel_incoming_drag("Ziehen abgebrochen");
                    let acts = self.engine.inject_left(false);
                    for a in acts {
                        self.execute(a);
                    }
                } else if let Some(d) = &mut self.in_drag {
                    d.cancelled = true;
                }
            }
            Message::Enter { y } if !platform::can_inject() => {
                // Ohne Berechtigung könnten wir die Maus hier nicht bewegen:
                // sofort zurückgeben, statt den anderen Rechner festzuhalten.
                log::warn!("Maus kommt an, darf hier aber nicht gesteuert werden (Bedienungshilfen fehlen)");
                self.send(Message::Leave { y });
                self.set_note(Some("Maus/Tastatur: Bedienungshilfen erlauben".into()));
            }
            other => self.apply(Input::Remote(other)),
        }
    }

    /// Alle Dateien eines Drags sind im verdeckten Ordner.
    fn on_drag_files_ready(&mut self, files: Vec<PathBuf>, dirs: Vec<PathBuf>, base: &Path) {
        let Some(mut d) = self.in_drag.take() else {
            return;
        };
        let mut all = files.clone();
        all.extend(dirs);
        d.items = files::top_level(&all, base);
        log::info!("{} Datei(en) für das Ziehen bereit", files.len());
        if d.items.is_empty() {
            let _ = std::fs::remove_dir_all(&d.dir);
            return;
        }
        if d.released || d.cancelled || self.engine.mode() != Mode::Controlled {
            let reveal = d.released;
            self.store_drag_items(&d.items, reveal, "Dateien liegen im Empfangsordner");
            remove_staging_if_empty(&d.dir);
            return;
        }
        let (x, y) = self.engine.cursor();
        let tx = self.tx.clone();
        let items = d.items.clone();
        let on_end = Box::new(move |dropped: bool| {
            let _ = tx.send(Event::DragEnded { dropped, items });
        });
        match platform::start_file_drag(&d.items, x, y, on_end) {
            Ok(h) => {
                d.active = Some(h);
                self.in_drag = Some(d);
                let acts = self.engine.inject_left(true);
                for a in acts {
                    self.execute(a);
                }
                self.set_note(Some(
                    "Dateien hängen am Zeiger: loslassen zum Ablegen".into(),
                ));
            }
            Err(e) => {
                log::warn!("Ziehen am Zeiger nicht möglich: {e}");
                self.store_drag_items(&d.items, true, "Dateien liegen im Empfangsordner");
                remove_staging_if_empty(&d.dir);
            }
        }
    }

    /// Laufendes Ziehen abbrechen und die Dateien in den Empfangsordner legen.
    fn cancel_incoming_drag(&mut self, why: &str) {
        let Some(mut d) = self.in_drag.take() else {
            return;
        };
        if let Some(h) = d.active.take() {
            h.cancel();
        }
        log::info!("{why}");
        let items = std::mem::take(&mut d.items);
        self.store_drag_items(
            &items,
            false,
            &format!("{why}: Dateien liegen im Empfangsordner"),
        );
        remove_staging_if_empty(&d.dir);
    }

    fn store_drag_items(&mut self, items: &[PathBuf], reveal: bool, note: &str) {
        let dir = self.cfg.receive_dir();
        let mut first = None;
        for it in items {
            match files::move_into(it, &dir) {
                Ok(p) => {
                    first.get_or_insert(p);
                }
                Err(e) => log::warn!("{} verschieben: {e}", it.display()),
            }
        }
        log::info!(
            "{} Eintrag/Einträge nach {} gelegt",
            items.len(),
            dir.display()
        );
        if reveal {
            if let Some(f) = &first {
                let _ = opener::reveal(f);
            }
        }
        self.set_note(Some(note.into()));
    }

    /// Startet einen Sendevorgang. `drag`: drüben verdeckt bereitstellen und
    /// an den Zeiger hängen (vorher `DragBegin`).
    fn send_files(&mut self, paths: Vec<PathBuf>, open_first: bool, drag: bool) {
        let Some((_, outbox, _)) = self.link.clone() else {
            return;
        };
        let transfer = self.next_transfer;
        self.next_transfer = self.next_transfer.wrapping_add(1);
        if drag {
            self.send(Message::DragBegin { transfer });
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.pending.insert(
            transfer,
            Pending {
                cancel: cancel.clone(),
                queued: None,
            },
        );
        let tx = self.tx.clone();
        thread::spawn(move || {
            let done = |result| {
                let _ = tx.send(Event::TransferQueued { transfer, result });
            };
            let c = match files::collect(&paths) {
                Ok(c) => c,
                Err(e) => {
                    // Leeren Vorgang abschließen, damit drüben nichts offen bleibt.
                    outbox.push(Message::BatchEnd {
                        transfer,
                        files: 0,
                        open_first: false,
                    });
                    return done(Err(format!("Dateien lesen: {e}")));
                }
            };
            for (p, why) in &c.skipped {
                log::warn!("Nicht gesendet: {} ({why})", p.display());
            }
            if c.is_empty() {
                outbox.push(Message::BatchEnd {
                    transfer,
                    files: 0,
                    open_first: false,
                });
                return done(Err(format!(
                    "nichts zu senden ({} übersprungen)",
                    c.skipped.len()
                )));
            }
            let total = c.total_bytes();
            let n = c.items.len();
            let mut last = Instant::now();
            let res = files::send(&outbox, &c, transfer, open_first, &cancel, |done, total| {
                if last.elapsed() > Duration::from_millis(500) {
                    last = Instant::now();
                    let pct = (done * 100).checked_div(total).unwrap_or(100);
                    let _ = tx.send(Event::Note(Some(format!("Sende {n} Datei(en) … {pct} %"))));
                }
            });
            match res {
                Ok(()) => {
                    log::info!("{n} Datei(en) gesendet ({})", human(total));
                    done(Ok(QueuedInfo {
                        files: n,
                        bytes: total,
                        skipped: c.skipped.len(),
                    }))
                }
                Err(e) => {
                    log::warn!("Senden abgebrochen: {e}");
                    done(Err(e.to_string()))
                }
            }
        });
    }

    /// Bestätigungen, die zu lange ausbleiben, als Fehler melden.
    fn check_ack_timeouts(&mut self) {
        let expired: Vec<u32> = self
            .pending
            .iter()
            .filter(|(_, p)| {
                p.queued
                    .as_ref()
                    .is_some_and(|(t, _)| t.elapsed() > ACK_TIMEOUT)
            })
            .map(|(t, _)| *t)
            .collect();
        for t in expired {
            self.pending.remove(&t);
            log::warn!("Keine Empfangsbestätigung für Übertragung {t}");
            self.set_note(Some(
                "Keine Bestätigung vom anderen Rechner: Übertragung evtl. unvollständig".into(),
            ));
        }
    }

    /// Webseite oder Dokument vom anderen Rechner öffnen: nur mit Zustimmung,
    /// außer der Nutzer hat das automatische Öffnen eingeschaltet (#27).
    fn open_from_peer(&mut self, what: PathOrUrl) {
        if self.cfg.auto_open_from_peer {
            let _ = self.tx.send(Event::OpenApproved(what));
            return;
        }
        let peer = self
            .status
            .peer_name
            .clone()
            .unwrap_or_else(|| "Der andere Rechner".into());
        let text = match &what {
            PathOrUrl::Url(u) => format!("{peer} möchte diese Webseite öffnen:\n\n{u}"),
            PathOrUrl::Path(p) => {
                format!("{peer} möchte dieses Dokument öffnen:\n\n{}", p.display())
            }
        };
        if let PathOrUrl::Path(p) = &what {
            let _ = opener::reveal(p);
        }
        let tx = self.tx.clone();
        thread::spawn(move || {
            if platform::confirm("Device Connector", &text, "Öffnen") {
                let _ = tx.send(Event::OpenApproved(what));
            }
        });
    }

    fn on_ui(&mut self, c: UiCommand) {
        match c {
            UiCommand::SetPeerSide(side) => {
                self.cfg.peer_side = side;
                self.save_config();
                self.status.peer_side = side;
                self.apply(Input::SetPeerSide(side));
                self.publish();
            }
            UiCommand::SetTransport(t) => {
                self.cfg.transport = t.clone();
                self.save_config();
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
                self.save_config();
                let mut s = self.settings.lock().unwrap();
                s.psk = pairing::derive_psk(&code);
                s.generation += 1;
            }
            UiCommand::SetShareClipboard(on) => {
                self.cfg.share_clipboard = on;
                self.save_config();
                self.engine.set_share_clipboard(on);
            }
            UiCommand::SetSwapCtrlCmd(on) => {
                self.cfg.swap_ctrl_cmd = on;
                self.save_config();
                self.engine.set_swap_ctrl_cmd(on);
            }
            UiCommand::SetAutoOpen(on) => {
                self.cfg.auto_open_from_peer = on;
                self.save_config();
            }
            UiCommand::SendFiles(paths) => {
                if self.link.is_none() {
                    self.set_note(Some("Nicht verbunden: Dateien nicht gesendet".into()));
                } else {
                    log::info!("Sende {} ausgewählte Datei(en)", paths.len());
                    self.send_files(paths, false, false);
                }
            }
            UiCommand::Quit => {}
        }
    }
}

/// Nur http(s), keine Steuerzeichen, vernünftige Länge.
fn is_openable_url(u: &str) -> bool {
    (u.starts_with("https://") || u.starts_with("http://"))
        && u.len() <= 8192
        && !u.chars().any(|c| c.is_control() || c.is_whitespace())
}

fn human(bytes: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    if bytes as f64 >= MB {
        format!("{:.1} MB", bytes as f64 / MB)
    } else {
        format!("{} KB", bytes / 1024)
    }
}
