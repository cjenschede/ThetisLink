// SPDX-License-Identifier: GPL-2.0-or-later

//! The roger beep: a short tone at the end of a transmission, sent while the
//! transmitter is still keyed.
//!
//! That timing is the whole trick. A beep played after the transmitter drops
//! is heard by nobody, so releasing PTT does not release it immediately -
//! the tone goes out first and the release follows it. Everything here is the
//! sound and the rules; the holding-off lives in the engine that owns PTT.

/// What the beep sounds like and where it is used.
///
/// One set of numbers for every channel, and a tick per channel, because an
/// operator wants one beep of their own - not three that differ by accident.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RogerBeep {
    pub freq_hz: f32,
    /// 0.0 - 1.0 of full scale, as it leaves this client.
    pub volume: f32,
    pub duration_ms: u32,
    /// Whether FM counts as a mode to beep in. SSB and AM always do; data
    /// modes never do, at any setting - a tone on top of a data mode is
    /// corruption, not courtesy.
    pub include_fm: bool,
    pub on_thetis: bool,
    pub on_radio1: bool,
    pub on_radio2: bool,
}

impl Default for RogerBeep {
    fn default() -> Self {
        Self {
            // Around a kilohertz sits in the middle of every transmit filter
            // this project can be used with, so the default is audible before
            // anyone touches it.
            freq_hz: 1000.0,
            volume: 0.25,
            duration_ms: 150,
            include_fm: true,
            on_thetis: false,
            on_radio1: false,
            on_radio2: false,
        }
    }
}

/// The bounds the settings are held to, so a typed-in value cannot produce
/// silence, a click, or something a transmit filter would cut in half.
pub const FREQ_MIN_HZ: f32 = 300.0;
pub const FREQ_MAX_HZ: f32 = 2700.0;
pub const DURATION_MIN_MS: u32 = 50;
pub const DURATION_MAX_MS: u32 = 1500;

impl RogerBeep {
    pub fn clamped(mut self) -> Self {
        self.freq_hz = self.freq_hz.clamp(FREQ_MIN_HZ, FREQ_MAX_HZ);
        self.volume = self.volume.clamp(0.0, 1.0);
        self.duration_ms = self.duration_ms.clamp(DURATION_MIN_MS, DURATION_MAX_MS);
        self
    }

    /// Whether this channel is ticked. 0 = Thetis, 1 = radio 1, 2 = radio 2.
    pub fn enabled_for_channel(&self, channel: u8) -> bool {
        match channel {
            0 => self.on_thetis,
            1 => self.on_radio1,
            2 => self.on_radio2,
            _ => false,
        }
    }

    /// Whether a beep belongs in this mode.
    ///
    /// Mode numbers are the ones this project uses throughout: 0 LSB, 1 USB,
    /// 2 DSB, 3 CW-L, 4 CW-U, 5 FM, 6 AM, 7 DIGU, 8 SPEC, 9 DIGL, 10 SAM,
    /// 11 DRM, 12 C4FM.
    ///
    /// Voice modes only. CW has its own conventions and a tone on the end of a
    /// data or digital-voice transmission is interference with the payload,
    /// whatever the setting says.
    pub fn applies_to_mode(&self, mode: u8) -> bool {
        match mode {
            0 | 1 | 2 => true,       // LSB, USB, DSB
            6 | 10 => true,          // AM, SAM
            5 => self.include_fm,    // FM
            _ => false,
        }
    }

    pub fn should_beep(&self, channel: u8, mode: u8) -> bool {
        self.enabled_for_channel(channel) && self.applies_to_mode(mode)
    }
}

