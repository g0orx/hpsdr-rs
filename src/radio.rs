/*
    Protocol 1 (Metis/old protocol) and Protocol 2 start-and-stream
    implementation.

    RX frame layout and register encoding confirmed against the user's
    own old_protocol.c / new_protocol.c and the official openHPSDR
    Ethernet Protocol v4.3 spec, rather than reconstructed from public
    docs alone -- see inline notes for the handful of RX pieces that
    are still educated assumptions rather than verified.

    TX (MOX/PTT + TX audio/IQ streaming) is NOT held to that same bar.
    None of it has a confirmed reference -- see the module notes on
    fill_tx_payload (P1) and the "Protocol 2 TX (DUC) IQ streaming"
    section (P2) below for exactly what's guessed and how each guess
    is designed to fail closed (no transmission) rather than fail open
    (unintended transmission) if wrong. Bench-test into a dummy load
    at reduced drive before ever keying into an antenna.
*/

use crate::discovery::{Boards, Device};
use std::collections::VecDeque;
use std::io;
use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const DATA_PORT: u16 = 1024; // same port as discovery, confirmed
const USB_FRAME_SIZE: usize = 512;
const HEADER_SIZE: usize = 8; // 0xEF 0xFE 0x01 <endpoint> <4-byte seq>
const PACKET_SIZE: usize = HEADER_SIZE + USB_FRAME_SIZE * 2; // 1032

const EP_COMMAND_AUDIO: u8 = 0x02; // host -> radio
const EP_IQ_DATA: u8 = 0x06; // radio -> host, narrowband IQ
const EP_WIDEBAND: u8 = 0x04; // radio -> host, wideband (ignored for now)

// Protocol 2 -- fixed ports per the openHPSDR Ethernet Protocol v4.3 spec.
const P2_GENERAL_PORT: u16 = 1024;
const P2_DDC_SPECIFIC_PORT: u16 = 1025;
const P2_TX_SPECIFIC_PORT: u16 = 1026;
const P2_HIGH_PRIORITY_PORT: u16 = 1027;
const P2_DDC0_IQ_PORT: u16 = 1035; // DDC1 = 1036, DDC2 = 1037, ...
// Confirmed by the user: separate from P2_TX_SPECIFIC_PORT above, which
// only carries the small TX-specific *config* packet (DAC count, DUC
// rate/size) -- the actual outgoing DUC IQ audio stream itself goes
// here instead. An earlier version of this file guessed 1026 (reusing
// the config port) for this, which was wrong -- see p2_tx_iq_loop.
const P2_TX_IQ_PORT: u16 = 1029;
// Destination port for streaming locally-demodulated RX audio TO the
// radio's own local audio output -- confirmed against piHPSDR's
// new_protocol.c/new_protocol.h (AUDIO_FROM_HOST_PORT). Named "from
// host" from the radio's perspective (audio flowing FROM the host TO
// the radio), the mirror image of P2_TX_IQ_PORT's own naming. See
// p2_rx_audio_loop.
const P2_AUDIO_PORT: u16 = 1028;
// Incoming (radio -> host) source port for the radio's own
// high-priority status packets -- confirmed by the user: the host is
// expected to respond with a fresh outgoing High Priority packet (port
// P2_HIGH_PRIORITY_PORT above) whenever one of these arrives, in
// addition to sending on content change. Same numeric value as
// P2_DDC_SPECIFIC_PORT above by protocol convention, but a completely
// different thing -- that's the *outgoing* DDC-config destination
// port, this is an *incoming* source port. See p2_receiver_loop.
const P2_HP_STATUS_SOURCE_PORT: u16 = 1025;
const P2_PACKET_SIZE: usize = 1444; // General/DDC-specific/High-Priority and DDC IQ -- NOT the TX-specific packet, see P2_TX_SPECIFIC_PACKET_SIZE
const P2_KEEPALIVE_INTERVAL: Duration = Duration::from_millis(250);
const P2_DSP_CLOCK_HZ: f64 = 122_880_000.0; // Hermes/Angelia/Orion; fixed for v1

/// How many IQ samples to keep buffered per receiver before dropping the
/// oldest. ~2 seconds at 48kHz; tune once real DSP consumption exists.
/// How many IQ samples to keep buffered per receiver before dropping the
/// oldest. Deliberately small (~0.25s at 48kHz) -- this is a FIFO with
/// no catch-up mechanism, so any backlog that accumulates becomes
/// permanent added latency rather than self-correcting. A small cap
/// bounds worst-case latency rather than papering over a timing
/// mismatch by delaying everything.
const IQ_BUFFER_CAPACITY: usize = 12_000;

/// TX-direction counterpart of IQ_BUFFER_CAPACITY -- same "small,
/// drop-oldest" reasoning. Sized generously since TX IQ can be
/// produced at a higher rate (DUC rate, e.g. 192ksps) than RX IQ is
/// consumed from, but still bounded so a stall doesn't grow key-down
/// latency without limit.
const TX_IQ_BUFFER_CAPACITY: usize = 100_000;

/// Same "small, bounded, drop-oldest" reasoning as the other buffers
/// here -- a backlog becomes added key-down latency, not something
/// that self-corrects. Mono audio at 48kHz, ~0.5s, matching audio.rs's
/// own MIC_BUFFER_CAPACITY for the same reason (this is the TCI-client
/// counterpart of that local-mic buffer).
const TCI_TX_AUDIO_CAPACITY: usize = 24_000;

/// Same "small, bounded, drop-oldest" reasoning as TCI_TX_AUDIO_CAPACITY
/// above -- radio-sourced mic audio (see RadioSession::radio_mic_audio).
const RADIO_MIC_AUDIO_CAPACITY: usize = 24_000;

/// RadioSession::tx_audio_source values. Auto is the long-standing
/// default (TCI-sourced audio preferred whenever a TCI client is
/// actively sending it, falling back to the local mic otherwise --
/// see tx.rs's run() for the exact priority). LocalMic exists because
/// WSJT-X's own TCI audio generation has a confirmed, real bug (its
/// TCITransceiver.cpp reuses a stale slot in an 8-entry ring buffer
/// roughly 1-in-8 messages) that a real side-by-side recording
/// confirmed by ear: the local mic path -- fed from WSJT-X's own
/// soundcard output via rigctl+pipewire, same audio content, no TCI
/// audio stream involved at all -- was clean, while the TCI-sourced
/// recording of the same transmission was audibly rough. This lets a
/// TCI client (WSJT-X) keep driving frequency/mode/PTT while sidestepping
/// its own broken audio path entirely, routing local mic input (e.g.
/// WSJT-X's own audio output looped back via pipewire) into the TX
/// chain instead -- not just falling back to it on underrun, like Auto
/// already does, but using it exclusively regardless of whether the
/// TCI client is also sending audio.
pub const TX_AUDIO_SOURCE_AUTO: u8 = 0;
pub const TX_AUDIO_SOURCE_RADIO_MIC: u8 = 1;
pub const TX_AUDIO_SOURCE_LOCAL_MIC: u8 = 2;

/// Just an initial-capacity hint for the queue below (like the other
/// constants here) -- the actual enforced drop-oldest bound is
/// spectrum.rs's own AUDIO_BUFFER_CAPACITY (also 14,400, same value),
/// since that's where samples are pushed in. Kept as its own constant
/// rather than importing spectrum.rs's private one, matching this
/// file's existing pattern of not sharing buffer-size constants across
/// modules.
const RX_AUDIO_TO_RADIO_CAPACITY: usize = 14_400;

/// Confirmed against rustyHPSDR: TWO complete rotations before the
/// Start command -- see start_protocol1's pre-config-rotation doc
/// comment. TESTED AND RULED OUT: bumped to 5 as an experiment (raw
/// UDP packet loss/reordering during this one-time window, theorized
/// to explain an intermittent comb-pattern/sawtooth-audio artifact on
/// 2-ADC P1 boards) -- real hardware testing showed no meaningful
/// improvement (~1-in-5 successful connections either way), which
/// rules out packet loss in THIS window as the/a dominant cause: if it
/// were, 5 independent copies of each command instead of 2 should have
/// made failure astronomically unlikely, not left it at ~80%. Reverted
/// to the confirmed reference value rather than leave an unjustified
/// deviation in place.
const PRE_CONFIG_ROTATIONS: u32 = 2;

/// PureSignal feedback IQ arrives at 192ksps on P2 (confirmed fixed
/// rate -- new_protocol.c hardcodes it for the reserved DDC0/DDC1
/// regardless of any receiver's own configured rate) and is only ever
/// produced while transmitting -- same "small, bounded, drop-oldest"
/// reasoning as TX_IQ_BUFFER_CAPACITY, sized the same for consistency
/// since both run at a similar order-of-magnitude rate.
const PS_FEEDBACK_BUFFER_CAPACITY: usize = 100_000;

#[derive(Copy, Clone, Debug)]
pub struct IqSample {
    pub i: i32,
    pub q: i32,
}

#[derive(Copy, Clone, Debug)]
pub struct RadioSettings {
    pub frequency_hz: u32,
    pub sample_rate: u32,
    pub receivers: u8,
    /// Requests the 2 extra "feedback" pseudo-receivers PureSignal
    /// needs (TX-DAC loopback + off-air RX feedback) instead of
    /// `receivers` real, user-visible ones -- see
    /// RadioSession::ps_rx_feedback_iq/ps_tx_feedback_iq's doc comment
    /// and ps_feedback_config's doc comment for the full story. Fixed
    /// at session-start time, same as `receivers` itself -- toggling
    /// this requires a reconnect, not a live setting.
    pub puresignal_enabled: bool,
    /// Initial value for RadioSession::rx_attenuation (P1, standard
    /// boards only) -- see that field's doc comment. main.rs loads
    /// this from Config, falling back to this struct's own default
    /// for a never-saved config.
    pub rx_attenuation: u32,
    /// Initial value for RadioSession::ps_tx_attenuation (P1, standard
    /// boards only, PureSignal) -- see that field's doc comment.
    pub ps_tx_attenuation: u32,
    /// Initial value for RadioSession::diversity_enabled -- see that
    /// field's doc comment. Fixed at session-start time, same as
    /// `puresignal_enabled` (and mutually exclusive with it -- see
    /// main.rs's Settings UI).
    pub diversity_enabled: bool,
    /// Initial values for RadioSession::diversity_gain_db/
    /// diversity_phase_deg -- unlike diversity_enabled these ARE live,
    /// this is just their starting point on connect.
    pub diversity_gain_db: f32,
    pub diversity_phase_deg: f32,
}

impl Default for RadioSettings {
    fn default() -> Self {
        Self {
            frequency_hz: 7_100_000, // 40m, arbitrary sensible default
            sample_rate: 48_000,
            receivers: 1, // hardcoded single-receiver for this first version
            puresignal_enabled: false,
            // Non-zero rather than 0dB -- see RadioSession::rx_attenuation's
            // doc comment for why 0dB caused real front-end overload.
            rx_attenuation: 12,
            ps_tx_attenuation: 0,
            diversity_enabled: false,
            // Neutral starting point -- matches piHPSDR's own default,
            // user tunes by ear/S-meter after enabling.
            diversity_gain_db: 0.0,
            diversity_phase_deg: 0.0,
        }
    }
}

/// PureSignal feedback receiver-index table -- CONFIRMED against
/// piHPSDR (the only implementation with actual hardware behind it
/// checked so far): old_protocol.c's how_many_receivers/
/// rx_feedback_channel/tx_feedback_channel for P1's fixed, board-
/// dependent indices, and new_protocol.c's PS-specific branch
/// (`transmitter->puresignal && isTransmitting()`) for P2's DDC0/DDC1
/// reservation, which is NOT board-dependent the way P1 is.
///
/// Returns `(rx_feedback_idx, tx_feedback_idx, max_real_receivers)`,
/// all 0-based, or `None` if PureSignal isn't known to be supported on
/// this board/protocol combination. `max_real_receivers` differs in
/// meaning by protocol:
/// - P1: a hard, board-dependent CAP on real/user-visible receivers
///   while PS is active -- the total receiver count requested from the
///   radio is fixed per board (e.g. 5 for Angelia/Orion/Orion2, with
///   feedback occupying the last 2), so real RX is capped at
///   `total - 2`. On the smallest boards (Metis/HermesLite, total=2)
///   this is 0 -- no real RX at all while PS is active.
/// - P2: always `None` here -- DDC0/DDC1 are reserved for feedback and
///   real receivers are offset to start at DDC2, but unlike P1 the cap
///   isn't a fixed board constant (P2's DDC count varies by board), so
///   it can't be encoded in this table. BUG FIX: this used to be
///   (wrongly) documented as "bounded only by the board's own
///   supported_receivers maximum" -- it was NOT actually bounded at
///   all: `settings.receivers` (from the board's discovery reply) was
///   used unreduced for both `iq_buffers` sizing and the "Add
///   Receiver" UI cap, while p2_sender_loop separately ADDS 2 reserved
///   DDCs on top whenever PS is active. A user could "Add Receiver" up
///   to the board's full advertised DDC count and PS enabled would
///   then request `count + 2` DDCs -- a real over-request past what
///   the board actually has. Fixed at the call site instead
///   (start_protocol2 reduces `settings.receivers` by 2 up front when
///   PS is active, before it ever reaches `iq_buffers`/"Add Receiver"),
///   since the actual cap value needs the board's live discovered
///   receiver count, which this function doesn't have.
///
/// Both protocols agree on one more thing this function doesn't encode
/// (handled at the call site instead, since it needs the live TX
/// frequency, not just a static table lookup): the feedback DDCs are
/// always tuned to the TX frequency, never an independent RX
/// frequency -- confirmed via old_protocol.c's channel_freq ("all
/// other channels are used for PURESIGNAL and get the TX freq") and
/// new_protocol.c's high-priority packet builder ("Set DDC0 and DDC1
/// (synchronized) to the transmit frequency").
fn ps_feedback_config(protocol: u8, board: Boards) -> Option<(u8, u8, Option<u8>)> {
    match protocol {
        1 => match board {
            Boards::Metis | Boards::HermesLite => Some((0, 1, Some(0))),
            Boards::Hermes | Boards::Hermes2 | Boards::HermesLite2 => Some((2, 3, Some(2))),
            Boards::Angelia | Boards::Orion | Boards::Orion2 => Some((3, 4, Some(3))),
            Boards::Saturn | Boards::Unknown => None,
        },
        // P2: DDC0/DDC1 reservation is universal, not board-dependent --
        // confirmed via new_protocol.c, no per-board variation in that
        // branch unlike P1's.
        2 => Some((0, 1, None)),
        _ => None,
    }
}

