// SPDX-License-Identifier: GPL-2.0-or-later
//! Step 4 of the transmit chain (`PLAN-stap4-proefopstelling.md`, build 256): a
//! development build of the server plays the TX arrivals of a drive into its own
//! receive path (`--inspelen <servermeting.csv>`), with a recording in the packets
//! (`--geluid`, build 257). From the receive path on everything is the real server
//! and the real Thetis; what the replay (`tx_replay`) predicts for the same
//! arrivals is laid next to what Thetis reports. Tuning B and catching up in the
//! pauses were trial options (`--zendafstelling B`, `--inlopen pauzes`) until they
//! became the server's own in build 258.
//!
//! Only with the cargo feature `naspelen` (owner, 2026-10-12: a developer's tool,
//! never in a release); compiled for the tests too, so it cannot rot.
#![cfg_attr(not(feature = "naspelen"), allow(dead_code))]

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use sdr_remote_core::protocol::{AudioPacket, Capabilities, Flags, Heartbeat, VersionTag};

use crate::tx_replay::Arrival;

/// The client the arrivals come from: TEST-NET-1 (RFC 5737), never a QUIC
/// sentinel (`198.51.100.0/24`), so what the server sends back goes nowhere.
pub const TRIAL_CLIENT: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 250)), 4580);

/// From the start of the server to the first arrival: time for TCI to Thetis.
pub const START_DELAY: Duration = Duration::from_secs(10);

/// A heartbeat this often, as a client sends them (`HEARTBEAT_INTERVAL_MS`).
pub const HEARTBEAT_EVERY: Duration = Duration::from_millis(500);

/// What a development start asked for.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Trial {
    /// A server measurement to play in.
    pub replay: Option<String>,
    /// A recording the played-in packets carry instead of the tone (build 257).
    pub sound: Option<String>,
}

static TRIAL: OnceLock<Trial> = OnceLock::new();

/// The trial of this start, if any (set once by `take_args`).
pub fn get() -> Option<&'static Trial> {
    TRIAL.get()
}

/// The trial's options, for the server's usage text (build 257).
pub const USAGE: &str = "Trial options (development build):\n  \
  --inspelen <servermeting.csv>  play the TX arrivals of a drive in, as a client\n  \
  --geluid <recording.wav>       the played-in packets carry this recording instead of a tone\n\
Options are lower case. Tuning B and catching up in the pauses are the server's own since build 258.";

/// Take the trial's options out of the command line and keep them; the rest goes
/// on to the server's own parsing, so a start with only these still opens the
/// settings screen.
pub fn take_args(args: Vec<String>) -> Result<Vec<String>, String> {
    let (trial, rest) = parse(args)?;
    if trial != Trial::default() {
        let _ = TRIAL.set(trial);
    }
    Ok(rest)
}

fn parse(args: Vec<String>) -> Result<(Trial, Vec<String>), String> {
    let mut trial = Trial::default();
    let mut rest = Vec::with_capacity(args.len());
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--inspelen" => trial.replay = Some(it.next().ok_or("--inspelen needs a server measurement (.csv)")?),
            "--geluid" => trial.sound = Some(it.next().ok_or("--geluid needs a recording (.wav)")?),
            "--zendafstelling" | "--inlopen" => {
                return Err(format!("{} is gone: tuning B and catching up in the pauses are the server's own since build 258", a))
            }
            _ => rest.push(a),
        }
    }
    Ok((trial, rest))
}

/// One thing to hand to the receive path, `at` after the first arrival.
#[derive(Debug, Clone, PartialEq)]
pub struct Delivery {
    pub at: Duration,
    pub bytes: Vec<u8>,
}

