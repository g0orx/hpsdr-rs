[← XVTR](14-xvtr.md) | [Index](README.md) | [About →](16-about.md)

# Settings: Open Collector

Open **Settings...** from the main window, then the **Open Collector** tab.

![Open Collector settings tab](images/15-open-collector-tab.png)

Open Collector outputs (**OC1**-**OC7**) are general-purpose relay-driver
lines present on most HPSDR boards -- used for things like external antenna
switching, bandpass filter selection, or amplifier keying. This tab lets you
configure which outputs are active on each band, separately for receive and
transmit.

One row per band, plus a row per configured [XVTR](14-xvtr.md) slot with a
non-empty name:

- **Rx** -- which of OC1-OC7 are active while receiving on that band.
- **Tx** -- which are active while transmitting on that band.
- **Tune** (its own row at the bottom) -- a single set of outputs, not tied
  to any one band, that get OR'd into the current band's **Tx** outputs
  whenever the Tune button is engaged.

Unchecked means that output is off. There's no "leave alone" state -- every
packet sent to the radio declares the full set of outputs that should be
active right now.

## Which band applies

Whichever band the primary receiver's real hardware frequency (or, if a
transverter is active, its configured RF range) currently falls in --
shared across every receiver, the same way antenna and PA gain selection
already are, since there's only one physical front end. Extra receiver
windows don't have their own Open Collector settings.

## Limitations

- This feature has not been verified against real Open-Collector-driven
  relays/filters -- check with a meter or by ear (relay click) before
  relying on it for anything that could be damaged by the wrong filter path
  being selected, same as any other new hardware-control feature in this
  project.

---

[← XVTR](14-xvtr.md) | [Index](README.md) | [About →](16-about.md)
