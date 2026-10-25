// SPDX-License-Identifier: GPL-2.0-or-later
//
// Measurement mode: a raw record of what arrived and when, for comparing the
// link before and after a change (open point 34).
//
// What the rest of the engine keeps is smoothed - a jitter estimate, a loss
// figure per ten seconds. To see what a mobile network does to the stream, and
// whether a change helped, the raw arrivals are needed: every audio packet with
// its sequence number, its sender timestamp and the moment it arrived, and every
// heartbeat round trip. Arithmetic (arrival deviation, gaps, bursts) is done
// afterwards, on the file, so this records facts and computes nothing.
//
// Off by default, and free when off: one `Option` check per packet. When on, a
// packet costs one push into a fixed ring - no allocation, no lock, no file
// access on the receive path. A thread of its own drains the ring to disk. If
// the ring is ever full, the record is dropped and COUNTED, and the count goes
// into the file: a gap in the measurement must never look like a gap in the link.
//
// Time: `mono_ms` is the engine's own clock (what the jitter buffer uses);
// `wall_ms` is that clock mapped once onto the wall clock when measuring starts.
// On the phone the cell, signal and GPS readings go into the same file, on the
// same clock (build 181), instead of a separate meter app's log beside it.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ringbuf::traits::{Consumer, Producer, Split};
use ringbuf::HeapRb;

use crate::loss_window::Stream;

/// Room for about 20 s of everything at once: six receive streams, three send
/// streams, and as many concealed frames as received ones, at 50 a second each.
/// The writer drains every 200 ms, so this is a margin for a slow disk, not a
/// working depth.
pub const RING_CAPACITY: usize = (6 + 3 + 6) * 50 * 20;
/// How often the writer drains the ring. It waits on a channel, not a sleep, so
/// stopping wakes it at once (round 90: a plain sleep held the engine's loop up
/// to this long on every Stop).
const DRAIN_EVERY: Duration = Duration::from_millis(200);

/// Which way the session runs, so two files of the same build over two
/// transports can be told apart (round 90). Also on screen
/// (`RadioState::route`). The engine's transport says which
/// (`ClientTransport::route`). Since build 133 there is only QUIC; the words
/// `relay` and `direct` of older files are no longer written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Route {
    #[default]
    Quic,
    /// QUIC through the relay (phase 3b).
    QuicRelay,
}

impl Route {
    /// The word for files and logs.
    pub fn name(self) -> &'static str {
        match self {
            Route::Quic => "quic",
            Route::QuicRelay => "quic-relay",
        }
    }

    /// The word on the connect screens, desktop and phone alike - one source, so
    /// the phone does not keep a table of its own that a rename would leave
    /// behind (review 111).
    pub fn label(self) -> &'static str {
        match self {
            Route::Quic => "QUIC",
            Route::QuicRelay => "QUIC via relay",
        }
    }

    /// The route the settings ask for. One rule for the choice at start
    /// (desktop `main.rs`, the phone's bridge) and for the "in use" line that
    /// compares against it (both connect screens): a relay that is set up means
    /// QUIC through the relay, otherwise QUIC straight to the address typed.
    /// `relay_configured` is `sdr_remote_relay::is_configured` (review 118).
    pub fn asked_for(relay_configured: bool) -> Route {
        if relay_configured { Route::QuicRelay } else { Route::Quic }
    }
}

/// Something that happened to the session, for reading the file around it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// Measuring started (first line after the header).
    Start,
    /// A session was accepted, over this route (written in the stream column).
    SessionStart(Route),
    /// The session ended (link down, disconnect, new session).
    SessionEnd,
    /// Measuring stopped (last line).
    Stop,
    /// "Relay only" switched on or off (build 137; detail `on`/`off`).
    RelayOnly(bool),
    /// "Force TCP" switched on or off (build 146; detail `on`/`off`).
    ForceTcp(bool),
    /// The way the traffic takes changed (build 139; detail `L`, `D`, `R`,
    /// `R-coupling` or `D+R`).
    Path(sdr_remote_core::path::PathKind),
}

impl Event {
    fn name(self) -> &'static str {
        match self {
            Event::Start => "start",
            Event::SessionStart(_) => "session_start",
            Event::SessionEnd => "session_end",
            Event::Stop => "stop",
            Event::RelayOnly(_) => "relay_only",
            Event::ForceTcp(_) => "force_tcp",
            Event::Path(_) => "path",
        }
    }

    /// What goes in the stream column for an event: the route, or nothing.
    fn detail(self) -> &'static str {
        match self {
            Event::SessionStart(route) => route.name(),
            Event::Path(kind) => kind.file_word(),
            Event::RelayOnly(true) | Event::ForceTcp(true) => "on",
            Event::RelayOnly(false) | Event::ForceTcp(false) => "off",
            Event::Start | Event::SessionEnd | Event::Stop => "",
        }
    }
}

/// One line of the file. Copy, so a push moves a few bytes and nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Record {
    /// An audio packet arrived: stream, sequence, sender timestamp (ms), size,
    /// and whether it carries redundancy for the frame before it (the LBRR bit,
    /// `codec::carries_redundancy`; for Thetis channel 0). Written as `1`/`0` in
    /// the last column, which is empty for this kind otherwise (build 157: with
    /// it, every lost packet says whether a rebuild was possible).
    Audio { mono_ms: u64, stream: Stream, seq: u32, ts: u32, bytes: u16, redundancy: bool },
    /// A heartbeat round trip, in ms.
    Rtt { mono_ms: u64, rtt_ms: u32 },
    /// A session event.
    Event { mono_ms: u64, event: Event },
    /// An audio packet this client sent: stream (thetis, radio1, radio2),
    /// sequence, size. A repeated send (the PTT burst) is a line of its own.
    Tx { mono_ms: u64, stream: Stream, seq: u32, bytes: u16 },
    /// A transmitter keyed (`on`) or let go: thetis, radio1 or radio2.
    Ptt { mono_ms: u64, stream: Stream, on: bool },
    /// A frame of this stream was filled in rather than received - what the
    /// operator heard instead of audio.
    Conceal { mono_ms: u64, stream: Stream },
    /// A control sent again (phase 2b), by its number.
    Repeat { mono_ms: u64, seq: u32 },
    /// A control given up after its repeats.
    GiveUp { mono_ms: u64, key: sdr_remote_core::control_ack::ControlKey },
    /// Once a second, what stands between arrival and the speaker for this
    /// stream: the jitter buffer's depth and target (frames of 20 ms), the
    /// playout ring (ms) and the jitter estimate (ms). Written as
    /// `buffer,<stream>,<depth>,<target>,<ring_ms>,<jitter_ms>` - the columns
    /// seq, ts, bytes and rtt_ms carry these four for this kind (2026-10-04,
    /// `PLAN-meting-buffers-en-omschakeltijd.md`).
    Buffer { mono_ms: u64, stream: Stream, depth: u16, target: u16, ring_ms: u16, jitter_ms: u16 },
    /// Once a second, what the playout rule did since the last line: the ring at
    /// the START of the latest tick (ms, before anything went in), the lowest
    /// start in that time, how many ticks asked for two frames and how many were
    /// skipped above the bleed-off level or gave their frame back (build 158,
    /// `after_tx::Settle`). Written as
    /// `ring,,<start_ms>,<lowest_ms>,<two_frame_ticks>,<skipped_ticks>` (build
    /// 157, `ANALYSE-ring-100ms.md`: the `buffer` line reads the ring after the
    /// write, so it cannot tell the rule's decisions apart).
    Ring { mono_ms: u64, start_ms: u16, lowest_ms: u16, two_frame_ticks: u16, skipped_ticks: u16 },
    /// What the phone says about its network and position (build 181), read
    /// about once a second while the app is on screen. Facts only: a handover
    /// is a different cell id in the next `cell` line, worked out afterwards.
    Net { mono_ms: u64, sample: NetSample },
}

