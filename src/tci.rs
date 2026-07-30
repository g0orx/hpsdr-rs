/*
    Minimal implementation of the TCI (Transceiver Control Interface)
    protocol -- an open WebSocket-based control protocol originally from
    Expert Electronics (ExpertSDR2/3), also supported by Thetis/
    OpenHPSDR and digital-mode software like JTDX. Listens on
    127.0.0.1:40001, TCI's standard default port.

    CONFIDENCE LEVEL, please read before trusting this blindly:
    - Command syntax (`name:arg1,arg2,...;`, reserved chars :,;) and the
      default port are confirmed from the official protocol README
      (github.com/ExpertSDR3/TCI).
    - The specific commands implemented here (vfo, modulation, trx) and
      their argument order are confirmed against JTDX's actual working
      TCI client implementation (TCITransceiver.cpp), not guessed.
    - The *initial handshake sequence* sent on connect (protocol name,
      then vfo/modulation/trx state) is my best-effort reconstruction of
      "server pushes current state on connect" per the README -- the
      full official handshake may include additional fields (device
      capabilities, channel counts, VFO limits, etc.) that aren't
      implemented here. If a TCI client fails to fully recognize this
      as a valid server, this is the first thing to check.
    - TCI mode-string spellings (LSB/USB/CW/etc.) are assumed similar to
      the equally-unverified-for-TCI-specifically ones used in rigctl.rs
      -- not confirmed against a TCI reference for exact spelling.
    - tungstenite's exact API surface (accept(), get_ref(), etc.) is
      from general knowledge of the crate, not verified against 0.29
      specifically -- same caveat as every other external-crate API in
      this project.

    trx (PTT) flips RadioSession's mox flag -- same one the on-screen
    PTT button and rigctl's set_ptt use. See rigctl.rs's module note on
    the "armed but keyed with silence" gap if TX isn't enabled in
    Settings -> TX when a client sends trx:0,true;.

    Also unlike rigctl.rs: TCI supports IQ/audio streaming to clients
    (for skimmers, recording, etc.) as a separate concern from this
    control channel. Not implemented here -- this is control-only.
*/

use crate::spectrum::{DemodParams, Mode};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use tungstenite::Message;

pub const DEFAULT_ADDR: &str = "127.0.0.1:40001";
const PROTOCOL_NAME: &str = "protocol:hpsdr-rs;";

/// A swappable reference to "whichever DemodParams is current right
/// now" -- see TciServer::set_demod_params's doc comment (and
/// rigctl.rs's identical DemodParamsCell, which this mirrors) for why
/// this indirection exists.
type DemodParamsCell = Arc<Mutex<Arc<Mutex<DemodParams>>>>;

