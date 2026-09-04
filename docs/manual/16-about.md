[← Open Collector](15-open-collector.md) | [Index](README.md) | [Ozy USB →](17-ozy-usb.md)

# Settings: About

Open **Settings...** from the main window, then the **About** tab.

Shows details of the radio currently connected to:

- **Board** -- the detected board type (e.g. Orion2, HermesLite2).
- **Protocol** -- 1 or 2 (openHPSDR Protocol 1/USB-style, or Protocol 2/
  Ethernet).
- **Protocol Version** -- the firmware's reported protocol version.
- **IP Address** -- the radio's own address on the network. Shows "USB"
  for [Ozy USB](17-ozy-usb.md) instead, which has no network address.
- **MAC Address** -- the radio's hardware Ethernet address. Not shown
  for Ozy (no real MAC).
- **Interface** -- which of this computer's own network interfaces (e.g.
  `eth0`) the connection is using. Shows "USB" for Ozy.

Connected to Ozy USB specifically, three extra rows also appear: **Ozy
FX2 Version**, **Mercury FW** (one or two version numbers, separated by
`/` for a two-Mercury setup), and **Penny FW** -- all read once over I2C
at connect time.

Also shows the author's name and contact email.

---

[← Open Collector](15-open-collector.md) | [Index](README.md) | [Ozy USB →](17-ozy-usb.md)
