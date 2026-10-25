// SPDX-License-Identifier: GPL-2.0-or-later

use std::sync::Mutex;

use log::{info, warn};
use tokio::sync::{mpsc, watch};

use sdr_remote_core::protocol::ControlId;
use sdr_remote_logic::audio::AudioBackend;
use sdr_remote_logic::ble_ptt::{
    BleButtonEvent, BlePttAction as LogicAction, BlePttState,
};
use sdr_remote_logic::commands::Command;
use sdr_remote_logic::ptt_intent::{
    PhonePtt as LogicPhonePtt, PttSource as LogicPttSource, PttTarget as LogicPttTarget,
};
use sdr_remote_logic::engine::ClientEngine;
use sdr_remote_logic::state::RadioState;

/// Namespace function: returns shared version string (with build number in dev)
pub fn version() -> String {
    sdr_remote_core::version_string()
}

/// Is this relay actually set up to run?
///
/// The same rule the bridge uses to decide whether to build the tunnel, handed
/// to Compose rather than recomputed there. It was recomputed there, with three
/// of the four fields: with a relay switched on, an address and a station name
/// filled in but the token still empty, Kotlin called it configured while the
/// Rust side did not - so filling in the token afterwards changed nothing on
/// screen and the "restart to apply" notice never appeared, at exactly the
/// moment the relay started working (review finding, 2026-08-20).
pub fn relay_is_configured(enabled: bool, url: String, station: String, token: String) -> bool {
    sdr_remote_relay::is_configured(enabled, &url, &station, &token)
}

/// The word for the route these settings ask for, to hold against the
/// state's `route` - the rule the bridge chooses with (`Route::asked_for`),
/// handed to Compose rather than kept there a second time (review 118).
pub fn expected_route_label(relay_enabled: bool, url: String, station: String, token: String) -> String {
    sdr_remote_logic::measure::Route::asked_for(sdr_remote_relay::is_configured(relay_enabled, &url, &station, &token))
        .label()
        .to_string()
}

/// Filter edges for one press of a width button.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BridgeFilterEdges {
    pub low_hz: i32,
    pub high_hz: i32,
}

/// The edges one press of a width button sends, or nothing when there is no step
/// that way. The rule is `sdr_remote_logic::filter_width`, shared with the
/// desktop; Compose kept its own copy until build 47, and that copy is the one
/// report 74 locked up in.
pub fn filter_step(mode: u8, low_hz: i32, high_hz: i32, wider: bool) -> Option<BridgeFilterEdges> {
    use sdr_remote_logic::filter_width::{step, Step};
    let dir = if wider { Step::Wider } else { Step::Narrower };
    step(mode, low_hz, high_hz, dir).map(|(low_hz, high_hz)| BridgeFilterEdges { low_hz, high_hz })
}

/// The width between the buttons, as Thetis has it.
pub fn filter_width_label(mode: u8, low_hz: i32, high_hz: i32) -> String {
    sdr_remote_logic::filter_width::width_label(mode, low_hz, high_hz)
}

/// The S reading for a level in dBm (build 218).
pub fn s_meter_text(dbm: f32) -> String {
    sdr_remote_core::s_meter::text(dbm)
}

/// Where a level in dBm sits on the S-meter bar, 0 to 1: the desktop's scale
/// (`dbm_to_display`), so the bar stands where the desktop has it (build 219).
pub fn s_meter_fraction(dbm: f32) -> f32 {
    f32::from(sdr_remote_core::dbm_to_display(dbm)) / 228.0
}

/// A Yaesu CAT S-meter value in dBm, capped at S9+60 like the desktop
/// (build 219).
pub fn yaesu_s_meter_dbm(raw: u32) -> f32 {
    sdr_remote_core::s_meter::yaesu_raw_to_dbm(raw.min(u32::from(u16::MAX)) as u16)
}

#[cfg(test)]
mod filter_bridge_tests {
    use super::*;

    /// The phone's S-meter scale is the desktop's (review of 218): S9 at
    /// 108/228, the ends clamped, and a Yaesu value past the scale is S9+60.
    #[test]
    fn the_phone_s_meter_scale_is_the_desktops() {
        assert_eq!(s_meter_fraction(-73.0), 108.0 / 228.0);
        assert_eq!(s_meter_fraction(-200.0), 0.0);
        assert_eq!(s_meter_fraction(0.0), 1.0);
        assert_eq!(s_meter_fraction(-103.0), 48.0 / 228.0);
        assert_eq!(yaesu_s_meter_dbm(108), -73.0);
        assert_eq!(yaesu_s_meter_dbm(255), -13.0);
        assert_eq!(s_meter_text(yaesu_s_meter_dbm(255)), "S9+60dB");
        assert_eq!(yaesu_s_meter_dbm(70_000), -13.0);
    }

    /// The two hops Kotlin makes: the direction flag and the edges that come back.
    /// The bridge had no test at all, so swapping `wider` left all of them green
    /// (review finding, 2026-09-16) - and Compose would then narrow on plus.
    #[test]
    fn the_direction_flag_and_the_edges_survive_the_bridge() {
        assert_eq!(
            filter_step(1, 100, 2800, true),
            Some(BridgeFilterEdges { low_hz: 100, high_hz: 3100 })
        );
        assert_eq!(
            filter_step(1, 100, 2800, false),
            Some(BridgeFilterEdges { low_hz: 100, high_hz: 2500 })
        );
        assert_eq!(filter_step(1, 100, 1900, false), None, "nothing narrower than the bottom step");
        assert_eq!(filter_step(5, -8000, 8000, true), None, "FM has no steps");
    }

    #[test]
    fn the_label_crosses_the_bridge_unchanged() {
        assert_eq!(filter_width_label(1, 7579, 10000), "2.42 kHz");
        assert_eq!(filter_width_label(4, 350, 850), "500 Hz");
        assert_eq!(filter_width_label(1, 0, 0), "--");
    }
}

/// What to do with the connection after a Bluetooth link went away.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BleDropPolicy {
    Reconnect,
    Close,
}

/// Wait for the button to come back, or let the client go?
///
/// One input. The second half - "is this still our connection" - moved into
/// the gate, which knows the generation; asking it here as well was a check
/// that could no longer fail.
pub fn ble_drop_policy(wanted: bool) -> BleDropPolicy {
    match sdr_remote_logic::ble_ptt::on_drop(wanted) {
        sdr_remote_logic::ble_ptt::DropPolicy::Reconnect => BleDropPolicy::Reconnect,
        sdr_remote_logic::ble_ptt::DropPolicy::Close => BleDropPolicy::Close,
    }
}

/// What a PTT button shows, handed to Kotlin rather than written a second time
/// there.
///
/// The rule was centralised into `sdr_remote_logic::ptt_button` for the three
/// desktop buttons, and Compose kept its own copy of the same expression - so
/// the one place with tests was the one place the phone did not use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PttButton {
    Busy,
    Transmitting,
    Idle,
}

/// Ownership first, then our own request. See the logic crate for why the
/// radio's own TX state is not an input.
pub fn ptt_button(held_by_other: bool, want_tx: bool) -> PttButton {
    match sdr_remote_logic::ptt_button::ptt_button(held_by_other, want_tx) {
        sdr_remote_logic::ptt_button::PttButton::Busy => PttButton::Busy,
        sdr_remote_logic::ptt_button::PttButton::Transmitting => PttButton::Transmitting,
        sdr_remote_logic::ptt_button::PttButton::Idle => PttButton::Idle,
        // Cannot arise here: this path calls ptt_button(), the variant without a block of
        // our own, and the phone keys one radio at a time anyway.
        //
        // Named rather than left out all the same - the compiler forced it and that is
        // exactly right. If Android ever gets several transmitters at once, this line has
        // to be looked at again instead of quietly reading as "free".
        sdr_remote_logic::ptt_button::PttButton::BlockedByOwnTx => PttButton::Idle,
    }
}

/// What the Bluetooth PTT button asks for, on its way to Compose.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlePttAction {
    /// Nothing changed. Touch nothing.
    Nothing,
    KeyDown,
    KeyUp,
    /// The link is gone, whatever the reason. Not the same as a release: a
    /// release means nothing in toggle mode, and this always releases.
    Gone,
    /// A byte this button is not known to send. Log it and change nothing;
    /// Kotlin still has the byte it just passed in.
    Unrecognised,
}

/// The transmit decision for a Bluetooth PTT button, handed to Kotlin rather
/// than written a second time there.
///
/// The rule underneath - every drop releases, a connection starts in not
/// transmitting - is the one that keeps a licence holder off the air by
/// accident, and it is tested in `sdr-remote-logic` without a phone and
/// without a button. Kotlin owns the GATT callbacks and nothing else: it
/// reports what happened and does what this says.
pub struct BlePttGate {
    inner: Mutex<BlePttState>,
}

impl Default for BlePttGate {
    fn default() -> Self {
        Self::new()
    }
}

impl BlePttGate {
    pub fn new() -> Self {
        Self { inner: Mutex::new(BlePttState::new()) }
    }

    /// A new connection attempt starts and becomes the only one that counts.
    ///
    /// Everything still in flight from an older attempt is ignored from here
    /// on, and whatever was held is released - a press cannot survive a
    /// replaced link, because the button only reports changes.
    pub fn begin(&self, generation: u64) -> BlePttAction {
        Self::translate(self.inner.lock().unwrap().begin(generation))
    }

    pub fn connected(&self, generation: u64) -> BlePttAction {
        self.apply(generation, BleButtonEvent::Connected)
    }

    pub fn disconnected(&self, generation: u64) -> BlePttAction {
        self.apply(generation, BleButtonEvent::Disconnected)
    }

    pub fn notification(&self, generation: u64, value: u8) -> BlePttAction {
        self.apply(generation, BleButtonEvent::Notification(value))
    }

    /// Is the physical button down? For a log line - whether that means
    /// transmitting is PhonePtt's to say.
    pub fn held(&self) -> bool {
        self.inner.lock().unwrap().held()
    }

    fn apply(&self, generation: u64, event: BleButtonEvent) -> BlePttAction {
        Self::translate(self.inner.lock().unwrap().apply(generation, event))
    }

    fn translate(action: Option<LogicAction>) -> BlePttAction {
        match action {
            None => BlePttAction::Nothing,
            Some(LogicAction::KeyDown) => BlePttAction::KeyDown,
            Some(LogicAction::KeyUp) => BlePttAction::KeyUp,
            Some(LogicAction::Gone) => BlePttAction::Gone,
            Some(LogicAction::Unrecognised(_)) => BlePttAction::Unrecognised,
        }
    }
}

// --------------------------------------------------------------- the PTT button
//
// Same division of labour as BlePttGate above: the rule lives in
// `sdr-remote-logic` and is tested there without a phone, this is the lock around
// it, and Kotlin does what it says. Until now the ViewModel kept flags of its own
// - `requestedPtt`, `requestedPttTarget` and `_transmitting`, the last of which
// always moved together with the first. Two booleans that have to stay equal in
// four places is exactly the shape Lane 6 clears up.

/// Which transmitter the phone is working.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PttTarget {
    Thetis,
    Yaesu0,
    Yaesu1,
}

/// What the operator presses with. The phone has two; the desktop knows more
/// (mouse, spacebar, MIDI) and those live in the same shared type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PttSource {
    Screen,
    Bluetooth,
    Midi,
    /// The volume rocker and the page-turner keys of a BLE HID remote. Both
    /// arrive as key events to the foreground activity, so both stop reaching
    /// the app the moment the screen goes.
    VolumeKey,
}

impl PttTarget {
    fn to_logic(self) -> LogicPttTarget {
        match self {
            PttTarget::Thetis => LogicPttTarget::Thetis,
            PttTarget::Yaesu0 => LogicPttTarget::Yaesu0,
            PttTarget::Yaesu1 => LogicPttTarget::Yaesu1,
        }
    }

    fn from_logic(t: LogicPttTarget) -> Self {
        match t {
            LogicPttTarget::Thetis => PttTarget::Thetis,
            LogicPttTarget::Yaesu0 => PttTarget::Yaesu0,
            LogicPttTarget::Yaesu1 => PttTarget::Yaesu1,
        }
    }
}

impl PttSource {
    fn to_logic(self) -> LogicPttSource {
        match self {
            PttSource::Screen => LogicPttSource::Screen,
            PttSource::Bluetooth => LogicPttSource::Bluetooth,
            PttSource::Midi => LogicPttSource::Midi,
            PttSource::VolumeKey => LogicPttSource::VolumeKey,
        }
    }
}

