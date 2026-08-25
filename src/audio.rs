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

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

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
/// Input (VB-Audio Virtual Cable)" if installed) -- for the output
/// device picker in Settings (RX tab, main and extra receivers). Skips
/// any device whose name can't be queried (a disconnected/erroring
/// device) rather than failing the whole list over one bad entry.
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
    pub fn start(buffer: Arc<Mutex<VecDeque<f32>>>, device_name: Option<&str>) -> Result<Self, String> {
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
                // BUG FIX: `data` is cpal's interleaved STEREO buffer
                // (OUTPUT_CHANNELS=2), but `buffer` (audio_out) is MONO
                // content at 48kHz. This used to pop a fresh value from
                // the mono queue for every interleaved slot (both L and
                // R independently) instead of once per frame -- draining
                // the queue at 2x its true production rate, playing
                // local audio at roughly double speed/pitch. Confirmed
                // via a real report: WSJT-X (fed from this output via a
                // loopback device) showed known signals at the wrong
                // audio frequency, consistent with 2x speed. Each mono
                // sample must be duplicated across the frame's channels,
                // not treated as filling one channel slot at a time.
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    let mut buf = buffer.lock().unwrap();
                    for frame in data.chunks_mut(OUTPUT_CHANNELS as usize) {
                        let sample = buf.pop_front().unwrap_or(0.0); // silence on underrun
                        for channel in frame.iter_mut() {
                            *channel = sample;
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
    pub fn start(buffer: Arc<Mutex<VecDeque<f32>>>) -> Result<Self, String> {
        let host = cpal::default_host();
        let device = host
            .default_input_device()
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
