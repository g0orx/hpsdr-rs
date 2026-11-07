/*
    The receiver's DSP settings a remote client shows and changes -- filter
    width, AGC, noise blanker and reduction -- as the extensions' own
    commands rather than TCI's, whose filter edges and NB/NR stage numbers
    don't map cleanly onto hpsdr-rs (docs/remote-protocol.md):

    - `hpsdr_state:<width>,<agc>,<agc_gain>,<nb>,<nr>,<anf>,<snb>;` server
      to client, every second and straight after a change;
    - `hpsdr_set:<name>,<value>;` client to server, one setting at a time.
*/

use crate::tci::Command;

pub const STATE: &str = "hpsdr_state";
pub const SET: &str = "hpsdr_set";

/// Names for `hpsdr_set`, and the values each takes.
pub const SET_WIDTH: &str = "width"; // Hz
pub const SET_AGC: &str = "agc"; // one of AGC_MODES
pub const SET_AGC_GAIN: &str = "agc_gain"; // dB
pub const SET_NB: &str = "nb"; // one of NB_MODES
pub const SET_NR: &str = "nr"; // one of NR_MODES
pub const SET_ANF: &str = "anf"; // true/false
pub const SET_SNB: &str = "snb"; // true/false

pub const AGC_MODES: [&str; 5] = ["off", "long", "slow", "medium", "fast"];
pub const NB_MODES: [&str; 3] = ["off", "nb", "nb2"];
pub const NR_MODES: [&str; 4] = ["off", "nr", "nr2", "nr3"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DspState {
    pub width_hz: u32,
    pub agc: String,
    pub agc_gain_db: i32,
    pub nb: String,
    pub nr: String,
    pub anf: bool,
    pub snb: bool,
}

impl DspState {
    pub fn to_command(&self) -> Command {
        Command::new(
            STATE,
            &[
                &self.width_hz.to_string(),
                &self.agc,
                &self.agc_gain_db.to_string(),
                &self.nb,
                &self.nr,
                &self.anf.to_string(),
                &self.snb.to_string(),
            ],
        )
    }

    /// From an `hpsdr_state` command, or None if it isn't one or is short.
    pub fn from_command(cmd: &Command) -> Option<Self> {
        if cmd.name != STATE || cmd.args.len() < 7 {
            return None;
        }
        Some(Self {
            width_hz: cmd.arg(0)?,
            agc: cmd.args[1].to_ascii_lowercase(),
            agc_gain_db: cmd.arg(2)?,
            nb: cmd.args[3].to_ascii_lowercase(),
            nr: cmd.args[4].to_ascii_lowercase(),
            anf: cmd.args[5].eq_ignore_ascii_case("true"),
            snb: cmd.args[6].eq_ignore_ascii_case("true"),
        })
    }
}

/// `hpsdr_set:<name>,<value>;`
pub fn set(name: &str, value: &str) -> Command {
    Command::new(SET, &[name, value])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tci::parse;

    #[test]
    fn state_round_trips() {
        let s = DspState {
            width_hz: 2700,
            agc: "fast".into(),
            agc_gain_db: 80,
            nb: "nb2".into(),
            nr: "off".into(),
            anf: true,
            snb: false,
        };
        let text = s.to_command().to_string();
        assert_eq!(text, "hpsdr_state:2700,fast,80,nb2,off,true,false;");
        assert_eq!(DspState::from_command(&parse(&text)[0]), Some(s));
        assert!(DspState::from_command(&parse("hpsdr_state:2700,fast;")[0]).is_none());
        assert_eq!(set(SET_NR, "nr2").to_string(), "hpsdr_set:nr,nr2;");
    }
}
