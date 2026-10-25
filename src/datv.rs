/*
    DATV receive: runs the DVB-S2 receiver (dvbs2/) on an RTL-SDR /
    MSI.SDR's wideband stream and sends the MPEG transport stream it
    recovers over UDP, for VLC, ffplay or the like to show.

    Two threads, so a slow LDPC decode never holds up the demodulator:
    - demod: takes the dongle's raw stream (radio::WidebandTap), keeps
      the channel's offset from the hardware's centre up to date as the
      dial or the transverter's LO correction moves, and turns samples
      into demodulated frames;
    - decode: LDPC, BCH, BBFRAME and TS packets, sent as UDP datagrams
      of 7 packets (1316 bytes, the usual TS-over-UDP size).
*/

use crate::dvbs2::decoder::{Decoder, DecoderStats};
use crate::dvbs2::demod::{Demod, Frame, Status};
use crate::dvbs2::Cf;
use crate::radio::WidebandTap;
use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// QO-100's wideband beacon (DVB-S2, 1.5 MS/s, QPSK 4/5) -- the default
/// channel.
pub const QO100_WB_BEACON_HZ: u64 = 10_491_500_000;

#[derive(Clone, Debug)]
pub struct DatvSettings {
    /// The signal's centre, in RF (what the dial shows).
    pub frequency_hz: u64,
    pub symbol_rate: u32,
    pub rolloff: f32,
    /// Where to send the transport stream, host:port.
    pub udp_addr: String,
}

#[derive(Clone, Debug, Default)]
pub struct DatvStatus {
    pub demod: Status,
    pub decoder: DecoderStats,
    /// Transport stream bit rate over the last few seconds.
    pub ts_bitrate: f64,
    /// Frames the decoder thread couldn't keep up with.
    pub dropped_frames: u64,
    /// Wideband reads the demod thread couldn't keep up with.
    pub dropped_blocks: u64,
}

pub struct DatvReceiver {
    pub status: Arc<Mutex<DatvStatus>>,
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
    tap: Arc<WidebandTap>,
}

/// Frames waiting for the decoder before new ones are dropped (~0.25 s of
/// short QPSK frames at 1.5 MS/s; more than enough for normal frames).
const FRAME_QUEUE: usize = 48;

