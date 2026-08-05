use crate::discovery::{discover, manual_discovery, Device};
use eframe::egui;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::thread;

/// Render one grid cell as a selectable widget rather than a plain label,
/// so every column in a row participates in click-to-select and shows the
/// same highlight when the row is selected.
fn selectable_cell(
    ui: &mut egui::Ui,
    text: impl Into<egui::WidgetText>,
    selected: bool,
    enabled: bool,
) -> egui::Response {
    ui.add_enabled(enabled, egui::Button::selectable(selected, text))
}

/// What the caller should do after this frame's `show()` call.
pub enum DiscoveryAction {
    None,
    Cancelled,
    Start(Device),
}

pub struct DiscoveryWindow {
    pub open: bool,
    devices: Arc<Mutex<Vec<Device>>>,
    discovering: Arc<Mutex<bool>>,
    selected: Option<usize>,
    manual_ip: String,
    manual_error: Option<String>,
}

impl DiscoveryWindow {
    /// Creates the window and immediately kicks off a background discovery
    /// pass, same as the original GTK dialog did on open.
    pub fn new(ctx: &egui::Context) -> Self {
        let window = Self {
            open: true,
            devices: Arc::new(Mutex::new(Vec::new())),
            discovering: Arc::new(Mutex::new(false)),
            selected: None,
            manual_ip: String::new(),
            manual_error: None,
        };
        window.spawn_discovery(ctx.clone());
        window
    }

    fn spawn_discovery(&self, ctx: egui::Context) {
        let devices = Arc::clone(&self.devices);
        let discovering = Arc::clone(&self.discovering);
        *discovering.lock().unwrap() = true;
        thread::spawn(move || {
            discover(Arc::clone(&devices));
            *discovering.lock().unwrap() = false;
            ctx.request_repaint(); // wake the UI thread once results land
        });
    }

    fn spawn_manual(&self, ctx: egui::Context, ip: IpAddr) {
        let devices = Arc::clone(&self.devices);
        let discovering = Arc::clone(&self.discovering);
        *discovering.lock().unwrap() = true;
        thread::spawn(move || {
            let found = manual_discovery(Arc::clone(&devices), ip);
            *discovering.lock().unwrap() = false;
            if !found {
                eprintln!("manual_discovery: no radio responded at {ip}");
            }
            ctx.request_repaint();
        });
    }

    /// Draw the window for this frame. Call every frame while `open` is true.
    /// Takes `&mut Ui` (not `&Context`) to match eframe 0.35's App::ui model.
    pub fn show(&mut self, ui: &mut egui::Ui) -> DiscoveryAction {
        let mut action = DiscoveryAction::None;
        let mut still_open = self.open;

        // Light theme, matching the Settings window's own override (see
        // its doc comment in main.rs) -- for the same reason: egui only
        // tints a window's title bar while focused, so overriding the
        // whole window's visuals is the only way to keep it consistently
        // white regardless of focus.
        let light_visuals = egui::Visuals::light();
        let light_style = egui::Style { visuals: light_visuals.clone(), ..Default::default() };
        egui::Window::new("Discover HPSDR Radios")
            .open(&mut still_open)
            .resizable(true)
            .collapsible(false)
            .frame(egui::Frame::window(&light_style))
            .show(ui, |ui| {
                ui.visuals_mut().clone_from(&light_visuals);
                let discovering = *self.discovering.lock().unwrap();

                if discovering {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Discovering...");
                    });
                    ui.add_space(8.0);
                }

                let devices_snapshot = self.devices.lock().unwrap().clone();

                egui::Grid::new("discovery_grid")
                    .num_columns(7)
                    .striped(true)
                    .min_col_width(70.0)
                    .show(ui, |ui| {
                        for heading in
                            ["Device", "Interface", "IP", "MAC", "Protocol", "Version", "Status"]
                        {
                            ui.label(egui::RichText::new(heading).strong());
                        }
                        ui.end_row();

                        for (i, dev) in devices_snapshot.iter().enumerate() {
                            let available = dev.status == 2;
                            let is_selected = self.selected == Some(i);

                            let mut row_clicked = false;
                            row_clicked |= selectable_cell(
                                ui,
                                format!("{:?}", dev.board),
                                is_selected,
                                available,
                            )
                            .clicked();
                            row_clicked |= selectable_cell(
                                ui,
                                dev.my_address.ip().to_string(),
                                is_selected,
                                available,
                            )
                            .clicked();
                            row_clicked |= selectable_cell(
                                ui,
                                dev.address.ip().to_string(),
                                is_selected,
                                available,
                            )
                            .clicked();
                            row_clicked |= selectable_cell(
                                ui,
                                format!("{:02X?}", dev.mac),
                                is_selected,
                                available,
                            )
                            .clicked();
                            row_clicked |= selectable_cell(
                                ui,
                                dev.protocol.to_string(),
                                is_selected,
                                available,
                            )
                            .clicked();
                            row_clicked |= selectable_cell(
                                ui,
                                format!("{}.{}", dev.version / 10, dev.version % 10),
                                is_selected,
                                available,
                            )
                            .clicked();
                            row_clicked |= selectable_cell(
                                ui,
                                match dev.status {
                                    2 => "Available",
                                    3 => "In Use",
                                    _ => "Unknown",
                                },
                                is_selected,
                                available,
                            )
                            .clicked();

                            if row_clicked {
                                self.selected = Some(i);
                            }

                            ui.end_row();
                        }
                    });

                if devices_snapshot.is_empty() && !discovering {
                    ui.add_space(8.0);
                    ui.weak("No radios found. Try Rediscover or add one manually below.");
                }

                ui.add_space(12.0);
                ui.separator();

                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(!discovering, egui::Button::new("Rediscover"))
                        .clicked()
                    {
                        self.selected = None;
                        self.spawn_discovery(ui.ctx().clone());
                    }

                    ui.separator();

                    ui.label("Manual IP:");
                    ui.add_enabled(
                        !discovering,
                        egui::TextEdit::singleline(&mut self.manual_ip).desired_width(120.0),
                    );
                    if ui
                        .add_enabled(!discovering, egui::Button::new("Add"))
                        .clicked()
                    {
                        match self.manual_ip.trim().parse::<IpAddr>() {
                            Ok(ip) => {
                                self.manual_error = None;
                                self.spawn_manual(ui.ctx().clone(), ip);
                            }
                            Err(_) => {
                                self.manual_error = Some("Invalid IP address".to_string());
                            }
                        }
                    }
                });

                if let Some(err) = &self.manual_error {
                    ui.colored_label(egui::Color32::from_rgb(200, 60, 60), err);
                }

                ui.add_space(12.0);

                ui.horizontal(|ui| {
                    let can_start = self
                        .selected
                        .and_then(|i| devices_snapshot.get(i))
                        .map(|d| d.status == 2)
                        .unwrap_or(false);

                    if ui.add_enabled(can_start, egui::Button::new("Start")).clicked() {
                        if let Some(dev) = self.selected.and_then(|i| devices_snapshot.get(i)) {
                            action = DiscoveryAction::Start(*dev);
                        }
                    }

                    if ui.button("Cancel").clicked() {
                        action = DiscoveryAction::Cancelled;
                    }
                });
            });

        self.open = still_open;
        if !still_open {
            action = DiscoveryAction::Cancelled;
        }

        action
    }
}
