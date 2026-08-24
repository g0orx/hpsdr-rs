/*
    FPGA firmware upload and static-IP configuration for openHPSDR radios,
    via two entirely separate, unrelated protocols -- see each section's
    own doc comment below. Both are high-stakes (they reprogram a radio's
    flash over the network) but bounded-risk: neither protocol can touch
    the radio's own bootloader/recovery image, only the "Application"
    firmware region, so a failed/aborted upload is always recoverable
    (re-enter bootloader mode and try again for the P1 path below), never
    a full, unrecoverable brick.

    Confirmed against three independent, hardware-tested/authoritative
    local references (not guessed):
      - piHPSDR's own working bootloader client, ~/github/dl1ycf/pihpsdr/
        src/bootloader.c
      - the FPGA-side Verilog bootloader source (the authoritative
        definition of the wire format), ~/github/OpenHPSDR-Firmware/...
        (Metis/Angelia/Orion/Orion MkII Bootloader.v -- confirmed
        byte-for-byte identical command/reply codes across all four)
      - piHPSDR's new_protocol_programmer.c for the P2 in-application path
        (see that section's own doc comment for why it's lower-confidence)

    IMPORTANT protocol limitation, ported faithfully rather than "improved"
    with invented robustness the firmware can't actually use: neither
    protocol supports resuming or retrying a single dropped packet
    mid-upload. The P1 FPGA's own program state has no formal exit once
    started (confirmed in Bootloader.v's own doc comment) -- a lost packet
    always means abort-the-whole-upload-and-power-cycle, never
    skip-and-continue. Any anomaly (timeout, wrong reply, wrong sequence)
    must abort immediately with a clear message.
*/

use std::io;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use pnet_datalink::{Channel, DataLinkReceiver, DataLinkSender, MacAddr, NetworkInterface};

/// 256 bytes/page -- both protocols' fixed program-block size.
const PAGE_SIZE: usize = 256;

// ===========================================================================
// Shared progress reporting (both protocols below) -- polled by the UI each
// frame, same idiom as discovery_ui.rs's `discovering: Arc<Mutex<bool>>`,
// just carrying a running percentage instead of a plain bool.
// ===========================================================================

#[derive(Clone)]
pub enum UploadStage {
    Erasing,
    Programming { blocks_sent: usize, blocks_total: usize },
    Done,
    /// `needs_power_cycle`: see this module's own doc comment. Defaulted
    /// to `true` for any failure past a permissions/setup error on EITHER
    /// protocol -- for the P1 path this is a confirmed hardware fact; for
    /// the lower-confidence P2 path it's a deliberately conservative
    /// default given genuine uncertainty about its real recovery
    /// semantics (better to over-warn than leave the user confused why a
    /// retry silently doesn't work).
    Failed { message: String, needs_power_cycle: bool },
}

