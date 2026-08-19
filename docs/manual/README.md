# hpsdr-rs User Manual

A guide to using hpsdr-rs, a desktop client for openHPSDR Protocol 1 and
Protocol 2 radios. This manual covers the running application; for building
from source and project status, see the [top-level README](../../README.md).

## Contents

1. [Getting Started](01-getting-started.md) -- installing, launching, discovering and connecting to a radio
2. [Main Window](02-main-window.md) -- tuning, bands/modes, PTT, spectrum/waterfall, S-meter
3. [Settings: Network](03-settings-network.md) -- rigctl and TCI control servers
4. [Settings: RX](04-settings-rx.md) -- sample rate, ADC/antenna, AGC, noise blanker/reduction
5. [Settings: Spectrum](05-settings-spectrum.md) -- display range, waterfall palette
6. [Settings: TX](06-settings-tx.md) -- TX power, mic source, safety notes
7. [Settings: PA Calibration](07-pa-calibration.md) -- per-band power calibration
8. [Settings: PureSignal](08-puresignal.md) -- PA linearization setup and calibration procedure
9. [Settings: Diversity](09-diversity.md) -- 2-ADC diversity reception
10. [Settings: Equalizer](10-equalizer.md) -- RX/TX graphic EQ
11. [Extra Receivers](11-extra-receivers.md) -- adding independent receiver windows

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
