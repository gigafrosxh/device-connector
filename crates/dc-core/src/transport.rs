//! Transporte: alles, was einen zuverlässigen Byte-Strom liefert.
//!
//! * [`SerialTransport`] – Bluetooth-Seriell (SPP). Windows: eingehender oder
//!   ausgehender Bluetooth-COM-Port. macOS: `/dev/cu.<Gerätename>…`.
//!   Funktioniert genauso mit USB-Seriell-Kabeln.
//! * [`TcpTransport`] – WLAN, Ethernet, Bluetooth-PAN und USB-C/Thunderbolt-
//!   Netzwerk (Phase 4).
//!
//! Ein neuer Transport (z. B. natives RFCOMM) muss nur [`Transport`] umsetzen.

use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::time::Duration;

/// Ein Byte-Strom, der sich für Lese- und Schreib-Thread teilen lässt.
pub trait Duplex: Read + Write + Send {
    fn set_read_timeout(&mut self, t: Duration) -> io::Result<()>;
    /// Schreiben bricht nach `t` mit `TimedOut`/`WouldBlock` ab, statt bei
    /// einer hängenden Gegenstelle ewig zu blockieren (#23).
    fn set_write_timeout(&mut self, _t: Duration) -> io::Result<()> {
        Ok(())
    }
    /// Kanal schließen, so dass blockierte Lese-/Schreibaufrufe (auch in
    /// anderen Klonen) sofort zurückkehren. Ohne Unterstützung: nichts tun,
    /// dann begrenzen die Timeouts die Wartezeit.
    fn shutdown(&self) {}
    fn try_clone_box(&self) -> io::Result<Box<dyn Duplex>>;
}

pub trait Transport: Send {
    /// Verbindung aufbauen (oder auf eine warten). Darf blockieren, sollte aber
    /// nach einigen Sekunden mit `TimedOut` zurückkehren, damit die App
    /// beendet werden kann.
    fn open(&mut self) -> io::Result<Box<dyn Duplex>>;
    fn describe(&self) -> String;
}

pub const DEFAULT_TCP_PORT: u16 = 47812;

/// Ausweich-Ports, wenn [`DEFAULT_TCP_PORT`] nicht gebunden werden kann
/// (unter Windows reservieren Hyper-V, WSL oder Docker oft ganze Port-Bereiche,
/// dann meldet `bind` Fehler 10013). Wer verbindet und keinen Port angibt,
/// probiert sie der Reihe nach.
pub const FALLBACK_TCP_PORTS: [u16; 3] = [47820, 47830, 47840];

/// Windows: `WSAEACCES`, Fehler 10013. Beim Verbinden fast immer eine
/// Firewall, ein Virenschutz oder ein VPN, das Verbindungen ins lokale Netz
/// sperrt; beim Warten ein von Windows reservierter Port.
fn is_access_denied(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::PermissionDenied || e.raw_os_error() == Some(10013)
}

/// Verständliche Erklärung für einen Fehler beim Verbinden (#10013).
pub fn connect_hint(e: &io::Error) -> Option<&'static str> {
    is_access_denied(e).then_some(
        "Verbindung blockiert (Fehler 10013): Firewall, Virenschutz oder VPN prüfen, siehe Hilfe",
    )
}

/// Steht in `addr` ein Port (`host:port`, `[v6]:port`)?
pub fn has_explicit_port(addr: &str) -> bool {
    let addr = addr.trim();
    if addr.parse::<IpAddr>().is_ok() {
        return false;
    }
    if addr.parse::<SocketAddr>().is_ok() {
        return true;
    }
    if addr.starts_with('[') {
        return addr.contains("]:");
    }
    addr.matches(':').count() == 1
}

// ---------------------------------------------------------------- TCP

impl Duplex for TcpStream {
    fn set_read_timeout(&mut self, t: Duration) -> io::Result<()> {
        TcpStream::set_read_timeout(self, Some(t))
    }
    fn set_write_timeout(&mut self, t: Duration) -> io::Result<()> {
        TcpStream::set_write_timeout(self, Some(t))
    }
    fn shutdown(&self) {
        let _ = TcpStream::shutdown(self, std::net::Shutdown::Both);
    }
    fn try_clone_box(&self) -> io::Result<Box<dyn Duplex>> {
        Ok(Box::new(self.try_clone()?))
    }
}

