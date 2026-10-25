[← MSI.SDR](22-msi-sdr.md) | [Index](README.md)

# DATV (DVB-S2 receive)

> **New, and not yet confirmed off-air.** The receiver decodes video
> end to end from synthetic test signals, and finds and identifies the
> real QO-100 wideband beacon's frames. A full off-air decode hasn't
> been confirmed yet.

hpsdr-rs can receive **DVB-S2** digital TV, the format almost all
amateur DATV uses, including QO-100's wideband transponder. It
demodulates and decodes the signal itself. The resulting MPEG transport
stream (the video and sound) goes out over the network to a video
player such as **VLC** or **ffplay**.

DATV works with an [RTL-SDR](21-rtl-sdr.md) or an
[MSI.SDR](22-msi-sdr.md), because it uses the dongle's whole raw stream
rather than the receiver's own span. Click **DATV**, next to CTUN in the
main window, to open the DATV window. The button only appears with one
of those dongles connected.

## How wide a signal fits

| Dongle | Stream | Widest signal |
|---|---|---|
| RTL-SDR | 1536 kHz | about 1000 kS/s (roll-off 0.35) |
| MSI.SDR | 1536 kHz | about 1000 kS/s |
| MSI.SDR with [Wide stream](22-msi-sdr.md#wide-stream) | 4608 kHz | about 2000 kS/s |

QO-100's wideband beacon is 1500 kS/s, so it needs an MSI.SDR with
**Wide stream** on. Most QO-100 stations use 333 to 1000 kS/s.

## Receiving QO-100's wideband transponder

The wideband transponder's downlink is **horizontally** polarised, so
set your LNB for horizontal (18 V on most LNBs; 13 V selects vertical,
the narrowband transponder).

1. Set up the [QO-100 transverter](18-xvtr.md#example-qo-100-with-an-lnb)
   with an RF range that covers the wideband transponder, e.g. RF Min
   10491000000 and RF Max 10499500000, and select it.
2. On an MSI.SDR, turn on Settings → RX → **Wide stream**, and
   reconnect.
3. Tune so the signal you want is in the received span. The dongle sits
   300 kHz above the dial, so put the dial about 300 kHz below the
   signal to centre it in the dongle's stream.
4. Click **DATV**. Set the **Frequency** (RF) and **Symbol rate** of the
   signal you want, or click **Beacon** for the wideband beacon
   (10491.500 MHz, 1500 kS/s). **VFO A** copies the dial's frequency.
5. Click **Start**, then open the stream in a player:
   - VLC: `vlc udp://@:1234`
   - ffplay: `ffplay udp://127.0.0.1:1234`

## The DATV window

- **Frequency**: the signal's centre, in the frequency shown on the dial
  (RF with a transverter active).
- **Symbol rate**: presets from 125 to 2000 kS/s, or any value. It must
  be the signal's actual rate. The receiver tracks small clock errors
  but doesn't search for the rate.
- **Roll-off**: the transmitter's filter roll-off (0.35, 0.25 or 0.20).
  0.35 works for all of them, with a little loss on the others.
- **Send to**: where the transport stream goes, as `host:port` (default
  `127.0.0.1:1234`). Send it to another machine's address to watch
  there.
- **Start** / **Stop**. Closing the window also stops it. DATV and
  [Record raw IQ](21-rtl-sdr.md#recording-raw-iq) can't run at the same
  time.

While running, the window shows:

- **Signal**: **Locked** once frames are being found where they should
  be, otherwise **Searching**.
- **Mode**: the signal's modulation and code rate (e.g. QPSK 4/5),
  frame size, and whether it sends pilots.
- **SNR**: measured on the received symbols.
- **Carrier**: how far the signal is from the frequency you set. The
  receiver follows offsets of up to a quarter of the symbol rate, so LNB
  drift is taken care of. "(spectrum inverted)" means the signal arrived
  mirrored and is being turned the right way up.
- **Frames**: decoded and failed. Failures while locked mean the signal
  is too weak (or too strong: check for clipping).
- **Transport stream**: its bit rate, and the packets sent.
- **Dropped**: only shown if the computer isn't keeping up.

## What's supported

- DVB-S2 QPSK and 8PSK, every code rate, normal and short frames, with
  or without pilots. Constant coding (one MODCOD at a time), as amateur
  DATV uses.
- Not yet: 16APSK and 32APSK, DVB-S (the older standard some stations
  still use), multiple input streams, and showing the video inside
  hpsdr-rs.
- 8PSK without pilots and QPSK at the bottom of its range (below about
  4 dB SNR) can lose frames when the carrier tracking slips. Pilots fix
  this for 8PSK.

The receiver is written from the DVB-S2 standard (ETSI EN 302 307-1) for
this project, rather than ported from another DATV receiver.

---

[← MSI.SDR](22-msi-sdr.md) | [Index](README.md)
