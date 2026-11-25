/*
    What hpsdr-remote remembers between runs: the last few servers it
    connected to, so they needn't be typed again. Natively in a text file
    with the other programs' settings (~/.config/hpsdr-remote/ on Linux),
    one address a line; in a browser, in that site's localStorage.
    Passwords aren't kept. The front-panel controller's settings
    (controller.json) are kept in the same directory.
*/

/// How many servers are remembered.
pub const RECENT_MAX: usize = 3;

/// `url` to the front of `list`, once, keeping at most RECENT_MAX.
pub fn remember(list: &mut Vec<String>, url: &str) {
    let url = url.trim();
    if url.is_empty() {
        return;
    }
    list.retain(|u| u != url);
    list.insert(0, url.to_string());
    list.truncate(RECENT_MAX);
}

fn parse(text: &str) -> Vec<String> {
    let mut list = Vec::new();
    for line in text.lines().rev() {
        remember(&mut list, line);
    }
    list
}

/// A settings file of hpsdr-remote's: natively in its own settings
/// directory; none in a browser.
#[cfg(not(target_arch = "wasm32"))]
pub fn settings_path(name: &str) -> Option<std::path::PathBuf> {
    use std::path::PathBuf;
    let mut path = if cfg!(target_os = "windows") {
        PathBuf::from(std::env::var_os("APPDATA")?)
    } else if cfg!(target_os = "macos") {
        PathBuf::from(std::env::var_os("HOME")?).join("Library").join("Application Support")
    } else {
        match std::env::var_os("XDG_CONFIG_HOME").filter(|d| !d.is_empty()) {
            Some(d) => PathBuf::from(d),
            None => PathBuf::from(std::env::var_os("HOME")?).join(".config"),
        }
    };
    path.push("hpsdr-remote");
    path.push(name);
    Some(path)
}

#[cfg(target_arch = "wasm32")]
pub fn settings_path(_name: &str) -> Option<std::path::PathBuf> {
    None
}

#[cfg(not(target_arch = "wasm32"))]
fn recent_path() -> Option<std::path::PathBuf> {
    settings_path("recent-servers.txt")
}

#[cfg(not(target_arch = "wasm32"))]
pub fn load_recent() -> Vec<String> {
    recent_path().and_then(|p| std::fs::read_to_string(p).ok()).map(|t| parse(&t)).unwrap_or_default()
}

#[cfg(not(target_arch = "wasm32"))]
pub fn save_recent(list: &[String]) {
    let Some(path) = recent_path() else { return };
    let saved = path.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|_| std::fs::write(&path, list.join("\n") + "\n"));
    if let Err(e) = saved {
        eprintln!("hpsdr-remote: couldn't save {}: {e}", path.display());
    }
}

#[cfg(target_arch = "wasm32")]
const STORAGE_KEY: &str = "hpsdr-remote-recent-servers";

#[cfg(target_arch = "wasm32")]
fn storage() -> Option<web_sys::Storage> {
    // Blocked storage (a private window, say) throws: then nothing's kept.
    web_sys::window()?.local_storage().ok().flatten()
}

#[cfg(target_arch = "wasm32")]
pub fn load_recent() -> Vec<String> {
    storage().and_then(|s| s.get_item(STORAGE_KEY).ok().flatten()).map(|t| parse(&t)).unwrap_or_default()
}

#[cfg(target_arch = "wasm32")]
pub fn save_recent(list: &[String]) {
    if let Some(s) = storage() {
        let _ = s.set_item(STORAGE_KEY, &list.join("\n"));
    }
}

/// How the spectrum and waterfall are shown: each on or off, the
/// spectrum's share of the height they have when both are, and the share
/// one of them takes when it's alone. Also the audio Mute, kept with them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Display {
    pub spectrum: bool,
    pub waterfall: bool,
    pub spectrum_share: f32,
    pub alone_share: f32,
    pub muted: bool,
}

pub const SHARE_RANGE: (f32, f32) = (0.1, 0.9);
pub const ALONE_RANGE: (f32, f32) = (0.1, 1.0);

impl Default for Display {
    fn default() -> Self {
        Display { spectrum: true, waterfall: true, spectrum_share: 0.45, alone_share: 1.0, muted: false }
    }
}

impl Display {
    /// "spectrum=1 waterfall=1 share=0.45", in any order; anything
    /// missing or unreadable is the default.
    fn parse(text: &str) -> Display {
        let mut d = Display::default();
        for item in text.split_whitespace() {
            match item.split_once('=') {
                Some(("spectrum", v)) => d.spectrum = v != "0",
                Some(("waterfall", v)) => d.waterfall = v != "0",
                Some(("muted", v)) => d.muted = v == "1",
                Some(("share", v)) => {
                    if let Ok(s) = v.parse::<f32>() {
                        d.spectrum_share = s.clamp(SHARE_RANGE.0, SHARE_RANGE.1);
                    }
                }
                Some(("alone", v)) => {
                    if let Ok(s) = v.parse::<f32>() {
                        d.alone_share = s.clamp(ALONE_RANGE.0, ALONE_RANGE.1);
                    }
                }
                _ => {}
            }
        }
        d
    }

    fn text(&self) -> String {
        format!(
            "spectrum={} waterfall={} share={:.3} alone={:.3} muted={}",
            self.spectrum as u8, self.waterfall as u8, self.spectrum_share, self.alone_share, self.muted as u8
        )
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub fn load_display() -> Display {
    settings_path("display.txt").and_then(|p| std::fs::read_to_string(p).ok()).map_or_else(Display::default, |t| Display::parse(&t))
}

#[cfg(not(target_arch = "wasm32"))]
pub fn save_display(d: &Display) {
    let Some(path) = settings_path("display.txt") else { return };
    let saved = path.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|_| std::fs::write(&path, d.text() + "\n"));
    if let Err(e) = saved {
        eprintln!("hpsdr-remote: couldn't save {}: {e}", path.display());
    }
}

#[cfg(target_arch = "wasm32")]
const DISPLAY_KEY: &str = "hpsdr-remote-display";

#[cfg(target_arch = "wasm32")]
pub fn load_display() -> Display {
    storage().and_then(|s| s.get_item(DISPLAY_KEY).ok().flatten()).map_or_else(Display::default, |t| Display::parse(&t))
}

#[cfg(target_arch = "wasm32")]
pub fn save_display(d: &Display) {
    if let Some(s) = storage() {
        let _ = s.set_item(DISPLAY_KEY, &d.text());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_round_trips_and_defaults() {
        let d = Display { spectrum: false, waterfall: true, spectrum_share: 0.3, alone_share: 0.5, muted: true };
        assert_eq!(Display::parse(&d.text()), d);
        assert_eq!(Display::parse(""), Display::default());
        assert_eq!(Display::parse("share=5 waterfall=0").spectrum_share, SHARE_RANGE.1);
        assert!(!Display::parse("share=5 waterfall=0").waterfall);
    }

    #[test]
    fn newest_first_once_and_at_most_three() {
        let mut list = Vec::new();
        for u in ["a", "b", "c", "b", "d", " ", "a"] {
            remember(&mut list, u);
        }
        assert_eq!(list, ["a", "d", "b"]);
        assert_eq!(parse("x\n\ny\nx\nz\nw\n"), ["x", "y", "z"]);
    }
}
