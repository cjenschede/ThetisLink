// SPDX-License-Identifier: GPL-2.0-or-later
//
// Eén zelfstandig kanaal-spectrum. Zie docs/internal/patch-briefs/
// REFACTOR-audio-spectrum-per-channel.md §6.1.
//
// De grens wordt door het typesysteem afgedwongen, niet door discipline: bins
// komen UITSLUITEND binnen via `ingest(SpectrumSnapshot)`, en de snapshot draagt
// een `ChannelId` die tegen `self.channel` wordt gematcht. `update_smeter` /
// `update_auto_ref` leiden af uit `self.bins` — nooit uit een ander kanaal. Een
// functie die op een `ChannelSpectrum` werkt kan per constructie geen RX-data
// lezen: die zit niet in scope.

use std::time::Instant;

use super::WaterfallRingBuffer;

/// Server-side vaste dB-schaal waarmee bins in u16 worden gepakt (matcht het
/// hoofdspectrum). dB = server_floor + (val/65535) * server_range.
const SERVER_FLOOR_DB: f64 = -150.0;
const SERVER_RANGE_DB: f64 = 120.0;

fn bin_db(val: u16) -> f64 {
    SERVER_FLOOR_DB + (val as f64 / 65535.0) * SERVER_RANGE_DB
}

/// Kanaal-identiteit; reist mee met elk spectrum-pakket en met de bundel.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) enum ChannelId {
    Vrx1,
    Vrx2,
}

impl ChannelId {
    fn short(self) -> &'static str {
        match self {
            ChannelId::Vrx1 => "vrx1",
            ChannelId::Vrx2 => "vrx2",
        }
    }
}

/// Getypeerde ingang. De ENIGE manier om bins in een `ChannelSpectrum` te
/// krijgen. `channel` bewijst uit welke stroom de bins komen — een verkeerde
/// bron faalt de assert (test) / wordt genegeerd (release).
pub(crate) struct SpectrumSnapshot {
    pub channel: ChannelId,
    pub bins: Vec<u16>,
    pub center_hz: u32,
    pub span_hz: u32,
    pub sequence: u16,
}

/// S-meter-toestand voor het no-data-contract (§6.7): geen NaN/sentinel-sprong
/// wanneer er (nog) geen data is.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) enum SmeterState {
    /// Kanaal uit, of nog geen bins ontvangen — s-meter niet geldig.
    NoData,
    /// Levende meting uit eigen bins.
    Active,
}

/// Auto-ref-toestand (§6.7).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) enum AutoRefState {
    /// Nog geen bins om een ref op te baseren.
    NoData,
    /// Ref regelt in (snelle EMA, eerste ~45 frames).
    Converging,
    /// Ingeregeld (trage EMA).
    Settled,
}

/// Alles voor ÉÉN kanaal-spectrum + s-meter. Een methode die hierop werkt kan
/// geen ander kanaal lezen — de RX-data zit niet in scope.
pub(crate) struct ChannelSpectrum {
    channel: ChannelId,
    // Ontvangen data (dit kanaal) — privaat, alleen via ingest() te vullen.
    bins: Vec<u16>,
    center_hz: u32,
    span_hz: u32,
    sequence: u16,
    waterfall: WaterfallRingBuffer,
    // S-meter (afgeleid uit self.bins).
    smeter_dbm: f32,
    smeter_peak: f32,
    smeter_peak_time: Instant,
    smeter_initialized: bool,
    smeter_state: SmeterState,
    // Auto-ref-afleiding (schrijft ref_db terug naar de aanroeper).
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

    /// Getypeerde ingang. Bewijst dat de bins bij DIT kanaal horen; een
    /// verkeerde bron faalt de assert (test) en wordt in release stil genegeerd
    /// i.p.v. verkeerd getoond.
    pub(crate) fn ingest(&mut self, snap: SpectrumSnapshot) {
        debug_assert_eq!(snap.channel, self.channel, "spectrum-bron != kanaal");
        if snap.channel != self.channel {
            return;
        }
        self.bins = snap.bins;
        self.center_hz = snap.center_hz;
        self.span_hz = snap.span_hz;
        self.sequence = snap.sequence;
        self.waterfall
            .push_full_only(&self.bins, self.center_hz, self.span_hz, self.sequence);
    }

    /// Kanaal uitgeschakeld / high-res uit: bins wissen zodat render de
    /// "(spectrum nog niet ontvangen)"-placeholder toont en de s-meter naar
    /// NoData valt.
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

    /// Auto-ref herstarten (bij mode/freq-wissel): volgende frame regelt vers in.
    pub(crate) fn reset_auto_ref(&mut self) {
        self.auto_ref_frames = 0;
        self.auto_ref_initialized = false;
        self.auto_ref_state = AutoRefState::NoData;
    }

    /// Per-VRX S-meter: integreer spectrum-vermogen over de SSB-passband met
    /// deelbin-weging aan de randen, EMA-ballistiek. Bron: `self.bins` (eigen
    /// kanaal), nooit een RX-veld. `cal_offset_db` matcht de hoofd-RX-meter.
    ///
    /// No-data-contract (§6.7): uitgeschakeld of lege bins → NoData zonder
    /// sprong/NaN; bij het eerste echte frame regelt de EMA soepel in.
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
                // Geen bruikbare bins: houd de laatste waarde vast (geen sprong),
                // markeer NoData.
                self.smeter_state = SmeterState::NoData;
                return;
            }
        };

        // EMA-ballistiek (per frame; spectrum ~15 FPS → Δt ≈ 66 ms):
        // snelle attack (~15 ms τ), trage decay (~400 ms τ).
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
            // Deelbin-weging: volledig=1.0, rand=fractie binnen de passband.
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

    /// Per-VRX auto-ref: EMA op de gemiddelde ruisvloer (SSB-passband ±3 kHz
    /// rond de VRX-freq overgeslagen). Bron: `self.bins` (eigen kanaal). Geeft de
    /// nieuwe `ref_db` terug (aanroeper schrijft die naar zijn `vrx*_ref_db`), of
    /// `None` als er (nog) geen bins zijn — dan geen sprong.
    pub(crate) fn update_auto_ref(
        &mut self,
        enabled: bool,
        vrx_freq_hz: u64,
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
        let pass_lo = vrx_freq_hz as f64 - 3_000.0;
        let pass_hi = vrx_freq_hz as f64 + 3_000.0;
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

    /// Observability (§6.7): compacte per-kanaal-toestand voor een logregel.
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
    // CI-grens (REFACTOR-audio-spectrum-per-channel §6.7): de kanaal-spectrum-logica
    // mag NOOIT de platte RX-velden van een ander kanaal lezen. Deze test faalt de
    // build zodra zo'n referentie in dit bestand insluipt — het typesysteem dwingt
    // de scope-grens af, deze test dwingt af dat de grens niet via de tekst omzeild
    // wordt. De needles zijn uit stukjes opgebouwd zodat de test zichzelf niet matcht.
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
