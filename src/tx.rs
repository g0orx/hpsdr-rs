/*
    TXA (WDSP's transmit chain) wrapper: takes mic/TX audio captured by
    audio.rs's MicInput, runs it through WDSP's TXA processing, and
    produces TX IQ samples at the radio's DUC rate for radio.rs's
    sender threads to stream out when MOX (PTT) is asserted.

    CONFIDENCE LEVEL, please read before trusting this blindly --
    this is the least-verified file in the whole project:

    - Every WDSP call for the RX/RXA side elsewhere in this codebase
      (spectrum.rs) was confirmed against a working reference
      implementation before being written. This file was NOT: there is
      no confirmed reference for the TXA call sequence, so everything
      here is inferred by symmetry with the confirmed RXA pattern (same
      OpenChannel/fexchange0 shape, mirrored: audio in instead of IQ
      in, IQ out instead of audio out) plus the WDSP header's own
      function names. That symmetry is a reasonable bet, not a
      confirmed fact.
    - In particular: whether fexchange0 is really the correct exchange
      function for a TXA (type_=1) channel -- as opposed to a separate,
      differently-named TX exchange function that isn't in this
      project's current wdsp_sys bindings -- is unconfirmed. If TX
      audio comes out silent, distorted, or fexchange0 errors out on a
      TXA channel, this is the first thing to check.
    - ALC is enabled with conservative fixed attack/decay/hang and
      WDSP's own internal default target level (there's no exposed
      Set*ALC target/top function in the current bindings, unlike
      RXA's AGC) -- not tuned or verified against real RF output.
    - FIXED, but noted in case it recurs in a different form: an
      earlier version used TX_CHANNEL=1000 (reasoning: "must be
      globally unique, distinct from any RX channel"), which segfaulted
      the whole process immediately on enabling TX -- almost certainly
      OpenChannel indexing "channel" directly into a small fixed-size
      C array with no bounds checking. RXA and TXA are separate
      internal tables (that's what OpenChannel's type_ param is for),
      so TX_CHANNEL only needs to be unique among other TXA channels,
      not globally; it's 0 now. If a segfault reappears immediately on
      enabling TX after some future change, a too-large or otherwise
      out-of-range id passed to any Set*() / Get*() call in this file is
      the first thing to suspect.

    DO NOT key this into an antenna without first verifying, into a
    dummy load at reduced drive, that: MOX actually gates transmission
    (never transmits unless explicitly armed), the audio isn't garbled,
    and ALC is actually preventing overdrive. See radio.rs for the
    equally-unverified protocol-level MOX/TX-IQ framing this feeds.

    PureSignal (Phase 2 -- see the plan doc for the full multi-phase
    story): feeds forward TX IQ and the two feedback streams radio.rs
    already demuxes into WDSP's calcc engine (psccF), which lives
    inside this same TXA channel with no separate OpenChannel needed.
    Feedback arrives with real network latency behind the corresponding
    TX audio it's paired with here (see drain_ps_feedback) -- any
    chunk where it hasn't caught up yet simply skips the PS feed rather
    than stalling the real-time audio loop, which is expected/normal
    right after PTT, not an error. Forward TX IQ and RX-feedback are
    paired 1:1, both protocols -- see drain_ps_feedback's doc comment
    for a real bug this used to have here (an unnecessary 2:1 RX-
    feedback decimation, confirmed via real hardware A/B against
    piHPSDR to be actively suppressing Feedback Level). Like everything
    else in this file, none of this has a confirmed-working reference
    for the exact call cadence/timing -- Phase 4's real-hardware
    verification is what actually validates it.
*/

use crate::radio::{
    IqSample, TX_AUDIO_SOURCE_LOCAL_MIC, TX_AUDIO_SOURCE_RADIO_MIC,
};
use crate::spectrum::{EqBandCount, EqualizerParams, Mode};
use crate::wdsp_sys as wdsp;
use std::collections::VecDeque;
use std::ffi::CString;
use std::os::raw::c_int;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

// WDSP's C internals almost certainly index "channel" directly into a
// small fixed-size array with no bounds checking (typical for this
// style of embedded DSP library).
//
// Two hardcoded guesses here both crashed, for opposite reasons:
//   - channel=1000 ("must be globally unique") segfaulted immediately
//     on enabling TX -- almost certainly out-of-bounds on that array.
//   - channel=0 (assuming RXA and TXA are separate internal tables,
//     since that's what OpenChannel's type_ parameter suggests) also
//     segfaulted, but inside spectrum::run() -- the *RX* thread --
//     meaning it wasn't out-of-bounds, it collided with and corrupted
//     the already-running RXA channel 0's own state. So RXA and TXA
//     apparently do NOT have separate channel-number namespaces after
//     all, at least not for channel 0.
//
// Given two wrong guesses at a fixed constant, TxProcessor now takes
// its channel number as a parameter instead: the caller (main.rs)
// passes one derived from the actual number of RX channels open this
// session (RadioSession::iq_buffers.len()), which is structurally
// guaranteed distinct from every RX channel actually in use, and is
// always small (bounded by real receiver hardware, typically <=7) --
// as opposed to another arbitrary hardcoded number that might still
// collide with a future RX channel or exceed some still-unknown array
// bound. If this *still* segfaults, the channel-as-array-index theory
// itself may be wrong, or there's a different fixed bound (e.g. a
// MAX_CHANNELS-style constant) worth checking directly against your
// WDSP source if you have it.

// Confirmed against a working reference (rustyHPSDR): matches its
// microphone_buffer_size and fft_size exactly. An earlier version of
// this file used 1024 for both in_size and dsp_size (mirroring RX's
// own chunking by unconfirmed guess) -- both wrong: they're supposed
// to be different values, and neither was 1024.
const TX_BUFFER_SIZE: usize = 512;
const TX_FFT_SIZE: i32 = 2048;

// ~0.25s of TX IQ at a typical 192ksps DUC rate -- same "small ring
// buffer, drop oldest on overflow" reasoning as every other buffer in
// this project (see radio.rs's IQ_BUFFER_CAPACITY comment): a backlog
// here becomes added key-down latency, not something that
// self-corrects.
const TX_IQ_BUFFER_CAPACITY: usize = 100_000;

/// Same convention as spectrum.rs's own (private) IQ_NORM -- 2^23,
/// scaling a 24-bit signed sample to [-1.0, 1.0].
const PS_IQ_NORM: f32 = 8_388_608.0;

/// ~0.25s of TX spectrum-display IQ at a typical 192ksps DUC rate --
/// same "small ring buffer, drop oldest" reasoning as TX_IQ_BUFFER_CAPACITY.
const TX_SPECTRUM_IQ_CAPACITY: usize = 50_000;

/// ~0.3s at 48kHz -- same "small ring buffer, drop oldest" reasoning
/// as the other capacities above. See TxHandle::tx_audio_monitor's doc
/// comment for what this is for.
const TX_AUDIO_MONITOR_CAPACITY: usize = 14_400;

/// Reference ALC decay (ms) -- piHPSDR's own confirmed value.
///
/// TRIED AND REVERTED: a TCI-only 300ms override (30x this), on the
/// theory that slowing how fast ALC gain rides up during TCI's real,
/// repeating quiet stretches (confirmed via WDSP's own mic_pk meter --
/// see SetTXAALCMaxGain's doc comment for the pumping mechanism this
/// retriggers) would reduce the resulting power pumping without
/// touching attack's fast overdrive protection. Real-hardware test
/// showed the opposite: mic_pk swung even WIDER (0.57-2.06 vs the
/// previous 0.57-0.88) and alc_gain never settled back to 0 anymore,
/// chasing the input with a 300ms lag instead of responding cleanly --
/// an asymmetric fast-attack/slow-decay pairing made the loop more
/// underdamped, not less. Reverted; the underlying defect is in
/// WSJT-X's own TCI Tune-audio generation and isn't fixable by
/// retuning this project's ALC.
const ALC_DECAY_DEFAULT_MS: i32 = 10;

// BUG FIX (removed a real one): this file used to 2:1-decimate
// (average) RX-feedback samples before pairing them with TX-feedback
// for pscc/psccF, on the claim that "DDC0 (RX-feedback) delivers
// samples at ~2x the packet rate of DDC1 (TX-feedback), despite both
// being configured identically at 192ksps" on Protocol 2 -- introduced
// in the original PureSignal Phase 2/3 commit, which *also* says in
// its own message "PS Phase 2/3 not yet verified against real
// hardware (Phase 4)", i.e. before any actual hardware test of PS
// existed to have confirmed this. Confirmed wrong two ways: (1)
// piHPSDR's radio.c requests the IDENTICAL rate expression for both
// (`create_pure_signal_receiver(PS_TX_FEEDBACK, ..., 192000, ...)` and
// same for PS_RX_FEEDBACK); (2) this project's own radio.rs already
// requests both DDC0 and DDC1 at the same 192_000 (see
// p2_sender_loop's PureSignal DDC-rate comment) -- there was never a
// real 2x rate mismatch to correct for. The averaging was actively
// harmful: boxcar-averaging pairs of complex feedback samples smooths
// exactly the two-tone envelope peaks WDSP's calcc engine measures,
// suppressing the reported Feedback Level -- confirmed via real
// hardware A/B against piHPSDR on the same radio at the same drive
// (piHPSDR: Feedback Level 158 at 75W; hpsdr-rs before this fix: ~60
// at the same 75W). Removed entirely; both feedback streams are now
// paired 1:1, matching both confirmed references.

#[derive(Copy, Clone, Default)]
pub struct TxDisplay {
    /// Peak mic input level, roughly 0.0-1.0-ish (units not confirmed
    /// -- see module note). For a simple TX level bar in the UI.
    pub mic_pk: f64,
    /// Average ALC gain reduction. WDSP convention for this value
    /// (dB? linear? sign?) is not confirmed against a reference --
    /// treat the UI meter built from this as relative/qualitative
    /// ("is ALC doing something") rather than a calibrated reading.
    pub alc_av: f64,
}

