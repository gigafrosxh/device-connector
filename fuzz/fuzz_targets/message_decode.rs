//! Beliebige Bytes als Nachricht dekodieren: Fehler ja, Panik nie. Was
//! dekodiert, muss sich stabil neu kodieren lassen.
#![no_main]
use dc_core::Message;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(m) = Message::decode(data) {
        let once = m.encode();
        let back = Message::decode(&once).expect("eigene Kodierung muss dekodieren");
        assert_eq!(back.encode(), once);
    }
});
