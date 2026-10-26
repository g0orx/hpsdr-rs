[← MSI.SDR](22-msi-sdr.md) | [Index](README.md) | [Pluto →](24-pluto.md)

# DATV (DVB-S2 receive)

> **Confirmed off-air**: with an [ADALM-Pluto](24-pluto.md) on QO-100,
> both the wideband beacon and a 500 kS/s station decoded to watchable
> video in VLC. Not yet tried off-air with an RTL-SDR or MSI.SDR.

hpsdr-rs can receive **DVB-S2** digital TV, the format almost all
amateur DATV uses, including QO-100's wideband transponder. It
demodulates and decodes the signal itself. The resulting MPEG transport
stream (the video and sound) goes out over the network to a video
player such as **VLC** or **ffplay**.

DATV works with an [RTL-SDR](21-rtl-sdr.md), an [MSI.SDR](22-msi-sdr.md)
or a [Pluto](24-pluto.md), because it uses the radio's whole raw stream
rather than the receiver's own span. Click **DATV**, next to CTUN in the
main window, to open the DATV window. The button only appears with one
of those connected.

## How wide a signal fits

| Radio | Stream | Widest signal |
|---|---|---|
| RTL-SDR | 1536 kHz | about 1000 kS/s (roll-off 0.35) |
| MSI.SDR or Pluto | 1536 kHz | about 1000 kS/s |
| MSI.SDR or Pluto with **Wide stream** ([MSI.SDR](22-msi-sdr.md#wide-stream), [Pluto](24-pluto.md#wide-stream)) | 4608 kHz | about 2000 kS/s |

QO-100's wideband beacon is 1500 kS/s, so it needs **Wide stream**.
Most QO-100 stations use 333 to 1000 kS/s.

## Receiving QO-100's wideband transponder

The wideband transponder's downlink is **horizontally** polarised, so
set your LNB for horizontal (18 V on most LNBs; 13 V selects vertical,
the narrowband transponder).

1. Set up the [QO-100 transverter](18-xvtr.md#example-qo-100-with-an-lnb)
   with an RF range that covers the wideband transponder, e.g. RF Min
   10491000000 and RF Max 10499500000, and select it.
2. Turn on Settings → RX → **Wide stream**, and reconnect.
3. Click **DATV**. Pick a signal: click it in the
   [Transponder view](#transponder-view) (Pluto), or set its
   **Frequency** (RF) and **Symbol rate**, or click **Beacon** for the
   wideband beacon.
4. Click **Start**, then open the stream in a player:
   - VLC: `vlc udp://@:1234`
   - ffplay: `ffplay udp://127.0.0.1:1234`

If the signal isn't inside the receiver's stream when you click
**Start**, VFO A moves onto the DATV frequency first, so you don't have
to tune there yourself.

## Transponder view

With a [Pluto](24-pluto.md), **Transponder view** at the top of the
DATV window shows all of QO-100's wideband transponder, 10491-10499.5
MHz, at once. Hover over it to read a frequency. The orange band marks
where DATV is set.

![Transponder view: the wideband beacon (left, marked in orange) and
four stations](images/23-datv-transponder-view.png)

**Click a signal** to receive it. The frequency is set to the signal's
centre, and the symbol rate is estimated from its width and snapped to
the nearest of 125, 250, 333, 500, 1000, 1500 or 2000 kS/s. If the width
doesn't match any of those, only the frequency changes, so set the rate
yourself. Clicking empty spectrum just sets that frequency.

The transponder is wider than any stream the receiver can use, so while
the view is shown the Pluto runs at 12.288 MS/s, faster than its USB
link can carry. Some data is lost, but the spectrum doesn't need all of
it. **The main receiver is paused while the view is shown** (no audio,
and the main spectrum stops). Clicking **Start**, clicking **Transponder
view** again, or closing the DATV window puts the Pluto back to normal.

The view needs the QO-100 transverter selected, so that it knows where
the transponder is in the Pluto's IF. It isn't available with an
RTL-SDR or MSI.SDR.

## The DATV window

- **Frequency**: the signal's centre, in the frequency shown on the dial
  (RF with a transverter active).
- **Symbol rate**: presets from 125 to 2000 kS/s, or any value. It must
  be the signal's actual rate. The receiver tracks small clock errors
  but doesn't search for the rate.
- **Roll-off**: the transmitter's filter roll-off (0.35, 0.25 or 0.20).
  Use the signal's own: a mismatch costs decoding margin. The wideband
  beacon uses **0.20**, which **Beacon** sets.
- **Send to**: where the transport stream goes, as `host:port` (default
  `127.0.0.1:1234`). Send it to another machine's address to watch
  there.
- **Start** / **Stop**. Closing the window also stops it. DATV and
  [Record raw IQ](21-rtl-sdr.md#recording-raw-iq) can't run at the same
  time.

While running, the window shows:

- **Signal**: **Locked** once frames are being found where they should
  be, otherwise **Searching**. If a lock turns out to be false (headers
  that don't keep matching), the receiver drops it and searches again.
- **Mode**: the signal's modulation and code rate (e.g. QPSK 4/5),
  frame size, and whether it sends pilots.
- **SNR**: Es/N0, measured on the received symbols.
- **Carrier**: how far the signal is from the frequency you set. The
  receiver follows offsets of up to a quarter of the symbol rate, so LNB
  drift is taken care of. "(spectrum inverted)" means the signal arrived
  mirrored and is being turned the right way up. While **Searching** it
  can flip back and forth as the receiver tries both ways.
- **Frames**: decoded and failed. Failures while locked mean the signal
  is too weak (or too strong: check for clipping).
- **Transport stream**: its bit rate, and the packets sent.
- **Dropped**: only shown if the computer isn't keeping up.

## How much signal is needed

Compare the **SNR** with what the signal's mode needs. Roughly, for
error-free decoding:

| Mode | SNR needed |
|---|---|
| QPSK 1/2 | 1.0 dB |
| QPSK 3/5 | 2.2 dB |
| QPSK 2/3 | 3.1 dB |
| QPSK 3/4 | 4.0 dB |
| QPSK 4/5 | 4.7 dB (the wideband beacon) |
| QPSK 5/6 | 5.2 dB |
| 8PSK 2/3 | 6.6 dB |
| 8PSK 3/4 | 7.9 dB |

These are the DVB-S2 standard's figures (ETSI EN 302 307-1). In
practice hpsdr-rs needs more: from a few tenths of a dB on a clean test
signal up to about 1 dB on real stations. For example, a QPSK 3/5
station at 3.3 dB lost about one frame in ten, which still gave a
watchable picture. Close to the figure most frames fail, and below it
nothing decodes. Since every 0.1 dB counts there, dish pointing and LNB
skew are worth getting right.

## What's supported

- DVB-S2 QPSK and 8PSK, every code rate, normal and short frames, with
  or without pilots. Constant coding (one MODCOD at a time), as amateur
  DATV uses.
- Not yet: 16APSK and 32APSK, DVB-S (the older standard some stations
  still use), and multiple input streams.
- The video is shown by an external player (VLC, ffplay), not inside
  hpsdr-rs.
- Decoding uses several CPU cores. A frame that can't be decoded is
  given up on early, so a signal that's too weak doesn't tie the
  computer up.

The receiver is written from the DVB-S2 standard (ETSI EN 302 307-1) for
this project, rather than ported from another DATV receiver.

---

[← MSI.SDR](22-msi-sdr.md) | [Index](README.md) | [Pluto →](24-pluto.md)
