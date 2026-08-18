use crate::discovery::{discover, manual_discovery, Device};
use eframe::egui;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;

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
    /// When this window was created -- used to keep re-sending a focus
    /// command for a short window after creation (see `show`'s doc
    /// comment on why a single one-shot Focus isn't reliable enough: the
    /// main/root window is a genuinely separate OS window that can get
    /// mapped/raised by the window manager slightly *after* this one,
    /// re-covering it even though this window already received focus a
    /// moment earlier). `None` once that grace period has ended so the
    /// window stops stealing focus back if the user deliberately clicks
    /// the main window during that window.
    focus_deadline: Option<Instant>,
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
            focus_deadline: Some(Instant::now() + std::time::Duration::from_millis(1500)),
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
        // Rendered in its own OS-level viewport (like the extra receiver
        // windows and the Settings window -- see its doc comment in
        // main.rs) rather than an embedded egui::Window, so it can be
        // dragged outside the main window's bounds. show_viewport_immediate
        // (not _deferred) since this closure borrows `self`/`action`
        // directly by reference rather than through an Arc<Mutex<>>.
        // While `focus_deadline` is still active, also force the window
        // level to AlwaysOnTop -- a plain Focus command only asks for
        // keyboard focus, but the actual symptom here is the *main*
        // window's own initial show/raise (eframe reveals its root
        // window only after the first frame paints, and most window
        // managers raise a window when it's newly shown) re-covering
        // this one in stacking order a moment later regardless of which
        // window has focus. AlwaysOnTop guarantees stacking order
        // directly instead of depending on that race. Dropped back to
        // Normal once the deadline passes so this window doesn't stay
        // pinned above everything (e.g. the Settings window) forever.
        let window_level = if self.focus_deadline.is_some() {
            egui::WindowLevel::AlwaysOnTop
        } else {
            egui::WindowLevel::Normal
        };
        ui.ctx().show_viewport_immediate(
            egui::ViewportId::from_hash_of("discovery_window"),
            egui::ViewportBuilder::default()
                .with_title("Discover HPSDR Radios")
                .with_inner_size([700.0, 500.0])
                .with_active(true)
                .with_window_level(window_level),
            |ui, _class| {
                if let Some(deadline) = self.focus_deadline {
                    let focused = ui.input(|i| i.viewport().focused).unwrap_or(false);
                    if focused || Instant::now() >= deadline {
                        self.focus_deadline = None;
                    } else {
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Focus);
                        ui.ctx().request_repaint();
                    }
                }
                if ui.input(|i| i.viewport().close_requested()) {
                    still_open = false;
                    return;
                }
                egui::CentralPanel::default().frame(egui::Frame::central_panel(&light_style)).show(
                    ui,
                    |ui| {
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

                // Default to the first AVAILABLE device in the list as
                // soon as results land, so a single radio (the common
                // case) is ready to Start immediately without an extra
                // click. Specifically NOT just index 0 -- a radio
                // already in use by another program (status 3) still
                // shows up in the list, and defaulting to it would
                // select something Start can't actually act on (the
                // button is disabled for non-available rows) instead of
                // a real radio that's actually usable right now. Only
                // fires while nothing is selected yet -- Rediscover
                // explicitly resets `selected` to None (below) so this
                // re-applies to the next batch of results rather than
                // fighting a deliberate user selection.
                if self.selected.is_none() {
                    if let Some(i) = devices_snapshot.iter().position(|d| d.status == 2) {
                        self.selected = Some(i);
                    }
                }

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
            },
        );
        self.open = still_open;

        if !still_open {
            action = DiscoveryAction::Cancelled;
        }

        action
    }
}
