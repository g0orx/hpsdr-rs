/*
    Persists user-adjustable settings (mode, filter width, gain, AGC
    mode + tuning, spectrum display range, waterfall palette, last
    tuned frequency) across restarts as a small JSON file.

    One config file per radio, named after its MAC address, so
    different physical radios keep independent saved settings rather
    than sharing/overwriting one config.
*/

use crate::spectrum::{Agc, EqualizerParams, Mode, NoiseBlanker, NoiseReduction};
use crate::{BandSettings, Palette};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Default)]
pub struct Config {
    pub frequency_hz: Option<u32>,
    pub sample_rate: Option<u32>,
    pub mode: Option<Mode>,
    pub width_hz: Option<f64>,
    pub gain: Option<f32>,
    /// Output device for local RX audio playback (Settings -> Audio's
    /// "Output device" picker), by name -- e.g. "CABLE Input (VB-Audio
    /// Virtual Cable)" to feed a decoder instead of/alongside real
    /// speakers. `None`/missing (configs saved before this existed)
    /// falls back to the system default, same as this always did before
    /// device selection existed. An unrecognized name (the saved device
    /// no longer present on this machine) also falls back to the
    /// default rather than erroring -- see AudioOutput::start's doc
    /// comment.
    #[serde(default)]
    pub audio_output_device: Option<String>,
    /// Input device for TX mic audio (Settings -> Audio's "Input device"
    /// picker), by name -- e.g. "CABLE Output (VB-Audio Virtual Cable)"
    /// to feed TX audio from a virtual cable instead of/alongside a real
    /// mic. Same `None`/unrecognized-name fallback-to-default contract
    /// as `audio_output_device` above -- see MicInput::start's doc
    /// comment.
    #[serde(default)]
    pub mic_input_device: Option<String>,
    pub agc: Option<Agc>,
    pub agc_attack_ms: Option<i32>,
    pub agc_decay_ms: Option<i32>,
    pub agc_hang_ms: Option<i32>,
    pub agc_top_db: Option<f64>,
    pub agc_slope_db: Option<i32>,
    pub agc_thresh_db: Option<f64>,
    pub db_low: Option<f32>,
    pub db_high: Option<f32>,
    /// "Auto" mode for db_low -- see ConnectedState::db_low_auto's doc
    /// comment (main.rs). Missing (a config saved before this existed)
    /// falls back to on -- the default for this feature.
    #[serde(default)]
    pub db_low_auto: Option<bool>,
    pub waterfall_db_low: Option<f32>,
    pub waterfall_db_high: Option<f32>,
    pub waterfall_palette: Option<Palette>,
    /// Spectrum's share (0.0-1.0) of the combined spectrum+waterfall
    /// height -- draggable via the divider between them. Missing falls
    /// back to their old fixed 150/350 proportions.
    pub spectrum_waterfall_ratio: Option<f32>,
    /// Spectrum/waterfall zoom/pan -- see ConnectedState::spectrum_zoom/
    /// spectrum_pan's doc comments (main.rs). Missing (a config saved
    /// before this existed) falls back to zoom 1 / pan 0.0, i.e. the
    /// full sample-rate span, unzoomed -- this feature's own "off" state.
    #[serde(default)]
    pub spectrum_zoom: Option<i32>,
    #[serde(default)]
    pub spectrum_pan: Option<f32>,
    pub adc: Option<u8>,
    pub antenna: Option<u8>,
    pub rigctl_addr: Option<String>,
    pub tci_addr: Option<String>,
    /// Kenwood TS-2000 CAT emulation address -- see cat.rs's module doc
    /// comment. Same `#[serde(default)]`/missing-means-off treatment as
    /// rigctl_running/tci_running below (this field predates their
    /// addition, so it can't share their attribute, but the same
    /// reasoning applies: a config saved before CAT existed has no
    /// value here).
    #[serde(default)]
    pub cat_addr: Option<String>,
    /// Whether rigctl/TCI/CAT were running at last save -- since starting
    /// them is now a manual action (Settings -> Network) rather than
    /// automatic, this is what lets a reconnect restore "was running"
    /// state instead of always coming back stopped. `None`/missing
    /// (e.g. configs saved before this existed) is treated as "was
    /// not running", matching the old default-off behavior.
    #[serde(default)]
    pub rigctl_running: Option<bool>,
    #[serde(default)]
    pub tci_running: Option<bool>,
    #[serde(default)]
    pub cat_running: Option<bool>,
    /// Debug logging to rigctl_log.txt/tci_log.txt/cat_log.txt (Settings
    /// -> Network) -- see debug_log.rs's own doc comment. Off by default
    /// (missing = `false`), same reasoning as every other Option<bool>
    /// here: a config saved before this existed shouldn't suddenly start
    /// logging.
    #[serde(default)]
    pub rigctl_logging_enabled: Option<bool>,
    #[serde(default)]
    pub tci_logging_enabled: Option<bool>,
    #[serde(default)]
    pub cat_logging_enabled: Option<bool>,
    /// Noise blanker (NB/NB2, mutually exclusive) and noise reduction
    /// (NR/NR2, mutually exclusive) state -- see the field docs on
    /// spectrum::DemodParams and the NoiseBlanker/NoiseReduction enums
    /// for what each one actually does.
    pub noise_blanker: Option<NoiseBlanker>,
    pub nb_threshold: Option<f64>,
    pub noise_reduction: Option<NoiseReduction>,
    /// SNB ("Spectral Noise Blanker") -- independent of noise_reduction
    /// above, see spectrum::DemodParams::snb's doc comment for why.
    pub snb: Option<bool>,
    /// Main receiver's graphic EQ -- see spectrum::EqualizerParams's doc
    /// comment. Each extra receiver window persists its own copy
    /// separately, see ExtraReceiverConfig::eq.
    pub rx_eq: Option<EqualizerParams>,
    /// TX mic gain. Deliberately one of very few TX settings persisted
    /// -- whether TX was armed (tx_enabled) is intentionally NOT saved
    /// (see main.rs's auto-arm-on-connect comment).
    pub mic_gain: Option<f32>,
    /// TX graphic EQ -- see spectrum::EqualizerParams's doc comment.
    pub tx_eq: Option<EqualizerParams>,
    /// Gain applied specifically to TX audio received from a TCI
    /// client (WSJT-X, TCI Remote, etc.), independent of mic_gain
    /// above -- see radio::RadioSession::tci_tx_gain's doc comment for
    /// why a real test needed these decoupled (WSJT-X's own TCI audio
    /// arrived roughly 700x quieter than mic_gain's range is
    /// calibrated for). Defaults to 1.0 (unchanged behavior) when unset.
    #[serde(default)]
    pub tci_tx_gain: Option<f32>,
    /// TX power target in watts, both protocols -- see
    /// radio::drive_byte_for_watts for how this becomes each protocol's
    /// actual wire-level drive byte. See mic_gain's note -- same
    /// reasoning for persisting this despite TX arming itself not being
    /// saved.
    pub tx_power_watts: Option<u32>,
    /// Per-band PA gain (dB) entered via the PA Calibration sliders
    /// (Settings -> TX), keyed by band name (see main.rs's BANDS).
    /// Feeds radio::drive_byte_for_watts in place of the flat
    /// radio::DEFAULT_PA_GAIN_DB fallback -- a band with no entry here
    /// just uses that fallback, same as an out-of-the-box,
    /// never-calibrated install would.
    #[serde(default)]
    pub pa_calibration: std::collections::HashMap<String, f32>,
    /// Upper bound (watts) for the main panel's TX Power slider --
    /// see main.rs's ConnectedState::max_tx_power_watts doc comment for
    /// why this has to be a per-radio (per-MAC) override rather than a
    /// fixed board-type default: the discovery protocol can't tell a
    /// 100W ANAN-100D and a 200W ANAN-8000DLE apart (both report as
    /// Orion2). Missing (e.g. never set, or configs saved before this
    /// existed) falls back to main.rs's default_max_tx_power_watts.
    pub max_tx_power_watts: Option<u32>,
    /// TX Power used while the Tune button is active, as a percentage
    /// of whatever the TX Power slider was set to at the moment TUNE
    /// was pressed (main.rs's pre_tune_power_watts) -- scales with
    /// your normal operating power rather than being a fixed ceiling.
    /// Missing falls back to a conservative 20%.
    pub tune_power_percent: Option<u32>,
    /// Spectrum/waterfall display range while transmitting -- separate
    /// from db_low/db_high/waterfall_db_low/waterfall_db_high (which
    /// are for receiving) because a locally-picked-up TX signal is
    /// typically far stronger than the weak RX signals those are
    /// normally tuned for. Defaults (when unset) are derived from the
    /// RX range plus headroom, not independent hardcoded values -- see
    /// where these are read in main.rs.
    pub tx_db_low: Option<f32>,
    pub tx_db_high: Option<f32>,
    pub tx_waterfall_db_low: Option<f32>,
    pub tx_waterfall_db_high: Option<f32>,
    #[serde(default)]
    pub band_settings: std::collections::HashMap<String, BandSettings>,
    /// Last filter width used per mode, keyed by Mode::label() (e.g.
    /// "USB") -- see main.rs's width_for_mode. A mode with no entry
    /// here (never used yet, or a config saved before this existed)
    /// falls back to spectrum::default_width_hz(mode), same as if this
    /// map didn't exist at all.
    #[serde(default)]
    pub width_memory: std::collections::HashMap<String, f64>,
    /// Extra receivers (beyond the primary one above), P2 only. On
    /// reconnect these are automatically recreated with their saved
    /// settings, matching however many were active last time.
    #[serde(default)]
    pub extra_receivers: Vec<ExtraReceiverConfig>,
    /// PureSignal (experimental, Phase 1 -- protocol-level feedback
    /// plumbing only, no WDSP predistortion engine wired up yet). Only
    /// takes effect on the NEXT connect -- see radio::RadioSettings's
    /// matching field doc comment for why this can't be a live toggle.
    #[serde(default)]
    pub puresignal_enabled: Option<bool>,
    /// Diversity reception (2-ADC boards only, Settings -> Diversity) --
    /// see radio::RadioSettings's matching field doc comment. Only takes
    /// effect on the next connect, same as puresignal_enabled (and
    /// mutually exclusive with it) -- see main.rs's Settings UI.
    #[serde(default)]
    pub diversity_enabled: Option<bool>,
    /// Diversity gain (dB, -27.0..27.0) / phase (degrees, -180.0..180.0)
    /// -- unlike diversity_enabled these ARE live-adjustable without a
    /// reconnect (see RadioSession::diversity_gain_db/diversity_phase_deg's
    /// doc comments); saved here purely so the last-tuned values survive
    /// a restart.
    #[serde(default)]
    pub diversity_gain_db: Option<f32>,
    #[serde(default)]
    pub diversity_phase_deg: Option<f32>,
    /// Protocol 1 RX step attenuator (0-31 dB), standard (non-HermesLite)
    /// boards only -- see radio::RadioSession::rx_attenuation's doc
    /// comment. Missing (e.g. configs saved before this existed)
    /// falls back to RadioSession::start's own default rather than the
    /// old hardcoded 0dB.
    pub rx_attenuation: Option<u32>,
    /// PureSignal calibration values (Settings -> PureSignal) -- see
    /// tx::PsParams's field docs for what each one means. Missing
    /// (e.g. configs saved before Phase 3 existed) falls back to the
    /// same reference defaults tx::PsParams::default uses.
    #[serde(default)]
    pub ps_hw_peak: Option<f64>,
    #[serde(default)]
    pub ps_mox_delay: Option<f64>,
    #[serde(default)]
    pub ps_loop_delay: Option<f64>,
    #[serde(default)]
    pub ps_tx_delay_ns: Option<f64>,
    /// PureSignal feedback TX-time step attenuator (0-31 dB, Protocol 1
    /// standard boards only) -- see radio::RadioSession::ps_tx_attenuation's
    /// doc comment. Missing falls back to RadioSettings::default's own
    /// 0dB (no attenuation, matching the old unconditional hardcoded
    /// behavior before this control existed).
    #[serde(default)]
    pub ps_tx_attenuation: Option<u32>,
    /// See tx::PsParams::ptol's doc comment. Missing falls back to
    /// WDSP's own reference default (0.8).
    #[serde(default)]
    pub ps_ptol: Option<f64>,
    /// See radio::RadioSession::send_rx_audio_to_radio's doc comment
    /// (Settings -> RX). Missing/never set falls back to off, same as
    /// the live default.
    #[serde(default)]
    pub send_rx_audio_to_radio: Option<bool>,
    /// See radio::RadioSession::tx_audio_source's doc comment (Settings
    /// -> TX) -- one of radio::TX_AUDIO_SOURCE_AUTO/RADIO_MIC/LOCAL_MIC.
    /// Missing/never set falls back to Auto, same as the live default.
    /// Renamed from the old `use_radio_mic: Option<bool>` when a third
    /// value was added -- an old saved config with that field just
    /// falls back to Auto once, same as never having been set.
    #[serde(default)]
    pub tx_audio_source: Option<u8>,
    /// See radio::RadioSession::mic_ptt_enabled/mic_bias_enabled/
    /// mic_ptt_on_tip's doc comments (Settings -> TX, standard boards
    /// only). Missing/never set falls back to off/off/"PTT on Ring",
    /// same as the live defaults.
    #[serde(default)]
    pub mic_ptt_enabled: Option<bool>,
    #[serde(default)]
    pub mic_bias_enabled: Option<bool>,
    #[serde(default)]
    pub mic_ptt_on_tip: Option<bool>,
    /// Main window's position/size as last seen for THIS radio -- keyed
    /// per-MAC (like the rest of this file) rather than globally, so
    /// each physical radio can reopen its window wherever it was last
    /// used, independent of any other radio's window. Applied once, via
    /// an explicit ViewportCommand right after connecting (see main.rs)
    /// -- the main window already exists by then (it's also the
    /// Discovery screen), so unlike a fresh viewport's ViewportBuilder
    /// this can't just be an initial hint.
    #[serde(default)]
    pub window_geometry: Option<WindowGeometry>,
    /// CTUN ("Click to Tune") state for the main receiver -- see
    /// ConnectedState::ctun's doc comment (main.rs) for what this
    /// actually does. `ctun_frequency_hz` is only meaningful/restored
    /// when `ctun` is `Some(true)`; otherwise a fresh connect just uses
    /// the dial frequency (`frequency_hz` above) for both, same as
    /// CTUN's own live "off" behavior.
    #[serde(default)]
    pub ctun: Option<bool>,
    #[serde(default)]
    pub ctun_frequency_hz: Option<u32>,
    /// VFO B / Split -- see ConnectedState::vfo_b_frequency_hz/split's
    /// doc comments (main.rs). `None`/missing falls back to A's
    /// frequency and Split off, respectively -- same "never leave a
    /// frequency field at a meaningless 0, and a config saved before
    /// this existed shouldn't suddenly start in Split" reasoning as
    /// ctun/ctun_frequency_hz above.
    #[serde(default)]
    pub vfo_b_frequency_hz: Option<u32>,
    #[serde(default)]
    pub split: Option<bool>,
    /// RIT / XIT -- see ConnectedState::rit_enabled/xit_enabled's doc
    /// comments (main.rs). `None`/missing falls back to off with a
    /// zero offset, same "a config saved before this existed shouldn't
    /// suddenly start in RIT/XIT" reasoning as ctun/split above.
    #[serde(default)]
    pub rit_enabled: Option<bool>,
    #[serde(default)]
    pub rit_offset_hz: Option<f64>,
    #[serde(default)]
    pub xit_enabled: Option<bool>,
    #[serde(default)]
    pub xit_offset_hz: Option<f64>,
    /// Configured transverters (up to 8, see main.rs's MAX_XVTRS) -- see
    /// main.rs's Xvtr struct doc comment. An empty `name` marks an unused
    /// slot. `#[serde(default)]` so configs saved before this existed just
    /// load with no transverters defined, same as every other feature
    /// added to this struct.
    #[serde(default)]
    pub xvtrs: Vec<crate::Xvtr>,
    /// Name of the XVTR slot that was active/displayed-through at last
    /// disconnect, if any -- see main.rs's ConnectedState::active_xvtr
    /// doc comment. `#[serde(default)]` so configs saved before this
    /// existed just start with no transverter active, same as every
    /// other feature added to this struct. Restoring this is safe (won't
    /// reintroduce the ambiguity active_xvtr itself exists to avoid)
    /// because it's restored verbatim as explicit state, not re-derived
    /// from the restored frequency -- and if the named slot no longer
    /// exists, or the restored frequency no longer falls within its
    /// range (e.g. its settings changed since), the very first frame's
    /// own auto-clear check (see the per-frame reconciliation block)
    /// clears it right back to None.
    #[serde(default)]
    pub active_xvtr: Option<String>,
    /// Per-band (or XVTR) Open Collector Rx/Tx masks -- see main.rs's
    /// OcMask struct doc comment. Keyed by band/XVTR name, same pattern
    /// as pa_calibration above. `#[serde(default)]` so configs saved
    /// before this existed just load with no OC outputs configured.
    #[serde(default)]
    pub oc_settings: std::collections::HashMap<String, crate::OcMask>,
    /// Global Open Collector mask ORed into the active band's Tx mask
    /// while TUNE is active -- see main.rs's ConnectedState::oc_tune
    /// doc comment.
    #[serde(default)]
    pub oc_tune: u8,
}

