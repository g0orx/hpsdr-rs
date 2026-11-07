/*
    A tiny web server for hpsdr-remote's web client (crates/hpsdr-remote),
    so a phone or tablet only needs a browser: Settings -> Network -> Web
    client starts it, and https://<this machine>:<port> opens the client.

    It serves just the client's three files, compiled into hpsdr-rs when
    the web client was built first (crates/hpsdr-remote/build-web.sh; see
    build.rs) -- otherwise a page saying so. Browsers are told not to cache
    them, so a rebuilt client is what a reload gets.

    The client's connection comes back to this same port, at /tci, and is
    passed straight through to the TCI server (docs/remote-protocol.md):
    one address and one certificate for a browser to accept, and the TCI
    port itself can stay private (bound to 127.0.0.1) when this one is
    reachable from the internet. An optional password guards /tci.

    HTTPS by default, because browsers only let a page use the microphone
    over HTTPS (or from localhost) -- with a self-signed certificate made
    on first use and kept with the settings (web-cert.pem / web-key.pem,
    which can be replaced with a real one). A browser warns about it once.
*/

use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::{ServerConfig, ServerConnection, StreamOwned};

pub const DEFAULT_ADDR: &str = "0.0.0.0:8080";
/// Where the client's connection goes: passed through to the TCI server.
const TCI_PATH: &str = "/tci";
const CERT_FILE: &str = "web-cert.pem";
const KEY_FILE: &str = "web-key.pem";

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

/// What each connection needs.
struct Context {
    tls: Option<Arc<ServerConfig>>,
    /// Required on /tci if not empty.
    password: String,
    /// The TCI server's address (Settings -> Network), kept current by
    /// main.rs -- where /tci goes.
    tci_addr: Arc<Mutex<String>>,
}

pub struct WebServer {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl WebServer {
    /// Starts serving on `addr`: HTTPS with `tls`, else plain HTTP.
    pub fn start(
        addr: &str,
        tls: Option<Arc<ServerConfig>>,
        password: String,
        tci_addr: Arc<Mutex<String>>,
    ) -> io::Result<Self> {
        let listener = TcpListener::bind(addr)?;
        listener.set_nonblocking(true)?;
        let context = Arc::new(Context { tls, password, tci_addr });
        let stop = Arc::new(AtomicBool::new(false));
        let accept_stop = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            while !accept_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        // One thread a connection: a slow phone fetching
                        // the 5 MB module, or a client's connection held
                        // open, doesn't hold up anyone else.
                        let context = Arc::clone(&context);
                        thread::spawn(move || serve(stream, &context));
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(100)),
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

/// The HTTPS setup from the certificate and key in `dir`, making a
/// self-signed pair first if there isn't one. `names` (this machine's
/// addresses) go into a new certificate.
pub fn tls_config(dir: &Path, names: &[String]) -> io::Result<Arc<ServerConfig>> {
    let (cert_path, key_path) = (dir.join(CERT_FILE), dir.join(KEY_FILE));
    if !cert_path.exists() || !key_path.exists() {
        let mut all = vec!["localhost".to_string(), "hpsdr-rs".to_string()];
        all.extend(names.iter().cloned());
        let made = rcgen::generate_simple_self_signed(all).map_err(io::Error::other)?;
        std::fs::create_dir_all(dir)?;
        std::fs::write(&cert_path, made.cert.pem())?;
        std::fs::write(&key_path, made.signing_key.serialize_pem())?;
    }
    let certs = CertificateDer::pem_file_iter(&cert_path)
        .and_then(|i| i.collect::<Result<Vec<_>, _>>())
        .map_err(|e| io::Error::other(format!("{}: {e}", cert_path.display())))?;
    let key = PrivateKeyDer::from_pem_file(&key_path)
        .map_err(|e| io::Error::other(format!("{}: {e}", key_path.display())))?;
    let config = ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(io::Error::other)?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(io::Error::other)?;
    Ok(Arc::new(config))
}

/// The port of an address like "0.0.0.0:8080" (8080 if it has none).
pub fn port_of(addr: &str) -> u16 {
    addr.rsplit(':').next().and_then(|p| p.parse().ok()).unwrap_or(8080)
}

/// A connection, plain or TLS, whose reads can be made to time out at once
/// (`poll`) while passing a client's connection through.
trait Socket: Read + Write {
    fn set_poll(&self, poll: bool);
}

impl Socket for TcpStream {
    fn set_poll(&self, poll: bool) {
        let _ = self.set_read_timeout(Some(if poll { POLL } else { REQUEST_TIMEOUT }));
    }
}

impl Socket for StreamOwned<ServerConnection, TcpStream> {
    fn set_poll(&self, poll: bool) {
        self.sock.set_poll(poll);
    }
}

const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// How long each side is waited for while passing a connection through.
const POLL: Duration = Duration::from_millis(2);

fn serve(stream: TcpStream, context: &Context) {
    // Accepted from a non-blocking listener: on Windows it inherits that.
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_nodelay(true);
    stream.set_poll(false);
    let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));
    match &context.tls {
        Some(config) => {
            let Ok(conn) = ServerConnection::new(Arc::clone(config)) else { return };
            let mut tls = StreamOwned::new(conn, stream);
            handle(&mut tls, context);
            tls.conn.send_close_notify();
            let _ = tls.flush();
        }
        None => {
            let mut stream = stream;
            handle(&mut stream, context);
        }
    }
}

