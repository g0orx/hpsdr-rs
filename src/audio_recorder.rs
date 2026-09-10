/*
    Optional WAV recording of the RX audio the local speaker actually
    plays -- see main.rs's "Record" toggle in the main window's
    toolbar. A pure tap fed inline from spectrum.rs's run(), same
    "cheap no-op when disabled" pattern as debug_log.rs's DebugLog, so
    the call site doesn't need its own enabled check first.

    Streams PCM straight to disk rather than buffering the whole
    recording in memory -- a long recording session shouldn't grow
    unbounded RAM usage the way, say, a Vec<f32> collecting every
    sample for the whole session would.
*/

use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Matches spectrum::DSP_RATE/audio::OUTPUT_SAMPLE_RATE -- audio_out
/// (the tap this reads the same (l, r) frames from, at the exact point
/// they're pushed to the local speaker's own queue) is always 48kHz.
const SAMPLE_RATE_HZ: u32 = 48_000;
const CHANNELS: u16 = 2;
const BITS_PER_SAMPLE: u16 = 16;

#[derive(Clone)]
pub struct AudioRecorder {
    enabled: Arc<AtomicBool>,
    state: Arc<Mutex<Option<RecordingState>>>,
}

struct RecordingState {
    file: File,
    data_bytes_written: u32,
}

