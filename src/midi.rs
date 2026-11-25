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

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::radio::CwKeyerAtomics;

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

// The actions a control can be bound to: shared with the front-panel
// controllers and hpsdr-remote (crates/hpsdr-controller).
pub use hpsdr_controller::actions::{MidiAction, KEY_ACTIONS, KNOB_ACTIONS, WHEEL_ACTIONS};

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
    /// Wheel bindings only: turn the other way (issue #15) -- for an
    /// encoder that counts the opposite way to what's wanted, without
    /// reconfiguring the controller, which another program may need as
    /// it is.
    #[serde(default)]
    pub reverse: bool,
}

fn default_sensitivity() -> f32 {
    1.0
}

impl MidiBinding {
    /// Whether `other` is bound to the same control as this one.
    fn matches_same_control(&self, other: &MidiBinding) -> bool {
        self.event == other.event && self.channel == other.channel && self.number == other.number
    }

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
    /// A CW key bound on this device (MidiAction::CwKey).
    pub cw_key: Arc<MidiCwKey>,
    /// Send control changes back to the device (issue #15): main.rs's
    /// midi_feedback sends the app's values to Knob bindings' controls,
    /// so a controller (motor faders, LED rings, a keyer's own display)
    /// stays in step with changes made in the app.
    pub feedback: Arc<AtomicBool>,
    output: Arc<Mutex<Option<midir::MidiOutputConnection>>>,
    /// Bumped on each new output connection, so main.rs sends every value
    /// afresh to a device that's just (re)appeared.
    pub output_generation: Arc<AtomicU32>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
    cw_thread: Option<thread::JoinHandle<()>>,
}

impl MidiWorker {
    pub fn start(cw_target: MidiCwTarget) -> Self {
        let enabled = Arc::new(AtomicBool::new(false));
        let device_name = Arc::new(Mutex::new(None));
        let status = Arc::new(Mutex::new(MidiStatus::Disabled));
        let events = Arc::new(Mutex::new(std::collections::VecDeque::new()));
        let cw_key = Arc::new(MidiCwKey::new(cw_target));
        let feedback = Arc::new(AtomicBool::new(false));
        let output = Arc::new(Mutex::new(None));
        let output_generation = Arc::new(AtomicU32::new(0));
        let stop = Arc::new(AtomicBool::new(false));

        let worker = Worker {
            enabled: Arc::clone(&enabled),
            device_name: Arc::clone(&device_name),
            status: Arc::clone(&status),
            events: Arc::clone(&events),
            cw_key: Arc::clone(&cw_key),
            feedback: Arc::clone(&feedback),
            output: Arc::clone(&output),
            output_generation: Arc::clone(&output_generation),
            stop: Arc::clone(&stop),
        };
        let thread = thread::spawn(move || run(worker));
        // Ends a CW key's transmission once the key has been up for the
        // hang time -- see MidiCwKey::tick.
        let cw_thread = {
            let cw_key = Arc::clone(&cw_key);
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let keying = cw_key.tick();
                    thread::sleep(if keying { Duration::from_millis(2) } else { Duration::from_millis(20) });
                }
                cw_key.release("MIDI stopped");
            })
        };

        Self {
            enabled,
            device_name,
            status,
            events,
            cw_key,
            feedback,
            output,
            output_generation,
            stop,
            thread: Some(thread),
            cw_thread: Some(cw_thread),
        }
    }

    /// Whether control changes can be sent back to the device now.
    pub fn output_connected(&self) -> bool {
        self.output.lock().unwrap().is_some()
    }

    /// Sends one message to the device, if an output is connected.
    pub fn send(&self, bytes: &[u8]) {
        if let Some(conn) = self.output.lock().unwrap().as_mut() {
            let _ = conn.send(bytes);
        }
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        if let Some(t) = self.cw_thread.take() {
            let _ = t.join();
        }
    }
}

