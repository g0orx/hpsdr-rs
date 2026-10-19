/*
    USB I/O and tuner control for Mirics MSi2500 + MSi001 receivers --
    the cheap "MSI.SDR" USB sticks (and the original SDRplay RSP1, which
    is the same chip pair). Receive-only.

    Same shape as rtlsdr.rs: a raw USB device streaming complex baseband
    at a fixed rate, tuned by real PLL retunes, decimated in software by
    rtlsdr::Decimator (shared, not copied -- same fixed 1.536MS/s input
    rate, same offset-tuning trick for the centre spike). Unlike the
    RTL-SDR, the MSi001 covers HF natively (its AM path upconverts by
    120MHz internally), so there's no direct-sampling mode to manage.

    PORTED, not invented: every register sequence below is a
    transcription of libmirisdr-4 (~/github/libmirisdr-4, commit fd0452e:
    src/adc.c, hard.c, soft.c, gain.c, streaming.c, convert/252_s16.c),
    which is GPL-2.0-or-later -- the same licence as this project. Function
    names match the C ones minus their mirisdr_ prefix. Deliberately NOT
    ported:
    - Isochronous streaming (libmirisdr's default on Unix): nusb has no
      isochronous transfer API, so this uses the MSi2500's bulk alternate
      setting (3), which libmirisdr itself uses on Windows.
    - Sample formats other than 252_S16 -- the only one libmirisdr's
      automatic choice ever picks at or below 6.048MS/s.
    - The SDRplay hardware flavour's band-select table -- the default
      (clone) table is used; a genuine RSP1's front-end filter GPIOs may
      want the other one.
    - Bias-T, IF modes other than zero-IF, and the commented-out AGC.
*/

use crate::rtlsdr;
use crate::usb_port;
use nusb::transfer::{Bulk, ControlOut, ControlType, In, Recipient};
use nusb::{Interface, MaybeFuture};
use std::io::{self, Read};
use std::time::Duration;

/// libmirisdr's `mirisdr_devices[]` table (devices.c), verbatim. The
/// MSI.SDR sticks (and the SDRplay RSP1) enumerate as 1df7:2500.
const KNOWN_DEVICES: &[(u16, u16, &str)] = &[
    (0x1df7, 0x2500, "Mirics MSi2500 default (e.g. VTX3D card)"),
    (0x1df7, 0x3000, "SDRplay RSP1A"),
    (0x1df7, 0x3010, "SDRplay RSP2"),
    (0x2040, 0xd300, "Hauppauge WinTV 133559 LF"),
    (0x07ca, 0x8591, "AverMedia A859 Pure DVBT"),
    (0x04bb, 0x0537, "IO-DATA GV-TV100 stick"),
    (0x0511, 0x0037, "Logitec LDT-1S310U/J"),
];

/// Same fixed rate as the RTL-SDR (see rtlsdr::DONGLE_RATE_HZ for why
/// 1.536MS/s) -- inside the MSi2500's 1.3-15MS/s range, and below
/// 6.048MS/s, so libmirisdr's automatic format choice is 252_S16. Kept
/// equal to rtlsdr's so its Decimator, output-rate list and tuning
/// offset all apply unchanged.
pub const SAMPLE_RATE_HZ: u32 = rtlsdr::DONGLE_RATE_HZ;

/// Top of the MSi001's frequency plan in practice (libmirisdr's table
/// runs to a 2.4GHz sentinel; sticks are sold as "up to 2GHz").
pub const FREQUENCY_MAX_HZ: u64 = 2_000_000_000;

/// libmirisdr's DEFAULT_GAIN, and the top of mirisdr_set_tuner_gain's
/// range (LNA 24 + mixer 19 + baseband 59 dB).
pub const DEFAULT_GAIN_DB: u32 = 43;
pub const MAX_GAIN_DB: u32 = 102;

