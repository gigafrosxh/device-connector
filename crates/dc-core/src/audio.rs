//! Ton des anderen Rechners: Kodierung, Umrechnung und Wiedergabepuffer.
//!
//! Der sendende Rechner nimmt seinen Systemton auf, rechnet ihn auf eine
//! feste Rate um und schickt alle [`FRAME_MS`] einen [`AudioFrame`]. Kodiert
//! wird mit IMA-ADPCM (4 Bit pro Abtastwert): ohne C-Bibliothek, sehr wenig
//! Rechenzeit, und 48 kHz Stereo passen in rund 390 kbit/s. Jeder Block trägt
//! den Decoder-Zustand im Kopf, ein verlorener Block stört die folgenden nicht.
//!
//! Der empfangende Rechner dekodiert, passt Kanäle und Rate an sein
//! Ausgabegerät an und spielt aus einem [`PlayBuffer`], der kleine
//! Schwankungen der Verbindung ausgleicht und bei zu viel Verzug aufholt.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

/// Länge eines Blocks.
pub const FRAME_MS: u32 = 20;
/// Rate über Netzwerk/Kabel.
pub const RATE_FAST: u32 = 48_000;
/// Rate über Bluetooth (halbe Datenmenge).
pub const RATE_SLOW: u32 = 24_000;
pub const MAX_CHANNELS: u8 = 2;
const MIN_RATE: u32 = 8_000;
const MAX_RATE: u32 = 96_000;
/// Höchstens 100 ms pro Block (Schutz vor unsinnigen Größen).
const MAX_SAMPLES: u16 = 9_600;
/// Kopf pro Kanal: Vorhersage (i16) und Schrittweiten-Index (u8).
const CHANNEL_HEADER: usize = 3;

/// Ein Stück Ton, IMA-ADPCM-kodiert, Kanäle nacheinander (planar).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct AudioFrame {
    /// Laufende Nummer (Lücken = verlorene Blöcke).
    pub seq: u32,
    pub rate: u32,
    pub channels: u8,
    /// Abtastwerte pro Kanal.
    pub samples: u16,
    pub data: Vec<u8>,
}

fn encoded_len(channels: usize, samples: usize) -> usize {
    channels * (CHANNEL_HEADER + samples.div_ceil(2))
}

impl AudioFrame {
    /// Passt der Block zu seinen eigenen Angaben?
    pub fn is_valid(&self) -> bool {
        (1..=MAX_CHANNELS).contains(&self.channels)
            && (MIN_RATE..=MAX_RATE).contains(&self.rate)
            && self.samples > 0
            && self.samples <= MAX_SAMPLES
            && self.data.len() == encoded_len(self.channels as usize, self.samples as usize)
    }
}

// ---------------------------------------------------------------- IMA-ADPCM

const STEPS: [i32; 89] = [
    7, 8, 9, 10, 11, 12, 13, 14, 16, 17, 19, 21, 23, 25, 28, 31, 34, 37, 41, 45, 50, 55, 60, 66,
    73, 80, 88, 97, 107, 118, 130, 143, 157, 173, 190, 209, 230, 253, 279, 307, 337, 371, 408, 449,
    494, 544, 598, 658, 724, 796, 876, 963, 1060, 1166, 1282, 1411, 1552, 1707, 1878, 2066, 2272,
    2499, 2749, 3024, 3327, 3660, 4026, 4428, 4871, 5358, 5894, 6484, 7132, 7845, 8630, 9493,
    10442, 11487, 12635, 13899, 15289, 16818, 18500, 20350, 22385, 24623, 27086, 29794, 32767,
];
const INDEX_ADJUST: [i32; 8] = [-1, -1, -1, -1, 2, 4, 6, 8];

#[derive(Clone, Copy, Default, Debug)]
struct AdpcmState {
    predictor: i32,
    index: i32,
}

impl AdpcmState {
    fn decode(&mut self, nibble: u8) -> i16 {
        let step = STEPS[self.index as usize];
        let mut diff = step >> 3;
        if nibble & 4 != 0 {
            diff += step;
        }
        if nibble & 2 != 0 {
            diff += step >> 1;
        }
        if nibble & 1 != 0 {
            diff += step >> 2;
        }
        if nibble & 8 != 0 {
            self.predictor -= diff;
        } else {
            self.predictor += diff;
        }
        self.predictor = self.predictor.clamp(-32768, 32767);
        self.index = (self.index + INDEX_ADJUST[(nibble & 7) as usize]).clamp(0, 88);
        self.predictor as i16
    }

