/*
    A piHPSDR front-panel controller on the Raspberry Pi running
    hpsdr-remote (crates/hpsdr-controller: encoders and switches on GPIO,
    an MCP23017 on I2C): each control's action done here, as this client's
    own controls would, and sent to the server. The same controllers,
    settings and actions as hpsdr-rs's Settings -> Controller, kept in
    hpsdr-remote's own controller.json; the actions this client can't do
    aren't offered.
*/

use hpsdr_controller::{ControllerConfig, ControllerEvent, ControllerKind, MidiAction, WHEEL_ACTIONS};
use hpsdr_proto::bands;
use hpsdr_proto::cw;
use hpsdr_proto::state as dsp;
use hpsdr_proto::tci::Command;
use hpsdr_proto::tx;

use super::{App, FALLBACK_BANDS, MODES, STEPS_HZ};

/// RIT and XIT move this far a step (Hz), as in hpsdr-rs, up to this.
const RIT_STEP_HZ: i32 = 10;
const RIT_MAX_HZ: i32 = 9_999;
/// A value pop-up stays this long (s).
pub(super) const VALUE_POPUP_S: f64 = 1.5;

/// Whether this client can do `action` (Settings offers only these).
pub(super) fn offered(action: MidiAction) -> bool {
    use MidiAction::*;
    !matches!(
        action,
        SatMode | CtunToggle | RxEqToggle | DiversityToggle | RfAttenuation | CwSidetoneLevel | CwSidetoneFreq | CwPitch | CwKey
    )
}

/// The band an action selects, by its button's name.
fn band_name(action: MidiAction) -> Option<&'static str> {
    use MidiAction::*;
    Some(match action {
        Band160m => "160m",
        Band80m => "80m",
        Band40m => "40m",
        Band30m => "30m",
        Band20m => "20m",
        Band17m => "17m",
        Band15m => "15m",
        Band12m => "12m",
        Band10m => "10m",
        Band6m => "6m",
        _ => return None,
    })
}

/// Where hpsdr-remote keeps the controller's settings (native).
fn config_path() -> Option<std::path::PathBuf> {
    crate::prefs::settings_path("controller.json")
}

pub(super) fn load_config() -> ControllerConfig {
    ControllerConfig::load(config_path().as_deref())
}

impl App {
    /// Starts the controller the settings describe, if any (again, after a
    /// change to its lines).
    pub(super) fn start_controller(&mut self) {
        self.controller = None;
        if self.controller_config.kind == ControllerKind::None {
            self.controller_status = "No controller".to_string();
            return;
        }
        match hpsdr_controller::Controller::start(&self.controller_config, "hpsdr-remote") {
            Ok(c) => {
                self.controller_status = c.status();
                self.controller = Some(c);
            }
            Err(e) => {
                eprintln!("hpsdr-remote: controller: {e}");
                self.controller_status = e;
            }
        }
    }

    /// The controller's settings, in a window of their own.
    pub(super) fn controller_window(&mut self, ctx: &egui::Context) {
        if !self.controller_open {
            return;
        }
        let mut open = true;
        let screen = ctx.content_rect();
        let before = self.controller_config.clone();
        let mut cfg = before.clone();
        egui::Window::new("Controller")
            .open(&mut open)
            .max_size(screen.size() - egui::vec2(16.0, 48.0))
            .show(ctx, |ui| {
                egui::ScrollArea::both().show(ui, |ui| {
                    ui.weak(
                        "A piHPSDR front panel wired to this Raspberry Pi: rotary encoders and their push \
                         buttons on GPIO lines and, on Controller 2 and the G2 panel, 16 switches on an \
                         MCP23017 on I2C. Each knob, button and switch can do any of the actions this client \
                         has. Kept on this computer.",
                    );
                    ui.add_space(4.0);
                    hpsdr_controller::settings_ui(
                        ui,
                        &mut cfg,
                        &self.controller_status,
                        self.controller.is_some(),
                        &offered,
                        "Sensitivity: multiplies the steps. VFO Tune and VFO B Tune move by the step \
                         (Step); RIT and XIT by 10 Hz a step.",
                    );
                });
            });
        self.controller_open = open;
        if cfg != before {
            let restart = cfg.hardware() != before.hardware();
            cfg.save(config_path().as_deref());
            self.controller_config = cfg;
            if restart {
                self.start_controller();
            }
        }
    }

