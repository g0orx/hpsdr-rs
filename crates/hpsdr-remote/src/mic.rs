/*
    TX audio in: the default microphone, mono, brought down to the rate the
    client sends at (hpsdr_proto::tx::TX_AUDIO_RATE) by averaging -- enough
    of a low-pass for a voice channel the transmitter filters to 3 kHz
    anyway. Natively through cpal; in a browser through the Web Audio API
    (cpal's browser backend is output only), which asks for permission and
    only works on an HTTPS page.

    Opened on the first MOX press -- browsers want a click before audio and
    a permission prompt -- and kept open; the app drains it continually and
    sends only while transmitting.
*/

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use hpsdr_proto::tx::TX_AUDIO_RATE;

/// At most this much is kept if the app falls behind draining it.
const MAX_S: f32 = 0.5;

#[derive(Default)]
struct Shared {
    buf: VecDeque<f32>,
    /// The averaging decimator: sum and count so far, and the place in the
    /// output period.
    sum: f32,
    count: u32,
    phase: f64,
    /// Peak of the input since last read, for the level meter.
    peak: f32,
    /// Why it isn't working (the browser opens it asynchronously).
    error: Option<String>,
}

impl Shared {
    fn push(&mut self, mono: impl Iterator<Item = f32>, rate: u32) {
        let step = TX_AUDIO_RATE as f64 / rate.max(TX_AUDIO_RATE) as f64;
        for x in mono {
            self.peak = self.peak.max(x.abs());
            self.sum += x;
            self.count += 1;
            self.phase += step;
            if self.phase >= 1.0 {
                self.phase -= 1.0;
                self.buf.push_back(self.sum / self.count as f32);
                self.sum = 0.0;
                self.count = 0;
            }
        }
        let max = (MAX_S * TX_AUDIO_RATE as f32) as usize;
        if self.buf.len() > max {
            let excess = self.buf.len() - max;
            self.buf.drain(..excess);
        }
    }
}

pub struct Mic {
    shared: Arc<Mutex<Shared>>,
    #[cfg(not(target_arch = "wasm32"))]
    _stream: cpal::Stream,
}

impl Mic {
    /// Opens the default microphone. In a browser it opens in the
    /// background: a refusal shows later, through error().
    pub fn open() -> Result<Self, String> {
        let shared = Arc::new(Mutex::new(Shared::default()));
        #[cfg(not(target_arch = "wasm32"))]
        {
            let stream = native::open(Arc::clone(&shared))?;
            Ok(Self { shared, _stream: stream })
        }
        #[cfg(target_arch = "wasm32")]
        {
            web::open(Arc::clone(&shared))?;
            Ok(Self { shared })
        }
    }

    /// Everything captured since the last call, at TX_AUDIO_RATE.
    pub fn take(&self) -> Vec<f32> {
        self.shared.lock().unwrap().buf.drain(..).collect()
    }

    /// Peak input level since the last call, 0 to 1.
    pub fn take_peak(&self) -> f32 {
        std::mem::take(&mut self.shared.lock().unwrap().peak)
    }

    pub fn error(&self) -> Option<String> {
        self.shared.lock().unwrap().error.clone()
    }
}

#[cfg(not(target_arch = "wasm32"))]
mod native {
    use super::*;
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

    pub fn open(shared: Arc<Mutex<Shared>>) -> Result<cpal::Stream, String> {
        let device = cpal::default_host().default_input_device().ok_or("no microphone")?;
        let supported = device.default_input_config().map_err(|e| format!("microphone: {e}"))?;
        let format = supported.sample_format();
        let config: cpal::StreamConfig = supported.into();
        let (channels, rate) = (config.channels as usize, config.sample_rate);
        let error = |e| log::warn!("microphone: {e}");
        let stream = match format {
            cpal::SampleFormat::F32 => device.build_input_stream(
                &config,
                move |data: &[f32], _: &cpal::InputCallbackInfo| {
                    shared.lock().unwrap().push(data.chunks(channels).map(|f| f[0]), rate);
                },
                error,
                None,
            ),
            cpal::SampleFormat::I16 => device.build_input_stream(
                &config,
                move |data: &[i16], _: &cpal::InputCallbackInfo| {
                    shared.lock().unwrap().push(data.chunks(channels).map(|f| f[0] as f32 / 32768.0), rate);
                },
                error,
                None,
            ),
            other => return Err(format!("microphone: unsupported sample format {other:?}")),
        }
        .map_err(|e| format!("microphone: {e}"))?;
        stream.play().map_err(|e| format!("microphone: {e}"))?;
        Ok(stream)
    }
}