/// Handle to a running (or just-finished) Erase+Program sequence, on either
/// protocol -- returned by `spawn_raw_upload`/`spawn_inapp_upload`.
pub struct UploadHandle {
    pub progress: Arc<Mutex<UploadStage>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl UploadHandle {
    /// Requests cancellation -- takes effect between pages, not
    /// immediately (matches how every other cancellable background
    /// operation in this codebase works, e.g. RadioSession's own
    /// stop_flag). Cancelling mid-upload still leaves the radio needing
    /// the same recovery as any other abort -- there is no clean "abort"
    /// as far as the radio's own firmware is concerned.
    pub fn cancel(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Drop for UploadHandle {
    fn drop(&mut self) {
        self.cancel();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Pads `firmware` up to a whole number of 256-byte pages with 0xFF,
/// matching the reference exactly (bootloader.c) -- the FPGA has no
/// concept of a partial final page.
fn pad_firmware(firmware: &[u8]) -> Vec<u8> {
    let mut padded = firmware.to_vec();
    let remainder = padded.len() % PAGE_SIZE;
    if remainder != 0 {
        padded.extend(std::iter::repeat(0xFFu8).take(PAGE_SIZE - remainder));
    }
    padded
}

// ===========================================================================
// P1: raw-Ethernet bootloader-mode protocol (Metis/Hermes/Hermes2/Angelia/
// Orion/Orion2 -- identical command set confirmed across all of them).
//
// NOT UDP/IP -- raw Ethernet frames (EtherType byte pair 0xEF 0xFE, our own
// sub-protocol marker 0x03) to/from a fixed bogus MAC 11:22:33:44:55:66,
// which is ALL a bootloader-mode radio ever answers to (it doesn't run the
// normal UDP discovery responder -- that's part of the Application image,
// not the bootloader image, so bootloader-mode radios never show up in
// discovery.rs's normal device list). The radio must already be physically
// switched into bootloader mode and power-cycled -- nothing here can put
// it there over the network.
//
// Requires raw packet send/receive, hence pnet_datalink rather than this
// project's usual socket2-based UDP sockets -- and, on every platform,
// elevated privileges this project has never needed before (root/
// CAP_NET_RAW on Linux, BPF device access on macOS, Administrator + Npcap
// on Windows). See Cargo.toml's own doc comment on the pnet_datalink entry.
// ===========================================================================

/// Fixed bogus MAC every bootloader-mode radio answers to and sends from --
/// confirmed identical across Metis/Angelia/Orion/Orion2's own Verilog
/// source (Bootloader.v, all four board families).
pub const BOOTLOADER_MAC: MacAddr = MacAddr(0x11, 0x22, 0x33, 0x44, 0x55, 0x66);

const RAW_ETHERTYPE: [u8; 2] = [0xEF, 0xFE];
const RAW_SUBPROTOCOL: u8 = 0x03;

const RAW_CMD_PROGRAM: u8 = 0x01;
const RAW_CMD_ERASE: u8 = 0x02;
const RAW_CMD_READ_MAC: u8 = 0x03;
const RAW_CMD_READ_IP: u8 = 0x04;
const RAW_CMD_WRITE_IP: u8 = 0x05;

const RAW_REPLY_ERASE_DONE: u8 = 0x01;
const RAW_REPLY_SEND_MORE: u8 = 0x02;
const RAW_REPLY_HAVE_MAC: u8 = 0x03;
const RAW_REPLY_HAVE_IP: u8 = 0x04;

/// Minimum Ethernet frame size (excluding the 4-byte FCS trailer, which
/// every backend adds automatically) -- short frames (Erase/Read MAC/Read
/// IP/Write IP, all well under this) are zero-padded up to it explicitly
/// rather than relying on the OS/driver to pad short sends, since that
/// isn't guaranteed identically across Linux/BPF/Npcap.
const MIN_FRAME_LEN: usize = 60;

/// Lists usable network interfaces for the raw-Ethernet path (has a MAC,
/// isn't loopback) -- the radio must be reached via a direct cable or a
/// plain unmanaged switch on this interface; it won't cross a router, VPN,
/// or most managed switches (confirmed in the Apache Labs user guide and
/// bootloader.c's own doc comment).
pub fn list_raw_interfaces() -> Vec<NetworkInterface> {
    pnet_datalink::interfaces().into_iter().filter(|i| !i.is_loopback() && i.mac.is_some()).collect()
}

fn open_raw_channel(
    interface: &NetworkInterface,
) -> io::Result<(Box<dyn DataLinkSender>, Box<dyn DataLinkReceiver>)> {
    let config = pnet_datalink::Config {
        // Short so callers can poll a stop flag / overall deadline between
        // reads rather than blocking indefinitely -- matches this
        // project's existing UDP socket read-timeout idiom (discovery.rs's
        // SOCKET_TIMEOUT), just at the datalink layer instead.
        read_timeout: Some(Duration::from_millis(100)),
        ..Default::default()
    };
    match pnet_datalink::channel(interface, config) {
        Ok(Channel::Ethernet(tx, rx)) => Ok((tx, rx)),
        Ok(_) => Err(io::Error::other("unsupported datalink channel type")),
        Err(e) => Err(annotate_raw_channel_error(e)),
    }
}

/// Raw-channel-open failures are overwhelmingly a permissions problem on
/// every platform this project targets (see this module's own doc comment)
/// -- surface that plainly rather than a bare OS error string, since
/// nothing else in this codebase has ever needed to explain "run this as
/// root/Administrator" to a user before.
fn annotate_raw_channel_error(e: io::Error) -> io::Error {
    if e.kind() == io::ErrorKind::PermissionDenied {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{e} -- raw Ethernet access needs elevated privileges: run as root or \
                 `sudo setcap cap_net_raw+ep` the built binary on Linux; run as Administrator \
                 with Npcap installed (WinPcap-compatible mode) on Windows; run as root (or \
                 grant /dev/bpf* access) on macOS."
            ),
        )
    } else {
        e
    }
}

fn build_raw_frame(src_mac: MacAddr, command: u8, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(16 + payload.len());
    frame.extend_from_slice(&BOOTLOADER_MAC.octets());
    frame.extend_from_slice(&src_mac.octets());
    frame.extend_from_slice(&RAW_ETHERTYPE);
    frame.push(RAW_SUBPROTOCOL);
    frame.push(command);
    frame.extend_from_slice(payload);
    while frame.len() < MIN_FRAME_LEN {
        frame.push(0);
    }
    frame
}

/// True if `frame` is a bootloader reply addressed to us -- dest MAC is our
/// own NIC, src MAC is the fixed bootloader MAC, same EtherType/sub-
/// protocol marker. A raw Layer2 channel receives every frame on the wire,
/// not just ones for this protocol, so this filter matters.
fn is_our_reply(frame: &[u8], src_mac: MacAddr) -> bool {
    frame.len() >= 16
        && frame[0..6] == src_mac.octets()[..]
        && frame[6..12] == BOOTLOADER_MAC.octets()[..]
        && frame[12..14] == RAW_ETHERTYPE[..]
        && frame[14] == RAW_SUBPROTOCOL
}

/// Sends one command and, if `reply_timeout` is `Some`, blocks (subject to
/// `stop`) until a matching reply arrives or the timeout elapses -- see
/// this module's own doc comment on why a timeout/anomaly here always
/// means "abort", never "retry the same page".
fn raw_command(
    tx: &mut dyn DataLinkSender,
    rx: &mut dyn DataLinkReceiver,
    src_mac: MacAddr,
    command: u8,
    payload: &[u8],
    reply_timeout: Option<Duration>,
    stop: &AtomicBool,
) -> io::Result<Option<Vec<u8>>> {
    let frame = build_raw_frame(src_mac, command, payload);
    tx.send_to(&frame, None).unwrap_or_else(|| Err(io::Error::other("send_to: no packet sent")))?;

    let Some(timeout) = reply_timeout else {
        return Ok(None);
    };
    let deadline = Instant::now() + timeout;
    loop {
        if stop.load(Ordering::Relaxed) {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "no reply from bootloader"));
        }
        match rx.next() {
            Ok(frame) if is_our_reply(frame, src_mac) => return Ok(Some(frame.to_vec())),
            Ok(_) => continue, // someone else's traffic on this interface
            Err(e) if matches!(e.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock) => continue,
            Err(e) => return Err(e),
        }
    }
}

/// One raw-Ethernet bootloader-mode radio, reachable on `interface`. Opens
/// a fresh datalink channel per call rather than staying connected across
/// UI actions -- each operation here is already a fraction of a second (or
/// a few minutes for Erase+Program) to open and run, and this avoids any
/// cross-thread channel/actor complexity for what's always a strictly
/// sequential, user-driven flow (Test -> Read IP -> [Write IP] -> Erase ->
/// Program, one button at a time -- see the rollout plan this was built
/// against).
pub struct RawBootloader {
    interface: NetworkInterface,
}

impl RawBootloader {
    pub fn new(interface: NetworkInterface) -> Self {
        Self { interface }
    }

