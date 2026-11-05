//! Zufällige Folgen von Datei-Nachrichten an den Empfänger: nichts landet
//! außerhalb des Empfangsordners, nach `abort` bleiben keine `.part`-Reste.
#![no_main]
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use dc_core::files::{ReceiveEvent, Receiver};
use dc_core::Message;
use libfuzzer_sys::fuzz_target;

fn all_files(d: &Path, out: &mut Vec<PathBuf>) {
    if let Ok(rd) = std::fs::read_dir(d) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                all_files(&p, out);
            }
            out.push(p);
        }
    }
}

fuzz_target!(|data: &[u8]| {
    static N: AtomicU64 = AtomicU64::new(0);
    let root = std::env::temp_dir().join(format!(
        "dc-fuzz-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let base = root.join("empfang");
    std::fs::create_dir_all(&base).unwrap();
    let base = std::fs::canonicalize(base).unwrap();
    let mut rx = Receiver::new(base.clone());

    // Eingabe = Folge von [Länge u8][postcard-Nachricht].
    let mut rest = data;
    while let Some((&len, tail)) = rest.split_first() {
        let n = (len as usize).min(tail.len());
        let (msg, tail) = tail.split_at(n);
        rest = tail;
        let Ok(m) = Message::decode(msg) else {
            continue;
        };
        if let Some(ReceiveEvent::Done { files, .. }) = rx.handle(&m) {
            for f in files {
                assert!(f.starts_with(&base), "{}", f.display());
            }
        }
    }
    rx.abort();
    let mut files = Vec::new();
    all_files(&root, &mut files);
    for f in &files {
        assert!(f.starts_with(&base), "außerhalb: {}", f.display());
        assert!(
            !f.to_string_lossy().ends_with(".part"),
            "Rest: {}",
            f.display()
        );
    }
    let _ = std::fs::remove_dir_all(&root);
});
