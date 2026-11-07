/*
    hpsdr-rs's remote extensions to TCI, server side (docs/remote-protocol.md):
    for one connected client, the `hpsdr_*` commands it has sent and the
    streams they ask for -- spectrum rows from the main receiver's display,
    and its RX audio as IMA-ADPCM at a chosen rate. tci.rs's per-client loop
    feeds this and sends what it returns; a client that never sends an
    `hpsdr_` command never gets any of it.
*/

use std::time::{Duration, Instant};

use hpsdr_proto::adpcm;
use hpsdr_proto::bands::{self, BandInfo};
use hpsdr_proto::spectrum::SpectrumFrame;
use hpsdr_proto::state::{self as dsp, DspState};
use hpsdr_proto::tci::{Command, AUDIO, AUDIO_RATES, FEATURE_ADPCM, FEATURE_SPECTRUM, HELLO, SPECTRUM};
use hpsdr_proto::{Header, CODEC_IMA_ADPCM, FORMAT_ADPCM, PROTOCOL_VERSION, TYPE_ADPCM_AUDIO};

use std::sync::Mutex;

use crate::spectrum::{Agc, DemodParams, NoiseBlanker, NoiseReduction, SpectrumDisplay};

/// The rate RX audio arrives at (tci.rs's TCI_AUDIO_SAMPLE_RATE).
const INPUT_RATE: u32 = 48_000;
/// Audio is sent in blocks this long.
const BLOCK_MS: u32 = 20;

#[derive(Default)]
pub struct RemoteStreams {
    /// The client sent hpsdr_hello: it takes the extension streams in
    /// place of TCI's float audio.
    pub hello: bool,
    spectrum: Option<SpectrumStream>,
    audio: Option<AudioStream>,
    /// The DSP settings last sent (hpsdr_state), and when.
    last_state: Option<(DspState, Instant)>,
    /// The BandShare version last sent (hpsdr_bands); 0 = none yet.
    bands_sent: u64,
}

/// The radio's band buttons, shared between main.rs and the TCI clients'
/// threads: main.rs sets the list every frame (it only becomes "new" when
/// it changes), clients send it as hpsdr_bands, and a client's hpsdr_band
/// waits here until main.rs applies it -- as its band buttons do.
#[derive(Default)]
pub struct BandShare {
    list: Mutex<(u64, Vec<BandInfo>)>,
    requested: Mutex<Option<String>>,
}

impl BandShare {
    pub fn set(&self, bands: Vec<BandInfo>) {
        let mut list = self.list.lock().unwrap();
        if list.1 != bands {
            *list = (list.0 + 1, bands);
        }
    }

    /// The list and its version, if it's newer than `seen`.
    fn newer_than(&self, seen: u64) -> Option<(u64, Vec<BandInfo>)> {
        let list = self.list.lock().unwrap();
        (list.0 > seen).then(|| list.clone())
    }

    pub fn take_request(&self) -> Option<String> {
        self.requested.lock().unwrap().take()
    }
}

struct SpectrumStream {
    points: usize,
    fps: u32,
    interval: Duration,
    next: Instant,
    last_revision: u64,
}

impl RemoteStreams {
    /// Handles an `hpsdr_*` command, returning the reply to send, if any.
    /// Commands that aren't extensions return None and change nothing.
    pub fn handle(&mut self, cmd: &Command, params: &Mutex<DemodParams>, band_share: &BandShare) -> Option<String> {
        match cmd.name.as_str() {
            bands::BAND => {
                if let Some(name) = cmd.args.first().filter(|n| !n.is_empty()) {
                    *band_share.requested.lock().unwrap() = Some(name.clone());
                }
                None
            }
            dsp::SET => {
                apply_set(cmd, &mut params.lock().unwrap());
                None
            }
            HELLO => {
                self.hello = true;
                let reply = Command::new(HELLO, &[&PROTOCOL_VERSION.to_string(), FEATURE_SPECTRUM, FEATURE_ADPCM]);
                Some(reply.to_string())
            }
            SPECTRUM => {
                let points: usize = cmd.arg(0).unwrap_or(0);
                let fps: u32 = cmd.arg(1).unwrap_or(0);
                self.spectrum = (points > 0 && fps > 0).then(|| {
                    let fps = fps.clamp(1, 30);
                    SpectrumStream {
                        points: points.clamp(64, 4096),
                        fps,
                        interval: Duration::from_secs_f64(1.0 / fps as f64),
                        next: Instant::now(),
                        last_revision: u64::MAX,
                    }
                });
                None
            }
            AUDIO => {
                let codec: String = cmd.arg(0).unwrap_or_default();
                let rate: u32 = cmd.arg(1).unwrap_or(0);
                self.audio = (codec == FEATURE_ADPCM && AUDIO_RATES.contains(&rate)).then(|| AudioStream::new(rate));
                None
            }
            _ => None,
        }
    }