/// What the PTT handler should do with a request, while a beep may be running.
///
/// Every one of the four faults this feature shipped with was a wrong answer to
/// this question, and none of them was reachable from a test because the
/// question was only ever asked inline in the engine's audio loop. It is asked
/// here now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PttVerdict {
    /// Do what was asked - key up, or release.
    Proceed,
    /// A beep has started. Hold the transmitter and say nothing to the radio;
    /// the release follows when the tone has gone out.
    HoldForBeep,
    /// Ignore this entirely. It is a repeat of the release already in hand, and
    /// obeying it unkeys the transmitter mid-tone - after which the tone can
    /// never finish and the slot it holds is never given back, which silences
    /// every channel (build 65, 2026-08-14).
    Ignore,
}

/// Decide what a PTT request means while `beeping_channel` may hold a tone.
///
/// `was_transmitting` is the channel's own PTT state as it stood before this
/// request. A release arriving while already released is not the end of a
/// transmission, and beeping at it keys the transmitter up in order to do so
/// (build 62, 2026-08-14) - so it has to be part of the decision.
pub fn ptt_verdict(
    cfg: &RogerBeep,
    beeping_channel: Option<u8>,
    channel: u8,
    keyed: bool,
    was_transmitting: bool,
    mode: u8,
) -> PttVerdict {
    let ours = beeping_channel == Some(channel);
    if keyed {
        // Keyed again during our own beep: the operator has more to say and
        // wins over a courtesy. The caller cancels the tone and proceeds.
        return PttVerdict::Proceed;
    }
    if ours {
        return PttVerdict::Ignore;
    }
    if beeping_channel.is_none() && was_transmitting && cfg.should_beep(channel, mode) {
        return PttVerdict::HoldForBeep;
    }
    PttVerdict::Proceed
}

/// Whether a running tone should now give the transmitter back.
///
/// `overdue_after_ms` is the tone's own length plus a margin. A tone that
/// cannot be played out would otherwise hold PTT and the slot indefinitely -
/// the failure mode is a transmitter that stays keyed, which is out of all
/// proportion to a missing beep.
pub fn beep_is_over(finished: bool, age_ms: u64, duration_ms: u32, margin_ms: u64) -> bool {
    finished || age_ms > duration_ms as u64 + margin_ms
}

/// The margin the engine allows a tone beyond its own length.
pub const OVERDUE_MARGIN_MS: u64 = 1000;

/// A tone being played out, one frame at a time.
pub struct RogerTone {
    phase: f32,
    phase_step: f32,
    /// Samples still to produce, at the rate this was made for.
    remaining: usize,
    total: usize,
    fade: usize,
}

impl RogerTone {
    pub fn new(sample_rate: u32, cfg: &RogerBeep) -> Self {
        let cfg = cfg.clamped();
        let total = (sample_rate as u64 * cfg.duration_ms as u64 / 1000) as usize;
        // A tone that starts and stops at full amplitude clicks, and a click
        // carries far wider than the tone does. Five milliseconds each end is
        // inaudible as a fade and removes the splatter.
        let fade = ((sample_rate as u64 * 5) / 1000) as usize;
        Self {
            phase: 0.0,
            phase_step: std::f32::consts::TAU * cfg.freq_hz / sample_rate as f32,
            remaining: total,
            total,
            fade: fade.min(total / 2),
        }
    }

    pub fn finished(&self) -> bool {
        self.remaining == 0
    }

    /// Fill `out` with the next stretch of tone, padding with silence once the
    /// tone has run out. Returns how many of the written samples were tone.
    pub fn fill(&mut self, out: &mut [f32], volume: f32) -> usize {
        let mut written = 0;
        for slot in out.iter_mut() {
            if self.remaining == 0 {
                *slot = 0.0;
                continue;
            }
            let done = self.total - self.remaining;
            let env = if self.fade == 0 {
                1.0
            } else if done < self.fade {
                done as f32 / self.fade as f32
            } else if self.remaining <= self.fade {
                self.remaining as f32 / self.fade as f32
            } else {
                1.0
            };
            *slot = self.phase.sin() * volume * env;
            self.phase += self.phase_step;
            if self.phase > std::f32::consts::TAU {
                self.phase -= std::f32::consts::TAU;
            }
            self.remaining -= 1;
            written += 1;
        }
        written
    }
}

