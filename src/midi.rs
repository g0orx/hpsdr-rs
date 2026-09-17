//! MIDI control-surface support: any class-compliant MIDI controller's
//! notes/CCs/pitch-bend can be "learned" (Settings -> MIDI) and bound to a
//! radio action.
//!
//! Split the way piHPSDR's own MIDI support is split (this project's own
//! author has maintained that split for years: midi.h/midi2.c/midi_menu.c),
//! but collapsed from three layers into two pieces:
//!
//! - Layer 1 (this module): hardware I/O only. `MidiWorker` owns a `midir`
//!   connection, (re)connecting by device name every ~1s since neither
//!   ALSA/CoreMIDI/WinMM nor `midir` itself gives an unplug/replug
//!   notification -- a controller that comes back after a cable wiggle
//!   must reconnect without the operator doing anything. Raw bytes are
//!   parsed into a `RawMidiEvent` and pushed onto a bounded, drop-oldest
//!   queue; nothing here knows what a "binding" or an "action" is.
//! - Layers 2 (binding lookup) and 3 (action dispatch) live in main.rs's
//!   existing per-frame loop, which drains that queue. This follows the
//!   same "background thread writes an intent, the frame loop drains and
//!   acts" convention this codebase already uses for CAT/rigctl's
//!   cw_remote_pending queue (see rigctl.rs's own module doc comment) --
//!   there's no need for this thread to hold a clone of the binding table
//!   or touch any RadioSession atomics directly.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Client name this app registers under, as other MIDI software sees it.
const CLIENT_NAME: &str = "hpsdr-rs";
/// How often the worker re-scans for the configured device by name.
const RESCAN_INTERVAL: Duration = Duration::from_secs(1);
/// How often the worker wakes while idle (disabled, or no device chosen).
const IDLE_TICK: Duration = Duration::from_millis(200);
/// Depth of the inbound event queue. A UI that isn't draining (not
/// connected to a radio yet) drops the oldest event rather than growing
/// without bound or blocking the MIDI driver's own callback thread.
const QUEUE_CAPACITY: usize = 64;

/// Which kind of MIDI status byte a `RawMidiEvent` came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MidiEventKind {
    /// Note On/Off (a button/pad). `value` is velocity; a Note On at
    /// velocity 0 is conventionally a release too (some controllers never
    /// send a real Note Off byte), folded in by `parse_midi_bytes`.
    NoteKey,
    /// Control Change (a knob/slider/wheel). `value` is 0-127.
    ControlChange,
    /// Pitch Bend (a single dedicated slider on some controllers).
    /// `value` is the coarse 0-127 position (high 7 bits of the 14-bit
    /// wire value) -- plenty of resolution for a UI control, and it
    /// keeps `value`'s type the same as every other event kind.
    PitchBend,
}

/// One MIDI message decoded off the wire, with the binding-relevant parts
/// only -- this is what both learn mode and binding lookup match against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawMidiEvent {
    pub kind: MidiEventKind,
    /// 0-15.
    pub channel: u8,
    /// Note number or CC number. Unused (0) for PitchBend, which has only
    /// one "control" per channel.
    pub number: u8,
    /// 0-127.
    pub value: u8,
    /// True for a Note Off (including a velocity-0 Note On). Always false
    /// for ControlChange/PitchBend.
    pub off: bool,
}

