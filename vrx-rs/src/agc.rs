// SPDX-License-Identifier: GPL-2.0-or-later

//! The VRX's AGC: each channel regulates its own level, and behaves like the
//! receiver it listens on.
//!
//! The shape follows the operator's Thetis settings for that receiver: the AGC
//! mode sets how long the gain holds after a peak and how fast it comes back,
//! and AGC-T sets the most it may amplify. Below that point a weaker signal is
//! simply quieter, as on RX1; above it the level is held. Before this AGC the
//! VRX amplified up to 100 dB with one time constant, so it kept a signal at
//! its level some 20 dB further down than RX1 did, and sounded louder.

/// The AGC modes, as Thetis numbers them (`agc_mode` 0-5; 5 = custom, taken
/// as Med here).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VrxAgcMode {
    Off,
    Long,
    Slow,
    Med,
    Fast,
}

impl VrxAgcMode {
    /// How long the gain holds after the level drops, s.
    fn hang_s(self) -> f32 {
        match self {
            Self::Long => 2.0,
            Self::Slow => 1.0,
            Self::Off | Self::Med | Self::Fast => 0.0,
        }
    }

    /// The time constant of the recovery once it no longer holds, s.
    fn recovery_s(self) -> f32 {
        match self {
            Self::Long => 2.0,
            Self::Slow => 0.5,
            Self::Off | Self::Med => 0.25,
            Self::Fast => 0.05,
        }
    }
}

/// What the VRX's AGC follows: the mode and AGC-T of its receiver.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VrxAgcSettings {
    pub mode: VrxAgcMode,
    /// AGC-T in dB, as Thetis reports it (`agc_gain`).
    pub top_db: f32,
}

impl Default for VrxAgcSettings {
    /// Thetis's own start values: Med, AGC-T 80.
    fn default() -> Self {
        Self { mode: VrxAgcMode::Med, top_db: 80.0 }
    }
}

impl VrxAgcSettings {
    /// From the values Thetis reports over TCI (`agc_mode`, `agc_gain`).
    pub fn from_thetis(mode: u8, gain: u8) -> Self {
        let mode = match mode {
            0 => VrxAgcMode::Off,
            1 => VrxAgcMode::Long,
            2 => VrxAgcMode::Slow,
            4 => VrxAgcMode::Fast,
            _ => VrxAgcMode::Med,
        };
        Self { mode, top_db: gain as f32 }
    }
}

/// The level the AGC holds a signal at, as a peak. Measured 2026-11-04: at 0.5
/// a -73 dBm tone read 2 dB above RX1, the 991A and the FTX-1 on the level bar.
pub const AGC_TARGET: f32 = 0.40;

/// AGC-T is a number on Thetis's scale, not on the VRX's: the channelizer's
/// samples are scaled differently from WDSP's. This shifts it so the point
/// below which a weaker signal goes quieter lies where RX1's does. An estimate
/// from the measurement of 2026-11-04 (with 100 dB the VRX held a -73 dBm tone
/// for about 22 dB more than RX1, which went quieter at once); to be checked
/// against RX1 in the field.
pub const AGC_TOP_OFFSET_DB: f32 = -4.0;

/// How fast the gain comes down when the level rises, s.
const ATTACK_S: f32 = 0.002;

/// One channel's AGC.
#[derive(Debug, Clone)]
pub struct VrxAgc {
    rate_hz: f32,
    settings: VrxAgcSettings,
    level: f32,
    hold_left: u32,
    hold_samples: u32,
    attack: f32,
    recovery: f32,
    max_gain: f32,
}

impl VrxAgc {
    pub fn new(rate_hz: u32, settings: VrxAgcSettings) -> Self {
        let mut agc = Self {
            rate_hz: rate_hz.max(1) as f32,
            settings,
            level: 1e-5,
            hold_left: 0,
            hold_samples: 0,
            attack: 0.0,
            recovery: 0.0,
            max_gain: 1.0,
        };
        agc.apply(settings);
        agc
    }