/// A value the phone does not give: Android's `CellInfo.UNAVAILABLE`. Written as
/// an empty column.
pub const UNAVAILABLE: i32 = i32::MAX;
/// The same for the 64-bit cell id (`CellInfo.UNAVAILABLE_LONG`).
pub const UNAVAILABLE_LONG: i64 = i64::MAX;

/// The radio technology of a cell or signal reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tech {
    Lte,
    Nr,
    Wcdma,
    Gsm,
    Wifi,
    Other,
}

impl Tech {
    /// From the phone's name for it; anything unknown is `Other`.
    pub fn from_name(name: &str) -> Tech {
        match name {
            "lte" => Tech::Lte,
            "nr" => Tech::Nr,
            "wcdma" => Tech::Wcdma,
            "gsm" => Tech::Gsm,
            "wifi" => Tech::Wifi,
            _ => Tech::Other,
        }
    }

    /// The name in the file. A `match` with no catch-all.
    pub fn name(self) -> &'static str {
        match self {
            Tech::Lte => "lte",
            Tech::Nr => "nr",
            Tech::Wcdma => "wcdma",
            Tech::Gsm => "gsm",
            Tech::Wifi => "wifi",
            Tech::Other => "other",
        }
    }
}

/// How a 5G cell that is not the registered one is used, as Android says it
/// (`CellInfo.getCellConnectionStatus`, build 210). With 5G beside 4G (EN-DC)
/// the 4G cell is the registered one and the 5G cell, if Android lists it, is
/// a secondary serving cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellLink {
    None,
    Primary,
    Secondary,
    Unknown,
}

impl CellLink {
    /// From Android's number: `CONNECTION_NONE` 0, `CONNECTION_PRIMARY_SERVING`
    /// 1, `CONNECTION_SECONDARY_SERVING` 2, `CONNECTION_UNKNOWN` `Int.MAX_VALUE`.
    /// The phone passes it on unchanged; anything else is `Unknown`.
    pub fn from_android(code: i32) -> CellLink {
        match code {
            0 => CellLink::None,
            1 => CellLink::Primary,
            2 => CellLink::Secondary,
            _ => CellLink::Unknown,
        }
    }

    /// The name in the file. A `match` with no catch-all.
    pub fn name(self) -> &'static str {
        match self {
            CellLink::None => "none",
            CellLink::Primary => "primary",
            CellLink::Secondary => "secondary",
            CellLink::Unknown => "unknown",
        }
    }
}

/// One reading of the phone's network or position (build 181). The five
/// columns after `kind` carry it, as the `buffer` and `ring` lines do, and what
/// a column holds depends on the technology (review of 181):
/// - `cell,<tech>,<cell id>,<pci>,<arfcn>,<tac>` - the serving cell; for wcdma
///   the PSC and LAC, for gsm the BSIC and LAC in the pci and tac columns;
/// - `signal,<tech>,<rsrp dBm>,<rsrq dB>,<sinr>,<level 0-4>`; for wcdma, gsm
///   and wifi the dBm (wifi: RSSI) in the rsrp column and nothing in the next
///   two. LTE's sinr is in tenths of a dB on Android 10 and in dB from 11 (the
///   start line in the log names the version);
/// - `gps,gps,<latitude x 10^6>,<longitude x 10^6>,<km/h>,<accuracy m>`;
/// - `nrcell,<link>,<nci>,<pci>,<nr-arfcn>,<tac>` - a 5G cell Android lists but
///   not as the registered one (build 210), with how it is used (`CellLink`).
///   Its own kind and not `cell`, which readers take as the cell the phone is
///   on: a second one there every second would read as a handover.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetSample {
    Cell { tech: Tech, cell_id: i64, pci: i32, arfcn: i32, tac: i32 },
    NrCell { link: CellLink, cell_id: i64, pci: i32, arfcn: i32, tac: i32 },
    Signal { tech: Tech, rsrp: i32, rsrq: i32, sinr: i32, level: i32 },
    Gps { lat_e6: i32, lon_e6: i32, speed_kmh: i32, accuracy_m: i32 },
}

/// A number, or an empty column when the phone did not give it.
fn known(v: i32) -> String {
    if v == UNAVAILABLE { String::new() } else { v.to_string() }
}

/// The first line of every file: the columns, in order.
pub const HEADER: &str = "wall_ms,mono_ms,kind,stream,seq,ts,bytes,rtt_ms";

/// The stream's name in the file: short, no spaces. A `match` with no catch-all,
/// so a new stream does not compile until it has a name here.
fn stream_column(s: Stream) -> &'static str {
    match s {
        Stream::Thetis => "thetis",
        Stream::Radio1 => "radio1",
        Stream::Radio2 => "radio2",
        Stream::Vrx1 => "vrx1",
        Stream::Vrx2 => "vrx2",
    }
}