/// The transmitter state a MIDI CW key works with -- the same that a
/// remote client's live keying uses (remote.rs's TxLink).
pub struct MidiCwTarget {
    pub mox: Arc<AtomicBool>,
    /// The hang time, and live keying (radio::CwLive).
    pub cw: Arc<CwKeyerAtomics>,
    pub tx_frequency_hz: Arc<AtomicU32>,
    pub allow_out_of_band_tx: Arc<AtomicBool>,
    /// RadioSession::cw_mode_active: the mode is CWL/CWU.
    pub cw_mode: Arc<AtomicBool>,
    /// Transmit is armed (a TxHandle exists) -- kept up to date by
    /// main.rs every frame.
    pub tx_available: Arc<AtomicBool>,
    /// Tune, Two Tone or a CW text message is transmitting, so a MOX
    /// that's up isn't one the key can send inside -- kept up to date
    /// by main.rs every frame.
    pub tx_busy: Arc<AtomicBool>,
}

/// Its first key-down is played this long after it arrives, so later
/// transitions -- each played at its own offset from the first -- keep
/// their spacing through the MIDI driver's and the TX thread's jitter.
const CW_KEY_DELAY_MS: u64 = 20;
/// Once the key is up, the radio stays keyed this much longer than the
/// CW hang time, for the release's ramp to play out (remote.rs's
/// KEY_RAMP_MS).
const CW_KEY_RAMP_MS: u64 = 10;

/// A CW key on the MIDI device (MidiAction::CwKey): keys the transmitter
/// live, as a remote client's paddle does (remote.rs's LiveKey) -- key
/// transitions are scheduled on the transmitter's CW sample clock and
/// tx.rs's CwLiveGen plays them as a shaped carrier. Runs on the MIDI
/// driver's callback thread, straight to the transmitter, so keying
/// timing doesn't depend on the UI's frame rate.
///
/// The first key-down of a burst keys the radio -- only in CW, in band,
/// with transmit armed and not Tune, Two Tone or CW text -- and the burst
/// ends, unkeying, once the key has been up for the CW hang time (Settings
/// -> CW, Break-in Delay). It also ends at once if the radio is unkeyed
/// some other way, transmit is disarmed, or the device goes away with the
/// key down.
///
/// If MOX is already up -- a keyer that sends a PTT note ahead of its
/// paddle, bound to MOX -- the key sends inside it: the burst leaves MOX
/// alone and lasts until MOX drops, so the keyer's own PTT and hang time
/// rule. The key's state also drives the PC sidetone (CwLive::sidetone_key).
pub struct MidiCwKey {
    target: MidiCwTarget,
    /// The CwKey bindings (main.rs keeps this up to date).
    bindings: Mutex<Vec<MidiBinding>>,
    burst: Mutex<Option<CwBurst>>,
    /// The last key-down was refused (and said why): the rest of that
    /// press is ignored until the key comes up.
    refused: AtomicBool,
}

struct CwBurst {
    /// The first key-down: when it arrived, and the sample it plays at.
    anchor: Instant,
    anchor_sample: u64,
    rate: u64,
    last_sample: u64,
    down: bool,
    /// The burst raised MOX (else it joined one already up, and leaves
    /// MOX to whoever raised it).
    owns_mox: bool,
}

impl MidiCwKey {
    pub fn new(target: MidiCwTarget) -> Self {
        Self { target, bindings: Mutex::new(Vec::new()), burst: Mutex::new(None), refused: AtomicBool::new(false) }
    }

    /// Kept up to date by main.rs every frame -- see MidiCwTarget::tx_available.
    pub fn set_tx_available(&self, available: bool) {
        self.target.tx_available.store(available, Ordering::Relaxed);
    }

    /// Kept up to date by main.rs every frame -- see MidiCwTarget::tx_busy.
    pub fn set_tx_busy(&self, busy: bool) {
        self.target.tx_busy.store(busy, Ordering::Relaxed);
    }

    /// The bindings whose action is CwKey, from the full list.
    pub fn set_bindings(&self, all: &[MidiBinding]) {
        let keys: Vec<MidiBinding> = all.iter().filter(|b| b.action == MidiAction::CwKey).copied().collect();
        let mut bindings = self.bindings.lock().unwrap();
        if bindings.len() != keys.len()
            || bindings.iter().zip(&keys).any(|(a, b)| !a.matches_same_control(b))
        {
            *bindings = keys;
        }
    }