    /// The controller's events since the last frame.
    pub(super) fn poll_controller(&mut self) {
        let events = self.controller.as_ref().map(|c| c.poll()).unwrap_or_default();
        for ev in events {
            self.controller_event(ev);
        }
    }

    fn controller_event(&mut self, ev: ControllerEvent) {
        match ev {
            ControllerEvent::Turned(id, steps) => {
                let Some(action) = self.controller_config.action(id) else { return };
                if WHEEL_ACTIONS.contains(&action) {
                    // Sensitivity: the fraction of a step left over waits
                    // for the next turn.
                    let r = self.controller_tune_remainder.entry(id).or_insert(0.0);
                    *r += steps as f32 * self.controller_config.sensitivity(id);
                    let whole = r.trunc();
                    *r -= whole;
                    if whole != 0.0 {
                        self.wheel(action, whole as i64);
                    }
                } else {
                    self.knob(action, steps);
                }
            }
            ControllerEvent::Pressed(id) => {
                if let Some(action) = self.controller_config.action(id) {
                    self.key(action);
                }
            }
            ControllerEvent::Released(_) => {}
        }
    }

    fn popup(&mut self, text: String) {
        self.value_popup = Some((text, self.now));
    }

    /// A tuning action, by `steps`.
    fn wheel(&mut self, action: MidiAction, steps: i64) {
        match action {
            MidiAction::VfoTune => self.step(steps, self.step_hz),
            MidiAction::VfoBTune => {
                if self.radio.vfo_b_hz > 0 {
                    let step = self.step_hz as i64;
                    let hz = ((self.radio.vfo_b_hz as i64 / step + steps) * step).max(0) as u64;
                    self.set_vfo_b(hz);
                }
            }
            MidiAction::RitAdjust | MidiAction::XitAdjust => {
                let rit = action == MidiAction::RitAdjust;
                let (_, offset) = if rit { self.radio.rit } else { self.radio.xit };
                let offset = (offset + steps as i32 * RIT_STEP_HZ).clamp(-RIT_MAX_HZ, RIT_MAX_HZ);
                self.set_rit_offset(rit, offset);
                self.popup(format!("{} {offset:+} Hz", if rit { "RIT" } else { "XIT" }));
            }
            _ => {}
        }
    }

    /// A setting moved by `steps`.
    fn knob(&mut self, action: MidiAction, steps: i32) {
        match action {
            MidiAction::AfGain => {
                self.af_gain_db = (self.af_gain_db + steps as f32).clamp(-40.0, 20.0);
                self.apply_gain();
                self.popup(format!("Audio {:.0} dB", self.af_gain_db));
            }
            MidiAction::MicGain => {
                self.mic_gain_db = (self.mic_gain_db + steps as f32).clamp(-20.0, 30.0);
                self.popup(format!("Mic {:.0} dB", self.mic_gain_db));
            }
            MidiAction::AgcGain => {
                let Some(mut d) = self.radio.dsp.clone() else { return };
                d.agc_gain_db = (d.agc_gain_db + steps).clamp(0, 140);
                let text = format!("AGC Gain {} dB", d.agc_gain_db);
                self.set_dsp(d, dsp::SET_AGC_GAIN, |d| d.agc_gain_db.to_string());
                self.popup(text);
            }
            MidiAction::FilterWidth => {
                let Some(mut d) = self.radio.dsp.clone() else { return };
                d.width_hz = (d.width_hz as i64 + steps as i64 * 50).clamp(50, 10_000) as u32;
                let text = format!("Filter {}", super::format_hz(d.width_hz));
                self.set_dsp(d, dsp::SET_WIDTH, |d| d.width_hz.to_string());
                self.popup(text);
            }
            MidiAction::TxDrive => {
                if !self.radio.can_transmit() {
                    return;
                }
                let max = self.radio.tx.unwrap_or_default().max_power_w.max(1) as i64;
                self.power_w = (self.power_w as i64 + steps as i64).clamp(0, max) as u32;
                self.power_set_at = self.now;
                let watts = self.power_w.to_string();
                self.send(Command::new(dsp::SET, &[tx::SET_POWER, &watts]));
                self.popup(format!("Power {} W", self.power_w));
            }
            MidiAction::CwSpeed => {
                self.cw_speed_wpm = (self.cw_speed_wpm as i64 + steps as i64).clamp(cw::SPEED_MIN as i64, cw::SPEED_MAX as i64) as u32;
                self.cw_speed_set_at = self.now;
                self.send(cw::speed(self.cw_speed_wpm));
                self.popup(format!("CW {} WPM", self.cw_speed_wpm));
            }
            _ => {}
        }
    }

