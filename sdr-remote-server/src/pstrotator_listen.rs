// SPDX-License-Identifier: GPL-2.0-or-later

//! PstRotator UDP listener - parallel input source naast de actieve
//! `rotor_backend`. Vertaalt inkomende PstRotator azimuth-broadcasts
//! naar `RotorCmd::GoTo` op de gedeelde `Rotor`-facade zodat een
//! logger zoals Log4OM via PstRotator de echte rotor-hardware kan
//! aansturen - ongeacht welke backend (EA7HG / PstRotator-outgoing /
//! Adafruit MCP2221A) die hardware bedient.
//!
//! **Topologie:**
//!
//! ```text
//! Win4OM / Log4OM
//!   ↓ (XML over Log4OM-rotorlogica)
//! PstRotator (Win4OM-PC)
//!   ↓ UDP broadcast naar geconfigureerde endpoints
//! ThetisLink-server (deze module)
//!   ↓ RotorCmd::GoTo(angle_x10)
//! Rotor-facade
//!   ↓
//! actieve backend (EA7HG / PstRotator / MCP2221A)
//! ```
//!
//! **Geaccepteerde packet-formaten** - kies in PstRotator één van:
//!
//! - **Yaesu GS-232A / GS-232B** (PstRotator -> "Controller: Yaesu
//!   GS-232A/B"; **aanbevolen**): tekstuele ASCII-commando's, simpel
//!   en goed gedocumenteerd.
//!   - `M<nnn>\r` - move to azimuth (3-cijferig 000-450). Voorbeeld:
//!     `M090\r`.
//!   - `S\r` - stop
//!   - `C\r` - current position query. Reply: `+<nnn>\r` (3-cijferig).
//!   - `C2\r` - azimuth + elevation query. Reply: `+0aaa+0eee\r`
//!     (we sturen elevation altijd 000 - geen el-as).
//!   - `R\r` / `L\r` - manual rotate (worden genegeerd).
//!   Bidirectionele protocol: listener antwoordt op `C` / `C2` met de
//!   actuele rotor-positie zodat PstRotator zijn display kan
//!   synchroniseren.
//!
//! - **Prosistel binair (EA7HG-variant)** (PstRotator -> "Controller:
//!   EA7HG Visual Rotor"): `\x02AG<nnn>\r` of `AAG<nnn>\r`. Stop is
//!   `\x02AG999\r` of `AAR\r`. Status-query `\x02A?\r` of `AA?\r`,
//!   reply `\x02A,?,<nnn>,<R|B>\r`. Werkt maar minder gestandaardiseerd
//!   dan GS-232A.
//!
//! - **Tekstmode broadcast** (PstRotator's reply-format als output):
//!   `AZ:nnn.n\r`. Eenrichtingsverkeer; geen status-replies. Voorbeeld:
//!   `AZ:271.5\r`.
//!
//! - **XML-mode** (PstRotator "Output" forwarding):
//!   `<PST><AZIMUTH>nnn.n</AZIMUTH></PST>`.
//!
//! `EL:...` / `<ELEVATION>` is in fase 1 niet geïmplementeerd -
//! ThetisLink's Rotor-facade kent geen elevation-axis. Worden stil
//! genegeerd via `debug!`.
//!
//! **Loop-bescherming:** wanneer `rotor_backend = pstrotator` (de
//! outgoing backend) ook draait en zijn AZ?-replies pikken we niet
//! per ongeluk op. PstRotator beantwoordt replies aan `port + 1`
//! (default 12001) - als de gebruiker daar ook luistert ontstaat
//! een feedback-loop. UI noteert dit; runtime negeren we packets
//! waarvan de azimuth gelijk is aan de laatst-uitgaande GoTo
//! binnen `LOOPBACK_DEDUP_WINDOW`.
//!
//! **Rate-limit:** PstRotator broadcast typisch elke 0,5-1 s zelfs
//! als de azimuth onveranderd is. Identieke azimuth binnen
//! `DEDUPE_INTERVAL` wordt gefilterd zodat de poll-thread van de
//! Adafruit niet onnodig per-tick een GoTo opnieuw beoordeelt.

