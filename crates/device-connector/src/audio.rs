//! Ton des anderen Rechners hier abspielen (#40).
//!
//! * **Abspielen** (dieser Rechner): Ist „Ton vom anderen Rechner hier
//!   abspielen“ an, bittet er drüben per [`Message::AudioRequest`] um den Ton
//!   und gibt die ankommenden Blöcke über das gewählte Ausgabegerät aus
//!   (Standard: das jeweils aktuelle Standard-Ausgabegerät des Systems).
//! * **Senden** (der andere Rechner): nimmt auf Anfrage den Systemton auf
//!   (Windows: WASAPI-Loopback, macOS ab 14.2: Core-Audio-Prozess-Tap) und
//!   schickt ihn kodiert über die bestehende, verschlüsselte Verbindung.
//!
//! Immer nur eine Richtung zugleich: Spielt ein Rechner den Ton von drüben
//! ab, nimmt er selbst nichts auf, sonst liefe der Ton im Kreis.

use std::sync::Arc;

use dc_core::audio::{AudioFrame, RATE_FAST, RATE_SLOW};
use dc_core::link::Outbox;
use dc_core::Message;

pub type NoteFn = Arc<dyn Fn(String) + Send + Sync>;

/// Ein Ausgabegerät für das Menü.
#[derive(Debug, Clone, PartialEq)]
pub struct OutputDevice {
    /// Dauerhafte Kennung (wird in der Konfiguration gespeichert).
    pub id: String,
    pub name: String,
}

pub fn output_devices() -> Vec<OutputDevice> {
    backend::output_devices()
}

pub struct Audio {
    note: NoteFn,
    outbox: Option<Arc<Outbox>>,
    /// Bluetooth: halbe Abtastrate.
    slow: bool,
    want_play: bool,
    /// `None`: Standard-Ausgabegerät.
    output: Option<String>,
    capture: Option<backend::Capture>,
    player: Option<backend::Player>,
    /// Ausgabe ließ sich nicht öffnen: nicht bei jedem Block neu versuchen.
    player_failed: bool,
    /// Drüben hat bestätigt, dass Ton kommt.
    peer_sending: bool,
}

impl Audio {
    pub fn new(want_play: bool, output: Option<String>, note: NoteFn) -> Audio {
        Audio {
            note,
            outbox: None,
            slow: false,
            want_play,
            output,
            capture: None,
            player: None,
            player_failed: false,
            peer_sending: false,
        }
    }

    fn send(&self, m: Message) {
        if let Some(o) = &self.outbox {
            o.push(m);
        }
    }

    pub fn connected(&mut self, outbox: Arc<Outbox>, slow: bool) {
        self.outbox = Some(outbox);
        self.slow = slow;
        self.player_failed = false;
        if self.want_play {
            self.send(Message::AudioRequest { on: true });
        }
    }

    pub fn disconnected(&mut self) {
        self.outbox = None;
        self.peer_sending = false;
        self.stop_capture();
        self.player = None;
    }

    pub fn shutdown(&mut self) {
        if self.want_play {
            self.send(Message::AudioRequest { on: false });
        }
        self.disconnected();
    }

    pub fn set_play(&mut self, on: bool) {
        if self.want_play == on {
            return;
        }
        self.want_play = on;
        self.player_failed = false;
        if on {
            // Wer hier spielt, nimmt selbst nichts auf (sonst Rückkopplung).
            if self.capture.is_some() {
                self.stop_capture();
                self.send(Message::AudioStatus {
                    sending: false,
                    error: Some("Der andere Rechner spielt jetzt deinen Ton nicht mehr, weil er hier abgespielt werden soll.".into()),
                });
            }
            self.send(Message::AudioRequest { on: true });
        } else {
            self.send(Message::AudioRequest { on: false });
            self.player = None;
            self.peer_sending = false;
        }
    }

    pub fn set_output(&mut self, output: Option<String>) {
        self.output = output;
        self.player_failed = false;
        // Beim nächsten Block mit dem neuen Gerät öffnen.
        self.player = None;
    }

