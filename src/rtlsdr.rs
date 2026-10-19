/*
    USB I/O, tuner control and decimation for RTL2832U-based "RTL-SDR"
    dongles with a Rafael Micro R820T/R820T2/R828D tuner -- e.g. the
    NooElec NESDR Smart (R820T2), RTL-SDR Blog V3/V4, and most generic
    DVB-T sticks. Receive-only.

    Like the RX-888 (see rx888.rs) and unlike every openHPSDR board, the
    dongle has no framing: once started it streams interleaved unsigned
    8-bit I/Q at the configured rate over a bulk endpoint. Unlike the
    RX-888, that stream is ALREADY complex baseband centred on the tuned
    frequency (the R82xx mixes RF down to a low IF, the RTL2832U's own
    DDC brings it to zero), so tuning needs no NCO here -- just a CIC
    decimator from the dongle's fixed DONGLE_RATE_HZ down to whichever
    WDSP-recognised rate the user picked (see `decimation_for_output_rate`),
    plus a fixed offset-tuning shift (see `Decimator`). mirisdr.rs reuses
    that same Decimator.

    PORTED, not invented: every register sequence below is a
    transcription of osmocom librtlsdr v2.0.2 (~/github/rtl-sdr,
    commit 619ac31: src/librtlsdr.c and src/tuner_r82xx.c), which is
    GPL-2.0-or-later -- the same licence as this project. Function names
    match the C ones (minus their rtlsdr_/r82xx_ prefixes where a method
    receiver makes them redundant) so each can be checked side by side
    against its original. Deliberately NOT ported:
    - The E4000/FC0012/FC0013/FC2580 tuners (old, rare sticks) -- `open`
      fails with a clear message naming the problem instead.
    - Offset tuning (librtlsdr itself refuses it for R82xx tuners) and
      the RTL2832U's own digital AGC (left off, as librtlsdr defaults).
    - r82xx_sysfreq_sel's non-DVB-T branches -- r82xx_init only ever
      calls it with SYS_DVBT / TUNER_DIGITAL_TV, so only that path's
      constants are reproduced.
    - Frequency (ppm) correction -- the NESDR Smart v5 this was written
      for has a 0.5ppm TCXO; `corr` is fixed at 0, and the places
      librtlsdr would apply it are noted inline.

    A second, independent pure-Rust port exists in ~/github/sdroxide
    (crates/sdroxide-rtlsdr, sdroxide-r82xx). It is GPL-3.0-or-later, so
    NO code was copied from it (that would force this project to GPL-3);
    it was only consulted to confirm the overall nusb approach is sound.
*/

use nusb::transfer::{Bulk, ControlIn, ControlOut, ControlType, In, Recipient};
use nusb::{Interface, MaybeFuture};
use crate::usb_port;
use std::io::{self, Read};
use std::sync::atomic::{AtomicBool, AtomicU8};
use std::time::Duration;

/// librtlsdr's `known_devices[]` table (librtlsdr.c), verbatim. The
/// NooElec NESDR Smart enumerates as the generic 0bda:2838.
const KNOWN_DEVICES: &[(u16, u16, &str)] = &[
    (0x0bda, 0x2832, "Generic RTL2832U"),
    (0x0bda, 0x2838, "Generic RTL2832U OEM"),
    (0x0413, 0x6680, "DigitalNow Quad DVB-T PCI-E card"),
    (0x0413, 0x6f0f, "Leadtek WinFast DTV Dongle mini D"),
    (0x0458, 0x707f, "Genius TVGo DVB-T03 USB dongle (Ver. B)"),
    (0x0ccd, 0x00a9, "Terratec Cinergy T Stick Black (rev 1)"),
    (0x0ccd, 0x00b3, "Terratec NOXON DAB/DAB+ USB dongle (rev 1)"),
    (0x0ccd, 0x00b4, "Terratec Deutschlandradio DAB Stick"),
    (0x0ccd, 0x00b5, "Terratec NOXON DAB Stick - Radio Energy"),
    (0x0ccd, 0x00b7, "Terratec Media Broadcast DAB Stick"),
    (0x0ccd, 0x00b8, "Terratec BR DAB Stick"),
    (0x0ccd, 0x00b9, "Terratec WDR DAB Stick"),
    (0x0ccd, 0x00c0, "Terratec MuellerVerlag DAB Stick"),
    (0x0ccd, 0x00c6, "Terratec Fraunhofer DAB Stick"),
    (0x0ccd, 0x00d3, "Terratec Cinergy T Stick RC (Rev.3)"),
    (0x0ccd, 0x00d7, "Terratec T Stick PLUS"),
    (0x0ccd, 0x00e0, "Terratec NOXON DAB/DAB+ USB dongle (rev 2)"),
    (0x1554, 0x5020, "PixelView PV-DT235U(RN)"),
    (0x15f4, 0x0131, "Astrometa DVB-T/DVB-T2"),
    (0x15f4, 0x0133, "HanfTek DAB+FM+DVB-T"),
    (0x185b, 0x0620, "Compro Videomate U620F"),
    (0x185b, 0x0650, "Compro Videomate U650F"),
    (0x185b, 0x0680, "Compro Videomate U680F"),
    (0x1b80, 0xd393, "GIGABYTE GT-U7300"),
    (0x1b80, 0xd394, "DIKOM USB-DVBT HD"),
    (0x1b80, 0xd395, "Peak 102569AGPK"),
    (0x1b80, 0xd397, "KWorld KW-UB450-T USB DVB-T Pico TV"),
    (0x1b80, 0xd398, "Zaapa ZT-MINDVBZP"),
    (0x1b80, 0xd39d, "SVEON STV20 DVB-T USB & FM"),
    (0x1b80, 0xd3a4, "Twintech UT-40"),
    (0x1b80, 0xd3a8, "ASUS U3100MINI_PLUS_V2"),
    (0x1b80, 0xd3af, "SVEON STV27 DVB-T USB & FM"),
    (0x1b80, 0xd3b0, "SVEON STV21 DVB-T USB & FM"),
    (0x1d19, 0x1101, "Dexatek DK DVB-T Dongle (Logilink VG0002A)"),
    (0x1d19, 0x1102, "Dexatek DK DVB-T Dongle (MSI DigiVox mini II V3.0)"),
    (0x1d19, 0x1103, "Dexatek Technology Ltd. DK 5217 DVB-T Dongle"),
    (0x1d19, 0x1104, "MSI DigiVox Micro HD"),
    (0x1f4d, 0xa803, "Sweex DVB-T USB"),
    (0x1f4d, 0xb803, "GTek T803"),
    (0x1f4d, 0xc803, "Lifeview LV5TDeluxe"),
    (0x1f4d, 0xd286, "MyGica TD312"),
    (0x1f4d, 0xd803, "PROlectrix DV107669"),
];

/// The rate the dongle itself always runs at. 1,536,000 = 48,000 * 32,
/// so every WDSP-recognised rate from 48k to 1536k is an exact integer
/// decimation of it (see rx888.rs's CIC_DECIMATION doc comment for the
/// real WDSP crash an unrecognised rate causes). It's also inside the
/// RTL2832U resampler's valid 900,001-3,200,000 range and divides its
/// 28.8MHz crystal exactly (rsamp_ratio comes out a clean multiple of
/// 4), so `set_sample_rate` reports no rounding.
pub const DONGLE_RATE_HZ: u32 = 1_536_000;

/// Output rates offered in Settings -> RX. 768k/1536k are exposed here
/// even though no P1 board offers them, because this board has no P1
/// wire encoding to be limited by.
pub const SUPPORTED_OUTPUT_RATES: [u32; 6] = [48_000, 96_000, 192_000, 384_000, 768_000, 1_536_000];

/// Fresh-connect default -- wide enough to be a useful panadapter
/// without leaning on CPU the way 1536k would.
pub const DEFAULT_OUTPUT_RATE_HZ: u32 = 192_000;

pub fn decimation_for_output_rate(output_rate_hz: u32) -> Option<u32> {
    SUPPORTED_OUTPUT_RATES.contains(&output_rate_hz).then(|| DONGLE_RATE_HZ / output_rate_hz)
}

/// Tuned below this, the R82xx can't help (its PLL bottoms out around
/// 24MHz) and direct sampling takes over automatically if enabled --
/// see `Controls::direct_sampling`.
pub const DIRECT_SAMPLING_BELOW_HZ: u32 = 24_000_000;

/// Top of the R820T2's usable range (librtlsdr has no hard limit; this
/// is the commonly quoted figure, and above it the PLL simply fails to
/// lock -- harmless, just silence).
pub const FREQUENCY_MAX_HZ: u64 = 1_766_000_000;

/// librtlsdr's r82xx_gains[] (rtlsdr_get_tuner_gains), tenths of a dB.
/// Every value r82xx_set_gain's LNA/mixer step walk can land on --
/// kept only to test that walk against (the UI just takes whole dB and
/// lets set_gain snap it, exactly as librtlsdr clients do).
#[cfg(test)]
const R82XX_GAINS_TENTH_DB: [i32; 29] = [
    0, 9, 14, 27, 37, 77, 87, 125, 144, 157, 166, 197, 207, 229, 254, 280, 297, 328, 338, 364, 372, 386, 402, 421,
    434, 439, 445, 480, 496,
];

/// Live, RTL-SDR-only settings polled by radio.rs's rtlsdr_control_loop.
/// RF gain itself lives in RadioSession::rx_attenuation (as whole dB,
/// 0-50) rather than here, so MIDI and config persistence pick it up
/// for free, same as the RX-888's attenuator.
pub struct Controls {
    /// 0 = off, 1 = I-branch, 2 = Q-branch (librtlsdr's own numbering
    /// for rtlsdr_set_direct_sampling). Only takes effect while tuned
    /// below DIRECT_SAMPLING_BELOW_HZ; above it the tuner is always used.
    /// The NESDR Smart needs a hardware mod to get anything useful from
    /// either branch -- on an unmodified dongle this just shows noise.
    pub direct_sampling: AtomicU8,
    /// Hands gain control to the R82xx's own LNA/mixer AGC instead of
    /// the RF Gain value (rtlsdr_set_tuner_gain_mode(dev, 0)).
    pub tuner_agc: AtomicBool,
}

const CTRL_TIMEOUT: Duration = Duration::from_millis(300);
const EP_STREAM_IN: u8 = 0x81;

/// Per-transfer size for the bulk stream. librtlsdr's own default is
/// 256KiB, but at 3MB/s that is ~85ms per transfer, and rx888.rs
/// already found bursty delivery hurts real-time consumers (TCI,
/// local audio) -- see its STREAM_READ_SIZE doc comment. 16KiB is
/// ~5ms. Must be a multiple of 512 (the RTL2832U's bulk packet size).
pub const STREAM_READ_SIZE: usize = 16 * 1024;
/// Transfers kept queued in the kernel, so the host keeps draining the
/// dongle's small FIFO while this side is busy (processing, or the
/// control thread holding the bus for a retune). 16 * 5ms = ~85ms of
/// slack, matching librtlsdr's own total buffering of ~15 transfers.
const STREAM_NUM_TRANSFERS: usize = 16;

const DEF_RTL_XTAL_FREQ: u32 = 28_800_000;

// enum blocks (librtlsdr.c)
const USBB: u8 = 1;
const SYSB: u8 = 2;
const IICB: u8 = 6;

