/*
    hpsdr-remote's window: connect to an hpsdr-rs TCI server, then its VFO,
    band and mode buttons, S-meter and audio gain, and the spectrum and
    waterfall it streams -- click or scroll them to tune -- and transmit:
    MOX, the microphone, power and the power/SWR meter, and in CW, text
    sent as CW and an on-screen key or paddles (docs/remote-protocol.md).
*/

use std::collections::{HashMap, VecDeque};

use egui::{Color32, ColorImage, Pos2, Rect, Stroke, TextureHandle, TextureOptions, Vec2};
use hpsdr_proto::bands::{self, BandInfo};
use hpsdr_proto::cw;
use hpsdr_proto::spectrum::SpectrumFrame;
use hpsdr_proto::state::{self as dsp, DspState};
use hpsdr_proto::tci::{Command, AUDIO, FEATURE_CW, FEATURE_KEY, FEATURE_TX, HELLO, NOTICE, SPECTRUM};
use hpsdr_proto::tx::{self, TxState, TX_AUDIO_RATE};
use hpsdr_proto::{
    adpcm, Header, CODEC_IMA_ADPCM, FORMAT_ADPCM, PROTOCOL_VERSION, TYPE_ADPCM_AUDIO, TYPE_SPECTRUM,
};

use crate::audio::Player;
use crate::keyer::{self, Keyer};
use crate::link::{Incoming, Link, Status};
use crate::mic::Mic;

/// What's asked of the server: points a row, rows a second, audio rate.
const SPECTRUM_POINTS: usize = 1024;
const SPECTRUM_FPS: u32 = 15;
const AUDIO_RATE: u32 = 24_000;
const WATERFALL_ROWS: usize = 200;

/// Name, edges and first-visit frequency, Hz -- only for a server too old
/// to send its own band buttons (hpsdr_bands).
const FALLBACK_BANDS: [(&str, u64, u64, u64); 14] = [
    ("160m", 1_800_000, 2_000_000, 1_900_000),
    ("80m", 3_500_000, 4_000_000, 3_700_000),
    ("60m", 5_250_000, 5_450_000, 5_357_000),
    ("40m", 7_000_000, 7_300_000, 7_100_000),
    ("30m", 10_100_000, 10_150_000, 10_120_000),
    ("20m", 14_000_000, 14_350_000, 14_200_000),
    ("17m", 18_068_000, 18_168_000, 18_120_000),
    ("15m", 21_000_000, 21_450_000, 21_200_000),
    ("12m", 24_890_000, 24_990_000, 24_940_000),
    ("10m", 28_000_000, 29_700_000, 28_500_000),
    ("6m", 50_000_000, 54_000_000, 50_150_000),
    ("2m", 144_000_000, 148_000_000, 144_300_000),
    ("70cm", 420_000_000, 450_000_000, 432_200_000),
    ("QO-100", 10_489_500_000, 10_490_000_000, 10_489_750_000),
];

/// TCI mode names and their buttons.
const MODES: [(&str, &str); 9] = [
    ("lsb", "LSB"),
    ("usb", "USB"),
    ("cwl", "CWL"),
    ("cwu", "CWU"),
    ("am", "AM"),
    ("sam", "SAM"),
    ("nfm", "FM"),
    ("digu", "DIGU"),
    ("digl", "DIGL"),
];

#[derive(Default)]
struct Radio {
    vfo_hz: u64,
    mode: String,
    /// Receive filter edges, Hz from the carrier, as the server last said
    /// (rx_filter_band) -- for the mode it said it for.
    filter: Option<(String, i64, i64)>,
    mox: bool,
    device: String,
    /// The server's hpsdr_hello reply: None until it answers.
    extensions: Option<Vec<String>>,
    /// Its DSP settings (hpsdr_state): None until the first report.
    dsp: Option<DspState>,
    /// Its band buttons (hpsdr_bands): None from a server without them.
    bands: Option<Vec<BandInfo>>,
    /// Its transmitter (hpsdr_tx): None until the first report.
    tx: Option<TxState>,
    /// Its saved CW messages (hpsdr_cw_messages).
    cw_messages: Vec<String>,
    /// The keys a paddle plugged in as a keyboard types (dit, dah), as
    /// set at the radio (hpsdr_paddle_keys); None until it says.
    paddle_keys: Option<(char, char)>,
}

impl Radio {
    /// The server can take this client's transmissions, and the radio
    /// can transmit.
    fn can_transmit(&self) -> bool {
        self.extensions.as_ref().is_some_and(|e| e.iter().any(|f| f == FEATURE_TX))
            && self.tx.is_some_and(|t| t.available)
    }

    /// In CW, with a server that sends text as CW: the CW controls take
    /// MOX's place.
    fn sends_cw(&self) -> bool {
        self.is_cw()
            && self.can_transmit()
            && self.extensions.as_ref().is_some_and(|e| e.iter().any(|f| f == FEATURE_CW))
    }

    /// In CW, with a server that takes live keying: the key or paddles too.
    fn keys_cw(&self) -> bool {
        self.sends_cw() && self.extensions.as_ref().is_some_and(|e| e.iter().any(|f| f == FEATURE_KEY))
    }

    /// CW, either sideband (or plain "cw", from an older hpsdr-rs).
    fn is_cw(&self) -> bool {
        matches!(self.mode.as_str(), "cw" | "cwl" | "cwu")
    }

    /// The paddle keys (dit, dah): the radio's, or `[` and `]`.
    fn paddle_keys(&self) -> (char, char) {
        self.paddle_keys.unwrap_or((cw::PADDLE_DIT_DEFAULT, cw::PADDLE_DAH_DEFAULT))
    }
}

impl Radio {
    /// The passband to shade: the server's filter if it's for this mode,
    /// otherwise the mode's usual one.
    /// From the filter width once the server has reported it (the same
    /// shapes as hpsdr-rs's spectrum::passband_for; CW around a 600 Hz
    /// pitch, which TCI doesn't report), before that its rx_filter_band.
    fn passband(&self) -> (i64, i64) {
        if let Some(d) = &self.dsp {
            let w = d.width_hz.max(50) as i64;
            let low_cut = 150.min(w / 10);
            return match self.mode.as_str() {
                "lsb" => (-w, -low_cut),
                "digl" => (-w, 0),
                "digu" => (0, w),
                "cwl" => (-600 - w / 2, -600 + w / 2),
                "cw" | "cwu" => (600 - w / 2, 600 + w / 2),
                "am" | "sam" | "nfm" | "dsb" => (-w, w),
                _ => (low_cut, w),
            };
        }
        match &self.filter {
            Some((mode, low, high)) if *mode == self.mode => (*low, *high),
            _ => match self.mode.as_str() {
                "lsb" | "digl" => (-2700, -150),
                "cwl" => (-850, -350),
                "cw" | "cwu" => (350, 850),
                "am" | "sam" | "nfm" => (-4000, 4000),
                _ => (150, 2700),
            },
        }
    }
}

/// The groups of controls a narrow screen shows one at a time.
#[derive(Clone, Copy, PartialEq)]
enum Section {
    Step,
    Band,
    Mode,
    Filter,
    Noise,
    Audio,
    Key,
    Tx,
}

impl Section {
    fn label(self) -> &'static str {
        match self {
            Section::Step => "Step",
            Section::Band => "Band",
            Section::Mode => "Mode",
            Section::Filter => "Filter",
            Section::Noise => "Noise",
            Section::Audio => "Audio",
            Section::Key => "Key",
            Section::Tx => "TX",
        }
    }
}

/// Narrower than this (points) -- a phone held upright -- or shorter than
/// SHORT_HEIGHT -- one held sideways -- the controls show a section at a
/// time.
const NARROW_WIDTH: f32 = 600.0;
const SHORT_HEIGHT: f32 = 500.0;
/// The controls take at most this share of the screen's height; beyond
/// it they scroll, so the spectrum always keeps the rest.
const CONTROLS_MAX_HEIGHT: f32 = 0.55;

struct Waterfall {
    rows: VecDeque<Vec<Color32>>,
    texture: Option<TextureHandle>,
    dirty: bool,
}

