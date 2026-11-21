/*
    In a browser: when the page's WebGL context is lost, reload the page
    and connect again to the same server.

    REAL REPORT 2026-11-21, Radeon 680M, Chrome and Chromium on Ubuntu
    26.10: the browser's GPU process faults, the browser restarts it, and
    the page's WebGL context is gone. eframe's glow painter doesn't
    recover from that, so the display froze while the audio carried on.
    A reload makes a new context; the server and password are kept in
    sessionStorage (this tab only) across it, just long enough for the
    new page to connect, and then removed.

    Not more than MAX_RELOADS in RELOAD_WINDOW_MS, so a browser that loses
    the context again straight away isn't reloaded for ever.
*/

#[cfg(target_arch = "wasm32")]
mod web {
    use std::cell::RefCell;

    use wasm_bindgen::JsCast;
    use wasm_bindgen::closure::Closure;

    const RECONNECT_KEY: &str = "hpsdr-remote-reconnect";
    const RELOADS_KEY: &str = "hpsdr-remote-reloads";
    const MAX_RELOADS: usize = 3;
    const RELOAD_WINDOW_MS: f64 = 60_000.0;

    thread_local! {
        /// The server and password while connected, for the reload.
        static CONNECTED: RefCell<Option<(String, String)>> = const { RefCell::new(None) };
    }

    fn storage() -> Option<web_sys::Storage> {
        web_sys::window()?.session_storage().ok().flatten()
    }

    pub fn set_connected(server: Option<(&str, &str)>) {
        CONNECTED.with(|c| *c.borrow_mut() = server.map(|(u, p)| (u.to_string(), p.to_string())));
    }

    /// The server and password to connect to again, if this page was
    /// loaded by a reload after a lost context.
    pub fn take_reconnect() -> Option<(String, String)> {
        let s = storage()?;
        let text = s.get_item(RECONNECT_KEY).ok().flatten()?;
        let _ = s.remove_item(RECONNECT_KEY);
        let (url, password) = text.split_once('\n')?;
        Some((url.to_string(), password.to_string()))
    }

    /// Too many reloads lately? Otherwise counts this one.
    fn allow_reload(s: &web_sys::Storage) -> bool {
        let now = js_sys_now();
        let mut times: Vec<f64> = s
            .get_item(RELOADS_KEY)
            .ok()
            .flatten()
            .map(|t| t.split(',').filter_map(|v| v.parse().ok()).collect())
            .unwrap_or_default();
        times.retain(|&t| now - t < RELOAD_WINDOW_MS);
        if times.len() >= MAX_RELOADS {
            return false;
        }
        times.push(now);
        let text: Vec<String> = times.iter().map(|t| format!("{t:.0}")).collect();
        let _ = s.set_item(RELOADS_KEY, &text.join(","));
        true
    }

    fn js_sys_now() -> f64 {
        web_sys::window().and_then(|w| w.performance()).map(|p| p.time_origin() + p.now()).unwrap_or(0.0)
    }

    pub fn install(canvas: &web_sys::HtmlCanvasElement) {
        let on_lost = Closure::<dyn FnMut(web_sys::Event)>::new(|_event: web_sys::Event| {
            let Some(window) = web_sys::window() else { return };
            let Some(s) = storage() else {
                log::error!("hpsdr-remote: WebGL context lost; no sessionStorage, not reloading");
                return;
            };
            if !allow_reload(&s) {
                log::error!("hpsdr-remote: WebGL context lost again; too many reloads, reload the page by hand");
                return;
            }
            if let Some((url, password)) = CONNECTED.with(|c| c.borrow().clone()) {
                let _ = s.set_item(RECONNECT_KEY, &format!("{url}\n{password}"));
            }
            log::warn!("hpsdr-remote: WebGL context lost; reloading the page");
            let _ = window.location().reload();
        });
        let _ = canvas.add_event_listener_with_callback("webglcontextlost", on_lost.as_ref().unchecked_ref());
        // For the life of the page.
        on_lost.forget();
    }
}

#[cfg(target_arch = "wasm32")]
pub use web::{install, set_connected, take_reconnect};

#[cfg(not(target_arch = "wasm32"))]
pub fn set_connected(_server: Option<(&str, &str)>) {}

#[cfg(not(target_arch = "wasm32"))]
pub fn take_reconnect() -> Option<(String, String)> {
    None
}