/// The arrivals as packets: each at its measured time after the first, with its
/// measured number, timestamp and PTT bit; with audio as the app sends it (Opus
/// wideband, 20 ms) when the measured packet carried audio, empty when it did not
/// (the release packets). A tone, so Thetis gets sound and not silence.
pub fn audio_sends(arrivals: &[Arrival], sound: Option<&[i16]>) -> anyhow::Result<Vec<Delivery>> {
    let Some(first) = arrivals.first().map(|a| a.mono_ms) else { return Ok(Vec::new()) };
    let mut encoder = sdr_remote_core::codec::OpusEncoderWideband::new()?;
    let mut phase = 0.0f32;
    let n = sdr_remote_core::FRAME_SAMPLES_WIDEBAND;
    let mut pcm = vec![0i16; n];
    let step = std::f32::consts::TAU * 800.0 / 16_000.0;
    let sound = sound.filter(|s| s.len() >= n);
    let mut at = 0usize;
    let mut out = Vec::with_capacity(arrivals.len());
    for a in arrivals {
        let opus_data = if a.bytes == 0 {
            Vec::new()
        } else {
            match sound {
                // The recording, 20 ms a packet, from the start again at its end.
                Some(s) => {
                    for v in pcm.iter_mut() {
                        *v = s[at];
                        at = (at + 1) % s.len();
                    }
                }
                None => {
                    for v in pcm.iter_mut() {
                        *v = (phase.sin() * 8_000.0) as i16;
                        phase = (phase + step) % std::f32::consts::TAU;
                    }
                }
            }
            encoder.encode(&pcm)?
        };
        let pkt = AudioPacket { flags: Flags::NONE.with_ptt(a.ptt), sequence: a.seq, timestamp: a.ts, opus_data };
        let mut bytes = Vec::new();
        pkt.serialize(&mut bytes);
        out.push(Delivery { at: Duration::from_millis(a.mono_ms - first), bytes });
    }
    Ok(out)
}

/// A 16-bit PCM WAV (mono or more channels, any rate) as mono at 16 kHz, the
/// rate the app records at (build 257). Linear interpolation: a test tool.
pub fn read_wav_16k(bytes: &[u8]) -> Result<Vec<i16>, String> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("not a WAV file".into());
    }
    let (mut channels, mut rate, mut bits, mut data) = (0u16, 0u32, 0u16, None);
    let mut i = 12;
    while i + 8 <= bytes.len() {
        let id = &bytes[i..i + 4];
        let len = u32::from_le_bytes([bytes[i + 4], bytes[i + 5], bytes[i + 6], bytes[i + 7]]) as usize;
        let body = &bytes[i + 8..(i + 8 + len).min(bytes.len())];
        if id == b"fmt " && body.len() >= 16 {
            let format = u16::from_le_bytes([body[0], body[1]]);
            if format != 1 && format != 0xFFFE {
                return Err(format!("WAV format {} is not PCM", format));
            }
            channels = u16::from_le_bytes([body[2], body[3]]);
            rate = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
            bits = u16::from_le_bytes([body[14], body[15]]);
        } else if id == b"data" {
            data = Some(body);
        }
        i += 8 + len + (len & 1);
    }
    let data = data.ok_or("no data in the WAV file")?;
    if bits != 16 || channels == 0 || rate == 0 {
        return Err(format!("only 16-bit PCM, not {} bit / {} channel(s) / {} Hz", bits, channels, rate));
    }
    let ch = channels as usize;
    let mono: Vec<f32> = data
        .chunks_exact(2 * ch)
        .map(|f| (0..ch).map(|c| i16::from_le_bytes([f[2 * c], f[2 * c + 1]]) as f32).sum::<f32>() / ch as f32)
        .collect();
    if mono.is_empty() {
        return Err("the WAV file is empty".into());
    }
    let out_len = (mono.len() as u64 * 16_000 / rate as u64) as usize;
    Ok((0..out_len)
        .map(|k| {
            let pos = k as f64 * rate as f64 / 16_000.0;
            let j = pos as usize;
            let frac = (pos - j as f64) as f32;
            let a = mono[j.min(mono.len() - 1)];
            let b = mono[(j + 1).min(mono.len() - 1)];
            (a + (b - a) * frac).round().clamp(-32768.0, 32767.0) as i16
        })
        .collect())
}