fn line(r: &Record, wall_base_ms: i64) -> String {
    let wall = |mono: u64| wall_base_ms + mono as i64;
    match *r {
        Record::Audio { mono_ms, stream, seq, ts, bytes, redundancy } => format!(
            "{},{},audio,{},{},{},{},{}",
            wall(mono_ms),
            mono_ms,
            stream_column(stream),
            seq,
            ts,
            bytes,
            redundancy as u8
        ),
        Record::Rtt { mono_ms, rtt_ms } => format!("{},{},rtt,,,,,{}", wall(mono_ms), mono_ms, rtt_ms),
        Record::Event { mono_ms, event } => {
            format!("{},{},{},{},,,,", wall(mono_ms), mono_ms, event.name(), event.detail())
        }
        Record::Tx { mono_ms, stream, seq, bytes } => {
            format!("{},{},tx,{},{},,{},", wall(mono_ms), mono_ms, stream_column(stream), seq, bytes)
        }
        Record::Ptt { mono_ms, stream, on } => {
            format!("{},{},ptt,{},{},,,", wall(mono_ms), mono_ms, stream_column(stream), on as u8)
        }
        Record::Conceal { mono_ms, stream } => {
            format!("{},{},conceal,{},,,,", wall(mono_ms), mono_ms, stream_column(stream))
        }
        Record::Repeat { mono_ms, seq } => format!("{},{},repeat,,{},,,", wall(mono_ms), mono_ms, seq),
        Record::GiveUp { mono_ms, key } => {
            use sdr_remote_core::control_ack::ControlKey;
            let (what, n) = match key {
                ControlKey::Control(id) => ("control", id as u32),
                ControlKey::Yaesu { control, .. } => ("yaesu", control as u32),
                ControlKey::Action { seq, .. } => ("act", seq),
                ControlKey::YaesuAction { seq, .. } => ("yaesu_act", seq),
            };
            format!("{},{},give_up,{},{},,,", wall(mono_ms), mono_ms, what, n)
        }
        Record::Buffer { mono_ms, stream, depth, target, ring_ms, jitter_ms } => format!(
            "{},{},buffer,{},{},{},{},{}",
            wall(mono_ms),
            mono_ms,
            stream_column(stream),
            depth,
            target,
            ring_ms,
            jitter_ms
        ),
        Record::Ring { mono_ms, start_ms, lowest_ms, two_frame_ticks, skipped_ticks } => format!(
            "{},{},ring,,{},{},{},{}",
            wall(mono_ms),
            mono_ms,
            start_ms,
            lowest_ms,
            two_frame_ticks,
            skipped_ticks
        ),
        Record::Net { mono_ms, sample } => match sample {
            NetSample::Cell { tech, cell_id, pci, arfcn, tac } => format!(
                "{},{},cell,{},{},{},{},{}",
                wall(mono_ms),
                mono_ms,
                tech.name(),
                if cell_id == UNAVAILABLE_LONG { String::new() } else { cell_id.to_string() },
                known(pci),
                known(arfcn),
                known(tac)
            ),
            NetSample::NrCell { link, cell_id, pci, arfcn, tac } => format!(
                "{},{},nrcell,{},{},{},{},{}",
                wall(mono_ms),
                mono_ms,
                link.name(),
                if cell_id == UNAVAILABLE_LONG { String::new() } else { cell_id.to_string() },
                known(pci),
                known(arfcn),
                known(tac)
            ),
            NetSample::Signal { tech, rsrp, rsrq, sinr, level } => format!(
                "{},{},signal,{},{},{},{},{}",
                wall(mono_ms),
                mono_ms,
                tech.name(),
                known(rsrp),
                known(rsrq),
                known(sinr),
                known(level)
            ),
            NetSample::Gps { lat_e6, lon_e6, speed_kmh, accuracy_m } => format!(
                "{},{},gps,gps,{},{},{},{}",
                wall(mono_ms),
                mono_ms,
                known(lat_e6),
                known(lon_e6),
                known(speed_kmh),
                known(accuracy_m)
            ),
        },
    }
}

struct Running {
    producer: ringbuf::HeapProd<Record>,
    dropped: Arc<AtomicU64>,
    /// Wakes the writer; a message (or the sender gone) means "drain and close".
    wake: mpsc::Sender<()>,
    /// Set by the writer when the file could not be written (full disk, storage
    /// gone): the measurement has stopped, and the engine must say so.
    failed: Arc<AtomicBool>,
    writer: Option<std::thread::JoinHandle<()>>,
    path: PathBuf,
}

impl Running {
    /// Wake the writer, let it drain and close the file, and wait for that. The
    /// wait is the drain itself (a few ms), not a sleep. Returns whether the
    /// writer had given up on a write error - so a caller (and a test) can tell
    /// a short file from a failed one (round 101).
    fn finish(mut self) -> bool {
        let _ = self.wake.send(());
        if let Some(w) = self.writer.take() {
            let _ = w.join();
        }
        self.failed.load(Ordering::Acquire)
    }
}

/// The engine's handle. `off()` until `start`; every record call is a no-op then.
pub struct Recorder {
    running: Option<Running>,
}

impl Default for Recorder {
    fn default() -> Self {
        Self::off()
    }
}

impl Recorder {
    pub fn off() -> Self {
        Self { running: None }
    }

    pub fn is_on(&self) -> bool {
        self.running.is_some()
    }

    /// The file being written, when on.
    pub fn path(&self) -> Option<&Path> {
        self.running.as_ref().map(|r| r.path.as_path())
    }

    /// Start writing a new file in `dir`, named after the wall clock to the
    /// millisecond, and never over an existing file. `engine_start` is the
    /// instant the engine's `mono_ms` counts from. Starting while already on
    /// stops the previous file first. `label` goes into the header comment (the
    /// build, so a file says which code it measured).
    pub fn start(&mut self, dir: &Path, engine_start: Instant, label: &str) -> std::io::Result<PathBuf> {
        self.stop(engine_start);
        std::fs::create_dir_all(dir)?;
        let now = chrono::Local::now();
        let path = dir.join(format!("tl-meting-{}.csv", now.format("%Y%m%d-%H%M%S-%3f")));
        let mut file = BufWriter::new(std::fs::OpenOptions::new().write(true).create_new(true).open(&path)?);
        let wall_now_ms = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64);
        let mono_now_ms = engine_start.elapsed().as_millis() as i64;
        let wall_base_ms = wall_now_ms - mono_now_ms;
        writeln!(file, "# ThetisLink measurement, {}, started {}", label, now.to_rfc3339())?;
        writeln!(file, "{HEADER}")?;
        file.flush()?;

