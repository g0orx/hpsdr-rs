/*
    Small optional file logger backing the "Log to file" toggles in
    Settings -> Network for rigctl/TCI (see main.rs's Network tab and
    RigctlServer/TciServer's own `logging: DebugLog` fields) -- exists
    purely to let the user capture exactly what a real client (WSJT-X,
    TCI Remote, Hamlib's own `rigctl` CLI, etc.) sends and receives,
    since neither protocol's behavior against a given client is
    otherwise observable without an external packet capture. A real
    debugging session earlier (diagnosing TCI Remote's iq_start/iq_stop
    behavior around PTT) had to work from the CLIENT's own log; this is
    the server-side equivalent.

    Deliberately scoped to the request/reply command exchange only (see
    each protocol's own handle_client for exactly what gets logged) --
    NOT the fixed connect-time handshake (doesn't vary per client, so it
    doesn't help debug one) and NOT the high-rate binary audio/IQ stream
    (would make the log file grow at audio rate for no diagnostic
    benefit). Off by default -- this is a debugging aid, not something
    that should quietly grow a file during normal use.
*/

use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Cheaply `Clone`-able (internally `Arc`-backed) so every per-client
/// handler thread of a given server shares the exact same enabled flag
/// and open file -- toggling the Settings -> Network checkbox takes
/// effect immediately for already-connected clients too, not just new
/// connections, matching how mox/demod_params are already shared the
/// same way elsewhere in these two servers.
#[derive(Clone)]
pub struct DebugLog {
    enabled: Arc<AtomicBool>,
    file: Arc<Mutex<Option<File>>>,
    path: PathBuf,
    /// Set the moment the file is opened -- log lines are timestamped
    /// relative to this (`+12.345s`) rather than wall-clock time, so no
    /// date/time-formatting dependency is needed for what's primarily
    /// useful as relative timing between messages anyway.
    opened_at: Arc<Mutex<Option<Instant>>>,
}

impl DebugLog {
    pub fn new(path: PathBuf) -> Self {
        Self {
            enabled: Arc::new(AtomicBool::new(false)),
            file: Arc::new(Mutex::new(None)),
            path,
            opened_at: Arc::new(Mutex::new(None)),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// Opens (truncating any previous run's content -- each enable
    /// starts a fresh file, since the whole point is capturing THIS
    /// session's exchange, not accumulating indefinitely) or closes the
    /// log file.
    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Relaxed);
        let mut file = self.file.lock().unwrap();
        if enabled {
            match File::create(&self.path) {
                Ok(f) => {
                    *file = Some(f);
                    *self.opened_at.lock().unwrap() = Some(Instant::now());
                }
                Err(e) => {
                    eprintln!("debug log: failed to open {}: {e}", self.path.display());
                    self.enabled.store(false, Ordering::Relaxed);
                }
            }
        } else {
            *file = None; // closes it
        }
    }

    /// Writes one timestamped line if logging is currently enabled --
    /// cheap no-op otherwise (a single relaxed atomic load), so call
    /// sites don't need their own enabled-check first.
    pub fn log(&self, line: &str) {
        if !self.is_enabled() {
            return;
        }
        let mut file = self.file.lock().unwrap();
        let Some(f) = file.as_mut() else { return };
        let elapsed = self.opened_at.lock().unwrap().map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0);
        let _ = writeln!(f, "[+{elapsed:.3}s] {line}");
    }
}

/// Where a log file with this name lives -- same directory as everything
/// else this app persists (config-*.json, ps_corr-*.dat), so it's easy
/// to find alongside them rather than wherever the app happened to be
/// launched from. `None` if settings_dir() itself is unavailable (see
/// its own doc comment).
pub fn log_path(filename: &str) -> Option<PathBuf> {
    let mut path = crate::config::settings_dir()?;
    path.push(filename);
    Some(path)
}
