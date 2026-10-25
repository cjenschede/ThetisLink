// SPDX-License-Identifier: GPL-2.0-or-later
//! The server's measurement of transmit packets (build 230): one line per
//! TX audio packet that goes into a jitter buffer - when it arrived, its
//! sequence number, the phone's timestamp and, since build 263, which
//! transmitter it is for (`stream`: Thetis or one of the Yaesu radios). Step 2 of
//! `PLAN-zendketen-samen-met-thetis.md`: the replay of step 3 needs the
//! arrival of every packet, and the 10 s summaries in the log are too coarse.
//!
//! Developer only (owner, 2026-10-12): on by a checkbox in the server's
//! settings screen (`tx_measurement`), read when the server starts.
//!
//! The receive loop must not wait on a disk: a row is handed to a writer
//! thread with `try_send` into a bounded channel, and a full channel counts
//! the row as dropped instead of blocking. The channel's buffer is allocated
//! once, so a row costs no allocation either.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Rows the writer may fall behind by: ~80 s of one transmitter.
const CHANNEL_ROWS: usize = 4096;

/// How often the log says how far the file got.
const SAY_EVERY: Duration = Duration::from_secs(600);

/// One TX audio packet as it arrived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxArrival {
    /// Wall clock, ms since the epoch: what the phone's file is joined on.
    pub wall_ms: u64,
    /// Server clock, ms since the server started (the jitter buffer's clock).
    pub mono_ms: u64,
    pub seq: u32,
    /// The phone's timestamp in the packet (its send time).
    pub ts: u32,
    pub bytes: usize,
    /// The packet asked for PTT (false: the tail after the release).
    pub ptt: bool,
    pub stream: TxStream,
}

/// Which transmitter a packet is for (build 263), in the words of the phone's
/// own file (`stream`: thetis, radio1, radio2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxStream {
    Thetis,
    Radio1,
    Radio2,
}

impl TxStream {
    pub fn file_word(self) -> &'static str {
        match self {
            TxStream::Thetis => "thetis",
            TxStream::Radio1 => "radio1",
            TxStream::Radio2 => "radio2",
        }
    }
}

/// Wall clock in ms, for `TxArrival::wall_ms`.
pub fn wall_ms_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

pub struct TxArrivals {
    tx: Option<SyncSender<TxArrival>>,
    writer: Option<JoinHandle<u64>>,
    path: PathBuf,
    sent: u64,
    /// The channel was full: the writer fell behind.
    dropped_full: u64,
    /// The writer had stopped (a write failed); counted apart since build 231.
    dropped_stopped: u64,
    said_at: Instant,
}

impl TxArrivals {
    /// Opens `tl-servermeting-<date>-<time>.csv` in `dir`, writes the header and
    /// starts the writer.
    pub fn start(dir: &Path, label: &str) -> std::io::Result<TxArrivals> {
        let now = chrono::Local::now();
        let path = dir.join(format!("tl-servermeting-{}.csv", now.format("%Y%m%d-%H%M%S")));
        let mut file = std::io::BufWriter::new(std::fs::File::create(&path)?);
        writeln!(file, "# ThetisLink server measurement, {}, started {}", label, now.to_rfc3339())?;
        writeln!(file, "wall_ms,mono_ms,kind,seq,ts,bytes,ptt,stream")?;
        file.flush()?;
        let (tx, rx) = sync_channel(CHANNEL_ROWS);
        let writer = std::thread::Builder::new()
            .name("tx-arrivals".into())
            .spawn(move || write_rows(rx, file))?;
        Ok(TxArrivals::with_channel(tx, Some(writer), path))
    }

