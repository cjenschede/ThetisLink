// SPDX-License-Identifier: GPL-2.0-or-later

//! ThetisLink adapter around the generic `vrx-rs` crate.
//!
//! Maps the standalone runtime's outputs to ThetisLink-specific
//! transport (VrxAudioPacket via UDP) and provides a process-wide
//! singleton control-state per VRX channel.
//!
//! Operator of all ThetisLink-VRX coupling. If `vrx-rs` is ever
//! removed from this server, only this file + the dependency in
//! `Cargo.toml` need to go.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::tracked_socket::TrackedSocket;

use sdr_remote_core::protocol::{PacketType, VrxAudioPacket, VrxFrequencyPacket};
use sdr_remote_core::MAX_PACKET_SIZE;
use vrx_rs::VrxAudioCallback;

// NOTE: the former process-wide `vrx_control_thetislink()` singleton is removed -
// per-client VRX control-state now lives in `vrx_manager::PerClientVrxManager`
// (PATCH-vrx-per-client).

/// Per-stream feed-timing tracker (observability, PATCH-vrx-per-client §4bis).
/// Measures the fraction of the real-time batch budget that VRX `feed()` consumes
/// and warns as it approaches the deadline - the primary early-warning for stutter
/// now that there is no hard client cap. EMA + rate-limit keep the hot path
/// log-quiet in steady state (only value-crossings and a 30 s heartbeat emit).
///
/// Step 0 (baseline): instrumented on the current 2-runtime loop before the
/// per-client refactor, so we have a pre-refactor µs/batch reference and close the
/// observability gap noted in the audio-stutter diagnosis.
pub struct VrxFeedTimer {
    label: &'static str,
    ema_ratio: f32,             // ~1 s EMA of feed_time / real-time budget
    peak_ratio: f32,            // worst single-batch ratio since last heartbeat
    consecutive_miss: u32,      // run-length of batches over budget (resets on a good batch)
    last_warn: Option<Instant>, // rate-limit for the sustained-overload ERROR
    last_heartbeat: Instant,
    warned_50: bool,
    warned_80: bool,
}

impl VrxFeedTimer {
    pub fn new(label: &'static str) -> Self {
        Self {
            label,
            ema_ratio: 0.0,
            peak_ratio: 0.0,
            consecutive_miss: 0,
            last_warn: None,
            last_heartbeat: Instant::now(),
            warned_50: false,
            warned_80: false,
        }
    }

    /// Call right after `feed()`. `elapsed` = feed duration, `samples` = IQ pairs in
    /// this batch, `rate_hz` = IQ sample rate, `active` = active runtime count.
    pub fn record(&mut self, elapsed: Duration, samples: usize, rate_hz: u32, active: usize) {
        if rate_hz == 0 || samples == 0 {
            return;
        }
        let budget = samples as f32 / rate_hz as f32; // seconds of real-time this batch covers
        let ratio = elapsed.as_secs_f32() / budget;
        let alpha = budget.min(1.0); // time-based EMA (~1 s window)
        self.ema_ratio += alpha * (ratio - self.ema_ratio);
        if ratio > self.peak_ratio {
            self.peak_ratio = ratio;
        }
        if ratio > 1.0 {
            self.consecutive_miss += 1;
        } else {
            self.consecutive_miss = 0;
        }
        let now = Instant::now();

        // A single batch over budget is normal: IQ arrives bursty and the jitter
        // buffer absorbs the transient. Only alarm on *sustained* overload - the
        // running average can't keep up (EMA > budget) OR a solid run of misses
        // (~200 ms at the ~20 ms batch cadence). Lone spikes are DEBUG only.
        let sustained = self.ema_ratio > 1.0 || self.consecutive_miss >= 10;
        if sustained {
            if self.last_warn.map_or(true, |t| now.duration_since(t).as_secs() >= 5) {
                log::error!(
                    "VRX {}: sustained feed overload - EMA {:.0}% of budget, peak {:.0}%, {} consecutive miss(es), {} active runtime(s)",
                    self.label, self.ema_ratio * 100.0, self.peak_ratio * 100.0,
                    self.consecutive_miss, active
                );
                self.last_warn = Some(now);
            }
        } else if ratio > 1.0 {
            // Transient spike absorbed by the jitter buffer - visible in the
            // heartbeat's peak field, but no alarm.
            log::debug!(
                "VRX {}: transient feed spike {:.0}% (absorbed, EMA ~{:.0}%) - {} active runtime(s)",
                self.label, ratio * 100.0, self.ema_ratio * 100.0, active
            );
        } else if self.ema_ratio > 0.8 && !self.warned_80 {
            log::warn!(
                "VRX {}: feed ~{:.0}% of budget (EMA) - {} active runtime(s), stutter imminent",
                self.label, self.ema_ratio * 100.0, active
            );
            self.warned_80 = true;
        } else if self.ema_ratio > 0.5 && !self.warned_50 {
            log::warn!(
                "VRX {}: feed ~{:.0}% of budget (EMA) - {} active runtime(s)",
                self.label, self.ema_ratio * 100.0, active
            );
            self.warned_50 = true;
        }
        if self.ema_ratio < 0.4 {
            // Hysteresis re-arm so we warn again only after recovering.
            self.warned_50 = false;
            self.warned_80 = false;
        }

        if active > 0 && now.duration_since(self.last_heartbeat).as_secs() >= 30 {
            log::info!(
                "VRX {} heartbeat: feed ~{:.0}% of budget (EMA), peak {:.0}% this window, {} active runtime(s)",
                self.label, self.ema_ratio * 100.0, self.peak_ratio * 100.0, active
            );
            self.last_heartbeat = now;
            self.peak_ratio = self.ema_ratio; // start the next window from the running average
        }
    }
}