    /// Follow new settings; nothing happens when they are the ones in use.
    pub fn set(&mut self, settings: VrxAgcSettings) {
        if settings != self.settings {
            self.apply(settings);
        }
    }

    pub fn settings(&self) -> VrxAgcSettings {
        self.settings
    }

    fn apply(&mut self, settings: VrxAgcSettings) {
        let per_sample = |tau_s: f32| 1.0 - (-1.0 / (self.rate_hz * tau_s)).exp();
        self.settings = settings;
        self.attack = per_sample(ATTACK_S);
        self.recovery = per_sample(settings.mode.recovery_s());
        self.hold_samples = (settings.mode.hang_s() * self.rate_hz) as u32;
        self.hold_left = self.hold_left.min(self.hold_samples);
        self.max_gain = 10f32.powf((settings.top_db + AGC_TOP_OFFSET_DB) / 20.0);
    }

    /// The gain for the next sample, and the sample with it.
    pub fn process(&mut self, sample: f32) -> f32 {
        if self.settings.mode == VrxAgcMode::Off {
            return sample * self.max_gain;
        }
        let a = sample.abs();
        if a >= self.level {
            self.level += self.attack * (a - self.level);
            self.hold_left = self.hold_samples;
        } else if self.hold_left > 0 {
            self.hold_left -= 1;
        } else {
            self.level += self.recovery * (a - self.level);
        }
        let gain = (AGC_TARGET / self.level.max(1e-12)).min(self.max_gain);
        sample * gain
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 8_000;

    /// The peak of the last second of `secs` seconds of a 1 kHz tone.
    fn peak_out(agc: &mut VrxAgc, amplitude: f32, secs: f32) -> f32 {
        let n = (RATE as f32 * secs) as usize;
        let mut peak = 0f32;
        for i in 0..n {
            let s = amplitude * (std::f32::consts::TAU * 1000.0 * i as f32 / RATE as f32).sin();
            let out = agc.process(s);
            if i + RATE as usize >= n {
                peak = peak.max(out.abs());
            }
        }
        peak
    }

    fn db(x: f32) -> f32 {
        20.0 * x.log10()
    }

    /// Above the point AGC-T sets the level is held; below it a signal 20 dB
    /// weaker comes out 20 dB weaker, as on RX1.
    #[test]
    fn the_level_is_held_above_the_top_and_follows_below_it() {
        let settings = VrxAgcSettings::default();
        let max_gain = 10f32.powf((settings.top_db + AGC_TOP_OFFSET_DB) / 20.0);
        let knee = AGC_TARGET / max_gain;
        let strong = peak_out(&mut VrxAgc::new(RATE, settings), knee * 100.0, 3.0);
        let stronger = peak_out(&mut VrxAgc::new(RATE, settings), knee * 1000.0, 3.0);
        assert!((db(stronger) - db(strong)).abs() < 1.0, "held: {} vs {}", db(strong), db(stronger));
        assert!((strong - AGC_TARGET).abs() < 0.05, "at the target: {strong}");
        let weak = peak_out(&mut VrxAgc::new(RATE, settings), knee / 10.0, 3.0);
        let weaker = peak_out(&mut VrxAgc::new(RATE, settings), knee / 100.0, 3.0);
        assert!((db(weak) - db(weaker) - 20.0).abs() < 0.5, "follows: {} vs {}", db(weak), db(weaker));
    }

    /// A higher AGC-T lets a weaker signal reach the target.
    #[test]
    fn agc_t_moves_the_point() {
        let low = VrxAgcSettings { mode: VrxAgcMode::Med, top_db: 60.0 };
        let high = VrxAgcSettings { mode: VrxAgcMode::Med, top_db: 100.0 };
        let input = AGC_TARGET / 10f32.powf((80.0 + AGC_TOP_OFFSET_DB) / 20.0);
        let a = peak_out(&mut VrxAgc::new(RATE, low), input, 3.0);
        let b = peak_out(&mut VrxAgc::new(RATE, high), input, 3.0);
        assert!(db(b) - db(a) > 15.0, "{} vs {}", db(a), db(b));
        assert!((b - AGC_TARGET).abs() < 0.05);
    }

    /// Off is no regulation: the output is the input times the most AGC-T
    /// allows, also for a strong signal a regulating AGC would bring down.
    #[test]
    fn off_is_a_fixed_gain() {
        let s = VrxAgcSettings { mode: VrxAgcMode::Off, top_db: 40.0 };
        let mut agc = VrxAgc::new(RATE, s);
        let g = 10f32.powf((40.0 + AGC_TOP_OFFSET_DB) / 20.0);
        let peak = peak_out(&mut agc, 0.1, 1.0);
        assert!((peak - 0.1 * g).abs() < 0.01 * g, "{peak} vs {}", 0.1 * g);
        assert!((agc.process(-0.002) + 0.002 * g).abs() < 1e-6);
    }

    /// The hold itself: after a loud tone stops, Slow keeps its gain for its
    /// second of hang, where Med, without one, has already come up.
    #[test]
    fn slow_holds_its_gain_and_med_does_not() {
        let gain_after = |mode: VrxAgcMode, quiet_s: f32| {
            let mut agc = VrxAgc::new(RATE, VrxAgcSettings { mode, top_db: 80.0 });
            peak_out(&mut agc, 0.1, 1.0);
            for _ in 0..(RATE as f32 * quiet_s) as usize {
                agc.process(0.0001);
            }
            agc.process(0.0001) / 0.0001
        };
        let during = AGC_TARGET / 0.1;
        let slow = gain_after(VrxAgcMode::Slow, 0.5);
        assert!((db(slow) - db(during)).abs() < 1.5, "held: {} vs {}", db(slow), db(during));
        let med = gain_after(VrxAgcMode::Med, 0.5);
        assert!(db(med) - db(during) > 6.0, "Med recovers: {} vs {}", db(med), db(during));
    }

    /// After a loud tone stops, Slow holds its gain down for a second; Fast
    /// lets it come back within a fraction of that.
    #[test]
    fn the_mode_sets_the_hold_and_the_recovery() {
        let gain_after = |mode: VrxAgcMode, quiet_s: f32| {
            let mut agc = VrxAgc::new(RATE, VrxAgcSettings { mode, top_db: 80.0 });
            peak_out(&mut agc, 0.1, 1.0);
            let quiet = (RATE as f32 * quiet_s) as usize;
            for _ in 0..quiet {
                agc.process(0.0001);
            }
            agc.process(0.0001) / 0.0001
        };
        let slow = gain_after(VrxAgcMode::Slow, 0.5);
        let fast = gain_after(VrxAgcMode::Fast, 0.5);
        assert!(db(fast) - db(slow) > 20.0, "slow {} fast {}", db(slow), db(fast));
        // Long still holds after a second and a half, Slow has started to recover.
        let long = gain_after(VrxAgcMode::Long, 1.5);
        let slow = gain_after(VrxAgcMode::Slow, 1.5);
        assert!(db(slow) - db(long) > 3.0, "long {} slow {}", db(long), db(slow));
    }

    /// The gain after a loud tone and `quiet_s` of near silence, against the
    /// gain during the tone, in dB.
    fn rise_db(agc: &mut VrxAgc, quiet_s: f32) -> f32 {
        peak_out(agc, 0.1, 1.0);
        for _ in 0..(RATE as f32 * quiet_s) as usize {
            agc.process(0.0001);
        }
        db(agc.process(0.0001) / 0.0001) - db(AGC_TARGET / 0.1)
    }

    /// Each mode recovers with its own time: one time constant after its hang
    /// the gain has come up by e (8.7 dB), half of one by the root of e.
    #[test]
    fn each_mode_recovers_in_its_own_time() {
        for (mode, hang, tau) in [
            (VrxAgcMode::Long, 2.0, 2.0),
            (VrxAgcMode::Slow, 1.0, 0.5),
            (VrxAgcMode::Med, 0.0, 0.25),
            (VrxAgcMode::Fast, 0.0, 0.05),
        ] {
            let s = VrxAgcSettings { mode, top_db: 80.0 };
            let half = rise_db(&mut VrxAgc::new(RATE, s), hang + tau / 2.0);
            let one = rise_db(&mut VrxAgc::new(RATE, s), hang + tau);
            assert!((half - 4.34).abs() < 1.0, "{mode:?} half: {half}");
            assert!((one - 8.69).abs() < 1.0, "{mode:?} one: {one}");
        }
    }

    /// A switch to a mode with a shorter hang ends the old hang there: from
    /// Long to Fast the gain comes back at Fast's pace, not after Long's 2 s.
    #[test]
    fn a_switch_shortens_the_hang() {
        let mut agc = VrxAgc::new(RATE, VrxAgcSettings { mode: VrxAgcMode::Long, top_db: 80.0 });
        let held = rise_db(&mut agc, 0.1);
        assert!(held.abs() < 1.0, "Long holds: {held}");
        agc.set(VrxAgcSettings { mode: VrxAgcMode::Fast, top_db: 80.0 });
        for _ in 0..(RATE as f32 * 0.2) as usize {
            agc.process(0.0001);
        }
        let up = db(agc.process(0.0001) / 0.0001) - db(AGC_TARGET / 0.1);
        assert!(up > 25.0, "Fast recovers: {up}");
    }

    /// New settings take hold on a running AGC: from Med to Off it gives the
    /// fixed gain of the new AGC-T at once; the same settings again change
    /// nothing.
    #[test]
    fn set_follows_new_settings() {
        let mut agc = VrxAgc::new(RATE, VrxAgcSettings::default());
        peak_out(&mut agc, 0.1, 1.0);
        let off = VrxAgcSettings { mode: VrxAgcMode::Off, top_db: 40.0 };
        agc.set(off);
        assert_eq!(agc.settings(), off);
        let g = 10f32.powf((40.0 + AGC_TOP_OFFSET_DB) / 20.0);
        assert!((agc.process(0.01) - 0.01 * g).abs() < 1e-5 * g);
        // AGC-T alone, the mode unchanged, takes hold too.
        let higher = VrxAgcSettings { mode: VrxAgcMode::Off, top_db: 60.0 };
        agc.set(higher);
        let g = 10f32.powf((60.0 + AGC_TOP_OFFSET_DB) / 20.0);
        assert!((agc.process(0.01) - 0.01 * g).abs() < 1e-5 * g);
        let before = agc.clone();
        agc.set(higher);
        assert_eq!(format!("{before:?}"), format!("{agc:?}"));
    }

    /// The channel runs its audio through this AGC, and the runtime hands it
    /// the settings of the receiver every batch, also right after a rebuild.
    #[test]
    fn the_channel_uses_this_agc_with_the_receivers_settings() {
        let ch = include_str!("channelizer.rs");
        let ch = &ch[..ch.find("#[cfg(test)]").unwrap()];
        assert_eq!(ch.matches("audio_out.push(self.agc.process(sample));").count(), 1);
        assert!(!ch.contains("agc_target") && !ch.contains("agc_max_gain"), "no AGC of its own left");
        let rt = include_str!("runtime.rs");
        let rt = &rt[..rt.find("#[cfg(test)]").unwrap_or(rt.len())];
        let set = rt.find("ch.set_agc(agc);").expect("handed on");
        assert!(set > rt.find("ch.set_filter(lo_bins, hi_bins);").unwrap(), "after a rebuild too");
        assert!(rt.contains("s.sam_auto_tune, s.agc)"), "read from the control state");
    }

    /// The numbers Thetis reports map to the modes; 5 (custom) is Med.
    #[test]
    fn thetis_values_map_to_the_modes() {
        let m = |v| VrxAgcSettings::from_thetis(v, 80).mode;
        assert_eq!([m(0), m(1), m(2), m(3), m(4), m(5)], [
            VrxAgcMode::Off, VrxAgcMode::Long, VrxAgcMode::Slow, VrxAgcMode::Med, VrxAgcMode::Fast, VrxAgcMode::Med,
        ]);
        assert_eq!(VrxAgcSettings::from_thetis(3, 95).top_db, 95.0);
    }
}
