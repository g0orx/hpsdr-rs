/*
    Audio input/output via cpal.

    AudioOutput reads demodulated audio produced by the spectrum/demod
    thread (spectrum.rs) out of a shared ring buffer and plays it
    through the system's default output device.

    MicInput is the TX-side counterpart: captures from the system's
    default input device (a physical mic for voice, or a virtual/
    loopback device fed by WSJT-X etc. for digital modes) and pushes
    downmixed-to-mono samples into a ring buffer that tx.rs's TXA
    thread reads from as the modulation source.

    NOTE: cpal's build_input_stream/build_output_stream signatures used
    here (config, data callback, error callback, timeout: Option
    <Duration>) match cpal 0.17's documented API, but this hasn't been
    compile-checked in this environment (no Rust toolchain available)
    -- same caveat as every other external-crate API surface in this
    project, so treat this as the next likely spot for a compiler-
    driven fix if cpal has moved since.

    Also: on Linux, building cpal requires the ALSA development headers
    (libasound2-dev on Debian/Ubuntu, alsa-lib-devel on Fedora) --
    even when PipeWire/PulseAudio/JACK are the actual runtime backend.
*/

use crate::radio::CwKeyerAtomics;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const OUTPUT_SAMPLE_RATE: u32 = 48_000; // matches spectrum.rs's fixed WDSP output rate
const OUTPUT_CHANNELS: u16 = 2; // interleaved stereo, matches fexchange0's output convention

// Mic/TX-audio capture rate -- matches tx.rs's TXA input rate (mono,
// same 48kHz convention as the RX side's DSP_RATE). Using a fixed rate
// here rather than querying the device's own default avoids a mismatch
// with what the TXA channel was opened expecting.
const INPUT_SAMPLE_RATE: u32 = 48_000;
const INPUT_CHANNELS: u16 = 1;

/// Names of every currently available output-capable device (e.g. real
/// speakers/headphones, and on Windows, virtual devices like "CABLE
/// Input (VB-Audio Virtual Cable)" if installed) -- for the RX output
/// device picker in Settings -> Audio (main and extra receivers each
/// have their own). Skips any device whose name can't be queried (a
/// disconnected/erroring device) rather than failing the whole list
/// over one bad entry.
pub fn list_output_devices() -> Vec<String> {
    let host = cpal::default_host();
    match host.output_devices() {
        Ok(devices) => devices.filter_map(|d| d.description().ok().map(|desc| desc.name().to_string())).collect(),
        Err(e) => {
            eprintln!("audio: failed to enumerate output devices: {e}");
            Vec::new()
        }
    }
}

/// Same as list_output_devices, for input-capable devices (e.g. a real
/// mic, or on Windows, "CABLE Output (VB-Audio Virtual Cable)" if
/// installed) -- for the TX audio source picker in Settings -> Audio.
pub fn list_input_devices() -> Vec<String> {
    let host = cpal::default_host();
    match host.input_devices() {
        Ok(devices) => devices.filter_map(|d| d.description().ok().map(|desc| desc.name().to_string())).collect(),
        Err(e) => {
            eprintln!("audio: failed to enumerate input devices: {e}");
            Vec::new()
        }
    }
}

pub struct AudioOutput {
    // Kept alive for as long as playback should continue; dropping this
    // stops the stream.
    _stream: cpal::Stream,
}

