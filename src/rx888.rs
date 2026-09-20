/*
    USB I/O + digital down-conversion for the RX-888 Mk2 -- a receive-
    only, Cypress FX3-based direct-sampling HF SDR (LTC2208-class ADC,
    no on-board DDC/tuner in the mode this module uses). Unlike every
    other board this project talks to (openHPSDR Protocol 1/2 over UDP,
    or Ozy's own P1-over-USB -- see ozy.rs), the RX-888 has no framing
    and no complex IQ at all: it streams the ENTIRE captured Nyquist
    band as raw signed 16-bit real ADC samples over a USB3 bulk
    endpoint, with no on-board mixer/decimator. This module is what
    turns that firehose into the same `IqSample` stream every other
    board already feeds into `RadioSession::iq_buffers` -- an NCO
    (numerically controlled oscillator) mixer to shift the wanted RF
    frequency to baseband, followed by a CIC decimator to bring the
    rate down to something WDSP can consume (WDSP does its own further
    decimation from there down to 48kHz, exactly as it already does for
    every other board -- see radio.rs's start_rx888_usb doc comment).

    Firmware: `SDDC_FX3.img` is BUNDLED (assets/rx888/, see
    default_firmware_path below) -- unlike this module's own earlier doc
    comments/README text claimed, this turned out to be fine to
    redistribute. Verified directly (not assumed) by finding that a
    second, independent RX-888 Rust implementation
    (~/github/sdroxide's sdroxide-rx888 crate) bundles the IDENTICAL
    file (byte-for-byte, same SHA-256) under its own
    crates/sdroxide-rx888/firmware/, with its own LICENSE.txt (MIT,
    (c) 2017-2020 Oscar Steila IK1XPV and the rx888-firmware
    contributors) and PROVENANCE.md citing the real upstream
    (github.com/ringof/rx888-firmware v0.1.0) -- both copied verbatim
    into assets/rx888/ alongside the image itself. An explicit path
    chosen via the Discover window's "RX-888 USB setup" still always
    overrides this default, same as Ozy's own bundled-firmware fallback.

    USB protocol constants and the firmware bring-up sequence below are
    ported from the user's own local reference checkout
    (~/github/ka9q-radio/rx888.c, rx888.h, ezusb.c -- a real, working
    Linux driver for this exact device), not guessed or reconstructed
    from general Cypress FX3 documentation:
    - VID/PID and the two-phase "find unloaded -> load firmware -> find
      loaded" bring-up: rx888.c's rx888_usb_init.
    - The FX3 RAM-image (.img) format `load_fx3_image` below parses:
      ezusb.c's fx3_load_ram (itself citing Cypress AN76405/"docID
      41351"). NOT the same format as Ozy's Intel-HEX .hex file (see
      ozy.rs's load_firmware) -- FX3 uses its own binary record format
      with a running checksum, ported byte-for-byte.
    - Vendor control-transfer conventions (`command_send`/
      `argument_send` below): ezusb.c lines 847-884, confirmed exact
      (bRequest/wValue/wIndex/payload split for each).
    - Streaming/gain/dither bring-up order (`initialise` below):
      rx888.c's rx888_set_samprate / rx888_set_dither_and_randomizer /
      rx888_set_att / rx888_start_rx, in that same order. Dither and the
      output randomizer default OFF, matching ka9q-radio's own
      `config_getboolean(..., "dither"/"rand", false)` default -- NOT
      "on by default" as this feature's own early planning notes
      assumed before this file was written against the real source.
    - Si5351 clock-generator register math is intentionally NOT ported
      here: rx888_set_samprate only falls through to manual Si5351 PLL
      programming for a non-default reference clock or a calibration
      offset. For the default 27MHz reference (this module's only
      supported case for now -- see DEFAULT_SAMPLE_RATE_HZ), it just
      sends STARTADC with the desired rate and the FX3 firmware itself
      programs the Si5351 -- a real simplification confirmed by reading
      that function, not an omission.

    UNTESTED end-to-end: this development environment has no USB
    access at all. Every constant/sequence below is ported from
    ka9q-radio's working source, but the USB transport plumbing itself
    has only ever been `cargo build`'d, never run against a real
    RX-888. The NCO/CIC math is standard, well-understood DSP (not
    protocol-guesswork), but its exact gain scaling and tuning-offset
    convention are flagged in the feature's own plan as the two things
    most likely to need real-hardware iteration.
*/

use nusb::transfer::{Bulk, ControlIn, ControlOut, ControlType, In, Recipient};
use nusb::{Interface, MaybeFuture, Speed};
use std::io::{self, Read};
use std::path::Path;
use std::time::Duration;

pub const VID: u16 = 0x04b4;
const PID_UNLOADED: u16 = 0x00f3;
const PID_STREAMING: u16 = 0x00f1;

const IO_TIMEOUT: Duration = Duration::from_millis(1000);
// Bulk IN endpoint 1 (rx888.c: `unsigned int ep = 1 | LIBUSB_ENDPOINT_IN`).
const EP_STREAM_IN: u8 = 0x81;
// One read's worth of raw i16 samples.
//
// REVISED (2026-09-19, after real hardware testing) down from an
// initial 1<<20 (1 MiB). Not load-bearing for CORRECTNESS -- any read
// size works, this project's own receiver loop and Ddc::process_block
// handle a block of any length identically -- but it turned out very
// much load-bearing for real-time SMOOTHNESS: at 1 MiB, one read
// covers ~7.7ms of real time at this device's ~130MB/s data rate, so
// this module delivered roughly 777 decimated IQ samples in one single
// burst every ~7.7ms, then nothing until the next read completed --
// correct in total, but bursty rather than a steady trickle. A real
// report: live local audio playback and TCI streaming to WSJT-X (both
// genuinely real-time-paced consumers) were audibly/visibly affected
// (choppy audio; WSJT-X's own waterfall showing signals stretched far
// wider than they should be, consistent with its internal timing
// assumptions being fed data in uneven bursts) while a plain file
// recording of the SAME session was completely clean -- exactly the
// signature of a data-PACING problem, not a data-CORRECTNESS one (a
// recorder has no real-time deadline to miss, only genuinely real-time
// consumers do). 1<<16 (64 KiB) covers ~0.5ms of real time instead,
// delivering ~48 decimated samples roughly every half-millisecond --
// 16x smoother, at negligible extra per-read overhead (this module's
// own receiver loop and Ddc::process_block were already benchmarked
// with over 3x real-time CPU headroom -- see process_block's own doc
// comment -- so processing shorter blocks far more often costs nothing
// meaningful there).
pub const STREAM_READ_SIZE: usize = 1 << 16;

// FX3Command (rx888.h) -- only the subset direct-sampling HF mode needs.
const RW_INTERNAL: u8 = 0xA0; // Cypress-defined, used only during firmware RAM load/jump (ezusb.c)
const STARTFX3: u8 = 0xAA;
const STOPFX3: u8 = 0xAB;
const GPIOFX3: u8 = 0xAD;
const STARTADC: u8 = 0xB2;
const TUNERSTDBY: u8 = 0xB8; // parks the (unused, direct-sampling mode) R820T2 tuner
const SETARGFX3: u8 = 0xB6;

// ArgumentList (rx888.h) -- DAT31_ATT is the step attenuator used in
// direct-sampling mode (0-63, 0.5dB/step -- rx888_set_att's own `att*2`).
const DAT31_ATT: u16 = 10;
// AD8340 VGA -- a SEPARATE, REQUIRED analog gain stage ahead of the
// ADC, distinct from the DAT31 step attenuator above. ka9q-radio's own
// rx888_setup ALWAYS programs this (default "gainmode high" + 1.5dB),
// never leaving it at the chip's own power-on-reset state -- a real
// hardware report (2026-09-19: RX-888 producing an almost flat,
// featureless noise floor on a real antenna where a HermesLite2 on the
// same antenna showed normal FT8 activity) traced back to this module
// never having programmed it at all, leaving the VGA at whatever it
// happens to power up as (evidently far too low to be usable). See
// `vga_gain_arg` below for the exact formula, ported from rx888.c's
// `gain2val`/`val2gain` (lines 847-864) rather than guessed.
const AD8340_VGA: u16 = 11;
// rx888.c's own Vernier/Pregain constants (lines 844-845) -- part of
// the AD8340's real analog calibration, not tunable/derivable, ported
// as literal values.
const VGA_VERNIER: f64 = 0.055744;
const VGA_PREGAIN: f64 = 7.079458;

// GPIOPin bits (rx888.h: OUTXIO6/OUTXIO7).
const GPIO_DITH: u32 = 1 << 6;
const GPIO_RANDO: u32 = 1 << 7;

