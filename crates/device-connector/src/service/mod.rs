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
use dc_core::link::{self, Closer, Connection, Outbox};
use dc_core::protocol::{Handoff, TransferOutcome, PROTOCOL_VERSION};
use dc_core::transport::{SerialTransport, TcpTransport, Transport};
use dc_core::{pairing, Action, Align, Engine, EngineConfig, Input, Message, Mode, Os, Side};

use crate::config::{Config, TransportChoice};
use crate::platform::{self, HandoffTarget};
use crate::trace;

mod actions;
mod clipboard;
mod commands;
mod connection;
mod drag;
mod messages;
mod transfer;

use clipboard::clipboard_watcher;
use connection::link_manager;
use drag::*;
use transfer::*;

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
    pub align: Align,
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
    SetTrace(bool),
    SetAlign(Align),
    SendFiles(Vec<PathBuf>),
    /// Anordnungsfenster öffnen (#36).
    EditLayout,
    /// Ergebnis aus dem Anordnungsfenster.
    ApplyLayout(crate::arrange::Choice),
    Quit,
}

enum Event {
    Input(Input),
    Net(u32, Message),
    LinkUp {
        id: u32,
        outbox: Arc<Outbox>,
        close: Closer,
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

/// Die aktuelle Verbindung aus Sicht des Dienstes.
struct LinkRef {
    id: u32,
    outbox: Arc<Outbox>,
    close: Closer,
    /// Hello der Gegenstelle geprüft und kompatibel: erst dann zählen
    /// alle anderen Nachrichten (#24).
    hello_ok: bool,
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
    link: Option<LinkRef>,
    /// Letzte Meldung über eine inkompatible Gegenstelle (bleibt über
    /// Wiederverbindungsversuche hinweg sichtbar, #24).
    incompatible: Option<String>,
    status: Status,
    next_transfer: u32,
    pending: HashMap<u32, Pending>,
    last_screen_check: Instant,
    input_warned: bool,
    early: Vec<(u32, Message)>,
    out_drag: OutDrag,
    in_drag: Option<InDrag>,
    /// Zähler für die Diagnose: aufgezeichnete Bewegungen seit dem Betreten.
    trace_moves: u32,
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
                align: cfg.screen_align,
            },
            platform::screen_bounds(),
        );
        let missing = platform::missing_permissions(false);
        if !missing.is_empty() {
            log::warn!("Fehlende Berechtigungen: {}", missing.join(", "));
        }
        cleanup_staging(Duration::from_secs(24 * 3600));
        trace::set_enabled(
            cfg.trace_handoff || std::env::var("DC_TRACE").is_ok_and(|v| v != "0"),
            &crate::config::config_dir(),
        );
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
            align: cfg.screen_align,
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
            incompatible: None,
            status,
            next_transfer: rand::random::<u32>() >> 1,
            pending: HashMap::new(),
            last_screen_check: Instant::now(),
            input_warned: false,
            early: Vec::new(),
            out_drag: OutDrag::None,
            in_drag: None,
            trace_moves: 0,
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
        if let Some(l) = &self.link {
            l.outbox.push(m);
        }
    }

    fn apply(&mut self, input: Input) {
        let actions = self.engine.handle(input);
        for a in actions {
            self.execute(a);
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
                close,
                initiator,
                via,
            } => {
                self.link = Some(LinkRef {
                    id,
                    outbox,
                    close,
                    hello_ok: false,
                });
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
                        align: self.cfg.screen_align,
                    });
                }
                for m in early {
                    self.on_message(m);
                }
            }
            Event::LinkDown { id, reason } => {
                if self.link.as_ref().map(|l| l.id) == Some(id) {
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
                    self.status.link = match &self.incompatible {
                        Some(why) => LinkState::Error(why.clone()),
                        None => LinkState::Connecting,
                    };
                    self.status.peer_name = None;
                    self.status.note = Some(format!("Getrennt: {reason}"));
                    self.publish();
                }
            }
            Event::Net(id, m) => {
                match self.link.as_ref().map(|l| l.id) {
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
                    if paths.is_empty() && matches!(state, OutDrag::Resolving { crossed: true, .. })
                    {
                        self.set_note(Some(
                            "Ziehen: keine Dateien erkannt (nur aus Explorer-/Finder-Fenstern)"
                                .into(),
                        ));
                    }
                    return;
                }
                match state {
                    OutDrag::Resolving {
                        cancelled: true, ..
                    } => {
                        log::info!("Ziehen abgebrochen, bevor die Dateien bereit waren");
                    }
                    OutDrag::Resolving {
                        released: false,
                        crossed,
                        ..
                    } => {
                        // Drüben verdeckt bereitstellen und an den Zeiger hängen.
                        if let Some(transfer) = self.send_files(paths, false, true) {
                            if crossed && self.engine.mode() != Mode::Controlling {
                                // Ziehen ging aus dem gesteuerten Zustand zum
                                // anderen Rechner zurück: der übernimmt es.
                                self.out_drag = OutDrag::None;
                            } else {
                                self.out_drag = OutDrag::Sent { transfer, crossed };
                            }
                        }
                    }
                    OutDrag::Resolving {
                        released: true,
                        crossed: true,
                        ..
                    } => {
                        // Drüben schon losgelassen: normal in den Empfangsordner.
                        self.send_files(paths, false, false);
                    }
                    _ => {}
                }
            }
            Event::Handoff(t) => match t {
                HandoffTarget::Url(u) => {
                    self.send(Message::Handoff(Handoff::OpenUrl(u)));
                    self.set_note(Some("Webseite an den anderen Rechner übergeben".into()));
                }
                HandoffTarget::File(p) => {
                    self.send_files(vec![p], true, false);
                }
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
                if self
                    .in_drag
                    .as_ref()
                    .is_some_and(|d| d.reverse && d.active.is_some())
                {
                    self.in_drag = None;
                    self.set_note(None);
                }
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
}

/// Kurzname einer Nachricht für die Diagnose (ohne Inhalt).
fn msg_name(m: &Message) -> &'static str {
    match m {
        Message::Enter { .. } => "Enter",
        Message::Leave { .. } => "Leave",
        Message::Release => "Release",
        _ => "?",
    }
}