// enum usb_reg / sys_reg (librtlsdr.c) -- only those used below.
const USB_SYSCTL: u16 = 0x2000;
const USB_EPA_CTL: u16 = 0x2148;
const USB_EPA_MAXPKT: u16 = 0x2158;
const DEMOD_CTL: u16 = 0x3000;
const GPO: u16 = 0x3001;
const GPOE: u16 = 0x3003;
const GPD: u16 = 0x3004;
const DEMOD_CTL_1: u16 = 0x300b;

/// librtlsdr.c's fir_default[] -- the Windows driver's DAB/FM FIR.
const FIR_DEFAULT: [i32; 16] = [-54, -36, -41, -40, -32, -14, 14, 53, 101, 156, 215, 273, 327, 372, 404, 421];

fn io_err(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
}

fn is_known(info: &nusb::DeviceInfo) -> bool {
    KNOWN_DEVICES.iter().any(|&(vid, pid, _)| vid == info.vendor_id() && pid == info.product_id())
}

/// Last byte of every RTL-SDR sentinel MAC -- see usb_port::port_mac.
pub const MAC_TAG: u8 = 2;

/// Every plugged-in dongle matching KNOWN_DEVICES, as usb_port::port_mac
/// values in port order -- for discovery.rs's synthetic Device rows. No
/// open/claim, safe to poll.
pub fn discover() -> Vec<[u8; 6]> {
    let Ok(devices) = nusb::list_devices().wait() else {
        return Vec::new();
    };
    let mut found: Vec<_> = devices.filter(is_known).map(|d| usb_port::port_mac(&d, MAC_TAG)).collect();
    found.sort();
    found
}

// ---------------------------------------------------------------------
// Register access (librtlsdr.c: rtlsdr_read_array .. rtlsdr_set_i2c_repeater)
// ---------------------------------------------------------------------

struct Usb {
    interface: Interface,
}

impl Usb {
    fn ctrl_in(&self, value: u16, index: u16, length: u16) -> io::Result<Vec<u8>> {
        self.interface
            .control_in(
                ControlIn { control_type: ControlType::Vendor, recipient: Recipient::Device, request: 0, value, index, length },
                CTRL_TIMEOUT,
            )
            .wait()
            .map_err(io_err)
    }

    fn ctrl_out(&self, value: u16, index: u16, data: &[u8]) -> io::Result<()> {
        self.interface
            .control_out(
                ControlOut { control_type: ControlType::Vendor, recipient: Recipient::Device, request: 0, value, index, data },
                CTRL_TIMEOUT,
            )
            .wait()
            .map_err(io_err)
    }

    fn read_array(&self, block: u8, addr: u16, len: u16) -> io::Result<Vec<u8>> {
        self.ctrl_in(addr, (block as u16) << 8, len)
    }

    fn write_array(&self, block: u8, addr: u16, data: &[u8]) -> io::Result<()> {
        self.ctrl_out(addr, ((block as u16) << 8) | 0x10, data)
    }

    /// The C version assembles `(data[1] << 8) | data[0]` even for
    /// len 1, leaving the high byte as stack garbage; callers only ever
    /// use the low byte in that case, so 0 here is equivalent.
    fn read_reg(&self, block: u8, addr: u16, len: u16) -> io::Result<u16> {
        let d = self.read_array(block, addr, len)?;
        Ok(d.first().copied().unwrap_or(0) as u16 | (d.get(1).copied().unwrap_or(0) as u16) << 8)
    }

    fn write_reg(&self, block: u8, addr: u16, val: u16, len: u8) -> io::Result<()> {
        let data = if len == 1 { vec![(val & 0xff) as u8] } else { vec![(val >> 8) as u8, (val & 0xff) as u8] };
        self.write_array(block, addr, &data)
    }

    fn demod_read_reg(&self, page: u8, addr: u16, len: u16) -> io::Result<u16> {
        let d = self.ctrl_in((addr << 8) | 0x20, page as u16, len)?;
        Ok(d.first().copied().unwrap_or(0) as u16 | (d.get(1).copied().unwrap_or(0) as u16) << 8)
    }

    fn demod_write_reg(&self, page: u8, addr: u16, val: u16, len: u8) -> io::Result<()> {
        let data = if len == 1 { vec![(val & 0xff) as u8] } else { vec![(val >> 8) as u8, (val & 0xff) as u8] };
        self.ctrl_out((addr << 8) | 0x20, 0x10 | page as u16, &data)?;
        // librtlsdr does this dummy read after every demod write.
        let _ = self.demod_read_reg(0x0a, 0x01, 1);
        Ok(())
    }

    fn i2c_write(&self, i2c_addr: u8, buf: &[u8]) -> io::Result<()> {
        self.write_array(IICB, i2c_addr as u16, buf)
    }

    fn i2c_read(&self, i2c_addr: u8, len: u16) -> io::Result<Vec<u8>> {
        self.read_array(IICB, i2c_addr as u16, len)
    }

    /// rtlsdr_i2c_read_reg -- used only for tuner probing, where a
    /// missing chip makes the transfer fail; librtlsdr ignores that and
    /// returns 0, which is what lets the probe move on to the next address.
    fn i2c_read_reg(&self, i2c_addr: u8, reg: u8) -> u8 {
        let _ = self.write_array(IICB, i2c_addr as u16, &[reg]);
        self.read_array(IICB, i2c_addr as u16, 1).ok().and_then(|d| d.first().copied()).unwrap_or(0)
    }

    fn set_gpio_bit(&self, gpio: u8, on: bool) -> io::Result<()> {
        let bit = 1u16 << gpio;
        let r = self.read_reg(SYSB, GPO, 1)?;
        self.write_reg(SYSB, GPO, if on { r | bit } else { r & !bit }, 1)
    }

    fn set_gpio_output(&self, gpio: u8) -> io::Result<()> {
        let bit = 1u16 << gpio;
        let r = self.read_reg(SYSB, GPD, 1)?;
        self.write_reg(SYSB, GPD, r & !bit, 1)?;
        let r = self.read_reg(SYSB, GPOE, 1)?;
        self.write_reg(SYSB, GPOE, r | bit, 1)
    }

    fn set_i2c_repeater(&self, on: bool) -> io::Result<()> {
        self.demod_write_reg(1, 0x01, if on { 0x18 } else { 0x10 }, 1)
    }

    /// rtlsdr_set_fir
    fn set_fir(&self) -> io::Result<()> {
        let mut fir = [0u8; 20];
        for i in 0..8 {
            fir[i] = FIR_DEFAULT[i] as u8;
        }
        let mut i = 0;
        while i < 8 {
            let val0 = FIR_DEFAULT[8 + i];
            let val1 = FIR_DEFAULT[8 + i + 1];
            fir[8 + i * 3 / 2] = (val0 >> 4) as u8;
            fir[8 + i * 3 / 2 + 1] = ((val0 << 4) | ((val1 >> 8) & 0x0f)) as u8;
            fir[8 + i * 3 / 2 + 2] = val1 as u8;
            i += 2;
        }
        for (i, &b) in fir.iter().enumerate() {
            self.demod_write_reg(1, 0x1c + i as u16, b as u16, 1)?;
        }
        Ok(())
    }

    /// rtlsdr_init_baseband
    fn init_baseband(&self) -> io::Result<()> {
        // initialize USB
        self.write_reg(USBB, USB_SYSCTL, 0x09, 1)?;
        self.write_reg(USBB, USB_EPA_MAXPKT, 0x0002, 2)?;
        self.write_reg(USBB, USB_EPA_CTL, 0x1002, 2)?;
        // poweron demod
        self.write_reg(SYSB, DEMOD_CTL_1, 0x22, 1)?;
        self.write_reg(SYSB, DEMOD_CTL, 0xe8, 1)?;
        // reset demod (bit 3, soft_rst)
        self.demod_write_reg(1, 0x01, 0x14, 1)?;
        self.demod_write_reg(1, 0x01, 0x10, 1)?;
        // disable spectrum inversion and adjacent channel rejection
        self.demod_write_reg(1, 0x15, 0x00, 1)?;
        self.demod_write_reg(1, 0x16, 0x0000, 2)?;
        // clear both DDC shift and IF frequency registers
        for i in 0..6 {
            self.demod_write_reg(1, 0x16 + i, 0x00, 1)?;
        }
        self.set_fir()?;
        // enable SDR mode, disable DAGC (bit 5)
        self.demod_write_reg(0, 0x19, 0x05, 1)?;
        // init FSM state-holding register
        self.demod_write_reg(1, 0x93, 0xf0, 1)?;
        self.demod_write_reg(1, 0x94, 0x0f, 1)?;
        // disable AGC (en_dagc, bit 0)
        self.demod_write_reg(1, 0x11, 0x00, 1)?;
        // disable RF and IF AGC loop
        self.demod_write_reg(1, 0x04, 0x00, 1)?;
        // disable PID filter (enable_PID = 0)
        self.demod_write_reg(0, 0x61, 0x60, 1)?;
        // opt_adc_iq = 0, default ADC_I/ADC_Q datapath
        self.demod_write_reg(0, 0x06, 0x80, 1)?;
        // Enable Zero-IF mode, DC cancellation, IQ estimation/compensation
        self.demod_write_reg(1, 0xb1, 0x1b, 1)?;
        // disable 4.096 MHz clock output on pin TP_CK0
        self.demod_write_reg(0, 0x0d, 0x83, 1)
    }
}

// ---------------------------------------------------------------------
// R820T/R828D tuner (tuner_r82xx.c)
// ---------------------------------------------------------------------

const R820T_I2C_ADDR: u8 = 0x34;
const R828D_I2C_ADDR: u8 = 0x74;
const R828D_XTAL_FREQ: u32 = 16_000_000;
const R82XX_CHECK_ADDR: u8 = 0x00;
const R82XX_CHECK_VAL: u8 = 0x69;
const R82XX_IF_FREQ: u32 = 3_570_000;
const REG_SHADOW_START: u8 = 5;
const NUM_REGS: usize = 30;
const VER_NUM: u8 = 49;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TunerChip {
    R820T,
    R828D,
}

/// Initial values from REG_SHADOW_START (0x05) to 0x22.
const R82XX_INIT_ARRAY: [u8; NUM_REGS] = [
    0x83, 0x32, 0x75, // 05 to 07
    0xc0, 0x40, 0xd6, 0x6c, // 08 to 0b
    0xf5, 0x63, 0x75, 0x68, // 0c to 0f
    0x6c, 0x83, 0x80, 0x00, // 10 to 13
    0x0f, 0x00, 0xc0, 0x30, // 14 to 17
    0x48, 0xcc, 0x60, 0x00, // 18 to 1b
    0x54, 0xae, 0x4a, 0xc0, // 1c to 1f
    0x00, 0x00, 0x00, // 20 to 22: the C array is NUM_REGS long with 27 initialisers, so these are zero
];

