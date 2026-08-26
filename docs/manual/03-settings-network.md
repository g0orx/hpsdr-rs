[← Main Window](02-main-window.md) | [Index](README.md) | [Audio →](04-settings-audio.md)

# Settings: Network

Open **Settings...** from the main window, then the **Network** tab.

![Network settings tab](images/03-network-tab.png)

This tab starts and stops three optional control servers that let other
software (loggers, digital-mode programs, remote-control apps) drive
hpsdr-rs. Each server's **Start**/**Stop** button is colored green when it
would start the server, red when it would stop it. None of the three
require authentication, so only bind `0.0.0.0` (all interfaces) if you
trust your local network.

PTT sent through any of these three does drive the radio's real transmit
(the same `mox` flag as the on-screen **MOX** button and Spacebar
hold-to-talk), but only while Settings → TX → **Enable Transmit** is on --
with transmit not armed, a PTT request is accepted (so the client's own
handshake doesn't fail) but has no receiver on the other end.

## rigctl

Hamlib-compatible TCP control, for software that supports "Hamlib NET
rigctl" (e.g. WSJT-X, N1MM, and most logging/digital-mode programs).

- Address field (`host:port`) -- default `0.0.0.0:4532`.
- **Start** / **Stop** button. The address field is disabled while running
  -- stop the server before changing it.
- Status line shows **Running** (with a reminder to stop before changing
  the address) or **Stopped**. A red error line appears if the server
  fails to bind (e.g. the port is already in use).
- **Log to file (rigctl_log.txt)** -- optional debug logging of every
  command received and reply sent, saved alongside this radio's settings.

`0.0.0.0` accepts connections from any machine on your network;
`127.0.0.1` restricts it to this machine only.

## TCI (Transceiver Control Interface)

A WebSocket-based control protocol, used by some digital-mode software
(e.g. as an alternative to rigctl for WSJT-X, or TCI Remote-style clients)
for both control and audio streaming.

- Address field -- default `0.0.0.0:40001`.
- Same **Start**/**Stop**/status/logging behavior as rigctl above (logs to
  `tci_log.txt`).

## CAT (Kenwood TS-2000 emulation)

A plain-ASCII, semicolon-terminated command set for logging/rig-control
software that talks directly to a "Kenwood TS-2000" over a network socket
(e.g. N1MM+, Log4OM, DXLab Commander, Ham Radio Deluxe) rather than through
Hamlib's rigctld protocol. This is a different, separate protocol from
rigctl above, even though piHPSDR's own equivalent feature happens to be
named "rigctl" too.

- Address field -- default `0.0.0.0:19090` (matches piHPSDR's own default
  port, so a logger already configured against a piHPSDR station needs no
  changes to point at hpsdr-rs instead).
- Same **Start**/**Stop**/status/logging behavior as rigctl above (logs to
  `cat_log.txt`).
- Implements a practical subset of the TS-2000 command set: frequency,
  mode, PTT, S-meter, and a few identification/status commands. Commands
  with no clean equivalent in this app yet (drive/power level, mic gain,
  CW keyer, RIT/XIT, memory channels) respond `?;` or are accepted with no
  effect, rather than reporting a made-up value.

None of the three servers auto-start -- start them explicitly here each
session, or whenever you need them.

---

[← Main Window](02-main-window.md) | [Index](README.md) | [Audio →](04-settings-audio.md)