impl DatvReceiver {
    /// `rf_offset_hz` is the active transverter's RF - IF offset (shared
    /// with the UI, so a moving LNB correction is followed).
    pub fn start(
        tap: Arc<WidebandTap>,
        rf_offset_hz: Arc<AtomicI64>,
        settings: DatvSettings,
    ) -> Result<Self, String> {
        let fs = tap.rate_hz as f64;
        let rs = settings.symbol_rate as f64;
        if rs * 2.2 > fs {
            return Err(format!(
                "{} kS/s is too fast for a {} kHz stream -- turn on the MSI.SDR's wide stream, or pick a lower symbol rate",
                settings.symbol_rate / 1000,
                tap.rate_hz / 1000
            ));
        }
        let socket = UdpSocket::bind("0.0.0.0:0").map_err(|e| e.to_string())?;
        socket.connect(&settings.udp_addr).map_err(|e| format!("{}: {e}", settings.udp_addr))?;

        let status = Arc::new(Mutex::new(DatvStatus::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let blocks = tap.attach();
        let (frame_tx, frame_rx) = mpsc::sync_channel::<Frame>(FRAME_QUEUE);

        let demod_status = Arc::clone(&status);
        let demod_stop = Arc::clone(&stop);
        let demod_tap = Arc::clone(&tap);
        let s = settings.clone();
        let demod_thread = thread::spawn(move || {
            let mut demod: Option<(i64, Demod)> = None;
            let mut frames = Vec::new();
            let mut samples = Vec::new();
            let mut last_status = Instant::now();
            let mut dropped_frames = 0u64;
            while !demod_stop.load(Ordering::Relaxed) {
                let block = match blocks.recv_timeout(Duration::from_millis(200)) {
                    Ok(b) => b,
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                };
                // The channel's offset from the stream's centre: follows
                // retuning and LNB correction. A big change (the dial
                // moved) restarts the demodulator; small ones (LNB drift)
                // just steer it.
                let centre_rf = block.center_hz as i64 + rf_offset_hz.load(Ordering::Relaxed);
                let channel = s.frequency_hz as i64 - centre_rf;
                match demod.as_mut() {
                    Some((c, d)) if (*c - channel).abs() < 1_000 => {
                        if *c != channel {
                            d.set_channel(channel as f64);
                            *c = channel;
                        }
                    }
                    _ => demod = Some((channel, Demod::new(fs, channel as f64, rs, s.rolloff as f64))),
                }
                let (_, d) = demod.as_mut().unwrap();
                samples.clear();
                samples.extend(block.samples.iter().map(|&[i, q]| Cf::new(i as f32 / 32768.0, q as f32 / 32768.0)));
                d.process(&samples, &mut frames);
                for f in frames.drain(..) {
                    if frame_tx.try_send(f).is_err() {
                        dropped_frames += 1;
                    }
                }
                if last_status.elapsed() > Duration::from_millis(250) {
                    last_status = Instant::now();
                    let mut st = demod_status.lock().unwrap();
                    st.demod = d.status;
                    st.dropped_frames = dropped_frames;
                    st.dropped_blocks = demod_tap.dropped_blocks.load(Ordering::Relaxed);
                }
            }
        });

        let decode_status = Arc::clone(&status);
        let decode_stop = Arc::clone(&stop);
        let decode_thread = thread::spawn(move || {
            let mut decoder = Decoder::new();
            let mut packets = Vec::new();
            let mut datagram = Vec::with_capacity(7 * 188);
            let mut window_start = Instant::now();
            let mut window_packets = 0u64;
            while !decode_stop.load(Ordering::Relaxed) {
                let frame = match frame_rx.recv_timeout(Duration::from_millis(200)) {
                    Ok(f) => f,
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                };
                decoder.decode(frame, &mut packets);
                for p in packets.drain(..) {
                    datagram.extend_from_slice(&p);
                    if datagram.len() == 7 * 188 {
                        let _ = socket.send(&datagram);
                        datagram.clear();
                    }
                    window_packets += 1;
                }
                let mut st = decode_status.lock().unwrap();
                st.decoder = decoder.stats;
                let elapsed = window_start.elapsed().as_secs_f64();
                if elapsed > 2.0 {
                    st.ts_bitrate = window_packets as f64 * 188.0 * 8.0 / elapsed;
                    window_start = Instant::now();
                    window_packets = 0;
                }
            }
        });

        Ok(Self { status, stop, threads: vec![demod_thread, decode_thread], tap })
    }

    pub fn status(&self) -> DatvStatus {
        self.status.lock().unwrap().clone()
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.tap.detach();
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

impl Drop for DatvReceiver {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dvbs2::params::PlsCode;
    use crate::dvbs2::testsignal::{frames, test_packets, waveform, Channel};
    use crate::radio::WidebandBlock;

    /// The whole runtime path: a synthetic beacon-like signal through a
    /// real WidebandTap, the two threads, and out of a UDP socket.
    #[test]
    fn transport_stream_arrives_over_udp() {
        let fs = crate::mirisdr::WIDE_SAMPLE_RATE_HZ;
        let packets = test_packets(900);
        let pls = PlsCode { modcod: 8, short: false, pilots: false };
        let ch = Channel {
            fs: fs as f64,
            symbol_rate: 1_500_000.0,
            rolloff: 0.35,
            carrier_hz: 0.0,
            clock_ppm: 15.0,
            esn0_db: 8.0,
            seed: 7,
        };
        let samples = waveform(&frames(&packets, pls), &ch);

        let listener = UdpSocket::bind("127.0.0.1:0").unwrap();
        listener.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let tap = WidebandTap::new(fs);
        // Hardware at 741.8 MHz, a 9750 MHz LO: the signal (at the
        // stream's centre here) is at 10491.8 MHz.
        let rf_offset = Arc::new(AtomicI64::new(9_750_000_000));
        let settings = DatvSettings {
            frequency_hz: 10_491_800_000,
            symbol_rate: 1_500_000,
            rolloff: 0.35,
            udp_addr: listener.local_addr().unwrap().to_string(),
        };
        let mut rx = DatvReceiver::start(Arc::clone(&tap), rf_offset, settings).unwrap();
        for chunk in samples.chunks(32_768) {
            let block = chunk
                .iter()
                .map(|s| [(s.re * 32768.0).clamp(-32768.0, 32767.0) as i16, (s.im * 32768.0).clamp(-32768.0, 32767.0) as i16])
                .collect();
            tap.send(WidebandBlock { center_hz: 741_800_000, samples: block });
            // Roughly real time is plenty slower than needed; just don't
            // overrun the tap's queue.
            std::thread::sleep(Duration::from_millis(2));
        }
        let mut got = Vec::new();
        let mut buf = [0u8; 2048];
        while let Ok(n) = listener.recv(&mut buf) {
            assert_eq!(n % 188, 0);
            got.extend(buf[..n].chunks(188).map(|c| c.to_vec()));
            if got.len() > 600 {
                break;
            }
        }
        rx.stop();
        assert!(got.len() > 600, "only {} packets", got.len());
        // In order, and each one sent.
        let mut next = 0;
        for g in &got {
            let i = packets[next..].iter().position(|p| p[..] == g[..]).expect("corrupt packet");
            next += i + 1;
        }
    }

    /// Real video through the whole chain: `HPSDR_DATV_TS_IN` (a .ts file,
    /// e.g. from ffmpeg) is modulated, received, and the stream that comes
    /// out written to `HPSDR_DATV_TS_OUT` for ffprobe/ffplay to check.
    #[test]
    #[ignore]
    fn real_transport_stream_round_trip() {
        let input = std::fs::read(std::env::var("HPSDR_DATV_TS_IN").unwrap()).unwrap();
        let packets: Vec<[u8; 188]> = input.chunks_exact(188).map(|c| c.try_into().unwrap()).collect();
        let fs = crate::mirisdr::WIDE_SAMPLE_RATE_HZ;
        let pls = PlsCode { modcod: 8, short: false, pilots: false };
        let ch = Channel { fs: fs as f64, symbol_rate: 1_500_000.0, rolloff: 0.35, carrier_hz: 3_000.0, clock_ppm: 10.0, esn0_db: 7.0, seed: 3 };
        let samples = waveform(&frames(&packets, pls), &ch);
        let listener = UdpSocket::bind("127.0.0.1:0").unwrap();
        listener.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let tap = WidebandTap::new(fs);
        let settings = DatvSettings {
            frequency_hz: 741_800_000,
            symbol_rate: 1_500_000,
            rolloff: 0.35,
            udp_addr: listener.local_addr().unwrap().to_string(),
        };
        let mut rx = DatvReceiver::start(Arc::clone(&tap), Arc::new(AtomicI64::new(0)), settings).unwrap();
        let reader = std::thread::spawn(move || {
            let mut out = Vec::new();
            let mut buf = [0u8; 2048];
            while let Ok(n) = listener.recv(&mut buf) {
                out.extend_from_slice(&buf[..n]);
            }
            out
        });
        for chunk in samples.chunks(32_768) {
            let block = chunk
                .iter()
                .map(|s| [(s.re * 32768.0).clamp(-32768.0, 32767.0) as i16, (s.im * 32768.0).clamp(-32768.0, 32767.0) as i16])
                .collect();
            tap.send(WidebandBlock { center_hz: 741_800_000, samples: block });
            std::thread::sleep(Duration::from_secs_f64(32_768.0 / fs as f64));
        }
        let out = reader.join().unwrap();
        eprintln!("{:?}", rx.status());
        rx.stop();
        std::fs::write(std::env::var("HPSDR_DATV_TS_OUT").unwrap(), &out).unwrap();
        eprintln!("in {} packets, out {}", packets.len(), out.len() / 188);
    }
}
