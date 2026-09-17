//! Imports MIDI-to-CAT bindings from a Thetis "Midi2Cat" XML export
//! (Settings -> CAT/Midi -> Save As in Thetis) into this project's own
//! `midi::MidiBinding` list.
//!
//! Thetis's Midi2Cat XML is a .NET `DataSet`-serialized file: an inline
//! `<xs:schema>` block (the DataSet's own column definitions, not real
//! data) followed by one element per record, using the *controller's own
//! name* as the tag (confirmed against a real export: a Behringer "CMD
//! Studio 2A" produces `<Studio_x0020_2A>` elements -- `_x0020_` is the
//! standard XML-name-escaping for a literal space). A `Not_x0020_Saved`
//! section duplicates whatever's in the real controller-named section
//! (Thetis's own in-memory "working copy" of pending edits) and a
//! `Midi2Cat--Settings` section holds unrelated app settings (e.g.
//! `LoadedMapping`) -- both skipped here.
//!
//! Traced against the actual Thetis source
//! (`Project Files/Source/Midi2Cat/Midi2Cat.IO/MidiDevice.cs`,
//! `InCallback`/`FixBehringerCtlID`) to confirm what each field actually
//! means on the wire, since the XML alone doesn't say:
//! - `MidiControlId` is the raw MIDI byte straight off the wire --
//!   `(byte)(dwParam1 >> 8)`, i.e. a Note number for a Note On/Off
//!   message or a CC number for a Control Change message. Thetis only
//!   remaps this per-device for "CMD PL-1" and "CMD Micro"
//!   specifically (`FixBehringerCtlID`) -- NOT "CMD Studio 2A" or any
//!   other controller, so for every other device (including Studio 2A)
//!   this is exactly the CC/note number, no hidden table needed.
//! - `MidiControlType` (1 = Button, 4 = Knob_or_Slider, 5 = Wheel, per
//!   `Midi2Cat.Data.Enums.ControlType`) records the raw MIDI event
//!   category actually observed (Note vs Control Change) -- NOT
//!   necessarily the semantic knob-vs-wheel intent. A relative endless
//!   encoder sends ordinary-looking Control Change messages on the wire
//!   (there's no MIDI-level way to tell "absolute knob" from "relative
//!   encoder" from the bytes alone), so Thetis instead trusts the
//!   *assigned CAT command's own declared type*
//!   (`Midi2Cat.Data.CatCmdDb`'s `CatCommandAttribute`) to decide how to
//!   interpret the value -- e.g. `CatCmdId=101` ("Change Freq Vfo A") is
//!   declared `ControlType.Wheel` there even though a real export's
//!   `MidiControlType` for it reads 4. This importer follows the same
//!   rule: `CAT_CMD_TABLE` below is each command's semantic kind, not
//!   the XML's own `MidiControlType` field (which this importer doesn't
//!   need to read at all as a result).
//! - `CatCmdId` is `Midi2Cat.Data.CatCmd`'s numeric value -- decoded via
//!   `CAT_CMD_TABLE` below, hand-built from that same enum
//!   (`CatCmdDb.cs`) restricted to commands with a usable hpsdr-rs
//!   `MidiAction` equivalent of the *same* binding kind (Key/Knob/
//!   Wheel). Deliberately conservative about anything that isn't an
//!   exact behavioral match -- e.g. Thetis's *absolute* Knob-type "RIT"
//!   (`CatCmdId=201`) has no equivalent at all (this project only has a
//!   *relative* `RitAdjust` wheel action) and is left out of the table
//!   entirely, reported to the user as skipped with a reason. Thetis's
//!   per-type Noise Blanker/Reduction On/Off buttons are a *partial*
//!   match instead (see `CAT_CMD_TABLE`'s own comment on those entries
//!   for the one-way-only caveat) and ARE imported, on the reasoning
//!   that one press still does the useful thing.

use crate::midi::{MidiAction, MidiBinding, MidiBindingKind, MidiEventKind};

/// One record parsed out of the XML, before CAT-command translation.
struct ThetisRecord {
    control_id: u8,
    control_name: String,
    cat_cmd_id: u32,
}