/// The phone's PTT state, on the shared rule.
pub struct PhonePtt {
    inner: Mutex<LogicPhonePtt>,
}

impl Default for PhonePtt {
    fn default() -> Self {
        Self::new()
    }
}

impl PhonePtt {
    pub fn new() -> Self {
        Self { inner: Mutex::new(LogicPhonePtt::new()) }
    }

    /// One source, pressed or let go. Pressing releases the other transmitters - the
    /// phone keys one at a time, and here that is a rule.
    pub fn set(&self, target: PttTarget, source: PttSource, held: bool) {
        self.inner.lock().unwrap().set(target.to_logic(), source.to_logic(), held);
    }

    pub fn want_tx(&self, target: PttTarget) -> bool {
        self.inner.lock().unwrap().want_tx(target.to_logic())
    }

    /// Which transmitter is the operator asking for? At most one.
    pub fn asking(&self) -> Option<PttTarget> {
        self.inner.lock().unwrap().asking().map(PttTarget::from_logic)
    }

    pub fn refused(&self) {
        self.inner.lock().unwrap().refused();
    }

    pub fn busy(&self, target: PttTarget) {
        self.inner.lock().unwrap().busy(target.to_logic());
    }

    pub fn disconnected(&self) {
        self.inner.lock().unwrap().disconnected();
    }

    pub fn radio_left_tx(&self, target: PttTarget) {
        self.inner.lock().unwrap().radio_left_tx(target.to_logic());
    }

    /// The operator switches the on-screen button off: everything on that transmitter
    /// ends, including a Bluetooth button that is still held. That is the way out when
    /// it jams.
    pub fn operator_stop(&self, target: PttTarget) {
        self.inner.lock().unwrap().operator_stop(target.to_logic());
    }

    /// The screen is gone - locked or in the background. Only the sources that die
    /// with the screen; the Bluetooth button stays.
    pub fn screen_gone(&self) {
        self.inner.lock().unwrap().screen_gone();
    }

    /// The Bluetooth button is no longer holding anything. Every link event
    /// reports this, and it is unconditional - see PhonePtt::bluetooth_gone.
    pub fn bluetooth_gone(&self) {
        self.inner.lock().unwrap().bluetooth_gone();
    }

    /// Hold to transmit, or one press on and the next off. Compose re-applies
    /// this on every redraw, so setting it to what it already is changes nothing.
    pub fn set_toggle_mode(&self, toggle: bool) {
        self.inner.lock().unwrap().set_toggle_mode(toggle);
    }

    /// A control went down. What that means - latch, or hold - is decided here
    /// and not by the button.
    pub fn down(&self, target: PttTarget, source: PttSource) {
        self.inner.lock().unwrap().down(target.to_logic(), source.to_logic());
    }

    /// A control came up.
    pub fn up(&self, target: PttTarget, source: PttSource) {
        self.inner.lock().unwrap().up(target.to_logic(), source.to_logic());
    }
}

/// DX cluster spot exposed to Kotlin via uniffi.
pub struct BridgeDxSpot {
    pub callsign: String,
    pub frequency_hz: u64,
    pub mode: String,
    pub spotter: String,
    pub comment: String,
    pub age_seconds: u16,
    pub expiry_seconds: u16,
}

/// One chat message, as Compose draws it.
///
/// The two judgements that need the model to make them - is this ours, and may
/// it still be corrected - are answered here rather than left to Kotlin, so the
/// phone and the desktop cannot disagree about whose message it is.
pub struct BridgeChatMessage {
    pub id: i64,
    /// Unix seconds; the UI turns it into local time.
    pub at: i64,
    /// Empty for somebody who left the chat: their words stay, they do not.
    pub name: String,
    pub body: String,
    /// The message this one answers, or empty. Its author and first words come
    /// along, because a client only asks for what is new and cannot look up a
    /// message it never held.
    pub reply_name: String,
    pub reply_text: String,
    pub edited: bool,
    pub mine: bool,
    pub can_edit: bool,
}

/// One answer from the administrator on a problem report.
pub struct BridgeChatAnswer {
    pub id: i64,
    pub at: i64,
    pub body: String,
}

/// The roger beep, as the phone sets it.
///
/// A mirror of `sdr_remote_logic::roger::RogerBeep` rather than a re-export,
/// because UniFFI needs the type declared in this crate. The values are handed
/// straight over and clamped there, so a phone cannot ask for something the
/// desktop would refuse.
pub struct BridgeRogerBeep {
    pub freq_hz: f32,
    pub volume: f32,
    pub duration_ms: u32,
    pub include_fm: bool,
    pub on_thetis: bool,
    pub on_radio1: bool,
    pub on_radio2: bool,
}

/// The chat as one screen's worth of state.
pub struct BridgeChatState {
    /// 0 = reachable, 1 = no relay configured, 2 = a relay without a chat,
    /// 3 = nothing answering. Three reasons that need three different things
    /// from the user; one word covering all of them sends people to the
    /// maker's mailbox.
    pub offline_reason: u8,
    /// The service has said whether this station is in the chat. Until it has,
    /// neither the consent screen nor the conversation is shown - guessing
    /// wrong means showing somebody a consent form they already filled in.
    pub consent_known: bool,
    pub consented: bool,
    pub display_name: String,
    pub unread: u32,
    /// The service's own words for a refusal, or empty.
    pub error: String,
    /// How many problem reports this station may still send today; -1 when the
    /// service has not said. Known before the form is filled in, because being
    /// told at the send button is being told after the work.
    pub reports_left: i64,
    pub messages: Vec<BridgeChatMessage>,
    pub answers: Vec<BridgeChatAnswer>,
}

/// Radio state exposed to Kotlin via uniffi.
/// 1:1 mapping with RadioState fields.
pub struct BridgeRadioState {
    pub connected: bool,
    /// Goes up on every new session on this link (`RadioState::session_generation`).
    /// The phone lets go of a held PTT on that edge, as it does when the link
    /// dies: the engine let go of its flags at the authentication (build 60).
    pub session_generation: u64,
    /// The transport this engine runs on, as the screen shows it
    /// (`Route::label`), so the settings show what is in use - and compare it
    /// with `expected_route_label`, the same word from the same table.
    pub route: String,
    pub ptt_denied: bool,
    /// What blocks the station and what helps, one line each, in the app's
    /// language (`sdr_remote_logic::station_notice`). Filled in
    /// `bridge_state_from_radio_state`, which knows the language.
    pub station_notices: Vec<String>,
    /// Whether Connect may be offered (`station_notice::can_offer_connect`).
    pub can_connect: bool,
    /// That refusal was the server letting go, not another client holding on.
    ///
    /// The radio stopped or its time-out timer is about to fire. A held control
    /// has to be released before it may ask again; an ordinary refusal does not
    /// carry that. See `Flags::SERVER_RELEASED`.
    pub ptt_released_by_server: bool,
    pub audio_error: bool,
    pub rtt_ms: u16,
    pub jitter_ms: f32,
    pub buffer_depth: u32,
    pub rx_packets: u64,
    pub yaesu_audio_packets: u64,
    pub yaesu_jitter_ms: f32,
    pub yaesu_buffer_depth: u32,
    pub yaesu2_audio_packets: u64,
    pub yaesu2_jitter_ms: f32,
    pub yaesu2_buffer_depth: u32,
    pub vrx1_audio_packets: u64,
    pub vrx1_jitter_ms: f32,
    pub vrx1_buffer_depth: u32,
    pub vrx2_audio_packets: u64,
    pub vrx2_jitter_ms: f32,
    pub vrx2_buffer_depth: u32,
    pub loss_percent: u8,
    pub down_kbps: u32,
    pub up_kbps: u32,
    pub dx_spots_enabled: bool,
    /// Does this server have a DX cluster at all? False only when the server
    /// says so, so an older server that cannot say still counts as having one.
    /// The toggle is hidden without one: a switch that promises a stream the
    /// server can never send is worse than no switch.
    pub dx_cluster_available: bool,
    /// How each Yaesu slot is named on screen: "Yaesu 1: FTX1" once the
    /// server has said what it is, plain "Yaesu 1" until then. Composed here
    /// from the shared rule rather than in Compose, so the phone and the
    /// desktop cannot end up naming the same radio differently.
    pub yaesu_label: String,
    pub yaesu2_label: String,
    pub capture_level: f32,
    /// TX level of the Yaesu chain - measured on the frame as encoded, so EQ,
    /// compressor and AGC are included. One field for both radios: only the
    /// selected one transmits. Without this the Yaesu panel showed the THETIS
    /// capture level, which sits at silence while a Yaesu is being keyed.
    pub yaesu_mic_level: f32,
    pub playback_level: f32,
    pub frequency_hz: u64,
    pub frequency_rx2_hz: u64,
    pub mode: u8,
    /// S-meter — dBm in RX, watts in TX (use `other_tx` / client PTT to
    /// disambiguate, same as the desktop client and wire protocol).
    pub smeter: f32,
    pub power_on: bool,
    pub tx_profile: u8,
    pub nr_level: u8,
    pub anf_on: bool,
    pub nb_level: u8,
    pub diversity_enabled: bool,
    pub diversity_phase: f32,
    pub diversity_gain_rx1: f32,
    pub diversity_gain_rx2: f32,
    pub diversity_ref: u8,
    pub diversity_source: u8,
    pub diversity_autonull_result: u16,
    pub drive_level: u8,
    pub rx_af_gain: u8,
    pub agc_enabled: bool,
    /// The server is not running this client's build.
    ///
    /// Flattened from the shared `PeerBuild` rather than mirrored as an enum:
    /// the judgement stays in one place (core), and what crosses is only what
    /// a screen needs to draw. Nothing on Android refuses to work over this.
    pub server_build_differs: bool,
    /// What the server calls itself. Empty when it agrees, and also when it
    /// is a build too old to say which - the flag above is what separates
    /// "nothing to report" from "could not say".
    pub server_build: String,
    /// The measurement file being written (open point 34), `None` when off.
    pub measurement_file: Option<String>,
    /// "Relay only" as last switched (build 137, `RadioState::relay_only`).
    pub relay_only: bool,
    /// "Force TCP" as last switched (build 146, `RadioState::force_tcp`).
    pub force_tcp: bool,
    /// The connection runs through the relay, so "relay only" means something
    /// (build 137). From `Route` here, not from the label in Kotlin, so a
    /// renamed label cannot leave the switch grey.
    pub via_relay: bool,
    /// The connection name (build 139): the letters, the long name in the
    /// app's language, and whether it is the coupling wait (drawn amber). All
    /// empty or false while there is no session. The words come from
    /// `sdr_remote_logic::path_name`, so the phone keeps no table of its own.
    pub path_short: String,
    pub path_name: String,
    pub path_hint: String,
    pub path_coupling: bool,
    pub other_tx: bool,
    /// The Thetis meter carries watts now - what the meter chooses its scale
    /// on, not the phone's own PTT (`RadioState::smeter_tx`, build 169).
    pub smeter_tx: bool,
    /// The spectrum's automatic reference pauses now
    /// (`RadioState::auto_ref_paused`, build 176).
    pub auto_ref_paused: bool,
    pub filter_low_hz: i32,
    pub filter_high_hz: i32,
    pub thetis_configured: bool,
    pub thetis_starting: bool,
    /// Server explicitly reports Thetis.exe is not running on the server PC.
    /// Drives the Thetis-autostart option; false when the server says nothing
    /// about the process (old server) - see `ConnectStatus::thetis_reported_not_running`.
    pub thetis_not_running: bool,
    pub tx_profile_names: Vec<String>,
    // Spectrum (extracted view)
    pub spectrum_bins: Vec<u8>,
    pub spectrum_center_hz: u32,
    pub spectrum_span_hz: u32,
    pub spectrum_ref_level: i8,
    pub spectrum_db_per_unit: u8,
    pub spectrum_sequence: u16,
    // Full DDC spectrum (for waterfall)
    pub full_spectrum_bins: Vec<u8>,
    pub full_spectrum_center_hz: u32,
    pub full_spectrum_span_hz: u32,
    pub full_spectrum_sequence: u16,
    // External equipment
    pub amplitec_connected: bool,
    pub amplitec_switch_a: u8,
    pub amplitec_switch_b: u8,
    pub amplitec_labels: String,
    // Tuner
    pub tuner_connected: bool,
    pub tuner_state: u8,
    pub tuner_can_tune: bool,
    // SPE Expert
    pub spe_connected: bool,
    pub spe_state: u8,
    pub spe_band: u8,
    pub spe_ptt: bool,
    pub spe_power_w: u16,
    pub spe_swr_x10: u16,
    pub spe_temp: u8,
    pub spe_warning: u8,
    pub spe_alarm: u8,
    pub spe_power_level: u8,
    pub spe_antenna: u8,
    pub spe_input: u8,
    pub spe_voltage_x10: u16,
    pub spe_current_x10: u16,
    pub spe_atu_bypassed: bool,
    pub spe_available: bool,
    pub spe_active: bool,
    // RF2K-S Amplifier
    pub rf2k_connected: bool,
    pub rf2k_operate: bool,
    pub rf2k_band: u8,
    pub rf2k_frequency_khz: u16,
    pub rf2k_temperature_x10: u16,
    pub rf2k_voltage_x10: u16,
    pub rf2k_current_x10: u16,
    pub rf2k_forward_w: u16,
    pub rf2k_reflected_w: u16,
    pub rf2k_swr_x100: u16,
    pub rf2k_max_forward_w: u16,
    pub rf2k_max_reflected_w: u16,
    pub rf2k_max_swr_x100: u16,
    pub rf2k_error_state: u8,
    pub rf2k_error_text: String,
    pub rf2k_antenna_type: u8,
    pub rf2k_antenna_number: u8,
    pub rf2k_tuner_mode: u8,
    pub rf2k_tuner_setup: String,
    pub rf2k_tuner_l_nh: u16,
    pub rf2k_tuner_c_pf: u16,
    pub rf2k_drive_w: u16,
    pub rf2k_modulation: String,
    pub rf2k_max_power_w: u16,
    pub rf2k_device_name: String,
    pub rf2k_available: bool,
    pub rf2k_active: bool,
    // Yaesu FT-991A
    pub yaesu_connected: bool,
    pub yaesu_freq_a: u64,
    pub yaesu_freq_b: u64,
    pub yaesu_mode: u8,
    pub yaesu_smeter: u16,
    pub yaesu_tx_active: bool,
    /// This radio is held by a different client (server's ownership table).
    pub yaesu_held_by_other: bool,
    pub yaesu2_held_by_other: bool,
    pub yaesu_power_on: bool,
    pub yaesu_af_gain: u8,
    pub yaesu_tx_power: u8,
    pub yaesu_squelch: u8,
    pub yaesu_rf_gain: u8,
    pub yaesu_mic_gain: u8,
    pub yaesu_vfo_select: u8,
    pub yaesu_memory_channel: u16,
    pub yaesu_split: bool,
    pub yaesu_scan: bool,
    pub playback_level_yaesu: f32,
    pub yaesu_memory_data: String,
    /// EX/menu values, in their OWN field. They used to share the memory field,
    /// told apart by a "MENU:" prefix - which worked only while the two never
    /// arrived together. Since both are pushed on connect they do, and whichever
    /// came second hid the other: the memory list appeared for a moment and then
    /// vanished behind the EX list.
    pub yaesu_menu_data: String,
    pub yaesu_model: u8,
    pub yaesu_tuner_state: u8,
    /// The radio reports high SWR during TX (clears itself).
    pub yaesu_hi_swr: bool,
    pub pa_drive_warning: bool,
    /// Maximum TX power for the current band (from the EX max-power menus; 0 = unknown).
    /// The slider clamps to this, like the desktop.
    pub yaesu_tx_power_max: u8,
    // Radio 2 (yaesu2_*) - Android shows one radio at a time; the selector picks which.
    // The selector only shows a radio when it is connected (= configured and active).
    pub yaesu2_connected: bool,
    pub yaesu2_model: u8,
    pub yaesu2_tuner_state: u8,
    pub yaesu2_hi_swr: bool,
    pub yaesu2_tx_power_max: u8,
    pub yaesu2_freq_a: u64,
    pub yaesu2_freq_b: u64,
    pub yaesu2_mode: u8,
    pub yaesu2_smeter: u16,
    pub yaesu2_tx_active: bool,
    pub yaesu2_power_on: bool,
    pub yaesu2_af_gain: u8,
    pub yaesu2_tx_power: u8,
    pub yaesu2_squelch: u8,
    pub yaesu2_rf_gain: u8,
    pub yaesu2_mic_gain: u8,
    pub yaesu2_vfo_select: u8,
    pub yaesu2_memory_channel: u16,
    pub yaesu2_split: bool,
    pub yaesu2_scan: bool,
    pub playback_level_yaesu2: f32,
    pub yaesu2_memory_data: String,
    pub yaesu2_menu_data: String,
    // DSP/function feature state (both slots): toggles bitfield, levels, freqs.
    pub yaesu_feature_toggles: u32,
    pub yaesu_feature_levels: Vec<u8>,
    pub yaesu_feature_freqs: Vec<u16>,
    pub yaesu2_feature_toggles: u32,
    pub yaesu2_feature_levels: Vec<u8>,
    pub yaesu2_feature_freqs: Vec<u16>,
    // UltraBeam RCU-06
    pub ub_connected: bool,
    pub ub_frequency_khz: u16,
    pub ub_band: u8,
    pub ub_direction: u8,
    pub ub_off_state: bool,
    pub ub_motors_moving: u8,
    pub ub_motor_completion: u16,
    pub ub_fw_major: u8,
    pub ub_fw_minor: u8,
    pub ub_available: bool,
    pub ub_elements_mm: Vec<u16>,
    // Rotor
    pub rotor_connected: bool,
    pub rotor_angle_x10: u16,
    pub rotor_rotating: bool,
    pub rotor_target_x10: u16,
    pub rotor_available: bool,
    // DX Cluster spots
    pub dx_spots: Vec<BridgeDxSpot>,
    // Auth (legacy bools — Compose UI should prefer connect_status_* fields below)
    pub auth_rejected: bool,
    pub totp_required: bool,
    // PATCH-1: connect-status as pre-rendered text. Single source of truth in
    // sdr-remote-logic::i18n (NL+EN). Compose UI just renders these strings.
    pub connect_status_headline: String,
    pub connect_status_action: String, // empty if no action hint
    pub connect_status_is_error: bool,
    pub connect_status_is_awaiting_totp: bool,
}

