//! macOS: `CGEventTap` zum Abgreifen, `CGEventPost` zum Abspielen.
//!
//! Benötigte Berechtigungen (Systemeinstellungen → Datenschutz & Sicherheit):
//! * **Bedienungshilfen** – Ereignisse abspielen und abfangen.
//! * **Eingabeüberwachung** – Ereignisse mitlesen.
//! * **Automation (Finder, System Events)** – markierte Dateien bzw. das
//!   Dokument des aktiven Fensters lesen (Phase 2/3), wird beim ersten Mal erfragt.
//!
//! Beim Abfangen wird der Cursor mit `CGAssociateMouseAndMouseCursorPosition(false)`
//! eingefroren; die Bewegung kommt weiterhin als Delta in den Ereignissen an.
//! Abgefangene Ereignisse werden zu `Null`-Ereignissen umgeschrieben, die das
//! System verwirft.

use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

use core_foundation::base::TCFType;
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::CFDictionary;
use core_foundation::runloop::{kCFRunLoopCommonModes, CFRunLoop};
use core_foundation::string::CFString;
use core_graphics::display::CGDisplay;
use core_graphics::event::{
    CGEvent, CGEventFlags, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
    CGEventType, CGMouseButton, EventField, ScrollEventUnit,
};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::CGPoint;
use dc_core::{keymap, Button, Inject, Input, Rect};

use super::{file_url_to_path, run_capture, HandoffTarget, Sink, INJECT_MAGIC};

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrustedWithOptions(options: *const c_void) -> bool;
    static kAXTrustedCheckOptionPrompt: *const c_void;
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    // Private, aber seit Jahren stabil: erlaubt Hintergrund-Apps, den Zeiger zu verstecken.
    fn _CGSDefaultConnection() -> i32;
    fn CGSSetConnectionProperty(
        cid: i32,
        target: i32,
        key: *const c_void,
        value: *const c_void,
    ) -> i32;
    fn CGEventTapEnable(tap: *mut c_void, enable: bool);
    fn CGPreflightListenEventAccess() -> bool;
    fn CGRequestListenEventAccess() -> bool;
}

// ---------------------------------------------------------------- Abgreifen

static SINK: OnceLock<Sink> = OnceLock::new();
static CAPTURING: AtomicBool = AtomicBool::new(false);
static TAP_PORT: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

fn emit(i: Input) {
    if let Some(s) = SINK.get() {
        s(i);
    }
}

const MODIFIER_FLAGS: [(u16, u64); 9] = [
    (0x38, 0x0002_0000), // Shift links
    (0x3C, 0x0002_0000), // Shift rechts
    (0x3B, 0x0004_0000), // Ctrl links
    (0x3E, 0x0004_0000), // Ctrl rechts
    (0x3A, 0x0008_0000), // Option links
    (0x3D, 0x0008_0000), // Option rechts
    (0x37, 0x0010_0000), // Cmd links
    (0x36, 0x0010_0000), // Cmd rechts
    (0x39, 0x0001_0000), // Feststelltaste
];

fn modifier_mask(keycode: u16) -> Option<u64> {
    MODIFIER_FLAGS.iter().find(|m| m.0 == keycode).map(|m| m.1)
}

