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
        }
    }
}

struct TxProcessor {
    channel: i32,
    mic_scratch: Vec<f64>,
    iq_scratch: Vec<f64>,
    last_mode: Option<Mode>,
    last_gain: Option<f32>,
}

impl TxProcessor {
    fn open(channel: i32, protocol: u8, mic_rate: i32, duc_rate: i32) -> Self {
        // Confirmed against the reference: internal TXA processing rate
        // is protocol-dependent -- 96000 for Protocol 2, 48000 for
        // Protocol 1. An earlier version of this file fixed this at
        // 48000 always, which was wrong for P2.
        let dsp_rate = if protocol == 2 { 96_000 } else { 48_000 };

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
            wdsp::TXASetMP(channel, 0); // low-latency mode off, matches reference default
            wdsp::SetTXABandpassWindow(channel, 1);
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

            // ALC decay corrected to match the reference (10, not the
            // previous unconfirmed guess of 60). Always-on, as before.
            wdsp::SetTXAALCAttack(channel, 1);
            wdsp::SetTXAALCDecay(channel, 10);
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

            // TX bandpass passband -- fixed at a typical SSB voice
            // bandwidth for now. NOT mode-adaptive yet (the reference
            // switches this per-mode via its own filter_low/filter_high
            // state, e.g. different values for CW); a reasonable
            // default given this project's current single fixed-filter
            // setup, but worth revisiting if CW or other modes need a
            // narrower passband.
            wdsp::SetTXABandpassFreqs(channel, 300.0, 2700.0);
        }

        let duc_ratio = ((duc_rate / dsp_rate).max(1)) as usize;
        let out_iq_pairs = TX_BUFFER_SIZE * duc_ratio;

        TxProcessor {
            channel,
            mic_scratch: vec![0.0; TX_BUFFER_SIZE * 2],
            iq_scratch: vec![0.0; out_iq_pairs * 2],
            last_mode: None,
            last_gain: None,
        }
    }

    /// mic_samples: TX_BUFFER_SIZE mono samples. Returns interleaved
    /// I/Q f32 TX samples at the DUC rate this channel was opened
    /// with.
    fn process(&mut self, mic_samples: &[f32], mode: Mode, mic_gain: f32) -> Vec<f32> {
        debug_assert_eq!(mic_samples.len(), TX_BUFFER_SIZE);

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

        self.iq_scratch.iter().map(|&v| v as f32).collect()
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

    while !stop.load(Ordering::Relaxed) {
        if !mox.load(Ordering::Relaxed) {
            // Not transmitting -- drop any mic audio that accumulated
            // while idle so the next PTT doesn't start by replaying a
            // backlog of stale audio, and don't burn CPU running the
            // TXA chain on nothing.
            mic_buffer.lock().unwrap().clear();
            thread::sleep(Duration::from_millis(20));
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
        let iq = processor.process(&chunk, p.mode, p.mic_gain);

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

        thread::sleep(chunk_interval);
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
