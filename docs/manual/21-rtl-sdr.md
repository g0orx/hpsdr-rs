[← RX-888 Mk2](20-rx888-mk2.md) | [Index](README.md) | [MSI.SDR →](22-msi-sdr.md)

# RTL-SDR dongles (receive-only)

> **Confirmed working on a real NooElec NESDR Smart v5** -- HF via direct
> sampling, 2m and 70cm, with correct spectrum direction and tuning
> accuracy. Other R820T/R820T2/R828D dongles should work the same way but
> haven't been tried; reports are very welcome.

hpsdr-rs can use an RTL2832U-based "RTL-SDR" USB dongle with a Rafael
Micro **R820T, R820T2 or R828D** tuner as a receive-only radio -- e.g. the
NooElec NESDR Smart, the RTL-SDR Blog V3 and V4, and most generic DVB-T
sticks. Dongles with the older E4000, FC0012, FC0013 or FC2580 tuners
aren't supported; connecting to one fails with a message saying so.

The driver is built in: it's a Rust port of osmocom's librtlsdr talking
to the dongle directly over USB, so there's no librtlsdr, libusb or
SoapySDR to install.

## One-time setup

1. **Linux only**: install the udev rule for non-root USB access. Copy
   `assets/90-rtlsdr.rules` (from the hpsdr-rs source, or
   `/usr/share/doc/hpsdr-rs/90-rtlsdr.rules` if installed via the `.deb`)
   to `/etc/udev/rules.d/`, then either replug the dongle or run:
   ```sh
   sudo udevadm control --reload-rules && sudo udevadm trigger
   ```
   If you already use other RTL-SDR software, the `rtl-sdr.rules` it
   installed works too. There's no need to blacklist the kernel's
   `dvb_usb_rtl28xxu` DVB-T driver -- hpsdr-rs detaches it itself when it
   opens the dongle.
2. **Windows only**: bind the WinUSB driver to the dongle (usually shown
   as "Bulk-In, Interface (Interface 0)", `0bda:2838`) with
   [Zadig](https://zadig.akeo.ie/) -- the same step every RTL-SDR program
   on Windows needs. With several dongles, do this for **each** one: a
   dongle with no driver shows a "!" in Device Manager and Windows
   doesn't list it to hpsdr-rs at all, so it won't appear in Discover.
3. **macOS**: no extra driver needed (untested).

## Connecting

Plug in the dongle and click **Rediscover** -- it appears in the list as
board **RtlSdr**, with "USB" and the USB port it's plugged into (e.g.
`USB 1-3.2`) in place of an IP address. Select it and click **Start**.

With more than one dongle plugged in, each gets its own row. Many
dongles share the same serial number (often `00000001`), so hpsdr-rs
tells them apart by USB port: each port keeps its own saved settings
(frequency, gain, transverters and so on). Moving a dongle to a
different port makes it look like a new dongle.

## Frequency coverage

- **~24 MHz to 1.766 GHz** through the tuner. Band buttons for **10m,
  6m, 2m, 70cm and 23cm** appear, plus [**Gen**](02-main-window.md#bands-and-modes)
  for anywhere else.
- **Below 24 MHz**, the tuner can't help, so hpsdr-rs switches to
  **direct sampling** automatically whenever you tune below 24 MHz, and
  back to the tuner above it. The antenna is then fed straight into one
  of the RTL2832U's ADC inputs. Settings → RX → **Below 24 MHz** picks
  which one:
  - **Direct sampling (Q)** (the default) suits most dongles, including
    the NESDR Smart.
  - **Direct sampling (I)**: try it if Q is weak.
- An **RTL-SDR Blog V4** has a built-in HF upconverter, so it stays on
  the tuner below 28.8 MHz and the choice isn't shown.

## RF Gain

- **RF Gain** (0-50 dB), on the main window next to AGC Gain, sets the
  R820T2's LNA and mixer gain. The tuner only has fixed steps (0 to
  49.6 dB), so the value snaps to the nearest step at or above what you
  ask for. Turn it down if strong signals cause spurs or a raised noise
  floor. The **RF Attenuation** MIDI control drives this too.
- **Tuner AGC** (Settings → RX) hands gain control to the tuner's own
  AGC instead, and greys out the RF Gain slider.
- Neither has any effect in direct sampling mode, so both are greyed
  out while you're tuned below 24 MHz with direct sampling on.

## Sample rate

The dongle always samples at 1536 kHz; [Settings:
RX](15-settings-rx.md#sample-rate)'s Sample Rate buttons choose how far
hpsdr-rs decimates that: **48, 96, 192, 384, 768 or 1536** kHz (default
192). Changing it briefly stops and restarts streaming.

## Recording raw IQ

**Record raw IQ** in [Settings: RX](15-settings-rx.md#sample-rate), under
the Sample Rate buttons, saves the dongle's whole stream -- all 1536 kHz,
before hpsdr-rs decimates it -- for the number of seconds you set (up to
600), or until you click **Stop**. It's useful for looking at a signal in
other software, or for sending a real off-air signal to a developer.

Recordings are saved in the `recordings` folder next to this radio's
[settings](README.md#conventions-used-in-this-manual) as a
[SigMF](https://sigmf.org) pair:

- `hpsdr-rs_<time>_iq.sigmf-data`: the samples, as interleaved 16-bit I/Q
  (`ci16_le`). That's about 6 MB a second at 1536 kHz.
- `hpsdr-rs_<time>_iq.sigmf-meta`: the sample rate, the time, and the
  centre frequency. With a [transverter](18-xvtr.md) active, this is the
  real RF frequency (e.g. 10491.8 MHz on QO-100). Retuning during a
  recording adds another capture entry, with the sample it starts at.

GNU Radio, inspectrum and SDR++ open these directly. The centre
frequency is where the dongle itself was tuned: 300 kHz above the dial
(see below), except at 1536 kHz. If the computer can't keep up, the
recording skips some of the stream, and the result line under the button
says how many USB reads were lost.

## Centre spike

RTL-SDR dongles produce a spike at whatever frequency the dongle itself is
tuned to. hpsdr-rs tunes the dongle 300 kHz above your dial and shifts the
signal back in software, so the spike falls outside the displayed span at
sample rates up to 384 kHz; a DC blocker removes what's left of it (which
is what covers 768 and 1536 kHz). You shouldn't see a spike at the dial at
any rate.

## Limitations

- **One receiver** -- extra receivers aren't available on this board.
- Receive only: the transmit controls are hidden.
- No frequency (ppm) correction setting yet. The NESDR Smart v5 has a
  0.5 ppm TCXO, so this shouldn't matter for it, but a cheaper dongle may
  read noticeably off frequency.
- The signal level mapping isn't calibrated yet, so the S-meter won't
  match a calibrated receiver.

---

[← RX-888 Mk2](20-rx888-mk2.md) | [Index](README.md) | [MSI.SDR →](22-msi-sdr.md)