fn on_event(etype: CGEventType, ev: &CGEvent) -> bool {
    use CGEventType as T;
    if ev.get_integer_value_field(EventField::EVENT_SOURCE_USER_DATA) == INJECT_MAGIC as i64 {
        return false;
    }
    let capturing = CAPTURING.load(Ordering::SeqCst);
    match etype {
        T::MouseMoved | T::LeftMouseDragged | T::RightMouseDragged | T::OtherMouseDragged => {
            if capturing {
                let dx = ev.get_integer_value_field(EventField::MOUSE_EVENT_DELTA_X);
                let dy = ev.get_integer_value_field(EventField::MOUSE_EVENT_DELTA_Y);
                if dx != 0 || dy != 0 {
                    emit(Input::LocalDelta {
                        dx: dx as f64,
                        dy: dy as f64,
                    });
                }
            } else {
                let p = ev.location();
                emit(Input::LocalMove { x: p.x, y: p.y });
            }
        }
        T::LeftMouseDown | T::LeftMouseUp => emit(Input::LocalButton {
            button: Button::Left,
            down: matches!(etype, T::LeftMouseDown),
        }),
        T::RightMouseDown | T::RightMouseUp => emit(Input::LocalButton {
            button: Button::Right,
            down: matches!(etype, T::RightMouseDown),
        }),
        T::OtherMouseDown | T::OtherMouseUp => {
            let n = ev.get_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER);
            let button = match n {
                3 => Button::Back,
                4 => Button::Forward,
                _ => Button::Middle,
            };
            emit(Input::LocalButton {
                button,
                down: matches!(etype, T::OtherMouseDown),
            })
        }
        T::ScrollWheel => {
            let continuous =
                ev.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_IS_CONTINUOUS) != 0;
            let (dy, dx) = if continuous {
                // Trackpad: Pixel. 40 Pixel ≈ eine Raststufe (120).
                (
                    ev.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_POINT_DELTA_AXIS_1)
                        * 3,
                    ev.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_POINT_DELTA_AXIS_2)
                        * 3,
                )
            } else {
                (
                    ev.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_1) * 120,
                    ev.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_2) * 120,
                )
            };
            if dx != 0 || dy != 0 {
                emit(Input::LocalWheel {
                    dx: dx as i32,
                    dy: dy as i32,
                });
            }
        }
        T::KeyDown | T::KeyUp => {
            let kc = ev.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16;
            if let Some(hid) = keymap::mac_to_hid(kc) {
                emit(Input::LocalKey {
                    hid,
                    down: matches!(etype, T::KeyDown),
                });
            }
        }
        T::FlagsChanged => {
            let kc = ev.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16;
            if let (Some(hid), Some(mask)) = (keymap::mac_to_hid(kc), modifier_mask(kc)) {
                if hid == keymap::hid::CAPS_LOCK {
                    // Feststelltaste meldet nur den Zustand: als Druck+Loslassen senden.
                    emit(Input::LocalKey { hid, down: true });
                    emit(Input::LocalKey { hid, down: false });
                } else {
                    let down = ev.get_flags().bits() & mask != 0;
                    emit(Input::LocalKey { hid, down });
                }
            }
        }
        T::TapDisabledByTimeout | T::TapDisabledByUserInput => {
            let p = TAP_PORT.load(Ordering::SeqCst);
            if !p.is_null() {
                unsafe { CGEventTapEnable(p, true) };
            }
            return false;
        }
        _ => return false,
    }
    capturing
}

pub struct InputHandle;

impl InputHandle {
    pub fn set_capturing(&self, on: bool) {
        let was = CAPTURING.swap(on, Ordering::SeqCst);
        let _ = CGDisplay::associate_mouse_and_mouse_cursor_position(!on);
        // Zeiger hier verstecken, solange er drüben ist (Aufrufe sind gezählt,
        // daher nur beim Wechsel).
        if on && !was {
            allow_background_cursor_hide();
            let _ = CGDisplay::main().hide_cursor();
        } else if !on && was {
            let _ = CGDisplay::main().show_cursor();
        }
    }

    pub fn warp(&self, x: f64, y: f64) {
        let _ = CGDisplay::warp_mouse_cursor_position(CGPoint::new(x, y));
        // Nach dem Versetzen kurz die Mausbewegung „entkoppeln“ ist nicht nötig,
        // aber die Kopplung muss sicher wieder aktiv sein.
        let _ = CGDisplay::associate_mouse_and_mouse_cursor_position(true);
    }
}

