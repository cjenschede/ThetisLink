// SPDX-License-Identifier: GPL-2.0-or-later
//
// The transmit audio as the server receives it, for Thetis and the Yaesu
// radios alike (build 166). The operator's microphone reaches the server over
// the same path whichever radio it keys, with the same jitter and the same
// losses, so it is buffered and repaired the same way (owner, 2026-10-10: "ik
// zie geen aanleiding de pakketten anders te behandelen"). Until build 166
// Thetis concealed every lost frame although the client always sends in-band
// redundancy, and a Yaesu had no jitter buffer at all: a late or lost packet
// was a gap on the air.

use std::sync::atomic::{AtomicU32, Ordering};
use sdr_remote_core::dev_info;
use std::sync::Arc;
use std::time::{Duration, Instant};

use log::{info, warn};

use sdr_remote_core::jitter::{BufferedFrame, JitterBuffer, JitterResult, TX_MAX_DEPTH, TX_MIN_DEPTH};
use sdr_remote_core::resample::{resampler, Resampler, UpFilter};
use sdr_remote_core::stream::{Filled, StreamDecoder};
use sdr_remote_core::{FRAME_SAMPLES_WIDEBAND, NETWORK_SAMPLE_RATE_WIDEBAND};

use crate::pause_trim::PauseTrim;
use crate::yaesu::tx_timing::TxTiming;

/// The start line of a transmit buffer, Thetis's and a radio's from one place
/// (build 263): it is the same buffer, so the same words. Thetis adds its ring.
pub fn buffer_line(label: &str, ring_hold: Option<Duration>) -> String {
    let t = sdr_remote_core::jitter::Tuning::transmit();
    let ring = ring_hold.map_or(String::new(), |h| format!(", ring hold {} ms", h.as_millis()));
    format!(
        "{} TX buffer: depth {}-{}, {} ms per frame, spike decay {}{}; catching up in the pauses (within {} dB of the noise floor, fallback after {} ms)",
        label,
        TX_MIN_DEPTH,
        TX_MAX_DEPTH,
        t.ms_per_frame,
        t.spike_decay_packets,
        ring,
        crate::pause_trim::QUIET_ABOVE_FLOOR_DB,
        crate::pause_trim::MAX_WAIT.as_millis()
    )
}

/// What the catching up let go in ten seconds, Thetis's and a radio's from one
/// place (builds 258, 263).
pub fn pause_line(label: &str, in_pause: u64, fallback: u64) -> String {
    format!(
        "{} TX buffer (10 s): {} frame(s) let go in a pause, {} by the fallback after {} ms",
        label,
        in_pause,
        fallback,
        crate::pause_trim::MAX_WAIT.as_millis()
    )
}

/// How often the lost-frame line may be written.
const LOSS_EVERY: Duration = Duration::from_secs(10);

/// Lost transmit frames and what became of them, for one line every ten
/// seconds when there were any; and for a radio's buffer how far it grew
/// (build 167).
pub struct TxLoss {
    label: &'static str,
    rebuilt: u32,
    concealed: u32,
    target_max: u32,
    depth_max: usize,
    /// In this window: on arrival above the cap, trimmed, too late, cleared,
    /// skipped over (builds 183, 184) - and the buffer's totals at the last look.
    thrown: [u64; 5],
    seen: [u64; 5],
    /// Second frames a playout tick pulled to rebuild Thetis's ring (build 193).
    extra: u32,
    since: Instant,
}

impl TxLoss {
    pub fn new(label: &'static str) -> Self {
        Self {
            label,
            rebuilt: 0,
            concealed: 0,
            target_max: 0,
            depth_max: 0,
            thrown: [0; 5],
            seen: [0; 5],
            extra: 0,
            since: Instant::now(),
        }
    }

    /// The buffer after a tick: its target and how much it holds. Thetis's
    /// loop calls it too (build 168), so both lines say the same thing. An
    /// empty buffer is not counted: between transmissions the target keeps
    /// what it learned, and would otherwise be reported every ten seconds
    /// (in the field log of 167: "target up to 20 frames, depth up to 0").
    pub fn note_buffer(&mut self, jb: &JitterBuffer) {
        // What it threw away since the last look - also when it is empty now,
        // which it often is right after a trim or a skip (build 183).
        let now = [jb.stats_capped, jb.stats_trimmed, jb.stats_late, jb.stats_cleared, jb.stats_skipped];
        for i in 0..5 {
            self.thrown[i] += now[i].saturating_sub(self.seen[i]);
        }
        self.seen = now;
        if jb.depth() == 0 {
            return;
        }
        self.target_max = self.target_max.max(jb.target_depth());
        self.depth_max = self.depth_max.max(jb.depth());
    }

    /// What the buffer line would say: the highest target and depth, when the
    /// target rose above the minimum (build 168; review of 167: the line the
    /// field test leans on was seen by no test).
    fn buffer_grew(&self) -> Option<(u32, usize)> {
        (self.target_max > TX_MIN_DEPTH).then_some((self.target_max, self.depth_max))
    }

    /// What the thrown-away line would say: on arrival above the cap, trimmed,
    /// too late, cleared, skipped over - when any was (builds 183, 184).
    fn thrown_away(&self) -> Option<[u64; 5]> {
        (self.thrown.iter().sum::<u64>() > 0).then_some(self.thrown)
    }

    /// A second frame pulled this tick for Thetis's ring (build 193).
    pub fn count_extra(&mut self) {
        self.extra += 1;
    }

    fn count(&mut self, filled: &Filled) {
        match filled {
            Filled::Rebuilt(..) => self.rebuilt += 1,
            Filled::Concealed(..) => self.concealed += 1,
        }
    }

