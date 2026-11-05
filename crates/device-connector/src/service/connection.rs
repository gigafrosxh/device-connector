//! Verbindungsaufbau: Transport wählen, verbinden oder warten, Handshake,
//! nach Abbruch neu versuchen (Thread „dc-link“).

use super::*;

pub(super) fn build_transport(
    choice: &TransportChoice,
    tcp_port: u16,
) -> Option<Box<dyn Transport>> {
    match choice {
        TransportChoice::None => None,
        TransportChoice::Serial { port } => Some(Box::new(SerialTransport { port: port.clone() })),
        TransportChoice::TcpListen => Some(Box::new(TcpTransport::listen(tcp_port))),
        TransportChoice::TcpConnect { addr } => Some(Box::new(TcpTransport::connect(addr.clone()))),
    }
}

pub(super) fn link_manager(
    settings: Arc<Mutex<LinkSettings>>,
    tx: Sender<Event>,
    stop: Arc<AtomicBool>,
) {
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
            close: conn.closer(),
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

pub(super) fn sleep_unless_changed(
    settings: &Mutex<LinkSettings>,
    gen: u64,
    stop: &AtomicBool,
    d: Duration,
) {
    let end = Instant::now() + d;
    while Instant::now() < end
        && !stop.load(Ordering::SeqCst)
        && settings.lock().unwrap().generation == gen
    {
        thread::sleep(Duration::from_millis(100));
    }
}
