/*
    Raw IQ recording of an RTL-SDR / MSI.SDR's whole stream (see
    radio::WidebandTap), before any decimation -- for wideband signals
    the receiver's own span can't hold (QO-100's DATV), and for replaying
    real off-air signals through decoders offline while developing them.

    Written as a SigMF recording (https://sigmf.org): `<name>.sigmf-data`
    holds the samples as interleaved little-endian i16 I/Q (ci16_le,
    standard I/Q orientation) and `<name>.sigmf-meta` is a small JSON
    file with the sample rate and the RF centre frequency, so GNU Radio,
    inspectrum, SDR++ and the like open it directly. A retune during the
    recording starts a new capture segment in the metadata rather than
    a new file.

    Runs on its own thread so file I/O never stalls the USB receiver;
    if the disk can't keep up, the tap drops whole blocks and the
    recording says how many.
*/

use crate::radio::WidebandTap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// What the UI reads while a recording runs.
#[derive(Default)]
pub struct Progress {
    pub samples: AtomicU64,
    pub dropped_blocks: AtomicU64,
    /// Set once the thread has finished (stopped, hit its limit, or
    /// failed).
    pub finished: AtomicBool,
    pub error: Mutex<Option<String>>,
}

pub struct IqRecorder {
    pub data_path: PathBuf,
    pub rate_hz: u32,
    pub progress: Arc<Progress>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

/// One SigMF capture segment: where it starts and what it was tuned to.
struct Capture {
    sample_start: u64,
    frequency_hz: u64,
}

impl IqRecorder {
    /// Starts recording `tap` to `<base>.sigmf-data/.sigmf-meta` for at
    /// most `max_seconds`. `rf_offset_hz` (the active transverter's
    /// offset, shared with the UI) is added to the hardware frequency so
    /// the metadata names the real RF frequency.
    pub fn start(
        tap: Arc<WidebandTap>,
        base: &Path,
        max_seconds: f64,
        rf_offset_hz: Arc<AtomicI64>,
        description: String,
    ) -> Result<Self, String> {
        let data_path = base.with_extension("sigmf-data");
        let meta_path = base.with_extension("sigmf-meta");
        let file = File::create(&data_path).map_err(|e| format!("{}: {e}", data_path.display()))?;
        let rate_hz = tap.rate_hz;
        let max_samples = (max_seconds.max(0.0) * rate_hz as f64) as u64;
        let progress = Arc::new(Progress::default());
        let stop = Arc::new(AtomicBool::new(false));
        let rx = tap.attach();
        let datetime = iso8601_now();

        let thread_data_path = data_path.clone();
        let thread_progress = Arc::clone(&progress);
        let thread_stop = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            let mut writer = BufWriter::with_capacity(1 << 20, file);
            let mut captures: Vec<Capture> = Vec::new();
            let mut written: u64 = 0;
            let mut result: Result<(), String> = Ok(());
            while !thread_stop.load(Ordering::Relaxed) && written < max_samples {
                let block = match rx.recv_timeout(Duration::from_millis(200)) {
                    Ok(b) => b,
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                };
                let rf_hz = (block.center_hz as i64 + rf_offset_hz.load(Ordering::Relaxed)).max(0) as u64;
                if captures.last().is_none_or(|c| c.frequency_hz != rf_hz) {
                    captures.push(Capture { sample_start: written, frequency_hz: rf_hz });
                }
                let take = (block.samples.len() as u64).min(max_samples - written) as usize;
                let mut bytes = Vec::with_capacity(take * 4);
                for [i, q] in &block.samples[..take] {
                    bytes.extend_from_slice(&i.to_le_bytes());
                    bytes.extend_from_slice(&q.to_le_bytes());
                }
                if let Err(e) = writer.write_all(&bytes) {
                    result = Err(format!("{}: {e}", thread_data_path.display()));
                    break;
                }
                written += take as u64;
                thread_progress.samples.store(written, Ordering::Relaxed);
                thread_progress.dropped_blocks.store(tap.dropped_blocks.load(Ordering::Relaxed), Ordering::Relaxed);
            }
            tap.detach();
            let dropped = tap.dropped_blocks.load(Ordering::Relaxed);
            thread_progress.dropped_blocks.store(dropped, Ordering::Relaxed);
            if let Err(e) = writer.flush() {
                result = result.and(Err(e.to_string()));
            }
            let meta = sigmf_meta(rate_hz, &datetime, &description, dropped, &captures);
            if let Err(e) = std::fs::write(&meta_path, meta) {
                result = result.and(Err(format!("{}: {e}", meta_path.display())));
            }
            if let Err(e) = result {
                *thread_progress.error.lock().unwrap() = Some(e);
            }
            thread_progress.finished.store(true, Ordering::Release);
        });
        Ok(Self { data_path, rate_hz, progress, stop, thread: Some(thread) })
    }

    pub fn seconds(&self) -> f64 {
        self.progress.samples.load(Ordering::Relaxed) as f64 / self.rate_hz as f64
    }

    pub fn is_finished(&self) -> bool {
        self.progress.finished.load(Ordering::Acquire)
    }

    /// Stops and waits for the metadata to be written.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for IqRecorder {
    fn drop(&mut self) {
        self.stop();
    }
}