/// struct r82xx_freq_range: (start MHz, open_d, rf_mux_ploy, tf_c,
/// xtal_cap20p, xtal_cap10p, xtal_cap0p)
const FREQ_RANGES: [(u32, u8, u8, u8, u8, u8, u8); 21] = [
    (0, 0x08, 0x02, 0xdf, 0x02, 0x01, 0x00),
    (50, 0x08, 0x02, 0xbe, 0x02, 0x01, 0x00),
    (55, 0x08, 0x02, 0x8b, 0x02, 0x01, 0x00),
    (60, 0x08, 0x02, 0x7b, 0x02, 0x01, 0x00),
    (65, 0x08, 0x02, 0x69, 0x02, 0x01, 0x00),
    (70, 0x08, 0x02, 0x58, 0x02, 0x01, 0x00),
    (75, 0x00, 0x02, 0x44, 0x02, 0x01, 0x00),
    (80, 0x00, 0x02, 0x44, 0x02, 0x01, 0x00),
    (90, 0x00, 0x02, 0x34, 0x01, 0x01, 0x00),
    (100, 0x00, 0x02, 0x34, 0x01, 0x01, 0x00),
    (110, 0x00, 0x02, 0x24, 0x01, 0x01, 0x00),
    (120, 0x00, 0x02, 0x24, 0x01, 0x01, 0x00),
    (140, 0x00, 0x02, 0x14, 0x01, 0x01, 0x00),
    (180, 0x00, 0x02, 0x13, 0x00, 0x00, 0x00),
    (220, 0x00, 0x02, 0x13, 0x00, 0x00, 0x00),
    (250, 0x00, 0x02, 0x11, 0x00, 0x00, 0x00),
    (280, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00),
    (310, 0x00, 0x41, 0x00, 0x00, 0x00, 0x00),
    (450, 0x00, 0x41, 0x00, 0x00, 0x00, 0x00),
    (588, 0x00, 0x40, 0x00, 0x00, 0x00, 0x00),
    (650, 0x00, 0x40, 0x00, 0x00, 0x00, 0x00),
];

const R82XX_LNA_GAIN_STEPS: [i32; 16] = [0, 9, 13, 40, 38, 13, 31, 22, 26, 31, 26, 14, 19, 5, 35, 13];
const R82XX_MIXER_GAIN_STEPS: [i32; 16] = [0, 5, 10, 10, 19, 9, 10, 25, 17, 10, 8, 16, 13, 6, 3, -8];

/// Bandwidth contribution by low-pass filter (r82xx_if_low_pass_bw_table).
const R82XX_IF_LOW_PASS_BW_TABLE: [i32; 10] =
    [1_700_000, 1_600_000, 1_550_000, 1_450_000, 1_200_000, 900_000, 700_000, 550_000, 450_000, 350_000];
const FILT_HP_BW1: i32 = 350_000;
const FILT_HP_BW2: i32 = 380_000;

const HF: u8 = 1;
const VHF: u8 = 2;
const UHF: u8 = 3;

/// r82xx_set_pll's mixer-divider search: the power of two (2..64)
/// that puts the VCO in 1.77-3.54GHz, and log2(mix_div)-1. If none fits
/// (out-of-range request), upstream's u8 loop ends with mix_div = 128.
fn pll_divider(freq_khz: u32) -> (u32, u8) {
    let vco_min: u32 = 1_770_000; // kHz
    let vco_max: u32 = vco_min * 2;
    let mut mix_div: u32 = 2;
    let mut div_num: u8 = 0;
    while mix_div <= 64 {
        if freq_khz * mix_div >= vco_min && freq_khz * mix_div < vco_max {
            let mut div_buf = mix_div;
            while div_buf > 2 {
                div_buf >>= 1;
                div_num += 1;
            }
            break;
        }
        mix_div <<= 1;
    }
    (mix_div, div_num)
}

/// vco_freq / (2 * pll_ref) as nint + sdm/65536, rounded:
/// vco_div = 65536*nint + sdm = int((pll_ref + 65536*vco_freq) / (2*pll_ref)).
/// nint truncated to u8 exactly as upstream's `uint8_t nint` does.
fn pll_nint_sdm(freq: u32, mix_div: u32, pll_ref: u32) -> (u8, u32) {
    let vco_freq = freq as u64 * mix_div as u64;
    let vco_div = (pll_ref as u64 + 65536 * vco_freq) / (2 * pll_ref as u64);
    ((vco_div / 65536) as u32 as u8, (vco_div % 65536) as u32)
}

/// R0x14: ni + (si << 6), with upstream's int-then-u8 truncation.
fn pll_ni_si(nint: u8) -> u8 {
    let ni = (nint as i32 - 13) / 4;
    let si = nint as i32 - 4 * ni - 13;
    (ni + (si << 6)) as u8
}

fn mask_reg8(reg: u8, val: u8, mask: u8) -> u8 {
    (reg & !mask) | (val & mask)
}

fn bitrev(byte: u8) -> u8 {
    const LUT: [u8; 16] = [0x0, 0x8, 0x4, 0xc, 0x2, 0xa, 0x6, 0xe, 0x1, 0x9, 0x5, 0xd, 0x3, 0xb, 0x7, 0xf];
    (LUT[(byte & 0xf) as usize] << 4) | LUT[(byte >> 4) as usize]
}

/// struct r82xx_priv + r82xx_config. xtal_cap_sel is always
/// XTAL_HIGH_CAP_0P (r82xx_init hardcodes it; r82xx_xtal_check is
/// unused upstream too), so it isn't stored.
struct R82xx {
    chip: TunerChip,
    i2c_addr: u8,
    xtal: u32,
    regs: [u8; NUM_REGS],
    int_freq: u32,
    fil_cal_code: u8,
    input: u8,
    has_lock: bool,
    init_done: bool,
    is_blog_v4: bool,
}

/// max_i2c_msg_len (r820t_init sets 8).
const MAX_I2C_MSG_LEN: usize = 8;

impl R82xx {
    fn shadow_equal(&self, reg: u8, val: &[u8]) -> bool {
        let r = reg as i32 - REG_SHADOW_START as i32;
        if r < 0 || val.len() > NUM_REGS - r as usize {
            return false;
        }
        self.regs[r as usize..r as usize + val.len()] == *val
    }

    fn shadow_store(&mut self, reg: u8, val: &[u8]) {
        let mut r = reg as i32 - REG_SHADOW_START as i32;
        let mut val = val;
        if r < 0 {
            let skip = (-r) as usize;
            if skip >= val.len() {
                return;
            }
            val = &val[skip..];
            r = 0;
        }
        let r = r as usize;
        let len = val.len().min(NUM_REGS - r);
        self.regs[r..r + len].copy_from_slice(&val[..len]);
    }

    fn write(&mut self, usb: &Usb, reg: u8, val: &[u8]) -> io::Result<()> {
        // Avoid setting registers unnecessarily since it's slow
        if self.shadow_equal(reg, val) {
            return Ok(());
        }
        self.shadow_store(reg, val);
        let mut reg = reg;
        for chunk in val.chunks(MAX_I2C_MSG_LEN - 1) {
            let mut buf = Vec::with_capacity(chunk.len() + 1);
            buf.push(reg);
            buf.extend_from_slice(chunk);
            usb.i2c_write(self.i2c_addr, &buf)?;
            reg += chunk.len() as u8;
        }
        Ok(())
    }

    fn write_reg(&mut self, usb: &Usb, reg: u8, val: u8) -> io::Result<()> {
        self.write(usb, reg, &[val])
    }

    fn write_reg_mask(&mut self, usb: &Usb, reg: u8, val: u8, bit_mask: u8) -> io::Result<()> {
        let idx = reg as i32 - REG_SHADOW_START as i32;
        if idx < 0 || idx as usize >= NUM_REGS {
            return Err(io_err(format!("r82xx: register 0x{reg:02x} not shadowed")));
        }
        let rc = self.regs[idx as usize];
        self.write(usb, reg, &[(rc & !bit_mask) | (val & bit_mask)])
    }

    /// Reads always start at R0 and come back bit-reversed.
    fn read(&self, usb: &Usb, reg: u8, len: u16) -> io::Result<Vec<u8>> {
        usb.i2c_write(self.i2c_addr, &[reg])?;
        let data = usb.i2c_read(self.i2c_addr, len)?;
        if data.len() != len as usize {
            return Err(io_err(format!("r82xx: i2c read returned {} of {len} bytes", data.len())));
        }
        Ok(data.into_iter().map(bitrev).collect())
    }

    /// r82xx_set_mux
    fn set_mux(&mut self, usb: &Usb, freq: u32) -> io::Result<()> {
        let freq_mhz = freq / 1_000_000;
        let mut i = 0;
        while i < FREQ_RANGES.len() - 1 {
            if freq_mhz < FREQ_RANGES[i + 1].0 {
                break;
            }
            i += 1;
        }
        let (_, open_d, rf_mux_ploy, tf_c, _cap20p, _cap10p, cap0p) = FREQ_RANGES[i];

        // Open Drain
        self.write_reg_mask(usb, 0x17, open_d, 0x08)?;
        // RF_MUX,Polymux
        self.write_reg_mask(usb, 0x1a, rf_mux_ploy, 0xc3)?;
        // TF BAND
        self.write_reg(usb, 0x1b, tf_c)?;
        // XTAL CAP & Drive -- XTAL_HIGH_CAP_0P case
        self.write_reg_mask(usb, 0x10, cap0p, 0x0b)?;
        self.write_reg_mask(usb, 0x08, 0x00, 0x3f)?;
        self.write_reg_mask(usb, 0x09, 0x00, 0x3f)
    }

    /// r82xx_set_pll. The C version's u8 truncations/wraparounds (nint,
    /// ni, si, div_num) are reproduced deliberately -- they only matter
    /// for out-of-range requests, where they decide exactly which junk
    /// gets written before the lock check fails, but matching them keeps
    /// this a faithful port rather than a reinterpretation.
    fn set_pll(&mut self, usb: &Usb, freq: u32) -> io::Result<()> {
        let freq_khz = (freq + 500) / 1000;
        let pll_ref = self.xtal;
        let refdiv2: u8 = 0;
        let mut vco_power_ref: u8 = 2;

        // set pll autotune = 128kHz
        self.write_reg_mask(usb, 0x1a, 0x00, 0x0c)?;

        // regs 0x10 to 0x16
        let base = (0x10 - REG_SHADOW_START) as usize;
        let mut regs = [0u8; 7];
        regs.copy_from_slice(&self.regs[base..base + 7]);

        regs[0] = mask_reg8(regs[0], refdiv2, 0x10);
        // set VCO current = 100
        regs[2] = mask_reg8(regs[2], 0x80, 0xe0);

        // Calculate divider
        let (mix_div, mut div_num) = pll_divider(freq_khz);

        let data = self.read(usb, 0x00, 5)?;

        if self.chip == TunerChip::R828D {
            vco_power_ref = 1;
        }

        let vco_fine_tune = (data[4] & 0x30) >> 4;
        if vco_fine_tune > vco_power_ref {
            div_num = div_num.wrapping_sub(1);
        } else if vco_fine_tune < vco_power_ref {
            div_num = div_num.wrapping_add(1);
        }

        regs[0] = mask_reg8(regs[0], ((div_num as u32) << 5) as u8, 0xe0);

        let (nint, sdm) = pll_nint_sdm(freq, mix_div, pll_ref);
        if nint as i32 > (128 / vco_power_ref as i32) - 1 {
            return Err(io_err(format!("r82xx: no valid PLL values for {freq} Hz")));
        }
        regs[4] = pll_ni_si(nint);

        // pw_sdm
        let val = if sdm == 0 { 0x08 } else { 0x00 };
        regs[2] = mask_reg8(regs[2], val, 0x08);
        regs[5] = (sdm & 0xff) as u8;
        regs[6] = (sdm >> 8) as u8;

        self.write(usb, 0x10, &regs)?;

        let mut data = [0u8; 3];
        for i in 0..2 {
            let d = self.read(usb, 0x00, 3)?;
            data.copy_from_slice(&d);
            // Check if PLL has locked
            if data[2] & 0x40 != 0 {
                break;
            }
            if i == 0 {
                // Didn't lock. Increase VCO current
                self.write_reg_mask(usb, 0x12, 0x60, 0xe0)?;
            }
        }

        if data[2] & 0x40 == 0 {
            eprintln!("[R82XX] PLL not locked at {freq} Hz");
            self.has_lock = false;
            return Ok(());
        }
        self.has_lock = true;

        // set pll autotune = 8kHz
        self.write_reg_mask(usb, 0x1a, 0x08, 0x08)
    }

