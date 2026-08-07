// SPDX-License-Identifier: GPL-2.0-or-later
//
// One self-contained channel spectrum. See docs/internal/patch-briefs/
// REFACTOR-audio-spectrum-per-channel.md §6.1.
//
// The boundary is enforced by the type system, not by discipline: bins
// arrive EXCLUSIVELY via `ingest(SpectrumSnapshot)`, and the snapshot carries
// a `ChannelId` that is matched against `self.channel`. `update_smeter` /
// `update_auto_ref` derive from `self.bins` — never from another channel. A
// function that operates on a `ChannelSpectrum` cannot by construction read RX
// data: it is not in scope.

use std::time::Instant;

use super::WaterfallRingBuffer;

/// Convert a packed u16 bin to dB via the shared wire-contract unpack in core
/// (single source of truth; can never drift from the server pack scale).
fn bin_db(val: u16) -> f64 {
    sdr_remote_core::unpack_spectrum_db(val) as f64
}

/// Channel identity; travels with every spectrum packet and with the bundle.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) enum ChannelId {
    Rx1,
    Rx2,
    Vrx1,
    Vrx2,
}

impl ChannelId {
    fn short(self) -> &'static str {
        match self {
            ChannelId::Rx1 => "rx1",
            ChannelId::Rx2 => "rx2",
            ChannelId::Vrx1 => "vrx1",
            ChannelId::Vrx2 => "vrx2",
        }
    }
}

/// Typed input. The ONLY way to get bins into a `ChannelSpectrum`.
/// `channel` proves which stream the bins come from — a wrong
/// source fails the assert (test) / is ignored (release).
pub(crate) struct SpectrumSnapshot {
    pub channel: ChannelId,
    pub bins: Vec<u16>,
    pub center_hz: u32,
    pub span_hz: u32,
    pub sequence: u16,
}

/// S-meter state for the no-data contract (§6.7): no NaN/sentinel jump
/// when there is (still) no data.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) enum SmeterState {
    /// Channel off, or no bins received yet — s-meter not valid.
    NoData,
    /// Live measurement from own bins.
    Active,
}

/// Auto-ref state (§6.7).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) enum AutoRefState {
    /// No bins yet to base a ref on.
    NoData,
    /// Ref is settling in (fast EMA, first ~45 frames).
    Converging,
    /// Settled (slow EMA).
    Settled,
}

/// Everything for ONE channel spectrum + s-meter. A method that operates on
/// this cannot read another channel — the RX data is not in scope.
pub(crate) struct ChannelSpectrum {
    channel: ChannelId,
    // Received data (this channel) — private, fillable only via ingest().
    bins: Vec<u16>,
    center_hz: u32,
    span_hz: u32,
    sequence: u16,
    waterfall: WaterfallRingBuffer,
    // Full-band backdrop: the DDC row this channel lives in. NOT this channel's
    // own data - it is display background only, and is deliberately kept out of
    // `bins` so the s-meter and auto-ref keep measuring this channel alone.
    backdrop_bins: Vec<u16>,
    backdrop_center_hz: u32,
    backdrop_span_hz: u32,
    // S-meter (derived from self.bins).
    smeter_dbm: f32,
    smeter_peak: f32,
    smeter_peak_time: Instant,
    smeter_initialized: bool,
    smeter_state: SmeterState,
    // Auto-ref derivation (writes ref_db back to the caller).
    auto_ref_value: f32,
    auto_ref_frames: u32,
    auto_ref_initialized: bool,
    auto_ref_state: AutoRefState,
}

impl ChannelSpectrum {
    pub(crate) fn new(channel: ChannelId) -> Self {
        Self {
            channel,
            bins: Vec::new(),
            center_hz: 0,
            span_hz: 0,
            sequence: 0,
            waterfall: WaterfallRingBuffer::new(200),
            backdrop_bins: Vec::new(),
            backdrop_center_hz: 0,
            backdrop_span_hz: 0,
            smeter_dbm: -127.0,
            smeter_peak: -127.0,
            smeter_peak_time: Instant::now(),
            smeter_initialized: false,
            smeter_state: SmeterState::NoData,
            auto_ref_value: -20.0,
            auto_ref_frames: 0,
            auto_ref_initialized: false,
            auto_ref_state: AutoRefState::NoData,
        }
    }

