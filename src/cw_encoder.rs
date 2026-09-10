/*
    Text -> Morse timing, for the "send canned CW text" feature
    (Settings -> CW's 5 message slots, sent via the main window's Send
    CW control). The DECODE direction (received tone durations -> text)
    lives in cw_decoder.rs -- deliberately separate, since this module
    has no dependency on audio/DSP at all, just string -> timing.

    Element/gap lengths follow the standard international Morse timing
    convention (dot = 1 unit, dash = 3 units at "neutral" weight,
    intra-character gap = 1 unit, inter-character gap = 3 units,
    inter-word gap = 7 units), with dash length and all gaps scaled the
    same way tx.rs's CwKeyerAtomics/audio.rs's IambicSimulator already
    do (see text_to_elements's own doc comment for the exact formulas,
    shared with deskHPSDR's keyer_update() for weight/speed scaling).
*/

/// Morse code for one character (letters, digits, common punctuation),
/// as a string of '.' (dot) and '-' (dash). `None` for anything not in
/// the standard set (including whitespace, handled separately by the
/// caller as a word gap) -- unsupported characters are silently
/// skipped by `text_to_elements` rather than erroring, since a typo or
/// stray symbol in a saved message shouldn't block sending the rest of
/// it.
fn morse_pattern(c: char) -> Option<&'static str> {
    match c.to_ascii_uppercase() {
        'A' => Some(".-"),
        'B' => Some("-..."),
        'C' => Some("-.-."),
        'D' => Some("-.."),
        'E' => Some("."),
        'F' => Some("..-."),
        'G' => Some("--."),
        'H' => Some("...."),
        'I' => Some(".."),
        'J' => Some(".---"),
        'K' => Some("-.-"),
        'L' => Some(".-.."),
        'M' => Some("--"),
        'N' => Some("-."),
        'O' => Some("---"),
        'P' => Some(".--."),
        'Q' => Some("--.-"),
        'R' => Some(".-."),
        'S' => Some("..."),
        'T' => Some("-"),
        'U' => Some("..-"),
        'V' => Some("...-"),
        'W' => Some(".--"),
        'X' => Some("-..-"),
        'Y' => Some("-.--"),
        'Z' => Some("--.."),
        '0' => Some("-----"),
        '1' => Some(".----"),
        '2' => Some("..---"),
        '3' => Some("...--"),
        '4' => Some("....-"),
        '5' => Some("....."),
        '6' => Some("-...."),
        '7' => Some("--..."),
        '8' => Some("---.."),
        '9' => Some("----."),
        '.' => Some(".-.-.-"),
        ',' => Some("--..--"),
        '?' => Some("..--.."),
        '\'' => Some(".----."),
        '!' => Some("-.-.--"),
        '/' => Some("-..-."),
        '(' => Some("-.--."),
        ')' => Some("-.--.-"),
        '&' => Some(".-..."),
        ':' => Some("---..."),
        ';' => Some("-.-.-."),
        '=' => Some("-...-"),
        '+' => Some(".-.-."),
        '-' => Some("-....-"),
        '_' => Some("..--.-"),
        '"' => Some(".-..-."),
        '$' => Some("...-..-"),
        '@' => Some(".--.-."),
        _ => None,
    }
}