pub struct RadioSession {
    pub iq_buffers: Vec<Arc<Mutex<VecDeque<IqSample>>>>,
    pub frequency_hz: Arc<AtomicU32>,
    pub sample_rate: Arc<AtomicU32>,
    /// Which ADC (0-indexed) the primary receiver's DDC pulls from.
    pub adc: Arc<AtomicU32>,
    /// Antenna port selection (0=ANT1, 1=ANT2, 2=ANT3). This is a
    /// single shared value, not per-receiver -- Alex's antenna relays
    /// are one physical shared resource, only meaningful when ADC0 is
    /// in use (only ADC0's signal path runs through the Alex relay
    /// bank on this board family). Whichever receiver last changes it
    /// affects every receiver sharing ADC0.
    pub antenna: Arc<AtomicU32>,
    /// Protocol 1, standard (non-HermesLite) boards only -- RX step
    /// attenuator, 0-31 dB, encoded into the C4 byte of command 4
    /// (0x14) as `0x20 | attenuation` (bit 5 = attenuator-enable,
    /// confirmed against piHPSDR's old_protocol.c: `output_buffer[C4]
    /// = 0x20 | ((int)adc[0].gain & 0x1F)` while receiving). ROOT CAUSE
    /// FIX: this was previously hardcoded to a fixed 0x20 (0dB, no
    /// attenuation at all) -- confirmed via real hardware testing
    /// (ANAN-100D/Angelia on a real HF antenna) that this causes
    /// genuine front-end overload from ordinary band signals, visible
    /// as either a comb-shaped intermod pattern or sustained broadband
    /// noise depending on exactly what's on the band at the moment,
    /// randomly varying between connections since real RF conditions
    /// vary. HermesLite/HermesLite2 are unaffected -- they use a
    /// different, already-separate RX gain mechanism (bit 6 of the
    /// same byte, see p1_build_packet's is_hermes_lite branch).
    pub rx_attenuation: Arc<AtomicU32>,
    /// PureSignal -- TX-time step attenuator (0-31 dB) applied to
    /// ADC0's input while transmitting (ADC0 doubles as PureSignal's
    /// feedback source during TX on this board family, on both
    /// protocols). Standard (non-HermesLite) boards only.
    ///
    /// P1: encoded into command 6 (0x1C)'s C3 byte -- confirmed against
    /// piHPSDR's old_protocol.c: `output_buffer[C3] |=
    /// transmitter->attenuation; // Step attenuator of first ADC, value
    /// used when TXing`.
    /// P2: encoded into the High Priority packet's byte 1443 -- confirmed
    /// against piHPSDR's new_protocol.c: `high_priority_buffer_to_radio[1443]
    /// = transmitter->attenuation;` while transmitting (byte 1442, ADC1's
    /// attenuator, is separately forced to 31/max while transmitting "to
    /// protect RX2 in DIVERSITY setups").
    ///
    /// hpsdr-rs previously never implemented either byte at all
    /// (hardcoded 0x00/unwritten) -- a real, confirmed gap on BOTH
    /// protocols: with no attenuation control on the feedback path at
    /// all, the feedback signal is far stronger than WDSP's PS engine
    /// expects (confirmed via real hardware testing on both P1/Angelia
    /// and P2/Orion2: raw feedback amplitude 40-50% of full ADC scale,
    /// pinning GetPSInfo's reported feedback level at its maximum
    /// regardless of drive level or the HW Peak calibration constant).
    /// piHPSDR's own "Auto Attenuate" logic targets a feedback level
    /// near 152 (its comment: "175 means 1.2dB too strong, 132 means
    /// 1.2dB too weak") by adjusting exactly this value -- not by
    /// touching HW Peak, which is a fixed per-hardware-model reference
    /// constant, not a per-session tuning knob.
    pub ps_tx_attenuation: Arc<AtomicU32>,
    /// Additional receivers beyond the first -- works on both protocols
    /// (P1's classic Metis/Ozy DDC round-robin genuinely supports
    /// independent per-receiver tuning too, see p1_build_packet's
    /// ozy_command==2 branch; this doc comment previously said P1 had no
    /// confirmed way to do this, which was stale even before this file's
    /// own "Add Receiver" UI gating was corrected -- see main.rs).
    /// Index 0 here corresponds to receiver index 1 overall (receiver
    /// 0 is frequency_hz/sample_rate/adc above). Pre-sized up to
    /// whatever the board reported supporting; active_receiver_count
    /// tracks how many of these are actually turned on right now.
    pub extra_frequencies_hz: Vec<Arc<AtomicU32>>,
    pub extra_sample_rates_hz: Vec<Arc<AtomicU32>>,
    pub extra_adcs: Vec<Arc<AtomicU32>>,
    pub active_receiver_count: Arc<AtomicU32>,
    /// Diversity reception (2-ADC boards only) -- combines ADC1's IQ into
    /// ADC0's before demodulation to help null multipath fades/local
    /// noise that hit each antenna differently. Ported from piHPSDR's own
    /// diversity feature (`~/github/pihpsdr/diversity_menu.c`,
    /// `receiver.c: add_div_iq_samples`), which the user originally wrote.
    ///
    /// Live on BOTH protocols -- see `RadioSession::set_diversity_enabled`.
    /// Enabling reserves wire index 1 as a hidden ADC1-only feed (forced
    /// ADC via p1_build_packet's command 6/extra_adcs override, or P2's
    /// equivalent; its frequency/rate track `frequency_hz`/`sample_rate`
    /// directly, never independently tunable). `active_receiver_count`
    /// gets bumped to at least 2 when this turns on, keeping "Add
    /// Receiver" from handing wire 1 out to the user.
    ///
    /// P1: `sender_loop` watches this itself (comparing against the value
    /// it last saw) and, on a change, replays the whole preconfig-
    /// rotations+250ms+Start burst on its OWN existing socket, exactly
    /// mirroring piHPSDR's own `diversity_cb` (`old_protocol_stop()` +
    /// flip + `old_protocol_run()` -> `metis_restart()`, all on the one
    /// socket/thread the app has had since startup, never recreated).
    /// Confirmed via extensive real-hardware testing that the OLD
    /// approach here -- a full session teardown+rebuild through
    /// `connect_to_device()`, mirroring PureSignal's own "Enable"
    /// checkbox (see commit 7853f60) -- reliably hung this board's P1/
    /// Metis firmware within about a second, every time, regardless of
    /// direction (enabling OR disabling), while a fresh connect to
    /// either state ran indefinitely; four separate wire-level fixes
    /// (the sync bit itself, ADC1 forcing, preconfig/ongoing receiver-
    /// count mismatch, the pre-Start settle delay) and a same-port Stop
    /// packet made no difference, isolating the actual cause to the full
    /// reconnect's new socket (new ephemeral port) and full subsystem
    /// teardown (mic/audio/rigctl/TCI/spectrum) -- neither of which
    /// piHPSDR's own diversity toggle ever does.
    ///
    /// P2: `p2_sender_loop`/`p2_receiver_loop` just read this fresh every
    /// cycle -- P2 has no discrete preconfig/Start handshake to replay
    /// at all (it continuously sends updated General/DDC-specific/High-
    /// Priority packets on a timer regardless), so nothing beyond a
    /// live read was ever needed here.
    pub diversity_enabled: Arc<AtomicBool>,
    /// Gain (dB) and phase (degrees) of the ADC1-aux rotate-and-add --
    /// see the combiner thread (spawned in `RadioSession::start`) for the
    /// exact formula, identical to piHPSDR's `set_gain_phase()`/
    /// `add_div_iq_samples`. Bit-cast f32 in an AtomicU32, same pattern
    /// as `pa_gain_db` -- these ARE truly live, no reconnect needed to
    /// retune them (the combiner reads them fresh every pass). Range
    /// -27.0..27.0 dB / -180.0..180.0 degrees, matching piHPSDR's own
    /// slider ranges. Meaningless (not read) while diversity_enabled is
    /// false.
    pub diversity_gain_db: Arc<AtomicU32>,
    pub diversity_phase_deg: Arc<AtomicU32>,
    /// Wire 0's (ADC0/main) raw IQ, redirected here instead of
    /// `iq_buffers[0]` by the demux while diversity is enabled -- the
    /// combiner thread is the ONLY consumer (mirrors
    /// `ps_rx_feedback_iq`'s "own dedicated queue, not a second reader"
    /// shape). The combiner pairs this with `iq_buffers[1]` (wire 1's raw
    /// ADC1 feed, otherwise unused while reserved) and pushes the
    /// combined result into `iq_buffers[0]` -- so `SpectrumHandle` for
    /// receiver 0 needs no changes at all, it just finds the combined
    /// signal where ADC0's raw signal would normally be. Unused (never
    /// written to) while diversity_enabled is false.
    pub diversity_main_raw_iq: Arc<Mutex<VecDeque<IqSample>>>,
    /// PureSignal feedback IQ -- see ps_feedback_config's doc comment
    /// for which receiver/DDC index these actually come from per
    /// protocol/board. Raw ADC-scale IqSample, same as every other
    /// receiver's buffer above (not yet normalized to float) --
    /// intentionally NOT part of `iq_buffers`, which is sized to
    /// user-visible receivers only and drives the Add Receiver UI;
    /// these are a separate, always-present pair of dedicated queues
    /// (matching the tx_iq/tci_tx_audio convention: never share a
    /// queue between two independent consumers). Empty/unused
    /// whenever `puresignal_enabled` was false at connect time -- no
    /// behavior change for existing non-PS sessions.
    ///
    /// On Protocol 2, both this and `ps_tx_feedback_iq` are populated
    /// TOGETHER from the same single DDC0 packet stream (see
    /// p2_parse_ps_feedback_packet's doc comment) -- DDC1 is never
    /// independently enabled, its samples arrive interleaved within
    /// DDC0's own packets instead, hardware-synchronized. This is what
    /// makes tx.rs's drain_ps_feedback's simple positional 1:1 pairing
    /// correct: both queues fill in lockstep from one source, not from
    /// two independently-timed streams.
    pub ps_rx_feedback_iq: Arc<Mutex<VecDeque<IqSample>>>,
    pub ps_tx_feedback_iq: Arc<Mutex<VecDeque<IqSample>>>,
    /// PTT/MOX state. Read by both protocols' sender loops (to decide
    /// whether to key the radio and stream TX audio/IQ instead of
    /// silence), and by tx.rs's TXA thread (to decide whether to
    /// actually run mic audio through TXA or idle). Written from the
    /// UI's PTT control and from rigctl/TCI's set_ptt/trx commands.
    pub mox: Arc<AtomicBool>,
    /// TX audio/IQ produced by tx.rs, consumed by whichever sender
    /// loop(s) below are currently keyed. See tx.rs and
    /// fill_tx_payload's module notes for the confidence caveats on
    /// what format this actually needs to be in per protocol.
    pub tx_iq: Arc<Mutex<VecDeque<f32>>>,
    /// TX audio *received from a TCI client* (mono, downmixed from the
    /// stereo wire format) -- see tx.rs's run() for how this takes
    /// priority over the local mic_buffer on any chunk where it has
    /// data, and tci.rs's TX_AUDIO_STREAM handling for how it gets
    /// filled. Long-lived here (like mox/tx_iq above) so it stays
    /// stable across TX arm/disarm cycles and TCI server restarts,
    /// rather than being recreated each time either does.
    pub tci_tx_audio: Arc<Mutex<VecDeque<f32>>>,
    /// Gain applied to tci_tx_audio content in tci.rs, before it's
    /// pushed into the queue above -- separate from tx.rs's mic_gain
    /// (WDSP's TXAPanelGain1, applied uniformly to whichever source
    /// fed `chunk` that cycle) because a real test against WSJT-X
    /// showed its TCI TX audio arriving at roughly 1/700th the
    /// amplitude mic_gain's existing 0.0..=2.0 range is calibrated
    /// for (WSJT-X's own TCITransceiver.cpp confirmed this isn't an
    /// hpsdr-rs decode bug -- its Pwr slider was already at 0dB/max in
    /// the test that found this). Defaults to 1.0 (no change from
    /// prior behavior) rather than a guessed large value, matching
    /// Audio Gain's precedent of needing real-hardware dialing-in
    /// rather than a hardcoded constant.
    pub tci_tx_gain: Arc<Mutex<f32>>,
    /// Mic audio digitized by the RADIO'S OWN mic input (as opposed to
    /// this PC's local mic/sound card) and sent to the host as part of
    /// the normal incoming stream -- confirmed against piHPSDR's
    /// old_protocol.c/new_protocol.c (`local_microphone` flag: false =
    /// use this radio-sourced sample instead of the local one). P1:
    /// interleaved into the SAME per-sample-group 2-byte slot as every
    /// other RX IQ sample (parse_iq_stream already skipped past this
    /// slot before this feature existed -- see its own doc comment).
    /// P2: a dedicated incoming UDP stream on P2_TX_SPECIFIC_PORT
    /// (source port, distinct from that same port's OUTGOING use for
    /// TX-specific config -- see p2_receiver_loop). Always filled
    /// regardless of `tx_audio_source` below, same "cheap to fill, gate
    /// consumption instead" convention as rx_audio_to_radio.
    pub radio_mic_audio: Arc<Mutex<VecDeque<f32>>>,
    /// Live TX audio source selector (Settings -> TX) -- one of
    /// TX_AUDIO_SOURCE_AUTO/RADIO_MIC/LOCAL_MIC (see those constants'
    /// doc comment for what each one means and why LOCAL_MIC exists).
    /// Defaults to Auto, matching every other opt-in feature added so
    /// far. Previously a plain bool named `use_radio_mic` before a
    /// third value was added -- see git history if that name shows up
    /// in an old comment/commit.
    pub tx_audio_source: Arc<AtomicU8>,
    /// Set (and cleared) by tci.rs's `trx` command handler from that
    /// command's optional 3rd argument, per the official TCI Protocol
    /// spec (v2.0, section 4.2, TRX command): "arg3 - signal source
    /// (optional): tci - take signal from TCI audio stream; mic1/mic2/
    /// micPC/ecoder2 - take signal from that input... If an argument is
    /// absent, the signal is taken from the microphone." True only when
    /// a client explicitly names a non-TCI source (mic1/mic2/micPC/
    /// ecoder2); false when it says `tci`, or omits arg3 entirely.
    /// Consulted only by tx.rs's Auto branch (TX_AUDIO_SOURCE_AUTO) to
    /// skip tci_tx_audio even if it currently has content -- explicit
    /// user selections in Settings -> TX (RADIO_MIC/LOCAL_MIC above)
    /// already bypass tci_tx_audio unconditionally and don't consult
    /// this. Deliberately does NOT flip Auto's default the other way
    /// (i.e. arg3 absent does not disable tci_tx_audio) -- TCI Remote,
    /// this project's one confirmed-working TCI TX-audio client, never
    /// sends arg3 at all, so defaulting to "spec-literal: no arg3 means
    /// don't use TCI audio" would silently break that already-working
    /// path for the sake of a stricter reading with no reference client
    /// known to need it.
    pub tci_wants_mic: Arc<AtomicBool>,
    /// Radio's mic-jack PTT input enabled/disabled -- confirmed against
    /// piHPSDR (`mic_ptt_enabled`, its own default off). Standard
    /// (Angelia/Orion/Orion2) boards only. P1: command 4 (0x14)'s C1
    /// bit 0x40 -- inverted logic, set means DISABLED (this project
    /// previously hardcoded that bit permanently set, i.e. permanently
    /// disabled, before this control existed -- see p1_build_packet's
    /// command-4 doc comment). P2: TX-specific packet byte 50 bit 0x04,
    /// same inverted logic, confirmed against new_protocol.c.
    pub mic_ptt_enabled: Arc<AtomicBool>,
    /// Radio's mic-jack bias voltage (for electret mic elements)
    /// enabled/disabled -- confirmed against piHPSDR (`mic_bias_enabled`,
    /// its own default off). Standard boards only. P1: command 4's C1
    /// bit 0x20. P2: TX-specific packet byte 50 bit 0x10.
    pub mic_bias_enabled: Arc<AtomicBool>,
    /// Mic connector wiring: false (default, matches piHPSDR's own
    /// default) = "PTT on Ring, Mic and Bias on Tip"; true = "PTT on
    /// Tip, Mic and Bias on Ring" -- confirmed against piHPSDR
    /// (`mic_ptt_tip_bias_ring`, radio_menu.c's own exact button
    /// labels). Standard boards only. P1: command 4's C1 bit 0x10. P2:
    /// TX-specific packet byte 50 bit 0x08.
    pub mic_ptt_on_tip: Arc<AtomicBool>,
    /// Locally-demodulated audio for the MAIN receiver only (not extra
    /// receivers -- the radio has exactly one local audio output,
    /// matching piHPSDR's "active receiver" concept), fed directly by
    /// spectrum.rs's analyzer thread (see SpectrumHandle::start's
    /// `rx_audio_to_radio` parameter) whenever this session's main
    /// SpectrumHandle was built with it wired up. Consumed by P1's
    /// sender_loop (interleaved into the same per-sample slot
    /// fill_tx_payload uses while transmitting -- see
    /// fill_rx_audio_payload) and P2's p2_rx_audio_loop (a dedicated
    /// UDP stream to P2_AUDIO_PORT, confirmed against piHPSDR's
    /// new_protocol.c). Always filled regardless of
    /// `send_rx_audio_to_radio` below (the queue is capacity-bounded,
    /// drop-oldest, so leaving it unread when the feature is off just
    /// means it sits idle) -- only whether it's actually SENT is gated.
    pub rx_audio_to_radio: Arc<Mutex<VecDeque<f32>>>,
    /// Live toggle for streaming rx_audio_to_radio back to the radio's
    /// own local audio output (Settings -> RX -- "Send RX audio to
    /// radio"). Default off: most setups have no use for a radio-side
    /// headphone/speaker jack and this adds continuous extra traffic.
    /// No effect on HermesLite/HermesLite2 over Protocol 1 regardless
    /// of this setting -- see p1_build_packet's send_rx_audio note
    /// (that board's firmware repurposes the same wire bytes for
    /// extended-address writes, confirmed via piHPSDR's own explicit
    /// precaution). Protocol 2 has no such restriction (separate UDP
    /// port, not sharing bytes with anything else).
    pub send_rx_audio_to_radio: Arc<AtomicBool>,
    /// Desired TX output power in watts, converted to each protocol's
    /// actual drive byte via drive_byte_for_watts -- see that
    /// function's doc comment. Confirmed by the user to belong at byte
    /// 345 of the P2 High Priority packet (P1's equivalent is command
    /// address=3); previously never set at all on P2 (left at 0), which
    /// the radio may have refused to transmit at, same as the
    /// previously-unset TX frequency bytes. Starts deliberately low
    /// (see RadioSession::start) rather than defaulting to max power.
    pub tx_power_watts: Arc<AtomicU32>,
    /// Current band's PA gain in dB (f32 bits, via
    /// f32::to_bits/from_bits), fed into drive_byte_for_watts in place
    /// of a flat constant. main.rs owns the actual per-band calibration
    /// table (keyed by band name, alongside its other per-band UI
    /// state) and keeps this updated to whichever band the current
    /// frequency falls in -- radio.rs has no concept of bands, so it
    /// just carries whatever single resolved value main.rs last stored
    /// here. Defaults to DEFAULT_PA_GAIN_DB.
    pub pa_gain_db: Arc<AtomicU32>,
    /// Forward-power ADC reading reported back by the radio itself
    /// while transmitting (P1: confirmed via a working reference --
    /// status address 1, bytes 3-4 of the incoming C&C header. P2:
    /// confirmed via the official protocol spec -- bytes 14-15 of the
    /// incoming High-Priority status packet). NOT converted to watts
    /// here -- that needs board-specific calibration constants,
    /// confirmed by the user and applied in main.rs's
    /// power_watts_and_swr (kept at the UI layer since it's a pure,
    /// board-dependent display-time conversion, not radio state). P2:
    /// IS a 16-sample moving average of the raw per-packet reading
    /// (`p2_receiver_loop`'s own fwd_acc), not the single latest raw
    /// value -- see that averaging's own doc comment for why (matches
    /// piHPSDR's identical `fwd_acc` smoothing, confirmed necessary via
    /// real-hardware diagnostic logging: the true raw single-packet
    /// reading genuinely alternates between near-zero and full-scale).
    pub tx_forward_power: Arc<AtomicU32>,
    /// Same as tx_forward_power but for reverse (reflected) power --
    /// P1 confirmed at status address 2, bytes 1-2; P2 confirmed at
    /// bytes 22-23 of the High-Priority status packet. Needed together
    /// with forward power to compute SWR.
    pub tx_reverse_power: Arc<AtomicU32>,
    /// ADC0/ADC1 front-end overload flags -- set when the radio's own
    /// status packet reports the front end clipping (P1: address 0/4's
    /// C1/C2 bit 0, confirmed against piHPSDR's old_protocol.c; P2:
    /// byte 5 bits 0/1 of the incoming High-Priority status packet,
    /// confirmed against piHPSDR's new_protocol.c -- `adc[0].overload
    /// |= buffer[5] & 0x01;`/`adc[1].overload |= (buffer[5] & 0x02) >>
    /// 1;`). Added specifically because PureSignal RX-feedback shares
    /// ADC0 with the main receiver: real-hardware testing found
    /// piHPSDR showing an explicit "ADC0 Overload" warning below 24dB
    /// of Feedback Attenuation on the same radio/antenna hpsdr-rs was
    /// being tested on, with no equivalent visibility here at all --
    /// clipped/overloaded samples would plausibly explain much of the
    /// marginal PS calibration behavior chased this session, and are
    /// generally useful to surface for ordinary RX too.
    pub adc0_overload: Arc<AtomicBool>,
    pub adc1_overload: Arc<AtomicBool>,
    /// P2 only (byte 4, bits 0x40/0x20 of the incoming High-Priority
    /// status packet, confirmed against piHPSDR's new_protocol.c --
    /// `tx_fifo_overrun |= (buffer[4] & 0x40) >> 6;`/`tx_fifo_underrun
    /// |= (buffer[4] & 0x20) >> 5;`). The radio's own signal that its
    /// TX IQ sample FIFO ran dry (host isn't keeping up) or overflowed
    /// -- a real, hardware-side pacing problem distinct from any
    /// software-side queue (e.g. tci_tx_audio), which can be perfectly
    /// healthy while this still happens downstream. Added while
    /// investigating a real-hardware report of TX power cycling
    /// (35-55W instead of a steady level) with an otherwise-clean
    /// spectrum over TCI-sourced audio -- exactly what a periodically
    /// starved/overrun TX FIFO would plausibly cause, and something
    /// this project had no visibility into at all before this. Left
    /// unset (always false) on P1, which has no general equivalent --
    /// piHPSDR's own P1 handling of this is HermesLite-II-specific,
    /// not applicable to other P1 boards.
    pub tx_fifo_underrun: Arc<AtomicBool>,
    pub tx_fifo_overrun: Arc<AtomicBool>,
    stop_flag: Arc<AtomicBool>,
    sender_thread: Option<JoinHandle<()>>,
    receiver_thread: Option<JoinHandle<()>>,
    /// Diversity combiner -- see spawn_diversity_combiner's doc comment.
    /// Always None unless diversity_enabled (set right after
    /// start_protocol1/2 returns, see RadioSession::start, or live via
    /// set_diversity_enabled).
    diversity_combiner_thread: Option<JoinHandle<()>>,
    /// Dedicated stop flag for diversity_combiner_thread, separate from
    /// `stop_flag` (which sender_loop/receiver_loop also use, and which
    /// must NOT stop just because diversity got toggled off live) --
    /// see set_diversity_enabled. Fresh Arc each time the combiner is
    /// (re)spawned, so a stale flag from a previous spawn can never
    /// leak into a new one. None whenever the combiner isn't running.
    diversity_combiner_stop: Option<Arc<AtomicBool>>,
    /// P2 only -- p2_tx_iq_loop's handle. Always None on P1, which
    /// streams TX audio through the existing sender_loop instead (its
    /// packet cadence is already fast enough to double as an audio
    /// stream; P2's isn't, hence the separate thread -- see
    /// p2_tx_iq_loop's doc comment).
    tx_iq_thread: Option<JoinHandle<()>>,
    /// P2 only -- p2_rx_audio_loop's handle. Always None on P1, which
    /// streams RX audio through the existing sender_loop instead (same
    /// reasoning as tx_iq_thread being P2-only).
    rx_audio_thread: Option<JoinHandle<()>>,
    protocol: u8,
    radio_ip: std::net::IpAddr,
    /// A clone of the session's own socket, kept ONLY so send_stop_command
    /// can send the Stop packet from the SAME local port the rest of the
    /// session used, instead of a brand new one. BUG FIX: this used to
    /// bind a fresh throwaway socket (`UdpSocket::bind(("0.0.0.0", 0))`)
    /// just for the Stop packet -- confirmed via a real side-by-side
    /// packet capture (fresh connect with diversity enabled: works
    /// indefinitely; in-app reconnect to enable/disable it: reliably
    /// hangs the radio after about a second, every time) that a
    /// reconnect sends from FOUR different source ports within about a
    /// second (the ending session's own data port, its own throwaway
    /// Stop port, the new session's data port, and -- if toggled back --
    /// another throwaway Stop port), while a fresh connect never sends a
    /// Stop at all and only ever uses one port. piHPSDR, by contrast,
    /// keeps one socket for its entire process lifetime and never
    /// recreates it across a diversity/PureSignal-style reconnect. This
    /// board's P1/Metis firmware is a plausible candidate for having a
    /// single-peer assumption that a Stop arriving from an unrelated
    /// port -- right as a brand new session is also mid-handshake from
    /// yet another port -- could genuinely confuse. Doesn't fully match
    /// piHPSDR's approach (this project still opens a new socket, hence
    /// a new port, for each reconnect's actual data session), but at
    /// least makes the Stop packet itself consistent with the session it
    /// belongs to, which is the cheapest testable step toward that.
    stop_socket: UdpSocket,
}

impl RadioSession {
    pub fn start(device: &Device, settings: RadioSettings) -> io::Result<Self> {
        let frequency_hz = Arc::new(AtomicU32::new(settings.frequency_hz));
        let sample_rate = Arc::new(AtomicU32::new(settings.sample_rate));
        let adc = Arc::new(AtomicU32::new(0));
        let antenna = Arc::new(AtomicU32::new(0));
        // See RadioSettings::rx_attenuation's doc comment -- main.rs
        // loads this from Config, falling back to RadioSettings::default's
        // own non-zero default rather than the old hardcoded 0dB, which
        // real-hardware testing confirmed causes front-end overload on
        // an ordinary HF antenna.
        let rx_attenuation = Arc::new(AtomicU32::new(settings.rx_attenuation));
        // See RadioSession::ps_tx_attenuation's doc comment.
        let ps_tx_attenuation = Arc::new(AtomicU32::new(settings.ps_tx_attenuation));
        let mox = Arc::new(AtomicBool::new(false));
        let tx_iq = Arc::new(Mutex::new(VecDeque::with_capacity(TX_IQ_BUFFER_CAPACITY)));
        let tci_tx_audio = Arc::new(Mutex::new(VecDeque::with_capacity(TCI_TX_AUDIO_CAPACITY)));
        let tci_tx_gain = Arc::new(Mutex::new(1.0f32));
        // Deliberately low rather than defaulting to max power -- easier
        // to notice "too low, turn it up" on a bench test than to start
        // a first-ever TX test at full drive into whatever's connected
        // to the antenna port.
        let tx_power_watts = Arc::new(AtomicU32::new(2));
        let pa_gain_db = Arc::new(AtomicU32::new(DEFAULT_PA_GAIN_DB.to_bits()));
        let tx_forward_power = Arc::new(AtomicU32::new(0));
        let tx_reverse_power = Arc::new(AtomicU32::new(0));
        let ps_rx_feedback_iq = Arc::new(Mutex::new(VecDeque::with_capacity(PS_FEEDBACK_BUFFER_CAPACITY)));
        let ps_tx_feedback_iq = Arc::new(Mutex::new(VecDeque::with_capacity(PS_FEEDBACK_BUFFER_CAPACITY)));
        let rx_audio_to_radio = Arc::new(Mutex::new(VecDeque::with_capacity(RX_AUDIO_TO_RADIO_CAPACITY)));
        let send_rx_audio_to_radio = Arc::new(AtomicBool::new(false));
        let radio_mic_audio = Arc::new(Mutex::new(VecDeque::with_capacity(RADIO_MIC_AUDIO_CAPACITY)));
        let tx_audio_source = Arc::new(AtomicU8::new(TX_AUDIO_SOURCE_AUTO));
        let tci_wants_mic = Arc::new(AtomicBool::new(false));
        let mic_ptt_enabled = Arc::new(AtomicBool::new(false));
        let mic_bias_enabled = Arc::new(AtomicBool::new(false));
        let mic_ptt_on_tip = Arc::new(AtomicBool::new(false));
        // See RadioSession::diversity_enabled/diversity_gain_db/
        // diversity_phase_deg/diversity_main_raw_iq's doc comments.
        // diversity_enabled is live (P1) -- shared with start_protocol1's
        // sender_loop/receiver_loop so they can react to
        // set_diversity_enabled without a reconnect.
        let diversity_enabled = Arc::new(AtomicBool::new(settings.diversity_enabled));
        let diversity_gain_db = Arc::new(AtomicU32::new(settings.diversity_gain_db.to_bits()));
        let diversity_phase_deg = Arc::new(AtomicU32::new(settings.diversity_phase_deg.to_bits()));
        let diversity_main_raw_iq = Arc::new(Mutex::new(VecDeque::with_capacity(IQ_BUFFER_CAPACITY)));
        let adc0_overload = Arc::new(AtomicBool::new(false));
        let adc1_overload = Arc::new(AtomicBool::new(false));
        let tx_fifo_underrun = Arc::new(AtomicBool::new(false));
        let tx_fifo_overrun = Arc::new(AtomicBool::new(false));
        let mut result = match device.protocol {
            1 => start_protocol1(
                device, settings, frequency_hz, sample_rate, adc, antenna, rx_attenuation,
                ps_tx_attenuation, mox, tx_iq, tci_tx_audio, tci_tx_gain, tx_power_watts, pa_gain_db,
                tx_forward_power, tx_reverse_power, adc0_overload, adc1_overload,
                tx_fifo_underrun, tx_fifo_overrun, ps_rx_feedback_iq, ps_tx_feedback_iq,
                rx_audio_to_radio, send_rx_audio_to_radio, radio_mic_audio, tx_audio_source,
                tci_wants_mic, mic_ptt_enabled, mic_bias_enabled, mic_ptt_on_tip,
                diversity_enabled, diversity_gain_db, diversity_phase_deg, diversity_main_raw_iq,
            ),
            2 => start_protocol2(
                device, settings, frequency_hz, sample_rate, adc, antenna, rx_attenuation,
                ps_tx_attenuation, mox, tx_iq, tci_tx_audio, tci_tx_gain, tx_power_watts, pa_gain_db,
                tx_forward_power, tx_reverse_power, adc0_overload, adc1_overload,
                tx_fifo_underrun, tx_fifo_overrun, ps_rx_feedback_iq, ps_tx_feedback_iq,
                rx_audio_to_radio, send_rx_audio_to_radio, radio_mic_audio, tx_audio_source,
                tci_wants_mic, mic_ptt_enabled, mic_bias_enabled, mic_ptt_on_tip,
                diversity_enabled, diversity_gain_db, diversity_phase_deg, diversity_main_raw_iq,
            ),
            p => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unknown protocol {p}"),
            )),
        };
        // Diversity combiner -- spawned here (not inside start_protocol1/2)
        // since it's protocol-agnostic: it only touches queues, never the
        // wire format. See RadioSession::diversity_main_raw_iq's doc
        // comment for what it does. Needs `session.iq_buffers[1]` to
        // exist, which real_receivers' `.max(2)` (inside start_protocol1/
        // 2, see their own diversity_enabled handling) guarantees whenever
        // diversity_enabled is true.
        if let Ok(session) = &mut result {
            if session.diversity_enabled.load(Ordering::Relaxed) {
                session.spawn_diversity_combiner_now();
            }
        }
        result
    }

    /// Keys or unkeys the transmitter. See RadioSession::mox's doc
    /// comment for who reads this.
    ///
    /// SAFETY: this is the one call in this whole project that can
    /// cause actual RF to leave the radio. Callers (UI PTT control,
    /// rigctl/TCI PTT commands) are responsible for only calling this
    /// with `true` when the operator actually intends to transmit --
    /// this method itself does no license/band/power-limit checking
    /// whatsoever.
    pub fn set_mox(&self, on: bool) {
        self.mox.store(on, Ordering::Relaxed);
    }

    pub fn mox_active(&self) -> bool {
        self.mox.load(Ordering::Relaxed)
    }

    /// Retunes the running receiver. Takes effect on the next packet the
    /// sender thread sends (up to one pacing interval away -- effectively
    /// immediate for P1, up to 250ms for P2's keep-alive cadence).
    pub fn set_frequency(&self, hz: u32) {
        self.frequency_hz.store(hz, Ordering::Relaxed);
    }

    /// Changes the live radio-side sample rate (P1: shared across all
    /// receivers; P2: this receiver's DDC only). Same timing as
    /// set_frequency. NOTE: this alone does not update WDSP's demod
    /// chain, which has its input rate fixed at channel-creation time --
    /// callers must recreate SpectrumHandle/AudioOutput after calling
    /// this for the whole pipeline to stay consistent.
    pub fn set_sample_rate(&self, hz: u32) {
        self.sample_rate.store(hz, Ordering::Relaxed);
    }

    /// Total buffered samples across all receivers -- handy for a simple
    /// "is data flowing" indicator in the UI before real DSP consumes this.
    pub fn total_buffered_samples(&self) -> usize {
        self.iq_buffers.iter().map(|b| b.lock().unwrap().len()).sum()
    }

    /// Activates the next configured-but-inactive receiver (P2 only --
    /// for P1 this always returns None, since extra_frequencies_hz is
    /// always empty there). Returns the new receiver's overall index
    /// (1-based, since 0 is the original receiver) if one was available.
    pub fn add_receiver(&self) -> Option<usize> {
        let current = self.active_receiver_count.load(Ordering::Relaxed) as usize;
        if current >= self.iq_buffers.len() {
            return None; // no more configured slots
        }
        self.active_receiver_count.store((current + 1) as u32, Ordering::Relaxed);
        Some(current)
    }

    /// Spawns the diversity combiner now, creating a fresh dedicated stop
    /// flag for it -- see spawn_diversity_combiner's and
    /// diversity_combiner_stop's doc comments.
    fn spawn_diversity_combiner_now(&mut self) {
        let stop = Arc::new(AtomicBool::new(false));
        let handle = spawn_diversity_combiner(self, Arc::clone(&stop));
        self.diversity_combiner_thread = Some(handle);
        self.diversity_combiner_stop = Some(stop);
    }

    /// Stops and joins the diversity combiner if one is running -- no-op
    /// otherwise. See diversity_combiner_stop's doc comment for why this
    /// is a SEPARATE flag from the whole session's stop_flag.
    fn stop_diversity_combiner_now(&mut self) {
        if let Some(stop) = self.diversity_combiner_stop.take() {
            stop.store(true, Ordering::SeqCst);
        }
        if let Some(t) = self.diversity_combiner_thread.take() {
            let _ = t.join();
        }
    }

    /// Live toggle for Diversity -- both protocols, see
    /// RadioSession::diversity_enabled's doc comment for how P1 and P2
    /// each pick this up. Updates the live flag sender_loop/receiver_loop
    /// (or p2_sender_loop/p2_receiver_loop) watch, reserves/releases wire
    /// 1 via active_receiver_count, and spawns/stops the combiner thread
    /// to match -- does NOT touch the socket/wire protocol directly,
    /// that's each protocol's own sender loop's job (P1's replays the
    /// preconfig+Start burst on noticing the change; P2's needs nothing
    /// beyond its own already-live per-cycle read).
    pub fn set_diversity_enabled(&mut self, enabled: bool) {
        self.diversity_enabled.store(enabled, Ordering::SeqCst);
        if enabled {
            // Reserve wire 1 immediately (don't wait for the next UI
            // frame's own floor logic, see main.rs's diversity_floor) so
            // sender_loop's very next packet already reflects it.
            if self.active_receiver_count.load(Ordering::Relaxed) < 2 {
                self.active_receiver_count.store(2, Ordering::Relaxed);
            }
            if self.diversity_combiner_thread.is_none() {
                self.spawn_diversity_combiner_now();
            }
        } else {
            self.stop_diversity_combiner_now();
            // active_receiver_count's floor back down to 1 (when no
            // extra receivers are active) is handled every frame by
            // main.rs's own existing diversity_floor logic -- not
            // duplicated here.
        }
    }

    pub fn stop(&mut self) {
        // Unkey first, before anything else -- a session ending (app
        // closing, "Stop" clicked, sample rate change tearing this
        // down for a rebuild) must never leave the transmitter keyed.
        self.set_mox(false);
        self.stop_flag.store(true, Ordering::SeqCst);
        // Join the sender first so it's no longer sending "keep running"
        // traffic, then tell the radio to actually stop. P1 has no
        // watchdog at all -- without an explicit stop it can stay wedged
        // in a running state until power-cycled. P2 does have a watchdog
        // but it can take up to ~1s; better to stop it immediately.
        if let Some(t) = self.sender_thread.take() {
            let _ = t.join();
        }
        if let Some(t) = self.tx_iq_thread.take() {
            let _ = t.join();
        }
        if let Some(t) = self.rx_audio_thread.take() {
            let _ = t.join();
        }
        self.send_stop_command();
        if let Some(t) = self.receiver_thread.take() {
            let _ = t.join();
        }
        self.stop_diversity_combiner_now();
    }

    fn send_stop_command(&self) {
        // See stop_socket's own doc comment -- sent from the SAME local
        // port the rest of this session used, not a fresh throwaway one.
        match self.protocol {
            1 => {
                // Same Start packet shape, Command 0x00 = stop.
                // Size confirmed against the reference (metis_stop) --
                // corrects an earlier 63-byte guess to the actual 64.
                let mut pkt = [0u8; 64];
                pkt[0] = 0xEF;
                pkt[1] = 0xFE;
                pkt[2] = 0x04;
                pkt[3] = 0x00;
                let _ = self.stop_socket.send_to(&pkt, (self.radio_ip, DATA_PORT));
            }
            2 => {
                // High Priority packet with the run bit cleared.
                let pkt = [0u8; P2_PACKET_SIZE]; // seq=0, byte4=0 (run=0) is fine for a one-shot goodbye
                let _ = self.stop_socket.send_to(&pkt, (self.radio_ip, P2_HIGH_PRIORITY_PORT));
            }
            _ => {}
        }
    }
}

impl Drop for RadioSession {
    fn drop(&mut self) {
        self.stop();
    }
}

