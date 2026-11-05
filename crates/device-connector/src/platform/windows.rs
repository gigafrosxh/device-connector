//! Windows 11: Low-Level-Hooks zum Abgreifen, `SendInput` zum Abspielen.
//!
//! * Maus und Tastatur: `WH_MOUSE_LL` / `WH_KEYBOARD_LL` in einem eigenen
//!   Thread mit Nachrichtenschleife. Beim Abfangen gibt der Hook `1` zurück,
//!   dann sieht Windows das Ereignis nicht. Der Cursor wird in die Mitte des
//!   Hauptbildschirms gesetzt und die Bewegung als Differenz gemeldet.
//! * Eigene, per `SendInput` erzeugte Ereignisse tragen [`INJECT_MAGIC`] in
//!   `dwExtraInfo` und werden vom Hook durchgelassen.
//! * Programme mit Admin-Rechten nehmen von uns keine Eingaben an (UIPI),
//!   solange Device Connector nicht selbst als Admin läuft.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

use dc_core::{keymap, Button, Inject, Input, Rect};
use windows::core::{HSTRING, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{
    GetCurrentThreadId, OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT,
    KEYBD_EVENT_FLAGS, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE,
    MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
    MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN,
    MOUSEEVENTF_XUP, MOUSEINPUT, MOUSE_EVENT_FLAGS, VIRTUAL_KEY, VK_CONTROL, VK_LWIN, VK_MENU,
    VK_RWIN, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetCursorPos, GetForegroundWindow, GetMessageW,
    GetSystemMetrics, GetWindowThreadProcessId, MessageBoxW, PostThreadMessageW, SetCursorPos,
    SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, HHOOK, KBDLLHOOKSTRUCT,
    LLKHF_EXTENDED, LLKHF_UP, MB_ICONINFORMATION, MB_OK, MSG, MSLLHOOKSTRUCT, SM_CXSCREEN,
    SM_CXVIRTUALSCREEN, SM_CYSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    WH_KEYBOARD_LL, WH_MOUSE_LL, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_QUIT,
    WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_XBUTTONDOWN, WM_XBUTTONUP,
};

use super::{run_capture, HandoffTarget, Sink, INJECT_MAGIC};

/// Muss vor dem Anlegen von Fenstern laufen: echte Pixel statt skalierter.
pub fn init_process() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

// ---------------------------------------------------------------- Abgreifen

static SINK: OnceLock<Sink> = OnceLock::new();
static CAPTURING: AtomicBool = AtomicBool::new(false);
static CENTER_X: AtomicI32 = AtomicI32::new(0);
static CENTER_Y: AtomicI32 = AtomicI32::new(0);
static HOOK_THREAD: AtomicU32 = AtomicU32::new(0);

fn emit(i: Input) {
    if let Some(s) = SINK.get() {
        s(i);
    }
}

unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code < 0 {
        return CallNextHookEx(HHOOK::default(), code, wparam, lparam);
    }
    let info = &*(lparam.0 as *const MSLLHOOKSTRUCT);
    if info.dwExtraInfo == INJECT_MAGIC {
        return CallNextHookEx(HHOOK::default(), code, wparam, lparam);
    }
    let capturing = CAPTURING.load(Ordering::SeqCst);
    let msg = wparam.0 as u32;
    let hi = ((info.mouseData >> 16) & 0xFFFF) as u16;
    match msg {
        WM_MOUSEMOVE => {
            if capturing {
                let cx = CENTER_X.load(Ordering::Relaxed);
                let cy = CENTER_Y.load(Ordering::Relaxed);
                let (dx, dy) = (info.pt.x - cx, info.pt.y - cy);
                if dx != 0 || dy != 0 {
                    emit(Input::LocalDelta {
                        dx: dx as f64,
                        dy: dy as f64,
                    });
                }
            } else {
                emit(Input::LocalMove {
                    x: info.pt.x as f64,
                    y: info.pt.y as f64,
                });
            }
        }
        WM_LBUTTONDOWN | WM_LBUTTONUP => emit(Input::LocalButton {
            button: Button::Left,
            down: msg == WM_LBUTTONDOWN,
        }),
        WM_RBUTTONDOWN | WM_RBUTTONUP => emit(Input::LocalButton {
            button: Button::Right,
            down: msg == WM_RBUTTONDOWN,
        }),
        WM_MBUTTONDOWN | WM_MBUTTONUP => emit(Input::LocalButton {
            button: Button::Middle,
            down: msg == WM_MBUTTONDOWN,
        }),
        WM_XBUTTONDOWN | WM_XBUTTONUP => {
            let button = if hi == 1 {
                Button::Back
            } else {
                Button::Forward
            };
            emit(Input::LocalButton {
                button,
                down: msg == WM_XBUTTONDOWN,
            })
        }
        WM_MOUSEWHEEL => emit(Input::LocalWheel {
            dx: 0,
            dy: hi as i16 as i32,
        }),
        WM_MOUSEHWHEEL => emit(Input::LocalWheel {
            dx: hi as i16 as i32,
            dy: 0,
        }),
        _ => {}
    }
    if capturing {
        return LRESULT(1);
    }
    CallNextHookEx(HHOOK::default(), code, wparam, lparam)
}