#[cfg(test)]
mod tests {
    use super::*;


    // ── The four faults this feature shipped with, as tests ────────────

    /// Build 62: ticking a channel made it key up, beep and unkey on its own.
    /// A release while already released is not the end of a transmission.
    #[test]
    fn a_release_while_not_transmitting_starts_nothing() {
        let cfg = RogerBeep { on_thetis: true, ..Default::default() };
        assert_eq!(
            ptt_verdict(&cfg, None, 0, false, false, 1),
            PttVerdict::Proceed,
            "beeping here keys the transmitter up to do it"
        );
        assert_eq!(ptt_verdict(&cfg, None, 0, false, true, 1), PttVerdict::HoldForBeep);
    }

    /// Build 65: Thetis sends its PTT state on more than the transitions. The
    /// second release found a beep running, fell through and unkeyed - after
    /// which the tone never finished and no channel could beep again.
    #[test]
    fn a_repeat_release_during_our_own_beep_is_ignored() {
        let cfg = RogerBeep { on_thetis: true, on_radio1: true, ..Default::default() };
        assert_eq!(ptt_verdict(&cfg, Some(0), 0, false, true, 1), PttVerdict::Ignore);
        // A different channel's release is not ours to swallow.
        assert_eq!(ptt_verdict(&cfg, Some(0), 1, false, true, 1), PttVerdict::Proceed);
    }

    /// Keying up during a beep hands the transmitter straight back.
    #[test]
    fn keying_up_during_a_beep_always_proceeds() {
        let cfg = RogerBeep { on_thetis: true, ..Default::default() };
        assert_eq!(ptt_verdict(&cfg, Some(0), 0, true, false, 1), PttVerdict::Proceed);
    }

    /// One tone at a time: a second channel releasing mid-beep does not start
    /// another, or two would share one slot and one would never be released.
    #[test]
    fn only_one_channel_beeps_at_a_time() {
        let cfg = RogerBeep { on_thetis: true, on_radio1: true, on_radio2: true, ..Default::default() };
        assert_eq!(ptt_verdict(&cfg, Some(0), 1, false, true, 1), PttVerdict::Proceed);
        assert_eq!(ptt_verdict(&cfg, Some(0), 2, false, true, 1), PttVerdict::Proceed);
    }

    /// A channel with the tick off never holds the transmitter.
    #[test]
    fn an_unticked_channel_never_holds_ptt() {
        let cfg = RogerBeep { on_thetis: false, ..Default::default() };
        assert_eq!(ptt_verdict(&cfg, None, 0, false, true, 1), PttVerdict::Proceed);
    }

    /// Nor does a mode that does not beep - a tone on data is interference.
    #[test]
    fn a_data_mode_never_holds_ptt() {
        let cfg = RogerBeep { on_thetis: true, ..Default::default() };
        assert_eq!(ptt_verdict(&cfg, None, 0, false, true, 7), PttVerdict::Proceed);
    }

    /// The failsafe: a tone that cannot play out still gives PTT back. This is
    /// the property the release gate asks for - "PTT can never hang" - and
    /// until now nothing tested it.
    #[test]
    fn a_tone_that_never_plays_still_releases_ptt() {
        // Not finished, and not yet overdue: keep holding.
        assert!(!beep_is_over(false, 150, 150, OVERDUE_MARGIN_MS));
        assert!(!beep_is_over(false, 1_100, 150, OVERDUE_MARGIN_MS));
        // Past its own length plus the margin: let go regardless.
        assert!(beep_is_over(false, 1_200, 150, OVERDUE_MARGIN_MS));
        // And a tone that did play out releases immediately.
        assert!(beep_is_over(true, 5, 150, OVERDUE_MARGIN_MS));
    }

    /// The longest beep that can be configured must still release, or the
    /// margin would be the thing that hangs.
    #[test]
    fn even_the_longest_beep_releases() {
        let longest = DURATION_MAX_MS;
        assert!(!beep_is_over(false, longest as u64, longest, OVERDUE_MARGIN_MS));
        assert!(beep_is_over(false, longest as u64 + OVERDUE_MARGIN_MS + 1, longest, OVERDUE_MARGIN_MS));
    }

