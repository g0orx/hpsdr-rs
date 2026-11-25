/*
    Sending CW text (docs/remote-protocol.md) -- TCI's own commands, as
    Thetis and deskHPSDR implement them, so any TCI client can use them:

    - `cw_macros:<rx>,<text>;` client to server: send this text as CW, at
      the CW speed, after anything already being sent.
    - `cw_macros_stop;` client to server: stop at once, dropping the rest.
    - `cw_macros_speed:<wpm>;` either way: the speed, words a minute.
    - `cw_macros_empty:<rx>;` server to client: everything has been sent.

    TCI text can't contain its reserved characters, so `:` `,` `;` travel
    as `^` `~` `*`.

    Two extensions, and TCI's own keying command:

    - `hpsdr_cw_messages:<text>,<text>,...;` server to client after
      hpsdr_hello, and when they change: the radio's saved CW messages
      (Settings -> CW), encoded the same way, an empty one as nothing.
    - `hpsdr_key:<time>,<down>,<delay>;` client to server: live keying
      from a key or paddles. `time` is when the key went down or up, in ms
      on the client's own clock; the server plays each transition `delay`
      ms after the first of a burst, keeping the client's spacing exactly,
      so network jitter up to `delay` doesn't touch the timing. While the
      key is down the client repeats its state every KEY_REPEAT_MS; the
      server lets go if it hears nothing for KEY_TIMEOUT_MS.
    - `hpsdr_paddle_keys:<dit>,<dah>;` server to client after hpsdr_hello,
      and when they change: the keys a key or paddle plugged into the
      client as a keyboard types (Settings -> CW), encoded the same way.
    - TCI's own `keyer:<rx>,<true|false>[,<ms>];` is taken too, for other
      TCI programs: down on arrival, up `ms` after it went down.
*/

use crate::tci::Command;

pub const MACROS: &str = "cw_macros";
pub const MACROS_STOP: &str = "cw_macros_stop";
pub const MACROS_SPEED: &str = "cw_macros_speed";
pub const MACROS_EMPTY: &str = "cw_macros_empty";
pub const MESSAGES: &str = "hpsdr_cw_messages";
pub const KEY: &str = "hpsdr_key";
pub const KEYER: &str = "keyer";
pub const PADDLE_KEYS: &str = "hpsdr_paddle_keys";

/// The keys a paddle plugged in as a keyboard may type: ones on a US
/// keyboard typed without shift, so a key-up always matches its key-down.
pub const PADDLE_KEY_CHOICES: &str = "[];',./\\-=`abcdefghijklmnopqrstuvwxyz0123456789";
pub const PADDLE_DIT_DEFAULT: char = '[';
pub const PADDLE_DAH_DEFAULT: char = ']';

/// The playout delay a client may ask for, ms.
pub const KEY_DELAY_MIN_MS: u32 = 50;
pub const KEY_DELAY_MAX_MS: u32 = 1_000;
pub const KEY_DELAY_DEFAULT_MS: u32 = 150;
/// While the key is down the client repeats it this often; the server
/// lets go if it hears nothing for KEY_TIMEOUT_MS.
pub const KEY_REPEAT_MS: u32 = 500;
pub const KEY_TIMEOUT_MS: u32 = 1_500;

/// Speeds a server accepts, WPM (hpsdr-rs's own CW speed range).
pub const SPEED_MIN: u32 = 1;
pub const SPEED_MAX: u32 = 60;

/// `text` with TCI's reserved characters replaced, to send.
pub fn encode(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            ':' => '^',
            ',' => '~',
            ';' => '*',
            c => c,
        })
        .collect()
}

/// The text a client sent, its reserved characters restored.
pub fn decode(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '^' => ':',
            '~' => ',',
            '*' => ';',
            c => c,
        })
        .collect()
}

/// `cw_macros:0,<text>;`
pub fn macros(text: &str) -> Command {
    Command::new(MACROS, &["0", &encode(text)])
}

/// The text of a `cw_macros` command: everything after the receiver,
/// decoded. None if it isn't one or has no text.
pub fn macros_text(cmd: &Command) -> Option<String> {
    if cmd.name != MACROS || cmd.args.len() < 2 {
        return None;
    }
    let text = decode(&cmd.args[1..].join(","));
    (!text.trim().is_empty()).then_some(text)
}

pub fn stop() -> Command {
    Command::new(MACROS_STOP, &[])
}

pub fn speed(wpm: u32) -> Command {
    Command::new(MACROS_SPEED, &[&wpm.to_string()])
}

pub fn empty() -> Command {
    Command::new(MACROS_EMPTY, &["0"])
}

