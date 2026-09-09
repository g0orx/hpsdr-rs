/*
    Single-signal CW (Morse) decoder -- turns tone-on/tone-off timing
    on the demodulated RX audio into decoded text. Deliberately NOT a
    multi-signal "skimmer": this only decodes whatever one CW signal is
    actually tuned in and audible, the same as a human ear would.

    Frequency selectivity is a non-problem here: spectrum::passband_for
    centers Mode::Cwl/Cwu on a fixed 600Hz pitch, and WDSP's own RXA
    filter (a ~200Hz-wide passband by default) has therefore already
    band-limited the audio this module sees to just that tone before it
    ever reaches here -- no Goertzel/FFT tone detection needed, just an
    envelope follower on the pre-filtered audio.
*/

use std::sync::{Arc, Mutex};

/// Matches spectrum::DSP_RATE -- WDSP's RXA output is always 48kHz
/// regardless of the receiver's actual ADC sample rate.
const SAMPLE_RATE_HZ: f32 = 48_000.0;

/// Hard cap on the shared decoded-text buffer -- same "bounded,
/// drop-oldest" reasoning as AUDIO_BUFFER_CAPACITY/WATERFALL_HISTORY
/// elsewhere in spectrum.rs; a long CW session shouldn't grow this
/// without bound.
const MAX_TEXT_LEN: usize = 2000;

/// Unit-time (dot length) clamp range, in samples -- the standard
/// PARIS timing convention (dot_ms = 1200 / wpm) bounded to a 5-60 WPM
/// range so a burst of noise can't drag the adaptive estimate outside
/// anything a real CW operator would actually send at.
const MIN_UNIT_SAMPLES: f32 = SAMPLE_RATE_HZ * 0.020; // 60 WPM
const MAX_UNIT_SAMPLES: f32 = SAMPLE_RATE_HZ * 0.240; // 5 WPM

/// Debounce floor -- a candidate state flip shorter than this is
/// treated as a noise glitch and folded back into whichever state was
/// already committed, regardless of the current WPM estimate, so an
/// isolated click can't masquerade as a dot even at 60 WPM (where a
/// real dot is only 20ms).
const MIN_RUN_SAMPLES: f32 = SAMPLE_RATE_HZ * 0.008; // 8ms

pub struct CwDecoder {
    text_out: Arc<Mutex<String>>,

    // Envelope follower state.
    envelope: f32,

    // Adaptive threshold state -- see process_sample's own comments.
    noise_floor: f32,
    signal_peak: f32,

    // Debounced tone-on/off state machine.
    tone_on: bool,
    run_samples: u32,
    candidate_run: u32,

    // Adaptive unit-time (dot length) estimate, in samples.
    unit_samples: f32,

    // Accumulated '.'/'-' for the character currently being spelled.
    symbol: String,
}

impl CwDecoder {
    pub fn new(text_out: Arc<Mutex<String>>) -> Self {
        Self {
            text_out,
            envelope: 0.0,
            noise_floor: 0.0,
            signal_peak: 0.0,
            tone_on: false,
            run_samples: 0,
            candidate_run: 0,
            unit_samples: SAMPLE_RATE_HZ * 0.060, // starting guess: ~20 WPM
            symbol: String::new(),
        }
    }

    /// Clears in-flight timing/symbol state -- call once when leaving
    /// CW mode. Deliberately does NOT touch text_out: switching away
    /// from CW mid-QSO (even briefly) shouldn't erase what's already
    /// been decoded, only a fresh connect or the UI's own Clear button
    /// should do that.
    pub fn reset(&mut self) {
        self.envelope = 0.0;
        self.noise_floor = 0.0;
        self.signal_peak = 0.0;
        self.tone_on = false;
        self.run_samples = 0;
        self.candidate_run = 0;
        self.symbol.clear();
    }

