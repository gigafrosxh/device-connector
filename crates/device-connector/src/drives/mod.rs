//! Laufwerke beider Rechner gegenseitig sichtbar machen.
//!
//! * Freigeben: Anfragen des anderen Rechners ([`FsOp`]) werden in eigenen
//!   Threads gegen die lokalen Laufwerke ausgeführt, damit Plattenzugriffe
//!   nie Maus und Tastatur aufhalten.
//! * Anzeigen: Ein WebDAV-Server auf 127.0.0.1 ([`dav`]) leitet Zugriffe
//!   über die Verbindung weiter; [`mount`] bindet ihn als Laufwerk ein
//!   (Finder-Seitenleiste bzw. eigener Buchstabe im Explorer).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use dc_core::link::Outbox;
use dc_core::remotefs::{self, FsData, FsError, FsErrorKind, FsOp, FsResult, Resolved, Volume};
use dc_core::Message;

pub mod dav;
mod mount;
mod volumes;

/// Bevorzugter Port des WebDAV-Servers (gleiche Adresse über Neustarts).
pub const DAV_PORT: u16 = dc_core::transport::DEFAULT_TCP_PORT + 1;
const SHARE_WORKERS: usize = 4;
const VOLUME_CACHE: Duration = Duration::from_secs(3);

pub type NoteFn = Arc<dyn Fn(String) + Send + Sync>;

// ---------------------------------------------------------------- Anfragen an drüben

/// Schickt Anfragen über die Verbindung und ordnet die Antworten zu.
#[derive(Default)]
pub struct Client {
    outbox: Mutex<Option<Arc<Outbox>>>,
    pending: Mutex<HashMap<u32, Sender<FsResult>>>,
    next: AtomicU32,
}

fn unavailable() -> FsError {
    FsError::new(FsErrorKind::Unavailable, "nicht verbunden")
}

impl Client {
    fn set_link(&self, outbox: Option<Arc<Outbox>>) {
        *self.outbox.lock().unwrap() = outbox;
        for (_, tx) in self.pending.lock().unwrap().drain() {
            let _ = tx.send(Err(unavailable()));
        }
    }

    fn deliver(&self, id: u32, result: FsResult) {
        if let Some(tx) = self.pending.lock().unwrap().remove(&id) {
            let _ = tx.send(result);
        }
    }
}

impl dav::Backend for Client {
    fn submit(&self, op: FsOp) -> Receiver<FsResult> {
        let (tx, rx) = mpsc::channel();
        let Some(outbox) = self.outbox.lock().unwrap().clone() else {
            let _ = tx.send(Err(unavailable()));
            return rx;
        };
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        self.pending.lock().unwrap().insert(id, tx);
        // Kann bei vielen Dateiblöcken kurz warten (nie im Dienst-Thread).
        outbox.push(Message::FsRequest { id, op });
        rx
    }
}

// ---------------------------------------------------------------- Freigeben

struct ShareState {
    enabled: AtomicBool,
    cache: Mutex<Option<(Instant, Vec<Volume>)>>,
    /// Eigene Einbindung der Laufwerke von drüben: nicht wieder zurückgeben.
    hidden: Mutex<Option<PathBuf>>,
}

impl ShareState {
    fn volumes(&self) -> Vec<Volume> {
        let mut g = self.cache.lock().unwrap();
        if let Some((t, v)) = &*g {
            if t.elapsed() < VOLUME_CACHE {
                return v.clone();
            }
        }
        let v = volumes::list();
        *g = Some((Instant::now(), v.clone()));
        v
    }

    fn serve(&self, op: &FsOp) -> FsResult {
        if !self.enabled.load(Ordering::SeqCst) {
            return Err(FsError::new(
                FsErrorKind::Disabled,
                "Laufwerke sind dort nicht freigegeben",
            ));
        }
        let vols = self.volumes();
        if let Some(hidden) = self.hidden.lock().unwrap().clone() {
            for p in op_paths(op) {
                if let Ok(Resolved::Inside(_, real)) = remotefs::resolve(&vols, p) {
                    if real.starts_with(&hidden) {
                        return Err(FsError::new(
                            FsErrorKind::Denied,
                            "Laufwerke des anderen Rechners",
                        ));
                    }
                }
            }
        }
        remotefs::serve(&vols, op)
    }
}

