//! Verbindungsschicht über einem beliebigen [`Duplex`]-Strom:
//!
//! 1. **Rahmen**: `0xDC, Typ, Länge (u16 BE), Nutzdaten`.
//! 2. **Sync**: Beide Seiten senden Zufallszahlen, bis jede die andere kennt.
//!    Die größere Zahl übernimmt die Initiator-Rolle. Das ist nötig, weil bei
//!    Bluetooth-Seriell beide Seiten einfach „den Port öffnen“.
//! 3. **Noise-Handshake** `NNpsk0` mit dem Schlüssel aus dem Kopplungscode:
//!    Wer den Code nicht kennt, kann weder mitlesen noch Eingaben einschleusen.
//! 4. **Daten**: verschlüsselte, ggf. fragmentierte [`Message`]s. Eingaben
//!    haben Vorrang vor Dateiblöcken ([`Outbox`]).

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use rand::RngCore;
use snow::StatelessTransportState;

use crate::protocol::Message;
use crate::transport::Duplex;

const MAGIC: u8 = 0xDC;
const T_SYNC: u8 = 0;
const T_HS1: u8 = 1;
const T_HS2: u8 = 2;
const T_DATA: u8 = 3;

const NOISE_PATTERN: &str = "Noise_NNpsk0_25519_ChaChaPoly_SHA256";
const MAX_FRAME: usize = 65535;
const TAG_LEN: usize = 16;
/// Klartext pro Fragment: Rahmen-Limit minus Tag minus 1 Byte „mehr folgt“.
const MAX_FRAGMENT: usize = MAX_FRAME - TAG_LEN - 1;
/// Größte zusammengesetzte Nachricht (Schutz vor Speicher-Angriffen).
const MAX_MESSAGE: usize = 4 * 1024 * 1024;

pub const KEEPALIVE: Duration = Duration::from_secs(2);
pub const DEAD_AFTER: Duration = Duration::from_secs(8);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug)]
pub enum LinkError {
    Io(io::Error),
    /// Kopplungscode stimmt nicht überein (oder Daten beschädigt).
    BadPairingCode,
    Protocol(String),
    Timeout,
}

impl std::fmt::Display for LinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LinkError::Io(e) => write!(f, "Verbindungsfehler: {e}"),
            LinkError::BadPairingCode => write!(f, "Kopplungscode passt nicht"),
            LinkError::Protocol(s) => write!(f, "Protokollfehler: {s}"),
            LinkError::Timeout => write!(f, "Zeitüberschreitung"),
        }
    }
}

impl std::error::Error for LinkError {}

impl From<io::Error> for LinkError {
    fn from(e: io::Error) -> Self {
        LinkError::Io(e)
    }
}

fn is_timeout(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
    )
}

// ---------------------------------------------------------------- Rahmen

pub fn write_frame(w: &mut dyn Write, ty: u8, payload: &[u8]) -> io::Result<()> {
    assert!(payload.len() <= MAX_FRAME);
    let mut buf = Vec::with_capacity(4 + payload.len());
    buf.push(MAGIC);
    buf.push(ty);
    buf.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    buf.extend_from_slice(payload);
    w.write_all(&buf)?;
    w.flush()
}

/// Liest Rahmen; überspringt Müll bis zum nächsten Magic-Byte (nur vor dem
/// Handshake sinnvoll, danach führt Müll ohnehin zum Entschlüsselungsfehler).
pub struct FrameReader {
    buf: Vec<u8>,
}

impl Default for FrameReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameReader {
    pub fn new() -> Self {
        FrameReader {
            buf: Vec::with_capacity(8192),
        }
    }

