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
use hpsdr_proto::cw;
use hpsdr_proto::spectrum::SpectrumFrame;
use hpsdr_proto::state::{self as dsp, DspState};
use hpsdr_proto::tci::{
    Command, AUDIO, AUDIO_RATES, FEATURE_ADPCM, FEATURE_CW, FEATURE_KEY, FEATURE_SPECTRUM, FEATURE_TX, HELLO, NOTICE,
    SPECTRUM,
};
use hpsdr_proto::tx::{self, TxState};
use hpsdr_proto::{Header, CODEC_IMA_ADPCM, FORMAT_ADPCM, PROTOCOL_VERSION, TYPE_ADPCM_AUDIO};

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::spectrum::{Agc, DemodParams, Mode, NoiseBlanker, NoiseReduction, SpectrumDisplay};

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
    /// The RemoteShare version last sent (hpsdr_bands); 0 = none yet.
    bands_sent: u64,
    /// Set while this client has the radio keyed (hpsdr_mox) -- see Keyed.
    keyed: Option<Keyed>,
    /// The TX state last sent (hpsdr_tx), and when.
    last_tx: Option<(TxState, Instant)>,
    /// CW text sending, as last told to this client: the speed
    /// (cw_macros_speed), whether text was being sent (for
    /// cw_macros_empty), and the saved messages' version
    /// (hpsdr_cw_messages; 0 = none yet), and the paddle keys' version
    /// (hpsdr_paddle_keys; 0 = none yet).
    cw_speed_sent: Option<u32>,
    cw_was_sending: bool,
    cw_messages_sent: u64,
    paddle_keys_sent: u64,
    /// Live keying (hpsdr_key, or TCI's keyer): this client's burst of
    /// keying in progress, if any -- see LiveKey.
    live: Option<LiveKey>,
    /// Its last key-down was refused (the reason already told): the rest
    /// of that burst is ignored until the key comes up.
    key_refused: bool,
    /// TCI's keyer: when its key went down, ms on this thread's clock.
    keyer_down_ms: Option<u64>,
    /// This thread's clock for TCI's keyer, which has no times of its own.
    epoch: Option<Instant>,
}

/// A client keying the transmitter live (radio::CwLive). Its first key-down
/// of a burst is played `delay` after it arrives; every later transition at
/// the same offset from the first as on the client's clock -- so the
/// client's timing survives network jitter up to the delay. The burst ends,
/// and the radio unkeys, when the key has been up for the CW hang time.
struct LiveKey {
    /// The burst's first transition: its time on the client's clock (ms)
    /// and the sample it was scheduled for on the transmitter's.
    anchor_ms: u64,
    anchor_sample: u64,
    rate: u64,
    /// The last transition scheduled, and whether the key is down.
    last_sample: u64,
    down: bool,
    last_heard: Instant,
    mox: Arc<AtomicBool>,
    cw: Arc<crate::radio::CwKeyerAtomics>,
}

impl LiveKey {
    /// Lets go: the key up now, and the transmitter told keying is over --
    /// it plays out the release and unkeys with MOX.
    fn end(&self) {
        let live = &self.cw.live;
        if self.down {
            live.events.lock().unwrap().push_back((live.clock.load(Ordering::Relaxed), false));
        }
        live.active.store(false, Ordering::Relaxed);
        self.mox.store(false, Ordering::Relaxed);
    }
}

/// The transmitter state a client's thread keys and feeds: the same TCI's
/// own trx and TX audio use (tci.rs's handle_client).
pub struct TxLink<'a> {
    pub mox: &'a Arc<AtomicBool>,
    pub tx_frequency_hz: &'a AtomicU32,
    pub allow_out_of_band: &'a AtomicBool,
    /// RadioSession::tci_wants_mic: false takes TX audio from TCI.
    pub wants_mic: &'a AtomicBool,
    /// RadioSession::tci_tx_audio: 48 kHz mono, which the transmitter
    /// takes in preference to the mic while it has any.
    pub audio: &'a Mutex<VecDeque<f32>>,
    /// RadioSession::cw_keyer: the hang time, and live keying (CwLive).
    pub cw: &'a Arc<crate::radio::CwKeyerAtomics>,
}

/// A client's transmission. Unkeys if the client stops sending TX audio
/// (TX_AUDIO_TIMEOUT -- a phone put to sleep, a stalled link) or goes away
/// (RemoteStreams' Drop), so a lost remote can't leave the radio keyed.
struct Keyed {
    mox: Arc<AtomicBool>,
    last_audio: Instant,
    upsampler: Upsampler,
}

/// No TX audio from a keyed client for this long unkeys it.
const TX_AUDIO_TIMEOUT: Duration = Duration::from_secs(1);
/// The rate the transmitter takes TX audio at (tci.rs's
/// TCI_AUDIO_SAMPLE_RATE).
const TX_RATE: u32 = 48_000;
/// Silence queued at key-up, so network jitter doesn't empty the queue.
const TX_PRIME: usize = TX_RATE as usize / 10;
/// Below this the queue is topped up with silence: an empty queue makes
/// the transmitter take the radio's own mic instead (tx.rs's Auto source).
const TX_LOW: usize = TX_RATE as usize / 100;
/// Above this (the client's clock running fast against the radio's), the
/// queue is cut back to TX_TARGET, so the delay can't keep growing.
const TX_HIGH: usize = TX_RATE as usize * 3 / 10;
const TX_TARGET: usize = TX_RATE as usize * 3 / 20;

