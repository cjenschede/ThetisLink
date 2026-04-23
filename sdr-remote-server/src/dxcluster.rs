// SPDX-License-Identifier: GPL-2.0-or-later

#![allow(dead_code)]
use std::sync::{Arc, Mutex};
use std::time::Instant;

use log::{info, warn};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

/// A single DX cluster spot
#[derive(Clone, Debug)]
pub struct DxSpot {
    pub callsign: String,
    pub frequency_hz: u64,
    pub mode: String,
    pub spotter: String,
    pub comment: String,
    pub time: Instant,
}

/// DX Cluster client — connects to a DX Spider telnet cluster,
/// parses spots, and stores them with automatic expiry.
pub struct DxCluster {
    spots: Arc<Mutex<Vec<DxSpot>>>,
    _cmd_tx: mpsc::Sender<ClusterCmd>,
    expiry_secs: u64,
}

enum ClusterCmd {
    _Shutdown,
}

impl DxCluster {
    /// Start a new DX Cluster connection in a background task.
    pub fn new(server: &str, callsign: &str, expiry_min: u16) -> Self {
        let expiry_secs = expiry_min as u64 * 60;
        let spots: Arc<Mutex<Vec<DxSpot>>> = Arc::new(Mutex::new(Vec::new()));
        let (cmd_tx, cmd_rx) = mpsc::channel::<ClusterCmd>(8);

        let server = server.to_string();
        let callsign = callsign.to_string();
        let spots_clone = spots.clone();

        tokio::spawn(async move {
            cluster_task(&server, &callsign, spots_clone, cmd_rx, expiry_secs).await;
        });

        Self {
            spots,
            _cmd_tx: cmd_tx,
            expiry_secs,
        }
    }

    /// Get the configured expiry time in seconds.
    pub fn expiry_secs(&self) -> u64 {
        self.expiry_secs
    }

    /// Get all non-expired spots, optionally filtered to a specific band.
    pub fn spots_for_bands(&self, vfo_a_hz: u64, vfo_b_hz: u64) -> Vec<DxSpot> {
        let now = Instant::now();
        let guard = self.spots.lock().unwrap();
        guard
            .iter()
            .filter(|s| now.duration_since(s.time).as_secs() < self.expiry_secs)
            .filter(|s| {
                let band_a = freq_to_band(vfo_a_hz);
                let band_b = freq_to_band(vfo_b_hz);
                let spot_band = freq_to_band(s.frequency_hz);
                spot_band != 0 && (spot_band == band_a || spot_band == band_b)
            })
            .cloned()
            .collect()
    }

    /// Get all non-expired spots (unfiltered).
    pub fn all_spots(&self) -> Vec<DxSpot> {
        let now = Instant::now();
        let guard = self.spots.lock().unwrap();
        guard
            .iter()
            .filter(|s| now.duration_since(s.time).as_secs() < self.expiry_secs)
            .cloned()
            .collect()
    }
}

/// Background task: connect, login, parse spots, reconnect on failure.
async fn cluster_task(
    server: &str,
    callsign: &str,
    spots: Arc<Mutex<Vec<DxSpot>>>,
    mut _cmd_rx: mpsc::Receiver<ClusterCmd>,
    expiry_secs: u64,
) {
    let mut backoff_secs = 1u64;

    loop {
        info!("DX Cluster: connecting to {}...", server);
        match tokio::time::timeout(
            tokio::time::Duration::from_secs(10),
            TcpStream::connect(server),
        )
        .await
        {
            Ok(Ok(stream)) => {
                info!("DX Cluster: connected to {}", server);
                backoff_secs = 1;
                if let Err(e) = handle_connection(stream, callsign, &spots, expiry_secs).await {
                    warn!("DX Cluster: connection error: {}", e);
                }
            }
            Ok(Err(e)) => {
                warn!("DX Cluster: connect failed: {}", e);
            }
            Err(_) => {
                warn!("DX Cluster: connect timeout");
            }
        }

        // Reconnect with backoff
        info!(
            "DX Cluster: reconnecting in {} seconds...",
            backoff_secs
        );
        tokio::time::sleep(tokio::time::Duration::from_secs(backoff_secs)).await;
        backoff_secs = (backoff_secs * 2).min(60);
    }
}

