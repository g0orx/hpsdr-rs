/*
    Minimal implementation of Hamlib's rigctld network protocol, enough
    for WSJT-X's "Hamlib NET rigctl" rig backend to read/set frequency
    and mode, and key/unkey PTT. Listens on 127.0.0.1:4532 by default --
    Hamlib's own standard default port for this.

    set_ptt here just flips RadioSession's mox flag -- the same one the
    on-screen PTT button and TCI's trx command use. See radio.rs and
    tx.rs for the (unverified -- see their module notes) actual TX
    audio/protocol path this then drives. PTT only works while transmit
    is armed (Settings -> TX -> Enable Transmit); if it's not, set_ptt
    still reports RPRT 0 (so WSJT-X's handshake doesn't fail) but mox
    has no receiver on the other end, since RadioSession's sender loops
    check mox regardless of whether a TxHandle is currently producing
    audio for it -- keyed-with-silence rather than not keying at all,
    which is a real (if narrow) gap: WSJT-X could believe it's
    transmitting FT8 while actually sending silence into the DUC. Arm
    transmit before running any digital-mode session that uses CAT PTT.

    The \dump_state response format below is reconstructed from general
    knowledge of the rigctld protocol, not verified against a reference
    implementation the way the WDSP FFI calls were -- if WSJT-X fails to
    fully initialize rig control, this is the first thing to suspect.
    Hamlib's own `rigctl` CLI tool (if installed) is a good way to test
    basic get/set commands independently of WSJT-X's own handshake:
    `rigctl -m 2 -r 127.0.0.1:4532 f`
*/

use crate::spectrum::{DemodParams, Mode};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

pub const DEFAULT_ADDR: &str = "127.0.0.1:4532";

/// A swappable reference to "whichever DemodParams is current right
/// now" -- see RigctlServer::set_demod_params's doc comment for why
/// this indirection exists.
type DemodParamsCell = Arc<Mutex<Arc<Mutex<DemodParams>>>>;