/// Thetis `CatCmd` id -> hpsdr-rs action + binding kind, restricted to
/// clean, same-kind equivalents (see this module's own doc comment).
/// `CatCmd` numeric values confirmed against
/// `Project Files/Source/Midi2Cat/Midi2Cat.Data/CatCmdDb.cs` in the
/// user's local Thetis checkout.
const CAT_CMD_TABLE: &[(u32, MidiAction, MidiBindingKind)] = &[
    // -- Buttons (Note On/Off) --
    (1, MidiAction::VfoAtoB, MidiBindingKind::Key),
    (2, MidiAction::VfoBtoA, MidiBindingKind::Key),
    (3, MidiAction::VfoSwap, MidiBindingKind::Key),
    (4, MidiAction::Split, MidiBindingKind::Key),
    (6, MidiAction::RitToggle, MidiBindingKind::Key),
    (7, MidiAction::XitToggle, MidiBindingKind::Key),
    (8, MidiAction::RitClear, MidiBindingKind::Key),
    (9, MidiAction::XitClear, MidiBindingKind::Key),
    (13, MidiAction::Mox, MidiBindingKind::Key),
    (22, MidiAction::FilterWidthUp, MidiBindingKind::Key),
    (23, MidiAction::FilterWidthDown, MidiBindingKind::Key),
    (24, MidiAction::ModeUp, MidiBindingKind::Key),
    (25, MidiAction::ModeDown, MidiBindingKind::Key),
    (26, MidiAction::VfoStepUp, MidiBindingKind::Key),
    (27, MidiAction::VfoStepDown, MidiBindingKind::Key),
    (28, MidiAction::BandUp, MidiBindingKind::Key),
    (29, MidiAction::BandDown, MidiBindingKind::Key),
    (76, MidiAction::Tune, MidiBindingKind::Key),
    (73, MidiAction::CtunToggle, MidiBindingKind::Key),
    (41, MidiAction::RxEqToggle, MidiBindingKind::Key),
    (33, MidiAction::DiversityToggle, MidiBindingKind::Key),
    (21, MidiAction::BinauralToggle, MidiBindingKind::Key),
    (44, MidiAction::SnbToggle, MidiBindingKind::Key),
    // Thetis's NB1/NR/NR2 "On Off" commands are independent TOGGLE
    // buttons (press to turn on, press again to turn off); hpsdr-rs's
    // matching actions each just SET that one state (mutually exclusive
    // with the others, per NoiseBlanker/NoiseReduction's own doc
    // comments in spectrum.rs) -- a second press does nothing, it's
    // already there. Imported anyway since one press still does the
    // useful thing (turns that stage on); bind NoiseBlankerOff/
    // NoiseReductionOff to another control to turn it back off.
    (16, MidiAction::NoiseBlankerNb, MidiBindingKind::Key),
    (19, MidiAction::NoiseReductionNr, MidiBindingKind::Key),
    (20, MidiAction::NoiseReductionNr2, MidiBindingKind::Key),
    (78, MidiAction::Band160m, MidiBindingKind::Key),
    (79, MidiAction::Band80m, MidiBindingKind::Key),
    (81, MidiAction::Band40m, MidiBindingKind::Key),
    (82, MidiAction::Band30m, MidiBindingKind::Key),
    (83, MidiAction::Band20m, MidiBindingKind::Key),
    (84, MidiAction::Band17m, MidiBindingKind::Key),
    (85, MidiAction::Band15m, MidiBindingKind::Key),
    (86, MidiAction::Band12m, MidiBindingKind::Key),
    (87, MidiAction::Band10m, MidiBindingKind::Key),
    (88, MidiAction::Band6m, MidiBindingKind::Key),
    // -- Knobs/sliders (Control Change, absolute) --
    (208, MidiAction::CwSpeed, MidiBindingKind::Knob),
    (209, MidiAction::AfGain, MidiBindingKind::Knob),
    (210, MidiAction::AgcGain, MidiBindingKind::Knob),
    (211, MidiAction::TxDrive, MidiBindingKind::Knob),
    (212, MidiAction::MicGain, MidiBindingKind::Knob),
    // -- Wheels (Control Change, relative) --
    (101, MidiAction::VfoTune, MidiBindingKind::Wheel),
    (102, MidiAction::VfoBTune, MidiBindingKind::Wheel),
    (104, MidiAction::RitAdjust, MidiBindingKind::Wheel),
    (105, MidiAction::XitAdjust, MidiBindingKind::Wheel),
];

