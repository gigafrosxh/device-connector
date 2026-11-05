//! Tray- (Windows) bzw. Menüleisten-Symbol (macOS) mit dem Menü.
//!
//! Bewusst wenige Einstellungen: Wo steht der andere Rechner, welche
//! Verbindung, Kopplungscode, Zwischenablage an/aus.

use std::thread;

use dc_core::transport::list_serial_ports;
use dc_core::{pairing, Mode, Side};
use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tray_icon::menu::{
    CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu,
};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use crate::config::{Config, TransportChoice};
use crate::platform;
use crate::service::{self, LinkState, ServiceHandle, Status, UiCommand};

const HELP_URL: &str = "https://github.com/gigafrosxh/device-connector#readme";

#[derive(Debug)]
enum UserEvent {
    Status(Status),
    Menu(MenuEvent),
    PairingCode(String),
    Transport(TransportChoice),
}

struct Items {
    status: MenuItem,
    note: MenuItem,
    side_left: CheckMenuItem,
    side_right: CheckMenuItem,
    conn_menu: Submenu,
    ports: Vec<(MenuId, CheckMenuItem, String)>,
    conn_extra: Vec<Box<dyn tray_icon::menu::IsMenuItem>>,
    tcp_listen: CheckMenuItem,
    tcp_connect: CheckMenuItem,
    rescan: MenuItem,
    disconnect: MenuItem,
    code_show: MenuItem,
    code_enter: MenuItem,
    clipboard: CheckMenuItem,
    swap: CheckMenuItem,
    received: MenuItem,
    send_files: MenuItem,
    permissions: MenuItem,
    help: MenuItem,
    quit: MenuItem,
}

pub fn run(cfg: Config) {
    let mut builder = EventLoopBuilder::<UserEvent>::with_user_event();
    let mut event_loop = builder.build();
    #[cfg(target_os = "macos")]
    {
        use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
        event_loop.set_activation_policy(ActivationPolicy::Accessory);
    }
    #[cfg(not(target_os = "macos"))]
    let _ = &mut event_loop;

    let proxy = event_loop.create_proxy();
    {
        let proxy = proxy.clone();
        MenuEvent::set_event_handler(Some(move |e| {
            let _ = proxy.send_event(UserEvent::Menu(e));
        }));
    }

    let (svc, _join) = {
        let proxy = proxy.clone();
        service::start(cfg.clone(), move |s| {
            let _ = proxy.send_event(UserEvent::Status(s));
        })
    };

    let mut cfg = cfg;
    let mut tray: Option<TrayIcon> = None;
    let mut items: Option<Items> = None;
    let mut last_status: Option<Status> = None;

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::NewEvents(StartCause::Init) => {
                let (menu, it) = build_menu(&cfg);
                let icon = make_icon(IconState::Off);
                tray = TrayIconBuilder::new()
                    .with_menu(Box::new(menu))
                    .with_tooltip("Device Connector")
                    .with_icon(icon)
                    .build()
                    .map_err(|e| log::error!("Tray-Symbol: {e}"))
                    .ok();
                items = Some(it);
                let missing = platform::missing_permissions(true);
                if !missing.is_empty() {
                    thread::spawn(move || {
                        platform::show_info(
                            "Device Connector braucht Berechtigungen",
                            &format!(
                                "Bitte in den Systemeinstellungen unter „Datenschutz & Sicherheit“ erlauben:\n\n• {}\n\nDanach Device Connector neu starten.",
                                missing.join("\n• ")
                            ),
                        );
                        platform::open_permission_settings();
                    });
                }
            }
            Event::UserEvent(UserEvent::Status(s)) => {
                if let (Some(t), Some(it)) = (&tray, &items) {
                    apply_status(t, it, &s);
                }
                cfg.peer_side = s.peer_side;
                last_status = Some(s);
            }
            Event::UserEvent(UserEvent::PairingCode(code)) => {
                cfg.pairing_code = code.clone();
                if let Some(it) = &items {
                    it.code_show.set_text(format!("Kopplungscode: {code}"));
                }
                svc.command(UiCommand::SetPairingCode(code));
            }
            Event::UserEvent(UserEvent::Transport(t)) => {
                cfg.transport = t.clone();
                if let Some(it) = &mut items {
                    refresh_connection_checks(it, &cfg.transport);
                }
                svc.command(UiCommand::SetTransport(t));
            }
            Event::UserEvent(UserEvent::Menu(e)) => {
                let Some(it) = &mut items else { return };
                if on_menu(e.id(), it, &mut cfg, &svc, &proxy) {
                    svc.command(UiCommand::Quit);
                    tray.take();
                    *control_flow = ControlFlow::Exit;
                }
                let _ = &last_status;
            }
            _ => {}
        }
    });
}

