/*
    The CW keyer behind the on-screen key and paddles: turns what's pressed
    into key transitions, each with the time it happens on the client's
    clock (ms). The server plays them with the same spacing
    (hpsdr-proto's cw.rs, hpsdr_key), so what goes on air is the keyer's
    own timing, not the screen's or the network's.

    Straight: down while the key is pressed. Iambic: a dit (one unit) or a
    dah (three) for each paddle, a one-unit space after each, alternating
    while both are squeezed. As piHPSDR's keyer (iambic.c, after KO0B and
    the Curtis 8044): the other paddle is sent next if it's held at the end
    of an element's space, if it was hit at any time during the element or
    its space, or if it was held when the element began. Mode A forgets
    that last one when both paddles are up at the end of the space -- so
    releasing a squeeze during a dah gives K, not Mode B's C -- but a paddle
    hit during an element is sent in either mode. Elements follow each
    other exactly, even though this is only looked at once a frame: their
    times are worked out, not read from the clock.
*/

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Straight,
    IambicA,
    IambicB,
}

impl Mode {
    pub const ALL: [Mode; 3] = [Mode::Straight, Mode::IambicA, Mode::IambicB];

    pub fn label(self) -> &'static str {
        match self {
            Mode::Straight => "Straight",
            Mode::IambicA => "Iambic A",
            Mode::IambicB => "Iambic B",
        }
    }
}

/// An element being sent: dah or dit, when the key comes up, and when its
/// following space ends.
#[derive(Debug, Clone, Copy)]
struct Element {
    dah: bool,
    up_at: f64,
    end_at: f64,
    up_sent: bool,
}

pub struct Keyer {
    pub mode: Mode,
    element: Option<Element>,
    /// The paddle was hit during the current element or its space.
    dit_memory: bool,
    dah_memory: bool,
    /// The other paddle was held when the current element began.
    dit_held: bool,
    dah_held: bool,
    /// The paddles at the last update, to see them hit.
    last_dit: bool,
    last_dah: bool,
    /// Straight key: down now.
    straight_down: bool,
}

impl Keyer {
    pub fn new(mode: Mode) -> Self {
        Self {
            mode,
            element: None,
            dit_memory: false,
            dah_memory: false,
            dit_held: false,
            dah_held: false,
            last_dit: false,
            last_dah: false,
            straight_down: false,
        }
    }

    /// The key is down at `now` (the sidetone follows this).
    pub fn is_down(&self, now: f64) -> bool {
        match self.mode {
            Mode::Straight => self.straight_down,
            _ => self.element.is_some_and(|e| now < e.up_at),
        }
    }

    /// Sending, or in the space after an element.
    pub fn is_busy(&self) -> bool {
        self.straight_down || self.element.is_some()
    }

    /// Advances to `now` (ms) with the key (straight: `dit`) or paddles as
    /// they are, at `wpm`. Returns the transitions since the last call --
    /// (time ms, down) -- in order. An element still under way keeps its
    /// times if the speed changes.
    pub fn update(&mut self, now: f64, dit: bool, dah: bool, wpm: u32) -> Vec<(f64, bool)> {
        let mut out = Vec::new();
        if self.mode == Mode::Straight {
            if dit != self.straight_down {
                self.straight_down = dit;
                out.push((now, dit));
            }
            self.element = None;
            return out;
        }
        if self.straight_down {
            self.straight_down = false;
            out.push((now, false));
        }
        let unit = 1200.0 / wpm.max(1) as f64;
        // A paddle hit since the last look is remembered for the element
        // under way.
        if dit && !self.last_dit {
            self.dit_memory = true;
        }
        if dah && !self.last_dah {
            self.dah_memory = true;
        }
        self.last_dit = dit;
        self.last_dah = dah;
        loop {
            match self.element.as_mut() {
                Some(e) => {
                    if !e.up_sent && now >= e.up_at {
                        e.up_sent = true;
                        out.push((e.up_at, false));
                    }
                    if now < e.end_at {
                        break;
                    }
                    let (was_dah, end_at) = (e.dah, e.end_at);
                    if self.mode == Mode::IambicA && !dit && !dah {
                        self.dit_held = false;
                        self.dah_held = false;
                    }
                    let next = if was_dah {
                        if self.dit_memory || dit || self.dit_held {
                            Some(false)
                        } else if dah {
                            Some(true)
                        } else {
                            None
                        }
                    } else if self.dah_memory || dah || self.dah_held {
                        Some(true)
                    } else if dit {
                        Some(false)
                    } else {
                        None
                    };
                    self.element = None;
                    match next {
                        Some(next_dah) => self.start(next_dah, end_at, unit, dit, dah, &mut out),
                        None => {
                            self.dit_memory = false;
                            self.dah_memory = false;
                            break;
                        }
                    }
                }
                None => {
                    if dit {
                        self.start(false, now, unit, dit, dah, &mut out);
                    } else if dah {
                        self.start(true, now, unit, dit, dah, &mut out);
                    } else {
                        break;
                    }
                }
            }
        }
        out
    }

