[← Equalizer](11-equalizer.md) | [Index](README.md) | [Firmware Update →](13-firmware-update.md)

# Extra Receivers

hpsdr-rs supports multiple simultaneous, independent receivers on radios
that report supporting more than one (Protocol 2 boards, generally). Each
extra receiver gets its own window with its own VFO, mode, filter width,
spectrum/waterfall, and settings -- entirely separate from the main
receiver and from each other.

## Adding a receiver

Click **Add Receiver (n/max)**, below the S-meter on the main window. A new
window titled **Receiver N** opens. This button disappears (replaced by
**All N receivers active**) once you've reached the radio's maximum
receiver count.

On a [PureSignal](09-puresignal.md)-capable radio, "max" here is 2 lower
than the radio's full receiver capacity -- PureSignal's two feedback
receivers are reserved for the whole session as soon as you connect,
whether or not PureSignal is actually enabled.

![An extra receiver's own window](images/11-extra-receiver-window.png)

## Using an extra receiver window

It behaves like a scaled-down version of the main window: frequency
display, band and mode rows, **Filter width** and **Audio gain** sliders,
the CTUN/NB/NR/SNB/ANF/BIN/AGC/AGC Gain/RIT toggle row (unlike the main
window, CTUN stays in this same row here rather than moving into its own
spot -- extra receivers have no A<>B/Split box to sit below) -- RIT here
works the same as the
main window's own (see [Transmit controls](02-main-window.md#transmit-controls)),
just always visible rather than tucked into a TX-only row, since it's a
receive-only feature and this window has no XIT (extra receivers never
transmit) -- its own spectrum and waterfall panes with their own
[Zoom/Pan sliders](02-main-window.md#zoom-and-pan) below
the waterfall (click or scroll to tune, same conventions as the main
window, though Ctrl+scroll's 10kHz-step tuning gesture isn't available
here), and its own S-meter with a **Settings...** button.

## Extra receiver Settings

Each extra receiver's **Settings...** opens its own settings window with
three tabs -- **RX**, **Spectrum**, **EQ** -- a subset of the main window's
tabs, since things like Network, Audio (main output only), TX, PA
Calibration, PureSignal, and Diversity are session-wide, not per-receiver:

- **RX** -- sample rate (Protocol 1 boards follow the main receiver's
  rate, since P1 has one shared clock rather than per-receiver rates),
  ADC/antenna selection, the same AGC attack/decay/hang/top/slope/thresh
  and NB threshold controls as the main window's RX tab, and its own
  independent **Output device** picker (same device list and fallback
  behavior as the main window's [Audio tab](04-settings-audio.md)) -- so
  this receiver can play to a different device than the main receiver.
- **Spectrum** -- display range and waterfall palette, same as the main
  window's Spectrum tab (no separate TX range here, since extra receivers
  never transmit).
- **EQ** -- its own independent [graphic equalizer](11-equalizer.md),
  RX-only.

![Extra receiver settings window](images/11-extra-receiver-settings.png)

Every extra receiver's settings persist per radio, the same as the main
window's.

---

[← Equalizer](11-equalizer.md) | [Index](README.md) | [Firmware Update →](13-firmware-update.md)