/// PureSignal calibration controls (Settings -> PureSignal), applied
/// live each chunk while transmitting -- see TxProcessor::apply_ps_params.
/// Distinct from TxHandle::puresignal_enabled (mirroring RadioSession::
/// puresignal_enabled -- both live now, see that field's doc comment):
/// this struct's own `enabled` is PureSignal's live on/off for the WDSP
/// engine itself, meaningful only while the session's feedback-receiver
/// wire plumbing is actually reserved -- which, since it's now reserved
/// unconditionally for the whole session on any board that supports it,
/// just means "this radio/board combination supports PureSignal at all",
/// not "PureSignal happened to be on when this session connected".
#[derive(Copy, Clone)]
pub struct PsParams {
    /// true = continuous auto-calibrate (`SetPSControl(ch,0,0,1,0)`,
    /// the normal running state during TX, confirmed against Thetis/
    /// piHPSDR); false = reset/off (`SetPSControl(ch,1,0,0,0)`).
    pub enabled: bool,
    /// Incremented by the Calibrate button each click; tx.rs issues a
    /// single-shot manual calibration (`SetPSControl(ch,1,1,0,0)`)
    /// whenever it sees this change, matching Thetis/piHPSDR's own
    /// "Single Calibrate" action -- separate from the continuous
    /// auto-calibrate `enabled` above runs the rest of the time.
    pub calibrate_request: u32,
    /// `SetPSHWPeak` -- per-hardware-model calibration constant (0.0-1.0
    /// normalized ADC feedback level at full-scale RF), confirmed
    /// Thetis defaults: P1/USB 0.4072, P2 0.2899, Saturn 0.6121. Set
    /// once per radio model and left alone; not auto-detected.
    pub hw_peak: f64,
    /// `SetPSMoxDelay` (seconds) -- settling time between MOX assertion
    /// and feedback collection. Confirmed reference default: 0.2s.
    pub mox_delay: f64,
    /// `SetPSLoopDelay` (seconds) -- time between correction
    /// recalculations. Confirmed reference default: 0.0s (fastest).
    pub loop_delay: f64,
    /// `SetPSTXDelay` (nanoseconds) -- PA chain group delay
    /// compensation. Confirmed reference default: 150ns (Apache Labs
    /// hardware).
    pub tx_delay_ns: f64,
    /// `SetPSPtol` -- outlier tolerance (0.0-1.0) for the correction-
    /// table curve fit's culling step. Confirmed reference default:
    /// 0.8 (TXA.c's create_calcc call), previously never exposed/set
    /// by this project at all (left at that WDSP-internal default).
    /// LOWER values allow culling MORE outlier samples before fitting
    /// (`cull()`'s allowed-cull-count is proportional to `1.0-ptol`) --
    /// worth lowering if `Correcting` never turns on despite
    /// calibration attempts completing: WDSP's own scheck() function
    /// (which gates whether a computed correction table is trusted
    /// enough to apply) can reject an otherwise-valid table if the fit
    /// overshoots full scale anywhere, which noisy/imprecise
    /// calibration data (e.g. from feedback with reduced time
    /// resolution) can cause even when the underlying signal is fine.
    pub ptol: f64,
    /// Incremented to trigger an async save of the current correction
    /// table (`PSSaveCorr`) to the fixed per-radio path `TxProcessor`
    /// was opened with -- see `TxHandle::save_ps_corr`'s doc comment
    /// for when this actually gets called (auto, on a `Correcting`
    /// false->true edge). Same monotonic-counter-not-a-bool shape as
    /// `calibrate_request`, for the same reason: never miss/coalesce a
    /// request within one chunk.
    pub save_corr_request: u32,
    /// Incremented to trigger an async load+apply of a previously-saved
    /// correction table (`PSRestoreCorr`) from that same path -- see
    /// `TxHandle::restore_ps_corr`'s doc comment. `PSRestoreCorr` sets
    /// WDSP's internal `turnon` flag, which clears `automode` once
    /// applied (a one-shot "use this table" mode, not continuous
    /// recalibration) -- so continuous auto-calibrate (`enabled` above)
    /// needs re-asserting afterward if that's wanted, same as
    /// piHPSDR/Thetis's own restore-then-resume-automode pattern.
    pub restore_corr_request: u32,
    /// piHPSDR's "OneShot" -- when true, the `enabled` edge (and every
    /// resend of it -- see `TxProcessor::ps_resend_enabled_countdown`)
    /// issues a single manual calibration (`SetPSControl(ch,0,1,0,0)`,
    /// mancal=1/automode=0) instead of continuous auto-calibrate
    /// (`SetPSControl(ch,0,0,1,0)`). Confirmed against piHPSDR's
    /// tx_ps_resume: `if (tx->ps_oneshot) SetPSControl(id,0,1,0,0);
    /// else SetPSControl(id,0,0,1,0);`. Needed for constant-envelope
    /// digital modes (FT8/FT4 etc.): their TX envelope never sweeps
    /// through the full amplitude range a correction table needs to
    /// fill all its bins, so continuous auto-calibrate against that
    /// audio can never complete a cycle and eventually forces a full
    /// reset via calcc.c's own watchdogs, dropping predistortion
    /// entirely (confirmed via real-hardware report: PS worked
    /// correctly with Two Tone -- envelope-rich -- but showed no
    /// correction/feedback at all transmitting FT8 via WSJT-X/rigctl).
    /// The intended workflow (same as piHPSDR): calibrate with Two Tone
    /// in continuous mode first, then enable OneShot before running
    /// digital traffic so the already-good table just gets applied.
    pub oneshot: bool,
}

impl Default for PsParams {
    fn default() -> Self {
        Self {
            enabled: true,
            calibrate_request: 0,
            hw_peak: 0.2899, // P2 default -- see field doc comment
            mox_delay: 0.2,
            loop_delay: 0.0,
            tx_delay_ns: 150.0,
            ptol: 0.8,
            save_corr_request: 0,
            restore_corr_request: 0,
            oneshot: false, // matches piHPSDR's own default (unchecked)
        }
    }
}

/// Live PureSignal status, read back from WDSP each chunk while
/// transmitting -- see TxProcessor::ps_status.
#[derive(Copy, Clone, Default)]
pub struct PsStatus {
    /// GetPSInfo's info[4] -- feedback signal level, confirmed ranges
    /// (Thetis/piHPSDR): <90 too weak, 128-181 ideal, >256 dangerously
    /// strong.
    pub feedback_level: i32,
    /// GetPSInfo's info[14] -- corrections currently being applied.
    pub correcting: bool,
    /// GetPSMaxTX -- measured peak TX amplitude (0.0-1.0 normalized),
    /// polled live so the user can compare against hw_peak.
    pub max_tx: f64,
}

#[derive(Copy, Clone)]
pub struct TxParams {
    pub mode: Mode,
    pub mic_gain: f32,
    /// Filter width (Hz), same UI control/meaning as RX's per-mode
    /// width slider (spectrum::width_for_mode) -- ROOT CAUSE FIX, see
    /// TxProcessor's bandpass-freqs update in process() for the full
    /// story: this was previously not threaded through to TX at all,
    /// and the TX bandpass filter was hardcoded to a fixed 300-2700Hz
    /// regardless of mode or this setting.
    pub width_hz: f64,
    /// When true, WDSP's PostGen tone generator replaces mic audio
    /// with a steady single tone centered in the current passband --
    /// see TxProcessor::process()'s PostGen update for the mechanism
    /// (confirmed against rustyHPSDR's own Tune implementation).
    pub tune: bool,
    /// When true, WDSP's PostGen replaces mic audio with a two-tone
    /// test signal instead of Tune's steady single tone. NOT just a
    /// cosmetic alternative to Tune -- PureSignal calibration actually
    /// REQUIRES this: confirmed by reading WDSP's calcc.c LCOLLECT
    /// state directly, PS bins TX envelope samples into 16 amplitude
    /// buckets spanning 0..HW Peak and only progresses once every
    /// bucket has collected enough samples. A steady tone has a
    /// CONSTANT envelope, so it only ever fills one bucket -- collect
    /// can never complete, regardless of drive level, HW Peak, or
    /// feedback attenuation (confirmed via real hardware testing: PS
    /// stuck in the COLLECT state indefinitely, on both P1 and P2,
    /// while testing with Tune). A two-tone signal's envelope beats
    /// between the two tones' magnitudes, continuously sweeping the
    /// full amplitude range -- exactly what calibration needs, and
    /// exactly why every reference PS implementation calibrates with a
    /// two-tone generator, never a steady carrier.
    pub two_tone: bool,
    /// See spectrum::EqualizerParams's doc comment -- same type, TXA side.
    pub eq: EqualizerParams,
}

impl Default for TxParams {
    fn default() -> Self {
        Self {
            mode: Mode::Usb,
            // Conservative starting point, same reasoning as
            // spectrum::DemodParams's RX audio gain default -- easier
            // to notice "too quiet" and turn it up than to start too
            // hot into a live transmitter. Fed directly to WDSP's Panel
            // gain stage (SetTXAPanelGain1) as a linear multiplier --
            // this project's UI slider has always used linear 0.0-1.0
            // semantics, kept as-is rather than switching to the
            // reference's dB convention (10^(dB/20)) to avoid a
            // breaking change to already-saved config values; the
            // *mechanism* (WDSP's own Panel gain stage rather than
            // pre-scaling raw samples ourselves) is what's now
            // confirmed-correct and fixed to match the reference, the
            // unit convention on top of it is a deliberate deviation.
            mic_gain: 0.5,
            width_hz: crate::spectrum::default_width_hz(Mode::Usb),
            tune: false,
            two_tone: false,
            eq: EqualizerParams::default(),
        }
    }
}

/// Shared signature of PSSaveCorr/PSRestoreCorr -- see
/// TxProcessor::ps_corr_action.
type PsCorrFn = unsafe extern "C" fn(c_int, *mut std::os::raw::c_char);

struct TxProcessor {
    channel: i32,
    mic_scratch: Vec<f64>,
    iq_scratch: Vec<f64>,
    last_mode: Option<Mode>,
    last_gain: Option<f32>,
    last_passband: Option<(f64, f64)>,
    last_eq: Option<EqualizerParams>,
    /// (tune, two_tone) as last applied to WDSP's PostGen -- see
    /// process()'s PostGen update for why these are tracked together.
    last_post_gen: Option<(bool, bool)>,
    /// See set_ps_mox's doc comment.
    last_ps_mox: Option<bool>,
    /// See apply_ps_params's doc comment.
    last_ps_enabled: Option<bool>,
    /// See apply_ps_params's doc comment -- PsParams::oneshot's edge
    /// (while already enabled) triggers a reset-then-resume cycle the
    /// same way piHPSDR's ps_off_on does.
    last_ps_oneshot: Option<bool>,
    last_ps_calibrate_request: Option<u32>,
    last_ps_hw_peak: Option<f64>,
    last_ps_mox_delay: Option<f64>,
    last_ps_loop_delay: Option<f64>,
    last_ps_tx_delay_ns: Option<f64>,
    last_ps_ptol: Option<f64>,
    last_ps_save_corr_request: Option<u32>,
    last_ps_restore_corr_request: Option<u32>,
    /// BUG FIX: `PSRestoreCorr` sets WDSP's internal `turnon` flag as a
    /// side effect (confirmed by reading calcc.c directly), which the
    /// state machine processes by forcing `automode` back to 0 (case
    /// LTURNON: `a->ctrl.automode = 0;`) regardless of what this
    /// project's own `ps.enabled` ("Running (continuous auto-
    /// calibrate)") is set to -- so restoring a saved correction table
    /// (done automatically on connect whenever one exists, see
    /// connect_to_device's doc comment) silently turns continuous
    /// auto-calibrate off in WDSP while the checkbox keeps showing it as
    /// checked. Confirmed via real hardware: `Correcting` stays on
    /// (using the restored table, as intended -- see
    /// TxHandle::restore_ps_corr's doc comment) but Measured Peak TX/
    /// Feedback Level never update again, since the state machine is
    /// stuck in LSTAYON. Since `ps.enabled`'s value never actually
    /// changes across a restore (it was already true, WDSP just silently
    /// stopped honoring it), apply_ps_params's normal `last_ps_enabled
    /// != Some(ps.enabled)` edge-detection can never notice or recover
    /// on its own.
    ///
    /// Fixed by re-sending the ON-path SetPSControl (reset=0,
    /// automode=1 -- NOT the OFF-path, which sends reset=1 and would
    /// immediately clear the just-restored correction's `Correcting`
    /// state, undoing the whole point of auto-restoring) for a short
    /// window after every restore, rather than a single attempt --
    /// `PSRestoreCorr`'s turnon=1 is set synchronously, but the actual
    /// state-machine tick that processes it (and clears automode) runs
    /// on WDSP's own audio-chunk cadence, so a single immediate resend
    /// could race it and lose. Counted in audio chunks, not wall time,
    /// since apply_ps_params is called once per chunk -- ~500ms at
    /// TX_BUFFER_SIZE/48kHz is comfortably longer than that race window.
    ps_resend_enabled_countdown: u32,
    /// Fixed for the whole session (set once at open(), from the same
    /// per-radio path save_ps_corr/restore_ps_corr trigger against) --
    /// None if unavailable (e.g. $HOME unset), in which case save/
    /// restore requests are silently skipped rather than attempted
    /// against a made-up path.
    ps_corr_path: Option<PathBuf>,
}

