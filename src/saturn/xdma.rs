/*
    ANAN G2 (Saturn) through its FPGA's PCIe XDMA interface, for running
    hpsdr-rs on the G2's own Raspberry Pi in place of p2app.

    Ported from piHPSDR's Saturn support (~/github/dl1ycf/pihpsdr/src:
    saturnmain.c, saturnregisters.c, saturndrivers.c and their headers),
    copyright 2021 Laurence Barker G8NJJ, with the piHPSDR interfacing by
    Rick Koch N1GP and changes by Christoph van Wullen DL1YCF, licensed
    GPL-3.0-or-later -- which is why hpsdr-rs is GPL-3.0-or-later.

    The same idea as piHPSDR's: nothing above this file changes. The
    Protocol 2 code in radio.rs builds the same packets it sends a network
    radio, and hands them here instead of to a socket (radio::P2Socket);
    here they become FPGA register writes and DMA transfers, and what the
    FPGA delivers -- receive IQ, microphone samples, status -- comes back
    as the Protocol 2 packets a network radio would send, from the port it
    would send them from. So this is p2app's job, done in-process.

    One difference from piHPSDR: it keeps DDC0-5 for its built-in network
    server and gives its own receivers DDC6-9. There's no server here, so
    the packets' DDC0-9 are the FPGA's DDC0-9, as with p2app.

    Linux only: XDMA is the Xilinx PCIe driver, /dev/xdma0_*.

    This program is free software: you can redistribute it and/or modify it
    under the terms of the GNU General Public License as published by the
    Free Software Foundation, either version 3 of the License, or (at your
    option) any later version.
*/

use std::alloc::{alloc_zeroed, dealloc, Layout};
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Devices, registers and constants (saturnregisters.h, saturnmain.c).

pub const XDMA_USER: &str = "/dev/xdma0_user";
const DDC_DMA_DEVICE: &str = "/dev/xdma0_c2h_0";
const MIC_DMA_DEVICE: &str = "/dev/xdma0_c2h_1";
const DUC_DMA_DEVICE: &str = "/dev/xdma0_h2c_0";
const SPK_DMA_DEVICE: &str = "/dev/xdma0_h2c_1";

const NUM_DDC: usize = 10;

const ADDR_DDC_REGS: [u32; NUM_DDC] =
    [0x00000, 0x00004, 0x00008, 0x0000C, 0x00010, 0x00014, 0x00018, 0x0001C, 0x01000, 0x01004];
const ADDR_DDC_RATES: u32 = 0x0100C;
const ADDR_DDC_IN_SEL: u32 = 0x01010;
const ADDR_KEYER_CONFIG: u32 = 0x02000;
const ADDR_SIDETONE_CONFIG: u32 = 0x02004;
const ADDR_TX_CONFIG: u32 = 0x02008;
const ADDR_TX_DUC: u32 = 0x0200C;
const ADDR_RF_GPIO: u32 = 0x02014;
const ADDR_ADC_CTRL: u32 = 0x02018;
const ADDR_DAC_CTRL: u32 = 0x0201C;
const ADDR_STATUS: u32 = 0x04000;
const ADDR_USER_VERSION: u32 = 0x04004;
const ADDR_ADC_OVERFLOW: u32 = 0x05000;
const ADDR_FIFO_RESET: u32 = 0x07000;
const ADDR_IAMBIC_CONFIG: u32 = 0x07004;
const ADDR_FIFO_MON_BASE: u32 = 0x09000;
const ADDR_ALEX_ADC_BASE: u32 = 0x0A000;
const ADDR_ALEX_SPI: u32 = 0x0B000;
const ADDR_SW_VERSION: u32 = 0x0C000;
const ADDR_PROD_VERSION: u32 = 0x0C004;
const ADDR_CODEC_SPI_WRITE: u32 = 0x14000;
const ADDR_CW_KEYER_RAM: u32 = 0x1C000;

const ADDR_DDC_STREAM_READ: u64 = 0x0;
const ADDR_DUC_STREAM_WRITE: u64 = 0x0;
const ADDR_MIC_STREAM_READ: u64 = 0x40000;
const ADDR_SPK_STREAM_WRITE: u64 = 0x40000;

const ALEX_TX_FILT_OFFSET: u32 = 0;
const ALEX_RX_OFFSET: u32 = 4;
const ALEX_TX_ANT_OFFSET: u32 = 8;

// RF GPIO register bits.
const MIC_BIAS_ENABLE_BIT: u32 = 0;
const MIC_PTT_SELECT_BIT: u32 = 1;
const MIC_SIGNAL_SELECT_BIT: u32 = 2;
const MIC_BIAS_SELECT_BIT: u32 = 3;
const SPKR_MUTE_BIT: u32 = 4;
const BALANCED_MIC_SELECT_BIT: u32 = 5;
const ADC1_RAND_BIT: u32 = 8;
const ADC1_PGA_BIT: u32 = 9;
const ADC1_DITHER_BIT: u32 = 10;
const ADC2_RAND_BIT: u32 = 11;
const ADC2_PGA_BIT: u32 = 12;
const ADC2_DITHER_BIT: u32 = 13;
const OPEN_COLLECTOR_SHIFT: u32 = 16;
const MOX_BIT: u32 = 24;
const TX_ENABLE_BIT: u32 = 25;
const DATA_ENDIAN_BIT: u32 = 26;
const TX_RELAY_DISABLE_BIT: u32 = 27;
const XVTR_ENABLE_BIT: u32 = 30;

// Status register bits.
const STATUS_KEY_IN_A: u32 = 2;
const STATUS_KEY_IN_B: u32 = 3;
const STATUS_USER_IO4: u32 = 4;
const STATUS_USER_IO8: u32 = 7;
const STATUS_PLL_LOCKED: u32 = 10;
const STATUS_CW_KEY_DOWN: u32 = 11;

// Keyer and iambic registers.
const KEYER_ENABLE_BIT: u32 = 31;
const KEYER_HANG_SHIFT: u32 = 8;
const KEYER_RAMP_SHIFT: u32 = 18;
const RAMP_SIZE: u32 = 4096;
const IAMBIC_WEIGHT_SHIFT: u32 = 8;
const IAMBIC_REVERSED_BIT: u32 = 16;
const IAMBIC_ENABLE_BIT: u32 = 17;
const IAMBIC_MODE_BIT: u32 = 18;
const IAMBIC_STRICT_BIT: u32 = 19;
const CW_BREAKIN_BIT: u32 = 23;
const IAMBIC_BITS: u32 = 0x000F_FFFF;

// TX config register.
const TX_CONFIG_PROTOCOL_BIT: u32 = 3;
const TX_CONFIG_SCALE_SHIFT: u32 = 4;
const TX_CONFIG_HPF_ENABLE_BIT: u32 = 27;
const TX_CONFIG_MUX_RESET_BIT: u32 = 29;
const TX_CONFIG_IQ_DEINTERLEAVE_BIT: u32 = 30;
const TX_CONFIG_IQ_STREAM_ENABLE_BIT: u32 = 31;

// FIFO reset register bits.
const FIFO_RESET_MIC_BIT: u32 = 0;
const FIFO_RESET_SPK_BIT: u32 = 1;
const FIFO_RESET_DDC_BIT: u32 = 2;
const FIFO_RESET_DUC_BIT: u32 = 3;

// TX amplitude scale, by board and firmware.
const TX_SCALE: u32 = 0x0001_FFFF;
const TX_SCALE_FW13: u32 = 0x0002_000;
const TX_SCALE_PCB_V3: u32 = 0x0002_A00;

// Firmware this is known to work with (saturnmain.c).
const FIRMWARE_MIN_MINOR: u32 = 8;
const FIRMWARE_MAX_MINOR: u32 = 18;
const FIRMWARE_MIN_MAJOR: u32 = 1;
const FIRMWARE_MAX_MAJOR: u32 = 1;

const SATURN_PRODUCT_ID: u32 = 1;
const SATURN_GOLDEN_CONFIG_ID: u32 = 3;
const SATURN_PRIMARY_CONFIG_ID: u32 = 4;

// Protocol 2 packet layout.
const DDC_PACKET_SIZE: usize = 1444;
const IQ_SAMPLES_PER_FRAME: usize = 238;
const IQ_BYTES_PER_FRAME: usize = 6 * IQ_SAMPLES_PER_FRAME;
const DUC_SAMPLES_PER_FRAME: usize = 240;
const DUC_WORDS_PER_FRAME: u32 = 180;
const DUC_TRANSFER_SIZE: usize = 1440;
const SPK_WORDS_PER_FRAME: u32 = 32;
const SPK_TRANSFER_SIZE: usize = 256;
const MIC_SAMPLES_PER_FRAME: u32 = 64;
const MIC_TRANSFER_SIZE: usize = 128;
const MIC_PACKET_SIZE: usize = 132;
const STATUS_PACKET_SIZE: usize = 60;

