/*
    Minimal implementation of the TCI (Transceiver Control Interface)
    protocol -- an open WebSocket-based control protocol originally from
    Expert Electronics (ExpertSDR2/3), also supported by Thetis/
    OpenHPSDR and digital-mode software like JTDX. Listens on
    0.0.0.0:40001 by default (TCI's standard default port, but bound to
    all interfaces rather than just loopback) so a client on another
    machine on the network -- a remote-operating laptop, a tablet, a
    separate shack PC -- can connect too, not just software running on
    this same machine. This protocol has no authentication of its own,
    so anyone who can reach this port on the network can control the
    radio; only expose it on networks you trust.

    CONFIDENCE LEVEL, please read before trusting this blindly:
    - Command syntax (`name:arg1,arg2,...;`, reserved chars :,;) and the
      default port are confirmed from the official protocol README
      (github.com/ExpertSDR3/TCI).
    - The specific commands implemented here (vfo, modulation, trx) and
      their argument order are confirmed against JTDX's actual working
      TCI client implementation (TCITransceiver.cpp), not guessed.
    - The *initial handshake sequence* sent on connect (protocol name,
      then vfo/modulation/trx state) is my best-effort reconstruction of
      "server pushes current state on connect" per the README -- the
      full official handshake may include additional fields (device
      capabilities, channel counts, VFO limits, etc.) that aren't
      implemented here. If a TCI client fails to fully recognize this
      as a valid server, this is the first thing to check.
    - TCI mode-string spellings (LSB/USB/CW/etc.) are assumed similar to
      the equally-unverified-for-TCI-specifically ones used in rigctl.rs
      -- not confirmed against a TCI reference for exact spelling.
    - tungstenite's exact API surface (accept(), get_ref(), etc.) is
      from general knowledge of the crate, not verified against 0.29
      specifically -- same caveat as every other external-crate API in
      this project.
    - The binary streaming format (audio_start/audio_stop/iq_start/
      iq_stop, and the 64-byte header + LE f32 samples it triggers) is
      confirmed against rustyHPSDR's own TCI server
      (~/github/rustyHPSDR/src/tci/mod.rs) -- the strongest reference
      available here, since the user has it directly confirmed
      working against TCI Remote, the exact client this was written
      for. An earlier pass was based on github.com/ftl/tci (a clean
      independent Go client library) instead, which got several
      concrete details wrong once cross-checked against rustyHPSDR:
      Format=4 instead of the correct 3, no explicit `channels` field
      (rustyHPSDR always declares stereo, channels=2, as its own
      header field rather than folding it into reserved padding),
      9 reserved u32s instead of 8, `length` as total float count
      instead of frame-pair count, and I/Q samples in the naive I-then-Q
      order rather than rustyHPSDR's explicitly-swapped Q-then-I.
      rustyHPSDR also streams audio to any connected client
      unconditionally (no audio_start/audio_stop gate at all, only
      iq_start/iq_stop) -- matched here too (audio_streaming defaults
      to true), while still honoring audio_start/audio_stop if a
      client sends them, for compatibility with clients that expect
      that gate to exist. Thetis's own TCI server
      (TAPR/OpenHPSDR-Thetis/.../TCIServer.cs) turned out to have this
      as a literal `// todo !` stub, so wasn't usable as a reference at
      all.

    trx (PTT) flips RadioSession's mox flag -- same one the on-screen
    PTT button and rigctl's set_ptt use. See rigctl.rs's module note on
    the "armed but keyed with silence" gap if TX isn't enabled in
    Settings -> TX when a client sends trx:0,true;.

    Unlike rigctl.rs: TCI also streams RX audio (audio_start/stop) and
    raw wideband IQ (iq_start/stop, which is what lets a client render
    its own spectrum/waterfall -- TCI has no separate spectrum message
    type) to clients, as TCI Remote (an Android remote-listening app)
    needs both to be useful for anything beyond frequency/mode/PTT
    sync. See spectrum.rs's tci_audio_out/iq_out doc comments for
    where this data actually comes from -- dedicated taps, not shared
    with the existing local-playback/analyzer consumers of similar
    data, since a second reader of an already-consumed queue would
    steal samples from the first.

    TX audio (a client sending audio to the radio, e.g. for voice
    macros or a fully remote digital-mode setup) is implemented and
    confirmed working end-to-end against TCI Remote: PTT, TxChrono
    requests, and the TX_AUDIO_STREAM response are all exchanged
    correctly, and the received audio reaches WDSP's TXA chain at a
    proper level (verified via its own internal meters against a Tune
    baseline). One real-world quirk found along the way: TCI Remote's
    `length` header field does NOT follow the frame-pair convention
    this project's own outgoing messages use (see
    decode_binary_message's doc comment) -- incoming payload size is
    derived from the actual received byte count instead of trusting
    that field. Received TX audio lands in radio.rs's
    RadioSession::tci_tx_audio, which tx.rs's TXA loop prefers over the
    local mic_buffer on any chunk where it has data (see run()'s doc
    comment there) -- so local mic PTT keeps working completely
    unaffected whenever no TCI client is actively sending audio.
*/

use crate::spectrum::{DemodParams, Mode};
use std::collections::VecDeque;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use tungstenite::Message;

