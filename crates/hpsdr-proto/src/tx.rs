/*
    Transmitting from a remote client (docs/remote-protocol.md):

    - `hpsdr_mox:<true|false>;` client to server: key or unkey. The server
      keys only where the radio itself would (in band, unless out-of-band
      TX is allowed) and not in CW, and unkeys a client's transmission if
      the client goes, or sends no TX audio for a second.
    - TX audio, client to server: binary type 101 (IMA-ADPCM, mono), the
      same blocks as RX audio, at the rate in the header.
    - `hpsdr_tx:<available>,<mox>,<power>,<max>,<forward>,<swr>;` server to
      client after hpsdr_hello: whether this radio can transmit at all
      (receive-only ones can't), keyed or not, the TX power setting and its
      maximum (W), and the measured forward power (W) and SWR -- five times
      a second while transmitting, every second otherwise.
    - `hpsdr_set:power,<watts>;` client to server: the TX power setting,
      as the radio's own slider.
*/

use crate::tci::Command;

pub const MOX: &str = "hpsdr_mox";
pub const TX: &str = "hpsdr_tx";
pub const SET_POWER: &str = "power"; // hpsdr_set name, W

/// TX audio rate a client sends at: plenty for a 3 kHz voice channel.
pub const TX_AUDIO_RATE: u32 = 16_000;

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct TxState {
    pub available: bool,
    pub mox: bool,
    pub power_w: u32,
    pub max_power_w: u32,
    pub forward_w: f32,
    pub swr: f32,
}

impl TxState {
    pub fn to_command(&self) -> Command {
        Command::new(
            TX,
            &[
                &self.available.to_string(),
                &self.mox.to_string(),
                &self.power_w.to_string(),
                &self.max_power_w.to_string(),
                &format!("{:.1}", self.forward_w),
                &format!("{:.2}", self.swr),
            ],
        )
    }

    /// From an `hpsdr_tx` command, or None if it isn't one or is short.
    pub fn from_command(cmd: &Command) -> Option<Self> {
        if cmd.name != TX || cmd.args.len() < 6 {
            return None;
        }
        Some(Self {
            available: cmd.args[0] == "true",
            mox: cmd.args[1] == "true",
            power_w: cmd.arg(2)?,
            max_power_w: cmd.arg(3)?,
            forward_w: cmd.arg(4)?,
            swr: cmd.arg(5)?,
        })
    }
}

pub fn mox(on: bool) -> Command {
    Command::new(MOX, &[&on.to_string()])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tci::parse;

    #[test]
    fn tx_state_round_trips() {
        let s = TxState { available: true, mox: true, power_w: 50, max_power_w: 200, forward_w: 48.5, swr: 1.25 };
        let text = s.to_command().to_string();
        assert_eq!(text, "hpsdr_tx:true,true,50,200,48.5,1.25;");
        assert_eq!(TxState::from_command(&parse(&text)[0]), Some(s));
        assert_eq!(mox(false).to_string(), "hpsdr_mox:false;");
        assert!(TxState::from_command(&parse("hpsdr_tx:true,50;")[0]).is_none());
    }
}
