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
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::time::Duration;

/// Ein Byte-Strom, der sich für Lese- und Schreib-Thread teilen lässt.
pub trait Duplex: Read + Write + Send {
    fn set_read_timeout(&mut self, t: Duration) -> io::Result<()>;
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

// ---------------------------------------------------------------- TCP

impl Duplex for TcpStream {
    fn set_read_timeout(&mut self, t: Duration) -> io::Result<()> {
        TcpStream::set_read_timeout(self, Some(t))
    }
    fn try_clone_box(&self) -> io::Result<Box<dyn Duplex>> {
        Ok(Box::new(self.try_clone()?))
    }
}

pub enum TcpTransport {
    /// Auf eingehende Verbindung warten.
    Listen {
        port: u16,
        listener: Option<TcpListener>,
    },
    /// Mit dem anderen Rechner verbinden (`host:port` oder `host`).
    Connect { addr: String },
}

impl TcpTransport {
    pub fn listen(port: u16) -> Self {
        TcpTransport::Listen {
            port,
            listener: None,
        }
    }
    pub fn connect(addr: impl Into<String>) -> Self {
        TcpTransport::Connect { addr: addr.into() }
    }
}

fn resolve(addr: &str) -> io::Result<SocketAddr> {
    let has_port = (!addr.starts_with('[') && addr.matches(':').count() == 1)
        || (addr.starts_with('[') && addr.contains("]:"));
    let with_port = if has_port {
        addr.to_string()
    } else {
        format!("{addr}:{DEFAULT_TCP_PORT}")
    };
    with_port
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Adresse nicht gefunden"))
}

impl Transport for TcpTransport {
    fn open(&mut self) -> io::Result<Box<dyn Duplex>> {
        match self {
            TcpTransport::Listen { port, listener } => {
                if listener.is_none() {
                    let l = TcpListener::bind(("0.0.0.0", *port))?;
                    l.set_nonblocking(true)?;
                    *listener = Some(l);
                }
                let l = listener.as_ref().unwrap();
                // Bis zu 3 s warten, dann TimedOut, damit die Schleife Stop prüfen kann.
                for _ in 0..30 {
                    match l.accept() {
                        Ok((s, peer)) => {
                            log::info!("Eingehende Verbindung von {peer}");
                            s.set_nonblocking(false)?;
                            s.set_nodelay(true)?;
                            return Ok(Box::new(s));
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(100))
                        }
                        Err(e) => {
                            // Listener ist kaputt (z. B. nach Firewall-Änderung, EINVAL):
                            // beim nächsten Versuch neu binden.
                            *listener = None;
                            return Err(e);
                        }
                    }
                }
                Err(io::ErrorKind::TimedOut.into())
            }
            TcpTransport::Connect { addr } => {
                let sa = resolve(addr)?;
                let s = TcpStream::connect_timeout(&sa, Duration::from_secs(3))?;
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
        assert_eq!(resolve("127.0.0.1").unwrap().port(), DEFAULT_TCP_PORT);
        assert_eq!(resolve("127.0.0.1:9000").unwrap().port(), 9000);
    }
}