fn start_protocol1(
    device: &Device,
    settings: RadioSettings,
    frequency_hz: Arc<AtomicU32>,
    sample_rate: Arc<AtomicU32>,
    adc: Arc<AtomicU32>,
    antenna: Arc<AtomicU32>,
    rx_attenuation: Arc<AtomicU32>,
    ps_tx_attenuation: Arc<AtomicU32>,
    mox: Arc<AtomicBool>,
    tx_iq: Arc<Mutex<VecDeque<f32>>>,
    tci_tx_audio: Arc<Mutex<VecDeque<f32>>>,
    tci_tx_gain: Arc<Mutex<f32>>,
    tx_power_watts: Arc<AtomicU32>,
    pa_gain_db: Arc<AtomicU32>,
    tx_forward_power: Arc<AtomicU32>,
    tx_reverse_power: Arc<AtomicU32>,
    adc0_overload: Arc<AtomicBool>,
    adc1_overload: Arc<AtomicBool>,
    tx_fifo_underrun: Arc<AtomicBool>,
    tx_fifo_overrun: Arc<AtomicBool>,
    ps_rx_feedback_iq: Arc<Mutex<VecDeque<IqSample>>>,
    ps_tx_feedback_iq: Arc<Mutex<VecDeque<IqSample>>>,
    rx_audio_to_radio: Arc<Mutex<VecDeque<f32>>>,
    send_rx_audio_to_radio: Arc<AtomicBool>,
    radio_mic_audio: Arc<Mutex<VecDeque<f32>>>,
    tx_audio_source: Arc<AtomicU8>,
    tci_wants_mic: Arc<AtomicBool>,
    mic_ptt_enabled: Arc<AtomicBool>,
    mic_bias_enabled: Arc<AtomicBool>,
    mic_ptt_on_tip: Arc<AtomicBool>,
    diversity_enabled: Arc<AtomicBool>,
    diversity_gain_db: Arc<AtomicU32>,
    diversity_phase_deg: Arc<AtomicU32>,
    diversity_main_raw_iq: Arc<Mutex<VecDeque<IqSample>>>,
) -> io::Result<RadioSession> {
    let socket = UdpSocket::bind(("0.0.0.0", 0))?;
    socket.set_read_timeout(Some(Duration::from_millis(500)))?;
    // device.address is the radio's IP, captured from its discovery reply;
    // confirmed streaming traffic stays on the same port 1024.
    let target = std::net::SocketAddr::new(device.address.ip(), DATA_PORT);
    socket.connect(target)?;
    // See RadioSession::stop_socket's doc comment -- cloned now, before
    // `socket` itself gets consumed by the sender/receiver threads below,
    // so send_stop_command can later send from this SAME local port.
    let stop_socket = socket.try_clone()?;

    // PureSignal (P1): see ps_feedback_config's doc comment. `ps_config`
    // is None whenever PS wasn't requested, or this board has no known
    // PS support -- everything below falls back to today's exact
    // behavior in that case. `ps_wire_total` is the FIXED total
    // receiver count the radio must be told about for the feedback
    // indices to line up (independent of active_receiver_count/Add
    // Receiver, which only tracks how many of the REAL slots the UI
    // has turned on); `real_receivers` is the cap on those real slots.
    //
    // NOT forced to a minimum of 1 real receiver -- on Metis/HermesLite
    // (max_real=0), real receiver indices would otherwise collide with
    // the feedback indices themselves (rx_feedback_idx=0 there), a
    // genuine wire-level correctness bug, not just a UI nicety. This
    // does mean `iq_buffers` can legitimately end up empty on those
    // smallest boards while PS is active -- main.rs's connect flow
    // (which assumes iq_buffers[0] always exists for the main
    // SpectrumHandle) doesn't handle that yet; not a concern for the
    // 2-ADC Angelia/Orion/Orion2-class hardware this was built against,
    // but flagged rather than silently papered over for anyone with
    // one of the smaller boards.
    let ps_config = if settings.puresignal_enabled {
        ps_feedback_config(1, device.board)
    } else {
        None
    };
    let mut real_receivers = match ps_config {
        Some((_, _, Some(max_real))) => settings.receivers.max(1).min(max_real),
        Some((_, _, None)) | None => settings.receivers.max(1),
    };
    // Diversity: forces at least 2 wire slots (wire 0 = ADC0/main, wire 1
    // = the reserved ADC1 aux feed) -- see RadioSession::diversity_enabled's
    // doc comment. PS and diversity are mutually exclusive (enforced in
    // main.rs's Settings UI), so no interaction with the ps_config branch
    // above to worry about here.
    if settings.diversity_enabled {
        real_receivers = real_receivers.max(2);
    }
    let ps_wire_total: Option<u8> = ps_config.map(|(_, tx_idx, _)| tx_idx + 1);
    let ps_feedback_indices: Option<(u8, u8)> = ps_config.map(|(rx_idx, tx_idx, _)| (rx_idx, tx_idx));
    // Initial value active_receiver_count starts at (see its own
    // creation just below) -- the count actually being streamed from
    // the very first packet, as opposed to `real_receivers` (this
    // board's full reported capacity, used only for buffer sizing).
    let initial_active_receivers: u32 = if settings.diversity_enabled { 2 } else { 1 };

    // Confirmed against a working reference (rustyHPSDR): before
    // sending the actual start command, the client sends TWO COMPLETE
    // rotations of all 11 C&C registers (RX/TX frequency, receiver
    // count/antenna, drive, attenuation, the fixed-value registers,
    // etc.), THEN the Start command. See p1_send_preconfig_and_start's
    // doc comment -- this is also what's replayed on a detected frame
    // desync (receiver_loop's sync check), not just here at initial
    // connect.
    //
    // BUG FIX (diversity): this used to pass `real_receivers` (this
    // board's full reported capacity -- 7 on the user's Orion2, even
    // though only 2 are ever actually streamed for diversity), NOT the
    // count the ongoing stream actually uses. Confirmed via a real
    // packet capture: preconfig's C4 byte declared "7 receivers, RX
    // sync bit set" while the ongoing stream immediately afterward
    // declared "2 receivers, RX sync bit set" -- baseline (non-
    // diversity) has an analogous 7-vs-1 mismatch that's harmless, but
    // COMBINED with the sync bit (which asks the radio to pair up its
    // DDCs), a declared-7-then-actually-2 transition very plausibly
    // leaves the FPGA's internal dual-DDC pairing setup inconsistent --
    // a real candidate for the radio going silent about a second into
    // 2-receiver diversity streaming regardless of sample rate. Now
    // matches `active_receiver_count`'s own initial value, so preconfig
    // and the ongoing stream agree on the receiver count from the very
    // first packet, not just the sync bit.
    p1_send_preconfig_and_start(
        &socket,
        ps_wire_total,
        initial_active_receivers as u8,
        settings.frequency_hz,
        settings.sample_rate,
        matches!(device.board, Boards::HermesLite | Boards::HermesLite2),
        rx_attenuation.load(Ordering::Relaxed) as u8,
        ps_tx_attenuation.load(Ordering::Relaxed) as u8,
        device.adcs,
        settings.diversity_enabled,
        &tx_iq,
        mic_ptt_enabled.load(Ordering::Relaxed),
        mic_bias_enabled.load(Ordering::Relaxed),
        mic_ptt_on_tip.load(Ordering::Relaxed),
    )?;

    let stop_flag = Arc::new(AtomicBool::new(false));
    let iq_buffers: Vec<Arc<Mutex<VecDeque<IqSample>>>> = (0..real_receivers)
        .map(|_| Arc::new(Mutex::new(VecDeque::with_capacity(IQ_BUFFER_CAPACITY))))
        .collect();

    // Extra receivers beyond the first, pre-sized to whatever was
    // requested (settings.receivers, from the board's reported
    // capability -- e.g. a HermesLite2 reporting 4 via its discovery
    // reply's buf19 byte) -- capped to `real_receivers` when PureSignal
    // reserves some of that capability for feedback instead (see
    // ps_feedback_config). None are active until add_receiver() is
    // called -- active_receiver_count starts at 1. Same pattern as
    // start_protocol2's own init just below in this file. Sample rate
    // is still tracked per-extra-receiver for struct-shape parity with
    // P2, but P1 has only one real shared clock (see sample_rate_code
    // in the general-control frame, no per-receiver override slot) --
    // main.rs keeps every extra receiver's rate in sync with the
    // primary's rather than exposing it as independently adjustable.
    let extra_count = real_receivers.saturating_sub(1) as usize;
    let extra_frequencies_hz: Vec<Arc<AtomicU32>> =
        (0..extra_count).map(|_| Arc::new(AtomicU32::new(settings.frequency_hz))).collect();
    let extra_sample_rates_hz: Vec<Arc<AtomicU32>> =
        (0..extra_count).map(|_| Arc::new(AtomicU32::new(settings.sample_rate))).collect();
    let extra_adcs: Vec<Arc<AtomicU32>> = (0..extra_count).map(|_| Arc::new(AtomicU32::new(0))).collect();
    // Diversity reserves wire 1 out of "Add Receiver"'s reach the same
    // way receiver 0 itself is pre-consumed -- see RadioSession::
    // diversity_enabled's doc comment. Same value preconfig just used
    // (initial_active_receivers), so both phases agree.
    let active_receiver_count = Arc::new(AtomicU32::new(initial_active_receivers));

    let sender_socket = socket.try_clone()?;
    let sender_stop = Arc::clone(&stop_flag);
    let sender_frequency = Arc::clone(&frequency_hz);
    let sender_sample_rate = Arc::clone(&sample_rate);
    let sender_mox = Arc::clone(&mox);
    let sender_tx_iq = Arc::clone(&tx_iq);
    let sender_antenna = Arc::clone(&antenna);
    let sender_active_receiver_count = Arc::clone(&active_receiver_count);
    let sender_extra_frequencies_hz = extra_frequencies_hz.clone();
    let sender_tx_power_watts = Arc::clone(&tx_power_watts);
    let sender_pa_gain_db = Arc::clone(&pa_gain_db);
    let sender_rx_attenuation = Arc::clone(&rx_attenuation);
    let sender_ps_tx_attenuation = Arc::clone(&ps_tx_attenuation);
    let sender_is_hermes_lite = matches!(device.board, Boards::HermesLite | Boards::HermesLite2);
    let sender_num_adcs = device.adcs;
    let sender_rx_audio_to_radio = Arc::clone(&rx_audio_to_radio);
    let sender_send_rx_audio_to_radio = Arc::clone(&send_rx_audio_to_radio);
    let sender_mic_ptt_enabled = Arc::clone(&mic_ptt_enabled);
    let sender_mic_bias_enabled = Arc::clone(&mic_bias_enabled);
    let sender_mic_ptt_on_tip = Arc::clone(&mic_ptt_on_tip);
    let sender_adc = Arc::clone(&adc);
    let sender_extra_adcs = extra_adcs.clone();
    // Live -- see RadioSession::diversity_enabled's doc comment.
    // sender_loop watches this itself for changes.
    let sender_diversity_enabled = Arc::clone(&diversity_enabled);
    let sender_thread = thread::spawn(move || {
        sender_loop(
            sender_socket,
            sender_frequency,
            sender_sample_rate,
            sender_mox,
            sender_tx_iq,
            sender_active_receiver_count,
            sender_extra_frequencies_hz,
            sender_antenna,
            sender_tx_power_watts,
            sender_pa_gain_db,
            sender_rx_attenuation,
            sender_ps_tx_attenuation,
            sender_is_hermes_lite,
            sender_num_adcs,
            sender_adc,
            sender_extra_adcs,
            sender_diversity_enabled,
            ps_wire_total,
            sender_rx_audio_to_radio,
            sender_send_rx_audio_to_radio,
            sender_mic_ptt_enabled,
            sender_mic_bias_enabled,
            sender_mic_ptt_on_tip,
            sender_stop,
        );
    });

    let receiver_socket = socket.try_clone()?;
    let receiver_stop = Arc::clone(&stop_flag);
    let receiver_buffers = iq_buffers.clone();
    let receiver_sample_rate = Arc::clone(&sample_rate);
    let receiver_active_receiver_count = Arc::clone(&active_receiver_count);
    let receiver_tx_forward_power = Arc::clone(&tx_forward_power);
    let receiver_tx_reverse_power = Arc::clone(&tx_reverse_power);
    let receiver_adc0_overload = Arc::clone(&adc0_overload);
    let receiver_adc1_overload = Arc::clone(&adc1_overload);
    let receiver_ps_rx_feedback_iq = Arc::clone(&ps_rx_feedback_iq);
    let receiver_ps_tx_feedback_iq = Arc::clone(&ps_tx_feedback_iq);
    let receiver_radio_mic_audio = Arc::clone(&radio_mic_audio);
    // Live -- see RadioSession::diversity_enabled's doc comment.
    let receiver_diversity_enabled = Arc::clone(&diversity_enabled);
    let receiver_diversity_main_raw_iq = Arc::clone(&diversity_main_raw_iq);
    let receiver_thread = thread::spawn(move || {
        receiver_loop(
            receiver_socket,
            receiver_buffers,
            receiver_active_receiver_count,
            receiver_sample_rate,
            receiver_tx_forward_power,
            receiver_tx_reverse_power,
            receiver_adc0_overload,
            receiver_adc1_overload,
            ps_wire_total,
            ps_feedback_indices,
            receiver_ps_rx_feedback_iq,
            receiver_ps_tx_feedback_iq,
            receiver_radio_mic_audio,
            receiver_diversity_enabled,
            receiver_diversity_main_raw_iq,
            receiver_stop,
        );
    });

    Ok(RadioSession {
        iq_buffers,
        frequency_hz,
        sample_rate,
        adc,
        antenna,
        rx_attenuation,
        ps_tx_attenuation,
        extra_frequencies_hz,
        extra_sample_rates_hz,
        extra_adcs,
        active_receiver_count,
        ps_rx_feedback_iq,
        ps_tx_feedback_iq,
        mox,
        tx_iq,
        tci_tx_audio,
        tci_tx_gain,
        rx_audio_to_radio,
        send_rx_audio_to_radio,
        radio_mic_audio,
        tx_audio_source,
        tci_wants_mic,
        mic_ptt_enabled,
        mic_bias_enabled,
        mic_ptt_on_tip,
        diversity_enabled,
        diversity_gain_db,
        diversity_phase_deg,
        diversity_main_raw_iq,
        tx_power_watts,
        pa_gain_db,
        tx_forward_power,
        tx_reverse_power,
        adc0_overload,
        adc1_overload,
        tx_fifo_underrun,
        tx_fifo_overrun,
        stop_flag,
        sender_thread: Some(sender_thread),
        receiver_thread: Some(receiver_thread),
        tx_iq_thread: None, // P1 streams TX audio through sender_thread itself
        rx_audio_thread: None, // P1 streams RX audio through sender_thread itself
        diversity_combiner_thread: None, // set by RadioSession::start right after this returns, if diversity_enabled
        diversity_combiner_stop: None,
        protocol: 1,
        radio_ip: device.address.ip(),
        stop_socket,
    })
}

/// Builds one 512-byte USB frame: 3 sync bytes, 5 C&C bytes, rest
/// zeroed unless overwritten afterward (see fill_tx_payload -- the
/// tail carries TX audio/IQ while keyed, silence/zero otherwise).
fn build_usb_frame(c0: u8, c1: u8, c2: u8, c3: u8, c4: u8) -> [u8; USB_FRAME_SIZE] {
    let mut frame = [0u8; USB_FRAME_SIZE];
    frame[0] = 0x7F;
    frame[1] = 0x7F;
    frame[2] = 0x7F;
    frame[3] = c0;
    frame[4] = c1;
    frame[5] = c2;
    frame[6] = c3;
    frame[7] = c4;
    frame
}

/// Inverse of sign_extend_24: packs a [-1.0, 1.0] sample into 3
/// big-endian bytes, same 2^23-1 scale spectrum.rs's IQ_NORM uses on
/// the RX side. Rounding (round-half-away-from-zero rather than
/// truncation toward zero) confirmed against a working reference
/// (rustyHPSDR).
fn pack_24(v: f32) -> [u8; 3] {
    let scaled = (v.clamp(-1.0, 1.0) * 8_388_607.0) as f64;
    let rounded = if scaled >= 0.0 { (scaled + 0.5).floor() } else { (scaled - 0.5).ceil() };
    let b = (rounded as i32).to_be_bytes();
    [b[1], b[2], b[3]]
}

/// Fills `frame`'s payload (everything after the 8-byte sync+C&C
/// header) with interleaved I/Q pulled from `tx_iq`, padding with
/// silence if the buffer underruns so frame timing/size stays exact
/// regardless of how much real TX audio is available yet.
///
/// UNVERIFIED, and the least-confident part of the whole TX path: it's
/// not confirmed whether Protocol 1's outgoing C&C frames actually
/// carry interleaved I/Q here (mirroring how parse_iq_packet reads the
/// *incoming* RX frames) or instead expect raw audio samples for the
/// radio's own hardware to modulate -- see tx.rs's module note on why
/// this project's TxProcessor produces IQ rather than audio. If TX
/// sounds garbled or silent on a Protocol 1 radio, checking which of
/// those two this radio actually wants is the first thing to try.
/// Confirmed against a working reference (rustyHPSDR): this is NOT
/// simply a continuous stream of packed I/Q like RX's own payload is.
/// Each unit is 4 bytes of "dummy RX audio" (always zero while
/// actually transmitting -- the reference's own naming, not guessed)
/// followed by one 16-bit I/Q pair (NOT 24-bit -- TX uses a narrower
/// sample width than RX does), big-endian, scaled by 32767 (signed
/// 16-bit max). An earlier version of this function packed 24-bit I/Q
/// with no padding at all between samples -- structurally wrong on
/// every count (wrong width, wrong interleaving, missing the padding
/// bytes entirely), which a radio's firmware would have no way to
/// decode as valid TX audio.
fn fill_tx_payload(frame: &mut [u8; USB_FRAME_SIZE], tx_iq: &Mutex<VecDeque<f32>>) {
    let mut buf = tx_iq.lock().unwrap();
    let mut b = HEADER_SIZE;
    while b + 8 <= USB_FRAME_SIZE {
        frame[b] = 0;
        frame[b + 1] = 0;
        frame[b + 2] = 0;
        frame[b + 3] = 0;
        let i = buf.pop_front().unwrap_or(0.0);
        let q = buf.pop_front().unwrap_or(0.0);
        let i_sample = (i.clamp(-1.0, 1.0) * 32767.0) as i16;
        let q_sample = (q.clamp(-1.0, 1.0) * 32767.0) as i16;
        frame[b + 4] = (i_sample >> 8) as u8;
        frame[b + 5] = i_sample as u8;
        frame[b + 6] = (q_sample >> 8) as u8;
        frame[b + 7] = q_sample as u8;
        b += 8;
    }
}

/// Zero-order-hold pacing state for fill_rx_audio_payload -- see that
/// function's doc comment for why this is needed at all: the 8-byte
/// slot cadence sender_loop sends at tracks the RX ADC sample rate (and
/// receiver count), NOT the fixed 48kHz spectrum.rs's demod audio is
/// actually produced at. Popping a "new" queued sample on every single
/// slot regardless of that mismatch drains rx_audio_to_radio far faster
/// than it's filled whenever the slot rate exceeds 48kHz (the common
/// case -- anything other than exactly 48kHz/1 receiver), so most slots
/// fall back to silence -- confirmed as the cause of a real report of
/// scratchy/staticky audio at the radio's own local output, while the
/// local PC speaker path (fed and drained at the same true 48kHz rate
/// throughout) sounded fine. Owned by sender_loop for the whole
/// session so the pacing ratio can change live (sample rate change,
/// Add Receiver) without more than the one glitch the change itself
/// would cause anyway.
struct RxAudioPacer {
    accum: f64,
    held: i16,
}

impl RxAudioPacer {
    fn new() -> Self {
        Self { accum: 0.0, held: 0 }
    }
}

/// Fills the same 8-byte-per-sample slot fill_tx_payload uses (see its
/// doc comment for the reference's own naming: "dummy RX audio" while
/// transmitting), but for the receive side -- confirmed against
/// piHPSDR's old_protocol.c (old_protocol_audio_samples): mono
/// demodulated audio (L=R) in the first 4 bytes, big-endian 16-bit,
/// scaled by 32767, with the trailing 4 IQ bytes left zero (no IQ goes
/// out in this slot while not transmitting). Only ever called while
/// !mox_on -- see p1_build_packet's call site.
///
/// `slots_per_sample` (>= 1.0, see RxAudioPacer's doc comment): a new
/// sample is only actually popped from the queue once per this many
/// slots; slots in between repeat `pacer.held` (zero-order hold) rather
/// than draining the queue empty and falling back to silence.
fn fill_rx_audio_payload(
    frame: &mut [u8; USB_FRAME_SIZE],
    rx_audio: &Mutex<VecDeque<f32>>,
    pacer: &mut RxAudioPacer,
    slots_per_sample: f64,
) {
    let mut buf = rx_audio.lock().unwrap();
    let mut b = HEADER_SIZE;
    while b + 8 <= USB_FRAME_SIZE {
        pacer.accum += 1.0;
        if pacer.accum >= slots_per_sample {
            pacer.accum -= slots_per_sample;
            let sample = buf.pop_front().unwrap_or(0.0);
            pacer.held = (sample.clamp(-1.0, 1.0) * 32767.0) as i16;
        }
        let s = pacer.held;
        frame[b] = (s >> 8) as u8;
        frame[b + 1] = s as u8;
        frame[b + 2] = (s >> 8) as u8;
        frame[b + 3] = s as u8;
        frame[b + 4] = 0;
        frame[b + 5] = 0;
        frame[b + 6] = 0;
        frame[b + 7] = 0;
        b += 8;
    }
}

/// Reference's own default pa_calibration-table gain, in dB, before
/// any user calibration is applied. Used both as drive_byte_for_watts's
/// fallback and as the UI's slider default (see main.rs's PA Calibration
/// settings) so an unset/never-calibrated band behaves identically to
/// this uncalibrated reference starting point.
pub const DEFAULT_PA_GAIN_DB: f32 = 38.8;

/// Converts a desired TX output power (watts) into a protocol drive
/// byte (0-255), via a dBm/DAC-voltage calibration curve using the
/// given per-band PA gain. Originally P1-only: confirmed against a
/// working reference (rustyHPSDR) that P1's command address=3 drive
/// byte is NOT a simple linear 0-255 scale (sending a raw slider value
/// directly, as an earlier version of this file did, doesn't
/// correspond to anything meaningful for P1 -- plausibly why "0 watts"
/// showed regardless of the slider value). P2's High Priority packet
/// byte 345 *is* confirmed linear 0-255 by the official protocol spec
/// at the wire level, but that says nothing about how a real PA
/// actually responds to it -- so P2 now uses this same conversion too,
/// purely host-side (nothing in the P2 protocol itself requires it).
///
/// `gain_db` is the current band's PA gain (main.rs resolves this from
/// its per-band PA Calibration sliders, falling back to
/// DEFAULT_PA_GAIN_DB for any band the user hasn't calibrated) --
/// real per-band calibration varies with the actual amplifier's
/// response per band, which no fixed constant here can capture.
fn drive_byte_for_watts(watts: f32, gain_db: f32) -> u8 {
    let watts = watts.max(0.01); // avoid log10(0)/log10(negative)
    let target_dbm = 10.0 * (watts * 1000.0).log10() - gain_db;
    let target_volts = (10.0_f32.powf(target_dbm * 0.1) * 0.05).sqrt();
    let volts = (target_volts / 0.8).min(1.0);
    let actual_volts = (volts / 0.98).clamp(0.0, 1.0);
    (actual_volts * 255.0) as u8
}

fn sample_rate_code(rate: u32) -> u8 {
    match rate {
        48_000 => 0x00,
        96_000 => 0x01,
        192_000 => 0x02,
        384_000 => 0x03,
        _ => 0x00,
    }
}

/// Sends the confirmed-reference pre-config sequence (two full
/// rotations of all 11 C&C registers over raw, unacknowledged UDP)
/// followed by the Start command, at initial connection (start_protocol1).
///
/// NOTE: an earlier version of this project also called this from
/// sender_loop to recover from a detected frame desync via a full
/// stop+restart, on the theory (borrowed from rustyHPSDR) that a lost
/// USB-frame sync couldn't be recovered any other way. Real hardware
/// testing disproved that: the actual cause was a fixed, connection-
/// wide byte-phase offset (not per-frame corruption), which restarting
/// just reproduced identically every time (an infinite restart loop).
/// The real fix is in parse_iq_stream (receiver_loop) -- discover the
/// phase once via a byte scan, then track it for the rest of the
/// connection -- so no restart-on-desync path exists here anymore.
#[allow(clippy::too_many_arguments)]
fn p1_send_preconfig_and_start(
    socket: &UdpSocket,
    ps_wire_total: Option<u8>,
    // The receiver count actually being streamed from the very first
    // packet -- start_protocol1's `initial_active_receivers` (the value
    // `active_receiver_count` itself starts at), NOT `real_receivers`
    // (this board's full reported capacity, used only for buffer
    // sizing). See that call site's own BUG FIX comment: passing
    // `real_receivers` here previously left preconfig declaring a
    // receiver count the ongoing stream never actually used (e.g. 7 on
    // a board that only ever streams 2 for diversity), confirmed via a
    // real packet capture to coincide with the radio going silent about
    // a second into 2-receiver diversity streaming.
    receivers_fallback: u8,
    frequency_hz: u32,
    sample_rate: u32,
    is_hermes_lite: bool,
    rx_attenuation: u8,
    ps_tx_attenuation: u8,
    num_adcs: u8,
    // BUG FIX: this used to be hardcoded `false` in this function's own
    // call to p1_build_packet below, meaning the diversity-only C4 sync
    // bit (0x80) and C1 ADC1-forcing bit never went out during the
    // critical two-rotations-plus-Start handshake -- only afterward,
    // once the ongoing sender_loop took over post-Start. Now threaded
    // through so preconfig and the ongoing stream present an identical,
    // consistent diversity state from the very first packet.
    diversity_enabled: bool,
    tx_iq: &Mutex<VecDeque<f32>>,
    mic_ptt_enabled: bool,
    mic_bias_enabled: bool,
    mic_ptt_on_tip: bool,
) -> io::Result<()> {
    let mut pre_seq: u32 = 0;
    let mut pre_ozy_command: u8 = 1;
    let mut pre_current_receiver: u8 = 0;
    let mut rotations = 0;
    let mut dummy_rx_audio_pacer = RxAudioPacer::new(); // send_rx_audio is false below -- never actually read
    let dummy_adc = Arc::new(AtomicU32::new(0)); // ADC0 -- nothing keyed yet this early, see extra_frequencies_hz's identical reasoning below
    while rotations < PRE_CONFIG_ROTATIONS {
        let packet = p1_build_packet(
            pre_seq,
            &mut pre_ozy_command,
            &mut pre_current_receiver,
            ps_wire_total.unwrap_or(receivers_fallback),
            frequency_hz,
            0, // antenna: ANT1 default: nothing to key yet, live antenna updates once running
            0, // tx_power_watts: not transmitting during startup config
            DEFAULT_PA_GAIN_DB, // irrelevant while not transmitting (drive forced to 0 above)
            sample_rate,
            false, // mox: never keyed during startup config
            is_hermes_lite,
            rx_attenuation,
            ps_tx_attenuation,
            num_adcs,
            &[], // no extra receivers active yet this early -- falls back to the main frequency
            &dummy_adc,
            &[], // no extra receivers' ADCs active yet this early either -- diversity_enabled below still forces wire 1 to ADC1 regardless
            diversity_enabled,
            tx_iq,
            ps_wire_total.is_some(),
            tx_iq, // send_rx_audio is false below, so this is never actually read -- reusing tx_iq's Mutex just to satisfy the type, not a real audio source
            false, // send_rx_audio: never during startup config, nothing keyed yet
            &mut dummy_rx_audio_pacer,
            1.0,
            mic_ptt_enabled,
            mic_bias_enabled,
            mic_ptt_on_tip,
        );
        socket.send(&packet)?;
        pre_seq = pre_seq.wrapping_add(1);
        if pre_ozy_command == 1 && pre_current_receiver == 0 {
            rotations += 1;
        }
    }

    // BUG FIX: confirmed against piHPSDR's metis_restart() (the
    // function behind BOTH a fresh connect and resuming after a stop --
    // old_protocol_run() calls it directly): `usleep(250000);` right
    // here, between the preconfig rotations and the actual Start
    // packet, with the reference's own comment explaining why ("some
    // apps have very small buffers that over-run if too much data is
    // sent... before sending a METIS start packet"). This project never
    // had this delay at all. A fresh connect's own natural human-paced
    // discovery-screen delay likely papered over its absence there, but
    // an in-app reconnect (PureSignal/Diversity's "Enable" checkbox)
    // fires the new Start moments after the OLD session's Stop with
    // nothing to fill that gap.
    thread::sleep(Duration::from_millis(250));

    // Start command: <0xEF><0xFE><0x04><Command><60 zero bytes>.
    // Command byte and packet size both confirmed against the
    // reference (metis_start) -- corrects two previously-wrong
    // guesses: this was 0x01 in a 63-byte buffer; the reference uses
    // 0x03 in a 64-byte buffer.
    let mut start_pkt = [0u8; 64];
    start_pkt[0] = 0xEF;
    start_pkt[1] = 0xFE;
    start_pkt[2] = 0x04;
    start_pkt[3] = 0x03;
    socket.send(&start_pkt)?;
    Ok(())
}

