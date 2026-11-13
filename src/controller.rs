//! piHPSDR-style front-panel controllers on a Raspberry Pi: rotary
//! encoders (with push buttons) on GPIO lines, and on the Controller 2 /
//! G2 front panel, 16 push switches on an MCP23017 I2C port expander whose
//! interrupt line is another GPIO line.
//!
//! Ported from piHPSDR's gpio.c and i2c.c (John Melton G0ORX, Christoph van
//! Wüllen DL1YCF, GPL): the same controllers, pin assignments, default
//! actions, encoder state table and MCP23017 set-up. GPIO lines are read
//! through the kernel's GPIO character device (uAPI v2, via gpiocdev, which
//! also does the pull-ups and debouncing), the MCP23017 through
//! /dev/i2c-N's SMBus ioctls.
//!
//! Each knob, encoder button and switch is bound to one of the MIDI actions
//! (midi::MidiAction), and the main window applies it through the same code
//! as a MIDI control (main.rs's dispatch_controller_event). This module only
//! turns the hardware into ControllerEvents.

// The hardware side is Linux-only; elsewhere only the settings are used.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use crate::midi::MidiAction;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Which controller is fitted. Pin assignments and default actions come
/// from piHPSDR (see ControllerConfig::defaults).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ControllerKind {
    #[default]
    None,
    /// 3 encoders + VFO, each small encoder with a push button, and 8
    /// push switches on GPIO lines.
    Controller1,
    /// 4 encoders + VFO, each small one with a push button; 16 switches
    /// on an MCP23017.
    Controller2V1,
    /// 4 dual (top and bottom) encoders with push buttons + VFO; 16
    /// switches on an MCP23017.
    Controller2V2,
    /// The ANAN G2 front panel: as Controller 2 V2, with other default
    /// actions.
    G2V1Panel,
}

impl ControllerKind {
    pub const ALL: [ControllerKind; 5] = [
        ControllerKind::None,
        ControllerKind::Controller1,
        ControllerKind::Controller2V1,
        ControllerKind::Controller2V2,
        ControllerKind::G2V1Panel,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ControllerKind::None => "None",
            ControllerKind::Controller1 => "Controller 1",
            ControllerKind::Controller2V1 => "Controller 2 V1",
            ControllerKind::Controller2V2 => "Controller 2 V2",
            ControllerKind::G2V1Panel => "G2 V1 front panel",
        }
    }

    /// Whether its switches are on an MCP23017 (I2C) rather than GPIO lines.
    pub fn uses_i2c(self) -> bool {
        matches!(self, ControllerKind::Controller2V1 | ControllerKind::Controller2V2 | ControllerKind::G2V1Panel)
    }
}

/// One rotary encoder: two GPIO lines (A, B), the action it drives, and
/// how its steps are counted.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct EncoderLines {
    pub enabled: bool,
    pub pin_a: u32,
    pub pin_b: u32,
    pub action: Option<MidiAction>,
    /// A step on each half cycle (the Controller 2 V2 / G2 panel's small
    /// encoders), rather than one per full cycle of the A/B lines.
    pub half_step: bool,
    /// Multiplies the steps of a tuning (Wheel) action, e.g. lower for a
    /// high-resolution optical VFO encoder: VFO Tune / VFO B Tune move by
    /// the VFO step (VFO-A's box), RIT/XIT 10 Hz a step.
    #[serde(default = "one")]
    pub sensitivity: f32,
}

fn one() -> f32 {
    1.0
}

/// A push button or switch: a GPIO line (or, for MCP23017 switches, a
/// position on the expander) and its action.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SwitchLine {
    pub enabled: bool,
    pub pin: u32,
    pub action: Option<MidiAction>,
}

/// An encoder position on the controller: the bottom (or only) encoder,
/// the top one of a dual encoder, and its push button.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct EncoderConfig {
    pub bottom: EncoderLines,
    pub top: EncoderLines,
    pub button: SwitchLine,
}

pub const MAX_ENCODERS: usize = 5;
pub const MAX_SWITCHES: usize = 16;

/// Kept per computer (the controller is wired to it), in controller.json
/// in the settings directory.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ControllerConfig {
    pub kind: ControllerKind,
    pub encoders: Vec<EncoderConfig>,
    /// Controller 1: GPIO switches. The others: the MCP23017's 16 inputs,
    /// in piHPSDR's order (SW2..SW17; `pin` unused).
    pub switches: Vec<SwitchLine>,
    pub i2c_device: String,
    pub i2c_address: u8,
    /// The GPIO line the MCP23017's interrupt output is wired to.
    pub i2c_irq_pin: u32,
}

impl Default for ControllerConfig {
    fn default() -> Self {
        ControllerConfig::defaults(ControllerKind::None)
    }
}

fn enc(enabled: bool, a: u32, b: u32, action: Option<MidiAction>, half_step: bool) -> EncoderLines {
    EncoderLines { enabled, pin_a: a, pin_b: b, action, half_step, sensitivity: 1.0 }
}

fn sw(enabled: bool, pin: u32, action: Option<MidiAction>) -> SwitchLine {
    SwitchLine { enabled, pin, action }
}

