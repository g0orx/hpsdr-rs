/*
    ADALM-Pluto (and its clones) -- an AD9363/AD9361 transceiver behind
    a Zynq running Linux, reached over the network: iiod, libiio's own
    server, listens on TCP 30431. Over USB the Pluto presents itself as
    an RNDIS/CDC Ethernet adapter, so the default address 192.168.2.1 is
    a network address even when it's plugged into a USB port. No libiio
    or libusb is needed -- iiod's text protocol is small enough to speak
    directly. Receive-only for now.

    The protocol, as libiio v0.25's iiod-client.c speaks it (every
    command is one line, and every reply starts with a decimal integer
    line: negative errno on failure):
    - `PRINT` -> length, then the context XML (device ids/names, context
      attributes such as hw_model / hw_serial).
    - `READ <dev> [INPUT|OUTPUT <chan>] <attr>` -> length, then the value.
    - `WRITE <dev> [INPUT|OUTPUT <chan>] <attr> <len>` followed by the
      value's bytes -> bytes written.
    - `OPEN <dev> <samples> <hex channel mask>` -> 0, then
      `READBUF <dev> <bytes>` -> repeatedly: chunk length, channel mask
      line, chunk bytes, until the requested bytes have all arrived
      (there is no terminating 0 -- confirmed on a real Pluto clone
      running firmware v0.39, 2026-10-26).
    - `CLOSE <dev>`.
    Devices are named rather than numbered (iiod looks a device up by id
    or name), so nothing depends on the order the firmware registered
    them in. libiio streams over its own connection, separate from the
    one used for attributes; so does this -- a blocking READBUF would
    otherwise stall every retune behind it.

    Streaming rate: the AD9363 itself can't run below ~2.083MS/s without
    a custom FIR, so the phy runs at 8 x 1.536MS/s and the FPGA's fixed
    decimate-by-8 (enabled by asking cf-ad9361-lpc for phy/8, as
    libad9361's ad9361_set_bb_rate does) brings it down to
    rtlsdr::DONGLE_RATE_HZ. From there rtlsdr::Decimator, its output-rate
    list and its offset tuning apply unchanged, exactly as for the
    MSI.SDR. 1.536MS/s of 16-bit I/Q is ~6MB/s, comfortably inside what
    iiod manages over the USB link.
*/

use crate::rtlsdr;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::time::Duration;

pub const IIOD_PORT: u16 = 30431;

/// The Pluto's own address on its USB network link, unless someone has
/// changed it in the Pluto's config.txt.
pub const DEFAULT_IP: IpAddr = IpAddr::V4(Ipv4Addr::new(192, 168, 2, 1));

/// Same fixed rate as the USB dongles -- see the module doc comment.
pub const SAMPLE_RATE_HZ: u32 = rtlsdr::DONGLE_RATE_HZ;

/// The wide mode's rate (Settings -> RX, "Wide stream"), the same as
/// mirisdr::WIDE_SAMPLE_RATE_HZ, so the DATV path sees the same rate
/// from either. Above the AD9363's floor, so the FPGA's decimation is
/// off. ~18MB/s over iiod: a real Pluto clone kept up with it (4.590M
/// samples/s read over 10 s, 2026-10-26, against the beacon on QO-100).
pub const WIDE_SAMPLE_RATE_HZ: u32 = 4_608_000;

/// The analog filter in wide mode -- wide enough for QO-100's ~2MHz
/// DATV beacon with room to tune either side of it.
const WIDE_RF_BANDWIDTH_HZ: u32 = 4_000_000;

/// The transponder view's rate: all of QO-100's wideband transponder
/// (10491-10499.5 MHz) at once. Faster than the USB link can carry --
/// REAL MEASUREMENT on a Pluto clone (2026-10-26): ~6.45M samples/s
/// arrive at any rate from 9.2 MS/s up, in whole buffers with gaps
/// between -- so it's only for a spectrum, which doesn't need the stream
/// to be continuous; see transponder.rs. Since the link limits what
/// arrives either way, a faster rate costs nothing, and leaves the
/// transponder well inside the filters' flat part: at 10.24 MS/s with a
/// 9.2 MHz filter the beacon, at the bottom edge, was tilted enough to
/// read 82 kHz off centre.
pub const SCAN_SAMPLE_RATE_HZ: u32 = 12_288_000;

/// The analog filter for SCAN_SAMPLE_RATE_HZ: measured flat to within
/// ~0.3 dB across the transponder.
const SCAN_RF_BANDWIDTH_HZ: u32 = 11_000_000;

