# FX3 firmware image

`SDDC_FX3.img` is the Cypress FX3 firmware that turns an RX-888 Mk2 from a bare
USB bootloader into a radio. It is **uploaded to the device's RAM**, never linked
into sdroxide, and is redistributed here under the MIT licence in `LICENSE.txt`.

| | |
|---|---|
| Upstream | <https://github.com/ringof/rx888-firmware> |
| Release | `v0.1.0` (2024-05-16) |
| Asset | `SDDC_FX3.img` |
| Size | 128 744 bytes |
| SHA-256 | `f1c682293c5cb1714b75e8b8cfad0e6cfe86b5f83b987082d425404dc56e4a06` |
| Licence | MIT — © 2017-2020 Oscar Steila IK1XPV, and the rx888-firmware contributors |

Upstream is itself a fork of Oscar Steila's [ExtIO_sddc](https://github.com/ik1xpv/ExtIO_sddc);
Franco Venturi's [SDDC_FX3](https://github.com/fventuri/SDDC_FX3) is a sibling fork.

## Why the image is bundled

The FX3 on this board has no populated boot EEPROM, so it enumerates on **every**
plug-in as `04b4:00f3` "FX3 BootROM" with no radio function whatsoever. Firmware
must be pushed over EP0 before there is anything to talk to. Bundling it is what
makes the receiver work out of the box instead of requiring the vendor's driver
package; `Rx888Config::firmware_path` overrides it for anyone testing their own build.

## Structure of this particular image

Verified by parsing the artifact — the loader in `src/firmware.rs` is written against
exactly this layout, and its unit tests reproduce it:

```
signature "CY", imagectl 0x1c, imagetype 0xb0
  section 0: addr 0x00000100  2272 words   9088 bytes   (I-TCM)
  section 1: addr 0x40003000 16384 words  65536 bytes   (SYSMEM)
  section 2: addr 0x40013000 11254 words  45016 bytes
  section 3: addr 0x40030000  2264 words   9056 bytes
terminator length 0, program entry 0x4000e8bc
checksum 0xb97153de (sum of every payload word, wrapping u32) — matches
```

## Updating

Firmware version is readable at runtime with `TESTFX3` (`0xAC`). If a newer release
is vendored, update the table above (including the SHA-256) and re-run
`cargo test -p sdroxide-rx888`, which parses this file and checks the checksum.