impl TxProcessor {
    fn open(channel: i32, protocol: u8, mic_rate: i32, duc_rate: i32, ps_corr_path: Option<PathBuf>) -> Self {
        // Confirmed against the reference: internal TXA processing rate
        // is protocol-dependent -- 96000 for Protocol 2, 48000 for
        // Protocol 1. An earlier version of this file fixed this at
        // 48000 always, which was wrong for P2.
        let dsp_rate = if protocol == 2 { 96_000 } else { 48_000 };
        let default_passband =
            crate::spectrum::passband_for(Mode::Usb, crate::spectrum::default_width_hz(Mode::Usb));

        // Serializes this one-time TXA setup sequence against every RXA
        // channel's own setup (spectrum.rs's SpectrumAnalyzer::open) --
        // see wdsp_sys::SETUP_LOCK's doc comment. Without this, TX
        // setup (started unconditionally alongside RX at initial
        // connect -- see main.rs's MicInput::start/TxHandle::start call
        // site) could run its own OpenChannel/FFTW calls concurrently
        // with an in-progress RXA setup, most dangerously the FFTW
        // wisdom-generation pass on a fresh machine/config, corrupting
        // FFTW's shared planner state (glibc's "double free or
        // corruption (!prev)" crash on startup was the confirmed
        // real-world symptom this fixed).
        let _setup_guard = wdsp::SETUP_LOCK.lock().unwrap();

        unsafe {
            // type_=1: TXA. Delay/slew params (tdelayup/tslewup/
            // tdelaydown/tslewdown) and dsp_size now match the
            // reference exactly rather than being guessed at 0.0 /
            // equal-to-in_size.
            wdsp::OpenChannel(
                channel,
                TX_BUFFER_SIZE as i32,
                TX_FFT_SIZE,
                mic_rate,
                dsp_rate,
                duc_rate,
                1, // type: TXA
                1, // state: running
                0.010,
                0.025,
                0.0,
                0.010,
                0,
            );

            // Every call below this point was entirely missing before
            // this fix -- confirmed against the reference, not
            // previously guessed at all. Most consequential: the TX
            // bandpass filter was never enabled, meaning transmitted
            // audio bandwidth was effectively unbounded/unfiltered
            // rather than limited to a sensible SSB voice passband --
            // a real spectral-cleanliness issue, not just a missing
            // nicety. The CFIR compensation filter (protocol 2 only)
            // is what the official protocol spec itself calls out as
            // needed to correct CIC droop at the edges of the transmit
            // passband.
            wdsp::TXASetNC(channel, TX_FFT_SIZE);
            // TESTED AND RULED OUT: tried mp=1 (minimum phase,
            // matching Thetis's actual default -- rustyHPSDR and
            // piHPSDR both effectively use mp=0/linear-phase) on the
            // theory that a shorter effective filter settling time
            // would shrink the transient a meter trace had localized
            // to this filter stage. A real-hardware trace compared
            // point-by-point against the mp=0 baseline (same offsets
            // since PTT) showed near-identical alc_av/out_av numbers
            // (e.g. -10.4dB vs -10.6dB, -6.7dB vs -6.8dB) -- no
            // measurable difference, so filter phase type isn't it
            // either. Reverted to 0, matching 2 of 3 references again.
            wdsp::TXASetMP(channel, 0);
            wdsp::SetTXABandpassWindow(channel, 1);
            // CONFIRMED via the diagnostic test this replaced: with
            // the bandpass filter (bp0) off, alc_pk/alc_av went
            // through one settling transient and then locked
            // completely flat (zero movement) for the rest of a
            // ~2-second trace -- with it on, they cycle continuously
            // for the whole transmission and never settle. That rules
            // out a transient/settling-time mechanism (also consistent
            // with attack-time and min-phase both measuring zero
            // effect earlier) and points at the filter's STEADY-STATE
            // passband not being flat across the narrow ~50Hz range
            // WSJT-X's FT8 hops its 8 tones within: each tone gets a
            // measurably different filter gain, and since WSJT-X
            // cycles through that same tone set repeatedly for the
            // whole transmission, output visibly bounces among those
            // same few levels on repeat -- explaining the continuous,
            // repeating "fast flutter" reported, why Tune (one fixed
            // frequency, always the same gain) never shows it, and why
            // neither attack time nor filter phase changed anything.
            // Filter back on -- it must stay on for real operation.
            wdsp::SetTXABandpassRun(channel, 1);
            wdsp::SetTXAFMEmphPosition(channel, 0);
            if protocol == 1 {
                wdsp::SetTXACFIRRun(channel, 0); // not needed for P1 -- done in FPGA instead
            } else {
                wdsp::SetTXACFIRRun(channel, 1);
            }
            wdsp::SetTXAEQRun(channel, 0);
            wdsp::SetTXAAMSQRun(channel, 0);
            wdsp::SetTXAosctrlRun(channel, 0);

            // Attack/decay: back to piHPSDR's confirmed 1ms/10ms.
            // TESTED AND RULED OUT: raised attack to 25ms on the
            // theory that the ALC (which WDSP's xtxa() runs AFTER the
            // 2048-tap/~21ms-group-delay TX bandpass filter) was
            // reacting to filter-settling transients on each FT8 tone
            // change (~6/sec, matching the reported "fast regular
            // flutter") -- real hardware test showed the bouncing
            // persisted unchanged at 25ms, so the ALC's own attack
            // speed is not the (or not the only) mechanism. Reverted
            // rather than leave an unjustified reference deviation in
            // place. See the meter-trace diagnostic below this
            // function -- added instead of guessing a fourth ALC
            // constant -- for actually observing what's happening
            // through the chain during a bouncing transmission.
            wdsp::SetTXAALCAttack(channel, 1);
            wdsp::SetTXAALCDecay(channel, ALC_DECAY_DEFAULT_MS);
            // MaxGain: this is the actual root cause of the
            // pumping/power-bouncing-on-real-speech bug (steady on
            // Tune's continuous tone, bouncing on WSJT-X CQ/voice).
            // The parameter is in dB (WDSP: max_gain =
            // pow(10, maxgain/20) -- see wcpAGC.c SetTXAALCMaxGain),
            // and it sets how far the ALC's gain floor is allowed to
            // rise during quiet gaps (wcpAGC.c: min_volts =
            // out_target/(var_gain*max_gain)), which then gets
            // slammed back down by the 1ms attack on the next loud
            // syllable/tone. Real speech has gaps for that gain to
            // ride up into every cycle -- Tune's unbroken tone
            // doesn't, so it settles and stays steady. WDSP's own
            // create_wcpagc default (TXA.c) is max_gain=1.0 (0dB,
			// no boost allowed at all), and piHPSDR never calls this
            // setter, so it runs at that same no-boost default --
            // confirmed by checking WDSP's C source directly, not
            // just piHPSDR's call list. 5.0 (dB, not linear -- a
            // unit mixup with TXALevelerTop below, which is also dB)
            // was giving ~1.8x of boost headroom, enough to
            // reproduce the exact symptom reported. Reverted to 0.0
            // dB (i.e. no boost) to match the confirmed-working
            // reference exactly; use the existing Mic Gain slider
            // for headroom instead, which is what piHPSDR relies on
            // for the same purpose.
            wdsp::SetTXAALCMaxGain(channel, 0.0);
            // RULED OUT: ALC gain-pumping as the cause of a persistent
            // wide spectral "skirt" seen on ANY mic-chain audio (TCI,
            // local USB mic via pipewire -- confirmed NOT WSJT-X/TCI-
            // specific) but never on Tune (PostGen bypasses the whole
            // mic-chain path, including ALC, entirely -- see xtxa()'s
            // processing order in WDSP's TXA.c). A direct test with ALC
            // fully disabled (SetTXAALCSt 0) showed the identical skirt,
            // ruling this stage out; back to its normal enabled state.
            wdsp::SetTXAALCSt(channel, 1);

            // Leveler: a separate, slower average-level-normalizing
            // stage from ALC's fast peak limiting. Previously not
            // touched at all; matches the reference's own defaults,
            // including leaving it OFF (SetTXALevelerSt 0) -- present
            // and correctly configured in case it's turned on later,
            // not silently absent.
            wdsp::SetTXALevelerAttack(channel, 1);
            wdsp::SetTXALevelerDecay(channel, 500);
            wdsp::SetTXALevelerTop(channel, 5.0);
            wdsp::SetTXALevelerSt(channel, 0);

            // Pre/PostGen (internal WDSP test-tone injection, used for
            // e.g. a "Tune" feature this project doesn't expose yet):
            // explicitly configured inert/off, matching the reference,
            // rather than left at whatever WDSP's own uninitialized
            // default happens to be.
            wdsp::SetTXAPreGenMode(channel, 0);
            wdsp::SetTXAPreGenToneMag(channel, 0.0);
            wdsp::SetTXAPreGenToneFreq(channel, 0.0);
            wdsp::SetTXAPreGenRun(channel, 0);
            wdsp::SetTXAPostGenMode(channel, 0);
            wdsp::SetTXAPostGenToneMag(channel, 0.2);
            wdsp::SetTXAPostGenTTMag(channel, 0.2, 0.2);
            wdsp::SetTXAPostGenToneFreq(channel, 0.0);
            wdsp::SetTXAPostGenRun(channel, 0);

            // PureSignal: BUG FIX -- this was never called at all.
            // Confirmed against Thetis's cmaster.cs (SetPSFeedbackRate
            // (txch, ps_rate) with a dedicated ps_rate = 192000,
            // completely separate from TXA's own internal DSP rate).
            // Without it, WDSP's calcc engine keeps using create_calcc's
            // initial "rate" parameter (TXA's own dsp_rate above -- 96000
            // for P2, HALF the real feedback rate) for every internal
            // seconds-to-samples conversion (SetPSMoxDelay/SetPSLoopDelay
            // -> a->ctrl.moxsamps/waitsamps = rate*delay), making those
            // timing windows silently half as long as configured. That's
            // a real, plausible source of "calibration keeps calling
            // itself unstable between attempts" (WDSP's own scheck()
            // rejects a correction table that changed too much from the
            // previous one) -- rushed collection/settling windows would
            // produce noisier, less consistent measurements each cycle.
            // `duc_rate` is exactly right here: it's the same rate
            // psccF's feedback buffers are actually paired/fed at.
            wdsp::SetPSFeedbackRate(channel, duc_rate);

            // PureSignal: enable WDSP's own table-stabilization ("Stbl"
            // in piHPSDR's PS menu, off by default there too -- this
            // project has no UI for it, so it's just turned on
            // unconditionally rather than left at WDSP's default off).
            // Confirmed via real-hardware log analysis: once calibration
            // was otherwise succeeding, the spectrum was still visibly
            // flickering between corrected and uncorrected about once a
            // second. Root cause was calcc.c's scheck() (binfo[6] |=
            // 0x0040), which rejects a newly-computed correction table
            // if it differs from the previous cycle's by more than 5%
            // -- a legitimate noise guard, but with `stbl` off there's
            // no damping between cycles at all, so ordinary cycle-to-
            // cycle measurement noise on real two-tone RF data tripped
            // it on ~73% of attempts in that log, and two rejections in
            // a row forces a full reset (clearing the correction table
            // and dropping TX back to uncorrected) -- exactly the
            // flicker observed. `SetPSStabilize` IIR-blends each new fit
            // toward the previous table (alpha=0.9, calcc.c) before
            // scheck() compares them, which is WDSP's own built-in
            // answer to this, not a workaround.
            wdsp::SetPSStabilize(channel, 1);

            // Panel gain: WDSP's own dedicated mic gain stage. This
            // project previously applied mic_gain by scaling raw
            // sample values itself before ever handing them to WDSP,
            // with this stage never turned on at all -- now uses the
            // stage WDSP actually provides for this, matching the
            // reference's architecture (unit convention kept linear,
            // not dB -- see TxParams::default's note). Started at the
            // TxParams default; process() updates this live on change.
            wdsp::SetTXAPanelGain1(channel, 0.5);
            wdsp::SetTXAPanelRun(channel, 1);

            // FM/AM-specific settings: not exercised by SSB/digital
            // modes, included for completeness/parity with the
            // reference rather than because this project currently
            // supports those modes.
            wdsp::SetTXAFMDeviation(channel, 2500.0);
            wdsp::SetTXAAMCarrierLevel(channel, 0.5);

            wdsp::SetTXACompressorGain(channel, 0.0);
            wdsp::SetTXACompressorRun(channel, 0);

            // TX bandpass passband -- ROOT CAUSE FIX: this was
            // hardcoded to 300-2700Hz regardless of mode/width, which
            // is what actually caused a reported TX power/ALC
            // "bouncing" bug during real WSJT-X FT8 traffic (steady on
            // WSJT-X's own Tune, bouncing on a real CQ). Root-caused
            // via a real hardware meter trace to the bandpass filter's
            // STEADY-STATE (not transient) response -- confirmed by
            // testing attack time and filter phase, neither of which
            // (both transient-related) changed anything, while
            // disabling the filter entirely turned the continuous
            // bounce into a single settling blip. The user's actual
            // setup: DIGU mode, 4000Hz filter width, WSJT-X TX audio
            // frequency 2700Hz -- exactly on this hardcoded filter's
            // upper edge, so FT8's ~50Hz-wide set of 8 tones straddled
            // the edge, each getting a different real gain from the
            // filter's transition band, and WSJT-X cycling through
            // that tone set for the whole transmission produced a
            // continuous, repeating power swing. Now computed with the
            // SAME passband_for(mode, width_hz) RX already uses for
            // RXASetPassband, so TX tracks the user's actual mode/
            // width instead of a fixed guess. Initial value here
            // matches TxParams::default(); process() updates it live
            // on mode/width change, same pattern as mic_gain/mode.
            wdsp::SetTXABandpassFreqs(channel, default_passband.0, default_passband.1);
        }

        // PureSignal: no separate OpenChannel/channel-id needed -- the
        // PS engine (WDSP's calcc) lives inside this SAME TXA channel,
        // auto-created by the OpenChannel call above (confirmed via
        // wdsp/TXA.c: `txa[channel].calcc.p = create_calcc(...)` runs
        // unconditionally as part of TXA setup). Live control (enable/
        // calibrate/HWPeak/delays) is applied per-chunk from
        // apply_ps_params instead of a fixed call here -- see that
        // method's doc comment and Settings -> PureSignal's UI.

        // BUG FIX, confirmed against the reference (rustyHPSDR's
        // Transmitter::new: `output_samples = microphone_buffer_size *
        // (output_rate/sample_rate)`, where sample_rate is always the
        // MIC INPUT rate, never the internal dsp_rate): this buffer's
        // size must scale with duc_rate/mic_rate, not duc_rate/dsp_rate.
        // For Protocol 2 those give different answers (192000/96000=2
        // vs. the correct 192000/48000=4) -- an earlier version of this
        // file used dsp_rate here, sizing iq_scratch at HALF the pairs
        // WDSP's fexchange0 actually writes for this channel's real
        // configured rates (OpenChannel above already passes the true
        // mic_rate/dsp_rate/duc_rate triple, matching the reference
        // exactly -- only this buffer's OWN size calculation, done
        // independently on this side, had the wrong ratio). fexchange0
        // trusts the caller to size its output buffer correctly and
        // does no bounds checking of its own, so this was an actual
        // out-of-bounds write into memory past this Vec's allocation on
        // every single TX audio chunk while transmitting on Protocol 2
        // -- undefined behavior, and a very plausible explanation for a
        // reported wideband/dirty TX signal (a real screenshot
        // comparison against rustyHPSDR's clean single-tone spike on
        // the same Tune test first surfaced this).
        let duc_ratio = ((duc_rate / mic_rate).max(1)) as usize;
        let out_iq_pairs = TX_BUFFER_SIZE * duc_ratio;

        TxProcessor {
            channel,
            mic_scratch: vec![0.0; TX_BUFFER_SIZE * 2],
            iq_scratch: vec![0.0; out_iq_pairs * 2],
            last_mode: None,
            last_gain: None,
            last_passband: Some(default_passband),
            last_eq: None,
            last_post_gen: None,
            last_ps_mox: None,
            last_ps_enabled: None,
            last_ps_oneshot: None,
            // BUG FIX: was `None`, but PsParams::calibrate_request
            // starts at 0, not absent -- `None != Some(0)` is true, so
            // the very first apply_ps_params call spuriously fired a
            // one-shot manual-calibration trigger (SetPSControl's
            // mancal=1, automode=0), UNDOING the enable call's
            // automode=1 moments earlier (both fire in the same call,
            // calibrate_request's check runs second and wins). Matches
            // PsParams::default's own starting value so the first real
            // comparison is a true no-op, only firing on an actual
            // Calibrate Now click.
            last_ps_calibrate_request: Some(0),
            last_ps_hw_peak: None,
            last_ps_mox_delay: None,
            last_ps_loop_delay: None,
            last_ps_tx_delay_ns: None,
            last_ps_ptol: None,
            // Same Some(0)-matches-PsParams::default reasoning as
            // last_ps_calibrate_request above -- both save_corr_request
            // and restore_corr_request also start at 0 in PsParams, so
            // this avoids an equivalent spurious first-apply trigger.
            last_ps_save_corr_request: Some(0),
            last_ps_restore_corr_request: Some(0),
            ps_resend_enabled_countdown: 0,
            ps_corr_path,
        }
    }