    fn src_mac(&self) -> io::Result<MacAddr> {
        self.interface.mac.ok_or_else(|| io::Error::other("interface has no MAC address"))
    }

    /// "Test for Bootloader" -- confirms a radio is actually present in
    /// bootloader mode on this interface before anything else. Read-only
    /// and safe -- the recommended first step for any real attempt.
    pub fn read_mac(&self) -> io::Result<[u8; 6]> {
        let src_mac = self.src_mac()?;
        let (mut tx, mut rx) = open_raw_channel(&self.interface)?;
        let stop = AtomicBool::new(false);
        let reply = raw_command(
            &mut *tx,
            &mut *rx,
            src_mac,
            RAW_CMD_READ_MAC,
            &[],
            Some(Duration::from_millis(500)),
            &stop,
        )?
        .ok_or_else(|| io::Error::other("no reply"))?;
        if reply.len() < 22 || reply[15] != RAW_REPLY_HAVE_MAC {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "unexpected reply to Read MAC"));
        }
        Ok([reply[16], reply[17], reply[18], reply[19], reply[20], reply[21]])
    }

    /// Reads the radio's currently configured static IP.
    pub fn read_ip(&self) -> io::Result<[u8; 4]> {
        let src_mac = self.src_mac()?;
        let (mut tx, mut rx) = open_raw_channel(&self.interface)?;
        let stop = AtomicBool::new(false);
        let reply = raw_command(
            &mut *tx,
            &mut *rx,
            src_mac,
            RAW_CMD_READ_IP,
            &[],
            Some(Duration::from_millis(500)),
            &stop,
        )?
        .ok_or_else(|| io::Error::other("no reply"))?;
        if reply.len() < 20 || reply[15] != RAW_REPLY_HAVE_IP {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "unexpected reply to Read IP"));
        }
        Ok([reply[16], reply[17], reply[18], reply[19]])
    }

    /// Fire-and-forget -- the radio sends no reply to Write IP (confirmed
    /// against bootloader.c, which explicitly waits a fixed 100ms and
    /// moves on rather than expecting one). Read IP back afterward to
    /// confirm, same as the reference UI flow. `0.0.0.0` reverts to
    /// DHCP/APIPA, per the Apache Labs user guide.
    pub fn write_ip(&self, ip: [u8; 4]) -> io::Result<()> {
        let src_mac = self.src_mac()?;
        let (mut tx, mut rx) = open_raw_channel(&self.interface)?;
        let stop = AtomicBool::new(false);
        raw_command(&mut *tx, &mut *rx, src_mac, RAW_CMD_WRITE_IP, &ip, None, &stop)?;
        thread::sleep(Duration::from_millis(100));
        Ok(())
    }

    /// Erases the Application flash region, then programs `firmware`
    /// (raw, unpadded .rbf file bytes) page by page, reporting progress
    /// via `progress` and checking `stop` between pages -- see
    /// UploadHandle::cancel's doc comment: cancelling here still requires
    /// a power cycle before the radio will accept another attempt, same
    /// as any other abort.
    fn erase_and_program(
        &self,
        firmware: &[u8],
        progress: &Mutex<UploadStage>,
        stop: &AtomicBool,
    ) -> io::Result<()> {
        let src_mac = self.src_mac()?;
        let (mut tx, mut rx) = open_raw_channel(&self.interface)?;

        *progress.lock().unwrap() = UploadStage::Erasing;
        let reply = raw_command(
            &mut *tx,
            &mut *rx,
            src_mac,
            RAW_CMD_ERASE,
            &[],
            Some(Duration::from_secs(180)),
            stop,
        )?
        .ok_or_else(|| io::Error::other("no reply to Erase"))?;
        if reply.get(15) != Some(&RAW_REPLY_ERASE_DONE) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "unexpected reply to Erase"));
        }

        let padded = pad_firmware(firmware);
        let total_blocks = padded.len() / PAGE_SIZE;
        for (i, page) in padded.chunks(PAGE_SIZE).enumerate() {
            if stop.load(Ordering::Relaxed) {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
            }
            *progress.lock().unwrap() = UploadStage::Programming { blocks_sent: i, blocks_total: total_blocks };
            let mut payload = Vec::with_capacity(4 + PAGE_SIZE);
            payload.extend_from_slice(&[0u8; 4]); // reserved, matches bootloader.c
            payload.extend_from_slice(page);
            let reply = raw_command(
                &mut *tx,
                &mut *rx,
                src_mac,
                RAW_CMD_PROGRAM,
                &payload,
                Some(Duration::from_millis(500)),
                stop,
            )?
            .ok_or_else(|| io::Error::other("no reply to Program"))?;
            if reply.get(15) != Some(&RAW_REPLY_SEND_MORE) {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "unexpected reply to Program"));
            }
        }
        *progress.lock().unwrap() =
            UploadStage::Programming { blocks_sent: total_blocks, blocks_total: total_blocks };
        Ok(())
    }
}

