/*
    Firmware Update window -- UI for bootloader.rs's two upload protocols.
    Mirrors discovery_ui.rs's shape (a self-contained window struct with
    its own `show(&mut self, ui: &mut egui::Ui)`), since this is a
    substantial standalone window, not a small addition to an existing
    screen -- see bootloader.rs's own doc comment for the protocol details
    this drives.
*/

use crate::bootloader::{self, UploadHandle, UploadStage};
use eframe::egui;
use pnet_datalink::NetworkInterface;
use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const WARNING_COLOR: egui::Color32 = egui::Color32::from_rgb(180, 90, 0);
const ERROR_COLOR: egui::Color32 = egui::Color32::from_rgb(200, 40, 40);
const OK_COLOR: egui::Color32 = egui::Color32::from_rgb(0, 130, 0);

/// A one-shot background operation (sub-second to ~1s, e.g. Test for
/// Bootloader/Read IP/Write IP) plus its polled result -- same "spawn a
/// thread, poll an Arc<Mutex<>> each frame, ctx.request_repaint() on
/// completion" idiom as discovery_ui.rs's `discovering`/`devices` fields,
/// generalized so each action here doesn't need its own hand-duplicated
/// pair of fields. NOT used for Erase+Program, which needs live
/// percentage progress -- see bootloader::UploadHandle for that instead.
struct AsyncOp<T> {
    running: Arc<AtomicBool>,
    result: Arc<Mutex<Option<Result<T, String>>>>,
}

impl<T: Send + Clone + 'static> AsyncOp<T> {
    fn new() -> Self {
        Self { running: Arc::new(AtomicBool::new(false)), result: Arc::new(Mutex::new(None)) }
    }

    fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    fn spawn(&self, ctx: egui::Context, f: impl FnOnce() -> Result<T, String> + Send + 'static) {
        self.running.store(true, Ordering::Relaxed);
        *self.result.lock().unwrap() = None;
        let running = Arc::clone(&self.running);
        let result = Arc::clone(&self.result);
        thread::spawn(move || {
            let outcome = f();
            *result.lock().unwrap() = Some(outcome);
            running.store(false, Ordering::Relaxed);
            ctx.request_repaint();
        });
    }

    fn peek(&self) -> Option<Result<T, String>> {
        self.result.lock().unwrap().clone()
    }
}

/// Which protocol/target this window is driving -- see bootloader.rs's own
/// module doc comment for why these are two unrelated mechanisms, not one.
enum Target {
    /// P1 raw-Ethernet bootloader mode, entered from the Discovery screen
    /// (bootloader-mode radios never answer normal discovery, so there's
    /// no already-known IP/MAC to start from -- the user picks a network
    /// interface and this window finds the radio itself via "Test for
    /// Bootloader").
    RawEthernet { interfaces: Vec<NetworkInterface>, selected: Option<usize> },
    /// P2 in-application update, against the radio this session is
    /// already connected to.
    InApp { radio_ip: IpAddr, radio_mac: [u8; 6] },
}

pub struct FirmwareUpdateWindow {
    pub open: bool,
    target: Target,
    test_mac: AsyncOp<[u8; 6]>,
    read_ip: AsyncOp<[u8; 4]>,
    write_ip_op: AsyncOp<()>,
    new_ip_text: String,
    firmware_path: Option<PathBuf>,
    firmware_read_error: Option<String>,
    upload: Option<UploadHandle>,
    confirm: bool,
}

impl FirmwareUpdateWindow {
    pub fn new_raw_ethernet() -> Self {
        Self {
            open: true,
            target: Target::RawEthernet { interfaces: bootloader::list_raw_interfaces(), selected: None },
            test_mac: AsyncOp::new(),
            read_ip: AsyncOp::new(),
            write_ip_op: AsyncOp::new(),
            new_ip_text: String::new(),
            firmware_path: None,
            firmware_read_error: None,
            upload: None,
            confirm: false,
        }
    }

    pub fn new_in_app(radio_ip: IpAddr, radio_mac: [u8; 6]) -> Self {
        Self {
            open: true,
            target: Target::InApp { radio_ip, radio_mac },
            test_mac: AsyncOp::new(),
            read_ip: AsyncOp::new(),
            write_ip_op: AsyncOp::new(),
            new_ip_text: String::new(),
            firmware_path: None,
            firmware_read_error: None,
            upload: None,
            confirm: false,
        }
    }

