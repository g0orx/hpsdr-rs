//! The Settings page for a front-panel controller, shared by hpsdr-rs and
//! hpsdr-remote.

use egui::Color32;

use crate::{ControllerConfig, ControllerKind, EncoderLines, MidiAction, SwitchLine, KEY_ACTIONS, KNOB_ACTIONS, WHEEL_ACTIONS};

/// Dimmed on a light background, so green and red stay readable.
fn readable(ui: &egui::Ui, c: Color32) -> Color32 {
    if ui.visuals().dark_mode {
        return c;
    }
    let [r, g, b, a] = c.to_array();
    let dim = |v: u8| (v as f32 * 0.6) as u8;
    Color32::from_rgba_premultiplied(dim(r), dim(g), dim(b), a)
}

/// The controller's settings, edited in place: which controller, its lines,
/// and each control's action -- from those `offered` allows. `status` is
/// the running controller's (green when `running`, red otherwise);
/// `sensitivity_hint` explains a tuning knob's sensitivity. The caller
/// compares `cfg` with what it was to save it, and starts the controller
/// again if ControllerConfig::hardware changed.
pub fn settings_ui(
    ui: &mut egui::Ui,
    cfg: &mut ControllerConfig,
    status: &str,
    running: bool,
    offered: &dyn Fn(MidiAction) -> bool,
    sensitivity_hint: &str,
) {
    ui.horizontal(|ui| {
        ui.label("Controller:");
        let mut kind = cfg.kind;
        egui::ComboBox::from_id_salt("controller_kind").selected_text(kind.label()).show_ui(ui, |ui| {
            for k in ControllerKind::ALL {
                ui.selectable_value(&mut kind, k, k.label());
            }
        });
        if kind != cfg.kind {
            *cfg = ControllerConfig::defaults(kind);
        }
        if cfg.kind != ControllerKind::None
            && ui.button("Restore defaults").on_hover_text("piHPSDR's lines and actions for this controller").clicked()
        {
            *cfg = ControllerConfig::defaults(cfg.kind);
        }
    });
    let status_color = if cfg.kind == ControllerKind::None {
        ui.visuals().weak_text_color()
    } else if running {
        readable(ui, Color32::from_rgb(60, 180, 60))
    } else {
        readable(ui, Color32::from_rgb(220, 60, 60))
    };
    ui.colored_label(status_color, status);

    // An action list: none, then `choices`.
    fn action_combo(ui: &mut egui::Ui, salt: impl std::hash::Hash + std::fmt::Debug, action: &mut Option<MidiAction>, choices: &[MidiAction]) {
        egui::ComboBox::from_id_salt(salt)
            .selected_text(action.map_or("--", |a| a.label()))
            .width(150.0)
            .show_ui(ui, |ui| {
                ui.selectable_value(action, None, "--");
                for &a in choices {
                    ui.selectable_value(action, Some(a), a.label());
                }
            });
    }
    let knob_choices: Vec<MidiAction> = WHEEL_ACTIONS.iter().chain(KNOB_ACTIONS).copied().filter(|&a| offered(a)).collect();
    let key_choices: Vec<MidiAction> =
        KEY_ACTIONS.iter().copied().filter(|&a| a != MidiAction::CwKey && offered(a)).collect();
    let pin = |ui: &mut egui::Ui, value: &mut u32| {
        ui.add(egui::DragValue::new(value).range(0..=53).prefix("GPIO "));
    };
    let encoder_cell = |ui: &mut egui::Ui, salt: (&str, usize), e: &mut EncoderLines| {
        if !e.enabled {
            ui.weak("--");
            return;
        }
        ui.horizontal(|ui| {
            pin(ui, &mut e.pin_a);
            pin(ui, &mut e.pin_b);
            action_combo(ui, salt, &mut e.action, &knob_choices);
            if e.action.is_some_and(|a| WHEEL_ACTIONS.contains(&a)) {
                ui.add(egui::DragValue::new(&mut e.sensitivity).range(0.1..=20.0).speed(0.05).prefix("x"))
                    .on_hover_text(sensitivity_hint);
            }
        });
    };
    let button_cell = |ui: &mut egui::Ui, salt: (&str, usize), s: &mut SwitchLine, show_pin: bool| {
        if !s.enabled && show_pin {
            ui.weak("--");
            return;
        }
        ui.horizontal(|ui| {
            if show_pin {
                pin(ui, &mut s.pin);
            }
            action_combo(ui, salt, &mut s.action, &key_choices);
        });
    };

    if cfg.kind != ControllerKind::None {
        ui.add_space(8.0);
        ui.strong("Encoders");
        ui.weak("Each with its A and B lines; turning one moves its action's setting, or tunes.");
        egui::Grid::new("controller_encoders").striped(true).show(ui, |ui| {
            ui.label("");
            ui.label("Bottom (or only)");
            ui.label("Top");
            ui.label("Push button");
            ui.end_row();
            for (i, e) in cfg.encoders.iter_mut().enumerate() {
                if !e.bottom.enabled && !e.top.enabled && !e.button.enabled {
                    continue;
                }
                ui.label(if e.bottom.action == Some(MidiAction::VfoTune) && !e.top.enabled {
                    "VFO".to_string()
                } else {
                    format!("Encoder {}", i + 1)
                });
                encoder_cell(ui, ("enc_bottom", i), &mut e.bottom);
                encoder_cell(ui, ("enc_top", i), &mut e.top);
                button_cell(ui, ("enc_button", i), &mut e.button, true);
                ui.end_row();
            }
        });

        ui.add_space(8.0);
        let i2c = cfg.kind.uses_i2c();
        ui.strong("Switches");
        if i2c {
            ui.weak("On the MCP23017, by its input.");
        }
        egui::Grid::new("controller_switches").striped(true).show(ui, |ui| {
            let mut column = 0;
            for (i, s) in cfg.switches.iter_mut().enumerate() {
                if !i2c && !s.enabled {
                    continue;
                }
                ui.label(if i2c {
                    crate::i2c_switch_pin_name(crate::I2C_SWITCH_BITS[i])
                } else {
                    format!("Switch {}", i + 1)
                });
                button_cell(ui, ("switch", i), s, !i2c);
                column += 1;
                if column % 2 == 0 {
                    ui.end_row();
                }
            }
        });

        if i2c {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.label("MCP23017:");
                ui.add(egui::TextEdit::singleline(&mut cfg.i2c_device).desired_width(100.0));
                ui.label("address");
                ui.add(egui::DragValue::new(&mut cfg.i2c_address).range(0x08..=0x77).hexadecimal(2, false, true));
                ui.label("interrupt on");
                pin(ui, &mut cfg.i2c_irq_pin);
            });
        }
        ui.weak(
            "Lines are Broadcom GPIO numbers, as piHPSDR's. On a Raspberry Pi 5 GPIO 20 may be unavailable: \
             rewire that encoder line to GPIO 14 and set it here. Your user needs to be in the 'gpio' and \
             'i2c' groups, and I2C enabled (raspi-config).",
        );
    }

}
