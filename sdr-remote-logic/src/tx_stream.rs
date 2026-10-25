// SPDX-License-Identifier: GPL-2.0-or-later

//! One transmit stream to the server, per transmitter (build 291, step 2a of
//! `KOPPELING-ZONDER-MECHANISME.md` point 18): Thetis and each Yaesu. What
//! every packet of the microphone's audio carries - its type, its sequence,
//! its PTT flag - and the burst of packets after the flag changes, built in
//! one place. Until 291 the engine built them in five places, Thetis's three
//! (microphone, WAV, flag only) with one counter and the radios' two with
//! another, and burst in two ways.
//!
//! One way to burst since build 292, Thetis's (owner, 2026-10-23): a change of
//! the flag is seen at the audio tick; while the burst lasts every audio frame
//! goes twice (same sequence); a tick with no audio sends one packet with only
//! the flag. Until 292 a radio sent a packet with only the flag at once on the
//! button and did not double its audio (build 282).
//!
//! What happens to the sound before it is encoded - EQ, compressor, AGC,
//! gain, the encoder and its bitrate - is each radio's own and stays in the
//! engine.

use sdr_remote_core::protocol::{AudioPacket, Flags, PacketType};

use crate::loss_window::Stream;

/// How many packets a change of the PTT flag is sent with.
pub const PTT_BURST_COUNT: u32 = 5;

/// One packet to send, as many times as `copies` says, and what the
/// measurement writes down for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outgoing {
    pub bytes: Vec<u8>,
    pub sequence: u32,
    pub opus_len: usize,
    pub copies: u8,
}

/// The transmit stream of one transmitter.
#[derive(Debug)]
pub struct TxStream {
    ptype: PacketType,
    stream: Stream,
    sequence: u32,
    /// The flag as last seen at a tick: what a change is measured against.
    flag: bool,
    burst: u32,
}

impl TxStream {
    pub fn new(ptype: PacketType, stream: Stream) -> Self {
        Self { ptype, stream, sequence: 0, flag: false, burst: 0 }
    }

    /// Thetis's stream.
    pub fn thetis() -> Self {
        Self::new(PacketType::Audio, Stream::Thetis)
    }

    /// A Yaesu's stream, by slot (0 or 1).
    pub fn yaesu(slot: usize) -> Self {
        match slot {
            0 => Self::new(PacketType::AudioYaesu, Stream::Radio1),
            _ => Self::new(PacketType::AudioYaesu2, Stream::Radio2),
        }
    }

    /// Where the measurement writes this stream's packets.
    pub fn stream(&self) -> Stream {
        self.stream
    }

    /// The sequence the next packet gets.
    pub fn next_sequence(&self) -> u32 {
        self.sequence
    }

    fn build(&mut self, on: bool, opus: Vec<u8>, timestamp: u32, copies: u8) -> Outgoing {
        let pkt = AudioPacket { flags: Flags::NONE.with_ptt(on), sequence: self.sequence, timestamp, opus_data: opus };
        self.sequence = self.sequence.wrapping_add(1);
        let mut bytes = Vec::with_capacity(AudioPacket::HEADER_SIZE + pkt.opus_data.len());
        pkt.serialize_as_type(&mut bytes, self.ptype);
        Outgoing { bytes, sequence: pkt.sequence, opus_len: pkt.opus_data.len(), copies }
    }

    /// The flag is `on` at this audio tick. Whether it changed; a change
    /// starts the burst. No packet of its own: the tick sends what follows.
    pub fn ptt(&mut self, on: bool) -> bool {
        if on == self.flag {
            return false;
        }
        self.flag = on;
        self.burst = PTT_BURST_COUNT;
        true
    }

    /// One encoded audio frame with the flag `on`: twice while the burst
    /// lasts, once after.
    pub fn audio(&mut self, on: bool, opus: Vec<u8>, timestamp: u32) -> Outgoing {
        let copies = if self.burst > 0 {
            self.burst -= 1;
            2
        } else {
            1
        };
        self.build(on, opus, timestamp, copies)
    }