fn build_menu(cfg: &Config) -> (Menu, Items) {
    let menu = Menu::new();
    let status = MenuItem::new("○ Nicht verbunden", false, None);
    let note = MenuItem::new("", false, None);

    let side_menu = Submenu::new("Anderer Rechner steht …", true);
    let side_left = CheckMenuItem::new("… links", true, cfg.peer_side == Side::Left, None);
    let side_right = CheckMenuItem::new("… rechts", true, cfg.peer_side == Side::Right, None);
    let _ = side_menu.append_items(&[&side_left, &side_right]);

    let conn_menu = Submenu::new("Verbindung", true);
    let tcp_listen = CheckMenuItem::new("Netzwerk: auf Verbindung warten", true, false, None);
    let tcp_connect = CheckMenuItem::new("Netzwerk: mit Adresse verbinden …", true, false, None);
    let rescan = MenuItem::new("Bluetooth-Ports neu suchen", true, None);
    let disconnect = MenuItem::new("Trennen", true, None);

    let code_show = MenuItem::new(format!("Kopplungscode: {}", cfg.pairing_code), true, None);
    let code_enter = MenuItem::new("Kopplungscode eingeben …", true, None);
    let clipboard = CheckMenuItem::new("Zwischenablage teilen", true, cfg.share_clipboard, None);
    let swap = CheckMenuItem::new("Strg und Cmd tauschen", true, cfg.swap_ctrl_cmd, None);
    let received = MenuItem::new("Empfangene Dateien öffnen", true, None);
    let send_files = MenuItem::new("Dateien senden …", true, None);
    let permissions = MenuItem::new("Berechtigungen prüfen …", cfg!(target_os = "macos"), None);
    let help = MenuItem::new("Hilfe", true, None);
    let quit = MenuItem::new("Beenden", true, None);

    let _ = menu.append_items(&[
        &status,
        &note,
        &PredefinedMenuItem::separator(),
        &side_menu,
        &conn_menu,
        &PredefinedMenuItem::separator(),
        &code_show,
        &code_enter,
        &PredefinedMenuItem::separator(),
        &clipboard,
        &swap,
        &send_files,
        &received,
        &PredefinedMenuItem::separator(),
        &permissions,
        &help,
        &quit,
    ]);

    let mut items = Items {
        status,
        note,
        side_left,
        side_right,
        conn_menu,
        ports: Vec::new(),
        conn_extra: Vec::new(),
        tcp_listen,
        tcp_connect,
        rescan,
        disconnect,
        code_show,
        code_enter,
        clipboard,
        swap,
        received,
        send_files,
        permissions,
        help,
        quit,
    };
    rebuild_connection_menu(&mut items, &cfg.transport);
    (menu, items)
}

fn rebuild_connection_menu(it: &mut Items, current: &TransportChoice) {
    for (_, item, _) in it.ports.drain(..) {
        let _ = it.conn_menu.remove(&item);
    }
    for extra in it.conn_extra.drain(..) {
        let _ = it.conn_menu.remove(extra.as_ref());
    }
    let _ = it.conn_menu.remove(&it.tcp_listen);
    let _ = it.conn_menu.remove(&it.tcp_connect);
    let _ = it.conn_menu.remove(&it.rescan);
    let _ = it.conn_menu.remove(&it.disconnect);

    let header = MenuItem::new("Bluetooth / Seriell:", false, None);
    let _ = it.conn_menu.append(&header);
    it.conn_extra.push(Box::new(header));

    let ports = list_serial_ports();
    if ports.is_empty() {
        let none = MenuItem::new("   (kein Port gefunden – siehe Hilfe)", false, None);
        let _ = it.conn_menu.append(&none);
        it.conn_extra.push(Box::new(none));
    }
    for p in ports {
        let item = CheckMenuItem::new(format!("   {}", p.label), true, false, None);
        let _ = it.conn_menu.append(&item);
        it.ports.push((item.id().clone(), item, p.name));
    }
    let sep = PredefinedMenuItem::separator();
    let _ = it.conn_menu.append(&sep);
    it.conn_extra.push(Box::new(sep));
    let _ = it
        .conn_menu
        .append_items(&[&it.tcp_listen, &it.tcp_connect]);
    let sep2 = PredefinedMenuItem::separator();
    let _ = it.conn_menu.append(&sep2);
    it.conn_extra.push(Box::new(sep2));
    let _ = it.conn_menu.append_items(&[&it.rescan, &it.disconnect]);
    refresh_connection_checks(it, current);
}