    /// If `ev` is a CW key binding's, keys or unkeys and returns true (so
    /// it isn't also queued for the frame loop).
    pub fn handle(&self, ev: &RawMidiEvent, now: Instant) -> bool {
        if !self.bindings.lock().unwrap().iter().any(|b| b.matches(ev)) {
            return false;
        }
        self.key(!ev.off, now);
        true
    }

    fn key(&self, down: bool, now: Instant) {
        let live = &self.target.cw.live;
        let mut burst = self.burst.lock().unwrap();
        if let Some(b) = burst.as_mut() {
            if down == b.down {
                return;
            }
            live.sidetone_key.store(down, Ordering::Relaxed);
            let offset = now.saturating_duration_since(b.anchor).as_micros() as u64 * b.rate / 1_000_000;
            let at = (b.anchor_sample + offset).max(b.last_sample).max(live.clock.load(Ordering::Relaxed));
            live.events.lock().unwrap().push_back((at, down));
            b.last_sample = at;
            b.down = down;
            return;
        }
        if !down {
            self.refused.store(false, Ordering::Relaxed);
            return;
        }
        if self.refused.load(Ordering::Relaxed) {
            return;
        }
        let rate = live.rate.load(Ordering::Relaxed) as u64;
        let mox_up = self.target.mox.load(Ordering::Relaxed);
        let why = if !self.target.tx_available.load(Ordering::Relaxed) || rate == 0 {
            Some("transmit isn't armed (Settings -> TX -> Enable Transmit)")
        } else if !self.target.cw_mode.load(Ordering::Relaxed) {
            Some("not in CW (choose CWL or CWU)")
        } else if !crate::tx_frequency_allowed(
            self.target.tx_frequency_hz.load(Ordering::Relaxed),
            self.target.allow_out_of_band_tx.load(Ordering::Relaxed),
        ) {
            Some("TX frequency is outside the ham bands")
        } else if live.active.load(Ordering::Relaxed)
            || (mox_up && self.target.tx_busy.load(Ordering::Relaxed))
        {
            Some("the radio is already transmitting (Tune, Two Tone, CW text or a remote key)")
        } else {
            None
        };
        if let Some(why) = why {
            eprintln!("midi: CW key refused -- {why}");
            self.refused.store(true, Ordering::Relaxed);
            return;
        }
        let at = live.clock.load(Ordering::Relaxed) + CW_KEY_DELAY_MS * rate / 1000;
        {
            let mut events = live.events.lock().unwrap();
            events.clear();
            events.push_back((at, true));
        }
        live.active.store(true, Ordering::Relaxed);
        live.sidetone_key.store(true, Ordering::Relaxed);
        if !mox_up {
            self.target.mox.store(true, Ordering::Relaxed);
        }
        *burst = Some(CwBurst { anchor: now, anchor_sample: at, rate, last_sample: at, down: true, owns_mox: !mox_up });
    }

    /// Ends the burst once the key has been up for the hang time, or at
    /// once if the radio was unkeyed elsewhere or transmit disarmed.
    /// Returns whether a burst is still going.
    pub fn tick(&self) -> bool {
        let live = &self.target.cw.live;
        let mut burst = self.burst.lock().unwrap();
        let Some(b) = burst.as_ref() else { return false };
        let why = if !self.target.mox.load(Ordering::Relaxed) {
            Some("radio unkeyed elsewhere")
        } else if !self.target.tx_available.load(Ordering::Relaxed) {
            Some("transmit disarmed")
        } else {
            None
        };
        let hang_ms = self.target.cw.hang_time_ms.load(Ordering::Relaxed) as u64 + CW_KEY_RAMP_MS;
        // A burst inside someone else's MOX lasts as long as that MOX.
        let hung = b.owns_mox && !b.down && live.clock.load(Ordering::Relaxed) >= b.last_sample + hang_ms * b.rate / 1000;
        if why.is_some() || hung {
            if let Some(why) = why.filter(|_| b.owns_mox) {
                eprintln!("midi: CW key -- {why}, keying ended");
            }
            self.end(b);
            *burst = None;
            return false;
        }
        true
    }

    /// Lets go at once, e.g. the device unplugged with the key down.
    pub fn release(&self, why: &str) {
        let mut burst = self.burst.lock().unwrap();
        if let Some(b) = burst.take() {
            eprintln!("midi: CW key -- {why} while keying, unkeyed");
            self.end(&b);
        }
    }

