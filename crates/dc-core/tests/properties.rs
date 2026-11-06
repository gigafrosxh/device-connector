//! Property-Tests (#32): Parser für Bytes von außen dürfen nie abstürzen,
//! nie unbegrenzt Speicher halten und nie aus dem Empfangsordner ausbrechen.
//! Läuft mit festen Fallzahlen in CI; längere Läufe mit
//! `PROPTEST_CASES=100000 cargo test -p dc-core --test properties` und die
//! Fuzz-Ziele in `fuzz/` (siehe README dort).

use std::path::{Component, Path, PathBuf};

use dc_core::files::{sanitize, ReceiveEvent, Receiver};
use dc_core::link::{write_frame, FrameReader, Outbox, HIGH_LIMIT};
use dc_core::{Button, Message};
use proptest::prelude::*;

/// Liest in zufälligen Häppchen (wie ein echtes Netz).
struct Chunked {
    data: Vec<u8>,
    pos: usize,
    sizes: Vec<usize>,
    i: usize,
}

impl std::io::Read for Chunked {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.pos >= self.data.len() {
            return Ok(0);
        }
        let want = self.sizes[self.i % self.sizes.len()].max(1);
        self.i += 1;
        let n = want.min(buf.len()).min(self.data.len() - self.pos);
        buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    #[test]
    fn frame_reader_never_panics_and_stays_bounded(
        data in proptest::collection::vec(any::<u8>(), 0..20_000),
        sizes in proptest::collection::vec(1usize..5000, 1..8),
    ) {
        let mut r = FrameReader::new();
        let mut src = Chunked { data, pos: 0, sizes, i: 0 };
        loop {
            match r.read(&mut src) {
                Ok(Some((ty, p))) => {
                    prop_assert!(ty <= 3);
                    prop_assert!(p.len() <= 65535);
                }
                Ok(None) => {}
                Err(_) => break,
            }
            prop_assert!(r.buffered() <= 4 + 65535 + 16384);
        }
    }

    #[test]
    fn frames_survive_garbage_prefix(
        garbage in proptest::collection::vec(any::<u8>().prop_filter("kein Magic", |b| *b != 0xDC), 0..2000),
        ty in 0u8..=3,
        payload in proptest::collection::vec(any::<u8>(), 0..3000),
        sizes in proptest::collection::vec(1usize..700, 1..5),
    ) {
        let mut data = garbage;
        write_frame(&mut data, ty, &payload).unwrap();
        let mut r = FrameReader::new();
        let mut src = Chunked { data, pos: 0, sizes, i: 0 };
        let got = loop {
            if let Some(f) = r.read(&mut src).unwrap() {
                break f;
            }
        };
        prop_assert_eq!(got, (ty, payload));
    }

    #[test]
    fn message_decode_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..4096)) {
        if let Ok(m) = Message::decode(&bytes) {
            // Was sich dekodieren lässt, lässt sich auch wieder kodieren.
            // Byteweise vergleichen: NaN in Gleitkommafeldern ist nie == NaN.
            let again = Message::decode(&m.encode()).unwrap();
            prop_assert_eq!(again.encode(), m.encode());
        }
    }

    #[test]
    fn message_roundtrip(
        dx in any::<i32>(), dy in any::<i32>(), hid in any::<u16>(), down in any::<bool>(),
        text in ".{0,200}", id in any::<u32>(), size in any::<u64>(),
        data in proptest::collection::vec(any::<u8>(), 0..512),
    ) {
        let msgs = vec![
            Message::MouseMove { dx, dy },
            Message::Wheel { dx, dy },
            Message::Key { hid, down },
            Message::MouseButton { button: Button::Left, down },
            Message::Clipboard { text: text.clone() },
            Message::FileStart { transfer: id, id, path: text.clone(), size },
            Message::FileChunk { transfer: id, id, data },
            Message::Dir { transfer: id, path: text },
            Message::BatchEnd { transfer: id, files: id, open_first: down },
        ];
        for m in msgs {
            prop_assert_eq!(Message::decode(&m.encode()).unwrap(), m);
        }
    }

    #[test]
    fn sanitized_paths_stay_relative_and_safe(rel in "\\PC{0,80}") {
        if let Some(p) = sanitize(&rel) {
            prop_assert!(!p.as_os_str().is_empty());
            for c in p.components() {
                match c {
                    Component::Normal(n) => {
                        let n = n.to_string_lossy();
                        prop_assert!(!n.is_empty());
                        prop_assert!(!n.ends_with(' ') && !n.ends_with('.'));
                        prop_assert!(!n.contains(['\\', ':', '<', '>', '"', '|', '?', '*']));
                        prop_assert!(!n.chars().any(|c| c.is_control()));
                        let stem = n.split('.').next().unwrap().trim_end().to_uppercase();
                        prop_assert!(!["CON", "PRN", "AUX", "NUL"].contains(&stem.as_str()));
                    }
                    other => prop_assert!(false, "unerlaubte Komponente {:?}", other),
                }
            }
            let base = Path::new("/empfang");
            prop_assert!(base.join(&p).starts_with(base));
        }
    }

    #[test]
    fn sanitize_handles_path_like_inputs(
        parts in proptest::collection::vec(
            prop_oneof![
                Just("..".to_string()), Just(".".to_string()), Just("".to_string()),
                Just("CON".to_string()), Just("com7.txt".to_string()), Just("a. ".to_string()),
                "[a-zA-Z0-9äöü _.-]{1,12}",
            ],
            1..6,
        )
    ) {
        let rel = parts.join("/");
        if let Some(p) = sanitize(&rel) {
            prop_assert!(p.components().all(|c| matches!(c, Component::Normal(_))));
        }
    }

    #[test]
    fn outbox_stays_bounded_and_keeps_releases(
        ops in proptest::collection::vec((0u8..4, any::<u16>(), any::<i32>()), 0..5000)
    ) {
        let o = Outbox::new();
        let mut ups = 0usize;
        for (kind, h, v) in &ops {
            match kind {
                0 => o.push(Message::MouseMove { dx: *v, dy: -*v }),
                1 => o.push(Message::Key { hid: *h, down: true }),
                2 => { o.push(Message::Key { hid: *h, down: false }); ups += 1; }
                _ => o.push(Message::Wheel { dx: 0, dy: *v }),
            }
        }
        let (len, peak, _) = o.stats();
        prop_assert!(len <= 4 * HIGH_LIMIT && peak <= 4 * HIGH_LIMIT);
        let mut got_ups = 0;
        while let Some(m) = o.pop(std::time::Duration::ZERO) {
            if matches!(m, Message::Key { down: false, .. }) {
                got_ups += 1;
            }
        }
        // Loslassen geht nur verloren, wenn selbst die harte Grenze erreicht ist.
        if ups < HIGH_LIMIT {
            prop_assert_eq!(got_ups, ups);
        }
    }
}

