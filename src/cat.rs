/*
    Kenwood TS-2000 CAT command emulation over TCP -- the ASCII,
    semicolon-terminated command set real Kenwood rigs (and, notably,
    piHPSDR's own "rigctl.c", despite that file's name -- it's actually
    a TS-2000 emulator, not literal Hamlib rigctld) speak, distinct from
    this app's OWN rigctl.rs (which implements Hamlib's rigctld network
    wire protocol -- single-letter/word commands like "f"/"F freq").
    Kenwood CAT is what many logging/rig-control programs (N1MM+,
    Log4OM, DXLab Commander, HRD, and others) talk directly when told
    the rig is a "Kenwood TS-2000" reachable over a network socket,
    without needing a separate rigctld translator in between. Default
    port 19090 matches piHPSDR's own rigctl_tcp_port default, so a
    logger already configured against a piHPSDR station needs no
    reconfiguration to point at this app instead.

    Ported from piHPSDR's src/rigctl.c (hardware-tested reference,
    written by the same author as this app), scoped down to a practical
    subset rather than porting its full ~5900 lines. Implemented:
    ID, IF, FA, FB, FR, FT, MD, AI, PS, TX, RX, SM, TY, RT, RC, RD, RU, XT.
    Deliberately NOT implemented (unlike the reference) because this app
    has no clean value/range to back them with yet, matching rigctl.rs's
    own "respond unsupported rather than a made-up value" philosophy:
      - PC (drive/power level): max_tx_power_watts isn't currently
        shared via an Arc the way frequency/mox are, and the reference's
        0-100 scale doesn't map cleanly onto watts without it.
      - MG (mic gain): this app's mic_gain is a linear 0.0-8.0
        multiplier, not the reference's -12..+50 dB range -- no honest
        conversion between the two.
      - KS/KY (CW keyer speed/send): no CW keyer exists in this app.
      - Split (FT accepted but always reports/treats as off), CTCSS,
        memory channels, SAT mode: none of these have any backing state
        in this app. RIT/XIT DO now (RT/RC/RD/RU/XT, and the IF
        response's rit/rit_en/xit_en fields) -- see RadioSession::
        rit_enabled's doc comment (radio.rs). No command to set an
        absolute XIT value exists in real Kenwood CAT either, only XT's
        on/off toggle -- matches the reference exactly.
      - AI's auto-reporting flag is stored and read back per the
        client's own request, but no asynchronous FA/MD/... push
        notifications are actually sent on state changes -- a client
        relying on AI>0 to avoid polling will not see updates. Poll-
        based clients (the common case) are unaffected.

    Kenwood CAT has no request/reply symmetry like Hamlib's rigctld:
    "read" commands (bare, e.g. "FA;") get a reply; "set" commands
    (e.g. "FA00014074000;") get NO reply at all, matching real Kenwood
    behavior (confirmed against piHPSDR's send_resp call sites -- only
    ever called from the read branch of each command). An entirely
    unrecognized 2-letter command code gets "?;", matching Kenwood's
    own convention for a command it doesn't understand -- useful for a
    logger's initial capability probing so it doesn't hang waiting on a
    reply that will never come.

    PTT (TX;/RX;) has the exact same caveat as rigctl.rs's set_ptt --
    see that file's module note: only meaningfully drives the radio
    while transmit is armed (Settings -> TX -> Enable Transmit).

    PS (power on/off): reading always reports PS1; (powered on).
    UNLIKE the reference (which calls radio_shutdown() on "PS0;"),
    setting is silently ignored here -- a stray or buggy CAT client
    sending PS0 shouldn't be able to quit this app. Deliberate
    deviation from the reference for safety, not an oversight.
*/

use crate::debug_log::DebugLog;
use crate::spectrum::{DemodParams, Mode, SpectrumDisplay};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

pub const DEFAULT_ADDR: &str = "0.0.0.0:19090";