    fn encode(&mut self, sample: i16) -> u8 {
        let step = STEPS[self.index as usize];
        let mut diff = sample as i32 - self.predictor;
        let mut nibble = 0u8;
        if diff < 0 {
            nibble = 8;
            diff = -diff;
        }
        let mut s = step;
        if diff >= s {
            nibble |= 4;
            diff -= s;
        }
        s >>= 1;
        if diff >= s {
            nibble |= 2;
            diff -= s;
        }
        s >>= 1;
        if diff >= s {
            nibble |= 1;
        }
        // Den Zustand genau so fortschreiben wie der Decoder.
        self.decode(nibble);
        nibble
    }
}

fn to_i16(s: f32) -> i16 {
    (s.clamp(-1.0, 1.0) * 32767.0).round() as i16
}

/// Kodiert fortlaufend Blöcke; der Zustand läuft über Blockgrenzen weiter.
pub struct Encoder {
    rate: u32,
    channels: usize,
    states: Vec<AdpcmState>,
    seq: u32,
}

impl Encoder {
    pub fn new(rate: u32, channels: u8) -> Encoder {
        let channels = channels.clamp(1, MAX_CHANNELS) as usize;
        Encoder {
            rate,
            channels,
            states: vec![AdpcmState::default(); channels],
            seq: 0,
        }
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// `pcm`: verschachtelt (L R L R …), Länge ein Vielfaches der Kanalzahl.
    pub fn encode(&mut self, pcm: &[f32]) -> AudioFrame {
        let ch = self.channels;
        let samples = (pcm.len() / ch).min(MAX_SAMPLES as usize);
        let mut data = Vec::with_capacity(encoded_len(ch, samples));
        for (c, st) in self.states.iter_mut().enumerate() {
            data.extend_from_slice(&(st.predictor as i16).to_le_bytes());
            data.push(st.index as u8);
            let mut pending: Option<u8> = None;
            for i in 0..samples {
                let n = st.encode(to_i16(pcm[i * ch + c]));
                match pending.take() {
                    None => pending = Some(n),
                    Some(lo) => data.push(lo | (n << 4)),
                }
            }
            if let Some(lo) = pending {
                data.push(lo);
            }
        }
        let seq = self.seq;
        self.seq = self.seq.wrapping_add(1);
        AudioFrame {
            seq,
            rate: self.rate,
            channels: ch as u8,
            samples: samples as u16,
            data,
        }
    }
}

/// Dekodiert einen Block zu verschachtelten Abtastwerten in -1..1.
/// `None` bei einem kaputten Block.
pub fn decode(frame: &AudioFrame) -> Option<Vec<f32>> {
    if !frame.is_valid() {
        return None;
    }
    let ch = frame.channels as usize;
    let n = frame.samples as usize;
    let per_channel = CHANNEL_HEADER + n.div_ceil(2);
    let mut out = vec![0f32; n * ch];
    for c in 0..ch {
        let block = &frame.data[c * per_channel..(c + 1) * per_channel];
        let index = block[2] as i32;
        if index > 88 {
            return None;
        }
        let mut st = AdpcmState {
            predictor: i16::from_le_bytes([block[0], block[1]]) as i32,
            index,
        };
        for i in 0..n {
            let byte = block[CHANNEL_HEADER + i / 2];
            let nibble = if i % 2 == 0 { byte & 0x0f } else { byte >> 4 };
            out[i * ch + c] = st.decode(nibble) as f32 / 32768.0;
        }
    }
    Some(out)
}

// ---------------------------------------------------------------- Kanäle und Rate

/// Verschachtelte Abtastwerte von `from` auf `to` Kanäle bringen: Mono wird
/// verteilt, nach Mono wird gemittelt, sonst die ersten Kanäle übernommen.
pub fn map_channels(pcm: &[f32], from: usize, to: usize) -> Vec<f32> {
    if from == to || from == 0 || to == 0 {
        return pcm.to_vec();
    }
    let frames = pcm.len() / from;
    let mut out = Vec::with_capacity(frames * to);
    for f in pcm.chunks_exact(from) {
        if to == 1 {
            out.push(f.iter().sum::<f32>() / from as f32);
        } else if from == 1 {
            out.extend(std::iter::repeat_n(f[0], to));
        } else {
            let n = from.min(to);
            out.extend_from_slice(&f[..n]);
            out.extend(std::iter::repeat_n(0.0, to - n));
        }
    }
    out
}

/// Lineare Umrechnung der Abtastrate, fortlaufend über Blockgrenzen.
pub struct Resampler {
    channels: usize,
    /// Eingangsbilder pro Ausgangsbild.
    step: f64,
    /// Position des nächsten Ausgangsbilds zwischen `prev` (0) und dem
    /// nächsten Eingangsbild (1).
    phase: f64,
    prev: Vec<f32>,
    from: u32,
    to: u32,
}

impl Resampler {
    pub fn new(from: u32, to: u32, channels: usize) -> Resampler {
        Resampler {
            channels: channels.max(1),
            step: from as f64 / to as f64,
            phase: 1.0,
            prev: vec![0.0; channels.max(1)],
            from,
            to,
        }
    }

