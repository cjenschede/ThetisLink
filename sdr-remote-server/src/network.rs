// SPDX-License-Identifier: GPL-2.0-or-later

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::time::Instant;

use anyhow::{Context, Result};
use log::{debug, info, warn};
use tokio::net::UdpSocket;
use tokio::sync::{watch, Mutex};
use tokio::time::{interval, Duration};

use sdr_remote_core::jitter::{BufferedFrame, JitterBuffer, JitterResult};
use sdr_remote_core::protocol::*;
use sdr_remote_core::{FRAME_SAMPLES_WIDEBAND, FULL_SPECTRUM_BINS, MAX_PACKET_SIZE, NETWORK_SAMPLE_RATE_WIDEBAND};

use crate::amplitec::AmplitecSwitch;
use crate::config::ServerConfig;
use crate::ptt::RadioBackend;
use crate::ptt::PttController;
use crate::rf2k::Rf2k;
use crate::session::SessionManager;
use crate::spe_expert::SpeExpert;
use crate::spectrum::{Rx2SpectrumProcessor, SpectrumProcessor};
use crate::tuner::Jc4sTuner;
use crate::dxcluster::DxCluster;
use crate::rotor::Rotor;
use crate::ultrabeam::UltraBeam;

/// Server network service
pub struct NetworkService {
    socket: Arc<UdpSocket>,
    session: Arc<Mutex<SessionManager>>,
    ptt: Arc<Mutex<PttController>>,
    spectrum: Arc<Mutex<SpectrumProcessor>>,
    rx2_spectrum: Arc<Mutex<Rx2SpectrumProcessor>>,
    shutdown: watch::Receiver<bool>,
    amplitec: Option<Arc<AmplitecSwitch>>,
    tuner: Option<Arc<Jc4sTuner>>,
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
        tuner: Option<Arc<Jc4sTuner>>,
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
    ) -> Result<Self> {
        let socket = UdpSocket::bind(bind_addr)
            .await
            .context("bind UDP socket")?;
        // Enlarge UDP buffers: default Windows buffer (8KB) overflows with
        // spectrum packets (4-8KB each × clients), dropping audio packets.
        {
            use socket2::SockRef;
            let sock_ref = SockRef::from(&socket);
            let _ = sock_ref.set_send_buffer_size(2 * 1024 * 1024); // 2MB
            let _ = sock_ref.set_recv_buffer_size(512 * 1024);
            let send = sock_ref.send_buffer_size().unwrap_or(0);
            let recv = sock_ref.recv_buffer_size().unwrap_or(0);
            info!("UDP socket bound to {} (send_buf={}KB, recv_buf={}KB)", bind_addr, send/1024, recv/1024);
        }

        Ok(Self {
            socket: Arc::new(socket),
            session,
            ptt,
            spectrum,
            rx2_spectrum,
            shutdown,
            amplitec,
            tuner,
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
        })
    }

    pub async fn run(mut self) -> Result<()> {
        let start = Instant::now();
        let yaesu = self.yaesu.clone();

        // TCI is the only audio/IQ/control path
        let playback_rate = 48000u32;

        info!("TCI mode: audio at 48kHz via WebSocket");

        // TCI IQ consumer task: drain IQ channels → spectrum processor
        let tci_iq_handle = {
            let spectrum = self.spectrum.clone();
            let rx2_spectrum = self.rx2_spectrum.clone();
            let ptt = self.ptt.clone();
            let mut shutdown = self.shutdown.clone();
            tokio::spawn(async move {
                crate::audio_loops::tci_iq_consumer(ptt, spectrum, rx2_spectrum, &mut shutdown).await;
            })
        };

        // Extract TCI audio receivers from PttController
        let (tci_rx1_audio, tci_rx2_audio, tci_bin_r_audio) = {
            let mut ptt = self.ptt.lock().await;
            if let RadioBackend::Tci(ref mut tci) = ptt.radio {
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
            tokio::spawn(async move {
                if let Err(e) = tci_multichannel_audio_loop(
                    socket, session, ptt,
                    tci_rx1_audio, tci_rx2_audio, tci_bin_r_audio,
                    &mut shutdown, start,
                ).await { log::error!("Multi-channel audio bundler error: {}", e); }
            })
        };

        // Spawn Yaesu audio TX loop
        let _yaesu_audio_handle = {
            let yaesu_audio = yaesu.as_ref().and_then(|y| {
                let rx = y.audio_rx.lock().ok().and_then(|mut a| a.take())?;
                Some((rx, y.audio_sample_rate))
            });
            if let Some((audio_rx, sample_rate)) = yaesu_audio {
                let socket = self.socket.clone();
                let session = self.session.clone();
                let mut shutdown = self.shutdown.clone();
                Some(tokio::spawn(async move {
                    if let Err(e) = crate::audio_loops::yaesu_audio_loop(socket, session, audio_rx, sample_rate, &mut shutdown, start).await {
                        log::error!("Yaesu audio loop error: {}", e);
                    }
                }))
            } else {
                None
            }
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
                    loop {
                        tokio::select! {
                            _ = tick.tick() => {
                                let addrs = session.lock().await.yaesu_addrs();
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
                                    };
                                    let mut buf = [0u8; YaesuStatePacket::SIZE];
                                    pkt.serialize(&mut buf);
                                    for addr in &addrs {
                                        let _ = socket.try_send_to(&buf, *addr);
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

        // Spawn the safety check task
        let safety_handle = {
            let ptt = self.ptt.clone();
            let session = self.session.clone();
            let socket = self.socket.clone();
            let spectrum = self.spectrum.clone();
            let rx2_spectrum = self.rx2_spectrum.clone();
            let amplitec = self.amplitec.clone();
            let tuner = self.tuner.clone();
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
                let mut spectrum_frame_count: u32 = 0;
                let mut tuner_done_freq: u64 = 0; // VFO at tune complete
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
                let mut prev_equipment: std::collections::HashMap<u8, Vec<u8>> = std::collections::HashMap::new();
                let mut prev_eq_client_count: usize = 0;
                loop {
                    tokio::select! {
                        _ = safety_tick.tick() => {
                            ptt.lock().await.check_safety().await;
                        }
                        _ = spectrum_tick.tick() => {
                            // Collect client info + loss BEFORE locking spectrum
                            // (avoids deadlock: main loop does session→spectrum)
                            let client_info: Vec<(SocketAddr, f32, f32, u16, u8)> = {
                                let sess = session.lock().await;
                                sess.spectrum_clients().into_iter().map(|(addr, zoom, pan, max_bins)| {
                                    let loss = sess.client_loss(addr);
                                    (addr, zoom, pan, max_bins, loss)
                                }).collect()
                            };
                            if client_info.is_empty() {
                                continue;
                            }
                            // Extract all spectrum data under the lock, then release before sending
                            let mut packets_to_send: Vec<(SocketAddr, Vec<u8>)> = Vec::new();
                            {
                                let mut sp = spectrum.lock().await;
                                if !sp.is_frame_ready() {
                                    continue;
                                }
                                spectrum_frame_count = spectrum_frame_count.wrapping_add(1);
                                for (addr, zoom, pan, max_bins, loss) in &client_info {
                                    if *loss > 15 { continue; }
                                    if *loss > 5 && spectrum_frame_count % 2 != 0 { continue; }
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
                            } // spectrum lock released

                            // RX2 spectrum: also extract under lock, then release
                            {
                                let rx2_client_info: Vec<(SocketAddr, f32, f32, u16)> = {
                                    let sess = session.lock().await;
                                    sess.rx2_spectrum_clients()
                                };
                                if !rx2_client_info.is_empty() {
                                    let mut rx2_sp = rx2_spectrum.lock().await;
                                    if rx2_sp.is_frame_ready() {
                                        for (addr, zoom, pan, max_bins) in &rx2_client_info {
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
                                let labels = Some(crate::config::labels_string(&config));
                                let pkt = EquipmentStatusPacket {
                                    device_type: DeviceType::Amplitec6x2,
                                    switch_a: status.switch_a,
                                    switch_b: status.switch_b,
                                    connected: status.connected,
                                    labels,
                                };
                                send_if_changed!(DeviceType::Amplitec6x2, pkt, &eq_addrs);
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
                            // Tuner: track tune frequency, show stale if VFO moved >25kHz
                            if let Some(ref tuner_ref) = tuner {
                                let ts = tuner_ref.status();
                                let current_freq = last_vfo_freq;
                                let tuner_done = ts.state == crate::tuner::TUNER_DONE_OK
                                    || ts.state == crate::tuner::TUNER_DONE_ASSUMED;
                                if tuner_done {
                                    if tuner_done_freq == 0 {
                                        tuner_done_freq = current_freq;
                                    }
                                } else {
                                    tuner_done_freq = 0;
                                }
                                let is_stale = tuner_done
                                    && tuner_done_freq > 0 && current_freq > 0
                                    && (current_freq as i64 - tuner_done_freq as i64).unsigned_abs() > 25_000;
                                let broadcast_state = if is_stale { crate::tuner::TUNER_IDLE } else { ts.state };
                                tuner_ref.set_stale(is_stale);
                                let ts_for_broadcast = crate::tuner::TunerStatus {
                                    state: broadcast_state,
                                    ..ts
                                };
                                let can_tune = if let Some(ref amp) = amplitec {
                                    let amp_status = amp.status();
                                    let pos = amp_status.switch_a;
                                    pos > 0 && pos <= 6 && config.amplitec_labels[(pos - 1) as usize]
                                        .to_lowercase().contains("jc-4s")
                                } else {
                                    true
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
                            // DX Cluster spot broadcast
                            if let Some(ref cluster) = dxcluster {
                                let spots = cluster.spots_for_bands(last_vfo_freq, last_vfo_b_freq);
                                let expiry = cluster.expiry_secs() as u16;
                                let now = std::time::Instant::now();
                                for spot in &spots {
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
                                    for addr in &eq_addrs {
                                        let _ = socket.try_send_to(&buf, *addr);
                                    }
                                }
                                // Forward NEW spots to Thetis via TCI SPOT command (only once per spot)
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
                            info!("Tuner TCI: {} → {}", cmd.trim_end_matches(';'), tci_cmd.trim_end_matches(';'));
                            ptt.lock().await.send_cat(tci_cmd).await;
                        }
                        _ = cat_tick.tick() => {
                            // Two-phase connect: check needs (brief lock), connect (no lock), store (brief lock)
                            let ptt_clone = ptt.clone();
                            tokio::spawn(async move {
                                // Phase 1: check what connections are needed (brief, no I/O)
                                let (tci_url, cat_addr, aux_addr) = {
                                    let mut guard = ptt_clone.lock().await;
                                    let r = guard.needed_connections();
                                    r // guard dropped here
                                };
                                // Phase 2: attempt connections WITHOUT holding the ptt lock
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
                                let mut cat_stream = None;
                                if let Some(ref addr) = cat_addr {
                                    match tokio::time::timeout(
                                        Duration::from_millis(100),
                                        tokio::net::TcpStream::connect(addr),
                                    ).await {
                                        Ok(Ok(stream)) => { cat_stream = Some(stream); }
                                        Ok(Err(_)) => {}
                                        Err(_) => {}
                                    }
                                }
                                let mut aux_stream = None;
                                if let Some(ref addr) = aux_addr {
                                    match tokio::time::timeout(
                                        Duration::from_millis(100),
                                        tokio::net::TcpStream::connect(addr),
                                    ).await {
                                        Ok(Ok(stream)) => { aux_stream = Some(stream); }
                                        Ok(Err(_)) => {}
                                        Err(_) => {}
                                    }
                                }
                                // Phase 3: store established connections (brief lock, no I/O)
                                if tci_stream.is_some() || cat_stream.is_some() || aux_stream.is_some() {
                                    ptt_clone.lock().await.accept_connections(
                                        tci_stream, cat_stream, aux_stream,
                                    );
                                }
                            });
                            session.lock().await.check_timeout();
                        }
                        _ = freq_tick.tick() => {
                            let ptt_guard = ptt.lock().await;
                            let freq = ptt_guard.vfo_a_freq();
                            let mode = ptt_guard.vfo_a_mode();
                            let is_tx = ptt_guard.is_transmitting();
                            let smeter = if is_tx {
                                ptt_guard.fwd_power_raw()
                            } else {
                                ptt_guard.smeter_avg()
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
                            let thetis_starting = ptt_guard.thetis_starting();
                            let ctun = ptt_guard.ctun();
                            // RX2 state
                            let vfo_b_freq = ptt_guard.vfo_b_freq();
                            let vfo_b_mode = ptt_guard.vfo_b_mode();
                            let smeter_rx2 = ptt_guard.smeter_rx2_avg();
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
                            let has_att_ex = ptt_guard.has_tci_cap("step_attenuator_ex");
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

                            // VFO Sync: Thetis handles A↔B sync natively via ZZSY.
                            // No server-side sync needed — just relay Thetis frequency updates.

                            // Spectrum calibration
                            {
                                let mut sp = spectrum.lock().await;
                                if freq != 0 { sp.set_vfo_freq(freq, ctun); }
                                if dds_rx1 != 0 { sp.set_ddc_center(dds_rx1); }

                                if !is_tx {
                                    if has_att_ex {
                                        // Direct: static_cal + ATT (ATT is negative, negate for positive offset)
                                        sp.set_cal_offset_db(static_cal_rx1 + (-step_att_rx1 as f32));
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
                                if has_att_ex {
                                    rx2_sp.set_cal_offset_db(static_cal_rx2 + (-step_att_rx2 as f32));
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
                            let (smeter_addrs, rx2_addrs, all_addrs) = {
                                let sess = session.lock().await;
                                (sess.smeter_addrs(), sess.rx2_addrs(), sess.active_addrs())
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

                            // S-meter to Thetis clients (Yaesu-only clients don't need it)
                            if !smeter_addrs.is_empty() {
                                let flags = if is_tx || yaesu_ptt_flag.load(Ordering::Relaxed) { Flags::PTT } else { Flags::NONE };
                                let pkt = SmeterPacket { level: smeter, flags };
                                let mut buf = [0u8; SmeterPacket::SIZE];
                                pkt.serialize(&mut buf);
                                for addr in &smeter_addrs {
                                    let _ = socket.try_send_to(&buf, *addr);
                                }
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

                            // TX profile names — only on change
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

                            // RX2 broadcasts — only to clients that have RX2 enabled
                            if !rx2_addrs.is_empty() {
                                // Update RX2 spectrum processor with VFO-B freq + DDS in one lock
                                {
                                    let mut rx2_sp = rx2_spectrum.lock().await;
                                    if vfo_b_freq != 0 { rx2_sp.set_vfo_freq(vfo_b_freq, ctun); }
                                    if dds_rx2 != 0 { rx2_sp.set_ddc_center(dds_rx2); }
                                }

                                if vfo_b_freq != 0 && vfo_b_freq != prev_vfo_b_freq {
                                    prev_vfo_b_freq = vfo_b_freq;
                                    let pkt = FrequencyPacket { frequency_hz: vfo_b_freq };
                                    let mut buf = [0u8; FrequencyPacket::SIZE];
                                    pkt.serialize_as_type(&mut buf, PacketType::FrequencyRx2);
                                    for addr in &rx2_addrs {
                                        let _ = socket.try_send_to(&buf, *addr);
                                    }
                                }

                                if vfo_b_mode != prev_vfo_b_mode {
                                    prev_vfo_b_mode = vfo_b_mode;
                                    let pkt = ModePacket { mode: vfo_b_mode };
                                    let mut buf = [0u8; ModePacket::SIZE];
                                    pkt.serialize_as_type(&mut buf, PacketType::ModeRx2);
                                    for addr in &rx2_addrs {
                                        let _ = socket.try_send_to(&buf, *addr);
                                    }
                                }

                                // RX2 S-meter to RX2-enabled clients
                                {
                                    let pkt = SmeterPacket { level: smeter_rx2, flags: Flags::NONE };
                                    let mut buf = [0u8; SmeterPacket::SIZE];
                                    pkt.serialize_as_type(&mut buf, PacketType::SmeterRx2);
                                    for addr in &rx2_addrs {
                                        let _ = socket.try_send_to(&buf, *addr);
                                    }
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
                                    for addr in &rx2_addrs {
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

        // TX resampler: 16kHz wideband → TCI rate (for mic audio from client)
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
        .context("create 16k→device TX resampler")?;

        // Main RX loop
        let mut recv_buf = vec![0u8; MAX_PACKET_SIZE];
        let mut opus_decoder = sdr_remote_core::codec::OpusDecoderWideband::new()?;
        let mut jitter_buf = JitterBuffer::new(3, 20);
        let mut tx_holder_addr: Option<SocketAddr> = None;

        let mut shutdown = self.shutdown.clone();
        let mut playout_tick = interval(Duration::from_millis(20));
        let mut pending_filter_low: Option<i32> = None;
        let mut pending_rx2_filter_low: Option<i32> = None;

        // Yaesu TX audio: forward AudioYaesu packets to a separate decode task
        let mut yaesu_ptt_active = false;
        let mut yaesu_write_pending: Option<String> = None;
        let yaesu_mic_gain = Arc::new(AtomicU32::new(20.0_f32.to_bits())); // default 20.0x
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
                    info!("Yaesu TX task started (wideband Opus 16kHz → {}Hz)", tx_rate);
                    while let Some(opus_data) = pkt_rx.recv().await {
                        let gain = f32::from_bits(gain_shared.load(Ordering::Relaxed));
                        // Decode wideband Opus (16kHz)
                        let pcm_i16 = match decoder.decode(&opus_data) {
                            Ok(p) => p,
                            Err(e) => { log::warn!("Yaesu TX decode: {}", e); continue; }
                        };
                        // Convert to f32, apply gain
                        let pcm_f32: Vec<f32> = pcm_i16.iter()
                            .map(|&s| (s as f32 / 32768.0 * gain).clamp(-1.0, 1.0))
                            .collect();
                        // Resample 16kHz → tx_rate (48kHz)
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

        loop {
            tokio::select! {
                result = self.socket.recv_from(&mut recv_buf) => {
                    let (len, addr) = result.context("recv_from")?;
                    let data = &recv_buf[..len];

                    let packet = match Packet::deserialize(data) {
                        Ok(p) => p,
                        Err(e) => {
                            // Unknown packet types from old clients — silently ignore
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
                                // New unknown client or pending challenge — send challenge
                                // Don't overwrite PendingTotp state with a new challenge
                                if session.get_auth_state(addr).is_none()
                                    || matches!(session.get_auth_state(addr), Some(crate::session::AuthState::NoAuth)) {
                                    let nonce = session.create_challenge(addr);
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
                            // Thetis-only audio path — Yaesu TX handled via AudioYaesu packets
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
                                jitter_buf.push(
                                    BufferedFrame {
                                        sequence: audio_pkt.sequence,
                                        timestamp: audio_pkt.timestamp,
                                        opus_data: audio_pkt.opus_data,
                                        ptt: true,
                                    },
                                    arrival_ms,
                                );
                            } else if session.tx_holder() == Some(addr) {
                                // This client held TX — push non-PTT frame so tail
                                // audio plays out, then release will trigger from playout
                                drop(session);

                                self.ptt.lock().await.record_ptt_packet();

                                let arrival_ms = start.elapsed().as_millis() as u64;
                                jitter_buf.push(
                                    BufferedFrame {
                                        sequence: audio_pkt.sequence,
                                        timestamp: audio_pkt.timestamp,
                                        opus_data: audio_pkt.opus_data,
                                        ptt: false,
                                    },
                                    arrival_ms,
                                );
                            }
                            // else: non-PTT audio from non-TX-holder — ignore
                        }
                        Packet::Heartbeat(hb) => {
                            self.ptt.lock().await.heartbeat_received();
                            self.session.lock().await.update_heartbeat(
                                addr,
                                hb.sequence,
                                hb.rtt_ms,
                                hb.loss_percent,
                                hb.jitter_ms,
                            );

                            let ack = HeartbeatAck {
                                flags: Flags::NONE,
                                echo_sequence: hb.sequence,
                                echo_time: hb.local_time,
                                capabilities: Capabilities::NONE,
                            };
                            let mut ack_buf = [0u8; HeartbeatAck::SIZE];
                            ack.serialize(&mut ack_buf);
                            let _ = self.socket.send_to(&ack_buf, addr).await;
                        }
                        Packet::Frequency(freq_pkt) => {
                            self.ptt.lock().await.set_vfo_a_freq(freq_pkt.frequency_hz).await;
                        }
                        Packet::Mode(mode_pkt) => {
                            self.ptt.lock().await.set_vfo_a_mode(mode_pkt.mode).await;
                        }
                        Packet::Smeter(_) => {} // server-only, ignore from clients
                        Packet::Spectrum(_) | Packet::FullSpectrum(_) => {} // server-only, ignore from clients
                        Packet::EquipmentStatus(_) => {} // server-only, ignore from clients
                        Packet::EquipmentCommand(eq_cmd) => {
                            info!("Equipment command from {}: device={:?} cmd=0x{:02X} data={:?}",
                                addr, eq_cmd.device_type, eq_cmd.command_id, eq_cmd.data);
                            match eq_cmd.device_type {
                                DeviceType::Amplitec6x2 => {
                                    if let Some(ref amp) = self.amplitec {
                                        match eq_cmd.command_id {
                                            EquipmentCommandPacket::CMD_SET_SWITCH_A => {
                                                if let Some(&pos) = eq_cmd.data.first() {
                                                    info!("Amplitec: requesting Switch A → {}", pos);
                                                    amp.send_command(crate::amplitec::AmplitecCmd::SetSwitchA(pos));
                                                }
                                            }
                                            EquipmentCommandPacket::CMD_SET_SWITCH_B => {
                                                if let Some(&pos) = eq_cmd.data.first() {
                                                    info!("Amplitec: requesting Switch B → {}", pos);
                                                    amp.send_command(crate::amplitec::AmplitecCmd::SetSwitchB(pos));
                                                }
                                            }
                                            _ => {
                                                debug!("Unknown amplitec command: 0x{:02X}", eq_cmd.command_id);
                                            }
                                        }
                                    }
                                }
                                DeviceType::Tuner => {
                                    if let Some(ref tuner_ref) = self.tuner {
                                        match eq_cmd.command_id {
                                            CMD_TUNE_START => {
                                                info!("Tuner: tune requested by client");
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
                        // RX2 packets: client → server (frequency, mode set)
                        Packet::FrequencyRx2(freq_pkt) => {
                            let mut ptt = self.ptt.lock().await;
                            ptt.set_vfo_b_freq(freq_pkt.frequency_hz).await;
                        }
                        Packet::ModeRx2(mode_pkt) => {
                            let mut ptt = self.ptt.lock().await;
                            ptt.set_vfo_b_mode(mode_pkt.mode).await;
                        }
                        // RX2 packets that are server→client only (ignore if received)
                        Packet::AudioRx2(_) | Packet::AudioBinR(_) | Packet::SmeterRx2(_)
                        | Packet::SpectrumRx2(_) | Packet::FullSpectrumRx2(_) => {}
                        // Server→client only, ignore if received
                        Packet::Spot(_) | Packet::TxProfiles(_) | Packet::YaesuState(_)
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
                                yaesu_write_pending = Some(text);
                            }
                        }
                        // Yaesu TX audio: forward to separate decode task
                        Packet::AudioYaesu(pkt) => {
                            if yaesu_ptt_active && !pkt.opus_data.is_empty() {
                                if let Some(ref tx) = yaesu_tx_packet_tx {
                                    let _ = tx.try_send(pkt.opus_data);
                                }
                            }
                        }
                        Packet::FrequencyYaesu(freq_pkt) => {
                            if let Some(ref yaesu) = yaesu {
                                // Don't send FA in memory mode — it forces the radio to VFO mode
                                let status = yaesu.status();
                                if status.vfo_select != 1 { // 1=Memory
                                    yaesu.send_command(crate::yaesu::YaesuCmd::SetFreqA(freq_pkt.frequency_hz));
                                }
                            }
                        }
                        Packet::AudioMultiCh(_) => {} // server→client only, ignore
                        Packet::Disconnect => {
                            info!("Client {} disconnected", addr);
                            self.session.lock().await.remove(addr);
                        }
                        Packet::Control(ctrl) => {
                            let mut ptt = self.ptt.lock().await;
                            match ctrl.control_id {
                                ControlId::Rx1AfGain => {
                                    let val = ctrl.value.min(100);
                                    // rx_volume via TCI (v2.10.3.13+)
                                    let db = (val as i32) - 100; // 0-100 → -100..0 dB
                                    let cmd = format!("rx_volume:0,0,{};", db);
                                    ptt.send_cat(&cmd).await;
                                    ptt.set_rx_af_gain(val as u8);
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
                                    // Enable processor if any client wants spectrum
                                    let any_enabled = !self.session.lock().await.spectrum_addrs().is_empty();
                                    self.spectrum.lock().await.set_enabled(any_enabled);
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
                                    self.session.lock().await.set_spectrum_zoom(addr, zoom);
                                    info!("Client {} spectrum zoom: {:.1}x", addr, zoom);
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
                                ControlId::ThetisStarting => {} // server→client only
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
                                    // Enable RX2 processor if any client wants RX2 spectrum
                                    let any_rx2 = !self.session.lock().await.rx2_spectrum_clients().is_empty();
                                    self.rx2_spectrum.lock().await.set_enabled(any_rx2);
                                    info!("Client {} RX2 spectrum: {}", addr, if enabled { "ON" } else { "OFF" });
                                }
                                ControlId::Rx2SpectrumFps => {
                                    let fps = (ctrl.value as u8).clamp(5, 30);
                                    self.session.lock().await.set_rx2_spectrum_fps(addr, fps);
                                }
                                ControlId::Rx2SpectrumZoom => {
                                    let zoom = ctrl.value as f32 / 10.0;
                                    self.session.lock().await.set_rx2_spectrum_zoom(addr, zoom);
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
                                        info!("Starting Thetis smart null (coarse={}°@{}ms fine=±{}°@{}ms gain=±{}dB@{}ms)",
                                            params[0], params[1], params[2], params[4], params[5], params[7]);
                                        ptt.diversity_smartnull(&params).await;
                                    } else {
                                    // Fastsweep (F line) — requires Thetis cap
                                    let fastsweep = if has_fastsweep { crate::load_smart_fastsweep() } else { None };
                                    if let Some((start, end, step, settle, meter)) = fastsweep {
                                        let meter_name = if meter == 1 { "AVG" } else { "instant" };
                                        info!("Starting Thetis fastsweep: {:.0}° to {:.0}° step {:.2}° settle {}ms meter={}", start, end, step, settle, meter_name);
                                        ptt.diversity_fastsweep(start, end, step, settle, meter).await;
                                    } else {
                                        // Fallback: step-based autonull (P/G lines or default sweep)
                                        let mut steps = crate::load_smart_steps_server();
                                        let settle = crate::load_smart_settle_ms();
                                        if steps.is_empty() {
                                            // Generate default P/G steps: coarse 360° in 5°, fine ±15° in 1°, gain ±6dB in 0.5dB
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
                                ControlId::DdcSampleRateRx1 => {
                                    let rate = ctrl.value as u32 * 1000;
                                    info!("Client {} DDC RX1 rate: {}kHz", addr, ctrl.value);
                                    ptt.set_ddc_sample_rate(0, rate).await;
                                }
                                ControlId::DdcSampleRateRx2 => {
                                    let rate = ctrl.value as u32 * 1000;
                                    info!("Client {} DDC RX2 rate: {}kHz", addr, ctrl.value);
                                    ptt.set_ddc_sample_rate(1, rate).await;
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
                                        let mut ptt_guard = ptt.lock().await;
                                        let use_tci = ptt_guard.has_tci_cap("diversity_ex");

                                        if use_tci {
                                            // Read from TCI state (already populated by init queries)
                                            let tci = ptt_guard.tci_ref().unwrap();
                                            let values = [
                                                (ControlId::DiversityEnable, tci.diversity_enabled as u16),
                                                (ControlId::DiversityRef, tci.diversity_ref as u16),
                                                (ControlId::DiversitySource, tci.diversity_source as u16),
                                                (ControlId::DiversityGainRx1, tci.diversity_gain_rx1),
                                                (ControlId::DiversityGainRx2, tci.diversity_gain_rx2),
                                                (ControlId::DiversityPhase, (tci.diversity_phase + 18000).max(0) as u16),
                                            ];
                                            for (ctrl_id, value) in &values {
                                                let ctrl = ControlPacket { control_id: *ctrl_id, value: *value };
                                                let mut buf = [0u8; ControlPacket::SIZE];
                                                ctrl.serialize(&mut buf);
                                                let _ = socket.try_send_to(&buf, addr);
                                            }
                                            info!("Diversity read via TCI state");
                                        } else {
                                            // Fallback: query via auxiliary CAT
                                            let queries = [
                                                ("ZZDE;", ControlId::DiversityEnable),
                                                ("ZZDB;", ControlId::DiversityRef),
                                                ("ZZDH;", ControlId::DiversitySource),
                                                ("ZZDG;", ControlId::DiversityGainRx1),
                                                ("ZZDC;", ControlId::DiversityGainRx2),
                                                ("ZZDD;", ControlId::DiversityPhase),
                                            ];
                                            for (cmd, ctrl_id) in &queries {
                                                if let Some(val_str) = ptt_guard.query_cat(cmd).await {
                                                    let value: u16 = match ctrl_id {
                                                        ControlId::DiversityPhase => {
                                                            if let Ok(v) = val_str.parse::<i32>() {
                                                                (v + 18000).max(0) as u16
                                                            } else { 18000 }
                                                        }
                                                        _ => val_str.trim().parse().unwrap_or(0),
                                                    };
                                                    let ctrl = ControlPacket { control_id: *ctrl_id, value };
                                                    let mut buf = [0u8; ControlPacket::SIZE];
                                                    ctrl.serialize(&mut buf);
                                                    let _ = socket.try_send_to(&buf, addr);
                                                }
                                            }
                                            info!("Diversity read via CAT");
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
                                    info!("Client {} BIN: {}", addr, if ctrl.value != 0 { "ON" } else { "OFF" });
                                    ptt.set_binaural(ctrl.value != 0).await;
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
                                | ControlId::DiversityGainRx2 | ControlId::DiversityPhase => {
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
                                            _ => {}
                                        }
                                    });
                                }
                                // --- Yaesu controls ---
                                ControlId::YaesuEnable => {
                                    let enabled = ctrl.value != 0;
                                    self.session.lock().await.set_yaesu_enabled(addr, enabled);
                                    info!("Client {} Yaesu: {}", addr, if enabled { "ON" } else { "OFF" });
                                }
                                ControlId::YaesuPtt => {
                                    if let Some(ref yaesu) = yaesu {
                                        let on = ctrl.value != 0;
                                        let status = yaesu.status();
                                        let in_memory = status.vfo_select == 1; // 1=Memory mode
                                        // FM → DATA-FM transparent switch for USB mic TX
                                        // Skip mode switch in memory mode (would force radio to VFO mode)
                                        let needs_data_fm = !in_memory && status.mode_char == '4';
                                        if on && needs_data_fm {
                                            yaesu.send_command(crate::yaesu::YaesuCmd::RawCat("MD0A;".to_string()));
                                            info!("Yaesu: FM → DATA-FM for TX");
                                        }
                                        yaesu.send_command(crate::yaesu::YaesuCmd::SetPtt(on));
                                        yaesu_ptt_active = on;
                                        self.yaesu_ptt_flag.store(on, Ordering::Relaxed);
                                        if !on && needs_data_fm {
                                            yaesu.send_command(crate::yaesu::YaesuCmd::RawCat("MD04;".to_string()));
                                            info!("Yaesu: DATA-FM → FM after TX");
                                        }
                                        info!("Client {} Yaesu PTT: {}{}", addr,
                                            if on { "TX" } else { "RX" },
                                            if in_memory { " (memory mode)" } else { "" });
                                    }
                                }
                                ControlId::YaesuFreq => {} // handled via FrequencyYaesu packet
                                ControlId::YaesuMicGain => {
                                    // Client sends slider value * 100 (range 5-200).
                                    // Multiply with base gain 20x for USB mic level compensation.
                                    let gain = ctrl.value as f32 / 100.0 * 20.0;
                                    yaesu_mic_gain.store(gain.to_bits(), Ordering::Relaxed);
                                    info!("Client {} Yaesu mic gain: {:.1}x (slider {:.2})", addr, gain, ctrl.value as f32 / 100.0);
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
                                            3 => "AC002;",   // Tuner on
                                            4 => "AC000;",   // Tuner off
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
                                        // Memory channel up/down: use MC with current channel ±1
                                        if ctrl.value == 9 || ctrl.value == 10 {
                                            let status = yaesu.status();
                                            let cur = status.memory_channel;
                                            let next = if ctrl.value == 9 {
                                                if cur >= 99 { 1 } else { cur + 1 }
                                            } else {
                                                if cur <= 1 { 99 } else { cur - 1 }
                                            };
                                            info!("Client {} Yaesu Mem: {} -> {}", addr, cur, next);
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
                                    // Data comes via YaesuMemoryData packet (stored in pending_write)
                                    if let Some(ref yaesu) = yaesu {
                                        if let Some(text) = yaesu_write_pending.take() {
                                            info!("Client {} writing Yaesu memories", addr);
                                            yaesu.send_command(crate::yaesu::YaesuCmd::WriteAllMemories(text));
                                        } else {
                                            warn!("Client {} write memories: no data pending", addr);
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
                    // Check if prefill delay elapsed → send ZZTX1; now
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

