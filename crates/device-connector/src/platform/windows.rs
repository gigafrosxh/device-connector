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
    VK_PAUSE, VK_RWIN, VK_SHIFT,
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
    // Falls ein früherer Lauf abgestürzt ist, während der Zeiger versteckt war.
    show_cursor();
}

// ---------------------------------------------------------------- Zeiger verstecken

/// Alle Systemzeiger durch einen durchsichtigen ersetzen. Windows kennt kein
/// globales „Zeiger aus“, so machen es auch andere Programme dieser Art.
fn hide_cursor() {
    use windows::Win32::UI::WindowsAndMessaging::{
        CopyImage, CreateCursor, DestroyCursor, SetSystemCursor, HCURSOR, IMAGE_CURSOR,
        IMAGE_FLAGS, SYSTEM_CURSOR_ID,
    };
    // Normal, IBeam, Wait, Cross, UpArrow, Size…, No, Hand, AppStarting, Help, Pin, Person.
    const IDS: [u32; 17] = [
        32512, 32513, 32514, 32515, 32516, 32640, 32641, 32642, 32643, 32644, 32645, 32646, 32648,
        32649, 32650, 32651, 32671,
    ];
    let and_mask = [0xFFu8; 128];
    let xor_mask = [0u8; 128];
    unsafe {
        let hinst = GetModuleHandleW(None).unwrap_or_default();
        let blank = match CreateCursor(
            hinst,
            0,
            0,
            32,
            32,
            and_mask.as_ptr() as _,
            xor_mask.as_ptr() as _,
        ) {
            Ok(c) => c,
            Err(e) => {
                log::warn!("Zeiger verstecken: CreateCursor fehlgeschlagen: {e}");
                return;
            }
        };
        let mut ok = 0;
        let mut first_err = None;
        for id in IDS {
            // SetSystemCursor übernimmt (und zerstört) den Zeiger, daher je eine Kopie.
            let copy = match CopyImage(
                windows::Win32::Foundation::HANDLE(blank.0),
                IMAGE_CURSOR,
                0,
                0,
                IMAGE_FLAGS(0),
            ) {
                Ok(h) => h,
                Err(e) => {
                    first_err.get_or_insert(format!("CopyImage: {e}"));
                    continue;
                }
            };
            match SetSystemCursor(HCURSOR(copy.0), SYSTEM_CURSOR_ID(id)) {
                Ok(()) => ok += 1,
                Err(e) => {
                    first_err.get_or_insert(format!("SetSystemCursor({id}): {e}"));
                }
            }
        }
        let _ = DestroyCursor(blank);
        match first_err {
            Some(e) if ok == 0 => log::warn!("Zeiger verstecken fehlgeschlagen: {e}"),
            Some(e) => log::debug!("Zeiger versteckt ({ok}/{}), Fehler: {e}", IDS.len()),
            None => log::debug!("Zeiger versteckt ({ok} Zeigerformen)"),
        }
    }
}