/// Buffers to throw away after changing rate with the stream open: the
/// Pluto keeps a few filled at the old rate. REAL MEASUREMENT: skipping
/// 3 after going back to the wide rate from the scan rate still counted
/// ~4% too many samples over the next 2 s.
pub const RATE_CHANGE_STALE_BUFFERS: usize = 6;

pub fn sample_rate_hz(wide: bool) -> u32 {
    if wide {
        WIDE_SAMPLE_RATE_HZ
    } else {
        SAMPLE_RATE_HZ
    }
}

/// The FPGA's fixed decimation (cf-ad9361-lpc's only other rate).
const FPGA_DECIMATION: u32 = 8;

/// The ADC delivers 12-bit samples sign-extended into 16 bits (the
/// XML's `le:S12/16>>0`).
pub const SAMPLE_FULL_SCALE: f64 = 2048.0;

/// RX hardwaregain in manual mode: -3..71 dB on the AD9363 (read from
/// hardwaregain_available on a real clone). The slider stops at 0 --
/// rx_attenuation is unsigned, and negative gain is never useful here.
pub const MAX_GAIN_DB: u32 = 71;
pub const DEFAULT_GAIN_DB: u32 = 40;

/// The stock AD9363's tuning range, used until the real one is read
/// from the radio (a Pluto switched to AD9364 mode reports 70MHz-6GHz).
const DEFAULT_FREQUENCY_MIN_HZ: u64 = 325_000_000;
const DEFAULT_FREQUENCY_MAX_HZ: u64 = 3_800_000_000;

/// Samples per READBUF -- ~21 ms at SAMPLE_RATE_HZ. Also the kernel
/// buffer size OPEN asks for; iiod keeps a few of them, which is all the
/// slack there is before the Pluto drops samples.
pub const BUFFER_SAMPLES: usize = 32 * 1024;

/// At WIDE_SAMPLE_RATE_HZ, BUFFER_SAMPLES would be only ~7 ms a buffer;
/// this is ~57 ms. The scratch recorder that captured the first
/// decodable QO-100 beacon read this much at a time with no loss.
pub const WIDE_BUFFER_SAMPLES: usize = 256 * 1024;

pub fn buffer_samples(wide: bool) -> usize {
    if wide {
        WIDE_BUFFER_SAMPLES
    } else {
        BUFFER_SAMPLES
    }
}

/// Last byte of the Pluto's sentinel MAC (the rest is a hash of its
/// serial number) -- distinct from rtlsdr::MAC_TAG and mirisdr::MAC_TAG,
/// whose MACs are USB-port based.
pub const MAC_TAG: u8 = 4;

const PHY: &str = "ad9361-phy";
const RX_ADC: &str = "cf-ad9361-lpc";

const CONNECT_TIMEOUT: Duration = Duration::from_millis(400);
const IO_TIMEOUT: Duration = Duration::from_secs(2);

/// One iiod connection.
pub struct Iiod {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
}

impl Iiod {
    pub fn connect(addr: SocketAddr) -> io::Result<Self> {
        let stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)?;
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(IO_TIMEOUT))?;
        stream.set_write_timeout(Some(IO_TIMEOUT))?;
        Ok(Self { reader: BufReader::with_capacity(256 * 1024, stream.try_clone()?), writer: stream })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.writer.local_addr()
    }

    fn read_int(&mut self) -> io::Result<i64> {
        let mut line = String::new();
        if self.reader.read_line(&mut line)? == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "iiod closed the connection"));
        }
        line.trim().parse().map_err(|_| io::Error::other(format!("iiod: unexpected reply {:?}", line.trim())))
    }

    /// Sends one command line and returns its integer reply, mapping a
    /// negative one to an error.
    fn command(&mut self, cmd: &str) -> io::Result<i64> {
        self.writer.write_all(format!("{cmd}\r\n").as_bytes())?;
        let n = self.read_int()?;
        if n < 0 {
            return Err(io::Error::other(format!("iiod: {cmd:?} failed (errno {})", -n)));
        }
        Ok(n)
    }

    /// Reads `n` bytes of a reply plus the newline iiod puts after it.
    fn read_payload(&mut self, n: usize) -> io::Result<Vec<u8>> {
        let mut buf = vec![0u8; n];
        self.reader.read_exact(&mut buf)?;
        let mut nl = String::new();
        self.reader.read_line(&mut nl)?;
        Ok(buf)
    }

    pub fn print_xml(&mut self) -> io::Result<String> {
        let n = self.command("PRINT")? as usize;
        Ok(String::from_utf8_lossy(&self.read_payload(n)?).into_owned())
    }

    /// `target` is "<dev>" or "<dev> INPUT|OUTPUT <chan>".
    pub fn read_attr(&mut self, target: &str, attr: &str) -> io::Result<String> {
        let n = self.command(&format!("READ {target} {attr}"))? as usize;
        Ok(String::from_utf8_lossy(&self.read_payload(n)?).trim_end_matches(['\0', '\n']).to_string())
    }

    pub fn write_attr(&mut self, target: &str, attr: &str, value: &str) -> io::Result<()> {
        self.writer.write_all(format!("WRITE {target} {attr} {}\r\n", value.len()).as_bytes())?;
        self.writer.write_all(value.as_bytes())?;
        let n = self.read_int()?;
        if n < 0 {
            return Err(io::Error::other(format!("iiod: writing {target} {attr} = {value} failed (errno {})", -n)));
        }
        Ok(())
    }
}