fn allow_background_cursor_hide() {
    static DONE: AtomicBool = AtomicBool::new(false);
    if DONE.swap(true, Ordering::SeqCst) {
        return;
    }
    unsafe {
        let cid = _CGSDefaultConnection();
        let key = CFString::from_static_string("SetsCursorInBackground");
        CGSSetConnectionProperty(
            cid,
            cid,
            key.as_concrete_TypeRef() as *const c_void,
            CFBoolean::true_value().as_concrete_TypeRef() as *const c_void,
        );
    }
}

pub fn start_input(sink: Sink) -> Result<InputHandle, String> {
    if unsafe { !CGPreflightListenEventAccess() } {
        unsafe { CGRequestListenEventAccess() };
    }
    // Beim erneuten Versuch (Berechtigung nachträglich erteilt) bleibt der erste Empfänger.
    let _ = SINK.set(sink);
    let (tx, rx) = std::sync::mpsc::channel();
    thread::Builder::new()
        .name("dc-eventtap".into())
        .spawn(move || {
            use CGEventType as T;
            let events = vec![
                T::MouseMoved,
                T::LeftMouseDown,
                T::LeftMouseUp,
                T::RightMouseDown,
                T::RightMouseUp,
                T::LeftMouseDragged,
                T::RightMouseDragged,
                T::OtherMouseDown,
                T::OtherMouseUp,
                T::OtherMouseDragged,
                T::ScrollWheel,
                T::KeyDown,
                T::KeyUp,
                T::FlagsChanged,
            ];
            let tap = CGEventTap::new(
                CGEventTapLocation::Session,
                CGEventTapPlacement::HeadInsertEventTap,
                CGEventTapOptions::Default,
                events,
                |_proxy, etype, ev| {
                    if on_event(etype, ev) {
                        // Abfangen: Ereignis zu „Null“ machen, das System verwirft es.
                        ev.set_type(CGEventType::Null);
                    }
                    None
                },
            );
            let tap = match tap {
                Ok(t) => t,
                Err(()) => {
                    let _ = tx.send(Err(
                        "Event-Tap verweigert: Bedienungshilfen und Eingabeüberwachung erlauben"
                            .to_string(),
                    ));
                    return;
                }
            };
            TAP_PORT.store(
                tap.mach_port.as_concrete_TypeRef() as *mut c_void,
                Ordering::SeqCst,
            );
            let source = match tap.mach_port.create_runloop_source(0) {
                Ok(s) => s,
                Err(()) => {
                    let _ = tx.send(Err("Run-Loop-Quelle fehlgeschlagen".to_string()));
                    return;
                }
            };
            let rl = CFRunLoop::get_current();
            unsafe { rl.add_source(&source, kCFRunLoopCommonModes) };
            tap.enable();
            let _ = tx.send(Ok(()));
            CFRunLoop::run_current();
        })
        .map_err(|e| e.to_string())?;
    rx.recv_timeout(Duration::from_secs(5))
        .map_err(|_| "Event-Tap antwortet nicht".to_string())??;
    Ok(InputHandle)
}

pub fn screen_bounds() -> Rect {
    let ids = CGDisplay::active_displays().unwrap_or_default();
    let mut rects = ids.into_iter().map(|id| CGDisplay::new(id).bounds());
    let Some(first) = rects.next() else {
        let b = CGDisplay::main().bounds();
        return Rect::new(b.origin.x, b.origin.y, b.size.width, b.size.height);
    };
    let (mut x0, mut y0) = (first.origin.x, first.origin.y);
    let (mut x1, mut y1) = (x0 + first.size.width, y0 + first.size.height);
    for r in rects {
        x0 = x0.min(r.origin.x);
        y0 = y0.min(r.origin.y);
        x1 = x1.max(r.origin.x + r.size.width);
        y1 = y1.max(r.origin.y + r.size.height);
    }
    Rect::new(x0, y0, x1 - x0, y1 - y0)
}

// ---------------------------------------------------------------- Abspielen