    /// The end of an audio tick: while the burst lasts and no audio of this
    /// stream went in the tick, one packet with only the flag `on`. The caller
    /// asks only when it can send.
    pub fn tick_end(&mut self, on: bool, audio_sent: bool, timestamp: u32) -> Option<Outgoing> {
        if audio_sent || self.burst == 0 {
            return None;
        }
        self.burst -= 1;
        Some(self.build(on, Vec::new(), timestamp, 1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind(o: &Outgoing) -> (PacketType, bool, u32, usize, u8) {
        let p = AudioPacket::deserialize(&o.bytes).unwrap();
        let header = sdr_remote_core::protocol::Header::deserialize(&o.bytes).unwrap();
        assert_eq!(p.sequence, o.sequence);
        (header.packet_type, p.flags.ptt(), p.sequence, p.opus_data.len(), o.copies)
    }

    /// Every transmitter the same way (build 292, Thetis's): a change is a
    /// burst and no packet; the next five audio frames go twice; a tick
    /// without audio sends one with only the flag, and a tick with audio none.
    /// Thetis and both radios, each its own type; twice, the second press on
    /// the running sequence.
    #[test]
    fn every_transmitter_doubles_the_audio_of_its_burst() {
        for (mut s, ptype) in [
            (TxStream::thetis(), PacketType::Audio),
            (TxStream::yaesu(0), PacketType::AudioYaesu),
            (TxStream::yaesu(1), PacketType::AudioYaesu2),
        ] {
            let mut seq = 0;
            for press in 0..2 {
                assert!(s.ptt(true), "{ptype:?} press {press}: a change");
                assert!(!s.ptt(true), "{ptype:?} press {press}: no change, nothing");
                for i in 0..6 {
                    let o = s.audio(true, vec![1, 2, 3], 0);
                    let copies = if i < 5 { 2 } else { 1 };
                    assert_eq!(kind(&o), (ptype, true, seq, 3, copies), "{ptype:?} press {press}, frame {i}");
                    seq += 1;
                    assert_eq!(s.tick_end(true, true, 0), None, "audio went this tick");
                }
                assert_eq!(s.tick_end(true, false, 0), None, "the burst is spent");
                assert!(s.ptt(false));
                for i in 0..5 {
                    let o = s.tick_end(false, false, 0).expect("a packet with only the flag");
                    assert_eq!(kind(&o), (ptype, false, seq, 0, 1), "{ptype:?} release {press}, {i}");
                    seq += 1;
                }
                assert_eq!(s.tick_end(false, false, 0), None, "five, and no more");
            }
        }
    }

    /// A press without a microphone (the flag on packets of its own): five
    /// packets with the flag, then five without at the release - what the
    /// engine's test against a fake server sees.
    #[test]
    fn a_press_without_audio_sends_five_flags_each_way() {
        let mut s = TxStream::yaesu(0);
        s.ptt(true);
        let on: Vec<_> = (0..7).filter_map(|_| s.tick_end(true, false, 0)).collect();
        assert_eq!(on.len(), 5);
        s.ptt(false);
        let off: Vec<_> = (0..7).filter_map(|_| s.tick_end(false, false, 0)).collect();
        assert_eq!(off.len(), 5);
        assert!(on.iter().all(|o| kind(o).1) && off.iter().all(|o| !kind(o).1));
    }

    /// Each transmitter its own type and measurement stream.
    #[test]
    fn each_transmitter_has_its_own_stream() {
        assert_eq!((TxStream::thetis().ptype, TxStream::thetis().stream()), (PacketType::Audio, Stream::Thetis));
        assert_eq!((TxStream::yaesu(0).ptype, TxStream::yaesu(0).stream()), (PacketType::AudioYaesu, Stream::Radio1));
        assert_eq!((TxStream::yaesu(1).ptype, TxStream::yaesu(1).stream()), (PacketType::AudioYaesu2, Stream::Radio2));
    }
}