// The ports a network radio sends from, and is sent to.
const PORT_GENERAL: u16 = 1024;
const PORT_DDC_SPECIFIC: u16 = 1025;
const PORT_TX_SPECIFIC: u16 = 1026;
const PORT_HIGH_PRIORITY: u16 = 1027;
const PORT_AUDIO: u16 = 1028;
const PORT_TX_IQ: u16 = 1029;
const PORT_STATUS_FROM: u16 = 1025;
const PORT_MIC_FROM: u16 = 1026;
const PORT_DDC0_IQ_FROM: u16 = 1035;

/// How long a DMA write waits for room in its FIFO before the packet is
/// dropped -- longer than any real wait (a few ms), so only a stuck FPGA.
const FIFO_WAIT_LIMIT: Duration = Duration::from_millis(200);

/// HPSDR_SATURN_DEBUG set: say what's happening -- set-up steps, the
/// first packet of each kind from the host, run/MOX changes, and every
/// few seconds the receive stream's state. For testing on a G2 at a
/// distance, where each round trip is a day.
fn debug() -> bool {
    static DEBUG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *DEBUG.get_or_init(|| std::env::var_os("HPSDR_SATURN_DEBUG").is_some())
}

const DEBUG_EVERY: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// Discovery.

/// What the FPGA says about itself.
#[derive(Debug, Clone, Copy)]
pub struct Info {
    pub minor_version: u32,
    pub major_version: u32,
    pub pcb_version: u32,
    pub user_version: u32,
}

/// What discovery found at /dev/xdma0_user.
pub struct Found {
    pub info: Info,
    /// Firmware outside the versions this is known to work with.
    pub incompatible: bool,
    /// Another program (p2app, piHPSDR, another hpsdr-rs) has it open.
    pub in_use_by: Option<String>,
    /// eth0's MAC, to tell this radio's saved settings apart; "SATURN" if
    /// there's no eth0 (not zeros, which Ozy's settings use).
    pub mac: [u8; 6],
}

/// Looks for a Saturn FPGA behind XDMA: the device there, and version
/// registers that say Saturn (saturn_discovery). Nothing is written.
pub fn discover() -> Option<Found> {
    use std::os::unix::fs::FileTypeExt;
    let meta = match std::fs::metadata(XDMA_USER) {
        Ok(m) => m,
        Err(e) => {
            if debug() {
                eprintln!("saturn debug: {XDMA_USER}: {e}");
            }
            return None;
        }
    };
    if !meta.file_type().is_char_device() {
        return None;
    }
    let user = match OpenOptions::new().read(true).write(true).open(XDMA_USER) {
        Ok(f) => f,
        Err(e) => {
            // Usually permissions: worth saying whether debugging or not.
            eprintln!("saturn: can't open {XDMA_USER}: {e}");
            return None;
        }
    };
    let read = |addr: u32| read_register(&user, addr);
    // SW[31:25] major (valid from minor 18), SW[24:20] id, SW[19:4] minor,
    // SW[3:0] clocks; PR[31:16] product, PR[15:0] PCB version.
    let sw = read(ADDR_SW_VERSION);
    let product = read(ADDR_PROD_VERSION);
    let clocks = sw & 0xF;
    let swid = (sw >> 20) & 0x1F;
    let minor = (sw >> 4) & 0xFFFF;
    let major = if minor < 18 { 0 } else { (sw >> 25) & 0x7F };
    let product_id = (product >> 16) & 0xFFFF;
    if product_id != SATURN_PRODUCT_ID
        || (swid != SATURN_GOLDEN_CONFIG_ID && swid != SATURN_PRIMARY_CONFIG_ID)
        || clocks != 0xF
    {
        eprintln!(
            "saturn: {XDMA_USER} isn't a working Saturn (product {product_id}, id {swid}, clocks {clocks:#x})"
        );
        return None;
    }
    let info = Info { minor_version: minor, major_version: major, pcb_version: product & 0xFFFF, user_version: read(ADDR_USER_VERSION) };
    let incompatible = !((major == 0 && (FIRMWARE_MIN_MINOR..=FIRMWARE_MAX_MINOR).contains(&minor))
        || (FIRMWARE_MIN_MAJOR..=FIRMWARE_MAX_MAJOR).contains(&major));
    Some(Found { info, incompatible, in_use_by: xdma_user_in_use(), mac: eth0_mac() })
}

/// The name of another process holding /dev/xdma0_user open, if one is
/// (piHPSDR runs lsof for this; /proc says the same without it).
fn xdma_user_in_use() -> Option<String> {
    let me = std::process::id().to_string();
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let pid = entry.file_name().to_string_lossy().to_string();
        if !pid.chars().all(|c| c.is_ascii_digit()) || pid == me {
            continue;
        }
        let Ok(fds) = std::fs::read_dir(entry.path().join("fd")) else { continue };
        for fd in fds.flatten() {
            if std::fs::read_link(fd.path()).is_ok_and(|t| t.as_os_str() == XDMA_USER) {
                let name = std::fs::read_to_string(entry.path().join("comm")).unwrap_or_default();
                return Some(format!("{} (pid {pid})", name.trim()));
            }
        }
    }
    None
}

fn eth0_mac() -> [u8; 6] {
    let mut mac = [0u8; 6];
    if let Ok(text) = std::fs::read_to_string("/sys/class/net/eth0/address") {
        for (i, part) in text.trim().split(':').take(6).enumerate() {
            mac[i] = u8::from_str_radix(part, 16).unwrap_or(0);
        }
    }
    if mac == [0; 6] {
        mac = *b"SATURN";
    }
    mac
}

fn read_register(file: &File, addr: u32) -> u32 {
    let mut b = [0u8; 4];
    match file.read_exact_at(&mut b, addr as u64) {
        Ok(()) => u32::from_ne_bytes(b),
        Err(e) => {
            eprintln!("saturn: register read at {addr:#x} failed: {e}");
            0
        }
    }
}

// ---------------------------------------------------------------------------
// DMA buffers.

/// A page-aligned buffer for XDMA transfers (posix_memalign in the C).
struct DmaBuf {
    ptr: *mut u8,
    layout: Layout,
}

// Only ever used behind a Mutex or by one thread.
unsafe impl Send for DmaBuf {}

impl DmaBuf {
    fn new(size: usize) -> DmaBuf {
        let layout = Layout::from_size_align(size, 4096).expect("DMA buffer layout");
        // SAFETY: size is non-zero; the null check follows.
        let ptr = unsafe { alloc_zeroed(layout) };
        assert!(!ptr.is_null(), "DMA buffer allocation failed");
        DmaBuf { ptr, layout }
    }

    fn slice(&mut self, len: usize) -> &mut [u8] {
        assert!(len <= self.layout.size());
        // SAFETY: ptr is valid for layout.size() bytes, owned by self.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, len) }
    }
}

impl Drop for DmaBuf {
    fn drop(&mut self) {
        // SAFETY: allocated in new() with this layout.
        unsafe { dealloc(self.ptr, self.layout) }
    }
}

// ---------------------------------------------------------------------------
// Registers (saturnregisters.c, saturndrivers.c). The C keeps a copy of
// each write-only register and writes only on a change, each under its own
// mutex; here one mutex covers them all.

#[derive(Clone, Copy, PartialEq)]
enum Fifo {
    Ddc = 0,
    Duc = 1,
    Mic = 2,
    Spk = 3,
}

#[derive(Clone, Copy, PartialEq)]
enum Codec {
    /// TLV320AIC23B, PCB up to version 2.
    Aic23b,
    /// TLV320AIC3204, PCB version 3 on.
    Aic3204,
}

#[derive(Default)]
struct Shadow {
    gpio: u32,
    tx_config: u32,
    ddc_in_sel: u32,
    ddc_rate: u32,
    ddc_rate_written: Option<u32>,
    tx_dac: u32,
    adc_ctrl: u32,
    alex_tx_filt: u32,
    alex_tx_ant: u32,
    alex_rx: u32,
    iambic: u32,
    keyer: u32,
    ramp_length: u32,
    sidetone: Option<u32>,
    cw_enabled: bool,
    breakin: bool,
    codec_gain: u32,
    codec_path: u32,
    ddc_phase: [Option<u32>; NUM_DDC],
    duc_phase: Option<u32>,
}

struct Hw {
    user: File,
    info: Info,
    codec: Codec,
    fifo_depths: [u32; 4],
    /// Step attenuator and DAC current for each drive level 0-255.
    dac_current: [u32; 256],
    dac_step_atten: [u32; 256],
    s: Mutex<Shadow>,
}