    /// r82xx_sysfreq_sel -- only the (TUNER_DIGITAL_TV, SYS_DVBT, freq 0)
    /// path r82xx_init actually takes; see the module doc comment.
    fn sysfreq_sel(&mut self, usb: &Usb) -> io::Result<()> {
        let mixer_top = 0x24; // mixer top:13 , top-1, low-discharge
        let lna_top = 0xe5; // detect bw 3, lna top:4, predet top:2
        let cp_cur = 0x38; // 111, auto
        let div_buf_cur = 0x30; // 11, 150u
        let lna_vth_l = 0x53; // lna vth 0.84, vtl 0.64
        let mixer_vth_l = 0x75; // mixer vth 1.04, vtl 0.84
        let air_cable1_in = 0x00;
        let cable2_in = 0x00;
        let lna_discharge = 14;
        let filter_cur = 0x40; // 10, low

        // use_predetect is 0 (r820t_init), so no pre_dect write here.
        self.write_reg_mask(usb, 0x1d, lna_top, 0xc7)?;
        self.write_reg_mask(usb, 0x1c, mixer_top, 0xf8)?;
        self.write_reg(usb, 0x0d, lna_vth_l)?;
        self.write_reg(usb, 0x0e, mixer_vth_l)?;

        self.input = air_cable1_in;

        // Air-IN only for Astrometa
        self.write_reg_mask(usb, 0x05, air_cable1_in, 0x60)?;
        self.write_reg_mask(usb, 0x06, cable2_in, 0x08)?;
        self.write_reg_mask(usb, 0x11, cp_cur, 0x38)?;
        self.write_reg_mask(usb, 0x17, div_buf_cur, 0x30)?;
        self.write_reg_mask(usb, 0x0a, filter_cur, 0x60)?;

        // Set LNA (type != TUNER_ANALOG_TV)
        // LNA TOP: lowest
        self.write_reg_mask(usb, 0x1d, 0, 0x38)?;
        // 0: normal mode
        self.write_reg_mask(usb, 0x1c, 0, 0x04)?;
        // 0: PRE_DECT off
        self.write_reg_mask(usb, 0x06, 0, 0x40)?;
        // agc clk 250hz
        self.write_reg_mask(usb, 0x1a, 0x30, 0x30)?;
        // write LNA TOP = 3
        self.write_reg_mask(usb, 0x1d, 0x18, 0x38)?;
        // write discharge mode (mask as upstream, which flags it FIXME)
        self.write_reg_mask(usb, 0x1c, mixer_top, 0x04)?;
        // LNA discharge current
        self.write_reg_mask(usb, 0x1e, lna_discharge, 0x1f)?;
        // agc clk 60hz
        self.write_reg_mask(usb, 0x1a, 0x20, 0x30)
    }

    /// r82xx_set_tv_standard(priv, 3, TUNER_DIGITAL_TV, 0)
    fn set_tv_standard(&mut self, usb: &Usb) -> io::Result<()> {
        // BW < 6 MHz
        let if_khz = 3570;
        let filt_cal_lo = 56000; // 52000->56000
        let filt_gain = 0x10; // +3db, 6mhz on
        let img_r = 0x00; // image negative
        let filt_q = 0x10; // r10[4]:low q(1'b1)
        let hp_cor = 0x6b; // 1.7m disable, +2cap, 1.0mhz
        let ext_enable = 0x60; // r30[6]=1 ext enable; r30[5]:1 ext at lna max-1
        let loop_through = 0x01; // r5[7], lt off
        let lt_att = 0x00; // r31[7], lt att enable
        let flt_ext_widest = 0x00; // r15[7]: flt_ext_wide off
        let polyfil_cur = 0x60; // r25[6:5]:min

        // Initialize the shadow registers
        self.regs = R82XX_INIT_ARRAY;

        // Init Flag & Xtal_check Result
        self.write_reg_mask(usb, 0x0c, 0x00, 0x0f)?;
        // version
        self.write_reg_mask(usb, 0x13, VER_NUM, 0x3f)?;
        // for LT Gain test
        self.write_reg_mask(usb, 0x1d, 0x00, 0x38)?;
        self.int_freq = if_khz * 1000;

        // librtlsdr always forces filter calibration here.
        for _ in 0..2 {
            // Set filt_cap
            self.write_reg_mask(usb, 0x0b, hp_cor, 0x60)?;
            // set cali clk =on
            self.write_reg_mask(usb, 0x0f, 0x04, 0x04)?;
            // X'tal cap 0pF for PLL
            self.write_reg_mask(usb, 0x10, 0x00, 0x03)?;

            self.set_pll(usb, filt_cal_lo * 1000)?;
            if !self.has_lock {
                // Upstream returns success here without finishing the
                // standard setup -- kept as-is.
                return Ok(());
            }

            // Start Trigger
            self.write_reg_mask(usb, 0x0b, 0x10, 0x10)?;
            // Stop Trigger
            self.write_reg_mask(usb, 0x0b, 0x00, 0x10)?;
            // set cali clk =off
            self.write_reg_mask(usb, 0x0f, 0x00, 0x04)?;

            // Check if calibration worked
            let data = self.read(usb, 0x00, 5)?;
            self.fil_cal_code = data[4] & 0x0f;
            if self.fil_cal_code != 0 && self.fil_cal_code != 0x0f {
                break;
            }
        }
        // narrowest
        if self.fil_cal_code == 0x0f {
            self.fil_cal_code = 0;
        }

        self.write_reg_mask(usb, 0x0a, filt_q | self.fil_cal_code, 0x1f)?;
        // Set BW, Filter_gain, & HP corner
        self.write_reg_mask(usb, 0x0b, hp_cor, 0xef)?;
        // Set Img_R
        self.write_reg_mask(usb, 0x07, img_r, 0x80)?;
        // Set filt_3dB, V6MHz
        self.write_reg_mask(usb, 0x06, filt_gain, 0x30)?;
        // channel filter extension
        self.write_reg_mask(usb, 0x1e, ext_enable, 0x60)?;
        // Loop through
        self.write_reg_mask(usb, 0x05, loop_through, 0x80)?;
        // Loop through attenuation
        self.write_reg_mask(usb, 0x1f, lt_att, 0x80)?;
        // filter extension widest
        self.write_reg_mask(usb, 0x0f, flt_ext_widest, 0x80)?;
        // RF poly filter current
        self.write_reg_mask(usb, 0x19, polyfil_cur, 0x60)
    }

    /// r82xx_set_gain. `gain` in tenths of a dB, ignored when !manual.
    fn set_gain(&mut self, usb: &Usb, manual: bool, gain: i32) -> io::Result<()> {
        if manual {
            // LNA auto off
            self.write_reg_mask(usb, 0x05, 0x10, 0x10)?;
            // Mixer auto off
            self.write_reg_mask(usb, 0x07, 0, 0x10)?;
            // Upstream reads 4 bytes here and discards them.
            self.read(usb, 0x00, 4)?;
            // set fixed VGA gain for now (16.3 dB)
            self.write_reg_mask(usb, 0x0c, 0x08, 0x9f)?;

            let mut total_gain = 0;
            let mut lna_index = 0usize;
            let mut mix_index = 0usize;
            for _ in 0..15 {
                if total_gain >= gain {
                    break;
                }
                lna_index += 1;
                total_gain += R82XX_LNA_GAIN_STEPS[lna_index];
                if total_gain >= gain {
                    break;
                }
                mix_index += 1;
                total_gain += R82XX_MIXER_GAIN_STEPS[mix_index];
            }

            // set LNA gain
            self.write_reg_mask(usb, 0x05, lna_index as u8, 0x0f)?;
            // set Mixer gain
            self.write_reg_mask(usb, 0x07, mix_index as u8, 0x0f)
        } else {
            // LNA
            self.write_reg_mask(usb, 0x05, 0, 0x10)?;
            // Mixer
            self.write_reg_mask(usb, 0x07, 0x10, 0x10)?;
            // set fixed VGA gain for now (26.5 dB)
            self.write_reg_mask(usb, 0x0c, 0x0b, 0x9f)
        }
    }

    /// r82xx_set_bandwidth -- returns the resulting IF frequency.
    fn set_bandwidth(&mut self, usb: &Usb, bw: i32) -> io::Result<u32> {
        let mut bw = bw;
        let reg_0a: u8;
        let mut reg_0b: u8;
        let mut real_bw = 0;

        if bw > 7_000_000 {
            // BW: 8 MHz
            reg_0a = 0x10;
            reg_0b = 0x0b;
            self.int_freq = 4_570_000;
        } else if bw > 6_000_000 {
            // BW: 7 MHz
            reg_0a = 0x10;
            reg_0b = 0x2a;
            self.int_freq = 4_570_000;
        } else if bw > R82XX_IF_LOW_PASS_BW_TABLE[0] + FILT_HP_BW1 + FILT_HP_BW2 {
            // BW: 6 MHz
            reg_0a = 0x10;
            reg_0b = 0x6b;
            self.int_freq = 3_570_000;
        } else {
            reg_0a = 0x00;
            reg_0b = 0x80;
            let mut int_freq: i32 = 2_300_000;

            if bw > R82XX_IF_LOW_PASS_BW_TABLE[0] + FILT_HP_BW1 {
                bw -= FILT_HP_BW2;
                int_freq += FILT_HP_BW2;
                real_bw += FILT_HP_BW2;
            } else {
                reg_0b |= 0x20;
            }

            if bw > R82XX_IF_LOW_PASS_BW_TABLE[0] {
                bw -= FILT_HP_BW1;
                int_freq += FILT_HP_BW1;
                real_bw += FILT_HP_BW1;
            } else {
                reg_0b |= 0x40;
            }

            // find low-pass filter
            let mut i = 0;
            while i < R82XX_IF_LOW_PASS_BW_TABLE.len() {
                if bw > R82XX_IF_LOW_PASS_BW_TABLE[i] {
                    break;
                }
                i += 1;
            }
            // Upstream decrements unconditionally; i == 0 only for
            // bw > 1.7MHz, which the branches above already exclude.
            let i = i.saturating_sub(1);
            reg_0b |= 15 - i as u8;
            real_bw += R82XX_IF_LOW_PASS_BW_TABLE[i];

            int_freq -= real_bw / 2;
            self.int_freq = int_freq as u32;
        }

        self.write_reg_mask(usb, 0x0a, reg_0a, 0x10)?;
        self.write_reg_mask(usb, 0x0b, reg_0b, 0xef)?;
        Ok(self.int_freq)
    }