    pub fn rates(&self) -> (u32, u32) {
        (self.from, self.to)
    }

    pub fn process(&mut self, pcm: &[f32]) -> Vec<f32> {
        let ch = self.channels;
        if self.from == self.to {
            return pcm.to_vec();
        }
        let frames = pcm.len() / ch;
        let mut out = Vec::with_capacity((frames as f64 / self.step) as usize * ch + ch);
        for f in 0..frames {
            let cur = &pcm[f * ch..(f + 1) * ch];
            while self.phase <= 1.0 {
                let t = self.phase as f32;
                out.extend(self.prev.iter().zip(cur).map(|(p, c)| p + (c - p) * t));
                self.phase += self.step;
            }
            self.phase -= 1.0;
            self.prev.copy_from_slice(cur);
        }
        out
    }
}

/// Sammelt aufgenommene Abtastwerte zu festen Blöcken: Kanäle anpassen,
/// Rate umrechnen, alle [`FRAME_MS`] einen kodierten Block ausgeben.
pub struct Packer {
    in_channels: usize,
    resampler: Resampler,
    encoder: Encoder,
    pending: Vec<f32>,
    frame_len: usize,
}

impl Packer {
    pub fn new(in_rate: u32, in_channels: usize, out_rate: u32) -> Packer {
        let out_ch = in_channels.clamp(1, MAX_CHANNELS as usize);
        Packer {
            in_channels: in_channels.max(1),
            resampler: Resampler::new(in_rate, out_rate, out_ch),
            encoder: Encoder::new(out_rate, out_ch as u8),
            pending: Vec::new(),
            frame_len: (out_rate * FRAME_MS / 1000) as usize * out_ch,
        }
    }

    /// Nimmt verschachtelte Abtastwerte des Aufnahmegeräts und ruft `emit`
    /// für jeden fertigen Block. Ganz stille Blöcke werden nicht gesendet
    /// (spart die Verbindung, wenn nichts läuft).
    pub fn push(&mut self, pcm: &[f32], mut emit: impl FnMut(AudioFrame)) {
        let out_ch = self.encoder.channels();
        let mapped = map_channels(pcm, self.in_channels, out_ch);
        let resampled = self.resampler.process(&mapped);
        self.pending.extend_from_slice(&resampled);
        while self.pending.len() >= self.frame_len {
            let block: Vec<f32> = self.pending.drain(..self.frame_len).collect();
            let frame = self.encoder.encode(&block);
            if block.iter().any(|s| s.abs() > 1.0 / 32768.0) {
                emit(frame);
            }
        }
    }
}

// ---------------------------------------------------------------- Wiedergabe

/// Puffer zwischen Verbindung und Ausgabegerät.
///
/// Erst wenn [`PlayBuffer::target`] Bilder da sind, beginnt die Wiedergabe;
/// läuft er leer, wird still gespielt und neu gefüllt. Wächst er über
/// `max` (z. B. weil die Uhren beider Geräte leicht abweichen oder Blöcke
/// gebündelt ankamen), wird der Überschuss verworfen.
pub struct PlayBuffer {
    buf: VecDeque<f32>,
    channels: usize,
    target: usize,
    max: usize,
    priming: bool,
    underruns: u64,
}

impl PlayBuffer {
    /// `target_ms`: Vorlauf vor dem Start, `max_ms`: Obergrenze.
    pub fn new(rate: u32, channels: usize, target_ms: u32, max_ms: u32) -> PlayBuffer {
        let ch = channels.max(1);
        let frames = |ms: u32| (rate as usize * ms as usize / 1000).max(1);
        PlayBuffer {
            buf: VecDeque::new(),
            channels: ch,
            target: frames(target_ms) * ch,
            max: frames(max_ms.max(target_ms * 2)) * ch,
            priming: true,
            underruns: 0,
        }
    }