fn lookup_cat_cmd(cat_cmd_id: u32) -> Option<(MidiAction, MidiBindingKind)> {
    CAT_CMD_TABLE.iter().find(|(id, ..)| *id == cat_cmd_id).map(|(_, action, kind)| (*action, *kind))
}

/// One converted-or-skipped line for the import summary shown to the
/// user -- `Ok` bindings have already been through `lookup_cat_cmd`;
/// `Err` carries a human-readable reason (unknown/unsupported CAT
/// command, or a value that didn't parse).
#[derive(Debug)]
pub struct ImportOutcome {
    pub control_name: String,
    pub result: Result<MidiBinding, String>,
}

#[derive(Debug)]
pub struct ImportResult {
    pub outcomes: Vec<ImportOutcome>,
}

impl ImportResult {
    pub fn imported(&self) -> impl Iterator<Item = &MidiBinding> {
        self.outcomes.iter().filter_map(|o| o.result.as_ref().ok())
    }

    pub fn imported_count(&self) -> usize {
        self.outcomes.iter().filter(|o| o.result.is_ok()).count()
    }

    pub fn skipped_count(&self) -> usize {
        self.outcomes.iter().filter(|o| o.result.is_err()).count()
    }
}

/// Parses a Thetis Midi2Cat XML export and converts every record it can
/// to an hpsdr-rs `MidiBinding`. Never fails outright for a single bad
/// record -- each record either converts or gets a skip reason, all
/// reported back via `ImportResult` for the caller to show the user.
/// Only fails the whole import if the file doesn't look like a Midi2Cat
/// export at all (no records of any kind found).
pub fn import_thetis_midi2cat(xml: &str) -> Result<ImportResult, String> {
    let records = parse_records(xml)?;
    if records.is_empty() {
        return Err("no MIDI control records found -- is this a Thetis Midi2Cat XML export?".to_string());
    }
    let mut seen_control_ids = std::collections::HashSet::new();
    let mut outcomes = Vec::new();
    for record in records {
        // The "Not_x0020_Saved" section (skipped in parse_records) would
        // otherwise duplicate every record; this also guards against a
        // controller profile that genuinely lists the same control twice.
        if !seen_control_ids.insert(record.control_id) {
            continue;
        }
        let result = match lookup_cat_cmd(record.cat_cmd_id) {
            Some((action, kind)) => Ok(MidiBinding {
                event: match kind {
                    MidiBindingKind::Key => MidiEventKind::NoteKey,
                    MidiBindingKind::Knob | MidiBindingKind::Wheel => MidiEventKind::ControlChange,
                },
                // Thetis's Studio_x0020_2A profile records no channel at
                // all (see this module's own doc comment) -- None (any
                // channel) matches its own actual matching behavior.
                channel: None,
                number: record.control_id,
                kind,
                action,
                momentary: false,
                sensitivity: 1.0,
                debounce_ms: 0,
            }),
            None => Err(format!("CAT command id {} has no hpsdr-rs equivalent", record.cat_cmd_id)),
        };
        outcomes.push(ImportOutcome { control_name: record.control_name, result });
    }
    Ok(ImportResult { outcomes })
}

/// Extracts every top-level `Studio_x0020_2A`-style record element from
/// the file, in document order -- record elements are whatever isn't
/// `<xs:schema>` (column definitions, not data), `Midi2Cat--Settings`
/// (unrelated app settings), or `Not_x0020_Saved` (a duplicate working
/// copy -- see this module's own doc comment).
fn parse_records(xml: &str) -> Result<Vec<ThetisRecord>, String> {
    let root_open = xml.find("<Midi2CatData").ok_or("not a Midi2CatData file (missing root element)")?;
    let root_close = xml.rfind("</Midi2CatData>").ok_or("not a Midi2CatData file (missing closing tag)")?;
    let body_start = xml[root_open..].find('>').map(|i| root_open + i + 1).ok_or("malformed root element")?;
    let mut body = &xml[body_start..root_close];

    let mut records = Vec::new();
    while let Some(lt) = body.find('<') {
        body = &body[lt..];
        if body.starts_with("<?") {
            let end = body.find("?>").ok_or("unterminated processing instruction")?;
            body = &body[end + 2..];
            continue;
        }
        // Grab the tag name (up to the first of '>', ' ', or '/').
        let name_end =
            body[1..].find(|c: char| c == '>' || c.is_whitespace() || c == '/').map(|i| i + 1).unwrap_or(body.len());
        let tag_name = &body[1..name_end];
        if tag_name == "xs:schema" {
            let close = format!("</{tag_name}>");
            let end = body.find(&close).ok_or("unterminated xs:schema element")?;
            body = &body[end + close.len()..];
            continue;
        }
        let open_end = body.find('>').ok_or("unterminated element")?;
        if body[..open_end].ends_with('/') {
            // Self-closing top-level element (not expected here, but
            // harmless) -- skip past it.
            body = &body[open_end + 1..];
            continue;
        }
        let close = format!("</{tag_name}>");
        let content_start = open_end + 1;
        let content_end = body[content_start..].find(&close).ok_or("unterminated element")?;
        let content = &body[content_start..content_start + content_end];
        if tag_name != "Midi2Cat--Settings" && tag_name != "Not_x0020_Saved" {
            if let Some(record) = parse_record_fields(content) {
                records.push(record);
            }
        }
        body = &body[content_start + content_end + close.len()..];
    }
    Ok(records)
}