fn default_nb_threshold() -> f64 {
    20.0
}

#[derive(Serialize, Deserialize, Clone)]
pub struct ExtraReceiverConfig {
    pub frequency_hz: u32,
    pub sample_rate_hz: u32,
    pub adc: u8,
    #[serde(default)]
    pub band_settings: std::collections::HashMap<String, BandSettings>,
    /// See Config::width_memory's doc comment -- same thing, per extra
    /// receiver instead of shared across the session.
    #[serde(default)]
    pub width_memory: std::collections::HashMap<String, f64>,
    pub mode: Mode,
    pub width_hz: f64,
    pub gain: f32,
    /// See Config::audio_output_device's doc comment -- same thing, this
    /// receiver's own independent output device selection.
    #[serde(default)]
    pub audio_output_device: Option<String>,
    pub agc: Agc,
    pub agc_attack_ms: i32,
    pub agc_decay_ms: i32,
    pub agc_hang_ms: i32,
    pub agc_top_db: f64,
    pub agc_slope_db: i32,
    pub agc_thresh_db: f64,
    pub db_low: f32,
    pub db_high: f32,
    pub waterfall_db_low: f32,
    pub waterfall_db_high: f32,
    pub waterfall_palette: Palette,
    // Added after the fields above -- #[serde(default)] so extra
    // receivers saved by an older build (without these) still load
    // instead of failing the whole Config and losing every setting.
    #[serde(default)]
    pub noise_blanker: NoiseBlanker,
    #[serde(default = "default_nb_threshold")]
    pub nb_threshold: f64,
    #[serde(default)]
    pub noise_reduction: NoiseReduction,
    /// See Config::snb's doc comment.
    #[serde(default)]
    pub snb: bool,
    /// See Config::spectrum_waterfall_ratio's doc comment.
    #[serde(default = "default_spectrum_waterfall_ratio")]
    pub spectrum_waterfall_ratio: f32,
    /// See Config::rx_eq's doc comment -- same type, this receiver's own
    /// independent copy.
    #[serde(default)]
    pub eq: EqualizerParams,
    /// See Config::window_geometry's doc comment -- same thing, this
    /// receiver's own window. Unlike the main window, an extra
    /// receiver's viewport doesn't exist yet when this is read, so it's
    /// seeded straight into that viewport's initial ViewportBuilder
    /// (see main.rs's spawn_extra_receiver/show_viewport_deferred).
    #[serde(default)]
    pub window_geometry: Option<WindowGeometry>,
    /// See Config::ctun's doc comment -- same thing, this receiver's own.
    #[serde(default)]
    pub ctun: bool,
    #[serde(default)]
    pub ctun_frequency_hz: u32,
    /// See Config::spectrum_zoom/spectrum_pan's doc comments -- same
    /// thing, this receiver's own.
    #[serde(default = "default_spectrum_zoom")]
    pub spectrum_zoom: i32,
    #[serde(default)]
    pub spectrum_pan: f32,
    /// See Config::db_low_auto's doc comment -- same thing, this
    /// receiver's own.
    #[serde(default = "default_db_low_auto")]
    pub db_low_auto: bool,
    /// See Config::rit_enabled's doc comment -- same thing, this
    /// receiver's own. No XIT here -- extra receivers never transmit.
    #[serde(default)]
    pub rit_enabled: bool,
    #[serde(default)]
    pub rit_offset_hz: f64,
}

