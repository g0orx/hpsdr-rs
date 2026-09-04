[← Settings: About](16-about.md) | [Index](README.md)

# Ozy USB (legacy hardware)

> **New, partially confirmed against real hardware.** USB discovery and
> the FX2 firmware load stage have both been confirmed working on a
> real Ozy. FPGA bitstream load, RX/TX streaming, and I2C telemetry are
> still unconfirmed -- if you try it, reports (good or bad) are very
> welcome.

The original HPSDR hardware -- an "Ozy" board (Cypress FX2 USB
controller + FPGA) paired with separate Mercury (RX) and Penny
(TX/audio codec) boards on a backplane -- predates every other radio
hpsdr-rs talks to, which all use Ethernet/UDP. Ozy speaks the same
Protocol 1 framing this app already uses, just over raw USB instead of
the network.

## One-time setup

1. **Firmware files** -- the Cypress FX2 RAM firmware
   (`ozyfw-sdr1k.hex`) and the FPGA bitstream (`Ozy_Janus.rbf`) are
   **bundled with hpsdr-rs** (sourced from the author's own piHPSDR
   repo, same GPL-2.0 license) -- nothing to do here unless you want to
   use a different/custom build, via **Choose...** in the Discover
   window's **Ozy USB setup** section.
2. **Linux only**: install the udev rule for non-root USB access. Copy
   `assets/90-ozy.rules` (from the hpsdr-rs source, or
   `/usr/share/doc/hpsdr-rs/90-ozy.rules` if installed via the `.deb`)
   to `/etc/udev/rules.d/`, then either replug the device or run:
   ```sh
   sudo udevadm control --reload-rules && sudo udevadm trigger
   ```
3. **Windows only**: bind a WinUSB driver to Ozy's `fffe:0007` VID:PID
   using [Zadig](https://zadig.akeo.ie/) -- Windows has no built-in
   generic driver for an unrecognized USB device.
4. **macOS**: no extra driver needed.

Open the Discover window's **Ozy USB setup** section any time to check
which firmware/FPGA files are currently in use -- bundled copies show
as "(bundled)"; anything chosen via **Choose...** overrides that and is
saved for next time.

![Discovery window -- Ozy USB setup](images/17-ozy-usb-setup.png)

## Connecting

Once the udev rule (Linux) is set, plug in the Ozy and click
**Rediscover** -- it appears in the list as board **Ozy**, with "USB"
in place of an IP address. Select it and click **Start**, same as any
network radio.

The first connect takes noticeably longer than a network radio: behind
the scenes, hpsdr-rs loads the FX2 firmware, waits a few seconds for the
device to re-enumerate, then loads the FPGA bitstream, before streaming
actually begins.

## Differences from network-connected boards

- **2-receiver cap.** Classic Ozy hardware is capped at 2 receivers,
  matching piHPSDR's own documented limit for it (these boards are
  reported to hang with more).
- **No Diversity, no PureSignal.** Both need a second/feedback ADC this
  hardware generation doesn't have wired for that purpose.
- **TX power/SWR meter and ADC overload indicators still work** -- Ozy
  reports these over a separate I2C channel (Penny/Mercury telemetry)
  rather than in the main data stream the way Metis/Hermes-class boards
  do, but they feed the exact same meter and Max SWR protection (see
  [Settings: TX](07-settings-tx.md)) as every other board.
- See [Settings: About](16-about.md) for what the About tab shows for
  an Ozy connection specifically (firmware versions, "USB" in place of
  network details).

---

[← Settings: About](16-about.md) | [Index](README.md)