    /// `Ok(None)` bei Lese-Timeout ohne vollständigen Rahmen.
    pub fn read(&mut self, r: &mut dyn Read) -> io::Result<Option<(u8, Vec<u8>)>> {
        loop {
            if let Some(f) = self.try_parse() {
                return Ok(Some(f));
            }
            let mut tmp = [0u8; 16384];
            match r.read(&mut tmp) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(n) => self.buf.extend_from_slice(&tmp[..n]),
                Err(e) if is_timeout(&e) => return Ok(None),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }

    /// Iterativ (nicht rekursiv): Eine Gegenstelle kann vor der Authentisierung
    /// beliebig viele ungültige Kopfzeilen schicken, ohne den Stack zu sprengen.
    fn try_parse(&mut self) -> Option<(u8, Vec<u8>)> {
        let mut start = 0;
        loop {
            // Bis zum nächsten Magic-Byte vorspulen.
            match self.buf[start..].iter().position(|&b| b == MAGIC) {
                Some(i) => start += i,
                None => {
                    self.buf.clear();
                    return None;
                }
            }
            if self.buf.len() - start < 4 {
                self.buf.drain(..start);
                return None;
            }
            let ty = self.buf[start + 1];
            if ty > T_DATA {
                // Kein gültiger Rahmen, ein Byte weiter suchen.
                start += 1;
                continue;
            }
            self.buf.drain(..start);
            let len = u16::from_be_bytes([self.buf[2], self.buf[3]]) as usize;
            if self.buf.len() < 4 + len {
                return None;
            }
            let payload = self.buf[4..4 + len].to_vec();
            self.buf.drain(..4 + len);
            return Some((ty, payload));
        }
    }

    /// Gepufferte, noch nicht verarbeitete Bytes (für Tests).
    #[cfg(test)]
    fn buffered(&self) -> usize {
        self.buf.len()
    }
}

// ---------------------------------------------------------------- Handshake

/// Ergebnis des Handshakes: die Verschlüsselung plus Rolle.
pub struct Secured {
    pub noise: Arc<StatelessTransportState>,
    pub initiator: bool,
    pub frames: FrameReader,
}

fn sync(stream: &mut dyn Duplex, frames: &mut FrameReader) -> Result<bool, LinkError> {
    let mut mine = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut mine);
    let mut peer: Option<[u8; 8]> = None;
    let mut peer_saw_me = false;
    let start = Instant::now();
    let mut last_send = Instant::now() - Duration::from_secs(1);

    stream.set_read_timeout(Duration::from_millis(100))?;
    loop {
        if start.elapsed() > HANDSHAKE_TIMEOUT {
            return Err(LinkError::Timeout);
        }
        if last_send.elapsed() >= Duration::from_millis(300) {
            let mut p = mine.to_vec();
            p.push(peer.is_some() as u8);
            write_frame(stream, T_SYNC, &p)?;
            last_send = Instant::now();
        }
        if let Some((ty, payload)) = frames.read(stream)? {
            if ty == T_SYNC && payload.len() == 9 {
                let mut n = [0u8; 8];
                n.copy_from_slice(&payload[..8]);
                if n == mine {
                    // Echo (z. B. Loopback) – ignorieren.
                    continue;
                }
                peer = Some(n);
                if payload[8] == 1 {
                    peer_saw_me = true;
                }
            }
        }
        if let (Some(p), true) = (peer, peer_saw_me) {
            // Ein letztes Mal bestätigen, damit die Gegenseite sicher fertig wird.
            let mut fin = mine.to_vec();
            fin.push(1);
            write_frame(stream, T_SYNC, &fin)?;
            if p == mine {
                return Err(LinkError::Protocol("gleiche Zufallszahl".into()));
            }
            return Ok(mine > p);
        }
    }
}

fn read_typed(
    stream: &mut dyn Duplex,
    frames: &mut FrameReader,
    want: u8,
    deadline: Instant,
) -> Result<Vec<u8>, LinkError> {
    loop {
        if Instant::now() > deadline {
            return Err(LinkError::Timeout);
        }
        if let Some((ty, payload)) = frames.read(stream)? {
            if ty == want {
                return Ok(payload);
            }
            if ty == T_SYNC {
                continue; // Nachzügler aus der Sync-Phase
            }
            return Err(LinkError::Protocol(format!("unerwarteter Rahmen {ty}")));
        }
    }
}