/// PATCH-1: build a BridgeRadioState with connect-status text rendered in the
/// caller-specified language. `From<RadioState>` defaults to English for
/// compatibility; call sites that know the user's language preference should
/// use this helper.
pub fn bridge_state_from_radio_state(
    s: RadioState,
    lang: sdr_remote_logic::i18n::Lang,
) -> BridgeRadioState {
    let (headline, action) = sdr_remote_logic::i18n::connect_status_text(
        &s.connect_status,
        lang,
        sdr_remote_logic::i18n::Platform::Mobile,
    );
    let station_notices = sdr_remote_logic::station_notice::station_notice_lines(&s, lang);
    let path_name = s.path.map(|k| sdr_remote_logic::path_name::long_name(k, lang)).unwrap_or_default();
    let path_hint = s.path.map(|k| sdr_remote_logic::path_name::hint(k, lang)).unwrap_or_default();
    BridgeRadioState {
        station_notices,
        path_name,
        path_hint,
        connect_status_headline: headline,
        connect_status_action: action.unwrap_or_default(),
        connect_status_is_error: matches!(
            s.connect_status,
            sdr_remote_logic::state::ConnectStatus::Failed(_)
        ),
        connect_status_is_awaiting_totp: matches!(
            s.connect_status,
            sdr_remote_logic::state::ConnectStatus::AwaitingTotp
        ),
        ..BridgeRadioState::from(s)
    }
}