    /// Drüben möchte unseren Ton.
    pub fn on_request(&mut self, on: bool) {
        if !on {
            if self.capture.is_some() {
                log::info!("Ton senden beendet");
            }
            self.stop_capture();
            self.send(Message::AudioStatus {
                sending: false,
                error: None,
            });
            return;
        }
        if self.want_play {
            self.send(Message::AudioStatus {
                sending: false,
                error: Some(
                    "Der Ton läuft schon in die andere Richtung. Bitte an einem Rechner „Ton vom anderen Rechner hier abspielen“ ausschalten."
                        .into(),
                ),
            });
            (self.note)(
                "Ton: beide Rechner wollen den Ton des anderen, bitte an einem ausschalten".into(),
            );
            return;
        }
        let Some(outbox) = self.outbox.clone() else {
            return;
        };
        self.stop_capture();
        let rate = if self.slow { RATE_SLOW } else { RATE_FAST };
        match backend::Capture::start(outbox, rate) {
            Ok(c) => {
                log::info!("Sende Systemton ({} Hz) von „{}“", rate, c.device_name());
                self.capture = Some(c);
                self.send(Message::AudioStatus {
                    sending: true,
                    error: None,
                });
            }
            Err(e) => {
                log::warn!("Systemton aufnehmen: {e}");
                self.send(Message::AudioStatus {
                    sending: false,
                    error: Some(e),
                });
            }
        }
    }

    pub fn on_status(&mut self, sending: bool, error: Option<String>) {
        if !self.want_play {
            return;
        }
        self.peer_sending = sending;
        match error {
            Some(e) => {
                log::warn!("Ton vom anderen Rechner: {e}");
                (self.note)(format!("Ton vom anderen Rechner: {e}"));
            }
            None if sending => {
                (self.note)("Ton vom anderen Rechner wird hier abgespielt".into());
            }
            None => {}
        }
    }

    pub fn on_frame(&mut self, frame: AudioFrame) {
        if !self.want_play {
            return;
        }
        if self.player.is_none() && !self.player_failed {
            match backend::Player::start(self.output.as_deref()) {
                Ok((p, fallback)) => {
                    log::info!("Spiele Ton von drüben über „{}“", p.device_name());
                    if fallback {
                        (self.note)(format!(
                            "Gewähltes Ausgabegerät fehlt, Ton kommt über „{}“",
                            p.device_name()
                        ));
                    }
                    self.player = Some(p);
                }
                Err(e) => {
                    log::warn!("Ausgabegerät öffnen: {e}");
                    (self.note)(format!("Ton abspielen nicht möglich: {e}"));
                    self.player_failed = true;
                }
            }
        }
        if let Some(p) = &mut self.player {
            p.push(&frame);
        }
    }

    /// Regelmäßig aufrufen: folgt einem gewechselten Standard-Ausgabegerät
    /// (z. B. Kopfhörer eingesteckt) beim Abspielen wie beim Aufnehmen.
    pub fn tick(&mut self) {
        if self.player.is_none() && self.capture.is_none() {
            return;
        }
        let default = backend::default_output_id();
        if self.output.is_none() {
            if let Some(p) = &self.player {
                if default.is_some() && p.device_id() != default.as_deref() {
                    log::info!("Standard-Ausgabegerät gewechselt, Wiedergabe zieht mit");
                    self.player = None;
                    self.player_failed = false;
                }
            }
        }
        if let Some(c) = &self.capture {
            if default.is_some() && c.device_id() != default.as_deref() {
                log::info!("Standard-Ausgabegerät gewechselt, Aufnahme zieht mit");
                self.on_request(true);
            }
        }
    }

    fn stop_capture(&mut self) {
        self.capture = None;
    }
}

// ---------------------------------------------------------------- cpal

#[cfg(any(windows, target_os = "macos"))]
mod backend {
    use std::sync::{Arc, Mutex};

    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use cpal::{FromSample, Sample, SampleFormat, SizedSample, StreamConfig};
    use dc_core::audio::{decode, map_channels, AudioFrame, Packer, PlayBuffer, Resampler};
    use dc_core::link::Outbox;
    use dc_core::Message;

    use super::OutputDevice;

    /// Vorlauf und Obergrenze des Wiedergabepuffers.
    const TARGET_MS: u32 = 80;
    const MAX_MS: u32 = 300;

    fn device_id(d: &cpal::Device) -> Option<String> {
        d.id().ok().map(|id| id.to_string())
    }

    pub fn default_output_id() -> Option<String> {
        cpal::default_host()
            .default_output_device()
            .and_then(|d| device_id(&d))
    }