const CTRL_TIMEOUT: Duration = Duration::from_millis(2000); // CTRL_TIMEOUT
const EP_STREAM_IN: u8 = 0x81;
/// Interface 0's bulk alternate setting (libmirisdr's MIRISDR_TRANSFER_BULK).
const ALT_SETTING_BULK: u8 = 3;

/// Every block on the stream: a 16-byte header (little-endian sample
/// counter in the first 4 bytes) then 1008 bytes of payload -- for
/// 252_S16, 252 I/Q pairs of 14-bit samples in 16-bit words.
const BLOCK_LEN: usize = 1024;
const BLOCK_HEADER_LEN: usize = 16;
const SAMPLES_PER_BLOCK: u32 = 252;

/// See rtlsdr::STREAM_READ_SIZE -- same ~5ms-per-transfer reasoning
/// (1.536MS/s * 4 bytes * 1024/1008 ~= 6.2MB/s, so 32KiB ~= 5ms), and
/// a whole number of blocks. libmirisdr's own is 16KiB.
pub const STREAM_READ_SIZE: usize = 32 * BLOCK_LEN;
const STREAM_NUM_TRANSFERS: usize = 16;

// MSi2500 vendor requests (hard.h).
const CMD_WREG: u8 = 0x41;
const CMD_START_STREAMING: u8 = 0x43;
const CMD_STOP_STREAMING: u8 = 0x45;

fn io_err(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
}

fn known_name(info: &nusb::DeviceInfo) -> Option<&'static str> {
    KNOWN_DEVICES
        .iter()
        .find(|&&(vid, pid, _)| vid == info.vendor_id() && pid == info.product_id())
        .map(|&(_, _, name)| name)
}

/// Last byte of every MSi2500 sentinel MAC -- see usb_port::port_mac.
pub const MAC_TAG: u8 = 3;

/// Every plugged-in device matching KNOWN_DEVICES, as (name, usb_port::port_mac),
/// in port order -- for discovery.rs's synthetic Device rows. No
/// open/claim, safe to poll.
pub fn discover() -> Vec<(&'static str, [u8; 6])> {
    let Ok(devices) = nusb::list_devices().wait() else {
        return Vec::new();
    };
    let mut found: Vec<_> = devices.filter_map(|d| known_name(&d).map(|name| (name, usb_port::port_mac(&d, MAC_TAG)))).collect();
    found.sort_by_key(|&(_, mac)| mac);
    found
}

// ---------------------------------------------------------------------
// Frequency plan (soft.c)
// ---------------------------------------------------------------------

// reg0 bits 4:8 (soft.h)
const MODE_AM: u32 = 0x01;
const MODE_VHF: u32 = 0x02;
const MODE_B3: u32 = 0x04;
const MODE_B45: u32 = 0x08;
const MODE_BL: u32 = 0x10;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Band {
    Am1,
    Am2,
    Vhf,
    B3,
    B45,
    L,
}

/// hw_switch_freq_plan_t: (low cut MHz, mode, upconvert mixer on, AM
/// port, LO divider, band-select GPIO word).
struct PlanEntry {
    low_cut_mhz: u32,
    mode: i32,
    upconvert: u32,
    am_port: u32,
    lo_div: u64,
    band_select_word: u32,
}

const fn plan(low_cut_mhz: u32, mode: i32, upconvert: u32, am_port: u32, lo_div: u64, band_select_word: u32) -> PlanEntry {
    PlanEntry { low_cut_mhz, mode, upconvert, am_port, lo_div, band_select_word }
}

/// hw_switch_freq_plan_default[], verbatim -- including the 259MHz
/// entry's undocumented mode 6 (VHF|B3 bits together).
const FREQ_PLAN: [PlanEntry; 10] = [
    plan(0, MODE_AM as i32, 1, 1, 16, 0xf780),
    plan(12, MODE_AM as i32, 1, 1, 16, 0xff80),
    plan(30, MODE_AM as i32, 1, 1, 16, 0xf280),
    plan(50, MODE_VHF as i32, 0, 0, 32, 0xf380),
    plan(108, MODE_B3 as i32, 0, 0, 16, 0xfa80),
    plan(250, MODE_B3 as i32, 0, 0, 16, 0xf680),
    plan(259, 6, 0, 0, 8, 0xf680),
    plan(330, MODE_B45 as i32, 0, 0, 4, 0xf380),
    plan(960, MODE_BL as i32, 0, 0, 2, 0xfa80),
    plan(2400, -1, 0, 0, 0, 0x0000),
];