const NO_ENC: EncoderLines =
    EncoderLines { enabled: false, pin_a: 0, pin_b: 0, action: None, half_step: false, sensitivity: 1.0 };
const NO_SW: SwitchLine = SwitchLine { enabled: false, pin: 0, action: None };

impl ControllerConfig {
    /// piHPSDR's pin assignments and default actions for `kind` (gpio.c's
    /// encoders_* and switches_* tables). Its actions that have no
    /// equivalent here (second receiver, diversity gain/phase, PureSignal,
    /// Two Tone, Lock, menus) are mapped to the nearest one or left
    /// unassigned; Controller 1's eight switches, hard-wired to piHPSDR's
    /// toolbar there, get actions of their own.
    pub fn defaults(kind: ControllerKind) -> ControllerConfig {
        use MidiAction::*;
        let (encoders, switches): (Vec<EncoderConfig>, Vec<SwitchLine>) = match kind {
            ControllerKind::None => (
                vec![EncoderConfig { bottom: NO_ENC, top: NO_ENC, button: NO_SW }; MAX_ENCODERS],
                vec![NO_SW; MAX_SWITCHES],
            ),
            ControllerKind::Controller1 => (
                vec![
                    EncoderConfig { bottom: enc(true, 20, 26, Some(AfGain), false), top: NO_ENC, button: sw(true, 25, Some(BandUp)) },
                    EncoderConfig { bottom: enc(true, 16, 19, Some(AgcGain), false), top: NO_ENC, button: sw(true, 8, Some(BandStack)) },
                    EncoderConfig { bottom: enc(true, 4, 21, Some(TxDrive), false), top: NO_ENC, button: sw(true, 7, Some(ModeUp)) },
                    EncoderConfig { bottom: enc(true, 18, 17, Some(VfoTune), false), top: NO_ENC, button: NO_SW },
                    EncoderConfig { bottom: NO_ENC, top: NO_ENC, button: NO_SW },
                ],
                {
                    let mut s = vec![
                        sw(true, 27, Some(Mox)),
                        sw(true, 13, Some(Tune)),
                        sw(true, 12, Some(BandDown)),
                        sw(true, 6, Some(BandUp)),
                        sw(true, 5, Some(ModeDown)),
                        sw(true, 24, Some(ModeUp)),
                        sw(true, 23, Some(NoiseReductionCycle)),
                        sw(true, 22, Some(NoiseBlankerCycle)),
                    ];
                    s.resize(MAX_SWITCHES, NO_SW);
                    s
                },
            ),
            ControllerKind::Controller2V1 => (
                vec![
                    EncoderConfig { bottom: enc(true, 20, 26, Some(AfGain), false), top: NO_ENC, button: sw(true, 22, Some(BandUp)) },
                    EncoderConfig { bottom: enc(true, 4, 21, Some(AgcGain), false), top: NO_ENC, button: sw(true, 27, Some(BandStack)) },
                    EncoderConfig { bottom: enc(true, 16, 19, Some(FilterWidth), false), top: NO_ENC, button: sw(true, 23, Some(ModeUp)) },
                    EncoderConfig { bottom: enc(true, 25, 8, Some(RitAdjust), false), top: NO_ENC, button: sw(true, 24, Some(RitClear)) },
                    EncoderConfig { bottom: enc(true, 18, 17, Some(VfoTune), false), top: NO_ENC, button: NO_SW },
                ],
                i2c_switches(&[
                    Some(Mox),
                    Some(Tune),
                    None, // PS
                    None, // Two Tone
                    Some(NoiseReductionCycle),
                    Some(VfoAtoB),
                    Some(VfoBtoA),
                    Some(ModeDown),
                    Some(BandDown),
                    Some(ModeUp),
                    Some(BandUp),
                    Some(XitToggle),
                    Some(NoiseBlankerCycle),
                    Some(SnbToggle),
                    None, // Lock
                    Some(CtunToggle),
                ]),
            ),
            ControllerKind::Controller2V2 => (
                vec![
                    // ENC2: AGC/AF gain (piHPSDR: RX1's), button RX1.
                    EncoderConfig {
                        bottom: enc(true, 5, 6, Some(AgcGain), true),
                        top: enc(true, 26, 20, Some(AfGain), true),
                        button: sw(true, 22, Some(BandStack)),
                    },
                    // ENC3: piHPSDR's RX2 gains, RX2.
                    EncoderConfig {
                        bottom: enc(true, 9, 7, Some(FilterWidth), true),
                        top: enc(true, 21, 4, Some(TxDrive), true),
                        button: sw(true, 27, Some(ModeUp)),
                    },
                    // ENC4: piHPSDR's diversity gain/phase, DIV.
                    EncoderConfig {
                        bottom: enc(true, 11, 10, Some(CwSpeed), true),
                        top: enc(true, 19, 16, Some(MicGain), true),
                        button: sw(true, 23, Some(DiversityToggle)),
                    },
                    // ENC5: XIT/RIT, piHPSDR's frequency menu.
                    EncoderConfig {
                        bottom: enc(true, 13, 12, Some(XitAdjust), true),
                        top: enc(true, 8, 25, Some(RitAdjust), true),
                        button: sw(true, 24, Some(RitClear)),
                    },
                    EncoderConfig { bottom: enc(true, 18, 17, Some(VfoTune), false), top: NO_ENC, button: NO_SW },
                ],
                i2c_switches(&[
                    Some(Mox),                 // GPB7 SW2
                    Some(Tune),                // GPB6 SW3
                    None,                      // GPB5 SW4 PS
                    None,                      // GPB4 SW5 Two Tone
                    Some(NoiseReductionCycle), // GPA3 SW6
                    Some(NoiseBlankerCycle),   // GPB3 SW14
                    Some(SnbToggle),           // GPB2 SW15
                    Some(XitToggle),           // GPA7 SW13
                    Some(BandUp),              // GPA6 SW12
                    Some(ModeUp),              // GPA5 SW11
                    Some(BandDown),            // GPA4 SW10
                    Some(ModeDown),            // GPA0 SW9
                    Some(VfoAtoB),             // GPA2 SW7
                    Some(VfoBtoA),             // GPA1 SW8
                    None,                      // GPB1 SW16 Lock
                    Some(CtunToggle),          // GPB0 SW17
                ]),
            ),
            ControllerKind::G2V1Panel => (
                vec![
                    EncoderConfig {
                        bottom: enc(true, 5, 6, Some(TxDrive), true),
                        top: enc(true, 26, 20, Some(MicGain), true),
                        button: NO_SW_AT(22),
                    },
                    EncoderConfig {
                        bottom: enc(true, 9, 7, Some(AgcGain), true),
                        top: enc(true, 21, 4, Some(AfGain), true),
                        button: NO_SW_AT(27),
                    },
                    EncoderConfig {
                        bottom: enc(true, 11, 10, Some(FilterWidth), true),
                        top: enc(true, 19, 16, Some(CwSpeed), true),
                        button: sw(true, 23, Some(DiversityToggle)),
                    },
                    EncoderConfig {
                        bottom: enc(true, 13, 12, Some(XitAdjust), true),
                        top: enc(true, 8, 25, Some(RitAdjust), true),
                        button: sw(true, 24, Some(RitClear)),
                    },
                    EncoderConfig { bottom: enc(true, 18, 17, Some(VfoTune), false), top: NO_ENC, button: NO_SW },
                ],
                i2c_switches(&[
                    Some(XitToggle),       // GPB7 SW22
                    Some(RitToggle),       // GPB6 SW21
                    None,                  // GPB5 SW20 Function
                    Some(Split),           // GPB4 SW19
                    None,                  // GPA3 SW9 Lock
                    Some(VfoBtoA),         // GPB3 SW18
                    Some(VfoAtoB),         // GPB2 SW17
                    Some(ModeDown),        // GPA7 SW13
                    Some(BandUp),          // GPA6 SW12
                    Some(FilterWidthUp),   // GPA5 SW11
                    Some(ModeUp),          // GPA4 SW10
                    Some(Mox),             // GPA0 SW6
                    Some(CtunToggle),      // GPA2 SW8
                    Some(Tune),            // GPA1 SW7
                    Some(BandDown),        // GPB1 SW16
                    Some(FilterWidthDown), // GPB0 SW15
                ]),
            ),
        };
        ControllerConfig {
            kind,
            encoders,
            switches,
            i2c_device: "/dev/i2c-1".to_string(),
            i2c_address: 0x20,
            i2c_irq_pin: 15,
        }
    }

