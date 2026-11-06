//! Diagnose für den Mauswechsel (#30), standardmäßig aus.
//!
//! Einschalten: Umgebungsvariable `DC_TRACE=1` oder im Menü
//! „Diagnose: Mauswechsel aufzeichnen“. Dann schreibt die App in
//! `handoff-trace.log` (neben `device-connector.log`) eine Zeile je Schritt:
//!
//! ```text
//! t=1234.567ms capture_start origin=(1919,540) cursor=(960,540)
//! t=1234.890ms send Enter y=0.500
//! t=1302.120ms recv Leave y=0.312
//! t=1302.300ms capture_stop target=(1917,336) cursor=(1917,336)
//! ```
//!
//! `t` ist eine monotone Zeit seit Start dieses Prozesses; zusammen mit der
//! Aufzeichnung des anderen Rechners lässt sich so unterscheiden, ob das
//! Abfangen, die Übertragung oder das Abspielen hakt. Tastendrücke werden
//! nie aufgezeichnet, nur Positionen, Modi und Nachrichtenarten.
//!
//! Ausgeschaltet kostet ein Aufruf nur das Lesen eines `AtomicBool`.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

static ON: AtomicBool = AtomicBool::new(false);
static FILE: Mutex<Option<File>> = Mutex::new(None);
static START: OnceLock<Instant> = OnceLock::new();

pub fn enabled() -> bool {
    ON.load(Ordering::Relaxed)
}

/// Ein- oder ausschalten; die Datei liegt in `dir`.
pub fn set_enabled(on: bool, dir: &Path) {
    START.get_or_init(Instant::now);
    let mut f = FILE.lock().unwrap_or_else(|e| e.into_inner());
    if on && f.is_none() {
        let path = dir.join("handoff-trace.log");
        // Nicht endlos wachsen lassen.
        if std::fs::metadata(&path).is_ok_and(|m| m.len() > 10 * 1024 * 1024) {
            let _ = std::fs::rename(&path, dir.join("handoff-trace.old.log"));
        }
        match OpenOptions::new().create(true).append(true).open(&path) {
            Ok(file) => {
                *f = Some(file);
                log::info!("Diagnose-Aufzeichnung an: {}", path.display());
            }
            Err(e) => {
                log::warn!("Diagnose-Aufzeichnung nicht möglich: {e}");
                return;
            }
        }
    } else if !on {
        *f = None;
    }
    ON.store(on, Ordering::Relaxed);
}

#[doc(hidden)]
pub fn write(args: std::fmt::Arguments) {
    let t = START.get_or_init(Instant::now).elapsed().as_secs_f64() * 1000.0;
    let mut f = FILE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(file) = f.as_mut() {
        let _ = writeln!(file, "t={t:.3}ms {args}");
    }
}

/// `trace!("capture_start origin=({x:.0},{y:.0})")` – nur aktiv, wenn eingeschaltet.
#[macro_export]
macro_rules! trace {
    ($($arg:tt)*) => {
        if $crate::trace::enabled() {
            $crate::trace::write(format_args!($($arg)*));
        }
    };
}

/// Position als `(x,y)` oder `?`.
pub fn pos(p: Option<(f64, f64)>) -> String {
    match p {
        Some((x, y)) => format!("({x:.0},{y:.0})"),
        None => "?".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_only_when_enabled() {
        let dir = std::env::temp_dir().join(format!("dc-trace-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        crate::trace!("nicht da");
        set_enabled(true, &dir);
        crate::trace!("capture_start origin={}", pos(Some((1.0, 2.0))));
        set_enabled(false, &dir);
        crate::trace!("auch nicht da");
        let text = std::fs::read_to_string(dir.join("handoff-trace.log")).unwrap();
        assert!(text.contains("capture_start origin=(1,2)"), "{text}");
        assert!(!text.contains("nicht da"));
        assert_eq!(text.lines().count(), 1);
    }
}