    /// `hpsdr_state` to send, if this client has said hello and it's due:
    /// once a second, or as soon as anything differs from what was last
    /// sent -- so a change made at the radio, or by another client, reaches
    /// it quickly.
    pub fn state_message(&mut self, params: &DemodParams) -> Option<String> {
        if !self.hello {
            return None;
        }
        let state = dsp_state(params);
        let due = self
            .last_state
            .as_ref()
            .is_none_or(|(last, at)| *last != state || at.elapsed() >= Duration::from_secs(1));
        if !due {
            return None;
        }
        let text = state.to_command().to_string();
        self.last_state = Some((state, Instant::now()));
        Some(text)
    }

    /// `hpsdr_bands` to send, if this client has said hello and hasn't had
    /// the current list.
    pub fn bands_message(&mut self, band_share: &BandShare) -> Option<String> {
        if !self.hello {
            return None;
        }
        let (version, list) = band_share.newer_than(self.bands_sent)?;
        self.bands_sent = version;
        Some(bands::to_command(&list).to_string())
    }

    pub fn spectrum_on(&self) -> bool {
        self.spectrum.is_some()
    }

    pub fn audio_on(&self) -> bool {
        self.audio.is_some()
    }

    /// RX audio (48 kHz stereo, as TCI streams it) in; the audio messages
    /// it completes out.
    pub fn audio_messages(&mut self, stereo: &[(f32, f32)]) -> Vec<Vec<u8>> {
        self.audio.as_mut().map_or_else(Vec::new, |a| a.push(stereo))
    }

    /// A spectrum message, if one is due and the display has a new row.
    /// `centre_hz`/`span_hz` are what that row covers.
    pub fn spectrum_message(&mut self, display: &SpectrumDisplay, centre_hz: u64, span_hz: u32) -> Option<Vec<u8>> {
        let s = self.spectrum.as_mut()?;
        let now = Instant::now();
        if now < s.next || display.revision == s.last_revision || display.spectrum.is_empty() {
            return None;
        }
        s.next = (s.next + s.interval).max(now);
        s.last_revision = display.revision;
        let frame = SpectrumFrame::from_db(&display.spectrum, s.points, centre_hz, span_hz, display.meter_db as f32);
        Some(frame.encode(s.fps))
    }
}

/// The settings hpsdr_state reports, from the receiver's.
fn dsp_state(p: &DemodParams) -> DspState {
    DspState {
        width_hz: p.width_hz.round().max(0.0) as u32,
        agc: dsp::AGC_MODES[p.agc as usize].to_string(),
        agc_gain_db: p.agc_top_db.round() as i32,
        nb: dsp::NB_MODES[match p.noise_blanker {
            NoiseBlanker::Off => 0,
            NoiseBlanker::Nb => 1,
            NoiseBlanker::Nb2 => 2,
        }]
        .to_string(),
        nr: dsp::NR_MODES[match p.noise_reduction {
            NoiseReduction::Off => 0,
            NoiseReduction::Nr => 1,
            NoiseReduction::Nr2 => 2,
            NoiseReduction::Nr3 => 3,
        }]
        .to_string(),
        anf: p.anf,
        snb: p.snb,
    }
}

/// Applies one `hpsdr_set:<name>,<value>;`. Unknown names or values change
/// nothing. Ranges as the main window's own controls.
fn apply_set(cmd: &Command, p: &mut DemodParams) {
    let name: String = cmd.arg(0).unwrap_or_default();
    let value = cmd.args.get(1).map(|v| v.trim().to_ascii_lowercase()).unwrap_or_default();
    match name.as_str() {
        dsp::SET_WIDTH => {
            if let Ok(hz) = value.parse::<f64>() {
                p.width_hz = hz.clamp(50.0, 20_000.0);
            }
        }
        dsp::SET_AGC => {
            p.agc = match value.as_str() {
                "off" => Agc::Off,
                "long" => Agc::Long,
                "slow" => Agc::Slow,
                "medium" => Agc::Medium,
                "fast" => Agc::Fast,
                _ => return,
            }
        }
        dsp::SET_AGC_GAIN => {
            if let Ok(db) = value.parse::<f64>() {
                p.agc_top_db = db.clamp(0.0, 140.0);
            }
        }
        dsp::SET_NB => {
            p.noise_blanker = match value.as_str() {
                "off" => NoiseBlanker::Off,
                "nb" => NoiseBlanker::Nb,
                "nb2" => NoiseBlanker::Nb2,
                _ => return,
            }
        }
        dsp::SET_NR => {
            p.noise_reduction = match value.as_str() {
                "off" => NoiseReduction::Off,
                "nr" => NoiseReduction::Nr,
                "nr2" => NoiseReduction::Nr2,
                "nr3" => NoiseReduction::Nr3,
                _ => return,
            }
        }
        dsp::SET_ANF => p.anf = value == "true",
        dsp::SET_SNB => p.snb = value == "true",
        _ => {}
    }
}

