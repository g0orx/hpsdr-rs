# hpsdr-rs

![License: GPLv2+](https://img.shields.io/badge/license-GPLv2%2B-blue.svg)
![Status: early development](https://img.shields.io/badge/status-early%20development-yellow.svg)
![Rust edition](https://img.shields.io/badge/rust-2021-orange.svg)

A Rust/egui desktop client for [openHPSDR](https://openhpsdr.org/) Protocol 1 and Protocol 2 radios (Metis/Ozy-style and Hermes/Orion-style boards), using [WDSP](https://github.com/NR0V/wdsp) for DSP.

> **Status: early / actively in development.** RX has seen the most real-world testing. TX works and has been used for real QSOs, but parts of the TX signal path are still noted in the source as unverified against the official protocol spec on untested hardware. **Always bench-test into a dummy load at reduced drive before transmitting into a real antenna**, especially after pulling a new build.

![hpsdr-rs main window and a second independent receiver, showing the spectrum/waterfall display, S-meter, and TX controls](images/screenshot.png)

## Features

- Protocol 1 (Metis/Ozy) and Protocol 2 (Hermes/Orion) support, with standard openHPSDR UDP discovery (broadcast, port 1024)
- Multiple simultaneous receivers (main receiver + independent "extra receiver" windows), each with its own VFO, mode, filter width, and band memory
- Spectrum/waterfall display with adjustable dB range, palette, and Click-to-Tune (CTUN)
- SSB/CW/AM/FM/digital modes, per-mode/per-band filter width and mode memory
- Noise blanker (NB/NB2), noise reduction (NR/NR2/NR3/NR4), and SNB (spectral noise blanker), independently switchable
- AGC with selectable Off/Long/Slow/Medium/Fast modes
- TX: mic audio through WDSP's TXA chain, ALC, TX power/SWR meter with per-band PA calibration, and a Tune button (WDSP PostGen tone centered in the passband, at a separate reduced "Tune Power" for safe antenna/PA tuning)
- PureSignal (PA linearization/predistortion), on both Protocol 1 and Protocol 2 — see [PureSignal calibration](#puresignal-calibration) below for how to set it up
- rigctl (Hamlib-compatible) and TCI (WebSocket) control servers, for use with WSJT-X and similar digital-mode software
- Per-radio settings persistence (keyed by the radio's MAC address, so multiple physical radios each keep their own saved configuration)

## Supported hardware

Any board the standard openHPSDR discovery protocol reports as one of: Metis, Hermes, Hermes2, Angelia, Orion, Orion2, HermesLite, or HermesLite2 (this covers most Protocol 1/2 hardware, including the ANAN series). The discovery/board-type reply doesn't distinguish specific models or their actual max power (e.g. a 100W vs. 200W ANAN both report as Orion2) — set your radio's actual max TX power in Settings once connected.

## Building

Linux only for now (the config path and a couple of other details are Linux-specific — see the source for notes on what would need to change for Windows/macOS).

Requirements:
- A recent Rust toolchain (`rustup` recommended)
- FFTW3 development headers (e.g. `apt install libfftw3-dev` on Debian/Ubuntu)
- ALSA development headers for audio I/O (e.g. `apt install libasound2-dev`)

WDSP and its noise-reduction dependencies (libspecbleach, rnnoise) are vendored as prebuilt static libraries under `vendor/` and linked automatically by `build.rs` — no separate WDSP build step is needed.

```sh
cargo build --release
```

## Running

```sh
cargo run --release
```

The app opens a discovery window that listens for radios on the network; select one to connect. Settings (frequency, mode, filter width, TX power, calibration, etc.) are saved automatically per-radio under `~/.config/hpsdr-rs/`.

See the **[User Manual](docs/manual/README.md)** for a full walkthrough of the UI -- every settings tab, tuning gestures, extra receivers, and the PureSignal/Diversity/Equalizer features.

## PureSignal calibration

PureSignal is enabled in Settings (takes effect on the next connect), then configured live in Settings → PureSignal while transmitting. The one setting that actually matters — and the thing to change first if calibration won't complete or `Correcting` never turns on — is **HW Peak**, which has to track the *real* envelope peak your radio produces at whatever drive level you're actually calibrating at. It is not a fixed per-board constant to leave alone, and the **Feedback Level** meter's "ideal" 90-256 range is only a rough guide, not a hard requirement — calibration has been confirmed working on real hardware at feedback levels both far below (single digits) and far above (thousands) that range, as long as HW Peak itself is right.

Procedure, on any radio:
1. Set **Tune Power %** low (start around 10-15%) — PureSignal calibration works best at a low real TX drive level, not a normal operating power.
2. Press **Two Tone** (not Tune — a steady tone's constant envelope can never fill PureSignal's calibration buckets) and watch **Measured Peak TX** in the PureSignal panel for a few seconds.
3. Set **HW Peak** to just above whatever Measured Peak TX settled at.
4. Re-engage Two Tone. `Correcting` should turn on within a few seconds. If it doesn't:
   - Stuck with Feedback Level at 0 and no progress: HW Peak is likely still too far from the true peak — recheck Measured Peak TX and adjust again.
   - `Correcting` flickers on/off or never turns on despite Feedback Level being nonzero: try nudging Tune Power % up or down a little and repeat from step 2 — the exact drive level a clean calibration converges at is somewhat radio-dependent.

## Roadmap

Current focus is on stabilizing and testing Protocol 1 and Protocol 2 operation, including PureSignal, across more radios.

## Contributing

Issues and pull requests are welcome. A few things that'll help:

- **For anything touching the TX path**: bench-test into a dummy load at reduced drive before a real antenna, and say in the PR description what you actually tested it against (mode, protocol, hardware). Several TX bugs in this project's history turned out to be protocol- or radio-specific, so mentioning which you used helps a lot.
- **Reference-driven DSP/protocol changes**: where this project's behavior is meant to match a known-working implementation (piHPSDR, rustyHPSDR, Thetis, or the official openHPSDR protocol docs/WDSP source), please check against the actual reference rather than a plausible-sounding guess, and say which reference and where in the PR/commit message. A few real bugs here came from parameter changes that sounded reasonable but turned out not to match how any working reference actually behaves.
- **Comments**: explain *why*, not *what* -- code should already say what it does. A comment earns its place by capturing a non-obvious constraint, a reference confirmation, or the reasoning behind a fix, not by restating the line below it.
- For larger changes, opening an issue first to discuss the approach is appreciated before investing in a big PR.

## License

GNU General Public License, version 2 or (at your option) any later version -- see [LICENSE](LICENSE). This matches WDSP's own license, which hpsdr-rs statically links against.