    /// Typed input. Proves that the bins belong to THIS channel; a
    /// wrong source fails the assert (test) and is silently ignored in release
    /// instead of shown incorrectly.
    pub(crate) fn ingest(&mut self, snap: SpectrumSnapshot) {
        debug_assert_eq!(snap.channel, self.channel, "spectrum-bron != kanaal");
        if snap.channel != self.channel {
            return;
        }
        self.bins = snap.bins;
        self.center_hz = snap.center_hz;
        self.span_hz = snap.span_hz;
        self.sequence = snap.sequence;
        // Two layers when a backdrop is available: the wide DDC row underneath,
        // this channel's extracted view on top. That is what keeps the waterfall
        // history filled after tuning or zooming - old rows then still carry data
        // outside the window they were recorded in. Deduplication is on the view
        // sequence, because the view is the channel's own stream; the backdrop is
        // whatever came in alongside it.
        if self.backdrop_bins.is_empty() || self.backdrop_span_hz == 0 {
            self.waterfall
                .push_full_only(&self.bins, self.center_hz, self.span_hz, self.sequence);
        } else {
            self.waterfall.push(
                &self.backdrop_bins, self.backdrop_center_hz, self.backdrop_span_hz,
                self.sequence,
                &self.bins, self.center_hz, self.span_hz,
            );
        }
    }

    /// Hand this channel the full-band row of the DDC it lives in (VRX1 rides
    /// RX1's DDC, VRX2 rides RX2's). Purely a display backdrop for the waterfall:
    /// it never reaches `bins`, so the s-meter and auto-ref cannot see it.
    ///
    /// The same row already goes to the RX window, so on a link where the RX
    /// spectrum is running this costs nothing extra - it is one row per DDC per
    /// client, not one per window.
    pub(crate) fn set_backdrop(&mut self, bins: &[u16], center_hz: u32, span_hz: u32) {
        if self.backdrop_bins.len() == bins.len() {
            self.backdrop_bins.copy_from_slice(bins);
        } else {
            self.backdrop_bins = bins.to_vec();
        }
        self.backdrop_center_hz = center_hz;
        self.backdrop_span_hz = span_hz;
    }

    /// Drop the backdrop (option switched off, or the source RX stopped).
    pub(crate) fn clear_backdrop(&mut self) {
        self.backdrop_bins.clear();
        self.backdrop_span_hz = 0;
    }

    /// Channel disabled / high-res off: clear bins so render shows the
    /// "(spectrum nog niet ontvangen)" placeholder and the s-meter falls
    /// to NoData.
    pub(crate) fn clear(&mut self) {
        self.bins.clear();
    }

    pub(crate) fn sequence(&self) -> u16 {
        self.sequence
    }
    pub(crate) fn bins(&self) -> &[u16] {
        &self.bins
    }
    pub(crate) fn center_hz(&self) -> u32 {
        self.center_hz
    }
    pub(crate) fn span_hz(&self) -> u32 {
        self.span_hz
    }
    pub(crate) fn waterfall(&self) -> &WaterfallRingBuffer {
        &self.waterfall
    }
    pub(crate) fn smeter_dbm(&self) -> f32 {
        self.smeter_dbm
    }
    pub(crate) fn smeter_peak(&self) -> f32 {
        self.smeter_peak
    }
    #[allow(dead_code)]
    pub(crate) fn smeter_state(&self) -> SmeterState {
        self.smeter_state
    }
    #[allow(dead_code)]
    pub(crate) fn auto_ref_state(&self) -> AutoRefState {
        self.auto_ref_state
    }

    /// Restart auto-ref (on mode/freq change): next frame settles in fresh.
    pub(crate) fn reset_auto_ref(&mut self) {
        self.auto_ref_frames = 0;
        self.auto_ref_initialized = false;
        self.auto_ref_state = AutoRefState::NoData;
    }