#[cfg(target_arch = "wasm32")]
mod web {
    use super::*;
    use wasm_bindgen::prelude::*;
    use wasm_bindgen::JsCast;
    use wasm_bindgen_futures::JsFuture;
    use web_sys::{AudioContext, AudioProcessingEvent, MediaStream, MediaStreamConstraints};

    /// The AudioContext is made here, during the click, so the browser
    /// lets it run; the permission request then finishes in the background.
    pub fn open(shared: Arc<Mutex<Shared>>) -> Result<(), String> {
        let context = AudioContext::new().map_err(|e| format!("microphone: {e:?}"))?;
        wasm_bindgen_futures::spawn_local(async move {
            if let Err(e) = start(&context, Arc::clone(&shared)).await {
                shared.lock().unwrap().error = Some(e);
            }
        });
        Ok(())
    }

    async fn start(context: &AudioContext, shared: Arc<Mutex<Shared>>) -> Result<(), String> {
        let window = web_sys::window().ok_or("microphone: no window")?;
        // Missing altogether on a page that isn't HTTPS.
        let devices = window
            .navigator()
            .media_devices()
            .map_err(|_| "the microphone needs the page on HTTPS (Settings -> Network -> Web client)".to_string())?;
        let constraints = MediaStreamConstraints::new();
        constraints.set_audio(&JsValue::TRUE);
        let request = devices.get_user_media_with_constraints(&constraints).map_err(|e| format!("microphone: {e:?}"))?;
        let stream: MediaStream = JsFuture::from(request)
            .await
            .map_err(|_| "microphone: not allowed (check the browser's permission for this page)".to_string())?
            .dyn_into()
            .map_err(|_| "microphone: no stream".to_string())?;
        let source = context.create_media_stream_source(&stream).map_err(|e| format!("microphone: {e:?}"))?;
        // ScriptProcessorNode: deprecated, but everywhere (iOS Safari
        // included) and needs no separate worklet script.
        let processor = context
            .create_script_processor_with_buffer_size_and_number_of_input_channels_and_number_of_output_channels(
                2048, 1, 1,
            )
            .map_err(|e| format!("microphone: {e:?}"))?;
        let rate = context.sample_rate() as u32;
        let on_audio = Closure::<dyn FnMut(AudioProcessingEvent)>::new(move |e: AudioProcessingEvent| {
            if let Ok(data) = e.input_buffer().and_then(|b| b.get_channel_data(0)) {
                shared.lock().unwrap().push(data.into_iter(), rate);
            }
        });
        processor.set_onaudioprocess(Some(on_audio.as_ref().unchecked_ref()));
        // Lives as long as the page: the microphone stays open.
        on_audio.forget();
        source.connect_with_audio_node(&processor).map_err(|e| format!("microphone: {e:?}"))?;
        // Some browsers only run a processor that goes somewhere; it
        // outputs silence.
        processor
            .connect_with_audio_node(&context.destination())
            .map_err(|e| format!("microphone: {e:?}"))?;
        let _ = context.resume();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 48 kHz comes out at 16 kHz, a steady level unchanged.
    #[test]
    fn brings_the_rate_down_by_averaging() {
        let mut s = Shared::default();
        s.push(std::iter::repeat_n(0.5, 4800), 48_000);
        assert_eq!(s.buf.len(), 1600);
        assert!(s.buf.iter().all(|&v| (v - 0.5).abs() < 1e-6));
        assert_eq!(s.peak, 0.5);
    }
}
