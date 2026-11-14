/*
    RX audio out: the server's ADPCM blocks, decoded, through a jitter
    buffer to the default output device (WebAudio in a browser). Audio
    from a network arrives in uneven bursts, so playback waits for
    TARGET_S of it before starting, and drops back to that if bursts pile
    it past MAX_S -- a little latency for no gaps. Resampled to whatever
    rate the device runs at.

    Also the CW sidetone while keying from the on-screen key or paddles:
    made here, straight into the output, so it follows the key with no
    network delay.
*/

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

/// Seconds buffered before playing, and the most allowed to pile up.
const TARGET_S: f32 = 0.15;
const MAX_S: f32 = 0.6;

struct Jitter {
    buf: VecDeque<f32>,
    rate: u32,
    device_rate: u32,
    /// Fractional read position between buf[0] and buf[1].
    pos: f64,
    playing: bool,
    gain: f32,
    sidetone: Sidetone,
    /// Output callbacks so far: the status line says if they stop.
    callbacks: u64,
    /// The stream's last error, if any.
    error: Option<String>,
}

/// A tone at SIDETONE_HZ while the key is down, with 5 ms ramps so it
/// doesn't click.
#[derive(Default)]
struct Sidetone {
    on: bool,
    level: f32,
    gain: f32,
    phase: f32,
}

const SIDETONE_HZ: f32 = 600.0;

impl Sidetone {
    fn next(&mut self, device_rate: u32) -> f32 {
        let target = if self.on { 1.0 } else { 0.0 };
        if self.gain == 0.0 && target == 0.0 {
            return 0.0;
        }
        let step = 1.0 / (0.005 * device_rate as f32).max(1.0);
        self.gain = if self.gain < target { (self.gain + step).min(target) } else { (self.gain - step).max(target) };
        self.phase += 2.0 * std::f32::consts::PI * SIDETONE_HZ / device_rate as f32;
        if self.phase >= 2.0 * std::f32::consts::PI {
            self.phase -= 2.0 * std::f32::consts::PI;
        }
        self.level * self.gain * self.phase.sin()
    }
}

impl Jitter {
    fn push(&mut self, samples: &[i16], rate: u32) {
        if rate != self.rate {
            self.buf.clear();
            self.rate = rate;
            self.playing = false;
        }
        self.buf.extend(samples.iter().map(|&s| s as f32 / 32768.0));
        let max = (MAX_S * rate as f32) as usize;
        if self.buf.len() > max {
            let target = (TARGET_S * rate as f32) as usize;
            self.buf.drain(..self.buf.len() - target);
        }
        if !self.playing && self.buf.len() >= (TARGET_S * rate as f32) as usize {
            self.playing = true;
        }
    }

    fn next(&mut self) -> f32 {
        if !self.playing || self.buf.len() < 2 {
            self.playing = false;
            return 0.0;
        }
        let a = self.buf[0];
        let b = self.buf[1];
        let out = a + (b - a) * self.pos as f32;
        self.pos += self.rate as f64 / self.device_rate as f64;
        while self.pos >= 1.0 && !self.buf.is_empty() {
            self.buf.pop_front();
            self.pos -= 1.0;
        }
        out * self.gain
    }
}

pub struct Player {
    jitter: Arc<Mutex<Jitter>>,
    _stream: cpal::Stream,
}

/// Natively, the stereo 48 kHz f32 output hpsdr-rs itself opens, which works
/// on the Raspberry Pis it runs on; the device's default config if that's
/// refused.
const OUTPUT_RATE: u32 = 48_000;
#[cfg(not(target_arch = "wasm32"))]
const OUTPUT_CHANNELS: u16 = 2;

/// The output stream, writing samples of type `T`.
fn build_output<T>(device: &cpal::Device, config: &cpal::StreamConfig, jitter: &Arc<Mutex<Jitter>>) -> Result<cpal::Stream, String>
where
    T: cpal::SizedSample + cpal::FromSample<f32>,
{
    let channels = config.channels as usize;
    let feed = Arc::clone(jitter);
    device
        .build_output_stream(
            config,
            move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                let mut j = feed.lock().unwrap();
                j.callbacks += 1;
                let rate = j.device_rate;
                for frame in data.chunks_mut(channels) {
                    let v = (j.next() + j.sidetone.next(rate)).clamp(-1.0, 1.0);
                    frame.fill(T::from_sample(v));
                }
            },
            {
                let jitter = Arc::clone(jitter);
                move |e| {
                    let e = format!("audio output: {e}");
                    #[cfg(not(target_arch = "wasm32"))]
                    eprintln!("hpsdr-remote: {e}");
                    jitter.lock().unwrap().error = Some(e);
                }
            },
            None,
        )
        .map_err(|e| format!("audio output: {e}"))
}