/// Builds one full P1 packet (general-control frame + whichever C&C
/// register is currently up in the rotation), advancing `ozy_command`
/// and `current_receiver` exactly as the confirmed reference does.
/// Shared between the pre-start "send two full rotations" sequence in
/// start_protocol1 and the ongoing sender_loop, so both send identical
/// packet content rather than two slightly-different implementations
/// drifting apart over time.
#[allow(clippy::too_many_arguments)]
fn p1_build_packet(
    seq: u32,
    ozy_command: &mut u8,
    current_receiver: &mut u8,
    receivers: u8,
    frequency_hz: u32,
    antenna_val: u32,
    tx_power_watts_val: u32,
    pa_gain_db: f32,
    sample_rate_hz: u32,
    mox_on: bool,
    is_hermes_lite: bool,
    rx_attenuation: u8,
    ps_tx_attenuation: u8,
    num_adcs: u8,
    extra_frequencies_hz: &[Arc<AtomicU32>],
    // Diversity: wire 0's (main) ADC assignment, and each extra wire's
    // (index 0 = wire 1, etc.) -- see command 6/0x1C's own doc comment
    // below for the byte layout and the prerequisite-bug context.
    adc: &Arc<AtomicU32>,
    extra_adcs: &[Arc<AtomicU32>],
    // Diversity: when true, wire 1 is the reserved ADC1 aux feed -- its
    // RX frequency (command 2) tracks the main frequency directly
    // instead of its own independent extra_frequencies_hz entry, and
    // its ADC (command 6) is forced to 1 regardless of extra_adcs[0].
    diversity_enabled: bool,
    tx_iq: &Mutex<VecDeque<f32>>,
    // PureSignal: command 10 (0x24)'s C2 bit 0x40 -- see that command's
    // own doc comment below for what it does and why it matters.
    puresignal_enabled: bool,
    rx_audio: &Mutex<VecDeque<f32>>,
    // Already resolved by the caller (not mox_on, not is_hermes_lite,
    // and the live setting itself) -- see
    // RadioSession::send_rx_audio_to_radio's doc comment.
    send_rx_audio: bool,
    rx_audio_pacer: &mut RxAudioPacer,
    rx_audio_slots_per_sample: f64,
    // See RadioSession::mic_ptt_enabled/mic_bias_enabled/mic_ptt_on_tip's
    // doc comments.
    mic_ptt_enabled: bool,
    mic_bias_enabled: bool,
    mic_ptt_on_tip: bool,
) -> [u8; PACKET_SIZE] {
    // MOX/PTT bit: inferred to be C0's bit 0 on both frames, based
    // on every register value used elsewhere in this file (0x00,
    // 0x04, ...) already being even -- i.e. bit 0 has never been
    // meaningfully used for register selection, which is
    // consistent with (but not confirmed as) it being a separate
    // MOX flag orthogonal to the register address in bits 7:1.
    // Corroborated by public HPSDR/HL2 docs, not yet verified
    // against your old_protocol.c -- flag if this differs.
    let mox_bit: u8 = if mox_on { 0x01 } else { 0x00 };

    let mut packet = [0u8; PACKET_SIZE];
    packet[0] = 0xEF;
    packet[1] = 0xFE;
    packet[2] = 0x01;
    packet[3] = EP_COMMAND_AUDIO;
    packet[4..8].copy_from_slice(&seq.to_be_bytes());

    // USB frame 1: always register 0 (general control).
    //
    // C4 confirmed against a working reference (rustyHPSDR):
    // previously hardcoded to 0x00 here, which meant the radio was
    // NEVER told the actual receiver count at all -- a real bug,
    // not just a missing nicety, since the receiver count directly
    // determines the byte stride of the interleaved IQ stream the
    // radio sends back. Duplex (bit 2) is unconditionally set in
    // the reference; antenna selection (bits 0-1) now uses the
    // same antenna value P2 already tracks. NOT yet implemented,
    // unlike the reference: per-band attenuation, EXT1/EXT2/XVTR
    // antenna types, and separate TX-vs-RX antenna selection while
    // keyed -- this project doesn't have equivalent per-band
    // config infrastructure for P1 yet.
    let c1 = sample_rate_code(sample_rate_hz);
    let mut c4: u8 = 0x04; // Duplex -- confirmed always set
    c4 |= match antenna_val {
        1 => 0x01, // ANT2
        2 => 0x02, // ANT3
        _ => 0x00, // ANT1
    };
    c4 |= (receivers.max(1) - 1) << 3;
    // BUG FIX (diversity): bit 7 was never set at all. Confirmed against
    // piHPSDR's old_protocol.c: `output_buffer[C4] |= 0x80;` whenever
    // diversity_enabled, with the comment "used to phase-synchronize RX1
    // and RX2 on some boards and enforces that the RX1 and RX2
    // frequencies are the same." Without it the radio has no signal that
    // the two DDCs it's now streaming need to stay synchronized -- a
    // very plausible cause for a real report of the radio going
    // completely silent (confirmed via packet capture: host keeps
    // sending correctly-paced packets, radio stops replying entirely)
    // roughly a second into 2-receiver P1 streaming, independent of
    // sample rate.
    if diversity_enabled {
        c4 |= 0x80;
    }
    let mut frame0 = build_usb_frame(0x00 | mox_bit, c1, 0x00, 0x00, c4);

    // USB frame 2: the rotating command. Ported directly from the
    // reference where this project has equivalent state to feed
    // it (frequency, mox, receivers, drive); fixed/inert defaults
    // where it doesn't (CW keyer, mic bias, per-band LO
    // offset/attenuation, per-receiver ADC assignment) -- flagged
    // per-command below, not silently guessed.
    let freq = frequency_hz as i32;
    let (c0b, c1b, c2b, c3b, c4b) = match *ozy_command {
        1 => {
            // TX frequency. No split-VFO support yet, so this is
            // always the same frequency as RX -- matches this
            // project's existing simplex-only assumption elsewhere
            // (see radio.rs's P2 TX-freq handling). No per-band LO
            // offset applied (not tracked here).
            (0x02, (freq >> 24) as u8, (freq >> 16) as u8, (freq >> 8) as u8, freq as u8)
        }
        2 => {
            // RX frequency for current_receiver. ROOT CAUSE FIX:
            // this previously sent the SAME frequency_hz for every
            // receiver index regardless of which one c0's register
            // address actually pointed at -- the cycling logic
            // itself was already correct (confirmed against the
            // reference), it just had no per-receiver frequency
            // source to pull from yet, so every receiver beyond the
            // first was silently retuned to the main frequency on
            // every single cycle. Now pulls each extra receiver's
            // own tracked frequency (extra_frequencies_hz, index 0 =
            // the second receiver overall), matching how Protocol 2
            // already gives each DDC its own independent VFO.
            let rx_index = *current_receiver;
            let c0 = 0x04 + (rx_index * 2);
            *current_receiver += 1;
            if *current_receiver >= receivers.max(1) {
                *current_receiver = 0;
            }
            let rx_freq = if rx_index == 0 || (diversity_enabled && rx_index == 1) {
                // Diversity's reserved wire 1 (ADC1 aux feed) is never
                // independently tunable -- it must stay locked to the
                // main frequency for the two ADCs' IQ to combine
                // meaningfully. See this function's diversity_enabled
                // doc comment.
                freq
            } else {
                extra_frequencies_hz
                    .get(rx_index as usize - 1)
                    .map(|f| f.load(Ordering::Relaxed) as i32)
                    .unwrap_or(freq)
            };
            (c0, (rx_freq >> 24) as u8, (rx_freq >> 16) as u8, (rx_freq >> 8) as u8, rx_freq as u8)
        }
        3 => {
            // Drive level (while transmitting) + mic boost. Confirmed
            // against the reference: computed from a desired power
            // target (watts) via a dBm/DAC-voltage calibration curve --
            // see p1_drive_byte_for_watts's doc comment. An earlier
            // version of this file sent the UI's 0-255 value directly
            // as a raw byte (correct for P2's confirmed-linear byte
            // 345, but not what P1 actually expects), which very
            // plausibly explains persistent "0 watts" TX output even
            // with a nonzero drive setting. Mic boost not tracked --
            // left off.
            let c1 = if mox_on {
                drive_byte_for_watts(tx_power_watts_val as f32, pa_gain_db)
            } else {
                0x00
            };
            // HermesLite/HermesLite2's REAL PA-enable mechanism,
            // confirmed against piHPSDR's old_protocol.c (case 3,
            // the DEVICE_HERMES_LITE2 block): C2 bit 3 (0x08) is what
            // actually enables the PA on this board -- NOT the C4
            // byte in command 0x14 below (an earlier attempt touched
            // that instead, based on a DIFFERENT HL2-specific block
            // in the same reference for command 0x14; both blocks
            // are real, but 0x14's C4 controls an extended RX-gain
            // range, not PA enable, so it alone didn't fix TX output).
            // C2/C3/C4 are also explicitly zeroed for HL2 here
            // (piHPSDR's comment: "do not set any Apollo/Alex bits"),
            // since those bits mean something else on this board than
            // on standard Hermes-family hardware. Sent unconditionally
            // (not gated on mox_on) to match how this board's PA
            // enable works in the reference (a persistent "PA
            // enabled" mode, not a per-transmission key) and this
            // project's existing P2 "enable PA" fix, which is also
            // unconditional.
            let (c2, c3, c4) = if is_hermes_lite { (0x08, 0x00, 0x00) } else { (0x00, 0x00, 0x00) };
            (0x12, c1, c2, c3, c4)
        }
        4 => {
            // Mic bias/PTT-source config (C1) and RX/TX
            // attenuation (C4).
            //
            // C1 bits confirmed against piHPSDR's old_protocol.c
            // (command-4 case) and radio_menu.c (exact UI labels/
            // semantics) -- see RadioSession::mic_ptt_enabled/
            // mic_bias_enabled/mic_ptt_on_tip's doc comments. Live,
            // user-configurable settings (Settings -> TX, standard
            // Angelia/Orion/Orion2 boards -- matching piHPSDR's own UI
            // gating, though the wire encoding itself isn't board-
            // gated, same as ps_tx_attenuation). Default false/false/
            // false (PTT disabled, bias off, PTT-on-Ring) matches both
            // piHPSDR's own defaults and this project's prior hardcoded
            // "always 0x40, no mic PTT" behavior, so existing setups
            // see zero behavior change until this is explicitly turned
            // on.
            //
            // BUG FIX: an earlier session's "ROOT CAUSE FIX" claimed
            // piHPSDR sends 0x3F (all attenuator bits set) here while
            // transmitting, and that this magic value is what actually
            // enables the PA -- that was a misreading. Direct source
            // inspection of old_protocol.c's real command-4 case shows
            // no such thing: the standard (non-HermesLite,
            // !have_rx_gain) branch is `output_buffer[C4] = 0x20 |
            // (transmitter->attenuation & 0x1F)` while transmitting --
            // the SAME 0x20-enable-bit-plus-attenuation shape as
            // receiving, just a different attenuation source. The only
            // real `0x3F` in that file is a completely different byte
            // (command 5/0x16's C1, the SECOND ADC's attenuator on
            // 2-ADC boards) -- conflating the two was the bug. This
            // project has no separate TX-attenuation setting, so reuses
            // RadioSession::rx_attenuation for both cases (matching
            // this project's existing simplification pattern for the
            // HermesLite RX-gain case just below), removing the
            // mox_on-dependent branch entirely for standard boards.
            //
            // ROOT CAUSE FIX (RX case, still valid): this was hardcoded
            // to a fixed 0x20 (0dB, no attenuation) regardless of
            // `rx_attenuation` -- confirmed via real hardware testing
            // (ANAN-100D/Angelia on a real HF antenna) that 0dB causes
            // genuine front-end overload from ordinary band signals.
            // piHPSDR's own reference for this byte while receiving is
            // `0x20 | ((int)adc[0].gain & 0x1F)` -- a real,
            // user-configured value, not a constant.
            //
            // HermesLite/HermesLite2 repurpose this byte entirely:
            // bit 6 (0x40) must always be set, with bits 0-5 as an
            // extended RX gain value (0-60) this project has no UI
            // for yet, left at 0 -- which happens to exactly match
            // what piHPSDR itself forces RX gain to while
            // transmitting with the PA enabled, so this simplification
            // costs nothing on TX and is a reasonable "no extra RX
            // gain boost" default otherwise.
            let c4: u8 =
                if is_hermes_lite { 0x40 } else { 0x20 | (rx_attenuation & 0x1F) };
            let mut c1 = 0u8;
            if !mic_ptt_enabled {
                c1 |= 0x40;
            }
            if mic_bias_enabled {
                c1 |= 0x20;
            }
            if mic_ptt_on_tip {
                c1 |= 0x10;
            }
            (0x14, c1, 0x00, 0x00, c4)
        }
        5 => {
            // CW keyer settings (C2-C4) -- this project has no CW
            // keyer, so all inert/off.
            //
            // piHPSDR's old_protocol.c (case 5) shows that on 2-ADC
            // boards (Angelia, Orion, Orion2) C1 is the SECOND ADC's
            // step attenuator, and bit 5 (0x20, "Att enable") "must be
            // set all the time" regardless of whether the second ADC
            // is actually in use. This project previously left C1
            // hardcoded to 0x00 unconditionally -- an unconfigured
            // second-ADC attenuator circuit was the confirmed real
            // cause of a persistent comb-pattern spectrum + sawtooth-
            // sounding audio on ANAN-100D/Angelia (fixed by always
            // setting the enable bit, at 0dB, below).
            //
            // BUG FIX: a later pass wrongly flattened this to an
            // unconditional 0x20 in every case, based on a (correct)
            // observation that command 4/0x14's C4 byte does NOT use
            // 0x3F while transmitting -- but conflated that with THIS
            // byte, which per the same reference DOES: `if
            // (isTransmitting()) { output_buffer[C1] = 0x3F; }` (max
            // attenuation, "to protect the second ADC from strong
            // signals"). Confirmed via a real packet capture of
            // piHPSDR driving this exact radio with PureSignal active:
            // C1 reads 0x3F throughout the TX+PS session, never 0x20.
            // RX5 (this board family's TX-feedback receiver, see
            // ps_feedback_config) very plausibly taps this second ADC
            // -- sending 0dB instead of the expected max attenuation
            // during TX would let the TX-feedback signal run far
            // hotter into that ADC than intended, quite possibly
            // clipping it and corrupting exactly the kind of curve fit
            // PureSignal's calibration depends on. See the PureSignal
            // plan doc's real-hardware-findings section.
            let c1: u8 = if num_adcs == 2 {
                if mox_on { 0x3F } else { 0x20 }
            } else {
                0x00
            };
            (0x16, c1, 0x00, 0x00, 0x00)
        }
        6 => {
            // Per-receiver ADC assignment (C1), 2 bits per wire index:
            // adc[chan] << (2*chan) -- confirmed against piHPSDR's
            // old_protocol.c (command 6/0x1C case): `output_buffer[C1] |=
            // (receiver[0]->adc<<(2*rx1channel));` etc, gated on
            // `n_adc > 1` (this project only ever has 1 or 2 ADCs, so
            // `num_adcs == 2` is the equivalent gate already used for
            // command 5/0x16's neighboring C1 attenuator byte above).
            //
            // BUG FIX: this was hardcoded to 0x00 -- i.e. every wire
            // always read as ADC0 regardless of what the "Add Receiver"
            // ADC dropdown (extra_adcs) was actually set to, a silent
            // no-op on Protocol 1 (Protocol 2's equivalent,
            // p2_ddc_specific_packet, already did this correctly). Found
            // while implementing diversity, which requires wire 1 to
            // really land on ADC1 to have any effect at all.
            //
            // Diversity: piHPSDR fixes wire 1 to ADC1 unconditionally
            // while diversity is enabled ("use ADC0 for RX1 and ADC1 for
            // RX2 (fixed setting)"), overriding whatever extra_adcs[0]
            // would otherwise say -- matched here the same way.
            let c1: u8 = if num_adcs == 2 {
                let mut bits = adc.load(Ordering::Relaxed) & 0x3; // wire 0, bits 0-1
                // BUG FIX: forcing wire 1 to ADC1 used to happen only
                // inside the extra_adcs loop below, which the preconfig
                // handshake (p1_send_preconfig_and_start) always calls
                // with an EMPTY extra_adcs slice -- so wire 1's bit
                // never actually got set during that critical
                // two-rotations-plus-Start sequence, only afterward once
                // the ongoing sender_loop took over. Forced here
                // unconditionally instead, independent of extra_adcs'
                // length, so it's present from the very first packet of
                // the handshake -- a real, plausible reason the radio
                // was still going silent ~1s in even after the C4 sync
                // bit fix: the FPGA may only latch dual-DDC ADC routing
                // at Start time, not on every packet afterward.
                if diversity_enabled {
                    bits |= 1 << 2; // wire 1 (chan=1), bits 2-3 = ADC1
                }
                for (i, a) in extra_adcs.iter().enumerate() {
                    let chan = i + 1; // wire index (extra_adcs[0] = wire 1)
                    if chan > 3 {
                        break; // only 2 bits/channel fit in one byte (4 channels)
                    }
                    if diversity_enabled && chan == 1 {
                        continue; // already forced above
                    }
                    bits |= (a.load(Ordering::Relaxed) & 0x3) << (2 * chan);
                }
                bits as u8
            } else {
                0x00
            };
            // BUG FIX: C3 (step attenuator of the FIRST ADC, applied
            // only while transmitting -- see RadioSession::
            // ps_tx_attenuation's doc comment) was hardcoded to 0x00,
            // meaning PureSignal's feedback path (which shares ADC0 on
            // this board family) had no attenuation control at all.
            // Confirmed against piHPSDR's old_protocol.c: `output_buffer[C3]
            // |= transmitter->attenuation;`, sent unconditionally (the
            // radio only actually applies it during TX, per the
            // reference's own comment) -- matches this byte's mox-
            // independent send here too.
            (0x1C, c1, 0x00, ps_tx_attenuation & 0x1F, 0x00)
        }
        7 => {
            // CW mode bit (C1) + sidetone volume/PTT delay (C2/C3).
            // No CW support -- all off/zero.
            (0x1E, 0x00, 0x00, 0x00, 0x00)
        }
        // Confirmed fixed values from the reference -- sent
        // unconditionally every cycle by a working client
        // regardless of any session state. Exact purpose not
        // independently documented (possibly clock/codec init);
        // included verbatim rather than omitted, since these were
        // never sent at all before this fix.
        8 => (0x20, 0x00, 0x00, 0x28, 0x0A),
        9 => (0x22, 0x19, 0x00, 0xC8, 0x00),
        10 => {
            // BUG FIX: C2 bit 0x40 ("Synchronize RX5 and TX frequency
            // on transmit (ANAN-7000)") was never set at all -- this
            // command was hardcoded to all-zeros. Confirmed via a real
            // packet capture of piHPSDR driving the same ANAN-8000DLE
            // over P1 with PureSignal enabled: this exact byte reads
            // 0x40 throughout the session (piHPSDR's old_protocol.c:
            // `if (transmitter->puresignal) { output_buffer[C2] |=
            // 0x40; }`, sent unconditionally whenever PS is enabled,
            // not gated on mox). RX5 is this board family's TX-feedback
            // receiver (see ps_feedback_config) -- without this bit,
            // firmware has no reason to keep it tracking the actual TX
            // frequency, so the "TX feedback" signal PureSignal
            // calibrates against may not even be tuned to the right
            // passband. This was found only by comparing wire bytes
            // directly against a confirmed-working reference; see the
            // PureSignal plan doc's real-hardware-findings section.
            //
            // C1 bit 0x80 ("ground RX2 on transmit") -- same capture,
            // same command, also never implemented (was hardcoded to
            // 0x00 always). piHPSDR: `if (isTransmitting()) {
            // output_buffer[C1] |= 0x80; }`, unconditional on any
            // board, not just PS -- included alongside the PS fix
            // above since it's the same command and equally confirmed,
            // even though it isn't itself PS-specific.
            //
            // C1 bits 0-6 ("BPF2" -- Alex2/RX2 bandpass filter bank,
            // ANAN-7000/8000DLE (Orion2, num_adcs==2) only): this
            // project's P1 path never set these at all. Confirmed bit
            // layout from Thetis (the reference Windows app for this
            // hardware family) -- Project Files/Source/ChannelMaster/
            // networkproto1.c, command 0x24 case ("BPF2"):
            // `C1 = _13MHz_HPF | (_20MHz_HPF<<1) | (_9_5MHz_HPF<<2) |
            // (_6_5MHz_HPF<<3) | (_1_5MHz_HPF<<4) | (_Bypass<<5) |
            // (_6M_preamp<<6) | (_rx2_gnd<<7)`, independently
            // cross-checked against netInterface.c's matching
            // bit-to-field parse of an incoming BPF2 word. Same
            // frequency-threshold ladder as alex0_word's HPF bits and
            // alex1_word's P2 equivalent (1.5M/2.1M/5.5M/11M/22M/35M),
            // just mapped to bits 0-6 here instead of alex0_word's/
            // alex1_word's own bit positions. RX2's effective frequency
            // mirrors p2_sender_loop's rx2_freq_hz (see its doc comment
            // for the real-hardware finding this priority order is based
            // on): the main receiver's own frequency whenever the main
            // receiver's own ADC dropdown is set to 1 (there's only one
            // physical Alex1 board -- if wire 0 itself is using it,
            // that's what matters) or diversity is on (both ADCs must
            // look at the same passband to combine); otherwise wire 1's
            // own independently-tuned frequency (extra_frequencies_hz[0]),
            // same as an ordinary "Add Receiver" on ADC1.
            const BPF2_13MHZ_HPF: u8 = 0x01;
            const BPF2_20MHZ_HPF: u8 = 0x02;
            const BPF2_9_5MHZ_HPF: u8 = 0x04;
            const BPF2_6_5MHZ_HPF: u8 = 0x08;
            const BPF2_1_5MHZ_HPF: u8 = 0x10;
            const BPF2_BYPASS: u8 = 0x20;
            const BPF2_6M_PREAMP: u8 = 0x40;
            let bpf2 = if num_adcs == 2 {
                let rx2_freq_hz = if diversity_enabled || adc.load(Ordering::Relaxed) == 1 {
                    frequency_hz
                } else {
                    extra_frequencies_hz.first().map(|f| f.load(Ordering::Relaxed)).unwrap_or(frequency_hz)
                };
                let f = rx2_freq_hz as f64;
                if f < 1_500_000.0 {
                    BPF2_BYPASS
                } else if f < 2_100_000.0 {
                    BPF2_1_5MHZ_HPF
                } else if f < 5_500_000.0 {
                    BPF2_6_5MHZ_HPF
                } else if f < 11_000_000.0 {
                    BPF2_9_5MHZ_HPF
                } else if f < 22_000_000.0 {
                    BPF2_13MHZ_HPF
                } else if f < 35_000_000.0 {
                    BPF2_20MHZ_HPF
                } else {
                    BPF2_6M_PREAMP
                }
            } else {
                0x00
            };
            let c1 = bpf2 | if mox_on { 0x80 } else { 0x00 };
            let c2 = if puresignal_enabled { 0x40 } else { 0x00 };
            (0x24, c1, c2, 0x00, 0x00)
        }
        _ => (0x2E, 0x00, 0x00, 0x04, 0x15),
    };
    if *current_receiver == 0 {
        *ozy_command = if *ozy_command >= 11 { 1 } else { *ozy_command + 1 };
    }
    let mut frame1 = build_usb_frame(c0b | mox_bit, c1b, c2b, c3b, c4b);

    // While keyed, both frames' payloads carry TX audio/IQ instead
    // of staying zeroed -- see fill_tx_payload's confidence note.
    // Keep sending real (or silence-padded) TX data on every
    // packet while mox_on, never a stale/half-built one: an
    // under-full or garbage payload going out while the
    // transmitter is actually keyed is worse than silence.
    if mox_on {
        fill_tx_payload(&mut frame0, tx_iq);
        fill_tx_payload(&mut frame1, tx_iq);
    } else if send_rx_audio {
        // Same 8-byte-per-sample slot fill_tx_payload uses while
        // transmitting, but for the receive side: local audio in
        // bytes 0..4 (see fill_rx_audio_payload), no IQ in bytes
        // 4..8 -- confirmed against piHPSDR's old_protocol.c
        // (old_protocol_audio_samples, sent continuously whenever
        // NOT transmitting). Paced via rx_audio_pacer -- see its doc
        // comment for why a straight one-sample-per-slot pop would
        // starve/glitch at any ADC rate other than exactly 48kHz with
        // 1 receiver.
        fill_rx_audio_payload(&mut frame0, rx_audio, rx_audio_pacer, rx_audio_slots_per_sample);
        fill_rx_audio_payload(&mut frame1, rx_audio, rx_audio_pacer, rx_audio_slots_per_sample);
    }

    packet[HEADER_SIZE..HEADER_SIZE + USB_FRAME_SIZE].copy_from_slice(&frame0);
    packet[HEADER_SIZE + USB_FRAME_SIZE..].copy_from_slice(&frame1);
    packet
}