pub fn handshake(stream: &mut dyn Duplex, psk: &[u8; 32]) -> Result<Secured, LinkError> {
    let mut frames = FrameReader::new();
    let initiator = sync(stream, &mut frames)?;
    let params = NOISE_PATTERN.parse().expect("gültiges Noise-Muster");
    let builder = snow::Builder::new(params).psk(0, psk);
    let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    let mut buf = vec![0u8; MAX_FRAME];

    let hs = if initiator {
        let mut hs = builder
            .build_initiator()
            .map_err(|e| LinkError::Protocol(e.to_string()))?;
        let n = hs
            .write_message(b"dc1", &mut buf)
            .map_err(|e| LinkError::Protocol(e.to_string()))?;
        write_frame(stream, T_HS1, &buf[..n])?;
        let reply = read_typed(stream, &mut frames, T_HS2, deadline)?;
        hs.read_message(&reply, &mut buf)
            .map_err(|_| LinkError::BadPairingCode)?;
        hs
    } else {
        let mut hs = builder
            .build_responder()
            .map_err(|e| LinkError::Protocol(e.to_string()))?;
        let first = read_typed(stream, &mut frames, T_HS1, deadline)?;
        hs.read_message(&first, &mut buf)
            .map_err(|_| LinkError::BadPairingCode)?;
        let n = hs
            .write_message(b"dc1", &mut buf)
            .map_err(|e| LinkError::Protocol(e.to_string()))?;
        write_frame(stream, T_HS2, &buf[..n])?;
        hs
    };

    let noise = hs
        .into_stateless_transport_mode()
        .map_err(|e| LinkError::Protocol(e.to_string()))?;
    Ok(Secured {
        noise: Arc::new(noise),
        initiator,
        frames,
    })
}

// ---------------------------------------------------------------- Ausgangs-Warteschlange

/// Zwei Warteschlangen: Eingaben/Steuerung zuerst, Dateiblöcke danach.
/// `push_bulk` blockiert, wenn schon viele Blöcke warten, damit große Dateien
/// nicht komplett im Speicher landen.
pub struct Outbox {
    inner: Mutex<OutboxInner>,
    cv: Condvar,
}

struct OutboxInner {
    high: VecDeque<Message>,
    bulk: VecDeque<Message>,
    closed: bool,
}

const BULK_LIMIT: usize = 32;

impl Default for Outbox {
    fn default() -> Self {
        Self::new()
    }
}

impl Outbox {
    pub fn new() -> Outbox {
        Outbox {
            inner: Mutex::new(OutboxInner {
                high: VecDeque::new(),
                bulk: VecDeque::new(),
                closed: false,
            }),
            cv: Condvar::new(),
        }
    }

    pub fn push(&self, m: Message) {
        let mut g = self.inner.lock().unwrap();
        if g.closed {
            return;
        }
        if m.is_bulk() {
            drop(g);
            self.push_bulk(m);
            return;
        }
        // Aufeinanderfolgende Mausbewegungen zusammenfassen: spart Bandbreite,
        // wenn Bluetooth kurz hinterherhängt.
        if let (Some(Message::MouseMove { dx, dy }), Message::MouseMove { dx: ndx, dy: ndy }) =
            (g.high.back_mut(), &m)
        {
            *dx += ndx;
            *dy += ndy;
            return;
        }
        g.high.push_back(m);
        self.cv.notify_all();
    }

    /// Gibt `false` zurück, wenn die Verbindung geschlossen wurde.
    pub fn push_bulk(&self, m: Message) -> bool {
        let mut g = self.inner.lock().unwrap();
        while !g.closed && g.bulk.len() >= BULK_LIMIT {
            g = self.cv.wait(g).unwrap();
        }
        if g.closed {
            return false;
        }
        g.bulk.push_back(m);
        self.cv.notify_all();
        true
    }