// Empfänger mit zufälligen Nachrichtenfolgen: bleibt im Ordner, keine Reste.
#[derive(Debug, Clone)]
enum Op {
    Start(u32, u32, String, u64),
    Chunk(u32, u32, usize),
    End(u32, u32),
    Dir(u32, String),
    Batch(u32, u32),
}

fn op() -> impl Strategy<Value = Op> {
    let t = 0u32..3;
    let id = 0u32..4;
    let name = prop_oneof![
        "[a-c]{1,3}(/[a-c]{1,3}){0,2}",
        Just("../x".to_string()),
        Just("CON".to_string()),
        Just("a/./b".to_string()),
    ];
    prop_oneof![
        (t.clone(), id.clone(), name.clone(), 0u64..64)
            .prop_map(|(t, i, n, s)| Op::Start(t, i, n, s)),
        (t.clone(), id.clone(), 0usize..40).prop_map(|(t, i, n)| Op::Chunk(t, i, n)),
        (t.clone(), id).prop_map(|(t, i)| Op::End(t, i)),
        (t.clone(), name).prop_map(|(t, n)| Op::Dir(t, n)),
        (t, 0u32..4).prop_map(|(t, n)| Op::Batch(t, n)),
    ]
}

fn all_files(d: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(d) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(all_files(&p));
            }
            out.push(p);
        }
    }
    out
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]

    #[test]
    fn receiver_random_sequences(ops in proptest::collection::vec(op(), 0..60)) {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let root = std::env::temp_dir().join(format!(
            "dc-prop-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("empfang")).unwrap();
        // Kanonisch vergleichen (macOS: /var → /private/var, Windows: \\?\).
        let root = std::fs::canonicalize(&root).unwrap();
        let base = root.join("empfang");
        let mut rx = Receiver::new(base.clone());
        for o in &ops {
            let m = match o.clone() {
                Op::Start(t, i, n, s) => Message::FileStart { transfer: t, id: i, path: n, size: s },
                Op::Chunk(t, i, n) => Message::FileChunk { transfer: t, id: i, data: vec![7; n] },
                Op::End(t, i) => Message::FileEnd { transfer: t, id: i },
                Op::Dir(t, n) => Message::Dir { transfer: t, path: n },
                Op::Batch(t, n) => Message::BatchEnd { transfer: t, files: n, open_first: false },
            };
            if let Some(ReceiveEvent::Done { files, .. }) = rx.handle(&m) {
                for f in files {
                    prop_assert!(f.starts_with(&base));
                    prop_assert!(f.is_file());
                }
            }
        }
        rx.abort();
        for f in all_files(&root) {
            prop_assert!(f.starts_with(&base), "außerhalb: {}", f.display());
            prop_assert!(!f.to_string_lossy().ends_with(".part"), "Rest: {}", f.display());
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}