fn sender_loop(
    socket: UdpSocket,
    frequency_hz: Arc<AtomicU32>,
    sample_rate: Arc<AtomicU32>,
    mox: Arc<AtomicBool>,
    tx_iq: Arc<Mutex<VecDeque<f32>>>,
    active_receiver_count: Arc<AtomicU32>,
    extra_frequencies_hz: Vec<Arc<AtomicU32>>,
    antenna: Arc<AtomicU32>,
    tx_power_watts: Arc<AtomicU32>,
    pa_gain_db: Arc<AtomicU32>,
    rx_attenuation: Arc<AtomicU32>,
    ps_tx_attenuation: Arc<AtomicU32>,
    is_hermes_lite: bool,
    num_adcs: u8,
    // See p1_build_packet's identically-named params' doc comments.
    adc: Arc<AtomicU32>,
    extra_adcs: Vec<Arc<AtomicU32>>,
    // Live -- see RadioSession::diversity_enabled's doc comment. This
    // loop watches it itself (comparing against the value last seen)
    // and, on a change, replays the whole preconfig+Start burst on its
    // own socket before resuming normal operation -- see the main loop
    // body below for the actual edge-detection. Reuses this loop's OWN
    // socket for the replay (see RadioSession::stop_socket's doc
    // comment for why port consistency matters here), so it happens
    // sequentially on the one thread already doing all of this loop's
    // sending -- no concurrent-sender interleaving risk, exactly
    // mirroring piHPSDR's own single-threaded diversity_cb.
    diversity_enabled: Arc<AtomicBool>,
    // PureSignal: overrides active_receiver_count's live value with a
    // FIXED total when Some -- see start_protocol1's ps_wire_total doc
    // comment for why these need to be decoupled (active_receiver_count
    // only tracks how many REAL slots the Add Receiver UI has turned
    // on; the wire-level total must stay fixed so the feedback indices
    // always land at the same position regardless of that).
    ps_wire_total: Option<u8>,
    rx_audio_to_radio: Arc<Mutex<VecDeque<f32>>>,
    send_rx_audio_to_radio: Arc<AtomicBool>,
    mic_ptt_enabled: Arc<AtomicBool>,
    mic_bias_enabled: Arc<AtomicBool>,
    mic_ptt_on_tip: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
) {
    let mut seq: u32 = 0;
    let mut ozy_command: u8 = 1;
    let mut current_receiver: u8 = 0;
    let mut rx_audio_pacer = RxAudioPacer::new();

    // Absolute-deadline pacing, not `thread::sleep(interval)` computed
    // fresh each iteration (which this loop used until now) -- same
    // fix, same reasoning, as p2_tx_iq_loop's own next_send (see its
    // doc comment for the full explanation): relative sleep-based
    // pacing lets any one iteration's jitter (packet-build time,
    // socket.send() time, OS scheduling contention with this
    // process's several other real-time-ish threads) get permanently
    // baked into every later send's timing rather than corrected on
    // the next one. This loop carries P1's TX IQ (fill_tx_payload
    // pulls from the same tx_iq queue tx.rs produces into) as well as
    // RX control, so drifting send timing here doesn't just risk a
    // late control update -- it corrupts the TX IQ stream's actual
    // timing, which for a radio whose DAC expects a steady sample
    // clock is exactly the kind of transport-level jitter that shows
    // up as broadband splatter identical regardless of the underlying
    // audio content (matching a report of the same wide/splattering
    // signal from both WDSP's own Tune tone and WSJT-X's).
    let mut next_send = Instant::now();
    // See diversity_enabled's own doc comment above for the full story
    // on this edge-detection. Initialized from the CURRENT value (not a
    // fixed `false`) so a fresh connect that starts with diversity
    // already on doesn't spuriously replay the burst it just sent
    // moments ago (in start_protocol1's own p1_send_preconfig_and_start
    // call, before this thread even existed).
    let mut last_diversity_enabled = diversity_enabled.load(Ordering::Relaxed);

    while !stop.load(Ordering::Relaxed) {
        let now_diversity_enabled = diversity_enabled.load(Ordering::Relaxed);
        if now_diversity_enabled != last_diversity_enabled {
            // Diversity was just toggled live (RadioSession::
            // set_diversity_enabled) -- replay piHPSDR's own
            // diversity_cb sequence (Stop, flip, restart) right here,
            // on this thread's own socket, sequentially: this loop is
            // the ONLY thing that ever sends on it, so there's no
            // concurrent-sender interleaving to worry about, exactly
            // matching the reference's single-threaded model (unlike
            // the old approach of tearing down this whole thread/socket
            // and starting a completely new one, which reliably hung
            // this board's P1 firmware -- see diversity_enabled's doc
            // comment for the full story).
            let mut stop_pkt = [0u8; 64];
            stop_pkt[0] = 0xEF;
            stop_pkt[1] = 0xFE;
            stop_pkt[2] = 0x04;
            stop_pkt[3] = 0x00;
            let _ = socket.send(&stop_pkt);
            let receivers_fallback = (active_receiver_count.load(Ordering::Relaxed) as u8).max(1);
            let _ = p1_send_preconfig_and_start(
                &socket,
                ps_wire_total,
                receivers_fallback,
                frequency_hz.load(Ordering::Relaxed),
                sample_rate.load(Ordering::Relaxed),
                is_hermes_lite,
                rx_attenuation.load(Ordering::Relaxed) as u8,
                ps_tx_attenuation.load(Ordering::Relaxed) as u8,
                num_adcs,
                now_diversity_enabled,
                &tx_iq,
                mic_ptt_enabled.load(Ordering::Relaxed),
                mic_bias_enabled.load(Ordering::Relaxed),
                mic_ptt_on_tip.load(Ordering::Relaxed),
            );
            last_diversity_enabled = now_diversity_enabled;
            next_send = Instant::now(); // resync pacing after the burst
        }

        let current_rate = sample_rate.load(Ordering::Relaxed);

        // Read live each cycle (not a fixed value captured at session
        // start) so a receiver added mid-session via the "Add
        // Receiver" button is actually told to the radio, matching
        // how p2_sender_loop already reads active_receiver_count.
        // PureSignal overrides this with a fixed total when active --
        // see this function's ps_wire_total doc comment.
        let receivers =
            ps_wire_total.unwrap_or_else(|| (active_receiver_count.load(Ordering::Relaxed) as u8).max(1));

        // ROOT CAUSE FIX: this used to hardcode "126 samples per
        // 1032-byte packet" regardless of `receivers`, which only
        // happens to be correct for the single-receiver case (63
        // sample-groups/frame * 2 frames -- see parse_iq_packet's
        // identical stride formula). More receivers means fewer
        // sample-groups fit in the same fixed 512-byte USB frame, so
        // a real packet actually represents LESS wall-clock time as
        // receiver count grows -- pacing against a fixed 126 paced
        // this host's outgoing C&C/TX-audio stream 4x+ too slowly
        // whenever receivers>1 relative to what the radio's own
        // real-time ADC production needs, throwing off the return IQ
        // stream's framing (P1's simple USB-audio-style protocol has
        // no independent flow control -- the host's own outgoing
        // cadence doubles as the radio's timing reference). This went
        // completely unexercised until PureSignal became the first
        // thing to ever force receivers>1 in a real session (confirmed
        // via a real hardware test: a garbled/aliased-looking waterfall
        // with PS enabled, gone the moment PS -- and therefore the
        // forced receivers=5 -- was disabled again).
        // The `8` here is the per-FRAME 3-byte-sync + 5-byte-C&C prefix
        // build_usb_frame writes (NOT the same thing as this file's
        // top-level HEADER_SIZE constant, which is the OUTER packet's
        // header -- same numeric value, 8, but a different 8 bytes) --
        // matches parse_iq_packet's identical stride formula exactly.
        let samples_per_frame = (USB_FRAME_SIZE - 8) / ((receivers as usize * 6) + 2);
        let samples_per_packet = samples_per_frame * 2; // two USB frames per packet
        let interval = Duration::from_secs_f64(samples_per_packet as f64 / current_rate as f64);
        let mox_on = mox.load(Ordering::Relaxed);
        // See RadioSession::send_rx_audio_to_radio's doc comment --
        // never sent while transmitting (fill_tx_payload owns this
        // slot then) or on HermesLite/HermesLite2 (firmware repurposes
        // these bytes for extended-address writes there).
        let send_rx_audio = !mox_on && !is_hermes_lite && send_rx_audio_to_radio.load(Ordering::Relaxed);
        // See RxAudioPacer's doc comment -- the true per-slot rate this
        // packet cadence works out to (126 fixed slots/packet, see
        // fill_rx_audio_payload's HEADER_SIZE-based stride), versus the
        // fixed 48kHz rate rx_audio_to_radio is actually filled at.
        // Always >= 1.0 in any realistic configuration (current_rate is
        // always >= 48000), but clamped defensively regardless.
        const SLOTS_PER_PACKET: f64 = 126.0;
        let rx_audio_slots_per_sample =
            (SLOTS_PER_PACKET * current_rate as f64 / (samples_per_packet as f64 * 48_000.0)).max(1.0);

        let packet = p1_build_packet(
            seq,
            &mut ozy_command,
            &mut current_receiver,
            receivers,
            frequency_hz.load(Ordering::Relaxed),
            antenna.load(Ordering::Relaxed),
            tx_power_watts.load(Ordering::Relaxed),
            f32::from_bits(pa_gain_db.load(Ordering::Relaxed)),
            current_rate,
            mox_on,
            is_hermes_lite,
            rx_attenuation.load(Ordering::Relaxed) as u8,
            ps_tx_attenuation.load(Ordering::Relaxed) as u8,
            num_adcs,
            &extra_frequencies_hz,
            &adc,
            &extra_adcs,
            now_diversity_enabled,
            &tx_iq,
            ps_wire_total.is_some(),
            &rx_audio_to_radio,
            send_rx_audio,
            &mut rx_audio_pacer,
            rx_audio_slots_per_sample,
            mic_ptt_enabled.load(Ordering::Relaxed),
            mic_bias_enabled.load(Ordering::Relaxed),
            mic_ptt_on_tip.load(Ordering::Relaxed),
        );

        if socket.send(&packet).is_err() {
            break; // socket closed or radio gone; let the thread exit
        }

        seq = seq.wrapping_add(1);

        next_send += interval;
        let now = Instant::now();
        if next_send > now {
            thread::sleep(next_send - now);
        } else {
            // Fell behind real time -- resync to now rather than
            // bursting several packets back-to-back to "catch up",
            // same reasoning as p2_tx_iq_loop's own fallback.
            next_send = now;
        }
    }
}

// ---------------------------------------------------------------------
// RX audio -> radio (P2). Separate UDP stream to P2_AUDIO_PORT, NOT
// part of the DDC-specific/High-Priority C&C cadence -- confirmed
// against piHPSDR's new_protocol.c (new_protocol_audio_samples/
// AUDIO_FROM_HOST_PORT): a 4-byte sequence number followed by 64
// interleaved 16-bit L/R samples (4 + 64*4 = 260 bytes), sent
// continuously at the audio production rate whenever not transmitting.
// Mono here (spectrum.rs's demod output), so L=R same as P1's own
// fill_rx_audio_payload.
// ---------------------------------------------------------------------

const P2_AUDIO_SAMPLES_PER_FRAME: usize = 64;
const P2_AUDIO_PACKET_SIZE: usize = 4 + P2_AUDIO_SAMPLES_PER_FRAME * 4;
const P2_AUDIO_RATE_HZ: f64 = 48_000.0; // matches spectrum.rs's OUTPUT_RATE

/// Cushion p2_rx_audio_loop waits for rx_audio_to_radio to accumulate
/// before it starts actually draining it, each time streaming (re)starts
/// -- same "absorb scheduling jitter with a buffer instead of relying on
/// razor-precise sub-millisecond thread::sleep pacing" reasoning as
/// p2_tx_iq_loop's own TX_PREBUFFER_PAIRS (see its doc comment). Without
/// this, any momentary scheduling delay on this ~1.33ms-interval loop
/// (contending with several other real-time-ish threads in this
/// process) immediately either starves the queue into silence or -- once
/// it falls behind -- lets the writer's own drop-oldest overflow handling
/// discard chunks of real audio, both of which are exactly what a real
/// report of scratchy/staticky RX audio at the radio's local output
/// sounds like. 50ms (2400 samples @ 48kHz) is a small, inaudible extra
/// latency for a monitor-audio feature (unlike TX IQ, this isn't
/// real-time-critical the way keying a transmitter is).
const AUDIO_PREBUFFER_SAMPLES: usize = 2_400;

fn p2_audio_packet(seq: u32, rx_audio: &Mutex<VecDeque<f32>>) -> [u8; P2_AUDIO_PACKET_SIZE] {
    let mut p = [0u8; P2_AUDIO_PACKET_SIZE];
    p[0..4].copy_from_slice(&seq.to_be_bytes());

    let mut buf = rx_audio.lock().unwrap();
    let mut b = 4;
    for _ in 0..P2_AUDIO_SAMPLES_PER_FRAME {
        let sample = buf.pop_front().unwrap_or(0.0);
        let s = (sample.clamp(-1.0, 1.0) * 32767.0) as i16;
        p[b] = (s >> 8) as u8;
        p[b + 1] = s as u8;
        p[b + 2] = (s >> 8) as u8;
        p[b + 3] = s as u8;
        b += 4;
    }
    p
}

/// Streams locally-demodulated RX audio to the radio's own local audio
/// output while (and only while) MOX is clear and
/// send_rx_audio_to_radio is on -- see RadioSession::
/// send_rx_audio_to_radio's doc comment. Same absolute-deadline pacing
/// approach as p2_tx_iq_loop, for the same reason (avoid jitter baked
/// into the send schedule); no prebuffer/warm-up needed here the way
/// p2_tx_iq_loop has one, since this isn't a real-time-sensitive RF
/// signal and a brief startup gap of silence is harmless.
fn p2_rx_audio_loop(
    socket: UdpSocket,
    radio_ip: std::net::IpAddr,
    mox: Arc<AtomicBool>,
    send_rx_audio_to_radio: Arc<AtomicBool>,
    rx_audio_to_radio: Arc<Mutex<VecDeque<f32>>>,
    stop: Arc<AtomicBool>,
) {
    let mut seq: u32 = 0;
    let interval = Duration::from_secs_f64(P2_AUDIO_SAMPLES_PER_FRAME as f64 / P2_AUDIO_RATE_HZ);
    let mut next_send = Instant::now();
    // See AUDIO_PREBUFFER_SAMPLES's doc comment. Reset false whenever
    // streaming (re)starts (mox clears or the setting is turned back on)
    // so each fresh start gets its own cushion rather than trusting
    // whatever's left in the queue from before.
    let mut warmed_up = false;

    while !stop.load(Ordering::Relaxed) {
        if mox.load(Ordering::Relaxed) || !send_rx_audio_to_radio.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(20));
            next_send = Instant::now();
            warmed_up = false;
            continue;
        }

        if !warmed_up {
            if rx_audio_to_radio.lock().unwrap().len() >= AUDIO_PREBUFFER_SAMPLES {
                warmed_up = true;
            } else {
                // Still building the cushion -- send silence without
                // touching the queue, so it actually accumulates instead
                // of being drained back down as fast as it fills (same
                // approach as p2_tx_iq_loop's own warm-up).
                let mut p = [0u8; P2_AUDIO_PACKET_SIZE];
                p[0..4].copy_from_slice(&seq.to_be_bytes());
                if let Err(e) = socket.send_to(&p, (radio_ip, P2_AUDIO_PORT)) {
                    eprintln!("radio: RX audio socket.send_to failed, stopping RX audio streaming: {e}");
                    return;
                }
                seq = seq.wrapping_add(1);
                next_send += interval;
                let now = Instant::now();
                if next_send > now {
                    thread::sleep(next_send - now);
                } else {
                    next_send = now;
                }
                continue;
            }
        }

        let packet = p2_audio_packet(seq, &rx_audio_to_radio);
        if let Err(e) = socket.send_to(&packet, (radio_ip, P2_AUDIO_PORT)) {
            eprintln!("radio: RX audio socket.send_to failed, stopping RX audio streaming: {e}");
            return; // socket closed or radio gone; stop this thread
        }
        seq = seq.wrapping_add(1);

        next_send += interval;
        let now = Instant::now();
        if next_send > now {
            thread::sleep(next_send - now);
        } else {
            next_send = now;
        }
    }
}

fn receiver_loop(
    socket: UdpSocket,
    buffers: Vec<Arc<Mutex<VecDeque<IqSample>>>>,
    active_receiver_count: Arc<AtomicU32>,
    sample_rate: Arc<AtomicU32>,
    tx_forward_power: Arc<AtomicU32>,
    tx_reverse_power: Arc<AtomicU32>,
    adc0_overload: Arc<AtomicBool>,
    adc1_overload: Arc<AtomicBool>,
    // PureSignal -- see start_protocol1's ps_wire_total/ps_feedback_indices
    // doc comments. All None/unused when PS wasn't requested.
    ps_wire_total: Option<u8>,
    ps_feedback_indices: Option<(u8, u8)>,
    ps_rx_feedback_iq: Arc<Mutex<VecDeque<IqSample>>>,
    ps_tx_feedback_iq: Arc<Mutex<VecDeque<IqSample>>>,
    radio_mic_audio: Arc<Mutex<VecDeque<f32>>>,
    // Diversity -- see RadioSession::diversity_main_raw_iq's doc comment.
    // When enabled, wire 0's samples are redirected here instead of
    // `buffers[0]` (parse_iq_stream), and the combiner thread (spawned
    // in RadioSession::start, or live via set_diversity_enabled) is the
    // one that ends up filling `buffers[0]` with the actual combined
    // signal. Live -- see RadioSession::diversity_enabled's doc comment;
    // read fresh each packet, same as active_receiver_count just above.
    diversity_enabled: Arc<AtomicBool>,
    diversity_main_raw_iq: Arc<Mutex<VecDeque<IqSample>>>,
    stop: Arc<AtomicBool>,
) {
    let mut buf = [0u8; PACKET_SIZE + 64]; // a little slack in case of larger packets
    // Persistent byte-stream parse state for parse_iq_stream -- see its
    // doc comment. Owned here (not per-packet) because a "frame" can
    // straddle two packets once the discovered sync phase isn't a
    // multiple of the packet size, and the discovered phase itself is
    // a connection-wide constant, not something to rediscover per call.
    let mut carry: Vec<u8> = Vec::new();
    let mut frame_synced = false;
    while !stop.load(Ordering::Relaxed) {
        match socket.recv(&mut buf) {
            Ok(n) if n == PACKET_SIZE => {
                if buf[0] == 0xEF && buf[1] == 0xFE && buf[2] == 0x01 && buf[3] == EP_IQ_DATA {
                    let capacity = iq_buffer_capacity_for_rate(sample_rate.load(Ordering::Relaxed));
                    // Read live (not a fixed value captured at session
                    // start) so the interleaving stride matches
                    // however many receivers sender_loop is CURRENTLY
                    // telling the radio to stream -- same reasoning as
                    // sender_loop's own live read just above. PureSignal
                    // overrides this with a fixed total, same as
                    // sender_loop, so both sides of the wire always
                    // agree on the stride.
                    let receivers = ps_wire_total
                        .unwrap_or_else(|| (active_receiver_count.load(Ordering::Relaxed) as u8).max(1));
                    // Return value (rx/tx feedback sample counts) was
                    // only ever consumed by a since-removed once/sec
                    // diagnostic -- see parse_iq_stream's own doc
                    // comment, kept returning the tuple regardless
                    // since it's cheap and harmless to leave bound to
                    // real behavior other callers might want later.
                    let _ = parse_iq_stream(
                        &buf[HEADER_SIZE..PACKET_SIZE],
                        receivers,
                        &buffers,
                        capacity,
                        &tx_forward_power,
                        &tx_reverse_power,
                        &adc0_overload,
                        &adc1_overload,
                        ps_feedback_indices,
                        &ps_rx_feedback_iq,
                        &ps_tx_feedback_iq,
                        &radio_mic_audio,
                        diversity_enabled.load(Ordering::Relaxed),
                        &diversity_main_raw_iq,
                        &mut carry,
                        &mut frame_synced,
                    );
                }
                // EP_WIDEBAND (0x04) and anything else: ignored for now.
            }
            Ok(_) => continue, // unexpected length, ignore
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut =>
            {
                continue
            }
            Err(_) => break,
        }
    }
}

/// ~0.25s worth of samples at the given rate, floored so very low rates
/// still get a sane minimum. Computed live (not a fixed constant) so
/// the buffer represents a constant TIME duration regardless of actual
/// sample rate -- a fixed sample count would represent much less time
/// at high rates, giving the demod thread far less headroom before a
/// brief hiccup causes samples to be dropped (heard as audio glitches).
fn iq_buffer_capacity_for_rate(sample_rate_hz: u32) -> usize {
    ((sample_rate_hz as usize) / 4).max(4_000)
}

/// `payload` is the two 512-byte USB frames (no outer 8-byte header).
///
/// ROOT CAUSE FIX for the intermittent P1 comb-pattern bug -- replaces
/// an earlier, WRONG fixed-position model (frame 0 always starts at
/// payload byte 0, frame 1 always at byte 512) that assumed every
/// received packet's 512-byte "USB frames" line up with the packet
/// boundary. Real hardware testing proved that's false: the radio's
/// actual sync preamble sits at a CONSTANT but non-zero phase offset
/// relative to that assumption (confirmed via a byte-scan diagnostic --
/// e.g. exactly 360 bytes in one observed session, identical at every
/// single frame for the entire session, never drifting). That's a
/// FIXED, connection-wide phase shift (plausibly some one-time
/// leftover-FIFO condition right after Start), not per-frame
/// corruption -- which also explains why an earlier stop+restart
/// attempt looped forever: restarting just reproduces the identical
/// fixed shift again, since whatever causes it recurs on every fresh
/// Start too.
///
/// The correct fix, confirmed by rustyHPSDR's own ACTIVE (not the
/// red-herring commented-out) protocol1/mod.rs::process_ozy_buffer,
/// which is 100% reliable on this exact hardware: treat the incoming
/// data as a continuous BYTE STREAM, not discrete fixed-size frames.
/// Discover the true sync phase once via a byte scan, then carry any
/// leftover bytes across packet boundaries indefinitely (a "frame" can
/// -- and after a phase shift, will -- straddle two packets) rather
/// than assuming each 1024-byte payload starts a fresh frame at byte 0.
/// `carry`/`frame_synced` are owned by the caller (receiver_loop) and
/// persist for the whole connection, exactly like the reference
/// client's own persistent parse state. Confirmed working on real
/// hardware, P1 with PureSignal both off and on.
#[allow(clippy::too_many_arguments)]
fn parse_iq_stream(
    payload: &[u8],
    receivers: u8,
    buffers: &[Arc<Mutex<VecDeque<IqSample>>>],
    capacity: usize,
    tx_forward_power: &Arc<AtomicU32>,
    tx_reverse_power: &Arc<AtomicU32>,
    // ADC front-end overload flags -- see RadioSession::adc0_overload's
    // doc comment. P1: confirmed against piHPSDR's old_protocol.c,
    // address 0's C1 bit 0 (ADC0) and address 4's C1/C2 bit 0
    // (ADC0/ADC1 respectively).
    adc0_overload: &Arc<AtomicBool>,
    adc1_overload: &Arc<AtomicBool>,
    // PureSignal: `ps_feedback_indices` is `Some((rx_feedback_idx,
    // tx_feedback_idx))` when active -- see ps_feedback_config's doc
    // comment. Those two wire indices are diverted into the dedicated
    // feedback queues below INSTEAD of `buffers`, which is only sized
    // to the real/user-visible receiver count (see start_protocol1's
    // real_receivers) and would be out of bounds for them.
    ps_feedback_indices: Option<(u8, u8)>,
    ps_rx_feedback_iq: &Arc<Mutex<VecDeque<IqSample>>>,
    ps_tx_feedback_iq: &Arc<Mutex<VecDeque<IqSample>>>,
    radio_mic_audio: &Arc<Mutex<VecDeque<f32>>>,
    // Diversity: when true, wire 0's samples go to
    // `diversity_main_raw_iq` instead of `buffers[0]` -- the combiner
    // thread (spawned in RadioSession::start) pairs them with wire 1's
    // raw samples (which land in `buffers[1]` completely unchanged, see
    // RadioSession::diversity_main_raw_iq's doc comment) and fills
    // `buffers[0]` itself with the combined result.
    diversity_enabled: bool,
    diversity_main_raw_iq: &Arc<Mutex<VecDeque<IqSample>>>,
    // DIAGNOSTIC (Phase 1 -- protocol plumbing verification, see
    // receiver_loop's per-second summary): returns how many samples
    // this call routed into (ps_rx_feedback_iq, ps_tx_feedback_iq), so
    // the caller can report a real arrival rate rather than just a
    // queue depth (which plateaus at PS_FEEDBACK_BUFFER_CAPACITY and
    // stops moving once full, misleadingly looking "stalled"). Remove
    // this return value once PS's real WDSP consumer exists and this
    // diagnostic is no longer needed.
    carry: &mut Vec<u8>,
    frame_synced: &mut bool,
) -> (u32, u32) {
    let mut rx_fb_pushed: u32 = 0;
    let mut tx_fb_pushed: u32 = 0;

    carry.extend_from_slice(payload);

    if !*frame_synced {
        let found = (0..carry.len().saturating_sub(2))
            .find(|&i| carry[i] == 0x7F && carry[i + 1] == 0x7F && carry[i + 2] == 0x7F);
        match found {
            Some(i) => {
                if i > 0 {
                    carry.drain(0..i);
                }
                *frame_synced = true;
            }
            None => {
                // Sync pattern not found yet -- keep accumulating, but
                // cap growth so a stream that never contains one
                // (e.g. no cable connected) doesn't grow unbounded.
                if carry.len() > 8192 {
                    carry.clear();
                }
                return (0, 0);
            }
        }
    }

    while carry.len() >= USB_FRAME_SIZE {
        // Defensive re-check: if a frame boundary we expect to be
        // sync-aligned isn't, alignment has genuinely been lost (not
        // just this code's own wrong initial assumption, since that's
        // already been corrected above) -- force full rediscovery
        // rather than silently parsing garbage.
        if carry[0] != 0x7F || carry[1] != 0x7F || carry[2] != 0x7F {
            eprintln!("radio: P1 frame sync lost mid-stream, rediscovering");
            *frame_synced = false;
            carry.clear();
            return (rx_fb_pushed, tx_fb_pushed);
        }

        // frame[0..3] = sync, frame[3..8] = C0-C4 status from the radio.
        //
        // Confirmed against a working reference: C0's bits mirror the
        // same layout the host uses when *sending* commands -- bit 0 =
        // PTT, bits 1-2 = dot/dash (not consumed here, no CW support),
        // and bits 3-7 = a status "address" the radio cycles through on
        // its own, the same way the host cycles through C&C registers.
        // Address 1 carries exciter power (C1-C2) and Alex forward
        // power (C3-C4); address 2 carries Alex reverse power (C1-C2).
        let frame = &carry[0..USB_FRAME_SIZE];
        let c0 = frame[3];
        let address = (c0 >> 3) & 0x1F;
        if address == 1 {
            let forward = u16::from_be_bytes([frame[6], frame[7]]);
            tx_forward_power.store(forward as u32, Ordering::Relaxed);
        } else if address == 2 {
            let reverse = u16::from_be_bytes([frame[4], frame[5]]);
            tx_reverse_power.store(reverse as u32, Ordering::Relaxed);
        } else if address == 0 {
            adc0_overload.store(frame[4] & 0x01 != 0, Ordering::Relaxed);
        } else if address == 4 {
            adc0_overload.store(frame[4] & 0x01 != 0, Ordering::Relaxed);
            adc1_overload.store(frame[5] & 0x01 != 0, Ordering::Relaxed);
        }

        let mut b = 8;
        let iq_samples = (USB_FRAME_SIZE - 8) / ((receivers as usize * 6) + 2);

        for _s in 0..iq_samples {
            for rx in 0..receivers as usize {
                let i = sign_extend_24(frame[b], frame[b + 1], frame[b + 2]);
                b += 3;
                let q = sign_extend_24(frame[b], frame[b + 1], frame[b + 2]);
                b += 3;
                let sample = IqSample { i, q };
                match ps_feedback_indices {
                    Some((rx_fb, _)) if rx as u8 == rx_fb => {
                        push_sample(ps_rx_feedback_iq, sample, PS_FEEDBACK_BUFFER_CAPACITY);
                        rx_fb_pushed += 1;
                    }
                    Some((_, tx_fb)) if rx as u8 == tx_fb => {
                        push_sample(ps_tx_feedback_iq, sample, PS_FEEDBACK_BUFFER_CAPACITY);
                        tx_fb_pushed += 1;
                    }
                    // Diversity: wire 0's raw ADC0 samples go to the
                    // combiner's input queue instead of buffers[0] --
                    // see this function's diversity_enabled doc comment.
                    // Wire 1 (the ADC1 aux feed) needs no special case
                    // here at all: it already lands in buffers[1], which
                    // nothing else reads while reserved.
                    _ if diversity_enabled && rx == 0 => {
                        push_sample(diversity_main_raw_iq, sample, capacity);
                    }
                    _ => push_sample(&buffers[rx], sample, capacity),
                }
            }
            // Radio's own mic ADC sample -- confirmed against piHPSDR's
            // old_protocol.c: signed 16-bit big-endian, immediately
            // after this sample-group's IQ data (same slot this project
            // previously just skipped past). See
            // RadioSession::radio_mic_audio's doc comment.
            let mic_sample = i16::from_be_bytes([frame[b], frame[b + 1]]);
            b += 2;
            push_audio_sample(radio_mic_audio, mic_sample as f32 / 32767.0, RADIO_MIC_AUDIO_CAPACITY);
        }

        carry.drain(0..USB_FRAME_SIZE);
    }

    (rx_fb_pushed, tx_fb_pushed)
}

