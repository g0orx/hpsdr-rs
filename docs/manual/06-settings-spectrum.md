[← RX](05-settings-rx.md) | [Index](README.md) | [TX →](07-settings-tx.md)

# Settings: Spectrum

Open **Settings...** from the main window, then the **Spectrum** tab.

![Spectrum settings tab](images/05-spectrum-tab.png)

## Display range

**Low** and **High** sliders (-180.0 to 0.0 dB) set the dB range the
spectrum trace is drawn against -- signals below **Low** sit at the bottom
of the pane, signals at or above **High** clip at the top. A separate pair
of sliders sets the same range for the waterfall.

If a strong or weak station always looks pinned to the top or bottom of the
display, adjust these.

**Auto**, next to the spectrum **Low** slider, continuously tracks the
lowest level currently shown in the trace and sets **Low** from it, so the
noise floor stays pinned near the bottom of the display without manual
re-adjustment as band conditions change (a few bins at each edge of the
trace are excluded from that tracking, and the result is smoothed, so it
doesn't jump on every noise spike or edge artifact). While **Auto** is on,
the **Low** slider is disabled -- turn **Auto** off to set it manually
again. RX only; the TX range below is unaffected.

## Waterfall palette

Four color palettes: **Fire, Ocean, Classic, Grayscale**. Pick whichever is
easiest to read for you.

## While transmitting

The spectrum/waterfall panes switch to a separate TX-specific display (fed
from your own transmitted signal, not off-air reception) while MOX is
active, since your TX signal is typically much stronger than anything
you'd receive. A second set of **Low**/**High** sliders (default -180.0 to
40.0 dB -- a wider range than RX, to accommodate that) controls this
separately, so tuning your RX display doesn't also require re-tuning it
every time you key up.

The shaded filter-passband overlay also switches to red/orange while
transmitting (blue for RX) and reflects the actual TX filter -- so it
shows what's really being transmitted, which can land on a different part
of the display than the RX passband once [Split](02-main-window.md#vfo-a--vfo-b--split)
is engaged.

![TX spectrum display](images/05-tx-spectrum.png)

---

[← RX](05-settings-rx.md) | [Index](README.md) | [TX →](07-settings-tx.md)
