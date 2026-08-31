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
      (github.com/ExpertSDR3/TCI), and since then directly against
      Expert Electronics' own "TCI Protocol Ver. 2.0" PDF (12 Jan 2024)
      -- the authoritative spec, not a third-party reconstruction.
    - The specific commands implemented here (vfo, modulation, trx,
      audio_start/stop, iq_start/stop, rit_offset, rit_enable,
      xit_offset, xit_enable) and their argument order are confirmed
      against BOTH that spec and JTDX's actual working TCI client
      implementation (TCITransceiver.cpp) -- except rit/xit, whose
      argument order (receiver,value/bool) is inferred from this
      project's own already-existing initial-state push for these same
      commands (see handle_client), not independently re-verified
      against JTDX specifically.
    - The *initial handshake sequence* sent on connect now includes
      every Initialization command the spec defines (section 4.1):
      protocol, device, vfo_limits, trx_count, channels_count,
      receive_only, modulations_list, plus vfo/modulation/trx state and
      start;/ready; (semicolon-terminated, start before ready -- CONFIRMED
      against a real packet capture of actual Thetis wire traffic, see
      handle_client's own doc comment at the send site for the full story
      of why a written protocol reference claiming otherwise turned out
      to be wrong here), and if_limits (a fixed -96000..96000 Hz range --
      no real IF-shift-within-panorama concept implemented here to report
      an exact one for, but a static "wide enough" range is harmless and
      this field is documented as accepted-and-discarded by TCI Remote
      regardless).
    - TCI mode-string spellings (LSB/USB/CW/etc.) match the spec's own
      MODULATIONS_LIST example and are cross-checked against JTDX/
      WSJT-X source for exact case-sensitivity behavior (see
      mode_to_tci's doc comment).
    - trx's optional 3rd argument (signal source: tci/mic1/mic2/micPC/
      ecoder2, spec section 4.2) is now parsed -- see tci_wants_mic's
      doc comment in radio.rs for what it does and, importantly, what
      it deliberately does NOT do (flip Auto's default when arg3 is
      absent, which would break TCI Remote's confirmed-working TX audio
      path -- that client never sends arg3 at all).
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

use crate::debug_log::DebugLog;
use crate::spectrum::{Agc, DemodParams, Mode, NoiseBlanker, NoiseReduction, passband_for};
use std::collections::VecDeque;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use tungstenite::Message;

pub const DEFAULT_ADDR: &str = "0.0.0.0:40001";
/// PROTOCOL:program-name,protocol-version; -- confirmed against the
/// official TCI Protocol spec (v2.0, section 4.1): arg1 is the program
/// name, arg2 the TCI protocol version implemented.
///
/// BUG FIX: previously sent this project's own name ("protocol:
/// hpsdr-rs,1.9;"), on the reasonable-looking assumption that a
/// client wouldn't care what a server calls itself. Root-caused a
/// real report of WSJT-X-over-TCI TX audio being consistently
/// splattered/uncopyable regardless of any audio-content fix tried
/// (gain, clamping, interpolating around WSJT-X's own known ring-
/// buffer corruption -- all real, all necessary, none sufficient) via
/// a controlled A/B on completely different hardware/software (Thetis
/// on a separate laptop, same WSJT-X): Thetis's native "protocol:
/// Thetis,2.0;" greeting produces the exact same broken TX audio;
/// switching on Thetis's own "Emulate ExpertSDR3 protocol" option --
/// confirmed by reading Thetis's TCIServer.cs to change nothing but
/// this one greeting string, to "protocol:ExpertSDR3,2.0;" -- made TX
/// audio clean immediately, confirmed via PSKReporter spots and a
/// clean waterfall on a separate receiving radio. WSJT-X's TCI client
/// evidently gates its own TX-audio handling on recognizing
/// "ExpertSDR3" specifically as the declared server, not on anything
/// about the actual wire content. Mirrors Thetis's exact known-good
/// string (including its "2.0" version claim) rather than this
/// project's own real implemented-command-set version, since the
/// point is to be recognized as ExpertSDR3-compatible, not to
/// accurately self-describe.
const PROTOCOL_MESSAGE: &str = "protocol:ExpertSDR3,2.0;";

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

/// One connected client's own private audio/IQ queue -- REPLACES the
/// earlier design where every client drained the SAME shared AudioIqCell
/// directly. ROOT CAUSE FIX for a real report ("intermittent/bouncing
/// audio" with 2 clients connected, and separately, a healthy/actively-
/// streaming client getting killed within milliseconds by a second
/// connection arriving -- confirmed via tci_log.txt timestamps, and by
/// the user's own observation that starting "TCI Remote Compactor"
/// opens two connections immediately): this project used to paper over
/// the queue-sharing problem with single-client "supersede" enforcement
/// (kill whichever client was already connected the instant a new TCP
/// connection arrived), which piHPSDR-style server behavior never
/// required and which an authoritative TCI Remote/Compactor protocol
/// reference the user found explicitly says is the wrong model ("the
/// server must broadcast state changes to all connected clients").
/// Worse, the supersede logic didn't even require the NEW connection to
/// finish its own WebSocket handshake first -- any raw TCP accept()
/// killed the previous (possibly perfectly healthy) client -- and
/// separately, `handle_client`'s own "clear stale queue on connect"
/// step cleared the SHARED queue out from under an already-connected
/// client too. Giving each client its own queue, fed by a single
/// dedicated broadcaster (spawn_audio_iq_broadcaster) that's the ONLY
/// reader of the real shared taps, removes the need for any of that:
/// clients can now come and go freely with no cross-talk.
struct ClientAudioIqSink {
    audio: Mutex<VecDeque<f32>>,
    iq: Mutex<VecDeque<(f32, f32)>>,
}

/// Registry of currently-connected clients' own sinks, as Weak refs so a
/// client that disconnects (normally, or via an early return/panic on
/// any of handle_client's several exit paths) is automatically dropped
/// from the fan-out on the broadcaster's next tick, with no explicit
/// unregister bookkeeping needed at each exit point.
type ClientAudioIqRegistry = Arc<Mutex<Vec<Weak<ClientAudioIqSink>>>>;

/// Caps how much unconsumed audio/IQ a single client's own sink can
/// accumulate before the oldest data gets dropped -- protects memory if
/// one client's connection stalls (slow network, blocked send) while
/// others keep streaming normally. Matches the existing shared-tap
/// queues' own "drop oldest once full" behavior (see spectrum.rs's
/// tci_audio_out/iq_out doc comments), just enforced per-client now
/// that each client has its own queue instead of sharing one. ~1s of
/// audio at 48kHz; the IQ figure is generous headroom across every
/// sample rate this app supports (up to 1.536Msps -- see main.rs's rate
/// list -- would still buffer over 100ms before dropping).
const CLIENT_SINK_MAX_AUDIO_SAMPLES: usize = 48_000;
const CLIENT_SINK_MAX_IQ_PAIRS: usize = 200_000;

/// Max audio/IQ frame-pairs sent in a single WebSocket binary message.
/// A client's sink can legitimately accumulate a large backlog -- e.g.
/// it hasn't sent iq_start yet (the broadcaster fills every registered
/// sink regardless), or its socket briefly stalled -- and draining that
/// whole backlog into ONE message produces frames far bigger than
/// anything seen in normal steady-state streaming. Confirmed via a real
/// packet capture (tcpdump + a hand-rolled TCP/WS reassembly, tshark
/// unavailable) of a TCI Remote Compactor probe connection: the very
/// first IQ send after iq_start was a single 1,600,064-byte frame (the
/// full 200,000-pair backlog cap), which the client's own WebSocket
/// library closed the connection over with code 1009 "Message Too Big"
/// (its default max is 1MiB) -- and matches this file's own earlier
/// doc comment (see the IQ send site below) describing the Compactor
/// re-sending iq_start every few hundred ms and eventually giving up,
/// exactly the symptom an oversized first frame would cause against a
/// client enforcing a smaller limit. Chunking every drain to this size
/// keeps every message close to what normal steady-state ticks already
/// produce (seen fine in the same capture up to ~9,000 pairs/message)
/// regardless of how large the backlog was.
const MAX_AUDIO_SAMPLES_PER_MESSAGE: usize = 4096;
const MAX_IQ_PAIRS_PER_MESSAGE: usize = 4096;

