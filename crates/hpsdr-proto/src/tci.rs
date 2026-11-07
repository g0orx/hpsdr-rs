/*
    TCI's text commands: `name:arg,arg,...;`, several to a WebSocket text
    message. Arguments never contain the reserved characters `:` `,` `;`.
    Also the extension commands' names (docs/remote-protocol.md).
*/

pub const HELLO: &str = "hpsdr_hello";
pub const SPECTRUM: &str = "hpsdr_spectrum";
pub const AUDIO: &str = "hpsdr_audio";

/// Features the server lists in its `hpsdr_hello` reply.
pub const FEATURE_SPECTRUM: &str = "spectrum";
pub const FEATURE_ADPCM: &str = "adpcm";

/// Audio rates `hpsdr_audio` accepts.
pub const AUDIO_RATES: [u32; 5] = [8_000, 12_000, 16_000, 24_000, 48_000];

/// One command: its name (lower case) and arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    pub name: String,
    pub args: Vec<String>,
}

impl Command {
    pub fn new(name: &str, args: &[&str]) -> Self {
        Self { name: name.to_string(), args: args.iter().map(|a| a.to_string()).collect() }
    }

    /// Argument `i` parsed as `T`.
    pub fn arg<T: std::str::FromStr>(&self, i: usize) -> Option<T> {
        self.args.get(i)?.trim().parse().ok()
    }
}

impl std::fmt::Display for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.args.is_empty() {
            write!(f, "{};", self.name)
        } else {
            write!(f, "{}:{};", self.name, self.args.join(","))
        }
    }
}

/// Every command in a text message. Empty pieces are skipped.
pub fn parse(text: &str) -> Vec<Command> {
    text.split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| match s.split_once(':') {
            Some((name, args)) => Command {
                name: name.trim().to_ascii_lowercase(),
                args: args.split(',').map(|a| a.trim().to_string()).collect(),
            },
            None => Command { name: s.to_ascii_lowercase(), args: Vec::new() },
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_several_and_round_trips() {
        let cmds = parse("vfo:0,0,14200000;Modulation:0,usb; ready;");
        assert_eq!(cmds.len(), 3);
        assert_eq!(cmds[0], Command::new("vfo", &["0", "0", "14200000"]));
        assert_eq!(cmds[0].arg::<u64>(2), Some(14_200_000));
        assert_eq!(cmds[1].name, "modulation");
        assert_eq!(cmds[2], Command::new("ready", &[]));
        assert_eq!(cmds[0].to_string(), "vfo:0,0,14200000;");
        assert_eq!(cmds[2].to_string(), "ready;");
        assert_eq!(Command::new(SPECTRUM, &["1024", "15"]).to_string(), "hpsdr_spectrum:1024,15;");
    }
}