// reg0 fields (soft.h)
const RF_SYNTHESIZER_ON: u32 = 1;
const IF_MODE_ZERO: u32 = 3;
/// FIL_BW_SEL for 1.536MHz (MIRISDR_BW_1536KHZ) -- matched to
/// SAMPLE_RATE_HZ so the MSi001's own IF filter is the anti-alias
/// filter. libmirisdr defaults to 8MHz, which at this sample rate would
/// fold everything up to +-4MHz into the passband.
const BW_1536KHZ: u32 = 0x03;
/// XTAL_SEL for 24MHz (libmirisdr only supports its default crystal).
const XTAL_24M: u32 = 0x02;
const RF_SYNTHESIZER_RESERVED_PROGRAMMING: u32 = 0x28;

/// The register words mirisdr_set_soft writes for `freq`, plus the band
/// it selects (needed for gain). Pure, so it can be tested against
/// values from upstream's own C arithmetic. Returns (band,
/// band_select_word, reg0, reg2, reg3, reg5).
fn soft_regs(freq: u32) -> (Band, u32, u32, u32, u32, u32) {
    let mut reg0: u32 = 0;
    let mut reg2: u32 = 2;
    let mut reg5: u32 = 5;
    let mut reg3: u32 = 3;
    let mut offset: u64 = 0;

    let mut i = 0;
    while freq as u64 >= 1_000_000 * FREQ_PLAN[i].low_cut_mhz as u64 {
        if FREQ_PLAN[i].mode < 0 {
            break;
        }
        i += 1;
    }
    let switch_plan = &FREQ_PLAN[i - 1];

    let (lo_div, band) = if switch_plan.mode == MODE_AM as i32 {
        reg0 |= MODE_AM << 4;
        reg0 |= switch_plan.upconvert << 9;
        reg0 |= switch_plan.am_port << 11;
        if switch_plan.upconvert != 0 {
            offset += 120_000_000;
        }
        (16, if switch_plan.am_port == 0 { Band::Am1 } else { Band::Am2 })
    } else {
        reg0 |= (switch_plan.mode as u32) << 4;
        let band = match switch_plan.mode as u32 {
            MODE_VHF => Band::Vhf,
            MODE_B3 => Band::B3,
            MODE_B45 => Band::B45,
            MODE_BL => Band::L,
            // The mode-6 entry: upstream leaves p->band unchanged, which
            // coming from either neighbour is a non-AM band -- only
            // AM-vs-not matters to set_gain.
            _ => Band::B3,
        };
        (switch_plan.lo_div, band)
    };

    reg0 |= RF_SYNTHESIZER_ON << 10;
    reg0 |= IF_MODE_ZERO << 12;
    reg0 |= BW_1536KHZ << 14;
    reg0 |= XTAL_24M << 17;
    // IF_LPMODE_NORMAL / VCO_LPMODE_NORMAL are both 0.

    let f = freq as u64 + offset;
    let fvco = f * lo_div;
    let n = fvco / 96_000_000;
    let mut thresh = 96_000_000 / lo_div;
    let mut frac = (fvco % 96_000_000) / lo_div;

    // greatest common divisor of thresh and frac
    let (mut a, mut b) = (thresh, frac);
    while a != 0 {
        let c = a;
        a = b % a;
        b = c;
    }
    thresh /= b;
    frac /= b;

    // reduce to the register's 12-bit range
    let a = thresh.div_ceil(4095);
    thresh = (thresh + a / 2) / a;
    frac = (frac + a / 2) / a;

    let denom = thresh * 4096 * lo_div;
    let rfvco = (96_000_000 * (n * thresh * 4096 + frac * 4096)) / denom;
    if f < rfvco {
        frac = frac.wrapping_sub(1);
    }
    let rfvco = (96_000_000 * (n * thresh * 4096 + frac.wrapping_mul(4096))) / denom;
    let afc = (f.wrapping_sub(rfvco).wrapping_mul(thresh * 4096 * lo_div)) / 96_000_000;

    reg3 |= ((afc & 4095) << 4) as u32;
    reg5 |= ((0xfff & thresh) << 4) as u32;
    reg5 |= RF_SYNTHESIZER_RESERVED_PROGRAMMING << 16;
    reg2 |= ((0xfff & frac) << 4) as u32;
    reg2 |= ((0x3f & n) << 16) as u32;
    // LBAND_LNA_CALIBRATION_OFF (bit 22) is 0.

    (band, switch_plan.band_select_word, reg0, reg2, reg3, reg5)
}