/// Default ADC sample rate (rx888.h's Default_samprate) -- the only
/// rate this module supports for now (see this module's own doc
/// comment on why Si5351 reprogramming for a different rate isn't
/// ported yet). Fixed at this one rate means `initialise` can check
/// the negotiated USB link speed against a single known requirement
/// (see REQUIRED_BYTES_PER_SEC) rather than a dynamic one -- a real
/// USB2 cable/port produced exactly the confusing failure this guards
/// against: firmware loads, the ADC clocks, samples simply go missing
/// or corrupted, and it looks like a broken receiver rather than an
/// insufficient link.
pub const DEFAULT_SAMPLE_RATE_HZ: u32 = 64_800_000;

/// CIC decimation ratio and stage count for the NCO+CIC DDC below.
///
/// REVISED TWICE (2026-09-19, after real hardware testing), both times
/// chasing the same real WDSP crash on session teardown -- `gdb`
/// confirmed a heap "double free or corruption" inside `destroy_bpsnba`
/// (vendor/wdsp/snb.c), reached from `CloseChannel` -> `destroy_rxa`:
///
/// 1. First: DECIMATION=256 gave an exact-power-of-two output rate
///    (253,125 Hz), suspected as "not an integer multiple of WDSP's
///    48kHz DSP_RATE". Changed to DECIMATION=270 (240,000 Hz = 48,000*5)
///    -- crashed identically. That fix was wrong; the real requirement
///    is narrower than "any 48kHz multiple" (see #2). A separate fix
///    attempt in between -- extending wdsp_sys::SETUP_LOCK to also cover
///    CloseChannel/DestroyAnalyzer teardown, on the theory this was a
///    cross-thread WDSP-teardown race -- also did NOT fix it, and a
///    control test confirmed a real HL2 does NOT crash under the same
///    repeated-Stop test, ruling out "general pre-existing bug" too.
///    (The SETUP_LOCK teardown fix is real and worth keeping regardless
///    -- it closes a genuine gap relative to the already-documented
///    OpenChannel race -- it just isn't what was crashing here.)
/// 2. Actual root cause, found by reading vendor/wdsp/reshb.c directly:
///    `calc_HBResampler`'s cascaded half-band decimator (channel input
///    rate -> WDSP's internal 48kHz) is a hardcoded `switch` over EXACT
///    recognized `(inrate,outrate)` pairs -- 96000/192000/384000/768000/
///    1536000/3072000/6144000 -> a lower rate, matching precisely the
///    UI's own Sample Rate button values (and their x2/x4 wideband-
///    display multiples). ANY unrecognized rate falls through to
///    `default: run=0, nStages=0, nBuffs=0` -- silently DISABLING the
///    resampler instead of erroring, leaving it in bypass/memcpy mode.
///    240,000 Hz (from fix #1, a real 48kHz multiple but NOT one of
///    these specific table entries) hit exactly that default case: WDSP
///    believed it was receiving 48kHz-paced samples while this DDC
///    actually delivered them 5x faster, a throughput mismatch that
///    plausibly overran a buffer sized for the wrong rate elsewhere,
///    with the corruption only detected later at teardown. Fixed by
///    using DECIMATION=675, landing EXACTLY on 96,000 Hz -- one of the
///    table's own recognized entries (96000 -> 48000: nStages=1,
///    taps[0]=319, nBuffs=0), the same value real P1/P2 boards already
///    use safely today, not just "a multiple" of it.
///
/// 675 is not a power of two, so the CIC's gain compensation (see
/// `Ddc::new`'s `gain_comp`) is a floating-point multiply -- applied
/// ONCE to each already-bounded decimated output sample, never
/// accumulated, so none of the catastrophic-cancellation risk a running
/// float ACCUMULATOR had (see CicStage's own doc comment for that
/// separate, already-fixed bug) applies here.
///
/// 5 stages: R^N * i16::MAX (worst-case per-decimation-window magnitude
/// the i64 wrapping comb arithmetic must represent exactly) is
/// 675^5 * 32767 ~= 4.6e18, comfortably under i64::MAX (~9.2e18) with
/// about 2x margin -- checked explicitly since 675 is a much larger
/// ratio than the original 256.
pub const CIC_DECIMATION: u32 = 675;
pub const CIC_STAGES: usize = 5;
/// Exact (64_800_000 / 675 = 96,000) -- the rate reported to WDSP via
/// RadioSession::sample_rate / SpectrumHandle::start. See
/// CIC_DECIMATION's own doc comment for why this MUST be one of WDSP's
/// own specifically-recognized rates (96000 here), not merely "a
/// multiple of 48kHz" -- a real, confirmed WDSP crash otherwise, not
/// just a nicety.
pub const OUTPUT_SAMPLE_RATE_HZ: u32 = DEFAULT_SAMPLE_RATE_HZ / CIC_DECIMATION;

/// (ADC sample rate, CIC decimation) for a WDSP-recognized DDC output
/// rate -- `None` for anything else. ADDED (2026-09-20, real request:
/// "are there any other sample rates we can run the DDC at" /
/// "implement that") -- until now this module only ever ran at the one
/// fixed 96,000 Hz output (DEFAULT_SAMPLE_RATE_HZ / CIC_DECIMATION).
///
/// Both the output rate AND the decimation must be exact: WDSP's own
/// `calc_HBResampler` (vendor/wdsp/reshb.c) is a hardcoded switch over
/// specific `(inrate,outrate)` pairs -- see CIC_DECIMATION's own doc
/// comment for the real crash an unrecognized/mismatched rate caused
/// earlier this session. Only 96000, 192000, and 384000 are offered
/// here (out of WDSP's full recognized set, which also includes 48000,
/// 768000, 1536000, 3072000, 6144000):
/// - 48000 is deliberately EXCLUDED despite landing exactly (decimation
///   1350 on the same 64.8MHz clock as the 96000 preset): CicStage's
///   i64 wrapping arithmetic needs `decimation^CIC_STAGES * i16::MAX`
///   to fit i64::MAX for the wraparound-cancellation trick in the comb
///   stage to stay numerically exact (see CicStage's own doc comment).
///   96000's own decimation (675) already uses about half of i64's
///   range (675^5 * 32767 ~= 4.6e18 of ~9.2e18) -- 1350^5 is 2^5 = 32x
///   that, comfortably OVER i64::MAX, which would silently corrupt
///   output rather than just alias/attenuate it. 192000 and 384000 use
///   SMALLER decimation than 675 (a wider output rate needs less
///   decimation from the same ADC rate), so they're safer on this
///   front, not riskier.
/// - 768000 and above aren't offered in this v1 pass either: hitting
///   them exactly needs a third ADC-clock family again (this module's
///   two, 64.8MHz and 64.896MHz, don't divide evenly into either), and
///   at that width the instantaneous bandwidth is already well beyond
///   what a single receiver's own passband filter/audio chain is
///   useful for -- a v2 addition if actually wanted, not attempted
///   speculatively here.
///
/// TWO ADC clock "families" are used because no single ADC rate divides
/// evenly into every WDSP-recognized rate (64,800,000 only divides
/// evenly by 675, landing on 96000 -- see DEFAULT_SAMPLE_RATE_HZ's own
/// doc comment): 64,800,000 Hz (this module's original, most-tested
/// rate) covers 96000 via decimation 675 unchanged; 64,896,000 Hz (a
/// +0.148% nudge, well within a real crystal's own tolerance, and only
/// ever sent to the hardware if the user actually asks for one of these
/// wider rates) covers both 192000 (decimation 338) and 384000
/// (decimation 169) exactly. See `initialise`'s own doc comment for why
/// reprogramming the ADC to 64,896,000 needs no new protocol work: the
/// FX3 firmware's own internal Si5351 PLL math handles any requested
/// rate for this module's always-default-reference case, the same
/// STARTADC command already used for 64,800,000.
pub fn ddc_params_for_output_rate(output_rate_hz: u32) -> Option<(u32, u32)> {
    match output_rate_hz {
        96_000 => Some((DEFAULT_SAMPLE_RATE_HZ, CIC_DECIMATION)),
        192_000 => Some((64_896_000, 338)),
        384_000 => Some((64_896_000, 169)),
        _ => None,
    }
}

fn io_err(e: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::Other, e.to_string())
}