use std::io::{BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use log::{debug, info, warn};

use crate::rotor::{Rotor, RotorCmd};

/// Hoe lang we dezelfde azimuth slikken voordat we weer een GoTo
/// publiceren naar de Rotor-facade. 3 s is ruim onder een typische
/// antenne-beweging (~2°/sec -> 6° gemist worst-case) maar absorbeert
/// PstRotator's heartbeat-broadcasts (~1 Hz bij stilstand) én een
/// eventuele bidirectionele "AAG-herhaling" die PstRotator stuurt
/// als hij geen status-reply terug krijgt.
const DEDUPE_INTERVAL: Duration = Duration::from_secs(3);

/// Read-timeout op de UDP-socket - bepaalt hoe snel we de shutdown-flag
/// zien als er geen verkeer is.
const READ_TIMEOUT: Duration = Duration::from_millis(500);

/// Verlies-tolerantie voor identieke-azimuth dedupe (in tienden van
/// graden). 0,1° = onder rotor-mechanische resolutie.
const AZIMUTH_DEDUPE_EPSILON_X10: u16 = 1;

/// Tolerantie waarbinnen een incoming GoTo als feedback-echo van de
/// huidige rotor-positie wordt beschouwd. PstRotator's "follow rotor"
/// / auto-track modus broadcast de gemeten positie als een nieuwe
/// goto-stream (~3 Hz). Zonder filter overschrijft die elke TL2-
/// client-target binnen ~300 ms (de oranje doelhoek verdwijnt uit de
/// rotor-window). Een GoTo naar de huidige positie is sowieso een
/// no-op voor de hardware, dus droppen kost geen functionaliteit.
/// 1.5° = ruim boven mechanische dead-band, ruim onder de kleinste
/// echte user-input (5°-stappen of meer in de UI).
const FEEDBACK_TO_CURRENT_EPSILON_X10: u16 = 15;

/// Tijdvenster waarbinnen `AZ:nnn` text broadcasts na een echte
/// `AG<nnn>` goto worden behandeld als PstRotator-simulator-feedback
/// en stil gedropt. Buiten dit venster wordt `AZ:nnn` weer als goto
/// geaccepteerd (backwards-compat voor setups die alleen AZ
/// broadcasten zonder Prosistel goto-stream).
const AZ_FEEDBACK_AFTER_AG: Duration = Duration::from_secs(30);

/// Configuratie voor `spawn`. Bevat alleen wat de listener-thread
/// hoeft te weten; de Rotor-facade komt apart binnen via `rotor`.
pub struct ListenConfig {
    /// UDP-poort waar we op luisteren. Operator-keuze; default 12001 is
    /// PstRotator's standaard feedback-poort.
    pub port: u16,
}

/// Spawn de listener-thread. Retourneert een shutdown-handle die op
/// `true` gezet kan worden om de thread netjes te laten exit'en bij
/// server-stop. De thread bindt zelf de UDP-poort; bind-fouten worden
/// gelogd en geven `Err` terug zodat de caller kan beslissen om wel
/// of niet te continueren zonder listener.
pub fn spawn(config: ListenConfig, rotor: Rotor) -> Result<Arc<AtomicBool>, std::io::Error> {
    let bind_addr: SocketAddr = format!("0.0.0.0:{}", config.port)
        .parse()
        .map_err(|e: std::net::AddrParseError| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string())
        })?;
    // UDP en TCP delen dezelfde poort - OS ziet ze als verschillende
    // protocols. PstRotator-clients kunnen kiezen welk transport ze
    // gebruiken; beide threads draaien parallel.
    let udp_sock = UdpSocket::bind(bind_addr)?;
    udp_sock.set_read_timeout(Some(READ_TIMEOUT))?;
    let tcp_listener = TcpListener::bind(bind_addr)?;
    tcp_listener.set_nonblocking(true)?;
    let shutdown = Arc::new(AtomicBool::new(false));

    let udp_shutdown = shutdown.clone();
    let udp_rotor = rotor.clone();
    let udp_port = config.port;
    std::thread::Builder::new()
        .name("pstrotator-listen-udp".to_string())
        .spawn(move || {
            run_udp(udp_sock, udp_rotor, udp_shutdown, udp_port);
        })?;

    let tcp_shutdown = shutdown.clone();
    let tcp_rotor = rotor.clone();
    let tcp_port = config.port;
    std::thread::Builder::new()
        .name("pstrotator-listen-tcp".to_string())
        .spawn(move || {
            run_tcp(tcp_listener, tcp_rotor, tcp_shutdown, tcp_port);
        })?;

    info!(
        "PstRotator listener listening on UDP+TCP {} (accepts Yaesu GS-232A `M<nnn>`/`C`, \
         Prosistel `AAG<nnn>`/`A?`, XML `<AZIMUTH>`; `AZ:nn` over UDP wordt binnen 30s na \
         een AG-goto als simulator-broadcast genegeerd)",
        config.port
    );
    Ok(shutdown)
}