/// RX audio to IMA-ADPCM blocks at the client's rate: mono, low-pass
/// filtered and decimated from 48 kHz.
struct AudioStream {
    rate: u32,
    factor: usize,
    taps: Vec<f32>,
    history: Vec<f32>,
    pos: usize,
    phase: usize,
    block: Vec<i16>,
    block_len: usize,
    encoder: adpcm::Encoder,
}

impl AudioStream {
    fn new(rate: u32) -> Self {
        let factor = (INPUT_RATE / rate) as usize;
        // Windowed sinc, cut off at 0.45 of the output rate -- under its
        // Nyquist, above the ~3 kHz SSB audio that matters most.
        let n = 16 * factor + 1;
        let cutoff = 0.45 / factor as f64;
        let taps: Vec<f32> = (0..n)
            .map(|k| {
                let t = k as f64 - (n - 1) as f64 / 2.0;
                let sinc = if t == 0.0 {
                    2.0 * cutoff
                } else {
                    (2.0 * std::f64::consts::PI * cutoff * t).sin() / (std::f64::consts::PI * t)
                };
                let window = 0.54 - 0.46 * (2.0 * std::f64::consts::PI * k as f64 / (n - 1).max(1) as f64).cos();
                (sinc * window) as f32
            })
            .collect();
        Self {
            rate,
            factor,
            history: vec![0.0; taps.len()],
            taps,
            pos: 0,
            phase: 0,
            block: Vec::new(),
            block_len: (rate * BLOCK_MS / 1000) as usize,
            encoder: adpcm::Encoder::new(),
        }
    }

    fn push(&mut self, stereo: &[(f32, f32)]) -> Vec<Vec<u8>> {
        let mut messages = Vec::new();
        let n = self.taps.len();
        for &(l, r) in stereo {
            self.history[self.pos] = 0.5 * (l + r);
            self.pos = (self.pos + 1) % n;
            self.phase += 1;
            if self.phase < self.factor {
                continue;
            }
            self.phase = 0;
            let y: f32 = self.taps.iter().enumerate().map(|(k, &t)| t * self.history[(self.pos + k) % n]).sum();
            self.block.push((y * 32767.0).clamp(-32768.0, 32767.0) as i16);
            if self.block.len() == self.block_len {
                messages.push(self.message());
                self.block.clear();
            }
        }
        messages
    }