    pub fn target(&self) -> usize {
        self.target / self.channels
    }

    /// Gepufferte Bilder.
    pub fn len_frames(&self) -> usize {
        self.buf.len() / self.channels
    }

    pub fn underruns(&self) -> u64 {
        self.underruns
    }

    /// Verschachtelte Abtastwerte mit der Kanalzahl des Ausgabegeräts.
    pub fn push(&mut self, pcm: &[f32]) {
        let whole = pcm.len() - pcm.len() % self.channels;
        self.buf.extend(&pcm[..whole]);
        if self.buf.len() > self.max {
            let drop = self.buf.len() - self.target;
            self.buf.drain(..drop);
        }
    }

    /// Füllt `out` (verschachtelt); fehlt etwas, wird es still.
    pub fn pull(&mut self, out: &mut [f32]) {
        if self.priming {
            if self.buf.len() < self.target {
                out.fill(0.0);
                return;
            }
            self.priming = false;
        }
        let n = out.len().min(self.buf.len());
        for (o, s) in out[..n].iter_mut().zip(self.buf.drain(..n)) {
            *o = s;
        }
        if n < out.len() {
            out[n..].fill(0.0);
            self.priming = true;
            self.underruns += 1;
        }
    }

    pub fn clear(&mut self) {
        self.buf.clear();
        self.priming = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(rate: u32, freq: f32, frames: usize, ch: usize) -> Vec<f32> {
        (0..frames)
            .flat_map(|i| {
                let v = (i as f32 / rate as f32 * freq * std::f32::consts::TAU).sin() * 0.5;
                std::iter::repeat_n(v, ch)
            })
            .collect()
    }

    #[test]
    fn adpcm_roundtrip_is_close() {
        let all = sine(48_000, 440.0, 1920, 2);
        let pcm = &all[1920..];
        let mut enc = Encoder::new(48_000, 2);
        // Zwei Blöcke eines durchgehenden Tons: der zweite beginnt eingeschwungen.
        let _ = enc.encode(&all[..1920]);
        let f = enc.encode(pcm);
        assert!(f.is_valid());
        assert_eq!(f.seq, 1);
        assert_eq!(f.data.len(), 2 * (3 + 480));
        let out = decode(&f).unwrap();
        assert_eq!(out.len(), pcm.len());
        let err: f32 = out
            .iter()
            .zip(pcm)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0, f32::max);
        assert!(err < 0.02, "max. Abweichung {err}");
    }

    #[test]
    fn frames_decode_independently() {
        let pcm = sine(24_000, 1000.0, 480, 1);
        let mut enc = Encoder::new(24_000, 1);
        let frames: Vec<_> = (0..5).map(|_| enc.encode(&pcm)).collect();
        // Den dritten allein dekodieren (die vorigen „verloren“).
        let solo = decode(&frames[3]).unwrap();
        let err: f32 = solo
            .iter()
            .zip(&pcm)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0, f32::max);
        assert!(err < 0.02, "max. Abweichung {err}");
    }

    #[test]
    fn odd_sample_count() {
        let mut enc = Encoder::new(8_000, 2);
        let f = enc.encode(&[0.1, -0.1, 0.2, -0.2, 0.3, -0.3]);
        assert_eq!(f.samples, 3);
        assert!(f.is_valid());
        assert_eq!(decode(&f).unwrap().len(), 6);
    }

