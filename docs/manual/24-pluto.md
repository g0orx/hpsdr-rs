[← DATV](23-datv.md) | [Index](README.md)

# ADALM-Pluto

> **Confirmed working on a real Pluto clone** (Analog Devices PlutoSDR
> Rev.B, AD9363, firmware v0.39, and later F5OEO's Tezuka v0.3.24):
> receiving QO-100 through an LNB, and [DATV](23-datv.md) on the
> wideband transponder. [Transmit](#transmit) is checked through the
> chip's own loopback and shows correctly on the TX display, but hasn't
> been tested on air yet.

hpsdr-rs can use an **ADALM-Pluto** (or one of its many clones) as a
receiver and transmitter. Unlike the USB dongles, the Pluto is reached over the network:
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
limit then, much as over USB.

If samples go missing, hpsdr-rs prints a line in the terminal, every 10
seconds that it happens, starting with the time (UTC), e.g.
`2026-11-04T09:15:30Z Pluto: received 58.0% of the expected samples over
10.0 s -- some were lost`. The share and the times tell you the kind of
problem:

- **A steady, large shortfall** (the same 50-60% every time): the link
  can't carry the stream. On the Pluto's console, `cat
  /sys/class/net/eth0/speed` shows the speed it connected at; 100 instead
  of 1000 means a fault in the cable or something on the way. A long
  outdoor run that came up at 100 Mbit/s was cured by replacing the
  cable.
- **Occasional small losses**, scattered over hours: look at power. A
  Pluto and its USB-Ethernet adapter together need more than many small
  supplies give; at the dish, a stronger supply (through a fused 5 V
  converter) stopped them.

To keep a record over a long run, start hpsdr-rs with
`hpsdr-rs 2>&1 | tee ~/pluto-log.txt`.

The adapter has to be on the OTG port with a true OTG cable or adapter
(one that grounds the ID pin), which puts the port into USB host mode.
A clone with a **USB-C** socket has no ID pin: an OTG adapter signals
on USB-C's CC pins instead, which a Pluto without a USB-C controller
ignores, so it boots as a USB device whatever cable is used (`dmesg`
shows `usb0` and no `eth0`). Switch it by hand from the Pluto's console
(`root`, password `analog`):

```sh
echo host > /sys/bus/platform/devices/ci_hdrc.0/role
udhcpc -i eth0
```

(On firmware where that `role` file doesn't exist, the same switch is
`echo host > /sys/kernel/debug/usb/ci_hdrc.0/role`.)

To have that happen at every boot, put it in `/mnt/jffs2/autorun.sh`,
which the Pluto's firmware runs at startup (`/mnt/jffs2` is its
persistent storage) -- both Analog Devices' firmware and F5OEO's Tezuka
do. One command per line is safest over a serial console; this tries
both switches, and logs each boot to `/mnt/jffs2/autorun.log`:

```sh
echo 'date >> /mnt/jffs2/autorun.log' > /mnt/jffs2/autorun.sh
echo 'echo host > /sys/bus/platform/devices/ci_hdrc.0/role 2>> /mnt/jffs2/autorun.log || { mount -t debugfs debugfs /sys/kernel/debug 2>/dev/null; echo host > /sys/kernel/debug/usb/ci_hdrc.0/role; }' >> /mnt/jffs2/autorun.sh
echo 'for i in 1 2 3 4 5 6 7 8 9 10; do [ -e /sys/class/net/eth0 ] && break; sleep 1; done' >> /mnt/jffs2/autorun.sh
echo 'ip link show eth0 >> /mnt/jffs2/autorun.log 2>&1' >> /mnt/jffs2/autorun.sh
echo 'ifup eth0 2>/dev/null || udhcpc -b -i eth0' >> /mnt/jffs2/autorun.sh
sync
```

On Tezuka v0.3.24 the debugfs switch alone worked by hand but not from
this script at boot; the `role` file in `/sys/bus/platform/devices`
(what Tezuka's own `/root/usb_host.sh` uses) works in both.

That port then stays a USB host, so it can't connect to a PC by USB;
`rm /mnt/jffs2/autorun.sh` undoes it. If the file has vanished after a
reboot, `/mnt/jffs2` isn't mounted: `mount | grep jffs2` shows nothing,
and `dmesg | grep jffs2` complains. One clone's partition held leftover
data instead of a filesystem; `flash_erase -j /dev/mtd2 0 0` then
`mount -a` formatted it (this erases only `mtd2`, the small
user-settings partition -- check `cat /proc/mtd` lists it as
`qspi-nvmfs` first), after which the script survived and ran at every
boot.

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
noise floor. The **RF Attenuation** MIDI control drives this too.

The spectrum, waterfall and S-meter take RF Gain back out, so signal
levels and the noise floor stay put as you change it. They read as they
would at the default 40 dB. The chip's gain steps aren't 1 dB each, so
below 1300 MHz (70cm, and the QO-100 LNB's IF) hpsdr-rs uses steps
measured on a real Pluto; above that it assumes 1 dB per dB, which can
be a few dB out.

On QO-100's narrowband transponder, with the Pluto at the dish and only a
short lead from the LNB, every setting from 0 to 62 dB gave the same
signal-to-noise ratio, within about 1 dB: the LNB's noise is far above
the Pluto's own. The ADC starts to clip at 58 dB. **30 dB** is a good
setting: the strongest samples stay over 20 dB below full scale, leaving
room for strong signals. With a long coax run from the LNB, or a weaker
LNB, low settings can lose signal-to-noise ratio; raise it until the
noise floor stops improving.

## Sample rate

The Pluto normally streams at 1536 kHz (the AD9363 runs at 12.288 MHz
and the Pluto's FPGA decimates by 8). As with the
[RTL-SDR](21-rtl-sdr.md), [Settings: RX](15-settings-rx.md#sample-rate)'s
Sample Rate buttons choose how far hpsdr-rs decimates that: **48, 96,
192, 384, 768 or 1536** kHz (default 192). Changing the rate is quick:
the Pluto keeps streaming.

Some firmware's FPGA doesn't convert rates the way it reports: on
F5OEO's **Tezuka** (v0.3.24), only 40% of the samples arrived, and
transmit ran at the wrong rate. hpsdr-rs times the stream when it
connects, and on such firmware runs the AD936x at 3072 kHz directly and
does all the rate conversion itself; the terminal says so when it does.
Everything works the same, but the normal stream then needs about 12
MB/s (receive and transmit together about 24 MB/s, close to what the
link carries).

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

## Transmit

The Pluto transmits full duplex: its receiver keeps running while it
transmits, so you can hear your own downlink on QO-100 while sending on
the 2400 MHz uplink (the new **13cm** band, 2300-2450 MHz, covers it).
Turn on **Enable Transmit** in [Settings: TX](17-settings-tx.md) as for
any radio; MOX, Tune, Two Tone and [Duplex](02-main-window.md) work as
usual.

- **TX Drive** replaces the TX Power slider: -60 to 0 dB, where 0 dB is
  the Pluto's full output -- only a few milliwatts, so an uplink needs an
  amplifier after it. The scroll wheel and the **TX Drive** MIDI control
  move it in 1 dB steps. Tune and Two Tone use **Tune Power %** as a share
  of that power.
- There's no power detector, so the watts meter, Max TX Power and PA
  Calibration are hidden.
- The transmitter is powered down whenever you're not transmitting, so
  it puts nothing out between overs.
- **Not with the Wide stream on**: the transmit sample rate follows the
  receiver's, and at the wide rate the two together need more than the
  link carries. The main window says so in place of the transmit
  controls; turn Wide stream off and reconnect.

Check the output on a spectrum analyser or a second receiver, into a
dummy load, before connecting an amplifier.

## Recording raw IQ

**Record raw IQ** in Settings: RX saves the Pluto's whole stream, before
any decimation -- see [the RTL-SDR chapter](21-rtl-sdr.md#recording-raw-iq).
In wide mode that's the full 4608 kHz.

## Centre spike

Like the USB dongles, the Pluto has some DC offset at the frequency it's
tuned to. hpsdr-rs tunes it 300 kHz above the dial, and a DC blocker
removes what's left -- see [the RTL-SDR chapter](21-rtl-sdr.md#centre-spike).

## Limitations

- **Transmit is untested on air**, and not available with the Wide
  stream on.
- **One receiver**: extra receivers aren't available.
- No frequency (ppm) correction yet. For QO-100, the transverter's LO
  Error and [PSK Beacon](02-main-window.md#psk-beacon-qo-100)'s Lock LNB
  correct the LNB and the Pluto together.
- The signal level mapping isn't calibrated, so the S-meter won't match
  a calibrated receiver.

---

[← DATV](23-datv.md) | [Index](README.md)