    /// r82xx_set_freq. Like upstream, a PLL that fails to lock is
    /// logged (in set_pll) but not reported as an error.
    fn set_freq(&mut self, usb: &Usb, freq: u32) -> io::Result<()> {
        const MHZ_28_8: u32 = 28_800_000;
        // RTL-SDR Blog V4: automatically upconvert by 28.8 MHz on HF.
        let upconvert_freq = if self.is_blog_v4 && freq < MHZ_28_8 { freq + MHZ_28_8 } else { freq };
        let lo_freq = upconvert_freq + self.int_freq;

        self.set_mux(usb, lo_freq)?;
        self.set_pll(usb, lo_freq)?;
        if !self.has_lock {
            return Ok(());
        }

        if self.is_blog_v4 {
            // notches OFF when tuned within the notch band, ON outside it
            let open_d = if freq <= 2_200_000
                || (85_000_000..=112_000_000).contains(&freq)
                || (172_000_000..=242_000_000).contains(&freq)
            {
                0x00
            } else {
                0x08
            };
            self.write_reg_mask(usb, 0x17, open_d, 0x08)?;

            let band = if freq <= MHZ_28_8 {
                HF
            } else if freq < 250_000_000 {
                VHF
            } else {
                UHF
            };
            if band != self.input {
                self.input = band;
                // activate cable 2 (HF input)
                let cable_2_in = if band == HF { 0x08 } else { 0x00 };
                self.write_reg_mask(usb, 0x06, cable_2_in, 0x08)?;
                // Control upconverter GPIO switch on newer batches
                usb.set_gpio_output(5)?;
                usb.set_gpio_bit(5, cable_2_in == 0)?;
                // activate cable 1 (VHF input)
                let cable_1_in = if band == VHF { 0x40 } else { 0x00 };
                self.write_reg_mask(usb, 0x05, cable_1_in, 0x40)?;
                // activate air_in (UHF input)
                let air_in = if band == UHF { 0x00 } else { 0x20 };
                self.write_reg_mask(usb, 0x05, air_in, 0x20)?;
            }
        } else if self.chip == TunerChip::R828D {
            // switch between 'Cable1' and 'Air-In' at 345 MHz
            let air_cable1_in = if freq > 345_000_000 { 0x00 } else { 0x60 };
            if air_cable1_in != self.input {
                self.input = air_cable1_in;
                self.write_reg_mask(usb, 0x05, air_cable1_in, 0x60)?;
            }
        }
        Ok(())
    }

    /// r82xx_standby
    fn standby(&mut self, usb: &Usb) -> io::Result<()> {
        // If device was not initialized yet, don't need to standby
        if !self.init_done {
            return Ok(());
        }
        self.write_reg(usb, 0x06, 0xb1)?;
        self.write_reg(usb, 0x05, 0xa0)?;
        self.write_reg(usb, 0x07, 0x3a)?;
        self.write_reg(usb, 0x08, 0x40)?;
        self.write_reg(usb, 0x09, 0xc0)?;
        self.write_reg(usb, 0x0a, 0x36)?;
        self.write_reg(usb, 0x0c, 0x35)?;
        self.write_reg(usb, 0x0f, 0x68)?;
        self.write_reg(usb, 0x11, 0x03)?;
        self.write_reg(usb, 0x17, 0xf4)?;
        self.write_reg(usb, 0x19, 0x0c)
    }

    /// r82xx_init
    fn init(&mut self, usb: &Usb) -> io::Result<()> {
        // Initialize registers (upstream ignores this write's result,
        // and set_tv_standard rewrites the shadow straight after).
        self.regs = [0; NUM_REGS];
        let _ = self.write(usb, 0x05, &R82XX_INIT_ARRAY);

        self.set_tv_standard(usb)?;
        self.sysfreq_sel(usb)?;
        self.init_done = true;
        Ok(())
    }
}

// ---------------------------------------------------------------------
// Device (librtlsdr.c: rtlsdr_open and the rtlsdr_set_* API)
// ---------------------------------------------------------------------

/// Control-transfer side of an open dongle -- owned exclusively by
/// radio.rs's rtlsdr_control_loop, on its own thread separate from the
/// bulk stream (`RxEndpoint`), same split as rx888.rs's
/// Rx888Device/RxEndpoint.
pub struct RtlSdrDevice {
    usb: Usb,
    tuner: R82xx,
    rtl_xtal: u32,
    rate: u32,
    freq: u32,
    direct_sampling: u8,
}

pub struct RxEndpoint {
    reader: nusb::io::EndpointRead<Bulk>,
}

impl RxEndpoint {
    pub fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.reader.read(buf)
    }
}

impl RtlSdrDevice {
    /// rtlsdr_set_if_freq. `corr` is always 0 here, so the "corrected"
    /// crystal value librtlsdr reads is just rtl_xtal.
    fn set_if_freq(&self, freq: u32) -> io::Result<()> {
        let if_freq = -((freq as f64 * (1u64 << 22) as f64 / self.rtl_xtal as f64) as i32);
        self.usb.demod_write_reg(1, 0x19, ((if_freq >> 16) & 0x3f) as u16, 1)?;
        self.usb.demod_write_reg(1, 0x1a, ((if_freq >> 8) & 0xff) as u16, 1)?;
        self.usb.demod_write_reg(1, 0x1b, (if_freq & 0xff) as u16, 1)
    }

    /// rtlsdr_set_sample_freq_correction(dev, 0)
    fn set_sample_freq_correction(&self) -> io::Result<()> {
        let offs: i16 = 0;
        self.usb.demod_write_reg(1, 0x3f, (offs & 0xff) as u16, 1)?;
        self.usb.demod_write_reg(1, 0x3e, ((offs >> 8) & 0x3f) as u16, 1)
    }

    fn with_repeater<T>(&mut self, f: impl FnOnce(&mut R82xx, &Usb) -> io::Result<T>) -> io::Result<T> {
        self.usb.set_i2c_repeater(true)?;
        let r = f(&mut self.tuner, &self.usb);
        let off = self.usb.set_i2c_repeater(false);
        let v = r?;
        off?;
        Ok(v)
    }

    /// rtlsdr_set_center_freq
    pub fn set_center_freq(&mut self, freq: u32) -> io::Result<()> {
        let r = if self.direct_sampling != 0 {
            self.set_if_freq(freq)
        } else {
            self.with_repeater(|t, usb| t.set_freq(usb, freq))
        };
        self.freq = if r.is_ok() { freq } else { 0 };
        r
    }

    /// r820t_set_bw. Upstream re-applies the centre frequency
    /// unconditionally, even before one was ever set (freq 0), which
    /// just writes a PLL setting that can't lock; skipped here in that
    /// case -- the real frequency is set straight afterwards anyway.
    fn set_bw(&mut self, bw: u32) -> io::Result<()> {
        let if_freq = self.with_repeater(|t, usb| t.set_bandwidth(usb, bw as i32))?;
        self.set_if_freq(if_freq)?;
        if self.freq != 0 {
            self.set_center_freq(self.freq)?;
        }
        Ok(())
    }

    /// rtlsdr_set_sample_rate
    fn set_sample_rate(&mut self, samp_rate: u32) -> io::Result<()> {
        if samp_rate <= 225_000 || samp_rate > 3_200_000 || (samp_rate > 300_000 && samp_rate <= 900_000) {
            return Err(io_err(format!("RTL-SDR: invalid sample rate {samp_rate} Hz")));
        }
        let two_pow_22 = (1u64 << 22) as f64;
        let mut rsamp_ratio = (self.rtl_xtal as f64 * two_pow_22 / samp_rate as f64) as u32;
        rsamp_ratio &= 0x0fff_fffc;
        let real_rsamp_ratio = rsamp_ratio | ((rsamp_ratio & 0x0800_0000) << 1);
        let real_rate = self.rtl_xtal as f64 * two_pow_22 / real_rsamp_ratio as f64;
        if samp_rate as f64 != real_rate {
            eprintln!("RTL-SDR: exact sample rate is {real_rate} Hz");
        }
        self.rate = real_rate as u32;

        self.set_bw(self.rate)?;

        self.usb.demod_write_reg(1, 0x9f, (rsamp_ratio >> 16) as u16, 2)?;
        self.usb.demod_write_reg(1, 0xa1, (rsamp_ratio & 0xffff) as u16, 2)?;
        self.set_sample_freq_correction()?;
        // reset demod (bit 3, soft_rst)
        self.usb.demod_write_reg(1, 0x01, 0x14, 1)?;
        self.usb.demod_write_reg(1, 0x01, 0x10, 1)
    }

    /// rtlsdr_set_tuner_gain_mode + rtlsdr_set_tuner_gain. librtlsdr's
    /// own set_gain_mode(manual) leaves LNA/mixer at index 0 until a
    /// gain is set, so the two are always applied together here.
    pub fn set_gain(&mut self, agc: bool, gain_tenth_db: i32) -> io::Result<()> {
        self.with_repeater(|t, usb| if agc { t.set_gain(usb, false, 0) } else { t.set_gain(usb, true, gain_tenth_db) })
    }

    /// rtlsdr_set_direct_sampling. 0 = off, 1 = I, 2 = Q.
    pub fn set_direct_sampling(&mut self, on: u8) -> io::Result<()> {
        if on != 0 {
            self.with_repeater(|t, usb| t.standby(usb))?;
            // disable Zero-IF mode
            self.usb.demod_write_reg(1, 0xb1, 0x1a, 1)?;
            // disable spectrum inversion
            self.usb.demod_write_reg(1, 0x15, 0x00, 1)?;
            // only enable In-phase ADC input
            self.usb.demod_write_reg(0, 0x08, 0x4d, 1)?;
            // swap I and Q ADC, this allows to select between two inputs
            self.usb.demod_write_reg(0, 0x06, if on > 1 { 0x90 } else { 0x80 }, 1)?;
            self.direct_sampling = on;
        } else {
            self.with_repeater(|t, usb| t.init(usb))?;
            self.set_if_freq(R82XX_IF_FREQ)?;
            // enable spectrum inversion
            self.usb.demod_write_reg(1, 0x15, 0x01, 1)?;
            // opt_adc_iq = 0, default ADC_I/ADC_Q datapath
            self.usb.demod_write_reg(0, 0x06, 0x80, 1)?;
            self.direct_sampling = 0;
        }
        // Upstream re-applies dev->freq even when it's still 0; see set_bw.
        if self.freq != 0 {
            self.set_center_freq(self.freq)?;
        }
        Ok(())
    }

    pub fn direct_sampling(&self) -> u8 {
        self.direct_sampling
    }

    /// rtlsdr_reset_buffer
    fn reset_buffer(&self) -> io::Result<()> {
        self.usb.write_reg(USBB, USB_EPA_CTL, 0x1002, 2)?;
        self.usb.write_reg(USBB, USB_EPA_CTL, 0x0000, 2)
    }

    /// rtlsdr_deinit_baseband -- tuner to standby, demod/ADCs off.
    pub fn close(&mut self) {
        let _ = self.with_repeater(|t, usb| t.standby(usb));
        let _ = self.usb.write_reg(SYSB, DEMOD_CTL, 0x20, 1);
    }
}