unsafe extern "system" fn keyboard_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code < 0 {
        return CallNextHookEx(HHOOK::default(), code, wparam, lparam);
    }
    let info = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
    if info.dwExtraInfo == INJECT_MAGIC {
        return CallNextHookEx(HHOOK::default(), code, wparam, lparam);
    }
    let msg = wparam.0 as u32;
    let down = matches!(msg, WM_KEYDOWN | WM_SYSKEYDOWN) && (info.flags.0 & LLKHF_UP.0) == 0;
    let up = matches!(msg, WM_KEYUP | WM_SYSKEYUP);
    let extended = (info.flags.0 & LLKHF_EXTENDED.0) != 0;
    let mut sc = info.scanCode as u16;
    // NumLock meldet sich als „erweitert“, ist im Scancode-Set aber 0x45.
    if extended && sc != 0x45 {
        sc |= 0xE000;
    }
    // Manche Treiber liefern für die Windows-Tasten keinen Scancode.
    if sc == 0 || sc == 0xE000 {
        sc = match VIRTUAL_KEY(info.vkCode as u16) {
            VK_LWIN => 0xE05B,
            VK_RWIN => 0xE05C,
            _ => 0,
        };
    }
    if let Some(hid) = keymap::win_to_hid(sc) {
        if down || up {
            emit(Input::LocalKey { hid, down });
        }
    }
    if CAPTURING.load(Ordering::SeqCst) {
        return LRESULT(1);
    }
    CallNextHookEx(HHOOK::default(), code, wparam, lparam)
}

pub struct InputHandle;

impl InputHandle {
    pub fn set_capturing(&self, on: bool) {
        if on {
            // Mitte des Hauptbildschirms: von dort aus misst der Hook die Bewegung.
            let (w, h) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
            CENTER_X.store(w / 2, Ordering::SeqCst);
            CENTER_Y.store(h / 2, Ordering::SeqCst);
            CAPTURING.store(true, Ordering::SeqCst);
            unsafe {
                let _ = SetCursorPos(w / 2, h / 2);
            }
        } else {
            CAPTURING.store(false, Ordering::SeqCst);
        }
    }

    pub fn warp(&self, x: f64, y: f64) {
        unsafe {
            let _ = SetCursorPos(x.round() as i32, y.round() as i32);
        }
    }
}

impl Drop for InputHandle {
    fn drop(&mut self) {
        CAPTURING.store(false, Ordering::SeqCst);
        let t = HOOK_THREAD.load(Ordering::SeqCst);
        if t != 0 {
            unsafe {
                let _ = PostThreadMessageW(t, WM_QUIT, WPARAM(0), LPARAM(0));
            }
        }
    }
}