    /// Nächste Nachricht, Eingaben zuerst. `None` bei Timeout oder geschlossen.
    pub fn pop(&self, timeout: Duration) -> Option<Message> {
        let deadline = Instant::now() + timeout;
        let mut g = self.inner.lock().unwrap();
        loop {
            if let Some(m) = g.high.pop_front() {
                return Some(m);
            }
            if let Some(m) = g.bulk.pop_front() {
                self.cv.notify_all();
                return Some(m);
            }
            if g.closed {
                return None;
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            g = self.cv.wait_timeout(g, deadline - now).unwrap().0;
        }
    }

    pub fn close(&self) {
        let mut g = self.inner.lock().unwrap();
        g.closed = true;
        g.high.clear();
        g.bulk.clear();
        self.cv.notify_all();
    }

    pub fn is_closed(&self) -> bool {
        self.inner.lock().unwrap().closed
    }
}

// ---------------------------------------------------------------- Laufende Verbindung

/// Schreibt eine Nachricht verschlüsselt (und fragmentiert) in den Strom.
pub struct Sealer {
    noise: Arc<StatelessTransportState>,
    nonce: u64,
    buf: Vec<u8>,
}

impl Sealer {
    pub fn new(noise: Arc<StatelessTransportState>) -> Self {
        Sealer {
            noise,
            nonce: 0,
            buf: vec![0u8; MAX_FRAME],
        }
    }

    pub fn send(&mut self, w: &mut dyn Write, m: &Message) -> Result<(), LinkError> {
        let bytes = m.encode();
        let mut chunks = bytes.chunks(MAX_FRAGMENT).peekable();
        if bytes.is_empty() {
            return Err(LinkError::Protocol("leere Nachricht".into()));
        }
        while let Some(chunk) = chunks.next() {
            let more = chunks.peek().is_some();
            let mut plain = Vec::with_capacity(chunk.len() + 1);
            plain.push(more as u8);
            plain.extend_from_slice(chunk);
            let n = self
                .noise
                .write_message(self.nonce, &plain, &mut self.buf)
                .map_err(|e| LinkError::Protocol(e.to_string()))?;
            self.nonce += 1;
            write_frame(w, T_DATA, &self.buf[..n])?;
        }
        Ok(())
    }
}

/// Liest und entschlüsselt Nachrichten.
pub struct Opener {
    noise: Arc<StatelessTransportState>,
    nonce: u64,
    frames: FrameReader,
    partial: Vec<u8>,
    buf: Vec<u8>,
}

impl Opener {
    pub fn new(noise: Arc<StatelessTransportState>, frames: FrameReader) -> Self {
        Opener {
            noise,
            nonce: 0,
            frames,
            partial: Vec::new(),
            buf: vec![0u8; MAX_FRAME],
        }
    }