pub struct App {
    url: String,
    link: Option<Link>,
    radio: Radio,
    player: Option<Player>,
    audio_error: Option<String>,
    spectrum: Option<SpectrumFrame>,
    waterfall: Waterfall,
    af_gain_db: f32,
    muted: bool,
    db_low: f32,
    db_range: f32,
    band_memory: HashMap<&'static str, u64>,
    /// Data rates for the status line: bytes in and out at the last
    /// sample, when, and the rates in and out (kbit/s).
    rate_sample: (u64, u64, f64, f64, f64),
    /// The step for the frequency display's slide, the - and + buttons,
    /// and a plain scroll.
    step_hz: u64,
    /// Slide distance not yet turned into a whole step (frequency
    /// display, px) or a whole Hz (spectrum/waterfall).
    vfo_slide_px: f32,
    span_slide_hz: f64,
    /// egui time now, when the client last tuned, and when it last sent:
    /// sends are spaced while sliding (a final one always goes), and the
    /// server's own frequency reports are ignored just after a local
    /// change, so one already on its way doesn't snap the display back.
    now: f64,
    tuned_at: f64,
    sent_at: f64,
    unsent: bool,
    /// When a DSP setting was last changed here -- the server's
    /// hpsdr_state is ignored briefly after, as with tuning.
    dsp_set_at: f64,
    /// Sent with the connection, if the server asks for one.
    password: String,
    /// Transmitting: the microphone (opened on the first MOX press) or why
    /// it couldn't be; its gain and recent level; whether this client
    /// keyed (it sends audio only then), and when MOX was last pressed --
    /// the server's state is ignored just after, as with tuning; the TX
    /// audio encoder and samples not yet a whole block; the power setting
    /// shown, and when it was last changed here.
    mic: Option<Mic>,
    mic_error: Option<String>,
    mic_gain_db: f32,
    mic_level: f32,
    keyed_here: bool,
    mox_pressed_at: f64,
    tx_encoder: adpcm::Encoder,
    tx_pending: Vec<i16>,
    power_w: u32,
    power_set_at: f64,
    /// CW: the text being typed, and the speed (WPM) with when it was last
    /// changed here.
    cw_text: String,
    cw_speed_wpm: u32,
    cw_speed_set_at: f64,
    /// Live keying: the keyer, the playout delay asked of the server (ms),
    /// the sidetone level; the pads pressed this frame (None if not shown);
    /// the key state last sent and when, for the repeats while it's down;
    /// and the fingers on the screen (for squeezing both paddles).
    keyer: Keyer,
    key_delay_ms: u32,
    sidetone_level: f32,
    pads: Option<(bool, bool)>,
    /// A key or paddle plugged in as a keyboard (a microcontroller that
    /// types the radio's paddle keys): dit and dah held now.
    paddle_keys: (bool, bool),
    key_sent: (bool, f64),
    touches: HashMap<u64, Pos2>,
    /// The server's last hpsdr_notice, and when it came.
    notice: Option<(String, f64)>,
    /// The section of controls open on a narrow screen, if any.
    section: Option<Section>,
}

/// TX audio goes in blocks this long (20 ms).
const TX_BLOCK: usize = TX_AUDIO_RATE as usize / 50;
/// How long a notice stays in the status line.
const NOTICE_S: f64 = 8.0;

/// Slide this far on the frequency display for one step.
const VFO_SLIDE_PX_PER_STEP: f32 = 12.0;
/// At most this often while sliding.
const TUNE_SEND_INTERVAL_S: f64 = 0.05;
/// Ignore the server's frequency for this long after tuning here.
const TUNE_HOLD_S: f64 = 1.5;
const STEPS_HZ: [(u64, &str); 4] = [(10, "10 Hz"), (100, "100 Hz"), (1_000, "1 kHz"), (10_000, "10 kHz")];