/// What discovery learns about a Pluto.
pub struct Info {
    pub addr: SocketAddr,
    pub local_addr: SocketAddr,
    pub serial: String,
    pub frequency_min_hz: u64,
    pub frequency_max_hz: u64,
}

impl Info {
    /// The sentinel MAC that keys this Pluto's config: a hash of its
    /// serial, so it follows the radio rather than its address.
    pub fn mac(&self) -> [u8; 6] {
        let h = self.serial.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100_0000_01b3));
        let b = h.to_le_bytes();
        [0, b[0], b[1], b[2], b[3], MAC_TAG]
    }
}

/// The value of a context attribute in iiod's XML, e.g.
/// `<context-attribute name="hw_model" value="..." />`.
fn context_attribute(xml: &str, name: &str) -> Option<String> {
    let start = xml.find(&format!("<context-attribute name=\"{name}\" value=\""))?;
    let rest = &xml[start..];
    let rest = &rest[rest.find("value=\"")? + 7..];
    Some(rest[..rest.find('"')?].replace("&amp;", "&"))
}

/// Parses an IIO range attribute like "[325000000 1 3800000000]".
fn parse_range(s: &str) -> Option<(u64, u64)> {
    let mut parts = s.trim().trim_start_matches('[').trim_end_matches(']').split_whitespace();
    let min = parts.next()?.parse().ok()?;
    let _step = parts.next()?;
    let max = parts.next()?.parse().ok()?;
    Some((min, max))
}

/// Asks the iiod at `ip` what it is. None if nothing answers, or it
/// isn't an AD936x radio.
pub fn probe(ip: IpAddr) -> Option<Info> {
    let addr = SocketAddr::new(ip, IIOD_PORT);
    let mut iiod = Iiod::connect(addr).ok()?;
    let xml = iiod.print_xml().ok()?;
    if !xml.contains(&format!("name=\"{PHY}\"")) || !xml.contains(&format!("name=\"{RX_ADC}\"")) {
        return None;
    }
    let (frequency_min_hz, frequency_max_hz) = iiod
        .read_attr(&format!("{PHY} OUTPUT altvoltage0"), "frequency_available")
        .ok()
        .and_then(|s| parse_range(&s))
        .unwrap_or((DEFAULT_FREQUENCY_MIN_HZ, DEFAULT_FREQUENCY_MAX_HZ));
    Some(Info {
        addr,
        local_addr: iiod.local_addr().ok()?,
        serial: context_attribute(&xml, "hw_serial").unwrap_or_default(),
        frequency_min_hz,
        // The receiver's frequency is a u32 (see RadioSession::frequency_hz).
        frequency_max_hz: frequency_max_hz.min(u32::MAX as u64),
    })
}

/// The Pluto's control connection -- tuning and gain. Owned by
/// radio.rs's pluto_control_loop.
pub struct PlutoDevice {
    iiod: Iiod,
}

impl PlutoDevice {
    pub fn set_center_freq(&mut self, hz: u32) -> io::Result<()> {
        self.iiod.write_attr(&format!("{PHY} OUTPUT altvoltage0"), "frequency", &hz.to_string())
    }

    pub fn set_gain(&mut self, gain_db: u32) -> io::Result<()> {
        self.iiod.write_attr(&format!("{PHY} INPUT voltage0"), "hardwaregain", &gain_db.min(MAX_GAIN_DB).to_string())
    }

