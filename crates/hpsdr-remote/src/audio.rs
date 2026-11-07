/*
    RX audio out: the server's ADPCM blocks, decoded, through a jitter
    buffer to the default output device (WebAudio in a browser). Audio
    from a network arrives in uneven bursts, so playback waits for
    TARGET_S of it before starting, and drops back to that if bursts pile
    it past MAX_S -- a little latency for no gaps. Resampled to whatever
    rate the device runs at.
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

impl Player {
    /// Opens the default output device. In a browser this has to happen in
    /// response to a click (browsers only start audio after one).
    pub fn open() -> Result<Self, String> {
        let host = cpal::default_host();
        let device = host.default_output_device().ok_or("no audio output device")?;
        let supported = device.default_output_config().map_err(|e| format!("audio output: {e}"))?;
        if supported.sample_format() != cpal::SampleFormat::F32 {
            return Err(format!("audio output: unsupported sample format {:?}", supported.sample_format()));
        }
        let config: cpal::StreamConfig = supported.into();
        let channels = config.channels as usize;
        let jitter = Arc::new(Mutex::new(Jitter {
            buf: VecDeque::new(),
            rate: 0,
            device_rate: config.sample_rate,
            pos: 0.0,
            playing: false,
            gain: 1.0,
        }));
        let feed = Arc::clone(&jitter);
        let stream = device
            .build_output_stream(
                &config,
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    let mut j = feed.lock().unwrap();
                    for frame in data.chunks_mut(channels) {
                        let v = j.next();
                        frame.fill(v);
                    }
                },
                |e| log::warn!("audio output: {e}"),
                None,
            )
            .map_err(|e| format!("audio output: {e}"))?;
        stream.play().map_err(|e| format!("audio output: {e}"))?;
        Ok(Self { jitter, _stream: stream })
    }

    pub fn push(&self, samples: &[i16], rate: u32) {
        self.jitter.lock().unwrap().push(samples, rate);
    }

    pub fn set_gain(&self, gain: f32) {
        self.jitter.lock().unwrap().gain = gain;
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
        Jitter { buf: VecDeque::new(), rate: 0, device_rate, pos: 0.0, playing: false, gain: 1.0 }
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
