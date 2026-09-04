/*
    USB I/O for the original "Ozy" HPSDR hardware -- a Cypress FX2 USB
    controller + FPGA, paired on a backplane with a separate Mercury
    (RX) board and Penny (TX/audio codec) board. This predates every
    other board this project talks to, which all use Ethernet/UDP
    (Metis-style discovery + P1/P2 packet I/O in discovery.rs/radio.rs).
    Ozy speaks the SAME Protocol 1 512-byte frame format those boards
    do (see radio.rs's USB_FRAME_SIZE/build_usb_frame/p1_build_packet --
    Metis just wraps two of those frames into one UDP packet; Ozy sends
    them directly over USB bulk transfers) -- so this module is only
    the USB *transport and device bring-up* layer, not a second P1
    implementation. radio.rs's start_protocol1_ozy_usb is what ties
    this to the rest of the app.

    Ported faithfully from piHPSDR's own working, hardware-tested
    reference (~/github/dl1ycf/pihpsdr/src/ozyio.c, read in full) rather
    than reconstructed from general USB/Cypress FX2 documentation --
    every constant and control-transfer field below (vendor request
    numbers, FL_BEGIN/FL_XFER/FL_END sequence, the 0xe600 CPU-reset
    "RAM" address, I2C command bytes) is copied from that source, not
    guessed. NONE of this has been run against real hardware yet (this
    development environment has no USB access at all) -- see this
    module's own doc comments below for what's confirmed-by-reference
    vs. still first-bringup-risk, and README's Ozy USB section for the
    user-facing bring-up checklist.
*/

use nusb::transfer::{Bulk, ControlIn, ControlOut, ControlType, In, Out, Recipient};
use nusb::{Interface, MaybeFuture};
use std::io::{self, Read, Write};
use std::path::Path;
use std::time::Duration;

pub const VID: u16 = 0xfffe;
pub const PID: u16 = 0x0007;

// Same OZY_IO_TIMEOUT the reference uses everywhere (control transfers
// AND the streaming bulk reads/writes) -- ported as-is rather than
// "improved", per this project's own discipline of matching a working
// reference exactly for unverifiable low-level protocol code. A bulk
// read timing out is the NORMAL idle case (no data ready yet), not a
// fatal error -- see `RxEndpoint::read`'s own handling.
const IO_TIMEOUT: Duration = Duration::from_millis(10);

// Bulk endpoint addresses (old_protocol.c's EP6_IN_ID/EP2_OUT_ID) --
// standard USB convention: bit 7 set = IN. Endpoint 6 IN carries P1 RX
// frames from the radio; endpoint 2 OUT carries P1 TX/C&C frames to it.
const EP6_IN: u8 = 0x86;
const EP2_OUT: u8 = 0x02;

// Read size for the RX bulk endpoint -- matches piHPSDR's own
// ozy_ep6_rx_thread (2KB = four 512-byte P1 sub-frames per USB read).
// Not load-bearing for correctness: radio.rs's parse_iq_stream (reused
// by ozy_receiver_loop) treats the incoming data as a continuous byte
// stream with its own sync recovery, so any read size works -- this
// just matches the reference's own chunking for a fair comparison if
// something needs debugging against it later.
pub const EP6_READ_SIZE: usize = 2048;
const EP2_WRITE_SIZE: usize = 512;

const VRQ_SDR1K_CTL: u8 = 0x0d;
const SDR1KCTRL_READ_VERSION: u16 = 0x7;
const VENDOR_REQ_SET_LED: u8 = 0x01;
const VENDOR_REQ_FPGA_LOAD: u8 = 0x02;
const FL_BEGIN: u16 = 0;
const FL_XFER: u16 = 1;
const FL_END: u16 = 2;
// Cypress FX2 "anchor download" vendor request for loading firmware
// straight into the chip's internal RAM -- 0xa0 is a Cypress-defined
// constant (not HPSDR-specific), also reused for the single-byte
// CPU-reset write to address 0xe600 (the FX2's CPUCS register).
const RQ_ANCHOR_LOAD: u8 = 0xa0;
const CPUCS_ADDR: u16 = 0xe600;
const MAX_ANCHOR_CHUNK: usize = 64; // MAX_EPO_PACKET_SIZE in the reference

// I2C-over-USB vendor requests (ozyio.h).
const VRQ_I2C_WRITE: u8 = 0x08;
const VRQ_I2C_READ: u8 = 0x81;