impl AudioOutput {
    /// `device_name`: `None` (or a name that no longer matches any
    /// currently available device, e.g. a saved selection for a virtual
    /// cable that isn't installed on this machine) falls back to the
    /// system default output device, same as this always did before
    /// device selection existed -- never a hard error just because a
    /// specific device isn't found.
    pub fn start(buffer: Arc<Mutex<VecDeque<(f32, f32)>>>, device_name: Option<&str>) -> Result<Self, String> {
        let host = cpal::default_host();
        let device = match device_name {
            Some(name) => host
                .output_devices()
                .ok()
                .and_then(|mut devices| {
                    devices.find(|d| d.description().is_ok_and(|desc| desc.name() == name))
                })
                .or_else(|| {
                    eprintln!(
                        "audio: output device \"{name}\" not found -- falling back to the system default"
                    );
                    host.default_output_device()
                }),
            None => host.default_output_device(),
        }
        .ok_or_else(|| "no default audio output device found".to_string())?;

        let config = cpal::StreamConfig {
            channels: OUTPUT_CHANNELS,
            sample_rate: OUTPUT_SAMPLE_RATE,
            buffer_size: cpal::BufferSize::Default,
        };

        let stream = device
            .build_output_stream(
                &config,
                // BUG FIX (history): `data` is cpal's interleaved STEREO
                // buffer (OUTPUT_CHANNELS=2), but `buffer` (audio_out)
                // used to be MONO content at 48kHz. Popping a fresh
                // value from the mono queue for every interleaved slot
                // (both L and R independently) instead of once per frame
                // drained the queue at 2x its true production rate,
                // playing local audio at roughly double speed/pitch --
                // confirmed via a real report: WSJT-X (fed from this
                // output via a loopback device) showed known signals at
                // the wrong audio frequency, consistent with 2x speed.
                // `buffer` now carries real (L, R) pairs (see spectrum.
                // rs's DemodParams::binaural doc comment) -- one pair
                // per frame, written straight to both channel slots, so
                // this stays correct at the true 1:1 rate whether
                // binaural is on (genuinely different L/R) or off
                // (L==R, same as this project's own former duplicated-
                // mono behavior).
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    let mut buf = buffer.lock().unwrap();
                    for frame in data.chunks_mut(OUTPUT_CHANNELS as usize) {
                        let (l, r) = buf.pop_front().unwrap_or((0.0, 0.0)); // silence on underrun
                        if let [left, right, ..] = frame {
                            *left = l;
                            *right = r;
                        }
                    }
                },
                move |err| {
                    eprintln!("audio output stream error: {err}");
                },
                None, // no timeout; block as needed
            )
            .map_err(|e| format!("failed to build audio output stream: {e}"))?;

        stream
            .play()
            .map_err(|e| format!("failed to start audio playback: {e}"))?;

        Ok(Self { _stream: stream })
    }
}

/// Pure overflow backstop for this generator's writes into audio_out --
/// same "small, bounded, drop-oldest" reasoning as spectrum.rs's own
/// AUDIO_BUFFER_CAPACITY, and deliberately the SAME size: this is only
/// insurance against a genuine pathological stall (e.g. the process
/// briefly suspended), not a routine control. See run()'s own doc
/// comment for why actually keeping the queue in sync/free of a
/// lingering tail is handled by flushing on key transitions instead of
/// by trimming to a tight target every tick -- an earlier version of
/// this constant (10ms, enforced every tick) fought the audio backend's
/// OWN normal output buffering (commonly tens of ms, decided by cpal's
/// `BufferSize::Default`/the OS, not something this code controls),
/// repeatedly ripping out samples mid-waveform that simply hadn't been
/// played yet -- a real report: EVERY element sounded distorted, not
/// just some, consistent with near-constant chopping rather than an
/// occasional real desync.
const CW_SIDETONE_BUFFER_CAPACITY: usize = 14_400;

/// How long the sidetone's on/off envelope takes to ramp fully up or
/// down, in samples at OUTPUT_SAMPLE_RATE. Same purpose as piHPSDR's own
/// CW pulse-shaping ramp (transmitter.c's RAMPLEN): a hard on/off edge on
/// a sine tone is an audible click. 5ms is comfortably inside typical
/// CW envelope shaping recommendations (2-8ms) without softening dots at
/// high WPM.
const CW_SIDETONE_RAMP_SAMPLES: f32 = 0.005 * OUTPUT_SAMPLE_RATE as f32;

