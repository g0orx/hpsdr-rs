mod audio;
mod config;
mod discovery;
mod discovery_ui;
mod radio;
mod rigctl;
mod spectrum;
mod tci;
mod tx;
mod wdsp_sys;

use audio::{AudioOutput, MicInput};
use config::{Config, ExtraReceiverConfig};
use discovery::{Boards, Device};
use discovery_ui::{DiscoveryAction, DiscoveryWindow};
use eframe::egui;
use radio::{IqSample, RadioSession, RadioSettings};
use rigctl::RigctlServer;
use spectrum::{SpectrumHandle, ALL_MODES};
use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tci::TciServer;
use tx::TxHandle;

/// Amateur bands 160m-6m. Default frequencies are FT8 calling
/// frequencies (matches the digital-mode focus of the rest of this
/// app -- rigctl/TCI support etc.) rather than voice calling
/// frequencies. 60m is treated as a simple continuous range for
/// simplicity; real-world allocations there are channelized and vary
/// significantly by country/region, which isn't something we can get
/// "correct" without knowing where the radio actually is.
struct Band {
    name: &'static str,
    low_hz: u32,
    high_hz: u32,
    default_hz: u32,
    default_mode: spectrum::Mode,
    default_width_hz: f64,
}

const BANDS: [Band; 11] = [
    Band { name: "160m", low_hz: 1_800_000, high_hz: 2_000_000, default_hz: 1_900_000, default_mode: spectrum::Mode::Lsb, default_width_hz: 2700.0 },
    Band { name: "80m", low_hz: 3_500_000, high_hz: 4_000_000, default_hz: 3_573_000, default_mode: spectrum::Mode::Lsb, default_width_hz: 2700.0 },
    Band { name: "60m", low_hz: 5_330_000, high_hz: 5_406_000, default_hz: 5_357_000, default_mode: spectrum::Mode::Usb, default_width_hz: 2700.0 }, // USB by regulatory convention despite being below 10MHz
    Band { name: "40m", low_hz: 7_000_000, high_hz: 7_300_000, default_hz: 7_074_000, default_mode: spectrum::Mode::Lsb, default_width_hz: 2700.0 },
    Band { name: "30m", low_hz: 10_100_000, high_hz: 10_150_000, default_hz: 10_136_000, default_mode: spectrum::Mode::Usb, default_width_hz: 2700.0 },
    Band { name: "20m", low_hz: 14_000_000, high_hz: 14_350_000, default_hz: 14_074_000, default_mode: spectrum::Mode::Usb, default_width_hz: 2700.0 },
    Band { name: "17m", low_hz: 18_068_000, high_hz: 18_168_000, default_hz: 18_100_000, default_mode: spectrum::Mode::Usb, default_width_hz: 2700.0 },
    Band { name: "15m", low_hz: 21_000_000, high_hz: 21_450_000, default_hz: 21_074_000, default_mode: spectrum::Mode::Usb, default_width_hz: 2700.0 },
    Band { name: "12m", low_hz: 24_890_000, high_hz: 24_990_000, default_hz: 24_915_000, default_mode: spectrum::Mode::Usb, default_width_hz: 2700.0 },
    Band { name: "10m", low_hz: 28_000_000, high_hz: 29_700_000, default_hz: 28_074_000, default_mode: spectrum::Mode::Usb, default_width_hz: 2700.0 },
    Band { name: "6m", low_hz: 50_000_000, high_hz: 54_000_000, default_hz: 50_313_000, default_mode: spectrum::Mode::Usb, default_width_hz: 2700.0 },
];

fn band_for_frequency(freq_hz: u32) -> Option<&'static Band> {
    BANDS.iter().find(|b| freq_hz >= b.low_hz && freq_hz <= b.high_hz)
}

/// Looks up the current band's calibrated PA gain (dB), falling back to
/// radio::DEFAULT_PA_GAIN_DB for a band with no calibration entry yet
/// (or a frequency outside every defined band). See ConnectedState's
/// pa_calibration field doc for how this gets pushed into the running
/// session.
fn resolved_pa_gain_db(pa_calibration: &std::collections::HashMap<String, f32>, freq_hz: u32) -> f32 {
    band_for_frequency(freq_hz)
        .and_then(|b| pa_calibration.get(b.name))
        .copied()
        .unwrap_or(radio::DEFAULT_PA_GAIN_DB)
}

/// Everything remembered per-band: not just the last frequency used,
/// but also the spectrum/waterfall level ranges, since different bands
/// often want different level settings (e.g. a noisy 160m vs a quiet
/// 6m opening).
#[derive(Copy, Clone, serde::Serialize, serde::Deserialize)]
pub struct BandSettings {
    pub frequency_hz: u32,
    pub db_low: f32,
    pub db_high: f32,
    pub waterfall_db_low: f32,
    pub waterfall_db_high: f32,
    /// Mode last used on this band. Option (not just Mode) so band
    /// entries saved before this field existed still deserialize --
    /// serde defaults a missing key to None for Option fields with no
    /// #[serde(default)] needed. None is also what a band that's never
    /// actually been visited (band_memory has no entry for it at all)
    /// effectively behaves like, so band-switch logic treats "no entry"
    /// and "entry with mode: None" the same way: fall back to the
    /// band's own default_mode.
    #[serde(default)]
    pub mode: Option<spectrum::Mode>,
}

/// Records the current frequency, level ranges, and mode against
/// whichever band the frequency falls in, so switching bands and back
/// remembers where you actually were, how the displays were set, and
/// what mode you had selected -- not just the band's defaults. Called
/// on every tuning/mode/level-range change (not just band switches),
/// since a full BandSettings replace on each call means anything not
/// passed through here would otherwise get silently reset on the next
/// unrelated change within the same band.
fn remember_band_settings(
    band_memory: &mut std::collections::HashMap<String, BandSettings>,
    freq_hz: u32,
    db_low: f32,
    db_high: f32,
    waterfall_db_low: f32,
    waterfall_db_high: f32,
    mode: spectrum::Mode,
) {
    if let Some(band) = band_for_frequency(freq_hz) {
        band_memory.insert(
            band.name.to_string(),
            BandSettings {
                frequency_hz: freq_hz,
                db_low,
                db_high,
                waterfall_db_low,
                waterfall_db_high,
                mode: Some(mode),
            },
        );
    }
}