/// mirisdr_set_hard's sample-rate words for 252_S16: (reg3, reg4).
fn hard_regs(rate: u32) -> (u32, u32) {
    let mut i: u64 = 4;
    let mut vco: u64 = 0;
    while i < 16 {
        vco = rate as u64 * i * 12;
        if vco >= 202_000_000 {
            break;
        }
        i += 2;
    }
    let n = vco / 48_000_000;
    let fract = 0x200000 * (vco % 48_000_000) / 48_000_000;

    let mut reg3: u32 = 0;
    reg3 |= 3; // "??" bits 0:1
    reg3 |= ((0x07 & (i / 2 - 1)) as u32) << 2;
    reg3 |= ((0x01 & (fract >> 20)) as u32) << 7;
    reg3 |= ((0x0f & n) as u32) << 8;
    reg3 |= (0x0f & 0x01) << 12; // 252_S16
    reg3 |= 1 << 16; // "??"
    let reg4 = (0xfffff & fract) as u32;
    (reg3, reg4)
}

/// mirisdr_set_tuner_gain: total gain (0..=102dB) -> (lna, mixbuffer,
/// mixer, baseband) gain REDUCTIONS.
fn gain_reductions(gain_db: u32) -> (u32, u32, u32, u32) {
    let gain = gain_db.min(MAX_GAIN_DB);
    if gain >= 43 {
        (0, 0, 0, 59 - (gain - 43))
    } else if gain >= 19 {
        (1, 3, 0, 59 - (gain - 19))
    } else {
        (1, 3, 1, 59 - gain)
    }
}

// ---------------------------------------------------------------------
// Device
// ---------------------------------------------------------------------

/// Control-transfer side of an open device -- owned exclusively by
/// radio.rs's mirisdr_control_loop, separate from the bulk stream
/// (`RxEndpoint`), same split as rtlsdr.rs.
pub struct MiriSdrDevice {
    interface: Interface,
    band: Band,
    gain_db: u32,
}

pub struct RxEndpoint {
    reader: nusb::io::EndpointRead<Bulk>,
}

impl RxEndpoint {
    pub fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.reader.read(buf)
    }
}

impl MiriSdrDevice {
    /// libusb_control_transfer(dh, 0x42, request, value, index, NULL, 0)
    /// -- 0x42 is vendor/OUT with an endpoint recipient. Windows sends it
    /// with a device recipient (0x40) instead: WinUSB checks an
    /// endpoint-recipient request's wIndex names a real endpoint, and
    /// write_reg packs register data into wIndex, so it failed with
    /// ERROR_INVALID_PARAMETER (87). The MSi2500 decodes these from
    /// bRequest/wValue/wIndex; Linux keeps libmirisdr's exact 0x42.
    fn vendor_out(&self, request: u8, value: u16, index: u16) -> io::Result<()> {
        let recipient = if cfg!(windows) { Recipient::Device } else { Recipient::Endpoint };
        self.interface
            .control_out(
                ControlOut {
                    control_type: ControlType::Vendor,
                    recipient,
                    request,
                    value,
                    index,
                    data: &[],
                },
                CTRL_TIMEOUT,
            )
            .wait()
            .map_err(|e| io_err(format!("MSi2500 control request 0x{request:02x} failed: {e}")))
    }