/// Parse one MIDI message. Returns `None` for anything that cannot carry a
/// binding: system messages (0xF0-0xFF: sysex, clock, active sensing),
/// (poly/channel) aftertouch, program change, or a malformed/short buffer.
fn parse_midi_bytes(bytes: &[u8]) -> Option<RawMidiEvent> {
    let status = *bytes.first()?;
    if !(0x80..0xF0).contains(&status) {
        return None;
    }
    let channel = status & 0x0F;
    let d1 = bytes.get(1).copied().unwrap_or(0) & 0x7F;
    let d2 = bytes.get(2).copied().unwrap_or(0) & 0x7F;
    match status & 0xF0 {
        0x80 => Some(RawMidiEvent { kind: MidiEventKind::NoteKey, channel, number: d1, value: 0, off: true }),
        0x90 => Some(RawMidiEvent {
            kind: MidiEventKind::NoteKey,
            channel,
            number: d1,
            value: d2,
            off: d2 == 0,
        }),
        0xB0 => {
            Some(RawMidiEvent { kind: MidiEventKind::ControlChange, channel, number: d1, value: d2, off: false })
        }
        0xE0 => {
            // 14-bit little-endian; a binding only needs the coarse position.
            Some(RawMidiEvent { kind: MidiEventKind::PitchBend, channel, number: 0, value: d2, off: false })
        }
        _ => None,
    }
}

/// A radio function a MIDI control can be bound to. Deliberately a small
/// subset of piHPSDR's ~90 actions -- see the MIDI support plan for which
/// ones were left out for v1 and why (mostly: setup/calibration-time
/// controls like PureSignal/Diversity/Equalizer, and anything tied to a
/// per-receiver concept this app doesn't have).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MidiAction {
    Mox,
    Tune,
    Split,
    RitToggle,
    RitClear,
    XitToggle,
    XitClear,
    VfoAtoB,
    VfoBtoA,
    VfoSwap,
    ModeUp,
    ModeDown,
    BandUp,
    BandDown,
    FilterWidthUp,
    FilterWidthDown,
    VfoStepUp,
    VfoStepDown,
    NoiseBlankerCycle,
    NoiseReductionCycle,
    AfGain,
    AgcGain,
    MicGain,
    RfAttenuation,
    TxDrive,
    CwSpeed,
    FilterWidth,
    VfoTune,
    RitAdjust,
    XitAdjust,
    VfoBTune,
    CtunToggle,
    RxEqToggle,
    DiversityToggle,
    BinauralToggle,
    SnbToggle,
    Band160m,
    Band80m,
    Band40m,
    Band30m,
    Band20m,
    Band17m,
    Band15m,
    Band12m,
    Band10m,
    Band6m,
}

impl MidiAction {
    pub fn label(self) -> &'static str {
        match self {
            MidiAction::Mox => "MOX (PTT)",
            MidiAction::Tune => "Tune",
            MidiAction::Split => "Split",
            MidiAction::RitToggle => "RIT On/Off",
            MidiAction::RitClear => "RIT Clear",
            MidiAction::XitToggle => "XIT On/Off",
            MidiAction::XitClear => "XIT Clear",
            MidiAction::VfoAtoB => "VFO A -> B",
            MidiAction::VfoBtoA => "VFO B -> A",
            MidiAction::VfoSwap => "VFO A/B Swap",
            MidiAction::ModeUp => "Mode Up",
            MidiAction::ModeDown => "Mode Down",
            MidiAction::BandUp => "Band Up",
            MidiAction::BandDown => "Band Down",
            MidiAction::FilterWidthUp => "Filter Width Up",
            MidiAction::FilterWidthDown => "Filter Width Down",
            MidiAction::VfoStepUp => "VFO Step Up",
            MidiAction::VfoStepDown => "VFO Step Down",
            MidiAction::NoiseBlankerCycle => "Noise Blanker Cycle",
            MidiAction::NoiseReductionCycle => "Noise Reduction Cycle",
            MidiAction::AfGain => "AF Gain",
            MidiAction::AgcGain => "AGC Gain",
            MidiAction::MicGain => "Mic Gain",
            MidiAction::RfAttenuation => "RF Attenuation",
            MidiAction::TxDrive => "TX Drive",
            MidiAction::CwSpeed => "CW Speed",
            MidiAction::FilterWidth => "Filter Width",
            MidiAction::VfoTune => "VFO Tune",
            MidiAction::RitAdjust => "RIT Adjust",
            MidiAction::XitAdjust => "XIT Adjust",
            MidiAction::VfoBTune => "VFO B Tune",
            MidiAction::CtunToggle => "CTUN On/Off",
            MidiAction::RxEqToggle => "RX Equalizer On/Off",
            MidiAction::DiversityToggle => "Diversity On/Off",
            MidiAction::BinauralToggle => "Binaural On/Off",
            MidiAction::SnbToggle => "Spectral Noise Blanker On/Off",
            MidiAction::Band160m => "Band 160m",
            MidiAction::Band80m => "Band 80m",
            MidiAction::Band40m => "Band 40m",
            MidiAction::Band30m => "Band 30m",
            MidiAction::Band20m => "Band 20m",
            MidiAction::Band17m => "Band 17m",
            MidiAction::Band15m => "Band 15m",
            MidiAction::Band12m => "Band 12m",
            MidiAction::Band10m => "Band 10m",
            MidiAction::Band6m => "Band 6m",
        }
    }
}

