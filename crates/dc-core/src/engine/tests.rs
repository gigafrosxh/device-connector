//! Tests des Zustandsautomaten.

use super::*;

fn engine(side: Side, os: Os) -> Engine {
    let mut e = Engine::new(
        EngineConfig {
            peer_side: side,
            swap_ctrl_cmd: true,
            share_clipboard: true,
            local_os: os,
            align: Align::Stretch,
        },
        Rect::new(0.0, 0.0, 1920.0, 1080.0),
    );
    let peer = if os == Os::Windows {
        Os::MacOs
    } else {
        Os::Windows
    };
    e.handle(Input::Connected { peer_os: peer });
    e
}

fn sent(actions: &[Action]) -> Vec<Message> {
    actions
        .iter()
        .filter_map(|a| match a {
            Action::Send(m) => Some(m.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn crossing_the_edge_hands_over_control() {
    let mut pc = engine(Side::Right, Os::Windows);
    assert!(pc
        .handle(Input::LocalMove {
            x: 1000.0,
            y: 540.0
        })
        .is_empty());
    let a = pc.handle(Input::LocalMove {
        x: 1919.0,
        y: 540.0,
    });
    assert!(a.contains(&Action::StartCapture));
    assert!(matches!(sent(&a)[0], Message::Enter { y } if (y - 0.5).abs() < 0.01));
    assert_eq!(pc.mode(), Mode::Controlling);

    // Bewegungen und Tasten gehen jetzt rüber.
    let a = pc.handle(Input::LocalDelta { dx: 3.0, dy: -1.0 });
    assert_eq!(sent(&a), vec![Message::MouseMove { dx: 3, dy: -1 }]);
    let a = pc.handle(Input::LocalKey {
        hid: 0x04,
        down: true,
    });
    assert_eq!(
        sent(&a),
        vec![Message::Key {
            hid: 0x04,
            down: true
        }]
    );
}

#[test]
fn wrong_edge_does_nothing() {
    let mut pc = engine(Side::Right, Os::Windows);
    assert!(pc.handle(Input::LocalMove { x: 0.0, y: 540.0 }).is_empty());
    assert_eq!(pc.mode(), Mode::Local);
}

#[test]
fn not_connected_does_nothing() {
    let mut e = Engine::new(
        EngineConfig {
            peer_side: Side::Right,
            swap_ctrl_cmd: true,
            share_clipboard: true,
            local_os: Os::Windows,
            align: Align::Stretch,
        },
        Rect::new(0.0, 0.0, 1920.0, 1080.0),
    );
    assert!(e.handle(Input::LocalMove { x: 1919.0, y: 10.0 }).is_empty());
}

#[test]
fn controlled_side_moves_cursor_and_returns() {
    // Mac: PC steht links.
    let mut mac = engine(Side::Left, Os::MacOs);
    let a = mac.handle(Input::Remote(Message::Enter { y: 0.5 }));
    assert_eq!(mac.mode(), Mode::Controlled);
    assert!(matches!(a.last(), Some(Action::Inject(Inject::MoveTo { x, .. })) if *x == 1.0));

    let a = mac.handle(Input::Remote(Message::MouseMove { dx: 100, dy: 0 }));
    assert!(matches!(a[0], Action::Inject(Inject::MoveTo { x, .. }) if x == 101.0));

    // Zurück über die linke Kante.
    let a = mac.handle(Input::Remote(Message::MouseMove { dx: -200, dy: 0 }));
    assert!(sent(&a).iter().any(|m| matches!(m, Message::Leave { .. })));
    assert_eq!(mac.mode(), Mode::Local);
}

#[test]
fn jitter_right_after_entering_does_not_bounce_back() {
    let mut mac = engine(Side::Left, Os::MacOs);
    mac.handle(Input::Remote(Message::Enter { y: 0.5 }));
    mac.handle(Input::Remote(Message::MouseMove { dx: 1, dy: 0 }));
    mac.handle(Input::Remote(Message::MouseMove { dx: -3, dy: 0 }));
    assert_eq!(mac.mode(), Mode::Controlled);
    mac.handle(Input::Remote(Message::MouseMove { dx: 20, dy: 0 }));
    let a = mac.handle(Input::Remote(Message::MouseMove { dx: -30, dy: 0 }));
    assert!(sent(&a).iter().any(|m| matches!(m, Message::Leave { .. })));
}

#[test]
fn slow_push_against_edge_still_returns() {
    let mut mac = engine(Side::Left, Os::MacOs);
    mac.handle(Input::Remote(Message::Enter { y: 0.5 }));
    let mut left = false;
    for _ in 0..10 {
        let a = mac.handle(Input::Remote(Message::MouseMove { dx: -1, dy: 0 }));
        left |= sent(&a).iter().any(|m| matches!(m, Message::Leave { .. }));
    }
    assert!(left, "langsames Drücken gegen die Kante muss zurückführen");
}

#[test]
fn pushing_against_edge_after_return_rearms() {
    let mut pc = engine(Side::Right, Os::Windows);
    pc.handle(Input::LocalMove {
        x: 1919.0,
        y: 500.0,
    });
    pc.handle(Input::Remote(Message::Leave { y: 0.5 }));
    assert_eq!(pc.mode(), Mode::Local);
    let mut crossed = false;
    for _ in 0..10 {
        pc.handle(Input::LocalMove {
            x: 1919.0,
            y: 500.0,
        });
        crossed |= pc.mode() == Mode::Controlling;
    }
    assert!(crossed);
}

#[test]
fn full_roundtrip_between_two_engines() {
    let mut pc = engine(Side::Right, Os::Windows);
    let mut mac = engine(Side::Left, Os::MacOs);

    let mut to_mac = sent(&pc.handle(Input::LocalMove {
        x: 1919.0,
        y: 270.0,
    }));
    for m in to_mac.drain(..) {
        mac.handle(Input::Remote(m));
    }
    assert_eq!(mac.mode(), Mode::Controlled);

    let back = sent(&mac.handle(Input::Remote(Message::MouseMove { dx: -50, dy: 0 })));
    let mut actions = Vec::new();
    for m in back {
        actions.extend(pc.handle(Input::Remote(m)));
    }
    assert_eq!(pc.mode(), Mode::Local);
    let stop = actions
        .iter()
        .find_map(|a| match a {
            Action::StopCapture { x, y } => Some((*x, *y)),
            _ => None,
        })
        .unwrap();
    assert_eq!(stop.0, 1917.0);
    assert!((stop.1 - 270.0).abs() < 1.0);

    // Direkt danach am Rand: noch nicht scharf, erst nach Entfernen.
    assert!(pc
        .handle(Input::LocalMove {
            x: 1919.0,
            y: 270.0
        })
        .is_empty());
    pc.handle(Input::LocalMove {
        x: 1900.0,
        y: 270.0,
    });
    assert!(!pc
        .handle(Input::LocalMove {
            x: 1919.0,
            y: 270.0
        })
        .is_empty());
}

#[test]
fn ctrl_and_cmd_are_swapped_between_different_os() {
    let mut mac = engine(Side::Left, Os::MacOs);
    mac.handle(Input::Remote(Message::Enter { y: 0.5 }));
    let a = mac.handle(Input::Remote(Message::Key {
        hid: hid::LEFT_CTRL,
        down: true,
    }));
    assert_eq!(
        a,
        vec![Action::Inject(Inject::Key {
            hid: hid::LEFT_GUI,
            down: true
        })]
    );
}

#[test]
fn key_release_uses_mapping_from_key_down() {
    // #29: Tausch-Einstellung ändert sich, während Strg gehalten wird.
    let mut mac = engine(Side::Left, Os::MacOs);
    mac.handle(Input::Remote(Message::Enter { y: 0.5 }));
    let down = Message::Key {
        hid: hid::LEFT_CTRL,
        down: true,
    };
    let up = Message::Key {
        hid: hid::LEFT_CTRL,
        down: false,
    };
    let a = mac.handle(Input::Remote(down.clone()));
    assert_eq!(
        a,
        vec![Action::Inject(Inject::Key {
            hid: hid::LEFT_GUI,
            down: true
        })]
    );
    mac.set_swap_ctrl_cmd(false);
    // Wiederholung (Auto-Repeat) behält die Zuordnung.
    let a = mac.handle(Input::Remote(down));
    assert_eq!(
        a,
        vec![Action::Inject(Inject::Key {
            hid: hid::LEFT_GUI,
            down: true
        })]
    );
    let a = mac.handle(Input::Remote(up.clone()));
    assert_eq!(
        a,
        vec![Action::Inject(Inject::Key {
            hid: hid::LEFT_GUI,
            down: false
        })]
    );
    // Doppeltes Loslassen: harmlos, nutzt die aktuelle Zuordnung.
    let a = mac.handle(Input::Remote(up));
    assert_eq!(
        a,
        vec![Action::Inject(Inject::Key {
            hid: hid::LEFT_CTRL,
            down: false
        })]
    );
    // Danach gilt die neue Einstellung, und Trennen lässt nichts hängen.
    mac.handle(Input::Remote(Message::Key {
        hid: hid::LEFT_CTRL,
        down: true,
    }));
    mac.set_swap_ctrl_cmd(true);
    let a = mac.handle(Input::Disconnected);
    assert!(a.contains(&Action::Inject(Inject::Key {
        hid: hid::LEFT_CTRL,
        down: false
    })));
}

#[test]
fn panic_hotkey_returns_control_and_releases_keys() {
    let mut pc = engine(Side::Right, Os::Windows);
    pc.handle(Input::LocalMove { x: 1919.0, y: 10.0 });
    pc.handle(Input::LocalKey {
        hid: hid::LEFT_CTRL,
        down: true,
    });
    pc.handle(Input::LocalKey {
        hid: hid::LEFT_ALT,
        down: true,
    });
    let a = pc.handle(Input::LocalKey {
        hid: hid::ESCAPE,
        down: true,
    });
    assert_eq!(pc.mode(), Mode::Local);
    let msgs = sent(&a);
    assert_eq!(msgs[0], Message::Release);
    assert!(msgs.contains(&Message::Key {
        hid: hid::LEFT_CTRL,
        down: false
    }));
    assert!(msgs.contains(&Message::Key {
        hid: hid::LEFT_ALT,
        down: false
    }));
    assert!(a.iter().any(|x| matches!(x, Action::StopCapture { .. })));
}

#[test]
fn dragging_across_edge_requests_drag_resolution_and_hides_release() {
    let mut pc = engine(Side::Right, Os::Windows);
    pc.handle(Input::LocalButton {
        button: Button::Left,
        down: true,
    });
    let a = pc.handle(Input::LocalMove {
        x: 1919.0,
        y: 500.0,
    });
    assert!(a.contains(&Action::ResolveDrag));
    let a = pc.handle(Input::LocalButton {
        button: Button::Left,
        down: false,
    });
    assert_eq!(sent(&a), vec![Message::DragDrop]);
}

#[test]
fn drag_is_prefetched_near_edge_and_resolved_before_capture() {
    // #3: Vorab-Übertragung beim Ziehen Richtung Rand, Abbruch des
    // lokalen Ziehens vor dem Versetzen des Zeigers.
    let mut pc = engine(Side::Right, Os::Windows);
    pc.handle(Input::LocalButton {
        button: Button::Left,
        down: true,
    });
    assert!(pc
        .handle(Input::LocalMove { x: 500.0, y: 500.0 })
        .is_empty());
    let a = pc.handle(Input::LocalMove {
        x: 1750.0,
        y: 500.0,
    });
    assert_eq!(a, vec![Action::PrefetchDrag]);
    // Nur einmal pro Druck.
    assert!(pc
        .handle(Input::LocalMove {
            x: 1800.0,
            y: 500.0
        })
        .is_empty());
    let a = pc.handle(Input::LocalMove {
        x: 1919.0,
        y: 500.0,
    });
    let resolve = a.iter().position(|x| *x == Action::ResolveDrag).unwrap();
    let capture = a.iter().position(|x| *x == Action::StartCapture).unwrap();
    assert!(resolve < capture);
    // Loslassen drüben: Ablegen dort, und hier die Taste fürs System lösen.
    let a = pc.handle(Input::LocalButton {
        button: Button::Left,
        down: false,
    });
    assert!(a.contains(&Action::ReleaseLocal {
        button: Button::Left,
        x: 1919.0,
        y: 500.0
    }));
    assert_eq!(sent(&a), vec![Message::DragDrop]);
    assert!(!a.contains(&Action::DragEndedLocally));
}

#[test]
fn prefetch_is_discarded_when_released_locally() {
    let mut pc = engine(Side::Right, Os::Windows);
    pc.handle(Input::LocalButton {
        button: Button::Left,
        down: true,
    });
    assert_eq!(
        pc.handle(Input::LocalMove {
            x: 1800.0,
            y: 500.0
        }),
        vec![Action::PrefetchDrag]
    );
    let a = pc.handle(Input::LocalButton {
        button: Button::Left,
        down: false,
    });
    assert_eq!(a, vec![Action::DragEndedLocally]);
    // Normales Klicken ohne Ziehen am Rand löst nichts aus.
    pc.handle(Input::LocalButton {
        button: Button::Left,
        down: true,
    });
    assert!(pc
        .handle(Input::LocalButton {
            button: Button::Left,
            down: false
        })
        .is_empty());
}

#[test]
fn buttons_held_while_crossing_are_released_locally() {
    // #2: Rechte Taste gehalten über den Rand → das System hier hat nur
    // das Drücken gesehen. Beim Loslassen drüben auch hier lösen, und
    // drüben kein einzelnes „Loslassen“ ohne Drücken senden.
    let mut pc = engine(Side::Right, Os::Windows);
    pc.handle(Input::LocalButton {
        button: Button::Right,
        down: true,
    });
    pc.handle(Input::LocalMove { x: 1919.0, y: 20.0 });
    assert_eq!(pc.mode(), Mode::Controlling);
    let a = pc.handle(Input::LocalButton {
        button: Button::Right,
        down: false,
    });
    assert_eq!(
        a,
        vec![Action::ReleaseLocal {
            button: Button::Right,
            x: 1919.0,
            y: 20.0
        }]
    );
    // Danach normal: Drücken/Loslassen gehen rüber.
    let a = pc.handle(Input::LocalButton {
        button: Button::Right,
        down: true,
    });
    assert_eq!(
        sent(&a),
        vec![Message::MouseButton {
            button: Button::Right,
            down: true
        }]
    );
}

#[test]
fn returning_while_dragging_cancels_drag() {
    let mut pc = engine(Side::Right, Os::Windows);
    pc.handle(Input::LocalButton {
        button: Button::Left,
        down: true,
    });
    pc.handle(Input::LocalMove {
        x: 1919.0,
        y: 500.0,
    });
    let a = pc.handle(Input::Remote(Message::Leave { y: 0.5 }));
    assert!(sent(&a).contains(&Message::DragCancel));
    assert_eq!(pc.mode(), Mode::Local);
}

#[test]
fn injected_left_is_released_on_exit() {
    let mut mac = engine(Side::Left, Os::MacOs);
    mac.handle(Input::Connected {
        peer_os: Os::Windows,
    });
    mac.handle(Input::Remote(Message::Enter { y: 0.5 }));
    assert_eq!(mac.inject_left(true).len(), 1);
    assert!(mac.inject_left(true).is_empty());
    let a = mac.handle(Input::Remote(Message::Release));
    assert!(a.iter().any(|x| matches!(
        x,
        Action::Inject(Inject::Button {
            button: Button::Left,
            down: false,
            ..
        })
    )));
}

#[test]
fn clipboard_is_not_echoed_back() {
    let mut mac = engine(Side::Left, Os::MacOs);
    let a = mac.handle(Input::Remote(Message::Clipboard {
        text: "hallo".into(),
    }));
    assert_eq!(a, vec![Action::SetClipboard("hallo".into())]);
    // Unser Watcher sieht den neuen Inhalt – darf nicht zurückgeschickt werden.
    assert!(mac.handle(Input::LocalClipboard("hallo".into())).is_empty());
    let a = mac.handle(Input::LocalClipboard("neu".into()));
    assert_eq!(sent(&a), vec![Message::Clipboard { text: "neu".into() }]);
}

#[test]
fn unsupported_keys_are_reported_while_controlling() {
    let mut pc = engine(Side::Right, Os::Windows);
    assert!(pc.handle(Input::LocalUnsupportedKey("X".into())).is_empty());
    pc.handle(Input::LocalMove { x: 1919.0, y: 10.0 });
    let a = pc.handle(Input::LocalUnsupportedKey("X".into()));
    assert!(matches!(&a[..], [Action::Notice(n)] if n.contains("X")));
}

#[test]
fn inbound_clipboard_is_limited() {
    // #25
    let mut mac = engine(Side::Left, Os::MacOs);
    // Genau an der Grenze, mehrbyte-Zeichen (ä = 2 Bytes).
    let at_limit = "ä".repeat(MAX_CLIPBOARD_BYTES / 2);
    assert_eq!(at_limit.len(), MAX_CLIPBOARD_BYTES);
    let a = mac.handle(Input::Remote(Message::Clipboard {
        text: at_limit.clone(),
    }));
    assert_eq!(a, vec![Action::SetClipboard(at_limit)]);
    let over = format!("x{}", "ä".repeat(MAX_CLIPBOARD_BYTES / 2));
    assert!(mac
        .handle(Input::Remote(Message::Clipboard { text: over }))
        .is_empty());
    assert!(mac
        .handle(Input::Remote(Message::Clipboard {
            text: String::new()
        }))
        .is_empty());
    // Teilen aus: nichts wird gesetzt.
    mac.set_share_clipboard(false);
    assert!(mac
        .handle(Input::Remote(Message::Clipboard { text: "a".into() }))
        .is_empty());
}

#[test]
fn layout_is_mirrored_to_peer() {
    let mut pc = engine(Side::Right, Os::Windows);
    let a = pc.handle(Input::SetPeerSide(Side::Left));
    assert_eq!(
        sent(&a),
        vec![Message::Layout {
            i_am_on_your: Side::Right,
            align: Align::Stretch,
        }]
    );
    let a = pc.handle(Input::SetAlign(Align::Bottom));
    assert_eq!(
        sent(&a),
        vec![Message::Layout {
            i_am_on_your: Side::Right,
            align: Align::Bottom,
        }]
    );
    let mut mac = engine(Side::Left, Os::MacOs);
    let a = mac.handle(Input::Remote(Message::Layout {
        i_am_on_your: Side::Right,
        align: Align::Bottom,
    }));
    assert_eq!(
        a,
        vec![
            Action::PeerSideChanged(Side::Right),
            Action::AlignChanged(Align::Bottom)
        ]
    );
    assert_eq!(mac.peer_side(), Side::Right);
    assert_eq!(mac.align(), Align::Bottom);
}

#[test]
fn entry_height_follows_alignment() {
    // #4: PC 1080 hoch, Mac 900 hoch, Unterkanten bündig.
    let mut pc = engine(Side::Right, Os::Windows);
    pc.handle(Input::Remote(Message::Screen {
        width: 1440.0,
        height: 900.0,
    }));
    // Ohne Abgleich: proportional.
    let a = pc.handle(Input::LocalMove {
        x: 1919.0,
        y: 1079.0,
    });
    assert_eq!(sent(&a)[0], Message::Enter { y: 1.0 });
    pc.handle(Input::Remote(Message::Leave { y: 0.5 }));
    pc.handle(Input::SetAlign(Align::Bottom));
    // Oberhalb des Macs (PC-Zeile 100 < 180): an die Mac-Oberkante.
    for _ in 0..10 {
        pc.handle(Input::LocalMove {
            x: 1919.0,
            y: 100.0,
        });
    }
    assert_eq!(pc.mode(), Mode::Controlling);
    pc.handle(Input::Remote(Message::Leave { y: 0.5 }));
    pc.handle(Input::LocalMove { x: 1000.0, y: 0.0 });
    let a = pc.handle(Input::LocalMove {
        x: 1919.0,
        y: 629.0,
    });
    // PC-Zeile 629 → Mac-Zeile 449 von 899.
    match sent(&a)[0] {
        Message::Enter { y } => assert!((y as f64 * 899.0 - 449.0).abs() < 0.5, "{y}"),
        ref m => panic!("{m:?}"),
    }
    // Rückweg vom Mac: Mac-Zeile 0 → PC-Zeile 180.
    let mut mac = engine(Side::Left, Os::MacOs);
    mac.handle(Input::ScreenChanged(Rect::new(0.0, 0.0, 1440.0, 900.0)));
    mac.handle(Input::Remote(Message::Screen {
        width: 1920.0,
        height: 1080.0,
    }));
    mac.handle(Input::SetAlign(Align::Bottom));
    mac.handle(Input::Remote(Message::Enter { y: 0.0 }));
    mac.handle(Input::Remote(Message::MouseMove { dx: 50, dy: 0 }));
    let a = mac.handle(Input::Remote(Message::MouseMove { dx: -500, dy: 0 }));
    match sent(&a).last() {
        Some(Message::Leave { y }) => {
            assert!((*y as f64 * 1079.0 - 180.0).abs() < 0.5, "{y}")
        }
        m => panic!("{m:?}"),
    }
}

#[test]
fn screen_change_while_controlled_keeps_cursor_inside() {
    // #30: Auflösung/DPI ändert sich, während gesteuert wird.
    let mut mac = engine(Side::Left, Os::MacOs);
    mac.handle(Input::Remote(Message::Enter { y: 1.0 }));
    mac.handle(Input::Remote(Message::MouseMove { dx: 1800, dy: 0 }));
    let a = mac.handle(Input::ScreenChanged(Rect::new(0.0, 0.0, 1280.0, 800.0)));
    assert!(matches!(&sent(&a)[..], [Message::Screen { .. }]));
    let a = mac.handle(Input::Remote(Message::MouseMove { dx: 1, dy: 0 }));
    match &a[..] {
        [Action::Inject(Inject::MoveTo { x, y, .. })] => {
            assert!(*x <= 1279.0 && *y <= 799.0, "{x},{y}")
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn disconnect_while_controlling_frees_the_mouse() {
    let mut pc = engine(Side::Right, Os::Windows);
    pc.handle(Input::LocalMove { x: 1919.0, y: 10.0 });
    let a = pc.handle(Input::Disconnected);
    assert!(a.iter().any(|x| matches!(x, Action::StopCapture { .. })));
    assert_eq!(pc.mode(), Mode::Local);
}

#[test]
fn disconnect_while_controlled_releases_injected_keys() {
    let mut mac = engine(Side::Left, Os::MacOs);
    mac.handle(Input::Remote(Message::Enter { y: 0.5 }));
    mac.handle(Input::Remote(Message::Key {
        hid: 0x04,
        down: true,
    }));
    let a = mac.handle(Input::Disconnected);
    assert!(a.contains(&Action::Inject(Inject::Key {
        hid: 0x04,
        down: false
    })));
}

#[test]
fn handoff_hotkey() {
    let mut pc = engine(Side::Right, Os::Windows);
    pc.handle(Input::LocalKey {
        hid: hid::LEFT_CTRL,
        down: true,
    });
    pc.handle(Input::LocalKey {
        hid: hid::LEFT_ALT,
        down: true,
    });
    let a = pc.handle(Input::LocalKey {
        hid: hid::H,
        down: true,
    });
    assert!(a.contains(&Action::RequestHandoff));
}