    /// mirisdr_write_reg
    fn write_reg(&self, reg: u8, val: u32) -> io::Result<()> {
        let value = ((val & 0xff) << 8) as u16 | reg as u16;
        let index = ((val >> 8) & 0xffff) as u16;
        self.vendor_out(CMD_WREG, value, index)
    }

    /// mirisdr_streaming_start / _stop
    fn streaming_start(&self) -> io::Result<()> {
        self.vendor_out(CMD_START_STREAMING, 0, 0)
    }

    fn streaming_stop(&self) -> io::Result<()> {
        self.vendor_out(CMD_STOP_STREAMING, 0, 0)
    }

    /// mirisdr_adc_init
    fn adc_init(&self) -> io::Result<()> {
        self.write_reg(0x08, 0x006080)?;
        self.write_reg(0x05, 0x00000c)?;
        self.write_reg(0x00, 0x000200)?;
        self.write_reg(0x02, 0x004801)?;
        self.write_reg(0x08, 0x00f380)
    }

    /// mirisdr_adc_stop -- puts the USB interface and ADC to sleep.
    fn adc_stop(&self) -> io::Result<()> {
        self.write_reg(0x03, 0x010000)
    }

    /// mirisdr_set_hard, for a stopped stream and the 252_S16 format.
    fn set_hard(&self, rate: u32) -> io::Result<()> {
        self.write_reg(0x07, 0x000094)?;
        let (reg3, reg4) = hard_regs(rate);
        self.write_reg(0x04, reg4)?;
        self.write_reg(0x03, reg3)
    }

    /// mirisdr_set_soft (bias off, so reg8 is just the band-select word).
    fn set_soft(&mut self, freq: u32) -> io::Result<()> {
        let (band, band_select_word, reg0, reg2, reg3, reg5) = soft_regs(freq);
        self.band = band;
        self.write_reg(0x08, band_select_word)?;
        self.write_reg(0x09, 0x0e)?;
        self.write_reg(0x09, reg3)?;
        self.write_reg(0x09, reg0)?;
        self.write_reg(0x09, reg5)?;
        self.write_reg(0x09, reg2)
    }

    /// mirisdr_set_gain (the register write; `self.gain_db` is the
    /// total gain mirisdr_set_tuner_gain splits up).
    fn write_gain(&self) -> io::Result<()> {
        let (lna, mixbuffer, mixer, baseband) = gain_reductions(self.gain_db);
        let mut reg1: u32 = 1;
        reg1 |= baseband << 4;
        // Mixbuffer is on AM1 and AM2 inputs only
        reg1 |= match self.band {
            Band::Am1 => (mixbuffer & 0x03) << 10,
            Band::Am2 => (if mixbuffer == 0 { 0x0 } else { 0x03 }) << 10,
            _ => 0,
        };
        reg1 |= mixer << 12;
        // LNA is not on AM1 nor AM2 inputs
        if !matches!(self.band, Band::Am1 | Band::Am2) {
            reg1 |= lna << 13;
        }
        reg1 |= 2 << 14; // DC_OFFSET_CALIBRATION_PERIODIC2
        // DC_OFFSET_CALIBRATION_SPEEDUP_OFF (bit 17) is 0.
        self.write_reg(0x09, reg1)?;

        // DC Offset Calibration setup
        let reg6: u32 = 6 | (0x1f << 4) | (0x800 << 10);
        self.write_reg(0x09, reg6)
    }

    /// mirisdr_set_center_freq -- retune, then restore gain (a band
    /// change moves which gain stages exist).
    pub fn set_center_freq(&mut self, freq: u32) -> io::Result<()> {
        self.set_soft(freq)?;
        self.write_gain()
    }