fn show_cursor() {
    use windows::Win32::UI::WindowsAndMessaging::{
        SystemParametersInfoW, SPI_SETCURSORS, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
    };
    unsafe {
        let _ = SystemParametersInfoW(
            SPI_SETCURSORS,
            0,
            None,
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
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
        WM_LBUTTONDOWN | WM_LBUTTONUP => {
            log::debug!(
                "Hook: linke Taste {} bei ({}, {})",
                if msg == WM_LBUTTONDOWN {
                    "gedrückt"
                } else {
                    "los"
                },
                info.pt.x,
                info.pt.y
            );
            emit(Input::LocalButton {
                button: Button::Left,
                down: msg == WM_LBUTTONDOWN,
            })
        }
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
    // Pause und NumLock melden beide Scancode 0x45; Pause erkennt man am VK.
    if VIRTUAL_KEY(info.vkCode as u16) == VK_PAUSE {
        sc = keymap::WIN_PAUSE;
    }
    // Manche Treiber liefern für die Windows-Tasten keinen Scancode.
    if sc == 0 || sc == 0xE000 {
        sc = match VIRTUAL_KEY(info.vkCode as u16) {
            VK_LWIN => 0xE05B,
            VK_RWIN => 0xE05C,
            _ => 0,
        };
    }
    match keymap::win_to_hid(sc) {
        Some(hid) => {
            if down || up {
                emit(Input::LocalKey { hid, down });
            }
        }
        None => {
            // Nicht still verschlucken (#28): beim ersten Mal melden. Abgefangen
            // wird sie trotzdem, damit z. B. Medientasten nicht hier wirken,
            // während man drüben arbeitet.
            if down
                && CAPTURING.load(Ordering::SeqCst)
                && super::first_unsupported("Windows", (info.vkCode << 16) | sc as u32)
            {
                emit(Input::LocalUnsupportedKey(format!(
                    "VK {:#04x}",
                    info.vkCode
                )));
            }
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
            let was = CAPTURING.swap(true, Ordering::SeqCst);
            if !was {
                hide_cursor();
            }
            unsafe {
                // Windows zeichnet die neue Zeigerform erst bei einer Bewegung:
                // einmal hin und her, damit der leere Zeiger sofort greift.
                let _ = SetCursorPos(w / 2 + 1, h / 2);
                let _ = SetCursorPos(w / 2, h / 2);
            }
            // Versetzen kann scheitern (z. B. Fenster mit Adminrechten vorne,
            // Sperrbildschirm). Dann bliebe der Zeiger am Rand stehen (#2).
            if let Some((x, y)) = cursor_pos() {
                if (x as i32, y as i32) != (w / 2, h / 2) {
                    log::warn!(
                        "Zeiger ließ sich nicht in die Mitte setzen (steht bei {x:.0},{y:.0}); läuft ein Programm mit Administratorrechten im Vordergrund?"
                    );
                    crate::trace!("center_failed cursor=({x:.0},{y:.0})");
                }
            }
        } else if CAPTURING.swap(false, Ordering::SeqCst) {
            show_cursor();
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
        if CAPTURING.swap(false, Ordering::SeqCst) {
            show_cursor();
        }
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

fn vk_input(vk: VIRTUAL_KEY, down: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: if down {
                    KEYBD_EVENT_FLAGS(0)
                } else {
                    KEYEVENTF_KEYUP
                },
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
            Inject::Key { hid, down } => match keymap::hid_to_win(hid) {
                Some(keymap::WIN_PAUSE) => send(&[vk_input(VK_PAUSE, down)]),
                Some(sc) => send(&[key_input(sc, down)]),
                None => {
                    super::first_unsupported("abspielen", hid as u32);
                }
            },
        }
    }
}

/// Ist ein Explorer-Fenster oder der Desktop vorne?
pub fn file_manager_in_front() -> bool {
    let hwnd = unsafe { GetForegroundWindow() };
    let mut buf = [0u16; 128];
    let n = unsafe { windows::Win32::UI::WindowsAndMessaging::GetClassNameW(hwnd, &mut buf) };
    let class = String::from_utf16_lossy(&buf[..n.max(0) as usize]);
    matches!(
        class.as_str(),
        "CabinetWClass" | "ExploreWClass" | "Progman" | "WorkerW"
    )
}

/// Lokal laufenden Drag (im Explorer) mit Esc abbrechen.
pub fn cancel_local_drag() {
    send(&[key_input(0x01, true), key_input(0x01, false)]);
}

// ---------------------------------------------------------------- Dateien und Fenster

/// Markierte Dateien im vordersten Explorer-Fenster (über Shell.Application).
pub fn selected_files() -> Vec<PathBuf> {
    let hwnd = unsafe { GetForegroundWindow() };
    let class = unsafe {
        let mut buf = [0u16; 128];
        let n = windows::Win32::UI::WindowsAndMessaging::GetClassNameW(hwnd, &mut buf);
        String::from_utf16_lossy(&buf[..n.max(0) as usize])
    };
    log::info!(
        "Vorderstes Fenster beim Ziehen: HWND {} Klasse {class}",
        hwnd.0 as isize
    );
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
    // Die Zwischenablage des Nutzers komplett sichern (alle Formate, auch
    // Bilder und Dateien), nicht nur Text (#26).
    let saved = clip::snapshot();
    let before = clip::sequence();
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
                 // Warten, bis der Browser kopiert hat (höchstens 1 s).
    let start = Instant::now();
    while clip::sequence() == before && start.elapsed() < Duration::from_secs(1) {
        thread::sleep(Duration::from_millis(20));
    }
    send(&[key_input(0x01, true), key_input(0x01, false)]);
    let ours = clip::sequence();
    if ours == before {
        log::info!("Fenster-Übergabe: Browser hat nichts kopiert");
        return None;
    }
    let url = arboard::Clipboard::new().ok()?.get_text().ok();
    // Nur zurücksetzen, wenn seitdem niemand sonst etwas kopiert hat.
    if clip::sequence() == ours {
        match &saved {
            Some(s) => {
                if !clip::restore(s) {
                    log::warn!("Fenster-Übergabe: Zwischenablage nicht wiederhergestellt");
                }
            }
            None => log::warn!("Fenster-Übergabe: Zwischenablage war nicht lesbar"),
        }
    } else {
        log::info!("Zwischenablage wurde inzwischen geändert, nicht zurückgesetzt");
    }
    let url = url?.trim().to_string();
    if url.starts_with("http://") || url.starts_with("https://") {
        Some(HandoffTarget::Url(url))
    } else {
        None
    }
}

/// Zwischenablage mit allen Formaten sichern und wiederherstellen.
mod clip {
    use std::thread;
    use std::time::Duration;
    use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND};
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, EnumClipboardFormats, GetClipboardData,
        GetClipboardSequenceNumber, OpenClipboard, SetClipboardData,
    };
    use windows::Win32::System::Memory::{
        GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
    };

    /// Formate, deren Daten kein Speicherblock sind (GDI-Handles); Windows
    /// erzeugt CF_BITMAP beim Wiederherstellen aus CF_DIB selbst.
    const NOT_HGLOBAL: [u32; 8] = [2, 3, 9, 14, 0x80, 0x82, 0x83, 0x8E];

    pub struct Snapshot(Vec<(u32, Vec<u8>)>);

    #[cfg(test)]
    impl Snapshot {
        pub fn from_formats(v: Vec<(u32, Vec<u8>)>) -> Snapshot {
            Snapshot(v)
        }
        pub fn get(&self, f: u32) -> Option<&[u8]> {
            self.0
                .iter()
                .find(|(g, _)| *g == f)
                .map(|(_, d)| d.as_slice())
        }
    }

    pub fn sequence() -> u32 {
        unsafe { GetClipboardSequenceNumber() }
    }

    fn open() -> bool {
        for _ in 0..25 {
            if unsafe { OpenClipboard(HWND::default()) }.is_ok() {
                return true;
            }
            thread::sleep(Duration::from_millis(20));
        }
        false
    }

    pub fn snapshot() -> Option<Snapshot> {
        if !open() {
            return None;
        }
        let mut out = Vec::new();
        let mut f = 0u32;
        loop {
            f = unsafe { EnumClipboardFormats(f) };
            if f == 0 {
                break;
            }
            if NOT_HGLOBAL.contains(&f) {
                continue;
            }
            let Ok(h) = (unsafe { GetClipboardData(f) }) else {
                continue;
            };
            let g = HGLOBAL(h.0);
            let size = unsafe { GlobalSize(g) };
            if size == 0 {
                continue;
            }
            let p = unsafe { GlobalLock(g) };
            if p.is_null() {
                continue;
            }
            let bytes = unsafe { std::slice::from_raw_parts(p as *const u8, size) }.to_vec();
            unsafe {
                let _ = GlobalUnlock(g);
            }
            out.push((f, bytes));
        }
        unsafe {
            let _ = CloseClipboard();
        }
        Some(Snapshot(out))
    }

    pub fn restore(s: &Snapshot) -> bool {
        if !open() {
            return false;
        }
        unsafe {
            let _ = EmptyClipboard();
        }
        for (f, bytes) in &s.0 {
            let Ok(g) = (unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes.len()) }) else {
                continue;
            };
            let p = unsafe { GlobalLock(g) };
            if p.is_null() {
                unsafe {
                    let _ = GlobalFree(g);
                }
                continue;
            }
            unsafe {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), p as *mut u8, bytes.len());
                let _ = GlobalUnlock(g);
            }
            // Bei Erfolg gehört der Speicher jetzt der Zwischenablage.
            if unsafe { SetClipboardData(*f, HANDLE(g.0)) }.is_err() {
                unsafe {
                    let _ = GlobalFree(g);
                }
            }
        }
        unsafe {
            let _ = CloseClipboard();
        }
        true
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

