//! Dateien senden und Bestätigungen abwarten, Geöffnetes vom anderen Rechner.

use super::*;

#[derive(Debug, Clone)]
pub(super) struct QueuedInfo {
    pub(super) files: usize,
    pub(super) bytes: u64,
    pub(super) skipped: usize,
}

#[derive(Debug, Clone)]
pub(super) enum PathOrUrl {
    Path(PathBuf),
    Url(String),
}

/// Eigener Sendevorgang, der auf die Bestätigung des Empfängers wartet.
pub(super) struct Pending {
    pub(super) cancel: Arc<AtomicBool>,
    /// Gesetzt, sobald alles in der Warteschlange ist.
    pub(super) queued: Option<(Instant, QueuedInfo)>,
}

/// So lange wartet der Sender nach dem letzten Block auf die Bestätigung.
pub(super) const ACK_TIMEOUT: Duration = Duration::from_secs(60);

impl Service {
    /// Startet einen Sendevorgang. `drag`: drüben verdeckt bereitstellen und
    /// an den Zeiger hängen (vorher `DragBegin`).
    pub(super) fn send_files(
        &mut self,
        paths: Vec<PathBuf>,
        open_first: bool,
        drag: bool,
    ) -> Option<u32> {
        let outbox = self.link.as_ref().map(|l| l.outbox.clone())?;
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
        Some(transfer)
    }

    /// Markierte Dateien im vordersten Explorer/Finder abfragen (im Hintergrund).
    pub(super) fn resolve_selected_files(&self) {
        let tx = self.tx.clone();
        thread::spawn(move || {
            let files = platform::selected_files();
            if files.is_empty() {
                log::info!(
                    "Keine markierten Dateien im vordersten Explorer/Finder-Fenster gefunden"
                );
            } else {
                log::info!("Ziehe {} Datei(en) rüber", files.len());
            }
            let _ = tx.send(Event::DragFiles(files));
        });
    }

    /// Bestätigungen, die zu lange ausbleiben, als Fehler melden.
    pub(super) fn check_ack_timeouts(&mut self) {
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
    pub(super) fn open_from_peer(&mut self, what: PathOrUrl) {
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
}

/// Nur http(s), keine Steuerzeichen, vernünftige Länge.
pub(super) fn is_openable_url(u: &str) -> bool {
    (u.starts_with("https://") || u.starts_with("http://"))
        && u.len() <= 8192
        && !u.chars().any(|c| c.is_control() || c.is_whitespace())
}

pub(super) fn human(bytes: u64) -> String {
    pub(super) const MB: f64 = 1024.0 * 1024.0;
    if bytes as f64 >= MB {
        format!("{:.1} MB", bytes as f64 / MB)
    } else {
        format!("{} KB", bytes / 1024)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_plain_web_urls_are_opened() {
        assert!(is_openable_url("https://example.com/a?b=c"));
        assert!(is_openable_url("http://localhost:8080/"));
        assert!(!is_openable_url("file:///etc/passwd"));
        assert!(!is_openable_url("javascript:alert(1)"));
        assert!(!is_openable_url("https://example.com/a b"));
        assert!(!is_openable_url("https://example.com/\n--flag"));
        assert!(!is_openable_url(&format!("https://{}", "a".repeat(9000))));
    }

    #[test]
    fn sizes_are_human_readable() {
        assert_eq!(human(512), "0 KB");
        assert_eq!(human(10 * 1024), "10 KB");
        assert_eq!(human(3 * 1024 * 1024 / 2), "1.5 MB");
    }
}