/// The single reader of the real shared audio/IQ taps -- drains them
/// and fans a clone of each batch out to every currently-registered
/// client's own sink, at the same ~20ms cadence handle_client's own
/// read-timeout loop already used for streaming (so this doesn't change
/// end-to-end latency). See ClientAudioIqSink's own doc comment for why
/// this replaced N client threads each draining the shared taps
/// directly.
fn spawn_audio_iq_broadcaster(
    audio_iq: AudioIqCell,
    registry: ClientAudioIqRegistry,
    stop: Arc<AtomicBool>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(20));
            let taps = audio_iq.lock().unwrap().clone();
            let audio_batch: Vec<f32> = {
                let mut q = taps.audio.lock().unwrap();
                q.drain(..).collect()
            };
            let iq_batch: Vec<(f32, f32)> = {
                let mut q = taps.iq.lock().unwrap();
                q.drain(..).collect()
            };
            if audio_batch.is_empty() && iq_batch.is_empty() {
                continue;
            }
            let mut clients = registry.lock().unwrap();
            clients.retain(|w| w.strong_count() > 0);
            for sink in clients.iter().filter_map(|w| w.upgrade()) {
                if !audio_batch.is_empty() {
                    let mut q = sink.audio.lock().unwrap();
                    q.extend(audio_batch.iter().copied());
                    while q.len() > CLIENT_SINK_MAX_AUDIO_SAMPLES {
                        q.pop_front();
                    }
                }
                if !iq_batch.is_empty() {
                    let mut q = sink.iq.lock().unwrap();
                    q.extend(iq_batch.iter().copied());
                    while q.len() > CLIENT_SINK_MAX_IQ_PAIRS {
                        q.pop_front();
                    }
                }
            }
        }
    })
}

