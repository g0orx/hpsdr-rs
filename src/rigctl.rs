/*
    Minimal implementation of Hamlib's rigctld network protocol, enough
    for WSJT-X's "Hamlib NET rigctl" rig backend to read/set frequency
    and mode, key/unkey PTT, and read the S-meter (`l STRENGTH`, returns
    the raw dBm value from the same calibrated GetRXAMeter reading
    main.rs's on-screen S-meter uses). Every other Hamlib level (SWR,
    ALC, RFPOWER, ...) responds RPRT -1 (unsupported) rather than a
    made-up value. Listens on 0.0.0.0:4532 by default --
    Hamlib's own standard default port for this, but bound to all
    interfaces rather than just loopback, so a client on another
    machine on the network (a remote-operating laptop, a tablet, a
    separate shack PC) can connect too, not just software running on
    this same machine. This protocol has no authentication of its own,
    so anyone who can reach this port on the network can key the
    radio; only expose it on networks you trust.

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

use crate::debug_log::DebugLog;
use crate::spectrum::{DemodParams, Mode, SpectrumDisplay};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

pub const DEFAULT_ADDR: &str = "0.0.0.0:4532";

/// A swappable reference to "whichever DemodParams is current right
/// now" -- see RigctlServer::set_demod_params's doc comment for why
/// this indirection exists.
type DemodParamsCell = Arc<Mutex<Arc<Mutex<DemodParams>>>>;

/// Same swappable-reference reasoning as DemodParamsCell, for the
/// SpectrumDisplay get_level (STRENGTH) reads that a sample-rate change
/// (main.rs's change_sample_rate) would otherwise leave pointed at a
/// stale, no-longer-updated SpectrumHandle -- see set_display's doc
/// comment.
type DisplayCell = Arc<Mutex<Arc<Mutex<SpectrumDisplay>>>>;

pub struct RigctlServer {
    demod_params: DemodParamsCell,
    display: DisplayCell,
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
    /// Starts listening in the background on `addr` (e.g. "0.0.0.0:4532",
    /// the default -- or "127.0.0.1:4532" to restrict to this machine only).
    /// Returns Err if the address is invalid or the port is already in
    /// use (e.g. another rigctld already running) -- the caller should
    /// treat that as non-fatal, same as audio device failures elsewhere
    /// in this app.
    pub fn start(
        addr: &str,
        // Where `F`/`\set_freq` writes its request -- NOT the raw
        // hardware frequency. See RadioSession::requested_frequency_hz's
        // doc comment: main.rs's own per-frame loop reconciles this
        // (moving the CTUN target if CTUN is on, retuning the real
        // hardware otherwise), since CTUN state lives in the UI layer,
        // not anywhere reachable from this server thread.
        requested_frequency_hz: Arc<AtomicU32>,
        // See RadioSession::rx_frequency_hz's doc comment -- used for
        // `f`/`\get_freq` so a CTUN'd listen frequency is reported
        // correctly, not the parked hardware LO.
        rx_frequency_hz: Arc<AtomicU32>,
        demod_params: Arc<Mutex<DemodParams>>,
        display: Arc<Mutex<SpectrumDisplay>>,
        mox: Arc<AtomicBool>,
        // See RadioSession::rit_enabled/rit_offset_hz/xit_enabled/
        // xit_offset_hz's doc comments -- backs j/J (get/set_rit), z/Z
        // (get/set_xit), and u/U (get/set_func RIT/XIT).
        rit_enabled: Arc<AtomicBool>,
        rit_offset_hz: Arc<AtomicI32>,
        xit_enabled: Arc<AtomicBool>,
        xit_offset_hz: Arc<AtomicI32>,
        logging: DebugLog,
    ) -> std::io::Result<Self> {
        let listener = TcpListener::bind(addr)?;
        listener.set_nonblocking(true)?;
        println!("rigctl: listening on {addr}");

        let demod_params: DemodParamsCell = Arc::new(Mutex::new(demod_params));
        let display: DisplayCell = Arc::new(Mutex::new(display));
        let stop = Arc::new(AtomicBool::new(false));
        let connected = Arc::new(AtomicU32::new(0));
        let client_threads: Arc<Mutex<Vec<JoinHandle<()>>>> = Arc::new(Mutex::new(Vec::new()));
        let accept_stop = Arc::clone(&stop);
        let accept_connected = Arc::clone(&connected);
        let accept_client_threads = Arc::clone(&client_threads);
        let accept_demod_params = Arc::clone(&demod_params);
        let accept_display = Arc::clone(&display);
        let accept_logging = logging.clone();
        let thread = thread::spawn(move || {
            while !accept_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, peer)) => {
                        println!("rigctl: client connected from {peer}");
                        accept_logging.log(&format!("client connected from {peer}"));
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
                        let freq = Arc::clone(&requested_frequency_hz);
                        let rx_freq = Arc::clone(&rx_frequency_hz);
                        let params = Arc::clone(&accept_demod_params);
                        let disp = Arc::clone(&accept_display);
                        let conn_mox = Arc::clone(&mox);
                        let conn_rit_enabled = Arc::clone(&rit_enabled);
                        let conn_rit_offset_hz = Arc::clone(&rit_offset_hz);
                        let conn_xit_enabled = Arc::clone(&xit_enabled);
                        let conn_xit_offset_hz = Arc::clone(&xit_offset_hz);
                        let conn_stop = Arc::clone(&accept_stop);
                        let conn_connected = Arc::clone(&accept_connected);
                        let conn_logging = accept_logging.clone();
                        let handle = thread::spawn(move || {
                            conn_connected.fetch_add(1, Ordering::Relaxed);
                            handle_client(
                                stream, freq, rx_freq, params, disp, conn_mox, conn_rit_enabled,
                                conn_rit_offset_hz, conn_xit_enabled, conn_xit_offset_hz, conn_stop, conn_logging,
                            );
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
            display,
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
    /// "rigctl disconnects on sample rate change". requested_frequency_hz and mox
    /// don't need this treatment since they're the same Arc from
    /// RadioSession across a sample-rate change; only DemodParams gets
    /// replaced.
    pub fn set_demod_params(&self, new_demod_params: Arc<Mutex<DemodParams>>) {
        *self.demod_params.lock().unwrap() = new_demod_params;
    }

    /// Same reasoning as set_demod_params -- a sample-rate change
    /// rebuilds SpectrumHandle (and its `display`) from scratch, so
    /// `l STRENGTH`/`\get_level STRENGTH` would silently keep reading a
    /// frozen meter_db from the old, no-longer-updated SpectrumDisplay
    /// without this.
    pub fn set_display(&self, new_display: Arc<Mutex<SpectrumDisplay>>) {
        *self.display.lock().unwrap() = new_display;
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
    requested_frequency_hz: Arc<AtomicU32>,
    rx_frequency_hz: Arc<AtomicU32>,
    demod_params: DemodParamsCell,
    display: DisplayCell,
    mox: Arc<AtomicBool>,
    rit_enabled: Arc<AtomicBool>,
    rit_offset_hz: Arc<AtomicI32>,
    xit_enabled: Arc<AtomicBool>,
    xit_offset_hz: Arc<AtomicI32>,
    stop: Arc<AtomicBool>,
    logging: DebugLog,
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
            Ok(0) => {
                logging.log("client closed the connection");
                break;
            }
            Ok(_) => {
                let cmd = line.trim();
                if cmd.is_empty() {
                    continue;
                }
                logging.log(&format!("<< {cmd}"));
                // Resolved fresh on every command (not once per
                // connection) so a sample-rate change mid-session -- see
                // set_demod_params -- takes effect immediately for
                // already-connected clients too, not just new ones.
                let current_params = demod_params.lock().unwrap().clone();
                let current_display = display.lock().unwrap().clone();
                match handle_command(
                    cmd,
                    &requested_frequency_hz,
                    &rx_frequency_hz,
                    &current_params,
                    &current_display,
                    &mox,
                    &rit_enabled,
                    &rit_offset_hz,
                    &xit_enabled,
                    &xit_offset_hz,
                ) {
                    Some(response) => {
                        logging.log(&format!(">> {}", response.trim_end()));
                        if writer.write_all(response.as_bytes()).is_err() {
                            break;
                        }
                    }
                    None => {
                        logging.log("client sent quit");
                        break; // quit command
                    }
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
            // See tci.rs's identical arm's doc comment -- same read-
            // timeout pattern, same investigation into a real report of
            // Windows silently dropping a connection this way that Linux
            // doesn't; logging here in case WSJT-X-over-rigctl hits the
            // same thing.
            Err(e) => {
                logging.log(&format!("connection error, closing: {e:?}"));
                break;
            }
        }
    }
}

fn handle_command(
    cmd: &str,
    requested_frequency_hz: &Arc<AtomicU32>,
    rx_frequency_hz: &Arc<AtomicU32>,
    demod_params: &Arc<Mutex<DemodParams>>,
    display: &Arc<Mutex<SpectrumDisplay>>,
    mox: &Arc<AtomicBool>,
    rit_enabled: &Arc<AtomicBool>,
    rit_offset_hz: &Arc<AtomicI32>,
    xit_enabled: &Arc<AtomicBool>,
    xit_offset_hz: &Arc<AtomicI32>,
) -> Option<String> {
    let mut parts = cmd.split_whitespace();
    let op = parts.next().unwrap_or("");

    let response = match op {
        "f" | "\\get_freq" => {
            let f = rx_frequency_hz.load(Ordering::Relaxed);
            format!("{f}\n")
        }
        "F" | "\\set_freq" => match parts.next().and_then(|s| s.parse::<f64>().ok()) {
            Some(f) => {
                requested_frequency_hz.store(f.round().max(0.0) as u32, Ordering::Relaxed);
                "RPRT 0\n".to_string()
            }
            None => "RPRT -1\n".to_string(),
        },
        // RIT/XIT offset, Hz, signed -- clamped to +-9999 matching
        // main.rs's own UI clamp for these values.
        "j" | "\\get_rit" => {
            let v = rit_offset_hz.load(Ordering::Relaxed);
            format!("{v}\n")
        }
        "J" | "\\set_rit" => match parts.next().and_then(|s| s.parse::<i32>().ok()) {
            Some(v) => {
                rit_offset_hz.store(v.clamp(-9999, 9999), Ordering::Relaxed);
                "RPRT 0\n".to_string()
            }
            None => "RPRT -1\n".to_string(),
        },
        "z" | "\\get_xit" => {
            let v = xit_offset_hz.load(Ordering::Relaxed);
            format!("{v}\n")
        }
        "Z" | "\\set_xit" => match parts.next().and_then(|s| s.parse::<i32>().ok()) {
            Some(v) => {
                xit_offset_hz.store(v.clamp(-9999, 9999), Ordering::Relaxed);
                "RPRT 0\n".to_string()
            }
            None => "RPRT -1\n".to_string(),
        },
        // RIT/XIT on/off via Hamlib's func mechanism -- only these two
        // funcs are backed by anything; any other func name is RPRT -1,
        // same "respond unsupported rather than a made-up value"
        // philosophy as everywhere else in this file. dump_state's own
        // has_get_func/has_set_func are deliberately left at 0 below
        // regardless (see that function's own comment) -- this doesn't
        // claim full Hamlib function-bitmap support, just these two
        // specific funcs for a client that tries them directly.
        "u" | "\\get_func" => match parts.next().unwrap_or("") {
            "RIT" => format!("{}\n", rit_enabled.load(Ordering::Relaxed) as i32),
            "XIT" => format!("{}\n", xit_enabled.load(Ordering::Relaxed) as i32),
            _ => "RPRT -1\n".to_string(),
        },
        "U" | "\\set_func" => {
            let func = parts.next().unwrap_or("");
            let val = parts.next().and_then(|s| s.parse::<i32>().ok());
            match (func, val) {
                ("RIT", Some(v)) => {
                    rit_enabled.store(v != 0, Ordering::Relaxed);
                    "RPRT 0\n".to_string()
                }
                ("XIT", Some(v)) => {
                    xit_enabled.store(v != 0, Ordering::Relaxed);
                    "RPRT 0\n".to_string()
                }
                _ => "RPRT -1\n".to_string(),
            }
        }
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
        "l" | "\\get_level" => {
            let level = parts.next().unwrap_or("");
            match level {
                // STRENGTH: the raw dBm value from GetRXAMeter, no S9
                // offset applied -- same source main.rs's on-screen
                // S-meter reads (see its own draw_s_meter doc comment
                // for the S9=-73dBm reference point that display
                // applies separately, purely for its own tick labels).
                // WSJT-X and Hamlib's own `rigctl` CLI both send this
                // bare (no numeric arg) to poll the meter; there's no
                // set_level counterpart since it's a read-only
                // receive-side meter.
                "STRENGTH" => {
                    let db = display.lock().unwrap().meter_db;
                    format!("{}\n", db.round() as i32)
                }
                // Every other Hamlib level (SWR, ALC, RFPOWER, AF, RF,
                // SQL, ...) isn't backed by anything yet -- RPRT -1
                // (unsupported) rather than a made-up value, matching
                // this file's existing fallback for any unrecognized
                // command.
                _ => "RPRT -1\n".to_string(),
            }
        }
        // No settable levels yet -- every one of them (including
        // STRENGTH, which is receive-only on real hardware too) is
        // read-only from this app's side.
        "L" | "\\set_level" => "RPRT -1\n".to_string(),
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
        "9999\n",    // max RIT -- real now, see handle_command's j/J
        "9999\n",    // max XIT -- real now, see handle_command's z/Z
        "0\n",       // max IF shift
        "0\n",       // announces
        "0\n",       // preamp list
        "0\n",       // attenuator list
        // Left at 0 despite j/J/z/Z/u/U (RIT/XIT get/set_func) now being
        // real -- see handle_command's own comment on why: this would
        // need the correct Hamlib RIG_FUNC bitmask encoding to claim
        // properly, which isn't confirmed here, and a wrong bitmask
        // could be worse than 0 for a client that trusts it. A client
        // that just tries u/U RIT/XIT directly (rather than gating on
        // this) sees the real values regardless.
        "0\n",       // has_get_func
        "0\n",       // has_set_func
        // Left at 0 rather than advertising a RIG_LEVEL_STRENGTH bit --
        // this file's exact bitmask value isn't confirmed against
        // Hamlib source (see module note), and getting it wrong here
        // could be worse than a plain 0 for a client that trusts this
        // capability list. `l STRENGTH` is handled directly regardless
        // of what's advertised here -- see handle_command -- which is
        // enough for Hamlib's own `rigctl` CLI and any client that
        // just tries it rather than gating on this first.
        "0\n",       // has_get_level
        "0\n",       // has_set_level
        "0\n",       // has_get_parm
        "0\n",       // has_set_parm
        "done\n",
    )
    .to_string()
}