/// Converts `text` into a sequence of (keyed: bool, duration_samples)
/// elements at `sample_rate`, timed for `speed_wpm`/`weight` -- the
/// SAME two CwKeyerAtomics settings (Settings -> CW) that configure the
/// radio's own internal keyer, so a sent text message runs at
/// whatever speed/weight is currently dialed in there, matching the
/// user's own request ("Send at the speed defined in the CW tab").
///
/// Dot length follows the standard PARIS-timing formula (dot_seconds =
/// 1.2/wpm); dash length is dot_samples scaled by weight/50 * 3 (weight
/// 50 is "neutral", giving the standard 3:1 dash:dot ratio) -- the same
/// relationship deskHPSDR's own keyer_update() computes for its
/// 48kHz-fixed dot_samples/dash_samples (57600/wpm and 3456*weight/wpm
/// respectively; 3456*weight/57600 = weight*0.06, confirming the ratio
/// used here), just generalized to any sample_rate instead of a
/// hardcoded 48000 -- this needs to run at whatever DUC rate the
/// current protocol/board actually transmits at (48kHz on Protocol 1,
/// 192kHz on Protocol 2).
///
/// Words are split on whitespace; consecutive/leading/trailing
/// whitespace collapses to nothing extra (no doubled word gaps, no
/// leading gap before the first character). Unsupported characters
/// (anything morse_pattern doesn't recognize) are silently dropped.
pub fn text_to_elements(text: &str, speed_wpm: u32, weight: u32, sample_rate: u32) -> Vec<(bool, u32)> {
    let dot = ((sample_rate as f64 * 1.2 / speed_wpm.max(1) as f64).round() as u32).max(1);
    let dash = ((dot as f64 * weight.max(1) as f64 * 0.06).round() as u32).max(1);

    let mut elements = Vec::new();
    let mut first_word = true;
    for word in text.split_whitespace() {
        // Word gap replaces (not adds to) the inter-character gap that
        // would otherwise follow the previous word's last character --
        // there is no gap at all before the very first word.
        if !first_word {
            elements.push((false, dot * 7));
        }
        first_word = false;

        let mut first_char_in_word = true;
        for ch in word.chars() {
            let Some(pattern) = morse_pattern(ch) else { continue };
            if !first_char_in_word {
                elements.push((false, dot * 3));
            }
            first_char_in_word = false;

            for (i, symbol) in pattern.chars().enumerate() {
                if i > 0 {
                    elements.push((false, dot));
                }
                elements.push((true, if symbol == '-' { dash } else { dot }));
            }
        }
    }
    elements
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_letter_e_is_one_dot() {
        let el = text_to_elements("E", 20, 50, 48_000);
        let dot = (48_000.0_f64 * 1.2 / 20.0).round() as u32;
        assert_eq!(el, vec![(true, dot)]);
    }

    #[test]
    fn letter_t_is_one_dash() {
        let el = text_to_elements("T", 20, 50, 48_000);
        let dot = (48_000.0_f64 * 1.2 / 20.0).round() as u32;
        let dash = (dot as f64 * 50.0 * 0.06).round() as u32;
        assert_eq!(dash, dot * 3); // sanity: neutral weight is exactly 3:1
        assert_eq!(el, vec![(true, dash)]);
    }

    #[test]
    fn sos_has_expected_element_count_and_pattern() {
        // S = ..., O = ---, S = ... ; each letter's own elements
        // interleaved with 1-unit gaps, letters separated by 3-unit
        // gaps, no leading/trailing gap.
        let el = text_to_elements("SOS", 20, 50, 48_000);
        let on: Vec<bool> = el.iter().map(|(on, _)| *on).collect();
        assert_eq!(
            on,
            vec![
                true, false, true, false, true, // S
                false, // inter-char gap
                true, false, true, false, true, // O
                false, // inter-char gap
                true, false, true, false, true, // S
            ]
        );
    }

    #[test]
    fn word_gap_replaces_not_adds_to_char_gap() {
        let el = text_to_elements("E E", 20, 50, 48_000);
        let dot = (48_000.0_f64 * 1.2 / 20.0).round() as u32;
        // E, word-gap(7 dots), E -- no separate char-gap in between.
        assert_eq!(el, vec![(true, dot), (false, dot * 7), (true, dot)]);
    }

    #[test]
    fn extra_whitespace_does_not_duplicate_word_gaps() {
        let collapsed = text_to_elements("E  E", 20, 50, 48_000);
        let single = text_to_elements("E E", 20, 50, 48_000);
        assert_eq!(collapsed, single);
    }

    #[test]
    fn leading_and_trailing_whitespace_add_no_extra_gap() {
        let padded = text_to_elements("  E  ", 20, 50, 48_000);
        let bare = text_to_elements("E", 20, 50, 48_000);
        assert_eq!(padded, bare);
    }

    #[test]
    fn unsupported_characters_are_skipped_not_errored() {
        let el = text_to_elements("E\u{1F600}E", 20, 50, 48_000);
        let dot = (48_000.0_f64 * 1.2 / 20.0).round() as u32;
        // Treated as one "word" (emoji isn't whitespace) with only the
        // two E's recognized -- so they get an inter-CHARACTER gap
        // (3 dots), not a word gap, since split_whitespace never broke
        // this into two words.
        assert_eq!(el, vec![(true, dot), (false, dot * 3), (true, dot)]);
    }

    #[test]
    fn higher_speed_yields_shorter_elements() {
        let slow = text_to_elements("E", 10, 50, 48_000);
        let fast = text_to_elements("E", 40, 50, 48_000);
        assert!(fast[0].1 < slow[0].1);
        // Doubling speed should roughly quarter... no -- halve element
        // length (dot_seconds = 1.2/wpm is inversely proportional).
        assert!((slow[0].1 as f64 / fast[0].1 as f64 - 4.0).abs() < 0.01);
    }

    #[test]
    fn higher_weight_lengthens_dash_but_not_dot() {
        let light = text_to_elements("T", 20, 30, 48_000);
        let heavy = text_to_elements("T", 20, 70, 48_000);
        assert!(heavy[0].1 > light[0].1);
        let dot_e = text_to_elements("E", 20, 30, 48_000)[0].1;
        assert_eq!(text_to_elements("E", 20, 70, 48_000)[0].1, dot_e); // weight never affects dot length
    }

    #[test]
    fn sample_rate_scales_linearly() {
        let at_48k = text_to_elements("E", 20, 50, 48_000);
        let at_192k = text_to_elements("E", 20, 50, 192_000);
        assert_eq!(at_192k[0].1, at_48k[0].1 * 4);
    }
}