    /// `Ok(None)` bei Lese-Timeout.
    pub fn recv(&mut self, r: &mut dyn Read) -> Result<Option<Message>, LinkError> {
        loop {
            let Some((ty, payload)) = self.frames.read(r)? else {
                return Ok(None);
            };
            match ty {
                T_DATA => {}
                T_SYNC | T_HS1 | T_HS2 => continue,
                _ => return Err(LinkError::Protocol("unbekannter Rahmen".into())),
            }
            let n = self
                .noise
                .read_message(self.nonce, &payload, &mut self.buf)
                .map_err(|_| LinkError::BadPairingCode)?;
            self.nonce += 1;
            if n == 0 {
                return Err(LinkError::Protocol("leeres Fragment".into()));
            }
            let more = self.buf[0] == 1;
            self.partial.extend_from_slice(&self.buf[1..n]);
            if self.partial.len() > MAX_MESSAGE {
                return Err(LinkError::Protocol("Nachricht zu groß".into()));
            }
            if more {
                continue;
            }
            let bytes = std::mem::take(&mut self.partial);
            let m = Message::decode(&bytes).map_err(|e| LinkError::Protocol(e.to_string()))?;
            return Ok(Some(m));
        }
    }
}

type OnClosed = Box<dyn FnOnce(String) + Send>;

/// Eine aufgebaute, verschlüsselte Verbindung mit Lese- und Schreib-Thread.
pub struct Connection {
    pub outbox: Arc<Outbox>,
    pub initiator: bool,
    stop: Arc<AtomicBool>,
    threads: Vec<thread::JoinHandle<()>>,
}

impl Connection {
    /// Startet die Threads. `on_message` wird im Lese-Thread aufgerufen,
    /// `on_closed` genau einmal, wenn die Verbindung endet.
    pub fn start(
        stream: Box<dyn Duplex>,
        secured: Secured,
        on_message: impl Fn(Message) + Send + 'static,
        on_closed: impl FnOnce(String) + Send + 'static,
    ) -> io::Result<Connection> {
        let outbox = Arc::new(Outbox::new());
        let stop = Arc::new(AtomicBool::new(false));
        let closed_once: Arc<Mutex<Option<OnClosed>>> =
            Arc::new(Mutex::new(Some(Box::new(on_closed))));
        let close = {
            let stop = stop.clone();
            let outbox = outbox.clone();
            let closed_once = closed_once.clone();
            move |why: String| {
                stop.store(true, Ordering::SeqCst);
                outbox.close();
                if let Some(f) = closed_once.lock().unwrap().take() {
                    f(why);
                }
            }
        };
        let close = Arc::new(close);

        let mut reader_stream = stream.try_clone_box()?;
        let mut writer_stream = stream;
        reader_stream.set_read_timeout(Duration::from_millis(500))?;

        let reader = {
            let stop = stop.clone();
            let close = close.clone();
            let mut opener = Opener::new(secured.noise.clone(), secured.frames);
            thread::Builder::new()
                .name("dc-reader".into())
                .spawn(move || {
                    let mut last_rx = Instant::now();
                    while !stop.load(Ordering::SeqCst) {
                        match opener.recv(reader_stream.as_mut()) {
                            Ok(Some(m)) => {
                                last_rx = Instant::now();
                                on_message(m);
                            }
                            Ok(None) => {
                                if last_rx.elapsed() > DEAD_AFTER {
                                    close("Gegenstelle antwortet nicht mehr".into());
                                    return;
                                }
                            }
                            Err(e) => {
                                close(e.to_string());
                                return;
                            }
                        }
                    }
                })?
        };

        let writer = {
            let stop = stop.clone();
            let close = close.clone();
            let outbox = outbox.clone();
            let mut sealer = Sealer::new(secured.noise);
            thread::Builder::new()
                .name("dc-writer".into())
                .spawn(move || {
                    while !stop.load(Ordering::SeqCst) {
                        let m = match outbox.pop(KEEPALIVE) {
                            Some(m) => m,
                            None if outbox.is_closed() => return,
                            None => Message::Ping,
                        };
                        if let Err(e) = sealer.send(writer_stream.as_mut(), &m) {
                            close(e.to_string());
                            return;
                        }
                    }
                })?
        };

        Ok(Connection {
            outbox,
            initiator: secured.initiator,
            stop,
            threads: vec![reader, writer],
        })
    }

    pub fn send(&self, m: Message) {
        self.outbox.push(m);
    }

    pub fn is_alive(&self) -> bool {
        !self.stop.load(Ordering::SeqCst)
    }

    pub fn shutdown(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.outbox.close();
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pairing::derive_psk;
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;

    fn pair() -> (TcpStream, TcpStream) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let a = TcpStream::connect(l.local_addr().unwrap()).unwrap();
        let (b, _) = l.accept().unwrap();
        (a, b)
    }

    #[test]
    fn frame_reader_skips_garbage() {
        let mut data = vec![1, 2, 3];
        write_frame(&mut data, T_SYNC, b"abc").unwrap();
        let mut r = FrameReader::new();
        let mut cur = std::io::Cursor::new(data);
        let (ty, p) = r.read(&mut cur).unwrap().unwrap();
        assert_eq!((ty, p.as_slice()), (T_SYNC, &b"abc"[..]));
    }