fn default_db_low_auto() -> bool {
    true
}

fn default_spectrum_zoom() -> i32 {
    1
}

fn default_spectrum_waterfall_ratio() -> f32 {
    150.0 / 350.0
}

/// Per-platform settings directory, created if it doesn't exist yet.
/// `config_path`/`ps_corr_path` both build on this rather than each
/// duplicating their own copy of the same platform logic.
///
/// BUG FIX: this used to build `$HOME/.config/hpsdr-rs` unconditionally
/// on every platform -- correct for Linux (matches the XDG convention),
/// but `$HOME` isn't normally set outside an MSYS2 shell on Windows
/// (confirmed via a real Windows/MSVC build+run session: the app ran
/// fine, but had no way to persist settings between runs), and even
/// where it is set, `.config` isn't the native Windows convention
/// anyway. Now branches by `target_os`: Windows uses `%APPDATA%\
/// hpsdr-rs` (the standard per-user roaming-settings location); macOS
/// uses `~/Library/Application Support/hpsdr-rs` (matching WDSP's own
/// C source, which already has a real `__APPLE__` code path -- see
/// build.rs's doc comment -- even though macOS isn't a built/tested
/// target yet); everything else (Linux, BSDs) keeps the original
/// `$HOME/.config/hpsdr-rs` behavior unchanged.
pub(crate) fn settings_dir() -> Option<PathBuf> {
    let mut path = if cfg!(target_os = "windows") {
        PathBuf::from(std::env::var_os("APPDATA")?)
    } else if cfg!(target_os = "macos") {
        let mut p = PathBuf::from(std::env::var_os("HOME")?);
        p.push("Library");
        p.push("Application Support");
        p
    } else {
        let mut p = PathBuf::from(std::env::var_os("HOME")?);
        p.push(".config");
        p
    };
    path.push("hpsdr-rs");
    std::fs::create_dir_all(&path).ok()?;
    Some(path)
}