    /// mic_samples: TX_BUFFER_SIZE mono samples. Returns interleaved
    /// I/Q f32 TX samples at the DUC rate this channel was opened
    /// with, plus fexchange0's own error code (0 = ok, see call site
    /// below for what nonzero means -- this was previously discarded
    /// entirely, silently hiding a whole class of possible dropout).
    fn process(
        &mut self,
        mic_samples: &[f32],
        mode: Mode,
        mic_gain: f32,
        width_hz: f64,
        tune: bool,
        two_tone: bool,
        eq: EqualizerParams,
    ) -> (Vec<f32>, c_int) {
        debug_assert_eq!(mic_samples.len(), TX_BUFFER_SIZE);

        // Live bandpass-freqs update -- see open()'s SetTXABandpassFreqs
        // comment for the full root-cause story. Same
        // change-detection pattern as last_mode/last_gain below, just
        // keyed on the computed (f_low, f_high) pair instead of the
        // raw mode/width so a width change alone (same mode) still
        // triggers it.
        let passband = crate::spectrum::passband_for(mode, width_hz);
        if self.last_passband != Some(passband) {
            unsafe {
                wdsp::SetTXABandpassFreqs(self.channel, passband.0, passband.1);
                // Tune tone frequency tracks the SAME passband as the
                // bandpass filter -- its midpoint is the right tone
                // for every mode (including CW, which centers on its
                // 600Hz pitch via passband_for's own offset), with no
                // mode-specific sign handling needed unlike
                // rustyHPSDR's set_tuning, since passband_for already
                // encodes that convention.
                wdsp::SetTXAPostGenToneFreq(self.channel, (passband.0 + passband.1) / 2.0);
            }
            self.last_passband = Some(passband);
        }

        // Tune/Two-Tone: WDSP's PostGen generator runs AFTER the
        // ALC/AM/FM stages in xtxa()'s processing order (confirmed in
        // TXA.c), so it overwrites whatever the normal
        // mic->Panel-gain->ALC chain produced -- no interaction with
        // mic input or ALC to worry about, matches rustyHPSDR's own
        // Tune mechanism (set_tuning: ToneMag 0.99999, Mode 0 = Tone,
        // Run toggles the generator on/off).
        //
        // Two-Tone (mode 1, confirmed against WDSP's gen.c) is NOT
        // just an alternative to Tune -- see TxParams::two_tone's doc
        // comment for why PureSignal calibration actually requires it
        // (a steady tone's constant envelope can never fill PS's 16
        // amplitude-bucket collection, so calibration hangs forever
        // regardless of drive/HW Peak/feedback attenuation). Two_tone
        // takes priority if both are somehow set (shouldn't happen --
        // the UI treats them as mutually exclusive). TTMag left at
        // open()'s conservative 0.2/0.2 default rather than Tune's
        // near-max 0.99999 -- two tones summing at max magnitude each
        // would clip.
        let desired = (tune, two_tone);
        if self.last_post_gen != Some(desired) {
            unsafe {
                if two_tone {
                    // BUG FIX: TTMag was never set here at all, so it
                    // stayed at open()'s conservative 0.2/0.2 init value
                    // (peak sum only 0.4, vs Tune's near-max 0.99999)
                    // forever -- confirmed via real hardware testing:
                    // ~0W output with Two-Tone vs ~100W with Tune at
                    // the same TX Power, and a correspondingly tiny
                    // measured envelope (0.0198 vs an expected several
                    // times that). 0.45/0.45 (peak sum 0.9) mirrors
                    // Tune's "near max but leave a little headroom"
                    // choice, adapted for two tones summing instead of
                    // one.
                    //
                    // TRIED AND REVERTED: unequal magnitudes (0.55/0.35)
                    // to avoid the exact-zero envelope null equal
                    // magnitudes produce, on the theory that noise-floor
                    // samples at the null were causing WDSP's scheck()
                    // info[6] bit 0x0010 ("fitted curve dips negative").
                    // Confirmed WRONG on real P2 hardware: unequal
                    // magnitudes never let the envelope reach near-zero
                    // at all, so PS's lowest amplitude bucket never
                    // fills and calibration collection (state=LCOLLECT)
                    // never completes even once -- worse than the
                    // original problem. Equal magnitudes are required
                    // for calibration to complete at all; the 0x0010
                    // cause is still unresolved (see the PureSignal plan
                    // doc's real-hardware-findings section).
                    //
                    // BUG FIX: SetTXAPostGenTTFreq was never called at
                    // all, meaning the two tones ran at whatever WDSP's
                    // internal default/uninitialized frequencies are --
                    // NOT a real, properly-spread two-tone test signal.
                    // Confirmed via piHPSDR's tx_set_twotone
                    // (transmitter.c): it explicitly sets 900/1700 Hz
                    // (negated for LSB-ish modes) before enabling. Magnitude
                    // also nudged from 0.45 to piHPSDR's own 0.49 for a
                    // full match while chasing the remaining PS
                    // calibration issue on P1/Orion2 -- see the plan
                    // doc's real-hardware-findings section.
                    let (f1, f2) = match mode {
                        Mode::Cwl | Mode::Lsb | Mode::Digl => (-900.0, -1700.0),
                        _ => (900.0, 1700.0),
                    };
                    wdsp::SetTXAPostGenTTFreq(self.channel, f1, f2);
                    wdsp::SetTXAPostGenTTMag(self.channel, 0.49, 0.49);
                    wdsp::SetTXAPostGenMode(self.channel, 1);
                    wdsp::SetTXAPostGenRun(self.channel, 1);
                } else if tune {
                    wdsp::SetTXAPostGenMode(self.channel, 0);
                    wdsp::SetTXAPostGenToneMag(self.channel, 0.99999);
                    wdsp::SetTXAPostGenRun(self.channel, 1);
                } else {
                    wdsp::SetTXAPostGenRun(self.channel, 0);
                }
            }
            self.last_post_gen = Some(desired);
        }

        if self.last_mode != Some(mode) {
            unsafe {
                wdsp::SetTXAMode(self.channel, mode as c_int);
            }
            self.last_mode = Some(mode);
        }

        if self.last_gain != Some(mic_gain) {
            unsafe {
                wdsp::SetTXAPanelGain1(self.channel, mic_gain as f64);
            }
            self.last_gain = Some(mic_gain);
        }

        // Graphic EQ -- see spectrum::EqualizerParams's doc comment and
        // the RX-side equivalent in spectrum.rs's demod() for the shared
        // reasoning (same WDSP band layouts, same piHPSDR-matching
        // coefficients-then-Run sequence).
        if self.last_eq != Some(eq) {
            unsafe {
                match eq.band_count {
                    EqBandCount::Three => {
                        let mut coeffs = [eq.preamp_db, eq.bands_3_db[0], eq.bands_3_db[1], eq.bands_3_db[2]];
                        wdsp::SetTXAGrphEQ(self.channel, coeffs.as_mut_ptr());
                    }
                    EqBandCount::Ten => {
                        let mut coeffs = [0i32; 11];
                        coeffs[0] = eq.preamp_db;
                        coeffs[1..11].copy_from_slice(&eq.bands_10_db);
                        wdsp::SetTXAGrphEQ10(self.channel, coeffs.as_mut_ptr());
                    }
                }
                wdsp::SetTXAEQRun(self.channel, eq.enabled as c_int);
            }
            self.last_eq = Some(eq);
        }

        // Confirmed against the reference: real mono mic sample in the
        // first slot of each interleaved pair, zero in the second (a
        // real-valued signal presented as a complex one with zero
        // quadrature component) -- NOT duplicating the sample into
        // both slots as an earlier, unconfirmed version of this file
        // did. Gain is no longer applied here at all -- that's now
        // entirely WDSP's Panel gain stage's job, set above.
        for (i, &s) in mic_samples.iter().enumerate() {
            self.mic_scratch[i * 2] = s as f64;
            self.mic_scratch[i * 2 + 1] = 0.0;
        }

        let mut error: c_int = 0;
        unsafe {
            wdsp::fexchange0(
                self.channel,
                self.mic_scratch.as_mut_ptr(),
                self.iq_scratch.as_mut_ptr(),
                &mut error,
            );
        }

        (self.iq_scratch.iter().map(|&v| v as f32).collect(), error)
    }