impl App {
    pub fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        Self {
            url: default_url(),
            link: None,
            radio: Radio::default(),
            player: None,
            audio_error: None,
            spectrum: None,
            waterfall: Waterfall { rows: VecDeque::new(), texture: None, dirty: false },
            af_gain_db: 0.0,
            muted: false,
            db_low: -130.0,
            db_range: 60.0,
            band_memory: HashMap::new(),
            rate_sample: (0, 0, 0.0, 0.0, 0.0),
            step_hz: 1_000,
            vfo_slide_px: 0.0,
            span_slide_hz: 0.0,
            now: 0.0,
            tuned_at: f64::NEG_INFINITY,
            sent_at: f64::NEG_INFINITY,
            unsent: false,
            dsp_set_at: f64::NEG_INFINITY,
            password: String::new(),
            mic: None,
            mic_error: None,
            mic_gain_db: 0.0,
            mic_level: 0.0,
            keyed_here: false,
            mox_pressed_at: f64::NEG_INFINITY,
            tx_encoder: adpcm::Encoder::new(),
            tx_pending: Vec::new(),
            power_w: 0,
            power_set_at: f64::NEG_INFINITY,
            cw_text: String::new(),
            cw_speed_wpm: 20,
            cw_speed_set_at: f64::NEG_INFINITY,
            keyer: Keyer::new(keyer::Mode::IambicA),
            key_delay_ms: cw::KEY_DELAY_DEFAULT_MS,
            sidetone_level: 0.3,
            pads: None,
            paddle_keys: (false, false),
            key_sent: (false, f64::NEG_INFINITY),
            touches: HashMap::new(),
            notice: None,
            section: Some(Section::Band),
        }
    }

    fn connect(&mut self, ctx: &egui::Context) {
        self.radio = Radio::default();
        self.spectrum = None;
        self.waterfall.rows.clear();
        self.waterfall.dirty = true;
        self.keyed_here = false;
        self.notice = None;
        self.link = match Link::connect(&with_password(&self.url, &self.password), ctx.clone()) {
            Ok(link) => Some(link),
            Err(e) => {
                self.radio.device = format!("couldn't connect: {e}");
                None
            }
        };
        // Opened here, on the click: browsers only start audio after one.
        if self.player.is_none() {
            match Player::open() {
                Ok(p) => self.player = Some(p),
                Err(e) => self.audio_error = Some(e),
            }
        }
    }

    fn send(&mut self, cmd: Command) {
        if let Some(link) = &mut self.link {
            link.send(cmd);
        }
    }

    /// Tunes here at once; tells the server now, or -- while sliding --
    /// at most every TUNE_SEND_INTERVAL_S (flush_tune sends the last one).
    fn tune(&mut self, hz: u64) {
        self.radio.vfo_hz = hz;
        self.tuned_at = self.now;
        self.unsent = true;
        self.flush_tune();
    }

    fn flush_tune(&mut self) {
        if self.unsent && self.now - self.sent_at >= TUNE_SEND_INTERVAL_S {
            let hz = self.radio.vfo_hz.to_string();
            self.send(Command::new("vfo", &["0", "0", &hz]));
            self.sent_at = self.now;
            self.unsent = false;
        }
    }

    /// Moves by `steps` of `step_hz`, onto a multiple of it.
    fn step(&mut self, steps: i64, step_hz: u64) {
        if steps != 0 && self.radio.vfo_hz > 0 {
            let step = step_hz as i64;
            let hz = ((self.radio.vfo_hz as i64 / step + steps) * step).max(0) as u64;
            self.tune(hz);
        }
    }

    fn handle(&mut self, incoming: Incoming) {
        match incoming {
            Incoming::Opened => {
                self.send(Command::new(HELLO, &[&PROTOCOL_VERSION.to_string()]));
                self.send(Command::new(SPECTRUM, &[&SPECTRUM_POINTS.to_string(), &SPECTRUM_FPS.to_string()]));
                self.send(Command::new(AUDIO, &["adpcm", &AUDIO_RATE.to_string()]));
            }
            // Receiver 0 only: hpsdr-rs reports its second receiver (an
            // extra receiver) as receiver 1, as Thetis does its RX2.
            Incoming::Command(cmd)
                if matches!(cmd.name.as_str(), "vfo" | "modulation" | "rx_filter_band")
                    && cmd.arg::<u32>(0) != Some(0) => {}
            Incoming::Command(cmd) => match cmd.name.as_str() {
                "vfo" if cmd.arg::<u32>(1) == Some(0) => {
                    // Not just after tuning here -- see tuned_at.
                    if let Some(hz) = cmd.arg::<u64>(2).filter(|_| self.now - self.tuned_at >= TUNE_HOLD_S) {
                        self.radio.vfo_hz = hz;
                    }
                }
                "modulation" => {
                    if let Some(m) = cmd.args.get(1) {
                        self.radio.mode = m.to_ascii_lowercase();
                    }
                }
                "rx_filter_band" => {
                    if let (Some(low), Some(high)) = (cmd.arg::<i64>(1), cmd.arg::<i64>(2)) {
                        self.radio.filter = Some((self.radio.mode.clone(), low, high));
                    }
                }
                "trx" => self.server_mox(cmd.args.get(1).is_some_and(|v| v == "true")),
                tx::TX => {
                    if let Some(state) = TxState::from_command(&cmd) {
                        self.server_mox(state.mox);
                        if self.now - self.power_set_at >= TUNE_HOLD_S {
                            self.power_w = state.power_w;
                        }
                        self.radio.tx = Some(state);
                    }
                }
                NOTICE => {
                    // Just after pressing MOX, a notice is the server
                    // saying why it didn't key: show it unkeyed at once.
                    if self.keyed_here && self.now - self.mox_pressed_at < TUNE_HOLD_S {
                        self.keyed_here = false;
                        self.radio.mox = false;
                        self.mox_pressed_at = f64::NEG_INFINITY;
                    }
                    self.notice = Some((cmd.args.join(", "), self.now));
                }
                cw::MACROS_SPEED => {
                    if let Some(wpm) = cmd.arg::<u32>(0).filter(|_| self.now - self.cw_speed_set_at >= TUNE_HOLD_S) {
                        self.cw_speed_wpm = wpm;
                    }
                }
                cw::MESSAGES => self.radio.cw_messages = cw::messages_from(&cmd).unwrap_or_default(),
                cw::PADDLE_KEYS => {
                    let keys = cw::paddle_keys_from(&cmd);
                    if keys.is_some() && keys != self.radio.paddle_keys {
                        self.radio.paddle_keys = keys;
                        self.paddle_keys = (false, false);
                    }
                }
                "device" => self.radio.device = cmd.args.first().cloned().unwrap_or_default(),
                HELLO => self.radio.extensions = Some(cmd.args.iter().skip(1).cloned().collect()),
                bands::BANDS => self.radio.bands = bands::from_command(&cmd),
                dsp::STATE => {
                    if self.now - self.dsp_set_at >= TUNE_HOLD_S {
                        if let Some(state) = DspState::from_command(&cmd) {
                            self.radio.dsp = Some(state);
                        }
                    }
                }
                _ => {}
            },
            Incoming::Binary(data) => {
                let Some((header, payload)) = Header::decode(&data) else { return };
                match header.msg_type {
                    TYPE_SPECTRUM => {
                        if let Some(frame) = SpectrumFrame::decode(&header, payload) {
                            self.add_waterfall_row(&frame);
                            self.spectrum = Some(frame);
                        }
                    }
                    TYPE_ADPCM_AUDIO => {
                        if let (Some(player), Some(samples)) =
                            (&self.player, adpcm::decode(payload, header.length as usize))
                        {
                            player.push(&samples, header.sample_rate);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    /// The server says the radio is (or isn't) transmitting. Not just after
    /// MOX was pressed here, when a report sent before the press can still
    /// arrive; after that, unkeyed there means stop sending audio here.
    fn server_mox(&mut self, on: bool) {
        if self.now - self.mox_pressed_at < TUNE_HOLD_S {
            return;
        }
        self.radio.mox = on;
        if !on {
            self.keyed_here = false;
        }
    }

    /// MOX pressed: key (opening the microphone the first time -- in the
    /// click, as browsers want) or unkey.
    fn set_mox(&mut self, on: bool) {
        if on && self.mic.is_none() {
            match Mic::open() {
                Ok(m) => {
                    self.mic = Some(m);
                    self.mic_error = None;
                }
                Err(e) => {
                    self.mic_error = Some(e);
                    return;
                }
            }
        }
        if on {
            // Start fresh: nothing captured before the press.
            if let Some(m) = &self.mic {
                m.take();
            }
            self.tx_encoder = adpcm::Encoder::new();
            self.tx_pending.clear();
        }
        self.keyed_here = on;
        self.radio.mox = on;
        self.mox_pressed_at = self.now;
        self.send(tx::mox(on));
    }

    /// Sends what the microphone has captured, in 20 ms ADPCM blocks,
    /// while this client has the radio keyed; otherwise just keeps it
    /// drained, and the level meter moving.
    fn send_tx_audio(&mut self) {
        let Some(mic) = &self.mic else { return };
        if let Some(e) = mic.error() {
            self.mic_error = Some(e);
        }
        let samples = mic.take();
        let gain = 10f32.powf(self.mic_gain_db / 20.0);
        self.mic_level = (self.mic_level * 0.8).max(mic.take_peak() * gain);
        if !self.keyed_here {
            return;
        }
        self.tx_pending.extend(samples.iter().map(|&s| (s * gain * 32767.0).clamp(-32767.0, 32767.0) as i16));
        while self.tx_pending.len() >= TX_BLOCK {
            let block: Vec<i16> = self.tx_pending.drain(..TX_BLOCK).collect();
            let header = Header {
                sample_rate: TX_AUDIO_RATE,
                format: FORMAT_ADPCM,
                codec: CODEC_IMA_ADPCM,
                length: TX_BLOCK as u32,
                msg_type: TYPE_ADPCM_AUDIO,
                channels: 1,
                ..Default::default()
            };
            let mut msg = Vec::new();
            header.encode(&mut msg);
            msg.extend(self.tx_encoder.encode(&block));
            if let Some(link) = &mut self.link {
                link.send_binary(msg);
            }
        }
    }

    /// MOX -- shown once the server says this radio can transmit.
    fn mox_button(&mut self, ui: &mut egui::Ui) {
        let cw = self.radio.is_cw();
        let (fill, text) = if self.radio.mox {
            (Color32::from_rgb(200, 30, 30), Color32::WHITE)
        } else {
            (ui.visuals().widgets.inactive.bg_fill, ui.visuals().text_color())
        };
        let mox = ui
            .add_enabled(
                !cw || self.radio.mox,
                egui::Button::new(egui::RichText::new("MOX").strong().size(18.0).color(text))
                    .fill(fill)
                    .min_size(Vec2::new(72.0, 32.0)),
            )
            .on_hover_text("Transmit from this microphone; press again to stop")
            .on_disabled_hover_text("In CW, this server can't send CW from here: choose a voice or data mode to transmit");
        if mox.clicked() {
            self.set_mox(!self.radio.mox);
        }
    }

    /// The microphone's gain and level, the power setting, and the
    /// power/SWR meter while transmitting -- the power on a line of its own
    /// when `narrow`, as audio_controls.
    fn tx_settings(&mut self, ui: &mut egui::Ui, narrow: bool) {
        let tx = self.radio.tx.unwrap_or_default();
        if !self.radio.sends_cw() {
            self.mic_settings(ui, narrow);
        }
        ui.label("Power:");
        let max = tx.max_power_w.max(1);
        if ui.add(egui::Slider::new(&mut self.power_w, 0..=max).suffix(" W")).changed() {
            self.power_set_at = self.now;
            let watts = self.power_w.to_string();
            self.send(Command::new(dsp::SET, &[tx::SET_POWER, &watts]));
        }
        if self.radio.mox {
            ui.add_space(12.0);
            ui.strong(format!("{:.1} W   SWR {:.2}", tx.forward_w, tx.swr));
        }
    }

    /// The microphone's gain and level meter.
    fn mic_settings(&mut self, ui: &mut egui::Ui, narrow: bool) {
        ui.label("Mic:");
        ui.add(egui::Slider::new(&mut self.mic_gain_db, -20.0..=30.0).suffix(" dB"));
        let (rect, _) = ui.allocate_exact_size(Vec2::new(80.0, 10.0), egui::Sense::hover());
        ui.painter().rect_filled(rect, 2.0, Color32::from_gray(40));
        let level = self.mic_level.min(1.0);
        let colour = if level > 0.9 { Color32::from_rgb(230, 60, 60) } else { Color32::from_rgb(60, 200, 60) };
        ui.painter().rect_filled(
            Rect::from_min_size(rect.min, Vec2::new(rect.width() * level, rect.height())),
            2.0,
            colour,
        );
        if narrow {
            ui.end_row();
        } else {
            ui.add_space(12.0);
        }
    }

    /// Sends text as CW (TCI's cw_macros), after anything already going.
    fn send_cw(&mut self, text: &str) {
        if !text.trim().is_empty() {
            self.send(cw::macros(text));
        }
    }

    /// CW: Stop, red while the radio is sending -- in MOX's place.
    fn cw_stop_button(&mut self, ui: &mut egui::Ui) {
        let (fill, text) = if self.radio.mox {
            (Color32::from_rgb(200, 30, 30), Color32::WHITE)
        } else {
            (ui.visuals().widgets.inactive.bg_fill, ui.visuals().text_color())
        };
        let stop = ui
            .add(
                egui::Button::new(egui::RichText::new("Stop").strong().size(18.0).color(text))
                    .fill(fill)
                    .min_size(Vec2::new(72.0, 32.0)),
            )
            .on_hover_text("Stop sending CW at once");
        if stop.clicked() {
            self.send(cw::stop());
        }
    }

    /// CW: a line of text to send (Enter or Send), and Stop unless
    /// `narrow`, where it's already on the line above.
    fn cw_send_row(&mut self, ui: &mut egui::Ui, narrow: bool) {
        let width = if narrow { (ui.available_width() - 80.0).max(120.0) } else { 280.0 };
        let edit = ui.add(
            egui::TextEdit::singleline(&mut self.cw_text).hint_text("Text to send as CW").desired_width(width),
        );
        let entered = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if ui.add(egui::Button::new(egui::RichText::new("Send").strong()).min_size(Vec2::new(60.0, 30.0))).clicked()
            || entered
        {
            let text = std::mem::take(&mut self.cw_text);
            self.send_cw(&text);
            if entered {
                edit.request_focus();
            }
        }
        if !narrow {
            self.cw_stop_button(ui);
        }
    }

    /// Live keying's settings: the keyer's mode, the delay asked of the
    /// server, and the sidetone -- on lines of their own when `narrow`.
    fn key_settings(&mut self, ui: &mut egui::Ui, narrow: bool) {
        for mode in keyer::Mode::ALL {
            if ui.add(egui::Button::selectable(self.keyer.mode == mode, mode.label())).clicked() && self.keyer.mode != mode {
                self.release_key();
                self.keyer.mode = mode;
            }
        }
        if narrow {
            ui.end_row();
        } else {
            ui.add_space(12.0);
        }
        ui.label("Delay:").on_hover_text(
            "How far behind your keying the radio sends, so the network's ups and downs \
             don't change your timing: more for a slower or busier connection",
        );
        ui.add(
            egui::Slider::new(&mut self.key_delay_ms, cw::KEY_DELAY_MIN_MS..=500)
                .suffix(" ms")
                .logarithmic(true),
        );
        if narrow {
            ui.end_row();
        } else {
            ui.add_space(12.0);
        }
        ui.label("Sidetone:");
        ui.add(egui::Slider::new(&mut self.sidetone_level, 0.0..=1.0).show_value(false));
    }

    /// The on-screen key -- one pad -- or paddles -- dit and dah side by
    /// side -- noting what's pressed for run_keyer. Fingers on a touch
    /// screen count (both paddles at once squeezes them); otherwise the
    /// mouse.
    fn key_pads(&mut self, ui: &mut egui::Ui, narrow: bool) {
        let straight = self.keyer.mode == keyer::Mode::Straight;
        let height = if narrow { 90.0 } else { 70.0 };
        let gap = ui.spacing().item_spacing.x;
        let width = if straight {
            if narrow { ui.available_width() } else { 240.0 }
        } else if narrow {
            (ui.available_width() - gap) / 2.0
        } else {
            140.0
        };
        let labels: &[&str] = if straight { &["KEY"] } else { &["dit", "dah"] };
        let mut pressed = [false; 2];
        ui.horizontal(|ui| {
            for (i, label) in labels.iter().enumerate() {
                let (rect, _) = ui.allocate_exact_size(Vec2::new(width, height), egui::Sense::click_and_drag());
                pressed[i] = self.pad_pressed(ui, rect);
                let keyed = if straight {
                    self.paddle_keys.0 || self.paddle_keys.1
                } else if i == 0 {
                    self.paddle_keys.0
                } else {
                    self.paddle_keys.1
                };
                let fill = if pressed[i] || keyed { Color32::from_rgb(200, 30, 30) } else { ui.visuals().widgets.inactive.bg_fill };
                ui.painter().rect_filled(rect, 8.0, fill);
                ui.painter().text(
                    rect.center() - Vec2::new(0.0, 9.0),
                    egui::Align2::CENTER_CENTER,
                    label,
                    egui::FontId::proportional(22.0),
                    ui.visuals().strong_text_color(),
                );
                // The key a paddle plugged in as a keyboard types for it.
                let (dit_key, dah_key) = self.radio.paddle_keys();
                let key_text = if straight {
                    format!("{dit_key} or {dah_key}")
                } else if i == 0 {
                    dit_key.to_string()
                } else {
                    dah_key.to_string()
                };
                ui.painter().text(
                    rect.center() + Vec2::new(0.0, 16.0),
                    egui::Align2::CENTER_CENTER,
                    key_text,
                    egui::FontId::monospace(15.0),
                    ui.visuals().weak_text_color(),
                );
            }
        });
        self.pads = Some((pressed[0], pressed[1]));
    }

    fn pad_pressed(&self, ui: &egui::Ui, rect: Rect) -> bool {
        if !self.touches.is_empty() {
            return self.touches.values().any(|p| rect.contains(*p));
        }
        ui.input(|i| i.pointer.primary_down() && i.pointer.interact_pos().is_some_and(|p| rect.contains(p)))
    }

    /// Keeps track of the fingers on the screen, for the pads.
    fn track_touches(&mut self, ui: &egui::Ui) {
        ui.input(|i| {
            for e in &i.events {
                if let egui::Event::Touch { id, phase, pos, .. } = e {
                    match phase {
                        egui::TouchPhase::Start | egui::TouchPhase::Move => {
                            self.touches.insert(id.0, *pos);
                        }
                        egui::TouchPhase::End | egui::TouchPhase::Cancel => {
                            self.touches.remove(&id.0);
                        }
                    }
                }
            }
        });
    }

    fn can_key(&self) -> bool {
        self.radio.keys_cw() && self.link.as_ref().is_some_and(|l| l.status == Status::Open)
    }

    /// Before the controls are drawn: a key or paddle plugged in as a
    /// keyboard. While it can key, its two keys are taken out of the input
    /// -- they key, rather than typing into the CW text -- and what's held
    /// is noted for run_keyer. The keyboard's own repeats are ignored, and
    /// everything's let go if the page loses focus (no key-up comes then).
    fn take_paddle_keys(&mut self, ctx: &egui::Context) {
        if !self.can_key() {
            self.paddle_keys = (false, false);
            return;
        }
        let mut keys = self.paddle_keys;
        // Matched by where the key is on a US keyboard (what a
        // microcontroller sends), so a phone set to another layout still
        // sees it.
        let (dit_char, dah_char) = self.radio.paddle_keys();
        let dit_key = egui::Key::from_name(&dit_char.to_string());
        let dah_key = egui::Key::from_name(&dah_char.to_string());
        let is_paddle_text = |t: &str| {
            let t = t.to_lowercase();
            t == dit_char.to_string() || t == dah_char.to_string()
        };
        ctx.input_mut(|i| {
            i.events.retain(|e| match e {
                egui::Event::Key { key, physical_key, pressed, .. } => {
                    let k = Some(physical_key.unwrap_or(*key));
                    if k == dit_key {
                        keys.0 = *pressed;
                        false
                    } else if k == dah_key {
                        keys.1 = *pressed;
                        false
                    } else {
                        true
                    }
                }
                egui::Event::Text(t) => !is_paddle_text(t),
                egui::Event::WindowFocused(false) => {
                    keys = (false, false);
                    true
                }
                _ => true,
            })
        });
        self.paddle_keys = keys;
    }

    /// After the controls are drawn: runs the keyer on the pads and a key
    /// or paddle plugged in as a keyboard, and sends its transitions,
    /// repeating a held key so the server knows this client is still there;
    /// and the sidetone.
    fn run_keyer(&mut self, ctx: &egui::Context) {
        let now_ms = self.now * 1000.0;
        if !self.can_key() {
            self.pads = None;
            if self.keyer.is_busy() || self.key_sent.0 {
                self.release_key();
            }
            return;
        }
        let (pad_dit, pad_dah) = self.pads.take().unwrap_or_default();
        let (key_dit, key_dah) = self.paddle_keys;
        let (dit, dah) = if self.keyer.mode == keyer::Mode::Straight {
            (pad_dit || key_dit || key_dah, false)
        } else {
            (pad_dit || key_dit, pad_dah || key_dah)
        };
        let wpm = self.cw_speed_wpm;
        let transitions = self.keyer.update(now_ms, dit, dah, wpm);
        self.send_key(transitions);
        if self.key_sent.0 && now_ms - self.key_sent.1 >= cw::KEY_REPEAT_MS as f64 {
            self.send_key(vec![(now_ms, true)]);
        }
        let down = self.keyer.is_down(now_ms);
        if let Some(p) = &self.player {
            p.set_sidetone(down, self.sidetone_level);
        }
        if self.keyer.is_busy() || dit || dah {
            ctx.request_repaint();
        }
    }

    fn send_key(&mut self, transitions: Vec<(f64, bool)>) {
        for (t, down) in transitions {
            self.send(cw::key(t.max(0.0).round() as u64, down, self.key_delay_ms));
            self.key_sent = (down, self.now * 1000.0);
        }
    }

    /// Lets go of the key at once, and stops the sidetone.
    fn release_key(&mut self) {
        let now_ms = self.now * 1000.0;
        let mut transitions = self.keyer.release(now_ms);
        if transitions.is_empty() && self.key_sent.0 {
            transitions.push((now_ms, false));
        }
        self.send_key(transitions);
        self.key_sent.0 = false;
        if let Some(p) = &self.player {
            p.set_sidetone(false, self.sidetone_level);
        }
    }

    /// CW: the radio's saved messages, one button each, and the speed --
    /// on a line of its own when `narrow`.
    fn cw_settings(&mut self, ui: &mut egui::Ui, narrow: bool) {
        let messages: Vec<(usize, String)> = self
            .radio
            .cw_messages
            .iter()
            .enumerate()
            .filter(|(_, m)| !m.trim().is_empty())
            .map(|(i, m)| (i, m.clone()))
            .collect();
        for (i, message) in messages {
            let label: String = message.chars().take(10).collect();
            let label = if message.chars().count() > 10 { format!("{}: {label}...", i + 1) } else { format!("{}: {label}", i + 1) };
            if ui.button(label).on_hover_text(format!("Send \"{message}\"")).clicked() {
                self.send_cw(&message);
            }
        }
        if narrow {
            ui.end_row();
        } else {
            ui.add_space(12.0);
        }
        ui.label("Speed:");
        if ui
            .add(egui::Slider::new(&mut self.cw_speed_wpm, cw::SPEED_MIN..=cw::SPEED_MAX).suffix(" WPM"))
            .changed()
        {
            self.cw_speed_set_at = self.now;
            self.send(cw::speed(self.cw_speed_wpm));
        }
        if narrow {
            ui.end_row();
        } else {
            ui.add_space(12.0);
        }
    }

    fn add_waterfall_row(&mut self, frame: &SpectrumFrame) {
        let row = frame.db().iter().map(|&db| waterfall_colour((db - self.db_low) / self.db_range)).collect();
        self.waterfall.rows.push_front(row);
        self.waterfall.rows.truncate(WATERFALL_ROWS);
        self.waterfall.dirty = true;
    }

    /// Server, password and Connect -- or, connected on a narrow screen,
    /// just Disconnect and the status, since the address can't change then.
    /// Wraps, so Connect can't end up off the edge of a phone held upright.
    fn top_bar(&mut self, ui: &mut egui::Ui, narrow: bool) {
        let connected = self.link.as_ref().is_some_and(|l| !matches!(l.status, Status::Closed(_)));
        ui.horizontal_wrapped(|ui| {
            let button = |ui: &mut egui::Ui, text: &str| {
                ui.add(egui::Button::new(egui::RichText::new(text).strong()).min_size(Vec2::new(96.0, 30.0))).clicked()
            };
            if connected {
                if button(ui, "Disconnect") {
                    if self.keyed_here {
                        self.set_mox(false);
                    }
                    self.link = None;
                    self.keyed_here = false;
                }
                if !narrow {
                    ui.weak(&self.url);
                }
            } else {
                let mut go = button(ui, "Connect");
                // Narrow: a line each, so a label stays with its field.
                if narrow {
                    ui.end_row();
                }
                let field = |ui: &mut egui::Ui, max: f32| if narrow { ui.available_width() - 80.0 } else { max };
                ui.label("Server:");
                let width = field(ui, 260.0);
                ui.add(egui::TextEdit::singleline(&mut self.url).desired_width(width));
                if narrow {
                    ui.end_row();
                }
                ui.label("Password:");
                let width = field(ui, 120.0);
                let password = ui.add(egui::TextEdit::singleline(&mut self.password).password(true).desired_width(width));
                go |= password.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if go {
                    self.connect(ui.ctx());
                }
                if narrow {
                    ui.end_row();
                }
            }
            let status = match &self.link {
                None => "not connected".to_string(),
                Some(l) => match &l.status {
                    Status::Connecting => "connecting...".to_string(),
                    Status::Open => match &self.radio.extensions {
                        Some(_) => format!("connected to {}", self.radio.device),
                        None => format!("connected to {} (no hpsdr-rs extensions yet)", self.radio.device),
                    },
                    Status::Closed(why) => format!("disconnected: {why}"),
                },
            };
            ui.weak(status);
        });
    }

    /// Wide: every row of controls. Narrow (a phone held upright): the
    /// frequency, S-meter, MOX and - / + always, and the rest one section at
    /// a time, picked by a row of buttons -- the selected one again closes
    /// it, leaving more of the screen to the spectrum.
    fn controls(&mut self, ui: &mut egui::Ui, narrow: bool) {
        self.frequency_row(ui);
        if !narrow {
            self.step_row(ui);
            self.band_row(ui);
            ui.horizontal_wrapped(|ui| {
                self.mode_buttons(ui);
                ui.add_space(16.0);
                self.audio_controls(ui, false);
            });
            self.dsp_controls(ui, true, true);
            if self.radio.sends_cw() {
                ui.horizontal_wrapped(|ui| self.cw_send_row(ui, false));
                ui.horizontal_wrapped(|ui| {
                    self.cw_settings(ui, false);
                    self.tx_settings(ui, false);
                });
                if self.radio.keys_cw() {
                    ui.horizontal_wrapped(|ui| self.key_settings(ui, false));
                    self.key_pads(ui, false);
                }
            } else if self.radio.can_transmit() {
                ui.horizontal_wrapped(|ui| {
                    self.mox_button(ui);
                    ui.add_space(12.0);
                    self.tx_settings(ui, false);
                });
            }
            return;
        }
        ui.horizontal_wrapped(|ui| {
            if self.radio.sends_cw() {
                self.cw_stop_button(ui);
                ui.add_space(8.0);
            } else if self.radio.can_transmit() {
                self.mox_button(ui);
                ui.add_space(8.0);
            }
            self.minus_plus(ui);
            if self.radio.mox {
                let tx = self.radio.tx.unwrap_or_default();
                ui.strong(format!("{:.1} W  SWR {:.2}", tx.forward_w, tx.swr));
            }
        });
        let mut sections = vec![Section::Step, Section::Band, Section::Mode];
        if self.radio.dsp.is_some() {
            sections.extend([Section::Filter, Section::Noise]);
        }
        sections.push(Section::Audio);
        if self.radio.keys_cw() {
            sections.push(Section::Key);
        }
        if self.radio.can_transmit() {
            sections.push(Section::Tx);
        }
        ui.horizontal_wrapped(|ui| {
            for s in sections {
                let open = self.section == Some(s);
                if ui.add(egui::Button::selectable(open, egui::RichText::new(s.label()).strong())).clicked() {
                    self.section = if open { None } else { Some(s) };
                }
            }
        });
        match self.section {
            Some(Section::Step) => self.step_row(ui),
            Some(Section::Band) => self.band_row(ui),
            Some(Section::Mode) => {
                ui.horizontal_wrapped(|ui| self.mode_buttons(ui));
            }
            Some(Section::Filter) => self.dsp_controls(ui, true, false),
            Some(Section::Noise) => self.dsp_controls(ui, false, true),
            Some(Section::Audio) => {
                ui.horizontal_wrapped(|ui| self.audio_controls(ui, true));
            }
            Some(Section::Key) if self.radio.keys_cw() => {
                self.key_pads(ui, true);
                ui.horizontal_wrapped(|ui| self.key_settings(ui, true));
            }
            Some(Section::Key) => {}
            Some(Section::Tx) => {
                if self.radio.sends_cw() {
                    ui.horizontal_wrapped(|ui| self.cw_send_row(ui, true));
                    ui.horizontal_wrapped(|ui| self.cw_settings(ui, true));
                }
                ui.horizontal_wrapped(|ui| self.tx_settings(ui, true));
            }
            None => {}
        }
    }

    fn frequency_row(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            let digits = format_frequency(self.radio.vfo_hz);
            let colour = readable(
                ui,
                if self.radio.mox { Color32::from_rgb(230, 60, 60) } else { Color32::from_rgb(60, 230, 60) },
            );
            let resp = ui
                .add(
                    egui::Label::new(egui::RichText::new(digits).monospace().size(32.0).color(colour))
                        .sense(egui::Sense::drag()),
                )
                .on_hover_text("Slide left or right, or scroll, to tune by the step");
            if resp.hovered() {
                self.scroll_tune(ui);
            }
            // Slide: one step per VFO_SLIDE_PX_PER_STEP, right to go up.
            if resp.dragged() {
                self.vfo_slide_px += resp.drag_delta().x;
                let steps = (self.vfo_slide_px / VFO_SLIDE_PX_PER_STEP).trunc();
                self.vfo_slide_px -= steps * VFO_SLIDE_PX_PER_STEP;
                self.step(steps as i64, self.step_hz);
            } else {
                self.vfo_slide_px = 0.0;
            }
            ui.add_space(16.0);
            self.s_meter(ui);
        });
    }

    /// Steps, and - / +.
    fn step_row(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.label("Step:");
            for (hz, label) in STEPS_HZ {
                if ui.add(egui::Button::selectable(self.step_hz == hz, label)).clicked() {
                    self.step_hz = hz;
                }
            }
            ui.add_space(8.0);
            self.minus_plus(ui);
        });
    }

    /// - / + by the step -- big enough to tap.
    fn minus_plus(&mut self, ui: &mut egui::Ui) {
        let size = Vec2::new(44.0, 30.0);
        if ui.add_sized(size, egui::Button::new("-")).clicked() {
            self.step(-1, self.step_hz);
        }
        if ui.add_sized(size, egui::Button::new("+")).clicked() {
            self.step(1, self.step_hz);
        }
    }

    fn band_row(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            if let Some(list) = self.radio.bands.clone() {
                // The radio's own buttons; selecting one does just what its
                // button does there (remembered frequency, mode, filter,
                // transverter).
                let current = current_band(&list, self.radio.vfo_hz);
                for b in &list {
                    let selected = current == Some(b.name.as_str());
                    if ui.add(egui::Button::selectable(selected, &b.name)).clicked() && !selected {
                        self.send(bands::select(&b.name));
                    }
                }
            } else {
                let current = FALLBACK_BANDS.iter().find(|b| (b.1..=b.2).contains(&self.radio.vfo_hz)).map(|b| b.0);
                for (name, low, high, first) in FALLBACK_BANDS {
                    if ui.add(egui::Button::selectable(current == Some(name), name)).clicked() && current != Some(name) {
                        if let Some(c) = current {
                            self.band_memory.insert(c, self.radio.vfo_hz);
                        }
                        let hz =
                            self.band_memory.get(name).copied().filter(|f| (low..=high).contains(f)).unwrap_or(first);
                        self.tune(hz);
                    }
                }
            }
        });
    }

    fn mode_buttons(&mut self, ui: &mut egui::Ui) {
        for (tci, label) in MODES {
            if ui.add(egui::Button::selectable(self.radio.mode == tci, label)).clicked() {
                self.radio.mode = tci.to_string();
                self.send(Command::new("modulation", &["0", tci]));
            }
        }
    }

    /// Audio gain, mute, and the spectrum's range -- on lines of their own
    /// when `narrow` (a slider doesn't wrap onto the next line by itself).
    fn audio_controls(&mut self, ui: &mut egui::Ui, narrow: bool) {
        ui.label("Audio:");
        if ui.add(egui::Slider::new(&mut self.af_gain_db, -40.0..=20.0).suffix(" dB")).changed() {
            self.apply_gain();
        }
        if ui.add(egui::Button::selectable(self.muted, "Mute")).clicked() {
            self.muted = !self.muted;
            self.apply_gain();
        }
        if narrow {
            ui.end_row();
        } else {
            ui.add_space(16.0);
        }
        ui.label("Range:");
        ui.add(egui::Slider::new(&mut self.db_range, 20.0..=120.0).suffix(" dB"));
    }

    /// Filter and AGC (`filter`), and noise controls (`noise`), once the
    /// server has reported them.
    fn dsp_controls(&mut self, ui: &mut egui::Ui, filter: bool, noise: bool) {
        let Some(mut d) = self.radio.dsp.clone() else { return };
        let mut changes: Vec<(&str, String)> = Vec::new();
        if filter {
            ui.horizontal_wrapped(|ui| {
                ui.label("Filter:");
                let presets: &[u32] = match self.radio.mode.as_str() {
                    "cw" | "cwl" | "cwu" => &[100, 250, 500, 1000],
                    "am" | "sam" | "dsb" => &[3000, 4000, 5000],
                    "nfm" => &[],
                    _ => &[1800, 2100, 2400, 2700, 3000],
                };
                for &w in presets {
                    if ui.add(egui::Button::selectable(d.width_hz == w, format_hz(w))).clicked() {
                        d.width_hz = w;
                        changes.push((dsp::SET_WIDTH, w.to_string()));
                    }
                }
                if !presets.contains(&d.width_hz) {
                    ui.weak(format_hz(d.width_hz));
                }
                ui.add_space(12.0);
                for (mode, label) in dsp::AGC_MODES.iter().zip(["AGC Off", "Long", "Slow", "Med", "Fast"]) {
                    if ui.add(egui::Button::selectable(d.agc == *mode, label)).clicked() {
                        d.agc = mode.to_string();
                        changes.push((dsp::SET_AGC, mode.to_string()));
                    }
                }
                if ui.add(egui::Slider::new(&mut d.agc_gain_db, 0..=140).suffix(" dB")).changed() {
                    changes.push((dsp::SET_AGC_GAIN, d.agc_gain_db.to_string()));
                }
            });
        }
        if noise {
            ui.horizontal_wrapped(|ui| {
                for (mode, label) in dsp::NB_MODES.iter().zip(["NB Off", "NB", "NB2"]) {
                    if ui.add(egui::Button::selectable(d.nb == *mode, label)).clicked() {
                        d.nb = mode.to_string();
                        changes.push((dsp::SET_NB, mode.to_string()));
                    }
                }
                ui.add_space(12.0);
                // Labels as hpsdr-rs's own NR menu: its third, "nr3" here, is NNR.
                for (mode, label) in dsp::NR_MODES.iter().zip(["NR Off", "NR", "NR2", "NNR"]) {
                    if ui.add(egui::Button::selectable(d.nr == *mode, label)).clicked() {
                        d.nr = mode.to_string();
                        changes.push((dsp::SET_NR, mode.to_string()));
                    }
                }
                ui.add_space(12.0);
                if ui.add(egui::Button::selectable(d.anf, "ANF")).clicked() {
                    d.anf = !d.anf;
                    changes.push((dsp::SET_ANF, d.anf.to_string()));
                }
                if ui.add(egui::Button::selectable(d.snb, "SNB")).clicked() {
                    d.snb = !d.snb;
                    changes.push((dsp::SET_SNB, d.snb.to_string()));
                }
            });
        }
        if !changes.is_empty() {
            self.dsp_set_at = self.now;
            for (name, value) in changes {
                self.send(dsp::set(name, &value));
            }
            self.radio.dsp = Some(d);
        }
    }

    fn apply_gain(&self) {
        if let Some(p) = &self.player {
            p.set_gain(if self.muted { 0.0 } else { 10f32.powf(self.af_gain_db / 20.0) });
        }
    }

    fn s_meter(&self, ui: &mut egui::Ui) {
        let Some(frame) = &self.spectrum else { return };
        let dbm = frame.meter_dbm;
        let s_units = ((dbm + 127.0) / 6.0).clamp(0.0, 9.0);
        let text = if dbm > -73.0 { format!("S9+{:.0}", dbm + 73.0) } else { format!("S{s_units:.0}") };
        // Narrower on a phone, so it fits beside its reading.
        let width = (ui.available_width() - 130.0).clamp(80.0, 200.0);
        let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 18.0), egui::Sense::hover());
        let painter = ui.painter();
        painter.rect_filled(rect, 2.0, Color32::from_gray(30));
        let frac = ((dbm + 127.0) / 94.0).clamp(0.0, 1.0); // S0 to S9+40
        let fill = Rect::from_min_size(rect.min, Vec2::new(rect.width() * frac, rect.height()));
        painter.rect_filled(fill, 2.0, if dbm > -73.0 { Color32::from_rgb(200, 60, 60) } else { Color32::from_rgb(60, 180, 60) });
        ui.label(egui::RichText::new(format!("{text}  {dbm:.0} dBm")).monospace());
    }

    /// Scroll-wheel tuning over the frequency or the spectrum.
    fn scroll_tune(&mut self, ui: &egui::Ui) {
        let (notches, step) = ui.input(|i| {
            let n: i64 = i
                .events
                .iter()
                .filter_map(|e| match e {
                    egui::Event::MouseWheel { delta, .. } if delta.y != 0.0 => Some(delta.y.signum() as i64),
                    _ => None,
                })
                .sum();
            let step = if i.modifiers.alt { 10 } else if i.modifiers.shift { 100 } else { self.step_hz };
            (n, step)
        });
        self.step(notches, step);
    }

    fn spectrum_and_waterfall(&mut self, ui: &mut egui::Ui) {
        let Some(frame) = self.spectrum.clone() else {
            ui.weak("No spectrum yet.");
            return;
        };
        let db = frame.db();
        // Auto low: follow the noise (the row's 20th percentile) slowly.
        let mut sorted = db.clone();
        sorted.sort_by(f32::total_cmp);
        let floor = sorted[sorted.len() / 5] - 5.0;
        self.db_low += 0.05 * (floor - self.db_low);

        let full = ui.available_size();
        let spectrum_h = (full.y * 0.45).max(100.0);
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(full.x, spectrum_h), egui::Sense::click_and_drag());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, Color32::BLACK);
        let low_hz = frame.centre_hz as f64 - frame.span_hz as f64 / 2.0;
        let x_of = |hz: f64| rect.left() + ((hz - low_hz) / frame.span_hz as f64) as f32 * rect.width();
        let y_of = |v: f32| rect.bottom() - ((v - self.db_low) / self.db_range).clamp(0.0, 1.0) * rect.height();

        // Passband and dial line.
        let (pb_low, pb_high) = self.radio.passband();
        let vfo = self.radio.vfo_hz as f64;
        painter.rect_filled(
            Rect::from_x_y_ranges(x_of(vfo + pb_low as f64)..=x_of(vfo + pb_high as f64), rect.y_range()),
            0.0,
            Color32::from_rgba_unmultiplied(70, 150, 230, 50),
        );
        painter.line_segment([Pos2::new(x_of(vfo), rect.top()), Pos2::new(x_of(vfo), rect.bottom())], Stroke::new(1.0, Color32::RED));

        // dB gridlines every 10 dB, and frequency labels.
        let mut g = (self.db_low / 10.0).ceil() * 10.0;
        while g < self.db_low + self.db_range {
            let y = y_of(g);
            painter.line_segment([Pos2::new(rect.left(), y), Pos2::new(rect.right(), y)], Stroke::new(1.0, Color32::from_gray(45)));
            painter.text(Pos2::new(rect.left() + 2.0, y), egui::Align2::LEFT_BOTTOM, format!("{g:.0} dB"), egui::FontId::monospace(10.0), Color32::GRAY);
            g += 10.0;
        }
        let tick = nice_step(frame.span_hz as f64 / 8.0);
        let mut f = (low_hz / tick).ceil() * tick;
        while f < low_hz + frame.span_hz as f64 {
            let x = x_of(f);
            painter.line_segment([Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())], Stroke::new(1.0, Color32::from_gray(45)));
            painter.text(Pos2::new(x + 2.0, rect.bottom()), egui::Align2::LEFT_BOTTOM, format!("{:.3}", f / 1e6), egui::FontId::monospace(10.0), Color32::GRAY);
            f += tick;
        }

        // The trace.
        let n = db.len().max(2);
        let points: Vec<Pos2> =
            db.iter().enumerate().map(|(i, &v)| Pos2::new(rect.left() + i as f32 / (n - 1) as f32 * rect.width(), y_of(v))).collect();
        painter.add(egui::Shape::line(points, Stroke::new(1.0, Color32::from_rgb(140, 230, 140))));

        self.click_and_scroll_tune(ui, &resp, low_hz, frame.span_hz);

        // Waterfall below, newest row at the top -- tuned the same way.
        ui.add_space(4.0);
        let (wf_rect, wf_resp) =
            ui.allocate_exact_size(Vec2::new(full.x, ui.available_height().max(20.0)), egui::Sense::click_and_drag());
        self.click_and_scroll_tune(ui, &wf_resp, low_hz, frame.span_hz);
        if self.waterfall.dirty && !self.waterfall.rows.is_empty() {
            let width = self.waterfall.rows[0].len();
            let mut image = ColorImage::new([width, WATERFALL_ROWS], vec![Color32::BLACK; width * WATERFALL_ROWS]);
            for (y, row) in self.waterfall.rows.iter().enumerate() {
                if row.len() == width {
                    image.pixels[y * width..(y + 1) * width].copy_from_slice(row);
                }
            }
            match &mut self.waterfall.texture {
                Some(t) => t.set(image, TextureOptions::LINEAR),
                None => self.waterfall.texture = Some(ui.ctx().load_texture("waterfall", image, TextureOptions::LINEAR)),
            }
            self.waterfall.dirty = false;
        }
        if let Some(t) = &self.waterfall.texture {
            ui.painter().image(t.id(), wf_rect, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
        }
    }

    /// Click-to-tune and scroll-to-tune over the spectrum or waterfall:
    /// `resp` covers `span_hz` from `low_hz`, left to right.
    fn click_and_scroll_tune(&mut self, ui: &egui::Ui, resp: &egui::Response, low_hz: f64, span_hz: u32) {
        if let Some(pos) = resp.interact_pointer_pos().filter(|_| resp.clicked()) {
            let rect = resp.rect;
            let hz = low_hz + ((pos.x - rect.left()) / rect.width()) as f64 * span_hz as f64;
            let step = if self.radio.is_cw() { 10.0 } else { 100.0 };
            self.tune(((hz / step).round() * step).max(0.0) as u64);
        }
        if resp.hovered() {
            self.scroll_tune(ui);
        }
        // Slide: right tunes up by the distance slid, left down -- the same
        // way round as the frequency display. (It was the other way, as if
        // dragging the band itself; REAL REPORT 2026-11-07, Android: sliding
        // to go higher went lower.)
        if resp.dragged() && resp.rect.width() > 0.0 {
            self.span_slide_hz += (resp.drag_delta().x / resp.rect.width()) as f64 * span_hz as f64;
            let whole = self.span_slide_hz.trunc();
            self.span_slide_hz -= whole;
            if whole != 0.0 && self.radio.vfo_hz > 0 {
                self.tune((self.radio.vfo_hz as f64 + whole).max(0.0) as u64);
            }
        } else if !resp.dragged() && resp.drag_stopped() {
            self.span_slide_hz = 0.0;
        }
    }

    fn status_line(&mut self, ui: &mut egui::Ui) {
        let now = ui.input(|i| i.time);
        if let Some(link) = &self.link {
            let (bytes_in, bytes_out, at, ..) = self.rate_sample;
            if now - at >= 1.0 {
                let kbits = |bytes: u64| bytes as f64 * 8.0 / (now - at) / 1000.0;
                let rate_in = kbits(link.bytes_in.saturating_sub(bytes_in));
                let rate_out = kbits(link.bytes_out.saturating_sub(bytes_out));
                self.rate_sample = (link.bytes_in, link.bytes_out, now, rate_in, rate_out);
            }
        }
        let mut text = format!("built {}   {:.0} kbit/s in", built_at(), self.rate_sample.3);
        if self.rate_sample.4 >= 1.0 {
            text.push_str(&format!(", {:.0} out", self.rate_sample.4));
        }
        if let Some(p) = &self.player {
            text.push_str(&format!("   audio buffer {:.0} ms", p.buffered_s() * 1000.0));
        }
        if let Some(e) = &self.audio_error {
            text.push_str(&format!("   {e}"));
        }
        ui.horizontal_wrapped(|ui| {
            ui.weak(text);
            if let Some(e) = &self.mic_error {
                ui.colored_label(readable(ui, Color32::from_rgb(230, 80, 80)), e);
            }
            if let Some((notice, at)) = &self.notice {
                if now - at < NOTICE_S {
                    ui.colored_label(readable(ui, Color32::from_rgb(230, 180, 60)), notice);
                }
            }
        });
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.now = ui.input(|i| i.time);
        self.flush_tune();
        let incoming = self.link.as_mut().map(|l| l.poll()).unwrap_or_default();
        for i in incoming {
            self.handle(i);
        }
        // Lost the connection: nothing to send to.
        if self.link.as_ref().is_none_or(|l| l.status != Status::Open) {
            self.keyed_here = false;
        }
        self.track_touches(ui);
        self.take_paddle_keys(ui.ctx());
        self.send_tx_audio();
        if self.keyed_here {
            // Audio goes out in 20 ms blocks: keep coming back for it.
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(10));
        }
        let screen = ui.ctx().content_rect().size();
        let narrow = screen.x < NARROW_WIDTH || screen.y < SHORT_HEIGHT;
        egui::Panel::top("top").show(ui, |ui| {
            // As tall as the controls, up to the limit: without the minimum,
            // the scroll area only gets what the panel had the frame before,
            // and shows a couple of rows.
            let limit = screen.y * CONTROLS_MAX_HEIGHT;
            egui::ScrollArea::vertical().max_height(limit).min_scrolled_height(limit).show(ui, |ui| {
                self.top_bar(ui, narrow);
                self.controls(ui, narrow);
            });
        });
        self.run_keyer(ui.ctx());
        egui::Panel::bottom("status").show(ui, |ui| self.status_line(ui));
        egui::CentralPanel::default().show(ui, |ui| self.spectrum_and_waterfall(ui));
        // Keep the status line (data rate, buffer) moving.
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
    }
}