/// Last filter width the user set while in `mode`, if any -- falls back
/// to the mode's built-in default (spectrum::default_width_hz) the
/// first time a mode is used, same as before this per-mode memory
/// existed. Keyed by Mode::label() (a fixed string) rather than Mode
/// itself so it round-trips through JSON the same way band_memory does.
fn width_for_mode(width_memory: &std::collections::HashMap<String, f64>, mode: spectrum::Mode) -> f64 {
    width_memory
        .get(mode.label())
        .copied()
        .unwrap_or_else(|| spectrum::default_width_hz(mode))
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum SettingsTab {
    Network,
    Agc,
    Spectrum,
    Tx,
    PaCalibration,
    PureSignal,
}

/// A receiver beyond the first, shown in its own native OS window (P2
/// only). Deliberately simpler than the main receiver's UI -- fixed
/// level range/palette rather than full AGC-settings-window parity,
/// to keep this addition bounded in size.
struct ExtraReceiver {
    ddc_index: usize, // 1-based receiver index; 0 is the primary receiver shown in the main window
    iq_buffer: Arc<Mutex<VecDeque<IqSample>>>,
    frequency_hz: Arc<std::sync::atomic::AtomicU32>,
    sample_rate_hz: Arc<std::sync::atomic::AtomicU32>,
    adc: Arc<std::sync::atomic::AtomicU32>,
    num_adcs: u8,
    /// 1 or 2 -- Protocol 1 has a single shared RX/TX sample-rate
    /// register with no per-receiver override slot, unlike Protocol 2
    /// where each DDC really can run its own rate. Used to disable
    /// this receiver's own sample-rate control when it can't actually
    /// be honored independently (see render_extra_receiver_settings).
    protocol: u8,
    /// Shared with every other receiver (including the primary) --
    /// Alex's antenna relays are one physical resource, not per-DDC.
    antenna: Arc<std::sync::atomic::AtomicU32>,
    spectrum: SpectrumHandle,
    audio_output: Option<AudioOutput>,
    waterfall_texture: Option<egui::TextureHandle>,
    /// (SpectrumDisplay::revision, palette, db_low, db_high) the
    /// waterfall texture was last built from -- lets the UI skip
    /// re-cloning waterfall_rows and rebuilding/re-uploading the
    /// texture on repaints where nothing that affects its pixels has
    /// actually changed (new analyzer data, or the palette/range).
    waterfall_signature: Option<(u64, Palette, f32, f32)>,
    scroll_accum: f32,
    slider_scroll_accum: f32,
    db_low: f32,
    db_high: f32,
    waterfall_db_low: f32,
    waterfall_db_high: f32,
    waterfall_palette: Palette,
    /// See ConnectedState::spectrum_waterfall_ratio's doc comment --
    /// same thing, per extra receiver instead of shared.
    spectrum_waterfall_ratio: f32,
    show_settings_window: bool,
    settings_tab: SettingsTab,
    /// Shared with ConnectedState -- any control here that changes a
    /// setting flips this, so the root window's per-frame save (which
    /// is the only place with convenient access to build the full
    /// Config) knows to persist it too.
    settings_dirty: Arc<std::sync::atomic::AtomicBool>,
    band_memory: std::collections::HashMap<String, BandSettings>,
    /// Last filter width used per mode -- see width_for_mode's doc
    /// comment. Keyed by Mode::label().
    width_memory: std::collections::HashMap<String, f64>,
    /// CTUN ("Click to Tune") -- see ConnectedState::ctun's doc comment
    /// for the full explanation; same behavior here, just per extra
    /// receiver instead of shared across the whole session.
    ctun: bool,
    ctun_frequency_hz: u32,
    open: bool,
}

struct ConnectedState {
    device: Device,
    session: RadioSession,
    spectrum: SpectrumHandle,
    audio_output: Option<AudioOutput>,
    rigctl_server: Option<RigctlServer>,
    tci_server: Option<TciServer>,
    waterfall_texture: Option<egui::TextureHandle>,
    /// (SpectrumDisplay::revision, palette, db_low, db_high) the
    /// waterfall texture was last built from -- lets the UI skip
    /// re-cloning waterfall_rows and rebuilding/re-uploading the
    /// texture on repaints where nothing that affects its pixels has
    /// actually changed (new analyzer data, or the palette/range).
    waterfall_signature: Option<(u64, Palette, f32, f32)>,
    scroll_accum: f32,
    zoom_accum: f32,
    sample_rate: u32,
    db_low: f32,
    db_high: f32,
    waterfall_db_low: f32,
    waterfall_db_high: f32,
    /// Spectrum/waterfall display range while transmitting -- see
    /// Config's field docs for why these are separate from the RX
    /// ones above rather than a fixed offset applied at render time.
    tx_db_low: f32,
    tx_db_high: f32,
    tx_waterfall_db_low: f32,
    tx_waterfall_db_high: f32,
    waterfall_palette: Palette,
    /// Spectrum's share (0.0-1.0) of the combined spectrum+waterfall
    /// height, adjustable via the drag handle between them -- see
    /// Config::spectrum_waterfall_ratio's doc comment.
    spectrum_waterfall_ratio: f32,
    slider_scroll_accum: f32,
    show_settings_window: bool,
    settings_tab: SettingsTab,
    extra_receivers: Vec<Arc<Mutex<ExtraReceiver>>>,
    settings_dirty: Arc<std::sync::atomic::AtomicBool>,
    band_memory: std::collections::HashMap<String, BandSettings>,
    /// Last filter width used per mode -- see width_for_mode's doc
    /// comment. Keyed by Mode::label().
    width_memory: std::collections::HashMap<String, f64>,
    /// CTUN ("Click to Tune"): when on, the hardware/LO frequency
    /// (session.frequency_hz) stays fixed and clicking/scrolling the
    /// spectrum instead moves ctun_frequency_hz -- a listen frequency
    /// within the same spectrum window -- by shifting the RXA demod
    /// chain (see spectrum::SpectrumHandle::set_ctun). Lets you browse
    /// around inside the passband without retuning the radio itself.
    /// Confirmed against a working reference (rustyHPSDR).
    ctun: bool,
    /// Only meaningful while ctun is true; kept in sync with
    /// session.frequency_hz otherwise (see resolve_tune).
    ctun_frequency_hz: u32,
    rigctl_addr: String,
    tci_addr: String,
    /// Set when a manual Start from the Network tab fails (e.g. port
    /// already in use); cleared on the next Start attempt. rigctl/TCI
    /// no longer auto-start on connect, so there's no "unavailable at
    /// startup" case to report here -- only ones the user triggered.
    rigctl_error: Option<String>,
    tci_error: Option<String>,
    /// TX is armed automatically on connect (MicInput/TxHandle created
    /// right away, PTT control visible immediately) -- this flag is
    /// still tracked (and can still be turned off mid-session via
    /// Settings -> TX) but no longer requires a manual per-session
    /// arming step by default.
    tx_enabled: bool,
    mic_input: Option<MicInput>,
    tx_handle: Option<TxHandle>,
    /// Tracks spacebar's own press/release edges for hold-to-talk PTT
    /// (separate from the MOX button, which is a plain toggle) -- see
    /// the main-panel PTT block for why this needs edge tracking rather
    /// than just mirroring mox_active().
    ptt_held: bool,
    /// Tracked here (not just on TxHandle) so it survives a
    /// disable/re-enable of TX within the same session, and so it's
    /// available to persist even while TX is currently disarmed.
    mic_gain: f32,
    /// PureSignal calibration values (Settings -> PureSignal), same
    /// "tracked here, pushed to TxHandle on change" pattern as
    /// mic_gain above -- see tx::PsParams's field docs for what each
    /// one means. `ps_enabled` is the LIVE engine on/off (tx::PsParams::
    /// enabled), distinct from `puresignal_enabled` above (which only
    /// gates the connect-time feedback-receiver wire request).
    ps_enabled: bool,
    ps_hw_peak: f64,
    ps_mox_delay: f64,
    ps_loop_delay: f64,
    ps_tx_delay_ns: f64,
    /// Per-band PA gain (dB), keyed by band name. See
    /// Config::pa_calibration and radio::drive_byte_for_watts. Resolved
    /// to the current band and pushed into session.pa_gain_db once per
    /// frame (see the freq_hz block near the top of the main update
    /// loop) rather than at each individual set_frequency call site,
    /// since there are several of those and a once-per-frame resolve is
    /// cheap and can't drift out of sync.
    pa_calibration: std::collections::HashMap<String, f32>,
    /// Upper bound (watts) for the main panel's TX Power slider. The
    /// discovery protocol only reports board *type* (Boards), not the
    /// specific radio model or its PA's actual max output -- e.g.
    /// Orion2 covers both a 100W ANAN-100D and a 200W ANAN-8000DLE, so
    /// a board-type default can't be right for both. Defaults per
    /// default_max_tx_power_watts(board) on first connect, but is
    /// persisted per-radio (Config is already keyed by MAC) once the
    /// user corrects it in Settings -> TX, so each physical radio
    /// remembers its own real limit from then on.
    max_tx_power_watts: u32,
    /// TX Power used while tuning, as a percentage of whatever the TX
    /// Power slider was set to when TUNE was pressed (pre_tune_power_watts
    /// below) -- see Config::tune_power_percent.
    tune_power_percent: u32,
    /// Whether the Tune button is currently engaged -- transient, not
    /// persisted. See the main-panel Tune button handler for the full
    /// mechanism (WDSP PostGen tone + a temporary TX Power override).
    tune_active: bool,
    /// The TX Power (watts) value to restore when Tune ends -- saved
    /// at the moment Tune is engaged, since tx_power_watts itself gets
    /// temporarily overwritten with the reduced tune wattage while
    /// tuning. None whenever tune_active is false.
    pre_tune_power_watts: Option<u32>,
    /// Exponentially-smoothed forward/reverse power ADC counts (same
    /// raw units as session.tx_forward_power/tx_reverse_power), used
    /// only for the TX meter display -- NOT written back to the
    /// session, which keeps carrying the true raw per-packet value (see
    /// its own doc comment) in case something else ever needs it
    /// unsmoothed.
    ///
    /// Added after confirming (radio.rs's forward-power diagnostic,
    /// plus rustyHPSDR's own source) that the raw single-packet ADC
    /// reading genuinely bounces between near-zero and full-scale on
    /// this board, independent of protocol send cadence -- rustyHPSDR
    /// shows the same raw value, but only samples it on a slow GTK
    /// display timer, whereas this UI redraws from the live atomic
    /// every egui frame (~60fps), turning normal single-sample ADC
    /// ripple into a highly visible bounce no real client would show.
    /// Every real-world wattmeter (mechanical or digital) has some
    /// ballistic damping for exactly this reason.
    smoothed_fwd_power: f32,
    smoothed_rev_power: f32,
    /// PureSignal (experimental, Phase 1 -- protocol plumbing only, see
    /// radio::RadioSettings::puresignal_enabled). Reflects what THIS
    /// session was actually started with -- editable in Settings, but
    /// only takes effect on the next connect (can't be toggled live,
    /// same reasoning as sample_rate's Add Receiver interaction: the
    /// wire-level receiver/DDC count is fixed for the life of the
    /// sender/receiver threads).
    puresignal_enabled: bool,
}

enum AppState {
    Discovering(DiscoveryWindow),
    Connected(ConnectedState),
    Error(String),
}

struct HpsdrApp {
    state: AppState,
}

impl HpsdrApp {
    fn new(ctx: &egui::Context) -> Self {
        Self {
            state: AppState::Discovering(DiscoveryWindow::new(ctx)),
        }
    }
}

impl eframe::App for HpsdrApp {
    // eframe 0.35 replaced `update(&Context)` with `ui(&mut Ui)` -- see
    // https://github.com/emilk/egui/blob/main/CHANGELOG.md (0.35.0).
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        match &mut self.state {
            AppState::Discovering(window) => match window.show(ui) {
                DiscoveryAction::Start(device) => {
                    let cfg = Config::load(device.mac);
                    let mut settings = RadioSettings::default();
                    if let Some(sr) = cfg.sample_rate {
                        settings.sample_rate = sr;
                    }
                    // Pre-size for multiple receivers, per whatever the
                    // radio's own discovery reply reported supporting --
                    // both protocols now genuinely support independent
                    // per-receiver tuning (P1: start_protocol1's
                    // extra_frequencies_hz + p1_build_packet's
                    // ozy_command==2 branch; this used to be gated to P2
                    // only, which is why iq_buffers/Add Receiver stayed
                    // stuck at 1 for every P1 radio regardless of what it
                    // actually supports).
                    settings.receivers = device.supported_receivers.max(1);
                    settings.puresignal_enabled = cfg.puresignal_enabled.unwrap_or(false);
                    if let Some(atten) = cfg.rx_attenuation {
                        settings.rx_attenuation = atten;
                    }
                    match RadioSession::start(&device, settings) {
                        Ok(session) => {
                            // Override RadioSession::start's hardcoded
                            // conservative default with whatever was
                            // last saved, if anything -- otherwise TX
                            // would reset to a token power level every
                            // single session despite this now being
                            // persisted.
                            session
                                .tx_power_watts
                                .store(cfg.tx_power_watts.unwrap_or(2), Ordering::Relaxed);
                            let spectrum = SpectrumHandle::start(
                                0,
                                Arc::clone(&session.iq_buffers[0]),
                                settings.sample_rate as i32,
                            );
                            let audio_output =
                                match AudioOutput::start(Arc::clone(&spectrum.audio_out)) {
                                    Ok(a) => Some(a),
                                    Err(e) => {
                                        eprintln!("audio output unavailable: {e}");
                                        None
                                    }
                                };
                            let rigctl_addr =
                                cfg.rigctl_addr.clone().unwrap_or_else(|| rigctl::DEFAULT_ADDR.to_string());
                            let tci_addr =
                                cfg.tci_addr.clone().unwrap_or_else(|| tci::DEFAULT_ADDR.to_string());
                            // rigctl/TCI are started/stopped manually from the Network
                            // settings tab rather than always-on, but their run state
                            // is still persisted -- so a fresh connect restores
                            // whichever ones were actually running last time, instead
                            // of always coming back stopped (or always grabbing the
                            // port regardless of whether the user ever used them).
                            let mut rigctl_server: Option<RigctlServer> = None;
                            let mut rigctl_error: Option<String> = None;
                            if cfg.rigctl_running.unwrap_or(false) {
                                rigctl_server = match RigctlServer::start(
                                    &rigctl_addr,
                                    Arc::clone(&session.frequency_hz),
                                    spectrum.demod_params_handle(),
                                    Arc::clone(&session.mox),
                                ) {
                                    Ok(s) => Some(s),
                                    Err(e) => {
                                        let msg = format!("couldn't listen on {rigctl_addr}: {e}");
                                        eprintln!("rigctl: {msg}");
                                        rigctl_error = Some(msg);
                                        None
                                    }
                                };
                            }
                            let mut tci_server: Option<TciServer> = None;
                            let mut tci_error: Option<String> = None;
                            if cfg.tci_running.unwrap_or(false) {
                                tci_server = match TciServer::start(
                                    &tci_addr,
                                    Arc::clone(&session.frequency_hz),
                                    Arc::clone(&session.sample_rate),
                                    spectrum.demod_params_handle(),
                                    Arc::clone(&session.mox),
                                    Arc::clone(&spectrum.tci_audio_out),
                                    Arc::clone(&spectrum.iq_out),
                                    Arc::clone(&session.tci_tx_audio),
                                ) {
                                    Ok(s) => Some(s),
                                    Err(e) => {
                                        let msg = format!("couldn't listen on {tci_addr}: {e}");
                                        eprintln!("tci: {msg}");
                                        tci_error = Some(msg);
                                        None
                                    }
                                };
                            }
                            if let Some(f) = cfg.frequency_hz {
                                session.set_frequency(f);
                            }
                            if let Some(a) = cfg.adc {
                                session.adc.store(a as u32, Ordering::Relaxed);
                            }
                            if let Some(a) = cfg.antenna {
                                session.antenna.store(a as u32, Ordering::Relaxed);
                            }
                            if let Some(m) = cfg.mode {
                                spectrum.set_mode(m);
                            }
                            if let Some(w) = cfg.width_hz {
                                spectrum.set_width_hz(w);
                            }
                            if let Some(g) = cfg.gain {
                                spectrum.set_gain(g);
                            }
                            if let Some(a) = cfg.agc {
                                spectrum.set_agc(a);
                            }
                            if let Some(v) = cfg.agc_attack_ms {
                                spectrum.set_agc_attack_ms(v);
                            }
                            if let Some(v) = cfg.agc_decay_ms {
                                spectrum.set_agc_decay_ms(v);
                            }
                            if let Some(v) = cfg.agc_hang_ms {
                                spectrum.set_agc_hang_ms(v);
                            }
                            if let Some(v) = cfg.agc_top_db {
                                spectrum.set_agc_top_db(v);
                            }
                            if let Some(v) = cfg.agc_slope_db {
                                spectrum.set_agc_slope_db(v);
                            }
                            if let Some(v) = cfg.agc_thresh_db {
                                spectrum.set_agc_thresh_db(v);
                            }
                            if let Some(v) = cfg.noise_blanker {
                                spectrum.set_noise_blanker(v);
                            }
                            if let Some(v) = cfg.nb_threshold {
                                spectrum.set_nb_threshold(v);
                            }
                            if let Some(v) = cfg.noise_reduction {
                                spectrum.set_noise_reduction(v);
                            }
                            if let Some(v) = cfg.snb {
                                spectrum.set_snb(v);
                            }
                            let mic_gain = cfg.mic_gain.unwrap_or(0.5);
                            // See tx::PsParams::default for these same
                            // fallback values -- kept in sync deliberately
                            // (both are "reference default if never
                            // calibrated", just one's Config's fallback,
                            // one's PsParams's fallback for a session that
                            // skips Config loading entirely).
                            let ps_enabled = true;
                            let ps_hw_peak = cfg.ps_hw_peak.unwrap_or(0.2899);
                            let ps_mox_delay = cfg.ps_mox_delay.unwrap_or(0.2);
                            let ps_loop_delay = cfg.ps_loop_delay.unwrap_or(0.0);
                            let ps_tx_delay_ns = cfg.ps_tx_delay_ns.unwrap_or(150.0);

                            let settings_dirty = Arc::new(std::sync::atomic::AtomicBool::new(false));
                            let mut extra_receivers = Vec::new();
                            for saved in &cfg.extra_receivers {
                                if let Some(rx) = spawn_extra_receiver(
                                    &session,
                                    device.adcs,
                                    device.protocol,
                                    Arc::clone(&settings_dirty),
                                    Some(saved),
                                ) {
                                    extra_receivers.push(rx);
                                }
                            }

                            // TX is now armed automatically on connect, at the
                            // user's request, rather than requiring the
                            // "Enable Transmit" checkbox each session. Mirrors
                            // that checkbox's logic exactly (see Settings ->
                            // TX) -- the checkbox itself is still there and
                            // can still be used to disarm mid-session if
                            // wanted; this just changes the default from off
                            // to on rather than removing the control.
                            let duc_rate = if device.protocol == 2 {
                                192_000
                            } else {
                                settings.sample_rate as i32
                            };
                            let mic_buffer = Arc::new(Mutex::new(VecDeque::new()));
                            let (tx_enabled, mic_input, tx_handle) = match MicInput::start(Arc::clone(&mic_buffer)) {
                                Ok(mic) => {
                                    let tx_handle = TxHandle::start(
                                        mic_buffer,
                                        Arc::clone(&session.tci_tx_audio),
                                        Arc::clone(&session.tx_iq),
                                        Arc::clone(&session.mox),
                                        session.iq_buffers.len() as i32,
                                        device.protocol,
                                        48_000,
                                        duc_rate,
                                        settings.puresignal_enabled,
                                        Arc::clone(&session.ps_rx_feedback_iq),
                                        Arc::clone(&session.ps_tx_feedback_iq),
                                    );
                                    tx_handle.set_mic_gain(mic_gain);
                                    tx_handle.set_mode(spectrum.mode());
                                    tx_handle.set_width_hz(spectrum.width_hz());
                                    tx_handle.set_ps_enabled(ps_enabled);
                                    tx_handle.set_ps_hw_peak(ps_hw_peak);
                                    tx_handle.set_ps_mox_delay(ps_mox_delay);
                                    tx_handle.set_ps_loop_delay(ps_loop_delay);
                                    tx_handle.set_ps_tx_delay_ns(ps_tx_delay_ns);
                                    (true, Some(mic), Some(tx_handle))
                                }
                                Err(e) => {
                                    eprintln!("mic input unavailable, TX not armed: {e}");
                                    (false, None, None)
                                }
                            };

                            println!(
                                "Started {:?} at {} (protocol {}, {} ADC(s), reports supporting {} receiver(s))",
                                device.board,
                                device.address.ip(),
                                device.protocol,
                                device.adcs,
                                device.supported_receivers,
                            );
                            let initial_frequency_hz = session.frequency_hz.load(Ordering::Relaxed);
                            self.state = AppState::Connected(ConnectedState {
                                device,
                                session,
                                spectrum,
                                audio_output,
                                rigctl_server,
                                tci_server,
                                waterfall_texture: None,
                                waterfall_signature: None,
                                scroll_accum: 0.0,
                                zoom_accum: 0.0,
                                sample_rate: settings.sample_rate,
                                db_low: cfg.db_low.unwrap_or(-140.0),
                                db_high: cfg.db_high.unwrap_or(-40.0),
                                waterfall_db_low: cfg.waterfall_db_low.unwrap_or(-140.0),
                                waterfall_db_high: cfg.waterfall_db_high.unwrap_or(-60.0),
                                tx_db_low: cfg.tx_db_low.unwrap_or(cfg.db_low.unwrap_or(-140.0)),
                                tx_db_high: cfg.tx_db_high.unwrap_or(cfg.db_high.unwrap_or(-40.0) + 60.0),
                                tx_waterfall_db_low: cfg
                                    .tx_waterfall_db_low
                                    .unwrap_or(cfg.waterfall_db_low.unwrap_or(-140.0)),
                                tx_waterfall_db_high: cfg
                                    .tx_waterfall_db_high
                                    .unwrap_or(cfg.waterfall_db_high.unwrap_or(-60.0) + 60.0),
                                waterfall_palette: cfg.waterfall_palette.unwrap_or(Palette::Ocean),
                                spectrum_waterfall_ratio: cfg
                                    .spectrum_waterfall_ratio
                                    .unwrap_or(150.0 / 350.0),
                                slider_scroll_accum: 0.0,
                                show_settings_window: false,
                                settings_tab: SettingsTab::Agc,
                                extra_receivers,
                                settings_dirty,
                                band_memory: cfg.band_settings.clone(),
                                width_memory: cfg.width_memory.clone(),
                                ctun: false,
                                ctun_frequency_hz: initial_frequency_hz,
                                rigctl_addr,
                                tci_addr,
                                rigctl_error,
                                tci_error,
                                tx_enabled,
                                mic_input,
                                tx_handle,
                                ptt_held: false,
                                mic_gain,
                                ps_enabled,
                                ps_hw_peak,
                                ps_mox_delay,
                                ps_loop_delay,
                                ps_tx_delay_ns,
                                pa_calibration: cfg.pa_calibration.clone(),
                                max_tx_power_watts: cfg
                                    .max_tx_power_watts
                                    .unwrap_or_else(|| default_max_tx_power_watts(device.board)),
                                tune_power_percent: cfg.tune_power_percent.unwrap_or(20),
                                tune_active: false,
                                pre_tune_power_watts: None,
                                smoothed_fwd_power: 0.0,
                                smoothed_rev_power: 0.0,
                                puresignal_enabled: settings.puresignal_enabled,
                            });
                        }
                        Err(e) => {
                            self.state = AppState::Error(format!("Failed to start radio: {e}"));
                        }
                    }
                }
                DiscoveryAction::Cancelled => {
                    self.state = AppState::Error("Discovery cancelled.".to_string());
                }
                DiscoveryAction::None => {}
            },
            AppState::Connected(connected) => {
                // Shown in the OS window title bar rather than as an
                // in-UI heading -- frees up vertical space for the
                // spectrum/waterfall, which is at a premium in the
                // main window (see the initial-size note in main()).
                let window_title = format!(
                    "hpsdr-rs -- {:?} at {}",
                    connected.device.board,
                    connected.device.address.ip()
                );
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Title(window_title));
                // None = not running, Some(false) = listening/idle,
                // Some(true) = a client is currently connected. Drives
                // the gray/green/red status text in the main panel.
                let rigctl_status: Option<bool> = connected.rigctl_server.as_ref().map(|s| s.is_connected());
                let tci_status: Option<bool> = connected.tci_server.as_ref().map(|s| s.is_connected());
                let freq_hz = connected
                    .session
                    .frequency_hz
                    .load(std::sync::atomic::Ordering::Relaxed);
                // Protocol 1 only (see p1_drive_byte_for_watts); harmless
                // no-op to compute/store on P2 too rather than special-
                // casing it here.
                connected.session.pa_gain_db.store(
                    resolved_pa_gain_db(&connected.pa_calibration, freq_hz).to_bits(),
                    std::sync::atomic::Ordering::Relaxed,
                );
                let sample_rate = connected.sample_rate;
                let current_mode = connected.spectrum.mode();
                let current_width = connected.spectrum.width_hz();
                // Reused by resolve_tune (clamping a CTUN target so the
                // passband stays fully on-screen) and by the passband
                // overlay drawn below -- computed once here rather than
                // separately in both places.
                let passband = spectrum::passband_for(current_mode, current_width);
                let current_gain = connected.spectrum.gain();
                let current_agc = connected.spectrum.agc();
                let agc_params = connected.spectrum.agc_params();

                // CTUN: keep the analyzer thread's copy of the LO
                // frequency and shift offset in sync every frame,
                // regardless of whether either changed this frame --
                // cheap (a mutex lock), and means a SpectrumHandle
                // recreated elsewhere (e.g. change_sample_rate) picks up
                // the right state on its very next frame rather than
                // needing its own special-cased resync.
                let ctun_offset_hz = if connected.ctun {
                    connected.ctun_frequency_hz as f64 - freq_hz as f64
                } else {
                    0.0
                };
                connected.spectrum.set_lo_frequency_hz(freq_hz as f64);
                connected.spectrum.set_ctun(connected.ctun, ctun_offset_hz);
                let dial_freq_hz = if connected.ctun { connected.ctun_frequency_hz } else { freq_hz };

                let (spectrum_row, meter_db, waterfall_data_revision) = {
                    let d = connected.spectrum.display.lock().unwrap();
                    (d.spectrum.clone(), d.meter_db, d.revision)
                };

                // Fixed dB bounds (user-set, not auto-scaled) so the
                // Fixed dB bounds (user-set, not auto-scaled) so the
                // trace and gridlines stay put rather than shifting as
                // power levels change. Separate ranges for RX and TX
                // (Settings -> Display) -- a locally-picked-up TX
                // signal is typically far stronger than the weak RX
                // signals the RX range is normally tuned for, so they
                // need independent headroom rather than sharing one
                // range or a fixed offset applied at render time.
                let transmitting = connected.session.mox_active();
                let (rx_low, rx_high) = (connected.db_low, connected.db_high);
                let (tx_low, tx_high) = (connected.tx_db_low, connected.tx_db_high);
                let (base_low, base_high) = if transmitting { (tx_low, tx_high) } else { (rx_low, rx_high) };
                let (db_low, db_high) = if base_low < base_high {
                    (base_low, base_high)
                } else {
                    (base_high, base_high + 1.0)
                };
                let (wf_rx_low, wf_rx_high) = (connected.waterfall_db_low, connected.waterfall_db_high);
                let (wf_tx_low, wf_tx_high) = (connected.tx_waterfall_db_low, connected.tx_waterfall_db_high);
                let (wf_base_low, wf_base_high) =
                    if transmitting { (wf_tx_low, wf_tx_high) } else { (wf_rx_low, wf_rx_high) };
                let (wf_db_low, wf_db_high) = if wf_base_low < wf_base_high {
                    (wf_base_low, wf_base_high)
                } else {
                    (wf_base_high, wf_base_high + 1.0)
                };

                // Texture update needs &egui::Context, so do it before
                // opening the panel closure (same reasoning as the
                // borrow-checker fix earlier: don't mix reading
                // connected's fields with reassigning self.state inside
                // one closure). Only actually re-clone the row history
                // and rebuild/re-upload the texture -- by far the most
                // expensive things done per frame here -- when the
                // analyzer produced new data or the palette/range
                // changed; egui can repaint far more often than the
                // analyzer's own ~10Hz update rate, and redoing this
                // work on every one of those repaints for no reason was
                // enough to peg a CPU core.
                let wanted_signature = (waterfall_data_revision, connected.waterfall_palette, wf_db_low, wf_db_high);
                if connected.waterfall_signature != Some(wanted_signature) {
                    let waterfall_rows: Vec<Vec<f32>> = {
                        let d = connected.spectrum.display.lock().unwrap();
                        d.waterfall_rows.iter().cloned().collect()
                    };
                    let waterfall_image =
                        build_waterfall_image(&waterfall_rows, connected.waterfall_palette, wf_db_low, wf_db_high);
                    if let Some(image) = &waterfall_image {
                        match &mut connected.waterfall_texture {
                            Some(tex) => tex.set(image.clone(), egui::TextureOptions::LINEAR),
                            None => {
                                let tex = ui.ctx().load_texture(
                                    "waterfall",
                                    image.clone(),
                                    egui::TextureOptions::LINEAR,
                                );
                                connected.waterfall_texture = Some(tex);
                            }
                        }
                        connected.waterfall_signature = Some(wanted_signature);
                    }
                    // else: no rows yet (still computing FFTW wisdom on
                    // first run) -- leave waterfall_signature unset so
                    // this retries (cheaply -- build_waterfall_image
                    // bails out immediately on empty rows) next frame.
                }
                let waterfall_texture_id = connected.waterfall_texture.as_ref().map(|t| t.id());

                let mut stop_clicked = false;
                let mut settings_changed = false;

                egui::CentralPanel::default().show(ui, |ui| {
                    ui.add_space(4.0);
                    let freq_label = ui
                        .add(
                            egui::Label::new(
                                egui::RichText::new(format_frequency(dial_freq_hz))
                                    .monospace()
                                    .size(28.0)
                                    .strong()
                                    .color(egui::Color32::GREEN),
                            )
                            .sense(egui::Sense::hover()),
                        )
                        .on_hover_text("Scroll to tune -- Shift: 100 Hz, Ctrl: 10 kHz, none: 1 kHz");

                    ui.add_space(8.0);
                    ui.horizontal_wrapped(|ui| {
                        let current_band = band_for_frequency(dial_freq_hz).map(|b| b.name);
                        for band in &BANDS {
                            let selected = Some(band.name) == current_band;
                            if ui.add(egui::Button::selectable(selected, band.name)).clicked() && !selected {
                                let saved = connected.band_memory.get(band.name).copied();
                                let target = saved.map(|s| s.frequency_hz).unwrap_or(band.default_hz);
                                connected.session.set_frequency(target);
                                // Keep CTUN on but re-center it at the
                                // new band's frequency (offset 0) rather
                                // than carrying over an offset that made
                                // sense for the old band.
                                connected.ctun_frequency_hz = target;
                                if let Some(s) = saved {
                                    connected.db_low = s.db_low;
                                    connected.db_high = s.db_high;
                                    connected.waterfall_db_low = s.waterfall_db_low;
                                    connected.waterfall_db_high = s.waterfall_db_high;
                                }
                                // Restore whatever mode was last used on
                                // this band, if any -- falls back to the
                                // band's own default_mode the first time
                                // it's visited. Width follows the mode
                                // (width_for_mode's own per-mode memory),
                                // not a per-band value.
                                let resolved_mode =
                                    saved.and_then(|s| s.mode).unwrap_or(band.default_mode);
                                remember_band_settings(
                                    &mut connected.band_memory,
                                    target,
                                    connected.db_low,
                                    connected.db_high,
                                    connected.waterfall_db_low,
                                    connected.waterfall_db_high,
                                    resolved_mode,
                                );
                                connected.spectrum.set_mode(resolved_mode);
                                let resolved_width_hz =
                                    width_for_mode(&connected.width_memory, resolved_mode);
                                connected.spectrum.set_width_hz(resolved_width_hz);
                                if let Some(tx) = &connected.tx_handle {
                                    tx.set_mode(resolved_mode);
                                    tx.set_width_hz(resolved_width_hz);
                                }
                                settings_changed = true;
                            }
                        }
                    });

                    ui.horizontal_wrapped(|ui| {
                        for mode in ALL_MODES {
                            let selected = mode == current_mode;
                            if ui
                                .add(egui::Button::selectable(selected, mode.label()))
                                .clicked()
                                && !selected
                            {
                                connected.spectrum.set_mode(mode);
                                let mode_width_hz = width_for_mode(&connected.width_memory, mode);
                                connected.spectrum.set_width_hz(mode_width_hz);
                                if let Some(tx) = &connected.tx_handle {
                                    tx.set_mode(mode);
                                    tx.set_width_hz(mode_width_hz);
                                }
                                remember_band_settings(
                                    &mut connected.band_memory,
                                    dial_freq_hz,
                                    connected.db_low,
                                    connected.db_high,
                                    connected.waterfall_db_low,
                                    connected.waterfall_db_high,
                                    mode,
                                );
                                settings_changed = true;
                            }
                        }

                        ui.add_space(12.0);
                        ui.label("Filter width:");
                        let mut width = current_width;
                        if scroll_slider_f64(
                            ui,
                            &mut connected.slider_scroll_accum,
                            &mut width,
                            50.0..=5000.0,
                            50.0,
                            " Hz",
                        ) {
                            connected.spectrum.set_width_hz(width);
                            if let Some(tx) = &connected.tx_handle {
                                tx.set_width_hz(width);
                            }
                            connected
                                .width_memory
                                .insert(current_mode.label().to_string(), width);
                            settings_changed = true;
                        }
                    });

                    ui.horizontal_wrapped(|ui| {
                        ui.label("Audio gain:");
                        let mut gain = current_gain;
                        if scroll_slider_f32(
                            ui,
                            &mut connected.slider_scroll_accum,
                            &mut gain,
                            0.0..=1.5,
                            0.05,
                        ) {
                            connected.spectrum.set_gain(gain);
                            settings_changed = true;
                        }

                        if connected.tx_enabled {
                            if connected.tx_handle.is_some() {
                                ui.add_space(12.0);
                                ui.label("Mic gain:");
                                let mut mic_gain = connected.mic_gain;
                                if scroll_slider_f32(
                                    ui,
                                    &mut connected.slider_scroll_accum,
                                    &mut mic_gain,
                                    0.0..=2.0,
                                    0.05,
                                ) {
                                    connected.mic_gain = mic_gain;
                                    if let Some(tx) = &connected.tx_handle {
                                        tx.set_mic_gain(mic_gain);
                                    }
                                    settings_changed = true;
                                }
                            }
                            // Neither protocol's wire-level drive byte is
                            // linear with actual output watts on real
                            // hardware (P1's is confirmed non-linear against
                            // a reference; P2's byte itself is a confirmed
                            // linear 0-255 field, but that's a statement
                            // about the wire format, not about how a real PA
                            // responds to it). Both protocols now compute
                            // their drive byte from the same watts-target +
                            // per-band-gain curve (see
                            // radio::drive_byte_for_watts) rather than
                            // exposing a raw 0-255 slider -- P2 used to
                            // expose the raw byte directly here, which
                            // worked but couldn't be calibrated to match a
                            // real wattmeter reading the way P1's watts
                            // slider already could.
                            ui.add_space(12.0);
                            ui.label("TX Power:");
                            // Adjustable during Tune too, not just
                            // normal TX -- Tune Power only sets the
                            // starting reduced level when TUNE is
                            // pressed (see the Tune button handler), it
                            // doesn't keep re-enforcing a ratio, so
                            // adjusting here works exactly like normal
                            // operation while tuning.
                            let mut watts =
                                connected.session.tx_power_watts.load(Ordering::Relaxed) as i32;
                            if scroll_slider_i32(
                                ui,
                                &mut connected.slider_scroll_accum,
                                &mut watts,
                                0..=connected.max_tx_power_watts as i32,
                                1,
                                "W",
                            ) {
                                connected.session.tx_power_watts.store(watts as u32, Ordering::Relaxed);
                                settings_changed = true;
                            }
                        }
                    });

                    // Moved here from Settings -> TX (still shown there
                    // too) so it's visible alongside the TX power/SWR
                    // gauge without needing a separate window open --
                    // added specifically to help tell apart "ALC is
                    // pumping / mic is clipping on real modulated audio"
                    // from a buffering/timing issue when a reported
                    // power swing (steady on Tune's flat tone, bouncing
                    // on a real WSJT-X transmission) didn't correlate
                    // with any DUC IQ queue/mic buffer underrun log.
                    if connected.tx_enabled {
                        if let Some(tx) = &connected.tx_handle {
                            let disp = *tx.display.lock().unwrap();
                            ui.weak(format!("Mic level: {:.3}    ALC: {:.1}", disp.mic_pk, disp.alc_av));
                        }
                    }

                    ui.horizontal_wrapped(|ui| {
                        if ui
                            .add(egui::Button::selectable(connected.ctun, "CTUN"))
                            .on_hover_text(
                                "Click to Tune: browse within the spectrum without retuning the radio",
                            )
                            .clicked()
                        {
                            if connected.ctun {
                                // Turning off: commit the CTUN'd listen
                                // frequency as the new hardware/LO
                                // frequency, so listening continues
                                // uninterrupted at the same real
                                // frequency rather than snapping back.
                                connected.session.set_frequency(connected.ctun_frequency_hz);
                            } else {
                                connected.ctun_frequency_hz = freq_hz;
                            }
                            connected.ctun = !connected.ctun;
                            settings_changed = true;
                        }
                        let nb = connected.spectrum.noise_blanker();
                        if ui
                            .add(egui::Button::selectable(nb != spectrum::NoiseBlanker::Off, nb.label()))
                            .on_hover_text("Click to cycle: Off -> NB -> NB2 -> Off")
                            .clicked()
                        {
                            connected.spectrum.set_noise_blanker(nb.next());
                            settings_changed = true;
                        }
                        let nr = connected.spectrum.noise_reduction();
                        if ui
                            .add(egui::Button::selectable(nr != spectrum::NoiseReduction::Off, nr.label()))
                            .on_hover_text("Click to cycle: Off -> NR -> NR2 -> NR3 -> NR4 -> Off")
                            .clicked()
                        {
                            connected.spectrum.set_noise_reduction(nr.next());
                            settings_changed = true;
                        }
                        let snb = connected.spectrum.snb();
                        if ui
                            .add(egui::Button::selectable(snb, "SNB"))
                            .on_hover_text(
                                "Spectral Noise Blanker -- independent of NB/NR, can run alongside them",
                            )
                            .clicked()
                        {
                            connected.spectrum.set_snb(!snb);
                            settings_changed = true;
                        }
                        if ui
                            .add(egui::Button::selectable(
                                current_agc != spectrum::Agc::Off,
                                current_agc.label(),
                            ))
                            .on_hover_text("Click to cycle: Off -> Long -> Slow -> Medium -> Fast -> Off")
                            .clicked()
                        {
                            connected.spectrum.set_agc(current_agc.next());
                            settings_changed = true;
                        }
                    });

                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.colored_label(network_status_color(rigctl_status), "rigctl")
                            .on_hover_text(network_status_hover("rigctl", rigctl_status, &connected.rigctl_addr));
                        ui.add_space(12.0);
                        ui.colored_label(network_status_color(tci_status), "TCI")
                            .on_hover_text(network_status_hover("TCI", tci_status, &connected.tci_addr));
                    });

                    if connected.tx_enabled {
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            // Read the session's actual MOX state for
                            // display (not just connected.ptt_held),
                            // since rigctl/TCI can also assert PTT --
                            // e.g. WSJT-X keying up should show as
                            // transmitting here even though nothing
                            // touched this button.
                            let mox_now = connected.session.mox_active();
                            let mox_label = if mox_now { "MOX ON" } else { "MOX" };
                            let mox_color = if mox_now {
                                egui::Color32::from_rgb(210, 50, 50)
                            } else {
                                egui::Color32::from_gray(60)
                            };
                            // Click-to-toggle rather than hold-to-talk:
                            // most CAT-driven operation (WSJT-X etc.)
                            // and typical ham software convention key
                            // via rigctl/TCI or a spacebar hold, not a
                            // mouse hold -- a toggle is what's actually
                            // useful for a mouse-driven on-screen
                            // control, especially for transmissions
                            // that run many seconds (holding a mouse
                            // button that long is impractical).
                            let mox_resp = ui
                                .add_sized(
                                    [90.0, 32.0],
                                    egui::Button::new(
                                        egui::RichText::new(mox_label).strong().color(egui::Color32::WHITE),
                                    )
                                    .fill(mox_color),
                                )
                                .on_hover_text("Click to toggle transmit on/off");
                            if mox_resp.clicked() {
                                connected.session.set_mox(!mox_now);
                            }

                            // Tune: WDSP PostGen tone at passband
                            // center, replacing mic audio, at a
                            // reduced/configurable power (Settings ->
                            // TX, "Tune Power") -- see tx.rs's
                            // TxParams::tune and config.rs's
                            // tune_power_percent doc comments for the
                            // full mechanism. Disabled (can't be
                            // clicked to START) whenever something
                            // else already has MOX asserted -- e.g.
                            // WSJT-X/rigctl mid-transmission -- so
                            // Tune can't hijack an externally-keyed
                            // transmission; still clickable to turn
                            // OFF if tune itself is what's currently
                            // keying.
                            let tune_may_start = !mox_now || connected.tune_active;
                            let tune_label = if connected.tune_active { "TUNE ON" } else { "TUNE" };
                            let tune_color = if connected.tune_active {
                                egui::Color32::from_rgb(230, 140, 20)
                            } else {
                                egui::Color32::from_gray(60)
                            };
                            let tune_resp = ui
                                .add_enabled(
                                    tune_may_start,
                                    egui::Button::new(
                                        egui::RichText::new(tune_label)
                                            .strong()
                                            .color(egui::Color32::WHITE),
                                    )
                                    .fill(tune_color),
                                )
                                .on_hover_text(
                                    "Click to toggle a steady tone centered in the passband, \
                                     at Tune Power (Settings -> TX), for antenna/PA tuning",
                                );
                            if tune_resp.clicked() {
                                if connected.tune_active {
                                    connected.session.set_mox(false);
                                    if let Some(tx) = &connected.tx_handle {
                                        tx.set_tune(false);
                                    }
                                    if let Some(prev) = connected.pre_tune_power_watts.take() {
                                        connected.session.tx_power_watts.store(prev, Ordering::Relaxed);
                                    }
                                    connected.tune_active = false;
                                } else {
                                    let current_watts =
                                        connected.session.tx_power_watts.load(Ordering::Relaxed);
                                    connected.pre_tune_power_watts = Some(current_watts);
                                    // Applied once, as a safety-reduced
                                    // starting point -- NOT continuously
                                    // re-enforced, so the TX Power slider
                                    // stays fully adjustable during tune
                                    // (see below, no more add_enabled_ui
                                    // wrapper) rather than fighting a
                                    // per-frame override.
                                    let tune_watts = current_watts * connected.tune_power_percent / 100;
                                    connected.session.tx_power_watts.store(tune_watts, Ordering::Relaxed);
                                    if let Some(tx) = &connected.tx_handle {
                                        tx.set_tune(true);
                                    }
                                    connected.session.set_mox(true);
                                    connected.tune_active = true;
                                }
                            }

                            // Safety net: if something else cleared
                            // MOX while tune was active (TX disarmed,
                            // an external CAT client, etc.), clean up
                            // rather than leaving the button stuck
                            // showing "TUNE ON" while nothing is
                            // actually transmitting.
                            if connected.tune_active && !connected.session.mox_active() {
                                if let Some(tx) = &connected.tx_handle {
                                    tx.set_tune(false);
                                }
                                if let Some(prev) = connected.pre_tune_power_watts.take() {
                                    connected.session.tx_power_watts.store(prev, Ordering::Relaxed);
                                }
                                connected.tune_active = false;
                            }

                            // Spacebar: hold-to-talk, the traditional
                            // PTT gesture (mirrors a physical
                            // footswitch/mic button) -- deliberately a
                            // different interaction style from the MOX
                            // button's toggle, since they serve
                            // different needs (long digital-mode
                            // transmissions vs. quick voice PTT). Only
                            // live when no text field currently has
                            // focus, so typing in an address box
                            // elsewhere in the UI can't accidentally
                            // key the radio. ptt_held here tracks
                            // spacebar's own press/release edges (not
                            // just "is mox on"), so it only unkeys on
                            // release if spacebar itself was what most
                            // recently keyed -- pressing spacebar while
                            // the MOX button is already latched on and
                            // then releasing it will still unkey,
                            // though; a minor interaction edge case,
                            // not a general PTT-conflict resolver.
                            let editing_text = ui.ctx().memory(|m| m.focused().is_some());
                            let space_down = !editing_text && ui.input(|i| i.key_down(egui::Key::Space));
                            if space_down && !connected.ptt_held {
                                connected.ptt_held = true;
                                connected.session.set_mox(true);
                            } else if !space_down && connected.ptt_held {
                                connected.ptt_held = false;
                                connected.session.set_mox(false);
                            }

                            if mox_now {
                                ui.colored_label(egui::Color32::from_rgb(210, 50, 50), "TRANSMITTING");
                            }
                        });
                    }

                    // Split the window's remaining vertical space between
                    // the spectrum and waterfall, according to
                    // connected.spectrum_waterfall_ratio (adjustable via
                    // the drag handle between them, see
                    // spectrum_waterfall_divider) -- so they grow with
                    // the window instead of leaving empty space below a
                    // fixed size. Reserve room for the gap+Stop button
                    // below the waterfall first, since available_height()
                    // here is everything down to the bottom of the
                    // panel, not just what's free for the spectrum alone.
                    let stop_button_reserve =
                        ui.spacing().interact_size.y + 8.0 + SPECTRUM_WATERFALL_DIVIDER_HEIGHT;
                    let spectrum_waterfall_height =
                        (ui.available_height() - stop_button_reserve).max(200.0);
                    let spectrum_height =
                        (spectrum_waterfall_height * connected.spectrum_waterfall_ratio).max(80.0);
                    let (rect, spectrum_resp) = ui.allocate_exact_size(
                        egui::vec2(ui.available_width(), spectrum_height),
                        egui::Sense::click(),
                    );

                    if let Some(pos) = spectrum_resp.interact_pointer_pos() {
                        if spectrum_resp.clicked() {
                            let new_freq = freq_at_x(pos.x, rect, freq_hz, sample_rate);
                            let (effective_freq, retune) =
                                resolve_tune(connected.ctun, freq_hz, sample_rate, passband, new_freq);
                            if let Some(lo) = retune {
                                connected.session.set_frequency(lo);
                            } else {
                                connected.ctun_frequency_hz = effective_freq;
                            }
                            remember_band_settings(&mut connected.band_memory, effective_freq, connected.db_low, connected.db_high, connected.waterfall_db_low, connected.waterfall_db_high, current_mode);
                            settings_changed = true;
                        }
                    }

                    // Scroll-to-tune: active while hovering the frequency
                    // label or the spectrum itself.
                    if freq_label.hovered() || spectrum_resp.hovered() {
                        let scroll_delta = ui.input(|i| i.smooth_scroll_delta);
                        // egui redirects vertical scroll into the x-axis
                        // while Shift is held (its convention for
                        // horizontal-scroll support elsewhere) -- so
                        // check whichever axis actually has motion
                        // rather than only .y. Ctrl+scroll doesn't reach
                        // here at all -- egui diverts it into a zoom
                        // gesture instead, handled separately below.
                        let delta = if scroll_delta.y.abs() >= scroll_delta.x.abs() {
                            scroll_delta.y
                        } else {
                            scroll_delta.x
                        };

                        if delta != 0.0 {
                            connected.scroll_accum += delta;

                            // Roughly one physical wheel "notch" on most
                            // platforms/mice -- not verified against your
                            // specific hardware, tune if steps feel too
                            // coarse or too fine. Raised from an earlier
                            // 20.0 -- reported as too sensitive (one
                            // wheel click jumping more than a single
                            // frequency step), so this now requires more
                            // accumulated scroll motion per step.
                            const NOTCH: f32 = 50.0;

                            let shift = ui.input(|i| i.modifiers.shift);
                            let step: i64 = if shift { 100 } else { 1_000 };

                            let mut new_freq = dial_freq_hz as i64;
                            while connected.scroll_accum.abs() >= NOTCH {
                                let sign = connected.scroll_accum.signum();
                                connected.scroll_accum -= sign * NOTCH;
                                new_freq += step * sign as i64;
                            }
                            new_freq = new_freq.max(0);

                            if new_freq as u32 != dial_freq_hz {
                                let (effective_freq, retune) =
                                    resolve_tune(connected.ctun, freq_hz, sample_rate, passband, new_freq as u32);
                                if let Some(lo) = retune {
                                    connected.session.set_frequency(lo);
                                } else {
                                    connected.ctun_frequency_hz = effective_freq;
                                }
                                remember_band_settings(&mut connected.band_memory, effective_freq, connected.db_low, connected.db_high, connected.waterfall_db_low, connected.waterfall_db_high, current_mode);
                                settings_changed = true;
                            }
                        }

                        // Ctrl+scroll: egui treats this as a zoom gesture
                        // and reports it via zoom_delta() (1.0 = no
                        // change) rather than smooth_scroll_delta, so it
                        // needs its own accumulate-and-threshold path.
                        let zoom = ui.input(|i| i.zoom_delta());
                        if zoom != 1.0 {
                            connected.zoom_accum += zoom - 1.0;

                            // Unverified threshold, same caveat as NOTCH
                            // above -- tune if 10kHz steps feel off.
                            const ZOOM_NOTCH: f32 = 0.05;

                            let mut new_freq = dial_freq_hz as i64;
                            while connected.zoom_accum.abs() >= ZOOM_NOTCH {
                                let sign = connected.zoom_accum.signum();
                                connected.zoom_accum -= sign * ZOOM_NOTCH;
                                new_freq += 10_000 * sign as i64;
                            }
                            new_freq = new_freq.max(0);

                            if new_freq as u32 != dial_freq_hz {
                                let (effective_freq, retune) =
                                    resolve_tune(connected.ctun, freq_hz, sample_rate, passband, new_freq as u32);
                                if let Some(lo) = retune {
                                    connected.session.set_frequency(lo);
                                } else {
                                    connected.ctun_frequency_hz = effective_freq;
                                }
                                remember_band_settings(&mut connected.band_memory, effective_freq, connected.db_low, connected.db_high, connected.waterfall_db_low, connected.waterfall_db_high, current_mode);
                                settings_changed = true;
                            }
                        }
                    }

                    ui.painter().rect_filled(rect, 0.0, egui::Color32::BLACK);

                    // Frequency axis: assumes the spectrum linearly spans
                    // the full DDC sample rate centered on the tuned
                    // frequency (standard convention at zoom=1). If the
                    // displayed span looks wrong, this assumption is the
                    // first thing to check.
                    let half_span_hz = sample_rate as f64 / 2.0;

                    // Filter passband overlay: shaded region between the
                    // current mode's filter edges, plus a line marking
                    // the dial (tuned) frequency itself. Same freq-to-x
                    // mapping as the axis ticks below.
                    let x_for_offset = |offset_hz: f64| -> f32 {
                        let frac = ((offset_hz + half_span_hz) / sample_rate as f64)
                            .clamp(0.0, 1.0) as f32;
                        rect.left() + frac * rect.width()
                    };
                    let (pb_low, pb_high) = passband;
                    let x_low = x_for_offset(pb_low + ctun_offset_hz);
                    let x_high = x_for_offset(pb_high + ctun_offset_hz);
                    ui.painter().rect_filled(
                        egui::Rect::from_min_max(
                            egui::pos2(x_low, rect.top()),
                            egui::pos2(x_high, rect.bottom()),
                        ),
                        0.0,
                        egui::Color32::from_rgba_unmultiplied(70, 150, 230, 50),
                    );
                    for x in [x_low, x_high] {
                        ui.painter().line_segment(
                            [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
                            egui::Stroke::new(1.0, egui::Color32::from_rgb(100, 180, 255)),
                        );
                    }
                    let x_dial = x_for_offset(ctun_offset_hz);

                    let num_freq_ticks = 5;
                    for t in 0..num_freq_ticks {
                        let frac = t as f32 / (num_freq_ticks - 1) as f32;
                        let x = rect.left() + frac * rect.width();
                        ui.painter().line_segment(
                            [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
                            egui::Stroke::new(1.0, egui::Color32::from_gray(55)),
                        );
                        let tick_freq_hz =
                            freq_hz as f64 - half_span_hz + frac as f64 * sample_rate as f64;
                        ui.painter().text(
                            egui::pos2(x + 2.0, rect.bottom() - 2.0),
                            egui::Align2::LEFT_BOTTOM,
                            format_khz(tick_freq_hz),
                            egui::FontId::monospace(13.0),
                            egui::Color32::GRAY,
                        );
                    }

                    if spectrum_row.len() > 1 {
                        let range = (db_high - db_low).max(1.0);

                        // Reserve space at the bottom for the frequency
                        // axis labels drawn there, so the trace/gridlines
                        // never overdraw them. Sized for the 13.0 font
                        // above, not just the older/smaller 10.0.
                        const FREQ_AXIS_MARGIN: f32 = 20.0;
                        let plot_bottom = rect.bottom() - FREQ_AXIS_MARGIN;
                        let plot_height = plot_bottom - rect.top();

                        // Power-level gridlines. Values are whatever units
                        // WDSP's log-average detector outputs -- real dB,
                        // but not calibrated to absolute dBm since the
                        // analyzer's fscLin/fscHin were left at 0.0.
                        let num_db_ticks = 4;
                        for t in 0..=num_db_ticks {
                            let frac = t as f32 / num_db_ticks as f32;
                            let y = plot_bottom - frac * plot_height;
                            ui.painter().line_segment(
                                [egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)],
                                egui::Stroke::new(1.0, egui::Color32::from_gray(55)),
                            );
                            let db = db_low + frac * range;
                            ui.painter().text(
                                egui::pos2(rect.left() + 2.0, y),
                                egui::Align2::LEFT_TOP,
                                format!("{db:.0} dB"),
                                egui::FontId::monospace(10.0),
                                egui::Color32::GRAY,
                            );
                        }

                        let n = spectrum_row.len().saturating_sub(1).max(1);
                        let points: Vec<egui::Pos2> = spectrum_row
                            .iter()
                            .enumerate()
                            .map(|(i, &v)| {
                                let x = rect.left() + (i as f32 / n as f32) * rect.width();
                                let t = ((v - db_low) / range).clamp(0.0, 1.0);
                                let y = plot_bottom - t * plot_height;
                                egui::pos2(x, y)
                            })
                            .collect();
                        ui.painter().add(egui::Shape::line(
                            points,
                            egui::Stroke::new(1.5, egui::Color32::LIGHT_GREEN),
                        ));
                    }

                    // Drawn last (on top of the trace/gridlines above)
                    // and thicker than a plain 1px stroke so it's
                    // unambiguous regardless of what's under it or the
                    // display's DPI scaling.
                    ui.painter().line_segment(
                        [egui::pos2(x_dial, rect.top()), egui::pos2(x_dial, rect.bottom())],
                        egui::Stroke::new(2.0, egui::Color32::RED),
                    );

                    if let Some(pos) = spectrum_resp.hover_pos() {
                        let hover_freq = freq_at_x(pos.x, rect, freq_hz, sample_rate);
                        draw_freq_hover_tooltip(ui.painter(), pos, hover_freq);
                    }

                    if spectrum_waterfall_divider(
                        ui,
                        &mut connected.spectrum_waterfall_ratio,
                        spectrum_waterfall_height,
                    ) {
                        settings_changed = true;
                    }
                    let waterfall_height = (spectrum_waterfall_height - spectrum_height).max(80.0);
                    let (rect, waterfall_click_resp) = ui.allocate_exact_size(
                        egui::vec2(ui.available_width(), waterfall_height),
                        egui::Sense::click(),
                    );
                    if let Some(pos) = waterfall_click_resp.interact_pointer_pos() {
                        if waterfall_click_resp.clicked() {
                            let new_freq = freq_at_x(pos.x, rect, freq_hz, sample_rate);
                            let (effective_freq, retune) =
                                resolve_tune(connected.ctun, freq_hz, sample_rate, passband, new_freq);
                            if let Some(lo) = retune {
                                connected.session.set_frequency(lo);
                            } else {
                                connected.ctun_frequency_hz = effective_freq;
                            }
                            remember_band_settings(&mut connected.band_memory, effective_freq, connected.db_low, connected.db_high, connected.waterfall_db_low, connected.waterfall_db_high, current_mode);
                            settings_changed = true;
                        }
                    }
                    if let Some(tex_id) = waterfall_texture_id {
                        ui.painter().image(
                            tex_id,
                            rect,
                            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                            egui::Color32::WHITE,
                        );
                    } else {
                        ui.painter().rect_filled(rect, 0.0, egui::Color32::BLACK);
                        ui.put(rect, egui::Label::new(wisdom_status_text()));
                    }
                    if let Some(pos) = waterfall_click_resp.hover_pos() {
                        let hover_freq = freq_at_x(pos.x, rect, freq_hz, sample_rate);
                        draw_freq_hover_tooltip(ui.painter(), pos, hover_freq);
                    }

                    ui.add_space(8.0);
                    if ui.button("Stop").clicked() {
                        stop_clicked = true;
                    }
                });

                egui::Area::new(egui::Id::new("s_meter_area"))
                    .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-10.0, 10.0))
                    .show(ui, |ui| {
                        let (meter_rect, _resp) =
                            ui.allocate_exact_size(egui::vec2(180.0, 110.0), egui::Sense::hover());
                        if connected.session.mox_active() {
                            let raw_fwd = connected
                                .session
                                .tx_forward_power
                                .load(std::sync::atomic::Ordering::Relaxed);
                            let raw_rev = connected
                                .session
                                .tx_reverse_power
                                .load(std::sync::atomic::Ordering::Relaxed);
                            // See ConnectedState::smoothed_fwd_power's doc
                            // comment -- damps normal single-sample ADC
                            // ripple (confirmed present on the raw value
                            // itself, not introduced by this UI) the same
                            // way any real wattmeter's ballistics would,
                            // rather than redrawing the raw bounce every
                            // frame. alpha=0.15 at ~60fps is roughly a
                            // 150ms time constant -- fast enough to track
                            // a real key-up ramp, slow enough to smooth
                            // frame-to-frame sample noise.
                            const SMOOTHING_ALPHA: f32 = 0.15;
                            connected.smoothed_fwd_power +=
                                SMOOTHING_ALPHA * (raw_fwd as f32 - connected.smoothed_fwd_power);
                            connected.smoothed_rev_power +=
                                SMOOTHING_ALPHA * (raw_rev as f32 - connected.smoothed_rev_power);
                            let (watts, _reverse_watts, swr) = power_watts_and_swr(
                                connected.smoothed_fwd_power as u32,
                                connected.smoothed_rev_power as u32,
                                connected.device.board,
                            );
                            draw_power_meter(ui, meter_rect, watts, swr, connected.max_tx_power_watts as f32);
                        } else {
                            // Reset so the next key-up's meter ramps from
                            // zero (like a real wattmeter's needle
                            // settling back down) instead of smoothing in
                            // from whatever the last transmission ended
                            // at.
                            connected.smoothed_fwd_power = 0.0;
                            connected.smoothed_rev_power = 0.0;
                            draw_s_meter(ui, meter_rect, meter_db);
                        }

                        ui.add_space(4.0);
                        if ui.button("Settings...").clicked() {
                            connected.show_settings_window = !connected.show_settings_window;
                        }

                        // Used to be gated to protocol == 2 only -- P1
                        // genuinely supports independent per-receiver
                        // tuning too (classic Metis/Ozy DDC round-robin),
                        // it just wasn't wired up: see start_protocol1's
                        // extra_frequencies_hz and p1_build_packet's
                        // ozy_command==2 branch for the actual fix.
                        let active =
                            connected.session.active_receiver_count.load(Ordering::Relaxed) as usize;
                        let max = connected.session.iq_buffers.len();
                        if active < max {
                            if ui.button(format!("Add Receiver ({active}/{max})")).clicked() {
                                if let Some(rx) = spawn_extra_receiver(
                                    &connected.session,
                                    connected.device.adcs,
                                    connected.device.protocol,
                                    Arc::clone(&connected.settings_dirty),
                                    None,
                                ) {
                                    connected.extra_receivers.push(rx);
                                    // Without this, a freshly added
                                    // receiver is only persisted if
                                    // some other setting happens to
                                    // change afterward -- closing the
                                    // app right after adding one
                                    // would silently lose it.
                                    connected.settings_dirty.store(true, Ordering::Relaxed);
                                }
                            }
                        } else {
                            ui.weak(format!("All {max} receivers active"));
                        }
                    });

                // One native OS window per extra receiver. Must be called
                // every frame to stay open (egui's viewport convention) --
                // dropping out of this loop (window closed) lets the Arc's
                // last reference go once removed from extra_receivers,
                // which cleanly stops that receiver's threads/audio via
                // its Drop impls.
                connected.extra_receivers.retain(|rx| rx.lock().unwrap().open);
                // active_receiver_count drives which DDCs the radio is
                // actually told to enable/stream (see p2_sender_loop's
                // contiguous DDC0..active-1 enable mask) -- it must
                // shrink back down when a receiver window closes, or
                // the radio keeps streaming a DDC nobody's reading and
                // "Add Receiver" undercounts how many slots are really
                // free. DDCs can only be enabled as a contiguous block
                // from DDC0, so the count can't drop below whatever the
                // highest still-open extra receiver's index requires --
                // e.g. closing receiver 1 while receiver 2 stays open
                // must leave DDC1 enabled too, since DDC2 can't run
                // without it.
                let active_count = connected
                    .extra_receivers
                    .iter()
                    .map(|rx| rx.lock().unwrap().ddc_index)
                    .max()
                    .map_or(1, |highest| highest + 1);
                connected
                    .session
                    .active_receiver_count
                    .store(active_count as u32, Ordering::Relaxed);
                for rx in connected.extra_receivers.clone() {
                    let ddc_index = rx.lock().unwrap().ddc_index;
                    let viewport_id = egui::ViewportId::from_hash_of(("extra_receiver", ddc_index));
                    let title = format!("Receiver {}", ddc_index + 1);
                    let rx_for_closure = Arc::clone(&rx);
                    ui.ctx().show_viewport_deferred(
                        viewport_id,
                        egui::ViewportBuilder::default()
                            .with_title(title)
                            .with_inner_size([1024.0, 500.0]),
                        move |ui: &mut egui::Ui, _class: egui::ViewportClass| {
                            if ui.input(|i| i.viewport().close_requested()) {
                                let mut rx = rx_for_closure.lock().unwrap();
                                rx.open = false;
                                // Without this, closing a receiver is
                                // only persisted if some other setting
                                // happens to change afterward --
                                // closing the app right after would
                                // silently bring it back next launch.
                                rx.settings_dirty.store(true, Ordering::Relaxed);
                                return;
                            }

                            let meter_db = rx_for_closure.lock().unwrap().spectrum.display.lock().unwrap().meter_db;
                            egui::Area::new(egui::Id::new(("extra_s_meter", ddc_index)))
                                .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-10.0, 10.0))
                                .show(ui, |ui| {
                                    let (meter_rect, _resp) =
                                        ui.allocate_exact_size(egui::vec2(180.0, 110.0), egui::Sense::hover());
                                    draw_s_meter(ui, meter_rect, meter_db);
                                });

                            egui::CentralPanel::default().show(ui, |ui| {
                                render_extra_receiver_ui(ui, &rx_for_closure);
                            });

                            let show_settings = rx_for_closure.lock().unwrap().show_settings_window;
                            if show_settings {
                                let mut still_open = show_settings;
                                egui::Window::new(format!("Receiver {} Settings", ddc_index + 1))
                                    .open(&mut still_open)
                                    .show(ui, |ui| {
                                        render_extra_receiver_settings(ui, &rx_for_closure);
                                    });
                                rx_for_closure.lock().unwrap().show_settings_window = still_open;
                            }
                        },
                    );
                }

                if connected.show_settings_window {
                    let mut still_open = connected.show_settings_window;
                    // Light theme for this window specifically (white
                    // title bar/background, dark text) rather than the
                    // app's normal dark theme -- egui only paints a
                    // window's title bar in a distinct color while it's
                    // focused/on top, and even then from the same
                    // app-wide style used elsewhere, so there's no way
                    // to whiten just the title strip on its own without
                    // it flickering back dark whenever this window
                    // isn't focused. Overriding the whole window's
                    // visuals instead keeps it consistently white
                    // regardless of focus.
                    let light_visuals = egui::Visuals::light();
                    let light_style = egui::Style { visuals: light_visuals.clone(), ..Default::default() };
                    egui::Window::new("Settings")
                        .open(&mut still_open)
                        .frame(egui::Frame::window(&light_style))
                        .show(ui, |ui| {
                            ui.visuals_mut().clone_from(&light_visuals);
                            ui.horizontal(|ui| {
                                for (tab, label) in [
                                    (SettingsTab::Network, "Network"),
                                    (SettingsTab::Agc, "RX"),
                                    (SettingsTab::Spectrum, "Spectrum"),
                                    (SettingsTab::Tx, "TX"),
                                    (SettingsTab::PaCalibration, "PA Calibration"),
                                    (SettingsTab::PureSignal, "PureSignal"),
                                ] {
                                    if ui
                                        .selectable_label(connected.settings_tab == tab, label)
                                        .clicked()
                                    {
                                        connected.settings_tab = tab;
                                    }
                                }
                            });
                            ui.separator();

                            match connected.settings_tab {
                                SettingsTab::Network => {
                                    ui.label("rigctl (for WSJT-X's \"Hamlib NET rigctl\", etc.):");
                                    ui.horizontal(|ui| {
                                        let running = connected.rigctl_server.is_some();
                                        ui.add_enabled(
                                            !running,
                                            egui::TextEdit::singleline(&mut connected.rigctl_addr),
                                        );
                                        if running {
                                            if ui.button("Stop").clicked() {
                                                connected.rigctl_server = None;
                                                settings_changed = true;
                                            }
                                        } else if ui.button("Start").clicked() {
                                            connected.rigctl_error = None;
                                            connected.rigctl_server = match RigctlServer::start(
                                                &connected.rigctl_addr,
                                                Arc::clone(&connected.session.frequency_hz),
                                                connected.spectrum.demod_params_handle(),
                                                Arc::clone(&connected.session.mox),
                                            ) {
                                                Ok(s) => Some(s),
                                                Err(e) => {
                                                    let msg = format!(
                                                        "couldn't listen on {}: {e}",
                                                        connected.rigctl_addr
                                                    );
                                                    eprintln!("rigctl: {msg}");
                                                    connected.rigctl_error = Some(msg);
                                                    None
                                                }
                                            };
                                            settings_changed = true;
                                        }
                                    });
                                    ui.weak(if connected.rigctl_server.is_some() {
                                        "Running -- Stop before changing the address."
                                    } else {
                                        "Stopped"
                                    });
                                    if let Some(err) = &connected.rigctl_error {
                                        ui.colored_label(egui::Color32::from_rgb(220, 60, 60), err);
                                    }

                                    ui.add_space(8.0);
                                    ui.label("TCI (Transceiver Control Interface):");
                                    ui.horizontal(|ui| {
                                        let running = connected.tci_server.is_some();
                                        ui.add_enabled(
                                            !running,
                                            egui::TextEdit::singleline(&mut connected.tci_addr),
                                        );
                                        if running {
                                            if ui.button("Stop").clicked() {
                                                connected.tci_server = None;
                                                settings_changed = true;
                                            }
                                        } else if ui.button("Start").clicked() {
                                            connected.tci_error = None;
                                            connected.tci_server = match TciServer::start(
                                                &connected.tci_addr,
                                                Arc::clone(&connected.session.frequency_hz),
                                                Arc::clone(&connected.session.sample_rate),
                                                connected.spectrum.demod_params_handle(),
                                                Arc::clone(&connected.session.mox),
                                                Arc::clone(&connected.spectrum.tci_audio_out),
                                                Arc::clone(&connected.spectrum.iq_out),
                                                Arc::clone(&connected.session.tci_tx_audio),
                                            ) {
                                                Ok(s) => Some(s),
                                                Err(e) => {
                                                    let msg = format!(
                                                        "couldn't listen on {}: {e}",
                                                        connected.tci_addr
                                                    );
                                                    eprintln!("tci: {msg}");
                                                    connected.tci_error = Some(msg);
                                                    None
                                                }
                                            };
                                            settings_changed = true;
                                        }
                                    });
                                    ui.weak(if connected.tci_server.is_some() {
                                        "Running -- Stop before changing the address."
                                    } else {
                                        "Stopped"
                                    });
                                    if let Some(err) = &connected.tci_error {
                                        ui.colored_label(egui::Color32::from_rgb(220, 60, 60), err);
                                    }

                                    ui.add_space(8.0);
                                    ui.weak(
                                        "Format: address:port, e.g. 0.0.0.0:4532 (the default -- listens on \
                                         every network interface, so another machine on your network can \
                                         connect too, not just this one). Use 127.0.0.1:PORT instead to \
                                         restrict it to this machine only. Neither protocol has any \
                                         authentication, so only expose this on networks you trust. Both \
                                         are RX only -- PTT is accepted but not implemented.",
                                    );
                                }

                                SettingsTab::Agc => {
                                    ui.label("Sample Rate:");
                                    ui.horizontal_wrapped(|ui| {
                                        // Protocol 2 boards support 768/1536ksps too (encoded as
                                        // a raw ksps value in p2_ddc_specific_packet, not the
                                        // fixed 2-bit code P1 uses -- see sample_rate_code, which
                                        // only has entries up to 384000 and would silently fall
                                        // through to 48kHz for anything higher, so these extra
                                        // rates are P2-only).
                                        let rates: &[u32] = if connected.device.protocol == 2 {
                                            &[48_000, 96_000, 192_000, 384_000, 768_000, 1_536_000]
                                        } else {
                                            &[48_000, 96_000, 192_000, 384_000]
                                        };
                                        for &rate in rates {
                                            let selected = rate == connected.sample_rate;
                                            let label = format!("{}", rate / 1000);
                                            if ui
                                                .add(egui::Button::selectable(selected, label))
                                                .clicked()
                                                && !selected
                                            {
                                                change_sample_rate(connected, rate);
                                                settings_changed = true;
                                            }
                                        }
                                        ui.weak("kHz");
                                    });
                                    ui.weak(
                                        "Changing this briefly interrupts audio/spectrum while the demod chain restarts.",
                                    );
                                    ui.separator();

                                    if connected.device.protocol == 2 {
                                        let current_adc = connected.session.adc.load(Ordering::Relaxed);
                                        ui.label("ADC:");
                                        ui.horizontal_wrapped(|ui| {
                                            for adc in 0..connected.device.adcs as u32 {
                                                let selected = adc == current_adc;
                                                if ui
                                                    .add(egui::Button::selectable(selected, format!("ADC{adc}")))
                                                    .clicked()
                                                    && !selected
                                                {
                                                    connected.session.adc.store(adc, Ordering::Relaxed);
                                                    settings_changed = true;
                                                }
                                            }
                                        });

                                        if current_adc == 0 {
                                            let current_ant = connected.session.antenna.load(Ordering::Relaxed);
                                            ui.label("Antenna (shared across all ADC0 receivers):");
                                            ui.horizontal_wrapped(|ui| {
                                                for (ant, label) in [(0u32, "ANT1"), (1, "ANT2"), (2, "ANT3")] {
                                                    let selected = ant == current_ant;
                                                    if ui
                                                        .add(egui::Button::selectable(selected, label))
                                                        .clicked()
                                                        && !selected
                                                    {
                                                        connected.session.antenna.store(ant, Ordering::Relaxed);
                                                        settings_changed = true;
                                                    }
                                                }
                                            });
                                        }
                                        ui.separator();
                                    }

                                    // Protocol 1, standard (non-HermesLite) boards only --
                                    // HermesLite/HermesLite2 use a different RX gain mechanism
                                    // this project doesn't expose a control for yet (see
                                    // radio.rs's p1_build_packet command-4 doc comment), and P2
                                    // doesn't use this field at all. ROOT CAUSE FIX: this was
                                    // previously hardcoded to 0dB (no attenuation), which real
                                    // hardware testing (ANAN-100D/Angelia on an HF antenna)
                                    // confirmed causes front-end overload from ordinary band
                                    // signals -- visible as an intermod comb pattern or
                                    // sustained broadband noise depending on band conditions at
                                    // the moment, which is why it looked random between connects.
                                    if connected.device.protocol == 1
                                        && !matches!(connected.device.board, Boards::HermesLite | Boards::HermesLite2)
                                    {
                                        let mut atten =
                                            connected.session.rx_attenuation.load(Ordering::Relaxed) as i32;
                                        ui.horizontal(|ui| {
                                            ui.label("RX Attenuation:");
                                            if scroll_slider_i32(
                                                ui,
                                                &mut connected.slider_scroll_accum,
                                                &mut atten,
                                                0..=31,
                                                1,
                                                " dB",
                                            ) {
                                                connected
                                                    .session
                                                    .rx_attenuation
                                                    .store(atten as u32, Ordering::Relaxed);
                                                settings_changed = true;
                                            }
                                        });
                                        ui.weak(
                                            "Raise this if the spectrum looks garbled/overloaded on a \
                                             strong band -- 0dB is maximum sensitivity, not a safe default.",
                                        );
                                        ui.separator();
                                    }

                                    ui.horizontal_wrapped(|ui| {
                                        let mut attack = agc_params.agc_attack_ms;
                                        ui.label("Attack:");
                                        if scroll_slider_i32(
                                            ui,
                                            &mut connected.slider_scroll_accum,
                                            &mut attack,
                                            0..=20,
                                            1,
                                            " ms",
                                        ) {
                                            connected.spectrum.set_agc_attack_ms(attack);
                                            settings_changed = true;
                                        }

                                        let mut decay = agc_params.agc_decay_ms;
                                        ui.label("Decay:");
                                        if scroll_slider_i32(
                                            ui,
                                            &mut connected.slider_scroll_accum,
                                            &mut decay,
                                            0..=2000,
                                            25,
                                            " ms",
                                        ) {
                                            connected.spectrum.set_agc_decay_ms(decay);
                                            settings_changed = true;
                                        }

                                        let mut hang = agc_params.agc_hang_ms;
                                        ui.label("Hang:");
                                        if scroll_slider_i32(
                                            ui,
                                            &mut connected.slider_scroll_accum,
                                            &mut hang,
                                            0..=2000,
                                            25,
                                            " ms",
                                        ) {
                                            connected.spectrum.set_agc_hang_ms(hang);
                                            settings_changed = true;
                                        }
                                    });

                                    ui.horizontal_wrapped(|ui| {
                                        let mut top = agc_params.agc_top_db;
                                        ui.label("Top:");
                                        if scroll_slider_f64(
                                            ui,
                                            &mut connected.slider_scroll_accum,
                                            &mut top,
                                            0.0..=140.0,
                                            2.0,
                                            " dB",
                                        ) {
                                            connected.spectrum.set_agc_top_db(top);
                                            settings_changed = true;
                                        }

                                        let mut slope = agc_params.agc_slope_db;
                                        ui.label("Slope:");
                                        if scroll_slider_i32(
                                            ui,
                                            &mut connected.slider_scroll_accum,
                                            &mut slope,
                                            0..=100,
                                            2,
                                            " dB",
                                        ) {
                                            connected.spectrum.set_agc_slope_db(slope);
                                            settings_changed = true;
                                        }

                                        let mut thresh = agc_params.agc_thresh_db;
                                        ui.label("Thresh:");
                                        if scroll_slider_f64(
                                            ui,
                                            &mut connected.slider_scroll_accum,
                                            &mut thresh,
                                            -140.0..=0.0,
                                            2.0,
                                            " dB",
                                        ) {
                                            connected.spectrum.set_agc_thresh_db(thresh);
                                            settings_changed = true;
                                        }
                                    });

                                    ui.separator();
                                    ui.horizontal(|ui| {
                                        let mut nb_threshold = connected.spectrum.nb_threshold();
                                        ui.label("NB Threshold:");
                                        if scroll_slider_f64(
                                            ui,
                                            &mut connected.slider_scroll_accum,
                                            &mut nb_threshold,
                                            0.0..=100.0,
                                            1.0,
                                            "",
                                        ) {
                                            connected.spectrum.set_nb_threshold(nb_threshold);
                                            settings_changed = true;
                                        }
                                    });
                                    ui.weak("Shared by both NB and NB2 (toggle either on the main panel).");
                                }

                                SettingsTab::Spectrum => {
                                    ui.horizontal(|ui| {
                                        ui.label("Spectrum");
                                        let mut low = connected.db_low;
                                        ui.label("Low:");
                                        if scroll_slider_f32(
                                            ui,
                                            &mut connected.slider_scroll_accum,
                                            &mut low,
                                            -180.0..=0.0,
                                            2.0,
                                        ) {
                                            connected.db_low = low;
                                            remember_band_settings(
                                                &mut connected.band_memory,
                                                freq_hz,
                                                connected.db_low,
                                                connected.db_high,
                                                connected.waterfall_db_low,
                                                connected.waterfall_db_high,
                                                current_mode,
                                            );
                                            settings_changed = true;
                                        }
                                        let mut high = connected.db_high;
                                        ui.label("High:");
                                        if scroll_slider_f32(
                                            ui,
                                            &mut connected.slider_scroll_accum,
                                            &mut high,
                                            -180.0..=0.0,
                                            2.0,
                                        ) {
                                            connected.db_high = high;
                                            remember_band_settings(
                                                &mut connected.band_memory,
                                                freq_hz,
                                                connected.db_low,
                                                connected.db_high,
                                                connected.waterfall_db_low,
                                                connected.waterfall_db_high,
                                                current_mode,
                                            );
                                            settings_changed = true;
                                        }
                                    });

                                    ui.separator();
                                    ui.horizontal(|ui| {
                                        ui.label("Waterfall palette:");
                                        for palette in ALL_PALETTES {
                                            let selected = palette == connected.waterfall_palette;
                                            if ui
                                                .add(egui::Button::selectable(selected, palette.label()))
                                                .clicked()
                                            {
                                                connected.waterfall_palette = palette;
                                                settings_changed = true;
                                            }
                                        }
                                    });
                                    ui.horizontal(|ui| {
                                        let mut wlow = connected.waterfall_db_low;
                                        ui.label("Low:");
                                        if scroll_slider_f32(
                                            ui,
                                            &mut connected.slider_scroll_accum,
                                            &mut wlow,
                                            -180.0..=0.0,
                                            2.0,
                                        ) {
                                            connected.waterfall_db_low = wlow;
                                            remember_band_settings(
                                                &mut connected.band_memory,
                                                freq_hz,
                                                connected.db_low,
                                                connected.db_high,
                                                connected.waterfall_db_low,
                                                connected.waterfall_db_high,
                                                current_mode,
                                            );
                                            settings_changed = true;
                                        }
                                        let mut whigh = connected.waterfall_db_high;
                                        ui.label("High:");
                                        if scroll_slider_f32(
                                            ui,
                                            &mut connected.slider_scroll_accum,
                                            &mut whigh,
                                            -180.0..=0.0,
                                            2.0,
                                        ) {
                                            connected.waterfall_db_high = whigh;
                                            remember_band_settings(
                                                &mut connected.band_memory,
                                                freq_hz,
                                                connected.db_low,
                                                connected.db_high,
                                                connected.waterfall_db_low,
                                                connected.waterfall_db_high,
                                                current_mode,
                                            );
                                            settings_changed = true;
                                        }
                                    });

                                    ui.separator();
                                    ui.label("While transmitting:");
                                    ui.weak(
                                        "A locally-picked-up TX signal is typically far \
                                         stronger than weak RX signals -- separate range so \
                                         one doesn't compromise the other.",
                                    );
                                    ui.horizontal(|ui| {
                                        ui.label("Spectrum");
                                        let mut tx_low = connected.tx_db_low;
                                        ui.label("Low:");
                                        if scroll_slider_f32(
                                            ui,
                                            &mut connected.slider_scroll_accum,
                                            &mut tx_low,
                                            -180.0..=40.0,
                                            2.0,
                                        ) {
                                            connected.tx_db_low = tx_low;
                                            settings_changed = true;
                                        }
                                        let mut tx_high = connected.tx_db_high;
                                        ui.label("High:");
                                        if scroll_slider_f32(
                                            ui,
                                            &mut connected.slider_scroll_accum,
                                            &mut tx_high,
                                            -180.0..=40.0,
                                            2.0,
                                        ) {
                                            connected.tx_db_high = tx_high;
                                            settings_changed = true;
                                        }
                                    });
                                    ui.horizontal(|ui| {
                                        ui.label("Waterfall");
                                        let mut tx_wlow = connected.tx_waterfall_db_low;
                                        ui.label("Low:");
                                        if scroll_slider_f32(
                                            ui,
                                            &mut connected.slider_scroll_accum,
                                            &mut tx_wlow,
                                            -180.0..=40.0,
                                            2.0,
                                        ) {
                                            connected.tx_waterfall_db_low = tx_wlow;
                                            settings_changed = true;
                                        }
                                        let mut tx_whigh = connected.tx_waterfall_db_high;
                                        ui.label("High:");
                                        if scroll_slider_f32(
                                            ui,
                                            &mut connected.slider_scroll_accum,
                                            &mut tx_whigh,
                                            -180.0..=40.0,
                                            2.0,
                                        ) {
                                            connected.tx_waterfall_db_high = tx_whigh;
                                            settings_changed = true;
                                        }
                                    });
                                }

                                SettingsTab::Tx => {
                                    ui.colored_label(
                                        egui::Color32::from_rgb(230, 150, 50),
                                        "Transmit is unverified against your radio's actual protocol.",
                                    );
                                    ui.weak(
                                        "Bench-test into a dummy load at reduced drive before ever \
                                         using a real antenna. See radio.rs/tx.rs for exactly which \
                                         parts are confirmed vs. best-effort guesses.",
                                    );
                                    ui.add_space(8.0);

                                    ui.horizontal(|ui| {
                                        ui.label(format!("Max TX Power ({:?}):", connected.device.board));
                                        let mut max_watts = connected.max_tx_power_watts as i32;
                                        if scroll_slider_i32(
                                            ui,
                                            &mut connected.slider_scroll_accum,
                                            &mut max_watts,
                                            1..=1000,
                                            5,
                                            "W",
                                        ) {
                                            connected.max_tx_power_watts = max_watts as u32;
                                            let capped = connected
                                                .session
                                                .tx_power_watts
                                                .load(Ordering::Relaxed)
                                                .min(connected.max_tx_power_watts);
                                            connected.session.tx_power_watts.store(capped, Ordering::Relaxed);
                                            settings_changed = true;
                                        }
                                    });
                                    ui.add_space(8.0);

                                    ui.horizontal(|ui| {
                                        ui.label("Tune Power:");
                                        let mut percent = connected.tune_power_percent as i32;
                                        if scroll_slider_i32(
                                            ui,
                                            &mut connected.slider_scroll_accum,
                                            &mut percent,
                                            1..=100,
                                            1,
                                            "%",
                                        ) {
                                            connected.tune_power_percent = percent as u32;
                                            settings_changed = true;
                                        }
                                    });
                                    ui.add_space(8.0);

                                    let mut tx_enabled = connected.tx_enabled;
                                    if ui.checkbox(&mut tx_enabled, "Enable Transmit").changed() {
                                        if tx_enabled {
                                            // P2 has a fixed DUC rate (matches the already-stubbed
                                            // 192ksps in radio.rs's p2_tx_specific_packet); P1 has no
                                            // separate DUC concept, so TX IQ must be produced at
                                            // whatever the shared RX/TX clock is currently set to.
                                            let duc_rate = if connected.device.protocol == 2 {
                                                192_000
                                            } else {
                                                connected.sample_rate as i32
                                            };
                                            let mic_buffer = Arc::new(Mutex::new(VecDeque::new()));
                                            match MicInput::start(Arc::clone(&mic_buffer)) {
                                                Ok(mic) => {
                                                    let tx_handle = TxHandle::start(
                                                        mic_buffer,
                                                        Arc::clone(&connected.session.tci_tx_audio),
                                                        Arc::clone(&connected.session.tx_iq),
                                                        Arc::clone(&connected.session.mox),
                                                        connected.session.iq_buffers.len() as i32,
                                                        connected.device.protocol,
                                                        48_000,
                                                        duc_rate,
                                                        connected.puresignal_enabled,
                                                        Arc::clone(&connected.session.ps_rx_feedback_iq),
                                                        Arc::clone(&connected.session.ps_tx_feedback_iq),
                                                    );
                                                    tx_handle.set_mic_gain(connected.mic_gain);
                                                    tx_handle.set_mode(connected.spectrum.mode());
                                                    tx_handle.set_width_hz(connected.spectrum.width_hz());
                                                    tx_handle.set_ps_enabled(connected.ps_enabled);
                                                    tx_handle.set_ps_hw_peak(connected.ps_hw_peak);
                                                    tx_handle.set_ps_mox_delay(connected.ps_mox_delay);
                                                    tx_handle.set_ps_loop_delay(connected.ps_loop_delay);
                                                    tx_handle.set_ps_tx_delay_ns(connected.ps_tx_delay_ns);
                                                    connected.mic_input = Some(mic);
                                                    connected.tx_handle = Some(tx_handle);
                                                    connected.tx_enabled = true;
                                                }
                                                Err(e) => {
                                                    eprintln!("mic input unavailable: {e}");
                                                    connected.tx_enabled = false;
                                                }
                                            }
                                        } else {
                                            // Disarming must be at least as safe as never having
                                            // armed at all -- force MOX off regardless of whether
                                            // PTT happened to be held at this exact moment.
                                            connected.session.set_mox(false);
                                            connected.ptt_held = false;
                                            connected.tx_handle = None;
                                            connected.mic_input = None;
                                            connected.tx_enabled = false;
                                            // tx_handle is gone regardless, but tune_active/
                                            // pre_tune_power_watts live on ConnectedState and
                                            // would otherwise survive a disarm -- restore the
                                            // real TX Power rather than leaving it at whatever
                                            // reduced tune wattage happened to be active.
                                            if let Some(prev) = connected.pre_tune_power_watts.take() {
                                                connected.session.tx_power_watts.store(prev, Ordering::Relaxed);
                                            }
                                            connected.tune_active = false;
                                        }
                                        settings_changed = true;
                                    }
                                }
                                SettingsTab::PaCalibration => {
                                    // Used by both protocols -- see
                                    // radio::drive_byte_for_watts. Neither
                                    // protocol's raw drive byte tracks
                                    // actual output watts linearly on
                                    // real hardware, so both need the same
                                    // per-band calibration to make the
                                    // main panel's TX Power (W) slider
                                    // mean anything close to accurate.
                                    ui.add_space(4.0);
                                    for band in &BANDS {
                                        let mut gain_db = connected
                                            .pa_calibration
                                            .get(band.name)
                                            .copied()
                                            .unwrap_or(radio::DEFAULT_PA_GAIN_DB);
                                        ui.horizontal(|ui| {
                                            ui.label(format!("{:>4}:", band.name));
                                            if scroll_slider_f32(
                                                ui,
                                                &mut connected.slider_scroll_accum,
                                                &mut gain_db,
                                                20.0..=50.0,
                                                0.1,
                                            ) {
                                                connected
                                                    .pa_calibration
                                                    .insert(band.name.to_string(), gain_db);
                                                settings_changed = true;
                                            }
                                            if ui.small_button("Reset").clicked() {
                                                connected.pa_calibration.remove(band.name);
                                                settings_changed = true;
                                            }
                                        });
                                    }
                                }
                                SettingsTab::PureSignal => {
                                    // See radio::RadioSettings::puresignal_enabled
                                    // and radio::ps_feedback_config for what
                                    // the checkbox below actually requests
                                    // from the radio; tx::PsParams/PsStatus
                                    // for the live calibration controls.
                                    ui.add_space(4.0);
                                    let mut puresignal_enabled = connected.puresignal_enabled;
                                    if ui
                                        .checkbox(&mut puresignal_enabled, "Enable PureSignal")
                                        .changed()
                                    {
                                        connected.puresignal_enabled = puresignal_enabled;
                                        settings_changed = true;
                                    }
                                    ui.weak(
                                        "Reserves 2 extra feedback receivers from the radio -- \
                                         takes effect on next connect, not live.",
                                    );

                                    if connected.puresignal_enabled {
                                        if let Some(tx) = &connected.tx_handle {
                                            ui.add_space(6.0);

                                            let mut ps_enabled = connected.ps_enabled;
                                            if ui
                                                .checkbox(&mut ps_enabled, "Running (continuous auto-calibrate)")
                                                .changed()
                                            {
                                                connected.ps_enabled = ps_enabled;
                                                tx.set_ps_enabled(ps_enabled);
                                                settings_changed = true;
                                            }

                                            if ui.button("Calibrate Now").clicked() {
                                                tx.ps_calibrate();
                                            }
                                            ui.weak(
                                                "Runs one single manual calibration on top of Running above -- \
                                                 e.g. after changing drive or band.",
                                            );

                                            let status = *tx.ps_status.lock().unwrap();
                                            ui.horizontal(|ui| {
                                                ui.label("Feedback level:");
                                                // Confirmed ranges (Thetis/piHPSDR):
                                                // <90 too weak, 128-181 ideal, >256
                                                // dangerously strong.
                                                let color = if status.feedback_level > 256 {
                                                    egui::Color32::from_rgb(220, 60, 60)
                                                } else if status.feedback_level > 181 {
                                                    egui::Color32::from_rgb(80, 140, 220)
                                                } else if status.feedback_level >= 128 {
                                                    egui::Color32::from_rgb(80, 200, 80)
                                                } else if status.feedback_level >= 90 {
                                                    egui::Color32::from_rgb(220, 200, 60)
                                                } else {
                                                    egui::Color32::from_rgb(220, 60, 60)
                                                };
                                                ui.colored_label(color, format!("{}", status.feedback_level));
                                            });
                                            ui.horizontal(|ui| {
                                                ui.label("Correcting:");
                                                let (text, color) = if status.correcting {
                                                    ("yes", egui::Color32::from_rgb(80, 200, 80))
                                                } else {
                                                    ("no", egui::Color32::GRAY)
                                                };
                                                ui.colored_label(color, text);
                                            });
                                            ui.label(format!("Measured peak TX: {:.4}", status.max_tx));

                                            ui.add_space(4.0);
                                            let mut hw_peak = connected.ps_hw_peak;
                                            ui.horizontal(|ui| {
                                                ui.label("HW Peak:");
                                                if scroll_slider_f64(
                                                    ui,
                                                    &mut connected.slider_scroll_accum,
                                                    &mut hw_peak,
                                                    0.0..=1.0,
                                                    0.001,
                                                    "",
                                                ) {
                                                    connected.ps_hw_peak = hw_peak;
                                                    tx.set_ps_hw_peak(hw_peak);
                                                    settings_changed = true;
                                                }
                                            });
                                            ui.weak(
                                                "Per-hardware-model constant -- set once, compare against \
                                                 Measured peak TX above, leave alone otherwise.",
                                            );

                                            ui.horizontal(|ui| {
                                                ui.label("MOX Delay (s):");
                                                let mut mox_delay = connected.ps_mox_delay;
                                                if scroll_slider_f64(
                                                    ui,
                                                    &mut connected.slider_scroll_accum,
                                                    &mut mox_delay,
                                                    0.0..=1.0,
                                                    0.01,
                                                    " s",
                                                ) {
                                                    connected.ps_mox_delay = mox_delay;
                                                    tx.set_ps_mox_delay(mox_delay);
                                                    settings_changed = true;
                                                }
                                            });
                                            ui.horizontal(|ui| {
                                                ui.label("Loop Delay (s):");
                                                let mut loop_delay = connected.ps_loop_delay;
                                                if scroll_slider_f64(
                                                    ui,
                                                    &mut connected.slider_scroll_accum,
                                                    &mut loop_delay,
                                                    0.0..=1.0,
                                                    0.01,
                                                    " s",
                                                ) {
                                                    connected.ps_loop_delay = loop_delay;
                                                    tx.set_ps_loop_delay(loop_delay);
                                                    settings_changed = true;
                                                }
                                            });
                                            ui.horizontal(|ui| {
                                                ui.label("TX Delay (ns):");
                                                let mut tx_delay_ns = connected.ps_tx_delay_ns;
                                                if scroll_slider_f64(
                                                    ui,
                                                    &mut connected.slider_scroll_accum,
                                                    &mut tx_delay_ns,
                                                    0.0..=2000.0,
                                                    1.0,
                                                    " ns",
                                                ) {
                                                    connected.ps_tx_delay_ns = tx_delay_ns;
                                                    tx.set_ps_tx_delay_ns(tx_delay_ns);
                                                    settings_changed = true;
                                                }
                                            });
                                            ui.weak("Advanced -- rarely need changing from the defaults.");
                                        } else {
                                            ui.weak("TX must be enabled for PureSignal calibration controls.");
                                        }
                                    }
                                }
                            }
                        });
                    connected.show_settings_window = still_open;
                }

                if settings_changed
                    || connected.settings_dirty.swap(false, std::sync::atomic::Ordering::Relaxed)
                {
                    let agc_params_now = connected.spectrum.agc_params();
                    let extra_receivers: Vec<ExtraReceiverConfig> = connected
                        .extra_receivers
                        .iter()
                        .map(|rx| {
                            let rx = rx.lock().unwrap();
                            let agc_params = rx.spectrum.agc_params();
                            ExtraReceiverConfig {
                                frequency_hz: rx.frequency_hz.load(std::sync::atomic::Ordering::Relaxed),
                                sample_rate_hz: rx.sample_rate_hz.load(std::sync::atomic::Ordering::Relaxed),
                                mode: rx.spectrum.mode(),
                                width_hz: rx.spectrum.width_hz(),
                                gain: rx.spectrum.gain(),
                                agc: rx.spectrum.agc(),
                                agc_attack_ms: agc_params.agc_attack_ms,
                                agc_decay_ms: agc_params.agc_decay_ms,
                                agc_hang_ms: agc_params.agc_hang_ms,
                                agc_top_db: agc_params.agc_top_db,
                                agc_slope_db: agc_params.agc_slope_db,
                                agc_thresh_db: agc_params.agc_thresh_db,
                                noise_blanker: agc_params.noise_blanker,
                                nb_threshold: agc_params.nb_threshold,
                                noise_reduction: agc_params.noise_reduction,
                                snb: agc_params.snb,
                                db_low: rx.db_low,
                                db_high: rx.db_high,
                                waterfall_db_low: rx.waterfall_db_low,
                                waterfall_db_high: rx.waterfall_db_high,
                                waterfall_palette: rx.waterfall_palette,
                                spectrum_waterfall_ratio: rx.spectrum_waterfall_ratio,
                                adc: rx.adc.load(std::sync::atomic::Ordering::Relaxed) as u8,
                                band_settings: rx.band_memory.clone(),
                                width_memory: rx.width_memory.clone(),
                            }
                        })
                        .collect();
                    Config {
                        frequency_hz: Some(
                            connected.session.frequency_hz.load(std::sync::atomic::Ordering::Relaxed),
                        ),
                        sample_rate: Some(connected.sample_rate),
                        mode: Some(connected.spectrum.mode()),
                        width_hz: Some(connected.spectrum.width_hz()),
                        gain: Some(connected.spectrum.gain()),
                        agc: Some(connected.spectrum.agc()),
                        agc_attack_ms: Some(agc_params_now.agc_attack_ms),
                        agc_decay_ms: Some(agc_params_now.agc_decay_ms),
                        agc_hang_ms: Some(agc_params_now.agc_hang_ms),
                        agc_top_db: Some(agc_params_now.agc_top_db),
                        agc_slope_db: Some(agc_params_now.agc_slope_db),
                        agc_thresh_db: Some(agc_params_now.agc_thresh_db),
                        noise_blanker: Some(agc_params_now.noise_blanker),
                        nb_threshold: Some(agc_params_now.nb_threshold),
                        noise_reduction: Some(agc_params_now.noise_reduction),
                        snb: Some(agc_params_now.snb),
                        mic_gain: Some(connected.mic_gain),
                        tx_power_watts: Some(connected.session.tx_power_watts.load(Ordering::Relaxed)),
                        db_low: Some(connected.db_low),
                        db_high: Some(connected.db_high),
                        waterfall_db_low: Some(connected.waterfall_db_low),
                        waterfall_db_high: Some(connected.waterfall_db_high),
                        tx_db_low: Some(connected.tx_db_low),
                        tx_db_high: Some(connected.tx_db_high),
                        tx_waterfall_db_low: Some(connected.tx_waterfall_db_low),
                        tx_waterfall_db_high: Some(connected.tx_waterfall_db_high),
                        waterfall_palette: Some(connected.waterfall_palette),
                        spectrum_waterfall_ratio: Some(connected.spectrum_waterfall_ratio),
                        adc: Some(connected.session.adc.load(std::sync::atomic::Ordering::Relaxed) as u8),
                        antenna: Some(
                            connected.session.antenna.load(std::sync::atomic::Ordering::Relaxed) as u8,
                        ),
                        band_settings: connected.band_memory.clone(),
                        width_memory: connected.width_memory.clone(),
                        pa_calibration: connected.pa_calibration.clone(),
                        max_tx_power_watts: Some(connected.max_tx_power_watts),
                        tune_power_percent: Some(connected.tune_power_percent),
                        rigctl_addr: Some(connected.rigctl_addr.clone()),
                        tci_addr: Some(connected.tci_addr.clone()),
                        rigctl_running: Some(connected.rigctl_server.is_some()),
                        tci_running: Some(connected.tci_server.is_some()),
                        extra_receivers,
                        puresignal_enabled: Some(connected.puresignal_enabled),
                        rx_attenuation: Some(
                            connected.session.rx_attenuation.load(std::sync::atomic::Ordering::Relaxed),
                        ),
                        ps_hw_peak: Some(connected.ps_hw_peak),
                        ps_mox_delay: Some(connected.ps_mox_delay),
                        ps_loop_delay: Some(connected.ps_loop_delay),
                        ps_tx_delay_ns: Some(connected.ps_tx_delay_ns),
                    }
                    .save(connected.device.mac);
                }
                // Bounded rather than unconditional: this is what
                // keeps the meter/spectrum/waterfall live without a
                // background thread's data update, but requesting an
                // immediate repaint every single frame turns this into
                // an unthrottled busy-loop -- easily the single biggest
                // cause of one CPU core sitting at/near 100% with this
                // app open. ~30Hz is still smooth and comfortably above
                // the analyzer's own ~10Hz update rate.
                ui.ctx().request_repaint_after(Duration::from_millis(33));

                if stop_clicked {
                    connected.session.stop();
                    connected.spectrum.stop();
                    let ctx = ui.ctx().clone();
                    self.state = AppState::Discovering(DiscoveryWindow::new(&ctx));
                }
            }
            AppState::Error(message) => {
                let text = message.clone();
                let mut retry_clicked = false;

                egui::CentralPanel::default().show(ui, |ui| {
                    ui.heading(text);
                    if ui.button("Try again").clicked() {
                        retry_clicked = true;
                    }
                });

                if retry_clicked {
                    let ctx = ui.ctx().clone();
                    self.state = AppState::Discovering(DiscoveryWindow::new(&ctx));
                }
            }
        }
    }
}