    /// Per-VRX S-meter: integrate spectrum power over the SSB passband with
    /// partial-bin weighting at the edges, EMA ballistics. Source: `self.bins`
    /// (own channel), never an RX field. `cal_offset_db` matches the main RX meter.
    ///
    /// No-data contract (§6.7): disabled or empty bins → NoData without
    /// jump/NaN; at the first real frame the EMA settles in smoothly.
    pub(crate) fn update_smeter(
        &mut self,
        enabled: bool,
        vrx_freq_hz: u64,
        filt_low_hz: i32,
        filt_high_hz: i32,
        cal_offset_db: f32,
    ) {
        if !enabled {
            self.smeter_dbm = -127.0;
            self.smeter_peak = -127.0;
            self.smeter_initialized = false;
            self.smeter_state = SmeterState::NoData;
            return;
        }
        let dbm = match Self::compute_smeter(
            &self.bins,
            self.center_hz,
            self.span_hz,
            vrx_freq_hz,
            filt_low_hz,
            filt_high_hz,
            cal_offset_db,
        ) {
            Some(v) => v,
            None => {
                // No usable bins: hold the last value (no jump),
                // mark NoData.
                self.smeter_state = SmeterState::NoData;
                return;
            }
        };

        // EMA ballistics (per frame; spectrum ~15 FPS → Δt ≈ 66 ms):
        // fast attack (~15 ms τ), slow decay (~400 ms τ).
        let alpha_attack = 1.0_f32 - (-0.066_f32 / 0.015).exp();
        let alpha_decay = 1.0_f32 - (-0.066_f32 / 0.400).exp();
        self.smeter_dbm = if !self.smeter_initialized {
            dbm
        } else {
            let alpha = if dbm > self.smeter_dbm { alpha_attack } else { alpha_decay };
            alpha * dbm + (1.0 - alpha) * self.smeter_dbm
        };
        self.smeter_initialized = true;
        self.smeter_state = SmeterState::Active;

        if self.smeter_dbm >= self.smeter_peak {
            self.smeter_peak = self.smeter_dbm;
            self.smeter_peak_time = Instant::now();
        } else if self.smeter_peak_time.elapsed().as_secs_f32() > 2.0 {
            self.smeter_peak = self.smeter_dbm;
            self.smeter_peak_time = Instant::now();
        }
    }

    fn compute_smeter(
        bins: &[u16],
        center_hz: u32,
        span_hz: u32,
        vrx_freq_hz: u64,
        filt_low_hz: i32,
        filt_high_hz: i32,
        cal_offset_db: f32,
    ) -> Option<f32> {
        if bins.is_empty() || span_hz == 0 {
            return None;
        }
        let hz_per_bin = span_hz as f64 / bins.len() as f64;
        let start_hz = center_hz as f64 - span_hz as f64 / 2.0;
        let lo_hz = vrx_freq_hz as f64 + filt_low_hz as f64;
        let hi_hz = vrx_freq_hz as f64 + filt_high_hz as f64;
        if hi_hz <= lo_hz {
            return None;
        }
        let lo_bin_f = (lo_hz - start_hz) / hz_per_bin;
        let hi_bin_f = (hi_hz - start_hz) / hz_per_bin;
        let lo_bin = lo_bin_f.floor() as i32;
        let hi_bin = hi_bin_f.ceil() as i32;
        if hi_bin <= 0 || lo_bin >= bins.len() as i32 {
            return None;
        }
        let mut power_mw = 0.0_f64;
        for i in lo_bin.max(0)..hi_bin.min(bins.len() as i32) {
            // Partial-bin weighting: full=1.0, edge=fraction within the passband.
            let bin_lo = i as f64;
            let bin_hi = (i + 1) as f64;
            let overlap_lo = bin_lo.max(lo_bin_f);
            let overlap_hi = bin_hi.min(hi_bin_f);
            let frac = (overlap_hi - overlap_lo).max(0.0).min(1.0);
            if frac <= 0.0 {
                continue;
            }
            let db = bin_db(bins[i as usize]);
            power_mw += frac * 10.0_f64.powf(db / 10.0);
        }
        if power_mw > 0.0 {
            Some(10.0 * power_mw.log10() as f32 + cal_offset_db)
        } else {
            None
        }
    }

