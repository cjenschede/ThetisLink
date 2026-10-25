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

/// One line from the Kotlin side, into the same log the Rust side writes.
///
/// Everything on the Bluetooth button went to the system log alone, so the
/// owner could never see any of it and a diagnosis needed a cable attached -
/// and a cable suppresses the very power saving the overnight test is meant to
/// measure. The logger installed at start writes to the file *and* the system
/// log, so one call across the bridge puts both back in reach (review finding,
/// a review).
pub fn log_line(text: String) {
    log::info!("{}", text);
}

/// The developer mode on the phone (build 298; owner decision 2026-10-23):
/// the switch the logger reads, and the line that says so - at every start and
/// at every change. What the mode is (the code, the rule) is
/// `sdr_remote_core::dev_mode`; the app stores it with its settings.
pub fn set_developer_mode(on: bool) {
    sdr_remote_core::dev_mode::set(on);
    // Named in advance (`logregels-build298.md`).
    log::info!("{}", sdr_remote_core::dev_mode::state_line(on));
}

/// Does what was typed open the developer mode? The core's one code, so the
/// app does not hold a copy of it.
pub fn developer_code_matches(code: String) -> bool {
    sdr_remote_core::dev_mode::code_matches(&code)
}

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

/// Put a copy of the log next to a measurement (owner, 2026-10-03): the log
/// lives in the app's private folder, which no one can fetch without rooting the
/// phone - deliberately, it is not debuggable - while the measurement folder can
/// be fetched over USB. The older generation first, then the current one, so the
/// copy reads in time order. Says what it did not find rather than failing.
pub fn copy_log_to(dest: &std::path::Path) -> std::io::Result<()> {
    let dir = match DIR.lock().map(|d| d.clone()) {
        Ok(Some(p)) => p,
        _ => return std::fs::write(dest, "(no log file: logging was never started)
"),
    };
    copy_log_from(&dir, dest)
}

/// When to copy the log: the moment a measurement file is no longer the
/// running one - stopped by the operator, ended by a write error, or replaced
/// by a new one. Following the state rather than the Stop button is what
/// catches the write error, where there is no Stop to press, and it copies
/// AFTER the engine has stopped, so the log holds the end of its own
/// measurement (round 91).
#[derive(Default)]
pub struct MeasurementLogFollower {
    last: Option<String>,
}

impl MeasurementLogFollower {
    /// Feed the measurement file the state names now. Returns where to put the
    /// log copy when a measurement has just ended.
    pub fn step(&mut self, now: Option<&str>) -> Option<std::path::PathBuf> {
        if self.last.as_deref() == now {
            return None;
        }
        let ended = self.last.take().map(|f| std::path::Path::new(&f).with_extension("log"));
        self.last = now.map(str::to_string);
        ended
    }
}

fn copy_log_from(dir: &std::path::Path, dest: &std::path::Path) -> std::io::Result<()> {
    // Both opened BEFORE either is copied: a roll in between (`.log` renamed to
    // `.log.1`) would otherwise leave the copy with the older generation twice
    // and the newest lines missing. An open handle keeps the file it opened
    // across a rename, so this fixes one moment (round 92).
    let sources: Vec<std::fs::File> = [format!("{FILE_NAME}.1"), FILE_NAME.to_string()]
        .iter()
        .filter_map(|name| std::fs::File::open(dir.join(name)).ok())
        .collect();
    let mut out = std::fs::File::create(dest)?;
    for mut f in sources {
        std::io::copy(&mut f, &mut out)?;
    }
    Ok(())
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
            metadata.level() <= Level::Info && sdr_remote_core::dev_mode::record_passes(metadata.target())
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

    /// The copy next to a measurement holds both generations, oldest first.
    #[test]
    fn the_copy_holds_both_generations_in_order() {
        let dir = std::env::temp_dir().join("tl-log-copy-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut sink = Sink::open(&dir).unwrap();
        sink.write_line("older line");
        sink.written = MAX_BYTES;
        sink.write_line("newer line");
        let dest = dir.join("tl-meting-x.log");
        copy_log_from(&dir, &dest).unwrap();
        let text = std::fs::read_to_string(&dest).unwrap();
        let (a, b) = (text.find("older line").unwrap(), text.find("newer line").unwrap());
        assert!(a < b, "{text}");
    }

    /// The copy follows the state: once when a measurement ends - by Stop or
    /// by a write error, both just "no file any more" - and once for a file
    /// replaced by a new one. Nothing while one runs, nothing when none ran.
    #[test]
    fn the_log_is_copied_once_when_a_measurement_ends() {
        let mut f = MeasurementLogFollower::default();
        assert_eq!(f.step(None), None, "nothing ran");
        assert_eq!(f.step(Some("/m/tl-meting-a.csv")), None, "started");
        assert_eq!(f.step(Some("/m/tl-meting-a.csv")), None, "running");
        assert_eq!(f.step(None), Some(std::path::PathBuf::from("/m/tl-meting-a.log")), "ended");
        assert_eq!(f.step(None), None, "and only once");
        f.step(Some("/m/tl-meting-b.csv"));
        assert_eq!(
            f.step(Some("/m/tl-meting-c.csv")),
            Some(std::path::PathBuf::from("/m/tl-meting-b.log")),
            "replaced by a new one"
        );
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

    /// A line handed in from Kotlin reaches the log path.
    ///
    /// Not a regression test - nothing was broken. It proves the half that was
    /// only asserted: `log_line` was added so the Bluetooth button would be
    /// visible without a cable, and nothing said it arrived anywhere. On the
    /// phone it could not be checked at all (the APK is not debuggable, so
    /// `run-as` refuses), which is exactly why it went unverified for a day.
    ///
    /// Off Android no logger is installed - `init_logging` is a no-op there -
    /// so this test may install one. It is the only test in this crate that
    /// does; a second would fight it, because the global logger can be set
    /// once per process.
    #[test]
    fn a_line_from_kotlin_reaches_the_log() {
        use std::sync::Mutex as StdMutex;

        static CAUGHT: StdMutex<Vec<String>> = StdMutex::new(Vec::new());

        struct Catcher;
        impl log::Log for Catcher {
            fn enabled(&self, _: &log::Metadata) -> bool {
                true
            }
            fn log(&self, record: &log::Record) {
                CAUGHT.lock().unwrap().push(record.args().to_string());
            }
            fn flush(&self) {}
        }

        // Ignore the error: another test binary in the same process may have
        // installed one already, and then the assertion below simply fails
        // loudly instead of this line panicking quietly.
        let _ = log::set_boxed_logger(Box::new(Catcher));
        log::set_max_level(log::LevelFilter::Info);

        super::log_line("BlePtt: key down (notification)".to_string());

        let caught = CAUGHT.lock().unwrap();
        assert!(
            caught.iter().any(|l| l == "BlePtt: key down (notification)"),
            "the line handed in from Kotlin never reached the logger: {caught:?}"
        );
    }

    /// The phone's logger applies the developer mode's one rule before a line
    /// reaches logcat or the file, it is the only logger the app installs, and
    /// switching the mode says so in the log (build 298). The logger runs on the
    /// phone only.
    #[test]
    fn the_phone_s_logger_holds_back_the_developer_lines() {
        let src = include_str!("logging.rs");
        let src = &sdr_remote_core::source_text::production_code(src);
        assert_eq!(
            src.matches("metadata.level() <= Level::Info && sdr_remote_core::dev_mode::record_passes(metadata.target())").count(),
            1
        );
        assert!(src.contains("fn log(&self, record: &Record) {\n            if !self.enabled(record.metadata()) {\n                return;\n            }"));
        assert!(src.contains("    sdr_remote_core::dev_mode::set(on);\n    // Named in advance (`logregels-build298.md`).\n    log::info!(\"{}\", sdr_remote_core::dev_mode::state_line(on));"));
        // One logger install in the crate's production code: this file's. The
        // test code goes out with the core's one reader (review of 299).
        let mut files = Vec::new();
        sdr_remote_core::source_text::rust_files(std::path::Path::new("src"), &mut files);
        let mut installs = Vec::new();
        for path in &files {
            let code = sdr_remote_core::source_text::production_code(&std::fs::read_to_string(path).unwrap());
            for call in ["set_boxed_logger(", "set_logger(", "android_logger::init"] {
                for _ in code.matches(call) {
                    installs.push(format!("{} {call}", path.display()));
                }
            }
        }
        assert_eq!(installs.len(), 1, "one logger, the app's: {installs:?}");
        assert!(installs[0].ends_with("logging.rs set_boxed_logger("), "{installs:?}");
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
