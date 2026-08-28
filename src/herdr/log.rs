//! The recorder's log file.
//!
//! `sheep watch` runs for a whole day in a pane the user is also looking at.
//! Anything it prints to stdout lands in the middle of whatever else is there,
//! so the recorder writes to a file under the plugin state directory and stays
//! silent when started detached (stdout and stderr redirected). A hand-run in a
//! terminal echoes the same lines — `--verbose` forces that even when
//! redirected, and `--dry-run` prints instead of writing a file.

use anyhow::{Context, Result};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Rotate once the log passes this. One previous file is kept.
const MAX_BYTES: u64 = 4 * 1024 * 1024;

pub struct Log {
    path: PathBuf,
    echo: bool,
}

impl Log {
    /// Open (creating as needed) `<state>/logs/watch.log`.
    pub fn open(state: &Path, echo: bool) -> Result<Self> {
        let dir = state.join("logs");
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("cannot create {}", dir.display()))?;
        Ok(Self { path: dir.join("watch.log"), echo })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A logger that only ever prints. Used by `--dry-run`, which must leave
    /// nothing behind — including a log file.
    pub fn to_stdout() -> Self {
        Self { path: PathBuf::new(), echo: true }
    }

    pub fn info(&self, message: impl AsRef<str>) {
        self.write("info", message.as_ref());
    }

    pub fn warn(&self, message: impl AsRef<str>) {
        self.write("warn", message.as_ref());
    }

    fn write(&self, level: &str, message: &str) {
        let line = format!("{} {level:<4} {message}", stamp(unix_now()));
        if self.echo {
            println!("{line}");
        }
        if self.path.as_os_str().is_empty() {
            return;
        }
        self.rotate();
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&self.path) {
            let _ = writeln!(file, "{line}");
        }
    }

    fn rotate(&self) {
        let Ok(meta) = std::fs::metadata(&self.path) else {
            return;
        };
        if meta.len() < MAX_BYTES {
            return;
        }
        let _ = std::fs::rename(&self.path, self.path.with_extension("log.1"));
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `YYYY-MM-DD HH:MM:SSZ`. UTC, because the alternative is a dependency that
/// needs a C toolchain and the binary has to cross-compile without one.
fn stamp(unix: u64) -> String {
    let (year, month, day) = civil_from_days((unix / 86_400) as i64);
    let secs = unix % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}Z",
        secs / 3600,
        (secs / 60) % 60,
        secs % 60
    )
}

/// Howard Hinnant's `civil_from_days`, days since 1970-01-01 to a date.
fn civil_from_days(days: i64) -> (i64, u64, u64) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 { shifted } else { shifted - 146_096 } / 146_097;
    let day_of_era = (shifted - era * 146_097) as u64;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era as i64 + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 { shifted_month + 3 } else { shifted_month - 9 };
    (if month <= 2 { year + 1 } else { year }, month, day)
}