pub fn messages(list: &[String]) -> Command {
    let encoded: Vec<String> = list.iter().map(|m| encode(m)).collect();
    let refs: Vec<&str> = encoded.iter().map(String::as_str).collect();
    Command::new(MESSAGES, &refs)
}

/// `hpsdr_key:<time>,<down>,<delay>;`
pub fn key(time_ms: u64, down: bool, delay_ms: u32) -> Command {
    Command::new(KEY, &[&time_ms.to_string(), &down.to_string(), &delay_ms.to_string()])
}

/// A key transition: (time ms, down, delay ms), the delay clamped to
/// what's allowed. None if it isn't one.
pub fn key_from(cmd: &Command) -> Option<(u64, bool, u32)> {
    if cmd.name != KEY || cmd.args.len() < 3 {
        return None;
    }
    let delay: u32 = cmd.arg(2)?;
    Some((cmd.arg(0)?, cmd.args[1] == "true", delay.clamp(KEY_DELAY_MIN_MS, KEY_DELAY_MAX_MS)))
}

/// `hpsdr_paddle_keys:<dit>,<dah>;`
pub fn paddle_keys(dit: char, dah: char) -> Command {
    Command::new(PADDLE_KEYS, &[&encode(&dit.to_string()), &encode(&dah.to_string())])
}

/// The paddle keys -- (dit, dah) -- from an `hpsdr_paddle_keys` command,
/// each one of PADDLE_KEY_CHOICES (lower case) or its default.
pub fn paddle_keys_from(cmd: &Command) -> Option<(char, char)> {
    if cmd.name != PADDLE_KEYS {
        return None;
    }
    let key = |i: usize, default: char| {
        let text = decode(cmd.args.get(i).map(String::as_str).unwrap_or("")).to_lowercase();
        let mut chars = text.chars();
        match (chars.next(), chars.next()) {
            (Some(c), None) if PADDLE_KEY_CHOICES.contains(c) => c,
            _ => default,
        }
    };
    Some((key(0, PADDLE_DIT_DEFAULT), key(1, PADDLE_DAH_DEFAULT)))
}

/// The saved messages from an `hpsdr_cw_messages` command.
pub fn messages_from(cmd: &Command) -> Option<Vec<String>> {
    (cmd.name == MESSAGES).then(|| cmd.args.iter().map(|a| decode(a)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tci::parse;

    #[test]
    fn text_survives_the_reserved_characters() {
        let text = "CQ DE G0ORX: 5NN, TU; K";
        let sent = macros(text).to_string();
        assert_eq!(sent, "cw_macros:0,CQ DE G0ORX^ 5NN~ TU* K;");
        assert_eq!(macros_text(&parse(&sent)[0]).as_deref(), Some(text));
        // A client that didn't encode its commas still gets them through.
        assert_eq!(macros_text(&parse("cw_macros:0,TU,73;")[0]).as_deref(), Some("TU,73"));
        assert_eq!(macros_text(&parse("cw_macros:0, ;")[0]), None);
        assert_eq!(stop().to_string(), "cw_macros_stop;");
        assert_eq!(speed(25).to_string(), "cw_macros_speed:25;");
        assert_eq!(empty().to_string(), "cw_macros_empty:0;");
    }

    #[test]
    fn key_round_trips() {
        let sent = key(123_456, true, 150).to_string();
        assert_eq!(sent, "hpsdr_key:123456,true,150;");
        assert_eq!(key_from(&parse(&sent)[0]), Some((123_456, true, 150)));
        assert_eq!(key_from(&parse("hpsdr_key:5,false,9999;")[0]), Some((5, false, KEY_DELAY_MAX_MS)));
        assert_eq!(key_from(&parse("hpsdr_key:5,false;")[0]), None);
    }

    #[test]
    fn paddle_keys_round_trip() {
        let sent = paddle_keys(',', ';').to_string();
        assert_eq!(sent, "hpsdr_paddle_keys:~,*;");
        assert_eq!(paddle_keys_from(&parse(&sent)[0]), Some((',', ';')));
        assert_eq!(paddle_keys_from(&parse("hpsdr_paddle_keys:Z,{;")[0]), Some(('z', PADDLE_DAH_DEFAULT)));
        assert_eq!(paddle_keys_from(&parse("hpsdr_paddle_keys;")[0]), Some((PADDLE_DIT_DEFAULT, PADDLE_DAH_DEFAULT)));
    }

    #[test]
    fn messages_round_trip() {
        let list = vec!["CQ TEST G0ORX".to_string(), String::new(), "5NN, TU".to_string()];
        let sent = messages(&list).to_string();
        assert_eq!(messages_from(&parse(&sent)[0]), Some(list));
    }
}
