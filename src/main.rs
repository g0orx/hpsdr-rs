mod audio;
mod bootloader;
mod bootloader_ui;
mod cat;
mod config;
mod debug_log;
mod discovery;
mod discovery_ui;
mod radio;
mod rigctl;
mod spectrum;
mod tci;
mod tx;
mod wdsp_sys;

use audio::{AudioOutput, MicInput};
use cat::CatServer;
use config::{ps_corr_path, Config, ExtraReceiverConfig, WindowGeometry};
use discovery::{manual_discovery, Boards, Device};
use discovery_ui::{DiscoveryAction, DiscoveryWindow};
use eframe::egui;
use radio::{
    IqSample, RadioSession, RadioSettings, TX_AUDIO_SOURCE_AUTO, TX_AUDIO_SOURCE_LOCAL_MIC,
    TX_AUDIO_SOURCE_RADIO_MIC,
};
use rigctl::RigctlServer;
use spectrum::{SpectrumHandle, ALL_MODES};
use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
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
}

const BANDS: [Band; 11] = [
    Band { name: "160m", low_hz: 1_800_000, high_hz: 2_000_000, default_hz: 1_900_000, default_mode: spectrum::Mode::Lsb },
    Band { name: "80m", low_hz: 3_500_000, high_hz: 4_000_000, default_hz: 3_573_000, default_mode: spectrum::Mode::Lsb },
    Band { name: "60m", low_hz: 5_330_000, high_hz: 5_406_000, default_hz: 5_357_000, default_mode: spectrum::Mode::Usb }, // USB by regulatory convention despite being below 10MHz
    Band { name: "40m", low_hz: 7_000_000, high_hz: 7_300_000, default_hz: 7_074_000, default_mode: spectrum::Mode::Lsb },
    Band { name: "30m", low_hz: 10_100_000, high_hz: 10_150_000, default_hz: 10_136_000, default_mode: spectrum::Mode::Usb },
    Band { name: "20m", low_hz: 14_000_000, high_hz: 14_350_000, default_hz: 14_074_000, default_mode: spectrum::Mode::Usb },
    Band { name: "17m", low_hz: 18_068_000, high_hz: 18_168_000, default_hz: 18_100_000, default_mode: spectrum::Mode::Usb },
    Band { name: "15m", low_hz: 21_000_000, high_hz: 21_450_000, default_hz: 21_074_000, default_mode: spectrum::Mode::Usb },
    Band { name: "12m", low_hz: 24_890_000, high_hz: 24_990_000, default_hz: 24_915_000, default_mode: spectrum::Mode::Usb },
    Band { name: "10m", low_hz: 28_000_000, high_hz: 29_700_000, default_hz: 28_074_000, default_mode: spectrum::Mode::Usb },
    Band { name: "6m", low_hz: 50_000_000, high_hz: 54_000_000, default_hz: 50_313_000, default_mode: spectrum::Mode::Usb },
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
    Audio,
    Agc,
    Spectrum,
    Tx,
    PaCalibration,
    PureSignal,
    Diversity,
    Equalizer,
    Firmware,
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
    /// Same Arc as RadioSession::mox -- MOX is a whole-session concept,
    /// not per-receiver. Kept here (not just read once at spawn time) so
    /// change_extra_receiver_sample_rate can pass it to a rebuilt
    /// SpectrumHandle too -- see SpectrumHandle::start's doc comment for
    /// why this receiver's own audio_output (local playback) needs it
    /// just as much as the main receiver does.
    mox: Arc<std::sync::atomic::AtomicBool>,
    spectrum: SpectrumHandle,
    audio_output: Option<AudioOutput>,
    /// Selected output device name (Settings -> RX's "Output device"
    /// picker) -- `None` = system default, same as this always used
    /// before device selection existed. See AudioOutput::start's own
    /// doc comment for why an unrecognized/no-longer-present name (e.g.
    /// a saved VB-Cable selection on a machine that doesn't have it
    /// installed) falls back to the default rather than erroring.
    audio_output_device: Option<String>,
    waterfall_texture: Option<egui::TextureHandle>,
    /// (SpectrumDisplay::revision, palette, db_low, db_high) the
    /// waterfall texture was last built from -- lets the UI skip
    /// re-cloning waterfall_rows and rebuilding/re-uploading the
    /// texture on repaints where nothing that affects its pixels has
    /// actually changed (new analyzer data, or the palette/range).
    waterfall_signature: Option<(u64, Palette, f32, f32)>,
    scroll_accum: f32,
    slider_scroll_accum: f32,
    /// See ConnectedState::drag_tune_accum_hz's doc comment -- same
    /// thing, per extra receiver instead of shared.
    drag_tune_accum_hz: f64,
    db_low: f32,
    /// See ConnectedState::db_low_auto's doc comment -- same thing, per
    /// extra receiver instead of shared.
    db_low_auto: bool,
    /// Runtime-only smoothing state for db_low_auto -- see
    /// ConnectedState::db_low_auto_smoothed's doc comment. Not persisted.
    db_low_auto_smoothed: Option<f32>,
    db_high: f32,
    waterfall_db_low: f32,
    waterfall_db_high: f32,
    waterfall_palette: Palette,
    /// See ConnectedState::spectrum_waterfall_ratio's doc comment --
    /// same thing, per extra receiver instead of shared.
    spectrum_waterfall_ratio: f32,
    /// See ConnectedState::spectrum_zoom/spectrum_pan's doc comments --
    /// same thing, per extra receiver instead of shared.
    spectrum_zoom: i32,
    spectrum_pan: f32,
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
    /// RIT ("Receiver Incremental Tuning") -- see ConnectedState::rit_enabled's
    /// doc comment for the full explanation; same behavior here, just
    /// per extra receiver instead of shared. No XIT here -- extra
    /// receivers never transmit.
    rit_enabled: bool,
    rit_offset_hz: f64,
    rit_scroll_accum: f32,
    open: bool,
    /// This window's current on-screen position/size, refreshed every
    /// frame it's rendered so the periodic per-radio Config save (see
    /// ui()'s AppState::Connected arm) always has a current value to
    /// write back out. Deliberately NOT what seeds the viewport's
    /// position/size on creation -- see initial_window_geometry's doc
    /// comment for why a live-changing value can't be used for that.
    window_geometry: Option<WindowGeometry>,
    /// This radio's saved position/size for this receiver (from
    /// ExtraReceiverConfig), set once at spawn_extra_receiver time and
    /// never touched again. Used only to seed the ViewportBuilder that
    /// creates this window's OS-level viewport.
    ///
    /// BUG FIX: this used to reuse the live-tracked window_geometry
    /// above for that too, rebuilding `.with_position()`/
    /// `.with_inner_size()` from its current value every frame -- looked
    /// harmless (a real ViewportBuilder position hint only actually
    /// moves an already-existing OS window at creation time, confirmed
    /// by reading eframe's `initialize_window`), but that's not the
    /// whole story: eframe ALSO diffs each frame's requested builder
    /// against the previous frame's via `viewport.builder.patch()`
    /// (glow_integration.rs's `initialize_or_update_viewport`) and
    /// issues an explicit OuterPosition/InnerSize command for whatever
    /// changed, even on an existing window. Feeding in a value that's
    /// different every frame (because it's read from the window's own
    /// live position) meant every single frame requested a "move" to
    /// wherever the window was roughly one frame ago -- fighting the
    /// user's own drag/resize in real time (confirmed via a real
    /// report: the window kept jumping around while being dragged).
    /// A seed value that's set once and never changes again is what
    /// keeps `patch()` from ever seeing a diff after that first frame.
    initial_window_geometry: Option<WindowGeometry>,
}

struct ConnectedState {
    device: Device,
    session: RadioSession,
    spectrum: SpectrumHandle,
    /// A SEPARATE analyzer fed with the actual generated TX IQ (via
    /// TxHandle's tx_spectrum_iq queue), not RX ADC samples -- matches
    /// piHPSDR/rustyHPSDR's own TX-spectrum architecture. `spectrum`
    /// (the RX analyzer) can't double as a TX monitor: it only ever
    /// sees whatever the receiver itself picks up over the air, which
    /// depends entirely on antenna/relay coupling and can be weak,
    /// badly overloaded (showing as a comb pattern), or simply absent
    /// depending on the radio's T/R isolation -- not a meaningful "is
    /// my transmitted signal clean" signal. Rendered in place of
    /// `spectrum` whenever `session.mox_active()` is true (see the
    /// main panel's spectrum-drawing code).
    tx_spectrum: SpectrumHandle,
    audio_output: Option<AudioOutput>,
    /// Selected output device name for `audio_output` above (Settings ->
    /// Audio's "Output device" picker) -- see ExtraReceiver's identical
    /// field doc comment. Deliberately NOT used for tx_audio_monitor_output
    /// below -- monitoring your own TX audio should go to your own
    /// speakers/headphones, not wherever RX audio's been routed (e.g. a
    /// virtual cable feeding a decoder).
    audio_output_device: Option<String>,
    /// Local playback of TxHandle::tx_audio_monitor -- see that field's
    /// doc comment. None when not actively monitoring (the common
    /// case); toggled on/off via Settings -> TX's "Monitor TX Audio"
    /// checkbox. A SEPARATE AudioOutput instance from `audio_output`
    /// above (that one is RX; this taps TX audio instead), so both can
    /// run at once without interfering -- though listening to your own
    /// TX audio while transmitting is naturally only useful set up
    /// through headphones/a mixer, not the radio's own speaker path.
    tx_audio_monitor_output: Option<AudioOutput>,
    rigctl_server: Option<RigctlServer>,
    tci_server: Option<TciServer>,
    cat_server: Option<CatServer>,
    waterfall_texture: Option<egui::TextureHandle>,
    /// (SpectrumDisplay::revision, palette, db_low, db_high) the
    /// waterfall texture was last built from -- lets the UI skip
    /// re-cloning waterfall_rows and rebuilding/re-uploading the
    /// texture on repaints where nothing that affects its pixels has
    /// actually changed (new analyzer data, or the palette/range).
    waterfall_signature: Option<(u64, Palette, f32, f32)>,
    scroll_accum: f32,
    zoom_accum: f32,
    /// Fractional-Hz leftover for click-and-drag tuning on the spectrum/
    /// waterfall -- same "accumulate the sub-step remainder across
    /// frames" pattern as scroll_accum, applied to drag_delta() instead
    /// of scroll input. Needed (not just rounding each frame's delta to
    /// the nearest 1kHz step outright) so a slow drag at high zoom --
    /// where a single frame's pixel delta can correspond to well under
    /// 1kHz -- doesn't just get truncated to a dead no-op every frame.
    drag_tune_accum_hz: f64,
    sample_rate: u32,
    db_low: f32,
    /// "Auto" mode for the Spectrum Low slider (Settings -> Spectrum):
    /// when on, db_low is continuously overwritten each frame (RX only,
    /// not while transmitting -- see the TX range's own doc comment for
    /// why TX is a fundamentally different scenario) from a smoothed
    /// tracking of the lowest level currently shown in the trace, so the
    /// noise floor stays pinned near the bottom of the display without
    /// manual re-adjustment as band conditions change. Excludes a few
    /// bins at each edge of the trace when finding that minimum, since
    /// WDSP's analyzer can show rolloff/artifacts right at the edges of
    /// the visible span that aren't representative of the real noise
    /// floor. Smoothed (see db_low_auto_smoothed) rather than snapping
    /// straight to the raw per-frame minimum so it doesn't visibly jump
    /// on every noise spike.
    db_low_auto: bool,
    /// Exponentially-smoothed state for db_low_auto, same ballistics
    /// pattern as smoothed_fwd_power/smoothed_rev_power above. `None`
    /// until the first frame with db_low_auto on (seeds from that
    /// frame's raw value instead of smoothing from 0.0). Runtime-only,
    /// not persisted -- there's nothing meaningful to resume across a
    /// restart, it just re-converges from the first frame's data.
    db_low_auto_smoothed: Option<f32>,
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
    /// Spectrum/waterfall zoom (1 = full sample-rate span, higher =
    /// narrower, higher-resolution visible window), set via the Zoom
    /// slider below the waterfall. Pushed to the analyzer thread every
    /// frame via SpectrumHandle::set_zoom_pan, which actually grows the
    /// live WDSP FFT size to genuinely resolve more detail (not just a
    /// visual crop/stretch of a fixed-resolution trace) -- see that
    /// method's/SpectrumAnalyzer::set_zoom_pan's doc comments, confirmed
    /// against piHPSDR/rustyHPSDR's own zoom implementations. Narrows
    /// the visible frequency window symmetrically around the dial
    /// (before Pan is applied) -- see the spectrum-drawing code's
    /// visible_half_span_hz/pan_offset_hz for the frequency-axis math
    /// (labels, band-edge markers, passband overlay) shared with that;
    /// the trace/waterfall themselves need no equivalent cropping code
    /// since WDSP already returns only the visible window's data.
    spectrum_zoom: i32,
    /// Pan position within the zoomed window, -1.0 (leftmost/lowest
    /// frequency the current zoom can reach) to +1.0 (rightmost/
    /// highest), 0.0 = centered on the dial. Has no visible effect at
    /// zoom 1.0 (nothing to pan to -- the full span is already shown),
    /// set via the Pan slider below the waterfall.
    spectrum_pan: f32,
    slider_scroll_accum: f32,
    show_settings_window: bool,
    settings_tab: SettingsTab,
    /// P2 in-application firmware update against THIS connected radio --
    /// see bootloader_ui::FirmwareUpdateWindow/bootloader.rs's own doc
    /// comments. `None` = not open, same toggle idiom as
    /// show_settings_window above.
    firmware_update: Option<bootloader_ui::FirmwareUpdateWindow>,
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
    /// The last value of session.requested_frequency_hz this app has
    /// already handled -- see that field's doc comment. Compared against
    /// its live value once per frame; a mismatch means a network client
    /// (rigctl/CAT/TCI) has requested a new frequency since, which gets
    /// reconciled through the same CTUN-aware resolve_tune path any
    /// other frequency change goes through, then this is updated to
    /// match so the same request isn't reapplied every frame.
    last_requested_frequency_hz: u32,
    /// VFO B -- a second, independently-remembered frequency. Set via
    /// the Copy A->B/Copy B->A/Swap A<->B buttons, or by scrolling
    /// directly on its own box (see vfo_b_scroll_accum below) -- unlike
    /// VFO A, this never drives a live receiver (this app has no second
    /// RX chain), so scrolling it just changes the stored value with no
    /// retune/CTUN/passband-clamp concerns. Used for TX when `split` is
    /// on (see that field's doc comment).
    vfo_b_frequency_hz: u32,
    /// Scroll accumulator for VFO B's own box -- same NOTCH-based
    /// accumulate-then-step scheme as `scroll_accum` below, kept
    /// separate so scrolling VFO A and VFO B can never cross-contaminate
    /// each other's pending sub-step motion.
    vfo_b_scroll_accum: f32,
    /// Split: when on, TX uses `vfo_b_frequency_hz` instead of the
    /// normal dial/CTUN frequency -- see the per-frame CTUN block in
    /// ui() where session.tx_frequency_hz is resolved (Split takes
    /// priority over CTUN there, matching standard rig convention:
    /// Split is a deliberate, explicit TX-frequency override). RX is
    /// unaffected -- this app has no dual-watch/second-RX-chain
    /// concept, so VFO A keeps receiving regardless of Split.
    split: bool,
    /// RIT ("Receiver Incremental Tuning"): when on, rit_offset_hz is
    /// added to the RXA demod shift (see spectrum::SpectrumHandle::
    /// set_ctun -- RIT and CTUN share WDSP's one RXA shift register, so
    /// the per-frame block sums whichever of ctun_offset_hz/rit_offset_hz
    /// are currently active into a single value/enable pushed there),
    /// same DSP-only mechanism as CTUN: the hardware/LO frequency stays
    /// fixed, only what's actually demodulated shifts. Unlike CTUN, RIT
    /// works independently of it -- CTUN moves the *displayed* listen
    /// point, RIT is a small fine-tuning nudge on top that never
    /// changes VFO-A's own displayed/logged frequency, matching
    /// standard rig convention (RIT is meant for zero-beating a
    /// slightly-off-frequency station without touching your actual
    /// dial or transmit frequency).
    rit_enabled: bool,
    rit_offset_hz: f64,
    /// Scroll accumulator for the RIT control -- same NOTCH-based
    /// accumulate-then-step scheme as `scroll_accum`, kept separate so
    /// scrolling RIT/XIT/VFO-A/VFO-B can never cross-contaminate each
    /// other's pending sub-step motion.
    rit_scroll_accum: f32,
    /// XIT ("Transmitter Incremental Tuning"): the TX-side equivalent of
    /// RIT, but implemented completely differently since WDSP has no
    /// TXA-side shift primitive (confirmed: nothing like SetRXAShiftFreq
    /// exists for TXA in wdsp_sys). Instead xit_offset_hz is added
    /// directly to the real tx_frequency_hz value sent to the radio's
    /// TX NCO/register -- the same genuine-hardware-retune path Split
    /// already uses (see `split`'s doc comment), which both protocols
    /// keep continuously live and independent of the RX frequency
    /// regardless of MOX state, so there's no settling-time concern
    /// distinct from what Split already has. Composes with Split (XIT
    /// nudges whichever TX frequency -- VFO A or, if Split is on, VFO
    /// B -- is already selected) the same way RIT composes with CTUN.
    xit_enabled: bool,
    xit_offset_hz: f64,
    xit_scroll_accum: f32,
    rigctl_addr: String,
    tci_addr: String,
    cat_addr: String,
    /// Debug logging toggles (Settings -> Network) -- see
    /// debug_log.rs's own doc comment. Constructed once per connection
    /// (not per Start/Stop of the server itself), and handed (cloned) to
    /// whichever RigctlServer/TciServer/CatServer is currently running so
    /// toggling the checkbox takes effect immediately without needing to
    /// restart that server.
    rigctl_debug_log: debug_log::DebugLog,
    tci_debug_log: debug_log::DebugLog,
    cat_debug_log: debug_log::DebugLog,
    /// Set when a manual Start from the Network tab fails (e.g. port
    /// already in use); cleared on the next Start attempt. rigctl/TCI/CAT
    /// no longer auto-start on connect, so there's no "unavailable at
    /// startup" case to report here -- only ones the user triggered.
    rigctl_error: Option<String>,
    tci_error: Option<String>,
    cat_error: Option<String>,
    /// TX is armed automatically on connect (MicInput/TxHandle created
    /// right away, PTT control visible immediately) -- this flag is
    /// still tracked (and can still be turned off mid-session via
    /// Settings -> TX) but no longer requires a manual per-session
    /// arming step by default.
    tx_enabled: bool,
    mic_input: Option<MicInput>,
    /// Selected input device name for `mic_input` above (Settings ->
    /// Audio's "Input device" picker) -- `None` = system default. Same
    /// fallback-to-default contract as `audio_output_device` (see its doc
    /// comment) via MicInput::start.
    mic_input_device: Option<String>,
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
    /// Cached UI copy of session.tci_tx_gain -- see that field's doc
    /// comment (radio.rs) for what it does. Written through to the
    /// live Arc<Mutex<f32>> on change, same "cache here, write-through"
    /// pattern as mic_gain above.
    tci_tx_gain: f32,
    /// PureSignal calibration values (Settings -> PureSignal), same
    /// "tracked here, pushed to TxHandle on change" pattern as
    /// mic_gain above -- see tx::PsParams's field docs for what each
    /// one means. `ps_enabled` is the LIVE engine on/off (tx::PsParams::
    /// enabled), distinct from `puresignal_enabled` above (which only
    /// gates the connect-time feedback-receiver wire request).
    ps_enabled: bool,
    /// See tx::PsParams::oneshot's doc comment. Not persisted, same as
    /// ps_enabled -- always starts false (continuous) each session.
    ps_oneshot: bool,
    ps_hw_peak: f64,
    ps_mox_delay: f64,
    ps_loop_delay: f64,
    ps_tx_delay_ns: f64,
    /// See tx::PsParams::ptol's doc comment.
    ps_ptol: f64,
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
    /// Whether the Two-Tone test button is currently engaged --
    /// mutually exclusive with tune_active (mirrors it structurally,
    /// including reusing pre_tune_power_watts/tune_power_percent for
    /// the same "reduced power while testing" mechanism). See
    /// tx::PsParams::two_tone's doc comment for why this exists as a
    /// DISTINCT control from Tune, not just a variant of it --
    /// PureSignal calibration actually requires a varying-envelope
    /// signal that a steady Tune tone can never provide.
    two_tone_active: bool,
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
    /// See RadioSession::tx_fifo_underrun's doc comment. Latched for a
    /// couple of seconds after last seen set (same reasoning as
    /// piHPSDR's own rx_panadapter.c: a single status packet's worth
    /// of "true" would otherwise be too brief to actually notice at
    /// this UI's frame rate).
    tx_fifo_warning_until: Option<Instant>,
    /// Edge-tracks session.mox_active() so tx_spectrum.clear_display()
    /// only fires once per fresh PTT (not every frame while
    /// transmitting) -- see that method's doc comment for why a long-
    /// lived tx_spectrum otherwise keeps showing a blend of whatever a
    /// previous, possibly very different transmission looked like.
    tx_spectrum_mox_was_active: bool,
    /// PureSignal (experimental, Phase 1 -- protocol plumbing only, see
    /// radio::RadioSettings::puresignal_enabled). Reflects what THIS
    /// session was actually started with -- editable in Settings, but
    /// only takes effect on the next connect (can't be toggled live,
    /// same reasoning as sample_rate's Add Receiver interaction: the
    /// wire-level receiver/DDC count is fixed for the life of the
    /// sender/receiver threads).
    puresignal_enabled: bool,
    /// Diversity (2-ADC boards, see radio::RadioSession::diversity_enabled).
    /// Same "reflects what THIS session started with, editable in
    /// Settings but only takes effect on next connect" staging as
    /// puresignal_enabled just above, and for the identical reason
    /// (fixed wire-level DDC layout) -- mutually exclusive with it, see
    /// the Diversity/PureSignal tabs' checkbox handlers.
    diversity_enabled: bool,
    /// Which side (RX vs TX) the main window's Equalizer tab is
    /// currently showing -- purely a UI selection, not persisted (always
    /// reopens on RX). See the SettingsTab::Equalizer match arm.
    eq_tab_is_tx: bool,
    /// Edge-detection for auto-saving the PS correction table -- see
    /// the "PS" badge's own doc comment (main panel toolbar) for where
    /// this is checked each frame. True once a false->true
    /// `PsStatus::correcting` transition has been seen and saved this
    /// session, so it only saves once per transition, not every frame
    /// `correcting` stays true.
    ps_was_correcting: bool,
    /// General-purpose one-line status message, shown next to the main
    /// window's Stop button (see its rendering code). Added specifically
    /// because the existing FFTW-wisdom-generation status (shown as an
    /// overlay on the waterfall area while no rows have arrived yet --
    /// see `wisdom_status_text`) went unnoticed in a real report, since
    /// it only appears somewhere a user might not be looking during
    /// startup. Any code with a `&mut ConnectedState` can set this to
    /// surface a message here; `None` shows nothing. No history/queue by
    /// design -- just the current message, overwritten by the next one
    /// that gets set (matches how little is actually needed here today;
    /// revisit if a real future need for multiple/queued messages shows
    /// up).
    status_message: Option<String>,
}