impl AudioRecorder {
    pub fn new() -> Self {
        Self {
            enabled: Arc::new(AtomicBool::new(false)),
            state: Arc::new(Mutex::new(None)),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// Creates `path` (truncating any existing file of the same name --
    /// shouldn't happen in practice since recording_path() names each
    /// recording by the second it started) and writes a placeholder WAV
    /// header, fixed up with the real sizes once stop() knows them.
    pub fn start(&self, path: &PathBuf) -> Result<(), String> {
        let mut file = File::create(path).map_err(|e| e.to_string())?;
        write_wav_header_placeholder(&mut file).map_err(|e| e.to_string())?;
        *self.state.lock().unwrap() = Some(RecordingState { file, data_bytes_written: 0 });
        self.enabled.store(true, Ordering::Relaxed);
        Ok(())
    }

    /// Stops the current recording (if any), patching the WAV header's
    /// size fields now that the real length is known.
    pub fn stop(&self) {
        self.enabled.store(false, Ordering::Relaxed);
        if let Some(mut rec) = self.state.lock().unwrap().take() {
            let _ = finalize_wav_header(&mut rec.file, rec.data_bytes_written);
        }
    }

    /// Writes one (L, R) frame as 16-bit PCM -- cheap no-op if not
    /// currently recording, so call sites don't need their own check
    /// first. Silently drops the frame on a write error (e.g. disk
    /// full) rather than panicking -- the recording just ends up
    /// truncated, no worse than stopping it manually at that point.
    pub fn write_frame(&self, l: f32, r: f32) {
        if !self.is_enabled() {
            return;
        }
        let mut guard = self.state.lock().unwrap();
        let Some(rec) = guard.as_mut() else { return };
        let l16 = (l.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
        let r16 = (r.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
        let mut buf = [0u8; 4];
        buf[0..2].copy_from_slice(&l16.to_le_bytes());
        buf[2..4].copy_from_slice(&r16.to_le_bytes());
        if rec.file.write_all(&buf).is_ok() {
            rec.data_bytes_written = rec.data_bytes_written.saturating_add(buf.len() as u32);
        }
    }
}

fn write_wav_header_placeholder(file: &mut File) -> std::io::Result<()> {
    // Standard 44-byte canonical PCM WAV header. The RIFF chunk size
    // (offset 4) and data chunk size (offset 40) are placeholders --
    // real total length isn't known until stop(), since this streams
    // straight to disk rather than buffering the recording first.
    file.write_all(b"RIFF")?;
    file.write_all(&0u32.to_le_bytes())?;
    file.write_all(b"WAVE")?;
    file.write_all(b"fmt ")?;
    file.write_all(&16u32.to_le_bytes())?; // fmt chunk size (PCM)
    file.write_all(&1u16.to_le_bytes())?; // format tag: 1 = PCM
    file.write_all(&CHANNELS.to_le_bytes())?;
    file.write_all(&SAMPLE_RATE_HZ.to_le_bytes())?;
    let block_align = CHANNELS * (BITS_PER_SAMPLE / 8);
    let byte_rate = SAMPLE_RATE_HZ * block_align as u32;
    file.write_all(&byte_rate.to_le_bytes())?;
    file.write_all(&block_align.to_le_bytes())?;
    file.write_all(&BITS_PER_SAMPLE.to_le_bytes())?;
    file.write_all(b"data")?;
    file.write_all(&0u32.to_le_bytes())?;
    Ok(())
}

fn finalize_wav_header(file: &mut File, data_bytes: u32) -> std::io::Result<()> {
    // RIFF chunk size = everything after the first 8 bytes ("RIFF" +
    // this size field itself): 4("WAVE") + 8+16("fmt " chunk) +
    // 8("data" header) + the payload.
    let riff_size = 36u32.saturating_add(data_bytes);
    file.seek(SeekFrom::Start(4))?;
    file.write_all(&riff_size.to_le_bytes())?;
    file.seek(SeekFrom::Start(40))?;
    file.write_all(&data_bytes.to_le_bytes())?;
    file.flush()
}

/// Where the next recording should be written -- a timestamped .wav
/// under a `recordings` subfolder of the same per-platform settings
/// directory everything else persists to (see config::settings_dir),
/// created on demand. Named by Unix epoch seconds rather than a
/// calendar date/time string purely to avoid pulling in a date/time
/// crate for this one filename -- still unique and chronologically
/// sortable, just not human-readable at a glance in a file browser.
/// `None` if settings_dir() itself is unavailable (see its own doc
/// comment) or the recordings folder couldn't be created.
///
/// `receiver_label` (e.g. "main", "rx1") disambiguates filenames when
/// more than one receiver's own Record button gets clicked within the
/// same second -- without it, the main receiver and an extra receiver
/// (or two extra receivers) starting a recording in the same second
/// would compute the identical filename and one would silently
/// clobber the other's file mid-write.
pub fn recording_path(receiver_label: &str) -> Option<PathBuf> {
    let mut dir = crate::config::settings_dir()?;
    dir.push("recordings");
    std::fs::create_dir_all(&dir).ok()?;
    let epoch_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    dir.push(format!("hpsdr-rs_{epoch_secs}_{receiver_label}.wav"));
    Some(dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn wav_header_and_samples_round_trip() {
        let path = std::env::temp_dir().join(format!(
            "hpsdr-rs-test-{}.wav",
            std::process::id()
        ));

        let rec = AudioRecorder::new();
        assert!(!rec.is_enabled());
        rec.start(&path).expect("start");
        assert!(rec.is_enabled());

        let frames = [(0.0_f32, 0.0_f32), (1.0, -1.0), (0.5, -0.5)];
        for (l, r) in frames {
            rec.write_frame(l, r);
        }
        rec.stop();
        assert!(!rec.is_enabled());

        let mut bytes = Vec::new();
        std::fs::File::open(&path).unwrap().read_to_end(&mut bytes).unwrap();
        std::fs::remove_file(&path).ok();

        let expected_data_bytes = (frames.len() * 4) as u32;
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), 36 + expected_data_bytes);
        assert_eq!(&bytes[8..12], b"WAVE");
        assert_eq!(&bytes[12..16], b"fmt ");
        assert_eq!(u16::from_le_bytes(bytes[20..22].try_into().unwrap()), 1); // PCM
        assert_eq!(u16::from_le_bytes(bytes[22..24].try_into().unwrap()), CHANNELS);
        assert_eq!(u32::from_le_bytes(bytes[24..28].try_into().unwrap()), SAMPLE_RATE_HZ);
        assert_eq!(&bytes[36..40], b"data");
        assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()), expected_data_bytes);

        let payload = &bytes[44..];
        assert_eq!(payload.len() as u32, expected_data_bytes);
        // First frame: silence.
        assert_eq!(i16::from_le_bytes(payload[0..2].try_into().unwrap()), 0);
        assert_eq!(i16::from_le_bytes(payload[2..4].try_into().unwrap()), 0);
        // Second frame: full-scale +1.0 / -1.0.
        assert_eq!(i16::from_le_bytes(payload[4..6].try_into().unwrap()), i16::MAX);
        assert_eq!(i16::from_le_bytes(payload[6..8].try_into().unwrap()), -i16::MAX);
    }

    #[test]
    fn write_frame_is_a_no_op_when_not_recording() {
        let rec = AudioRecorder::new();
        // Must not panic (no open file, no state) and must not error --
        // this is the "cheap no-op" property call sites rely on to
        // skip their own enabled check.
        rec.write_frame(1.0, -1.0);
    }
}