fn handle(s: &mut impl Socket, context: &Context) {
    let mut request = Vec::new();
    let mut buf = [0u8; 4096];
    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
        if request.len() > 16384 {
            return;
        }
        match s.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => request.extend_from_slice(&buf[..n]),
        }
    }
    let text = String::from_utf8_lossy(&request).to_string();
    let mut parts = text.lines().next().unwrap_or("").split_whitespace();
    let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    if path == TCI_PATH {
        pass_through(s, &request, query, &text, context);
        return;
    }
    let _ = s.write_all(&respond(method, path));
    let _ = s.flush();
}

/// /tci: checks the password, then hands the whole connection -- the
/// WebSocket handshake included -- to the TCI server, unchanged both ways.
fn pass_through(s: &mut impl Socket, request: &[u8], query: &str, head: &str, context: &Context) {
    let upgrade = head.lines().any(|l| {
        let l = l.to_ascii_lowercase();
        l.starts_with("upgrade:") && l.contains("websocket")
    });
    if !upgrade {
        let _ = s.write_all(&message(400, "Bad Request", "text/plain", b"WebSocket only", false));
        return;
    }
    if !context.password.is_empty() && query_value(query, "password").as_deref() != Some(context.password.as_str()) {
        let _ = s.write_all(&message(401, "Unauthorized", "text/plain", b"wrong password", false));
        let _ = s.flush();
        return;
    }
    let target = tci_target(&context.tci_addr.lock().unwrap());
    let Some(mut tci) = target.and_then(|t| TcpStream::connect_timeout(&t, Duration::from_secs(2)).ok()) else {
        let _ = s.write_all(&message(502, "Bad Gateway", "text/plain", b"the TCI server isn't running", false));
        let _ = s.flush();
        return;
    };
    let _ = tci.set_nodelay(true);
    let _ = tci.set_read_timeout(Some(POLL));
    if tci.write_all(request).is_err() {
        return;
    }
    s.set_poll(true);
    let mut buf = vec![0u8; 16384];
    loop {
        match s.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if tci.write_all(&buf[..n]).is_err() {
                    break;
                }
            }
            Err(e) if timed_out(&e) => {}
            Err(_) => break,
        }
        match tci.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if s.write_all(&buf[..n]).and_then(|_| s.flush()).is_err() {
                    break;
                }
            }
            Err(e) if timed_out(&e) => {}
            Err(_) => break,
        }
    }
}

/// A read that found nothing within its timeout (WouldBlock on Unix,
/// TimedOut on Windows).
fn timed_out(e: &io::Error) -> bool {
    matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut)
}

/// Where to reach the TCI server at `addr`: this machine if it listens on
/// every address.
fn tci_target(addr: &str) -> Option<SocketAddr> {
    let mut a: SocketAddr = addr.parse().ok()?;
    if a.ip().is_unspecified() {
        a.set_ip(IpAddr::V4(Ipv4Addr::LOCALHOST));
    }
    Some(a)
}

/// The value of `name` in a URL query, %-decoded.
fn query_value(query: &str, name: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == name).then(|| percent_decode(v))
    })
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => match (hex(b[i + 1]), hex(b[i + 2])) {
                (Some(h), Some(l)) => {
                    out.push(h << 4 | l);
                    i += 3;
                    continue;
                }
                _ => out.push(b'%'),
            },
            b'+' => out.push(b' '),
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

/// The whole HTTP response for `method path`.
fn respond(method: &str, path: &str) -> Vec<u8> {
    if method != "GET" && method != "HEAD" {
        return message(405, "Method Not Allowed", "text/plain", b"GET only", false);
    }
    let head = method == "HEAD";
    match (path, HAS_CLIENT) {
        ("/" | "/index.html", true) => message(200, "OK", "text/html; charset=utf-8", page().as_bytes(), head),
        ("/" | "/index.html", false) => message(200, "OK", "text/html; charset=utf-8", NOT_BUILT.as_bytes(), head),
        #[cfg(remote_web)]
        ("/pkg/hpsdr-remote.js", _) => message(200, "OK", "text/javascript", SCRIPT, head),
        #[cfg(remote_web)]
        ("/pkg/hpsdr-remote_bg.wasm", _) => message(200, "OK", "application/wasm", WASM, head),
        _ => message(404, "Not Found", "text/plain", b"not found", head),
    }
}