    /// mirisdr_set_tuner_gain, total gain in dB (0..=MAX_GAIN_DB).
    pub fn set_gain(&mut self, gain_db: u32) -> io::Result<()> {
        self.gain_db = gain_db.min(MAX_GAIN_DB);
        self.write_gain()
    }

    /// The stream stop libmirisdr does at the end of mirisdr_read_async,
    /// plus mirisdr_adc_stop so the stick isn't left running.
    pub fn close(&mut self) {
        let _ = self.streaming_stop();
        let _ = self.adc_stop();
    }
}

/// Opens the stick in the USB port `mac` names (see usb_port::port_mac).
fn find_and_open(mac: [u8; 6]) -> io::Result<(nusb::Device, String)> {
    let (info, name) = nusb::list_devices()
        .wait()
        .map_err(io_err)?
        .find_map(|d| known_name(&d).filter(|_| usb_port::port_mac(&d, MAC_TAG) == mac).map(|name| (d, name)))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("no MSi2500 (MSI.SDR) device found in USB port {}", usb_port::port_label(mac)),
            )
        })?;
    let device = info.open().wait().map_err(io_err)?;
    Ok((device, name.to_string()))
}

/// mirisdr_open followed by the bring-up rtl_sdr-style clients do (rate,
/// frequency, gain), then mirisdr_read_async's start: bulk alternate
/// setting, stream on, for the stick `mac` (a discover() port MAC)
/// names. Returns the control half, the streaming half, and the
/// device's name.
pub fn initialise(mac: [u8; 6], initial_freq_hz: u32, gain_db: u32) -> io::Result<(MiriSdrDevice, RxEndpoint, String)> {
    // "reset je potřeba, jinak občas zařízení odmítá komunikovat" --
    // libmirisdr always resets on open (otherwise the device sometimes
    // refuses to talk). A reset can re-enumerate it, so look it up again
    // afterwards rather than reusing the handle.
    {
        let (device, _) = find_and_open(mac)?;
        if let Err(e) = device.reset().wait() {
            eprintln!("MSi2500: USB reset failed ({e}), continuing");
        }
    }
    std::thread::sleep(Duration::from_millis(500));
    let (device, name) = find_and_open(mac)?;
    // Linux's msi2500 V4L2 driver binds these sticks by default; detach
    // it (libmirisdr's DETACH_KERNEL_DRIVER build option) rather than
    // making the user blacklist msi2500/msi001.
    let interface = device.detach_and_claim_interface(0).wait().map_err(|e| {
        io_err(format!(
            "couldn't claim the MSi2500's USB interface ({e}) -- {}",
            if cfg!(windows) {
                "on Windows, bind the WinUSB driver to it with Zadig (see the manual), and check no \
                 other SDR program is using it"
            } else if cfg!(target_os = "linux") {
                "check the udev rules (90-mirisdr.rules) give your user access, and check no other SDR \
                 program is using it"
            } else {
                "check no other SDR program is using it"
            }
        ))
    })?;

    let mut dev = MiriSdrDevice { interface: interface.clone(), band: Band::Vhf, gain_db: gain_db.min(MAX_GAIN_DB) };

    // also stop any streaming that may be active from before
    dev.streaming_stop()?;
    dev.adc_stop()?;

    dev.adc_init()?;
    dev.set_hard(SAMPLE_RATE_HZ)?;
    dev.set_center_freq(initial_freq_hz)?;

    interface
        .set_alt_setting(ALT_SETTING_BULK)
        .wait()
        .map_err(|e| io_err(format!("couldn't select the MSi2500's bulk alt setting {ALT_SETTING_BULK}: {e}")))?;
    // Clear any halt / data-toggle state the previous session left on the
    // stream endpoint. Linux resets the device on open, which does this;
    // WinUSB can't reset a device, so on Windows a restart (e.g. a sample
    // rate change) inherited a stalled endpoint ("stream read failed:
    // endpoint stalled"). CLEAR_FEATURE(ENDPOINT_HALT) is harmless when
    // nothing is stalled.
    let mut endpoint = interface
        .endpoint::<Bulk, In>(EP_STREAM_IN)
        .map_err(|e| io_err(format!("couldn't open the MSi2500's stream endpoint: {e}")))?;
    if let Err(e) = endpoint.clear_halt().wait() {
        eprintln!("MSi2500: clearing the stream endpoint failed ({e}), continuing");
    }
    let reader = endpoint
        .reader(STREAM_READ_SIZE)
        .with_num_transfers(STREAM_NUM_TRANSFERS)
        .with_read_timeout(Duration::from_millis(500));
    dev.streaming_start()?;

    Ok((dev, RxEndpoint { reader }, name))
}