/// Ja/Nein-Rückfrage; `true` nur bei „Ja“.
pub fn confirm(title: &str, text: &str, _yes: &str) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{
        IDYES, MB_DEFBUTTON2, MB_ICONQUESTION, MB_SETFOREGROUND, MB_TOPMOST, MB_YESNO,
    };
    let r = unsafe {
        MessageBoxW(
            HWND::default(),
            &HSTRING::from(text),
            &HSTRING::from(title),
            MB_YESNO | MB_ICONQUESTION | MB_DEFBUTTON2 | MB_TOPMOST | MB_SETFOREGROUND,
        )
    };
    r == IDYES
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

/// Aktuelle Zeigerposition (für die Diagnose, #30).
pub fn cursor_pos() -> Option<(f64, f64)> {
    let mut p = POINT::default();
    unsafe { GetCursorPos(&mut p) }.ok()?;
    Some((p.x as f64, p.y as f64))
}

#[allow(dead_code)]
const _: KEYBD_EVENT_FLAGS = KEYEVENTF_SCANCODE;

pub fn can_inject() -> bool {
    true
}

/// Dateiauswahl-Dialog (für „Dateien senden …“).
pub fn pick_files() -> Vec<PathBuf> {
    let script = "[Console]::OutputEncoding=[Text.Encoding]::UTF8; \
        Add-Type -AssemblyName System.Windows.Forms; \
        $f = New-Object System.Windows.Forms.Form -Property @{TopMost=$true}; \
        $d = New-Object System.Windows.Forms.OpenFileDialog; \
        $d.Multiselect = $true; $d.Title = 'Dateien an den anderen Rechner senden'; \
        if ($d.ShowDialog($f) -eq 'OK') { $d.FileNames }";
    run_capture(
        "powershell.exe",
        &["-NoProfile", "-NonInteractive", "-STA", "-Command", script],
    )
    .map(|out| {
        out.lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(PathBuf::from)
            .collect()
    })
    .unwrap_or_default()
}

