// SPDX-License-Identifier: GPL-2.0-or-later
//! The paddle transport: an MCP2221A read as fast as its HID link allows, the
//! keyer fed from every read, and the key edges handed to the UI thread over a
//! channel.
//!
//! Step 2 of `docs/internal/DESIGN-remote-cw.md`. Only the carrying lives here:
//! the rule - what a contact means, when an element starts and how long it is -
//! is `sdr_remote_logic::cw_keyer`, shared with every front end. The keyer runs
//! *on the reader thread*, right next to the hardware, so an edge exists the
//! moment the sample that caused it has been read; the UI thread only ever
//! reads a channel, and never has a read blocking on it.
//!
//! Measured on the owner's board on 2026-09-17 (`mcp2221-test paddle`): one
//! `gpio_read` takes 1.9 ms median, 2.1 ms at the 95th percentile, so the loop
//! reads without sleeping and the board sets the pace. All four GP pins come
//! back from one transaction, so the two paddle contacts are read at the same
//! instant - what an iambic squeeze needs.
//!
//! Pins, from the design (section 5.1): GP0 = paddle contact A, GP1 = paddle
//! contact B, GP2 = straight key. The board has no pull-ups; the daughter board
//! holds each pin high and a closed contact pulls it low. A pin the operator
//! has not wired floats and follows its neighbour (measured), which is why the
//! keyer reads only the inputs that are switched on.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use mcp2221_hal::gpio::{GpioDirection, LogicLevel};
use sdr_remote_logic::commands::Command;
use sdr_remote_logic::cw_keyer::{Contacts, Edge, Keyer, KeyerSettings};
use sdr_remote_logic::sidetone::Sidetone;

/// A key-up longer than a word gap plus this margin ends a run; the next
/// key-down starts a new one, anchored afresh on the server.
pub const RUN_GAP_MARGIN_MS: u64 = 50;

/// Does a key-down after `gap_ms` of silence start a new run? A word gap is
/// seven dots.
pub fn starts_a_run(gap_ms: Option<u64>, dot_ms: u64) -> bool {
    match gap_ms {
        None => true,
        Some(g) => g > 7 * dot_ms + RUN_GAP_MARGIN_MS,
    }
}

/// How long to wait before looking for the board again after it was not
/// there, or went away.
pub const RECONNECT_DELAY: Duration = Duration::from_secs(2);
/// How often the reader reports its read-time statistics.
pub const RATE_REPORT_EVERY: Duration = Duration::from_secs(10);

/// What the reader thread reports to the UI thread.
#[derive(Debug)]
pub enum PaddleEvent {
    /// The board is open and its pins are configured; reading has begun.
    Opened,
    /// The board could not be opened, or a read failed. The reader keeps
    /// trying every `RECONNECT_DELAY`; this is sent once per loss, not per try.
    Lost(String),
    /// A key edge from the keyer, stamped in milliseconds since the board was
    /// opened, and the reader's clock at the sample that produced it - so the
    /// log can say how far behind its planned time an edge was handed on.
    Edge { edge: Edge, seen_at_ms: u64 },
    /// Read-time statistics over the last reporting window.
    Rate { reads: u32, seconds: f32, median_us: u32, p95_us: u32, max_us: u32 },
}

/// Where the reader stands, for the settings screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaddleStatus {
    Off,
    /// The thread runs but has no board.
    Looking,
    Reading,
}

pub struct PaddleManager {
    pub event_rx: mpsc::Receiver<PaddleEvent>,
    event_tx: mpsc::Sender<PaddleEvent>,
    /// The keyer's settings, shared with the reader thread, which picks up a
    /// change at its next read.
    settings: Arc<Mutex<KeyerSettings>>,
    /// The sidetone, keyed from the reader thread on every edge: the shortest
    /// path from contact to ear there is, no hop over the UI thread.
    sidetone: Arc<Sidetone>,
    /// The engine's command channel: every edge goes to the engine from the
    /// reader thread, with the length of the state it ends, and the engine
    /// sends it to the server. No hop over the UI thread here either.
    cmd_tx: tokio::sync::mpsc::UnboundedSender<Command>,
    /// The reader that is running: its own stop flag and its handle. One flag
    /// per thread, so a stop followed by a start cannot leave the old reader
    /// running on a flag that was set again.
    reader: Option<(Arc<AtomicBool>, JoinHandle<()>)>,
    /// Readers that were told to stop and not yet joined. The UI thread never
    /// joins - a HID call in flight can take longer than a frame (review
    /// finding on build 89) - the next reader joins them before it opens the
    /// board, which is also what keeps two readers off one board.
    retired: Vec<JoinHandle<()>>,
    status: PaddleStatus,
}