impl Hw {
    fn read(&self, addr: u32) -> u32 {
        read_register(&self.user, addr)
    }

    fn write(&self, addr: u32, value: u32) {
        if let Err(e) = self.user.write_all_at(&value.to_ne_bytes(), addr as u64) {
            eprintln!("saturn: register write at {addr:#x} failed: {e}");
        }
    }

    /// A read-modify-write of the RF GPIO register.
    fn gpio(&self, f: impl FnOnce(u32) -> u32) {
        let mut s = self.s.lock().unwrap();
        let new = f(s.gpio);
        if new != s.gpio {
            s.gpio = new;
            self.write(ADDR_RF_GPIO, new);
        }
    }

    fn gpio_bit(&self, bit: u32, on: bool) {
        self.gpio(|r| if on { r | (1 << bit) } else { r & !(1 << bit) });
    }

    fn tx_config(&self, f: impl FnOnce(u32) -> u32) {
        let mut s = self.s.lock().unwrap();
        let new = f(s.tx_config);
        if new != s.tx_config {
            s.tx_config = new;
            self.write(ADDR_TX_CONFIG, new);
        }
    }

    fn codec_write(&self, reg: u32, data: u32) {
        self.write(ADDR_CODEC_SPI_WRITE, (reg << 9) | (data & 0x1FF));
        thread::sleep(Duration::from_micros(5));
    }

    // --- FIFOs ------------------------------------------------------------

    fn setup_fifo_monitor(&self, fifo: Fifo) {
        self.write(ADDR_FIFO_MON_BASE + 4 * fifo as u32 + 0x10, self.fifo_depths[fifo as usize]);
    }

    /// Occupied locations for a read FIFO, free ones for a write FIFO (64
    /// bits each). Reading clears its overflow flags.
    fn fifo_level(&self, fifo: Fifo) -> u32 {
        let current = self.read(ADDR_FIFO_MON_BASE + 4 * fifo as u32) & 0xFFFF;
        match fifo {
            Fifo::Duc | Fifo::Spk => self.fifo_depths[fifo as usize].saturating_sub(current),
            Fifo::Ddc | Fifo::Mic => current,
        }
    }

    fn reset_fifo(&self, fifo: Fifo) {
        let bit = 1
            << match fifo {
                Fifo::Ddc => FIFO_RESET_DDC_BIT,
                Fifo::Duc => FIFO_RESET_DUC_BIT,
                Fifo::Mic => FIFO_RESET_MIC_BIT,
                Fifo::Spk => FIFO_RESET_SPK_BIT,
            };
        let _s = self.s.lock().unwrap();
        let r = self.read(ADDR_FIFO_RESET);
        self.write(ADDR_FIFO_RESET, r & !bit);
        self.write(ADDR_FIFO_RESET, r | bit);
    }

    /// Waits until `fifo` has at least `needed` locations (to read, or
    /// free to write); false if it never does.
    fn wait_fifo(&self, fifo: Fifo, needed: u32, poll: Duration, stop: Option<&AtomicBool>) -> bool {
        let start = Instant::now();
        while self.fifo_level(fifo) < needed {
            if stop.is_some_and(|s| s.load(Ordering::Relaxed)) {
                return false;
            }
            if stop.is_none() && start.elapsed() > FIFO_WAIT_LIMIT {
                return false;
            }
            thread::sleep(poll);
        }
        true
    }

    // --- Setup ------------------------------------------------------------

    /// saturn_register_init: codec, drive tables, keyer defaults, P2 TX.
    fn initialise(&self) {
        self.gpio_bit(SPKR_MUTE_BIT, true);
        thread::sleep(Duration::from_millis(10));
        match self.codec {
            Codec::Aic3204 => self.init_aic3204(),
            Codec::Aic23b => self.init_aic23b(),
        }
        self.set_keyer_params(30, 500, 9);
        self.set_sidetone(true, 50, 800);
        // P2's 192 kHz TX.
        self.tx_config(|r| (r & 0xFFFF_FFF7) | (1 << TX_CONFIG_PROTOCOL_BIT));
        self.enable_cw(false, false);
        // Samples in network byte order, as Protocol 2 has them.
        self.gpio_bit(DATA_ENDIAN_BIT, true);
        self.gpio_bit(SPKR_MUTE_BIT, false);
        let scale = if self.info.pcb_version <= 2 {
            if self.info.minor_version < 13 { TX_SCALE } else { TX_SCALE_FW13 }
        } else {
            TX_SCALE_PCB_V3
        };
        self.tx_config(|r| (r & 0xFFC0_000F) | ((scale & 0x3FFFF) << TX_CONFIG_SCALE_SHIFT));
        self.gpio_bit(BALANCED_MIC_SELECT_BIT, false);
    }

    fn init_aic23b(&self) {
        let mut s = self.s.lock().unwrap();
        s.codec_gain = 0;
        s.codec_path = 0x14; // mic input, no boost
        let pause = || thread::sleep(Duration::from_micros(100));
        self.codec_write(15, 0x0); // reset
        pause();
        self.codec_write(9, 0x1); // active
        pause();
        self.codec_write(4, s.codec_path);
        pause();
        self.codec_write(6, 0x0); // all powered on
        pause();
        self.codec_write(7, 0x2); // slave, 16 bit I2S
        pause();
        self.codec_write(8, 0x0); // 256 Fs
        pause();
        self.codec_write(5, 0x0); // ADC high pass on
        pause();
        self.codec_write(0, s.codec_gain);
        pause();
    }

    fn init_aic3204(&self) {
        let mut s = self.s.lock().unwrap();
        s.codec_gain = 46; // mic preamp 23 dB
        s.codec_path = 0x04; // IN3 to the PGA, 10k
        let w = |r, d| self.codec_write(r, d);
        // Page 0: software reset.
        w(0, 0x00);
        w(1, 0x01);
        thread::sleep(Duration::from_millis(2));
        // Clocks: MCLK 12.288 MHz, 48 kHz, NDAC 1, MDAC 2; ADC clocks = DAC's.
        w(11, 0x81);
        w(12, 0x82);
        w(18, 0x01);
        w(19, 0x02);
        // PRB_P1 processing blocks.
        w(60, 0x01);
        w(61, 0x01);
        // Page 1: power.
        w(0, 0x01);
        w(1, 0x08);
        w(2, 0x09);
        w(123, 0x00);
        w(1, 0x08);
        w(2, 0x01);
        w(61, 0x00);
        w(71, 0x32);
        // Recording.
        w(0x00, 0x01);
        w(58, 0x30);
        w(52, s.codec_path);
        w(55, s.codec_path);
        w(54, 0x40);
        w(57, 0x40);
        w(71, 0x32);
        w(59, s.codec_gain);
        w(60, s.codec_gain);
        w(51, 0x68); // mic bias
        w(0x00, 0x00);
        w(81, 0xC0); // ADCs up
        w(82, 0x00); // and unmuted
        // Playback, with the anti-thump steps.
        w(0, 0x01);
        w(20, 0x65);
        w(10, 0x3B);
        w(12, 0x08);
        w(13, 0x08);
        w(14, 0x08);
        w(15, 0x08);
        w(22, 0x72);
        w(23, 0x72);
        w(0x00, 0x00);
        w(0x3F, 0xD6);
        w(0x00, 0x01);
        w(16, 0x00);
        w(17, 0x00);
        w(9, 0x3F);
        w(18, 0x00);
        w(19, 0x00);
        w(0x00, 0x00);
        w(65, 0x00);
        w(66, 0x00);
        thread::sleep(Duration::from_millis(300));
        w(64, 0x00); // DACs unmuted
    }

    // --- Receive ----------------------------------------------------------

    fn set_ddc_frequency(&self, ddc: usize, phase: u32) {
        let mut s = self.s.lock().unwrap();
        if s.ddc_phase[ddc] != Some(phase) {
            s.ddc_phase[ddc] = Some(phase);
            self.write(ADDR_DDC_REGS[ddc], phase);
        }
    }

    /// ADC for a DDC: 0 ADC1, 1 ADC2, 3 TX samples (PureSignal).
    fn set_ddc_adc(&self, ddc: usize, adc: u32) {
        let mut s = self.s.lock().unwrap();
        let shift = 2 * ddc as u32;
        let new = (s.ddc_in_sel & !(3 << shift)) | ((adc & 3) << shift);
        if new != s.ddc_in_sel {
            s.ddc_in_sel = new;
            self.write(ADDR_DDC_IN_SEL, new);
        }
    }