    /// Reads controller.json; the default (no controller) if there's none.
    pub fn load() -> ControllerConfig {
        let mut cfg = config_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str::<ControllerConfig>(&s).ok())
            .unwrap_or_default();
        cfg.encoders.resize(MAX_ENCODERS, EncoderConfig { bottom: NO_ENC, top: NO_ENC, button: NO_SW });
        cfg.switches.resize(MAX_SWITCHES, NO_SW);
        cfg
    }

    pub fn save(&self) {
        let Some(path) = config_path() else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            if let Err(e) = std::fs::write(&path, json) {
                eprintln!("controller: couldn't save {}: {e}", path.display());
            }
        }
    }

    /// The parts that need the hardware set up again when they change:
    /// everything but the actions and sensitivities.
    pub fn hardware(&self) -> impl PartialEq {
        let enc = |e: &EncoderLines| (e.enabled, e.pin_a, e.pin_b, e.half_step);
        (
            self.kind,
            self.encoders.iter().map(|e| (enc(&e.bottom), enc(&e.top), e.button.enabled, e.button.pin)).collect::<Vec<_>>(),
            self.switches.iter().map(|s| (s.enabled, s.pin)).collect::<Vec<_>>(),
            self.i2c_device.clone(),
            self.i2c_address,
            self.i2c_irq_pin,
        )
    }
}

/// A disabled button that still says which line piHPSDR wires it to.
#[allow(non_snake_case)]
fn NO_SW_AT(pin: u32) -> SwitchLine {
    sw(true, pin, None)
}