        let (producer, consumer) = HeapRb::<Record>::new(RING_CAPACITY).split();
        let dropped = Arc::new(AtomicU64::new(0));
        let failed = Arc::new(AtomicBool::new(false));
        let (wake, woken) = mpsc::channel();
        let writer = {
            let dropped = dropped.clone();
            let failed = failed.clone();
            let shown = path.display().to_string();
            std::thread::Builder::new()
                .name("tl-measure".into())
                .spawn(move || write_loop(consumer, file, wall_base_ms, dropped, woken, failed, shown))?
        };
        self.running = Some(Running { producer, dropped, wake, failed, writer: Some(writer), path: path.clone() });
        self.record(Record::Event { mono_ms: mono_now_ms as u64, event: Event::Start });
        Ok(path)
    }

    /// Stop and close the file (the writer drains what is left first).
    /// Returns whether the writer had given up on a write error (then the file
    /// ends where the error was).
    pub fn stop(&mut self, engine_start: Instant) -> bool {
        if self.running.is_none() {
            return false;
        }
        self.record(Record::Event { mono_ms: engine_start.elapsed().as_millis() as u64, event: Event::Stop });
        self.running.take().is_some_and(|r| r.finish())
    }

    /// The writer gave up (the file could not be written): the measurement has
    /// stopped. Returns its path once, and the recorder is off from then on, so
    /// the screen stops saying it measures. Cheap - one atomic load - for a tick.
    pub fn take_failure(&mut self) -> Option<PathBuf> {
        if !self.running.as_ref().is_some_and(|r| r.failed.load(Ordering::Acquire)) {
            return None;
        }
        let r = self.running.take()?;
        let path = r.path.clone();
        r.finish();
        Some(path)
    }

    /// Push one record. Never blocks; a full ring counts a drop.
    #[inline]
    pub fn record(&mut self, r: Record) {
        if let Some(run) = &mut self.running {
            if run.producer.try_push(r).is_err() {
                run.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    #[inline]
    pub fn audio(&mut self, mono_ms: u64, stream: Stream, seq: u32, ts: u32, bytes: usize, opus: &[u8]) {
        if self.running.is_some() {
            let bytes = bytes.min(u16::MAX as usize) as u16;
            let redundancy = sdr_remote_core::codec::carries_redundancy(opus);
            self.record(Record::Audio { mono_ms, stream, seq, ts, bytes, redundancy });
        }
    }

    #[inline]
    pub fn rtt(&mut self, mono_ms: u64, rtt_ms: u32) {
        if self.running.is_some() {
            self.record(Record::Rtt { mono_ms, rtt_ms });
        }
    }

    #[inline]
    pub fn event(&mut self, mono_ms: u64, event: Event) {
        if self.running.is_some() {
            self.record(Record::Event { mono_ms, event });
        }
    }
}

impl Recorder {
    #[inline]
    pub fn tx(&mut self, mono_ms: u64, stream: Stream, seq: u32, bytes: usize) {
        if self.running.is_some() {
            let bytes = bytes.min(u16::MAX as usize) as u16;
            self.record(Record::Tx { mono_ms, stream, seq, bytes });
        }
    }

    #[inline]
    pub fn conceal(&mut self, mono_ms: u64, stream: Stream) {
        if self.running.is_some() {
            self.record(Record::Conceal { mono_ms, stream });
        }
    }

    /// The buffers of one stream, once per `BUFFER_EVERY_MS` - or at every call
    /// while `burst` (around a PTT edge, where the playout ring empties and
    /// refills within a few hundred ms and a line a second sees only the resting
    /// state; round 100). `last` is the caller's clock of the previous line.
    /// Saturating, so a nonsense value still fits its column.
    #[allow(clippy::too_many_arguments)]
    #[inline]
    pub fn buffers(
        &mut self,
        mono_ms: u64,
        last: &mut u64,
        burst: bool,
        stream: Stream,
        depth: usize,
        target: u32,
        ring_ms: u64,
        jitter_ms: f32,
    ) {
        if self.running.is_none() || !buffer_line_due(mono_ms, *last, burst) {
            return;
        }
        *last = mono_ms;
        let fit = |v: u64| v.min(u16::MAX as u64) as u16;
        self.record(Record::Buffer {
            mono_ms,
            stream,
            depth: fit(depth as u64),
            target: fit(target as u64),
            ring_ms: fit(ring_ms),
            jitter_ms: fit(jitter_ms.max(0.0).round() as u64),
        });
    }
}

impl Recorder {
    /// The playout rule's line, on the same rhythm as `buffers` (its own clock
    /// `last`), and then `ticks` starts over. Off, or with no tick since the
    /// last line: nothing is written, and the ticks are dropped when off so they
    /// do not pile up for minutes (build 157).
    #[inline]
    pub fn ring(&mut self, mono_ms: u64, last: &mut u64, burst: bool, ticks: &mut RingTicks) {
        if self.running.is_none() {
            *ticks = RingTicks::default();
            return;
        }
        if !buffer_line_due(mono_ms, *last, burst) {
            return;
        }
        let Some(lowest) = ticks.lowest_ms else {
            return;
        };
        *last = mono_ms;
        let fit = |v: u64| v.min(u16::MAX as u64) as u16;
        self.record(Record::Ring {
            mono_ms,
            start_ms: fit(ticks.start_ms),
            lowest_ms: fit(lowest),
            two_frame_ticks: fit(ticks.two_frame_ticks as u64),
            skipped_ticks: fit(ticks.skipped_ticks as u64),
        });
        *ticks = RingTicks::default();
    }
}

/// What the playout rule did since the last `ring` line, tick by tick: the
/// ring at the start of the latest tick, the lowest start, and how often the
/// rule asked for two frames or skipped the tick (build 157).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RingTicks {
    start_ms: u64,
    lowest_ms: Option<u64>,
    two_frame_ticks: u32,
    skipped_ticks: u32,
}

impl RingTicks {
    /// One tick: the ring at its start (ms), the frames the rule asked for,
    /// and whether the tick was skipped above the bleed-off level.
    pub fn tick(&mut self, start_ms: u64, frames: u32, skipped: bool) {
        self.start_ms = start_ms;
        self.lowest_ms = Some(self.lowest_ms.map_or(start_ms, |l| l.min(start_ms)));
        if skipped {
            self.skipped_ticks = self.skipped_ticks.saturating_add(1);
        } else if frames >= 2 {
            self.two_frame_ticks = self.two_frame_ticks.saturating_add(1);
        }
    }
}

/// How often the buffers are written: once a second (2026-10-04).
pub const BUFFER_EVERY_MS: u64 = 1000;

/// After a PTT edge, the buffers are written at every tick for this long
/// (round 100): the stretch in which the playout ring refills. Measured
/// 2026-10-04 (tenth measurement): the ring is back at rest within ~120-420 ms
/// and RX returns after 340-470 ms, so a window under ~500 ms misses it.
pub const BUFFER_BURST_MS: u64 = 1000;

/// Whether a buffer line is due at `now`, the previous one written at `last`:
/// always in a burst, otherwise once per `BUFFER_EVERY_MS`. The decision on its
/// own, so a test can check it without the writer thread (round 101).
fn buffer_line_due(now: u64, last: u64, burst: bool) -> bool {
    burst || now.saturating_sub(last) >= BUFFER_EVERY_MS
}

/// The playout ring's fill in ms. The ring holds items, not frames: the desktop
/// writes L and R as two items per frame (`write_playback_stereo`), Android one
/// (mono). `rate` is per channel. Round 99: without the division the desktop read
/// twice its real time. The ring is shared by all streams.
pub fn ring_ms(level_items: usize, rate: u32, stereo: bool) -> u64 {
    let per_frame = if stereo { 2 } else { 1 };
    level_items as u64 * 1000 / (rate.max(1) as u64 * per_frame)
}

/// The other way round: how many ring items hold `ms` of playout. The one place
/// that knows a desktop frame is two items, next to `ring_ms` (build 107).
pub fn ring_items(ms: u64, rate: u32, stereo: bool) -> usize {
    let per_frame = if stereo { 2 } else { 1 };
    (ms * rate as u64 / 1000) as usize * per_frame
}

/// The three transmitters, followed for their edges in ONE place in the
/// engine: the flags are set in some ten places (commands, link-down, server
/// requests, safety nets), and comparing once per loop catches every one of
/// them, including a future one.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PttSeen {
    pub thetis: bool,
    pub radio1: bool,
    pub radio2: bool,
}