    /// Draw the window for this frame -- same viewport/light-theme
    /// conventions as every other secondary window in this app (see
    /// discovery_ui.rs's DiscoveryWindow::show doc comment). Called from
    /// within the caller's own viewport closure (nested viewports are
    /// fine -- egui's per-receiver Settings sub-window already does this).
    pub fn show(&mut self, ui: &mut egui::Ui) {
        let light_visuals = egui::Visuals::light();
        let light_style = egui::Style { visuals: light_visuals.clone(), ..Default::default() };
        let mut still_open = self.open;
        ui.ctx().show_viewport_immediate(
            egui::ViewportId::from_hash_of("firmware_update_window"),
            egui::ViewportBuilder::default().with_title("Firmware Update").with_inner_size([560.0, 520.0]),
            |ui, _class| {
                if ui.input(|i| i.viewport().close_requested()) {
                    still_open = false;
                    return;
                }
                egui::CentralPanel::default().frame(egui::Frame::central_panel(&light_style)).show(ui, |ui| {
                    ui.visuals_mut().clone_from(&light_visuals);
                    self.show_contents(ui);
                });
            },
        );
        self.open = still_open;
    }

    fn show_contents(&mut self, ui: &mut egui::Ui) {
        let uploading = self.upload.is_some();

        ui.colored_label(
            WARNING_COLOR,
            "Do not disconnect the radio or close this window during Erase/Program. If an \
             update is interrupted, the radio will need to be power-cycled before trying again.",
        );
        ui.add_space(8.0);

        match &mut self.target {
            Target::RawEthernet { interfaces, selected } => {
                Self::show_raw_ethernet_target(
                    ui,
                    interfaces,
                    selected,
                    uploading,
                    &self.test_mac,
                    &self.read_ip,
                    &self.write_ip_op,
                    &mut self.new_ip_text,
                );
            }
            Target::InApp { radio_ip, radio_mac } => {
                Self::show_in_app_target(ui, *radio_ip, *radio_mac, uploading, &self.write_ip_op, &mut self.new_ip_text);
            }
        }

        ui.add_space(12.0);
        ui.separator();
        ui.add_space(8.0);

        ui.horizontal(|ui| {
            ui.label("Firmware file:");
            ui.label(
                self.firmware_path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "(none selected)".to_string()),
            );
            if ui.add_enabled(!uploading, egui::Button::new("Browse...")).clicked() {
                if let Some(path) = rfd::FileDialog::new().add_filter("FPGA firmware", &["rbf"]).pick_file() {
                    self.firmware_path = Some(path);
                    self.firmware_read_error = None;
                    self.confirm = false;
                }
            }
        });
        if let Some(path) = &self.firmware_path {
            if path.extension().and_then(|e| e.to_str()) != Some("rbf") {
                ui.colored_label(
                    WARNING_COLOR,
                    "This file doesn't have a .rbf extension -- double check it's the right firmware image.",
                );
            }
        }
        if let Some(err) = &self.firmware_read_error {
            ui.colored_label(ERROR_COLOR, err);
        }

        ui.add_space(8.0);

        let target_ready = match &self.target {
            Target::RawEthernet { selected, .. } => selected.is_some(),
            Target::InApp { .. } => true,
        };
        let ready_to_start = !uploading && target_ready && self.firmware_path.is_some();

        if !ready_to_start {
            ui.add_enabled(false, egui::Button::new("Erase && Program..."));
        } else if !self.confirm {
            if ui.button("Erase && Program...").clicked() {
                self.confirm = true;
            }
        } else {
            ui.colored_label(
                ERROR_COLOR,
                "This will erase and reprogram the radio's firmware. This cannot be undone. Are you sure?",
            );
            ui.horizontal(|ui| {
                if ui.button("Yes, start Erase && Program").clicked() {
                    self.confirm = false;
                    self.start_upload(ui.ctx());
                }
                if ui.button("Cancel").clicked() {
                    self.confirm = false;
                }
            });
        }

