//! Nachrichten vom anderen Rechner, die die App-Schicht selbst behandelt
//! (Hello, Dateien, Übergabe, Drag); der Rest geht an die Engine.

use super::*;

impl Service {
    pub(super) fn on_message(&mut self, m: Message) {
        match m {
            Message::Hello {
                protocol,
                name,
                os,
                app_version,
            } => {
                if protocol != PROTOCOL_VERSION {
                    let why = format!(
                        "Versionen passen nicht: {name} hat v{app_version} (Protokoll {protocol}), hier v{} (Protokoll {PROTOCOL_VERSION}). Bitte beide auf denselben Stand bringen.",
                        env!("CARGO_PKG_VERSION")
                    );
                    log::warn!("{why}");
                    self.incompatible = Some(why.clone());
                    self.status.link = LinkState::Error(why);
                    self.publish();
                    // Verbindung sofort beenden; der Verbindungsaufbau versucht
                    // es weiter, damit ein aktualisiertes Gegenüber ohne
                    // Neustart wieder verbunden wird.
                    if let Some(l) = &self.link {
                        (l.close)("Version passt nicht".into());
                    }
                    return;
                }
                if let Some(l) = &mut self.link {
                    l.hello_ok = true;
                    self.drives.connected(l.outbox.clone(), &name);
                }
                self.incompatible = None;
                log::info!("Gegenstelle: {name} ({os:?}, v{app_version})");
                self.status.link = LinkState::Connected;
                self.status.peer_name = Some(name);
                self.status.note = None;
                self.apply(Input::Connected { peer_os: os });
                self.publish();
            }
            // Vor einem gültigen Hello zählt nichts anderes (#24).
            _ if !self.link.as_ref().is_some_and(|l| l.hello_ok) => {
                log::debug!("Nachricht vor dem Hello verworfen");
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
            Message::FsRequest { id, op } => {
                if let Some(l) = &self.link {
                    self.drives.on_request(l.outbox.clone(), id, op);
                }
            }
            Message::FsReply { id, result } => self.drives.on_reply(id, result),
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
                    ready: false,
                    released: false,
                    cancelled: false,
                    items: Vec::new(),
                    active: None,
                    // Kommt das Ziehen erst an, nachdem wir mit gehaltener
                    // Taste schon zurück sind (Dateien waren drüben noch
                    // nicht abgefragt), übernehmen wir es ebenfalls (#3).
                    reverse: self.engine.mode() == Mode::Local && self.engine.local_left_held(),
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
                    self.try_start_incoming_drag();
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
                    self.try_start_incoming_drag();
                }
            }
            Message::DragDiscard { transfer } => {
                if self
                    .in_drag
                    .as_ref()
                    .is_some_and(|d| d.transfer == transfer)
                {
                    let mut d = self.in_drag.take().unwrap();
                    if let Some(h) = d.active.take() {
                        h.cancel();
                    }
                    let _ = std::fs::remove_dir_all(&d.dir);
                }
                self.receiver.discard(transfer);
                log::info!("Vorab übertragene Dateien verworfen");
            }
            Message::Enter { y } if !platform::can_inject() => {
                // Ohne Berechtigung könnten wir die Maus hier nicht bewegen:
                // sofort zurückgeben, statt den anderen Rechner festzuhalten.
                log::warn!("Maus kommt an, darf hier aber nicht gesteuert werden (Bedienungshilfen fehlen)");
                self.send(Message::Leave { y });
                self.set_note(Some("Maus/Tastatur: Bedienungshilfen erlauben".into()));
            }
            other => {
                match &other {
                    Message::Enter { y } | Message::Leave { y } => {
                        if matches!(other, Message::Enter { .. }) {
                            self.trace_moves = 0;
                        }
                        trace!(
                            "recv {} y={y:.3} cursor={}",
                            msg_name(&other),
                            trace::pos(platform::cursor_pos())
                        )
                    }
                    Message::Release => trace!("recv Release"),
                    _ => {}
                }
                self.apply(Input::Remote(other))
            }
        }
    }
}
