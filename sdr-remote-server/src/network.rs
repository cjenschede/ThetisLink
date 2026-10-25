// SPDX-License-Identifier: GPL-2.0-or-later

/// A control a client sent: an event at INFO, unless it is part of the state
/// dump that arrives right after connecting.
///
/// That dump restates what the client already had - 35, 32 and 25 lines inside
/// a single second in one operator's log, 184 of 627 lines in total. It is one
/// event, and forty lines is not how you write one down. Same rule as the TCI
/// state notifications and the squelch gate: log the change, not the state.
macro_rules! ctrl_log {
    ($opening:expr, $($arg:tt)*) => {
        if $opening { log::debug!($($arg)*) } else { log::info!($($arg)*) }
    };
}

use std::net::SocketAddr;
use sdr_remote_core::dev_info;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};

// NOTE: the former global VRX spectrum-span + audio-rate atomics are removed -
// both are now per-client in `vrx_manager::PerClientVrxManager`
// (PATCH-vrx-per-client).
use std::time::Instant;

use anyhow::{Context, Result};
use log::{debug, error, info, warn};
use tokio::net::UdpSocket;
use crate::tracked_socket::TrackedSocket;
use tokio::sync::{watch, Mutex};
use tokio::time::{interval, Duration};

use sdr_remote_core::jitter::{BufferedFrame, JitterBuffer, TX_MAX_DEPTH, TX_MIN_DEPTH};
use sdr_remote_core::resample::{resampler, UpFilter};
use sdr_remote_core::protocol::*;
use sdr_remote_core::{FRAME_SAMPLES_WIDEBAND, FULL_SPECTRUM_BINS, MAX_PACKET_SIZE, NETWORK_SAMPLE_RATE_WIDEBAND};

use crate::amplitec::AmplitecSwitch;
use crate::config::ServerConfig;

/// Next (up) or previous (down) memory channel, SKIPPING empty channels. Uses
/// the filled-channel list when available (wrapping to the first/last filled
/// channel); falls back to cur+/-1 within 1..=max when the list is empty.
fn next_memory_channel(filled: &[u16], cur: u16, up: bool, max: u16) -> u16 {
    if filled.is_empty() {
        return if up {
            if cur >= max { 1 } else { cur + 1 }
        } else if cur <= 1 { max } else { cur - 1 };
    }
    if up {
        filled.iter().copied().find(|&c| c > cur).unwrap_or(filled[0])
    } else {
        filled.iter().rev().copied().find(|&c| c < cur).unwrap_or(filled[filled.len() - 1])
    }
}

use crate::ptt::PttController;
use crate::rf2k::Rf2k;
use crate::session::SessionManager;
use crate::spe_expert::SpeExpert;
use crate::spectrum::{Rx2SpectrumProcessor, SpectrumProcessor};
use crate::tuner::Tuners;
use crate::dxcluster::DxCluster;
use crate::rotor::Rotor;
use crate::ultrabeam::UltraBeam;

/// Pack a dBm reading into the wire format: dBm x 10 as i16, saturating at the
/// ends so a stale `-200 dBm` sentinel still survives the cast intact.
fn dbm_to_deci(dbm: f32) -> i16 {
    (dbm * 10.0).round().clamp(i16::MIN as f32, i16::MAX as f32) as i16
}


/// Bind-fail / bind-timeout diagnostic write helper. Direct file-write to
/// thetislink-server.log next to the executable, bypassing `log::` macros for
/// consistency with panic-hook and to ensure visibility even if logger state
/// is partially initialised.
const BIND_TIMEOUT_SECS: u64 = 30;
const BIND_RETRY_MS: u64 = 250;

/// Bind the server's UDP port, retrying a port that is not yet free for up to
/// [`BIND_TIMEOUT_SECS`] - and giving up within one retry when the server is
/// stopped meanwhile (`Ok(None)`). Until build 131 a stop during this wait went unheard
/// for up to 30 s (review 130, measured).
async fn bind_with_retry(
    bind_addr: SocketAddr,
    shutdown: watch::Receiver<bool>,
) -> std::io::Result<Option<UdpSocket>> {
    let started = std::time::Instant::now();
    let deadline = Duration::from_secs(BIND_TIMEOUT_SECS);
    loop {
        if *shutdown.borrow() {
            return Ok(None);
        }
        match UdpSocket::bind(bind_addr).await {
            Ok(s) => return Ok(Some(s)),
            Err(e) => {
                // Identify transient port-not-yet-released errors so we
                // distinguish them from "no such interface" etc.
                let kind = e.kind();
                let transient = matches!(
                    kind,
                    std::io::ErrorKind::AddrInUse | std::io::ErrorKind::PermissionDenied
                );
                if !transient || started.elapsed() >= deadline {
                    return Err(e);
                }
                info!(
                    "bind {} transient error ({:?}: {}), retrying in {}ms",
                    bind_addr, kind, e, BIND_RETRY_MS
                );
                // The stop is looked at before every try: heard within one
                // retry, a quarter of a second.
                tokio::time::sleep(Duration::from_millis(BIND_RETRY_MS)).await;
            }
        }
    }
}

fn write_bind_diag(entry: &str) {
    let log_path = crate::server_log_path();
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true).append(true).open(&log_path)
    {
        use std::io::Write;
        let _ = f.write_all(entry.as_bytes());
    }
}

/// How often the SMALL state is sent again regardless of the tick-lists.
///
/// The safety net from `docs/internal/DESIGN-state-sync-push.md`: a push is not
/// guaranteed to arrive, and a tick-list that says "delivered" cannot tell the
/// difference between arrived and lost. Costs nothing at the device end - it is
/// served from what the server already holds - and only a few dozen bytes on the
/// network, so it may be generous. Only the DX-spot stream had one before.
const STATE_RESEND_SECS: u64 = 10;

/// The same net for the BIG blobs - the memory list and the EX settings.
///
/// Deliberately slower: those are kilobytes each (the FTX-1 alone has 405 EX values
/// and 99 channels), and repeating them every 10 s would put roughly 100 kB/min per
/// client on the wire - which a metered mobile link notices. A lost memory list is
/// not time-critical: a minute late is not worse than ten seconds late, while the
/// traffic is six times less.
const BLOB_RESEND_SECS: u64 = 60;

/// Decide what to push and to whom, for a blob that follows the state-sync rule
/// (see `docs/internal/DESIGN-state-sync-push.md`): if it CHANGED it goes to every
/// subscriber; otherwise only to subscribers that have not had the current one yet.
///
/// `sent` is the tick-list of who already has it, kept per blob. It is pruned here
/// so a departed client cannot keep a stale entry alive.
///
/// The third element says whether this was a real CHANGE. A top-up after the safety
/// net cleared the tick-list is not an event - logging it as one buries the real
/// ones: four identical lines every interval, tens of thousands a day, and then
/// "did client X get its block" is no longer answerable.
fn blob_push_targets(
    mailbox: &std::sync::Mutex<Option<String>>,
    stored: impl FnOnce() -> Option<String>,
    addrs: &[std::net::SocketAddr],
    sent: &mut std::collections::HashSet<std::net::SocketAddr>,
) -> (Option<String>, Vec<std::net::SocketAddr>, bool) {
    sent.retain(|a| addrs.contains(a));
    match mailbox.lock().unwrap().take() {
        Some(text) => {
            sent.clear(); // changed - everyone gets the new one
            (Some(text), addrs.to_vec(), true)
        }
        None => {
            let fresh: Vec<_> = addrs.iter().copied().filter(|a| !sent.contains(a)).collect();
            if fresh.is_empty() { (None, Vec::new(), false) } else { (stored(), fresh, false) }
        }
    }
}

/// Whether the server's copy of a radio's memories can stand in for a read: it
/// lists at least one channel. A cold FTX-1 that answers no channel in three
/// passes still gives `Ok` with the header alone, and that copy kept the
/// radio from ever being read again once the phone asked for the copy too
/// (build 170; review of 169).
fn memory_copy_has_channels(copy: Option<&str>) -> bool {
    copy.is_some_and(|text| text.lines().skip(1).any(|line| !line.trim().is_empty()))
}

/// Send one blob to a set of clients, chunked, and report who took EVERY chunk.
///
/// Only a complete send counts as delivered: ticking a client off on the intent
/// would make a failed send permanent, because the tick-list would say "done" and
/// nothing would ever offer it again.
fn push_blob(
    socket: &crate::tracked_socket::TrackedSocket,
    packet_type: PacketType,
    text: &str,
    targets: &[std::net::SocketAddr],
) -> std::collections::HashSet<std::net::SocketAddr> {
    let mut delivered: std::collections::HashSet<std::net::SocketAddr> =
        targets.iter().copied().collect();
    // UDP max ~64KB per datagram; the client reassembles on packet type.
    for chunk in text.as_bytes().chunks(60000) {
        let mut send_buf = Vec::with_capacity(6 + chunk.len());
        let header = Header::new(packet_type, Flags::NONE);
        let mut hdr_buf = [0u8; 4];
        header.serialize(&mut hdr_buf);
        send_buf.extend_from_slice(&hdr_buf);
        send_buf.extend_from_slice(&(chunk.len() as u16).to_be_bytes());
        send_buf.extend_from_slice(chunk);
        for addr in targets {
            if socket.try_send_to(&send_buf, *addr).is_err() {
                delivered.remove(addr);
            }
        }
    }
    delivered
}

/// Build this server's own problem-report attachment and send it to one client.
///
/// The cleaning happens HERE, before a byte leaves, and by the same code the
/// desktop client uses on its own files (`sdr_remote_core::diagnose`). The raw
/// log never goes on the wire, not even to a client of ours: "it is only going
/// to my own client" is exactly the reasoning that puts a relay host in
/// somebody's report.
///
/// Sent in numbered parts because this is UDP and a log does not fit in a
/// datagram. Nothing is retransmitted and nothing is acknowledged: the parts
/// carry their own numbering, so the client can see that a transfer was
/// incomplete and ask again. A transfer that silently lost its middle would be
/// worse than one that failed.
/// How often one address may ask. Building the attachment reads a 200 kB log,
/// cleans it line by line and puts a couple of hundred datagrams on the wire;
/// asking for that in a loop is work the server does instead of carrying audio.
///
/// Long enough to be a limit, short enough that a genuine retry after a lossy
/// transfer does not mean waiting around.
const REPORT_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(20);

/// When each address was last served, and whether one is being served now.
///
/// An authenticated client is trusted to key the transmitter, which is not the
/// same as being trusted to ask for the same expensive thing without pause. The
/// in-flight flag matters as much as the clock: two requests arriving in the
/// same instant would otherwise both get through the cooldown check.
static REPORT_GUARD: std::sync::Mutex<Option<(std::collections::HashMap<std::net::SocketAddr, std::time::Instant>, bool)>> =
    std::sync::Mutex::new(None);

/// One radio's list as the report carries it: slot, model code, and the
/// server's own copy of its memory channels.
type RadioList = Option<(u8, u8, Option<String>)>;

fn send_own_report(
    socket: std::sync::Arc<crate::tracked_socket::TrackedSocket>,
    addr: std::net::SocketAddr,
    lists: [RadioList; 2],
) {
    {
        let mut g = REPORT_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let (seen, busy) = g.get_or_insert_with(|| (std::collections::HashMap::new(), false));
        if *busy {
            info!("client {} asked for the server report while one is already going out", addr);
            return;
        }
        if let Some(last) = seen.get(&addr) {
            if last.elapsed() < REPORT_COOLDOWN {
                info!(
                    "client {} asked for the server report again after {:?}; asked to wait",
                    addr,
                    last.elapsed()
                );
                return;
            }
        }
        // Bounded: one entry per address that ever asked, cleared of anything
        // older than the cooldown, so a scanner cannot grow this.
        seen.retain(|_, t| t.elapsed() < REPORT_COOLDOWN * 4);
        seen.insert(addr, std::time::Instant::now());
        *busy = true;
    }
    let done = ReportInFlight;

    // The expensive half runs on its own thread: building the attachment reads
    // a 200 kB log and cleans it line by line, and the send paces a couple of
    // hundred datagrams with deliberate pauses. The loop this is called from is
    // the one that carries audio and PTT, and it must never wait on a file.
    // The busy flag above already limits this to one thread at a time; if the
    // spawn fails, dropping the closure drops the guard and clears the flag.
    if let Err(e) = std::thread::Builder::new()
        .name("server-report".into())
        .spawn(move || {
            let _done = done;
            send_own_report_inner(&socket, addr, &lists);
        })
    {
        warn!("client {} asked for the server report, but no thread could be started: {}", addr, e);
    }
}

/// Clears the in-flight flag however the send ends, including on a panic.
struct ReportInFlight;
/// Which clients have already been reported as sending a control this build
/// does not know, and when.
///
/// Keyed per address, on the same pattern as `REPORT_GUARD` above. It was a
/// single flag for the whole class to begin with, which damped the flood and
/// also everything after it: once any client had sent any unknown control, no
/// other client and no other control was ever mentioned again for the rest of
/// the run - and these run for days. Damping a class must not mean reporting
/// one member of it.
static UNKNOWN_CONTROL_SEEN: std::sync::Mutex<
    Option<std::collections::HashMap<std::net::SocketAddr, std::time::Instant>>,
> = std::sync::Mutex::new(None);

/// How long before the same client is worth mentioning again.
const UNKNOWN_CONTROL_QUIET: Duration = Duration::from_secs(300);

/// Whether this address should be logged now.
fn unknown_control_worth_logging(addr: std::net::SocketAddr) -> bool {
    let mut guard = match UNKNOWN_CONTROL_SEEN.lock() {
        Ok(g) => g,
        // A poisoned lock must not silence the log it guards.
        Err(e) => e.into_inner(),
    };
    let seen = guard.get_or_insert_with(std::collections::HashMap::new);
    let now = std::time::Instant::now();
    match seen.get(&addr) {
        Some(t) if now.duration_since(*t) < UNKNOWN_CONTROL_QUIET => false,
        _ => {
            seen.insert(addr, now);
            true
        }
    }
}

impl Drop for ReportInFlight {
    fn drop(&mut self) {
        if let Ok(mut g) = REPORT_GUARD.lock() {
            if let Some((_, busy)) = g.as_mut() {
                *busy = false;
            }
        }
    }
}

fn send_own_report_inner(
    socket: &crate::tracked_socket::TrackedSocket,
    addr: std::net::SocketAddr,
    lists: &[RadioList; 2],
) {
    let dir = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let log = crate::server_log_path();
    let conf = dir.join("thetislink-server.conf");
    // The relay address is read from the config the same way the GUI does, so
    // the one host that must never travel is removed here too.
    let relay_url = crate::config::load().relay_url;

    let text = match sdr_remote_core::diagnose::build_attachment(
        &log.to_string_lossy(),
        &conf.to_string_lossy(),
        &relay_url,
    ) {
        Ok(t) => t,
        Err(e) => {
            // Nothing is sent. The client waits, times out, and says the server
            // log could not be fetched - which is the truth, and better than a
            // half report labelled as the server's.
            warn!("client {} asked for the server report, which could not be built: {}", addr, e);
            return;
        }
    };

    // The radios' lists, appended after the cleaned log. Not run through the
    // redaction: a memory channel is a frequency, a name and a tone, and none
    // of those is somebody's address or password. It is also the one thing a
    // report could not answer before - for an FTX-1 the list is where its tones
    // live, so "the tone is gone" and "the server still has it" are different
    // faults that looked identical from outside (2026-08-12).
    let mut text = text;
    for entry in lists.iter().flatten() {
        let (slot, model, blob) = entry;
        let name = sdr_remote_core::protocol::radio_model_name(*model);
        text.push_str(&format!("\n--- radio {slot} ({name}): the server's memory list ---\n"));
        match blob {
            Some(b) if !b.trim().is_empty() => {
                text.push_str(b);
                if !b.ends_with('\n') {
                    text.push('\n');
                }
            }
            // Said rather than left out: an empty list is a fact, and it is
            // exactly the fact a restart-loses-my-tones report turns on.
            _ => text.push_str("(the server holds no list for this radio)\n"),
        }
    }

    let bytes = text.as_bytes();
    // Small enough to cross a normal path without IP fragmentation, tunnel
    // overhead included. The existing blob push uses 60 kB, which is fine on a
    // LAN and a lottery over the internet.
    const PART: usize = 1100;
    let parts = bytes.len().div_ceil(PART).max(1);
    if parts > u16::MAX as usize {
        warn!("client {} asked for the server report, which is too large to send", addr);
        return;
    }
    info!("client {}: sending the server report, {} bytes in {} parts", addr, bytes.len(), parts);

    for (i, chunk) in bytes.chunks(PART).enumerate() {
        let mut buf = Vec::with_capacity(10 + chunk.len());
        let header = Header::new(PacketType::ServerReportPart, Flags::NONE);
        let mut hdr = [0u8; 4];
        header.serialize(&mut hdr);
        buf.extend_from_slice(&hdr);
        buf.extend_from_slice(&(i as u16).to_be_bytes());
        buf.extend_from_slice(&(parts as u16).to_be_bytes());
        buf.extend_from_slice(&(chunk.len() as u16).to_be_bytes());
        buf.extend_from_slice(chunk);
        let _ = socket.try_send_to(&buf, addr);
        // A pause every so often. Two hundred datagrams sent back to back
        // overrun a receive buffer, and the ones that get dropped are dropped
        // for no reason at all: nobody is waiting on this in a hurry.
        if i % 16 == 15 {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }
}

/// Step 4's way in (build 256): set where the socket is made, taken when the
/// service runs.
#[cfg(feature = "naspelen")]
static TRIAL_INBOUND: std::sync::OnceLock<tokio::sync::mpsc::UnboundedSender<(SocketAddr, Vec<u8>)>> = std::sync::OnceLock::new();

/// Server network service
pub struct NetworkService {
    socket: Arc<TrackedSocket>,
    session: Arc<Mutex<SessionManager>>,
    ptt: Arc<Mutex<PttController>>,
    spectrum: Arc<Mutex<SpectrumProcessor>>,
    rx2_spectrum: Arc<Mutex<Rx2SpectrumProcessor>>,
    shutdown: watch::Receiver<bool>,
    /// Per-client VRX state (PATCH-vrx-per-client). Shared with the audio-loop
    /// task (control-state per client); the loop owns the runtimes. Sync mutex
    /// (short locks, no `.await` held) - distinct from the tokio mutexes above.
    vrx_mgr: Arc<std::sync::Mutex<crate::vrx_manager::PerClientVrxManager>>,
    amplitec: Option<Arc<AmplitecSwitch>>,
    /// Multi-tuner collection (0..=`MAX_TUNERS` instances). The single-
    /// tuner status broadcast and the Tune command handler route via
    /// `Tuners::for_amplitec_pos(active_pos)` and fall back to
    /// `Tuners::primary()` when no Amplitec mapping matches.
    tuners: Arc<Tuners>,
    spe: Option<Arc<SpeExpert>>,
    rf2k: Option<Arc<Rf2k>>,
    ultrabeam: Option<Arc<UltraBeam>>,
    rotor: Option<Arc<Rotor>>,
    config: ServerConfig,
    tuner_cat_rx: Option<tokio::sync::mpsc::Receiver<String>>,
    drive_level_shared: Option<Arc<AtomicU8>>,
    active_pa_shared: Option<Arc<AtomicU8>>,
    dxcluster: Option<Arc<DxCluster>>,
    vfo_freq_shared: Option<Arc<AtomicU64>>,
    vfo_b_freq_shared: Option<Arc<AtomicU64>>,
    yaesu_ptt_flag: Arc<std::sync::atomic::AtomicBool>,
    /// Slot 1's counterpart. "The server keyed this radio" - the one thing that
    /// separates our transmission from an operator standing at the front panel,
    /// and the reason the orphan watch can never take his away (see tx_orphan).
    yaesu2_ptt_flag: Arc<std::sync::atomic::AtomicBool>,
    yaesu: Option<Arc<crate::yaesu::YaesuRadio>>,
    /// Dual-radio slot 1 (PATCH-dual-radio-991a-ftx1, Option B-prime). Own
    /// independent broadcast/audio/control chain; None = slot 1 off.
    yaesu2: Option<Arc<crate::yaesu::YaesuRadio>>,
    /// The CW playout (cw_playout.rs): edges in from the clients, key
    /// commands out at exact moments on their own thread.
    cw: crate::cw_playout::Handle,
    /// The key lines of the two Yaesu slots (CW design step 6): RTS on each
    /// radio's second port, opened at the first CW run to that radio.
    cw_lines: CwKeyLines,
    /// The one way to the key line. Everything that has to key or unkey sends
    /// its edge here and one task writes it, in the order the channel keeps.
    ///
    /// Three places used to call `cw_output` directly beside the playout's own
    /// task - the watchdog, the time-out and the goodbye - and that is a race
    /// with a real end state: the playout task checks "is this edge's run still
    /// current", drops its lock, and only then writes. A safety key-up sent in
    /// that window lands BEFORE a key-down that was still valid when it was
    /// checked, so the last word on the line is "key down" while the playout
    /// believes the key is up. Nothing comes after that, and a Yaesu has no
    /// keying watchdog of its own (review 2120A1 round 2).
    ///
    /// The safety property survives the change: the consumer is a tokio task,
    /// not the playout thread, so it still runs when that thread stalls - which
    /// is what the watchdog on the tick was added for.
    cw_due: tokio::sync::mpsc::UnboundedSender<crate::cw_playout::DueEdge>,
    /// Refusals of CW keying, told once per attempt like the PTT ones.
    cw_denied: crate::tx_denial::DenialLog,
    /// PATCH-2: lock-free audio-activity counters shared with the
    /// audio loops AND the UI Status panel.
    audio_stats: Arc<crate::audio_stats::AudioActivityStats>,
    /// PATCH-2: lock-free TCI-connection probe - updated from the
    /// heartbeat handler, read by the Status panel.
    tci_probe: Arc<crate::audio_stats::TciStatusProbe>,
    /// PATCH-2: server start time - used as the mono-clock reference
    /// for `AudioActivityStats::tick()` and the Status panel snapshot.
    server_start: Instant,
}

impl NetworkService {
    pub async fn new(
        bind_addr: SocketAddr,
        session: Arc<Mutex<SessionManager>>,
        ptt: Arc<Mutex<PttController>>,
        spectrum: Arc<Mutex<SpectrumProcessor>>,
        rx2_spectrum: Arc<Mutex<Rx2SpectrumProcessor>>,
        shutdown: watch::Receiver<bool>,
        amplitec: Option<Arc<AmplitecSwitch>>,
        tuners: Arc<Tuners>,
        spe: Option<Arc<SpeExpert>>,
        rf2k: Option<Arc<Rf2k>>,
        ultrabeam: Option<Arc<UltraBeam>>,
        rotor: Option<Arc<Rotor>>,
        config: ServerConfig,
        tuner_cat_rx: Option<tokio::sync::mpsc::Receiver<String>>,
        drive_level_shared: Option<Arc<AtomicU8>>,
        active_pa_shared: Option<Arc<AtomicU8>>,
        dxcluster: Option<Arc<DxCluster>>,
        vfo_freq_shared: Option<Arc<AtomicU64>>,
        vfo_b_freq_shared: Option<Arc<AtomicU64>>,
        yaesu: Option<Arc<crate::yaesu::YaesuRadio>>,
        yaesu2: Option<Arc<crate::yaesu::YaesuRadio>>,
        audio_stats: Arc<crate::audio_stats::AudioActivityStats>,
        tci_probe: Arc<crate::audio_stats::TciStatusProbe>,
        server_start: Instant,
        bind_status_slot: Arc<std::sync::OnceLock<crate::audio_stats::BindStatus>>,
        // When the relay is on: its grants, for the QUIC endpoint's route layer
        // (phase 3b).
        quic_relay: Option<sdr_remote_quic::RelayRoute>,
    ) -> Result<Self> {
        // The CW playout: a thread that hands due edges to a task that sends
        // them to Thetis. The thread keeps the moments; the task holds the PTT
        // lock only for the send, and says so when that took longer than a
        // couple of milliseconds (measurement 5 of the CW design).
        let cw = crate::cw_playout::new_handle();
        let cw_lines: CwKeyLines = Arc::new(std::sync::Mutex::new(KeyLineSet::new()));
        let cw_due;
        {
            let (due_tx, mut due_rx) = tokio::sync::mpsc::unbounded_channel::<crate::cw_playout::DueEdge>();
            cw_due = Some(due_tx.clone());
            let _thread = crate::cw_playout::spawn_playout_thread(cw.clone(), due_tx);
            let ptt_for_cw = ptt.clone();
            let lines_for_cw = cw_lines.clone();
            let cw_for_stats = cw.clone();
            tokio::spawn(async move {
                while let Some(e) = due_rx.recv().await {
                    let taken = Instant::now();
                    // Stale after a release, a watchdog or the end of the run:
                    // the key-up that ended it stays the last word.
                    if crate::cw_playout::lock(&cw_for_stats).current_run() != e.run {
                        info!("cw playout: edge seq={} dropped, its run is over", e.seq);
                        continue;
                    }
                    cw_output(&ptt_for_cw, &lines_for_cw, e.target, e.down, Some(e.len_ms)).await;
                    let lag = taken.saturating_duration_since(e.planned);
                    if lag > Duration::from_millis(2) {
                        warn!("cw playout: edge seq={} sent {} ms after its moment", e.seq, lag.as_millis());
                    }
                    let (played, max_dev) = {
                        let g = crate::cw_playout::lock(&cw_for_stats);
                        (g.played, g.max_deviation_us)
                    };
                    if played % 100 == 0 {
                        info!("cw playout: {} edges played, worst deviation so far {:.2} ms", played, max_dev as f64 / 1000.0);
                    }
                }
            });
        }

        // Wrap bind in 30s timeout to catch cold-boot kernel-hangs (memory
        // project_tl2_coldboot_bind_fail.md). Within that window we retry
        // every 250 ms on "address in use" / "access denied" / similar
        // transient errors so the auto-restart path doesn't lose the race
        // when the previous instance's socket is still in TIME_WAIT or the
        // process slot is briefly held. Other errors fail immediately.
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let socket = match tokio::time::timeout(
            Duration::from_secs(BIND_TIMEOUT_SECS + 2),
            bind_with_retry(bind_addr, shutdown.clone()),
        )
        .await
        {
            Ok(Ok(None)) => {
                info!("Stopped while waiting for port {} - not started", bind_addr);
                return Err(anyhow::anyhow!("stopped while waiting for port {}", bind_addr));
            }
            Ok(Ok(Some(s))) => s,
            Ok(Err(e)) => {
                let entry = format!("[BIND-FAIL] ts={} addr={} error={}\n", ts, bind_addr, e);
                write_bind_diag(&entry);
                let _ = bind_status_slot.set(
                    crate::audio_stats::BindStatus::Failed {
                        addr: bind_addr.to_string(),
                        error: format!("{}", e),
                    },
                );
                return Err(anyhow::Error::new(e)).context("bind the server's port");
            }
            Err(_elapsed) => {
                let entry = format!(
                    "[BIND-TIMEOUT] ts={} addr={} elapsed={}s\n",
                    ts, bind_addr, BIND_TIMEOUT_SECS
                );
                write_bind_diag(&entry);
                let _ = bind_status_slot.set(
                    crate::audio_stats::BindStatus::Failed {
                        addr: bind_addr.to_string(),
                        error: format!("bind timed out after {}s", BIND_TIMEOUT_SECS),
                    },
                );
                return Err(anyhow::anyhow!(
                    "bind the server's port timed out after {}s on {}",
                    BIND_TIMEOUT_SECS, bind_addr
                ));
            }
        };
        // Enlarge the socket buffers: the default Windows buffer (8KB) overflows
        // with spectrum packets (4-8KB each x clients), dropping audio packets.
        // QUIC runs on this same socket.
        {
            use socket2::SockRef;
            let sock_ref = SockRef::from(&socket);
            let _ = sock_ref.set_send_buffer_size(2 * 1024 * 1024); // 2MB
            let _ = sock_ref.set_recv_buffer_size(512 * 1024);
            let send = sock_ref.send_buffer_size().unwrap_or(0);
            let recv = sock_ref.recv_buffer_size().unwrap_or(0);
            info!("Socket bound to {} (send_buf={}KB, recv_buf={}KB)", bind_addr, send/1024, recv/1024);
        }
        // Mark the socket non-inheritable on Windows. Default WSASocket
        // handles are inheritable, which means any child process we spawn
        // (e.g. the auto-restart helper) keeps the socket alive after we
        // exit, blocking the new process from binding the same port for
        // ~tens of seconds. Clearing HANDLE_FLAG_INHERIT here makes the
        // socket exclusively ours so it closes the instant we exit.
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawSocket;
            extern "system" {
                fn SetHandleInformation(
                    h_object: usize,
                    dw_mask: u32,
                    dw_flags: u32,
                ) -> i32;
            }
            const HANDLE_FLAG_INHERIT: u32 = 0x0000_0001;
            unsafe {
                SetHandleInformation(
                    socket.as_raw_socket() as usize,
                    HANDLE_FLAG_INHERIT,
                    0,
                );
            }
        }

        // The QUIC endpoint on that socket (since build 133 the server's only
        // one, on the port the operator set - 4580 by default), and with the
        // relay also the relay's: one endpoint for both. A server that cannot
        // run it does not start: there is nothing else for a client to reach.
        let via_relay = quic_relay.is_some();
        let direct = crate::config::direct_connections(&config);
        let started = socket
            .into_std()
            .and_then(|std_socket| sdr_remote_quic::QuicServer::start_on(std_socket, sdr_remote_quic::Congestion::Bbr, quic_relay, direct));
        let socket = match started {
            Ok((server, link)) => {
                // Named in the brief before it was built (working rule 11, build 133).
                info!(
                    "QUIC: listening on {}{}{}",
                    bind_addr,
                    if via_relay { " and via the relay" } else { "" },
                    // Named in the brief before it was built (working rule 11, build 135).
                    if direct == sdr_remote_quic::Direct::RelayOnly { " - direct connections refused (relay only)" } else { "" }
                );
                let _ = bind_status_slot.set(
                    crate::audio_stats::BindStatus::Ok { addr: bind_addr.to_string() },
                );
                // The permit to punch follows the session (build 127).
                {
                    let mut s = session.lock().await;
                    s.watch_admissions(Arc::new(crate::tracked_socket::PunchPermits(server.punch_gate())));
                    // The way each client takes, for the status panel (build 143).
                    s.see_ways(Arc::new(crate::tracked_socket::WaysOf(server.ways())));
                }
                let sock = TrackedSocket::new(server, link);
                // Step 4's way in, a development build only (build 256).
                #[cfg(feature = "naspelen")]
                let sock = match crate::trial::get().and_then(|t| t.replay.clone()) {
                    Some(_) => {
                        let (sock, to_server) = sock.with_trial_inbound();
                        let _ = TRIAL_INBOUND.set(to_server);
                        sock
                    }
                    None => sock,
                };
                sock
            }
            Err(e) => {
                let entry = format!("[QUIC-FAIL] ts={} addr={} error={}\n", ts, bind_addr, e);
                write_bind_diag(&entry);
                let _ = bind_status_slot.set(
                    crate::audio_stats::BindStatus::Failed {
                        addr: bind_addr.to_string(),
                        error: format!("QUIC: {}", e),
                    },
                );
                error!("QUIC: could not start on {} ({}) - the server does not start", bind_addr, e);
                return Err(anyhow::Error::new(e)).context("start QUIC");
            }
        };

        Ok(Self {
            socket: Arc::new(socket),
            session,
            ptt,
            spectrum,
            rx2_spectrum,
            shutdown,
            vrx_mgr: Arc::new(std::sync::Mutex::new(crate::vrx_manager::PerClientVrxManager::new())),
            amplitec,
            tuners,
            spe,
            rf2k,
            ultrabeam,
            rotor,
            config,
            tuner_cat_rx,
            drive_level_shared,
            active_pa_shared,
            dxcluster,
            vfo_freq_shared,
            vfo_b_freq_shared,
            yaesu_ptt_flag: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            yaesu2_ptt_flag: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            yaesu,
            yaesu2,
            cw,
            cw_lines,
            cw_due: cw_due.expect("the playout channel is made just above"),
            cw_denied: crate::tx_denial::DenialLog::default(),
            audio_stats,
            tci_probe,
            server_start,
        })
    }

    /// Turn both spectrum processors on/off based on ALL consumers.
    ///
    /// The RX1 processor feeds both the RX1 spectrum and VRX1 high-res; the
    /// RX2 processor does the same for RX2 and VRX2. A processor may therefore
    /// only turn off once neither consumer still needs it. Earlier this looked
    /// only at the RX* spectrum subscribers, which made VRX high-res stall as
    /// soon as the user turned off the RX spectrum.
    ///
    /// Call after every change to a spectrum subscription (RX1, RX2, VRX1,
    /// VRX2). The disconnect path deliberately does NOT call this: thanks to
    /// the per-stream guards in the tick no packets go out anyway, and the
    /// state recovers on the next subscription change.
    ///
    /// Also recomputes the frame rate. It is global per processor and was
    /// earlier derived only from the RX spectrum subscribers. Because VRX
    /// high-res now shares the same frame gate, an old high RX fps would remain
    /// set after the RX spectrum is turned off - and then precisely the largest
    /// stream (4096 bins) runs unnecessarily fast. `spectrum_max_fps()` falls
    /// back to the default once there are no more RX spectrum subscribers, which
    /// gives exactly the desired VRX-only behaviour.
    async fn refresh_spectrum_processors(&self) {
        let (rx1_spec, rx1_vrx, rx2_spec, rx2_vrx, fps) = {
            let sess = self.session.lock().await;
            (
                sess.spectrum_addrs().len(),
                sess.vrx_spectrum_addrs(0).len(),
                sess.rx2_spectrum_clients().len(),
                sess.vrx_spectrum_addrs(1).len(),
                sess.spectrum_max_fps(),
            )
        };
        let rx1_needed = rx1_spec > 0 || rx1_vrx > 0;
        let rx2_needed = rx2_spec > 0 || rx2_vrx > 0;
        {
            let mut sp = self.spectrum.lock().await;
            sp.set_enabled(rx1_needed);
            sp.set_fps(fps);
        }
        // The RX2 processor deliberately gets NO fps: `Rx2SpectrumFps` only stores
        // the value in the session and is never applied, so that processor has run
        // on the default for ages. Imposing the RX1 fps here would change RX2
        // behaviour without cause.
        self.rx2_spectrum.lock().await.set_enabled(rx2_needed);
        // One line that shows which streams have consumers and what that does to
        // the processors - enough to trace a "why don't I see any spectrum"
        // complaint at a glance.
        info!(
            "spectrum procs: rx1={} (spec={} vrx1={} fps={}) rx2={} (spec={} vrx2={})",
            if rx1_needed { "on" } else { "off" }, rx1_spec, rx1_vrx, fps,
            if rx2_needed { "on" } else { "off" }, rx2_spec, rx2_vrx
        );
    }

    /// Let go of one transmitter, now - the radio side of a release; who held
    /// it is the session's business. Exhaustive on purpose: a new transmitter
    /// does not compile until it says how it is let go.
    async fn release_transmitter(&self, target: crate::session::TxTarget) {
        match target {
            crate::session::TxTarget::Thetis => {
                let mut p = self.ptt.lock().await;
                p.cancel_prefill();
                p.release().await;
            }
            crate::session::TxTarget::Yaesu1 => {
                if let Some(ref y) = self.yaesu {
                    y.send_command(crate::yaesu::YaesuCmd::SetPtt(false));
                }
                self.yaesu_ptt_flag.store(false, Ordering::Relaxed);
            }
            crate::session::TxTarget::Yaesu2 => {
                if let Some(ref y) = self.yaesu2 {
                    y.send_command(crate::yaesu::YaesuCmd::SetPtt(false));
                }
                self.yaesu2_ptt_flag.store(false, Ordering::Relaxed);
            }
        }
    }

    /// A radio and the flag that says this server keyed it (build 282).
    fn radio_and_flag(&self, target: crate::session::TxTarget) -> Option<(&crate::yaesu::YaesuRadio, &std::sync::atomic::AtomicBool)> {
        match target {
            crate::session::TxTarget::Yaesu1 => self.yaesu.as_deref().map(|y| (y, &*self.yaesu_ptt_flag)),
            crate::session::TxTarget::Yaesu2 => self.yaesu2.as_deref().map(|y| (y, &*self.yaesu2_ptt_flag)),
            crate::session::TxTarget::Thetis => None,
        }
    }

    /// Key a radio for `addr`, which holds it now: its first packet with the
    /// flag came in (`radio_audio!`, build 282). The mirror of
    /// `release_played`. Auto-DFM (FM <-> DATA-FM) and the memory-mode skip
    /// are `YaesuCmd::SetPtt`'s own (build 12).
    fn key_radio(&self, target: crate::session::TxTarget, addr: SocketAddr) {
        let Some((y, flag)) = self.radio_and_flag(target) else { return };
        let label = {
            let st = y.status_arc();
            let st = st.lock().unwrap();
            st.tx_timing.request(true);
            st.tx_timing.label()
        };
        y.send_command(crate::yaesu::YaesuCmd::SetPtt(true));
        flag.store(true, Ordering::Relaxed);
        // Named in advance (working rule 11, build 282).
        info!("{} PTT: keyed by the first PTT frame from {}", label, addr);
    }

    /// A packet with the flag asks for its transmitter (`tx_intake::Intake::Claim`,
    /// build 285): the radio's own refusal first (`radio_refusal`: a Yaesu's
    /// `tx_start_refusal`, none for Thetis), then the session's. A refusal is
    /// told to the client at the pace `denied` allows; a grant ends the
    /// attempt. Whether it was granted. Thetis and the radios each had their
    /// own copy of this until build 285.
    async fn claim_tx(
        &self,
        target: crate::session::TxTarget,
        addr: SocketAddr,
        radio_refusal: Option<&'static str>,
        denied: &mut crate::tx_denial::DenialLog,
    ) -> bool {
        let refusal = match radio_refusal {
            Some(why) => Some((why, Denial::Refused)),
            None => {
                let mut s = self.session.lock().await;
                if s.try_acquire_tx(target, addr) {
                    None
                } else {
                    let r = s.acquire_refusal();
                    Some((r.why, r.denial))
                }
            }
        };
        let Some((why, denial)) = refusal else {
            denied.attempt_over(&addr);
            return true;
        };
        let first = denied.is_first(&addr);
        let now_ms = self.server_start.elapsed().as_millis() as u64;
        if denied.should_send(addr, now_ms) {
            if first {
                info!("Client {} asked for {} TX but {} - refused", addr, target.radio_name(), why);
            }
            let mut buf = [0u8; PttDeniedPacket::SIZE];
            PttDeniedPacket::serialize(&mut buf, denied_target_of(target), denial);
            let _ = self.socket.send_to(&buf, addr).await;
        }
        false
    }

    /// The holder's release frame came out of the radio's buffer (build 282):
    /// everything before it is with the sound card. The holder lets go, and
    /// the radio goes off once the sound card played it (`asked_release`, the
    /// wait in the poll loop). Nothing when the radio was let go another way
    /// meanwhile, or a new press keyed it again (`over`).
    async fn release_played(&self, played: crate::tx_recover::ReleasePlayed) {
        let target = played.target;
        let Some((y, flag)) = self.radio_and_flag(target) else { return };
        if y.status_arc().lock().unwrap().tx_timing.over() != played.over {
            return;
        }
        if !self.session.lock().await.release_tx(target, played.from) {
            return;
        }
        let label = {
            let st = y.status_arc();
            let st = st.lock().unwrap();
            st.tx_timing.request(false);
            st.tx_timing.label()
        };
        y.send_command(crate::yaesu::YaesuCmd::SetPtt(false));
        flag.store(false, Ordering::Relaxed);
        // Named in advance (working rule 11, build 282).
        info!("{} PTT: the release frame from {} played - TX0 after the sound card", label, played.from);
    }

    /// The safety task stopped while the server runs. That task is what ends a
    /// transmission nobody ends: the heartbeat watchdog (`check_safety`), the
    /// time-out that takes a transmitter from a client gone silent
    /// (`check_timeout`) and the CW key watchdog. Without it a client that
    /// vanishes mid-transmission leaves the radio on the air - and a Yaesu
    /// keyed on its RTS line has no watchdog of its own at all.
    ///
    /// So everything is let go, and transmitting stays refused until the
    /// server is restarted (owner's decision, 2026-09-29). The one block for
    /// this; `PTT-NOOIT-HANGEN.md` §4.6.
    async fn safety_task_lost(&self, task: &str, why: &str) {
        error!(
            "SAFETY: {} stopped ({}) - every transmitter that is ours released, transmitting refused until the server is restarted (the [PANIC] entry in this log says where)",
            task, why
        );
        // What the server keys on its own (tune carrier, ZZTX1) goes past the
        // session; this gate stops it. The dead task was the one that set it.
        self.ptt.lock().await.tx_blocked_handle().store(true, Ordering::Relaxed);
        let closed = self.session.lock().await.close_tx();
        for (target, addr) in &closed.held {
            error!("SAFETY: {} held {} - taken away", addr, target.radio_name());
        }
        // Every transmitter that is ours: held by a client, or keyed by this
        // server with nobody holding it (the orphan case). Not one that is on
        // the air by an operator at the radio - forcing RX there cuts them off,
        // the same rule as `release_on_reopen` and the orphan watch.
        // None of these waits on the radio - TCI goes out with `try_send`, a Yaesu
        // command is a channel send - so the key lines below are reached at once
        // (review build 57: said here, so a change that makes one wait is seen).
        for target in crate::session::TxTarget::ALL {
            let server_keyed = match target {
                crate::session::TxTarget::Thetis => self.ptt.lock().await.is_transmitting(),
                crate::session::TxTarget::Yaesu1 => self.yaesu_ptt_flag.load(Ordering::Relaxed),
                crate::session::TxTarget::Yaesu2 => self.yaesu2_ptt_flag.load(Ordering::Relaxed),
            };
            if ours_to_let_go(closed.held.iter().any(|(t, _)| *t == target), server_keyed) {
                self.release_transmitter(target).await;
            }
        }
        // A CW key that is down comes up through the one writer, then the
        // Yaesu key lines go down and stay closed for this run.
        if let Some(up) = crate::cw_playout::lock(&self.cw).release_owner(Instant::now()) {
            let _ = self.cw_due.send(up);
        }
        let down = lower_all_key_lines(&self.cw_lines);
        self.tell_all_closed(closed, down).await;
    }

    /// Told LAST: the radios first, the clients after - a send that waits on a
    /// full socket must not stand between a loss and a key line going down
    /// (review 61). The order is a type, not an anchor: `KeyLinesDown` exists
    /// only once the lines are down, and `closed.tell` only once the server is
    /// closed, so this cannot be called earlier (review 63). Every client,
    /// holder or not: a holder's button lets go (one of the refusals names the
    /// transmitter it asks for) and a held key does not keep asking; a listener
    /// notes the closed server instead of learning it at its first press - the
    /// phone, listening, was told nothing (field test build 60).
    async fn tell_all_closed(&self, closed: crate::session::TxClosed, _after: KeyLinesDown) {
        for addr in closed.tell {
            self.tell_if_closed(addr).await;
        }
    }

    /// Step 4 (build 256, a development build only): play the drive of
    /// `--inspelen` in through the trial's way in, `START_DELAY` after the start,
    /// as the trial client, whose session is let in again before a packet if it
    /// went (a time-out between overs).
    #[cfg(feature = "naspelen")]
    fn start_trial_replay(&self) {
        let Some(path) = crate::trial::get().and_then(|t| t.replay.clone()) else { return };
        let Some(to_server) = TRIAL_INBOUND.get().cloned() else {
            warn!("Trial: no way in for {} - the replay does not start", path);
            return;
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                warn!("Trial: {}: {} - the replay does not start", path, e);
                return;
            }
        };
        let arrivals = crate::tx_replay::parse_measurement(&text);
        let sound = match crate::trial::get().and_then(|t| t.sound.clone()) {
            None => None,
            Some(p) => match std::fs::read(&p).map_err(|e| e.to_string()).and_then(|b| crate::trial::read_wav_16k(&b)) {
                Ok(s) => {
                    // Named in advance (working rule 11, build 257).
                    info!("Trial: the played-in packets carry {} ({} s of sound)", p, s.len() / 16_000);
                    Some(s)
                }
                Err(e) => {
                    warn!("Trial: {}: {} - the replay does not start", p, e);
                    return;
                }
            },
        };
        let audio = match crate::trial::audio_sends(&arrivals, sound.as_deref()) {
            Ok(a) => a,
            Err(e) => {
                warn!("Trial: no Opus encoder ({}) - the replay does not start", e);
                return;
            }
        };
        let ptt: Vec<bool> = arrivals.iter().map(|a| a.ptt).collect();
        let sends = crate::trial::schedule(audio, &ptt);
        // Named in advance (working rule 11, build 256).
        info!(
            "Trial: replaying {} TX arrivals from {} in {} s, as client {}",
            arrivals.len(),
            path,
            crate::trial::START_DELAY.as_secs(),
            crate::trial::TRIAL_CLIENT
        );
        let session = self.session.clone();
        let admit = move || {
            if session.blocking_lock().admit_trial_client(crate::trial::TRIAL_CLIENT) {
                info!("Trial: client {} admitted", crate::trial::TRIAL_CLIENT);
            }
        };
        let done = |n: usize, took: Duration| info!("Trial: replay done - {} packets in {} s", n, took.as_secs());
        crate::trial::play(sends, Instant::now() + crate::trial::START_DELAY, to_server, admit, done);
    }

    /// Tell this client that transmitting is closed, if it is: a refusal per
    /// transmitter, `Denial::Closed`. The one way a client learns it, for a
    /// client that has just authenticated (after the accept, so its clean-up
    /// of the new session - `session_lost!` on AUTH_ACCEPTED - has happened)
    /// and for every client at the moment of the loss (`safety_task_lost`). A
    /// client asking for none of the transmitters lets go of nothing and notes
    /// the closed server; one asking for a transmitter lets that one go.
    async fn tell_if_closed(&self, addr: SocketAddr) {
        if self.session.lock().await.acquire_refusal().denial != Denial::Closed {
            return;
        }
        for target in crate::session::TxTarget::ALL {
            let mut buf = [0u8; PttDeniedPacket::SIZE];
            PttDeniedPacket::serialize(&mut buf, denied_target_of(target), Denial::Closed);
            let _ = self.socket.send_to(&buf, addr).await;
        }
    }

    pub async fn run(mut self) -> Result<()> {
        let start = Instant::now();
        // Step 4, a development build only (build 256): a drive played in.
        #[cfg(feature = "naspelen")]
        self.start_trial_replay();
        // Developer only (build 230): the arrival of every Thetis TX packet.
        let mut tx_measure = if self.config.measuring() {
            let dir = crate::server_log_path().parent().map(|d| d.to_path_buf()).unwrap_or_default();
            match crate::tx_arrivals::TxArrivals::start(&dir, &sdr_remote_core::version_string()) {
                Ok(m) => {
                    // Named in advance (`logregels-build230.md`).
                    info!("Measurement (server) on: {}", m.path().display());
                    Some(m)
                }
                Err(e) => {
                    warn!("Measurement (server) could not start in {}: {}", dir.display(), e);
                    None
                }
            }
        } else {
            None
        };
        let yaesu = self.yaesu.clone();
        let yaesu2 = self.yaesu2.clone();

        // TCI is the only audio/IQ/control path
        let playback_rate = 48000u32;

        info!("TCI mode: audio at 48kHz via WebSocket");

        // TCI IQ consumer task: drain IQ channels -> spectrum processor
        let tci_iq_handle = {
            let spectrum = self.spectrum.clone();
            let rx2_spectrum = self.rx2_spectrum.clone();
            let ptt = self.ptt.clone();
            let mut shutdown = self.shutdown.clone();
            let socket = self.socket.clone();
            let session = self.session.clone();
            let vrx_mgr = self.vrx_mgr.clone();
            tokio::spawn(async move {
                crate::audio_loops::tci_iq_consumer(ptt, spectrum, rx2_spectrum, &mut shutdown, socket, session, vrx_mgr).await;
            })
        };

        // Extract TCI audio receivers from PttController
        let (tci_rx1_audio, tci_rx2_audio, tci_bin_r_audio) = {
            let mut ptt = self.ptt.lock().await;
            if let Some(tci) = Some(&mut ptt.tci) {
                (tci.rx1_audio_rx.take(), tci.rx2_audio_rx.take(), tci.bin_r_audio_rx.take())
            } else {
                (None, None, None)
            }
        };

        // Spawn single stereo audio mixer (replaces three separate RX1/RX2/BinR loops)
        use crate::audio_loops::tci_multichannel_audio_loop;

        let tx_handle = {
            let socket = self.socket.clone();
            let session = self.session.clone();
            let mut shutdown = self.shutdown.clone();
            let ptt = self.ptt.clone();
            let audio_stats = self.audio_stats.clone();
            let server_start = self.server_start;
            tokio::spawn(async move {
                if let Err(e) = tci_multichannel_audio_loop(
                    socket, session, ptt,
                    tci_rx1_audio, tci_rx2_audio, tci_bin_r_audio,
                    &mut shutdown, start,
                    audio_stats, server_start,
                ).await { log::error!("Multi-channel audio bundler error: {}", e); }
            })
        };

        // Spawn Yaesu audio TX loop
        let _yaesu_audio_handle = {
            let yaesu_audio = yaesu.as_ref().and_then(|y| {
                let rx = y.audio_rx.lock().ok().and_then(|mut a| a.take())?;
                Some((rx, y.audio_sample_rate, y.status_arc()))
            });
            if let Some((audio_rx, sample_rate, yaesu_status)) = yaesu_audio {
                let socket = self.socket.clone();
                let session = self.session.clone();
                let mut shutdown = self.shutdown.clone();
                let audio_stats = self.audio_stats.clone();
                let server_start = self.server_start;
                Some(tokio::spawn(async move {
                    if let Err(e) = crate::audio_loops::yaesu_audio_loop(socket, session, audio_rx, sample_rate, &mut shutdown, start, audio_stats, server_start, 0, yaesu_status).await {
                        log::error!("Yaesu audio loop error: {}", e);
                    }
                }))
            } else {
                None
            }
        };

        // Slot-1 RX audio loop (Option B-prime) - exact mirror of slot 0,
        // but broadcasts to yaesu2_addrs as AudioYaesu2 (slot=1).
        let _yaesu2_audio_handle = {
            let yaesu2_audio = yaesu2.as_ref().and_then(|y| {
                let rx = y.audio_rx.lock().ok().and_then(|mut a| a.take())?;
                Some((rx, y.audio_sample_rate, y.status_arc()))
            });
            if let Some((audio_rx, sample_rate, yaesu2_status)) = yaesu2_audio {
                let socket = self.socket.clone();
                let session = self.session.clone();
                let mut shutdown = self.shutdown.clone();
                let audio_stats = self.audio_stats.clone();
                let server_start = self.server_start;
                Some(tokio::spawn(async move {
                    if let Err(e) = crate::audio_loops::yaesu_audio_loop(socket, session, audio_rx, sample_rate, &mut shutdown, start, audio_stats, server_start, 1, yaesu2_status).await {
                        log::error!("[radio1] audio loop error: {}", e);
                    }
                }))
            } else {
                None
            }
        };

        // PATCH-2 operator-feedback (2026-05-13): drive the TCI status probe
        // from an independent 1Hz ticker so the Status panel reflects reality
        // even when no clients are connected (heartbeat-driven updates only
        // fire on client traffic - without this the panel would say
        // "Disconnected since start" while Thetis is happily attached).
        let _tci_probe_ticker = {
            let ptt = self.ptt.clone();
            let probe = self.tci_probe.clone();
            let server_start = self.server_start;
            let mut shutdown = self.shutdown.clone();
            tokio::spawn(async move {
                // 250 ms since build 272: the panel shows Thetis transmitting
                // from here, and a second was late for a press.
                let mut tick = interval(Duration::from_millis(250));
                loop {
                    tokio::select! {
                        _ = tick.tick() => {
                            let tci_up = {
                                let p = ptt.lock().await;
                                // One definition of "Thetis transmits" (build 273).
                                probe.set_transmitting(p.tx_now().thetis);
                                p.tci_connected()
                            };
                            probe.update(tci_up, server_start);
                        }
                        _ = shutdown.changed() => break,
                    }
                }
            })
        };

        // Spawn Yaesu state broadcast task (separate from Thetis broadcast)
        let yaesu_state_handle = {
            let yaesu = yaesu.clone();
            let ptt_flag = self.yaesu_ptt_flag.clone();
            let socket = self.socket.clone();
            let session = self.session.clone();
            let mut shutdown = self.shutdown.clone();
            if yaesu.is_some() {
                Some(tokio::spawn(async move {
                    let mut tick = interval(Duration::from_millis(200));
                    let mut last_features0: u32 = u32::MAX; // value-change-only feature-state
                    let mut last_levels0 = [255u8; YaesuFeaturePacket::N_LEVELS];
                    let mut last_freqs0 = [u16::MAX; YaesuFeaturePacket::N_FREQS];
                    // Fresh subscribers get the current feature-state once (sent-set),
                    // so a just-connected client immediately has the correct AGC/IPO/NB/DNR/Proc
                    // states (otherwise the first cycle-click is a no-op on stale 0).
                    let mut sent_features0: std::collections::HashSet<std::net::SocketAddr> = std::collections::HashSet::new();
                    // The memory list follows the same rule as the feature-state above:
                    // a fresh subscriber gets the current list once, and after that it is
                    // sent only when it CHANGES. The client therefore never has to ask for
                    // it, and can never hold something the server does not have.
                    let mut sent_memory0: std::collections::HashSet<std::net::SocketAddr> = std::collections::HashSet::new();
                    // The EX/menu values follow the same rule, on their own tick-list.
                    let mut sent_menu0: std::collections::HashSet<std::net::SocketAddr> = std::collections::HashSet::new();
                    let mut last_resend0 = std::time::Instant::now();
                    let mut last_blob_resend0 = std::time::Instant::now();
                    // Presence-based SSB routing (opt-out mode, per-PTT OFF): turn the USB
                    // routing on as long as a client is on radio 1, and restore ~2s after
                    // the last one leaves. In per-PTT mode the SetPtt handler does this.
                    let mut ssb_applied0 = false;
                    let mut absent_ticks0: u32 = 0;
                    // Its safety nets (`RadioSafety`, one block for both radios
                    // since build 289).
                    let mut safety = RadioSafety::new(crate::session::TxTarget::Yaesu1);
                    let mut joined0 = crate::session::GenerationWatch::new();
                    // Why the blob tick-list was last cleared, so the log names the real
                    // cause instead of always blaming the safety net.
                    let mut blob_reason0: &'static str = "safety-net top-up";
                    loop {
                        tokio::select! {
                            _ = tick.tick() => {
                                // Safety net: periodically forget who has what, so a lost
                                // push is repaired. The small feature-state runs at the fast
                                // interval; the memory list and the EX settings are kilobytes
                                // each and run at the slow one.
                                if last_resend0.elapsed().as_secs() >= STATE_RESEND_SECS {
                                    last_resend0 = std::time::Instant::now();
                                    sent_features0.clear();
                                }
                                if last_blob_resend0.elapsed().as_secs() >= BLOB_RESEND_SECS {
                                    last_blob_resend0 = std::time::Instant::now();
                                    sent_memory0.clear();
                                    sent_menu0.clear();
                                    blob_reason0 = "safety-net top-up";
                                }
                                // Someone joined (see SessionManager::connect_generation).
                                // A client that dropped without saying so and came back on
                                // the same address inside the session timeout is still
                                // ticked off, so it would wait for the slow resend above -
                                // up to a minute with an empty memory table. Offer
                                // everything again instead; it is kilobytes on a rare event.
                                {
                                    let gen = session.lock().await.connect_generation();
                                    if joined0.changed(gen) {
                                        sent_features0.clear();
                                        sent_memory0.clear();
                                        sent_menu0.clear();
                                        blob_reason0 = "a client joined";
                                    }
                                }
                                // Audio subscribers (for SSB-USB routing/TX) and state
                                // subscribers (window-open OR audio) separately, under one lock.
                                let (audio_addrs, addrs, tx_holder) = {
                                    let s = session.lock().await;
                                    (
                                        s.yaesu_addrs(),
                                        s.yaesu_state_addrs(),
                                        s.tx_holder(crate::session::TxTarget::Yaesu1),
                                    )
                                };
                                safety.tick(&session, &socket, yaesu.as_deref(), &ptt_flag, tx_holder).await;
                                if let Some(ref y) = yaesu {
                                    let st = y.status();
                                    if !st.ssb_switch_on_ptt {
                                        absent_ticks0 = if audio_addrs.is_empty() { absent_ticks0.saturating_add(1) } else { 0 };
                                        let step = PresenceRouting { applied: ssb_applied0, clients: !audio_addrs.is_empty(), absent_ticks: absent_ticks0 };
                                        if let Some(on) = step.step(&st) {
                                            y.send_command(crate::yaesu::YaesuCmd::SetSsbRouting(on));
                                            ssb_applied0 = on;
                                        }
                                    }
                                }
                                // State/feature/memory to the STATE subscribers.
                                if addrs.is_empty() { continue; }
                                if let Some(ref y) = yaesu {
                                    let ys = y.status();
                                    let pkt = YaesuStatePacket {
                                        // False in the shared copy; a second copy with it
                                        // set goes to everyone who is not holding this
                                        // radio. The holder must never be told its own
                                        // transmission belongs to somebody else - that is
                                        // the false flash this field exists to end.
                                        held_by_other: false,
                                        freq_a: ys.vfo_a_freq,
                                        freq_b: ys.vfo_b_freq,
                                        mode: ys.mode,
                                        smeter: if ys.connected { ys.smeter } else { 0 },
                                        tx_active: ys.reported_tx(),
                                        power_on: if ys.connected { ys.power_on } else { false },
                                        af_gain: ys.af_gain,
                                        tx_power: ys.tx_power,
                                        vfo_select: ys.vfo_select,
                                        memory_channel: ys.memory_channel,
                                        squelch: ys.squelch,
                                        rf_gain: ys.rf_gain,
                                        mic_gain: ys.mic_gain,
                                        split: ys.split_active,
                                        scan: ys.scan_active,
                                        tuner_state: ys.tuner_state,
                                        hi_swr: if ys.connected { ys.hi_swr } else { false },
                                        tx_power_max: ys.tx_power_max(), loading: if ys.connected { ys.loading } else { 0 },
                                    };
                                    let mut buf = [0u8; YaesuStatePacket::SIZE];
                                    pkt.serialize(&mut buf);
                                    // Same numbers, one flag apart. Taken from the
                                    // ownership table rather than the radio, so it turns
                                    // over when the lock changes hands instead of when
                                    // the radio stops - which is what removes the flash.
                                    let held_buf = tx_holder.map(|_| {
                                        let mut b = [0u8; YaesuStatePacket::SIZE];
                                        YaesuStatePacket { held_by_other: true, ..pkt }.serialize(&mut b);
                                        b
                                    });
                                    for addr in &addrs {
                                        match &held_buf {
                                            Some(b) if tx_holder != Some(*addr) => { let _ = socket.try_send_to(b, *addr); }
                                            _ => { let _ = socket.try_send_to(&buf, *addr); }
                                        }
                                    }
                                    // Feature-state (toggles + levels): value-change-only push
                                    // + initial push to fresh subscribers (sent-set). Mark an
                                    // addr only on a successful send -> retry next tick on error.
                                    let feat_changed0 = ys.feature_toggles != last_features0 || ys.feature_levels != last_levels0 || ys.feature_freqs != last_freqs0;
                                    if feat_changed0 {
                                        last_features0 = ys.feature_toggles;
                                        last_levels0 = ys.feature_levels;
                                        last_freqs0 = ys.feature_freqs;
                                        sent_features0.clear(); // everyone gets the new state
                                    }
                                    sent_features0.retain(|a| addrs.contains(a)); // prune disconnected
                                    if feat_changed0 || addrs.iter().any(|a| !sent_features0.contains(a)) {
                                        let fp = YaesuFeaturePacket { slot: 0, toggles: ys.feature_toggles, levels: ys.feature_levels, freqs: ys.feature_freqs };
                                        let mut fbuf = [0u8; YaesuFeaturePacket::SIZE];
                                        fp.serialize(&mut fbuf);
                                        for addr in &addrs {
                                            if feat_changed0 || !sent_features0.contains(addr) {
                                                if socket.try_send_to(&fbuf, *addr).is_ok() { sent_features0.insert(*addr); }
                                            }
                                        }
                                    }

                                    // Memory list. Two reasons to send: it just changed (a
                                    // read, or a write by us) -> everyone; or a subscriber is
                                    // here that has never had it -> that subscriber only.
                                    // Memory list, pushed per the state-sync rule.
                                    let (blob0, targets0, blob0_changed) = blob_push_targets(
                                        &y.memory_data,
                                        || y.status().last_memory_blob.clone(),
                                        &addrs, &mut sent_memory0);
                                    if let Some(text) = blob0 {
                                        if !targets0.is_empty() {
                                            let done = push_blob(&socket, PacketType::YaesuMemoryData, &text, &targets0);
                                            for a in &done { sent_memory0.insert(*a); }
                                            // A change is an event; a safety-net top-up is not.
                                            if blob0_changed {
                                                info!("Sent Yaesu memory data to {} clients ({}B)", done.len(), text.len());
                                            } else {
                                                log::debug!("Sent Yaesu memory data ({}) to {} clients ({}B)", blob_reason0, done.len(), text.len());
                                                blob_reason0 = "new subscriber";
                                            }
                                        }
                                    }

                                    // EX/menu values, same rule, own tick-list. Kept
                                    // separate from the memory list so neither can
                                    // overwrite the other in a 200ms window.
                                    let (menu0, mtargets0, menu0_changed) = blob_push_targets(
                                        &y.menu_data,
                                        || y.status().last_menu_blob.clone().map(|b| format!("MENU:{}", b)),
                                        &addrs, &mut sent_menu0);
                                    if let Some(text) = menu0 {
                                        if !mtargets0.is_empty() {
                                            let done = push_blob(&socket, PacketType::YaesuMemoryData, &text, &mtargets0);
                                            for a in &done { sent_menu0.insert(*a); }
                                            // A change is an event; a safety-net top-up is not.
                                            if menu0_changed {
                                                info!("Sent Yaesu EX settings to {} clients ({}B)", done.len(), text.len());
                                            } else {
                                                log::debug!("Sent Yaesu EX settings ({}) to {} clients ({}B)", blob_reason0, done.len(), text.len());
                                                blob_reason0 = "new subscriber";
                                            }
                                        }
                                    }
                                }
                            }
                            _ = shutdown.changed() => break,
                        }
                    }
                }))
            } else {
                None
            }
        };

        // Slot-1 state broadcast (Option B-prime) - mirror of slot 0, but as
        // YaesuState2 / YaesuMemoryData2 to yaesu2_addrs (subscription-gated).
        let yaesu2_state_handle = {
            let yaesu2 = yaesu2.clone();
            let ptt_flag = self.yaesu2_ptt_flag.clone();
            let socket = self.socket.clone();
            let session = self.session.clone();
            let mut shutdown = self.shutdown.clone();
            if yaesu2.is_some() {
                Some(tokio::spawn(async move {
                    let mut tick = interval(Duration::from_millis(200));
                    let mut last_features1: u32 = u32::MAX; // value-change-only feature-state
                    let mut last_levels1 = [255u8; YaesuFeaturePacket::N_LEVELS];
                    let mut last_freqs1 = [u16::MAX; YaesuFeaturePacket::N_FREQS];
                    // Fresh subscribers get the current feature-state once (see slot 0).
                    let mut sent_features1: std::collections::HashSet<std::net::SocketAddr> = std::collections::HashSet::new();
                    // Same rule as slot 0: the list once per subscriber, then on change only.
                    let mut sent_memory1: std::collections::HashSet<std::net::SocketAddr> = std::collections::HashSet::new();
                    let mut sent_menu1: std::collections::HashSet<std::net::SocketAddr> = std::collections::HashSet::new();
                    let mut last_resend1 = std::time::Instant::now();
                    let mut last_blob_resend1 = std::time::Instant::now();
                    // Presence-based SSB routing (opt-out mode) for radio 2 (FTX-1).
                    let mut ssb_applied1 = false;
                    let mut absent_ticks1: u32 = 0;
                    // Its safety nets (`RadioSafety`, one block for both radios
                    // since build 289).
                    let mut safety = RadioSafety::new(crate::session::TxTarget::Yaesu2);
                    let mut joined1 = crate::session::GenerationWatch::new();
                    // Why the blob tick-list was last cleared, so the log names the real
                    // cause instead of always blaming the safety net.
                    let mut blob_reason1: &'static str = "safety-net top-up";
                    loop {
                        tokio::select! {
                            _ = tick.tick() => {
                                // Safety net: periodically forget who has what, so a lost
                                // push is repaired. The small feature-state runs at the fast
                                // interval; the memory list and the EX settings are kilobytes
                                // each and run at the slow one.
                                if last_resend1.elapsed().as_secs() >= STATE_RESEND_SECS {
                                    last_resend1 = std::time::Instant::now();
                                    sent_features1.clear();
                                }
                                if last_blob_resend1.elapsed().as_secs() >= BLOB_RESEND_SECS {
                                    last_blob_resend1 = std::time::Instant::now();
                                    sent_memory1.clear();
                                    sent_menu1.clear();
                                    blob_reason1 = "safety-net top-up";
                                }
                                // Someone joined (see SessionManager::connect_generation).
                                // A client that dropped without saying so and came back on
                                // the same address inside the session timeout is still
                                // ticked off, so it would wait for the slow resend above -
                                // up to a minute with an empty memory table. Offer
                                // everything again instead; it is kilobytes on a rare event.
                                {
                                    let gen = session.lock().await.connect_generation();
                                    if joined1.changed(gen) {
                                        sent_features1.clear();
                                        sent_memory1.clear();
                                        sent_menu1.clear();
                                        blob_reason1 = "a client joined";
                                    }
                                }
                                let (audio_addrs, addrs, tx_holder) = {
                                    let s = session.lock().await;
                                    (
                                        s.yaesu2_addrs(),
                                        s.yaesu2_state_addrs(),
                                        s.tx_holder(crate::session::TxTarget::Yaesu2),
                                    )
                                };
                                safety.tick(&session, &socket, yaesu2.as_deref(), &ptt_flag, tx_holder).await;
                                if let Some(ref y) = yaesu2 {
                                    let st = y.status();
                                    if !st.ssb_switch_on_ptt {
                                        absent_ticks1 = if audio_addrs.is_empty() { absent_ticks1.saturating_add(1) } else { 0 };
                                        let step = PresenceRouting { applied: ssb_applied1, clients: !audio_addrs.is_empty(), absent_ticks: absent_ticks1 };
                                        if let Some(on) = step.step(&st) {
                                            y.send_command(crate::yaesu::YaesuCmd::SetSsbRouting(on));
                                            ssb_applied1 = on;
                                        }
                                    }
                                }
                                if addrs.is_empty() { continue; }
                                if let Some(ref y) = yaesu2 {
                                    let ys = y.status();
                                    let pkt = YaesuStatePacket {
                                        // False in the shared copy; a second copy with it
                                        // set goes to everyone who is not holding this
                                        // radio. The holder must never be told its own
                                        // transmission belongs to somebody else - that is
                                        // the false flash this field exists to end.
                                        held_by_other: false,
                                        freq_a: ys.vfo_a_freq,
                                        freq_b: ys.vfo_b_freq,
                                        mode: ys.mode,
                                        smeter: if ys.connected { ys.smeter } else { 0 },
                                        tx_active: ys.reported_tx(),
                                        power_on: if ys.connected { ys.power_on } else { false },
                                        af_gain: ys.af_gain,
                                        tx_power: ys.tx_power,
                                        vfo_select: ys.vfo_select,
                                        memory_channel: ys.memory_channel,
                                        squelch: ys.squelch,
                                        rf_gain: ys.rf_gain,
                                        mic_gain: ys.mic_gain,
                                        split: ys.split_active,
                                        scan: ys.scan_active,
                                        tuner_state: ys.tuner_state,
                                        hi_swr: if ys.connected { ys.hi_swr } else { false },
                                        tx_power_max: ys.tx_power_max(), loading: if ys.connected { ys.loading } else { 0 },
                                    };
                                    let mut buf = [0u8; YaesuStatePacket::SIZE];
                                    pkt.serialize_as_type(&mut buf, PacketType::YaesuState2);
                                    // Mirror of slot 0 above.
                                    let held_buf = tx_holder.map(|_| {
                                        let mut b = [0u8; YaesuStatePacket::SIZE];
                                        YaesuStatePacket { held_by_other: true, ..pkt }
                                            .serialize_as_type(&mut b, PacketType::YaesuState2);
                                        b
                                    });
                                    for addr in &addrs {
                                        match &held_buf {
                                            Some(b) if tx_holder != Some(*addr) => { let _ = socket.try_send_to(b, *addr); }
                                            _ => { let _ = socket.try_send_to(&buf, *addr); }
                                        }
                                    }
                                    // Feature-state (toggles + levels): value-change-only + initial
                                    // push to fresh subscribers (sent-set), slot 1. See slot 0.
                                    let feat_changed1 = ys.feature_toggles != last_features1 || ys.feature_levels != last_levels1 || ys.feature_freqs != last_freqs1;
                                    if feat_changed1 {
                                        last_features1 = ys.feature_toggles;
                                        last_levels1 = ys.feature_levels;
                                        last_freqs1 = ys.feature_freqs;
                                        sent_features1.clear();
                                    }
                                    sent_features1.retain(|a| addrs.contains(a));
                                    if feat_changed1 || addrs.iter().any(|a| !sent_features1.contains(a)) {
                                        let fp = YaesuFeaturePacket { slot: 1, toggles: ys.feature_toggles, levels: ys.feature_levels, freqs: ys.feature_freqs };
                                        let mut fbuf = [0u8; YaesuFeaturePacket::SIZE];
                                        fp.serialize(&mut fbuf);
                                        for addr in &addrs {
                                            if feat_changed1 || !sent_features1.contains(addr) {
                                                if socket.try_send_to(&fbuf, *addr).is_ok() { sent_features1.insert(*addr); }
                                            }
                                        }
                                    }

                                    // Memory list, pushed per the state-sync rule.
                                    let (blob1, targets1, blob1_changed) = blob_push_targets(
                                        &y.memory_data,
                                        || y.status().last_memory_blob.clone(),
                                        &addrs, &mut sent_memory1);
                                    if let Some(text) = blob1 {
                                        if !targets1.is_empty() {
                                            let done = push_blob(&socket, PacketType::YaesuMemoryData2, &text, &targets1);
                                            for a in &done { sent_memory1.insert(*a); }
                                            // A change is an event; a safety-net top-up is not.
                                            if blob1_changed {
                                                info!("[radio2] Sent memory data to {} clients ({}B)", done.len(), text.len());
                                            } else {
                                                log::debug!("[radio2] Sent memory data ({}) to {} clients ({}B)", blob_reason1, done.len(), text.len());
                                                blob_reason1 = "new subscriber";
                                            }
                                        }
                                    }

                                    // EX/menu values, same rule, own tick-list. Kept
                                    // separate from the memory list so neither can
                                    // overwrite the other in a 200ms window.
                                    let (menu1, mtargets1, menu1_changed) = blob_push_targets(
                                        &y.menu_data,
                                        || y.status().last_menu_blob.clone().map(|b| format!("MENU:{}", b)),
                                        &addrs, &mut sent_menu1);
                                    if let Some(text) = menu1 {
                                        if !mtargets1.is_empty() {
                                            let done = push_blob(&socket, PacketType::YaesuMemoryData2, &text, &mtargets1);
                                            for a in &done { sent_menu1.insert(*a); }
                                            // A change is an event; a safety-net top-up is not.
                                            if menu1_changed {
                                                info!("[radio2] Sent EX settings to {} clients ({}B)", done.len(), text.len());
                                            } else {
                                                log::debug!("[radio2] Sent EX settings ({}) to {} clients ({}B)", blob_reason1, done.len(), text.len());
                                                blob_reason1 = "new subscriber";
                                            }
                                        }
                                    }
                                }
                            }
                            _ = shutdown.changed() => break,
                        }
                    }
                }))
            } else {
                None
            }
        };

        // RadioInfo broadcast (Option B-prime): lets dual-radio-aware clients
        // (which sent Yaesu2Enable -> yaesu2_addrs) know the detected model per
        // slot, for the panel naming ("991A 1"/"FTX1" etc.). Which slots are
        // CONFIGURED travels in YaesuPresence, which every client gets; this
        // packet only reaches clients that asked for slot 1.
        // Old clients are never on yaesu2_addrs -> never get this new packet
        // type (back-compat by construction). 1 Hz is more than enough.
        let _radio_info_handle = {
            let yaesu = yaesu.clone();
            let yaesu2 = yaesu2.clone();
            let socket = self.socket.clone();
            let session = self.session.clone();
            let mut shutdown = self.shutdown.clone();
            if yaesu.is_some() || yaesu2.is_some() {
                Some(tokio::spawn(async move {
                    let mut tick = interval(Duration::from_millis(1000));
                    loop {
                        tokio::select! {
                            _ = tick.tick() => {
                                let addrs = session.lock().await.yaesu2_addrs();
                                if addrs.is_empty() { continue; }
                                let mut infos: Vec<(u8, u8)> = Vec::new();
                                if let Some(ref y) = yaesu { infos.push((0, y.model_code())); }
                                if let Some(ref y) = yaesu2 { infos.push((1, y.model_code())); }
                                for (slot, model) in infos {
                                    let mut buf = [0u8; Header::SIZE + 2];
                                    let mut hdr = [0u8; Header::SIZE];
                                    Header::new(PacketType::RadioInfo, Flags::NONE).serialize(&mut hdr);
                                    buf[..Header::SIZE].copy_from_slice(&hdr);
                                    buf[Header::SIZE] = slot;
                                    buf[Header::SIZE + 1] = model;
                                    for addr in &addrs {
                                        let _ = socket.try_send_to(&buf, *addr);
                                    }
                                }
                            }
                            _ = shutdown.changed() => break,
                        }
                    }
                }))
            } else {
                None
            }
        };

        // YaesuPresence broadcast (PATCH-android-yaesu-presence-datasaver): presence
        // + model per slot to ALL active clients - decouples the selector/
        // connected-status from the audio+state subscription so dropping out/coming
        // back is dynamic (not sticky). Push-on-change (prev-tuple diff) +
        // deterministic delivery to fresh clients (each addr gets the current tuple
        // once via the sent-set, <= tick-latency; a fresh client therefore does not
        // miss the diff, without ambiguous auth-timing). Value-change-only logging.
        let _yaesu_presence_handle = {
            let yaesu = yaesu.clone();
            let yaesu2 = yaesu2.clone();
            let socket = self.socket.clone();
            let session = self.session.clone();
            let mut shutdown = self.shutdown.clone();
            // Also with no Yaesu configured: the packet is how a client learns
            // the server's list of radios (RadioState::radios_known), and until it
            // has it, it does not redirect a key to another radio. Without the
            // packet a Thetis-only station would never learn that (build 130).
            {
                Some(tokio::spawn(async move {
                    let mut tick = interval(Duration::from_millis(500));
                    let mut prev: Option<(bool, u8, u8, bool, u8, u8)> = None;
                    let mut sent_addrs: std::collections::HashSet<std::net::SocketAddr> =
                        std::collections::HashSet::new();
                    let mut last_presence_resend = std::time::Instant::now();
                    let mut joined_presence = crate::session::GenerationWatch::new();
                    // Who has been TOLD about, as opposed to who has been sent to. The
                    // safety net clears `sent_addrs` every STATE_RESEND_SECS so the value
                    // goes out again, which is the point - but it meant every client was
                    // announced as a fresh subscriber every ten seconds, forever. This set
                    // is pruned the same way, so a client that leaves and returns is
                    // reported once more, and a client that simply stays is reported once.
                    let mut logged_addrs: std::collections::HashSet<std::net::SocketAddr> =
                        std::collections::HashSet::new();
                    loop {
                        tokio::select! {
                            _ = tick.tick() => {
                                // Safety net (see STATE_RESEND_SECS).
                                if last_presence_resend.elapsed().as_secs() >= STATE_RESEND_SECS {
                                    last_presence_resend = std::time::Instant::now();
                                    sent_addrs.clear();
                                }
                                // Same reason as the memory list: a rejoin inside the
                                // session timeout is invisible to the address pruning.
                                {
                                    let gen = session.lock().await.connect_generation();
                                    if joined_presence.changed(gen) {
                                        sent_addrs.clear();
                                        logged_addrs.clear();
                                    }
                                }
                                let present0 = yaesu.as_ref().map(|y| y.status().connected).unwrap_or(false);
                                let present1 = yaesu2.as_ref().map(|y| y.status().connected).unwrap_or(false);
                                let model0 = yaesu.as_ref().map(|y| y.model_code()).unwrap_or(0);
                                let model1 = yaesu2.as_ref().map(|y| y.model_code()).unwrap_or(0);
                                // Why an absent radio is absent, when the server knows:
                                // carried with the presence so the client can say "port
                                // in use by another program" instead of showing nothing.
                                let trouble0 = yaesu.as_ref().map(|y| y.status().port_trouble).unwrap_or(0);
                                let trouble1 = yaesu2.as_ref().map(|y| y.status().port_trouble).unwrap_or(0);
                                let tuple = (present0, model0, trouble0, present1, model1, trouble1);

                                let addrs = session.lock().await.active_addrs();
                                sent_addrs.retain(|a| addrs.contains(a)); // prune disconnected
                                logged_addrs.retain(|a| addrs.contains(a));

                                let changed = prev != Some(tuple);
                                if changed {
                                    // L1: value-change-only, incl. number of recipients.
                                    info!("Yaesu presence push: slot0={}/{}/t{} slot1={}/{}/t{} -> {} clients",
                                        present0, model0, trouble0, present1, model1, trouble1, addrs.len());
                                    prev = Some(tuple);
                                    sent_addrs.clear(); // everyone gets the new tuple
                                }

                                let pkt = YaesuPresencePacket {
                                    slot0_present: present0, slot0_model: model0,
                                    slot1_present: present1, slot1_model: model1,
                                    slot0_trouble: trouble0, slot1_trouble: trouble1,
                                    // Configured is what this server was started with;
                                    // it does not change during a run.
                                    slot0_configured: yaesu.is_some(),
                                    slot1_configured: yaesu2.is_some(),
                                };
                                let mut buf = [0u8; YaesuPresencePacket::SIZE];
                                pkt.serialize(&mut buf);
                                // On change to all active clients; otherwise only
                                // fresh clients that did not yet get the current tuple.
                                // Mark an addr as 'sent' ONLY on a successful send:
                                // on a send error it stays unmarked -> retried next tick
                                // (500 ms), so a fresh client does not permanently miss
                                // the initial push until the next presence change.
                                for addr in &addrs {
                                    if changed || !sent_addrs.contains(addr) {
                                        if socket.try_send_to(&buf, *addr).is_ok() {
                                            if !changed && logged_addrs.insert(*addr) {
                                                info!("Yaesu presence initial push -> {}", addr);
                                            }
                                            sent_addrs.insert(*addr);
                                        }
                                    }
                                }
                            }
                            _ = shutdown.changed() => break,
                        }
                    }
                }))
            }
        };

        // Spawn the safety check task. The main loop watches it: if it ends while
        // the server runs, everything it guards is released (`safety_task_lost`).
        let mut safety_handle = {
            let ptt = self.ptt.clone();
            let session = self.session.clone();
            let socket = self.socket.clone();
            let spectrum = self.spectrum.clone();
            let rx2_spectrum = self.rx2_spectrum.clone();
            let vrx_mgr = self.vrx_mgr.clone();
            let amplitec = self.amplitec.clone();
            let tuners = self.tuners.clone();
            let spe = self.spe.clone();
            let rf2k = self.rf2k.clone();
            let ultrabeam = self.ultrabeam.clone();
            let rotor = self.rotor.clone();
            let cw_ticks = self.cw.clone();
            let cw_due_ticks = self.cw_due.clone();
            let dxcluster = self.dxcluster.clone();
            let drive_level_shared = self.drive_level_shared.clone();
            let active_pa_shared = self.active_pa_shared.clone();
            let vfo_freq_shared = self.vfo_freq_shared.clone();
            let vfo_b_freq_shared = self.vfo_b_freq_shared.clone();
            let mut shutdown = self.shutdown.clone();
            let mut tuner_cat_rx = self.tuner_cat_rx.take();
            let yaesu_inner = yaesu.clone();
            let yaesu2_inner = yaesu2.clone();

            let yaesu_ptt_flag = self.yaesu_ptt_flag.clone();
            let yaesu2_ptt_flag = self.yaesu2_ptt_flag.clone();

            tokio::spawn(async move {
                let _yaesu = yaesu_inner;
                let _yaesu2 = yaesu2_inner;
                let yaesu_ptt_flag = yaesu_ptt_flag;
                let yaesu2_ptt_flag = yaesu2_ptt_flag;
                let mut safety_tick = interval(Duration::from_millis(100));
                let mut cat_tick = interval(Duration::from_secs(1));
                let mut freq_tick = interval(Duration::from_millis(100));
                let mut spectrum_tick = interval(Duration::from_millis(50)); // 20 Hz check rate
                let mut equipment_tick = interval(Duration::from_millis(200));
                // Who may amplify, and what the drive falls back to. Its own tick
                // because it has to run without a client connected: the case it
                // repairs is a server starting up after everything was switched off
                // while a PA was in Operate.
                let mut pa_policy_tick = interval(Duration::from_secs(1));
                let mut pa_prev_spe_operate: Option<bool> = None;
                let mut pa_startup_done = false;
                let mut pa_startup_waiting_logged = false;
                let mut pa_prev_thetis_known = false;
                // Thetis.exe's process id in the run the startup check was made
                // for (build 226).
                let mut pa_thetis_pid: Option<u32> = None;
                let mut pa_last_standby_cmd: Option<std::time::Instant> = None;
                let mut pa_waiting_for_rx_logged = false;
                // What the RF2K-S was last seen doing, and the drive at the moment
                // contact went away - the value the alarm compares against.
                let mut pa_rf2k_last_seen: Option<bool> = crate::config::load().rf2k_last_seen_operate;
                let mut pa_drive_when_lost: Option<u8> = None;
                let mut pa_drive_warning = false;
                let mut pa_drive_alarm_was_on = false;
                let (pa_default_drive, pa_single_operate) = {
                    let c = crate::config::load();
                    (c.pa_default_drive, c.pa_single_operate)
                };
                // Loss-throttling counts per ACTUALLY-ready frame, per processor.
                // A tick counter does not work here: the tick runs at 20 Hz and the
                // frame gate at 15 fps, which makes ready-frames always get the same
                // parity and the half-cadence rule always or never bites.
                let mut rx1_frame_count: u32 = 0;
                let mut rx2_frame_count: u32 = 0;
                // Per-tuner VFO-at-tune-complete tracking - each tuner has its
                // own physical memory, so the "stale" check (VFO moved >25 kHz
                // from the last tune) must be evaluated independently per slot.
                // Index by `TunerInstance::slot_index()`.
                let mut tuner_done_freqs: [u64; 2] = [0, 0];
                let mut last_vfo_freq: u64 = 0; // cached VFO A for tuner stale check
                let mut last_vfo_b_freq: u64 = 0; // cached VFO B for DX cluster band filter
                let mut tci_spots_sent: std::collections::HashSet<(String, u64)> = std::collections::HashSet::new();
                // Logged once per stretch without a client (build 219).
                let mut dx_spots_alone_logged = false;
                // The TCI connection the spots in `tci_spots_sent` went to (build 220).
                let mut dx_spots_tci_conn: u64 = 0;
                // The RX-only stop of Thetis' own TX, logged once per stretch (build 220).
                let mut thetis_tx_off_logged = false;
                // Stock "Receive only" through CAT ZZTI (build 221): the TCI
                // connection it was last said on, and when.
                let mut stock_rx_only_conn: u64 = 0;
                let mut stock_rx_only_said = std::time::Instant::now();
                // This server sent ZZTI1 and has not turned it off since: only then
                // does it send ZZTI0 (build 222, review of 221).
                let mut stock_rx_only_ours = false;
                let _last_sync_freq: u64 = 0; // last freq synced B=A
                let mut prev_controls: std::collections::HashMap<u8, u16> = std::collections::HashMap::new();
                let mut prev_client_count: usize = 0;
                // A rejoin inside the session timeout keeps the client COUNT the
                // same (over the relay the address too), so the count check below
                // cannot see it; the presence loop already watches the connect
                // generation for that reason. Without it the TX filter band and the
                // rest of this family came back only with the 10 s resend (review 83).
                let mut joined_state = crate::session::GenerationWatch::new();
                // The equipment status keeps a tick-list per device and needs the
                // same: since build 83 a client clears its device panels with the
                // session, and a rejoin waited for the 10 s safety net to see them
                // again (review 84).
                let mut joined_equipment = crate::session::GenerationWatch::new();
                let mut last_state_resend = std::time::Instant::now();
                let mut prev_smeter_count: usize = 0;
                let mut prev_freq: u64 = 0;
                let mut prev_mode: u8 = 255;
                let mut prev_vfo_b_freq: u64 = 0;
                let mut prev_vfo_b_mode: u8 = 255;
                let mut prev_tx_profile_names: Vec<String> = Vec::new();
                let mut prev_tx_filter: Option<(i32, i32)> = None;
                // Per device: the last payload sent, and WHO already has it. It used to
                // be the payload plus a client COUNT, which cannot see one client
                // leaving and another arriving between two ticks - the newcomer then
                // waited for the device to change by itself, which for something like
                // a tuner can be a very long time.
                let mut prev_equipment: std::collections::HashMap<
                    u8, (Vec<u8>, std::collections::HashSet<std::net::SocketAddr>)
                > = std::collections::HashMap::new();
                let mut last_equipment_resend = std::time::Instant::now();
                // DX-spot dedup + refresh-tracking. Before this fix the
                // server resent every equipment_tick (200 ms = 5 Hz) all cached
                // spots again to all clients - ~90 Kbit/s in steady-state with
                // a filled cache. New spots now go out immediately; the
                // age-refresh pass runs every 10 s (visible enough for the
                // "5m ago" UI). On a new client prev_spot_keys is
                // cleared so the next tick sends a full resync.
                let mut prev_spot_keys: std::collections::HashSet<(String, u64, String)> =
                    std::collections::HashSet::new();
                let mut last_spot_full_refresh = std::time::Instant::now();
                let mut prev_spot_client_count: usize = 0;
                // Reactive RF-power-cap per Amplitec-A position. One instance
                // for the whole broadcast task; cap-state + snapshot/restore
                // are kept between tick iterations. See `power_cap.rs`
                // for the position->watts table + activation conditions.
                let mut power_cap_state = crate::power_cap::PowerCapState::new();
                // Preventive TX-gate: handle to the tx_blocked flag in
                // PttController. Every broadcast iteration we push the
                // current "is the current Amplitec position RX-only?" status
                // in here so all server-initiated TX paths
                // (PTT packet, ZZTX1, ZZTU1) never send Thetis a TX-on
                // command on a blocked position.
                let tx_blocked_gate = ptt.lock().await.tx_blocked_handle();
                // Written right below, where `RxOnlyGuard` is computed; the
                // heartbeat and the server window read it (build 217).
                let rx_only_unguarded_gate = ptt.lock().await.rx_only_unguarded_handle();
                // Diagnostics: previous gate value for on-change logging.
                let mut prev_pos_is_blocked = false;
                let mut prev_rx_only_unguarded = false;
                // Previous availability of the rx_only_ex cap. When the
                // operator ticks the ThetisLink-extensions checkbox in Thetis
                // while the Amplitec is already on an RX-only position,
                // pos_is_blocked does not change - so on the
                // cap transition (absent->present) we must push RXOnly again.
                let mut prev_had_rx_only_cap = false;
                // Snapshot of Thetis' RXOnly state from just before TL2 took
                // it over for an RX-only position. `Some(prev)` means
                // "TL2 took over, restore `prev` on release".
                // `None` = TL2 did not take over RXOnly. Only TL2 knows
                // this pre-state, so the whole snapshot/restore decision sits
                // here (the Thetis fork stays "dumb").
                let mut rx_only_saved: Option<bool> = None;
                // Log the tx_blocked + max_w config state once at the start
                // of the broadcast task, so we can see directly in the log whether
                // the active config even has the power-cap/RX-only values.
                {
                    let cfg = crate::config::load();
                    info!(
                        "TX-gate config at start: amplitec_tx_blocked={:?} amplitec_max_w={:?}",
                        cfg.amplitec_tx_blocked, cfg.amplitec_max_w
                    );
                }
                // Test switch, deliberately not in the UI: the task panics after
                // this many seconds, to see the main loop catch it in the field.
                //   set THETISLINK_TEST_STOP_SAFETY_TASK=30  before starting the server
                let test_stop_after = std::env::var("THETISLINK_TEST_STOP_SAFETY_TASK")
                    .ok()
                    .and_then(|v| v.trim().parse::<u64>().ok())
                    .map(Duration::from_secs);
                if let Some(d) = test_stop_after {
                    warn!(
                        "THETISLINK_TEST_STOP_SAFETY_TASK is set: the safety task stops itself in {} s - a test of the safety net, not a fault",
                        d.as_secs()
                    );
                }
                let task_started = Instant::now();
                // The spectrum around a change of the transmitter (a
                // measurement, build 172): the freq tick knows `is_tx`, the
                // spectrum tick writes the line.
                let mut probe_is_tx = false;
                let mut tx_probe = crate::spectrum::TxProbe::default();
                loop {
                    tokio::select! {
                        _ = safety_tick.tick() => {
                            if test_stop_after.map_or(false, |d| task_started.elapsed() >= d) {
                                panic!("THETISLINK_TEST_STOP_SAFETY_TASK: the safety task stops here on purpose");
                            }
                            if ptt.lock().await.check_safety().await {
                                if let Some(holder) = session.lock().await.release_after_safety() {
                                    // Named in advance (working rule 11, build 251).
                                    info!("SAFETY: {} held Thetis when its transmitter was released - the ownership goes with it", holder);
                                }
                            }
                            // The 2 s clock of each Yaesu, in the same tick as
                            // Thetis's (build 290, owner 2026-10-23; until then in
                            // each radio's 200 ms status task, and a radio went off
                            // up to 200 ms after the window instead of 100).
                            for (target, radio, flag) in [
                                (crate::session::TxTarget::Yaesu1, &_yaesu, &yaesu_ptt_flag),
                                (crate::session::TxTarget::Yaesu2, &_yaesu2, &yaesu2_ptt_flag),
                            ] {
                                let_a_silent_holder_go(&session, target, radio.as_deref(), flag).await;
                            }
                            // Every 100 ms, not on the 1 s CAT tick it ran on until
                            // build 134: a deadline of 3 s was met anywhere between
                            // 3 and 4 s, and the PTT of a dead link hung that long
                            // (owner's decision, 2026-09-19).
                            let timed_out = session.lock().await.check_timeout();
                            // The dead link releases the PTT above (in the session);
                            // the CW key of that client goes up here, in the same
                            // breath - guarantee 1 on the map.
                            for addr in timed_out {
                                let up = crate::cw_playout::lock(&cw_ticks).release(addr, Instant::now());
                                if let Some(up) = up {
                                    warn!("cw playout: {} timed out with the key down - key up now", addr);
                                    // Through the channel, not straight to the line:
                                    // see NetworkService::cw_due for the order this
                                    // keeps. `release` stamps the new run on this
                                    // edge, so the consumer does not drop it.
                                    let _ = cw_due_ticks.send(up);
                                }
                            }
                            // The key-down watchdog, from here as well as from the
                            // playout thread. It has to be here: the thread is the
                            // only other caller, and if the thread is the thing that
                            // stalled - a serial write that blocks, the line shared
                            // with the poll loop - then a key held down is exactly
                            // what it stalled on. For Thetis that ends anyway (the
                            // packet watchdog drops MOX after 2 s); for a Yaesu
                            // there is no MOX, the RTS line IS the transmitter, and
                            // it would stay high (review 2120A1, both reviewers).
                            // `watchdog` lifts once and then reports nothing, so two
                            // callers cannot double up.
                            let dog = crate::cw_playout::lock(&cw_ticks).watchdog(Instant::now());
                            if let Some(up) = dog {
                                warn!("cw playout: the key was down past the watchdog - key up now (from the safety tick)");
                                let _ = cw_due_ticks.send(up);
                            }
                            // A CW run that has been quiet for a second is over:
                            // hand Thetis back, unless the same client is on the
                            // air with its microphone.
                            let (idle, holds, run_target) = {
                                let g = crate::cw_playout::lock(&cw_ticks);
                                let now = Instant::now();
                                (g.idle_owner(now), g.holds_transmitter(now), g.target())
                            };
                            let run_tx = run_target.and_then(crate::session::TxTarget::from_cw_target);
                            // Tell each Yaesu whether a CW run holds it, so what moves
                            // the radio (the tone read, a memory write) waits.
                            for (target, radio) in [(1u8, &_yaesu), (2u8, &_yaesu2)] {
                                if let Some(r) = radio {
                                    r.status_arc().lock().unwrap_or_else(|e| e.into_inner())
                                        .cw_tick(holds && run_target == Some(target), Instant::now());
                                }
                            }
                            // Thetis's MOX is the server's for a Thetis run only; a Yaesu
                            // keys itself (its own semi break-in), and its run must not
                            // hold Thetis's packet watchdog open.
                            if holds && run_tx == Some(crate::session::TxTarget::Thetis) {
                                // A held key and the hang time send no edges; keep the
                                // packet watchdog fed while the run holds the transmitter,
                                // so the run's own clocks end it: the idle release after
                                // the hang time (up to 3 s, longer than this watchdog's
                                // 2 s: build 105 dropped MOX a second early) and the CW
                                // watchdog for a key held down. The heartbeat stays the
                                // client's: a dead client is dropped at 2 s regardless.
                                ptt.lock().await.record_cw_activity();
                            }
                            if let Some(owner) = idle {
                                let tx = run_tx.unwrap_or(crate::session::TxTarget::Thetis);
                                if tx == crate::session::TxTarget::Thetis {
                                    // The run is over: MOX off, the transmitter handed back.
                                    ptt.lock().await.release().await;
                                }
                                session.lock().await.release_tx(tx, owner);
                                crate::cw_playout::lock(&cw_ticks).forget_owner();
                                info!("cw playout: run of {} over, PTT released and {:?} handed back", owner, tx);
                            }
                        }
                        _ = spectrum_tick.tick() => {
                            // All subscriber lists in ONE session-lock, before any
                            // spectrum-lock whatsoever (deadlock order: the main loop
                            // does session->spectrum).
                            let (client_info, rx2_client_info, vrx1_spec_addrs, vrx2_spec_addrs,
                                 full_row_targets1, full_row_targets2) = {
                                let sess = session.lock().await;
                                let rx1: Vec<(SocketAddr, f32, f32, u16, u8)> = sess.spectrum_clients()
                                    .into_iter()
                                    .map(|(addr, zoom, pan, max_bins)| (addr, zoom, pan, max_bins, sess.client_loss(addr)))
                                    .collect();
                                let rx2: Vec<(SocketAddr, f32, f32, u16, u8)> = sess.rx2_spectrum_clients()
                                    .into_iter()
                                    .map(|(addr, zoom, pan, max_bins)| (addr, zoom, pan, max_bins, sess.client_loss(addr)))
                                    .collect();
                                let v1: Vec<(SocketAddr, u8)> = sess.vrx_spectrum_addrs(0)
                                    .into_iter().map(|a| (a, sess.client_loss(a))).collect();
                                let v2: Vec<(SocketAddr, u8)> = sess.vrx_spectrum_addrs(1)
                                    .into_iter().map(|a| (a, sess.client_loss(a))).collect();
                                let f1 = sess.full_row_clients(0);
                                let f2 = sess.full_row_clients(1);
                                (rx1, rx2, v1, v2, f1, f2)
                            };
                            // Per-client (PATCH-vrx-per-client): each VRX spectrum subscriber
                            // gets a window on ITS own listening frequency + span.
                            // Snapshot the jobs under the manager lock; the extraction then
                            // happens without holding that lock.
                            let jobs1: Vec<(SocketAddr, u64, u32, u8)> = if vrx1_spec_addrs.is_empty() {
                                Vec::new()
                            } else {
                                let m = vrx_mgr.lock().unwrap();
                                vrx1_spec_addrs.iter().filter_map(|(a, loss)| {
                                    let span = m.spectrum_span(a, 0);
                                    // Cut where the operator is looking, not
                                    // always on the listening frequency.
                                    let freq = m.target_freq(a, 0)
                                        .saturating_add_signed(m.spectrum_pan(a, 0) as i64);
                                    (span > 0 && freq > 0).then_some((*a, freq, span as u32 * 1000, *loss))
                                }).collect()
                            };
                            let jobs2: Vec<(SocketAddr, u64, u32, u8)> = if vrx2_spec_addrs.is_empty() {
                                Vec::new()
                            } else {
                                let m = vrx_mgr.lock().unwrap();
                                vrx2_spec_addrs.iter().filter_map(|(a, loss)| {
                                    let span = m.spectrum_span(a, 1);
                                    let freq = m.target_freq(a, 1)
                                        .saturating_add_signed(m.spectrum_pan(a, 1) as i64);
                                    (span > 0 && freq > 0).then_some((*a, freq, span as u32 * 1000, *loss))
                                }).collect()
                            };

                            // Each stream has its OWN guard. Previously the whole
                            // tick bailed out as soon as there were no RX1 spectrum subscribers,
                            // which made RX2 and VRX spectrum stall while they had their own
                            // subscribers - the RX1 spectrum was thereby the de facto master switch.
                            if client_info.is_empty() && rx2_client_info.is_empty()
                                && jobs1.is_empty() && jobs2.is_empty()
                            {
                                continue;
                            }
                            let mut packets_to_send: Vec<(SocketAddr, Vec<u8>)> = Vec::new();

                            // RX1 processor: feeds both the RX1 spectrum and VRX1 high-res.
                            // One frame gate for both, so VRX does not run faster than
                            // the configured fps.
                            if !client_info.is_empty() || !jobs1.is_empty() {
                                let mut sp = spectrum.lock().await;
                                if sp.is_frame_ready() {
                                    rx1_frame_count = rx1_frame_count.wrapping_add(1);
                                    if tx_probe.wants(probe_is_tx) {
                                        if let Some(line) = tx_probe.frame(probe_is_tx, sp.levels(), Instant::now()) {
                                            dev_info!("{}", line);
                                        }
                                    }
                                    for (addr, zoom, pan, max_bins, loss) in &client_info {
                                        if *loss > 15 { continue; }
                                        if *loss > 5 && rx1_frame_count % 2 != 0 { continue; }
                                        let pkt = sp.extract_view(*zoom, *pan, *max_bins as usize);
                                        let mut buf = Vec::with_capacity(*max_bins as usize + 20);
                                        pkt.serialize(&mut buf);
                                        packets_to_send.push((*addr, buf));
                                    }
                                    // The full-DDC row has its own audience: the RX1
                                    // subscribers plus everyone watching VRX1, which
                                    // rides this same DDC. One row per client, never
                                    // one per window - the client lays the same bytes
                                    // under every window that wants a full-band
                                    // backdrop.
                                    for (addr, max_bins, loss) in &full_row_targets1 {
                                        if *loss > 15 { continue; }
                                        if *loss > 5 && rx1_frame_count % 2 != 0 { continue; }
                                        let full_bins = (*max_bins as usize).min(FULL_SPECTRUM_BINS);
                                        let full_pkt = sp.get_full_row(full_bins);
                                        if full_pkt.num_bins > 0 {
                                            let mut full_buf = Vec::with_capacity(full_bins + 20);
                                            full_pkt.serialize_as_type(&mut full_buf, PacketType::FullSpectrum);
                                            packets_to_send.push((*addr, full_buf));
                                        }
                                    }
                                    // VRX1 high-res: only clients that turned VrxSpectrumEnable on
                                    // get this packet type (old v2.1.x clients never).
                                    for (a, freq, span_hz, loss) in &jobs1 {
                                        // Same loss gate as RX: with 4096 bins VRX high-res is
                                        // precisely the largest stream, so it must be throttled
                                        // on a bad link just as much.
                                        if *loss > 15 { continue; }
                                        if *loss > 5 && rx1_frame_count % 2 != 0 { continue; }
                                        let pkt = sp.extract_view_at(*freq, *span_hz, 4096);
                                        if pkt.num_bins > 0 {
                                            let mut buf = Vec::with_capacity(pkt.num_bins as usize * 2 + 32);
                                            pkt.serialize_as_type(&mut buf, PacketType::SpectrumVrx1);
                                            packets_to_send.push((*a, buf));
                                        }
                                    }
                                }
                            } // spectrum lock released

                            // RX2 processor: likewise for the RX2 spectrum and VRX2 high-res.
                            if !rx2_client_info.is_empty() || !jobs2.is_empty() {
                                let mut rx2_sp = rx2_spectrum.lock().await;
                                if rx2_sp.is_frame_ready() {
                                    rx2_frame_count = rx2_frame_count.wrapping_add(1);
                                    for (addr, zoom, pan, max_bins, loss) in &rx2_client_info {
                                        if *loss > 15 { continue; }
                                        if *loss > 5 && rx2_frame_count % 2 != 0 { continue; }
                                        let pkt = rx2_sp.extract_view(*zoom, *pan, *max_bins as usize);
                                        let mut buf = Vec::with_capacity(*max_bins as usize + 20);
                                        pkt.serialize_as_type(&mut buf, PacketType::SpectrumRx2);
                                        packets_to_send.push((*addr, buf));
                                    }
                                    // Same audience rule for the RX2 chain, where VRX2 lives.
                                    for (addr, max_bins, loss) in &full_row_targets2 {
                                        if *loss > 15 { continue; }
                                        if *loss > 5 && rx2_frame_count % 2 != 0 { continue; }
                                        let full_bins = (*max_bins as usize).min(FULL_SPECTRUM_BINS);
                                        let full_pkt = rx2_sp.get_full_row(full_bins);
                                        if full_pkt.num_bins > 0 {
                                            let mut full_buf = Vec::with_capacity(full_bins + 20);
                                            full_pkt.serialize_as_type(&mut full_buf, PacketType::FullSpectrumRx2);
                                            packets_to_send.push((*addr, full_buf));
                                        }
                                    }
                                    for (a, freq, span_hz, loss) in &jobs2 {
                                        if *loss > 15 { continue; }
                                        if *loss > 5 && rx2_frame_count % 2 != 0 { continue; }
                                        let pkt = rx2_sp.extract_view_at(*freq, *span_hz, 4096);
                                        if pkt.num_bins > 0 {
                                            let mut buf = Vec::with_capacity(pkt.num_bins as usize * 2 + 32);
                                            pkt.serialize_as_type(&mut buf, PacketType::SpectrumVrx2);
                                            packets_to_send.push((*a, buf));
                                        }
                                    }
                                }
                            } // rx2 spectrum lock released

                            // Send all packets without holding any locks (non-blocking to avoid stalling select! loop)
                            for (addr, buf) in &packets_to_send {
                                let _ = socket.try_send_to(buf, *addr);
                            }
                        }
                        _ = equipment_tick.tick() => {
                            // Get client addresses ONCE for all equipment broadcasts
                            let (eq_addrs, eq_gen) = {
                                let s = session.lock().await;
                                (s.active_addrs(), s.connect_generation())
                            };
                            // The protection per antenna position runs with or without a
                            // client (review of 217): whoever keys at the radio PC - the
                            // footswitch, MOX in Thetis - often has no client open, and
                            // the skip below used to stop all of it.
                            let active_amplitec_pos = amplitec
                                .as_ref()
                                .map(|a| a.status().switch_a)
                                .filter(|p| (1..=6).contains(p));
                            // Generic RF-power cap per Amplitec-A position -
                            // reactive drive reduction when the active
                            // antenna position has a max_w in
                            // `config.amplitec_max_w` and the PA meter is
                            // above it. Also reactive TX-block for
                            // positions marked in `config.amplitec_tx_blocked`
                            // (RX-only antennas).
                            //
                            // Cap operation: we send the PA's OWN drive knob
                            // (same command as the "-" button in the client's
                            // SPE/RF2K tab). Not Thetis ZZPC: the PA
                            // determines the drive via a TCI loop back to
                            // Thetis, so a ZZPC reduction from the server
                            // gets pushed back straight to the PA-determined
                            // value. Only active on the PA set in
                            // `config.active_pa`.
                            //
                            // TX-block: if Thetis goes into TX on an
                            // RX-only position we send `ZZTX0;` directly
                            // (translates to `trx:0,false;`). Reactive -
                            // preventive client-side gating comes later.
                            {
                                let live_config = crate::config::load();
                                let active_pa = live_config.active_pa;
                                let (pa_in_operate, pa_fwd_watts) = match active_pa {
                                    1 => spe
                                        .as_ref()
                                        .map(|s| {
                                            let st = s.status();
                                            (st.state == 2, Some(st.forward_power))
                                        })
                                        .unwrap_or((false, None)),
                                    2 => rf2k
                                        .as_ref()
                                        .map(|r| {
                                            let st = r.status();
                                            (st.operate, Some(st.forward_w))
                                        })
                                        .unwrap_or((false, None)),
                                    _ => (false, None),
                                };
                                // By name (build 226, review of 224).
                                let crate::ptt::TxNow { mode: mode_u8, ours: tx_active, thetis: thetis_tx } = ptt.lock().await.tx_now();
                                // Preventive TX-gate: push the current
                                // "is the current Amplitec position RX-only?"
                                // status into the Ptt gate so that no
                                // server-initiated PTT path can send a TX
                                // command to Thetis.
                                let pos_is_blocked = active_amplitec_pos
                                    .map(|p| {
                                        live_config
                                            .amplitec_tx_blocked
                                            .get((p - 1) as usize)
                                            .copied()
                                            .unwrap_or(false)
                                    })
                                    .unwrap_or(false);
                                tx_blocked_gate.store(
                                    pos_is_blocked,
                                    std::sync::atomic::Ordering::Relaxed,
                                );
                                // On a change of the RX-only status:
                                // push Thetis' "Receive only" preventive
                                // TX-inhibit via the fork command `rx_only_ex`.
                                // With fork-extensions Thetis then blocks ALL
                                // TX sources (MOX/spacebar/hardware-PTT/VOX)
                                // at the source - no more TX window. Against
                                // stock Thetis (no cap) set_rx_only returns
                                // false and the reactive ZZTX0 below remains
                                // the only (best-effort) protection.
                                // Snapshot/restore of Thetis' RXOnly. TL2 is
                                // the only one that knows the pre-takeover state, so
                                // the whole decision sits here; the fork simply
                                // sets whatever we send.
                                //
                                // - Takeover (position becomes RX-only): save the
                                //   current Thetis-RXOnly as snapshot (only
                                //   on the first takeover), set RXOnly=true.
                                // - Release (position no longer RX-only):
                                //   restore the snapshot (NOT blindly false) -
                                //   this keeps a manually set RXOnly
                                //   respected.
                                // - Cap appears (extensions off->on while
                                //   already on RX-only position): treat as
                                //   takeover.
                                // - Cap disappears (extensions off) while TL2
                                //   had taken over: we can no longer
                                //   restore (fork ignores rx_only_ex). RXOnly
                                //   stays on for safety; warn the
                                //   operator instead of removing the protection.
                                let has_rx_only_cap =
                                    ptt.lock().await.has_cap("rx_only_ex");
                                let crate::ptt::TciLinkNow { up: tci_up_now, answers_run_cat_ex, connections: tci_conn_now } =
                                    ptt.lock().await.tci_link_now();
                                let unguarded_now = crate::ptt::RxOnlyGuard {
                                    position_rx_only: pos_is_blocked,
                                    tci_connected: tci_up_now,
                                    has_rx_only_ex: has_rx_only_cap,
                                    answers_run_cat_ex,
                                }
                                .unguarded();
                                rx_only_unguarded_gate.store(unguarded_now, Ordering::Relaxed);
                                if unguarded_now != prev_rx_only_unguarded {
                                    prev_rx_only_unguarded = unguarded_now;
                                    if unguarded_now {
                                        // Named in advance (`logregels-build215.md`).
                                        log::warn!(
                                            "TX-gate: RX-only position without rx_only_ex in Thetis - only a TCI PTT can be stopped; clients are warned"
                                        );
                                    }
                                }
                                let cap_just_appeared =
                                    has_rx_only_cap && !prev_had_rx_only_cap;
                                let cap_just_disappeared =
                                    !has_rx_only_cap && prev_had_rx_only_cap;
                                prev_had_rx_only_cap = has_rx_only_cap;

                                let pos_changed = pos_is_blocked != prev_pos_is_blocked;
                                prev_pos_is_blocked = pos_is_blocked;

                                // Stock Thetis (v2.10.3.14+): the same "Receive only"
                                // through CAT ZZTI over run_cat_ex, where the fork has
                                // rx_only_ex (build 221, review of 220). An older Thetis
                                // ignores it and stays "unguarded" with the warning.
                                // Off only to undo our own on (build 222).
                                if !has_rx_only_cap && tci_up_now {
                                    let new_conn = tci_conn_now != stock_rx_only_conn;
                                    stock_rx_only_conn = tci_conn_now;
                                    let input = crate::ptt::StockRxOnlyInput {
                                        new_conn,
                                        pos_changed,
                                        pos_is_blocked,
                                        we_set_it: stock_rx_only_ours,
                                    };
                                    if let Some(step) = input.step(stock_rx_only_said.elapsed()) {
                                        stock_rx_only_said = std::time::Instant::now();
                                        ptt.lock().await.set_rx_only_cat(step).await;
                                        stock_rx_only_ours = step.ours();
                                        // Named in advance (`logregels-build221.md`).
                                        match step {
                                            crate::ptt::StockRxOnly::TakeOver => info!(
                                                "TX-gate: RX-only takeover pos={:?} through CAT ZZTI1 (stock Thetis)",
                                                active_amplitec_pos
                                            ),
                                            crate::ptt::StockRxOnly::Release => info!(
                                                "TX-gate: RX-only release pos={:?} through CAT ZZTI0 (stock Thetis)",
                                                active_amplitec_pos
                                            ),
                                            crate::ptt::StockRxOnly::Reassert => {}
                                        }
                                    }
                                }

                                if has_rx_only_cap && (pos_changed || cap_just_appeared) {
                                    let mut p = ptt.lock().await;
                                    if pos_is_blocked {
                                        if rx_only_saved.is_none() {
                                            // At bootstrap (cap_just_appeared, first detect
                                            // since server-start) `thetis_rx_only` is an
                                            // unreliable pre-state: it may be stale residue
                                            // from a previous TL session that itself turned
                                            // RXOnly on and did not clean up (process crash,
                                            // PC reboot, etc.). Default at bootstrap is therefore
                                            // `false` - on release RXOnly turns off cleanly
                                            // and Thetis does not stay stuck. Outside
                                            // bootstrap (mid-session cap-toggle) we do
                                            // snapshot the current state so a manual
                                            // RXOnly stays respected.
                                            rx_only_saved = Some(if cap_just_appeared {
                                                false
                                            } else {
                                                p.thetis_rx_only()
                                            });
                                        }
                                        p.set_rx_only(true).await;
                                        info!(
                                            "TX-gate: RX-only takeover pos={:?} (saved pre-state={:?})",
                                            active_amplitec_pos, rx_only_saved,
                                        );
                                    } else if let Some(prev) = rx_only_saved.take() {
                                        p.set_rx_only(prev).await;
                                        info!(
                                            "TX-gate: RX-only release pos={:?} -> restored RXOnly={}",
                                            active_amplitec_pos, prev,
                                        );
                                    } else if cap_just_appeared {
                                        // Bootstrap edge: cap became available and TL2 did
                                        // not take over (rx_only_saved is None) - pos is
                                        // not RX-only. Always send `rx_only_ex:false`,
                                        // regardless of `p.thetis_rx_only()`: at bootstrap
                                        // the echo arrives only seconds AFTER the cap-detect, so
                                        // `thetis_rx_only` here may still be its default
                                        // `false` while Thetis in reality has a stale
                                        // RXOnly from a previous session. An
                                        // unconditional reset is idempotent if Thetis
                                        // was already off, and clears the residue without waiting
                                        // for the echo timing. Any deliberately-manually-
                                        // set RXOnly is overruled; acceptable per
                                        // the earlier trade-off - the operator can tick it
                                        // again.
                                        p.set_rx_only(false).await;
                                        info!(
                                            "TX-gate: cleared possible stale RXOnly at bootstrap (pos={:?})",
                                            active_amplitec_pos,
                                        );
                                    }
                                } else if has_rx_only_cap && pos_is_blocked {
                                    // Level-maintain during active takeover. Edge-triggered
                                    // only (pos_changed / cap_just_appeared) would leave
                                    // the preventive TX-inhibit if the operator in
                                    // Thetis manually unchecks 'Receive only'
                                    // (setup.cs:6493) or a second TCI client sends a
                                    // `rx_only_ex:false;` - the protection would then
                                    // only come back on the next position change,
                                    // until then falling back on the reactive ZZTX0
                                    // catch-all (~100 ms TX window). Re-assert only
                                    // when the TCI echo indicates RXOnly was actually
                                    // cleared externally - avoids 5 Hz unnecessary
                                    // TCI traffic and repeated UI invokes in the Thetis
                                    // fork on a stable takeover.
                                    let mut p = ptt.lock().await;
                                    if !p.thetis_rx_only() {
                                        p.set_rx_only(true).await;
                                        log::warn!(
                                            "TX-gate: RXOnly externally cleared on blocked pos {:?} - re-asserting preventive TX-inhibit",
                                            active_amplitec_pos,
                                        );
                                    }
                                }

                                // Extensions turned off while TL2 had taken
                                // over RXOnly: protection stays (safe),
                                // but TL2 can no longer restore -> warn.
                                if cap_just_disappeared {
                                    if let Some(prev) = rx_only_saved.take() {
                                        log::warn!(
                                            // Named in advance (`logregels-build221.md`): since 221 the
                                            // stock path keeps it through CAT ZZTI and releases it.
                                            "ThetisLink extensions turned off while RX-only was active (pre-state was {}). \
                                             Receive only is kept through CAT ZZTI from now on and turned off when the position is left.",
                                            prev,
                                        );
                                    }
                                }
                                // Reactive catch-all for a ThetisLink TX that
                                // was already on when the position became
                                // RX-only (`tx_active` is our own PTT state).
                                // New server-initiated TX is closed off by the
                                // preventive gate above; a spacebar or
                                // hardware PTT at Thetis is stopped in the TRX
                                // arm of tci.rs, through `tx_blocked`.
                                // A TX Thetis was already making itself is stopped
                                // here too since build 220 (review of 219): the
                                // TRX arm only answers a new "on".
                                let rx_only_off = crate::ptt::RxOnlyTxInput {
                                    pos_is_blocked,
                                    our_tx: tx_active,
                                    thetis_tx,
                                }
                                .tx_off();
                                match rx_only_off {
                                    Some(crate::ptt::RxOnlyTxOff::Ours) => {
                                        log::warn!(
                                            "Amplitec pos={:?} is RX-only and Thetis is in TX; forcing TX off",
                                            active_amplitec_pos
                                        );
                                        ptt.lock().await.send_cat("ZZTX0").await;
                                    }
                                    Some(crate::ptt::RxOnlyTxOff::Thetis) => {
                                        let mut p = ptt.lock().await;
                                        // Once per stretch: a MOX by the button stays
                                        // on in stock Thetis, and this runs every tick.
                                        if !thetis_tx_off_logged {
                                            thetis_tx_off_logged = true;
                                            if p.tune_is_ours() {
                                                // Named in advance (`logregels-build221.md`).
                                                log::warn!(
                                                    "Amplitec pos={:?} is RX-only and ThetisLink's own TUNE is on (receiver {}); forcing it off",
                                                    active_amplitec_pos,
                                                    p.thetis_tx_receiver()
                                                );
                                            } else {
                                                // Named in advance (`logregels-build220.md`).
                                                log::warn!(
                                                    "Amplitec pos={:?} is RX-only and Thetis was already transmitting on receiver {}; forcing TRX off",
                                                    active_amplitec_pos,
                                                    p.thetis_tx_receiver()
                                                );
                                            }
                                        }
                                        p.force_thetis_tx_off().await;
                                    }
                                    None => {}
                                }
                                if rx_only_off != Some(crate::ptt::RxOnlyTxOff::Thetis) {
                                    thetis_tx_off_logged = false;
                                }
                                // Switch detector - when switching away from a
                                // position with an active cap cycle we send
                                // as many DriveUp commands as we had sent
                                // DriveDowns, so the
                                // PA drive is back at the pre-cap position.
                                if let Some(restore) =
                                    crate::power_cap::on_position_change(
                                        &mut power_cap_state,
                                        active_amplitec_pos,
                                    )
                                {
                                    if let Some(ref s) = spe {
                                        for _ in 0..restore.spe_drive_up {
                                            s.send_command(
                                                crate::spe_expert::SpeCmd::DriveUp,
                                            );
                                        }
                                    }
                                    if let Some(ref r) = rf2k {
                                        for _ in 0..restore.rf2k_drive_up {
                                            r.send_command(
                                                crate::rf2k::Rf2kCmd::DriveUp,
                                            );
                                        }
                                    }
                                }
                                // Reactive cap-check
                                if let Some(action) = crate::power_cap::tick(
                                    &mut power_cap_state,
                                    crate::power_cap::PowerCapReading {
                                        active_pos: active_amplitec_pos,
                                        max_w_table: &live_config.amplitec_max_w,
                                        active_pa,
                                        pa_in_operate,
                                        pa_fwd_watts,
                                        mode: mode_u8,
                                    },
                                ) {
                                    match action {
                                        crate::power_cap::PowerCapAction::SpeDriveDown => {
                                            if let Some(ref s) = spe {
                                                s.send_command(
                                                    crate::spe_expert::SpeCmd::DriveDown,
                                                );
                                            }
                                        }
                                        crate::power_cap::PowerCapAction::Rf2kDriveDown => {
                                            if let Some(ref r) = rf2k {
                                                r.send_command(
                                                    crate::rf2k::Rf2kCmd::DriveDown,
                                                );
                                            }
                                        }
                                    }
                                }
                            }

                            // Forward NEW DX spots to Thetis via TCI SPOT (only once per spot).
                            // Independent of TL2-client subscriptions, and of any
                            // client at all - the fork's own DX-spot display must
                            // always be fed as long as the DX cluster is on. Before
                            // the no-client skip below since build 219 (review of 218).
                            let dx_spots = dxcluster
                                .as_ref()
                                .map(|cluster| cluster.spots_for_bands(last_vfo_freq, last_vfo_b_freq));
                            // A spot counts as sent only while TCI is up, and a new
                            // TCI connection (a restarted Thetis) gets the current
                            // spots again (build 220, review of 219).
                            let (tci_up, tci_conn) = {
                                let p = ptt.lock().await;
                                (p.tci_connected(), p.tci_connection_count())
                            };
                            if tci_conn != dx_spots_tci_conn {
                                dx_spots_tci_conn = tci_conn;
                                if !tci_spots_sent.is_empty() {
                                    // Named in advance (`logregels-build220.md`).
                                    info!(
                                        "DX spots: new TCI connection - the current spots go to Thetis again ({} went to the previous one)",
                                        tci_spots_sent.len()
                                    );
                                }
                                tci_spots_sent.clear();
                            }
                            if let Some(spots) = dx_spots.as_ref().filter(|_| tci_up) {
                                if !spots.is_empty() {
                                    let mut new_spots: Vec<&crate::dxcluster::DxSpot> = Vec::new();
                                    for spot in spots {
                                        let key = (spot.callsign.clone(), spot.frequency_hz);
                                        if tci_spots_sent.insert(key) {
                                            new_spots.push(spot);
                                        }
                                    }
                                    if !new_spots.is_empty() {
                                        if eq_addrs.is_empty() && !dx_spots_alone_logged {
                                            dx_spots_alone_logged = true;
                                            // Named in advance (`logregels-build219.md`).
                                            info!(
                                                "DX spots: forwarding to Thetis with no client connected ({} new)",
                                                new_spots.len()
                                            );
                                        }
                                        let mut ptt_guard = ptt.lock().await;
                                        for spot in &new_spots {
                                            let color = crate::dxcluster::mode_color_argb(&spot.mode);
                                            let text = if spot.comment.is_empty() {
                                                spot.spotter.clone()
                                            } else {
                                                format!("{} {}", spot.spotter, spot.comment)
                                            };
                                            ptt_guard.send_tci_spot(&spot.callsign, &spot.mode, spot.frequency_hz, color, &text).await;
                                        }
                                    }
                                    // Clean expired spots from tracking set
                                    let active_keys: std::collections::HashSet<(String, u64)> = spots.iter()
                                        .map(|s| (s.callsign.clone(), s.frequency_hz))
                                        .collect();
                                    tci_spots_sent.retain(|k| active_keys.contains(k));
                                }
                            }
                            if !eq_addrs.is_empty() {
                                dx_spots_alone_logged = false;
                            }

                            // Tuner: track tune frequency, show stale if VFO moved >25kHz.
                            // Multi-tuner: pick the tuner bound to the active Amplitec-A
                            // position; fall back to the primary (first enabled) tuner so
                            // the panel still works without an Amplitec mapping configured.
                            // Before the no-client skip since build 220 (review of
                            // 219): the server window shows "Tune OK" from `stale` too,
                            // and the tuned frequency is taken when the tune ends.
                            let active_tuner = active_amplitec_pos
                                .and_then(|p| tuners.for_amplitec_pos(p))
                                .or_else(|| tuners.primary());
                            let tuner_broadcast = if let Some(tuner_ref) = active_tuner.as_ref() {
                                let ts = tuner_ref.status();
                                let current_freq = last_vfo_freq;
                                let tuner_done = ts.state == crate::tuner::TUNER_DONE_OK;
                                // Per-tuner stale-check: the slot's last-tune VFO is
                                // remembered across Amplitec switches, so a second tuner
                                // does not see the first tuner's tune freq.
                                let done_slot = tuner_ref.slot_index().min(tuner_done_freqs.len() - 1);
                                let tuner_done_freq = if tuner_done {
                                    if tuner_done_freqs[done_slot] == 0 {
                                        tuner_done_freqs[done_slot] = current_freq;
                                    }
                                    tuner_done_freqs[done_slot]
                                } else {
                                    tuner_done_freqs[done_slot] = 0;
                                    0
                                };
                                let is_stale = tuner_done
                                    && tuner_done_freq > 0 && current_freq > 0
                                    && (current_freq as i64 - tuner_done_freq as i64).unsigned_abs() > 25_000;
                                let broadcast_state = if is_stale { crate::tuner::TUNER_IDLE } else { ts.state };
                                tuner_ref.set_stale(is_stale);
                                let ts_for_broadcast = crate::tuner::TunerStatus {
                                    state: broadcast_state,
                                    ..ts
                                };
                                Some(ts_for_broadcast)
                            } else {
                                None
                            };

                            if eq_addrs.is_empty() { continue; }
                            // Safety net: forget who has what every so often, so one
                            // lost datagram cannot leave a client stale indefinitely.
                            // The payloads are kept - only the tick-lists are cleared.
                            // And at once when someone joined: a rejoin on the same
                            // address is still ticked off here.
                            if joined_equipment.changed(eq_gen) {
                                for (_, sent) in prev_equipment.values_mut() { sent.clear(); }
                            }
                            if last_equipment_resend.elapsed().as_secs() >= STATE_RESEND_SECS {
                                last_equipment_resend = std::time::Instant::now();
                                for (_, sent) in prev_equipment.values_mut() { sent.clear(); }
                            }

                            // Helper: serialize, compare with prev, send only if changed
                            // Changed -> everyone; otherwise only whoever is not ticked
                            // off yet. A client counts as served only on a successful
                            // send: ticking it off on the intent would make a failed
                            // send permanent.
                            macro_rules! send_if_changed {
                                ($device_id:expr, $pkt:expr, $addrs:expr) => {{
                                    let mut buf = Vec::with_capacity(128);
                                    $pkt.serialize(&mut buf);
                                    let key = $device_id as u8;
                                    let entry = prev_equipment.entry(key).or_insert_with(
                                        || (Vec::new(), std::collections::HashSet::new()));
                                    let changed = entry.0 != buf;
                                    if changed {
                                        entry.0 = buf.clone();
                                        entry.1.clear();
                                    }
                                    entry.1.retain(|a| $addrs.contains(a));
                                    for addr in $addrs {
                                        if changed || !entry.1.contains(addr) {
                                            if socket.try_send_to(&buf, *addr).is_ok() {
                                                entry.1.insert(*addr);
                                            }
                                        }
                                    }
                                }};
                            }

                            if let Some(ref amp) = amplitec {
                                let status = amp.status();
                                // Live label snapshot: the `config` local is a snapshot
                                // at `start_server` time; on a rename via the Amplitec
                                // context menu the global config is updated but this
                                // local is not. So read from the mutex-protected cache
                                // every tick so clients get the new names via the
                                // send_if_changed dedup.
                                let live_config = crate::config::load();
                                let labels = Some(crate::config::labels_string(&live_config));
                                let pkt = EquipmentStatusPacket {
                                    device_type: DeviceType::Amplitec6x2,
                                    switch_a: status.switch_a,
                                    switch_b: status.switch_b,
                                    connected: status.connected,
                                    labels,
                                };
                                send_if_changed!(DeviceType::Amplitec6x2, pkt, &eq_addrs);
                            }
                            // Amplitec power-cap table - re-broadcast on change (or new
                            // client, via prev_equipment-clear).
                            // Reuses `send_if_changed!` with a unique key
                            // outside the DeviceType range (0xFE).
                            {
                                let live = crate::config::load();
                                let pkt = sdr_remote_core::protocol::AmplitecPowerTablePacket {
                                    max_w: std::array::from_fn(|i| {
                                        live.amplitec_max_w[i].unwrap_or(0)
                                    }),
                                    tx_blocked: live.amplitec_tx_blocked,
                                };
                                let mut buf = vec![0u8; sdr_remote_core::protocol::AmplitecPowerTablePacket::SIZE];
                                let arr: &mut [u8; sdr_remote_core::protocol::AmplitecPowerTablePacket::SIZE] =
                                    (&mut buf[..]).try_into().unwrap();
                                pkt.serialize(arr);
                                let key: u8 = 0xFE; // sentinel outside DeviceType range
                                // Hand-rolled copy of send_if_changed (the packet is
                                // serialised into a fixed array, so it cannot use the
                                // macro) - same tick-list rule.
                                let entry = prev_equipment.entry(key).or_insert_with(
                                    || (Vec::new(), std::collections::HashSet::new()));
                                let changed = entry.0 != buf;
                                if changed {
                                    entry.0 = buf.clone();
                                    entry.1.clear();
                                }
                                entry.1.retain(|a| eq_addrs.contains(a));
                                for addr in &eq_addrs {
                                    if changed || !entry.1.contains(addr) {
                                        if socket.try_send_to(&buf, *addr).is_ok() {
                                            entry.1.insert(*addr);
                                        }
                                    }
                                }
                            }
                            // SPE Expert status broadcast
                            if let Some(ref spe_ref) = spe {
                                let ss = spe_ref.status();
                                let mut lbl = crate::spe_expert::status_labels_string(&ss);
                                let ap = active_pa_shared.as_ref().map(|a| a.load(Ordering::Relaxed)).unwrap_or(0);
                                lbl.push_str(if ap == 1 { ",1" } else { ",0" });
                                let labels = Some(lbl);
                                let pkt = EquipmentStatusPacket {
                                    device_type: DeviceType::SpeExpert,
                                    switch_a: ss.state,
                                    switch_b: ss.band,
                                    connected: ss.connected,
                                    labels,
                                };
                                send_if_changed!(DeviceType::SpeExpert, pkt, &eq_addrs);
                            }
                            // RF2K-S status broadcast
                            if let Some(ref rf2k_ref) = rf2k {
                                let rs = rf2k_ref.status();
                                let mut lbl = crate::rf2k::status_labels_string(&rs);
                                let ap = active_pa_shared.as_ref().map(|a| a.load(Ordering::Relaxed)).unwrap_or(0);
                                lbl.push_str(if ap == 2 { ",1" } else { ",0" });
                                lbl.push_str(&crate::rf2k::debug_labels_string(&rs));
                                let labels = Some(lbl);
                                let pkt = EquipmentStatusPacket {
                                    device_type: DeviceType::Rf2k,
                                    switch_a: rs.operate as u8,
                                    switch_b: rs.band,
                                    connected: rs.connected,
                                    labels,
                                };
                                send_if_changed!(DeviceType::Rf2k, pkt, &eq_addrs);
                            }
                            // The tuner packet to the clients (its state computed above).
                            if let Some(ts_for_broadcast) = tuner_broadcast {
                                // can_tune: a tuner is available for the active antenna iff
                                // a tuner-config slot is bound to that position. Without an
                                // Amplitec mapping at all, the primary tuner is always usable.
                                let can_tune = match active_amplitec_pos {
                                    Some(p) => tuners.for_amplitec_pos(p).is_some(),
                                    None => true,
                                };
                                let pkt = EquipmentStatusPacket {
                                    device_type: DeviceType::Tuner,
                                    switch_a: ts_for_broadcast.state,
                                    switch_b: can_tune as u8,
                                    connected: ts_for_broadcast.connected,
                                    labels: None,
                                };
                                send_if_changed!(DeviceType::Tuner, pkt, &eq_addrs);
                            }

                            // UltraBeam status broadcast
                            if let Some(ref ub_ref) = ultrabeam {
                                let us = ub_ref.status();
                                let labels = Some(crate::ultrabeam::status_labels_string(&us));
                                let pkt = EquipmentStatusPacket {
                                    device_type: DeviceType::UltraBeam,
                                    switch_a: us.direction,
                                    switch_b: us.band,
                                    connected: us.connected,
                                    labels,
                                };
                                send_if_changed!(DeviceType::UltraBeam, pkt, &eq_addrs);
                            }
                            // Rotor status broadcast
                            if let Some(ref rotor_ref) = rotor {
                                let rs = rotor_ref.status();
                                let labels = Some(crate::rotor::status_labels_string(&rs));
                                let pkt = EquipmentStatusPacket {
                                    device_type: DeviceType::Rotor,
                                    switch_a: rs.rotating as u8,
                                    switch_b: 0,
                                    connected: rs.connected,
                                    labels,
                                };
                                send_if_changed!(DeviceType::Rotor, pkt, &eq_addrs);
                            }
                            // DX Cluster spot broadcast - dedup + 10 s refresh.
                            // Per tick: send only spots whose
                            // (callsign, freq, mode) key has not been seen before.
                            // Every 10 s and on client-count change:
                            // full resync so age fields are updated
                            // and new clients get the cache.
                            // Spot-addrs filtered on `dx_spots_enabled` -
                            // clients can opt out of the stream via the
                            // DxSpotsEnabled control for metered links.
                            if let Some(ref cluster) = dxcluster {
                                let spots = dx_spots.unwrap_or_default();
                                let spot_addrs = session.lock().await.dx_spots_addrs();
                                if !spot_addrs.is_empty() {
                                    let expiry = cluster.expiry_secs() as u16;
                                    let now = std::time::Instant::now();
                                    let do_full_refresh = last_spot_full_refresh.elapsed()
                                        >= std::time::Duration::from_secs(10)
                                        || spot_addrs.len() != prev_spot_client_count;
                                    if do_full_refresh {
                                        prev_spot_keys.clear();
                                        last_spot_full_refresh = now;
                                        prev_spot_client_count = spot_addrs.len();
                                    }
                                    let current_keys: std::collections::HashSet<(String, u64, String)> =
                                        spots.iter()
                                            .map(|s| (s.callsign.clone(), s.frequency_hz, s.mode.clone()))
                                            .collect();
                                    // Garbage-collect: remove keys that have
                                    // disappeared from the cache (expired)
                                    // so prev_spot_keys does not grow unbounded.
                                    prev_spot_keys.retain(|k| current_keys.contains(k));
                                    for spot in &spots {
                                        let key = (spot.callsign.clone(), spot.frequency_hz, spot.mode.clone());
                                        if !prev_spot_keys.insert(key) {
                                            continue; // already sent in an earlier tick
                                        }
                                        let age = now.duration_since(spot.time).as_secs().min(expiry as u64) as u16;
                                        let pkt = SpotPacket {
                                            callsign: spot.callsign.clone(),
                                            frequency_hz: spot.frequency_hz,
                                            mode: spot.mode.clone(),
                                            spotter: spot.spotter.clone(),
                                            comment: spot.comment.clone(),
                                            age_seconds: age,
                                            expiry_seconds: expiry,
                                        };
                                        let mut buf = Vec::with_capacity(128);
                                        pkt.serialize(&mut buf);
                                        for addr in &spot_addrs {
                                            let _ = socket.try_send_to(&buf, *addr);
                                        }
                                    }
                                } else {
                                    // No subscribing clients: reset dedup state
                                    // so a re-subscribing client immediately
                                    // gets a full sync.
                                    prev_spot_keys.clear();
                                    prev_spot_client_count = 0;
                                }
                            }
                        }
                        // Tuner CAT commands (ZZTU1/ZZTU0) forwarded to Thetis
                        Some(cmd) = async {
                            match tuner_cat_rx.as_mut() {
                                Some(rx) => rx.recv().await,
                                None => std::future::pending::<Option<String>>().await,
                            }
                        } => {
                            // Translate ZZTU CAT to TCI TUNE command
                            let tci_cmd = if cmd.contains("ZZTU1") {
                                "TUNE:0,true;"
                            } else if cmd.contains("ZZTU0") {
                                "TUNE:0,false;"
                            } else {
                                // Unknown tune command, pass through
                                &cmd
                            };
                            debug!("Tuner TCI: {} -> {}", cmd.trim_end_matches(';'), tci_cmd.trim_end_matches(';'));
                            ptt.lock().await.send_cat(tci_cmd).await;
                        }
                        _ = pa_policy_tick.tick() => {
                            use crate::pa_policy::{self, Enforce, PA_RF2K, PA_SPE};

                            let spe_st = spe.as_ref().map(|s| s.status());
                            let rf_st = rf2k.as_ref().map(|r| r.status());
                            // A reading is only a reading when the PA answers.
                            let spe_seen = spe_st.as_ref().map(|s| s.connected).unwrap_or(false);
                            let rf_seen = rf_st.as_ref().map(|r| r.connected).unwrap_or(false);
                            let spe_operate = spe_st.as_ref().map(|s| s.connected && s.state == 2).unwrap_or(false);
                            let rf_operate = rf_st.as_ref().map(|r| r.connected && r.operate).unwrap_or(false);
                            let active_pa = pa_policy::ActiveMark {
                                marked: active_pa_shared.as_ref().map(|a| a.load(Ordering::Relaxed)).unwrap_or(0),
                                spe_present: spe.is_some(),
                                rf2k_present: rf2k.is_some(),
                            }
                            .effective();
                            // "Transmitting" from every side that can know it; see
                            // `TxSources` for why each one is there. Standby with RF
                            // going through a PA is the switching this project exists to
                            // avoid, so any one of them is enough to wait.
                            // By name, in and out (build 226, review of 224).
                            let pa_policy::ThetisReading { tx_active: is_tx, drive: drive_now, known: thetis_known } = {
                                let g = ptt.lock().await;
                                let (thetis_trx, thetis_tune) = g.tci.thetis_trx_and_tune();
                                let tx = pa_policy::TxSources {
                                    client: g.is_transmitting(),
                                    thetis_trx,
                                    thetis_tune,
                                    spe_ptt: spe_st.as_ref().map(|s| s.connected && s.ptt).unwrap_or(false),
                                    rf2k_ptt: rf_st.as_ref().map(|r| r.connected && r.ptt).unwrap_or(false),
                                    yaesu_ptt: yaesu_ptt_flag.load(Ordering::Relaxed),
                                    yaesu2_ptt: yaesu2_ptt_flag.load(Ordering::Relaxed),
                                    // Gated on connected like the PAs' PTT above: a radio
                                    // that drops out while keyed would otherwise hold the
                                    // guard for good (review finding on build 65).
                                    yaesu_tx: _yaesu.as_ref().map(|y| y.status().reported_tx()).unwrap_or(false),
                                    yaesu2_tx: _yaesu2.as_ref().map(|y| y.status().reported_tx()).unwrap_or(false),
                                };
                                pa_policy::ThetisReading { tx_active: tx.any(), drive: g.drive_level(), known: g.power_on() }
                            };

                            // One startup attempt per Thetis run, not per server run.
                            // A run is a process since build 226: TCI back to the same
                            // Thetis.exe leaves the drive as it is.
                            if (pa_policy::ThetisKnown { before: pa_prev_thetis_known, now: thetis_known }).became_known() {
                                let pids_now = crate::ptt::thetis_process_ids();
                                let back_input = pa_policy::ThetisBackInput { run_pid: pa_thetis_pid, pids_now: &pids_now };
                                let back = back_input.back();
                                if pa_startup_done {
                                    // Named in advance (`logregels-build226.md`).
                                    match back {
                                        pa_policy::ThetisBack::NewProcess { pid, was } => info!(
                                            "PA drive: Thetis is back as a new process (Thetis.exe {}, was {}) - the startup check runs again",
                                            pid,
                                            was.map_or_else(|| "none".to_string(), |w| w.to_string())
                                        ),
                                        pa_policy::ThetisBack::SameProcess { pid } => info!(
                                            "PA drive: TCI is back to the same Thetis (Thetis.exe {}) - the drive is left as it is",
                                            pid
                                        ),
                                        pa_policy::ThetisBack::NotSeen => info!(
                                            "PA drive: Thetis is back and its process is not seen on this PC - the startup check runs again"
                                        ),
                                        // Named in advance (`logregels-build227.md`).
                                        pa_policy::ThetisBack::Several { count } => info!(
                                            "PA drive: Thetis is back and {} Thetis.exe processes run on this PC - the startup check runs again",
                                            count
                                        ),
                                    }
                                    if back.new_run() {
                                        pa_startup_done = false;
                                        pa_startup_waiting_logged = false;
                                    }
                                }
                                pa_thetis_pid = back_input.run_pid_now();
                            }
                            pa_prev_thetis_known = thetis_known;

                            // What each PA is doing, with its own meaning of "no contact".
                            let spe_amp = if spe.is_some() {
                                pa_policy::spe_amplifying(spe_seen, spe_st.as_ref().map(|s| s.state).unwrap_or(0))
                            } else {
                                pa_policy::Amplifying::No
                            };
                            let rf_amp = if rf2k.is_some() {
                                pa_policy::Rf2kReading {
                                    reachable: rf_seen,
                                    reports_operate: rf_operate,
                                    last_seen_operate: pa_rf2k_last_seen,
                                }
                                .amplifying()
                            } else {
                                pa_policy::Amplifying::No
                            };
                            let amplifying = pa_policy::worst(spe_amp, rf_amp);

                            // Remember what we saw of the RF2K-S, on disk: after a
                            // restart it is the only way to tell "switched off" from
                            // "amplifying out of reach".
                            if rf_seen && pa_rf2k_last_seen != Some(rf_operate) {
                                pa_rf2k_last_seen = Some(rf_operate);
                                crate::config::modify_config(|c| c.rf2k_last_seen_operate = Some(rf_operate));
                            }

                            // Two answers, not one. The beep is about the drive *now*
                            // and stops when the operator puts it back; the red line is
                            // about what happened and stays until the PA is back in
                            // reach (owner, after hearing it in the field).
                            let out_of_reach = matches!(rf_amp, pa_policy::Amplifying::Unknown);
                            pa_drive_when_lost = pa_policy::AlarmBaseline {
                                prev: pa_drive_when_lost,
                                out_of_reach,
                                drive_known: thetis_known,
                                drive_now,
                            }
                            .next();
                            let mut pa_drive_alarm_now = false;
                            if let Some(base) = pa_drive_when_lost {
                                pa_drive_alarm_now = pa_policy::DriveAlarm { state: rf_amp, drive_when_lost: base, drive_now }.raised();
                                if pa_drive_alarm_now && !pa_drive_warning {
                                    warn!("PA drive warning: drive went from {}% to {}% while the RF2K-S is out of reach - it may be amplifying and cannot be told",
                                        base, drive_now);
                                } else if !pa_drive_alarm_now && pa_drive_warning && !pa_drive_alarm_was_on {
                                    // nothing: the line stays, the beep already stopped
                                } else if !pa_drive_alarm_now && pa_drive_alarm_was_on {
                                    info!("PA drive back to {}% - the beep stops, the warning stays until the RF2K-S is reachable", drive_now);
                                }
                            }
                            let warning_now = pa_policy::DriveWarning {
                                was_up: pa_drive_warning,
                                alarm_now: pa_drive_alarm_now,
                                reachable: !out_of_reach,
                            }
                            .stays();
                            if pa_drive_warning && !warning_now {
                                info!("PA drive warning cleared - the RF2K-S can be reached again");
                            }
                            pa_drive_warning = warning_now;
                            pa_drive_alarm_was_on = pa_drive_alarm_now;
                            // The SPE has no observer of its own; the RF2K-S keeps
                            // its snapshot in its own thread. Same rule, one tick.
                            let spe_steady_operate =
                                pa_policy::steady_in_operate(pa_prev_spe_operate, spe_operate);
                            if spe_seen {
                                if let Some(v) = pa_policy::snapshot_on_operate(
                                    pa_prev_spe_operate, spe_operate, drive_now)
                                {
                                    crate::config::save_saved_drive(PA_SPE, Some(v));
                                    info!("SPE drive: snapshot before Operate = {}%", v);
                                }
                                pa_prev_spe_operate = Some(spe_operate);
                            }

                            // Once per run: nobody is amplifying, so the drive is the
                            // operator's again.
                            if !pa_startup_done && (spe_seen || rf_seen) {
                                let cfg = crate::config::load();
                                let saved = match active_pa {
                                    PA_SPE => cfg.spe_saved_drive,
                                    PA_RF2K => cfg.rf2k_saved_drive,
                                    _ => cfg.rf2k_saved_drive.or(cfg.spe_saved_drive),
                                };
                                match pa_policy::startup_restore(
                                    amplifying, thetis_known, saved, pa_default_drive, drive_now)
                                {
                                    Some(target) => {
                                        info!("PA drive: no PA in Operate at startup - Thetis drive {}% -> {}% (snapshot={:?}, default={}%)",
                                            drive_now, target, saved, pa_default_drive);
                                        ptt.lock().await.set_drive(target).await;
                                    }
                                    None => {
                                        // Only once Thetis has said something: before that
                                        // the drive reads 0 % because nothing told us, and
                                        // saying "leaving it at 0%" reads like a measurement
                                        // (seen twice in the field log of build 53).
                                        if thetis_known {
                                            match amplifying {
                                                pa_policy::Amplifying::Yes => info!(
                                                    "PA drive: a PA is in Operate at startup - leaving the drive at {}%", drive_now),
                                                pa_policy::Amplifying::Unknown => {
                                                    if !pa_startup_waiting_logged {
                                                        info!("PA drive: the RF2K-S cannot be reached and was last seen in Operate - leaving the drive at {}% until it answers", drive_now);
                                                        pa_startup_waiting_logged = true;
                                                    }
                                                }
                                                pa_policy::Amplifying::No => {}
                                            }
                                        }
                                    }
                                }
                                // Spent on a reading, never on a gap: Thetis has to have
                                // spoken, and an RF2K-S out of reach has said nothing yet.
                                if pa_policy::startup_attempt_used(amplifying, thetis_known) {
                                    pa_startup_done = true;
                                }
                            }

                            // Only the active PA may amplify. Two in Operate both write
                            // the Thetis drive, with different values, and it keeps
                            // flipping between them.
                            if pa_single_operate {
                                // By name (build 224, review of 223).
                                let rows = [
                                    (
                                        pa_policy::EnforceInput {
                                            active_pa,
                                            pa_id: PA_SPE,
                                            connected: spe_seen,
                                            in_operate: spe_operate,
                                            tx_active: is_tx,
                                        },
                                        "SPE Expert",
                                    ),
                                    (
                                        pa_policy::EnforceInput {
                                            active_pa,
                                            pa_id: PA_RF2K,
                                            connected: rf_seen,
                                            in_operate: rf_operate,
                                            tx_active: is_tx,
                                        },
                                        "RF2K-S",
                                    ),
                                ];
                                let mut waiting = false;
                                for (input, name) in rows {
                                    let pa_id = input.pa_id;
                                    match input.decide() {
                                        Enforce::Standby => {
                                            let ready = pa_last_standby_cmd
                                                .map_or(true, |t| t.elapsed() >= Duration::from_secs(5))
                                                // The SPE's Operate command is a toggle and it is
                                                // polled once a second: on a single reading a
                                                // front-panel press in that gap would make this
                                                // switch the PA ON. Two readings that agree.
                                                && (pa_id != PA_SPE || spe_steady_operate);
                                            if pa_id == PA_SPE && !spe_steady_operate {
                                                // The one tick the toggle gate holds; said out
                                                // loud so the gate can be seen in a field log.
                                                info!("SPE Expert is in Operate while PA {} is the active one - waiting for a second reading before the toggle", active_pa);
                                            }
                                            if ready {
                                                warn!("{} is in Operate while PA {} is the active one - sending Standby (two PAs in Operate fight over the Thetis drive)",
                                                    name, active_pa);
                                                if pa_id == PA_SPE {
                                                    if let Some(ref s) = spe {
                                                        s.send_command(crate::spe_expert::SpeCmd::ToggleOperate);
                                                    }
                                                } else if let Some(ref r) = rf2k {
                                                    r.send_command(crate::rf2k::Rf2kCmd::SetOperate(false));
                                                }
                                                pa_last_standby_cmd = Some(std::time::Instant::now());
                                            }
                                        }
                                        Enforce::WaitForRx => {
                                            waiting = true;
                                            if !pa_waiting_for_rx_logged {
                                                info!("{} has to go to Standby, waiting for RX", name);
                                                pa_waiting_for_rx_logged = true;
                                            }
                                        }
                                        Enforce::Nothing => {}
                                    }
                                }
                                if !waiting {
                                    pa_waiting_for_rx_logged = false;
                                }
                            }
                        }
                        _ = cat_tick.tick() => {
                            // Two-phase connect: check needs (brief lock), connect (no lock), store (brief lock)
                            let ptt_clone = ptt.clone();
                            tokio::spawn(async move {
                                // Phase 1: check if TCI connection is needed (brief, no I/O)
                                let tci_url = {
                                    let mut guard = ptt_clone.lock().await;
                                    guard.needed_connections()
                                };
                                // Phase 2: attempt TCI WebSocket connect WITHOUT holding the ptt lock
                                let mut tci_stream = None;
                                if let Some(url) = tci_url {
                                    debug!("TCI: connecting to {}...", url);
                                    match tokio::time::timeout(
                                        Duration::from_millis(500),
                                        tokio_tungstenite::connect_async(&url),
                                    ).await {
                                        Ok(Ok((stream, _))) => { tci_stream = Some(stream); }
                                        Ok(Err(e)) => { log::debug!("TCI connect failed: {}", e); }
                                        Err(_) => { log::debug!("TCI connect timed out"); }
                                    }
                                }
                                // Phase 3: store established connection (brief lock, no I/O)
                                if tci_stream.is_some() {
                                    ptt_clone.lock().await.accept_connections(tci_stream);
                                }
                            });
                        }
                        _ = freq_tick.tick() => {
                            let ptt_guard = ptt.lock().await;
                            let freq = ptt_guard.vfo_a_freq();
                            let mode = ptt_guard.vfo_a_mode();
                            let is_tx = ptt_guard.is_transmitting();
                            probe_is_tx = is_tx;
                            // S-meter packet `level` is signed deci-units on the wire:
                            //  - RX (PTT flag clear): dBm x 10  (e.g. -730 = -73 dBm = S9)
                            //  - TX (PTT flag set):   watts x 10 (e.g. 1000 = 100.0 W FWD)
                            let smeter: i16 = if is_tx {
                                ptt_guard.fwd_power_raw() as i16
                            } else {
                                dbm_to_deci(ptt_guard.smeter_avg())
                            };
                            // Per-source S-meter values for client-side
                            // subscription (Sig / Avg / MaxBin). During TX the
                            // S-meter widget switches to FWD-power so the
                            // alternate sources are unused; we keep them at 0
                            // to avoid the cost of three more upstream sensor lookups.
                            let (smeter_sig, smeter_peakbin): (i16, i16) = if is_tx {
                                (0, 0)
                            } else {
                                (dbm_to_deci(ptt_guard.smeter_sig()), dbm_to_deci(ptt_guard.smeter_peakbin()))
                            };
                            let swr_x100 = if is_tx { ptt_guard.swr_x100() } else { 100 };
                            let power_on = ptt_guard.power_on();
                            let tx_profile = ptt_guard.tx_profile();
                            let nr_level = ptt_guard.nr_level();
                            let anf_on = ptt_guard.anf_on();
                            let drive_level = ptt_guard.drive_level();
                            if let Some(ref shared) = drive_level_shared {
                                shared.store(drive_level, Ordering::Relaxed);
                            }
                            let rx_af_gain = ptt_guard.rx_af_gain();
                            let filter_low = ptt_guard.filter_low_hz();
                            let filter_high = ptt_guard.filter_high_hz();
                            let tx_filter_low = ptt_guard.tx_filter_low_hz();
                            let tx_filter_high = ptt_guard.tx_filter_high_hz();
                            let tx_filter_known = ptt_guard.tx_filter_known();
                            let thetis_starting = ptt_guard.thetis_starting();
                            let ctun = ptt_guard.ctun();
                            // RX2 state
                            let vfo_b_freq = ptt_guard.vfo_b_freq();
                            let vfo_b_mode = ptt_guard.vfo_b_mode();
                            let smeter_rx2: i16 = dbm_to_deci(ptt_guard.smeter_rx2_avg());
                            let smeter_rx2_sig: i16 = dbm_to_deci(ptt_guard.smeter_rx2_sig());
                            let smeter_rx2_peakbin: i16 = dbm_to_deci(ptt_guard.smeter_rx2_peakbin());
                            let rx2_af_gain = ptt_guard.rx2_af_gain();
                            let rx2_filter_low = ptt_guard.filter_rx2_low_hz();
                            let rx2_filter_high = ptt_guard.filter_rx2_high_hz();
                            let rx2_nr_level = ptt_guard.rx2_nr_level();
                            let rx2_anf = ptt_guard.rx2_anf_on();
                            let rx2_agc_mode = ptt_guard.rx2_agc_mode();
                            let rx2_agc_gain = ptt_guard.rx2_agc_gain();
                            let rx2_sql_enable = ptt_guard.rx2_sql_enable();
                            let rx2_sql_level = ptt_guard.rx2_sql_level();
                            let rx2_nb_enable = ptt_guard.rx2_nb_enable();
                            let rx2_binaural = ptt_guard.rx2_binaural();
                            let rx2_apf_enable = ptt_guard.rx2_apf_enable();
                            let rx2_vfo_lock = ptt_guard.rx2_vfo_lock();
                            let mon_on = ptt_guard.mon_on();
                            let agc_mode = ptt_guard.agc_mode();
                            let agc_gain = ptt_guard.agc_gain();
                            let rit_enable = ptt_guard.rit_enable();
                            let rit_offset = ptt_guard.rit_offset();
                            let xit_enable = ptt_guard.xit_enable();
                            let xit_offset = ptt_guard.xit_offset();
                            let sql_enable = ptt_guard.sql_enable();
                            let sql_level = ptt_guard.sql_level();
                            let nb_level = ptt_guard.nb_level();
                            let agc_auto_rx1 = ptt_guard.agc_auto(0);
                            let agc_auto_rx2 = ptt_guard.agc_auto(1);
                            let diversity_enabled = ptt_guard.diversity_enabled();
                            let div_phase = ptt_guard.diversity_phase();
                            let div_gain_rx1 = ptt_guard.diversity_gain(0);
                            let div_gain_rx2 = ptt_guard.diversity_gain(1);
                            let div_gain_multi = ptt_guard.diversity_gain_multi();
                            let diversity_autonull_done = ptt_guard.diversity_autonull_done();
                            let vfo_sync_on = ptt_guard.vfo_sync_on();
                            let cw_keyer_speed = ptt_guard.cw_keyer_speed();
                            let vfo_lock = ptt_guard.vfo_lock();
                            let binaural = ptt_guard.binaural();
                            let apf_enable = ptt_guard.apf_enable();
                            let mute = ptt_guard.mute();
                            let rx_mute = ptt_guard.rx_mute();
                            let nf_enable = ptt_guard.nf_enable();
                            let rx2_nf_enable = ptt_guard.rx2_nf_enable();
                            let rx_balance = ptt_guard.rx_balance();
                            let tune_drive = ptt_guard.tune_drive();
                            let mon_volume = ptt_guard.mon_volume();
                            let tx_profile_names: Vec<String> = ptt_guard.tx_profile_names().to_vec();
                            let tx_profile_name = ptt_guard.tx_profile_name().to_string();

                            // TCI: read DDS center frequencies + calibration data
                            let dds_rx1 = ptt_guard.dds_freq(0);
                            let dds_rx2 = ptt_guard.dds_freq(1);
                            let tci_smeter_dbm_rx1 = ptt_guard.smeter_raw_dbm(0);
                            let tci_smeter_dbm_rx2 = ptt_guard.smeter_raw_dbm(1);
                            let static_cal_rx1 = ptt_guard.static_cal_offset(0);
                            let static_cal_rx2 = ptt_guard.static_cal_offset(1);
                            let step_att_rx1 = ptt_guard.step_att(0);
                            let step_att_rx2 = ptt_guard.step_att(1);
                            // Stock v2.10.3.14: rx_step_att_enabled_ex per receiver.
                            // Replaces TL-26 fork's `step_attenuator_ex` capability gate.
                            // Sign convention: stock .14 sends |attenuation| (positive), so we
                            // ADD step_att to the calibration offset (no negation).
                            let step_att_enabled_rx1 = ptt_guard.step_att_enabled(0);
                            let step_att_enabled_rx2 = ptt_guard.step_att_enabled(1);
                            let ddc_rate_rx1 = ptt_guard.ddc_sample_rate(0);
                            let ddc_rate_rx2 = ptt_guard.ddc_sample_rate(1);
                            drop(ptt_guard);

                            // Cache VFO freq for tuner stale check + DX cluster band filter
                            if freq != 0 {
                                last_vfo_freq = freq;
                                if let Some(ref vfs) = vfo_freq_shared {
                                    vfs.store(freq, Ordering::Relaxed);
                                }
                            }
                            if vfo_b_freq != 0 {
                                last_vfo_b_freq = vfo_b_freq;
                                if let Some(ref vfs) = vfo_b_freq_shared {
                                    vfs.store(vfo_b_freq, Ordering::Relaxed);
                                }
                            }

                            // VFO Sync: Thetis handles A<->B sync natively via ZZSY.
                            // No server-side sync needed - just relay Thetis frequency updates.

                            // Spectrum calibration
                            {
                                let mut sp = spectrum.lock().await;
                                if freq != 0 { sp.set_vfo_freq(freq, ctun); }
                                if dds_rx1 != 0 { sp.set_ddc_center(dds_rx1); }

                                if !is_tx {
                                    if step_att_enabled_rx1 {
                                        // Direct: static_cal + ATT (stock .14 sends positive dB)
                                        sp.set_cal_offset_db(static_cal_rx1 + (step_att_rx1 as f32));
                                    } else if let Some(tci_dbm) = tci_smeter_dbm_rx1 {
                                        // Fallback: auto-calibrate from S-meter vs raw spectrum
                                        let raw_dbm = sp.compute_raw_passband_power_dbm(filter_low, filter_high);
                                        if raw_dbm > -130.0 && tci_dbm > -130.0 {
                                            let dynamic = tci_dbm - raw_dbm - static_cal_rx1;
                                            let cur_dynamic = sp.cal_offset_db() - static_cal_rx1;
                                            let new_dynamic = cur_dynamic * 0.9 + dynamic * 0.1;
                                            sp.set_cal_offset_db(static_cal_rx1 + new_dynamic);
                                        } else {
                                            let cur = sp.cal_offset_db();
                                            if cur.abs() < 0.01 {
                                                sp.set_cal_offset_db(static_cal_rx1);
                                            }
                                        }
                                    } else if sp.cal_offset_db().abs() < 0.01 {
                                        sp.set_cal_offset_db(static_cal_rx1);
                                    }
                                }
                            }

                            // RX2 spectrum calibration
                            if !is_tx {
                                let mut rx2_sp = rx2_spectrum.lock().await;
                                if step_att_enabled_rx2 {
                                    // Direct: static_cal + ATT (stock .14 sends positive dB)
                                    rx2_sp.set_cal_offset_db(static_cal_rx2 + (step_att_rx2 as f32));
                                } else if let Some(tci_dbm) = tci_smeter_dbm_rx2 {
                                    let raw_dbm = rx2_sp.compute_raw_passband_power_dbm(rx2_filter_low, rx2_filter_high);
                                    if raw_dbm > -130.0 && tci_dbm > -130.0 {
                                        let dynamic = tci_dbm - raw_dbm - static_cal_rx2;
                                        let cur_dynamic = rx2_sp.cal_offset_db() - static_cal_rx2;
                                        let new_dynamic = cur_dynamic * 0.9 + dynamic * 0.1;
                                        rx2_sp.set_cal_offset_db(static_cal_rx2 + new_dynamic);
                                    } else if rx2_sp.cal_offset_db().abs() < 0.01 {
                                        rx2_sp.set_cal_offset_db(static_cal_rx2);
                                    }
                                }
                            }

                            // S-meter: TCI values are already calibrated
                            let (smeter, smeter_rx2) = (smeter, smeter_rx2);

                            // smeter_addrs: clients that should receive S-meter (not Yaesu-only)
                            // all_addrs: all clients (receive freq, mode, controls, equipment)
                            let (smeter_addrs, rx2_addrs, rx2_spectrum_addrs, all_addrs, conn_gen) = {
                                let sess = session.lock().await;
                                (sess.smeter_addrs(), sess.rx2_addrs(), sess.rx2_spectrum_addrs(), sess.active_addrs(), sess.connect_generation())
                            };
                            // RX2 freq/mode/smeter/controls are DISPLAY data that both the
                            // RX2-audio and the RX2-spectrum subscribers need. Otherwise a
                            // spectrum-without-audio client gets no RX2 freq and
                            // renders the spectrum wrongly centred (phase 3b/4 bug).
                            let rx2_display_addrs: Vec<std::net::SocketAddr> = {
                                let mut v = rx2_addrs.clone();
                                for a in &rx2_spectrum_addrs {
                                    if !v.contains(a) { v.push(*a); }
                                }
                                v
                            };
                            if all_addrs.is_empty() {
                                prev_client_count = 0;
                                continue;
                            }
                            // Safety net (see STATE_RESEND_SECS): this family keeps a
                            // previous VALUE per item rather than a tick-list per client,
                            // so "send it again" means forgetting the previous value.
                            // Without it a lost push here is never repaired, and the
                            // count check below cannot see one client leaving while
                            // another arrives between two ticks.
                            let periodic_resend =
                                last_state_resend.elapsed().as_secs() >= STATE_RESEND_SECS;
                            if periodic_resend { last_state_resend = std::time::Instant::now(); }
                            // New client or Yaesu mode change: force full state resend
                            let rejoined = joined_state.changed(conn_gen);
                            if periodic_resend
                                || rejoined
                                || all_addrs.len() != prev_client_count
                                || smeter_addrs.len() != prev_smeter_count {
                                prev_freq = 0;
                                prev_mode = 255;
                                prev_vfo_b_freq = 0;
                                prev_vfo_b_mode = 255;
                                prev_controls.clear();
                                prev_tx_profile_names.clear();
                                // The TX filter band was the one push in this family
                                // that this block did not forget, so a client that
                                // arrived later - or lost its session and came back -
                                // heard it only when the band next changed. Since
                                // build 82 the client forgets "the server has a TX
                                // filter" with the session, so without this the
                                // control stayed away after every reconnect.
                                prev_tx_filter = None;
                                prev_client_count = all_addrs.len();
                                prev_smeter_count = smeter_addrs.len();
                            }

                            // Broadcast freq/mode to ALL clients (small, push-based, always needed)
                            if freq != 0 && freq != prev_freq {
                                prev_freq = freq;
                                let pkt = FrequencyPacket { frequency_hz: freq };
                                let mut buf = [0u8; FrequencyPacket::SIZE];
                                pkt.serialize(&mut buf);
                                for addr in &all_addrs {
                                    let _ = socket.try_send_to(&buf, *addr);
                                }
                            }

                            if mode != prev_mode {
                                prev_mode = mode;
                                let pkt = ModePacket { mode };
                                let mut buf = [0u8; ModePacket::SIZE];
                                pkt.serialize(&mut buf);
                                for addr in &all_addrs {
                                    let _ = socket.try_send_to(&buf, *addr);
                                }
                            }

                            // S-meter to Thetis clients (Yaesu-only clients don't need it).
                            // Per-client: emit one packet per subscribed source from the
                            // SmeterSources bitmap. The default mask (0x22) emits
                            // RX1 Avg + RX2 Avg, matching pre-multi-source behaviour.
                            // During TX we keep emitting the legacy Smeter (Avg slot)
                            // packet with FWD-power so the existing TX-meter rendering
                            // path on the client keeps working unchanged; the Sig and
                            // MaxBin packets are zero-valued and effectively unused.
                            if !smeter_addrs.is_empty() {
                                let flags = if is_tx || yaesu_ptt_flag.load(Ordering::Relaxed) { Flags::PTT } else { Flags::NONE };
                                // Pre-serialise the three RX1 packet bodies once per
                                // tick - they're identical across clients.
                                let mut buf_avg = [0u8; SmeterPacket::SIZE];
                                SmeterPacket { level: smeter, flags }.serialize(&mut buf_avg);
                                let mut buf_sig = [0u8; SmeterPacket::SIZE];
                                SmeterPacket { level: smeter_sig, flags }.serialize_as_type(&mut buf_sig, PacketType::SmeterSig);
                                let mut buf_pkb = [0u8; SmeterPacket::SIZE];
                                SmeterPacket { level: smeter_peakbin, flags }.serialize_as_type(&mut buf_pkb, PacketType::SmeterMaxBin);

                                let sess = session.lock().await;
                                // The same three packets with HELD_BY_OTHER set, for
                                // every client that is not holding Thetis.
                                //
                                // "Somebody else is transmitting" is news for the other
                                // operators; the one doing it already knows. It used to
                                // be worked out per client from the PTT bit, which does
                                // not say who - right except for one packet after
                                // release, and in that packet the operator was told his
                                // own carrier belonged to a stranger.
                                //
                                // Taken from the ownership table rather than from is_tx,
                                // so it turns over when the lock changes hands and not
                                // when the transmitter falls silent.
                                let thetis_holder = sess.tx_holder(crate::session::TxTarget::Thetis);
                                let held_bufs = thetis_holder.map(|_| {
                                    let f = Flags(flags.0 | Flags::HELD_BY_OTHER.0);
                                    let mut a = [0u8; SmeterPacket::SIZE];
                                    SmeterPacket { level: smeter, flags: f }.serialize(&mut a);
                                    let mut g = [0u8; SmeterPacket::SIZE];
                                    SmeterPacket { level: smeter_sig, flags: f }
                                        .serialize_as_type(&mut g, PacketType::SmeterSig);
                                    let mut k = [0u8; SmeterPacket::SIZE];
                                    SmeterPacket { level: smeter_peakbin, flags: f }
                                        .serialize_as_type(&mut k, PacketType::SmeterMaxBin);
                                    (a, g, k)
                                });
                                for addr in &smeter_addrs {
                                    let mask = sess.smeter_sources(*addr);
                                    // The holder gets the plain copies; everyone else
                                    // gets the ones that say the transmitter is taken.
                                    let (buf_avg, buf_sig, buf_pkb) = match &held_bufs {
                                        Some((a, g, k)) if thetis_holder != Some(*addr) => (*a, *g, *k),
                                        _ => (buf_avg, buf_sig, buf_pkb),
                                    };
                                    if is_tx {
                                        // During TX the `smeter` field carries FWD-power
                                        // (from `fwd_power_raw()` above), not an S-meter
                                        // value. The Sig/MaxBin packets are zero-valued
                                        // by design - emitting them would overwrite the
                                        // client's TX-meter to 0. Force every TX-active
                                        // client onto the legacy `Smeter` (Avg-slot)
                                        // packet, regardless of subscription mask, so
                                        // the PTT path keeps working for Sig/MaxBin
                                        // subscribers too. Post-TX the regular per-source
                                        // emission resumes.
                                        let _ = socket.try_send_to(&buf_avg, *addr);
                                    } else {
                                        if mask & 0x01 != 0 { let _ = socket.try_send_to(&buf_sig, *addr); }
                                        if mask & 0x02 != 0 { let _ = socket.try_send_to(&buf_avg, *addr); }
                                        if mask & 0x04 != 0 { let _ = socket.try_send_to(&buf_pkb, *addr); }
                                    }
                                }
                                drop(sess);
                            }

                            // Broadcast SWR during TX
                            if is_tx && swr_x100 > 100 && !smeter_addrs.is_empty() {
                                let pkt = ControlPacket { control_id: ControlId::ThetisSwr, value: swr_x100 };
                                let mut buf = [0u8; ControlPacket::SIZE];
                                pkt.serialize(&mut buf);
                                for addr in &smeter_addrs {
                                    let _ = socket.try_send_to(&buf, *addr);
                                }
                            }

                            // The DDC centres the server's own VRX uses (build 231),
                            // only once measured (build 232: a starting value went
                            // out as one); RX2's is the last tick's.
                            let ddc_centre_rx1 = {
                                let sp = spectrum.lock().await;
                                if sp.ddc_center_measured() { sp.ddc_center_hz() } else { 0 }
                            };
                            let ddc_centre_rx2 = {
                                let sp = rx2_spectrum.lock().await;
                                if sp.ddc_center_measured() { sp.ddc_center_hz() } else { 0 }
                            };

                            // Broadcast control states (to ALL clients)
                            let controls: &[(ControlId, u16)] = &[
                                (ControlId::PowerOnOff, power_on as u16),
                                (ControlId::TxProfile, tx_profile as u16),
                                (ControlId::NoiseReduction, nr_level as u16),
                                (ControlId::AutoNotchFilter, anf_on as u16),
                                (ControlId::DriveLevel, drive_level as u16),
                                (ControlId::Rx1AfGain, rx_af_gain as u16),
                                (ControlId::Rx2AfGain, rx2_af_gain as u16),
                                (ControlId::FilterLow, filter_low as i16 as u16),
                                (ControlId::FilterHigh, filter_high as i16 as u16),
                                (ControlId::ThetisStarting, thetis_starting as u16),
                                // Bit 0: show the red line. Bit 1: beep. The line stays
                                // until the PA is reachable, the beep only while the drive
                                // is actually up - two questions, one control.
                                (
                                    ControlId::PaDriveWarning,
                                    (pa_drive_warning as u16) | ((pa_drive_alarm_was_on as u16) << 1),
                                ),
                                (ControlId::MonitorOn, mon_on as u16),
                                (ControlId::AgcMode, agc_mode as u16),
                                (ControlId::AgcGain, agc_gain as u16),
                                (ControlId::RitEnable, rit_enable as u16),
                                (ControlId::RitOffset, rit_offset as i16 as u16),
                                (ControlId::XitEnable, xit_enable as u16),
                                (ControlId::XitOffset, xit_offset as i16 as u16),
                                (ControlId::SqlEnable, sql_enable as u16),
                                (ControlId::SqlLevel, sql_level as u16),
                                (ControlId::NoiseBlanker, nb_level as u16),
                                (ControlId::CwKeyerSpeed, cw_keyer_speed as u16),
                                (ControlId::VfoLock, vfo_lock as u16),
                                (ControlId::Binaural, binaural as u16),
                                (ControlId::ApfEnable, apf_enable as u16),
                                (ControlId::Mute, mute as u16),
                                (ControlId::RxMute, rx_mute as u16),
                                (ControlId::ManualNotchFilter, nf_enable as u16),
                                (ControlId::RxBalance, rx_balance as i16 as u16),
                                (ControlId::TuneDrive, tune_drive as u16),
                                (ControlId::DdcSampleRateRx1, (ddc_rate_rx1 / 1000) as u16),
                                (ControlId::DdcSampleRateRx2, (ddc_rate_rx2 / 1000) as u16),
                                (ControlId::DdcCentreRx1, sdr_remote_core::protocol::ddc_centre_to_khz(ddc_centre_rx1)),
                                (ControlId::DdcCentreRx2, sdr_remote_core::protocol::ddc_centre_to_khz(ddc_centre_rx2)),
                                (ControlId::AgcAutoRx1, agc_auto_rx1 as u16),
                                (ControlId::AgcAutoRx2, agc_auto_rx2 as u16),
                                (ControlId::DiversityEnable, diversity_enabled as u16),
                                (ControlId::DiversityPhase, (div_phase + 18000).max(0) as u16),
                                (ControlId::DiversityGainRx1, div_gain_rx1),
                                (ControlId::DiversityGainRx2, div_gain_rx2),
                                (ControlId::DiversityGainMulti, div_gain_multi),
                                (ControlId::DiversityAutoNull, diversity_autonull_done),
                                (ControlId::VfoSync, vfo_sync_on as u16),
                                (ControlId::MonitorVolume, mon_volume as i8 as i16 as u16),
                            ];
                            for &(id, value) in controls {
                                let key = id as u8;
                                if prev_controls.get(&key) == Some(&value) {
                                    continue; // unchanged, skip
                                }
                                prev_controls.insert(key, value);
                                let pkt = ControlPacket { control_id: id, value };
                                let mut buf = [0u8; ControlPacket::SIZE];
                                pkt.serialize(&mut buf);
                                for addr in &all_addrs {
                                    let _ = socket.try_send_to(&buf, *addr);
                                }
                            }

                            // TX modulation filter band - push only when Thetis
                            // reports it (stock 2.10.3.14+/fork), on change. The
                            // client uses this both to display the value and to
                            // know that setting it is supported.
                            if tx_filter_known && prev_tx_filter != Some((tx_filter_low, tx_filter_high)) {
                                prev_tx_filter = Some((tx_filter_low, tx_filter_high));
                                let pkt = TxFilterBandPacket { low_hz: tx_filter_low, high_hz: tx_filter_high };
                                let mut buf = [0u8; TxFilterBandPacket::SIZE];
                                pkt.serialize(&mut buf);
                                for addr in &all_addrs {
                                    let _ = socket.try_send_to(&buf, *addr);
                                }
                            }

                            // TX profile names - only on change
                            if !tx_profile_names.is_empty() && tx_profile_names != prev_tx_profile_names {
                                prev_tx_profile_names = tx_profile_names.clone();
                                let active = tx_profile_names.iter()
                                    .position(|n| n == &tx_profile_name)
                                    .unwrap_or(0) as u8;
                                let pkt = TxProfilesPacket { names: tx_profile_names, active };
                                let mut buf = Vec::new();
                                pkt.serialize(&mut buf);
                                for addr in &all_addrs {
                                    let _ = socket.try_send_to(&buf, *addr);
                                }
                            }

                            // Yaesu state broadcast moved to separate task

                            // RX2 spectrum-processor center/VFO: ALWAYS update, independent of
                            // RX2-AUDIO subscribers. Otherwise the DDC center stays at 0 and the
                            // RX2 spectrum shows nonsense (wobbling line) as long as nobody has
                            // RX2-AUDIO on — while the RX2 spectrum is a subscription of its own
                            // (phase 3b/4, just as RX1 always does).
                            {
                                let mut rx2_sp = rx2_spectrum.lock().await;
                                if vfo_b_freq != 0 { rx2_sp.set_vfo_freq(vfo_b_freq, ctun); }
                                if dds_rx2 != 0 { rx2_sp.set_ddc_center(dds_rx2); }
                            }

                            // RX2 display broadcasts to audio AND spectrum subscribers.
                            if !rx2_display_addrs.is_empty() {
                                if vfo_b_freq != 0 && vfo_b_freq != prev_vfo_b_freq {
                                    prev_vfo_b_freq = vfo_b_freq;
                                    let pkt = FrequencyPacket { frequency_hz: vfo_b_freq };
                                    let mut buf = [0u8; FrequencyPacket::SIZE];
                                    pkt.serialize_as_type(&mut buf, PacketType::FrequencyRx2);
                                    for addr in &rx2_display_addrs {
                                        let _ = socket.try_send_to(&buf, *addr);
                                    }
                                }

                                if vfo_b_mode != prev_vfo_b_mode {
                                    prev_vfo_b_mode = vfo_b_mode;
                                    let pkt = ModePacket { mode: vfo_b_mode };
                                    let mut buf = [0u8; ModePacket::SIZE];
                                    pkt.serialize_as_type(&mut buf, PacketType::ModeRx2);
                                    for addr in &rx2_display_addrs {
                                        let _ = socket.try_send_to(&buf, *addr);
                                    }
                                }

                                // RX2 S-meter: per-client subscription mirror of RX1.
                                // Bits 4/5/6 in SmeterSources control which RX2 packet
                                // types are emitted. Default mask 0x22 -> bit 5 (Avg) only.
                                {
                                    let mut buf_avg = [0u8; SmeterPacket::SIZE];
                                    SmeterPacket { level: smeter_rx2, flags: Flags::NONE }.serialize_as_type(&mut buf_avg, PacketType::SmeterRx2);
                                    let mut buf_sig = [0u8; SmeterPacket::SIZE];
                                    SmeterPacket { level: smeter_rx2_sig, flags: Flags::NONE }.serialize_as_type(&mut buf_sig, PacketType::SmeterRx2Sig);
                                    let mut buf_pkb = [0u8; SmeterPacket::SIZE];
                                    SmeterPacket { level: smeter_rx2_peakbin, flags: Flags::NONE }.serialize_as_type(&mut buf_pkb, PacketType::SmeterRx2MaxBin);
                                    let sess = session.lock().await;
                                    for addr in &rx2_display_addrs {
                                        let mask = sess.smeter_sources(*addr);
                                        if mask & 0x10 != 0 { let _ = socket.try_send_to(&buf_sig, *addr); }
                                        if mask & 0x20 != 0 { let _ = socket.try_send_to(&buf_avg, *addr); }
                                        if mask & 0x40 != 0 { let _ = socket.try_send_to(&buf_pkb, *addr); }
                                    }
                                    drop(sess);
                                }
                                // RX2 control states
                                let rx2_controls: &[(ControlId, u16)] = &[
                                    (ControlId::Rx2AfGain, rx2_af_gain as u16),
                                    (ControlId::Rx2FilterLow, rx2_filter_low as i16 as u16),
                                    (ControlId::Rx2FilterHigh, rx2_filter_high as i16 as u16),
                                    (ControlId::Rx2NoiseReduction, rx2_nr_level as u16),
                                    (ControlId::Rx2AutoNotchFilter, rx2_anf as u16),
                                    (ControlId::Rx2AgcMode, rx2_agc_mode as u16),
                                    (ControlId::Rx2AgcGain, rx2_agc_gain as u16),
                                    (ControlId::Rx2SqlEnable, rx2_sql_enable as u16),
                                    (ControlId::Rx2SqlLevel, rx2_sql_level as u16),
                                    (ControlId::Rx2NoiseBlanker, rx2_nb_enable as u16),
                                    (ControlId::Rx2Binaural, rx2_binaural as u16),
                                    (ControlId::Rx2ApfEnable, rx2_apf_enable as u16),
                                    (ControlId::Rx2VfoLock, rx2_vfo_lock as u16),
                                    (ControlId::Rx2ManualNotchFilter, rx2_nf_enable as u16),
                                ];
                                for &(id, value) in rx2_controls {
                                    let pkt = ControlPacket { control_id: id, value };
                                    let mut buf = [0u8; ControlPacket::SIZE];
                                    pkt.serialize(&mut buf);
                                    for addr in &rx2_display_addrs {
                                        let _ = socket.try_send_to(&buf, *addr);
                                    }
                                }
                            }
                        }
                        _ = shutdown.changed() => break,
                    }
                }
            })
        };

        // TX resampler: 16kHz wideband -> TCI rate (for mic audio from client)
        let mut tx_resampler_out = resampler(NETWORK_SAMPLE_RATE_WIDEBAND, playback_rate, FRAME_SAMPLES_WIDEBAND, UpFilter::Long)
            .context("create 16k->device TX resampler")?;
        info!(
            "Thetis TX resampler: {} Hz to {} Hz, {}",
            NETWORK_SAMPLE_RATE_WIDEBAND, playback_rate, tx_resampler_out
        );

        // Main RX loop
        // Whether the safety task has ended (watched in the loop below).
        let mut safety_lost = false;
        // The two Yaesu state tasks carry the orphan watch (PTT-NOOIT-HANGEN 4.3)
        // and the release on the radio's own time-out: watched the same way
        // (review build 57, both reviewers). A slot without a radio gets a task
        // that never ends.
        let mut yaesu_state_watch = watched(yaesu_state_handle);
        let mut yaesu2_state_watch = watched(yaesu2_state_handle);
        let mut yaesu_state_lost = false;
        let mut yaesu2_state_lost = false;
        let mut recv_buf = vec![0u8; MAX_PACKET_SIZE];
        // The microphone's decoder: the core's, so a lost frame has the client's
        // one rule - rebuilt from the next packet's redundancy, else concealed
        // (build 166, `tx_recover::fill_gap`).
        let mut opus_decoder = sdr_remote_core::stream::StreamDecoder::new()?;
        let mut thetis_tx_loss = crate::tx_recover::TxLoss::new("Thetis");
        // A transmit buffer: the release is played out, so it does not wait
        // for its target with the release in hand (build 237). Tuning B, and no
        // slow trim of its own (`Tuning::transmit`, build 258).
        let mut jitter_buf = JitterBuffer::for_transmit(TX_MIN_DEPTH, TX_MAX_DEPTH);
        // It catches up in the pauses of speech instead (`pause_trim.rs`, build
        // 258): the tick lets a quiet frame go. The fast trim and the cap stay.
        let mut pause_trim = crate::pause_trim::PauseTrim::new(Instant::now());
        // Named in advance (working rule 11, build 258); one text with the
        // radios' since build 263.
        dev_info!("{}", crate::tx_recover::buffer_line("Thetis", Some(crate::tci::TX_HOLD_MAX)));
        let mut tx_holder_addr: Option<SocketAddr> = None;
        // Whose line the jitter buffer has learned: the same sender pressing
        // again keeps it, as a Yaesu does (owner, 2026-10-11: "thetis gaat met
        // yaesu mee. Voor zekerheid van geen verlies van audio"; build 175).
        let mut line_learned_from: Option<SocketAddr> = None;
        // Who has been told their transmit request was refused, and when.
        //
        // A named type rather than a bare map, so its lifetime can be tested
        // and not just the interval - the bookkeeping leaked an entry for every
        // client that fell silent while the interval test stayed green
        // (review finding, round 2). The exits live with it.
        let mut denied = crate::tx_denial::DenialLog::default();
        // When a client was last told about in the log for a repeated control:
        // a repeat is normal on a lossy path, one line per five minutes is plenty.
        let mut control_repeat_said: std::collections::HashMap<SocketAddr, Instant> =
            std::collections::HashMap::new();

        let mut shutdown = self.shutdown.clone();
        let mut playout_tick = interval(Duration::from_millis(20));
        let mut pending_filter_low: Option<i32> = None;
        let mut pending_rx2_filter_low: Option<i32> = None;
        let mut pending_tx_filter_low: Option<i32> = None;

        let mut yaesu_write = MemoryWrite::default();
        // If the YaesuWriteMemories control arrives BEFORE the
        // YaesuMemoryData packet (the data is ~2.7kB, IP-fragmented;
        // control is ~8B, single packet - control can overtake on
        // reorder), latch the intent and fire the write when the data
        // finally lands. Otherwise the trigger was silently dropped.

        let yaesu_mic_gain = Arc::new(AtomicU32::new(1.0_f32.to_bits())); // Yaesu gain now lives before Opus in the client
        let yaesu2_mic_gain = Arc::new(AtomicU32::new(1.0_f32.to_bits()));
        // Slot-1 memory-write latch (Phase B).
        let mut yaesu2_write = MemoryWrite::default();

        // The release frames the radios' tasks played (build 282), back to
        // this loop, which owns the transmitters.
        let (released_tx, mut released_rx) = tokio::sync::mpsc::unbounded_channel::<crate::tx_recover::ReleasePlayed>();
        let yaesu_tx_packet_tx = {
            let tx_audio_tx = yaesu.as_ref().and_then(|y| y.tx_audio_tx.clone());
            let tx_rate = yaesu.as_ref().map(|y| y.tx_sample_rate).unwrap_or(0);
            let timing = yaesu.as_ref().map(|y| y.status_arc().lock().unwrap().tx_timing.clone());
            if let (Some(tx_audio), true) = (tx_audio_tx, tx_rate > 0) {
                let (pkt_tx, pkt_rx) = tokio::sync::mpsc::channel::<crate::tx_recover::TxPacket>(64);
                // One task for both radios, with the jitter buffer and the gap
                // rule Thetis's transmit audio has (build 166).
                tokio::spawn(crate::tx_recover::radio_tx_task("Radio 1", crate::session::TxTarget::Yaesu1, pkt_rx, tx_audio, yaesu_mic_gain.clone(), timing, tx_rate, released_tx.clone()));
                Some(pkt_tx)
            } else {
                None
            }
        };

        // Slot-1 TX decode task (Option B-prime) - exact mirror of slot 0,
        // fed by AudioYaesu2 -> yaesu2's tx_audio_tx.
        let yaesu2_tx_packet_tx = {
            let tx_audio_tx = yaesu2.as_ref().and_then(|y| y.tx_audio_tx.clone());
            let tx_rate = yaesu2.as_ref().map(|y| y.tx_sample_rate).unwrap_or(0);
            let timing = yaesu2.as_ref().map(|y| y.status_arc().lock().unwrap().tx_timing.clone());
            if let (Some(tx_audio), true) = (tx_audio_tx, tx_rate > 0) {
                let (pkt_tx, pkt_rx) = tokio::sync::mpsc::channel::<crate::tx_recover::TxPacket>(64);
                // One task for both radios, with the jitter buffer and the gap
                // rule Thetis's transmit audio has (build 166).
                tokio::spawn(crate::tx_recover::radio_tx_task("Radio 2", crate::session::TxTarget::Yaesu2, pkt_rx, tx_audio, yaesu2_mic_gain.clone(), timing, tx_rate, released_tx.clone()));
                Some(pkt_tx)
            } else {
                None
            }
        };

        // Forgetting Thetis's holder, in one place (build 248, working rule 18):
        // the playout buffer with that client's last audio - it carries the PTT
        // bit and keys Thetis when it is played - who held it, and what the
        // buffer learned of its line. Used by a goodbye, a lost guard task, and
        // a holder the session let go elsewhere.
        macro_rules! forget_thetis_holder {
            () => {
                jitter_buf.reset();
                tx_holder_addr = None;
                line_learned_from = None;
            };
        }
        // The holder this loop remembers lost the transmitter elsewhere - a
        // time-out, the end of a CW run (build 247). Asked before anyone takes
        // Thetis again, with the session's holder read before that: the audio
        // arm and the CW gate (build 248).
        macro_rules! forget_thetis_holder_if_outlived {
            ($holder_now:expr) => {
                if let Some(old) = tx_holder_addr.filter(|_| crate::session::holder_outlived(tx_holder_addr, $holder_now)) {
                    // Named in advance (working rule 11, build 247).
                    info!("TX holder {} lost the transmitter while silent - its buffered audio goes, as at a goodbye", old);
                    forget_thetis_holder!();
                }
            };
        }
        // The one reaction for every task that carries a PTT safety net: when it
        // ends while the server runs, everything that is ours is let go and
        // transmitting is closed (`safety_task_lost`). The playout buffer goes
        // too: buffered audio carries the PTT bit and keyed Thetis again after a
        // release once (see the goodbye).
        macro_rules! on_guard_task_end {
            ($svc:expr, $end:expr, $task:expr) => {
                if let Some(why) = safety_task_end($end.as_ref().err().map(|e| e.is_panic()), *shutdown.borrow()) {
                    forget_thetis_holder!();
                    $svc.safety_task_lost($task, why).await;
                }
            };
        }
        // The newest packet each transmitter had from its sender, and how many
        // of its packets with the flag were ignored since it was last taken -
        // Thetis's and both radios' (`tx_intake::Senders`, build 285; the
        // radios' since 282). Declared before `radio_audio!`, which binds the
        // locals it sees at its definition.
        let mut senders = crate::tx_intake::Senders::default();
        // What an arriving transmit packet does, for Thetis's arm and the
        // radios' (build 286; round 285: the four ways out were pinned in the
        // radios' block only). The decision (`tx_intake::intake`) and its four
        // ways out - nothing, too late, let go by the radio, a refused claim -
        // in one block; it gives whether the packet claimed its transmitter.
        // `$label` and `$radio_refusal` are evaluated only where they are used:
        // the label for the first of a run of ignored packets, the radio's own
        // refusal only for a claim (round 265: a refused client's packets come
        // fifty times a second).
        macro_rules! intake_gate {
            ($target:expr, $addr:expr, $seq:expr, $arrival:expr, $label:expr, $radio_refusal:expr) => {{
                let (target, addr, seq) = ($target, $addr, $seq);
                match crate::tx_intake::intake($arrival) {
                    crate::tx_intake::Intake::Pass => continue,
                    crate::tx_intake::Intake::Stale => {
                        if senders.ignored(target) {
                            // Named in advance (working rule 11, builds 282 and 285).
                            info!("{}", crate::tx_intake::stale_line($label, addr, seq));
                        }
                        continue;
                    }
                    crate::tx_intake::Intake::LetGoByRadio => {
                        if senders.ignored(target) {
                            // Named in advance (working rule 11, build 282).
                            info!("{}", crate::tx_intake::let_go_line($label, addr));
                        }
                        continue;
                    }
                    crate::tx_intake::Intake::Claim => {
                        // A statement of its own: a Yaesu's refusal is read
                        // under its status lock, and a temporary in the `if`
                        // below would live across the claim's `.await` - the
                        // serial thread of the holder waiting on a refused
                        // client's packets (round 286).
                        let radio_refusal: Option<&'static str> = $radio_refusal;
                        if !self.claim_tx(target, addr, radio_refusal, &mut denied).await {
                            continue;
                        }
                        true
                    }
                    crate::tx_intake::Intake::Play => false,
                }
            }};
        }
        // A radio's transmit audio and its PTT, both radios through this one
        // block (build 282, open point 39): the PTT is the flag on the packet,
        // as Thetis's. The first packet with the flag takes the radio and keys
        // it on arrival - not at playout as Thetis does: the serial line and
        // the 991A's USB routing cost 40-100 ms, and that now overlaps the
        // buffer's fill. The holder's packets go to the radio's task, the
        // release among them: the first without the flag, let go when it
        // plays (`release_played`). Until build 282 the PTT was a control of
        // its own, and its "off" could arrive before the last of the audio.
        macro_rules! radio_audio {
            ($pkt:expr, $addr:expr, $slot:expr, $target:expr, $packet_tx:expr, $stream:expr) => {{
                let (pkt, addr, slot, target) = ($pkt, $addr, $slot as usize, $target);
                // No radio in this slot: no claim on its transmitter (review
                // 2120A3).
                let Some((radio, flag)) = self.radio_and_flag(target) else { continue };
                let ptt = pkt.flags.ptt();
                if !ptt {
                    // Not asking any more: the next refusal is a first one again.
                    denied.attempt_over(&addr);
                }
                let (holder, let_go_by_radio) = {
                    let mut s = self.session.lock().await;
                    s.heard_on(target, addr, Instant::now());
                    if !ptt {
                        s.let_go_seen(target, addr);
                    }
                    (s.tx_holder(target), s.let_go_by_the_radio_and_still_pressing(target, addr))
                };
                // What the packet does, decided where Thetis's is (build 285).
                let stale = senders.stale(target, addr, pkt.sequence, Instant::now());
                let arrival = crate::tx_intake::Arrival {
                    ptt,
                    from_holder: holder == Some(addr),
                    keyed: flag.load(Ordering::Relaxed),
                    stale,
                    let_go_by_radio,
                };
                // The radio's own refusal, asked of its state in place:
                // `status()` copies the whole state (round 265).
                let claimed = intake_gate!(
                    target,
                    addr,
                    pkt.sequence,
                    arrival,
                    radio.status_arc().lock().unwrap().tx_timing.label(),
                    radio.status_arc().lock().unwrap().tx_start_refusal()
                );
                if claimed && !flag.load(Ordering::Relaxed) {
                    senders.taken(target);
                    self.key_radio(target, addr);
                }
                // The holder's packet, with the flag or its release.
                if !stale {
                    senders.played(target, addr, pkt.sequence, Instant::now());
                }
                // The server's measurement sees a radio's packets too (build
                // 263), as they go to its task - with their flag (build 282).
                if let Some(m) = tx_measure.as_mut() {
                    m.record(crate::tx_arrivals::TxArrival {
                        wall_ms: crate::tx_arrivals::wall_ms_now(),
                        mono_ms: start.elapsed().as_millis() as u64,
                        seq: pkt.sequence,
                        ts: pkt.timestamp,
                        bytes: pkt.opus_data.len(),
                        ptt,
                        stream: $stream,
                    });
                }
                if let Some(ref tx) = $packet_tx {
                    let audio = !pkt.opus_data.is_empty();
                    // Only tick on successful enqueue - dropped frames
                    // (channel full) didn't reach the radio.
                    if tx.try_send(crate::tx_recover::TxPacket { from: addr, sequence: pkt.sequence, timestamp: pkt.timestamp, opus: pkt.opus_data, ptt }).is_ok() && audio {
                        self.audio_stats.yaesu_tx[slot].tick(self.server_start);
                    }
                }
            }};
        }
        loop {
            tokio::select! {
                result = self.socket.recv_from(&mut recv_buf) => {
                    let (len, addr) = match result {
                        Ok(v) => v,
                        // The only error left is the QUIC endpoint itself gone
                        // (BrokenPipe). The Windows UDP quirk this loop used to
                        // skip (a reset after a client vanished, the "force-stop
                        // the app -> server crashes" report) is quinn's to skip
                        // now, on its own socket (build 133): quinn 0.11.9,
                        // src/endpoint.rs:839-843, "Ignore ECONNRESET".
                        Err(e) => return Err(e).context("recv_from"),
                    };
                    let data = &recv_buf[..len];

                    // Protocol-version mismatch: detect BEFORE Packet::deserialize so we
                    // can record it in the Status-panel ringbuffer. Otherwise the bail!
                    // from Header::deserialize lands in the generic "Invalid packet" log
                    // branch and the operator has no UI indication that an old client
                    // (e.g. v2.0.2 APK against a build-58+ server) is retrying.
                    if data.len() >= 2 && data[0] == MAGIC && data[1] != VERSION {
                        let client_version = data[1];
                        let mut session = self.session.lock().await;
                        // De-dup against the previous entry for the same addr+version so
                        // a reconnecting old client does not flood the 10-slot ringbuffer.
                        let already_recent = session
                            .recent_connect_attempts()
                            .last()
                            .map(|a| {
                                a.remote_addr == addr
                                    && matches!(
                                        a.outcome,
                                        crate::session::ConnectOutcome::ProtocolVersionMismatch {
                                            client_version: v
                                        } if v == client_version
                                    )
                            })
                            .unwrap_or(false);
                        if !already_recent {
                            session.record_connect_attempt(
                                addr,
                                crate::session::ConnectOutcome::ProtocolVersionMismatch {
                                    client_version,
                                },
                            );
                        }
                        drop(session);
                        if !already_recent {
                            // Log + back-channel reply happen at most once per
                            // addr+version pair (gated on the same de-dup as
                            // the status-panel ringbuffer entry). Without this,
                            // a v2.0.2 client retrying connect every ~500 ms
                            // would flood the server log at >100 lines/min
                            // before its own ProtocolVersionMismatch UX kicks
                            // in and the user closes the app.
                            info!(
                                "Rejecting packet from {} (client protocol v{}, server v{})",
                                addr, client_version, VERSION
                            );
                            // 4-byte back-channel rejection so the client can
                            // surface a localised ProtocolVersionMismatch
                            // outcome instead of a generic "server unreachable"
                            // timeout. Header layout is
                            // `[MAGIC, VERSION, packet_type, flags]`; the
                            // client's `Header::deserialize` checks the version
                            // byte before the packet_type, so any value works
                            // for bytes 2-3 - we use 0xFF as an "obviously
                            // meaningless" type sentinel. The client falls into
                            // the `Err(e)` recovery in
                            // `sdr-remote-logic/src/engine.rs` (around L2274)
                            // which checks for MAGIC + wrong-VERSION and raises
                            // `ConnectError::ProtocolVersionMismatch`.
                            let rejection: [u8; 4] = [MAGIC, VERSION, 0xFF, 0x00];
                            if let Err(e) = self.socket.send_to(&rejection, addr).await {
                                warn!(
                                    "Failed to send protocol-mismatch rejection to {}: {}",
                                    addr, e
                                );
                            }
                        }
                        continue;
                    }

                    let packet = match Packet::deserialize(data) {
                        Ok(p) => p,
                        Err(e) => {
                            // Unknown packet types from old clients - silently ignore.
                            //
                            // An unknown *control id* is the same situation and
                            // was not covered: a newer client sending a control
                            // this build does not know would be logged for every
                            // packet. The VRX pan is the case that made it
                            // concrete - a control a client can send repeatedly
                            // while dragging - but the damping belongs to the
                            // class, not to that one control.
                            let text = e.to_string();
                            let expected = text.contains("unknown packet type")
                                || text.contains("control id");
                            if !expected {
                                info!("Invalid packet from {} ({}B): {}", addr, len, e);
                            } else if !text.contains("unknown packet type")
                                && unknown_control_worth_logging(addr)
                            {
                                // Once per client per five minutes. Knowing a
                                // client is ahead of this build is worth a line;
                                // the hundredth buries what else is in the log,
                                // and silence for every other client would bury
                                // it just as thoroughly.
                                info!(
                                    "Client {} uses a control this build does not know ({}) - ignored",
                                    addr, e
                                );
                            }
                            continue;
                        }
                    };

                    // --- Authentication gate ---
                    {
                        let mut session = self.session.lock().await;

                        // Rate-limit check - on the real peer behind a QUIC sentinel.
                        if session.is_blocked(self.socket.throttle_key(addr)) {
                            continue; // Silently drop
                        }

                        // Handle auth packets regardless of state
                        match &packet {
                            Packet::AuthResponse { hmac, client_nonce } => {
                                // Over QUIC: the binding of the connection this
                                // response came in on, or nothing at all when that
                                // connection is gone - then nothing is answered.
                                let Ok(channel) = self.socket.auth_channel(addr) else {
                                    continue;
                                };
                                let verdict = session.verify_auth(addr, hmac, *client_nonce, channel);
                                let result = verdict.code;
                                if result == crate::session::AUTH_NOT_ANSWERED {
                                    // Nothing pending for this address: nothing to say.
                                    // Its next packet gets a fresh challenge the usual way.
                                    continue;
                                }
                                // PATCH-2: record outcome in Status-panel ringbuffer.
                                let outcome = match result {
                                    sdr_remote_core::protocol::AUTH_ACCEPTED => {
                                        Some(crate::session::ConnectOutcome::Accepted)
                                    }
                                    sdr_remote_core::protocol::AUTH_TOTP_REQUIRED => {
                                        Some(crate::session::ConnectOutcome::TotpRequired)
                                    }
                                    sdr_remote_core::protocol::AUTH_REJECTED => {
                                        Some(crate::session::ConnectOutcome::WrongPassword)
                                    }
                                    _ => None,
                                };
                                if let Some(o) = outcome {
                                    session.record_connect_attempt(addr, o);
                                }
                                drop(session);
                                // Test switch: hand the client a false proof, to see its
                                // "server proof invalid" line in the field
                                // (THETISLINK_TEST_BAD_PROOF=1). Nothing else can produce one.
                                let proof = verdict.proof.map(|mut p| {
                                    if std::env::var("THETISLINK_TEST_BAD_PROOF").map_or(false, |v| v == "1") {
                                        warn!("THETISLINK_TEST_BAD_PROOF is set: a false proof goes to {} - a test, not a fault", addr);
                                        p[0] ^= 0xff;
                                    }
                                    p
                                });
                                let mut buf = [0u8; AUTH_RESULT_SIZE_WITH_PROOF];
                                let n = serialize_auth_result(&mut buf, result, proof.as_ref());
                                let _ = self.socket.try_send_to(&buf[..n], addr);
                                if result == sdr_remote_core::protocol::AUTH_ACCEPTED {
                                    self.tell_if_closed(addr).await;
                                }
                                // If TOTP required, also send TotpChallenge
                                if result == sdr_remote_core::protocol::AUTH_TOTP_REQUIRED {
                                    let mut totp_buf = [0u8; 4];
                                    let totp_header = Header::new(PacketType::TotpChallenge, Flags::NONE);
                                    totp_header.serialize(&mut totp_buf);
                                    let _ = self.socket.try_send_to(&totp_buf, addr);
                                }
                                continue;
                            }
                            Packet::TotpResponse(code) => {
                                let accepted = match session.verify_totp(addr, code, self.socket.throttle_key(addr)) {
                                    Some(a) => a,
                                    // Nothing pending: nothing to say (see AUTH_NOT_ANSWERED).
                                    None => continue,
                                };
                                // PATCH-2: record TOTP outcome in Status-panel ringbuffer.
                                let outcome = if accepted {
                                    crate::session::ConnectOutcome::Accepted
                                } else {
                                    crate::session::ConnectOutcome::WrongTotp
                                };
                                session.record_connect_attempt(addr, outcome);
                                drop(session);
                                // The final verdict. No proof here: it went with the
                                // password step, and the code step adds nothing to it.
                                let code = if accepted { sdr_remote_core::protocol::AUTH_ACCEPTED } else { sdr_remote_core::protocol::AUTH_REJECTED };
                                let mut buf = [0u8; AUTH_RESULT_SIZE_WITH_PROOF];
                                let n = serialize_auth_result(&mut buf, code, None);
                                let _ = self.socket.try_send_to(&buf[..n], addr);
                                if accepted {
                                    self.tell_if_closed(addr).await;
                                }
                                continue;
                            }
                            _ => {}
                        }

                        if session.auth_required() {
                            if !session.is_authenticated(addr) {
                                // A stranger, or a knock at the code step, gets a
                                // challenge; a client answering one does not. The rule
                                // is the session manager's (`wants_challenge`).
                                if session.wants_challenge(addr, &packet) {
                                    let nonce = session.create_challenge(addr);
                                    // PATCH-2: record diagnostic "client started handshake".
                                    session.record_connect_attempt(
                                        addr,
                                        crate::session::ConnectOutcome::ChallengeSent,
                                    );
                                    drop(session);
                                    let mut buf = [0u8; AUTH_CHALLENGE_SIZE];
                                    serialize_auth_challenge(&mut buf, &nonce);
                                    let _ = self.socket.try_send_to(&buf, addr);
                                }
                                continue; // Drop all non-auth packets from unauthenticated clients
                            }
                        }

                        session.touch(addr);
                    }

                    // Phase 2b: a numbered control is acknowledged whatever happens
                    // to it - the client wants to stop repeating - and applied only
                    // if it is newer than what this session applied for the same
                    // control. That last part keeps a late repeat of "Yaesu PTT on"
                    // from keying the radio after "off" (control_ack.rs).
                    if let Some((key, seq)) = sdr_remote_core::protocol::numbered_control(data) {
                        let mut ack = [0u8; ControlAckPacket::SIZE];
                        ControlAckPacket::serialize(seq, &mut ack);
                        let _ = self.socket.try_send_to(&ack, addr);
                        let verdict = self.session.lock().await.judge_control(addr, key, seq);
                        if let Some(v) = verdict {
                            if v != sdr_remote_core::control_ack::Verdict::Apply {
                                let say = control_repeat_said
                                    .get(&addr)
                                    .map_or(true, |t| t.elapsed() >= Duration::from_secs(300));
                                if say {
                                    control_repeat_said.insert(addr, Instant::now());
                                    info!(
                                        "Control {:?} seq {} from {} not applied ({:?}) - a repeat; once per 5 minutes",
                                        key, seq, addr, v
                                    );
                                }
                                continue;
                            }
                        }
                    }

                    match packet {
                        Packet::Audio(audio_pkt) => {
                            // Thetis's transmit audio. What the packet does is decided
                            // where a radio's is (`tx_intake`, build 285); the buffer,
                            // the decoder and the keying at playout stay here.
                            let target = crate::session::TxTarget::Thetis;
                            let ptt_requested = audio_pkt.flags.ptt();
                            if !ptt_requested {
                                // Not asking any more, so this attempt is over and
                                // the next refusal counts as a first one again.
                                denied.attempt_over(&addr);
                            }
                            // How long the client was silent before this packet, read
                            // before the session's lock: for every audio packet (builds
                            // 248, 249).
                            let silence = self.ptt.lock().await.packet_silence(Instant::now());
                            let mut session = self.session.lock().await;

                            if ptt_requested {
                                forget_thetis_holder_if_outlived!(session.tx_holder(crate::session::TxTarget::Thetis));
                            }
                            // A silence past the safety's packet window inside a press
                            // (build 248): what waits in the buffer is from before it -
                            // packets under the target when the line went - and would
                            // play first, with PTT. It goes before this packet goes in,
                            // with PTT or without: a release packet first after the
                            // silence lets the buffer play below its target (build 237),
                            // the oldest first (build 249; round 248). What the buffer
                            // learned of the line stays.
                            if tx_holder_addr == Some(addr) && crate::ptt::silence_ended_the_over(silence) {
                                // Named in advance (working rule 11, builds 248 and 249).
                                info!("TX holder {} silent for {} ms, past the packet window - its buffered audio goes", addr, silence.map_or(0, |d| d.as_millis()));
                                jitter_buf.discard();
                            }
                            session.heard_on(target, addr, Instant::now());
                            if !ptt_requested {
                                session.let_go_seen(target, addr);
                            }
                            let holder = session.tx_holder(target);
                            let let_go_by_radio = session.let_go_by_the_radio_and_still_pressing(target, addr);
                            drop(session);

                            let stale = senders.stale(target, addr, audio_pkt.sequence, Instant::now());
                            // Thetis keys when its buffer plays: its holder's packets play.
                            let arrival = crate::tx_intake::Arrival {
                                ptt: ptt_requested,
                                from_holder: holder == Some(addr),
                                keyed: holder == Some(addr),
                                stale,
                                let_go_by_radio,
                            };
                            if intake_gate!(target, addr, audio_pkt.sequence, arrival, "Thetis", None) {
                                senders.taken(target);
                            }
                            if !stale {
                                senders.played(target, addr, audio_pkt.sequence, Instant::now());
                            }

                            if ptt_requested && tx_holder_addr != Some(addr) {
                                if line_learned_from == Some(addr) {
                                    // Named in advance (working rule 11, build 175).
                                    info!("TX holder {} again, keeping what the jitter buffer learned", addr);
                                    jitter_buf.discard();
                                } else {
                                    info!("New TX holder {}, resetting jitter buffer and decoder", addr);
                                    jitter_buf.reset();
                                    line_learned_from = Some(addr);
                                }
                                opus_decoder = sdr_remote_core::stream::StreamDecoder::new().unwrap();
                                tx_holder_addr = Some(addr);
                            }

                            // A press, or the holder's release: its tail audio plays
                            // out, and the release triggers from playout.
                            self.ptt.lock().await.record_ptt_packet();

                            let arrival_ms = start.elapsed().as_millis() as u64;
                            if let Some(m) = tx_measure.as_mut() {
                                m.record(crate::tx_arrivals::TxArrival {
                                    wall_ms: crate::tx_arrivals::wall_ms_now(),
                                    mono_ms: arrival_ms,
                                    seq: audio_pkt.sequence,
                                    ts: audio_pkt.timestamp,
                                    bytes: audio_pkt.opus_data.len(),
                                    ptt: ptt_requested,
                                    stream: crate::tx_arrivals::TxStream::Thetis,
                                });
                            }
                            let wideband = audio_pkt.flags.wideband();
                            jitter_buf.push(
                                BufferedFrame {
                                    sequence: audio_pkt.sequence,
                                    timestamp: audio_pkt.timestamp,
                                    opus_data: audio_pkt.opus_data,
                                    ptt: ptt_requested,
                                    wideband,
                                },
                                arrival_ms,
                            );
                        }
                        Packet::Heartbeat(hb) => {
                            let thetis_configured = self.config.thetis_configured();
                            // Acquire PTT lock once: signal heartbeat received + read TCI/process/launch status
                            // in the same scope so we don't lock multiple times per heartbeat.
                            // By name (build 217): a tuple of four booleans let two of them swap.
                            let crate::ptt::ThetisStatus {
                                tci_connected,
                                thetis_running,
                                thetis_starting,
                                rx_only_unguarded,
                            } = {
                                let mut ptt = self.ptt.lock().await;
                                ptt.heartbeat_received();
                                if thetis_configured {
                                    ptt.thetis_status()
                                } else {
                                    crate::ptt::ThetisStatus::default()
                                }
                            };
                            // PATCH-2: feed the Status-panel TCI probe.
                            // Idempotent - only stamps last_state_change on real flips.
                            self.tci_probe.update(tci_connected, self.server_start);
                            {
                                let mut session = self.session.lock().await;
                                session.update_heartbeat(
                                    addr,
                                    hb.sequence,
                                    hb.rtt_ms,
                                    hb.loss_percent,
                                    hb.jitter_ms,
                                );
                                // Only speaks up when the judgement changes, so
                                // a mismatch is stated once per client and not
                                // twice a second for as long as it stays on.
                                if let Some((was, now)) =
                                    session.note_client_build(addr, hb.version)
                                {
                                    let who = format!("The client at {addr}");
                                    if let Some((bad, line)) = now.change_line(&was, &who) {
                                        if bad {
                                            warn!("{}", line);
                                        } else {
                                            info!("{}", line);
                                        }
                                    }
                                }
                            }

                            // PATCH-1: report TCI + Thetis process status in HeartbeatAck.
                            // The two bits together let the client give a targeted hint:
                            //   TCI_CONNECTED clear + THETIS_RUNNING set   -> "Thetis runs, check TCI settings"
                            //   TCI_CONNECTED clear + THETIS_RUNNING clear -> "Thetis is not running, press Start"
                            let mut state_flags = sdr_remote_core::protocol::ServerStateFlags::NONE;
                            if thetis_configured {
                                state_flags = state_flags
                                    .with(sdr_remote_core::protocol::ServerStateFlags::THETIS_CONFIGURED);
                            }
                            if tci_connected {
                                state_flags = state_flags
                                    .with(sdr_remote_core::protocol::ServerStateFlags::TCI_CONNECTED);
                            }
                            if thetis_running {
                                state_flags = state_flags
                                    .with(sdr_remote_core::protocol::ServerStateFlags::THETIS_RUNNING);
                            }
                            if thetis_starting {
                                state_flags = state_flags
                                    .with(sdr_remote_core::protocol::ServerStateFlags::THETIS_STARTING);
                            }
                            // An RX-only position that Thetis cannot guard by itself
                            // (build 215): the clients say so on screen.
                            if rx_only_unguarded {
                                state_flags = state_flags
                                    .with(sdr_remote_core::protocol::ServerStateFlags::RX_ONLY_UNGUARDED);
                            }
                            // Single-receiver radio: advertise so clients hide RX2 + VRX2.
                            // Inverted flag (absent = normal 2-RX default). Startup snapshot;
                            // changing it in the server GUI applies on the next server start.
                            if !self.config.rx2_present {
                                state_flags = state_flags
                                    .with(sdr_remote_core::protocol::ServerStateFlags::SINGLE_RECEIVER);
                            }
                            // No DX cluster here: switched off, or no callsign to
                            // log in with - the cluster stays offline without one.
                            // Inverted flag, so an older server keeps the spot
                            // toggle visible exactly as before.
                            if !self.config.dxcluster_enabled
                                || self.config.dxcluster_callsign.trim().is_empty()
                            {
                                state_flags = state_flags
                                    .with(sdr_remote_core::protocol::ServerStateFlags::NO_DX_CLUSTER);
                            }

                            // PATCH-1 review finding (B3): advertise REPORTS_STATE_FLAGS
                            // so the client knows the state_flags field is authoritative.
                            // Old servers (pre-PATCH-1) leave both at NONE - client must
                            // NOT then assume "TCI down" from an absent flag.
                            // What this server believes the client is subscribed
                            // to, on a packet that already leaves every second.
                            // It carries no instruction: it is there so a client
                            // can notice that the two have drifted apart after a
                            // reconnect whose edge it may not have seen.
                            let subs = self.session.lock().await.subscription_mask(addr);
                            let ack = HeartbeatAck {
                                flags: Flags::NONE,
                                echo_sequence: hb.sequence,
                                echo_time: hb.local_time,
                                capabilities: Capabilities::NONE
                                    .with(Capabilities::REPORTS_STATE_FLAGS),
                                state_flags,
                                subs: Some(subs),
                                // What this server calls itself. The client does
                                // its own comparing: the operator is usually
                                // sitting at the client, and often nowhere near
                                // this screen.
                                version: Some(
                                    sdr_remote_core::protocol::VersionTag::mine(),
                                ),
                            };
                            let mut ack_buf = [0u8; HeartbeatAck::SIZE_WITH_VERSION];
                            ack.serialize_with_version(&mut ack_buf);
                            let _ = self.socket.send_to(&ack_buf, addr).await;
                        }
                        Packet::Frequency(freq_pkt) => {
                            self.ptt.lock().await.set_vfo_a_freq(freq_pkt.frequency_hz).await;
                        }
                        Packet::Mode(mode_pkt) => {
                            // Thetis-bug workaround: a VFO-A mode change during TX
                            // is shown by the indication but NOT applied by Thetis
                            // -> desync. Drop it while transmitting so Thetis never
                            // gets a mid-TX mode change (covers stock + fork, any client).
                            let mut ptt = self.ptt.lock().await;
                            if ptt.is_transmitting() {
                                // The client already shows the mode it asked for,
                                // and the regular mode packet only goes out on a
                                // change - so without this the wrong mode stayed
                                // on its screen. Every sender of the command is
                                // covered here: the buttons, the memories, MIDI,
                                // the phone (owner, 2026-10-11, choice a; build 174).
                                let mode = ptt.vfo_a_mode();
                                drop(ptt);
                                let mut buf = [0u8; ModePacket::SIZE];
                                ModePacket { mode }.serialize(&mut buf);
                                let _ = self.socket.send_to(&buf, addr).await;
                                // Named in advance (working rule 11, build 174).
                                info!("Client {} VFO-A mode change ignored during TX - sent mode {} back", addr, mode);
                            } else {
                                ptt.set_vfo_a_mode(mode_pkt.mode).await;
                            }
                        }
                        Packet::Smeter(_) => {} // server-only, ignore from clients
                        Packet::Spectrum(_) | Packet::FullSpectrum(_) => {} // server-only, ignore from clients
                        Packet::EquipmentStatus(_) => {} // server-only, ignore from clients
                        Packet::ControlAck(_) => {} // server -> client only
                        // Phase 3c: punching, wired in build 124.
                        Packet::PunchOffer(_) | Packet::PunchReady(_) | Packet::PunchLocal(_) => {}
                        Packet::EquipmentCommand(eq_cmd) => {
                            info!("Equipment command from {}: device={:?} cmd=0x{:02X} data={:?}",
                                addr, eq_cmd.device_type, eq_cmd.command_id, eq_cmd.data);
                            match eq_cmd.device_type {
                                DeviceType::Amplitec6x2 => {
                                    match eq_cmd.command_id {
                                        EquipmentCommandPacket::CMD_SET_SWITCH_A => {
                                            if let Some(ref amp) = self.amplitec {
                                                if let Some(&pos) = eq_cmd.data.first() {
                                                    info!("Amplitec: requesting Switch A -> {}", pos);
                                                    amp.send_command(crate::amplitec::AmplitecCmd::SetSwitchA(pos));
                                                }
                                            }
                                        }
                                        EquipmentCommandPacket::CMD_SET_SWITCH_B => {
                                            if let Some(ref amp) = self.amplitec {
                                                if let Some(&pos) = eq_cmd.data.first() {
                                                    info!("Amplitec: requesting Switch B -> {}", pos);
                                                    amp.send_command(crate::amplitec::AmplitecCmd::SetSwitchB(pos));
                                                }
                                            }
                                        }
                                        sdr_remote_core::protocol::CMD_AMPLITEC_SET_POWER_TABLE => {
                                            // 6 x { u16 max_w BE, u8 tx_blocked } = 18 bytes.
                                            // No amplitec device needs to be present; the table
                                            // is server config and applies as soon as the Amplitec
                                            // comes back online.
                                            if eq_cmd.data.len() < 18 {
                                                warn!(
                                                    "Amplitec power-table command too short: {} bytes",
                                                    eq_cmd.data.len()
                                                );
                                            } else {
                                                let mut new_max_w = [None::<u16>; 6];
                                                let mut new_tx_blocked = [false; 6];
                                                for i in 0..6 {
                                                    let off = i * 3;
                                                    let w = u16::from_be_bytes([
                                                        eq_cmd.data[off],
                                                        eq_cmd.data[off + 1],
                                                    ]);
                                                    new_max_w[i] = if w == 0 { None } else { Some(w) };
                                                    new_tx_blocked[i] = eq_cmd.data[off + 2] != 0;
                                                }
                                                info!(
                                                    "Amplitec power-table update from {}: max_w={:?} tx_blocked={:?}",
                                                    addr, new_max_w, new_tx_blocked
                                                );
                                                crate::config::modify_config(|cfg| {
                                                    cfg.amplitec_max_w = new_max_w;
                                                    cfg.amplitec_tx_blocked = new_tx_blocked;
                                                });
                                            }
                                        }
                                        _ => {
                                            debug!("Unknown amplitec command: 0x{:02X}", eq_cmd.command_id);
                                        }
                                    }
                                }
                                DeviceType::Tuner => {
                                    // Multi-tuner routing: pick the tuner bound to the
                                    // active Amplitec-A position; fall back to the primary
                                    // (first enabled) when no Amplitec mapping matches.
                                    let active_pos = self.amplitec
                                        .as_ref()
                                        .map(|a| a.status().switch_a)
                                        .filter(|p| (1..=6).contains(p));
                                    let target = active_pos
                                        .and_then(|p| self.tuners.for_amplitec_pos(p))
                                        .or_else(|| self.tuners.primary());
                                    if let Some(tuner_ref) = target {
                                        match eq_cmd.command_id {
                                            CMD_TUNE_START => {
                                                info!(
                                                    "Tuner: tune requested by client ({}{})",
                                                    tuner_ref.label(),
                                                    active_pos
                                                        .map(|p| format!(", amplitec_pos={}", p))
                                                        .unwrap_or_default()
                                                );
                                                tuner_ref.send_command(crate::tuner::TunerCmd::StartTune);
                                            }
                                            CMD_TUNE_ABORT => {
                                                info!("Tuner: abort requested by client");
                                                tuner_ref.send_command(crate::tuner::TunerCmd::AbortTune);
                                            }
                                            _ => {
                                                debug!("Unknown tuner command: 0x{:02X}", eq_cmd.command_id);
                                            }
                                        }
                                    }
                                }
                                DeviceType::SpeExpert => {
                                    if let Some(ref spe_ref) = self.spe {
                                        // `active_pa` is a permission, not just a
                                        // preference: a second PA in Operate writes the
                                        // Thetis drive too, and the two then flip it
                                        // between their own values. Refusing here is
                                        // kinder than switching it on and taking it
                                        // back a second later.
                                        let operate_blocked = eq_cmd.command_id == CMD_SPE_OPERATE
                                            && spe_ref.status().state != 2
                                            && !crate::pa_policy::operate_allowed(
                                                crate::pa_policy::ActiveMark {
                                                    marked: self.active_pa_shared.as_ref()
                                                        .map(|a| a.load(Ordering::Relaxed)).unwrap_or(0),
                                                    spe_present: self.spe.is_some(),
                                                    rf2k_present: self.rf2k.is_some(),
                                                }
                                                .effective(),
                                                crate::pa_policy::PA_SPE,
                                            );
                                        let spe_cmd = match eq_cmd.command_id {
                                            CMD_SPE_OPERATE if operate_blocked => {
                                                warn!("SPE Expert: Operate refused - the RF2K-S is the active PA");
                                                None
                                            }
                                            CMD_SPE_OPERATE => Some(crate::spe_expert::SpeCmd::ToggleOperate),
                                            CMD_SPE_TUNE => Some(crate::spe_expert::SpeCmd::Tune),
                                            CMD_SPE_ANTENNA => Some(crate::spe_expert::SpeCmd::CycleAntenna),
                                            CMD_SPE_INPUT => Some(crate::spe_expert::SpeCmd::CycleInput),
                                            CMD_SPE_POWER => Some(crate::spe_expert::SpeCmd::CyclePower),
                                            CMD_SPE_BAND_UP => Some(crate::spe_expert::SpeCmd::BandUp),
                                            CMD_SPE_BAND_DOWN => Some(crate::spe_expert::SpeCmd::BandDown),
                                            CMD_SPE_OFF => Some(crate::spe_expert::SpeCmd::PowerOff),
                                            CMD_SPE_POWER_ON => Some(crate::spe_expert::SpeCmd::PowerOn),
                                            CMD_SPE_DRIVE_DOWN => Some(crate::spe_expert::SpeCmd::DriveDown),
                                            CMD_SPE_DRIVE_UP => Some(crate::spe_expert::SpeCmd::DriveUp),
                                            _ => {
                                                debug!("Unknown SPE command: 0x{:02X}", eq_cmd.command_id);
                                                None
                                            }
                                        };
                                        if let Some(cmd) = spe_cmd {
                                            info!("SPE Expert: command 0x{:02X} from client", eq_cmd.command_id);
                                            spe_ref.send_command(cmd);
                                        }
                                    }
                                }
                                DeviceType::Rf2k => {
                                    if let Some(ref rf2k_ref) = self.rf2k {
                                        let rf2k_cmd = match eq_cmd.command_id {
                                            CMD_RF2K_OPERATE => {
                                                // Toggle: data[0]=1 for Operate, 0 for Standby
                                                let to_operate = eq_cmd.data.first().copied().unwrap_or(1) != 0;
                                                // Same permission as on the SPE side: only the
                                                // active PA may amplify. Standby is always allowed.
                                                let allowed = !to_operate
                                                    || crate::pa_policy::operate_allowed(
                                                        crate::pa_policy::ActiveMark {
                                                            marked: self.active_pa_shared.as_ref()
                                                                .map(|a| a.load(Ordering::Relaxed)).unwrap_or(0),
                                                            spe_present: self.spe.is_some(),
                                                            rf2k_present: self.rf2k.is_some(),
                                                        }
                                                        .effective(),
                                                        crate::pa_policy::PA_RF2K,
                                                    );
                                                if !allowed {
                                                    warn!("RF2K-S: Operate refused - the SPE Expert is the active PA");
                                                    None
                                                } else {
                                                    Some(crate::rf2k::Rf2kCmd::SetOperate(to_operate))
                                                }
                                            }
                                            CMD_RF2K_TUNE => Some(crate::rf2k::Rf2kCmd::Tune),
                                            CMD_RF2K_ANT1 => Some(crate::rf2k::Rf2kCmd::SetAntenna { antenna_type: 0, number: 1 }),
                                            CMD_RF2K_ANT2 => Some(crate::rf2k::Rf2kCmd::SetAntenna { antenna_type: 0, number: 2 }),
                                            CMD_RF2K_ANT3 => Some(crate::rf2k::Rf2kCmd::SetAntenna { antenna_type: 0, number: 3 }),
                                            CMD_RF2K_ANT4 => Some(crate::rf2k::Rf2kCmd::SetAntenna { antenna_type: 0, number: 4 }),
                                            CMD_RF2K_ANT_EXT => Some(crate::rf2k::Rf2kCmd::SetAntenna { antenna_type: 1, number: 1 }),
                                            CMD_RF2K_ERROR_RESET => Some(crate::rf2k::Rf2kCmd::ErrorReset),
                                            CMD_RF2K_CLOSE => Some(crate::rf2k::Rf2kCmd::Close),
                                            CMD_RF2K_TUNER_MODE => {
                                                let mode = eq_cmd.data.first().copied().unwrap_or(0);
                                                Some(crate::rf2k::Rf2kCmd::TunerMode(mode))
                                            }
                                            CMD_RF2K_TUNER_BYPASS => {
                                                let on = eq_cmd.data.first().copied().unwrap_or(1) != 0;
                                                Some(crate::rf2k::Rf2kCmd::TunerBypass(on))
                                            }
                                            CMD_RF2K_TUNER_RESET => Some(crate::rf2k::Rf2kCmd::TunerReset),
                                            CMD_RF2K_TUNER_STORE => Some(crate::rf2k::Rf2kCmd::TunerStore),
                                            CMD_RF2K_TUNER_L_UP => Some(crate::rf2k::Rf2kCmd::TunerLUp),
                                            CMD_RF2K_TUNER_L_DOWN => Some(crate::rf2k::Rf2kCmd::TunerLDown),
                                            CMD_RF2K_TUNER_C_UP => Some(crate::rf2k::Rf2kCmd::TunerCUp),
                                            CMD_RF2K_TUNER_C_DOWN => Some(crate::rf2k::Rf2kCmd::TunerCDown),
                                            CMD_RF2K_TUNER_K => Some(crate::rf2k::Rf2kCmd::TunerK),
                                            CMD_RF2K_DRIVE_UP => Some(crate::rf2k::Rf2kCmd::DriveUp),
                                            CMD_RF2K_DRIVE_DOWN => Some(crate::rf2k::Rf2kCmd::DriveDown),
                                            CMD_RF2K_SET_HIGH_POWER => {
                                                let v = eq_cmd.data.first().copied().unwrap_or(0) != 0;
                                                Some(crate::rf2k::Rf2kCmd::SetHighPower(v))
                                            }
                                            CMD_RF2K_SET_TUNER_6M => {
                                                let v = eq_cmd.data.first().copied().unwrap_or(0) != 0;
                                                Some(crate::rf2k::Rf2kCmd::SetTuner6m(v))
                                            }
                                            CMD_RF2K_SET_BAND_GAP => {
                                                let v = eq_cmd.data.first().copied().unwrap_or(0) != 0;
                                                Some(crate::rf2k::Rf2kCmd::SetBandGap(v))
                                            }
                                            CMD_RF2K_FRQ_DELAY_UP => Some(crate::rf2k::Rf2kCmd::FrqDelayUp),
                                            CMD_RF2K_FRQ_DELAY_DOWN => Some(crate::rf2k::Rf2kCmd::FrqDelayDown),
                                            CMD_RF2K_AUTOTUNE_THRESH_UP => Some(crate::rf2k::Rf2kCmd::AutotuneThresholdUp),
                                            CMD_RF2K_AUTOTUNE_THRESH_DOWN => Some(crate::rf2k::Rf2kCmd::AutotuneThresholdDown),
                                            CMD_RF2K_DAC_ALC_UP => Some(crate::rf2k::Rf2kCmd::DacAlcUp),
                                            CMD_RF2K_DAC_ALC_DOWN => Some(crate::rf2k::Rf2kCmd::DacAlcDown),
                                            CMD_RF2K_ZERO_FRAM => Some(crate::rf2k::Rf2kCmd::ZeroFRAM),
                                            // Row, band and value from the one place
                                            // the client encodes with (`rf2k_drive`,
                                            // build 205-207).
                                            c if sdr_remote_core::rf2k_drive::category_of(c).is_some() => {
                                                sdr_remote_core::rf2k_drive::decode(c, &eq_cmd.data).map(crate::rf2k::Rf2kCmd::SetDriveConfig)
                                            }
                                            _ => {
                                                debug!("Unknown RF2K command: 0x{:02X}", eq_cmd.command_id);
                                                None
                                            }
                                        };
                                        if let Some(cmd) = rf2k_cmd {
                                            info!("RF2K-S: command 0x{:02X} from client", eq_cmd.command_id);
                                            rf2k_ref.send_command(cmd);
                                        }
                                    }
                                }
                                DeviceType::UltraBeam => {
                                    if let Some(ref ub_ref) = self.ultrabeam {
                                        let ub_cmd = match eq_cmd.command_id {
                                            CMD_UB_RETRACT => {
                                                Some(crate::ultrabeam::UltraBeamCmd::Retract)
                                            }
                                            CMD_UB_SET_FREQ => {
                                                if eq_cmd.data.len() >= 3 {
                                                    let khz = u16::from_le_bytes([eq_cmd.data[0], eq_cmd.data[1]]);
                                                    let direction = eq_cmd.data[2];
                                                    Some(crate::ultrabeam::UltraBeamCmd::SetFrequency { khz, direction })
                                                } else { None }
                                            }
                                            CMD_UB_READ_ELEMENTS => {
                                                Some(crate::ultrabeam::UltraBeamCmd::ReadElements)
                                            }
                                            CMD_UB_MODIFY_ELEMENT => {
                                                if eq_cmd.data.len() >= 3 && eq_cmd.data[0] < 6 {
                                                    let index = eq_cmd.data[0];
                                                    let length_mm = u16::from_le_bytes([eq_cmd.data[1], eq_cmd.data[2]]);
                                                    Some(crate::ultrabeam::UltraBeamCmd::ModifyElement { index, length_mm })
                                                } else { None }
                                            }
                                            _ => {
                                                debug!("Unknown UltraBeam command: 0x{:02X}", eq_cmd.command_id);
                                                None
                                            }
                                        };
                                        if let Some(cmd) = ub_cmd {
                                            info!("UltraBeam: command 0x{:02X} from client", eq_cmd.command_id);
                                            ub_ref.send_command(cmd);
                                        }
                                    }
                                }
                                DeviceType::RemoteServer => {
                                    if eq_cmd.command_id == sdr_remote_core::protocol::CMD_SERVER_REBOOT {
                                        info!("Client requested remote reboot");
                                        std::thread::spawn(|| {
                                            match std::process::Command::new("C:\\Windows\\System32\\cmd.exe")
                                                .args(["/c", "schtasks", "/run", "/tn", "ThetisLinkReboot"])
                                                .output()
                                            {
                                                Ok(out) => {
                                                    let stdout = String::from_utf8_lossy(&out.stdout);
                                                    let stderr = String::from_utf8_lossy(&out.stderr);
                                                    info!("schtasks exit={} stdout={} stderr={}", out.status, stdout.trim(), stderr.trim());
                                                }
                                                Err(e) => log::error!("Failed to run schtasks: {}", e),
                                            }
                                        });
                                    } else if eq_cmd.command_id == sdr_remote_core::protocol::CMD_SERVER_SHUTDOWN {
                                        info!("Client requested remote shutdown");
                                        std::thread::spawn(|| {
                                            match std::process::Command::new("C:\\Windows\\System32\\shutdown.exe")
                                                .args(["/s", "/t", "5", "/f"])
                                                .output()
                                            {
                                                Ok(out) => {
                                                    let stdout = String::from_utf8_lossy(&out.stdout);
                                                    let stderr = String::from_utf8_lossy(&out.stderr);
                                                    info!("shutdown exit={} stdout={} stderr={}", out.status, stdout.trim(), stderr.trim());
                                                }
                                                Err(e) => log::error!("Failed to run shutdown: {}", e),
                                            }
                                        });
                                    }
                                }
                                DeviceType::Rotor => {
                                    if let Some(ref rotor_ref) = self.rotor {
                                        let rotor_cmd = match eq_cmd.command_id {
                                            CMD_ROTOR_GOTO => {
                                                if eq_cmd.data.len() >= 2 {
                                                    let angle = u16::from_le_bytes([eq_cmd.data[0], eq_cmd.data[1]]);
                                                    Some(crate::rotor::RotorCmd::GoTo(angle))
                                                } else { None }
                                            }
                                            CMD_ROTOR_STOP => Some(crate::rotor::RotorCmd::Stop),
                                            CMD_ROTOR_CW => Some(crate::rotor::RotorCmd::Cw),
                                            CMD_ROTOR_CCW => Some(crate::rotor::RotorCmd::Ccw),
                                            _ => {
                                                debug!("Unknown Rotor command: 0x{:02X}", eq_cmd.command_id);
                                                None
                                            }
                                        };
                                        if let Some(cmd) = rotor_cmd {
                                            info!("Rotor: command 0x{:02X} from client", eq_cmd.command_id);
                                            rotor_ref.send_command(cmd);
                                        }
                                    }
                                }
                            }
                        }
                        Packet::HeartbeatAck(_) | Packet::PttDenied { .. } => {}
                        // RX2 packets: client -> server (frequency, mode set)
                        Packet::FrequencyRx2(freq_pkt) => {
                            let mut ptt = self.ptt.lock().await;
                            ptt.set_vfo_b_freq(freq_pkt.frequency_hz).await;
                        }
                        Packet::ModeRx2(mode_pkt) => {
                            let mut ptt = self.ptt.lock().await;
                            ptt.set_vfo_b_mode(mode_pkt.mode).await;
                        }
                        // RX2 packets that are server->client only (ignore if received)
                        Packet::AudioRx2(_) | Packet::AudioBinR(_) | Packet::SmeterRx2(_)
                        | Packet::SpectrumRx2(_) | Packet::FullSpectrumRx2(_)
                        | Packet::SmeterSig(_) | Packet::SmeterMaxBin(_)
                        | Packet::SmeterRx2Sig(_) | Packet::SmeterRx2MaxBin(_)
                        | Packet::AudioVrx(_)
                        | Packet::SpectrumVrx1(_) | Packet::SpectrumVrx2(_)
                        | Packet::FrequencyVrxActual(_) | Packet::TxFilterBand(_) => {}
                        // Server->client only, ignore if received
                        Packet::Spot(_) | Packet::TxProfiles(_) | Packet::YaesuState(_)
                        | Packet::AmplitecPowerTable(_)
                        | Packet::AuthChallenge(_) | Packet::AuthResponse { .. } | Packet::AuthResult { .. }
                        | Packet::TotpChallenge | Packet::TotpResponse(_) => {}
                        // Sent by this server, never received by it.
                        Packet::ServerReportPart(..) => {}
                        Packet::ServerReportRequest => {
                            // Only an authenticated client gets here: everything
                            // else was dropped at the gate above. That is the
                            // whole authorisation this needs - somebody who can
                            // key the transmitter can read the log about it.
                            // The radios' own lists travel with it. For an FTX-1
                            // that list IS where its memory tones live, so a
                            // report without it cannot answer "what does the
                            // server think channel 5 holds" - which is the
                            // question a tone that went missing turns into.
                            let lists = [
                                self.yaesu.as_ref().map(|y| (0u8, y.model_code(), y.status().last_memory_blob.clone())),
                                self.yaesu2.as_ref().map(|y| (1u8, y.model_code(), y.status().last_memory_blob.clone())),
                            ];
                            send_own_report(self.socket.clone(), addr, lists);
                        }
                        Packet::YaesuMemoryData(text) => {
                            if let Some(rest) = text.strip_prefix("SETMENU:") {
                                // "SETMENU:<key>:<value>". The key's shape follows
                                // the radio, not the slot - see `set_menu_entry`.
                                if let Some(ref yaesu) = yaesu {
                                    if let Some((key, value)) = rest.split_once(':') {
                                        match yaesu.set_menu_entry(key, value) {
                                            Ok(what) => info!("Client {} [radio0] set {} = {}", addr, what, value),
                                            Err(why) => warn!("Client {} [radio0] EX set refused: {}", addr, why),
                                        }
                                    }
                                }
                            } else {
                                info!("Received Yaesu memory data from client ({}B)", text.len());
                                if let Some(text) = yaesu_write.data(addr, text) {
                                    if let Some(ref yaesu) = yaesu {
                                        info!("Client {} writing Yaesu memories (deferred - data arrived after control)", addr);
                                        yaesu.send_command(crate::yaesu::YaesuCmd::WriteAllMemories(text));
                                    }
                                }
                            }
                        }
                        // Yaesu TX audio, with its PTT (build 282): one block for
                        // both radios, `radio_audio!`.
                        Packet::AudioYaesu(pkt) => {
                            radio_audio!(pkt, addr, 0, crate::session::TxTarget::Yaesu1, yaesu_tx_packet_tx, crate::tx_arrivals::TxStream::Radio1);
                        }
                        Packet::FrequencyYaesu(freq_pkt) => {
                            if let Some(ref yaesu) = yaesu {
                                // Always forward: SetFreqA decides for itself. In memory mode
                                // it slides via the memory-escape (MA;MD;FA) to VFO, in
                                // VFO it sends a bare FA. (Previously a guard on
                                // vfo_select==1 blocked here, which actually held back the
                                // desired memory->VFO escape - the escape then sat as dead code in SetFreqA.)
                                yaesu.send_command(crate::yaesu::YaesuCmd::SetFreqA(freq_pkt.frequency_hz));
                            }
                        }
                        Packet::FrequencyVrx(pkt) => {
                            // pkt.vrx_id: 0=VRX1, 1=VRX2, anything else ignored.
                            let id = match pkt.vrx_id {
                                0 | 1 => pkt.vrx_id,
                                _ => { continue; }
                            };
                            let ctl = self.vrx_mgr.lock().unwrap().control(addr, id);
                            ctl.lock().unwrap().target_freq_hz = pkt.frequency_hz;
                            info!("Client {} VRX{} freq: {} Hz", addr, id + 1, pkt.frequency_hz);
                        }
                        Packet::AudioMultiCh(_) => {} // server->client only, ignore
                        // Dual-radio slot 1 (Option B-prime) - mirror of AudioYaesu/FrequencyYaesu.
                        Packet::AudioYaesu2(pkt) => {
                            radio_audio!(pkt, addr, 1, crate::session::TxTarget::Yaesu2, yaesu2_tx_packet_tx, crate::tx_arrivals::TxStream::Radio2);
                        }
                        Packet::FrequencyYaesu2(freq_pkt) => {
                            if let Some(ref yaesu) = yaesu2 {
                                // Always forward: SetFreqA handles memory-escape vs bare FA
                                // itself (see FrequencyYaesu above).
                                yaesu.send_command(crate::yaesu::YaesuCmd::SetFreqA(freq_pkt.frequency_hz));
                            }
                        }
                        // Typed Yaesu DSP/function control (PATCH-yaesu-extra-controls).
                        // Route to the addressed slot. Feedback (YaesuFeaturePacket) is
                        // value-change-only and unknown-type-safe (old server/client
                        // ignore it nonfatal), so no separate opt-in set is needed.
                        Packet::YaesuControl(pkt) => {
                            let target = if pkt.slot == 1 { &yaesu2 } else { &yaesu };
                            if let Some(ref y) = target {
                                info!("Client {} [radio{}] Yaesu control {} = {}", addr, pkt.slot, pkt.control, pkt.value);
                                y.send_command(crate::yaesu::YaesuCmd::SetFeature(pkt.control, pkt.value));
                            }
                        }
                        // Server->client only, ignore if received.
                        Packet::YaesuState2(_) | Packet::RadioInfo { .. }
                        | Packet::YaesuFeature(_) | Packet::YaesuPresence(_) => {}
                        // Slot-1 memory write (Phase B): client sends the tab text.
                        // Fire directly if the write control already arrived (armed), otherwise
                        // latch until Yaesu2WriteMemories (UDP-reorder-safe, like slot 0).
                        Packet::YaesuMemoryData2(text) => {
                            if let Some(rest) = text.strip_prefix("SETMENU:") {
                                // Same handler shape as slot 0: the radio decides
                                // which EX dialect its key is in.
                                if let Some(ref yaesu) = yaesu2 {
                                    if let Some((key, value)) = rest.split_once(':') {
                                        match yaesu.set_menu_entry(key, value) {
                                            Ok(what) => info!("Client {} [radio1] set {} = {}", addr, what, value),
                                            Err(why) => warn!("Client {} [radio1] EX set refused: {}", addr, why),
                                        }
                                    }
                                }
                            } else if let Some(text) = yaesu2_write.data(addr, text) {
                                if let Some(ref yaesu) = yaesu2 {
                                    info!("Client {} [radio1] writing memories (deferred)", addr);
                                    yaesu.send_command(crate::yaesu::YaesuCmd::WriteAllMemories(text));
                                }
                            }
                        }
                        Packet::Disconnect => {
                            info!("Client {} disconnected", addr);
                            denied.attempt_over(&addr);
                            // A memory write it left half-done goes with it. Not
                            // strictly needed to keep the radio safe - a trigger
                            // only fires on data from the same client - but a list
                            // of somebody who has left is not state worth keeping.
                            yaesu_write.forget(addr);
                            yaesu2_write.forget(addr);
                            let released = self.session.lock().await.remove(addr);
                            // The ownership went above; the transmitter itself goes
                            // here, now. It used to be left to the watchdogs - two
                            // seconds of carrier after a clean goodbye (field test,
                            // 2026-09-17 14:47:04 -> 14:47:06), and a Yaesu until the
                            // orphan check came round.
                            for target in released {
                                info!("Client {} said goodbye while keying {} - releasing PTT now", addr, target.radio_name());
                                if target == crate::session::TxTarget::Thetis {
                                    // What is still in the playout buffer is the last of
                                    // this client's audio, with its PTT bit set - the
                                    // client clears its flags after sending. Left there,
                                    // playout keyed the radio again 37 ms after this
                                    // release (field test, 2026-09-17 15:02:55). It goes
                                    // with the client.
                                    forget_thetis_holder!();
                                }
                                self.release_transmitter(target).await;
                            }
                            // And its CW key, if it was down - on whichever radio its
                            // run keyed (step 6: a Yaesu has no keying watchdog of its
                            // own; its RTS stays high until someone lowers it).
                            let up = crate::cw_playout::lock(&self.cw).release(addr, Instant::now());
                            if let Some(up) = up {
                                info!("Client {} said goodbye with the CW key down - key up now", addr);
                                let _ = self.cw_due.send(up);
                            }
                            // TL2-1 ctun-auto-recenter: recompute effective_zoom +
                            // strictest-tick after disconnect - the last unchecked client may
                            // have just left, allowing zoom-min to go from 2.0 -> 1.0.
                            let new_eff_rx1 = self.session.lock().await.effective_zoom_rx1();
                            let new_eff_rx2 = self.session.lock().await.effective_zoom_rx2();
                            let mut p = self.ptt.lock().await;
                            p.tci.effective_zoom_rx1_cache = new_eff_rx1;
                            p.tci.effective_zoom_rx2_cache = new_eff_rx2;
                            // No trigger_eval on disconnect - the next vfo event or
                            // remaining-client-zoom-change will trigger eval.
                        }
                        Packet::CwEdges(pkt) => {
                            // Keying is transmitting: the same gates as the PTT,
                            // before anything reaches the playout.
                            let now_ms = self.server_start.elapsed().as_millis() as u64;
                            // Old news first, before anything is asked or taken for it:
                            // a repeat of a run that is over must not take the lock or
                            // key MOX (see CwPlayout::has_news).
                            if !crate::cw_playout::lock(&self.cw).has_news(addr, &pkt) {
                                continue;
                            }
                            // The gates. Every refusal leaves through the one exit below,
                            // so what is refused is seen: its edges ride along in the
                            // next packet and must not play then (CwPlayout::discard).
                            let admitted: Option<bool> = 'gate: {
                                // A run this packet cannot join - another client's, or this
                                // client's on another radio - refuses it before any lock is
                                // taken or MOX raised; the ingest would refuse it anyway, and
                                // what the gates took for it would be left behind.
                                if crate::cw_playout::lock(&self.cw).busy_for(addr, pkt.target) {
                                    if self.cw_denied.should_send(addr, now_ms) {
                                        warn!("Client {} keys CW while a run of another client or on another radio is playing - refused", addr);
                                        let mut buf = [0u8; PttDeniedPacket::SIZE];
                                        PttDeniedPacket::serialize(&mut buf, match pkt.target { 1 => DeniedTarget::Radio1, 2 => DeniedTarget::Radio2, _ => DeniedTarget::Thetis }, Denial::Refused);
                                        let _ = self.socket.send_to(&buf, addr).await;
                                    }
                                    break 'gate None;
                                }
                                if pkt.target != 0 {
                                    // A Yaesu (CW design step 6): keyed through PC KEYING on
                                    // RTS of its second port. No MOX of ours - the radio does
                                    // its own semi break-in - so nothing to rise for.
                                    match self.admit_yaesu_cw(addr, pkt.target).await {
                                        Ok(()) => {
                                            self.cw_denied.attempt_over(&addr);
                                            break 'gate Some(false);
                                        }
                                        Err(why) => {
                                            if self.cw_denied.should_send(addr, now_ms) {
                                                warn!("Client {} keys CW to Yaesu slot {} but {} - refused", addr, pkt.target.saturating_sub(1), why);
                                                let denial = self.session.lock().await.acquire_refusal().denial;
                                                let mut buf = [0u8; PttDeniedPacket::SIZE];
                                                PttDeniedPacket::serialize(&mut buf, if pkt.target == 1 { DeniedTarget::Radio1 } else { DeniedTarget::Radio2 }, denial);
                                                let _ = self.socket.send_to(&buf, addr).await;
                                            }
                                            break 'gate None;
                                        }
                                    }
                                }
                                let (blocked, mode, transmitting) = {
                                    let p = self.ptt.lock().await;
                                    (p.tx_blocked(), p.vfo_a_mode(), p.is_transmitting())
                                };
                                let mut thetis_held_before = false;
                                // Closed first: a closed server also has the
                                // receive-only gate shut, and that is not the reason.
                                let closed = self.session.lock().await.acquire_refusal();
                                let refuse = if closed.denial == Denial::Closed {
                                    Some(closed.why)
                                } else if blocked {
                                    Some("the Amplitec position is receive-only")
                                } else if !crate::tci_parser::is_cw_mode(mode) {
                                    Some("Thetis is not in a CW mode")
                                } else {
                                    let mut s = self.session.lock().await;
                                    forget_thetis_holder_if_outlived!(s.tx_holder(crate::session::TxTarget::Thetis));
                                    match s.acquire_for_attempt(crate::session::TxTarget::Thetis, addr) {
                                        None => Some(s.acquire_refusal().why),
                                        Some(held) => {
                                            thetis_held_before = held;
                                            None
                                        }
                                    }
                                };
                                if let Some(why) = refuse {
                                    if self.cw_denied.should_send(addr, now_ms) {
                                        warn!("Client {} keys CW but {} - refused", addr, why);
                                        let mut buf = [0u8; PttDeniedPacket::SIZE];
                                        PttDeniedPacket::serialize(&mut buf, DeniedTarget::Thetis, closed.denial);
                                        let _ = self.socket.send_to(&buf, addr).await;
                                    }
                                    break 'gate None;
                                }
                                self.cw_denied.attempt_over(&addr);
                                // The server keys MOX for the run, through the PTT machine
                                // (see PttController::activate_for_cw for why); a run that
                                // finds MOX down waits for the rise before its first edge.
                                let starts_run = pkt.edges.iter().any(|e| e.run_start);
                                let mox_rise = !transmitting;
                                if starts_run || !transmitting {
                                    let mut p = self.ptt.lock().await;
                                    if !p.activate_for_cw().await {
                                        warn!("Client {} keys CW but Thetis could not be keyed - refused", addr);
                                        drop(p);
                                        // The lock goes back if it was taken for this attempt;
                                        // one this client held already stays its own.
                                        self.session.lock().await.give_back_attempt(crate::session::TxTarget::Thetis, addr, thetis_held_before);
                                        break 'gate None;
                                    }
                                }
                                Some(mox_rise)
                            };
                            let Some(mox_rise) = admitted else {
                                crate::cw_playout::lock(&self.cw).discard(addr, &pkt);
                                continue;
                            };
                            // A packet from the client is a sign of life from the client -
                            // for Thetis's watchdogs, which a Yaesu run has nothing to do with.
                            if pkt.target == 0 {
                                self.ptt.lock().await.record_ptt_packet();
                            }
                            let result = {
                                let mut g = crate::cw_playout::lock(&self.cw);
                                g.set_hang(pkt.hang_ms);
                                g.ingest(addr, &pkt, Instant::now(), mox_rise)
                            };
                            self.cw.wake.notify_all();
                            if result.refused_other_owner {
                                warn!("Client {} keys CW while another client's run is playing - refused", addr);
                            }
                            if result.stale > 0 {
                                // Their own packets never arrived; they belong to a run
                                // that is over (see CwPlayout::ingest).
                                warn!("cw playout: {} stale edge(s) before a run start dropped", result.stale);
                            }
                            for (seq, ms) in &result.late {
                                // The one place jitter still reaches the signal.
                                warn!("cw playout: late edge seq={} by {} ms", seq, ms);
                            }
                        }
                        Packet::Control(ctrl) => {
                            // A client sends its whole state right after it
                            // connects - forty controls restating what it
                            // arrived with. That is one event, not forty, so
                            // during the opening seconds these go to debug.
                            let opening = {
                                let sess = self.session.lock().await;
                                sess.opening_burst(addr)
                            };
                            let mut ptt = self.ptt.lock().await;
                            match ctrl.control_id {
                                ControlId::Rx1AfGain => {
                                    let val = ctrl.value.min(100);
                                    // rx_volume via TCI (stock v2.10.3.13+).
                                    // Scale: 0..100 % -> -60..0 dB (matches parser in tci.rs RxVolume handler).
                                    let db = ((val as i32 - 100) * 60) / 100;
                                    let cmd = format!("rx_volume:0,0,{};", db);
                                    ptt.send_cat(&cmd).await;
                                    // No optimistic state update - Thetis echoes rx_volume back
                                    // and the parser updates rx_af_gain via the notification path.
                                }
                                ControlId::PowerOnOff => {
                                    if ctrl.value == 2 {
                                        // Shutdown Thetis via TCI (v2.10.3.13+)
                                        ctrl_log!(opening, "Client {} requested Thetis shutdown", addr);
                                        // Not send_cat: the controller has to
                                        // remember it asked, or the next
                                        // power-on mistakes a closing Thetis
                                        // for a running one (#67).
                                        ptt.request_thetis_shutdown().await;
                                    } else {
                                        // Which client: two front ends with the autostart
                                        // on launched Thetis twice, and the log did not say
                                        // who (field test 2026-09-19).
                                        ctrl_log!(opening, "Client {} asks for power {}", addr, if ctrl.value != 0 { "on" } else { "off" });
                                        ptt.set_power(ctrl.value != 0).await;
                                    }
                                }
                                ControlId::TxProfile => {
                                    ptt.set_tx_profile(ctrl.value.min(99) as u8).await;
                                }
                                ControlId::ThetisTxeq => {
                                    // Thetis TX-EQ bypass during WAV playback to the
                                    // main radio (over TCI run_cat_ex). value 0 = start
                                    // bypass: query the current ZZET state, save it and
                                    // turn TX-EQ off. value 1 = end: restore exactly the
                                    // saved state - just like Thetis' own playback.
                                    if ctrl.value != 0 {
                                        ptt.tci.thetis_txeq_bypass_end().await;
                                    } else {
                                        ptt.tci.thetis_txeq_bypass_begin().await;
                                    }
                                }
                                ControlId::NoiseReduction => {
                                    ptt.set_nr(ctrl.value.min(4) as u8).await;
                                }
                                ControlId::AutoNotchFilter => {
                                    ptt.set_anf(ctrl.value != 0).await;
                                }
                                ControlId::DriveLevel => {
                                    ptt.set_drive(ctrl.value.min(100) as u8).await;
                                }
                                ControlId::SpectrumEnable => {
                                    let enabled = ctrl.value != 0;
                                    self.session.lock().await.set_spectrum_enabled(addr, enabled);
                                    self.refresh_spectrum_processors().await;
                                    ctrl_log!(opening, "Client {} spectrum: {}", addr, if enabled { "ON" } else { "OFF" });
                                }
                                ControlId::SpectrumFps => {
                                    let fps = (ctrl.value as u8).clamp(5, 30);
                                    self.session.lock().await.set_spectrum_fps(addr, fps);
                                    let max_fps = self.session.lock().await.spectrum_max_fps();
                                    self.spectrum.lock().await.set_fps(max_fps);
                                    ctrl_log!(opening, "Client {} spectrum fps: {}", addr, fps);
                                }
                                ControlId::SpectrumZoom => {
                                    let zoom = ctrl.value as f32 / 10.0;
                                    let prev_eff = self.session.lock().await.effective_zoom_rx1();
                                    self.session.lock().await.set_spectrum_zoom(addr, zoom);
                                    let new_eff = self.session.lock().await.effective_zoom_rx1();
                                    info!("TCI: client {} rx1 zoom={:.1}x; effective_rx1={:?} (was {:?})",
                                          addr, zoom, new_eff, prev_eff);
                                    // TL2-1 ctun-auto-recenter: update cached effective_zoom + trigger eval.
                                    // Use the existing outer `ptt` guard (line ~1572
                                    // `let mut ptt = self.ptt.lock().await;`) - the Tokio mutex is not
                                    // reentrant, nested self.ptt.lock() = deadlock.
                                    ptt.tci.effective_zoom_rx1_cache = new_eff;
                                    ptt.tci.trigger_eval_and_act_rx1(new_eff).await;
                                }
                                ControlId::SpectrumPan => {
                                    let pan = ctrl.value as f32 / 10000.0 - 0.5;
                                    self.session.lock().await.set_spectrum_pan(addr, pan);
                                    // Logged like the zoom beside it. Without this a
                                    // report about a pan that does nothing cannot say
                                    // whether the value ever arrived - which is exactly
                                    // where one such report ran out of evidence.
                                    ctrl_log!(opening, "Client {} rx1 spectrum pan: {:+.4}", addr, pan);
                                }
                                ControlId::FilterLow => {
                                    // Buffer low edge; send combined with high edge
                                    pending_filter_low = Some(ctrl.value as i16 as i32);
                                }
                                ControlId::FilterHigh => {
                                    let high = ctrl.value as i16 as i32;
                                    let low = pending_filter_low.take()
                                        .unwrap_or(ptt.filter_low_hz());
                                    // Only edges that fit the mode Thetis has or was
                                    // just asked for (build 244): a late repeat, a
                                    // stale low edge, a lost mode packet or a mode
                                    // change dropped during TX put another mode's
                                    // filter in.
                                    let mode = crate::tci::mode_for_filter(ptt.tci.asked_mode_a, ptt.vfo_a_mode(), Instant::now());
                                    if sdr_remote_core::filter_fit::edges_fit_mode(mode, low, high) {
                                        ctrl_log!(opening, "Client {} filter: {} .. {} Hz", addr, low, high);
                                        ptt.set_filter(low, high).await;
                                    } else {
                                        // Named in advance (`logregels-build244.md`).
                                        info!("Client {} filter: {} .. {} Hz does not fit mode {} - not sent to Thetis", addr, low, high, mode);
                                    }
                                }
                                ControlId::TxFilterLow => {
                                    // Buffer low edge; apply combined with high edge.
                                    pending_tx_filter_low = Some(ctrl.value as i16 as i32);
                                }
                                ControlId::TxFilterHigh => {
                                    let high = ctrl.value as i16 as i32;
                                    let low = pending_tx_filter_low.take()
                                        .unwrap_or(ptt.tx_filter_low_hz());
                                    // Defensive clamp/sort at the server boundary - the TX
                                    // path is sensitive; an old/corrupt client could send
                                    // out-of-range or reversed edges. Bound 0..=8000
                                    // (TX audio is 16 kS/s -> Nyquist 8 kHz), low<=high.
                                    let lo = low.clamp(0, 8000);
                                    let hi = high.clamp(0, 8000);
                                    let (lo, hi) = (lo.min(hi), lo.max(hi));
                                    ctrl_log!(opening, "Client {} TX filter: {} .. {} Hz", addr, lo, hi);
                                    ptt.set_tx_filter_band(lo, hi).await;
                                }
                                ControlId::ThetisStarting => {} // server->client only
                                ControlId::PaDriveWarning => {} // server->client only
                                ControlId::DdcCentreRx1 | ControlId::DdcCentreRx2 => {} // server->client only
                                ControlId::Rx1Enable => {
                                    let enabled = ctrl.value != 0;
                                    self.session.lock().await.set_rx1_enabled(addr, enabled);
                                    log::info!("RX1 audio subscription {} for {}", if enabled { "ON" } else { "OFF" }, addr);
                                }
                                ControlId::Rx2Enable => {
                                    let enabled = ctrl.value != 0;
                                    self.session.lock().await.set_rx2_enabled(addr, enabled);
                                    // Send current VFO-B freq/mode to newly enabled RX2 client
                                    if enabled {
                                        let vfo_b = ptt.vfo_b_freq();
                                        let mode_b = ptt.vfo_b_mode();
                                        if vfo_b != 0 {
                                            let pkt = FrequencyPacket { frequency_hz: vfo_b };
                                            let mut buf = [0u8; FrequencyPacket::SIZE];
                                            pkt.serialize_as_type(&mut buf, PacketType::FrequencyRx2);
                                            let _ = self.socket.try_send_to(&buf, addr);
                                        }
                                        let pkt = ModePacket { mode: mode_b };
                                        let mut buf = [0u8; ModePacket::SIZE];
                                        pkt.serialize_as_type(&mut buf, PacketType::ModeRx2);
                                        let _ = self.socket.try_send_to(&buf, addr);
                                    }
                                    ctrl_log!(opening, "Client {} RX2: {}", addr, if enabled { "ON" } else { "OFF" });
                                }
                                ControlId::Rx2AfGain => {
                                    let val = ctrl.value.min(100);
                                    ptt.set_rx2_af_gain(val as u8).await;
                                }
                                ControlId::Rx2SpectrumEnable => {
                                    let enabled = ctrl.value != 0;
                                    self.session.lock().await.set_rx2_spectrum_enabled(addr, enabled);
                                    self.refresh_spectrum_processors().await;
                                    // Send the CURRENT VFO-B freq/mode along immediately (initial
                                    // snapshot, just as with Rx2Enable): a spectrum-without-audio
                                    // client needs the RX2 freq to center correctly, and the
                                    // change-gated broadcast otherwise only sends it on the next
                                    // freq change.
                                    if enabled {
                                        let vfo_b = ptt.vfo_b_freq();
                                        let mode_b = ptt.vfo_b_mode();
                                        if vfo_b != 0 {
                                            let pkt = FrequencyPacket { frequency_hz: vfo_b };
                                            let mut buf = [0u8; FrequencyPacket::SIZE];
                                            pkt.serialize_as_type(&mut buf, PacketType::FrequencyRx2);
                                            let _ = self.socket.try_send_to(&buf, addr);
                                        }
                                        let pkt = ModePacket { mode: mode_b };
                                        let mut buf = [0u8; ModePacket::SIZE];
                                        pkt.serialize_as_type(&mut buf, PacketType::ModeRx2);
                                        let _ = self.socket.try_send_to(&buf, addr);
                                    }
                                    ctrl_log!(opening, "Client {} RX2 spectrum: {}", addr, if enabled { "ON" } else { "OFF" });
                                }
                                ControlId::Rx2SpectrumFps => {
                                    let fps = (ctrl.value as u8).clamp(5, 30);
                                    self.session.lock().await.set_rx2_spectrum_fps(addr, fps);
                                }
                                ControlId::Rx2SpectrumZoom => {
                                    let zoom = ctrl.value as f32 / 10.0;
                                    let prev_eff = self.session.lock().await.effective_zoom_rx2();
                                    self.session.lock().await.set_rx2_spectrum_zoom(addr, zoom);
                                    let new_eff = self.session.lock().await.effective_zoom_rx2();
                                    info!("TCI: client {} rx2 zoom={:.1}x; effective_rx2={:?} (was {:?})",
                                          addr, zoom, new_eff, prev_eff);
                                    // Use the outer ptt guard (no nested lock).
                                    ptt.tci.effective_zoom_rx2_cache = new_eff;
                                    ptt.tci.trigger_eval_and_act_rx2(new_eff).await;
                                }
                                ControlId::AllowZoomBelow2x => {
                                    let allow = ctrl.value != 0;
                                    let prev_min = self.session.lock().await.server_enforced_zoom_min();
                                    self.session.lock().await.set_allow_zoom_below_2x(addr, allow);
                                    let new_min = self.session.lock().await.server_enforced_zoom_min();
                                    info!("TCI: client {} allow_zoom_below_2x={}; server-strictest zoom-min: {:.1}x -> {:.1}x",
                                          addr, allow, prev_min, new_min);
                                    // Checkbox toggle changes server_enforced_zoom_min -> effective_zoom
                                    // may change -> recompute cache + trigger eval for both RX.
                                    // No nested lock (outer ptt guard already available).
                                    let new_eff_rx1 = self.session.lock().await.effective_zoom_rx1();
                                    let new_eff_rx2 = self.session.lock().await.effective_zoom_rx2();
                                    ptt.tci.effective_zoom_rx1_cache = new_eff_rx1;
                                    ptt.tci.effective_zoom_rx2_cache = new_eff_rx2;
                                    ptt.tci.trigger_eval_and_act_rx1(new_eff_rx1).await;
                                    ptt.tci.trigger_eval_and_act_rx2(new_eff_rx2).await;
                                }
                                ControlId::SmeterSources => {
                                    let mask = ctrl.value;
                                    ctrl_log!(opening, "Client {} S-meter sources mask: 0x{:02x}", addr, mask);
                                    self.session.lock().await.set_smeter_sources(addr, mask);
                                }
                                ControlId::DxSpotsEnabled => {
                                    let enabled = ctrl.value != 0;
                                    ctrl_log!(opening, "Client {} DX spots: {}", addr, if enabled { "ON" } else { "OFF" });
                                    self.session.lock().await.set_dx_spots_enabled(addr, enabled);
                                }
                                ControlId::FullSpectrumEnabled => {
                                    let enabled = ctrl.value != 0;
                                    ctrl_log!(opening, "Client {} full-spectrum row: {}", addr, if enabled { "ON" } else { "OFF" });
                                    self.session.lock().await.set_full_spectrum_enabled(addr, enabled);
                                }
                                ControlId::ThetisWidebandAudio => {
                                    let on = ctrl.value != 0;
                                    ctrl_log!(opening, "Client {} Thetis wideband audio: {}", addr, if on { "ON" } else { "OFF" });
                                    self.session.lock().await.set_thetis_wideband(addr, on);
                                }
                                ControlId::Rx2SpectrumPan => {
                                    let pan = ctrl.value as f32 / 10000.0 - 0.5;
                                    self.session.lock().await.set_rx2_spectrum_pan(addr, pan);
                                    ctrl_log!(opening, "Client {} rx2 spectrum pan: {:+.4}", addr, pan);
                                }
                                ControlId::Rx2FilterLow => {
                                    pending_rx2_filter_low = Some(ctrl.value as i16 as i32);
                                }
                                ControlId::Rx2FilterHigh => {
                                    let high = ctrl.value as i16 as i32;
                                    let low = pending_rx2_filter_low.take()
                                        .unwrap_or(ptt.filter_rx2_low_hz());
                                    // As RX1 (build 244).
                                    let mode = crate::tci::mode_for_filter(ptt.tci.asked_mode_b, ptt.vfo_b_mode(), Instant::now());
                                    if sdr_remote_core::filter_fit::edges_fit_mode(mode, low, high) {
                                        ctrl_log!(opening, "Client {} RX2 filter: {} .. {} Hz", addr, low, high);
                                        ptt.set_rx2_filter(low, high).await;
                                    } else {
                                        // Named in advance (`logregels-build244.md`).
                                        info!("Client {} RX2 filter: {} .. {} Hz does not fit mode {} - not sent to Thetis", addr, low, high, mode);
                                    }
                                }
                                ControlId::VfoSync => {
                                    let enabled = ctrl.value != 0;
                                    self.session.lock().await.set_vfo_sync(addr, enabled);
                                    ctrl_log!(opening, "Client {} VFO sync: {}", addr, if enabled { "ON" } else { "OFF" });
                                    // Delay before sending ZZSY to let frequencies settle
                                    tokio::time::sleep(Duration::from_millis(200)).await;
                                    ptt.set_vfo_sync_thetis(enabled).await;
                                }
                                ControlId::MonitorOn => {
                                    let on = ctrl.value != 0;
                                    ptt.set_mon(on).await;
                                    ctrl_log!(opening, "Client {} MON: {}", addr, if on { "ON" } else { "OFF" });
                                }
                                ControlId::Rx2NoiseReduction => {
                                    ptt.set_rx2_nr(ctrl.value.min(4) as u8).await;
                                }
                                ControlId::Rx2AutoNotchFilter => {
                                    ptt.set_rx2_anf(ctrl.value != 0).await;
                                }
                                ControlId::DiversityAutoNull => {
                                    let is_ultra = ctrl.value == 2;
                                    let has_smartnull = ptt.has_cap("diversity_smartnull_ex");
                                    let has_fastsweep = ptt.has_cap("diversity_fastsweep_ex");
                                    let has_autonull = ptt.has_cap("diversity_sweep_ex");

                                    // Default params: coarseStep coarseSettle fineRange fineStep fineSettle gainRange gainStep gainSettle
                                    let default_params = vec![5.0, 50.0, 15.0, 1.0, 50.0, 6.0, 0.5, 50.0];
                                    let params = crate::load_smart_null_params().unwrap_or(default_params.clone());

                                    if is_ultra && has_smartnull {
                                        info!("Starting Thetis ULTRA null (continuous AVG sweep)");
                                        ptt.diversity_ultranull(&params).await;
                                    } else if has_smartnull {
                                        info!("Starting Thetis smart null (coarse={} deg@{}ms fine=+/-{} deg@{}ms gain=+/-{}dB@{}ms)",
                                            params[0], params[1], params[2], params[4], params[5], params[7]);
                                        ptt.diversity_smartnull(&params).await;
                                    } else {
                                    // Fastsweep (F line) - requires Thetis cap
                                    let fastsweep = if has_fastsweep { crate::load_smart_fastsweep() } else { None };
                                    if let Some((start, end, step, settle, meter)) = fastsweep {
                                        let meter_name = if meter == 1 { "AVG" } else { "instant" };
                                        info!("Starting Thetis fastsweep: {:.0} deg to {:.0} deg step {:.2} deg settle {}ms meter={}", start, end, step, settle, meter_name);
                                        ptt.diversity_fastsweep(start, end, step, settle, meter).await;
                                    } else {
                                        // Fallback: step-based autonull (P/G lines or default sweep)
                                        let mut steps = crate::load_smart_steps_server();
                                        let settle = crate::load_smart_settle_ms();
                                        if steps.is_empty() {
                                            // Generate default P/G steps: coarse 360 deg in 5 deg, fine +/-15 deg in 1 deg, gain +/-6dB in 0.5dB
                                            info!("No P/G steps in config, generating default sweep plan");
                                            let mut phase_offsets: Vec<f32> = (-180..=180).step_by(5).map(|d| d as f32).collect();
                                            phase_offsets.extend((-15..=15).map(|d| d as f32));
                                            steps.push((phase_offsets, false));
                                            let gain_offsets: Vec<f32> = (-12..=12).map(|d| d as f32 * 0.5).collect();
                                            steps.push((gain_offsets, true));
                                        }
                                        if has_autonull {
                                            info!("Starting Thetis-side auto-null ({} steps, {}ms settle)", steps.len(), settle);
                                            ptt.diversity_autonull(settle, &steps).await;
                                        } else {
                                            warn!("Auto-null: Thetis has no _ex caps, auto-null not available");
                                        }
                                    }
                                    }
                                }
                                ControlId::AgcAutoRx1 => {
                                    ptt.set_agc_auto(0, ctrl.value != 0).await;
                                }
                                ControlId::AgcAutoRx2 => {
                                    ptt.set_agc_auto(1, ctrl.value != 0).await;
                                }
                                ControlId::AudioMode => {
                                    ctrl_log!(opening, "Client {} audio mode: {}", addr, ctrl.value);
                                    self.session.lock().await.set_audio_mode(addr, ctrl.value as u8);
                                }
                                ControlId::VfoSwap => {
                                    ptt.vfo_swap().await;
                                }
                                ControlId::ThetisTune => {
                                    let on = ctrl.value != 0;
                                    ctrl_log!(opening, "Client {} Thetis TUNE {}", addr, if on { "ON" } else { "OFF" });
                                    ptt.set_tune(on).await;
                                }
                                ControlId::DiversityRead => {
                                    ctrl_log!(opening, "Client {} reading diversity state", addr);
                                    let ptt = self.ptt.clone();
                                    let socket = self.socket.clone();
                                    tokio::spawn(async move {
                                        let ptt_guard = ptt.lock().await;
                                        // Diversity state is only available when TL2-x fork extensions
                                        // are enabled (Phase 3); stock Thetis has no diversity_*_ex push.
                                        // No CAT-fallback exists in TL2 v2 (aux CAT is removed).
                                        // Sentinel cap: enable_ex is canonical indicator that the per-command
                                        // diversity suite is advertised by the fork.
                                        if ptt_guard.has_tci_cap("diversity_enable_ex") {
                                            let tci = ptt_guard.tci_ref().unwrap();
                                            let values = [
                                                (ControlId::DiversityEnable, tci.diversity_enabled as u16),
                                                (ControlId::DiversityRef, tci.diversity_ref as u16),
                                                (ControlId::DiversitySource, tci.diversity_source as u16),
                                                (ControlId::DiversityGainRx1, tci.diversity_gain_rx1),
                                                (ControlId::DiversityGainRx2, tci.diversity_gain_rx2),
                                                (ControlId::DiversityPhase, (tci.diversity_phase + 18000).max(0) as u16),
                                                (ControlId::DiversityGainMulti, tci.diversity_gain_multi),
                                            ];
                                            for (ctrl_id, value) in &values {
                                                let ctrl = ControlPacket { control_id: *ctrl_id, value: *value };
                                                let mut buf = [0u8; ControlPacket::SIZE];
                                                ctrl.serialize(&mut buf);
                                                let _ = socket.try_send_to(&buf, addr);
                                            }
                                            info!("Diversity read via TCI state");
                                        } else {
                                            info!("Diversity read skipped: stock Thetis has no diversity_ex");
                                        }
                                    });
                                }
                                // --- New TCI controls (v2.10.3.13) ---
                                ControlId::AgcMode => {
                                    ctrl_log!(opening, "Client {} AGC mode: {}", addr, ctrl.value);
                                    ptt.set_agc_mode(ctrl.value.min(5) as u8).await;
                                }
                                ControlId::AgcGain => {
                                    ctrl_log!(opening, "Client {} AGC gain: {}", addr, ctrl.value);
                                    ptt.set_agc_gain(ctrl.value.min(120) as u8).await;
                                }
                                ControlId::RitEnable => {
                                    ctrl_log!(opening, "Client {} RIT: {}", addr, if ctrl.value != 0 { "ON" } else { "OFF" });
                                    ptt.set_rit_enable(ctrl.value != 0).await;
                                }
                                ControlId::RitOffset => {
                                    ptt.set_rit_offset(ctrl.value as i16 as i32).await;
                                }
                                ControlId::XitEnable => {
                                    ctrl_log!(opening, "Client {} XIT: {}", addr, if ctrl.value != 0 { "ON" } else { "OFF" });
                                    ptt.set_xit_enable(ctrl.value != 0).await;
                                }
                                ControlId::XitOffset => {
                                    ptt.set_xit_offset(ctrl.value as i16 as i32).await;
                                }
                                ControlId::SqlEnable => {
                                    ctrl_log!(opening, "Client {} SQL: {}", addr, if ctrl.value != 0 { "ON" } else { "OFF" });
                                    ptt.set_sql_enable(ctrl.value != 0).await;
                                }
                                ControlId::SqlLevel => {
                                    // Client sends 0..160, map to Thetis -140..0 dB
                                    let db = (ctrl.value as i16) - 140;
                                    ptt.set_sql_level(db.clamp(-140, 0)).await;
                                }
                                ControlId::NoiseBlanker => {
                                    let level = ctrl.value.min(2) as u8;
                                    ctrl_log!(opening, "Client {} NB: {}", addr, match level { 0 => "OFF", 1 => "NB1", _ => "NB2" });
                                    ptt.set_nb(level).await;
                                }
                                ControlId::CwKeyerSpeed => {
                                    ctrl_log!(opening, "Client {} CW speed: {} WPM", addr, ctrl.value);
                                    ptt.set_cw_keyer_speed(ctrl.value.clamp(1, 60) as u8).await;
                                }
                                ControlId::CwKey => {
                                    // Retired: keying travels as CwEdges packets, through the
                                    // playout and its checks. This path had none.
                                    ctrl_log!(opening, "Client {} sent the retired CwKey control - ignored", addr);
                                }
                                ControlId::CwMacroStop => {
                                    ctrl_log!(opening, "Client {} CW macro stop", addr);
                                    ptt.cw_macro_stop().await;
                                }
                                ControlId::VfoLock => {
                                    ctrl_log!(opening, "Client {} VFO Lock: {}", addr, if ctrl.value != 0 { "ON" } else { "OFF" });
                                    ptt.set_vfo_lock(ctrl.value != 0).await;
                                }
                                ControlId::Binaural => {
                                    // Log only on state-change to suppress spam from clients that
                                    // re-emit BIN ControlPackets ~50 Hz (alpha-5/8 testlogs). Server
                                    // set_binaural() already idempotent; this filters the info-log
                                    // along the same axis.
                                    let new_on = ctrl.value != 0;
                                    let cur_on = ptt.binaural();
                                    if new_on != cur_on {
                                        ctrl_log!(opening, "Client {} BIN: {}", addr, if new_on { "ON" } else { "OFF" });
                                    }
                                    ptt.set_binaural(new_on).await;
                                }
                                ControlId::ApfEnable => {
                                    ctrl_log!(opening, "Client {} APF: {}", addr, if ctrl.value != 0 { "ON" } else { "OFF" });
                                    ptt.set_apf_enable(ctrl.value != 0).await;
                                }
                                ControlId::Mute => {
                                    ctrl_log!(opening, "Client {} MUTE: {}", addr, if ctrl.value != 0 { "ON" } else { "OFF" });
                                    ptt.set_mute(ctrl.value != 0).await;
                                }
                                ControlId::RxMute => {
                                    ctrl_log!(opening, "Client {} RX MUTE: {}", addr, if ctrl.value != 0 { "ON" } else { "OFF" });
                                    ptt.set_rx_mute(ctrl.value != 0).await;
                                }
                                ControlId::ManualNotchFilter => {
                                    ctrl_log!(opening, "Client {} NF: {}", addr, if ctrl.value != 0 { "ON" } else { "OFF" });
                                    ptt.set_nf_enable(ctrl.value != 0).await;
                                }
                                ControlId::RxBalance => {
                                    let val = ctrl.value as i16 as i8;
                                    ctrl_log!(opening, "Client {} RX Balance: {}", addr, val);
                                    ptt.set_rx_balance(val).await;
                                }
                                // --- RX2 TCI controls ---
                                ControlId::Rx2AgcMode => {
                                    ptt.set_rx2_agc_mode(ctrl.value.min(5) as u8).await;
                                }
                                ControlId::Rx2AgcGain => {
                                    ptt.set_rx2_agc_gain(ctrl.value.min(120) as u8).await;
                                }
                                ControlId::Rx2SqlEnable => {
                                    ptt.set_rx2_sql_enable(ctrl.value != 0).await;
                                }
                                ControlId::Rx2SqlLevel => {
                                    ptt.set_rx2_sql_level(ctrl.value.min(160) as i16).await;
                                }
                                ControlId::Rx2NoiseBlanker => {
                                    let level = ctrl.value.min(2) as u8;
                                    ptt.set_rx2_nb(level).await;
                                }
                                ControlId::Rx2Binaural => {
                                    ptt.set_rx2_binaural(ctrl.value != 0).await;
                                }
                                ControlId::Rx2ApfEnable => {
                                    ptt.set_rx2_apf_enable(ctrl.value != 0).await;
                                }
                                ControlId::Rx2VfoLock => {
                                    ptt.set_rx2_vfo_lock(ctrl.value != 0).await;
                                }
                                ControlId::Rx2ManualNotchFilter => {
                                    ptt.set_rx2_nf_enable(ctrl.value != 0).await;
                                }
                                ControlId::TuneDrive => {
                                    ctrl_log!(opening, "Client {} Tune drive: {}%", addr, ctrl.value);
                                    ptt.set_tune_drive(ctrl.value.min(100) as u8).await;
                                }
                                ControlId::MonitorVolume => {
                                    let db = ctrl.value as i16 as i8;
                                    ctrl_log!(opening, "Client {} Mon volume: {} dB", addr, db);
                                    ptt.set_mon_volume(db).await;
                                }
                                // --- Diversity controls (Thetis CAT) ---
                                // All diversity commands via spawn to prevent main loop blocking
                                // (ZZDE causes Thetis to reconfigure IQ streams which blocks TCP CAT)
                                ControlId::DiversityEnable | ControlId::DiversityRef
                                | ControlId::DiversitySource | ControlId::DiversityGainRx1
                                | ControlId::DiversityGainRx2 | ControlId::DiversityPhase
                                | ControlId::DiversityGainMulti => {
                                    let ptt = self.ptt.clone();
                                    let cid = ctrl.control_id;
                                    let val = ctrl.value;
                                    tokio::spawn(async move {
                                        let mut guard = ptt.lock().await;
                                        match cid {
                                            ControlId::DiversityEnable => guard.set_diversity_enable(val != 0).await,
                                            ControlId::DiversityRef => guard.set_diversity_ref(val != 0).await,
                                            ControlId::DiversitySource => guard.set_diversity_source(val as u32).await,
                                            ControlId::DiversityGainRx1 => guard.set_diversity_gain(0, val).await,
                                            ControlId::DiversityGainRx2 => guard.set_diversity_gain(1, val).await,
                                            ControlId::DiversityPhase => guard.set_diversity_phase((val as i32) - 18000).await,
                                            ControlId::DiversityGainMulti => guard.set_diversity_gain_multi(val).await,
                                            _ => {}
                                        }
                                    });
                                }
                                // --- Yaesu controls ---
                                ControlId::YaesuEnable => {
                                    let enabled = ctrl.value != 0;
                                    self.session.lock().await.set_yaesu_enabled(addr, enabled);
                                    ctrl_log!(opening, "Client {} Yaesu audio: {}", addr, if enabled { "ON" } else { "OFF" });
                                }
                                ControlId::YaesuStateEnable => {
                                    let enabled = ctrl.value != 0;
                                    self.session.lock().await.set_yaesu_state_enabled(addr, enabled);
                                }
                                ControlId::Yaesu2StateEnable => {
                                    let enabled = ctrl.value != 0;
                                    self.session.lock().await.set_yaesu2_state_enabled(addr, enabled);
                                }
                                ControlId::YaesuReadMemoryTones => {
                                    let slot1 = ctrl.value != 0;
                                    let radio = if slot1 { &yaesu2 } else { &yaesu };
                                    if let Some(r) = radio {
                                        r.send_command(crate::yaesu::YaesuCmd::ReadMemoryTones);
                                        ctrl_log!(opening, "Client {} read memory tones (radio {})",
                                            addr, if slot1 { 2 } else { 1 });
                                    }
                                }
                                // Retired as a user control: the CAT monitor is a
                                // diagnostic, not a feature, and it multiplies the server
                                // log. It is reached with THETISLINK_CAT_MONITOR=1 on the
                                // server instead (yaesu/poll.rs). The id stays reserved so
                                // the byte is never reused, and an older client that still
                                // sends it is ignored rather than refused.
                                ControlId::YaesuCatMonitor => {}
                                ControlId::YaesuPowerOnOff => {
                                    if let Some(ref yaesu) = yaesu {
                                        let on = ctrl.value != 0;
                                        yaesu.send_command(crate::yaesu::YaesuCmd::SetPower(on));
                                        ctrl_log!(opening, "Client {} Yaesu power: {}", addr, if on { "ON" } else { "OFF" });
                                    }
                                }
                                ControlId::Yaesu2PowerOnOff => {
                                    if let Some(ref yaesu2) = yaesu2 {
                                        let on = ctrl.value != 0;
                                        yaesu2.send_command(crate::yaesu::YaesuCmd::SetPower(on));
                                        ctrl_log!(opening, "Client {} Yaesu2 power: {}", addr, if on { "ON" } else { "OFF" });
                                    }
                                }
                                // A radio's PTT is the flag on its audio since
                                // build 282 (`radio_audio!`); the ids stay
                                // reserved. A client from before that keys
                                // nothing here - safe, and said once per press
                                // with the build to look at.
                                ControlId::YaesuPtt | ControlId::Yaesu2Ptt => {
                                    if ctrl.value != 0 {
                                        warn!("Client {} sent a Yaesu PTT control - this server keys a Yaesu by the flag on its audio (build 282); the client needs the same build", addr);
                                    }
                                }
                                ControlId::YaesuFreq => {} // handled via FrequencyYaesu packet
                                ControlId::YaesuMicGain => {
                                    // Legacy server-side gain control; current clients apply Yaesu mic gain before Opus.
                                    // Kept only for older clients that still send this control.
                                    let gain = ctrl.value as f32 / 100.0;
                                    yaesu_mic_gain.store(gain.to_bits(), Ordering::Relaxed);
                                    ctrl_log!(opening, "Client {} Yaesu legacy server gain: {:.2}x", addr, gain);
                                }
                                ControlId::YaesuMode => {
                                    if let Some(ref yaesu) = yaesu {
                                        yaesu.send_command(crate::yaesu::YaesuCmd::SetMode(ctrl.value as u8));
                                        ctrl_log!(opening, "Client {} Yaesu mode: {}", addr, ctrl.value);
                                    }
                                }
                                ControlId::YaesuReadMemories => {
                                    if let Some(ref yaesu) = yaesu {
                                        // value 1 = "the server's copy is fine" (a client
                                        // that just connected); value 0 = "read the radio
                                        // now" (the Read radio button). Older clients only
                                        // ever send 0, so they keep today's behaviour
                                        // exactly - and a newer client talking to an older
                                        // server has its 1 ignored, which also lands on a
                                        // real read. No capability flag needed either way.
                                        // "The server's copy is fine" - normally delivered by
                                        // the subscriber push, so there is nothing to do. But if
                                        // there IS no copy the push has nothing to send and the
                                        // client would wait forever: the connect-time read can
                                        // fail (radio in standby, FTX-1 cold scan). Fall back to
                                        // a real read in that case.
                                        let copy = yaesu.status().last_memory_blob.clone();
                                        if ctrl.value == 1 && memory_copy_has_channels(copy.as_deref()) {
                                            ctrl_log!(opening, "Client {} will get the Yaesu memories from the server's copy", addr);
                                        } else {
                                            if ctrl.value == 1 && copy.is_none() {
                                                warn!("Client {} wanted the server's copy of the Yaesu memories, but there is none - reading the radio", addr);
                                            } else if ctrl.value == 1 {
                                                // Named in advance (working rule 11, build 170).
                                                warn!("Client {} wanted the server's copy of the Yaesu memories, but it has no channels - reading the radio", addr);
                                            }
                                            ctrl_log!(opening, "Client {} requested Yaesu memory read", addr);
                                            yaesu.send_command(crate::yaesu::YaesuCmd::ReadAllMemories);
                                        }
                                    }
                                }
                                ControlId::YaesuRecallMemory => {
                                    if let Some(ref yaesu) = yaesu {
                                        ctrl_log!(opening, "Client {} Yaesu recall memory {}", addr, ctrl.value);
                                        yaesu.send_command(crate::yaesu::YaesuCmd::RecallMemory(ctrl.value));
                                    }
                                }
                                ControlId::YaesuSelectVfo => {
                                    if let Some(ref yaesu) = yaesu {
                                        ctrl_log!(opening, "Client {} Yaesu VFO: {}", addr, match ctrl.value { 0 => "A", 1 => "B", _ => "swap" });
                                        yaesu.send_command(crate::yaesu::YaesuCmd::SelectVfo(ctrl.value as u8));
                                    }
                                }
                                ControlId::YaesuSquelch => {
                                    if let Some(ref yaesu) = yaesu {
                                        yaesu.send_command(crate::yaesu::YaesuCmd::RawCat(
                                            format!("SQ0{:03};", ctrl.value.min(255))));
                                    }
                                }
                                ControlId::YaesuRfGain => {
                                    if let Some(ref yaesu) = yaesu {
                                        yaesu.send_command(crate::yaesu::YaesuCmd::RawCat(
                                            format!("RG0{:03};", ctrl.value.min(255))));
                                    }
                                }
                                ControlId::YaesuRadioMicGain => {
                                    if let Some(ref yaesu) = yaesu {
                                        yaesu.send_command(crate::yaesu::YaesuCmd::RawCat(
                                            format!("MG{:03};", ctrl.value.min(100))));
                                    }
                                }
                                ControlId::YaesuRfPower => {
                                    if let Some(ref yaesu) = yaesu {
                                        yaesu.send_command(crate::yaesu::YaesuCmd::SetTxPower(ctrl.value as u8));
                                    }
                                }
                                ControlId::YaesuButton => {
                                    if let Some(ref yaesu) = yaesu {
                                        // The ATU speaks the MODEL's dialect, not the slot's
                                        // (RadioModel::atu_cat). And a tuning cycle transmits,
                                        // so it asks the same permission a PTT asks.
                                        let model = crate::yaesu::RadioModel::from_code(yaesu.model_code());
                                        // A tuning cycle is a carrier, so it asks what a PTT
                                        // asks. Not a `continue`: the rest of this arm handles
                                        // the memory channel buttons and must still run.
                                        let mut refused: Option<&'static str> = None;
                                        let cat = match ctrl.value {
                                            0 => "AB;",      // A=B
                                            // Scan start/stop, per model like the ATU below.
                                            1 => model.scan_cat(true),
                                            2 => model.scan_cat(false),
                                            3 => match yaesu.status().tx_start_refusal() {
                                                Some(why) => {
                                                    refused = Some(why);
                                                    ""
                                                }
                                                None => model.atu_cat(crate::yaesu::AtuAction::TuneNow),
                                            },
                                            4 => model.atu_cat(crate::yaesu::AtuAction::Off),
                                            15 => model.atu_cat(crate::yaesu::AtuAction::On),
                                            5 => "BU0;",     // Band up (VFO-A)
                                            6 => "BD0;",     // Band down (VFO-A)
                                            7 => "ST1;",     // Split on
                                            8 => "ST0;",     // Split off
                                            _ => "",
                                        };
                                        if let Some(why) = refused {
                                            warn!("Client {} asked Yaesu radio 0 to tune but {} - refused", addr, why);
                                            let mut buf = [0u8; PttDeniedPacket::SIZE];
                                            PttDeniedPacket::serialize(&mut buf, DeniedTarget::Radio1, Denial::Refused);
                                            let _ = self.socket.send_to(&buf, addr).await;
                                        }
                                        if !cat.is_empty() {
                                            ctrl_log!(opening, "Client {} Yaesu button {}: {}", addr, ctrl.value, cat);
                                            yaesu.send_command(crate::yaesu::YaesuCmd::RawCat(cat.to_string()));
                                        }
                                        // Memory channel up/down: jump to the next/previous NON-EMPTY
                                        // channel via MC. MC{cur+/-1} gets stuck on gaps (the radio
                                        // rejects MC to an empty slot) and the radio's UP;/DN; steps
                                        // the VFO (MemTune), not the channel - so we skip empties using
                                        // the channel list from the last memory read, falling back to
                                        // +/-1 when no list is available yet.
                                        if ctrl.value == 9 || ctrl.value == 10 {
                                            let st = yaesu.status();
                                            let cur = st.memory_channel;
                                            let filled = st.filled_memory_channels;
                                            let next = next_memory_channel(&filled, cur, ctrl.value == 9, model.memory_channels());
                                            ctrl_log!(opening, "Client {} Yaesu Mem: {} -> {} (skip empty, {} filled)", addr, cur, next, filled.len());
                                            yaesu.send_command(crate::yaesu::YaesuCmd::RecallMemory(next));
                                        }
                                    }
                                }
                                ControlId::YaesuReadMenus => {
                                    if let Some(ref yaesu) = yaesu {
                                        // Same rule as the memory list: value 1 = the
                                        // server's copy is fine, which the subscriber
                                        // push already delivers. 0 = walk the radio now.
                                        if ctrl.value == 1 && yaesu.status().last_menu_blob.is_some() {
                                            ctrl_log!(opening, "Client {} will get the EX settings from the server's copy", addr);
                                        } else {
                                            if ctrl.value == 1 {
                                                warn!("Client {} wanted the server's copy of the EX settings, but there is none - reading the radio", addr);
                                            }
                                            ctrl_log!(opening, "Client {} requested Yaesu menu read", addr);
                                            yaesu.send_command(crate::yaesu::YaesuCmd::ReadAllMenus);
                                        }
                                    }
                                }
                                ControlId::YaesuSetMenu => {
                                    // value encodes menu number; P2 data arrives via YaesuMemoryData packet
                                    if let Some(ref yaesu) = yaesu {
                                        if let Some(text) = yaesu_write.trigger(addr) {
                                            // text format: "<key>:value". No client
                                            // in this tree sends this control any
                                            // more - the SETMENU packet replaced it -
                                            // but it decides the EX dialect, so it
                                            // asks the radio like every other writer
                                            // rather than being a second place that
                                            // assumes a 991A.
                                            if let Some((key, val)) = text.split_once(':') {
                                                // `set_menu_entry` also patches the
                                                // server's copy and lets it push:
                                                // waiting for a re-scan would occupy
                                                // CAT for seconds over one value, and
                                                // until then every other client would
                                                // hold the old one.
                                                match yaesu.set_menu_entry(key, val) {
                                                    Ok(what) => ctrl_log!(opening, "Client {} [radio0] set {} = {}", addr, what, val),
                                                    Err(why) => warn!("Client {} [radio0] EX set refused: {}", addr, why),
                                                }
                                            }
                                        }
                                    }
                                }
                                ControlId::YaesuWriteMemories => {
                                    // Data comes via YaesuMemoryData packet (stored in pending_write).
                                    // If the data hasn't arrived yet (UDP reorder on
                                    // 2.7kB fragmented vs 8B control), arm the latch so
                                    // the YaesuMemoryData handler fires the write when
                                    // the data lands. See `yaesu_write_armed` decl above.
                                    if let Some(ref yaesu) = yaesu {
                                        match yaesu_write.trigger(addr) {
                                            Some(text) => {
                                                ctrl_log!(opening, "Client {} writing Yaesu memories", addr);
                                                yaesu.send_command(crate::yaesu::YaesuCmd::WriteAllMemories(text));
                                            }
                                            None => ctrl_log!(opening, "Client {} write memories: its data not here yet, waiting for it", addr),
                                        }
                                    }
                                }
                                ControlId::SpectrumMaxBins => {
                                    self.session.lock().await.set_spectrum_max_bins(addr, ctrl.value);
                                    ctrl_log!(opening, "Client {} spectrum max_bins: {}", addr, ctrl.value);
                                }
                                ControlId::Rx2SpectrumMaxBins => {
                                    self.session.lock().await.set_rx2_spectrum_max_bins(addr, ctrl.value);
                                    ctrl_log!(opening, "Client {} RX2 spectrum max_bins: {}", addr, ctrl.value);
                                }
                                ControlId::SpectrumFftSize => {
                                    self.spectrum.lock().await.set_fft_size(ctrl.value);
                                }
                                ControlId::Rx2SpectrumFftSize => {
                                    self.rx2_spectrum.lock().await.set_fft_size(ctrl.value);
                                }
                                ControlId::SpectrumBinDepth => {
                                    // Reserved for future use, ignored
                                }
                                ControlId::DdcSampleRateRx1 | ControlId::DdcSampleRateRx2 => {
                                    // TL2-1 fork extension: when ddc_sample_rate_ex cap is
                                    // advertised, dispatch via PTT chooser. Stock-mode (no
                                    // cap) silently ignores (chooser self-gates).
                                    let rx: u32 = if matches!(ctrl.control_id, ControlId::DdcSampleRateRx1) { 0 } else { 1 };
                                    let rate_hz: u32 = (ctrl.value as u32).saturating_mul(1000); // client sends kHz
                                    let ptt = self.ptt.clone();
                                    tokio::spawn(async move {
                                        let mut guard = ptt.lock().await;
                                        guard.set_ddc_sample_rate(rx, rate_hz).await;
                                    });
                                }
                                // --- VRX (Virtual RX) controls - per-client (PATCH-vrx-per-client) ---
                                // Each control writes THIS client's VrxControlState via the
                                // manager; the manager lock is released before the inner
                                // control lock (control() returns an owned Arc).
                                ControlId::VrxEnable => {
                                    let on = ctrl.value != 0;
                                    let ctl = self.vrx_mgr.lock().unwrap().control(addr, 0);
                                    ctl.lock().unwrap().enabled = on;
                                    self.session.lock().await.set_vrx_audio(addr, 0, on);
                                    ctrl_log!(opening, "Client {} VRX1 enable: {}", addr, if on { "ON" } else { "OFF" });
                                }
                                ControlId::VrxMode => {
                                    let mode = match ctrl.value {
                                        0 => vrx_rs::VrxMode::Usb, 1 => vrx_rs::VrxMode::Lsb,
                                        2 => vrx_rs::VrxMode::Am, 3 => vrx_rs::VrxMode::Sam,
                                        4 => vrx_rs::VrxMode::Fm, _ => vrx_rs::VrxMode::Usb,
                                    };
                                    let ctl = self.vrx_mgr.lock().unwrap().control(addr, 0);
                                    ctl.lock().unwrap().mode = mode;
                                    ctrl_log!(opening, "Client {} VRX1 mode: {:?}", addr, mode);
                                }
                                ControlId::VrxVolume => {
                                    let v = (ctrl.value as f32 / 100.0).clamp(0.0, 4.0);
                                    let ctl = self.vrx_mgr.lock().unwrap().control(addr, 0);
                                    ctl.lock().unwrap().volume = v;
                                    ctrl_log!(opening, "Client {} VRX1 volume: {:.2}", addr, v);
                                }
                                ControlId::VrxEnable2 => {
                                    let on = ctrl.value != 0;
                                    let ctl = self.vrx_mgr.lock().unwrap().control(addr, 1);
                                    ctl.lock().unwrap().enabled = on;
                                    self.session.lock().await.set_vrx_audio(addr, 1, on);
                                    ctrl_log!(opening, "Client {} VRX2 enable: {}", addr, if on { "ON" } else { "OFF" });
                                }
                                ControlId::VrxMode2 => {
                                    let mode = match ctrl.value {
                                        0 => vrx_rs::VrxMode::Usb, 1 => vrx_rs::VrxMode::Lsb,
                                        2 => vrx_rs::VrxMode::Am, 3 => vrx_rs::VrxMode::Sam,
                                        4 => vrx_rs::VrxMode::Fm, _ => vrx_rs::VrxMode::Usb,
                                    };
                                    let ctl = self.vrx_mgr.lock().unwrap().control(addr, 1);
                                    ctl.lock().unwrap().mode = mode;
                                    ctrl_log!(opening, "Client {} VRX2 mode: {:?}", addr, mode);
                                }
                                ControlId::VrxVolume2 => {
                                    let v = (ctrl.value as f32 / 100.0).clamp(0.0, 4.0);
                                    let ctl = self.vrx_mgr.lock().unwrap().control(addr, 1);
                                    ctl.lock().unwrap().volume = v;
                                    ctrl_log!(opening, "Client {} VRX2 volume: {:.2}", addr, v);
                                }
                                ControlId::VrxFilterLow => {
                                    let hz = ctrl.value as i16 as i32;
                                    let ctl = self.vrx_mgr.lock().unwrap().control(addr, 0);
                                    ctl.lock().unwrap().filter_low_hz = hz;
                                }
                                ControlId::VrxFilterHigh => {
                                    let hz = ctrl.value as i16 as i32;
                                    let ctl = self.vrx_mgr.lock().unwrap().control(addr, 0);
                                    let (lo, hi) = {
                                        let mut s = ctl.lock().unwrap();
                                        s.filter_high_hz = hz;
                                        (s.filter_low_hz, s.filter_high_hz)
                                    };
                                    ctrl_log!(opening, "Client {} VRX1 filter: {} .. {} Hz", addr, lo, hi);
                                }
                                ControlId::VrxFilterLow2 => {
                                    let hz = ctrl.value as i16 as i32;
                                    let ctl = self.vrx_mgr.lock().unwrap().control(addr, 1);
                                    ctl.lock().unwrap().filter_low_hz = hz;
                                }
                                ControlId::VrxFilterHigh2 => {
                                    let hz = ctrl.value as i16 as i32;
                                    let ctl = self.vrx_mgr.lock().unwrap().control(addr, 1);
                                    let (lo, hi) = {
                                        let mut s = ctl.lock().unwrap();
                                        s.filter_high_hz = hz;
                                        (s.filter_low_hz, s.filter_high_hz)
                                    };
                                    ctrl_log!(opening, "Client {} VRX2 filter: {} .. {} Hz", addr, lo, hi);
                                }
                                ControlId::VrxSpectrumEnable => {
                                    let on = ctrl.value != 0;
                                    {
                                        let mut m = self.vrx_mgr.lock().unwrap();
                                        let cur = m.spectrum_span(&addr, 0);
                                        m.set_spectrum_span(addr, 0, if on { if cur == 0 { 24 } else { cur } } else { 0 });
                                    }
                                    self.session.lock().await.set_vrx_spectrum(addr, 0, on);
                                    // VRX1 reads from the RX1 processor: it must be on, even if
                                    // nobody wants the regular RX1 spectrum.
                                    self.refresh_spectrum_processors().await;
                                    ctrl_log!(opening, "Client {} VRX1 high-res spectrum: {}", addr, if on { "ON" } else { "OFF" });
                                }
                                ControlId::VrxSpectrumEnable2 => {
                                    let on = ctrl.value != 0;
                                    {
                                        let mut m = self.vrx_mgr.lock().unwrap();
                                        let cur = m.spectrum_span(&addr, 1);
                                        m.set_spectrum_span(addr, 1, if on { if cur == 0 { 24 } else { cur } } else { 0 });
                                    }
                                    self.session.lock().await.set_vrx_spectrum(addr, 1, on);
                                    self.refresh_spectrum_processors().await;
                                    ctrl_log!(opening, "Client {} VRX2 high-res spectrum: {}", addr, if on { "ON" } else { "OFF" });
                                }
                                ControlId::VrxSpectrumPan => {
                                    let hz = sdr_remote_core::protocol::unpack_vrx_pan(ctrl.value);
                                    self.vrx_mgr.lock().unwrap().set_spectrum_pan(addr, 0, hz);
                                }
                                ControlId::VrxSpectrumPan2 => {
                                    let hz = sdr_remote_core::protocol::unpack_vrx_pan(ctrl.value);
                                    self.vrx_mgr.lock().unwrap().set_spectrum_pan(addr, 1, hz);
                                }
                                ControlId::VrxSpectrumSpanKhz => {
                                    let mut m = self.vrx_mgr.lock().unwrap();
                                    if m.spectrum_span(&addr, 0) != 0 {
                                        m.set_spectrum_span(addr, 0, ctrl.value.max(1));
                                    }
                                }
                                ControlId::VrxSpectrumSpanKhz2 => {
                                    let mut m = self.vrx_mgr.lock().unwrap();
                                    if m.spectrum_span(&addr, 1) != 0 {
                                        m.set_spectrum_span(addr, 1, ctrl.value.max(1));
                                    }
                                }
                                // --- VRX audio rate (per-client per-VRX, Option B) ---
                                ControlId::VrxAudioRate => {
                                    // VRX1 rate (0=NB,1=WB,2=Auto). Old clients send only this.
                                    self.vrx_mgr.lock().unwrap().set_rate_mode(addr, 0, (ctrl.value & 0xFF) as u8);
                                    ctrl_log!(opening, "Client {} VRX1 audio-rate: {}", addr,
                                        match ctrl.value { 0 => "NB", 1 => "WB", _ => "Auto" });
                                }
                                ControlId::VrxAudioRate2 => {
                                    // VRX2 rate; new clients send this for independent per-VRX rate.
                                    self.vrx_mgr.lock().unwrap().set_rate_mode(addr, 1, (ctrl.value & 0xFF) as u8);
                                    ctrl_log!(opening, "Client {} VRX2 audio-rate: {}", addr,
                                        match ctrl.value { 0 => "NB", 1 => "WB", _ => "Auto" });
                                }
                                ControlId::VrxSamAutoTune => {
                                    // Per-client subscription only - the audio loop
                                    // derives the per-VRX AFC enable as the aggregate
                                    // (any active subscriber) before each feed, so one
                                    // client never flips another client's AFC.
                                    let on = ctrl.value != 0;
                                    self.session.lock().await.set_vrx_autotune(addr, 0, on);
                                    ctrl_log!(opening, "Client {} VRX1 SAM auto-tune: {}", addr, if on { "ON" } else { "OFF" });
                                }
                                ControlId::VrxSamAutoTune2 => {
                                    let on = ctrl.value != 0;
                                    self.session.lock().await.set_vrx_autotune(addr, 1, on);
                                    ctrl_log!(opening, "Client {} VRX2 SAM auto-tune: {}", addr, if on { "ON" } else { "OFF" });
                                }
                                // --- Dual-radio slot 1 controls (Option B-prime) ---
                                // Mirror of the slot-0 Yaesu controls, routed to yaesu2.
                                // Yaesu2Enable is the subscription gate (same pattern as YaesuEnable).
                                ControlId::Yaesu2Enable => {
                                    let enabled = ctrl.value != 0;
                                    self.session.lock().await.set_yaesu2_enabled(addr, enabled);
                                    ctrl_log!(opening, "Client {} [radio1]: {}", addr, if enabled { "ON" } else { "OFF" });
                                }
                                ControlId::Yaesu2Freq => {} // handled via FrequencyYaesu2 packet
                                ControlId::Yaesu2MicGain => {
                                    let gain = ctrl.value as f32 / 100.0;
                                    yaesu2_mic_gain.store(gain.to_bits(), Ordering::Relaxed);
                                    ctrl_log!(opening, "Client {} [radio1] legacy server gain: {:.2}x", addr, gain);
                                }
                                ControlId::Yaesu2Mode => {
                                    if let Some(ref yaesu) = yaesu2 {
                                        yaesu.send_command(crate::yaesu::YaesuCmd::SetMode(ctrl.value as u8));
                                    }
                                }
                                ControlId::Yaesu2SelectVfo => {
                                    if let Some(ref yaesu) = yaesu2 {
                                        yaesu.send_command(crate::yaesu::YaesuCmd::SelectVfo(ctrl.value as u8));
                                    }
                                }
                                ControlId::Yaesu2Squelch => {
                                    if let Some(ref yaesu) = yaesu2 {
                                        yaesu.send_command(crate::yaesu::YaesuCmd::RawCat(
                                            format!("SQ0{:03};", ctrl.value.min(255))));
                                    }
                                }
                                ControlId::Yaesu2RfGain => {
                                    if let Some(ref yaesu) = yaesu2 {
                                        yaesu.send_command(crate::yaesu::YaesuCmd::RawCat(
                                            format!("RG0{:03};", ctrl.value.min(255))));
                                    }
                                }
                                ControlId::Yaesu2RadioMicGain => {
                                    if let Some(ref yaesu) = yaesu2 {
                                        yaesu.send_command(crate::yaesu::YaesuCmd::RawCat(
                                            format!("MG{:03};", ctrl.value.min(100))));
                                    }
                                }
                                ControlId::Yaesu2RfPower => {
                                    if let Some(ref yaesu) = yaesu2 {
                                        yaesu.send_command(crate::yaesu::YaesuCmd::SetTxPower(ctrl.value as u8));
                                    }
                                }
                                ControlId::Yaesu2RecallMemory => {
                                    if let Some(ref yaesu) = yaesu2 {
                                        yaesu.send_command(crate::yaesu::YaesuCmd::RecallMemory(ctrl.value));
                                    }
                                }
                                ControlId::Yaesu2Button => {
                                    if let Some(ref yaesu) = yaesu2 {
                                        let model2 = crate::yaesu::RadioModel::from_code(yaesu.model_code());
                                        let mut refused: Option<&'static str> = None;
                                        let cat = match ctrl.value {
                                            0 => "AB;",
                                            // Scan and ATU per model, not per slot; the
                                            // dialects and why they differ are on
                                            // RadioModel. The FTX-1 scans on its MAIN
                                            // side, which is where its USB audio is.
                                            1 => model2.scan_cat(true),
                                            2 => model2.scan_cat(false),
                                            // The ATU per model, not per slot: RadioModel::atu_cat.
                                            3 => match yaesu.status().tx_start_refusal() {
                                                Some(why) => {
                                                    refused = Some(why);
                                                    ""
                                                }
                                                None => model2.atu_cat(crate::yaesu::AtuAction::TuneNow),
                                            },
                                            4 => model2.atu_cat(crate::yaesu::AtuAction::Off),
                                            15 => model2.atu_cat(crate::yaesu::AtuAction::On),
                                            5 => "BU0;", 6 => "BD0;", 7 => "ST1;", 8 => "ST0;",
                                            // Dual-RX MAIN/SUB active-TX/RX selection (FTX-1) - verified
                                            // command format from cat-ftx1: FT0=MAIN, FT1=SUB.
                                            11 => "FT0;", 12 => "FT1;",
                                            // Quick Memory Bank (FTX-1): QI=store current VFO,
                                            // QR=recall/cycle. Action commands, no answer.
                                            13 => "QI;", 14 => "QR;",
                                            _ => "",
                                        };
                                        if let Some(why) = refused {
                                            warn!("Client {} asked Yaesu radio 1 to tune but {} - refused", addr, why);
                                            let mut buf = [0u8; PttDeniedPacket::SIZE];
                                            PttDeniedPacket::serialize(&mut buf, DeniedTarget::Radio2, Denial::Refused);
                                            let _ = self.socket.send_to(&buf, addr).await;
                                        }
                                        if !cat.is_empty() {
                                            ctrl_log!(opening, "Client {} [radio1] Yaesu button {}: {}", addr, ctrl.value, cat);
                                            yaesu.send_command(crate::yaesu::YaesuCmd::RawCat(cat.to_string()));
                                        }
                                        // Same empty-channel skip as slot 0, via the filled-channel
                                        // list from the last read (FTX-1 range 1..=99). Falls back to
                                        // +/-1 when no list is available.
                                        if ctrl.value == 9 || ctrl.value == 10 {
                                            let st = yaesu.status();
                                            let cur = st.memory_channel;
                                            let filled = st.filled_memory_channels;
                                            let next = next_memory_channel(&filled, cur, ctrl.value == 9, model2.memory_channels());
                                            ctrl_log!(opening, "Client {} [radio1] Yaesu Mem: {} -> {} (skip empty, {} filled)", addr, cur, next, filled.len());
                                            yaesu.send_command(crate::yaesu::YaesuCmd::RecallMemory(next));
                                        }
                                    }
                                }
                                // Slot-1 memories (Phase B) - read/write to radio 2.
                                ControlId::Yaesu2ReadMemories => {
                                    if let Some(ref yaesu) = yaesu2 {
                                        // Same rule as slot 0, including the fall-back when
                                        // the server has no copy yet.
                                        let copy = yaesu.status().last_memory_blob.clone();
                                        if ctrl.value == 1 && memory_copy_has_channels(copy.as_deref()) {
                                            ctrl_log!(opening, "Client {} [radio1] will get the memories from the server's copy", addr);
                                        } else {
                                            if ctrl.value == 1 && copy.is_none() {
                                                warn!("Client {} [radio1] wanted the server's copy of the memories, but there is none - reading the radio", addr);
                                            } else if ctrl.value == 1 {
                                                // Named in advance (working rule 11, build 170).
                                                warn!("Client {} [radio1] wanted the server's copy of the memories, but it has no channels - reading the radio", addr);
                                            }
                                            ctrl_log!(opening, "Client {} [radio1] requested memory read", addr);
                                            yaesu.send_command(crate::yaesu::YaesuCmd::ReadAllMemories);
                                        }
                                    }
                                }
                                ControlId::Yaesu2WriteMemories => {
                                    if let Some(ref yaesu) = yaesu2 {
                                        match yaesu2_write.trigger(addr) {
                                            Some(text) => {
                                                ctrl_log!(opening, "Client {} [radio1] writing memories", addr);
                                                yaesu.send_command(crate::yaesu::YaesuCmd::WriteAllMemories(text));
                                            }
                                            None => ctrl_log!(opening, "Client {} [radio1] write memories: its data not here yet, waiting for it", addr),
                                        }
                                    }
                                }
                                // Slot-1 EX menu (Phase C1): read = hierarchical scan
                                // on the FTX-1. The write arrives via YaesuMemoryData2
                                // with a "SETMENU:" prefix (see handler below).
                                ControlId::Yaesu2ReadMenus => {
                                    if let Some(ref yaesu) = yaesu2 {
                                        if ctrl.value == 1 && yaesu.status().last_menu_blob.is_some() {
                                            ctrl_log!(opening, "Client {} [radio1] will get the EX settings from the server's copy", addr);
                                        } else {
                                            if ctrl.value == 1 {
                                                warn!("Client {} [radio1] wanted the server's copy of the EX settings, but there is none - reading the radio", addr);
                                            }
                                            ctrl_log!(opening, "Client {} [radio1] requested EX menu read", addr);
                                            yaesu.send_command(crate::yaesu::YaesuCmd::ReadAllMenus);
                                        }
                                    }
                                }
                                // Nothing to do, and that is the whole handling:
                                // no client in this tree sends this any more (the
                                // SETMENU packet replaced it, see the slot-0 arm),
                                // and an older one that still does is ignored
                                // rather than refused. The id stays reserved so the
                                // byte is never reused.
                                ControlId::Yaesu2SetMenu => {}
                                // Server -> client only: the SWR reading is pushed
                                // out with the TX meters, never sent in by a client.
                                // Explicit rather than absent, so this match stays
                                // exhaustive and the compiler keeps guarding it.
                                ControlId::ThetisSwr => {}
                            }
                        }
                    }
                }

                // Playout: one frame per 20 ms tick (smooth, regular cadence), a
                // second only to rebuild Thetis's ring (build 193).
                // PTT state changes are driven by jitter buffer output, not packet arrival.
                _ = playout_tick.tick() => {
                    // Check if prefill delay elapsed -> send ZZTX1; now
                    self.ptt.lock().await.check_prefill().await;
                    // The same playout as a radio's (`tx_recover::play_tick`, build
                    // 288; step 1b of `KOPPELING` 18): the buffer, a gap, catching
                    // up in the pauses, the release frame and a press behind it.
                    // One frame per tick, two when Thetis's ring stood low at the
                    // tick's start and the buffer holds more than its target
                    // (build 193, `TX_RING_LOW`). What Thetis does with it stays
                    // here: it keys when the press plays, and lets go when the
                    // release does.
                    let ring_at_tick = self.ptt.lock().await.tx_ring_level();
                    let ring = ring_at_tick.map(|level| (level as u64, crate::tci::TX_RING_LOW as u64));
                    let played = crate::tx_recover::play_tick(&mut jitter_buf, &mut opus_decoder, &mut thetis_tx_loss, &mut pause_trim, Instant::now(), ring);
                    for pcm in played.pcm {
                        let resampled = resample_to_device(&mut tx_resampler_out, &pcm);
                        self.ptt.lock().await.write_tx_audio(&resampled);
                        self.audio_stats.tx.tick(self.server_start);
                    }
                    if played.pressed_again {
                        // Named in advance (working rule 11, build 288).
                        info!("Thetis TX: pressed again behind the release frame - it plays on, the radio stays keyed");
                    }
                    // PTT trigger from playout: the press, then its release - the
                    // order they played in. Only while somebody holds the
                    // transmitter: a frame with the PTT bit from a client that has
                    // since left - said goodbye, or timed out - keys nothing. The
                    // session's ownership table is the named decision (`accepts`);
                    // this asks the same table.
                    for frame_ptt in played.pressed.then_some(true).into_iter().chain(played.release.map(|_| false)) {
                        let owner = self.session.lock().await.tx_holder(crate::session::TxTarget::Thetis);
                        if crate::session::playout_keys(frame_ptt, owner, tx_holder_addr) {
                            self.ptt.lock().await.activate_from_playout();
                        } else if self.ptt.lock().await.is_tx_or_prefill() {
                            if crate::cw_playout::lock(&self.cw).holds_transmitter(Instant::now()) {
                                // The microphone is done, but a CW run holds the
                                // transmitter (keyed within the microphone's tail).
                                // Its tail frames must not re-arm the release that
                                // activate_for_cw cancelled: that dropped MOX in the
                                // middle of the run (found reading for review 313).
                                // The run hands MOX and the lock back itself, and
                                // always does (CwPlayout::holds_transmitter).
                                if tx_holder_addr.take().is_some() {
                                    info!("PTT: microphone done while a CW run holds the transmitter - the run hands it back");
                                }
                            } else {
                                // First non-PTT frame after TX/prefill (or a press
                                // whose holder is gone): cancel prefill, start tail
                                // delay, release session. The tail is reckoned on
                                // what stood behind that frame, as before build 288:
                                // for the release the buffer before it went
                                // (`release_depth`), for a press the buffer now.
                                let depth = if frame_ptt { jitter_buf.depth() } else { played.release_depth };
                                let mut ptt = self.ptt.lock().await;
                                ptt.cancel_prefill();
                                let started = ptt.release_from_playout(depth);
                                let tail = ptt.tail_delay();
                                drop(ptt);
                                // End the tail when it is over, not on the next
                                // 100 ms safety tick (0-100 ms late until build
                                // 103). The tick stays as the net under it. One
                                // timer per tail (round 103).
                                if started {
                                    let ptt = self.ptt.clone();
                                    tokio::spawn(async move {
                                        tokio::time::sleep(tail).await;
                                        ptt.lock().await.finish_tail_if_due().await;
                                    });
                                }
                                if let Some(addr) = tx_holder_addr.take() {
                                    self.session.lock().await.release_tx(crate::session::TxTarget::Thetis, addr);
                                }
                            }
                        // Nothing transmitted, and the holder's release frame plays:
                        // the safety let go in a silence and the operator let go in
                        // it. The ownership goes back as at any release (build 250).
                        } else if crate::session::playout_hands_back(frame_ptt, owner, tx_holder_addr) {
                            if let Some(addr) = tx_holder_addr.take() {
                                // Named in advance (working rule 11, build 250).
                                info!("PTT: {} let go while nothing was transmitted - Thetis handed back", addr);
                                self.session.lock().await.release_tx(crate::session::TxTarget::Thetis, addr);
                            }
                        }
                    }
                    thetis_tx_loss.maybe_log();
                    if let Some((in_pause, fallback)) = pause_trim.counts_to_say(Instant::now()) {
                        // Named in advance (working rule 11, builds 257 and 258).
                        dev_info!("{}", crate::tx_recover::pause_line("Thetis", in_pause, fallback));
                    }
                }

                // A radio's release frame played (build 282).
                Some(played) = released_rx.recv() => {
                    self.release_played(played).await;
                }

                // The safety task ended. On a shutdown that is its normal way
                // out; any other time it is the thing that ends transmissions
                // nobody lets go of, gone - see `safety_task_lost`. Polled once:
                // a finished JoinHandle may not be polled again.
                end = &mut safety_handle, if !safety_lost => {
                    safety_lost = true;
                    on_guard_task_end!(self, end, "the safety task");
                }
                end = &mut yaesu_state_watch, if !yaesu_state_lost => {
                    yaesu_state_lost = true;
                    on_guard_task_end!(self, end, "the state task of Yaesu radio 0 (its orphan watch)");
                }
                end = &mut yaesu2_state_watch, if !yaesu2_state_lost => {
                    yaesu2_state_lost = true;
                    on_guard_task_end!(self, end, "the state task of Yaesu radio 1 (its orphan watch)");
                }

                _ = shutdown.changed() => {
                    info!("Network RX loop shutting down");
                    // Notify all active clients
                    let addrs = self.session.lock().await.active_addrs();
                    let mut buf = [0u8; DisconnectPacket::SIZE];
                    DisconnectPacket::serialize(&mut buf);
                    for addr in &addrs {
                        let _ = self.socket.send_to(&buf, *addr).await;
                        info!("Sent disconnect to client {}", addr);
                    }
                    break;
                }
            }
        }

        tx_handle.abort();
        safety_handle.abort();
        yaesu_state_watch.abort();
        yaesu2_state_watch.abort();
        tci_iq_handle.abort();
        Ok(())
    }
}

// Audio loops, resampling helpers, and IQ consumer moved to audio_loops.rs

// Re-export for use within this file (Yaesu TX decode task uses resample_to_device)
use crate::audio_loops::resample_to_device;

/// The key lines of the two Yaesu slots, shared by the receive path (which
/// opens them), the playout's send task and the release hooks.
/// The Yaesu key lines of one server run, and whether that run has ended.
///
/// The end lives here, with the lines, and not in a static: the Settings
/// button stops a server and starts the next one in the same process, and a
/// process-wide flag set by the first one silenced every Yaesu edge of the
/// second - no keying and no warning (review 317, both reviewers).
pub(crate) struct KeyLineSet<L> {
    pub(crate) slots: [Option<L>; 2],
    /// Set by `lower_all_key_lines`: the lines are down and stay down, and an
    /// edge the playout still plays after it is dropped without a word.
    pub(crate) closed_at_exit: bool,
}

impl<L> KeyLineSet<L> {
    pub(crate) fn new() -> Self {
        Self { slots: [None, None], closed_at_exit: false }
    }
}

pub(crate) type CwKeyLines = Arc<std::sync::Mutex<KeyLineSet<crate::cw_key_line::KeyLine>>>;

/// Where a CW edge goes - the one place that knows. Thetis: `keyer:` over TCI,
/// and a played edge feeds its packet watchdog (never the heartbeat, which only
/// the client can give). A Yaesu slot: RTS on its key line. Used by the send
/// task, the goodbye and the time-out, so a key-up always reaches the radio
/// the run keyed.
/// The one place that writes to a key line, and since build 154 that is true:
/// every edge - the playout's own, the watchdog's, a time-out's, a goodbye's -
/// arrives through `NetworkService::cw_due` and is written by the task that
/// drains it. The ratchet `cw_output call sites` in `fitness.ps1` keeps it at
/// one; a second caller is a second writer, and two writers have no order.
pub(crate) async fn cw_output(
    ptt: &Arc<Mutex<PttController>>,
    lines: &CwKeyLines,
    target: u8,
    down: bool,
    len_ms: Option<u16>,
) {
    if target == 0 {
        let mut p = ptt.lock().await;
        p.record_cw_activity();
        p.cw_key(down, len_ms).await;
        return;
    }
    let slot = usize::from(target - 1);
    let mut g = lines.lock().unwrap_or_else(|e| e.into_inner());
    yaesu_key_edge(&mut g, slot, down, len_ms);
}

/// One edge on the key line of a Yaesu slot. A line whose write fails is
/// dropped (its `Drop` still tries to lower both lines), so the next packet
/// opens the port again and prepares the radio anew; the other slot is left
/// alone. Separate from `cw_output` so it can be tested with a fake line.
pub(crate) fn yaesu_key_edge<L: crate::cw_key_line::KeyOut>(lines: &mut KeyLineSet<L>, slot: usize, down: bool, len_ms: Option<u16>) {
    if lines.closed_at_exit {
        // The playout thread outlives the network loop by a moment and still
        // plays what it had queued; the lines are down on purpose. Nothing to
        // say - it read as a fault in every exit log (field test 2026-09-19).
        return;
    }
    let g = &mut lines.slots;
    let failed = match g.get_mut(slot).and_then(|l| l.as_mut()) {
        Some(line) => {
            // The same line per edge as Thetis's "TCI: CW key DOWN/UP", so a field
            // log shows what went to the radio.
            info!("cw: Yaesu slot {} key {} dur={:?} ({})", slot, if down { "DOWN" } else { "UP" }, len_ms, line.name());
            match line.set(down) {
                Ok(()) => false,
                Err(e) => {
                    warn!("cw: key line {} of Yaesu slot {} - setting RTS {} failed: {}", line.name(), slot, if down { "high" } else { "low" }, e);
                    true
                }
            }
        }
        None => {
            warn!("cw: an edge for Yaesu slot {} but its key line is not open", slot);
            false
        }
    };
    if failed {
        // A handle that fails is a port that went away (the radio's USB). Kept,
        // it would count as ready for ever and every run would key nothing;
        // dropped, the next run finds the port again and prepares the radio anew.
        //
        // But not before one explicit attempt to put the line down. A failed
        // key-UP is a radio that is transmitting, and after this the playout
        // believes the key is up: no further edge comes, so nothing would try
        // again (review 2120A1). The drop writes too, on this same
        // handle, but it discards what the port said - here we can say it.
        if let Some(mut line) = g.get_mut(slot).and_then(|l| l.take()) {
            match line.lower_now() {
                Ok(()) => warn!("cw: key line {} of Yaesu slot {} put down and dropped - the next run opens it again", line.name(), slot),
                // Which edge failed decides what this means, and the operator is
                // the one who acts on it. A key-DOWN that failed leaves a radio
                // that was not keyed by us; sending someone to the shack for that
                // is an alarm for nothing (review 2120A1 round 2 - and this
                // project has just finished repairing a line that claimed to know
                // something it did not).
                // A key-UP that failed is the dangerous one: we had the radio
                // keyed and could not let go.
                Err(e) if !down => error!(
                    "cw: key line {} of Yaesu slot {} could not be put down ({}) - this radio may still be transmitting; take it off the air by hand",
                    line.name(), slot, e
                ),
                // A key-DOWN that failed means the keying never reached the
                // radio, so there is nothing on the air to go and switch off.
                Err(e) => warn!(
                    "cw: key line {} of Yaesu slot {} could not be put down ({}) - it was not keyed by us, so the radio is not left transmitting",
                    line.name(), slot, e
                ),
            }
        }
    }
}

/// The steps of admitting a CW packet to a Yaesu, each with its effect. A
/// trait so the order - and "a refusal leaves nothing behind" - can be tested
/// with a fake that writes down what happened. This one function produced a
/// finding in three review rounds in a row (125 the slot, 128 two spellings of
/// the slot rule, 135 the mark); review 317 asked for this shape, the one
/// `ExitRadio` (315) and `KeyOut` (316) already have.
pub(crate) trait YaesuCwAdmission {
    /// Why the radio's status refuses CW now (model, off, not in CW), if so.
    fn radio_refusal(&self) -> Option<String>;
    /// Take the transmitter for this attempt: `None` when another client holds
    /// it, else whether this client held it already.
    async fn take_slot(&mut self) -> Result<bool, &'static str>;
    /// Give back what `take_slot` took - only that.
    async fn give_back_slot(&mut self, held_before: bool);
    /// Is the key line of this radio open (and menu 060 set with it)?
    fn line_open(&self) -> bool;
    /// Open it and prepare the radio; nothing is kept when this fails.
    async fn open_line(&mut self) -> Result<(), String>;
    /// Mark the radio as keyed by CW; false while its channels are walked.
    fn mark_keying(&mut self) -> bool;
}

/// Admit a CW packet to a Yaesu. A refusal at any step leaves nothing behind:
/// no slot (unless this client held it before), no mark. The mark comes last:
/// it tells the tone read, every client and the PA guard that we key this
/// radio (review 317).
pub(crate) async fn admit_yaesu_cw_steps<A: YaesuCwAdmission>(a: &mut A) -> Result<(), String> {
    if let Some(why) = a.radio_refusal() {
        return Err(why);
    }
    let held_before = a.take_slot().await?;
    if !a.line_open() {
        if let Err(why) = a.open_line().await {
            a.give_back_slot(held_before).await;
            return Err(why);
        }
    }
    if !a.mark_keying() {
        a.give_back_slot(held_before).await;
        return Err("the radio is stepping through its memory channels".into());
    }
    Ok(())
}

/// The real steps, on the network service.
struct ServiceAdmission<'a> {
    svc: &'a NetworkService,
    addr: SocketAddr,
    tx: crate::session::TxTarget,
    slot: usize,
    radio: Arc<crate::yaesu::YaesuRadio>,
    cat_port: Option<String>,
}

impl YaesuCwAdmission for ServiceAdmission<'_> {
    fn radio_refusal(&self) -> Option<String> {
        let st = self.radio.status();
        yaesu_cw_refusal(st.connected, st.power_on, st.mode)
            .or_else(|| st.tx_start_refusal())
            .map(str::to_string)
    }
    async fn take_slot(&mut self) -> Result<bool, &'static str> {
        let mut s = self.svc.session.lock().await;
        match s.acquire_for_attempt(self.tx, self.addr) {
            Some(held) => Ok(held),
            None => Err(s.acquire_refusal().why),
        }
    }
    async fn give_back_slot(&mut self, held_before: bool) {
        self.svc.session.lock().await.give_back_attempt(self.tx, self.addr, held_before);
    }
    fn line_open(&self) -> bool {
        self.svc.cw_lines.lock().unwrap_or_else(|e| e.into_inner()).slots[self.slot].is_some()
    }
    async fn open_line(&mut self) -> Result<(), String> {
        self.svc.open_yaesu_key_line(&self.radio, self.slot, self.cat_port.as_deref()).await
    }
    fn mark_keying(&mut self) -> bool {
        // Under the YaesuState lock, like ChannelWalk::begin: the two cannot
        // both start.
        self.radio.status_arc().lock().unwrap_or_else(|e| e.into_inner()).begin_cw_keying(Instant::now())
    }
}

#[cfg(test)]
mod tx_line_tests {
    /// The decision on an arriving transmit packet and its four ways out are
    /// one block for Thetis's arm and the radios' (build 286; round 285: in
    /// Thetis's arm `Pass => {}`, a `Stale` that played on and a refused claim
    /// that played on all stayed green). The gate: each way out ends the
    /// packet. Its users: exactly the two arms, once each, and Thetis's arm
    /// has no way out of its own - so nothing it reads after the gate can be
    /// skipped or reached around it. And Thetis's `keyed` is its holder (round
    /// 285: `false` would log every relay duplicate as late). A text
    /// guard: the receive loop runs no test; the decision is tested in
    /// `tx_intake`.
    #[test]
    fn both_arms_go_through_the_one_gate() {
        let src = include_str!("network.rs");
        let code = &sdr_remote_core::source_text::production_code(src);
        let at = code.find("        macro_rules! intake_gate {").expect("the gate");
        let gate = &code[at..at + code[at..].find("\n        }\n").unwrap()];
        for way_out in [
            "crate::tx_intake::Intake::Pass => continue,",
            "crate::tx_intake::Intake::Stale => {\n                        if senders.ignored(target) {",
            "crate::tx_intake::Intake::LetGoByRadio => {\n                        if senders.ignored(target) {",
            "let radio_refusal: Option<&'static str> = $radio_refusal;\n                        if !self.claim_tx(target, addr, radio_refusal, &mut denied).await {\n                            continue;\n                        }\n                        true",
            "crate::tx_intake::Intake::Play => false,",
        ] {
            assert_eq!(gate.matches(way_out).count(), 1, "in the gate: {way_out}");
        }
        assert_eq!(gate.matches("continue").count(), 4, "nothing, too late, let go by the radio, refused");
        // No lock read inside the claim's condition (round 286): `$radio_refusal`
        // only in its own statement.
        assert_eq!(gate.matches("$radio_refusal").count(), 2, "the parameter and its one statement");
        assert!(!gate.contains("claim_tx(target, addr, $radio_refusal"));
        assert_eq!(code.matches("intake_gate!(").count(), 2, "Thetis's arm and the radios'");
        let thetis = &code[code.find("Packet::Audio(audio_pkt) => {").unwrap()..];
        let thetis = &thetis[..thetis.find("Packet::Heartbeat(hb) => {").unwrap()];
        assert_eq!(thetis.matches("intake_gate!(target, addr, audio_pkt.sequence, arrival, \"Thetis\", None)").count(), 1);
        assert_eq!(thetis.matches("continue").count(), 0, "no way out of its own");
        assert_eq!(thetis.matches("keyed: holder == Some(addr),").count(), 1, "Thetis keys when its holder's audio plays");
        let at = code.find("        macro_rules! radio_audio {").unwrap();
        let radios = &code[at..at + code[at..].find("\n        }\n").unwrap()];
        assert_eq!(radios.matches("let claimed = intake_gate!(").count(), 1);
        // What both arms give the decision, and what they keep of a packet
        // that plays (round 286: the holder's own, the filter, the newest -
        // each one word from a second client playing along or the filter off).
        for (arm, seq) in [(thetis, "audio_pkt.sequence"), (radios, "pkt.sequence")] {
            assert_eq!(arm.matches("from_holder: holder == Some(addr),").count(), 1, "{seq}: from its holder");
            assert_eq!(arm.matches("\n                    stale,\n").count() + arm.matches("\n                                stale,\n").count(), 1, "{seq}: the filter's answer");
            let played = format!("if !stale {{\n");
            assert_eq!(arm.matches(&played).count(), 1, "{seq}: the newest, only when not late");
            assert_eq!(arm.matches(&format!("senders.played(target, addr, {seq}, Instant::now());")).count(), 1, "{seq}: the newest kept");
        }
    }
    /// Thetis's transmit buffer keeps what it learned for the same sender
    /// pressing again, and forgets it for another sender, a goodbye or a lost
    /// guard task - as a Yaesu's (owner, 2026-10-11, choice a; build 175). A
    /// text guard: the receive loop runs no test.
    #[test]
    fn the_same_sender_keeps_what_thetis_learned() {
        let network = include_str!("network.rs");
        let network = &sdr_remote_core::source_text::production_code(network);
        assert!(network.contains(
            "if ptt_requested && tx_holder_addr != Some(addr) {\n                                if line_learned_from == Some(addr) {\n                                    // Named in advance (working rule 11, build 175).\n                                    info!(\"TX holder {} again, keeping what the jitter buffer learned\", addr);\n                                    jitter_buf.discard();\n                                } else {\n                                    info!(\"New TX holder {}, resetting jitter buffer and decoder\", addr);\n                                    jitter_buf.reset();\n                                    line_learned_from = Some(addr);"
        ));
        // Forgetting the holder is one macro (build 248), and its users are
        // counted: a goodbye, a lost guard task and a holder the session let go.
        assert_eq!(network.matches("line_learned_from = None;").count(), 1, "only in the macro");
        assert_eq!(network.matches("line_learned_from = ").count(), 2);
        assert!(network.contains(
            "        macro_rules! forget_thetis_holder {\n            () => {\n                jitter_buf.reset();\n                tx_holder_addr = None;\n                line_learned_from = None;\n            };\n        }"
        ));
        assert_eq!(network.matches("forget_thetis_holder!();").count(), 3);
        assert_eq!(network.matches("tx_holder_addr = None;").count(), 1, "only in the macro");
    }

    /// A PTT packet from a client while the loop still remembers a holder the
    /// session no longer has (a time-out, build 247): the loop forgets it as at
    /// a goodbye - buffer, holder, what it learned - before it asks for the
    /// transmitter, and so before the new holder's branch. Asked after the
    /// acquisition, the session would say "held" again. A text guard: the
    /// receive loop runs no test.
    #[test]
    fn a_holder_that_timed_out_is_forgotten_before_it_holds_again() {
        let network = include_str!("network.rs");
        let network = &sdr_remote_core::source_text::production_code(network);
        let arm = network.find("Packet::Audio(audio_pkt) => {").unwrap();
        let arm = &network[arm..arm + 6000];
        let forget = "forget_thetis_holder_if_outlived!(session.tx_holder(crate::session::TxTarget::Thetis));";
        let at = arm.find(forget).expect("the forgetting, in the audio arm");
        // The claim is `claim_tx` since build 285, inside the one gate since 286.
        let acquire = arm.find("if intake_gate!(target, addr, audio_pkt.sequence, arrival, \"Thetis\", None) {").unwrap();
        let new_holder = arm.find("if ptt_requested && tx_holder_addr != Some(addr) {").unwrap();
        assert!(at < acquire && acquire < new_holder, "forget, then acquire, then the new holder's branch");
        // The question and its line, in one place (build 248).
        assert!(network.contains(
            "                if let Some(old) = tx_holder_addr.filter(|_| crate::session::holder_outlived(tx_holder_addr, $holder_now)) {\n                    // Named in advance (working rule 11, build 247).\n                    info!(\"TX holder {} lost the transmitter while silent - its buffered audio goes, as at a goodbye\", old);\n                    forget_thetis_holder!();\n                }"
        ));
        assert_eq!(network.matches("forget_thetis_holder_if_outlived!(").count(), 2, "the audio arm and the CW gate");
    }

    /// The safety's release takes the Thetis ownership with it, in the same
    /// tick (build 251; rounds 249 and 250): only when `check_safety` released,
    /// and through the session's own block. A text guard: the safety task runs
    /// no test.
    #[test]
    fn the_safety_release_takes_the_thetis_ownership_with_it() {
        let network = include_str!("network.rs");
        let network = &sdr_remote_core::source_text::production_code(network);
        assert!(network.contains(
            "if ptt.lock().await.check_safety().await {\n                                if let Some(holder) = session.lock().await.release_after_safety() {\n                                    // Named in advance (working rule 11, build 251).\n                                    info!(\"SAFETY: {} held Thetis when its transmitter was released - the ownership goes with it\", holder);\n                                }\n                            }\n                            // The 2 s clock of each Yaesu, in the same tick as"
        ), "the safety tick hands the ownership back when it released");
        assert_eq!(network.matches("check_safety().await").count(), 1, "one caller");
        assert_eq!(network.matches("release_after_safety()").count(), 1, "one place");
    }

    /// A release frame played while nothing is transmitted - after a safety
    /// release in a silence - hands Thetis back, as the release while
    /// transmitting does (build 250; round 249). The branch comes after the
    /// transmitting one, so it is asked only when nothing is transmitted. A text
    /// guard: the playout tick runs no test.
    #[test]
    fn a_release_while_nothing_transmits_hands_thetis_back() {
        let network = include_str!("network.rs");
        let network = &sdr_remote_core::source_text::production_code(network);
        // Since build 288 per played press and release (`play_tick`), in that order.
        let tick = network.find("for frame_ptt in played.pressed.then_some(true).into_iter().chain(played.release.map(|_| false)) {").unwrap();
        let tick = &network[tick..tick + 5000];
        assert!(tick.contains("if crate::session::playout_keys(frame_ptt, owner, tx_holder_addr) {"));
        let transmitting = tick.find("} else if self.ptt.lock().await.is_tx_or_prefill() {").unwrap();
        let back = tick.find("} else if crate::session::playout_hands_back(frame_ptt, owner, tx_holder_addr) {\n                            if let Some(addr) = tx_holder_addr.take() {\n                                // Named in advance (working rule 11, build 250).\n                                info!(\"PTT: {} let go while nothing was transmitted - Thetis handed back\", addr);\n                                self.session.lock().await.release_tx(crate::session::TxTarget::Thetis, addr);\n                            }\n                        }").expect("the hand-back branch");
        // The tail on what stood behind the release frame, as before 288.
        assert!(tick.contains("let depth = if frame_ptt { jitter_buf.depth() } else { played.release_depth };"));
        assert!(transmitting < back, "asked only when nothing is transmitted");
    }

    /// What Thetis's playout tick does with a played press and release, in
    /// order (build 289; round 288: keying, the cancelled prefill and the
    /// ownership going back were pinned by nothing - each one line from Thetis
    /// never keying on the microphone, a short press keying it after the
    /// release, or a client that keeps Thetis after it let go). A text guard:
    /// the tick runs no test; the tail is pinned in `ptt.rs`.
    #[test]
    fn thetis_keys_and_lets_go_in_its_tick() {
        let network = include_str!("network.rs");
        let network = &sdr_remote_core::source_text::production_code(network);
        let at = network.find("for frame_ptt in played.pressed.then_some(true).into_iter().chain(played.release.map(|_| false)) {").unwrap();
        let tick = &network[at..at + 5000];
        let order = [
            "if crate::session::playout_keys(frame_ptt, owner, tx_holder_addr) {
                            self.ptt.lock().await.activate_from_playout();
                        } else if self.ptt.lock().await.is_tx_or_prefill() {",
            "ptt.cancel_prefill();
                                let started = ptt.release_from_playout(depth);",
            "if let Some(addr) = tx_holder_addr.take() {
                                    self.session.lock().await.release_tx(crate::session::TxTarget::Thetis, addr);
                                }",
            "} else if crate::session::playout_hands_back(frame_ptt, owner, tx_holder_addr) {",
        ];
        let mut last = 0;
        for what in order {
            let here = tick.find(what).unwrap_or_else(|| panic!("in the tick: {what}"));
            assert!(here >= last, "in order: {what}");
            last = here;
        }
        assert_eq!(network.matches("activate_from_playout()").count(), 1, "one place keys Thetis from playout");
        assert_eq!(tick.matches("cancel_prefill()").count(), 1);
    }

    /// The CW gate takes Thetis too (`acquire_for_attempt`): it asks the same
    /// question first, so a holder back with CW after a time-out does not find
    /// its old buffer playable (build 248; round 247).
    #[test]
    fn the_cw_gate_forgets_a_holder_that_timed_out_before_it_takes_thetis() {
        let network = include_str!("network.rs");
        let network = &sdr_remote_core::source_text::production_code(network);
        let gate = network.find("let mut s = self.session.lock().await;\n                                    forget_thetis_holder_if_outlived!(s.tx_holder(crate::session::TxTarget::Thetis));\n                                    match s.acquire_for_attempt(crate::session::TxTarget::Thetis, addr) {");
        assert!(gate.is_some(), "asked in the CW gate, before the attempt takes Thetis");
    }

    /// A silence past the safety's packet window inside a press (build 248):
    /// what the buffer holds is from before it and goes before the next packet
    /// of the holder goes in - with PTT or without (build 249: a release packet
    /// first after the silence played the old audio too, round 248). The
    /// silence is read for every audio packet, before the session's lock, and
    /// the buffer emptied before the split into PTT and release, so before
    /// either push. A text guard: the receive loop runs no test.
    #[test]
    fn a_silence_inside_a_press_empties_the_buffer_before_the_next_packet() {
        let network = include_str!("network.rs");
        let network = &sdr_remote_core::source_text::production_code(network);
        // The whole arm, to the next one (it grew past a fixed window in build 285).
        let arm = &network[network.find("Packet::Audio(audio_pkt) => {").unwrap()..];
        let arm = &arm[..arm.find("Packet::Heartbeat(hb) => {").unwrap()];
        let read = arm.find("let silence = self.ptt.lock().await.packet_silence(Instant::now());").expect("the silence, read for every packet");
        let lock = arm.find("let mut session = self.session.lock().await;").unwrap();
        let ends = arm.find("if tx_holder_addr == Some(addr) && crate::ptt::silence_ended_the_over(silence) {\n                                // Named in advance (working rule 11, builds 248 and 249).\n                                info!(\"TX holder {} silent for {} ms, past the packet window - its buffered audio goes\", addr, silence.map_or(0, |d| d.as_millis()));\n                                jitter_buf.discard();\n                            }").expect("the silence empties the buffer, whatever the packet");
        // Since build 285 one decision (`tx_intake`) and one push for the press
        // and the release, the frame carrying the packet's own flag.
        let split = arm.find("if intake_gate!(target, addr, audio_pkt.sequence, arrival, \"Thetis\", None) {").unwrap();
        let push = arm.find("jitter_buf.push(").unwrap();
        assert!(read < lock && lock < ends && ends < split && split < push, "read, lock, empty, then the decision and the push");
        assert_eq!(arm.matches("jitter_buf.push(").count(), 1, "one push");
        assert!(arm.contains("ptt: ptt_requested,\n                                    wideband,"), "with the packet's flag");
        assert!(!arm.contains("silence_ended_the_over(silence) {\n                                // Named in advance (working rule 11, build 248)."), "no PTT-only rule left");
    }
}

#[cfg(test)]
mod mode_refusal_tests {
    /// A mode change refused during TX sends the current mode back to that
    /// client, so a screen that already shows the asked-for mode is put right
    /// (owner, 2026-10-11, choice a; build 174). A text guard: the receive
    /// loop runs no test.
    #[test]
    fn a_refused_mode_change_sends_the_current_mode_back() {
        let network = include_str!("network.rs");
        let network = &sdr_remote_core::source_text::production_code(network);
        assert_eq!(
            network
                .matches("                            if ptt.is_transmitting() {\n                                // The client already shows the mode it asked for,")
                .count(),
            1
        );
        assert_eq!(
            network
                .matches("let mode = ptt.vfo_a_mode();\n                                drop(ptt);\n                                let mut buf = [0u8; ModePacket::SIZE];\n                                ModePacket { mode }.serialize(&mut buf);\n                                let _ = self.socket.send_to(&buf, addr).await;")
                .count(),
            1
        );
        // And the line named in advance (review of 174, M1/M2).
        assert_eq!(network.matches("info!(\"Client {} VFO-A mode change ignored during TX - sent mode {} back\", addr, mode);").count(), 1);
        // The probe's wiring (review of 174, S5).
        assert_eq!(network.matches("if tx_probe.wants(probe_is_tx) {").count(), 1);
    }
}

#[cfg(test)]
mod memory_copy_tests {
    use super::memory_copy_has_channels;

    /// A copy with only the header - a cold FTX-1's empty scan - does not
    /// stand in for a read; one channel does (build 170; review of 169).
    #[test]
    fn an_empty_memory_copy_is_read_again() {
        let header = sdr_remote_core::YAESU_MEMORY_TAB_HEADER;
        assert!(!memory_copy_has_channels(None));
        assert!(!memory_copy_has_channels(Some(&format!("{header}
"))));
        assert!(!memory_copy_has_channels(Some(&format!("{header}

"))));
        assert!(memory_copy_has_channels(Some(&format!("{header}
001	145500000
"))));
        // Both handlers ask it (a text guard).
        let network = include_str!("network.rs");
        let network = &sdr_remote_core::source_text::production_code(network);
        assert_eq!(network.matches("ctrl.value == 1 && memory_copy_has_channels(copy.as_deref())").count(), 2);
        assert_eq!(network.matches("last_memory_blob.is_some()").count(), 0);
        // Each line with its own case (review of 170: D3).
        assert_eq!(network.matches("if ctrl.value == 1 && copy.is_none() {\n                                                warn!(\"Client {} wanted the server's copy of the Yaesu memories, but there is none").count(), 1);
        assert_eq!(network.matches("if ctrl.value == 1 && copy.is_none() {\n                                                warn!(\"Client {} [radio1] wanted the server's copy of the memories, but there is none").count(), 1);
        assert_eq!(network.matches("but it has no channels - reading the radio").count(), 2);
    }
}

#[cfg(test)]
mod admission_tests {
    use super::{admit_yaesu_cw_steps, YaesuCwAdmission};

    /// A radio and a session that write down what was done to them.
    #[derive(Default)]
    struct Fake {
        refuse: bool,
        held_by_other: bool,
        held_before: bool,
        line_open: bool,
        line_fails: bool,
        walking: bool,
        // what is left behind
        slot_taken: bool,
        line_opened: bool,
        marked: bool,
    }

    impl YaesuCwAdmission for Fake {
        fn radio_refusal(&self) -> Option<String> {
            self.refuse.then(|| "the radio is not in a CW mode".to_string())
        }
        async fn take_slot(&mut self) -> Result<bool, &'static str> {
            if self.held_by_other {
                return Err("another client holds it");
            }
            self.slot_taken = true;
            Ok(self.held_before)
        }
        async fn give_back_slot(&mut self, held_before: bool) {
            if !held_before {
                self.slot_taken = false;
            }
        }
        fn line_open(&self) -> bool {
            self.line_open
        }
        async fn open_line(&mut self) -> Result<(), String> {
            if self.line_fails {
                return Err("no key port".into());
            }
            self.line_opened = true;
            Ok(())
        }
        fn mark_keying(&mut self) -> bool {
            if self.walking {
                return false;
            }
            self.marked = true;
            true
        }
    }

    fn run(f: &mut Fake) -> Result<(), String> {
        tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(admit_yaesu_cw_steps(f))
    }

    #[test]
    fn a_refusal_leaves_nothing_behind() {
        for (name, mut f) in [
            ("the radio refuses", Fake { refuse: true, ..Default::default() }),
            ("another client holds it", Fake { held_by_other: true, ..Default::default() }),
            ("the key port fails", Fake { line_fails: true, ..Default::default() }),
            ("its channels are walked", Fake { line_open: true, walking: true, ..Default::default() }),
        ] {
            assert!(run(&mut f).is_err(), "{name}: refused");
            assert!(!f.slot_taken, "{name}: the slot went back");
            assert!(!f.marked, "{name}: no mark");
            assert!(!f.line_opened, "{name}: no line");
        }
    }

    /// A key line opened for an attempt that the walk then refuses stays open:
    /// it is the radio's preparation (menu 060 on RTS, put back at exit), not
    /// a transmission - both lines low, no slot, no mark - and the next packet
    /// uses it. Written out because review 318 found the earlier test
    /// claiming otherwise in a clause that could not fire.
    #[test]
    fn a_prepared_line_is_kept_when_the_walk_refuses() {
        let mut f = Fake { walking: true, ..Default::default() };
        assert!(run(&mut f).is_err());
        assert!(f.line_opened, "prepared, kept");
        assert!(!f.slot_taken && !f.marked, "and nothing that transmits");
    }

    #[test]
    fn a_slot_this_client_held_before_stays_its_own() {
        let mut f = Fake { held_before: true, line_fails: true, ..Default::default() };
        assert!(run(&mut f).is_err());
        assert!(f.slot_taken, "the microphone's slot is not taken away");
        assert!(!f.marked);
    }

    #[test]
    fn an_admitted_packet_has_its_slot_its_line_and_its_mark() {
        let mut f = Fake::default();
        assert!(run(&mut f).is_ok());
        assert!(f.slot_taken && f.line_opened && f.marked);
        // The second packet of the run: the line is open already.
        let mut f = Fake { line_open: true, ..Default::default() };
        assert!(run(&mut f).is_ok());
        assert!(f.slot_taken && !f.line_opened && f.marked, "not opened twice");
    }
}

pub(crate) use key_lines::{lower_all_key_lines, KeyLinesDown};

/// The key lines go down in here, and the proof that they did is made in here
/// only. A private field is private to its MODULE, and `network.rs` is one
/// module: a `KeyLinesDown(())` written next to `safety_task_lost` compiled, with
/// the clients told before the lines went down - the order build 63's anchor pair
/// still caught (review 64, both reviewers). Nothing else lives here, so the
/// constructor is out of reach of everything that has to come after it.
mod key_lines {
    use super::*;

    /// Proof that `lower_all_key_lines` has run: the Yaesu key lines are down and the
    /// set is closed. Made only in this module. What has to happen AFTER the lines are down
    /// (telling the clients at a loss) takes one by value, so the compiler keeps the
    /// order (review 63: an anchor on two lines kept their order to each other, not
    /// their place in the function).
    #[must_use]
    pub(crate) struct KeyLinesDown(());

    /// Lower every Yaesu key line now: the end of the server, before the radios are
    /// put back. Taking a line out of its slot drops it, and its `Drop` lowers RTS
    /// and DTR. Until review 316 this happened only when the runtime was torn down,
    /// after the put-back's wait of up to 2 s - harmless once menu 060 is back on
    /// OFF, but not on a radio that was on RTS before this server.
    pub(crate) fn lower_all_key_lines(lines: &CwKeyLines) -> KeyLinesDown {
        let mut g = lines.lock().unwrap_or_else(|e| e.into_inner());
        // Under the lock, so an edge that waits for it finds the set closed.
        g.closed_at_exit = true;
        for (slot, l) in g.slots.iter_mut().enumerate() {
            if let Some(mut line) = l.take() {
                // Lower first, then say what the port said. Until build 152 this
                // line was printed from inside the `if let`, one statement before
                // the drop that does the writing, and the drop threw its result
                // away: the line said "lowered at exit" whether or not RTS had
                // moved (review 2120A1). It is the line the card and the
                // release notes lean on, and under it there is no net - a Yaesu
                // has no keying watchdog of its own.
                match line.lower_now() {
                    Ok(()) => info!("cw: key line {} of Yaesu slot {} lowered at exit", line.name(), slot),
                    Err(e) => error!(
                        "cw: key line {} of Yaesu slot {} did NOT go down at exit ({}) - check the radio is not transmitting",
                        line.name(), slot, e
                    ),
                }
            }
        }
        KeyLinesDown(())
    }
}

/// Why the safety task's end is a loss, or `None` when it is not: on a
/// shutdown it ends by design. `panicked` is `None` when it ended by itself.
fn safety_task_end(panicked: Option<bool>, shutting_down: bool) -> Option<&'static str> {
    if shutting_down {
        return None;
    }
    Some(match panicked {
        Some(true) => "panic",
        Some(false) => "cancelled",
        None => "it ended without a shutdown",
    })
}

/// A task to watch in the main loop's `select!`; a slot without one gets a
/// task that never ends, so the branch stays the same shape.
fn watched(handle: Option<tokio::task::JoinHandle<()>>) -> tokio::task::JoinHandle<()> {
    handle.unwrap_or_else(|| tokio::spawn(std::future::pending::<()>()))
}

/// The transmitter a refusal names, for a transmitter of the session.
fn denied_target_of(target: crate::session::TxTarget) -> DeniedTarget {
    match target {
        crate::session::TxTarget::Thetis => DeniedTarget::Thetis,
        crate::session::TxTarget::Yaesu1 => DeniedTarget::Radio1,
        crate::session::TxTarget::Yaesu2 => DeniedTarget::Radio2,
    }
}

/// The safety nets of one Yaesu, run by its status task on every tick (build
/// 289, step 1c of `KOPPELING` 18; until then a copy in each of the two
/// tasks). In this order, as both copies had it:
/// - the radio let go by itself (`auto_release`): its own time-out timer, or
///   the radio stopping. Unkeying it is half the job. Without the rest the
///   session still has the client down as the holder and the client still
///   believes it transmits, so it keeps sending TX audio at a radio that is
///   off and its button stays red - the complaint from 2026-08-07, which
///   survived five attempts at fixing it on the client side (2026-09-05);
/// - the watchdog: the owner is gone but the radio is still keyed. Nothing
///   else would end this: it would transmit until its own time-out timer
///   expires, and on a 991A that timer is often off. Thetis has its own
///   dead-man switch; this is the Yaesu's.
///
/// The 2 s clock of a silent holder ran here too until build 290; it runs in
/// the 100 ms safety tick now, beside Thetis's (owner, 2026-10-23). The
/// watchdog looks twice before it fires (`OrphanWatch`), so a clock in
/// another task that is half way through its release is not taken for an
/// orphan.
struct RadioSafety {
    target: crate::session::TxTarget,
    /// The last `auto_release` seen from the poller. A counter and not a
    /// flag: two of these between ticks must not collapse into one, and one
    /// must not be seen twice.
    last_auto_release: u32,
    orphan: crate::tx_orphan::OrphanWatch,
}

impl RadioSafety {
    fn new(target: crate::session::TxTarget) -> Self {
        Self { target, last_auto_release: 0, orphan: crate::tx_orphan::OrphanWatch::default() }
    }

    async fn tick(
        &mut self,
        session: &Mutex<SessionManager>,
        socket: &crate::tracked_socket::TrackedSocket,
        radio: Option<&crate::yaesu::YaesuRadio>,
        ptt_flag: &std::sync::atomic::AtomicBool,
        tx_holder: Option<SocketAddr>,
    ) {
        let index = radio_index(self.target);
        if let Some(y) = radio {
            let seen = y.status().auto_release;
            if seen != self.last_auto_release {
                self.last_auto_release = seen;
                if let Some(holder) = tx_holder {
                    // Its packets still carry the flag until it hears of this,
                    // and must not key the radio again (build 282); and the
                    // radio is off, so the next press keys it.
                    session.lock().await.let_go_by_the_radio(self.target, holder);
                    ptt_flag.store(false, Ordering::Relaxed);
                    let mut buf = [0u8; PttDeniedPacket::SIZE];
                    PttDeniedPacket::serialize(&mut buf, denied_target_of(self.target), Denial::ServerReleased);
                    let sent = socket.try_send_to(&buf, holder);
                    // Whether it actually went out, not just that we tried.
                    // `try_send_to` returns a Result and it was being
                    // discarded, so a refusal from the socket looked exactly
                    // like a delivery.
                    info!(
                        "Yaesu radio {} released by the server (time-out timer or the radio stopping) - told {} ({})",
                        index,
                        holder,
                        match sent {
                            Ok(n) => format!("{n} bytes sent"),
                            Err(e) => format!("SEND FAILED: {e}"),
                        },
                    );
                }
            }
        }
        if self.orphan.observe(ptt_flag.load(Ordering::Relaxed), tx_holder.is_some()) {
            if let Some(y) = radio {
                log::error!("SAFETY: Yaesu radio {} still keyed with no client holding it - releasing PTT", index);
                y.send_command(crate::yaesu::YaesuCmd::SetPtt(false));
            }
            ptt_flag.store(false, Ordering::Relaxed);
        }
    }
}

/// The 2 s clock of a radio, from its status task (build 265): a holder not
/// heard past the packet window lets go (`release_silent_radio`) and the radio
/// goes off at once - no asked release, so no waiting for its buffer. Its
/// next packet with the flag keys it again (build 282). Only while
/// the server keyed it: a CW run keys a radio on its own line, with its own
/// watchdogs. Mirrored in the two status tasks, like the orphan watch.
async fn let_a_silent_holder_go(
    session: &Mutex<SessionManager>,
    target: crate::session::TxTarget,
    radio: Option<&crate::yaesu::YaesuRadio>,
    ptt_flag: &std::sync::atomic::AtomicBool,
) {
    if !ptt_flag.load(Ordering::Relaxed) {
        return;
    }
    let Some((holder, silence)) = session.lock().await.release_silent_radio(target, Instant::now()) else {
        return;
    };
    // Named in advance (working rule 11, build 265).
    warn!(
        "SAFETY: Yaesu radio {}: nothing from its holder {} for {} ms (window {} ms) - PTT off, back on if it returns inside the press",
        radio_index(target),
        holder,
        silence.as_millis(),
        crate::ptt::PTT_PACKET_TIMEOUT_MS
    );
    if let Some(y) = radio {
        y.send_command(crate::yaesu::YaesuCmd::SetPtt(false));
    }
    ptt_flag.store(false, Ordering::Relaxed);
}

/// The index a radio has in the server's own log lines ("Yaesu radio 0").
fn radio_index(target: crate::session::TxTarget) -> u8 {
    match target {
        crate::session::TxTarget::Yaesu2 => 1,
        crate::session::TxTarget::Yaesu1 | crate::session::TxTarget::Thetis => 0,
    }
}

/// Whether a transmitter is ThetisLink's to release when the safety task is
/// gone: a client held it, or this server keyed it. A radio on the air by
/// its own operator is not ours, and stays as it is.
fn ours_to_let_go(held: bool, server_keyed: bool) -> bool {
    held || server_keyed
}

#[cfg(test)]
mod silent_radio_holder_tests {
    /// Both radios run the 2 s clock in their status task, before the orphan
    /// watch (build 265). Since build 282 a radio's PTT is the flag on its
    /// audio: the one arm takes and keys a radio in a fixed order, the release
    /// frame lets it go when it played, the radio's own release waits for its
    /// holder to let go, and the old control keys nothing. The loop, which no
    /// test drives; the decisions are tested in `session.rs` and
    /// `tx_recover.rs`, the client's side in the engine's tests.
    #[test]
    fn both_radios_run_the_two_second_clock_and_key_by_the_flag() {
        let src = include_str!("network.rs");
        let src = &sdr_remote_core::source_text::production_code(src);
        // Since build 289 one block for both radios (`RadioSafety`), each task
        // calling it once with its own radio.
        for (target, radio) in [("Yaesu1", "yaesu"), ("Yaesu2", "yaesu2")] {
            assert_eq!(src.matches(&format!("let mut safety = RadioSafety::new(crate::session::TxTarget::{target});")).count(), 1, "{target}");
            assert_eq!(src.matches(&format!("safety.tick(&session, &socket, {radio}.as_deref(), &ptt_flag, tx_holder).await;")).count(), 1, "{target}");
        }
        assert_eq!(src.matches("safety.tick(").count(), 2);
        // The block: the radio's own release marks its holder and lowers the
        // flag; then the clock; then the orphan watch, which unkeys and lowers
        // the flag.
        let at = src.find("impl RadioSafety {").unwrap();
        let block = &src[at..at + src[at..].find("\n}\n").unwrap()];
        let order = [
            "let seen = y.status().auto_release;",
            "session.lock().await.let_go_by_the_radio(self.target, holder);
                    ptt_flag.store(false, Ordering::Relaxed);",
            "PttDeniedPacket::serialize(&mut buf, denied_target_of(self.target), Denial::ServerReleased);",
            "if self.orphan.observe(ptt_flag.load(Ordering::Relaxed), tx_holder.is_some()) {",
            "y.send_command(crate::yaesu::YaesuCmd::SetPtt(false));",
            "ptt_flag.store(false, Ordering::Relaxed);
        }",
        ];
        let mut last = 0;
        for what in order {
            let here = block.find(what).unwrap_or_else(|| panic!("in RadioSafety: {what}"));
            assert!(here >= last, "in order: {what}");
            last = here;
        }
        // The clock is not in the block any more, but in the safety tick for
        // both radios, right after Thetis's (build 290).
        assert!(!block.contains("let_a_silent_holder_go("), "not in the 200 ms status task");
        assert_eq!(src.matches("let_a_silent_holder_go(").count(), 2, "its definition and the one call");
        let at = src.find("if ptt.lock().await.check_safety().await {").unwrap();
        let tick = &src[at..at + 2000];
        let clock = tick.find("let_a_silent_holder_go(&session, target, radio.as_deref(), flag).await;").expect("in the safety tick");
        assert!(tick[..clock].contains("(crate::session::TxTarget::Yaesu1, &_yaesu, &yaesu_ptt_flag),"));
        assert!(tick[..clock].contains("(crate::session::TxTarget::Yaesu2, &_yaesu2, &yaesu2_ptt_flag),"));
        assert!(src.contains("let mut safety_tick = interval(Duration::from_millis(100));"));
        assert_eq!(src.matches("back_inside_the_press").count(), 0, "no way back of its own");
        // The arm: a stale packet, the radio's own release, a refusal, the
        // claim - and only then the key, and only when not keyed.
        let at = src.find("macro_rules! radio_audio {").unwrap();
        let arm = &src[at..at + src[at..].find("
        }
").unwrap()];
        // What the packet does is `tx_intake::intake` since build 285 (its
        // rule tested there); here the order of what the arm does with it.
        let order = [
            "s.heard_on(target, addr, Instant::now());",
            "s.let_go_seen(target, addr);",
            "let stale = senders.stale(target, addr, pkt.sequence, Instant::now());",
            "keyed: flag.load(Ordering::Relaxed),",
            "let claimed = intake_gate!(",
            "radio.status_arc().lock().unwrap().tx_start_refusal()",
            "if claimed && !flag.load(Ordering::Relaxed) {
                    senders.taken(target);
                    self.key_radio(target, addr);",
            "senders.played(target, addr, pkt.sequence, Instant::now());",
            "tx.try_send(crate::tx_recover::TxPacket {",
        ];
        let mut last = 0;
        for what in order {
            let here = arm.find(what).unwrap_or_else(|| panic!("in the arm: {what}"));
            assert!(here >= last, "in order: {what}");
            last = here;
        }
        assert_eq!(arm.matches("continue").count(), 1, "no radio; the other four ways out are the gate's (build 286)");
        // The claim, Thetis's and the radios': the radio's refusal first, then
        // the session's, a refusal told with its transmitter, a grant ends the
        // attempt (build 285).
        let at = src.find("    async fn claim_tx(").unwrap();
        let claim = &src[at..at + src[at..].find("
    }
").unwrap()];
        let order = [
            "let refusal = match radio_refusal {",
            "Some(why) => Some((why, Denial::Refused)),",
            "if s.try_acquire_tx(target, addr) {",
            "denied.attempt_over(&addr);
            return true;",
            "PttDeniedPacket::serialize(&mut buf, denied_target_of(target), denial);",
            // What a refusal gives: not granted (round 286, T6b). The window
            // ends before the function's closing brace, so this is its last line.
            "let _ = self.socket.send_to(&buf, addr).await;\n        }\n        false",
        ];
        let mut last = 0;
        for what in order {
            let here = claim.find(what).unwrap_or_else(|| panic!("in claim_tx: {what}"));
            assert!(here >= last, "in order: {what}");
            last = here;
        }
        assert!(claim.ends_with("\n        false"), "a refusal is not granted: the last line");
        assert_eq!(src.matches("self.claim_tx(").count(), 1, "in the gate, for Thetis's arm and the radios' (build 286)");
        // The release: not a press that came in meanwhile, only its holder's,
        // then the request (the wait for the sound card), the radio, the flag.
        let at = src.find("    async fn release_played(").unwrap();
        let rel = &src[at..at + src[at..].find("
    }
").unwrap()];
        let order = [
            "tx_timing.over() != played.over",
            "release_tx(target, played.from)",
            "st.tx_timing.request(false);",
            "y.send_command(crate::yaesu::YaesuCmd::SetPtt(false));",
            "flag.store(false, Ordering::Relaxed);",
        ];
        let mut last = 0;
        for what in order {
            let here = rel.find(what).unwrap_or_else(|| panic!("in release_played: {what}"));
            assert!(here >= last, "in order: {what}");
            last = here;
        }
        assert_eq!(src.matches("self.release_played(played).await;").count(), 1);
        // The old control keys nothing.
        let at = src.find("ControlId::YaesuPtt | ControlId::Yaesu2Ptt => {").expect("the old control, answered");
        let old = &src[at..at + src[at..].find("
                                }
").unwrap()];
        assert!(!old.contains("SetPtt") && !old.contains("try_acquire_tx"), "it keys nothing");
        // The clock lowers the flag after it unkeys.
        let at = src.find("async fn let_a_silent_holder_go(").unwrap();
        let clock = &src[at..at + src[at..].find("\n}\n").unwrap()];
        let off = clock.find("SetPtt(false)").unwrap();
        assert!(off < clock.find("ptt_flag.store(false, Ordering::Relaxed);").expect("the clock lowers the flag"));
    }

}

#[cfg(test)]
mod safety_task_end_tests {
    use super::{ours_to_let_go, safety_task_end};

    #[test]
    fn only_a_transmitter_that_is_ours_is_let_go() {
        assert!(ours_to_let_go(true, true));
        assert!(ours_to_let_go(true, false), "held by a client, not keyed yet");
        assert!(ours_to_let_go(false, true), "keyed by the server, nobody holds it");
        assert!(!ours_to_let_go(false, false), "an operator at the radio is not cut off");
    }

    #[test]
    fn a_safety_task_that_ends_while_the_server_runs_is_a_loss() {
        assert!(safety_task_end(Some(true), false).is_some(), "a panic");
        assert!(safety_task_end(Some(false), false).is_some(), "cancelled");
        assert!(safety_task_end(None, false).is_some(), "ended by itself");
    }

    #[test]
    fn a_safety_task_that_ends_on_a_shutdown_is_not() {
        assert_eq!(safety_task_end(None, true), None);
        assert_eq!(safety_task_end(Some(true), true), None);
    }
}

/// The presence mode's USB routing on one tick of one Yaesu (per-PTT off):
/// `Some(true)` apply now, `Some(false)` restore now, `None` nothing this tick.
/// `None` is also "wait": the flag stays as it is, and the next tick asks
/// again. One rule for both slots; they were two copies.
///
/// - Apply once there are clients and the radio is on: a 991A asleep takes no
///   menu writes, and sent while it slept the routing was never applied after
///   it woke (review 315 round 2).
/// - Restore 2 s (ten 200 ms ticks) after the last client left.
/// - Neither while a CW run keys the radio, nor a restore while it transmits
///   by any account (`on_the_air`): no automatic writes under a keyed radio
///   (review 317). An apply may go out under a microphone PTT - that
///   transmission is what the routing is for.
///
/// Named fields since build 228 (review of 226): by position, `applied` and
/// `clients` swapped wrote the 991A route at the wrong moments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PresenceRouting {
    /// The routing is applied now.
    pub applied: bool,
    /// A client listens.
    pub clients: bool,
    /// 200 ms ticks since the last client left.
    pub absent_ticks: u32,
}

impl PresenceRouting {
    pub(crate) fn step(self, st: &crate::yaesu::YaesuState) -> Option<bool> {
        if st.cw_keyed_at.is_some() {
            return None;
        }
        if self.clients {
            (!self.applied && st.power_on).then_some(true)
        } else if self.applied && self.absent_ticks >= 10 && !st.on_the_air() {
            Some(false)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod ddc_centre_send_tests {
    /// Each receiver's DDC centre goes out under its own control, and only
    /// once Thetis has told something (build 231).
    #[test]
    fn each_ddc_centre_goes_out_under_its_own_control() {
        let net = include_str!("network.rs");
        let net = &sdr_remote_core::source_text::production_code(net);
        for line in [
            "(ControlId::DdcCentreRx1, sdr_remote_core::protocol::ddc_centre_to_khz(ddc_centre_rx1)),",
            "(ControlId::DdcCentreRx2, sdr_remote_core::protocol::ddc_centre_to_khz(ddc_centre_rx2)),",
            "let ddc_centre_rx1 = {\n                                let sp = spectrum.lock().await;\n                                if sp.ddc_center_measured() { sp.ddc_center_hz() } else { 0 }",
            "let ddc_centre_rx2 = {\n                                let sp = rx2_spectrum.lock().await;\n                                if sp.ddc_center_measured() { sp.ddc_center_hz() } else { 0 }",
        ] {
            assert_eq!(net.matches(line).count(), 1, "{line}");
        }
    }
}

#[cfg(test)]
mod presence_routing_tests {
    use super::PresenceRouting;
    use crate::yaesu::YaesuState;

    // Spelled by position below; built by name here, as the loop does (build 228).
    fn presence_routing_step(applied: bool, clients: bool, absent_ticks: u32, st: &YaesuState) -> Option<bool> {
        PresenceRouting { applied, clients, absent_ticks }.step(st)
    }

    /// Both slots hand their state over by name (build 228, review of 226).
    #[test]
    fn both_slots_hand_the_presence_routing_over_by_name() {
        let net = include_str!("network.rs");
        let net = &sdr_remote_core::source_text::production_code(net);
        for line in [
            "PresenceRouting { applied: ssb_applied0, clients: !audio_addrs.is_empty(), absent_ticks: absent_ticks0 };",
            "PresenceRouting { applied: ssb_applied1, clients: !audio_addrs.is_empty(), absent_ticks: absent_ticks1 };",
        ] {
            assert_eq!(net.matches(line).count(), 1, "{line}");
        }
    }

    #[test]
    fn the_presence_routing_waits_for_the_radio_and_for_a_cw_run() {
        let mut st = YaesuState::default();
        st.power_on = true;
        assert_eq!(presence_routing_step(false, true, 0, &st), Some(true), "a client, the radio on");
        assert_eq!(presence_routing_step(true, true, 0, &st), None, "once");
        assert_eq!(presence_routing_step(true, false, 9, &st), None, "not before 2 s");
        assert_eq!(presence_routing_step(true, false, 10, &st), Some(false), "2 s without a client");
        assert_eq!(presence_routing_step(false, false, 10, &st), None, "nothing applied, nothing to restore (review 317 r2)");
        st.power_on = false;
        assert_eq!(presence_routing_step(false, true, 0, &st), None, "asleep: wait");
        st.power_on = true;
        assert!(st.begin_cw_keying(std::time::Instant::now()));
        assert_eq!(presence_routing_step(false, true, 0, &st), None, "a CW run keys it: no apply");
        assert_eq!(presence_routing_step(true, false, 10, &st), None, "and no restore");
        st.cw_keyed_at = None;
        st.ptt_commanded = true;
        assert_eq!(presence_routing_step(false, true, 0, &st), Some(true), "a microphone PTT: the routing is for it");
        assert_eq!(presence_routing_step(true, false, 10, &st), None, "but not pulled from under it");
    }
}

/// A memory write of ONE client: its data and its trigger, which arrive in
/// either order.
///
/// The order is the whole reason this exists - the data is ~2.7 kB and gets
/// fragmented, the trigger is 8 bytes, and on a reordering link the trigger
/// overtakes. So one of the two waits for the other.
///
/// What it did not have until build 160 was a NAME on that wait. Both halves
/// were locals of the packet loop, shared by every client and cleared by
/// nothing: no time-out, and not when a client said goodbye or timed out. Two
/// ways that went wrong, and the second is the bad one (review 2120A3):
///
/// - a client sends its list and then loses its link: the list stays for the
///   rest of the server run;
/// - and if another client presses "write memories" while that list is still
///   there, the server writes the FIRST client's memories into the radio. On a
///   991A that is the whole memory bank of somebody else's station.
///
/// Now both halves carry the address they came from, and a trigger only fires
/// on data from the same client.
#[derive(Default)]
pub(crate) struct MemoryWrite {
    /// Data that arrived before its trigger.
    pending: Option<(SocketAddr, String)>,
    /// A trigger that arrived before its data.
    armed: Option<SocketAddr>,
}

impl MemoryWrite {
    /// The data arrived. `Some(text)` when this client had already asked for
    /// the write, so it goes to the radio now.
    pub(crate) fn data(&mut self, from: SocketAddr, text: String) -> Option<String> {
        if self.armed == Some(from) {
            self.armed = None;
            return Some(text);
        }
        // Someone else's half-finished write is replaced, not merged: the
        // newest data is the only one anybody is still waiting for.
        self.pending = Some((from, text));
        None
    }

    /// The trigger arrived. `Some(text)` when this client's data is already
    /// here.
    pub(crate) fn trigger(&mut self, from: SocketAddr) -> Option<String> {
        if matches!(self.pending, Some((a, _)) if a == from) {
            return self.pending.take().map(|(_, t)| t);
        }
        self.armed = Some(from);
        None
    }

    /// This client is gone. Whatever it left half-done goes with it - that is
    /// what makes the stale list impossible rather than unlikely.
    pub(crate) fn forget(&mut self, gone: SocketAddr) {
        if matches!(self.pending, Some((a, _)) if a == gone) {
            self.pending = None;
        }
        if self.armed == Some(gone) {
            self.armed = None;
        }
    }
}

#[cfg(test)]
mod memory_write_tests {
    use super::MemoryWrite;
    use std::net::SocketAddr;

    fn c(n: u8) -> SocketAddr {
        format!("127.0.0.{n}:1").parse().unwrap()
    }

    /// Either order, one client: the write happens exactly once.
    #[test]
    fn data_and_trigger_meet_in_either_order() {
        let mut w = MemoryWrite::default();
        assert_eq!(w.data(c(1), "A".into()), None, "data first: nothing to write yet");
        assert_eq!(w.trigger(c(1)).as_deref(), Some("A"), "and the trigger fires it");
        assert_eq!(w.trigger(c(1)), None, "not a second time");

        let mut w = MemoryWrite::default();
        assert_eq!(w.trigger(c(1)), None, "trigger first: wait for the data");
        assert_eq!(w.data(c(1), "A".into()).as_deref(), Some("A"));
        assert_eq!(w.data(c(1), "A".into()), None, "and the latch is spent");
    }

    /// The one that matters: one client's data must never be written by
    /// another client's button. Before build 160 both halves were shared
    /// locals, so this wrote station A's whole memory bank into the radio on
    /// B's press (review 2120A3).
    #[test]
    fn one_clients_data_is_never_written_by_anothers_button() {
        let mut w = MemoryWrite::default();
        w.data(c(1), "the memories of station A".into());
        assert_eq!(w.trigger(c(2)), None, "B's button does not write A's list");
        // B's own write still works, and it is B's data that goes out.
        assert_eq!(w.data(c(2), "B".into()).as_deref(), Some("B"));
    }

    /// And the mirror of it: a latch armed by one client is not fired by
    /// another client's data.
    #[test]
    fn one_clients_button_is_not_fired_by_anothers_data() {
        let mut w = MemoryWrite::default();
        assert_eq!(w.trigger(c(1)), None);
        assert_eq!(w.data(c(2), "B".into()), None, "A's latch is not B's to fire");
        assert_eq!(w.trigger(c(2)).as_deref(), Some("B"), "B's own pair still meets");
    }

    /// A client that leaves takes its half-finished write with it.
    #[test]
    fn a_client_that_leaves_takes_its_half_write_with_it() {
        let mut w = MemoryWrite::default();
        w.data(c(1), "A".into());
        w.forget(c(1));
        assert_eq!(w.trigger(c(1)), None, "gone, so there is nothing to fire");

        let mut w = MemoryWrite::default();
        w.trigger(c(1));
        w.forget(c(1));
        assert_eq!(w.data(c(1), "A".into()), None, "and the latch left with it");

        // Somebody else leaving changes nothing.
        let mut w = MemoryWrite::default();
        w.data(c(1), "A".into());
        w.forget(c(2));
        assert_eq!(w.trigger(c(1)).as_deref(), Some("A"));
    }
}

/// Why a CW run may not key this Yaesu, from what the radio's status says. The
/// transmitter lock and the key line come after this.
pub(crate) fn yaesu_cw_refusal(connected: bool, power_on: bool, mode: u8) -> Option<&'static str> {
    // Both models key the same way - RTS of the port beside the CAT port -
    // with their own PC KEYING menu (ex_menu::pc_keying_menu). The FTX-1 was
    // measured on 2026-09-20 (COM27 beside COM25), the 991A on 2026-09-18.
    if !connected || !power_on {
        return Some("the radio is off or not connected");
    }
    if !matches!(mode, 3 | 4) {
        return Some("the radio is not in a CW mode");
    }
    None
}

#[cfg(test)]
mod yaesu_key_edge_tests {
    use super::yaesu_key_edge;
    use crate::cw_key_line::KeyOut;

    struct Fake {
        name: &'static str,
        fails: bool,
        writes: Vec<bool>,
        /// What `lower_now` did, in a place the test can still read after the
        /// line has been dropped: `Some(true)` it reached the port. Shared,
        /// because the whole point is what happens on the way out.
        lowered: std::sync::Arc<std::sync::Mutex<Option<bool>>>,
        lower_fails: bool,
    }
    impl KeyOut for Fake {
        fn name(&self) -> &str {
            self.name
        }
        fn set(&mut self, down: bool) -> anyhow::Result<()> {
            if self.fails {
                anyhow::bail!("the port went away");
            }
            self.writes.push(down);
            Ok(())
        }
        fn lower_now(&mut self) -> anyhow::Result<()> {
            // Deliberately its own flag: a keying write and a lower are two
            // writes to the same port, and the case that matters is the one
            // where the first fails and the second still gets through.
            *self.lowered.lock().unwrap() = Some(!self.lower_fails);
            if self.lower_fails {
                anyhow::bail!("the port went away");
            }
            self.writes.push(false);
            Ok(())
        }
    }

    #[test]
    fn a_line_that_fails_is_dropped_and_only_that_one() {
        let mut lines = super::KeyLineSet {
            slots: [
                Some(Fake { name: "COM12", fails: true, writes: vec![], lowered: Default::default(), lower_fails: false }),
                Some(Fake { name: "COM27", fails: false, writes: vec![], lowered: Default::default(), lower_fails: false }),
            ],
            closed_at_exit: false,
        };
        yaesu_key_edge(&mut lines, 0, true, Some(66));
        assert!(lines.slots[0].is_none(), "the failing line is gone, so the next run opens it again");
        yaesu_key_edge(&mut lines, 1, true, Some(66));
        yaesu_key_edge(&mut lines, 1, false, Some(66));
        assert_eq!(lines.slots[1].as_ref().map(|l| l.writes.clone()), Some(vec![true, false]), "the other slot keyed as asked");
        yaesu_key_edge(&mut lines, 0, false, None); // no line: logged, nothing else
        assert!(lines.slots[0].is_none());
    }

    /// A failed key-UP is the dangerous one: after it the playout believes the
    /// key is up, so no further edge comes and nothing would try again - and
    /// on a Yaesu the line IS the transmitter. So the line is asked to go down
    /// explicitly before the handle is given up (review 2120A1).
    #[test]
    fn a_key_up_that_fails_still_tries_to_put_the_line_down() {
        // A line that keys fine, then loses its port before the key-up.
        let mut lines = super::KeyLineSet {
            slots: [Some(Fake { name: "COM12", fails: false, writes: vec![], lowered: Default::default(), lower_fails: false }), None],
            closed_at_exit: false,
        };
        yaesu_key_edge(&mut lines, 0, true, Some(66));
        lines.slots[0].as_mut().unwrap().fails = true;

        // The journal outlives the line, which is the point: the line is gone
        // after this and the question is what it did on the way out.
        lines.slots[0].as_mut().unwrap().lower_fails = true;
        let journal = std::sync::Arc::clone(&lines.slots[0].as_ref().unwrap().lowered);
        assert_eq!(*journal.lock().unwrap(), None, "nothing has tried to lower it yet");

        yaesu_key_edge(&mut lines, 0, false, None);

        assert!(lines.slots[0].is_none(), "the handle is given up either way");
        assert_eq!(
            *journal.lock().unwrap(),
            Some(false),
            "the line was asked to go down before the handle was given up -              it failed here, which is what the error line in the log is for"
        );
    }

    /// The case this is all for: the keying write fails, the port is still
    /// there, and the line comes down. Without the explicit lower the radio
    /// would keep transmitting with nobody left to ask.
    #[test]
    fn a_failed_key_up_on_a_live_port_still_brings_the_line_down() {
        let mut lines = super::KeyLineSet {
            slots: [Some(Fake { name: "COM12", fails: false, writes: vec![], lowered: Default::default(), lower_fails: false }), None],
            closed_at_exit: false,
        };
        let journal = std::sync::Arc::clone(&lines.slots[0].as_ref().unwrap().lowered);
        yaesu_key_edge(&mut lines, 0, true, Some(66));
        lines.slots[0].as_mut().unwrap().fails = true; // the key-up write fails
        yaesu_key_edge(&mut lines, 0, false, None);
        assert_eq!(*journal.lock().unwrap(), Some(true), "it reached the port: the radio is off the air");
        assert!(lines.slots[0].is_none(), "and the handle is given up, so the next run opens it fresh");
    }

    /// And an edge that goes through changes nothing: no drop, no lower.
    #[test]
    fn an_edge_that_succeeds_drops_nothing_and_lowers_nothing() {
        let mut lines = super::KeyLineSet {
            slots: [Some(Fake { name: "COM12", fails: false, writes: vec![], lowered: Default::default(), lower_fails: false }), None],
            closed_at_exit: false,
        };
        let journal = std::sync::Arc::clone(&lines.slots[0].as_ref().unwrap().lowered);
        yaesu_key_edge(&mut lines, 0, true, Some(66));
        yaesu_key_edge(&mut lines, 0, false, None);
        assert_eq!(*journal.lock().unwrap(), None);
        assert_eq!(lines.slots[0].as_ref().map(|l| l.writes.clone()), Some(vec![true, false]));
    }

    /// Review 317: the end of a run silences its own lines, and only its own.
    #[test]
    fn after_the_exit_nothing_is_written_and_a_new_run_keys_again() {
        let mut ended = super::KeyLineSet {
            slots: [Some(Fake { name: "COM12", fails: false, writes: vec![], lowered: Default::default(), lower_fails: false }), None],
            closed_at_exit: true,
        };
        yaesu_key_edge(&mut ended, 0, true, Some(66));
        assert_eq!(ended.slots[0].as_ref().map(|l| l.writes.len()), Some(0), "closed: nothing reaches the line");
        // The next server run (Settings, then Save & Start) has a set of its own.
        let mut next = super::KeyLineSet::new();
        next.slots[0] = Some(Fake { name: "COM12", fails: false, writes: vec![], lowered: Default::default(), lower_fails: false });
        yaesu_key_edge(&mut next, 0, true, Some(66));
        assert_eq!(next.slots[0].as_ref().map(|l| l.writes.clone()), Some(vec![true]), "a new run keys");
    }
}

#[cfg(test)]
mod yaesu_cw_gate_tests {
    use super::yaesu_cw_refusal;

    #[test]
    fn a_yaesu_is_keyed_only_when_it_is_on_and_in_cw() {
        assert_eq!(yaesu_cw_refusal(true, true, 3), None, "CW-L");
        assert_eq!(yaesu_cw_refusal(true, true, 4), None, "CW-U");
        // Since build 148 both models: the FTX-1 keys the same way, with its
        // own PC KEYING menu (measured 2026-09-20).
        assert!(yaesu_cw_refusal(false, true, 3).is_some(), "the port is away");
        assert!(yaesu_cw_refusal(true, false, 3).is_some(), "the radio is off");
        assert!(yaesu_cw_refusal(true, true, 0).is_some(), "LSB");
        assert!(yaesu_cw_refusal(true, true, 1).is_some(), "USB");
        assert!(yaesu_cw_refusal(true, true, 2).is_some(), "internal 2 is neither CW-L (3) nor CW-U (4)");
        assert!(yaesu_cw_refusal(true, true, 5).is_some(), "anything else");
    }
}

impl NetworkService {
    /// The key lines, for the end of the server to lower before it puts the
    /// radios back (`lower_all_key_lines`).
    pub fn cw_lines(&self) -> CwKeyLines {
        self.cw_lines.clone()
    }

    /// May this client's CW run key this Yaesu, and is the radio ready for it?
    /// The steps and their order are `admit_yaesu_cw_steps`: the radio's own
    /// status, the transmitter lock, at the first run its key line and menu
    /// 060, and last the CW mark. On a refusal nothing is left behind: a lock
    /// taken for the attempt goes back (one this client held before stays).
    async fn admit_yaesu_cw(&self, addr: SocketAddr, target: u8) -> Result<(), String> {
        let tx = crate::session::TxTarget::from_cw_target(target).ok_or("no such radio")?;
        let slot = usize::from(target - 1);
        let (radio, cat_port) = match slot {
            0 => (self.yaesu.clone(), self.config.yaesu_port.clone()),
            _ => (self.yaesu2.clone(), self.config.yaesu2_port.clone()),
        };
        let radio = radio.ok_or_else(|| format!("there is no Yaesu in slot {}", slot))?;
        admit_yaesu_cw_steps(&mut ServiceAdmission { svc: self, addr, tx, slot, radio, cat_port }).await
    }

    /// The first CW run to this radio: find the key port next to its CAT port,
    /// open it (both lines low), and have the serial thread set menu 060 to RTS.
    /// The line is kept only when all of that worked.
    async fn open_yaesu_key_line(
        &self,
        radio: &Arc<crate::yaesu::YaesuRadio>,
        slot: usize,
        cat_port: Option<&str>,
    ) -> Result<(), String> {
        let cat = cat_port.ok_or("its CAT port is not configured")?;
        let key_port = crate::cw_key_line::sibling_port(&crate::cw_key_line::ports_now(), cat)
            .ok_or_else(|| format!("no key port found next to its CAT port {}", cat))?;
        let line = crate::cw_key_line::KeyLine::open(&key_port).map_err(|e| format!("opening its key port {} failed: {}", key_port, e))?;
        let r = radio.clone();
        // Long enough to sit behind a menu batch of the FTX-1 scan. Measured
        // from the code: settle 250 ms + batch 150 + one read_ftx1_ex_value of
        // 3x715+60 = 2205, so ~2.6 s. At 1000 a first CW stroke in the first
        // minute after connecting was refused, with a reason naming the wrong
        // cause (review 319; found still un-applied in 2120A2). Since build
        // 284 a batch gives way between items and between attempts, so the
        // wait is one attempt (~0.7 s); 3000 is kept as the margin.
        tokio::task::spawn_blocking(move || r.prepare_cw_keying(std::time::Duration::from_millis(3000)))
            .await
            .map_err(|e| format!("preparing the radio failed: {}", e))??;
        info!("cw: Yaesu slot {} keys on RTS of {} (CAT on {})", slot, line.name(), cat);
        self.cw_lines.lock().unwrap_or_else(|e| e.into_inner()).slots[slot] = Some(line);
        Ok(())
    }
}

#[cfg(test)]
mod quic_endpoint {
    /// The server's one endpoint is QUIC, on the port it binds, with the
    /// relay's route when there is one - and a QUIC endpoint that cannot start
    /// stops the server, it is not a warning (build 133; until then it was,
    /// beside the plain UDP socket). Pinned, not run: the start-up block runs
    /// in no test.
    #[test]
    fn the_start_up_runs_quic_or_does_not_start() {
        let src = include_str!("network.rs");
        let call = "sdr_remote_quic::QuicServer::start_on(std_socket, sdr_remote_quic::Congestion::Bbr, quic_relay, direct)";
        assert_eq!(src.matches(call).count(), 2, "the call, and this line quoting it");
        let at = src.find(call).unwrap();
        let rest = &src[at..];
        let fail = rest.find("Err(e) => {").expect("the failure arm");
        let arm = &rest[fail..fail + rest[fail..].find("
            }
").expect("the arm ends")];
        assert!(arm.contains("return Err("), "a QUIC endpoint that cannot start stops the server: {arm}");
    }
}

#[cfg(test)]
mod bind_with_retry_tests {
    use super::bind_with_retry;
    use std::time::Duration;

    /// A port that is taken: the wait ends at once on a stop, not after 30 s.
    /// And a free port binds. Both states (review 130).
    #[tokio::test]
    async fn a_stop_ends_the_wait_for_a_taken_port() {
        let taken = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = taken.local_addr().unwrap();
        let (tx, rx) = tokio::sync::watch::channel(false);
        let wait = tokio::spawn(bind_with_retry(addr, rx));
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(!wait.is_finished(), "still waiting for the taken port");
        tx.send(true).unwrap();
        let got = tokio::time::timeout(Duration::from_secs(1), wait).await.expect("stopped within a second").unwrap();
        assert!(matches!(got, Ok(None)), "a stop, not a socket and not an error");
        drop(taken);
        let (_tx, rx) = tokio::sync::watch::channel(false);
        let free = bind_with_retry("127.0.0.1:0".parse().unwrap(), rx).await;
        assert!(matches!(free, Ok(Some(_))), "a free port binds");
        // A lasting error (an address this machine does not have) fails at
        // once, not after 30 s of retries (review 131: the sorting of errors
        // into lasting and passing ran no test).
        let (_tx, rx) = tokio::sync::watch::channel(false);
        let elsewhere = tokio::time::timeout(Duration::from_secs(1), bind_with_retry("192.0.2.1:0".parse().unwrap(), rx))
            .await
            .expect("a lasting error is not retried");
        assert!(elsewhere.is_err(), "and it is an error");
    }
}
