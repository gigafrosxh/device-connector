//! Lokale Eingaben: Rand erkennen, Steuerung übergeben, Tasten und Tasten
//! weiterleiten, solange der Cursor drüben ist (Modus `Local`/`Controlling`).

use super::*;

impl Engine {
    pub(super) fn on_local_move(&mut self, x: f64, y: f64, out: &mut Vec<Action>) {
        if self.mode != Mode::Local || !self.connected {
            return;
        }
        let side = self.cfg.peer_side;
        if self.local_buttons & Button::Left.bit() != 0 && !self.prefetch_sent {
            let zone = (self.screen.w * PREFETCH_ZONE).max(PREFETCH_MIN_PX);
            if self.screen.distance_from_edge(side, x) <= zone {
                self.prefetch_sent = true;
                out.push(Action::PrefetchDrag);
            }
        }
        if !self.edge_armed {
            if self.screen.distance_from_edge(side, x) >= EDGE_REARM_PX {
                self.edge_armed = true;
                self.edge_pushes = 0;
            } else if self.screen.at_edge(side, x) {
                // Wer nach der Rückkehr am Rand weiter dagegen drückt, will
                // wieder rüber: nach ein paar Bewegungen ist der Rand scharf.
                self.edge_pushes += 1;
                if self.edge_pushes >= EDGE_PUSHES_TO_REARM {
                    self.edge_armed = true;
                    self.edge_pushes = 0;
                }
            }
            if !self.edge_armed {
                return;
            }
        }
        if self.screen.at_edge(side, x) && y >= self.screen.y && y <= self.screen.bottom() {
            log::debug!(
                "Rand überquert bei ({x:.0}, {y:.0}), gedrückte Maustasten {:#b}",
                self.local_buttons
            );
            if self.local_buttons & Button::Left.bit() != 0 {
                self.carrying_drag = true;
                out.push(Action::ResolveDrag);
            }
            self.held_at_capture = self.local_buttons;
            self.capture_origin = (x, y);
            self.set_mode(Mode::Controlling, out);
            out.push(Action::StartCapture);
            out.push(Action::Send(Message::Enter {
                y: self.to_peer_ratio(self.screen.y_ratio(y)),
            }));
            // Gedrückte Modifier mitnehmen (z. B. Shift beim Rüberziehen).
            for &k in self.local_keys.iter().filter(|k| keymap::is_modifier(**k)) {
                self.sent_keys.insert(k);
                out.push(Action::Send(Message::Key { hid: k, down: true }));
            }
        }
    }

    pub(super) fn on_local_button(&mut self, button: Button, down: bool, out: &mut Vec<Action>) {
        if button == Button::Left {
            log::debug!(
                "Linke Taste {} ({:?})",
                if down { "gedrückt" } else { "los" },
                self.mode
            );
        }
        if down {
            self.local_buttons |= button.bit();
        } else {
            self.local_buttons &= !button.bit();
        }
        if button == Button::Left
            && !down
            && std::mem::take(&mut self.prefetch_sent)
            && self.mode == Mode::Local
            && !self.carrying_drag
        {
            out.push(Action::DragEndedLocally);
        }
        if self.mode != Mode::Controlling {
            return;
        }
        if !down && self.held_at_capture & button.bit() != 0 {
            self.held_at_capture &= !button.bit();
            let (x, y) = self.capture_origin;
            out.push(Action::ReleaseLocal { button, x, y });
        }
        if self.carrying_drag && button == Button::Left {
            if !down {
                // Ende des Datei-Drags: drüben die Dateien fallen lassen. Die
                // Taste selbst hält dort die App, nicht wir.
                self.carrying_drag = false;
                out.push(Action::Send(Message::DragDrop));
            }
            return;
        }
        if down {
            self.sent_buttons |= button.bit();
        } else {
            if self.sent_buttons & button.bit() == 0 {
                return;
            }
            self.sent_buttons &= !button.bit();
        }
        out.push(Action::Send(Message::MouseButton { button, down }));
    }

    pub(super) fn hotkey_base_held(&self) -> bool {
        let ctrl =
            self.local_keys.contains(&hid::LEFT_CTRL) || self.local_keys.contains(&hid::RIGHT_CTRL);
        let alt =
            self.local_keys.contains(&hid::LEFT_ALT) || self.local_keys.contains(&hid::RIGHT_ALT);
        ctrl && alt
    }

    pub(super) fn on_local_key(&mut self, hid_code: u16, down: bool, out: &mut Vec<Action>) {
        if down {
            // Notausgang: Strg+Alt+Esc holt die Kontrolle zurück.
            if hid_code == hid::ESCAPE && self.hotkey_base_held() && self.mode == Mode::Controlling
            {
                out.push(Action::Send(Message::Release));
                self.return_home(out, None);
                self.set_mode(Mode::Local, out);
                self.local_keys.insert(hid_code);
                return;
            }
            if hid_code == hid::H
                && self.hotkey_base_held()
                && self.mode == Mode::Local
                && self.connected
            {
                out.push(Action::RequestHandoff);
            }
            self.local_keys.insert(hid_code);
        } else {
            self.local_keys.remove(&hid_code);
        }

        if self.mode == Mode::Controlling {
            if down {
                self.sent_keys.insert(hid_code);
            } else if !self.sent_keys.remove(&hid_code) {
                return;
            }
            out.push(Action::Send(Message::Key {
                hid: hid_code,
                down,
            }));
        }
    }

    /// Kontrolle zurück auf diesen Rechner. `y_ratio`: wo der Cursor
    /// zurückkommt (None = Bildschirmmitte).
    pub(super) fn return_home(&mut self, out: &mut Vec<Action>, y_ratio: Option<f32>) {
        if self.mode != Mode::Controlling {
            return;
        }
        for k in std::mem::take(&mut self.sent_keys) {
            out.push(Action::Send(Message::Key {
                hid: k,
                down: false,
            }));
        }
        for b in Button::ALL {
            if self.sent_buttons & b.bit() != 0 {
                out.push(Action::Send(Message::MouseButton {
                    button: b,
                    down: false,
                }));
            }
        }
        self.sent_buttons = 0;
        self.held_at_capture = 0;
        if std::mem::take(&mut self.carrying_drag) {
            out.push(Action::Send(Message::DragCancel));
        }
        let (x, y) = match y_ratio {
            Some(r) => (
                self.screen.inset_x(self.cfg.peer_side, RETURN_INSET_PX),
                self.screen.y_from_ratio(r),
            ),
            None => self.screen.center(),
        };
        self.edge_armed = false;
        out.push(Action::StopCapture { x, y });
    }

    pub(super) fn translate_key(&self, hid_code: u16) -> u16 {
        let differs = self
            .peer_os
            .map(|p| p != self.cfg.local_os)
            .unwrap_or(false);
        if self.cfg.swap_ctrl_cmd && differs {
            keymap::swap_ctrl_gui(hid_code)
        } else {
            hid_code
        }
    }
}
