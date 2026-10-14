[← RTL-SDR](21-rtl-sdr.md) | [Index](README.md)

# MSI.SDR / MSi2500 sticks (receive-only)

> **Confirmed working on a real MSI.SDR stick** (1df7:2500). A genuine
> SDRplay RSP1 should work the same way but hasn't been tried; reports
> are very welcome.

hpsdr-rs can use a receiver built on the Mirics **MSi2500** USB interface
and **MSi001** tuner as a receive-only radio. This covers the cheap
"MSI.SDR" sticks sold as covering 10 kHz to 2 GHz, and the same chip
pair is used in the original SDRplay RSP1. Both enumerate as USB
`1df7:2500` (Linux's `lsusb` names it "SDRplay RSP1" either way).

The driver is built in. It's a Rust port of libmirisdr-4 that talks to the
stick directly over USB, so there's no libmirisdr, SDRplay API, libusb
or SoapySDR to install.

## One-time setup

1. **Linux only**: install the udev rule for non-root USB access. Copy
   `assets/90-mirisdr.rules` (from the hpsdr-rs source, or
   `/usr/share/doc/hpsdr-rs/90-mirisdr.rules` if installed via the
   `.deb`) to `/etc/udev/rules.d/`, then either replug the stick or run:
   ```sh
   sudo udevadm control --reload-rules && sudo udevadm trigger
   ```
   If libmirisdr is already installed, its `60-libmirisdr4.rules` works
   too. The kernel's `msi2500`/`msi001` drivers don't need
   blacklisting, because hpsdr-rs detaches them itself when it opens the
   stick.
2. **Windows only**: use [Zadig](https://zadig.akeo.ie/) to bind the WinUSB
   driver to the stick (`1df7:2500`). This is untested.
3. **macOS**: no extra driver needed (untested).

## Connecting

Plug in the stick and click **Rediscover**. It appears in the list as
board **MsiSdr**, with "USB" in place of an IP address. Select it and
click **Start**.

## Frequency coverage

The MSi001 covers **HF through ~2 GHz** natively. Below 50 MHz its
built-in upconverter is used, so there's no direct-sampling mode to set
up. Every HF band button appears, plus **2m, 70cm and 23cm**, and
[**Gen**](02-main-window.md#bands-and-modes) reaches anywhere else.

## Settings → RX

- **RF Gain** (0-102 dB) sets the MSi001's total gain. It's split across
  the LNA (24 dB), mixer (19 dB) and baseband (0-59 dB) stages
  the same way libmirisdr does it. The default is 43 dB. Near the top of the range,
  the ADC clips on any real antenna, so turn it down if strong signals
  cause spurs or a raised noise floor. The **RF Attenuation** MIDI
  control drives this too.

## Sample rate

The stick always samples at 1536 kHz. As with the RTL-SDR, [Settings:
RX](15-settings-rx.md#sample-rate)'s Sample Rate buttons choose how far
hpsdr-rs decimates that: **48, 96, 192, 384, 768 or 1536** kHz (default
192). The MSi001's IF filter is set to 1.536 MHz to match. Changing the
rate briefly stops and restarts streaming.

## Centre spike

The MSi001 has a sizeable DC offset at whatever frequency it is tuned
to. hpsdr-rs handles it the same way as on the
[RTL-SDR](21-rtl-sdr.md#centre-spike): the stick is tuned 300 kHz above
the dial, and a DC blocker removes what's left.

## Limitations

- **One receiver**: extra receivers aren't available on this board.
- Receive only: the transmit controls are hidden.
- The band-select table is the one libmirisdr uses for generic sticks.
  A genuine SDRplay RSP1 has its own front-end filters, which may want
  SDRplay's table instead. It should still receive, but possibly with
  less filtering.
- SDRplay's later models (RSP1A, RSP2 and so on) are listed by libmirisdr and
  detected here, but they use different hardware and almost certainly
  won't work.
- No bias-T control and no frequency (ppm) correction yet.
- The signal level mapping isn't calibrated yet, so the S-meter won't
  match a calibrated receiver.

---

[← RTL-SDR](21-rtl-sdr.md) | [Index](README.md)