/// Actions valid for a Key (button) binding.
pub const KEY_ACTIONS: &[MidiAction] = &[
    MidiAction::Mox,
    MidiAction::Tune,
    MidiAction::Split,
    MidiAction::RitToggle,
    MidiAction::RitClear,
    MidiAction::XitToggle,
    MidiAction::XitClear,
    MidiAction::VfoAtoB,
    MidiAction::VfoBtoA,
    MidiAction::VfoSwap,
    MidiAction::ModeUp,
    MidiAction::ModeDown,
    MidiAction::BandUp,
    MidiAction::BandDown,
    MidiAction::FilterWidthUp,
    MidiAction::FilterWidthDown,
    MidiAction::VfoStepUp,
    MidiAction::VfoStepDown,
    MidiAction::NoiseBlankerCycle,
    MidiAction::NoiseReductionCycle,
    MidiAction::CtunToggle,
    MidiAction::RxEqToggle,
    MidiAction::DiversityToggle,
    MidiAction::BinauralToggle,
    MidiAction::SnbToggle,
    MidiAction::Band160m,
    MidiAction::Band80m,
    MidiAction::Band40m,
    MidiAction::Band30m,
    MidiAction::Band20m,
    MidiAction::Band17m,
    MidiAction::Band15m,
    MidiAction::Band12m,
    MidiAction::Band10m,
    MidiAction::Band6m,
];

/// Actions valid for a Knob (absolute value) binding.
pub const KNOB_ACTIONS: &[MidiAction] = &[
    MidiAction::AfGain,
    MidiAction::AgcGain,
    MidiAction::MicGain,
    MidiAction::RfAttenuation,
    MidiAction::TxDrive,
    MidiAction::CwSpeed,
    MidiAction::FilterWidth,
];

/// Actions valid for a Wheel (relative encoder) binding.
pub const WHEEL_ACTIONS: &[MidiAction] =
    &[MidiAction::VfoTune, MidiAction::RitAdjust, MidiAction::XitAdjust, MidiAction::VfoBTune];

/// How a ControlChange/PitchBend binding's value should be interpreted.
/// (A Key/Note binding has no ambiguity, so this only matters for CC and
/// PitchBend bindings.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MidiBindingKind {
    /// Note On/Off -- a button.
    Key,
    /// Absolute value 0-127, e.g. a slider/fader or rotary-with-detent.
    Knob,
    /// Relative direction+speed, e.g. an endless rotary encoder that
    /// centers its value around 64 (piHPSDR's own convention, used
    /// as-is here: `value as i16 - 64` is the signed step).
    Wheel,
}