/// Parses one record's known child fields. Returns `None` (silently
/// skipped) if `MidiControlId` or `CatCmdId` is missing/unparseable --
/// both are required for a usable binding, and Thetis's own schema
/// marks other fields `minOccurs="0"` (optional) for a reason (e.g. a
/// record with a CAT command that expects no MIDI feedback out leaves
/// `MidiOutCmd*` empty, which is fine and unrelated to whether the IN
/// side is usable).
fn parse_record_fields(xml: &str) -> Option<ThetisRecord> {
    let control_id: u8 = xml_child_text(xml, "MidiControlId")?.trim().parse().ok()?;
    let cat_cmd_id: u32 = xml_child_text(xml, "CatCmdId")?.trim().parse().ok()?;
    let control_name =
        xml_child_text(xml, "MidiControlName").unwrap_or_default().trim().to_string();
    Some(ThetisRecord { control_id, control_name, cat_cmd_id })
}

/// Text content of `<tag>...</tag>` (or `None` for a self-closing
/// `<tag />`/`<tag/>`, or absent entirely) within `xml`. Good enough for
/// Thetis's flat per-record fields, which never nest a same-named child.
fn xml_child_text(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    if let Some(start) = xml.find(&open) {
        let after = &xml[start + open.len()..];
        let close = format!("</{tag}>");
        let end = after.find(&close)?;
        return Some(xml_unescape(&after[..end]));
    }
    if xml.contains(&format!("<{tag} />")) || xml.contains(&format!("<{tag}/>")) {
        return Some(String::new());
    }
    None
}

fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0" standalone="yes"?>
<Midi2CatData>
  <xs:schema id="Midi2CatData" xmlns="" xmlns:xs="http://www.w3.org/2001/XMLSchema" xmlns:msdata="urn:schemas-microsoft-com:xml-msdata">
    <xs:element name="Midi2CatData">
      <xs:complexType>
        <xs:choice minOccurs="0" maxOccurs="unbounded">
          <xs:element name="Studio_x0020_2A">
            <xs:complexType>
              <xs:sequence>
                <xs:element name="MidiControlId" type="xs:int" />
                <xs:element name="CatCmdId" type="xs:int" minOccurs="0" />
              </xs:sequence>
            </xs:complexType>
          </xs:element>
        </xs:choice>
      </xs:complexType>
    </xs:element>
  </xs:schema>
  <Studio_x0020_2A>
    <MidiControlId>3</MidiControlId>
    <MidiControlName>VFO</MidiControlName>
    <MidiControlType>4</MidiControlType>
    <MinValue>0</MinValue>
    <MaxValue>127</MaxValue>
    <CatCmdId>101</CatCmdId>
    <MidiOutCmdDown />
    <MidiOutCmdUp />
    <MidiOutCmdSetValue />
  </Studio_x0020_2A>
  <Studio_x0020_2A>
    <MidiControlId>1</MidiControlId>
    <MidiControlName>MOX</MidiControlName>
    <MidiControlType>1</MidiControlType>
    <MinValue>0</MinValue>
    <MaxValue>127</MaxValue>
    <CatCmdId>13</CatCmdId>
    <MidiOutCmdDown />
    <MidiOutCmdUp />
    <MidiOutCmdSetValue />
  </Studio_x0020_2A>
  <Studio_x0020_2A>
    <MidiControlId>19</MidiControlId>
    <MidiControlName>RIT</MidiControlName>
    <MidiControlType>4</MidiControlType>
    <MinValue>0</MinValue>
    <MaxValue>117</MaxValue>
    <CatCmdId>201</CatCmdId>
    <MidiOutCmdDown />
    <MidiOutCmdUp />
    <MidiOutCmdSetValue />
  </Studio_x0020_2A>
  <Midi2Cat--Settings>
    <Name>LoadedMapping</Name>
    <Value>Not Saved</Value>
    <ValueType>string</ValueType>
  </Midi2Cat--Settings>
  <Not_x0020_Saved>
    <MidiControlId>3</MidiControlId>
    <MidiControlName>VFO</MidiControlName>
    <MidiControlType>4</MidiControlType>
    <MinValue>0</MinValue>
    <MaxValue>127</MaxValue>
    <CatCmdId>101</CatCmdId>
    <MidiOutCmdDown />
    <MidiOutCmdUp />
    <MidiOutCmdSetValue />
  </Not_x0020_Saved>