/// Compact axis-label format, e.g. 7,100,000 Hz -> "7100.0k".
fn format_khz(hz: f64) -> String {
    format!("{:.1}k", hz / 1000.0)
}

/// Height of the draggable divider between the spectrum and waterfall
/// displays -- replaces the plain ui.add_space() that used to sit
/// there, so it doesn't add extra vertical space on top of it.
const SPECTRUM_WATERFALL_DIVIDER_HEIGHT: f32 = 8.0;

/// Draggable divider between the spectrum and waterfall displays.
/// Updates `ratio` (spectrum's share of their combined height, see
/// Config::spectrum_waterfall_ratio's doc comment) from vertical drag
/// delta, clamped so neither pane can be dragged down to nothing.
/// `combined_pane_height` is spectrum_height + waterfall_height as
/// used by the caller (i.e. excluding this divider's own height) --
/// needed to convert a pixel drag delta into a ratio delta. Returns
/// true if the ratio actually changed this frame, so callers can flag
/// settings_changed/settings_dirty the same way every other
/// interactive control here does.
fn spectrum_waterfall_divider(ui: &mut egui::Ui, ratio: &mut f32, combined_pane_height: f32) -> bool {
    let (rect, resp) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), SPECTRUM_WATERFALL_DIVIDER_HEIGHT),
        egui::Sense::drag(),
    );
    let mut changed = false;
    if resp.dragged() && combined_pane_height > 1.0 {
        let new_ratio =
            (*ratio + resp.drag_delta().y / combined_pane_height).clamp(0.15, 0.85);
        if new_ratio != *ratio {
            *ratio = new_ratio;
            changed = true;
        }
    }
    if resp.hovered() || resp.dragged() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical);
    }
    let color = if resp.dragged() {
        egui::Color32::from_gray(200)
    } else if resp.hovered() {
        egui::Color32::from_gray(150)
    } else {
        egui::Color32::from_gray(70)
    };
    let mid_y = rect.center().y;
    ui.painter().line_segment(
        [egui::pos2(rect.left() + 4.0, mid_y), egui::pos2(rect.right() - 4.0, mid_y)],
        egui::Stroke::new(2.0, color),
    );
    changed
}