pub const I2C_MERC1_FW: u8 = 0x10;
pub const I2C_MERC2_FW: u8 = 0x11;
pub const I2C_MERC1_ADC_OFS: u8 = 0x10;
pub const I2C_MERC2_ADC_OFS: u8 = 0x11;
pub const I2C_PENNY_FW: u8 = 0x15;
pub const I2C_PENNY_ALC: u8 = 0x16;
pub const I2C_PENNY_FWD: u8 = 0x17;
pub const I2C_PENNY_REV: u8 = 0x18;
pub const I2C_PENNY_TLV320: u8 = 0x1b;

/// Firmware/board versions read once at connect time -- see
/// `read_firmware_versions`. Surfaced on the About tab (main.rs).
#[derive(Clone, Debug, Default)]
pub struct OzyVersions {
    pub ozy_fx2: String,
    pub mercury: [Option<u8>; 2],
    pub penny: Option<u8>,
}

fn io_err(e: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::Other, e.to_string())
}

/// Bundled copies of `ozyfw-sdr1k.hex`/`Ozy_Janus.rbf` -- sourced
/// directly from the user's own piHPSDR repo (github.com/g0orx/pihpsdr,
/// GPL-2.0, same license this project is under), not a third-party
/// blob of uncertain provenance. Checked in each candidate location a
/// real install/build of hpsdr-rs might put them, mirroring (a
/// simplified version of) piHPSDR's own `filePath()` multi-directory
/// search:
/// - `usr/share/hpsdr-rs/ozy/<file>` next to wherever the running
///   executable lives (covers a `.deb` install, where the asset lands
///   at `/usr/share/hpsdr-rs/ozy/` -- see Cargo.toml's `[package.
///   metadata.deb]` -- and the binary at `/usr/bin/`, a fixed relative
///   offset between the two)
/// - the exe's own directory (a portable, non-packaged layout with the
///   files dropped alongside the binary)
/// - the repo's own `assets/ozy/<file>`, relative to the current
///   working directory (covers `cargo run` from the repo root, which
///   cargo sets as CWD)
/// - the repo's own `assets/ozy/<file>` again, this time via
///   `CARGO_MANIFEST_DIR` (a compile-time-baked absolute path to the
///   source checkout that built this binary) -- CWD-independent, so
///   this is what actually makes the fallback work on Windows when the
///   built `.exe` is launched some other way (double-clicked from
///   `target\release\`, a desktop shortcut, etc.) rather than via
///   `cargo run`, given there's no Windows packaging/installer step
///   (unlike the `.deb` candidate above) to provide a fixed relative
///   layout instead. Only valid on the machine/checkout that did the
///   build, same as any `cargo run`-based workflow already implies.
///
/// Only used as a fallback when the user hasn't explicitly picked a
/// file via the Discover window's "Ozy USB setup" -- an explicit
/// choice there always wins, so a custom/updated firmware build still
/// works.
fn bundled_path(filename: &str) -> Option<std::path::PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            // /usr/bin/hpsdr-rs -> /usr/share/hpsdr-rs/ozy/<file>
            if let Some(prefix) = exe_dir.parent() {
                candidates.push(prefix.join("share/hpsdr-rs/ozy").join(filename));
            }
            // Portable/non-packaged layout: files dropped next to the exe.
            candidates.push(exe_dir.join("ozy").join(filename));
        }
    }
    // Relative to the current working directory -- covers `cargo run`
    // from the repo root (cargo sets CWD there), but NOT e.g. Windows,
    // where there's no packaging step yet (unlike the .deb above) and a
    // built .exe is commonly launched some other way -- double-clicked
    // from `target\release\`, or from a shortcut elsewhere entirely,
    // neither of which has the repo root as CWD.
    candidates.push(std::path::PathBuf::from("assets/ozy").join(filename));
    // CARGO_MANIFEST_DIR is baked in at COMPILE time (always set by
    // Cargo, not a runtime env var) -- an absolute path to this exact
    // source checkout's `assets/ozy/`, valid as long as the built
    // binary is run on the same machine/checkout it was built on. This
    // is what actually makes the fallback CWD-independent on Windows
    // today, since there's no equivalent to the .deb's installed-path
    // candidate above without a real Windows packaging/installer step.
    candidates.push(std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets/ozy").join(filename));
    candidates.into_iter().find(|p| p.is_file())
}

pub fn default_firmware_path() -> Option<std::path::PathBuf> {
    bundled_path("ozyfw-sdr1k.hex")
}

pub fn default_fpga_path() -> Option<std::path::PathBuf> {
    bundled_path("Ozy_Janus.rbf")
}

/// Lightweight "is an Ozy plugged in" probe for the discovery list --
/// no open/claim, just an enumeration match, cheaper than the
/// reference's own open-then-immediately-close `ozy_discover`.
pub fn discover() -> bool {
    match nusb::list_devices().wait() {
        Ok(devices) => devices.into_iter().any(|d| d.vendor_id() == VID && d.product_id() == PID),
        Err(_) => false,
    }
}

