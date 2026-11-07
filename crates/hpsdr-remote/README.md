# hpsdr-remote

A thin remote client for hpsdr-rs. The radio stays with hpsdr-rs; this
connects to its TCI server (port 50001) and uses hpsdr-rs's remote
extensions ([docs/remote-protocol.md](../../docs/remote-protocol.md)) so only
control, a spectrum row and compressed audio cross the network --
about 150 kbit/s, against tens of Mbit/s for standard TCI streams.

One codebase, two builds: a native program (Linux, Windows, macOS), and a
web page for any modern browser, including iPhone and iPad.

**Phase 1: receive only, on a LAN.** VFO (slide, scroll or step to tune), bands, modes, filter width, AGC, NB, NR, ANF and SNB,
S-meter, audio gain and mute, and the spectrum and waterfall (click or
scroll to tune). The spectrum shows the hpsdr-rs main receiver's own view,
so it follows that receiver's zoom and pan.

There is no password or encryption yet, and TCI itself has none: anyone
who can reach port 50001 can control the radio. Keep it on your own
network, or reach it through WireGuard, Tailscale or an SSH tunnel.

## Native

    cargo run -p hpsdr-remote --release

and enter `ws://<hpsdr-rs machine>:50001`.

## Web

    rustup target add wasm32-unknown-unknown
    cargo install wasm-bindgen-cli --version <the wasm-bindgen version in Cargo.lock>
    crates/hpsdr-remote/build-web.sh
    python3 -m http.server -d crates/hpsdr-remote/web 8080

then browse to `http://<that machine>:8080`. It suggests the same host,
port 50001, as the server.

Or let hpsdr-rs serve it: build the web client first, then hpsdr-rs (which
then has the client built in), and start Settings -> Network -> **Web
client**. Served that way, the page suggests hpsdr-rs's actual TCI port. Browsers only start audio after a click, so
audio starts when you press **Connect**.