fn refresh_connection_checks(it: &mut Items, current: &TransportChoice) {
    for (_, item, name) in &it.ports {
        item.set_checked(matches!(current, TransportChoice::Serial { port } if port == name));
    }
    it.tcp_listen
        .set_checked(matches!(current, TransportChoice::TcpListen));
    it.tcp_connect
        .set_checked(matches!(current, TransportChoice::TcpConnect { .. }));
    if let TransportChoice::TcpConnect { addr } = current {
        it.tcp_connect
            .set_text(format!("Netzwerk: verbinden mit {addr} …"));
    } else {
        it.tcp_connect.set_text("Netzwerk: mit Adresse verbinden …");
    }
}

/// Gibt `true` zurück, wenn beendet werden soll.
fn on_menu(
    id: &MenuId,
    it: &mut Items,
    cfg: &mut Config,
    svc: &ServiceHandle,
    proxy: &EventLoopProxy<UserEvent>,
) -> bool {
    if id == it.quit.id() {
        return true;
    }
    if id == it.side_left.id() || id == it.side_right.id() {
        let side = if id == it.side_left.id() {
            Side::Left
        } else {
            Side::Right
        };
        cfg.peer_side = side;
        it.side_left.set_checked(side == Side::Left);
        it.side_right.set_checked(side == Side::Right);
        svc.command(UiCommand::SetPeerSide(side));
    } else if let Some((_, _, name)) = it.ports.iter().find(|(pid, _, _)| pid == id) {
        cfg.transport = TransportChoice::Serial { port: name.clone() };
        refresh_connection_checks(it, &cfg.transport);
        svc.command(UiCommand::SetTransport(cfg.transport.clone()));
    } else if id == it.tcp_listen.id() {
        cfg.transport = TransportChoice::TcpListen;
        refresh_connection_checks(it, &cfg.transport);
        svc.command(UiCommand::SetTransport(cfg.transport.clone()));
    } else if id == it.tcp_connect.id() {
        refresh_connection_checks(it, &cfg.transport); // Haken erst nach Eingabe
        let current = match &cfg.transport {
            TransportChoice::TcpConnect { addr } => addr.clone(),
            _ => String::new(),
        };
        let proxy = proxy.clone();
        thread::spawn(move || {
            if let Some(addr) = platform::ask_text(
                "Device Connector",
                "IP-Adresse oder Name des anderen Rechners (dort „Netzwerk: auf Verbindung warten“ wählen):",
                &current,
            ) {
                let addr = addr.trim().to_string();
                if !addr.is_empty() {
                    let _ = proxy.send_event(UserEvent::Transport(TransportChoice::TcpConnect { addr }));
                }
            }
        });
    } else if id == it.rescan.id() {
        rebuild_connection_menu(it, &cfg.transport);
    } else if id == it.disconnect.id() {
        cfg.transport = TransportChoice::None;
        refresh_connection_checks(it, &cfg.transport);
        svc.command(UiCommand::SetTransport(TransportChoice::None));
    } else if id == it.code_show.id() {
        let code = cfg.pairing_code.clone();
        thread::spawn(move || {
            platform::show_info(
                "Kopplungscode",
                &format!(
                    "Dieser Code muss auf beiden Rechnern gleich sein:\n\n{code}\n\nAm anderen Rechner „Kopplungscode eingeben …“ wählen und ihn eintippen."
                ),
            )
        });
    } else if id == it.code_enter.id() {
        let proxy = proxy.clone();
        thread::spawn(move || {
            let Some(code) = platform::ask_text(
                "Kopplungscode",
                "Kopplungscode vom anderen Rechner (z. B. K7QM-3XPA-9TRW):",
                "",
            ) else {
                return;
            };
            if pairing::is_valid(&code) {
                let n = pairing::normalize(&code);
                let pretty = format!("{}-{}-{}", &n[0..4], &n[4..8], &n[8..12]);
                let _ = proxy.send_event(UserEvent::PairingCode(pretty));
            } else {
                platform::show_info(
                    "Kopplungscode",
                    "Der Code ist ungültig. Er hat 12 Zeichen, z. B. K7QM-3XPA-9TRW.",
                );
            }
        });
    } else if id == it.clipboard.id() {
        cfg.share_clipboard = it.clipboard.is_checked();
        svc.command(UiCommand::SetShareClipboard(cfg.share_clipboard));
    } else if id == it.swap.id() {
        cfg.swap_ctrl_cmd = it.swap.is_checked();
        svc.command(UiCommand::SetSwapCtrlCmd(cfg.swap_ctrl_cmd));
    } else if id == it.send_files.id() {
        let svc = svc.clone();
        thread::spawn(move || {
            let files = platform::pick_files();
            if !files.is_empty() {
                svc.command(UiCommand::SendFiles(files));
            }
        });
    } else if id == it.received.id() {
        let dir = cfg.receive_dir();
        let _ = std::fs::create_dir_all(&dir);
        let _ = opener::open(&dir);
    } else if id == it.permissions.id() {
        let missing = platform::missing_permissions(true);
        thread::spawn(move || {
            if missing.is_empty() {
                platform::show_info(
                    "Berechtigungen",
                    "Alle nötigen Berechtigungen sind erteilt.",
                );
            } else {
                platform::show_info(
                    "Berechtigungen",
                    &format!(
                        "Es fehlt:\n\n• {}\n\nDanach Device Connector neu starten.",
                        missing.join("\n• ")
                    ),
                );
                platform::open_permission_settings();
            }
        });
    } else if id == it.help.id() {
        let _ = opener::open(HELP_URL);
    }
    false
}