impl From<RadioState> for BridgeRadioState {
    fn from(s: RadioState) -> Self {
        Self {
            connected: s.connected,
            session_generation: s.session_generation,
            route: s.route.label().to_string(),

            // Derived from the refusal record rather than carried as flags.
            // Android never had the fault the counter is for - it has no
            // per-frame Thetis PTT-off wiping an unrelated radio's refusal -
            // so it still reads levels, and the counter comes over with the
            // protocol patch that gives a refusal a slot number.
            //
            // It does read them at delay(33), a window twice as wide as the
            // desktop's, so "it cannot happen there" is a statement about
            // today's code and not a guarantee (review).
            //
            // released_by_server_now() rather than the bare field: the reason
            // belongs to the event and is not taken back when the refusal is
            // settled, so a level reader has to AND the two. Writing that out
            // here would have put the rule in one front end again.
            ptt_denied: s.ptt_denial.active(),
            station_notices: sdr_remote_logic::station_notice::station_notice_lines(
                &s,
                sdr_remote_logic::i18n::Lang::En,
            ),
            can_connect: sdr_remote_logic::station_notice::can_offer_connect(&s),
            ptt_released_by_server: s.ptt_denial.released_by_server_now(),
            audio_error: s.audio_error,
            rtt_ms: s.rtt_ms,
            jitter_ms: s.jitter_ms,
            buffer_depth: s.buffer_depth,
            rx_packets: s.rx_packets,
            yaesu_audio_packets: s.yaesu_audio_packets,
            yaesu_jitter_ms: s.yaesu_jitter_ms,
            yaesu_buffer_depth: s.yaesu_buffer_depth,
            yaesu2_audio_packets: s.yaesu2_audio_packets,
            yaesu2_jitter_ms: s.yaesu2_jitter_ms,
            yaesu2_buffer_depth: s.yaesu2_buffer_depth,
            vrx1_audio_packets: s.vrx1_audio_packets,
            vrx1_jitter_ms: s.vrx1_jitter_ms,
            vrx1_buffer_depth: s.vrx1_buffer_depth,
            vrx2_audio_packets: s.vrx2_audio_packets,
            vrx2_jitter_ms: s.vrx2_jitter_ms,
            vrx2_buffer_depth: s.vrx2_buffer_depth,
            loss_percent: s.loss_percent,
            down_kbps: s.down_kbps,
            up_kbps: s.up_kbps,
            dx_spots_enabled: s.dx_spots_enabled,
            dx_cluster_available: s.dx_cluster_available,
            yaesu_label: sdr_remote_core::protocol::radio_slot_label(0, s.yaesu_model),
            yaesu2_label: sdr_remote_core::protocol::radio_slot_label(1, s.yaesu2_model),
            capture_level: s.capture_level,
            yaesu_mic_level: s.yaesu_mic_level,
            playback_level: s.playback_level,
            frequency_hz: s.frequency_hz,
            frequency_rx2_hz: s.frequency_rx2_hz,
            mode: s.mode,
            smeter: s.smeter,
            power_on: s.power_on,
            tx_profile: s.tx_profile,
            nr_level: s.nr_level,
            anf_on: s.anf_on,
            nb_level: s.nb_level,
            diversity_enabled: s.diversity_enabled,
            diversity_phase: (s.diversity_phase as i32 - 18000) as f32 / 100.0,
            diversity_gain_rx1: s.diversity_gain_rx1 as f32 / 1000.0,
            diversity_gain_rx2: s.diversity_gain_rx2 as f32 / 1000.0,
            diversity_ref: s.diversity_ref,
            diversity_source: s.diversity_source,
            diversity_autonull_result: s.diversity_autonull_result,
            drive_level: s.drive_level,
            rx_af_gain: s.rx_af_gain,
            agc_enabled: s.agc_enabled,
            server_build_differs: s.server_build.is_trouble(),
            server_build: s.server_build.theirs().unwrap_or_default().to_string(),
            measurement_file: s.measurement_file.clone(),
            relay_only: s.relay_only,
            force_tcp: s.force_tcp,
            via_relay: s.route == sdr_remote_logic::measure::Route::QuicRelay,
            path_short: s.path.map(|k| k.short().to_string()).unwrap_or_default(),
            // English here; `bridge_state_from_radio_state` puts them in the
            // app's language, as it does the station notices.
            path_name: s.path.map(|k| sdr_remote_logic::path_name::long_name(k, sdr_remote_logic::i18n::Lang::En)).unwrap_or_default(),
            path_hint: s.path.map(|k| sdr_remote_logic::path_name::hint(k, sdr_remote_logic::i18n::Lang::En)).unwrap_or_default(),
            path_coupling: s.path == Some(sdr_remote_core::path::PathKind::RelayCoupling),
            other_tx: s.other_tx,
            smeter_tx: s.smeter_tx,
            auto_ref_paused: s.auto_ref_paused,
            filter_low_hz: s.filter_low_hz,
            filter_high_hz: s.filter_high_hz,
            thetis_configured: s.thetis_configured,
            thetis_starting: s.thetis_starting,
            thetis_not_running: s.connect_status.thetis_reported_not_running(),
            tx_profile_names: s.tx_profile_names,
            spectrum_bins: s.spectrum_bins.iter().map(|v| (v >> 8) as u8).collect(),
            spectrum_center_hz: s.spectrum_center_hz,
            spectrum_span_hz: s.spectrum_span_hz,
            spectrum_ref_level: s.spectrum_ref_level,
            spectrum_db_per_unit: s.spectrum_db_per_unit,
            spectrum_sequence: s.spectrum_sequence,
            full_spectrum_bins: s.full_spectrum_bins.iter().map(|v| (v >> 8) as u8).collect(),
            full_spectrum_center_hz: s.full_spectrum_center_hz,
            full_spectrum_span_hz: s.full_spectrum_span_hz,
            full_spectrum_sequence: s.full_spectrum_sequence,
            amplitec_connected: s.amplitec_connected,
            amplitec_switch_a: s.amplitec_switch_a,
            amplitec_switch_b: s.amplitec_switch_b,
            amplitec_labels: s.amplitec_labels,
            tuner_connected: s.tuner_connected,
            tuner_state: s.tuner_state,
            tuner_can_tune: s.tuner_can_tune,
            spe_connected: s.spe_connected,
            spe_state: s.spe_state,
            spe_band: s.spe_band,
            spe_ptt: s.spe_ptt,
            spe_power_w: s.spe_power_w,
            spe_swr_x10: s.spe_swr_x10,
            spe_temp: s.spe_temp,
            spe_warning: s.spe_warning,
            spe_alarm: s.spe_alarm,
            spe_power_level: s.spe_power_level,
            spe_antenna: s.spe_antenna,
            spe_input: s.spe_input,
            spe_voltage_x10: s.spe_voltage_x10,
            spe_current_x10: s.spe_current_x10,
            spe_atu_bypassed: s.spe_atu_bypassed,
            spe_available: s.spe_available,
            spe_active: s.spe_active,
            rf2k_connected: s.rf2k_connected,
            rf2k_operate: s.rf2k_operate,
            rf2k_band: s.rf2k_band,
            rf2k_frequency_khz: s.rf2k_frequency_khz,
            rf2k_temperature_x10: s.rf2k_temperature_x10,
            rf2k_voltage_x10: s.rf2k_voltage_x10,
            rf2k_current_x10: s.rf2k_current_x10,
            rf2k_forward_w: s.rf2k_forward_w,
            rf2k_reflected_w: s.rf2k_reflected_w,
            rf2k_swr_x100: s.rf2k_swr_x100,
            rf2k_max_forward_w: s.rf2k_max_forward_w,
            rf2k_max_reflected_w: s.rf2k_max_reflected_w,
            rf2k_max_swr_x100: s.rf2k_max_swr_x100,
            rf2k_error_state: s.rf2k_error_state,
            rf2k_error_text: s.rf2k_error_text,
            rf2k_antenna_type: s.rf2k_antenna_type,
            rf2k_antenna_number: s.rf2k_antenna_number,
            rf2k_tuner_mode: s.rf2k_tuner_mode,
            rf2k_tuner_setup: s.rf2k_tuner_setup,
            rf2k_tuner_l_nh: s.rf2k_tuner_l_nh,
            rf2k_tuner_c_pf: s.rf2k_tuner_c_pf,
            rf2k_drive_w: s.rf2k_drive_w,
            rf2k_modulation: s.rf2k_modulation,
            rf2k_max_power_w: s.rf2k_max_power_w,
            rf2k_device_name: s.rf2k_device_name,
            rf2k_available: s.rf2k_available,
            rf2k_active: s.rf2k_active,
            yaesu_connected: s.yaesu_connected,
            yaesu_freq_a: s.yaesu_freq_a,
            yaesu_freq_b: s.yaesu_freq_b,
            yaesu_mode: s.yaesu_mode,
            yaesu_smeter: s.yaesu_smeter,
            yaesu_tx_active: s.yaesu_tx_active,
            yaesu_held_by_other: s.yaesu_held_by_other,
            yaesu2_held_by_other: s.yaesu2_held_by_other,
            yaesu_power_on: s.yaesu_power_on,
            yaesu_af_gain: s.yaesu_af_gain,
            yaesu_tx_power: s.yaesu_tx_power,
            yaesu_squelch: s.yaesu_squelch,
            yaesu_rf_gain: s.yaesu_rf_gain,
            yaesu_mic_gain: s.yaesu_mic_gain,
            yaesu_vfo_select: s.yaesu_vfo_select,
            yaesu_memory_channel: s.yaesu_memory_channel,
            yaesu_split: s.yaesu_split,
            yaesu_scan: s.yaesu_scan,
            playback_level_yaesu: s.playback_level_yaesu,
            yaesu_memory_data: s.yaesu_memory_data.clone().unwrap_or_default(),
            yaesu_menu_data: s.yaesu_menu_data.clone().unwrap_or_default(),
            yaesu_model: s.yaesu_model,
            yaesu_tuner_state: s.yaesu_tuner_state,
            yaesu_hi_swr: s.yaesu_hi_swr,
            pa_drive_warning: s.pa_drive_warning,
            yaesu_tx_power_max: s.yaesu_tx_power_max,
            yaesu2_connected: s.yaesu2_connected,
            yaesu2_model: s.yaesu2_model,
            yaesu2_tuner_state: s.yaesu2_tuner_state,
            yaesu2_hi_swr: s.yaesu2_hi_swr,
            yaesu2_tx_power_max: s.yaesu2_tx_power_max,
            yaesu2_freq_a: s.yaesu2_freq_a,
            yaesu2_freq_b: s.yaesu2_freq_b,
            yaesu2_mode: s.yaesu2_mode,
            yaesu2_smeter: s.yaesu2_smeter,
            yaesu2_tx_active: s.yaesu2_tx_active,
            yaesu2_power_on: s.yaesu2_power_on,
            yaesu2_af_gain: s.yaesu2_af_gain,
            yaesu2_tx_power: s.yaesu2_tx_power,
            yaesu2_squelch: s.yaesu2_squelch,
            yaesu2_rf_gain: s.yaesu2_rf_gain,
            yaesu2_mic_gain: s.yaesu2_mic_gain,
            yaesu2_vfo_select: s.yaesu2_vfo_select,
            yaesu2_memory_channel: s.yaesu2_memory_channel,
            yaesu2_split: s.yaesu2_split,
            yaesu2_scan: s.yaesu2_scan,
            playback_level_yaesu2: s.playback_level_yaesu2,
            yaesu2_memory_data: s.yaesu2_memory_data.clone().unwrap_or_default(),
            yaesu2_menu_data: s.yaesu2_menu_data.clone().unwrap_or_default(),
            yaesu_feature_toggles: s.yaesu_feature_toggles,
            yaesu_feature_levels: s.yaesu_feature_levels.to_vec(),
            yaesu_feature_freqs: s.yaesu_feature_freqs.to_vec(),
            yaesu2_feature_toggles: s.yaesu2_feature_toggles,
            yaesu2_feature_levels: s.yaesu2_feature_levels.to_vec(),
            yaesu2_feature_freqs: s.yaesu2_feature_freqs.to_vec(),
            ub_connected: s.ub_connected,
            ub_frequency_khz: s.ub_frequency_khz,
            ub_band: s.ub_band,
            ub_direction: s.ub_direction,
            ub_off_state: s.ub_off_state,
            ub_motors_moving: s.ub_motors_moving,
            ub_motor_completion: s.ub_motor_completion,
            ub_fw_major: s.ub_fw_major,
            ub_fw_minor: s.ub_fw_minor,
            ub_available: s.ub_available,
            ub_elements_mm: s.ub_elements_mm.to_vec(),
            rotor_connected: s.rotor_connected,
            rotor_angle_x10: s.rotor_angle_x10,
            rotor_rotating: s.rotor_rotating,
            rotor_target_x10: s.rotor_target_x10,
            rotor_available: s.rotor_available,
            dx_spots: s.dx_spots.iter().map(|spot| {
                let total_age = spot.age_seconds as u64
                    + spot.received.elapsed().as_secs().min(u16::MAX as u64);
                BridgeDxSpot {
                    callsign: spot.callsign.clone(),
                    frequency_hz: spot.frequency_hz,
                    mode: spot.mode.clone(),
                    spotter: spot.spotter.clone(),
                    comment: spot.comment.clone(),
                    age_seconds: (total_age as u16).min(spot.expiry_seconds),
                    expiry_seconds: spot.expiry_seconds,
                }
            }).collect(),
            auth_rejected: s.auth_rejected,
            totp_required: s.totp_required,
            // PATCH-1: render connect_status text once in Rust so Android Compose
            // UI is automatically lockstep with desktop egui UI on NL/EN strings.
            // TODO(PATCH-1 follow-up): expose Lang as user config + OS-locale detect.
            connect_status_headline: {
                let (h, _) = sdr_remote_logic::i18n::connect_status_text(
                    &s.connect_status,
                    sdr_remote_logic::i18n::Lang::En,
                    sdr_remote_logic::i18n::Platform::Mobile,
                );
                h
            },
            connect_status_action: {
                let (_, a) = sdr_remote_logic::i18n::connect_status_text(
                    &s.connect_status,
                    sdr_remote_logic::i18n::Lang::En,
                    sdr_remote_logic::i18n::Platform::Mobile,
                );
                a.unwrap_or_default()
            },
            connect_status_is_error: matches!(
                s.connect_status,
                sdr_remote_logic::state::ConnectStatus::Failed(_)
            ),
            connect_status_is_awaiting_totp: matches!(
                s.connect_status,
                sdr_remote_logic::state::ConnectStatus::AwaitingTotp
            ),
        }
    }
}

/// Platform-specific audio factory.
/// On Android: creates OboeAudioBackend.
/// On other platforms: returns error (for cargo check only).
#[cfg(target_os = "android")]
fn make_audio(
    _input: Option<&str>,
    _output: Option<&str>,
) -> anyhow::Result<Box<dyn AudioBackend>> {
    let audio = crate::audio_oboe::OboeAudioBackend::new()?;
    Ok(Box::new(audio))
}

#[cfg(not(target_os = "android"))]
fn make_audio(
    _input: Option<&str>,
    _output: Option<&str>,
) -> anyhow::Result<Box<dyn AudioBackend>> {
    anyhow::bail!("Audio not available on this platform (Android only)")
}

/// Bridge between Kotlin/uniffi and the Rust ClientEngine.
/// Wraps engine lifecycle, command forwarding, and state polling.
/// The stop handle of the last relay monitor started in this process, so a new
/// one can put the old one down. See the comment where it is set.
static PREVIOUS_MONITOR: Mutex<Option<sdr_remote_relay::RelayStopHandle>> = Mutex::new(None);

pub struct SdrBridge {
    cmd_tx: mpsc::UnboundedSender<Command>,
    state_rx: Mutex<watch::Receiver<RadioState>>,
    shutdown_tx: Mutex<Option<watch::Sender<bool>>>,
    /// UI language for connect-status / connect-error rendering in `get_state()`.
    /// Set via `set_language`, which the app calls once at startup with the language
    /// Android resolved `strings.xml` to, so this side cannot drift from that one.
    /// Defaults to "en" for the window before that call.
    ui_language: Mutex<String>,
    /// Follows the measurement file in the state, to copy the log when one ends.
    measurement_log: Mutex<crate::logging::MeasurementLogFollower>,
    /// Phase C: keeps the relay monitor alive for as long as the bridge exists (it runs
    /// on a thread of its own), and passes the phone's network events on to it.
    /// `None` in direct mode.
    relay_monitor: Mutex<Option<sdr_remote_relay::RelayMonitor>>,
    /// Phase 3c: relay status handle to surface the transport (UDP / wss-fallback) to the
    /// Compose UI. `None` in direct mode.
    relay_status: Option<sdr_remote_relay::RelayStatusHandle>,
    /// The relay address this bridge was built with, exactly as configured -
    /// including when the relay is switched off. Used for redaction: the one host
    /// a problem report must never carry is the one that is written down, whether
    /// or not it is in use.
    relay_url: String,
    /// The relay address the chat may work with: the same one, but empty unless
    /// the relay is actually running this session.
    ///
    /// The two are separate because an address that is merely written down is not
    /// a relay. Deriving the chat endpoint from `relay_url` meant that a phone
    /// with the relay switched off but an address still in its settings was told
    /// "this relay offers no chat" - blaming a relay it was not talking to -
    /// where the honest answer is that no relay is configured.
    chat_relay_url: String,
    /// The chat, exactly as the desktop holds it. Compose asks for its state on
    /// a timer and calls the same handful of verbs; the worker thread inside
    /// does the network, so nothing here can get between an operator and PTT.
    chat: Mutex<sdr_remote_chat::ChatModel>,
}