/// Diversity combiner -- pairs ADC0's raw IQ (`diversity_main_raw_iq`,
/// redirected there by the demux instead of `iq_buffers[0]`, see
/// parse_iq_stream/p2_receiver_loop) with ADC1's raw IQ (`iq_buffers[1]`,
/// the reserved wire-1 slot, untouched by the demux) and pushes the
/// phase/gain-rotated sum into `iq_buffers[0]` -- where SpectrumHandle
/// for receiver 0 already reads from, unmodified. Ported from piHPSDR's
/// own diversity feature (`add_div_iq_samples`/`set_gain_phase` in
/// `~/github/pihpsdr/receiver.c`/`diversity_menu.c`, which the user
/// originally wrote): same formula, same live-adjustable gain(dB)/
/// phase(degrees) semantics -- `amp = 10^(gain_db/20)`, combined =
/// main + amp*(cos(phase)+j*sin(phase))*aux.
///
/// Protocol-agnostic: works identically for P1 (wire 0/1's samples
/// arrive already interleaved, sample-for-sample, within the same USB
/// frame) and P2 (DDC0/DDC1 arrive as independent, potentially jittered
/// UDP packet streams) -- pairs whatever's oldest-available from each
/// side, draining only as many samples as both queues currently have, so
/// a transient one-sided lead just leaves the surplus queued rather than
/// desyncing the pairing (same philosophy as tx.rs's drain_ps_feedback:
/// never touch one queue without the other).
///
/// Spawned whenever `session.diversity_enabled` is true -- either at
/// connect time (see RadioSession::start) or live (P1, see
/// RadioSession::set_diversity_enabled) -- `iq_buffers[1]` is guaranteed
/// to exist by then, since start_protocol1/2 both force
/// `real_receivers.max(2)` whenever diversity is on at connect time (and
/// live toggling on P1 never changes real_receivers, only which wire is
/// actively used -- see set_diversity_enabled).
///
/// `stop` is a DEDICATED flag for this combiner specifically, not
/// `session.stop_flag` (which sender_loop/receiver_loop/etc. all share
/// and which must keep running when diversity is merely toggled off) --
/// see RadioSession::diversity_combiner_stop's doc comment.
fn spawn_diversity_combiner(session: &RadioSession, stop: Arc<AtomicBool>) -> JoinHandle<()> {
    let main_raw = Arc::clone(&session.diversity_main_raw_iq);
    let aux_raw = Arc::clone(&session.iq_buffers[1]);
    let output = Arc::clone(&session.iq_buffers[0]);
    let gain_db = Arc::clone(&session.diversity_gain_db);
    let phase_deg = Arc::clone(&session.diversity_phase_deg);
    let sample_rate = Arc::clone(&session.sample_rate);
    thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            let pairs = {
                let main_q = main_raw.lock().unwrap();
                let aux_q = aux_raw.lock().unwrap();
                main_q.len().min(aux_q.len())
            };
            if pairs == 0 {
                // Idle poll -- same interval spectrum.rs's own run() loop
                // uses while its input queue is empty.
                thread::sleep(Duration::from_millis(5));
                continue;
            }
            let amp = 10f32.powf(f32::from_bits(gain_db.load(Ordering::Relaxed)) / 20.0);
            let phase_rad = f32::from_bits(phase_deg.load(Ordering::Relaxed)).to_radians();
            let (cos, sin) = (amp * phase_rad.cos(), amp * phase_rad.sin());
            let mut combined = Vec::with_capacity(pairs);
            {
                let mut main_q = main_raw.lock().unwrap();
                let mut aux_q = aux_raw.lock().unwrap();
                for _ in 0..pairs {
                    let main = main_q.pop_front().unwrap();
                    let aux = aux_q.pop_front().unwrap();
                    let (i0, q0) = (main.i as f32, main.q as f32);
                    let (i1, q1) = (aux.i as f32, aux.q as f32);
                    let i_out = i0 + (cos * i1 - sin * q1);
                    let q_out = q0 + (sin * i1 + cos * q1);
                    // Clamped to the same +/-8_388_607 (24-bit signed)
                    // range every other raw IqSample in this file
                    // represents -- gain > 0dB can genuinely push the sum
                    // outside it.
                    combined.push(IqSample {
                        i: i_out.clamp(-8_388_607.0, 8_388_607.0) as i32,
                        q: q_out.clamp(-8_388_607.0, 8_388_607.0) as i32,
                    });
                }
            }
            let capacity = iq_buffer_capacity_for_rate(sample_rate.load(Ordering::Relaxed));
            for s in combined {
                push_sample(&output, s, capacity);
            }
        }
    })
}

fn sign_extend_24(b0: u8, b1: u8, b2: u8) -> i32 {
    if b0 & 0x80 != 0 {
        i32::from_be_bytes([0xFF, b0, b1, b2])
    } else {
        i32::from_be_bytes([0, b0, b1, b2])
    }
}

fn push_sample(buf: &Arc<Mutex<VecDeque<IqSample>>>, s: IqSample, capacity: usize) {
    let mut q = buf.lock().unwrap();
    if q.len() >= capacity {
        q.pop_front();
    }
    q.push_back(s);
}

fn push_audio_sample(buf: &Arc<Mutex<VecDeque<f32>>>, s: f32, capacity: usize) {
    let mut q = buf.lock().unwrap();
    if q.len() >= capacity {
        q.pop_front();
    }
    q.push_back(s);
}

// ---------------------------------------------------------------------
// Protocol 2
//
// Unlike Protocol 1's single shared port, P2 uses four fixed destination
// ports on the radio (General/DDC-specific/TX-specific/High-priority),
// and the radio streams data back to whatever address+port the host
// used to make contact -- so one unconnected local socket handles
// everything, with incoming packets demultiplexed by source port.
//
// "Start" is not a dedicated command: setup packets are sent once
// (General, DDC-specific, TX-specific), then a High Priority packet
// with the run bit set actually starts streaming. All four are then
// resent together on the keep-alive timer, since the radio drops back
// to standby if it doesn't see a C&C packet within ~1 second.
// ---------------------------------------------------------------------

/// phase_word[31:0] = 2^32 * frequency(Hz) / DSP clock frequency (Hz)
fn phase_word(freq_hz: u32) -> u32 {
    ((4294967296.0_f64 * freq_hz as f64) / P2_DSP_CLOCK_HZ) as u32
}

fn start_protocol2(
    device: &Device,
    settings: RadioSettings,
    frequency_hz: Arc<AtomicU32>,
    sample_rate: Arc<AtomicU32>,
    adc: Arc<AtomicU32>,
    antenna: Arc<AtomicU32>,
    rx_attenuation: Arc<AtomicU32>, // P1-only setting; carried here purely to populate RadioSession's shared field
    ps_tx_attenuation: Arc<AtomicU32>, // P1-only setting; carried here purely to populate RadioSession's shared field
    mox: Arc<AtomicBool>,
    tx_iq: Arc<Mutex<VecDeque<f32>>>,
    tci_tx_audio: Arc<Mutex<VecDeque<f32>>>,
    tci_tx_gain: Arc<Mutex<f32>>,
    tx_power_watts: Arc<AtomicU32>,
    pa_gain_db: Arc<AtomicU32>,
    tx_forward_power: Arc<AtomicU32>,
    tx_reverse_power: Arc<AtomicU32>,
    adc0_overload: Arc<AtomicBool>,
    adc1_overload: Arc<AtomicBool>,
    tx_fifo_underrun: Arc<AtomicBool>,
    tx_fifo_overrun: Arc<AtomicBool>,
    ps_rx_feedback_iq: Arc<Mutex<VecDeque<IqSample>>>,
    ps_tx_feedback_iq: Arc<Mutex<VecDeque<IqSample>>>,
    rx_audio_to_radio: Arc<Mutex<VecDeque<f32>>>,
    send_rx_audio_to_radio: Arc<AtomicBool>,
    radio_mic_audio: Arc<Mutex<VecDeque<f32>>>,
    tx_audio_source: Arc<AtomicU8>,
    tci_wants_mic: Arc<AtomicBool>,
    mic_ptt_enabled: Arc<AtomicBool>,
    mic_bias_enabled: Arc<AtomicBool>,
    mic_ptt_on_tip: Arc<AtomicBool>,
    diversity_enabled: Arc<AtomicBool>,
    diversity_gain_db: Arc<AtomicU32>,
    diversity_phase_deg: Arc<AtomicU32>,
    diversity_main_raw_iq: Arc<Mutex<VecDeque<IqSample>>>,
) -> io::Result<RadioSession> {
    // PureSignal (P2): see ps_feedback_config's doc comment. DDC0/DDC1
    // are reserved ahead of real receivers, which start at DDC2 instead
    // of DDC0 (p2_sender_loop/p2_receiver_loop) -- so real-receiver
    // capacity (and therefore the "Add Receiver" UI cap, which derives
    // from iq_buffers.len()) is reduced by 2 here when PS is active.
    //
    // BUG FIX: this used to leave `settings.receivers` (from the
    // board's discovery reply) unreduced, on the wrong assumption that
    // PS's 2 reserved DDCs came out of that same total "for free". They
    // don't -- p2_sender_loop ADDS 2 reserved entries on top of however
    // many real receivers are active. A user could "Add Receiver" up to
    // the board's full advertised DDC count (e.g. 7) and PS enabled
    // would then request 9 DDCs from a 7-DDC board -- a real wire-level
    // over-request, not just a theoretical one. `.max(1)` after the
    // subtraction matches P1's own equivalent cap (`ps_feedback_config`'s
    // `max_real_receivers`), which also never allows zero real
    // receivers even on boards where PS's fixed reservation would
    // otherwise imply it.
    let puresignal_enabled =
        settings.puresignal_enabled && ps_feedback_config(2, device.board).is_some();
    let mut real_receivers = if puresignal_enabled {
        settings.receivers.max(1).saturating_sub(2).max(1)
    } else {
        settings.receivers.max(1)
    };
    // Diversity: forces at least 2 wire slots -- see start_protocol1's
    // identical handling for the full explanation.
    if settings.diversity_enabled {
        real_receivers = real_receivers.max(2);
    }

    // Confirmed against a working reference (rustyHPSDR): it explicitly
    // sets SO_REUSEADDR and (on Unix) SO_REUSEPORT before binding, via
    // socket2, rather than a plain UdpSocket::bind. Matching that here
    // for correctness/robustness (e.g. faster reconnects after a crash
    // without waiting out TIME_WAIT) -- though these are host-side
    // kernel socket options with no effect on what the radio actually
    // receives, so this isn't expected to explain the state-transition
    // problem specifically.
    let socket_addr: std::net::SocketAddr = "0.0.0.0:0".parse().expect("invalid address");
    let setup_socket = socket2::Socket::new(
        socket2::Domain::for_address(socket_addr),
        socket2::Type::DGRAM,
        Some(socket2::Protocol::UDP),
    )?;
    setup_socket.set_reuse_address(true)?;
    #[cfg(unix)]
    {
        setup_socket.set_reuse_port(true)?;
    }
    setup_socket.bind(&socket_addr.into())?;
    let socket: UdpSocket = setup_socket.into();
    socket.set_read_timeout(Some(Duration::from_millis(500)))?;
    // Deliberately not calling connect(): we need to both send to four
    // different destination ports on the radio and receive from several
    // different source ports (1025 status, 1035+ IQ, etc.) on the radio.
    let radio_ip = device.address.ip();
    // See RadioSession::stop_socket's doc comment (P1's own copy has the
    // full story) -- cloned now, before `socket` gets consumed by the
    // sender/receiver threads below.
    let stop_socket = socket.try_clone()?;

    let stop_flag = Arc::new(AtomicBool::new(false));
    let iq_buffers: Vec<Arc<Mutex<VecDeque<IqSample>>>> = (0..real_receivers)
        .map(|_| Arc::new(Mutex::new(VecDeque::with_capacity(IQ_BUFFER_CAPACITY))))
        .collect();

    // Extra receivers beyond the first, pre-sized to whatever was
    // requested (the caller sets settings.receivers from the board's
    // reported capability for P2, reduced above by PS's 2 reserved DDCs
    // when active). None are active until add_receiver() is called --
    // active_receiver_count starts at 1.
    let extra_count = real_receivers.saturating_sub(1) as usize;
    let extra_frequencies_hz: Vec<Arc<AtomicU32>> = (0..extra_count)
        .map(|_| Arc::new(AtomicU32::new(settings.frequency_hz)))
        .collect();
    let extra_sample_rates_hz: Vec<Arc<AtomicU32>> = (0..extra_count)
        .map(|_| Arc::new(AtomicU32::new(settings.sample_rate)))
        .collect();
    let extra_adcs: Vec<Arc<AtomicU32>> = (0..extra_count).map(|_| Arc::new(AtomicU32::new(0))).collect();
    // Diversity reserves wire 1 out of "Add Receiver"'s reach -- see
    // start_protocol1's identical handling for the full explanation.
    let active_receiver_count =
        Arc::new(AtomicU32::new(if settings.diversity_enabled { 2 } else { 1 }));
    // Shared between the sender and receiver threads: set by
    // p2_receiver_loop when the radio's own high-priority status
    // packet arrives, consumed by p2_sender_loop to send an immediate
    // response rather than waiting for content to change or the next
    // keepalive tick.
    let hp_request = Arc::new(AtomicBool::new(false));

    let sender_socket = socket.try_clone()?;
    let sender_stop = Arc::clone(&stop_flag);
    let sender_frequency = Arc::clone(&frequency_hz);
    let sender_sample_rate = Arc::clone(&sample_rate);
    let sender_adc = Arc::clone(&adc);
    let sender_antenna = Arc::clone(&antenna);
    let sender_extra_frequencies = extra_frequencies_hz.clone();
    let sender_extra_sample_rates = extra_sample_rates_hz.clone();
    let sender_extra_adcs = extra_adcs.clone();
    let sender_active_count = Arc::clone(&active_receiver_count);
    let sender_mox = Arc::clone(&mox);
    let sender_tx_power_watts = Arc::clone(&tx_power_watts);
    let sender_pa_gain_db = Arc::clone(&pa_gain_db);
    let sender_hp_request = Arc::clone(&hp_request);
    let sender_ps_tx_attenuation = Arc::clone(&ps_tx_attenuation);
    let sender_mic_ptt_enabled = Arc::clone(&mic_ptt_enabled);
    let sender_mic_bias_enabled = Arc::clone(&mic_bias_enabled);
    let sender_mic_ptt_on_tip = Arc::clone(&mic_ptt_on_tip);
    let num_adcs = device.adcs;
    let is_orion2 = device.board == Boards::Orion2;
    // Live -- see RadioSession::diversity_enabled's doc comment.
    let sender_diversity_enabled = Arc::clone(&diversity_enabled);
    let sender_thread = thread::spawn(move || {
        p2_sender_loop(
            sender_socket,
            radio_ip,
            num_adcs,
            is_orion2,
            sender_frequency,
            sender_sample_rate,
            sender_adc,
            sender_antenna,
            sender_extra_frequencies,
            sender_extra_sample_rates,
            sender_extra_adcs,
            sender_active_count,
            sender_mox,
            sender_tx_power_watts,
            sender_pa_gain_db,
            sender_hp_request,
            puresignal_enabled,
            sender_ps_tx_attenuation,
            sender_mic_ptt_enabled,
            sender_mic_bias_enabled,
            sender_mic_ptt_on_tip,
            sender_diversity_enabled,
            sender_stop,
        );
    });

    let tx_iq_socket = socket.try_clone()?;
    let tx_iq_stop = Arc::clone(&stop_flag);
    let tx_iq_mox = Arc::clone(&mox);
    let tx_iq_buffer = Arc::clone(&tx_iq);
    let tx_iq_thread = thread::spawn(move || {
        p2_tx_iq_loop(tx_iq_socket, radio_ip, tx_iq_mox, tx_iq_buffer, tx_iq_stop);
    });

    let rx_audio_socket = socket.try_clone()?;
    let rx_audio_stop = Arc::clone(&stop_flag);
    let rx_audio_mox = Arc::clone(&mox);
    let rx_audio_send_flag = Arc::clone(&send_rx_audio_to_radio);
    let rx_audio_buffer = Arc::clone(&rx_audio_to_radio);
    let rx_audio_thread = thread::spawn(move || {
        p2_rx_audio_loop(rx_audio_socket, radio_ip, rx_audio_mox, rx_audio_send_flag, rx_audio_buffer, rx_audio_stop);
    });

    let receiver_socket = socket.try_clone()?;
    let receiver_stop = Arc::clone(&stop_flag);
    let receiver_buffers = iq_buffers.clone();
    let receiver_sample_rate = Arc::clone(&sample_rate);
    let receiver_hp_request = Arc::clone(&hp_request);
    let receiver_tx_forward_power = Arc::clone(&tx_forward_power);
    let receiver_tx_reverse_power = Arc::clone(&tx_reverse_power);
    let receiver_adc0_overload = Arc::clone(&adc0_overload);
    let receiver_adc1_overload = Arc::clone(&adc1_overload);
    let receiver_tx_fifo_underrun = Arc::clone(&tx_fifo_underrun);
    let receiver_tx_fifo_overrun = Arc::clone(&tx_fifo_overrun);
    let receiver_ps_rx_feedback_iq = Arc::clone(&ps_rx_feedback_iq);
    let receiver_ps_tx_feedback_iq = Arc::clone(&ps_tx_feedback_iq);
    let receiver_radio_mic_audio = Arc::clone(&radio_mic_audio);
    // Live -- see RadioSession::diversity_enabled's doc comment.
    let receiver_diversity_enabled = Arc::clone(&diversity_enabled);
    let receiver_diversity_main_raw_iq = Arc::clone(&diversity_main_raw_iq);
    let receiver_thread = thread::spawn(move || {
        p2_receiver_loop(
            receiver_socket,
            receiver_buffers,
            receiver_sample_rate,
            receiver_hp_request,
            receiver_tx_forward_power,
            receiver_tx_reverse_power,
            receiver_adc0_overload,
            receiver_adc1_overload,
            receiver_tx_fifo_underrun,
            receiver_tx_fifo_overrun,
            puresignal_enabled,
            receiver_ps_rx_feedback_iq,
            receiver_ps_tx_feedback_iq,
            receiver_radio_mic_audio,
            receiver_diversity_enabled,
            receiver_diversity_main_raw_iq,
            receiver_stop,
        );
    });

    Ok(RadioSession {
        iq_buffers,
        frequency_hz,
        sample_rate,
        adc,
        antenna,
        rx_attenuation,
        ps_tx_attenuation,
        extra_frequencies_hz,
        extra_sample_rates_hz,
        extra_adcs,
        active_receiver_count,
        ps_rx_feedback_iq,
        ps_tx_feedback_iq,
        mox,
        tx_iq,
        tci_tx_audio,
        tci_tx_gain,
        rx_audio_to_radio,
        send_rx_audio_to_radio,
        radio_mic_audio,
        tx_audio_source,
        tci_wants_mic,
        mic_ptt_enabled,
        mic_bias_enabled,
        mic_ptt_on_tip,
        diversity_enabled,
        diversity_gain_db,
        diversity_phase_deg,
        diversity_main_raw_iq,
        tx_power_watts,
        pa_gain_db,
        tx_forward_power,
        tx_reverse_power,
        adc0_overload,
        adc1_overload,
        tx_fifo_underrun,
        tx_fifo_overrun,
        stop_flag,
        sender_thread: Some(sender_thread),
        receiver_thread: Some(receiver_thread),
        tx_iq_thread: Some(tx_iq_thread),
        rx_audio_thread: Some(rx_audio_thread),
        diversity_combiner_thread: None, // set by RadioSession::start right after this returns, if diversity_enabled
        diversity_combiner_stop: None,
        protocol: 2,
        radio_ip,
        stop_socket,
    })
}

// Confirmed against the protocol spec (fields only defined through
// byte 59) and the very first reference capture (General packet
// captured at exactly 60 bytes): unlike DDC-specific/High-Priority,
// which really are P2_PACKET_SIZE (1444) uniformly, the General
// packet is only 60 bytes. This was wrong for this entire project --
// sent as the full 1444 bytes -- until this fix. If the radio's
// firmware validates packet length against expected size per type
// (plausible for embedded/FPGA firmware), a wrong-sized General
// packet could be silently rejected or mishandled -- which would mean
// none of byte 58's PA-enable or byte 59's Alex-enable were ever
// actually being applied, regardless of how correct their bit values
// were, explaining "PA-enable and Alex-enable bits confirmed correct
// byte-for-byte, yet the radio never transitions" perfectly.
const P2_GENERAL_PACKET_SIZE: usize = 60;

fn p2_general_packet(seq: u32, num_adcs: u8) -> [u8; P2_GENERAL_PACKET_SIZE] {
    let mut p = [0u8; P2_GENERAL_PACKET_SIZE];
    p[0..4].copy_from_slice(&seq.to_be_bytes());
    p[4] = 0x00; // General packet command
    // Bytes 5..33: DDC/DUC/high-priority/audio/IQ port overrides, left at
    // zero throughout so the radio uses its documented default ports.
    p[23] = 0x00; // wideband not enabled
    p[37] = 0x08; // bit 3: send DDC/DUC tuning as phase word (required by all current FPGA code)
    p[38] = 0x01; // bit 0: enable hardware watchdog timer (auto-standby on lost link)
    // Confirmed against a working reference (rustyHPSDR): this was
    // missing entirely before. Without the PA itself enabled here, the
    // radio may never actually transition into transmit regardless of
    // MOX/TR_RELAY/filter word all being correct -- a strong candidate
    // for the root cause of "no state transition at all".
    p[58] = 0x01; // enable PA
    p[59] = if num_adcs == 2 { 0x03 } else { 0x01 }; // enable Alex0 (+ Alex1 if this board has 2 ADCs)
    p
}