/// Handle a single cluster connection: login, then read and parse lines.
async fn handle_connection(
    stream: TcpStream,
    callsign: &str,
    spots: &Arc<Mutex<Vec<DxSpot>>>,
    expiry_secs: u64,
) -> anyhow::Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    // Send callsign as login
    writer
        .write_all(format!("{}\r\n", callsign).as_bytes())
        .await?;
    info!("DX Cluster: logged in as {}", callsign);

    while let Some(line) = lines.next_line().await? {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }

        if let Some(spot) = parse_dx_spot(&line) {
            log::debug!(
                "DX SPOT: {} {:.1} kHz {} by {}",
                spot.callsign,
                spot.frequency_hz as f64 / 1000.0,
                spot.mode,
                spot.spotter
            );

            let mut guard = spots.lock().unwrap();
            // Expire old spots
            let now = Instant::now();
            guard.retain(|s| now.duration_since(s.time).as_secs() < expiry_secs);
            guard.push(spot);
        }
    }

    Ok(())
}

/// Parse a DX Spider spot line.
/// Format: `DX de SPOTTER:  FREQ  CALLSIGN  comment  TIME`
/// Example: `DX de W3LPL:     14025.0  JA1ABC        CW up 1              1234Z`
fn parse_dx_spot(line: &str) -> Option<DxSpot> {
    if !line.starts_with("DX de ") {
        return None;
    }

    let rest = &line[6..]; // after "DX de "

    // Find spotter (ends with ':')
    let colon_pos = rest.find(':')?;
    let spotter = rest[..colon_pos].trim().to_string();
    let after_colon = &rest[colon_pos + 1..];

    // Split remaining by whitespace
    let parts: Vec<&str> = after_colon.split_whitespace().collect();
    if parts.len() < 2 {
        return None;
    }

    // First field: frequency in kHz (e.g. "14025.0")
    let freq_khz: f64 = parts[0].parse().ok()?;
    let frequency_hz = (freq_khz * 1000.0) as u64;

    // Second field: callsign
    let callsign = parts[1].to_string();

    // Remaining: comment (everything between callsign and optional time at end)
    let comment = if parts.len() > 2 {
        // Check if last part is a time like "1234Z"
        let last = *parts.last().unwrap();
        let has_time = last.len() == 5
            && last.ends_with('Z')
            && last[..4].chars().all(|c| c.is_ascii_digit());
        if has_time && parts.len() > 3 {
            parts[2..parts.len() - 1].join(" ")
        } else if has_time {
            String::new()
        } else {
            parts[2..].join(" ")
        }
    } else {
        String::new()
    };

    // Guess mode from frequency and comment
    let mode = guess_mode(frequency_hz, &comment);

    Some(DxSpot {
        callsign,
        frequency_hz,
        mode,
        spotter,
        comment,
        time: Instant::now(),
    })
}

