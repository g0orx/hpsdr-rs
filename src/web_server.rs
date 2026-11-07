/*
    A tiny web server for hpsdr-remote's web client (crates/hpsdr-remote),
    so a phone or tablet only needs a browser: Settings -> Network -> Web
    client starts it, and http://<this machine>:<port> opens the client,
    which then talks to the TCI server (docs/remote-protocol.md).

    It serves just the client's three files, compiled into hpsdr-rs when
    the web client was built first (crates/hpsdr-remote/build-web.sh; see
    build.rs) -- otherwise a page saying so. Browsers are told not to cache
    them, so a rebuilt client is what a reload gets. The page learns the TCI
    port from here, so the address it suggests is right even if that port
    was changed. No authentication, like TCI itself: LAN, or a VPN/tunnel.
*/

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

pub const DEFAULT_ADDR: &str = "0.0.0.0:8080";

#[cfg(remote_web)]
const INDEX: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/crates/hpsdr-remote/web/index.html"));
#[cfg(remote_web)]
const SCRIPT: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/crates/hpsdr-remote/web/pkg/hpsdr-remote.js"));
#[cfg(remote_web)]
const WASM: &[u8] =
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/crates/hpsdr-remote/web/pkg/hpsdr-remote_bg.wasm"));

/// Whether this build has the web client in it.
pub const HAS_CLIENT: bool = cfg!(remote_web);

const NOT_BUILT: &str = "<!doctype html><html><head><meta charset=\"utf-8\"><title>hpsdr-remote</title></head>\
<body style=\"font-family:sans-serif;background:#1b1b1b;color:#ddd;padding:2em\">\
<h2>hpsdr-remote isn't included in this build of hpsdr-rs</h2>\
<p>Build the web client first (crates/hpsdr-remote/build-web.sh), then rebuild hpsdr-rs.</p></body></html>";

pub struct WebServer {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl WebServer {
    /// Starts serving on `addr`. `tci_port` is read on each page load, for
    /// the address the page suggests.
    pub fn start(addr: &str, tci_port: Arc<AtomicU16>) -> std::io::Result<Self> {
        let listener = TcpListener::bind(addr)?;
        listener.set_nonblocking(true)?;
        let stop = Arc::new(AtomicBool::new(false));
        let accept_stop = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            while !accept_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let port = tci_port.load(Ordering::Relaxed);
                        // One thread a request: a slow phone fetching the
                        // 5 MB module doesn't hold up anyone else.
                        thread::spawn(move || serve(stream, port));
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(100)),
                    Err(_) => break,
                }
            }
        });
        Ok(Self { stop, thread: Some(thread) })
    }
}

impl Drop for WebServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn serve(mut stream: TcpStream, tci_port: u16) {
    // Accepted from a non-blocking listener: on Windows it inherits that.
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));
    let mut request = Vec::new();
    let mut buf = [0u8; 1024];
    while !request.windows(4).any(|w| w == b"\r\n\r\n") && request.len() < 8192 {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => request.extend_from_slice(&buf[..n]),
        }
    }
    let line = String::from_utf8_lossy(&request);
    let mut parts = line.lines().next().unwrap_or("").split_whitespace();
    let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let response = respond(method, target, tci_port);
    let _ = stream.write_all(&response);
}

/// The whole HTTP response for `method target`.
fn respond(method: &str, target: &str, tci_port: u16) -> Vec<u8> {
    if method != "GET" && method != "HEAD" {
        return message(405, "Method Not Allowed", "text/plain", b"GET only", method == "HEAD");
    }
    // The page loads its files with ?v=<time> -- see index.html.
    let path = target.split('?').next().unwrap_or("/");
    let head = method == "HEAD";
    match (path, HAS_CLIENT) {
        ("/" | "/index.html", true) => message(200, "OK", "text/html; charset=utf-8", page(tci_port).as_bytes(), head),
        ("/" | "/index.html", false) => message(200, "OK", "text/html; charset=utf-8", NOT_BUILT.as_bytes(), head),
        #[cfg(remote_web)]
        ("/pkg/hpsdr-remote.js", _) => message(200, "OK", "text/javascript", SCRIPT, head),
        #[cfg(remote_web)]
        ("/pkg/hpsdr-remote_bg.wasm", _) => message(200, "OK", "application/wasm", WASM, head),
        _ => message(404, "Not Found", "text/plain", b"not found", head),
    }
}

/// index.html with the TCI port filled in.
fn page(tci_port: u16) -> String {
    #[cfg(remote_web)]
    {
        INDEX.replace("__TCI_PORT__", &tci_port.to_string())
    }
    #[cfg(not(remote_web))]
    {
        let _ = tci_port;
        String::new()
    }
}

fn message(code: u16, reason: &str, content_type: &str, body: &[u8], head: bool) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    if !head {
        out.extend_from_slice(body);
    }
    out
}

/// The port of a TCI address like "0.0.0.0:50001" (50001 if it has none).
pub fn port_of(addr: &str) -> u16 {
    addr.rsplit(':').next().and_then(|p| p.parse().ok()).unwrap_or(50001)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(r: &[u8]) -> String {
        let text = String::from_utf8_lossy(r);
        text.split_once("\r\n\r\n").map(|(_, b)| b.to_string()).unwrap_or_default()
    }

    #[test]
    fn serves_the_page_and_refuses_the_rest() {
        let page = respond("GET", "/", 50002);
        let head = String::from_utf8_lossy(&page[..60.min(page.len())]).to_string();
        assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
        if HAS_CLIENT {
            let b = body(&page);
            assert!(b.contains("50002") && !b.contains("__TCI_PORT__"));
            let wasm = respond("GET", "/pkg/hpsdr-remote_bg.wasm?v=123", 50002);
            assert!(String::from_utf8_lossy(&wasm[..120]).contains("application/wasm"));
        } else {
            assert!(body(&page).contains("isn't included"));
        }
        assert!(String::from_utf8_lossy(&respond("GET", "/../etc/passwd", 0)).starts_with("HTTP/1.1 404"));
        assert!(String::from_utf8_lossy(&respond("POST", "/", 0)).starts_with("HTTP/1.1 405"));
        assert!(body(&respond("HEAD", "/", 0)).is_empty());
        assert_eq!((port_of("0.0.0.0:50001"), port_of("127.0.0.1:4000"), port_of("bad")), (50001, 4000, 50001));
    }

    /// A real request over a socket.
    #[test]
    fn answers_over_the_network() {
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let _server = WebServer::start(&format!("127.0.0.1:{port}"), Arc::new(AtomicU16::new(50001))).unwrap();
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.write_all(b"GET /index.html HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        let mut reply = String::new();
        s.read_to_string(&mut reply).unwrap();
        assert!(reply.starts_with("HTTP/1.1 200 OK") && reply.contains("no-store"), "{reply}");
    }
}
