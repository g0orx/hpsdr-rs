use crate::bootloader_ui::FirmwareUpdateWindow;
use crate::config::Config;
use crate::discovery::{discover, manual_discovery, Boards, Device};
use eframe::egui;
use std::collections::HashMap;
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

/// Whether a device is actually startable right now -- the protocol's
/// own `status` byte, AND (RE-ADDED 2026-09-20, after
/// discovery::discover_rx888_usb was changed to actually load firmware
/// before reading the link speed -- see rx888::link_speed_warning's own
/// doc comment) gated on `usb_link_speed_warning` too. This was
/// previously reverted because the discovery-time speed check couldn't
/// reliably tell a genuinely slow link apart from a device that simply
/// hadn't loaded its real firmware yet this session -- a false positive
/// there blocked a working device from connecting at all. Now that
/// discovery brings the device up to the real streaming firmware first
/// (same PID `initialise` itself connects to), the two checks read the
/// SAME underlying speed, so gating here is no longer meaningfully less
/// trustworthy than the connect-time failure it would otherwise just
/// delay -- it just surfaces the same answer earlier.
fn device_available(dev: &Device) -> bool {
    dev.status == 2 && dev.usb_link_speed_warning.is_none()
}

/// Display text for one Ozy firmware/FPGA path row -- an explicit
/// choice always wins; otherwise shows the bundled default's own path
/// (with "(bundled)" so it's clear no action is needed) or "(not
/// found)" if even that's missing (e.g. running a non-packaged build
/// outside its source checkout with no override set yet).
fn effective_path_label(explicit: &Option<String>, default: fn() -> Option<std::path::PathBuf>) -> String {
    if let Some(p) = explicit {
        return p.clone();
    }
    match default() {
        Some(p) => format!("{} (bundled)", p.display()),
        None => "(not found -- use Choose... below)".to_string(),
    }
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
    /// See discover()'s own doc comment -- this machine's own address ->
    /// the interface name it belongs to, for the "Interface" column.
    interface_names: Arc<Mutex<HashMap<IpAddr, String>>>,
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
    /// Bootloader-mode radios never answer normal discovery (see
    /// bootloader.rs's own doc comment), so this is a standalone entry
    /// point independent of the `devices` list -- same
    /// `Option<...Window>` toggle idiom as every other secondary window
    /// in this app (e.g. ConnectedState::show_settings_window).
    firmware_update: Option<FirmwareUpdateWindow>,
    /// Classic Ozy hardware's user-supplied FX2 firmware (.hex) / FPGA
    /// bitstream (.rbf) paths -- set here, not in the (post-connect)
    /// Settings window, since Ozy needs them just to complete its very
    /// first connect. Persisted under a fixed sentinel MAC ([0;6],
    /// matching discover_ozy_usb's own synthetic Device.mac) via the
    /// same Config file mechanism every other radio's settings use --
    /// see `save_ozy_paths`.
    ozy_firmware_path: Option<String>,
    ozy_fpga_path: Option<String>,
    /// RX-888 Mk2's own user-supplied FX3 RAM image path -- same
    /// "set here, not post-connect Settings" reasoning as the Ozy paths
    /// above (needed just to complete the very first connect). See
    /// `discovery::RX888_SENTINEL_MAC`'s doc comment for why this uses a
    /// different sentinel MAC than Ozy's.
    rx888_firmware_path: Option<String>,
}

/// Sentinel MAC discover_ozy_usb's synthetic `Device` uses (Ozy has no
/// real MAC) -- doubles as a stable, dedicated Config-file identity for
/// Ozy's own global (not per-connect) settings.
const OZY_CONFIG_MAC: [u8; 6] = [0; 6];

impl DiscoveryWindow {
    /// Creates the window and immediately kicks off a background discovery
    /// pass, same as the original GTK dialog did on open.
    pub fn new(ctx: &egui::Context) -> Self {
        let ozy_cfg = Config::load(OZY_CONFIG_MAC);
        let rx888_cfg = Config::load(crate::discovery::RX888_SENTINEL_MAC);
        let window = Self {
            open: true,
            devices: Arc::new(Mutex::new(Vec::new())),
            interface_names: Arc::new(Mutex::new(HashMap::new())),
            discovering: Arc::new(Mutex::new(false)),
            selected: None,
            manual_ip: String::new(),
            manual_error: None,
            focus_deadline: Some(Instant::now() + std::time::Duration::from_millis(1500)),
            firmware_update: None,
            ozy_firmware_path: ozy_cfg.ozy_firmware_path,
            ozy_fpga_path: ozy_cfg.ozy_fpga_path,
            rx888_firmware_path: rx888_cfg.rx888_firmware_path,
        };
        window.spawn_discovery(ctx.clone());
        window
    }