pub const DEFAULT_ADDR: &str = "0.0.0.0:40001";
const PROTOCOL_NAME: &str = "protocol:hpsdr-rs;";

/// A swappable reference to "whichever DemodParams is current right
/// now" -- see TciServer::set_demod_params's doc comment (and
/// rigctl.rs's identical DemodParamsCell, which this mirrors) for why
/// this indirection exists.
type DemodParamsCell = Arc<Mutex<Arc<Mutex<DemodParams>>>>;

/// The two dedicated streaming taps a SpectrumHandle exposes for TCI
/// specifically (spectrum.rs's tci_audio_out/iq_out -- NOT the same
/// queues local playback/other consumers use, see that module's doc
/// comments for why). Bundled together since they always come from
/// and get swapped together with the same SpectrumHandle.
#[derive(Clone)]
struct AudioIqTaps {
    audio: Arc<Mutex<VecDeque<f32>>>,
    iq: Arc<Mutex<VecDeque<(f32, f32)>>>,
}

/// Swappable the same way DemodParamsCell is -- see
/// TciServer::set_audio_iq's doc comment.
type AudioIqCell = Arc<Mutex<AudioIqTaps>>;

pub struct TciServer {
    demod_params: DemodParamsCell,
    audio_iq: AudioIqCell,
    stop: Arc<AtomicBool>,
    /// Count of currently-connected clients (normally 0 or 1, but the
    /// accept loop doesn't limit concurrent connections, so a counter
    /// is more correct than a bool if two ever overlap). Lets the UI
    /// show "listening, no client" vs. "client connected" separately
    /// from "not running at all" (server is None).
    connected: Arc<AtomicU32>,
    thread: Option<JoinHandle<()>>,
    /// Per-client handler threads -- stop() joins these too, not just
    /// the accept thread. handle_client already polls `stop` via its
    /// own read timeout (so it was never leaked indefinitely the way
    /// rigctl.rs's equivalent was before a matching fix), but without
    /// this a caller that immediately rebinds the same address (e.g.
    /// the user stopping/restarting this from Settings -> Network)
    /// could still race a client thread that hasn't quite exited yet.
    /// A sample-rate change no longer tears this server down at all
    /// anymore -- see set_demod_params.
    client_threads: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl TciServer {
    /// Starts listening in the background on `addr` (e.g.
    /// "0.0.0.0:40001", the default -- or "127.0.0.1:40001" to restrict
    /// to this machine only). Returns Err if the address is invalid or
    /// the port is already in use.
    pub fn start(
        addr: &str,
        frequency_hz: Arc<AtomicU32>,
        sample_rate: Arc<AtomicU32>,
        demod_params: Arc<Mutex<DemodParams>>,
        mox: Arc<AtomicBool>,
        tci_audio_out: Arc<Mutex<VecDeque<f32>>>,
        iq_out: Arc<Mutex<VecDeque<(f32, f32)>>>,
        tci_tx_audio: Arc<Mutex<VecDeque<f32>>>,
        tci_tx_gain: Arc<Mutex<f32>>,
    ) -> std::io::Result<Self> {
        let listener = TcpListener::bind(addr)?;
        listener.set_nonblocking(true)?;
        println!("tci: listening on {addr}");

        let demod_params: DemodParamsCell = Arc::new(Mutex::new(demod_params));
        let audio_iq: AudioIqCell = Arc::new(Mutex::new(AudioIqTaps { audio: tci_audio_out, iq: iq_out }));
        let stop = Arc::new(AtomicBool::new(false));
        let connected = Arc::new(AtomicU32::new(0));
        let client_threads: Arc<Mutex<Vec<JoinHandle<()>>>> = Arc::new(Mutex::new(Vec::new()));
        let accept_stop = Arc::clone(&stop);
        let accept_connected = Arc::clone(&connected);
        let accept_client_threads = Arc::clone(&client_threads);
        let accept_demod_params = Arc::clone(&demod_params);
        let accept_audio_iq = Arc::clone(&audio_iq);
        let accept_tci_tx_audio = Arc::clone(&tci_tx_audio);
        let accept_tci_tx_gain = Arc::clone(&tci_tx_gain);
        let thread = thread::spawn(move || {
            while !accept_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, peer)) => {
                        println!("tci: client connected from {peer}");
                        let freq = Arc::clone(&frequency_hz);
                        let rate = Arc::clone(&sample_rate);
                        let params = Arc::clone(&accept_demod_params);
                        let audio_iq = Arc::clone(&accept_audio_iq);
                        let tx_audio = Arc::clone(&accept_tci_tx_audio);
                        let tx_gain = Arc::clone(&accept_tci_tx_gain);
                        let conn_mox = Arc::clone(&mox);
                        let conn_stop = Arc::clone(&accept_stop);
                        let conn_connected = Arc::clone(&accept_connected);
                        let handle = thread::spawn(move || {
                            conn_connected.fetch_add(1, Ordering::Relaxed);
                            handle_client(
                                stream, freq, rate, params, audio_iq, tx_audio, tx_gain, conn_mox,
                                conn_stop,
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
            audio_iq,
            stop,
            connected,
            thread: Some(thread),
            client_threads,
        })
    }

    /// Points this server (and every currently-connected client's
    /// handler thread) at a different DemodParams -- see rigctl.rs's
    /// identical set_demod_params for the full explanation (this fixes
    /// the same reported "disconnects on sample rate change" bug for
    /// TCI clients too).
    pub fn set_demod_params(&self, new_demod_params: Arc<Mutex<DemodParams>>) {
        *self.demod_params.lock().unwrap() = new_demod_params;
    }

    /// Same idea as set_demod_params, for the streaming taps -- a
    /// SpectrumHandle recreated on a sample-rate change (main.rs's
    /// change_sample_rate) hands out entirely new audio_out/iq_out
    /// queues, so any client currently mid-stream needs pointing at
    /// the new ones or its stream would silently go dead.
    pub fn set_audio_iq(
        &self,
        new_audio_out: Arc<Mutex<VecDeque<f32>>>,
        new_iq_out: Arc<Mutex<VecDeque<(f32, f32)>>>,
    ) {
        *self.audio_iq.lock().unwrap() = AudioIqTaps { audio: new_audio_out, iq: new_iq_out };
    }

    /// True while at least one client is currently connected. Callers
    /// (the Network settings tab, the status indicator) use this to
    /// tell "listening but idle" apart from "actively in use".
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed) > 0
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Join client threads too -- see client_threads' doc comment.
        // Each should already be on its way out (its own read timeout
        // means it notices `stop` within ~250ms), this just makes sure
        // stop() doesn't return before that's actually happened.
        let threads: Vec<JoinHandle<()>> = self.client_threads.lock().unwrap().drain(..).collect();
        for t in threads {
            let _ = t.join();
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for TciServer {
    fn drop(&mut self) {
        self.stop();
    }
}

fn handle_client(
    stream: TcpStream,
    frequency_hz: Arc<AtomicU32>,
    sample_rate: Arc<AtomicU32>,
    demod_params: DemodParamsCell,
    audio_iq: AudioIqCell,
    tci_tx_audio: Arc<Mutex<VecDeque<f32>>>,
    tci_tx_gain: Arc<Mutex<f32>>,
    mox: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
) {
    let _ = stream.set_nodelay(true);

    let mut ws = match tungstenite::accept(stream) {
        Ok(ws) => ws,
        Err(e) => {
            eprintln!("tci: websocket handshake failed: {e}");
            return;
        }
    };

    // Poll frequently enough for real-time audio/IQ streaming to not
    // feel laggy (previously 250ms, fine for control-only but far too
    // coarse once this loop is also responsible for pushing streaming
    // data at roughly this same cadence -- see the streaming sends
    // below). Still just a read timeout, not a hard latency guarantee.
    if let Err(e) = ws.get_ref().set_read_timeout(Some(Duration::from_millis(20))) {
        eprintln!("tci: failed to set read timeout: {e}");
    }

    // Best-effort initial state push -- see module-level note.
    //
    // BUG FIX: this used to stop at trx:, missing the `start;`/`ready;`
    // handshake messages entirely. Confirmed against rustyHPSDR's own
    // TCI server (~/github/rustyHPSDR/src/tci/mod.rs, proven working
    // against real TCI clients): its init sequence explicitly sends
    // `"start;"` then `"ready;"` after the initial state. A real report
    // of WSJT-X connecting, streaming data successfully for a while,
    // then showing "TCI SDR is not switched on" is consistent with a
    // client-side state machine that's waiting for that `start;`
    // signal and eventually times out/gives up without it -- this
    // project never sent it at all. `device:` added alongside it since
    // it's part of the same reference sequence and cheap/harmless for
    // any client that reads it.
    let freq = frequency_hz.load(Ordering::Relaxed);
    let mode = demod_params.lock().unwrap().clone().lock().unwrap().mode;
    let _ = ws.send(Message::Text(PROTOCOL_NAME.into()));
    let _ = ws.send(Message::Text("device:hpsdr-rs;".into()));
    let _ = ws.send(Message::Text(format!("vfo:0,0,{freq};").into()));
    let _ = ws.send(Message::Text(format!("modulation:0,{};", mode_to_tci(mode)).into()));
    let _ = ws.send(Message::Text(
        format!("trx:0,{};", mox.load(Ordering::Relaxed)).into(),
    ));
    let _ = ws.send(Message::Text("start;".into()));
    let _ = ws.send(Message::Text("ready;".into()));

    // Per-client streaming state -- audio defaults to ON (see below),
    // IQ defaults to OFF, only enabled by an explicit iq_start. This
    // asymmetry matches rustyHPSDR's own TCI server (confirmed working
    // against TCI Remote): it streams audio to any connected client
    // unconditionally, with no audio_start/audio_stop gate at all, and
    // only gates iq_start/iq_stop. audio_start/audio_stop are still
    // handled below (see handle_command) in case a client does send
    // them -- harmless either way, and keeps this spec-compliant for
    // any other TCI client that relies on that gate.
    let mut audio_streaming = true;
    let mut iq_streaming = false;
    // See the periodic vfo/modulation/trx heartbeat below.
    let mut last_status_broadcast = Instant::now();
    // BUG FIX: TxChrono used to be sent unconditionally once per loop
    // iteration, on the assumption that the loop's own ~20ms read
    // timeout paced it -- true only while nothing is arriving to read.
    // A real WSJT-X capture (tcpdump, reassembled and decoded by hand)
    // showed this loop actually sending TxChrono and receiving
    // WSJT-X's near-instant reply back-to-back with ~12 MICROSECONDS
    // between messages once a session was underway -- ~80,000
    // messages/sec, versus the ~94/sec real-time playback actually
    // needs (TCI_TX_AUDIO_CHUNK/TCI_AUDIO_SAMPLE_RATE), because
    // ws.read() returns immediately whenever a response is already
    // waiting and never actually blocks for the timeout. This
    // explains a report of WSJT-X-driven TX (never local Tune, which
    // doesn't touch this queue at all) sounding wide/noisy and
    // collapsing to ~0W within 1-2 seconds regardless of any TX-audio-
    // content fix tried first: WSJT-X's own tone generator (its
    // internal sample counter advances one TxChrono-response's worth
    // per call, not per elapsed wall-clock time) gets fast-forwarded
    // through its entire programmed duration in a couple of seconds
    // instead of the real tens of seconds, running into its own
    // tail/Idle-state handling (a separate, already-diagnosed WSJT-X
    // bug -- see decode_binary_message's doc comment) almost
    // immediately, while tci_tx_audio's bounded queue on this side
    // gets so overrun (~860x the real-time rate) that its capacity
    // trim is constantly discarding samples, feeding tx.rs's
    // real-time-paced consumer a decimated, discontinuous fraction of
    // whatever WSJT-X actually generated. Real, explicit pacing here
    // (absolute-deadline, same pattern as tx.rs's own next_chunk --
    // see its doc comment for why relative sleep-based pacing isn't
    // used) rather than leaning on the read timeout fixes the request
    // rate regardless of how fast a client replies.
    let tx_chrono_interval = Duration::from_secs_f64(
        TCI_TX_AUDIO_CHUNK as f64 / TCI_AUDIO_SAMPLE_RATE as f64,
    );
    let mut next_tx_chrono = Instant::now();
    let mut mox_was_active = false;

    while !stop.load(Ordering::Relaxed) {
        match ws.read() {
            Ok(Message::Text(text)) => {
                for cmd in text.split(';') {
                    let cmd = cmd.trim();
                    if cmd.is_empty() {
                        continue;
                    }
                    // Resolved fresh on every command (not once per
                    // connection) so a sample-rate change mid-session --
                    // see set_demod_params -- takes effect immediately
                    // for already-connected clients too, not just new
                    // ones.
                    let current_params = demod_params.lock().unwrap().clone();
                    if let Some(response) = handle_command(
                        cmd,
                        &frequency_hz,
                        &current_params,
                        &mox,
                        &mut audio_streaming,
                        &mut iq_streaming,
                    ) {
                        if ws.send(Message::Text(response.into())).is_err() {
                            return;
                        }
                    }
                }
            }
            Ok(Message::Close(_)) => break,
            Ok(Message::Binary(data)) => {
                // A client sending TX audio -- see decode_binary_message's
                // doc comment for the confidence caveat on this whole
                // exchange (no confirmed-working reference for the
                // server side of it, unlike RX audio/IQ above).
                if let Some((msg_type, samples)) = decode_binary_message(&data) {
                    if msg_type == BinaryMessageType::TxAudioStream as u32 {
                        // Stereo -> mono: simple L/R average. No
                        // existing precedent to match here (audio.rs's
                        // MicInput requests mono directly from cpal
                        // rather than downmixing stereo in software).
                        //
                        // Sanity-check each pair before mixing: a real
                        // WSJT-X test (see decode_binary_message's doc
                        // comment) showed every 8th TxAudioStream
                        // message periodically containing astronomical
                        // (~1e27+) garbage values on WSJT-X's own side
                        // -- likely a stale/uninitialized buffer region
                        // on its end, not a framing bug here (message
                        // size and payload offset were identical on
                        // good and bad messages alike).
                        //
                        // DROPPING a bad pair, not muting it to 0.0 --
                        // an earlier version of this substituted hard
                        // silence instead, on the reasoning that WDSP's
                        // stateful TXA chain (AGC, IIR filters) would
                        // ring/saturate from a single insane sample.
                        // That didn't fix a real report of wide/noisy
                        // WSJT-X-only TX spectrum with power stuck near
                        // 0W and ALC pinned heavily negative regardless
                        // of drive level -- a real A/B test (same TCI
                        // session, TX audio source switched to the
                        // radio's own mic input, bypassing this queue
                        // entirely) confirmed the fault really is in
                        // this TCI audio content path, not PTT/mox
                        // sequencing. Forcing silence, THEN jumping back
                        // to real audio, repeated every 8th message for
                        // the WHOLE transmission, is itself a periodic
                        // discontinuity an ALC/leveler never gets to
                        // settle past -- dropping instead just shrinks
                        // this cycle's contribution to the queue by a
                        // pair, seamless as long as the queue has any
                        // headroom (normal case; the existing capacity
                        // trim above already handles genuine underrun).
                        // Applied to the mixed-but-not-yet-gained sample,
                        // so gain doesn't turn a legitimately-quiet good
                        // pair into a false positive -- see RadioSession::
                        // tci_tx_gain's doc comment for why gain is a
                        // separate control from tx.rs's mic_gain.
                        let gain = *tci_tx_gain.lock().unwrap();
                        let mut q = tci_tx_audio.lock().unwrap();
                        let capacity = q.capacity(); // matches radio.rs's TCI_TX_AUDIO_CAPACITY
                        for pair in samples.chunks_exact(2) {
                            let (l, r) = (pair[0], pair[1]);
                            if !l.is_finite() || !r.is_finite() || l.abs() > 2.0 || r.abs() > 2.0 {
                                continue;
                            }
                            if q.len() >= capacity {
                                q.pop_front();
                            }
                            q.push_back((l + r) * 0.5 * gain);
                        }
                    }
                }
            }
            Ok(_) => {} // ping/pong -- ignored for now
            Err(tungstenite::Error::Io(ref e))
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                // Falls through to the streaming sends below rather
                // than `continue`ing past them -- a read timing out
                // is the NORMAL case for most of this loop's life
                // (nothing to read most of the time), and streaming
                // needs to keep flowing on every tick regardless of
                // whether a text command happened to arrive this
                // cycle.
            }
            Err(_) => break,
        }

        // Streaming taps re-resolved fresh each tick (not once per
        // connection) for the same reason current_params is above --
        // see TciServer::set_audio_iq's doc comment: a sample-rate
        // change mid-session hands out new queues, and an
        // already-streaming client needs to follow them, not keep
        // draining an abandoned queue that will never get new data
        // again.
        let taps = audio_iq.lock().unwrap().clone();

        if audio_streaming {
            let samples: Vec<f32> = {
                let mut q = taps.audio.lock().unwrap();
                q.drain(..).collect()
            };
            if !samples.is_empty() {
                // Mono -> stereo: TCI's audio format is stereo (both
                // the reference client library and rustyHPSDR's own
                // confirmed-working server always declare channels=2);
                // this project's RX audio is mono, so duplicate each
                // sample to both channels.
                let stereo: Vec<(f32, f32)> = samples.into_iter().map(|s| (s, s)).collect();
                // BUG FIX: `length` here used to be stereo.len() (frame-PAIR
                // count), matching rustyHPSDR's own convention (confirmed
                // working against TCI Remote). A real report of WSJT-X's
                // waterfall/decode looking compressed/stretched over TCI
                // audio led to checking github.com/ftl/tci (an independent
                // Go client library) directly: its ParseBinaryMessage reads
                // `data = make([]float32, msg.DataLength)`, i.e. DataLength
                // is the RAW FLOAT COUNT (both channels included), not a
                // frame-pair count -- exactly half of what this project was
                // sending. A client following that convention (apparently
                // WSJT-X, unlike TCI Remote) would read only half the
                // intended samples per packet as "the whole chunk", which
                // is exactly the timing distortion reported. Only the
                // announced `length` value changes here -- the actual
                // payload (`&stereo`) and its real sample count are
                // untouched, so this doesn't affect TCI Remote's own IQ
                // streaming (still frame-pair count below, unconfirmed
                // either way and not reported broken) or the real audio
                // content itself.
                let msg = encode_binary_message(
                    0,
                    TCI_AUDIO_SAMPLE_RATE,
                    BinaryMessageType::RxAudioStream,
                    stereo.len() as u32 * 2,
                    &stereo,
                );
                if ws.send(Message::Binary(msg.into())).is_err() {
                    return;
                }
            }
        }

        if iq_streaming {
            let pairs: Vec<(f32, f32)> = {
                let mut q = taps.iq.lock().unwrap();
                q.drain(..).collect()
            };
            if !pairs.is_empty() {
                // Q before I -- confirmed against rustyHPSDR's own
                // working IQ streaming code, which swaps this
                // explicitly ("SWAP: Push Q then I"), not I before Q
                // as would be the naive/obvious order.
                let swapped: Vec<(f32, f32)> = pairs.into_iter().map(|(i, q)| (q, i)).collect();
                let msg = encode_binary_message(
                    0,
                    sample_rate.load(Ordering::Relaxed),
                    BinaryMessageType::IqStream,
                    swapped.len() as u32,
                    &swapped,
                );
                if ws.send(Message::Binary(msg.into())).is_err() {
                    return;
                }
            }
        }

        // TxChrono -- requests TX audio from this client while
        // transmitting. Confirmed working end-to-end against TCI Remote,
        // requesting TCI_TX_AUDIO_CHUNK samples -- chosen to match
        // tx.rs's own TX_BUFFER_SIZE. Real-time paced now -- see
        // tx_chrono_interval's doc comment above for why sending on
        // every loop tick (relying on the read timeout for pacing) was
        // wrong. A client that doesn't implement TX audio simply won't
        // respond to this; harmless either way.
        //
        // BUG FIX: pure fixed-interval pacing alone still let
        // tci_tx_audio slowly drain over a real, multi-second WSJT-X
        // transmission (a real trace showed the "mic buffer underrun"
        // rate climbing from ~7% to ~20% over a few seconds of steady
        // Tune) -- expected, not mysterious, once you account for
        // WSJT-X's own confirmed ~12.5% corrupted-message rate (one of
        // its 8 ring-buffer slots, dropped by decode_binary_message's
        // sanity check): requesting at exactly the real-time rate with
        // ~87.5% of replies actually usable means supply is
        // structurally ~12.5% short of consumption, a deficit that
        // only grows the longer a transmission runs. Rather than
        // baking in a fixed compensation percentage (fragile, and
        // risks recreating the original runaway-request bug above if
        // the real loss rate is ever lower than assumed), this reacts
        // to the queue's actual occupancy: below a small low-water
        // mark, request immediately (resyncing the schedule from now)
        // instead of waiting for the next tick, letting the queue
        // catch up at whatever rate WSJT-X actually replies; once
        // healthy again, it drops straight back to strict real-time
        // pacing. Self-limiting either way -- catch-up requests are
        // still gated one-per-loop-iteration by this same check, so
        // this can't reproduce the original unpaced-runaway behavior.
        let mox_active = mox.load(Ordering::Relaxed);
        if mox_active && !mox_was_active {
            // Resync on every fresh PTT rather than sending a burst of
            // "overdue" requests built up while idle -- same reasoning
            // as tx.rs's own next_chunk resync on mox going active.
            next_tx_chrono = Instant::now();
        }
        mox_was_active = mox_active;
        if mox_active {
            let queue_low = tci_tx_audio.lock().unwrap().len() < TX_CHRONO_LOW_WATERMARK;
            if queue_low || Instant::now() >= next_tx_chrono {
                next_tx_chrono = if queue_low {
                    Instant::now() + tx_chrono_interval
                } else {
                    next_tx_chrono + tx_chrono_interval
                };
                let chrono = encode_binary_message(
                    0,
                    TCI_AUDIO_SAMPLE_RATE,
                    BinaryMessageType::TxChrono,
                    TCI_TX_AUDIO_CHUNK,
                    &[],
                );
                if ws.send(Message::Binary(chrono.into())).is_err() {
                    return;
                }
            }
        }

        // BUG FIX: vfo/modulation/trx state was previously only ever
        // sent once at connect (or in direct reply to a client's own
        // command) -- never reasserted afterward. A real report of
        // WSJT-X reporting "TCI failed set mode" consistently ~2s after
        // PTT engages, despite the initial trx:0,true;/modulation
        // exchange completing correctly (confirmed via a real capture:
        // WSJT-X received a proper reply to both), is consistent with a
        // client-side staleness check that expects to keep seeing
        // confirmation of the current state, not just a one-time ack.
        // Resent every second here (piggybacking on this loop's
        // existing ~20ms tick) as a low-risk heartbeat -- harmless for
        // clients that don't need it (TCI Remote/rustyHPSDR never
        // solicited this either, and ignore unsolicited state messages
        // they don't ask for).
        if last_status_broadcast.elapsed() >= Duration::from_secs(1) {
            let freq = frequency_hz.load(Ordering::Relaxed);
            let mode = demod_params.lock().unwrap().clone().lock().unwrap().mode;
            let mox_on = mox.load(Ordering::Relaxed);
            let _ = ws.send(Message::Text(format!("vfo:0,0,{freq};").into()));
            let _ =
                ws.send(Message::Text(format!("modulation:0,{};", mode_to_tci(mode)).into()));
            if ws
                .send(Message::Text(format!("trx:0,{mox_on};").into()))
                .is_err()
            {
                return;
            }
            last_status_broadcast = Instant::now();
        }
    }
}

/// Matches tx.rs's own TX_BUFFER_SIZE -- see the TxChrono comment
/// above for why this is what gets requested per chunk.
const TCI_TX_AUDIO_CHUNK: u32 = 512;

/// Below this many buffered samples, the TxChrono pacing loop above
/// requests immediately instead of waiting for its next scheduled
/// tick -- see that comment for why real-time pacing alone still lets
/// the queue drain over a long transmission. Two chunks' worth: small
/// enough to only kick in on genuine, sustained shortfall (a single
/// dropped/corrupted reply is one chunk, ~10.7ms), not on ordinary
/// per-chunk timing jitter.
const TX_CHRONO_LOW_WATERMARK: usize = TCI_TX_AUDIO_CHUNK as usize * 2;

/// RX audio's fixed output rate -- must match spectrum.rs's own
/// OUTPUT_RATE (not imported directly since that constant is private
/// to that module and this is the only other place that needs it).
/// Lines up exactly with TCI's own AudioSampleRate48k, one of only
/// four rates (8k/12k/24k/48k) the protocol defines for audio.
const TCI_AUDIO_SAMPLE_RATE: u32 = 48_000;

/// TCI's binary streaming message types -- numeric values confirmed
/// against BOTH github.com/ftl/tci and rustyHPSDR's own TCIStreamType
/// enum (rustyHPSDR's is the stronger reference here: it's confirmed
/// actually working against TCI Remote, the exact client this was
/// written for). TxAudioStream/TxChrono (a client sending TX audio
/// back to the radio) are BEST-EFFORT, not confirmed the way the RX
/// side is -- see decode_binary_message's and the TxChrono call
/// site's doc comments.
#[derive(PartialEq)]
enum BinaryMessageType {
    IqStream = 0,
    RxAudioStream = 1,
    TxAudioStream = 2,
    TxChrono = 3,
}

/// Encodes one TCI binary streaming message: a fixed 64-byte
/// little-endian header, then `frames` interleaved sample pairs as
/// little-endian f32 (so `frames.len()` samples per channel, 2*
/// `frames.len()` floats total). Header layout and field values
/// confirmed against rustyHPSDR's own TCI server
/// (~/github/rustyHPSDR/src/tci/mod.rs), which is proven working
/// against TCI Remote -- this replaced an earlier version based on
/// github.com/ftl/tci (a third-party client library, not confirmed
/// against this specific app) that had it subtly wrong: Format=4
/// instead of 3, no explicit `channels` field (rustyHPSDR always
/// declares stereo, channels=2), 9 reserved u32s instead of 8, and
/// `length` as total float count instead of frame-pair count.
/// `trx` is always 0 here -- rigctl/TCI only ever expose the primary
/// receiver (see spawn_extra_receiver and TciServer's own module
/// note), so there's no second index to send. `length` is passed
/// explicitly rather than derived from `frames.len()` so a TxChrono
/// message (no payload bytes at all, per the one confirmed detail
/// found on this exchange -- see ftl/tci's own decode skipping
/// payload parsing for this type) can still carry a requested-sample-
/// count hint in the header.
fn encode_binary_message(
    trx: u32,
    sample_rate: u32,
    msg_type: BinaryMessageType,
    length: u32,
    frames: &[(f32, f32)],
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(64 + frames.len() * 8);
    buf.extend_from_slice(&trx.to_le_bytes());
    buf.extend_from_slice(&sample_rate.to_le_bytes());
    buf.extend_from_slice(&3u32.to_le_bytes()); // Format: float32
    buf.extend_from_slice(&0u32.to_le_bytes()); // Codec: uncompressed
    buf.extend_from_slice(&0u32.to_le_bytes()); // CRC: unused
    buf.extend_from_slice(&length.to_le_bytes()); // frame-pair count, not float count
    buf.extend_from_slice(&(msg_type as u32).to_le_bytes());
    buf.extend_from_slice(&2u32.to_le_bytes()); // channels: always stereo
    buf.extend_from_slice(&[0u8; 32]); // Reserved: 8 * u32
    for (a, b) in frames {
        buf.extend_from_slice(&a.to_le_bytes());
        buf.extend_from_slice(&b.to_le_bytes());
    }
    buf
}

/// Inverse of encode_binary_message -- parses the 64-byte header and
/// returns (Type, samples), where `samples` is the raw float32 payload
/// (still interleaved, e.g. stereo L,R,L,R for TxAudioStream; caller
/// downmixes). Returns None if the buffer is shorter than the header.
///
/// CORRECTED after real-world testing against TCI Remote: the header's
/// `length` field is NOT used to determine how many floats to read.
/// An earlier version derived the expected float count from `length`
/// (assuming it meant frame-pairs, matching this project's own
/// *outgoing* convention, confirmed against rustyHPSDR) and rejected
/// the frame if the payload didn't match -- but real frames from TCI
/// Remote were rejected this way (16448 bytes = 64-byte header +
/// 16384-byte payload = exactly 4096 f32s / 2048 stereo pairs, yet
/// still didn't satisfy that check), meaning this client's `length`
/// doesn't follow the same frame-pair convention for data it SENDS
/// (whatever it actually means here isn't confirmed). Rather than
/// guess at yet another `length` semantic, this now derives the float
/// count directly from the actual received payload size instead --
/// unambiguous, self-describing, and doesn't depend on a convention
/// that's turned out to differ by direction/client.
fn decode_binary_message(data: &[u8]) -> Option<(u32, Vec<f32>)> {
    if data.len() < 64 {
        return None;
    }
    let msg_type = u32::from_le_bytes(data[24..28].try_into().ok()?);
    // format/channels (bytes 8-12/28-32) are NOT validated against what
    // this project's own encode_binary_message assumes (float32, 2
    // interleaved channels) -- confirmed harmless to skip: a real
    // packet capture of WSJT-X's own TxAudioStream messages showed its
    // declared `channels` field is just unwritten reserved padding
    // (a different garbage value every connection, changing per 8-
    // message cycle in step with WSJT-X's own internal ring-buffer
    // reuse -- see the sanity-check below), not a real field, and
    // `format` isn't otherwise in question (payload starts at the same
    // fixed 64-byte offset regardless).
    let payload = &data[64..];
    let num_floats = payload.len() / 4; // drops any trailing partial float, if ever present
    let samples = payload[..num_floats * 4]
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
        .collect();
    Some((msg_type, samples))
}

/// Returns Some(response-to-send) for recognized commands, or None to
/// send nothing back -- including for unrecognized commands, since the
/// protocol itself says invalid commands should just be ignored.
fn handle_command(
    cmd: &str,
    frequency_hz: &Arc<AtomicU32>,
    demod_params: &Arc<Mutex<DemodParams>>,
    mox: &Arc<AtomicBool>,
    audio_streaming: &mut bool,
    iq_streaming: &mut bool,
) -> Option<String> {
    let mut parts = cmd.splitn(2, ':');
    let name = parts.next().unwrap_or("").to_lowercase();
    let args: Vec<&str> = parts.next().unwrap_or("").split(',').collect();

    match name.as_str() {
        // vfo:receiver,vfo,frequency;
        "vfo" => {
            let f = args.get(2)?.parse::<f64>().ok()?;
            let f = f.round().max(0.0) as u32;
            frequency_hz.store(f, Ordering::Relaxed);
            Some(format!(
                "vfo:{},{},{};",
                args.first().unwrap_or(&"0"),
                args.get(1).unwrap_or(&"0"),
                f
            ))
        }
        // modulation:receiver,mode;
        "modulation" => {
            let requested = *args.get(1)?;
            let mode = tci_to_mode(requested)?;
            demod_params.lock().unwrap().mode = mode;
            // BUG FIX: this used to echo back mode_to_tci(mode) (this
            // project's own fixed-case convention, e.g. "USB") instead
            // of the case the client actually sent (e.g. "usb"). A real
            // report showed WSJT-X retrying modulation:0,usb; three
            // times in a row right after PTT, each time getting our
            // reply back correctly PARSED but in a different case, then
            // giving up with "TCI failed set mode" -- consistent with a
            // literal string comparison on the client side rather than
            // case-insensitive parsing. Echoing the exact string
            // received guarantees a match regardless of what case
            // convention any given client uses, at no cost (the parsed
            // `mode` value -- what actually matters -- is identical
            // either way).
            Some(format!("modulation:{},{};", args.first().unwrap_or(&"0"), requested))
        }
        // trx:receiver,state;
        "trx" => {
            let on = matches!(args.get(1), Some(&"true") | Some(&"1"));
            mox.store(on, Ordering::Relaxed);
            Some(format!("trx:{},{};", args.first().unwrap_or(&"0"), on))
        }
        // audio_start:receiver; / audio_stop:receiver; -- receiver index
        // itself is ignored (only the primary receiver is ever exposed
        // here, see this file's module note), so there's nothing to
        // distinguish.
        //
        // BUG FIX: these used to return None (no reply) on the theory
        // that they're fire-and-forget commands, not requests -- true
        // for rustyHPSDR's own TCI server (confirmed working against
        // TCI Remote, which has no audio_start handler at all and never
        // replies to iq_start either). WSJT-X's own TCI client is
        // stricter: a real report of "TCI Audio could not be switched
        // on" after WSJT-X sends audio_start is consistent with it
        // waiting for the same echoed confirmation every OTHER command
        // here already sends (vfo/modulation/trx all echo back what was
        // set) and giving up without one. Echoing the command back,
        // matching that existing convention, costs nothing for clients
        // that don't need it (TCI Remote/rustyHPSDR ignore replies to
        // commands they didn't ask a question with).
        "audio_start" => {
            *audio_streaming = true;
            Some(format!("audio_start:{};", args.first().unwrap_or(&"0")))
        }
        "audio_stop" => {
            *audio_streaming = false;
            Some(format!("audio_stop:{};", args.first().unwrap_or(&"0")))
        }
        // iq_start:receiver; / iq_stop:receiver; -- same reasoning as
        // audio_start/stop above.
        "iq_start" => {
            *iq_streaming = true;
            Some(format!("iq_start:{};", args.first().unwrap_or(&"0")))
        }
        "iq_stop" => {
            *iq_streaming = false;
            Some(format!("iq_stop:{};", args.first().unwrap_or(&"0")))
        }
        _ => None,
    }
}

// BUG FIX: these were uppercase ("USB" etc). Confirmed via WSJT-X's own
// TCITransceiver.cpp (Transceiver/TCITransceiver.cpp, Cmd_Mode handler):
// it only lowercases an incoming modulation value when `device:` was
// exactly "Thetis" or "ExpertSDR3" (an internal ESDR3/HPSDR flag pair
// set from the Cmd_Version handler) -- any other device string (this
// project sends "hpsdr-rs") takes the plain `mode_ = args.at(1);`
// branch, no case-folding. Meanwhile WSJT-X's own OUTGOING/desired mode
// (`map_mode()`) is unconditionally lowercase ("usb" etc). If this
// project's own CONNECT-TIME handshake sent uppercase, WSJT-X's
// internal `mode_` would end up uppercase while its `requested_mode_`
// stays lowercase -- guaranteeing a mismatch the moment Tune/TX starts,
// which triggers WSJT-X's own confirmed race-prone blocking mode-set
// exchange (do_frequency, TCITransceiver.cpp ~line 1155) and a real
// chance of "TCI failed set mode". Sending lowercase everywhere (this
// project's own handshake, heartbeat, and command replies) means
// WSJT-X's `mode_` already matches `requested_mode_` before Tune is
// ever pressed, avoiding that exchange entirely rather than trying to
// win its race.
fn mode_to_tci(mode: Mode) -> &'static str {
    match mode {
        Mode::Lsb => "lsb",
        Mode::Usb => "usb",
        Mode::Dsb => "dsb",
        Mode::Cwl | Mode::Cwu => "cw",
        Mode::Fmn => "nfm",
        Mode::Am => "am",
        Mode::Digu => "digu",
        Mode::Digl => "digl",
        Mode::Sam => "sam",
        Mode::Drm => "am",
        Mode::Spec => "usb",
    }
}

fn tci_to_mode(s: &str) -> Option<Mode> {
    match s.to_uppercase().as_str() {
        "LSB" => Some(Mode::Lsb),
        "USB" => Some(Mode::Usb),
        "DSB" => Some(Mode::Dsb),
        "CW" | "CWR" => Some(Mode::Cwu),
        "NFM" | "FM" | "WFM" => Some(Mode::Fmn),
        "AM" => Some(Mode::Am),
        "DIGU" | "PKTUSB" => Some(Mode::Digu),
        "DIGL" | "PKTLSB" => Some(Mode::Digl),
        "SAM" => Some(Mode::Sam),
        _ => None,
    }
}