/// Undoes the LTC2208's own output randomizer -- REQUIRED whenever
/// GPIO_RANDO is enabled (see `initialise`'s own doc comment on why
/// dither/randomizer are both on). The ADC scrambles its own parallel
/// output bus for EMI reasons: every bit except the LSB is XORed with
/// the LSB, which is left alone to say whether the rest were inverted.
/// The operation is its own inverse, so this both applies and removes
/// it.
///
/// ROOT CAUSE FIX for a real report: enabling the randomizer GPIO bit
/// without this step corrupts roughly HALF of all samples (every one
/// with the LSB set has its upper 15 bits inverted) -- not a subtle
/// error, full-scale noise on half the stream, compounding every other
/// symptom this feature has chased. Found and ported from a second,
/// independent real-world RX-888 Rust implementation
/// (~/github/sdroxide's sdroxide-rx888 crate, whose own `convert.rs`
/// documents this exact hazard almost word for word) after this
/// project's own dither/randomizer change made things worse rather than
/// better -- confirms the earlier good instinct (dither/randomizer
/// address a real undithered-ADC spur problem) was right, but was
/// missing this required host-side half.
///
/// Written branchlessly (matching sdroxide's own approach, verified
/// against the LTC2208 datasheet's own description independently via
/// this module's own tests below): bit 0 is broadcast to a full-width
/// mask, with bit 0 itself cleared from that mask so the flag survives
/// the XOR untouched.
#[inline]
pub fn derandomize(sample: i16) -> i16 {
    sample ^ ((sample & 1).wrapping_neg() & !1i16)
}

/// Bundled `SDDC_FX3.img` location -- same "installed .deb / portable /
/// `cargo run` from repo root / built-on-this-checkout" candidate list
/// as ozy.rs's own (private, so duplicated rather than shared -- this
/// project's existing per-module convention) `bundled_path`. Only used
/// as a fallback when the user hasn't explicitly picked a file via the
/// Discover window's "RX-888 USB setup" -- an explicit choice there
/// always wins, so a custom/updated firmware build still works.
fn bundled_path(filename: &str) -> Option<std::path::PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            // /usr/bin/hpsdr-rs -> /usr/share/hpsdr-rs/rx888/<file>
            if let Some(prefix) = exe_dir.parent() {
                candidates.push(prefix.join("share/hpsdr-rs/rx888").join(filename));
            }
            // Portable/non-packaged layout: files dropped next to the exe.
            candidates.push(exe_dir.join("rx888").join(filename));
        }
    }
    candidates.push(std::path::PathBuf::from("assets/rx888").join(filename));
    candidates.push(std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets/rx888").join(filename));
    candidates.into_iter().find(|p| p.is_file())
}

pub fn default_firmware_path() -> Option<std::path::PathBuf> {
    bundled_path("SDDC_FX3.img")
}

/// Lightweight "is an RX-888 plugged in" probe for the discovery list --
/// matches either PID (unloaded or already streaming from a prior
/// session -- FX3 RAM firmware persists until power-cycle, so a
/// reconnect without unplugging often finds it already loaded).
pub fn discover() -> bool {
    match nusb::list_devices().wait() {
        Ok(devices) => {
            devices.into_iter().any(|d| d.vendor_id() == VID && (d.product_id() == PID_UNLOADED || d.product_id() == PID_STREAMING))
        }
        Err(_) => false,
    }
}

/// A short, human-readable warning if a currently-plugged-in RX-888's
/// USB link SEEMS too slow to sustain this module's fixed data rate --
/// `None` if no device is present, its link speed can't be determined,
/// or it looks fast enough. Safe to call from the Discover window's own
/// background discovery pass alongside every other board's probe --
/// `discover()` only runs once per window-open/rescan click, not on a
/// continuous timer, so the occasional extra second this can now cost
/// (see below) is a one-off, not a repeated tax.
///
/// REVISED (2026-09-20, after a real report -- "now it always says it
/// is available whether USB3 or USB2 connected", i.e. the warning
/// stopped ever firing in the common case): earlier versions of this
/// function only ever queried `DeviceInfo` passively, without opening
/// or claiming anything, and had to guess which PID to trust -- the
/// bootloader/unloaded PID's own negotiated speed is NOT reliably the
/// same thing as the real firmware's (a genuine USB3 cable was once
/// seen reported as "too slow" when only the unloaded PID was checked,
/// then restricting the check to only the streaming PID fixed that but
/// broke the overwhelmingly common case -- discovery normally runs
/// BEFORE the device has ever been connected this session, i.e. while
/// it's still sitting at the unloaded PID). Rather than keep guessing
/// between two unreliable proxies, this now does what the user
/// suggested: actually brings the device up to the streaming PID first
/// (`load_firmware_if_needed`, best-effort -- silently does nothing if
/// `firmware_path` is `None` or the load fails, e.g. no path configured
/// yet), then reads the SAME real, reliably-negotiated speed
/// `initialise`'s own check uses. `firmware_path` is `Option` rather
/// than required precisely so a first-run user with no path configured
/// yet still gets a (less certain) best-effort hint instead of nothing.
///
/// STILL DELIBERATELY A HINT, NOT AUTHORITATIVE, even now: a firmware
/// load can fail for reasons that have nothing to do with link speed
/// (wrong/missing file, a device that only ever answers at the unloaded
/// PID for some other reason), in which case this falls back to the
/// old, less certain "whatever PID happens to be present" read rather
/// than returning `None` outright -- still better than silence, still
/// not something to gate a row's availability on (see discovery_ui.rs's
/// own device_available, which does NOT check this field, for why: a
/// false positive here would otherwise BLOCK a working device from
/// connecting at all via the UI). `initialise`'s own identical check at
/// real connect time remains the true safety net regardless.
pub fn link_speed_warning(firmware_path: Option<&Path>) -> Option<&'static str> {
    if let Some(path) = firmware_path {
        let _ = load_firmware_if_needed(path);
    }
    let speed = nusb::list_devices()
        .wait()
        .ok()?
        .into_iter()
        .find(|d| d.vendor_id() == VID && (d.product_id() == PID_UNLOADED || d.product_id() == PID_STREAMING))
        .and_then(|d| d.speed())?;
    (usable_bytes_per_sec(Some(speed)) < REQUIRED_BYTES_PER_SEC).then(|| speed_name(Some(speed)))
}

fn find_and_open(pid: u16) -> io::Result<(Interface, Option<Speed>)> {
    let device_info = nusb::list_devices()
        .wait()
        .map_err(io_err)?
        .into_iter()
        .find(|d| d.vendor_id() == VID && d.product_id() == pid)
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, format!("no RX-888 device (04b4:{pid:04x}) found on USB"))
        })?;
    // Negotiated USB link speed -- captured here, from DeviceInfo,
    // before opening (nusb's own natural place for it; the property
    // belongs to the physical link/port/cable, not to anything the
    // firmware does). See DEFAULT_SAMPLE_RATE_HZ's own doc comment for
    // why `initialise` checks this against REQUIRED_BYTES_PER_SEC once
    // the streaming device is open.
    let speed = device_info.speed();
    let device = device_info.open().wait().map_err(io_err)?;
    let interface = device.claim_interface(0).wait().map_err(io_err)?;
    Ok((interface, speed))
}

/// A human-readable USB link speed, for the error message below.
fn speed_name(speed: Option<Speed>) -> &'static str {
    match speed {
        Some(Speed::Low) => "USB 1.1 low speed",
        Some(Speed::Full) => "USB 1.1 full speed",
        Some(Speed::High) => "USB 2.0 high speed",
        Some(Speed::Super) => "USB 3.0 SuperSpeed",
        Some(Speed::SuperPlus) => "USB 3.1 SuperSpeed+",
        _ => "an unknown speed",
    }
}

/// Sustained bulk throughput to plan for at a given negotiated link
/// speed, in bytes/sec -- deliberately pessimistic relative to each
/// speed's textbook peak (53MB/s for USB2 High Speed, 500MB/s+ for
/// USB3 SuperSpeed): what a real host sustains, sharing a real bus with
/// other traffic, is a good deal less, and over-promising here would
/// only turn into dropped samples rather than an honest "this link is
/// too slow" -- matching a second, independent RX-888 Rust
/// implementation's own identical reasoning and constants
/// (~/github/sdroxide's sdroxide-rx888::usb::usable_bytes_per_sec).
fn usable_bytes_per_sec(speed: Option<Speed>) -> f64 {
    match speed {
        Some(Speed::SuperPlus) => 800e6,
        Some(Speed::Super) => 380e6,
        Some(Speed::High) => 38e6,
        _ => 1e6,
    }
}

/// What this module's own fixed-rate design (see DEFAULT_SAMPLE_RATE_HZ)
/// needs the link to sustain: 64.8Msps of real i16 samples, 2 bytes
/// each, continuously.
const REQUIRED_BYTES_PER_SEC: f64 = DEFAULT_SAMPLE_RATE_HZ as f64 * 2.0;

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

/// `command_send` (ezusb.c:847) -- bRequest=cmd, wValue=wIndex=0, a
/// 4-byte little-endian payload carrying `data`.
fn command_send(interface: &Interface, cmd: u8, data: u32) -> io::Result<()> {
    control_out(interface, cmd, 0, 0, &data.to_le_bytes())
}