/// Opens the dongle in the USB port `mac` names (see usb_port::port_mac).
fn find_and_open(mac: [u8; 6]) -> io::Result<(nusb::Device, Interface, String, bool)> {
    let info = nusb::list_devices()
        .wait()
        .map_err(io_err)?
        .find(|d| is_known(d) && usb_port::port_mac(d, MAC_TAG) == mac)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("no RTL-SDR dongle found in USB port {}", usb_port::port_label(mac)),
            )
        })?;
    // rtlsdr_check_dongle_model(dev, "RTLSDRBlog", "Blog V4") -- exact
    // string match, as upstream. Windows doesn't cache these strings
    // (see nusb's DeviceInfo docs), so a Blog V4 there is treated as a
    // plain R828D -- the only cost is no automatic HF upconversion.
    let is_blog_v4 = info.manufacturer_string() == Some("RTLSDRBlog") && info.product_string() == Some("Blog V4");
    let name = info.product_string().unwrap_or("RTL-SDR").to_string();
    let device = info.open().wait().map_err(io_err)?;
    // Linux's dvb_usb_rtl28xxu DVB-T driver binds these sticks by
    // default; detach it (librtlsdr's DETACH_KERNEL_DRIVER build option)
    // rather than making the user blacklist the module.
    let interface = device.detach_and_claim_interface(0).wait().map_err(|e| {
        io_err(format!(
            "couldn't claim the RTL-SDR's USB interface ({e}) -- {}",
            if cfg!(windows) {
                "on Windows, bind the WinUSB driver to it with Zadig (see the manual), and check no \
                 other SDR program is using it"
            } else if cfg!(target_os = "linux") {
                "check the udev rules (rtl-sdr.rules) give your user access, and check no other SDR \
                 program is using it"
            } else {
                "check no other SDR program is using it"
            }
        ))
    })?;
    Ok((device, interface, name, is_blog_v4))
}

/// rtlsdr_open, followed by the standard bring-up a client like rtl_sdr
/// does (sample rate, gain, centre frequency, reset buffer), for the
/// dongle `mac` (a discover() port MAC) names. Returns the control
/// half, the streaming half, and the dongle's product name.
pub fn initialise(
    mac: [u8; 6],
    initial_freq_hz: u32,
    gain_db: u32,
    tuner_agc: bool,
    direct_sampling: u8,
) -> io::Result<(RtlSdrDevice, RxEndpoint, String)> {
    let (device, mut interface, name, is_blog_v4) = find_and_open(mac)?;
    let mut usb = Usb { interface: interface.clone() };

    // perform a dummy write, if it fails, reset the device
    if usb.write_reg(USBB, USB_SYSCTL, 0x09, 1).is_err() {
        eprintln!("RTL-SDR: resetting device...");
        drop(usb);
        drop(interface);
        let _ = device.reset().wait();
        drop(device);
        std::thread::sleep(Duration::from_millis(500));
        let (_, i, _, _) = find_and_open(mac)?;
        interface = i;
        usb = Usb { interface: interface.clone() };
    }

    usb.init_baseband()?;

    // Probe tuners -- R820T then R828D only (see module doc comment).
    usb.set_i2c_repeater(true)?;
    let chip = if usb.i2c_read_reg(R820T_I2C_ADDR, R82XX_CHECK_ADDR) == R82XX_CHECK_VAL {
        TunerChip::R820T
    } else if usb.i2c_read_reg(R828D_I2C_ADDR, R82XX_CHECK_ADDR) == R82XX_CHECK_VAL {
        TunerChip::R828D
    } else {
        let _ = usb.set_i2c_repeater(false);
        return Err(io_err(
            "RTL-SDR: no R820T/R820T2/R828D tuner found -- older E4000/FC0012/FC0013/FC2580 \
             dongles aren't supported",
        ));
    };
    eprintln!("RTL-SDR: found {chip:?} tuner{}", if is_blog_v4 { " (RTL-SDR Blog V4)" } else { "" });

    let rtl_xtal = DEF_RTL_XTAL_FREQ;
    // use the rtl clock value by default; a non-Blog-V4 R828D runs its
    // tuner from its own 16MHz crystal.
    let tun_xtal = if chip == TunerChip::R828D && !is_blog_v4 { R828D_XTAL_FREQ } else { rtl_xtal };

    // disable Zero-IF mode
    usb.demod_write_reg(1, 0xb1, 0x1a, 1)?;
    // only enable In-phase ADC input
    usb.demod_write_reg(0, 0x08, 0x4d, 1)?;

    let mut dev = RtlSdrDevice {
        usb,
        tuner: R82xx {
            chip,
            i2c_addr: if chip == TunerChip::R828D { R828D_I2C_ADDR } else { R820T_I2C_ADDR },
            xtal: tun_xtal,
            regs: [0; NUM_REGS],
            int_freq: 0,
            fil_cal_code: 0,
            input: 0,
            has_lock: false,
            init_done: false,
            is_blog_v4,
        },
        rtl_xtal,
        rate: 0,
        freq: 0,
        direct_sampling: 0,
    };

    // the R82XX use 3.57 MHz IF for the DVB-T 6 MHz mode
    dev.set_if_freq(R82XX_IF_FREQ)?;
    // enable spectrum inversion
    dev.usb.demod_write_reg(1, 0x15, 0x01, 1)?;

    dev.tuner.init(&dev.usb)?;
    dev.usb.set_i2c_repeater(false)?;

    // --- end of rtlsdr_open; client-side bring-up from here ---
    dev.set_sample_rate(DONGLE_RATE_HZ)?;
    let ds = if initial_freq_hz < DIRECT_SAMPLING_BELOW_HZ { direct_sampling } else { 0 };
    if ds != 0 {
        dev.set_direct_sampling(ds)?;
    }
    // A frequency the tuner can't reach (e.g. HF with direct sampling
    // off) isn't fatal -- the user can retune -- so just log it.
    if let Err(e) = dev.set_center_freq(initial_freq_hz) {
        eprintln!("RTL-SDR: couldn't tune to {initial_freq_hz} Hz: {e}");
    }
    if ds == 0 {
        dev.set_gain(tuner_agc, gain_db as i32 * 10)?;
    }
    dev.reset_buffer()?;

    // Clear any halt / data-toggle state the previous session left on the
    // stream endpoint. Linux resets the device on open, which does this;
    // WinUSB can't reset a device, so on Windows a restart (e.g. a sample
    // rate change) inherited a stalled endpoint ("stream read failed:
    // endpoint stalled"). CLEAR_FEATURE(ENDPOINT_HALT) is harmless when
    // nothing is stalled.
    let mut endpoint = interface.endpoint::<Bulk, In>(EP_STREAM_IN).map_err(io_err)?;
    if let Err(e) = endpoint.clear_halt().wait() {
        eprintln!("RTL-SDR: clearing the stream endpoint failed ({e}), continuing");
    }
    let reader = endpoint
        .reader(STREAM_READ_SIZE)
        .with_num_transfers(STREAM_NUM_TRANSFERS)
        .with_read_timeout(Duration::from_millis(500));

    Ok((dev, RxEndpoint { reader }, name))
}

// ---------------------------------------------------------------------
// Decimation (not from librtlsdr -- this project's own DSP)
// ---------------------------------------------------------------------

const CIC_STAGES: usize = 5;

/// Same reasoning and value as rx888.rs's HEADROOM_FACTOR: map the
/// ADC's own full scale well below IqSample's (2^23) so a strong signal
/// doesn't saturate something inside WDSP. Not yet calibrated against
/// real RTL-SDR hardware -- revisit if audio clips on strong signals or
/// the S-meter reads far off another receiver on the same signal.
const HEADROOM_FACTOR: f64 = 0.2;

#[derive(Clone, Copy, Default)]
struct CicStage {
    integrator: i64,
    comb_prev: i64,
}

/// How far above the dial the dongle is actually tuned, at decimations
/// of 2 or more -- see `Decimator`'s doc comment. 300kHz puts the
/// dongle's own centre spike outside the displayed span at every rate up
/// to 384k (half-span 192k), while keeping the wanted span inside the
/// R82xx's ~1.55MHz IF filter (and, at 768k, inside the dongle's own
/// +-768kHz Nyquist band: -84k..+684k).
pub const TUNING_OFFSET_HZ: u32 = 300_000;

/// 0 at decimation 1: a 1536k output needs the dongle's whole band, so
/// there's no room to move the spike out of it (the DC blocker still
/// removes it).
pub fn tuning_offset_hz(decimation: u32) -> u32 {
    if decimation >= 2 {
        TUNING_OFFSET_HZ
    } else {
        0
    }
}

/// Fixed-point scale applied to the (floating-point) DC-blocked, mixed
/// samples before the integer CIC, so it keeps fractional precision.
/// Worst case CIC growth is then 32^5 * 255*sqrt(2) * 256 ~= 3.1e12,
/// still nowhere near i64's range.
const CIC_INPUT_SCALE: f64 = 256.0;

/// DC blocker pole: y[n] = x[n] - x[n-1] + a*y[n-1]. 1 - 2^-12 gives a
/// ~60Hz corner at DONGLE_RATE_HZ and settles in a few milliseconds.
const DC_BLOCK_POLE: f64 = 1.0 - 1.0 / 4096.0;

/// See rx888.rs's RENORM_INTERVAL -- same rotating-oscillator technique.
const RENORM_INTERVAL: u32 = 4096;

/// u8 I/Q at DONGLE_RATE_HZ -> IqSample-scaled (i32, i32) at
/// DONGLE_RATE_HZ / decimation. Per sample:
///
/// 1. `2*v - 255` (odd integers -255..=255), so the unsigned 8-bit
///    midpoint 127.5 maps exactly to zero.
/// 2. DC blocker on each rail -- removes the dongle's own centre spike
///    (ADC DC offset / LO leakage at the hardware's tuned frequency).
///    REAL REPORT on a NESDR Smart v5 (2026-10-14): a peak at the centre
///    frequency on HF (direct sampling) and on 2m/70cm (tuner). The
///    module's original assumption that the R82xx's low IF would keep
///    the centre clean turned out wrong.
/// 3. Offset-tuning shift: radio.rs's rtlsdr_control_loop tunes the
///    dongle `tuning_offset_hz` ABOVE the dial, so the wanted signal
///    arrives at -offset and the spike's residue (and any I/Q-imbalance
///    image) sits well away from it; an NCO here shifts -offset back to
///    zero. Not relying on the DC blocker alone matters: on its own it
///    would also notch out anything sitting exactly on the dial -- an AM
///    carrier, say.
/// 4. N-stage CIC per rail decimating by `decimation / 2` (same
///    wrapping i64 arithmetic as rx888::Ddc, see its CicStage doc
///    comment for why wrapping integers rather than floats) -- skipped
///    entirely at decimation 2.
/// 5. A final decimate-by-2 FIR (see `design_final_fir`) that both
///    cancels the CIC's droop exactly and does the alias rejection the
///    CIC is poor at. REAL REPORT (2026-10-14): the previous design (the
///    whole decimation in the CIC, rx888's large-R compensator only from
///    R=4) left a clearly curved spectrum at 768k -- a 5-stage CIC
///    decimating by 2 is ~15dB down at the band edge.
///
/// R=1 has no CIC, FIR or offset at all.
///
/// Emits (i, -q): this codebase's I/Q convention is the conjugate of
/// the standard one the RTL2832U produces (see rx888::Ddc::process_block's
/// mixer comment -- real P1/P2 hardware delivers a signal above the dial
/// at negative baseband frequency). CONFIRMED on a real NESDR Smart v5
/// (2026-10-14): spectrum direction correct, and with the offset shift
/// above, the centre spike gone and tuning accurate.
pub struct Decimator {
    decimation: u32,
    /// CIC decimation: decimation / 2 (the final FIR does the last 2x).
    cic_decimation: u32,
    count: u32,
    stages_i: [CicStage; CIC_STAGES],
    stages_q: [CicStage; CIC_STAGES],
    gain_comp: f64,
    fir_taps: Vec<f64>,
    /// FIR history, each sample stored twice (at `fir_pos` and
    /// `fir_pos + FIR_LEN`) so the latest FIR_LEN are always one
    /// contiguous slice.
    fir_hist_i: Vec<f64>,
    fir_hist_q: Vec<f64>,
    fir_pos: usize,
    /// Whether the next FIR input produces an output (every other one).
    fir_emit: bool,
    /// DC blocker state: previous input and output, per rail.
    dc_prev_in: (f64, f64),
    dc_prev_out: (f64, f64),
    /// Offset-shift oscillator (unit vector) and its fixed per-sample
    /// rotation.
    osc: (f64, f64),
    rot: (f64, f64),
    renorm_counter: u32,
    /// Odd trailing byte from the previous block, so I/Q pairing
    /// survives a read that ends mid-pair.
    carry: Option<u8>,
}