enum AppState {
    Discovering(DiscoveryWindow),
    Connected(ConnectedState),
    Error(String),
}

struct HpsdrApp {
    state: AppState,
    // See the focus-transition check at the top of `ui()` for what this
    // tracks and why. Starts `true` so the very first frame (window not
    // focused yet on some platforms/WMs, but nothing was clicked to get
    // here) never triggers a spurious disable.
    was_focused: bool,
    // Interaction is disabled while `Instant::now()` is before this --
    // see `ui()`'s doc comment for why a single-frame check isn't
    // enough and this needs to be a short window instead.
    ignore_interaction_until: Option<Instant>,
    /// This window's current on-screen position/size, refreshed every
    /// frame (see `ui()`) so it's always ready to persist into the
    /// connected radio's own Config -- see Config::window_geometry's doc
    /// comment for why this lives per-radio rather than globally, and
    /// why it's applied via an explicit ViewportCommand at connect time
    /// (in the DiscoveryAction::Start handler) instead of a
    /// ViewportBuilder hint like every other window here: this one
    /// already exists (it's also the Discovery screen) by the time a
    /// radio -- and so its saved geometry -- is even known.
    main_window_geometry: Option<WindowGeometry>,
}

impl HpsdrApp {
    fn new(ctx: &egui::Context) -> Self {
        // Pin the app to dark, rather than leaving egui's default
        // ThemePreference::System in effect. This app's whole design
        // assumes dark by default -- the Settings/extra-receiver-settings
        // windows deliberately override to light on top of that (see
        // their own "light theme override" comments) rather than the
        // other way around. Without pinning this, egui silently follows
        // the OS theme: on a real report, this looked white/light
        // throughout on Windows (winit reliably reports the actual system
        // theme there), while looking fine on Linux, where system theme
        // detection generally isn't available and egui was quietly
        // falling back to its own built-in dark default instead.
        ctx.set_theme(egui::ThemePreference::Dark);
        Self {
            state: AppState::Discovering(DiscoveryWindow::new(ctx)),
            was_focused: true,
            ignore_interaction_until: None,
            main_window_geometry: None,
        }
    }
}

