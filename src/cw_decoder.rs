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

use std::collections::VecDeque;
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

/// How many of the most recent tone durations (dots AND dashes both --
/// see classify_tone's own comment) feed the unit-time estimate.
/// Bounds how long a single bad reading (a misclassified element
/// while there's not yet been a real dot to compare against, or a
/// noise glitch) can skew the estimate: it only pollutes the window
/// for this many more elements, then ages out on its own, unlike an
/// unbounded exponential-moving-average estimator that can drift
/// permanently in one direction with no way back (see this file's own
/// history: a real report showed a too-slow cold-start guess
/// misclassifying real dashes as dots for several seconds, and a
/// first attempt at fixing that -- nudging on every tone, not just
/// ones already classified as a dot -- caused a WORSE regression,
/// collapsing the estimate toward the debounce floor with no way back
/// on a single bad reading).
const WINDOW_CAP: usize = 8;

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

    // Adaptive unit-time (dot length) estimate, in samples -- the
    // minimum of recent_tones below, computed fresh after every tone
    // (see classify_tone). Dots are the shortest element by
    // definition, so tracking the minimum over a real recent window
    // is a description of reality that doesn't depend on any
    // classification threshold being right yet -- see WINDOW_CAP's own
    // doc comment for why a bounded window, not an unbounded EMA.
    unit_samples: f32,
    recent_tones: VecDeque<f32>,

    // Accumulated '.'/'-' for the character currently being spelled.
    symbol: String,

    // Whether a word-space has already been emitted for the silence
    // run currently in progress -- see process_sample's own comment on
    // why gap classification is eager (during the silence) rather than
    // reactive (on the next tone start), which is also why this can't
    // just be inferred from `symbol` being empty the way the
    // character-boundary case can.
    word_space_emitted: bool,
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
            unit_samples: SAMPLE_RATE_HZ * 0.060, // starting guess: ~20 WPM, until recent_tones has real data
            recent_tones: VecDeque::with_capacity(WINDOW_CAP),
            symbol: String::new(),
            word_space_emitted: false,
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
        self.unit_samples = SAMPLE_RATE_HZ * 0.060;
        self.recent_tones.clear();
        self.symbol.clear();
        self.word_space_emitted = false;
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

            // Gap classification is done EAGERLY, as the silence is
            // still ongoing, rather than reactively once a new tone
            // starts: a signal that just stops (end of a transmission,
            // a fade) would otherwise leave the last character's
            // dots/dashes sitting in `symbol` forever, since nothing
            // would ever trigger the tone-start transition that used
            // to be the only place a character got resolved. Real bug
            // report: the last character before a break in the signal
            // never appeared in the decoded text.
            if !self.tone_on {
                let silence = self.run_samples as f32;
                if silence >= self.unit_samples * 2.0 {
                    self.resolve_symbol();
                }
                if !self.word_space_emitted && silence >= self.unit_samples * 6.0 {
                    self.push_text(' ');
                    self.word_space_emitted = true;
                }
            }
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
            // Tone just started -- nothing to do here: any character/
            // word boundary in the silence that just ended was already
            // resolved eagerly above, while that silence was ongoing.
        } else {
            // Tone just ended -- the ended run was a tone. Entering a
            // fresh silence run, so a new word-space (if warranted) can
            // be emitted for it.
            self.classify_tone(ended_run);
            self.word_space_emitted = false;
        }
    }

    /// Classifies one just-ended tone as a dot or dash, and updates the
    /// unit-time estimate from it first -- see WINDOW_CAP's own doc
    /// comment for why a bounded sliding-window minimum, and why this
    /// pushes EVERY tone into that window (not just ones already
    /// classified as a dot the way the two previous, reverted schemes
    /// did): dots are the shortest element by definition, so as soon
    /// as one real dot is anywhere in the last WINDOW_CAP elements, the
    /// window's minimum reflects the true unit time regardless of how
    /// confused earlier classifications were. Validated against a real
    /// recording (a fast, ~33 WPM CQ call) that used to misclassify
    /// several seconds' worth of letters after a too-slow cold-start
    /// guess -- with this scheme, only the first one or two elements
    /// (before any real dot has been seen at all) are still at risk;
    /// everything after self-corrects within a handful of elements.
    fn classify_tone(&mut self, run_samples: u32) {
        let run = run_samples as f32;

        if self.recent_tones.len() >= WINDOW_CAP {
            self.recent_tones.pop_front();
        }
        self.recent_tones.push_back(run);
        // Wait for a second data point before trusting the window --
        // a single element (which could easily be this transmission's
        // first dash) is no better a unit-time estimate than the
        // cold-start guess it would replace.
        if self.recent_tones.len() >= 2 {
            let window_min = self.recent_tones.iter().copied().fold(f32::MAX, f32::min);
            self.unit_samples = window_min.clamp(MIN_UNIT_SAMPLES, MAX_UNIT_SAMPLES);
        }

        if run < self.unit_samples * 2.0 {
            self.symbol.push('.');
        } else {
            self.symbol.push('-');
        }
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