pub enum TcpTransport {
    /// Auf eingehende Verbindung warten (IPv4 und IPv6).
    Listen {
        port: u16,
        listeners: Vec<TcpListener>,
    },
    /// Mit dem anderen Rechner verbinden (`host:port`, `host`, `[v6]:port`, `v6`).
    Connect { addr: String },
}

impl TcpTransport {
    pub fn listen(port: u16) -> Self {
        TcpTransport::Listen {
            port,
            listeners: Vec::new(),
        }
    }
    pub fn connect(addr: impl Into<String>) -> Self {
        TcpTransport::Connect { addr: addr.into() }
    }
}

/// Alle Adressen zu `addr` (#22). Ohne Port gilt [`DEFAULT_TCP_PORT`].
pub fn resolve_all(addr: &str) -> io::Result<Vec<SocketAddr>> {
    let addr = addr.trim();
    // Reine IP (auch IPv6 ohne Klammern).
    if let Ok(ip) = addr.parse::<IpAddr>() {
        return Ok(vec![SocketAddr::new(ip, DEFAULT_TCP_PORT)]);
    }
    if let Ok(sa) = addr.parse::<SocketAddr>() {
        return Ok(vec![sa]);
    }
    let with_port = if let Some(inner) = addr.strip_prefix('[').and_then(|a| a.strip_suffix(']')) {
        format!("[{inner}]:{DEFAULT_TCP_PORT}")
    } else if addr.starts_with('[') || addr.matches(':').count() == 1 {
        addr.to_string()
    } else {
        format!("{addr}:{DEFAULT_TCP_PORT}")
    };
    let mut v: Vec<SocketAddr> = Vec::new();
    for a in with_port.to_socket_addrs()? {
        if !v.contains(&a) {
            v.push(a);
        }
    }
    if v.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "Adresse nicht gefunden",
        ));
    }
    Ok(v)
}

/// Nacheinander alle Adressen probieren; Fehler werden gesammelt gemeldet.
pub fn connect_any(addrs: &[SocketAddr], timeout: Duration) -> io::Result<TcpStream> {
    let mut errors = Vec::new();
    let mut last_kind = io::ErrorKind::NotFound;
    for a in addrs {
        match TcpStream::connect_timeout(a, timeout) {
            Ok(s) => return Ok(s),
            Err(e) => {
                last_kind = e.kind();
                errors.push(format!("{a}: {e}"));
            }
        }
    }
    Err(io::Error::new(last_kind, errors.join("; ")))
}

/// IPv6- und IPv4-Listener. Wo `[::]` schon beides annimmt (Linux, macOS),
/// meldet die IPv4-Bindung „belegt“; das ist dann in Ordnung.
/// Wie [`bind_dual`], weicht aber vom Standard-Port auf
/// [`FALLBACK_TCP_PORTS`] aus, wenn Windows den Port sperrt oder er belegt ist.
fn bind_with_fallback(port: u16) -> io::Result<(u16, Vec<TcpListener>)> {
    let first = match bind_dual(port) {
        Ok(l) => return Ok((port, l)),
        Err(e) => e,
    };
    let can_fall_back = port == DEFAULT_TCP_PORT
        && (is_access_denied(&first) || first.kind() == io::ErrorKind::AddrInUse);
    if can_fall_back {
        for alt in FALLBACK_TCP_PORTS {
            if let Ok(l) = bind_dual(alt) {
                log::warn!("Port {port} nicht nutzbar ({first}), warte stattdessen auf Port {alt}");
                return Ok((alt, l));
            }
        }
    }
    if is_access_denied(&first) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "Port {port} ist gesperrt (Fehler 10013, oft von Hyper-V/WSL reserviert, \
                 siehe „netsh int ipv4 show excludedportrange protocol=tcp“). \
                 Ausweich-Ports gehen auch nicht."
            ),
        ));
    }
    Err(first)
}

fn bind_dual(port: u16) -> io::Result<Vec<TcpListener>> {
    let mut out = Vec::new();
    let v6 = TcpListener::bind((Ipv6Addr::UNSPECIFIED, port));
    if let Ok(l) = v6 {
        out.push(l);
    }
    match TcpListener::bind((Ipv4Addr::UNSPECIFIED, port)) {
        Ok(l) => out.push(l),
        Err(e) if !out.is_empty() && e.kind() == io::ErrorKind::AddrInUse => {}
        Err(e) => return Err(e),
    }
    for l in &out {
        l.set_nonblocking(true)?;
    }
    Ok(out)
}

