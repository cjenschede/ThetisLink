// SPDX-License-Identifier: GPL-2.0-or-later
//! Per-client VRX state manager.
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
    /// High-res spectrum width in Hz; 0 = spectrum off.
    pub spectrum_span_hz: u32,
    /// The width came in steps of 25 Hz (`VrxSpectrumSpanFine`): the whole-kHz
    /// one beside it is then ignored, so a late one cannot widen it again.
    pub fine_span: bool,
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
            spectrum_span_hz: 0,
            fine_span: false,
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

    /// The width in Hz; 0 turns the spectrum off and forgets the fine width.
    pub fn set_spectrum_span(&mut self, addr: SocketAddr, ch: u8, span_hz: u32) {
        let c = &mut self.entry(addr)[idx(ch)];
        c.spectrum_span_hz = span_hz;
        if span_hz == 0 {
            c.fine_span = false;
        }
    }

    /// A width the client asks for while its spectrum is on: `fine` from
    /// `VrxSpectrumSpanFine`, otherwise whole kHz from `VrxSpectrumSpanKhz`,
    /// which is ignored once a fine one came. With the spectrum off, nothing.
    pub fn request_span(&mut self, addr: SocketAddr, ch: u8, span_hz: u32, fine: bool) {
        if self.spectrum_span(&addr, ch) == 0 {
            return;
        }
        let c = &mut self.entry(addr)[idx(ch)];
        if fine || !c.fine_span {
            c.spectrum_span_hz = span_hz;
        }
        c.fine_span |= fine;
    }

    /// A VRX width control of either channel, whole kHz or fine: decoded and
    /// handed to [`Self::request_span`], in one place the receive loop only
    /// passes it on to. Gives the channel (0 or 1), the width in Hz and whether
    /// it was the fine one; `None` for any other control.
    pub fn span_control(
        &mut self,
        addr: SocketAddr,
        id: sdr_remote_core::protocol::ControlId,
        value: u16,
    ) -> Option<(u8, u32, bool)> {
        use sdr_remote_core::protocol::{unpack_vrx_span_fine, ControlId};
        let (ch, hz, fine) = match id {
            ControlId::VrxSpectrumSpanKhz => (0, value.max(1) as u32 * 1000, false),
            ControlId::VrxSpectrumSpanKhz2 => (1, value.max(1) as u32 * 1000, false),
            ControlId::VrxSpectrumSpanFine => (0, unpack_vrx_span_fine(value), true),
            ControlId::VrxSpectrumSpanFine2 => (1, unpack_vrx_span_fine(value), true),
            _ => return None,
        };
        self.request_span(addr, ch, hz, fine);
        Some((ch, hz, fine))
    }

    #[cfg(test)]
    pub fn fine_span(&self, addr: &SocketAddr, ch: u8) -> bool {
        self.clients.get(addr).map(|c| c[idx(ch)].fine_span).unwrap_or(false)
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

    pub fn spectrum_span(&self, addr: &SocketAddr, ch: u8) -> u32 {
        self.clients
            .get(addr)
            .map(|c| c[idx(ch)].spectrum_span_hz)
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
        // Isolation invariant (control side): writing client A's
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
        m.set_spectrum_span(a(1), 0, 24_000);
        assert_eq!(m.rate_mode(&a(1), 0), 0);
        assert_eq!(m.rate_mode(&a(1), 1), 1);
        assert_eq!(m.spectrum_span(&a(1), 0), 24_000);
        // The fine width (build 75): taken, and remembered until the spectrum
        // goes off, so a whole-kHz one cannot widen it again.
        m.request_span(a(1), 0, 1000, false);
        assert_eq!(m.spectrum_span(&a(1), 0), 1000, "an older client: whole kHz");
        m.request_span(a(1), 0, 375, true);
        m.request_span(a(1), 0, 1000, false);
        assert_eq!((m.spectrum_span(&a(1), 0), m.fine_span(&a(1), 0)), (375, true), "a late whole kHz is ignored");
        m.set_spectrum_span(a(1), 0, 0);
        assert!(!m.fine_span(&a(1), 0), "off forgets it");
        m.request_span(a(1), 0, 375, true);
        assert_eq!(m.spectrum_span(&a(1), 0), 0, "nothing while the spectrum is off");
        // Through the controls: each to its own channel, on its own scale.
        use sdr_remote_core::protocol::{pack_vrx_span_fine, ControlId as C};
        m.set_spectrum_span(a(2), 0, 24_000);
        m.set_spectrum_span(a(2), 1, 24_000);
        assert_eq!(m.span_control(a(2), C::VrxSpectrumSpanKhz2, 3), Some((1, 3000, false)));
        assert_eq!((m.spectrum_span(&a(2), 0), m.spectrum_span(&a(2), 1)), (24_000, 3000), "an older client, VRX2");
        assert_eq!(m.span_control(a(2), C::VrxSpectrumSpanFine, pack_vrx_span_fine(375)), Some((0, 375, true)));
        assert_eq!(m.span_control(a(2), C::VrxSpectrumSpanKhz, 1), Some((0, 1000, false)));
        assert_eq!(m.span_control(a(2), C::VrxSpectrumSpanFine2, pack_vrx_span_fine(1500)), Some((1, 1500, true)));
        assert_eq!((m.spectrum_span(&a(2), 0), m.spectrum_span(&a(2), 1)), (375, 1500));
        assert!(m.fine_span(&a(2), 0) && m.fine_span(&a(2), 1));
        assert_eq!(m.span_control(a(2), C::VrxSpectrumPan, 0), None, "not a width");
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