    fn meter(&self, mt: u32) -> f64 {
        unsafe { wdsp::GetTXAMeter(self.channel, mt as c_int) }
    }

    /// PureSignal: tells WDSP the current PTT state -- confirmed
    /// against Thetis (console.cs), which calls this on EVERY MOX
    /// transition (TX and RX), not just once at setup. Edge-triggered
    /// (only calls into WDSP when the state actually changes) as a
    /// cheap FFI-call optimization, same pattern as last_mode/
    /// last_gain elsewhere in this struct -- not a correctness
    /// requirement.
    fn set_ps_mox(&mut self, mox_on: bool) {
        if self.last_ps_mox != Some(mox_on) {
            unsafe {
                wdsp::SetPSMox(self.channel, mox_on as c_int);
            }
            self.last_ps_mox = Some(mox_on);
        }
    }

    /// PureSignal: applies Settings -> PureSignal's live controls,
    /// each edge-triggered against its own cached last-value (same
    /// pattern as last_mode/last_gain elsewhere in this struct) so an
    /// unchanged control costs nothing beyond the comparison. `enabled`
    /// toggling maps to the confirmed Thetis/piHPSDR call patterns:
    /// `SetPSControl(ch,0,1,0,0)` (single manual calibration -- see
    /// PsParams::oneshot's doc comment) or `SetPSControl(ch,0,0,1,0)`
    /// (continuous auto-calibrate) when true, depending on `oneshot`;
    /// `SetPSControl(ch,1,0,0,0)` (reset/off) when false.
    /// `calibrate_request` is a monotonic counter rather than a plain
    /// bool specifically so a click is never missed/coalesced with
    /// another change in the same chunk -- any change in the counter
    /// (not just "became true") triggers one single-shot manual
    /// calibration (`SetPSControl(ch,1,1,0,0)`).
    fn apply_ps_params(&mut self, ps: &PsParams) {
        if self.last_ps_enabled != Some(ps.enabled) {
            unsafe {
                if ps.enabled {
                    if ps.oneshot {
                        wdsp::SetPSControl(self.channel, 0, 1, 0, 0);
                    } else {
                        wdsp::SetPSControl(self.channel, 0, 0, 1, 0);
                    }
                } else {
                    wdsp::SetPSControl(self.channel, 1, 0, 0, 0);
                }
            }
            self.last_ps_enabled = Some(ps.enabled);
            self.last_ps_oneshot = Some(ps.oneshot);
        } else if self.last_ps_oneshot != Some(ps.oneshot) {
            // Mode switched while already enabled -- matches piHPSDR's
            // ps_off_on (tx_ps_reset then tx_ps_resume): reset first so
            // the state machine doesn't try to carry over
            // mancal/automode state from the old mode, then resend the
            // correct resume command via the same short-window resend
            // mechanism restore_corr_request uses below (the state-
            // machine tick that processes the reset runs on WDSP's own
            // audio-chunk cadence, not synchronously with this call).
            if ps.enabled {
                unsafe {
                    wdsp::SetPSControl(self.channel, 1, 0, 0, 0);
                }
                self.ps_resend_enabled_countdown = 50; // ~530ms at TX_BUFFER_SIZE/48kHz
            }
            self.last_ps_oneshot = Some(ps.oneshot);
        }
        if self.last_ps_calibrate_request != Some(ps.calibrate_request) {
            unsafe {
                wdsp::SetPSControl(self.channel, 1, 1, 0, 0);
            }
            self.last_ps_calibrate_request = Some(ps.calibrate_request);
        }
        if self.last_ps_hw_peak != Some(ps.hw_peak) {
            unsafe {
                wdsp::SetPSHWPeak(self.channel, ps.hw_peak);
            }
            self.last_ps_hw_peak = Some(ps.hw_peak);
        }
        if self.last_ps_mox_delay != Some(ps.mox_delay) {
            unsafe {
                wdsp::SetPSMoxDelay(self.channel, ps.mox_delay);
            }
            self.last_ps_mox_delay = Some(ps.mox_delay);
        }
        if self.last_ps_loop_delay != Some(ps.loop_delay) {
            unsafe {
                wdsp::SetPSLoopDelay(self.channel, ps.loop_delay);
            }
            self.last_ps_loop_delay = Some(ps.loop_delay);
        }
        if self.last_ps_tx_delay_ns != Some(ps.tx_delay_ns) {
            unsafe {
                // WDSP's SetPSTXDelay takes seconds, not nanoseconds --
                // confirmed against piHPSDR/Thetis, both of which
                // expose this to the user in ns (a PA/relay group delay
                // is naturally a small ns-scale number) but convert
                // before the call.
                wdsp::SetPSTXDelay(self.channel, ps.tx_delay_ns * 1e-9);
            }
            self.last_ps_tx_delay_ns = Some(ps.tx_delay_ns);
        }
        if self.last_ps_ptol != Some(ps.ptol) {
            unsafe {
                wdsp::SetPSPtol(self.channel, ps.ptol);
            }
            self.last_ps_ptol = Some(ps.ptol);
        }
        if self.last_ps_save_corr_request != Some(ps.save_corr_request) {
            self.ps_corr_action(wdsp::PSSaveCorr as PsCorrFn);
            self.last_ps_save_corr_request = Some(ps.save_corr_request);
        }
        if self.last_ps_restore_corr_request != Some(ps.restore_corr_request) {
            self.ps_corr_action(wdsp::PSRestoreCorr as PsCorrFn);
            self.last_ps_restore_corr_request = Some(ps.restore_corr_request);
            // See ps_resend_enabled_countdown's doc comment -- only
            // needed to re-assert continuous auto-calibrate. In OneShot
            // mode, PSRestoreCorr's own LTURNON handling already leaves
            // WDSP exactly where OneShot wants it (LSTAYON: apply the
            // restored table, don't keep relearning) -- resending
            // mancal=1 here would instead force a fresh, unwanted
            // recalibration attempt right after every restore.
            if !ps.oneshot {
                self.ps_resend_enabled_countdown = 50; // ~530ms at TX_BUFFER_SIZE/48kHz
            }
        }
        if self.ps_resend_enabled_countdown > 0 {
            self.ps_resend_enabled_countdown -= 1;
            if ps.enabled {
                unsafe {
                    if ps.oneshot {
                        wdsp::SetPSControl(self.channel, 0, 1, 0, 0);
                    } else {
                        wdsp::SetPSControl(self.channel, 0, 0, 1, 0);
                    }
                }
            }
        }
    }

