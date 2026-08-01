# hpsdr-rs

A Rust/egui desktop client for [openHPSDR](https://openhpsdr.org/) Protocol 1 and Protocol 2 radios (Metis/Ozy-style and Hermes/Orion-style boards), using [WDSP](https://github.com/NR0V/wdsp) for DSP.

> **Status: early / actively in development.** RX has seen the most real-world testing. TX works and has been used for real QSOs, but parts of the TX signal path are still noted in the source as unverified against the official protocol spec on untested hardware. **Always bench-test into a dummy load at reduced drive before transmitting into a real antenna**, especially after pulling a new build.

## Features

- Protocol 1 (Metis/Ozy) and Protocol 2 (Hermes/Orion) support, with standard openHPSDR UDP discovery (broadcast, port 1024)
- Multiple simultaneous receivers (main receiver + independent "extra receiver" windows), each with its own VFO, mode, filter width, and band memory
- Spectrum/waterfall display with adjustable dB range, palette, and Click-to-Tune (CTUN)
- SSB/CW/AM/FM/digital modes, per-mode/per-band filter width and mode memory
- Noise blanker (NB/NB2), noise reduction (NR/NR2/NR3/NR4), and SNB (spectral noise blanker), independently switchable
- AGC with selectable Off/Long/Slow/Medium/Fast modes
- TX: mic audio through WDSP's TXA chain, ALC, TX power/SWR meter with per-band PA calibration, and a Tune button (WDSP PostGen tone centered in the passband, at a separate reduced "Tune Power" for safe antenna/PA tuning)
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

## Roadmap

PureSignal (PA linearization/predistortion) is planned but not yet implemented — current focus is on stabilizing and testing Protocol 1 and Protocol 2 operation across different radios first.

## License

GNU General Public License, version 2 or (at your option) any later version -- see [LICENSE](LICENSE). This matches WDSP's own license, which hpsdr-rs statically links against.