// ---------------------------------------------------------------- Ziehen am Zeiger
//
// Dateien, die vom Mac herübergezogen werden, liegen schon in einem
// verdeckten Ordner. Damit sie wie beim normalen Ziehen am Zeiger hängen,
// legen wir ein fast unsichtbares Fensterchen unter den Zeiger; der Dienst
// drückt dann die linke Taste (SendInput), das Fenster bekommt
// `WM_LBUTTONDOWN` und startet `DoDragDrop`. Ab da bewegt der Mac den Zeiger
// wie gewohnt, und das Loslassen dort lässt die Dateien hier fallen.

use std::cell::RefCell;
use std::sync::{mpsc, Arc};

use windows::core::{implement, w, HRESULT};
use windows::Win32::Foundation::{
    BOOL, COLORREF, DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS, S_OK,
};
use windows::Win32::System::Com::{CoTaskMemFree, IBindCtx, IDataObject};
use windows::Win32::System::Ole::{
    DoDragDrop, IDropSource, IDropSource_Impl, OleInitialize, OleUninitialize, DROPEFFECT,
    DROPEFFECT_COPY, DROPEFFECT_NONE,
};
use windows::Win32::System::SystemServices::{MK_LBUTTON, MODIFIERKEYS_FLAGS};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    BHID_DataObject, SHCreateShellItemArrayFromIDLists, SHParseDisplayName,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, KillTimer, PostQuitMessage, RegisterClassW,
    SetLayeredWindowAttributes, SetTimer, ShowWindow, LWA_ALPHA, MA_NOACTIVATE, SW_HIDE,
    SW_SHOWNOACTIVATE, WM_DESTROY, WM_MOUSEACTIVATE, WM_TIMER, WNDCLASSW, WS_EX_LAYERED,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