    fn set_ddc_enabled(&self, on: bool) {
        let mut s = self.s.lock().unwrap();
        let new = if on { s.ddc_in_sel | (1 << 30) } else { s.ddc_in_sel & !(1 << 30) };
        if new != s.ddc_in_sel {
            s.ddc_in_sel = new;
            self.write(ADDR_DDC_IN_SEL, new);
        }
    }

    /// A DDC's rate (kHz) into the rate register's copy; written by
    /// write_ddc_rates once all are set. 7 = interleaved with the next.
    fn set_ddc_rate(&self, ddc: usize, enabled: bool, khz: u16, interleave: bool) {
        let code: u32 = if !enabled {
            0
        } else if interleave {
            7
        } else {
            match khz {
                96 => 2,
                192 => 3,
                384 => 4,
                768 => 5,
                1536 => 6,
                _ => 1,
            }
        };
        let mut s = self.s.lock().unwrap();
        let shift = 3 * ddc as u32;
        s.ddc_rate = (s.ddc_rate & !(7 << shift)) | (code << shift);
    }

    fn write_ddc_rates(&self) {
        let mut s = self.s.lock().unwrap();
        if s.ddc_rate_written != Some(s.ddc_rate) {
            s.ddc_rate_written = Some(s.ddc_rate);
            self.write(ADDR_DDC_RATES, s.ddc_rate);
            if debug() {
                // 3 bits a DDC from DDC0: 0 off, 1-6 48-1536 kHz, 7 paired.
                let codes: Vec<String> = (0..NUM_DDC).map(|d| ((s.ddc_rate >> (3 * d)) & 7).to_string()).collect();
                eprintln!("saturn debug: DDC rates {} (input select {:#010x})", codes.join(" "), s.ddc_in_sel);
            }
        }
    }

    fn set_adc_options(&self, dither1: bool, random1: bool, dither2: bool, random2: bool) {
        self.gpio(|mut r| {
            for bit in [ADC1_RAND_BIT, ADC1_PGA_BIT, ADC1_DITHER_BIT, ADC2_RAND_BIT, ADC2_PGA_BIT, ADC2_DITHER_BIT] {
                r &= !(1 << bit);
            }
            for (on, bit) in [(dither1, ADC1_DITHER_BIT), (random1, ADC1_RAND_BIT), (dither2, ADC2_DITHER_BIT), (random2, ADC2_RAND_BIT)] {
                if on {
                    r |= 1 << bit;
                }
            }
            r
        });
    }

    /// The 5-bit step attenuators: for each ADC, set its RX and/or TX value.
    fn set_adc_attenuator(&self, adc1: Option<(u32, bool, bool)>, adc2: Option<(u32, bool, bool)>) {
        let mut s = self.s.lock().unwrap();
        let mut r = s.adc_ctrl;
        for (setting, shift) in [(adc1, 0u32), (adc2, 10)] {
            if let Some((atten, rx, tx)) = setting {
                if rx {
                    r = (r & !(0x1F << shift)) | ((atten & 0x1F) << shift);
                }
                if tx {
                    r = (r & !(0x1F << (shift + 5))) | ((atten & 0x1F) << (shift + 5));
                }
            }
        }
        if r != s.adc_ctrl {
            s.adc_ctrl = r;
            self.write(ADDR_ADC_CTRL, r);
        }
    }

    /// One receiver's 16-bit Alex word: RX 0 into the low half, 2 the high.
    fn alex_rx_filters(&self, bits: u32, rx: u32) {
        let mut s = self.s.lock().unwrap();
        let r = if rx != 2 { (s.alex_rx & 0xFFFF_0000) | bits } else { (s.alex_rx & 0x0000_FFFF) | (bits << 16) };
        if r != s.alex_rx {
            s.alex_rx = r;
            self.write(ADDR_ALEX_SPI + ALEX_RX_OFFSET, r);
        }
    }

    /// The Alex TX word; firmware 12 on has a second register with the TX
    /// antenna in it.
    fn alex_tx_filters(&self, bits: u32, tx_ant_register: bool) {
        let mut s = self.s.lock().unwrap();
        if tx_ant_register {
            if bits != s.alex_tx_ant {
                s.alex_tx_ant = bits;
                self.write(ADDR_ALEX_SPI + ALEX_TX_ANT_OFFSET, bits);
            }
        } else if bits != s.alex_tx_filt {
            s.alex_tx_filt = bits;
            self.write(ADDR_ALEX_SPI + ALEX_TX_FILT_OFFSET, bits);
        }
    }

    // --- Transmit ---------------------------------------------------------

    fn set_duc_frequency(&self, phase: u32) {
        {
            let mut s = self.s.lock().unwrap();
            if s.duc_phase != Some(phase) {
                s.duc_phase = Some(phase);
                self.write(ADDR_TX_DUC, phase);
            }
        }
        // PCB V3 on: the TX high pass filter above 49 MHz.
        if self.info.pcb_version >= 3 {
            const PHASE_49_MHZ: u32 = 1_712_674_133;
            let hpf = phase > PHASE_49_MHZ;
            self.tx_config(|r| if hpf { r | (1 << TX_CONFIG_HPF_ENABLE_BIT) } else { r & !(1 << TX_CONFIG_HPF_ENABLE_BIT) });
        }
    }

    /// Protocol 2's 0-255 drive level, as the step attenuator (0.5 dB
    /// steps) plus the DAC current for what's left between two steps.
    fn set_drive_level(&self, level: u8) {
        let dac = self.dac_current[level as usize];
        let atten = self.dac_step_atten[level as usize];
        let value = dac | (dac << 8) | (atten << 16) | (atten << 24);
        let mut s = self.s.lock().unwrap();
        if value != s.tx_dac {
            s.tx_dac = value;
            self.write(ADDR_DAC_CTRL, value);
        }
    }

    fn set_mox(&self, mox: bool) {
        self.gpio_bit(MOX_BIT, mox);
        let (cw, breakin) = {
            let s = self.s.lock().unwrap();
            (s.cw_enabled, s.breakin)
        };
        // The keyer runs in CW while transmitting, or with break-in.
        self.activate_keyer(if mox { cw } else { cw && breakin });
    }

    fn activate_keyer(&self, on: bool) {
        let mut s = self.s.lock().unwrap();
        let new = if on { s.keyer | (1 << KEYER_ENABLE_BIT) } else { s.keyer & !(1 << KEYER_ENABLE_BIT) };
        if new != s.keyer {
            s.keyer = new;
            self.write(ADDR_KEYER_CONFIG, new);
        }
    }

    /// CW as the modulation source, or IQ; the keyer runs with break-in.
    fn enable_cw(&self, enabled: bool, breakin: bool) {
        {
            let mut s = self.s.lock().unwrap();
            s.cw_enabled = enabled;
            s.breakin = breakin;
        }
        // Modulation source: 0 IQ data, 3 CW keyer.
        self.tx_config(|r| (r & 0xFFFF_FFFC) | if enabled { 3 } else { 0 });
        self.activate_keyer(enabled && breakin);
    }

    fn set_sidetone(&self, enabled: bool, volume: u8, frequency: u16) {
        // For 48 kHz.
        let mut r = (512 * frequency as u32) / 375;
        if enabled {
            r |= (volume as u32) << 24;
        }
        let mut s = self.s.lock().unwrap();
        if s.sidetone != Some(r) {
            s.sidetone = Some(r);
            self.write(ADDR_SIDETONE_CONFIG, r);
        }
    }

    fn set_iambic(&self, speed: u8, weight: u8, reversed: bool, mode_b: bool, strict: bool, iambic: bool, breakin: bool) {
        let mut s = self.s.lock().unwrap();
        let mut r = s.iambic & !IAMBIC_BITS;
        r |= speed as u32;
        r |= (weight as u32) << IAMBIC_WEIGHT_SHIFT;
        for (on, bit) in [
            (reversed, IAMBIC_REVERSED_BIT),
            (mode_b, IAMBIC_MODE_BIT),
            (strict, IAMBIC_STRICT_BIT),
            (iambic, IAMBIC_ENABLE_BIT),
            (breakin, CW_BREAKIN_BIT),
        ] {
            if on {
                r |= 1 << bit;
            }
        }
        if r != s.iambic {
            s.iambic = r;
            self.write(ADDR_IAMBIC_CONFIG, r);
        }
    }

    /// RF delay (ms), hang time (ms) and the keying ramp (ms).
    fn set_keyer_params(&self, delay: u8, hang: u16, ramp: u8) {
        let mut s = self.s.lock().unwrap();
        let mut r = s.keyer & 0xFFFC_0000;
        r |= delay as u32;
        r |= (hang as u32 & 0x3FF) << KEYER_HANG_SHIFT;
        if ramp > 0 {
            self.load_keyer_ramp(&mut s, ramp);
            r &= 0x8003_FFFF;
            if self.info.minor_version >= 14 {
                r |= s.ramp_length << KEYER_RAMP_SHIFT; // end address in words
            } else {
                r |= (s.ramp_length << 2) << KEYER_RAMP_SHIFT; // in bytes
            }
        }
        if r != s.keyer {
            s.keyer = r;
            self.write(ADDR_KEYER_CONFIG, r);
        }
    }