impl SdrBridge {
    pub fn new(
        relay_enabled: bool,
        relay_url: String,
        relay_station: String,
        relay_token: String,
        relay_instance: String,
        relay_name: String,
    ) -> Self {
        // No logger here: the app installs its one at start (`init_logging`,
        // before any bridge exists), and that one applies the developer
        // mode's filter. A second install here could only win if that start
        // had failed, and then without the filter (build 298).

        let (engine, state_rx, cmd_tx) = ClientEngine::new();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        // The relay, for phones that cannot be reached straight (mobile behind
        // CGNAT): when its settings are complete, a client relay monitor signs in
        // and hands its grants to the engine's QUIC client (phase 3b). Otherwise
        // QUIC straight to the address typed.
        let mut relay_monitor: Option<sdr_remote_relay::RelayMonitor> = None;
        // One condition, read twice: it decides both whether the relay is used
        // and whether the chat has a relay to reach. Splitting them is how the
        // chat came to blame a relay that was switched off.
        let relay_active =
            sdr_remote_relay::is_configured(relay_enabled, &relay_url, &relay_station, &relay_token);
        // `relay_active` is the one rule the settings' "in use" line compares
        // against (`expected_route_label`, `Route::asked_for`).
        let mut relay_quic_grants = None;
        let mut relay_tcp_line = None;
        if relay_active {
            let relay_cfg = sdr_remote_relay::RelayConfig {
                enabled: true,
                url: relay_url.clone(),
                station: relay_station,
                token: relay_token,
                role: sdr_remote_relay::RelayRole::Client,
                instance: relay_instance,
                name: relay_name,
                // QUIC through the relay always needs the relay's UDP port.
                udp_port: sdr_remote_relay::relay_udp_port_resolve(true),
                // Set by the constructor below: `start_threaded_quic` makes it true.
                quic: false,
            };
            let (grant_tx, grant_rx) = watch::channel(None);
            // The TCP line for a network that lets no UDP through (build 144).
            let (tcp, tcp_monitor) = sdr_remote_relay::tcp_line();
            relay_quic_grants = Some(grant_rx);
            relay_tcp_line = Some(tcp);
            let monitor = sdr_remote_relay::RelayMonitor::start_threaded_quic(relay_cfg, grant_tx, Some(tcp_monitor));
            // Only one relay monitor may live in this process. Android can build
            // a second bridge while the first is still running - the phone was
            // seen with two, three seconds apart, after switching network - and
            // both carry the same install id. The relay hands a returning client
            // its own slot back and closes the older connection, so two live
            // monitors evict each other every five seconds for as long as the
            // app runs. Control traffic squeezes through the gaps, which is why
            // the S-meter kept working while audio and spectrum never got a
            // stable path, and why only killing the app helped: that took both
            // monitors with it (2026-08-17).
            //
            // Stopping the older one here rather than trusting the owner of it
            // to do so: whoever forgets, the invariant holds.
            // What the slot holds here may be the handle of a monitor that has
            // already stopped - it was, on every restart, until build 13 made a
            // clean shutdown empty the slot. Saying "still running" regardless made
            // this warning untrue twice in one field log (2026-09-24), 0.9 s after
            // "Relay monitor stopped" two lines above it - and a warning that is
            // untrue every ordinary time is one nobody reads when the real case
            // returns. So it asks first. `stop()` is still unconditional: the safety
            // net does not depend on the answer, only the sentence does.
            if let Some(previous) = PREVIOUS_MONITOR.lock().unwrap().replace(monitor.stop_handle()) {
                if !previous.is_stopped() {
                    warn!("a relay monitor was still running - stopping it, or the two would evict each other");
                }
                previous.stop();
            }
            relay_monitor = Some(monitor);
        }
        // Which transport is logged by the engine, once, for both front ends
        // (review 133). Never the relay's address: it is the one thing a report
        // must never carry (2026-08-17).
        let engine = engine.with_quic_relay(relay_quic_grants, relay_tcp_line);

        // Through the reporter, so a panic leaves "no connection" on screen
        // instead of the last state; nothing caught one here before.
        let engine_stop = engine.stop_reporter();
        std::thread::spawn(move || {
            engine_stop.run(|| {
                let rt = tokio::runtime::Runtime::new().expect("create tokio runtime");
                // The result goes to the reporter: an error is a stopped engine,
                // like a panic, and it says so on screen.
                rt.block_on(async { engine.run(make_audio, shutdown_rx).await })
            });
            info!("Engine thread exited");
        });

        // Capture the status handle before the monitor is moved into the struct.
        let relay_status = relay_monitor.as_ref().map(|m| m.status_handle());
        Self {
            cmd_tx,
            state_rx: Mutex::new(state_rx),
            shutdown_tx: Mutex::new(Some(shutdown_tx)),
            ui_language: Mutex::new("en".to_string()),
            measurement_log: Mutex::new(crate::logging::MeasurementLogFollower::default()),
            relay_monitor: Mutex::new(relay_monitor),
            relay_status,
            chat_relay_url: if relay_active {
                relay_url.clone()
            } else {
                String::new()
            },
            relay_url,
            chat: Mutex::new(sdr_remote_chat::ChatModel::default()),
        }
    }

    /// Set the UI language used for connect-status / connect-error text in
    /// `get_state()`. Accepts any code `Lang::from_code` knows - "en", "nl",
    /// "de", "fr" - and stores English for anything else. Compose should call
    /// this once on startup with the language it is itself rendering in.
    ///
    /// It used to keep only "nl" and flatten everything else to "en", so a
    /// German or French phone kept getting English connect text even once
    /// those translations existed (2026-08-20).
    ///
    /// The app calls it once at startup with the language Android resolved
    /// `strings.xml` to, so this side cannot drift from the strings around it.
    pub fn set_language(&self, lang: String) {
        *self.ui_language.lock().unwrap() = sdr_remote_logic::i18n::Lang::from_code(&lang)
            .code()
            .to_string();
    }

    pub fn connect(&self, addr: String, password: String) {
        let pw = if password.is_empty() { None } else { Some(password) };
        let _ = self.cmd_tx.send(Command::Connect(addr, pw));
        // Android: request 128K FFT for faster spectrum refresh
        let _ = self.cmd_tx.send(Command::SetSpectrumFftSize(128));
    }

    pub fn send_totp_code(&self, code: String) {
        let _ = self.cmd_tx.send(Command::SendTotpCode(code));
    }

    pub fn disconnect(&self) {
        let _ = self.cmd_tx.send(Command::Disconnect);
    }

    pub fn set_ptt(&self, active: bool) {
        let _ = self.cmd_tx.send(Command::SetPtt(active));
    }

    /// Hand the beep settings to the engine.
    ///
    /// Everything that makes a roger beep work - the tone, which modes it
    /// belongs in, holding PTT until it has gone out - is in the engine this
    /// app already runs. Nothing of it is duplicated here; this is the way in.
    pub fn set_roger_beep(&self, beep: BridgeRogerBeep) {
        let _ = self.cmd_tx.send(Command::SetRogerBeep(
            sdr_remote_logic::roger::RogerBeep {
                freq_hz: beep.freq_hz,
                volume: beep.volume,
                duration_ms: beep.duration_ms,
                include_fm: beep.include_fm,
                on_thetis: beep.on_thetis,
                on_radio1: beep.on_radio1,
                on_radio2: beep.on_radio2,
            }
            .clamped(),
        ));
    }

    pub fn set_mic_gate_delay_ms(&self, delay_ms: u32) {
        let _ = self.cmd_tx.send(Command::SetMicGateDelayMs(delay_ms));
    }

    /// Measurement mode (open point 34): `Some(dir)` starts a new file in `dir`,
    /// `None` stops it.
    /// The log copy next to a finished measurement is made in `get_state`, when
    /// the state stops naming the file (round 91) - not here.
    pub fn set_measurement(&self, dir: Option<String>) {
        let _ = self.cmd_tx.send(Command::SetMeasurement(dir));
    }

    /// "Relay only" from the measurement screen (build 137): no direct path
    /// while on. Does nothing without a relay; the engine says so in the log.
    pub fn set_relay_only(&self, on: bool) {
        let _ = self.cmd_tx.send(Command::SetRelayOnly(on));
    }

    /// "Force TCP" from the measurement screen (build 146): everything over the
    /// relay's TCP line while on. Does nothing without a relay; the engine says
    /// so in the log.
    pub fn set_force_tcp(&self, on: bool) {
        let _ = self.cmd_tx.send(Command::SetForceTcp(on));
    }

    /// The serving cell, for the measurement file (build 181). The tech by its
    /// short name (`lte`, `nr`, ...); Android's "unavailable" for what it does
    /// not give.
    pub fn measure_cell(&self, tech: String, cell_id: i64, pci: i32, arfcn: i32, tac: i32) {
        use sdr_remote_logic::measure::{NetSample, Tech};
        let tech = Tech::from_name(&tech);
        let _ = self.cmd_tx.send(Command::MeasureNetwork(NetSample::Cell { tech, cell_id, pci, arfcn, tac }));
    }

    /// A 5G cell Android lists but not as the registered one, with how it is
    /// used (Android's `CellInfo.CONNECTION_*` number, passed on unchanged), for
    /// the measurement file (build 210).
    pub fn measure_nr_cell(&self, link: i32, cell_id: i64, pci: i32, arfcn: i32, tac: i32) {
        use sdr_remote_logic::measure::{CellLink, NetSample};
        let link = CellLink::from_android(link);
        let _ = self.cmd_tx.send(Command::MeasureNetwork(NetSample::NrCell { link, cell_id, pci, arfcn, tac }));
    }

    /// The signal of the serving cell (or the wifi RSSI), for the measurement
    /// file (build 181).
    pub fn measure_signal(&self, tech: String, rsrp: i32, rsrq: i32, sinr: i32, level: i32) {
        use sdr_remote_logic::measure::{NetSample, Tech};
        let tech = Tech::from_name(&tech);
        let _ = self.cmd_tx.send(Command::MeasureNetwork(NetSample::Signal { tech, rsrp, rsrq, sinr, level }));
    }

    /// The phone's position, for the measurement file (build 181).
    pub fn measure_gps(&self, lat_e6: i32, lon_e6: i32, speed_kmh: i32, accuracy_m: i32) {
        use sdr_remote_logic::measure::NetSample;
        let _ = self.cmd_tx.send(Command::MeasureNetwork(NetSample::Gps { lat_e6, lon_e6, speed_kmh, accuracy_m }));
    }

    /// Android reports that the phone has a (new) default network. Passed to the
    /// relay monitor, which uses it only to end a wait for a network that was not
    /// up yet (build 98). Nothing to do in direct mode.
    pub fn network_available(&self) {
        info!("Android: a network is available");
        if let Some(monitor) = self.relay_monitor.lock().unwrap().as_ref() {
            monitor.network_available();
        }
    }

    /// Android reports that the default network is another one than before.
    /// The relay session still runs over the old one; the monitor ends it and
    /// starts one on the new network (build 186). Nothing to do in direct mode.
    pub fn network_changed(&self) {
        // Named in advance (working rule 11, build 186).
        info!("Android: the default network changed");
        if let Some(monitor) = self.relay_monitor.lock().unwrap().as_ref() {
            monitor.network_changed();
        }
    }

    pub fn set_playback_mute(&self, mute: bool) {
        let _ = self.cmd_tx.send(Command::SetPlaybackMute(mute));
    }

    pub fn set_dx_spots_enabled(&self, enabled: bool) {
        let _ = self.cmd_tx.send(Command::SetDxSpotsEnabled(enabled));
    }

    pub fn set_rx_volume(&self, volume: f32) {
        let _ = self.cmd_tx.send(Command::SetRxVolume(volume));
    }

    /// Local RX1 playback volume - client-only, independent of the Thetis AF gain
    /// (ZZLA) and of the master. This is how Thetis audio is silenced while a Yaesu
    /// is being listened to: the master would silence the Yaesu along with it.
    pub fn set_vfo_a_volume(&self, volume: f32) {
        let _ = self.cmd_tx.send(Command::SetVfoAVolume(volume));
    }

    /// Local RX2 playback volume, counterpart of `set_vfo_a_volume`.
    pub fn set_vfo_b_volume(&self, volume: f32) {
        let _ = self.cmd_tx.send(Command::SetVfoBVolume(volume));
    }

    pub fn set_local_volume(&self, volume: f32) {
        let _ = self.cmd_tx.send(Command::SetLocalVolume(volume));
    }