    #[test]
    fn broken_frames_are_rejected() {
        let mut enc = Encoder::new(48_000, 2);
        let mut f = enc.encode(&sine(48_000, 300.0, 100, 2));
        f.data.pop();
        assert!(decode(&f).is_none());
        let mut g = enc.encode(&sine(48_000, 300.0, 100, 2));
        g.channels = 7;
        assert!(decode(&g).is_none());
        let mut h = enc.encode(&sine(48_000, 300.0, 100, 2));
        h.data[2] = 200; // Index außerhalb der Tabelle
        assert!(decode(&h).is_none());
    }

    #[test]
    fn resampler_ratio() {
        let mut r = Resampler::new(44_100, 48_000, 2);
        let mut total = 0;
        for _ in 0..100 {
            total += r.process(&sine(44_100, 440.0, 441, 2)).len() / 2;
        }
        // 1 s Eingang → ~1 s Ausgang.
        assert!((47_990..=48_010).contains(&total), "{total}");
        let mut d = Resampler::new(48_000, 24_000, 1);
        assert_eq!(d.process(&vec![0.5; 960]).len(), 480);
    }

    #[test]
    fn resampler_keeps_signal() {
        let mut r = Resampler::new(48_000, 24_000, 1);
        let out = r.process(&sine(48_000, 100.0, 4800, 1));
        let expect = sine(24_000, 100.0, out.len(), 1);
        let err: f32 = out[2..]
            .iter()
            .zip(&expect[2..])
            .map(|(a, b)| (a - b).abs())
            .fold(0.0, f32::max);
        assert!(err < 0.01, "{err}");
    }

    #[test]
    fn channel_mapping() {
        assert_eq!(map_channels(&[0.2, 0.4], 2, 1), vec![0.3f32]);
        assert_eq!(map_channels(&[0.5], 1, 2), vec![0.5, 0.5]);
        assert_eq!(
            map_channels(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0], 6, 2),
            vec![1.0, 2.0]
        );
        assert_eq!(map_channels(&[1.0, 2.0], 2, 4), vec![1.0, 2.0, 0.0, 0.0]);
    }

    #[test]
    fn packer_emits_20ms_frames_and_skips_silence() {
        let mut p = Packer::new(44_100, 2, RATE_FAST);
        let mut frames = Vec::new();
        p.push(&vec![0.0; 44_100 * 2 / 10], |f| frames.push(f));
        assert!(frames.is_empty(), "Stille wird nicht gesendet");
        p.push(&sine(44_100, 440.0, 4410, 2), |f| frames.push(f));
        assert!(frames.len() >= 4);
        for f in &frames {
            assert_eq!(f.samples, 960);
            assert_eq!(f.channels, 2);
            assert_eq!(f.rate, RATE_FAST);
        }
        // Laufende Nummern zählen auch übersprungene Stille mit.
        assert!(frames[0].seq >= 4);
    }

    #[test]
    fn packer_downmixes_surround() {
        let mut p = Packer::new(48_000, 6, RATE_SLOW);
        let mut frames = Vec::new();
        p.push(&vec![0.25; 48_000 * 6 / 25], |f| frames.push(f));
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].channels, 2);
        assert_eq!(frames[0].samples, 480);
    }

    #[test]
    fn play_buffer_primes_underruns_and_catches_up() {
        let mut b = PlayBuffer::new(1000, 2, 50, 200);
        assert_eq!(b.target(), 50);
        let mut out = vec![1.0; 20];
        b.push(&[0.5; 40]); // 20 Bilder < Vorlauf
        b.pull(&mut out);
        assert!(out.iter().all(|&s| s == 0.0), "noch im Vorlauf");
        b.push(&[0.5; 60]); // jetzt 50 Bilder
        b.pull(&mut out);
        assert!(out.iter().all(|&s| s == 0.5));
        assert_eq!(b.len_frames(), 40);
        let mut big = vec![1.0; 100];
        b.pull(&mut big); // leer gelaufen
        assert_eq!(b.underruns(), 1);
        assert!(big[80..].iter().all(|&s| s == 0.0));
        // Überlauf: auf den Vorlauf zurückschneiden.
        b.push(&vec![0.1; 2 * 250]);
        assert_eq!(b.len_frames(), 50);
    }

    #[test]
    fn play_buffer_ignores_partial_frames() {
        let mut b = PlayBuffer::new(1000, 2, 1, 10);
        b.push(&[0.1, 0.2, 0.3]);
        assert_eq!(b.len_frames(), 1);
    }
}