    /// The receiver's own stream rate: SAMPLE_RATE_HZ, or with `wide`
    /// WIDE_SAMPLE_RATE_HZ. Works with the stream open.
    pub fn set_stream_rate(&mut self, wide: bool) -> io::Result<()> {
        if wide {
            self.set_rates(WIDE_SAMPLE_RATE_HZ, WIDE_SAMPLE_RATE_HZ, WIDE_RF_BANDWIDTH_HZ)
        } else {
            self.set_rates(SAMPLE_RATE_HZ * FPGA_DECIMATION, SAMPLE_RATE_HZ, SAMPLE_RATE_HZ)
        }
    }

    /// SCAN_SAMPLE_RATE_HZ, for the transponder view.
    pub fn set_scan_rate(&mut self) -> io::Result<()> {
        self.set_rates(SCAN_SAMPLE_RATE_HZ, SCAN_SAMPLE_RATE_HZ, SCAN_RF_BANDWIDTH_HZ)
    }

    /// `phy_rate`: the AD936x's own; `rate`: what the FPGA delivers (the
    /// same, or phy_rate / FPGA_DECIMATION); `rf_bandwidth`: the analog
    /// filter.
    fn set_rates(&mut self, phy_rate: u32, rate: u32, rf_bandwidth: u32) -> io::Result<()> {
        let rx = format!("{PHY} INPUT voltage0");
        self.iiod.write_attr(&rx, "sampling_frequency", &phy_rate.to_string())?;
        // Only valid once the phy rate above is set: the FPGA offers that
        // rate and rate/8.
        self.iiod.write_attr(&format!("{RX_ADC} INPUT voltage0"), "sampling_frequency", &rate.to_string())?;
        self.iiod.write_attr(&rx, "rf_bandwidth", &rf_bandwidth.to_string())
    }
}

/// Configures the receive side, opens the stream and returns the
/// control and streaming halves. `wide`: stream at WIDE_SAMPLE_RATE_HZ
/// rather than SAMPLE_RATE_HZ.
pub fn initialise(
    addr: SocketAddr,
    frequency_hz: u32,
    gain_db: u32,
    wide: bool,
) -> io::Result<(PlutoDevice, RxStream, String)> {
    let mut iiod = Iiod::connect(addr)?;
    let xml = iiod.print_xml()?;
    let model = context_attribute(&xml, "hw_model").unwrap_or_else(|| "AD936x".to_string());

    let rx = format!("{PHY} INPUT voltage0");
    iiod.write_attr(&rx, "rf_port_select", "A_BALANCED")?;
    iiod.write_attr(&rx, "gain_control_mode", "manual")?;
    // Receive-only: keep the transmitter's LO off, so nothing leaks out
    // of the TX port.
    iiod.write_attr(&format!("{PHY} OUTPUT altvoltage1"), "powerdown", "1")?;
    let mut device = PlutoDevice { iiod };
    device.set_stream_rate(wide)?;
    device.set_center_freq(frequency_hz)?;
    device.set_gain(gain_db)?;

    let stream = RxStream::open(addr, buffer_samples(wide))?;
    Ok((device, stream, model))
}

/// The receive stream, on its own connection.
pub struct RxStream {
    iiod: Iiod,
    open: bool,
    buffer_samples: usize,
}

impl RxStream {
    fn open(addr: SocketAddr, buffer_samples: usize) -> io::Result<Self> {
        let mut iiod = Iiod::connect(addr)?;
        // Channel mask 3: voltage0 (I) and voltage1 (Q).
        iiod.command(&format!("OPEN {RX_ADC} {buffer_samples} 00000003"))?;
        Ok(Self { iiod, open: true, buffer_samples })
    }

    /// I/Q pairs per buffer -- what `read` should be given room for.
    pub fn buffer_samples(&self) -> usize {
        self.buffer_samples
    }

    /// Fills `buf` (buffer_samples() interleaved I/Q pairs, as raw
    /// little-endian bytes) with the next buffer's worth of samples.
    pub fn read(&mut self, buf: &mut [u8]) -> io::Result<()> {
        self.iiod.writer.write_all(format!("READBUF {RX_ADC} {}\r\n", buf.len()).as_bytes())?;
        let mut filled = 0;
        while filled < buf.len() {
            let n = self.iiod.read_int()?;
            if n <= 0 {
                return Err(io::Error::other(format!("iiod: READBUF failed (errno {})", -n)));
            }
            let n = n as usize;
            if filled + n > buf.len() {
                return Err(io::Error::other("iiod: READBUF sent more than requested"));
            }
            let mut mask = String::new();
            self.iiod.reader.read_line(&mut mask)?;
            self.iiod.reader.read_exact(&mut buf[filled..filled + n])?;
            filled += n;
        }
        Ok(())
    }