    pub fn set_tx_gain(&self, gain: f32) {
        let _ = self.cmd_tx.send(Command::SetTxGain(gain));
    }

    pub fn set_frequency(&self, hz: u64) {
        let _ = self.cmd_tx.send(Command::SetFrequency(hz));
    }

    pub fn set_mode(&self, mode: u8) {
        let _ = self.cmd_tx.send(Command::SetMode(mode));
    }

    pub fn set_agc_enabled(&self, enabled: bool) {
        let _ = self.cmd_tx.send(Command::SetAgcEnabled(enabled));
    }

    /// The operator is hearing a Yaesu rather than Thetis.
    ///
    /// The engine owns what follows - a grace period, then an unsubscribe of the
    /// Thetis audio and spectrum. Deliberately not decided here: this screen has
    /// no test harness, and the unsubscribe has to travel through the engine's
    /// send path so it is replayed after a network switch. A saving a reconnect
    /// silently undoes is not a saving.
    pub fn set_yaesu_is_listening_radio(&self, on: bool) {
        let _ = self.cmd_tx.send(Command::SetYaesuIsListeningRadio(on));
    }

    /// The "Start Thetis automatically" option; the engine decides and
    /// launches (sdr_remote_logic::thetis_autostart), as on the desktop.
    pub fn set_thetis_autostart(&self, on: bool) {
        let _ = self.cmd_tx.send(Command::SetThetisAutostart(on));
    }

    pub fn set_control(&self, control_id: u8, value: u16) {
        if let Some(id) = ControlId::from_u8(control_id) {
            let _ = self.cmd_tx.send(Command::SetControl(id, value));
        }
    }

    pub fn enable_spectrum(&self, enabled: bool) {
        let _ = self.cmd_tx.send(Command::EnableSpectrum(enabled));
    }

    pub fn set_spectrum_fps(&self, fps: u8) {
        let _ = self.cmd_tx.send(Command::SetSpectrumFps(fps));
    }

    pub fn set_spectrum_max_bins(&self, bins: u16) {
        let _ = self.cmd_tx.send(Command::SetSpectrumMaxBins(bins));
        let _ = self.cmd_tx.send(Command::SetControl(
            sdr_remote_core::protocol::ControlId::Rx2SpectrumMaxBins, bins));
    }

    pub fn set_spectrum_zoom(&self, zoom: f32) {
        let _ = self.cmd_tx.send(Command::SetSpectrumZoom(zoom));
    }

    pub fn set_spectrum_pan(&self, pan: f32) {
        let _ = self.cmd_tx.send(Command::SetSpectrumPan(pan));
    }

    pub fn set_amplitec_switch_a(&self, pos: u8) {
        let _ = self.cmd_tx.send(Command::SetAmplitecSwitchA(pos));
    }

    pub fn set_amplitec_switch_b(&self, pos: u8) {
        let _ = self.cmd_tx.send(Command::SetAmplitecSwitchB(pos));
    }

    pub fn tuner_tune(&self) {
        let _ = self.cmd_tx.send(Command::TunerTune);
    }

    pub fn tuner_abort(&self) {
        let _ = self.cmd_tx.send(Command::TunerAbort);
    }

    pub fn spe_operate(&self) {
        let _ = self.cmd_tx.send(Command::SpeOperate);
    }

    pub fn spe_tune(&self) {
        let _ = self.cmd_tx.send(Command::SpeTune);
    }

    pub fn spe_antenna(&self) {
        let _ = self.cmd_tx.send(Command::SpeAntenna);
    }

    pub fn spe_input(&self) {
        let _ = self.cmd_tx.send(Command::SpeInput);
    }

    pub fn spe_power(&self) {
        let _ = self.cmd_tx.send(Command::SpePower);
    }

    pub fn spe_band_up(&self) {
        let _ = self.cmd_tx.send(Command::SpeBandUp);
    }

    pub fn spe_band_down(&self) {
        let _ = self.cmd_tx.send(Command::SpeBandDown);
    }

    pub fn spe_off(&self) {
        let _ = self.cmd_tx.send(Command::SpeOff);
    }

    pub fn spe_power_on(&self) {
        let _ = self.cmd_tx.send(Command::SpePowerOn);
    }

    pub fn spe_drive_down(&self) {
        let _ = self.cmd_tx.send(Command::SpeDriveDown);
    }

    pub fn spe_drive_up(&self) {
        let _ = self.cmd_tx.send(Command::SpeDriveUp);
    }

    pub fn rf2k_operate(&self, on: bool) {
        let _ = self.cmd_tx.send(Command::Rf2kOperate(on));
    }

    pub fn rf2k_tune(&self) {
        let _ = self.cmd_tx.send(Command::Rf2kTune);
    }

    pub fn rf2k_ant1(&self) {
        let _ = self.cmd_tx.send(Command::Rf2kAnt1);
    }

    pub fn rf2k_ant2(&self) {
        let _ = self.cmd_tx.send(Command::Rf2kAnt2);
    }

    pub fn rf2k_ant3(&self) {
        let _ = self.cmd_tx.send(Command::Rf2kAnt3);
    }

    pub fn rf2k_ant4(&self) {
        let _ = self.cmd_tx.send(Command::Rf2kAnt4);
    }

    pub fn rf2k_ant_ext(&self) {
        let _ = self.cmd_tx.send(Command::Rf2kAntExt);
    }

    pub fn rf2k_error_reset(&self) {
        let _ = self.cmd_tx.send(Command::Rf2kErrorReset);
    }

    pub fn rf2k_close(&self) {
        let _ = self.cmd_tx.send(Command::Rf2kClose);
    }

    pub fn rf2k_drive_up(&self) {
        let _ = self.cmd_tx.send(Command::Rf2kDriveUp);
    }

    pub fn rf2k_drive_down(&self) {
        let _ = self.cmd_tx.send(Command::Rf2kDriveDown);
    }

    pub fn rf2k_tuner_mode(&self, mode: u8) {
        let _ = self.cmd_tx.send(Command::Rf2kTunerMode(mode));
    }

    pub fn rf2k_tuner_bypass(&self, on: bool) {
        let _ = self.cmd_tx.send(Command::Rf2kTunerBypass(on));
    }

    pub fn rf2k_tuner_reset(&self) {
        let _ = self.cmd_tx.send(Command::Rf2kTunerReset);
    }

    pub fn rf2k_tuner_store(&self) {
        let _ = self.cmd_tx.send(Command::Rf2kTunerStore);
    }

    pub fn rf2k_tuner_l_up(&self) {
        let _ = self.cmd_tx.send(Command::Rf2kTunerLUp);
    }

    pub fn rf2k_tuner_l_down(&self) {
        let _ = self.cmd_tx.send(Command::Rf2kTunerLDown);
    }

    pub fn rf2k_tuner_c_up(&self) {
        let _ = self.cmd_tx.send(Command::Rf2kTunerCUp);
    }

    pub fn rf2k_tuner_c_down(&self) {
        let _ = self.cmd_tx.send(Command::Rf2kTunerCDown);
    }

    pub fn rf2k_tuner_k(&self) {
        let _ = self.cmd_tx.send(Command::Rf2kTunerK);
    }

    pub fn ub_retract(&self) {
        let _ = self.cmd_tx.send(Command::UbRetract);
    }

    pub fn ub_set_frequency(&self, khz: u16, direction: u8) {
        let _ = self.cmd_tx.send(Command::UbSetFrequency(khz, direction));
    }

    pub fn ub_read_elements(&self) {
        let _ = self.cmd_tx.send(Command::UbReadElements);
    }

    pub fn rotor_goto(&self, angle_x10: u16) {
        let _ = self.cmd_tx.send(Command::RotorGoTo(angle_x10));
    }

    pub fn rotor_stop(&self) {
        let _ = self.cmd_tx.send(Command::RotorStop);
    }

    pub fn rotor_cw(&self) {
        let _ = self.cmd_tx.send(Command::RotorCw);
    }

    pub fn rotor_ccw(&self) {
        let _ = self.cmd_tx.send(Command::RotorCcw);
    }

    // Yaesu FT-991A
    pub fn yaesu_enable(&self, on: bool) {
        let _ = self.cmd_tx.send(Command::SetControl(
            sdr_remote_core::protocol::ControlId::YaesuEnable, on as u16));
    }

    /// Yaesu radio power on/off (CAT PS). Only meaningful on the 991A (PS0 = standby,
    /// USB stays up -> remote on again); the FTX-1 really switches off -> label only in the UI.
    pub fn yaesu_power_on_off(&self, on: bool) {
        let _ = self.cmd_tx.send(Command::SetControl(
            sdr_remote_core::protocol::ControlId::YaesuPowerOnOff, on as u16));
    }
    pub fn yaesu2_power_on_off(&self, on: bool) {
        let _ = self.cmd_tx.send(Command::SetControl(
            sdr_remote_core::protocol::ControlId::Yaesu2PowerOnOff, on as u16));
    }

    /// The server's copy of the memories (value 1), as the desktop asks: the
    /// server reads the radio once and gives every subscriber its copy. With
    /// 0 every switch of radio read the FTX-1 again - 3.5 s in which the
    /// server refused the PTT (owner, 2026-10-11; build 169). A server
    /// without a copy still reads the radio.
    pub fn yaesu_read_memories(&self) {
        let _ = self.cmd_tx.send(Command::SetControl(
            sdr_remote_core::protocol::ControlId::YaesuReadMemories, 1));
    }

    pub fn yaesu_ptt(&self, on: bool) {
        let _ = self.cmd_tx.send(Command::SetYaesuPtt(on));
    }

    pub fn yaesu_volume(&self, vol: f32) {
        let _ = self.cmd_tx.send(Command::SetYaesuVolume(vol));
    }

    pub fn yaesu_select_vfo(&self, vfo: u8) {
        let _ = self.cmd_tx.send(Command::SetControl(
            sdr_remote_core::protocol::ControlId::YaesuSelectVfo, vfo as u16));
    }

    pub fn yaesu_recall_memory(&self, channel: u16) {
        let _ = self.cmd_tx.send(Command::SetControl(
            sdr_remote_core::protocol::ControlId::YaesuRecallMemory, channel));
    }

    pub fn yaesu_freq(&self, hz: u64) {
        let _ = self.cmd_tx.send(Command::SetYaesuFreq(hz));
    }

    pub fn yaesu_mode(&self, mode: u8) {
        let _ = self.cmd_tx.send(Command::SetYaesuMode(mode));
    }

    pub fn yaesu_button(&self, button_id: u16) {
        let _ = self.cmd_tx.send(Command::SetControl(
            sdr_remote_core::protocol::ControlId::YaesuButton, button_id));
    }

    pub fn yaesu_tx_gain(&self, gain: f32) {
        let _ = self.cmd_tx.send(Command::SetYaesuTxGain(gain));
    }

    pub fn yaesu_eq_band(&self, band: u8, gain_db: f32) {
        let _ = self.cmd_tx.send(Command::SetYaesuEqBand(band, gain_db));
    }

    pub fn yaesu_eq_enabled(&self, on: bool) {
        let _ = self.cmd_tx.send(Command::SetYaesuEqEnabled(on));
    }

    /// Client-side speech compressor amount (0-100) for the Yaesu TX, radio 1.
    pub fn yaesu_compressor(&self, level: u8) {
        let _ = self.cmd_tx.send(Command::SetYaesuCompressor(level));
    }

    /// Client-side Yaesu TX AGC on/off, radio 1 (its own toggle, separate from the
    /// Thetis AGC).
    pub fn yaesu_tx_agc(&self, on: bool) {
        let _ = self.cmd_tx.send(Command::SetYaesuTxAgc(on));
    }

    /// Client-side speech compressor amount (0-100) for the Yaesu TX, radio 2 (FTX-1).
    pub fn yaesu2_compressor(&self, level: u8) {
        let _ = self.cmd_tx.send(Command::SetYaesu2Compressor(level));
    }

    /// Client-side Yaesu TX AGC on/off, radio 2 (FTX-1).
    pub fn yaesu2_tx_agc(&self, on: bool) {
        let _ = self.cmd_tx.send(Command::SetYaesu2TxAgc(on));
    }

    /// Typed DSP/function control for a slot (0 = radio 1, 1 = radio 2). Covers all
    /// DSP buttons + clarifier (YaesuCtrl index in `control`, value in `value`).
    pub fn yaesu_control(&self, slot: u8, control: u8, value: u16) {
        let _ = self.cmd_tx.send(Command::SetYaesuControl(slot, control, value));
    }