impl Drop for RemoteStreams {
    fn drop(&mut self) {
        if let Some(k) = &self.keyed {
            k.mox.store(false, Ordering::Relaxed);
            eprintln!("remote: client went while transmitting -- unkeyed");
        }
        if let Some(k) = &self.live {
            k.end();
            eprintln!("remote: client went while keying CW -- unkeyed");
        }
    }
}

/// Live keying: once the key is up, the radio stays keyed this much
/// longer than the CW hang time, for the release's ramp to play out.
const KEY_RAMP_MS: u64 = 10;
/// TCI's keyer commands carry no times of their own: played this long
/// after they arrive.
const KEYER_DELAY_MS: u32 = 100;

/// Linear interpolation up to TX_RATE, carrying its place across blocks.
/// Plenty here: WDSP's TX filter removes the images above the voice band.
#[derive(Default)]
struct Upsampler {
    prev: f32,
    pos: f64,
}

impl Upsampler {
    fn push(&mut self, input: &[f32], rate: u32, out: &mut Vec<f32>) {
        let step = rate as f64 / TX_RATE as f64;
        for &x in input {
            while self.pos < 1.0 {
                out.push(self.prev + (x - self.prev) * self.pos as f32);
                self.pos += step;
            }
            self.pos -= 1.0;
            self.prev = x;
        }
    }
}

/// What main.rs and the TCI clients' threads share for remote clients.
/// The radio's band buttons: main.rs sets the list every frame (it only
/// becomes "new" when it changes), clients send it as hpsdr_bands, and a
/// client's hpsdr_band waits here until main.rs applies it -- as its band
/// buttons do. The same for the transmitter: main.rs sets what hpsdr_tx
/// reports (TxInfo), and a client's power setting waits for it -- and CW
/// text to send (CwRequests), which main.rs feeds to the same queue as
/// CAT's KY, and the saved CW messages it shows clients. Also counts
/// the remote clients connected, for the main window's Web indicator (see
/// ClientCount).
#[derive(Default)]
pub struct RemoteShare {
    list: Mutex<(u64, Vec<BandInfo>)>,
    requested: Mutex<Option<String>>,
    clients: AtomicUsize,
    tx: Mutex<TxInfo>,
    power_request: Mutex<Option<u32>>,
    control: Mutex<ControlRequests>,
    /// A client has changed receiver 0's filter width (TCI's
    /// rx_filter_band, or an hpsdr-remote client's hpsdr_set width):
    /// main.rs makes the TX filter follow, as its own width control does.
    filter_changed: std::sync::atomic::AtomicBool,
    /// TCI's receiver 1 -- see crate::tci::Rx2Share.
    pub rx2: crate::tci::Rx2Share,
    cw: Mutex<CwRequests>,
    cw_messages: Mutex<(u64, Vec<String>)>,
    paddle_keys: Mutex<(u64, (char, char))>,
}

/// CW text clients have asked to send (TCI's cw_macros), a stop
/// (cw_macros_stop), and a speed (cw_macros_speed), waiting for main.rs.
#[derive(Debug, Default, PartialEq)]
pub struct CwRequests {
    pub text: Vec<String>,
    pub stop: bool,
    pub speed_wpm: Option<u32>,
}

/// TCI's tune and tune_drive, for main.rs to apply as the TUNE button and
/// Settings -> TX -> Tune Power: the latest of each since it last looked.
#[derive(Debug, Default, PartialEq)]
pub struct ControlRequests {
    pub tune: Option<bool>,
    pub tune_power_percent: Option<u32>,
    /// split_enable, as the Split button.
    pub split: Option<bool>,
}

/// The transmitter as main.rs sees it, for hpsdr_tx.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TxInfo {
    /// This radio can transmit (not a receive-only one, and TX not off).
    pub available: bool,
    /// Remote clients may transmit (Settings -> Network -> Web client ->
    /// Allow transmit). Off, they're told TX isn't available.
    pub remote_allowed: bool,
    pub power_w: u32,
    pub max_power_w: u32,
    /// Measured, while transmitting; 0 and 1 otherwise.
    pub forward_w: f32,
    pub swr: f32,
    /// CW text is being sent (the Send CW button, CAT's KY, or a client's
    /// cw_macros), and the CW speed, WPM.
    pub cw_sending: bool,
    pub cw_speed_wpm: u32,
    /// The Tune button is on, at tune_power_percent (Settings -> TX,
    /// Tune Power); split is on (TX on VFO B). For TCI's tune,
    /// tune_drive and split_enable.
    pub tune: bool,
    pub tune_power_percent: u32,
    pub split: bool,
    /// Transmitting on VFO B: Split, or Duplex -- for TCI's tx_frequency.
    pub tx_on_vfo_b: bool,
    /// The microphone's peak level, dB (WDSP's TXA mic peak meter), while
    /// transmitting -- for TCI's tx_sensors.
    pub mic_db: f32,
}

/// One connected remote client (one that said hpsdr_hello), counted in
/// RemoteShare for as long as this lives -- the client's thread holds it,
/// so however it ends, the count comes down.
pub struct ClientCount(Arc<RemoteShare>);

impl ClientCount {
    pub fn new(share: Arc<RemoteShare>) -> Self {
        share.clients.fetch_add(1, Ordering::Relaxed);
        Self(share)
    }
}

impl Drop for ClientCount {
    fn drop(&mut self) {
        self.0.clients.fetch_sub(1, Ordering::Relaxed);
    }
}

impl RemoteShare {
    pub fn set(&self, bands: Vec<BandInfo>) {
        let mut list = self.list.lock().unwrap();
        if list.1 != bands {
            *list = (list.0 + 1, bands);
        }
    }