#[implement(IDropSource)]
struct DropSource {
    cancel: Arc<AtomicBool>,
}

impl IDropSource_Impl for DropSource_Impl {
    fn QueryContinueDrag(&self, escape: BOOL, keys: MODIFIERKEYS_FLAGS) -> HRESULT {
        if self.cancel.load(Ordering::SeqCst) || escape.as_bool() {
            DRAGDROP_S_CANCEL
        } else if keys.0 & MK_LBUTTON.0 == 0 {
            DRAGDROP_S_DROP
        } else {
            S_OK
        }
    }

    fn GiveFeedback(&self, _effect: DROPEFFECT) -> HRESULT {
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}

struct PendingDrag {
    data: IDataObject,
    source: IDropSource,
    on_end: Option<Box<dyn FnOnce(bool) + Send>>,
}

thread_local! {
    static PENDING_DRAG: RefCell<Option<PendingDrag>> = const { RefCell::new(None) };
}

const DRAG_TIMER: usize = 1;

/// Griff auf ein laufendes Ziehen.
pub struct FileDrag {
    cancel: Arc<AtomicBool>,
}

impl FileDrag {
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }
}

/// Explorer-Datenobjekt für die Dateien (wie beim Ziehen aus dem Explorer).
fn data_object_for(items: &[PathBuf]) -> windows::core::Result<IDataObject> {
    let mut pidls: Vec<*const ITEMIDLIST> = Vec::new();
    let mut res = Ok(());
    for p in items {
        let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
        let path = HSTRING::from(p.as_os_str());
        res = unsafe { SHParseDisplayName(&path, None::<&IBindCtx>, &mut pidl, 0, None) };
        if res.is_err() {
            break;
        }
        pidls.push(pidl);
    }
    let out = res.and_then(|()| unsafe {
        let arr = SHCreateShellItemArrayFromIDLists(&pidls)?;
        arr.BindToHandler::<_, IDataObject>(None::<&IBindCtx>, &BHID_DataObject)
    });
    for p in pidls {
        unsafe { CoTaskMemFree(Some(p as *const _)) };
    }
    out
}

pub fn start_file_drag(
    items: &[PathBuf],
    x: f64,
    y: f64,
    on_end: Box<dyn FnOnce(bool) + Send>,
) -> Result<FileDrag, String> {
    let cancel = Arc::new(AtomicBool::new(false));
    let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
    let items = items.to_vec();
    let c = cancel.clone();
    thread::Builder::new()
        .name("dc-drag".into())
        .spawn(move || drag_thread(items, x, y, c, on_end, ready_tx))
        .map_err(|e| e.to_string())?;
    match ready_rx.recv_timeout(Duration::from_secs(3)) {
        Ok(Ok(())) => Ok(FileDrag { cancel }),
        Ok(Err(e)) => Err(e),
        Err(_) => Err("Zeitüberschreitung beim Vorbereiten".into()),
    }
}

