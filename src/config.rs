/*
    Persists user-adjustable settings (mode, filter width, gain, AGC
    mode + tuning, spectrum display range, waterfall palette, last
    tuned frequency) across restarts as a small JSON file.

    One config file per radio, named after its MAC address, so
    different physical radios keep independent saved settings rather
    than sharing/overwriting one config.

    NOTE: config path uses $HOME directly -- Linux-only for now, same
    caveat as libwdsp.a itself. Windows/macOS would need a different
    path convention (e.g. %APPDATA% / ~/Library/Application Support).
*/

use crate::spectrum::{Agc, Mode, NoiseBlanker, NoiseReduction};
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
    pub agc: Option<Agc>,
    pub agc_attack_ms: Option<i32>,
    pub agc_decay_ms: Option<i32>,
    pub agc_hang_ms: Option<i32>,
    pub agc_top_db: Option<f64>,
    pub agc_slope_db: Option<i32>,
    pub agc_thresh_db: Option<f64>,
    pub db_low: Option<f32>,
    pub db_high: Option<f32>,
    pub waterfall_db_low: Option<f32>,
    pub waterfall_db_high: Option<f32>,
    pub waterfall_palette: Option<Palette>,
    /// Spectrum's share (0.0-1.0) of the combined spectrum+waterfall
    /// height -- draggable via the divider between them. Missing falls
    /// back to their old fixed 150/350 proportions.
    pub spectrum_waterfall_ratio: Option<f32>,
    pub adc: Option<u8>,
    pub antenna: Option<u8>,
    pub rigctl_addr: Option<String>,
    pub tci_addr: Option<String>,
    /// Whether rigctl/TCI were running at last save -- since starting
    /// them is now a manual action (Settings -> Network) rather than
    /// automatic, this is what lets a reconnect restore "was running"
    /// state instead of always coming back stopped. `None`/missing
    /// (e.g. configs saved before this existed) is treated as "was
    /// not running", matching the old default-off behavior.
    #[serde(default)]
    pub rigctl_running: Option<bool>,
    #[serde(default)]
    pub tci_running: Option<bool>,
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
    /// TX mic gain. Deliberately one of very few TX settings persisted
    /// -- whether TX was armed (tx_enabled) is intentionally NOT saved
    /// (see main.rs's auto-arm-on-connect comment).
    pub mic_gain: Option<f32>,
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
}

fn default_spectrum_waterfall_ratio() -> f32 {
    150.0 / 350.0
}

fn config_path(mac: [u8; 6]) -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let mut path = PathBuf::from(home);
    path.push(".config");
    path.push("hpsdr-rs");
    if std::fs::create_dir_all(&path).is_err() {
        return None;
    }
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
    let home = std::env::var_os("HOME")?;
    let mut path = PathBuf::from(home);
    path.push(".config");
    path.push("hpsdr-rs");
    if std::fs::create_dir_all(&path).is_err() {
        return None;
    }
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
