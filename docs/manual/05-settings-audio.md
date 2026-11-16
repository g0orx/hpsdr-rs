[← About](04-about.md) | [Index](README.md) | [CW →](06-settings-cw.md)

# Settings: Audio

Open **Settings...** from the main window, then the **Audio** tab.

![Audio settings tab](images/04-audio-tab.png)

Picks which real or virtual sound device local RX playback and TX capture
use -- independent of your OS's own default device. This is the same
`cpal`-based device list on every platform (Linux, Windows, macOS).

## RX audio: Output device

Where local RX audio actually plays. **(System Default)** uses whatever
your OS considers the default output device; otherwise pick any listed
device -- for example a virtual cable (VB-Audio Virtual Cable on Windows)
to feed a decoder like WSJT-X, instead of or alongside real speakers.

Changing this restarts local RX playback immediately on the newly selected
device.

## TX audio: Input device

Where TX audio is captured from. **(System Default)** uses your OS's
default microphone; otherwise pick any listed input device -- for example
a virtual cable to feed TX audio in from another application instead of a
real microphone.

Changing this restarts mic capture immediately, without needing to
disable/re-enable transmit (Settings → TX).

## Notes

- If a previously-selected device is no longer present on this machine
  (e.g. a saved virtual-cable choice on a machine that doesn't have it
  installed), hpsdr-rs falls back to the system default rather than
  failing to start audio at all.
- Each [extra receiver](03-extra-receivers.md) has its own, independent
  Output device picker in its own Settings window's **RX** tab -- so, for
  example, the main receiver can go to real speakers while an extra
  receiver feeds a virtual cable for a second decoder, or vice versa.

## No sound on Linux or a Raspberry Pi

On Linux, **(System Default)** uses PipeWire's ALSA device (`pipewire`)
or PulseAudio's (`pulse`) when ALSA has one, so the audio goes through
the sound server: hpsdr-rs shows in pavucontrol, and you can send it to
any output there. If neither will open, it uses ALSA's own default.

Without either device, ALSA's default is simply the first sound card --
on a Raspberry Pi, the **bcm2835 Headphones** jack -- so a USB headset
or HDMI monitor stays silent, hpsdr-rs doesn't appear in pavucontrol,
and the default microphone is reported as unavailable. Settings → Audio
then shows a warning. Install the plugin and restart hpsdr-rs:

```
sudo apt install pipewire-alsa
```

---

[← About](04-about.md) | [Index](README.md) | [CW →](06-settings-cw.md)