impl Decimator {
    pub fn new(decimation: u32) -> Self {
        Self::with_full_scale(decimation, 255.0)
    }

    /// For a source other than this dongle's centred 8-bit samples
    /// (mirisdr.rs's 14-bit ones): `full_scale` is the input magnitude
    /// that should map to IqSample's own full scale, before
    /// HEADROOM_FACTOR.
    pub fn with_full_scale(decimation: u32, full_scale: f64) -> Self {
        let decimation = decimation.max(1);
        let cic_decimation = (decimation / 2).max(1);
        let cic_gain = (cic_decimation as f64).powi(CIC_STAGES as i32);
        let phase_inc =
            2.0 * std::f64::consts::PI * tuning_offset_hz(decimation) as f64 / DONGLE_RATE_HZ as f64;
        Self {
            decimation,
            cic_decimation,
            count: 0,
            stages_i: [CicStage::default(); CIC_STAGES],
            stages_q: [CicStage::default(); CIC_STAGES],
            gain_comp: (8_388_608.0 / full_scale) * HEADROOM_FACTOR / (cic_gain * CIC_INPUT_SCALE),
            fir_taps: if decimation > 1 { design_final_fir(cic_decimation) } else { Vec::new() },
            fir_hist_i: vec![0.0; 2 * FIR_LEN],
            fir_hist_q: vec![0.0; 2 * FIR_LEN],
            fir_pos: 0,
            fir_emit: false,
            dc_prev_in: (0.0, 0.0),
            dc_prev_out: (0.0, 0.0),
            osc: (1.0, 0.0),
            rot: (phase_inc.cos(), phase_inc.sin()),
            renorm_counter: 0,
            carry: None,
        }
    }

    pub fn process_block(&mut self, bytes: &[u8], mut emit: impl FnMut((i32, i32))) {
        let mut bytes = bytes;
        if let Some(i_byte) = self.carry.take() {
            match bytes.split_first() {
                Some((&q_byte, rest)) => {
                    self.process_pairs(&[i_byte, q_byte], &mut emit);
                    bytes = rest;
                }
                None => {
                    self.carry = Some(i_byte);
                    return;
                }
            }
        }
        let even = bytes.len() & !1;
        self.process_pairs(&bytes[..even], &mut emit);
        if even < bytes.len() {
            self.carry = Some(bytes[even]);
        }
    }

    fn process_pairs(&mut self, bytes: &[u8], emit: &mut impl FnMut((i32, i32))) {
        // 1: centre.
        let samples = bytes.chunks_exact(2).map(|p| ((2 * p[0] as i32 - 255) as f64, (2 * p[1] as i32 - 255) as f64));
        self.process_samples(samples, emit);
    }

    /// Steps 2-5 of the doc comment above, on already-centred samples
    /// (scaled so `full_scale` -- see `with_full_scale` -- is full scale).
    pub fn process_samples(&mut self, samples: impl Iterator<Item = (f64, f64)>, emit: &mut impl FnMut((i32, i32))) {
        let decimation = self.decimation;
        let cic_decimation = self.cic_decimation;
        let gain_comp = self.gain_comp;
        let (mut prev_in_i, mut prev_in_q) = self.dc_prev_in;
        let (mut prev_out_i, mut prev_out_q) = self.dc_prev_out;
        let (mut osc_cos, mut osc_sin) = self.osc;
        let (rot_cos, rot_sin) = self.rot;
        let mut renorm_counter = self.renorm_counter;
        let mut count = self.count;
        let mut stages_i = self.stages_i;
        let mut stages_q = self.stages_q;

        for (x_i, x_q) in samples {
            // 2: DC-block.
            let y_i = x_i - prev_in_i + DC_BLOCK_POLE * prev_out_i;
            let y_q = x_q - prev_in_q + DC_BLOCK_POLE * prev_out_q;
            prev_in_i = x_i;
            prev_in_q = x_q;
            prev_out_i = y_i;
            prev_out_q = y_q;

            if decimation == 1 {
                emit(((y_i * gain_comp) as i32, -((y_q * gain_comp) as i32)));
                continue;
            }

            // 3: (y_i + j*y_q) * e^{+j*phase} -- moves -offset up to 0.
            let m_i = y_i * osc_cos - y_q * osc_sin;
            let m_q = y_i * osc_sin + y_q * osc_cos;
            let new_cos = osc_cos * rot_cos - osc_sin * rot_sin;
            let new_sin = osc_cos * rot_sin + osc_sin * rot_cos;
            osc_cos = new_cos;
            osc_sin = new_sin;
            renorm_counter += 1;
            if renorm_counter >= RENORM_INTERVAL {
                renorm_counter = 0;
                let mag = (osc_cos * osc_cos + osc_sin * osc_sin).sqrt();
                osc_cos /= mag;
                osc_sin /= mag;
            }

            // 4: CIC.
            let (c_i, c_q) = if cic_decimation == 1 {
                (m_i * CIC_INPUT_SCALE, m_q * CIC_INPUT_SCALE)
            } else {
                let mut i_val = (m_i * CIC_INPUT_SCALE) as i64;
                let mut q_val = (m_q * CIC_INPUT_SCALE) as i64;
                for stage in &mut stages_i {
                    stage.integrator = stage.integrator.wrapping_add(i_val);
                    i_val = stage.integrator;
                }
                for stage in &mut stages_q {
                    stage.integrator = stage.integrator.wrapping_add(q_val);
                    q_val = stage.integrator;
                }
                count += 1;
                if count < cic_decimation {
                    continue;
                }
                count = 0;
                for stage in &mut stages_i {
                    let diff = i_val.wrapping_sub(stage.comb_prev);
                    stage.comb_prev = i_val;
                    i_val = diff;
                }
                for stage in &mut stages_q {
                    let diff = q_val.wrapping_sub(stage.comb_prev);
                    stage.comb_prev = q_val;
                    q_val = diff;
                }
                (i_val as f64, q_val as f64)
            };

            // 5: final FIR, decimating by 2.
            if let Some((out_i, out_q)) = self.fir_push(c_i * gain_comp, c_q * gain_comp) {
                emit((out_i as i32, -(out_q as i32)));
            }
        }

        self.dc_prev_in = (prev_in_i, prev_in_q);
        self.dc_prev_out = (prev_out_i, prev_out_q);
        self.osc = (osc_cos, osc_sin);
        self.renorm_counter = renorm_counter;
        self.count = count;
        self.stages_i = stages_i;
        self.stages_q = stages_q;
    }

    /// Step 5: pushes one sample into the final FIR; every other call
    /// returns a filtered output (polyphase -- the discarded outputs are
    /// never computed).
    fn fir_push(&mut self, i: f64, q: f64) -> Option<(f64, f64)> {
        let pos = self.fir_pos;
        self.fir_hist_i[pos] = i;
        self.fir_hist_i[pos + FIR_LEN] = i;
        self.fir_hist_q[pos] = q;
        self.fir_hist_q[pos + FIR_LEN] = q;
        self.fir_pos = (pos + 1) % FIR_LEN;
        self.fir_emit = !self.fir_emit;
        if !self.fir_emit {
            return None;
        }
        // The taps are symmetric, so the window's order doesn't matter.
        let win_i = &self.fir_hist_i[pos + 1..pos + 1 + FIR_LEN];
        let win_q = &self.fir_hist_q[pos + 1..pos + 1 + FIR_LEN];
        let mut acc_i = 0.0;
        let mut acc_q = 0.0;
        for ((&t, &xi), &xq) in self.fir_taps.iter().zip(win_i).zip(win_q) {
            acc_i += t * xi;
            acc_q += t * xq;
        }
        Some((acc_i, acc_q))
    }
}

/// Final FIR: 2*FIR_HALF_LEN+1 taps, linear phase. Prototyped in numpy
/// against the exact CIC response: 81 taps gives <0.04dB passband ripple
/// and >66dB alias rejection at every decimation -- comfortably past an
/// 8-bit RTL2832U and fine for the MSi2500's 14 bits.
const FIR_HALF_LEN: usize = 40;
const FIR_LEN: usize = 2 * FIR_HALF_LEN + 1;
/// Band edges as fractions of the FIR's INPUT rate. After decimating by
/// 2 these are 0.45/0.55 of the output rate: flat (and alias-free) over
/// the middle 90% of the displayed span; the outer 5% each side rolls
/// off and may carry the other edge's alias, as on any real decimator.
const FIR_PASSBAND_EDGE: f64 = 0.225;
const FIR_STOPBAND_EDGE: f64 = 0.275;
/// Least-squares weight of the stopband error relative to the passband's.
const FIR_STOPBAND_WEIGHT: f64 = 30.0;
/// Frequency grid points (0..0.5) the least-squares fit is taken over.
const FIR_DESIGN_GRID: usize = 4000;

/// Magnitude of an N-stage, decimate-by-`r` CIC (normalised to unity at
/// DC) at `f` cycles per sample of its OUTPUT rate.
fn cic_response(f: f64, r: u32) -> f64 {
    if r <= 1 {
        return 1.0;
    }
    let x = std::f64::consts::PI * f / r as f64;
    if x.abs() < 1e-12 {
        return 1.0;
    }
    ((r as f64 * x).sin() / (r as f64 * x.sin())).abs().powi(CIC_STAGES as i32)
}

/// Weighted least-squares linear-phase lowpass whose passband is the
/// inverse of the preceding CIC's droop (`cic_decimation`, 1 = no CIC),
/// normalised to exactly unity gain at DC. Solved once per Decimator,
/// so a rate change costs a fraction of a millisecond.
fn design_final_fir(cic_decimation: u32) -> Vec<f64> {
    let m = FIR_HALF_LEN;
    // Normal equations A*a = b for the cosine coefficients a[0..=m] of
    // H(f) = a0 + 2*sum(a_k cos(2*pi*k*f)).
    let mut a = vec![vec![0.0; m + 1]; m + 1];
    let mut b = vec![0.0; m + 1];
    let mut basis = vec![0.0; m + 1];
    for g in 0..FIR_DESIGN_GRID {
        let f = 0.5 * g as f64 / (FIR_DESIGN_GRID - 1) as f64;
        let (weight, desired) = if f <= FIR_PASSBAND_EDGE {
            (1.0, 1.0 / cic_response(f, cic_decimation))
        } else if f >= FIR_STOPBAND_EDGE {
            (FIR_STOPBAND_WEIGHT, 0.0)
        } else {
            continue; // transition band: don't care
        };
        for (k, v) in basis.iter_mut().enumerate() {
            let c = (2.0 * std::f64::consts::PI * k as f64 * f).cos();
            *v = if k == 0 { c } else { 2.0 * c };
        }
        for r in 0..=m {
            b[r] += weight * basis[r] * desired;
            for c in 0..=m {
                a[r][c] += weight * basis[r] * basis[c];
            }
        }
    }
    let coeffs = solve_linear(a, b);
    let mut taps: Vec<f64> = (0..FIR_LEN).map(|n| coeffs[n.abs_diff(m)]).collect();
    let dc: f64 = taps.iter().sum();
    for t in &mut taps {
        *t /= dc;
    }
    taps
}