    fn message(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        Header {
            sample_rate: self.rate,
            format: FORMAT_ADPCM,
            codec: CODEC_IMA_ADPCM,
            length: self.block.len() as u32,
            msg_type: TYPE_ADPCM_AUDIO,
            channels: 1,
            ..Default::default()
        }
        .encode(&mut out);
        out.extend(self.encoder.encode(&self.block));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hpsdr_proto::tci::parse;

    fn handle(r: &mut RemoteStreams, text: &str) -> Option<String> {
        r.handle(&parse(text)[0], &Mutex::new(DemodParams::default()), &BandShare::default())
    }

    /// The band list goes once after hello and again when it changes; a
    /// band selection waits for main.rs.
    #[test]
    fn bands_are_sent_and_selected() {
        let mut r = RemoteStreams::default();
        let share = BandShare::default();
        let params = Mutex::new(DemodParams::default());
        share.set(vec![BandInfo { name: "40m".into(), low_hz: 7_000_000, high_hz: 7_300_000 }]);
        assert!(r.bands_message(&share).is_none(), "not before hello");
        r.handle(&parse("hpsdr_hello:1;")[0], &params, &share);
        assert_eq!(r.bands_message(&share).as_deref(), Some("hpsdr_bands:40m,7000000,7300000;"));
        assert!(r.bands_message(&share).is_none(), "already sent");
        share.set(vec![BandInfo { name: "40m".into(), low_hz: 7_000_000, high_hz: 7_300_000 }]);
        assert!(r.bands_message(&share).is_none(), "the same list isn't new");
        share.set(vec![]);
        assert_eq!(r.bands_message(&share).as_deref(), Some("hpsdr_bands;"));
        r.handle(&parse("hpsdr_band:QO-100;")[0], &params, &share);
        assert_eq!(share.take_request().as_deref(), Some("QO-100"));
        assert!(share.take_request().is_none());
    }

    /// hpsdr_set changes the receiver's settings, and hpsdr_state reports
    /// them -- at once after a change, otherwise once a second.
    #[test]
    fn settings_are_set_and_reported() {
        let mut r = RemoteStreams::default();
        let params = Mutex::new(DemodParams::default());
        assert!(r.state_message(&params.lock().unwrap()).is_none(), "not before hello");
        r.handle(&parse("hpsdr_hello:1;")[0], &params, &BandShare::default());
        let first = r.state_message(&params.lock().unwrap()).unwrap();
        assert!(first.starts_with("hpsdr_state:"));
        assert!(r.state_message(&params.lock().unwrap()).is_none(), "unchanged, under a second");
        for text in ["hpsdr_set:width,1800;", "hpsdr_set:agc,slow;", "hpsdr_set:agc_gain,70;", "hpsdr_set:nb,nb2;",
                     "hpsdr_set:nr,nr3;", "hpsdr_set:anf,true;", "hpsdr_set:snb,true;", "hpsdr_set:nr,bogus;"] {
            r.handle(&parse(text)[0], &params, &BandShare::default());
        }
        let state = r.state_message(&params.lock().unwrap()).unwrap();
        assert_eq!(state, "hpsdr_state:1800,slow,70,nb2,nr3,true,true;");
    }

    #[test]
    fn hello_and_stream_commands() {
        let mut r = RemoteStreams::default();
        assert_eq!(handle(&mut r, "hpsdr_hello:1;").as_deref(), Some("hpsdr_hello:1,spectrum,adpcm;"));
        assert!(r.hello);
        assert!(handle(&mut r, "hpsdr_audio:adpcm,24000;").is_none());
        assert!(r.audio_on());
        handle(&mut r, "hpsdr_audio:adpcm,22050;");
        assert!(!r.audio_on(), "an unsupported rate turns it off");
        assert!(handle(&mut r, "vfo:0,0,7000000;").is_none());
    }

    /// 1 s of a 1 kHz tone at 48 kHz comes out as 50 blocks of 20 ms at
    /// 24 kHz, which decode back to the tone.
    #[test]
    fn audio_is_decimated_into_20_ms_blocks() {
        let mut r = RemoteStreams::default();
        handle(&mut r, "hpsdr_audio:adpcm,24000;");
        let stereo: Vec<(f32, f32)> = (0..48_000)
            .map(|i| {
                let v = 0.3 * (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / 48_000.0).sin();
                (v, v)
            })
            .collect();
        let messages = r.audio_messages(&stereo);
        assert_eq!(messages.len(), 50);
        let (header, payload) = Header::decode(&messages[25]).unwrap();
        assert_eq!((header.msg_type, header.sample_rate, header.length), (TYPE_ADPCM_AUDIO, 24_000, 480));
        let samples = adpcm::decode(payload, 480).unwrap();
        let peak = samples.iter().map(|s| s.unsigned_abs()).max().unwrap() as f32 / 32767.0;
        assert!((peak - 0.3).abs() < 0.03, "peak {peak}");
    }

    #[test]
    fn spectrum_only_sends_new_rows_at_the_rate_asked() {
        let mut r = RemoteStreams::default();
        let display = SpectrumDisplay { spectrum: vec![-100.0; 1024], revision: 1, ..Default::default() };
        assert!(r.spectrum_message(&display, 7_100_000, 192_000).is_none(), "not asked for");
        handle(&mut r, "hpsdr_spectrum:256,10;");
        let msg = r.spectrum_message(&display, 7_100_000, 192_000).unwrap();
        let (header, payload) = Header::decode(&msg).unwrap();
        let frame = SpectrumFrame::decode(&header, payload).unwrap();
        assert_eq!((frame.points.len(), frame.centre_hz, frame.span_hz), (256, 7_100_000, 192_000));
        let newer = SpectrumDisplay { revision: 2, ..display };
        assert!(r.spectrum_message(&newer, 7_100_000, 192_000).is_none(), "too soon after the last");
    }
}
