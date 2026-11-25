//! piHPSDR-style front-panel controllers on a Raspberry Pi: the hardware,
//! settings and Settings page are in crates/hpsdr-controller, shared with
//! hpsdr-remote. hpsdr-rs keeps controller.json in its settings directory,
//! and applies each control's action through the same code as a MIDI
//! control (main.rs's dispatch_controller_event).

pub use hpsdr_controller::*;

/// controller.json in hpsdr-rs's settings directory.
pub fn config_path() -> Option<std::path::PathBuf> {
    let mut path = crate::config::settings_dir()?;
    path.push("controller.json");
    Some(path)
}
