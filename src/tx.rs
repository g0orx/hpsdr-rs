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
*/

use crate::spectrum::Mode;
use crate::wdsp_sys as wdsp;
use std::collections::VecDeque;
use std::os::raw::c_int;
use std::sync::atomic::{AtomicBool, Ordering};
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
        }
    }
}

struct TxProcessor {
    channel: i32,
    mic_scratch: Vec<f64>,
    iq_scratch: Vec<f64>,
    last_mode: Option<Mode>,
    last_gain: Option<f32>,
    last_passband: Option<(f64, f64)>,
}

impl TxProcessor {
    fn open(channel: i32, protocol: u8, mic_rate: i32, duc_rate: i32) -> Self {
        // Confirmed against the reference: internal TXA processing rate
        // is protocol-dependent -- 96000 for Protocol 2, 48000 for
        // Protocol 1. An earlier version of this file fixed this at
        // 48000 always, which was wrong for P2.
        let dsp_rate = if protocol == 2 { 96_000 } else { 48_000 };
        let default_passband =
            crate::spectrum::passband_for(Mode::Usb, crate::spectrum::default_width_hz(Mode::Usb));

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
            wdsp::SetTXAALCDecay(channel, 10);
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
            }
            self.last_passband = Some(passband);
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
}

impl Drop for TxProcessor {
    fn drop(&mut self) {
        unsafe {
            wdsp::CloseChannel(self.channel);
        }
    }
}

fn run(
    mic_buffer: Arc<Mutex<VecDeque<f32>>>,
    tx_iq_out: Arc<Mutex<VecDeque<f32>>>,
    mox: Arc<AtomicBool>,
    params: Arc<Mutex<TxParams>>,
    display: Arc<Mutex<TxDisplay>>,
    channel: i32,
    protocol: u8,
    mic_rate: i32,
    duc_rate: i32,
    stop: Arc<AtomicBool>,
) {
    let mut processor = TxProcessor::open(channel, protocol, mic_rate, duc_rate);
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

    while !stop.load(Ordering::Relaxed) {
        if !mox.load(Ordering::Relaxed) {
            // Not transmitting -- drop any mic audio that accumulated
            // while idle so the next PTT doesn't start by replaying a
            // backlog of stale audio, and don't burn CPU running the
            // TXA chain on nothing.
            mic_buffer.lock().unwrap().clear();
            thread::sleep(Duration::from_millis(20));
            // Resync so the first chunk after PTT is produced against a
            // fresh schedule, not delayed by however long MOX was off --
            // same reasoning as p2_tx_iq_loop's own resync here.
            next_chunk = Instant::now();
            continue;
        }

        {
            let mut buf = mic_buffer.lock().unwrap();
            if buf.len() < TX_BUFFER_SIZE {
                starved_chunks_this_window += 1;
            }
            for slot in chunk.iter_mut() {
                *slot = buf.pop_front().unwrap_or(0.0); // silence on underrun, not silence on stall
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
        let (iq, exch_error) = processor.process(&chunk, p.mode, p.mic_gain, p.width_hz);
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
    params: Arc<Mutex<TxParams>>,
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
    pub fn start(
        mic_buffer: Arc<Mutex<VecDeque<f32>>>,
        tx_iq_out: Arc<Mutex<VecDeque<f32>>>,
        mox: Arc<AtomicBool>,
        channel: i32,
        protocol: u8,
        mic_rate: i32,
        duc_rate: i32,
    ) -> Self {
        let display = Arc::new(Mutex::new(TxDisplay::default()));
        let params = Arc::new(Mutex::new(TxParams::default()));
        let stop = Arc::new(AtomicBool::new(false));

        let thread = {
            let display = Arc::clone(&display);
            let params = Arc::clone(&params);
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                run(
                    mic_buffer, tx_iq_out, mox, params, display, channel, protocol, mic_rate,
                    duc_rate, stop,
                )
            })
        };

        Self {
            display,
            params,
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
