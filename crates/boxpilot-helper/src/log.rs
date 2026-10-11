//! The helper's own log: a size-capped file in the state directory, plus
//! stderr in console mode.
//!
//! It records what the helper did (starts, refusals by count, exits,
//! connections by authority, verification failures), never what it was
//! given: no config, no attachment, no secret, no line of sing-box's output,
//! and no protocol error text, which can quote what a peer sent. Those can
//! hold passwords and keys, and sing-box's logs are as private as the files
//! it reads (ADR 0006 rule 4).
//!
//! The file is opened only once the state directory has passed
//! verification: a SYSTEM process appending to a file in a folder a user
//! controls is the bug this helper exists to avoid. Until then, and in the
//! tests, lines go to stderr in console mode and nowhere otherwise.

#![forbid(unsafe_code)]

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// The log file's cap. Past it, the file becomes `helper.log.1`, replacing
/// the one before, and a new file starts: at most twice this on disk.
pub const MAX_LOG_BYTES: u64 = 1024 * 1024;

static LOG: OnceLock<Mutex<Sink>> = OnceLock::new();

struct Sink {
    file: Option<CappedFile>,
    stderr: bool,
}

struct CappedFile {
    path: PathBuf,
    file: File,
    written: u64,
    cap: u64,
}

impl CappedFile {
    fn open(path: &Path, cap: u64) -> io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let written = file.metadata()?.len();
        let mut capped = Self {
            path: path.to_owned(),
            file,
            written,
            cap,
        };
        if capped.written >= cap {
            capped.rotate()?;
        }
        Ok(capped)
    }

    fn rotate(&mut self) -> io::Result<()> {
        let mut old = self.path.clone().into_os_string();
        old.push(".1");
        fs::rename(&self.path, &old)?;
        self.file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        self.written = 0;
        Ok(())
    }

    fn write_line(&mut self, line: &str) -> io::Result<()> {
        if self.written + line.len() as u64 > self.cap {
            self.rotate()?;
        }
        self.file.write_all(line.as_bytes())?;
        self.written += line.len() as u64;
        Ok(())
    }
}

fn sink() -> &'static Mutex<Sink> {
    LOG.get_or_init(|| {
        Mutex::new(Sink {
            file: None,
            stderr: false,
        })
    })
}

/// Echo every line to stderr from now on (console mode).
pub fn echo_to_stderr() {
    sink()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .stderr = true;
}

/// Log to `path` from now on, capped at `cap` bytes. Only for a file in a
/// verified state directory.
pub fn open_file(path: &Path, cap: u64) -> io::Result<()> {
    let file = CappedFile::open(path, cap)?;
    sink()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .file = Some(file);
    Ok(())
}

/// Write one line, stamped with the UTC time. Failures are ignored: the log
/// must never be why the helper stops.
pub fn write(message: fmt::Arguments<'_>) {
    let mut sink = sink()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if sink.file.is_none() && !sink.stderr {
        return;
    }
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    let line = format!(
        "{} {}\n",
        utc_timestamp(secs),
        one_line(&message.to_string())
    );
    if sink.stderr {
        let _ = io::stderr().write_all(line.as_bytes());
    }
    if let Some(file) = &mut sink.file {
        let _ = file.write_line(&line);
    }
}

/// `text` on one line: a line break in it would let it forge another entry.
fn one_line(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// Log a line: `helper_log!("sing-box exited: {code}")`.
#[macro_export]
macro_rules! helper_log {
    ($($arg:tt)*) => {
        $crate::log::write(format_args!($($arg)*))
    };
}

/// `secs` since the Unix epoch as `YYYY-MM-DDTHH:MM:SSZ`, by Howard
/// Hinnant's days-to-civil algorithm, so the helper needs no date crate.
pub fn utc_timestamp(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rest = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3_600,
        rest % 3_600 / 60,
        rest % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;

    #[test]
    fn timestamps_are_utc() {
        assert_eq!(utc_timestamp(0), "1970-01-01T00:00:00Z");
        assert_eq!(utc_timestamp(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(utc_timestamp(1_791_460_496), "2026-10-08T11:54:56Z");
        assert_eq!(utc_timestamp(4_102_444_799), "2099-12-31T23:59:59Z");
    }

    #[test]
    fn a_message_stays_on_one_line() {
        assert_eq!(one_line("a\nb\r\tc"), "a b  c");
    }

    #[test]
    fn the_file_is_capped_and_rotated() {
        let temp = TempDir::new("log");
        let path = temp.0.join("helper.log");
        fs::write(&path, "x".repeat(40)).unwrap();
        let mut file = CappedFile::open(&path, 40).unwrap();
        assert_eq!(
            fs::read_to_string(temp.0.join("helper.log.1")).unwrap(),
            "x".repeat(40)
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "");
        file.write_line("first line\n").unwrap();
        file.write_line("second line\n").unwrap();
        file.write_line("third line is longer\n").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "third line is longer\n");
        assert_eq!(
            fs::read_to_string(temp.0.join("helper.log.1")).unwrap(),
            "first line\nsecond line\n"
        );
    }
}