fn open_interface() -> io::Result<Interface> {
    let device_info = nusb::list_devices()
        .wait()
        .map_err(io_err)?
        .into_iter()
        .find(|d| d.vendor_id() == VID && d.product_id() == PID)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no Ozy device (fffe:0007) found on USB"))?;
    let device = device_info.open().wait().map_err(io_err)?;
    let interface = device.claim_interface(0).wait().map_err(io_err)?;
    Ok(interface)
}

fn control_out(interface: &Interface, request: u8, value: u16, index: u16, data: &[u8]) -> io::Result<()> {
    interface
        .control_out(
            ControlOut { control_type: ControlType::Vendor, recipient: Recipient::Device, request, value, index, data },
            IO_TIMEOUT,
        )
        .wait()
        .map_err(io_err)?;
    Ok(())
}

fn control_in(interface: &Interface, request: u8, value: u16, index: u16, length: u16) -> io::Result<Vec<u8>> {
    interface
        .control_in(
            ControlIn { control_type: ControlType::Vendor, recipient: Recipient::Device, request, value, index, length },
            IO_TIMEOUT,
        )
        .wait()
        .map_err(io_err)
}

/// Cypress FX2 "anchor download" RAM write, chunked at 64 bytes per
/// control transfer (MAX_EPO_PACKET_SIZE in the reference) -- used both
/// for real Intel-HEX firmware records and for the single-byte
/// CPU-reset write to CPUCS_ADDR below.
fn write_ram(interface: &Interface, start_addr: u16, data: &[u8]) -> io::Result<()> {
    for chunk_start in (0..data.len()).step_by(MAX_ANCHOR_CHUNK) {
        let chunk_end = (chunk_start + MAX_ANCHOR_CHUNK).min(data.len());
        let addr = start_addr.wrapping_add(chunk_start as u16);
        control_out(interface, RQ_ANCHOR_LOAD, addr, 0, &data[chunk_start..chunk_end])?;
    }
    Ok(())
}

fn reset_cpu(interface: &Interface, reset: bool) -> io::Result<()> {
    write_ram(interface, CPUCS_ADDR, &[if reset { 1 } else { 0 }])
}

/// Parses one Intel HEX record (`:LLAAAATT<data>CC`) into (addr, data),
/// or `None` for an EOF (type 01) record. Returns an error for a
/// malformed record or bad checksum -- matches ozy_load_firmware's own
/// fail-closed behavior (abort the whole load rather than skip a bad
/// line).
fn parse_hex_record(line: &str) -> io::Result<Option<(u16, Vec<u8>)>> {
    let line = line.trim_end();
    let bytes = line
        .strip_prefix(':')
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Ozy firmware .hex: record missing leading ':'"))?;
    let raw: Vec<u8> = (0..bytes.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(bytes.get(i..i + 2).unwrap_or(""), 16)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "Ozy firmware .hex: bad hex digit"))
        })
        .collect::<io::Result<_>>()?;
    if raw.len() < 5 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "Ozy firmware .hex: record too short"));
    }
    let length = raw[0] as usize;
    let addr = u16::from_be_bytes([raw[1], raw[2]]);
    let rec_type = raw[3];
    if raw.len() != 4 + length + 1 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "Ozy firmware .hex: length field mismatch"));
    }
    let data = raw[4..4 + length].to_vec();
    let checksum = raw[4 + length];
    let sum: u8 = raw[..4 + length].iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
    if sum.wrapping_add(checksum) != 0 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "Ozy firmware .hex: checksum mismatch"));
    }
    match rec_type {
        0 => Ok(Some((addr, data))),
        1 => Ok(None), // EOF
        _ => Err(io::Error::new(io::ErrorKind::InvalidData, "Ozy firmware .hex: unsupported record type")),
    }
}

fn load_firmware(interface: &Interface, hex_path: &Path) -> io::Result<()> {
    let text = std::fs::read_to_string(hex_path)?;
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        match parse_hex_record(line)? {
            Some((addr, data)) => write_ram(interface, addr, &data)?,
            None => break, // EOF record
        }
    }
    Ok(())
}

fn load_fpga(interface: &Interface, rbf_path: &Path) -> io::Result<()> {
    let bytes = std::fs::read(rbf_path)?;
    control_out(interface, VENDOR_REQ_FPGA_LOAD, 0, FL_BEGIN, &[])?;
    for chunk in bytes.chunks(MAX_ANCHOR_CHUNK) {
        control_out(interface, VENDOR_REQ_FPGA_LOAD, 0, FL_XFER, chunk)?;
    }
    control_out(interface, VENDOR_REQ_FPGA_LOAD, 0, FL_END, &[])
}