pub struct Injector {
    source: CGEventSource,
    buttons: u8,
    flags: u64,
    last_click: Option<(Button, Instant, f64, f64, i64)>,
    pos: (f64, f64),
    scroll_rest: (i32, i32),
}

impl Injector {
    pub fn new() -> Result<Injector, String> {
        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .map_err(|_| "CGEventSource fehlgeschlagen".to_string())?;
        Ok(Injector {
            source,
            buttons: 0,
            flags: 0,
            last_click: None,
            pos: (0.0, 0.0),
            scroll_rest: (0, 0),
        })
    }

    fn post(&self, ev: CGEvent) {
        ev.set_integer_value_field(EventField::EVENT_SOURCE_USER_DATA, INJECT_MAGIC as i64);
        ev.post(CGEventTapLocation::HID);
    }

    fn mouse(&self, ty: CGEventType, button: CGMouseButton) -> Option<CGEvent> {
        CGEvent::new_mouse_event(
            self.source.clone(),
            ty,
            CGPoint::new(self.pos.0, self.pos.1),
            button,
        )
        .ok()
    }

    pub fn inject(&mut self, i: &Inject) {
        match *i {
            Inject::MoveTo { x, y, dx, dy } => {
                self.pos = (x, y);
                let (ty, b) = if self.buttons & Button::Left.bit() != 0 {
                    (CGEventType::LeftMouseDragged, CGMouseButton::Left)
                } else if self.buttons & Button::Right.bit() != 0 {
                    (CGEventType::RightMouseDragged, CGMouseButton::Right)
                } else if self.buttons != 0 {
                    (CGEventType::OtherMouseDragged, CGMouseButton::Center)
                } else {
                    (CGEventType::MouseMoved, CGMouseButton::Left)
                };
                if let Some(ev) = self.mouse(ty, b) {
                    ev.set_integer_value_field(EventField::MOUSE_EVENT_DELTA_X, dx as i64);
                    ev.set_integer_value_field(EventField::MOUSE_EVENT_DELTA_Y, dy as i64);
                    ev.set_flags(CGEventFlags::from_bits_truncate(self.flags));
                    self.post(ev);
                }
            }
            Inject::Button { button, down, x, y } => {
                self.pos = (x, y);
                if down {
                    self.buttons |= button.bit();
                } else {
                    self.buttons &= !button.bit();
                }
                let (ty, cg, number) = match (button, down) {
                    (Button::Left, true) => (CGEventType::LeftMouseDown, CGMouseButton::Left, 0),
                    (Button::Left, false) => (CGEventType::LeftMouseUp, CGMouseButton::Left, 0),
                    (Button::Right, true) => (CGEventType::RightMouseDown, CGMouseButton::Right, 1),
                    (Button::Right, false) => (CGEventType::RightMouseUp, CGMouseButton::Right, 1),
                    (b, d) => (
                        if d {
                            CGEventType::OtherMouseDown
                        } else {
                            CGEventType::OtherMouseUp
                        },
                        CGMouseButton::Center,
                        match b {
                            Button::Back => 3,
                            Button::Forward => 4,
                            _ => 2,
                        },
                    ),
                };
                // Doppelklicks: macOS erwartet den Klickzähler im Ereignis.
                let clicks = if down {
                    let n = match self.last_click {
                        Some((b, t, lx, ly, n))
                            if b == button
                                && t.elapsed() < Duration::from_millis(450)
                                && (lx - x).abs() < 5.0
                                && (ly - y).abs() < 5.0 =>
                        {
                            n + 1
                        }
                        _ => 1,
                    };
                    self.last_click = Some((button, Instant::now(), x, y, n));
                    n
                } else {
                    self.last_click.map(|c| c.4).unwrap_or(1)
                };
                if let Some(ev) = self.mouse(ty, cg) {
                    ev.set_integer_value_field(EventField::MOUSE_EVENT_CLICK_STATE, clicks);
                    if number >= 2 {
                        ev.set_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER, number);
                    }
                    ev.set_flags(CGEventFlags::from_bits_truncate(self.flags));
                    self.post(ev);
                }
            }
            Inject::Wheel { dx, dy } => {
                // 120 Einheiten = eine Raststufe ≈ 40 Pixel; Rest aufheben.
                let ty = self.scroll_rest.1 + dy;
                let tx = self.scroll_rest.0 + dx;
                let (py, px) = (ty / 3, tx / 3);
                self.scroll_rest = (tx - px * 3, ty - py * 3);
                if px != 0 || py != 0 {
                    if let Ok(ev) = CGEvent::new_scroll_event(
                        self.source.clone(),
                        ScrollEventUnit::PIXEL,
                        2,
                        py,
                        px,
                        0,
                    ) {
                        self.post(ev);
                    }
                }
            }
            Inject::Key { hid, down } => {
                let Some(kc) = keymap::hid_to_mac(hid) else {
                    return;
                };
                let modifier = modifier_mask(kc);
                if let Some(mask) = modifier {
                    if hid == keymap::hid::CAPS_LOCK {
                        if down {
                            self.flags ^= mask;
                        }
                    } else if down {
                        self.flags |= mask;
                    } else {
                        self.flags &= !mask;
                    }
                }
                if let Ok(ev) = CGEvent::new_keyboard_event(self.source.clone(), kc, down) {
                    if modifier.is_some() {
                        ev.set_type(CGEventType::FlagsChanged);
                    }
                    ev.set_flags(CGEventFlags::from_bits_truncate(self.flags));
                    self.post(ev);
                }
            }
        }
    }
}