    fn save_ozy_paths(&self) {
        let mut cfg = Config::load(OZY_CONFIG_MAC);
        cfg.ozy_firmware_path = self.ozy_firmware_path.clone();
        cfg.ozy_fpga_path = self.ozy_fpga_path.clone();
        cfg.save(OZY_CONFIG_MAC);
    }

    fn save_rx888_path(&self) {
        let mut cfg = Config::load(crate::discovery::RX888_SENTINEL_MAC);
        cfg.rx888_firmware_path = self.rx888_firmware_path.clone();
        cfg.save(crate::discovery::RX888_SENTINEL_MAC);
    }

    fn spawn_discovery(&self, ctx: egui::Context) {
        let devices = Arc::clone(&self.devices);
        let interface_names = Arc::clone(&self.interface_names);
        let discovering = Arc::clone(&self.discovering);
        *discovering.lock().unwrap() = true;
        thread::spawn(move || {
            discover(Arc::clone(&devices), interface_names);
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
        // Always AlwaysOnTop, for the window's whole lifetime -- a real
        // report: it was only pinned above everything for the brief
        // `focus_deadline` grace period below (originally just to win
        // the initial show/raise race against the main window -- see
        // that field's own doc comment), then dropped back to Normal,
        // letting it get buried behind other windows (a terminal, a
        // browser) while discovery is still running/waiting for a
        // selection. `focus_deadline` still separately controls the
        // one-shot keyboard-focus grab just below -- that's a different
        // concern (focus vs. stacking order) that happens to have
        // shared this same window_level gate before.
        let window_level = egui::WindowLevel::AlwaysOnTop;
        ui.ctx().show_viewport_immediate(
            egui::ViewportId::from_hash_of("discovery_window"),
            egui::ViewportBuilder::default()
                .with_title("Discover HPSDR Radios")
                // Widened from 700 -- the Interface column now shows the
                // interface name alongside its address (e.g. "eth0
                // (192.168.1.50)"), which combined with the MAC column's
                // own width was pushing the trailing Status column past
                // the fixed window edge and clipping it.
                .with_inner_size([900.0, 500.0])
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
                    if let Some(i) = devices_snapshot.iter().position(device_available) {
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

                        let interface_names = self.interface_names.lock().unwrap();
                        for (i, dev) in devices_snapshot.iter().enumerate() {
                            let available = device_available(dev);
                            let is_selected = self.selected == Some(i);

                            let mut row_clicked = false;
                            // Double-click on an available row starts it
                            // immediately, same as selecting it then
                            // pressing Start -- OR'd across all cells the
                            // same way row_clicked already is, since each
                            // is its own egui widget/Response.
                            let mut row_double_clicked = false;
                            let resp = selectable_cell(
                                ui,
                                format!("{:?}", dev.board),
                                is_selected,
                                available,
                            );
                            row_clicked |= resp.clicked();
                            row_double_clicked |= resp.double_clicked();
                            // Real interface name (e.g. "eth0") next to
                            // this machine's own address on it -- a real
                            // report that this column, despite its own
                            // "Interface" heading, was only ever showing
                            // the address. Manually-discovered devices
                            // (manual_discovery has no interface concept)
                            // just fall back to the address alone.
                            // Ozy has no real network address at all (it's
                            // USB) -- `dev.address`/`dev.my_address` are just
                            // sentinels for it (see discover_ozy_usb's doc
                            // comment), so show "USB" instead of formatting
                            // them like a real IP/interface.
                            let interface_cell = if matches!(dev.board, Boards::Ozy | Boards::Rx888) {
                                "USB".to_string()
                            } else {
                                match interface_names.get(&dev.my_address.ip()) {
                                    Some(name) => format!("{name} ({})", dev.my_address.ip()),
                                    None => dev.my_address.ip().to_string(),
                                }
                            };
                            let resp = selectable_cell(
                                ui,
                                interface_cell,
                                is_selected,
                                available,
                            );
                            row_clicked |= resp.clicked();
                            row_double_clicked |= resp.double_clicked();
                            let ip_cell = if matches!(dev.board, Boards::Ozy | Boards::Rx888) {
                                "USB".to_string()
                            } else {
                                dev.address.ip().to_string()
                            };
                            let resp = selectable_cell(
                                ui,
                                ip_cell,
                                is_selected,
                                available,
                            );
                            row_clicked |= resp.clicked();
                            row_double_clicked |= resp.double_clicked();
                            let resp = selectable_cell(
                                ui,
                                format!("{:02X?}", dev.mac),
                                is_selected,
                                available,
                            );
                            row_clicked |= resp.clicked();
                            row_double_clicked |= resp.double_clicked();
                            let resp = selectable_cell(
                                ui,
                                dev.protocol.to_string(),
                                is_selected,
                                available,
                            );
                            row_clicked |= resp.clicked();
                            row_double_clicked |= resp.double_clicked();
                            let resp = selectable_cell(
                                ui,
                                format!("{}.{}", dev.version / 10, dev.version % 10),
                                is_selected,
                                available,
                            );
                            row_clicked |= resp.clicked();
                            row_double_clicked |= resp.double_clicked();
                            let status_text = match dev.status {
                                2 => "Available",
                                3 => "In Use",
                                _ => "Unknown",
                            };
                            // See device_available's own doc comment --
                            // discovery now brings the RX-888 up to its
                            // real streaming firmware before reading this,
                            // so a warning here means the row is actually
                            // disabled (device_available returns false),
                            // not just a soft hint -- worded to match.
                            let status_text = match dev.usb_link_speed_warning {
                                Some(warning) => format!("Too slow ({warning})"),
                                None => status_text.to_string(),
                            };
                            let resp = selectable_cell(ui, status_text, is_selected, available);
                            row_clicked |= resp.clicked();
                            row_double_clicked |= resp.double_clicked();

                            if row_clicked {
                                self.selected = Some(i);
                            }
                            if row_double_clicked && available {
                                self.selected = Some(i);
                                action = DiscoveryAction::Start(*dev);
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
                        .map(device_available)
                        .unwrap_or(false);

                    if ui.add_enabled(can_start, egui::Button::new("Start")).clicked() {
                        if let Some(dev) = self.selected.and_then(|i| devices_snapshot.get(i)) {
                            action = DiscoveryAction::Start(*dev);
                        }
                    }

                    if ui.button("Cancel").clicked() {
                        action = DiscoveryAction::Cancelled;
                    }

                    ui.separator();

                    if ui.button("Firmware Update...").on_hover_text(
                        "Update FPGA firmware or change the static IP of a radio in bootloader mode \
                         (Metis/Hermes/Hermes2/Angelia/Orion/Orion2). The radio must already be \
                         physically switched into bootloader mode and power-cycled.",
                    ).clicked() {
                        self.firmware_update = Some(FirmwareUpdateWindow::new_raw_ethernet());
                    }
                });

                if let Some(fw) = &mut self.firmware_update {
                    fw.show(ui);
                    if !fw.open {
                        self.firmware_update = None;
                    }
                }

                ui.add_space(8.0);
                egui::CollapsingHeader::new("Ozy USB setup").show(ui, |ui| {
                    ui.label(
                        "Classic Ozy/Mercury/Penny hardware needs these two files \
                         to connect. hpsdr-rs bundles its own copies (sourced from \
                         piHPSDR) -- only use Choose... below to override with a \
                         different/custom build.",
                    );
                    ui.horizontal(|ui| {
                        ui.label("FX2 firmware (.hex):");
                        ui.label(effective_path_label(&self.ozy_firmware_path, crate::ozy::default_firmware_path));
                        if ui.button("Choose...").clicked() {
                            if let Some(path) =
                                rfd::FileDialog::new().add_filter("Ozy FX2 firmware", &["hex"]).pick_file()
                            {
                                self.ozy_firmware_path = Some(path.display().to_string());
                                self.save_ozy_paths();
                            }
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.label("FPGA bitstream (.rbf):");
                        ui.label(effective_path_label(&self.ozy_fpga_path, crate::ozy::default_fpga_path));
                        if ui.button("Choose...").clicked() {
                            if let Some(path) =
                                rfd::FileDialog::new().add_filter("FPGA firmware", &["rbf"]).pick_file()
                            {
                                self.ozy_fpga_path = Some(path.display().to_string());
                                self.save_ozy_paths();
                            }
                        }
                    });
                });

                ui.add_space(8.0);
                egui::CollapsingHeader::new("RX-888 USB setup").show(ui, |ui| {
                    ui.label(
                        "RX-888 Mk2 needs its own Cypress FX3 RAM image \
                         (SDDC_FX3.img) to connect -- bundled with hpsdr-rs \
                         (MIT-licensed, see assets/rx888/PROVENANCE.md), so \
                         no setup is needed unless you want to point it at \
                         your own copy instead.",
                    );
                    ui.horizontal(|ui| {
                        ui.label("FX3 firmware (.img):");
                        ui.label(effective_path_label(&self.rx888_firmware_path, crate::rx888::default_firmware_path));
                        if ui.button("Choose...").clicked() {
                            if let Some(path) =
                                rfd::FileDialog::new().add_filter("RX-888 FX3 firmware", &["img"]).pick_file()
                            {
                                self.rx888_firmware_path = Some(path.display().to_string());
                                self.save_rx888_path();
                            }
                        }
                    });
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