/// One user-configured note/CC-to-action mapping. Persisted in
/// `Config::midi_bindings`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct MidiBinding {
    pub event: MidiEventKind,
    /// `None` = any channel (piHPSDR's channel `-1`).
    pub channel: Option<u8>,
    pub number: u8,
    pub kind: MidiBindingKind,
    pub action: MidiAction,
    /// Key bindings only: fire on both press AND release (piHPSDR's
    /// "ONOFF" modifier), for a control meant to be held down -- e.g.
    /// binding Mox to ONOFF gives press-to-transmit/release-to-receive
    /// instead of toggle-on-each-press.
    #[serde(default)]
    pub momentary: bool,
    /// Wheel bindings only: multiplies the base Hz-per-unit step (see
    /// dispatch_midi_event's VfoTune/RitAdjust/XitAdjust arms in main.rs).
    /// A relative MIDI encoder's per-message delta magnitude reflects how
    /// sensitive/high-resolution that specific piece of hardware is, not
    /// a calibrated "one click" unit the way a mouse-wheel notch is --
    /// e.g. a real report: a continuous (no-detent) jog wheel moved the
    /// VFO several kHz on barely a touch under the mouse-wheel-derived
    /// step size this used before sensitivity existed. Exposed as a
    /// Settings -> MIDI slider so it can be dialed in per-controller
    /// without a code change. `#[serde(default = "default_sensitivity")]`
    /// (not plain `#[serde(default)]`, which would zero it out) so
    /// configs saved before this field existed load at the old fixed
    /// behavior (1.0), not silently frozen at 0.
    #[serde(default = "default_sensitivity")]
    pub sensitivity: f32,
    /// Wheel bindings only: minimum time (ms) between two applied steps
    /// from THIS binding -- any further messages arriving sooner are
    /// dropped outright, not queued/coalesced. 0 = no limit (every
    /// message applies a step), which is also what a config saved
    /// before this field existed loads as via `#[serde(default)]`.
    ///
    /// This exists because lowering the per-message step size
    /// (`sensitivity`, or the base Hz/message itself) alone could not
    /// fix a real report: a continuous, no-detent jog wheel apparently
    /// sends a very large NUMBER of relative messages even for a brief,
    /// light touch, and no per-message step size is small enough to stay
    /// controllable against an unbounded burst of them arriving within
    /// a few milliseconds. A time-based rate limit bounds the worst case
    /// to `1000/debounce_ms` steps per second regardless of how chatty a
    /// specific piece of hardware is -- piHPSDR's own MIDI support has
    /// the identical control (its `desc->delay`, see midi2.c) for this
    /// exact reason, "it is difficult to hit the correct [target] if
    /// wheel events are generated at a very high rate."
    #[serde(default)]
    pub debounce_ms: u32,
}

fn default_sensitivity() -> f32 {
    1.0
}

impl MidiBinding {
    /// Whether `ev` matches this binding's event kind/channel/number.
    /// Does NOT check `off` -- callers decide whether a release should be
    /// acted on (see `momentary`).
    pub fn matches(&self, ev: &RawMidiEvent) -> bool {
        self.event == ev.kind
            && self.number == ev.number
            && self.channel.is_none_or(|c| c == ev.channel)
    }
}

/// Live connection state, shown in the Settings -> MIDI page.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum MidiStatus {
    #[default]
    Disabled,
    Searching,
    Connected(String),
    Error(String),
}

/// The app's handle to the MIDI worker thread: a live enable flag, a live
/// target device name, a status readback, and the inbound event queue.
/// Changing `enabled`/`device_name` takes effect on the worker's next
/// tick -- no restart needed, same pattern as `audio::CwSidetone::enabled`.
pub struct MidiWorker {
    pub enabled: Arc<AtomicBool>,
    pub device_name: Arc<Mutex<Option<String>>>,
    pub status: Arc<Mutex<MidiStatus>>,
    pub events: Arc<Mutex<std::collections::VecDeque<RawMidiEvent>>>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl MidiWorker {
    pub fn start() -> Self {
        let enabled = Arc::new(AtomicBool::new(false));
        let device_name = Arc::new(Mutex::new(None));
        let status = Arc::new(Mutex::new(MidiStatus::Disabled));
        let events = Arc::new(Mutex::new(std::collections::VecDeque::new()));
        let stop = Arc::new(AtomicBool::new(false));

        let thread_enabled = Arc::clone(&enabled);
        let thread_device_name = Arc::clone(&device_name);
        let thread_status = Arc::clone(&status);
        let thread_events = Arc::clone(&events);
        let thread_stop = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            run(thread_enabled, thread_device_name, thread_status, thread_events, thread_stop);
        });