/// Lokal laufenden Drag (im Finder) mit Esc abbrechen.
pub fn cancel_local_drag() {
    if let Ok(src) = CGEventSource::new(CGEventSourceStateID::HIDSystemState) {
        for down in [true, false] {
            if let Ok(ev) = CGEvent::new_keyboard_event(src.clone(), 0x35, down) {
                ev.set_integer_value_field(EventField::EVENT_SOURCE_USER_DATA, INJECT_MAGIC as i64);
                ev.post(CGEventTapLocation::HID);
            }
        }
    }
}

// ---------------------------------------------------------------- Dateien und Fenster

fn osascript(script: &str) -> Option<String> {
    run_capture("/usr/bin/osascript", &["-e", script]).map(|s| s.trim().to_string())
}

fn frontmost_app() -> Option<String> {
    osascript("tell application \"System Events\" to get name of first application process whose frontmost is true")
}

/// Ist der Finder vorne?
pub fn file_manager_in_front() -> bool {
    NSWorkspace::sharedWorkspace()
        .frontmostApplication()
        .and_then(|a| a.bundleIdentifier())
        .is_some_and(|b| b.to_string() == "com.apple.finder")
}

/// Markierte Dateien im Finder, wenn der Finder vorne ist.
pub fn selected_files() -> Vec<PathBuf> {
    if frontmost_app().as_deref() != Some("Finder") {
        return Vec::new();
    }
    let script = r#"tell application "Finder"
  set out to ""
  repeat with f in (get selection as alias list)
    set out to out & POSIX path of f & linefeed
  end repeat
  return out
end tell"#;
    osascript(script)
        .map(|s| {
            s.lines()
                .map(|l| l.trim_end_matches('/'))
                .filter(|l| !l.is_empty())
                .map(PathBuf::from)
                .filter(|p| p.exists())
                .collect()
        })
        .unwrap_or_default()
}