/// `ps_mox_gate`: `Some(mox_on)` when PureSignal is active -- the
/// caller (p2_sender_loop) has then prepended 2 entries to
/// sample_rates_hz/adcs for DDC0 (RX-feedback)/DDC1 (TX-feedback)
/// ahead of any real receivers. Confirmed against piHPSDR's
/// new_protocol.c PS-specific branch: those two DDCs' enable bits are
/// gated on MOX (only stream feedback while transmitting, unlike real
/// receivers which stay enabled regardless), and a "sync DDC1 to
/// DDC0" flag at byte 1363 is required since DDC1 has no independent
/// enable bit of its own -- it only streams by riding DDC0's.
fn p2_ddc_specific_packet(
    seq: u32,
    sample_rates_hz: &[u32],
    adcs: &[u32],
    num_adcs: u8,
    ps_mox_gate: Option<bool>,
) -> [u8; P2_PACKET_SIZE] {
    let mut p = [0u8; P2_PACKET_SIZE];
    p[0..4].copy_from_slice(&seq.to_be_bytes());
    p[4] = num_adcs.max(1); // number of ADCs the board actually has

    // Dither/random: confirmed 0 (both off) against a working reference
    // capture -- corrects an earlier unconfirmed assumption here that
    // these should be all-1s ("off produces worse ADC noise"). Left at
    // 0 to match what's actually been observed working.
    p[5] = 0;
    p[6] = 0;

    // Enable bits for DDC0..DDCn-1 (byte 7 covers DDC0-7; we don't
    // support boards with more than 8 DDCs in this pass).
    let n = sample_rates_hz.len().min(8);
    p[7] = match ps_mox_gate {
        Some(mox_on) => {
            // BUG FIX: bit 0 (DDC0) only, gated on MOX; bits 2.. (real
            // receivers) always enabled, same as the non-PS formula
            // below just shifted up by the 2 reserved slots. Previously
            // set BOTH bits 0 AND 1 (0x03) -- confirmed wrong against
            // piHPSDR's new_protocol.c, which sets ONLY
            // `receive_specific_buffer[7] |= 1` (bit 0) for every
            // PureSignal-while-transmitting case, relying entirely on
            // the "sync DDC1 to DDC0" byte below to have the radio's
            // firmware embed DDC1's (ADC1/TX-feedback loopback) samples
            // into DDC0's own packet stream, hardware-synchronized --
            // DDC1 is never independently enabled (`rxcase[1]` stays
            // `RXACTION_SKIP` in every PS case in
            // update_action_table()). Explicitly enabling DDC1's own
            // bit here, on top of the sync byte, produced two
            // independently-timed streams instead of one truly
            // hardware-synced one -- confirmed via real hardware A/B
            // against piHPSDR on the same radio/antenna port/drive
            // level (piHPSDR: Feedback Level 158 at 75W; hpsdr-rs stuck
            // at 37-63 with this bug). See p2_parse_ps_feedback_packet's
            // doc comment for the matching receive-side fix.
            let real_n = n.saturating_sub(2).min(6);
            let real_bits: u8 = if real_n > 0 { (((1u16 << real_n) - 1) << 2) as u8 } else { 0 };
            let fb_bits: u8 = if mox_on { 0x01 } else { 0x00 };
            real_bits | fb_bits
        }
        None => {
            if n >= 8 {
                0xFF
            } else {
                ((1u16 << n) - 1) as u8
            }
        }
    };

    // Each DDC's config is a 6-byte entry starting at byte 17: ADC(1),
    // rate(2, ksps big-endian), CIC1(1), CIC2(1), sample size(1).
    for (i, &rate) in sample_rates_hz.iter().enumerate() {
        let base = 17 + i * 6;
        if base + 6 > P2_PACKET_SIZE {
            break; // more receivers than fit in the packet -- shouldn't happen in practice
        }
        let adc = adcs.get(i).copied().unwrap_or(0);
        p[base] = adc as u8;
        let rate_ksps = (rate / 1000) as u16;
        p[base + 1..base + 3].copy_from_slice(&rate_ksps.to_be_bytes());
        // BUG FIX: DDC1's "sample size" byte goes at base+3, NOT base+5
        // like every other DDC's entry (including DDC0's, right next to
        // it), when it's the PS-reserved TX-feedback slot (i==1 while
        // ps_mox_gate is active). Confirmed via a real packet capture
        // A/B against piHPSDR on the same radio: piHPSDR's own
        // new_protocol.c writes `receive_specific_buffer[26]=24` for
        // this entry (base+3, base=23), leaving base+5 (byte 28) at 0 --
        // NOT the `[28]=24` the general per-DDC pattern (and every other
        // DDC's own entry, confirmed byte-identical in both captures)
        // would suggest. Whether that's an intentional protocol quirk
        // for the synced/virtual DDC1 slot or a piHPSDR-side oddity
        // doesn't matter -- it's what the confirmed-working reference
        // actually sends, byte-for-byte, and hpsdr-rs's own PS
        // calibration reliably failed (WDSP's rxscheck rejecting every
        // fit attempt) sending the "logically consistent" but different
        // base+5 position instead.
        if ps_mox_gate.is_some() && i == 1 {
            p[base + 3] = 24;
        } else {
            p[base + 5] = 24; // sample size, bits
        }
    }

    if ps_mox_gate.is_some() {
        p[1363] = 0x02; // sync DDC1 to DDC0 -- DDC1 has no enable bit of its own
    }

    p
}

// Confirmed by the user: unlike the other three C&C packet types
// (General/DDC-specific/High-Priority), which really are P2_PACKET_SIZE
// (1444) uniformly, the TX-specific packet is only 60 bytes. An earlier
// version of this file sent it at the full 1444 bytes, which was wrong
// -- see p2_tx_specific_packet.
const P2_TX_SPECIFIC_PACKET_SIZE: usize = 60;

fn p2_tx_specific_packet(
    seq: u32,
    mic_ptt_enabled: bool,
    mic_bias_enabled: bool,
    mic_ptt_on_tip: bool,
    mox_on: bool,
    ps_tx_attenuation: u8,
) -> [u8; P2_TX_SPECIFIC_PACKET_SIZE] {
    let mut p = [0u8; P2_TX_SPECIFIC_PACKET_SIZE];
    p[0..4].copy_from_slice(&seq.to_be_bytes());
    // Number of DACs -- confirmed by the user: always 1, not gated on
    // mox_on. Correcting an earlier assumption here (this used to
    // toggle 0/1 with mox_on on the theory that 0 "disables the DUC
    // output path" when not transmitting) -- actual key/unkey is
    // handled entirely by the High Priority packet's MOX bit and
    // Alex's TR_RELAY flag, not by this count.
    p[4] = 1;
    // Confirmed against a working reference (rustyHPSDR) that this
    // packet does NOT carry a DUC rate/sample-size field at bytes
    // 14..17 -- an earlier version of this file invented one there,
    // which was wrong; removed.
    //
    // What the reference DOES set here, which this project doesn't
    // populate yet (left at 0, i.e. all these features off/default) --
    // confirmed non-zero in a real working session capture (values in
    // parens are what was actually observed, not guessed): byte 5 --
    // CW sidetone/keyer-mode/breakin flags (0x11 observed); byte 6 --
    // sidetone volume (0x14 observed); bytes 7-8 -- sidetone frequency
    // (0x028a = 650Hz observed); byte 9 -- keyer speed (0x0c = 12wpm
    // observed); byte 10 -- keyer weight (0x1e observed); bytes 11-12
    // -- keyer hang time (0x012c = 300ms observed); byte 51 -- line-in
    // gain (0x10 observed). None of these looked related to the "no
    // state transition" symptom (they're CW/audio-routing config, not
    // TX-enable), so still left as a follow-up rather than guessed at
    // -- but now with real confirmed values to match if it turns out
    // to matter, rather than needing to reverse-engineer them blind.
    //
    // Byte 50 -- mic/line routing flags: bits 0x01 (mic_linein) and
    // 0x02 (mic_boost) still left at 0/unimplemented (not requested).
    // Bits 0x04/0x08/0x10 confirmed against piHPSDR's new_protocol.c --
    // see RadioSession::mic_ptt_enabled/mic_bias_enabled/
    // mic_ptt_on_tip's doc comments for what each means.
    let mut b50 = 0u8;
    if !mic_ptt_enabled {
        b50 |= 0x04;
    }
    if mic_ptt_on_tip {
        b50 |= 0x08;
    }
    if mic_bias_enabled {
        b50 |= 0x10;
    }
    p[50] = b50;

    // BUG FIX: the ADC0/ADC1 step attenuators (bytes 59/58) were never
    // written here at all, staying 0 -- meaning PureSignal's "Feedback
    // Attenuation" setting (already correctly written into the High
    // Priority packet's byte 1443, see p2_high_priority_packet) never
    // actually reached the radio on modern firmware. Confirmed by
    // diffing dl1ycf's more recent piHPSDR fork (~/github/dl1ycf/pihpsdr,
    // src/new_protocol.c) against the original piHPSDR reference used
    // earlier in this project: the newer fork's own comment states
    // outright that the High Priority packet's 1442/1443 attenuator
    // bytes have "no effect according to the latest protocol
    // definition" and are only kept for old firmware -- bytes 58/59
    // here are what current firmware actually uses. This was confirmed
    // as the real gap via real-hardware evidence: raising Feedback
    // Attenuation from 0 to 31dB produced zero measurable change in the
    // RX-feedback envelope actually received (0.40271 vs 0.40270), i.e.
    // the setting was never reaching the hardware at all.
    p[59] = if mox_on { ps_tx_attenuation } else { 0 };
    p[58] = if mox_on { 31 } else { 0 };

    p
}

fn p2_high_priority_packet(
    seq: u32,
    frequencies_hz: &[u32],
    antenna: u32,
    mox_on: bool,
    tx_freq_hz: u32,
    tx_drive: u8,
    ps_tx_attenuation: u8,
    // RX2/Alex1 bandpass-filter selection (bytes 1430-1431) -- Some(freq)
    // on Orion2-class boards (see p2_sender_loop's is_orion2/rx2_freq_hz
    // doc comments), None elsewhere (byte pair left at 0, this board
    // family's own default/no-op state). Confirmed against piHPSDR's
    // new_protocol.c: without this, ADC1/RX2 has no bandpass filter path
    // selected at all -- signal-blocking, not just a nicety -- verified
    // by the user on a real ANAN-8000DLE (Orion2 family): correct
    // wiring + correct ADC assignment still produced nothing on RX2
    // until this was identified as the missing piece.
    alex1_rx2_freq_hz: Option<u32>,
) -> [u8; P2_PACKET_SIZE] {
    let mut p = [0u8; P2_PACKET_SIZE];
    p[0..4].copy_from_slice(&seq.to_be_bytes());
    // bit 0: run (unchanged -- already confirmed working for RX).
    //
    // MOX/PTT bit position is NOT confirmed against your reference.
    // Deliberately placed at bit 1 rather than reusing/overloading bit
    // 0: if this guess is wrong, the fail mode is "PTT silently
    // doesn't key the radio" (bit 1 turns out to mean something else,
    // or MOX is actually elsewhere), never "the radio transmits when
    // it shouldn't" -- getting this bit wrong must fail closed, not
    // open. Verify against new_protocol.c / the official Ethernet
    // protocol v4.3 spec before relying on this to actually key.
    let mox_bit: u8 = if mox_on { 0x02 } else { 0x00 };
    p[4] = 0x01 | mox_bit;

    // Each DDC's frequency/phase word is a 4-byte big-endian entry
    // starting at byte 9 (DDC0 = 9..13, DDC1 = 13..17, ...).
    for (i, &freq) in frequencies_hz.iter().enumerate() {
        let base = 9 + i * 4;
        if base + 4 > P2_PACKET_SIZE {
            break;
        }
        let phase = phase_word(freq);
        p[base..base + 4].copy_from_slice(&phase.to_be_bytes());
    }

    // TX frequency (bytes 329..333) and TX drive/power level (byte
    // 345, 0-255) -- both confirmed by the user. Drive is gated on
    // mox_on (0 when receiving), matching the confirmed reference --
    // it computes power as 0 whenever not transmitting, tx_drive only
    // while keyed.
    p[329..333].copy_from_slice(&phase_word(tx_freq_hz).to_be_bytes());
    p[345] = if mox_on { tx_drive } else { 0 };

    // BUG FIX: bytes 1442/1443 (ADC1/ADC0 step attenuators) were never
    // written at all, staying at the zero-initialized default -- a real
    // gap matching the one found and fixed on Protocol 1 (see
    // RadioSession::ps_tx_attenuation's doc comment). Confirmed against
    // piHPSDR's new_protocol.c: "Upon transmitting, set the attenuator
    // of ADC0 to the 'transmitter attenuation' (used in PURESIGNAL
    // signal strength adjustment) and the attenuator of ADC1 to the
    // maximum value (to protect RX2 in DIVERSITY setups)." This project
    // has no P2 RX-attenuation setting yet (unlike P1's rx_attenuation),
    // so the non-transmitting byte 1443 case is left at 0 for now --
    // only the TX-time PureSignal attenuation path is implemented here.
    p[1443] = if mox_on { ps_tx_attenuation } else { 0 };
    p[1442] = if mox_on { 31 } else { 0 };

    // Antenna/filter selection is driven by receiver 0's frequency --
    // there's only one Alex front end, shared across all DDCs.
    let primary_freq = frequencies_hz.first().copied().unwrap_or(7_100_000);
    p[1432..1436].copy_from_slice(&alex0_word(primary_freq, antenna, mox_on).to_be_bytes());

    // RX2/Alex1 bandpass filter (bytes 1430-1431) -- see this param's own
    // doc comment. BUG FIX: previously never written at all (stayed
    // 0x0000), leaving RX2's filter bank in whatever state it powered up
    // in -- plausibly no signal path selected, matching a real report of
    // "correct wiring, correct ADC assignment, still nothing on RX2".
    if let Some(rx2_freq) = alex1_rx2_freq_hz {
        p[1430..1432].copy_from_slice(&alex1_word(rx2_freq, mox_on).to_be_bytes());
    }

    // Bytes 1428-1429: the v4.3 spec documents this as an "Alex0 TX
    // relay pre-stage" field, and a previous version of this file
    // populated it on that basis -- but confirmed against three
    // independent known-working implementations (piHPSDR, linHPSDR,
    // rustyHPSDR), none of them set it; all three leave it at the
    // initialized 0x0000. Reverted to match every real-world
    // implementation actually observed, rather than a spec detail that
    // isn't actually exercised in practice. Left at 0 (the array's
    // default), so no explicit write needed here.

    p
}

/// Alex "filter1" register: HPF/preamp selection, LPF selection,
/// antenna, and T/R relay, per the Orion Mk II / ANAN-7000DLE/8000DLE
/// bit table (matches this board -- board type "Orion2"). Other board
/// families use different bit maps entirely (see the Alex appendix),
/// so this specific mapping is board-specific, not a general-protocol
/// constant.
///
/// Bit values and both filter ladders below are a direct, confirmed
/// port of the user's own reference implementation (not a guess) --
/// including the fact that HPF and LPF selection are both set on
/// *every* packet regardless of RX/TX state, with TR_RELAY (bit 27)
/// separately controlling which physical signal path (RX front-end
/// through the HPF bank, or TX output through the LPF bank) is
/// actually connected. Only the antenna/TR_RELAY handling was written
/// by me; the two threshold ladders and every constant value came
/// directly from the user.
fn alex0_word(freq_hz: u32, antenna: u32, mox_on: bool) -> u32 {
    const HPF_13MHZ: u32 = 0x00000002;
    const HPF_20MHZ: u32 = 0x00000004;
    const PREAMP_6M: u32 = 0x00000008;
    const HPF_9_5MHZ: u32 = 0x00000010;
    const HPF_6_5MHZ: u32 = 0x00000020;
    const HPF_1_5MHZ: u32 = 0x00000040;
    const HPF_BYPASS: u32 = 0x00001000;
    const LPF_30_20: u32 = 0x00100000;
    const LPF_60_40: u32 = 0x00200000;
    const LPF_80: u32 = 0x00400000;
    const LPF_160: u32 = 0x00800000;
    const ANT_1: u32 = 0x01000000;
    const ANT_2: u32 = 0x02000000;
    const ANT_3: u32 = 0x04000000;
    const TR_RELAY: u32 = 0x08000000;
    const LPF_BYPASS: u32 = 0x20000000;
    const LPF_12_10: u32 = 0x40000000;
    const LPF_17_15: u32 = 0x80000000;

    let f = freq_hz as f64;

    // HPF/preamp ladder ("set BPF" in the reference).
    let hpf = if f < 1_500_000.0 {
        HPF_BYPASS
    } else if f < 2_100_000.0 {
        HPF_1_5MHZ
    } else if f < 5_500_000.0 {
        HPF_6_5MHZ
    } else if f < 11_000_000.0 {
        HPF_9_5MHZ
    } else if f < 22_000_000.0 {
        HPF_13MHZ
    } else if f < 35_000_000.0 {
        HPF_20MHZ
    } else {
        PREAMP_6M
    };

    // LPF ladder -- previously entirely missing in this project (only
    // HPF was ever set), which is the most likely reason TX produced
    // no RF output even after TR_RELAY started being set correctly:
    // with no LPF bits set, the TX output path had no filter selected
    // at all.
    let lpf = if f > 32_000_000.0 {
        LPF_BYPASS
    } else if f > 22_000_000.0 {
        LPF_12_10
    } else if f > 15_000_000.0 {
        LPF_17_15
    } else if f > 8_000_000.0 {
        LPF_30_20
    } else if f > 4_500_000.0 {
        LPF_60_40
    } else if f > 2_400_000.0 {
        LPF_80
    } else if f > 1_500_000.0 {
        LPF_160
    } else {
        LPF_BYPASS
    };

    let ant = match antenna {
        1 => ANT_2,
        2 => ANT_3,
        _ => ANT_1,
    };

    let tr = if mox_on { TR_RELAY } else { 0 };

    hpf | lpf | ant | tr
}

/// Alex1 "RX2" bandpass filter register, ANAN-7000/8000DLE (Orion2)
/// only -- direct port of piHPSDR's new_protocol.c ORION2-specific
/// block (`alex1|=ALEX_ANAN7000_RX_*`), bit values from its alex.h.
/// Only the low 16 bits are ever used (written to bytes 1430-1431 of
/// the High Priority packet, big-endian) -- unlike alex0_word, RX2 has
/// no separate TX/LPF path of its own to select (its whole purpose is
/// RX, primarily diversity/second-receiver use), just one BPF ladder
/// plus a TX-time ground-protection bit.
fn alex1_word(freq_hz: u32, mox_on: bool) -> u16 {
    const RX_20_15_BPF: u16 = 0x0002;
    const RX_12_10_BPF: u16 = 0x0004;
    const RX_6_PRE_BPF: u16 = 0x0008;
    const RX_40_30_BPF: u16 = 0x0010;
    const RX_80_60_BPF: u16 = 0x0020;
    const RX_160_BPF: u16 = 0x0040;
    const RX_BYPASS_BPF: u16 = 0x1000;
    const RX_GND_ON_TX: u16 = 0x0100;

    let f = freq_hz as f64;
    let bpf = if f < 1_500_000.0 {
        RX_BYPASS_BPF
    } else if f < 2_100_000.0 {
        RX_160_BPF
    } else if f < 5_500_000.0 {
        RX_80_60_BPF
    } else if f < 11_000_000.0 {
        RX_40_30_BPF
    } else if f < 22_000_000.0 {
        RX_20_15_BPF
    } else if f < 35_000_000.0 {
        RX_12_10_BPF
    } else {
        RX_6_PRE_BPF
    };

    // "The main purpose of RX2 is DIVERSITY. Therefore, ground RX2 upon
    // TX *always*" -- piHPSDR's own comment, matched verbatim: protects
    // RX2's front end from the transmitter's RF regardless of whether
    // diversity or an independent second receiver is what's actually
    // using ADC1 right now.
    let gnd = if mox_on { RX_GND_ON_TX } else { 0 };

    bpf | gnd
}

