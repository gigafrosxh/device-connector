//! Aktionen der Engine ausführen: Plattform, Verbindung, Oberfläche.

use super::*;

impl Service {
    pub(super) fn execute(&mut self, a: Action) {
        match a {
            Action::Send(Message::DragDrop) => match self.out_drag {
                OutDrag::Resolving {
                    crossed, cancelled, ..
                } => {
                    self.out_drag = OutDrag::Resolving {
                        crossed,
                        released: true,
                        cancelled,
                    }
                }
                OutDrag::Sent { .. } => {
                    self.out_drag = OutDrag::None;
                    self.send(Message::DragDrop);
                }
                OutDrag::None => {}
            },
            Action::Send(Message::DragCancel) => match self.out_drag {
                OutDrag::Resolving {
                    crossed, released, ..
                } => {
                    self.out_drag = OutDrag::Resolving {
                        crossed,
                        released,
                        cancelled: true,
                    }
                }
                OutDrag::Sent { .. } => {
                    self.out_drag = OutDrag::None;
                    self.send(Message::DragCancel);
                }
                OutDrag::None => {}
            },
            Action::DragEndedLocally => match self.out_drag {
                OutDrag::Resolving { crossed: false, .. } => {
                    self.out_drag = OutDrag::None;
                }
                OutDrag::Sent {
                    transfer,
                    crossed: false,
                } => {
                    log::info!("Ziehen endete hier: Vorab-Übertragung wird verworfen");
                    self.out_drag = OutDrag::None;
                    if let Some(p) = self.pending.remove(&transfer) {
                        p.cancel.store(true, Ordering::SeqCst);
                    }
                    self.send(Message::DragDiscard { transfer });
                    self.set_note(None);
                }
                _ => {}
            },
            Action::ReleaseLocal { button, x, y } => {
                trace!("release_local {button:?} at={}", trace::pos(Some((x, y))));
                // Das System hier hat das Drücken gesehen, das Loslassen aber
                // nicht (abgefangen): nachholen, sonst bleibt die Taste hängen.
                if let Some(inj) = &mut self.injector {
                    inj.inject(&dc_core::Inject::Button {
                        button,
                        down: false,
                        x,
                        y,
                    });
                }
            }
            Action::Send(m) => {
                match &m {
                    Message::Enter { y } | Message::Leave { y } => {
                        trace!("send {} y={y:.3}", msg_name(&m))
                    }
                    Message::Release => trace!("send Release"),
                    _ => {}
                }
                self.send(m)
            }
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
                let t0 = Instant::now();
                if let Some(i) = &self.input {
                    i.set_capturing(true);
                }
                trace!(
                    "capture_start origin={} screen={:?} cursor_after={} took={}us hook={}",
                    trace::pos(Some(self.engine.capture_origin())),
                    self.engine.screen(),
                    trace::pos(platform::cursor_pos()),
                    t0.elapsed().as_micros(),
                    self.input.is_some()
                );
            }
            Action::StopCapture { x, y } => {
                let t0 = Instant::now();
                if let Some(i) = &self.input {
                    i.set_capturing(false);
                    i.warp(x, y);
                }
                trace!(
                    "capture_stop target={} cursor_after={} took={}us",
                    trace::pos(Some((x, y))),
                    trace::pos(platform::cursor_pos()),
                    t0.elapsed().as_micros()
                );
            }
            Action::Inject(ev) => {
                if let dc_core::Inject::MoveTo { x, y, dx, dy } = ev {
                    // Nur die ersten Bewegungen nach dem Betreten (Einstiegspunkt).
                    if self.trace_moves < 5 {
                        self.trace_moves += 1;
                        trace!(
                            "inject_move to={} delta=({dx:.0},{dy:.0})",
                            trace::pos(Some((x, y)))
                        );
                    }
                }
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
            Action::PrefetchDrag => {
                if self.out_drag != OutDrag::None || !platform::file_manager_in_front() {
                    return;
                }
                log::info!("Dateien werden Richtung Rand gezogen: Vorab-Übertragung");
                self.out_drag = OutDrag::Resolving {
                    crossed: false,
                    released: false,
                    cancelled: false,
                };
                self.resolve_selected_files();
            }
            Action::ResolveDrag => {
                let fm = platform::file_manager_in_front();
                log::info!(
                    "Ziehen über den Rand erkannt (Dateimanager vorne: {fm}), frage markierte Dateien ab"
                );
                if fm {
                    // Ziehen hier abbrechen (Esc), bevor der Zeiger versetzt
                    // wird: nichts wird hier abgelegt, und Symbol und
                    // Drag-Zeiger verschwinden.
                    platform::cancel_local_drag();
                }
                match self.out_drag {
                    OutDrag::Resolving {
                        released,
                        cancelled,
                        ..
                    } => {
                        self.out_drag = OutDrag::Resolving {
                            crossed: true,
                            released,
                            cancelled,
                        }
                    }
                    OutDrag::Sent { transfer, .. } => {
                        self.out_drag = OutDrag::Sent {
                            transfer,
                            crossed: true,
                        }
                    }
                    OutDrag::None => {
                        self.out_drag = OutDrag::Resolving {
                            crossed: true,
                            released: false,
                            cancelled: false,
                        };
                        self.resolve_selected_files();
                    }
                }
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
            Action::Notice(n) => self.set_note(Some(n)),
            Action::AlignChanged(align) => {
                self.cfg.screen_align = align;
                self.save_config();
                self.status.align = align;
                self.publish();
            }
            Action::ModeChanged(mode) => {
                log::info!("Modus: {mode:?}");
                trace!("mode {mode:?}");
                self.status.mode = mode;
                self.publish();
                if mode == Mode::Controlled {
                    // Vorab übertragene Dateien sind evtl. schon da (#3).
                    self.try_start_incoming_drag();
                }
            }
        }
    }
}