// ---------------------------------------------------------------------
// Stream unpacking (convert/252_s16.c)
// ---------------------------------------------------------------------

/// Full scale of the unpacked samples: 14-bit values left-justified in
/// an i16, exactly as mirisdr_samples_convert_252_s16 produces them.
pub const SAMPLE_FULL_SCALE: f64 = 32768.0;

/// Splits the byte stream into 1024-byte blocks and yields each block's
/// I/Q samples, carrying a partial block across reads. Also checks each
/// block's sample counter, as upstream does, and reports gaps (lost USB
/// data) -- rate-limited, since one gap tends to come with more.
pub struct Unpacker {
    pending: Vec<u8>,
    expected_addr: Option<u32>,
    gaps_reported: u32,
}

impl Default for Unpacker {
    fn default() -> Self {
        Self::new()
    }
}

impl Unpacker {
    pub fn new() -> Self {
        Self { pending: Vec::with_capacity(2 * BLOCK_LEN), expected_addr: None, gaps_reported: 0 }
    }

    pub fn process(&mut self, bytes: &[u8], mut emit: impl FnMut(&[(f64, f64)])) {
        let mut samples = Vec::with_capacity(SAMPLES_PER_BLOCK as usize);
        let mut bytes = bytes;
        if !self.pending.is_empty() {
            let need = BLOCK_LEN - self.pending.len();
            let take = need.min(bytes.len());
            self.pending.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
            if self.pending.len() < BLOCK_LEN {
                return;
            }
            let block = std::mem::take(&mut self.pending);
            self.unpack_block(&block, &mut samples);
            emit(&samples);
            self.pending = block;
            self.pending.clear();
        }
        let mut blocks = bytes.chunks_exact(BLOCK_LEN);
        for block in &mut blocks {
            self.unpack_block(block, &mut samples);
            emit(&samples);
        }
        self.pending.extend_from_slice(blocks.remainder());
    }