/// Phase 3: Webseite (Safari, Chromium-Browser) oder Dokument (über das
/// Bedienungshilfen-Attribut `AXDocument`) des aktiven Fensters.
pub fn handoff_target() -> Option<HandoffTarget> {
    let app = frontmost_app()?;
    let url = match app.as_str() {
        "Safari" | "Safari Technology Preview" => osascript(&format!(
            "tell application \"{app}\" to get URL of current tab of front window"
        )),
        "Google Chrome" | "Microsoft Edge" | "Brave Browser" | "Arc" | "Vivaldi" | "Chromium" => {
            osascript(&format!(
                "tell application \"{app}\" to get URL of active tab of front window"
            ))
        }
        _ => None,
    };
    if let Some(u) = url.filter(|u| u.starts_with("http://") || u.starts_with("https://")) {
        return Some(HandoffTarget::Url(u));
    }
    let doc = osascript(
        "tell application \"System Events\" to tell (first application process whose frontmost is true) to get value of attribute \"AXDocument\" of front window",
    )?;
    let path = file_url_to_path(&doc)?;
    path.is_file().then_some(HandoffTarget::File(path))
}

// ---------------------------------------------------------------- Dialoge

fn applescript_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

pub fn ask_text(title: &str, prompt: &str, default: &str) -> Option<String> {
    let script = format!(
        "text returned of (display dialog {} default answer {} with title {} buttons {{\"Abbrechen\", \"OK\"}} default button \"OK\")",
        applescript_string(prompt),
        applescript_string(default),
        applescript_string(title)
    );
    osascript(&script).filter(|s| !s.is_empty())
}

pub fn show_info(title: &str, text: &str) {
    let script = format!(
        "display dialog {} with title {} buttons {{\"OK\"}} default button \"OK\"",
        applescript_string(text),
        applescript_string(title)
    );
    let _ = osascript(&script);
}

pub fn missing_permissions(prompt: bool) -> Vec<String> {
    let mut missing = Vec::new();
    let trusted = unsafe {
        if prompt {
            let key = CFString::wrap_under_get_rule(kAXTrustedCheckOptionPrompt as _);
            let dict = CFDictionary::from_CFType_pairs(&[(
                key.as_CFType(),
                CFBoolean::true_value().as_CFType(),
            )]);
            AXIsProcessTrustedWithOptions(dict.as_concrete_TypeRef() as *const c_void)
        } else {
            AXIsProcessTrustedWithOptions(std::ptr::null())
        }
    };
    if !trusted {
        missing.push("Bedienungshilfen (Device Connector)".to_string());
    }
    if unsafe { !CGPreflightListenEventAccess() } {
        missing.push("Eingabeüberwachung (Device Connector)".to_string());
    }
    missing
}

pub fn open_permission_settings() {
    let _ = std::process::Command::new("open")
        .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")
        .status();
}

pub fn can_inject() -> bool {
    unsafe { AXIsProcessTrustedWithOptions(std::ptr::null()) }
}

