# Changelog

All notable changes to ThetisLink are documented in this file. The format is
loosely based on [Keep a Changelog](https://keepachangelog.com/) and the
project follows [Semantic Versioning](https://semver.org/). Public releases
are tagged on the [`cjenschede/ThetisLink`](https://github.com/cjenschede/ThetisLink)
mirror and shipped as zipped binary bundles via that repository's Releases
page; this file is the user-facing summary so an upgrade-decision can be made
from one document.

For the protocol-level technical reference and the in-depth multi-tuner
hardware notes, see `docs-book/src/technical-reference.md` and
`docs-book/src/user-manual-en.md` (English) or
`docs-book/src/technische-referentie.md` and `docs-book/src/user-manual.md`
(Dutch).

---

## [2.4.2] — 2026-07-16 (Recorded-audio playback to the radio: clean modulation · Thetis TX-EQ bypass · play-volume)

> **Patch release.** One additive wire-protocol addition only (a new client→server control
> `ThetisTxeq = 0x90`); `VERSION` stays 3, so a direct connection stays fully interoperable
> with v2.4.0 / v2.4.1 (an older peer simply ignores the new control). Stock Thetis v2.10.3.15
> is sufficient — no Thetis-fork change. Android is functionally unchanged; the APK is rebuilt
> at 2.4.2.

### Fixed
- **Recorded audio transmitted through the radio was overmodulated / distorted.** Playback to
  the transmitter (TX inject) ran through the live-microphone processing chain — 5-band EQ,
  compressor, AGC and the 4× mic-gain boost — so an already line-level recording clipped.
  Playback now bypasses the mic chain entirely for Thetis and both Yaesu radios: the recording
  goes out clean at line level (only the play-volume scaling is applied). Live-microphone
  transmit is unchanged.
- **Playback to the 2nd Yaesu (FTX-1 / radio 2) did not come through.** The WAV-TX inject path
  was missing the radio-2 PTT case, so a recording only reached the main radio and the first
  Yaesu.
- **RX audio was muted on all receivers during transmit.** Pressing PTT on one receiver
  silenced the audio of every other receiver in that client. RX audio now stays audible during
  TX; the internal-speaker mute (to suppress the PTT plop) applies only when PTT
  spike-protection is enabled.
- **State was not reset after a playback ended or on disconnect.** The live microphone could
  stay mic-chain-bypassed after a recording finished, and Thetis TX-EQ could be left off after
  a manual disconnect or app shutdown mid-playback. Both are now reset / restored on teardown.

### Added
- **Thetis TX-EQ is bypassed automatically while a recording is played to the main radio,** and
  restored afterwards — mirroring Thetis's own record/playback behaviour, so the transmit
  profile's EQ does not colour the recording. The server reads the operator's actual TX-EQ
  setting first and restores that exact state (falls back to on if it cannot read it).
- **Play-volume slider** next to Play/Stop (0–2×) to trim the level of a recording sent to the
  transmitter.
- **Transmit level meter during playback** — the audio bar shows the level of the audio
  actually being transmitted while a recording plays, instead of the (muted) microphone.

### Notes
- Recordings at a sample rate other than 8 kHz or 16 kHz (only possible with an externally
  imported WAV — ThetisLink records only 8/16 kHz) are now refused with a clear log message
  instead of being played back at the wrong speed.

## [2.4.1] — 2026-07-16 (Rotor link screen · Settings visibility · recorded-audio TX playback rate · FT-991A memory 100+)

> **Patch release.** Bug fixes only, no protocol or feature changes. Fully interoperable
> with v2.4.0; wire protocol `VERSION` stays 3. Stock Thetis v2.10.3.15 is sufficient.

### Fixed
- **Rotor (MCP2221A) could not be linked from a clean config.** The MCP2221A section had no
  "link an existing board" step for the rotor — tuners already had one — so an
  already-programmed `rot_` board could not be attached to the rotor slot from a fresh
  `thetislink-server.conf`; it stayed amber even though the scan detected the board. A link
  row (MCP-serial dropdown) now writes the rotor entry and selects the MCP2221A rotor backend
  in one action; "Add board → Rotor" also sets the backend.
- **Settings button disappeared after an MCP2221A scan.** The status/scan list could push the
  bottom Settings button out of view (the panel itself does not scroll), forcing an Exit +
  restart. The bottom controls now reserve the correct height, so Exit and Settings stay
  visible regardless of the list length.
- **Recorded audio played back too slowly / stuttering through the radio.** Playback to the
  transmitter (TX inject) ignored the recording's sample rate: a 16 kHz recording played at
  half speed on the main radio, and any recording played 3–6× too slow through a Yaesu.
  The TX-inject path is now sample-rate-aware (speaker playback was already correct).
  Recording itself was already correct for every channel (RX1/RX2/Yaesu 1-2/VRX1-2).
- **FT-991A memory channels 100–117 were not read.** The memory read stopped at channel 099,
  so the FT-991A PMS channels (100–117, i.e. P-1L…P-9U) never appeared in the memory editor —
  a channel stored at 100+ went missing. The read now covers the full CAT range 001–117 (the
  write side already accepted it). The FTX-1 is unaffected: its regular memory is 001–099 and
  its PMS uses a different (`P-01L`) addressing, so that path stays at 099.

## [2.4.0] — 2026-07-15 (Relay v2 UDP audio + auto TCP fallback · Yaesu SSB-over-USB & TX processing · full Android Yaesu parity · expanded monitoring · desktop themes)

> **A broad release.** Beyond the relay, this version brings remote **SSB transmit over
> the Yaesu USB audio**, client-side **TX compressor/AGC** and **clarifier** for the Yaesu,
> a large **Android Yaesu parity** push (dual-radio, full DSP panel, touch tuning, internal
> ATU), **mobile data-saving**, and a much **expanded connection monitor**.
>
> **Compatibility.** The TL wire-protocol `VERSION` stays 3 (additive), so a direct
> (LAN / port-forward) connection is fully backwards-compatible. The **relay** path is a
> coordinated upgrade: the relay, server and client (and Android) should all run 2.4.0
> together; a mixed setup keeps working but stays on the reliable wss/TCP audio path.
> Stock Thetis v2.10.3.15 is sufficient; no Thetis-fork change.

### Relay v2 — low-latency audio through the VPS relay
- **Audio + PTT over UDP.** Relayed audio now travels over bare UDP (no retransmit /
  head-of-line blocking) instead of wss/TCP, matching the direct-connection latency feel.
  Control and spectrum stay on the encrypted wss channel.
- **Automatic UDP↔wss fallback (make-before-break).** When a network blocks or degrades
  UDP, the audio auto-falls-back to the reliable wss/TCP path within ~2 s and returns to
  UDP once it recovers (with hysteresis so it never flaps). During transmit the audio is
  sent over both paths, so PTT is never lost. A small **transport indicator** ("UDP" /
  "TCP fallback") shows which path is active — on desktop and Android. There can be a
  brief (~0.5–1.5 s) gap only on a sudden total UDP loss; gradual degradation is seamless.
- **UDP capability-token rotation.** The per-session UDP token has a bounded TTL and is
  rotated over wss before it expires, so an active session never drops and no token
  outlives its lifetime. A choice of UDP (low latency) vs wss (encrypted) audio is exposed
  as a setting on desktop, server and Android.
- **Admin dashboard (web).** Manage stations and devices (block / rename), see per-device
  and per-station monthly usage, set data/device/client quotas, and download a one-click
  consistent **database backup**. Argon2id login, CSRF protection, per-IP login rate-limit;
  the API is internal-only behind the TLS reverse proxy.

### Yaesu — SSB transmit over the USB audio
- **SSB over the USB CODEC.** The Yaesu radios now transmit **SSB** using the streamed USB
  microphone audio, not just FM/DATA. On the **FT-991A** this switches SSB MIC SELECT=REAR
  + PORT SELECT=USB per PTT (restored on release); the **FTX-1** uses its own internal
  automatic modulation source, which ThetisLink leaves untouched. Previously SSB TX
  required the local hand mic.
- **FM auto-DATA unchanged; SSB does not use DATA.** The automatic FM → DATA-FM switch (for
  USB-mic FM TX) is unchanged. SSB stays in the normal SSB mode with the REAR/USB routing
  above — the earlier SSB → DATA-LSB/USB approach was reverted (carrier offset + narrow
  data filters are unsuitable for speech).
- **Hybrid per-PTT routing + Exit.** SSB USB-routing is applied per-PTT by default and
  restored between overs (presence-based restore in opt-out mode); an explicit **Exit**
  button returns the radio to its fixed MIC/DATA baseline. The TX-audio output is retried
  until the USB CODEC device is free.

### Yaesu — TX audio processing & clarifier
- **Client-side compressor + AGC, per radio.** A transmit compressor and an AGC toggle run
  in the client audio engine for the Yaesu TX branch (desktop and Android), independently
  per radio, alongside the existing per-radio TX EQ. Includes FTX-1 TX-mute and
  volume-restore fixes and a clean **AGC cycle** (FAST → MID → SLOW → AUTO).
- **Clarifier (RIT/XIT).** Clarifier control for both radios — RIT/XIT enable, offset
  steps and clear.

### Android — Yaesu parity
- **Dual-radio on Android.** A radio 1 / radio 2 selector brings the second Yaesu to the
  Android client, with per-radio PTT/volume routing.
- **Full DSP panel.** Collapsible DSP controls (ATT/AGC/NB/NR/IPO/Contour/APF/Notch/Proc/
  AMC) with adjustable sliders.
- **Touch frequency tuning.** A large tappable digit tuner plus a stepper for direct
  on-screen tuning of the Yaesu.
- **Internal ATU** (Tune + ATU on/off) and the **clarifier** (RIT/XIT + offset) on Android.

### Mobile data-saving
- **Yaesu-only clients skip the Thetis RX stream.** A client that only listens to a Yaesu
  radio no longer receives the Thetis RX audio, cutting mobile data.
- **On-demand Yaesu streaming.** Yaesu data is streamed only while its window is open/
  active (short spectrum grace on resume); a dynamic presence-push keeps a single Yaesu
  stream alive when shared.

### WebSDR
- **Reload button** for a quick recovery of the embedded WebSDR/KiwiSDR view after a
  network interruption.

### Desktop theming
- **Selectable UI themes** — Classic (light), Dark, Slate — plus a **Custom** theme with
  live colour pickers for background, widgets, text and the slider knob. Applied
  immediately and persisted.

### Connection monitoring
- **Greatly expanded per-stream statistics.** The Statistics panel now reports every audio
  stream separately — Thetis RX, Yaesu 1/2 and VRX1/VRX2 — each with its own jitter,
  jitter-buffer depth and packet count (and loss where the transport exposes it), alongside
  RTT and up/down bandwidth. The Down (RX) figure expands into a per-packet-type breakdown
  of the recent window, and the relay transport ("UDP" / "TCP fallback") is shown inline.
  On both desktop and Android.
- **Server-side bandwidth breakdown.** The server Status panel shows per-client down/up
  bandwidth, split into audio / spectrum / other, so it is clear where the traffic goes.

### Robustness fixes
- **Main window self-heal.** A window position saved on a since-disconnected or rearranged
  second monitor no longer opens off-screen — it falls back to the primary monitor.
- **Spectrum toggle no longer cuts RX1 audio** for clients that also have a Yaesu radio
  configured.
- **Jitter-buffer recovery.** The per-stream jitter buffers re-baseline on a large backward
  sequence jump (stream restart) and reset cleanly on disconnect, avoiding stuck audio.
- **Spectrum safety net.** A conservative default `max_bins` (2048) guards against an
  oversized spectrum request; the Android spectrum grace/`max_bins` handshake is honoured
  on connect and resume.

### Compliance / hygiene
- Refreshed dependency **SBOM** + **THIRD-PARTY-LICENSES** bundle; all licenses verified
  GPL-2.0-or-later compatible. Source line-endings normalized to LF; internal development
  markers removed from product source.

---

## [2.3.0] — 2026-06-27 (Synchronous AM (SAM-PLL) + AM auto-tune + TX modulation bandwidth)

> **Backwards-compatible with 2.1.x / 2.2.0.** Wire-protocol `VERSION` stays 3 —
> the new VRX-AFC and TX-filter packet/control types are purely additive
> (`0x2A`/`0x2B`, control `0x75`–`0x79`) and are sent only to clients that
> support them, so older clients keep working. Stock Thetis (v2.10.3.14+) is
> sufficient for the TX-bandwidth feature; no Thetis-fork update is required for
> this release. The Android client is unchanged this release (it has no VRX);
> the bundled APK is rebuilt at 2.3.0. Download `ThetisLink-2.3.0.zip` from the
> [Releases page](https://github.com/cjenschede/ThetisLink/releases) — the ZIP
> contains both Windows binaries, the Android APK, the PDF manuals, `LICENSE`
> and `SHA256SUMS.txt`. SBOM and third-party license artefacts are attached to
> the same release as separate assets.

### Added — Synchronous AM with a carrier-tracking PLL (SAM)

The VRX **SAM** mode is now a real synchronous-AM demodulator: a
critically-damped (ζ=1.0) carrier-tracking PLL locks onto the AM carrier and
demodulates against the recovered phase, mirroring Thetis/WDSP `amd.c`. This
removes the beat-note of the previous pseudo-SAM when the tuning is a few Hz
off and stays clean through selective fading. Capture range ±3 kHz.

### Added — AM auto-tune-to-carrier (AFC) + per-VRX audio rate

In SAM with auto-tune enabled, the listen frequency continuously follows the AM
carrier onto exact zero-beat (the client VFO follows). The tracker is a
two-speed, noise-robust AFC (fast pull-in when far out, slow ~2 s tracking near
the carrier, 5 Hz deadband) that holds a strong/wide carrier without hunting and
preserves the lock across an NB↔WB audio-rate rebuild. Each VRX gets its own
audio-rate selector — **NB (8 kHz) / WB (16 kHz) / Auto** — independently per
channel; Auto widens to 16 kHz when the filter is opened past ~4 kHz.

### Added — Settable TX modulation bandwidth (desktop, Thetis tab)

The main-radio TX modulation bandwidth is now adjustable from the desktop
client's Thetis tab: **Follow RX bandwidth** (TX mirrors the RX filter 1:1,
manual fields greyed) or independent **Low/High** edges. Range 0–8 kHz (TX audio
is 16 kS/s, so the audio passband tops out at 8 kHz; a wider RX filter is flagged
and clamped). In symmetric modes (AM/SAM/DSB/FM) the RX spectrum filter edges now
mirror, so dragging one edge moves both sides — matching how Thetis enforces a
symmetric filter.

### Fixed

- During PTT, mode changes are no longer forwarded to Thetis — works around a
  Thetis desync where a mode change mid-transmit updated the indicator but not
  the actual mode.
- **Follow RX bandwidth** is now available immediately on connect: the server
  reads the TX filter band at TCI connect (`tx_filter_band_ex`) instead of only
  learning it when Thetis first changes it, so a server restart is no longer
  needed for the feature to work.
- **Pop-out windows on a disconnected monitor** are recovered automatically: the
  client validates each saved pop-out position against the live monitor layout
  (Windows) and opens off-screen windows on the primary monitor instead. A manual
  **"Recenter windows"** button (Server tab) is also available.
- AFC handoff is clamp-aware at the ±3 kHz capture edge (no offset double-count
  or drift).

## [2.2.0] — 2026-06-18 (Virtual receivers + dual-radio FT-991A/FTX-1)

> **Backwards-compatible with 2.1.x.** Wire-protocol `VERSION` stays 3 — the new
> VRX and second-radio packet types are purely additive (`0x21`–`0x29`) and are
> sent only to clients that explicitly subscribe, so a v2.1.x client keeps working
> and never receives the new types. Pair with **Thetis fork PA3GHM TL2-4** for the
> full feature-set; stock Thetis remains supported. Download
> `ThetisLink-2.2.0.zip` from the
> [Releases page](https://github.com/cjenschede/ThetisLink/releases) — the ZIP
> contains both Windows binaries, the Android APK, all PDF manuals, `LICENSE` and
> `SHA256SUMS.txt`. SBOM and third-party license artefacts are attached to the
> same release as separate download assets.

### Added — Virtual receivers (VRX)

Two independent **virtual receivers** — VRX1 on RX1/VFO-A and VRX2 on RX2/VFO-B —
are carved out of the wideband DDC I/Q stream by an FFT channelizer (new
`vrx-rs` crate). Each VRX has its own listen frequency, mode (USB/LSB/AM/SAM/FM),
filter, high-resolution spectrum + waterfall and S-meter, shown together in a
joint pop-out window and mixed into the main audio alongside RX1/RX2/Yaesu.
Audio is Opus narrowband (8 kHz) or wideband (16 kHz). Per-DDC-bucket frequency
memory and full state persistence (enable/frequency/mode/filter) across
reconnects.

A browser-readable, illustrated explanation of the whole VRX signal chain — from
radio wave to sound — is published on GitHub Pages:
**[How a VRX works](https://cjenschede.github.io/ThetisLink/VRX-explained.html)**
(English) · **[Hoe een VRX werkt](https://cjenschede.github.io/ThetisLink/VRX-uitleg.html)**
(Nederlands), with a companion document on the server → client network path.

### Added — Second radio (FT-991A + FTX-1, dual-radio)

A second Yaesu radio can run alongside the first as an **independent channel**
(slot 1), each with its own CAT serial port, USB audio, frequency, mode, PTT and
memory. The radio model is **auto-detected** from the CAT `ID;` response
(`0670` = FT-991A, `0840` = FTX-1); a bring-up probe logs a warning if the
detected model does not match the configured slot (possible USB-enumeration
swap). New additive packet types carry the slot-1 audio/state/frequency/memory,
plus a `RadioInfo` broadcast so dual-radio-aware clients label the panels
correctly. The Yaesu **FTX-1 WIRES-X** EX-menu fields are added to the EX editor.
Two identically-named `USB Audio CODEC` devices can be disambiguated with a
**`#N` index suffix** in the audio-device selector.

### Added — FTX-1 software squelch

The FTX-1's hardware squelch does not gate its USB audio, so an FM channel
streams noise continuously. A **server-side software squelch** now polls the
radio's busy state (`RI`) and fades the audio to silence when the squelch is
closed — **FM-family modes only**; SSB/CW/AM/data always pass through (where the
busy flag is meaningless).

### Added — Switchable radio RX bandwidth + dynamic recording rate

One client switch now sets the **RX audio bandwidth** (narrow 8 kHz / wide
16 kHz) for the Thetis receiver, the VRX channels and the connected Yaesu radios
together (receive only; transmit stays wideband). WAV recording sample-rate
auto-scales with that setting.

### Fixed — VRX traffic isolation for older clients

VRX audio (`AudioVrx`) and high-resolution VRX spectrum (`SpectrumVrx1/2`) are
now gated by **per-client subscription** (mirroring the second-radio gate), so a
v2.1.x client that never enables VRX receives none of the new packet types — no
parse errors, no log-spam, no wasted bandwidth. The FM demodulator's phase
discriminator was corrected to use a full-quadrant `atan2`.

## [2.1.1] — 2026-06-07 (PstRotator + Log4OM direct rotor control)

> **Backwards-compatible with 2.1.0.** Wire-protocol unchanged. Adds a
> parallel UDP+TCP listener on the server so PstRotator (any mode) or
> Log4OM (via PstRotator-emulation) can command the active rotor backend
> directly. Existing TCI-client rotor control is unchanged.

### Added — PstRotator listener (parallel input source)

The server now opens a combined UDP + TCP listener (default port
12001, configurable via `pstrotator_listen_enabled` / `pstrotator_listen_port`
in `thetislink-server.conf`) that accepts rotor commands from PstRotator
or any PstRotator-compatible application. Commands are routed through
the active rotor backend (EA7HG, PstRotator-outgoing, or Adafruit
MCP2221A) — so PstRotator can drive a G-1000DXC connected via the
Adafruit breakout without any intermediate hardware.

Supported protocol formats (auto-detected per packet):

- **Yaesu GS-232A / GS-232B** (text): `M<nnn>\r` (goto), `S\r` (stop),
  `C\r` (query → `+<nnn>\r`), `C2\r` (query → `+0aaa+0eee\r`)
- **Prosistel binary / EA7HG**: `\x02AG<nnn>\r` or `AAG<nnn>\r` (goto),
  `\x02A?\r` or `AA?\r` (query → `\x02A,?,<nnn>,<R|B>\r`),
  `\x02AG999\r` or `AAR\r` (stop)
- **PstRotator native XML**: `<PST><AZIMUTH>nnn.n</AZIMUTH></PST>`
  (goto), `<PST>AZ?</PST>` (query → `AZ:<nnn.n>\r`),
  `<PST><STOP>1</STOP></PST>` (stop)
- **AZ-text broadcast**: `AZ:nnn.n\r` (PstRotator's simulator output,
  treated as feedback within 30 s of a real goto to avoid override)

The TCP path also pushes TL2-originated targets back to PstRotator
(`M<nnn>\r` or `\x02AG<nnn>\r` depending on detected protocol), so
PstRotator's compass shows the same target indicator regardless of
which side initiated the move. **Note:** PstRotator's client-mode UI
may not visualise externally-pushed targets — this is a protocol-side
limitation of GS-232A / Prosistel, not a TL2 issue.

### Added — Log4OM direct (PstRotator-emulation)

Log4OM does not natively support `rotctld` or other generic rotor
protocols — its only rotor option is PstRotator. To drive the rotor
without a PstRotator instance running, point Log4OM's PstRotator
settings at the TL2 server:

1. In Log4OM: **Settings → External Services → PstRotator** (or
   equivalent rotator-control panel)
2. Set **Host** to the TL2 server's IP (e.g. `192.168.1.97`) — change
   from `localhost` / `127.0.0.1`
3. Set **Port** to TL2's PstRotator listener port (default `12001`)
4. Stop PstRotator on the Win4OM PC if it is running (no longer needed)

Log4OM now sends `<PST><AZIMUTH>nnn</AZIMUTH></PST>` directly to TL2.
TL2 acts as a drop-in PstRotator replacement. Metadata tags Log4OM
also sends (`<CALL>`, `<NAME>`, `<QTH>`, `<FREQUENCY>`, `<MODE>`,
`<GRID>`, `<COMMENT>`, `<COUNTRY>`, `<CONTINENT>`) are silently
ignored — no parse-fail warnings in the server log.

### Fixed — Rotor target oscillation when PstRotator simulator broadcasts AZ:nn

When PstRotator's "UDP output" was enabled in parallel with the
EA7HG-UDP controller, PstRotator's internal rotor simulator
broadcast `AZ:nn\r` packets ~1 Hz that the listener interpreted as
new goto commands. Each simulator step pulled the rotor to that
position, causing visible stepwise oscillation. The listener now
classifies AZ-broadcasts that arrive within 30 s of a real
`AAG`/`M` goto as simulator-feedback and silently drops them.
AZ-broadcasts outside that window continue to work as goto commands
for AZ-only PstRotator output configurations.

### Changed — Server-log volume

The high-frequency raw-packet log lines (`PstRotator listen RX from
...`) are now emitted at `debug!` level instead of `info!`. The
default-level log shows only actionable rotor events
(`compass X° → mech Y°` on a real goto, connect/disconnect, parse
warnings on truly unknown packets). Use `RUST_LOG=debug` to restore
the full RX visibility for diagnostics.

### Fixed — Rotor direction-reversal ramp protection

When a running GoTo received a new target on the opposite side of the
compass (delta sign flip), the Adafruit MCP2221A backend previously
flipped the CW/CCW gates while the DAC was still at full speed,
causing the motor to slam from full-power one direction to full-power
the other. The poll-tick now detects a direction mismatch between the
desired rotation and the active gate; while `current_dac` is above
the dead-band it leaves the gates alone and ramps the DAC down to
zero first. Once stopped, the gates switch to the new direction and
the normal soft-start ramps back up. Existing `ramp_pct_per_sec`
controls both phases — no separate reversal rate.

### Fixed — Yaesu FT-991A memory write

"Write radio" from the FT-991A memory-edit window previously reported
success in the server log but the radio silently rejected the writes,
and a follow-up "Read radio" surfaced the unchanged state. Three
underlying issues, all addressed:

- **UDP packet-reorder race.** The client sends the tab-text data
  (~2.7 kB, IP-fragmented) and the write-trigger control (8 B) in
  quick succession. The control routinely overtook the data on the
  wire, so the server saw a trigger without data and dropped it. A
  latch on the trigger now fires the write when the data arrives,
  regardless of order.
- **MT frame P9 violation.** The CTCSS-tone index was emitted in the
  P9 field where the FT-991A spec requires literal `"00"`. Any
  channel with `Tone ENC` (or any non-default tone-mode) was silently
  rejected. P9 is now hard-coded to `"00"`; the per-channel
  CTCSS-tone *frequency* is no longer transmitted via MT.
- **FM force-mapped to DATA-FM on storage.** All `FM` / `FM-N` /
  `DATA-FM` / `C4FM` channels were stored as DATA-FM, leaving the
  radio in DATA-FM after every Write-radio cycle (USB-mic only,
  no local mic). The mode-mapping now round-trips correctly: `FM`
  stays `FM`, `FM-N` stays `FM-N`, `AM-N` stays `AM-N`, `C4FM` stays
  `C4FM`. The runtime FM ↔ DATA-FM swap during remote PTT
  (`set_ptt()`) is unchanged.

**Note:** the per-channel CTCSS *frequency* is no longer written via
MT — only the tone-mode (on/off, ENC/DCS) propagates. Set the CTCSS
frequency from the radio's front-panel menu (or wait for a follow-up
patch that drives the dedicated `CN` command). Tone-mode aan/uit
works as expected.

---

## [2.1.0] — 2026-06 (Yaesu rotor MCP2221A backend, wideband Thetis RX, Amplitec reliability)

> **Backwards-compatible with 2.0.4.** Wire-protocol unchanged — a
> 2.0.4 client talks to a 2.1.0 server (and vice versa) without
> issues. The new rotor backend, wideband RX opt-in and Amplitec
> reconnect logic are all server-side; clients see them through the
> existing TCI/Rotor/Amplitec channels. Pair 2.1.0 with the matching
> Thetis-fork build **PA3GHM TL2-4** to unlock the full feature-set;
> stock Thetis remains supported via the standard fallback paths.

### Added — Yaesu G-1000DXC rotor via Adafruit MCP2221A

A third rotor backend joins the existing **EA7HG** and **PstRotator**
options, driving a Yaesu G-1000DXC's EXT CONTROL port directly from
a Adafruit MCP2221A breakout (5 V mod) without any intermediate
controller PCB or third-party software. The on-board MCP2221A speaks
GPIO (CW/CCW gates via BST82 low-side switches), DAC (speed) and ADC
(position feedback) over USB-HID; ThetisLink does all the control
logic in-process.

- **Soft-start / soft-stop ramp** — configurable acceleration
  (`ramp_pct_per_sec`, 1–200 %/s, default 50 %/s) protects heavy mast
  hardware. The GoTo soft-stop landing computes a deceleration
  distance from the current speed + ramp-rate and reaches the target
  within ±1° without overshoot.
- **Adaptive sample rate** — ADC polled at 30 Hz during motion
  (33 ms tick, intentionally off the 50/60 Hz mains-ripple multiples)
  with a 10-sample median filter for control loop responsiveness, and
  at 1 Hz when idle with a 60-sample median for a calm position
  display.
- **Shortest-route option** for rotors with overlap range
  (`max_deg > 360`): with the checkbox enabled, a GoTo from e.g. 350°
  to 30° picks the 40° CW path through the overlap zone instead of
  the 320° CCW path through the dead band.
- **Manual override** — the server-UI's CW/CCW test buttons and
  speed-slider take precedence over the ramp loop while you debug
  hardware; the ramp resumes control when the next client GoTo
  arrives.
- **Calibration wizard** — "Park CCW" / "Park CW" buttons capture the
  Yaesu position-pin voltage at the mechanical endpoints; the linear
  mapping survives the slightly above-spec voltage range some
  G-1000DXC units exhibit (up to ~7.5 V on pin 4, well above the
  schema-documented 4.5 V).

The client side stays unchanged: the existing Rotor window (compass
circle, GoTo input, Stop button) drives the new backend through the
same `Rotor` facade as EA7HG and PstRotator.

### Added — Optional wideband Thetis RX audio

A new server checkbox **"ThetisLink extensions WB RX"** lifts the
fixed 48 kHz RX-audio rate when paired with a Thetis-fork that
supports the wideband-IQ extension. Owners with capable network and
desktop hardware can now stream RX at the wider rate without giving
up the standard fallback for stock Thetis. Default off — the existing
narrow-band path is unchanged.

### Added — Modular multi-tuner wizard

The server's MCP2221A tuner-bridge section now supports multiple tuner
slots driven from a `Vec<TunerConfig>` schema. Each slot is added via
a board-scan wizard that classifies detected USB devices (Tuner vs
Rotor vs Unprogrammed) and writes the chosen function to the board's
EEPROM. Per-slot rename, delete and threshold-slider; the surrounding
**MCP2221A** section is now collapsible and its expanded/collapsed
state persists across restarts.

### Fixed — Amplitec reconnect after power cycle

The Amplitec 6/2 serial worker thread no longer dies on the first
USB-error. It loops with a 5-second retry, marks the device as
disconnected during the outage and reconnects automatically when the
controller comes back online. The Amplitec window now also appears
even when the device is offline at server start — previously a missed
COM-port at boot made the whole UI section invisible until a server
restart.

### Fixed — RX2 mode-switch filter restore

Switching RX2 to a mode the client had never seen (USB → CW for
example) restored the filter edges from the new mode's defaults
instead of carrying over the obsolete previous mode's filter. A
one-line guard in the client's modulation handler honours the
server's filter-band update during the switch instead of overwriting
it with stale state.

### Fixed — RX2 spectrum filter-drag isolation

Per-channel filter-edge drag keys decoupled the RX1 and RX2 drag
state, so a filter-edge drag on RX2 no longer pulls RX1's filter
along by accident.

### Fixed — Yaesu EQ profile mic-gain persistence

The Yaesu FT-991A equalizer profile now saves the mic-gain slider
together with the band/treble levels; switching profiles or
restarting the client preserves the slider value.

### Fixed — Yaesu TX resampler aliasing

Sharper anti-alias filter on the client's Yaesu TX audio resampler;
high-frequency artefacts in the transmitted audio are reduced.

### Fixed — Server status panel scroll-jump

The Status panel's "Active clients" and "Recent connect attempts"
sections briefly shrank to a 1-line "snapshot busy…" placeholder
whenever the SessionManager lock was contended. The lost rows pulled
the scrollable content above any expanded section underneath, so
scrolling down to inspect the MCP2221A panel kept jumping back up.
Snapshots are now cached and reused on contention; the layout stays
stable across renders.

### Fixed — Graceful server auto-restart

Auto-restart now runs the hardware-Arc Drop handlers before the new
process spawns, releasing cpal audio streams and the TCI WebSocket
cleanly. Audio on the new instance works on the first try instead of
requiring a manual stop+start cycle.

### Fixed — UltraBeam element-lengths at connect

Initial element-length read on connect; the UI no longer briefly
shows zeros for the first ~300 ms after the UltraBeam controller
appears on the network.

### Fixed — Yaesu audio cold-start fail-soft

Yaesu output stream retry-loop + de-duplicated retry logs prevent the
"audio device disappeared at boot" failure mode where the server
gave up after one attempt; the first poll after the device enumerates
correctly now succeeds quietly.

### Changed — UI polish across server and client

- All `CollapsingHeader` widgets replaced by a custom `chevron_label`
  with a geometric triangle marker, ASCII-only to avoid the egui
  default font's missing-glyph tofu on some Windows setups
- Server Settings tab now wrapped in a `ScrollArea` so the panel
  stays usable on smaller displays
- Amplitec antenna-button two-line layout with prominent alias label;
  rename via right-click context menu; auto-scale buttons on long
  names
- Client frequency-digit hover blocks the parent `ScrollArea` so
  mouse-wheel digit edits no longer scroll the surrounding panel
- Rotor poll-thread log noise demoted to `debug!` (per-tick
  `set_direction` and 5-second ADC stats) — no longer floods the
  default server log

### Compliance

- New driver module `mcp2221_yaesu_rotor.rs` carries an
  `SPDX-License-Identifier: GPL-2.0-or-later` header. The vendored
  `mcp2221-hal` crate keeps its original MIT/Apache-2 dual license
  alongside ThetisLink's GPL-2.0-or-later distribution as before.
- SBOM (`compliance/sbom.spdx.json`) and third-party-licenses bundle
  regenerated for v2.1.0.
- No new third-party crate additions on top of v2.0.4 beyond what the
  workspace already depended on.

### Hardware reference — Yaesu G-1000DXC + MCP2221A

For owners building the same setup: the rotor printje uses a
**1.8 kΩ + 2.2 kΩ** divider (ratio 1.818) on the position-feedback
pin, mapping the 0–4.8 V (or higher, depending on unit) Yaesu output
into the MCP2221A's internal 4.096 V ADC reference with a safe
margin. The initial 1.8 kΩ + 10 kΩ design (ratio 1.18) clipped above
~365° on some units; rebuild with 2.2 kΩ if you observe ADC
saturation past the 365° mark. A 10 µF cap parallel to the 2.2 kΩ
suppresses the 100 Hz mains-rectifier ripple visible on the position
signal. Recalibrate with **Park CCW** + **Park CW** after any divider
change.

---

## [2.0.4] — 2026-05 (bandwidth toolkit, preventive TX-inhibit, power-cap, PstRotator)

> **Backwards-compatible with 2.0.3.** Wire-protocol additive only —
> one new control ID (`DxSpotsEnabled`); older clients ignore it,
> older servers default the new behaviour to ON. Mix v2.0.3 and
> v2.0.4 freely while you roll out, but pair v2.0.4 with the
> matching Thetis-fork build to unlock the full feature-set.

### Added — Preventive RX-only TX-inhibit (Thetis-fork TL2-3 required)

A new chokepoint between ThetisLink and Thetis stops the radio from
transmitting on an antenna position marked as **RX-only**, before TX
can briefly come up. When the fork-side ThetisLink extensions are
enabled, ThetisLink drives Thetis' "Receive only" flag directly via
the new `rx_only_ex` TCI command — MOX, spacebar, hardware-PTT and
VOX are all refused at the source instead of being flipped back
reactively. The reactive ZZTX0 catch-all remains the safety floor
for stock Thetis (no fork extensions) and for any path the
preventive gate cannot reach.

- Server-side state machine handles takeover, level-maintain and
  release, including a bootstrap-stale clear so a leftover
  `RXOnly=true` from a previous session is wiped within ~1 ms after
  the cap is detected.
- The Thetis-fork `RXOnly` setter now broadcasts a TCI push-notify on
  every real transition, so external Setup → "Receive only" toggles
  are visible to ThetisLink in real time (was: only on TCI SET/GET
  echoes, which left ThetisLink with a stale cache).
- Server-side dedup on the `rx_only_ex` notification keeps the log
  clean when fork-broadcast and handler-echo arrive together.

Requires Thetis fork **PA3GHM TL2-3** or newer for the full
preventive path. Stock Thetis falls back to the reactive ZZTX0
catch-all without any user action.

### Added — Reactive RF power-cap per antenna position

Per-Amplitec-A position the server enforces a maximum forward-power
(`amplitec_max_w`) by sending the PA's own `DriveDown` button (SPE
Expert or RF2K-S) — not ZZPC, which the PA pushes back through the
TCI loop. Mode-multipliers are applied universally: SSB/CW × 1.0,
AM × 0.5, FM/DIG × 0.4. A counter remembers how many DriveDowns
were sent on the active position and restores them as DriveUps when
the user switches to a different position.

- New first-class GUI editor in the Amplitec tab (6 rows × max W
  + TX-blocked checkbox) replaces the previous file-edit workflow.
- Rate-limited to one DriveDown per second to let the PA-meter
  settle; brief CW-bursts under that interval may pass the cap
  (reactive only — preventive coverage exists on RX-only positions).
- Tuner first-config: the server UI now shows tuner slots without
  requiring an existing instance, breaking the catch-22 for new
  installs.

### Added — PstRotator UDP/XML rotor backend

Native PstRotator support alongside the existing rotctl-TCP backend.
Per-installation choice via `rotor_backend = pstrotator` in the
server config. Integer-degree AZIMUTH commands; AZ/EL replies parsed
fallible; offline-timeout marks status `false` cleanly. Host field
is a **numeric IP address** — no DNS resolution. mDNS troubleshooting
notes added to the manuals.

### Added — Editable WebSDR favorite names

Favorites in the WebSDR list now have an explicit Edit-toggle so a
rename commits on Done / loss of focus and survives reconnect.

### Added — Server-tab bandwidth monitor + DX-spots opt-out

The desktop Server tab now shows the live UDP bandwidth in both
directions:

- **Down (RX)** and **Up (TX)** in Kbit/s, updated every 500 ms.
- Click on Down to expand a per-stream breakdown (audio, spectrum,
  S-meter, DX-spots, …) refreshed every 5 s.
- A **DX spots ontvangen** checkbox lets you opt out of the DX-cluster
  spot stream on metered links. The Android client has the same
  switch in Settings.

The monitor counts UDP application-payload bytes; the operating-
system network meter typically reads 1.5–2× higher because it
includes IP/UDP/Ethernet headers. The Android DX-spots toggle
resets to ON when the app restarts (no preference persistence).

### Fixed — DX-cluster spot broadcast storm (~90 Kbit/s → ~6 Kbit/s)

The server used to re-send all cached DX-cluster spots to every
client on every equipment-tick (5 Hz). With ~100 spots in cache this
consumed ~90 Kbit/s steady-state on each client. The broadcast now
sends only new spots per tick and triggers a full age-refresh every
10 s — about 15× less data without a user-visible change.

### Fixed — Server log spam

Two periodic log sources became state-change-driven:

- `PowerCap state` only logs when `(pos, mode, pa_in_operate, cap)`
  transitions — used to fire every 2 s regardless. PA-meter fluctuations
  are intentionally excluded from the snapshot so they don't reintroduce
  the spam.
- `DX Cluster` reconnect now emits one line per failure plus one
  line on recovery (`reconnected after N failed attempts`) instead of
  the previous three lines per backoff cycle.

### Notes for upgraders

- A v2.0.3 client connects to a v2.0.4 server without problems; the
  new `DxSpotsEnabled` control is simply unused, default = ON.
- A v2.0.4 client connects to a v2.0.3 server without problems; the
  opt-out toggle is harmless (server ignores the unknown control)
  but you cannot turn off the spot stream on the older server.
- The new preventive TX-inhibit only activates when paired with a
  **Thetis fork build PA3GHM TL2-3 or newer**. Stock Thetis remains
  fully supported via the reactive ZZTX0 fallback that already
  existed in v2.0.3.

---

## [2.0.3] — 2026-05 (multi-tuner + wire-protocol breaking change)

> **Breaking change — wire-protocol version bumped from 2 → 3.**
> A v2.0.3 server and a v2.0.2 client (or vice versa) are not compatible:
> the S-meter payload layout was rearranged to support multi-source
> subscriptions (Sig peak-hold, Avg true-mean, MaxBin) and an S9-frequency
> band-shift. Mismatched pairs are detected in the handshake and the user
> sees a localised `ProtocolVersionMismatch` modal ("Server is too old" /
> "Client is too old") instead of garbled audio or a silent connect failure.
> Upgrade server, desktop client and Android client together.

### Added — multi-tuner runtime via Adafruit MCP2221A USB-HID

- Up to **two physical StockCorner JC-4s / JC-3s tuners in parallel**, each
  driven through its own MCP2221A breakout (replaces the v2.0.2 serial-port
  RTS/CTS flow). JC-4s and JC-3s share the same control protocol; the model
  alias is cosmetic.
- **Per-tuner status panel rows** with: connection state, MCP serial
  dropdown, Amplitec-A position binding, live yellow-wire voltage,
  threshold slider (0.5 V – 4.5 V, default 2.25 V), hysteresis slider
  (0.1 V – 2.0 V, default 0.50 V), and the derived `active < … V` /
  `idle > … V` edge display. An amber **⚠ clamped** warning appears when
  the slider combination falls outside the physically reachable yellow
  range so the user sees that the configuration would never trigger.
- **USB board scan + "Program serial"** UI: identify anonymous boards by
  HID path, give each one a unique serial that survives a USB-replug.
- **USB auto-reconnect** (5 s retry interval) for tuner bridges that drop
  the link after first connecting — no server restart needed to recover
  from a cable replug or hub reset.
- **Collapsible MCP2221A section** at the bottom of the status panel with
  its open/closed state persisted across server restarts
  (`mcp2221_section_expanded` config key).

### Added — S-meter and TCI sensor layer

- Multi-source S-meter subscription via the `rx_channel_sensors_ex` TCI
  payload: peak-hold ("Sig"), true-mean ("Avg"), and the single highest
  FFT-bin in the passband ("MaxBin") are all cached server-side; clients
  pick the source they want via `SmeterSource`.
- **S9-frequency band shift** (HF vs VHF/UHF S-meter scale) honours the
  Thetis-fork-provided crossover frequency (`s9_frequency_ex`); falls
  back to 50 MHz against stock Thetis.
- FWD-power continues to update during TX with Sig/MaxBin subscription
  active (previous bundle showed zero forward power when the client
  switched away from the Avg source).

### Added — CTUN, MIDI, Spectrum

- **CTUN coupled-recenter mirror**: server now syncs the second RX
  spectrum so rapid VFO-A tuning does not leave RX2 lagging.
- **MIDI client-side VFO coalesce** + auto-recenter ownership handshake
  with the Thetis fork: extreme MIDI wheel input no longer fills the
  VFO queue, and the fork-side smooth-scroll guard now also works when
  no ThetisLink server is connected.
- Connect-time RX1/RX2 spectrum **balancing** so both RX paths come up
  at roughly the same moment; Auto-FFT retuned to ~25 FPS.

### Added — Persistence and UI polish

- PA `active_pa` choice and per-PA pre-Operate drive snapshots survive
  a non-graceful shutdown (process kill / power loss) without waiting
  for the next `start_server()` write.
- RF2K-S drive restore guard: no `ZZPC000;` is sent if the snapshot
  is missing.
- Status-panel **protocol version-mismatch** banner: previously the
  mismatch was silent; now there is a visible row when a v2.0.2 client
  contacts the v2.0.3 server (or vice versa).
- Scrollable Radio tab, resizable spectrum split, persisted pop-out
  window geometry.

### Changed

- Tuner removal: the v2.0.2 `assume_tuned` checkbox, `TUNER_DONE_ASSUMED`
  state (5) and the 500 ms assume-deadline pad were retired now that
  feedback-driven tune-detection works reliably in production. The
  client/Android paths that still recognise state 5 are harmless dead
  paths and will be removed in v2.0.4.
- Voltage divider on the yellow tune-status wire: both R1 and R2
  moved from 10 kΩ to **1 MΩ** to reduce loading on the JC-Control
  LED circuit. Ratio (1:1, ×2 in voltage) and the threshold defaults are
  unchanged. The full wiring schema is documented separately and
  available on request.

### Fixed

- **Config RMW race** in the status-panel write paths: per-tuner MCP
  serial, Amplitec-pos, threshold and hysteresis edits now go through
  `config::modify_config(|c| …)` so the load/mutate/save sequence is
  atomic under `CONFIG_LOCK` — closing the same race that was fixed for
  the RF2K drive snapshot earlier in the v2.0.3 cycle but had been
  reintroduced by the new tuner UI.
- **ADC dedup** in the tuner thread: the bridge `snapshot()` rate-limits
  USB ADC polls to 100 ms while the tuner thread runs the active/idle
  edge loops at 25 ms. The thread now checks the
  `DebugSnapshot.last_adc_at` timestamp and only counts a consecutive
  edge when the timestamp has advanced — defeating the "count the same
  cached sample twice" failure mode that defeated noise rejection.
- **Threshold/hysteresis edge-clamp**: slider combinations like
  threshold 0.5 V + hysteresis 2.0 V used to produce an unreachable
  active edge at −0.5 V. The bridge now clamps the computed edges to
  the physically reachable range `[0, ADC_VREF × divider]` and exposes
  `edges_clamped: bool` so the UI can show an amber warning instead of
  silently locking the tuner.

### Compliance

- `vendor/mcp2221-hal/` v0.1.0 (Copyright © 2025 Rob Wells, MIT branch
  of the dual-licensed `MIT OR Apache-2.0` source) added as a vendored
  Rust crate; attribution added to `NOTICE.md` and the full MIT license
  text is reproduced in `compliance/THIRD-PARTY-LICENSES.html` and the
  SPDX SBOM at `compliance/sbom.spdx.json`. The Apache-2.0 alternative
  is deliberately not used because Apache-2.0's patent-grant clauses
  are not compatible with the `GPL-2.0-or-later` licence ThetisLink
  itself is distributed under.
- All `compliance/*` artefacts regenerated for the v2.0.3 binary set.

---

## [2.0.2] — 2026-05 (log-spam hotfix)

- Server-side `DiversityPhaseEx`, `DiversityGainEx` and
  `DiversityGainMultiEx` TCI notifications now log INFO only on a real
  value change. Thetis pushes these at every diversity tick (~10–20 Hz)
  which previously filled the server log at hundreds of thousands of
  lines per session.
- Functional behaviour and wire protocol unchanged (`VERSION = 2`) —
  fully interoperable with v2.0.0 and v2.0.1.

## [2.0.1] — 2026-05 (connect-experience release)

- First-run 4-step setup wizard (Find server → Password → 2FA →
  Connected).
- mDNS local-network discovery — clients auto-find servers on the same
  WiFi/LAN.
- Nine differentiated connect states with platform-aware NL/EN hints
  including a smart `TciUnreachable` hint that knows whether Thetis is
  running, starting up, or fully stopped.
- Server status panel: bind address, TCI status, active clients with
  RTT / loss / jitter, audio routing chips, recent connect attempts.
- Server-side RX2 audio-filter fix (no more phantom CH2 stream when RX2
  is off).
- "Restart setup wizard" button.
- Wire protocol unchanged (`VERSION = 2`) — fully interoperable with
  v2.0.0.

## [2.0.0] — 2026-04 (TL2 release)

- Yaesu FT-991A auto-DFM PTT toggle (FM ↔ DATA-FM with memory restore).
- Server-side CTUN auto-recenter (Thetis-fork `auto_recenter_ex`).
- Live diversity null-circle broadcast (Smart / Ultra).
- Filter-preset push (F1..VAR2/NONE), per-RX DDC sample rate
  (48..1536 kHz), `tci_caps_ex` capability broadcast.
- DX cluster click-to-tune, SWR display in TX meter.
- CW keyer + macros over TCI.
- Single-TCI-only architecture — separate CAT connection retired.
- **Wire-protocol VERSION bumped from 1 → 2.**

## [1.0.0] — 2026-03

- First public release on [`cjenschede/ThetisLink`](https://github.com/cjenschede/ThetisLink).

## [0.5.0]

- Yaesu FT-991A support, Bluetooth headset (Android), diversity-receive
  fix, TCI control elements, RF2K-S reset, PTT modes, DX cluster.

## [0.4.9]

- Wideband Opus TX, device-switch fix.

## [0.4.2]

- Configurable FFT size, dynamic spectrum bins, Android power-button fix.

## [0.4.1]

- WebSDR / KiwiSDR integration, frequency sync, TX-spectrum auto-override.

## [0.4.0]

- TCI WebSocket, waterfall click-to-tune (Android).

## [0.3.2]

- MIDI controller support, PTT toggle with LED, mic AGC.

## [0.3.1]

- Band memory, FM filter fix, macOS client.

## [0.3.0]

- Full RX2 / VFO-B support, DDC spectrum + waterfall.