pub struct RigctlServer {
    demod_params: DemodParamsCell,
    stop: Arc<AtomicBool>,
    /// Count of currently-connected clients (normally 0 or 1, but the
    /// accept loop doesn't limit concurrent connections, so a counter
    /// is more correct than a bool if two ever overlap). Lets the UI
    /// show "listening, no client" vs. "client connected" separately
    /// from "not running at all" (server is None).
    connected: Arc<AtomicU32>,
    thread: Option<JoinHandle<()>>,
    /// Per-client handler threads (see handle_client) -- stop() joins
    /// these too, not just the accept thread. Without this, a client
    /// connected at the moment this server is torn down (e.g. the user
    /// explicitly stopping/restarting it from Settings -> Network) was
    /// silently leaked: its thread kept running, still holding the
    /// TCP connection and the old local port, so a caller that
    /// immediately rebinds the same address would race a socket the
    /// old listener hadn't actually released yet. A sample-rate change
    /// no longer tears this server down at all anymore -- see
    /// set_demod_params -- but this still matters for the
    /// user-initiated stop/restart case.
    client_threads: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl RigctlServer {
    /// Starts listening in the background on `addr` (e.g. "127.0.0.1:4532").
    /// Returns Err if the address is invalid or the port is already in
    /// use (e.g. another rigctld already running) -- the caller should
    /// treat that as non-fatal, same as audio device failures elsewhere
    /// in this app.
    pub fn start(
        addr: &str,
        frequency_hz: Arc<AtomicU32>,
        demod_params: Arc<Mutex<DemodParams>>,
        mox: Arc<AtomicBool>,
    ) -> std::io::Result<Self> {
        let listener = TcpListener::bind(addr)?;
        listener.set_nonblocking(true)?;
        println!("rigctl: listening on {addr}");

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
                        println!("rigctl: client connected from {peer}");
                        // Matches tci.rs's already-proven approach: a
                        // read timeout so handle_client's loop notices
                        // `stop` promptly instead of blocking forever
                        // in read_line() on an idle connection -- see
                        // client_threads' doc comment for why an
                        // unbounded block here was a real bug, not
                        // just untidy.
                        if let Err(e) = stream.set_read_timeout(Some(Duration::from_millis(250))) {
                            eprintln!("rigctl: failed to set read timeout: {e}");
                        }
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
    /// handler thread) at a different DemodParams -- e.g. after
    /// main.rs's change_sample_rate rebuilds the SpectrumHandle, which
    /// creates a fresh DemodParams that the *old* one this server
    /// started with knows nothing about. Previously the only way to
    /// pick up a new DemodParams was to drop and recreate the whole
    /// RigctlServer, which tore down the TCP listener and forcibly
    /// disconnected any client (e.g. WSJT-X) that happened to be
    /// connected at the time -- confirmed as the cause of a reported
    /// "rigctl disconnects on sample rate change". frequency_hz and mox
    /// don't need this treatment since they're the same Arc from
    /// RadioSession across a sample-rate change; only DemodParams gets
    /// replaced.
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
        // Join client threads too (not just the accept thread) so a
        // caller that immediately rebinds the same address (e.g. after
        // a sample-rate change) doesn't race a not-yet-closed client
        // connection still holding the port -- see client_threads'
        // doc comment.
        let threads: Vec<JoinHandle<()>> = self.client_threads.lock().unwrap().drain(..).collect();
        for t in threads {
            let _ = t.join();
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for RigctlServer {
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
    let mut writer = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    let mut reader = BufReader::new(stream);
    let mut line = String::new();

    while !stop.load(Ordering::Relaxed) {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break, // client closed the connection
            Ok(_) => {
                let cmd = line.trim();
                if cmd.is_empty() {
                    continue;
                }
                // Resolved fresh on every command (not once per
                // connection) so a sample-rate change mid-session -- see
                // set_demod_params -- takes effect immediately for
                // already-connected clients too, not just new ones.
                let current_params = demod_params.lock().unwrap().clone();
                match handle_command(cmd, &frequency_hz, &current_params, &mox) {
                    Some(response) => {
                        if writer.write_all(response.as_bytes()).is_err() {
                            break;
                        }
                    }
                    None => break, // quit command
                }
            }
            // Read timeout (see start()'s set_read_timeout) -- expected
            // on an idle connection, not a real error. Loop back around
            // to re-check `stop` rather than treating it as the
            // connection having failed. `line` may hold a partial
            // fragment of whatever WSJT-X was mid-way through sending;
            // next iteration's `line.clear()` drops it, same tradeoff
            // tci.rs already accepts for its own read-timeout loop --
            // fine in practice since commands are tiny single-line
            // sends, not something realistically split across a 250ms
            // window.
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => {
                continue;
            }
            Err(_) => break,
        }
    }
}

fn handle_command(
    cmd: &str,
    frequency_hz: &Arc<AtomicU32>,
    demod_params: &Arc<Mutex<DemodParams>>,
    mox: &Arc<AtomicBool>,
) -> Option<String> {
    let mut parts = cmd.split_whitespace();
    let op = parts.next().unwrap_or("");

    let response = match op {
        "f" | "\\get_freq" => {
            let f = frequency_hz.load(Ordering::Relaxed);
            format!("{f}\n")
        }
        "F" | "\\set_freq" => match parts.next().and_then(|s| s.parse::<f64>().ok()) {
            Some(f) => {
                frequency_hz.store(f.round().max(0.0) as u32, Ordering::Relaxed);
                "RPRT 0\n".to_string()
            }
            None => "RPRT -1\n".to_string(),
        },
        "m" | "\\get_mode" => {
            let p = *demod_params.lock().unwrap();
            format!("{}\n{}\n", mode_to_hamlib(p.mode), p.width_hz.round() as i64)
        }
        "M" | "\\set_mode" => {
            let mode_str = parts.next().unwrap_or("");
            match hamlib_to_mode(mode_str) {
                Some(mode) => {
                    let mut p = demod_params.lock().unwrap();
                    p.mode = mode;
                    // Passband, if given and positive, sets filter width too.
                    if let Some(pb) = parts.next().and_then(|s| s.parse::<f64>().ok()) {
                        if pb > 0.0 {
                            p.width_hz = pb;
                        }
                    }
                    "RPRT 0\n".to_string()
                }
                None => "RPRT -1\n".to_string(),
            }
        }
        "v" | "\\get_vfo" => "VFOA\n".to_string(),
        "V" | "\\set_vfo" => "RPRT 0\n".to_string(),
        // No real VFO lock concept in this app -- always report
        // unlocked, accept a set as a no-op. NOT confirmed whether
        // there's also a single-letter short form for these two (some
        // rigctld commands don't have one); only the long forms are
        // handled here. Previously falling through to the generic
        // RPRT -1 error case, which WSJT-X's rigctl backend apparently
        // doesn't expect for this specific command as part of its
        // normal polling.
        // Response format here is per the user's direct guidance, not
        // independently confirmed against Hamlib source/docs -- unlike
        // get_ptt/get_vfo (which return a bare value, matching the
        // normal rigctld "get" convention), get_lock_mode apparently
        // expects an RPRT-style acknowledgment instead.
        "\\get_lock_mode" => "RPRT 0\n".to_string(),
        "\\set_lock_mode" => "RPRT 0\n".to_string(),
        "t" | "\\get_ptt" => {
            let on = mox.load(Ordering::Relaxed);
            format!("{}\n", if on { 1 } else { 0 })
        }
        "T" | "\\set_ptt" => match parts.next().and_then(|s| s.parse::<i32>().ok()) {
            Some(v) => {
                mox.store(v != 0, Ordering::Relaxed);
                "RPRT 0\n".to_string()
            }
            None => "RPRT -1\n".to_string(),
        },
        "\\chk_vfo" => "CHKVFO 0\n".to_string(),
        "\\dump_state" => dump_state(),
        "q" | "Q" | "\\quit" => return None,
        _ => "RPRT -1\n".to_string(),
    };

    Some(response)
}

fn mode_to_hamlib(mode: Mode) -> &'static str {
    match mode {
        Mode::Lsb => "LSB",
        Mode::Usb => "USB",
        Mode::Dsb => "DSB",
        Mode::Cwl => "CWR",
        Mode::Cwu => "CW",
        Mode::Fmn => "FM",
        Mode::Am => "AM",
        Mode::Digu => "PKTUSB",
        Mode::Digl => "PKTLSB",
        Mode::Sam => "SAM",
        // No clean Hamlib equivalent for these two -- fall back to
        // something that won't confuse a client expecting a real mode.
        Mode::Drm => "AM",
        Mode::Spec => "USB",
    }
}

fn hamlib_to_mode(s: &str) -> Option<Mode> {
    match s.to_uppercase().as_str() {
        "LSB" => Some(Mode::Lsb),
        "USB" => Some(Mode::Usb),
        "DSB" => Some(Mode::Dsb),
        "CWR" => Some(Mode::Cwl),
        "CW" => Some(Mode::Cwu),
        "FM" | "WFM" | "FMN" => Some(Mode::Fmn),
        "AM" => Some(Mode::Am),
        "PKTUSB" | "RTTY" => Some(Mode::Digu),
        "PKTLSB" | "RTTYR" => Some(Mode::Digl),
        "SAM" => Some(Mode::Sam),
        _ => None,
    }
}

/// Best-effort rigctld \dump_state response -- see module-level note on
/// confidence level. Advertises a permissive 0Hz-4GHz RX range, no TX
/// capability, and minimal/zeroed everything else.
fn dump_state() -> String {
    concat!(
        "0\n",       // protocol version
        "2\n",       // rig model (2 = generic/dummy in Hamlib's convention)
        "2\n",       // ITU region
        // RX range: start(Hz) end(Hz) modes(bitmask, -1=all) low_power high_power vfo(-1=all) ant(0)
        "0 4000000000 -1 -1 -1 -1 0\n",
        "0 0 0 0 0 0 0\n", // end of RX range list
        // TX range: mirrors the RX range above -- PTT is real now
        // (flips RadioSession's mox), but this app has no drive/power
        // level control yet, so the power figures here (0-100, meant
        // as "some plausible watts") are placeholders, not something
        // WSJT-X can actually use to set/limit drive through rigctl.
        "0 4000000000 -1 0 100 -1 0\n",
        "0 0 0 0 0 0 0\n", // end of TX range list
        "-1 1\n",    // tuning steps: all modes, 1Hz
        "0 0\n",     // end of tuning step list
        "-1 2400\n", // filters: all modes, 2400Hz default
        "0 0\n",     // end of filter list
        "0\n",       // max RIT
        "0\n",       // max XIT
        "0\n",       // max IF shift
        "0\n",       // announces
        "0\n",       // preamp list
        "0\n",       // attenuator list
        "0\n",       // has_get_func
        "0\n",       // has_set_func
        "0\n",       // has_get_level
        "0\n",       // has_set_level
        "0\n",       // has_get_parm
        "0\n",       // has_set_parm
        "done\n",
    )
    .to_string()
}
