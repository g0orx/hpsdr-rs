[← Firmware Update](13-firmware-update.md) | [Index](README.md)

# Settings: XVTR

Open **Settings...** from the main window, then the **XVTR** tab.

(Screenshot needed: images/14-xvtr-tab.png)

Transverters convert this radio's real tunable range (its **IF**) to some
other operating frequency (**RF**) via an external analog box -- e.g. a 10m
IF of 28-29.7MHz driving a 2m transverter to cover 144-145.7MHz. This tab
lets you define up to 8 of them.

One row per slot, each with:

- **Name** -- anything you like; an empty name marks the slot unused (the
  other fields are ignored until you set one).
- **RF Min/Max** -- the transverter's operating range, in the frequency you
  actually want to see and work on.
- **LO Offset** -- the transverter's own local-oscillator offset, such that
  `RF = IF + LO Offset + LO Error`.
- **LO Error** -- a small trim on top of LO Offset, for whatever the real
  LO measures in practice (kept separate so you don't have to recompute the
  nominal offset every time you touch it).
- **Disable PA** -- when checked, this radio's internal PA and antenna T/R
  relay are left alone while transmitting on this band, so full PA
  drive/relay switching never gets routed into a transverter's low-level IF
  input. This does **not** limit drive level by itself -- keep TX Power low
  regardless; it only stops the internal PA/relay from engaging.

## Using a configured transverter

A configured, in-range slot appears as an extra button alongside the
ordinary [band row](02-main-window.md#bands-and-modes) -- only if its
corresponding IF range actually fits this radio's native tunable range.
Clicking it retunes to the slot's RF minimum. While active:

- VFO-A, the spectrum/waterfall frequency axis, and
  [CAT/rigctl/TCI](03-settings-network.md) frequency reporting all show the
  real RF frequency, even though the radio's actual hardware LO stays at
  the true IF underneath.
- Clicking back to an ordinary band leaves the transverter and returns to
  plain IF display.

## Limitations

- Supports up to ~4.3GHz of RF range (covers roughly 2m through 9cm) -- not
  microwave/QO-100-class transverters.
- Main receiver only -- extra receiver windows and
  [PA Calibration](08-pa-calibration.md) don't know about configured
  transverters.
- This feature has not been verified against real transverter hardware --
  bench-test at low power before relying on it, same as any other TX-path
  change in this project.

---

[← Firmware Update](13-firmware-update.md) | [Index](README.md)