fn run_udp(sock: UdpSocket, rotor: Rotor, shutdown: Arc<AtomicBool>, port: u16) {
    let mut buf = [0u8; 256];
    let mut last_az_x10: Option<u16> = None;
    let mut last_dispatch_at: Option<Instant> = None;
    let mut packet_count: u64 = 0;
    let mut parse_fail_count: u64 = 0;
    // Operator-bevinding 2026-06-05: PstRotator broadcastet zijn eigen
    // simulator-positie als `AZ:nnn` parallel aan de echte `AG<nnn>`
    // goto-stream. Zonder onderscheid overschreef elke AZ de goto en
    // de naald schommelde stapsgewijs. Track wanneer de laatste echte
    // AG-goto binnenkwam zodat we AZ binnen het venster kunnen droppen
    // (simulator-noise), maar buiten het venster nog steeds accepteren
    // (backwards-compat voor AZ-only setups).
    let mut last_ag_at: Option<Instant> = None;
    while !shutdown.load(Ordering::Relaxed) {
        let (n, peer) = match sock.recv_from(&mut buf) {
            Ok(v) => v,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock
                || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(e) => {
                warn!("PstRotator listener recv error on port {}: {}", port, e);
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
        };
        packet_count += 1;
        let payload = &buf[..n];
        // Diagnostiek: log elk inkomend packet zodat we zien wat
        // PstRotator écht broadcast (commando vs status-reply vs
        // simulator-positie). Dit blijft tot we de bron van de
        // "target schiet weg"-issue hebben gevonden; daarna kan dit
        // weer naar debug-niveau.
        let raw_preview: String = payload
            .iter()
            .map(|b| {
                if (0x20..=0x7e).contains(b) {
                    (*b as char).to_string()
                } else {
                    format!("\\x{:02x}", b)
                }
            })
            .collect();
        debug!(
            "PstRotator listen RX from {} ({} bytes): {:?}",
            peer, n, raw_preview
        );
        let text = match std::str::from_utf8(payload) {
            Ok(s) => s,
            Err(_) => {
                parse_fail_count += 1;
                if parse_fail_count <= 3 || parse_fail_count.is_multiple_of(100) {
                    let preview: String = payload
                        .iter()
                        .take(40)
                        .map(|b| format!("{:02x}", b))
                        .collect::<Vec<_>>()
                        .join(" ");
                    warn!(
                        "PstRotator listener: non-UTF8 packet from {} ({} bytes, hex preview: {}), parse-fail #{}",
                        peer, n, preview, parse_fail_count
                    );
                }
                continue;
            }
        };
        let az_deg = match parse_packet(text) {
            Packet::GoTo(v) => {
                // Markeer dat we recent een echte AG-goto hadden zodat
                // AZ-broadcasts binnen het feedback-window genegeerd worden.
                last_ag_at = Some(Instant::now());
                v
            }
            Packet::GoToAz(v) => {
                // Operator-bevinding 2026-06-05: PstRotator's simulator
                // broadcastet `AZ:nnn` parallel aan de goto-stream.
                // Binnen het feedback-window na een echte AG: dump
                // als simulator-noise. Daarbuiten: behandel als goto
                // (AZ-only setups blijven werken).
                let within_window = last_ag_at
                    .map(|t| t.elapsed() < AZ_FEEDBACK_AFTER_AG)
                    .unwrap_or(false);
                if within_window {
                    debug!(
                        "PstRotator listen: AZ:{:.1}° from {} dropped - simulator broadcast within {}s of AG-goto",
                        v, peer, AZ_FEEDBACK_AFTER_AG.as_secs()
                    );
                    continue;
                }
                v
            }
            Packet::StatusQuery(proto) => {
                // Bidirectional: PstRotator polt regelmatig. Zonder reply
                // blijft hij in polling-mode en verstuurt onze goto-
                // commando's niet. Reply-format moet matchen met het
                // protocol dat de query gebruikte.
                let status = rotor.status();
                let angle_int = (status.angle_x10 as f32 / 10.0).round() as u16;
                let reply = match proto {
                    QueryProtocol::Gs232C => format!("+{:03}\r", angle_int),
                    QueryProtocol::Gs232C2 => {
                        // GS-232A C2 reply: `+0aaa+0eee\r`; we hebben
                        // geen elevation-axis, dus el=000.
                        format!("+0{:03}+0000\r", angle_int)
                    }
                    QueryProtocol::Prosistel => {
                        let rb = if status.rotating { 'B' } else { 'R' };
                        format!("\u{0002}A,?,{:03},{}\r", angle_int, rb)
                    }
                    QueryProtocol::PstXml => {
                        // PstRotator native reply: `AZ:nnn.n<CR>` - wat
                        // Log4OM verwacht in PstRotator-emulation pad.
                        let angle_deg = status.angle_x10 as f32 / 10.0;
                        format!("AZ:{:.1}\r", angle_deg)
                    }
                };
                if let Err(e) = sock.send_to(reply.as_bytes(), peer) {
                    warn!(
                        "PstRotator listen: reply to {} faalde: {}",
                        peer, e
                    );
                } else {
                    debug!(
                        "PstRotator listen: {:?} reply -> {} angle={}",
                        proto, peer, angle_int
                    );
                }
                continue;
            }
            Packet::Stop => {
                // CHANGELOG / Manual claimen Stop-support per protocol.
                // Een externe Stop (Yaesu `S\r`, Prosistel `\x02AR\r` /
                // `AG999`, PST-XML `<STOP>`) wordt doorgezet naar de
                // rotor-facade.
                info!("PstRotator listen: Stop from {} -> RotorCmd::Stop", peer);
                rotor.send_command(RotorCmd::Stop);
                continue;
            }
            Packet::ManualRotate => {
                info!("PstRotator listen: manual rotate R/L from {} (ignored - no continuous-rotate API)", peer);
                continue;
            }
            Packet::Elevation => {
                debug!("PstRotator listen: EL/elevation packet (ignored)");
                continue;
            }
            Packet::Metadata => {
                debug!(
                    "PstRotator listen: metadata-tag from {} (ignored): {:?}",
                    peer,
                    text.trim()
                );
                continue;
            }
            Packet::Unknown => {
                parse_fail_count += 1;
                if parse_fail_count <= 3 || parse_fail_count.is_multiple_of(100) {
                    warn!(
                        "PstRotator listener: unrecognised packet from {}: {:?} (parse-fail #{})",
                        peer,
                        text.trim(),
                        parse_fail_count
                    );
                }
                continue;
            }
        };
        // Compass-azimuth (0..360°) -> mechanische rotor-target. Bij
        // overlap-rotors (`max_deg > 360`) bestaan voor compass 0..(max-360)°
        // twee mechanische posities (X en X+360); kies degene het dichtst
        // bij de huidige rotor-positie. Operator-scenario: max_deg=450, dus
        // compass 0..90° kan op mech 0..90° óf mech 360..450°.
        let base_az_x10 = (az_deg.clamp(0.0, 360.0) * 10.0).round() as u16;
        let max_deg_x10 = rotor.max_deg_x10();
        let current_x10 = rotor.status().angle_x10;
        let az_x10 = pick_mechanical_target(base_az_x10, max_deg_x10, current_x10);
        let now = Instant::now();
        let is_duplicate = match (last_az_x10, last_dispatch_at) {
            (Some(prev), Some(t)) => {
                let diff = az_x10.abs_diff(prev);
                diff <= AZIMUTH_DEDUPE_EPSILON_X10 && now.duration_since(t) < DEDUPE_INTERVAL
            }
            _ => false,
        };
        if is_duplicate {
            debug!(
                "PstRotator listener: deduped {:.1}° (same as last within {:?})",
                az_deg, DEDUPE_INTERVAL
            );
            continue;
        }
        // Auto-track-feedback filter: een incoming GoTo waarvan het
        // mech-target binnen FEEDBACK_TO_CURRENT_EPSILON_X10 ligt van
        // de huidige rotor-positie is een no-op-echo van een externe
        // controller die de rotor volgt (PstRotator "follow rotor"
        // mode). Zonder filter overschrijft die echo elke fresh
        // TL2-client target binnen ~300 ms.
        if az_x10.abs_diff(current_x10) <= FEEDBACK_TO_CURRENT_EPSILON_X10 {
            debug!(
                "PstRotator listen UDP: dropped feedback-echo {:.1}° (current {:.1}°) from {}",
                az_x10 as f32 / 10.0,
                current_x10 as f32 / 10.0,
                peer
            );
            continue;
        }
        // Dispatch GoTo via de Rotor-facade. `send_command` slikt het
        // channel-closed-resultaat zelf, dus we kunnen niet detecteren
        // dat de cmd niet aankomt - bij channel-disconnect zou de
        // rotor-backend toch al uit zijn. De info-log hieronder
        // documenteert de poging.
        rotor.send_command(RotorCmd::GoTo(az_x10));
        last_az_x10 = Some(az_x10);
        last_dispatch_at = Some(now);
        info!(
            "PstRotator listen: compass {:.1}° -> mech {:.1}° from {} (cur={:.1}°, max={:.1}°, packets={}, parse-fail={})",
            az_deg,
            az_x10 as f32 / 10.0,
            peer,
            current_x10 as f32 / 10.0,
            max_deg_x10 as f32 / 10.0,
            packet_count,
            parse_fail_count
        );
    }
    info!(
        "PstRotator listener stopping (port {}, total packets={}, parse-fail={})",
        port, packet_count, parse_fail_count
    );
}

/// TCP-listener accepteert connecties van PstRotator TCP-clients en
/// spawnt per client een handler-thread die line-delimited commando's
/// leest, parsed, en GoTo's dispatcht. Connecties blijven persistent
/// totdat de client disconnect.
fn run_tcp(listener: TcpListener, rotor: Rotor, shutdown: Arc<AtomicBool>, port: u16) {
    info!("PstRotator TCP listener active on port {}", port);
    while !shutdown.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, peer)) => {
                info!("PstRotator TCP client connected from {}", peer);
                let r = rotor.clone();
                let s = shutdown.clone();
                std::thread::Builder::new()
                    .name(format!("pstrotator-tcp-{}", peer))
                    .spawn(move || {
                        handle_tcp_client(stream, r, s, peer);
                    })
                    .ok();
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => {
                warn!("PstRotator TCP accept error: {}", e);
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    }
    info!("PstRotator TCP listener stopping (port {})", port);
}