fn drag_thread(
    items: Vec<PathBuf>,
    x: f64,
    y: f64,
    cancel: Arc<AtomicBool>,
    on_end: Box<dyn FnOnce(bool) + Send>,
    ready: mpsc::Sender<Result<(), String>>,
) {
    unsafe {
        if let Err(e) = OleInitialize(None) {
            let _ = ready.send(Err(format!("OleInitialize: {e}")));
            return;
        }
        let data = match data_object_for(&items) {
            Ok(d) => d,
            Err(e) => {
                let _ = ready.send(Err(format!("Datenobjekt: {e}")));
                OleUninitialize();
                return;
            }
        };
        let source: IDropSource = DropSource { cancel }.into();
        PENDING_DRAG.with(|d| {
            *d.borrow_mut() = Some(PendingDrag {
                data,
                source,
                on_end: Some(on_end),
            })
        });

        let hinst = GetModuleHandleW(None).unwrap_or_default();
        let class = w!("DeviceConnectorDrag");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(drag_wndproc),
            hInstance: hinst.into(),
            lpszClassName: class,
            ..Default::default()
        };
        // Schlägt beim zweiten Mal fehl (schon registriert), das ist in Ordnung.
        RegisterClassW(&wc);
        const SIZE: i32 = 64;
        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            class,
            w!(""),
            WS_POPUP,
            x.round() as i32 - SIZE / 2,
            y.round() as i32 - SIZE / 2,
            SIZE,
            SIZE,
            None,
            None,
            hinst,
            None,
        );
        let hwnd = match hwnd {
            Ok(h) => h,
            Err(e) => {
                PENDING_DRAG.with(|d| d.borrow_mut().take());
                let _ = ready.send(Err(format!("Fenster: {e}")));
                OleUninitialize();
                return;
            }
        };
        // Fast durchsichtig, aber klickbar.
        let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 1, LWA_ALPHA);
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        // Kommt der Klick nicht an, nicht ewig unter dem Zeiger liegen bleiben.
        SetTimer(hwnd, DRAG_TIMER, 3000, None);
        let _ = ready.send(Ok(()));

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        if let Some(p) = PENDING_DRAG.with(|d| d.borrow_mut().take()) {
            if let Some(f) = p.on_end {
                f(false);
            }
        }
        OleUninitialize();
    }
}

unsafe extern "system" fn drag_wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_LBUTTONDOWN => {
            let _ = KillTimer(hwnd, DRAG_TIMER);
            // Fenster weg, damit die Ziele darunter das Ziehen sehen.
            let _ = ShowWindow(hwnd, SW_HIDE);
            if let Some(mut p) = PENDING_DRAG.with(|d| d.borrow_mut().take()) {
                let mut effect = DROPEFFECT_NONE;
                let hr = DoDragDrop(&p.data, &p.source, DROPEFFECT_COPY, &mut effect);
                let dropped = hr == DRAGDROP_S_DROP && effect != DROPEFFECT_NONE;
                log::info!("Ziehen beendet ({hr:?}, Wirkung {})", effect.0);
                if let Some(f) = p.on_end.take() {
                    f(dropped);
                }
            }
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_TIMER => {
            log::warn!("Ziehen: Klick kam nicht am Hilfsfenster an");
            let _ = KillTimer(hwnd, DRAG_TIMER);
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

#[cfg(test)]
mod tests {
    use super::clip;

    /// #26: Bild (CF_DIB) und Text überstehen Sichern → Überschreiben → Zurücksetzen.
    #[test]
    fn clipboard_snapshot_keeps_image_and_text() {
        const CF_UNICODETEXT: u32 = 13;
        const CF_DIB: u32 = 8;
        // 1×1-Bild, 32 Bit: BITMAPINFOHEADER (40 Bytes) + ein Pixel.
        let mut dib = Vec::new();
        for v in [40i32, 1, 1] {
            dib.extend_from_slice(&v.to_le_bytes());
        }
        dib.extend_from_slice(&1u16.to_le_bytes());
        dib.extend_from_slice(&32u16.to_le_bytes());
        dib.extend_from_slice(&[0u8; 24]);
        dib.extend_from_slice(&[1, 2, 3, 255]);
        let text: Vec<u8> = "Grüße\0"
            .encode_utf16()
            .flat_map(|u| u.to_le_bytes())
            .collect();
        let original =
            clip::Snapshot::from_formats(vec![(CF_DIB, dib.clone()), (CF_UNICODETEXT, text)]);
        if !clip::restore(&original) {
            eprintln!("Zwischenablage nicht verfügbar, Test übersprungen");
            return;
        }
        let saved = clip::snapshot().expect("lesbar");
        assert!(saved.get(CF_DIB).is_some_and(|d| d.starts_with(&dib)));

        let seq = clip::sequence();
        arboard::Clipboard::new()
            .unwrap()
            .set_text("https://example.com")
            .unwrap();
        assert_ne!(clip::sequence(), seq);

        assert!(clip::restore(&saved));
        let back = clip::snapshot().unwrap();
        assert!(back.get(CF_DIB).is_some_and(|d| d.starts_with(&dib)));
        assert_eq!(
            arboard::Clipboard::new().unwrap().get_text().unwrap(),
            "Grüße"
        );
    }
}
