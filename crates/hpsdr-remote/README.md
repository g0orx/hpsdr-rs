# hpsdr-remote

A thin remote client for hpsdr-rs. The radio stays with hpsdr-rs; this
connects to its TCI server and uses hpsdr-rs's remote extensions
([docs/remote-protocol.md](../../docs/remote-protocol.md)) so only control,
a spectrum row and compressed audio cross the network -- about 150 kbit/s
received, plus about 70 kbit/s sent while transmitting, against tens of
Mbit/s for standard TCI streams.

One codebase, two builds: a native program (Linux, Windows, macOS), and a
web page for any modern browser, including iPhone and iPad.

**Phase 2: receive and transmit.** VFO (slide, scroll or step to tune),
the radio's own band buttons, modes, filter width, AGC, NB, NR, ANF and
SNB, S-meter, audio gain and mute, and the spectrum and waterfall (click or
slide to tune). The spectrum shows the hpsdr-rs main receiver's own view,
so it follows that receiver's zoom and pan. MOX transmits from the
microphone, with mic gain and level, the radio's TX power setting, and
power and SWR while transmitting; not CW yet. If the connection drops, or
the client stops sending audio for a second, hpsdr-rs unkeys.

## Layout

**Layout** (beside the connection status) is Auto, Standard or Compact.
Standard has every control in rows above the spectrum. Compact, as
hpsdr-rs's own for small screens, has only the frequency, S-meter and - / +
above the spectrum, and a toolbar of large buttons below it -- Band and Mode
show what they're set to, then Step, Filter, Noise, Audio, Key and TX as
the server allows, and MOX (or CW's Stop) at the right. A button opens its
controls over the bottom of the spectrum; it again, the close button or
Escape closes them. The buttons go onto a second row when they'd be too
narrow for a finger. Auto is compact on a screen narrower than 600 or
shorter than 500 points: a phone either way up, or a Raspberry Pi's 7"
800x480 display. The choice isn't remembered between runs yet.

Natively, `--fullscreen` fills the screen (with a Quit button beside
Layout) and F11 toggles it:

    cargo run -p hpsdr-remote --release -- --fullscreen

## The easy way: served by hpsdr-rs

Build the web client first (below), then hpsdr-rs, which then has it built
in. In hpsdr-rs, start the TCI server and Settings -> Network -> **Web
client** (HTTPS on, and a password), then open the address it shows in a
browser. The page connects back through that same port, so there's one
address, one certificate to accept, and a password. Browsers only allow
the microphone on an HTTPS page, and only start audio after a click, so
audio starts when you press **Connect**.

## Native

    cargo run -p hpsdr-remote --release

and enter `ws://<hpsdr-rs machine>:50001` (straight to TCI, on your own
network) or `ws://<hpsdr-rs machine>:8080/tci` (through the web server
with HTTPS off). The native client doesn't accept hpsdr-rs's self-signed
HTTPS certificate yet.

## Web, served some other way

    crates/hpsdr-remote/build-web.sh
    python3 -m http.server -d crates/hpsdr-remote/web 8080

then browse to `http://<that machine>:8080`. It suggests the same host,
port 50001, as the server -- receive only, as the page isn't HTTPS.
build-web.sh installs the wasm32 target and the matching wasm-bindgen-cli
the first time.

## Security

TCI itself has no password: anyone who can reach port 50001 can control
the radio. Over the internet, make only the web server's port reachable,
with a password set (TCI can then listen on 127.0.0.1 only), or use
WireGuard, Tailscale or an SSH tunnel.