fn i2c_switches(actions: &[Option<MidiAction>; MAX_SWITCHES]) -> Vec<SwitchLine> {
    actions.iter().map(|&a| sw(false, 0, a)).collect()
}

fn config_path() -> Option<PathBuf> {
    let mut path = crate::config::settings_dir()?;
    path.push("controller.json");
    Some(path)
}

/// The MCP23017 input (bit in its 16-bit A/B register pair: GPA0-7 low
/// byte, GPB0-7 high byte) of each of the 16 switches, in piHPSDR's order
/// (i2c.c's i2c_sw[]).
pub const I2C_SWITCH_BITS: [u16; MAX_SWITCHES] = [
    0x8000, 0x4000, 0x2000, 0x1000, 0x0008, 0x0800, 0x0400, 0x0080, 0x0040, 0x0020, 0x0010, 0x0001, 0x0004,
    0x0002, 0x0200, 0x0100,
];

/// The board's name for each MCP23017 input, e.g. "GPB7".
pub fn i2c_switch_pin_name(bit: u16) -> String {
    let n = bit.trailing_zeros();
    if n < 8 {
        format!("GPA{n}")
    } else {
        format!("GPB{}", n - 8)
    }
}

/// Which control produced an event; the main window looks up its action
/// in the current ControllerConfig, so changing an action needs no restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ControlId {
    EncoderBottom(usize),
    EncoderTop(usize),
    EncoderButton(usize),
    Switch(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControllerEvent {
    /// Encoder steps since the last report (100 ms), accelerated as
    /// piHPSDR does for a fast turn; positive clockwise.
    Turned(ControlId, i32),
    Pressed(ControlId),
    Released(ControlId),
}

impl ControllerConfig {
    /// The action bound to the control `id`, if any.
    pub fn action(&self, id: ControlId) -> Option<MidiAction> {
        match id {
            ControlId::EncoderBottom(i) => self.encoders.get(i)?.bottom.action,
            ControlId::EncoderTop(i) => self.encoders.get(i)?.top.action,
            ControlId::EncoderButton(i) => self.encoders.get(i)?.button.action,
            ControlId::Switch(i) => self.switches.get(i)?.action,
        }
    }

    pub fn sensitivity(&self, id: ControlId) -> f32 {
        match id {
            ControlId::EncoderBottom(i) => self.encoders.get(i).map_or(1.0, |e| e.bottom.sensitivity),
            ControlId::EncoderTop(i) => self.encoders.get(i).map_or(1.0, |e| e.top.sensitivity),
            _ => 1.0,
        }
    }
}

// ---------------------------------------------------------------------------
// Encoder decoding -- piHPSDR's state table, unchanged.

const DIR_CW: u8 = 0x10;
const DIR_CCW: u8 = 0x20;

const R_START: u8 = 0x00;
const R_CW_FINAL: u8 = 0x01;
const R_CW_BEGIN: u8 = 0x02;
const R_CW_NEXT: u8 = 0x03;
const R_CCW_BEGIN: u8 = 0x04;
const R_CCW_FINAL: u8 = 0x05;
const R_CCW_NEXT: u8 = 0x06;
const R_START1: u8 = 0x07;
const R_START0: u8 = 0x08;
const R_CW_BEG1: u8 = 0x09;
const R_CW_BEG0: u8 = 0x0A;
const R_CCW_BEG1: u8 = 0x0B;
const R_CCW_BEG0: u8 = 0x0C;

/// [state][AB levels: 00, 10 (A only), 01 (B only), 11] -> next state, with
/// DIR_CW/DIR_CCW set on a completed step. "1" is a line pulled low (the
/// contact closed). See piHPSDR's gpio.c for the cycles this follows.
const ENCODER_STATE_TABLE: [[u8; 4]; 13] = [
    // Full cycle
    [R_START, R_CW_BEGIN, R_CCW_BEGIN, R_START],
    [R_CW_NEXT, R_START, R_CW_FINAL, R_START | DIR_CW],
    [R_CW_NEXT, R_CW_BEGIN, R_START, R_START],
    [R_CW_NEXT, R_CW_BEGIN, R_CW_FINAL, R_START],
    [R_CCW_NEXT, R_START, R_CCW_BEGIN, R_START],
    [R_CCW_NEXT, R_CCW_FINAL, R_START, R_START | DIR_CCW],
    [R_CCW_NEXT, R_CCW_FINAL, R_CCW_BEGIN, R_START],
    // Half cycles
    [R_START0, R_CW_BEG1, R_CCW_BEG1, R_START1],
    [R_START0, R_CCW_BEG0, R_CW_BEG0, R_START1],
    [R_START0 | DIR_CW, R_CW_BEG1, R_CW_BEG0, R_START1],
    [R_START0, R_CW_BEG1, R_CW_BEG0, R_START1 | DIR_CW],
    [R_START0 | DIR_CCW, R_CCW_BEG0, R_CCW_BEG1, R_START1],
    [R_START0, R_CCW_BEG0, R_CCW_BEG1, R_START1 | DIR_CCW],
];

/// One encoder's decoding state.
#[derive(Debug, Clone, Copy)]
pub struct EncoderState {
    a: u8,
    b: u8,
    state: u8,
    /// Steps not yet reported.
    pub pos: i32,
}

impl EncoderState {
    pub fn new(half_step: bool) -> EncoderState {
        EncoderState { a: 1, b: 1, state: if half_step { R_START1 } else { R_START }, pos: 0 }
    }

    fn step(&mut self) {
        let pins = ((self.b << 1) | self.a) as usize;
        self.state = ENCODER_STATE_TABLE[(self.state & 0x0F) as usize][pins];
        match self.state & 0x30 {
            DIR_CW => self.pos += 1,
            DIR_CCW => self.pos -= 1,
            _ => {}
        }
    }

    /// Line A changed: `level` 1 = pulled low (active).
    pub fn set_a(&mut self, level: u8) {
        self.a = level;
        self.step();
    }

    pub fn set_b(&mut self, level: u8) {
        self.b = level;
        self.step();
    }
}

/// piHPSDR's acceleration of a 100 ms step count: unchanged up to 20, then
/// growing about quadratically (50 -> 75, 100 -> 200, 400 -> 1800), so a
/// fast spin of the VFO knob covers ground.
pub fn accelerate(steps: i32) -> i32 {
    if steps > 20 {
        (steps * steps + 138 * steps - 776) / 117
    } else if steps < -20 {
        -(steps * steps - 138 * steps - 776) / 117
    } else {
        steps
    }
}

// ---------------------------------------------------------------------------
// Running the hardware (Linux).

#[cfg(target_os = "linux")]
pub use linux::Controller;

#[cfg(not(target_os = "linux"))]
pub struct Controller;

#[cfg(not(target_os = "linux"))]
impl Controller {
    pub fn start(_cfg: &ControllerConfig) -> Result<Controller, String> {
        Err("GPIO controllers are only supported on Linux (e.g. a Raspberry Pi)".to_string())
    }
    pub fn poll(&self) -> Vec<ControllerEvent> {
        Vec::new()
    }
    pub fn status(&self) -> String {
        String::new()
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use gpiocdev::line::{Bias, EdgeDetection, EdgeKind, Value};
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::{channel, Receiver, Sender};
    use std::sync::{Arc, Mutex};
    use std::thread::JoinHandle;
    use std::time::Duration;

    /// What a GPIO line does.
    #[derive(Debug, Clone, Copy)]
    enum LineRole {
        EncoderA(ControlId),
        EncoderB(ControlId),
        Button(ControlId),
        I2cIrq,
    }

    /// The running controller: a thread reading GPIO edges and one
    /// reporting encoder steps (and polling the MCP23017) every 100 ms.
    /// Dropping it stops both and releases the lines.
    pub struct Controller {
        stop: Arc<AtomicBool>,
        events: Receiver<ControllerEvent>,
        status: String,
        threads: Vec<JoinHandle<()>>,
    }

    impl Drop for Controller {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            for t in self.threads.drain(..) {
                let _ = t.join();
            }
        }
    }

    impl Controller {
        pub fn start(cfg: &ControllerConfig) -> Result<Controller, String> {
            let chip = find_gpio_chip()?;
            let mut roles: HashMap<u32, LineRole> = HashMap::new();
            let mut debounce: HashMap<u32, Duration> = HashMap::new();
            let mut encoders: HashMap<ControlId, EncoderState> = HashMap::new();
            let mut warnings = Vec::new();
            let mut add = |pin: u32, role: LineRole, bounce: Duration, warnings: &mut Vec<String>| {
                if roles.contains_key(&pin) {
                    warnings.push(format!("GPIO {pin} used twice"));
                } else {
                    roles.insert(pin, role);
                    debounce.insert(pin, bounce);
                }
            };
            for (i, e) in cfg.encoders.iter().enumerate() {
                for (lines, id) in [(&e.bottom, ControlId::EncoderBottom(i)), (&e.top, ControlId::EncoderTop(i))] {
                    if !lines.enabled {
                        continue;
                    }
                    // The optical VFO encoder mustn't be debounced; 2 ms
                    // for the mechanical ones (piHPSDR).
                    let bounce = if lines.action == Some(MidiAction::VfoTune) {
                        Duration::ZERO
                    } else {
                        Duration::from_millis(2)
                    };
                    add(lines.pin_a, LineRole::EncoderA(id), bounce, &mut warnings);
                    add(lines.pin_b, LineRole::EncoderB(id), bounce, &mut warnings);
                    encoders.insert(id, EncoderState::new(lines.half_step));
                }
                if e.button.enabled {
                    add(e.button.pin, LineRole::Button(ControlId::EncoderButton(i)), Duration::from_millis(25), &mut warnings);
                }
            }
            if !cfg.kind.uses_i2c() {
                for (i, s) in cfg.switches.iter().enumerate() {
                    if s.enabled {
                        add(s.pin, LineRole::Button(ControlId::Switch(i)), Duration::from_millis(25), &mut warnings);
                    }
                }
            }
            let i2c = if cfg.kind.uses_i2c() {
                let mcp = Mcp23017::open(&cfg.i2c_device, cfg.i2c_address)?;
                add(cfg.i2c_irq_pin, LineRole::I2cIrq, Duration::from_millis(1), &mut warnings);
                Some(Arc::new(Mutex::new(mcp)))
            } else {
                None
            };
            if roles.is_empty() {
                return Err("No lines to read: no encoders or switches are enabled".to_string());
            }

            let mut builder = gpiocdev::Request::builder();
            builder.on_chip(&chip).with_consumer("hpsdr-rs");
            for (&pin, &bounce) in &debounce {
                builder
                    .with_line(pin)
                    .as_input()
                    .with_bias(Bias::PullUp)
                    .with_edge_detection(EdgeDetection::BothEdges)
                    .with_debounce_period(bounce);
            }
            let request = builder.request().map_err(|e| {
                format!(
                    "Couldn't open the GPIO lines on {}: {e} (in use by another program, or not in the \
                     'gpio' group?)",
                    chip.display()
                )
            })?;
            // Where each encoder line is now: low (contact closed) is 1.
            for (&pin, role) in &roles {
                let level = match request.value(pin) {
                    Ok(Value::Inactive) => 1,
                    _ => 0,
                };
                match role {
                    LineRole::EncoderA(id) => encoders.get_mut(id).map(|e| e.a = level),
                    LineRole::EncoderB(id) => encoders.get_mut(id).map(|e| e.b = level),
                    _ => None,
                };
            }

            let stop = Arc::new(AtomicBool::new(false));
            let (tx, rx) = channel();
            let encoders = Arc::new(Mutex::new(encoders));
            let mut threads = Vec::new();

            {
                let (stop, tx, encoders, i2c) = (stop.clone(), tx.clone(), encoders.clone(), i2c.clone());
                threads.push(
                    std::thread::Builder::new()
                        .name("controller-gpio".into())
                        .spawn(move || edge_thread(request, roles, encoders, i2c, tx, stop))
                        .map_err(|e| e.to_string())?,
                );
            }
            {
                let (stop, encoders) = (stop.clone(), encoders.clone());
                threads.push(
                    std::thread::Builder::new()
                        .name("controller-tick".into())
                        .spawn(move || tick_thread(encoders, i2c, tx, stop))
                        .map_err(|e| e.to_string())?,
                );
            }

            let mut status = format!(
                "Running: {} lines on {}{}",
                debounce.len(),
                chip.display(),
                if cfg.kind.uses_i2c() {
                    format!(", MCP23017 at 0x{:02X} on {}", cfg.i2c_address, cfg.i2c_device)
                } else {
                    String::new()
                }
            );
            if !warnings.is_empty() {
                status.push_str(&format!(" -- {}", warnings.join(", ")));
            }
            eprintln!("controller: {status}");
            Ok(Controller { stop, events: rx, status, threads })
        }

        /// Events since the last call.
        pub fn poll(&self) -> Vec<ControllerEvent> {
            self.events.try_iter().collect()
        }

        pub fn status(&self) -> String {
            self.status.clone()
        }
    }

    /// The Pi's 40-pin header GPIO chip: labelled pinctrl-rp1 on a Pi 5,
    /// pinctrl-bcm2711/2835 on a Pi 4 or earlier (gpiochip4 or gpiochip0,
    /// depending on the kernel). Otherwise the first chip with 28+ lines.
    fn find_gpio_chip() -> Result<PathBuf, String> {
        let chips = gpiocdev::chip::chips().map_err(|e| format!("No GPIO chips: {e}"))?;
        let mut fallback = None;
        for path in chips {
            let Ok(info) = gpiocdev::chip::Chip::from_path(&path).and_then(|c| c.info()) else { continue };
            if info.label.starts_with("pinctrl-rp1") || info.label.starts_with("pinctrl-bcm") {
                return Ok(path);
            }
            if fallback.is_none() && info.num_lines >= 28 {
                fallback = Some(path);
            }
        }
        fallback.ok_or_else(|| "No Raspberry Pi GPIO chip found".to_string())
    }

    fn edge_thread(
        request: gpiocdev::Request,
        roles: HashMap<u32, LineRole>,
        encoders: Arc<Mutex<HashMap<ControlId, EncoderState>>>,
        i2c: Option<Arc<Mutex<Mcp23017>>>,
        tx: Sender<ControllerEvent>,
        stop: Arc<AtomicBool>,
    ) {
        while !stop.load(Ordering::Relaxed) {
            match request.wait_edge_event(Duration::from_millis(100)) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(e) => {
                    eprintln!("controller: GPIO wait failed: {e}");
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                }
            }
            let Ok(ev) = request.read_edge_event() else { continue };
            // Pulled-up lines: falling = contact closed = 1.
            let level = if ev.kind == EdgeKind::Falling { 1 } else { 0 };
            match roles.get(&ev.offset) {
                Some(LineRole::EncoderA(id)) => {
                    if let Some(e) = encoders.lock().unwrap().get_mut(id) {
                        e.set_a(level);
                    }
                }
                Some(LineRole::EncoderB(id)) => {
                    if let Some(e) = encoders.lock().unwrap().get_mut(id) {
                        e.set_b(level);
                    }
                }
                Some(LineRole::Button(id)) => {
                    let _ = tx.send(if level == 1 { ControllerEvent::Pressed(*id) } else { ControllerEvent::Released(*id) });
                }
                Some(LineRole::I2cIrq) => {
                    if level == 1 {
                        if let Some(mcp) = &i2c {
                            mcp.lock().unwrap().service(&tx);
                        }
                    }
                }
                None => {}
            }
        }
    }

    /// Every 100 ms: report the encoders' steps (accelerated), and service
    /// the MCP23017 in case an interrupt was missed -- a pending one leaves
    /// its line low, so no further edge would come (piHPSDR).
    fn tick_thread(
        encoders: Arc<Mutex<HashMap<ControlId, EncoderState>>>,
        i2c: Option<Arc<Mutex<Mcp23017>>>,
        tx: Sender<ControllerEvent>,
        stop: Arc<AtomicBool>,
    ) {
        while !stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(100));
            let turned: Vec<(ControlId, i32)> = encoders
                .lock()
                .unwrap()
                .iter_mut()
                .filter(|(_, e)| e.pos != 0)
                .map(|(id, e)| (*id, std::mem::take(&mut e.pos)))
                .collect();
            for (id, steps) in turned {
                let _ = tx.send(ControllerEvent::Turned(id, accelerate(steps)));
            }
            if let Some(mcp) = &i2c {
                mcp.lock().unwrap().service(&tx);
            }
        }
    }

    /// The MCP23017 16-bit I/O expander, as piHPSDR sets it up: all 16
    /// pins inputs with pull-ups, inverted (a closed switch reads 1), an
    /// interrupt on any change, INTA/INTB mirrored and open-drain.
    struct Mcp23017 {
        fd: std::os::fd::OwnedFd,
        last_error: Option<String>,
    }

    const I2C_SLAVE: libc::c_ulong = 0x0703;
    const I2C_SMBUS: libc::c_ulong = 0x0720;
    const I2C_SMBUS_READ: u8 = 1;
    const I2C_SMBUS_WRITE: u8 = 0;
    const I2C_SMBUS_BYTE_DATA: u32 = 2;
    const I2C_SMBUS_WORD_DATA: u32 = 3;

    #[repr(C)]
    union I2cSmbusData {
        byte: u8,
        word: u16,
        block: [u8; 34],
    }

    #[repr(C)]
    struct I2cSmbusIoctlData {
        read_write: u8,
        command: u8,
        size: u32,
        data: *mut I2cSmbusData,
    }

    impl Mcp23017 {
        fn open(device: &str, address: u8) -> Result<Mcp23017, String> {
            use std::os::fd::{AsRawFd, FromRawFd};
            let path = std::ffi::CString::new(device).map_err(|e| e.to_string())?;
            // SAFETY: a plain open(2) of a NUL-terminated path.
            let raw = unsafe { libc::open(path.as_ptr(), libc::O_RDWR) };
            if raw < 0 {
                return Err(format!(
                    "Couldn't open {device}: {} (I2C enabled in raspi-config? in the 'i2c' group?)",
                    std::io::Error::last_os_error()
                ));
            }
            // SAFETY: `raw` is a file descriptor we just opened and own.
            let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(raw) };
            // SAFETY: I2C_SLAVE takes the 7-bit address as its argument.
            if unsafe { libc::ioctl(fd.as_raw_fd(), I2C_SLAVE as _, address as libc::c_ulong) } < 0 {
                return Err(format!("I2C address 0x{address:02X}: {}", std::io::Error::last_os_error()));
            }
            let mcp = Mcp23017 { fd, last_error: None };
            // piHPSDR's i2c_init(), register by register (IOCON.BANK = 0).
            for (reg, value) in [
                (0x0A, 0x44), // IOCON: MIRROR (INTA = INTB), ODR (open-drain INT)
                (0x0B, 0x44),
                (0x04, 0x00), // GPINTEN off while setting up
                (0x05, 0x00),
                (0x06, 0x00), // DEFVAL
                (0x07, 0x00),
                (0x14, 0x00), // OLAT
                (0x15, 0x00),
                (0x0C, 0xFF), // GPPU: pull-ups
                (0x0D, 0xFF),
                (0x02, 0xFF), // IPOL: inverted, a closed switch reads 1
                (0x03, 0xFF),
                (0x00, 0xFF), // IODIR: inputs
                (0x01, 0xFF),
                (0x08, 0x00), // INTCON: interrupt on any change
                (0x09, 0x00),
                (0x04, 0xFF), // GPINTEN: interrupts on
                (0x05, 0xFF),
            ] {
                mcp.write_byte(reg, value).map_err(|e| {
                    format!("No MCP23017 at 0x{address:02X} on {device} ({e}) -- is the controller connected?")
                })?;
            }
            // Clear anything pending.
            for _ in 0..10 {
                match mcp.read_word(0x0E) {
                    Ok(0) => break,
                    Ok(_) => {
                        let _ = mcp.read_word(0x10);
                    }
                    Err(_) => break,
                }
            }
            Ok(mcp)
        }

        fn smbus(&self, read_write: u8, command: u8, size: u32, data: &mut I2cSmbusData) -> std::io::Result<()> {
            use std::os::fd::AsRawFd;
            let mut args = I2cSmbusIoctlData { read_write, command, size, data };
            // SAFETY: I2C_SMBUS with a valid i2c_smbus_ioctl_data pointing
            // at a live i2c_smbus_data union.
            if unsafe { libc::ioctl(self.fd.as_raw_fd(), I2C_SMBUS as _, &mut args) } < 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        }

        fn write_byte(&self, reg: u8, value: u8) -> std::io::Result<()> {
            let mut data = I2cSmbusData { byte: value };
            self.smbus(I2C_SMBUS_WRITE, reg, I2C_SMBUS_BYTE_DATA, &mut data)
        }

        fn read_word(&self, reg: u8) -> std::io::Result<u16> {
            let mut data = I2cSmbusData { word: 0 };
            self.smbus(I2C_SMBUS_READ, reg, I2C_SMBUS_WORD_DATA, &mut data)?;
            // SAFETY: a WORD_DATA read fills `word`.
            Ok(unsafe { data.word })
        }

        /// Reports every switch that changed (INTF, register 0x0E/0x0F)
        /// with its level at the interrupt (INTCAP, 0x10/0x11, which also
        /// clears it), until nothing is pending -- i2c_interrupt().
        fn service(&mut self, tx: &Sender<ControllerEvent>) {
            for _ in 0..16 {
                let flags = match self.read_word(0x0E) {
                    Ok(f) => f,
                    Err(e) => {
                        let msg = e.to_string();
                        if self.last_error.as_deref() != Some(msg.as_str()) {
                            eprintln!("controller: MCP23017 read failed: {msg}");
                            self.last_error = Some(msg);
                        }
                        return;
                    }
                };
                self.last_error = None;
                if flags == 0 {
                    return;
                }
                let ints = self.read_word(0x10).unwrap_or(0);
                for (i, &bit) in I2C_SWITCH_BITS.iter().enumerate() {
                    if flags & bit != 0 {
                        let id = ControlId::Switch(i);
                        let _ = tx.send(if ints & bit != 0 { ControllerEvent::Pressed(id) } else { ControllerEvent::Released(id) });
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drives an encoder through A/B levels (1 = closed).
    fn turn(e: &mut EncoderState, levels: &[(u8, u8)]) {
        for &(a, b) in levels {
            if a != e.a {
                e.set_a(a);
            }
            if b != e.b {
                e.set_b(b);
            }
        }
    }

    #[test]
    fn full_cycle_encoder_counts_one_step_per_cycle_each_way() {
        let mut e = EncoderState::new(false);
        // Clockwise: 11 -> 10 -> 00 -> 01 -> 11 (A then B released, then A, B closed).
        turn(&mut e, &[(1, 0), (0, 0), (0, 1), (1, 1)]);
        assert_eq!(e.pos, 1);
        // Back: 11 -> 01 -> 00 -> 10 -> 11.
        turn(&mut e, &[(0, 1), (0, 0), (1, 0), (1, 1)]);
        assert_eq!(e.pos, 0);
        // Bounce on one line doesn't count.
        turn(&mut e, &[(1, 0), (1, 1), (1, 0), (1, 1)]);
        assert_eq!(e.pos, 0);
    }

    #[test]
    fn half_step_encoder_counts_each_half_cycle() {
        let mut e = EncoderState::new(true);
        turn(&mut e, &[(1, 0), (0, 0), (0, 1), (1, 1)]);
        assert_eq!(e.pos, 2);
        turn(&mut e, &[(0, 1), (0, 0)]);
        assert_eq!(e.pos, 1);
    }

    #[test]
    fn acceleration_matches_pihpsdr() {
        assert_eq!(accelerate(5), 5);
        assert_eq!(accelerate(-20), -20);
        assert_eq!(accelerate(100), (100 * 100 + 13800 - 776) / 117);
        assert_eq!(accelerate(-100), -accelerate(100));
    }

    #[test]
    fn every_controller_has_five_encoders_and_sixteen_switches() {
        for kind in ControllerKind::ALL {
            let cfg = ControllerConfig::defaults(kind);
            assert_eq!(cfg.encoders.len(), MAX_ENCODERS, "{kind:?}");
            assert_eq!(cfg.switches.len(), MAX_SWITCHES, "{kind:?}");
        }
        // The MCP23017 switch bits are each used once.
        let mut all = 0u16;
        for bit in I2C_SWITCH_BITS {
            assert_eq!(all & bit, 0);
            all |= bit;
        }
        assert_eq!(all, 0xFFFF);
        assert_eq!(i2c_switch_pin_name(0x8000), "GPB7");
        assert_eq!(i2c_switch_pin_name(0x0001), "GPA0");
    }
}
