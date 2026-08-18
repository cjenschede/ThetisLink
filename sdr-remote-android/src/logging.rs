// SPDX-License-Identifier: GPL-2.0-or-later

//! A log this phone keeps for itself.
//!
//! Android already sends everything to the system log, and a problem report
//! already reads it back. But that log is one ring buffer shared with the whole
//! device: on a busy phone our lines are gone in minutes, evicted by Bluetooth
//! scans and window-manager chatter. Anything that happened while nobody was
//! watching - and a fault that only appears on mobile data is exactly that - is
//! unrecoverable by the time it is noticed. It cost a tethered reproduction
//! session to find that out (2026-08-17).
//!
//! So the same lines also go to a file in the app's own storage, where nothing
//! else can push them out. The format is the desktop's, on purpose: the two
//! logs end up side by side in one problem report, and a client line that
//! cannot be laid against a server line is half a line.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Roll at this size and keep one older file. Two megabytes is roughly a long
/// evening of ordinary running, and a phone has room for it.
const MAX_BYTES: u64 = 2 * 1024 * 1024;

/// The log file, and the one rule about it: it does not grow without bound.
struct Sink {
    path: PathBuf,
    file: Option<File>,
    written: u64,
}

impl Sink {
    fn open(dir: &Path) -> Option<Self> {
        let path = dir.join(FILE_NAME);
        let file = OpenOptions::new().create(true).append(true).open(&path).ok()?;
        let written = file.metadata().map(|m| m.len()).unwrap_or(0);
        Some(Self { path, file: Some(file), written })
    }

    fn write_line(&mut self, line: &str) {
        if self.written >= MAX_BYTES {
            self.roll();
        }
        if let Some(f) = self.file.as_mut() {
            if writeln!(f, "{line}").is_ok() {
                // Flushed per line rather than buffered: the lines worth having
                // are the ones written just before something went wrong, and a
                // buffer is exactly what loses those.
                let _ = f.flush();
                self.written += line.len() as u64 + 1;
            }
        }
    }

    /// Keep one generation. The previous file is overwritten rather than
    /// numbered upwards, so this cannot fill a phone however long it runs.
    fn roll(&mut self) {
        self.file = None;
        let _ = std::fs::rename(&self.path, self.path.with_extension("log.1"));
        self.file = OpenOptions::new().create(true).append(true).open(&self.path).ok();
        self.written = 0;
    }
}

const FILE_NAME: &str = "thetislink-client.log";

static DIR: Mutex<Option<PathBuf>> = Mutex::new(None);

/// The tail of the kept log, for a problem report.
///
/// Says what is missing rather than returning an empty string: a report that
/// explains little should not be mistaken for one that arrived damaged.
pub fn log_tail() -> String {
    let dir = match DIR.lock().map(|d| d.clone()) {
        Ok(Some(p)) => p,
        Ok(None) => return "(no log file: logging was never started)".to_string(),
        Err(_) => return "(no log file: it could not be read)".to_string(),
    };
    let path = dir.join(FILE_NAME);
    // The shared reader, so the phone and the desktop cut their logs the same
    // way and neither drifts.
    match sdr_remote_core::diagnose::read_tail(&path.to_string_lossy()) {
        Some(text) => text,
        None => format!("(no log file at {})", path.display()),
    }
}

#[cfg(target_os = "android")]
mod android {
    use super::{Sink, DIR, FILE_NAME};
    use android_logger::{AndroidLogger, Config};
    use log::{Level, LevelFilter, Log, Metadata, Record};
    use std::path::PathBuf;
    use std::sync::Mutex;

    struct FileAndSystem {
        system: AndroidLogger,
        sink: Mutex<Option<Sink>>,
    }

    impl Log for FileAndSystem {
        fn enabled(&self, metadata: &Metadata) -> bool {
            metadata.level() <= Level::Info
        }

        fn log(&self, record: &Record) {
            if !self.enabled(record.metadata()) {
                return;
            }
            // Straight on to the system log as before, so anything that reads
            // logcat today notices no change.
            self.system.log(record);

            // Local time, matching what the server writes. That is the whole
            // reason the desktop log was moved off UTC: two logs in one report
            // that cannot be laid side by side are worth less than one
            // (2026-08-15).
            let line = format!(
                "[{} {}] {} - {}",
                chrono::Local::now().format("%H:%M:%S%.3f"),
                record.level(),
                record.target(),
                record.args()
            );
            if let Ok(mut guard) = self.sink.lock() {
                if let Some(sink) = guard.as_mut() {
                    sink.write_line(&line);
                }
            }
        }

        fn flush(&self) {
            self.system.flush();
        }
    }

    /// Start logging to `dir` as well as to the system log.
    ///
    /// Safe to call twice and safe to call with a directory that cannot be
    /// written: the system log keeps working either way, which is why nothing
    /// here hands the caller an error to deal with. Called from the app's own
    /// start rather than from the bridge, so what happens before a connection
    /// is in the file too - a startup fault lives exactly there.
    pub fn init(dir: String) {
        let path = PathBuf::from(&dir);
        let sink = Sink::open(&path);
        if let Ok(mut d) = DIR.lock() {
            *d = Some(path);
        }
        let logger = FileAndSystem {
            system: AndroidLogger::new(Config::default().with_tag("ThetisLink")),
            sink: Mutex::new(sink),
        };
        // Installing fails when a logger is already there, which is what a
        // second call is. Nothing to repair: the first one is running.
        if log::set_boxed_logger(Box::new(logger)).is_ok() {
            log::set_max_level(LevelFilter::Info);
            log::info!(
                "ThetisLink {} - keeping a log at {}/{}",
                sdr_remote_core::version_string(),
                dir,
                FILE_NAME
            );
        }
    }
}

#[cfg(target_os = "android")]
pub use android::init as init_logging;

/// Off Android there is no system log to pair with, and no phone to fill.
/// The entry point still exists so the binding has one shape everywhere.
#[cfg(not(target_os = "android"))]
pub fn init_logging(_dir: String) {}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cap is the point: a phone must not fill up because a session ran
    /// long. One older generation is kept, and no more.
    #[test]
    fn it_rolls_and_keeps_exactly_one_older_file() {
        let dir = std::env::temp_dir().join("tl-log-roll-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut sink = Sink::open(&dir).unwrap();
        sink.write_line("before the roll");
        sink.written = MAX_BYTES; // as if a long session had already run
        sink.write_line("after the roll");

        let older = std::fs::read_to_string(dir.join("thetislink-client.log.1"))
            .expect("the older generation should have been kept");
        assert_eq!(older.trim(), "before the roll");
        let current = std::fs::read_to_string(dir.join(FILE_NAME)).unwrap();
        assert_eq!(current.trim(), "after the roll");
    }

    /// Rolling twice must not leave three files behind.
    #[test]
    fn rolling_twice_still_leaves_two_files() {
        let dir = std::env::temp_dir().join("tl-log-roll-twice-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut sink = Sink::open(&dir).unwrap();
        for round in 0..3 {
            sink.written = MAX_BYTES;
            sink.write_line(&format!("round {round}"));
        }
        let left: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left.len(), 2, "expected two files, found {left:?}");
    }

    /// Asked before anything was started, it says so instead of coming back
    /// empty - an empty attachment reads as "the app had nothing to say".
    #[test]
    fn without_a_directory_it_says_why_there_is_nothing() {
        // DIR is process-wide; this test only holds when init has not run,
        // which is the case in the test binary.
        let t = log_tail();
        assert!(t.starts_with("(no log file"), "{t}");
    }
}