    /// Shared body for the save/restore edge-triggers above -- both
    /// take the same (channel, *mut c_char) shape, differing only in
    /// which WDSP function to call. No-ops (doesn't call into WDSP at
    /// all) if `ps_corr_path` is None (e.g. $HOME unset at connect
    /// time) rather than attempting a call with a made-up path.
    fn ps_corr_action(&self, f: PsCorrFn) {
        let Some(path) = &self.ps_corr_path else { return };
        let Some(path_str) = path.to_str() else { return };
        let Ok(c_path) = CString::new(path_str) else { return };
        // into_raw (not as_ptr): calcc.c's PSSaveCorr/PSRestoreCorr copy
        // the filename into their own fixed-size internal buffer
        // (`while (a->util.savefile[i++] = *filename++);`) synchronously
        // before returning -- confirmed via source, not just assumed --
        // so a short-lived pointer from as_ptr() would already be valid
        // for the whole call. into_raw()+from_raw() here anyway, purely
        // to make the ownership/cleanup explicit rather than relying on
        // CString's Drop timing relative to the FFI call.
        let ptr = c_path.into_raw();
        unsafe {
            f(self.channel, ptr);
            drop(CString::from_raw(ptr));
        }
    }

    /// PureSignal: reads back live status for the UI (Settings ->
    /// PureSignal's feedback-level meter, Correcting indicator, and
    /// Get Peak readout) -- confirmed against Thetis/piHPSDR's own
    /// polling of the same three values. `GetPSInfo` fills a 16-int
    /// array; only indices 4 (feedback level) and 14 (correcting flag)
    /// are interpreted anywhere in either reference, the rest are raw
    /// diagnostic counters neither app gives semantic meaning to.
    fn read_ps_status(&self) -> PsStatus {
        let mut info = [0i32; 16];
        let mut max_tx: f64 = 0.0;
        unsafe {
            wdsp::GetPSInfo(self.channel, info.as_mut_ptr());
            wdsp::GetPSMaxTX(self.channel, &mut max_tx);
        }
        PsStatus {
            feedback_level: info[4],
            correcting: info[14] != 0,
            max_tx,
        }
    }

    /// PureSignal: feeds one chunk of forward (tx) and feedback (rx)
    /// IQ into WDSP's calcc engine. `size` is the pair count -- all
    /// four slices must be exactly this long. See tx.rs's module note
    /// and the PureSignal plan for the confidence caveats on this
    /// whole exchange (no confirmed-working reference for the exact
    /// call cadence, unlike the rest of this file's WDSP calls).
    /// `solidmox` (WDSP: whether MOX has been continuously asserted
    /// long enough to be considered "solid" rather than a transient
    /// key-up) is passed the same as `mox` -- this project has no
    /// separate debounce/hang-time tracking for that distinction yet.
    #[allow(clippy::too_many_arguments)]
    fn feed_ps(&self, itx: &mut [f32], qtx: &mut [f32], irx: &mut [f32], qrx: &mut [f32], mox_on: bool) {
        let size = itx.len() as c_int;
        unsafe {
            wdsp::psccF(
                self.channel,
                size,
                itx.as_mut_ptr(),
                qtx.as_mut_ptr(),
                irx.as_mut_ptr(),
                qrx.as_mut_ptr(),
                mox_on as c_int,
                mox_on as c_int,
            );
        }
    }
}

impl Drop for TxProcessor {
    fn drop(&mut self) {
        unsafe {
            wdsp::CloseChannel(self.channel);
        }
    }
}

/// PureSignal: drains exactly `pairs` matching pairs from the
/// TX-feedback and RX-feedback queues, normalized to [-1.0, 1.0], 1:1
/// (no decimation -- see this module's top doc comment for a real bug
/// this function used to have, an unnecessary 2:1 RX-feedback
/// averaging that suppressed Feedback Level). Returns None (leaving
/// both queues completely untouched) if either doesn't have enough
/// data yet -- feedback arrives with real network latency behind the
/// corresponding TX audio it's paired with here, so an empty/partial
/// queue is the normal case right after PTT, not an error condition.
fn drain_ps_feedback(
    tx_feedback: &Mutex<VecDeque<IqSample>>,
    rx_feedback: &Mutex<VecDeque<IqSample>>,
    pairs: usize,
) -> Option<(Vec<f32>, Vec<f32>, Vec<f32>, Vec<f32>)> {
    let mut tx_q = tx_feedback.lock().unwrap();
    let mut rx_q = rx_feedback.lock().unwrap();
    if tx_q.len() < pairs || rx_q.len() < pairs {
        return None;
    }

    let mut itx = Vec::with_capacity(pairs);
    let mut qtx = Vec::with_capacity(pairs);
    for _ in 0..pairs {
        let s = tx_q.pop_front().unwrap();
        itx.push(s.i as f32 / PS_IQ_NORM);
        qtx.push(s.q as f32 / PS_IQ_NORM);
    }
    drop(tx_q);

    let mut irx = Vec::with_capacity(pairs);
    let mut qrx = Vec::with_capacity(pairs);
    for _ in 0..pairs {
        let s = rx_q.pop_front().unwrap();
        irx.push(s.i as f32 / PS_IQ_NORM);
        qrx.push(s.q as f32 / PS_IQ_NORM);
    }
    drop(rx_q);

    Some((itx, qtx, irx, qrx))
}