fn set_led(interface: &Interface, which: u16, on: bool) -> io::Result<()> {
    control_out(interface, VENDOR_REQ_SET_LED, if on { 1 } else { 0 }, which, &[])
}

fn get_firmware_string(interface: &Interface) -> io::Result<String> {
    let data = control_in(interface, VRQ_SDR1K_CTL, SDR1KCTRL_READ_VERSION, 0, 8)?;
    let end = data.iter().position(|&b| b == 0).unwrap_or(data.len());
    Ok(String::from_utf8_lossy(&data[..end]).into_owned())
}

fn i2c_write(interface: &Interface, cmd: u8, data: &[u8]) -> io::Result<()> {
    control_out(interface, VRQ_I2C_WRITE, cmd as u16, 0, data)
}

fn i2c_read(interface: &Interface, cmd: u8, len: u16) -> io::Result<Vec<u8>> {
    control_in(interface, VRQ_I2C_READ, cmd as u16, 0, len)
}

/// Penny/Penelope/PennyLane's onboard TLV320 audio codec bring-up --
/// REQUIRED once at connect time for Penny to function at all (per
/// ozy_initialise() calling this unconditionally via
/// ozy_i2c_readvars/writepenny), not something this project's own
/// (cpal-based, host-side) audio pipeline otherwise touches. Ported as
/// a fixed init sequence, not user-configurable -- matches the
/// reference's own `writepenny(1, 1)` call (reset=1, mode=1 = "Mic
/// input with 20dB boost", the reference's own default for this call
/// site). If a future report needs Line In or no-boost Mic instead,
/// that's a `mode` bit (see the reference's own doc comment on
/// writepenny, ported verbatim below) this function would need to
/// expose as a setting -- not attempted here without a concrete report
/// to design against.
fn init_penny_codec(interface: &Interface) -> io::Result<()> {
    // Bits used in `mode` (from ozyio.c's own writepenny doc comment):
    //   b0: Mic input with 20dB boost   b1: Line in   b2: Mic input, no boost
    //   b[3:7]: Line-in gain (only used if b1 set)
    // reset=1, mode=1 here, matching ozy_i2c_readvars's own call.
    const TLV320_DATA: [u8; 16] =
        [0x1e, 0x00, 0x12, 0x01, 0x08, 0x15, 0x0c, 0x00, 0x0e, 0x02, 0x10, 0x00, 0x0a, 0x00, 0x00, 0x00];
    for i in (0..16).step_by(2) {
        i2c_write(interface, I2C_PENNY_TLV320, &[TLV320_DATA[i], TLV320_DATA[i + 1]])?;
    }
    Ok(())
}

fn read_firmware_versions(interface: &Interface, ozy_fx2: &str) -> io::Result<OzyVersions> {
    let mut versions = OzyVersions { ozy_fx2: ozy_fx2.to_string(), ..Default::default() };
    // Mercury1's I2C read failing is fatal to the rest of this sequence
    // in the reference (it bails out entirely, on the assumption the
    // I2C SCL/SDA jumpers aren't fitted) -- matched here: propagate the
    // error rather than silently reporting "no boards found".
    let merc1 = i2c_read(interface, I2C_MERC1_FW, 2)?;
    versions.mercury[0] = Some(merc1[1]);
    if let Ok(merc2) = i2c_read(interface, I2C_MERC2_FW, 2) {
        versions.mercury[1] = Some(merc2[1]);
    }
    let penny = i2c_read(interface, I2C_PENNY_FW, 2)?;
    versions.penny = Some(penny[1]);
    init_penny_codec(interface)?;
    Ok(versions)
}

/// I2C/control-transfer handle -- owns the claimed `Interface` outright
/// (moved into radio.rs's ozy_i2c_thread; NOT shared with the bulk RX/
/// TX threads, which each own their own exclusive endpoint reader/
/// writer instead -- see `RxEndpoint`/`TxEndpoint` below and
/// `initialise`'s doc comment for why).
pub struct OzyDevice {
    interface: Interface,
}