    /// The line, once per `LOSS_EVERY`, only when a frame was lost.
    pub fn maybe_log(&mut self) {
        if self.since.elapsed() < LOSS_EVERY {
            return;
        }
        if self.rebuilt + self.concealed > 0 {
            // Named in advance (working rule 11, build 166). An ordinary line,
            // not a developer's: it appears only when a frame was lost, and it is
            // the server's only trace of loss on the way to it (owner, 2026-10-24).
            info!(
                "{} TX audio (10 s): {} lost frame(s): {} rebuilt from redundancy, {} concealed",
                self.label,
                self.rebuilt + self.concealed,
                self.rebuilt,
                self.concealed
            );
        }
        if let Some((target, depth)) = self.buffer_grew() {
            // Named in advance (working rule 11, builds 167 and 168): the
            // buffer grew on a jittery line.
            dev_info!("{} TX buffer (10 s): target up to {} frames, depth up to {}", self.label, target, depth);
        }
        if let Some([arrival, trimmed, late, cleared, skipped]) = self.thrown_away() {
            // Named in advance (working rule 11, builds 183 and 184): speech
            // the buffer received and did not send, every way it can, and
            // the frames it stopped waiting for.
            dev_info!(
                "{} TX audio (10 s): {} frame(s) thrown away - {} on arrival above the cap, {} trimmed to catch up, {} arrived too late, {} cleared at a new over or sender; {} not there in time (skipped over)",
                self.label,
                arrival + trimmed + late + cleared,
                arrival,
                trimmed,
                late,
                cleared,
                skipped
            );
        }
        if self.extra > 0 {
            // Named in advance (working rule 11, build 193).
            dev_info!("{} TX ring (10 s): {} extra frame(s) pulled to rebuild the lead", self.label, self.extra);
        }
        self.extra = 0;
        self.thrown = [0; 5];
        self.rebuilt = 0;
        self.concealed = 0;
        self.target_max = 0;
        self.depth_max = 0;
        self.since = Instant::now();
    }
}

/// When the last packet of a radio's transmit audio came (build 264): a radio
/// has no packet clock of its own - its PTT is a control, and it is let go by
/// the session's time-out, the watchdog on a radio nobody holds, its own
/// time-out timer or a goodbye (`PTT-NOOIT-HANGEN.md` 4.3). Inside one press,
/// a silence past the safety's packet window ends what the buffer holds, as it
/// does for Thetis (build 249).
#[derive(Debug, Default)]
pub struct PacketClock {
    last: Option<Instant>,
}

impl PacketClock {
    /// A packet at `now`: the silence before it, when it was past the window.
    pub fn packet(&mut self, now: Instant) -> Option<Duration> {
        let silence = self.last.map(|t| now.saturating_duration_since(t));
        self.last = Some(now);
        silence.filter(|&d| crate::ptt::silence_ended_the_over(Some(d)))
    }
}

/// What a packet does to a radio's buffer before it goes in (build 264).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BeforePush {
    Keep,
    /// A new transmission: it starts empty, and keeps what was learned of the
    /// line (build 168).
    NewOver,
    /// Inside one press, after a silence past the packet window: what waits is
    /// from before it, and would play first (the rule of build 249).
    Silence(Duration),
}

/// The task was at over `over`; the packet comes in over `now_over`, after
/// `silence` (`PacketClock`).
pub fn before_push(over: u64, now_over: u64, silence: Option<Duration>) -> BeforePush {
    match (now_over != over, silence) {
        (true, _) => BeforePush::NewOver,
        (false, Some(d)) => BeforePush::Silence(d),
        (false, None) => BeforePush::Keep,
    }
}

/// One packet of a radio's transmit audio as the network loop hands it over:
/// who sent it, its sequence and timestamp, the Opus frame (build 169: the
/// sender, so the buffer forgets the line when another one transmits, as
/// Thetis's does), and its PTT flag (build 282): the holder's first packet
/// without it is the release, played in its place in the stream.
pub struct TxPacket {
    pub from: std::net::SocketAddr,
    pub sequence: u32,
    pub timestamp: u32,
    pub opus: Vec<u8>,
    pub ptt: bool,
}

/// The holder's release frame came out of a radio's buffer (build 282):
/// everything before it is with the sound card. Back to the network loop,
/// which owns the transmitter and lets it go (`release_played`); `over` so a
/// press that came in meanwhile is not let go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReleasePlayed {
    pub target: crate::session::TxTarget,
    pub from: std::net::SocketAddr,
    pub sequence: u32,
    pub over: u64,
}

/// What one playout tick of a transmit buffer gave: frames for the sound card
/// or Thetis's ring, the sequence of the release frame when it was played
/// (build 282), whether a release was passed over because a press stood behind
/// it (build 283), and whether a frame with the flag played (build 288): Thetis
/// keys on that, at playout; a radio keyed on arrival and does not ask.
#[derive(Debug, Default)]
pub struct Played {
    pub pcm: Vec<Vec<i16>>,
    pub release: Option<u32>,
    pub pressed_again: bool,
    pub pressed: bool,
    /// How many frames stood behind the release frame when it played, before
    /// they went: Thetis's tail is reckoned on it (`release_from_playout`, as
    /// its own tick did until build 288).
    pub release_depth: usize,
}

/// One lost transmit frame: rebuilt from the redundancy in the next packet when
/// that packet is buffered and carries some, otherwise concealed - the core's
/// one decision, as on the client. The microphone is always wideband, whatever
/// the packet's flag says.
pub fn fill_gap(dec: &mut StreamDecoder, jb: &JitterBuffer, loss: &mut TxLoss) -> Option<Vec<i16>> {
    let next = jb.next_seq_peek().and_then(|seq| jb.peek_frame(seq)).map(|(opus, _)| (opus, true));
    let filled = dec.recover_or_conceal(next)?;
    loss.count(&filled);
    match filled {
        Filled::Rebuilt(pcm, _) | Filled::Concealed(pcm, _) => Some(pcm),
    }
}