/// Small frequency readout drawn next to the mouse cursor while
/// hovering the spectrum trace or waterfall, so you can read off the
/// frequency under the pointer without having to tune there first.
fn draw_freq_hover_tooltip(painter: &egui::Painter, pos: egui::Pos2, freq_hz: u32) {
    let text = format_khz(freq_hz as f64);
    let text_pos = pos + egui::vec2(12.0, -18.0);
    let bg_rect = egui::Rect::from_min_size(text_pos - egui::vec2(4.0, 3.0), egui::vec2(74.0, 18.0));
    painter.rect_filled(bg_rect, 3.0, egui::Color32::from_rgba_unmultiplied(20, 20, 20, 220));
    painter.text(
        text_pos,
        egui::Align2::LEFT_TOP,
        text,
        egui::FontId::monospace(12.0),
        egui::Color32::WHITE,
    );
}

/// Status color for the rigctl/TCI indicators in the main panel:
/// gray = not running, green = listening but idle, red = a client is
/// actively connected. `None` means the server isn't running at all
/// (start/stop is manual now, from Settings -> Network); `Some(bool)`
/// is whether a client is currently connected.
fn network_status_color(status: Option<bool>) -> egui::Color32 {
    match status {
        None => egui::Color32::from_gray(120),
        Some(false) => egui::Color32::from_rgb(60, 190, 60),
        Some(true) => egui::Color32::from_rgb(220, 60, 60),
    }
}