    // -- Radio 2 (yaesu2): a mirror of the yaesu_* functions, routed to slot 1 --
    pub fn yaesu2_enable(&self, on: bool) {
        let _ = self.cmd_tx.send(Command::SetControl(
            sdr_remote_core::protocol::ControlId::Yaesu2Enable, on as u16));
    }

    /// As `yaesu_read_memories`, for the second radio.
    pub fn yaesu2_read_memories(&self) {
        let _ = self.cmd_tx.send(Command::SetControl(
            sdr_remote_core::protocol::ControlId::Yaesu2ReadMemories, 1));
    }

    pub fn yaesu2_ptt(&self, on: bool) {
        let _ = self.cmd_tx.send(Command::SetYaesu2Ptt(on));
    }

    pub fn yaesu2_volume(&self, vol: f32) {
        let _ = self.cmd_tx.send(Command::SetYaesu2Volume(vol));
    }

    pub fn yaesu2_select_vfo(&self, vfo: u8) {
        let _ = self.cmd_tx.send(Command::SetControl(
            sdr_remote_core::protocol::ControlId::Yaesu2SelectVfo, vfo as u16));
    }

    pub fn yaesu2_recall_memory(&self, channel: u16) {
        let _ = self.cmd_tx.send(Command::SetControl(
            sdr_remote_core::protocol::ControlId::Yaesu2RecallMemory, channel));
    }

    pub fn yaesu2_freq(&self, hz: u64) {
        let _ = self.cmd_tx.send(Command::SetYaesu2Freq(hz));
    }

    pub fn yaesu2_mode(&self, mode: u8) {
        let _ = self.cmd_tx.send(Command::SetYaesu2Mode(mode));
    }

    pub fn yaesu2_button(&self, button_id: u16) {
        let _ = self.cmd_tx.send(Command::SetControl(
            sdr_remote_core::protocol::ControlId::Yaesu2Button, button_id));
    }

    pub fn yaesu2_tx_gain(&self, gain: f32) {
        let _ = self.cmd_tx.send(Command::SetYaesu2TxGain(gain));
    }

    pub fn yaesu2_eq_band(&self, band: u8, gain_db: f32) {
        let _ = self.cmd_tx.send(Command::SetYaesu2EqBand(band, gain_db));
    }

    pub fn yaesu2_eq_enabled(&self, on: bool) {
        let _ = self.cmd_tx.send(Command::SetYaesu2EqEnabled(on));
    }

    pub fn server_reboot(&self) {
        let _ = self.cmd_tx.send(Command::ServerReboot);
    }

    pub fn server_shutdown(&self) {
        let _ = self.cmd_tx.send(Command::ServerShutdown);
    }

    pub fn get_state(&self) -> BridgeRadioState {
        let rx = self.state_rx.lock().unwrap();
        let state = rx.borrow().clone();
        // A measurement that just ended - by Stop, by a write error, or replaced -
        // gets the log next to it, with its name (`tl-meting-....log`), so one
        // fetch over USB brings both (owner, 2026-10-03). On a thread of its own:
        // up to 4 MB, and this is called from the screen's loop (round 91).
        let ended = self.measurement_log.lock().unwrap().step(state.measurement_file.as_deref());
        if let Some(dest) = ended {
            std::thread::spawn(move || match crate::logging::copy_log_to(&dest) {
                Ok(()) => log::info!("Measurement: log copied to {}", dest.display()),
                Err(e) => log::warn!("Measurement: log could not be copied to {}: {}", dest.display(), e),
            });
        }
        let lang_str = self.ui_language.lock().unwrap().clone();
        let lang = sdr_remote_logic::i18n::Lang::from_code(&lang_str);
        bridge_state_from_radio_state(state, lang)
    }

    // ---- the chat ---------------------------------------------------------
    //
    // One call does the housekeeping and returns the state, because Compose has
    // a timer and not a frame loop: `chat_state(open)` ticks the model (which
    // schedules its own polling) and hands back everything the screen draws.
    // Everything else is a verb the model already knows.

    /// The chat as it stands. `open` says whether the chat screen is showing:
    /// while it is, the conversation is polled briskly; while it is not, once
    /// every half minute, which is what keeps the unread badge honest.
    pub fn chat_state(&self, open: bool) -> BridgeChatState {
        let ticket = self
            .relay_status
            .as_ref()
            .and_then(|h| h.snapshot().chat_ticket);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let mut chat = self.chat.lock().unwrap();
        chat.tick(&self.chat_relay_url, ticket.as_deref(), open);
        BridgeChatState {
            offline_reason: match chat.offline {
                None => 0,
                Some(sdr_remote_chat::OfflineReason::NoRelay) => 1,
                Some(sdr_remote_chat::OfflineReason::NoTicket) => 2,
                Some(sdr_remote_chat::OfflineReason::Unreachable) => 3,
            },
            consent_known: chat.consented.is_some(),
            consented: chat.consented.unwrap_or(false),
            display_name: chat.display_name.clone(),
            unread: chat.unread as u32,
            error: chat.error.clone().unwrap_or_default(),
            reports_left: chat.reports_left,
            messages: chat
                .messages
                .iter()
                .map(|m| BridgeChatMessage {
                    id: m.id,
                    at: m.at,
                    // Empty rather than optional: a name that is not there
                    // belongs to somebody who left, and Compose says so in its
                    // own words rather than showing a null.
                    name: m.name.clone().unwrap_or_default(),
                    body: m.body.clone(),
                    reply_name: m.reply_name.clone().unwrap_or_default(),
                    reply_text: m.reply_text.clone().unwrap_or_default(),
                    edited: m.edited,
                    mine: chat.is_mine(m),
                    can_edit: chat.can_edit(m, now),
                })
                .collect(),
            // Only what has not been folded away. The whole list is what the
            // service holds; what is still on screen is this side's business.
            answers: chat
                .unread_answers()
                .iter()
                .map(|a| BridgeChatAnswer {
                    id: a.id,
                    at: a.at,
                    body: a.body.clone(),
                })
                .collect(),
        }
    }

    /// Join, under this name. The service checks the name and the consent-text
    /// version; this only carries the intent.
    pub fn chat_consent(&self, display_name: String) {
        self.chat.lock().unwrap().consent(&display_name);
    }

    /// Say something. `reply_to` is 0 when it answers nothing - Compose has no
    /// use for an optional here and 0 is not a message id.
    pub fn chat_send(&self, body: String, reply_to: i64) {
        let reply = if reply_to > 0 { Some(reply_to) } else { None };
        self.chat.lock().unwrap().send(&body, reply);
    }

    /// Correct one's own message. The service is the judge of "own" and of the
    /// fifteen-minute window; a refusal comes back as the error in `chat_state`.
    pub fn chat_edit(&self, id: i64, body: String) {
        self.chat.lock().unwrap().edit(id, &body);
    }

    /// Leave the chat, with or without taking one's own messages along.
    pub fn chat_leave(&self, delete_messages: bool) {
        self.chat.lock().unwrap().leave(delete_messages);
    }

    /// The unread badge goes out when the screen has been looked at.
    pub fn chat_mark_read(&self) {
        self.chat.lock().unwrap().mark_read();
    }

    /// Clean a log and a settings dump into the attachment the user then reads.
    ///
    /// Android has neither file to point at - the log comes from the system log
    /// and the settings from the framework's preferences - so both arrive here
    /// as text the app already holds. The cleaning is the shared one, rules and
    /// all. Returns the attachment, or a line saying why there is none; either
    /// way what comes back is what the screen shows and what `chat_report`
    /// sends, because the preview is the actual safeguard (design 1.3).
    pub fn chat_build_attachment(&self, log: String, settings: String) -> String {
        match sdr_remote_core::diagnose::build_attachment_from_text(
            &log,
            &settings,
            &self.relay_url,
        ) {
            Ok(a) => a,
            Err(e) => format!("(no attachment: {e})"),
        }
    }

    /// Report a problem, in the reporter's own words, with the attachment the
    /// reporter saw - or none at all.
    ///
    /// `attachment` is passed back exactly as `chat_build_attachment` returned
    /// it and is deliberately not rebuilt here: what leaves the phone has to be
    /// what was on the screen. Empty means the box was not ticked. Joining the
    /// chat is not required (design section 4), so this works for somebody who
    /// never consented.
    /// Fold an administrator answer away.
    ///
    /// The phone had no way to do this at all, and the answers sat unbounded
    /// above the conversation - so a reader with a few of them could not read
    /// the chat any more (two users, 2026-08-20). The remembering lives in the
    /// shared model, so both front ends agree on what has been put aside.
    pub fn chat_dismiss_answer(&self, id: i64) {
        self.chat.lock().unwrap().dismiss_answer(id);
    }

    /// The folded-away ids, for the app to keep in its preferences.
    pub fn chat_seen_ids(&self) -> Vec<i64> {
        self.chat.lock().unwrap().seen_ids()
    }

    /// Take back what the app had kept, at startup.
    pub fn chat_restore_seen(&self, ids: Vec<i64>) {
        self.chat.lock().unwrap().restore_seen(&ids);
    }

    pub fn chat_report(&self, note: String, attachment: String) {
        let full = sdr_remote_core::diagnose::describe(
            &note,
            &self.relay_url,
            sdr_remote_core::version_string().as_str(),
            "android",
            if attachment.trim().is_empty() {
                None
            } else {
                Some(attachment.as_str())
            },
        );
        self.chat.lock().unwrap().send_diagnosis(&full);
    }

    pub fn shutdown(&self) {
        // A measurement still running when the app goes ends without the state
        // ever saying so: copy its log now, and here rather than on a thread of
        // its own, which the process could take with it (round 92).
        if let Some(dest) = self.measurement_log.lock().unwrap().step(None) {
            match crate::logging::copy_log_to(&dest) {
                Ok(()) => log::info!("Measurement: log copied to {} at shutdown", dest.display()),
                Err(e) => log::warn!("Measurement: log could not be copied to {} at shutdown: {}", dest.display(), e),
            }
        }
        let mut guard = self.shutdown_tx.lock().unwrap();
        if let Some(tx) = guard.take() {
            let _ = tx.send(true);
        }
        drop(guard);
        // Also stop the relay monitor. Otherwise its background thread keeps the
        // relay connection (and its client_id slot) alive after this app instance is
        // gone — and when the next instance starts with the same install id, the two
        // ping-pong the slot (each reclaim closes the other, which reconnects after
        // RECONNECT_DELAY). Stopping the monitor here (ViewModel.onCleared) prevents
        // the zombie connection.
        // Taken out in its own statement so this mutex is released before the
        // slot's is taken below. Written inside the `if let`, the guard lived to
        // the end of the block (a scrutinee temporary, edition 2021), and the two
        // locks were nested in an order nobody had chosen or written down. No path
        // takes them the other way round today; not nesting them at all means none
        // can start to (reviewer finding, 2026-09-25).
        let taken = self.relay_monitor.lock().unwrap().take();
        if let Some(monitor) = taken {
            info!("bridge shutting down - stopping its relay monitor");
            let mine = monitor.stop_handle();
            monitor.stop();
            // And empty the duplicate-guard slot, which until now was filled in
            // one place and cleared in none. A stale handle there is what made
            // the guard's warning untrue on every restart; build 12 taught the
            // warning to ask, and this removes the reason it had to.
            //
            // Only if the slot still holds THIS monitor. On a network switch the
            // next bridge has already put its own handle there - that is the
            // case the slot exists for - and emptying it blindly would leave two
            // live monitors with nothing watching them.
            sdr_remote_relay::RelayStopHandle::clear_slot_if_mine(
                &mut PREVIOUS_MONITOR.lock().unwrap(),
                &mine,
            );
        }
    }
}

#[cfg(test)]
mod denial_bridge_tests {
    use super::*;
    use sdr_remote_core::protocol::DeniedTarget;

    /// The phone reads two levels off this conversion and nothing else, so this
    /// is the whole of what a named transmitter changes for Android.
    ///
    /// It was claimed rather than shown - "the sign only gets more accurate" -
    /// and a claim about the one front end nobody rebuilds is exactly the one
    /// worth pinning. No Kotlin needed: the conversion is plain Rust and it
    /// runs in the ordinary suite.
    #[test]
    fn the_phone_sees_only_the_transmitters_that_were_really_refused() {
        let mut state = RadioState::default();
        // Everything keyed, and the server refuses one radio by name.
        state
            .ptt_denial
            .apply_refusal([true, true, true], DeniedTarget::Radio1, true);

        let bridged: BridgeRadioState = state.clone().into();
        assert!(bridged.ptt_denied);
        assert!(bridged.ptt_released_by_server);

        // Letting go of that radio ends it. Before the server named which one,
        // the other two were in the mask as well and kept the sign up.
        state.ptt_denial.transmitter_off(1);
        let bridged: BridgeRadioState = state.into();
        assert!(
            !bridged.ptt_denied,
            "a transmitter that was never refused is holding the sign up"
        );
        assert!(
            !bridged.ptt_released_by_server,
            "the reason outlived the refusal it belonged to"
        );
    }
}