    pub fn output_devices() -> Vec<OutputDevice> {
        let host = cpal::default_host();
        let Ok(devs) = host.output_devices() else {
            return Vec::new();
        };
        let mut out: Vec<OutputDevice> = devs
            .filter_map(|d| {
                Some(OutputDevice {
                    id: device_id(&d)?,
                    name: d.to_string(),
                })
            })
            .collect();
        out.sort_by_key(|d| d.name.to_lowercase());
        out.dedup_by(|a, b| a.id == b.id);
        out
    }

    #[cfg(target_os = "macos")]
    fn check_os() -> Result<(), String> {
        // Prozess-Taps gibt es erst ab macOS 14.2.
        let v = crate::platform::macos_version();
        if v < (14, 2) {
            return Err(format!(
                "Den Systemton des Macs aufnehmen geht erst ab macOS 14.2 (hier {}.{}).",
                v.0, v.1
            ));
        }
        Ok(())
    }

    #[cfg(not(target_os = "macos"))]
    fn check_os() -> Result<(), String> {
        Ok(())
    }

    /// Nimmt den Systemton des Standard-Ausgabegeräts auf und sendet ihn.
    pub struct Capture {
        _stream: cpal::Stream,
        id: Option<String>,
        name: String,
    }

    impl Capture {
        pub fn start(outbox: Arc<Outbox>, out_rate: u32) -> Result<Capture, String> {
            check_os()?;
            let host = cpal::default_host();
            let dev = host
                .default_output_device()
                .ok_or("kein Ausgabegerät gefunden")?;
            let supported = dev
                .default_output_config()
                .map_err(|e| format!("Format des Ausgabegeräts: {e}"))?;
            let cfg: StreamConfig = supported.config();
            let packer = Packer::new(cfg.sample_rate, cfg.channels as usize, out_rate);
            let stream = match supported.sample_format() {
                SampleFormat::F32 => build_input::<f32>(&dev, &cfg, packer, outbox),
                SampleFormat::I16 => build_input::<i16>(&dev, &cfg, packer, outbox),
                SampleFormat::I32 => build_input::<i32>(&dev, &cfg, packer, outbox),
                SampleFormat::U16 => build_input::<u16>(&dev, &cfg, packer, outbox),
                f => return Err(format!("Tonformat {f:?} wird nicht unterstützt")),
            }
            .map_err(|e| format!("Systemton aufnehmen: {e}"))?;
            stream
                .play()
                .map_err(|e| format!("Systemton aufnehmen: {e}"))?;
            Ok(Capture {
                _stream: stream,
                id: device_id(&dev),
                name: dev.to_string(),
            })
        }

        pub fn device_id(&self) -> Option<&str> {
            self.id.as_deref()
        }

        pub fn device_name(&self) -> &str {
            &self.name
        }
    }

    fn build_input<T>(
        dev: &cpal::Device,
        cfg: &StreamConfig,
        mut packer: Packer,
        outbox: Arc<Outbox>,
    ) -> Result<cpal::Stream, cpal::Error>
    where
        T: SizedSample,
        f32: FromSample<T>,
    {
        let mut buf: Vec<f32> = Vec::new();
        dev.build_input_stream::<T, _, _>(
            *cfg,
            move |data: &[T], _| {
                buf.clear();
                buf.extend(data.iter().map(|s| f32::from_sample(*s)));
                packer.push(&buf, |f| outbox.push(Message::Audio(f)));
            },
            |e| log::warn!("Aufnahme: {e}"),
            None,
        )
    }

    /// Spielt Blöcke von drüben über ein Ausgabegerät ab.
    pub struct Player {
        _stream: cpal::Stream,
        buffer: Arc<Mutex<PlayBuffer>>,
        rate: u32,
        channels: usize,
        resampler: Option<Resampler>,
        last_seq: Option<u32>,
        id: Option<String>,
        name: String,
    }