/// `<settings dir>/recordings/hpsdr-rs_<epoch>_iq` (no extension -- the
/// recorder adds the two SigMF ones).
pub fn recording_base() -> Option<PathBuf> {
    let mut dir = crate::config::settings_dir()?;
    dir.push("recordings");
    std::fs::create_dir_all(&dir).ok()?;
    let epoch_secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
    dir.push(format!("hpsdr-rs_{epoch_secs}_iq"));
    Some(dir)
}

fn sigmf_meta(rate_hz: u32, datetime: &str, description: &str, dropped_blocks: u64, captures: &[Capture]) -> String {
    let mut global = serde_json::json!({
        "core:datatype": "ci16_le",
        "core:sample_rate": rate_hz,
        "core:version": "1.0.0",
        "core:recorder": concat!("hpsdr-rs ", env!("CARGO_PKG_VERSION")),
        "core:description": description,
    });
    if dropped_blocks > 0 {
        global["core:description"] =
            format!("{description} ({dropped_blocks} USB reads dropped -- the recording has gaps)").into();
    }
    let captures: Vec<_> = captures
        .iter()
        .enumerate()
        .map(|(n, c)| {
            let mut capture = serde_json::json!({
                "core:sample_start": c.sample_start,
                "core:frequency": c.frequency_hz,
            });
            if n == 0 {
                capture["core:datetime"] = datetime.into();
            }
            capture
        })
        .collect();
    let meta = serde_json::json!({ "global": global, "captures": captures, "annotations": [] });
    serde_json::to_string_pretty(&meta).unwrap_or_default()
}

/// UTC now as `YYYY-MM-DDTHH:MM:SSZ`.
fn iso8601_now() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    iso8601(secs)
}

fn iso8601(epoch_secs: u64) -> String {
    let days = (epoch_secs / 86_400) as i64;
    let rem = epoch_secs % 86_400;
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso8601_dates() {
        assert_eq!(iso8601(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso8601(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(iso8601(1_792_841_445), "2026-10-24T11:30:45Z");
    }

    #[test]
    fn meta_has_one_capture_per_retune() {
        let caps = [
            Capture { sample_start: 0, frequency_hz: 10_491_500_000 },
            Capture { sample_start: 4_608_000, frequency_hz: 10_492_000_000 },
        ];
        let meta: serde_json::Value =
            serde_json::from_str(&sigmf_meta(4_608_000, "2026-10-24T11:30:45Z", "test", 0, &caps)).unwrap();
        assert_eq!(meta["global"]["core:datatype"], "ci16_le");
        assert_eq!(meta["global"]["core:sample_rate"], 4_608_000);
        assert_eq!(meta["captures"][1]["core:sample_start"], 4_608_000);
        assert_eq!(meta["captures"][1]["core:frequency"], 10_492_000_000u64);
        assert_eq!(meta["captures"][0]["core:datetime"], "2026-10-24T11:30:45Z");
    }
}
