[← PureSignal](13-puresignal.md) | [Index](README.md) | [Spectrum →](15-settings-spectrum.md)

# Settings: RX

Open **Settings...** from the main window, then the **RX** tab.

![RX settings tab](images/04-rx-tab.png)

## Sample rate

Selectable buttons for the receiver's sample rate. Protocol 2 radios offer
**48, 96, 192, 384, 768, 1536** kHz; Protocol 1 radios offer **48, 96, 192,
384** kHz. Changing this briefly interrupts audio/spectrum while the demod
chain restarts.

## ADC and antenna (Protocol 2 only)

On boards with more than one ADC, **ADC** buttons choose which one the
main receiver listens to. If **ADC0** is selected, an **Antenna** row
appears -- **ANT1, ANT2, ANT3** -- since ADC0's antenna selection is shared
across every receiver using it.

## RX attenuation / RX Gain

On any board other than a HermesLite/HermesLite2 connected over Protocol
1, an **RX Attenuation** slider (0-31 dB) reduces the receiver's input
level -- turn this up if you're overloading the front end on a strong
signal, and back down if signals seem unusually weak. This also covers
Protocol 2 connections (including a HermesLite2 over Protocol 2 -- its
RX Gain control, below, only exists over Protocol 1).

A HermesLite/HermesLite2 connected over Protocol 1 has no step
attenuator at all; instead it shows an **RX Gain** slider (-12 to +48
dB) that adds front-end gain (positive values) or attenuation (negative
values) directly. Lower it if the spectrum looks garbled/overloaded on
a strong band, raise it if signals seem unusually weak.

## Send RX audio to radio

A checkbox that routes the demodulated audio back out through the radio's
own local audio jack, in addition to your computer's speakers.

On a HermesLite2 running Protocol 1 with the **HL2+ Audio Codec** option
below left off (the default), this has no effect -- that board's stock
firmware repurposes the same wire bytes for something else, so nothing is
actually sent. Protocol 2 has no such restriction and always works.

### HL2+ Audio Codec (AK4951 add-on board)

Shown only for a HermesLite2 on Protocol 1. A checkbox declaring that
this radio has a real add-on board installed -- one built around an
AK4951 codec, adding PHONES, MIC, and KEY jacks to emulate a standard
HPSDR radio's local audio I/O -- running that board's own dedicated
firmware build. There's no way to detect this automatically, so it has
to be set explicitly, same as piHPSDR-family apps' own equivalent RADIO
menu setting.

Enabling it does two things: lets **Send RX audio to radio** above
actually reach this board, and permanently sets a bit in the radio
command stream that board's gateware uses to recognize the codec is
present. Leave this off on a stock HermesLite/HermesLite2 with no add-on
board -- there's nothing for it to do there.

## AGC tuning

Six sliders fine-tune the AGC curve (the AGC mode itself -- Off/Long/Slow/
Medium/Fast -- is toggled from the main window, not here):

| Slider | Range | Step |
|---|---|---|
| Attack | 0-20 ms | 1 |
| Decay | 0-2000 ms | 25 |
| Hang | 0-2000 ms | 25 |
| Top | 0.0-140.0 dB | 2.0 |
| Slope | 0-100 dB | 2 |
| Thresh | -140.0-0.0 dB | 2.0 |

**Top** is also available directly on the main window as **AGC Gain**
(piHPSDR's name for the same value) -- both control the identical setting.

## Noise blanker threshold

**NB Threshold** (0-100) is shared by both blanker stages (NB and NB2,
toggled from the main window) -- there's only one threshold, whichever
stage is active uses it.

---

[← PureSignal](13-puresignal.md) | [Index](README.md) | [Spectrum →](15-settings-spectrum.md)