fn network_status_hover(name: &str, status: Option<bool>, addr: &str) -> String {
    let state = match status {
        None => format!("{name}: not running"),
        Some(false) => format!("{name}: listening on {addr}, no client connected"),
        Some(true) => format!("{name}: client connected on {addr}"),
    };
    format!("{state}\n(start/stop in Settings -> Network)")
}

fn format_frequency(hz: u32) -> String {
    let digits = hz.to_string();
    let bytes = digits.as_bytes();
    let mut out = String::new();
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(*b as char);
    }
    format!("{out} Hz")
}

/// Same accumulate-and-threshold pattern as frequency scroll-to-tune:
/// a step only fires once accumulated scroll motion crosses NOTCH, so
/// small/slow scrolling doesn't overshoot. Shares one accumulator
/// across all sliders (see slider_scroll_accum) since only one is ever
/// hovered at a time -- any leftover partial accumulation carrying
/// over when switching sliders mid-gesture is a negligible edge case.
fn scroll_slider_f64(
    ui: &mut egui::Ui,
    scroll_accum: &mut f32,
    value: &mut f64,
    range: std::ops::RangeInclusive<f64>,
    step: f64,
    suffix: &str,
) -> bool {
    let resp = ui.add(egui::Slider::new(value, range.clone()).suffix(suffix));
    let mut changed = resp.changed();
    if resp.hovered() {
        let scroll_delta = ui.input(|i| i.smooth_scroll_delta);
        let delta = if scroll_delta.y.abs() >= scroll_delta.x.abs() {
            scroll_delta.y
        } else {
            scroll_delta.x
        };
        if delta != 0.0 {
            *scroll_accum += delta;
            const NOTCH: f32 = 20.0;
            while scroll_accum.abs() >= NOTCH {
                let sign = scroll_accum.signum();
                *scroll_accum -= sign * NOTCH;
                *value = (*value + step * sign as f64).clamp(*range.start(), *range.end());
                changed = true;
            }
        }
    }
    changed
}

