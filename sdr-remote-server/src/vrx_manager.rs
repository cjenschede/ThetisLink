// SPDX-License-Identifier: GPL-2.0-or-later
//! Per-client VRX state manager (PATCH-vrx-per-client).
//!
//! Replaces the process-wide 2-channel singleton (`vrx_bridge::vrx_control_thetislink`)
//! with **independent VRX1/VRX2 per connected client**. This manager holds ONLY the
//! control-state (`Arc<Mutex<VrxControlState>>`) plus the ThetisLink-side per-channel
//! extras that are not part of the generic `vrx-rs` state (audio rate-mode and
//! high-res spectrum span).
//!
//! **Ownership split:** the channelizer *runtimes* are owned by the
//! audio-loop task, NOT here - so `feed()` (Opus encode, UDP send, spectrum extract)
//! never runs under this manager's lock. The network handler writes control through
//! short manager locks; each runtime holds its own clone of the inner
//! `Arc<Mutex<VrxControlState>>` and reads it during `feed()` lock-free w.r.t. the
//! manager.
//!
//! **Lifecycle:** control-state exists as soon as a client sends any VRX control and
//! survives `VrxEnable=0` (only the runtime is torn down); the whole entry is dropped
//! on disconnect/timeout (`remove_client`) or via the `retain_active` safety-net.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use vrx_rs::VrxControlState;

/// Audio rate-mode: Auto (resolve per filter width). NB/WB are represented by the
/// non-Auto path; only the Auto sentinel is referenced by name.
pub const RATE_AUTO: u8 = 2;

/// Per-(client, channel) VRX state.
pub struct ChState {
    /// Shared with the channelizer runtime (runtime reads, network writes).
    pub control: Arc<Mutex<VrxControlState>>,
    /// NB/WB/Auto (VrxAudioRate for ch0, VrxAudioRate2 for ch1).
    pub rate_mode: u8,
    /// High-res spectrum span in kHz; 0 = spectrum off.
    pub spectrum_span_khz: u16,
    /// Where this client is looking, relative to its listening frequency, in
    /// Hz. The window is cut here instead of always on the frequency itself -
    /// otherwise a client can only pan inside the one screen it was sent.
    pub spectrum_pan_hz: i32,
}

impl Default for ChState {
    fn default() -> Self {
        Self {
            control: Arc::new(Mutex::new(VrxControlState::default())),
            rate_mode: RATE_AUTO,
            spectrum_span_khz: 0,
            spectrum_pan_hz: 0,
        }
    }
}

/// Per-client VRX state (VRX1 + VRX2), keyed by the client's UDP `SocketAddr`.
#[derive(Default)]
pub struct PerClientVrxManager {
    clients: HashMap<SocketAddr, [ChState; 2]>,
}

#[inline]
fn idx(ch: u8) -> usize {
    (ch as usize).min(1)
}

impl PerClientVrxManager {
    pub fn new() -> Self {
        Self::default()
    }

    fn entry(&mut self, addr: SocketAddr) -> &mut [ChState; 2] {
        self.clients
            .entry(addr)
            .or_insert_with(|| [ChState::default(), ChState::default()])
    }

    /// Shared control-state Arc for (client, ch), lazily created. The audio loop
    /// clones this into the client's `VrxRuntime`; the network handler writes
    /// through it. Cloning the Arc is cheap and keeps writer/reader on the same
    /// inner mutex.
    pub fn control(&mut self, addr: SocketAddr, ch: u8) -> Arc<Mutex<VrxControlState>> {
        self.entry(addr)[idx(ch)].control.clone()
    }

    pub fn set_rate_mode(&mut self, addr: SocketAddr, ch: u8, mode: u8) {
        self.entry(addr)[idx(ch)].rate_mode = mode;
    }

    pub fn rate_mode(&self, addr: &SocketAddr, ch: u8) -> u8 {
        self.clients
            .get(addr)
            .map(|c| c[idx(ch)].rate_mode)
            .unwrap_or(RATE_AUTO)
    }

    pub fn set_spectrum_span(&mut self, addr: SocketAddr, ch: u8, span_khz: u16) {
        self.entry(addr)[idx(ch)].spectrum_span_khz = span_khz;
    }