/// Spawns the Erase+Program sequence on a background thread -- see
/// UploadHandle's own doc comment. `firmware` should be the raw `.rbf`
/// file bytes, unpadded (padding happens inside).
pub fn spawn_raw_upload(interface: NetworkInterface, firmware: Vec<u8>) -> UploadHandle {
    let progress = Arc::new(Mutex::new(UploadStage::Erasing));
    let stop = Arc::new(AtomicBool::new(false));
    let thread_progress = Arc::clone(&progress);
    let thread_stop = Arc::clone(&stop);
    let thread = thread::spawn(move || {
        let bootloader = RawBootloader::new(interface);
        match bootloader.erase_and_program(&firmware, &thread_progress, &thread_stop) {
            Ok(()) => *thread_progress.lock().unwrap() = UploadStage::Done,
            Err(e) => {
                // Every failure past simply opening the channel means the
                // FPGA's program state may already be mid-sequence with
                // nowhere to go -- see this module's own doc comment.
                // Treat all of them as needing a power cycle rather than
                // trying to distinguish "safe" failures from "unsafe" ones
                // the protocol itself can't tell apart either.
                let needs_power_cycle = e.kind() != io::ErrorKind::PermissionDenied;
                *thread_progress.lock().unwrap() = UploadStage::Failed { message: e.to_string(), needs_power_cycle };
            }
        }
    });
    UploadHandle { progress, stop, thread: Some(thread) }
}