fn apply_status(tray: &TrayIcon, it: &Items, s: &Status) {
    let (text, icon) = match (&s.link, s.mode) {
        (_, Mode::Controlling) => (
            format!(
                "● Maus ist bei {}",
                s.peer_name.as_deref().unwrap_or("Gegenstelle")
            ),
            IconState::Away,
        ),
        (_, Mode::Controlled) => (
            format!(
                "● {} steuert diesen Rechner",
                s.peer_name.as_deref().unwrap_or("Gegenstelle")
            ),
            IconState::On,
        ),
        (LinkState::Connected, _) => (
            format!(
                "● Verbunden mit {}",
                s.peer_name.as_deref().unwrap_or("Gegenstelle")
            ),
            IconState::On,
        ),
        (LinkState::Connecting, _) => {
            (format!("◌ Suche Gegenstelle … ({})", s.via), IconState::Off)
        }
        (LinkState::Idle, _) => ("○ Keine Verbindung gewählt".to_string(), IconState::Off),
        (LinkState::Error(e), _) => (format!("⚠ {e}"), IconState::Error),
    };
    let text = if s.input_ok {
        text
    } else {
        format!("{text}  (Maus/Tastatur: Berechtigung fehlt)")
    };
    it.status.set_text(&text);
    it.note.set_text(s.note.clone().unwrap_or_default());
    it.side_left.set_checked(s.peer_side == Side::Left);
    it.side_right.set_checked(s.peer_side == Side::Right);
    let _ = tray.set_icon(Some(make_icon(icon)));
    let _ = tray.set_tooltip(Some(format!("Device Connector – {text}")));
}

#[derive(Clone, Copy)]
enum IconState {
    Off,
    On,
    Away,
    Error,
}

/// Symbol: zwei Bildschirme nebeneinander, Farbe zeigt den Zustand.
fn make_icon(state: IconState) -> Icon {
    const N: usize = 32;
    let (frame, fill_l, fill_r): ([u8; 4], [u8; 4], [u8; 4]) = match state {
        IconState::Off => (
            [140, 140, 140, 255],
            [200, 200, 200, 255],
            [200, 200, 200, 255],
        ),
        IconState::On => ([30, 120, 60, 255], [80, 200, 120, 255], [80, 200, 120, 255]),
        IconState::Away => (
            [20, 90, 180, 255],
            [170, 200, 240, 255],
            [60, 140, 240, 255],
        ),
        IconState::Error => (
            [180, 60, 30, 255],
            [240, 150, 120, 255],
            [240, 150, 120, 255],
        ),
    };
    let mut px = vec![0u8; N * N * 4];
    let mut put = |x: usize, y: usize, c: [u8; 4]| {
        let i = (y * N + x) * 4;
        px[i..i + 4].copy_from_slice(&c);
    };
    for (x0, fill) in [(1usize, fill_l), (17usize, fill_r)] {
        for y in 7..22 {
            for x in x0..x0 + 14 {
                let border = y == 7 || y == 21 || x == x0 || x == x0 + 13;
                put(x, y, if border { frame } else { fill });
            }
        }
        // Standfuß
        for y in 22..25 {
            put(x0 + 6, y, frame);
            put(x0 + 7, y, frame);
        }
        for x in x0 + 3..x0 + 11 {
            put(x, 25, frame);
        }
    }
    Icon::from_rgba(px, N as u32, N as u32).expect("gültiges Symbol")
}