    /// Per-VRX auto-ref: EMA on the average noise floor (SSB passband ±3 kHz
    /// around the VRX freq skipped). Source: `self.bins` (own channel). Returns
    /// the new `ref_db` (the caller writes it to its `vrx*_ref_db`), or
    /// `None` if there are (still) no bins — then no jump.
    /// Auto-ref from the noise floor, excluding the passband being listened to.
    ///
    /// `pass_lo_hz`/`pass_hi_hz` are offsets from `listen_freq_hz`, so a caller
    /// passes what it actually excludes: a VRX its fixed +/-3 kHz window, an RX
    /// its own filter edges. Before this was shared, RX1, RX2 and both VRX
    /// channels each carried their own copy of this loop - the same arithmetic
    /// four times, differing only in which band it skipped.
    pub(crate) fn update_auto_ref(
        &mut self,
        enabled: bool,
        listen_freq_hz: u64,
        pass_lo_hz: i32,
        pass_hi_hz: i32,
        range_db: f32,
    ) -> Option<f32> {
        if !enabled {
            return None;
        }
        if self.bins.is_empty() || self.span_hz == 0 {
            self.auto_ref_state = AutoRefState::NoData;
            return None;
        }
        let nbins = self.bins.len();
        let hz_per_bin = self.span_hz as f64 / nbins as f64;
        let start_hz = self.center_hz as f64 - self.span_hz as f64 / 2.0;
        let pass_lo = listen_freq_hz as f64 + pass_lo_hz as f64;
        let pass_hi = listen_freq_hz as f64 + pass_hi_hz as f64;
        let lo_bin = ((pass_lo - start_hz) / hz_per_bin) as i32;
        let hi_bin = ((pass_hi - start_hz) / hz_per_bin) as i32;
        let mut sum_db = 0.0f64;
        let mut count = 0u32;
        for (i, &val) in self.bins.iter().enumerate() {
            let idx = i as i32;
            if idx >= lo_bin && idx <= hi_bin {
                continue;
            }
            sum_db += bin_db(val);
            count += 1;
        }
        if count == 0 {
            self.auto_ref_state = AutoRefState::NoData;
            return None;
        }
        let avg_db = sum_db / count as f64;
        let target = avg_db as f32 + range_db - 2.0;
        if !self.auto_ref_initialized {
            self.auto_ref_value = target;
            self.auto_ref_initialized = true;
        } else {
            let alpha = if self.auto_ref_frames < 45 { 0.10 } else { 0.002 };
            self.auto_ref_value = alpha * target + (1.0 - alpha) * self.auto_ref_value;
        }
        self.auto_ref_frames += 1;
        self.auto_ref_state = if self.auto_ref_frames < 45 {
            AutoRefState::Converging
        } else {
            AutoRefState::Settled
        };
        Some(self.auto_ref_value)
    }

    /// Observability (§6.7): compact per-channel state for a log line.
    pub(crate) fn debug_line(&self, audio_on: bool, spectrum_on: bool) -> String {
        format!(
            "ch {}: audio={} spectrum={} bins={} seq={} center={} span={} smeter={:.0}dBm/{:?}",
            self.channel.short(),
            if audio_on { "on" } else { "off" },
            if spectrum_on { "on" } else { "off" },
            self.bins.len(),
            self.sequence,
            self.center_hz,
            self.span_hz,
            self.smeter_dbm,
            self.smeter_state,
        )
    }
}

#[cfg(test)]
mod boundary_guard {
    use super::{ChannelId, ChannelSpectrum, SpectrumSnapshot};

    // CI boundary (REFACTOR-audio-spectrum-per-channel §6.7): the channel-spectrum logic
    // may NEVER read the flat RX fields of another channel. This test fails the
    // build as soon as such a reference creeps into this file — the type system enforces
    /// Build a channel with a flat noise floor plus one loud bin inside the
    /// passband, so auto-ref has something to exclude.
    fn seeded(channel: ChannelId, center_hz: u32, span_hz: u32) -> ChannelSpectrum {
        let mut cs = ChannelSpectrum::new(channel);
        // A sloping floor, not a flat one: excluding a wider band only changes the
        // average if the excluded part differs from the rest.
        let mut bins: Vec<u16> = (0..512u16).map(|i| 10_000 + i * 40).collect();
        bins[256] = 60_000; // strong signal at centre - must be excluded
        cs.ingest(SpectrumSnapshot {
            channel,
            bins,
            center_hz,
            span_hz,
            sequence: 1,
        });
        cs
    }