/// Builds a fresh `ConnectedState` for `device`, applying every saved
/// setting from `cfg` -- the full connect sequence (RadioSession,
/// SpectrumHandle, AudioOutput, rigctl/TCI, extra receivers, TX chain).
/// Only called from the initial discovery -> connect transition now --
/// PureSignal's enable/disable (formerly the one setting that needed a
/// full reconnect to apply, since it changed the wire-level receiver/DDC
/// layout `RadioSession::start` negotiates once at connect time) is a
/// true live toggle on both protocols now, same as Diversity already
/// was -- see `RadioSession::puresignal_enabled`'s doc comment (radio.rs).
fn connect_to_device(device: Device, cfg: &Config) -> Result<ConnectedState, String> {
    let mut settings = RadioSettings::default();
    // ROOT CAUSE FIX: this used to stay at RadioSettings::default()'s
    // hardcoded 7.1MHz here, with the real saved frequency only applied
    // afterward via a separate session.set_frequency(cfg.frequency_hz)
    // call once RadioSession::start returned. That left a real gap: P1's
    // initial preconfig burst went out at 7.1MHz regardless of what was
    // actually saved (briefly mistuning real hardware before the sender
    // loop's next iteration corrected it), and -- the concrete bug this
    // was confirmed to cause -- RadioSession::start also seeds
    // tx_frequency_hz/rx_frequency_hz/requested_frequency_hz from this
    // same (still-7.1MHz) settings.frequency_hz. requested_frequency_hz
    // in particular is never corrected afterward the way frequency_hz
    // is, so main.rs's own per-frame reconciliation (see
    // RadioSession::requested_frequency_hz's doc comment) saw a stale
    // 7.1MHz "request" on the very first frame after every connect and
    // resolved it through resolve_tune -- while CTUN was on, clamping
    // ctun_frequency_hz down to the bottom edge of the current passband
    // window instead of leaving the just-restored CTUN frequency alone
    // (confirmed by a real report: CTUN frequency reset to "the lowest
    // frequency" on every restart). Setting it here instead means every
    // frequency-tracking field RadioSession::start creates already
    // starts correct, with nothing left to reconcile away.
    if let Some(f) = cfg.frequency_hz {
        settings.frequency_hz = f;
    }
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
    settings.diversity_enabled = cfg.diversity_enabled.unwrap_or(false);
    settings.diversity_gain_db = cfg.diversity_gain_db.unwrap_or(0.0);
    settings.diversity_phase_deg = cfg.diversity_phase_deg.unwrap_or(0.0);
    if let Some(atten) = cfg.rx_attenuation {
        settings.rx_attenuation = atten;
    }
    if let Some(atten) = cfg.ps_tx_attenuation {
        settings.ps_tx_attenuation = atten;
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
            session.send_rx_audio_to_radio.store(
                cfg.send_rx_audio_to_radio.unwrap_or(false),
                Ordering::Relaxed,
            );
            session
                .tx_audio_source
                .store(cfg.tx_audio_source.unwrap_or(TX_AUDIO_SOURCE_AUTO), Ordering::Relaxed);
            session
                .mic_ptt_enabled
                .store(cfg.mic_ptt_enabled.unwrap_or(false), Ordering::Relaxed);
            session
                .mic_bias_enabled
                .store(cfg.mic_bias_enabled.unwrap_or(false), Ordering::Relaxed);
            session.mic_ptt_on_tip.store(cfg.mic_ptt_on_tip.unwrap_or(false), Ordering::Relaxed);
            let spectrum = SpectrumHandle::start(
                0,
                Arc::clone(&session.iq_buffers[0]),
                settings.sample_rate as i32,
                Some(Arc::clone(&session.rx_audio_to_radio)),
                Arc::clone(&session.mox),
            );
            let audio_output_device = cfg.audio_output_device.clone();
            let audio_output =
                match AudioOutput::start(Arc::clone(&spectrum.audio_out), audio_output_device.as_deref()) {
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
            let cat_addr = cfg.cat_addr.clone().unwrap_or_else(|| cat::DEFAULT_ADDR.to_string());
            // Debug logging (Settings -> Network) -- see debug_log.rs's
            // own doc comment. Constructed once per connection (not per
            // Start/Stop of the server itself) so toggling the checkbox
            // takes effect immediately without needing to restart
            // rigctl/TCI/CAT, and so the SAME instance can be handed to
            // whichever RigctlServer/TciServer/CatServer gets started
            // below or later from Settings -> Network.
            let rigctl_debug_log = debug_log::DebugLog::new(
                debug_log::log_path("rigctl_log.txt").unwrap_or_else(|| "rigctl_log.txt".into()),
            );
            rigctl_debug_log.set_enabled(cfg.rigctl_logging_enabled.unwrap_or(false));
            let tci_debug_log = debug_log::DebugLog::new(
                debug_log::log_path("tci_log.txt").unwrap_or_else(|| "tci_log.txt".into()),
            );
            tci_debug_log.set_enabled(cfg.tci_logging_enabled.unwrap_or(false));
            let cat_debug_log = debug_log::DebugLog::new(
                debug_log::log_path("cat_log.txt").unwrap_or_else(|| "cat_log.txt".into()),
            );
            cat_debug_log.set_enabled(cfg.cat_logging_enabled.unwrap_or(false));

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
                    Arc::clone(&session.requested_frequency_hz),
                    Arc::clone(&session.rx_frequency_hz),
                    spectrum.demod_params_handle(),
                    Arc::clone(&spectrum.display),
                    Arc::clone(&session.mox),
                    rigctl_debug_log.clone(),
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
                    Arc::clone(&session.requested_frequency_hz),
                    Arc::clone(&session.rx_frequency_hz),
                    Arc::clone(&session.sample_rate),
                    spectrum.demod_params_handle(),
                    Arc::clone(&session.mox),
                    Arc::clone(&spectrum.tci_audio_out),
                    Arc::clone(&spectrum.iq_out),
                    Arc::clone(&session.tci_tx_audio),
                    Arc::clone(&session.tci_tx_gain),
                    Arc::clone(&session.tci_wants_mic),
                    format!("{:?}", device.board),
                    tci_debug_log.clone(),
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
            let mut cat_server: Option<CatServer> = None;
            let mut cat_error: Option<String> = None;
            if cfg.cat_running.unwrap_or(false) {
                cat_server = match CatServer::start(
                    &cat_addr,
                    Arc::clone(&session.requested_frequency_hz),
                    Arc::clone(&session.rx_frequency_hz),
                    spectrum.demod_params_handle(),
                    Arc::clone(&spectrum.display),
                    Arc::clone(&session.mox),
                    cat_debug_log.clone(),
                ) {
                    Ok(s) => Some(s),
                    Err(e) => {
                        let msg = format!("couldn't listen on {cat_addr}: {e}");
                        eprintln!("cat: {msg}");
                        cat_error = Some(msg);
                        None
                    }
                };
            }
            // cfg.frequency_hz is now applied earlier, via
            // settings.frequency_hz before RadioSession::start -- see
            // that assignment's doc comment for why.
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
            if let Some(v) = cfg.rx_eq {
                spectrum.set_eq(v);
            }
            let mic_gain = cfg.mic_gain.unwrap_or(0.5);
            let tci_tx_gain = cfg.tci_tx_gain.unwrap_or(1.0);
            *session.tci_tx_gain.lock().unwrap() = tci_tx_gain;
            // See tx::PsParams::default for these same
            // fallback values -- kept in sync deliberately
            // (both are "reference default if never
            // calibrated", just one's Config's fallback,
            // one's PsParams's fallback for a session that
            // skips Config loading entirely).
            let ps_enabled = true;
            let ps_hw_peak = cfg.ps_hw_peak.unwrap_or_else(|| default_ps_hw_peak(device.protocol));
            let ps_mox_delay = cfg.ps_mox_delay.unwrap_or(0.2);
            let ps_loop_delay = cfg.ps_loop_delay.unwrap_or(0.0);
            let ps_tx_delay_ns = cfg.ps_tx_delay_ns.unwrap_or(150.0);
            let ps_ptol = cfg.ps_ptol.unwrap_or(0.8);

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
            // Fed by TxHandle with the actual generated TX
            // IQ (not RX ADC samples) -- see
            // ConnectedState::tx_spectrum's doc comment.
            let tx_spectrum_iq: Arc<Mutex<VecDeque<IqSample>>> =
                Arc::new(Mutex::new(VecDeque::new()));
            let tx_spectrum = SpectrumHandle::start(
                session.iq_buffers.len() as i32 + 1,
                Arc::clone(&tx_spectrum_iq),
                duc_rate,
                None,
                Arc::clone(&session.mox),
            );
            let mic_buffer = Arc::new(Mutex::new(VecDeque::new()));
            let mic_input_device = cfg.mic_input_device.clone();
            let (tx_enabled, mic_input, tx_handle) =
                match MicInput::start(Arc::clone(&mic_buffer), mic_input_device.as_deref()) {
                Ok(mic) => {
                    let tx_handle = TxHandle::start(
                        mic_buffer,
                        Arc::clone(&session.tci_tx_audio),
                        Arc::clone(&session.radio_mic_audio),
                        Arc::clone(&session.tx_audio_source),
                        Arc::clone(&session.tci_wants_mic),
                        Arc::clone(&session.tx_iq),
                        Arc::clone(&tx_spectrum_iq),
                        Arc::clone(&session.mox),
                        session.iq_buffers.len() as i32,
                        device.protocol,
                        48_000,
                        duc_rate,
                        settings.puresignal_enabled,
                        Arc::clone(&session.ps_rx_feedback_iq),
                        Arc::clone(&session.ps_tx_feedback_iq),
                        ps_corr_path(device.mac),
                    );
                    tx_handle.set_mic_gain(mic_gain);
                    tx_handle.set_mode(spectrum.mode());
                    tx_handle.set_width_hz(spectrum.width_hz());
                    tx_handle.set_ps_enabled(ps_enabled);
                    tx_handle.set_ps_hw_peak(ps_hw_peak);
                    tx_handle.set_ps_mox_delay(ps_mox_delay);
                    tx_handle.set_ps_loop_delay(ps_loop_delay);
                    tx_handle.set_ps_tx_delay_ns(ps_tx_delay_ns);
                    tx_handle.set_ps_ptol(ps_ptol);
                    if let Some(v) = cfg.tx_eq {
                        tx_handle.set_eq(v);
                    }
                    // Apply a previously-saved correction table
                    // immediately, if PS is enabled and one exists for
                    // this radio -- see TxHandle::restore_ps_corr's doc
                    // comment. Correcting can be true right away, no
                    // Two-Tone needed every session.
                    if settings.puresignal_enabled {
                        if let Some(path) = ps_corr_path(device.mac) {
                            if path.exists() {
                                tx_handle.restore_ps_corr();
                            }
                        }
                    }
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
            // Restore CTUN (see ConnectedState::ctun's doc comment) --
            // ctun_frequency_hz is only meaningful/saved-and-restored
            // while ctun was actually on; otherwise fall back to the
            // dial frequency, matching a fresh/never-used CTUN's own
            // "off" state.
            let ctun = cfg.ctun.unwrap_or(false);
            let ctun_frequency_hz =
                if ctun { cfg.ctun_frequency_hz.unwrap_or(initial_frequency_hz) } else { initial_frequency_hz };
            // VFO B / Split -- see ConnectedState's own doc comments.
            // VFO B falls back to A's frequency (matches a real rig's
            // typical power-on state, and this project's own convention
            // of never leaving a frequency field at a meaningless 0).
            let vfo_b_frequency_hz = cfg.vfo_b_frequency_hz.unwrap_or(initial_frequency_hz);
            let split = cfg.split.unwrap_or(false);
            // RIT / XIT -- see ConnectedState's own doc comments.
            let rit_enabled = cfg.rit_enabled.unwrap_or(false);
            let rit_offset_hz = cfg.rit_offset_hz.unwrap_or(0.0);
            let xit_enabled = cfg.xit_enabled.unwrap_or(false);
            let xit_offset_hz = cfg.xit_offset_hz.unwrap_or(0.0);
            Ok(ConnectedState {
                device,
                session,
                spectrum,
                tx_spectrum,
                audio_output,
                audio_output_device,
                tx_audio_monitor_output: None,
                rigctl_server,
                tci_server,
                cat_server,
                waterfall_texture: None,
                waterfall_signature: None,
                scroll_accum: 0.0,
                zoom_accum: 0.0,
                drag_tune_accum_hz: 0.0,
                sample_rate: settings.sample_rate,
                db_low: cfg.db_low.unwrap_or(-140.0),
                db_low_auto: cfg.db_low_auto.unwrap_or(true),
                db_low_auto_smoothed: None,
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
                spectrum_zoom: cfg.spectrum_zoom.unwrap_or(1),
                spectrum_pan: cfg.spectrum_pan.unwrap_or(0.0),
                slider_scroll_accum: 0.0,
                show_settings_window: false,
                settings_tab: SettingsTab::Agc,
                firmware_update: None,
                extra_receivers,
                settings_dirty,
                band_memory: cfg.band_settings.clone(),
                width_memory: cfg.width_memory.clone(),
                ctun,
                ctun_frequency_hz,
                last_requested_frequency_hz: initial_frequency_hz,
                vfo_b_frequency_hz,
                vfo_b_scroll_accum: 0.0,
                split,
                rit_enabled,
                rit_offset_hz,
                rit_scroll_accum: 0.0,
                xit_enabled,
                xit_offset_hz,
                xit_scroll_accum: 0.0,
                rigctl_addr,
                tci_addr,
                cat_addr,
                rigctl_debug_log,
                tci_debug_log,
                cat_debug_log,
                rigctl_error,
                tci_error,
                cat_error,
                tx_enabled,
                mic_input,
                mic_input_device,
                tx_handle,
                ptt_held: false,
                mic_gain,
                tci_tx_gain,
                ps_enabled,
                ps_oneshot: false,
                ps_hw_peak,
                ps_mox_delay,
                ps_loop_delay,
                ps_tx_delay_ns,
                ps_ptol,
                pa_calibration: cfg.pa_calibration.clone(),
                max_tx_power_watts: cfg
                    .max_tx_power_watts
                    .unwrap_or_else(|| default_max_tx_power_watts(device.board)),
                tune_power_percent: cfg.tune_power_percent.unwrap_or(20),
                tune_active: false,
                pre_tune_power_watts: None,
                two_tone_active: false,
                smoothed_fwd_power: 0.0,
                smoothed_rev_power: 0.0,
                tx_fifo_warning_until: None,
                tx_spectrum_mox_was_active: false,
                puresignal_enabled: settings.puresignal_enabled,
                diversity_enabled: settings.diversity_enabled,
                eq_tab_is_tx: false,
                ps_was_correcting: false,
                status_message: None,
            })
        }
        Err(e) => Err(format!("Failed to start radio: {e}")),
    }
}

impl eframe::App for HpsdrApp {
    // eframe 0.35 replaced `update(&Context)` with `ui(&mut Ui)` -- see
    // https://github.com/emilk/egui/blob/main/CHANGELOG.md (0.35.0).
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Track this window's current position/size every frame (cheap --
        // just copying floats already computed by egui-winit) so a final
        // value is always ready whenever the periodic per-radio Config
        // save below actually fires, rather than trying to read it fresh
        // only in that one frame. outer_rect (position, includes window-
        // manager chrome) pairs with ViewportBuilder::with_position/
        // ViewportCommand::OuterPosition; inner_rect (content size, no
        // chrome) pairs with with_inner_size/InnerSize -- see
        // Config::window_geometry and the DiscoveryAction::Start handler
        // below. Both are None on Wayland, but this app already forces
        // X11 (see main()'s WAYLAND_DISPLAY workaround), so that doesn't
        // apply here.
        if let (Some(outer), Some(inner)) =
            (ui.input(|i| i.viewport().outer_rect), ui.input(|i| i.viewport().inner_rect))
        {
            self.main_window_geometry = Some(WindowGeometry {
                x: outer.min.x,
                y: outer.min.y,
                width: inner.width(),
                height: inner.height(),
            });
        }
        let root_close_requested = ui.input(|i| i.viewport().close_requested());

        // BUG FIX: clicking the main window while it's not the active/
        // focused OS window would both raise/focus it AND process that
        // same click as normal input to whatever widget happened to be
        // under the cursor -- e.g. accidentally retuning by clicking
        // the spectrum/waterfall just to bring the app to the front,
        // confirmed via a real report. The OS/window manager already
        // focuses the window on click on its own -- that part isn't
        // something egui/eframe controls or needs to help with. What IS
        // controllable is whether THIS frame's widgets treat that same
        // click as a deliberate interaction.
        //
        // BUG FIX (round 2): a plain single-frame "focused && !was
        // focused" check (comparing only to the immediately preceding
        // frame) looked right but a real test disproved it -- clicking
        // the spectrum/waterfall to refocus the window still retuned
        // the radio. Root cause: while genuinely unfocused, most
        // window managers/compositors throttle or skip repaints
        // entirely regardless of this app's own request_repaint_after
        // calls, so there may be NO intervening frame with
        // focused=false to compare against -- the first frame that
        // runs again can already show focused=true with `was_focused`
        // still stuck at whatever it was before the gap. On top of
        // that, the OS's WindowFocused event and the click's own
        // PointerButton event aren't guaranteed to land in the exact
        // same frame either. Fixed by combining two independent
        // signals -- the frame-to-frame transition (works whenever the
        // app IS still repainting through it) and a raw
        // Event::WindowFocused(true) appearing anywhere in this
        // frame's input queue (works even after a repaint gap, since
        // it's a discrete per-frame event log entry, not a state
        // comparison this project has to have observed changing).
        // unwrap_or(true) errors toward NOT suppressing when focus state
        // is unknown (some platforms/WMs don't always report it), since
        // a false positive here blocks a real click rather than just
        // failing to catch a spurious one.
        //
        // BUG FIX (round 3): this used to call ui.disable() on the whole
        // window for the real-time window below, which also blocked
        // ordinary controls (e.g. the TX row's TWO TONE button) that
        // happen to sit in this same root Ui -- a real report: with the
        // Settings window open and focused for PureSignal calibration,
        // clicking TWO TONE in the (unfocused) main window to both
        // refocus it and fire the button just silently refocused it
        // instead, every time. The actual reported bug this was fixed
        // for was specifically about the spectrum/waterfall retuning
        // itself on a refocus click, not about buttons in general, so
        // this no longer disables anything globally -- `suppress_refocus_click`
        // is instead checked directly at the two click-to-tune sites
        // (spectrum and waterfall) further down, leaving every other
        // control free to respond to the very click that refocuses the
        // window, same as any normal application.
        let focused = ui.input(|i| i.viewport().focused).unwrap_or(true);
        let focus_event_this_frame = ui.input(|i| {
            i.events
                .iter()
                .any(|e| matches!(e, egui::Event::WindowFocused(true)))
        });
        if (focused && !self.was_focused) || focus_event_this_frame {
            self.ignore_interaction_until = Some(Instant::now() + Duration::from_millis(200));
            // Guarantees a follow-up frame runs to clear this promptly
            // once the window elapses, even if nothing else happens to
            // trigger a repaint in the meantime (the Connected view's
            // own request_repaint_after(33ms) calls normally cover this,
            // but this doesn't fire from every AppState).
            ui.ctx().request_repaint_after(Duration::from_millis(200));
        }
        self.was_focused = focused;
        let suppress_refocus_click = self
            .ignore_interaction_until
            .is_some_and(|deadline| Instant::now() < deadline);
        if !suppress_refocus_click {
            self.ignore_interaction_until = None;
        }

        match &mut self.state {
            AppState::Discovering(window) => match window.show(ui) {
                DiscoveryAction::Start(device) => {
                    let cfg = Config::load(device.mac);
                    // Move/resize the main window to wherever it was
                    // last left for THIS radio -- see
                    // Config::window_geometry's doc comment for why this
                    // is an explicit command rather than a
                    // ViewportBuilder hint (the window already exists).
                    // Sent once, here, not every frame -- unlike a
                    // ViewportBuilder field, a ViewportCommand actually
                    // re-applies every time it's sent, which would fight
                    // the user moving/resizing the window themselves.
                    if let Some(g) = cfg.window_geometry {
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::OuterPosition(
                            egui::pos2(g.x, g.y),
                        ));
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(
                            g.width, g.height,
                        )));
                    }
                    match connect_to_device(device, &cfg) {
                        Ok(connected) => self.state = AppState::Connected(connected),
                        Err(e) => self.state = AppState::Error(e),
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
                // Also reused (with " - RX N" appended) as each extra
                // receiver window's title further down, so both windows
                // are identifiable by board/protocol/IP at a glance.
                let base_title = format!(
                    "hpsdr-rs -- {:?} (P{} v{}.{}) at {}",
                    connected.device.board,
                    connected.device.protocol,
                    connected.device.version / 10,
                    connected.device.version % 10,
                    connected.device.address.ip()
                );
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Title(base_title.clone()));
                // None = not running, Some(false) = listening/idle,
                // Some(true) = a client is currently connected. Drives
                // the gray/green/red status text in the main panel.
                let rigctl_status: Option<bool> = connected.rigctl_server.as_ref().map(|s| s.is_connected());
                let tci_status: Option<bool> = connected.tci_server.as_ref().map(|s| s.is_connected());
                let cat_status: Option<bool> = connected.cat_server.as_ref().map(|s| s.is_connected());
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

                // See RadioSession::requested_frequency_hz's doc comment.
                // A network client (rigctl/CAT/TCI) requesting a new
                // frequency lands here, not directly on the hardware --
                // reconcile it exactly like any other frequency change
                // that needs to respect CTUN (same resolve_tune call the
                // scroll-tune/VFO-B-button handlers use).
                let requested_freq_hz = connected
                    .session
                    .requested_frequency_hz
                    .load(std::sync::atomic::Ordering::Relaxed);
                if requested_freq_hz != connected.last_requested_frequency_hz {
                    let (effective_freq, retune) =
                        resolve_tune(connected.ctun, freq_hz, sample_rate, passband, requested_freq_hz);
                    if let Some(lo) = retune {
                        connected.session.set_frequency(lo);
                    } else {
                        connected.ctun_frequency_hz = effective_freq;
                    }
                    connected.last_requested_frequency_hz = requested_freq_hz;
                    // settings_changed isn't declared yet at this point in
                    // the frame -- settings_dirty is the established
                    // mechanism for marking a save needed from outside its
                    // scope (see e.g. the rigctl/TCI/CAT Start buttons'
                    // own use of it).
                    connected.settings_dirty.store(true, std::sync::atomic::Ordering::Relaxed);
                }

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
                // RIT ("Receiver Incremental Tuning"): summed into the
                // same WDSP RXA shift CTUN uses (WDSP has one shift
                // register -- see ConnectedState::rit_enabled's doc
                // comment) but deliberately NOT folded into
                // ctun_offset_hz itself, which drives the visible dial
                // line/passband overlay/zoom centering below -- RIT is
                // meant to nudge only what's actually demodulated, never
                // the displayed/logged frequency, matching standard rig
                // convention.
                let rit_offset_hz = if connected.rit_enabled { connected.rit_offset_hz } else { 0.0 };
                connected
                    .spectrum
                    .set_ctun(connected.ctun || connected.rit_enabled, ctun_offset_hz + rit_offset_hz);
                // Zoom should keep the CTUN'd listen frequency (where the
                // filter/passband actually is) centered, not the parked
                // hardware LO -- otherwise the filter drifts toward one
                // edge of the zoomed view instead of staying put. WDSP's
                // own fscLin/fscHin clipping (inside set_zoom_pan) only
                // ever sees whatever pan value we hand it here, so the
                // CTUN adjustment has to happen before this call, not just
                // in the axis-label math further down.
                let rx_half_span_hz = sample_rate as f64 / 2.0;
                let rx_max_pan_hz = rx_half_span_hz - rx_half_span_hz / connected.spectrum_zoom as f64;
                let rx_pan_offset_hz = (ctun_offset_hz + connected.spectrum_pan as f64 * rx_max_pan_hz)
                    .clamp(-rx_max_pan_hz, rx_max_pan_hz);
                let rx_effective_pan =
                    if rx_max_pan_hz > 0.0 { (rx_pan_offset_hz / rx_max_pan_hz) as f32 } else { 0.0 };
                connected.spectrum.set_zoom_pan(connected.spectrum_zoom, rx_effective_pan);
                // TX has no CTUN concept -- tx_spectrum's own generated IQ
                // is always centered on the real TX carrier (see the "force
                // ctun_offset_hz to 0 while transmitting" passband-overlay
                // logic below) -- so it zooms around the plain slider Pan.
                connected.tx_spectrum.set_zoom_pan(connected.spectrum_zoom, connected.spectrum_pan);
                let dial_freq_hz = if connected.ctun { connected.ctun_frequency_hz } else { freq_hz };
                // See RadioSession::rx_frequency_hz's doc comment -- kept
                // in sync every frame here so rigctl/TCI/CAT report the
                // CTUN'd listen frequency, not the parked hardware LO.
                connected.session.rx_frequency_hz.store(dial_freq_hz, std::sync::atomic::Ordering::Relaxed);
                // See RadioSession::tx_frequency_hz's doc comment --
                // kept in sync every frame here (same "cheap, no call
                // site can forget it" reasoning as ctun_offset_hz just
                // above) so PTT transmits on the right frequency
                // regardless of which of CTUN/Split (if either) is
                // active. Split takes priority over CTUN when both are
                // somehow on at once -- see ConnectedState::split's doc
                // comment for why that's the correct precedence.
                let tx_dial_freq_hz =
                    if connected.split { connected.vfo_b_frequency_hz } else { dial_freq_hz };
                // XIT ("Transmitter Incremental Tuning"): nudges the real
                // TX NCO frequency on top of whichever of Split/dial is
                // already selected above -- see ConnectedState::
                // xit_enabled's doc comment for why this can't be a
                // WDSP-side shift the way RIT is (no TXA shift primitive
                // exists in this project's WDSP bindings).
                let xit_offset_hz = if connected.xit_enabled { connected.xit_offset_hz } else { 0.0 };
                let tx_dial_freq_hz = (tx_dial_freq_hz as i64 + xit_offset_hz.round() as i64).max(0) as u32;
                connected.session.tx_frequency_hz.store(tx_dial_freq_hz, std::sync::atomic::Ordering::Relaxed);

                // While transmitting, show tx_spectrum (fed with the
                // actual generated TX IQ -- see ConnectedState::tx_spectrum's
                // doc comment) instead of the RX analyzer: the RX buffer
                // only ever shows whatever the receiver happens to pick up
                // over the air, which is not a reliable "is my transmitted
                // signal clean" signal at all. No LO/CTUN translation
                // applied to tx_spectrum -- it's raw generated baseband,
                // not a wideband capture that needs retuning within.
                let transmitting = connected.session.mox_active();
                if transmitting && !connected.tx_spectrum_mox_was_active {
                    // Fresh PTT -- see SpectrumHandle::clear_display's
                    // doc comment for why this can't just be left to
                    // scroll/blend away naturally.
                    connected.tx_spectrum.clear_display();
                }
                connected.tx_spectrum_mox_was_active = transmitting;

                // ROOT CAUSE FIX for a real, persistent report of a wide
                // spectral "skirt" appearing on ANY mic-chain TX audio
                // (WSJT-X/TCI, local USB mic -- confirmed NOT specific to
                // either) but never on Tune: every frequency-axis and
                // click-to-tune calculation below this point assumes
                // `sample_rate` is the true span of whichever analyzer's
                // data is currently on screen. That's correct for RX
                // (spectrum's own analyzer is opened at exactly this same
                // `connected.sample_rate`), but while transmitting, the
                // data actually being shown is tx_spectrum's -- and that
                // analyzer is always opened at duc_rate (192kHz for P2,
                // confirmed via main.rs's own duc_rate formula elsewhere),
                // completely independent of whatever RX sample rate the
                // user has selected. Left unshadowed, the axis kept
                // assuming the RX span while displaying TX data -- e.g. a
                // real 384kHz RX rate would visually "stretch" a tx_spectrum
                // signal's true, narrow analyzer bandwidth across a much
                // wider apparent range. A direct capture of the raw
                // generated TX IQ (before it ever reaches this display)
                // confirmed the actual signal is clean -- down at the
                // FFT noise floor beyond roughly 20kHz of its passband --
                // proving this was never a real TX-quality issue. This
                // also explains why Tune never showed it: a single-bin
                // tone still looks like a narrow line under a wrong axis
                // scale, but real multi-Hz-wide voice/digital-mode audio
                // visibly spreads when relabeled onto the wrong (usually
                // much wider) span.
                let sample_rate = if transmitting {
                    if connected.device.protocol == 2 { 192_000 } else { sample_rate }
                } else {
                    sample_rate
                };

                // Zoom/Pan (sliders below the waterfall): narrows the
                // visible spectrum/waterfall window as zoom increases,
                // then shifts it within the full captured span by
                // pan_offset_hz. Computed here (not just where the
                // spectrum/waterfall are drawn) so freq_at_x -- used by
                // the click-to-tune handlers below, which run before the
                // drawing code -- can also account for it. max_pan_hz is
                // 0 at zoom 1.0, so Pan has no effect then regardless of
                // the slider -- there's nothing to pan to when the full
                // span is already shown.
                let half_span_hz = sample_rate as f64 / 2.0;
                let visible_half_span_hz = half_span_hz / connected.spectrum_zoom as f64;
                let max_pan_hz = half_span_hz - visible_half_span_hz;
                // Same CTUN-centering as the RX WDSP reconfigure above, so
                // the axis ticks/overlays match what WDSP actually returns.
                // TX has no CTUN concept (see the "force ctun_offset_hz to
                // 0 while transmitting" logic just below), so this reduces
                // to the plain slider pan while transmitting.
                let zoom_ctun_offset_hz = if transmitting { 0.0 } else { ctun_offset_hz };
                let pan_offset_hz = (zoom_ctun_offset_hz + connected.spectrum_pan as f64 * max_pan_hz)
                    .clamp(-max_pan_hz, max_pan_hz);

                let (spectrum_row, meter_db, waterfall_data_revision) = {
                    let d = if transmitting {
                        connected.tx_spectrum.display.lock().unwrap()
                    } else {
                        connected.spectrum.display.lock().unwrap()
                    };
                    (d.spectrum.clone(), d.meter_db, d.revision)
                };

                // "Auto" Low (Settings -> Spectrum) -- see
                // ConnectedState::db_low_auto's doc comment. RX only:
                // spectrum_row is tx_spectrum's data while transmitting,
                // which isn't a "find the noise floor" scenario (see the
                // TX range's own doc comment just below).
                if connected.db_low_auto && !transmitting {
                    let n = spectrum_row.len();
                    let edge = (n / AUTO_DB_LOW_EDGE_EXCLUDE_FRACTION).max(AUTO_DB_LOW_MIN_EDGE_EXCLUDE);
                    if n > edge * 2 {
                        let raw_min = spectrum_row[edge..n - edge].iter().copied().fold(f32::INFINITY, f32::min);
                        if raw_min.is_finite() {
                            let prev = connected.db_low_auto_smoothed.unwrap_or(raw_min);
                            let smoothed = prev + AUTO_DB_LOW_SMOOTHING_ALPHA * (raw_min - prev);
                            connected.db_low_auto_smoothed = Some(smoothed);
                            connected.db_low = smoothed.clamp(-180.0, connected.db_high - 1.0);
                        }
                    }
                }

                // Fixed dB bounds (user-set unless db_low_auto above just
                // overrode the low end, not otherwise auto-scaled) so the
                // trace and gridlines stay put rather than shifting as
                // power levels change. Separate ranges for RX and TX
                // (Settings -> Display) -- a locally-picked-up TX
                // signal is typically far stronger than the weak RX
                // signals the RX range is normally tuned for, so they
                // need independent headroom rather than sharing one
                // range or a fixed offset applied at render time.
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
                        let d = if transmitting {
                            connected.tx_spectrum.display.lock().unwrap()
                        } else {
                            connected.spectrum.display.lock().unwrap()
                        };
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
                // Set from deep inside the Settings window's nested
                // closure (same capture-a-local-flag pattern as
                // close_requested/settings_changed) once an in-app
                // firmware update finishes successfully -- handled at the
                // end of this match arm, alongside stop_clicked, since
                // reassigning self.state can't happen while `connected`
                // (borrowed from it) is still needed by code below.
                let mut restart_after_firmware_update: Option<Device> = None;
                egui::CentralPanel::default().show(ui, |ui| {
                    ui.add_space(4.0);
                    // Red while transmitting -- a clear, glanceable
                    // "you're on the air" signal right where the eye
                    // already goes to read the frequency, not just the
                    // separate TRANSMITTING label elsewhere in the row.
                    // While Split is on, TX actually goes out on VFO B
                    // (see ConnectedState::split's doc comment), so the
                    // red highlight follows VFO B instead of VFO A --
                    // otherwise it would point at the wrong box.
                    let freq_a_color = if transmitting && !connected.split {
                        egui::Color32::RED
                    } else {
                        egui::Color32::GREEN
                    };
                    let freq_b_color = if transmitting && connected.split {
                        egui::Color32::RED
                    } else {
                        egui::Color32::GRAY
                    };
                    let (freq_label, vfo_b_label) = ui
                        .horizontal(|ui| {
                            // VFO A, boxed and labeled to match VFO B's own
                            // box below -- see ConnectedState::
                            // vfo_b_frequency_hz/split's doc comments for
                            // what the buttons between the two boxes do.
                            let freq_label = ui
                                .group(|ui| {
                                    ui.vertical(|ui| {
                                        ui.label("VFO-A");
                                        ui.add(
                                            egui::Label::new(
                                                egui::RichText::new(format_frequency(dial_freq_hz))
                                                    .monospace()
                                                    .size(28.0)
                                                    .strong()
                                                    .color(freq_a_color),
                                            )
                                            .sense(egui::Sense::hover()),
                                        )
                                        .on_hover_text(
                                            "Scroll to tune -- Shift: 100 Hz, Ctrl: 10 kHz, none: 1 kHz",
                                        )
                                    })
                                    .inner
                                })
                                .inner;

                            ui.vertical(|ui| {
                                ui.horizontal(|ui| {
                                    if ui
                                        .button("A>B")
                                        .on_hover_text("Copy VFO A's frequency to VFO B")
                                        .clicked()
                                    {
                                        connected.vfo_b_frequency_hz = dial_freq_hz;
                                        settings_changed = true;
                                    }
                                    if ui
                                        .button("B>A")
                                        .on_hover_text("Retune VFO A to VFO B's frequency")
                                        .clicked()
                                    {
                                        // While CTUN is on, "A" is the
                                        // CTUN'd listen frequency, not the
                                        // parked hardware LO -- move that
                                        // (clamped to stay within the
                                        // current passband, same as
                                        // scroll-tuning) rather than
                                        // retuning the real hardware. See
                                        // resolve_tune's doc comment.
                                        let (effective_freq, retune) = resolve_tune(
                                            connected.ctun,
                                            freq_hz,
                                            sample_rate,
                                            passband,
                                            connected.vfo_b_frequency_hz,
                                        );
                                        if let Some(lo) = retune {
                                            connected.session.set_frequency(lo);
                                        } else {
                                            connected.ctun_frequency_hz = effective_freq;
                                        }
                                        settings_changed = true;
                                    }
                                });
                                ui.horizontal(|ui| {
                                    if ui
                                        .button("A<>B")
                                        .on_hover_text("Swap VFO A and VFO B")
                                        .clicked()
                                    {
                                        // Same CTUN-aware handling as B>A
                                        // above.
                                        let new_b = dial_freq_hz;
                                        let (effective_freq, retune) = resolve_tune(
                                            connected.ctun,
                                            freq_hz,
                                            sample_rate,
                                            passband,
                                            connected.vfo_b_frequency_hz,
                                        );
                                        if let Some(lo) = retune {
                                            connected.session.set_frequency(lo);
                                        } else {
                                            connected.ctun_frequency_hz = effective_freq;
                                        }
                                        connected.vfo_b_frequency_hz = new_b;
                                        settings_changed = true;
                                    }
                                    if ui
                                        .add(egui::Button::selectable(connected.split, "Split"))
                                        .on_hover_text(
                                            "Transmit on VFO B while continuing to receive on VFO A",
                                        )
                                        .clicked()
                                    {
                                        connected.split = !connected.split;
                                        settings_changed = true;
                                    }
                                });
                            });

                            let vfo_b_label = ui
                                .group(|ui| {
                                    ui.vertical(|ui| {
                                        ui.label("VFO-B");
                                        ui.add(
                                            egui::Label::new(
                                                egui::RichText::new(format_frequency(
                                                    connected.vfo_b_frequency_hz,
                                                ))
                                                .monospace()
                                                .size(28.0)
                                                .strong()
                                                .color(freq_b_color),
                                            )
                                            .sense(egui::Sense::hover()),
                                        )
                                        .on_hover_text(
                                            "Scroll to tune -- Shift: 100 Hz, none: 1 kHz",
                                        )
                                    })
                                    .inner
                                })
                                .inner;

                            (freq_label, vfo_b_label)
                        })
                        .inner;

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
                        // ROOT CAUSE FIX: max raised from 1.5 -- a real
                        // report needed more than that even with the
                        // system output already at 100%/0dB (pavucontrol).
                        // WDSP's RXA output level is apparently on the
                        // conservative side for this radio/setup, and
                        // this is a plain linear multiply against that
                        // sample before the -1.0..1.0 clamp (see
                        // spectrum.rs's run()), so there's no correctness
                        // reason to cap it as low as 1.5 -- just headroom.
                        // Displayed/dragged in dB (see scroll_slider_f32_db's
                        // doc comment) -- +18dB ceiling matches the old
                        // 8.0 linear max; -100dB floor is effectively
                        // silent (0.00001 linear) while still being a
                        // finite, draggable slider position.
                        if scroll_slider_f32_db(
                            ui,
                            &mut connected.slider_scroll_accum,
                            &mut gain,
                            -100.0,
                            18.0,
                            1.0,
                        ) {
                            connected.spectrum.set_gain(gain);
                            settings_changed = true;
                        }

                        if connected.tx_enabled {
                            if connected.tx_handle.is_some() {
                                ui.add_space(12.0);
                                ui.label("Mic gain:");
                                let mut mic_gain = connected.mic_gain;
                                // Displayed/dragged in dB (see
                                // scroll_slider_f32_db's doc comment) --
                                // +6dB ceiling matches the old 2.0 linear
                                // max, -60dB floor matches Audio gain's own.
                                if scroll_slider_f32_db(
                                    ui,
                                    &mut connected.slider_scroll_accum,
                                    &mut mic_gain,
                                    -60.0,
                                    6.0,
                                    1.0,
                                ) {
                                    connected.mic_gain = mic_gain;
                                    if let Some(tx) = &connected.tx_handle {
                                        tx.set_mic_gain(mic_gain);
                                    }
                                    settings_changed = true;
                                }

                                // Separate from Mic gain above -- a real
                                // test against WSJT-X found its TCI TX
                                // audio arriving at roughly 1/700th the
                                // amplitude Mic gain's 0.0..=2.0 range is
                                // calibrated for (confirmed via WSJT-X's
                                // own source, not an hpsdr-rs decode bug
                                // -- see radio::RadioSession::
                                // tci_tx_gain's doc comment). Displayed/
                                // dragged in dB for the same reason Audio
                                // Gain needed it: this needs to cover a
                                // couple orders of magnitude, dialed in by
                                // ear/meter against real traffic -- +60dB
                                // ceiling matches the old 1000.0 linear
                                // max exactly, -60dB floor matches Audio
                                // gain's own.
                                ui.add_space(12.0);
                                ui.label("TCI TX gain:");
                                let mut tci_tx_gain = connected.tci_tx_gain;
                                if scroll_slider_f32_db(
                                    ui,
                                    &mut connected.slider_scroll_accum,
                                    &mut tci_tx_gain,
                                    -60.0,
                                    60.0,
                                    1.0,
                                ) {
                                    connected.tci_tx_gain = tci_tx_gain;
                                    *connected.session.tci_tx_gain.lock().unwrap() = tci_tx_gain;
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
                                // A manual adjustment while Tune is active is
                                // a real, intentional power change (e.g.
                                // gradually raising drive while watching SWR
                                // on an antenna tuner) -- it should stick
                                // when Tune ends, not get silently discarded
                                // by the Tune button's restore-previous-value
                                // logic. Clearing pre_tune_power_watts makes
                                // that restore a no-op.
                                if connected.tune_active || connected.two_tone_active {
                                    connected.pre_tune_power_watts = None;
                                }
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
                        ui.add_space(12.0);
                        ui.colored_label(network_status_color(cat_status), "CAT")
                            .on_hover_text(network_status_hover("CAT", cat_status, &connected.cat_addr));
                        // PureSignal: only shown when actually enabled for
                        // this session (see ConnectedState::puresignal_enabled's
                        // doc comment -- a connect-time setting, not live).
                        // Same green/gray "Correcting" convention as the
                        // Settings -> PureSignal panel's own indicator,
                        // just compact enough for the main toolbar --
                        // added so PS state is visible at a glance without
                        // opening Settings, per a real report that this
                        // was hard to tell at a glance while testing.
                        if connected.puresignal_enabled {
                            ui.add_space(12.0);
                            let status = connected.tx_handle.as_ref().map(|tx| *tx.ps_status.lock().unwrap());
                            let correcting_now = status.is_some_and(|s| s.correcting);
                            // Auto-save on a false->true edge (not
                            // "every frame it's true") so a good table
                            // is persisted without a manual save
                            // button, but without spamming a disk write
                            // every frame while it stays true. Reset on
                            // the trailing edge (true->false) rather
                            // than latching "saved once ever this
                            // session", so a LATER re-calibration (e.g.
                            // after Calibrate Now) that converges again
                            // also gets saved, capturing whatever the
                            // most recent good table actually is.
                            if correcting_now && !connected.ps_was_correcting {
                                if let Some(tx) = &connected.tx_handle {
                                    tx.save_ps_corr();
                                }
                            }
                            connected.ps_was_correcting = correcting_now;
                            let (color, hover) = match status {
                                Some(s) if s.correcting => (
                                    egui::Color32::from_rgb(80, 200, 80),
                                    format!("PureSignal: Correcting (feedback level {})", s.feedback_level),
                                ),
                                Some(s) => (
                                    egui::Color32::GRAY,
                                    format!(
                                        "PureSignal: enabled, not yet correcting (feedback level {})",
                                        s.feedback_level
                                    ),
                                ),
                                None => (egui::Color32::GRAY, "PureSignal: enabled".to_string()),
                            };
                            ui.colored_label(color, "PS").on_hover_text(hover);
                        }
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
                            let tune_may_start =
                                (!mox_now || connected.tune_active) && !connected.two_tone_active;
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

                            // Two-Tone: see tx::PsParams::two_tone's doc
                            // comment for why this is a distinct control
                            // from Tune, not just a variant of it --
                            // PureSignal calibration requires a varying-
                            // envelope test signal a steady tone can
                            // never provide. Mutually exclusive with
                            // Tune (tune_may_start above already
                            // excludes two_tone_active; mirrored here).
                            let two_tone_may_start =
                                (!mox_now || connected.two_tone_active) && !connected.tune_active;
                            let two_tone_label =
                                if connected.two_tone_active { "TWO TONE ON" } else { "TWO TONE" };
                            let two_tone_color = if connected.two_tone_active {
                                egui::Color32::from_rgb(230, 140, 20)
                            } else {
                                egui::Color32::from_gray(60)
                            };
                            let two_tone_resp = ui
                                .add_enabled(
                                    two_tone_may_start,
                                    egui::Button::new(
                                        egui::RichText::new(two_tone_label)
                                            .strong()
                                            .color(egui::Color32::WHITE),
                                    )
                                    .fill(two_tone_color),
                                )
                                .on_hover_text(
                                    "Click to toggle a two-tone test signal, at Tune Power \
                                     (Settings -> TX) -- required for PureSignal calibration, \
                                     which a steady Tune tone can't provide",
                                );
                            if two_tone_resp.clicked() {
                                if connected.two_tone_active {
                                    connected.session.set_mox(false);
                                    if let Some(tx) = &connected.tx_handle {
                                        tx.set_two_tone(false);
                                    }
                                    if let Some(prev) = connected.pre_tune_power_watts.take() {
                                        connected.session.tx_power_watts.store(prev, Ordering::Relaxed);
                                    }
                                    connected.two_tone_active = false;
                                } else {
                                    let current_watts =
                                        connected.session.tx_power_watts.load(Ordering::Relaxed);
                                    connected.pre_tune_power_watts = Some(current_watts);
                                    let tune_watts = current_watts * connected.tune_power_percent / 100;
                                    connected.session.tx_power_watts.store(tune_watts, Ordering::Relaxed);
                                    if let Some(tx) = &connected.tx_handle {
                                        tx.set_two_tone(true);
                                    }
                                    connected.session.set_mox(true);
                                    connected.two_tone_active = true;
                                }
                            }

                            // Safety net: mirrors Tune's own, above.
                            if connected.two_tone_active && !connected.session.mox_active() {
                                if let Some(tx) = &connected.tx_handle {
                                    tx.set_two_tone(false);
                                }
                                if let Some(prev) = connected.pre_tune_power_watts.take() {
                                    connected.session.tx_power_watts.store(prev, Ordering::Relaxed);
                                }
                                connected.two_tone_active = false;
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

                            // RIT: click toggles on/off, scroll while
                            // hovering adjusts the offset. Placed here
                            // next to XIT (not up by VFO A/B, where it
                            // originally lived) at the user's own
                            // request, for the two to read as an obvious
                            // pair -- the tradeoff is RIT now only shows
                            // up once TX is armed too, same as XIT,
                            // even though RIT itself has nothing to do
                            // with TX capability.
                            let rit_label = if connected.rit_offset_hz == 0.0 {
                                "RIT".to_string()
                            } else {
                                format!("RIT {:+.0}", connected.rit_offset_hz)
                            };
                            let rit_resp = ui
                                .add(egui::Button::selectable(connected.rit_enabled, rit_label))
                                .on_hover_text(
                                    "Receiver Incremental Tuning -- nudges what you hear \
                                     without moving VFO A's displayed/logged frequency. \
                                     Scroll to adjust -- Shift: 10 Hz, none: 100 Hz.",
                                );
                            if rit_resp.clicked() {
                                connected.rit_enabled = !connected.rit_enabled;
                                settings_changed = true;
                            }
                            if rit_resp.hovered() {
                                let scroll_delta = ui.input(|i| i.smooth_scroll_delta);
                                let delta = if scroll_delta.y.abs() >= scroll_delta.x.abs() {
                                    scroll_delta.y
                                } else {
                                    scroll_delta.x
                                };
                                if delta != 0.0 {
                                    connected.rit_scroll_accum += delta;
                                    const NOTCH: f32 = 50.0;
                                    let shift = ui.input(|i| i.modifiers.shift);
                                    let step: i64 = if shift { 10 } else { 100 };
                                    let mut new_offset = connected.rit_offset_hz as i64;
                                    while connected.rit_scroll_accum.abs() >= NOTCH {
                                        let sign = connected.rit_scroll_accum.signum();
                                        connected.rit_scroll_accum -= sign * NOTCH;
                                        new_offset += step * sign as i64;
                                    }
                                    new_offset = new_offset.clamp(-9_999, 9_999);
                                    if new_offset as f64 != connected.rit_offset_hz {
                                        connected.rit_offset_hz = new_offset as f64;
                                        settings_changed = true;
                                    }
                                }
                            }
                            if ui.button("Clear").on_hover_text("Zero the RIT offset").clicked() {
                                connected.rit_offset_hz = 0.0;
                                settings_changed = true;
                            }

                            // XIT: same click-to-toggle/hover-to-scroll
                            // convention as RIT just above. See
                            // ConnectedState::xit_enabled's doc comment
                            // for how this nudges the real TX frequency.
                            let xit_label = if connected.xit_offset_hz == 0.0 {
                                "XIT".to_string()
                            } else {
                                format!("XIT {:+.0}", connected.xit_offset_hz)
                            };
                            let xit_resp = ui
                                .add(egui::Button::selectable(connected.xit_enabled, xit_label))
                                .on_hover_text(
                                    "Transmitter Incremental Tuning -- nudges your actual TX \
                                     frequency without moving VFO A's (or VFO B's, if Split is \
                                     on) displayed frequency. Scroll to adjust -- Shift: 10 Hz, \
                                     none: 100 Hz.",
                                );
                            if xit_resp.clicked() {
                                connected.xit_enabled = !connected.xit_enabled;
                                settings_changed = true;
                            }
                            if xit_resp.hovered() {
                                let scroll_delta = ui.input(|i| i.smooth_scroll_delta);
                                let delta = if scroll_delta.y.abs() >= scroll_delta.x.abs() {
                                    scroll_delta.y
                                } else {
                                    scroll_delta.x
                                };
                                if delta != 0.0 {
                                    connected.xit_scroll_accum += delta;
                                    const NOTCH: f32 = 50.0;
                                    let shift = ui.input(|i| i.modifiers.shift);
                                    let step: i64 = if shift { 10 } else { 100 };
                                    let mut new_offset = connected.xit_offset_hz as i64;
                                    while connected.xit_scroll_accum.abs() >= NOTCH {
                                        let sign = connected.xit_scroll_accum.signum();
                                        connected.xit_scroll_accum -= sign * NOTCH;
                                        new_offset += step * sign as i64;
                                    }
                                    new_offset = new_offset.clamp(-9_999, 9_999);
                                    if new_offset as f64 != connected.xit_offset_hz {
                                        connected.xit_offset_hz = new_offset as f64;
                                        settings_changed = true;
                                    }
                                }
                            }
                            if ui.button("Clear").on_hover_text("Zero the XIT offset").clicked() {
                                connected.xit_offset_hz = 0.0;
                                settings_changed = true;
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
                    // fixed size. Reserve room for the gap+Zoom/Pan row
                    // AND the gap+Stop button row below the waterfall
                    // first, since available_height() here is everything
                    // down to the bottom of the panel, not just what's
                    // free for the spectrum alone -- otherwise the
                    // spectrum/waterfall split greedily claims all of it
                    // and pushes the Zoom/Pan row (and potentially the
                    // Stop button) below the visible window.
                    let below_waterfall_reserve = 2.0 * (ui.spacing().interact_size.y + 8.0)
                        + SPECTRUM_WATERFALL_DIVIDER_HEIGHT;
                    let spectrum_waterfall_height =
                        (ui.available_height() - below_waterfall_reserve).max(200.0);
                    let spectrum_height =
                        (spectrum_waterfall_height * connected.spectrum_waterfall_ratio).max(80.0);
                    let (rect, spectrum_resp) = ui.allocate_exact_size(
                        egui::vec2(ui.available_width(), spectrum_height),
                        egui::Sense::click_and_drag(),
                    );

                    if let Some(pos) = spectrum_resp.interact_pointer_pos() {
                        // See suppress_refocus_click's own doc comment --
                        // this is the one thing that click-to-refocus the
                        // window must NOT also do.
                        if spectrum_resp.clicked() && !suppress_refocus_click {
                            let new_freq = freq_at_x(pos.x, rect, freq_hz, sample_rate, connected.spectrum_zoom, pan_offset_hz);
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
                    // Click-and-drag: moves the dial by however far the
                    // cursor has actually moved (drag_delta(), zero once
                    // the pointer stops), NOT by re-deriving an absolute
                    // frequency from the current cursor position each
                    // frame the way the plain click above does -- that
                    // approach fed back on itself here, since retuning
                    // re-centers the spectrum on the new dial frequency,
                    // which shifts what a STATIONARY cursor maps to on
                    // the very next frame, so the frequency kept drifting
                    // even after the drag stopped moving (a real report).
                    if spectrum_resp.dragged() && !suppress_refocus_click {
                        let hz_per_px = (2.0 * visible_half_span_hz) / rect.width().max(1.0) as f64;
                        // Negated: dragging right pulls lower frequencies
                        // in from the right edge, the same way dragging a
                        // map or a scrollable view does -- content moves
                        // right = the reference point tracks left. A real
                        // report: the unnegated version (drag right ->
                        // frequency up, like a tuning knob) felt backwards.
                        connected.drag_tune_accum_hz += -spectrum_resp.drag_delta().x as f64 * hz_per_px;
                        const STEP_HZ: i64 = 1_000;
                        let mut new_freq = dial_freq_hz as i64;
                        while connected.drag_tune_accum_hz.abs() >= STEP_HZ as f64 {
                            let sign = connected.drag_tune_accum_hz.signum();
                            connected.drag_tune_accum_hz -= sign * STEP_HZ as f64;
                            new_freq += STEP_HZ * sign as i64;
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

                    // VFO B: scroll directly on its own box to change its
                    // stored frequency. No live receiver sits behind it
                    // (see ConnectedState::vfo_b_frequency_hz's doc
                    // comment), so unlike VFO A's block above there's no
                    // CTUN/passband/retune handling needed here -- just a
                    // plain accumulate-then-step onto the stored value.
                    if vfo_b_label.hovered() {
                        let scroll_delta = ui.input(|i| i.smooth_scroll_delta);
                        let delta = if scroll_delta.y.abs() >= scroll_delta.x.abs() {
                            scroll_delta.y
                        } else {
                            scroll_delta.x
                        };

                        if delta != 0.0 {
                            connected.vfo_b_scroll_accum += delta;
                            // Same NOTCH/step convention as VFO A's block
                            // above.
                            const NOTCH: f32 = 50.0;
                            let shift = ui.input(|i| i.modifiers.shift);
                            let step: i64 = if shift { 100 } else { 1_000 };

                            let mut new_freq = connected.vfo_b_frequency_hz as i64;
                            while connected.vfo_b_scroll_accum.abs() >= NOTCH {
                                let sign = connected.vfo_b_scroll_accum.signum();
                                connected.vfo_b_scroll_accum -= sign * NOTCH;
                                new_freq += step * sign as i64;
                            }
                            new_freq = new_freq.max(0);

                            if new_freq as u32 != connected.vfo_b_frequency_hz {
                                connected.vfo_b_frequency_hz = new_freq as u32;
                                settings_changed = true;
                            }
                        }
                    }

                    ui.painter().rect_filled(rect, 0.0, egui::Color32::BLACK);

                    // Frequency axis: assumes the spectrum linearly spans
                    // the full DDC sample rate centered on the tuned
                    // frequency (standard convention at zoom=1). If the
                    // displayed span looks wrong, this assumption is the
                    // first thing to check. half_span_hz/visible_half_span_hz/
                    // pan_offset_hz (zoom/pan) are computed earlier in this
                    // same frame -- see their own doc comment -- so the
                    // click-to-tune handlers above (which run before this
                    // drawing code) can use them too.
                    let view_center_hz = freq_hz as f64 + pan_offset_hz;

                    draw_band_edge_markers(ui.painter(), rect, view_center_hz, visible_half_span_hz);

                    // While transmitting, this displays tx_spectrum --
                    // generated TX IQ that's always centered on the real
                    // TX carrier by construction (tx_spectrum never has
                    // set_ctun called on it, unlike the RX analyzer), with
                    // no RX-style CTUN shift concept of its own. Force the
                    // offset to 0 here so the filter overlay/dial marker
                    // below land at the TX carrier's actual position
                    // (screen center) rather than a stale RX/CTUN offset
                    // -- which, now that Split/CTUN can put TX on a
                    // different frequency than RX (see
                    // RadioSession::tx_frequency_hz's doc comment), is not
                    // guaranteed to be anywhere near the current TX
                    // frequency at all.
                    let ctun_offset_hz = if transmitting { 0.0 } else { ctun_offset_hz };

                    // Filter passband overlay: shaded region between the
                    // current mode's filter edges (mirrored onto TXA --
                    // see tx_handle.set_width_hz's call sites -- so this
                    // is genuinely the TX filter while transmitting, not
                    // just a repurposed RX one), plus a line marking the
                    // dial (tuned) frequency itself. Same freq-to-x
                    // mapping as the axis ticks below. Colored red while
                    // transmitting, matching the frequency display's own
                    // TX color, so it reads as "this is what's actually
                    // going out" rather than looking like the ordinary RX
                    // passband indicator.
                    let x_for_offset = |offset_hz: f64| -> f32 {
                        let frac = ((offset_hz - pan_offset_hz + visible_half_span_hz)
                            / (2.0 * visible_half_span_hz))
                            .clamp(0.0, 1.0) as f32;
                        rect.left() + frac * rect.width()
                    };
                    let (passband_fill, passband_line) = if transmitting {
                        (
                            egui::Color32::from_rgba_unmultiplied(230, 90, 70, 60),
                            egui::Color32::from_rgb(255, 120, 90),
                        )
                    } else {
                        (
                            egui::Color32::from_rgba_unmultiplied(70, 150, 230, 50),
                            egui::Color32::from_rgb(100, 180, 255),
                        )
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
                        passband_fill,
                    );
                    for x in [x_low, x_high] {
                        ui.painter().line_segment(
                            [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
                            egui::Stroke::new(1.0, passband_line),
                        );
                    }
                    let x_dial = x_for_offset(ctun_offset_hz);

                    let num_freq_ticks = 10;
                    for t in 0..num_freq_ticks {
                        let frac = t as f32 / (num_freq_ticks - 1) as f32;
                        let x = rect.left() + frac * rect.width();
                        ui.painter().line_segment(
                            [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
                            egui::Stroke::new(1.0, egui::Color32::from_gray(55)),
                        );
                        // Skip the label at the first/last tick -- right at
                        // the plot's edge, it either gets clipped or hangs
                        // off into the surrounding UI.
                        if t == 0 || t == num_freq_ticks - 1 {
                            continue;
                        }
                        let tick_freq_hz = view_center_hz - visible_half_span_hz
                            + frac as f64 * (2.0 * visible_half_span_hz);
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

                        // Plain full-width bin mapping -- unlike an
                        // earlier version of this, no zoom-aware
                        // filtering/cropping is needed here: WDSP's own
                        // analyzer (see SpectrumAnalyzer::set_zoom_pan's
                        // doc comment) already returns spectrum_row
                        // containing ONLY the current zoomed/panned
                        // window's data, evenly spaced across all
                        // SPECTRUM_WIDTH bins -- the real resolution gain
                        // happens upstream, in WDSP's own FFT size, not
                        // here.
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

                    // Small audio-waveform overlay -- output audio while
                    // receiving, whatever's actually feeding TX while
                    // transmitting (see TxHandle::waveform_tap's doc
                    // comment: fed at the same point as tx_audio_monitor,
                    // post source selection, so this reflects mic/TCI/
                    // radio-mic alike regardless of which is in use).
                    let waveform_samples = if transmitting {
                        connected
                            .tx_handle
                            .as_ref()
                            .map(|tx| peek_recent_samples(&tx.waveform_tap, WAVEFORM_WINDOW_SAMPLES))
                    } else {
                        Some(peek_recent_samples(&connected.spectrum.waveform_out, WAVEFORM_WINDOW_SAMPLES))
                    };
                    if let Some(samples) = waveform_samples {
                        draw_audio_waveform(ui.painter(), rect, &samples);
                    }

                    if let Some(pos) = spectrum_resp.hover_pos() {
                        let hover_freq = freq_at_x(pos.x, rect, freq_hz, sample_rate, connected.spectrum_zoom, pan_offset_hz);
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
                        egui::Sense::click_and_drag(),
                    );
                    if let Some(pos) = waterfall_click_resp.interact_pointer_pos() {
                        // See suppress_refocus_click's own doc comment.
                        if waterfall_click_resp.clicked() && !suppress_refocus_click {
                            let new_freq = freq_at_x(pos.x, rect, freq_hz, sample_rate, connected.spectrum_zoom, pan_offset_hz);
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
                    // Click-and-drag -- see the spectrum pane's identical
                    // treatment above for why this uses drag_delta()
                    // rather than an absolute cursor-position mapping.
                    if waterfall_click_resp.dragged() && !suppress_refocus_click {
                        let hz_per_px = (2.0 * visible_half_span_hz) / rect.width().max(1.0) as f64;
                        connected.drag_tune_accum_hz += -waterfall_click_resp.drag_delta().x as f64 * hz_per_px;
                        const STEP_HZ: i64 = 1_000;
                        let mut new_freq = dial_freq_hz as i64;
                        while connected.drag_tune_accum_hz.abs() >= STEP_HZ as f64 {
                            let sign = connected.drag_tune_accum_hz.signum();
                            connected.drag_tune_accum_hz -= sign * STEP_HZ as f64;
                            new_freq += STEP_HZ * sign as i64;
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
                    if let Some(tex_id) = waterfall_texture_id {
                        // No zoom-aware UV cropping needed -- see the
                        // spectrum trace's identical note above. Each
                        // waterfall row already covers only the current
                        // zoomed/panned window (WDSP's own analyzer did
                        // the real cropping), so the texture is drawn at
                        // its full [0,1] UV range as-is.
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
                        let hover_freq = freq_at_x(pos.x, rect, freq_hz, sample_rate, connected.spectrum_zoom, pan_offset_hz);
                        draw_freq_hover_tooltip(ui.painter(), pos, hover_freq);
                    }

                    ui.horizontal(|ui| {
                        // Fill the available width -- reserve space for
                        // the two labels, Reset button, and inter-widget
                        // spacing, split the rest evenly between the two
                        // sliders. Scoped to this row's own child Ui, so
                        // it doesn't affect any other slider elsewhere
                        // in the window.
                        let reserved = 230.0;
                        ui.spacing_mut().slider_width = ((ui.available_width() - reserved) / 2.0).max(80.0);

                        ui.label("Zoom:");
                        let mut zoom = connected.spectrum_zoom;
                        if scroll_slider_i32(ui, &mut connected.slider_scroll_accum, &mut zoom, 1..=16, 1, "x") {
                            connected.spectrum_zoom = zoom;
                            settings_changed = true;
                        }
                        ui.add_space(12.0);
                        ui.label("Pan:");
                        // Disabled rather than hidden at zoom 1 -- there's
                        // nothing to pan to (max_pan_hz is 0), but keeping
                        // it visible-but-inert avoids the layout jumping
                        // around as zoom changes.
                        ui.add_enabled_ui(connected.spectrum_zoom > 1, |ui| {
                            let mut pan = connected.spectrum_pan;
                            if scroll_slider_f32(ui, &mut connected.slider_scroll_accum, &mut pan, -1.0..=1.0, 0.1) {
                                connected.spectrum_pan = pan;
                                settings_changed = true;
                            }
                        });
                        if ui.button("Reset").on_hover_text("Zoom 1x, Pan centered").clicked() {
                            connected.spectrum_zoom = 1;
                            connected.spectrum_pan = 0.0;
                            settings_changed = true;
                        }
                    });

                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Stop").clicked() {
                            stop_clicked = true;
                        }
                        // See ConnectedState::status_message's doc
                        // comment. Wisdom generation takes priority
                        // while it's actually relevant (matches the
                        // waterfall overlay's own condition above) --
                        // it's the one thing this area was specifically
                        // added for, and it's already transient/self-
                        // clearing once the waterfall starts rendering,
                        // unlike status_message which persists until
                        // something else overwrites it.
                        if waterfall_texture_id.is_none() {
                            ui.weak(wisdom_status_text());
                        } else if let Some(msg) = &connected.status_message {
                            ui.weak(msg);
                        }
                    });
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
                            // frame. Was 0.15 (~150ms time constant) --
                            // confirmed via real-hardware PureSignal
                            // testing that this was still visibly
                            // fluctuating on a Two Tone signal while an
                            // external wattmeter (which averages over a
                            // longer window) showed steady output, i.e.
                            // the true TX power was stable and this was
                            // purely under-damped display, not a real
                            // envelope problem. Lowered to a ~500ms time
                            // constant, closer to typical analog
                            // wattmeter ballistics -- still fast enough
                            // to track a real key-up ramp.
                            const SMOOTHING_ALPHA: f32 = 0.045;
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

                        // ADC front-end overload -- see
                        // RadioSession::adc0_overload's doc comment.
                        // Reserves this row's height unconditionally (a
                        // real report: this message popping in and out
                        // was pushing the Settings/Add Receiver buttons
                        // below it up and down) rather than only
                        // allocating a row when there's something to
                        // show.
                        let adc0_ov = connected
                            .session
                            .adc0_overload
                            .load(std::sync::atomic::Ordering::Relaxed);
                        let adc1_ov = connected
                            .session
                            .adc1_overload
                            .load(std::sync::atomic::Ordering::Relaxed);
                        let line_height = ui.text_style_height(&egui::TextStyle::Body);
                        let (row_rect, _resp) = ui.allocate_exact_size(
                            egui::vec2(180.0, line_height),
                            egui::Sense::hover(),
                        );
                        if adc0_ov || adc1_ov {
                            let text = if adc0_ov && adc1_ov {
                                "ADC0+ADC1 OVERLOAD"
                            } else if adc0_ov {
                                "ADC0 OVERLOAD"
                            } else {
                                "ADC1 OVERLOAD"
                            };
                            ui.painter().text(
                                row_rect.left_center(),
                                egui::Align2::LEFT_CENTER,
                                text,
                                egui::TextStyle::Body.resolve(ui.style()),
                                egui::Color32::from_rgb(255, 60, 60),
                            );
                        }

                        // TX FIFO overrun/underrun -- see
                        // RadioSession::tx_fifo_underrun's doc comment.
                        // Same fixed-height-row treatment as the ADC
                        // overload row above, for the same reason.
                        let fifo_under = connected
                            .session
                            .tx_fifo_underrun
                            .load(std::sync::atomic::Ordering::Relaxed);
                        let fifo_over = connected
                            .session
                            .tx_fifo_overrun
                            .load(std::sync::atomic::Ordering::Relaxed);
                        if fifo_under || fifo_over {
                            connected.tx_fifo_warning_until = Some(Instant::now() + Duration::from_secs(2));
                        }
                        let (fifo_row_rect, _resp) = ui.allocate_exact_size(
                            egui::vec2(180.0, line_height),
                            egui::Sense::hover(),
                        );
                        if let Some(until) = connected.tx_fifo_warning_until {
                            if Instant::now() < until {
                                let text = if fifo_under && fifo_over {
                                    "TX Underrun/Overrun"
                                } else if fifo_under {
                                    "TX Underrun"
                                } else {
                                    "TX Overrun"
                                };
                                ui.painter().text(
                                    fifo_row_rect.left_center(),
                                    egui::Align2::LEFT_CENTER,
                                    text,
                                    egui::TextStyle::Body.resolve(ui.style()),
                                    egui::Color32::from_rgb(255, 60, 60),
                                );
                            } else {
                                connected.tx_fifo_warning_until = None;
                            }
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
                //
                // BUG FIX: floor was unconditionally 1, not accounting
                // for Diversity's own reserved wire 1 (radio::
                // RadioSession::diversity_enabled) -- wire 1 has no
                // ExtraReceiver/window at all (it feeds the diversity
                // combiner instead, see spawn_diversity_combiner), so
                // with no "Add Receiver" windows open this ran every
                // single frame and stomped active_receiver_count back
                // down to 1 moments after connect, which stops the radio
                // streaming DDC1 entirely -- the diversity combiner then
                // starves on its aux input forever (confirmed via a real
                // per-second diagnostic: main_raw filled to capacity,
                // aux_raw(buf1) stuck at 0, pushed_last_sec 0), which is
                // exactly "spectrum and waterfall hang the moment
                // Diversity is enabled".
                let diversity_floor =
                    if connected.session.diversity_enabled.load(Ordering::Relaxed) { 2 } else { 1 };
                let active_count = connected
                    .extra_receivers
                    .iter()
                    .map(|rx| rx.lock().unwrap().ddc_index)
                    .max()
                    .map_or(diversity_floor, |highest| (highest + 1).max(diversity_floor));
                connected
                    .session
                    .active_receiver_count
                    .store(active_count as u32, Ordering::Relaxed);
                for rx in connected.extra_receivers.clone() {
                    let ddc_index = rx.lock().unwrap().ddc_index;
                    let viewport_id = egui::ViewportId::from_hash_of(("extra_receiver", ddc_index));
                    let title = format!("{} - RX {}", base_title, ddc_index + 1);
                    let rx_for_closure = Arc::clone(&rx);
                    // Position/size seed from this radio's saved config --
                    // see ExtraReceiver::initial_window_geometry's doc
                    // comment for why this MUST be a value that stays
                    // constant across frames (a one-time seed, not the
                    // live-tracked window_geometry) to avoid fighting the
                    // user dragging/resizing the window themselves.
                    let seed_geometry = rx_for_closure.lock().unwrap().initial_window_geometry;
                    let mut viewport_builder =
                        egui::ViewportBuilder::default().with_title(title).with_inner_size([1024.0, 500.0]);
                    if let Some(g) = seed_geometry {
                        viewport_builder =
                            viewport_builder.with_position([g.x, g.y]).with_inner_size([g.width, g.height]);
                    }
                    ui.ctx().show_viewport_deferred(
                        viewport_id,
                        viewport_builder,
                        move |ui: &mut egui::Ui, _class: egui::ViewportClass| {
                            // Tracked every frame (not just when the
                            // periodic Config save fires) for the same
                            // reason as the main window's own geometry --
                            // see ExtraReceiver::window_geometry's doc
                            // comment.
                            if let (Some(outer), Some(inner)) = (
                                ui.input(|i| i.viewport().outer_rect),
                                ui.input(|i| i.viewport().inner_rect),
                            ) {
                                rx_for_closure.lock().unwrap().window_geometry = Some(WindowGeometry {
                                    x: outer.min.x,
                                    y: outer.min.y,
                                    width: inner.width(),
                                    height: inner.height(),
                                });
                            }

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

                                    ui.add_space(4.0);
                                    if ui.button("Settings...").clicked() {
                                        let mut rx = rx_for_closure.lock().unwrap();
                                        rx.show_settings_window = !rx.show_settings_window;
                                    }
                                });

                            egui::CentralPanel::default().show(ui, |ui| {
                                render_extra_receiver_ui(ui, &rx_for_closure);
                            });

                            let show_settings = rx_for_closure.lock().unwrap().show_settings_window;
                            if show_settings {
                                // Own OS-level viewport, not nested inside
                                // this receiver's window -- matches the
                                // main Settings window's own treatment
                                // (see its doc comment for why a light
                                // theme override is needed) rather than
                                // being confined to this receiver's own
                                // viewport.
                                let light_visuals = egui::Visuals::light();
                                let light_style = egui::Style {
                                    visuals: light_visuals.clone(),
                                    ..Default::default()
                                };
                                let settings_viewport_id = egui::ViewportId::from_hash_of((
                                    "extra_receiver_settings",
                                    ddc_index,
                                ));
                                let rx_for_settings = Arc::clone(&rx_for_closure);
                                let settings_title = format!("Receiver {} Settings", ddc_index + 1);
                                ui.ctx().show_viewport_deferred(
                                    settings_viewport_id,
                                    egui::ViewportBuilder::default()
                                        .with_title(settings_title)
                                        .with_inner_size([420.0, 500.0]),
                                    move |ui: &mut egui::Ui, _class: egui::ViewportClass| {
                                        if ui.input(|i| i.viewport().close_requested()) {
                                            rx_for_settings.lock().unwrap().show_settings_window = false;
                                            return;
                                        }
                                        egui::CentralPanel::default()
                                            .frame(egui::Frame::central_panel(&light_style))
                                            .show(ui, |ui| {
                                                ui.visuals_mut().clone_from(&light_visuals);
                                                render_extra_receiver_settings(ui, &rx_for_settings);
                                            });
                                    },
                                );
                            }
                        },
                    );
                }

                if connected.show_settings_window {
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
                    // Rendered in its own OS-level viewport (like the
                    // extra receiver windows) rather than an
                    // embedded egui::Window, so it can be dragged
                    // outside the main window's bounds -- see
                    // show_viewport_immediate (not _deferred, since
                    // this closure borrows `connected` directly by
                    // reference rather than through an Arc<Mutex<>>).
                    let mut close_requested = false;
                    ui.ctx().show_viewport_immediate(
                        egui::ViewportId::from_hash_of("settings_window"),
                        egui::ViewportBuilder::default()
                            .with_title("Settings")
                            .with_inner_size([860.0, 700.0]),
                        |ui, _class| {
                            if ui.input(|i| i.viewport().close_requested()) {
                                close_requested = true;
                                return;
                            }
                            egui::CentralPanel::default()
                                .frame(egui::Frame::central_panel(&light_style))
                                .show(ui, |ui| {
                            ui.visuals_mut().clone_from(&light_visuals);
                            ui.horizontal(|ui| {
                                for (tab, label) in [
                                    (SettingsTab::Network, "Network"),
                                    (SettingsTab::Audio, "Audio"),
                                    (SettingsTab::Agc, "RX"),
                                    (SettingsTab::Spectrum, "Spectrum"),
                                    (SettingsTab::Tx, "TX"),
                                    (SettingsTab::PaCalibration, "PA Calibration"),
                                    (SettingsTab::PureSignal, "PureSignal"),
                                    (SettingsTab::Diversity, "Diversity"),
                                    (SettingsTab::Equalizer, "Equalizer"),
                                    (SettingsTab::Firmware, "Firmware"),
                                ] {
                                    // Diversity requires a 2-ADC board -- see
                                    // radio::RadioSession::diversity_enabled's
                                    // doc comment. Hidden entirely rather than
                                    // shown-disabled on boards that can't use
                                    // it, same gating style already used for
                                    // the per-receiver ADC dropdown below.
                                    if tab == SettingsTab::Diversity && connected.device.adcs != 2 {
                                        continue;
                                    }
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
                                            if start_stop_button(ui, true) {
                                                connected.rigctl_server = None;
                                                settings_changed = true;
                                            }
                                        } else if start_stop_button(ui, false) {
                                            connected.rigctl_error = None;
                                            connected.rigctl_server = match RigctlServer::start(
                                                &connected.rigctl_addr,
                                                Arc::clone(&connected.session.requested_frequency_hz),
                                                Arc::clone(&connected.session.rx_frequency_hz),
                                                connected.spectrum.demod_params_handle(),
                                                Arc::clone(&connected.spectrum.display),
                                                Arc::clone(&connected.session.mox),
                                                connected.rigctl_debug_log.clone(),
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
                                    {
                                        let mut logging = connected.rigctl_debug_log.is_enabled();
                                        if ui
                                            .checkbox(&mut logging, "Log to file (rigctl_log.txt)")
                                            .on_hover_text(
                                                "Logs every command received and reply sent, for debugging a \
                                                 client's behavior -- saved alongside this radio's settings.",
                                            )
                                            .changed()
                                        {
                                            connected.rigctl_debug_log.set_enabled(logging);
                                            settings_changed = true;
                                        }
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
                                            if start_stop_button(ui, true) {
                                                connected.tci_server = None;
                                                settings_changed = true;
                                            }
                                        } else if start_stop_button(ui, false) {
                                            connected.tci_error = None;
                                            connected.tci_server = match TciServer::start(
                                                &connected.tci_addr,
                                                Arc::clone(&connected.session.requested_frequency_hz),
                                                Arc::clone(&connected.session.rx_frequency_hz),
                                                Arc::clone(&connected.session.sample_rate),
                                                connected.spectrum.demod_params_handle(),
                                                Arc::clone(&connected.session.mox),
                                                Arc::clone(&connected.spectrum.tci_audio_out),
                                                Arc::clone(&connected.spectrum.iq_out),
                                                Arc::clone(&connected.session.tci_tx_audio),
                                                Arc::clone(&connected.session.tci_tx_gain),
                                                Arc::clone(&connected.session.tci_wants_mic),
                                                format!("{:?}", connected.device.board),
                                                connected.tci_debug_log.clone(),
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
                                    {
                                        let mut logging = connected.tci_debug_log.is_enabled();
                                        if ui
                                            .checkbox(&mut logging, "Log to file (tci_log.txt)")
                                            .on_hover_text(
                                                "Logs every command received and reply sent, for debugging a \
                                                 client's behavior -- saved alongside this radio's settings.",
                                            )
                                            .changed()
                                        {
                                            connected.tci_debug_log.set_enabled(logging);
                                            settings_changed = true;
                                        }
                                    }

                                    ui.add_space(8.0);
                                    ui.label(
                                        "CAT (Kenwood TS-2000 emulation, for loggers/rig-control software \
                                         e.g. N1MM+, Log4OM, DXLab Commander, Ham Radio Deluxe):",
                                    );
                                    ui.horizontal(|ui| {
                                        let running = connected.cat_server.is_some();
                                        ui.add_enabled(
                                            !running,
                                            egui::TextEdit::singleline(&mut connected.cat_addr),
                                        );
                                        if running {
                                            if start_stop_button(ui, true) {
                                                connected.cat_server = None;
                                                settings_changed = true;
                                            }
                                        } else if start_stop_button(ui, false) {
                                            connected.cat_error = None;
                                            connected.cat_server = match CatServer::start(
                                                &connected.cat_addr,
                                                Arc::clone(&connected.session.requested_frequency_hz),
                                                Arc::clone(&connected.session.rx_frequency_hz),
                                                connected.spectrum.demod_params_handle(),
                                                Arc::clone(&connected.spectrum.display),
                                                Arc::clone(&connected.session.mox),
                                                connected.cat_debug_log.clone(),
                                            ) {
                                                Ok(s) => Some(s),
                                                Err(e) => {
                                                    let msg = format!(
                                                        "couldn't listen on {}: {e}",
                                                        connected.cat_addr
                                                    );
                                                    eprintln!("cat: {msg}");
                                                    connected.cat_error = Some(msg);
                                                    None
                                                }
                                            };
                                            settings_changed = true;
                                        }
                                    });
                                    ui.weak(if connected.cat_server.is_some() {
                                        "Running -- Stop before changing the address."
                                    } else {
                                        "Stopped"
                                    });
                                    if let Some(err) = &connected.cat_error {
                                        ui.colored_label(egui::Color32::from_rgb(220, 60, 60), err);
                                    }
                                    {
                                        let mut logging = connected.cat_debug_log.is_enabled();
                                        if ui
                                            .checkbox(&mut logging, "Log to file (cat_log.txt)")
                                            .on_hover_text(
                                                "Logs every command received and reply sent, for debugging a \
                                                 client's behavior -- saved alongside this radio's settings.",
                                            )
                                            .changed()
                                        {
                                            connected.cat_debug_log.set_enabled(logging);
                                            settings_changed = true;
                                        }
                                    }

                                    ui.add_space(8.0);
                                    ui.weak(
                                        "Format: address:port -- each protocol above shows its own default \
                                         (0.0.0.0 listens on every network interface, so another machine on \
                                         your network can connect too, not just this one). Use 127.0.0.1:PORT \
                                         instead to restrict it to this machine only. None of these have any \
                                         authentication, so only expose them on networks you trust. rigctl \
                                         and TCI are RX only -- PTT is accepted but not implemented. CAT's \
                                         TX;/RX; commands do drive real PTT (same as the on-screen PTT \
                                         button), but only while Settings -> TX -> Enable Transmit is on.",
                                    );
                                }

                                SettingsTab::Firmware => {
                                    ui.label("Firmware / IP configuration:");
                                    if ui
                                        .button("Firmware Update...")
                                        .on_hover_text(
                                            "Update this radio's FPGA firmware or change its static IP \
                                             while it's normally running -- see also the Discovery \
                                             screen's bootloader-mode Firmware Update, which is more \
                                             thoroughly verified.",
                                        )
                                        .clicked()
                                    {
                                        connected.firmware_update = Some(bootloader_ui::FirmwareUpdateWindow::new_in_app(
                                            connected.device.address.ip(),
                                            connected.device.mac,
                                        ));
                                    }
                                }

                                SettingsTab::Audio => {
                                    ui.label("RX audio:");
                                    ui.horizontal(|ui| {
                                        ui.label("Output device:");
                                        let devices = audio::list_output_devices();
                                        let current_label = connected
                                            .audio_output_device
                                            .clone()
                                            .unwrap_or_else(|| "(System Default)".to_string());
                                        egui::ComboBox::from_id_salt("main_audio_output_device")
                                            .selected_text(current_label)
                                            .show_ui(ui, |ui| {
                                                if ui
                                                    .selectable_label(
                                                        connected.audio_output_device.is_none(),
                                                        "(System Default)",
                                                    )
                                                    .clicked()
                                                    && connected.audio_output_device.is_some()
                                                {
                                                    connected.audio_output_device = None;
                                                    connected.audio_output =
                                                        AudioOutput::start(Arc::clone(&connected.spectrum.audio_out), None)
                                                            .ok();
                                                    settings_changed = true;
                                                }
                                                for name in &devices {
                                                    let selected =
                                                        connected.audio_output_device.as_deref() == Some(name.as_str());
                                                    if ui.selectable_label(selected, name).clicked() && !selected {
                                                        connected.audio_output_device = Some(name.clone());
                                                        connected.audio_output = AudioOutput::start(
                                                            Arc::clone(&connected.spectrum.audio_out),
                                                            Some(name),
                                                        )
                                                        .ok();
                                                        settings_changed = true;
                                                    }
                                                }
                                            })
                                            .response
                                            .on_hover_text(
                                                "Where local RX audio plays -- e.g. a virtual cable \
                                                 (VB-Audio Virtual Cable on Windows) to feed a decoder \
                                                 like WSJT-X instead of/alongside real speakers.",
                                            );
                                    });

                                    ui.add_space(8.0);
                                    ui.separator();
                                    ui.add_space(8.0);

                                    ui.label("TX audio:");
                                    ui.horizontal(|ui| {
                                        ui.label("Input device:");
                                        let devices = audio::list_input_devices();
                                        let current_label = connected
                                            .mic_input_device
                                            .clone()
                                            .unwrap_or_else(|| "(System Default)".to_string());
                                        egui::ComboBox::from_id_salt("main_mic_input_device")
                                            .selected_text(current_label)
                                            .show_ui(ui, |ui| {
                                                if ui
                                                    .selectable_label(
                                                        connected.mic_input_device.is_none(),
                                                        "(System Default)",
                                                    )
                                                    .clicked()
                                                    && connected.mic_input_device.is_some()
                                                {
                                                    connected.mic_input_device = None;
                                                    if let Some(mic) = &connected.mic_input {
                                                        let buffer = Arc::clone(mic.buffer());
                                                        match MicInput::start(buffer, None) {
                                                            Ok(new_mic) => connected.mic_input = Some(new_mic),
                                                            Err(e) => eprintln!("mic input unavailable: {e}"),
                                                        }
                                                    }
                                                    settings_changed = true;
                                                }
                                                for name in &devices {
                                                    let selected =
                                                        connected.mic_input_device.as_deref() == Some(name.as_str());
                                                    if ui.selectable_label(selected, name).clicked() && !selected {
                                                        connected.mic_input_device = Some(name.clone());
                                                        if let Some(mic) = &connected.mic_input {
                                                            let buffer = Arc::clone(mic.buffer());
                                                            match MicInput::start(buffer, Some(name)) {
                                                                Ok(new_mic) => connected.mic_input = Some(new_mic),
                                                                Err(e) => eprintln!("mic input unavailable: {e}"),
                                                            }
                                                        }
                                                        settings_changed = true;
                                                    }
                                                }
                                            })
                                            .response
                                            .on_hover_text(
                                                "Where TX audio is captured from -- e.g. a virtual cable \
                                                 (VB-Audio Virtual Cable on Windows) to feed TX audio from \
                                                 another application instead of a real mic.",
                                            );
                                    });
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

                                    // BUG FIX: this used to be gated on
                                    // `connected.device.protocol == 2`,
                                    // hiding ADC/Antenna selection for
                                    // any Protocol 1 board -- but Angelia/
                                    // Orion/Orion2 have 2 ADCs and Alex
                                    // antenna relays on P1 too (both are
                                    // already wired into P1's own
                                    // p1_build_packet -- see radio.rs's
                                    // wire-0 ADC bits and antenna_val/c4
                                    // handling), and the extra-receiver
                                    // settings panel already shows this
                                    // unconditionally (render_extra_receiver_settings,
                                    // no protocol check at all) -- a real
                                    // report confirmed a real Angelia was
                                    // missing both controls. Same recurring
                                    // pattern as Add Receiver/extra_frequencies_hz/
                                    // RX2 filter tracking before it -- check
                                    // for a bare `protocol == 2` gate first
                                    // whenever a P1 feature seems mysteriously
                                    // capped/missing while the P2 equivalent
                                    // works fine.
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

                                    // Streams the main receiver's demodulated audio back to the
                                    // radio's own local audio output (a headphone/speaker jack
                                    // driven by the radio's own DAC, independent of this PC's
                                    // sound card) -- see radio::RadioSession::
                                    // send_rx_audio_to_radio's doc comment. Off by default:
                                    // most setups have no local audio output in use, and this
                                    // adds continuous extra network/USB traffic for no benefit
                                    // otherwise.
                                    {
                                        let mut send_rx_audio = connected
                                            .session
                                            .send_rx_audio_to_radio
                                            .load(Ordering::Relaxed);
                                        if ui.checkbox(&mut send_rx_audio, "Send RX audio to radio").changed() {
                                            connected
                                                .session
                                                .send_rx_audio_to_radio
                                                .store(send_rx_audio, Ordering::Relaxed);
                                            settings_changed = true;
                                        }
                                        if send_rx_audio
                                            && connected.device.protocol == 1
                                            && matches!(
                                                connected.device.board,
                                                Boards::HermesLite | Boards::HermesLite2
                                            )
                                        {
                                            ui.weak(
                                                "No effect on this board over Protocol 1 -- its \
                                                 firmware reuses this slot for something else.",
                                            );
                                        }
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
                                        ui.label("Low:");
                                        ui.add_enabled_ui(!connected.db_low_auto, |ui| {
                                            let mut low = connected.db_low;
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
                                        });
                                        if ui
                                            .selectable_label(connected.db_low_auto, "Auto")
                                            .on_hover_text(
                                                "Continuously track the lowest level shown in \
                                                 the spectrum trace, smoothed to avoid jumping \
                                                 on every noise spike.",
                                            )
                                            .clicked()
                                        {
                                            connected.db_low_auto = !connected.db_low_auto;
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
                                            // Tear down the old tx_spectrum before creating a
                                            // replacement -- same reasoning as the RX
                                            // SpectrumHandle rebuild in change_sample_rate.
                                            connected.tx_spectrum.stop();
                                            let tx_spectrum_iq: Arc<Mutex<VecDeque<IqSample>>> =
                                                Arc::new(Mutex::new(VecDeque::new()));
                                            connected.tx_spectrum = SpectrumHandle::start(
                                                connected.session.iq_buffers.len() as i32 + 1,
                                                Arc::clone(&tx_spectrum_iq),
                                                duc_rate,
                                                None,
                                                Arc::clone(&connected.session.mox),
                                            );
                                            let mic_buffer = Arc::new(Mutex::new(VecDeque::new()));
                                            match MicInput::start(
                                                Arc::clone(&mic_buffer),
                                                connected.mic_input_device.as_deref(),
                                            ) {
                                                Ok(mic) => {
                                                    let tx_handle = TxHandle::start(
                                                        mic_buffer,
                                                        Arc::clone(&connected.session.tci_tx_audio),
                                                        Arc::clone(&connected.session.radio_mic_audio),
                                                        Arc::clone(&connected.session.tx_audio_source),
                                                        Arc::clone(&connected.session.tci_wants_mic),
                                                        Arc::clone(&connected.session.tx_iq),
                                                        Arc::clone(&tx_spectrum_iq),
                                                        Arc::clone(&connected.session.mox),
                                                        connected.session.iq_buffers.len() as i32,
                                                        connected.device.protocol,
                                                        48_000,
                                                        duc_rate,
                                                        connected.puresignal_enabled,
                                                        Arc::clone(&connected.session.ps_rx_feedback_iq),
                                                        Arc::clone(&connected.session.ps_tx_feedback_iq),
                                                        ps_corr_path(connected.device.mac),
                                                    );
                                                    tx_handle.set_mic_gain(connected.mic_gain);
                                                    tx_handle.set_mode(connected.spectrum.mode());
                                                    tx_handle.set_width_hz(connected.spectrum.width_hz());
                                                    tx_handle.set_ps_enabled(connected.ps_enabled);
                                                    tx_handle.set_ps_hw_peak(connected.ps_hw_peak);
                                                    tx_handle.set_ps_mox_delay(connected.ps_mox_delay);
                                                    tx_handle.set_ps_loop_delay(connected.ps_loop_delay);
                                                    tx_handle.set_ps_tx_delay_ns(connected.ps_tx_delay_ns);
                                                    tx_handle.set_ps_ptol(connected.ps_ptol);
                                                    // See connect_to_device's identical restore --
                                                    // this rebuild also opens a fresh WDSP channel
                                                    // with no calibration history of its own.
                                                    if connected.puresignal_enabled {
                                                        if let Some(path) = ps_corr_path(connected.device.mac) {
                                                            if path.exists() {
                                                                tx_handle.restore_ps_corr();
                                                            }
                                                        }
                                                    }
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
                                            connected.tx_audio_monitor_output = None;
                                            connected.mic_input = None;
                                            connected.tx_enabled = false;
                                            // tx_handle is gone regardless, but tune_active/
                                            // two_tone_active/pre_tune_power_watts live on
                                            // ConnectedState and would otherwise survive a
                                            // disarm -- restore the real TX Power rather than
                                            // leaving it at whatever reduced tune/two-tone
                                            // wattage happened to be active.
                                            if let Some(prev) = connected.pre_tune_power_watts.take() {
                                                connected.session.tx_power_watts.store(prev, Ordering::Relaxed);
                                            }
                                            connected.tune_active = false;
                                            connected.two_tone_active = false;
                                        }
                                        settings_changed = true;
                                    }

                                    // TX audio source selection -- see
                                    // radio::RadioSession::tx_audio_source's
                                    // doc comment. Auto (existing TCI-
                                    // preferred-with-local-mic-fallback
                                    // behavior) by default.
                                    ui.add_space(8.0);
                                    ui.label("TX audio source:");
                                    let current_source =
                                        connected.session.tx_audio_source.load(Ordering::Relaxed);
                                    ui.horizontal(|ui| {
                                        for (value, label) in [
                                            (TX_AUDIO_SOURCE_AUTO, "Auto"),
                                            (TX_AUDIO_SOURCE_RADIO_MIC, "Radio Mic"),
                                            (TX_AUDIO_SOURCE_LOCAL_MIC, "Local Mic (ignore TCI audio)"),
                                        ] {
                                            if ui.selectable_label(current_source == value, label).clicked()
                                                && current_source != value
                                            {
                                                connected.session.tx_audio_source.store(value, Ordering::Relaxed);
                                                settings_changed = true;
                                            }
                                        }
                                    });
                                    ui.weak(match current_source {
                                        TX_AUDIO_SOURCE_RADIO_MIC => {
                                            "Audio from the radio's own mic jack replaces the local \
                                             PC mic (and TCI audio) as the TX source."
                                        }
                                        TX_AUDIO_SOURCE_LOCAL_MIC => {
                                            "Local PC mic audio is used for TX regardless of TCI, even \
                                             while a TCI client is actively sending its own audio -- \
                                             useful for a TCI client (e.g. WSJT-X) with a known-bad TCI \
                                             audio path, routing its own audio output back via the \
                                             system's local mic input (e.g. pipewire) instead while \
                                             TCI still drives frequency/mode/PTT."
                                        }
                                        _ => {
                                            "TCI-sourced audio is used whenever a TCI client is \
                                             actively sending it, falling back to the local PC mic \
                                             otherwise."
                                        }
                                    });

                                    // TX audio monitor -- see TxHandle::tx_audio_monitor's doc
                                    // comment. Added while diagnosing a real report of TCI-sourced
                                    // TX audio producing splatter/no-decode: lets the user hear
                                    // exactly what's reaching WDSP, to tell "already wrong in the
                                    // source audio" apart from "introduced downstream".
                                    if let Some(tx) = &connected.tx_handle {
                                        ui.add_space(8.0);
                                        let mut monitoring = connected.tx_audio_monitor_output.is_some();
                                        if ui.checkbox(&mut monitoring, "Monitor TX Audio").changed() {
                                            if monitoring {
                                                // Always the system default (None) -- see
                                                // ConnectedState::audio_output_device's doc comment on
                                                // why this doesn't follow the RX output device
                                                // selection.
                                                match AudioOutput::start(Arc::clone(&tx.tx_audio_monitor), None) {
                                                    Ok(out) => connected.tx_audio_monitor_output = Some(out),
                                                    Err(e) => eprintln!("tx audio monitor unavailable: {e}"),
                                                }
                                            } else {
                                                connected.tx_audio_monitor_output = None;
                                            }
                                        }
                                        ui.weak(
                                            "Plays the exact audio being fed to WDSP (post source \
                                             selection, pre-processing) through the local speaker/ \
                                             headphones -- useful for telling whether a TX audio \
                                             problem is already present in the source (mic/TCI) or \
                                             introduced downstream.",
                                        );
                                    }

                                    // Radio mic connector config (PTT enable, tip/ring wiring,
                                    // bias) -- standard Angelia/Orion/Orion2 boards only, matching
                                    // piHPSDR's own UI gating (radio_menu.c) for the same controls.
                                    // See radio::RadioSession::mic_ptt_enabled/mic_bias_enabled/
                                    // mic_ptt_on_tip's doc comments for the exact wire encoding.
                                    if matches!(
                                        connected.device.board,
                                        Boards::Angelia | Boards::Orion | Boards::Orion2
                                    ) {
                                        ui.add_space(8.0);
                                        ui.separator();
                                        ui.label("Radio Mic Connector:");

                                        let mut ptt_on_tip =
                                            connected.session.mic_ptt_on_tip.load(Ordering::Relaxed);
                                        ui.horizontal(|ui| {
                                            if ui
                                                .add(egui::Button::selectable(
                                                    !ptt_on_tip,
                                                    "PTT on Ring, Mic/Bias on Tip",
                                                ))
                                                .clicked()
                                                && ptt_on_tip
                                            {
                                                ptt_on_tip = false;
                                                connected.session.mic_ptt_on_tip.store(false, Ordering::Relaxed);
                                                settings_changed = true;
                                            }
                                            if ui
                                                .add(egui::Button::selectable(
                                                    ptt_on_tip,
                                                    "PTT on Tip, Mic/Bias on Ring",
                                                ))
                                                .clicked()
                                                && !ptt_on_tip
                                            {
                                                connected.session.mic_ptt_on_tip.store(true, Ordering::Relaxed);
                                                settings_changed = true;
                                            }
                                        });

                                        let mut mic_ptt_enabled =
                                            connected.session.mic_ptt_enabled.load(Ordering::Relaxed);
                                        if ui.checkbox(&mut mic_ptt_enabled, "Mic PTT Enabled").changed() {
                                            connected
                                                .session
                                                .mic_ptt_enabled
                                                .store(mic_ptt_enabled, Ordering::Relaxed);
                                            settings_changed = true;
                                        }

                                        let mut mic_bias_enabled =
                                            connected.session.mic_bias_enabled.load(Ordering::Relaxed);
                                        if ui.checkbox(&mut mic_bias_enabled, "Mic Bias Enabled").changed() {
                                            connected
                                                .session
                                                .mic_bias_enabled
                                                .store(mic_bias_enabled, Ordering::Relaxed);
                                            settings_changed = true;
                                        }
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
                                    // Mutually exclusive with Diversity -- both reserve
                                    // wire indices at fixed positions that would collide
                                    // (see radio::RadioSession::diversity_enabled's doc
                                    // comment). Disabled, not hidden, so it's clear why.
                                    ui.add_enabled_ui(!connected.diversity_enabled, |ui| {
                                        if ui
                                            .checkbox(&mut puresignal_enabled, "Enable PureSignal")
                                            .changed()
                                        {
                                            connected.puresignal_enabled = puresignal_enabled;
                                            settings_changed = true;
                                            // Live toggle -- see RadioSession::
                                            // puresignal_enabled's doc comment (radio.rs).
                                            // Both calls needed: the radio-side wire
                                            // flag (session) and the TX-chain WDSP
                                            // engine flag (tx_handle) are independent
                                            // live flags that both need to move together.
                                            connected.session.set_puresignal_enabled(puresignal_enabled);
                                            if let Some(tx) = &connected.tx_handle {
                                                tx.set_puresignal_enabled(puresignal_enabled);
                                            }
                                        }
                                    });
                                    if connected.diversity_enabled {
                                        ui.weak("Disabled while Diversity is enabled (Settings -> Diversity).");
                                    }
                                    ui.weak(
                                        "Instant -- no reconnect needed. This radio/board \
                                         permanently reserves 2 feedback receivers for \
                                         PureSignal (reducing \"Add Receiver\" capacity by 2) \
                                         whether or not it's currently enabled, so toggling \
                                         here can't drop your rigctl/TCI connections.",
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

                                            let mut ps_oneshot = connected.ps_oneshot;
                                            if ui.checkbox(&mut ps_oneshot, "OneShot").changed() {
                                                connected.ps_oneshot = ps_oneshot;
                                                tx.set_ps_oneshot(ps_oneshot);
                                                settings_changed = true;
                                            }
                                            ui.weak(
                                                "Calibrate with Two Tone (envelope-rich) first, then \
                                                 enable OneShot before running constant-envelope digital \
                                                 modes (FT8 etc.) -- their TX envelope can't sweep the \
                                                 full amplitude range a correction table needs to keep \
                                                 relearning from, so Running above will never settle on \
                                                 that traffic. OneShot just applies the last good table \
                                                 instead of continuing to try.",
                                            );

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

                                            // Standard (non-HermesLite) boards only, both protocols
                                            // -- see radio::RadioSession::ps_tx_attenuation's doc
                                            // comment. This, not HW Peak, is the real per-session
                                            // tuning knob for bringing Feedback level above into
                                            // the ideal 128-181 range -- confirmed against
                                            // piHPSDR's own "Auto Attenuate" logic, which adjusts
                                            // exactly this value (not HW Peak) to target a
                                            // feedback level near 152.
                                            if !matches!(
                                                connected.device.board,
                                                Boards::HermesLite | Boards::HermesLite2
                                            ) {
                                                let mut ps_atten = connected
                                                    .session
                                                    .ps_tx_attenuation
                                                    .load(Ordering::Relaxed)
                                                    as i32;
                                                ui.horizontal(|ui| {
                                                    ui.label("Feedback Attenuation:");
                                                    if scroll_slider_i32(
                                                        ui,
                                                        &mut connected.slider_scroll_accum,
                                                        &mut ps_atten,
                                                        0..=31,
                                                        1,
                                                        " dB",
                                                    ) {
                                                        connected
                                                            .session
                                                            .ps_tx_attenuation
                                                            .store(ps_atten as u32, Ordering::Relaxed);
                                                        settings_changed = true;
                                                    }
                                                });
                                                ui.weak(
                                                    "Raise this if Feedback level above reads too high \
                                                     (near/over 256) -- target the 128-181 range.",
                                                );
                                            }

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
                                            ui.horizontal(|ui| {
                                                ui.label("Ptol:");
                                                let mut ptol = connected.ps_ptol;
                                                if scroll_slider_f64(
                                                    ui,
                                                    &mut connected.slider_scroll_accum,
                                                    &mut ptol,
                                                    0.0..=1.0,
                                                    0.01,
                                                    "",
                                                ) {
                                                    connected.ps_ptol = ptol;
                                                    tx.set_ps_ptol(ptol);
                                                    settings_changed = true;
                                                }
                                            });
                                            ui.weak(
                                                "Correction-table outlier tolerance -- lower this if \
                                                 Correcting never turns on despite Feedback level \
                                                 looking reasonable and Calibrate Now running \
                                                 repeatedly (WDSP default 0.8).",
                                            );
                                            ui.weak("Advanced -- rarely need changing from the defaults.");
                                        } else {
                                            ui.weak("TX must be enabled for PureSignal calibration controls.");
                                        }
                                    }
                                }
                                SettingsTab::Diversity => {
                                    // Ported from piHPSDR's own diversity feature
                                    // (diversity_menu.c/receiver.c, which the user
                                    // originally wrote) -- see radio::RadioSession::
                                    // diversity_enabled/diversity_gain_db/
                                    // diversity_phase_deg's doc comments for the
                                    // combining formula.
                                    ui.add_space(4.0);
                                    let mut diversity_enabled = connected.diversity_enabled;
                                    // Mutually exclusive with PureSignal -- see that
                                    // tab's own checkbox handler for why.
                                    ui.add_enabled_ui(!connected.puresignal_enabled, |ui| {
                                        if ui
                                            .checkbox(&mut diversity_enabled, "Enable Diversity")
                                            .changed()
                                        {
                                            connected.diversity_enabled = diversity_enabled;
                                            settings_changed = true;
                                            // A true live toggle on both protocols -- see
                                            // RadioSession::set_diversity_enabled's doc
                                            // comment for why this project moved away from
                                            // a full reconnect here (confirmed via
                                            // extensive real-hardware testing that it
                                            // reliably hung this board's P1 firmware; P2
                                            // never needed a reconnect for this in the
                                            // first place -- it just continuously sends
                                            // updated config packets on a timer, no
                                            // discrete preconfig/Start handshake to redo).
                                            connected.session.set_diversity_enabled(diversity_enabled);
                                        }
                                    });
                                    if connected.puresignal_enabled {
                                        ui.weak("Disabled while PureSignal is enabled (Settings -> PureSignal).");
                                    }
                                    ui.weak(
                                        "Combines ADC1's IQ into ADC0's before demodulation to help \
                                         null multipath fades/local noise that hit each antenna \
                                         differently -- reserves ADC1 as a hidden second receiver.",
                                    );
                                    ui.weak("Live -- takes effect immediately, no reconnect.");

                                    if connected.session.diversity_enabled.load(Ordering::Relaxed) {
                                        ui.add_space(6.0);
                                        let mut gain_db = f32::from_bits(
                                            connected.session.diversity_gain_db.load(Ordering::Relaxed),
                                        );
                                        if ui
                                            .add(
                                                egui::Slider::new(&mut gain_db, -27.0..=27.0)
                                                    .text("Gain")
                                                    .suffix(" dB"),
                                            )
                                            .changed()
                                        {
                                            connected
                                                .session
                                                .diversity_gain_db
                                                .store(gain_db.to_bits(), Ordering::Relaxed);
                                            settings_changed = true;
                                        }
                                        let mut phase_deg = f32::from_bits(
                                            connected.session.diversity_phase_deg.load(Ordering::Relaxed),
                                        );
                                        if ui
                                            .add(
                                                egui::Slider::new(&mut phase_deg, -180.0..=180.0)
                                                    .text("Phase")
                                                    .suffix("\u{b0}"),
                                            )
                                            .changed()
                                        {
                                            connected
                                                .session
                                                .diversity_phase_deg
                                                .store(phase_deg.to_bits(), Ordering::Relaxed);
                                            settings_changed = true;
                                        }
                                        ui.weak(
                                            "Tune by ear/S-meter for the best null or peak -- live, no \
                                             reconnect needed.",
                                        );
                                    }
                                }
                                SettingsTab::Equalizer => {
                                    // See spectrum::EqualizerParams's doc comment --
                                    // WDSP's own two graphic-EQ layouts (3-band legacy /
                                    // 10-band), ported from piHPSDR's equalizer_menu.c
                                    // (which the user originally wrote, 3-band only --
                                    // 10-band added here per explicit request).
                                    if connected.tx_handle.is_some() {
                                        ui.horizontal(|ui| {
                                            for (is_tx, label) in [(false, "RX"), (true, "TX")] {
                                                if ui
                                                    .selectable_label(connected.eq_tab_is_tx == is_tx, label)
                                                    .clicked()
                                                {
                                                    connected.eq_tab_is_tx = is_tx;
                                                }
                                            }
                                        });
                                        ui.separator();
                                    }
                                    if connected.eq_tab_is_tx && connected.tx_handle.is_some() {
                                        let tx = connected.tx_handle.as_ref().unwrap();
                                        let mut eq = tx.eq();
                                        if render_equalizer_panel(
                                            ui,
                                            &mut connected.slider_scroll_accum,
                                            "TX",
                                            &mut eq,
                                        ) {
                                            tx.set_eq(eq);
                                            settings_changed = true;
                                        }
                                    } else {
                                        let mut eq = connected.spectrum.eq();
                                        if render_equalizer_panel(
                                            ui,
                                            &mut connected.slider_scroll_accum,
                                            "RX",
                                            &mut eq,
                                        ) {
                                            connected.spectrum.set_eq(eq);
                                            settings_changed = true;
                                        }
                                    }
                                }
                            }

                            if let Some(fw) = &mut connected.firmware_update {
                                fw.show(ui);
                                if !fw.open {
                                    connected.firmware_update = None;
                                }
                            }
                            // In-app firmware update (P2) needs this radio
                            // genuinely idle to actually erase anything --
                            // a real report confirmed it just echoes a
                            // generic busy-status reply instead while
                            // we're actively streaming it. RadioSession::stop
                            // sends the real "run bit cleared" stop command
                            // (same as clicking the main Stop button), which
                            // is what should make the radio treat itself as
                            // idle/available again. Deliberately done here
                            // (not inside bootloader_ui.rs, which has no
                            // access to the live RadioSession) right after
                            // confirming Erase && Program, rather than
                            // fully disconnecting/returning to Discovery --
                            // simpler than handing the window off across an
                            // AppState transition, at the cost of this
                            // radio's Connected view going stale/frozen for
                            // the rest of the update (acceptable for a rare,
                            // deliberate action like this).
                            if connected.firmware_update.as_ref().is_some_and(|fw| fw.has_pending_inapp_start()) {
                                connected.session.stop();
                                connected.firmware_update.as_mut().unwrap().begin_pending_inapp_upload();
                            }
                            if connected.firmware_update.as_ref().is_some_and(|fw| fw.finished_in_app_upload()) {
                                restart_after_firmware_update = Some(connected.device);
                            }
                        });
                        },
                    );
                    if close_requested {
                        connected.show_settings_window = false;
                    }
                }

                // root_close_requested/stop_clicked (computed earlier
                // this frame -- see their own declarations) also force a
                // save here rather than relying on settings_dirty alone,
                // since moving/resizing a window doesn't set that flag
                // but should still be persisted before the app exits or
                // this radio is disconnected (see main_window_geometry's
                // doc comment).
                if settings_changed
                    || connected.settings_dirty.swap(false, std::sync::atomic::Ordering::Relaxed)
                    || root_close_requested
                    || stop_clicked
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
                                audio_output_device: rx.audio_output_device.clone(),
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
                                eq: agc_params.eq,
                                window_geometry: rx.window_geometry,
                                ctun: rx.ctun,
                                ctun_frequency_hz: rx.ctun_frequency_hz,
                                spectrum_zoom: rx.spectrum_zoom,
                                spectrum_pan: rx.spectrum_pan,
                                db_low_auto: rx.db_low_auto,
                                rit_enabled: rx.rit_enabled,
                                rit_offset_hz: rx.rit_offset_hz,
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
                        audio_output_device: connected.audio_output_device.clone(),
                        mic_input_device: connected.mic_input_device.clone(),
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
                        rx_eq: Some(agc_params_now.eq),
                        mic_gain: Some(connected.mic_gain),
                        tx_eq: connected.tx_handle.as_ref().map(|t| t.eq()),
                        tci_tx_gain: Some(connected.tci_tx_gain),
                        tx_power_watts: Some(connected.session.tx_power_watts.load(Ordering::Relaxed)),
                        db_low: Some(connected.db_low),
                        db_low_auto: Some(connected.db_low_auto),
                        db_high: Some(connected.db_high),
                        waterfall_db_low: Some(connected.waterfall_db_low),
                        waterfall_db_high: Some(connected.waterfall_db_high),
                        tx_db_low: Some(connected.tx_db_low),
                        tx_db_high: Some(connected.tx_db_high),
                        tx_waterfall_db_low: Some(connected.tx_waterfall_db_low),
                        tx_waterfall_db_high: Some(connected.tx_waterfall_db_high),
                        waterfall_palette: Some(connected.waterfall_palette),
                        spectrum_waterfall_ratio: Some(connected.spectrum_waterfall_ratio),
                        spectrum_zoom: Some(connected.spectrum_zoom),
                        spectrum_pan: Some(connected.spectrum_pan),
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
                        cat_addr: Some(connected.cat_addr.clone()),
                        rigctl_running: Some(connected.rigctl_server.is_some()),
                        tci_running: Some(connected.tci_server.is_some()),
                        cat_running: Some(connected.cat_server.is_some()),
                        rigctl_logging_enabled: Some(connected.rigctl_debug_log.is_enabled()),
                        tci_logging_enabled: Some(connected.tci_debug_log.is_enabled()),
                        cat_logging_enabled: Some(connected.cat_debug_log.is_enabled()),
                        extra_receivers,
                        puresignal_enabled: Some(connected.puresignal_enabled),
                        diversity_enabled: Some(connected.diversity_enabled),
                        diversity_gain_db: Some(f32::from_bits(
                            connected.session.diversity_gain_db.load(std::sync::atomic::Ordering::Relaxed),
                        )),
                        diversity_phase_deg: Some(f32::from_bits(
                            connected.session.diversity_phase_deg.load(std::sync::atomic::Ordering::Relaxed),
                        )),
                        rx_attenuation: Some(
                            connected.session.rx_attenuation.load(std::sync::atomic::Ordering::Relaxed),
                        ),
                        ps_tx_attenuation: Some(
                            connected.session.ps_tx_attenuation.load(std::sync::atomic::Ordering::Relaxed),
                        ),
                        ps_hw_peak: Some(connected.ps_hw_peak),
                        ps_mox_delay: Some(connected.ps_mox_delay),
                        ps_loop_delay: Some(connected.ps_loop_delay),
                        ps_tx_delay_ns: Some(connected.ps_tx_delay_ns),
                        ps_ptol: Some(connected.ps_ptol),
                        send_rx_audio_to_radio: Some(
                            connected
                                .session
                                .send_rx_audio_to_radio
                                .load(std::sync::atomic::Ordering::Relaxed),
                        ),
                        tx_audio_source: Some(
                            connected.session.tx_audio_source.load(std::sync::atomic::Ordering::Relaxed),
                        ),
                        mic_ptt_enabled: Some(
                            connected.session.mic_ptt_enabled.load(std::sync::atomic::Ordering::Relaxed),
                        ),
                        mic_bias_enabled: Some(
                            connected.session.mic_bias_enabled.load(std::sync::atomic::Ordering::Relaxed),
                        ),
                        mic_ptt_on_tip: Some(
                            connected.session.mic_ptt_on_tip.load(std::sync::atomic::Ordering::Relaxed),
                        ),
                        window_geometry: self.main_window_geometry,
                        ctun: Some(connected.ctun),
                        ctun_frequency_hz: Some(connected.ctun_frequency_hz),
                        vfo_b_frequency_hz: Some(connected.vfo_b_frequency_hz),
                        split: Some(connected.split),
                        rit_enabled: Some(connected.rit_enabled),
                        rit_offset_hz: Some(connected.rit_offset_hz),
                        xit_enabled: Some(connected.xit_enabled),
                        xit_offset_hz: Some(connected.xit_offset_hz),
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
                } else if let Some(device) = restart_after_firmware_update {
                    // Reconnects automatically once an in-app firmware
                    // update finishes, loading the same saved Config a
                    // manual Stop-then-rediscover-then-Start would have --
                    // this is exactly that flow automated, not a shortcut
                    // that skips anything.
                    //
                    // BUG FIX: this used to call connect_to_device directly
                    // here and only THEN assign the result to self.state,
                    // which -- since Rust evaluates the right-hand side of
                    // an assignment before dropping the old value being
                    // replaced -- builds the entire new ConnectedState
                    // while the OLD one (this `connected` binding) was
                    // still alive. connected.session.stop() above only
                    // covers the radio session itself (stopped earlier to
                    // let the update run at all -- see
                    // has_pending_inapp_start's doc comment in
                    // bootloader_ui.rs); connected.spectrum/tx_spectrum
                    // and every extra receiver's own SpectrumHandle still
                    // each held their own WDSP channel open by index, and
                    // connect_to_device immediately tries to reopen those
                    // same channel numbers -- exactly the double-open
                    // hazard change_sample_rate's own doc comment already
                    // warns about ("WDSP isn't confirmed thread-safe for
                    // concurrent access to the same channel"), confirmed
                    // as the real cause via a real segfault report. Fixed
                    // by dropping the whole old ConnectedState FIRST (via
                    // an intermediate Discovering state, same as
                    // stop_clicked above -- its Drop impls close every
                    // WDSP channel/audio device/socket correctly) before
                    // constructing the replacement, rather than trying to
                    // hand-replicate that teardown field-by-field here.
                    let ctx = ui.ctx().clone();
                    self.state = AppState::Discovering(DiscoveryWindow::new(&ctx));
                    // Re-query the radio rather than reusing `device` as-is
                    // -- it's a snapshot from BEFORE the update, so its
                    // `version` byte (shown in the title bar, see
                    // base_title above) would otherwise keep showing the
                    // old firmware version after a successful update. Only
                    // the version realistically changes here (board/MAC/
                    // protocol don't change from a firmware flash), but a
                    // fresh discovery reply is simpler and more honest than
                    // patching just that one field. Falls back to the
                    // stale `device` if the radio doesn't answer yet (e.g.
                    // still finishing its own reboot) so reconnecting still
                    // succeeds either way -- see manual_discovery's own
                    // short (250ms) timeout.
                    let discovered = Arc::new(Mutex::new(Vec::new()));
                    manual_discovery(Arc::clone(&discovered), device.address.ip());
                    let device = discovered.lock().unwrap().first().copied().unwrap_or(device);
                    let cfg = Config::load(device.mac);
                    self.state = match connect_to_device(device, &cfg) {
                        Ok(new_connected) => AppState::Connected(new_connected),
                        Err(e) => AppState::Error(e),
                    };
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

/// "Auto" Low tuning -- see ConnectedState::db_low_auto's doc comment.
/// Bins within the excluded edge (max of 1/20th of the trace width and
/// this minimum count) are skipped when finding the trace's minimum,
/// since WDSP's analyzer can show rolloff/artifacts right at the edges
/// of the visible span that would otherwise drag the tracked floor down
/// to somewhere unrepresentative of the real noise floor.
const AUTO_DB_LOW_EDGE_EXCLUDE_FRACTION: usize = 20;
const AUTO_DB_LOW_MIN_EDGE_EXCLUDE: usize = 4;
/// Per-frame smoothing factor for db_low_auto_smoothed -- same
/// ballistics pattern as the TX power meter's own SMOOTHING_ALPHA,
/// just slower (a noise floor should drift, not visibly jump).
const AUTO_DB_LOW_SMOOTHING_ALPHA: f32 = 0.03;

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

/// How many trailing samples draw_audio_waveform reads each frame --
/// ~500ms at 48kHz. A real report: an earlier, shorter ~200ms window
/// felt too fast/frantic (nearly all of it fresh content every frame);
/// this shows more history per frame so it reads as calmer. Comfortably
/// inside both source taps' own ~500ms capacity (spectrum.rs's
/// WAVEFORM_TAP_CAPACITY / tx.rs's WAVEFORM_TAP_CAPACITY -- each
/// dedicated to this display alone, not shared with the smaller
/// latency-sensitive playback/monitor buffers), so there's always a
/// full window's worth available rather than reading right up against
/// the producer.
const WAVEFORM_WINDOW_SAMPLES: usize = 24_000;

/// Read-only snapshot of the most recent `max_samples` values in `buf`,
/// oldest first. Never pops -- `buf` is a tap fed independently of
/// whatever else might be consuming it (or, for the waveform taps
/// specifically, fed to nobody else at all), so peeking here can't
/// steal samples from real audio playback/TX modulation.
fn peek_recent_samples(buf: &Arc<Mutex<VecDeque<f32>>>, max_samples: usize) -> Vec<f32> {
    let b = buf.lock().unwrap();
    let skip = b.len().saturating_sub(max_samples);
    b.iter().skip(skip).copied().collect()
}

/// Small audio-waveform display drawn in the top-right corner of the
/// spectrum plot `rect` -- output audio while receiving, whatever's
/// actually feeding TX (mic/TCI/radio-mic) while transmitting. A quick
/// visual check that audio is actually flowing and roughly what level
/// it's at, without needing an external scope.
///
/// `samples` is recent history in chronological order (oldest first).
/// Drawn as a per-pixel-column RMS envelope rather than a plain
/// connect-the-dots line (since `samples` normally holds far more points
/// than the panel is wide, that would just alias) or raw min/max peaks
/// (tried first -- looked like a solid filled block for any continuous
/// voice/mic audio, since a peak trace touches close to full-scale on
/// nearly every column once each column spans more than about one pitch
/// period; see the per-column comment below for the full reasoning).
fn draw_audio_waveform(painter: &egui::Painter, rect: egui::Rect, samples: &[f32]) {
    const MARGIN: f32 = 8.0;
    const WIDTH: f32 = 160.0;
    const HEIGHT: f32 = 50.0;
    let panel = egui::Rect::from_min_size(
        egui::pos2(rect.right() - MARGIN - WIDTH, rect.top() + MARGIN),
        egui::vec2(WIDTH, HEIGHT),
    );
    painter.rect_filled(panel, 3.0, egui::Color32::from_rgba_unmultiplied(20, 20, 20, 220));

    let mid_y = panel.center().y;
    if samples.len() < 2 {
        painter.line_segment(
            [egui::pos2(panel.left(), mid_y), egui::pos2(panel.right(), mid_y)],
            egui::Stroke::new(1.0, egui::Color32::from_gray(80)),
        );
        return;
    }

    // Auto-scale to the loudest sample in this window, like a scope on
    // auto-range, floored so near-silence doesn't get amplified into
    // looking like a full-scale signal. Without this, RX audio (whose
    // linear amplitude at a normal listening volume is typically well
    // under the raw +-1.0 range) looked like a flat line, and TX audio
    // (much closer to true full-scale already) looked permanently
    // clipped/filled -- confirmed by a real report of exactly that on
    // both sides. Also makes this robust to the two taps turning out to
    // carry genuinely different absolute scales, since normalizing to
    // each window's own peak maps whichever value is loudest to the
    // panel's full height regardless of the raw units underneath.
    const SILENCE_FLOOR: f32 = 0.05;
    let peak = samples.iter().fold(SILENCE_FLOOR, |acc, &s| acc.max(s.abs()));
    let norm = 1.0 / peak;

    let cols = panel.width().round().max(1.0) as usize;
    let half_height = panel.height() / 2.0 - 2.0;
    let samples_per_col = samples.len() as f32 / cols as f32;
    for col in 0..cols {
        let start = ((col as f32 * samples_per_col) as usize).min(samples.len());
        let end = (((col + 1) as f32 * samples_per_col) as usize).min(samples.len());
        if start >= end {
            continue;
        }
        let slice = &samples[start..end];
        // RMS per column, not min/max peak -- a real report: with each
        // column now spanning ~3ms of continuous voice/mic audio (up
        // from the shorter window before "slow it down"), a peak trace
        // reached close to this window's own normalized max on nearly
        // every column (voiced speech rarely goes a full pitch period
        // without a swing that wide), rendering as a solid filled block
        // rather than a readable waveform. RMS instead tracks the
        // column's loudness envelope, which varies meaningfully with
        // syllables/level even when the instantaneous peak doesn't, and
        // is inherently below the window's peak (a sine wave's RMS is
        // ~0.707x its peak, real speech usually further below that), so
        // it naturally leaves headroom instead of pinning to the edges.
        let sum_sq: f32 = slice.iter().map(|&s| { let s = s * norm; s * s }).sum();
        let rms = (sum_sq / slice.len() as f32).sqrt().min(1.0);
        let x = panel.left() + col as f32 + 0.5;
        painter.line_segment(
            [egui::pos2(x, mid_y - rms * half_height), egui::pos2(x, mid_y + rms * half_height)],
            egui::Stroke::new(1.0, egui::Color32::from_rgb(80, 220, 160)),
        );
    }
}

/// Vertical orange markers on the spectrum plot at any amateur band edge
/// (BANDS' low_hz/high_hz) that falls within the currently visible span
/// -- e.g. tuned near the top of 40m shows a line at 7.300MHz, or near a
/// WARC band shows both its edges. Deliberately spectrum-only (not the
/// waterfall) and drawn early, right after the black background, so the
/// spectrum trace/dial line/passband overlay all stay visually on top
/// of it rather than the reverse. Recomputes the same offset-from-dial-
/// frequency-to-x mapping the caller builds separately (as `x_for_offset`)
/// for its own passband overlay/axis ticks, rather than threading that
/// closure through -- this runs before `x_for_offset` even exists at
/// either call site.
/// `center_hz`/`half_span_hz` describe the currently VISIBLE window
/// (after zoom/pan), not necessarily the full captured sample-rate span
/// -- see the caller's own visible_half_span_hz/pan_offset_hz for how
/// that's derived.
fn draw_band_edge_markers(painter: &egui::Painter, rect: egui::Rect, center_hz: f64, half_span_hz: f64) {
    const BAND_EDGE_COLOR: egui::Color32 = egui::Color32::from_rgb(255, 140, 0);
    for band in &BANDS {
        for edge_hz in [band.low_hz, band.high_hz] {
            let offset_hz = edge_hz as f64 - center_hz;
            if offset_hz.abs() > half_span_hz {
                continue;
            }
            let frac = ((offset_hz + half_span_hz) / (2.0 * half_span_hz)) as f32;
            let x = rect.left() + frac * rect.width();
            painter.line_segment(
                [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
                egui::Stroke::new(1.0, BAND_EDGE_COLOR),
            );
        }
    }
}

/// Status color for the rigctl/TCI indicators in the main panel:
/// gray = not running, green = listening but idle, red = a client is
/// actively connected. `None` means the server isn't running at all
/// (start/stop is manual now, from Settings -> Network); `Some(bool)`
/// is whether a client is currently connected.
/// Renders a "Start"/"Stop" button (Settings -> Network's rigctl/TCI/CAT
/// rows) with a colored background matching network_status_color's own
/// green/red convention below, so the button's action is visible at a
/// glance rather than needing to read its label. Returns whether it was
/// clicked this frame, same as `ui.button(..).clicked()`.
fn start_stop_button(ui: &mut egui::Ui, running: bool) -> bool {
    let (label, color) = if running {
        ("Stop", egui::Color32::from_rgb(220, 60, 60))
    } else {
        ("Start", egui::Color32::from_rgb(50, 160, 50))
    };
    ui.add(egui::Button::new(egui::RichText::new(label).color(egui::Color32::WHITE)).fill(color)).clicked()
}

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

/// Displays and drags in dB for a gain control whose useful range spans
/// orders of magnitude (e.g. Audio Gain, Mic Gain, TCI TX Gain) -- `gain`
/// is still the actual linear amplitude value mutated in place (this
/// app's gain fields are a plain `sample * gain` multiply, see
/// SpectrumHandle::set_gain's doc comment), so nothing downstream of the
/// slider needs to change, only how the UI reads. `min_db` doubles as
/// the effective floor/mute point -- 0.0 linear gain is -infinity dB,
/// not representable, so dragging/scrolling to the bottom of the range
/// lands on `min_db` rather than true silence.
fn scroll_slider_f32_db(
    ui: &mut egui::Ui,
    scroll_accum: &mut f32,
    gain: &mut f32,
    min_db: f32,
    max_db: f32,
    step_db: f32,
) -> bool {
    let mut db = if *gain > 0.0 { 20.0 * gain.log10() } else { min_db };
    let resp = ui.add(egui::Slider::new(&mut db, min_db..=max_db).suffix(" dB"));
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
                db = (db + step_db * sign).clamp(min_db, max_db);
                changed = true;
            }
        }
    }
    if changed {
        *gain = 10f32.powf(db / 20.0);
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

/// Shared RX/TX graphic-EQ panel -- see spectrum::EqualizerParams's doc
/// comment for the two band layouts. `side_label` is just the checkbox
/// wording ("RX"/"TX"); range/step (-12..15dB, step 1) matches piHPSDR's
/// own equalizer_menu.c exactly. Mutates `eq` in place and returns
/// whether anything changed, so every call site can decide how to push
/// the result back (SpectrumHandle::set_eq / TxHandle::set_eq) and mark
/// its own dirty flag -- same "mutate a local copy, write back on
/// change" shape as the rest of this file's Settings panels.
fn render_equalizer_panel(ui: &mut egui::Ui, scroll_accum: &mut f32, side_label: &str, eq: &mut spectrum::EqualizerParams) -> bool {
    let mut changed = false;
    if ui.checkbox(&mut eq.enabled, format!("Enable {side_label} Equalizer")).changed() {
        changed = true;
    }
    ui.horizontal(|ui| {
        ui.label("Bands:");
        if ui.add(egui::Button::selectable(eq.band_count == spectrum::EqBandCount::Three, "3-Band")).clicked()
            && eq.band_count != spectrum::EqBandCount::Three
        {
            eq.band_count = spectrum::EqBandCount::Three;
            changed = true;
        }
        if ui.add(egui::Button::selectable(eq.band_count == spectrum::EqBandCount::Ten, "10-Band")).clicked()
            && eq.band_count != spectrum::EqBandCount::Ten
        {
            eq.band_count = spectrum::EqBandCount::Ten;
            changed = true;
        }
    });
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.label("Preamp:");
        if scroll_slider_i32(ui, scroll_accum, &mut eq.preamp_db, -12..=15, 1, " dB") {
            changed = true;
        }
    });
    match eq.band_count {
        spectrum::EqBandCount::Three => {
            const LABELS: [&str; 3] = ["Low", "Mid", "High"];
            for (label, gain) in LABELS.iter().zip(eq.bands_3_db.iter_mut()) {
                ui.horizontal(|ui| {
                    ui.label(format!("{label}:"));
                    if scroll_slider_i32(ui, scroll_accum, gain, -12..=15, 1, " dB") {
                        changed = true;
                    }
                });
            }
        }
        spectrum::EqBandCount::Ten => {
            const LABELS: [&str; 10] =
                ["32Hz", "63Hz", "125Hz", "250Hz", "500Hz", "1kHz", "2kHz", "4kHz", "8kHz", "16kHz"];
            for (label, gain) in LABELS.iter().zip(eq.bands_10_db.iter_mut()) {
                ui.horizontal(|ui| {
                    ui.label(format!("{label}:"));
                    if scroll_slider_i32(ui, scroll_accum, gain, -12..=15, 1, " dB") {
                        changed = true;
                    }
                });
            }
        }
    }
    changed
}

/// PureSignal HW Peak's confirmed reference default, per protocol --
/// see tx::PsParams::hw_peak's doc comment for what this represents.
/// BUG FIX: this used to be hardcoded to the P2 value (0.2899)
/// regardless of protocol -- confirmed via real hardware testing
/// (ANAN-100D/Angelia, Protocol 1) that this miscalibration pins
/// GetPSInfo's reported feedback level at its maximum display value
/// (255) REGARDLESS of actual drive level (raising or lowering TX
/// power made no difference at all, which is the signature of a
/// scaling/reference-point error, not a genuinely-too-strong signal --
/// a real overload would track drive level, not sit pinned at a
/// constant). Confirmed Thetis defaults: P1/USB 0.4072, P2 0.2899.
fn default_ps_hw_peak(protocol: u8) -> f64 {
    if protocol == 1 {
        0.4072
    } else {
        0.2899
    }
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
    // CTUN: same behavior as the main receiver -- see
    // ConnectedState::ctun's doc comment. Pushed to the analyzer thread
    // every frame regardless of change, same reasoning as the main
    // receiver's own per-frame resync. Computed before Zoom/Pan below so
    // zooming can keep the CTUN'd listen frequency centered.
    let ctun_offset_hz = if rx.ctun { rx.ctun_frequency_hz as f64 - freq_hz as f64 } else { 0.0 };
    // Zoom/Pan (sliders below the waterfall) -- see the main receiver's
    // identical computation for the full reasoning, including why the CTUN
    // offset has to be folded in here (for WDSP's own fscLin/fscHin
    // clipping) rather than just in axis-label math.
    let half_span_hz = sample_rate as f64 / 2.0;
    let visible_half_span_hz = half_span_hz / rx.spectrum_zoom as f64;
    let max_pan_hz = half_span_hz - visible_half_span_hz;
    let pan_offset_hz = (ctun_offset_hz + rx.spectrum_pan as f64 * max_pan_hz)
        .clamp(-max_pan_hz, max_pan_hz);
    let effective_pan = if max_pan_hz > 0.0 { (pan_offset_hz / max_pan_hz) as f32 } else { 0.0 };
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

    // "Auto" Low -- see ConnectedState::db_low_auto's doc comment (no TX
    // case to gate on here -- extra receivers never transmit).
    if rx.db_low_auto {
        let n = spectrum_row.len();
        let edge = (n / AUTO_DB_LOW_EDGE_EXCLUDE_FRACTION).max(AUTO_DB_LOW_MIN_EDGE_EXCLUDE);
        if n > edge * 2 {
            let raw_min = spectrum_row[edge..n - edge].iter().copied().fold(f32::INFINITY, f32::min);
            if raw_min.is_finite() {
                let prev = rx.db_low_auto_smoothed.unwrap_or(raw_min);
                let smoothed = prev + AUTO_DB_LOW_SMOOTHING_ALPHA * (raw_min - prev);
                rx.db_low_auto_smoothed = Some(smoothed);
                rx.db_low = smoothed.clamp(-180.0, rx.db_high - 1.0);
            }
        }
    }

    rx.spectrum.set_lo_frequency_hz(freq_hz as f64);
    // RIT -- see the main receiver's identical treatment for why this
    // is summed into the WDSP shift here but NOT into ctun_offset_hz
    // itself (which drives the visible dial/passband/zoom centering
    // above).
    let rit_offset_hz = if rx.rit_enabled { rx.rit_offset_hz } else { 0.0 };
    rx.spectrum.set_ctun(rx.ctun || rx.rit_enabled, ctun_offset_hz + rit_offset_hz);
    rx.spectrum.set_zoom_pan(rx.spectrum_zoom, effective_pan);
    let dial_freq_hz = if rx.ctun { rx.ctun_frequency_hz } else { freq_hz };

    ui.label(
        egui::RichText::new(format_frequency(dial_freq_hz))
            .monospace()
            .size(28.0)
            .strong()
            .color(egui::Color32::GREEN),
    )
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

        // Same row as the mode buttons, matching the main window's
        // identical placement.
        ui.add_space(12.0);
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
        // Same dB-displayed treatment as the main window's identical
        // control -- see scroll_slider_f32_db's doc comment (main.rs).
        if scroll_slider_f32_db(ui, &mut rx.slider_scroll_accum, &mut gain, -100.0, 18.0, 1.0) {
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
        // RIT -- see the main receiver's identical treatment for the
        // full reasoning. No XIT here -- extra receivers never transmit.
        let rit_label = if rx.rit_offset_hz == 0.0 {
            "RIT".to_string()
        } else {
            format!("RIT {:+.0}", rx.rit_offset_hz)
        };
        let rit_resp = ui
            .add(egui::Button::selectable(rx.rit_enabled, rit_label))
            .on_hover_text(
                "Receiver Incremental Tuning -- nudges what you hear without moving the \
                 displayed/logged frequency. Scroll to adjust -- Shift: 10 Hz, none: 100 Hz.",
            );
        if rit_resp.clicked() {
            rx.rit_enabled = !rx.rit_enabled;
            rx.settings_dirty.store(true, Ordering::Relaxed);
        }
        if rit_resp.hovered() {
            let scroll_delta = ui.input(|i| i.smooth_scroll_delta);
            let delta =
                if scroll_delta.y.abs() >= scroll_delta.x.abs() { scroll_delta.y } else { scroll_delta.x };
            if delta != 0.0 {
                rx.rit_scroll_accum += delta;
                const NOTCH: f32 = 50.0;
                let shift = ui.input(|i| i.modifiers.shift);
                let step: i64 = if shift { 10 } else { 100 };
                let mut new_offset = rx.rit_offset_hz as i64;
                while rx.rit_scroll_accum.abs() >= NOTCH {
                    let sign = rx.rit_scroll_accum.signum();
                    rx.rit_scroll_accum -= sign * NOTCH;
                    new_offset += step * sign as i64;
                }
                new_offset = new_offset.clamp(-9_999, 9_999);
                if new_offset as f64 != rx.rit_offset_hz {
                    rx.rit_offset_hz = new_offset as f64;
                    rx.settings_dirty.store(true, Ordering::Relaxed);
                }
            }
        }
        if ui.button("Clear").on_hover_text("Zero the RIT offset").clicked() {
            rx.rit_offset_hz = 0.0;
            rx.settings_dirty.store(true, Ordering::Relaxed);
        }
    });

    ui.add_space(4.0);
    ui.label("Spectrum");
    // Split the window's remaining vertical space between the spectrum
    // and waterfall, according to rx.spectrum_waterfall_ratio
    // (adjustable via the drag handle between them) -- see the main
    // receiver's own version of this for the full reasoning. The
    // Zoom/Pan row below the waterfall needs its own reserve here too
    // (unlike a plain no-reserve version, which would let the split
    // claim all available height and push Zoom/Pan below the window).
    let zoom_pan_reserve = ui.spacing().interact_size.y + 8.0 + SPECTRUM_WATERFALL_DIVIDER_HEIGHT;
    let spectrum_waterfall_height =
        (ui.available_height() - zoom_pan_reserve).max(200.0);
    let spectrum_height = (spectrum_waterfall_height * rx.spectrum_waterfall_ratio).max(80.0);
    let (rect, spectrum_resp) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), spectrum_height),
        egui::Sense::click_and_drag(),
    );

    if let Some(pos) = spectrum_resp.interact_pointer_pos() {
        if spectrum_resp.clicked() {
            let new_freq = freq_at_x(pos.x, rect, freq_hz, sample_rate, rx.spectrum_zoom, pan_offset_hz);
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
    // Click-and-drag -- see the main receiver's identical treatment for
    // why this uses drag_delta() rather than an absolute cursor-position
    // mapping.
    if spectrum_resp.dragged() {
        let hz_per_px = (2.0 * visible_half_span_hz) / rect.width().max(1.0) as f64;
        rx.drag_tune_accum_hz += -spectrum_resp.drag_delta().x as f64 * hz_per_px;
        const STEP_HZ: i64 = 1_000;
        let mut new_freq = dial_freq_hz as i64;
        while rx.drag_tune_accum_hz.abs() >= STEP_HZ as f64 {
            let sign = rx.drag_tune_accum_hz.signum();
            rx.drag_tune_accum_hz -= sign * STEP_HZ as f64;
            new_freq += STEP_HZ * sign as i64;
        }
        new_freq = new_freq.max(0);
        if new_freq as u32 != dial_freq_hz {
            let (effective_freq, retune) = resolve_tune(rx.ctun, freq_hz, sample_rate, passband, new_freq as u32);
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
    // half_span_hz/visible_half_span_hz/pan_offset_hz (zoom/pan) are
    // computed earlier in this function so the click-to-tune handlers
    // above (which run before this drawing code) can use them too.
    let view_center_hz = freq_hz as f64 + pan_offset_hz;

    draw_band_edge_markers(ui.painter(), rect, view_center_hz, visible_half_span_hz);

    let num_freq_ticks = 10;
    for t in 0..num_freq_ticks {
        let frac = t as f32 / (num_freq_ticks - 1) as f32;
        let x = rect.left() + frac * rect.width();
        ui.painter().line_segment(
            [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
            egui::Stroke::new(1.0, egui::Color32::from_gray(55)),
        );
        // Skip the label at the first/last tick -- see the main window's
        // identical treatment.
        if t == 0 || t == num_freq_ticks - 1 {
            continue;
        }
        let tick_freq_hz =
            view_center_hz - visible_half_span_hz + frac as f64 * (2.0 * visible_half_span_hz);
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
        let frac = ((offset_hz - pan_offset_hz + visible_half_span_hz) / (2.0 * visible_half_span_hz))
            .clamp(0.0, 1.0) as f32;
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

        // Plain full-width bin mapping -- see the main receiver's
        // identical treatment for why no zoom-aware cropping is needed
        // here (WDSP's own analyzer already returns just the visible
        // window's data).
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

    // Small audio-waveform overlay -- output audio, same as the main
    // receiver's RX case (extra receivers never transmit, so there's no
    // TX case to switch on here).
    let waveform_samples = peek_recent_samples(&rx.spectrum.waveform_out, WAVEFORM_WINDOW_SAMPLES);
    draw_audio_waveform(ui.painter(), rect, &waveform_samples);

    if let Some(pos) = spectrum_resp.hover_pos() {
        let hover_freq = freq_at_x(pos.x, rect, freq_hz, sample_rate, rx.spectrum_zoom, pan_offset_hz);
        draw_freq_hover_tooltip(ui.painter(), pos, hover_freq);
    }

    if spectrum_waterfall_divider(ui, &mut rx.spectrum_waterfall_ratio, spectrum_waterfall_height) {
        rx.settings_dirty.store(true, Ordering::Relaxed);
    }
    let waterfall_height = (spectrum_waterfall_height - spectrum_height).max(80.0);
    let (wf_rect, wf_resp) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), waterfall_height),
        egui::Sense::click_and_drag(),
    );
    if let Some(pos) = wf_resp.interact_pointer_pos() {
        if wf_resp.clicked() {
            let new_freq = freq_at_x(pos.x, wf_rect, freq_hz, sample_rate, rx.spectrum_zoom, pan_offset_hz);
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
    // Click-and-drag -- see the main receiver's identical treatment for
    // why this uses drag_delta() rather than an absolute cursor-position
    // mapping.
    if wf_resp.dragged() {
        let hz_per_px = (2.0 * visible_half_span_hz) / wf_rect.width().max(1.0) as f64;
        rx.drag_tune_accum_hz += -wf_resp.drag_delta().x as f64 * hz_per_px;
        const STEP_HZ: i64 = 1_000;
        let mut new_freq = dial_freq_hz as i64;
        while rx.drag_tune_accum_hz.abs() >= STEP_HZ as f64 {
            let sign = rx.drag_tune_accum_hz.signum();
            rx.drag_tune_accum_hz -= sign * STEP_HZ as f64;
            new_freq += STEP_HZ * sign as i64;
        }
        new_freq = new_freq.max(0);
        if new_freq as u32 != dial_freq_hz {
            let (effective_freq, retune) = resolve_tune(rx.ctun, freq_hz, sample_rate, passband, new_freq as u32);
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
        // No zoom-aware UV cropping needed -- see the main receiver's
        // identical treatment.
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
        let hover_freq = freq_at_x(pos.x, wf_rect, freq_hz, sample_rate, rx.spectrum_zoom, pan_offset_hz);
        draw_freq_hover_tooltip(ui.painter(), pos, hover_freq);
    }

    // Zoom/Pan -- see the main receiver's identical controls.
    ui.horizontal(|ui| {
        // Fill the available width -- see the main receiver's identical
        // treatment.
        let reserved = 230.0;
        ui.spacing_mut().slider_width = ((ui.available_width() - reserved) / 2.0).max(80.0);

        ui.label("Zoom:");
        let mut zoom = rx.spectrum_zoom;
        if scroll_slider_i32(ui, &mut rx.slider_scroll_accum, &mut zoom, 1..=16, 1, "x") {
            rx.spectrum_zoom = zoom;
            rx.settings_dirty.store(true, Ordering::Relaxed);
        }
        ui.add_space(12.0);
        ui.label("Pan:");
        ui.add_enabled_ui(rx.spectrum_zoom > 1, |ui| {
            let mut pan = rx.spectrum_pan;
            if scroll_slider_f32(ui, &mut rx.slider_scroll_accum, &mut pan, -1.0..=1.0, 0.1) {
                rx.spectrum_pan = pan;
                rx.settings_dirty.store(true, Ordering::Relaxed);
            }
        });
        if ui.button("Reset").on_hover_text("Zoom 1x, Pan centered").clicked() {
            rx.spectrum_zoom = 1;
            rx.spectrum_pan = 0.0;
            rx.settings_dirty.store(true, Ordering::Relaxed);
        }
    });

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
        for (tab, label) in [
            (SettingsTab::Agc, "RX"),
            (SettingsTab::Spectrum, "Spectrum"),
            (SettingsTab::Equalizer, "EQ"),
        ] {
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

        // No standalone Audio tab for extra receivers -- this receiver's
        // own Output device picker lives inline at the bottom of its RX
        // tab below (and it has no mic/TX concept at all) -- redirect
        // same as Network.
        SettingsTab::Audio => rx.settings_tab = SettingsTab::Agc,

        // TX (and PA Calibration/PureSignal, split out of it) are all
        // global (one radio, one PA/mic path), not per-receiver -- no
        // such tabs shown for extra receivers, so redirect same as
        // Network if any of these are ever somehow selected.
        SettingsTab::Tx => rx.settings_tab = SettingsTab::Agc,
        SettingsTab::PaCalibration => rx.settings_tab = SettingsTab::Agc,
        SettingsTab::PureSignal => rx.settings_tab = SettingsTab::Agc,
        SettingsTab::Diversity => rx.settings_tab = SettingsTab::Agc,
        // Firmware update is against the whole radio, not a per-receiver
        // concept -- redirect same as Network.
        SettingsTab::Firmware => rx.settings_tab = SettingsTab::Agc,

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
            ui.separator();

            ui.horizontal(|ui| {
                // Same picker/behavior as the main window's Settings ->
                // Audio "Output device" -- independent per receiver, so
                // e.g. the main receiver can go to real speakers while
                // this one feeds a virtual cable for a second decoder,
                // or vice versa.
                ui.label("Output device:");
                let devices = audio::list_output_devices();
                let current_label =
                    rx.audio_output_device.clone().unwrap_or_else(|| "(System Default)".to_string());
                egui::ComboBox::from_id_salt(("extra_receiver_audio_output_device", rx.ddc_index))
                    .selected_text(current_label)
                    .show_ui(ui, |ui| {
                        if ui.selectable_label(rx.audio_output_device.is_none(), "(System Default)").clicked()
                            && rx.audio_output_device.is_some()
                        {
                            rx.audio_output_device = None;
                            rx.audio_output = AudioOutput::start(Arc::clone(&rx.spectrum.audio_out), None).ok();
                            rx.settings_dirty.store(true, Ordering::Relaxed);
                        }
                        for name in &devices {
                            let selected = rx.audio_output_device.as_deref() == Some(name.as_str());
                            if ui.selectable_label(selected, name).clicked() && !selected {
                                rx.audio_output_device = Some(name.clone());
                                rx.audio_output = AudioOutput::start(Arc::clone(&rx.spectrum.audio_out), Some(name)).ok();
                                rx.settings_dirty.store(true, Ordering::Relaxed);
                            }
                        }
                    });
            });
        }

        SettingsTab::Spectrum => {
            ui.horizontal(|ui| {
                ui.label("Spectrum");
                ui.label("Low:");
                ui.add_enabled_ui(!rx.db_low_auto, |ui| {
                    let mut low = rx.db_low;
                    if scroll_slider_f32(ui, &mut rx.slider_scroll_accum, &mut low, -180.0..=0.0, 2.0) {
                        rx.db_low = low;
                        let (a, b, c, d) = (rx.db_low, rx.db_high, rx.waterfall_db_low, rx.waterfall_db_high);
                        remember_band_settings(&mut rx.band_memory, freq_hz, a, b, c, d, agc_params.mode);
                        rx.settings_dirty.store(true, Ordering::Relaxed);
                    }
                });
                if ui
                    .selectable_label(rx.db_low_auto, "Auto")
                    .on_hover_text(
                        "Continuously track the lowest level shown in the spectrum trace, \
                         smoothed to avoid jumping on every noise spike.",
                    )
                    .clicked()
                {
                    rx.db_low_auto = !rx.db_low_auto;
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

        SettingsTab::Equalizer => {
            // No RX/TX selector here -- extra receiver windows never
            // transmit, see render_equalizer_panel's doc comment.
            let mut eq = rx.spectrum.eq();
            if render_equalizer_panel(ui, &mut rx.slider_scroll_accum, "RX", &mut eq) {
                rx.spectrum.set_eq(eq);
                rx.settings_dirty.store(true, Ordering::Relaxed);
            }
        }
    }
}

/// `zoom`/`pan_offset_hz` describe the currently visible window the same
/// way the spectrum-drawing code's own visible_half_span_hz/
/// pan_offset_hz do -- pass 1.0/0.0 for the old (full-span) behavior.
fn freq_at_x(
    x: f32,
    rect: egui::Rect,
    center_freq_hz: u32,
    sample_rate: u32,
    zoom: i32,
    pan_offset_hz: f64,
) -> u32 {
    let frac = ((x - rect.left()) / rect.width()).clamp(0.0, 1.0) as f64;
    let half_span_hz = sample_rate as f64 / 2.0;
    let visible_half_span_hz = half_span_hz / zoom as f64;
    let freq =
        center_freq_hz as f64 + pan_offset_hz - visible_half_span_hz + frac * (2.0 * visible_half_span_hz);
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
    let spectrum = SpectrumHandle::start(
        idx as i32,
        Arc::clone(&iq_buffer),
        rate_val as i32,
        None,
        Arc::clone(&session.mox),
    );

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
        spectrum.set_eq(s.eq);
    }

    let audio_output_device = saved.and_then(|s| s.audio_output_device.clone());
    let audio_output =
        AudioOutput::start(Arc::clone(&spectrum.audio_out), audio_output_device.as_deref()).ok();
    let initial_frequency_hz = freq_arc.load(Ordering::Relaxed);
    // Restore CTUN -- see Config::ctun's doc comment (same reasoning,
    // per receiver).
    let ctun = saved.map(|s| s.ctun).unwrap_or(false);
    let ctun_frequency_hz =
        if ctun { saved.map(|s| s.ctun_frequency_hz).unwrap_or(initial_frequency_hz) } else { initial_frequency_hz };
    // Restore RIT -- see ConnectedState::rit_enabled's doc comment
    // (same reasoning, per receiver).
    let rit_enabled = saved.map(|s| s.rit_enabled).unwrap_or(false);
    let rit_offset_hz = saved.map(|s| s.rit_offset_hz).unwrap_or(0.0);

    Some(Arc::new(Mutex::new(ExtraReceiver {
        ddc_index: idx,
        iq_buffer,
        frequency_hz: freq_arc,
        sample_rate_hz: rate_arc,
        adc: adc_arc,
        num_adcs,
        protocol,
        antenna: antenna_arc,
        mox: Arc::clone(&session.mox),
        spectrum,
        audio_output,
        audio_output_device,
        waterfall_texture: None,
        waterfall_signature: None,
        scroll_accum: 0.0,
        slider_scroll_accum: 0.0,
        drag_tune_accum_hz: 0.0,
        db_low: saved.map(|s| s.db_low).unwrap_or(-140.0),
        db_low_auto: saved.map(|s| s.db_low_auto).unwrap_or(true),
        db_low_auto_smoothed: None,
        db_high: saved.map(|s| s.db_high).unwrap_or(-40.0),
        waterfall_db_low: saved.map(|s| s.waterfall_db_low).unwrap_or(-140.0),
        waterfall_db_high: saved.map(|s| s.waterfall_db_high).unwrap_or(-60.0),
        waterfall_palette: saved.map(|s| s.waterfall_palette).unwrap_or(Palette::Ocean),
        spectrum_waterfall_ratio: saved.map(|s| s.spectrum_waterfall_ratio).unwrap_or(150.0 / 350.0),
        spectrum_zoom: saved.map(|s| s.spectrum_zoom).unwrap_or(1),
        spectrum_pan: saved.map(|s| s.spectrum_pan).unwrap_or(0.0),
        show_settings_window: false,
        settings_tab: SettingsTab::Agc,
        settings_dirty,
        band_memory: saved.map(|s| s.band_settings.clone()).unwrap_or_default(),
        width_memory: saved.map(|s| s.width_memory.clone()).unwrap_or_default(),
        ctun,
        ctun_frequency_hz,
        rit_enabled,
        rit_offset_hz,
        rit_scroll_accum: 0.0,
        open: true,
        // Live-tracked every frame from here on (see this receiver's
        // viewport closure) -- seeded from saved config too, purely so
        // there's a sane value to write back out if the app exits
        // before this window ever renders a frame.
        window_geometry: saved.and_then(|s| s.window_geometry),
        // Set once, here, and never touched again -- see its own doc
        // comment for why.
        initial_window_geometry: saved.and_then(|s| s.window_geometry),
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

    let spectrum = SpectrumHandle::start(
        0,
        Arc::clone(&connected.session.iq_buffers[0]),
        new_rate as i32,
        Some(Arc::clone(&connected.session.rx_audio_to_radio)),
        Arc::clone(&connected.session.mox),
    );
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

    connected.audio_output = match AudioOutput::start(
        Arc::clone(&spectrum.audio_out),
        connected.audio_output_device.as_deref(),
    ) {
        Ok(a) => Some(a),
        Err(e) => {
            eprintln!("audio output unavailable after sample rate change: {e}");
            None
        }
    };
    if let Some(s) = &connected.rigctl_server {
        s.set_demod_params(spectrum.demod_params_handle());
        s.set_display(Arc::clone(&spectrum.display));
    }
    if let Some(s) = &connected.tci_server {
        s.set_demod_params(spectrum.demod_params_handle());
        // Points any already-streaming client at this new
        // SpectrumHandle's queues -- see TciServer::set_audio_iq's doc
        // comment for why this can't be skipped the same way
        // set_demod_params can't be.
        s.set_audio_iq(Arc::clone(&spectrum.tci_audio_out), Arc::clone(&spectrum.iq_out));
    }
    if let Some(s) = &connected.cat_server {
        s.set_demod_params(spectrum.demod_params_handle());
        s.set_display(Arc::clone(&spectrum.display));
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
            // Same reasoning as the RX SpectrumHandle rebuild above --
            // tear down before creating a replacement on the same channel.
            connected.tx_spectrum.stop();
            let tx_spectrum_iq: Arc<Mutex<VecDeque<IqSample>>> = Arc::new(Mutex::new(VecDeque::new()));
            connected.tx_spectrum = SpectrumHandle::start(
                connected.session.iq_buffers.len() as i32 + 1,
                Arc::clone(&tx_spectrum_iq),
                new_rate as i32,
                None,
                Arc::clone(&connected.session.mox),
            );
            if let Some(mic) = &connected.mic_input {
                let tx_handle = TxHandle::start(
                    Arc::clone(mic.buffer()),
                    Arc::clone(&connected.session.tci_tx_audio),
                    Arc::clone(&connected.session.radio_mic_audio),
                    Arc::clone(&connected.session.tx_audio_source),
                    Arc::clone(&connected.session.tci_wants_mic),
                    Arc::clone(&connected.session.tx_iq),
                    Arc::clone(&tx_spectrum_iq),
                    Arc::clone(&connected.session.mox),
                    connected.session.iq_buffers.len() as i32,
                    connected.device.protocol,
                    48_000,
                    new_rate as i32,
                    connected.puresignal_enabled,
                    Arc::clone(&connected.session.ps_rx_feedback_iq),
                    Arc::clone(&connected.session.ps_tx_feedback_iq),
                    ps_corr_path(connected.device.mac),
                );
                tx_handle.set_mic_gain(mic_gain);
                tx_handle.set_mode(connected.spectrum.mode());
                tx_handle.set_width_hz(connected.spectrum.width_hz());
                tx_handle.set_ps_enabled(connected.ps_enabled);
                tx_handle.set_ps_hw_peak(connected.ps_hw_peak);
                tx_handle.set_ps_mox_delay(connected.ps_mox_delay);
                tx_handle.set_ps_loop_delay(connected.ps_loop_delay);
                tx_handle.set_ps_tx_delay_ns(connected.ps_tx_delay_ns);
                tx_handle.set_ps_ptol(connected.ps_ptol);
                // See connect_to_device's identical restore -- this
                // rebuild also opens a fresh WDSP channel with no
                // calibration history of its own.
                if connected.puresignal_enabled {
                    if let Some(path) = ps_corr_path(connected.device.mac) {
                        if path.exists() {
                            tx_handle.restore_ps_corr();
                        }
                    }
                }
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

    let spectrum = SpectrumHandle::start(
        rx.ddc_index as i32,
        Arc::clone(&rx.iq_buffer),
        new_rate as i32,
        None,
        Arc::clone(&rx.mox),
    );
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

    rx.audio_output = match AudioOutput::start(Arc::clone(&spectrum.audio_out), rx.audio_output_device.as_deref()) {
        Ok(a) => Some(a),
        Err(e) => {
            eprintln!("audio output unavailable after sample rate change: {e}");
            None
        }
    };
    rx.spectrum = spectrum;
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
    // Window/taskbar icon -- also the source PNG for the .desktop entry's
    // app-menu icon installed by `cargo deb` (see assets/icons/hpsdr-rs.png
    // and assets/hpsdr-rs.desktop). Embedded at compile time so the running
    // app always shows an icon even when launched via `cargo run` outside
    // any package install.
    let icon = eframe::icon_data::from_png_bytes(include_bytes!("../assets/icons/hpsdr-rs.png"))
        .expect("bundled icon PNG failed to decode");

    // No saved position to restore here -- unlike everything else this
    // window shows (it's also the Discovery screen), its geometry is
    // now keyed per-radio (see Config::window_geometry's doc comment),
    // so there's nothing to seed until a specific radio is chosen; see
    // the DiscoveryAction::Start handler in ui() for where that happens.
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1200.0, 660.0])
            .with_min_inner_size([900.0, 520.0])
            .with_icon(icon),
        ..Default::default()
    };
    eframe::run_native(
        "hpsdr-rs",
        options,
        Box::new(|cc| Ok(Box::new(HpsdrApp::new(&cc.egui_ctx)))),
    )
}
