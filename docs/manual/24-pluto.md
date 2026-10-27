[← DATV](23-datv.md) | [Index](README.md)

# ADALM-Pluto (receive-only, for now)

> **Confirmed working on a real Pluto clone** (Analog Devices PlutoSDR
> Rev.B, AD9363, firmware v0.39): receiving QO-100 through an LNB, and
> [DATV](23-datv.md) on the wideband transponder. Transmit isn't
> supported yet.

hpsdr-rs can use an **ADALM-Pluto** (or one of its many clones) as a
receiver. Unlike the USB dongles, the Pluto is reached over the network:
it runs Analog Devices' own server, iiod, which hpsdr-rs talks to
directly. There's no libiio, libad9361 or SoapySDR to install.

## Connecting

Plug the Pluto into the micro-USB socket marked **OTG**. (On clones with
two sockets, the other one, often with a CH340 chip, is only a serial
console.) After about 20 seconds the Pluto appears to the computer as a
network adapter, and the Pluto itself is at **192.168.2.1**.

Click **Rediscover**. The Pluto appears in the list as board **Pluto**,
with its IP address. Select it and click **Start**.

Discovery looks for a Pluto at 192.168.2.1. It also tries the `.1`
address of any network where this computer's own address ends in `.10`,
which is what the Pluto hands out, so one moved to another subnet in its
`config.txt` is found too.

### Over Ethernet

A Pluto on your network (a Pluto+ with its own Ethernet socket, or a
USB-Ethernet adapter on the OTG port) is found automatically too, at
whatever address your router gave it. Discovery asks the network for
iiod servers (an mDNS query for `_iio._tcp`), which the Pluto's
firmware answers. If yours doesn't show up -- mDNS doesn't cross routers,
for one -- add its address with the Discover window's
[Manual IP](01-getting-started.md#discovering-your-radio).

A Pluto reachable both ways (USB and Ethernet at once) is listed once
for each address; pick either.

Mind the link's speed. The normal stream needs about 6 MB/s, and
receive and transmit together about 12 MB/s. [Wide
stream](#wide-stream) needs about 18 MB/s. A 100 Mbit/s adapter carries
at most about 11 MB/s, so on one of those use the normal stream, and
expect transmitting while receiving to be marginal. With a gigabit
adapter on the OTG port, a Pluto clone streamed the wide stream without
loss and topped out at about 29 MB/s -- the Pluto's own USB port is the
limit then, much as over USB. If samples go missing, hpsdr-rs says so
in the terminal.

The adapter has to be on the OTG port with a true OTG cable or adapter
(one that grounds the ID pin), which puts the port into USB host mode.
On a clone whose port stayed in device mode (`dmesg` shows `usb0` and
no `eth0`), it could be switched by hand from the Pluto's console:
`echo host > /sys/kernel/debug/usb/ci_hdrc.0/role`, then
`udhcpc -i eth0` for an address -- neither survives a reboot.

Settings are saved per Pluto, keyed by its serial number, so they
follow the radio whichever address it has.

The Pluto only streams to one program at a time. Close any other
software using it (SDR++, GNU Radio, `iio_readdev` and so on) before
connecting.

- **Linux**: no driver needed; the Pluto's USB network adapter works out
  of the box.
- **Windows** and **macOS**: untested. Windows may need Analog Devices'
  PlutoSDR USB drivers before the network adapter appears.

## Frequency coverage

A stock Pluto (AD9363) tunes **325 MHz to 3.8 GHz**, so the **70cm** and
**23cm** band buttons appear, and [**Gen**](02-main-window.md#bands-and-modes)
reaches the rest. 2m is below its range. A Pluto that has never been
connected before starts on 432.174 MHz.

hpsdr-rs reads the range from the Pluto itself, so one switched to
AD9364 mode (70 MHz to 6 GHz, a setting many owners change on the
Pluto) gets 2m as well. Above 4.29 GHz isn't reachable yet.

For QO-100, the LNB's IF (about 739-750 MHz) is well inside the range:
set up the [QO-100 transverter](18-xvtr.md#example-qo-100-with-an-lnb).

## RF Gain

**RF Gain** (0-71 dB), on the main window next to AGC Gain, sets the
AD9363's receive gain (its LNA, mixer and baseband stages together). The
default is 40 dB. Lower it if strong signals cause spurs or a raised
noise floor. On QO-100 with an LNB, 40-50 dB is a good starting point.
The **RF Attenuation** MIDI control drives this too.

## Sample rate

The Pluto normally streams at 1536 kHz (the AD9363 runs at 12.288 MHz
and the Pluto's FPGA decimates by 8). As with the
[RTL-SDR](21-rtl-sdr.md), [Settings: RX](15-settings-rx.md#sample-rate)'s
Sample Rate buttons choose how far hpsdr-rs decimates that: **48, 96,
192, 384, 768 or 1536** kHz (default 192). Changing the rate is quick:
the Pluto keeps streaming.

### Wide stream

**Wide stream (4608 kHz)**, under the Sample Rate buttons, makes the
Pluto stream at 4608 kHz with a 4 MHz filter instead. It's for signals
wider than 1.5 MHz, such as QO-100's wideband [DATV](23-datv.md)
beacon. The receiver's own rates are then **48 to 768** kHz.

That's about 18 MB a second over the USB link. On a real Pluto clone it
streamed without losing samples. If samples ever do go missing, hpsdr-rs
prints a line in the terminal saying what share of them arrived. Gaps
don't matter for listening, but they break DATV frames.

The Pluto is only set up when you connect, so a change takes effect the
next time you connect.

## Transponder view

With a Pluto, [DATV](23-datv.md#transponder-view)'s **Transponder view**
shows all of QO-100's wideband transponder (10491-10499.5 MHz) at once,
so you can see which stations are on and click one to receive it. See
the DATV chapter.

## Recording raw IQ

**Record raw IQ** in Settings: RX saves the Pluto's whole stream, before
any decimation -- see [the RTL-SDR chapter](21-rtl-sdr.md#recording-raw-iq).
In wide mode that's the full 4608 kHz.

## Centre spike

Like the USB dongles, the Pluto has some DC offset at the frequency it's
tuned to. hpsdr-rs tunes it 300 kHz above the dial, and a DC blocker
removes what's left -- see [the RTL-SDR chapter](21-rtl-sdr.md#centre-spike).

## Limitations

- **Receive only** for now: the transmit controls are hidden, and the
  Pluto's transmitter is kept switched off.
- **One receiver**: extra receivers aren't available.
- No frequency (ppm) correction yet. For QO-100, the transverter's LO
  Error and [PSK Beacon](02-main-window.md#psk-beacon-qo-100)'s Lock LNB
  correct the LNB and the Pluto together.
- The signal level mapping isn't calibrated, so the S-meter won't match
  a calibrated receiver.

---

[← DATV](23-datv.md) | [Index](README.md)
