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

pub struct AudioOutput {
    // Kept alive for as long as playback should continue; dropping this
    // stops the stream.
    _stream: cpal::Stream,
}

impl AudioOutput {
    pub fn start(buffer: Arc<Mutex<VecDeque<f32>>>) -> Result<Self, String> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| "no default audio output device found".to_string())?;

        let config = cpal::StreamConfig {
            channels: OUTPUT_CHANNELS,
            sample_rate: OUTPUT_SAMPLE_RATE,
            buffer_size: cpal::BufferSize::Default,
        };

        let stream = device
            .build_output_stream(
                &config,
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    let mut buf = buffer.lock().unwrap();
                    for sample in data.iter_mut() {
                        *sample = buf.pop_front().unwrap_or(0.0); // silence on underrun
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

/// Nearest-neighbor sample-rate converter (Bresenham-style integer
/// accumulator: exact, no float drift across calls) between whatever
/// rate a mic/virtual-cable device actually captures at and
/// INPUT_SAMPLE_RATE, which tx.rs's TXA chain is fixed to expect.
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
/// Nearest-neighbor (not linear-interpolated) is a deliberate
/// simplicity/quality tradeoff: good enough for voice/digital-mode
/// audio through TXA's existing 300-2700Hz bandpass filter (which
/// already discards anything a little resampling noise would add
/// outside that band), and -- unlike a hand-rolled interpolating
/// resampler -- simple enough to unit-test exactly (see tests below)
/// rather than trusting by inspection alone.
struct RateConverter {
    in_rate: u32,
    out_rate: u32,
    acc: i64,
}

impl RateConverter {
    fn new(in_rate: u32, out_rate: u32) -> Self {
        Self { in_rate: in_rate.max(1), out_rate: out_rate.max(1), acc: 0 }
    }

    /// Appends the resampled equivalent of `input` (mono, at in_rate)
    /// to `out` (mono, at out_rate). Carries its fractional position
    /// across calls, so chunk boundaries don't introduce timing error.
    fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        for &sample in input {
            self.acc += self.out_rate as i64;
            while self.acc >= self.in_rate as i64 {
                out.push(sample);
                self.acc -= self.in_rate as i64;
            }
        }
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
    fn rate_converter_passthrough_is_exact() {
        let mut conv = RateConverter::new(48_000, 48_000);
        let input = vec![1.0, 2.0, 3.0, 4.0];
        let mut out = Vec::new();
        conv.process(&input, &mut out);
        assert_eq!(out, input);
    }

    #[test]
    fn rate_converter_is_consistent_across_chunk_boundaries() {
        // Feeding the same total input in one call vs many small calls
        // must land on the same output length -- confirms the
        // accumulator correctly carries state across chunks the way
        // cpal's callback will actually deliver audio (many small
        // buffers, not one contiguous second).
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
        assert_eq!(whole_out.len(), chunked_out.len());
    }
}

pub struct MicInput {
    _stream: cpal::Stream,
    buffer: Arc<Mutex<VecDeque<f32>>>,
}

impl MicInput {
    /// Captures the default input device at ITS OWN native config
    /// (guaranteed supported, unlike forcing INPUT_SAMPLE_RATE/mono
    /// directly), then downmixes to mono and resamples to
    /// INPUT_SAMPLE_RATE itself (see RateConverter/downmix_to_mono)
    /// before pushing into `buffer` for tx.rs to consume.
    ///
    /// An earlier version of this requested INPUT_SAMPLE_RATE/mono
    /// directly via StreamConfig, relying on cpal/ALSA/PipeWire to
    /// silently resample+downmix on our behalf when the device's own
    /// native config didn't match (confirmed happening in practice:
    /// build_input_stream succeeded at a forced 48kHz mono config on a
    /// device whose own default was 44100Hz/2ch). That OS-side
    /// conversion path is a known source of periodic glitches, and
    /// matched a reported symptom of TX output power bouncing between
    /// the expected level and 0W on a steady tone -- consistent with
    /// the mic buffer periodically running dry (see tx.rs's underrun
    /// diagnostic) and, for an SSB TX chain, real silence going out as
    /// real near-zero RF, not just a display artifact. Requesting the
    /// native config removes that OS conversion path as a variable.
    pub fn start(buffer: Arc<Mutex<VecDeque<f32>>>) -> Result<Self, String> {
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .ok_or_else(|| "no default audio input device found".to_string())?;

        let device_name = device
            .description()
            .map(|d| d.name().to_string())
            .unwrap_or_else(|_| "<unknown>".to_string());
        let default_cfg = device
            .default_input_config()
            .map_err(|e| format!("couldn't query default input config: {e}"))?;
        let native_rate = default_cfg.sample_rate();
        let native_channels = default_cfg.channels();
        println!(
            "mic input: using \"{device_name}\" at its native {native_rate}Hz/{native_channels}ch, \
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
        let stream = device
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
            .map_err(|e| format!("failed to build audio input stream: {e}"))?;

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