fn p2_sender_loop(
    socket: UdpSocket,
    radio_ip: std::net::IpAddr,
    num_adcs: u8,
    // RX2/Alex1 bandpass-filter word (High Priority packet bytes
    // 1430-1431) -- Orion2-class boards only (confirmed against
    // piHPSDR's new_protocol.c: `if (device == NEW_DEVICE_ORION2)`).
    // See p2_high_priority_packet's alex1_rx2_freq_hz doc comment for
    // why this is a real, previously-missing gap: without it, ADC1/RX2
    // has no bandpass filter path selected at all, so nothing gets
    // through regardless of correct wiring/ADC assignment -- confirmed
    // by the user on a real ANAN-8000DLE (an Orion2-family board).
    is_orion2: bool,
    frequency_hz: Arc<AtomicU32>,
    sample_rate: Arc<AtomicU32>,
    adc: Arc<AtomicU32>,
    antenna: Arc<AtomicU32>,
    extra_frequencies_hz: Vec<Arc<AtomicU32>>,
    extra_sample_rates_hz: Vec<Arc<AtomicU32>>,
    extra_adcs: Vec<Arc<AtomicU32>>,
    active_receiver_count: Arc<AtomicU32>,
    mox: Arc<AtomicBool>,
    tx_power_watts: Arc<AtomicU32>,
    pa_gain_db: Arc<AtomicU32>,
    hp_request: Arc<AtomicBool>,
    // PureSignal -- see ps_feedback_config's doc comment. When true,
    // DDC0/DDC1 are reserved for feedback (RX-feedback/TX-feedback
    // respectively) ahead of any real receivers -- confirmed universal
    // on P2 regardless of board, unlike P1's board-dependent table.
    puresignal_enabled: bool,
    // See RadioSession::ps_tx_attenuation's doc comment.
    ps_tx_attenuation: Arc<AtomicU32>,
    // See RadioSession::mic_ptt_enabled/mic_bias_enabled/mic_ptt_on_tip's
    // doc comments.
    mic_ptt_enabled: Arc<AtomicBool>,
    mic_bias_enabled: Arc<AtomicBool>,
    mic_ptt_on_tip: Arc<AtomicBool>,
    // Diversity -- see RadioSession::diversity_enabled's doc comment.
    // Mutually exclusive with puresignal_enabled (enforced in main.rs's
    // Settings UI), so no interaction with that reserved-DDC scheme
    // above to worry about here. Live -- read fresh each cycle, same as
    // active_receiver_count just above: unlike P1, P2 has no discrete
    // preconfig/Start handshake to replay at all, it just continuously
    // sends updated DDC-specific/High-Priority packets on a timer, so a
    // live toggle needs nothing extra beyond reading this fresh --
    // RadioSession::set_diversity_enabled (bump active_receiver_count,
    // spawn/stop the combiner) already covers the rest.
    diversity_enabled: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
) {
    let mut general_seq: u32 = 0;
    let mut ddc_seq: u32 = 0;
    let mut tx_seq: u32 = 0;
    let mut hp_seq: u32 = 0;

    // Earlier versions of this loop tried "send only on change" for
    // TX-specific and High-Priority, based on a description of the
    // reference's behavior -- but a closer look at the actual
    // reference (rustyHPSDR) shows all four C&C packets share ONE
    // send trigger on its slow path (`if keepalive || updated { send
    // all four }`), which is what the unconditional periodic send
    // below (every P2_KEEPALIVE_INTERVAL) still mirrors.
    //
    // On top of that slow path, the reference also has a SECOND, much
    // faster path -- confirmed by reading its actual source (rustyHPSDR's
    // protocol2/mod.rs), not inferred: every single incoming
    // High-Priority status packet from the radio (port 1025, arriving
    // roughly every ~1ms while running, confirmed via a real packet
    // capture) immediately triggers an outgoing High-Priority reply,
    // keeping the drive/power byte continuously fresh rather than
    // stale for up to a full P2_KEEPALIVE_INTERVAL. hp_request (set by
    // p2_receiver_loop on every incoming status packet) now actually
    // gates this fast reactive resend below, instead of being
    // tracked-but-unused as before.
    //
    // NOTE on why this exists despite NOT being the fix for the bug
    // that prompted it: this was originally added chasing a report of
    // TX output power bouncing between the expected level and 0W on a
    // steady carrier, on the theory that a stale drive command was the
    // cause. An A/B test (this reactive send on vs. off, same board,
    // same test) showed an identical bounce pattern either way, ruling
    // that out -- the real cause turned out to be a raw single-packet
    // ADC ripple that's apparently normal for this board, made highly
    // visible only because the UI redrew it unsmoothed every frame (see
    // main.rs's smoothed_fwd_power/smoothed_rev_power, which is the
    // actual fix). This reactive send is kept anyway because it's still
    // a real, confirmed improvement over a 250ms-stale drive command
    // matching the reference's own behavior -- just not the fix for
    // that specific bug.
    let mut next_keepalive = Instant::now();
    // Diagnostic only -- edge-triggered (not every send) log of how many
    // DDCs this client is actually asking the radio to enable. Added
    // alongside p2_receiver_loop's "first IQ packet per DDC" log: if
    // `active` reaches 7 here but no IQ ever arrives on DDC4-6's ports,
    // that rules out a client-side bug in *requesting* the extra DDCs
    // and points at the radio itself (hardware/firmware not actually
    // streaming that many concurrent DDCs despite advertising support
    // for them in its discovery reply).
    let mut last_logged_active: Option<usize> = None;

    while !stop.load(Ordering::Relaxed) {
        let due_for_keepalive = Instant::now() >= next_keepalive;
        let reactive_hp = hp_request.swap(false, Ordering::Relaxed);
        if !due_for_keepalive && !reactive_hp {
            thread::sleep(Duration::from_millis(1));
            continue;
        }

        let active = (active_receiver_count.load(Ordering::Relaxed) as usize).max(1);
        if last_logged_active != Some(active) {
            eprintln!("radio: requesting {active} active DDC(s) from the radio");
            last_logged_active = Some(active);
        }
        // Live -- see this function's diversity_enabled doc comment.
        let now_diversity_enabled = diversity_enabled.load(Ordering::Relaxed);
        let mox_on = mox.load(Ordering::Relaxed);
        // No split VFO support yet -- TX frequency is always the
        // primary receiver's frequency (simplex). Computed up front
        // (not derived from freqs[0] further down) since PureSignal's
        // reserved DDC0/DDC1 entries need this same value prepended
        // ahead of the real receiver frequencies below.
        let tx_freq_hz = frequency_hz.load(Ordering::Relaxed);

        let mut freqs = Vec::with_capacity(active + 2);
        let mut rates = Vec::with_capacity(active + 2);
        let mut adcs = Vec::with_capacity(active + 2);
        // PureSignal: DDC0 (RX-feedback, ADC0) and DDC1 (TX-feedback,
        // the virtual loopback ADC past this board's real ones) always
        // tuned to the TX frequency and running at a fixed 192ksps --
        // confirmed against piHPSDR's new_protocol.c PS-specific
        // branch. Prepended ahead of any real receivers, which get
        // pushed to DDC2+ as a result -- see p2_ddc_specific_packet's
        // ps_mox_gate param for how their enable bits get gated on MOX
        // separately from these two.
        if puresignal_enabled {
            freqs.push(tx_freq_hz);
            rates.push(192_000);
            adcs.push(0);
            freqs.push(tx_freq_hz);
            rates.push(192_000);
            adcs.push(num_adcs as u32);
        }
        let main_freq = frequency_hz.load(Ordering::Relaxed);
        let main_adc = adc.load(Ordering::Relaxed);
        // RX2/Alex1 bandpass filter frequency -- see p2_sender_loop's
        // is_orion2 doc comment. Confirmed against piHPSDR: while
        // diversity is enabled, RX2's filter must track the MAIN
        // receiver's frequency (the two ADCs need to be looking at the
        // same passband to combine meaningfully). BUG FIX: previously
        // this only ever considered wire 1 (an added second receiver)
        // for the non-diversity case, silently ignoring the case where
        // the MAIN receiver's own ADC dropdown is set to 1 -- a real,
        // independently-selectable configuration this project already
        // supports (Settings -> RX -> ADC), and the exact one the user
        // confirmed exhibits this: 40m->20m produces a relay click when
        // the MAIN receiver is on ADC1 (proving that click was actually
        // Alex0 reacting to a frequency change on a receiver whose real
        // analog front end is Alex1, not evidence Alex1 itself was
        // tracking correctly). Priority: main receiver on ADC1 wins
        // (there's only one physical Alex1 board -- if wire 0 itself is
        // using it, that's what matters); otherwise falls back to wire
        // 1's own independent frequency (extra_frequencies_hz[0]), same
        // as an ordinary "Add Receiver" tuned to ADC1. Sent
        // unconditionally whenever this is an Orion2 board, regardless
        // of whether ADC1 is currently in use by anything, matching the
        // reference's own always-on behavior (harmless otherwise).
        let rx2_freq_hz = if now_diversity_enabled || main_adc == 1 {
            main_freq
        } else {
            extra_frequencies_hz.first().map(|f| f.load(Ordering::Relaxed)).unwrap_or(main_freq)
        };
        freqs.push(main_freq);
        rates.push(sample_rate.load(Ordering::Relaxed));
        adcs.push(main_adc);
        for i in 0..active.saturating_sub(1) {
            // Diversity: wire 1 (i==0 here) is the reserved ADC1 aux
            // feed -- never independently tunable, and always ADC1
            // regardless of extra_adcs[0]. See RadioSession::
            // diversity_enabled's doc comment.
            if now_diversity_enabled && i == 0 {
                freqs.push(main_freq);
                rates.push(sample_rate.load(Ordering::Relaxed));
                adcs.push(1);
                continue;
            }
            if let Some(f) = extra_frequencies_hz.get(i) {
                freqs.push(f.load(Ordering::Relaxed));
            }
            if let Some(r) = extra_sample_rates_hz.get(i) {
                rates.push(r.load(Ordering::Relaxed));
            }
            if let Some(a) = extra_adcs.get(i) {
                adcs.push(a.load(Ordering::Relaxed));
            }
        }

        let antenna_now = antenna.load(Ordering::Relaxed);
        let drive = drive_byte_for_watts(
            tx_power_watts.load(Ordering::Relaxed) as f32,
            f32::from_bits(pa_gain_db.load(Ordering::Relaxed)),
        );
        let ps_mox_gate = puresignal_enabled.then_some(mox_on);
        let ps_tx_atten = ps_tx_attenuation.load(Ordering::Relaxed) as u8;

        if due_for_keepalive {
            let general = p2_general_packet(general_seq, num_adcs);
            let ddc = p2_ddc_specific_packet(ddc_seq, &rates, &adcs, num_adcs, ps_mox_gate);
            let tx = p2_tx_specific_packet(
                tx_seq,
                mic_ptt_enabled.load(Ordering::Relaxed),
                mic_bias_enabled.load(Ordering::Relaxed),
                mic_ptt_on_tip.load(Ordering::Relaxed),
                mox_on,
                ps_tx_atten,
            );
            let hp =
                p2_high_priority_packet(
                    hp_seq,
                    &freqs,
                    antenna_now,
                    mox_on,
                    tx_freq_hz,
                    drive,
                    ps_tx_atten,
                    is_orion2.then_some(rx2_freq_hz),
                );

            let sends: [(&[u8], u16); 5] = [
                (&general[..], P2_GENERAL_PORT),
                (&ddc[..], P2_DDC_SPECIFIC_PORT),
                // Confirmed against a real working capture: DDC-specific
                // goes out twice per cycle, back-to-back, byte-identical
                // -- not a bug in the reference, an actual quirk of how
                // it talks to the radio.
                (&ddc[..], P2_DDC_SPECIFIC_PORT),
                (&tx[..], P2_TX_SPECIFIC_PORT),
                (&hp[..], P2_HIGH_PRIORITY_PORT),
            ];

            for (packet, port) in sends {
                if socket.send_to(packet, (radio_ip, port)).is_err() {
                    return; // socket closed or radio gone; stop this thread
                }
            }

            general_seq = general_seq.wrapping_add(1);
            ddc_seq = ddc_seq.wrapping_add(1);
            tx_seq = tx_seq.wrapping_add(1);
            hp_seq = hp_seq.wrapping_add(1);
            next_keepalive = Instant::now() + P2_KEEPALIVE_INTERVAL;
        } else {
            // Reactive path -- HP only, matching the reference's own
            // send_high_priority-on-every-status-packet behavior. Kept
            // deliberately minimal (not resending all four) to match
            // what was actually confirmed in the reference source
            // rather than guessing it should be more than that.
            let hp =
                p2_high_priority_packet(
                    hp_seq,
                    &freqs,
                    antenna_now,
                    mox_on,
                    tx_freq_hz,
                    drive,
                    ps_tx_atten,
                    is_orion2.then_some(rx2_freq_hz),
                );
            if socket.send_to(&hp, (radio_ip, P2_HIGH_PRIORITY_PORT)).is_err() {
                return;
            }
            hp_seq = hp_seq.wrapping_add(1);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn p2_receiver_loop(
    socket: UdpSocket,
    buffers: Vec<Arc<Mutex<VecDeque<IqSample>>>>,
    sample_rate: Arc<AtomicU32>,
    hp_request: Arc<AtomicBool>,
    tx_forward_power: Arc<AtomicU32>,
    tx_reverse_power: Arc<AtomicU32>,
    adc0_overload: Arc<AtomicBool>,
    adc1_overload: Arc<AtomicBool>,
    tx_fifo_underrun: Arc<AtomicBool>,
    tx_fifo_overrun: Arc<AtomicBool>,
    // PureSignal -- see p2_sender_loop's matching doc comment. When
    // true, DDC0's IQ (source port P2_DDC0_IQ_PORT) is diverted into
    // BOTH feedback queues at once (see p2_parse_ps_feedback_packet's
    // doc comment -- DDC0 alone carries both feedback streams
    // interleaved; DDC1 is never independently enabled) instead of
    // `buffers`, which is sized to real receivers only and indexed 2
    // lower (DDC2 -> buffers[0], DDC3 -> buffers[1], ...).
    puresignal_enabled: bool,
    ps_rx_feedback_iq: Arc<Mutex<VecDeque<IqSample>>>,
    ps_tx_feedback_iq: Arc<Mutex<VecDeque<IqSample>>>,
    radio_mic_audio: Arc<Mutex<VecDeque<f32>>>,
    // Diversity -- see RadioSession::diversity_main_raw_iq's doc comment
    // and parse_iq_stream's identical P1-side handling. Live -- read
    // fresh each packet, same as p2_sender_loop's own live read.
    diversity_enabled: Arc<AtomicBool>,
    diversity_main_raw_iq: Arc<Mutex<VecDeque<IqSample>>>,
    stop: Arc<AtomicBool>,
) {
    let mut buf = [0u8; P2_PACKET_SIZE + 64];
    // Diagnostic only -- added while chasing a report that extra
    // receivers beyond the 4th show no spectrum/waterfall at all.
    // Logs once, the first time any IQ packet actually arrives from a
    // given DDC's source port, so it's possible to tell "the radio
    // never sends this DDC's IQ at all" (a hardware/firmware/bandwidth
    // limit outside this codebase) apart from "IQ arrives fine but WDSP
    // isn't turning it into spectrum/waterfall pixels" (a WDSP-side
    // issue -- see SpectrumAnalyzer::open's XCreateAnalyzer success
    // check and demod()'s fexchange0 error check).
    let ddc_reserved = if puresignal_enabled { 2 } else { 0 };
    let mut ddc_seen = vec![false; buffers.len() + ddc_reserved];
    // BUG FIX: forward/reverse power used to be stored as the single
    // latest raw per-packet ADC reading, with only a UI-frame-rate
    // exponential filter downstream of that. Confirmed against
    // piHPSDR's new_protocol.c, which reads the exact same bytes but
    // immediately folds each one into its own 16-sample moving average
    // right here at the packet layer (`fwd_acc = (15*fwd_acc+val)/16`),
    // with an explicit comment that the raw per-packet reading needs
    // this. Real-hardware report: hpsdr-rs's own on-screen TX power
    // cycling 35-55W (roughly matching a smoothed 0W/100W average)
    // while an external wattmeter showed a steady 100W -- diagnostic
    // logging of the raw atomic confirmed it was genuinely alternating
    // between near-zero and full-scale on almost every consecutive
    // packet, with every other real pipeline (TCI audio queue, DUC IQ
    // queue, radio's own TX FIFO) confirmed healthy, i.e. this really
    // is just the expected raw ADC noise piHPSDR's own comment warns
    // about, sampled far too sparsely (once per UI frame, not once per
    // packet) to average out properly. Averaging every packet here
    // instead fixes it at the source rather than papering over it with
    // even heavier UI-side smoothing.
    let mut fwd_acc: u32 = 0;
    let mut rev_acc: u32 = 0;
    while !stop.load(Ordering::Relaxed) {
        match socket.recv_from(&mut buf) {
            Ok((n, src)) => {
                let port = src.port();
                if port >= P2_DDC0_IQ_PORT
                    && ((port - P2_DDC0_IQ_PORT) as usize) < buffers.len() + ddc_reserved
                {
                    let ddc = (port - P2_DDC0_IQ_PORT) as usize;
                    if !ddc_seen[ddc] {
                        ddc_seen[ddc] = true;
                        eprintln!("radio: first IQ packet received for DDC{ddc} (port {port})");
                    }
                    if n == P2_PACKET_SIZE {
                        let capacity = iq_buffer_capacity_for_rate(sample_rate.load(Ordering::Relaxed));
                        if puresignal_enabled && ddc == 0 {
                            // See p2_parse_ps_feedback_packet's doc comment --
                            // DDC0 alone carries both feedback streams,
                            // interleaved. DDC1 (ddc == 1) is never
                            // independently enabled (see
                            // p2_ddc_specific_packet's fb_bits fix), so no
                            // separate branch for it here.
                            p2_parse_ps_feedback_packet(
                                &buf[..n],
                                &ps_rx_feedback_iq,
                                &ps_tx_feedback_iq,
                                PS_FEEDBACK_BUFFER_CAPACITY,
                            );
                        } else if diversity_enabled.load(Ordering::Relaxed) && ddc - ddc_reserved == 0 {
                            // Wire 0's raw ADC0 samples go to the
                            // combiner's input queue instead of
                            // buffers[0] -- wire 1 (the ADC1 aux feed)
                            // needs no special case, it already lands in
                            // buffers[1] untouched.
                            p2_parse_ddc_iq_packet(&buf[..n], &diversity_main_raw_iq, capacity);
                        } else {
                            p2_parse_ddc_iq_packet(&buf[..n], &buffers[ddc - ddc_reserved], capacity);
                        }
                    } else if puresignal_enabled && ddc == 0 {
                        // Packet arrived the wrong size (rare -- e.g. a
                        // truncated/corrupt UDP datagram); nothing to
                        // parse, drop it.
                    }
                } else if port == P2_HP_STATUS_SOURCE_PORT {
                    // Confirmed by the user: the radio's own
                    // high-priority status packets should prompt an
                    // immediate response, not just the change-detected/
                    // keepalive send p2_sender_loop otherwise does.
                    //
                    // Forward power (bytes 14-15) and reverse power
                    // (bytes 22-23) confirmed against the official
                    // protocol spec ("Bytes 14 & 15... forward power
                    // from the exciter Power Amplifier... Bytes 22 &
                    // 23... reverse power from the exciter Power
                    // Amplifier"). See RadioSession::tx_forward_power's
                    // doc comment on why this isn't converted to real
                    // watts here.
                    if n >= 24 {
                        let forward = u16::from_be_bytes([buf[14], buf[15]]) as u32;
                        fwd_acc = (15 * fwd_acc + forward) / 16;
                        tx_forward_power.store(fwd_acc, Ordering::Relaxed);
                        let reverse = u16::from_be_bytes([buf[22], buf[23]]) as u32;
                        rev_acc = (15 * rev_acc + reverse) / 16;
                        tx_reverse_power.store(rev_acc, Ordering::Relaxed);
                    }
                    // ADC0/ADC1 front-end overload -- see
                    // RadioSession::adc0_overload's doc comment. Byte 5,
                    // bits 0/1, confirmed against piHPSDR's
                    // new_protocol.c.
                    if n >= 6 {
                        adc0_overload.store(buf[5] & 0x01 != 0, Ordering::Relaxed);
                        adc1_overload.store(buf[5] & 0x02 != 0, Ordering::Relaxed);
                    }
                    // TX FIFO overrun/underrun -- see
                    // RadioSession::tx_fifo_underrun's doc comment.
                    // Byte 4, bits 0x40/0x20, confirmed against
                    // piHPSDR's new_protocol.c.
                    if n >= 5 {
                        tx_fifo_overrun.store(buf[4] & 0x40 != 0, Ordering::Relaxed);
                        tx_fifo_underrun.store(buf[4] & 0x20 != 0, Ordering::Relaxed);
                    }
                    hp_request.store(true, Ordering::Relaxed);
                } else if port == P2_TX_SPECIFIC_PORT {
                    // Radio's own mic ADC samples -- confirmed against
                    // piHPSDR's new_protocol.c (process_mic_data): this
                    // SOURCE port (distinct from this same port number's
                    // OUTGOING use for TX-specific config, same "reused
                    // number, different direction" convention as
                    // P2_HP_STATUS_SOURCE_PORT above). See
                    // RadioSession::radio_mic_audio's doc comment.
                    p2_parse_mic_packet(&buf[..n], &radio_mic_audio);
                }
                // wideband (1027), command replies (1024): still not
                // consumed.
            }
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut =>
            {
                continue
            }
            Err(_) => break,
        }
    }
}

/// DDC I&Q packet: 4-byte seq, 8-byte timestamp, 2-byte bits-per-sample
/// (always 24), 2-byte samples-per-frame (always 238), then interleaved
/// 3-byte I / 3-byte Q samples.
fn p2_parse_ddc_iq_packet(packet: &[u8], buffer: &Arc<Mutex<VecDeque<IqSample>>>, capacity: usize) {
    if packet.len() < 16 {
        return;
    }
    let samples_per_frame = u16::from_be_bytes([packet[14], packet[15]]) as usize;
    let mut b = 16;
    for _ in 0..samples_per_frame {
        if b + 6 > packet.len() {
            break;
        }
        let i = sign_extend_24(packet[b], packet[b + 1], packet[b + 2]);
        let q = sign_extend_24(packet[b + 3], packet[b + 4], packet[b + 5]);
        b += 6;
        push_sample(buffer, IqSample { i, q }, capacity);
    }
}

/// PureSignal (Protocol 2 only): DDC0's packets carry BOTH RX-feedback
/// and TX-feedback (loopback) samples interleaved, hardware-
/// synchronized -- see p2_ddc_specific_packet's fb_bits doc comment for
/// the matching send-side fix (only DDC0's enable bit is set; DDC1's
/// samples arrive embedded here instead of on its own port). Confirmed
/// against piHPSDR's process_ps_iq_data: each step of the packet's own
/// declared samples_per_frame contributes TWO back-to-back IQ pairs --
/// first RX-feedback (this board's real ADC0), then TX-feedback (the
/// virtual DUC-loopback ADC) -- not one, unlike every other DDC's
/// packets (see p2_parse_ddc_iq_packet just above). Since both feedback
/// streams come from the SAME packet, they're guaranteed sample-for-
/// sample aligned -- exactly what makes tx.rs's drain_ps_feedback's
/// simple positional 1:1 pairing correct.
fn p2_parse_ps_feedback_packet(
    packet: &[u8],
    rx_feedback: &Arc<Mutex<VecDeque<IqSample>>>,
    tx_feedback: &Arc<Mutex<VecDeque<IqSample>>>,
    capacity: usize,
) {
    if packet.len() < 16 {
        return;
    }
    let samples_per_frame = u16::from_be_bytes([packet[14], packet[15]]) as usize;
    let mut b = 16;
    let mut i = 0;
    while i + 1 < samples_per_frame {
        if b + 12 > packet.len() {
            break;
        }
        let rx = IqSample {
            i: sign_extend_24(packet[b], packet[b + 1], packet[b + 2]),
            q: sign_extend_24(packet[b + 3], packet[b + 4], packet[b + 5]),
        };
        let tx = IqSample {
            i: sign_extend_24(packet[b + 6], packet[b + 7], packet[b + 8]),
            q: sign_extend_24(packet[b + 9], packet[b + 10], packet[b + 11]),
        };
        b += 12;
        i += 2;
        push_sample(rx_feedback, rx, capacity);
        push_sample(tx_feedback, tx, capacity);
    }
}

/// Radio's own mic ADC packet: 4-byte seq, then P2_MIC_SAMPLES_PER_FRAME
/// interleaved signed 16-bit big-endian samples -- confirmed against
/// piHPSDR's new_protocol.c (process_mic_data/MIC_SAMPLES=64, matching
/// this project's own 4+64*2=132-byte expectation).
const P2_MIC_SAMPLES_PER_FRAME: usize = 64;

fn p2_parse_mic_packet(packet: &[u8], radio_mic_audio: &Arc<Mutex<VecDeque<f32>>>) {
    if packet.len() < 4 + P2_MIC_SAMPLES_PER_FRAME * 2 {
        return;
    }
    let mut b = 4;
    for _ in 0..P2_MIC_SAMPLES_PER_FRAME {
        let sample = i16::from_be_bytes([packet[b], packet[b + 1]]);
        b += 2;
        push_audio_sample(radio_mic_audio, sample as f32 / 32767.0, RADIO_MIC_AUDIO_CAPACITY);
    }
}

// ---------------------------------------------------------------------
// Protocol 2 TX (DUC) IQ streaming
//
// Confirmed against a working reference (rustyHPSDR's Protocol2::
// send_iq_buffer): destination port P2_TX_IQ_PORT (1029), and the
// packet layout is NOT the same shape as the confirmed incoming DDC IQ
// packet the way an earlier version of this file assumed by symmetry
// -- there's no timestamp/bits-per-sample/samples-per-frame header at
// all here, just a 4-byte sequence number followed immediately by 240
// interleaved 24-bit I/Q samples (4 + 240*6 = 1444 bytes, filling
// P2_PACKET_SIZE exactly with no padding).
// ---------------------------------------------------------------------

const P2_DUC_SAMPLES_PER_FRAME: usize = 240; // confirmed (rustyHPSDR's IQ_BUFFER_SIZE)
const P2_DUC_RATE_HZ: f64 = 192_000.0; // matches p2_high_priority_packet's TX phase word clock assumption

/// How many IQ pairs p2_tx_iq_loop lets tx_iq accumulate before it
/// starts actually draining it, each time MOX goes active. One full
/// production cycle from tx.rs's TxProcessor (512 mic samples *
/// duc_ratio 4 at 192ksps/48kHz = 2048 pairs) arrives in one lump every
/// ~10.7ms, while this loop drains it steadily at 240 pairs/1.25ms in
/// between. A one-lump cushion (2048) got real, continuous underruns
/// (~1-2% of packets throughout a whole transmission) down to a single
/// occasional packet right at the key-down transition itself -- the
/// narrow race between finishing that first cushion and tx.rs's *next*
/// lump landing. Two full lumps' worth of margin instead of one, for
/// that last transition-edge case: at the cost of one more ~10.7ms of
/// one-time TX audio latency per PTT (now ~21ms total), still
/// essentially imperceptible for voice.
const TX_PREBUFFER_PAIRS: usize = 4096;

/// Returns (packet, starved) -- `starved` is true if tx_iq had fewer
/// than P2_DUC_SAMPLES_PER_FRAME I/Q pairs already buffered at the
/// start of this call, meaning at least part of this packet's payload
/// is unwrap_or(0.0) silence rather than real TXA output. See
/// p2_tx_iq_loop's aggregate diagnostic -- added to check for
/// production/consumption starvation at THIS stage (radio.rs's own
/// queue drain) specifically, as distinct from tx.rs's separate
/// mic-capture-buffer diagnostic, while chasing a reported wideband/
/// dirty TX spectrum: a starved chunk here means real, audible/
/// visible gaps get spliced into an otherwise-continuous carrier,
/// which is a textbook cause of broadband splatter (a gated/chopped
/// tone has sidebands a smooth one doesn't).
fn p2_duc_packet(seq: u32, tx_iq: &Mutex<VecDeque<f32>>) -> ([u8; P2_PACKET_SIZE], bool) {
    let mut p = [0u8; P2_PACKET_SIZE];
    p[0..4].copy_from_slice(&seq.to_be_bytes());

    let mut buf = tx_iq.lock().unwrap();
    let starved = buf.len() < P2_DUC_SAMPLES_PER_FRAME * 2;
    let mut b = 4;
    for _ in 0..P2_DUC_SAMPLES_PER_FRAME {
        let i = buf.pop_front().unwrap_or(0.0);
        let q = buf.pop_front().unwrap_or(0.0);
        p[b..b + 3].copy_from_slice(&pack_24(i));
        p[b + 3..b + 6].copy_from_slice(&pack_24(q));
        b += 6;
    }
    (p, starved)
}

/// Streams DUC IQ to the radio while (and only while) MOX is asserted.
/// Separate from p2_sender_loop's slow ~250ms C&C cadence -- this needs
/// to run continuously at close to real time (~1.25ms/packet at
/// 192ksps with 240 samples/packet) whenever transmitting, the same
/// way p2_receiver_loop's RX IQ ingestion is a separate fast path from
/// the C&C keepalive.
fn p2_tx_iq_loop(
    socket: UdpSocket,
    radio_ip: std::net::IpAddr,
    mox: Arc<AtomicBool>,
    tx_iq: Arc<Mutex<VecDeque<f32>>>,
    stop: Arc<AtomicBool>,
) {
    let mut seq: u32 = 0;
    let interval = Duration::from_secs_f64(P2_DUC_SAMPLES_PER_FRAME as f64 / P2_DUC_RATE_HZ);

    // Diagnostic only -- see p2_duc_packet's doc comment. Aggregated
    // per second (not per packet, which would be ~800/s) so it's cheap
    // to leave in.
    let mut starve_window_start = Instant::now();
    let mut starved_packets_this_window: u32 = 0;
    let mut packets_this_window: u32 = 0;

    // Absolute-deadline pacing, not `thread::sleep(interval)` after
    // every send (an earlier version of this loop did that). The
    // difference matters here specifically: this was confirmed by
    // measuring an actual reported wideband/dirty TX spectrum -- a
    // regular comb of spurs at ~755Hz spacing (pixel-measured from a
    // screenshot against the display's own gridline spacing for
    // calibration) -- against this loop's own packet rate: 240 samples
    // @ 192ksps = exactly 800Hz, an unmistakable match within
    // measurement precision. `thread::sleep(interval)` re-measured
    // fresh each iteration lets whatever scheduling jitter occurred on
    // one send (OS wake-up latency, contention with this process's
    // several other real-time-ish threads -- p2_sender_loop's C&C
    // polling, tx.rs's own TXA loop, MicInput's audio callback, etc.)
    // get permanently baked into that packet's send time rather than
    // corrected on the next one, producing exactly the periodic
    // jitter-at-the-packet-rate signature a comb like that implies.
    // Scheduling against a fixed, monotonically-advancing `next_send`
    // instead means jitter on one packet doesn't compound into the
    // next -- each send is timed relative to the ORIGINAL schedule,
    // not relative to whenever the previous send actually happened.
    let mut next_send = Instant::now();
    // See TX_PREBUFFER_PAIRS's doc comment. Reset false whenever MOX
    // drops so the next key-down re-fills its own cushion from scratch
    // rather than trusting whatever's left over from a previous, now
    // long-idle transmission.
    let mut warmed_up = false;

    while !stop.load(Ordering::Relaxed) {
        if !mox.load(Ordering::Relaxed) {
            // Not transmitting -- nothing to stream. Check back soon
            // so the first DUC packet goes out promptly after PTT.
            thread::sleep(Duration::from_millis(20));
            // Resync so the first packet after PTT goes out immediately
            // against a fresh schedule, not delayed by however long MOX
            // was off (which would otherwise leave `next_send` far in
            // the past, though the `else` branch below would also
            // eventually recover from that -- resetting here is just
            // more direct).
            next_send = Instant::now();
            warmed_up = false;
            continue;
        }

        // *2: tx_iq stores interleaved I/Q floats, not pairs -- same
        // convention as p2_duc_packet's own starved check just below.
        if !warmed_up && tx_iq.lock().unwrap().len() >= TX_PREBUFFER_PAIRS * 2 {
            warmed_up = true;
        }

        let (packet, starved) = if warmed_up {
            p2_duc_packet(seq, &tx_iq)
        } else {
            // Still building the initial cushion -- send silence
            // without touching the queue, so it actually accumulates
            // instead of being drained back down as fast as it fills.
            // Not counted as starved: this is an intentional, one-time
            // ramp-up, not a real underrun.
            let mut p = [0u8; P2_PACKET_SIZE];
            p[0..4].copy_from_slice(&seq.to_be_bytes());
            (p, false)
        };
        if let Err(e) = socket.send_to(&packet, (radio_ip, P2_TX_IQ_PORT)) {
            // Previously silent -- this thread just exited here with no
            // log at all, meaning a single transient send error (a full
            // OS send buffer, a brief network blip) would permanently
            // stop ALL further TX IQ for the rest of the session with
            // zero indication why, indistinguishable from a real
            // dropout with no clue left behind to tell them apart.
            eprintln!("tx: DUC IQ socket.send_to failed, stopping TX IQ streaming: {e}");
            return; // socket closed or radio gone; stop this thread
        }
        if starved {
            starved_packets_this_window += 1;
        }
        packets_this_window += 1;
        if starve_window_start.elapsed() >= Duration::from_secs(1) {
            if starved_packets_this_window > 0 {
                eprintln!(
                    "tx: DUC IQ queue underrun on {starved_packets_this_window}/{packets_this_window} \
                     packets in the last second -- silence spliced into the TX IQ stream during those"
                );
            }
            starve_window_start = Instant::now();
            starved_packets_this_window = 0;
            packets_this_window = 0;
        }
        seq = seq.wrapping_add(1);

        next_send += interval;
        let now = Instant::now();
        if next_send > now {
            thread::sleep(next_send - now);
        } else {
            // Fell behind real time (e.g. a genuine scheduling
            // hiccup) -- resync to now rather than trying to "catch
            // up" by bursting several packets back-to-back, which
            // would be worse than the drift it's correcting for.
            next_send = now;
        }
    }
}
