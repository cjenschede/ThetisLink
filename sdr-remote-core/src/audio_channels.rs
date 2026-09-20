// SPDX-License-Identifier: GPL-2.0-or-later

//! Which channels of the Thetis bundle a given client receives.
//!
//! The bundler sends ONE multi-channel packet per client, built by filtering a
//! shared channel set. Three other places need the answer to "does this client
//! get anything at all out of it": the server's choice of whose link decides the
//! error correction, the count in the log line beside it, and the client's own
//! loss window - which must not charge loss for a stream it is not receiving.
//!
//! Those three used to each state the rule in their own words. On 2026-09-13 a
//! patch narrowed one of them to `rx1_enabled` alone, which is not the rule: a
//! desktop in Mono, BIN or Split with RX1 audio off and RX2 on still receives
//! CH2, and dropping it from the selection takes away the error correction of
//! the very listener whose link is worst. Two independent reviews found it.
//!
//! So the per-channel rule lives here once, and "receives anything" is derived
//! from it rather than restated.

/// Channel ids in the bundle. CH1 is the imaginary half of RX1, sent only in BIN.
pub const CH_RX1: u8 = 0;
pub const CH_RX1_IMAG: u8 = 1;
pub const CH_RX2: u8 = 2;

/// Every channel the bundle can carry, so "receives anything" can be derived
/// instead of hand-listed. A new channel added to the bundler belongs here, and
/// then every caller of [`receives_any_channel`] follows automatically.
pub const ALL_CHANNELS: [u8; 3] = [CH_RX1, CH_RX1_IMAG, CH_RX2];

/// Does `mode` allow this channel, given the client's two audio subscriptions?
///
/// `mode` is the client's `AudioMode` control: 0 = Mono, 1 = BIN, 2 = Split,
/// 255 = the default for a client that never sent one (Android).
pub fn channel_allowed(mode: u8, ch_id: u8, rx1_enabled: bool, rx2_enabled: bool) -> bool {
    let allowed_by_mode = match mode {
        255 => ch_id == CH_RX1,                          // Android: RX1 only
        0 => ch_id == CH_RX1 || ch_id == CH_RX2,         // Mono: RX1 + RX2
        1 => true,                                        // BIN: all
        2 => ch_id == CH_RX1 || ch_id == CH_RX2,         // Split: RX1 + RX2
        _ => ch_id == CH_RX1,
    };
    if !allowed_by_mode {
        return false;
    }
    // The subscriptions. CH0 and CH1 are both RX1 audio, so a VRX-only client
    // (RX1 audio deliberately off) loses both.
    if (ch_id == CH_RX1 || ch_id == CH_RX1_IMAG) && !rx1_enabled {
        return false;
    }
    if ch_id == CH_RX2 && !rx2_enabled {
        return false;
    }
    true
}

/// Does this client receive at least one channel of the bundle?
///
/// This is the predicate for "we are sending to them", and therefore for "their
/// link gets a vote in the error correction" and "they count in the log line".
pub fn receives_any_channel(mode: u8, rx1_enabled: bool, rx2_enabled: bool) -> bool {
    ALL_CHANNELS
        .iter()
        .any(|&ch| channel_allowed(mode, ch, rx1_enabled, rx2_enabled))
}

/// Whose link decides the error correction for the Thetis bundle.
///
/// One set of encoders feeds everybody, so protection cannot be granted per
/// listener: the worst link wins. Only clients that actually receive a channel
/// get a vote - a client we send nothing to reports loss about a stream it is
/// not getting.
///
/// Returns the address and its loss, so the log line can name the listener
/// instead of only the percentage. `None` when nobody is receiving.
///
/// Ties go to the LAST client in the list. Deliberate and pinned by a test:
/// `max_by_key` behaves that way, the choice between two equally bad links is
/// arbitrary anyway, and a silent change of it would make a log line move
/// between clients for no reason anyone could explain.
pub fn worst_receiver<A: Copy>(clients: &[(A, u8, bool, bool, u8)]) -> Option<(A, u8)> {
    clients
        .iter()
        .filter(|(_, mode, rx1, rx2, _)| receives_any_channel(*mode, *rx1, *rx2))
        .map(|&(a, _, _, _, loss)| (a, loss))
        .max_by_key(|(_, loss)| *loss)
}