    pub fn close(&mut self) {
        if self.open {
            self.open = false;
            let _ = self.iiod.command(&format!("CLOSE {RX_ADC}"));
        }
    }
}

impl Drop for RxStream {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_real_pluto_replies() {
        // Verbatim from a Pluto clone, firmware v0.39.
        assert_eq!(parse_range("[325000000 1 3800000000] "), Some((325_000_000, 3_800_000_000)));
        let xml = "<context-attribute name=\"hw_model\" value=\"Analog Devices PlutoSDR Rev.B (Z7010-AD9363A)\" />\
                   <context-attribute name=\"hw_serial\" value=\"03df643eb44b274e38d76c36bc533f5620\" />";
        assert_eq!(context_attribute(xml, "hw_model").as_deref(), Some("Analog Devices PlutoSDR Rev.B (Z7010-AD9363A)"));
        assert_eq!(context_attribute(xml, "hw_serial").as_deref(), Some("03df643eb44b274e38d76c36bc533f5620"));
        assert_eq!(context_attribute(xml, "fw_version"), None);
    }

    /// Needs a Pluto at DEFAULT_IP, and one test at a time (the Pluto
    /// streams to one client only):
    /// cargo test pluto -- --ignored --test-threads=1
    #[test]
    #[ignore]
    fn streams_from_a_real_pluto() {
        stream_test(false);
    }

    #[test]
    #[ignore]
    fn streams_wide_from_a_real_pluto() {
        stream_test(true);
    }

    /// The transponder view switches rates with the stream open: wide ->
    /// scan -> wide, measuring what arrives at each.
    #[test]
    #[ignore]
    fn switches_rate_with_the_stream_open() {
        let info = probe(DEFAULT_IP).expect("no Pluto at 192.168.2.1");
        let (mut device, mut stream, _) = initialise(info.addr, 745_250_000, DEFAULT_GAIN_DB, true).unwrap();
        let mut buf = vec![0u8; stream.buffer_samples() * 4];
        let mut measure = |stream: &mut RxStream, label: &str| {
            // The first buffers can still hold the old rate's samples.
            for _ in 0..RATE_CHANGE_STALE_BUFFERS {
                stream.read(&mut buf).unwrap();
            }
            let start = std::time::Instant::now();
            let mut samples = 0;
            while start.elapsed() < Duration::from_secs(2) {
                stream.read(&mut buf).unwrap();
                samples += buf.len() / 4;
            }
            let rate = samples as f64 / start.elapsed().as_secs_f64();
            println!("{label}: {rate:.0} samples/s");
            rate
        };
        let wide = measure(&mut stream, "wide");
        device.set_scan_rate().unwrap();
        let scan = measure(&mut stream, "scan");
        device.set_stream_rate(true).unwrap();
        let back = measure(&mut stream, "wide again");
        assert!((wide - WIDE_SAMPLE_RATE_HZ as f64).abs() < 0.03 * WIDE_SAMPLE_RATE_HZ as f64);
        // Faster than wide, but the link can't keep up with all of it.
        assert!(scan > wide * 1.2, "{scan}");
        assert!((back - WIDE_SAMPLE_RATE_HZ as f64).abs() < 0.03 * WIDE_SAMPLE_RATE_HZ as f64);
    }

    fn stream_test(wide: bool) {
        let info = probe(DEFAULT_IP).expect("no Pluto at 192.168.2.1");
        println!("range {}..{} Hz, mac {:02x?}", info.frequency_min_hz, info.frequency_max_hz, info.mac());
        let (mut device, mut stream, name) = initialise(info.addr, 432_474_000, DEFAULT_GAIN_DB, wide).unwrap();
        println!("{name}");
        let expected = sample_rate_hz(wide) as f64;
        let mut buf = vec![0u8; stream.buffer_samples() * 4];
        let start = std::time::Instant::now();
        let mut buffers = 0;
        let mut peak = 0i16;
        while start.elapsed() < Duration::from_secs(3) {
            stream.read(&mut buf).unwrap();
            buffers += 1;
            peak = buf.chunks_exact(2).map(|p| i16::from_le_bytes([p[0], p[1]]).saturating_abs()).max().unwrap().max(peak);
            if buffers == 20 {
                device.set_center_freq(435_000_000).unwrap();
                device.set_gain(20).unwrap();
            }
        }
        let rate = (buffers * stream.buffer_samples()) as f64 / start.elapsed().as_secs_f64();
        println!("{buffers} buffers, {rate:.0} samples/s, peak {peak}");
        assert!((rate - expected).abs() < expected * 0.05);
        assert!(peak <= 2048);
    }
}