/// `VrxAudioCallback` implementation that builds a `VrxAudioPacket`
/// per Opus frame and sends it to every active client address
/// via `socket.try_send_to()`. `addrs` is a snapshot that the caller
/// refreshes before each `feed()` call under the session lock (sync
/// `try_send_to` needs no async runtime).
pub struct ThetisVrxSink {
    pub socket: Arc<TrackedSocket>,
    pub addrs: Vec<SocketAddr>,
    /// SAM auto-tune subscribers (per-client gated on `VrxSamAutoTune*`).
    /// Only these receive `FrequencyVrxActual` - old clients never do.
    pub autotune_addrs: Vec<SocketAddr>,
    pub timestamp_ms: u32,
    pub buf: Vec<u8>,
    /// Tags every emitted VrxAudioPacket as wideband (16 kHz Opus) vs.
    /// narrowband (8 kHz). Operator sets this before each feed() based on
    /// per-session toggle state.
    pub wideband: bool,
    /// Audio frames handed to the socket. Only used to spot the first frame of
    /// a freshly created runtime, so the wait after switching a channel on can
    /// be logged as a number.
    pub frames_sent: u64,
}

impl ThetisVrxSink {
    pub fn new(socket: Arc<TrackedSocket>) -> Self {
        Self {
            socket,
            addrs: Vec::new(),
            autotune_addrs: Vec::new(),
            timestamp_ms: 0,
            buf: Vec::with_capacity(MAX_PACKET_SIZE),
            wideband: false,
            frames_sent: 0,
        }
    }
}

impl VrxAudioCallback for ThetisVrxSink {
    fn on_frame(&mut self, vrx_id: u8, _audio: &[f32], opus_bytes: &[u8], sequence: u32) {
        if self.addrs.is_empty() || opus_bytes.is_empty() {
            return;
        }
        let packet = VrxAudioPacket {
            sequence,
            timestamp: self.timestamp_ms,
            vrx_id,
            opus_data: opus_bytes.to_vec(),
            wideband: self.wideband,
        };
        self.buf.clear();
        packet.serialize(&mut self.buf);
        for addr in &self.addrs {
            let _ = self.socket.try_send_to(&self.buf, *addr);
            self.frames_sent = self.frames_sent.wrapping_add(1);
        }
    }

    fn on_carrier_freq(&mut self, vrx_id: u8, freq_hz: u64) {
        if self.autotune_addrs.is_empty() {
            return;
        }
        let pkt = VrxFrequencyPacket { vrx_id, frequency_hz: freq_hz };
        let mut buf = [0u8; VrxFrequencyPacket::SIZE];
        pkt.serialize_with_type(PacketType::FrequencyVrxActual, &mut buf);
        for addr in &self.autotune_addrs {
            let _ = self.socket.try_send_to(&buf, *addr);
        }
    }
}
