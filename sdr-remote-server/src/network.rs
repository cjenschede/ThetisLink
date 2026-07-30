// SPDX-License-Identifier: GPL-2.0-or-later

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};

// NOTE: the former global VRX spectrum-span + audio-rate atomics are removed -
// both are now per-client in `vrx_manager::PerClientVrxManager`
// (PATCH-vrx-per-client).
use std::time::Instant;

use anyhow::{Context, Result};
use log::{debug, info, warn};
use tokio::net::UdpSocket;
use crate::tracked_socket::TrackedSocket;
use tokio::sync::{watch, Mutex};
use tokio::time::{interval, Duration};

use sdr_remote_core::jitter::{BufferedFrame, JitterBuffer, JitterResult};
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
fn write_bind_diag(entry: &str) {
    let log_path = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("thetislink-server.log");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true).append(true).open(&log_path)
    {
        use std::io::Write;
        let _ = f.write_all(entry.as_bytes());
    }
}

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
    yaesu: Option<Arc<crate::yaesu::YaesuRadio>>,
    /// Dual-radio slot 1 (PATCH-dual-radio-991a-ftx1, Optie B-prime). Eigen
    /// onafhankelijke broadcast/audio/control-keten; None = slot 1 uit.
    yaesu2: Option<Arc<crate::yaesu::YaesuRadio>>,
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
        // Phase C: als de relay aan staat, de tunnel-kanalen naar/van de relay-monitor.
        // uplink = TrackedSocket -> monitor (send naar sentinel); inbound = monitor -> TrackedSocket.
        relay_socket: Option<(
            tokio::sync::mpsc::UnboundedSender<(SocketAddr, Vec<u8>)>,
            tokio::sync::mpsc::UnboundedReceiver<(SocketAddr, Vec<u8>)>,
        )>,
    ) -> Result<Self> {
        // Wrap bind in 30s timeout to catch cold-boot kernel-hangs (memory
        // project_tl2_coldboot_bind_fail.md). Within that window we retry
        // every 250 ms on "address in use" / "access denied" / similar
        // transient errors so the auto-restart path doesn't lose the race
        // when the previous instance's socket is still in TIME_WAIT or the
        // process slot is briefly held. Other errors fail immediately.
        const BIND_TIMEOUT_SECS: u64 = 30;
        const BIND_RETRY_MS: u64 = 250;
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let bind_loop = async {
            let started = std::time::Instant::now();
            let deadline = Duration::from_secs(BIND_TIMEOUT_SECS);
            loop {
                match UdpSocket::bind(bind_addr).await {
                    Ok(s) => return Ok(s),
                    Err(e) => {
                        // Identify transient port-not-yet-released errors so we
                        // distinguish them from "no such interface" etc.
                        let kind = e.kind();
                        let transient = matches!(
                            kind,
                            std::io::ErrorKind::AddrInUse
                                | std::io::ErrorKind::PermissionDenied
                        );
                        if !transient || started.elapsed() >= deadline {
                            return Err(e);
                        }
                        info!(
                            "bind {} transient error ({:?}: {}), retrying in {}ms",
                            bind_addr, kind, e, BIND_RETRY_MS
                        );
                        tokio::time::sleep(Duration::from_millis(BIND_RETRY_MS)).await;
                    }
                }
            }
        };
        let socket = match tokio::time::timeout(
            Duration::from_secs(BIND_TIMEOUT_SECS + 2),
            bind_loop,
        )
        .await
        {
            Ok(Ok(s)) => {
                let _ = bind_status_slot.set(
                    crate::audio_stats::BindStatus::Ok { addr: bind_addr.to_string() },
                );
                s
            }
            Ok(Err(e)) => {
                let entry = format!("[BIND-FAIL] ts={} addr={} error={}\n", ts, bind_addr, e);
                write_bind_diag(&entry);
                let _ = bind_status_slot.set(
                    crate::audio_stats::BindStatus::Failed {
                        addr: bind_addr.to_string(),
                        error: format!("{}", e),
                    },
                );
                return Err(anyhow::Error::new(e)).context("bind UDP socket");
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
                    "bind UDP socket timed out after {}s on {}",
                    BIND_TIMEOUT_SECS, bind_addr
                ));
            }
        };
        // Enlarge UDP buffers: default Windows buffer (8KB) overflows with
        // spectrum packets (4-8KB each x clients), dropping audio packets.
        {
            use socket2::SockRef;
            let sock_ref = SockRef::from(&socket);
            let _ = sock_ref.set_send_buffer_size(2 * 1024 * 1024); // 2MB
            let _ = sock_ref.set_recv_buffer_size(512 * 1024);
            let send = sock_ref.send_buffer_size().unwrap_or(0);
            let recv = sock_ref.recv_buffer_size().unwrap_or(0);
            info!("UDP socket bound to {} (send_buf={}KB, recv_buf={}KB)", bind_addr, send/1024, recv/1024);
        }
        // Mark the UDP socket non-inheritable on Windows. Default WSASocket
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

        Ok(Self {
            socket: Arc::new(match relay_socket {
                Some((uplink_tx, inbound_rx)) => {
                    TrackedSocket::with_relay(socket, uplink_tx, inbound_rx)
                }
                None => TrackedSocket::new(socket),
            }),
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
            yaesu,
            yaesu2,
            audio_stats,
            tci_probe,
            server_start,
        })
    }

    /// Zet beide spectrumprocessors aan/uit op basis van ALLE afnemers.
    ///
    /// De RX1-processor voedt zowel het RX1-spectrum als VRX1-high-res; de
    /// RX2-processor doet hetzelfde voor RX2 en VRX2. Een processor mag dus pas
    /// uit als geen van beide afnemers hem nog nodig heeft. Eerder keek dit
    /// alleen naar de RX*-spectrumabonnees, waardoor VRX-high-res stilviel zodra
    /// de gebruiker het RX-spectrum uitzette.
    ///
    /// Aanroepen na elke wijziging van een spectrum-abonnement (RX1, RX2, VRX1,
    /// VRX2). Het disconnect-pad roept dit bewust NIET aan: dan gaan er dankzij
    /// de per-stroom-guards in de tick toch geen pakketten meer uit, en herstelt
    /// de staat zich bij de eerstvolgende abonnementswijziging.
    ///
    /// Herberekent ook de frame-rate. Die is per processor globaal en werd
    /// eerder alleen afgeleid uit de RX-spectrumabonnees. Omdat VRX-high-res nu
    /// dezelfde frame-gate deelt, zou een oude hoge RX-fps blijven staan nadat
    /// het RX-spectrum uitgezet is - en dan loopt juist de grootste stroom
    /// (4096 bins) onnodig snel. `spectrum_max_fps()` valt terug op de default
    /// zodra er geen RX-spectrumabonnees meer zijn, wat precies het gewenste
    /// VRX-only-gedrag geeft.
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
        // De RX2-processor krijgt bewust GEEN fps mee: `Rx2SpectrumFps` bewaart de
        // waarde alleen in de sessie en is nooit toegepast, dus die processor draait
        // sinds jaar en dag op de default. Hier de RX1-fps opleggen zou RX2-gedrag
        // veranderen zonder aanleiding.
        self.rx2_spectrum.lock().await.set_enabled(rx2_needed);
        // Eén regel die laat zien welke stromen afnemers hebben en wat dat met de
        // processors doet - genoeg om een "waarom zie ik geen spectrum"-klacht in
        // één blik te herleiden.
        info!(
            "spectrum procs: rx1={} (spec={} vrx1={} fps={}) rx2={} (spec={} vrx2={})",
            if rx1_needed { "aan" } else { "uit" }, rx1_spec, rx1_vrx, fps,
            if rx2_needed { "aan" } else { "uit" }, rx2_spec, rx2_vrx
        );
    }

    pub async fn run(mut self) -> Result<()> {
        let start = Instant::now();
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

        // Slot-1 RX audio loop (Optie B-prime) - exacte spiegel van slot 0,
        // maar broadcast naar yaesu2_addrs als AudioYaesu2 (slot=1).
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
                let mut tick = interval(Duration::from_millis(1000));
                loop {
                    tokio::select! {
                        _ = tick.tick() => {
                            let tci_up = {
                                let p = ptt.lock().await;
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
        let _yaesu_state_handle = {
            let yaesu = yaesu.clone();
            let socket = self.socket.clone();
            let session = self.session.clone();
            let mut shutdown = self.shutdown.clone();
            if yaesu.is_some() {
                Some(tokio::spawn(async move {
                    let mut tick = interval(Duration::from_millis(200));
                    let mut last_features0: u32 = u32::MAX; // value-change-only feature-state
                    let mut last_levels0 = [255u8; YaesuFeaturePacket::N_LEVELS];
                    let mut last_freqs0 = [u16::MAX; YaesuFeaturePacket::N_FREQS];
                    // Verse abonnees krijgen eenmalig de huidige feature-state (sent-set),
                    // zodat een net-verbonden client meteen de juiste AGC/IPO/NB/DNR/Proc-
                    // standen heeft (anders is de eerste cyclus-klik een no-op op stale 0).
                    let mut sent_features0: std::collections::HashSet<std::net::SocketAddr> = std::collections::HashSet::new();
                    // Presence-based SSB-routing (opt-out modus, per-PTT UIT): zet de USB-
                    // routing aan zolang er een client op radio 1 zit, en herstel ~2s nadat
                    // de laatste weg is. In per-PTT-modus doet de SetPtt-handler dit.
                    let mut ssb_applied0 = false;
                    let mut absent_ticks0: u32 = 0;
                    loop {
                        tokio::select! {
                            _ = tick.tick() => {
                                // Audio-abonnees (voor SSB-USB-routing/TX) én state-abonnees
                                // (venster-open OF audio) apart, onder één lock.
                                let (audio_addrs, addrs) = {
                                    let s = session.lock().await;
                                    (s.yaesu_addrs(), s.yaesu_state_addrs())
                                };
                                if let Some(ref y) = yaesu {
                                    if !y.status().ssb_switch_on_ptt {
                                        if !audio_addrs.is_empty() {
                                            absent_ticks0 = 0;
                                            if !ssb_applied0 {
                                                y.send_command(crate::yaesu::YaesuCmd::SetSsbRouting(true));
                                                ssb_applied0 = true;
                                            }
                                        } else {
                                            absent_ticks0 = absent_ticks0.saturating_add(1);
                                            if ssb_applied0 && absent_ticks0 >= 10 { // 10x200ms = 2s
                                                y.send_command(crate::yaesu::YaesuCmd::SetSsbRouting(false));
                                                ssb_applied0 = false;
                                            }
                                        }
                                    }
                                }
                                // State/feature/memory naar de STATE-abonnees.
                                if addrs.is_empty() { continue; }
                                if let Some(ref y) = yaesu {
                                    let ys = y.status();
                                    let pkt = YaesuStatePacket {
                                        freq_a: ys.vfo_a_freq,
                                        freq_b: ys.vfo_b_freq,
                                        mode: ys.mode,
                                        smeter: if ys.connected { ys.smeter } else { 0 },
                                        tx_active: if ys.connected { ys.tx_active } else { false },
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
                                        tx_power_max: ys.tx_power_max(),
                                    };
                                    let mut buf = [0u8; YaesuStatePacket::SIZE];
                                    pkt.serialize(&mut buf);
                                    for addr in &addrs {
                                        let _ = socket.try_send_to(&buf, *addr);
                                    }
                                    // Feature-state (toggles + levels): value-change-only push
                                    // + initiele push naar verse abonnees (sent-set). Markeer een
                                    // addr alleen bij geslaagde send -> retry volgende tick bij fout.
                                    let feat_changed0 = ys.feature_toggles != last_features0 || ys.feature_levels != last_levels0 || ys.feature_freqs != last_freqs0;
                                    if feat_changed0 {
                                        last_features0 = ys.feature_toggles;
                                        last_levels0 = ys.feature_levels;
                                        last_freqs0 = ys.feature_freqs;
                                        sent_features0.clear(); // iedereen krijgt de nieuwe state
                                    }
                                    sent_features0.retain(|a| addrs.contains(a)); // prune losgekoppelde
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

                                    // Check for memory data ready to send
                                    let mem_data = y.memory_data.lock().unwrap().take();
                                    if let Some(text) = mem_data {
                                        let text_bytes = text.as_bytes();
                                        // Split into chunks if needed (UDP max ~64KB)
                                        let chunk_size = 60000;
                                        for chunk in text_bytes.chunks(chunk_size) {
                                            let mut send_buf = Vec::with_capacity(6 + chunk.len());
                                            let header = Header::new(PacketType::YaesuMemoryData, Flags::NONE);
                                            let mut hdr_buf = [0u8; 4];
                                            header.serialize(&mut hdr_buf);
                                            send_buf.extend_from_slice(&hdr_buf);
                                            send_buf.extend_from_slice(&(chunk.len() as u16).to_be_bytes());
                                            send_buf.extend_from_slice(chunk);
                                            for addr in &addrs {
                                                let _ = socket.try_send_to(&send_buf, *addr);
                                            }
                                        }
                                        info!("Sent Yaesu memory data to {} clients ({}B)", addrs.len(), text_bytes.len());
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

        // Slot-1 state broadcast (Optie B-prime) - spiegel van slot 0, maar als
        // YaesuState2 / YaesuMemoryData2 naar yaesu2_addrs (subscription-gated).
        let _yaesu2_state_handle = {
            let yaesu2 = yaesu2.clone();
            let socket = self.socket.clone();
            let session = self.session.clone();
            let mut shutdown = self.shutdown.clone();
            if yaesu2.is_some() {
                Some(tokio::spawn(async move {
                    let mut tick = interval(Duration::from_millis(200));
                    let mut last_features1: u32 = u32::MAX; // value-change-only feature-state
                    let mut last_levels1 = [255u8; YaesuFeaturePacket::N_LEVELS];
                    let mut last_freqs1 = [u16::MAX; YaesuFeaturePacket::N_FREQS];
                    // Verse abonnees krijgen eenmalig de huidige feature-state (zie slot 0).
                    let mut sent_features1: std::collections::HashSet<std::net::SocketAddr> = std::collections::HashSet::new();
                    // Presence-based SSB-routing (opt-out modus) voor radio 2 (FTX-1).
                    let mut ssb_applied1 = false;
                    let mut absent_ticks1: u32 = 0;
                    loop {
                        tokio::select! {
                            _ = tick.tick() => {
                                let (audio_addrs, addrs) = {
                                    let s = session.lock().await;
                                    (s.yaesu2_addrs(), s.yaesu2_state_addrs())
                                };
                                if let Some(ref y) = yaesu2 {
                                    if !y.status().ssb_switch_on_ptt {
                                        if !audio_addrs.is_empty() {
                                            absent_ticks1 = 0;
                                            if !ssb_applied1 {
                                                y.send_command(crate::yaesu::YaesuCmd::SetSsbRouting(true));
                                                ssb_applied1 = true;
                                            }
                                        } else {
                                            absent_ticks1 = absent_ticks1.saturating_add(1);
                                            if ssb_applied1 && absent_ticks1 >= 10 { // 2s
                                                y.send_command(crate::yaesu::YaesuCmd::SetSsbRouting(false));
                                                ssb_applied1 = false;
                                            }
                                        }
                                    }
                                }
                                if addrs.is_empty() { continue; }
                                if let Some(ref y) = yaesu2 {
                                    let ys = y.status();
                                    let pkt = YaesuStatePacket {
                                        freq_a: ys.vfo_a_freq,
                                        freq_b: ys.vfo_b_freq,
                                        mode: ys.mode,
                                        smeter: if ys.connected { ys.smeter } else { 0 },
                                        tx_active: if ys.connected { ys.tx_active } else { false },
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
                                        tx_power_max: ys.tx_power_max(),
                                    };
                                    let mut buf = [0u8; YaesuStatePacket::SIZE];
                                    pkt.serialize_as_type(&mut buf, PacketType::YaesuState2);
                                    for addr in &addrs {
                                        let _ = socket.try_send_to(&buf, *addr);
                                    }
                                    // Feature-state (toggles + levels): value-change-only + initiele
                                    // push naar verse abonnees (sent-set), slot 1. Zie slot 0.
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

                                    let mem_data = y.memory_data.lock().unwrap().take();
                                    if let Some(text) = mem_data {
                                        let text_bytes = text.as_bytes();
                                        let chunk_size = 60000;
                                        for chunk in text_bytes.chunks(chunk_size) {
                                            let mut send_buf = Vec::with_capacity(6 + chunk.len());
                                            let header = Header::new(PacketType::YaesuMemoryData2, Flags::NONE);
                                            let mut hdr_buf = [0u8; 4];
                                            header.serialize(&mut hdr_buf);
                                            send_buf.extend_from_slice(&hdr_buf);
                                            send_buf.extend_from_slice(&(chunk.len() as u16).to_be_bytes());
                                            send_buf.extend_from_slice(chunk);
                                            for addr in &addrs {
                                                let _ = socket.try_send_to(&send_buf, *addr);
                                            }
                                        }
                                        info!("[radio2] Sent memory data to {} clients ({}B)", addrs.len(), text_bytes.len());
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

        // RadioInfo broadcast (Optie B-prime): laat dual-radio-bewuste clients
        // (die Yaesu2Enable hebben gestuurd -> yaesu2_addrs) het gedetecteerde
        // model per slot weten, voor de paneel-naamgeving ("991A 1"/"FTX1" etc.).
        // Oude clients zitten nooit op yaesu2_addrs -> krijgen dit nieuwe packet-
        // type nooit (back-compat by construction). 1 Hz volstaat ruim.
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
                                if let Some(ref y) = yaesu { infos.push((0, y.model.as_code())); }
                                if let Some(ref y) = yaesu2 { infos.push((1, y.model.as_code())); }
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
        // + model per slot naar ALLE actieve clients - ontkoppelt de selector/
        // connected-status van de audio+state-subscription zodat wegvallen/bijkomen
        // dynamisch is (niet sticky). Push-on-change (prev-tuple diff) + determin-
        // istische levering aan verse clients (elke addr krijgt de huidige tuple een
        // keer via de sent-set, <= tick-latency; een verse client mist de diff dus niet,
        // zonder ambigue auth-timing). Value-change-only logging.
        let _yaesu_presence_handle = {
            let yaesu = yaesu.clone();
            let yaesu2 = yaesu2.clone();
            let socket = self.socket.clone();
            let session = self.session.clone();
            let mut shutdown = self.shutdown.clone();
            if yaesu.is_some() || yaesu2.is_some() {
                // Diagnostische sim van radio-afwezigheid zonder loskoppelen.
                // THETISLINK_SIM_YAESU_ABSENT=0 | 1 | 0,1 forceert die slot(s) "afwezig"
                // (alleen leespad, low-risk). Eenmalig gelezen bij server-start.
                let sim = std::env::var("THETISLINK_SIM_YAESU_ABSENT").unwrap_or_default();
                let sim_absent0 = sim.split(',').any(|s| s.trim() == "0");
                let sim_absent1 = sim.split(',').any(|s| s.trim() == "1");
                if sim_absent0 || sim_absent1 {
                    info!("Yaesu presence SIM active: forcing absent slot0={} slot1={}", sim_absent0, sim_absent1);
                }
                Some(tokio::spawn(async move {
                    let mut tick = interval(Duration::from_millis(500));
                    let mut prev: Option<(bool, u8, bool, u8)> = None;
                    let mut sent_addrs: std::collections::HashSet<std::net::SocketAddr> =
                        std::collections::HashSet::new();
                    loop {
                        tokio::select! {
                            _ = tick.tick() => {
                                let present0 = yaesu.as_ref().map(|y| y.status().connected).unwrap_or(false) && !sim_absent0;
                                let present1 = yaesu2.as_ref().map(|y| y.status().connected).unwrap_or(false) && !sim_absent1;
                                let model0 = yaesu.as_ref().map(|y| y.model.as_code()).unwrap_or(0);
                                let model1 = yaesu2.as_ref().map(|y| y.model.as_code()).unwrap_or(0);
                                let tuple = (present0, model0, present1, model1);

                                let addrs = session.lock().await.active_addrs();
                                sent_addrs.retain(|a| addrs.contains(a)); // prune losgekoppelde

                                let changed = prev != Some(tuple);
                                if changed {
                                    // L1: value-change-only, incl. aantal ontvangers.
                                    info!("Yaesu presence push: slot0={}/{} slot1={}/{} -> {} clients",
                                        present0, model0, present1, model1, addrs.len());
                                    prev = Some(tuple);
                                    sent_addrs.clear(); // iedereen krijgt de nieuwe tuple
                                }

                                let pkt = YaesuPresencePacket {
                                    slot0_present: present0, slot0_model: model0,
                                    slot1_present: present1, slot1_model: model1,
                                };
                                let mut buf = [0u8; YaesuPresencePacket::SIZE];
                                pkt.serialize(&mut buf);
                                // Bij wijziging naar alle actieve clients; anders alleen
                                // verse clients die de huidige tuple nog niet kregen.
                                // Markeer een addr ALLEEN als 'sent' bij een geslaagde send:
                                // bij een send-fout blijft 'ie ongemarkeerd -> volgende tick
                                // (500 ms) opnieuw, zodat een verse client de initial push
                                // niet permanent misloopt tot de volgende presence-wijziging.
                                for addr in &addrs {
                                    if changed || !sent_addrs.contains(addr) {
                                        if socket.try_send_to(&buf, *addr).is_ok() {
                                            if !changed { info!("Yaesu presence initial push -> {}", addr); }
                                            sent_addrs.insert(*addr);
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

        // Spawn the safety check task
        let safety_handle = {
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
            let dxcluster = self.dxcluster.clone();
            let config = self.config.clone();
            let drive_level_shared = self.drive_level_shared.clone();
            let active_pa_shared = self.active_pa_shared.clone();
            let vfo_freq_shared = self.vfo_freq_shared.clone();
            let vfo_b_freq_shared = self.vfo_b_freq_shared.clone();
            let mut shutdown = self.shutdown.clone();
            let mut tuner_cat_rx = self.tuner_cat_rx.take();
            let yaesu_inner = yaesu.clone();

            let yaesu_ptt_flag = self.yaesu_ptt_flag.clone();

            tokio::spawn(async move {
                let _yaesu = yaesu_inner;
                let yaesu_ptt_flag = yaesu_ptt_flag;
                let mut safety_tick = interval(Duration::from_millis(100));
                let mut cat_tick = interval(Duration::from_secs(1));
                let mut freq_tick = interval(Duration::from_millis(100));
                let mut spectrum_tick = interval(Duration::from_millis(50)); // 20 Hz check rate
                let mut equipment_tick = interval(Duration::from_millis(200));
                // Loss-throttling telt per WERKELIJK gereed frame, per processor.
                // Een tick-teller werkt hier niet: de tick loopt op 20 Hz en de
                // frame-gate op 15 fps, waardoor ready-frames steeds dezelfde
                // pariteit krijgen en de halve-cadans-regel altijd of nooit bijt.
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
                let _last_sync_freq: u64 = 0; // last freq synced B=A
                let mut prev_controls: std::collections::HashMap<u8, u16> = std::collections::HashMap::new();
                let mut prev_client_count: usize = 0;
                let mut prev_smeter_count: usize = 0;
                let mut prev_freq: u64 = 0;
                let mut prev_mode: u8 = 255;
                let mut prev_vfo_b_freq: u64 = 0;
                let mut prev_vfo_b_mode: u8 = 255;
                let mut prev_tx_profile_names: Vec<String> = Vec::new();
                let mut prev_tx_filter: Option<(i32, i32)> = None;
                let mut prev_equipment: std::collections::HashMap<u8, Vec<u8>> = std::collections::HashMap::new();
                let mut prev_eq_client_count: usize = 0;
                // DX-spot dedup + refresh-tracking. Voor deze fix stuurde de
                // server elke equipment_tick (200 ms = 5 Hz) alle cached spots
                // opnieuw naar alle clients - ~90 Kbit/s in steady-state bij
                // een gevulde cache. Nieuwe spots gaan nu meteen door; de
                // age-refresh-pass loopt elke 10 s (zichtbaar genoeg voor de
                // "5m ago"-UI). Bij een nieuwe client wordt prev_spot_keys
                // geleegd zodat de volgende tick een volledige resync stuurt.
                let mut prev_spot_keys: std::collections::HashSet<(String, u64, String)> =
                    std::collections::HashSet::new();
                let mut last_spot_full_refresh = std::time::Instant::now();
                let mut prev_spot_client_count: usize = 0;
                // Reactive RF-power-cap per Amplitec-A positie. Een instantie
                // voor de hele broadcast-task; cap-state + snapshot/restore
                // worden tussen tick-iteraties bewaard. Zie `power_cap.rs`
                // voor de positie->watts tabel + activatievoorwaarden.
                let mut power_cap_state = crate::power_cap::PowerCapState::new();
                // Preventive TX-gate: handle naar de tx_blocked-flag in
                // PttController. We pushen elke broadcast-iteratie de
                // actuele "is huidige Amplitec-positie RX-only?" status
                // hier in zodat alle server-initiated TX-paden
                // (PTT-packet, ZZTX1, ZZTU1) Thetis nooit een TX-on
                // commando sturen op een blocked positie.
                let tx_blocked_gate = ptt.lock().await.tx_blocked_handle();
                // Diagnose: vorige gate-waarde voor on-change logging.
                let mut prev_pos_is_blocked = false;
                // Vorige beschikbaarheid van de rx_only_ex-cap. Wanneer de
                // operator de ThetisLink-extensions checkbox in Thetis
                // aanvinkt terwijl de Amplitec al op een RX-only stand staat,
                // verandert pos_is_blocked niet - dus moeten we op de
                // cap-transitie (afwezig->aanwezig) de RXOnly opnieuw pushen.
                let mut prev_had_rx_only_cap = false;
                // Snapshot van Thetis' RXOnly-staat van vlak voor TL2 'm
                // overnam voor een RX-only positie. `Some(prev)` betekent
                // "TL2 heeft overgenomen, herstel `prev` bij teruggave".
                // `None` = TL2 heeft RXOnly niet overgenomen. Alleen TL2 kent
                // deze pre-state, dus de hele snapshot/restore-beslissing zit
                // hier (de Thetis-fork blijft "dom").
                let mut rx_only_saved: Option<bool> = None;
                // Log de tx_blocked + max_w config-staat een keer bij start
                // van de broadcast-task, zodat we in de log direct zien of de
                // actieve config de power-cap/RX-only waarden uberhaupt heeft.
                {
                    let cfg = crate::config::load();
                    info!(
                        "TX-gate config at start: amplitec_tx_blocked={:?} amplitec_max_w={:?}",
                        cfg.amplitec_tx_blocked, cfg.amplitec_max_w
                    );
                }
                loop {
                    tokio::select! {
                        _ = safety_tick.tick() => {
                            ptt.lock().await.check_safety().await;
                        }
                        _ = spectrum_tick.tick() => {
                            // Alle abonneelijsten in EEN session-lock, voor welke
                            // spectrum-lock dan ook (deadlock-volgorde: de main loop
                            // doet session->spectrum).
                            let (client_info, rx2_client_info, vrx1_spec_addrs, vrx2_spec_addrs) = {
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
                                (rx1, rx2, v1, v2)
                            };
                            // Per-client (PATCH-vrx-per-client): elke VRX-spectrumabonnee
                            // krijgt een venster op ZIJN eigen luisterfrequentie + span.
                            // Snapshot de jobs onder de manager-lock; de extractie gebeurt
                            // daarna zonder die lock vast te houden.
                            let jobs1: Vec<(SocketAddr, u64, u32, u8)> = if vrx1_spec_addrs.is_empty() {
                                Vec::new()
                            } else {
                                let m = vrx_mgr.lock().unwrap();
                                vrx1_spec_addrs.iter().filter_map(|(a, loss)| {
                                    let span = m.spectrum_span(a, 0);
                                    let freq = m.target_freq(a, 0);
                                    (span > 0 && freq > 0).then_some((*a, freq, span as u32 * 1000, *loss))
                                }).collect()
                            };
                            let jobs2: Vec<(SocketAddr, u64, u32, u8)> = if vrx2_spec_addrs.is_empty() {
                                Vec::new()
                            } else {
                                let m = vrx_mgr.lock().unwrap();
                                vrx2_spec_addrs.iter().filter_map(|(a, loss)| {
                                    let span = m.spectrum_span(a, 1);
                                    let freq = m.target_freq(a, 1);
                                    (span > 0 && freq > 0).then_some((*a, freq, span as u32 * 1000, *loss))
                                }).collect()
                            };

                            // Elke stroom heeft zijn EIGEN guard. Voorheen sprong de hele
                            // tick eruit zodra er geen RX1-spectrumabonnees waren, waardoor
                            // RX2- en VRX-spectrum stilvielen terwijl die hun eigen abonnees
                            // hadden - het RX1-spectrum was daarmee de facto hoofdschakelaar.
                            if client_info.is_empty() && rx2_client_info.is_empty()
                                && jobs1.is_empty() && jobs2.is_empty()
                            {
                                continue;
                            }
                            let mut packets_to_send: Vec<(SocketAddr, Vec<u8>)> = Vec::new();

                            // RX1-processor: voedt zowel het RX1-spectrum als VRX1-high-res.
                            // Een frame-gate voor beide, zodat VRX niet sneller gaat lopen dan
                            // de ingestelde fps.
                            if !client_info.is_empty() || !jobs1.is_empty() {
                                let mut sp = spectrum.lock().await;
                                if sp.is_frame_ready() {
                                    rx1_frame_count = rx1_frame_count.wrapping_add(1);
                                    for (addr, zoom, pan, max_bins, loss) in &client_info {
                                        if *loss > 15 { continue; }
                                        if *loss > 5 && rx1_frame_count % 2 != 0 { continue; }
                                        let pkt = sp.extract_view(*zoom, *pan, *max_bins as usize);
                                        let mut buf = Vec::with_capacity(*max_bins as usize + 20);
                                        pkt.serialize(&mut buf);
                                        packets_to_send.push((*addr, buf));
                                        let full_bins = (*max_bins as usize).min(FULL_SPECTRUM_BINS);
                                        let full_pkt = sp.get_full_row(full_bins);
                                        if full_pkt.num_bins > 0 {
                                            let mut full_buf = Vec::with_capacity(full_bins + 20);
                                            full_pkt.serialize_as_type(&mut full_buf, PacketType::FullSpectrum);
                                            packets_to_send.push((*addr, full_buf));
                                        }
                                    }
                                    // VRX1 high-res: alleen clients die VrxSpectrumEnable aan
                                    // hebben gezet krijgen dit packet-type (oude v2.1.x clients
                                    // nooit).
                                    for (a, freq, span_hz, loss) in &jobs1 {
                                        // Zelfde loss-gate als RX: VRX high-res is met 4096 bins
                                        // juist de grootste stroom, dus die moet op een slechte
                                        // link net zo goed afgeknepen worden.
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

                            // RX2-processor: idem voor het RX2-spectrum en VRX2-high-res.
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
                            let eq_addrs = session.lock().await.active_addrs();
                            if eq_addrs.is_empty() { prev_eq_client_count = 0; continue; }
                            // New client: force full equipment sync
                            if eq_addrs.len() != prev_eq_client_count {
                                prev_equipment.clear();
                                prev_eq_client_count = eq_addrs.len();
                            }

                            // Helper: serialize, compare with prev, send only if changed
                            macro_rules! send_if_changed {
                                ($device_id:expr, $pkt:expr, $addrs:expr) => {{
                                    let mut buf = Vec::with_capacity(128);
                                    $pkt.serialize(&mut buf);
                                    let key = $device_id as u8;
                                    if prev_equipment.get(&key).map_or(true, |prev| prev != &buf) {
                                        prev_equipment.insert(key, buf.clone());
                                        for addr in $addrs {
                                            let _ = socket.try_send_to(&buf, *addr);
                                        }
                                    }
                                }};
                            }

                            if let Some(ref amp) = amplitec {
                                let status = amp.status();
                                // Live label-snapshot: `config` lokale is een snapshot
                                // bij `start_server` tijd; bij rename via de Amplitec
                                // context-menu wordt de globale config wel bijgewerkt
                                // maar deze lokale niet. Lees dus elke tick uit de
                                // mutex-protected cache zodat clients de nieuwe namen
                                // krijgen via de send_if_changed dedup.
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
                            // Amplitec power-cap tabel - bij wijziging (of nieuwe
                            // client, via prev_equipment-clear) opnieuw broadcast.
                            // Hergebruikt `send_if_changed!` met een unieke key
                            // buiten de DeviceType range (0xFE).
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
                                let key: u8 = 0xFE; // sentinel buiten DeviceType-range
                                if prev_equipment.get(&key).map_or(true, |prev| prev != &buf) {
                                    prev_equipment.insert(key, buf.clone());
                                    for addr in &eq_addrs {
                                        let _ = socket.try_send_to(&buf, *addr);
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
                            // Tuner: track tune frequency, show stale if VFO moved >25kHz.
                            // Multi-tuner: pick the tuner bound to the active Amplitec-A
                            // position; fall back to the primary (first enabled) tuner so
                            // the panel still works without an Amplitec mapping configured.
                            let active_amplitec_pos = amplitec
                                .as_ref()
                                .map(|a| a.status().switch_a)
                                .filter(|p| (1..=6).contains(p));
                            let active_tuner = active_amplitec_pos
                                .and_then(|p| tuners.for_amplitec_pos(p))
                                .or_else(|| tuners.primary());
                            if let Some(tuner_ref) = active_tuner.as_ref() {
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

                            // Generieke RF-power cap per Amplitec-A positie -
                            // reactieve drive-reductie wanneer de actieve
                            // antenne-positie een max_w heeft in
                            // `config.amplitec_max_w` en de PA-meter daar
                            // boven zit. Ook reactieve TX-block voor
                            // positions die in `config.amplitec_tx_blocked`
                            // gemarkeerd zijn (RX-only antennes).
                            //
                            // Cap-werking: we sturen de PA-EIGEN drive-knop
                            // (zelfde commando als de "-" knop in het SPE/RF2K
                            // tabblad van de client). Niet Thetis ZZPC: de PA
                            // bepaalt de drive via een TCI-loop terug naar
                            // Thetis, dus een ZZPC-verlaging vanuit de server
                            // wordt direct teruggepushed naar de PA-bepaalde
                            // waarde. Alleen actief op de PA die in
                            // `config.active_pa` staat.
                            //
                            // TX-block: als Thetis in TX gaat op een
                            // RX-only positie sturen we direct `ZZTX0;`
                            // (vertaalt naar `trx:0,false;`). Reactief -
                            // preventieve client-side gating komt later.
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
                                let (mode_u8, tx_active) = {
                                    let p = ptt.lock().await;
                                    (p.vfo_a_mode(), p.is_transmitting())
                                };
                                // Preventieve TX-gate: push de actuele
                                // "is huidige Amplitec-positie RX-only?"
                                // status in de Ptt-gate zodat alle
                                // server-initiated PTT-paden geen TX
                                // commando naar Thetis kunnen sturen.
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
                                // Bij verandering van de RX-only status:
                                // push Thetis' "Receive only" preventieve
                                // TX-inhibit via het fork-command `rx_only_ex`.
                                // Met fork-extensions blokkeert Thetis dan ALLE
                                // TX-bronnen (MOX/spatiebalk/hardware-PTT/VOX)
                                // aan de bron - geen TX-window meer. Tegen
                                // stock Thetis (geen cap) returnt set_rx_only
                                // false en blijft de reactieve ZZTX0 hieronder
                                // de enige (best-effort) bescherming.
                                // Snapshot/restore van Thetis' RXOnly. TL2 is
                                // de enige die de pre-overname staat kent, dus
                                // de hele beslissing zit hier; de fork zet
                                // simpelweg wat we sturen.
                                //
                                // - Overname (positie wordt RX-only): bewaar de
                                //   huidige Thetis-RXOnly als snapshot (alleen
                                //   bij de eerste overname), zet RXOnly=true.
                                // - Teruggave (positie niet meer RX-only):
                                //   herstel de snapshot (NIET blind false) -
                                //   zo blijft een handmatig gezette RXOnly
                                //   gerespecteerd.
                                // - Cap verschijnt (extensions uit->aan terwijl
                                //   al op RX-only positie): behandel als
                                //   overname.
                                // - Cap verdwijnt (extensions uit) terwijl TL2
                                //   had overgenomen: we kunnen niet meer
                                //   herstellen (fork negeert rx_only_ex). RXOnly
                                //   blijft veiligheidshalve aan; waarschuw de
                                //   operator i.p.v. de bescherming weg te halen.
                                let has_rx_only_cap =
                                    ptt.lock().await.has_cap("rx_only_ex");
                                let cap_just_appeared =
                                    has_rx_only_cap && !prev_had_rx_only_cap;
                                let cap_just_disappeared =
                                    !has_rx_only_cap && prev_had_rx_only_cap;
                                prev_had_rx_only_cap = has_rx_only_cap;

                                let pos_changed = pos_is_blocked != prev_pos_is_blocked;
                                prev_pos_is_blocked = pos_is_blocked;

                                if has_rx_only_cap && (pos_changed || cap_just_appeared) {
                                    let mut p = ptt.lock().await;
                                    if pos_is_blocked {
                                        if rx_only_saved.is_none() {
                                            // Bij bootstrap (cap_just_appeared, eerste detect
                                            // sinds server-start) is `thetis_rx_only` een
                                            // onbetrouwbare pre-state: het kan stale residu
                                            // zijn van een vorige TL-sessie die zelf RXOnly
                                            // had aangezet en niet opruimde (process-crash,
                                            // PC-reboot, etc.). Default bij bootstrap is dus
                                            // `false` - bij teruggave gaat RXOnly netjes uit
                                            // en blijft Thetis niet vastzitten. Buiten
                                            // bootstrap (mid-sessie cap-toggle) snapshotten
                                            // we wel de actuele staat zodat een handmatige
                                            // RXOnly gerespecteerd blijft.
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
                                        // Bootstrap-edge: cap kwam beschikbaar en TL2 heeft
                                        // niet overgenomen (rx_only_saved is None) - pos is
                                        // niet RX-only. Stuur altijd `rx_only_ex:false`,
                                        // ongeacht `p.thetis_rx_only()`: bij bootstrap komt
                                        // de echo pas seconden NA de cap-detect, dus
                                        // `thetis_rx_only` is hier mogelijk nog z'n default
                                        // `false` terwijl Thetis in werkelijkheid een stale
                                        // RXOnly van een vorige sessie heeft. Een
                                        // onvoorwaardelijke reset is idempotent als Thetis
                                        // al uit was, en ruimt het residu op zonder op de
                                        // echo-timing te wachten. Eventuele bewust-handmatig-
                                        // gezette RXOnly wordt overruled; acceptabel volgens
                                        // de eerdere trade-off - operator kan opnieuw
                                        // aanvinken.
                                        p.set_rx_only(false).await;
                                        info!(
                                            "TX-gate: cleared possible stale RXOnly at bootstrap (pos={:?})",
                                            active_amplitec_pos,
                                        );
                                    }
                                } else if has_rx_only_cap && pos_is_blocked {
                                    // Level-maintain tijdens active takeover. Edge-triggered
                                    // alleen (pos_changed / cap_just_appeared) zou de
                                    // preventieve TX-inhibit verlaten als de operator in
                                    // Thetis handmatig 'Receive only' uitvinkt
                                    // (setup.cs:6493) of een tweede TCI-client een
                                    // `rx_only_ex:false;` stuurt - de bescherming zou dan
                                    // pas bij de volgende positie-wissel terugkomen,
                                    // tot die tijd terugvallend op de reactieve ZZTX0
                                    // catch-all (~100 ms TX-window). Re-assert alleen
                                    // wanneer de TCI-echo aangeeft dat RXOnly daadwerkelijk
                                    // extern is gewist - voorkomt 5 Hz onnodige
                                    // TCI-traffic en herhaalde UI-invokes in de Thetis-
                                    // fork bij stabiele takeover.
                                    let mut p = ptt.lock().await;
                                    if !p.thetis_rx_only() {
                                        p.set_rx_only(true).await;
                                        log::warn!(
                                            "TX-gate: RXOnly externally cleared on blocked pos {:?} - re-asserting preventieve TX-inhibit",
                                            active_amplitec_pos,
                                        );
                                    }
                                }

                                // Extensions uitgezet terwijl TL2 RXOnly had
                                // overgenomen: bescherming blijft (veilig),
                                // maar TL2 kan niet meer herstellen -> waarschuw.
                                if cap_just_disappeared {
                                    if let Some(prev) = rx_only_saved.take() {
                                        log::warn!(
                                            "ThetisLink-extensions uitgezet terwijl RX-only actief was (pre-state was {}). \
                                             Thetis blijft op RXOnly; vink 'Receive only' handmatig uit om te kunnen zenden.",
                                            prev,
                                        );
                                    }
                                }
                                // Reactieve catch-all voor Thetis-direct PTT
                                // (spatiebalk, hardware-PTT op de radio).
                                // Server-initiated paden zijn al via de
                                // preventieve gate hierboven afgesloten;
                                // deze tak vangt alleen TX-events die
                                // buiten ons om bij Thetis zijn ontstaan.
                                if pos_is_blocked && tx_active {
                                    log::warn!(
                                        "Amplitec pos={:?} is RX-only and Thetis is in TX; forcing TX off",
                                        active_amplitec_pos
                                    );
                                    ptt.lock().await.send_cat("ZZTX0").await;
                                }
                                // Switch detector - bij wegschakelen van een
                                // positie met actieve cap-cyclus sturen we
                                // evenveel DriveUp commando's als we
                                // DriveDowns gestuurd hadden, zodat de
                                // PA-drive terug op de pre-cap positie staat.
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
                                // Reactieve cap-check
                                if let Some(action) = crate::power_cap::tick(
                                    &mut power_cap_state,
                                    active_amplitec_pos,
                                    &live_config.amplitec_max_w,
                                    active_pa,
                                    pa_in_operate,
                                    pa_fwd_watts,
                                    mode_u8,
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
                            // Per tick: stuur alleen spots waarvan de
                            // (callsign, freq, mode)-key niet eerder gezien is.
                            // Elke 10 s en bij client-aantal-wijziging:
                            // volledige resync zodat age-velden bijgewerkt
                            // worden en nieuwe clients de cache krijgen.
                            // Spot-addrs gefilterd op `dx_spots_enabled` -
                            // clients kunnen de stream opt-out via de
                            // DxSpotsEnabled control voor metered links.
                            if let Some(ref cluster) = dxcluster {
                                let spots = cluster.spots_for_bands(last_vfo_freq, last_vfo_b_freq);
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
                                    // Garbage-collect: verwijder keys die uit
                                    // de cache zijn verdwenen (geexpireerd)
                                    // zodat prev_spot_keys niet onbegrensd groeit.
                                    prev_spot_keys.retain(|k| current_keys.contains(k));
                                    for spot in &spots {
                                        let key = (spot.callsign.clone(), spot.frequency_hz, spot.mode.clone());
                                        if !prev_spot_keys.insert(key) {
                                            continue; // al verstuurd in eerdere tick
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
                                    // Geen abonnerende clients: reset dedup-state
                                    // zodat een herabonnerende client direct
                                    // een full sync krijgt.
                                    prev_spot_keys.clear();
                                    prev_spot_client_count = 0;
                                }
                                // Forward NEW spots to Thetis via TCI SPOT command (only once per spot).
                                // Onafhankelijk van TL2-client subscriptions -
                                // de fork's eigen DX-spot weergave moet altijd
                                // gevoed worden zolang de DX-cluster aanstaat.
                                if !spots.is_empty() {
                                    let mut new_spots: Vec<&crate::dxcluster::DxSpot> = Vec::new();
                                    for spot in &spots {
                                        let key = (spot.callsign.clone(), spot.frequency_hz);
                                        if tci_spots_sent.insert(key) {
                                            new_spots.push(spot);
                                        }
                                    }
                                    if !new_spots.is_empty() {
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
                            session.lock().await.check_timeout();
                        }
                        _ = freq_tick.tick() => {
                            let ptt_guard = ptt.lock().await;
                            let freq = ptt_guard.vfo_a_freq();
                            let mode = ptt_guard.vfo_a_mode();
                            let is_tx = ptt_guard.is_transmitting();
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
                            let (smeter_addrs, rx2_addrs, rx2_spectrum_addrs, all_addrs) = {
                                let sess = session.lock().await;
                                (sess.smeter_addrs(), sess.rx2_addrs(), sess.rx2_spectrum_addrs(), sess.active_addrs())
                            };
                            // RX2 freq/mode/smeter/controls zijn WEERGAVE-data die zowel de
                            // RX2-audio- als de RX2-spectrum-abonnees nodig hebben. Een
                            // spectrum-zonder-audio-client krijgt anders geen RX2-freq en
                            // rendert het spectrum verkeerd gecentreerd (fase 3b/4-bug).
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
                            // New client or Yaesu mode change: force full state resend
                            if all_addrs.len() != prev_client_count || smeter_addrs.len() != prev_smeter_count {
                                prev_freq = 0;
                                prev_mode = 255;
                                prev_vfo_b_freq = 0;
                                prev_vfo_b_mode = 255;
                                prev_controls.clear();
                                prev_tx_profile_names.clear();
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
                                for addr in &smeter_addrs {
                                    let mask = sess.smeter_sources(*addr);
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

                            // RX2 spectrum-processor center/VFO: ALTIJD bijwerken, los van
                            // RX2-AUDIO-abonnees. Anders blijft de DDC-center op 0 en toont het
                            // RX2-spectrum onzin (wiebelende lijn) zolang niemand RX2-AUDIO aan
                            // heeft — terwijl het RX2-spectrum een eigen abonnement is (fase 3b/4,
                            // net als RX1 dat wél altijd doet).
                            {
                                let mut rx2_sp = rx2_spectrum.lock().await;
                                if vfo_b_freq != 0 { rx2_sp.set_vfo_freq(vfo_b_freq, ctun); }
                                if dds_rx2 != 0 { rx2_sp.set_ddc_center(dds_rx2); }
                            }

                            // RX2 weergave-broadcasts naar audio- EN spectrum-abonnees.
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
        let tx_sinc = rubato::SincInterpolationParameters {
            sinc_len: 128, f_cutoff: 0.95, oversampling_factor: 128,
            interpolation: rubato::SincInterpolationType::Cubic,
            window: rubato::WindowFunction::Blackman,
        };
        let mut tx_resampler_out = rubato::SincFixedIn::<f32>::new(
            playback_rate as f64 / NETWORK_SAMPLE_RATE_WIDEBAND as f64,
            1.0,
            tx_sinc,
            FRAME_SAMPLES_WIDEBAND,
            1,
        )
        .context("create 16k->device TX resampler")?;

        // Main RX loop
        let mut recv_buf = vec![0u8; MAX_PACKET_SIZE];
        let mut opus_decoder = sdr_remote_core::codec::OpusDecoderWideband::new()?;
        let mut jitter_buf = JitterBuffer::new(3, 20);
        let mut tx_holder_addr: Option<SocketAddr> = None;

        let mut shutdown = self.shutdown.clone();
        let mut playout_tick = interval(Duration::from_millis(20));
        let mut pending_filter_low: Option<i32> = None;
        let mut pending_rx2_filter_low: Option<i32> = None;
        let mut pending_tx_filter_low: Option<i32> = None;

        // Yaesu TX audio: forward AudioYaesu packets to a separate decode task
        let mut yaesu_ptt_active = false;
        let mut yaesu_write_pending: Option<String> = None;
        // If the YaesuWriteMemories control arrives BEFORE the
        // YaesuMemoryData packet (the data is ~2.7kB, IP-fragmented;
        // control is ~8B, single packet - control can overtake on
        // reorder), latch the intent and fire the write when the data
        // finally lands. Otherwise the trigger was silently dropped.
        let mut yaesu_write_armed = false;
        let yaesu_mic_gain = Arc::new(AtomicU32::new(1.0_f32.to_bits())); // Yaesu gain now lives before Opus in the client
        // Slot-1 TX audio (Optie B-prime) + geheugen-write-latch (Fase B).
        let mut yaesu2_ptt_active = false;
        let yaesu2_mic_gain = Arc::new(AtomicU32::new(1.0_f32.to_bits()));
        let mut yaesu2_write_pending: Option<String> = None;
        let mut yaesu2_write_armed = false;
        let yaesu_tx_packet_tx = {
            let tx_audio_tx = yaesu.as_ref().and_then(|y| y.tx_audio_tx.clone());
            let tx_rate = yaesu.as_ref().map(|y| y.tx_sample_rate).unwrap_or(0);
            if tx_audio_tx.is_some() && tx_rate > 0 {
                let (pkt_tx, mut pkt_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
                let tx_audio = tx_audio_tx.unwrap();
                let gain_shared = yaesu_mic_gain.clone();
                tokio::spawn(async move {
                    let mut decoder = match sdr_remote_core::codec::OpusDecoderWideband::new() {
                        Ok(d) => d,
                        Err(e) => { log::error!("Yaesu TX wideband decoder init: {}", e); return; }
                    };
                    let sinc_params = rubato::SincInterpolationParameters {
                        sinc_len: 128, f_cutoff: 0.95, oversampling_factor: 128,
                        interpolation: rubato::SincInterpolationType::Cubic,
                        window: rubato::WindowFunction::Blackman,
                    };
                    let mut resampler = match rubato::SincFixedIn::<f32>::new(
                        tx_rate as f64 / NETWORK_SAMPLE_RATE_WIDEBAND as f64, 1.0,
                        sinc_params, FRAME_SAMPLES_WIDEBAND, 1,
                    ) {
                        Ok(r) => r,
                        Err(e) => { log::error!("Yaesu TX resampler init: {}", e); return; }
                    };
                    info!("Yaesu TX task started (wideband Opus 16kHz -> {}Hz)", tx_rate);
                    while let Some(opus_data) = pkt_rx.recv().await {
                        let gain = f32::from_bits(gain_shared.load(Ordering::Relaxed));
                        // Decode wideband Opus (16kHz)
                        let pcm_i16 = match decoder.decode(&opus_data) {
                            Ok(p) => p,
                            Err(e) => { log::warn!("Yaesu TX decode: {}", e); continue; }
                        };
                        // Convert to f32; Yaesu mic gain is applied before Opus in the client.
                        let pcm_f32: Vec<f32> = pcm_i16.iter()
                            .map(|&s| (s as f32 / 32768.0 * gain).clamp(-1.0, 1.0))
                            .collect();
                        // Resample 16kHz -> tx_rate (48kHz)
                        use rubato::Resampler;
                        let resampled = match resampler.process(&[pcm_f32], None) {
                            Ok(r) => r.into_iter().next().unwrap_or_default(),
                            Err(e) => { log::warn!("Yaesu TX resample: {}", e); continue; }
                        };
                        if !resampled.is_empty() {
                            let _ = tx_audio.try_send(resampled);
                        }
                    }
                });
                Some(pkt_tx)
            } else {
                None
            }
        };

        // Slot-1 TX decode task (Optie B-prime) - exacte spiegel van slot 0,
        // gevoed door AudioYaesu2 -> yaesu2's tx_audio_tx.
        let yaesu2_tx_packet_tx = {
            let tx_audio_tx = yaesu2.as_ref().and_then(|y| y.tx_audio_tx.clone());
            let tx_rate = yaesu2.as_ref().map(|y| y.tx_sample_rate).unwrap_or(0);
            if tx_audio_tx.is_some() && tx_rate > 0 {
                let (pkt_tx, mut pkt_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
                let tx_audio = tx_audio_tx.unwrap();
                let gain_shared = yaesu2_mic_gain.clone();
                tokio::spawn(async move {
                    let mut decoder = match sdr_remote_core::codec::OpusDecoderWideband::new() {
                        Ok(d) => d,
                        Err(e) => { log::error!("[radio1] TX wideband decoder init: {}", e); return; }
                    };
                    let sinc_params = rubato::SincInterpolationParameters {
                        sinc_len: 128, f_cutoff: 0.95, oversampling_factor: 128,
                        interpolation: rubato::SincInterpolationType::Cubic,
                        window: rubato::WindowFunction::Blackman,
                    };
                    let mut resampler = match rubato::SincFixedIn::<f32>::new(
                        tx_rate as f64 / NETWORK_SAMPLE_RATE_WIDEBAND as f64, 1.0,
                        sinc_params, FRAME_SAMPLES_WIDEBAND, 1,
                    ) {
                        Ok(r) => r,
                        Err(e) => { log::error!("[radio1] TX resampler init: {}", e); return; }
                    };
                    info!("[radio1] TX task started (wideband Opus 16kHz -> {}Hz)", tx_rate);
                    while let Some(opus_data) = pkt_rx.recv().await {
                        let gain = f32::from_bits(gain_shared.load(Ordering::Relaxed));
                        let pcm_i16 = match decoder.decode(&opus_data) {
                            Ok(p) => p,
                            Err(e) => { log::warn!("[radio1] TX decode: {}", e); continue; }
                        };
                        let pcm_f32: Vec<f32> = pcm_i16.iter()
                            .map(|&s| (s as f32 / 32768.0 * gain).clamp(-1.0, 1.0))
                            .collect();
                        use rubato::Resampler;
                        let resampled = match resampler.process(&[pcm_f32], None) {
                            Ok(r) => r.into_iter().next().unwrap_or_default(),
                            Err(e) => { log::warn!("[radio1] TX resample: {}", e); continue; }
                        };
                        if !resampled.is_empty() {
                            let _ = tx_audio.try_send(resampled);
                        }
                    }
                });
                Some(pkt_tx)
            } else {
                None
            }
        };

        loop {
            tokio::select! {
                result = self.socket.recv_from(&mut recv_buf) => {
                    let (len, addr) = match result {
                        Ok(v) => v,
                        // Windows UDP quirk: when a client (peer) disappears, a prior
                        // send to its now-closed port makes the NEXT recv_from return
                        // WSAECONNRESET / WSAECONNABORTED (ErrorKind::ConnectionReset /
                        // ConnectionAborted). It is a spurious per-datagram error, not a
                        // real socket failure — skip it and keep serving the other
                        // clients instead of tearing the whole server down (fixes the
                        // "force-stop the app -> server crashes" report).
                        Err(e) if matches!(
                            e.kind(),
                            std::io::ErrorKind::ConnectionReset
                                | std::io::ErrorKind::ConnectionAborted
                        ) => {
                            log::debug!("recv_from transient error (ignored): {}", e);
                            continue;
                        }
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
                            // Unknown packet types from old clients - silently ignore
                            if !e.to_string().contains("unknown packet type") {
                                info!("Invalid packet from {} ({}B): {}", addr, len, e);
                            }
                            continue;
                        }
                    };

                    // --- Authentication gate ---
                    {
                        let mut session = self.session.lock().await;

                        // Rate-limit check
                        if session.is_blocked(addr) {
                            continue; // Silently drop
                        }

                        // Handle auth packets regardless of state
                        match &packet {
                            Packet::AuthResponse(hmac) => {
                                let result = session.verify_auth(addr, hmac);
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
                                // Send AuthResult
                                let mut buf = [0u8; 5];
                                let header = Header::new(PacketType::AuthResult, Flags::NONE);
                                let mut hdr = [0u8; 4];
                                header.serialize(&mut hdr);
                                buf[..4].copy_from_slice(&hdr);
                                buf[4] = result;
                                let _ = self.socket.try_send_to(&buf, addr);
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
                                let accepted = session.verify_totp(addr, code);
                                // PATCH-2: record TOTP outcome in Status-panel ringbuffer.
                                let outcome = if accepted {
                                    crate::session::ConnectOutcome::Accepted
                                } else {
                                    crate::session::ConnectOutcome::WrongTotp
                                };
                                session.record_connect_attempt(addr, outcome);
                                drop(session);
                                // Send AuthResult with final verdict
                                let mut buf = [0u8; 5];
                                let header = Header::new(PacketType::AuthResult, Flags::NONE);
                                let mut hdr = [0u8; 4];
                                header.serialize(&mut hdr);
                                buf[..4].copy_from_slice(&hdr);
                                buf[4] = if accepted { sdr_remote_core::protocol::AUTH_ACCEPTED } else { sdr_remote_core::protocol::AUTH_REJECTED };
                                let _ = self.socket.try_send_to(&buf, addr);
                                continue;
                            }
                            _ => {}
                        }

                        if session.auth_required() {
                            if !session.is_authenticated(addr) {
                                // New unknown client or pending challenge - send challenge
                                // Don't overwrite PendingTotp state with a new challenge
                                if session.get_auth_state(addr).is_none()
                                    || matches!(session.get_auth_state(addr), Some(crate::session::AuthState::NoAuth)) {
                                    let nonce = session.create_challenge(addr);
                                    // PATCH-2: record diagnostic "client started handshake".
                                    session.record_connect_attempt(
                                        addr,
                                        crate::session::ConnectOutcome::ChallengeSent,
                                    );
                                    drop(session);
                                    // Send AuthChallenge
                                    let mut buf = [0u8; 20];
                                    let header = Header::new(PacketType::AuthChallenge, Flags::NONE);
                                    let mut hdr = [0u8; 4];
                                    header.serialize(&mut hdr);
                                    buf[..4].copy_from_slice(&hdr);
                                    buf[4..20].copy_from_slice(&nonce);
                                    let _ = self.socket.try_send_to(&buf, addr);
                                }
                                continue; // Drop all non-auth packets from unauthenticated clients
                            }
                        }

                        session.touch(addr);
                    }

                    match packet {
                        Packet::Audio(audio_pkt) => {
                            // Thetis-only audio path - Yaesu TX handled via AudioYaesu packets
                            let ptt_requested = audio_pkt.flags.ptt();
                            let mut session = self.session.lock().await;

                            if ptt_requested {
                                if !session.try_acquire_tx(addr) {
                                    drop(session);
                                    let mut buf = [0u8; PttDeniedPacket::SIZE];
                                    PttDeniedPacket::serialize(&mut buf);
                                    let _ = self.socket.send_to(&buf, addr).await;
                                    continue;
                                }
                                drop(session);

                                if tx_holder_addr != Some(addr) {
                                    info!("New TX holder {}, resetting jitter buffer and decoder", addr);
                                    jitter_buf.reset();
                                    opus_decoder = sdr_remote_core::codec::OpusDecoderWideband::new().unwrap();
                                    tx_holder_addr = Some(addr);
                                }

                                self.ptt.lock().await.record_ptt_packet();

                                let arrival_ms = start.elapsed().as_millis() as u64;
                                let wideband = audio_pkt.flags.wideband();
                                jitter_buf.push(
                                    BufferedFrame {
                                        sequence: audio_pkt.sequence,
                                        timestamp: audio_pkt.timestamp,
                                        opus_data: audio_pkt.opus_data,
                                        ptt: true,
                                        wideband,
                                    },
                                    arrival_ms,
                                );
                            } else if session.tx_holder() == Some(addr) {
                                // This client held TX - push non-PTT frame so tail
                                // audio plays out, then release will trigger from playout
                                drop(session);

                                self.ptt.lock().await.record_ptt_packet();

                                let arrival_ms = start.elapsed().as_millis() as u64;
                                let wideband = audio_pkt.flags.wideband();
                                jitter_buf.push(
                                    BufferedFrame {
                                        sequence: audio_pkt.sequence,
                                        timestamp: audio_pkt.timestamp,
                                        opus_data: audio_pkt.opus_data,
                                        ptt: false,
                                        wideband,
                                    },
                                    arrival_ms,
                                );
                            }
                            // else: non-PTT audio from non-TX-holder - ignore
                        }
                        Packet::Heartbeat(hb) => {
                            let thetis_configured = self.config.tci_addr
                                .as_deref()
                                .map(|s| !s.trim().is_empty())
                                .unwrap_or(false);
                            // Acquire PTT lock once: signal heartbeat received + read TCI/process/launch status
                            // in the same scope so we don't lock multiple times per heartbeat.
                            let (tci_connected, thetis_running, thetis_starting) = {
                                let mut ptt = self.ptt.lock().await;
                                ptt.heartbeat_received();
                                if thetis_configured {
                                    (ptt.tci_connected(), ptt.thetis_process_running(), ptt.thetis_starting())
                                } else {
                                    (false, false, false)
                                }
                            };
                            // PATCH-2: feed the Status-panel TCI probe.
                            // Idempotent - only stamps last_state_change on real flips.
                            self.tci_probe.update(tci_connected, self.server_start);
                            self.session.lock().await.update_heartbeat(
                                addr,
                                hb.sequence,
                                hb.rtt_ms,
                                hb.loss_percent,
                                hb.jitter_ms,
                            );

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
                            // Single-receiver radio: advertise so clients hide RX2 + VRX2.
                            // Inverted flag (absent = normal 2-RX default). Startup snapshot;
                            // changing it in the server GUI applies on the next server start.
                            if !self.config.rx2_present {
                                state_flags = state_flags
                                    .with(sdr_remote_core::protocol::ServerStateFlags::SINGLE_RECEIVER);
                            }

                            // PATCH-1 review finding (B3): advertise REPORTS_STATE_FLAGS
                            // so the client knows the state_flags field is authoritative.
                            // Old servers (pre-PATCH-1) leave both at NONE - client must
                            // NOT then assume "TCI down" from an absent flag.
                            let ack = HeartbeatAck {
                                flags: Flags::NONE,
                                echo_sequence: hb.sequence,
                                echo_time: hb.local_time,
                                capabilities: Capabilities::NONE
                                    .with(Capabilities::REPORTS_STATE_FLAGS),
                                state_flags,
                            };
                            let mut ack_buf = [0u8; HeartbeatAck::SIZE];
                            ack.serialize(&mut ack_buf);
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
                                info!("Client {} VFO-A mode change ignored during TX", addr);
                            } else {
                                ptt.set_vfo_a_mode(mode_pkt.mode).await;
                            }
                        }
                        Packet::Smeter(_) => {} // server-only, ignore from clients
                        Packet::Spectrum(_) | Packet::FullSpectrum(_) => {} // server-only, ignore from clients
                        Packet::EquipmentStatus(_) => {} // server-only, ignore from clients
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
                                            // Hoeft geen amplitec device aanwezig; de tabel
                                            // is server-config en geldt zodra de Amplitec
                                            // weer online komt.
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
                                        let spe_cmd = match eq_cmd.command_id {
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
                                                Some(crate::rf2k::Rf2kCmd::SetOperate(to_operate))
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
                                            CMD_RF2K_SET_DRIVE_SSB => {
                                                if eq_cmd.data.len() >= 2 {
                                                    Some(crate::rf2k::Rf2kCmd::SetDriveConfig { category: 0, band: eq_cmd.data[0], value: eq_cmd.data[1] })
                                                } else { None }
                                            }
                                            CMD_RF2K_SET_DRIVE_AM => {
                                                if eq_cmd.data.len() >= 2 {
                                                    Some(crate::rf2k::Rf2kCmd::SetDriveConfig { category: 1, band: eq_cmd.data[0], value: eq_cmd.data[1] })
                                                } else { None }
                                            }
                                            CMD_RF2K_SET_DRIVE_CONT => {
                                                if eq_cmd.data.len() >= 2 {
                                                    Some(crate::rf2k::Rf2kCmd::SetDriveConfig { category: 2, band: eq_cmd.data[0], value: eq_cmd.data[1] })
                                                } else { None }
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
                        Packet::HeartbeatAck(_) | Packet::PttDenied => {}
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
                        | Packet::AuthChallenge(_) | Packet::AuthResponse(_) | Packet::AuthResult(_)
                        | Packet::TotpChallenge | Packet::TotpResponse(_) => {}
                        Packet::YaesuMemoryData(text) => {
                            if text.starts_with("SETMENU:") {
                                // Direct menu set: "SETMENU:nnn:value"
                                if let Some(ref yaesu) = yaesu {
                                    let parts: Vec<&str> = text[8..].splitn(2, ':').collect();
                                    if parts.len() == 2 {
                                        if let Ok(num) = parts[0].parse::<u16>() {
                                            info!("Client {} set menu {:03} = {}", addr, num, parts[1]);
                                            yaesu.send_command(crate::yaesu::YaesuCmd::SetMenu(num, parts[1].to_string()));
                                        }
                                    }
                                }
                            } else {
                                info!("Received Yaesu memory data from client ({}B)", text.len());
                                if yaesu_write_armed {
                                    yaesu_write_armed = false;
                                    if let Some(ref yaesu) = yaesu {
                                        info!("Client {} writing Yaesu memories (deferred - data arrived after control)", addr);
                                        yaesu.send_command(crate::yaesu::YaesuCmd::WriteAllMemories(text));
                                    }
                                } else {
                                    yaesu_write_pending = Some(text);
                                }
                            }
                        }
                        // Yaesu TX audio: forward to separate decode task
                        Packet::AudioYaesu(pkt) => {
                            if yaesu_ptt_active && !pkt.opus_data.is_empty() {
                                if let Some(ref tx) = yaesu_tx_packet_tx {
                                    // Only tick on successful enqueue - dropped
                                    // frames (channel full) didn't reach Yaesu.
                                    if tx.try_send(pkt.opus_data).is_ok() {
                                        self.audio_stats.yaesu_tx.tick(self.server_start);
                                    }
                                }
                            }
                        }
                        Packet::FrequencyYaesu(freq_pkt) => {
                            if let Some(ref yaesu) = yaesu {
                                // Altijd doorsturen: SetFreqA beslist zelf. In memory-mode
                                // glijdt hij via de memory-escape (MA;MD;FA) naar VFO, in
                                // VFO stuurt hij kale FA. (Vroeger blokkeerde hier een guard
                                // op vfo_select==1, wat de gewenste memory->VFO-escape juist
                                // tegenhield - de escape zat dan als dode code in SetFreqA.)
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
                        // Dual-radio slot 1 (Optie B-prime) - spiegel van AudioYaesu/FrequencyYaesu.
                        Packet::AudioYaesu2(pkt) => {
                            if yaesu2_ptt_active && !pkt.opus_data.is_empty() {
                                if let Some(ref tx) = yaesu2_tx_packet_tx {
                                    if tx.try_send(pkt.opus_data).is_ok() {
                                        self.audio_stats.yaesu_tx.tick(self.server_start);
                                    }
                                }
                            }
                        }
                        Packet::FrequencyYaesu2(freq_pkt) => {
                            if let Some(ref yaesu) = yaesu2 {
                                // Altijd doorsturen: SetFreqA regelt memory-escape vs kale FA
                                // zelf (zie FrequencyYaesu hierboven).
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
                        // Slot-1 geheugen-write (Fase B): client stuurt de tab-text.
                        // Fire direct als de write-control al kwam (armed), anders
                        // latchen tot Yaesu2WriteMemories (UDP-reorder-safe, zoals slot 0).
                        Packet::YaesuMemoryData2(text) => {
                            if let Some(rest) = text.strip_prefix("SETMENU:") {
                                // FTX-1 EX-write: "SETMENU:p1p2p3:value" -> EX{p1p2p3}{value};
                                if let Some(ref yaesu) = yaesu2 {
                                    let parts: Vec<&str> = rest.splitn(2, ':').collect();
                                    if parts.len() == 2 && parts[0].len() == 6
                                        && parts[0].chars().all(|c| c.is_ascii_digit())
                                    {
                                        info!("Client {} [radio1] set EX {} = {}", addr, parts[0], parts[1]);
                                        yaesu.send_command(crate::yaesu::YaesuCmd::RawCat(
                                            format!("EX{}{};", parts[0], parts[1])));
                                    }
                                }
                            } else if yaesu2_write_armed {
                                yaesu2_write_armed = false;
                                if let Some(ref yaesu) = yaesu2 {
                                    info!("Client {} [radio1] writing memories (deferred)", addr);
                                    yaesu.send_command(crate::yaesu::YaesuCmd::WriteAllMemories(text));
                                }
                            } else {
                                yaesu2_write_pending = Some(text);
                            }
                        }
                        Packet::Disconnect => {
                            info!("Client {} disconnected", addr);
                            self.session.lock().await.remove(addr);
                            // TL2-1 ctun-auto-recenter: herbereken effective_zoom +
                            // strictest-vink na disconnect - laatste vink-uit client kan
                            // zojuist weg zijn waardoor zoom-min van 2.0 -> 1.0 mag.
                            let new_eff_rx1 = self.session.lock().await.effective_zoom_rx1();
                            let new_eff_rx2 = self.session.lock().await.effective_zoom_rx2();
                            let mut p = self.ptt.lock().await;
                            p.tci.effective_zoom_rx1_cache = new_eff_rx1;
                            p.tci.effective_zoom_rx2_cache = new_eff_rx2;
                            // Geen trigger_eval bij disconnect - eerstvolgende vfo-event of
                            // remaining-client-zoom-change zal eval triggeren.
                        }
                        Packet::Control(ctrl) => {
                            let mut ptt = self.ptt.lock().await;
                            match ctrl.control_id {
                                ControlId::Rx1AfGain => {
                                    let val = ctrl.value.min(100);
                                    // rx_volume via TCI (stock v2.10.3.13+).
                                    // Schaal: 0..100 % -> -60..0 dB (matches parser in tci.rs RxVolume handler).
                                    let db = ((val as i32 - 100) * 60) / 100;
                                    let cmd = format!("rx_volume:0,0,{};", db);
                                    ptt.send_cat(&cmd).await;
                                    // No optimistic state update - Thetis echoes rx_volume back
                                    // and the parser updates rx_af_gain via the notification path.
                                }
                                ControlId::PowerOnOff => {
                                    if ctrl.value == 2 {
                                        // Shutdown Thetis via TCI (v2.10.3.13+)
                                        info!("Client {} requested Thetis shutdown", addr);
                                        ptt.send_cat("shutdown_ex;").await;
                                    } else {
                                        ptt.set_power(ctrl.value != 0).await;
                                    }
                                }
                                ControlId::TxProfile => {
                                    ptt.set_tx_profile(ctrl.value.min(99) as u8).await;
                                }
                                ControlId::ThetisTxeq => {
                                    // Thetis TX-EQ bypass tijdens WAV-playback naar de
                                    // hoofdradio (over TCI run_cat_ex). value 0 = start
                                    // bypass: query de huidige ZZET-stand, bewaar 'm en
                                    // zet TX-EQ uit. value 1 = einde: herstel exact de
                                    // bewaarde stand - net als Thetis' eigen playback.
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
                                    info!("Client {} spectrum: {}", addr, if enabled { "ON" } else { "OFF" });
                                }
                                ControlId::SpectrumFps => {
                                    let fps = (ctrl.value as u8).clamp(5, 30);
                                    self.session.lock().await.set_spectrum_fps(addr, fps);
                                    let max_fps = self.session.lock().await.spectrum_max_fps();
                                    self.spectrum.lock().await.set_fps(max_fps);
                                    info!("Client {} spectrum fps: {}", addr, fps);
                                }
                                ControlId::SpectrumZoom => {
                                    let zoom = ctrl.value as f32 / 10.0;
                                    let prev_eff = self.session.lock().await.effective_zoom_rx1();
                                    self.session.lock().await.set_spectrum_zoom(addr, zoom);
                                    let new_eff = self.session.lock().await.effective_zoom_rx1();
                                    info!("TCI: client {} rx1 zoom={:.1}x; effective_rx1={:?} (was {:?})",
                                          addr, zoom, new_eff, prev_eff);
                                    // TL2-1 ctun-auto-recenter: update cached effective_zoom + trigger eval.
                                    // Gebruik bestaande outer `ptt`-guard (regel ~1572
                                    // `let mut ptt = self.ptt.lock().await;`) - Tokio mutex is niet
                                    // reentrant, nested self.ptt.lock() = deadlock.
                                    ptt.tci.effective_zoom_rx1_cache = new_eff;
                                    ptt.tci.trigger_eval_and_act_rx1(new_eff).await;
                                }
                                ControlId::SpectrumPan => {
                                    let pan = ctrl.value as f32 / 10000.0 - 0.5;
                                    self.session.lock().await.set_spectrum_pan(addr, pan);
                                }
                                ControlId::FilterLow => {
                                    // Buffer low edge; send combined with high edge
                                    pending_filter_low = Some(ctrl.value as i16 as i32);
                                }
                                ControlId::FilterHigh => {
                                    let high = ctrl.value as i16 as i32;
                                    let low = pending_filter_low.take()
                                        .unwrap_or(ptt.filter_low_hz());
                                    info!("Client {} filter: {} .. {} Hz", addr, low, high);
                                    ptt.set_filter(low, high).await;
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
                                    info!("Client {} TX filter: {} .. {} Hz", addr, lo, hi);
                                    ptt.set_tx_filter_band(lo, hi).await;
                                }
                                ControlId::ThetisStarting => {} // server->client only
                                ControlId::Rx1Enable => {
                                    let enabled = ctrl.value != 0;
                                    self.session.lock().await.set_rx1_enabled(addr, enabled);
                                    log::info!("RX1 audio-abonnement {} voor {}", if enabled { "AAN" } else { "UIT" }, addr);
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
                                    info!("Client {} RX2: {}", addr, if enabled { "ON" } else { "OFF" });
                                }
                                ControlId::Rx2AfGain => {
                                    let val = ctrl.value.min(100);
                                    ptt.set_rx2_af_gain(val as u8).await;
                                }
                                ControlId::Rx2SpectrumEnable => {
                                    let enabled = ctrl.value != 0;
                                    self.session.lock().await.set_rx2_spectrum_enabled(addr, enabled);
                                    self.refresh_spectrum_processors().await;
                                    // Stuur de HUIDIGE VFO-B freq/mode direct mee (initiële
                                    // snapshot, net als bij Rx2Enable): een spectrum-zonder-audio-
                                    // client heeft de RX2-freq nodig om correct te centreren, en de
                                    // change-gated broadcast stuurt 'm anders pas bij de volgende
                                    // freq-wijziging.
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
                                    info!("Client {} RX2 spectrum: {}", addr, if enabled { "ON" } else { "OFF" });
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
                                    // Gebruik outer ptt-guard (geen nested lock).
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
                                    // Vink-toggle wijzigt server_enforced_zoom_min -> effective_zoom
                                    // kan veranderen -> herbereken cache + trigger eval voor beide RX.
                                    // Geen nested lock (outer ptt-guard reeds beschikbaar).
                                    let new_eff_rx1 = self.session.lock().await.effective_zoom_rx1();
                                    let new_eff_rx2 = self.session.lock().await.effective_zoom_rx2();
                                    ptt.tci.effective_zoom_rx1_cache = new_eff_rx1;
                                    ptt.tci.effective_zoom_rx2_cache = new_eff_rx2;
                                    ptt.tci.trigger_eval_and_act_rx1(new_eff_rx1).await;
                                    ptt.tci.trigger_eval_and_act_rx2(new_eff_rx2).await;
                                }
                                ControlId::SmeterSources => {
                                    let mask = ctrl.value;
                                    info!("Client {} S-meter sources mask: 0x{:02x}", addr, mask);
                                    self.session.lock().await.set_smeter_sources(addr, mask);
                                }
                                ControlId::DxSpotsEnabled => {
                                    let enabled = ctrl.value != 0;
                                    info!("Client {} DX spots: {}", addr, if enabled { "ON" } else { "OFF" });
                                    self.session.lock().await.set_dx_spots_enabled(addr, enabled);
                                }
                                ControlId::ThetisWidebandAudio => {
                                    let on = ctrl.value != 0;
                                    info!("Client {} Thetis wideband audio: {}", addr, if on { "ON" } else { "OFF" });
                                    self.session.lock().await.set_thetis_wideband(addr, on);
                                }
                                ControlId::Rx2SpectrumPan => {
                                    let pan = ctrl.value as f32 / 10000.0 - 0.5;
                                    self.session.lock().await.set_rx2_spectrum_pan(addr, pan);
                                }
                                ControlId::Rx2FilterLow => {
                                    pending_rx2_filter_low = Some(ctrl.value as i16 as i32);
                                }
                                ControlId::Rx2FilterHigh => {
                                    let high = ctrl.value as i16 as i32;
                                    let low = pending_rx2_filter_low.take()
                                        .unwrap_or(ptt.filter_rx2_low_hz());
                                    info!("Client {} RX2 filter: {} .. {} Hz", addr, low, high);
                                    ptt.set_rx2_filter(low, high).await;
                                }
                                ControlId::VfoSync => {
                                    let enabled = ctrl.value != 0;
                                    self.session.lock().await.set_vfo_sync(addr, enabled);
                                    info!("Client {} VFO sync: {}", addr, if enabled { "ON" } else { "OFF" });
                                    // Delay before sending ZZSY to let frequencies settle
                                    tokio::time::sleep(Duration::from_millis(200)).await;
                                    ptt.set_vfo_sync_thetis(enabled).await;
                                }
                                ControlId::MonitorOn => {
                                    let on = ctrl.value != 0;
                                    ptt.set_mon(on).await;
                                    info!("Client {} MON: {}", addr, if on { "ON" } else { "OFF" });
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
                                    info!("Client {} audio mode: {}", addr, ctrl.value);
                                    self.session.lock().await.set_audio_mode(addr, ctrl.value as u8);
                                }
                                ControlId::VfoSwap => {
                                    ptt.vfo_swap().await;
                                }
                                ControlId::ThetisTune => {
                                    let on = ctrl.value != 0;
                                    info!("Client {} Thetis TUNE {}", addr, if on { "ON" } else { "OFF" });
                                    ptt.set_tune(on).await;
                                }
                                ControlId::DiversityRead => {
                                    info!("Client {} reading diversity state", addr);
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
                                    info!("Client {} AGC mode: {}", addr, ctrl.value);
                                    ptt.set_agc_mode(ctrl.value.min(5) as u8).await;
                                }
                                ControlId::AgcGain => {
                                    info!("Client {} AGC gain: {}", addr, ctrl.value);
                                    ptt.set_agc_gain(ctrl.value.min(120) as u8).await;
                                }
                                ControlId::RitEnable => {
                                    info!("Client {} RIT: {}", addr, if ctrl.value != 0 { "ON" } else { "OFF" });
                                    ptt.set_rit_enable(ctrl.value != 0).await;
                                }
                                ControlId::RitOffset => {
                                    ptt.set_rit_offset(ctrl.value as i16 as i32).await;
                                }
                                ControlId::XitEnable => {
                                    info!("Client {} XIT: {}", addr, if ctrl.value != 0 { "ON" } else { "OFF" });
                                    ptt.set_xit_enable(ctrl.value != 0).await;
                                }
                                ControlId::XitOffset => {
                                    ptt.set_xit_offset(ctrl.value as i16 as i32).await;
                                }
                                ControlId::SqlEnable => {
                                    info!("Client {} SQL: {}", addr, if ctrl.value != 0 { "ON" } else { "OFF" });
                                    ptt.set_sql_enable(ctrl.value != 0).await;
                                }
                                ControlId::SqlLevel => {
                                    // Client sends 0..160, map to Thetis -140..0 dB
                                    let db = (ctrl.value as i16) - 140;
                                    ptt.set_sql_level(db.clamp(-140, 0)).await;
                                }
                                ControlId::NoiseBlanker => {
                                    let level = ctrl.value.min(2) as u8;
                                    info!("Client {} NB: {}", addr, match level { 0 => "OFF", 1 => "NB1", _ => "NB2" });
                                    ptt.set_nb(level).await;
                                }
                                ControlId::CwKeyerSpeed => {
                                    info!("Client {} CW speed: {} WPM", addr, ctrl.value);
                                    ptt.set_cw_keyer_speed(ctrl.value.clamp(1, 60) as u8).await;
                                }
                                ControlId::CwKey => {
                                    let pressed = (ctrl.value & 1) != 0;
                                    let duration_ms = ctrl.value >> 1;
                                    let dur = if duration_ms > 0 { Some(duration_ms) } else { None };
                                    info!("Client {} CW key: {} dur={:?}", addr, if pressed { "DOWN" } else { "UP" }, dur);
                                    ptt.cw_key(pressed, dur).await;
                                }
                                ControlId::CwMacroStop => {
                                    info!("Client {} CW macro stop", addr);
                                    ptt.cw_macro_stop().await;
                                }
                                ControlId::VfoLock => {
                                    info!("Client {} VFO Lock: {}", addr, if ctrl.value != 0 { "ON" } else { "OFF" });
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
                                        info!("Client {} BIN: {}", addr, if new_on { "ON" } else { "OFF" });
                                    }
                                    ptt.set_binaural(new_on).await;
                                }
                                ControlId::ApfEnable => {
                                    info!("Client {} APF: {}", addr, if ctrl.value != 0 { "ON" } else { "OFF" });
                                    ptt.set_apf_enable(ctrl.value != 0).await;
                                }
                                ControlId::Mute => {
                                    info!("Client {} MUTE: {}", addr, if ctrl.value != 0 { "ON" } else { "OFF" });
                                    ptt.set_mute(ctrl.value != 0).await;
                                }
                                ControlId::RxMute => {
                                    info!("Client {} RX MUTE: {}", addr, if ctrl.value != 0 { "ON" } else { "OFF" });
                                    ptt.set_rx_mute(ctrl.value != 0).await;
                                }
                                ControlId::ManualNotchFilter => {
                                    info!("Client {} NF: {}", addr, if ctrl.value != 0 { "ON" } else { "OFF" });
                                    ptt.set_nf_enable(ctrl.value != 0).await;
                                }
                                ControlId::RxBalance => {
                                    let val = ctrl.value as i16 as i8;
                                    info!("Client {} RX Balance: {}", addr, val);
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
                                    info!("Client {} Tune drive: {}%", addr, ctrl.value);
                                    ptt.set_tune_drive(ctrl.value.min(100) as u8).await;
                                }
                                ControlId::MonitorVolume => {
                                    let db = ctrl.value as i16 as i8;
                                    info!("Client {} Mon volume: {} dB", addr, db);
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
                                    info!("Client {} Yaesu audio: {}", addr, if enabled { "ON" } else { "OFF" });
                                }
                                ControlId::YaesuStateEnable => {
                                    let enabled = ctrl.value != 0;
                                    self.session.lock().await.set_yaesu_state_enabled(addr, enabled);
                                }
                                ControlId::Yaesu2StateEnable => {
                                    let enabled = ctrl.value != 0;
                                    self.session.lock().await.set_yaesu2_state_enabled(addr, enabled);
                                }
                                ControlId::YaesuPowerOnOff => {
                                    if let Some(ref yaesu) = yaesu {
                                        let on = ctrl.value != 0;
                                        yaesu.send_command(crate::yaesu::YaesuCmd::SetPower(on));
                                        info!("Client {} Yaesu power: {}", addr, if on { "ON" } else { "OFF" });
                                    }
                                }
                                ControlId::Yaesu2PowerOnOff => {
                                    if let Some(ref yaesu2) = yaesu2 {
                                        let on = ctrl.value != 0;
                                        yaesu2.send_command(crate::yaesu::YaesuCmd::SetPower(on));
                                        info!("Client {} Yaesu2 power: {}", addr, if on { "ON" } else { "OFF" });
                                    }
                                }
                                ControlId::YaesuPtt => {
                                    if let Some(ref yaesu) = yaesu {
                                        let on = ctrl.value != 0;
                                        // Auto-DFM (FM <-> DATA-FM) wordt nu volledig
                                        // afgehandeld in YaesuCmd::SetPtt zelf (build 12) -
                                        // single source of truth voor mode-toggle, geen race
                                        // tussen network.rs en yaesu_poll_loop. Memory-mode-skip
                                        // zit ook in SetPtt.
                                        yaesu.send_command(crate::yaesu::YaesuCmd::SetPtt(on));
                                        yaesu_ptt_active = on;
                                        self.yaesu_ptt_flag.store(on, Ordering::Relaxed);
                                        info!("Client {} Yaesu PTT: {}", addr, if on { "TX" } else { "RX" });
                                    }
                                }
                                ControlId::YaesuFreq => {} // handled via FrequencyYaesu packet
                                ControlId::YaesuMicGain => {
                                    // Legacy server-side gain control; current clients apply Yaesu mic gain before Opus.
                                    // Kept only for older clients that still send this control.
                                    let gain = ctrl.value as f32 / 100.0;
                                    yaesu_mic_gain.store(gain.to_bits(), Ordering::Relaxed);
                                    info!("Client {} Yaesu legacy server gain: {:.2}x", addr, gain);
                                }
                                ControlId::YaesuMode => {
                                    if let Some(ref yaesu) = yaesu {
                                        yaesu.send_command(crate::yaesu::YaesuCmd::SetMode(ctrl.value as u8));
                                        info!("Client {} Yaesu mode: {}", addr, ctrl.value);
                                    }
                                }
                                ControlId::YaesuReadMemories => {
                                    if let Some(ref yaesu) = yaesu {
                                        info!("Client {} requested Yaesu memory read", addr);
                                        yaesu.send_command(crate::yaesu::YaesuCmd::ReadAllMemories);
                                    }
                                }
                                ControlId::YaesuRecallMemory => {
                                    if let Some(ref yaesu) = yaesu {
                                        info!("Client {} Yaesu recall memory {}", addr, ctrl.value);
                                        yaesu.send_command(crate::yaesu::YaesuCmd::RecallMemory(ctrl.value));
                                    }
                                }
                                ControlId::YaesuSelectVfo => {
                                    if let Some(ref yaesu) = yaesu {
                                        info!("Client {} Yaesu VFO: {}", addr, match ctrl.value { 0 => "A", 1 => "B", _ => "swap" });
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
                                        let cat = match ctrl.value {
                                            0 => "AB;",      // A=B
                                            1 => "SC1;",     // Scan start
                                            2 => "SC0;",     // Scan stop
                                            3 => "AC002;",   // Tune now (991A: start tuning)
                                            4 => "AC000;",   // Tuner off (bypass)
                                            15 => "AC001;",  // Tuner on (in-line) - PATCH-yaesu-internal-atu
                                            5 => "BU0;",     // Band up (VFO-A)
                                            6 => "BD0;",     // Band down (VFO-A)
                                            7 => "ST1;",     // Split on
                                            8 => "ST0;",     // Split off
                                            _ => "",
                                        };
                                        if !cat.is_empty() {
                                            info!("Client {} Yaesu button {}: {}", addr, ctrl.value, cat);
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
                                            let next = next_memory_channel(&filled, cur, ctrl.value == 9, 117);
                                            info!("Client {} Yaesu Mem: {} -> {} (skip empty, {} filled)", addr, cur, next, filled.len());
                                            yaesu.send_command(crate::yaesu::YaesuCmd::RecallMemory(next));
                                        }
                                    }
                                }
                                ControlId::YaesuReadMenus => {
                                    if let Some(ref yaesu) = yaesu {
                                        info!("Client {} requested Yaesu menu read", addr);
                                        yaesu.send_command(crate::yaesu::YaesuCmd::ReadAllMenus);
                                    }
                                }
                                ControlId::YaesuSetMenu => {
                                    // value encodes menu number; P2 data arrives via YaesuMemoryData packet
                                    if let Some(ref yaesu) = yaesu {
                                        if let Some(text) = yaesu_write_pending.take() {
                                            // text format: "nnn:value"
                                            if let Some((num_str, val)) = text.split_once(':') {
                                                if let Ok(num) = num_str.parse::<u16>() {
                                                    info!("Client {} Yaesu set menu {:03} = {}", addr, num, val);
                                                    yaesu.send_command(crate::yaesu::YaesuCmd::SetMenu(num, val.to_string()));
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
                                        if let Some(text) = yaesu_write_pending.take() {
                                            info!("Client {} writing Yaesu memories", addr);
                                            yaesu.send_command(crate::yaesu::YaesuCmd::WriteAllMemories(text));
                                        } else {
                                            info!("Client {} write memories: data not yet received, arming latch", addr);
                                            yaesu_write_armed = true;
                                        }
                                    }
                                }
                                ControlId::SpectrumMaxBins => {
                                    self.session.lock().await.set_spectrum_max_bins(addr, ctrl.value);
                                    info!("Client {} spectrum max_bins: {}", addr, ctrl.value);
                                }
                                ControlId::Rx2SpectrumMaxBins => {
                                    self.session.lock().await.set_rx2_spectrum_max_bins(addr, ctrl.value);
                                    info!("Client {} RX2 spectrum max_bins: {}", addr, ctrl.value);
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
                                    info!("Client {} VRX1 enable: {}", addr, if on { "ON" } else { "OFF" });
                                }
                                ControlId::VrxMode => {
                                    let mode = match ctrl.value {
                                        0 => vrx_rs::VrxMode::Usb, 1 => vrx_rs::VrxMode::Lsb,
                                        2 => vrx_rs::VrxMode::Am, 3 => vrx_rs::VrxMode::Sam,
                                        4 => vrx_rs::VrxMode::Fm, _ => vrx_rs::VrxMode::Usb,
                                    };
                                    let ctl = self.vrx_mgr.lock().unwrap().control(addr, 0);
                                    ctl.lock().unwrap().mode = mode;
                                    info!("Client {} VRX1 mode: {:?}", addr, mode);
                                }
                                ControlId::VrxVolume => {
                                    let v = (ctrl.value as f32 / 100.0).clamp(0.0, 4.0);
                                    let ctl = self.vrx_mgr.lock().unwrap().control(addr, 0);
                                    ctl.lock().unwrap().volume = v;
                                    info!("Client {} VRX1 volume: {:.2}", addr, v);
                                }
                                ControlId::VrxEnable2 => {
                                    let on = ctrl.value != 0;
                                    let ctl = self.vrx_mgr.lock().unwrap().control(addr, 1);
                                    ctl.lock().unwrap().enabled = on;
                                    self.session.lock().await.set_vrx_audio(addr, 1, on);
                                    info!("Client {} VRX2 enable: {}", addr, if on { "ON" } else { "OFF" });
                                }
                                ControlId::VrxMode2 => {
                                    let mode = match ctrl.value {
                                        0 => vrx_rs::VrxMode::Usb, 1 => vrx_rs::VrxMode::Lsb,
                                        2 => vrx_rs::VrxMode::Am, 3 => vrx_rs::VrxMode::Sam,
                                        4 => vrx_rs::VrxMode::Fm, _ => vrx_rs::VrxMode::Usb,
                                    };
                                    let ctl = self.vrx_mgr.lock().unwrap().control(addr, 1);
                                    ctl.lock().unwrap().mode = mode;
                                    info!("Client {} VRX2 mode: {:?}", addr, mode);
                                }
                                ControlId::VrxVolume2 => {
                                    let v = (ctrl.value as f32 / 100.0).clamp(0.0, 4.0);
                                    let ctl = self.vrx_mgr.lock().unwrap().control(addr, 1);
                                    ctl.lock().unwrap().volume = v;
                                    info!("Client {} VRX2 volume: {:.2}", addr, v);
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
                                    info!("Client {} VRX1 filter: {} .. {} Hz", addr, lo, hi);
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
                                    info!("Client {} VRX2 filter: {} .. {} Hz", addr, lo, hi);
                                }
                                ControlId::VrxSpectrumEnable => {
                                    let on = ctrl.value != 0;
                                    {
                                        let mut m = self.vrx_mgr.lock().unwrap();
                                        let cur = m.spectrum_span(&addr, 0);
                                        m.set_spectrum_span(addr, 0, if on { if cur == 0 { 24 } else { cur } } else { 0 });
                                    }
                                    self.session.lock().await.set_vrx_spectrum(addr, 0, on);
                                    // VRX1 leest uit de RX1-processor: die moet aan, ook als
                                    // niemand het gewone RX1-spectrum wil.
                                    self.refresh_spectrum_processors().await;
                                    info!("Client {} VRX1 high-res spectrum: {}", addr, if on { "ON" } else { "OFF" });
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
                                    info!("Client {} VRX2 high-res spectrum: {}", addr, if on { "ON" } else { "OFF" });
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
                                // --- VRX audio rate (per-client per-VRX, Optie B) ---
                                ControlId::VrxAudioRate => {
                                    // VRX1 rate (0=NB,1=WB,2=Auto). Old clients send only this.
                                    self.vrx_mgr.lock().unwrap().set_rate_mode(addr, 0, (ctrl.value & 0xFF) as u8);
                                    info!("Client {} VRX1 audio-rate: {}", addr,
                                        match ctrl.value { 0 => "NB", 1 => "WB", _ => "Auto" });
                                }
                                ControlId::VrxAudioRate2 => {
                                    // VRX2 rate; new clients send this for independent per-VRX rate.
                                    self.vrx_mgr.lock().unwrap().set_rate_mode(addr, 1, (ctrl.value & 0xFF) as u8);
                                    info!("Client {} VRX2 audio-rate: {}", addr,
                                        match ctrl.value { 0 => "NB", 1 => "WB", _ => "Auto" });
                                }
                                ControlId::VrxSamAutoTune => {
                                    // Per-client subscription only - the audio loop
                                    // derives the per-VRX AFC enable as the aggregate
                                    // (any active subscriber) before each feed, so one
                                    // client never flips another client's AFC.
                                    let on = ctrl.value != 0;
                                    self.session.lock().await.set_vrx_autotune(addr, 0, on);
                                    info!("Client {} VRX1 SAM auto-tune: {}", addr, if on { "ON" } else { "OFF" });
                                }
                                ControlId::VrxSamAutoTune2 => {
                                    let on = ctrl.value != 0;
                                    self.session.lock().await.set_vrx_autotune(addr, 1, on);
                                    info!("Client {} VRX2 SAM auto-tune: {}", addr, if on { "ON" } else { "OFF" });
                                }
                                // --- Dual-radio slot 1 controls (Optie B-prime) ---
                                // Spiegel van de slot-0 Yaesu-controls, geroute naar yaesu2.
                                // Yaesu2Enable is de subscription-gate (zelfde patroon als YaesuEnable).
                                ControlId::Yaesu2Enable => {
                                    let enabled = ctrl.value != 0;
                                    self.session.lock().await.set_yaesu2_enabled(addr, enabled);
                                    info!("Client {} [radio1]: {}", addr, if enabled { "ON" } else { "OFF" });
                                }
                                ControlId::Yaesu2Ptt => {
                                    if let Some(ref yaesu) = yaesu2 {
                                        let on = ctrl.value != 0;
                                        yaesu.send_command(crate::yaesu::YaesuCmd::SetPtt(on));
                                        yaesu2_ptt_active = on;
                                        info!("Client {} [radio1] PTT: {}", addr, if on { "TX" } else { "RX" });
                                    }
                                }
                                ControlId::Yaesu2Freq => {} // handled via FrequencyYaesu2 packet
                                ControlId::Yaesu2MicGain => {
                                    let gain = ctrl.value as f32 / 100.0;
                                    yaesu2_mic_gain.store(gain.to_bits(), Ordering::Relaxed);
                                    info!("Client {} [radio1] legacy server gain: {:.2}x", addr, gain);
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
                                        let cat = match ctrl.value {
                                            // FTX-1 SC heeft een MAIN/SUB-prefix (P1) + richting (P2):
                                            // SC01=MAIN scan-up aan, SC00=MAIN scan uit. Zonder P1
                                            // ("SC1;") wordt het commando genegeerd. USB-audio = MAIN,
                                            // dus scannen op MAIN-side.
                                            0 => "AB;", 1 => "SC01;", 2 => "SC00;",
                                            // FTX-1 interne ATU: de radio rapporteert zijn tuner met
                                            // P1=1 (AC; antwoord 'AC100' bij off) - P1=0 (manual) wordt
                                            // genegeerd. We spiegelen de radio: P1=1, P2=0, P3=stand.
                                            // P3: 0=off, 1=on, 3=tuning start. Zie PATCH-yaesu-internal-atu sec.2.
                                            3 => "AC103;",   // Tune now (tuning start)
                                            4 => "AC100;",   // Tuner off (bypass)
                                            15 => "AC101;",  // Tuner on (in-line)
                                            5 => "BU0;", 6 => "BD0;", 7 => "ST1;", 8 => "ST0;",
                                            // Dual-RX MAIN/SUB actieve-TX/RX-keuze (FTX-1) - geverifieerd
                                            // commando-formaat uit cat-ftx1: FT0=MAIN, FT1=SUB.
                                            11 => "FT0;", 12 => "FT1;",
                                            // Quick Memory Bank (FTX-1): QI=store huidige VFO,
                                            // QR=recall/doorloop. Actie-commando's, geen antwoord.
                                            13 => "QI;", 14 => "QR;",
                                            _ => "",
                                        };
                                        if !cat.is_empty() {
                                            info!("Client {} [radio1] Yaesu button {}: {}", addr, ctrl.value, cat);
                                            yaesu.send_command(crate::yaesu::YaesuCmd::RawCat(cat.to_string()));
                                        }
                                        // Same empty-channel skip as slot 0, via the filled-channel
                                        // list from the last read (FTX-1 range 1..=99). Falls back to
                                        // +/-1 when no list is available.
                                        if ctrl.value == 9 || ctrl.value == 10 {
                                            let st = yaesu.status();
                                            let cur = st.memory_channel;
                                            let filled = st.filled_memory_channels;
                                            let next = next_memory_channel(&filled, cur, ctrl.value == 9, 99);
                                            info!("Client {} [radio1] Yaesu Mem: {} -> {} (skip empty, {} filled)", addr, cur, next, filled.len());
                                            yaesu.send_command(crate::yaesu::YaesuCmd::RecallMemory(next));
                                        }
                                    }
                                }
                                // Slot-1 geheugens (Fase B) - read/write naar radio 2.
                                ControlId::Yaesu2ReadMemories => {
                                    if let Some(ref yaesu) = yaesu2 {
                                        info!("Client {} [radio1] requested memory read", addr);
                                        yaesu.send_command(crate::yaesu::YaesuCmd::ReadAllMemories);
                                    }
                                }
                                ControlId::Yaesu2WriteMemories => {
                                    if let Some(ref yaesu) = yaesu2 {
                                        if let Some(text) = yaesu2_write_pending.take() {
                                            info!("Client {} [radio1] writing memories", addr);
                                            yaesu.send_command(crate::yaesu::YaesuCmd::WriteAllMemories(text));
                                        } else {
                                            info!("Client {} [radio1] write memories: data not yet received, arming latch", addr);
                                            yaesu2_write_armed = true;
                                        }
                                    }
                                }
                                // Slot-1 EX-menu (Fase C1): read = hierarchische scan
                                // op de FTX-1. De write komt binnen via YaesuMemoryData2
                                // met "SETMENU:"-prefix (zie handler hieronder).
                                ControlId::Yaesu2ReadMenus => {
                                    if let Some(ref yaesu) = yaesu2 {
                                        info!("Client {} [radio1] requested EX menu read", addr);
                                        yaesu.send_command(crate::yaesu::YaesuCmd::ReadAllMenus);
                                    }
                                }
                                ControlId::Yaesu2SetMenu => {}
                                _ => {
                                    // Unknown or unhandled control, ignore
                                    debug!("Unhandled control: {:?} = {}", ctrl.control_id, ctrl.value);
                                }
                            }
                        }
                    }
                }

                // Playout: pull exactly 1 frame per 20ms tick (smooth, regular cadence)
                // PTT state changes are driven by jitter buffer output, not packet arrival.
                _ = playout_tick.tick() => {
                    // Check if prefill delay elapsed -> send ZZTX1; now
                    self.ptt.lock().await.check_prefill().await;

                    match jitter_buf.pull() {
                        JitterResult::Frame(frame) => {
                            // PTT trigger from playout: frame.ptt drives prefill/release
                            if frame.ptt {
                                self.ptt.lock().await.activate_from_playout();
                            } else if self.ptt.lock().await.is_tx_or_prefill() {
                                // First non-PTT frame after TX/prefill: cancel prefill, start tail delay, release session
                                let depth = jitter_buf.depth();
                                let mut ptt = self.ptt.lock().await;
                                ptt.cancel_prefill();
                                ptt.release_from_playout(depth);
                                drop(ptt);
                                if let Some(addr) = tx_holder_addr.take() {
                                    self.session.lock().await.release_tx(addr);
                                }
                            }

                            if frame.opus_data.is_empty() {
                                continue; // PTT-only packet
                            }
                            match opus_decoder.decode(&frame.opus_data) {
                                Ok(pcm) => {
                                    let resampled = resample_to_device(
                                        &mut tx_resampler_out,
                                        &pcm,
                                    );
                                    self.ptt.lock().await.write_tx_audio(&resampled);
                                    self.audio_stats.tx.tick(self.server_start);
                                }
                                Err(e) => warn!("Opus decode error: {}", e),
                            }
                        }
                        JitterResult::Missing => {
                            match opus_decoder.decode_plc() {
                                Ok(pcm) => {
                                    let resampled = resample_to_device(
                                        &mut tx_resampler_out,
                                        &pcm,
                                    );
                                    self.ptt.lock().await.write_tx_audio(&resampled);
                                    self.audio_stats.tx.tick(self.server_start);
                                }
                                Err(e) => warn!("PLC error: {}", e),
                            }
                        }
                        JitterResult::NotReady => {}
                    }
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
        tci_iq_handle.abort();
        Ok(())
    }
}

// Audio loops, resampling helpers, and IQ consumer moved to audio_loops.rs

// Re-export for use within this file (Yaesu TX decode task uses resample_to_device)
use crate::audio_loops::resample_to_device;