impl PaddleManager {
    pub fn new(settings: KeyerSettings, sidetone: Arc<Sidetone>, cmd_tx: tokio::sync::mpsc::UnboundedSender<Command>) -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            event_rx: rx,
            event_tx: tx,
            settings: Arc::new(Mutex::new(settings)),
            sidetone,
            cmd_tx,
            reader: None,
            retired: Vec::new(),
            status: PaddleStatus::Off,
        }
    }

    pub fn status(&self) -> PaddleStatus {
        self.status
    }

    pub fn set_status(&mut self, s: PaddleStatus) {
        self.status = s;
    }

    pub fn settings(&self) -> KeyerSettings {
        *self.settings.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Takes effect at the reader's next read; the element in flight keeps
    /// the length it was planned with (see the keyer).
    pub fn set_settings(&mut self, s: KeyerSettings) {
        *self.settings.lock().unwrap_or_else(|e| e.into_inner()) = s;
    }

    /// Start a reader thread. Idempotent while one is running. The new thread
    /// first waits for any retired reader to finish, so the board is never
    /// read by two threads at once - and that wait is on the new thread, not
    /// on the caller.
    pub fn start(&mut self) {
        if self.reader.is_some() {
            return;
        }
        let running = Arc::new(AtomicBool::new(true));
        let flag = Arc::clone(&running);
        let settings = Arc::clone(&self.settings);
        let sidetone = Arc::clone(&self.sidetone);
        let cmd_tx = self.cmd_tx.clone();
        let tx = self.event_tx.clone();
        let retired = std::mem::take(&mut self.retired);
        let handle = std::thread::Builder::new()
            .name("cw-paddle".into())
            .spawn(move || {
                for old in retired {
                    let _ = old.join();
                }
                run(running, settings, sidetone, cmd_tx, tx)
            })
            .expect("spawn the paddle reader");
        self.reader = Some((flag, handle));
        self.status = PaddleStatus::Looking;
    }

    /// Tell the reader to stop. Returns at once: the thread notices at its
    /// next read (~2 ms) or within one wait slice (100 ms), lifts the key if
    /// it was down - that edge still arrives on the channel - and ends. Nobody
    /// on the UI thread waits for a HID call that may be in flight.
    pub fn stop(&mut self) {
        if let Some((flag, handle)) = self.reader.take() {
            flag.store(false, Ordering::SeqCst);
            self.retired.push(handle);
        }
        // The reader lifts the key itself on its way out; this is the belt to
        // that brace, for the case where it is stuck in a HID call.
        self.sidetone.set_key(false);
        self.status = PaddleStatus::Off;
    }
}

impl Drop for PaddleManager {
    fn drop(&mut self) {
        // Only the flag: at exit the process takes the thread with it, and a
        // join here could hold the window open on a HID call that hangs.
        self.stop();
    }
}

/// The length of the state an edge ends: the time since the previous edge.
/// `None` when there is no previous edge, or when the stamp is lower than the
/// previous one - a new time base, a fresh board - where a naive subtraction
/// said `+0 ms` (field test of build 88).
pub(crate) fn gap_since(last: Option<u64>, at_ms: u64) -> Option<u64> {
    match last {
        Some(l) if at_ms >= l => Some(at_ms - l),
        _ => None,
    }
}