// ===========================================================================
// P2: in-application UDP update protocol -- works against a normally-
// running, already-discovered/connected radio, no physical mode switch.
//
// LOWER CONFIDENCE than the P1 path above: the only local reference is
// piHPSDR's new_protocol_programmer.c, which is compiled in but never
// actually called from its own GUI, plus a test simulator whose ack-byte
// assumptions don't fully cross-check against it. Ported as faithfully as
// the reference allows, but the exact reply byte offsets below are this
// module's own reasonable inference (mirroring the outgoing packet's own
// layout -- a 4-byte sequence number, then a single response-code byte)
// rather than a confirmed fact. Surfaced prominently in the UI as such --
// don't present this path as equally trustworthy as the P1 one.
// ===========================================================================

const INAPP_PORT: u16 = 1024;
const INAPP_CMD_ERASE: u8 = 0x04;
const INAPP_CMD_PROGRAM: u8 = 0x05;
const INAPP_CMD_SET_IP: u8 = 0x06;
const INAPP_ACK_ERASE: u8 = 0x03;
const INAPP_ACK_PROGRAM: u8 = 0x04;

fn open_inapp_socket() -> io::Result<UdpSocket> {
    use socket2::{Domain, Socket, Type};
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(socket2::Protocol::UDP))?;
    socket.set_read_timeout(Some(Duration::from_millis(200)))?;
    socket.bind(&SocketAddr::from(([0, 0, 0, 0], 0)).into())?;
    Ok(socket.into())
}

/// Waits (subject to `stop`, polled every ~200ms via the socket's own read
/// timeout) up to `timeout` for a reply from `target` whose response-code
/// byte (offset 4, right after the echoed 4-byte sequence number -- see
/// this section's own doc comment on why that offset is inferred, not
/// confirmed) equals `expect_code`.
fn inapp_wait_for_ack(
    socket: &UdpSocket,
    target: SocketAddr,
    expect_code: u8,
    timeout: Duration,
    stop: &AtomicBool,
) -> io::Result<()> {
    let deadline = Instant::now() + timeout;
    let mut buf = [0u8; 1024];
    loop {
        if stop.load(Ordering::Relaxed) {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "no reply from radio"));
        }
        match socket.recv_from(&mut buf) {
            Ok((amt, src)) if src == target && amt > 4 && buf[4] == expect_code => return Ok(()),
            Ok(_) => continue,
            Err(e) if matches!(e.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock) => continue,
            Err(e) => return Err(e),
        }
    }
}