        Self { enabled, device_name, status, events, stop, thread: Some(thread) }
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for MidiWorker {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Input port names currently visible to the OS MIDI stack, for the
/// Settings -> MIDI device dropdown. Queried fresh on demand (cheap, and
/// a controller can appear/disappear between frames), same idea as this
/// app's other device-listing helpers (e.g. audio output device names).
pub fn list_port_names() -> Vec<String> {
    let Ok(input) = midir::MidiInput::new(CLIENT_NAME) else { return Vec::new() };
    input.ports().iter().filter_map(|p| input.port_name(p).ok()).collect()
}

fn run(
    enabled: Arc<AtomicBool>,
    device_name: Arc<Mutex<Option<String>>>,
    status: Arc<Mutex<MidiStatus>>,
    events: Arc<Mutex<std::collections::VecDeque<RawMidiEvent>>>,
    stop: Arc<AtomicBool>,
) {
    let mut connection: Option<(String, midir::MidiInputConnection<()>)> = None;

    while !stop.load(Ordering::Relaxed) {
        if !enabled.load(Ordering::Relaxed) {
            if connection.take().is_some() {
                *status.lock().unwrap() = MidiStatus::Disabled;
            }
            thread::sleep(IDLE_TICK);
            continue;
        }

        let wanted = device_name.lock().unwrap().clone();
        let Some(wanted) = wanted else {
            if connection.take().is_some() {
                *status.lock().unwrap() = MidiStatus::Disabled;
            }
            thread::sleep(IDLE_TICK);
            continue;
        };

        // Drop a stale connection: either the user picked a different
        // device, or the previously-connected one vanished (no unplug
        // event exists, so the only way to notice is to re-enumerate and
        // check the name we're connected to is still there).
        let still_present = midir::MidiInput::new(CLIENT_NAME)
            .map(|probe| probe.ports().iter().any(|p| probe.port_name(p).as_deref() == Ok(wanted.as_str())))
            .unwrap_or(false);
        if let Some((name, _)) = &connection {
            if *name != wanted || !still_present {
                connection = None;
            }
        }

        if connection.is_none() {
            if !still_present {
                *status.lock().unwrap() = MidiStatus::Searching;
            } else {
                match connect(&wanted, Arc::clone(&events)) {
                    Ok(conn) => {
                        *status.lock().unwrap() = MidiStatus::Connected(wanted.clone());
                        connection = Some((wanted, conn));
                    }
                    Err(e) => *status.lock().unwrap() = MidiStatus::Error(e),
                }
            }
        }

        thread::sleep(RESCAN_INTERVAL);
    }
}

/// One connection attempt. `midir::MidiInput::connect` consumes the
/// `MidiInput`, so a fresh one is built per attempt rather than kept
/// around.
fn connect(
    wanted_name: &str,
    events: Arc<Mutex<std::collections::VecDeque<RawMidiEvent>>>,
) -> Result<midir::MidiInputConnection<()>, String> {
    let mut input = midir::MidiInput::new(CLIENT_NAME).map_err(|e| e.to_string())?;
    // Sysex/clock/active-sensing carry no binding and would only burn
    // queue slots.
    input.ignore(midir::Ignore::All);
    let port = input
        .ports()
        .into_iter()
        .find(|p| input.port_name(p).as_deref() == Ok(wanted_name))
        .ok_or_else(|| "port vanished before connecting".to_string())?;

    // Runs on the driver's own callback thread: parse and queue only,
    // nothing here may block. A full queue drops the oldest event rather
    // than stalling the MIDI driver.
    let callback = move |_stamp_us: u64, bytes: &[u8], _: &mut ()| {
        let Some(ev) = parse_midi_bytes(bytes) else { return };
        if let Ok(mut q) = events.lock() {
            if q.len() >= QUEUE_CAPACITY {
                q.pop_front();
            }
            q.push_back(ev);
        }
    };
    input.connect(&port, "hpsdr-rs-midi-in", callback, ()).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_note_on_and_off() {
        let on = parse_midi_bytes(&[0x90, 60, 127]).unwrap();
        assert_eq!(on.kind, MidiEventKind::NoteKey);
        assert_eq!(on.channel, 0);
        assert_eq!(on.number, 60);
        assert_eq!(on.value, 127);
        assert!(!on.off);

        let off = parse_midi_bytes(&[0x82, 60, 0]).unwrap();
        assert!(off.off);
        assert_eq!(off.channel, 2);
    }

    /// Controllers that never send a real 0x8n byte rely on this; without
    /// it, a button bound to MOX would key the rig and never unkey it.
    #[test]
    fn note_on_at_zero_velocity_is_a_release() {
        assert!(parse_midi_bytes(&[0x90, 60, 0]).unwrap().off);
    }

    #[test]
    fn parses_control_change() {
        let cc = parse_midi_bytes(&[0xB0, 16, 65]).unwrap();
        assert_eq!(cc.kind, MidiEventKind::ControlChange);
        assert_eq!(cc.number, 16);
        assert_eq!(cc.value, 65);
    }

    #[test]
    fn parses_pitch_bend_to_its_coarse_position() {
        let pb = parse_midi_bytes(&[0xE0, 0x00, 0x40]).unwrap();
        assert_eq!(pb.kind, MidiEventKind::PitchBend);
        assert_eq!(pb.value, 64);
    }

    #[test]
    fn ignores_system_and_aftertouch_traffic() {
        assert!(parse_midi_bytes(&[0xF8]).is_none(), "MIDI clock must not bind");
        assert!(parse_midi_bytes(&[0xF0, 0x7E, 0xF7]).is_none(), "sysex must not bind");
        assert!(parse_midi_bytes(&[0xA0, 60, 40]).is_none(), "poly aftertouch");
        assert!(parse_midi_bytes(&[0xD0, 40]).is_none(), "channel aftertouch");
        assert!(parse_midi_bytes(&[0xC0, 1]).is_none(), "program change");
        assert!(parse_midi_bytes(&[]).is_none());
    }

    #[test]
    fn short_messages_do_not_panic() {
        assert!(parse_midi_bytes(&[0xB0]).is_some());
        assert!(parse_midi_bytes(&[0x90, 60]).is_some());
    }

    #[test]
    fn binding_channel_none_matches_any_channel() {
        let binding = MidiBinding {
            event: MidiEventKind::ControlChange,
            channel: None,
            number: 20,
            kind: MidiBindingKind::Knob,
            action: MidiAction::AfGain,
            momentary: false,
            sensitivity: 1.0,
            debounce_ms: 0,
        };
        let ev = RawMidiEvent { kind: MidiEventKind::ControlChange, channel: 5, number: 20, value: 100, off: false };
        assert!(binding.matches(&ev));
    }

    #[test]
    fn binding_channel_some_rejects_other_channels() {
        let binding = MidiBinding {
            event: MidiEventKind::ControlChange,
            channel: Some(0),
            number: 20,
            kind: MidiBindingKind::Knob,
            action: MidiAction::AfGain,
            momentary: false,
            sensitivity: 1.0,
            debounce_ms: 0,
        };
        let ev = RawMidiEvent { kind: MidiEventKind::ControlChange, channel: 5, number: 20, value: 100, off: false };
        assert!(!binding.matches(&ev));
    }
}