    /// The band (its name) `hz` is in, from the same list, if any.
    pub fn band_name(&self, hz: u64) -> Option<String> {
        let list = self.list.lock().unwrap();
        list.1.iter().find(|b| (b.low_hz..=b.high_hz).contains(&hz)).map(|b| b.name.clone())
    }

    /// The list and its version, if it's newer than `seen`.
    fn newer_than(&self, seen: u64) -> Option<(u64, Vec<BandInfo>)> {
        let list = self.list.lock().unwrap();
        (list.0 > seen).then(|| list.clone())
    }

    pub fn take_request(&self) -> Option<String> {
        self.requested.lock().unwrap().take()
    }

    /// Remote clients connected now.
    pub fn clients(&self) -> usize {
        self.clients.load(Ordering::Relaxed)
    }

    pub fn set_tx(&self, info: TxInfo) {
        *self.tx.lock().unwrap() = info;
    }

    pub fn tx(&self) -> TxInfo {
        *self.tx.lock().unwrap()
    }

    /// A TX power setting a remote client has asked for, W.
    pub fn take_power_request(&self) -> Option<u32> {
        self.power_request.lock().unwrap().take()
    }

    /// A TX power setting a TCI client has asked for (drive), W.
    pub fn request_power(&self, watts: u32) {
        *self.power_request.lock().unwrap() = Some(watts);
    }

    /// Tune and Tune Power, as TCI clients have asked since the last call.
    pub fn take_control(&self) -> ControlRequests {
        std::mem::take(&mut *self.control.lock().unwrap())
    }