/// `argument_send` (ezusb.c:866) -- bRequest is ALWAYS SETARGFX3
/// (0xB6); the real payload travels in wValue (`data`, truncated to 16
/// bits -- every v1 use here is well within range) and wIndex (`arg`,
/// the ArgumentList id); the data stage itself is a single zero byte.
fn argument_send(interface: &Interface, arg: u16, data: u32) -> io::Result<()> {
    control_out(interface, SETARGFX3, data as u16, arg, &[0])
}

/// AD8340 VGA gain-in-dB -> raw `SETARGFX3`/`AD8340_VGA` argument value.
/// Ported exactly from rx888.c's `gain2val` (lines 854-864): a 7-bit
/// gain code (0-127) plus a high bit selecting the AD8340's own
/// "highgain" mode (adds a fixed `VGA_PREGAIN` multiplier ahead of the
/// per-code `VGA_VERNIER` step size). `highgain=true` matches
/// ka9q-radio's own default ("gainmode high").
fn vga_gain_arg(highgain: bool, gain_db: f64) -> u32 {
    let gain_db = gain_db.min(34.0);
    let voltage = 10f64.powf(gain_db / 20.0);
    let denom = VGA_VERNIER * (1.0 + (VGA_PREGAIN - 1.0) * if highgain { 1.0 } else { 0.0 });
    let code = (voltage / denom).round().clamp(0.0, 127.0) as u32;
    code | if highgain { 1 << 7 } else { 0 }
}

/// Cypress FX3 "RW_INTERNAL" (0xA0) RAM write/read/jump -- used ONLY
/// during firmware bring-up (`load_fx3_image`), a completely different
/// vendor-request numbering space from the FX3Command enum above
/// (which only applies once the SDDC firmware itself is running).
fn fx3_ram_write(interface: &Interface, addr: u32, data: &[u8]) -> io::Result<()> {
    control_out(interface, RW_INTERNAL, (addr & 0xFFFF) as u16, (addr >> 16) as u16, data)
}

fn fx3_ram_read(interface: &Interface, addr: u32, len: u16) -> io::Result<Vec<u8>> {
    control_in(interface, RW_INTERNAL, (addr & 0xFFFF) as u16, (addr >> 16) as u16, len)
}

fn fx3_jump(interface: &Interface, addr: u32) -> io::Result<()> {
    control_out(interface, RW_INTERNAL, (addr & 0xFFFF) as u16, (addr >> 16) as u16, &[])
}

/// Loads a Cypress FX3 RAM firmware image (`.img`, e.g. `SDDC_FX3.img`)
/// into the device -- ported from ezusb.c's `fx3_load_ram` (itself
/// citing Cypress AN76405/"docID 41351" for the file format), NOT
/// Ozy's Intel-HEX `.hex` format (see ozy.rs's `parse_hex_record`/
/// `load_firmware`) -- a completely different, binary, FX3-specific
/// record format with a running checksum:
///   4-byte "CY" signature + flags + image-type byte (0xB0 = normal FW
///   binary with checksum -- the only type this parses; 0xB1/0xB2 are
///   security/VID-PID images the reference itself doesn't support
///   either), then repeated `(length_words: u32, addr: u32, data:
///   [u32; length_words])` records (all little-endian) until a
///   zero-length record whose `addr` field is the program entry point,
///   then a final u32 checksum (sum of every u32 data word written,
///   wrapping) to verify against.
fn load_fx3_image(interface: &Interface, path: &Path) -> io::Result<()> {
    let bytes = std::fs::read(path)?;
    if bytes.len() < 4 || &bytes[0..2] != b"CY" {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "RX-888 firmware image: missing 'CY' signature"));
    }
    if bytes[3] != 0xB0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("RX-888 firmware image: unsupported image type 0x{:02x}", bytes[3]),
        ));
    }
    let mut pos = 4usize;
    let mut checksum: u32 = 0;
    let entry_addr;
    loop {
        if pos + 8 > bytes.len() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "RX-888 firmware image: truncated record header"));
        }
        let length_words = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap());
        let addr = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap());
        pos += 8;
        if length_words == 0 {
            entry_addr = addr;
            break;
        }
        let byte_len = (length_words as usize) * 4;
        if pos + byte_len > bytes.len() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "RX-888 firmware image: truncated record data"));
        }
        let section = &bytes[pos..pos + byte_len];
        for word in section.chunks_exact(4) {
            checksum = checksum.wrapping_add(u32::from_le_bytes(word.try_into().unwrap()));
        }
        pos += byte_len;

        let mut write_addr = addr;
        for chunk in section.chunks(4096) {
            fx3_ram_write(interface, write_addr, chunk)?;
            let verify = fx3_ram_read(interface, write_addr, chunk.len() as u16)?;
            if verify != chunk {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "RX-888 firmware image: RAM verify mismatch"));
            }
            write_addr += chunk.len() as u32;
        }
    }
    if pos + 4 > bytes.len() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "RX-888 firmware image: missing trailing checksum"));
    }
    let expected = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap());
    if checksum != expected {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "RX-888 firmware image: checksum mismatch"));
    }
    fx3_jump(interface, entry_addr)
}

/// Control-transfer handle -- owns the claimed `Interface`. Kept
/// separate from `RxEndpoint` for the same "exclusive owner per
/// endpoint/purpose" reasoning ozy.rs's `OzyDevice`/`RxEndpoint` split
/// documents: this is used from a small periodic control loop (see
/// radio.rs's rx888_control_loop) that only ever issues control
/// transfers, never touching the bulk streaming endpoint the receiver
/// thread owns exclusively.
pub struct Rx888Device {
    interface: Interface,
}

impl Rx888Device {
    /// Sets the DAT-31 step attenuator, 0-31.5dB in 0.5dB steps
    /// (rx888_set_att: `argument_send(DAT31_ATT, (int)(att*2))`).
    /// hpsdr-rs's existing RX attenuation setting only ever produces
    /// whole dB values (0-31), so the 0.5dB steps this hardware
    /// actually supports go unused for now -- simplest v1 mapping onto
    /// an already-existing, already-persisted UI control rather than
    /// adding a new one.
    pub fn set_attenuator_db(&self, att_db: u32) -> io::Result<()> {
        let arg = att_db.min(31) * 2;
        argument_send(&self.interface, DAT31_ATT, arg)
    }

    fn stop_streaming(&self) -> io::Result<()> {
        command_send(&self.interface, STOPFX3, 0)
    }
}

/// Exclusive owner of the bulk streaming endpoint -- moved into
/// radio.rs's rx888_receiver_loop. Same `with_read_timeout` idle-vs-
/// fatal-error handling idiom as ozy.rs's `RxEndpoint`.
pub struct RxEndpoint {
    reader: nusb::io::EndpointRead<Bulk>,
}

impl RxEndpoint {
    pub fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.reader.read(buf)
    }
}

/// If an unloaded-PID RX-888 is present, loads firmware into it and
/// waits for it to re-enumerate as the streaming PID; a no-op if no
/// unloaded device is found (either already loaded from a prior session
/// -- FX3 RAM firmware persists until power-cycle -- or not connected at
/// all). Factored out of `initialise` so `link_speed_warning` can also
/// use it, best-effort, to get an accurate real-firmware speed reading
/// at discovery time instead of guessing off the bootloader's own
/// enumeration -- see that function's own doc comment.
fn load_firmware_if_needed(firmware_path: &Path) -> io::Result<()> {
    if let Ok((interface, _)) = find_and_open(PID_UNLOADED) {
        load_fx3_image(&interface, firmware_path)?;
        drop(interface);
        // rx888.c's own "how long should this be? sleep(1)" for
        // re-enumeration -- ported as-is.
        std::thread::sleep(Duration::from_secs(1));
    }
    Ok(())
}

