# Front-panel controllers (Raspberry Pi)

hpsdr-rs can use the front-panel controllers piHPSDR supports, wired to a
Raspberry Pi's GPIO header: rotary encoders (most with a push button), a VFO
knob and push switches. Settings → **Controller** chooses which one is fitted:

| Controller | Encoders | Switches |
|---|---|---|
| **Controller 1** | 3 with push buttons, and the VFO knob | 8, on GPIO lines |
| **Controller 2 V1** | 4 with push buttons, and the VFO knob | 16, on an MCP23017 (I2C) |
| **Controller 2 V2** | 4 dual (top and bottom) with push buttons, and the VFO knob | 16, on an MCP23017 (I2C) |
| **G2 V1 front panel** | as Controller 2 V2 | 16, on an MCP23017 (I2C) |

Choosing one sets piHPSDR's GPIO lines for it and a default action for each
control; **Restore defaults** puts them back. The setup is kept on this
computer (the controller is wired to it), not per radio, and takes effect
when you start a radio.

## What the controls do

Each encoder, push button and switch can do any of the
[MIDI](10-settings-midi.md) actions:

- **Encoders** drive a tuning action (VFO Tune, VFO B Tune, RIT, XIT) or one
  of the knob actions (AF Gain, AGC Gain, Mic Gain, TX Drive, Filter Width,
  CW Speed and so on), which each step moves up or down. Turning fast speeds
  up, as in piHPSDR.
- **VFO Tune** and **VFO B Tune** move by the **VFO step**, chosen next to
  **VFO-A** in the main window (1 Hz to 1 MHz, 100 Hz to start with), and
  land on multiples of it, as in piHPSDR: from 14.074.123 one 1 kHz step up
  is 14.075.000. **VFO Step Size +** and **-** change it from a button.
- A tuning action has a **sensitivity** that multiplies its steps, e.g. x0.5
  for a high-resolution optical VFO knob, to move one VFO step for every two
  of its own. RIT and XIT move 10 Hz a step.
- **Push buttons and switches** do a button action on press: MOX, Tune,
  Band and Mode up/down, **Band Stack (next)** (the next entry of the band's
  [band stack](02-main-window.md#band-stack)), A>B, CTUN, NB/NR and so on.

piHPSDR actions with no equivalent here start unassigned or as the nearest
one: its second-receiver gains, diversity gain and phase, PureSignal, Two
Tone, Lock and its menu buttons. Controller 1's eight switches, which
piHPSDR ties to its toolbar, start as MOX, Tune, Band down/up, Mode
down/up, NR and NB.

## Wiring and lines

The lines are Broadcom GPIO numbers, as in piHPSDR, and can be changed in
the tab: each encoder's A and B lines, each push button's line, and for the
MCP23017 its I2C device (`/dev/i2c-1`), address (0x20) and the GPIO line its
interrupt output is wired to (15). Inputs use the Pi's pull-ups and are
debounced by the kernel (2 ms for encoders, none for the VFO knob, 25 ms
for buttons and switches). The status line under the controller's name says
whether it's running, on which GPIO chip, or why not.

On a Raspberry Pi 5, GPIO 20 may not be available: piHPSDR's advice is to
rewire that encoder line to GPIO 14, and set it here.

## Setting up the Pi

- Enable I2C (for the MCP23017 controllers): `sudo raspi-config` →
  Interface Options → I2C.
- Your user needs to be in the `gpio` and `i2c` groups, as the first user
  on Raspberry Pi OS is: `groups` lists them; `sudo adduser $USER gpio` and
  `sudo adduser $USER i2c` add them (log out and in again).
- No other program (piHPSDR, say) can be using the same GPIO lines.

The controller is only supported on Linux; elsewhere the tab says so.

The CW paddle, straight key and PTT lines piHPSDR also offers on spare GPIO
lines aren't supported yet.

## With hpsdr-remote

hpsdr-remote, the remote client, works the same controllers: run it on the
Pi the controller is wired to, and click **Controller** beside the
connection status for the same settings, kept in hpsdr-remote's own
`~/.config/hpsdr-remote/controller.json`. Each control does what the
client's own controls do, and the change goes to hpsdr-rs on the other
computer: tuning by the client's step, band, band stack, mode, filter,
AGC gain, NB, NR, SNB, binaural, MOX, Tune, TX power, CW speed, split,
RIT/XIT, and VFO B. Its audio and mic gain are the client's own. A knob
shows its new value over the spectrum for a moment. Actions the client
can't do (CTUN, SAT, the RX equalizer, diversity, RF attenuation and the
CW sidetone and pitch) aren't offered. Only one program can have the GPIO
lines at a time, so not hpsdr-rs on the same Pi as well.