    #[test]
    fn frame_reader_survives_many_bad_headers() {
        // #18: viele 0xDC-Bytes mit ungültigem Typ dürfen keinen Stack-Überlauf auslösen.
        let mut data = Vec::new();
        for _ in 0..200_000 {
            data.extend_from_slice(&[MAGIC, 0xFF]);
        }
        write_frame(&mut data, T_DATA, b"ok").unwrap();
        let mut r = FrameReader::new();
        let mut cur = std::io::Cursor::new(data);
        let (ty, p) = r.read(&mut cur).unwrap().unwrap();
        assert_eq!((ty, p.as_slice()), (T_DATA, &b"ok"[..]));
        assert_eq!(r.buffered(), 0);
    }

    #[test]
    fn frame_reader_buffer_stays_bounded_under_garbage() {
        let mut r = FrameReader::new();
        let garbage: Vec<u8> = (0..1_000_000u32).map(|i| (i % 200) as u8 + 1).collect();
        let mut cur = std::io::Cursor::new(garbage);
        assert!(r.read(&mut cur).is_err()); // EOF
        assert!(r.buffered() <= 4 + MAX_FRAME);
    }

    #[test]
    fn outbox_prefers_input_and_merges_moves() {
        let o = Outbox::new();
        o.push(Message::FileChunk {
            transfer: 1,
            id: 1,
            data: vec![0; 10],
        });
        o.push(Message::MouseMove { dx: 1, dy: 1 });
        o.push(Message::MouseMove { dx: 2, dy: -3 });
        assert_eq!(
            o.pop(Duration::ZERO),
            Some(Message::MouseMove { dx: 3, dy: -2 })
        );
        assert!(matches!(
            o.pop(Duration::ZERO),
            Some(Message::FileChunk { .. })
        ));
        assert_eq!(o.pop(Duration::from_millis(10)), None);
    }

    #[test]
    fn encrypted_roundtrip_with_large_message() {
        let (a, b) = pair();
        let psk = derive_psk("K7QM-3XPA-9TRW");
        let tb = thread::spawn(move || {
            let mut s: Box<dyn Duplex> = Box::new(b);
            let sec = handshake(s.as_mut(), &psk).unwrap();
            (s, sec)
        });
        let mut sa: Box<dyn Duplex> = Box::new(a);
        let seca = handshake(sa.as_mut(), &psk).unwrap();
        let (sb, secb) = tb.join().unwrap();
        assert_ne!(seca.initiator, secb.initiator);

        let (tx, rx) = mpsc::channel();
        let cb = Connection::start(sb, secb, move |m| tx.send(m).unwrap(), |_| {}).unwrap();
        let ca = Connection::start(sa, seca, |_| {}, |_| {}).unwrap();

        let big = "ä".repeat(200_000); // 400 KB, mehrere Fragmente
        ca.send(Message::Key { hid: 4, down: true });
        ca.send(Message::Clipboard { text: big.clone() });
        let first = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(first, Message::Key { hid: 4, down: true });
        let second = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(second, Message::Clipboard { text: big });
        ca.shutdown();
        cb.shutdown();
    }

    #[test]
    fn wrong_pairing_code_is_rejected() {
        let (a, b) = pair();
        let tb = thread::spawn(move || {
            let mut s: Box<dyn Duplex> = Box::new(b);
            handshake(s.as_mut(), &derive_psk("AAAA-AAAA-AAAA")).map(|_| ())
        });
        let mut sa: Box<dyn Duplex> = Box::new(a);
        let ra = handshake(sa.as_mut(), &derive_psk("BBBB-BBBB-BBBB")).map(|_| ());
        let rb = tb.join().unwrap();
        // Der Responder merkt es sofort, der Initiator spätestens beim Timeout
        // (der Responder antwortet nicht).
        assert!(ra.is_err() || rb.is_err());
        assert!(
            matches!(rb, Err(LinkError::BadPairingCode))
                || matches!(ra, Err(LinkError::BadPairingCode))
        );
    }
}