/// One playout tick of a transmit buffer, Thetis's and a radio's (build 288;
/// until then Thetis's loop had its own copy): one frame (a gap through
/// `fill_gap`). With a backlog a quiet frame goes and the next is played in
/// its place, one a tick (build 263).
///
/// A frame without the PTT flag is the release (build 282): the tick stops
/// there and says so, and what stands behind it - the rest of the burst - goes.
/// Unless a frame with the flag stands behind it: the operator pressed again
/// within the buffer, and the release is no release - its burst is passed over
/// in the same tick and the new press plays on, as Thetis keeps the
/// transmitter through a press in its tail (build 283; in 282 the new press's
/// first words went with the burst, round 282).
/// Until build 282 the release was a control that arrived on its own, and the
/// whole buffer was played out at once when it came (build 166). A frame with
/// only the flag has no audio and gives none, as in Thetis's tick.
///
/// `ring` is the level of what the frames go to and the mark below which a
/// second frame is pulled (`pull_extra_for_ring_below`): a radio's sound card
/// in ms against the over's lead target (build 278), Thetis's TCI ring in
/// samples against `TX_RING_LOW` (build 193). With more in the buffer than its
/// target, below the mark, a second frame - the first pull, the rule on the
/// depth after it, then the second. `None`, or a mark of 0, never pulls a
/// second frame.
pub fn play_tick(
    jitter: &mut JitterBuffer,
    dec: &mut StreamDecoder,
    loss: &mut TxLoss,
    trim: &mut PauseTrim,
    now: Instant,
    ring: Option<(u64, u64)>,
) -> Played {
    // The label apart, so the closure does not hold `loss` across the pulls.
    let label = loss.label;
    let decode = |opus: &[u8], dec: &mut StreamDecoder| {
        let pcm = dec.decode(opus, true);
        if pcm.is_none() {
            warn!("{} TX decode error", label);
        }
        pcm
    };
    let mut let_go = false;
    let mut out = Played::default();
    for pull in 0..2 {
        if pull == 1 {
            let more = ring.is_some_and(|(level, lead)| {
                crate::tci::pull_extra_for_ring_below(lead as usize, Some(level as usize), jitter.depth(), jitter.target_depth())
            });
            if !more {
                break;
            }
            loss.count_extra();
        }
        let pcm = loop {
            break match jitter.pull() {
                JitterResult::Frame(frame) if !frame.ptt && jitter.holds_a_press() => {
                    out.pressed_again = true;
                    continue;
                }
                JitterResult::Frame(frame) if !frame.ptt => {
                    out.release = Some(frame.sequence);
                    out.release_depth = jitter.depth();
                    jitter.discard();
                    None
                }
                JitterResult::Frame(frame) => {
                    // A frame with the flag: the press plays - also one with
                    // only the flag, and one the catching up lets go (build
                    // 288: Thetis keyed on each, before its decode).
                    out.pressed = true;
                    if frame.opus_data.is_empty() {
                        None
                    } else {
                        match decode(&frame.opus_data, dec) {
                            Some(pcm) => {
                                let backlog = crate::pause_trim::backlog(jitter.depth(), jitter.target_depth());
                                if trim.let_go(&pcm, backlog, !let_go, now) {
                                    let_go = true;
                                    continue;
                                }
                                Some(pcm)
                            }
                            None => None,
                        }
                    }
                }
                JitterResult::Missing => fill_gap(dec, jitter, loss),
                JitterResult::NotReady => None,
            };
        };
        out.pcm.extend(pcm);
        if out.release.is_some() {
            break;
        }
    }
    loss.note_buffer(jitter);
    out
}

