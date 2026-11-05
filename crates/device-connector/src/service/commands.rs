//! Befehle aus dem Tray-Menü.

use super::*;

impl Service {
    pub(super) fn on_ui(&mut self, c: UiCommand) {
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
            UiCommand::EditLayout => {
                let peer = self
                    .status
                    .peer_name
                    .clone()
                    .unwrap_or_else(|| "Anderer Rechner".into());
                let local = if cfg!(target_os = "macos") {
                    "Dieser Mac"
                } else {
                    "Dieser PC"
                };
                let screen = self.engine.screen();
                let snap = crate::arrange::Snapshot::new(
                    local.into(),
                    peer,
                    (screen.w, screen.h),
                    self.engine.peer_screen(),
                    self.cfg.peer_side,
                    self.cfg.screen_align,
                );
                let tx = self.tx.clone();
                let on_save = Box::new(move |c| {
                    let _ = tx.send(Event::Ui(UiCommand::ApplyLayout(c)));
                });
                match crate::arrange::start(snap, on_save) {
                    Ok(url) => {
                        if let Err(e) = opener::open(&url) {
                            log::warn!("Anordnungsfenster öffnen: {e}");
                            self.set_note(Some(format!("Im Browser öffnen: {url}")));
                        }
                    }
                    Err(e) => self.set_note(Some(format!("Anordnungsfenster: {e}"))),
                }
            }
            UiCommand::ApplyLayout(c) => {
                let screen = self.engine.screen();
                let peer_h = self.engine.peer_screen().map(|p| p.1).unwrap_or(1080.0);
                let align = c.align(screen.h, peer_h);
                log::info!("Anordnung: anderer Rechner {:?}, Höhe {align:?}", c.side);
                if c.side != self.cfg.peer_side {
                    self.on_ui(UiCommand::SetPeerSide(c.side));
                }
                self.on_ui(UiCommand::SetAlign(align));
                self.set_note(Some("Anordnung gespeichert".into()));
            }
            UiCommand::SetAlign(align) => {
                self.cfg.screen_align = align;
                self.save_config();
                self.status.align = align;
                self.apply(Input::SetAlign(align));
                self.publish();
            }
            UiCommand::SetTrace(on) => {
                self.cfg.trace_handoff = on;
                self.save_config();
                trace::set_enabled(on, &crate::config::config_dir());
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