    pub fn set_spectrum_pan(&mut self, addr: SocketAddr, ch: u8, pan_hz: i32) {
        self.entry(addr)[idx(ch)].spectrum_pan_hz = pan_hz;
    }

    pub fn spectrum_pan(&self, addr: &SocketAddr, ch: u8) -> i32 {
        self.clients
            .get(addr)
            .map(|c| c[idx(ch)].spectrum_pan_hz)
            .unwrap_or(0)
    }

    pub fn spectrum_span(&self, addr: &SocketAddr, ch: u8) -> u16 {
        self.clients
            .get(addr)
            .map(|c| c[idx(ch)].spectrum_span_khz)
            .unwrap_or(0)
    }

    /// Read-only listen frequency for (client, ch); 0 if unknown. Does NOT create
    /// an entry (safe to call from the spectrum tick for arbitrary addrs).
    pub fn target_freq(&self, addr: &SocketAddr, ch: u8) -> u64 {
        self.clients
            .get(addr)
            .and_then(|c| c[idx(ch)].control.lock().ok().map(|s| s.target_freq_hz))
            .unwrap_or(0)
    }

    /// Safety-net: drop entries for clients no longer active. Returns the number
    /// dropped (0 in steady state). Guards against a missed teardown path.
    /// Takes a slice (not a `HashSet`) so callers on the audio path don't have to
    /// allocate a set every batch - the active list is tiny (one entry per client).
    pub fn retain_active(&mut self, active: &[SocketAddr]) -> usize {
        let before = self.clients.len();
        self.clients.retain(|a, _| active.contains(a));
        before - self.clients.len()
    }

    pub fn client_count(&self) -> usize {
        self.clients.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(p: u16) -> SocketAddr {
        format!("127.0.0.1:{p}").parse().unwrap()
    }

    #[test]
    fn per_client_control_is_independent() {
        // Isolation invariant (§4bis test-hook, control side): writing client A's
        // VRX control must never touch client B's - they are distinct Arcs.
        let mut m = PerClientVrxManager::new();
        let ca = m.control(a(1000), 0);
        let cb = m.control(a(2000), 0);
        ca.lock().unwrap().target_freq_hz = 14_200_000;
        cb.lock().unwrap().target_freq_hz = 7_100_000;
        assert_eq!(ca.lock().unwrap().target_freq_hz, 14_200_000);
        assert_eq!(cb.lock().unwrap().target_freq_hz, 7_100_000);
        // VRX1 vs VRX2 within the same client are also distinct.
        let c1 = m.control(a(1000), 1);
        c1.lock().unwrap().target_freq_hz = 50_100_000;
        assert_eq!(m.control(a(1000), 0).lock().unwrap().target_freq_hz, 14_200_000);
    }

    #[test]
    fn rate_mode_and_span_per_client_channel() {
        let mut m = PerClientVrxManager::new();
        // Rate modes: 0 = NB, 1 = WB, RATE_AUTO = 2.
        m.set_rate_mode(a(1), 0, 0);
        m.set_rate_mode(a(1), 1, 1);
        m.set_spectrum_span(a(1), 0, 24);
        assert_eq!(m.rate_mode(&a(1), 0), 0);
        assert_eq!(m.rate_mode(&a(1), 1), 1);
        assert_eq!(m.spectrum_span(&a(1), 0), 24);
        // Unknown client -> defaults.
        assert_eq!(m.rate_mode(&a(9), 0), RATE_AUTO);
        assert_eq!(m.spectrum_span(&a(9), 0), 0);
    }

    #[test]
    fn retain_active_cleanup() {
        let mut m = PerClientVrxManager::new();
        m.control(a(1), 0);
        m.control(a(2), 0);
        m.control(a(3), 0);
        assert_eq!(m.client_count(), 3);
        // retain_active drops entries for clients no longer active.
        assert_eq!(m.retain_active(&[a(1), a(2)]), 1); // drops a(3)
        assert_eq!(m.client_count(), 2);
        assert_eq!(m.retain_active(&[a(1)]), 1); // drops a(2)
        assert_eq!(m.client_count(), 1);
    }
}
