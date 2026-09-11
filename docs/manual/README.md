# hpsdr-rs User Manual

A guide to using hpsdr-rs, a desktop client for openHPSDR Protocol 1 and
Protocol 2 radios. This manual covers the running application; for building
from source and project status, see the [top-level README](../../README.md).

## Contents

1. [Getting Started](01-getting-started.md) -- installing, launching, discovering and connecting to a radio
2. [Main Window](02-main-window.md) -- tuning, VFO A/B and Split, bands/modes, PTT, spectrum/waterfall, S-meter
3. [Extra Receivers](03-extra-receivers.md) -- adding independent receiver windows
4. [Settings: About](04-about.md) -- connected radio details and author info
5. [Settings: Audio](05-settings-audio.md) -- RX output device, TX input device
6. [Settings: CW](06-settings-cw.md) -- CW Pitch, the radio's internal keyer, and saved CW text messages
7. [Settings: Diversity](07-diversity.md) -- 2-ADC diversity reception
8. [Settings: Equalizer](08-equalizer.md) -- RX/TX graphic EQ
9. [Firmware Update](09-firmware-update.md) -- updating FPGA firmware and changing a radio's IP, in bootloader mode or in-application
10. [Settings: Network](10-settings-network.md) -- rigctl, TCI, and CAT control servers
11. [Settings: Open Collector](11-open-collector.md) -- per-band relay-driver output configuration
12. [Settings: PA Calibration](12-pa-calibration.md) -- per-band power calibration
13. [Settings: PureSignal](13-puresignal.md) -- PA linearization setup and calibration procedure
14. [Settings: RX](14-settings-rx.md) -- sample rate, ADC/antenna, AGC, noise blanker/reduction
15. [Settings: Spectrum](15-settings-spectrum.md) -- display range, waterfall palette
16. [Settings: TX](16-settings-tx.md) -- TX power, mic source, safety notes
17. [Settings: XVTR](17-xvtr.md) -- defining transverters (IF-to-RF band conversion)
18. [Ozy USB](18-ozy-usb.md) -- connecting the original Ozy/Mercury/Penny hardware over USB (new, unconfirmed)

**PDF**: this whole manual is also available as a single PDF, useful for
offline/printed reading. It's built automatically by the "Manual PDF"
GitHub Actions workflow on every change here -- grab it from that
workflow's most recent run (Actions tab -> Manual PDF -> latest run ->
Artifacts). To build it yourself instead: `./scripts/build-manual-pdf.sh`
(needs `pandoc` plus a LaTeX toolchain -- see the script's own doc
comment for exact package names).

Every page notes where a screenshot would help; those are marked
`(Screenshot needed: ...)` with a broken image link pointing at
`images/<page>-<name>.png`. Drop a matching PNG into
[`docs/manual/images/`](images/) and it'll start rendering there with no
further edits needed.

## Conventions used in this manual

- **Bold** names match the exact label text shown in the app.
- Sliders in this app respond to mouse-wheel scroll while hovered, not just
  dragging -- every slider mentioned in this manual can be adjusted either way.
- Settings are saved automatically per radio (keyed by its MAC address) under
  `~/.config/hpsdr-rs/` (Linux), `%APPDATA%\hpsdr-rs\` (Windows), or
  `~/Library/Application Support/hpsdr-rs/` (macOS) -- there's no manual
  "Save" step anywhere in the app.