    fn with_channel(tx: SyncSender<TxArrival>, writer: Option<JoinHandle<u64>>, path: PathBuf) -> TxArrivals {
        TxArrivals { tx: Some(tx), writer, path, sent: 0, dropped_full: 0, dropped_stopped: 0, said_at: Instant::now() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Hand one row to the writer, never waiting. Every ten minutes with rows
    /// the log says how far it got.
    pub fn record(&mut self, row: TxArrival) {
        let Some(tx) = self.tx.as_ref() else { return };
        match tx.try_send(row) {
            Ok(()) => self.sent += 1,
            Err(TrySendError::Full(_)) => self.dropped_full += 1,
            Err(TrySendError::Disconnected(_)) => self.dropped_stopped += 1,
        }
        if self.said_at.elapsed() >= SAY_EVERY {
            self.said_at = Instant::now();
            // Named in advance (`logregels-build230.md`, `logregels-build231.md`).
            log::info!(
                "Measurement (server): {} packet(s) written, {} dropped (channel full), {} dropped (writer stopped)",
                self.sent, self.dropped_full, self.dropped_stopped
            );
        }
    }

    /// Close the channel and wait for the writer: the rows it wrote, and the
    /// rows that were dropped here.
    fn close(&mut self) -> (u64, u64) {
        self.tx = None;
        let written = self.writer.take().and_then(|h| h.join().ok()).unwrap_or(0);
        (written, self.dropped_full + self.dropped_stopped)
    }

    #[cfg(test)]
    fn stop(mut self) -> (u64, u64) {
        self.close()
    }
}

/// The server stopping ends the measurement: the writer gets the last rows
/// out, and the log says where the file is and how much is in it.
impl Drop for TxArrivals {
    fn drop(&mut self) {
        if self.writer.is_some() {
            let (written, dropped) = self.close();
            // Named in advance (`logregels-build230.md`).
            log::info!(
                "Measurement (server) off: {}, {} packet(s) written, {} dropped",
                self.path.display(), written, dropped
            );
        }
    }
}

/// The writer thread: one line per row, flushed at least once a second so a
/// server that is killed loses at most that.
fn write_rows(rx: Receiver<TxArrival>, mut file: impl Write) -> u64 {
    let mut written = 0u64;
    let mut flushed = Instant::now();
    loop {
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(r) => {
                if writeln!(file, "{},{},tx_arrival,{},{},{},{},{}", r.wall_ms, r.mono_ms, r.seq, r.ts, r.bytes, r.ptt as u8, r.stream.file_word())
                    .is_err()
                {
                    log::warn!("Measurement (server): a write failed - the file ends here");
                    return written;
                }
                written += 1;
                if flushed.elapsed() >= Duration::from_secs(1) {
                    let _ = file.flush();
                    flushed = Instant::now();
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                let _ = file.flush();
                flushed = Instant::now();
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                let _ = file.flush();
                return written;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(seq: u32, ptt: bool) -> TxArrival {
        TxArrival { wall_ms: 1_000 + seq as u64, mono_ms: 20 * seq as u64, seq, ts: 960 * seq, bytes: 31, ptt, stream: TxStream::Thetis }
    }

    /// Every row in its own line, by name, under the header the phone's file
    /// has too; and the file is complete once the measurement stops.
    #[test]
    fn every_packet_is_one_line_in_the_file() {
        let dir = std::env::temp_dir().join(format!("tl-txarr-{}-{}", std::process::id(), wall_ms_now()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut m = TxArrivals::start(&dir, "2.13.0-dev (build 230)").unwrap();
        let path = m.path().to_path_buf();
        assert!(path.file_name().unwrap().to_string_lossy().starts_with("tl-servermeting-"));
        m.record(row(1, true));
        m.record(row(2, true));
        m.record(row(3, false));
        m.record(TxArrival { stream: TxStream::Radio1, ..row(4, true) });
        m.record(TxArrival { stream: TxStream::Radio2, ..row(5, true) });
        assert_eq!(m.stop(), (5, 0));
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].starts_with("# ThetisLink server measurement, 2.13.0-dev (build 230), started "));
        assert_eq!(lines[1], "wall_ms,mono_ms,kind,seq,ts,bytes,ptt,stream");
        assert_eq!(
            &lines[2..],
            [
                "1001,20,tx_arrival,1,960,31,1,thetis",
                "1002,40,tx_arrival,2,1920,31,1,thetis",
                "1003,60,tx_arrival,3,2880,31,0,thetis",
                "1004,80,tx_arrival,4,3840,31,1,radio1",
                "1005,100,tx_arrival,5,4800,31,1,radio2",
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A writer that falls behind costs rows, not the receive loop: a full
    /// channel counts the row as dropped and returns at once.
    #[test]
    fn a_full_channel_drops_the_row_and_does_not_wait() {
        let (tx, _rx) = sync_channel(2);
        let mut m = TxArrivals::with_channel(tx, None, PathBuf::from("unused"));
        for seq in 0..5 {
            m.record(row(seq, true));
        }
        assert_eq!((m.sent, m.dropped_full, m.dropped_stopped), (2, 3, 0));
        assert_eq!(m.stop(), (0, 3));
    }

    /// A writer that stopped (a write failed) is not a full channel: the rows
    /// are counted apart, so the log says which (review of 230).
    #[test]
    fn a_stopped_writer_is_not_a_full_channel() {
        let (tx, rx) = sync_channel(8);
        drop(rx);
        let mut m = TxArrivals::with_channel(tx, None, PathBuf::from("unused"));
        m.record(row(1, true));
        m.record(row(2, true));
        assert_eq!((m.sent, m.dropped_full, m.dropped_stopped), (0, 0, 2));
        assert_eq!(m.stop(), (0, 2));
    }

    /// The receive loop records both kinds of packet that go into the jitter
    /// buffer - the transmission and its tail - by name, before the push.
    #[test]
    fn the_receive_loop_records_both_pushes_into_the_jitter_buffer() {
        let net = include_str!("network.rs");
        let net = &sdr_remote_core::source_text::production_code(net);
        // Thetis's one, with the packet's flag (build 285: one push for the
        // press and the release; two until then), and the radios' one block
        // with its flag (build 282).
        assert_eq!(net.matches("crate::tx_arrivals::TxArrival {").count(), 2);
        let rec = net
            .find("ts: audio_pkt.timestamp,\n                                    bytes: audio_pkt.opus_data.len(),\n                                    ptt: ptt_requested,")
            .expect("Thetis's record, with the packet's flag");
        let push = net[rec..].find("jitter_buf.push(").expect("its push");
        assert!(net[rec..rec + push].matches("ptt: ").count() == 1, "the record is the one right before its push");
        assert_eq!(net.matches("seq: audio_pkt.sequence,").count(), 1);
        assert_eq!(net.matches("stream: crate::tx_arrivals::TxStream::Thetis,").count(), 1);
        // Each radio's packets, recorded where they go to its task (build 263),
        // in the one block for both since build 282, with their own flag and
        // stream.
        let site = net.find("if let Some(ref tx) = $packet_tx {").expect("the radios' forwarding");
        let rec = net[..site].rfind("crate::tx_arrivals::TxArrival {").expect("a record before it");
        assert!(net[rec..site].contains("ptt,
                        stream: $stream,"), "its flag and its stream");
        assert!(site - rec < 1200, "right before the forwarding");
        for (radio, stream) in [("yaesu_tx_packet_tx", "Radio1"), ("yaesu2_tx_packet_tx", "Radio2")] {
            assert_eq!(net.matches(&format!("{radio}, crate::tx_arrivals::TxStream::{stream});")).count(), 1, "{radio}");
        }
    }
}