/// A window's on-screen position and content size, in egui points
/// (matches `egui::ViewportBuilder::with_position`/`with_inner_size`'s
/// units -- see Config::window_geometry/ExtraReceiverConfig::window_geometry).
#[derive(Serialize, Deserialize, Clone, Copy)]
pub struct WindowGeometry {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

fn config_path(mac: [u8; 6]) -> Option<PathBuf> {
    let mut path = settings_dir()?;
    let [a, b, c, d, e, f] = mac;
    path.push(format!("config-{a:02x}-{b:02x}-{c:02x}-{d:02x}-{e:02x}-{f:02x}.json"));
    Some(path)
}

/// Per-radio PureSignal correction-table file, same MAC-keyed directory
/// convention as `config_path` -- written/read via WDSP's own
/// `PSSaveCorr`/`PSRestoreCorr` (tx.rs), not this project's own
/// serialization, so the `.dat` extension and internal format are
/// whatever WDSP itself uses, not something to parse here.
pub fn ps_corr_path(mac: [u8; 6]) -> Option<PathBuf> {
    let mut path = settings_dir()?;
    let [a, b, c, d, e, f] = mac;
    path.push(format!("ps_corr-{a:02x}-{b:02x}-{c:02x}-{d:02x}-{e:02x}-{f:02x}.dat"));
    Some(path)
}

impl Config {
    /// Loads the saved config for this specific radio (by MAC address),
    /// or a blank/default one if there isn't one yet (first run for
    /// this radio) or it can't be read/parsed for any reason.
    pub fn load(mac: [u8; 6]) -> Config {
        config_path(mac)
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, mac: [u8; 6]) {
        let Some(path) = config_path(mac) else {
            return;
        };
        if let Ok(json) = serde_json::to_string_pretty(self) {
            if let Err(e) = std::fs::write(&path, json) {
                eprintln!("failed to save config to {}: {e}", path.display());
            }
        }
    }
}