pub struct InAppUpdate {
    target: SocketAddr,
}

impl InAppUpdate {
    pub fn new(radio_ip: IpAddr) -> Self {
        Self { target: SocketAddr::new(radio_ip, INAPP_PORT) }
    }

    /// Changes the radio's static IP (or reverts to DHCP with 0.0.0.0, per
    /// the Apache Labs user guide) -- fire-and-forget, matching this
    /// section's own doc comment about the uncertainty in whether this
    /// should really be unicast (as implemented here, to the
    /// already-known connected IP) or broadcast; unicast is the
    /// conservative choice given a live connection to address already
    /// exists.
    pub fn set_ip(&self, target_mac: [u8; 6], new_ip: [u8; 4]) -> io::Result<()> {
        let socket = open_inapp_socket()?;
        let mut packet = [0u8; 60];
        packet[4] = INAPP_CMD_SET_IP;
        packet[5..11].copy_from_slice(&target_mac);
        packet[11..15].copy_from_slice(&new_ip);
        socket.send_to(&packet, self.target)?;
        Ok(())
    }

    fn erase_and_program(
        &self,
        firmware: &[u8],
        progress: &Mutex<UploadStage>,
        stop: &AtomicBool,
    ) -> io::Result<()> {
        let socket = open_inapp_socket()?;

        *progress.lock().unwrap() = UploadStage::Erasing;
        let mut erase_packet = [0u8; 60];
        erase_packet[4] = INAPP_CMD_ERASE;
        socket.send_to(&erase_packet, self.target)?;
        // Two acks per the reference: receipt, then completion.
        inapp_wait_for_ack(&socket, self.target, INAPP_ACK_ERASE, Duration::from_secs(120), stop)?;
        inapp_wait_for_ack(&socket, self.target, INAPP_ACK_ERASE, Duration::from_secs(120), stop)?;

        let padded = pad_firmware(firmware);
        let total_blocks = padded.len() / PAGE_SIZE;
        for (i, page) in padded.chunks(PAGE_SIZE).enumerate() {
            if stop.load(Ordering::Relaxed) {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
            }
            *progress.lock().unwrap() = UploadStage::Programming { blocks_sent: i, blocks_total: total_blocks };
            let mut packet = Vec::with_capacity(9 + PAGE_SIZE);
            packet.extend_from_slice(&(i as u32).to_be_bytes());
            packet.push(INAPP_CMD_PROGRAM);
            packet.extend_from_slice(&(total_blocks as u32).to_be_bytes());
            packet.extend_from_slice(page);
            socket.send_to(&packet, self.target)?;
            inapp_wait_for_ack(&socket, self.target, INAPP_ACK_PROGRAM, Duration::from_secs(5), stop)?;
        }
        *progress.lock().unwrap() =
            UploadStage::Programming { blocks_sent: total_blocks, blocks_total: total_blocks };
        Ok(())
    }
}

/// Spawns the Erase+Program sequence on a background thread -- see
/// UploadHandle's own doc comment.
pub fn spawn_inapp_upload(radio_ip: IpAddr, firmware: Vec<u8>) -> UploadHandle {
    let progress = Arc::new(Mutex::new(UploadStage::Erasing));
    let stop = Arc::new(AtomicBool::new(false));
    let thread_progress = Arc::clone(&progress);
    let thread_stop = Arc::clone(&stop);
    let thread = thread::spawn(move || {
        let updater = InAppUpdate::new(radio_ip);
        match updater.erase_and_program(&firmware, &thread_progress, &thread_stop) {
            Ok(()) => *thread_progress.lock().unwrap() = UploadStage::Done,
            Err(e) => {
                *thread_progress.lock().unwrap() =
                    UploadStage::Failed { message: e.to_string(), needs_power_cycle: true };
            }
        }
    });
    UploadHandle { progress, stop, thread: Some(thread) }
}