/// Full cold-boot bring-up: find the unloaded device (if present) and
/// load firmware into RAM, wait for it to re-enumerate as the streaming
/// PID (or find it already there, from a prior session -- FX3 RAM
/// firmware persists until power-cycle), then program the ADC sample
/// rate, dither/randomizer, and initial attenuator, exactly mirroring
/// rx888.c's own bring-up order (rx888_set_samprate ->
/// rx888_set_dither_and_randomizer -> rx888_set_att -> rx888_start_rx's
/// submit-transfers/STARTFX3/TUNERSTDBY sequence). The small per-command
/// sleeps match rx888_set_*'s own `usleep(5000)` calls, ported as-is
/// rather than removed, per this project's "match a working reference
/// exactly for unverifiable low-level protocol code" discipline.
///
/// `adc_rate_hz` -- ADDED (2026-09-20) as a real parameter rather than
/// always sending the hardcoded `DEFAULT_SAMPLE_RATE_HZ`, so a caller
/// can also (re)connect at one of `ddc_params_for_output_rate`'s other
/// supported ADC rates. Still needs NO Si5351 register math ported here
/// for any of them: this module only ever runs the RX-888 at its
/// default 27MHz reference with no calibration offset, which is exactly
/// ka9q-radio's own rx888_set_samprate fast path -- just send STARTADC
/// with the desired value and let the FX3 firmware's own internal PLL
/// math (see rx888.c's `actual_freq`, itself just a REPORTING/logging
/// helper for the tiny fractional-N quantization error, not something
/// this module needs to replicate) synthesize it. Called with
/// `DEFAULT_SAMPLE_RATE_HZ` for this module's own original, most-tested
/// preset; see ddc_params_for_output_rate for the others.
pub fn initialise(firmware_path: &Path, initial_attenuator_db: u32, adc_rate_hz: u32) -> io::Result<(Rx888Device, RxEndpoint)> {
    load_firmware_if_needed(firmware_path)?;

    let (interface, speed) = find_and_open(PID_STREAMING)?;

    // ROOT CAUSE FIX for a real report: connecting over a USB2 cable/
    // port (or a USB3 port behind a USB2-only hub) let everything ELSE
    // proceed normally -- firmware load, ADC clock, streaming all
    // "succeeded" -- while the actual sample data was silently dropped/
    // corrupted at the link layer, indistinguishable from a genuine
    // signal-quality bug and the root cause of a very long real-hardware
    // debugging session. Failing clearly here, before any of that can
    // happen, is a direct port of a second, independent RX-888 Rust
    // implementation's own identical check (~/github/sdroxide's
    // sdroxide-rx888::device::clamp_rate_to_link) -- that implementation
    // dynamically clamps its ADC rate to whatever the link can carry
    // instead of refusing outright; this module fails with a clear,
    // actionable message instead. Checked against the ACTUAL requested
    // `adc_rate_hz`, not the fixed DEFAULT_SAMPLE_RATE_HZ constant, now
    // that this can be one of ddc_params_for_output_rate's other rates
    // too (all within a fraction of a percent of each other in practice,
    // but this is the more correct check regardless).
    let usable = usable_bytes_per_sec(speed);
    let required = adc_rate_hz as f64 * 2.0;
    if usable < required {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!(
                "RX-888 is connected via {} (~{:.0}MB/s usable), which can't reliably carry this \
                 receiver's {:.1}Msps stream (~{:.0}MB/s needed). Use a USB 3 cable plugged \
                 directly into a USB 3 port (not a USB 2 hub) for the full rate.",
                speed_name(speed),
                usable / 1e6,
                adc_rate_hz as f64 / 1e6,
                required / 1e6,
            ),
        ));
    }

    command_send(&interface, STARTADC, adc_rate_hz)?;
    std::thread::sleep(Duration::from_millis(5));

    // REVISED (2026-09-19, after real hardware testing): dither AND the
    // output randomizer are now both ON, reversing this module's
    // earlier choice to match ka9q-radio's own config DEFAULT
    // (config_getboolean(..., "dither"/"rand", false)) -- that default
    // turned out to be the wrong thing to copy. A real report: a
    // rock-steady, undying tone at a fixed audio frequency (unmoving for
    // the entire length of a multi-second recording -- real voice/music
    // content never does that) plus periodic repeated structure visible
    // in the spectrum display, on hardware with dither/randomizer off.
    // This is the textbook signature of an UNDITHERED ADC's own
    // quantization-distortion spurs -- deterministic, repeating tones
    // from the quantizer's own patterns, especially pronounced on a
    // strong, simple signal (exactly this scenario: a strong local AM
    // carrier). Dithering (adding a small pseudorandom offset before
    // quantization) and the output randomizer (whitening the residual
    // quantization error's own spectral pattern) are the standard fixes
    // for exactly this -- both directly supported by this hardware's
    // own GPIO bits, just not the reference driver's own DEFAULT
    // config value for them.
    command_send(&interface, GPIOFX3, GPIO_DITH | GPIO_RANDO)?;
    std::thread::sleep(Duration::from_millis(5));

    argument_send(&interface, DAT31_ATT, initial_attenuator_db.min(31) * 2)?;
    std::thread::sleep(Duration::from_millis(5));

    // AD8340 VGA -- see AD8340_VGA's own doc comment for why this is
    // REQUIRED (a real report: omitting this entirely left the front
    // end far too quiet to show any real signal). Matches ka9q-radio's
    // own default exactly (highgain mode, +1.5dB requested).
    argument_send(&interface, AD8340_VGA, vga_gain_arg(true, 1.5))?;
    std::thread::sleep(Duration::from_millis(5));

    // Endpoint claimed here (borrowing `interface`) BEFORE `interface`
    // itself is moved into `Rx888Device` below -- same ordering ozy.rs's
    // `initialise` uses, for the same reason (an `Interface`'s own
    // `endpoint()` only borrows).
    let rx = RxEndpoint {
        reader: interface.endpoint::<Bulk, In>(EP_STREAM_IN).map_err(io_err)?.reader(STREAM_READ_SIZE).with_read_timeout(Duration::from_millis(500)),
    };

    // rx888_start_rx: submit queued transfers (this module's receiver
    // thread does the equivalent with synchronous reads instead, so
    // there's nothing to submit here), then STARTFX3, then TUNERSTDBY
    // to park the unused R820T2 tuner.
    command_send(&interface, STARTFX3, 0)?;
    std::thread::sleep(Duration::from_millis(5));
    command_send(&interface, TUNERSTDBY, 0)?;

    Ok((Rx888Device { interface }, rx))
}

pub fn stop(device: &Rx888Device) {
    let _ = device.stop_streaming();
}

/// NCO mixer + CIC decimator: turns a stream of raw real ADC samples
/// into a (much lower rate) complex `IqSample`-ready stream, tuned to
/// whatever RF frequency `set_tune_freq` was last called with.
///
/// NCO: a phase accumulator generating cos/sin per input sample, used
/// to multiply the real ADC sample by e^{-j*2*pi*f*n/Fs} -- standard
/// quadrature down-conversion of a real-sampled (no analog mixer)
/// receiver, shifting whatever's actually at RF frequency `f` (direct-
/// sampling HF mode digitizes 0..Nyquist directly, so an RF signal at
/// `f` Hz really does appear at `f` Hz in the ADC's own spectrum) down
/// to DC/baseband. This is a genuinely different, coarser operation
/// than WDSP's own SetRXAShiftFreq/SetRXAShiftRun (used elsewhere in
/// this codebase for CTUN/RIT) -- that shifts an ALREADY-narrowband,
/// already-decimated signal by at most half its own (already narrow)
/// bandwidth; this shifts the FULL wideband capture by up to its whole
/// Nyquist range, and has to happen before decimation, not after.
///
/// CIC: a standard N-stage cascaded-integrator-comb decimator (see
/// Hogenauer's original filter, well-known/well-understood structure,
/// not protocol-specific) applied independently to the mixer's I and Q
/// outputs, decimating by CIC_DECIMATION.
///
/// The integrator/comb stages use `i64` WITH WRAPPING ARITHMETIC
/// (`wrapping_add`/`wrapping_sub`), not plain integer ops and NOT
/// floating point. This matches how a real hardware CIC is built: the
/// integrators never reset and are explicitly ALLOWED to overflow,
/// because the later comb (differencing) stage exactly cancels any
/// wraparound -- differencing two wrapped values in two's-complement
/// modular arithmetic recovers the true difference as long as that true
/// difference itself fits the type's range (it does here with huge
/// margin: worst case is `decimation^stages * i16::MAX`, ~3.5e13 for
/// this module's own constants, against i64's ~9.2e18), regardless of
/// how many times the raw integrator has wrapped around.
///
/// REVISED from an initial, broken `f64` implementation
/// (2026-09-19, after real hardware testing): floating point does NOT
/// wrap, so any persistent non-zero mean in the mixed baseband signal
/// (which real ADC data always has some of -- there is no such thing as
/// a genuinely zero-mean real-world input) made the integrators grow
/// WITHOUT BOUND over time (an ideal integrator with no leakage
/// integrates a constant bias into an ever-growing ramp). That alone is
/// harmless in exact arithmetic, but in `f64` it caused catastrophic
/// cancellation in the comb stage (subtracting two huge, nearly-equal
/// floats loses precision) once the integrators grew large enough --
/// confirmed on real hardware as the S-meter/reported power climbing
/// without bound within a couple of seconds of connecting (worse with
/// more cascaded stages, which is exactly why bumping CIC_STAGES from 3
/// to 5 turned a subtle artifact into an obvious runaway). Integer
/// wraparound arithmetic has no such failure mode: it's numerically
/// exact regardless of how long the receiver runs or how large the
/// persistent bias is.
#[derive(Clone, Copy)]
struct CicStage {
    integrator: i64,
    comb_prev: i64,
}