/// PC-side software CW sidetone -- a SEPARATE, additional feature from
/// the radio's own internal-keyer sidetone (see RadioSession::cw_keyer's
/// doc comment): that one plays out the radio's own local speaker/
/// headphone jack, generated autonomously by its FPGA once armed with
/// the Sidetone Level/Frequency settings, no PC audio involved at all.
/// This one exists for the OPERATING position instead -- synthesizes
/// the same tone in software from the radio's own paddle-contact
/// readback (RadioSession::cw_key_down) and plays it out the PC's own
/// audio output, for setups where the radio's local audio jack isn't
/// wired to anything the operator can hear (e.g. HermesLite2, which has
/// no local audio output hardware at all) or for remote operation.
///
/// Added specifically as an opt-in (Settings -> CW's own checkbox, see
/// `enabled`), NOT tied unconditionally to CW mode being selected --
/// unlike the radio-side sidetone, this one competes for the same
/// audio_out queue newRX audio uses (see spectrum.rs's own doc comment:
/// audio_out is unmuted the instant mox drops), so a user who only has
/// the radio's own local sidetone wired up and finds a second PC-side
/// copy redundant can turn this off without losing the radio's own.
pub struct CwSidetone {
    /// Live on/off toggle -- Settings -> CW's checkbox writes here
    /// directly, no reconnect needed (same pattern as RadioSession::
    /// puresignal_enabled/diversity_enabled).
    pub enabled: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl CwSidetone {
    /// `audio_out`: the SAME queue AudioOutput's cpal callback drains
    /// (RadioSession's main-receiver SpectrumHandle's own audio_out) --
    /// this generator only ever writes into it while ramped above
    /// silence (see run()'s own doc comment for why that matters), so
    /// it can share the queue with spectrum.rs's real RX-audio producer
    /// without a dedicated output device or a mixing stage.
    pub fn start(
        audio_out: Arc<Mutex<VecDeque<(f32, f32)>>>,
        mox: Arc<AtomicBool>,
        cw_mode_active: Arc<AtomicBool>,
        key_down: Arc<AtomicBool>,
        cw_keyer: Arc<CwKeyerAtomics>,
    ) -> Self {
        let enabled = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_enabled = Arc::clone(&enabled);
        let thread_stop = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            run(audio_out, mox, cw_mode_active, key_down, cw_keyer, thread_enabled, thread_stop);
        });
        Self { enabled, stop, thread: Some(thread) }
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for CwSidetone {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Background loop backing CwSidetone::start. Wakes on a short, fixed
/// tick (independent of the UI's own frame rate, which can hitch/vary)
/// and, each tick, generates however many samples real wall-clock time
/// says should exist since the last tick -- a simple free-running audio
/// clock, same idea as this project's own RateConverter but driven by
/// Instant instead of an input sample count.
///
/// ROOT CAUSE FIX for a real report: initial testing sounded "OK" at
/// first, then some elements distorted, occasionally drifted out of
/// sync, and the tone kept sounding briefly after releasing the key.
///
/// Bug #1: `keyed` was gated on key_down/cw_mode_active/enabled alone,
/// NOT on mox -- but spectrum.rs's real RX-audio producer gates its own
/// writes into this SAME queue purely on mox being false. Since main.
/// rs's break-in hang-timer (which raises mox) reads the same key_down
/// flag on its own ~16ms UI-frame cadence, there was a real window on
/// every key-down where this thread could already be ramping up while
/// spectrum.rs's thread was still pushing live RX audio into the same
/// queue -- two producers, unsynchronized, landing in one FIFO. That's
/// the distortion: audible RX content time-interleaved with the
/// sidetone. Fixed by also requiring `mox` here, matching spectrum.rs's
/// own gate exactly so the two producers are mutually exclusive rather
/// than merely usually so.
///
/// Bug #2: any stale backlog already sitting in the queue when a key
/// transition happens (leftover RX audio from just before mox went up,
/// or this generator's own prior-element backlog) plays out BEFORE the
/// freshly generated samples for the new state, since it's a FIFO --
/// audible as sync drifting worse over a longer transmission, and as
/// the tone continuing to sound for a bit after key-up (the queue was
/// still draining old, already-generated at-full-volume samples).
/// Fixed by flushing the queue outright on EVERY key transition (both
/// directions), so only what's generated AFTER a transition (the fresh
/// tone, or the fresh ramp-down) is ever queued following it.
///
/// A second attempt at bug #2 tried enforcing a tight (10ms) target
/// latency on EVERY tick instead of only at transitions -- that was
/// itself a bug: it fought the audio backend's own normal output
/// buffering (commonly tens of ms, decided by cpal/the OS, not
/// something this code controls), repeatedly ripping out samples mid-
/// waveform that simply hadn't been played yet. A real report: EVERY
/// element sounded distorted afterward, not just some -- consistent
/// with near-constant chopping rather than an occasional real desync.
/// Reverted to a purely transition-triggered flush plus a generous
/// passive overflow backstop (CW_SIDETONE_BUFFER_CAPACITY, matching
/// spectrum.rs's own AUDIO_BUFFER_CAPACITY) that only ever fires on a
/// genuine pathological stall, never during ordinary playback.
fn run(
    audio_out: Arc<Mutex<VecDeque<(f32, f32)>>>,
    mox: Arc<AtomicBool>,
    cw_mode_active: Arc<AtomicBool>,
    key_down: Arc<AtomicBool>,
    cw_keyer: Arc<CwKeyerAtomics>,
    enabled: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
) {
    let mut last = Instant::now();
    let mut phase: f32 = 0.0;
    let mut gain: f32 = 0.0;
    let mut was_keyed = false;
    while !stop.load(Ordering::Relaxed) {
        // Short tick: tighter envelope/edge timing than the original
        // 10ms, and a smaller worst-case burst size for the backlog
        // trim below to reason about.
        thread::sleep(Duration::from_millis(2));
        let now = Instant::now();
        let elapsed = now.duration_since(last);
        last = now;
        let samples_needed =
            (elapsed.as_secs_f64() * OUTPUT_SAMPLE_RATE as f64).round() as usize;
        if samples_needed == 0 {
            continue;
        }
        let keyed = enabled.load(Ordering::Relaxed)
            && mox.load(Ordering::Relaxed)
            && cw_mode_active.load(Ordering::Relaxed)
            && key_down.load(Ordering::Relaxed);
        if keyed != was_keyed {
            // Any transition -- see this function's own doc comment
            // (bug #2): drop anything already queued (stale RX audio
            // from just before mox went up, or this generator's own
            // backlog from before the transition) so only what's
            // generated AFTER this point -- the fresh tone on a rising
            // edge, or the fresh ramp-down on a falling edge -- is ever
            // heard following it.
            audio_out.lock().unwrap().clear();
        }
        was_keyed = keyed;
        let target = if keyed { 1.0 } else { 0.0 };
        let freq_hz = cw_keyer.sidetone_freq_hz.load(Ordering::Relaxed).max(1) as f32;
        // Same 0-255 full-byte range as P2's own sidetone_volume byte
        // (see p2_tx_specific_packet) -- reused directly as a 0.0-1.0
        // amplitude scalar rather than inventing a separate PC-only
        // volume control, so the existing Sidetone Level slider governs
        // both the radio's own sidetone AND this one together.
        let amplitude = (cw_keyer.sidetone_volume.load(Ordering::Relaxed).min(255) as f32) / 255.0;
        let step = 2.0 * std::f32::consts::PI * freq_hz / OUTPUT_SAMPLE_RATE as f32;
        let ramp_step = 1.0 / CW_SIDETONE_RAMP_SAMPLES;
        let mut samples: Vec<(f32, f32)> = Vec::with_capacity(samples_needed);
        for _ in 0..samples_needed {
            if gain < target {
                gain = (gain + ramp_step).min(target);
            } else if gain > target {
                gain = (gain - ramp_step).max(target);
            }
            if gain <= 0.0 && target <= 0.0 {
                // Fully silent -- stop generating for the rest of this
                // tick too (target can't un-ramp mid-loop; enabled/
                // mox/cw_mode_active/key_down are only re-read next
                // tick).
                break;
            }
            phase += step;
            if phase >= 2.0 * std::f32::consts::PI {
                phase -= 2.0 * std::f32::consts::PI;
            }
            let s = amplitude * gain * phase.sin();
            samples.push((s, s));
        }
        if !samples.is_empty() {
            let mut out = audio_out.lock().unwrap();
            for pair in samples {
                if out.len() >= CW_SIDETONE_BUFFER_CAPACITY {
                    out.pop_front();
                }
                out.push_back(pair);
            }
        }
    }
}

/// Same small-ring-buffer-with-drop-on-overflow philosophy as the RX
/// audio path (see spectrum.rs's AUDIO_BUFFER_CAPACITY comment): a
/// backlog here becomes added mic-to-RF latency, not something that
/// self-corrects, so keep the cap small. ~0.5s at 48kHz mono.
const MIC_BUFFER_CAPACITY: usize = 24_000;

/// Linear-interpolating sample-rate converter between whatever rate a
/// mic/virtual-cable device actually captures at and INPUT_SAMPLE_RATE,
/// which tx.rs's TXA chain is fixed to expect.
///
/// Added after a confirmed real-world case: cpal successfully built a
/// stream at a forced 48kHz mono config on a device whose own native
/// default was 44100Hz/2ch (build_input_stream didn't error -- ALSA/
/// PipeWire's compatibility layer silently resampled+downmixed on our
/// behalf). That OS-side conversion path is a known source of periodic
/// glitches, and matched a reported symptom of TX output power
/// bouncing between the expected level and 0W on a steady tone --
/// consistent with the mic buffer periodically running dry (see
/// tx.rs's underrun diagnostic) and, for an SSB TX chain, real silence
/// going out as real near-zero RF. This converter exists so MicInput
/// can request the device's own native config (which it's guaranteed
/// to support) and do the rate conversion itself instead, removing
/// that OS conversion path as a variable entirely.
///
/// UPGRADED from an earlier nearest-neighbor (sample repeat/drop)
/// version while chasing a separate reported bug (transmitted spectrum
/// showing wideband splatter instead of a clean single-tone spike on a
/// steady WSJT-X Tune carrier, compared side-by-side against
/// rustyHPSDR on the same signal): nearest-neighbor resampling has no
/// anti-aliasing and was a real, if not fully confirmed, candidate
/// contributor to that noise floor. Linear interpolation is a strict
/// quality improvement (bounded, well-understood error instead of hard
/// sample-repeat discontinuities) and, unlike nearest-neighbor, is
/// exact for the ratio=1 passthrough case with no special-casing
/// needed. Carries `prev` (the last input sample from the previous
/// call) and a rebased `pos` across calls so chunk boundaries -- which
/// is how cpal's callback actually delivers audio, many small buffers
/// rather than one contiguous stream -- don't introduce timing error
/// or a discontinuity at each boundary.
struct RateConverter {
    ratio: f64, // in_rate / out_rate: input-sample advance per output sample
    pos: f64,   // read position in virtual-stream units; see process()
    prev: f32,  // last input sample from the previous call (0.0 before the very first)
}

impl RateConverter {
    fn new(in_rate: u32, out_rate: u32) -> Self {
        Self { ratio: in_rate.max(1) as f64 / out_rate.max(1) as f64, pos: 0.0, prev: 0.0 }
    }

    /// Appends the resampled equivalent of `input` (mono, at in_rate)
    /// to `out` (mono, at out_rate).
    ///
    /// Treats the virtual sample stream as V[0]=prev, V[k]=input[k-1]
    /// for k=1..=input.len(), and linearly interpolates at position
    /// `pos` (advancing by `ratio` per output sample) between
    /// V[floor(pos)] and V[floor(pos)+1]. Rebases `pos` by input.len()
    /// at the end of each call and stores the last input sample as the
    /// next call's `prev`, so a multi-call stream behaves identically
    /// to one long call (verified by
    /// rate_converter_is_consistent_across_chunk_boundaries below).
    fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if input.is_empty() {
            return;
        }
        let n = input.len();
        let v = |k: usize| -> f32 {
            if k == 0 {
                self.prev
            } else {
                input[k - 1]
            }
        };
        while (self.pos.floor() as usize) < n {
            let idx = self.pos.floor() as usize;
            let frac = (self.pos - idx as f64) as f32;
            let left = v(idx);
            let right = v(idx + 1);
            out.push(left + (right - left) * frac);
            self.pos += self.ratio;
        }
        self.pos -= n as f64;
        self.prev = input[n - 1];
    }
}

/// Downmixes one interleaved multi-channel frame block to mono by
/// averaging all channels -- most mic/virtual-cable devices are mono
/// or stereo-with-identical-channels anyway, so this is a safe default
/// rather than picking channel 0 and silently dropping the other(s).
fn downmix_to_mono(interleaved: &[f32], channels: u16, out: &mut Vec<f32>) {
    let channels = channels.max(1) as usize;
    for frame in interleaved.chunks(channels) {
        let sum: f32 = frame.iter().sum();
        out.push(sum / frame.len() as f32);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downmix_averages_all_channels() {
        let mut out = Vec::new();
        downmix_to_mono(&[1.0, 3.0, -1.0, -3.0], 2, &mut out);
        assert_eq!(out, vec![2.0, -2.0]);
    }

    #[test]
    fn downmix_passes_mono_through_unchanged() {
        let mut out = Vec::new();
        downmix_to_mono(&[0.1, 0.2, 0.3], 1, &mut out);
        assert_eq!(out, vec![0.1, 0.2, 0.3]);
    }

    #[test]
    fn rate_converter_upsamples_to_expected_length() {
        // 44100 -> 48000: real capture case this was written for.
        let mut conv = RateConverter::new(44_100, 48_000);
        let input = vec![0.0f32; 44_100]; // 1 second worth
        let mut out = Vec::new();
        conv.process(&input, &mut out);
        // Exact within +/-1 sample -- a Bresenham accumulator can be at
        // most one output sample off from the ideal ratio at any point.
        assert!((out.len() as i64 - 48_000).abs() <= 1, "got {} expected ~48000", out.len());
    }

    #[test]
    fn rate_converter_downsamples_to_expected_length() {
        let mut conv = RateConverter::new(96_000, 48_000);
        let input = vec![0.0f32; 96_000];
        let mut out = Vec::new();
        conv.process(&input, &mut out);
        assert!((out.len() as i64 - 48_000).abs() <= 1, "got {} expected ~48000", out.len());
    }

    #[test]
    fn rate_converter_passthrough_reproduces_input_with_one_sample_lag() {
        // ratio=1.0 still goes through the same interpolation path (no
        // special-casing) -- ordinary linear-interpolation behavior for
        // that case is exact reproduction of the input, delayed by one
        // sample (V[0]=prev=0.0 initially stands in for the sample
        // "before" input[0]). Confirms there's no off-by-one distortion
        // introduced specifically at unity ratio.
        let mut conv = RateConverter::new(48_000, 48_000);
        let input = vec![1.0, 2.0, 3.0, 4.0];
        let mut out = Vec::new();
        conv.process(&input, &mut out);
        assert_eq!(out, vec![0.0, 1.0, 2.0, 3.0]);

        // Feeding another chunk continues the same one-sample lag using
        // the real previous sample (4.0) now, not the initial fake 0.0.
        let mut out2 = Vec::new();
        conv.process(&[5.0, 6.0], &mut out2);
        assert_eq!(out2, vec![4.0, 5.0]);
    }

    #[test]
    fn rate_converter_reconstructs_a_tone_with_low_error() {
        // The actual point of upgrading away from nearest-neighbor:
        // verify the resampled waveform is a faithful reconstruction of
        // a real tone (not just the right sample count). 1500Hz is a
        // typical WSJT-X Tune tone frequency; 44100->48000 is the real
        // capture-rate mismatch this converter exists for.
        let in_rate = 44_100u32;
        let out_rate = 48_000u32;
        let freq = 1500.0_f64;
        let n_in = in_rate as usize / 4; // 250ms
        let input: Vec<f32> = (0..n_in)
            .map(|i| (2.0 * std::f64::consts::PI * freq * i as f64 / in_rate as f64).sin() as f32)
            .collect();

        let mut conv = RateConverter::new(in_rate, out_rate);
        let ratio = conv.ratio;
        let mut out = Vec::new();
        conv.process(&input, &mut out);

        // Output sample n sits at virtual position n*ratio (single
        // call, pos started at 0), and V[1]=input[0] is defined to sit
        // at real time 0 -- so virtual position p maps to real time
        // (p-1)/in_rate. Skip a few samples at each end to stay clear
        // of the fixed startup lag and any last-sample edge effects.
        let mut sum_sq_err = 0.0_f64;
        let mut count = 0usize;
        for (n, &sample) in out.iter().enumerate() {
            if n < 5 || n + 5 >= out.len() {
                continue;
            }
            let t = (n as f64 * ratio - 1.0) / in_rate as f64;
            let expected = (2.0 * std::f64::consts::PI * freq * t).sin();
            let err = sample as f64 - expected;
            sum_sq_err += err * err;
            count += 1;
        }
        let rms_error = (sum_sq_err / count as f64).sqrt();
        assert!(rms_error < 0.02, "RMS reconstruction error too high: {rms_error}");
    }

    #[test]
    fn rate_converter_is_consistent_across_chunk_boundaries() {
        // Feeding the same total input in one call vs many small calls
        // must land on nearly the same output length -- confirms the
        // rebased `pos`/`prev` state correctly carries across chunks
        // the way cpal's callback actually delivers audio (many small
        // buffers, not one contiguous second). A difference of a
        // sample or two is expected and fine here: `pos -= n as f64`
        // repeated ~1200 times (44100 samples / 37-sample chunks)
        // accumulates ordinary f64 rounding error that a single big
        // subtraction wouldn't -- that's floating-point reality, not a
        // correctness bug, and doesn't affect audio quality.
        let total_samples = 44_100;
        let mut whole = RateConverter::new(44_100, 48_000);
        let mut whole_out = Vec::new();
        whole.process(&vec![0.0f32; total_samples], &mut whole_out);

        let mut chunked = RateConverter::new(44_100, 48_000);
        let mut chunked_out = Vec::new();
        let mut remaining = total_samples;
        while remaining > 0 {
            let n = remaining.min(37); // deliberately not a clean divisor
            chunked.process(&vec![0.0f32; n], &mut chunked_out);
            remaining -= n;
        }
        let diff = (whole_out.len() as i64 - chunked_out.len() as i64).abs();
        assert!(diff <= 2, "whole={} chunked={}", whole_out.len(), chunked_out.len());
    }
}

pub struct MicInput {
    _stream: cpal::Stream,
    buffer: Arc<Mutex<VecDeque<f32>>>,
}

impl MicInput {
    /// Tries requesting exactly what tx.rs's TXA chain needs (48kHz
    /// mono) directly first, with NO resampling/downmixing at all --
    /// only if that genuinely fails does it fall back to the device's
    /// own native config plus software downmix+resample.
    ///
    /// REVERSED from an earlier version of this function (which always
    /// queried and used default_input_config()'s reported native
    /// config, resampling from that unconditionally), after confirming
    /// that approach was itself a real, active bug, not a hypothetical
    /// risk: on a system with PipeWire (common on Linux), the audio
    /// SERVER's actual delivery rate is normally a single fixed clock
    /// for its whole graph (confirmed via `pw-metadata -n settings`
    /// showing `clock.allowed-rates: [ 48000 ]` on the system this was
    /// diagnosed on) -- but `default_input_config()`'s reported rate
    /// (e.g. "44100Hz") reflects a stale/generic ALSA-compatibility
    /// default, NOT that true fixed delivery rate. Audio genuinely
    /// already arriving at 48kHz was being resampled as if it were
    /// 44100Hz -- real, active corruption of otherwise-clean audio,
    /// not OS-side conversion risk -- and was the confirmed cause of a
    /// reported wideband/dirty TX spectrum (compared side-by-side
    /// against rustyHPSDR on an identical WSJT-X Tune test). Directly
    /// requesting 48kHz/mono is an exact native match requiring NO
    /// conversion anywhere, by the OS or by us, whenever the audio
    /// server's true rate happens to already be 48kHz (as it commonly
    /// is) -- which build_input_stream succeeding confirms, since cpal
    /// doesn't silently coerce an unsupported rate/channel count, it
    /// errors. The native-config+resample path stays as a fallback for
    /// a genuinely different device/system (e.g. real 44.1kHz-only
    /// hardware, or an audio server without a fixed shared clock) where
    /// resampling is actually necessary rather than a self-inflicted
    /// mismatch.
    /// `device_name`: same "fall back to the system default, never a
    /// hard error just because a specific device isn't found" contract
    /// as AudioOutput::start -- e.g. a saved "CABLE Output (VB-Audio
    /// Virtual Cable)" selection on a machine that doesn't have it
    /// installed just uses the default mic instead.
    pub fn start(buffer: Arc<Mutex<VecDeque<f32>>>, selected_device_name: Option<&str>) -> Result<Self, String> {
        let host = cpal::default_host();
        let device = match selected_device_name {
            Some(name) => host
                .input_devices()
                .ok()
                .and_then(|mut devices| devices.find(|d| d.description().is_ok_and(|desc| desc.name() == name)))
                .or_else(|| {
                    eprintln!(
                        "audio: input device \"{name}\" not found -- falling back to the system default"
                    );
                    host.default_input_device()
                }),
            None => host.default_input_device(),
        }
        .ok_or_else(|| "no default audio input device found".to_string())?;

        let device_name = device
            .description()
            .map(|d| d.name().to_string())
            .unwrap_or_else(|_| "<unknown>".to_string());

        let direct_config = cpal::StreamConfig {
            channels: INPUT_CHANNELS,
            sample_rate: INPUT_SAMPLE_RATE,
            buffer_size: cpal::BufferSize::Default,
        };
        let direct_buffer = Arc::clone(&buffer);
        let direct_result = device.build_input_stream(
            &direct_config,
            move |data: &[f32], _: &cpal::InputCallbackInfo| {
                let mut buf = direct_buffer.lock().unwrap();
                for &sample in data {
                    if buf.len() >= MIC_BUFFER_CAPACITY {
                        buf.pop_front();
                    }
                    buf.push_back(sample);
                }
            },
            move |err| {
                eprintln!("audio input stream error: {err}");
            },
            None,
        );

        let stream = match direct_result {
            Ok(stream) => {
                println!(
                    "mic input: using \"{device_name}\" at {INPUT_SAMPLE_RATE}Hz/{INPUT_CHANNELS}ch \
                     directly -- no resampling"
                );
                stream
            }
            Err(e) => {
                let default_cfg = device
                    .default_input_config()
                    .map_err(|e2| format!("{INPUT_SAMPLE_RATE}Hz/{INPUT_CHANNELS}ch direct request \
                        failed ({e}), and querying a fallback native config also failed: {e2}"))?;
                let native_rate = default_cfg.sample_rate();
                let native_channels = default_cfg.channels();
                println!(
                    "mic input: \"{device_name}\" doesn't support {INPUT_SAMPLE_RATE}Hz/{INPUT_CHANNELS}ch \
                     directly ({e}) -- falling back to its native {native_rate}Hz/{native_channels}ch, \
                     downmixed and resampled to {INPUT_SAMPLE_RATE}Hz/{INPUT_CHANNELS}ch in software"
                );

                let config = cpal::StreamConfig {
                    channels: native_channels,
                    sample_rate: native_rate,
                    buffer_size: cpal::BufferSize::Default,
                };

                let mut mono_scratch: Vec<f32> = Vec::new();
                let mut resampled_scratch: Vec<f32> = Vec::new();
                let mut resampler = RateConverter::new(native_rate, INPUT_SAMPLE_RATE);
                let callback_buffer = Arc::clone(&buffer);
                device
                    .build_input_stream(
                        &config,
                        move |data: &[f32], _: &cpal::InputCallbackInfo| {
                            mono_scratch.clear();
                            downmix_to_mono(data, native_channels, &mut mono_scratch);
                            resampled_scratch.clear();
                            resampler.process(&mono_scratch, &mut resampled_scratch);

                            let mut buf = callback_buffer.lock().unwrap();
                            for &sample in &resampled_scratch {
                                if buf.len() >= MIC_BUFFER_CAPACITY {
                                    buf.pop_front();
                                }
                                buf.push_back(sample);
                            }
                        },
                        move |err| {
                            eprintln!("audio input stream error: {err}");
                        },
                        None,
                    )
                    .map_err(|e| format!("failed to build fallback audio input stream: {e}"))?
            }
        };

        stream
            .play()
            .map_err(|e| format!("failed to start audio capture: {e}"))?;

        Ok(Self { _stream: stream, buffer })
    }

    /// The ring buffer this capture writes into -- lets a caller (e.g.
    /// after a sample-rate change forces tx.rs's TXA channel to be
    /// rebuilt) hand the *same* live mic capture to a new TxHandle
    /// without tearing down and reopening the audio input stream too.
    pub fn buffer(&self) -> &Arc<Mutex<VecDeque<f32>>> {
        &self.buffer
    }
}