fn scroll_slider_f32(
    ui: &mut egui::Ui,
    scroll_accum: &mut f32,
    value: &mut f32,
    range: std::ops::RangeInclusive<f32>,
    step: f32,
) -> bool {
    let resp = ui.add(egui::Slider::new(value, range.clone()));
    let mut changed = resp.changed();
    if resp.hovered() {
        let scroll_delta = ui.input(|i| i.smooth_scroll_delta);
        let delta = if scroll_delta.y.abs() >= scroll_delta.x.abs() {
            scroll_delta.y
        } else {
            scroll_delta.x
        };
        if delta != 0.0 {
            *scroll_accum += delta;
            const NOTCH: f32 = 20.0;
            while scroll_accum.abs() >= NOTCH {
                let sign = scroll_accum.signum();
                *scroll_accum -= sign * NOTCH;
                *value = (*value + step * sign).clamp(*range.start(), *range.end());
                changed = true;
            }
        }
    }
    changed
}

fn scroll_slider_i32(
    ui: &mut egui::Ui,
    scroll_accum: &mut f32,
    value: &mut i32,
    range: std::ops::RangeInclusive<i32>,
    step: i32,
    suffix: &str,
) -> bool {
    let resp = ui.add(egui::Slider::new(value, range.clone()).suffix(suffix));
    let mut changed = resp.changed();
    if resp.hovered() {
        let scroll_delta = ui.input(|i| i.smooth_scroll_delta);
        let delta = if scroll_delta.y.abs() >= scroll_delta.x.abs() {
            scroll_delta.y
        } else {
            scroll_delta.x
        };
        if delta != 0.0 {
            *scroll_accum += delta;
            const NOTCH: f32 = 20.0;
            while scroll_accum.abs() >= NOTCH {
                let sign = scroll_accum.signum();
                *scroll_accum -= sign * NOTCH;
                *value = (*value + step * sign as i32).clamp(*range.start(), *range.end());
                changed = true;
            }
        }
    }
    changed
}

/// Starting guess for the main panel's TX Power slider's upper bound,
/// used only on first-ever connect to a given radio (MAC) before the
/// user has set anything in Settings -> TX -- see
/// ConnectedState::max_tx_power_watts's doc comment for why board type
/// alone can't be trusted as the final answer (Orion2 in particular
/// covers both a 100W ANAN-100D and a 200W ANAN-8000DLE). Values here
/// are deliberately the lower/safer end of what a board is typically
/// sold as, so an un-set-yet radio undershoots its real max rather than
/// overshoots it.
fn default_max_tx_power_watts(board: Boards) -> u32 {
    match board {
        Boards::HermesLite | Boards::HermesLite2 => 5,
        Boards::Metis | Boards::Hermes | Boards::Hermes2 | Boards::Angelia => 10,
        Boards::Orion | Boards::Orion2 | Boards::Saturn => 100,
        Boards::Unknown => 100,
    }
}

/// Draws a classic analog S-meter: semicircular scale, S0..S9 on the
/// left in 6dB steps (S9 = -73dBm, standard IARU reference), +10..+60
/// over S9 in red on the right, with a needle and digital readout.
///
/// Fed from WDSP's own GetRXAMeter(RXA_S_AV) -- a real calibrated meter
/// reading from the RXA chain, not derived from the uncalibrated
/// spectrum analyzer data. That said, the standard S9=-73dBm reference
/// assumes WDSP's raw output needs no further per-board calibration
/// offset; if S-readings look consistently off from a known reference
/// signal, that offset is the first thing to check.
/// Converts raw forward/reverse power ADC readings into real watts and
/// SWR, using board-specific calibration constants. Confirmed against
/// a working reference (rustyHPSDR) -- both the constants themselves
/// (from the reference's per-board table) and the conversion formula
/// (which also matches the official protocol spec's Appendix A: W =
/// (ADC/4095 * constant1)^2 / constant2). Returns (forward_watts,
/// reverse_watts, swr); SWR is clamped to a sane minimum of 1.0 rather
/// than propagating NaN/negative results from a near-zero forward
/// reading (e.g. right at PTT key-up before power has ramped).
fn power_watts_and_swr(raw_forward: u32, raw_reverse: u32, board: Boards) -> (f32, f32, f32) {
    let (c1, c2): (f32, f32) = match board {
        Boards::Metis => (3.3, 0.09),
        Boards::Hermes => (3.3, 0.095),
        Boards::Hermes2 => (3.3, 0.095),
        Boards::Angelia => (3.3, 0.095),
        Boards::Orion => (5.0, 0.108),
        Boards::Orion2 => (5.0, 0.08),
        Boards::Saturn => (3.3, 0.09),
        Boards::HermesLite => (3.3, 1.4),
        Boards::HermesLite2 => (3.3, 1.4),
        Boards::Unknown => (3.3, 0.09),
    };

    let v_fwd = (raw_forward as f32 / 4095.0) * c1;
    let forward = (v_fwd * v_fwd) / c2;
    let v_rev = (raw_reverse as f32 / 4095.0) * c1;
    let reverse = (v_rev * v_rev) / c2;

    let mut swr = (1.0 + (reverse / forward).sqrt()) / (1.0 - (reverse / forward).sqrt());
    if !swr.is_finite() || swr < 1.0 {
        swr = 1.0;
    }

    (forward, reverse, swr)
}

fn draw_s_meter(ui: &mut egui::Ui, rect: egui::Rect, db: f64) {
    let painter = ui.painter();
    painter.rect_filled(rect, 4.0, egui::Color32::from_gray(20));

    // Reserve room at the bottom for the digital readout text (drawn
    // below) so the arc's flat baseline and the needle's pivot dot --
    // both sitting right at center.y -- can't overlap it. The previous
    // fixed 14px gap here was less than a 13pt monospace glyph's actual
    // height, so the top of the readout visibly clipped through the
    // arc/pivot. TEXT_ZONE and TOP_MARGIN below derive the radius from
    // whatever's actually left over instead of a size tuned for one
    // specific rect, so this keeps working if the meter's rect size
    // ever changes again.
    const TEXT_ZONE: f32 = 26.0;
    const TOP_MARGIN: f32 = 6.0;
    let center = egui::pos2(rect.center().x, rect.bottom() - TEXT_ZONE);
    // The S9 tick label (closest to straight up, at angle_for_db(S9))
    // is the tallest thing drawn above center -- its label sits at
    // radius*1.16 from center, so that's what TOP_MARGIN is measured
    // against, not the bare arc radius itself.
    let radius = (rect.width() * 0.45).min((rect.height() - TEXT_ZONE - TOP_MARGIN) / 1.16);

    const S9: f64 = -73.0;
    const DB_MIN: f64 = S9 - 54.0; // S0
    const DB_MAX: f64 = S9 + 60.0; // S9+60

    let angle_for_db = |v: f64| -> f32 {
        let t = ((v - DB_MIN) / (DB_MAX - DB_MIN)).clamp(0.0, 1.0) as f32;
        std::f32::consts::PI - t * std::f32::consts::PI // left (180deg) to right (0deg)
    };
    let point_at = |angle: f32, r: f32| -> egui::Pos2 { center + egui::vec2(angle.cos(), -angle.sin()) * r };

    // Face arc
    let arc: Vec<egui::Pos2> = (0..=60)
        .map(|i| point_at(std::f32::consts::PI - (i as f32 / 60.0) * std::f32::consts::PI, radius))
        .collect();
    painter.add(egui::Shape::line(arc, egui::Stroke::new(2.0, egui::Color32::WHITE)));

    // S1, S3, S5, S7, S9 ticks (white)
    for s in [1, 3, 5, 7, 9] {
        let v = S9 - (9 - s) as f64 * 6.0;
        let angle = angle_for_db(v);
        painter.line_segment(
            [point_at(angle, radius * 0.85), point_at(angle, radius)],
            egui::Stroke::new(2.0, egui::Color32::WHITE),
        );
        painter.text(
            point_at(angle, radius * 1.16),
            egui::Align2::CENTER_CENTER,
            format!("{s}"),
            egui::FontId::proportional(11.0),
            egui::Color32::WHITE,
        );
    }
    // +20/+40/+60 over S9 ticks (red zone)
    for over in [20, 40, 60] {
        let angle = angle_for_db(S9 + over as f64);
        painter.line_segment(
            [point_at(angle, radius * 0.85), point_at(angle, radius)],
            egui::Stroke::new(2.0, egui::Color32::RED),
        );
        painter.text(
            point_at(angle, radius * 1.18),
            egui::Align2::CENTER_CENTER,
            format!("+{over}"),
            egui::FontId::proportional(10.0),
            egui::Color32::RED,
        );
    }

    // Needle
    let needle_angle = angle_for_db(db);
    painter.line_segment(
        [center, point_at(needle_angle, radius * 0.92)],
        egui::Stroke::new(2.5, egui::Color32::YELLOW),
    );
    painter.circle_filled(center, 4.0, egui::Color32::YELLOW);

    // Digital readout
    painter.text(
        egui::pos2(center.x, rect.bottom() - 2.0),
        egui::Align2::CENTER_BOTTOM,
        s_meter_label(db, S9),
        egui::FontId::monospace(13.0),
        egui::Color32::WHITE,
    );
}

/// Same semicircle-gauge treatment as draw_s_meter, scaled 0..max_watts
/// instead of S-units, shown in place of it while transmitting. The
/// needle (and the combined digital readout) turn red once SWR crosses
/// the same 3.0 threshold the plain-text display this replaces already
/// flagged, so a bad match is visible at a glance without reading the
/// number.
fn draw_power_meter(ui: &mut egui::Ui, rect: egui::Rect, watts: f32, swr: f32, max_watts: f32) {
    let painter = ui.painter();
    painter.rect_filled(rect, 4.0, egui::Color32::from_gray(20));

    // Same reserved-bottom-space approach as draw_s_meter, and the same
    // TEXT_ZONE/TOP_MARGIN values -- see its doc comment for why they
    // exist -- so this gauge ends up the same size, not a smaller one
    // just because its readout happens to say more.
    const TEXT_ZONE: f32 = 26.0;
    const TOP_MARGIN: f32 = 6.0;
    let center = egui::pos2(rect.center().x, rect.bottom() - TEXT_ZONE);
    let radius = (rect.width() * 0.45).min((rect.height() - TEXT_ZONE - TOP_MARGIN) / 1.16);

    let max_watts = max_watts.max(1.0);
    let angle_for_watts = |w: f32| -> f32 {
        let t = (w / max_watts).clamp(0.0, 1.0);
        std::f32::consts::PI - t * std::f32::consts::PI // left (180deg) to right (0deg)
    };
    let point_at = |angle: f32, r: f32| -> egui::Pos2 { center + egui::vec2(angle.cos(), -angle.sin()) * r };

    // Face arc
    let arc: Vec<egui::Pos2> = (0..=60)
        .map(|i| point_at(std::f32::consts::PI - (i as f32 / 60.0) * std::f32::consts::PI, radius))
        .collect();
    painter.add(egui::Shape::line(arc, egui::Stroke::new(2.0, egui::Color32::WHITE)));

    // 0/25/50/75/100% of max_watts ticks, labeled with the actual watt
    // value rather than a percentage -- max_watts varies per radio (see
    // ConnectedState::max_tx_power_watts), so a fixed set of watt
    // labels wouldn't make sense across boards the way S1-S9 does.
    for frac in [0.0, 0.25, 0.5, 0.75, 1.0] {
        let w = max_watts * frac;
        let angle = angle_for_watts(w);
        painter.line_segment(
            [point_at(angle, radius * 0.85), point_at(angle, radius)],
            egui::Stroke::new(2.0, egui::Color32::WHITE),
        );
        painter.text(
            point_at(angle, radius * 1.16),
            egui::Align2::CENTER_CENTER,
            format!("{w:.0}"),
            egui::FontId::proportional(11.0),
            egui::Color32::WHITE,
        );
    }

    let bad_swr = swr > 3.0;
    let needle_color =
        if bad_swr { egui::Color32::from_rgb(220, 60, 60) } else { egui::Color32::YELLOW };
    let needle_angle = angle_for_watts(watts);
    painter.line_segment(
        [center, point_at(needle_angle, radius * 0.92)],
        egui::Stroke::new(2.5, needle_color),
    );
    painter.circle_filled(center, 4.0, needle_color);

    // Digital readout: watts + SWR combined into the one line draw_s_meter
    // itself uses, so the two gauges stay visually consistent -- see
    // draw_s_meter's own readout for the pattern this mirrors.
    painter.text(
        egui::pos2(center.x, rect.bottom() - 2.0),
        egui::Align2::CENTER_BOTTOM,
        format!("{watts:.0}W  SWR {swr:.1}:1"),
        egui::FontId::monospace(13.0),
        if bad_swr { egui::Color32::from_rgb(220, 60, 60) } else { egui::Color32::from_rgb(255, 150, 70) },
    );
}

fn s_meter_label(db: f64, s9: f64) -> String {
    if db >= s9 {
        format!("S9+{:.0}", db - s9)
    } else {
        let s = (((db - s9) / 6.0) + 9.0).round().clamp(0.0, 9.0);
        format!("S{s:.0}")
    }
}

