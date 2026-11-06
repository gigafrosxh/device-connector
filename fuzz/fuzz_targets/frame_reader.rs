//! Beliebige Bytes in den Frame-Parser: darf nie paniken und nie mehr als
//! einen Frame plus Kopf puffern (#18).
#![no_main]
use std::io::Read;

use dc_core::link::FrameReader;
use libfuzzer_sys::fuzz_target;

/// Liefert die Eingabe in Häppchen, deren Größe aus dem ersten Byte kommt.
struct Chunked<'a> {
    data: &'a [u8],
    step: usize,
}

impl Read for Chunked<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.step.min(buf.len()).min(self.data.len());
        buf[..n].copy_from_slice(&self.data[..n]);
        self.data = &self.data[n..];
        Ok(n)
    }
}

fuzz_target!(|data: &[u8]| {
    let Some((&first, rest)) = data.split_first() else {
        return;
    };
    let mut src = Chunked {
        data: rest,
        step: first as usize % 64 + 1,
    };
    let mut r = FrameReader::new();
    for _ in 0..rest.len() + 2 {
        match r.read(&mut src) {
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => break,
        }
    }
    assert!(r.buffered() <= rest.len());
});