    impl Player {
        /// `Ok((player, true))`: gewähltes Gerät fehlt, Standard genommen.
        pub fn start(output: Option<&str>) -> Result<(Player, bool), String> {
            let host = cpal::default_host();
            let chosen = output.and_then(|s| {
                let id: cpal::DeviceId = s.parse().ok()?;
                host.device_by_id(&id)
            });
            let fallback = output.is_some() && chosen.is_none();
            let dev = match chosen {
                Some(d) => d,
                None => host
                    .default_output_device()
                    .ok_or("kein Ausgabegerät gefunden")?,
            };
            let supported = dev
                .default_output_config()
                .map_err(|e| format!("Format des Ausgabegeräts: {e}"))?;
            let cfg: StreamConfig = supported.config();
            let channels = cfg.channels.max(1) as usize;
            let buffer = Arc::new(Mutex::new(PlayBuffer::new(
                cfg.sample_rate,
                channels,
                TARGET_MS,
                MAX_MS,
            )));
            let b = buffer.clone();
            let stream = match supported.sample_format() {
                SampleFormat::F32 => build_output::<f32>(&dev, &cfg, b),
                SampleFormat::I16 => build_output::<i16>(&dev, &cfg, b),
                SampleFormat::I32 => build_output::<i32>(&dev, &cfg, b),
                SampleFormat::U16 => build_output::<u16>(&dev, &cfg, b),
                f => return Err(format!("Tonformat {f:?} wird nicht unterstützt")),
            }
            .map_err(|e| e.to_string())?;
            stream.play().map_err(|e| e.to_string())?;
            Ok((
                Player {
                    _stream: stream,
                    buffer,
                    rate: cfg.sample_rate,
                    channels,
                    resampler: None,
                    last_seq: None,
                    id: device_id(&dev),
                    name: dev.to_string(),
                },
                fallback,
            ))
        }

        pub fn device_id(&self) -> Option<&str> {
            self.id.as_deref()
        }

        pub fn device_name(&self) -> &str {
            &self.name
        }

        pub fn push(&mut self, frame: &AudioFrame) {
            let Some(pcm) = decode(frame) else {
                log::debug!("Kaputter Tonblock verworfen");
                return;
            };
            if let Some(last) = self.last_seq {
                let gap = frame.seq.wrapping_sub(last);
                if gap == 0 || gap > u32::MAX / 2 {
                    return; // doppelt oder veraltet
                }
            }
            self.last_seq = Some(frame.seq);
            let mapped = map_channels(&pcm, frame.channels as usize, self.channels);
            let rs = match &mut self.resampler {
                Some(r) if r.rates() == (frame.rate, self.rate) => r,
                slot => slot.insert(Resampler::new(frame.rate, self.rate, self.channels)),
            };
            let out = rs.process(&mapped);
            self.buffer.lock().unwrap().push(&out);
        }
    }

    fn build_output<T>(
        dev: &cpal::Device,
        cfg: &StreamConfig,
        buffer: Arc<Mutex<PlayBuffer>>,
    ) -> Result<cpal::Stream, cpal::Error>
    where
        T: SizedSample + FromSample<f32>,
    {
        let mut tmp: Vec<f32> = Vec::new();
        dev.build_output_stream::<T, _, _>(
            *cfg,
            move |out: &mut [T], _| {
                tmp.resize(out.len(), 0.0);
                match buffer.lock() {
                    Ok(mut b) => b.pull(&mut tmp),
                    Err(_) => tmp.fill(0.0),
                }
                for (o, s) in out.iter_mut().zip(&tmp) {
                    *o = T::from_sample(*s);
                }
            },
            |e| log::warn!("Wiedergabe: {e}"),
            None,
        )
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod backend {
    use std::sync::Arc;

    use dc_core::audio::AudioFrame;
    use dc_core::link::Outbox;

    use super::OutputDevice;

    const UNSUPPORTED: &str = "Ton wird auf diesem System nicht unterstützt";

    pub fn default_output_id() -> Option<String> {
        None
    }

    pub fn output_devices() -> Vec<OutputDevice> {
        Vec::new()
    }

    pub struct Capture;

    impl Capture {
        pub fn start(_: Arc<Outbox>, _: u32) -> Result<Capture, String> {
            Err(UNSUPPORTED.into())
        }
        pub fn device_id(&self) -> Option<&str> {
            None
        }
        pub fn device_name(&self) -> &str {
            ""
        }
    }

    pub struct Player;

    impl Player {
        pub fn start(_: Option<&str>) -> Result<(Player, bool), String> {
            Err(UNSUPPORTED.into())
        }
        pub fn device_id(&self) -> Option<&str> {
            None
        }
        pub fn device_name(&self) -> &str {
            ""
        }
        pub fn push(&mut self, _: &AudioFrame) {}
    }
}