#[cfg(test)]
mod relay_rule_tests {
    //! The bridge's own view of the relay rule. The rule itself is tested in
    //! `sdr-remote-relay`, and nothing said that the function Compose actually
    //! calls hands the same answer through - so a review went looking for a
    //! test on `relay_is_configured` and found none.

    #[test]
    fn the_bridge_hands_through_the_shared_rule() {
        let (u, s, t) = ("wss://relay.example/ws", "pa0xyz", "secret");
        for (enabled, url, station, token) in [
            (true, u, s, t),
            (false, u, s, t),
            (true, "", s, t),
            (true, u, "", t),
            // The one the phone got wrong: everything but the token.
            (true, u, s, ""),
        ] {
            assert_eq!(
                super::relay_is_configured(
                    enabled,
                    url.to_string(),
                    station.to_string(),
                    token.to_string()
                ),
                sdr_remote_relay::is_configured(enabled, url, station, token),
                "the bridge disagrees with the shared rule for {url:?}/{station:?}/{token:?}"
            );
        }
    }

    /// The same for the route word the settings compare with: the function
    /// Compose calls gives the shared rule's answer in every state of the relay
    /// settings, a half-filled relay included (review 119).
    #[test]
    fn the_bridge_hands_through_the_route_rule() {
        use sdr_remote_logic::measure::Route;
        let (u, s, t) = ("wss://relay.example/ws", "pa0xyz", "secret");
        for token in [t, ""] {
            for relay_enabled in [false, true] {
                let configured = sdr_remote_relay::is_configured(relay_enabled, u, s, token);
                assert_eq!(
                    super::expected_route_label(relay_enabled, u.to_string(), s.to_string(), token.to_string()),
                    Route::asked_for(configured).label(),
                    "relay {relay_enabled} (token {:?})",
                    token
                );
            }
        }
        // And the words themselves.
        let label = |relay| super::expected_route_label(relay, u.to_string(), s.to_string(), t.to_string());
        assert_eq!(label(true), "QUIC via relay");
        assert_eq!(label(false), "QUIC");
    }
}

#[cfg(test)]
mod language_resource_tests {
    //! The app hands the Rust side its language by reading `ui_language_code` out
    //! of the very `strings.xml` Android resolved. That only works while every
    //! locale file declares it, and declares its own language - a copy-paste that
    //! left `nl` in the German file would silently put one line in the wrong
    //! language, which is exactly the sort of thing nobody notices by looking.

    use std::path::PathBuf;

    fn res_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("android/app/src/main/res")
    }

    fn declared_code(dir: &str) -> String {
        let path = res_dir().join(dir).join("strings.xml");
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        let open = "<string name=\"ui_language_code\">";
        let start = text
            .find(open)
            .unwrap_or_else(|| panic!("{dir}/strings.xml does not declare ui_language_code"))
            + open.len();
        let end = start
            + text[start..]
                .find("</string>")
                .expect("unterminated ui_language_code");
        text[start..end].trim().to_string()
    }

    #[test]
    fn every_locale_declares_its_own_language() {
        for (dir, expected) in [
            ("values", "en"),
            ("values-nl", "nl"),
            ("values-de", "de"),
            ("values-fr", "fr"),
        ] {
            assert_eq!(
                declared_code(dir),
                expected,
                "{dir}/strings.xml declares the wrong language"
            );
        }
    }

    #[test]
    fn the_rust_side_recognises_every_declared_language() {
        use sdr_remote_logic::i18n::Lang;
        for dir in ["values", "values-nl", "values-de", "values-fr"] {
            let code = declared_code(dir);
            let lang = Lang::from_code(&code);
            assert_eq!(
                lang.code(),
                code,
                "{dir} declares {code}, which the Rust side quietly turns into {}",
                lang.code()
            );
        }
    }
}

#[cfg(test)]
mod ble_ptt_bridge_tests {
    //! The gate Kotlin actually calls, not the state machine underneath. The
    //! rule is tested in `sdr-remote-logic`; nothing said that this wrapper
    //! hands the same answer through, and a wrapper is exactly where a
    //! translation quietly loses a case.

    use super::{BlePttAction, BlePttGate};

    #[test]
    fn a_drop_while_held_releases_through_the_bridge_too() {
        let gate = BlePttGate::new();
        gate.begin(1);
        assert_eq!(gate.connected(1), BlePttAction::Gone);
        assert_eq!(gate.notification(1, 0x01), BlePttAction::KeyDown);
        assert!(gate.held());

        assert_eq!(gate.disconnected(1), BlePttAction::Gone);
        assert!(!gate.held(), "the drop must reach the transmitter");
    }

    /// The mode moved to PhonePtt, where the screen button and the volume keys
    /// obey the same one. What is left here is what only this side knows: a
    /// press is a press, and every link event releases.
    #[test]
    fn presses_and_drops_reach_the_transmitter_through_the_bridge() {
        let gate = BlePttGate::new();
        gate.begin(1);
        gate.connected(1);

        assert_eq!(gate.notification(1, 0x01), BlePttAction::KeyDown);
        assert!(gate.held());
        assert_eq!(gate.notification(1, 0x00), BlePttAction::KeyUp);
        assert!(!gate.held());

        gate.notification(1, 0x01);
        assert_eq!(gate.disconnected(1), BlePttAction::Gone);
        assert!(!gate.held(), "the drop must reach the transmitter");
    }

    #[test]
    fn the_drop_rule_survives_the_translation() {
        use super::{ble_drop_policy, BleDropPolicy};
        assert_eq!(ble_drop_policy(true), BleDropPolicy::Reconnect);
        assert_eq!(ble_drop_policy(false), BleDropPolicy::Close);
    }

    /// The scenario the review blocked on, over the boundary Kotlin calls.
    #[test]
    fn a_press_from_a_replaced_connection_stops_at_the_bridge() {
        let gate = BlePttGate::new();
        gate.begin(1);
        gate.connected(1);

        // A different button is picked while a callback is still in flight.
        gate.begin(2);

        assert_eq!(gate.notification(1, 0x01), BlePttAction::Nothing);
        assert!(!gate.held());
    }

    #[test]
    fn the_unknown_byte_survives_the_translation() {
        let gate = BlePttGate::new();
        gate.begin(1);
        gate.connected(1);
        // 0xAB is the other channel's press byte. Real, and not ours.
        assert_eq!(gate.notification(1, 0xAB), BlePttAction::Unrecognised);
        assert!(!gate.held());
    }

    #[test]
    fn a_release_is_not_invented_when_nothing_is_held() {
        let gate = BlePttGate::new();
        gate.begin(1);
        gate.connected(1);
        assert_eq!(gate.notification(1, 0x00), BlePttAction::Nothing);
        // A drop is the one thing that IS reported unconditionally, and that
        // changed on purpose: in toggle mode the button is not held while the
        // radio is on the air, so "nothing is held" no longer means "there is
        // nothing to release". Releasing nothing costs nothing.
        assert_eq!(gate.disconnected(1), BlePttAction::Gone);
    }
}

#[cfg(test)]
mod udl_order_tests {
    //! UniFFI calls the Rust method by position, not by name: a UDL line and a
    //! Rust signature that list two arguments of the same type in a different
    //! order compile, and every value lands in the other's place (review of
    //! 210: the four `i32` of `measure_nr_cell` were written by hand in both).

    /// Argument names in order, from the text between the parentheses: the
    /// last word of each UDL argument (`i32 pci`), the part before `:` of each
    /// Rust one (`pci: i32`), without the receiver.
    fn names(args: &str, udl: bool) -> Vec<String> {
        args.split(',')
            .map(str::trim)
            .filter(|a| !a.is_empty() && !a.ends_with("self"))
            .map(|a| if udl { a.rsplit(' ').next().unwrap() } else { a.split(':').next().unwrap().trim() })
            // An unused argument keeps its place with a leading `_` (`init_logging`
            // off Android): the same name for the order.
            .map(|a| a.trim_start_matches('_').to_string())
            .collect()
    }

    /// The text of `name(...)` up to its closing parenthesis, from `at`.
    fn args_at(text: &str, at: usize) -> &str {
        let open = at + text[at..].find('(').unwrap() + 1;
        let close = open + text[open..].find(')').unwrap();
        &text[open..close]
    }

    /// The lines of one UDL block that declare a function: name and argument
    /// text. A line with a `(` that is not one whole declaration is refused, so
    /// nothing is skipped without a word (review of 211: a comment after a line
    /// or a declaration over two lines was left out, and only a floor noticed).
    fn declarations(body: &str) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for line in body.lines().map(str::trim) {
            if line.starts_with("//") || !line.contains('(') {
                continue;
            }
            assert!(line.ends_with(");"), "a UDL line this test cannot read: `{line}`");
            let head = &line[..line.find('(').unwrap()];
            let name = head.rsplit(' ').next().unwrap().to_string();
            out.push((name, args_at(line, 0).to_string()));
        }
        out
    }

    /// Every function in the UDL has the same argument names, in the same
    /// order, as its Rust side: a method as the method of that name in the
    /// `impl` block of its interface, a constructor as that block's `fn new`,
    /// and a `namespace` function as the free `pub fn` of that name (review of
    /// 211: the constructor's five strings and `ptt_button`'s two bools were not
    /// covered).
    #[test]
    fn the_udl_and_the_rust_name_every_argument_in_the_same_order() {
        let udl = include_str!("sdr_remote.udl");
        let rust = include_str!("bridge.rs");
        let free = [include_str!("bridge.rs"), include_str!("logging.rs")];
        let mut compared = 0;
        let mut interfaces = 0;
        for block in udl.split("\ninterface ").skip(1) {
            interfaces += 1;
            let iface = block.split_whitespace().next().unwrap();
            let body = &block[..block.find("\n};").unwrap()];
            let start = rust.find(&format!("\nimpl {iface} {{\n")).unwrap_or_else(|| panic!("no `impl {iface}`"));
            let end = start + 1 + rust[start + 1..].find("\n}\n").unwrap();
            let imp = &rust[start..end];
            for (name, args) in declarations(body) {
                let rust_name = if name == "constructor" { "new" } else { name.as_str() };
                let at = imp.find(&format!("fn {rust_name}(")).unwrap_or_else(|| panic!("{iface}::{name} not in its impl"));
                assert_eq!(
                    names(&args, true),
                    names(args_at(imp, at), false),
                    "{iface}::{name}: the UDL and the Rust name the arguments differently"
                );
                compared += 1;
            }
        }
        assert_eq!(interfaces, 3, "SdrBridge, PhonePtt and BlePttGate");
        let ns = &udl[udl.find("namespace sdr_remote {").expect("the namespace")..];
        let ns = &ns[..ns.find("\n};").unwrap()];
        let mut free_fns = 0;
        for (name, args) in declarations(ns) {
            let pattern = format!("pub fn {name}(");
            let sites: Vec<&str> = free
                .iter()
                .flat_map(|f| f.match_indices(pattern.as_str()).map(move |(at, _)| args_at(f, at)))
                .collect();
            assert!(!sites.is_empty(), "namespace {name}: no `pub fn {name}(`");
            for site in sites {
                assert_eq!(names(&args, true), names(site, false), "namespace {name}: the UDL and the Rust differ");
            }
            free_fns += 1;
        }
        // 140 methods and three constructors, ten namespace functions, on 2026-10-15.
        assert!(compared >= 143 && free_fns >= 10, "only {compared} methods and {free_fns} functions compared");
    }

    /// The one call that builds the bridge names its arguments: five strings in
    /// a row, where Kotlin would otherwise go by position too (review of 211).
    #[test]
    fn the_app_builds_the_bridge_with_named_arguments() {
        let vm = include_str!("../android/app/src/main/java/com/sdrremote/viewmodel/SdrViewModel.kt");
        assert_eq!(vm.matches("SdrBridge(").count(), 1, "one place builds the bridge");
        let call = &vm[vm.find("SdrBridge(").unwrap()..];
        let call = &call[..call.find(')').unwrap()];
        for name in ["relayEnabled =", "relayUrl =", "relayStation =", "relayToken =", "relayInstance =", "relayName ="] {
            assert!(call.contains(name), "`{name}` not named in {call}");
        }
    }
}
