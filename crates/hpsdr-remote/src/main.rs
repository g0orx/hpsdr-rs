/*
    hpsdr-remote: a thin client for hpsdr-rs, talking TCI plus hpsdr-rs's
    remote extensions (docs/remote-protocol.md) -- the radio stays with
    hpsdr-rs, and only control, a spectrum row and compressed audio cross
    the network. Builds natively, and to WebAssembly for a browser (see
    crates/hpsdr-remote/README.md).
*/

mod app;
mod audio;
mod link;
mod mic;

#[cfg(not(target_arch = "wasm32"))]
fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1000.0, 700.0]).with_title("hpsdr-remote"),
        ..Default::default()
    };
    eframe::run_native("hpsdr-remote", options, Box::new(|cc| Ok(Box::new(app::App::new(cc)))))
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