/// A heartbeat as the app sends it, with its PTT bit.
pub fn heartbeat(sequence: u32, local_ms: u32, ptt: bool) -> Vec<u8> {
    let hb = Heartbeat {
        flags: Flags::NONE.with_ptt(ptt),
        sequence,
        local_time: local_ms,
        rtt_ms: 0,
        loss_percent: 0,
        jitter_ms: 0,
        capabilities: Capabilities::NONE,
        version: Some(VersionTag::mine()),
    };
    let mut buf = [0u8; Heartbeat::SIZE_WITH_VERSION];
    hb.serialize_with_version(&mut buf);
    buf.to_vec()
}

/// The audio and, between them, a heartbeat every `HEARTBEAT_EVERY` - from the
/// start until the last packet - in the order they go. A heartbeat carries the
/// PTT bit of the audio packet before it.
pub fn schedule(audio: Vec<Delivery>, ptt_of: &[bool]) -> Vec<Delivery> {
    let end = audio.last().map_or(Duration::ZERO, |s| s.at);
    let mut all = Vec::with_capacity(audio.len() + (end.as_millis() / 500) as usize + 1);
    let mut next_hb = Duration::ZERO;
    let mut seq = 0u32;
    let mut ptt = false;
    for (i, s) in audio.into_iter().enumerate() {
        while next_hb <= s.at {
            all.push(Delivery { at: next_hb, bytes: heartbeat(seq, next_hb.as_millis() as u32, ptt) });
            seq = seq.wrapping_add(1);
            next_hb += HEARTBEAT_EVERY;
        }
        ptt = ptt_of.get(i).copied().unwrap_or(false);
        all.push(s);
    }
    all
}

