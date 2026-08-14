← [Manual index](README.md)

# Main Window

![Main window overview](images/02-main-window-overview.png)

## Frequency display and tuning

The large green number near the top is the current VFO frequency
(comma-grouped, in Hz). You can tune it several ways:

- **Scroll** while hovering the frequency display, or the spectrum/waterfall
  panes: steps by 1 kHz per notch. Hold **Shift** while scrolling for 100 Hz
  steps.
- **Ctrl + scroll** (or a pinch/zoom gesture) over the spectrum: steps by
  10 kHz per notch.
- **Click** directly on the spectrum or waterfall: retunes straight to the
  clicked frequency (rounded to the nearest kHz).

### CTUN (Click to Tune)

The **CTUN** button toggles an alternate tuning mode: instead of retuning
the radio's actual hardware oscillator on every click/scroll, the *listen
point* moves within the currently-received passband, and the radio's real
tuned frequency stays fixed. This is useful for quickly browsing around a
band without the radio re-locking/re-settling each time. The CTUN dial is
clamped so the current mode's filter passband always stays fully within the
visible spectrum span.

## Bands and modes

One row of band buttons -- **160m, 80m, 60m, 40m, 30m, 20m, 17m, 15m, 12m,
10m, 6m** -- jumps to that band's remembered frequency, mode, and filter
width (or sensible defaults the first time you visit a band). Each band
remembers its own settings independently as you use the app.

Below that, a row of mode buttons: **LSB, USB, DSB, CWL, CWU, FM, AM, DIGU,
SPEC, DIGL, SAM, DRM**.

Next to the mode row, **Filter width** sets the demodulator passband width
in Hz (50-5000 Hz). Each mode remembers its own last-used width.

## Audio gain

**Audio gain** controls the speaker/headphone volume for the received
audio (this is WDSP's own output gain stage, not your OS/sound-card
volume). It's a log-scale slider, so small movements near the low end make
a bigger audible difference than the same movement near the top.

## Noise/AGC toggles

A row of cycling buttons, each click advancing to the next state:

- **NB** -- cycles Off → NB → NB2 → Off (two mutually-exclusive noise
  blanker stages; the threshold both share is in Settings → RX).
- **NR** -- cycles Off → NR → NR2 → NR3 → NR4 → Off (four mutually-exclusive
  noise reduction algorithms).
- **SNB** -- toggles the Spectral Noise Blanker on/off, independently of NB
  and NR (it can run alongside either).
- **AGC** -- cycles Off → Long → Slow → Medium → Fast → Off. Attack/decay/
  hang/top/slope/threshold for AGC are tuned in Settings → RX.

![Noise and AGC toggle row](images/02-toggle-row.png)

## rigctl / TCI status badges

Small colored badges show whether the rigctl and TCI control servers
(configured in Settings → Network) are running:

- Gray -- not running.
- Green -- listening, no client connected (or a PureSignal-specific state
  for the **PS** badge -- see below).
- Red -- a client is currently connected.

Hovering a badge shows its address and current state in a tooltip.

If PureSignal is enabled for the session, a **PS** badge also appears: gray
while enabled but not yet correcting, green while actively correcting, with
the current feedback level shown in the tooltip.

## Transmit controls

This row only appears once TX is armed (Settings → TX → **Enable
Transmit**):

- **MOX** -- toggles transmit on/off. While active it turns red and reads
  **MOX ON**, and a red **TRANSMITTING** label appears.
- **TUNE** -- transmits a steady test tone, centered in the current filter
  passband, at the reduced **Tune Power %** set in Settings → TX (not full
  TX Power) -- for safely tuning an antenna or amplifier.
- **TWO TONE** -- transmits a two-tone test signal instead of a steady
  tone, also at Tune Power. This is required (not just an alternative) for
  PureSignal calibration -- see [PureSignal](08-puresignal.md).
- **Spacebar** is a hold-to-talk shortcut for MOX, active whenever no text
  field has keyboard focus.

While transmitting, a line beneath the gain controls shows **Mic level**
and **ALC** readouts, and Mic gain / TCI TX gain / TX Power sliders appear
alongside Audio gain.

**Always bench-test into a dummy load at reduced drive before transmitting
into a real antenna.** See the [top-level README](../../README.md) for the
project's current TX verification status.

![Transmit controls active](images/02-tx-active.png)

## Spectrum and waterfall

The spectrum pane shows the live signal trace with a shaded band marking
the current filter passband and a vertical line marking the dial frequency.
The waterfall pane below it shows the same signal scrolling over time,
colored by the selected palette (Settings → Spectrum).

Drag the thin divider between the two panes to adjust how much vertical
space each gets.

## S-meter / power meter

Anchored in the top-right of the window:

- **Receiving**: a classic analog S-meter, S0-S9 in 6 dB steps, with
  +10..+60 over S9 shown in red.
- **Transmitting**: forward/reverse power and SWR, scaled to your
  configured **Max TX Power** (Settings → TX).

Below the meter:

- **Settings...** opens the [Settings window](03-settings-network.md).
- **Add Receiver (n/max)** adds another independent receiver window (see
  [Extra Receivers](11-extra-receivers.md)) -- hidden once you've reached
  the radio's maximum receiver count, replaced with **All N receivers
  active**.

![S-meter](images/02-s-meter.png)

![TX power/SWR meter](images/02-tx-meter.png)

## Stopping

The **Stop** button at the bottom of the window disconnects from the radio
and returns to the discovery window.