/// How many decimated (post-rotation) samples between oscillator-vector
/// renormalizations -- see `Ddc::process_block`'s own doc comment for
/// why the recursive rotation needs this at all. 4096 input samples is
/// a tiny fraction of a second even at this module's lowest realistic
/// rate, and a single renormalize is one sqrt + 2 divides -- negligible
/// cost at this cadence, empirically verified (see this module's own
/// tests) to hold the oscillator's magnitude within ~1e-14 of exact
/// after a full second (64.8 million rotations) at the default ADC rate.
const RENORM_INTERVAL: u32 = 4096;

/// Extra headroom below the "RX-888 ADC at its own literal full-scale
/// (32767) maps to IqSample's own literal full-scale (2^23)" mapping
/// this module originally used.
///
/// ROOT CAUSE FIX for a real report (2026-09-19): AM audio was clipped
/// hard on a strong-but-ordinary local station (~40-50% of samples
/// pinned at full scale), and turning the host-side Audio Gain control
/// all the way down made it quieter WITHOUT changing the distortion
/// character at all, until it cut to silence below a threshold. That's
/// the signature of clipping happening BEFORE Audio Gain sees the
/// signal -- a downstream volume control can shrink an already-squared-
/// off waveform, it can't un-clip it. So the saturation had to be
/// happening inside WDSP's own processing (its AM envelope detector, or
/// some internal limiter/AGC stage), fed by an input that was simply
/// too hot -- not something fixable from the UI at all.
///
/// The original 1:1 full-scale mapping never actually made physical
/// sense: a REAL receiver's ADC essentially never runs anywhere near
/// its own full-scale during normal operation -- real front ends
/// deliberately keep 20-40dB or more of headroom below overload for
/// exactly the signal peaks this station represents, via their own
/// attenuator/gain staging. Assuming "RX-888 ADC at ITS full scale"
/// should map to the same absolute IqSample value as "a real board's
/// ADC at ITS full scale" ignored that every real board's ADC is
/// normally run well below that point -- so for an ordinary strong
/// signal, this module was handing WDSP a value many times hotter than
/// what a real board would ever produce for the same on-air signal,
/// saturating something internal long before the user's own Audio Gain
/// slider (a later, host-side stage) ever got a chance to help.
///
/// REVISED (2026-09-19) from an initial -20dB (0.1x) to -14dB (0.2x, a
/// modest 2x bump) after real hardware testing: with the USB transport
/// and delivery-pacing bugs this same session's testing also uncovered
/// now fixed, WSJT-X was successfully decoding real signals via TCI at
/// -20dB, but local audio playback level felt a bit quiet for
/// comfortable listening. -14dB is still a clearly conservative margin
/// below the original, confirmed-too-hot 0dB mapping (which clipped
/// hard on nothing more than an ordinary strong AM station), not a
/// full reversion -- there still isn't a precise real-hardware
/// calibration reference, just real feedback that -20dB had margin to
/// spare. Revisit again the same way (real-hardware A/B against
/// another board on the same signal) if this turns out to be too much
/// or still not enough.
const HEADROOM_FACTOR: f64 = 0.2;

pub struct Ddc {
    adc_rate_hz: f64,
    /// Current oscillator state as a unit vector (cos, sin) -- see
    /// `process_block`'s own doc comment for why this replaced a plain
    /// phase-accumulator-plus-`sin_cos()`-per-sample design.
    osc_cos: f64,
    osc_sin: f64,
    /// Fixed per-sample rotation (cos(phase_inc), sin(phase_inc)) --
    /// recomputed (the only remaining `sin`/`cos` calls in this whole
    /// module) only on retune, never in the hot per-sample path.
    rot_cos: f64,
    rot_sin: f64,
    renorm_counter: u32,
    decimation: u32,
    count: u32,
    stages_i: [CicStage; CIC_STAGES],
    stages_q: [CicStage; CIC_STAGES],
    /// Compensates the CIC's inherent `decimation^stages` passband gain
    /// AND rescales from the ADC's 16-bit range to IqSample's 24-bit-
    /// equivalent (2^23, IQ_NORM in spectrum.rs) convention other
    /// boards' real hardware ADCs already produce. A genuine `f64`
    /// multiply (CIC_DECIMATION isn't a power of two -- see its own doc
    /// comment for why), but applied ONCE to each already-bounded
    /// decimated output sample, never accumulated across samples like
    /// the integrator/comb state above -- no catastrophic-cancellation
    /// risk from that (see CicStage's doc comment for the DIFFERENT,
    /// already-fixed bug that WAS a running-accumulation hazard).
    gain_comp: f64,
}

impl Ddc {
    pub fn new(adc_rate_hz: u32, decimation: u32, stages: usize) -> Self {
        debug_assert_eq!(stages, CIC_STAGES, "Ddc's stage count is fixed at compile time via CIC_STAGES");
        // 2^23 / 2^15 (IqSample's ~24-bit-signed convention over the
        // ADC's actual 16-bit-signed range) * HEADROOM_FACTOR (see its
        // own doc comment), divided by the CIC's own decimation^stages
        // passband gain.
        let gain_comp = (8_388_608.0 / 32_768.0) * HEADROOM_FACTOR / (decimation as f64).powi(stages as i32);
        Self {
            adc_rate_hz: adc_rate_hz as f64,
            osc_cos: 1.0,
            osc_sin: 0.0,
            rot_cos: 1.0,
            rot_sin: 0.0,
            renorm_counter: 0,
            decimation,
            count: 0,
            stages_i: [CicStage { integrator: 0, comb_prev: 0 }; CIC_STAGES],
            stages_q: [CicStage { integrator: 0, comb_prev: 0 }; CIC_STAGES],
            gain_comp,
        }
    }

    /// Retunes the NCO to bring `freq_hz` down to baseband. Does NOT
    /// reset the CIC's integrator/comb state -- a brief transient is
    /// expected right after a retune (same "accept a brief mute/glitch"
    /// tradeoff this feature's own plan calls out), simpler than
    /// threading a separate reset path for v1. Does NOT reset the
    /// oscillator's own current phase either (only the ROTATION it
    /// advances by each sample) -- a retune should change frequency, not
    /// snap phase discontinuously.
    pub fn set_tune_freq(&mut self, freq_hz: f64) {
        let phase_inc = 2.0 * std::f64::consts::PI * freq_hz / self.adc_rate_hz;
        self.rot_cos = phase_inc.cos();
        self.rot_sin = phase_inc.sin();
    }

    /// Feeds one real ADC sample through the mixer and CIC. Returns
    /// `Some((i, q))`, already scaled to IqSample's convention, once
    /// every `decimation` input samples; `None` otherwise. A thin
    /// convenience wrapper over `process_block` for callers (this
    /// module's own tests) that don't need the batch API's performance
    /// -- the real receiver loop (radio.rs's rx888_receiver_loop) always
    /// uses `process_block` directly, once per USB read.
    #[cfg(test)]
    pub fn process_sample(&mut self, sample: i16) -> Option<(i32, i32)> {
        let mut out = [(0i32, 0i32); 1];
        let mut len = 0usize;
        self.process_block(&[sample], |pair| {
            out[len] = pair;
            len += 1;
        });
        (len == 1).then_some(out[0])
    }