    /// A button pressed.
    fn key(&mut self, action: MidiAction) {
        use MidiAction::*;
        match action {
            Mox => {
                // As the MOX button: not in CW unless to stop.
                if self.radio.can_transmit() && (!self.radio.is_cw() || self.radio.mox) {
                    self.set_mox(!self.radio.mox);
                }
            }
            Tune => {
                if self.radio.can_transmit() {
                    self.radio.tune = !self.radio.tune;
                    self.controls_set_at = self.now;
                    self.send(Command::new("tune", &["0", &self.radio.tune.to_string()]));
                }
            }
            Split => {
                self.radio.split = !self.radio.split;
                self.controls_set_at = self.now;
                self.send(Command::new("split_enable", &["0", &self.radio.split.to_string()]));
                self.popup(format!("Split {}", on_off(self.radio.split)));
            }
            RitToggle | XitToggle => {
                let rit = action == RitToggle;
                let on = !if rit { self.radio.rit.0 } else { self.radio.xit.0 };
                if rit {
                    self.radio.rit.0 = on;
                } else {
                    self.radio.xit.0 = on;
                }
                self.controls_set_at = self.now;
                let name = if rit { "rit_enable" } else { "xit_enable" };
                self.send(Command::new(name, &["0", &on.to_string()]));
                self.popup(format!("{} {}", if rit { "RIT" } else { "XIT" }, on_off(on)));
            }
            RitClear => self.set_rit_offset(true, 0),
            XitClear => self.set_rit_offset(false, 0),
            VfoAtoB => self.set_vfo_b(self.radio.vfo_hz),
            VfoBtoA => {
                if self.radio.vfo_b_hz > 0 {
                    self.tune(self.radio.vfo_b_hz);
                }
            }
            VfoSwap => {
                let (a, b) = (self.radio.vfo_hz, self.radio.vfo_b_hz);
                if a > 0 && b > 0 {
                    self.tune(b);
                    self.set_vfo_b(a);
                }
            }
            ModeUp | ModeDown => {
                let i = MODES.iter().position(|(m, _)| *m == self.radio.mode).unwrap_or(0) as i64;
                let n = MODES.len() as i64;
                let (tci, label) = MODES[(i + if action == ModeUp { 1 } else { -1 }).rem_euclid(n) as usize];
                self.radio.mode = tci.to_string();
                self.send(Command::new("modulation", &["0", tci]));
                self.popup(label.to_string());
            }
            BandUp | BandDown => {
                let names = self.band_names();
                if names.is_empty() {
                    return;
                }
                let n = names.len() as i64;
                let next = match self.current_band_name().and_then(|c| names.iter().position(|b| *b == c)) {
                    Some(i) => (i as i64 + if action == BandUp { 1 } else { -1 }).rem_euclid(n) as usize,
                    None => 0,
                };
                self.select_band(&names[next]);
            }
            // The current band again: hpsdr-rs steps its band stack.
            BandStack => {
                if self.radio.bands.is_some() {
                    if let Some(c) = self.current_band_name() {
                        self.send(bands::select(&c));
                    }
                }
            }
            FilterWidthUp | FilterWidthDown => {
                let Some(mut d) = self.radio.dsp.clone() else { return };
                let presets = super::filter_presets(&self.radio.mode);
                if presets.is_empty() {
                    return;
                }
                let w = if action == FilterWidthUp {
                    presets.iter().copied().find(|&p| p > d.width_hz).unwrap_or(presets[presets.len() - 1])
                } else {
                    presets.iter().rev().copied().find(|&p| p < d.width_hz).unwrap_or(presets[0])
                };
                d.width_hz = w;
                self.set_dsp(d, dsp::SET_WIDTH, |d| d.width_hz.to_string());
                self.popup(format!("Filter {}", super::format_hz(w)));
            }
            VfoStepUp => self.step(1, self.step_hz),
            VfoStepDown => self.step(-1, self.step_hz),
            VfoStepSizeUp | VfoStepSizeDown => {
                let i = STEPS_HZ.iter().position(|(hz, _)| *hz == self.step_hz).unwrap_or(0);
                let i = if action == VfoStepSizeUp { (i + 1).min(STEPS_HZ.len() - 1) } else { i.saturating_sub(1) };
                self.step_hz = STEPS_HZ[i].0;
                self.popup(format!("Step {}", STEPS_HZ[i].1));
            }
            NoiseBlankerCycle | NoiseBlankerOff | NoiseBlankerNb | NoiseBlankerNb2 => {
                let Some(mut d) = self.radio.dsp.clone() else { return };
                let i = match action {
                    NoiseBlankerOff => 0,
                    NoiseBlankerNb => 1,
                    NoiseBlankerNb2 => 2,
                    _ => (dsp::NB_MODES.iter().position(|m| *m == d.nb).unwrap_or(0) + 1) % dsp::NB_MODES.len(),
                };
                d.nb = dsp::NB_MODES[i].to_string();
                self.set_dsp(d, dsp::SET_NB, |d| d.nb.clone());
                self.popup(["NB Off", "NB", "NB2"][i].to_string());
            }
            NoiseReductionCycle | NoiseReductionOff | NoiseReductionNr | NoiseReductionNr2 | NoiseReductionNr3 => {
                let Some(mut d) = self.radio.dsp.clone() else { return };
                let i = match action {
                    NoiseReductionOff => 0,
                    NoiseReductionNr => 1,
                    NoiseReductionNr2 => 2,
                    NoiseReductionNr3 => 3,
                    _ => (dsp::NR_MODES.iter().position(|m| *m == d.nr).unwrap_or(0) + 1) % dsp::NR_MODES.len(),
                };
                d.nr = dsp::NR_MODES[i].to_string();
                self.set_dsp(d, dsp::SET_NR, |d| d.nr.clone());
                self.popup(["NR Off", "NR", "NR2", "NNR"][i].to_string());
            }
            SnbToggle => {
                let Some(mut d) = self.radio.dsp.clone() else { return };
                d.snb = !d.snb;
                let text = format!("SNB {}", on_off(d.snb));
                self.set_dsp(d, dsp::SET_SNB, |d| d.snb.to_string());
                self.popup(text);
            }
            BinauralToggle => {
                self.radio.binaural = !self.radio.binaural;
                self.controls_set_at = self.now;
                self.send(Command::new("rx_bin_enable", &["0", &self.radio.binaural.to_string()]));
                self.popup(format!("Binaural {}", on_off(self.radio.binaural)));
            }
            a => {
                if let Some(name) = band_name(a) {
                    if self.band_names().iter().any(|b| b == name) {
                        self.select_band(name);
                    }
                }
            }
        }
    }

