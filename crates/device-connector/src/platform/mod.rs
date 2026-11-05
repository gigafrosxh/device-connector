//! Betriebssystem-Schicht. Jede Plattform bietet dieselben Funktionen:
//!
//! * `start_input(sink)` – globale Maus-/Tastatur-Ereignisse abgreifen;
//!   bei `set_capturing(true)` werden sie abgefangen statt ans System gegeben.
//! * `Injector` – Eingaben vom anderen Rechner abspielen.
//! * `screen_bounds()` – Fläche aller Monitore.
//! * `selected_files()` – im Explorer/Finder markierte Dateien (Phase 2).
//! * `handoff_target()` – Inhalt des aktiven Fensters (Phase 3).
//! * `ask_text` / `show_info` – einfache Dialoge ohne eigenes Fenster-Toolkit.

use std::path::PathBuf;

/// Kennzeichnet von uns erzeugte Ereignisse, damit unser eigener Hook sie
/// nicht wieder einsammelt.
#[allow(dead_code)]
pub const INJECT_MAGIC: usize = 0x00DC_C0DE;

/// Empfänger für lokale Eingaben (wird aus dem Hook-Thread aufgerufen).
pub type Sink = Box<dyn Fn(dc_core::Input) + Send + Sync>;

#[derive(Debug, Clone, PartialEq)]
pub enum HandoffTarget {
    Url(String),
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    File(PathBuf),
}

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::*;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use self::windows::*;

#[cfg(not(any(windows, target_os = "macos")))]
mod other;
#[cfg(not(any(windows, target_os = "macos")))]
pub use other::*;

/// Freier Speicherplatz (Bytes) für den Ordner `p`, falls bekannt.
#[cfg(unix)]
pub fn free_space(p: &std::path::Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(p.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    #[allow(clippy::unnecessary_cast)]
    Some(st.f_bavail as u64 * st.f_frsize as u64)
}

/// Freier Speicherplatz (Bytes) für den Ordner `p`, falls bekannt.
#[cfg(windows)]
pub fn free_space(p: &std::path::Path) -> Option<u64> {
    use ::windows::core::HSTRING;
    use ::windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let mut avail = 0u64;
    unsafe { GetDiskFreeSpaceExW(&HSTRING::from(p.as_os_str()), Some(&mut avail), None, None) }
        .ok()?;
    Some(avail)
}

#[cfg(not(any(unix, windows)))]
pub fn free_space(_p: &std::path::Path) -> Option<u64> {
    None
}

/// Gemeinsame Hilfe: Prozess ohne Konsolenfenster starten und stdout lesen.
#[allow(dead_code)]
pub(crate) fn run_capture(program: &str, args: &[&str]) -> Option<String> {
    let mut cmd = std::process::Command::new(program);
    cmd.args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let out = cmd.output().ok()?;
    if !out.status.success() {
        log::debug!(
            "{program} fehlgeschlagen: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).to_string())
}

/// `file:///Users/a/B%C3%BCro.txt` → `/Users/a/Büro.txt`
#[allow(dead_code)]
pub(crate) fn file_url_to_path(url: &str) -> Option<PathBuf> {
    let rest = url.strip_prefix("file://")?;
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    let s = String::from_utf8(out).ok()?;
    let s = s.trim_end_matches('/');
    Some(PathBuf::from(s))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn free_space_is_known_for_temp() {
        assert!(free_space(&std::env::temp_dir()).is_some_and(|b| b > 0));
    }

    #[test]
    fn decodes_file_urls() {
        assert_eq!(
            file_url_to_path("file:///Users/a/B%C3%BCro%20neu.txt"),
            Some(PathBuf::from("/Users/a/Büro neu.txt"))
        );
        assert_eq!(file_url_to_path("https://x"), None);
    }
}