fn op_paths(op: &FsOp) -> Vec<&str> {
    match op {
        FsOp::Volumes => vec![],
        FsOp::Stat { path }
        | FsOp::List { path }
        | FsOp::Read { path, .. }
        | FsOp::Write { path, .. }
        | FsOp::Mkdir { path }
        | FsOp::Remove { path } => vec![path],
        FsOp::Rename { from, to, .. } | FsOp::Copy { from, to, .. } => vec![from, to],
    }
}

type Job = (Arc<Outbox>, u32, FsOp);

fn share_worker(state: Arc<ShareState>, jobs: Arc<Mutex<Receiver<Job>>>) {
    loop {
        let job = jobs.lock().unwrap().recv();
        let Ok((outbox, id, op)) = job else { return };
        let result = state.serve(&op);
        if let Err(e) = &result {
            if !matches!(e.kind, FsErrorKind::NotFound) {
                log::debug!("Laufwerkszugriff von drüben: {e}");
            }
        }
        outbox.push(Message::FsReply { id, result });
    }
}

// ---------------------------------------------------------------- Einbinden

enum MountCmd {
    Connected(String),
    Disconnected,
    Open,
    SetAuto(bool),
    AllowLargeFiles,
    Quit(Sender<()>),
}

/// Anzeigename beim Einbinden: nur unproblematische Zeichen.
fn label_for(peer: &str) -> String {
    let s: String = peer
        .chars()
        .filter(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.'))
        .collect();
    let s = s.trim().trim_matches('.').trim().to_string();
    if s.is_empty() {
        "Anderer Rechner".into()
    } else {
        s
    }
}

struct Mounter {
    client: Arc<Client>,
    dav: Option<dav::DavServer>,
    share: Arc<ShareState>,
    note: NoteFn,
    auto: bool,
    peer: Option<String>,
    mounted: Option<mount::Mounted>,
    warned_large: bool,
}

impl Mounter {
    fn try_mount(&mut self, quiet: bool) {
        if self.mounted.is_some() {
            return;
        }
        let Some(peer) = self.peer.clone() else {
            return;
        };
        let Some(dav) = &self.dav else {
            (self.note)("Laufwerke: interner Server nicht gestartet".into());
            return;
        };
        match dav::Backend::submit(&*self.client, FsOp::Volumes)
            .recv_timeout(Duration::from_secs(15))
        {
            Ok(Ok(FsData::Entries(v))) => {
                log::info!("{peer} gibt {} Laufwerk(e) frei", v.len());
            }
            Ok(Err(e)) if e.kind == FsErrorKind::Disabled => {
                if !quiet {
                    (self.note)(format!("{peer} gibt keine Laufwerke frei"));
                }
                return;
            }
            Ok(Err(e)) => {
                (self.note)(format!("Laufwerke von {peer}: {}", e.message));
                return;
            }
            _ => {
                (self.note)(format!("Laufwerke von {peer}: keine Antwort"));
                return;
            }
        }
        let label = label_for(&peer);
        match mount::mount(&dav.url(&label), &label) {
            Ok(m) => {
                log::info!("Laufwerke von {peer} eingebunden: {}", m.point.display());
                let mut text = format!("Laufwerke von {peer} {}", m.describe());
                if !self.warned_large && mount::large_files_blocked() {
                    self.warned_large = true;
                    text.push_str(" – für Dateien über 50 MB: „Große Dateien erlauben …“");
                }
                (self.note)(text);
                *self.share.hidden.lock().unwrap() = Some(m.point.clone());
                self.mounted = Some(m);
            }
            Err(e) => {
                log::warn!("Laufwerke von {peer} einbinden: {e}");
                (self.note)(format!("Laufwerke von {peer}: {e}"));
            }
        }
    }

    fn unmount(&mut self) {
        if let Some(m) = self.mounted.take() {
            log::info!("Laufwerke lösen: {}", m.point.display());
            mount::unmount(&m);
        }
        *self.share.hidden.lock().unwrap() = None;
    }

    fn run(mut self, rx: Receiver<MountCmd>) {
        if let Some(d) = &self.dav {
            mount::cleanup_stale(d.port);
        }
        while let Ok(cmd) = rx.recv() {
            match cmd {
                MountCmd::Connected(peer) => {
                    if self.peer.as_deref() != Some(&peer) {
                        self.unmount();
                    }
                    self.peer = Some(peer);
                    if self.auto {
                        self.try_mount(true);
                    }
                }
                MountCmd::Disconnected => {
                    self.peer = None;
                    self.unmount();
                }
                MountCmd::Open => {
                    let Some(peer) = self.peer.clone() else {
                        (self.note)("Nicht verbunden: keine Laufwerke von drüben".into());
                        continue;
                    };
                    self.try_mount(false);
                    if let Some(m) = &self.mounted {
                        if let Err(e) = opener::open(&m.point) {
                            log::warn!("{} öffnen: {e}", m.point.display());
                        }
                    } else if let Some(d) = &self.dav {
                        // Rückfallebene: Ordnerliste im Browser.
                        let _ = opener::open(d.url(&label_for(&peer)));
                    }
                }
                MountCmd::SetAuto(on) => {
                    self.auto = on;
                    if on {
                        self.try_mount(false);
                    } else {
                        self.unmount();
                    }
                }
                MountCmd::AllowLargeFiles => {
                    let was = self.mounted.is_some();
                    self.unmount();
                    match mount::allow_large_files() {
                        Ok(()) => (self.note)("Große Dateien auf dem Netzlaufwerk erlaubt".into()),
                        Err(e) => (self.note)(format!("Große Dateien erlauben: {e}")),
                    }
                    if was {
                        self.try_mount(true);
                    }
                }
                MountCmd::Quit(ack) => {
                    self.unmount();
                    let _ = ack.send(());
                    return;
                }
            }
        }
        self.unmount();
    }
}

// ---------------------------------------------------------------- Griff für den Dienst

pub struct Drives {
    client: Arc<Client>,
    share: Arc<ShareState>,
    jobs: Sender<Job>,
    mount: Sender<MountCmd>,
}

impl Drives {
    pub fn start(share: bool, auto_mount: bool, note: NoteFn) -> Drives {
        let client = Arc::new(Client::default());
        let state = Arc::new(ShareState {
            enabled: AtomicBool::new(share),
            cache: Mutex::new(None),
            hidden: Mutex::new(None),
        });
        let (jobs, jobs_rx) = mpsc::channel::<Job>();
        let jobs_rx = Arc::new(Mutex::new(jobs_rx));
        for i in 0..SHARE_WORKERS {
            let state = state.clone();
            let jobs_rx = jobs_rx.clone();
            let _ = thread::Builder::new()
                .name(format!("dc-share-{i}"))
                .spawn(move || share_worker(state, jobs_rx));
        }
        let dav = match dav::start(client.clone(), DAV_PORT) {
            Ok(d) => Some(d),
            Err(e) => {
                log::error!("WebDAV-Server: {e}");
                None
            }
        };
        let (mount, mount_rx) = mpsc::channel();
        let mounter = Mounter {
            client: client.clone(),
            dav,
            share: state.clone(),
            note,
            auto: auto_mount,
            peer: None,
            mounted: None,
            warned_large: false,
        };
        let _ = thread::Builder::new()
            .name("dc-mount".into())
            .spawn(move || mounter.run(mount_rx));
        Drives {
            client,
            share: state,
            jobs,
            mount,
        }
    }

    /// Gegenstelle ist da und kompatibel.
    pub fn connected(&self, outbox: Arc<Outbox>, peer: &str) {
        self.client.set_link(Some(outbox));
        let _ = self.mount.send(MountCmd::Connected(peer.to_string()));
    }

    pub fn disconnected(&self) {
        self.client.set_link(None);
        let _ = self.mount.send(MountCmd::Disconnected);
    }

    pub fn on_request(&self, outbox: Arc<Outbox>, id: u32, op: FsOp) {
        let _ = self.jobs.send((outbox, id, op));
    }

    pub fn on_reply(&self, id: u32, result: FsResult) {
        self.client.deliver(id, result);
    }

    pub fn set_share(&self, on: bool) {
        self.share.enabled.store(on, Ordering::SeqCst);
    }

    pub fn set_auto_mount(&self, on: bool) {
        let _ = self.mount.send(MountCmd::SetAuto(on));
    }

    pub fn open(&self) {
        let _ = self.mount.send(MountCmd::Open);
    }

    pub fn allow_large_files(&self) {
        let _ = self.mount.send(MountCmd::AllowLargeFiles);
    }

    /// Einbindung lösen (höchstens ein paar Sekunden warten).
    pub fn shutdown(&self) {
        let (tx, rx) = mpsc::channel();
        if self.mount.send(MountCmd::Quit(tx)).is_ok() {
            let _ = rx.recv_timeout(Duration::from_secs(5));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dav::Backend;

    #[test]
    fn labels_are_safe() {
        assert_eq!(label_for("Daniels MacBook Pro"), "Daniels MacBook Pro");
        assert_eq!(label_for("PC/../x"), "PC..x");
        assert_eq!(label_for("..."), "Anderer Rechner");
    }

    #[test]
    fn client_matches_replies_and_fails_on_disconnect() {
        let c = Client::default();
        let rx = c.submit(FsOp::Volumes);
        assert_eq!(
            rx.recv().unwrap().unwrap_err().kind,
            FsErrorKind::Unavailable
        );
        let ob = Arc::new(Outbox::new());
        c.set_link(Some(ob.clone()));
        let a = c.submit(FsOp::Volumes);
        let b = c.submit(FsOp::Stat { path: "x".into() });
        let ids: Vec<u32> = (0..2)
            .map(|_| match ob.pop(Duration::from_secs(1)) {
                Some(Message::FsRequest { id, .. }) => id,
                other => panic!("{other:?}"),
            })
            .collect();
        c.deliver(ids[1], Ok(FsData::Done));
        assert_eq!(b.recv().unwrap(), Ok(FsData::Done));
        c.set_link(None);
        assert_eq!(
            a.recv().unwrap().unwrap_err().kind,
            FsErrorKind::Unavailable
        );
    }

    /// WebDAV → Client → Warteschlange → „anderer Rechner“ → zurück.
    #[test]
    fn dav_over_outbox_roundtrip() {
        use std::io::{Read, Write};
        let d = std::env::temp_dir().join(format!("dc-drv-e2e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let vols = vec![Volume {
            name: "Windows (C)".into(),
            root: d.clone(),
            total: None,
            free: None,
        }];
        let client = Arc::new(Client::default());
        let ob = Arc::new(Outbox::new());
        client.set_link(Some(ob.clone()));
        {
            let client = client.clone();
            let ob = ob.clone();
            thread::spawn(move || {
                while let Some(m) = ob.pop(Duration::from_secs(5)) {
                    if let Message::FsRequest { id, op } = m {
                        client.deliver(id, remotefs::serve(&vols, &op));
                    }
                }
            });
        }
        let srv = dav::start(client, 0).unwrap();
        let data: Vec<u8> = (0..300_000u32).map(|i| (i * 7) as u8).collect();
        let path = format!("/{}/PC/Windows%20(C)/x.bin", srv.token);
        let mut s = std::net::TcpStream::connect(("127.0.0.1", srv.port)).unwrap();
        s.write_all(
            format!(
                "PUT {path} HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
                data.len()
            )
            .as_bytes(),
        )
        .unwrap();
        s.write_all(&data).unwrap();
        s.write_all(format!("GET {path} HTTP/1.1\r\nConnection: close\r\n\r\n").as_bytes())
            .unwrap();
        let mut out = Vec::new();
        s.read_to_end(&mut out).unwrap();
        assert!(out.starts_with(b"HTTP/1.1 201"));
        assert!(out.ends_with(&data));
        assert_eq!(std::fs::read(d.join("x.bin")).unwrap(), data);
    }

    #[test]
    fn disabled_share_refuses() {
        let s = ShareState {
            enabled: AtomicBool::new(false),
            cache: Mutex::new(None),
            hidden: Mutex::new(None),
        };
        assert_eq!(
            s.serve(&FsOp::Volumes).unwrap_err().kind,
            FsErrorKind::Disabled
        );
        s.enabled.store(true, Ordering::SeqCst);
        assert!(s.serve(&FsOp::Volumes).is_ok());
    }
}
