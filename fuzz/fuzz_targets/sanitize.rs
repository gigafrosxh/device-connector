//! Pfade vom anderen Rechner (#13): entweder abgelehnt oder ein relativer
//! Pfad aus lauter harmlosen Teilen.
#![no_main]
use std::path::Component;

use dc_core::files::sanitize;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let rel = String::from_utf8_lossy(data);
    if let Some(p) = sanitize(&rel) {
        assert!(p.components().count() > 0);
        assert!(
            p.components().all(|c| matches!(c, Component::Normal(_))),
            "{rel:?} → {p:?}"
        );
        for c in p.components() {
            let s = c.as_os_str().to_string_lossy();
            assert!(
                !s.contains(['\\', ':', '\0', '<', '>', '|', '?', '*', '"']),
                "{s:?}"
            );
            assert!(!s.ends_with(['.', ' ']), "{s:?}");
        }
    }
});