#[allow(clippy::too_many_arguments)]
fn run(
    mic_buffer: Arc<Mutex<VecDeque<f32>>>,
    tci_tx_audio: Arc<Mutex<VecDeque<f32>>>,
    // Radio's own mic input (Settings -> TX -- "TX audio source: Radio
    // Mic") -- see radio.rs's RadioSession::radio_mic_audio/
    // tx_audio_source doc comments. An either/or/or SOURCE SELECTION,
    // not another priority tier alongside tci_tx_audio/mic_buffer: the
    // TX_AUDIO_SOURCE_* value below picks exactly one source, and only
    // that one is consulted for the whole chunk.
    radio_mic_audio: Arc<Mutex<VecDeque<f32>>>,
    tx_audio_source: Arc<AtomicU8>,
    // Set by tci.rs's `trx` command handler from that command's optional
    // signal-source argument (spec section 4.2) -- true only when a TCI
    // client explicitly names a non-"tci" source. Consulted only by the
    // Auto branch below; see radio.rs's RadioSession::tci_wants_mic doc
    // comment for the full reasoning.
    tci_wants_mic: Arc<AtomicBool>,
    tx_iq_out: Arc<Mutex<VecDeque<f32>>>,
    // Fed with the SAME generated TX IQ that goes out over the wire
    // (tx_iq_out), for a dedicated TX spectrum display -- see main.rs's
    // ConnectedState::tx_spectrum doc comment for why this exists:
    // piHPSDR/rustyHPSDR both feed their TX spectrum from the actual
    // generated IQ samples, not from whatever the receiver happens to
    // pick up over the air (which depends on antenna coupling/leakage
    // and can be weak, overloaded, or entirely absent depending on the
    // radio's T/R relay isolation -- not a reliable "am I transmitting
    // cleanly" signal at all).
    tx_spectrum_iq: Arc<Mutex<VecDeque<IqSample>>>,
    // See TxHandle::tx_audio_monitor's doc comment. Fed unconditionally
    // (cheap -- a bounded ring buffer nobody reads from just sits there
    // and gets capacity-trimmed like any other unused tap) so main.rs
    // can start/stop actually listening to it at any time without a
    // reconnect.
    tx_audio_monitor: Arc<Mutex<VecDeque<f32>>>,
    mox: Arc<AtomicBool>,
    params: Arc<Mutex<TxParams>>,
    display: Arc<Mutex<TxDisplay>>,
    channel: i32,
    protocol: u8,
    mic_rate: i32,
    duc_rate: i32,
    // Live -- see RadioSession::puresignal_enabled's doc comment (radio.rs)
    // for the full story on why this is now an Arc<AtomicBool> read fresh
    // each chunk instead of a bool baked in for this thread's lifetime.
    puresignal_enabled: Arc<AtomicBool>,
    ps_rx_feedback_iq: Arc<Mutex<VecDeque<IqSample>>>,
    ps_tx_feedback_iq: Arc<Mutex<VecDeque<IqSample>>>,
    ps_params: Arc<Mutex<PsParams>>,
    ps_status: Arc<Mutex<PsStatus>>,
    ps_corr_path: Option<PathBuf>,
    stop: Arc<AtomicBool>,
) {
    let mut processor = TxProcessor::open(channel, protocol, mic_rate, duc_rate, ps_corr_path);
    let mut chunk = vec![0.0f32; TX_BUFFER_SIZE];
    // Real-time duration of one TX_BUFFER_SIZE chunk at the mic capture
    // rate -- e.g. ~11ms for 512 samples at 48kHz. Without this, the
    // loop below has no pacing at all and spins as fast as the CPU
    // allows, draining the mic buffer far faster than cpal's real-time
    // audio callback can actually fill it -- so it finds the buffer
    // empty (silence-padded via unwrap_or(0.0)) on nearly every
    // iteration, with only an occasional real burst getting through
    // right when the loop happens to catch up to the callback. Mirrors
    // the same real-time pacing p2_tx_iq_loop already uses for its own
    // output rate.
    let chunk_interval = Duration::from_secs_f64(TX_BUFFER_SIZE as f64 / mic_rate as f64);

    // Diagnostic only -- added to help pin down reports of TX output
    // power bouncing between the expected level and 0W on a steady
    // carrier/tone. That symptom is consistent with mic_buffer running
    // dry here (each starved chunk gets silence-padded below, which for
    // SSB means real near-zero RF output, not just a display artifact)
    // -- most likely because audio.rs's MicInput hardcodes a 48kHz mono
    // capture request that isn't independently confirmed to match
    // whatever device is actually feeding it (see that module's doc
    // comment). Logged as a once-per-second summary (not per-chunk,
    // which could be ~90/s and flood stderr) so it's cheap to leave in.
    let mut starve_window_start = Instant::now();
    let mut starved_chunks_this_window: u32 = 0;
    let mut chunks_this_window: u32 = 0;

    // Second diagnostic, added after the mic-buffer one above didn't
    // reproduce anything (no underruns logged, yet power/ALC still
    // bounced): fexchange0's `error` output was being silently
    // discarded entirely. Per WDSP's iobuffs.c, fexchange0 is NOT a
    // direct passthrough -- it's a producer/consumer ring buffer
    // (r1 in / r2 out) serviced by WDSP's OWN internal worker thread,
    // completely separate from this loop's own pacing. If that
    // internal thread ever falls behind (e.g. under the kind of
    // system-wide CPU pressure WSJT-X's own decoding/waterfall
    // rendering can cause, vs. the near-idle system a bare Tune press
    // sees), fexchange0 hands back a ZERO-FILLED output buffer
    // (memset) and sets error to a nonzero value (-2 for "not enough
    // processed output ready") -- real silence inserted into the TX
    // IQ stream, invisible to the mic-buffer check above since it
    // happens entirely inside WDSP, downstream of anything this loop
    // can see. This would explain the exact reported shape: steady on
    // a continuous Tune, bouncing on real WSJT-X CQ traffic (which
    // runs alongside far more concurrent system load).
    let mut exch_error_window_start = Instant::now();
    let mut exch_errors_this_window: u32 = 0;

    // Absolute-deadline pacing, not `thread::sleep(chunk_interval)` after
    // every iteration (which this loop used until now). Same fix, same
    // reasoning, as p2_tx_iq_loop's own next_send -- see its doc comment
    // for the full explanation. This loop is the actual PRODUCER feeding
    // that one's tx_iq queue; relative sleep-based pacing here lets any
    // one iteration's jitter (WDSP processing time, mutex contention,
    // OS scheduling) push every later chunk's real production time
    // later too, rather than being corrected on the next iteration --
    // which starves that downstream queue exactly like drifting sender
    // pacing was already confirmed to spur the RF output on the
    // consumer side.
    let mut next_chunk = Instant::now();
    // PureSignal: when MOX went active most recently -- None while
    // idle. See below for why this matters.
    let mut mox_active_since: Option<Instant> = None;
    // piHPSDR's transmitter.c documents a real ordering requirement
    // that isn't optional: "enabling should restart feedback streams
    // first, wait ~100ms, then turn PS on" -- ROOT CAUSE FIX for
    // "feedback level stays at 0" despite real, strong feedback signal
    // confirmed reaching WDSP (verified via a real-hardware test: raw
    // feedback amplitude hit 3-4 million out of a possible 8388607
    // once transmitting). An earlier version of this loop called
    // apply_ps_params's SetPSControl enable in the SAME chunk as
    // set_ps_mox(true), before any real feedback data could possibly
    // have arrived yet -- plausibly latching WDSP's internal PS state
    // machine into a stuck state it never recovers from even once real
    // feedback starts flowing moments later, since nothing here ever
    // retries the enable call once already sent (apply_ps_params is
    // edge-triggered on ps.enabled's value, not on time or on whether
    // the previous attempt actually took).
    const PS_ENABLE_SETTLE: Duration = Duration::from_millis(100);

    while !stop.load(Ordering::Relaxed) {
        if !mox.load(Ordering::Relaxed) {
            // Not transmitting -- drop any mic/TCI audio that
            // accumulated while idle so the next PTT doesn't start by
            // replaying a backlog of stale audio, and don't burn CPU
            // running the TXA chain on nothing.
            mic_buffer.lock().unwrap().clear();
            tci_tx_audio.lock().unwrap().clear();
            radio_mic_audio.lock().unwrap().clear();
            if puresignal_enabled.load(Ordering::Relaxed) {
                // Same reasoning as the mic/TCI clears above -- a
                // feedback backlog from before this idle period is
                // stale by the time the next PTT starts.
                ps_rx_feedback_iq.lock().unwrap().clear();
                ps_tx_feedback_iq.lock().unwrap().clear();
                processor.set_ps_mox(false);
            }
            mox_active_since = None;
            thread::sleep(Duration::from_millis(20));
            // Resync so the first chunk after PTT is produced against a
            // fresh schedule, not delayed by however long MOX was off --
            // same reasoning as p2_tx_iq_loop's own resync here.
            next_chunk = Instant::now();
            continue;
        }
        if puresignal_enabled.load(Ordering::Relaxed) {
            // set_ps_mox(true) is what (re)starts feedback streaming --
            // send it as soon as MOX goes active, same as before. Only
            // apply_ps_params (which can turn PS ON) waits for the
            // settle delay above.
            processor.set_ps_mox(true);
            let settled_since = *mox_active_since.get_or_insert_with(Instant::now);
            if settled_since.elapsed() >= PS_ENABLE_SETTLE {
                processor.apply_ps_params(&ps_params.lock().unwrap());
            }
        }

        let selected_source = tx_audio_source.load(Ordering::Relaxed);
        if selected_source == TX_AUDIO_SOURCE_RADIO_MIC {
            // Explicit source selection (Settings -> TX) -- bypasses
            // TCI/local mic entirely while selected, rather than adding
            // an implicit-priority tier alongside them. See radio.rs's
            // RadioSession::tx_audio_source doc comment.
            let mut buf = radio_mic_audio.lock().unwrap();
            if buf.len() < TX_BUFFER_SIZE {
                starved_chunks_this_window += 1;
            }
            for slot in chunk.iter_mut() {
                *slot = buf.pop_front().unwrap_or(0.0); // silence on underrun, not silence on stall
            }
        } else if selected_source == TX_AUDIO_SOURCE_LOCAL_MIC {
            // Explicit source selection -- bypasses tci_tx_audio
            // ENTIRELY, unlike Auto's fallback below (which only
            // switches to mic_buffer on a TCI underrun/no-client).
            // Added specifically for a confirmed real bug in WSJT-X's
            // own TCI audio generation (TCITransceiver.cpp's tx_fifo
            // ring buffer resends stale content on roughly 1-in-8
            // messages -- confirmed via source, and by ear: a real
            // side-by-side recording of the SAME transmission was
            // clean via local mic/pipewire, audibly rough via TCI) --
            // this lets a TCI client keep driving frequency/mode/PTT
            // while using local mic input (e.g. the TCI client's own
            // audio output looped back via pipewire) for TX audio
            // instead of its broken TCI audio path.
            let mut buf = mic_buffer.lock().unwrap();
            if buf.len() < TX_BUFFER_SIZE {
                starved_chunks_this_window += 1;
            }
            for slot in chunk.iter_mut() {
                *slot = buf.pop_front().unwrap_or(0.0); // silence on underrun, not silence on stall
            }
        } else {
            // Auto (default): TCI-sourced audio takes priority over the
            // local mic --
            // see radio.rs's tci_tx_audio doc comment for why this is
            // a separate queue rather than both sources feeding
            // mic_buffer directly (would interleave/garble if both
            // were ever active at once). No TCI client sending audio
            // at all -> this queue stays completely empty -> falls
            // through to mic_buffer exactly as before, zero behavior
            // change for local-mic operation.
            //
            // BUG FIX: this used to require a FULL chunk's worth
            // (tci_buf.len() >= TX_BUFFER_SIZE) before touching this
            // queue at all, falling through to mic_buffer on ANY
            // shortfall -- indistinguishable from "no TCI client at
            // all" only when a TCI client's real content is well
            // ahead of consumption. A real report of WSJT-X's TX
            // audio producing a wide/noisy TX spectrum persisted
            // through ruling out audio-content corruption, gain/level,
            // and PTT-triggering (an A/B test with TX audio source
            // switched to the radio's own mic, still PTT'd via TCI,
            // came out clean) -- pointing at this fallback itself:
            // WSJT-X's real content has frequent natural gaps (its own
            // TxChrono/ring-buffer pacing isn't perfectly smooth), so
            // ordinary mid-stream shortfalls here were silently
            // pulling from mic_buffer instead -- i.e. splicing in
            // whatever the PC's live local mic is picking up (room
            // noise, breathing, anything), unrelated in timing/level/
            // phase to the TCI content, on top of an active TCI
            // session. That's a highly plausible source of broadband
            // splatter that none of the TCI-audio-focused fixes above
            // could have touched. Emptiness (not fullness) is now the
            // "is a TCI client actually active" signal -- a non-empty
            // but short queue drains what it has and silence-pads the
            // rest, same "silence on underrun, not silence on stall"
            // policy the radio-mic branch above already uses.
            // Spec section 4.2 (TRX command): a client can explicitly
            // name a non-TCI signal source (mic1/mic2/micPC/ecoder2) for
            // this transmission -- when it does, tci_tx_audio is skipped
            // even if it currently has content, same as if no TCI client
            // were sending audio at all. See tci_wants_mic's doc comment
            // above for why an absent/​"tci" source does NOT flip this
            // the other way.
            let mut tci_buf = tci_tx_audio.lock().unwrap();
            if !tci_wants_mic.load(Ordering::Relaxed) && !tci_buf.is_empty() {
                if tci_buf.len() < TX_BUFFER_SIZE {
                    starved_chunks_this_window += 1;
                }
                for slot in chunk.iter_mut() {
                    *slot = tci_buf.pop_front().unwrap_or(0.0);
                }
            } else {
                drop(tci_buf);
                let mut buf = mic_buffer.lock().unwrap();
                if buf.len() < TX_BUFFER_SIZE {
                    starved_chunks_this_window += 1;
                }
                for slot in chunk.iter_mut() {
                    *slot = buf.pop_front().unwrap_or(0.0); // silence on underrun, not silence on stall
                }
            }
        }
        // TX audio monitor tap -- the exact content about to be fed to
        // WDSP's fexchange0 (post source-selection, pre-processing), so
        // listening to this queue reveals whether a problem is already
        // present in the source audio or introduced downstream. See
        // TxHandle::tx_audio_monitor's doc comment.
        {
            let mut mon = tx_audio_monitor.lock().unwrap();
            for &sample in chunk.iter() {
                if mon.len() >= TX_AUDIO_MONITOR_CAPACITY {
                    mon.pop_front();
                }
                mon.push_back(sample);
            }
        }
        chunks_this_window += 1;
        if starve_window_start.elapsed() >= Duration::from_secs(1) {
            if starved_chunks_this_window > 0 {
                eprintln!(
                    "tx: mic buffer underrun on {starved_chunks_this_window}/{chunks_this_window} \
                     chunks in the last second -- real silence (0W on SSB) went out during those; \
                     see audio.rs's MicInput doc comment if this is frequent"
                );
            }
            starve_window_start = Instant::now();
            starved_chunks_this_window = 0;
            chunks_this_window = 0;
        }

        let p = *params.lock().unwrap();
        let (iq, exch_error) =
            processor.process(&chunk, p.mode, p.mic_gain, p.width_hz, p.tune, p.two_tone, p.eq);

        if exch_error != 0 {
            exch_errors_this_window += 1;
        }
        if exch_error_window_start.elapsed() >= Duration::from_secs(1) {
            if exch_errors_this_window > 0 {
                eprintln!(
                    "tx: fexchange0 returned a nonzero error on {exch_errors_this_window} chunks \
                     in the last second -- WDSP's own internal TXA worker thread fell behind and \
                     substituted real silence into the TX IQ output (see tx.rs's run() comment)"
                );
            }
            exch_error_window_start = Instant::now();
            exch_errors_this_window = 0;
        }

        if puresignal_enabled.load(Ordering::Relaxed) {
            // Pairs, not raw floats -- iq is interleaved I,Q,I,Q,...
            let pairs_needed = iq.len() / 2;
            if let Some((mut itx, mut qtx, mut irx, mut qrx)) =
                drain_ps_feedback(&ps_tx_feedback_iq, &ps_rx_feedback_iq, pairs_needed)
            {
                processor.feed_ps(&mut itx, &mut qtx, &mut irx, &mut qrx, true);
            }
            // else: feedback hasn't caught up yet (real network latency
            // behind this chunk's TX audio, most likely right after
            // PTT) -- skip this chunk's PS feed rather than stall the
            // real-time audio loop waiting for it.

            // Read back regardless of whether this chunk's feed
            // succeeded above -- reflects WDSP's own ongoing internal
            // state, not just this one call. Settings -> PureSignal
            // reads this live (feedback level, Correcting, measured
            // peak TX) -- no separate console diagnostic needed on top
            // of that UI.
            *ps_status.lock().unwrap() = processor.read_ps_status();
        }

        {
            let mut spec = tx_spectrum_iq.lock().unwrap();
            for pair in iq.chunks_exact(2) {
                if spec.len() >= TX_SPECTRUM_IQ_CAPACITY {
                    spec.pop_front();
                }
                spec.push_back(IqSample {
                    i: (pair[0] * PS_IQ_NORM) as i32,
                    q: (pair[1] * PS_IQ_NORM) as i32,
                });
            }
        }

        {
            let mut out = tx_iq_out.lock().unwrap();
            for v in iq {
                if out.len() >= TX_IQ_BUFFER_CAPACITY {
                    out.pop_front();
                }
                out.push_back(v);
            }
        }

        let mic_pk = processor.meter(wdsp::txaMeterType_TXA_MIC_PK);
        let alc_av = processor.meter(wdsp::txaMeterType_TXA_ALC_AV);
        *display.lock().unwrap() = TxDisplay { mic_pk, alc_av };

        next_chunk += chunk_interval;
        let now = Instant::now();
        if next_chunk > now {
            thread::sleep(next_chunk - now);
        } else {
            // Fell behind real time -- resync to now rather than
            // bursting several chunks back-to-back to "catch up", same
            // reasoning as p2_tx_iq_loop's own fallback.
            next_chunk = now;
        }
    }
}