/// Guess the mode from frequency position within the band and comment text.
fn guess_mode(freq_hz: u64, comment: &str) -> String {
    let comment_lower = comment.to_lowercase();

    // Check comment for explicit mode mentions
    if comment_lower.contains("ft8") {
        return "FT8".to_string();
    }
    if comment_lower.contains("ft4") {
        return "FT4".to_string();
    }
    if comment_lower.contains("rtty") || comment_lower.contains("psk") {
        return "DIGI".to_string();
    }
    if comment_lower.contains("cw") {
        return "CW".to_string();
    }
    if comment_lower.contains("ssb") || comment_lower.contains("phone") {
        return "SSB".to_string();
    }

    // Guess by frequency within band
    let freq_khz = freq_hz / 1000;
    match freq_khz {
        // 160m
        1800..=1840 => "CW".to_string(),
        1841..=2000 => "SSB".to_string(),
        // 80m
        3500..=3570 => "CW".to_string(),
        3571..=3600 => "DIGI".to_string(),
        3601..=3800 => "SSB".to_string(),
        // 40m
        7000..=7040 => "CW".to_string(),
        7041..=7080 => "DIGI".to_string(),
        7081..=7300 => "SSB".to_string(),
        // 30m
        10100..=10130 => "CW".to_string(),
        10131..=10150 => "DIGI".to_string(),
        // 20m
        14000..=14070 => "CW".to_string(),
        14071..=14099 => "DIGI".to_string(),
        14100..=14350 => "SSB".to_string(),
        // 17m
        18068..=18095 => "CW".to_string(),
        18096..=18110 => "DIGI".to_string(),
        18111..=18168 => "SSB".to_string(),
        // 15m
        21000..=21070 => "CW".to_string(),
        21071..=21150 => "DIGI".to_string(),
        21151..=21450 => "SSB".to_string(),
        // 12m
        24890..=24915 => "CW".to_string(),
        24916..=24930 => "DIGI".to_string(),
        24931..=24990 => "SSB".to_string(),
        // 10m
        28000..=28070 => "CW".to_string(),
        28071..=28190 => "DIGI".to_string(),
        28191..=29700 => "SSB".to_string(),
        // 6m
        50000..=50100 => "CW".to_string(),
        50101..=50500 => "SSB".to_string(),
        50501..=54000 => "DIGI".to_string(),
        _ => "SSB".to_string(),
    }
}

/// Map frequency to a band number (meters). Returns 0 for unknown.
pub fn freq_to_band(freq_hz: u64) -> u16 {
    let khz = freq_hz / 1000;
    match khz {
        1800..=2000 => 160,
        3500..=3800 => 80,
        5351..=5367 => 60,
        7000..=7300 => 40,
        10100..=10150 => 30,
        14000..=14350 => 20,
        18068..=18168 => 17,
        21000..=21450 => 15,
        24890..=24990 => 12,
        28000..=29700 => 10,
        50000..=54000 => 6,
        _ => 0,
    }
}

/// ARGB color for a mode string (for TCI SPOT command).
pub fn mode_color_argb(mode: &str) -> u32 {
    match mode {
        "CW" => 0xFFFFFF00,       // yellow
        "SSB" => 0xFF00FF00,      // green
        "FT8" | "FT4" | "DIGI" => 0xFF00FFFF, // cyan
        _ => 0xFFFFFFFF,          // white
    }
}

/// egui Color32 for a mode string (for client spectrum overlay).
pub fn mode_color_rgba(mode: &str, alpha: u8) -> [u8; 4] {
    match mode {
        "CW" => [255, 255, 0, alpha],       // yellow
        "SSB" => [0, 255, 0, alpha],        // green
        "FT8" | "FT4" | "DIGI" => [0, 255, 255, alpha], // cyan
        _ => [255, 255, 255, alpha],        // white
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_spot() {
        let line = "DX de W3LPL:     14025.0  JA1ABC        CW up 1              1234Z";
        let spot = parse_dx_spot(line).unwrap();
        assert_eq!(spot.spotter, "W3LPL");
        assert_eq!(spot.frequency_hz, 14025000);
        assert_eq!(spot.callsign, "JA1ABC");
        assert_eq!(spot.mode, "CW");
    }

    #[test]
    fn test_parse_spot_ft8() {
        let line = "DX de PA3GHM:    14074.0  VK2ABC        FT8 -12dB             0830Z";
        let spot = parse_dx_spot(line).unwrap();
        assert_eq!(spot.frequency_hz, 14074000);
        assert_eq!(spot.mode, "FT8");
    }

    #[test]
    fn test_parse_non_spot() {
        assert!(parse_dx_spot("Hello from the cluster").is_none());
        assert!(parse_dx_spot("").is_none());
    }

    #[test]
    fn test_freq_to_band() {
        assert_eq!(freq_to_band(14_200_000), 20);
        assert_eq!(freq_to_band(7_074_000), 40);
        assert_eq!(freq_to_band(3_500_000), 80);
        assert_eq!(freq_to_band(100_000), 0);
    }
}
