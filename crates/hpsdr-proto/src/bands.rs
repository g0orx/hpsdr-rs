/*
    The radio's band buttons, for a remote client to show the same ones
    (docs/remote-protocol.md):

    - `hpsdr_bands:<name>,<low>,<high>,<name>,<low>,<high>,...;` server to
      client, after hpsdr_hello and whenever the list changes: each band's
      name and RF edges, Hz, in the radio's own order. `Gen` (general
      coverage) spans the whole receiver; a transverter spans its RF range.
    - `hpsdr_band:<name>;` client to server: select it, exactly as clicking
      that button on the radio does -- its remembered frequency, mode and
      filter, and for a transverter, the transverter.
*/

use crate::tci::Command;

pub const BANDS: &str = "hpsdr_bands";
pub const BAND: &str = "hpsdr_band";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BandInfo {
    pub name: String,
    pub low_hz: u64,
    pub high_hz: u64,
}

/// A name with TCI's reserved characters (`:` `,` `;`) made spaces, so it
/// can go in a command.
pub fn clean_name(name: &str) -> String {
    name.chars().map(|c| if matches!(c, ':' | ',' | ';') { ' ' } else { c }).collect::<String>().trim().to_string()
}

pub fn to_command(bands: &[BandInfo]) -> Command {
    let mut args = Vec::with_capacity(bands.len() * 3);
    for b in bands {
        args.push(clean_name(&b.name));
        args.push(b.low_hz.to_string());
        args.push(b.high_hz.to_string());
    }
    Command { name: BANDS.to_string(), args }
}

/// From an `hpsdr_bands` command; None if it isn't one. A trailing partial
/// band is dropped.
pub fn from_command(cmd: &Command) -> Option<Vec<BandInfo>> {
    if cmd.name != BANDS {
        return None;
    }
    Some(
        cmd.args
            .chunks_exact(3)
            .filter_map(|c| Some(BandInfo { name: c[0].clone(), low_hz: c[1].parse().ok()?, high_hz: c[2].parse().ok()? }))
            .collect(),
    )
}

pub fn select(name: &str) -> Command {
    Command::new(BAND, &[&clean_name(name)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tci::parse;

    #[test]
    fn bands_round_trip() {
        let bands = vec![
            BandInfo { name: "70cm".into(), low_hz: 420_000_000, high_hz: 450_000_000 },
            BandInfo { name: "Gen".into(), low_hz: 325_000_000, high_hz: 3_800_000_000 },
            BandInfo { name: "QO-100".into(), low_hz: 10_489_500_000, high_hz: 10_490_000_000 },
        ];
        let text = to_command(&bands).to_string();
        assert_eq!(from_command(&parse(&text)[0]), Some(bands));
        assert_eq!(clean_name("QO-100; WB,x"), "QO-100  WB x");
        assert_eq!(select("2m").to_string(), "hpsdr_band:2m;");
        assert_eq!(from_command(&parse("hpsdr_bands:;")[0]), Some(vec![]));
    }
}
