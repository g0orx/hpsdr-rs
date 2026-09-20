[← Spectrum](16-settings-spectrum.md) | [Index](README.md) | [XVTR →](18-xvtr.md)

# Settings: TX

Open **Settings...** from the main window, then the **TX** tab.

![TX settings tab](images/06-tx-tab.png)

> **Transmit is unverified against your radio's actual protocol.**
> Bench-test into a dummy load at reduced drive before ever using a real
> antenna -- see the [top-level README](../../README.md) for what's
> confirmed vs. best-effort in the current build.

## Max TX Power

The ceiling for the **TX Power** slider on the main window, in watts
(1-1000W). Set this to your radio's actual maximum output -- the discovery
protocol reports board *type* (e.g. "Orion2"), not the specific model's
real power rating, so a 100W and a 200W radio of the same board family both
need this set correctly by hand. A sensible per-board default is applied
automatically the first time you connect to a given radio.

## Tune Power

The percentage (1-100%) of TX Power actually used while **TUNE** or **TWO
TONE** is engaged from the main window -- keep this low for safe antenna/
amplifier tuning rather than transmitting at full power.

## Max SWR

The SWR threshold (1.0-10.0:1, default 3.0:1) above which the main
window's power meter -- shown in place of the S-meter while transmitting
-- turns red, warning of a bad antenna match.

If SWR reaches or exceeds this while actually running more than 35W,
**TX Power** is automatically cut to 10W to protect the PA -- it doesn't
raise itself back up on its own once the match improves, so raise it
back manually when it's safe to.

## TX ADC0 Attenuation (standard boards only)

Protects ADC0's front end from this radio's own TX leakage while
transmitting (0-31dB, default 20dB). If you see an **ADC0 Overload**
warning while transmitting, raise this. Not shown on HermesLite/
HermesLite2, which handle RX gain differently. Same underlying value as
[PureSignal](14-puresignal.md)'s **Feedback Attenuation** slider --
adjusting either one changes both.

## Allow TX outside ham bands

Off by default. While off, hpsdr-rs refuses to key transmit at all
outside the defined ham band allocations -- e.g. while parked on
[**Gen**](02-main-window.md#bands-and-modes), general coverage -- across
every way to key TX: the main window's **MOX**/**TUNE**/**TWO TONE**
buttons (which grey out with a tooltip explaining why), the Spacebar
shortcut, a mapped MIDI Mox/Tune control, CW text send (including the
Kenwood CAT `KY` and rigctl `send_morse` remote equivalents), and CAT
`TX`/rigctl `\set_ptt`/TCI `trx` PTT commands from a client like WSJT-X.
An already-running transmission can always be stopped regardless of this
setting -- only *starting* one out of band is blocked.

Turn this on only if you have your own explicit authorization for
out-of-band operation (MARS/CAP, testing/development, etc.) -- it
doesn't check any regulatory database, it only removes hpsdr-rs's own
safety check. Takes effect immediately, no reconnect needed.

> **Note:** this can't intercept CW sent by a physical key/paddle wired
> directly into the radio's own hardware KEY jack (break-in keying) --
> the radio's own firmware keys the transmitter on its own, with no
> software PTT decision involved at all. This setting only covers
> transmissions this application itself initiates.

## Enable Transmit

Arms (or disarms) the whole TX signal path -- microphone input, the TX DSP
chain, and the TX-spectrum display. Disarming forces MOX off immediately if
it was active. The PTT row on the main window only appears while this is
enabled.

## TX audio source

Chooses where TX audio comes from:

- **Auto** -- radio mic normally, TCI client audio when one is actively
  streaming.
- **Radio Mic** -- always use the radio's own mic input, ignoring any TCI
  client audio.
- **Local Mic (ignore TCI audio)** -- always use this computer's local
  microphone, ignoring both the radio's mic input and TCI audio. Useful as
  a workaround if a particular TCI client's audio streaming has problems.

## Radio Mic Connector (Angelia/Orion/Orion2 only)

Two settings for boards with a shared PTT/mic/bias connector:

- Connector wiring: **PTT on Ring, Mic/Bias on Tip** or **PTT on Tip, Mic/
  Bias on Ring** -- match this to how your microphone/footswitch is
  actually wired.
- **Mic PTT Enabled** -- whether the radio should accept PTT from that
  connector at all.
- **Mic Bias Enabled** -- whether to supply bias voltage for an electret
  mic element.

---

[← Spectrum](16-settings-spectrum.md) | [Index](README.md) | [XVTR →](18-xvtr.md)