    fn unpack_block(&mut self, block: &[u8], out: &mut Vec<(f64, f64)>) {
        let addr = u32::from_le_bytes([block[0], block[1], block[2], block[3]]);
        if let Some(expected) = self.expected_addr {
            if addr != expected && self.gaps_reported < 10 {
                self.gaps_reported += 1;
                eprintln!("MSi2500: {} samples lost", addr.wrapping_sub(expected));
            }
        }
        self.expected_addr = Some(addr.wrapping_add(SAMPLES_PER_BLOCK));

        out.clear();
        for word in block[BLOCK_HEADER_LEN..].chunks_exact(4) {
            // dst = (src[0] << 2) | (src[1] << 10), as int16_t
            let i = (u16::from_le_bytes([word[0], word[1]]) << 2) as i16;
            let q = (u16::from_le_bytes([word[2], word[3]]) << 2) as i16;
            out.push((i as f64, q as f64));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;


    #[test]
    fn soft_regs_match_upstream_c() {
        // From compiling libmirisdr's mirisdr_set_soft arithmetic (I/O
        // stripped, default flavour, zero IF, 1.536MHz bandwidth, 24MHz
        // crystal) with gcc: (freq, band word, reg0, reg2, reg3, reg5).
        let cases = [
            (136_000u32, 0xf780u32, 0x04fe10u32, 0x140112u32, 0x000003u32, 0x282ee5u32),
            (7_374_000, 0xf780, 0x04fe10, 0x150e52, 0x000003, 0x283e85),
            (14_374_000, 0xff80, 0x04fe10, 0x164a32, 0x000003, 0x28bb85),
            (28_374_000, 0xff80, 0x04fe10, 0x182d92, 0x000003, 0x283e85),
            (50_613_000, 0xf380, 0x04f420, 0x103672, 0x000003, 0x283e85),
            (100_000_000, 0xf380, 0x04f420, 0x210012, 0x000003, 0x280035),
            (144_474_000, 0xfa80, 0x04f440, 0x1804f2, 0x000003, 0x283e85),
            (260_000_000, 0xf680, 0x04f460, 0x150022, 0x000003, 0x280035),
            (432_474_000, 0xf380, 0x04f480, 0x1204f2, 0x000003, 0x28fa05),
            (1_296_474_000, 0xfa80, 0x04f500, 0x1b0272, 0x008003, 0x28fa05),
            (1_900_000_000, 0xfa80, 0x04f500, 0x270072, 0x000003, 0x2800c5),
        ];
        for (freq, bsw, r0, r2, r3, r5) in cases {
            let (_, b, reg0, reg2, reg3, reg5) = soft_regs(freq);
            assert_eq!((b, reg0, reg2, reg3, reg5), (bsw, r0, r2, r3, r5), "{freq}");
        }
    }

    #[test]
    fn hard_regs_match_upstream_c() {
        assert_eq!(hard_regs(1_536_000), (0x011497, 0x0374bc));
        assert_eq!(hard_regs(3_072_000), (0x01148b, 0x0374bc));
    }

    #[test]
    fn sample_rate_is_in_range_and_uses_252_format() {
        assert!((1_300_000..=6_048_000).contains(&SAMPLE_RATE_HZ));
    }

    #[test]
    fn hf_selects_an_am_band_and_vhf_does_not() {
        assert_eq!(soft_regs(7_074_000).0, Band::Am2);
        assert_eq!(soft_regs(144_174_000).0, Band::B3);
    }

    #[test]
    fn gain_split_covers_0_to_102() {
        // Total = LNA (24 unless reduced) + mixer (19 unless reduced)
        // + baseband (59 - reduction), on a band that has the LNA.
        for g in 0..=MAX_GAIN_DB {
            let (lna, _, mixer, bb) = gain_reductions(g);
            let total = (1 - lna) * 24 + (1 - mixer) * 19 + (59 - bb);
            assert_eq!(total, g);
            assert!(bb <= 59);
        }
    }

    fn block(addr: u32, first: (i16, i16)) -> Vec<u8> {
        let mut b = vec![0u8; BLOCK_LEN];
        b[..4].copy_from_slice(&addr.to_le_bytes());
        // 14-bit sample v is stored as the 16-bit word v & 0x3fff.
        let (i, q) = first;
        b[16..18].copy_from_slice(&((i as u16) & 0x3fff).to_le_bytes());
        b[18..20].copy_from_slice(&((q as u16) & 0x3fff).to_le_bytes());
        b
    }

    #[test]
    fn unpacks_signed_14_bit_samples() {
        let mut u = Unpacker::new();
        let mut got = Vec::new();
        u.process(&block(0, (-8192, 8191)), |s| got.extend_from_slice(s));
        assert_eq!(got.len(), SAMPLES_PER_BLOCK as usize);
        assert_eq!(got[0], (-32768.0, 32764.0));
    }

    #[test]
    fn split_reads_reassemble_blocks() {
        let mut stream = block(0, (100, -100));
        stream.extend(block(252, (5, 6)));
        stream.extend(block(504, (7, 8)));
        let mut whole = Vec::new();
        Unpacker::new().process(&stream, |s| whole.extend_from_slice(s));
        let mut split = Vec::new();
        let mut u = Unpacker::new();
        for chunk in stream.chunks(700) {
            u.process(chunk, |s| split.extend_from_slice(s));
        }
        assert_eq!(whole.len(), 3 * SAMPLES_PER_BLOCK as usize);
        assert_eq!(whole, split);
    }
}