/// A radio's transmit audio: packets in (sequence, timestamp, Opus) - the
/// timestamp lets the buffer grow on a jittery line as Thetis's does (build
/// 167; review of 166: without it the target stayed at 3) -, a jitter buffer
/// played every 20 ms like Thetis's, gaps filled by `fill_gap`, and the result
/// resampled to the sound card's rate and handed to the bridge. The release
/// frame plays in its place in the stream, after the last word, and is told
/// to the network loop (`ReleasePlayed`, build 282). One task for both radios
/// (build 166; there were two copies).
pub async fn radio_tx_task(
    label: &'static str,
    target: crate::session::TxTarget,
    mut pkt_rx: tokio::sync::mpsc::Receiver<TxPacket>,
    tx_audio: tokio::sync::mpsc::Sender<Vec<f32>>,
    gain_shared: Arc<AtomicU32>,
    timing: Option<TxTiming>,
    tx_rate: u32,
    released: tokio::sync::mpsc::UnboundedSender<ReleasePlayed>,
) {
    let mut decoder = match StreamDecoder::new() {
        Ok(d) => d,
        Err(e) => {
            log::error!("{} TX decoder init: {}", label, e);
            return;
        }
    };
    let mut to_radio = match resampler(NETWORK_SAMPLE_RATE_WIDEBAND, tx_rate, FRAME_SAMPLES_WIDEBAND, UpFilter::Long) {
        Ok(r) => r,
        Err(e) => {
            log::error!("{} TX resampler init: {}", label, e);
            return;
        }
    };
    info!(
        "{} TX task started (wideband Opus 16kHz -> {}Hz, jitter buffer as Thetis); {}",
        label, tx_rate, to_radio
    );
    // Thetis's buffer and its catching up in the pauses (build 263; until then
    // the old numbers, 3-20, and a trim wherever it fell).
    let mut jitter = JitterBuffer::for_transmit(TX_MIN_DEPTH, TX_MAX_DEPTH);
    let mut trim = PauseTrim::new(Instant::now());
    // Named in advance (working rule 11, build 263).
    dev_info!("{}", buffer_line(label, None));
    let mut loss = TxLoss::new(label);
    let mut tick = tokio::time::interval(Duration::from_millis(20));
    let start = Instant::now();
    let mut over = timing.as_ref().map_or(0, |t| t.over());
    let mut sender: Option<std::net::SocketAddr> = None;
    let mut clock = PacketClock::default();
    let send = |pcm: Vec<i16>, resampler: &mut Resampler| {
        let gain = f32::from_bits(gain_shared.load(Ordering::Relaxed));
        let pcm_f32: Vec<f32> = pcm.iter().map(|&s| (s as f32 / 32768.0 * gain).clamp(-1.0, 1.0)).collect();
        match resampler.process(pcm_f32) {
            Ok(out) => {
                if !out.is_empty() {
                    let _ = tx_audio.try_send(out);
                }
            }
            Err(e) => warn!("{} TX resample: {}", label, e),
        }
    };
    loop {
        tokio::select! {
            pkt = pkt_rx.recv() => {
                let Some(TxPacket { from, sequence: seq, timestamp: ts, opus, ptt }) = pkt else { break };
                let silence = clock.packet(Instant::now());
                // Another sender - another operator, or the same one on a new
                // address after a network switch: what was learned about the
                // old line goes, as Thetis resets on a new holder (build 169;
                // review of 168).
                if sender != Some(from) {
                    sender = Some(from);
                    jitter.reset();
                }
                if let Some(t) = &timing {
                    // Audio, not a packet with only the flag (build 282).
                    if !opus.is_empty() {
                        t.tx_audio();
                    }
                    // A new transmission starts with an empty buffer, but keeps
                    // what the buffer learned about the line, as Thetis's does
                    // across one operator's transmissions (build 168; review
                    // of 167: a reset forgot it at every press).
                    match before_push(over, t.over(), silence) {
                        BeforePush::NewOver => {
                            over = t.over();
                            jitter.discard();
                        }
                        BeforePush::Silence(silence) => {
                            // Named in advance (working rule 11, build 264).
                            info!(
                                "{} TX: silent for {} ms inside a press, past the packet window - its buffered audio goes",
                                label,
                                silence.as_millis()
                            );
                            jitter.discard();
                        }
                        BeforePush::Keep => {}
                    }
                }
                let arrival = start.elapsed().as_millis() as u64;
                jitter.push(BufferedFrame { sequence: seq, timestamp: ts, opus_data: opus, ptt, wideband: true }, arrival);
            }
            _ = tick.tick() => {
                // The ring when the tick begins, against the over's lead
                // (build 278), as Thetis's tick reads its ring first.
                let ring = timing.as_ref().and_then(|t| t.ring_ms().map(|level| (level, t.lead_ms())));
                let played = play_tick(&mut jitter, &mut decoder, &mut loss, &mut trim, Instant::now(), ring);
                for pcm in played.pcm {
                    send(pcm, &mut to_radio);
                }
                if played.pressed_again {
                    // Named in advance (working rule 11, build 283).
                    info!("{} TX: pressed again behind the release frame - it plays on, the radio stays keyed", label);
                }
                // After the frames before it are with the bridge.
                if let (Some(sequence), Some(from)) = (played.release, sender) {
                    let _ = released.send(ReleasePlayed { target, from, sequence, over });
                }
                if let Some((in_pause, fallback)) = trim.counts_to_say(Instant::now()) {
                    // Named in advance (working rule 11, build 263).
                    dev_info!("{}", pause_line(label, in_pause, fallback));
                }
                loss.maybe_log();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sdr_remote_core::codec::OpusEncoderWideband;

    fn speechlike(n: usize, seed: &mut u32) -> Vec<i16> {
        (0..n)
            .map(|i| {
                *seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                let noise = ((*seed >> 16) as i16) / 8;
                let tone = ((i as f32 * 0.07).sin() * 6000.0) as i16;
                noise.saturating_add(tone)
            })
            .collect()
    }

    /// A lost microphone frame with the next packet buffered comes back from
    /// the redundancy the client always sends; with nothing buffered after it,
    /// it is concealed; both counted (build 166).
    #[test]
    fn a_lost_microphone_frame_is_rebuilt_from_the_next_packet() {
        // The client's transmit encoder: FEC always on.
        let mut enc = OpusEncoderWideband::new().unwrap();
        let mut seed = 7u32;
        let pkts: Vec<Vec<u8>> = (0..40).map(|_| enc.encode(&speechlike(FRAME_SAMPLES_WIDEBAND, &mut seed)).unwrap()).collect();
        let rebuilt_at = (20..39)
            .find(|&i| sdr_remote_core::codec::carries_redundancy(&pkts[i + 1]))
            .expect("the transmit encoder carries redundancy");
        let mut dec = StreamDecoder::new().unwrap();
        let mut jb = JitterBuffer::for_transmit(TX_MIN_DEPTH, TX_MAX_DEPTH);
        let mut loss = TxLoss::new("test");
        // Everything up to the lost one played, the one after it buffered.
        for (i, p) in pkts.iter().enumerate().take(rebuilt_at) {
            jb.push(BufferedFrame { sequence: i as u32, timestamp: 0, opus_data: p.clone(), ptt: true, wideband: false }, i as u64 * 20);
        }
        jb.push(BufferedFrame { sequence: rebuilt_at as u32 + 1, timestamp: 0, opus_data: pkts[rebuilt_at + 1].clone(), ptt: true, wideband: false }, 0);
        let mut missing = 0;
        loop {
            match jb.pull() {
                JitterResult::Frame(f) => {
                    dec.decode(&f.opus_data, true).unwrap();
                }
                JitterResult::Missing => {
                    missing += 1;
                    let pcm = fill_gap(&mut dec, &jb, &mut loss).expect("a frame for the gap");
                    assert_eq!(pcm.len(), FRAME_SAMPLES_WIDEBAND, "rebuilt by the wideband decoder (build 167)");
                    break;
                }
                JitterResult::NotReady => panic!("the buffer ran dry before the gap"),
            }
        }
        assert_eq!((missing, loss.rebuilt, loss.concealed), (1, 1, 0), "rebuilt, though the packet flag said narrowband");
        // Nothing after the gap: concealed.
        let mut jb2 = JitterBuffer::for_transmit(TX_MIN_DEPTH, TX_MAX_DEPTH);
        assert!(fill_gap(&mut dec, &jb2, &mut loss).is_some());
        assert_eq!((loss.rebuilt, loss.concealed), (1, 1));
        jb2.reset();
    }

    /// A tick plays one frame after the jitter buffer's fill; a frame with only
    /// the flag gives nothing; the release frame - the first without the flag -
    /// plays in its place behind the last word, is said once, and the rest of
    /// its burst goes (build 282; until then the release came as a control and
    /// played the buffer out at once, build 166). Twice without a new buffer
    /// (working rule 9), the second over on the same sequence run.
    #[test]
    fn the_release_frame_plays_behind_the_last_word() {
        let mut enc = OpusEncoderWideband::new().unwrap();
        let mut seed = 3u32;
        let mut jb = JitterBuffer::for_transmit(TX_MIN_DEPTH, TX_MAX_DEPTH);
        let mut dec = StreamDecoder::new().unwrap();
        let mut loss = TxLoss::new("test");
        let (mut trim, now) = (PauseTrim::new(Instant::now()), Instant::now());
        let mut seq = 0u32;
        for pass in 0..2 {
            // The press's flag first, then five words, then the release burst.
            let push = |jb: &mut JitterBuffer, seq: &mut u32, opus: Vec<u8>, ptt: bool| {
                jb.push(BufferedFrame { sequence: *seq, timestamp: *seq * 20, opus_data: opus, ptt, wideband: true }, *seq as u64 * 20);
                *seq += 1;
            };
            push(&mut jb, &mut seq, vec![], true);
            for _ in 0..5 {
                let opus = enc.encode(&speechlike(FRAME_SAMPLES_WIDEBAND, &mut seed)).unwrap();
                push(&mut jb, &mut seq, opus, true);
            }
            let release = seq;
            for _ in 0..5 {
                push(&mut jb, &mut seq, vec![], false);
            }
            let mut ticks = Vec::new();
            let mut said = Vec::new();
            let mut pressed = Vec::new();
            let mut behind = Vec::new();
            for _ in 0..12 {
                let p = play_tick(&mut jb, &mut dec, &mut loss, &mut trim, now, None);
                ticks.push(p.pcm.len());
                said.extend(p.release);
                pressed.push(p.pressed);
                if p.release.is_some() {
                    behind.push(p.release_depth);
                }
            }
            assert_eq!(ticks, vec![0, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0], "pass {pass}: the flag, five words, then the release");
            // Thetis keys on these (build 288): the flag's tick and the
            // words', not the release's or after.
            assert_eq!(
                pressed,
                vec![true, true, true, true, true, true, false, false, false, false, false, false],
                "pass {pass}: a frame with the flag played"
            );
            // Thetis's tail is reckoned on the burst behind the release, before
            // it goes (build 288, as its own tick did).
            assert_eq!(behind, vec![4], "pass {pass}: four of the burst stood behind it");
            assert_eq!(said, vec![release], "pass {pass}: the release frame, once");
            assert_eq!(jb.depth(), 0, "pass {pass}: the rest of the burst went");
        }
        // A tick whose first frame is the release pulls no second one for the
        // ring when only the burst stands behind it.
        let opus = enc.encode(&speechlike(FRAME_SAMPLES_WIDEBAND, &mut seed)).unwrap();
        for i in 0..5u32 {
            jb.push(BufferedFrame { sequence: seq + i, timestamp: 0, opus_data: vec![], ptt: false, wideband: true }, 0);
        }
        let p = play_tick(&mut jb, &mut dec, &mut loss, &mut trim, now, Some((0, 80)));
        assert_eq!((p.pcm.len(), p.release, p.pressed_again, jb.depth()), (0, Some(seq), false, 0), "the release stops the tick, the burst goes");
        seq += 5;
        // Pressed again within the buffer (build 283; round 282): the release
        // and its burst are passed over in the same tick, the new press plays
        // on, nothing is said - twice.
        for pass in 0..2 {
            for i in 0..3u32 {
                jb.push(BufferedFrame { sequence: seq + i, timestamp: 0, opus_data: vec![], ptt: false, wideband: true }, 0);
            }
            for i in 3..6u32 {
                jb.push(BufferedFrame { sequence: seq + i, timestamp: 0, opus_data: opus.clone(), ptt: true, wideband: true }, 0);
            }
            let p = play_tick(&mut jb, &mut dec, &mut loss, &mut trim, now, None);
            assert_eq!((p.pcm.len(), p.release, p.pressed_again, p.pressed), (1, None, true, true), "pass {pass}: the new press plays in the release's tick, and keys");
            let (q, r) = (
                play_tick(&mut jb, &mut dec, &mut loss, &mut trim, now, None),
                play_tick(&mut jb, &mut dec, &mut loss, &mut trim, now, None),
            );
            assert_eq!((q.pcm.len(), r.pcm.len(), q.release, r.release, jb.depth()), (1, 1, None, None, 0), "pass {pass}: all of it");
            seq += 6;
        }
    }

    /// The lead's rebuild (build 278): a ring below the over's target, with
    /// more in the buffer than its target, gives a second frame and counts it;
    /// a ring at the target, a target of 0, or no ring (not keyed) gives one -
    /// both settings (working rule 13). Never more than the buffer's excess.
    #[test]
    fn a_ring_below_its_lead_takes_a_second_frame() {
        let run_with = |frames: u32, ring: Option<(u64, u64)>| {
            let mut enc = OpusEncoderWideband::new().unwrap();
            let mut seed = 9u32;
            let mut jb = JitterBuffer::for_transmit(TX_MIN_DEPTH, TX_MAX_DEPTH);
            let (mut dec, mut loss) = (StreamDecoder::new().unwrap(), TxLoss::new("test"));
            let (mut trim, now) = (PauseTrim::new(Instant::now()), Instant::now());
            for seq in 0..frames {
                let opus = enc.encode(&speechlike(FRAME_SAMPLES_WIDEBAND, &mut seed)).unwrap();
                jb.push(BufferedFrame { sequence: seq, timestamp: seq * 20, opus_data: opus, ptt: true, wideband: true }, seq as u64 * 20);
            }
            let first = play_tick(&mut jb, &mut dec, &mut loss, &mut trim, now, ring).pcm.len();
            let mut rest = vec![first];
            while jb.depth() > 0 {
                rest.push(play_tick(&mut jb, &mut dec, &mut loss, &mut trim, now, ring).pcm.len());
            }
            (rest, loss.extra, jb.target_depth())
        };
        let run = |ring: Option<(u64, u64)>| run_with(8, ring);
        let (ticks, extra, target) = run(Some((0, 80)));
        assert_eq!(target, TX_MIN_DEPTH, "the buffer's target these counts are made for");
        // Eight frames, target two: after each first pull 7, 5, 3 stand - more
        // than the target, a second - then 1, which is not (review of 278: a
        // bound of `twos <= 8 - target` could not fail).
        assert_eq!(ticks, vec![2, 2, 2, 1, 1], "only the excess over the buffer's target");
        assert_eq!(extra, 3, "each counted for the 10 s line");
        // One over the target: after the first pull exactly the target stands,
        // so no second - the depth after the pull, and `>` not `>=`.
        let (ticks, extra, _) = run_with(TX_MIN_DEPTH + 1, Some((0, 80)));
        assert_eq!((ticks[0], extra), (1, 0), "the target stays in the buffer: {ticks:?}");
        for (ring, why) in [(Some((80, 80)), "the lead stands"), (Some((0, 0)), "target 0"), (None, "not keyed")] {
            let (ticks, extra, _) = run(ring);
            assert_eq!((ticks[0], extra), (1, 0), "{why}: one frame, as before build 278");
        }
    }

    /// A radio's buffer catches up as Thetis's does (build 263): with a backlog
    /// a quiet frame goes and the next is played in its place, so a tick still
    /// gives one frame; speech is never let go before the floor is learned.
    #[test]
    fn a_radio_lets_a_quiet_frame_go_with_a_backlog() {
        let mut enc = OpusEncoderWideband::new().unwrap();
        let mut seed = 5u32;
        let mut frame = |seq: u32| {
            let pcm = speechlike(FRAME_SAMPLES_WIDEBAND, &mut seed);
            // a pause every fourth frame: the same noise, fifty times softer
            let pcm: Vec<i16> = if seq % 4 == 0 { pcm.iter().map(|&v| v / 50).collect() } else { pcm };
            enc.encode(&pcm).unwrap()
        };
        let mut jb = JitterBuffer::for_transmit(TX_MIN_DEPTH, TX_MAX_DEPTH);
        let (mut dec, mut loss) = (StreamDecoder::new().unwrap(), TxLoss::new("test"));
        let t0 = Instant::now();
        let mut trim = PauseTrim::new(t0);
        // A backlog of eight frames, then one a tick at the line's pace.
        let push = |jb: &mut JitterBuffer, seq: u32, opus: Vec<u8>, at: u64| {
            jb.push(BufferedFrame { sequence: seq, timestamp: seq * 20, opus_data: opus, ptt: true, wideband: true }, at)
        };
        for seq in 0..8u32 {
            push(&mut jb, seq, frame(seq), 0);
        }
        let mut out = Vec::new();
        for i in 0..100u32 {
            push(&mut jb, 8 + i, frame(8 + i), 20 * i as u64);
            out.push(play_tick(&mut jb, &mut dec, &mut loss, &mut trim, t0 + Duration::from_millis(20 * i as u64), None).pcm.len());
        }
        let (in_pause, fallback) = trim.counts_to_say(t0 + Duration::from_secs(11)).expect("frames were let go");
        assert!(in_pause > 0 && fallback == 0, "in a pause {in_pause}, by the fallback {fallback}");
        assert!(out[2..].iter().all(|&n| n == 1), "one frame a tick, let go or not: {out:?}");
        assert!(jb.depth() < 8, "the backlog went: depth {}", jb.depth());
    }

    /// The start and pause lines of Thetis and the radios come from one place,
    /// and say what the field log of 2026-10-20 says (build 263).
    #[test]
    fn the_buffer_lines_say_what_was_named() {
        assert_eq!(
            buffer_line("Thetis", Some(Duration::from_millis(100))),
            "Thetis TX buffer: depth 2-10, 30 ms per frame, spike decay 115, ring hold 100 ms; catching up in the pauses (within 4 dB of the noise floor, fallback after 1000 ms)"
        );
        assert_eq!(
            buffer_line("Radio 1", None),
            "Radio 1 TX buffer: depth 2-10, 30 ms per frame, spike decay 115; catching up in the pauses (within 4 dB of the noise floor, fallback after 1000 ms)"
        );
        assert_eq!(pause_line("Radio 2", 1, 0), "Radio 2 TX buffer (10 s): 1 frame(s) let go in a pause, 0 by the fallback after 1000 ms");
    }

    /// Thetis's loop and the radios' task have the same buffer and catch up the
    /// same way: the two places that must agree, pinned (working rule 15,
    /// build 263). A shared playout step would be one source; Thetis's loop has
    /// its PTT machine between the pull and the decode.
    #[test]
    fn thetis_and_the_radios_share_the_buffer_and_its_catching_up() {
        let network = include_str!("network.rs");
        let task = include_str!("tx_recover.rs");
        let task = &task[..task.find("#[cfg(test)]").unwrap()];
        for (src, what) in [
            (network, "JitterBuffer::for_transmit(TX_MIN_DEPTH, TX_MAX_DEPTH)"),
            (task, "JitterBuffer::for_transmit(TX_MIN_DEPTH, TX_MAX_DEPTH)"),
            // One playout step since build 288: the catching up is in it, and
            // Thetis's loop hands it its own trim.
            (task, "let backlog = crate::pause_trim::backlog(jitter.depth(), jitter.target_depth());"),
            (task, "if trim.let_go(&pcm, backlog, !let_go, now) {"),
            (network, "crate::tx_recover::play_tick(&mut jitter_buf, &mut opus_decoder, &mut thetis_tx_loss, &mut pause_trim, Instant::now(), ring);"),
            (network, "crate::tx_recover::buffer_line(\"Thetis\", Some(crate::tci::TX_HOLD_MAX))"),
            (task, "info!(\"{}\", buffer_line(label, None));"),
            (network, "crate::tx_recover::pause_line(\"Thetis\", in_pause, fallback)"),
            (task, "info!(\"{}\", pause_line(label, in_pause, fallback));"),
        ] {
            assert_eq!(src.matches(what).count(), 1, "{what}");
        }
        assert_eq!(task.matches("JitterBuffer::new(").count(), 0, "no receive buffer for a radio's transmit audio");
    }

    /// A radio's packet clock says a silence only past the safety's packet
    /// window, the same comparison as Thetis's; the first packet has none
    /// (build 264).
    #[test]
    fn a_radio_hears_a_silence_past_the_packet_window() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut c = PacketClock::default();
        assert_eq!(c.packet(at(0)), None, "the first packet");
        assert_eq!(c.packet(at(20)), None);
        assert_eq!(c.packet(at(2_020)), None, "the window itself still plays");
        assert_eq!(c.packet(at(4_021)), Some(Duration::from_millis(2_001)));
        assert_eq!(c.packet(at(4_041)), None, "once");
    }

    /// Inside one press the silence empties the buffer before the packet goes
    /// in; a new over empties it already and says nothing of a silence (build
    /// 264). The task's packet arm, which no test drives.
    #[test]
    fn a_silence_inside_a_press_empties_a_radios_buffer() {
        let task = include_str!("tx_recover.rs");
        let task = &task[..task.find("#[cfg(test)]").unwrap()];
        let arm = &task[task.find("pkt = pkt_rx.recv() => {").unwrap()..task.find("_ = tick.tick() => {").unwrap()];
        let clock = arm.find("let silence = clock.packet(Instant::now());").expect("every packet goes by the clock");
        let decide = arm.find("match before_push(over, t.over(), silence) {").expect("the decision");
        let rule = arm.find("BeforePush::Silence(silence) => {").unwrap();
        let discard = rule + arm[rule..].find("jitter.discard();").unwrap();
        let push = arm.find("jitter.push(").unwrap();
        assert!(clock < decide && decide < rule && discard < push, "clock, decision, then the push");
        assert!(discard < arm.find("BeforePush::Keep => {}").unwrap(), "the silence's own discard");
        assert_eq!(arm.matches("jitter.discard();").count(), 2, "a new over, and a silence");
        assert_eq!(task.matches("clock.packet(").count(), 1);
        // The decision itself.
        let d = Some(Duration::from_millis(2_500));
        assert_eq!(before_push(3, 3, None), BeforePush::Keep);
        assert_eq!(before_push(3, 3, d), BeforePush::Silence(Duration::from_millis(2_500)));
        assert_eq!(before_push(3, 4, None), BeforePush::NewOver);
        assert_eq!(before_push(3, 4, d), BeforePush::NewOver, "a new over says nothing of a silence");
    }

    /// The line comes every ten seconds - a slower one would not show in a
    /// field log at all (build 167).
    #[test]
    fn the_loss_line_comes_every_ten_seconds() {
        assert_eq!(LOSS_EVERY, Duration::from_secs(10));
    }

    /// With the packets' timestamps the radio's buffer grows on a line that
    /// stalls and then delivers in a burst, as Thetis's does; without them it
    /// stayed at 3 and dropped the burst (build 167; review of 166).
    #[test]
    fn a_radio_buffer_grows_on_a_stalling_line() {
        let mut jb = JitterBuffer::for_transmit(TX_MIN_DEPTH, TX_MAX_DEPTH);
        let mut seq = 0u32;
        // Every second, 160 ms of nothing and then everything at once.
        for round in 0..10u64 {
            for i in 0..50u64 {
                let sent = round * 1000 + i * 20;
                let arrival = if i < 8 { round * 1000 + 160 } else { sent };
                jb.push(BufferedFrame { sequence: seq, timestamp: sent as u32, opus_data: vec![0], ptt: true, wideband: true }, arrival);
                seq += 1;
            }
        }
        assert!(jb.target_depth() > TX_MIN_DEPTH, "target {}", jb.target_depth());
    }

    /// The buffer line says the target, only when it rose above the minimum,
    /// and starts again after each line (build 168; review of 167).
    #[test]
    fn the_buffer_line_says_how_far_the_target_rose() {
        let mut loss = TxLoss::new("test");
        let mut jb = JitterBuffer::for_transmit(TX_MIN_DEPTH, TX_MAX_DEPTH);
        for seq in 0..6u32 {
            jb.push(BufferedFrame { sequence: seq, timestamp: seq * 20, opus_data: vec![0], ptt: true, wideband: true }, seq as u64 * 20);
        }
        loss.note_buffer(&jb);
        assert_eq!(loss.buffer_grew(), None, "a deep buffer at the minimum target is not growth");
        // A stall: one packet 270 ms late raises the target.
        jb.push(BufferedFrame { sequence: 6, timestamp: 120, opus_data: vec![0], ptt: true, wideband: true }, 120 + 270);
        loss.note_buffer(&jb);
        let (target, depth) = loss.buffer_grew().expect("the target rose");
        assert!(target > TX_MIN_DEPTH);
        assert_eq!((target, depth), (jb.target_depth(), 7));
        loss.since = Instant::now() - LOSS_EVERY;
        loss.maybe_log();
        assert_eq!(loss.buffer_grew(), None, "a new window starts from nothing");
        assert_eq!(loss.depth_max, 0, "the depth too (review of 168, N4)");
        // Between transmissions: the learned target stays, nothing is buffered.
        jb.drain_all();
        assert!(jb.target_depth() > TX_MIN_DEPTH);
        loss.note_buffer(&jb);
        assert_eq!(loss.buffer_grew(), None, "an idle buffer is not reported");
    }

    /// What the buffer threw away reaches the line per window, from the
    /// buffer's own totals, also when it is empty at the look; and the next
    /// window starts from nothing (build 183).
    #[test]
    fn the_thrown_away_line_counts_per_window() {
        let mut loss = TxLoss::new("test");
        let mut jb = JitterBuffer::for_transmit(TX_MIN_DEPTH, TX_MAX_DEPTH);
        loss.note_buffer(&jb);
        assert_eq!(loss.thrown_away(), None, "nothing thrown away, no line");
        // A burst after a stall: 40 frames at once, the buffer keeps 20 (its
        // maximum of 10 and the cap above it).
        for seq in 0..40u32 {
            jb.push(BufferedFrame { sequence: seq, timestamp: seq * 20, opus_data: vec![0], ptt: true, wideband: true }, 0);
        }
        // Looked at only once it is empty again (the release plays it out):
        // the count must not wait for a full buffer.
        jb.drain_all();
        loss.note_buffer(&jb);
        assert_eq!(loss.thrown_away(), Some([20, 0, 0, 0, 0]));
        // Counted once: a second look at the same totals adds nothing.
        loss.note_buffer(&jb);
        assert_eq!(loss.thrown_away(), Some([20, 0, 0, 0, 0]));
        // Every column to its own place (review of 183, T3).
        jb.stats_trimmed += 2;
        jb.stats_late += 3;
        jb.stats_cleared += 4;
        jb.stats_skipped += 5;
        loss.note_buffer(&jb);
        assert_eq!(loss.thrown_away(), Some([20, 2, 3, 4, 5]));
        loss.count_extra();
        loss.count_extra();
        assert_eq!(loss.extra, 2);
        loss.since = Instant::now() - LOSS_EVERY;
        loss.maybe_log();
        assert_eq!(loss.thrown_away(), None, "a new window starts from nothing");
        assert_eq!(loss.extra, 0, "and so do the extra frames (build 193)");
    }

    /// The thrown-away line as named in `logregels-build184.md` (review of 183,
    /// T5: the field log leans on it).
    #[test]
    fn the_thrown_away_line_says_what_was_named() {
        let src = include_str!("tx_recover.rs");
        let src = &src[..src.find("#[cfg(test)]").unwrap()];
        assert!(src.contains("info!(\"{} TX ring (10 s): {} extra frame(s) pulled to rebuild the lead\", self.label, self.extra);"));
        assert!(src.contains("\"{} TX audio (10 s): {} frame(s) thrown away - {} on arrival above the cap, {} trimmed to catch up, {} arrived too late, {} cleared at a new over or sender; {} not there in time (skipped over)\","));
        assert!(src.contains("arrival + trimmed + late + cleared,"));
    }

    /// Both loops reach the one gap rule and the one task: Thetis's missing
    /// arm and both radios (a text guard, build 166).
    #[test]
    fn thetis_and_both_radios_go_through_the_one_transmit_path() {
        let network = include_str!("network.rs");
        // Thetis's gap goes through the one playout step since build 288.
        assert_eq!(network.matches("crate::tx_recover::play_tick(&mut jitter_buf, &mut opus_decoder, &mut thetis_tx_loss, &mut pause_trim, Instant::now(), ring);").count(), 1);
        assert_eq!(network.matches("fill_gap(").count(), 0, "no gap of its own");
        assert_eq!(network.matches("jitter_buf.pull()").count(), 0, "no pull of its own");
        assert_eq!(network.matches("crate::tx_recover::radio_tx_task(").count(), 2, "both radios");
        assert_eq!(network.matches("opus_decoder.decode_plc()").count(), 0, "no second way to fill a gap");
        assert_eq!(network.matches("tx.try_send(crate::tx_recover::TxPacket { from: addr, sequence: pkt.sequence, timestamp: pkt.timestamp, opus: pkt.opus_data, ptt })").count(), 1, "sender, sequence, timestamp and flag go along, for both radios in one block");
        assert_eq!(network.matches("radio_audio!(pkt, addr, ").count(), 2, "both radios through it");
        // The task's wiring, which no test drives (review of 166).
        let task = include_str!("tx_recover.rs");
        let task = &task[..task.find("#[cfg(test)]").unwrap()];
        for wired in [
            "timestamp: ts, opus_data: opus",
            "let played = play_tick(&mut jitter, &mut decoder, &mut loss, &mut trim, Instant::now(), ring);",
            "let _ = released.send(ReleasePlayed { target, from, sequence, over });",
            "jitter.push(BufferedFrame { sequence: seq, timestamp: ts, opus_data: opus, ptt, wideband: true }, arrival);",
            "loss.note_buffer(jitter);",
            "warn!(\"{} TX decode error\", label);",
        ] {
            assert_eq!(task.matches(wired).count(), 1, "{wired}");
        }
        assert!(task.contains("BeforePush::NewOver => {
                            over = t.over();
                            jitter.discard();"), "a new transmission starts empty, and keeps what was learned");
        assert_eq!(task.matches("jitter.reset()").count(), 1, "only for another sender");
        assert!(task.contains("if sender != Some(from) {
                    sender = Some(from);
                    jitter.reset();"), "another sender forgets the line");
        // Nothing else writes the sender, and the reset comes before the
        // packet goes in (review of 169: S1, S2, X7).
        assert_eq!(task.matches("sender = ").count(), 1);
        assert_eq!(task.matches("sender.").count(), 0);
        assert!(task.find("if sender != Some(from) {").unwrap() < task.find("jitter.push(").unwrap());
        // Thetis reports its buffer too: through the one playout step since build 288.
        let step = &task[task.find("pub fn play_tick(").unwrap()..task.find("pub async fn radio_tx_task(").unwrap()];
        assert_eq!(step.matches("loss.note_buffer(jitter);").count(), 1, "the step reports the buffer, Thetis's and a radio's");
        assert_eq!(network.matches("note_buffer(").count(), 0, "not a second time in Thetis's loop");
        // The 991A waits for a quiet line only where it reads its menus.
        let poll = include_str!("yaesu/poll.rs");
        let ensure = &poll[poll.find("fn ensure_991a_usb_routing_snapshot(").unwrap()..];
        let ensure = &ensure[..ensure.find("\n}\n").unwrap()];
        assert_eq!(ensure.matches("settle_line(port, spill);").count(), 3, "before each read");
        assert_eq!(ensure.matches("spill_waiting").count(), 0);
        let arm = &poll[poll.find("PerPttRouting::Apply(scope) => {").unwrap()..];
        assert!(arm[..arm.find("apply_991a_usb_routing(").unwrap()].find("settle_line").is_none(), "no wait before every TX1");
    }
}