    #[test]
    fn data_modes_never_beep() {
        let mut cfg = RogerBeep::default();
        cfg.include_fm = true;
        for mode in [3u8, 4, 7, 8, 9, 11, 12] {
            assert!(!cfg.applies_to_mode(mode), "mode {mode} should stay clear");
        }
    }

    #[test]
    fn fm_follows_its_own_switch_and_voice_does_not() {
        let mut cfg = RogerBeep::default();
        cfg.include_fm = false;
        assert!(!cfg.applies_to_mode(5), "FM off means no FM beep");
        for mode in [0u8, 1, 2, 6, 10] {
            assert!(cfg.applies_to_mode(mode), "mode {mode} beeps regardless of the FM switch");
        }
        cfg.include_fm = true;
        assert!(cfg.applies_to_mode(5));
    }

    #[test]
    fn channels_are_independent() {
        let cfg = RogerBeep { on_thetis: false, on_radio1: true, on_radio2: false, ..Default::default() };
        assert!(!cfg.should_beep(0, 1));
        assert!(cfg.should_beep(1, 1));
        assert!(!cfg.should_beep(2, 1));
    }

    /// The length asked for is the length produced, whatever frame size it is
    /// pulled out in - an operator setting 200 ms and getting 180 would key
    /// down for the wrong time.
    #[test]
    fn the_tone_lasts_as_long_as_it_was_told_to() {
        let cfg = RogerBeep { duration_ms: 200, ..Default::default() };
        let mut tone = RogerTone::new(16_000, &cfg);
        let mut produced = 0;
        let mut buf = [0.0f32; 320];
        while !tone.finished() {
            produced += tone.fill(&mut buf, 1.0);
        }
        assert_eq!(produced, 16_000 * 200 / 1000);
    }

    /// It has to start and end at silence, or it clicks - and a click is far
    /// wider than the tone it decorates.
    #[test]
    fn the_tone_fades_in_and_out() {
        let cfg = RogerBeep { duration_ms: 100, volume: 1.0, ..Default::default() };
        let mut tone = RogerTone::new(16_000, &cfg);
        let mut all = Vec::new();
        let mut buf = [0.0f32; 320];
        while !tone.finished() {
            let n = tone.fill(&mut buf, 1.0);
            all.extend_from_slice(&buf[..n]);
        }
        assert!(all[0].abs() < 0.05, "starts at {}", all[0]);
        assert!(all[all.len() - 1].abs() < 0.05, "ends at {}", all[all.len() - 1]);
        // And in the middle it is a real tone, not a whisper.
        let mid = all[all.len() / 2..all.len() / 2 + 200].iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(mid > 0.9, "middle only reached {mid}");
    }

    #[test]
    fn silly_settings_are_pulled_back_into_range() {
        let cfg = RogerBeep { freq_hz: 40_000.0, volume: 9.0, duration_ms: 60_000, ..Default::default() }.clamped();
        assert_eq!(cfg.freq_hz, FREQ_MAX_HZ);
        assert_eq!(cfg.volume, 1.0);
        assert_eq!(cfg.duration_ms, DURATION_MAX_MS);
    }

    /// Volume scales the tone and nothing else about it.
    #[test]
    fn volume_scales_the_tone() {
        let cfg = RogerBeep { duration_ms: 100, ..Default::default() };
        let peak = |v: f32| {
            let mut t = RogerTone::new(16_000, &cfg);
            let mut buf = [0.0f32; 1600];
            t.fill(&mut buf, v);
            buf.iter().fold(0.0f32, |m, s| m.max(s.abs()))
        };
        let full = peak(1.0);
        let quarter = peak(0.25);
        assert!((quarter / full - 0.25).abs() < 0.02, "{quarter} vs {full}");
    }
}