    /// DL1YCF's keying ramp into the keyer's RAM, when its length changes
    /// (InitialiseCWKeyerRamp): 5 to 10 ms, 20 from firmware 14.
    fn load_keyer_ramp(&self, s: &mut Shadow, ms: u8) {
        let max = if self.info.minor_version >= 14 { 20 } else { 10 };
        let length = ms.clamp(5, max) as u32 * 192;
        if length == s.ramp_length {
            return;
        }
        for i in 0..length {
            let y = i as f64 / length as f64;
            let y2 = y * std::f64::consts::TAU;
            let sample = y - 0.12182865361171612 * y2.sin() - 0.018557469249199286 * (2.0 * y2).sin()
                - 0.0009378783245428506 * (3.0 * y2).sin()
                + 0.0008567571519403228 * (4.0 * y2).sin()
                + 0.00018706912431472442 * (5.0 * y2).sin();
            self.write(ADDR_CW_KEYER_RAM + 4 * i, (sample * 8_388_607.0) as u32);
        }
        for i in length..RAMP_SIZE {
            self.write(ADDR_CW_KEYER_RAM + 4 * i, 8_388_607);
        }
        s.ramp_length = length;
    }

    /// Line or mic input, mic boost, line-in gain (0-31).
    fn set_codec_input(&self, line: bool, boost: bool, line_gain: u32) {
        let mut s = self.s.lock().unwrap();
        match self.codec {
            Codec::Aic23b => {
                let mut path = s.codec_path & 0xFFF8;
                let mut gain = s.codec_gain & 0xFFE0;
                if line {
                    path |= 0x02;
                    gain |= line_gain & 0x1F;
                } else {
                    path |= 0x04;
                    if boost {
                        path |= 0x01;
                    }
                }
                if path != s.codec_path {
                    s.codec_path = path;
                    self.codec_write(4, path);
                }
                if gain != s.codec_gain {
                    s.codec_gain = gain;
                    self.codec_write(0, gain);
                }
            }
            Codec::Aic3204 => {
                let (path, gain) = if line { (0xC0, 3 * (line_gain & 0x1F)) } else { (0x04, if boost { 46 } else { 6 }) };
                if path != s.codec_path {
                    s.codec_path = path;
                    self.codec_write(0x00, 0x01);
                    self.codec_write(52, path);
                    self.codec_write(55, path);
                }
                if gain != s.codec_gain {
                    s.codec_gain = gain;
                    self.codec_write(0x00, 0x01);
                    self.codec_write(59, gain);
                    self.codec_write(60, gain);
                }
            }
        }
    }

    /// Orion-style mic jack: mic on the ring or the tip, bias, PTT.
    fn set_mic_options(&self, ring: bool, bias: bool, ptt: bool) {
        self.gpio(|mut r| {
            for bit in [MIC_BIAS_ENABLE_BIT, MIC_PTT_SELECT_BIT, MIC_SIGNAL_SELECT_BIT, MIC_BIAS_SELECT_BIT] {
                r &= !(1 << bit);
            }
            if ring {
                r |= (1 << MIC_SIGNAL_SELECT_BIT) | (1 << MIC_PTT_SELECT_BIT);
            } else {
                r |= 1 << MIC_BIAS_SELECT_BIT;
            }
            if bias {
                r |= 1 << MIC_BIAS_ENABLE_BIT;
            }
            // `ptt` isn't a register bit (saturnregisters.c doesn't use it).
            let _ = ptt;
            r
        });
    }

    fn reset_duc_mux(&self) {
        let mut s = self.s.lock().unwrap();
        let r = s.tx_config | (1 << TX_CONFIG_MUX_RESET_BIT);
        self.write(ADDR_TX_CONFIG, r);
        let r = r & !(1 << TX_CONFIG_MUX_RESET_BIT);
        self.write(ADDR_TX_CONFIG, r);
        s.tx_config = r;
    }

    // --- Status -----------------------------------------------------------

    /// Protocol 2's byte 4 bits from the status register: PTT (or the
    /// keyer keying), dot, dash (or IO8), PLL locked.
    fn ptt_key_bits(status: u32) -> u8 {
        let bit = |b: u32| (status >> b) & 1 != 0;
        let mut r = 0;
        if status & 1 != 0 || bit(STATUS_CW_KEY_DOWN) {
            r |= 1;
        }
        if bit(STATUS_KEY_IN_A) {
            r |= 2;
        }
        if bit(STATUS_KEY_IN_B) || !bit(STATUS_USER_IO8) {
            r |= 4;
        }
        if bit(STATUS_PLL_LOCKED) {
            r |= 16;
        }
        r
    }

    /// IO4, IO5, ATU and IO8 (inverted) for byte 59.
    fn user_io_bits(status: u32) -> u8 {
        let mut r = (status >> STATUS_USER_IO4) & 0b1011;
        r ^= 0x8;
        r |= (status >> 7) & 0b0100;
        r as u8
    }
}

/// DAC current and step attenuator for each drive level 0-255
/// (InitialiseDACAttenROMs): mostly the attenuator, the current
/// interpolating between its 0.5 dB steps.
fn drive_tables() -> ([u32; 256], [u32; 256]) {
    let mut current = [0u32; 256];
    let mut atten = [0u32; 256];
    atten[0] = 63;
    for level in 1..256 {
        let wanted = 20.0 * (255.0 / level as f64).log10();
        let step = ((2.0 * wanted) as u32).min(63);
        let residual = wanted - step as f64 * 0.5;
        current[level] = (255.0 / 10f64.powf(residual / 20.0)) as u32;
        atten[level] = step;
    }
    (current, atten)
}

/// Samples per frame for each DDC from a DDC stream header
/// (AnalyseDDCHeader): 3 bits a DDC, 7 meaning this and the next are an
/// interleaved pair whose samples all come in this one's share, at the
/// next one's rate. The frame's total, in 64-bit words.
fn analyse_ddc_header(mut header: u32, counts: &mut [u32; NUM_DDC]) -> u32 {
    const SAMPLES: [u32; 8] = [0, 1, 2, 4, 8, 16, 32, 0];
    let mut total = 0;
    let mut ddc = 0;
    while ddc < NUM_DDC {
        let rate = header & 7;
        if rate != 7 {
            counts[ddc] = SAMPLES[rate as usize];
            total += counts[ddc];
        } else {
            header >>= 3;
            let count = 2 * SAMPLES[(header & 7) as usize];
            counts[ddc] = count;
            total += count;
            if ddc + 1 < NUM_DDC {
                counts[ddc + 1] = 0;
            }
            ddc += 1;
        }
        header >>= 3;
        ddc += 1;
    }
    total
}

// ---------------------------------------------------------------------------
// The link: what radio.rs talks to in place of a socket.

pub struct SaturnLink {
    hw: Arc<Hw>,
    /// Packets from the "radio": (the port it'd come from, bytes).
    from_radio: Mutex<Receiver<(u16, Vec<u8>)>>,
    /// The Protocol 2 run bit: packets only flow while it's set.
    running: Arc<AtomicBool>,
    mox: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    threads: Mutex<Vec<JoinHandle<()>>>,
    duc: Mutex<(File, DmaBuf)>,
    spk: Mutex<(File, DmaBuf)>,
    /// HPSDR_SATURN_DEBUG: which host ports have been heard from, a bit
    /// each from 1024.
    ports_seen: AtomicU32,
}