pub fn start_input(sink: Sink) -> Result<InputHandle, String> {
    // Beim erneuten Versuch (Berechtigung nachträglich erteilt) bleibt der erste Empfänger.
    let _ = SINK.set(sink);
    let (tx, rx) = std::sync::mpsc::channel();
    thread::Builder::new()
        .name("dc-hooks".into())
        .spawn(move || unsafe {
            HOOK_THREAD.store(GetCurrentThreadId(), Ordering::SeqCst);
            let module = GetModuleHandleW(None).ok();
            let hinst = module
                .map(|m| windows::Win32::Foundation::HINSTANCE(m.0))
                .unwrap_or_default();
            let mouse = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), hinst, 0);
            let keyboard = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), hinst, 0);
            match (&mouse, &keyboard) {
                (Ok(_), Ok(_)) => {
                    let _ = tx.send(Ok(()));
                }
                _ => {
                    let _ = tx.send(Err(format!(
                        "Hooks fehlgeschlagen: {:?} {:?}",
                        mouse.as_ref().err(),
                        keyboard.as_ref().err()
                    )));
                    return;
                }
            }
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, HWND::default(), 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            if let Ok(h) = mouse {
                let _ = UnhookWindowsHookEx(h);
            }
            if let Ok(h) = keyboard {
                let _ = UnhookWindowsHookEx(h);
            }
        })
        .map_err(|e| e.to_string())?;
    rx.recv_timeout(Duration::from_secs(5))
        .map_err(|_| "Hook-Thread antwortet nicht".to_string())??;
    Ok(InputHandle)
}

pub fn screen_bounds() -> Rect {
    unsafe {
        Rect::new(
            GetSystemMetrics(SM_XVIRTUALSCREEN) as f64,
            GetSystemMetrics(SM_YVIRTUALSCREEN) as f64,
            GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1) as f64,
            GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1) as f64,
        )
    }
}

// ---------------------------------------------------------------- Abspielen

pub struct Injector;

fn send(inputs: &[INPUT]) {
    unsafe {
        SendInput(inputs, std::mem::size_of::<INPUT>() as i32);
    }
}

fn mouse_input(dx: i32, dy: i32, data: i32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data as u32,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: INJECT_MAGIC,
            },
        },
    }
}

fn key_input(scancode: u16, down: bool) -> INPUT {
    let mut flags = KEYEVENTF_SCANCODE;
    if scancode & 0xFF00 == 0xE000 {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    if !down {
        flags |= KEYEVENTF_KEYUP;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: scancode & 0xFF,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: INJECT_MAGIC,
            },
        },
    }
}

impl Injector {
    pub fn new() -> Result<Injector, String> {
        Ok(Injector)
    }