    /// Feeds a whole block of real ADC samples through the mixer and
    /// CIC, calling `emit(i, q)` for each decimated output produced
    /// (once every `decimation` input samples). This is the real
    /// receiver loop's hot path -- see radio.rs's rx888_receiver_loop,
    /// which calls this once per ~1MB USB read (hundreds of thousands
    /// of samples) rather than once per sample.
    ///
    /// PERFORMANCE, not just style: an earlier version of this DDC
    /// processed one sample at a time via `&mut self` field access
    /// (both a `phase.sin_cos()` call AND a struct-field-per-access
    /// design). Benchmarked directly against 1 real second's worth of
    /// input (64.8 million samples at this module's default ADC rate):
    /// the original design took 2.11s of CPU time to process 1s of
    /// audio -- literally could NOT keep up in real time, confirmed as
    /// the root cause of a real report (stale/backlogged spectrum
    /// display, missing discrete signals, corrupted audio -- all
    /// symptoms of samples backing up and the OS-level USB buffer
    /// eventually dropping data while this thread was still catching
    /// up). Two changes, both benchmarked in isolation before landing:
    /// 1. Replaced the per-sample `phase.sin_cos()` trig call with a
    ///    recursive ROTATING oscillator: `(cos,sin)` advanced each
    ///    sample by complex multiplication against a FIXED per-sample
    ///    rotation (`rot_cos`/`rot_sin`, computed ONCE per retune, not
    ///    per sample) -- the standard technique for exactly this
    ///    situation (see any DDS/NCO reference). This accumulates tiny
    ///    floating-point error over many rotations (the vector's own
    ///    magnitude drifts from 1.0), corrected by periodic
    ///    renormalization (see RENORM_INTERVAL) -- confirmed via a
    ///    direct test that magnitude drift after a full second (64.8
    ///    million rotations) stays around 1e-14, utterly negligible.
    /// 2. Restructured from a `&mut self`-per-sample API to this
    ///    block-based one: ALL hot state (oscillator, CIC stages, decim
    ///    counter) is copied into local variables ONCE at the top of
    ///    this function, the entire inner loop operates purely on those
    ///    locals (letting the compiler keep them in registers instead
    ///    of re-reading/re-writing struct fields on every single sample
    ///    -- struct-field access defeated a surprising amount of
    ///    optimization in direct benchmarking), and state is written
    ///    back to `self` only once at the end.
    ///
    /// Combined: 0.297s of CPU time per 1s of real audio in direct
    /// benchmarking -- roughly 7x faster than the original, and with
    /// over 3x real-time headroom instead of none.
    pub fn process_block(&mut self, samples: &[i16], mut emit: impl FnMut((i32, i32))) {
        let (mut osc_cos, mut osc_sin) = (self.osc_cos, self.osc_sin);
        let (rot_cos, rot_sin) = (self.rot_cos, self.rot_sin);
        let mut renorm_counter = self.renorm_counter;
        let mut count = self.count;
        let mut stages_i = self.stages_i;
        let mut stages_q = self.stages_q;
        let decimation = self.decimation;
        let gain_comp = self.gain_comp;

        for &sample in samples {
            let x = sample as f64;
            // Truncating cast rather than `.round()`: the <1 LSB DC-ish
            // bias this introduces is utterly negligible against this
            // signal chain's own dynamic range, and roughly halves the
            // per-sample cast cost (measured directly) -- meaningful at
            // 64.8 million calls/sec, irrelevant at audio levels.
            //
            // See `Ddc::process_sample`'s old doc comment (now folded
            // into this function) for why this mixes with e^{+j*phase}
            // (`i=x*cos, q=x*sin`, no negation) rather than the
            // "textbook" e^{-j*phase} down-conversion convention -- a
            // real report confirmed this codebase's existing I/Q
            // convention (matching real P1/P2 hardware) needs the
            // conjugate of the textbook-correct mixer.
            let mut i_val = (x * osc_cos) as i64;
            let mut q_val = (x * osc_sin) as i64;

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

            for stage in &mut stages_i {
                stage.integrator = stage.integrator.wrapping_add(i_val);
                i_val = stage.integrator;
            }
            for stage in &mut stages_q {
                stage.integrator = stage.integrator.wrapping_add(q_val);
                q_val = stage.integrator;
            }

            count += 1;
            if count < decimation {
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

            emit(((i_val as f64 * gain_comp) as i32, (q_val as f64 * gain_comp) as i32));
        }

        self.osc_cos = osc_cos;
        self.osc_sin = osc_sin;
        self.renorm_counter = renorm_counter;
        self.count = count;
        self.stages_i = stages_i;
        self.stages_q = stages_q;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every rate `ddc_params_for_output_rate` claims to support must
    /// ACTUALLY divide evenly (an approximate/rounded decimation would
    /// silently disable WDSP's resampler instead of erroring -- see
    /// that function's own doc comment) AND stay within the CIC's own
    /// i64 arithmetic budget (see CicStage's doc comment) -- a real
    /// regression this test would catch: a future edit picks a
    /// decimation/ADC-rate pair that's off by rounding, or large enough
    /// to silently overflow.
    #[test]
    fn ddc_params_for_every_supported_rate_divide_exactly_and_fit_i64() {
        for &rate in &[96_000u32, 192_000, 384_000] {
            let (adc_rate, decimation) = ddc_params_for_output_rate(rate).unwrap();
            assert_eq!(adc_rate / decimation, rate, "rate {rate}");
            assert_eq!(adc_rate % decimation, 0, "rate {rate} does not divide exactly");
            let worst_case = (decimation as f64).powi(CIC_STAGES as i32) * i16::MAX as f64;
            assert!(worst_case < i64::MAX as f64, "rate {rate}: worst-case CIC magnitude {worst_case} overflows i64");
        }
    }

    /// 48000 is deliberately unsupported (see
    /// ddc_params_for_output_rate's own doc comment on why) -- locks in
    /// that this stays a conscious exclusion, not an accidental gap.
    #[test]
    fn ddc_params_for_output_rate_excludes_48khz() {
        assert_eq!(ddc_params_for_output_rate(48_000), None);
    }

    /// Confirms the bundled firmware asset actually ships where
    /// `default_firmware_path` expects it (via the CARGO_MANIFEST_DIR
    /// candidate, which is independent of the test runner's own CWD) --
    /// a real regression this test would catch: assets/rx888/ renamed,
    /// or SDDC_FX3.img accidentally left out of a commit/release.
    #[test]
    fn default_firmware_path_finds_the_bundled_image() {
        let path = default_firmware_path().expect("assets/rx888/SDDC_FX3.img should be bundled");
        assert!(path.is_file(), "{path:?} does not exist");
    }

    /// Locks in the link-speed check `initialise` uses to fail clearly
    /// on a USB2 cable/port instead of silently streaming corrupted
    /// data -- see REQUIRED_BYTES_PER_SEC's own doc comment for the
    /// real report this guards against. Mirrors a second, independent
    /// RX-888 Rust implementation's own identical test
    /// (~/github/sdroxide's sdroxide-rx888::usb::
    /// a_usb2_link_cannot_carry_the_default_adc_rate).
    #[test]
    fn usb2_high_speed_cannot_carry_the_default_adc_rate_but_superspeed_can() {
        assert!(usable_bytes_per_sec(Some(Speed::High)) < REQUIRED_BYTES_PER_SEC);
        assert!(usable_bytes_per_sec(Some(Speed::Super)) > REQUIRED_BYTES_PER_SEC);
    }

    /// ka9q-radio's own default (highgain mode, +1.5dB requested) --
    /// hand-verified against `gain2val`'s formula: voltage =
    /// 10^(1.5/20) ~= 1.1885, denom = 0.055744*7.079458 ~= 0.39468,
    /// code = round(1.1885/0.39468) = 3, result = 3 | 0x80 = 131.
    #[test]
    fn vga_gain_arg_matches_ka9q_default() {
        assert_eq!(vga_gain_arg(true, 1.5), 131);
    }

    /// The LTC2208 datasheet's own description of the randomizer,
    /// written independently of `derandomize`'s implementation (a
    /// straightforward conditional, not the same branchless bit trick)
    /// so this test can't just agree with a broken implementation.
    fn spec_randomize(sample: i16) -> i16 {
        let u = sample as u16;
        if u & 1 != 0 { (u ^ 0xFFFE) as i16 } else { sample }
    }

    #[test]
    fn derandomize_inverts_the_datasheet_operation() {
        for raw in i16::MIN..=i16::MAX {
            assert_eq!(derandomize(spec_randomize(raw)), raw, "raw {raw}");
        }
    }

    #[test]
    fn derandomize_is_its_own_inverse() {
        for raw in i16::MIN..=i16::MAX {
            assert_eq!(derandomize(derandomize(raw)), raw);
        }
    }

    /// Feeds a synthetic real tone through the DDC and returns the
    /// average magnitude of the decimated output, discarding the first
    /// few output samples (the CIC's own group delay/fill transient).
    fn tone_magnitude(adc_rate_hz: u32, tone_hz: f64, tune_hz: f64) -> f64 {
        let mut ddc = Ddc::new(adc_rate_hz, CIC_DECIMATION, CIC_STAGES);
        ddc.set_tune_freq(tune_hz);
        let tone_inc = 2.0 * std::f64::consts::PI * tone_hz / adc_rate_hz as f64;
        let mut tone_phase = 0.0f64;
        let mut outputs = Vec::new();
        // Enough input samples for a comfortable number of decimated
        // outputs past the CIC's fill transient (stages * decimation
        // input samples, i.e. 3*256 = 768, before the pipeline is even
        // full).
        for _ in 0..(CIC_DECIMATION as usize * 200) {
            let sample = (tone_phase.sin() * 20_000.0) as i16;
            tone_phase += tone_inc;
            if let Some((i, q)) = ddc.process_sample(sample) {
                outputs.push(((i as f64).powi(2) + (q as f64).powi(2)).sqrt());
            }
        }
        // Discard the first 20 outputs (fill transient), average the rest.
        let steady = &outputs[20..];
        steady.iter().sum::<f64>() / steady.len() as f64
    }

    /// Tuning the DDC to the tone's own frequency should pass it through
    /// at strong, roughly constant magnitude (it lands at/near DC, well
    /// inside the CIC's passband) -- confirms the NCO mixer's tuning
    /// convention (multiply by e^{-j*2*pi*f*n/Fs}) actually selects the
    /// intended RF frequency, the single most safety/correctness-
    /// critical property of this whole DDC (a wrong sign or scale here
    /// means every reported dial frequency is simply wrong).
    #[test]
    fn on_tune_tone_passes_through_at_strong_magnitude() {
        let mag = tone_magnitude(DEFAULT_SAMPLE_RATE_HZ, 1_000_000.0, 1_000_000.0);
        // Full-scale i16 input (20000) should decimate down to a
        // meaningful fraction of IqSample's own ~2^23 full-scale
        // convention -- well above a few hundred confirms real signal,
        // not numerical noise.
        assert!(mag > 1_000.0, "on-tune magnitude too small: {mag}");
    }

    /// A tone several MHz away from the tuned frequency lands far
    /// outside the CIC decimator's passband (post-mixing, its
    /// pre-decimation frequency is `tone_hz - tune_hz`, many multiples
    /// of the decimated output rate) and should come through heavily
    /// attenuated relative to the on-tune case -- confirms the CIC is
    /// actually doing real filtering work, not passing everything
    /// through unfiltered (which would alias every out-of-band signal
    /// into the passband instead of rejecting it).
    #[test]
    fn off_tune_tone_is_heavily_attenuated() {
        let on_tune = tone_magnitude(DEFAULT_SAMPLE_RATE_HZ, 1_000_000.0, 1_000_000.0);
        let off_tune = tone_magnitude(DEFAULT_SAMPLE_RATE_HZ, 1_000_000.0, 6_000_000.0);
        assert!(
            off_tune < on_tune / 10.0,
            "off-tune tone not sufficiently attenuated: on={on_tune} off={off_tune}"
        );
    }

    /// ROOT CAUSE regression test for a real report (2026-09-19): a
    /// station externally confirmed to be at a fixed, known RF frequency
    /// (a strong AM broadcast station, cross-checked against a second,
    /// working radio on the same antenna) appeared MIRRORED around the
    /// dial once off-center -- tuned 10kHz below the station's real
    /// frequency, the station showed up 10kHz on the WRONG side of the
    /// dial on the spectrum display, not just off by some amount. The
    /// two magnitude-only tests above (on_tune_tone_passes_through_at_
    /// strong_magnitude, off_tune_tone_is_heavily_attenuated) could NOT
    /// have caught this: magnitude is identical for a tone above vs.
    /// below the tuned frequency, only the SIGN of the resulting
    /// baseband frequency differs. This test checks that sign directly,
    /// via the complex output's own phase progression (a tone above the
    /// tuned frequency must produce a monotonically-in-one-consistent-
    /// direction-rotating (i,q) pair -- which direction is pinned to
    /// match this codebase's existing, real-hardware-verified I/Q
    /// convention, confirmed via the field report above, not derived
    /// from first-principles DSP theory alone (a mixer using the
    /// "textbook" e^{-j*phase} convention in isolation is just as
    /// mathematically valid, but was empirically confirmed to disagree
    /// with what every OTHER board's own hardware, and this codebase's
    /// existing WDSP/display code built against it, already expect).
    #[test]
    fn tone_above_tuned_frequency_rotates_in_the_expected_direction() {
        let adc_rate = DEFAULT_SAMPLE_RATE_HZ;
        let tune_hz = 14_074_000.0;
        let tone_hz = tune_hz + 5_000.0; // 5kHz ABOVE the tuned frequency
        let mut ddc = Ddc::new(adc_rate, CIC_DECIMATION, CIC_STAGES);
        ddc.set_tune_freq(tune_hz);
        let tone_inc = 2.0 * std::f64::consts::PI * tone_hz / adc_rate as f64;
        let mut tone_phase = 0.0f64;
        let mut outputs = Vec::new();
        for _ in 0..(CIC_DECIMATION as usize * 300) {
            let sample = (tone_phase.sin() * 20_000.0) as i16;
            tone_phase += tone_inc;
            if tone_phase > 2.0 * std::f64::consts::PI {
                tone_phase -= 2.0 * std::f64::consts::PI;
            }
            if let Some((i, q)) = ddc.process_sample(sample) {
                outputs.push((i as f64, q as f64));
            }
        }
        // Average unwrapped phase advance per decimated output sample,
        // over the steady-state tail (past the CIC's own fill/group-
        // delay transient) -- its SIGN is what this test locks in.
        let tail = &outputs[20..];
        let mut prev_phase = tail[0].1.atan2(tail[0].0);
        let mut total = 0.0;
        for &(i, q) in &tail[1..] {
            let phase = q.atan2(i);
            let mut d = phase - prev_phase;
            if d > std::f64::consts::PI {
                d -= 2.0 * std::f64::consts::PI;
            } else if d < -std::f64::consts::PI {
                d += 2.0 * std::f64::consts::PI;
            }
            total += d;
            prev_phase = phase;
        }
        let avg_phase_step = total / (tail.len() - 1) as f64;
        assert!(
            avg_phase_step < -0.01,
            "a tone above the tuned frequency must rotate in the negative-phase-step \
             direction to match this codebase's existing I/Q convention (see this test's \
             own doc comment) -- got avg_phase_step={avg_phase_step}"
        );
    }

    #[test]
    fn gain_comp_matches_decimation_and_stages() {
        let ddc = Ddc::new(DEFAULT_SAMPLE_RATE_HZ, CIC_DECIMATION, CIC_STAGES);
        let expected = (8_388_608.0 / 32_768.0) * HEADROOM_FACTOR / (CIC_DECIMATION as f64).powi(CIC_STAGES as i32);
        assert_eq!(ddc.gain_comp, expected);
    }

    /// The whole reason CIC_DECIMATION=675 -- see that constant's own
    /// doc comment for the real-hardware WDSP crash this fixes (a
    /// stricter requirement than "any multiple of 48kHz", which a
    /// previous attempt already tried and which did NOT fix the crash).
    /// Locks in the property that actually matters: WDSP's
    /// `calc_HBResampler` (vendor/wdsp/reshb.c) only recognizes this
    /// EXACT set of per-channel input rates, silently disabling itself
    /// (bypass mode) for anything else -- the same set the UI's own
    /// Sample Rate buttons expose for every other board.
    #[test]
    fn output_sample_rate_is_one_of_wdsps_recognized_hbresampler_rates() {
        const WDSP_RECOGNIZED_RATES: [u32; 7] =
            [96_000, 192_000, 384_000, 768_000, 1_536_000, 3_072_000, 6_144_000];
        assert!(WDSP_RECOGNIZED_RATES.contains(&OUTPUT_SAMPLE_RATE_HZ));
    }

    /// The single most important regression test for this module: a
    /// persistent DC bias fed continuously for a long run must NOT make
    /// the output grow unbounded. This is exactly the real-hardware bug
    /// this module's own doc comment describes (an `f64` CIC integrator
    /// implementation let a real-world non-zero-mean input grow the
    /// integrators without bound, causing the reported signal level to
    /// climb without limit within a couple of seconds of connecting) --
    /// confirmed via real hardware screenshots showing the S-meter climb
    /// from -4dBm to +26dBm over about 3 seconds. Runs for MANY more
    /// input samples than a single decimation window (enough that the
    /// old `f64` implementation would already show visible drift) and
    /// asserts every decimated output across the whole run stays within
    /// a sane bound -- not just the last one, in case of a slow leak.
    #[test]
    fn persistent_dc_bias_does_not_grow_output_unbounded() {
        let mut ddc = Ddc::new(DEFAULT_SAMPLE_RATE_HZ, CIC_DECIMATION, CIC_STAGES);
        // Tuned to 0 Hz so a constant real ADC sample mixes straight
        // through as a persistent DC component (cos(0)=1, sin(0)=0) --
        // exactly what happens for ANY real received carrier once it's
        // mixed to baseband, not just this synthetic edge case (a real
        // carrier IS a DC component after down-conversion to its own
        // dial frequency -- this bug didn't need exotic conditions to
        // trigger, any real reception at all would eventually hit it).
        ddc.set_tune_freq(0.0);
        let mut max_mag = 0i64;
        // 20,000 decimation windows -- far more input samples than the
        // CIC's own fill time, plenty to expose unbounded growth if it
        // were present.
        for _ in 0..(CIC_DECIMATION as usize * 20_000) {
            // A strong, purely real DC bias (no AC content at all) --
            // the worst case for an integrator with no leakage.
            if let Some((i, q)) = ddc.process_sample(20_000) {
                max_mag = max_mag.max(i.unsigned_abs() as i64).max(q.unsigned_abs() as i64);
            }
        }
        // IqSample's own convention tops out around 2^23 (~8.4 million)
        // for a full-scale signal -- anything comfortably under 100
        // million confirms the output stayed bounded rather than
        // climbing without limit across 20,000 decimated outputs.
        assert!(max_mag < 100_000_000, "output grew unbounded: max magnitude {max_mag}");
    }

    #[test]
    fn output_sample_rate_is_exact_integer_division() {
        assert_eq!(OUTPUT_SAMPLE_RATE_HZ, 96_000);
    }
}