pub struct TciServer {
    demod_params: DemodParamsCell,
    stop: Arc<AtomicBool>,
    /// Count of currently-connected clients (normally 0 or 1, but the
    /// accept loop doesn't limit concurrent connections, so a counter
    /// is more correct than a bool if two ever overlap). Lets the UI
    /// show "listening, no client" vs. "client connected" separately
    /// from "not running at all" (server is None).
    connected: Arc<AtomicU32>,
    thread: Option<JoinHandle<()>>,
    /// Per-client handler threads -- stop() joins these too, not just
    /// the accept thread. handle_client already polls `stop` via its
    /// own read timeout (so it was never leaked indefinitely the way
    /// rigctl.rs's equivalent was before a matching fix), but without
    /// this a caller that immediately rebinds the same address (e.g.
    /// the user stopping/restarting this from Settings -> Network)
    /// could still race a client thread that hasn't quite exited yet.
    /// A sample-rate change no longer tears this server down at all
    /// anymore -- see set_demod_params.
    client_threads: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl TciServer {
    /// Starts listening in the background on `addr` (e.g.
    /// "127.0.0.1:40001"). Returns Err if the address is invalid or the
    /// port is already in use.
    pub fn start(
        addr: &str,
        frequency_hz: Arc<AtomicU32>,
        demod_params: Arc<Mutex<DemodParams>>,
        mox: Arc<AtomicBool>,
    ) -> std::io::Result<Self> {
        let listener = TcpListener::bind(addr)?;
        listener.set_nonblocking(true)?;
        println!("tci: listening on {addr}");

        let demod_params: DemodParamsCell = Arc::new(Mutex::new(demod_params));
        let stop = Arc::new(AtomicBool::new(false));
        let connected = Arc::new(AtomicU32::new(0));
        let client_threads: Arc<Mutex<Vec<JoinHandle<()>>>> = Arc::new(Mutex::new(Vec::new()));
        let accept_stop = Arc::clone(&stop);
        let accept_connected = Arc::clone(&connected);
        let accept_client_threads = Arc::clone(&client_threads);
        let accept_demod_params = Arc::clone(&demod_params);
        let thread = thread::spawn(move || {
            while !accept_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, peer)) => {
                        println!("tci: client connected from {peer}");
                        let freq = Arc::clone(&frequency_hz);
                        let params = Arc::clone(&accept_demod_params);
                        let conn_mox = Arc::clone(&mox);
                        let conn_stop = Arc::clone(&accept_stop);
                        let conn_connected = Arc::clone(&accept_connected);
                        let handle = thread::spawn(move || {
                            conn_connected.fetch_add(1, Ordering::Relaxed);
                            handle_client(stream, freq, params, conn_mox, conn_stop);
                            conn_connected.fetch_sub(1, Ordering::Relaxed);
                        });
                        let mut threads = accept_client_threads.lock().unwrap();
                        threads.retain(|h| !h.is_finished()); // opportunistic cleanup
                        threads.push(handle);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(100));
                    }
                    Err(_) => break,
                }
            }
        });

        Ok(Self {
            demod_params,
            stop,
            connected,
            thread: Some(thread),
            client_threads,
        })
    }

    /// Points this server (and every currently-connected client's
    /// handler thread) at a different DemodParams -- see rigctl.rs's
    /// identical set_demod_params for the full explanation (this fixes
    /// the same reported "disconnects on sample rate change" bug for
    /// TCI clients too).
    pub fn set_demod_params(&self, new_demod_params: Arc<Mutex<DemodParams>>) {
        *self.demod_params.lock().unwrap() = new_demod_params;
    }

    /// True while at least one client is currently connected. Callers
    /// (the Network settings tab, the status indicator) use this to
    /// tell "listening but idle" apart from "actively in use".
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed) > 0
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Join client threads too -- see client_threads' doc comment.
        // Each should already be on its way out (its own read timeout
        // means it notices `stop` within ~250ms), this just makes sure
        // stop() doesn't return before that's actually happened.
        let threads: Vec<JoinHandle<()>> = self.client_threads.lock().unwrap().drain(..).collect();
        for t in threads {
            let _ = t.join();
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for TciServer {
    fn drop(&mut self) {
        self.stop();
    }
}

fn handle_client(
    stream: TcpStream,
    frequency_hz: Arc<AtomicU32>,
    demod_params: DemodParamsCell,
    mox: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
) {
    let _ = stream.set_nodelay(true);

    let mut ws = match tungstenite::accept(stream) {
        Ok(ws) => ws,
        Err(e) => {
            eprintln!("tci: websocket handshake failed: {e}");
            return;
        }
    };

    // Poll for the stop flag periodically rather than blocking forever
    // on read().
    if let Err(e) = ws.get_ref().set_read_timeout(Some(Duration::from_millis(250))) {
        eprintln!("tci: failed to set read timeout: {e}");
    }

    // Best-effort initial state push -- see module-level note.
    let freq = frequency_hz.load(Ordering::Relaxed);
    let mode = demod_params.lock().unwrap().clone().lock().unwrap().mode;
    let _ = ws.send(Message::Text(PROTOCOL_NAME.into()));
    let _ = ws.send(Message::Text(format!("vfo:0,0,{freq};").into()));
    let _ = ws.send(Message::Text(format!("modulation:0,{};", mode_to_tci(mode)).into()));
    let _ = ws.send(Message::Text(
        format!("trx:0,{};", mox.load(Ordering::Relaxed)).into(),
    ));

    while !stop.load(Ordering::Relaxed) {
        match ws.read() {
            Ok(Message::Text(text)) => {
                for cmd in text.split(';') {
                    let cmd = cmd.trim();
                    if cmd.is_empty() {
                        continue;
                    }
                    // Resolved fresh on every command (not once per
                    // connection) so a sample-rate change mid-session --
                    // see set_demod_params -- takes effect immediately
                    // for already-connected clients too, not just new
                    // ones.
                    let current_params = demod_params.lock().unwrap().clone();
                    if let Some(response) = handle_command(cmd, &frequency_hz, &current_params, &mox) {
                        if ws.send(Message::Text(response.into())).is_err() {
                            return;
                        }
                    }
                }
            }
            Ok(Message::Close(_)) => break,
            Ok(_) => {} // ping/pong/binary -- ignored for now
            Err(tungstenite::Error::Io(ref e))
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(_) => break,
        }
    }
}

/// Returns Some(response-to-send) for recognized commands, or None to
/// send nothing back -- including for unrecognized commands, since the
/// protocol itself says invalid commands should just be ignored.
fn handle_command(
    cmd: &str,
    frequency_hz: &Arc<AtomicU32>,
    demod_params: &Arc<Mutex<DemodParams>>,
    mox: &Arc<AtomicBool>,
) -> Option<String> {
    let mut parts = cmd.splitn(2, ':');
    let name = parts.next().unwrap_or("").to_lowercase();
    let args: Vec<&str> = parts.next().unwrap_or("").split(',').collect();

    match name.as_str() {
        // vfo:receiver,vfo,frequency;
        "vfo" => {
            let f = args.get(2)?.parse::<f64>().ok()?;
            let f = f.round().max(0.0) as u32;
            frequency_hz.store(f, Ordering::Relaxed);
            Some(format!(
                "vfo:{},{},{};",
                args.first().unwrap_or(&"0"),
                args.get(1).unwrap_or(&"0"),
                f
            ))
        }
        // modulation:receiver,mode;
        "modulation" => {
            let mode = tci_to_mode(args.get(1)?)?;
            demod_params.lock().unwrap().mode = mode;
            Some(format!(
                "modulation:{},{};",
                args.first().unwrap_or(&"0"),
                mode_to_tci(mode)
            ))
        }
        // trx:receiver,state;
        "trx" => {
            let on = matches!(args.get(1), Some(&"true") | Some(&"1"));
            mox.store(on, Ordering::Relaxed);
            Some(format!("trx:{},{};", args.first().unwrap_or(&"0"), on))
        }
        _ => None,
    }
}

fn mode_to_tci(mode: Mode) -> &'static str {
    match mode {
        Mode::Lsb => "LSB",
        Mode::Usb => "USB",
        Mode::Dsb => "DSB",
        Mode::Cwl | Mode::Cwu => "CW",
        Mode::Fmn => "NFM",
        Mode::Am => "AM",
        Mode::Digu => "DIGU",
        Mode::Digl => "DIGL",
        Mode::Sam => "SAM",
        Mode::Drm => "AM",
        Mode::Spec => "USB",
    }
}

fn tci_to_mode(s: &str) -> Option<Mode> {
    match s.to_uppercase().as_str() {
        "LSB" => Some(Mode::Lsb),
        "USB" => Some(Mode::Usb),
        "DSB" => Some(Mode::Dsb),
        "CW" | "CWR" => Some(Mode::Cwu),
        "NFM" | "FM" | "WFM" => Some(Mode::Fmn),
        "AM" => Some(Mode::Am),
        "DIGU" | "PKTUSB" => Some(Mode::Digu),
        "DIGL" | "PKTLSB" => Some(Mode::Digl),
        "SAM" => Some(Mode::Sam),
        _ => None,
    }
}
