# hpsdr-rs remote protocol

How `hpsdr-remote` (the thin client) talks to hpsdr-rs. It is TCI --
[Expert Electronics' published protocol](https://github.com/ExpertSDR3/TCI),
which hpsdr-rs already serves on port 50001 -- plus a few extensions for
running over slow links. Standard TCI clients (WSJT-X, TCI Remote) never
send the extension commands, so they never see the extension streams.

Status: **phase 2** (receive and transmit; HTTPS and a password through
hpsdr-rs's web server). The shared encoding lives in the `hpsdr-proto`
crate, used by both ends.

## Connecting

Either straight to the TCI server (`ws://<host>:50001`), or through
hpsdr-rs's web server (Settings -> Network -> Web client) at
`wss://<host>:<port>/tci` -- the same port that serves the web client.
That path passes the connection through to the TCI server unchanged, so
everything below is the same either way. With a password set, it's given
as `/tci?password=<%-encoded password>`; a wrong or missing one gets HTTP
401 before the WebSocket opens. Plain TCI has no password: for use over
the internet, make only the web server's port reachable, and the TCI
server can listen on `127.0.0.1` only.

## Why extensions

Standard TCI streams audio and IQ as uncompressed 32-bit floats, and has no
spectrum message -- a client draws its own spectrum from the IQ stream. RX
audio alone is ~3 Mbit/s and IQ for a 192 kHz panorama ~12 Mbit/s. The
extensions send what the radio end has already computed instead: a
finished spectrum row and compressed audio, ~150 kbit/s together.

## Control

Plain TCI text commands (`name:arg,arg;`) for everything TCI already has:
`vfo`, `modulation`, `trx`, `rit_*`, `xit_*`, `rx_nb_*`, `rx_nr_*`,
`rx_anf_enable`, `rx_bin_enable`, and `rx_filter_band:0,low,high;` (Hz,
relative to the carrier). The server repeats `vfo`, `dds`, `if`,
`modulation` and `trx` every second, so the client follows changes made
anywhere else.

## Extension commands

All start `hpsdr_`.

- `hpsdr_hello:<version>;` -- client to server, first. The server answers
  `hpsdr_hello:<version>,<feature>,<feature>...;` with its own protocol
  version (1) and what it offers: `spectrum`, `adpcm`, `tx`, `cw`, `key`. A server
  without the extensions ignores the command; the client then has plain
  TCI only.
- `hpsdr_notice:<text>;` -- server to client: something to tell the
  operator, such as why it didn't key.
- `hpsdr_spectrum:<points>,<fps>;` -- start the spectrum stream: `points`
  per row (64-4096), at most `fps` rows a second (1-30). `hpsdr_spectrum:0,0;`
  stops it.
- `hpsdr_state:<width>,<agc>,<agc_gain>,<nb>,<nr>,<anf>,<snb>;` -- server
  to client, after `hpsdr_hello`: the receiver's DSP settings, every second
  and straight after any change (from the client, the radio, or anyone):
  filter width in Hz; AGC `off`/`long`/`slow`/`medium`/`fast`; AGC gain in
  dB; noise blanker `off`/`nb`/`nb2`; noise reduction `off`/`nr`/`nr2`/`nr3`
  (`nr3` is WDSP's NNR, shown as NNR on the radio);
  ANF and SNB `true`/`false`. These are the extensions' own rather than TCI's
  filter and NB/NR commands, whose filter edges and stage numbers don't map
  cleanly onto hpsdr-rs.
- `hpsdr_set:<name>,<value>;` -- client to server: change one of those:
  `width`, `agc`, `agc_gain`, `nb`, `nr`, `anf` or `snb`, with values as in
  `hpsdr_state`. Unknown names or values are ignored.
- `hpsdr_bands:<name>,<low>,<high>,...;` -- server to client, after
  `hpsdr_hello` and whenever it changes: the radio's band buttons in its own
  order, each with its RF edges in Hz -- the standard bands the radio can
  tune, `Gen` (its whole range) and each transverter it can reach. Names have
  TCI's reserved characters made spaces.
- `hpsdr_band:<name>;` -- client to server: select that band, exactly as its
  button on the radio does (remembered frequency, mode and filter; for a
  transverter, the transverter too).
- `hpsdr_audio:<codec>,<rate>;` -- start compressed RX audio: `adpcm` at
  `rate` samples a second (8000, 12000, 16000, 24000 or 48000). `hpsdr_audio:off,0;`
  stops it. A client using this should not also send `audio_start`.

## Transmitting

- `hpsdr_mox:<true|false>;` -- client to server: key or unkey. The server
  keys only where the radio itself would -- in band, unless out-of-band TX
  is allowed -- and not in CW (see below), answering `hpsdr_notice` with
  the reason if it doesn't. Any client can unkey.
- TX audio -- client to server, while keyed: binary type 101 (the same
  IMA-ADPCM blocks as RX audio, mono), 20 ms each, at the rate in the
  header (the client uses 16000). The server feeds it to the transmitter
  in place of the radio's microphone, keeping ~100 ms in hand against
  network jitter.
- **A keyed client that goes away, or sends no TX audio for one second,
  is unkeyed** -- a dropped connection or a phone put to sleep can't leave
  the radio transmitting.
- `hpsdr_tx:<available>,<mox>,<power>,<max>,<forward>,<swr>;` -- server to
  client after `hpsdr_hello`: whether this radio can transmit at all (also
  false while remote transmit is turned off at the radio -- turning it off
  unkeys a remote client that's transmitting),
  keyed or not, the TX power setting and its maximum (W), and the
  measured forward power (W) and SWR -- five times a second while
  transmitting, once a second otherwise, and at once when they change.
- `hpsdr_set:power,<watts>;` -- client to server: the TX power setting, as
  the radio's own TX Power slider.

### CW

CW is sent as text, with TCI's own commands (as Thetis and deskHPSDR
implement them), so any TCI client can use them, not only hpsdr-remote.
TCI text can't contain `:` `,` `;`, so they travel as `^` `~` `*`.

- `cw_macros:<rx>,<text>;` -- client to server: send this text as CW at the
  radio's CW speed (Settings -> CW), after anything already being sent. The
  server keys and unkeys the radio itself, exactly as for its own Send CW
  button or CAT's `KY`: only in CW, in band, and not while it's otherwise
  transmitting. Text it can't send then is dropped, not kept for later --
  with the reason in `hpsdr_notice` to a client that said `hpsdr_hello`
  (which is also held by the radio's **Allow transmit** setting).
- `cw_macros_stop;` -- client to server: stop at once, dropping the rest.
- `cw_macros_speed:<wpm>;` -- either way: the CW speed, 1-60 words a
  minute -- sent to every client at first and when it changes.
- `cw_macros_empty:<rx>;` -- server to client: everything has been sent.
- `hpsdr_cw_messages:<text>,<text>,...;` -- server to client, after
  `hpsdr_hello` and whenever they change: the radio's saved CW messages
  (Settings -> CW), encoded as above, an empty one as nothing.
- `hpsdr_paddle_keys:<dit>,<dah>;` -- server to client, after
  `hpsdr_hello` and whenever they change: the keys a key or paddle
  plugged into the client as a keyboard holds down (Settings -> CW,
  Remote Paddle Keys), one character each, encoded as above -- a
  lower-case letter, a digit or one of `` [ ] ; ' , . / \ - = ` ``, all
  typed without Shift. Matched by where they are on a US keyboard.
  Without it a client uses `[` and `]`.

Live keying, from a key or paddles at the client:

- `hpsdr_key:<time>,<down>,<delay>;` -- client to server: the key went down
  (`true`) or up at `time`, in ms on the client's own clock. The first
  key-down of a burst keys the radio -- under the same conditions as CW
  text, and not while it's otherwise transmitting -- and is played `delay`
  ms (50-1000) after it arrives; every later transition is played at the
  same distance from the first as on the client's clock. So the keying's
  timing, spaces included, comes through unchanged as long as the
  network's jitter stays under `delay`; one arriving too late is played at
  once. The radio unkeys when the key has been up for the CW hang time
  (Settings -> CW, Break-in delay). A paddle keyer runs in the client, and
  sends the times its elements should have, not when the screen saw them.
- **While the key is down the client repeats it every 500 ms; after
  1.5 s without a word, or if the client goes, the key is released and the
  radio unkeyed.**
- `keyer:<rx>,<true|false>[,<ms>];` -- TCI's own, as Thetis takes it, for
  other TCI programs: down as it arrives, up `ms` after it went down (or as
  it arrives), played 100 ms later.

## Binary streams

Both use TCI's 64-byte little-endian header (as hpsdr-rs's standard
streams do):

| Offset | Field | Spectrum (type 100) | Audio (type 101) |
|---|---|---|---|
| 0 | receiver | 0 | 0 |
| 4 | sample rate | rows/s requested | audio rate |
| 8 | format | 0x100 (u8 points) | 0x101 (ADPCM) |
| 12 | codec | 0 | 1 (IMA-ADPCM) |
| 16 | crc | 0 | 0 |
| 20 | length | points | samples |
| 24 | type | 100 | 101 |
| 28 | channels | 1 | 1 |
| 32 | reserved | 0 | 0 |

### Spectrum (type 100) payload

| Size | Field |
|---|---|
| u64 | centre frequency, Hz (RF, as displayed) |
| u32 | span, Hz |
| f32 | dB of point value 0 |
| f32 | dB per point step |
| f32 | S-meter, dBm |
| u8 x points | the row, low to high frequency: dB = base + value x step |

The server sends its main receiver's display row, resampled to `points`,
so in phase 1 the view follows the radio's own zoom and pan.

### Audio (type 101) payload

One self-contained IMA-ADPCM block (the standard IMA step table), mono:

| Size | Field |
|---|---|
| i16 | first sample, also the predictor's start |
| u8 | step index (0-88) |
| u8 | 0 |
| u8 x ceil((samples - 1) / 2) | 4-bit codes, low nibble first |

Each message decodes on its own, so a lost one costs only its own audio.