impl SaturnLink {
    /// Opens the FPGA, sets it up as piHPSDR's saturn_register_init and
    /// saturn_init do, and starts the threads that read it.
    pub fn open() -> io::Result<Arc<SaturnLink>> {
        let found = discover().ok_or_else(|| io::Error::other("no Saturn FPGA found on XDMA"))?;
        if let Some(who) = &found.in_use_by {
            return Err(io::Error::other(format!("{XDMA_USER} is in use by {who}")));
        }
        let info = found.info;
        let user = OpenOptions::new()
            .read(true)
            .write(true)
            .open(XDMA_USER)
            .map_err(|e| io::Error::new(e.kind(), format!("{XDMA_USER}: {e}")))?;
        let fifo_depths = if info.minor_version < 10 {
            [8192, 1024, 256, 256]
        } else if info.minor_version <= 12 {
            [16384, 2048, 256, 1024]
        } else {
            [16384, 4096, 256, 1024]
        };
        let (dac_current, dac_step_atten) = drive_tables();
        let hw = Arc::new(Hw {
            user,
            info,
            codec: if info.pcb_version >= 3 { Codec::Aic3204 } else { Codec::Aic23b },
            fifo_depths,
            dac_current,
            dac_step_atten,
            s: Mutex::new(Shadow::default()),
        });
        eprintln!(
            "saturn: FPGA firmware {}.{} (user version {:#x}), PCB version {}, {:?} codec",
            info.major_version,
            info.minor_version,
            info.user_version,
            info.pcb_version,
            if hw.codec == Codec::Aic3204 { "TLV320AIC3204" } else { "TLV320AIC23B" }
        );
        // Named in the error, so "Permission denied" says which.
        let open_dev = |path: &str, write: bool| {
            OpenOptions::new()
                .read(!write)
                .write(write)
                .open(path)
                .map_err(|e| io::Error::new(e.kind(), format!("{path}: {e}")))
        };
        hw.initialise();
        if debug() {
            eprintln!("saturn debug: registers and codec set up");
        }

        // Speaker audio (saturn_init_speaker_audio).
        let spk_file = open_dev(SPK_DMA_DEVICE, true)?;
        hw.reset_fifo(Fifo::Spk);
        hw.setup_fifo_monitor(Fifo::Spk);

        // TX IQ (saturn_init_duc_iq).
        let duc_file = open_dev(DUC_DMA_DEVICE, true)?;
        hw.tx_config(|r| r & !(1 << TX_CONFIG_IQ_STREAM_ENABLE_BIT));
        hw.tx_config(|r| r & !(1 << TX_CONFIG_IQ_DEINTERLEAVE_BIT));
        hw.reset_duc_mux();
        hw.reset_fifo(Fifo::Duc);
        hw.setup_fifo_monitor(Fifo::Duc);
        hw.tx_config(|r| r | (1 << TX_CONFIG_IQ_STREAM_ENABLE_BIT));

        let ddc_file = open_dev(DDC_DMA_DEVICE, false)?;
        let mic_file = open_dev(MIC_DMA_DEVICE, false)?;
        if debug() {
            eprintln!("saturn debug: DMA devices open, TX and speaker FIFOs reset");
        }

        // Bounded: if nothing reads them, they're dropped, not hoarded.
        let (to_host, from_radio) = mpsc::sync_channel(4096);
        let running = Arc::new(AtomicBool::new(false));
        let mox = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let mut threads = Vec::new();
        {
            let (hw, to_host, running, stop) = (Arc::clone(&hw), to_host.clone(), Arc::clone(&running), Arc::clone(&stop));
            threads.push(thread::Builder::new().name("saturn rx".into()).spawn(move || rx_loop(hw, ddc_file, to_host, running, stop))?);
        }
        {
            let (hw, to_host, running, stop) = (Arc::clone(&hw), to_host.clone(), Arc::clone(&running), Arc::clone(&stop));
            threads.push(thread::Builder::new().name("saturn mic".into()).spawn(move || mic_loop(hw, mic_file, to_host, running, stop))?);
        }
        {
            let (hw, running, mox, stop) = (Arc::clone(&hw), Arc::clone(&running), Arc::clone(&mox), Arc::clone(&stop));
            threads.push(
                thread::Builder::new().name("saturn status".into()).spawn(move || status_loop(hw, to_host, running, mox, stop))?,
            );
        }
        Ok(Arc::new(SaturnLink {
            hw,
            from_radio: Mutex::new(from_radio),
            running,
            mox,
            stop,
            threads: Mutex::new(threads),
            duc: Mutex::new((duc_file, DmaBuf::new(32768))),
            spk: Mutex::new((spk_file, DmaBuf::new(32768))),
            ports_seen: AtomicU32::new(0),
        }))
    }

    /// A packet "sent to the radio" on `port`.
    pub fn send(&self, port: u16, packet: &[u8]) {
        if debug() && (PORT_GENERAL..=PORT_TX_IQ).contains(&port) {
            let bit = 1u32 << (port - PORT_GENERAL);
            if self.ports_seen.fetch_or(bit, Ordering::Relaxed) & bit == 0 {
                eprintln!("saturn debug: first packet from the host to port {port}, {} bytes", packet.len());
            }
        }
        match port {
            PORT_GENERAL if packet.len() >= 59 => self.general(packet),
            PORT_DDC_SPECIFIC if packet.len() >= 1444 => self.ddc_specific(packet),
            PORT_TX_SPECIFIC if packet.len() >= 60 => self.tx_specific(packet),
            PORT_HIGH_PRIORITY if packet.len() >= 1444 => self.high_priority(packet),
            PORT_AUDIO if packet.len() >= 4 + SPK_TRANSFER_SIZE => self.speaker_audio(packet),
            PORT_TX_IQ if packet.len() >= 4 + 6 * DUC_SAMPLES_PER_FRAME => self.tx_iq(packet),
            _ => {}
        }
    }

    /// The next packet "from the radio", waiting up to `timeout`.
    pub fn recv(&self, timeout: Duration) -> Option<(u16, Vec<u8>)> {
        self.from_radio.lock().unwrap().recv_timeout(timeout).ok()
    }

    /// Stops the threads and leaves the radio receiving nothing, not
    /// transmitting (saturn_exit).
    pub fn close(&self) {
        self.running.store(false, Ordering::Relaxed);
        self.mox.store(false, Ordering::Relaxed);
        self.hw.set_mox(false);
        self.hw.gpio_bit(TX_ENABLE_BIT, false);
        self.hw.enable_cw(false, false);
        self.stop.store(true, Ordering::Relaxed);
        for t in self.threads.lock().unwrap().drain(..) {
            let _ = t.join();
        }
        self.hw.set_ddc_enabled(false);
    }

    // --- Packets to the radio (saturn_handle_*) ---------------------------

    fn general(&self, p: &[u8]) {
        // Alex is always on; only the PA enable bit counts.
        self.hw.gpio_bit(TX_RELAY_DISABLE_BIT, p[58] & 0x01 == 0);
    }

    fn ddc_specific(&self, p: &[u8]) {
        let hw = &self.hw;
        hw.set_adc_options(p[5] & 1 != 0, p[6] & 1 != 0, p[5] & 2 != 0, p[6] & 2 != 0);
        let enables = u16::from_le_bytes([p[7], p[8]]);
        for ddc in 0..NUM_DDC {
            let mut enabled = enables & (1 << ddc) != 0;
            let adc = match p[17 + 6 * ddc] {
                1 => 1,
                2 => 3, // TX samples, for PureSignal
                _ => 0,
            };
            hw.set_ddc_adc(ddc, adc);
            let khz = u16::from_be_bytes([p[18 + 6 * ddc], p[19 + 6 * ddc]]);
            // A synchronised pair (DDC0/1, 2/3, 4/5, 6/7): the even one is
            // "interleaved", the odd one enabled whatever the enable bits
            // say. The sync byte for DDC n is 1363 + n, and names n + 1.
            let pair = ddc & !1;
            let synced = pair < 8 && p[1363 + pair] == 1u8 << (pair + 1);
            let interleave = synced && ddc == pair;
            if synced && ddc != pair {
                enabled = true;
            }
            hw.set_ddc_rate(ddc, enabled, khz, interleave);
        }
        hw.write_ddc_rates();
    }

    fn tx_specific(&self, p: &[u8]) {
        let hw = &self.hw;
        let b = p[5];
        let cw = b & 0x02 != 0;
        let breakin = b & 0x80 != 0;
        hw.set_iambic(p[9], p[10], b & 0x04 != 0, b & 0x20 != 0, b & 0x40 != 0, b & 0x08 != 0, breakin);
        hw.set_sidetone(b & 0x10 != 0, p[6], u16::from_be_bytes([p[7], p[8]]));
        hw.enable_cw(cw, breakin);
        hw.set_keyer_params(p[13], u16::from_be_bytes([p[11], p[12]]), p[17]);
        let mic = p[50];
        hw.set_codec_input(mic & 0x01 != 0, mic & 0x02 != 0, p[51] as u32 & 0x1F);
        // Bit 2 set means PTT off.
        hw.set_mic_options(mic & 0x08 != 0, mic & 0x10 != 0, mic & 0x04 == 0);
        hw.gpio_bit(BALANCED_MIC_SELECT_BIT, mic & 0x20 != 0);
        // Attenuators while transmitting: byte 58 ADC1, 59 ADC0.
        hw.set_adc_attenuator(Some((p[59] as u32, false, true)), Some((p[58] as u32, false, true)));
    }