    #[test]
    fn every_channel_derives_auto_ref_the_same_way() {
        // Lane 3 phase 1: RX1, RX2, VRX1 and VRX2 each carried their own copy of
        // this loop - the same arithmetic four times, differing only in which
        // band it skipped. Identical input on any two channels must therefore
        // give an identical result; if it does not, the copies drifted again.
        let center = 14_200_000u32;
        let span = 192_000u32;
        let mut a = seeded(ChannelId::Rx1, center, span);
        let mut b = seeded(ChannelId::Vrx1, center, span);
        let ra = a.update_auto_ref(true, center as u64, -3_000, 3_000, 60.0);
        let rb = b.update_auto_ref(true, center as u64, -3_000, 3_000, 60.0);
        assert_eq!(ra, rb, "the same input must derive the same ref on any channel");
        assert!(ra.is_some(), "a seeded channel must produce a ref");
    }

    #[test]
    fn the_excluded_passband_comes_from_the_caller() {
        // RX excludes its filter edges, VRX a fixed +/-3 kHz. That difference is
        // real and must stay expressible - it is why the band is a parameter
        // rather than a constant inside the shared code.
        let center = 14_200_000u32;
        let span = 192_000u32;
        let mut narrow = seeded(ChannelId::Rx1, center, span);
        let mut wide = seeded(ChannelId::Rx1, center, span);
        let n = narrow.update_auto_ref(true, center as u64, -200, 200, 60.0).unwrap();
        let w = wide.update_auto_ref(true, center as u64, -90_000, 90_000, 60.0).unwrap();
        assert!(
            n != w,
            "excluding a wider band must change the derived ref ({n} vs {w});              if these are equal the passband parameter is being ignored"
        );
    }

    // the scope boundary, this test enforces that the boundary is not bypassed via the text.
    // The needles are assembled from pieces so the test does not match itself.
    /// The backdrop is another channel's data on purpose - the DDC row this
    /// channel lives in. It may reach the waterfall and nothing else: the moment
    /// it leaks into `bins`, the s-meter would read the whole band instead of
    /// this channel, which is exactly the boundary this module exists to hold.
    #[test]
    fn the_backdrop_never_reaches_the_channel_measurements() {
        let center = 14_200_000u32;
        let span = 12_000u32;
        let mut with_backdrop = seeded(ChannelId::Vrx1, center, span);
        let mut without = seeded(ChannelId::Vrx1, center, span);
        // A backdrop far louder than anything in the channel itself.
        let loud: Vec<u16> = vec![65_535; 1024];
        with_backdrop.set_backdrop(&loud, center, 192_000);
        with_backdrop.ingest(SpectrumSnapshot {
            channel: ChannelId::Vrx1,
            bins: (0..512u16).map(|i| 10_000 + i * 40).collect(),
            center_hz: center,
            span_hz: span,
            sequence: 2,
        });
        without.ingest(SpectrumSnapshot {
            channel: ChannelId::Vrx1,
            bins: (0..512u16).map(|i| 10_000 + i * 40).collect(),
            center_hz: center,
            span_hz: span,
            sequence: 2,
        });
        with_backdrop.update_smeter(true, center as u64, -3_000, 3_000, 0.0);
        without.update_smeter(true, center as u64, -3_000, 3_000, 0.0);
        assert_eq!(
            with_backdrop.smeter_dbm(), without.smeter_dbm(),
            "a backdrop must not move the s-meter"
        );
        assert_eq!(
            with_backdrop.update_auto_ref(true, center as u64, -3_000, 3_000, 60.0),
            without.update_auto_ref(true, center as u64, -3_000, 3_000, 60.0),
            "a backdrop must not move the auto-ref"
        );
        assert_eq!(
            with_backdrop.bins(), without.bins(),
            "a backdrop must never end up in the channel's own bins"
        );
    }

    #[test]
    fn geen_cross_kanaal_rx_veld_referenties() {
        let src = include_str!("channel_spectrum.rs");
        let forbidden = [
            concat!("full", "_spectrum"),
            concat!("rx2_full", "_spectrum"),
            concat!("spectrum", "_bins"),
        ];
        for needle in forbidden {
            assert!(
                !src.contains(needle),
                "channel_spectrum.rs bevat een verboden cross-kanaal RX-referentie: '{}'. \
                 Kanaal-spectrum-logica moet UITSLUITEND uit self.bins afleiden (§6.1/§6.7).",
                needle
            );
        }
    }
}