    /// Starts an element at `at`: the other paddle's memory starts afresh,
    /// and whether it's held now is noted.
    fn start(&mut self, dah: bool, at: f64, unit: f64, dit_now: bool, dah_now: bool, out: &mut Vec<(f64, bool)>) {
        if dah {
            self.dah_memory = false;
            self.dit_memory = false;
            self.dit_held = dit_now;
        } else {
            self.dit_memory = false;
            self.dah_memory = false;
            self.dah_held = dah_now;
        }
        let length = if dah { 3.0 * unit } else { unit };
        self.element = Some(Element { dah, up_at: at + length, end_at: at + length + unit, up_sent: false });
        out.push((at, true));
    }

    /// Lets go of everything: the key up at `now` if it was down.
    pub fn release(&mut self, now: f64) -> Vec<(f64, bool)> {
        let was_down = self.is_down(now);
        self.element = None;
        self.straight_down = false;
        self.dit_memory = false;
        self.dah_memory = false;
        self.dit_held = false;
        self.dah_held = false;
        if was_down {
            vec![(now, false)]
        } else {
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs the keyer a frame (16 ms) at a time from `from` to `to` with the
    /// paddles given by `paddles(t)`, collecting every transition.
    fn run(k: &mut Keyer, from: f64, to: f64, paddles: impl Fn(f64) -> (bool, bool)) -> Vec<(f64, bool)> {
        let mut out = Vec::new();
        let mut t = from;
        while t <= to {
            let (dit, dah) = paddles(t);
            out.extend(k.update(t, dit, dah, 20));
            t += 16.0;
        }
        out
    }

    // At 20 WPM a unit is 60 ms.

    #[test]
    fn straight_follows_the_key() {
        let mut k = Keyer::new(Mode::Straight);
        assert_eq!(k.update(0.0, true, false, 20), vec![(0.0, true)]);
        assert!(k.update(10.0, true, false, 20).is_empty());
        assert_eq!(k.update(300.0, false, false, 20), vec![(300.0, false)]);
    }

    #[test]
    fn held_dit_paddle_sends_evenly_spaced_dits() {
        let mut k = Keyer::new(Mode::IambicB);
        let out = run(&mut k, 0.0, 400.0, |t| (t < 300.0, false));
        assert_eq!(
            out,
            vec![(0.0, true), (60.0, false), (120.0, true), (180.0, false), (240.0, true), (300.0, false)]
        );
    }

    #[test]
    fn squeeze_alternates_and_mode_b_adds_one_on_release() {
        // Dah, then both squeezed from 16 ms -- dah 0-180, dit 240-300,
        // dah 360-540 -- let go together at 400 ms, during that second dah:
        // Mode A gives K (dah dit dah), Mode B C (dah dit dah dit).
        let squeeze = |t: f64| (t >= 16.0 && t < 400.0, t < 400.0);
        let mut a = Keyer::new(Mode::IambicA);
        let out_a = run(&mut a, 0.0, 900.0, squeeze);
        let mut b = Keyer::new(Mode::IambicB);
        let out_b = run(&mut b, 0.0, 900.0, squeeze);
        let k = [(0.0, true), (180.0, false), (240.0, true), (300.0, false), (360.0, true), (540.0, false)];
        assert_eq!(&out_a[..], &k);
        assert_eq!(&out_b[..6], &k);
        assert_eq!(&out_b[6..], &[(600.0, true), (660.0, false)]);
    }

    #[test]
    fn a_tap_during_an_element_is_sent_in_either_mode() {
        // Dah (0-180), dit tapped at 48-64 and let go, dah let go at 100:
        // N in both modes -- the dit, 240-300, though nothing's held then.
        let paddles = |t: f64| (t >= 48.0 && t < 64.0, t < 100.0);
        let n = vec![(0.0, true), (180.0, false), (240.0, true), (300.0, false)];
        let mut b = Keyer::new(Mode::IambicB);
        assert_eq!(run(&mut b, 0.0, 600.0, paddles), n);
        let mut a = Keyer::new(Mode::IambicA);
        assert_eq!(run(&mut a, 0.0, 600.0, paddles), n);
    }

    #[test]
    fn release_lets_go_at_once() {
        let mut k = Keyer::new(Mode::IambicA);
        k.update(0.0, false, true, 20);
        assert!(k.is_down(10.0));
        assert_eq!(k.release(50.0), vec![(50.0, false)]);
        assert!(!k.is_busy());
        assert!(k.update(60.0, false, false, 20).is_empty());
    }
}