    fn high_priority(&self, p: &[u8]) {
        let hw = &self.hw;
        for ddc in 0..NUM_DDC {
            let i = 9 + 4 * ddc;
            hw.set_ddc_frequency(ddc, u32::from_be_bytes([p[i], p[i + 1], p[i + 2], p[i + 3]]));
        }
        let run = p[4] & 0x01 != 0;
        let mox = run && p[4] & 0x02 != 0;
        if debug() && (run != self.running.load(Ordering::Relaxed) || mox != self.mox.load(Ordering::Relaxed)) {
            let phase = u32::from_be_bytes([p[9], p[10], p[11], p[12]]);
            eprintln!(
                "saturn debug: run {run}, MOX {mox}; DDC0 at {:.0} Hz, drive {}",
                phase as f64 * 122_880_000.0 / 4_294_967_296.0,
                p[345]
            );
        }
        if run {
            hw.gpio_bit(TX_ENABLE_BIT, true);
        } else {
            hw.gpio_bit(TX_ENABLE_BIT, false);
            hw.enable_cw(false, false);
        }
        self.mox.store(mox, Ordering::Relaxed);
        hw.set_mox(mox);
        self.running.store(run, Ordering::Relaxed);

        hw.set_duc_frequency(u32::from_be_bytes([p[329], p[330], p[331], p[332]]));
        hw.set_drive_level(p[345]);
        hw.gpio_bit(XVTR_ENABLE_BIT, p[1400] & 0x01 != 0);
        hw.gpio_bit(SPKR_MUTE_BIT, p[1400] & 0x02 != 0);
        // Open collectors: bits 7:1 of the packet, 22:16 of the register.
        let oc = ((p[1401] >> 1) & 0x7F) as u32;
        hw.gpio(|r| (r & !(0x7F << OPEN_COLLECTOR_SHIFT)) | (oc << OPEN_COLLECTOR_SHIFT));

        // Alex TX: firmware 12 on has a separate TX antenna register. With
        // a host that fills in byte 1428 (TX antenna), its word goes there
        // and 1432's to the filter register; otherwise 1432's to both. The
        // PA is enabled from bit 11 (27 of the Alex word), as p2app does.
        let word = |i: usize| ((p[i] as u32) << 8) | p[i + 1] as u32;
        let pa_enable;
        if self.hw.info.minor_version >= 12 && p[1428] & 0x07 != 0 {
            let ant = word(1428);
            pa_enable = ant & 0x0800 != 0;
            hw.alex_tx_filters(ant, true);
            hw.alex_tx_filters(word(1432), false);
        } else if self.hw.info.minor_version >= 12 {
            let w = word(1432);
            pa_enable = w & 0x0800 != 0;
            hw.alex_tx_filters(w, true);
            hw.alex_tx_filters(w, false);
        } else {
            let w = word(1432);
            pa_enable = w & 0x0800 != 0;
            hw.alex_tx_filters(w, false);
        }
        hw.gpio_bit(TX_RELAY_DISABLE_BIT, !pa_enable);
        hw.alex_rx_filters(word(1430), 2);
        hw.alex_rx_filters(word(1434), 0);
        // Attenuators while receiving: byte 1443 ADC0, 1442 ADC1.
        hw.set_adc_attenuator(Some((p[1443] as u32, true, false)), Some((p[1442] as u32, true, false)));
    }

    fn speaker_audio(&self, p: &[u8]) {
        let mut guard = self.spk.lock().unwrap();
        if !self.hw.wait_fifo(Fifo::Spk, SPK_WORDS_PER_FRAME, Duration::from_millis(1), None) {
            return;
        }
        let (file, buf) = &mut *guard;
        let dma = buf.slice(SPK_TRANSFER_SIZE);
        dma.copy_from_slice(&p[4..4 + SPK_TRANSFER_SIZE]);
        if let Err(e) = file.write_all_at(dma, ADDR_SPK_STREAM_WRITE) {
            eprintln!("saturn: speaker DMA failed: {e}");
        }
    }

    fn tx_iq(&self, p: &[u8]) {
        let mut guard = self.duc.lock().unwrap();
        if !self.hw.wait_fifo(Fifo::Duc, DUC_WORDS_PER_FRAME, Duration::from_micros(500), None) {
            return;
        }
        let (file, buf) = &mut *guard;
        let dma = buf.slice(DUC_TRANSFER_SIZE);
        // I and Q swapped: each 6-byte sample as bytes 3,4,5,0,1,2.
        for (out, s) in dma.chunks_exact_mut(6).zip(p[4..].chunks_exact(6)) {
            out[..3].copy_from_slice(&s[3..6]);
            out[3..].copy_from_slice(&s[..3]);
        }
        if let Err(e) = file.write_all_at(dma, ADDR_DUC_STREAM_WRITE) {
            eprintln!("saturn: TX IQ DMA failed: {e}");
        }
    }
}

impl Drop for SaturnLink {
    fn drop(&mut self) {
        if !self.stop.load(Ordering::Relaxed) {
            self.close();
        }
    }
}

fn post(to_host: &SyncSender<(u16, Vec<u8>)>, port: u16, packet: Vec<u8>) {
    match to_host.try_send((port, packet)) {
        Ok(()) | Err(TrySendError::Full(_)) => {}
        Err(TrySendError::Disconnected(_)) => {}
    }
}

/// Reads the DDC stream by DMA and splits it into each DDC's Protocol 2
/// IQ packets (saturn_rx_thread). The stream is 64-bit words: a header
/// word (top byte 0x80, the low 32 bits each DDC's rate) and then, DDC by
/// DDC, one word per IQ sample, 48 bits of sample and 16 unused.
fn rx_loop(hw: Arc<Hw>, file: File, to_host: SyncSender<(u16, Vec<u8>)>, running: Arc<AtomicBool>, stop: Arc<AtomicBool>) {
    hw.set_ddc_enabled(false);
    thread::sleep(Duration::from_millis(1));
    hw.setup_fifo_monitor(Fifo::Ddc);
    hw.reset_fifo(Fifo::Ddc);
    hw.gpio_bit(DATA_ENDIAN_BIT, true);
    hw.set_ddc_enabled(true);

    let mut dma = DmaBuf::new(32768);
    let mut stream: Vec<u8> = Vec::with_capacity(131_072);
    let mut header_found = false;
    let mut counts = [0u32; NUM_DDC];
    let mut frame_words = 0u32;
    let mut previous_rate_word = None;
    let mut iq: Vec<Vec<u8>> = vec![Vec::with_capacity(16384); NUM_DDC];
    let mut sequence = [0u32; NUM_DDC];
    let mut transfer = 4096usize;
    // HPSDR_SATURN_DEBUG's figures since the last report.
    let mut report_at = Instant::now() + DEBUG_EVERY;
    let (mut dma_bytes, mut frames, mut resyncs) = (0usize, 0u32, 0u32);
    let mut packets = [0u32; NUM_DDC];
    if debug() {
        eprintln!("saturn debug: receive stream enabled");
    }

    while !stop.load(Ordering::Relaxed) {
        if debug() && Instant::now() >= report_at {
            report_at = Instant::now() + DEBUG_EVERY;
            let sent: Vec<String> =
                packets.iter().enumerate().filter(|(_, &n)| n > 0).map(|(d, n)| format!("DDC{d} {n}")).collect();
            eprintln!(
                "saturn debug: last {}s: {} kB by DMA, {frames} frames, {resyncs} resyncs, FIFO {} words, \
                 rate word {:#010x}, {}, packets sent: {}",
                DEBUG_EVERY.as_secs(),
                dma_bytes / 1024,
                hw.fifo_level(Fifo::Ddc),
                previous_rate_word.unwrap_or(0),
                if running.load(Ordering::Relaxed) { "running" } else { "not running" },
                if sent.is_empty() { "none".to_string() } else { sent.join(", ") }
            );
            (dma_bytes, frames, resyncs) = (0, 0, 0);
            packets = [0; NUM_DDC];
        }
        // Whole packets out while there's enough of a DDC's samples.
        let run = running.load(Ordering::Relaxed);
        for ddc in 0..NUM_DDC {
            while iq[ddc].len() >= IQ_BYTES_PER_FRAME {
                if run {
                    let mut packet = Vec::with_capacity(DDC_PACKET_SIZE);
                    packet.extend_from_slice(&sequence[ddc].to_be_bytes());
                    packet.extend_from_slice(&[0; 8]); // timestamp
                    packet.extend_from_slice(&24u16.to_be_bytes()); // bits per sample
                    packet.extend_from_slice(&(IQ_SAMPLES_PER_FRAME as u16).to_be_bytes());
                    packet.extend_from_slice(&iq[ddc][..IQ_BYTES_PER_FRAME]);
                    sequence[ddc] = sequence[ddc].wrapping_add(1);
                    packets[ddc] += 1;
                    post(&to_host, PORT_DDC0_IQ_FROM + ddc as u16, packet);
                } else {
                    sequence[ddc] = 0;
                }
                iq[ddc].drain(..IQ_BYTES_PER_FRAME);
            }
        }

        // More from the FPGA when there's a transfer's worth.
        let words_needed = (transfer / 8) as u32;
        let mut waiting_since = Instant::now();
        while hw.fifo_level(Fifo::Ddc) < words_needed && !stop.load(Ordering::Relaxed) {
            if debug() && waiting_since.elapsed() >= DEBUG_EVERY {
                waiting_since = Instant::now();
                eprintln!(
                    "saturn debug: no receive data for {}s (FIFO {} words, want {words_needed})",
                    DEBUG_EVERY.as_secs(),
                    hw.fifo_level(Fifo::Ddc)
                );
            }
            thread::sleep(Duration::from_micros(500));
        }
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let depth = hw.fifo_level(Fifo::Ddc);
        transfer = if depth > 4096 {
            32768
        } else if depth > 2048 {
            16384
        } else if depth > 1024 {
            8192
        } else {
            4096
        };
        let chunk = dma.slice(transfer);
        if let Err(e) = file.read_exact_at(chunk, ADDR_DDC_STREAM_READ) {
            eprintln!("saturn: DDC DMA failed: {e}");
            thread::sleep(Duration::from_millis(10));
            continue;
        }
        stream.extend_from_slice(chunk);
        dma_bytes += transfer;

        // The first header may not be the first word.
        if !header_found {
            match (16..stream.len()).step_by(8).find(|&i| i + 7 < stream.len() && stream[i + 7] == 0x80) {
                Some(i) => {
                    if debug() {
                        eprintln!("saturn debug: stream header found {i} bytes in");
                    }
                    stream.drain(..i);
                    header_found = true;
                }
                None => {
                    stream.clear();
                    continue;
                }
            }
        }

        let mut pos = 0;
        while stream.len() - pos >= 16 {
            if stream[pos + 7] != 0x80 {
                // Lost the frame: start again from the next header.
                eprintln!("saturn: DDC stream out of step, finding the next header");
                header_found = false;
                resyncs += 1;
                pos = stream.len();
                break;
            }
            let rate_word = u32::from_ne_bytes([stream[pos], stream[pos + 1], stream[pos + 2], stream[pos + 3]]);
            if previous_rate_word != Some(rate_word) {
                frame_words = analyse_ddc_header(rate_word, &mut counts);
                previous_rate_word = Some(rate_word);
            }
            let frame_bytes = (frame_words as usize + 1) * 8;
            if stream.len() - pos < frame_bytes {
                break;
            }
            let mut src = pos + 8;
            for ddc in 0..NUM_DDC {
                for _ in 0..counts[ddc] {
                    iq[ddc].extend_from_slice(&stream[src..src + 6]);
                    src += 8;
                }
            }
            pos += frame_bytes;
            frames += 1;
        }
        stream.drain(..pos.min(stream.len()));
    }
}