impl PttSeen {
    /// Write an edge for every transmitter that changed since `self`, and become `now`.
    pub fn follow(&mut self, now: PttSeen, mono_ms: u64, rec: &mut Recorder) {
        for (was, is, stream) in [
            (self.thetis, now.thetis, Stream::Thetis),
            (self.radio1, now.radio1, Stream::Radio1),
            (self.radio2, now.radio2, Stream::Radio2),
        ] {
            if was != is {
                rec.record(Record::Ptt { mono_ms, stream, on: is });
            }
        }
        *self = now;
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        if let Some(r) = self.running.take() {
            r.finish();
        }
    }
}

/// The writer thread. Generic over the sink so a test can hand it one that
/// fails. A write or flush error is said once in the log, marks the recorder
/// failed and ends the thread - never silent (round 90).
fn write_loop<W: Write>(
    mut consumer: ringbuf::HeapCons<Record>,
    mut out: W,
    wall_base_ms: i64,
    dropped: Arc<AtomicU64>,
    woken: mpsc::Receiver<()>,
    failed: Arc<AtomicBool>,
    shown: String,
) {
    let mut dropped_written = 0u64;
    let give_up = |e: std::io::Error| {
        log::warn!("Measurement stopped: writing {} failed ({})", shown, e);
        failed.store(true, Ordering::Release);
    };
    loop {
        let stopping = match woken.recv_timeout(DRAIN_EVERY) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => true,
            Err(mpsc::RecvTimeoutError::Timeout) => false,
        };
        while let Some(r) = consumer.try_pop() {
            if let Err(e) = writeln!(out, "{}", line(&r, wall_base_ms)) {
                return give_up(e);
            }
        }
        // A drop is a hole in the measurement, not in the link: say so in the file.
        let d = dropped.load(Ordering::Relaxed);
        if d != dropped_written {
            if let Err(e) = writeln!(out, "# dropped {} records (ring full) - a hole in the measurement, not in the link", d) {
                return give_up(e);
            }
            dropped_written = d;
        }
        if let Err(e) = out.flush() {
            return give_up(e);
        }
        if stopping {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tl-measure-test-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    /// The engine hands the stereo flag to `ring_ms` and starts a burst at every
    /// PTT edge (round 100: C3 - the argument could go to `false` with every test
    /// green). Compared per trimmed line, so a commented-out call does not count.
    #[test]
    fn the_engine_feeds_the_buffer_line_as_built() {
        let src = include_str!("engine.rs");
        let lines: Vec<&str> = src.lines().map(str::trim).collect();
        let count = |l: &str| lines.iter().filter(|x| **x == l).count();
        // Three buffer lines since build 178: Thetis and the two radios, each
        // with the stereo flag, and a burst at any radio's PTT edge.
        assert_eq!(count("crate::measure::ring_ms(audio.playback_buffer_level(), playback_rate, stereo_output),"), 3);
        assert_eq!(count("let any_ptt = ptt || yaesu_ptt || yaesu2_ptt;"), 1);
        assert_eq!(count("if any_ptt != measure_ptt_was {"), 1);
        assert_eq!(count("now_ms < measure_buffers_burst_until,"), 3);
        // Build 157: every tick feeds the rule's decision to the ring line, read
        // before anything goes in, and the line goes out on the same burst.
        assert_eq!(count("let ring_start_ms = crate::measure::ring_ms(ring_level, playback_rate, stereo_output);"), 1);
        assert_eq!(count("ring_ticks.tick(ring_start_ms, frames_wanted, skip_this_tick || drain_this_tick);"), 1);
        assert_eq!(count("measure.ring(now_ms, &mut measure_ring_at, now_ms < measure_buffers_burst_until, &mut ring_ticks);"), 1);
        // And every arrival hands its Opus data for the redundancy bit.
        assert_eq!(src.matches("measure.audio(start.elapsed().as_millis() as u64, $which, $seq, $ts, $bytes, $opus);").count(), 1);
        // For the Thetis bundle that is channel 0, RX1 - the stream the loss
        // and conceal lines of `thetis` are about.
        assert_eq!(count("let rx1_opus = pkt.channels.iter().find(|(id, _)| *id == 0).map_or(&[][..], |(_, d)| d.as_slice());"), 1);
    }

    /// The rhythm on its own, without the writer thread (round 101: the file
    /// test lost its last lines once in ten under load).
    #[test]
    fn a_buffer_line_is_due_once_a_second_or_in_a_burst() {
        assert!(buffer_line_due(1000, 0, false));
        assert!(!buffer_line_due(1999, 1000, false), "within the second");
        assert!(buffer_line_due(2000, 1000, false));
        assert!(buffer_line_due(2020, 2000, true), "a burst writes at every tick");
        // After a burst the second counts from the burst's last line, not from the
        // last ordinary one (round 101: F2/M3 stayed green with 3500).
        let mut last = 0;
        for (now, burst) in [(3000, false), (3020, true), (3040, true)] {
            if buffer_line_due(now, last, burst) {
                last = now;
            }
        }
        assert_eq!(last, 3040);
        assert!(!buffer_line_due(4030, last, false), "990 ms after the burst's last line");
        assert!(buffer_line_due(4040, last, false));
        // The window is long enough to hold the refill (tenth measurement).
        assert!(BUFFER_BURST_MS >= 500, "the ring refills within ~120-420 ms; RX returns after 340-470 ms");
    }

    /// The ring's items become ms per frame, not per item (round 99).
    #[test]
    fn ring_ms_counts_frames_not_items() {
        assert_eq!(ring_ms(9600, 48_000, true), 100, "desktop: L and R are two items per frame");
        assert_eq!(ring_ms(4800, 48_000, false), 100, "Android: one item per frame");
        assert_eq!(ring_ms(0, 48_000, true), 0);
        assert_eq!(ring_ms(10, 0, false), 10_000, "a zero rate does not divide by zero");
    }

    #[test]
    fn ring_items_is_the_inverse_of_ring_ms() {
        assert_eq!(ring_items(40, 48_000, true), 3840, "desktop: two items per frame");
        assert_eq!(ring_items(40, 48_000, false), 1920, "Android: one");
        for &(ms, stereo) in &[(40u64, true), (40, false), (60, true), (0, false)] {
            assert_eq!(ring_ms(ring_items(ms, 48_000, stereo), 48_000, stereo), ms);
        }
    }

    /// The buffer line: its own meaning per column, the header's eight columns,
    /// and once a second - not every tick it is offered.
    #[test]
    fn buffers_are_a_line_a_second_with_four_numbers() {
        let dir = tmp_dir("buffers");
        let start = Instant::now();
        let mut r = Recorder::off();
        let mut last = 0;
        r.buffers(5000, &mut last, true, Stream::Thetis, 1, 3, 9, 1.0); // off: nothing, clock untouched
        assert_eq!(last, 0);
        let path = r.start(&dir, start, "test build").unwrap();
        r.buffers(1000, &mut last, false, Stream::Thetis, 4, 3, 80, 12.6);
        r.buffers(1500, &mut last, false, Stream::Thetis, 9, 9, 99, 99.0); // within the second: skipped
        r.buffers(2000, &mut last, false, Stream::Thetis, 12, 12, 200, 151.2);
        r.buffers(3000, &mut last, false, Stream::Thetis, 70_000, 1, 1, -3.0); // saturates, never negative
        // A burst writes at every call, and the rhythm then counts from its last line.
        r.buffers(3020, &mut last, true, Stream::Thetis, 0, 3, 0, 1.0);
        r.buffers(3040, &mut last, true, Stream::Thetis, 1, 3, 20, 1.0);
        r.buffers(4030, &mut last, false, Stream::Thetis, 2, 3, 40, 1.0); // skipped: 990 ms after 3040 (1030 after 3000)
        let writer_failed = r.stop(start);
        let text = std::fs::read_to_string(&path).unwrap();
        // A short file from a writer that gave up is not a rhythm fault: say which
        // it was (round 101: once in ten, under load, the last two lines were gone).
        assert!(!writer_failed, "the writer gave up on a write error - the file stops there:\n{text}");
        let lines: Vec<&str> = text.lines().filter(|l| l.contains(",buffer,")).collect();
        assert_eq!(lines.len(), 5, "{lines:?}");
        assert!(lines[3].ends_with(",3020,buffer,thetis,0,3,0,1"), "{lines:?}");
        assert!(lines[4].ends_with(",3040,buffer,thetis,1,3,20,1"), "{lines:?}");
        assert!(lines[0].ends_with(",1000,buffer,thetis,4,3,80,13"), "{lines:?}");
        assert!(lines[1].ends_with(",2000,buffer,thetis,12,12,200,151"), "{lines:?}");
        assert!(lines[2].ends_with(",3000,buffer,thetis,65535,1,1,0"), "{lines:?}");
        assert!(lines.iter().all(|l| l.split(',').count() == 8));
    }

    /// The playout rule's line: what it decided tick by tick since the last
    /// line - the latest start, the lowest start, two-frame and skipped ticks -
    /// on the buffer line's rhythm, and starting over after each line. Off, the
    /// ticks are dropped (build 157).
    #[test]
    fn the_ring_line_counts_the_rules_decisions() {
        let mut t = RingTicks::default();
        t.tick(78, 1, false);
        t.tick(58, 2, false);
        t.tick(76, 1, false);
        t.tick(210, 1, true);
        t.tick(80, 1, false);
        assert_eq!(t, RingTicks { start_ms: 80, lowest_ms: Some(58), two_frame_ticks: 1, skipped_ticks: 1 });

        let mut off = Recorder::off();
        let (mut last, mut ticks) = (0, t);
        off.ring(5000, &mut last, false, &mut ticks);
        assert_eq!((last, ticks), (0, RingTicks::default()), "off: nothing written and nothing kept");

        let dir = tmp_dir("ring");
        let start = Instant::now();
        let mut r = Recorder::off();
        let path = r.start(&dir, start, "test build").unwrap();
        let mut ticks = t;
        r.ring(1000, &mut last, false, &mut ticks);
        assert_eq!(ticks, RingTicks::default(), "a written line starts over");
        ticks.tick(60, 1, false);
        r.ring(1500, &mut last, false, &mut ticks); // within the second: kept
        ticks.tick(40, 2, false);
        r.ring(2000, &mut last, false, &mut ticks);
        r.ring(3000, &mut last, false, &mut ticks); // no tick since: no line
        ticks.tick(100_000, 1, false);
        r.ring(3020, &mut last, true, &mut ticks); // a burst writes at once; saturates
        let writer_failed = r.stop(start);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!writer_failed, "{text}");
        let lines: Vec<&str> = text.lines().filter(|l| l.contains(",ring,")).collect();
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert!(lines[0].ends_with(",1000,ring,,80,58,1,1"), "{lines:?}");
        assert!(lines[1].ends_with(",2000,ring,,40,40,1,0"), "{lines:?}");
        assert!(lines[2].ends_with(",3020,ring,,65535,65535,0,0"), "{lines:?}");
        assert!(lines.iter().all(|l| l.split(',').count() == 8));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn off_records_nothing_and_writes_nothing() {
        let mut r = Recorder::off();
        r.audio(1, Stream::Thetis, 1, 20, 80, &[0x08, 0x40]);
        r.rtt(2, 30);
        assert!(!r.is_on());
        assert!(r.path().is_none());
    }

    #[test]
    fn a_measurement_is_a_csv_with_header_and_one_line_per_record() {
        let dir = tmp_dir("lines");
        let start = Instant::now();
        let mut r = Recorder::off();
        let path = r.start(&dir, start, "test build").unwrap();
        r.audio(100, Stream::Radio1, 7, 140, 81, &[0x08, 0x40]);
        r.audio(101, Stream::Radio1, 8, 160, 30, &[0x08, 0x00]);
        r.rtt(110, 42);
        r.event(115, Event::SessionStart(Route::Quic));
        r.event(120, Event::SessionEnd);
        r.stop(start);
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].starts_with("# ThetisLink measurement, test build"));
        assert_eq!(lines[1], HEADER);
        let cols = |l: &str| l.split(',').count();
        assert!(lines[2..].iter().all(|l| cols(l) == 8), "every line has the header's columns: {lines:?}");
        assert!(lines[2].contains(",start,"));
        assert!(lines.iter().any(|l| l.ends_with(",100,audio,radio1,7,140,81,1")), "{lines:?}");
        assert!(lines.iter().any(|l| l.ends_with(",101,audio,radio1,8,160,30,0")), "the bit, not the size: {lines:?}");
        assert!(lines.iter().any(|l| l.ends_with(",110,rtt,,,,,42")), "{lines:?}");
        assert!(lines.iter().any(|l| l.ends_with(",115,session_start,quic,,,,")), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains(",120,session_end,")), "{lines:?}");
        assert!(lines.last().unwrap().contains(",stop,"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The wall clock of a line is the engine clock plus the offset taken once at
    /// the start: two lines 1000 ms apart on the engine are 1000 ms apart on the wall.
    #[test]
    fn the_wall_clock_follows_the_engine_clock() {
        let dir = tmp_dir("wall");
        let start = Instant::now();
        let mut r = Recorder::off();
        let path = r.start(&dir, start, "t").unwrap();
        r.rtt(5_000, 1);
        r.rtt(6_000, 1);
        r.stop(start);
        let text = std::fs::read_to_string(&path).unwrap();
        let walls: Vec<i64> = text
            .lines()
            .filter(|l| l.contains(",rtt,"))
            .map(|l| l.split(',').next().unwrap().parse().unwrap())
            .collect();
        assert_eq!(walls[1] - walls[0], 1_000);
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64;
        assert!((walls[0] - 5_000 - now).abs() < 60_000, "anchored on today's wall clock");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A full ring drops and counts, and the count is in the file.
    #[test]
    fn a_full_ring_is_counted_in_the_file() {
        let dir = tmp_dir("full");
        let start = Instant::now();
        let mut r = Recorder::off();
        let path = r.start(&dir, start, "t").unwrap();
        // Faster than the writer can drain in one sleep: well over the capacity.
        for i in 0..(RING_CAPACITY as u32 * 3) {
            r.audio(i as u64, Stream::Vrx1, i, i, 10, &[]);
        }
        r.stop(start);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# dropped "), "a hole in the measurement is said");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Starting again closes the first file and opens a second one.
    #[test]
    fn starting_twice_gives_two_closed_files() {
        let dir = tmp_dir("twice");
        let start = Instant::now();
        let mut r = Recorder::off();
        let a = r.start(&dir, start, "t").unwrap();
        // No pause: the name has milliseconds, and an existing file is never
        // overwritten (round 90 - this test used to sleep 1.1 s to dodge that).
        let b = match r.start(&dir, start, "t") {
            Ok(b) => b,
            Err(_) => {
                std::thread::sleep(Duration::from_millis(2));
                r.start(&dir, start, "t").unwrap()
            }
        };
        r.stop(start);
        assert_ne!(a, b);
        assert!(std::fs::read_to_string(&a).unwrap().lines().last().unwrap().contains(",stop,"));
        assert!(std::fs::read_to_string(&b).unwrap().lines().last().unwrap().contains(",stop,"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Stopping wakes the writer instead of waiting out its pause: the engine's
    /// loop is held for the drain, not for 200 ms (round 90, measured 151-193 ms).
    #[test]
    fn stopping_does_not_wait_for_the_writers_pause() {
        let dir = tmp_dir("quick");
        let start = Instant::now();
        let mut r = Recorder::off();
        r.start(&dir, start, "t").unwrap();
        // Let the writer settle into its wait.
        std::thread::sleep(Duration::from_millis(250));
        let t = Instant::now();
        r.stop(start);
        let took = t.elapsed();
        assert!(took < Duration::from_millis(50), "stop held the caller for {took:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    struct FailingSink;
    impl Write for FailingSink {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("disk full"))
        }
        // Flush succeeds: this sink tests the write error alone, so a writer
        // that ignores it is not rescued by the flush (round 91).
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Run the writer on a thread and wait at most 3 s: a writer that ignores
    /// its error and loops on must turn the test red, not hang it (round 91).
    fn run_writer_bounded<W: Write + Send + 'static>(consumer: ringbuf::HeapCons<Record>, sink: W) -> bool {
        let failed = Arc::new(AtomicBool::new(false));
        let (wake, woken) = mpsc::channel::<()>();
        let (done_tx, done_rx) = mpsc::channel();
        let f = failed.clone();
        std::thread::spawn(move || {
            write_loop(consumer, sink, 0, Arc::new(AtomicU64::new(0)), woken, f, "x".into());
            let _ = done_tx.send(());
        });
        let ended = done_rx.recv_timeout(Duration::from_secs(3)).is_ok();
        drop(wake);
        ended && failed.load(Ordering::Acquire)
    }

    /// A sink that cannot be written: the writer marks the recorder failed and
    /// ends, instead of popping records into nowhere (round 90).
    #[test]
    fn a_write_error_marks_the_measurement_failed() {
        let (mut producer, consumer) = HeapRb::<Record>::new(8).split();
        let _ = producer.try_push(Record::Rtt { mono_ms: 1, rtt_ms: 2 });
        // Kept alive until the writer has ended, so it is the error that ends it.
        let keep = producer;
        assert!(run_writer_bounded(consumer, FailingSink), "ended, and marked failed");
        drop(keep);
    }

    /// And a flush that fails is the same failure: a BufWriter only shows a full
    /// disk when it flushes.
    #[test]
    fn a_flush_error_marks_the_measurement_failed() {
        struct FlushFails;
        impl Write for FlushFails {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Err(std::io::Error::other("storage gone"))
            }
        }
        let (producer, consumer) = HeapRb::<Record>::new(8).split();
        assert!(run_writer_bounded(consumer, FlushFails), "ended, and marked failed");
        drop(producer);
    }

    /// The two routes keep their own word in the file and on screen.
    #[test]
    fn each_route_has_its_own_word() {
        assert_eq!(Event::SessionStart(Route::Quic).detail(), "quic");
        assert_eq!(Event::SessionStart(Route::QuicRelay).detail(), "quic-relay");
        assert_eq!(Route::Quic.label(), "QUIC");
        assert_eq!(Route::QuicRelay.label(), "QUIC via relay");
    }

    /// The settings' rule: a relay that is set up, or not.
    #[test]
    fn the_settings_ask_for_one_route() {
        assert_eq!(Route::asked_for(true), Route::QuicRelay);
        assert_eq!(Route::asked_for(false), Route::Quic);
    }

    /// The engine picks up a write failure and tells the screen. The loop that
    /// does it runs in no test (a failing disk behind the real engine is not
    /// portable to stage), so its presence is pinned - that it is there, not
    /// that it runs (round 91; the known limit of a text guard).
    #[test]
    fn the_engine_picks_up_a_write_failure() {
        let src = include_str!("engine.rs");
        let at = src.find("measure.take_failure()").expect("the engine asks the recorder whether it failed");
        let after = &src[at..(at + 300).min(src.len())];
        assert!(after.contains("state.measurement_file = None"), "and turns the screen off: {after}");
    }

    /// The engine sees a failure once, and the recorder is off after it.
    #[test]
    fn a_failure_is_taken_once_and_turns_the_recorder_off() {
        let dir = tmp_dir("taken");
        let start = Instant::now();
        let mut r = Recorder::off();
        let path = r.start(&dir, start, "t").unwrap();
        assert_eq!(r.take_failure(), None, "nothing failed");
        r.running.as_ref().unwrap().failed.store(true, Ordering::Release);
        assert_eq!(r.take_failure(), Some(path));
        assert!(!r.is_on());
        assert_eq!(r.take_failure(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The phone's network and position lines (build 181): the header's eight
    /// columns, the technology by name, and a value the phone did not give as
    /// an empty column - never as Android's "unavailable" number.
    #[test]
    fn network_lines_have_the_header_columns_and_leave_unknowns_empty() {
        let dir = tmp_dir("net");
        let start = Instant::now();
        let mut r = Recorder::off();
        let path = r.start(&dir, start, "t").unwrap();
        let net = |mono_ms, sample| Record::Net { mono_ms, sample };
        r.record(net(30, NetSample::Cell { tech: Tech::Lte, cell_id: 12_345_678, pci: 301, arfcn: 6300, tac: 4711 }));
        r.record(net(31, NetSample::Cell { tech: Tech::Nr, cell_id: UNAVAILABLE_LONG, pci: UNAVAILABLE, arfcn: 640_000, tac: UNAVAILABLE }));
        r.record(net(32, NetSample::Signal { tech: Tech::Lte, rsrp: -98, rsrq: -11, sinr: 7, level: 3 }));
        r.record(net(33, NetSample::Signal { tech: Tech::Wifi, rsrp: -61, rsrq: UNAVAILABLE, sinr: UNAVAILABLE, level: UNAVAILABLE }));
        r.record(net(34, NetSample::Gps { lat_e6: 52_000_001, lon_e6: 5_000_002, speed_kmh: 87, accuracy_m: 4 }));
        // A fix without speed or accuracy (standing still, the first fix): the
        // phone sends "not given" there (review of 181).
        r.record(net(35, NetSample::Gps { lat_e6: 52_000_003, lon_e6: 5_000_004, speed_kmh: UNAVAILABLE, accuracy_m: UNAVAILABLE }));
        // A 5G cell beside the 4G one (build 210): no NCI, a PCI and an NR-ARFCN.
        let secondary = CellLink::from_android(2);
        r.record(net(36, NetSample::NrCell { link: secondary, cell_id: UNAVAILABLE_LONG, pci: 412, arfcn: 640_000, tac: UNAVAILABLE }));
        r.stop(start);
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().filter(|l| !l.starts_with('#')).collect();
        assert!(lines.iter().all(|l| l.split(',').count() == 8), "{lines:?}");
        for want in [
            ",30,cell,lte,12345678,301,6300,4711",
            ",31,cell,nr,,,640000,",
            ",32,signal,lte,-98,-11,7,3",
            ",33,signal,wifi,-61,,,",
            ",34,gps,gps,52000001,5000002,87,4",
            ",35,gps,gps,52000003,5000004,,",
            ",36,nrcell,secondary,,412,640000,",
        ] {
            assert!(lines.iter().any(|l| l.ends_with(want)), "{want} not in {lines:?}");
        }
        assert!(!text.contains(&i32::MAX.to_string()) && !text.contains(&i64::MAX.to_string()));
        // The same "not given" on both sides of the bridge: Android's
        // CellInfo.UNAVAILABLE(_LONG), which the phone passes on unchanged
        // (`NetworkMeter.kt`, `U` and `long()`; the note in the UDL).
        assert_eq!(UNAVAILABLE, 2_147_483_647);
        assert_eq!(UNAVAILABLE_LONG, 9_223_372_036_854_775_807);
        for t in [Tech::Lte, Tech::Nr, Tech::Wcdma, Tech::Gsm, Tech::Wifi, Tech::Other] {
            assert_eq!(Tech::from_name(t.name()), t, "{t:?} does not read back");
        }
        assert_eq!(Tech::from_name("cdma"), Tech::Other);
        // Android's numbers for how a cell is used, as the phone passes them
        // (`CellInfo.CONNECTION_*`; `NetworkMeter.kt` sends them unchanged).
        for (code, link) in [(0, CellLink::None), (1, CellLink::Primary), (2, CellLink::Secondary), (UNAVAILABLE, CellLink::Unknown)] {
            assert_eq!(CellLink::from_android(code), link, "{code}");
        }
        assert_eq!(CellLink::from_android(7), CellLink::Unknown);
        let names: Vec<_> = [CellLink::None, CellLink::Primary, CellLink::Secondary, CellLink::Unknown].map(CellLink::name).to_vec();
        assert_eq!(names, ["none", "primary", "secondary", "unknown"]);
        // The phone passes Android's number unchanged, for a 5G cell that is
        // not the registered one only (build 210). Kotlin, so pinned as text.
        let meter = include_str!("../../sdr-remote-android/android/app/src/main/java/com/sdrremote/measure/NetworkMeter.kt");
        assert_eq!(meter.matches("bridge.measureNrCell(c.cellConnectionStatus, it.nci, it.pci, it.nrarfcn, it.tac)").count(), 1);
        let unregistered = meter.find("if (!c.isRegistered) {").expect("the unregistered branch");
        let call = meter.find("bridge.measureNrCell(").unwrap();
        let back = meter[unregistered..].find("continue").unwrap() + unregistered;
        assert!(unregistered < call && call < back, "measureNrCell outside the unregistered branch");
    }

    /// The new kinds of line, each with the header's eight columns.
    #[test]
    fn the_send_side_lines_have_the_header_columns() {
        use sdr_remote_core::control_ack::ControlKey;
        let dir = tmp_dir("send");
        let start = Instant::now();
        let mut r = Recorder::off();
        let path = r.start(&dir, start, "t").unwrap();
        r.tx(10, Stream::Thetis, 4, 60);
        PttSeen::default().follow(PttSeen { radio1: true, ..Default::default() }, 11, &mut r);
        r.conceal(12, Stream::Vrx2);
        r.record(Record::Repeat { mono_ms: 13, seq: 99 });
        r.record(Record::GiveUp { mono_ms: 14, key: ControlKey::Control(7) });
        r.event(15, Event::RelayOnly(true));
        r.event(16, Event::RelayOnly(false));
        r.event(19, Event::ForceTcp(true));
        r.event(20, Event::ForceTcp(false));
        r.event(17, Event::Path(sdr_remote_core::path::PathKind::DirectViaRelay));
        r.event(18, Event::Path(sdr_remote_core::path::PathKind::RelayCoupling));
        r.event(21, Event::Path(sdr_remote_core::path::PathKind::RelayTcp));
        r.stop(start);
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().filter(|l| !l.starts_with('#')).collect();
        assert!(lines.iter().all(|l| l.split(',').count() == 8), "{lines:?}");
        for want in [
            ",10,tx,thetis,4,,60,",
            ",11,ptt,radio1,1,,,",
            ",12,conceal,vrx2,,,,",
            ",13,repeat,,99,,,",
            ",14,give_up,control,7,,,",
            ",15,relay_only,on,,,,",
            ",16,relay_only,off,,,,",
            ",17,path,D+R,,,,",
            ",18,path,R-coupling,,,,",
            ",19,force_tcp,on,,,,",
            ",20,force_tcp,off,,,,",
            ",21,path,R-TCP,,,,",
        ] {
            assert!(lines.iter().any(|l| l.ends_with(want)), "{want}: {lines:?}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A PTT edge is written for each transmitter that changed, and only then.
    #[test]
    fn ptt_edges_are_written_once_per_change() {
        let dir = tmp_dir("ptt");
        let start = Instant::now();
        let mut r = Recorder::off();
        let path = r.start(&dir, start, "t").unwrap();
        let mut seen = PttSeen::default();
        let on = PttSeen { thetis: true, ..Default::default() };
        seen.follow(on, 1, &mut r);
        seen.follow(on, 2, &mut r);
        seen.follow(PttSeen::default(), 3, &mut r);
        r.stop(start);
        let text = std::fs::read_to_string(&path).unwrap();
        let ptt: Vec<&str> = text.lines().filter(|l| l.contains(",ptt,")).collect();
        assert_eq!(ptt.len(), 2, "{ptt:?}");
        assert!(ptt[0].ends_with(",1,ptt,thetis,1,,,") && ptt[1].ends_with(",3,ptt,thetis,0,,,"), "{ptt:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