    fn end(&self, b: &CwBurst) {
        let live = &self.target.cw.live;
        if b.down {
            live.events.lock().unwrap().push_back((live.clock.load(Ordering::Relaxed), false));
        }
        live.active.store(false, Ordering::Relaxed);
        live.sidetone_key.store(false, Ordering::Relaxed);
        if b.owns_mox {
            self.target.mox.store(false, Ordering::Relaxed);
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

/// What the worker thread shares with MidiWorker.
struct Worker {
    enabled: Arc<AtomicBool>,
    device_name: Arc<Mutex<Option<String>>>,
    status: Arc<Mutex<MidiStatus>>,
    events: Arc<Mutex<std::collections::VecDeque<RawMidiEvent>>>,
    cw_key: Arc<MidiCwKey>,
    feedback: Arc<AtomicBool>,
    output: Arc<Mutex<Option<midir::MidiOutputConnection>>>,
    output_generation: Arc<AtomicU32>,
    stop: Arc<AtomicBool>,
}

fn run(w: Worker) {
    let mut connection: Option<(String, midir::MidiInputConnection<()>)> = None;
    // The output port's name, while connected.
    let mut output_name: Option<String> = None;

    while !w.stop.load(Ordering::Relaxed) {
        let wanted = if w.enabled.load(Ordering::Relaxed) { w.device_name.lock().unwrap().clone() } else { None };
        let Some(wanted) = wanted else {
            if connection.take().is_some() {
                *w.status.lock().unwrap() = MidiStatus::Disabled;
                w.cw_key.release("MIDI disabled");
            }
            if output_name.take().is_some() {
                *w.output.lock().unwrap() = None;
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
                w.cw_key.release("MIDI device gone");
            }
        }

        if connection.is_none() {
            if !still_present {
                *w.status.lock().unwrap() = MidiStatus::Searching;
            } else {
                match connect(&wanted, Arc::clone(&w.events), Arc::clone(&w.cw_key)) {
                    Ok(conn) => {
                        *w.status.lock().unwrap() = MidiStatus::Connected(wanted.clone());
                        connection = Some((wanted.clone(), conn));
                    }
                    Err(e) => *w.status.lock().unwrap() = MidiStatus::Error(e),
                }
            }
        }

        // The output to send control changes back on: the device's output
        // port of the same name (how ALSA, CoreMIDI and WinMM all name a
        // USB device's two directions), while feedback is on.
        let want_output = w.feedback.load(Ordering::Relaxed) && connection.is_some();
        let output_present = want_output && output_port_present(&wanted);
        if output_name.is_some() && (!want_output || !output_present || output_name.as_deref() != Some(&wanted)) {
            output_name = None;
            *w.output.lock().unwrap() = None;
        }
        if want_output && output_present && output_name.is_none() {
            if let Ok(conn) = connect_output(&wanted) {
                *w.output.lock().unwrap() = Some(conn);
                w.output_generation.fetch_add(1, Ordering::Relaxed);
                output_name = Some(wanted.clone());
            }
        }

        thread::sleep(RESCAN_INTERVAL);
    }
}

fn output_port_present(name: &str) -> bool {
    midir::MidiOutput::new(CLIENT_NAME)
        .map(|probe| probe.ports().iter().any(|p| probe.port_name(p).as_deref() == Ok(name)))
        .unwrap_or(false)
}

fn connect_output(name: &str) -> Result<midir::MidiOutputConnection, String> {
    let output = midir::MidiOutput::new(CLIENT_NAME).map_err(|e| e.to_string())?;
    let port = output
        .ports()
        .into_iter()
        .find(|p| output.port_name(p).as_deref() == Ok(name))
        .ok_or_else(|| "output port vanished before connecting".to_string())?;
    output.connect(&port, "hpsdr-rs-midi-out").map_err(|e| e.to_string())
}

/// One connection attempt. `midir::MidiInput::connect` consumes the
/// `MidiInput`, so a fresh one is built per attempt rather than kept
/// around.
fn connect(
    wanted_name: &str,
    events: Arc<Mutex<std::collections::VecDeque<RawMidiEvent>>>,
    cw_key: Arc<MidiCwKey>,
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
    // than stalling the MIDI driver. A CW key's events are acted on here
    // instead (MidiCwKey), for timing.
    let callback = move |_stamp_us: u64, bytes: &[u8], _: &mut ()| {
        let Some(ev) = parse_midi_bytes(bytes) else { return };
        if cw_key.handle(&ev, Instant::now()) {
            return;
        }
        if let Ok(mut q) = events.lock() {
            if q.len() >= QUEUE_CAPACITY {
                q.pop_front();
            }
            q.push_back(ev);
        }
    };
    input.connect(&port, "hpsdr-rs-midi-in", callback, ()).map_err(|e| e.to_string())
}

/// The message that tells a controller a Knob binding's control is at
/// `value` (0-127): a Control Change, or a Pitch Bend (coarse 7 bits, as
/// it's read). None for a Note (a Key binding has no position). Sent on
/// the binding's channel, or channel 1 for "any".
pub fn feedback_message(binding: &MidiBinding, value: u8) -> Option<[u8; 3]> {
    let channel = binding.channel.unwrap_or(0) & 0x0F;
    let value = value.min(127);
    match binding.event {
        MidiEventKind::ControlChange => Some([0xB0 | channel, binding.number & 0x7F, value]),
        MidiEventKind::PitchBend => Some([0xE0 | channel, 0, value]),
        MidiEventKind::NoteKey => None,
    }
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
            reverse: false,
        };
        let ev = RawMidiEvent { kind: MidiEventKind::ControlChange, channel: 5, number: 20, value: 100, off: false };
        assert!(binding.matches(&ev));
    }

    #[test]
    fn feedback_messages() {
        let mut b = MidiBinding {
            event: MidiEventKind::ControlChange,
            channel: Some(2),
            number: 7,
            kind: MidiBindingKind::Knob,
            action: MidiAction::AfGain,
            momentary: false,
            sensitivity: 1.0,
            debounce_ms: 0,
            reverse: false,
        };
        assert_eq!(feedback_message(&b, 100), Some([0xB2, 7, 100]));
        b.channel = None;
        assert_eq!(feedback_message(&b, 200), Some([0xB0, 7, 127]));
        b.event = MidiEventKind::PitchBend;
        assert_eq!(feedback_message(&b, 64), Some([0xE0, 0, 64]));
        b.event = MidiEventKind::NoteKey;
        assert_eq!(feedback_message(&b, 64), None);
    }

    fn cw_target() -> (MidiCwTarget, Arc<CwKeyerAtomics>, Arc<AtomicBool>) {
        let cw = Arc::new(CwKeyerAtomics::default());
        cw.live.rate.store(48_000, Ordering::Relaxed);
        cw.hang_time_ms.store(100, Ordering::Relaxed);
        let mox = Arc::new(AtomicBool::new(false));
        let target = MidiCwTarget {
            mox: Arc::clone(&mox),
            cw: Arc::clone(&cw),
            tx_frequency_hz: Arc::new(AtomicU32::new(7_030_000)),
            allow_out_of_band_tx: Arc::new(AtomicBool::new(false)),
            cw_mode: Arc::new(AtomicBool::new(true)),
            tx_available: Arc::new(AtomicBool::new(true)),
            tx_busy: Arc::new(AtomicBool::new(false)),
        };
        (target, cw, mox)
    }

    fn key_binding() -> MidiBinding {
        MidiBinding {
            event: MidiEventKind::NoteKey,
            channel: None,
            number: 60,
            kind: MidiBindingKind::Key,
            action: MidiAction::CwKey,
            momentary: true,
            sensitivity: 1.0,
            debounce_ms: 0,
            reverse: false,
        }
    }

    fn note(down: bool) -> RawMidiEvent {
        RawMidiEvent { kind: MidiEventKind::NoteKey, channel: 0, number: 60, value: if down { 100 } else { 0 }, off: !down }
    }

    #[test]
    fn cw_key_keys_with_its_timing_and_unkeys_after_the_hang_time() {
        let (target, cw, mox) = cw_target();
        let key = MidiCwKey::new(target);
        key.set_bindings(&[key_binding()]);
        // Not a CW key binding's note: left for the frame loop.
        let other = RawMidiEvent { number: 61, ..note(true) };
        assert!(!key.handle(&other, Instant::now()));

        let t0 = Instant::now();
        cw.live.clock.store(1_000, Ordering::Relaxed);
        assert!(key.handle(&note(true), t0));
        assert!(mox.load(Ordering::Relaxed));
        assert!(cw.live.active.load(Ordering::Relaxed));
        // A dit of 60 ms, then a dah starting 60 ms later: same spacing
        // on the transmitter's clock (48 samples per ms), from 20 ms on.
        key.handle(&note(false), t0 + Duration::from_millis(60));
        key.handle(&note(true), t0 + Duration::from_millis(120));
        key.handle(&note(false), t0 + Duration::from_millis(300));
        let start = 1_000 + 20 * 48;
        assert_eq!(
            cw.live.events.lock().unwrap().iter().copied().collect::<Vec<_>>(),
            vec![(start, true), (start + 60 * 48, false), (start + 120 * 48, true), (start + 300 * 48, false)]
        );
        // Key up, but not yet for the hang time (100 ms + 10 ms ramp).
        cw.live.clock.store(start + 300 * 48 + 100 * 48, Ordering::Relaxed);
        assert!(key.tick());
        assert!(mox.load(Ordering::Relaxed));
        cw.live.clock.store(start + 300 * 48 + 110 * 48, Ordering::Relaxed);
        assert!(!key.tick());
        assert!(!mox.load(Ordering::Relaxed));
        assert!(!cw.live.active.load(Ordering::Relaxed));
    }

    #[test]
    fn cw_key_refused_outside_cw_and_released_when_the_device_goes() {
        let (target, cw, mox) = cw_target();
        let cw_mode = Arc::clone(&target.cw_mode);
        let key = MidiCwKey::new(target);
        key.set_bindings(&[key_binding()]);
        cw_mode.store(false, Ordering::Relaxed);
        assert!(key.handle(&note(true), Instant::now()));
        assert!(!mox.load(Ordering::Relaxed));
        // Still refused for the rest of that press, even back in CW.
        cw_mode.store(true, Ordering::Relaxed);
        key.handle(&note(true), Instant::now());
        assert!(!mox.load(Ordering::Relaxed));
        // A fresh press keys; the device going with the key down unkeys.
        key.handle(&note(false), Instant::now());
        key.handle(&note(true), Instant::now());
        assert!(mox.load(Ordering::Relaxed));
        key.release("MIDI device gone");
        assert!(!mox.load(Ordering::Relaxed));
        assert_eq!(cw.live.events.lock().unwrap().back().map(|e| e.1), Some(false));
    }

    #[test]
    fn cw_key_sends_inside_a_mox_already_up_and_leaves_it_alone() {
        let (target, cw, mox) = cw_target();
        let tx_busy = Arc::clone(&target.tx_busy);
        let key = MidiCwKey::new(target);
        key.set_bindings(&[key_binding()]);
        // The keyer's PTT note raised MOX first.
        mox.store(true, Ordering::Relaxed);
        let t0 = Instant::now();
        assert!(key.handle(&note(true), t0));
        assert!(cw.live.active.load(Ordering::Relaxed));
        assert!(cw.live.sidetone_key.load(Ordering::Relaxed));
        key.handle(&note(false), t0 + Duration::from_millis(60));
        assert!(!cw.live.sidetone_key.load(Ordering::Relaxed));
        // Long past the hang time: still keying, inside the keyer's PTT.
        cw.live.clock.store(10 * 48_000, Ordering::Relaxed);
        assert!(key.tick());
        assert!(mox.load(Ordering::Relaxed));
        // The keyer's PTT drops: the burst ends quietly.
        mox.store(false, Ordering::Relaxed);
        assert!(!key.tick());
        assert!(!cw.live.active.load(Ordering::Relaxed));

        // Inside Tune, it's refused, and MOX is left as it was.
        tx_busy.store(true, Ordering::Relaxed);
        mox.store(true, Ordering::Relaxed);
        key.handle(&note(true), Instant::now());
        assert!(!cw.live.active.load(Ordering::Relaxed));
        assert!(mox.load(Ordering::Relaxed));
    }

    /// The whole input path through the OS's MIDI: a virtual port stands
    /// in for a keyer, MidiWorker connects to it by name, a CW key note
    /// keys and a knob's CC reaches the frame loop's queue. Needs an ALSA
    /// sequencer (Linux), so on demand:
    /// `cargo test midi_worker_end_to_end -- --ignored`.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore]
    fn midi_worker_end_to_end() {
        use midir::os::unix::VirtualOutput;
        let (target, cw, mox) = cw_target();
        let worker = MidiWorker::start(target);
        let mut keyer = midir::MidiOutput::new("test-keyer").unwrap().create_virtual("keyer-out").unwrap();
        let name = list_port_names().into_iter().find(|n| n.contains("keyer-out")).expect("virtual port listed");
        worker.cw_key.set_bindings(&[key_binding()]);
        *worker.device_name.lock().unwrap() = Some(name.clone());
        worker.enabled.store(true, Ordering::Relaxed);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !matches!(*worker.status.lock().unwrap(), MidiStatus::Connected(_)) {
            assert!(Instant::now() < deadline, "never connected: {:?}", worker.status.lock().unwrap());
            thread::sleep(Duration::from_millis(20));
        }
        keyer.send(&[0x90, 60, 100]).unwrap(); // CW key down
        keyer.send(&[0xB0, 7, 99]).unwrap(); // a knob
        thread::sleep(Duration::from_millis(100));
        assert!(mox.load(Ordering::Relaxed), "CW key note didn't key");
        assert!(cw.live.active.load(Ordering::Relaxed));
        let queued: Vec<RawMidiEvent> = worker.events.lock().unwrap().iter().copied().collect();
        assert_eq!(queued.len(), 1, "only the knob is left for the frame loop: {queued:?}");
        assert_eq!((queued[0].number, queued[0].value), (7, 99));
        keyer.send(&[0x80, 60, 0]).unwrap(); // key up
        thread::sleep(Duration::from_millis(50));
        assert_eq!(cw.live.events.lock().unwrap().back().map(|e| e.1), Some(false));
        // The device going away with the key down unkeys.
        keyer.send(&[0x90, 60, 100]).unwrap();
        thread::sleep(Duration::from_millis(50));
        drop(keyer);
        let deadline = Instant::now() + Duration::from_secs(4);
        while mox.load(Ordering::Relaxed) || cw.live.active.load(Ordering::Relaxed) {
            // The hang time may end it first (the clock doesn't move
            // here, so it won't) -- otherwise the device check must.
            assert!(Instant::now() < deadline, "still keyed after the device went");
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Feedback reaches a device's input: a virtual port stands in for
    /// the controller. On demand, as midi_worker_end_to_end.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore]
    fn feedback_reaches_the_device() {
        use midir::os::unix::VirtualInput;
        let got = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let sink = Arc::clone(&got);
        let _device = midir::MidiInput::new("test-pod")
            .unwrap()
            .create_virtual("pod-in", move |_, bytes, _| sink.lock().unwrap().push(bytes.to_vec()), ())
            .unwrap();
        let probe = midir::MidiOutput::new("probe").unwrap();
        let name = probe
            .ports()
            .iter()
            .filter_map(|p| probe.port_name(p).ok())
            .find(|n| n.contains("pod-in"))
            .expect("virtual port listed");
        assert!(output_port_present(&name));
        let mut out = connect_output(&name).unwrap();
        let b = MidiBinding {
            event: MidiEventKind::ControlChange,
            channel: Some(0),
            number: 7,
            kind: MidiBindingKind::Knob,
            action: MidiAction::AfGain,
            momentary: false,
            sensitivity: 1.0,
            debounce_ms: 0,
            reverse: false,
        };
        out.send(&feedback_message(&b, 42).unwrap()).unwrap();
        thread::sleep(Duration::from_millis(100));
        assert_eq!(*got.lock().unwrap(), vec![vec![0xB0, 7, 42]]);
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
            reverse: false,
        };
        let ev = RawMidiEvent { kind: MidiEventKind::ControlChange, channel: 5, number: 20, value: 100, off: false };
        assert!(!binding.matches(&ev));
    }
}