/// Owns the background TXA thread. `tx_iq_out` is shared with whichever
/// radio.rs sender loop(s) are streaming TX IQ to the radio -- owned by
/// RadioSession, passed in here rather than created internally, same
/// pattern as SpectrumHandle::start's iq_buffer parameter.
pub struct TxHandle {
    pub display: Arc<Mutex<TxDisplay>>,
    /// PureSignal live status (feedback level, correcting, measured
    /// peak) -- see PsStatus's field docs. Only meaningful when this
    /// session was connected with PureSignal's feedback plumbing
    /// active; otherwise stays at its Default (all zero/false).
    pub ps_status: Arc<Mutex<PsStatus>>,
    /// TX audio monitor tap -- the exact audio about to be fed to
    /// WDSP's fexchange0 (post source-selection between mic/TCI/radio-
    /// mic, pre-processing), continuously filled whenever transmitting
    /// regardless of whether anything is reading from it. Added as a
    /// diagnostic while chasing a real report of TCI-sourced TX audio
    /// producing splatter/no-decode: play this back locally (e.g.
    /// `AudioOutput::start(tx_handle.tx_audio_monitor.clone())`) to
    /// hear exactly what's reaching WDSP, which distinguishes "already
    /// wrong in the source audio" from "introduced downstream in
    /// hpsdr-rs's own processing".
    pub tx_audio_monitor: Arc<Mutex<VecDeque<f32>>>,
    /// Live -- see RadioSession::puresignal_enabled's doc comment
    /// (radio.rs) for the full story. Mirrors that same flag on the TX
    /// side: WDSP's PS engine is always created (TxProcessor::open, see
    /// its own doc comment), this just controls whether run()'s per-chunk
    /// loop actually drives it (SetPSMox/SetPSControl/feed_ps) or leaves
    /// it idle. Set via TxHandle::set_puresignal_enabled -- normally in
    /// lockstep with RadioSession::set_puresignal_enabled (main.rs's PS
    /// checkbox calls both).
    puresignal_enabled: Arc<AtomicBool>,
    params: Arc<Mutex<TxParams>>,
    ps_params: Arc<Mutex<PsParams>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl TxHandle {
    /// `channel` is the WDSP channel id to open the TXA chain on --
    /// see the module note above TxProcessor::open on why this is a
    /// caller-supplied parameter (derived from the actual RX channel
    /// count) rather than a hardcoded constant. `protocol` (1 or 2)
    /// determines the internal TXA DSP rate (confirmed against the
    /// reference: 96000 for P2, 48000 for P1) and the CFIR filter
    /// setting. `mic_rate` is MicInput's capture rate (audio.rs's
    /// INPUT_SAMPLE_RATE). `duc_rate` is the rate TX IQ should be
    /// produced at -- 192000 for Protocol 2 (matches radio.rs's DUC
    /// packet stub), or the session's current RX sample rate for
    /// Protocol 1 (which has one shared ADC/DAC clock, no separate DUC
    /// concept).
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        mic_buffer: Arc<Mutex<VecDeque<f32>>>,
        tci_tx_audio: Arc<Mutex<VecDeque<f32>>>,
        // See run()'s doc comment on the parameters of the same name.
        radio_mic_audio: Arc<Mutex<VecDeque<f32>>>,
        tx_audio_source: Arc<AtomicU8>,
        // See run()'s doc comment on the parameter of the same name.
        tci_wants_mic: Arc<AtomicBool>,
        tx_iq_out: Arc<Mutex<VecDeque<f32>>>,
        // See run()'s doc comment on the parameter of the same name.
        tx_spectrum_iq: Arc<Mutex<VecDeque<IqSample>>>,
        mox: Arc<AtomicBool>,
        channel: i32,
        protocol: u8,
        mic_rate: i32,
        duc_rate: i32,
        puresignal_enabled: bool,
        ps_rx_feedback_iq: Arc<Mutex<VecDeque<IqSample>>>,
        ps_tx_feedback_iq: Arc<Mutex<VecDeque<IqSample>>>,
        // Fixed per-radio correction-table file (config::ps_corr_path) --
        // see save_ps_corr/restore_ps_corr's doc comments. Passed
        // unconditionally (not just when puresignal_enabled) since it's
        // harmless to have set even when unused; nothing reads it unless
        // save_ps_corr/restore_ps_corr are actually called.
        ps_corr_path: Option<PathBuf>,
    ) -> Self {
        let display = Arc::new(Mutex::new(TxDisplay::default()));
        let params = Arc::new(Mutex::new(TxParams::default()));
        let ps_params = Arc::new(Mutex::new(PsParams::default()));
        let ps_status = Arc::new(Mutex::new(PsStatus::default()));
        let tx_audio_monitor = Arc::new(Mutex::new(VecDeque::with_capacity(TX_AUDIO_MONITOR_CAPACITY)));
        // Live -- see TxHandle::puresignal_enabled's doc comment.
        let puresignal_enabled = Arc::new(AtomicBool::new(puresignal_enabled));
        let stop = Arc::new(AtomicBool::new(false));

        let thread = {
            let display = Arc::clone(&display);
            let params = Arc::clone(&params);
            let ps_params = Arc::clone(&ps_params);
            let ps_status = Arc::clone(&ps_status);
            let tx_audio_monitor = Arc::clone(&tx_audio_monitor);
            let puresignal_enabled = Arc::clone(&puresignal_enabled);
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                run(
                    mic_buffer, tci_tx_audio, radio_mic_audio, tx_audio_source, tci_wants_mic,
                    tx_iq_out, tx_spectrum_iq, tx_audio_monitor, mox, params, display, channel, protocol, mic_rate,
                    duc_rate, puresignal_enabled, ps_rx_feedback_iq, ps_tx_feedback_iq, ps_params,
                    ps_status, ps_corr_path, stop,
                )
            })
        };

        Self {
            display,
            ps_status,
            tx_audio_monitor,
            puresignal_enabled,
            params,
            ps_params,
            stop,
            thread: Some(thread),
        }
    }

    pub fn mode(&self) -> Mode {
        self.params.lock().unwrap().mode
    }
    pub fn set_mode(&self, mode: Mode) {
        self.params.lock().unwrap().mode = mode;
    }

    pub fn mic_gain(&self) -> f32 {
        self.params.lock().unwrap().mic_gain
    }
    pub fn set_mic_gain(&self, gain: f32) {
        self.params.lock().unwrap().mic_gain = gain.max(0.0);
    }

    /// See TxParams::width_hz's doc comment -- feeds the TX bandpass
    /// filter's passband, same UI control as RX's per-mode width.
    pub fn set_width_hz(&self, width_hz: f64) {
        self.params.lock().unwrap().width_hz = width_hz;
    }

    /// See TxParams::tune's doc comment.
    pub fn set_tune(&self, tune: bool) {
        self.params.lock().unwrap().tune = tune;
    }

    /// See TxParams::two_tone's doc comment.
    pub fn set_two_tone(&self, two_tone: bool) {
        self.params.lock().unwrap().two_tone = two_tone;
    }

    /// See spectrum::EqualizerParams's doc comment.
    pub fn eq(&self) -> EqualizerParams {
        self.params.lock().unwrap().eq
    }
    pub fn set_eq(&self, eq: EqualizerParams) {
        self.params.lock().unwrap().eq = eq;
    }

    /// Current PureSignal params snapshot, for Settings -> PureSignal
    /// to read on each redraw.
    pub fn ps_params(&self) -> PsParams {
        *self.ps_params.lock().unwrap()
    }
    pub fn set_ps_enabled(&self, enabled: bool) {
        self.ps_params.lock().unwrap().enabled = enabled;
    }
    /// Live toggle mirroring RadioSession::set_puresignal_enabled --
    /// main.rs's PS checkbox calls both together. See TxHandle::
    /// puresignal_enabled's doc comment.
    pub fn set_puresignal_enabled(&self, enabled: bool) {
        self.puresignal_enabled.store(enabled, Ordering::SeqCst);
    }
    /// Triggers one single-shot manual calibration -- see
    /// PsParams::calibrate_request's doc comment for why this is a
    /// counter bump rather than a plain flag.
    pub fn ps_calibrate(&self) {
        self.ps_params.lock().unwrap().calibrate_request += 1;
    }
    /// Triggers an async save of the current correction table to this
    /// session's fixed per-radio path (`PsParams::save_corr_request`'s
    /// doc comment) -- called automatically by main.rs on a
    /// `Correcting` false->true edge, so a good table is never lost
    /// just because the app was closed before the user thought to save
    /// it manually. Harmless to call when there's nothing meaningful to
    /// save yet (e.g. before calibration first converges) -- WDSP just
    /// writes out whatever the current (possibly all-zero/default)
    /// table is.
    pub fn save_ps_corr(&self) {
        self.ps_params.lock().unwrap().save_corr_request += 1;
    }
    /// Triggers an async load+apply of a previously-saved correction
    /// table from this session's fixed per-radio path
    /// (`PsParams::restore_corr_request`'s doc comment) -- called once
    /// automatically right after a PureSignal-enabled session connects,
    /// if a saved file exists for this radio, so `Correcting` can be
    /// true immediately without needing to re-run Two-Tone every
    /// session.
    pub fn restore_ps_corr(&self) {
        self.ps_params.lock().unwrap().restore_corr_request += 1;
    }
    pub fn set_ps_hw_peak(&self, hw_peak: f64) {
        self.ps_params.lock().unwrap().hw_peak = hw_peak.clamp(0.0, 1.0);
    }
    pub fn set_ps_mox_delay(&self, mox_delay: f64) {
        self.ps_params.lock().unwrap().mox_delay = mox_delay.max(0.0);
    }
    pub fn set_ps_loop_delay(&self, loop_delay: f64) {
        self.ps_params.lock().unwrap().loop_delay = loop_delay.max(0.0);
    }
    pub fn set_ps_tx_delay_ns(&self, tx_delay_ns: f64) {
        self.ps_params.lock().unwrap().tx_delay_ns = tx_delay_ns.max(0.0);
    }
    pub fn set_ps_ptol(&self, ptol: f64) {
        self.ps_params.lock().unwrap().ptol = ptol.clamp(0.0, 1.0);
    }
    /// See PsParams::oneshot's doc comment.
    pub fn set_ps_oneshot(&self, oneshot: bool) {
        self.ps_params.lock().unwrap().oneshot = oneshot;
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for TxHandle {
    fn drop(&mut self) {
        self.stop();
    }
}