/// Hand `sends` to the receive path from a thread of its own, each at its time
/// after `start`: sleep to a millisecond before, then wait it out - the tokio
/// timer follows the Windows clock tick (~15.6 ms) and would bunch what the drive
/// spread. `admit` lets the trial client in again before a packet when its session
/// went (a time-out between overs); `done` is called once at the end.
pub fn play(
    sends: Vec<Delivery>,
    start: Instant,
    to_server: tokio::sync::mpsc::UnboundedSender<(SocketAddr, Vec<u8>)>,
    mut admit: impl FnMut() + Send + 'static,
    done: impl FnOnce(usize, Duration) + Send + 'static,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let n = sends.len();
        let mut sent = 0usize;
        for s in sends {
            let due = start + s.at;
            loop {
                let now = Instant::now();
                if now >= due {
                    break;
                }
                let left = due - now;
                if left > Duration::from_millis(2) {
                    std::thread::sleep(left - Duration::from_millis(1));
                } else {
                    std::hint::spin_loop();
                }
            }
            admit();
            if to_server.send((TRIAL_CLIENT, s.bytes)).is_err() {
                // Named in advance (working rule 11, build 257).
                log::warn!("Trial: the way in is gone - the replay stops after {} packets", sent);
                return;
            }
            sent += 1;
        }
        done(n, start.elapsed());
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|a| a.to_string()).collect()
    }

    /// The trial's options are taken out; what the server parses itself stays,
    /// in its order. The two that became the server's own say so (build 258).
    #[test]
    fn the_options_come_out_and_the_rest_stays() {
        let (t, rest) = parse(args(&["srv", "--inspelen", "rit.csv", "--tci", "1.2.3.4:5", "--geluid", "stem.wav"])).unwrap();
        assert_eq!(t, Trial { replay: Some("rit.csv".into()), sound: Some("stem.wav".into()) });
        assert_eq!(rest, args(&["srv", "--tci", "1.2.3.4:5"]));
        assert!(parse(args(&["srv", "--geluid"])).is_err());
        assert!(parse(args(&["srv", "--inspelen"])).is_err());
        for gone in ["--zendafstelling", "--inlopen"] {
            assert!(parse(args(&["srv", gone, "B"])).unwrap_err().contains("since build 258"), "{gone}");
            assert!(!USAGE.contains(gone), "{gone}");
        }
        assert!(USAGE.contains("--inspelen") && USAGE.contains("--geluid"));
        let (t, rest) = parse(args(&["srv"])).unwrap();
        assert_eq!((t, rest), (Trial::default(), args(&["srv"])));
    }

    /// The trial client is never a QUIC client: what the server sends it goes
    /// nowhere.
    #[test]
    fn the_trial_client_is_no_quic_client() {
        assert!(!sdr_remote_quic::is_quic_sentinel(TRIAL_CLIENT));
    }

    /// Each arrival becomes the packet the app would have sent: its number, its
    /// timestamp, its PTT bit, at its time after the first; audio when the
    /// measured packet had audio, nothing when it had none. The server reads it
    /// back as the same packet.
    #[test]
    fn an_arrival_becomes_the_packet_that_arrived() {
        let a = |ms, seq, bytes, ptt| Arrival { mono_ms: ms, seq, ts: (ms * 2) as u32, bytes, ptt };
        let sends = audio_sends(&[a(1_000, 7, 60, true), a(1_013, 8, 60, true), a(1_090, 9, 0, false)], None).unwrap();
        assert_eq!(sends.iter().map(|s| s.at.as_millis()).collect::<Vec<_>>(), vec![0, 13, 90]);
        let back: Vec<AudioPacket> = sends
            .iter()
            .map(|s| match sdr_remote_core::protocol::Packet::deserialize(&s.bytes).unwrap() {
                sdr_remote_core::protocol::Packet::Audio(p) => p,
                _ => panic!("not an audio packet"),
            })
            .collect();
        assert_eq!(back.iter().map(|p| (p.sequence, p.timestamp, p.flags.ptt())).collect::<Vec<_>>(), vec![(7, 2_000, true), (8, 2_026, true), (9, 2_180, false)]);
        assert!(!back[0].opus_data.is_empty() && !back[1].opus_data.is_empty());
        assert!(back[2].opus_data.is_empty(), "a release packet carries no audio");
        // And it decodes to sound, not silence.
        let mut dec = sdr_remote_core::codec::OpusDecoderWideband::new().unwrap();
        let pcm = dec.decode(&back[1].opus_data).unwrap();
        assert!(pcm.iter().any(|&s| s.unsigned_abs() > 1_000), "a tone");
    }

    /// A recording instead of the tone: each packet with sound carries the next
    /// 20 ms of it, and at its end it starts again. Here 40 ms of silence and 40 ms
    /// of tone: quiet, quiet, loud, loud, then quiet again. (Opus lets a tone ring
    /// a few ms into the next packet, so the second quiet packet is the one to look at.)
    #[test]
    fn the_packets_carry_the_recording_in_turn() {
        let n = sdr_remote_core::FRAME_SAMPLES_WIDEBAND;
        let mut sound = vec![0i16; 2 * n];
        sound.extend((0..2 * n).map(|i| ((i as f32 * 0.3).sin() * 12_000.0) as i16));
        let a = |ms, seq| Arrival { mono_ms: ms, seq, ts: ms as u32, bytes: 60, ptt: true };
        let sends = audio_sends(&(0..6u64).map(|i| a(i * 20, i as u32)).collect::<Vec<_>>(), Some(&sound)).unwrap();
        let mut dec = sdr_remote_core::codec::OpusDecoderWideband::new().unwrap();
        let levels: Vec<f32> = sends
            .iter()
            .map(|s| match sdr_remote_core::protocol::Packet::deserialize(&s.bytes).unwrap() {
                sdr_remote_core::protocol::Packet::Audio(p) => crate::pause_trim::level_db(&dec.decode(&p.opus_data).unwrap()),
                _ => panic!("not an audio packet"),
            })
            .collect();
        assert!(levels[3] > levels[1] + 20.0, "loud after quiet: {levels:?}");
        assert!(levels[3] > levels[5] + 20.0, "and quiet again from the start of the recording: {levels:?}");
    }

    /// A WAV at 8 kHz in stereo reads as mono at 16 kHz: twice the samples, the
    /// channels averaged. Not PCM, not 16 bit or not a WAV is refused.
    #[test]
    fn a_wav_file_reads_as_mono_at_16_khz() {
        let wav = |channels: u16, rate: u32, bits: u16, samples: &[i16]| {
            let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
            let mut b = Vec::new();
            b.extend_from_slice(b"RIFF");
            b.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
            b.extend_from_slice(b"WAVEfmt ");
            b.extend_from_slice(&16u32.to_le_bytes());
            b.extend_from_slice(&1u16.to_le_bytes());
            b.extend_from_slice(&channels.to_le_bytes());
            b.extend_from_slice(&rate.to_le_bytes());
            b.extend_from_slice(&(rate * channels as u32 * 2).to_le_bytes());
            b.extend_from_slice(&(channels * 2).to_le_bytes());
            b.extend_from_slice(&bits.to_le_bytes());
            b.extend_from_slice(b"data");
            b.extend_from_slice(&(data.len() as u32).to_le_bytes());
            b.extend_from_slice(&data);
            b
        };
        // 8 kHz stereo: left 1000, right 3000 -> mono 2000, 800 frames -> 1600 at 16 kHz.
        let st: Vec<i16> = (0..800).flat_map(|_| [1_000i16, 3_000]).collect();
        let m = read_wav_16k(&wav(2, 8_000, 16, &st)).unwrap();
        assert_eq!(m.len(), 1_600);
        assert!(m.iter().all(|&s| s == 2_000));
        // 48 kHz mono: a third of the samples.
        assert_eq!(read_wav_16k(&wav(1, 48_000, 16, &vec![5i16; 4_800])).unwrap().len(), 1_600);
        assert!(read_wav_16k(&wav(1, 16_000, 8, &[0i16; 10])).is_err(), "8 bit");
        assert!(read_wav_16k(b"not a wav file at all").is_err());
    }

    /// Heartbeats every 500 ms from the start, between the audio in time order,
    /// each with the PTT bit of the audio before it.
    #[test]
    fn heartbeats_go_between_the_audio_every_half_second() {
        let audio: Vec<Delivery> = [0u64, 400, 1_200].iter().map(|&ms| Delivery { at: Duration::from_millis(ms), bytes: vec![ms as u8] }).collect();
        let all = schedule(audio, &[true, true, false]);
        let times: Vec<u128> = all.iter().map(|s| s.at.as_millis()).collect();
        assert_eq!(times, vec![0, 0, 400, 500, 1_000, 1_200]);
        let hb_ptt: Vec<bool> = all
            .iter()
            .filter_map(|s| match sdr_remote_core::protocol::Packet::deserialize(&s.bytes) {
                Ok(sdr_remote_core::protocol::Packet::Heartbeat(h)) => Some(h.flags.ptt()),
                _ => None,
            })
            .collect();
        assert_eq!(hb_ptt, vec![false, true, true], "before any audio no PTT; then that of the packet before");
    }

    /// The thread hands everything over in order and on time: none early, and on
    /// this machine none more than a few ms late.
    #[test]
    fn the_thread_hands_over_on_time_and_in_order() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let sends: Vec<Delivery> = (0..20u64).map(|i| Delivery { at: Duration::from_millis(i * 7), bytes: vec![i as u8] }).collect();
        let start = Instant::now() + Duration::from_millis(20);
        let admitted = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let a2 = admitted.clone();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let h = play(sends, start, tx, move || { a2.fetch_add(1, std::sync::atomic::Ordering::SeqCst); }, move |n, _| done_tx.send(n).unwrap());
        let mut late = Vec::new();
        for i in 0..20u64 {
            let (addr, bytes) = rx.blocking_recv().unwrap();
            let at = start.elapsed();
            assert_eq!((addr, bytes), (TRIAL_CLIENT, vec![i as u8]));
            assert!(at >= Duration::from_millis(i * 7), "packet {i} early: {at:?}");
            late.push(at.saturating_sub(Duration::from_millis(i * 7)));
        }
        h.join().unwrap();
        assert_eq!(done_rx.recv().unwrap(), 20);
        assert_eq!(admitted.load(std::sync::atomic::Ordering::SeqCst), 20, "asked before every packet");
        late.sort();
        assert!(late[10] < Duration::from_millis(5), "the median handover: {late:?}");
    }
}