/// Inverse of the frequency axis mapping used for the tick labels:
/// converts an x pixel position within the spectrum/waterfall rect back
/// into a frequency, rounded to the nearest 1kHz.
/// Renders one extra receiver's panel: frequency + scroll/click-to-tune,
/// mode buttons, spectrum, waterfall. Deliberately simpler than the main
/// receiver's UI -- fixed level range/palette rather than the full
/// AGC-settings-window/persistence treatment, to keep this addition
/// bounded in size. Locks the receiver's Mutex for the whole render,
/// which is fine at normal UI frame rates.
fn render_extra_receiver_ui(ui: &mut egui::Ui, rx: &Arc<Mutex<ExtraReceiver>>) {
    let mut rx = rx.lock().unwrap();

    let freq_hz = rx.frequency_hz.load(Ordering::Relaxed);
    let sample_rate = rx.sample_rate_hz.load(Ordering::Relaxed);
    let current_mode = rx.spectrum.mode();
    let current_width = rx.spectrum.width_hz();
    // Reused by resolve_tune (clamping a CTUN target so the passband
    // stays fully on-screen) and by the passband overlay drawn below.
    let passband = spectrum::passband_for(current_mode, current_width);
    let current_gain = rx.spectrum.gain();
    let db_low = rx.db_low;
    let db_high = rx.db_high;
    let wf_db_low = rx.waterfall_db_low;
    let wf_db_high = rx.waterfall_db_high;
    let palette = rx.waterfall_palette;

    let (spectrum_row, waterfall_data_revision) = {
        let d = rx.spectrum.display.lock().unwrap();
        (d.spectrum.clone(), d.revision)
    };

    // CTUN: same behavior as the main receiver -- see
    // ConnectedState::ctun's doc comment. Pushed to the analyzer thread
    // every frame regardless of change, same reasoning as the main
    // receiver's own per-frame resync.
    let ctun_offset_hz = if rx.ctun { rx.ctun_frequency_hz as f64 - freq_hz as f64 } else { 0.0 };
    rx.spectrum.set_lo_frequency_hz(freq_hz as f64);
    rx.spectrum.set_ctun(rx.ctun, ctun_offset_hz);
    let dial_freq_hz = if rx.ctun { rx.ctun_frequency_hz } else { freq_hz };

    ui.heading(egui::RichText::new(format_frequency(dial_freq_hz)).color(egui::Color32::GREEN))
        .on_hover_text("Scroll to tune -- Shift: 100 Hz, none: 1 kHz. Click spectrum/waterfall to jump.");

    ui.horizontal_wrapped(|ui| {
        let current_band = band_for_frequency(dial_freq_hz).map(|b| b.name);
        for band in &BANDS {
            let selected = Some(band.name) == current_band;
            if ui.add(egui::Button::selectable(selected, band.name)).clicked() && !selected {
                let saved = rx.band_memory.get(band.name).copied();
                let target = saved.map(|s| s.frequency_hz).unwrap_or(band.default_hz);
                rx.frequency_hz.store(target, Ordering::Relaxed);
                rx.ctun_frequency_hz = target;
                if let Some(s) = saved {
                    rx.db_low = s.db_low;
                    rx.db_high = s.db_high;
                    rx.waterfall_db_low = s.waterfall_db_low;
                    rx.waterfall_db_high = s.waterfall_db_high;
                }
                let (new_db_low, new_db_high, new_wf_low, new_wf_high) =
                    (rx.db_low, rx.db_high, rx.waterfall_db_low, rx.waterfall_db_high);
                // Restore whatever mode was last used on this band, if
                // any -- see the main receiver's own band-click handler
                // for the full reasoning.
                let resolved_mode = saved.and_then(|s| s.mode).unwrap_or(band.default_mode);
                remember_band_settings(
                    &mut rx.band_memory,
                    target,
                    new_db_low,
                    new_db_high,
                    new_wf_low,
                    new_wf_high,
                    resolved_mode,
                );
                rx.spectrum.set_mode(resolved_mode);
                rx.spectrum.set_width_hz(width_for_mode(&rx.width_memory, resolved_mode));
                rx.settings_dirty.store(true, Ordering::Relaxed);
            }
        }
    });

    ui.horizontal_wrapped(|ui| {
        for mode in ALL_MODES {
            let selected = mode == current_mode;
            if ui.add(egui::Button::selectable(selected, mode.label())).clicked() && !selected {
                rx.spectrum.set_mode(mode);
                rx.spectrum.set_width_hz(width_for_mode(&rx.width_memory, mode));
                remember_band_settings(
                    &mut rx.band_memory,
                    dial_freq_hz,
                    db_low,
                    db_high,
                    wf_db_low,
                    wf_db_high,
                    mode,
                );
                rx.settings_dirty.store(true, Ordering::Relaxed);
            }
        }
    });

    ui.horizontal(|ui| {
        ui.label("Filter width:");
        let mut width = current_width;
        if scroll_slider_f64(ui, &mut rx.slider_scroll_accum, &mut width, 50.0..=5000.0, 50.0, " Hz") {
            rx.spectrum.set_width_hz(width);
            rx.width_memory.insert(current_mode.label().to_string(), width);
            rx.settings_dirty.store(true, Ordering::Relaxed);
        }
    });
    ui.horizontal(|ui| {
        ui.label("Audio gain:");
        let mut gain = current_gain;
        if scroll_slider_f32(ui, &mut rx.slider_scroll_accum, &mut gain, 0.0..=1.5, 0.05) {
            rx.spectrum.set_gain(gain);
            rx.settings_dirty.store(true, Ordering::Relaxed);
        }
    });

    ui.horizontal_wrapped(|ui| {
        if ui
            .add(egui::Button::selectable(rx.ctun, "CTUN"))
            .on_hover_text("Click to Tune: browse within the spectrum without retuning the radio")
            .clicked()
        {
            if rx.ctun {
                rx.frequency_hz.store(rx.ctun_frequency_hz, Ordering::Relaxed);
            } else {
                rx.ctun_frequency_hz = freq_hz;
            }
            rx.ctun = !rx.ctun;
            rx.settings_dirty.store(true, Ordering::Relaxed);
        }
        let nb = rx.spectrum.noise_blanker();
        if ui
            .add(egui::Button::selectable(nb != spectrum::NoiseBlanker::Off, nb.label()))
            .on_hover_text("Click to cycle: Off -> NB -> NB2 -> Off")
            .clicked()
        {
            rx.spectrum.set_noise_blanker(nb.next());
            rx.settings_dirty.store(true, Ordering::Relaxed);
        }
        let nr = rx.spectrum.noise_reduction();
        if ui
            .add(egui::Button::selectable(nr != spectrum::NoiseReduction::Off, nr.label()))
            .on_hover_text("Click to cycle: Off -> NR -> NR2 -> NR3 -> NR4 -> Off")
            .clicked()
        {
            rx.spectrum.set_noise_reduction(nr.next());
            rx.settings_dirty.store(true, Ordering::Relaxed);
        }
        let snb = rx.spectrum.snb();
        if ui
            .add(egui::Button::selectable(snb, "SNB"))
            .on_hover_text("Spectral Noise Blanker -- independent of NB/NR, can run alongside them")
            .clicked()
        {
            rx.spectrum.set_snb(!snb);
            rx.settings_dirty.store(true, Ordering::Relaxed);
        }
        let current_agc = rx.spectrum.agc();
        if ui
            .add(egui::Button::selectable(current_agc != spectrum::Agc::Off, current_agc.label()))
            .on_hover_text("Click to cycle: Off -> Long -> Slow -> Medium -> Fast -> Off")
            .clicked()
        {
            rx.spectrum.set_agc(current_agc.next());
            rx.settings_dirty.store(true, Ordering::Relaxed);
        }
    });

    ui.add_space(4.0);
    if ui.button("Settings...").clicked() {
        rx.show_settings_window = !rx.show_settings_window;
    }

    ui.add_space(4.0);
    ui.label("Spectrum");
    // Split the window's remaining vertical space between the spectrum
    // and waterfall, according to rx.spectrum_waterfall_ratio
    // (adjustable via the drag handle between them) -- see the main
    // receiver's own version of this for the full reasoning. Nothing
    // else is rendered below the waterfall in this window, so no
    // reserve is needed here (unlike the main receiver's Stop button).
    let spectrum_waterfall_height =
        (ui.available_height() - SPECTRUM_WATERFALL_DIVIDER_HEIGHT).max(200.0);
    let spectrum_height = (spectrum_waterfall_height * rx.spectrum_waterfall_ratio).max(80.0);
    let (rect, spectrum_resp) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), spectrum_height),
        egui::Sense::click(),
    );

    if let Some(pos) = spectrum_resp.interact_pointer_pos() {
        if spectrum_resp.clicked() {
            let new_freq = freq_at_x(pos.x, rect, freq_hz, sample_rate);
            let (effective_freq, retune) = resolve_tune(rx.ctun, freq_hz, sample_rate, passband, new_freq);
            if let Some(lo) = retune {
                rx.frequency_hz.store(lo, Ordering::Relaxed);
            } else {
                rx.ctun_frequency_hz = effective_freq;
            }
            remember_band_settings(&mut rx.band_memory, effective_freq, db_low, db_high, wf_db_low, wf_db_high, current_mode);
            rx.settings_dirty.store(true, Ordering::Relaxed);
        }
    }
    if spectrum_resp.hovered() {
        let scroll_delta = ui.input(|i| i.smooth_scroll_delta);
        let delta = if scroll_delta.y.abs() >= scroll_delta.x.abs() {
            scroll_delta.y
        } else {
            scroll_delta.x
        };
        if delta != 0.0 {
            rx.scroll_accum += delta;
            // See the main receiver's own scroll-to-tune NOTCH comment.
            const NOTCH: f32 = 50.0;
            let shift = ui.input(|i| i.modifiers.shift);
            let step: i64 = if shift { 100 } else { 1_000 };
            let mut new_freq = dial_freq_hz as i64;
            while rx.scroll_accum.abs() >= NOTCH {
                let sign = rx.scroll_accum.signum();
                rx.scroll_accum -= sign * NOTCH;
                new_freq += step * sign as i64;
            }
            new_freq = new_freq.max(0);
            if new_freq as u32 != dial_freq_hz {
                let (effective_freq, retune) =
                    resolve_tune(rx.ctun, freq_hz, sample_rate, passband, new_freq as u32);
                if let Some(lo) = retune {
                    rx.frequency_hz.store(lo, Ordering::Relaxed);
                } else {
                    rx.ctun_frequency_hz = effective_freq;
                }
                remember_band_settings(&mut rx.band_memory, effective_freq, db_low, db_high, wf_db_low, wf_db_high, current_mode);
                rx.settings_dirty.store(true, Ordering::Relaxed);
            }
        }
    }

    ui.painter().rect_filled(rect, 0.0, egui::Color32::BLACK);

    // Frequency axis ticks -- same mapping as the main receiver window.
    let half_span_hz = sample_rate as f64 / 2.0;
    let num_freq_ticks = 5;
    for t in 0..num_freq_ticks {
        let frac = t as f32 / (num_freq_ticks - 1) as f32;
        let x = rect.left() + frac * rect.width();
        ui.painter().line_segment(
            [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
            egui::Stroke::new(1.0, egui::Color32::from_gray(55)),
        );
        let tick_freq_hz = freq_hz as f64 - half_span_hz + frac as f64 * sample_rate as f64;
        ui.painter().text(
            egui::pos2(x + 2.0, rect.bottom() - 2.0),
            egui::Align2::LEFT_BOTTOM,
            format_khz(tick_freq_hz),
            egui::FontId::monospace(13.0),
            egui::Color32::GRAY,
        );
    }

    // Filter passband overlay, same as the main window.
    let x_for_offset = |offset_hz: f64| -> f32 {
        let frac = ((offset_hz + half_span_hz) / sample_rate as f64).clamp(0.0, 1.0) as f32;
        rect.left() + frac * rect.width()
    };
    let (pb_low, pb_high) = passband;
    let x_low = x_for_offset(pb_low + ctun_offset_hz);
    let x_high = x_for_offset(pb_high + ctun_offset_hz);
    ui.painter().rect_filled(
        egui::Rect::from_min_max(egui::pos2(x_low, rect.top()), egui::pos2(x_high, rect.bottom())),
        0.0,
        egui::Color32::from_rgba_unmultiplied(70, 150, 230, 50),
    );
    let x_dial = x_for_offset(ctun_offset_hz);

    if spectrum_row.len() > 1 {
        let range = (db_high - db_low).max(1.0);

        // Reserve space at the bottom for the frequency axis labels
        // drawn there, so the trace/gridlines never overdraw them.
        // Sized for the 13.0 font above, not just the older/smaller 10.0.
        const FREQ_AXIS_MARGIN: f32 = 20.0;
        let plot_bottom = rect.bottom() - FREQ_AXIS_MARGIN;
        let plot_height = plot_bottom - rect.top();

        // Power-level gridlines, same treatment as the main window.
        let num_db_ticks = 4;
        for t in 0..=num_db_ticks {
            let frac = t as f32 / num_db_ticks as f32;
            let y = plot_bottom - frac * plot_height;
            ui.painter().line_segment(
                [egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)],
                egui::Stroke::new(1.0, egui::Color32::from_gray(55)),
            );
            let db = db_low + frac * range;
            ui.painter().text(
                egui::pos2(rect.left() + 2.0, y),
                egui::Align2::LEFT_TOP,
                format!("{db:.0} dB"),
                egui::FontId::monospace(10.0),
                egui::Color32::GRAY,
            );
        }

        let n = spectrum_row.len().saturating_sub(1).max(1);
        let points: Vec<egui::Pos2> = spectrum_row
            .iter()
            .enumerate()
            .map(|(i, &v)| {
                let x = rect.left() + (i as f32 / n as f32) * rect.width();
                let t = ((v - db_low) / range).clamp(0.0, 1.0);
                let y = plot_bottom - t * plot_height;
                egui::pos2(x, y)
            })
            .collect();
        ui.painter()
            .add(egui::Shape::line(points, egui::Stroke::new(1.5, egui::Color32::LIGHT_GREEN)));
    }

    // Drawn last (on top of the trace/gridlines above) and thicker
    // than a plain 1px stroke, same as the main window.
    ui.painter().line_segment(
        [egui::pos2(x_dial, rect.top()), egui::pos2(x_dial, rect.bottom())],
        egui::Stroke::new(2.0, egui::Color32::RED),
    );

    if let Some(pos) = spectrum_resp.hover_pos() {
        let hover_freq = freq_at_x(pos.x, rect, freq_hz, sample_rate);
        draw_freq_hover_tooltip(ui.painter(), pos, hover_freq);
    }

    if spectrum_waterfall_divider(ui, &mut rx.spectrum_waterfall_ratio, spectrum_waterfall_height) {
        rx.settings_dirty.store(true, Ordering::Relaxed);
    }
    let waterfall_height = (spectrum_waterfall_height - spectrum_height).max(80.0);
    let (wf_rect, wf_resp) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), waterfall_height),
        egui::Sense::click(),
    );
    if let Some(pos) = wf_resp.interact_pointer_pos() {
        if wf_resp.clicked() {
            let new_freq = freq_at_x(pos.x, wf_rect, freq_hz, sample_rate);
            let (effective_freq, retune) = resolve_tune(rx.ctun, freq_hz, sample_rate, passband, new_freq);
            if let Some(lo) = retune {
                rx.frequency_hz.store(lo, Ordering::Relaxed);
            } else {
                rx.ctun_frequency_hz = effective_freq;
            }
            remember_band_settings(&mut rx.band_memory, effective_freq, db_low, db_high, wf_db_low, wf_db_high, current_mode);
            rx.settings_dirty.store(true, Ordering::Relaxed);
        }
    }

    let wanted_signature = (waterfall_data_revision, palette, wf_db_low, wf_db_high);
    if rx.waterfall_signature != Some(wanted_signature) {
        let waterfall_rows: Vec<Vec<f32>> = {
            let d = rx.spectrum.display.lock().unwrap();
            d.waterfall_rows.iter().cloned().collect()
        };
        let waterfall_image = build_waterfall_image(&waterfall_rows, palette, wf_db_low, wf_db_high);
        if let Some(image) = &waterfall_image {
            let texture_name = format!("waterfall_rx{}", rx.ddc_index);
            match &mut rx.waterfall_texture {
                Some(tex) => tex.set(image.clone(), egui::TextureOptions::LINEAR),
                None => {
                    let tex = ui.ctx().load_texture(texture_name, image.clone(), egui::TextureOptions::LINEAR);
                    rx.waterfall_texture = Some(tex);
                }
            }
            rx.waterfall_signature = Some(wanted_signature);
        }
        // else: no rows yet -- leave waterfall_signature unset so this
        // retries (cheaply) next frame, same as the main receiver.
    }
    if rx.waterfall_texture.is_some() {
        ui.painter().image(
            rx.waterfall_texture.as_ref().unwrap().id(),
            wf_rect,
            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            egui::Color32::WHITE,
        );
    } else {
        ui.painter().rect_filled(wf_rect, 0.0, egui::Color32::BLACK);
        ui.put(wf_rect, egui::Label::new(wisdom_status_text()));
    }
    if let Some(pos) = wf_resp.hover_pos() {
        let hover_freq = freq_at_x(pos.x, wf_rect, freq_hz, sample_rate);
        draw_freq_hover_tooltip(ui.painter(), pos, hover_freq);
    }

    // Bounded rather than unconditional -- see the same call in the
    // main receiver's update loop for why.
    ui.ctx().request_repaint_after(Duration::from_millis(33));
}

/// Content of an extra receiver's own "AGC / Level Settings" popup --
/// mirrors the main window's settings window, minus sample rate (not
/// requested for extra receivers).
fn render_extra_receiver_settings(ui: &mut egui::Ui, rx: &Arc<Mutex<ExtraReceiver>>) {
    let mut rx = rx.lock().unwrap();
    let agc_params = rx.spectrum.agc_params();
    let current_rate = rx.sample_rate_hz.load(Ordering::Relaxed);
    let freq_hz = rx.frequency_hz.load(Ordering::Relaxed);

    ui.horizontal(|ui| {
        for (tab, label) in [(SettingsTab::Agc, "RX"), (SettingsTab::Spectrum, "Spectrum")] {
            if ui.selectable_label(rx.settings_tab == tab, label).clicked() {
                rx.settings_tab = tab;
            }
        }
    });
    ui.separator();

    match rx.settings_tab {
        // Extra receivers have no rigctl/TCI of their own (only the
        // primary receiver is exposed there), so there's no Network tab
        // -- fall back to AGC if this is ever somehow selected.
        SettingsTab::Network => rx.settings_tab = SettingsTab::Agc,

        // TX (and PA Calibration/PureSignal, split out of it) are all
        // global (one radio, one PA/mic path), not per-receiver -- no
        // such tabs shown for extra receivers, so redirect same as
        // Network if any of these are ever somehow selected.
        SettingsTab::Tx => rx.settings_tab = SettingsTab::Agc,
        SettingsTab::PaCalibration => rx.settings_tab = SettingsTab::Agc,
        SettingsTab::PureSignal => rx.settings_tab = SettingsTab::Agc,

        SettingsTab::Agc => {
            ui.label("Sample Rate:");
            // Disabled on Protocol 1 -- unlike P2 (where each DDC really
            // can run its own independent decimation rate), P1 has a
            // single shared RX/TX sample-rate register with no per-
            // receiver override slot (see radio.rs's p1_build_packet:
            // sample_rate_code(sample_rate_hz) in the general-control
            // frame, sent once for the whole session). This receiver's
            // rate is kept in sync with the main receiver's instead
            // (see change_sample_rate's P1 branch) rather than exposed
            // as independently adjustable, which the hardware has no
            // way to actually honor.
            ui.add_enabled_ui(rx.protocol != 1, |ui| {
                ui.horizontal_wrapped(|ui| {
                    for rate in [48_000u32, 96_000, 192_000, 384_000, 768_000, 1_536_000] {
                        let selected = rate == current_rate;
                        let label = format!("{}", rate / 1000);
                        if ui.add(egui::Button::selectable(selected, label)).clicked() && !selected {
                            change_extra_receiver_sample_rate(&mut rx, rate);
                            rx.settings_dirty.store(true, Ordering::Relaxed);
                        }
                    }
                    ui.weak("kHz");
                });
            });
            if rx.protocol == 1 {
                ui.weak("Follows the main receiver's sample rate on Protocol 1 (one shared clock).");
            } else {
                ui.weak("Changing this briefly interrupts audio/spectrum for this receiver while the demod chain restarts.");
            }
            ui.separator();

            let current_adc = rx.adc.load(Ordering::Relaxed);
            ui.label("ADC:");
            ui.horizontal_wrapped(|ui| {
                for adc in 0..rx.num_adcs as u32 {
                    let selected = adc == current_adc;
                    if ui.add(egui::Button::selectable(selected, format!("ADC{adc}"))).clicked() && !selected {
                        rx.adc.store(adc, Ordering::Relaxed);
                        rx.settings_dirty.store(true, Ordering::Relaxed);
                    }
                }
            });

            if current_adc == 0 {
                let current_ant = rx.antenna.load(Ordering::Relaxed);
                ui.label("Antenna (shared across all ADC0 receivers):");
                ui.horizontal_wrapped(|ui| {
                    for (ant, label) in [(0u32, "ANT1"), (1, "ANT2"), (2, "ANT3")] {
                        let selected = ant == current_ant;
                        if ui.add(egui::Button::selectable(selected, label)).clicked() && !selected {
                            rx.antenna.store(ant, Ordering::Relaxed);
                            rx.settings_dirty.store(true, Ordering::Relaxed);
                        }
                    }
                });
            }
            ui.separator();

            ui.horizontal_wrapped(|ui| {
                let mut attack = agc_params.agc_attack_ms;
                ui.label("Attack:");
                if scroll_slider_i32(ui, &mut rx.slider_scroll_accum, &mut attack, 0..=20, 1, " ms") {
                    rx.spectrum.set_agc_attack_ms(attack);
                    rx.settings_dirty.store(true, Ordering::Relaxed);
                }

                let mut decay = agc_params.agc_decay_ms;
                ui.label("Decay:");
                if scroll_slider_i32(ui, &mut rx.slider_scroll_accum, &mut decay, 0..=2000, 25, " ms") {
                    rx.spectrum.set_agc_decay_ms(decay);
                    rx.settings_dirty.store(true, Ordering::Relaxed);
                }

                let mut hang = agc_params.agc_hang_ms;
                ui.label("Hang:");
                if scroll_slider_i32(ui, &mut rx.slider_scroll_accum, &mut hang, 0..=2000, 25, " ms") {
                    rx.spectrum.set_agc_hang_ms(hang);
                    rx.settings_dirty.store(true, Ordering::Relaxed);
                }
            });

            ui.horizontal_wrapped(|ui| {
                let mut top = agc_params.agc_top_db;
                ui.label("Top:");
                if scroll_slider_f64(ui, &mut rx.slider_scroll_accum, &mut top, 0.0..=140.0, 2.0, " dB") {
                    rx.spectrum.set_agc_top_db(top);
                    rx.settings_dirty.store(true, Ordering::Relaxed);
                }

                let mut slope = agc_params.agc_slope_db;
                ui.label("Slope:");
                if scroll_slider_i32(ui, &mut rx.slider_scroll_accum, &mut slope, 0..=100, 2, " dB") {
                    rx.spectrum.set_agc_slope_db(slope);
                    rx.settings_dirty.store(true, Ordering::Relaxed);
                }

                let mut thresh = agc_params.agc_thresh_db;
                ui.label("Thresh:");
                if scroll_slider_f64(ui, &mut rx.slider_scroll_accum, &mut thresh, -140.0..=0.0, 2.0, " dB") {
                    rx.spectrum.set_agc_thresh_db(thresh);
                    rx.settings_dirty.store(true, Ordering::Relaxed);
                }
            });

            ui.separator();
            ui.horizontal(|ui| {
                let mut nb_threshold = rx.spectrum.nb_threshold();
                ui.label("NB Threshold:");
                if scroll_slider_f64(ui, &mut rx.slider_scroll_accum, &mut nb_threshold, 0.0..=100.0, 1.0, "") {
                    rx.spectrum.set_nb_threshold(nb_threshold);
                    rx.settings_dirty.store(true, Ordering::Relaxed);
                }
            });
            ui.weak("Shared by both NB and NB2 (toggle either on this window's main panel).");
        }

        SettingsTab::Spectrum => {
            ui.horizontal(|ui| {
                ui.label("Spectrum");
                let mut low = rx.db_low;
                ui.label("Low:");
                if scroll_slider_f32(ui, &mut rx.slider_scroll_accum, &mut low, -180.0..=0.0, 2.0) {
                    rx.db_low = low;
                    let (a, b, c, d) = (rx.db_low, rx.db_high, rx.waterfall_db_low, rx.waterfall_db_high);
                    remember_band_settings(&mut rx.band_memory, freq_hz, a, b, c, d, agc_params.mode);
                    rx.settings_dirty.store(true, Ordering::Relaxed);
                }
                let mut high = rx.db_high;
                ui.label("High:");
                if scroll_slider_f32(ui, &mut rx.slider_scroll_accum, &mut high, -180.0..=0.0, 2.0) {
                    rx.db_high = high;
                    let (a, b, c, d) = (rx.db_low, rx.db_high, rx.waterfall_db_low, rx.waterfall_db_high);
                    remember_band_settings(&mut rx.band_memory, freq_hz, a, b, c, d, agc_params.mode);
                    rx.settings_dirty.store(true, Ordering::Relaxed);
                }
            });

            ui.separator();
            ui.horizontal(|ui| {
                ui.label("Waterfall palette:");
                for palette in ALL_PALETTES {
                    let selected = palette == rx.waterfall_palette;
                    if ui.add(egui::Button::selectable(selected, palette.label())).clicked() {
                        rx.waterfall_palette = palette;
                        rx.settings_dirty.store(true, Ordering::Relaxed);
                    }
                }
            });
            ui.horizontal(|ui| {
                let mut wlow = rx.waterfall_db_low;
                ui.label("Low:");
                if scroll_slider_f32(ui, &mut rx.slider_scroll_accum, &mut wlow, -180.0..=0.0, 2.0) {
                    rx.waterfall_db_low = wlow;
                    let (a, b, c, d) = (rx.db_low, rx.db_high, rx.waterfall_db_low, rx.waterfall_db_high);
                    remember_band_settings(&mut rx.band_memory, freq_hz, a, b, c, d, agc_params.mode);
                    rx.settings_dirty.store(true, Ordering::Relaxed);
                }
                let mut whigh = rx.waterfall_db_high;
                ui.label("High:");
                if scroll_slider_f32(ui, &mut rx.slider_scroll_accum, &mut whigh, -180.0..=0.0, 2.0) {
                    rx.waterfall_db_high = whigh;
                    let (a, b, c, d) = (rx.db_low, rx.db_high, rx.waterfall_db_low, rx.waterfall_db_high);
                    remember_band_settings(&mut rx.band_memory, freq_hz, a, b, c, d, agc_params.mode);
                    rx.settings_dirty.store(true, Ordering::Relaxed);
                }
            });
        }
    }
}