/// index.html, marked as served by hpsdr-rs: the client then connects
/// back to this server's /tci.
fn page() -> String {
    #[cfg(remote_web)]
    {
        INDEX.replace("__HPSDR_SERVED__", TCI_PATH)
    }
    #[cfg(not(remote_web))]
    {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn body(r: &[u8]) -> String {
        let text = String::from_utf8_lossy(r);
        text.split_once("\r\n\r\n").map(|(_, b)| b.to_string()).unwrap_or_default()
    }

    fn free_port() -> u16 {
        TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
    }

    #[test]
    fn serves_the_page_and_refuses_the_rest() {
        let page = respond("GET", "/");
        assert!(String::from_utf8_lossy(&page).starts_with("HTTP/1.1 200 OK"));
        if HAS_CLIENT {
            let b = body(&page);
            assert!(b.contains("content=\"/tci\"") && !b.contains("__HPSDR_SERVED__"));
            let wasm = respond("GET", "/pkg/hpsdr-remote_bg.wasm");
            assert!(String::from_utf8_lossy(&wasm[..120]).contains("application/wasm"));
        } else {
            assert!(body(&page).contains("isn't included"));
        }
        assert!(String::from_utf8_lossy(&respond("GET", "/../etc/passwd")).starts_with("HTTP/1.1 404"));
        assert!(String::from_utf8_lossy(&respond("POST", "/")).starts_with("HTTP/1.1 405"));
        assert!(body(&respond("HEAD", "/")).is_empty());
        assert_eq!(query_value("a=1&password=p%40ss+word%21", "password").as_deref(), Some("p@ss word!"));
        assert_eq!(query_value("password=50%", "password").as_deref(), Some("50%"));
        assert_eq!(tci_target("0.0.0.0:50001"), "127.0.0.1:50001".parse().ok());
    }

    /// /tci reaches the TCI server -- a WebSocket echo server here --
    /// with the right password, and not without it.
    #[test]
    fn passes_a_websocket_through_with_the_password() {
        let tci_port = free_port();
        let tci = TcpListener::bind(("127.0.0.1", tci_port)).unwrap();
        thread::spawn(move || {
            for stream in tci.incoming().flatten() {
                thread::spawn(move || {
                    let mut ws = tungstenite::accept(stream).unwrap();
                    while let Ok(m) = ws.read() {
                        if m.is_text() && ws.send(m).is_err() {
                            break;
                        }
                    }
                });
            }
        });
        let port = free_port();
        let _server = WebServer::start(
            &format!("127.0.0.1:{port}"),
            None,
            "s3cret!".into(),
            Arc::new(Mutex::new(format!("0.0.0.0:{tci_port}"))),
        )
        .unwrap();
        assert!(tungstenite::connect(format!("ws://127.0.0.1:{port}/tci?password=wrong")).is_err());
        let (mut ws, _) = tungstenite::connect(format!("ws://127.0.0.1:{port}/tci?password=s3cret%21")).unwrap();
        ws.send(tungstenite::Message::Text("vfo:0,0;".into())).unwrap();
        assert_eq!(ws.read().unwrap().into_text().unwrap().as_str(), "vfo:0,0;");
    }

    /// HTTPS with a certificate made on first use, checked against itself.
    #[test]
    fn serves_https_with_its_own_certificate() {
        let dir = std::env::temp_dir().join(format!("hpsdr-rs-web-tls-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let config = tls_config(&dir, &["192.168.1.2".into()]).unwrap();
        assert!(dir.join(CERT_FILE).exists() && dir.join(KEY_FILE).exists());
        let port = free_port();
        let _server =
            WebServer::start(&format!("127.0.0.1:{port}"), Some(config), String::new(), Arc::new(Mutex::new(String::new())))
                .unwrap();

        let mut roots = rustls::RootCertStore::empty();
        for c in CertificateDer::pem_file_iter(dir.join(CERT_FILE)).unwrap() {
            roots.add(c.unwrap()).unwrap();
        }
        let client = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let conn = rustls::ClientConnection::new(Arc::new(client), "localhost".try_into().unwrap()).unwrap();
        let mut tls = StreamOwned::new(conn, TcpStream::connect(("127.0.0.1", port)).unwrap());
        tls.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
        let mut reply = Vec::new();
        let _ = tls.read_to_end(&mut reply);
        assert!(String::from_utf8_lossy(&reply).starts_with("HTTP/1.1 200 OK"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