/// The value at quantile `q` of a sorted, non-empty slice, rounding the index
/// up: on a window of two reads the 95th percentile is the slower one, not
/// the faster (review finding on build 89).
pub(crate) fn percentile(sorted: &[Duration], q: f64) -> Duration {
    let idx = ((sorted.len() - 1) as f64 * q).ceil() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

/// A GP pin reading as a contact: closed only when the pin is a GPIO *input*
/// that reads low. A pin that is not in GPIO mode is `None`, and a pin that is
/// an output - another process on the same board could have made it one -
/// reports its own drive level, not a contact; both count as open (review
/// finding on build 89).
fn closed(pin: Option<(GpioDirection, LogicLevel)>) -> bool {
    matches!(pin, Some((GpioDirection::Input, LogicLevel::Low)))
}

/// Sleep `d` in slices, so a stop request is honoured within a slice.
fn sleep_unless_stopped(running: &AtomicBool, d: Duration) {
    let slice = Duration::from_millis(100);
    let until = Instant::now() + d;
    while running.load(Ordering::SeqCst) && Instant::now() < until {
        std::thread::sleep(slice);
    }
}

/// Open the first board and put GP0, GP1 and GP2 into GPIO input mode.
///
/// Only the upstream API of the crate is used here (no `list_devices`, no
/// `connect_with_serial`, which are additions of the vendored fork), so the
/// macOS client builds against the crates.io release without the vendor tree.
fn open_board() -> Result<mcp2221_hal::MCP2221, String> {
    use mcp2221_hal::gpio::GpioDirection;
    use mcp2221_hal::settings::{Gp0Mode, Gp1Mode, Gp2Mode};

    let dev = mcp2221_hal::MCP2221::connect().map_err(|e| format!("{e:?}"))?;
    let (_chip, mut gp) = dev.sram_read_settings().map_err(|e| format!("read settings: {e:?}"))?;
    gp.gp0_mode = Gp0Mode::Gpio;
    gp.gp0_direction = GpioDirection::Input;
    gp.gp1_mode = Gp1Mode::Gpio;
    gp.gp1_direction = GpioDirection::Input;
    gp.gp2_mode = Gp2Mode::Gpio;
    gp.gp2_direction = GpioDirection::Input;
    dev.sram_write_gp_settings(gp).map_err(|e| format!("write settings: {e:?}"))?;
    Ok(dev)
}

/// Lifts the sidetone key when the reader thread ends - by any path, a panic
/// included. The edges lift it in the ordinary course of things; this is for
/// the path nobody wrote: a thread that dies with the key down would leave
/// the tone sounding with no one left to stop it.
struct KeyLift(Arc<Sidetone>);

impl Drop for KeyLift {
    fn drop(&mut self) {
        self.0.set_key(false);
    }
}

/// The reader thread: open, read until the board goes away or the manager
/// stops, report, repeat.
fn run(
    running: Arc<AtomicBool>,
    settings: Arc<Mutex<KeyerSettings>>,
    sidetone: Arc<Sidetone>,
    cmd_tx: tokio::sync::mpsc::UnboundedSender<Command>,
    tx: mpsc::Sender<PaddleEvent>,
) {
    let _lift = KeyLift(Arc::clone(&sidetone));
    let mut lost_reported = false;
    // The previous edge's time, for the length of the state each edge ends.
    let mut last_edge_ms: Option<u64> = None;
    let forward = |edge: Edge, last: &mut Option<u64>, dot_ms: u64, cmd_tx: &tokio::sync::mpsc::UnboundedSender<Command>| {
        let gap = gap_since(*last, edge.at_ms);
        let run_start = edge.down && starts_a_run(gap, dot_ms);
        let prev_len_ms = gap.unwrap_or(0).min(u16::MAX as u64) as u16;
        *last = Some(edge.at_ms);
        let _ = cmd_tx.send(Command::CwEdge { down: edge.down, prev_len_ms, run_start });
    };
    while running.load(Ordering::SeqCst) {
        let dev = match open_board() {
            Ok(x) => x,
            Err(why) => {
                if !lost_reported {
                    let _ = tx.send(PaddleEvent::Lost(why));
                    lost_reported = true;
                }
                sleep_unless_stopped(&running, RECONNECT_DELAY);
                continue;
            }
        };
        // No `lost_reported = false` here: the read loop below only ends by
        // returning or by a read failure, which reports that loss itself and
        // sets the flag so the retries after it stay quiet.
        let _ = tx.send(PaddleEvent::Opened);

        let mut keyer = Keyer::new(*settings.lock().unwrap_or_else(|e| e.into_inner()));
        let opened = Instant::now();
        let mut times: Vec<Duration> = Vec::with_capacity(8192);
        let mut window_started = Instant::now();
        loop {
            let now_ms = opened.elapsed().as_millis() as u64;
            if !running.load(Ordering::SeqCst) {
                if let Some(edge) = keyer.reset(now_ms) {
                    sidetone.set_key(false);
                    forward(edge, &mut last_edge_ms, keyer.dot_ms(), &cmd_tx);
                    let _ = tx.send(PaddleEvent::Edge { edge, seen_at_ms: now_ms });
                }
                return;
            }
            let wanted = *settings.lock().unwrap_or_else(|e| e.into_inner());
            if wanted != *keyer.settings() {
                keyer.set_settings(wanted);
            }

            let t = Instant::now();
            let values = match dev.gpio_read() {
                Ok(v) => v,
                Err(e) => {
                    let _ = tx.send(PaddleEvent::Lost(format!("read failed: {e:?}")));
                    lost_reported = true;
                    if let Some(edge) = keyer.reset(now_ms) {
                        sidetone.set_key(false);
                        forward(edge, &mut last_edge_ms, keyer.dot_ms(), &cmd_tx);
                        let _ = tx.send(PaddleEvent::Edge { edge, seen_at_ms: now_ms });
                    }
                    break;
                }
            };
            times.push(t.elapsed());
            let now_ms = opened.elapsed().as_millis() as u64;
            let contacts = Contacts {
                a: closed(values.gp0),
                b: closed(values.gp1),
                straight: closed(values.gp2),
            };
            for edge in keyer.sample(contacts, now_ms) {
                // The ear first, then the radio, the log last.
                sidetone.set_key(edge.down);
                forward(edge, &mut last_edge_ms, keyer.dot_ms(), &cmd_tx);
                let _ = tx.send(PaddleEvent::Edge { edge, seen_at_ms: now_ms });
            }

            if window_started.elapsed() >= RATE_REPORT_EVERY && !times.is_empty() {
                times.sort();
                let _ = tx.send(PaddleEvent::Rate {
                    reads: times.len() as u32,
                    seconds: window_started.elapsed().as_secs_f32(),
                    median_us: percentile(&times, 0.5).as_micros() as u32,
                    p95_us: percentile(&times, 0.95).as_micros() as u32,
                    max_us: times[times.len() - 1].as_micros() as u32,
                });
                times.clear();
                window_started = Instant::now();
            }
        }
        sleep_unless_stopped(&running, RECONNECT_DELAY);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The board has no pull-ups and the daughter board holds a pin high, so
    /// low is closed - and a pin that is not a GPIO input at all, or that is
    /// an output, counts as open, never as a press.
    #[test]
    fn a_pin_reads_closed_only_when_it_is_a_gpio_input_reading_low() {
        assert!(closed(Some((GpioDirection::Input, LogicLevel::Low))));
        assert!(!closed(Some((GpioDirection::Input, LogicLevel::High))));
        assert!(!closed(Some((GpioDirection::Output, LogicLevel::Low))), "an output driven low is not a contact");
        assert!(!closed(None), "a pin in another mode is not a contact");
    }

    /// The gap is the time since the previous edge, and nothing when there is
    /// no previous edge or the clock started over.
    #[test]
    fn the_gap_between_edges_is_none_across_a_new_time_base() {
        assert_eq!(gap_since(None, 100), None, "no previous edge");
        assert_eq!(gap_since(Some(52), 100), Some(48));
        assert_eq!(gap_since(Some(100), 100), Some(0), "two edges in one millisecond, if that ever came through");
        assert_eq!(gap_since(Some(132964), 4), None, "a fresh board: not +0, nothing");
    }

    /// On a short window the 95th percentile must not be the fastest read.
    #[test]
    fn a_percentile_on_a_short_window_rounds_towards_the_slow_end() {
        let ms = |n: u64| Duration::from_millis(n);
        assert_eq!(percentile(&[ms(1), ms(9)], 0.95), ms(9));
        assert_eq!(percentile(&[ms(1), ms(2), ms(3)], 0.5), ms(2));
        assert_eq!(percentile(&[ms(7)], 0.95), ms(7));
        let hundred: Vec<Duration> = (1..=100).map(ms).collect();
        assert_eq!(percentile(&hundred, 0.95), ms(96));
    }

    /// The tone cannot outlive the thread that keys it: whatever way the
    /// reader ends, the key goes up.
    #[test]
    fn the_key_is_lifted_when_the_reader_ends_by_any_path() {
        let st = Arc::new(Sidetone::new(sdr_remote_logic::sidetone::SidetoneSettings::default()));
        st.set_key(true);
        {
            let _lift = KeyLift(Arc::clone(&st));
            assert!(st.key_down(), "held while the guard lives");
        }
        assert!(!st.key_down(), "lifted when the guard is dropped");
        st.set_key(true);
        let inner = Arc::clone(&st);
        let r = std::thread::spawn(move || {
            let _lift = KeyLift(inner);
            panic!("the reader died with the key down");
        })
        .join();
        assert!(r.is_err());
        assert!(!st.key_down(), "lifted even by a panic");
    }

    /// A run starts on the first edge, and again after a word gap and a bit.
    #[test]
    fn a_run_starts_after_a_word_gap() {
        assert!(starts_a_run(None, 48));
        assert!(!starts_a_run(Some(48), 48), "an element gap");
        assert!(!starts_a_run(Some(7 * 48 + RUN_GAP_MARGIN_MS), 48), "exactly the limit is still the run");
        assert!(starts_a_run(Some(7 * 48 + RUN_GAP_MARGIN_MS + 1), 48));
    }

    /// The two delays the field test reads by the clock ("opened after 4 s: two
    /// tries of 2 s", a Rate line every 10 s), pinned so they cannot drift
    /// without a test saying so - same class as DEBOUNCE_MS in the keyer.
    #[test]
    fn the_delays_are_what_the_brief_says() {
        assert_eq!(RECONNECT_DELAY, Duration::from_secs(2));
        assert_eq!(RATE_REPORT_EVERY, Duration::from_secs(10));
    }

    /// A stop request is honoured inside the reconnect wait, not after it.
    #[test]
    fn the_reconnect_wait_ends_early_when_stopped() {
        let running = AtomicBool::new(false);
        let t = Instant::now();
        sleep_unless_stopped(&running, Duration::from_secs(5));
        assert!(t.elapsed() < Duration::from_secs(1));
    }

    /// Left alone, the wait lasts its length - and no longer than one slice
    /// past it, which is what pins the slice at a fraction of a second.
    #[test]
    fn the_reconnect_wait_lasts_its_length_when_not_stopped() {
        let running = AtomicBool::new(true);
        let t = Instant::now();
        sleep_unless_stopped(&running, Duration::from_millis(150));
        let took = t.elapsed();
        assert!(took >= Duration::from_millis(150), "returned early: {took:?}");
        // 300, not 400: with 150 ms asked for, 400 let 2.5 extra slices through
        // (review finding on build 90); the real code takes ~200.
        assert!(took < Duration::from_millis(300), "a slice longer than 100 ms: {took:?}");
    }

    /// A stop that arrives mid-wait ends the wait within one slice: the promise
    /// behind "switch it off while the board is away".
    #[test]
    fn a_stop_during_the_wait_ends_it_within_one_slice() {
        let running = Arc::new(AtomicBool::new(true));
        let flag = Arc::clone(&running);
        let stopper = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            flag.store(false, Ordering::SeqCst);
        });
        let t = Instant::now();
        sleep_unless_stopped(&running, Duration::from_secs(5));
        let took = t.elapsed();
        stopper.join().unwrap();
        assert!(took < Duration::from_millis(300), "did not stop within a slice: {took:?}");
    }
}
