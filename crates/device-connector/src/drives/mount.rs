//! Den WebDAV-Server als Laufwerk ins System einbinden: macOS über NetFS
//! (wie „Mit Server verbinden“ im Finder, erscheint unter /Volumes und in
//! der Seitenleiste), Windows über `net use` (WebClient-Dienst, eigener
//! Laufwerksbuchstabe im Explorer).

use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Mounted {
    /// Ordner bzw. Laufwerkswurzel (`Z:\`), zum Öffnen.
    pub point: PathBuf,
    /// Windows: Laufwerksbuchstabe mit Doppelpunkt; macOS: Einhängepunkt.
    pub id: String,
    /// Windows: Schlüssel für den Anzeigenamen in der Registry.
    #[allow(dead_code)]
    pub reg_key: Option<String>,
}

impl Mounted {
    pub fn describe(&self) -> String {
        if cfg!(windows) {
            format!("als Laufwerk {} im Explorer", self.id)
        } else {
            format!("im Finder unter {}", self.point.display())
        }
    }
}

/// Programm ohne Konsolenfenster starten; Fehlertext bei Misserfolg.
#[allow(dead_code)]
fn run(program: &str, args: &[&str]) -> Result<String, String> {
    let mut cmd = std::process::Command::new(program);
    cmd.args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let out = cmd.output().map_err(|e| format!("{program}: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    if out.status.success() {
        Ok(stdout)
    } else {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        Err(if err.is_empty() {
            stdout.trim().to_string()
        } else {
            err
        })
    }
}

// ---------------------------------------------------------------- macOS

#[cfg(target_os = "macos")]
mod imp {
    use super::*;
    use core_foundation::array::{CFArray, CFArrayRef};
    use core_foundation::base::{kCFAllocatorDefault, CFType, TCFType};
    use core_foundation::boolean::CFBoolean;
    use core_foundation::dictionary::{CFMutableDictionary, CFMutableDictionaryRef};
    use core_foundation::string::{CFString, CFStringRef};
    use core_foundation::url::{CFURLCreateWithString, CFURLRef, CFURL};

    #[link(name = "NetFS", kind = "framework")]
    extern "C" {
        fn NetFSMountURLSync(
            url: CFURLRef,
            mountpath: CFURLRef,
            user: CFStringRef,
            passwd: CFStringRef,
            open_options: CFMutableDictionaryRef,
            mount_options: CFMutableDictionaryRef,
            mountpoints: *mut CFArrayRef,
        ) -> i32;
    }

    /// Einhängungen einer früheren Sitzung (Absturz) mit demselben Port lösen.
    pub fn cleanup_stale(port: u16) {
        let Ok(out) = run("/sbin/mount", &[]) else {
            return;
        };
        let prefix = format!("http://127.0.0.1:{port}/");
        for line in out.lines().filter(|l| l.starts_with(&prefix)) {
            if let Some(rest) = line.split_once(" on ").map(|x| x.1) {
                if let Some((point, _)) = rest.rsplit_once(" (") {
                    log::info!("Alte Einbindung lösen: {point}");
                    force_unmount(point);
                }
            }
        }
    }

    pub fn mount(url: &str, _label: &str) -> Result<Mounted, String> {
        let s = CFString::new(url);
        let raw = unsafe {
            CFURLCreateWithString(
                kCFAllocatorDefault,
                s.as_concrete_TypeRef(),
                std::ptr::null(),
            )
        };
        if raw.is_null() {
            return Err("ungültige Adresse".into());
        }
        let cfurl = unsafe { CFURL::wrap_under_create_rule(raw) };
        let mut open: CFMutableDictionary<CFString, CFType> = CFMutableDictionary::new();
        open.set(CFString::new("UIOption"), CFString::new("NoUI").as_CFType());
        let mut opts: CFMutableDictionary<CFString, CFType> = CFMutableDictionary::new();
        // Ohne Verbindung Fehler melden statt Finder hängen zu lassen.
        opts.set(
            CFString::new("SoftMount"),
            CFBoolean::true_value().as_CFType(),
        );
        let mut points: CFArrayRef = std::ptr::null();
        let rc = unsafe {
            NetFSMountURLSync(
                cfurl.as_concrete_TypeRef(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                open.as_concrete_TypeRef(),
                opts.as_concrete_TypeRef(),
                &mut points,
            )
        };
        if rc != 0 {
            return Err(format!("Einbinden fehlgeschlagen (NetFS {rc})"));
        }
        if points.is_null() {
            return Err("Einbinden: kein Einhängepunkt".into());
        }
        let arr: CFArray<CFString> = unsafe { CFArray::wrap_under_create_rule(points) };
        let point = arr
            .iter()
            .next()
            .map(|p| p.to_string())
            .ok_or("Einbinden: kein Einhängepunkt")?;
        Ok(Mounted {
            point: PathBuf::from(&point),
            id: point,
            reg_key: None,
        })
    }

    fn force_unmount(point: &str) {
        if run("/usr/sbin/diskutil", &["unmount", "force", point]).is_err() {
            if let Err(e) = run("/sbin/umount", &["-f", point]) {
                log::warn!("{point} lösen: {e}");
            }
        }
    }

    pub fn unmount(m: &Mounted) {
        if run("/usr/sbin/diskutil", &["unmount", &m.id]).is_err() {
            force_unmount(&m.id);
        }
    }

    pub fn large_files_blocked() -> bool {
        false
    }

    pub fn allow_large_files() -> Result<(), String> {
        Ok(())
    }
}

// ---------------------------------------------------------------- Windows

#[cfg(windows)]
mod imp {
    use super::*;

    const DEFAULT_LIMIT: u64 = 50_000_000;
    const PARAMS: &str = r"HKLM\SYSTEM\CurrentControlSet\Services\WebClient\Parameters";

    fn free_letter() -> Option<char> {
        let used = unsafe { windows::Win32::Storage::FileSystem::GetLogicalDrives() };
        (b'D'..=b'Z')
            .rev()
            .find(|l| used & (1 << (l - b'A')) == 0)
            .map(|l| l as char)
    }

    /// Verbindungen einer früheren Sitzung (Absturz) mit demselben Port lösen.
    pub fn cleanup_stale(port: u16) {
        let Ok(out) = run("net", &["use"]) else {
            return;
        };
        let needle = format!(r"\\127.0.0.1@{port}\").to_lowercase();
        for line in out.lines() {
            if !line.to_lowercase().contains(&needle) {
                continue;
            }
            if let Some(drive) = line
                .split_whitespace()
                .find(|t| t.len() == 2 && t.ends_with(':'))
            {
                log::info!("Alte Einbindung lösen: {drive}");
                let _ = run("net", &["use", drive, "/delete", "/y"]);
            }
        }
    }

    pub fn mount(url: &str, label: &str) -> Result<Mounted, String> {
        // http://127.0.0.1:PORT/TOKEN/NAME/ → \\127.0.0.1@PORT\TOKEN\NAME
        let rest = url
            .strip_prefix("http://127.0.0.1:")
            .ok_or("ungültige Adresse")?;
        let (port, path) = rest.split_once('/').ok_or("ungültige Adresse")?;
        let token = path.split('/').next().unwrap_or_default();
        let unc = format!(r"\\127.0.0.1@{port}\{token}\{label}");
        let letter = free_letter().ok_or("kein Laufwerksbuchstabe frei")?;
        let drive = format!("{letter}:");
        let mut last = String::new();
        // Der WebClient-Dienst startet beim ersten Zugriff; dann evtl. erneut versuchen.
        for attempt in 0..3 {
            match run("net", &["use", &drive, &unc, "/persistent:no"]) {
                Ok(_) => {
                    // Anzeigename im Explorer statt „TOKEN (\\127.0.0.1@PORT)“.
                    let key = format!(
                        r"HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\MountPoints2\##127.0.0.1@{port}#{token}#{label}"
                    );
                    let _ = run(
                        "reg",
                        &[
                            "add",
                            &key,
                            "/v",
                            "_LabelFromReg",
                            "/t",
                            "REG_SZ",
                            "/d",
                            label,
                            "/f",
                        ],
                    );
                    return Ok(Mounted {
                        point: PathBuf::from(format!("{drive}\\")),
                        id: drive,
                        reg_key: Some(key),
                    });
                }
                Err(e) => {
                    log::info!("net use (Versuch {}): {e}", attempt + 1);
                    last = e;
                    std::thread::sleep(std::time::Duration::from_secs(2));
                }
            }
        }
        Err(format!(
            "Einbinden fehlgeschlagen: {last} (läuft der Windows-Dienst „WebClient“?)"
        ))
    }

    pub fn unmount(m: &Mounted) {
        if let Err(e) = run("net", &["use", &m.id, "/delete", "/y"]) {
            log::warn!("{} lösen: {e}", m.id);
        }
        if let Some(k) = &m.reg_key {
            let _ = run("reg", &["delete", k, "/f"]);
        }
    }

    /// Windows lädt über WebDAV standardmäßig höchstens 50 MB pro Datei.
    pub fn large_files_blocked() -> bool {
        let Ok(out) = run("reg", &["query", PARAMS, "/v", "FileSizeLimitInBytes"]) else {
            return true;
        };
        out.split_whitespace()
            .find_map(|t| t.strip_prefix("0x"))
            .and_then(|h| u64::from_str_radix(h, 16).ok())
            .is_none_or(|v| v <= DEFAULT_LIMIT)
    }

    /// Grenze auf 4 GB anheben (braucht Administratorrechte, fragt per UAC)
    /// und den WebClient-Dienst neu starten.
    pub fn allow_large_files() -> Result<(), String> {
        let inner = format!(
            "/c reg add {PARAMS} /v FileSizeLimitInBytes /t REG_DWORD /d 4294967295 /f & net stop WebClient & net start WebClient"
        );
        let ps = format!(
            "Start-Process -FilePath cmd.exe -Verb RunAs -Wait -WindowStyle Hidden -ArgumentList '{}'",
            inner.replace('\'', "''")
        );
        run(
            "powershell",
            &["-NoProfile", "-NonInteractive", "-Command", &ps],
        )?;
        if large_files_blocked() {
            Err("nicht geändert (Nachfrage abgelehnt?)".into())
        } else {
            Ok(())
        }
    }
}

// ---------------------------------------------------------------- sonst

#[cfg(not(any(windows, target_os = "macos")))]
mod imp {
    use super::*;

    pub fn cleanup_stale(_port: u16) {}

    pub fn mount(_url: &str, _label: &str) -> Result<Mounted, String> {
        Err("Einbinden wird auf diesem System nicht unterstützt".into())
    }

    pub fn unmount(_m: &Mounted) {}

    pub fn large_files_blocked() -> bool {
        false
    }

    pub fn allow_large_files() -> Result<(), String> {
        Ok(())
    }
}

pub use imp::*;