fn handle_tcp_client(
    stream: TcpStream,
    rotor: Rotor,
    shutdown: Arc<AtomicBool>,
    peer: SocketAddr,
) {
    stream
        .set_read_timeout(Some(READ_TIMEOUT))
        .ok();
    let writer = match stream.try_clone() {
        Ok(s) => s,
        Err(e) => {
            warn!("PstRotator TCP: cannot clone stream from {}: {}", peer, e);
            return;
        }
    };
    let mut writer = writer;
    let reader = BufReader::new(stream);
    let mut packet_count: u64 = 0;
    let mut parse_fail_count: u64 = 0;
    // Target-sync state: onthouden welk target PstRotator zelf
    // gestuurd heeft (M/AG) en welk target we als laatst naar
    // PstRotator gepushed hebben. Wanneer rotor.target_x10 wijzigt
    // naar een waarde die geen van beide is, kwam de GoTo vanuit
    // TL2 (server-UI of TCI-client) en pushen we 'm zodat
    // PstRotator's compass ook het nieuwe target laat zien.
    let mut last_received_target_x10: Option<u16> = None;
    let mut last_pushed_target_x10: Option<u16> = None;
    // Detecteer welk protocol PstRotator gebruikt zodat de push het
    // matchende formaat heeft. Default Prosistel (operator's EA7HG-mode
    // is het meest voorkomende) tot we anders horen via inkomende
    // commando's of queries.
    let mut peer_protocol: ProtocolKind = ProtocolKind::Prosistel;
    // PstRotator stuurt typisch `\r`-eindigde commando's. BufRead::lines()
    // split alleen op `\n` dus we lezen byte-voor-byte en split zelf op
    // CR óf LF om beide formats te ondersteunen.
    let mut buf = Vec::with_capacity(64);
    let mut byte = [0u8; 1];
    use std::io::Read;
    let mut reader = reader;
    while !shutdown.load(Ordering::Relaxed) {
        match reader.read(&mut byte) {
            Ok(0) => {
                info!("PstRotator TCP client {} closed connection", peer);
                break;
            }
            Ok(_) => {
                if byte[0] == b'\r' || byte[0] == b'\n' {
                    if buf.is_empty() {
                        continue;
                    }
                    let raw_preview: String = buf
                        .iter()
                        .map(|b| {
                            if (0x20..=0x7e).contains(b) {
                                (*b as char).to_string()
                            } else {
                                format!("\\x{:02x}", b)
                            }
                        })
                        .collect();
                    packet_count += 1;
                    debug!(
                        "PstRotator TCP RX from {} ({} bytes): {:?}",
                        peer,
                        buf.len(),
                        raw_preview
                    );
                    let text = match std::str::from_utf8(&buf) {
                        Ok(s) => s.to_string(),
                        Err(_) => {
                            parse_fail_count += 1;
                            warn!(
                                "PstRotator TCP: non-UTF8 line from {} (parse-fail #{})",
                                peer, parse_fail_count
                            );
                            buf.clear();
                            continue;
                        }
                    };
                    buf.clear();
                    match parse_packet(&text) {
                        Packet::GoTo(v) | Packet::GoToAz(v) => {
                            // TCP is connection-oriented, geen simulator-
                            // broadcasts; behandel AZ over TCP altijd als
                            // een echte goto (anders dan UDP-pad).
                            let max_deg_x10 = rotor.max_deg_x10();
                            let current_x10 = rotor.status().angle_x10;
                            let base_x10 = (v.clamp(0.0, 360.0) * 10.0).round() as u16;
                            let chosen = pick_mechanical_target(base_x10, max_deg_x10, current_x10);
                            // Auto-track-feedback filter (zie UDP-pad);
                            // PstRotator's "follow rotor" / auto-track
                            // mode echoot de gemeten positie als nieuwe
                            // goto en kan zo een fresh TL2-client-target
                            // overschrijven binnen ~300 ms.
                            if chosen.abs_diff(current_x10) <= FEEDBACK_TO_CURRENT_EPSILON_X10 {
                                debug!(
                                    "PstRotator TCP: dropped feedback-echo {:.1}° (current {:.1}°) from {}",
                                    chosen as f32 / 10.0,
                                    current_x10 as f32 / 10.0,
                                    peer
                                );
                                if let Some(k) = protocol_kind_of_goto_text(&text) {
                                    peer_protocol = k;
                                }
                                continue;
                            }
                            info!(
                                "PstRotator TCP: GoTo compass {:.1}° -> mech {:.1}° from {} (cur={:.1}°)",
                                v,
                                chosen as f32 / 10.0,
                                peer,
                                current_x10 as f32 / 10.0
                            );
                            rotor.send_command(RotorCmd::GoTo(chosen));
                            // Onthouden voor target-sync: deze GoTo
                            // kwam van PstRotator zelf, dus géén
                            // M<target> terug-pushen wanneer de rotor-
                            // status straks deze waarde laat zien.
                            last_received_target_x10 = Some(chosen);
                            // Protocol-detectie uit inkomende goto
                            // (Prosistel AAG/AG vs GS-232 M).
                            if let Some(k) = protocol_kind_of_goto_text(&text) {
                                peer_protocol = k;
                            }
                        }
                        Packet::StatusQuery(proto) => {
                            peer_protocol = protocol_kind_of_query(proto);
                            let status = rotor.status();
                            let angle_int = (status.angle_x10 as f32 / 10.0).round() as u16;
                            let reply = match proto {
                                QueryProtocol::Gs232C => format!("+{:03}\r", angle_int),
                                QueryProtocol::Gs232C2 => format!("+0{:03}+0000\r", angle_int),
                                QueryProtocol::Prosistel => {
                                    let rb = if status.rotating { 'B' } else { 'R' };
                                    format!("\u{0002}A,?,{:03},{}\r", angle_int, rb)
                                }
                                QueryProtocol::PstXml => {
                                    let angle_deg = status.angle_x10 as f32 / 10.0;
                                    format!("AZ:{:.1}\r", angle_deg)
                                }
                            };
                            if let Err(e) = writer.write_all(reply.as_bytes()) {
                                warn!(
                                    "PstRotator TCP: reply to {} faalde: {}",
                                    peer, e
                                );
                                break;
                            }
                        }
                        Packet::Stop => {
                            // CHANGELOG / Manual claimen Stop-support
                            // - zelfde semantiek als de UDP-pad.
                            info!("PstRotator TCP: Stop from {} -> RotorCmd::Stop", peer);
                            rotor.send_command(RotorCmd::Stop);
                        }
                        Packet::ManualRotate => {
                            info!("PstRotator TCP: manual rotate from {} (ignored)", peer);
                        }
                        Packet::Elevation => {
                            debug!("PstRotator TCP: elevation packet from {} (ignored)", peer);
                        }
                        Packet::Metadata => {
                            debug!(
                                "PstRotator TCP: metadata-tag from {} (ignored): {:?}",
                                peer,
                                text.trim()
                            );
                        }
                        Packet::Unknown => {
                            parse_fail_count += 1;
                            warn!(
                                "PstRotator TCP: unrecognised packet from {}: {:?} (parse-fail #{})",
                                peer, text.trim(), parse_fail_count
                            );
                        }
                    }
                } else {
                    buf.push(byte[0]);
                    if buf.len() > 256 {
                        warn!(
                            "PstRotator TCP: oversized line from {} (>{} bytes), resetting buffer",
                            peer, 256
                        );
                        buf.clear();
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock
                || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                // Idle moment - gebruik dit om TL2-originated targets
                // door te pushen naar PstRotator zodat zijn compass
                // ook de target-aanwijzer ziet (anders zou hij alleen
                // de huidige positie volgen via zijn polling).
                let cur_target_x10 = rotor.status().target_x10;
                if cur_target_x10 != 0
                    && Some(cur_target_x10) != last_received_target_x10
                    && Some(cur_target_x10) != last_pushed_target_x10
                {
                    // Nieuw target dat niet van PstRotator zelf kwam
                    // en nog niet eerder gepushed is. Vertaal naar het
                    // protocol dat PstRotator op deze connectie
                    // gebruikt (gedetecteerd uit eerdere queries /
                    // gotos; default Prosistel voor EA7HG).
                    // 3 digits, modulo 360 voor overlap-rotors zodat
                    // PstRotator's compass de compass-azimuth toont,
                    // niet de mech-positie.
                    let compass = ((cur_target_x10 as u32) / 10) % 360;
                    let push = match peer_protocol {
                        ProtocolKind::Prosistel => {
                            format!("\u{0002}AG{:03}\r", compass)
                        }
                        ProtocolKind::Gs232 => format!("M{:03}\r", compass),
                    };
                    if let Err(e) = writer.write_all(push.as_bytes()) {
                        warn!(
                            "PstRotator TCP: target push to {} faalde: {}",
                            peer, e
                        );
                        break;
                    }
                    info!(
                        "PstRotator TCP: pushed TL2-origin target {}° -> {} via {:?} (mech {}°)",
                        compass,
                        peer,
                        peer_protocol,
                        cur_target_x10 / 10
                    );
                    last_pushed_target_x10 = Some(cur_target_x10);
                }
                continue;
            }
            Err(e) => {
                warn!("PstRotator TCP read error from {}: {}", peer, e);
                break;
            }
        }
    }
    info!(
        "PstRotator TCP client {} disconnected (packets={}, parse-fail={})",
        peer, packet_count, parse_fail_count
    );
}

/// Welk protocol-format gebruikt PstRotator op deze connectie? Wordt
/// gedetecteerd uit inkomende commando's/queries en bepaalt het format
/// van uitgaande target-pushes (TL2 -> PstRotator UI-sync).
#[derive(Debug, PartialEq, Clone, Copy)]
enum ProtocolKind {
    /// Prosistel binair (EA7HG controller): goto = `\x02AG<nnn>\r`,
    /// query = `\x02A?\r` / `AA?\r`. Default voor onbekende clients.
    Prosistel,
    /// Yaesu GS-232A/B tekst: goto = `M<nnn>\r`, query = `C\r` / `C2\r`.
    Gs232,
}

/// Welke protocol-familie het query-packet gebruikte. Bepaalt het
/// reply-formaat zodat de zender de string kan parsen.
#[derive(Debug, PartialEq, Clone, Copy)]
enum QueryProtocol {
    /// Yaesu GS-232A `C\r` -> reply `+<nnn>\r`.
    Gs232C,
    /// Yaesu GS-232A `C2\r` -> reply `+0aaa+0eee\r`.
    Gs232C2,
    /// Prosistel `\x02A?\r` of `AA?\r` -> reply `\x02A,?,<nnn>,<R|B>\r`.
    Prosistel,
    /// PstRotator XML `<PST>AZ?</PST>` (Log4OM emulation pad) -> reply
    /// `AZ:<nnn.n>\r`. Log4OM stuurt dit naar PstRotator's host/poort;
    /// TL2 vangt het op als drop-in vervanger voor PstRotator.
    PstXml,
}

/// Classificatie van een PstRotator-packet.
#[derive(Debug, PartialEq)]
enum Packet {
    /// Echte goto-commando vanuit een AG/M-stroom (Prosistel binair of
    /// Yaesu GS-232A). Wordt altijd dispatched.
    GoTo(f32),
    /// Goto uit een AZ:nnn text broadcast. Wordt door PstRotator óók
    /// gebruikt voor zijn eigen simulator-positie-updates, dus binnen
    /// `AZ_FEEDBACK_AFTER_AG` na een echte AG-goto behandelen we dit
    /// als feedback (silent drop). Daarbuiten als echte goto (voor
    /// AZ-only setups).
    GoToAz(f32),
    /// Status-query; listener antwoordt met huidige rotor-positie in
    /// het overeenkomstige reply-formaat.
    StatusQuery(QueryProtocol),
    /// Stop-commando (`AAR`, `\x02AR\r`, `AG999` in EA7HG-variant, `S\r`
    /// in GS-232A, of `<STOP>` in PstRotator-XML). Wordt doorgestuurd
    /// als `RotorCmd::Stop` zodat een externe controller de rotor kan
    /// stilzetten (CHANGELOG belooft dit per protocol).
    Stop,
    /// Manual rotate-knop (`R\r` / `L\r` in GS-232A). Geen target,
    /// kunnen we niet zinvol doorzetten - gewoon loggen.
    ManualRotate,
    /// Elevation-reply / XML - overgeslagen, geen elevation-axis.
    Elevation,
    /// Metadata-tags die Log4OM meestuurt naast de azimuth: callsign,
    /// naam, QTH, frequentie, mode, etc. PstRotator gebruikt die voor
    /// zijn display; TL2 negeert ze stil (geen warn-spam).
    /// Voorbeelden: `<PST><CALL>PA0XYZ</CALL></PST>`,
    /// `<PST><NAME>...</NAME></PST>`, `<PST><QTH>...</QTH></PST>`.
    Metadata,
    /// Onbekend formaat.
    Unknown,
}

/// Parse een PstRotator-packet. Accepteert (in volgorde):
/// 1. Prosistel binair single-A: `\x02A?\r` (query), `\x02AG<nnn>\r` (goto),
///    `\x02AR\r` (stop). STX optioneel.
/// 2. Prosistel binair double-A: `AA?` / `AAG<nnn>\r` / `AAR\r`
///    (EA7HG/PstRotator alternative encoding).
/// 3. Text reply format: `AZ:nnn.n\r` (PstRotator's reply broadcast).
/// 4. XML-mode: `<PST><AZIMUTH>nnn.n</AZIMUTH>...</PST>`.
///
/// `EL:` / `<ELEVATION>` skip met `Elevation`. `AAG999` (park-positie)
/// skip met `Park` - geen mapping.
fn parse_packet(text: &str) -> Packet {
    let stripped: &str = text.trim_start_matches('\u{0002}');
    let trimmed = stripped
        .trim()
        .trim_end_matches(|c: char| c == '\r' || c == '\n');
    // Yaesu GS-232A - text protocol, single-char commands.
    if trimmed == "C" {
        return Packet::StatusQuery(QueryProtocol::Gs232C);
    }
    if trimmed == "C2" {
        return Packet::StatusQuery(QueryProtocol::Gs232C2);
    }
    if trimmed == "S" {
        return Packet::Stop;
    }
    if trimmed == "R" || trimmed == "L" {
        return Packet::ManualRotate;
    }
    // GS-232A move: `M<nnn>` (3 digits, optioneel met spaties).
    if let Some(rest) = trimmed.strip_prefix('M') {
        let digits: String = rest
            .trim_start()
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if !digits.is_empty() {
            if let Ok(n) = digits.parse::<u16>() {
                return Packet::GoTo(n as f32);
            }
        }
    }
    // Prosistel binair: strip 1-2 leading `A`'s; wat overblijft begint
    // met de actie-letter (G/R/?). Operator's PstRotator stuurt `\x02A?\r`
    // (single-A), de bestaande rotor.rs-backend stuurt `AA?` (double-A).
    let prosistel_rest = trimmed
        .strip_prefix("AA")
        .or_else(|| trimmed.strip_prefix(['A', 'a']));
    if let Some(rest) = prosistel_rest {
        if let Some(digits) = rest.strip_prefix(['G', 'g']) {
            let digits: String = digits.chars().take_while(|c| c.is_ascii_digit()).collect();
            if !digits.is_empty() {
                if let Ok(n) = digits.parse::<u16>() {
                    if n == 999 {
                        // Operator-bevinding (2026-06-05): in EA7HG-UDP-mode
                        // stuurt PstRotator `AG999` als STOP-signaal, niet
                        // als "park to 999°". Classificeer als Stop.
                        return Packet::Stop;
                    }
                    return Packet::GoTo(n as f32);
                }
            }
        }
        if rest.starts_with(['R', 'r']) {
            return Packet::Stop;
        }
        if rest.starts_with('?') {
            return Packet::StatusQuery(QueryProtocol::Prosistel);
        }
    }
    // Text format: AZ:nnn.n (case-insensitive). Treat this as a GoTo command;
    // feedback-only handling is intentionally limited to the explicit feedback
    // packet forms above so simulator streams keep their legacy behaviour.
    if let Some(rest) = strip_prefix_ci(trimmed, "AZ:") {
        if let Ok(v) = rest.trim().parse::<f32>() {
            return Packet::GoToAz(v);
        }
    }
    if strip_prefix_ci(trimmed, "EL:").is_some() {
        return Packet::Elevation;
    }
    // XML format (PstRotator native, ook gebruikt door Log4OM in
    // PstRotator-emulation pad):
    //   `<PST><AZIMUTH>nnn.n</AZIMUTH>...</PST>`   - goto
    //   `<PST>AZ?</PST>`                           - query
    //   `<PST><STOP>1</STOP></PST>`                - stop
    let lower = trimmed.to_ascii_lowercase();
    if lower.contains("<pst>") && lower.contains("az?") {
        return Packet::StatusQuery(QueryProtocol::PstXml);
    }
    if lower.contains("<stop>") {
        return Packet::Stop;
    }
    if let Some(open) = lower.find("<azimuth>") {
        let after = &trimmed[open + "<azimuth>".len()..];
        if let Some(close) = after.to_ascii_lowercase().find("</azimuth>") {
            if let Ok(v) = after[..close].trim().parse::<f32>() {
                return Packet::GoTo(v);
            }
        }
    }
    if lower.contains("<elevation>") {
        return Packet::Elevation;
    }
    // PstRotator-XML metadata-tags (Log4OM stuurt deze mee bij elke
    // spot-klik): call sign, naam, QTH, frequentie, mode, country, etc.
    // Stil droppen zodat ze geen "unrecognised packet"-warns vullen.
    if lower.contains("<pst>")
        && (lower.contains("<call>")
            || lower.contains("<name>")
            || lower.contains("<qth>")
            || lower.contains("<country>")
            || lower.contains("<frequency>")
            || lower.contains("<freq>")
            || lower.contains("<mode>")
            || lower.contains("<grid>")
            || lower.contains("<locator>")
            || lower.contains("<comment>")
            || lower.contains("<continent>"))
    {
        return Packet::Metadata;
    }
    Packet::Unknown
}

/// Backwards-compatible wrapper voor de unit-tests. Returnt Some voor
/// elke kind van goto-extractie (AG/M of AZ).
#[cfg(test)]
fn parse_azimuth(text: &str) -> Option<f32> {
    match parse_packet(text) {
        Packet::GoTo(v) | Packet::GoToAz(v) => Some(v),
        _ => None,
    }
}

/// Kies de mechanische target dichtst bij `current` voor een gegeven
/// compass-azimuth. Bij `max_deg_x10 > 3600` (overlap-rotor zoals Yaesu
/// G-1000DXC met max=450°) is de compass-azimuth `base` ook bereikbaar
/// als `base + 3600` zolang die binnen max blijft - kies de variant met
/// de kortste mechanische reis vanuit `current`.
fn pick_mechanical_target(base_x10: u16, max_deg_x10: u16, current_x10: u16) -> u16 {
    let primary = base_x10.min(max_deg_x10);
    // Alternatief alleen relevant als `base + 360°` ook binnen range valt.
    if max_deg_x10 > 3600 && (base_x10 as u32) + 3600 <= max_deg_x10 as u32 {
        let alt = base_x10 + 3600;
        let dist_primary = (primary as i32 - current_x10 as i32).unsigned_abs();
        let dist_alt = (alt as i32 - current_x10 as i32).unsigned_abs();
        if dist_alt < dist_primary {
            return alt;
        }
    }
    primary
}

/// Mappers van QueryProtocol -> ProtocolKind voor protocol-detectie
/// uit inkomende status-queries.
fn protocol_kind_of_query(p: QueryProtocol) -> ProtocolKind {
    match p {
        QueryProtocol::Prosistel => ProtocolKind::Prosistel,
        QueryProtocol::Gs232C | QueryProtocol::Gs232C2 => ProtocolKind::Gs232,
        // PstXml gebruikt een geheel ander format; behandeld als
        // Prosistel-equivalent voor target-push (PstRotator-native
        // commando is `<PST><AZIMUTH>` maar dat is geen "controller
        // sends to rotor" gebruik bij Log4OM-emulatie).
        QueryProtocol::PstXml => ProtocolKind::Prosistel,
    }
}

/// Detecteer protocol uit een geparste goto-packet text. Prosistel
/// commando's bevatten `AA` / `\x02A` prefix, GS-232 begint met `M`.
/// Bij twijfel (AZ-text) returnt None - laat de caller default-waarde
/// behouden.
fn protocol_kind_of_goto_text(text: &str) -> Option<ProtocolKind> {
    let s = text.trim_start_matches('\u{0002}').trim();
    if s.starts_with("AA") || s.starts_with('A') && (s.starts_with("AG") || s.starts_with("Ag")) {
        Some(ProtocolKind::Prosistel)
    } else if s.starts_with('M') || s.starts_with('m') {
        Some(ProtocolKind::Gs232)
    } else {
        None
    }
}

fn strip_prefix_ci<'a>(haystack: &'a str, prefix: &str) -> Option<&'a str> {
    if haystack.len() < prefix.len() {
        return None;
    }
    if haystack[..prefix.len()].eq_ignore_ascii_case(prefix) {
        Some(&haystack[prefix.len()..])
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_text_format() {
        // AZ:nn -> GoTo (revert van build 8's feedback-only naar
        // operator-known-working state; zie parse_packet kommentaar).
        assert_eq!(parse_azimuth("AZ:123.4"), Some(123.4));
        assert_eq!(parse_azimuth("az:0.0\r"), Some(0.0));
        assert_eq!(parse_azimuth("  AZ:359.9  \r\n"), Some(359.9));
    }

    #[test]
    fn parses_xml_format() {
        assert_eq!(
            parse_azimuth("<PST><AZIMUTH>271.5</AZIMUTH></PST>"),
            Some(271.5)
        );
        assert_eq!(
            parse_azimuth("<pst><azimuth>0</azimuth></pst>"),
            Some(0.0)
        );
    }

    #[test]
    fn ignores_elevation() {
        assert_eq!(parse_azimuth("EL:45.2"), None);
        assert_eq!(parse_azimuth("<PST><ELEVATION>30</ELEVATION></PST>"), None);
    }

    #[test]
    fn ignores_junk() {
        assert_eq!(parse_azimuth("hello world"), None);
        assert_eq!(parse_azimuth("AZIMUTH 90"), None);
        assert_eq!(parse_azimuth(""), None);
    }

    #[test]
    fn parses_prosistel_aag() {
        // PstRotator EA7HG UDP mode stuurt `AAG<nnn>\r`.
        assert_eq!(parse_azimuth("AAG090\r"), Some(90.0));
        assert_eq!(parse_azimuth("AAG000"), Some(0.0));
        assert_eq!(parse_azimuth("AAG359\r\n"), Some(359.0));
        assert_eq!(parse_azimuth("AAG270"), Some(270.0));
        // 4 cijfers ondersteund voor toekomstige rotors > 360
        assert_eq!(parse_azimuth("AAG0450"), Some(450.0));
    }

    #[test]
    fn ignores_prosistel_non_goto() {
        // AG999/AAG999 wordt als STOP (Packet::Stop) geclassificeerd
        // - operator-bevinding 2026-06-05. parse_azimuth returnt None
        // omdat het géén GoTo is, ongeacht of het Stop of Park is.
        assert_eq!(parse_azimuth("AAG999"), None);
        // Stop en query worden geskipt.
        assert_eq!(parse_azimuth("AAR\r"), None);
        assert_eq!(parse_azimuth("AA?\r"), None);
    }

    #[test]
    fn picks_overlap_when_closer() {
        // Yaesu G-1000DXC met max=450°.
        // Compass 30° (base=300) - primaire mech-target = 300, alt = 3900.
        // Bij huidige positie mech 350° (3500): alt 3900 is 400 weg, primary
        // is 3200 weg -> kies alt (overlap-route).
        assert_eq!(pick_mechanical_target(300, 4500, 3500), 3900);
        // Vanuit mech 0° (0): primary 300 (afstand 300), alt 3900 (afstand 3900) -> primary.
        assert_eq!(pick_mechanical_target(300, 4500, 0), 300);
        // Compass 91° (base=910): geen alt mogelijk (910+3600=4510 > 4500) -> primary.
        assert_eq!(pick_mechanical_target(910, 4500, 4400), 910);
    }

    #[test]
    fn no_overlap_for_360_rotors() {
        // max_deg=360 -> geen alternatief mogelijk, altijd primary.
        assert_eq!(pick_mechanical_target(300, 3600, 100), 300);
        assert_eq!(pick_mechanical_target(0, 3600, 3500), 0);
    }

    #[test]
    fn handles_stx_prefix() {
        // Prosistel-replies komen soms met STX(0x02) prefix.
        assert_eq!(parse_azimuth("\u{0002}AAG180\r"), Some(180.0));
    }

    #[test]
    fn classifies_status_query() {
        // EA7HG/Prosistel single-A query met STX-prefix.
        assert_eq!(parse_packet("\u{0002}A?\r"), Packet::StatusQuery(QueryProtocol::Prosistel));
        // Double-A variant.
        assert_eq!(parse_packet("AA?\r"), Packet::StatusQuery(QueryProtocol::Prosistel));
        assert_eq!(parse_packet("\u{0002}AA?\r"), Packet::StatusQuery(QueryProtocol::Prosistel));
    }

    #[test]
    fn classifies_gs232a_protocol() {
        // Yaesu GS-232A commands.
        assert_eq!(parse_packet("M090\r"), Packet::GoTo(90.0));
        assert_eq!(parse_packet("M000"), Packet::GoTo(0.0));
        assert_eq!(parse_packet("M450"), Packet::GoTo(450.0));
        assert_eq!(parse_packet("S\r"), Packet::Stop);
        assert_eq!(parse_packet("C\r"), Packet::StatusQuery(QueryProtocol::Gs232C));
        assert_eq!(parse_packet("C2\r"), Packet::StatusQuery(QueryProtocol::Gs232C2));
        assert_eq!(parse_packet("R\r"), Packet::ManualRotate);
        assert_eq!(parse_packet("L\r"), Packet::ManualRotate);
    }

    #[test]
    fn classifies_stop_and_goto_single_a() {
        // Single-A variants.
        assert_eq!(parse_packet("\u{0002}AR\r"), Packet::Stop);
        assert_eq!(parse_packet("\u{0002}AG090\r"), Packet::GoTo(90.0));
        // Double-A variants.
        assert_eq!(parse_packet("AAR\r"), Packet::Stop);
        assert_eq!(parse_packet("AAG090\r"), Packet::GoTo(90.0));
    }

    #[test]
    fn classifies_other_packet_kinds() {
        // Operator-bevinding 2026-06-05: AG999 = STOP-signaal in PstRotator
        // EA7HG-UDP, niet "park to 999°".
        assert_eq!(parse_packet("AAG999"), Packet::Stop);
        assert_eq!(parse_packet("\u{0002}AG999\r"), Packet::Stop);
        assert_eq!(parse_packet("EL:45.0"), Packet::Elevation);
        assert_eq!(parse_packet("hello"), Packet::Unknown);
    }
}
