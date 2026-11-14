/*
    hpsdr-remote: a thin client for hpsdr-rs, talking TCI plus hpsdr-rs's
    remote extensions (docs/remote-protocol.md) -- the radio stays with
    hpsdr-rs, and only control, a spectrum row and compressed audio cross
    the network. Builds natively, and to WebAssembly for a browser (see
    crates/hpsdr-remote/README.md).
*/

mod app;
mod audio;
mod keyer;
mod link;
mod mic;
mod prefs;

#[cfg(not(target_arch = "wasm32"))]
fn main() -> eframe::Result<()> {
    // --fullscreen: fill the screen, as hpsdr-rs's own, for a Raspberry
    // Pi's 7" display; F11 toggles it.
    let fullscreen = std::env::args().skip(1).any(|a| a == "--fullscreen");
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1000.0, 700.0])
            .with_min_inner_size([320.0, 400.0])
            .with_fullscreen(fullscreen)
            .with_title("hpsdr-remote"),
        // Always glow (OpenGL): if a build also brings in eframe's wgpu (as
        // cargo deb building it beside hpsdr-rs did), eframe would choose
        // wgpu, which on a Raspberry Pi 4 fails to start (REAL REPORT
        // 2026-11-14: "LimitsExceeded ... max_color_attachments, requested
        // 8, allowed 4").
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    eframe::run_native(
        "hpsdr-remote",
        options,
        Box::new(move |cc| {
            let mut app = app::App::new(cc);
            app.fullscreen = fullscreen;
            Ok(Box::new(app))
        }),
    )
}

#[cfg(target_arch = "wasm32")]
fn main() {
    use wasm_bindgen::JsCast;
    wasm_bindgen_futures::spawn_local(async {
        let canvas = web_sys::window()
            .and_then(|w| w.document())
            .and_then(|d| d.get_element_by_id("hpsdr_remote"))
            .and_then(|e| e.dyn_into::<web_sys::HtmlCanvasElement>().ok())
            .expect("no canvas with id hpsdr_remote");
        let result = eframe::WebRunner::new()
            .start(canvas, eframe::WebOptions::default(), Box::new(|cc| Ok(Box::new(app::App::new(cc)))))
            .await;
        if let Err(e) = result {
            log::error!("hpsdr-remote failed to start: {e:?}");
        }
    });
}