impl Player {
    /// Opens the default output device. In a browser this has to happen in
    /// response to a click (browsers only start audio after one).
    pub fn open() -> Result<Self, String> {
        let host = cpal::default_host();
        let device = host.default_output_device().ok_or("no audio output device")?;
        let jitter = Arc::new(Mutex::new(Jitter {
            buf: VecDeque::new(),
            rate: 0,
            device_rate: OUTPUT_RATE,
            pos: 0.0,
            playing: false,
            gain: 1.0,
            sidetone: Sidetone::default(),
            callbacks: 0,
            error: None,
        }));
        // REAL REPORT 2026-11-14, Raspberry Pi 4: no audio and no playback
        // stream in pavucontrol, though the stream opened, with the
        // device's default config. hpsdr-rs on the same Pi plays with this
        // fixed one.
        #[cfg(not(target_arch = "wasm32"))]
        let fixed = {
            let config =
                cpal::StreamConfig { channels: OUTPUT_CHANNELS, sample_rate: OUTPUT_RATE, buffer_size: cpal::BufferSize::Default };
            build_output::<f32>(&device, &config, &jitter).map(|s| (s, config, cpal::SampleFormat::F32))
        };
        #[cfg(target_arch = "wasm32")]
        let fixed: Result<(cpal::Stream, cpal::StreamConfig, cpal::SampleFormat), String> = Err(String::new());
        let (stream, config, format) = match fixed {
            Ok(opened) => opened,
            Err(_) => {
                let supported = device.default_output_config().map_err(|e| format!("audio output: {e}"))?;
                let format = supported.sample_format();
                let config: cpal::StreamConfig = supported.into();
                jitter.lock().unwrap().device_rate = config.sample_rate;
                // In the device's own sample format; any other than these,
                // f32, which ALSA's default device converts.
                let stream = match format {
                    cpal::SampleFormat::I16 => build_output::<i16>(&device, &config, &jitter),
                    cpal::SampleFormat::U16 => build_output::<u16>(&device, &config, &jitter),
                    cpal::SampleFormat::I32 => build_output::<i32>(&device, &config, &jitter),
                    _ => build_output::<f32>(&device, &config, &jitter),
                }?;
                (stream, config, format)
            }
        };
        #[cfg(not(target_arch = "wasm32"))]
        {
            let name = device.description().map(|d| d.name().to_string()).unwrap_or_else(|_| "?".to_string());
            eprintln!(
                "hpsdr-remote: audio output \"{name}\", {} Hz, {} channels, {format:?}",
                config.sample_rate, config.channels
            );
        }
        #[cfg(target_arch = "wasm32")]
        let _ = (&config, format);
        stream.play().map_err(|e| format!("audio output: {e}"))?;
        Ok(Self { jitter, _stream: stream })
    }

    pub fn push(&self, samples: &[i16], rate: u32) {
        self.jitter.lock().unwrap().push(samples, rate);
    }

    pub fn set_gain(&self, gain: f32) {
        self.jitter.lock().unwrap().gain = gain;
    }

    /// The CW sidetone: on while the key is down, at `level` (0-1).
    pub fn set_sidetone(&self, on: bool, level: f32) {
        let mut j = self.jitter.lock().unwrap();
        j.sidetone.on = on;
        j.sidetone.level = level;
    }

    /// Output callbacks so far, and the stream's last error: for the
    /// status line.
    pub fn callbacks(&self) -> u64 {
        self.jitter.lock().unwrap().callbacks
    }

    pub fn error(&self) -> Option<String> {
        self.jitter.lock().unwrap().error.clone()
    }

    /// Seconds buffered, for the status line.
    pub fn buffered_s(&self) -> f32 {
        let j = self.jitter.lock().unwrap();
        if j.rate == 0 {
            0.0
        } else {
            j.buf.len() as f32 / j.rate as f32
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jitter(device_rate: u32) -> Jitter {
        Jitter {
            buf: VecDeque::new(),
            rate: 0,
            device_rate,
            pos: 0.0,
            playing: false,
            gain: 1.0,
            sidetone: Sidetone::default(),
            callbacks: 0,
            error: None,
        }
    }

    #[test]
    fn waits_for_the_target_then_plays_and_resamples() {
        let mut j = jitter(48_000);
        j.push(&[1000; 1000], 24_000);
        assert_eq!(j.next(), 0.0, "not enough yet");
        j.push(&[1000; 3000], 24_000);
        assert!(j.playing);
        let before = j.buf.len();
        for _ in 0..200 {
            assert!((j.next() - 1000.0 / 32768.0).abs() < 1e-6);
        }
        // 200 output samples at 48 kHz use 100 at 24 kHz.
        assert_eq!(before - j.buf.len(), 100);
    }

    #[test]
    fn a_pile_up_drops_back_to_the_target() {
        let mut j = jitter(48_000);
        j.push(&vec![0; 24_000], 24_000);
        assert_eq!(j.buf.len(), (TARGET_S * 24_000.0) as usize);
    }
}
