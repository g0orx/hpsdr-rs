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
    ID, IF, FA, FB, FR, FT, MD, AI, PS, TX, RX, SM, TY, RT, RC, RD, RU,
    XT, KY. Deliberately NOT implemented (unlike the reference) because
    this app has no clean value/range to back them with yet, matching
    rigctl.rs's own "respond unsupported rather than a made-up value"
    philosophy:
      - PC (drive/power level): max_tx_power_watts isn't currently
        shared via an Arc the way frequency/mox are, and the reference's
        0-100 scale doesn't map cleanly onto watts without it.
      - MG (mic gain): this app's mic_gain is a linear 0.0-8.0
        multiplier, not the reference's -12..+50 dB range -- no honest
        conversion between the two.
      - KS (CW keyer speed): KY (CW keyer send -- see this file's own
        "KY" match arm) IS implemented, feeding tx::TxHandle's "send CW
        text" machinery (see main.rs's own reconciliation of
        cw_remote_pending/cw_remote_busy/cw_remote_stop -- this server
        thread can't reach TxHandle directly, same "write intent into a
        shared cell, let main.rs's frame loop drive the real call"
        pattern requested_frequency_hz already uses for FA). KS itself
        (setting the keyer's SPEED via CAT) isn't -- KY always sends at
        whatever Settings -> CW's own Speed/Weight are currently set to,
        matching how the main window's own Send button already works;
        a logger wanting a different speed would need to change that
        setting directly, not via CAT.
      - Split (FT accepted but always reports/treats as off), CTCSS,
        memory channels, SAT mode: none of these have any backing state
        in this app. RIT/XIT DO now (RT/RC/RD/RU/XT, and the IF
        response's rit/rit_en/xit_en fields) -- see RadioSession::
        rit_enabled's doc comment (radio.rs). No command to set an
        absolute XIT value exists in real Kenwood CAT either, only XT's
        on/off toggle -- matches the reference exactly.
    AI (auto-reporting) is real: with AI1 or higher the client is sent
    "FA...;" whenever the frequency changes, and with AI2 or higher
    "MD...;" whenever the mode changes, checked every 200 ms -- the same
    as deskHPSDR's autoreport_handler (FA/FB/MD, not IF). A serial port
    with "Auto report" ticked starts at AI2 without being asked, for
    devices that only listen (an amplifier or band decoder that never
    sends a command), and sends the current frequency and mode as soon
    as the port opens.

    Serial ports: the same commands can also be served on a serial port
    (CatSerialPort below; Settings -> Network, "Serial 1-3"), for an
    amplifier, ATU or band decoder that follows the radio's band over a
    Kenwood CAT cable, or a logger on a COM port (real or virtual, e.g.
    a com0com pair on Windows). 8 data bits, no parity, 1 stop bit, no
    flow control, as deskHPSDR's launch_serial_rigctl. A port that can't
    be opened, or goes away (a USB adapter unplugged), is retried every
    2 s until it's stopped. Per port: "TX frequency" makes FA/IF report
    the frequency transmitted on (CatShared::tx_rf_frequency_hz) rather
    than the RX one, so an amplifier follows Split/XIT; "PTT out" raises
    RTS or DTR while transmitting; "PTT in" keys the radio on CTS or DSR
    (run_ptt_lines).

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
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub const DEFAULT_ADDR: &str = "0.0.0.0:19090";

/// Serial ports offered in Settings -> Network, as deskHPSDR's MAX_SERIAL.
pub const SERIAL_PORTS: usize = 3;
/// Baud rates offered for a serial port (deskHPSDR offers 4800-38400;
/// some amplifiers' CAT inputs also do 57600/115200).
pub const SERIAL_BAUD_RATES: [u32; 6] = [4800, 9600, 19200, 38400, 57600, 115200];
pub const DEFAULT_SERIAL_BAUD: u32 = 9600;

/// How often an auto-reporting client is checked for a changed
/// frequency or mode.
const AUTO_REPORT_INTERVAL: Duration = Duration::from_millis(200);

/// The radio state every CAT client reads and changes -- the TCP
/// server's clients and each serial port alike. See CatServer::start's
/// parameters for what each one is.
#[derive(Clone)]
pub struct CatShared {
    pub requested_frequency_hz: Arc<AtomicU64>,
    pub rx_frequency_hz: Arc<AtomicU64>,
    /// The frequency transmitted on, in the same RF terms as
    /// rx_frequency_hz (through a transverter, its RF side): VFO B with
    /// Split/Duplex, plus XIT and CW pitch -- see main.rs's per-frame
    /// tx_dial_freq_hz. A serial port with "Report TX frequency" reports
    /// this as FA/IF, so an amplifier is on the band actually
    /// transmitted on.
    pub tx_rf_frequency_hz: Arc<AtomicU64>,
    pub mox: Arc<AtomicBool>,
    pub tx_frequency_hz: Arc<AtomicU32>,
    pub allow_out_of_band_tx: Arc<AtomicBool>,
    pub rit_enabled: Arc<AtomicBool>,
    pub rit_offset_hz: Arc<AtomicI32>,
    pub xit_enabled: Arc<AtomicBool>,
    pub cw_remote_pending: Arc<Mutex<VecDeque<String>>>,
    pub cw_remote_busy: Arc<AtomicBool>,
}

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
        // What clients read and change -- see CatShared. In it:
        // requested_frequency_hz is where FA's "set" form writes its
        // request -- NOT the raw hardware frequency (see
        // RadioSession::requested_frequency_hz's doc comment: main.rs's
        // own per-frame loop reconciles this, since CTUN state lives in
        // the UI layer); rx_frequency_hz is what FA/IF report, so a
        // CTUN'd listen frequency is reported, not the parked hardware
        // LO; tx_frequency_hz/allow_out_of_band_tx are checked before
        // "TX" is honored (main.rs's tx_frequency_allowed); rit_*/
        // xit_enabled back RT/RC/RD/RU and XT (no xit offset: Kenwood
        // CAT has no command to set an absolute XIT value); cw_remote_*
        // back "KY", shared with rigctl.rs's send_morse/stop_morse.
        shared: CatShared,
        demod_params: Arc<Mutex<DemodParams>>,
        display: Arc<Mutex<SpectrumDisplay>>,
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
                        let _ = stream.set_nodelay(true);
                        let writer = match stream.try_clone() {
                            Ok(s) => s,
                            Err(_) => continue,
                        };
                        let conn_shared = shared.clone();
                        let params = Arc::clone(&accept_demod_params);
                        let disp = Arc::clone(&accept_display);
                        let conn_stop = Arc::clone(&accept_stop);
                        let conn_connected = Arc::clone(&accept_connected);
                        let conn_logging = accept_logging.clone();
                        let handle = thread::spawn(move || {
                            conn_connected.fetch_add(1, Ordering::Relaxed);
                            // Logged without a prefix, as before serial
                            // ports existed.
                            handle_client(
                                stream, writer, "", 0, false, &conn_shared, &params, &disp, &conn_stop,
                                &conn_logging,
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

/// Serves one CAT client -- a TCP connection or an open serial port --
/// until it goes away, a read or write fails, or `stop` is set.
/// `reader` must time out (TCP read timeout / serial port timeout) so
/// `stop` and auto-reporting are checked even when nothing arrives.
/// `label` prefixes this client's log lines (the port name for a
/// serial port, empty for TCP); `initial_auto_reporting` is the AI level
/// it starts at (0 = only answer commands). `report_tx`: FA/IF and
/// auto-reports give the TX frequency rather than the RX one (see
/// CatShared::tx_rf_frequency_hz).
fn handle_client<R: Read, W: Write>(
    mut reader: R,
    mut writer: W,
    label: &str,
    initial_auto_reporting: u8,
    report_tx: bool,
    shared: &CatShared,
    demod_params: &DemodParamsCell,
    display: &DisplayCell,
    stop: &AtomicBool,
    logging: &DebugLog,
) {
    let prefix = if label.is_empty() { String::new() } else { format!("{label}: ") };
    let mut read_buf = [0u8; 1024];
    // Accumulates bytes across reads until a full ';'-terminated
    // command is available -- a real command CAN arrive split across
    // TCP packet boundaries (e.g. a slow/throttled client), or a few
    // bytes at a time from a serial port, unlike rigctl.rs's
    // newline-delimited protocol where read_line already handles this
    // for free.
    let mut pending = String::new();
    // Per-client AI (auto-reporting) level -- see this module's doc
    // comment.
    let mut auto_reporting: u8 = initial_auto_reporting;
    let mut auto_report = AutoReport::default();
    let reported_frequency_hz =
        if report_tx { &shared.tx_rf_frequency_hz } else { &shared.rx_frequency_hz };

    while !stop.load(Ordering::Relaxed) {
        match reader.read(&mut read_buf) {
            Ok(0) => {
                logging.log(&format!("{prefix}client closed the connection"));
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
                    logging.log(&format!("{prefix}<< {cmd};"));
                    let current_params = demod_params.lock().unwrap().clone();
                    let current_display = display.lock().unwrap().clone();
                    if let Some(response) = handle_command(
                        cmd,
                        &shared.requested_frequency_hz,
                        reported_frequency_hz,
                        &current_params,
                        &current_display,
                        &shared.mox,
                        &shared.tx_frequency_hz,
                        &shared.allow_out_of_band_tx,
                        &shared.rit_enabled,
                        &shared.rit_offset_hz,
                        &shared.xit_enabled,
                        &mut auto_reporting,
                        &shared.cw_remote_pending,
                        &shared.cw_remote_busy,
                    ) {
                        logging.log(&format!("{prefix}>> {response}"));
                        if writer.write_all(response.as_bytes()).and_then(|_| writer.flush()).is_err() {
                            return;
                        }
                    }
                }
                // A device sending something that never has a ';'
                // (line noise, a wrong baud rate) mustn't grow this
                // forever.
                if pending.len() > 4096 {
                    pending.clear();
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => {}
            // See rigctl.rs/tci.rs's identical arm's doc comment -- same
            // read-timeout pattern.
            Err(e) => {
                logging.log(&format!("{prefix}connection error, closing: {e:?}"));
                break;
            }
        }
        if auto_reporting > 0 {
            let mode = demod_params.lock().unwrap().lock().unwrap().mode;
            let reports = auto_report.due(
                auto_reporting,
                reported_frequency_hz.load(Ordering::Relaxed),
                mode,
                Instant::now(),
            );
            for report in reports {
                logging.log(&format!("{prefix}>> {report}"));
                if writer.write_all(report.as_bytes()).and_then(|_| writer.flush()).is_err() {
                    return;
                }
            }
        } else {
            // Turned off (AI0;): report afresh if it's turned on again.
            auto_report = AutoReport::default();
        }
    }
}

/// What an auto-reporting client was last sent (see this module's doc
/// comment on AI): None = nothing yet, so the first check reports the
/// current values.
#[derive(Default)]
struct AutoReport {
    last_check: Option<Instant>,
    last_frequency_hz: Option<u64>,
    last_mode: Option<u8>,
}

impl AutoReport {
    /// The reports to send now for AI level `level`: FA when the
    /// frequency has changed (level 1+), MD when the mode has (level 2+),
    /// at most every AUTO_REPORT_INTERVAL.
    fn due(&mut self, level: u8, frequency_hz: u64, mode: Mode, now: Instant) -> Vec<String> {
        let mut reports = Vec::new();
        if level == 0 || self.last_check.is_some_and(|t| now.duration_since(t) < AUTO_REPORT_INTERVAL) {
            return reports;
        }
        self.last_check = Some(now);
        if self.last_frequency_hz != Some(frequency_hz) {
            self.last_frequency_hz = Some(frequency_hz);
            reports.push(format!("FA{frequency_hz:011};"));
        }
        let code = mode_to_ts2000(mode);
        if level >= 2 && self.last_mode != Some(code) {
            self.last_mode = Some(code);
            reports.push(format!("MD{code};"));
        }
        reports
    }
}

/// A modem-control line hpsdr-rs raises while transmitting, e.g. to key
/// an amplifier through a USB-serial adapter.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum PttOut {
    #[default]
    Off,
    Rts,
    Dtr,
}

/// A modem-control input that keys the radio while it's raised: a
/// footswitch, or a program set to "PTT via RTS/DTR" on the other end
/// of a virtual serial pair (its RTS arrives here as CTS, its DTR as
/// DSR).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum PttIn {
    #[default]
    Off,
    Cts,
    Dsr,
}

impl PttOut {
    pub const ALL: [PttOut; 3] = [PttOut::Off, PttOut::Rts, PttOut::Dtr];
    pub fn label(self) -> &'static str {
        match self {
            PttOut::Off => "Off",
            PttOut::Rts => "RTS",
            PttOut::Dtr => "DTR",
        }
    }
}

impl PttIn {
    pub const ALL: [PttIn; 3] = [PttIn::Off, PttIn::Cts, PttIn::Dsr];
    pub fn label(self) -> &'static str {
        match self {
            PttIn::Off => "Off",
            PttIn::Cts => "CTS",
            PttIn::Dsr => "DSR",
        }
    }
}

/// How one serial port is set up (Settings -> Network).
#[derive(Clone, Debug, PartialEq)]
pub struct SerialOptions {
    /// e.g. "/dev/ttyUSB0", "COM3".
    pub port: String,
    pub baud: u32,
    /// Start at AI2 (send FA/MD on changes without being asked) -- for
    /// a device that only listens.
    pub auto_report: bool,
    /// FA/IF give the TX frequency, not the RX one -- see
    /// CatShared::tx_rf_frequency_hz. On by default: what an amplifier
    /// needs, and the same as the RX frequency without Split/XIT.
    pub report_tx: bool,
    pub ptt_out: PttOut,
    pub ptt_in: PttIn,
}

impl Default for SerialOptions {
    fn default() -> Self {
        Self {
            port: String::new(),
            baud: DEFAULT_SERIAL_BAUD,
            auto_report: false,
            report_tx: true,
            ptt_out: PttOut::Off,
            ptt_in: PttIn::Off,
        }
    }
}

/// One of Settings -> Network's CAT serial ports, as configured, and
/// running or not.
#[derive(Default)]
pub struct CatSerialSlot {
    pub options: SerialOptions,
    pub running: Option<CatSerialPort>,
}

/// The state of a running serial port, for Settings -> Network.
#[derive(Clone, Debug, PartialEq)]
pub enum SerialState {
    Opening,
    Open,
    /// Couldn't open, or lost: why. Retried every 2 s.
    Failed(String),
}

/// How often the PTT lines are looked at. Keying an amplifier late
/// means RF reaches it before its relay closes, so this is short.
const PTT_POLL_INTERVAL: Duration = Duration::from_millis(2);

/// Kenwood CAT on a serial port: opens it (retrying while it can't be
/// opened or is lost) and serves it as a CAT client would be served over
/// TCP, until stopped or dropped. With PTT lines set, a second thread
/// drives/watches them while the port is open (run_ptt_lines).
pub struct CatSerialPort {
    demod_params: DemodParamsCell,
    display: DisplayCell,
    stop: Arc<AtomicBool>,
    state: Arc<Mutex<SerialState>>,
    thread: Option<JoinHandle<()>>,
}

impl CatSerialPort {
    pub fn start(
        options: SerialOptions,
        shared: CatShared,
        demod_params: Arc<Mutex<DemodParams>>,
        display: Arc<Mutex<SpectrumDisplay>>,
        logging: DebugLog,
    ) -> Self {
        let demod_params: DemodParamsCell = Arc::new(Mutex::new(demod_params));
        let display: DisplayCell = Arc::new(Mutex::new(display));
        let stop = Arc::new(AtomicBool::new(false));
        let state = Arc::new(Mutex::new(SerialState::Opening));
        let port = options.port.trim().to_string();
        let baud = options.baud;
        let thread_params = Arc::clone(&demod_params);
        let thread_display = Arc::clone(&display);
        let thread_stop = Arc::clone(&stop);
        let thread_state = Arc::clone(&state);
        let thread = thread::spawn(move || {
            let initial_ai = if options.auto_report { 2 } else { 0 };
            let mut last_error = String::new();
            while !thread_stop.load(Ordering::Relaxed) {
                match open_serial(&port, baud) {
                    Ok((reader, writer)) => {
                        println!("cat: serial port {port} open at {baud} baud");
                        logging.log(&format!("{port}: open at {baud} baud"));
                        last_error.clear();
                        *thread_state.lock().unwrap() = SerialState::Open;
                        let lines_stop = Arc::new(AtomicBool::new(false));
                        let lines = if options.ptt_out != PttOut::Off || options.ptt_in != PttIn::Off {
                            match writer.try_clone() {
                                Ok(lines_port) => {
                                    let lines_stop = Arc::clone(&lines_stop);
                                    let lines_shared = shared.clone();
                                    let lines_logging = logging.clone();
                                    let lines_label = port.clone();
                                    let (ptt_out, ptt_in) = (options.ptt_out, options.ptt_in);
                                    Some(thread::spawn(move || {
                                        run_ptt_lines(
                                            lines_port, ptt_out, ptt_in, &lines_shared, &lines_stop,
                                            &lines_label, &lines_logging,
                                        )
                                    }))
                                }
                                Err(e) => {
                                    logging.log(&format!("{port}: PTT lines unavailable: {e}"));
                                    None
                                }
                            }
                        } else {
                            None
                        };
                        handle_client(
                            reader, writer, &port, initial_ai, options.report_tx, &shared, &thread_params,
                            &thread_display, &thread_stop, &logging,
                        );
                        lines_stop.store(true, Ordering::SeqCst);
                        if let Some(t) = lines {
                            let _ = t.join();
                        }
                        if thread_stop.load(Ordering::Relaxed) {
                            break;
                        }
                        *thread_state.lock().unwrap() = SerialState::Failed("lost; reopening".to_string());
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        // Logged once per distinct error, not every retry.
                        if msg != last_error {
                            eprintln!("cat: couldn't open serial port {port}: {msg}");
                            logging.log(&format!("{port}: couldn't open: {msg}"));
                            last_error = msg.clone();
                        }
                        *thread_state.lock().unwrap() = SerialState::Failed(msg);
                    }
                }
                for _ in 0..20 {
                    if thread_stop.load(Ordering::Relaxed) {
                        break;
                    }
                    thread::sleep(Duration::from_millis(100));
                }
            }
            logging.log(&format!("{port}: closed"));
        });
        Self { demod_params, display, stop, state, thread: Some(thread) }
    }

    /// See CatServer::set_demod_params.
    pub fn set_demod_params(&self, new_demod_params: Arc<Mutex<DemodParams>>) {
        *self.demod_params.lock().unwrap() = new_demod_params;
    }

    pub fn set_display(&self, new_display: Arc<Mutex<SpectrumDisplay>>) {
        *self.display.lock().unwrap() = new_display;
    }

    pub fn state(&self) -> SerialState {
        self.state.lock().unwrap().clone()
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for CatSerialPort {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Drives `ptt_out` from the radio's transmit state, and keys/unkeys
/// the radio on `ptt_in`'s edges, until `stop` is set or the port fails.
///
/// PTT in is edge-triggered: raising the line keys (if the frequency may
/// be transmitted on -- main.rs's tx_frequency_allowed, as CAT's TX;),
/// dropping it unkeys, but only if this line keyed it -- so the on-screen
/// MOX, or another client, isn't overridden by a line that simply sits
/// high or low, or rises while already transmitting. If the radio is unkeyed some other way while the line is
/// still up, it stays unkeyed until the line drops and rises again. On
/// leaving (stopped, or the port lost), a transmission this line started
/// is ended, and the PTT out line is dropped.
fn run_ptt_lines(
    mut port: impl ModemLines,
    ptt_out: PttOut,
    ptt_in: PttIn,
    shared: &CatShared,
    stop: &AtomicBool,
    label: &str,
    logging: &DebugLog,
) {
    let mut out_state: Option<bool> = None;
    // None until first read, so a line already up when the port opens
    // doesn't key the radio.
    let mut in_state: Option<bool> = None;
    let mut keyed_by_line = false;
    while !stop.load(Ordering::Relaxed) {
        let transmitting = shared.mox.load(Ordering::Relaxed);
        if ptt_out != PttOut::Off && out_state != Some(transmitting) {
            if let Err(e) = port.set_out(ptt_out, transmitting) {
                logging.log(&format!("{label}: couldn't set {}: {e}", ptt_out.label()));
                break;
            }
            out_state = Some(transmitting);
        }
        if keyed_by_line && !transmitting {
            // Unkeyed elsewhere: don't key again until the next rise.
            keyed_by_line = false;
        }
        if ptt_in != PttIn::Off {
            let up = match port.read_in(ptt_in) {
                Ok(up) => up,
                Err(e) => {
                    logging.log(&format!("{label}: couldn't read {}: {e}", ptt_in.label()));
                    break;
                }
            };
            match (in_state, up) {
                // Already transmitting (MOX button, another client): left
                // to whatever keyed it.
                (Some(false), true) if transmitting => {}
                (Some(false), true) => {
                    if crate::tx_frequency_allowed(
                        shared.tx_frequency_hz.load(Ordering::Relaxed),
                        shared.allow_out_of_band_tx.load(Ordering::Relaxed),
                    ) {
                        logging.log(&format!("{label}: {} up, TX", ptt_in.label()));
                        shared.mox.store(true, Ordering::Relaxed);
                        keyed_by_line = true;
                    } else {
                        logging.log(&format!("{label}: {} up, TX refused: out of band", ptt_in.label()));
                    }
                }
                (Some(true), false) => {
                    logging.log(&format!("{label}: {} down, RX", ptt_in.label()));
                    if keyed_by_line {
                        shared.mox.store(false, Ordering::Relaxed);
                        keyed_by_line = false;
                    }
                }
                _ => {}
            }
            in_state = Some(up);
        }
        thread::sleep(PTT_POLL_INTERVAL);
    }
    if keyed_by_line {
        shared.mox.store(false, Ordering::Relaxed);
    }
    if ptt_out != PttOut::Off {
        let _ = port.set_out(ptt_out, false);
    }
}

/// A serial port's modem-control lines, as run_ptt_lines uses them (a
/// trait so its logic can be tested without a serial port: a
/// pseudo-terminal has no such lines).
trait ModemLines {
    fn set_out(&mut self, line: PttOut, on: bool) -> serialport::Result<()>;
    fn read_in(&mut self, line: PttIn) -> serialport::Result<bool>;
}

impl ModemLines for Box<dyn serialport::SerialPort> {
    fn set_out(&mut self, line: PttOut, on: bool) -> serialport::Result<()> {
        match line {
            PttOut::Rts => self.write_request_to_send(on),
            PttOut::Dtr => self.write_data_terminal_ready(on),
            PttOut::Off => Ok(()),
        }
    }

    fn read_in(&mut self, line: PttIn) -> serialport::Result<bool> {
        match line {
            PttIn::Cts => self.read_clear_to_send(),
            PttIn::Dsr => self.read_data_set_ready(),
            PttIn::Off => Ok(false),
        }
    }
}

type SerialHalves = (Box<dyn serialport::SerialPort>, Box<dyn serialport::SerialPort>);

fn open_serial(port: &str, baud: u32) -> serialport::Result<SerialHalves> {
    if port.is_empty() {
        return Err(serialport::Error::new(serialport::ErrorKind::InvalidInput, "no port name"));
    }
    let reader = serialport::new(port, baud)
        .data_bits(serialport::DataBits::Eight)
        .parity(serialport::Parity::None)
        .stop_bits(serialport::StopBits::One)
        .flow_control(serialport::FlowControl::None)
        .timeout(Duration::from_millis(250))
        .open()?;
    let writer = reader.try_clone()?;
    Ok((reader, writer))
}

/// The serial ports this computer has, for Settings -> Network's port
/// list -- names only, sorted. Empty if they can't be listed.
///
/// On Linux, serialport (built without libudev) lists every legacy
/// /dev/ttyS* slot the kernel reserves, usually 32 with no UART behind
/// them; those whose sysfs `type` is 0 (PORT_UNKNOWN: no UART) are left
/// out. /dev/serial/by-id names are added first: they stay the same
/// whichever order USB adapters are plugged in, unlike /dev/ttyUSB0/1.
pub fn serial_port_names() -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    #[cfg(target_os = "linux")]
    if let Ok(entries) = std::fs::read_dir("/dev/serial/by-id") {
        let mut by_id: Vec<String> =
            entries.flatten().map(|e| e.path().to_string_lossy().into_owned()).collect();
        by_id.sort();
        names.extend(by_id);
    }
    let mut ports: Vec<String> = serialport::available_ports()
        .map(|ports| ports.into_iter().map(|p| p.port_name).collect())
        .unwrap_or_default();
    #[cfg(target_os = "linux")]
    ports.retain(|name| {
        let Some(base) = name.strip_prefix("/dev/").filter(|b| b.starts_with("ttyS")) else {
            return true;
        };
        std::fs::read_to_string(format!("/sys/class/tty/{base}/type")).map_or(true, |t| t.trim() != "0")
    });
    ports.sort();
    ports.dedup();
    names.extend(ports);
    names
}

/// `cmd` is one command with the trailing ';' already stripped (e.g.
/// "FA" or "FA00014074000"). Returns the reply to send (WITHOUT a
/// trailing ';' -- callers add it), or `None` to send nothing at all
/// (the normal case for a "set" command -- see this module's doc
/// comment on Kenwood CAT's request/reply asymmetry).
fn handle_command(
    cmd: &str,
    requested_frequency_hz: &Arc<AtomicU64>,
    rx_frequency_hz: &Arc<AtomicU64>,
    demod_params: &Arc<Mutex<DemodParams>>,
    display: &Arc<Mutex<SpectrumDisplay>>,
    mox: &Arc<AtomicBool>,
    // See main.rs's tx_frequency_allowed doc comment -- real request, a
    // safety default against accidentally transmitting outside the ham
    // bands. Checked before "TX" is honored, same as every other PTT
    // path in this project.
    tx_frequency_hz: &Arc<AtomicU32>,
    allow_out_of_band_tx: &Arc<AtomicBool>,
    rit_enabled: &Arc<AtomicBool>,
    rit_offset_hz: &Arc<AtomicI32>,
    xit_enabled: &Arc<AtomicBool>,
    auto_reporting: &mut u8,
    // "KY" -- see this module's own doc comment on why CW text sending
    // is reconciled through main.rs rather than reaching tx::TxHandle
    // directly from this server thread.
    cw_remote_pending: &Arc<Mutex<VecDeque<String>>>,
    cw_remote_busy: &Arc<AtomicBool>,
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
            } else if let Ok(f) = suffix.parse::<u64>() {
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
        // CW keyer send -- see this module's own doc comment. Real
        // Kenwood TS-2000: "KY;" (bare) queries the keyer buffer
        // (KY0; = ready for more, KY1; = still busy sending); "KY1
        // <text>;" (the "1" is a fixed marker, not a flag -- some
        // clients send "KY0<text>;" instead, so both are accepted the
        // same way) queues that text to be sent as CW at the rig's
        // own configured speed. Longer messages arrive as several
        // successive KY-set commands (the real TS-2000's own buffer
        // field is a fixed 24 characters) -- see tx::TxHandle::
        // queue_cw_text's own doc comment for how those get appended
        // rather than replacing whatever's already in flight.
        "KY" => {
            if suffix.is_empty() {
                let busy = cw_remote_busy.load(Ordering::Relaxed);
                Some(if busy { "KY1;".to_string() } else { "KY0;".to_string() })
            } else {
                // Strip the leading 0/1 marker if present, and trim
                // trailing padding (a fixed-width field on real
                // hardware) so it doesn't insert a spurious extra word
                // gap between this chunk and whatever's sent next.
                let text = suffix.strip_prefix(['0', '1']).unwrap_or(suffix).trim_end();
                if !text.is_empty() {
                    cw_remote_pending.lock().unwrap().push_back(text.to_string());
                }
                None
            }
        }
        "TX" => {
            if suffix.is_empty()
                && crate::tx_frequency_allowed(
                    tx_frequency_hz.load(Ordering::Relaxed),
                    allow_out_of_band_tx.load(Ordering::Relaxed),
                )
            {
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
    frequency_hz: &Arc<AtomicU64>,
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

    /// tx_frequency_hz stays u32 (a real hardware frequency) while the
    /// RF-space requested/rx values are u64 -- a TX copy of `f`.
    fn tx_of(f: &Arc<AtomicU64>) -> Arc<AtomicU32> {
        Arc::new(AtomicU32::new(f.load(Ordering::Relaxed) as u32))
    }

    fn fixture() -> (
        Arc<AtomicU64>,
        Arc<Mutex<DemodParams>>,
        Arc<Mutex<SpectrumDisplay>>,
        Arc<AtomicBool>,
        Arc<AtomicBool>,
        Arc<AtomicI32>,
        Arc<AtomicBool>,
        Arc<Mutex<VecDeque<String>>>,
        Arc<AtomicBool>,
    ) {
        let frequency_hz = Arc::new(AtomicU64::new(14_074_000));
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
        let cw_remote_pending = Arc::new(Mutex::new(VecDeque::new()));
        let cw_remote_busy = Arc::new(AtomicBool::new(false));
        (
            frequency_hz, demod_params, display, mox, rit_enabled, rit_offset_hz, xit_enabled,
            cw_remote_pending, cw_remote_busy,
        )
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
        let (f, p, d, m, rit, rit_hz, xit, cw_pending, cw_busy) = fixture();
        let mut ai = 0;
        assert_eq!(handle_command("ID", &f, &f, &p, &d, &m, &tx_of(&f), &Arc::new(AtomicBool::new(true)), &rit, &rit_hz, &xit, &mut ai, &cw_pending, &cw_busy), Some("ID019;".to_string()));
    }

    #[test]
    fn fa_read_reflects_current_frequency() {
        let (f, p, d, m, rit, rit_hz, xit, cw_pending, cw_busy) = fixture();
        let mut ai = 0;
        assert_eq!(handle_command("FA", &f, &f, &p, &d, &m, &tx_of(&f), &Arc::new(AtomicBool::new(true)), &rit, &rit_hz, &xit, &mut ai, &cw_pending, &cw_busy), Some("FA00014074000;".to_string()));
    }

    #[test]
    fn fa_set_updates_frequency_and_sends_no_reply() {
        let (f, p, d, m, rit, rit_hz, xit, cw_pending, cw_busy) = fixture();
        let mut ai = 0;
        assert_eq!(handle_command("FA00007074000", &f, &f, &p, &d, &m, &tx_of(&f), &Arc::new(AtomicBool::new(true)), &rit, &rit_hz, &xit, &mut ai, &cw_pending, &cw_busy), None);
        assert_eq!(f.load(Ordering::Relaxed), 7_074_000);
    }

    /// FA's 11 digits reach past u32 -- a QO-100 transverter's RF
    /// (10.4895GHz) must round-trip intact, not wrap.
    #[test]
    fn fa_round_trips_microwave_transverter_frequency() {
        let (f, p, d, m, rit, rit_hz, xit, cw_pending, cw_busy) = fixture();
        let mut ai = 0;
        assert_eq!(handle_command("FA10489500000", &f, &f, &p, &d, &m, &tx_of(&f), &Arc::new(AtomicBool::new(true)), &rit, &rit_hz, &xit, &mut ai, &cw_pending, &cw_busy), None);
        assert_eq!(f.load(Ordering::Relaxed), 10_489_500_000);
        assert_eq!(handle_command("FA", &f, &f, &p, &d, &m, &tx_of(&f), &Arc::new(AtomicBool::new(true)), &rit, &rit_hz, &xit, &mut ai, &cw_pending, &cw_busy), Some("FA10489500000;".to_string()));
    }

    #[test]
    fn fb_read_mirrors_fa() {
        let (f, p, d, m, rit, rit_hz, xit, cw_pending, cw_busy) = fixture();
        let mut ai = 0;
        assert_eq!(handle_command("FB", &f, &f, &p, &d, &m, &tx_of(&f), &Arc::new(AtomicBool::new(true)), &rit, &rit_hz, &xit, &mut ai, &cw_pending, &cw_busy), Some("FB00014074000;".to_string()));
    }

    #[test]
    fn md_get_set_round_trip() {
        let (f, p, d, m, rit, rit_hz, xit, cw_pending, cw_busy) = fixture();
        let mut ai = 0;
        // Default DemodParams starts at USB (code 2).
        assert_eq!(handle_command("MD", &f, &f, &p, &d, &m, &tx_of(&f), &Arc::new(AtomicBool::new(true)), &rit, &rit_hz, &xit, &mut ai, &cw_pending, &cw_busy), Some("MD2;".to_string()));
        assert_eq!(handle_command("MD3", &f, &f, &p, &d, &m, &tx_of(&f), &Arc::new(AtomicBool::new(true)), &rit, &rit_hz, &xit, &mut ai, &cw_pending, &cw_busy), None); // CWU
        assert_eq!(p.lock().unwrap().mode, Mode::Cwu);
        assert_eq!(handle_command("MD", &f, &f, &p, &d, &m, &tx_of(&f), &Arc::new(AtomicBool::new(true)), &rit, &rit_hz, &xit, &mut ai, &cw_pending, &cw_busy), Some("MD3;".to_string()));
    }

    #[test]
    fn rt_get_set_round_trip() {
        let (f, p, d, m, rit, rit_hz, xit, cw_pending, cw_busy) = fixture();
        let mut ai = 0;
        assert_eq!(handle_command("RT", &f, &f, &p, &d, &m, &tx_of(&f), &Arc::new(AtomicBool::new(true)), &rit, &rit_hz, &xit, &mut ai, &cw_pending, &cw_busy), Some("RT0;".to_string()));
        assert_eq!(handle_command("RT1", &f, &f, &p, &d, &m, &tx_of(&f), &Arc::new(AtomicBool::new(true)), &rit, &rit_hz, &xit, &mut ai, &cw_pending, &cw_busy), None);
        assert!(rit.load(Ordering::Relaxed));
        assert_eq!(handle_command("RT", &f, &f, &p, &d, &m, &tx_of(&f), &Arc::new(AtomicBool::new(true)), &rit, &rit_hz, &xit, &mut ai, &cw_pending, &cw_busy), Some("RT1;".to_string()));
    }

    #[test]
    fn rc_clears_rit_offset() {
        let (f, p, d, m, rit, rit_hz, xit, cw_pending, cw_busy) = fixture();
        let mut ai = 0;
        rit_hz.store(250, Ordering::Relaxed);
        assert_eq!(handle_command("RC", &f, &f, &p, &d, &m, &tx_of(&f), &Arc::new(AtomicBool::new(true)), &rit, &rit_hz, &xit, &mut ai, &cw_pending, &cw_busy), None);
        assert_eq!(rit_hz.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn rd_ru_step_and_absolute_set() {
        let (f, p, d, m, rit, rit_hz, xit, cw_pending, cw_busy) = fixture();
        let mut ai = 0;
        // Default mode (USB) steps by 50Hz.
        assert_eq!(handle_command("RU", &f, &f, &p, &d, &m, &tx_of(&f), &Arc::new(AtomicBool::new(true)), &rit, &rit_hz, &xit, &mut ai, &cw_pending, &cw_busy), None);
        assert_eq!(rit_hz.load(Ordering::Relaxed), 50);
        assert_eq!(handle_command("RD", &f, &f, &p, &d, &m, &tx_of(&f), &Arc::new(AtomicBool::new(true)), &rit, &rit_hz, &xit, &mut ai, &cw_pending, &cw_busy), None);
        assert_eq!(rit_hz.load(Ordering::Relaxed), 0);
        // CW modes step by 10Hz instead of 50Hz.
        p.lock().unwrap().mode = Mode::Cwl;
        assert_eq!(handle_command("RU", &f, &f, &p, &d, &m, &tx_of(&f), &Arc::new(AtomicBool::new(true)), &rit, &rit_hz, &xit, &mut ai, &cw_pending, &cw_busy), None);
        assert_eq!(rit_hz.load(Ordering::Relaxed), 10);
        // An explicit value is an ABSOLUTE set, negated for RD -- matches
        // piHPSDR's rigctl.c exactly (see handle_command's own comment).
        assert_eq!(
            handle_command("RU00500", &f, &f, &p, &d, &m, &tx_of(&f), &Arc::new(AtomicBool::new(true)), &rit, &rit_hz, &xit, &mut ai, &cw_pending, &cw_busy),
            None
        );
        assert_eq!(rit_hz.load(Ordering::Relaxed), 500);
        assert_eq!(
            handle_command("RD00500", &f, &f, &p, &d, &m, &tx_of(&f), &Arc::new(AtomicBool::new(true)), &rit, &rit_hz, &xit, &mut ai, &cw_pending, &cw_busy),
            None
        );
        assert_eq!(rit_hz.load(Ordering::Relaxed), -500);
    }

    #[test]
    fn xt_get_set_round_trip() {
        let (f, p, d, m, rit, rit_hz, xit, cw_pending, cw_busy) = fixture();
        let mut ai = 0;
        assert_eq!(handle_command("XT", &f, &f, &p, &d, &m, &tx_of(&f), &Arc::new(AtomicBool::new(true)), &rit, &rit_hz, &xit, &mut ai, &cw_pending, &cw_busy), Some("XT0;".to_string()));
        assert_eq!(handle_command("XT1", &f, &f, &p, &d, &m, &tx_of(&f), &Arc::new(AtomicBool::new(true)), &rit, &rit_hz, &xit, &mut ai, &cw_pending, &cw_busy), None);
        assert!(xit.load(Ordering::Relaxed));
        assert_eq!(handle_command("XT", &f, &f, &p, &d, &m, &tx_of(&f), &Arc::new(AtomicBool::new(true)), &rit, &rit_hz, &xit, &mut ai, &cw_pending, &cw_busy), Some("XT1;".to_string()));
    }

    #[test]
    fn tx_rx_drive_mox() {
        let (f, p, d, m, rit, rit_hz, xit, cw_pending, cw_busy) = fixture();
        let mut ai = 0;
        assert_eq!(handle_command("TX", &f, &f, &p, &d, &m, &tx_of(&f), &Arc::new(AtomicBool::new(true)), &rit, &rit_hz, &xit, &mut ai, &cw_pending, &cw_busy), None);
        assert!(m.load(Ordering::Relaxed));
        assert_eq!(handle_command("RX", &f, &f, &p, &d, &m, &tx_of(&f), &Arc::new(AtomicBool::new(true)), &rit, &rit_hz, &xit, &mut ai, &cw_pending, &cw_busy), None);
        assert!(!m.load(Ordering::Relaxed));
    }

    /// Real request: TX must be refused outside every ham band unless
    /// explicitly allowed -- see main.rs's tx_frequency_allowed doc
    /// comment. 6,000,000 Hz is a real gap between 60m (tops out at
    /// 5,406,000) and 40m (starts at 7,000,000), so it isn't just an
    /// untested edge of some band's own range.
    #[test]
    fn tx_blocked_outside_ham_bands_unless_allowed() {
        let (_f, p, d, m, rit, rit_hz, xit, cw_pending, cw_busy) = fixture();
        let out_of_band = Arc::new(AtomicU64::new(6_000_000));
        let mut ai = 0;

        let blocked = Arc::new(AtomicBool::new(false));
        assert_eq!(
            handle_command(
                "TX", &out_of_band, &out_of_band, &p, &d, &m, &tx_of(&out_of_band), &blocked, &rit, &rit_hz, &xit,
                &mut ai, &cw_pending, &cw_busy
            ),
            None
        );
        assert!(!m.load(Ordering::Relaxed), "TX should have been refused outside every ham band");

        let allowed = Arc::new(AtomicBool::new(true));
        assert_eq!(
            handle_command(
                "TX", &out_of_band, &out_of_band, &p, &d, &m, &tx_of(&out_of_band), &allowed, &rit, &rit_hz, &xit,
                &mut ai, &cw_pending, &cw_busy
            ),
            None
        );
        assert!(m.load(Ordering::Relaxed), "TX should be allowed once the operator opts in");
    }

    #[test]
    fn ky_query_reflects_busy_status() {
        let (f, p, d, m, rit, rit_hz, xit, cw_pending, cw_busy) = fixture();
        let mut ai = 0;
        assert_eq!(
            handle_command("KY", &f, &f, &p, &d, &m, &tx_of(&f), &Arc::new(AtomicBool::new(true)), &rit, &rit_hz, &xit, &mut ai, &cw_pending, &cw_busy),
            Some("KY0;".to_string())
        );
        cw_busy.store(true, Ordering::Relaxed);
        assert_eq!(
            handle_command("KY", &f, &f, &p, &d, &m, &tx_of(&f), &Arc::new(AtomicBool::new(true)), &rit, &rit_hz, &xit, &mut ai, &cw_pending, &cw_busy),
            Some("KY1;".to_string())
        );
    }

    #[test]
    fn ky_set_queues_text_and_strips_marker_and_padding() {
        let (f, p, d, m, rit, rit_hz, xit, cw_pending, cw_busy) = fixture();
        let mut ai = 0;
        assert_eq!(
            handle_command("KY1CQ CQ DE G0ORX  ", &f, &f, &p, &d, &m, &tx_of(&f), &Arc::new(AtomicBool::new(true)), &rit, &rit_hz, &xit, &mut ai, &cw_pending, &cw_busy),
            None
        );
        assert_eq!(cw_pending.lock().unwrap().pop_front(), Some("CQ CQ DE G0ORX".to_string()));
    }

    #[test]
    fn ky_set_without_leading_marker_still_works() {
        let (f, p, d, m, rit, rit_hz, xit, cw_pending, cw_busy) = fixture();
        let mut ai = 0;
        assert_eq!(
            handle_command("KYTEST", &f, &f, &p, &d, &m, &tx_of(&f), &Arc::new(AtomicBool::new(true)), &rit, &rit_hz, &xit, &mut ai, &cw_pending, &cw_busy),
            None
        );
        assert_eq!(cw_pending.lock().unwrap().pop_front(), Some("TEST".to_string()));
    }

    #[test]
    fn if_response_has_expected_length_and_frequency() {
        let (f, p, _d, m, rit, rit_hz, xit, _cw_pending, _cw_busy) = fixture();
        let resp = build_if_response(&f, &p, &m, &rit, &rit_hz, &xit);
        assert!(resp.starts_with("IF00014074000"), "{resp}");
        assert!(resp.ends_with(';'));
        // "IF" + freq(11) + step(4) + rit(6) + 10 single-digit fields +
        // 2 two-digit fields + ";" = 2 + 11 + 4 + 6 + 10 + 4 + 1 = 38
        assert_eq!(resp.len(), 38);
    }

    #[test]
    fn sm_reports_s9_as_midscale() {
        let (f, p, d, m, rit, rit_hz, xit, cw_pending, cw_busy) = fixture();
        let mut ai = 0;
        // fixture() sets meter_db to -73 (S9); piHPSDR's formula maps
        // that to roughly the middle of the 0-30 scale.
        let resp = handle_command("SM0", &f, &f, &p, &d, &m, &tx_of(&f), &Arc::new(AtomicBool::new(true)), &rit, &rit_hz, &xit, &mut ai, &cw_pending, &cw_busy).unwrap();
        assert!(resp.starts_with("SM0"), "{resp}");
        assert!(resp.ends_with(';'));
    }

    #[test]
    fn ai_get_set_round_trip() {
        let (f, p, d, m, rit, rit_hz, xit, cw_pending, cw_busy) = fixture();
        let mut ai = 0;
        assert_eq!(handle_command("AI", &f, &f, &p, &d, &m, &tx_of(&f), &Arc::new(AtomicBool::new(true)), &rit, &rit_hz, &xit, &mut ai, &cw_pending, &cw_busy), Some("AI0;".to_string()));
        assert_eq!(handle_command("AI2", &f, &f, &p, &d, &m, &tx_of(&f), &Arc::new(AtomicBool::new(true)), &rit, &rit_hz, &xit, &mut ai, &cw_pending, &cw_busy), None);
        assert_eq!(ai, 2);
        assert_eq!(handle_command("AI", &f, &f, &p, &d, &m, &tx_of(&f), &Arc::new(AtomicBool::new(true)), &rit, &rit_hz, &xit, &mut ai, &cw_pending, &cw_busy), Some("AI2;".to_string()));
    }

    #[test]
    fn ps_read_always_reports_on_and_set_is_ignored() {
        let (f, p, d, m, rit, rit_hz, xit, cw_pending, cw_busy) = fixture();
        let mut ai = 0;
        assert_eq!(handle_command("PS", &f, &f, &p, &d, &m, &tx_of(&f), &Arc::new(AtomicBool::new(true)), &rit, &rit_hz, &xit, &mut ai, &cw_pending, &cw_busy), Some("PS1;".to_string()));
        // "PS0;" (power off) is deliberately a no-op, not a shutdown --
        // see this module's doc comment.
        assert_eq!(handle_command("PS0", &f, &f, &p, &d, &m, &tx_of(&f), &Arc::new(AtomicBool::new(true)), &rit, &rit_hz, &xit, &mut ai, &cw_pending, &cw_busy), None);
    }

    #[test]
    fn auto_report_sends_changes_only() {
        let t0 = Instant::now();
        let mut ar = AutoReport::default();
        // First check reports the current state; AI1 is frequency only.
        assert_eq!(ar.due(1, 14_074_000, Mode::Usb, t0), vec!["FA00014074000;".to_string()]);
        // Nothing changed.
        assert!(ar.due(1, 14_074_000, Mode::Usb, t0 + Duration::from_millis(300)).is_empty());
        // Changed, but too soon after the last check.
        assert!(ar.due(1, 7_074_000, Mode::Usb, t0 + Duration::from_millis(350)).is_empty());
        assert_eq!(
            ar.due(1, 7_074_000, Mode::Usb, t0 + Duration::from_millis(600)),
            vec!["FA00007074000;".to_string()]
        );
        // AI2 adds the mode, reported the first time it's seen.
        assert_eq!(ar.due(2, 7_074_000, Mode::Lsb, t0 + Duration::from_millis(900)), vec!["MD1;".to_string()]);
        assert_eq!(
            ar.due(2, 3_573_000, Mode::Cwu, t0 + Duration::from_millis(1200)),
            vec!["FA00003573000;".to_string(), "MD3;".to_string()]
        );
        assert!(ar.due(0, 1_840_000, Mode::Lsb, t0 + Duration::from_millis(1500)).is_empty());
    }

    /// A CatShared for tests: RX at `f`, TX (hardware and RF) at `tx`.
    fn test_shared(f: &Arc<AtomicU64>, tx: u64) -> CatShared {
        let (_, _, _, m, rit, rit_hz, xit, cw_pending, cw_busy) = fixture();
        CatShared {
            requested_frequency_hz: Arc::clone(f),
            rx_frequency_hz: Arc::clone(f),
            tx_rf_frequency_hz: Arc::new(AtomicU64::new(tx)),
            mox: m,
            tx_frequency_hz: Arc::new(AtomicU32::new(tx as u32)),
            allow_out_of_band_tx: Arc::new(AtomicBool::new(false)),
            rit_enabled: rit,
            rit_offset_hz: rit_hz,
            xit_enabled: xit,
            cw_remote_pending: cw_pending,
            cw_remote_busy: cw_busy,
        }
    }

    #[test]
    fn serial_client_answers_and_auto_reports() {
        // A serial-style client: commands from a byte slice, replies
        // into a Vec, starting at AI2 as a port with Auto report does.
        let (f, p, d, ..) = fixture();
        let shared = test_shared(&f, 14_074_000);
        let mut out = Vec::new();
        handle_client(
            &b"ID;IF"[..],
            &mut out,
            "test",
            2,
            false,
            &shared,
            &Arc::new(Mutex::new(p)),
            &Arc::new(Mutex::new(d)),
            &AtomicBool::new(false),
            &DebugLog::new("unused.txt".into()),
        );
        let out = String::from_utf8(out).unwrap();
        // The partial "IF" (no ';') isn't answered; the end of input
        // closes the client.
        assert!(out.starts_with("ID019;FA00014074000;MD"), "{out}");
    }

    #[test]
    fn report_tx_gives_the_tx_frequency() {
        // Split: listening on 14.195, transmitting on 14.225.
        let (f, p, d, ..) = fixture();
        f.store(14_195_000, Ordering::Relaxed);
        let shared = test_shared(&f, 14_225_000);
        let params = Arc::new(Mutex::new(p));
        let display = Arc::new(Mutex::new(d));
        let run = |report_tx: bool| {
            let mut out = Vec::new();
            handle_client(
                &b"FA;IF;"[..], &mut out, "test", 0, report_tx, &shared, &params, &display,
                &AtomicBool::new(false), &DebugLog::new("unused.txt".into()),
            );
            String::from_utf8(out).unwrap()
        };
        let rx = run(false);
        assert!(rx.starts_with("FA00014195000;IF00014195000"), "{rx}");
        let tx = run(true);
        assert!(tx.starts_with("FA00014225000;IF00014225000"), "{tx}");
        // A set still moves the RX VFO, as for any client.
        let mut out = Vec::new();
        handle_client(
            &b"FA00007074000;"[..], &mut out, "test", 0, true, &shared, &params, &display,
            &AtomicBool::new(false), &DebugLog::new("unused.txt".into()),
        );
        assert_eq!(f.load(Ordering::Relaxed), 7_074_000);
    }

    /// A fake port for run_ptt_lines: the input line is set by the test,
    /// the output line recorded.
    #[derive(Clone, Default)]
    struct FakeLines {
        out: Arc<Mutex<Vec<(PttOut, bool)>>>,
        input: Arc<AtomicBool>,
    }

    impl ModemLines for FakeLines {
        fn set_out(&mut self, line: PttOut, on: bool) -> serialport::Result<()> {
            self.out.lock().unwrap().push((line, on));
            Ok(())
        }
        fn read_in(&mut self, _line: PttIn) -> serialport::Result<bool> {
            Ok(self.input.load(Ordering::Relaxed))
        }
    }

    fn wait_for(what: &str, cond: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !cond() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn ptt_out_follows_transmit() {
        let (f, ..) = fixture();
        let shared = test_shared(&f, 14_074_000);
        let lines = FakeLines::default();
        let stop = Arc::new(AtomicBool::new(false));
        let t = {
            let (lines, shared, stop) = (lines.clone(), shared.clone(), Arc::clone(&stop));
            thread::spawn(move || {
                run_ptt_lines(lines, PttOut::Rts, PttIn::Off, &shared, &stop, "t", &DebugLog::new("u.txt".into()))
            })
        };
        // Dropped at once (the OS may raise it on opening).
        wait_for("RTS off", || lines.out.lock().unwrap().last() == Some(&(PttOut::Rts, false)));
        shared.mox.store(true, Ordering::Relaxed);
        wait_for("RTS on", || lines.out.lock().unwrap().last() == Some(&(PttOut::Rts, true)));
        shared.mox.store(false, Ordering::Relaxed);
        wait_for("RTS off again", || lines.out.lock().unwrap().last() == Some(&(PttOut::Rts, false)));
        shared.mox.store(true, Ordering::Relaxed);
        wait_for("RTS on again", || lines.out.lock().unwrap().last() == Some(&(PttOut::Rts, true)));
        // Stopping (port closed) drops it even mid-transmission.
        stop.store(true, Ordering::Relaxed);
        t.join().unwrap();
        assert_eq!(lines.out.lock().unwrap().last(), Some(&(PttOut::Rts, false)));
        assert_eq!(*lines.out.lock().unwrap(), vec![
            (PttOut::Rts, false), (PttOut::Rts, true), (PttOut::Rts, false), (PttOut::Rts, true), (PttOut::Rts, false),
        ]);
    }

    #[test]
    fn ptt_in_keys_on_edges_only() {
        let (f, ..) = fixture();
        let shared = test_shared(&f, 14_074_000);
        let lines = FakeLines::default();
        // Already up when the port opens: must not key.
        lines.input.store(true, Ordering::Relaxed);
        let stop = Arc::new(AtomicBool::new(false));
        let t = {
            let (lines, shared, stop) = (lines.clone(), shared.clone(), Arc::clone(&stop));
            thread::spawn(move || {
                run_ptt_lines(lines, PttOut::Off, PttIn::Cts, &shared, &stop, "t", &DebugLog::new("u.txt".into()))
            })
        };
        thread::sleep(Duration::from_millis(30));
        assert!(!shared.mox.load(Ordering::Relaxed));
        // Drop and raise: keys; drop: unkeys.
        lines.input.store(false, Ordering::Relaxed);
        thread::sleep(Duration::from_millis(30));
        lines.input.store(true, Ordering::Relaxed);
        wait_for("TX", || shared.mox.load(Ordering::Relaxed));
        lines.input.store(false, Ordering::Relaxed);
        wait_for("RX", || !shared.mox.load(Ordering::Relaxed));
        // Keyed by something else (MOX button): the line rising and
        // dropping leaves it keyed.
        shared.mox.store(true, Ordering::Relaxed);
        thread::sleep(Duration::from_millis(30));
        lines.input.store(true, Ordering::Relaxed);
        thread::sleep(Duration::from_millis(30));
        lines.input.store(false, Ordering::Relaxed);
        thread::sleep(Duration::from_millis(30));
        assert!(shared.mox.load(Ordering::Relaxed));
        // Unkeyed elsewhere while the line is up: stays unkeyed.
        shared.mox.store(false, Ordering::Relaxed);
        thread::sleep(Duration::from_millis(30));
        lines.input.store(true, Ordering::Relaxed);
        wait_for("TX", || shared.mox.load(Ordering::Relaxed));
        shared.mox.store(false, Ordering::Relaxed);
        thread::sleep(Duration::from_millis(30));
        assert!(!shared.mox.load(Ordering::Relaxed));
        // Port closed while keyed by the line: unkeyed.
        lines.input.store(false, Ordering::Relaxed);
        thread::sleep(Duration::from_millis(30));
        lines.input.store(true, Ordering::Relaxed);
        wait_for("TX", || shared.mox.load(Ordering::Relaxed));
        stop.store(true, Ordering::Relaxed);
        t.join().unwrap();
        assert!(!shared.mox.load(Ordering::Relaxed));
    }

    #[test]
    fn ptt_in_refuses_out_of_band() {
        let (f, ..) = fixture();
        let shared = test_shared(&f, 15_000_000); // not a ham band
        let lines = FakeLines::default();
        let stop = Arc::new(AtomicBool::new(false));
        let t = {
            let (lines, shared, stop) = (lines.clone(), shared.clone(), Arc::clone(&stop));
            thread::spawn(move || {
                run_ptt_lines(lines, PttOut::Off, PttIn::Dsr, &shared, &stop, "t", &DebugLog::new("u.txt".into()))
            })
        };
        thread::sleep(Duration::from_millis(20));
        lines.input.store(true, Ordering::Relaxed);
        thread::sleep(Duration::from_millis(40));
        assert!(!shared.mox.load(Ordering::Relaxed));
        stop.store(true, Ordering::Relaxed);
        t.join().unwrap();
    }

    /// CatSerialPort on a real tty: a pseudo-terminal pair stands in for
    /// a serial cable, the test reading and writing the other end.
    #[cfg(unix)]
    #[test]
    fn serial_port_serves_cat_on_a_tty() {
        use serialport::SerialPort;
        use std::io::{Read, Write};
        let (mut master, slave) = serialport::TTYPort::pair().expect("pty pair");
        master.set_timeout(Duration::from_millis(100)).unwrap();
        let name = slave.name().expect("pty name");
        let (f, p, d, ..) = fixture();
        // RX reported (Report TX frequency off), so the band change below
        // shows in FA.
        let shared = test_shared(&f, 14_074_000);
        let options = SerialOptions { port: name, auto_report: true, report_tx: false, ..Default::default() };
        let mut port = CatSerialPort::start(options, shared, p, d, DebugLog::new("unused.txt".into()));
        let read_until = |master: &mut serialport::TTYPort, want: &str| -> String {
            let mut got = String::new();
            let deadline = Instant::now() + Duration::from_secs(3);
            while !got.contains(want) && Instant::now() < deadline {
                let mut buf = [0u8; 256];
                if let Ok(n) = master.read(&mut buf) {
                    got.push_str(&String::from_utf8_lossy(&buf[..n]));
                }
            }
            got
        };
        // Auto report: the current frequency and mode as soon as it opens.
        let got = read_until(&mut master, "MD");
        assert!(got.contains("FA00014074000;"), "{got}");
        assert_eq!(port.state(), SerialState::Open);
        // Polled, as an amplifier does.
        master.write_all(b"IF;").unwrap();
        let got = read_until(&mut master, "IF");
        assert!(got.contains("IF00014074000"), "{got}");
        // A band change is reported without being asked.
        f.store(7_074_000, Ordering::Relaxed);
        let got = read_until(&mut master, "FA00007074000;");
        assert!(got.contains("FA00007074000;"), "{got}");
        port.stop();
    }

    #[test]
    fn serial_port_that_cannot_open_reports_why() {
        let (f, p, d, ..) = fixture();
        let shared = test_shared(&f, 14_074_000);
        let options = SerialOptions { port: "/nonexistent/ttyNOPE".to_string(), ..Default::default() };
        let mut port = CatSerialPort::start(options, shared, p, d, DebugLog::new("unused.txt".into()));
        let deadline = Instant::now() + Duration::from_secs(3);
        while port.state() == SerialState::Opening && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(matches!(port.state(), SerialState::Failed(_)), "{:?}", port.state());
        // Stopping doesn't wait out the 2 s retry pause.
        let t = Instant::now();
        port.stop();
        assert!(t.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn unknown_command_gets_question_mark() {
        let (f, p, d, m, rit, rit_hz, xit, cw_pending, cw_busy) = fixture();
        let mut ai = 0;
        assert_eq!(handle_command("ZZ", &f, &f, &p, &d, &m, &tx_of(&f), &Arc::new(AtomicBool::new(true)), &rit, &rit_hz, &xit, &mut ai, &cw_pending, &cw_busy), Some("?;".to_string()));
    }
}
