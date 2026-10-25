// SPDX-License-Identifier: GPL-2.0-or-later

//! What this client has asked the server for, so it can ask again.
//!
//! The server keys clients by address. Come back on a new one - which is what a
//! phone does at every network switch - and the server builds a fresh
//! `ClientSession` at its defaults, having forgotten every subscription. The
//! client has to say it all again.
//!
//! Until now that "say it all again" was seventeen hand-written sends in the
//! engine's restore block, and the per-session fields on the server side were a
//! second hand-written list. They had drifted: thirteen fields were not replayed
//! by anyone, among them both Yaesu audio subscriptions and both Yaesu state
//! subscriptions. In the field that reads as a radio that went silent and a
//! screen that stopped following the VFO knob, with no error anywhere - and the
//! one control that looks like it should fix it does nothing.
//!
//! This records what actually went out, and replays it. Two properties matter:
//!
//! - it is **derived**, not retyped. A control added to
//!   [`SESSION_SCOPED_CONTROLS`] is replayed without anyone remembering to add a
//!   send here.
//! - it replays the **last intended value**, not a fixed "everything on". A
//!   stream switched off to save data therefore stays off across a network
//!   switch, instead of the switch quietly turning it back on.

use sdr_remote_core::protocol::{ControlId, ControlPacket, PacketType, SESSION_SCOPED_CONTROLS};

/// Record `buf` if - and only if - it is a control packet.
///
/// This sits in the send path of **every** outgoing packet, so it has to be
/// cheap on the packets it is not interested in. The first version simply called
/// [`ControlPacket::deserialize`] and ignored the error; a review measured what
/// that costs on the way out: the error path formats a message into an
/// `anyhow::Error`, so every heartbeat and every transmitted audio packet paid a
/// heap allocation to be told something this code already knew. In a project
/// whose first priority is latency, on the path that carries transmitted audio,
/// that is the wrong trade.
///
/// The header carries the packet type in byte 2, so one comparison answers the
/// question before any parsing begins. The full parse still runs for real
/// controls, which keeps the recognition exact rather than a guess from length.
pub fn record_if_control(intent: &mut SessionIntent, buf: &[u8]) {
    if buf.len() < ControlPacket::SIZE || buf[2] != PacketType::Control as u8 {
        return;
    }
    if let Ok(ctrl) = ControlPacket::deserialize(buf) {
        intent.record(ctrl.control_id, ctrl.value);
    }
}

/// The line the field log is read for after a reconnect.
///
/// Kept as a function so its wording is testable: a log line that only exists
/// inside a loop no assertion can open is the one thing working rule 11 wants
/// pinned down, and "I will check it by eye in the console" is how the wording
/// drifts away from what the patch brief promised.
///
/// `sent` is the number that actually left the socket, not the number that was
/// queued. Those differ exactly when it matters - a network switch is the moment
/// a send is most likely to fail - and a line that reports the intent as if it
/// were the outcome would read green through precisely the failure it exists to
/// show.
pub fn restore_summary(replay: &[(ControlId, u16)], sent: usize) -> String {
    let value_of = |want: ControlId| -> i32 {
        replay
            .iter()
            .find(|(c, _)| *c == want)
            .map_or(-1, |(_, v)| *v as i32)
    };
    format!(
        "Session restore: replayed {}/{} per-session control(s) \
         (yaesu={} yaesu_state={} yaesu2={} yaesu2_state={}; -1 = never asked for)",
        sent,
        replay.len(),
        value_of(ControlId::YaesuEnable),
        value_of(ControlId::YaesuStateEnable),
        value_of(ControlId::Yaesu2Enable),
        value_of(ControlId::Yaesu2StateEnable),
    )
}

/// The last value sent for each per-session control.
#[derive(Debug, Default, Clone)]
pub struct SessionIntent {
    /// Parallel to [`SESSION_SCOPED_CONTROLS`]: `None` until that control has
    /// been sent at least once. A control never sent is a control the server was
    /// never asked about, and replaying a guess would be worse than silence.
    values: Vec<Option<u16>>,
}

impl SessionIntent {
    pub fn new() -> Self {
        Self {
            values: vec![None; SESSION_SCOPED_CONTROLS.len()],
        }
    }

    fn index_of(id: ControlId) -> Option<usize> {
        SESSION_SCOPED_CONTROLS.iter().position(|c| *c == id)
    }