/// When this was built, as UTC "YYYY-MM-DD HH:MM" (see build.rs).
fn built_at() -> String {
    let secs: i64 = env!("HPSDR_REMOTE_BUILT").parse().unwrap_or(0);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02} {:02}:{:02} UTC", rem / 3600, rem % 3600 / 60)
}

/// The band to highlight for `hz`: the first whose range has it, Gen
/// (general coverage, which spans everything) only if no other does.
fn current_band(list: &[BandInfo], hz: u64) -> Option<&str> {
    list.iter()
        .find(|b| b.name != "Gen" && (b.low_hz..=b.high_hz).contains(&hz))
        .or_else(|| list.iter().find(|b| b.name == "Gen"))
        .map(|b| b.name.as_str())
}

/// A filter width for a button: "2.7k", "500".
fn format_hz(hz: u32) -> String {
    if hz >= 1000 {
        let k = format!("{:.1}", hz as f64 / 1000.0);
        format!("{}k", k.trim_end_matches(".0"))
    } else {
        hz.to_string()
    }
}

/// `hz` as MHz with kHz and Hz groups: 7.100.000 -> "7.100000".
fn format_frequency(hz: u64) -> String {
    format!("{}.{:06}", hz / 1_000_000, hz % 1_000_000)
}

/// A round number of Hz near `approx`: 1, 2 or 5 times a power of ten.
fn nice_step(approx: f64) -> f64 {
    let p = 10f64.powf(approx.max(1.0).log10().floor());
    [1.0, 2.0, 5.0, 10.0].into_iter().map(|m| m * p).find(|s| *s >= approx).unwrap_or(10.0 * p)
}