impl OzyDevice {
    /// Reads Penny's forward/reverse power + ALC, and Mercury's ADC
    /// overload flags -- meant to be polled periodically (see radio.rs's
    /// ozy_i2c_thread). Results are written by the caller directly into
    /// the SAME existing `tx_forward_power`/`tx_reverse_power`/
    /// `adc0_overload`/`adc1_overload` atomics real Metis/Hermes-class
    /// P1 packets populate, so the existing TX meter/SWR-protection UI
    /// needs no changes to work for Ozy.
    pub fn read_penny_power(&self) -> io::Result<(u16, u16, u16)> {
        let fwd = i2c_read(&self.interface, I2C_PENNY_FWD, 2)?;
        let rev = i2c_read(&self.interface, I2C_PENNY_REV, 2)?;
        let alc = i2c_read(&self.interface, I2C_PENNY_ALC, 2)?;
        Ok((
            u16::from_be_bytes([fwd[0], fwd[1]]),
            u16::from_be_bytes([rev[0], rev[1]]),
            u16::from_be_bytes([alc[0], alc[1]]),
        ))
    }

    /// `channel` is 0 or 1 (Mercury1/Mercury2). Matches the reference's
    /// own `buffer[0] == 0` => overloaded convention exactly (an
    /// active-low status byte, not the more intuitive active-high).
    pub fn read_mercury_overload(&self, channel: u8) -> io::Result<bool> {
        let cmd = if channel == 0 { I2C_MERC1_ADC_OFS } else { I2C_MERC2_ADC_OFS };
        let data = i2c_read(&self.interface, cmd, 2)?;
        Ok(data[0] == 0)
    }
}

/// Exclusive owner of the EP6 IN (RX) bulk endpoint -- moved into
/// radio.rs's ozy_receiver_loop. `with_read_timeout` (nusb's `io`
/// module) makes `read` return a `TimedOut` error on an idle endpoint
/// rather than blocking forever, so the owning thread can still notice
/// `stop_flag` promptly -- same IO_TIMEOUT the reference's own
/// OZY_IO_TIMEOUT-based polling uses.
pub struct RxEndpoint {
    reader: nusb::io::EndpointRead<Bulk>,
}

impl RxEndpoint {
    pub fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.reader.read(buf)
    }
}

/// Exclusive owner of the EP2 OUT (TX/C&C) bulk endpoint -- moved into
/// radio.rs's ozy_sender_loop.
pub struct TxEndpoint {
    writer: nusb::io::EndpointWrite<Bulk>,
}

impl TxEndpoint {
    pub fn write(&mut self, buf: &[u8]) -> io::Result<()> {
        self.writer.write_all(buf)
    }
}

/// Full cold-boot bring-up: open, load FX2 firmware, let the device
/// re-enumerate, reopen, load the FPGA bitstream, read board versions,
/// bring up Penny's audio codec, then claim the two bulk endpoints.
/// Mirrors `ozy_initialise()` exactly, including its own fixed sleeps
/// (4s after the firmware load for re-enumeration, 1s at the end) --
/// ported as-is rather than replaced with a poll loop, since there's no
/// documented signal for "re-enumeration finished" besides waiting.
///
/// Returns separate RX/TX endpoint handles rather than one shared
/// device handle: an `Endpoint` represents EXCLUSIVE access (nusb's own
/// description), so the natural, lowest-risk split is one owner per
/// endpoint -- ozy_sender_loop owns `TxEndpoint` outright,
/// ozy_receiver_loop owns `RxEndpoint` outright, and only the I2C-poll
/// thread (which never touches the bulk endpoints) needs the
/// `Interface` itself, for control transfers.
pub fn initialise(hex_path: &Path, rbf_path: &Path) -> io::Result<(OzyDevice, RxEndpoint, TxEndpoint, OzyVersions)> {
    let interface = open_interface()?;
    reset_cpu(&interface, true)?;
    load_firmware(&interface, hex_path)?;
    reset_cpu(&interface, false)?;
    drop(interface);
    std::thread::sleep(Duration::from_secs(4));

    let interface = open_interface()?;
    set_led(&interface, 1, true)?;
    load_fpga(&interface, rbf_path)?;
    set_led(&interface, 1, false)?;
    drop(interface);

    let interface = open_interface()?;
    let ozy_fx2 = get_firmware_string(&interface).unwrap_or_default();
    let versions = read_firmware_versions(&interface, &ozy_fx2)?;
    std::thread::sleep(Duration::from_secs(1));

    let rx = RxEndpoint {
        reader: interface
            .endpoint::<Bulk, In>(EP6_IN)
            .map_err(io_err)?
            .reader(EP6_READ_SIZE)
            .with_read_timeout(IO_TIMEOUT),
    };
    let tx =
        TxEndpoint { writer: interface.endpoint::<Bulk, Out>(EP2_OUT).map_err(io_err)?.writer(EP2_WRITE_SIZE) };

    Ok((OzyDevice { interface }, rx, tx, versions))
}
