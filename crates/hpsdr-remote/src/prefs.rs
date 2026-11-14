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

#[cfg(test)]
mod tests {
    use super::*;

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