pub struct TciServer {
    demod_params: DemodParamsCell,
    audio_iq: AudioIqCell,
    stop: Arc<AtomicBool>,
    /// Count of currently-connected clients -- genuinely N now (see
    /// ClientAudioIqSink's doc comment: multiple simultaneous clients
    /// are fully supported, not just tolerated during a handoff
    /// window). Lets the UI show "listening, no client" vs. "client(s)
    /// connected" separately from "not running at all" (server is
    /// None).
    connected: Arc<AtomicU32>,
    thread: Option<JoinHandle<()>>,
    /// See spawn_audio_iq_broadcaster's doc comment -- the single
    /// reader of the shared audio/IQ taps, fanning out to every
    /// connected client's own sink. Joined by stop() same as `thread`.
    broadcaster_thread: Option<JoinHandle<()>>,
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
        // Where an incoming `vfo` (set) command writes its request --
        // NOT the raw hardware frequency. See RadioSession::
        // requested_frequency_hz's doc comment: main.rs's own per-frame
        // loop reconciles this (moving the CTUN target if CTUN is on,
        // retuning the real hardware otherwise), since CTUN state lives
        // in the UI layer, not anywhere reachable from this server
        // thread.
        requested_frequency_hz: Arc<AtomicU32>,
        // See RadioSession::rx_frequency_hz's doc comment -- used for the
        // vfo/heartbeat frequency reports so a CTUN'd listen frequency is
        // reported correctly, not the parked hardware LO.
        rx_frequency_hz: Arc<AtomicU32>,
        sample_rate: Arc<AtomicU32>,
        demod_params: Arc<Mutex<DemodParams>>,
        mox: Arc<AtomicBool>,
        tci_audio_out: Arc<Mutex<VecDeque<f32>>>,
        iq_out: Arc<Mutex<VecDeque<(f32, f32)>>>,
        tci_tx_audio: Arc<Mutex<VecDeque<f32>>>,
        tci_tx_gain: Arc<Mutex<f32>>,
        tci_wants_mic: Arc<AtomicBool>,
        // See RadioSession::rit_enabled/rit_offset_hz/xit_enabled/
        // xit_offset_hz's doc comments -- backs the incoming
        // rit_offset/rit_enable/xit_offset/xit_enable commands and their
        // real values in the initial state push.
        rit_enabled: Arc<AtomicBool>,
        rit_offset_hz: Arc<AtomicI32>,
        xit_enabled: Arc<AtomicBool>,
        xit_offset_hz: Arc<AtomicI32>,
        // For the initial state push's device: field -- the actual
        // detected board (e.g. "Orion2"), not this project's own name.
        // See handle_client's doc comment on why this matters.
        board_name: String,
        logging: DebugLog,
    ) -> std::io::Result<Self> {
        let listener = TcpListener::bind(addr)?;
        listener.set_nonblocking(true)?;
        println!("tci: listening on {addr}");

        let demod_params: DemodParamsCell = Arc::new(Mutex::new(demod_params));
        let audio_iq: AudioIqCell = Arc::new(Mutex::new(AudioIqTaps { audio: tci_audio_out, iq: iq_out }));
        let stop = Arc::new(AtomicBool::new(false));
        let connected = Arc::new(AtomicU32::new(0));
        let client_threads: Arc<Mutex<Vec<JoinHandle<()>>>> = Arc::new(Mutex::new(Vec::new()));
        // See ClientAudioIqSink/spawn_audio_iq_broadcaster's doc comments
        // -- replaces the single-client "supersede" enforcement this
        // used to need.
        let audio_iq_registry: ClientAudioIqRegistry = Arc::new(Mutex::new(Vec::new()));
        let broadcaster_thread =
            spawn_audio_iq_broadcaster(Arc::clone(&audio_iq), Arc::clone(&audio_iq_registry), Arc::clone(&stop));
        let accept_stop = Arc::clone(&stop);
        let accept_connected = Arc::clone(&connected);
        let accept_client_threads = Arc::clone(&client_threads);
        let accept_demod_params = Arc::clone(&demod_params);
        let accept_audio_iq_registry = Arc::clone(&audio_iq_registry);
        let accept_tci_tx_audio = Arc::clone(&tci_tx_audio);
        let accept_tci_tx_gain = Arc::clone(&tci_tx_gain);
        let accept_tci_wants_mic = Arc::clone(&tci_wants_mic);
        let accept_logging = logging.clone();
        let thread = thread::spawn(move || {
            while !accept_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, peer)) => {
                        println!("tci: client connected from {peer}");
                        accept_logging.log(&format!("client connected from {peer}"));
                        let freq = Arc::clone(&requested_frequency_hz);
                        let rx_freq = Arc::clone(&rx_frequency_hz);
                        let rate = Arc::clone(&sample_rate);
                        let params = Arc::clone(&accept_demod_params);
                        let conn_audio_iq_registry = Arc::clone(&accept_audio_iq_registry);
                        let tx_audio = Arc::clone(&accept_tci_tx_audio);
                        let tx_gain = Arc::clone(&accept_tci_tx_gain);
                        let wants_mic = Arc::clone(&accept_tci_wants_mic);
                        let conn_mox = Arc::clone(&mox);
                        let conn_rit_enabled = Arc::clone(&rit_enabled);
                        let conn_rit_offset_hz = Arc::clone(&rit_offset_hz);
                        let conn_xit_enabled = Arc::clone(&xit_enabled);
                        let conn_xit_offset_hz = Arc::clone(&xit_offset_hz);
                        let conn_stop = Arc::clone(&accept_stop);
                        let conn_connected = Arc::clone(&accept_connected);
                        let conn_logging = accept_logging.clone();
                        let conn_board_name = board_name.clone();
                        let handle = thread::spawn(move || {
                            conn_connected.fetch_add(1, Ordering::Relaxed);
                            handle_client(
                                stream, freq, rx_freq, rate, params, conn_audio_iq_registry, tx_audio, tx_gain,
                                wants_mic, conn_mox, conn_rit_enabled, conn_rit_offset_hz, conn_xit_enabled,
                                conn_xit_offset_hz, conn_stop, conn_board_name, conn_logging,
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
            broadcaster_thread: Some(broadcaster_thread),
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
        if let Some(t) = self.broadcaster_thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for TciServer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// ROOT CAUSE FIX: `ws.send(...)` failing with `WouldBlock` used to be
/// treated as fatal (tearing down the whole connection) everywhere it's
/// called below -- confirmed via a real Windows report + the debug-log
/// error text (`Io(Os { code: 10035, kind: WouldBlock, ... })`, WinSock's
/// WSAEWOULDBLOCK) as the actual cause of TCI Remote reconnecting every
/// ~2s on Windows (never on Linux, same build otherwise): the socket's
/// send buffer was only ever momentarily full, not actually broken --
/// this project only sets a READ timeout on the stream (see
/// set_read_timeout above), not a write one, so a send "should" just
/// block until buffer space frees up the way it apparently does on
/// Linux, but Windows evidently doesn't honor that the same way here.
/// A `WouldBlock` send is now just skipped (the caller drops that one
/// chunk/reply and tries again next tick) rather than closing the
/// connection -- fine for audio/IQ streaming samples (a dropped chunk
/// beats added latency from retrying synchronously) and for command
/// replies/TxChrono (both already tolerate occasional loss elsewhere in
/// this file's own logic, e.g. tx_chrono_outstanding's staleness reset).
fn send_is_fatal(result: &tungstenite::Result<()>) -> bool {
    match result {
        Ok(()) => false,
        Err(tungstenite::Error::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => false,
        Err(_) => true,
    }
}

/// Sends one outgoing TCI text message and logs it (">> msg", matching
/// the existing reply-logging convention below) in the same call --
/// used for the initial per-client state push, which previously wasn't
/// logged at all. ROOT CAUSE FIX for a real report: with the push
/// invisible in tci_log.txt, there was no way to confirm from the log
/// alone whether a fix to that push (e.g. ready/start's trailing-
/// semicolon fix) had actually taken effect on a given build, as
/// opposed to the log simply predating a rebuild.
fn send_logged(ws: &mut tungstenite::WebSocket<TcpStream>, logging: &DebugLog, msg: String) {
    logging.log(&format!(">> {msg}"));
    let _ = ws.send(Message::Text(msg.into()));
}

fn handle_client(
    stream: TcpStream,
    requested_frequency_hz: Arc<AtomicU32>,
    rx_frequency_hz: Arc<AtomicU32>,
    sample_rate: Arc<AtomicU32>,
    demod_params: DemodParamsCell,
    // See ClientAudioIqSink's doc comment -- this client registers its
    // own sink into the registry below rather than draining a queue
    // shared with every other connected client.
    audio_iq_registry: ClientAudioIqRegistry,
    tci_tx_audio: Arc<Mutex<VecDeque<f32>>>,
    tci_tx_gain: Arc<Mutex<f32>>,
    tci_wants_mic: Arc<AtomicBool>,
    mox: Arc<AtomicBool>,
    rit_enabled: Arc<AtomicBool>,
    rit_offset_hz: Arc<AtomicI32>,
    xit_enabled: Arc<AtomicBool>,
    xit_offset_hz: Arc<AtomicI32>,
    stop: Arc<AtomicBool>,
    // Real detected board (e.g. "Orion2"), for the initial state push's
    // device: field -- see that field's own doc comment for why this
    // shouldn't just be this project's own name.
    board_name: String,
    logging: DebugLog,
) {
    let _ = stream.set_nodelay(true);
    // BUG FIX: same underlying Windows socket-mode quirk as
    // send_is_fatal's own doc comment above (that one confirmed via
    // WSAEWOULDBLOCK/error 10035 on writes), showing up here on the
    // read side instead, at a point that fix didn't reach. The listener
    // this stream came from is non-blocking (TciServer::start's
    // listener.set_nonblocking(true), needed for its own accept-loop
    // poll), and on Windows a socket returned by accept() on a
    // non-blocking listener inherits that non-blocking state too --
    // unlike Linux/macOS, where an accepted socket is always a fresh,
    // independent descriptor that defaults to blocking regardless of
    // the listener's own mode. Without this, tungstenite::accept()'s
    // single blocking-style read below could run before the client's
    // handshake bytes had actually arrived, see 0 bytes available, and
    // (unlike send_is_fatal's handling, there's no WouldBlock-tolerant
    // retry here -- tungstenite's own accept() treats it as the client
    // closing the connection) report "WebSocket protocol error:
    // handshake not finished" and give up immediately, instead of
    // actually waiting for the handshake to arrive. Confirmed via a
    // real report + packet capture on Windows: a client's connection
    // got an unsolicited FIN from this server ~65ms after connecting,
    // with zero bytes exchanged either direction, then sent its real
    // handshake ~700ms later onto an already-closed connection.
    let _ = stream.set_nonblocking(false);

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

    // This client's own private audio/IQ sink -- see ClientAudioIqSink's
    // doc comment. Starts empty by construction, so unlike the old
    // shared-queue design there's no "burst of stale pre-connect audio
    // on the very first drain" concern to work around here: nothing
    // gets pushed into this sink until spawn_audio_iq_broadcaster's next
    // tick, after this client is already registered. Kept alive as a
    // local variable for this whole function -- the registry below only
    // holds a Weak reference, so this Arc is what keeps the sink (and
    // this client's registration) alive for as long as this thread runs.
    let sink = Arc::new(ClientAudioIqSink { audio: Mutex::new(VecDeque::new()), iq: Mutex::new(VecDeque::new()) });
    audio_iq_registry.lock().unwrap().push(Arc::downgrade(&sink));

    // Best-effort initial state push -- see module-level note.
    //
    // REORDERED to match a real Thetis TCI session log's actual
    // sequence closely, not just its content -- a prior pass (see the
    // BUG FIX comments retained below on the individual fields that
    // pass added) matched Thetis's full field list but kept this
    // project's own original ordering (protocol/device first, ready;/
    // start; near the end), and a client ("TCI Remote Compactor", a
    // bandwidth-compacting relay, not TCI Remote itself) still kept
    // reconnecting every 1-3s even with every field present -- ruling
    // out "missing content" as the cause. Thetis's own reference
    // sequence sends tx_profile_ex/tx_profiles_ex FIRST, ready;/start;
    // (that order, not start;/ready;) right after the client's own
    // ready/start, and protocol:/device: LAST -- the opposite of what
    // this project was doing. device: specifically was never actually
    // sent at all despite an earlier comment here claiming it was
    // "added alongside" protocol -- confirmed missing by grep, not just
    // reasoning; Thetis's reference explicitly includes it.
    //
    // The start;/ready; ORDER swap is the one change here with a small
    // known regression risk: the original start;-then-ready; order was
    // copied from rustyHPSDR while fixing a real WSJT-X report ("TCI
    // SDR is not switched on"), but that report was about start;
    // missing entirely, not about its position relative to ready; --
    // there's no independent confirmation the order itself mattered to
    // WSJT-X, so matching Thetis (whose protocol identity this project
    // already claims via PROTOCOL_MESSAGE) is the more consistent
    // choice if this needs to be picked one way.
    let freq = rx_frequency_hz.load(Ordering::Relaxed);
    let params = *demod_params.lock().unwrap().clone().lock().unwrap();
    let mode = params.mode;
    let (pb_low, pb_high) = passband_for(mode, params.width_hz);
    let nb_on = params.noise_blanker != NoiseBlanker::Off;
    let nr_on = params.noise_reduction != NoiseReduction::Off;

    // CORRECTED per a real packet capture of actual Thetis wire traffic
    // the user provided (tci-idle-thetis.pcapng, an idle ExpertSDR3/
    // Thetis session), which directly contradicts a written protocol
    // reference (pure-editions.com/on7off's TCI Protocol Reference) an
    // earlier pass here trusted instead: that reference claims `ready`/
    // `start` are bare tokens with no trailing semicolon, but the actual
    // bytes on the wire from Thetis -- confirmed by exact WebSocket
    // frame length (8 bytes = 2-byte frame header + 6-byte payload
    // "start;"/"ready;", not "start"/"ready") -- are semicolon-
    // terminated after all, sent as **start; then ready;** (not
    // ready;/start; as an earlier pass here also had it), with
    // tx_profile_ex/tx_profiles_ex following AFTER ready, not before
    // it. A hard packet capture of the real reference implementation
    // outweighs a written doc here. This also vindicates this project's
    // OWN original start;-then-ready; order (copied from rustyHPSDR),
    // which a much earlier pass swapped away from based on an
    // unconfirmed belief about Thetis's own order.
    send_logged(&mut ws, &logging, "start;".into());
    send_logged(&mut ws, &logging, "ready;".into());
    // This project has no real TX-profile concept (Thetis-style PA/EQ
    // profile switching) -- "Default" is a static, harmless stand-in.
    send_logged(&mut ws, &logging, "tx_profile_ex:Default;".into());
    send_logged(&mut ws, &logging, "tx_profiles_ex:Default;".into());

    // TX_ENABLE (spec section 4.3): "informs clients that TX is
    // enabled... sent to the client when connected... in case
    // transmitter permission was changed" -- i.e. tells the client
    // whether it's allowed to transmit at all. AUDIO_SAMPLERATE/
    // IQ_SAMPLERATE and the four audio-stream parameters are, per spec,
    // normally client-to-server -- but Thetis proactively announces its
    // own values for them at connect too (sensible defaults a client
    // can still override), and a client that reads these rather than
    // assuming its own defaults would otherwise silently disagree with
    // this project's real values. AUDIO_STREAM_SAMPLES tracks
    // TCI_TX_AUDIO_CHUNK (this project's own real per-reply request
    // size -- see that constant's doc comment for the full back-and-
    // forth on what value actually belongs here).
    send_logged(&mut ws, &logging, "mon_volume:0.0;".into());
    send_logged(&mut ws, &logging, "mon_enable:false;".into());
    send_logged(&mut ws, &logging, "tune:0,false;".into());
    send_logged(&mut ws, &logging, "rx_mute:0,false;".into());
    send_logged(&mut ws, &logging, "mute:false;".into());
    send_logged(&mut ws, &logging, "tx_stream_audio_buffering:50;".into());
    send_logged(&mut ws, &logging, format!("audio_stream_samples:{TCI_TX_AUDIO_CHUNK};").into());
    send_logged(&mut ws, &logging, "audio_stream_channels:2;".into());
    send_logged(&mut ws, &logging, "audio_stream_sample_type:float32;".into());
    send_logged(&mut ws, &logging, format!("audio_samplerate:{TCI_AUDIO_SAMPLE_RATE};").into());
    send_logged(&mut ws, &logging, format!("iq_samplerate:{};", sample_rate.load(Ordering::Relaxed)).into());
    send_logged(&mut ws, &logging, "iq_stop:0;".into());
    send_logged(&mut ws, &logging, format!("trx:0,{};", mox.load(Ordering::Relaxed)).into());
    send_logged(&mut ws, &logging, "tune_drive:0,50;".into());
    send_logged(&mut ws, &logging, "drive:0,50;".into());
    send_logged(&mut ws, &logging, "rx_channel_enable:0,0,true;".into());
    send_logged(&mut ws, &logging, "tx_enable:0,true;".into());
    send_logged(&mut ws, &logging, "split_enable:0,false;".into());
    // No real value to report for features this project doesn't
    // implement at all (CW keyer, squelch, VFO lock, calibration,
    // preamp/step attenuator) -- a safe/neutral static default (matching
    // Thetis's own example values where they're clearly just
    // "off"/"none") still beats the client getting no reply at all to a
    // field it expects.
    send_logged(&mut ws, &logging, "sql_level:0,-140;".into());
    send_logged(&mut ws, &logging, "sql_enable:0,false;".into());
    send_logged(&mut ws, &logging, "lock:0,false;".into());
    // Real values below where this project actually tracks the
    // underlying state (RIT/XIT, CTUN, AGC mode, noise blanker/
    // reduction, audio gain, filter passband, frequency/mode/dds).
    // Channel 0 only, matching this project's own trx_count:1/
    // channels_count:1 self-declaration below.
    send_logged(&mut ws, &logging, format!("xit_offset:0,{};", xit_offset_hz.load(Ordering::Relaxed)).into());
    send_logged(&mut ws, &logging, format!("rit_offset:0,{};", rit_offset_hz.load(Ordering::Relaxed)).into());
    send_logged(&mut ws, &logging, format!("xit_enable:0,{};", xit_enabled.load(Ordering::Relaxed)).into());
    send_logged(&mut ws, &logging, format!("rit_enable:0,{};", rit_enabled.load(Ordering::Relaxed)).into());
    send_logged(&mut ws, &logging, format!("rx_ctun_ex:0,{};", params.ctun).into());
    send_logged(&mut ws, &logging, "fm_deviation_ex:0,5000;".into());
    send_logged(&mut ws, &logging, "agc_auto_ex:0,false;".into());
    send_logged(&mut ws, &logging, format!("agc_mode:0,{};", agc_to_tci(params.agc)).into());
    send_logged(&mut ws, &logging, "rx_preamp_att_ex:0,0;".into());
    send_logged(&mut ws, &logging, "rx_step_att_ex:0,0;".into());
    send_logged(&mut ws, &logging, "rx_step_att_enabled_ex:0,false;".into());
    send_logged(&mut ws, &logging, "vfo_sync_ex:false;".into());
    send_logged(&mut ws, &logging, "rx_balance:0,0,0.00;".into());
    send_logged(&mut ws, &logging, format!("rx_volume:0,0,{:.2};", gain_to_tci_db(params.gain)).into());
    send_logged(&mut ws, &logging, "rx_nf_enable:0,false;".into());
    send_logged(&mut ws, &logging, "rx_apf_enable:0,false;".into());
    send_logged(&mut ws, &logging, "rx_anf_enable:0,false;".into());
    send_logged(&mut ws, &logging, "rx_bin_enable:0,false;".into());
    send_logged(&mut ws, &logging, format!("rx_nb_enable_ex:0,{nb_on},0;").into());
    send_logged(&mut ws, &logging, format!("rx_nb_enable:0,{nb_on};").into());
    send_logged(&mut ws, &logging, format!("rx_nr_enable_ex:0,{nr_on},0;").into());
    send_logged(&mut ws, &logging, format!("rx_nr_enable:0,{nr_on};").into());
    send_logged(&mut ws, &logging, "rx_enable:0,true;".into());
    send_logged(&mut ws, &logging, format!("rx_filter_band:0,{},{};", pb_low.round() as i64, pb_high.round() as i64).into());
    send_logged(&mut ws, &logging, format!("modulation:0,{};", mode_to_tci(mode)).into());
    send_logged(&mut ws, &logging, format!("tx_frequency:{freq};").into());
    send_logged(&mut ws, &logging, format!("vfo:0,0,{freq};").into());
    send_logged(&mut ws, &logging, "if:0,0,0;".into());
    send_logged(&mut ws, &logging, format!("dds:0,{freq};").into());
    // MODULATIONS_LIST uses the canonical spellings tci_to_mode below
    // actually keys on, not its aliases (CWR/PKTUSB/PKTLSB/WFM).
    // IF_LIMITS/VFO_LIMITS: VFO_LIMITS mirrors rigctl.rs's own
    // permissive 0Hz-4GHz range (see its dump_state's doc comment)
    // rather than modeling exact hardware limits -- no single value is
    // right for every supported board/filter combination, and a
    // wide-open range never incorrectly rejects a client's request.
    // RECEIVE_ONLY:false -- this project always has TX support wired
    // up (Settings -> TX gates actual PTT, not the protocol handshake).
    // TRX_COUNT/CHANNEL_COUNT:1 -- TCI (unlike rigctl) has no second-
    // VFO/extra-receiver concept implemented here; see this file's
    // module note on trx always being 0.
    send_logged(&mut ws, &logging, "modulations_list:LSB,USB,DSB,CW,AM,NFM,DIGU,DIGL,SAM;".into());
    send_logged(&mut ws, &logging, "if_limits:-96000,96000;".into());
    send_logged(&mut ws, &logging, "vfo_limits:0,4000000000;".into());
    // FIELD NAME FIX: was "channel_count" (singular) -- confirmed wrong
    // against the same authoritative reference as the ready/start fix
    // above; the real field is "channels_count" (plural). Harmless
    // either way for TCI Remote specifically (this field is "accepted,
    // discarded" per that reference), but worth being correct for other
    // TCI clients. Value bumped to 2 to match this project's own
    // audio_stream_channels:2 declaration just above (RX audio frames
    // are always stereo here, mono duplicated to both channels).
    send_logged(&mut ws, &logging, "channels_count:2;".into());
    send_logged(&mut ws, &logging, "trx_count:1;".into());
    send_logged(&mut ws, &logging, "receive_only:false;".into());
    // DEVICE identifies the actual radio hardware (e.g. Thetis reports
    // "ANAN8000D" for its connected board) -- a real report/correction:
    // this used to send this project's own name ("hpsdr-rs") here
    // instead, which isn't what the field is for. board_name is
    // whatever discovery.rs's own Boards enum detected (e.g. "Orion2"),
    // not a specific commercial model name -- this project has no way
    // to know a radio's marketing/model name (e.g. "ANAN8000D" vs the
    // underlying "Orion2" board it's built around), only the board type
    // reported over the wire.
    send_logged(&mut ws, &logging, format!("device:{board_name};").into());
    send_logged(&mut ws, &logging, PROTOCOL_MESSAGE.into());

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
    // ROOT CAUSE INVESTIGATION continued: a real Windows report's
    // connections never logged a read error either (see "connection
    // error, closing" above) -- consistent with a connection just going
    // quiet (no read error, no data) until the client gives up and opens
    // a new one. Confirmed via a later report + tci_log.txt timestamps:
    // this project's own single-client "supersede" enforcement (since
    // removed -- see ClientAudioIqSink's doc comment) was then killing
    // that new connection's *predecessor* within milliseconds, even
    // when the predecessor was perfectly healthy and already streaming
    // -- multiple clients are now allowed to coexist instead. These
    // one-shot markers still narrow down WHERE a stream first starts
    // flowing, which remains useful for diagnosing anything similar in
    // the future: logged the first time each stream actually sends real
    // data, so comparing against "iq_start"/"audio_start" timestamps
    // already logged shows whether data flows at all before a given
    // reconnect, or never starts flowing in the first place (which
    // would point at the RX pipeline itself, not networking).
    let mut audio_first_sent = false;
    let mut iq_first_sent = false;
    // BUG FIX: a bad (garbage-value) pair used to be dropped outright
    // (`continue`, pushing nothing), which shortens tci_tx_audio's
    // effective timeline by one sample every time it fires -- confirmed
    // via real-hardware log analysis to happen at a real, sometimes
    // substantial rate (up to ~7% of pairs in one capture), not the
    // rare edge case it was assumed to be. A real timing discontinuity
    // repeated at that rate throughout a whole transmission is a
    // plausible source of real spectral splatter (phase/timing glitches
    // produce FM-like sidebands), and only affects TCI-sourced TX audio
    // -- mic/PipeWire audio never touches this path, matching a real
    // report of splatter/no-decode specific to TCI.
    //
    // BUG FIX (round 2): holding the last known-good sample flat for
    // the whole bad stretch (the first attempt at this) fixed the
    // timeline-shortening problem but introduced a new, worse one --
    // confirmed via the message-boundary-jump diagnostic below: it
    // produced a real, CONSTANT ~1.14 amplitude jump (in a +-1.0-range
    // signal, over half the full dynamic range) every time real data
    // resumed after a held stretch, repeating on nearly every single
    // second of a real capture. That's a much more direct, plausible
    // cause of the splatter being chased than anything upstream of
    // this. Fixed properly now: bad pairs are held back (not pushed
    // yet) until the next good sample arrives, then the whole gap is
    // linearly interpolated from the last confirmed-good sample to
    // that new one and pushed as a smooth ramp -- no flat spot, no
    // jump. pending_bad_count is capped (see PENDING_BAD_LIMIT) so a
    // long run of consecutive bad messages can't grow this unboundedly
    // or add unbounded latency; past that point it falls back to the
    // old hold behavior for the excess, same as before this fix.
    let mut last_confirmed_good: f32 = 0.0;
    let mut pending_bad_count: usize = 0;
    // See the periodic vfo/modulation/trx heartbeat below.
    let mut last_status_broadcast = Instant::now();
    // BUG FIX (round 2 -- replaced fixed-interval pacing entirely):
    // TxChrono used to be sent one-at-a-time, real-time paced (one
    // request roughly every TCI_TX_AUDIO_CHUNK/TCI_AUDIO_SAMPLE_RATE),
    // with a low-water-mark catch-up for the ~12.5% of replies WSJT-X's
    // own ring-buffer bug corrupts. That fixed the original runaway-
    // request bug (see git history: sending unconditionally once per
    // loop iteration let ws.read() returning instantly for a waiting
    // reply spin this loop at ~80,000 msgs/sec, fast-forwarding WSJT-X's
    // internal tone generator through an entire transmission in 1-2
    // real seconds) but kept tci_tx_audio perpetually thin -- never more
    // than about one chunk ahead of real-time consumption. Splatter
    // persisted regardless of every audio-CONTENT fix tried (gain,
    // clamping, interpolating around bad messages, matching every TCI
    // handshake announcement Thetis sends) until directly comparing
    // this project's pacing against Thetis's own (cmaster.cs's TCI TX
    // buffering logic, confirmed via source): Thetis targets a genuine
    // ~100ms PRE-BUFFERED queue (TX_STREAM_AUDIO_BUFFERING's default
    // 50ms + its own hardcoded TCI_TX_EXTRA_BUFFER_MS=50), keeping up to
    // TCI_TX_MAX_OUTSTANDING=64 TxChrono requests pipelined (sent before
    // their replies arrive) rather than one in flight at a time --
    // fundamentally a buffer-target control loop, not a real-time
    // one-for-one pull. tx_chrono_outstanding/TX_CHRONO_MAX_OUTSTANDING/
    // TX_CHRONO_TARGET_BUFFER_SAMPLES below mirror that: each tick,
    // enough requests are sent to bring (queued + outstanding*chunk) up
    // to the target, self-limiting once the buffer is full (steady
    // state sends nothing) and self-correcting as real consumption
    // drains it -- not the same failure mode as the original unbounded
    // flood, which had no target and no cap at all. Decremented back
    // down in the TxAudioStream handler below as real replies actually
    // arrive (matching Thetis's own dequeue-and-decrement).
    let mut tx_chrono_outstanding: u32 = 0;
    let mut last_tx_chrono_activity = Instant::now();
    let mut mox_was_active = false;

    while !stop.load(Ordering::Relaxed) {
        match ws.read() {
            Ok(Message::Text(text)) => {
                for cmd in text.split(';') {
                    let cmd = cmd.trim();
                    if cmd.is_empty() {
                        continue;
                    }
                    logging.log(&format!("<< {cmd};"));
                    // Resolved fresh on every command (not once per
                    // connection) so a sample-rate change mid-session --
                    // see set_demod_params -- takes effect immediately
                    // for already-connected clients too, not just new
                    // ones.
                    let current_params = demod_params.lock().unwrap().clone();
                    if let Some(response) = handle_command(
                        cmd,
                        &requested_frequency_hz,
                        &current_params,
                        &mox,
                        &rit_enabled,
                        &rit_offset_hz,
                        &xit_enabled,
                        &xit_offset_hz,
                        &tci_wants_mic,
                        &mut audio_streaming,
                        &mut iq_streaming,
                        &sink,
                    ) {
                        logging.log(&format!(">> {response}"));
                        let result = ws.send(Message::Text(response.into()));
                        if send_is_fatal(&result) {
                            logging.log(&format!("reply send failed, closing: {:?}", result.unwrap_err()));
                            return;
                        }
                    }
                }
            }
            Ok(Message::Close(_)) => {
                logging.log("client closed the connection");
                break;
            }
            Ok(Message::Binary(data)) => {
                // A client sending TX audio -- see decode_binary_message's
                // doc comment for the confidence caveat on this whole
                // exchange (no confirmed-working reference for the
                // server side of it, unlike RX audio/IQ above).
                if let Some((msg_type, samples)) = decode_binary_message(&data) {
                    if msg_type == BinaryMessageType::TxAudioStream as u32 {
                        // One reply consumes one outstanding TxChrono
                        // request -- see tx_chrono_outstanding's doc
                        // comment above. Matches Thetis's own
                        // dequeue-and-decrement (cmaster.cs).
                        tx_chrono_outstanding = tx_chrono_outstanding.saturating_sub(1);
                        last_tx_chrono_activity = Instant::now();
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
                                if pending_bad_count >= PENDING_BAD_LIMIT {
                                    // Safety fallback -- see
                                    // PENDING_BAD_LIMIT's doc comment.
                                    if q.len() >= capacity {
                                        q.pop_front();
                                    }
                                    q.push_back(last_confirmed_good);
                                } else {
                                    pending_bad_count += 1;
                                }
                                continue;
                            }
                            // BUG FIX: this used to push the gained
                            // sample with no clamp at all -- unlike
                            // spectrum.rs's RX audio path, which clamps
                            // to +-1.0 after applying its own gain.
                            // tci_tx_gain's range is 0.0..=1000.0 (see
                            // its slider's own doc comment -- WSJT-X's
                            // TCI audio arrives at roughly 1/700th
                            // normal amplitude, so gain routinely needs
                            // to be in the hundreds), and the sanity
                            // check just above only rejects raw samples
                            // above 2.0 -- so a real, legitimate gain
                            // setting could easily produce values in the
                            // hundreds or thousands here, fed straight
                            // into WDSP's TX chain with nothing to catch
                            // it. Confirmed via real-hardware report:
                            // gain=1000, TX power fluctuating 35-55W
                            // instead of a steady 100W, visibly
                            // splattered/broadband TX spectrum, and no
                            // PSKReporter decodes -- all consistent with
                            // a badly overdriven, clipped signal.
                            let sample = ((l + r) * 0.5 * gain).clamp(-1.0, 1.0);
                            // Resolve any held-back bad stretch now that
                            // we have a real value to ramp toward -- see
                            // pending_bad_count's doc comment.
                            for i in 1..=pending_bad_count {
                                let t = i as f32 / (pending_bad_count + 1) as f32;
                                let interp = last_confirmed_good + (sample - last_confirmed_good) * t;
                                if q.len() >= capacity {
                                    q.pop_front();
                                }
                                q.push_back(interp);
                            }
                            pending_bad_count = 0;
                            last_confirmed_good = sample;
                            if q.len() >= capacity {
                                q.pop_front();
                            }
                            q.push_back(sample);
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
            Err(e) => {
                // ROOT CAUSE INVESTIGATION: a real report of TCI Remote
                // reconnecting every ~2s on Windows (never on Linux, same
                // build otherwise) with no "client closed the connection"
                // ever logged points straight at this arm -- the only
                // other way this loop exits without logging something.
                // ~2s is suspiciously close to this read's own 20ms
                // timeout ticking over many times right after the
                // handshake goes quiet, which is consistent with Windows
                // surfacing a DIFFERENT io::ErrorKind (or a non-Io
                // tungstenite::Error variant entirely) for what's
                // actually just an ordinary idle-read timeout than Linux
                // does for the exact same socket state -- but logging the
                // real error here beats guessing further at which kind
                // that might be.
                logging.log(&format!("connection error, closing: {e:?}"));
                break;
            }
        }

        // This client's own sink -- see ClientAudioIqSink's doc comment.
        // No re-resolving needed on a sample-rate change the way the
        // old shared-taps design required: spawn_audio_iq_broadcaster is
        // the one place that has to follow TciServer::set_audio_iq's new
        // queues, and it feeds this same sink regardless.
        if audio_streaming {
            let samples: Vec<f32> = {
                let mut q = sink.audio.lock().unwrap();
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
                // untouched, so this doesn't affect the real audio
                // content itself. The IQ stream below got the identical
                // fix later, once a client hit the same issue there too
                // -- see its own doc comment.
                //
                // Chunked to MAX_AUDIO_SAMPLES_PER_MESSAGE -- see that
                // constant's doc comment for why a single unbounded
                // drain-and-send breaks real clients.
                for chunk in stereo.chunks(MAX_AUDIO_SAMPLES_PER_MESSAGE) {
                    let msg = encode_binary_message(
                        0,
                        TCI_AUDIO_SAMPLE_RATE,
                        BinaryMessageType::RxAudioStream,
                        chunk.len() as u32 * 2,
                        chunk,
                    );
                    let result = ws.send(Message::Binary(msg.into()));
                    if send_is_fatal(&result) {
                        logging.log(&format!("audio stream send failed, closing: {:?}", result.unwrap_err()));
                        return;
                    }
                    if result.is_ok() && !audio_first_sent {
                        audio_first_sent = true;
                        logging.log("first audio data sent");
                    }
                }
            }
        }

        if iq_streaming {
            let pairs: Vec<(f32, f32)> = {
                let mut q = sink.iq.lock().unwrap();
                q.drain(..).collect()
            };
            if !pairs.is_empty() {
                // Q before I -- confirmed against rustyHPSDR's own
                // working IQ streaming code, which swaps this
                // explicitly ("SWAP: Push Q then I"), not I before Q
                // as would be the naive/obvious order.
                let swapped: Vec<(f32, f32)> = pairs.into_iter().map(|(i, q)| (q, i)).collect();
                // BUG FIX: `length` here used to be swapped.len() (frame-
                // pair count), matching rustyHPSDR's own convention
                // (confirmed working against TCI Remote) -- same
                // reasoning as the audio stream's own identical fix
                // above, which was deliberately NOT extended to IQ at
                // the time since nothing had reported it broken. A real
                // report since then: "TCI Remote Compactor" (a separate
                // bridge app, re-encoding TCI for a bandwidth-constrained
                // link to a remote TCI Remote client -- not TCI Remote
                // itself) kept re-sending iq_start every few hundred ms
                // after already receiving "first IQ data sent" and
                // getting no closer, then giving up and reconnecting
                // from scratch -- audio_start was never retried the same
                // way, pointing specifically at the IQ stream's framing,
                // not a general connection problem. Matches the same
                // class of bug as the WSJT-X audio report: a client
                // reading `length` as the raw float count (both channels)
                // reads only half the intended samples per packet.
                // Payload (`&swapped`) and its real sample count are
                // unchanged, only the announced `length` value.
                //
                // FURTHER BUG FIX, found chasing the exact Compactor
                // symptom described above via a real packet capture: the
                // first drain after iq_start could be the client's whole
                // backlog (built up while iq_streaming was false for this
                // client but the broadcaster kept filling its sink
                // regardless -- up to CLIENT_SINK_MAX_IQ_PAIRS), sent as
                // ONE oversized WS message. A capture of a Compactor probe
                // connection caught exactly this: a single 1,600,064-byte
                // first IQ frame, closed by the client's own WebSocket
                // library with code 1009 "Message Too Big". Chunked to
                // MAX_IQ_PAIRS_PER_MESSAGE -- see that constant's doc
                // comment.
                for chunk in swapped.chunks(MAX_IQ_PAIRS_PER_MESSAGE) {
                    let msg = encode_binary_message(
                        0,
                        sample_rate.load(Ordering::Relaxed),
                        BinaryMessageType::IqStream,
                        chunk.len() as u32 * 2,
                        chunk,
                    );
                    let result = ws.send(Message::Binary(msg.into()));
                    if send_is_fatal(&result) {
                        logging.log(&format!("IQ stream send failed, closing: {:?}", result.unwrap_err()));
                        return;
                    }
                    if result.is_ok() && !iq_first_sent {
                        iq_first_sent = true;
                        logging.log("first IQ data sent");
                    }
                }
            }
        }

        // TxChrono -- requests TX audio from this client while
        // transmitting. See tx_chrono_outstanding's doc comment above
        // for the buffer-target pipelined strategy this uses (mirroring
        // Thetis's own confirmed-working approach) in place of the
        // earlier one-request-at-a-time real-time pacing. A client that
        // doesn't implement TX audio simply won't respond to this;
        // harmless either way (outstanding just naturally stays at 0
        // forever, tci_tx_audio stays empty, tx.rs falls through to
        // mic_buffer exactly as if no TCI client were sending audio).
        let mox_active = mox.load(Ordering::Relaxed);
        if mox_active && !mox_was_active {
            // Resync on every fresh PTT rather than trusting whatever
            // outstanding count survived from a previous transmission.
            tx_chrono_outstanding = 0;
            last_tx_chrono_activity = Instant::now();
        }
        mox_was_active = mox_active;
        if mox_active {
            // Staleness reset -- if WSJT-X stops replying entirely
            // (connection hiccup, client-side stall), outstanding would
            // otherwise sit stuck at whatever count it last reached,
            // permanently blocking new requests once
            // TX_CHRONO_MAX_OUTSTANDING is hit. 500ms mirrors Thetis's
            // own reset threshold (max(250, bufferingMs*4) with its
            // 50ms default buffering -- 500ms is that same formula's
            // result here).
            if tx_chrono_outstanding > 0
                && last_tx_chrono_activity.elapsed() >= Duration::from_millis(500)
            {
                tx_chrono_outstanding = 0;
            }
            let queued = tci_tx_audio.lock().unwrap().len();
            let future = queued + tx_chrono_outstanding as usize * TCI_TX_AUDIO_CHUNK as usize;
            if future < TX_CHRONO_TARGET_BUFFER_SAMPLES {
                let deficit = TX_CHRONO_TARGET_BUFFER_SAMPLES - future;
                let requests_needed = deficit.div_ceil(TCI_TX_AUDIO_CHUNK as usize) as u32;
                let requests_needed =
                    requests_needed.min(TX_CHRONO_MAX_OUTSTANDING.saturating_sub(tx_chrono_outstanding));
                for _ in 0..requests_needed {
                    let chrono = encode_binary_message(
                        0,
                        TCI_AUDIO_SAMPLE_RATE,
                        BinaryMessageType::TxChrono,
                        TCI_TX_AUDIO_CHUNK,
                        &[],
                    );
                    let result = ws.send(Message::Binary(chrono.into()));
                    if send_is_fatal(&result) {
                        logging.log(&format!("TxChrono send failed, closing: {:?}", result.unwrap_err()));
                        return;
                    }
                    if result.is_err() {
                        // WouldBlock -- the send buffer is full, so the
                        // rest of this batch would almost certainly hit
                        // the same thing; stop for this tick rather than
                        // spinning through requests_needed more attempts
                        // that will likely all fail too. tx_chrono_outstanding
                        // is deliberately NOT incremented for this one --
                        // it was never actually sent, so there's no real
                        // reply to wait for.
                        break;
                    }
                    tx_chrono_outstanding += 1;
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
            let freq = rx_frequency_hz.load(Ordering::Relaxed);
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

/// Samples requested per TxChrono message -- NOT tied to tx.rs's own
/// TX_BUFFER_SIZE (that's this project's WDSP-consumption granularity;
/// tci_tx_audio, a plain queue, already decouples producer and consumer
/// chunk sizes from each other), and NOT tied to TX_CHRONO_TARGET_
/// BUFFER_SAMPLES either -- request size and standing buffer depth are
/// independent knobs.
///
/// BUG FIX (round 3): raised to 2048, reverted to 512 on a wrong theory
/// (per-reply envelope shaping), now raised back to 2048 on the real
/// one. A temporary per-sample-pair silence diagnostic (since removed)
/// proved neither of those first two theories right: with the queue
/// confirmed healthy (a temporary outstanding/queued diagnostic, also
/// since removed, showed it never starving) and 512, 85-90% of
/// individual TxAudioStream replies came back near-silent
/// throughout an entire Tune -- WSJT-X legitimately padding most
/// replies with real silence, not corruption (which the existing
/// per-pair sanity check already handles separately). The likely cause:
/// at 512 samples/request, this project's own real-time consumption
/// (tx.rs draining TX_BUFFER_SIZE every ~10.7ms) forces a fresh
/// TxChrono roughly every ~10.7ms -- apparently faster than WSJT-X's
/// own internal Tune-audio generation can keep up with, so most
/// requests arrive before it has anything new and get silence-padded
/// (spec explicitly permits this: "may send a signal with zero counts,
/// which corresponds to no signal"). At 2048, each reply covers
/// ~42.7ms, so a fresh request is only needed about a quarter as often
/// -- much closer to Thetis's own proven-working cadence (its default
/// AUDIO_STREAM_SAMPLES, confirmed via a real captured working
/// session). The earlier "periodic pulsing" seen at 2048 was recorded
/// before that silence diagnostic existed to check WHY -- never actually
/// confirmed to be worse than 512's own pulsing, just assumed so from
/// an inconclusive visual read of a raw forward-power diagnostic's
/// bounce pattern; the silence diagnostic's first version also measured
/// whole-message peak rather than per-sample, which is itself biased
/// toward making larger chunk sizes look artificially better.
///
/// Overall conclusion (also since confirmed directly via WDSP's own
/// mic_pk meter -- see SetTXAALCMaxGain's doc comment in tx.rs): the
/// real defect is WSJT-X's own TCI Tune-audio generation genuinely,
/// repeatedly alternating in level -- not a request-cadence problem
/// this project can fix by tuning TCI_TX_AUDIO_CHUNK further. 2048 is
/// kept because it's the best-evidenced value (matches a real working
/// Thetis session), not because it resolved the underlying issue.
const TCI_TX_AUDIO_CHUNK: u32 = 2048;

/// Target amount of TX audio to keep pre-buffered in tci_tx_audio while
/// transmitting -- see tx_chrono_outstanding's doc comment for the full
/// story. 4800 samples = 100ms at 48kHz, matching Thetis's own default
/// target exactly (cmaster.cs: TX_STREAM_AUDIO_BUFFERING's 50ms default
/// + its hardcoded TCI_TX_EXTRA_BUFFER_MS=50).
const TX_CHRONO_TARGET_BUFFER_SAMPLES: usize = 4_800;

/// Cap on simultaneously outstanding (sent, not yet replied-to) TxChrono
/// requests -- see tx_chrono_outstanding's doc comment. Matches Thetis's
/// own TCI_TX_MAX_OUTSTANDING exactly (cmaster.cs).
const TX_CHRONO_MAX_OUTSTANDING: u32 = 64;

/// Safety cap on how many consecutive bad TxAudioStream pairs get held
/// back for interpolation (see pending_bad_count's doc comment) before
/// falling back to holding the last good sample for the excess -- 2048
/// pairs (~43ms at 48kHz), comfortably more than the observed ~1-in-8
/// corrupted-message rate, so this only engages on a genuinely abnormal
/// run of consecutive bad messages, not ordinary operation. Deliberately
/// NOT tied to TCI_TX_AUDIO_CHUNK (unrelated concepts: that's a TxChrono
/// request size, this is an interpolation-window safety bound) --
/// keeping this at its original effective value even after
/// TCI_TX_AUDIO_CHUNK's own increase above.
const PENDING_BAD_LIMIT: usize = 2_048;

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
    requested_frequency_hz: &Arc<AtomicU32>,
    demod_params: &Arc<Mutex<DemodParams>>,
    mox: &Arc<AtomicBool>,
    rit_enabled: &Arc<AtomicBool>,
    rit_offset_hz: &Arc<AtomicI32>,
    xit_enabled: &Arc<AtomicBool>,
    xit_offset_hz: &Arc<AtomicI32>,
    tci_wants_mic: &Arc<AtomicBool>,
    audio_streaming: &mut bool,
    iq_streaming: &mut bool,
    sink: &ClientAudioIqSink,
) -> Option<String> {
    let mut parts = cmd.splitn(2, ':');
    let name = parts.next().unwrap_or("").to_lowercase();
    let args: Vec<&str> = parts.next().unwrap_or("").split(',').collect();

    match name.as_str() {
        // vfo:receiver,vfo,frequency;
        "vfo" => {
            let f = args.get(2)?.parse::<f64>().ok()?;
            let f = f.round().max(0.0) as u32;
            requested_frequency_hz.store(f, Ordering::Relaxed);
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
        // rit_offset:receiver,value; -- see RadioSession::rit_enabled's
        // doc comment. Clamped to +-9999 Hz, matching main.rs's own UI
        // clamp for this value.
        "rit_offset" => {
            let v = args.get(1)?.trim().parse::<f64>().ok()?;
            let v = v.round().clamp(-9999.0, 9999.0) as i32;
            rit_offset_hz.store(v, Ordering::Relaxed);
            Some(format!("rit_offset:{},{};", args.first().unwrap_or(&"0"), v))
        }
        // rit_enable:receiver,bool;
        "rit_enable" => {
            let v = args.get(1)?.trim().eq_ignore_ascii_case("true");
            rit_enabled.store(v, Ordering::Relaxed);
            Some(format!("rit_enable:{},{};", args.first().unwrap_or(&"0"), v))
        }
        // xit_offset:receiver,value;
        "xit_offset" => {
            let v = args.get(1)?.trim().parse::<f64>().ok()?;
            let v = v.round().clamp(-9999.0, 9999.0) as i32;
            xit_offset_hz.store(v, Ordering::Relaxed);
            Some(format!("xit_offset:{},{};", args.first().unwrap_or(&"0"), v))
        }
        // xit_enable:receiver,bool;
        "xit_enable" => {
            let v = args.get(1)?.trim().eq_ignore_ascii_case("true");
            xit_enabled.store(v, Ordering::Relaxed);
            Some(format!("xit_enable:{},{};", args.first().unwrap_or(&"0"), v))
        }
        // trx:receiver,state,source; -- source (arg3) is optional, per
        // spec section 4.2: "tci" means take TX audio from the TCI
        // audio stream; mic1/mic2/micPC/ecoder2 mean take it from that
        // input instead. Stored in tci_wants_mic (true only for an
        // explicit non-tci source) -- see that field's doc comment in
        // radio.rs for why an absent arg3 deliberately does NOT disable
        // tci_tx_audio the way a strict reading of "signal is always
        // taken from the microphone... unless TCI is specified" would
        // suggest: TCI Remote, the one client this project's TX-audio
        // path is confirmed working against, never sends arg3 at all.
        "trx" => {
            let on = matches!(args.get(1), Some(&"true") | Some(&"1"));
            mox.store(on, Ordering::Relaxed);
            if let Some(source) = args.get(2) {
                tci_wants_mic.store(!source.eq_ignore_ascii_case("tci"), Ordering::Relaxed);
            } else {
                tci_wants_mic.store(false, Ordering::Relaxed);
            }
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
            // Discard whatever backlog the broadcaster already queued
            // into this sink before this client asked to start
            // streaming (it fills every registered sink unconditionally
            // -- see ClientAudioIqSink's doc comment) -- otherwise the
            // first send below replays stale, non-real-time audio
            // instead of starting fresh from now. Same reasoning as
            // iq_start's own identical clear.
            //
            // Only on the off->on transition, NOT on every audio_start
            // (a real client can send this more than once in a row
            // while already streaming, e.g. TCI Remote Compactor as
            // part of its own connect sequence, not just as an error
            // retry -- see iq_start's own identical guard for the full
            // story of why re-clearing on a repeat send is itself a
            // bug, not just redundant).
            if !*audio_streaming {
                sink.audio.lock().unwrap().clear();
            }
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
            // BUG FIX: a real report of the spectrum/waterfall showing
            // ~1s of garbled/compressed data right after connecting
            // (via TCI Remote Compactor) traced back to this same
            // backlog-accumulation mechanism the oversized-frame bug
            // above came from: the broadcaster fills this sink from the
            // moment the client connects, regardless of iq_streaming,
            // so by the time a client finishes its setup handshake and
            // finally sends iq_start, the sink can already hold a
            // second or more of stale IQ data queued up while nobody
            // was listening. Chunking (above) fixed the disconnect that
            // backlog caused, but still played the whole stale backlog
            // back-to-back once chunked -- a client's spectrum/
            // waterfall has no way to know that burst isn't real-time,
            // so it renders wrong until real-time data catches up.
            // Clearing here means streaming starts from "now" instead.
            //
            // FURTHER BUG FIX: only clear on the off->on transition, not
            // on every iq_start -- a real client can legitimately send
            // this more than once in a row while already streaming
            // (part of Compactor's own normal connect sequence, not
            // just the error-retry case this file already documents
            // elsewhere). Clearing unconditionally on a repeat send
            // discarded a live, already-flowing stream right as it
            // started, producing exactly the reported symptom: a short
            // burst, a gap (the re-clear), another burst, then correct
            // -- two false starts instead of one clean one.
            if !*iq_streaming {
                sink.iq.lock().unwrap().clear();
            }
            *iq_streaming = true;
            Some(format!("iq_start:{};", args.first().unwrap_or(&"0")))
        }
        "iq_stop" => {
            *iq_streaming = false;
            Some(format!("iq_stop:{};", args.first().unwrap_or(&"0")))
        }
        // Bare queries (no args) for state also pushed once at connect
        // (see handle_client's initial state push) -- answered here too
        // since a client is explicitly asking again, not just relying
        // on what it already got. This project has no real TX-profile
        // concept (Thetis-style PA/EQ profile switching), so "Default"
        // is a static, harmless stand-in.
        "tx_profile_ex" => Some("tx_profile_ex:Default;".to_string()),
        "tx_profiles_ex" => Some("tx_profiles_ex:Default;".to_string()),
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

/// For the initial state push's agc_mode field -- lowercase, matching
/// Thetis's own "normal" example (this project has no confirmed spec
/// list of valid agc_mode strings beyond that one sample, so this just
/// mirrors this project's own Agc variant names in the same casing
/// convention rather than guessing at Thetis-specific names).
fn agc_to_tci(agc: Agc) -> &'static str {
    match agc {
        Agc::Off => "off",
        Agc::Long => "long",
        Agc::Slow => "slow",
        Agc::Medium => "medium",
        Agc::Fast => "fast",
    }
}

/// For the initial state push's rx_volume field -- TCI reports gain in
/// dB (see Thetis's own "-14.42" example), but this project's own gain
/// is a linear amplitude multiplier (see spectrum.rs's DemodParams::gain
/// doc comment), same as the main window's Audio Gain slider before its
/// own dB conversion. Standard 20*log10 amplitude-to-dB, floored well
/// below audible rather than propagating -infinity for a exactly-zero
/// gain.
fn gain_to_tci_db(gain: f32) -> f32 {
    if gain <= 0.0001 { -100.0 } else { 20.0 * gain.log10() }
}
