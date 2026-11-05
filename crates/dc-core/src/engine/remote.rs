//! Nachrichten vom anderen Rechner: Eingaben abspielen, Steuerung annehmen
//! und zurückgeben, Zustand spiegeln (Modus `Controlled`).

use super::*;

impl Engine {
    pub(super) fn release_injected(&mut self, out: &mut Vec<Action>) {
        for (_, k) in std::mem::take(&mut self.injected_keys) {
            out.push(Action::Inject(Inject::Key {
                hid: k,
                down: false,
            }));
        }
        for b in Button::ALL {
            if self.injected_buttons & b.bit() != 0 {
                out.push(Action::Inject(Inject::Button {
                    button: b,
                    down: false,
                    x: self.cursor.x,
                    y: self.cursor.y,
                }));
            }
        }
        self.injected_buttons = 0;
    }

    pub(super) fn on_remote(&mut self, msg: Message, out: &mut Vec<Action>) {
        match msg {
            Message::Ping => out.push(Action::Send(Message::Pong)),
            Message::Layout {
                i_am_on_your,
                align,
            } => {
                if self.cfg.peer_side != i_am_on_your {
                    self.cfg.peer_side = i_am_on_your;
                    out.push(Action::PeerSideChanged(i_am_on_your));
                }
                if self.cfg.align != align {
                    self.cfg.align = align;
                    out.push(Action::AlignChanged(align));
                }
            }
            Message::Screen { width, height } => {
                if width.is_finite() && height.is_finite() && width >= 1.0 && height >= 1.0 {
                    self.peer_screen = Some((width, height));
                }
            }
            Message::Enter { y } => {
                if self.mode == Mode::Controlling {
                    // Beide Seiten gleichzeitig am Rand: wir geben nach.
                    self.return_home(out, Some(y));
                }
                self.cursor = VirtualCursor::enter(&self.screen, self.cfg.peer_side, y);
                self.set_mode(Mode::Controlled, out);
                out.push(Action::Inject(Inject::MoveTo {
                    x: self.cursor.x,
                    y: self.cursor.y,
                    dx: 0.0,
                    dy: 0.0,
                }));
            }
            Message::Leave { y } => {
                if self.mode == Mode::Controlling {
                    self.return_home(out, Some(y));
                    self.set_mode(Mode::Local, out);
                }
            }
            Message::Release => {
                if self.mode == Mode::Controlled {
                    self.release_injected(out);
                    self.set_mode(Mode::Local, out);
                }
            }
            Message::MouseMove { dx, dy } => {
                if self.mode != Mode::Controlled {
                    return;
                }
                match self
                    .cursor
                    .step(&self.screen, self.cfg.peer_side, dx as f64, dy as f64)
                {
                    CursorStep::Moved { x, y } => out.push(Action::Inject(Inject::MoveTo {
                        x,
                        y,
                        dx: dx as f64,
                        dy: dy as f64,
                    })),
                    CursorStep::Exited { y_ratio } => {
                        self.release_injected(out);
                        // Cursor unsichtbar an den Rand parken.
                        out.push(Action::Inject(Inject::MoveTo {
                            x: self.screen.inset_x(self.cfg.peer_side, RETURN_INSET_PX),
                            y: self.cursor.y,
                            dx: 0.0,
                            dy: 0.0,
                        }));
                        out.push(Action::Send(Message::Leave {
                            y: self.to_peer_ratio(y_ratio),
                        }));
                        self.set_mode(Mode::Local, out);
                        self.edge_armed = false;
                    }
                }
            }
            Message::MouseButton { button, down } => {
                if self.mode != Mode::Controlled {
                    return;
                }
                if down {
                    self.injected_buttons |= button.bit();
                } else {
                    self.injected_buttons &= !button.bit();
                }
                out.push(Action::Inject(Inject::Button {
                    button,
                    down,
                    x: self.cursor.x,
                    y: self.cursor.y,
                }));
            }
            Message::Wheel { dx, dy } => {
                if self.mode == Mode::Controlled {
                    out.push(Action::Inject(Inject::Wheel { dx, dy }));
                }
            }
            Message::Key { hid: h, down } => {
                if self.mode != Mode::Controlled {
                    return;
                }
                let local = if down {
                    let t = self.translate_key(h);
                    *self.injected_keys.entry(h).or_insert(t)
                } else {
                    match self.injected_keys.remove(&h) {
                        Some(k) => k,
                        None => self.translate_key(h),
                    }
                };
                out.push(Action::Inject(Inject::Key { hid: local, down }));
            }
            Message::Clipboard { text } if self.cfg.share_clipboard => {
                // Auch eingehend begrenzen (#25); die eigene Zwischenablage
                // bleibt dann unverändert.
                if text.len() > MAX_CLIPBOARD_BYTES {
                    log::warn!(
                        "Zwischenablage vom anderen Rechner zu groß ({} Bytes, höchstens {MAX_CLIPBOARD_BYTES}), ignoriert",
                        text.len()
                    );
                    return;
                }
                if text.is_empty() {
                    return;
                }
                self.last_clipboard = Some(text.clone());
                out.push(Action::SetClipboard(text));
            }
            // Hello/Pong/Dateien/Übergabe behandelt die App-Schicht.
            _ => {}
        }
    }
}