/// Microphone samples by DMA, 64 at a time, as Protocol 2 mic packets
/// (saturn_micaudio_thread).
fn mic_loop(hw: Arc<Hw>, file: File, to_host: SyncSender<(u16, Vec<u8>)>, running: Arc<AtomicBool>, stop: Arc<AtomicBool>) {
    hw.setup_fifo_monitor(Fifo::Mic);
    hw.reset_fifo(Fifo::Mic);
    let mut dma = DmaBuf::new(4096);
    let mut sequence = 0u32;
    while !stop.load(Ordering::Relaxed) {
        // Four samples a 64-bit location.
        if !hw.wait_fifo(Fifo::Mic, MIC_SAMPLES_PER_FRAME / 4, Duration::from_millis(1), Some(&stop)) {
            break;
        }
        let chunk = dma.slice(MIC_TRANSFER_SIZE);
        if let Err(e) = file.read_exact_at(chunk, ADDR_MIC_STREAM_READ) {
            eprintln!("saturn: mic DMA failed: {e}");
            thread::sleep(Duration::from_millis(10));
            continue;
        }
        if !running.load(Ordering::Relaxed) {
            sequence = 0;
            continue;
        }
        if debug() && sequence == 0 {
            eprintln!("saturn debug: microphone samples arriving");
        }
        let mut packet = Vec::with_capacity(MIC_PACKET_SIZE);
        packet.extend_from_slice(&sequence.to_be_bytes());
        packet.extend_from_slice(chunk);
        sequence = sequence.wrapping_add(1);
        post(&to_host, PORT_MIC_FROM, packet);
    }
}

/// Protocol 2's status ("high priority from the radio") packets
/// (saturn_high_priority_thread): every 200 ms receiving, every 1 ms
/// transmitting, at once when PTT or a key changes, and within 50 ms of
/// an ADC overload.
fn status_loop(hw: Arc<Hw>, to_host: SyncSender<(u16, Vec<u8>)>, running: Arc<AtomicBool>, mox: Arc<AtomicBool>, stop: Arc<AtomicBool>) {
    let mut sequence = 0u32;
    let mut overflows = 0u8;
    while !stop.load(Ordering::Relaxed) {
        if !running.load(Ordering::Relaxed) {
            sequence = 0;
            thread::sleep(Duration::from_millis(10));
            continue;
        }
        let status = hw.read(ADDR_STATUS);
        let ptt = Hw::ptt_key_bits(status);
        if debug() && sequence == 0 {
            eprintln!(
                "saturn debug: first status: register {status:#010x}, PLL {}, PTT/keys {:#04x}",
                if ptt & 16 != 0 { "locked" } else { "NOT locked" },
                ptt & 7
            );
        }
        overflows |= (hw.read(ADDR_ADC_OVERFLOW) & 3) as u8;
        let mut p = vec![0u8; STATUS_PACKET_SIZE];
        p[0..4].copy_from_slice(&sequence.to_be_bytes());
        sequence = sequence.wrapping_add(1);
        p[4] = ptt;
        p[5] = overflows;
        overflows = 0;
        p[59] = Hw::user_io_bits(status);
        let mut analogue = |select: u32, at: usize| {
            let v = (hw.read(ADDR_ALEX_ADC_BASE + 4 * select) & 0xFFFF) as u16;
            p[at..at + 2].copy_from_slice(&v.to_be_bytes());
        };
        analogue(4, 6); // exciter power
        analogue(0, 14); // forward power
        analogue(1, 22); // reverse power
        analogue(5, 49); // supply voltage
        analogue(2, 57); // user ADC0
        analogue(3, 55); // user ADC1
        post(&to_host, PORT_STATUS_FROM, p);

        // Until the next one: in 0.5 ms steps.
        let mut steps: u32 = if mox.load(Ordering::Relaxed) { 2 } else { 400 };
        while steps > 0 && !stop.load(Ordering::Relaxed) {
            steps -= 1;
            let status = hw.read(ADDR_STATUS);
            if Hw::ptt_key_bits(status) != ptt {
                break;
            }
            overflows |= (hw.read(ADDR_ADC_OVERFLOW) & 3) as u8;
            if overflows != 0 && steps > 100 {
                steps = 100;
            }
            if mox.load(Ordering::Relaxed) && steps > 1 {
                steps = 1;
            }
            thread::sleep(Duration::from_micros(500));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ddc_header_counts_samples_and_pairs() {
        let mut counts = [0; NUM_DDC];
        // DDC0 48 kHz (1), DDC1 off, DDC2 192 kHz (3).
        let total = analyse_ddc_header(1 | (3 << 6), &mut counts);
        assert_eq!((counts[0], counts[1], counts[2], total), (1, 0, 4, 5));
        // DDC0/1 interleaved at 192 kHz: DDC0 gets both, DDC1 none.
        let total = analyse_ddc_header(7 | (3 << 3), &mut counts);
        assert_eq!((counts[0], counts[1], total), (8, 0, 8));
    }

    #[test]
    fn drive_tables_match_the_c() {
        let (current, atten) = drive_tables();
        assert_eq!((current[0], atten[0]), (0, 63));
        assert_eq!((current[255], atten[255]), (255, 0));
        // From saturnregisters.c's own table: level 128 is step 11, 241.
        assert_eq!((atten[128], current[128]), (11, 241));
        assert_eq!((atten[81], current[81]), (19, 241));
    }

    #[test]
    fn status_bits() {
        // PTT, dot, PLL locked; IO8 high (inactive).
        let status = 1 | (1 << STATUS_KEY_IN_A) | (1 << STATUS_PLL_LOCKED) | (1 << STATUS_USER_IO8);
        assert_eq!(Hw::ptt_key_bits(status), 1 | 2 | 16);
        // IO8 low counts as dash.
        assert_eq!(Hw::ptt_key_bits(0) & 4, 4);
    }
}