impl Transport for TcpTransport {
    fn open(&mut self) -> io::Result<Box<dyn Duplex>> {
        match self {
            TcpTransport::Listen { port, listeners } => {
                if listeners.is_empty() {
                    let (actual, l) = bind_with_fallback(*port)?;
                    *port = actual;
                    *listeners = l;
                }
                // Bis zu 3 s warten, dann TimedOut, damit die Schleife Stop prüfen kann.
                for _ in 0..30 {
                    for i in 0..listeners.len() {
                        match listeners[i].accept() {
                            Ok((s, peer)) => {
                                log::info!("Eingehende Verbindung von {peer}");
                                s.set_nonblocking(false)?;
                                s.set_nodelay(true)?;
                                return Ok(Box::new(s));
                            }
                            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                            Err(e) => {
                                // Listener ist kaputt (z. B. nach Firewall-Änderung, EINVAL):
                                // beim nächsten Versuch neu binden.
                                listeners.clear();
                                return Err(e);
                            }
                        }
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(io::ErrorKind::TimedOut.into())
            }
            TcpTransport::Connect { addr } => {
                let addrs = resolve_all(addr)?;
                let mut res = connect_any(&addrs, Duration::from_secs(3));
                // Drüben läuft evtl. ein Ausweich-Port (Standard-Port gesperrt).
                if !has_explicit_port(addr) {
                    if let Err(e) = &res {
                        if e.kind() == io::ErrorKind::ConnectionRefused {
                            for alt in FALLBACK_TCP_PORTS {
                                let alts: Vec<SocketAddr> =
                                    addrs.iter().map(|a| SocketAddr::new(a.ip(), alt)).collect();
                                if let Ok(s) = connect_any(&alts, Duration::from_secs(1)) {
                                    log::info!("Verbunden über Ausweich-Port {alt}");
                                    res = Ok(s);
                                    break;
                                }
                            }
                        }
                    }
                }
                let s = res.map_err(|e| match connect_hint(&e) {
                    Some(hint) => {
                        log::warn!("{e}");
                        io::Error::new(io::ErrorKind::PermissionDenied, hint)
                    }
                    None => e,
                })?;
                s.set_nodelay(true)?;
                Ok(Box::new(s))
            }
        }
    }

    fn describe(&self) -> String {
        match self {
            TcpTransport::Listen { port, .. } => format!("Netzwerk (wartet auf Port {port})"),
            TcpTransport::Connect { addr } => format!("Netzwerk ({addr})"),
        }
    }
}

// ---------------------------------------------------------------- Seriell / Bluetooth

struct SerialDuplex(Box<dyn serialport::SerialPort>);

impl Read for SerialDuplex {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

impl Write for SerialDuplex {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl Duplex for SerialDuplex {
    fn set_read_timeout(&mut self, t: Duration) -> io::Result<()> {
        self.0.set_timeout(t).map_err(io::Error::from)
    }
    // serialport kennt nur einen Timeout für Lesen und Schreiben; der kurze
    // Lese-Timeout begrenzt damit auch jedes einzelne write().
    fn try_clone_box(&self) -> io::Result<Box<dyn Duplex>> {
        Ok(Box::new(SerialDuplex(
            self.0.try_clone().map_err(io::Error::from)?,
        )))
    }
}

pub struct SerialTransport {
    pub port: String,
}

impl Transport for SerialTransport {
    fn open(&mut self) -> io::Result<Box<dyn Duplex>> {
        // Bei Bluetooth-SPP ist die Baudrate bedeutungslos, bei USB-Seriell nicht.
        let p = serialport::new(&self.port, 921_600)
            .timeout(Duration::from_millis(500))
            .open()
            .map_err(io::Error::from)?;
        Ok(Box::new(SerialDuplex(p)))
    }

    fn describe(&self) -> String {
        format!("Bluetooth/Seriell ({})", self.port)
    }
}

/// Ein gefundener serieller Port, mit Hinweis ob er nach Bluetooth aussieht.
#[derive(Debug, Clone)]
pub struct PortInfo {
    pub name: String,
    pub label: String,
    pub likely_bluetooth: bool,
}

pub fn list_serial_ports() -> Vec<PortInfo> {
    let ports = serialport::available_ports().unwrap_or_default();
    let mut out: Vec<PortInfo> = ports
        .into_iter()
        // macOS: nur die cu.*-Variante (tty.* wartet auf Carrier-Signal).
        .filter(|p| !p.port_name.starts_with("/dev/tty."))
        .filter(|p| !p.port_name.contains("debug-console"))
        .map(|p| {
            let lower = p.port_name.to_lowercase();
            let bt_type = matches!(p.port_type, serialport::SerialPortType::BluetoothPort);
            let likely_bluetooth =
                bt_type || lower.contains("bluetooth") || lower.starts_with("/dev/cu.");
            let short = p.port_name.trim_start_matches("/dev/cu.").to_string();
            let label = if bt_type {
                format!("{short} (Bluetooth)")
            } else {
                short
            };
            PortInfo {
                name: p.port_name,
                label,
                likely_bluetooth,
            }
        })
        .collect();
    out.sort_by(|a, b| {
        b.likely_bluetooth
            .cmp(&a.likely_bluetooth)
            .then(a.name.cmp(&b.name))
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_adds_default_port() {
        let one = |a: &str| resolve_all(a).unwrap()[0];
        assert_eq!(one("127.0.0.1").port(), DEFAULT_TCP_PORT);
        assert_eq!(one("127.0.0.1:9000").port(), 9000);
        assert_eq!(one("::1"), "[::1]:47812".parse().unwrap());
        assert_eq!(one("[::1]"), "[::1]:47812".parse().unwrap());
        assert_eq!(one("[::1]:9000"), "[::1]:9000".parse().unwrap());
        assert_eq!(one(" 10.0.0.2 ").port(), DEFAULT_TCP_PORT);
        assert!(resolve_all("localhost")
            .unwrap()
            .iter()
            .all(|a| a.port() == DEFAULT_TCP_PORT));
        assert!(resolve_all("localhost:5")
            .unwrap()
            .iter()
            .all(|a| a.port() == 5));
    }

    #[test]
    fn explicit_port_detection() {
        assert!(!has_explicit_port("192.168.178.135"));
        assert!(has_explicit_port("192.168.178.135:9000"));
        assert!(!has_explicit_port("::1"));
        assert!(!has_explicit_port("[::1]"));
        assert!(has_explicit_port("[::1]:9000"));
        assert!(!has_explicit_port("macbook.local"));
        assert!(has_explicit_port("macbook.local:9000"));
    }

    #[test]
    fn access_denied_gets_a_hint() {
        let e = io::Error::from_raw_os_error(if cfg!(windows) { 10013 } else { 13 });
        assert!(connect_hint(&e).unwrap().contains("10013"));
        let e = io::Error::from(io::ErrorKind::ConnectionRefused);
        assert!(connect_hint(&e).is_none());
    }

    #[test]
    fn listen_falls_back_when_default_port_is_taken() {
        // Standard-Port belegen (falls schon belegt, ist er es eben).
        let _hold = bind_dual(DEFAULT_TCP_PORT).ok();
        let (port, l) = bind_with_fallback(DEFAULT_TCP_PORT).unwrap();
        assert!(!l.is_empty());
        if _hold.is_some() {
            assert!(FALLBACK_TCP_PORTS.contains(&port));
        }
    }

    #[test]
    fn connect_falls_back_to_next_address() {
        // Erste Adresse: geschlossener Port, zweite: offener Listener.
        let closed = TcpListener::bind("127.0.0.1:0").unwrap();
        let dead = closed.local_addr().unwrap();
        drop(closed);
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let ok = l.local_addr().unwrap();
        let s = connect_any(&[dead, ok], Duration::from_secs(2)).unwrap();
        assert_eq!(s.peer_addr().unwrap(), ok);
        let e = connect_any(&[dead], Duration::from_secs(2)).unwrap_err();
        assert!(e.to_string().contains(&dead.to_string()));
    }

    #[test]
    fn listener_accepts_ipv4_and_ipv6() {
        let probe = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let mut t = TcpTransport::listen(port);
        // Der Listener entsteht erst in open(): Verbindungsaufbau wiederholen.
        let retry = move |host: &'static str| {
            for _ in 0..30 {
                if let Ok(s) = TcpStream::connect((host, port)) {
                    return Some(s);
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            None
        };
        let h = std::thread::spawn(move || {
            let a = retry("127.0.0.1");
            let b = retry("::1");
            std::thread::sleep(Duration::from_millis(500));
            (a.is_some(), b.is_some())
        });
        assert!(t.open().is_ok());
        let (v4, v6) = h.join().unwrap();
        assert!(v4);
        if v6 {
            assert!(t.open().is_ok());
        }
    }
}