    /// See filter_changed.
    pub fn filter_changed(&self) {
        self.filter_changed.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Whether a client has changed receiver 0's filter width since the
    /// last call.
    pub fn take_filter_changed(&self) -> bool {
        self.filter_changed.swap(false, std::sync::atomic::Ordering::Relaxed)
    }

    pub fn request_split(&self, on: bool) {
        self.control.lock().unwrap().split = Some(on);
    }

    pub fn request_tune(&self, on: bool) {
        self.control.lock().unwrap().tune = Some(on);
    }

    pub fn request_tune_power(&self, percent: u32) {
        self.control.lock().unwrap().tune_power_percent = Some(percent);
    }

    /// CW text, a stop and a speed clients have asked for since the last
    /// call.
    pub fn take_cw(&self) -> CwRequests {
        std::mem::take(&mut *self.cw.lock().unwrap())
    }

    /// The saved CW messages, for hpsdr_cw_messages: "new" only when they
    /// change.
    pub fn set_cw_messages(&self, list: &[String]) {
        let mut m = self.cw_messages.lock().unwrap();
        if m.1 != list {
            *m = (m.0 + 1, list.to_vec());
        }
    }

    /// The keys a paddle plugged into a client as a keyboard types --
    /// (dit, dah) -- for hpsdr_paddle_keys: "new" the first time and when
    /// they change.
    pub fn set_paddle_keys(&self, keys: (char, char)) {
        let mut m = self.paddle_keys.lock().unwrap();
        if m.0 == 0 || m.1 != keys {
            *m = (m.0 + 1, keys);
        }
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
    pub fn handle(
        &mut self,
        cmd: &Command,
        params: &Mutex<DemodParams>,
        share: &RemoteShare,
        tx: &TxLink,
    ) -> Option<String> {
        match cmd.name.as_str() {
            bands::BAND => {
                if let Some(name) = cmd.args.first().filter(|n| !n.is_empty()) {
                    *share.requested.lock().unwrap() = Some(name.clone());
                }
                None
            }
            dsp::SET if cmd.args.first().map(String::as_str) == Some(tx::SET_POWER) => {
                *share.power_request.lock().unwrap() = cmd.arg(1);
                None
            }
            dsp::SET => {
                apply_set(cmd, &mut params.lock().unwrap());
                if cmd.args.first().map(String::as_str) == Some(dsp::SET_WIDTH) {
                    share.filter_changed();
                }
                None
            }
            cw::KEY => {
                let (time_ms, down, delay_ms) = cw::key_from(cmd)?;
                let mode = params.lock().unwrap().mode;
                self.key(time_ms, down, delay_ms, mode, share, tx)
            }
            tx::MOX => {
                let on = cmd.args.first().is_some_and(|a| a == "true");
                let mode = params.lock().unwrap().mode;
                self.set_mox(on, mode, share, tx).map(|why| Command::new(NOTICE, &[why]).to_string())
            }
            HELLO => {
                self.hello = true;
                let reply = Command::new(
                    HELLO,
                    &[&PROTOCOL_VERSION.to_string(), FEATURE_SPECTRUM, FEATURE_ADPCM, FEATURE_TX, FEATURE_CW, FEATURE_KEY],
                );
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
    pub fn bands_message(&mut self, share: &RemoteShare) -> Option<String> {
        if !self.hello {
            return None;
        }
        let (version, list) = share.newer_than(self.bands_sent)?;
        self.bands_sent = version;
        Some(bands::to_command(&list).to_string())
    }

    /// Keys or unkeys for this client; returns why not, if it couldn't key.
    /// Anyone may unkey, whoever keyed.
    fn set_mox(&mut self, on: bool, mode: Mode, share: &RemoteShare, tx: &TxLink) -> Option<&'static str> {
        if !on {
            tx.mox.store(false, Ordering::Relaxed);
            self.keyed = None;
            return None;
        }
        let info = share.tx();
        if !info.available {
            return Some("This radio can't transmit");
        }
        if !info.remote_allowed {
            return Some("Transmitting from remote clients is turned off at the radio");
        }
        if matches!(mode, Mode::Cwl | Mode::Cwu) {
            return Some("In CW, send text with the CW controls - MOX is for voice and data");
        }
        if !crate::tx_frequency_allowed(
            tx.tx_frequency_hz.load(Ordering::Relaxed),
            tx.allow_out_of_band.load(Ordering::Relaxed),
        ) {
            return Some("Out of band - not keyed");
        }
        tx.wants_mic.store(false, Ordering::Relaxed);
        {
            let mut q = tx.audio.lock().unwrap();
            q.clear();
            q.extend(std::iter::repeat_n(0.0, TX_PRIME));
        }
        tx.mox.store(true, Ordering::Relaxed);
        self.keyed = Some(Keyed { mox: Arc::clone(tx.mox), last_audio: Instant::now(), upsampler: Upsampler::default() });
        None
    }

    /// A binary message from the client: TX audio (IMA-ADPCM), taken only
    /// while it has the radio keyed. Anything else is ignored.
    pub fn tx_audio(&mut self, data: &[u8], tx: &TxLink) {
        let Some(k) = self.keyed.as_mut() else { return };
        let Some((header, payload)) = Header::decode(data) else { return };
        if header.msg_type != TYPE_ADPCM_AUDIO || !AUDIO_RATES.contains(&header.sample_rate) {
            return;
        }
        let Some(samples) = adpcm::decode(payload, header.length as usize) else { return };
        k.last_audio = Instant::now();
        let input: Vec<f32> = samples.iter().map(|&s| s as f32 / 32768.0).collect();
        let mut out = Vec::with_capacity(input.len() * (TX_RATE / header.sample_rate) as usize + 2);
        k.upsampler.push(&input, header.sample_rate, &mut out);
        let mut q = tx.audio.lock().unwrap();
        q.extend(out);
        if q.len() > TX_HIGH {
            let excess = q.len() - TX_TARGET;
            q.drain(..excess);
        }
    }

    /// Called every pass of the client's loop: ends this client's
    /// transmission if the radio was unkeyed elsewhere, if remote transmit
    /// was turned off, or if its TX audio has stopped (returning why, to
    /// log); otherwise keeps the TX queue from running empty (see TX_LOW).
    pub fn tx_tick(&mut self, share: &RemoteShare, tx: &TxLink) -> Option<&'static str> {
        let k = self.keyed.as_ref()?;
        if !k.mox.load(Ordering::Relaxed) {
            self.keyed = None;
            return None;
        }
        let info = share.tx();
        if !info.available || !info.remote_allowed {
            k.mox.store(false, Ordering::Relaxed);
            self.keyed = None;
            return Some("remote transmit turned off while a remote client was transmitting -- unkeyed");
        }
        if k.last_audio.elapsed() >= TX_AUDIO_TIMEOUT {
            k.mox.store(false, Ordering::Relaxed);
            self.keyed = None;
            return Some("no TX audio from the remote client for a second -- unkeyed");
        }
        let mut q = tx.audio.lock().unwrap();
        if q.len() < TX_LOW {
            q.extend(std::iter::repeat_n(0.0, TX_LOW));
        }
        None
    }

    /// `hpsdr_tx` to send, if this client has said hello and it's due:
    /// five times a second while transmitting, once a second otherwise, and
    /// straight away when keying or the settings change.
    pub fn tx_message(&mut self, share: &RemoteShare, mox: &AtomicBool) -> Option<String> {
        if !self.hello {
            return None;
        }
        let info = share.tx();
        let on = mox.load(Ordering::Relaxed);
        let state = TxState {
            // Off for remote clients is, to them, a radio that can't.
            available: info.available && info.remote_allowed,
            mox: on,
            power_w: info.power_w,
            max_power_w: info.max_power_w,
            forward_w: if on { info.forward_w } else { 0.0 },
            swr: if on { info.swr } else { 1.0 },
        };
        let interval = Duration::from_millis(if on { 200 } else { 1000 });
        let due = self.last_tx.as_ref().is_none_or(|(last, at)| {
            (last.available, last.mox, last.power_w, last.max_power_w)
                != (state.available, state.mox, state.power_w, state.max_power_w)
                || at.elapsed() >= interval
        });
        if !due {
            return None;
        }
        self.last_tx = Some((state, Instant::now()));
        Some(state.to_command().to_string())
    }

    /// TCI's CW text commands (cw_macros, cw_macros_stop, cw_macros_speed)
    /// from any client, standard TCI or hpsdr-remote: queued for main.rs,
    /// which sends the text as the Send CW button does (in CW mode, not
    /// while otherwise transmitting, in band). Text that can't be sent now
    /// is dropped here rather than left to go out later -- with the reason,
    /// to a client that said hpsdr_hello. Returns the reply, if any.
    pub fn cw_command(&mut self, cmd: &Command, mode: Mode, share: &RemoteShare, tx: &TxLink) -> Option<String> {
        let notice = |why: &str| self.hello.then(|| Command::new(NOTICE, &[why]).to_string());
        match cmd.name.as_str() {
            cw::MACROS_STOP => {
                share.cw.lock().unwrap().stop = true;
                None
            }
            cw::MACROS_SPEED => {
                let wpm: u32 = cmd.arg(0)?;
                share.cw.lock().unwrap().speed_wpm = Some(wpm.clamp(cw::SPEED_MIN, cw::SPEED_MAX));
                None
            }
            cw::MACROS => {
                let text = cw::macros_text(cmd)?;
                let info = share.tx();
                if !info.available {
                    return notice("This radio can't transmit");
                }
                if self.hello && !info.remote_allowed {
                    return notice("Transmitting from remote clients is turned off at the radio");
                }
                if !matches!(mode, Mode::Cwl | Mode::Cwu) {
                    return notice("Choose CW to send CW");
                }
                if !crate::tx_frequency_allowed(
                    tx.tx_frequency_hz.load(Ordering::Relaxed),
                    tx.allow_out_of_band.load(Ordering::Relaxed),
                ) {
                    return notice("Out of band - not sent");
                }
                share.cw.lock().unwrap().text.push(text);
                None
            }
            _ => None,
        }
    }

    /// A live key transition (hpsdr_key, or TCI's keyer): `time_ms` on the
    /// client's clock. The first key-down of a burst keys the radio -- only
    /// where CW text could be sent (see cw_command), and not while anything
    /// else is transmitting -- and is played `delay_ms` later; each later
    /// transition keeps its spacing from the first. A repeat of the key's
    /// state just shows the client is still there. Returns a notice if the
    /// key-down was refused (once a burst).
    pub fn key(&mut self, time_ms: u64, down: bool, delay_ms: u32, mode: Mode, share: &RemoteShare, tx: &TxLink) -> Option<String> {
        let live = &tx.cw.live;
        if let Some(k) = self.live.as_mut() {
            k.last_heard = Instant::now();
            if down == k.down {
                return None;
            }
            let offset = time_ms.saturating_sub(k.anchor_ms) * k.rate / 1000;
            let at = (k.anchor_sample + offset).max(k.last_sample).max(live.clock.load(Ordering::Relaxed));
            live.events.lock().unwrap().push_back((at, down));
            k.last_sample = at;
            k.down = down;
            return None;
        }
        if !down {
            self.key_refused = false;
            return None;
        }
        if self.key_refused {
            return None;
        }
        let info = share.tx();
        let rate = live.rate.load(Ordering::Relaxed) as u64;
        let why = if !info.available || rate == 0 {
            Some("This radio can't transmit")
        } else if self.hello && !info.remote_allowed {
            Some("Transmitting from remote clients is turned off at the radio")
        } else if !matches!(mode, Mode::Cwl | Mode::Cwu) {
            Some("Choose CW to key CW")
        } else if !crate::tx_frequency_allowed(
            tx.tx_frequency_hz.load(Ordering::Relaxed),
            tx.allow_out_of_band.load(Ordering::Relaxed),
        ) {
            Some("Out of band - not keyed")
        } else if tx.mox.load(Ordering::Relaxed) || info.cw_sending || live.active.load(Ordering::Relaxed) {
            Some("The radio is already transmitting")
        } else {
            None
        };
        if let Some(why) = why {
            self.key_refused = true;
            return self.hello.then(|| Command::new(NOTICE, &[why]).to_string());
        }
        let at = live.clock.load(Ordering::Relaxed) + delay_ms as u64 * rate / 1000;
        {
            let mut events = live.events.lock().unwrap();
            events.clear();
            events.push_back((at, true));
        }
        live.active.store(true, Ordering::Relaxed);
        tx.mox.store(true, Ordering::Relaxed);
        self.live = Some(LiveKey {
            anchor_ms: time_ms,
            anchor_sample: at,
            rate,
            last_sample: at,
            down: true,
            last_heard: Instant::now(),
            mox: Arc::clone(tx.mox),
            cw: Arc::clone(tx.cw),
        });
        None
    }

    /// Called every pass of the client's loop: ends this client's burst of
    /// live keying once the key has been up for the hang time, or at once
    /// if the radio was unkeyed elsewhere, remote transmit was turned off,
    /// or the client has gone quiet with the key down (returning why, to
    /// log).
    pub fn key_tick(&mut self, share: &RemoteShare) -> Option<&'static str> {
        let k = self.live.as_mut()?;
        let live = &k.cw.live;
        let why = if !k.mox.load(Ordering::Relaxed) {
            Some("radio unkeyed elsewhere while a remote client was keying CW")
        } else if !share.tx().available || (self.hello && !share.tx().remote_allowed) {
            Some("remote transmit turned off while a remote client was keying CW -- unkeyed")
        } else if k.down && k.last_heard.elapsed() >= Duration::from_millis(cw::KEY_TIMEOUT_MS as u64) {
            Some("nothing from the remote client keying CW for 1.5 s -- key released, unkeyed")
        } else {
            None
        };
        let hang_ms = k.cw.hang_time_ms.load(Ordering::Relaxed) as u64 + KEY_RAMP_MS;
        let hung = !k.down && live.clock.load(Ordering::Relaxed) >= k.last_sample + hang_ms * k.rate / 1000;
        if why.is_some() || hung {
            k.end();
            self.live = None;
        }
        why
    }

    /// TCI's own `keyer:<rx>,<true|false>[,<ms>];` from another TCI
    /// program: down as it arrives, up `ms` after it went down (or as it
    /// arrives, with no `ms`), played KEYER_DELAY_MS later.
    pub fn keyer(&mut self, cmd: &Command, mode: Mode, share: &RemoteShare, tx: &TxLink) -> Option<String> {
        let down = matches!(cmd.args.get(1).map(String::as_str), Some("true") | Some("1"));
        let now_ms = self.epoch.get_or_insert_with(Instant::now).elapsed().as_millis() as u64;
        let time_ms = if down {
            self.keyer_down_ms = Some(now_ms);
            now_ms
        } else {
            let held: u64 = cmd.arg(2).unwrap_or(0);
            match self.keyer_down_ms.take() {
                Some(at) if held > 0 => at + held,
                _ => now_ms,
            }
        };
        self.key(time_ms, down, KEYER_DELAY_MS, mode, share, tx)
    }

    /// TCI's CW reports due to this client: the speed when it changes (and
    /// at first), cw_macros_empty when sending ends, and -- to one that
    /// said hpsdr_hello -- the saved messages when they change.
    pub fn cw_messages(&mut self, share: &RemoteShare) -> Vec<String> {
        let mut out = Vec::new();
        let info = share.tx();
        if !info.available {
            return out;
        }
        if self.cw_speed_sent != Some(info.cw_speed_wpm) {
            self.cw_speed_sent = Some(info.cw_speed_wpm);
            out.push(cw::speed(info.cw_speed_wpm).to_string());
        }
        if self.cw_was_sending && !info.cw_sending {
            out.push(cw::empty().to_string());
        }
        self.cw_was_sending = info.cw_sending;
        if self.hello {
            let m = share.cw_messages.lock().unwrap();
            if m.0 > self.cw_messages_sent {
                self.cw_messages_sent = m.0;
                out.push(cw::messages(&m.1).to_string());
            }
            let k = share.paddle_keys.lock().unwrap();
            if k.0 > self.paddle_keys_sent {
                self.paddle_keys_sent = k.0;
                out.push(cw::paddle_keys(k.1.0, k.1.1).to_string());
            }
        }
        out
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

    /// A transmitter for TxLink to point at.
    #[derive(Default)]
    struct Tx {
        mox: Arc<AtomicBool>,
        tx_frequency_hz: AtomicU32,
        allow_out_of_band: AtomicBool,
        wants_mic: AtomicBool,
        audio: Mutex<VecDeque<f32>>,
        cw: Arc<crate::radio::CwKeyerAtomics>,
    }

    impl Tx {
        fn link(&self) -> TxLink<'_> {
            TxLink {
                mox: &self.mox,
                tx_frequency_hz: &self.tx_frequency_hz,
                allow_out_of_band: &self.allow_out_of_band,
                wants_mic: &self.wants_mic,
                audio: &self.audio,
                cw: &self.cw,
            }
        }
    }

    fn handle(r: &mut RemoteStreams, text: &str) -> Option<String> {
        let tx = Tx::default();
        r.handle(&parse(text)[0], &Mutex::new(DemodParams::default()), &RemoteShare::default(), &tx.link())
    }

    /// CW text (TCI's cw_macros): queued for main.rs only where it can be
    /// sent, with the reason to an hpsdr-remote client where it can't, and
    /// the speed, end of sending and saved messages reported back.
    #[test]
    fn cw_text() {
        let tx = Tx::default();
        tx.allow_out_of_band.store(true, Ordering::Relaxed);
        let share = RemoteShare::default();
        let mut r = RemoteStreams::default();
        let send = |r: &mut RemoteStreams, text: &str, mode: Mode| r.cw_command(&parse(text)[0], mode, &share, &tx.link());

        // A plain TCI client is told nothing; nothing is queued.
        assert_eq!(send(&mut r, "cw_macros:0,CQ;", Mode::Cwu), None);
        assert_eq!(share.take_cw(), CwRequests::default());
        assert!(r.cw_messages(&share).is_empty(), "no CW reports from a radio that can't transmit");

        let info = TxInfo { available: true, remote_allowed: false, cw_speed_wpm: 22, ..Default::default() };
        share.set_tx(info);
        // remote_allowed is the web client's setting: plain TCI isn't held by it.
        assert_eq!(send(&mut r, "cw_macros:0,CQ DE G0ORX~ K;", Mode::Cwu), None);
        assert_eq!(share.take_cw().text, vec!["CQ DE G0ORX, K".to_string()]);
        r.hello = true;
        assert!(send(&mut r, "cw_macros:0,CQ;", Mode::Cwu).unwrap().contains("turned off at the radio"));
        share.set_tx(TxInfo { remote_allowed: true, ..info });
        assert!(send(&mut r, "cw_macros:0,CQ;", Mode::Usb).unwrap().contains("Choose CW"));
        tx.allow_out_of_band.store(false, Ordering::Relaxed);
        tx.tx_frequency_hz.store(15_000_000, Ordering::Relaxed);
        assert!(send(&mut r, "cw_macros:0,CQ;", Mode::Cwu).unwrap().contains("Out of band"));
        tx.tx_frequency_hz.store(14_030_000, Ordering::Relaxed);
        assert_eq!(send(&mut r, "cw_macros:0,TEST;", Mode::Cwl), None);
        assert_eq!(send(&mut r, "cw_macros_speed:99;", Mode::Cwl), None);
        assert_eq!(send(&mut r, "cw_macros_stop;", Mode::Cwl), None);
        assert_eq!(
            share.take_cw(),
            CwRequests { text: vec!["TEST".to_string()], stop: true, speed_wpm: Some(cw::SPEED_MAX) }
        );

        // Reports: the speed at first and on change, the end of sending,
        // and the saved messages when they change.
        share.set_cw_messages(&["CQ TEST".to_string(), String::new()]);
        share.set_paddle_keys(('[', ']'));
        assert_eq!(
            r.cw_messages(&share),
            vec!["cw_macros_speed:22;", "hpsdr_cw_messages:CQ TEST,;", "hpsdr_paddle_keys:[,];"]
        );
        assert!(r.cw_messages(&share).is_empty());
        share.set_paddle_keys(('[', ']'));
        assert!(r.cw_messages(&share).is_empty());
        share.set_paddle_keys((',', '.'));
        assert_eq!(r.cw_messages(&share), vec!["hpsdr_paddle_keys:~,.;"]);
        share.set_tx(TxInfo { cw_sending: true, cw_speed_wpm: 25, ..share.tx() });
        assert_eq!(r.cw_messages(&share), vec!["cw_macros_speed:25;"]);
        share.set_tx(TxInfo { cw_sending: false, ..share.tx() });
        share.set_cw_messages(&["CQ TEST".to_string(), String::new()]);
        assert_eq!(r.cw_messages(&share), vec!["cw_macros_empty:0;"]);
    }

    /// Live keying: the first key-down of a burst keys the radio and is
    /// scheduled the delay ahead on the transmitter's clock; later
    /// transitions keep their spacing from it on the client's clock (or go
    /// at once if they arrive too late); the burst ends after the hang
    /// time, and at once if the client goes quiet or goes away.
    #[test]
    fn live_keying() {
        let tx = Tx::default();
        tx.allow_out_of_band.store(true, Ordering::Relaxed);
        let live = &tx.cw.live;
        live.rate.store(48_000, Ordering::Relaxed);
        live.clock.store(1_000, Ordering::Relaxed);
        tx.cw.hang_time_ms.store(100, Ordering::Relaxed);
        let share = RemoteShare::default();
        share.set_tx(TxInfo { available: true, remote_allowed: true, ..Default::default() });
        let mut r = RemoteStreams::default();
        r.hello = true;
        let key = |r: &mut RemoteStreams, t: u64, down: bool, mode: Mode| r.key(t, down, 150, mode, &share, &tx.link());

        assert!(key(&mut r, 10_000, true, Mode::Usb).unwrap().contains("Choose CW"));
        assert!(key(&mut r, 10_050, true, Mode::Cwu).is_none(), "rest of a refused burst ignored");
        assert!(key(&mut r, 10_060, false, Mode::Cwu).is_none());
        assert!(!tx.mox.load(Ordering::Relaxed));

        assert!(key(&mut r, 20_000, true, Mode::Cwu).is_none());
        assert!(tx.mox.load(Ordering::Relaxed) && live.active.load(Ordering::Relaxed));
        assert!(key(&mut r, 20_000, true, Mode::Cwu).is_none(), "a repeat is just a keep-alive");
        assert!(key(&mut r, 20_060, false, Mode::Cwu).is_none());
        assert!(key(&mut r, 20_120, true, Mode::Cwu).is_none());
        let start = 1_000 + 150 * 48;
        let expect = vec![(start, true), (start + 60 * 48, false), (start + 120 * 48, true)];
        assert_eq!(live.events.lock().unwrap().iter().copied().collect::<Vec<_>>(), expect);
        // Late: the transmitter has already passed where it belonged.
        live.events.lock().unwrap().clear();
        live.clock.store(start + 400 * 48, Ordering::Relaxed);
        assert!(key(&mut r, 20_180, false, Mode::Cwu).is_none());
        assert_eq!(live.events.lock().unwrap().front(), Some(&(start + 400 * 48, false)));

        // Held for the hang time (+ the ramp) after the last release.
        assert!(r.key_tick(&share).is_none());
        assert!(tx.mox.load(Ordering::Relaxed));
        live.clock.store(start + 400 * 48 + 110 * 48, Ordering::Relaxed);
        assert!(r.key_tick(&share).is_none());
        assert!(!tx.mox.load(Ordering::Relaxed) && !live.active.load(Ordering::Relaxed), "burst over");

        // Quiet with the key down: released and unkeyed.
        assert!(key(&mut r, 30_000, true, Mode::Cwu).is_none());
        r.live.as_mut().unwrap().last_heard = Instant::now() - Duration::from_secs(2);
        assert!(r.key_tick(&share).unwrap().contains("key released"));
        assert!(!tx.mox.load(Ordering::Relaxed));
        assert_eq!(live.events.lock().unwrap().back().map(|e| e.1), Some(false));

        // Gone with the key down: the same.
        assert!(key(&mut r, 40_000, true, Mode::Cwu).is_none());
        assert!(tx.mox.load(Ordering::Relaxed));
        drop(r);
        assert!(!tx.mox.load(Ordering::Relaxed) && !live.active.load(Ordering::Relaxed));
    }

    /// Keying: refused (with the reason) where the radio wouldn't key;
    /// otherwise TX audio goes into the transmitter's queue, and the
    /// transmission ends if that audio stops or the client goes.
    #[test]
    fn transmitting() {
        let tx = Tx::default();
        tx.allow_out_of_band.store(true, Ordering::Relaxed);
        let share = RemoteShare::default();
        let params = Mutex::new(DemodParams::default());
        let mut r = RemoteStreams::default();
        let mut send = |r: &mut RemoteStreams, text: &str| r.handle(&parse(text)[0], &params, &share, &tx.link());
        assert!(send(&mut r, "hpsdr_hello:1;").unwrap().contains(",tx,cw,key;"));

        assert_eq!(send(&mut r, "hpsdr_mox:true;").as_deref(), Some("hpsdr_notice:This radio can't transmit;"));
        let allowed = TxInfo {
            available: true,
            remote_allowed: true,
            power_w: 50,
            max_power_w: 100,
            forward_w: 0.0,
            swr: 1.0,
            ..Default::default()
        };
        share.set_tx(TxInfo { remote_allowed: false, ..allowed });
        assert!(send(&mut r, "hpsdr_mox:true;").unwrap().contains("turned off at the radio"));
        assert!(r.tx_message(&share, &tx.mox).unwrap().starts_with("hpsdr_tx:false,"), "shown as unavailable");
        share.set_tx(allowed);
        params.lock().unwrap().mode = Mode::Cwu;
        assert!(send(&mut r, "hpsdr_mox:true;").unwrap().contains("send text with the CW controls"));
        assert!(!tx.mox.load(Ordering::Relaxed));
        assert_eq!(r.tx_message(&share, &tx.mox).as_deref(), Some("hpsdr_tx:true,false,50,100,0.0,1.00;"));

        params.lock().unwrap().mode = Mode::Usb;
        tx.wants_mic.store(true, Ordering::Relaxed);
        assert!(send(&mut r, "hpsdr_mox:true;").is_none());
        assert!(tx.mox.load(Ordering::Relaxed) && !tx.wants_mic.load(Ordering::Relaxed));
        assert_eq!(tx.audio.lock().unwrap().len(), TX_PRIME);
        assert!(r.tx_message(&share, &tx.mox).unwrap().starts_with("hpsdr_tx:true,true,"), "sent at once on keying");

        // 20 ms at 16 kHz becomes 20 ms at 48 kHz.
        let block = adpcm::Encoder::new().encode(&[1000; 320]);
        let header = Header {
            sample_rate: 16_000,
            format: FORMAT_ADPCM,
            codec: CODEC_IMA_ADPCM,
            length: 320,
            msg_type: TYPE_ADPCM_AUDIO,
            channels: 1,
            ..Default::default()
        };
        let mut msg = Vec::new();
        header.encode(&mut msg);
        msg.extend(block);
        r.tx_audio(&msg, &tx.link());
        let queued = tx.audio.lock().unwrap().len() - TX_PRIME;
        assert!((958..=962).contains(&queued), "{queued}");

        // Kept from running dry while the audio arrives...
        tx.audio.lock().unwrap().clear();
        assert!(r.tx_tick(&share, &tx.link()).is_none());
        assert_eq!(tx.audio.lock().unwrap().len(), TX_LOW);
        // ...and unkeyed once it has stopped for a second.
        r.keyed.as_mut().unwrap().last_audio = Instant::now() - Duration::from_secs(2);
        assert!(r.tx_tick(&share, &tx.link()).is_some());
        assert!(!tx.mox.load(Ordering::Relaxed));

        // Turning remote transmit off at the radio unkeys at once.
        assert!(send(&mut r, "hpsdr_mox:true;").is_none());
        share.set_tx(TxInfo { remote_allowed: false, ..allowed });
        assert!(r.tx_tick(&share, &tx.link()).is_some());
        assert!(!tx.mox.load(Ordering::Relaxed));
        share.set_tx(allowed);

        // A client that goes while keyed unkeys.
        assert!(send(&mut r, "hpsdr_mox:true;").is_none());
        drop(r);
        assert!(!tx.mox.load(Ordering::Relaxed));

        // A power setting waits for main.rs.
        let mut r = RemoteStreams::default();
        send(&mut r, "hpsdr_set:power,30;");
        assert_eq!(share.take_power_request(), Some(30));
    }

    /// The band list goes once after hello and again when it changes; a
    /// band selection waits for main.rs.
    #[test]
    fn bands_are_sent_and_selected() {
        let mut r = RemoteStreams::default();
        let share = RemoteShare::default();
        let params = Mutex::new(DemodParams::default());
        share.set(vec![BandInfo { name: "40m".into(), low_hz: 7_000_000, high_hz: 7_300_000 }]);
        assert!(r.bands_message(&share).is_none(), "not before hello");
        let tx = Tx::default();
        r.handle(&parse("hpsdr_hello:1;")[0], &params, &share, &tx.link());
        assert_eq!(r.bands_message(&share).as_deref(), Some("hpsdr_bands:40m,7000000,7300000;"));
        assert!(r.bands_message(&share).is_none(), "already sent");
        share.set(vec![BandInfo { name: "40m".into(), low_hz: 7_000_000, high_hz: 7_300_000 }]);
        assert!(r.bands_message(&share).is_none(), "the same list isn't new");
        share.set(vec![]);
        assert_eq!(r.bands_message(&share).as_deref(), Some("hpsdr_bands;"));
        r.handle(&parse("hpsdr_band:QO-100;")[0], &params, &share, &tx.link());
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
        let tx = Tx::default();
        r.handle(&parse("hpsdr_hello:1;")[0], &params, &RemoteShare::default(), &tx.link());
        let first = r.state_message(&params.lock().unwrap()).unwrap();
        assert!(first.starts_with("hpsdr_state:"));
        assert!(r.state_message(&params.lock().unwrap()).is_none(), "unchanged, under a second");
        for text in ["hpsdr_set:width,1800;", "hpsdr_set:agc,slow;", "hpsdr_set:agc_gain,70;", "hpsdr_set:nb,nb2;",
                     "hpsdr_set:nr,nr3;", "hpsdr_set:anf,true;", "hpsdr_set:snb,true;", "hpsdr_set:nr,bogus;"] {
            r.handle(&parse(text)[0], &params, &RemoteShare::default(), &tx.link());
        }
        let state = r.state_message(&params.lock().unwrap()).unwrap();
        assert_eq!(state, "hpsdr_state:1800,slow,70,nb2,nr3,true,true;");
    }

    #[test]
    fn hello_and_stream_commands() {
        let mut r = RemoteStreams::default();
        assert_eq!(handle(&mut r, "hpsdr_hello:1;").as_deref(), Some("hpsdr_hello:1,spectrum,adpcm,tx,cw,key;"));
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