/// Dateiauswahl-Dialog (für „Dateien senden …“).
pub fn pick_files() -> Vec<PathBuf> {
    let script = r#"set out to ""
set fs to choose file with prompt "Dateien an den anderen Rechner senden" with multiple selections allowed
repeat with f in fs
  set out to out & POSIX path of f & linefeed
end repeat
return out"#;
    osascript(script)
        .map(|s| {
            s.lines()
                .filter(|l| !l.is_empty())
                .map(PathBuf::from)
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------- Ziehen am Zeiger
//
// Dateien, die vom PC herübergezogen werden, liegen schon in einem
// verdeckten Ordner. Damit sie wie beim normalen Ziehen am Zeiger hängen,
// legt die App ein fast durchsichtiges Feld unter den Zeiger; der Dienst
// drückt dann die linke Taste (CGEventPost), das Feld bekommt `mouseDown:`
// und startet eine `NSDraggingSession`. Ab da bewegt der PC den Zeiger wie
// gewohnt (als „Dragged“-Ereignisse), und das Loslassen lässt die Dateien
// dort fallen, wo der Zeiger steht.

use std::cell::{Cell, RefCell};
use std::sync::mpsc;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{define_class, msg_send, AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSBackingStoreType, NSColor, NSDragOperation, NSDraggingContext, NSDraggingItem,
    NSDraggingSession, NSDraggingSource, NSEvent, NSPanel, NSResponder, NSScreenSaverWindowLevel,
    NSView, NSWindowStyleMask, NSWorkspace,
};
use objc2_foundation::{
    NSArray, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString, NSURL,
};

extern "C" {
    static _dispatch_main_q: u8;
    fn dispatch_async_f(queue: *const u8, context: *mut c_void, work: extern "C" fn(*mut c_void));
    fn dispatch_after_f(
        when: u64,
        queue: *const u8,
        context: *mut c_void,
        work: extern "C" fn(*mut c_void),
    );
    fn dispatch_time(when: u64, delta: i64) -> u64;
}

type MainJob = Box<dyn FnOnce() + Send>;

extern "C" fn run_main_job(ctx: *mut c_void) {
    let job: Box<MainJob> = unsafe { Box::from_raw(ctx as *mut MainJob) };
    job();
}

/// Auf dem Haupt-Thread ausführen (dort läuft AppKit), optional verzögert.
fn on_main(delay: Option<Duration>, f: impl FnOnce() + Send + 'static) {
    let job: Box<MainJob> = Box::new(Box::new(f));
    let ctx = Box::into_raw(job) as *mut c_void;
    unsafe {
        let q = std::ptr::addr_of!(_dispatch_main_q);
        match delay {
            None => dispatch_async_f(q, ctx, run_main_job),
            Some(d) => {
                dispatch_after_f(dispatch_time(0, d.as_nanos() as i64), q, ctx, run_main_job)
            }
        }
    }
}

type OnDragEnd = Box<dyn FnOnce(bool) + Send>;
type DragPanel = (u64, Retained<NSPanel>, Retained<DragView>);

pub struct DragIvars {
    items: Vec<PathBuf>,
    started: Cell<bool>,
    on_end: RefCell<Option<OnDragEnd>>,
}

define_class!(
    // SAFETY: NSView darf abgeleitet werden; kein eigenes Drop.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "DCFileDragView"]
    #[ivars = DragIvars]
    struct DragView;

    impl DragView {
        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            self.begin(event);
        }
    }

    unsafe impl NSObjectProtocol for DragView {}

    unsafe impl NSDraggingSource for DragView {
        #[unsafe(method(draggingSession:sourceOperationMaskForDraggingContext:))]
        fn operation_mask(
            &self,
            _session: &NSDraggingSession,
            _context: NSDraggingContext,
        ) -> NSDragOperation {
            NSDragOperation::Copy
        }

        #[unsafe(method(draggingSession:endedAtPoint:operation:))]
        fn ended(&self, _session: &NSDraggingSession, _point: NSPoint, op: NSDragOperation) {
            let dropped = op != NSDragOperation::None;
            log::info!("Ziehen beendet (Wirkung {})", op.0);
            self.finish(dropped);
        }
    }
);

impl DragView {
    fn begin(&self, event: &NSEvent) {
        let iv = self.ivars();
        if iv.started.replace(true) {
            return;
        }
        let ws = NSWorkspace::sharedWorkspace();
        let mut items: Vec<Retained<NSDraggingItem>> = Vec::new();
        for (i, p) in iv.items.iter().enumerate() {
            let path = NSString::from_str(&p.to_string_lossy());
            let url = NSURL::fileURLWithPath(&path);
            let item = NSDraggingItem::initWithPasteboardWriter(
                NSDraggingItem::alloc(),
                ProtocolObject::from_ref(&*url),
            );
            let icon = ws.iconForFile(&path);
            let icon: &AnyObject = &icon;
            let off = (i.min(4) as f64) * 6.0;
            unsafe {
                item.setDraggingFrame_contents(
                    NSRect::new(NSPoint::new(off, off), NSSize::new(48.0, 48.0)),
                    Some(icon),
                );
            }
            items.push(item);
        }
        let arr = NSArray::from_retained_slice(&items);
        let _session = self.beginDraggingSessionWithItems_event_source(
            &arr,
            event,
            ProtocolObject::from_ref(self),
        );
        // Feld aus dem Weg schieben, damit die Ziele darunter das Ziehen sehen.
        if let Some(w) = self.window() {
            w.setFrameOrigin(NSPoint::new(-10000.0, -10000.0));
        }
    }

    fn finish(&self, dropped: bool) {
        if let Some(f) = self.ivars().on_end.borrow_mut().take() {
            f(dropped);
        }
        if let Some(w) = self.window() {
            w.orderOut(None);
        }
    }
}

thread_local! {
    /// Fenster des aktuellen Ziehens (nur auf dem Haupt-Thread benutzt).
    static DRAG_PANEL: RefCell<Option<DragPanel>> = const { RefCell::new(None) };
}

static DRAG_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Griff auf ein laufendes Ziehen.
pub struct FileDrag;

impl FileDrag {
    /// Esc bricht eine laufende Drag-Session ab.
    pub fn cancel(&self) {
        cancel_local_drag();
    }
}

pub fn start_file_drag(
    items: &[PathBuf],
    x: f64,
    y: f64,
    on_end: Box<dyn FnOnce(bool) + Send>,
) -> Result<FileDrag, String> {
    let items = items.to_vec();
    let generation = DRAG_GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    // Cocoa zählt y von unten am Hauptbildschirm, CoreGraphics von oben.
    let main_h = CGDisplay::main().bounds().size.height;
    let (tx, rx) = mpsc::channel::<()>();
    on_main(None, move || {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        const SIZE: f64 = 48.0;
        let frame = NSRect::new(
            NSPoint::new(x - SIZE / 2.0, main_h - y - SIZE / 2.0),
            NSSize::new(SIZE, SIZE),
        );
        let style = NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel;
        let panel: Retained<NSPanel> = unsafe {
            msg_send![
                NSPanel::alloc(mtm),
                initWithContentRect: frame,
                styleMask: style,
                backing: NSBackingStoreType::Buffered,
                defer: false
            ]
        };
        unsafe { panel.setReleasedWhenClosed(false) };
        panel.setOpaque(false);
        panel.setHasShadow(false);
        // Fast durchsichtig, aber klickbar.
        panel.setBackgroundColor(Some(&NSColor::colorWithCalibratedWhite_alpha(0.0, 0.02)));
        panel.setIgnoresMouseEvents(false);
        panel.setLevel(NSScreenSaverWindowLevel);

        let view = DragView::alloc(mtm).set_ivars(DragIvars {
            items,
            started: Cell::new(false),
            on_end: RefCell::new(Some(on_end)),
        });
        let view: Retained<DragView> = unsafe {
            msg_send![
                super(view),
                initWithFrame: NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(SIZE, SIZE))
            ]
        };
        panel.setContentView(Some(&view));
        panel.orderFrontRegardless();
        DRAG_PANEL.with(|d| {
            if let Some((_, old, _)) = d.borrow_mut().replace((generation, panel, view)) {
                old.orderOut(None);
            }
        });
        let _ = tx.send(());
    });
    // Kommt der Klick nicht an, nicht ewig unter dem Zeiger liegen bleiben.
    on_main(Some(Duration::from_secs(3)), move || {
        DRAG_PANEL.with(|d| {
            let mut d = d.borrow_mut();
            let stale =
                matches!(&*d, Some((g, _, v)) if *g == generation && !v.ivars().started.get());
            if stale {
                if let Some((_, panel, view)) = d.take() {
                    log::warn!("Ziehen: Klick kam nicht am Hilfsfeld an");
                    view.finish(false);
                    panel.orderOut(None);
                }
            }
        });
    });
    rx.recv_timeout(Duration::from_secs(2))
        .map_err(|_| "Haupt-Thread antwortet nicht".to_string())?;
    Ok(FileDrag)
}