/// Gaussian elimination with partial pivoting. Only used on
/// design_final_fir's small, symmetric positive-definite system
/// (condition number ~1e5), well within f64.
fn solve_linear(mut a: Vec<Vec<f64>>, mut b: Vec<f64>) -> Vec<f64> {
    let n = b.len();
    for col in 0..n {
        let pivot = (col..n).max_by(|&x, &y| a[x][col].abs().total_cmp(&a[y][col].abs())).unwrap();
        a.swap(col, pivot);
        b.swap(col, pivot);
        let pivot_row = a[col].clone();
        for row in col + 1..n {
            let factor = a[row][col] / pivot_row[col];
            if factor != 0.0 {
                for (dst, &src) in a[row][col..].iter_mut().zip(&pivot_row[col..]) {
                    *dst -= factor * src;
                }
                b[row] -= factor * b[col];
            }
        }
    }
    let mut x = vec![0.0; n];
    for row in (0..n).rev() {
        let s: f64 = (row + 1..n).map(|k| a[row][k] * x[k]).sum();
        x[row] = (b[row] - s) / a[row][row];
    }
    x
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_supported_rate_is_an_exact_decimation() {
        for &rate in &SUPPORTED_OUTPUT_RATES {
            let d = decimation_for_output_rate(rate).unwrap();
            assert_eq!(DONGLE_RATE_HZ / d, rate);
            assert_eq!(DONGLE_RATE_HZ % rate, 0);
        }
        assert_eq!(decimation_for_output_rate(250_000), None);
    }

    #[test]
    fn dongle_rate_needs_no_resampler_rounding() {
        let ratio = (DEF_RTL_XTAL_FREQ as f64 * (1u64 << 22) as f64 / DONGLE_RATE_HZ as f64) as u32;
        assert_eq!(ratio & 0x0fff_fffc, ratio);
    }

    #[test]
    fn every_published_gain_is_reachable_by_the_step_walk() {
        // Every entry in librtlsdr's r82xx_gains[] should be a point
        // r82xx_set_gain's alternating LNA/mixer walk actually lands on
        // (the published list skips one walk point, 48.3dB, so this is
        // a subset check, not equality).
        let mut totals = vec![0];
        let mut total = 0;
        for i in 1..16 {
            total += R82XX_LNA_GAIN_STEPS[i];
            totals.push(total);
            total += R82XX_MIXER_GAIN_STEPS[i];
            totals.push(total);
        }
        for g in R82XX_GAINS_TENTH_DB {
            assert!(totals.contains(&g), "{g} not reachable");
        }
    }

    #[test]
    fn constant_dc_is_removed() {
        // A stuck-off-centre input (the dongle's centre spike, in the
        // limit) should decay to ~nothing once the DC blocker settles.
        let mut d = Decimator::new(8);
        let mut out = Vec::new();
        let block = vec![200u8; 2 * 200_000];
        d.process_block(&block, |s| out.push(s));
        for &(i, q) in out.iter().skip(out.len() / 2) {
            assert!(i.abs() <= 2 && q.abs() <= 2, "({i},{q})");
        }
    }

    #[test]
    fn signal_at_minus_offset_lands_on_the_dial() {
        // The dongle is tuned TUNING_OFFSET_HZ above the dial, so the
        // dial's signal arrives at -offset (standard convention); after
        // the shift it should come out as (near) DC -- a steady,
        // non-rotating output.
        let decimation = 8;
        let off = tuning_offset_hz(decimation) as f64;
        let n = 2 * 200_000;
        let block: Vec<u8> = (0..n / 2)
            .flat_map(|k| {
                let ph = -2.0 * std::f64::consts::PI * off * k as f64 / DONGLE_RATE_HZ as f64;
                [(127.5 + 100.0 * ph.cos()).round() as u8, (127.5 + 100.0 * ph.sin()).round() as u8]
            })
            .collect();
        let mut d = Decimator::new(decimation);
        let mut out = Vec::new();
        d.process_block(&block, |s| out.push(s));
        let tail = &out[out.len() / 2..];
        let (mi, mq) = tail.iter().fold((0.0, 0.0), |a, &(i, q)| (a.0 + i as f64, a.1 + q as f64));
        let (mi, mq) = (mi / tail.len() as f64, mq / tail.len() as f64);
        let mean_mag = (mi * mi + mq * mq).sqrt();
        let rms = (tail.iter().map(|&(i, q)| (i as f64).powi(2) + (q as f64).powi(2)).sum::<f64>()
            / tail.len() as f64)
            .sqrt();
        // Nearly all the power is in the steady (DC) component.
        assert!(mean_mag > 0.95 * rms, "mean {mean_mag} rms {rms}");
    }

    /// Output RMS of a unit complex tone at `f_out_hz` relative to the
    /// dial (standard convention, before the offset shift), through a
    /// float-input Decimator -- no 8-bit quantisation.
    fn tone_rms(decimation: u32, f_out_hz: f64) -> f64 {
        let off = tuning_offset_hz(decimation) as f64;
        let f_in = f_out_hz - off;
        let n = 120_000 * decimation as usize / 8 + 40_000;
        let samples = (0..n).map(|k| {
            let ph = 2.0 * std::f64::consts::PI * f_in * k as f64 / DONGLE_RATE_HZ as f64;
            (1000.0 * ph.cos(), 1000.0 * ph.sin())
        });
        let mut d = Decimator::with_full_scale(decimation, 1000.0);
        let mut out = Vec::new();
        d.process_samples(samples, &mut |s| out.push(s));
        let tail = &out[out.len() / 2..];
        (tail.iter().map(|&(i, q)| (i as f64).powi(2) + (q as f64).powi(2)).sum::<f64>() / tail.len() as f64)
            .sqrt()
    }

    #[test]
    fn final_fir_is_symmetric_with_unity_dc_gain() {
        for r in [1, 2, 4, 8, 16] {
            let taps = design_final_fir(r);
            assert_eq!(taps.len(), FIR_LEN);
            assert!((taps.iter().sum::<f64>() - 1.0).abs() < 1e-12);
            for k in 0..FIR_LEN {
                assert_eq!(taps[k], taps[FIR_LEN - 1 - k]);
            }
        }
    }

    #[test]
    fn passband_is_flat_at_every_decimation() {
        // REAL REPORT: 768k (decimation 2) showed a clearly curved
        // spectrum. Tones across the middle 90% of the output span,
        // either side of the dial, must all come out within 0.1dB.
        for decimation in [2, 4, 8, 32] {
            let out_rate = (DONGLE_RATE_HZ / decimation) as f64;
            let reference = tone_rms(decimation, 0.02 * out_rate);
            for frac in [-0.44, -0.3, -0.15, 0.15, 0.3, 0.44] {
                let db = 20.0 * (tone_rms(decimation, frac * out_rate) / reference).log10();
                assert!(db.abs() < 0.1, "decimation {decimation}, {frac} of output rate: {db:.3}dB");
            }
        }
    }

    #[test]
    fn aliases_are_rejected() {
        // A tone one output-rate away from the passband aliases onto it
        // after decimating; it must come out >60dB down.
        for decimation in [2, 4, 8, 32] {
            let out_rate = (DONGLE_RATE_HZ / decimation) as f64;
            let wanted = tone_rms(decimation, 0.3 * out_rate);
            let alias = tone_rms(decimation, (0.3 - 1.0) * out_rate);
            let db = 20.0 * (alias / wanted).log10();
            assert!(db < -60.0, "decimation {decimation}: alias only {db:.1}dB down");
        }
    }

    #[test]
    fn split_reads_keep_iq_pairing() {
        let block: Vec<u8> = (0..4096u32).map(|n| (n * 37 % 251) as u8).collect();
        let mut whole = Decimator::new(4);
        let mut a = Vec::new();
        whole.process_block(&block, |s| a.push(s));
        let mut split = Decimator::new(4);
        let mut b = Vec::new();
        split.process_block(&block[..1001], |s| b.push(s));
        split.process_block(&block[1001..], |s| b.push(s));
        assert_eq!(a, b);
    }

    #[test]
    fn tone_above_centre_comes_out_at_negative_frequency() {
        // Standard-convention complex tone at +fs/16 into a passthrough
        // decimator; the conjugated output must rotate the other way.
        let n = 64;
        let block: Vec<u8> = (0..n)
            .flat_map(|k| {
                let ph = 2.0 * std::f64::consts::PI * k as f64 / 16.0;
                [(127.5 + 100.0 * ph.cos()).round() as u8, (127.5 + 100.0 * ph.sin()).round() as u8]
            })
            .collect();
        let mut d = Decimator::new(1);
        let mut out = Vec::new();
        d.process_block(&block, |s| out.push(s));
        // Cross product of consecutive samples: sign gives rotation direction.
        let cross: f64 = out
            .windows(2)
            .map(|w| w[0].0 as f64 * w[1].1 as f64 - w[0].1 as f64 * w[1].0 as f64)
            .sum();
        assert!(cross < 0.0);
    }

    #[test]
    fn pll_math_matches_upstream_c() {
        // Expected values from compiling upstream's r82xx_set_pll
        // arithmetic (I/O stripped) with gcc, at LO = dial + 1.525MHz IF
        // (the int_freq set_bandwidth picks for DONGLE_RATE_HZ), 28.8MHz
        // crystal: (freq, mix_div, div_num, nint, reg 0x14, sdm).
        let cases = [
            (147_025_000u32, 16u32, 3u8, 40u8, 198u8, 55_068u32),
            (434_525_000, 8, 2, 60, 203, 22_983),
            (1_297_525_000, 2, 0, 45, 8, 3_470),
            (56_000_000, 32, 4, 31, 132, 7_282),
            (29_599_000, 64, 5, 32, 196, 58_181),
        ];
        for (freq, mix_div, div_num, nint, r4, sdm) in cases {
            assert_eq!(pll_divider((freq + 500) / 1000), (mix_div, div_num), "{freq}");
            assert_eq!(pll_nint_sdm(freq, mix_div, DEF_RTL_XTAL_FREQ), (nint, sdm), "{freq}");
            assert_eq!(pll_ni_si(nint), r4, "{freq}");
        }
    }

    #[test]
    fn fir_packing_matches_upstream_layout() {
        // 12-bit pairs packed big-endian into 3 bytes, as rtlsdr_set_fir.
        let (val0, val1) = (FIR_DEFAULT[8], FIR_DEFAULT[9]); // 101, 156
        assert_eq!((val0 >> 4) as u8, 0x06);
        assert_eq!(((val0 << 4) | ((val1 >> 8) & 0x0f)) as u8, 0x50);
        assert_eq!(val1 as u8, 0x9c);
    }

    #[test]
    fn bitrev_matches_lut() {
        assert_eq!(bitrev(0x01), 0x80);
        assert_eq!(bitrev(0x69), 0x96);
        assert_eq!(bitrev(0xf0), 0x0f);
    }
}