        self.show_upload_progress(ui);
    }

    #[allow(clippy::too_many_arguments)]
    fn show_raw_ethernet_target(
        ui: &mut egui::Ui,
        interfaces: &[NetworkInterface],
        selected: &mut Option<usize>,
        uploading: bool,
        test_mac: &AsyncOp<[u8; 6]>,
        read_ip: &AsyncOp<[u8; 4]>,
        write_ip_op: &AsyncOp<()>,
        new_ip_text: &mut String,
    ) {
        ui.colored_label(
            WARNING_COLOR,
            "Before you begin: switch this radio into bootloader mode and power-cycle it \
             (see the radio's own documentation for how -- a jumper or slide switch, \
             board-dependent). It won't respond to anything below until you do.",
        );
        ui.add_space(6.0);
        ui.label("Network interface (direct cable or plain switch to the radio -- won't cross a router/VPN/most managed switches):");
        egui::ComboBox::from_id_salt("fw_update_interface")
            .selected_text(
                selected
                    .and_then(|i| interfaces.get(i))
                    .map(|i| i.name.clone())
                    .unwrap_or_else(|| "Select...".to_string()),
            )
            .show_ui(ui, |ui| {
                for (i, iface) in interfaces.iter().enumerate() {
                    let label = match iface.mac {
                        Some(mac) => format!("{} ({mac})", iface.name),
                        None => iface.name.clone(),
                    };
                    ui.selectable_value(selected, Some(i), label);
                }
            });

        ui.add_space(8.0);
        ui.horizontal(|ui| {
            let can_test = !uploading && !test_mac.is_running() && selected.is_some();
            if ui.add_enabled(can_test, egui::Button::new("Test for Bootloader")).clicked() {
                let iface = interfaces[selected.unwrap()].clone();
                let ctx = ui.ctx().clone();
                test_mac.spawn(ctx, move || {
                    bootloader::RawBootloader::new(iface).read_mac().map_err(|e| e.to_string())
                });
            }
            if test_mac.is_running() {
                ui.spinner();
            }
        });
        if let Some(result) = test_mac.peek() {
            match result {
                Ok(mac) => {
                    ui.colored_label(
                        OK_COLOR,
                        format!(
                            "Found a radio in bootloader mode (MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}).",
                            mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
                        ),
                    );
                }
                Err(e) => {
                    ui.colored_label(ERROR_COLOR, format!("No response: {e}"));
                }
            }
        }

        ui.add_space(8.0);
        ui.horizontal(|ui| {
            let can_read = !uploading && !read_ip.is_running() && selected.is_some();
            if ui.add_enabled(can_read, egui::Button::new("Read Current IP")).clicked() {
                let iface = interfaces[selected.unwrap()].clone();
                let ctx = ui.ctx().clone();
                read_ip.spawn(ctx, move || bootloader::RawBootloader::new(iface).read_ip().map_err(|e| e.to_string()));
            }
            if read_ip.is_running() {
                ui.spinner();
            }
        });
        if let Some(result) = read_ip.peek() {
            match result {
                Ok(ip) => {
                    ui.label(format!("Current IP: {}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3]));
                }
                Err(e) => {
                    ui.colored_label(ERROR_COLOR, format!("Read IP failed: {e}"));
                }
            }
        }

        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.label("New IP:");
            ui.add(egui::TextEdit::singleline(new_ip_text).desired_width(140.0));
            let parsed_ip = new_ip_text.trim().parse::<Ipv4Addr>().ok();
            let can_write = !uploading && !write_ip_op.is_running() && selected.is_some() && parsed_ip.is_some();
            if ui.add_enabled(can_write, egui::Button::new("Write IP")).clicked() {
                let iface = interfaces[selected.unwrap()].clone();
                let ip = parsed_ip.unwrap().octets();
                let ctx = ui.ctx().clone();
                write_ip_op.spawn(ctx, move || bootloader::RawBootloader::new(iface).write_ip(ip).map_err(|e| e.to_string()));
            }
            if write_ip_op.is_running() {
                ui.spinner();
            }
        });
        if let Some(result) = write_ip_op.peek() {
            match result {
                Ok(()) => {
                    ui.colored_label(OK_COLOR, "IP written -- use Read Current IP to confirm.");
                }
                Err(e) => {
                    ui.colored_label(ERROR_COLOR, format!("Write IP failed: {e}"));
                }
            }
        }
    }

    fn show_in_app_target(
        ui: &mut egui::Ui,
        radio_ip: IpAddr,
        radio_mac: [u8; 6],
        uploading: bool,
        write_ip_op: &AsyncOp<()>,
        new_ip_text: &mut String,
    ) {
        ui.colored_label(
            WARNING_COLOR,
            "This update method is less thoroughly verified against real firmware than \
             bootloader-mode update -- prefer that when available.",
        );
        ui.add_space(6.0);
        ui.label(format!(
            "Target radio: {radio_ip} (MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x})",
            radio_mac[0], radio_mac[1], radio_mac[2], radio_mac[3], radio_mac[4], radio_mac[5]
        ));

        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.label("New IP:");
            ui.add(egui::TextEdit::singleline(new_ip_text).desired_width(140.0));
            let parsed_ip = new_ip_text.trim().parse::<Ipv4Addr>().ok();
            let can_write = !uploading && !write_ip_op.is_running() && parsed_ip.is_some();
            if ui.add_enabled(can_write, egui::Button::new("Write IP")).clicked() {
                let ip = parsed_ip.unwrap().octets();
                let ctx = ui.ctx().clone();
                write_ip_op
                    .spawn(ctx, move || bootloader::InAppUpdate::new(radio_ip).set_ip(radio_mac, ip).map_err(|e| e.to_string()));
            }
            if write_ip_op.is_running() {
                ui.spinner();
            }
        });
        if let Some(result) = write_ip_op.peek() {
            match result {
                Ok(()) => {
                    ui.colored_label(OK_COLOR, "IP change sent (no confirmation reply is expected).");
                }
                Err(e) => {
                    ui.colored_label(ERROR_COLOR, format!("Write IP failed: {e}"));
                }
            }
        }
    }

    fn show_upload_progress(&mut self, ui: &mut egui::Ui) {
        let Some(handle) = &self.upload else { return };
        let stage = handle.progress.lock().unwrap().clone();

        ui.add_space(12.0);
        match &stage {
            UploadStage::Erasing => {
                ui.label("Erasing flash -- this can take up to a few minutes...");
                ui.add(egui::widgets::ProgressBar::new(0.0).show_percentage());
            }
            UploadStage::Programming { blocks_sent, blocks_total } => {
                let frac = if *blocks_total > 0 { *blocks_sent as f32 / *blocks_total as f32 } else { 0.0 };
                ui.label(format!("Programming: block {blocks_sent} of {blocks_total}"));
                ui.add(egui::widgets::ProgressBar::new(frac).show_percentage());
            }
            UploadStage::Done => {
                ui.colored_label(OK_COLOR, "Update complete.");
                if matches!(self.target, Target::RawEthernet { .. }) {
                    ui.colored_label(
                        WARNING_COLOR,
                        "Now switch the radio out of bootloader mode and power-cycle it before \
                         normal use.",
                    );
                }
            }
            UploadStage::Failed { message, needs_power_cycle } => {
                ui.colored_label(ERROR_COLOR, "Update failed.");
                ui.label(message);
                if *needs_power_cycle {
                    ui.colored_label(ERROR_COLOR, "Power-cycle the radio before trying again.");
                }
            }
        }

        let finished = matches!(stage, UploadStage::Done | UploadStage::Failed { .. });
        if !finished {
            if ui.button("Cancel").clicked() {
                handle.cancel();
            }
            // Keep repainting while an upload is in flight -- nothing else
            // in this window's own input triggers a repaint on its own
            // while the user isn't interacting with it.
            ui.ctx().request_repaint_after(Duration::from_millis(200));
        } else if ui.button("Close").clicked() {
            self.upload = None;
        }
    }

    fn start_upload(&mut self, ctx: &egui::Context) {
        let Some(path) = self.firmware_path.clone() else { return };
        let firmware = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) => {
                self.firmware_read_error = Some(format!("failed to read firmware file: {e}"));
                return;
            }
        };
        self.firmware_read_error = None;
        match &self.target {
            Target::RawEthernet { interfaces, selected } => {
                if let Some(i) = selected {
                    let iface = interfaces[*i].clone();
                    self.upload = Some(bootloader::spawn_raw_upload(iface, firmware));
                }
            }
            Target::InApp { radio_ip, .. } => {
                self.upload = Some(bootloader::spawn_inapp_upload(*radio_ip, firmware));
            }
        }
        ctx.request_repaint();
    }
}