    /// Feeds one demodulated audio sample (48kHz, already CW-filtered
    /// by WDSP's own RXA chain -- see this file's own top comment)
    /// through the envelope follower, adaptive threshold, and timing
    /// classifier. Call once per sample while the receiver is actually
    /// in Mode::Cwl/Cwu.
    pub fn process_sample(&mut self, sample: f32) {
        // Asymmetric one-pole envelope follower: fast attack, slower
        // release, so it tracks a dit's rising/falling edge without
        // chasing the underlying audio-rate wiggle inside a steady tone.
        const ATTACK: f32 = 0.35; // ~1ms time constant at 48kHz
        const RELEASE: f32 = 0.08; // ~4-5ms time constant at 48kHz
        let rectified = sample.abs();
        if rectified > self.envelope {
            self.envelope += ATTACK * (rectified - self.envelope);
        } else {
            self.envelope += RELEASE * (rectified - self.envelope);
        }

        // Adaptive noise floor / signal peak, each its own slow moving
        // average updated only on its own side of the CURRENT
        // threshold, so they track their respective level instead of
        // blending into each other -- self-adjusts to whatever level
        // WDSP's own AGC (already upstream, inside the RXA chain)
        // happens to settle the audio at, rather than assuming a fixed
        // dB level.
        const FLOOR_ALPHA: f32 = 0.001;
        const PEAK_ALPHA: f32 = 0.01;
        let threshold = (self.noise_floor + self.signal_peak) * 0.5;
        if self.envelope < threshold {
            self.noise_floor += FLOOR_ALPHA * (self.envelope - self.noise_floor);
        } else {
            self.signal_peak += PEAK_ALPHA * (self.envelope - self.signal_peak);
        }
        // Keeps signal_peak meaningfully above noise_floor even through
        // a long silence (e.g. between overs), so a fresh signal isn't
        // fighting a threshold that's collapsed toward the noise floor
        // from both sides at once.
        let min_peak = self.noise_floor * 1.5 + 1e-6;
        if self.signal_peak < min_peak {
            self.signal_peak = min_peak;
        }

        let tone_now = self.envelope > threshold;

        if tone_now == self.tone_on {
            // Matches the committed state -- fold in (rather than
            // discard) any pending candidate run of the opposite state
            // that never reached the debounce floor, since it was
            // genuinely part of this same committed run.
            self.run_samples += 1 + self.candidate_run;
            self.candidate_run = 0;
            return;
        }

        self.candidate_run += 1;
        if (self.candidate_run as f32) < MIN_RUN_SAMPLES {
            return;
        }

        // Debounce floor cleared -- this is a real transition. The run
        // that just ended (run_samples, under the OLD committed state)
        // gets classified now; the new state starts counting from
        // candidate_run (the samples already seen of it).
        let ended_run = self.run_samples;
        self.tone_on = tone_now;
        self.run_samples = self.candidate_run;
        self.candidate_run = 0;

        if tone_now {
            // Tone just started -- the ended run was silence.
            self.classify_gap(ended_run);
        } else {
            // Tone just ended -- the ended run was a tone.
            self.classify_tone(ended_run);
        }
    }

    fn classify_tone(&mut self, run_samples: u32) {
        let run = run_samples as f32;
        if run < self.unit_samples * 2.0 {
            self.symbol.push('.');
            // Dots are the most reliable sync reference for the
            // unit-time estimate -- the smallest element, least
            // distorted by an operator's own timing variation or QSB --
            // so only dots (not dashes) nudge the estimate.
            self.nudge_unit_time(run);
        } else {
            self.symbol.push('-');
        }
    }

    fn classify_gap(&mut self, run_samples: u32) {
        let run = run_samples as f32;
        if run < self.unit_samples * 2.0 {
            // Intra-character gap -- not a boundary, nothing to do.
            return;
        }
        self.resolve_symbol();
        if run >= self.unit_samples * 6.0 {
            self.push_text(' ');
        }
    }

    fn nudge_unit_time(&mut self, dot_samples: f32) {
        const ALPHA: f32 = 0.15;
        self.unit_samples += ALPHA * (dot_samples - self.unit_samples);
        self.unit_samples = self.unit_samples.clamp(MIN_UNIT_SAMPLES, MAX_UNIT_SAMPLES);
    }

    fn resolve_symbol(&mut self) {
        if self.symbol.is_empty() {
            return;
        }
        if let Some(c) = morse_lookup(&self.symbol) {
            self.push_text(c);
        }
        self.symbol.clear();
    }

    fn push_text(&mut self, c: char) {
        let mut text = self.text_out.lock().unwrap();
        text.push(c);
        let len = text.chars().count();
        if len > MAX_TEXT_LEN {
            let drop_chars = len - MAX_TEXT_LEN;
            if let Some((byte_idx, _)) = text.char_indices().nth(drop_chars) {
                text.drain(..byte_idx);
            }
        }
    }
}

/// Standard International Morse code -- letters, digits, and a few
/// common punctuation marks. Prosigns (AR, SK, BT, etc.) aren't
/// decoded; an unrecognized dot/dash sequence is simply dropped rather
/// than shown as some placeholder character.
fn morse_lookup(symbol: &str) -> Option<char> {
    Some(match symbol {
        ".-" => 'A',
        "-..." => 'B',
        "-.-." => 'C',
        "-.." => 'D',
        "." => 'E',
        "..-." => 'F',
        "--." => 'G',
        "...." => 'H',
        ".." => 'I',
        ".---" => 'J',
        "-.-" => 'K',
        ".-.." => 'L',
        "--" => 'M',
        "-." => 'N',
        "---" => 'O',
        ".--." => 'P',
        "--.-" => 'Q',
        ".-." => 'R',
        "..." => 'S',
        "-" => 'T',
        "..-" => 'U',
        "...-" => 'V',
        ".--" => 'W',
        "-..-" => 'X',
        "-.--" => 'Y',
        "--.." => 'Z',
        "-----" => '0',
        ".----" => '1',
        "..---" => '2',
        "...--" => '3',
        "....-" => '4',
        "....." => '5',
        "-...." => '6',
        "--..." => '7',
        "---.." => '8',
        "----." => '9',
        ".-.-.-" => '.',
        "--..--" => ',',
        "..--.." => '?',
        "-..-." => '/',
        "-...-" => '=',
        _ => return None,
    })
}