/// Black through blue, cyan, yellow to red, for 0..1.
/// Colour for text drawn straight on the page's background: as given in
/// dark mode, darker in light mode so it still reads. The client follows
/// the browser's (or desktop's) dark/light setting -- egui's default --
/// while the spectrum, waterfall and meters stay dark in both.
fn readable(ui: &egui::Ui, c: Color32) -> Color32 {
    if ui.visuals().dark_mode {
        return c;
    }
    let [r, g, b, a] = c.to_array();
    let dim = |v: u8| (v as f32 * 0.6) as u8;
    Color32::from_rgba_premultiplied(dim(r), dim(g), dim(b), a)
}

fn waterfall_colour(v: f32) -> Color32 {
    let v = v.clamp(0.0, 1.0);
    let stops = [(0.0, [0, 0, 0]), (0.3, [0, 0, 200]), (0.55, [0, 200, 220]), (0.8, [240, 230, 0]), (1.0, [255, 40, 40])];
    for w in stops.windows(2) {
        let ((a, ca), (b, cb)) = (w[0], w[1]);
        if v <= b {
            let t = (v - a) / (b - a);
            let c = |i: usize| (ca[i] as f32 + (cb[i] as f32 - ca[i] as f32) * t) as u8;
            return Color32::from_rgb(c(0), c(1), c(2));
        }
    }
    Color32::from_rgb(255, 40, 40)
}