/// How many clients the bundle is actually going to, for the log line beside it.
pub fn receiver_count<A: Copy>(clients: &[(A, u8, bool, bool, u8)]) -> usize {
    clients
        .iter()
        .filter(|(_, mode, rx1, rx2, _)| receives_any_channel(*mode, *rx1, *rx2))
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MONO: u8 = 0;
    const BIN: u8 = 1;
    const SPLIT: u8 = 2;
    const ANDROID: u8 = 255;

    #[test]
    fn rx2_only_listener_still_receives_the_bundle() {
        // The defect of build 34, in one line. A desktop that switched RX1 audio
        // off but kept RX2 gets CH2 - so it is a receiver, and its link must keep
        // its vote in the error correction.
        for mode in [MONO, BIN, SPLIT] {
            assert!(
                receives_any_channel(mode, false, true),
                "mode {mode}: an RX2-only listener must count as receiving"
            );
            assert!(channel_allowed(mode, CH_RX2, false, true));
        }
    }

    #[test]
    fn rx1_only_listener_receives_the_bundle() {
        for mode in [MONO, BIN, SPLIT, ANDROID] {
            assert!(receives_any_channel(mode, true, false), "mode {mode}");
        }
    }

    #[test]
    fn a_client_with_both_streams_off_receives_nothing() {
        // The VRX-only client, and the phone whose Thetis streams the data saving
        // switched off. Absence of audio is then not loss, and its link must not
        // decide anything for anybody.
        for mode in [MONO, BIN, SPLIT, ANDROID] {
            assert!(!receives_any_channel(mode, false, false), "mode {mode}");
        }
    }

    #[test]
    fn android_never_receives_rx2_even_when_it_asks() {
        // Mode 255 is RX1-only by construction. If Android ever gains RX2 this
        // test fails, and that is the point: the mode table is then out of date.
        assert!(!channel_allowed(ANDROID, CH_RX2, false, true));
        assert!(!receives_any_channel(ANDROID, false, true));
    }

    #[test]
    fn only_bin_carries_the_imaginary_half_of_rx1() {
        assert!(channel_allowed(BIN, CH_RX1_IMAG, true, false));
        for mode in [MONO, SPLIT, ANDROID] {
            assert!(!channel_allowed(mode, CH_RX1_IMAG, true, false), "mode {mode}");
        }
    }

    #[test]
    fn an_unknown_mode_falls_back_to_rx1_only() {
        // Defensive, and deliberately not "everything": a mode nobody recognises
        // should cost bandwidth, not gain it.
        assert!(channel_allowed(7, CH_RX1, true, true));
        assert!(!channel_allowed(7, CH_RX2, true, true));
    }

    // ---- worst_receiver ----
    //
    // Five assertions, and three of them would have caught something real: the
    // RX2-only case is the defect two reviews found in build 34, the
    // nobody-receiving case is the phone in data-saving mode that pinned maximum
    // redundancy on a desktop, and the tie is the one that has no defect behind
    // it yet - it is pinned so a refactor cannot move it by accident.

    #[test]
    fn the_worst_receiving_link_wins() {
        let clients = [("a", MONO, true, false, 3u8), ("b", MONO, true, false, 40)];
        assert_eq!(worst_receiver(&clients), Some(("b", 40)));
    }

    #[test]
    fn a_client_we_send_nothing_to_never_wins_however_bad_it_reports() {
        // The phone whose Thetis streams the data saving switched off. It reports
        // about a stream it asked us to stop; letting it decide put maximum error
        // correction on a desktop nobody complained about.
        let clients = [("desktop", MONO, true, false, 2u8), ("phone", ANDROID, false, false, 100)];
        assert_eq!(worst_receiver(&clients), Some(("desktop", 2)));
        assert_eq!(receiver_count(&clients), 1);
    }

    #[test]
    fn an_rx2_only_listener_keeps_its_vote() {
        // Build 34 filtered on RX1 alone and took this listener's protection away.
        let clients = [("rx1", MONO, true, false, 1u8), ("rx2only", SPLIT, false, true, 30)];
        assert_eq!(worst_receiver(&clients), Some(("rx2only", 30)));
        assert_eq!(receiver_count(&clients), 2);
    }

    #[test]
    fn nobody_receiving_means_no_decision_at_all() {
        let clients = [("vrx", MONO, false, false, 100u8)];
        assert_eq!(worst_receiver(&clients), None);
        assert_eq!(receiver_count(&clients), 0);
        assert_eq!(worst_receiver::<&str>(&[]), None);
    }

    #[test]
    fn a_tie_goes_to_the_last_one_listed() {
        let clients = [("a", MONO, true, false, 20u8), ("b", MONO, true, false, 20)];
        assert_eq!(worst_receiver(&clients), Some(("b", 20)));
    }

    #[test]
    fn receiving_is_derived_from_the_per_channel_rule_and_not_restated() {
        // The whole reason this module exists. Exhaustive over the small space:
        // "receives anything" must agree with the channel filter for every
        // combination, so the two can never drift apart.
        for mode in [MONO, BIN, SPLIT, ANDROID, 7] {
            for rx1 in [false, true] {
                for rx2 in [false, true] {
                    let any = ALL_CHANNELS
                        .iter()
                        .any(|&ch| channel_allowed(mode, ch, rx1, rx2));
                    assert_eq!(
                        receives_any_channel(mode, rx1, rx2),
                        any,
                        "mode {mode}, rx1 {rx1}, rx2 {rx2}"
                    );
                }
            }
        }
    }
}