    /// Note a control that has just been sent. Controls outside the per-session
    /// set are ignored - they are not lost when a session is replaced, so
    /// replaying them would be noise on the wire.
    pub fn record(&mut self, id: ControlId, value: u16) {
        if let Some(i) = Self::index_of(id) {
            self.values[i] = Some(value);
        }
    }

    /// What to send after a new session, in [`SESSION_SCOPED_CONTROLS`] order.
    ///
    /// The order is the list's, not the order things happened in: an `Enable`
    /// precedes the settings that shape that stream, so the server is never
    /// asked to shape a stream it is not producing yet.
    pub fn replay(&self) -> Vec<(ControlId, u16)> {
        SESSION_SCOPED_CONTROLS
            .iter()
            .zip(self.values.iter())
            .filter_map(|(id, v)| v.map(|v| (*id, v)))
            .collect()
    }

    /// How many per-session controls have ever been sent. For the log line: a
    /// replay of zero after a reconnect is worth seeing, because it means this
    /// client asked for nothing and the silence that follows is expected.
    pub fn len(&self) -> usize {
        self.values.iter().filter(|v| v.is_some()).count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_control_that_was_never_sent_is_not_replayed() {
        let intent = SessionIntent::new();
        assert!(intent.replay().is_empty());
        assert!(intent.is_empty());
    }

    #[test]
    fn the_last_value_wins_because_that_is_what_the_operator_meant() {
        let mut intent = SessionIntent::new();
        intent.record(ControlId::YaesuEnable, 1);
        intent.record(ControlId::YaesuEnable, 0);
        assert_eq!(intent.replay(), vec![(ControlId::YaesuEnable, 0)]);
    }

    #[test]
    fn a_stream_switched_off_stays_off_across_a_reconnect() {
        // The data-saving case, and the reason replay is not "everything on":
        // switching Thetis audio off in Yaesu mode must survive a network
        // switch, or every switch silently restores the traffic it saved.
        let mut intent = SessionIntent::new();
        intent.record(ControlId::Rx1Enable, 1);
        intent.record(ControlId::SpectrumEnable, 1);
        intent.record(ControlId::Rx1Enable, 0);
        intent.record(ControlId::SpectrumEnable, 0);
        let replay = intent.replay();
        assert!(replay.contains(&(ControlId::Rx1Enable, 0)));
        assert!(replay.contains(&(ControlId::SpectrumEnable, 0)));
    }

    #[test]
    fn controls_that_are_not_per_session_are_not_carried() {
        let mut intent = SessionIntent::new();
        // Frequency is radio state, not a subscription: a new session inherits
        // it from the radio, and replaying it would fight whoever tuned since.
        intent.record(ControlId::Rx1AfGain, 42);
        assert!(intent.replay().is_empty());
    }

    #[test]
    fn replay_follows_the_list_order_not_the_order_things_happened() {
        let mut intent = SessionIntent::new();
        // Recorded backwards on purpose.
        intent.record(ControlId::SpectrumMaxBins, 2048);
        intent.record(ControlId::SpectrumEnable, 1);
        let replay = intent.replay();
        let enable_at = replay.iter().position(|(c, _)| *c == ControlId::SpectrumEnable);
        let bins_at = replay.iter().position(|(c, _)| *c == ControlId::SpectrumMaxBins);
        assert!(
            enable_at < bins_at,
            "the enable must precede the settings that shape that stream"
        );
    }

    fn control_bytes(id: ControlId, value: u16) -> [u8; ControlPacket::SIZE] {
        let mut buf = [0u8; ControlPacket::SIZE];
        ControlPacket { control_id: id, value }.serialize(&mut buf);
        buf
    }

    #[test]
    fn a_control_on_the_wire_is_recognised_and_recorded() {
        let mut intent = SessionIntent::new();
        record_if_control(&mut intent, &control_bytes(ControlId::Yaesu2Enable, 1));
        assert_eq!(intent.replay(), vec![(ControlId::Yaesu2Enable, 1)]);
    }

    #[test]
    fn an_audio_packet_is_not_mistaken_for_a_control() {
        // The reason recognition reads the header's type byte instead of
        // guessing from the length: audio leaves through the same path.
        let mut intent = SessionIntent::new();
        let mut audio = [0u8; 64];
        audio[0] = sdr_remote_core::protocol::MAGIC;
        audio[1] = sdr_remote_core::protocol::VERSION;
        audio[2] = PacketType::Audio as u8;
        record_if_control(&mut intent, &audio);
        assert!(intent.replay().is_empty());
    }

    #[test]
    fn a_buffer_too_short_to_be_a_control_is_left_alone() {
        let mut intent = SessionIntent::new();
        let stub = [sdr_remote_core::protocol::MAGIC, 0, PacketType::Control as u8];
        record_if_control(&mut intent, &stub);
        assert!(intent.replay().is_empty());
    }

    #[test]
    fn the_summary_names_both_yaesu_slots_and_what_actually_left() {
        let replay = vec![
            (ControlId::Yaesu2Enable, 1),
            (ControlId::Yaesu2StateEnable, 1),
        ];
        let line = restore_summary(&replay, 2);
        assert!(line.starts_with("Session restore: replayed 2/2"), "{line}");
        assert!(line.contains("yaesu2=1"), "{line}");
        assert!(line.contains("yaesu2_state=1"), "{line}");
        // Never asked for reads as -1, not as 0: "off" and "never mentioned"
        // are different things, and an operator reading this log has to be able
        // to tell them apart.
        assert!(line.contains("yaesu=-1"), "{line}");
    }

    #[test]
    fn the_summary_reports_a_failed_send_instead_of_the_intent() {
        let replay = vec![(ControlId::YaesuEnable, 1), (ControlId::Rx1Enable, 1)];
        let line = restore_summary(&replay, 1);
        assert!(
            line.contains("replayed 1/2"),
            "a line that reported 2/2 here would read green through the failure \
             it exists to show: {line}"
        );
    }

    /// The two call sites in the engine, checked in its source.
    ///
    /// Why this shape: the restore branch sits inside a loop that no assertion
    /// can open, so nothing can call it from a test. Reverting the two engine
    /// hunks would leave every other test in this file green - a reviewer
    /// pointed that out, and he was right. This is the cheapest thing that
    /// notices, and it fails with a sentence saying what went missing.
    #[test]
    fn the_engine_still_records_what_it_sends_and_replays_it() {
        let src = include_str!("engine.rs");
        assert!(
            src.contains("record_if_control(&mut session_intent"),
            "the engine no longer records the controls it sends, so nothing will \
             be replayed after a new session and the Yaesu goes silent on the \
             next network switch"
        );
        assert!(
            src.contains("session_intent.replay()"),
            "the engine no longer replays the recorded controls in its restore \
             branch"
        );
        assert!(
            src.contains("restore_summary(&replay"),
            "the restore no longer logs its named line, so a field log cannot \
             show whether this works (working rule 11)"
        );
        for flag in [
            "yaesu_logged_first = false",
            "yaesu2_logged_first = false",
            "vrx1_logged_first = false",
            "vrx2_logged_first = false",
        ] {
            // Anchored on the one line that only the restore branch carries.
            // The first version of this anchored on `logged_first_rx = false;`,
            // which also matches that variable's DECLARATION hundreds of lines
            // earlier - so it inspected the wrong window and failed on correct
            // code. Caught by running it before trusting it.
            let in_restore = src
                .split("Connected to server (rtt=")
                .nth(1)
                .map_or(false, |after| after[..after.len().min(6000)].contains(flag));
            assert!(
                in_restore,
                "`{flag}` is no longer cleared in the restore branch. \
                 `Radio N: first audio packet` is the only line that shows the \
                 radio's audio came back, and it sits behind that flag - so the \
                 restore would work and log nothing about it on exactly the short \
                 reconnect this patch exists for"
            );
        }
    }

    #[test]
    fn every_per_session_control_can_be_recorded_and_comes_back() {
        // The property that makes this derived rather than retyped: add a
        // control to the shared list and it is replayed without touching this
        // file. If that ever stops being true, this test says so.
        let mut intent = SessionIntent::new();
        for (i, id) in SESSION_SCOPED_CONTROLS.iter().enumerate() {
            intent.record(*id, i as u16);
        }
        let replay = intent.replay();
        assert_eq!(replay.len(), SESSION_SCOPED_CONTROLS.len());
        for (i, id) in SESSION_SCOPED_CONTROLS.iter().enumerate() {
            assert_eq!(
                replay[i],
                (*id, i as u16),
                "{id:?} did not come back as recorded"
            );
        }
    }
}