</Midi2CatData>
"#;

    #[test]
    fn parses_real_shape_file_ignoring_schema_and_not_saved_and_settings() {
        let records = parse_records(SAMPLE).unwrap();
        // 3 real records, NOT 4 -- the Not_x0020_Saved duplicate of the
        // VFO record must not appear (it's filtered out at parse time,
        // not just at dedup time).
        assert_eq!(records.len(), 3);
        assert_eq!(records[0].control_id, 3);
        assert_eq!(records[0].control_name, "VFO");
        assert_eq!(records[0].cat_cmd_id, 101);
    }

    #[test]
    fn vfo_knob_maps_to_wheel_kind_not_the_xmls_own_knob_type() {
        // CatCmdId=101 (ChangeFreqVfoA) is declared ControlType.Wheel in
        // Thetis's own CatCmdDb.cs even though this record's own
        // MidiControlType field reads 4 (Knob_or_Slider) -- the CAT
        // command's declared kind wins, not the raw recorded type (see
        // this module's doc comment).
        let result = import_thetis_midi2cat(SAMPLE).unwrap();
        let vfo = result.outcomes.iter().find(|o| o.control_name == "VFO").unwrap();
        let binding = vfo.result.as_ref().unwrap();
        assert_eq!(binding.kind, MidiBindingKind::Wheel);
        assert_eq!(binding.action, MidiAction::VfoTune);
        assert_eq!(binding.number, 3);
        assert_eq!(binding.channel, None);
    }

    #[test]
    fn mox_button_maps_to_key_kind() {
        let result = import_thetis_midi2cat(SAMPLE).unwrap();
        let mox = result.outcomes.iter().find(|o| o.control_name == "MOX").unwrap();
        let binding = mox.result.as_ref().unwrap();
        assert_eq!(binding.kind, MidiBindingKind::Key);
        assert_eq!(binding.event, MidiEventKind::NoteKey);
        assert_eq!(binding.action, MidiAction::Mox);
        assert_eq!(binding.number, 1);
    }

    #[test]
    fn absolute_rit_knob_is_skipped_with_a_reason_not_forced_onto_the_relative_action() {
        // CatCmdId=201 is Thetis's *absolute* Knob-type RIT control --
        // hpsdr-rs only has a *relative* RitAdjust wheel action, a
        // different behavior shape, so this must be skipped rather than
        // silently bound to something that would behave differently
        // than the original Thetis binding did.
        let result = import_thetis_midi2cat(SAMPLE).unwrap();
        let rit = result.outcomes.iter().find(|o| o.control_name == "RIT").unwrap();
        assert!(rit.result.is_err());
        assert_eq!(result.imported_count(), 2);
        assert_eq!(result.skipped_count(), 1);
    }

    #[test]
    fn empty_file_is_an_error_not_a_silent_empty_import() {
        let err = import_thetis_midi2cat("<Midi2CatData></Midi2CatData>").unwrap_err();
        assert!(err.contains("no MIDI control records"));
    }

    #[test]
    fn not_a_midi2cat_file_is_rejected() {
        assert!(import_thetis_midi2cat("<SomeOtherXml></SomeOtherXml>").is_err());
    }
}