fn freq_at_x(x: f32, rect: egui::Rect, center_freq_hz: u32, sample_rate: u32) -> u32 {
    let frac = ((x - rect.left()) / rect.width()).clamp(0.0, 1.0) as f64;
    let half_span_hz = sample_rate as f64 / 2.0;
    let freq = center_freq_hz as f64 - half_span_hz + frac * sample_rate as f64;
    let rounded = (freq / 1000.0).round() * 1000.0;
    rounded.max(0.0) as u32
}

/// Decides what a tuning request (from a spectrum/waterfall click,
/// scroll, or zoom gesture) should actually do: retune the hardware/LO
/// frequency (CTUN off), or move the CTUN dial within the current LO's
/// spectrum window without touching the hardware (CTUN on). Returns the
/// frequency the user is now effectively listening to (for band-
/// membership/display and remember_band_settings) plus Some(lo) if the
/// hardware should be retuned to `lo`.
///
/// A CTUN target is clamped so the current mode's filter passband --
/// not just the dial point itself -- stays fully within the visible
/// spectrum span. Since the LO doesn't move to follow the dial while
/// CTUN is on, letting the dial get close enough to either edge would
/// let part of the passband (the shaded region drawn around the dial,
/// extending `passband.0`..`passband.1` Hz relative to it) run off the
/// edge of the spectrum/waterfall.
fn resolve_tune(
    ctun: bool,
    lo_freq_hz: u32,
    sample_rate: u32,
    passband: (f64, f64),
    new_freq: u32,
) -> (u32, Option<u32>) {
    if ctun {
        let half_span = sample_rate as f64 / 2.0;
        let (pb_low, pb_high) = passband;
        // Only the side each edge actually extends past the dial
        // tightens that bound -- e.g. USB's passband is entirely to
        // the right (pb_low > 0), so the left edge isn't restricted
        // any further than the plain dial-in-span limit.
        let lower = (lo_freq_hz as f64 - half_span - pb_low.min(0.0)).max(0.0);
        let upper = (lo_freq_hz as f64 + half_span - pb_high.max(0.0)).max(lower);
        let clamped = (new_freq as f64).clamp(lower, upper) as u32;
        (clamped, None)
    } else {
        (new_freq, Some(new_freq))
    }
}

/// Changing sample rate means WDSP's channel (fixed input rate at
/// OpenChannel time) has to be recreated, which means SpectrumHandle
/// has to be recreated, which means rigctl/TCI (which hold a clone of
/// the *old* SpectrumHandle's DemodParams Arc) would go stale unless
/// they're recreated too. So this restarts everything downstream of
/// the radio session itself, preserving current mode/width/gain/AGC
/// settings across the restart rather than resetting to defaults.
/// Activates and spawns a new extra receiver on `session`, optionally
/// applying saved settings (used both by the "Add Receiver" button --
/// saved=None, defaults -- and by auto-restoring from config on
/// connect -- saved=Some(...)).
fn spawn_extra_receiver(
    session: &RadioSession,
    num_adcs: u8,
    protocol: u8,
    settings_dirty: Arc<std::sync::atomic::AtomicBool>,
    saved: Option<&ExtraReceiverConfig>,
) -> Option<Arc<Mutex<ExtraReceiver>>> {
    let idx = session.add_receiver()?;
    let freq_arc = Arc::clone(&session.extra_frequencies_hz[idx - 1]);
    let rate_arc = Arc::clone(&session.extra_sample_rates_hz[idx - 1]);
    let adc_arc = Arc::clone(&session.extra_adcs[idx - 1]);
    let antenna_arc = Arc::clone(&session.antenna);

    if let Some(s) = saved {
        freq_arc.store(s.frequency_hz, Ordering::Relaxed);
        rate_arc.store(s.sample_rate_hz, Ordering::Relaxed);
        adc_arc.store(s.adc as u32, Ordering::Relaxed);
    }
    let rate_val = rate_arc.load(Ordering::Relaxed);

    let iq_buffer = Arc::clone(&session.iq_buffers[idx]);
    let spectrum = SpectrumHandle::start(idx as i32, Arc::clone(&iq_buffer), rate_val as i32);

    if let Some(s) = saved {
        spectrum.set_mode(s.mode);
        spectrum.set_width_hz(s.width_hz);
        spectrum.set_gain(s.gain);
        spectrum.set_agc(s.agc);
        spectrum.set_agc_attack_ms(s.agc_attack_ms);
        spectrum.set_agc_decay_ms(s.agc_decay_ms);
        spectrum.set_agc_hang_ms(s.agc_hang_ms);
        spectrum.set_agc_top_db(s.agc_top_db);
        spectrum.set_agc_slope_db(s.agc_slope_db);
        spectrum.set_agc_thresh_db(s.agc_thresh_db);
        spectrum.set_noise_blanker(s.noise_blanker);
        spectrum.set_nb_threshold(s.nb_threshold);
        spectrum.set_noise_reduction(s.noise_reduction);
        spectrum.set_snb(s.snb);
    }

    let audio_output = AudioOutput::start(Arc::clone(&spectrum.audio_out)).ok();
    let initial_frequency_hz = freq_arc.load(Ordering::Relaxed);

    Some(Arc::new(Mutex::new(ExtraReceiver {
        ddc_index: idx,
        iq_buffer,
        frequency_hz: freq_arc,
        sample_rate_hz: rate_arc,
        adc: adc_arc,
        num_adcs,
        protocol,
        antenna: antenna_arc,
        spectrum,
        audio_output,
        waterfall_texture: None,
        waterfall_signature: None,
        scroll_accum: 0.0,
        slider_scroll_accum: 0.0,
        db_low: saved.map(|s| s.db_low).unwrap_or(-140.0),
        db_high: saved.map(|s| s.db_high).unwrap_or(-40.0),
        waterfall_db_low: saved.map(|s| s.waterfall_db_low).unwrap_or(-140.0),
        waterfall_db_high: saved.map(|s| s.waterfall_db_high).unwrap_or(-60.0),
        waterfall_palette: saved.map(|s| s.waterfall_palette).unwrap_or(Palette::Ocean),
        spectrum_waterfall_ratio: saved.map(|s| s.spectrum_waterfall_ratio).unwrap_or(150.0 / 350.0),
        show_settings_window: false,
        settings_tab: SettingsTab::Agc,
        settings_dirty,
        band_memory: saved.map(|s| s.band_settings.clone()).unwrap_or_default(),
        width_memory: saved.map(|s| s.width_memory.clone()).unwrap_or_default(),
        ctun: false,
        ctun_frequency_hz: initial_frequency_hz,
        open: true,
    })))
}

fn change_sample_rate(connected: &mut ConnectedState, new_rate: u32) {
    let mode = connected.spectrum.mode();
    let width_hz = connected.spectrum.width_hz();
    let gain = connected.spectrum.gain();
    let agc = connected.spectrum.agc();
    let agc_params = connected.spectrum.agc_params();

    connected.session.set_sample_rate(new_rate);

    // Explicitly tear down everything that depends on the old WDSP
    // channel BEFORE creating a replacement. Otherwise the new
    // SpectrumHandle would open WDSP channel 0 again while the old
    // background thread still owns it (WDSP isn't confirmed thread-safe
    // for concurrent access to the same channel) -- simply reassigning
    // `connected.spectrum` at the end doesn't help, since Rust builds
    // the new value (and thus opens the channel) before dropping the
    // old one.
    //
    // rigctl/TCI themselves are NOT torn down here anymore -- see
    // RigctlServer::set_demod_params's doc comment. They keep running
    // (and keep any already-connected client, e.g. WSJT-X, connected)
    // and are just pointed at the new SpectrumHandle's DemodParams
    // below, once it exists.
    connected.audio_output = None;
    connected.spectrum.stop();

    let spectrum = SpectrumHandle::start(0, Arc::clone(&connected.session.iq_buffers[0]), new_rate as i32);
    spectrum.set_mode(mode);
    spectrum.set_width_hz(width_hz);
    spectrum.set_gain(gain);
    spectrum.set_agc(agc);
    spectrum.set_agc_attack_ms(agc_params.agc_attack_ms);
    spectrum.set_agc_decay_ms(agc_params.agc_decay_ms);
    spectrum.set_agc_hang_ms(agc_params.agc_hang_ms);
    spectrum.set_agc_top_db(agc_params.agc_top_db);
    spectrum.set_agc_slope_db(agc_params.agc_slope_db);
    spectrum.set_agc_thresh_db(agc_params.agc_thresh_db);
    spectrum.set_noise_blanker(agc_params.noise_blanker);
    spectrum.set_nb_threshold(agc_params.nb_threshold);
    spectrum.set_noise_reduction(agc_params.noise_reduction);
    spectrum.set_snb(agc_params.snb);

    connected.audio_output = match AudioOutput::start(Arc::clone(&spectrum.audio_out)) {
        Ok(a) => Some(a),
        Err(e) => {
            eprintln!("audio output unavailable after sample rate change: {e}");
            None
        }
    };
    if let Some(s) = &connected.rigctl_server {
        s.set_demod_params(spectrum.demod_params_handle());
    }
    if let Some(s) = &connected.tci_server {
        s.set_demod_params(spectrum.demod_params_handle());
        // Points any already-streaming client at this new
        // SpectrumHandle's queues -- see TciServer::set_audio_iq's doc
        // comment for why this can't be skipped the same way
        // set_demod_params can't be.
        s.set_audio_iq(Arc::clone(&spectrum.tci_audio_out), Arc::clone(&spectrum.iq_out));
    }

    connected.spectrum = spectrum;
    connected.sample_rate = new_rate;

    // P1 has one shared RX/TX clock (no separate DUC rate the way P2
    // has a fixed 192ksps regardless of RX rate) -- if TX is armed,
    // tx.rs's TXA channel was opened expecting the *old* sample rate
    // and needs rebuilding against the new one, or TX audio would come
    // out at the wrong pitch/speed. Mic capture itself (audio.rs's
    // MicInput) is unaffected -- only the TXA channel's output rate
    // needs to change, so only tx_handle is torn down and recreated
    // here, not mic_input.
    if connected.tx_enabled && connected.device.protocol == 1 {
        if let Some(old_tx) = connected.tx_handle.take() {
            let mic_gain = connected.mic_gain;
            drop(old_tx); // stop the old TXA thread before opening a new one on the same WDSP TX channel
            if let Some(mic) = &connected.mic_input {
                let tx_handle = TxHandle::start(
                    Arc::clone(mic.buffer()),
                    Arc::clone(&connected.session.tci_tx_audio),
                    Arc::clone(&connected.session.tx_iq),
                    Arc::clone(&connected.session.mox),
                    connected.session.iq_buffers.len() as i32,
                    connected.device.protocol,
                    48_000,
                    new_rate as i32,
                    connected.puresignal_enabled,
                    Arc::clone(&connected.session.ps_rx_feedback_iq),
                    Arc::clone(&connected.session.ps_tx_feedback_iq),
                );
                tx_handle.set_mic_gain(mic_gain);
                tx_handle.set_mode(connected.spectrum.mode());
                tx_handle.set_width_hz(connected.spectrum.width_hz());
                tx_handle.set_ps_enabled(connected.ps_enabled);
                tx_handle.set_ps_hw_peak(connected.ps_hw_peak);
                tx_handle.set_ps_mox_delay(connected.ps_mox_delay);
                tx_handle.set_ps_loop_delay(connected.ps_loop_delay);
                tx_handle.set_ps_tx_delay_ns(connected.ps_tx_delay_ns);
                connected.tx_handle = Some(tx_handle);
            }
        }
    }

    // P1 has one shared clock for every receiver, unlike P2 where each
    // DDC can run its own independent rate -- keep every currently-open
    // extra receiver in sync with the new rate rather than letting it
    // silently go stale relative to what's actually arriving from the
    // radio (see render_extra_receiver_settings's matching P1
    // sample-rate-disable note, the other half of this).
    if connected.device.protocol == 1 {
        for rx in &connected.extra_receivers {
            let mut rx = rx.lock().unwrap();
            if rx.sample_rate_hz.load(Ordering::Relaxed) != new_rate {
                change_extra_receiver_sample_rate(&mut rx, new_rate);
            }
        }
    }
}

/// Same idea as change_sample_rate above, but for an extra receiver --
/// simpler since extra receivers aren't wired into rigctl/TCI (those
/// only expose the primary receiver), so there's nothing else holding a
/// reference to the old SpectrumHandle's state that needs recreating.
fn change_extra_receiver_sample_rate(rx: &mut ExtraReceiver, new_rate: u32) {
    let mode = rx.spectrum.mode();
    let width_hz = rx.spectrum.width_hz();
    let gain = rx.spectrum.gain();
    let agc = rx.spectrum.agc();
    let agc_params = rx.spectrum.agc_params();

    rx.sample_rate_hz.store(new_rate, Ordering::Relaxed);

    // Explicit teardown before creating replacements -- same reasoning
    // as change_sample_rate: the new SpectrumHandle would otherwise
    // open this receiver's WDSP channel again while the old background
    // thread still owns it.
    rx.audio_output = None;
    rx.spectrum.stop();

    let spectrum = SpectrumHandle::start(rx.ddc_index as i32, Arc::clone(&rx.iq_buffer), new_rate as i32);
    spectrum.set_mode(mode);
    spectrum.set_width_hz(width_hz);
    spectrum.set_gain(gain);
    spectrum.set_agc(agc);
    spectrum.set_agc_attack_ms(agc_params.agc_attack_ms);
    spectrum.set_agc_decay_ms(agc_params.agc_decay_ms);
    spectrum.set_agc_hang_ms(agc_params.agc_hang_ms);
    spectrum.set_agc_top_db(agc_params.agc_top_db);
    spectrum.set_agc_slope_db(agc_params.agc_slope_db);
    spectrum.set_agc_thresh_db(agc_params.agc_thresh_db);
    spectrum.set_noise_blanker(agc_params.noise_blanker);
    spectrum.set_nb_threshold(agc_params.nb_threshold);
    spectrum.set_noise_reduction(agc_params.noise_reduction);
    spectrum.set_snb(agc_params.snb);

    rx.audio_output = match AudioOutput::start(Arc::clone(&spectrum.audio_out)) {
        Ok(a) => Some(a),
        Err(e) => {
            eprintln!("audio output unavailable after sample rate change: {e}");
            None
        }
    };
    rx.spectrum = spectrum;
}

fn min_max(v: &[f32]) -> (f32, f32) {
    let mut min = f32::INFINITY;
    let mut max = f32::NEG_INFINITY;
    for &x in v {
        if x < min {
            min = x;
        }
        if x > max {
            max = x;
        }
    }
    (min, max)
}

/// Color-maps the waterfall row history (newest row first) into an
/// image. Each row is auto-ranged independently for now -- a fixed
/// calibrated range would need real fscLin/fscHin values, which weren't
/// set in the confirmed SetAnalyzer call (both left at 0.0).
/// Live progress text for the one-time FFTW wisdom-generation pass
/// (spectrum.rs's WDSPwisdom call), shown in place of the waterfall
/// while no rows have arrived yet on a fresh machine/config. WDSP
/// itself maintains this as a plain C global (wisdom.c's `status`
/// buffer, updated via sprintf on the RX spectrum thread as each FFT
/// size is planned) with no synchronization -- reading it from the UI
/// thread while it's being written is technically a data race, but
/// it's a short, frequently-overwritten status string read purely for
/// display, the same way upstream reference clients (e.g. piHPSDR)
/// poll it, so a torn read at worst shows one garbled frame of text
/// rather than anything unsafe.
fn wisdom_status_text() -> String {
    const FALLBACK: &str = "Creating FFTW Wisdom File...";
    unsafe {
        let ptr = wdsp_sys::wisdom_get_status();
        if ptr.is_null() {
            return FALLBACK.to_string();
        }
        match std::ffi::CStr::from_ptr(ptr).to_str() {
            Ok(s) if !s.trim().is_empty() => s.trim().to_string(),
            _ => FALLBACK.to_string(),
        }
    }
}

fn build_waterfall_image(
    rows: &[Vec<f32>],
    palette: Palette,
    db_low: f32,
    db_high: f32,
) -> Option<egui::ColorImage> {
    if rows.is_empty() || rows[0].is_empty() {
        return None;
    }
    let width = rows[0].len();
    // Fixed height from the start (rather than rows.len(), which grows
    // from 1 to WATERFALL_HISTORY over time) so the image doesn't need
    // to "fill up" to look right -- new rows land at the top, the rest
    // stays black until real data arrives there.
    let height = spectrum::WATERFALL_HISTORY;
    let mut image = egui::ColorImage::new([width, height], vec![egui::Color32::BLACK; width * height]);

    // Same fixed range as the spectrum trace/gridlines, rather than
    // each row auto-normalizing to its own min/max -- keeps waterfall
    // color and spectrum trace level in sync with each other.
    let range = (db_high - db_low).max(1.0);
    for (row_idx, row) in rows.iter().enumerate().take(height) {
        for (col_idx, &v) in row.iter().enumerate() {
            if col_idx >= width {
                break;
            }
            let t = ((v - db_low) / range).clamp(0.0, 1.0);
            image.pixels[row_idx * width + col_idx] = palette.color(t);
        }
    }

    Some(image)
}

#[derive(Copy, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Palette {
    Classic,
    Fire,
    Ocean,
    Grayscale,
}

const ALL_PALETTES: [Palette; 4] = [Palette::Fire, Palette::Ocean, Palette::Classic, Palette::Grayscale];

impl Palette {
    fn label(self) -> &'static str {
        match self {
            Palette::Classic => "Classic",
            Palette::Fire => "Fire",
            Palette::Ocean => "Ocean",
            Palette::Grayscale => "Grayscale",
        }
    }

    fn color(self, t: f32) -> egui::Color32 {
        let t = t.clamp(0.0, 1.0);
        let (r, g, b) = match self {
            // black -> blue -> green -> yellow -> red. Most typical
            // noise-floor-dominated data clusters mid-range, which
            // lands in this map's green/yellow band -- hence why it
            // reads as "very yellow and green" in practice.
            Palette::Classic => {
                if t < 0.25 {
                    (0.0, 0.0, t / 0.25)
                } else if t < 0.5 {
                    let k = (t - 0.25) / 0.25;
                    (0.0, k, 1.0 - k)
                } else if t < 0.75 {
                    let k = (t - 0.5) / 0.25;
                    (k, 1.0, 0.0)
                } else {
                    let k = (t - 0.75) / 0.25;
                    (1.0, 1.0 - k, 0.0)
                }
            }
            // black -> red -> orange -> yellow -> white. Common SDR
            // waterfall default -- spreads warm colors earlier, less
            // green-dominated for typical noise-floor data.
            Palette::Fire => {
                if t < 0.4 {
                    let k = t / 0.4;
                    (k, 0.0, 0.0)
                } else if t < 0.75 {
                    let k = (t - 0.4) / 0.35;
                    (1.0, k * 0.65, 0.0)
                } else {
                    let k = (t - 0.75) / 0.25;
                    (1.0, 0.65 + k * 0.35, k)
                }
            }
            // black -> blue -> cyan -> white. Cooler alternative, easy
            // on the eyes for long monitoring sessions.
            Palette::Ocean => {
                if t < 0.5 {
                    let k = t / 0.5;
                    (0.0, 0.0, k)
                } else if t < 0.8 {
                    let k = (t - 0.5) / 0.3;
                    (0.0, k, 1.0)
                } else {
                    let k = (t - 0.8) / 0.2;
                    (k, 1.0, 1.0)
                }
            }
            Palette::Grayscale => (t, t, t),
        };
        egui::Color32::from_rgb((r * 255.0) as u8, (g * 255.0) as u8, (b * 255.0) as u8)
    }
}

fn main() -> eframe::Result<()> {
    // Force winit's X11 backend (via XWayland) rather than native Wayland.
    // Confirmed via `perf record`/`strace -p` on a running session: winit's
    // Wayland backend pegged one CPU core continuously on this system --
    // tens of thousands of epoll_ctl/timerfd_settime/epoll_pwait cycles per
    // second on the main UI thread alone (~150us/cycle), not a normal
    // blocking wait -- while switching to X11 (WAYLAND_DISPLAY cleared)
    // eliminated it entirely on the same session, same hardware. Root cause
    // not pinned down further (deep in winit/calloop's Wayland event-loop
    // internals; egui-winit/accesskit's AT-SPI/D-Bus stack was ruled out
    // and removed separately -- see Cargo.toml). winit 0.30 selects its
    // Linux backend purely by checking WAYLAND_DISPLAY/WAYLAND_SOCKET at
    // startup (see winit::platform_impl::linux::mod.rs), so clearing it
    // here -- before eframe/winit ever read it, and before any other
    // threads exist -- is enough to force X11 without needing the user to
    // remember an env var on every launch. Revisit if a real Wayland fix
    // ever lands upstream and this workaround is no longer needed.
    unsafe {
        std::env::remove_var("WAYLAND_DISPLAY");
    }

    // eframe's default inner size (~430x300 as of 0.35) is far too
    // small to show the whole main-window layout at once (band/mode
    // rows, spectrum, waterfall, and the Stop button all stack
    // vertically) -- start large enough that everything is visible
    // without the user having to resize first. Still resizable/
    // shrinkable afterward; min_inner_size just keeps it from being
    // dragged down to something unusably cramped again.
    //
    // Height reduced twice now: 950 -> 700 (estimate) -> 660 (this
    // time pixel-measured directly against an actual screenshot at
    // 1200x700 -- laid-out content, including the Stop button, ended
    // at y=637, leaving a 63px empty gap below it. 660 leaves a small,
    // deliberate margin rather than an exact fit, since content height
    // varies a little with things like whether TX is armed (extra
    // Mic gain/TX Power controls on the Audio gain row).
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1200.0, 660.0])
            .with_min_inner_size([900.0, 520.0]),
        ..Default::default()
    };
    eframe::run_native(
        "hpsdr-rs",
        options,
        Box::new(|cc| Ok(Box::new(HpsdrApp::new(&cc.egui_ctx)))),
    )
}