/// Same swappable-reference reasoning as rigctl.rs's DemodParamsCell --
/// a sample-rate change (main.rs's change_sample_rate) rebuilds
/// SpectrumHandle from scratch, producing a fresh DemodParams/
/// SpectrumDisplay this server needs to be re-pointed at without
/// tearing down the TCP listener (and disconnecting any client) to do
/// it.
type DemodParamsCell = Arc<Mutex<Arc<Mutex<DemodParams>>>>;
type DisplayCell = Arc<Mutex<Arc<Mutex<SpectrumDisplay>>>>;

pub struct CatServer {
    demod_params: DemodParamsCell,
    display: DisplayCell,
    stop: Arc<AtomicBool>,
    /// See rigctl.rs's identical field doc comment -- same reasoning.
    connected: Arc<AtomicU32>,
    thread: Option<JoinHandle<()>>,
    /// See rigctl.rs's identical field doc comment -- same reasoning.
    client_threads: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl CatServer {
    /// Starts listening in the background on `addr` (e.g.
    /// "0.0.0.0:19090", the default -- or "127.0.0.1:19090" to restrict
    /// to this machine only). Returns Err if the address is invalid or
    /// the port is already in use, same as rigctl.rs/tci.rs -- the
    /// caller should treat that as non-fatal.
    pub fn start(
        addr: &str,
        // Where FA's "set" form writes its request -- NOT the raw
        // hardware frequency. See RadioSession::requested_frequency_hz's
        // doc comment: main.rs's own per-frame loop reconciles this
        // (moving the CTUN target if CTUN is on, retuning the real
        // hardware otherwise), since CTUN state lives in the UI layer,
        // not anywhere reachable from this server thread.
        requested_frequency_hz: Arc<AtomicU32>,
        // See RadioSession::rx_frequency_hz's doc comment -- used for
        // FA/IF's frequency-read fields so a CTUN'd listen frequency is
        // reported correctly, not the parked hardware LO.
        rx_frequency_hz: Arc<AtomicU32>,
        demod_params: Arc<Mutex<DemodParams>>,
        display: Arc<Mutex<SpectrumDisplay>>,
        mox: Arc<AtomicBool>,
        // See RadioSession::rit_enabled/rit_offset_hz/xit_enabled's doc
        // comments -- backs RT/RC/RD/RU (RIT) and XT (XIT). No
        // xit_offset_hz here -- real Kenwood TS-2000 CAT has no command
        // to set an absolute XIT value, only RIT's RC/RD/RU (see
        // RadioSession::xit_enabled's doc comment).
        rit_enabled: Arc<AtomicBool>,
        rit_offset_hz: Arc<AtomicI32>,
        xit_enabled: Arc<AtomicBool>,
        logging: DebugLog,
    ) -> std::io::Result<Self> {
        let listener = TcpListener::bind(addr)?;
        listener.set_nonblocking(true)?;
        println!("cat: listening on {addr}");

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
                        println!("cat: client connected from {peer}");
                        accept_logging.log(&format!("client connected from {peer}"));
                        if let Err(e) = stream.set_read_timeout(Some(Duration::from_millis(250))) {
                            eprintln!("cat: failed to set read timeout: {e}");
                        }
                        let freq = Arc::clone(&requested_frequency_hz);
                        let rx_freq = Arc::clone(&rx_frequency_hz);
                        let params = Arc::clone(&accept_demod_params);
                        let disp = Arc::clone(&accept_display);
                        let conn_mox = Arc::clone(&mox);
                        let conn_rit_enabled = Arc::clone(&rit_enabled);
                        let conn_rit_offset_hz = Arc::clone(&rit_offset_hz);
                        let conn_xit_enabled = Arc::clone(&xit_enabled);
                        let conn_stop = Arc::clone(&accept_stop);
                        let conn_connected = Arc::clone(&accept_connected);
                        let conn_logging = accept_logging.clone();
                        let handle = thread::spawn(move || {
                            conn_connected.fetch_add(1, Ordering::Relaxed);
                            handle_client(
                                stream, freq, rx_freq, params, disp, conn_mox, conn_rit_enabled,
                                conn_rit_offset_hz, conn_xit_enabled, conn_stop, conn_logging,
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

    /// See rigctl.rs's set_demod_params/set_display doc comments -- same
    /// reasoning, needed for the same reason (a sample-rate change
    /// rebuilding SpectrumHandle).
    pub fn set_demod_params(&self, new_demod_params: Arc<Mutex<DemodParams>>) {
        *self.demod_params.lock().unwrap() = new_demod_params;
    }

    pub fn set_display(&self, new_display: Arc<Mutex<SpectrumDisplay>>) {
        *self.display.lock().unwrap() = new_display;
    }

    /// True while at least one client is currently connected -- see
    /// rigctl.rs's identical method doc comment.
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed) > 0
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let threads: Vec<JoinHandle<()>> = self.client_threads.lock().unwrap().drain(..).collect();
        for t in threads {
            let _ = t.join();
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for CatServer {
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
    stop: Arc<AtomicBool>,
    logging: DebugLog,
) {
    let _ = stream.set_nodelay(true);
    let mut writer = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    let mut reader = stream;
    let mut read_buf = [0u8; 1024];
    // Accumulates bytes across reads until a full ';'-terminated
    // command is available -- a real command CAN arrive split across
    // TCP packet boundaries (e.g. a slow/throttled client), unlike
    // rigctl.rs's newline-delimited protocol where read_line already
    // handles this for free.
    let mut pending = String::new();
    // Per-connection AI (auto-reporting) level -- see this module's
    // doc comment for why setting it doesn't actually enable any push
    // notifications yet; it's tracked and read back anyway since a
    // client polling `AI;` to confirm its own request took effect
    // shouldn't see it silently reset to 0.
    let mut auto_reporting: u8 = 0;

    while !stop.load(Ordering::Relaxed) {
        match reader.read(&mut read_buf) {
            Ok(0) => {
                logging.log("client closed the connection");
                break;
            }
            Ok(n) => {
                pending.push_str(&String::from_utf8_lossy(&read_buf[..n]));
                // Process every complete (';'-terminated) command now
                // buffered; keep whatever's left (a partial trailing
                // command, or nothing) for the next read.
                while let Some(idx) = pending.find(';') {
                    let cmd: String = pending.drain(..=idx).collect();
                    let cmd = cmd.trim_end_matches(';').trim();
                    if cmd.is_empty() {
                        continue;
                    }
                    logging.log(&format!("<< {cmd};"));
                    let current_params = demod_params.lock().unwrap().clone();
                    let current_display = display.lock().unwrap().clone();
                    if let Some(response) = handle_command(
                        cmd,
                        &requested_frequency_hz,
                        &rx_frequency_hz,
                        &current_params,
                        &current_display,
                        &mox,
                        &rit_enabled,
                        &rit_offset_hz,
                        &xit_enabled,
                        &mut auto_reporting,
                    ) {
                        logging.log(&format!(">> {response}"));
                        if writer.write_all(response.as_bytes()).is_err() {
                            return;
                        }
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => {
                continue;
            }
            // See rigctl.rs/tci.rs's identical arm's doc comment -- same
            // read-timeout pattern.
            Err(e) => {
                logging.log(&format!("connection error, closing: {e:?}"));
                break;
            }
        }
    }
}

/// `cmd` is one command with the trailing ';' already stripped (e.g.
/// "FA" or "FA00014074000"). Returns the reply to send (WITHOUT a
/// trailing ';' -- callers add it), or `None` to send nothing at all
/// (the normal case for a "set" command -- see this module's doc
/// comment on Kenwood CAT's request/reply asymmetry).
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
    auto_reporting: &mut u8,
) -> Option<String> {
    if cmd.len() < 2 {
        return None;
    }
    let code = cmd[..2].to_uppercase();
    let suffix = cmd[2..].trim();

    match code.as_str() {
        "ID" => Some("ID019;".to_string()),
        "TY" => Some("TY000;".to_string()),
        "PS" => {
            if suffix.is_empty() {
                Some("PS1;".to_string())
            } else {
                // Deliberately ignored (not radio_shutdown()) -- see
                // this module's doc comment.
                None
            }
        }
        "IF" => {
            if suffix.is_empty() {
                Some(build_if_response(rx_frequency_hz, demod_params, mox, rit_enabled, rit_offset_hz, xit_enabled))
            } else {
                None
            }
        }
        // RIT status/value -- ported directly from piHPSDR's rigctl.c
        // (case 'R'/'T', 'C', 'D', 'U'), confirmed exact field widths and
        // step-size behavior against that reference rather than guessed.
        "RT" => {
            if suffix.is_empty() {
                Some(format!("RT{};", rit_enabled.load(Ordering::Relaxed) as i32))
            } else if let Ok(v) = suffix.parse::<i32>() {
                rit_enabled.store(v != 0, Ordering::Relaxed);
                None
            } else {
                None
            }
        }
        // Clear VFO-A RIT value.
        "RC" => {
            if suffix.is_empty() {
                rit_offset_hz.store(0, Ordering::Relaxed);
            }
            None
        }
        // Set or decrement VFO-A RIT value -- "RD;" (no arg) decrements
        // by 10Hz (CW modes) or 50Hz (other modes); "RDxxxxx;" sets RIT
        // to the NEGATIVE of x (matches the reference exactly).
        "RD" => {
            if suffix.is_empty() {
                let mode = demod_params.lock().unwrap().mode;
                let step = if matches!(mode, Mode::Cwl | Mode::Cwu) { 10 } else { 50 };
                let v = (rit_offset_hz.load(Ordering::Relaxed) - step).clamp(-9999, 9999);
                rit_offset_hz.store(v, Ordering::Relaxed);
            } else if let Ok(v) = suffix.parse::<i32>() {
                rit_offset_hz.store((-v).clamp(-9999, 9999), Ordering::Relaxed);
            }
            None
        }
        // Set or increment VFO-A RIT value -- same shape as RD, positive
        // direction (and an absolute set to +x, not -x).
        "RU" => {
            if suffix.is_empty() {
                let mode = demod_params.lock().unwrap().mode;
                let step = if matches!(mode, Mode::Cwl | Mode::Cwu) { 10 } else { 50 };
                let v = (rit_offset_hz.load(Ordering::Relaxed) + step).clamp(-9999, 9999);
                rit_offset_hz.store(v, Ordering::Relaxed);
            } else if let Ok(v) = suffix.parse::<i32>() {
                rit_offset_hz.store(v.clamp(-9999, 9999), Ordering::Relaxed);
            }
            None
        }
        // XIT status -- real Kenwood TS-2000 CAT has no command to set
        // an absolute XIT value (see RadioSession::xit_enabled's doc
        // comment), only this on/off toggle.
        "XT" => {
            if suffix.is_empty() {
                Some(format!("XT{};", xit_enabled.load(Ordering::Relaxed) as i32))
            } else if let Ok(v) = suffix.parse::<i32>() {
                xit_enabled.store(v != 0, Ordering::Relaxed);
                None
            } else {
                None
            }
        }
        "FA" => {
            if suffix.is_empty() {
                // rx_frequency_hz, not requested_frequency_hz -- see this
                // module's doc comment: reports the CTUN'd listen
                // frequency, not the parked hardware LO.
                let f = rx_frequency_hz.load(Ordering::Relaxed);
                Some(format!("FA{f:011};"))
            } else if let Ok(f) = suffix.parse::<u32>() {
                requested_frequency_hz.store(f, Ordering::Relaxed);
                None
            } else {
                None
            }
        }
        // This app's own VFO-B/Split (main window) isn't exposed
        // through CAT yet -- read mirrors FA's current frequency; a set
        // is accepted (no reply, matching a real "set" command's
        // silence) but otherwise a no-op. See this module's doc
        // comment.
        "FB" => {
            if suffix.is_empty() {
                let f = rx_frequency_hz.load(Ordering::Relaxed);
                Some(format!("FB{f:011};"))
            } else {
                None
            }
        }
        // Only one receiver known to CAT -- always reports/accepts
        // receiver 0.
        "FR" => {
            if suffix.is_empty() {
                Some("FR0;".to_string())
            } else {
                None
            }
        }
        // No split-VFO support -- always reports off; a set is silently
        // ignored (accepted with no reply) rather than erroring.
        "FT" => {
            if suffix.is_empty() {
                Some("FT0;".to_string())
            } else {
                None
            }
        }
        "MD" => {
            if suffix.is_empty() {
                let mode = demod_params.lock().unwrap().mode;
                Some(format!("MD{};", mode_to_ts2000(mode)))
            } else if let Ok(code) = suffix.parse::<u8>() {
                if let Some(mode) = ts2000_to_mode(code) {
                    demod_params.lock().unwrap().mode = mode;
                }
                None
            } else {
                None
            }
        }
        "AI" => {
            if suffix.is_empty() {
                Some(format!("AI{auto_reporting};"))
            } else if let Ok(level) = suffix.parse::<u8>() {
                *auto_reporting = level.min(3);
                None
            } else {
                None
            }
        }
        "TX" => {
            if suffix.is_empty() {
                mox.store(true, Ordering::Relaxed);
            }
            None
        }
        "RX" => {
            if suffix.is_empty() {
                mox.store(false, Ordering::Relaxed);
            }
            None
        }
        "SM" => {
            if suffix == "0" {
                let db = display.lock().unwrap().meter_db;
                let val = (((db + 127.0) * 0.277778).round() as i32).clamp(0, 30);
                Some(format!("SM0{val:04};"))
            } else {
                None
            }
        }
        _ => Some("?;".to_string()),
    }
}

/// IF response field layout ported directly from piHPSDR's rigctl.c
/// (the "IF" case, see this module's doc comment) -- see that file for
/// the full field-by-field breakdown. Fields with no backing state in
/// this app (tuning step, split, CTCSS) are hardcoded to their
/// "off"/zero value; rit/rit_en/xit_en now reflect real live state --
/// see RadioSession::rit_enabled's doc comment.
fn build_if_response(
    frequency_hz: &Arc<AtomicU32>,
    demod_params: &Arc<Mutex<DemodParams>>,
    mox: &Arc<AtomicBool>,
    rit_enabled: &Arc<AtomicBool>,
    rit_offset_hz: &Arc<AtomicI32>,
    xit_enabled: &Arc<AtomicBool>,
) -> String {
    let f = frequency_hz.load(Ordering::Relaxed);
    let mode = demod_params.lock().unwrap().mode;
    let transmitting = mox.load(Ordering::Relaxed) as u8;
    format!(
        "IF{f:011}{step:04}{rit:+06}{rit_en}{xit_en}{z1}{z2:02}{tx}{mode}{z3}{z4}{split}{ctcss_en:01}{ctcss:02}{z5};",
        step = 0,
        rit = rit_offset_hz.load(Ordering::Relaxed),
        rit_en = rit_enabled.load(Ordering::Relaxed) as u8,
        xit_en = xit_enabled.load(Ordering::Relaxed) as u8,
        z1 = 0,
        z2 = 0,
        tx = transmitting,
        mode = mode_to_ts2000(mode),
        z3 = 0,
        z4 = 0,
        split = 0,
        ctcss_en = 0,
        ctcss = 0,
        z5 = 0,
    )
}

/// See piHPSDR's ts2000_mode() -- same mapping. Dsb/Spec/Drm have no
/// clean Kenwood equivalent (same gap rigctl.rs's mode_to_hamlib has
/// for Drm/Spec); Dsb falls back to AM (structurally the closest: both
/// are double-sideband AM variants), Spec falls back to USB and Drm to
/// AM, matching rigctl.rs's own fallback choices for consistency.
fn mode_to_ts2000(mode: Mode) -> u8 {
    match mode {
        Mode::Lsb => 1,
        Mode::Usb => 2,
        Mode::Cwu => 3,
        Mode::Fmn => 4,
        Mode::Am | Mode::Sam | Mode::Dsb | Mode::Drm => 5,
        Mode::Digl => 6,
        Mode::Cwl => 7,
        Mode::Digu => 9,
        Mode::Spec => 2,
    }
}

/// See piHPSDR's wdspmode() -- same mapping (the inverse of
/// mode_to_ts2000 above, for the modes it actually produces).
fn ts2000_to_mode(code: u8) -> Option<Mode> {
    match code {
        1 => Some(Mode::Lsb),
        2 => Some(Mode::Usb),
        3 => Some(Mode::Cwu),
        4 => Some(Mode::Fmn),
        5 => Some(Mode::Am),
        6 => Some(Mode::Digl),
        7 => Some(Mode::Cwl),
        9 => Some(Mode::Digu),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicBool;

    fn fixture() -> (
        Arc<AtomicU32>,
        Arc<Mutex<DemodParams>>,
        Arc<Mutex<SpectrumDisplay>>,
        Arc<AtomicBool>,
        Arc<AtomicBool>,
        Arc<AtomicI32>,
        Arc<AtomicBool>,
    ) {
        let frequency_hz = Arc::new(AtomicU32::new(14_074_000));
        let demod_params = Arc::new(Mutex::new(DemodParams::default()));
        let display = Arc::new(Mutex::new(SpectrumDisplay {
            spectrum: Vec::new(),
            waterfall_rows: VecDeque::new(),
            meter_db: -73.0, // S9, matches piHPSDR's SM formula's reference point
            revision: 0,
        }));
        let mox = Arc::new(AtomicBool::new(false));
        let rit_enabled = Arc::new(AtomicBool::new(false));
        let rit_offset_hz = Arc::new(AtomicI32::new(0));
        let xit_enabled = Arc::new(AtomicBool::new(false));
        (frequency_hz, demod_params, display, mox, rit_enabled, rit_offset_hz, xit_enabled)
    }

    #[test]
    fn mode_round_trips_through_ts2000_codes() {
        for mode in [
            Mode::Lsb,
            Mode::Usb,
            Mode::Cwu,
            Mode::Fmn,
            Mode::Am,
            Mode::Digl,
            Mode::Cwl,
            Mode::Digu,
        ] {
            let code = mode_to_ts2000(mode);
            assert_eq!(ts2000_to_mode(code), Some(mode), "mode {mode:?} -> code {code} didn't round-trip");
        }
    }

    #[test]
    fn id_reports_ts2000() {
        let (f, p, d, m, rit, rit_hz, xit) = fixture();
        let mut ai = 0;
        assert_eq!(handle_command("ID", &f, &f, &p, &d, &m, &rit, &rit_hz, &xit, &mut ai), Some("ID019;".to_string()));
    }

    #[test]
    fn fa_read_reflects_current_frequency() {
        let (f, p, d, m, rit, rit_hz, xit) = fixture();
        let mut ai = 0;
        assert_eq!(handle_command("FA", &f, &f, &p, &d, &m, &rit, &rit_hz, &xit, &mut ai), Some("FA00014074000;".to_string()));
    }

    #[test]
    fn fa_set_updates_frequency_and_sends_no_reply() {
        let (f, p, d, m, rit, rit_hz, xit) = fixture();
        let mut ai = 0;
        assert_eq!(handle_command("FA00007074000", &f, &f, &p, &d, &m, &rit, &rit_hz, &xit, &mut ai), None);
        assert_eq!(f.load(Ordering::Relaxed), 7_074_000);
    }

    #[test]
    fn fb_read_mirrors_fa() {
        let (f, p, d, m, rit, rit_hz, xit) = fixture();
        let mut ai = 0;
        assert_eq!(handle_command("FB", &f, &f, &p, &d, &m, &rit, &rit_hz, &xit, &mut ai), Some("FB00014074000;".to_string()));
    }

    #[test]
    fn md_get_set_round_trip() {
        let (f, p, d, m, rit, rit_hz, xit) = fixture();
        let mut ai = 0;
        // Default DemodParams starts at USB (code 2).
        assert_eq!(handle_command("MD", &f, &f, &p, &d, &m, &rit, &rit_hz, &xit, &mut ai), Some("MD2;".to_string()));
        assert_eq!(handle_command("MD3", &f, &f, &p, &d, &m, &rit, &rit_hz, &xit, &mut ai), None); // CWU
        assert_eq!(p.lock().unwrap().mode, Mode::Cwu);
        assert_eq!(handle_command("MD", &f, &f, &p, &d, &m, &rit, &rit_hz, &xit, &mut ai), Some("MD3;".to_string()));
    }

    #[test]
    fn rt_get_set_round_trip() {
        let (f, p, d, m, rit, rit_hz, xit) = fixture();
        let mut ai = 0;
        assert_eq!(handle_command("RT", &f, &f, &p, &d, &m, &rit, &rit_hz, &xit, &mut ai), Some("RT0;".to_string()));
        assert_eq!(handle_command("RT1", &f, &f, &p, &d, &m, &rit, &rit_hz, &xit, &mut ai), None);
        assert!(rit.load(Ordering::Relaxed));
        assert_eq!(handle_command("RT", &f, &f, &p, &d, &m, &rit, &rit_hz, &xit, &mut ai), Some("RT1;".to_string()));
    }

    #[test]
    fn rc_clears_rit_offset() {
        let (f, p, d, m, rit, rit_hz, xit) = fixture();
        let mut ai = 0;
        rit_hz.store(250, Ordering::Relaxed);
        assert_eq!(handle_command("RC", &f, &f, &p, &d, &m, &rit, &rit_hz, &xit, &mut ai), None);
        assert_eq!(rit_hz.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn rd_ru_step_and_absolute_set() {
        let (f, p, d, m, rit, rit_hz, xit) = fixture();
        let mut ai = 0;
        // Default mode (USB) steps by 50Hz.
        assert_eq!(handle_command("RU", &f, &f, &p, &d, &m, &rit, &rit_hz, &xit, &mut ai), None);
        assert_eq!(rit_hz.load(Ordering::Relaxed), 50);
        assert_eq!(handle_command("RD", &f, &f, &p, &d, &m, &rit, &rit_hz, &xit, &mut ai), None);
        assert_eq!(rit_hz.load(Ordering::Relaxed), 0);
        // CW modes step by 10Hz instead of 50Hz.
        p.lock().unwrap().mode = Mode::Cwl;
        assert_eq!(handle_command("RU", &f, &f, &p, &d, &m, &rit, &rit_hz, &xit, &mut ai), None);
        assert_eq!(rit_hz.load(Ordering::Relaxed), 10);
        // An explicit value is an ABSOLUTE set, negated for RD -- matches
        // piHPSDR's rigctl.c exactly (see handle_command's own comment).
        assert_eq!(
            handle_command("RU00500", &f, &f, &p, &d, &m, &rit, &rit_hz, &xit, &mut ai),
            None
        );
        assert_eq!(rit_hz.load(Ordering::Relaxed), 500);
        assert_eq!(
            handle_command("RD00500", &f, &f, &p, &d, &m, &rit, &rit_hz, &xit, &mut ai),
            None
        );
        assert_eq!(rit_hz.load(Ordering::Relaxed), -500);
    }

    #[test]
    fn xt_get_set_round_trip() {
        let (f, p, d, m, rit, rit_hz, xit) = fixture();
        let mut ai = 0;
        assert_eq!(handle_command("XT", &f, &f, &p, &d, &m, &rit, &rit_hz, &xit, &mut ai), Some("XT0;".to_string()));
        assert_eq!(handle_command("XT1", &f, &f, &p, &d, &m, &rit, &rit_hz, &xit, &mut ai), None);
        assert!(xit.load(Ordering::Relaxed));
        assert_eq!(handle_command("XT", &f, &f, &p, &d, &m, &rit, &rit_hz, &xit, &mut ai), Some("XT1;".to_string()));
    }

    #[test]
    fn tx_rx_drive_mox() {
        let (f, p, d, m, rit, rit_hz, xit) = fixture();
        let mut ai = 0;
        assert_eq!(handle_command("TX", &f, &f, &p, &d, &m, &rit, &rit_hz, &xit, &mut ai), None);
        assert!(m.load(Ordering::Relaxed));
        assert_eq!(handle_command("RX", &f, &f, &p, &d, &m, &rit, &rit_hz, &xit, &mut ai), None);
        assert!(!m.load(Ordering::Relaxed));
    }

    #[test]
    fn if_response_has_expected_length_and_frequency() {
        let (f, p, _d, m, rit, rit_hz, xit) = fixture();
        let resp = build_if_response(&f, &p, &m, &rit, &rit_hz, &xit);
        assert!(resp.starts_with("IF00014074000"), "{resp}");
        assert!(resp.ends_with(';'));
        // "IF" + freq(11) + step(4) + rit(6) + 10 single-digit fields +
        // 2 two-digit fields + ";" = 2 + 11 + 4 + 6 + 10 + 4 + 1 = 38
        assert_eq!(resp.len(), 38);
    }

    #[test]
    fn sm_reports_s9_as_midscale() {
        let (f, p, d, m, rit, rit_hz, xit) = fixture();
        let mut ai = 0;
        // fixture() sets meter_db to -73 (S9); piHPSDR's formula maps
        // that to roughly the middle of the 0-30 scale.
        let resp = handle_command("SM0", &f, &f, &p, &d, &m, &rit, &rit_hz, &xit, &mut ai).unwrap();
        assert!(resp.starts_with("SM0"), "{resp}");
        assert!(resp.ends_with(';'));
    }

    #[test]
    fn ai_get_set_round_trip() {
        let (f, p, d, m, rit, rit_hz, xit) = fixture();
        let mut ai = 0;
        assert_eq!(handle_command("AI", &f, &f, &p, &d, &m, &rit, &rit_hz, &xit, &mut ai), Some("AI0;".to_string()));
        assert_eq!(handle_command("AI2", &f, &f, &p, &d, &m, &rit, &rit_hz, &xit, &mut ai), None);
        assert_eq!(ai, 2);
        assert_eq!(handle_command("AI", &f, &f, &p, &d, &m, &rit, &rit_hz, &xit, &mut ai), Some("AI2;".to_string()));
    }

    #[test]
    fn ps_read_always_reports_on_and_set_is_ignored() {
        let (f, p, d, m, rit, rit_hz, xit) = fixture();
        let mut ai = 0;
        assert_eq!(handle_command("PS", &f, &f, &p, &d, &m, &rit, &rit_hz, &xit, &mut ai), Some("PS1;".to_string()));
        // "PS0;" (power off) is deliberately a no-op, not a shutdown --
        // see this module's doc comment.
        assert_eq!(handle_command("PS0", &f, &f, &p, &d, &m, &rit, &rit_hz, &xit, &mut ai), None);
    }

    #[test]
    fn unknown_command_gets_question_mark() {
        let (f, p, d, m, rit, rit_hz, xit) = fixture();
        let mut ai = 0;
        assert_eq!(handle_command("ZZ", &f, &f, &p, &d, &m, &rit, &rit_hz, &xit, &mut ai), Some("?;".to_string()));
    }
}