/// The server's address to offer first. On the web: served by hpsdr-rs
/// (which puts the path in the page's `hpsdr-tci-path`), back to that same
/// address and port -- wss on an HTTPS page; served some other way, the
/// TCI port on the same host. Natively, this machine's TCI port.
fn default_url() -> String {
    #[cfg(target_arch = "wasm32")]
    {
        let window = web_sys::window();
        let path = window
            .as_ref()
            .and_then(|w| w.document())
            .and_then(|d| d.query_selector("meta[name=hpsdr-tci-path]").ok().flatten())
            .and_then(|m| m.get_attribute("content"))
            .filter(|p| p.starts_with('/'));
        if let Some(location) = window.map(|w| w.location()) {
            let secure = location.protocol().is_ok_and(|p| p == "https:");
            let scheme = if secure { "wss" } else { "ws" };
            match (path, location.host(), location.hostname()) {
                (Some(path), Ok(host), _) if !host.is_empty() => return format!("{scheme}://{host}{path}"),
                (None, _, Ok(host)) if !host.is_empty() => return format!("{scheme}://{host}:50001"),
                _ => {}
            }
        }
    }
    "ws://127.0.0.1:50001".to_string()
}

/// `url` with `password` added as hpsdr-rs's web server expects it
/// (`?password=`, %-encoded); unchanged if there's no password.
fn with_password(url: &str, password: &str) -> String {
    if password.is_empty() {
        return url.to_string();
    }
    let encoded: String = password
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect();
    let separator = if url.contains('?') { '&' } else { '?' };
    format!("{url}{separator}password={encoded}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(with_password("wss://h:8080/tci", ""), "wss://h:8080/tci");
        assert_eq!(with_password("wss://h:8080/tci", "p@ss word!"), "wss://h:8080/tci?password=p%40ss%20word%21");
        assert_eq!(format_frequency(7_100_000), "7.100000");
        assert_eq!(format_frequency(10_489_750_000), "10489.750000");
        assert_eq!(nice_step(23_000.0), 50_000.0);
        assert_eq!(nice_step(9_000.0), 10_000.0);
        assert_eq!(waterfall_colour(0.0), Color32::BLACK);
        let mut r = Radio { mode: "usb".into(), ..Default::default() };
        assert_eq!(r.passband(), (150, 2700));
        r.filter = Some(("usb".into(), 100, 3000));
        assert_eq!(r.passband(), (100, 3000));
        r.mode = "lsb".into();
        assert_eq!(r.passband(), (-2700, -150), "filter was for another mode");
        r.dsp = Some(DspState {
            width_hz: 1800,
            agc: "fast".into(),
            agc_gain_db: 80,
            nb: "off".into(),
            nr: "off".into(),
            anf: false,
            snb: false,
        });
        assert_eq!(r.passband(), (-1800, -150), "from the reported width");
        r.mode = "cwu".into();
        r.dsp.as_mut().unwrap().width_hz = 500;
        assert_eq!(r.passband(), (350, 850));
        r.mode = "cwl".into();
        assert_eq!(r.passband(), (-850, -350));
        assert_eq!((format_hz(2700), format_hz(3000), format_hz(500)), ("2.7k".into(), "3k".into(), "500".into()));
        let list = vec![
            BandInfo { name: "70cm".into(), low_hz: 420_000_000, high_hz: 450_000_000 },
            BandInfo { name: "Gen".into(), low_hz: 325_000_000, high_hz: 3_800_000_000 },
            BandInfo { name: "QO-100".into(), low_hz: 10_489_500_000, high_hz: 10_490_000_000 },
        ];
        assert_eq!(current_band(&list, 432_200_000), Some("70cm"));
        assert_eq!(current_band(&list, 10_489_750_000), Some("QO-100"));
        assert_eq!(current_band(&list, 1_000_000_000), Some("Gen"));
    }
}