    /// A DSP setting changed: `d` with `name` changed to `value(d)`.
    fn set_dsp(&mut self, d: dsp::DspState, name: &str, value: impl Fn(&dsp::DspState) -> String) {
        let v = value(&d);
        self.radio.dsp = Some(d);
        self.dsp_set_at = self.now;
        self.send(dsp::set(name, &v));
    }

    fn set_vfo_b(&mut self, hz: u64) {
        if hz == 0 {
            return;
        }
        self.radio.vfo_b_hz = hz;
        self.controls_set_at = self.now;
        self.send(Command::new("vfo", &["0", "1", &hz.to_string()]));
    }

    fn set_rit_offset(&mut self, rit: bool, hz: i32) {
        if rit {
            self.radio.rit.1 = hz;
        } else {
            self.radio.xit.1 = hz;
        }
        self.controls_set_at = self.now;
        let name = if rit { "rit_offset" } else { "xit_offset" };
        self.send(Command::new(name, &["0", &hz.to_string()]));
    }

    /// The band buttons' names: the server's, or this client's own.
    fn band_names(&self) -> Vec<String> {
        match &self.radio.bands {
            Some(list) => list.iter().map(|b| b.name.clone()).collect(),
            None => FALLBACK_BANDS.iter().map(|b| b.0.to_string()).collect(),
        }
    }

    fn current_band_name(&self) -> Option<String> {
        match &self.radio.bands {
            Some(list) => super::current_band(list, self.radio.vfo_hz).map(str::to_string),
            None => FALLBACK_BANDS.iter().find(|b| (b.1..=b.2).contains(&self.radio.vfo_hz)).map(|b| b.0.to_string()),
        }
    }

    /// Selects a band as its button does.
    fn select_band(&mut self, name: &str) {
        if self.radio.bands.is_some() {
            self.send(bands::select(name));
        } else {
            self.select_fallback_band(name);
        }
        self.popup(name.to_string());
    }
}

fn on_off(on: bool) -> &'static str {
    if on {
        "On"
    } else {
        "Off"
    }
}