    pub fn inject(&mut self, i: &Inject) {
        match *i {
            Inject::MoveTo { x, y, .. } => {
                let r = screen_bounds();
                let nx = (((x - r.x) * 65535.0) / (r.w - 1.0).max(1.0)).round() as i32;
                let ny = (((y - r.y) * 65535.0) / (r.h - 1.0).max(1.0)).round() as i32;
                send(&[mouse_input(
                    nx,
                    ny,
                    0,
                    MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                )]);
            }
            Inject::Button { button, down, .. } => {
                let (flags, data) = match (button, down) {
                    (Button::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
                    (Button::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
                    (Button::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
                    (Button::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
                    (Button::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
                    (Button::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
                    (Button::Back, true) => (MOUSEEVENTF_XDOWN, 1),
                    (Button::Back, false) => (MOUSEEVENTF_XUP, 1),
                    (Button::Forward, true) => (MOUSEEVENTF_XDOWN, 2),
                    (Button::Forward, false) => (MOUSEEVENTF_XUP, 2),
                };
                send(&[mouse_input(0, 0, data, flags)]);
            }
            Inject::Wheel { dx, dy } => {
                if dy != 0 {
                    send(&[mouse_input(0, 0, dy, MOUSEEVENTF_WHEEL)]);
                }
                if dx != 0 {
                    send(&[mouse_input(0, 0, dx, MOUSEEVENTF_HWHEEL)]);
                }
            }
            Inject::Key { hid, down } => {
                if let Some(sc) = keymap::hid_to_win(hid) {
                    send(&[key_input(sc, down)]);
                }
            }
        }
    }
}

/// Lokal laufenden Drag (im Explorer) mit Esc abbrechen.
pub fn cancel_local_drag() {
    send(&[key_input(0x01, true), key_input(0x01, false)]);
}

// ---------------------------------------------------------------- Dateien und Fenster

/// Markierte Dateien im vordersten Explorer-Fenster (über Shell.Application).
pub fn selected_files() -> Vec<PathBuf> {
    let hwnd = unsafe { GetForegroundWindow() };
    let script = format!(
        "$ErrorActionPreference='SilentlyContinue'; \
         [Console]::OutputEncoding=[Text.Encoding]::UTF8; \
         $sh = New-Object -ComObject Shell.Application; \
         foreach ($w in $sh.Windows()) {{ if ($w.HWND -eq {h}) {{ foreach ($i in $w.Document.SelectedItems()) {{ $i.Path }} }} }}",
        h = hwnd.0 as isize
    );
    run_capture(
        "powershell.exe",
        &["-NoProfile", "-NonInteractive", "-Command", &script],
    )
    .map(|out| {
        out.lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(PathBuf::from)
            .filter(|p| p.exists())
            .collect()
    })
    .unwrap_or_default()
}

fn foreground_exe() -> Option<String> {
    unsafe {
        let hwnd = GetForegroundWindow();
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 {
            return None;
        }
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok =
            QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len);
        let _ = CloseHandle(h);
        ok.ok()?;
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        Some(path.rsplit('\\').next().unwrap_or("").to_lowercase())
    }
}

fn modifiers_held() -> bool {
    unsafe {
        [VK_CONTROL, VK_MENU, VK_SHIFT, VK_LWIN, VK_RWIN]
            .iter()
            .any(|vk| (GetAsyncKeyState(vk.0 as i32) as u16 & 0x8000) != 0)
    }
}

/// Phase 3: Browser-Adresse des aktiven Fensters. Unter Windows gibt es keine
/// allgemeine Schnittstelle dafür, deshalb: Strg+L, Strg+C, Esc und die
/// Zwischenablage danach wiederherstellen.
pub fn handoff_target() -> Option<HandoffTarget> {
    let exe = foreground_exe()?;
    const BROWSERS: [&str; 6] = [
        "chrome.exe",
        "msedge.exe",
        "firefox.exe",
        "brave.exe",
        "opera.exe",
        "vivaldi.exe",
    ];
    if !BROWSERS.contains(&exe.as_str()) {
        log::info!("Fenster-Übergabe: {exe} wird noch nicht unterstützt");
        return None;
    }
    // Warten, bis Strg+Alt+H losgelassen sind, sonst wird daraus Strg+Alt+L.
    let start = Instant::now();
    while modifiers_held() && start.elapsed() < Duration::from_secs(2) {
        thread::sleep(Duration::from_millis(20));
    }
    let mut cb = arboard::Clipboard::new().ok()?;
    let saved = cb.get_text().ok();
    let ctrl = 0x1D;
    let combo = |sc: u16| {
        send(&[
            key_input(ctrl, true),
            key_input(sc, true),
            key_input(sc, false),
            key_input(ctrl, false),
        ])
    };
    combo(0x26); // L
    thread::sleep(Duration::from_millis(120));
    combo(0x2E); // C
    thread::sleep(Duration::from_millis(200));
    send(&[key_input(0x01, true), key_input(0x01, false)]);
    let url = cb.get_text().ok();
    if let Some(s) = saved {
        let _ = cb.set_text(s);
    }
    let url = url?.trim().to_string();
    if url.starts_with("http://") || url.starts_with("https://") {
        Some(HandoffTarget::Url(url))
    } else {
        None
    }
}

// ---------------------------------------------------------------- Dialoge

pub fn ask_text(title: &str, prompt: &str, default: &str) -> Option<String> {
    let esc = |s: &str| s.replace('\'', "''");
    let script = format!(
        "[Console]::OutputEncoding=[Text.Encoding]::UTF8; Add-Type -AssemblyName Microsoft.VisualBasic; \
         [Microsoft.VisualBasic.Interaction]::InputBox('{}', '{}', '{}')",
        esc(prompt),
        esc(title),
        esc(default)
    );
    let out = run_capture(
        "powershell.exe",
        &["-NoProfile", "-NonInteractive", "-Command", &script],
    )?;
    let t = out.trim().to_string();
    if t.is_empty() {
        None
    } else {
        Some(t)
    }
}

pub fn show_info(title: &str, text: &str) {
    unsafe {
        MessageBoxW(
            HWND::default(),
            &HSTRING::from(text),
            &HSTRING::from(title),
            MB_OK | MB_ICONINFORMATION,
        );
    }
}

pub fn missing_permissions(_prompt: bool) -> Vec<String> {
    Vec::new()
}

pub fn open_permission_settings() {}

#[allow(dead_code)]
fn cursor_pos() -> (f64, f64) {
    let mut p = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut p);
    }
    (p.x as f64, p.y as f64)
}

#[allow(dead_code)]
const _: KEYBD_EVENT_FLAGS = KEYEVENTF_SCANCODE;

pub fn can_inject() -> bool {
    true
}
