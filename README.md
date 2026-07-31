# ThetisLink

> **Current release: [v2.6.0](https://github.com/cjenschede/ThetisLink/releases/tag/v2.6.0)** —
> **"Arrange windows" is now a matrix placer (up to 12×12):** pick a grid size, select a
> window and **paint it onto the cells** — **drag a rectangle to place a window in one gesture**
> — and it is now **also built into the server GUI** to arrange the server's own device windows
> (tuner, Amplitec, SPE, RF2K-S, UltraBeam, rotor), per monitor. The **server GUI is now
> multilingual** too — **English, Dutch, German and French** with a live language picker in
> Settings. The **analog S-meter was reworked** for a constant, more readable shape that scales
> uniformly and grows larger when there is room. Plus fixes: **turning the FT-991A on from a
> standby state is no longer delayed ~30 s**, **FT-991A/FTX-1 memory reads reliably**,
> collapsible sections **remember their state**, RX1 **"Auto ref" sticks**, and a **peak-hold
> tick** on the audio-level bars. Built on the per-channel audio/spectrum, VRX windows, Yaesu
> main-screen controls and multilingual clients of v2.5.0.
> Illustrated explainers are online — see **Documentation** below.
> **Backwards-compatible** — TL wire-protocol `VERSION` 3 unchanged; interoperates
> with v2.5.x. **Stock Thetis v2.10.3.15 suffices — no fork change required** for these
> features (the PA3GHM fork adds the extended-IQ feature-set).
> Download `ThetisLink-2.6.0.zip` from the
> [Releases page](https://github.com/cjenschede/ThetisLink/releases) — the ZIP
> contains both Windows binaries, the Android APK, all manuals,
> `LICENSE` and `SHA256SUMS.txt`. SBOM and third-party license artefacts are
> attached to the same release as separate download assets.
>
> **Want to try the relay without hosting your own?** For the first users who would
> like to try it, PA3GHM can — on request and while slots last — temporarily add you
> to a test relay. Note this is a **temporary server with a limited number of slots**,
> so there is no guarantee of availability or continuity. Contact **PA3GHM via
> [QRZ.com](https://www.qrz.com/db/PA3GHM)** (callsign PA3GHM). Otherwise you can
> self-host your own relay — see the manual.

Remote control for ANAN 7000DLE SDR with Thetis. Audio, spectrum, PTT and full
radio control over the network via TCI WebSocket.

## Components

- **ThetisLink Server** — runs on the Thetis PC (Windows), controls the radio via TCI
- **ThetisLink Client** — desktop client (Windows) with spectrum, waterfall and full control
- **ThetisLink Android** — mobile client app

## Features

- Real-time bidirectional audio (Opus codec, minimal latency)
- Two ways to connect — directly (LAN, or over the internet via router port-forward) or through a self-hosted VPS relay (works behind CGNAT, no port-forward). The server supports **both at the same time**: once a relay is configured, each client decides for itself whether to connect directly or via the relay, so one server can serve multiple clients concurrently — some direct, some relayed
- Spectrum and waterfall display (up to 1536 kHz with the PA3GHM Thetis fork)
- Full RX2/VFO-B support with diversity reception
- Virtual receivers (VRX1/VRX2): two independent receivers carved from the wideband DDC stream by an FFT channelizer, each with its own frequency, mode (USB/LSB/AM/SAM/FM), filter, high-resolution spectrum/waterfall and S-meter — including synchronous AM (SAM) with a carrier-tracking PLL, AM auto-tune-to-carrier, and a per-VRX NB/WB/Auto audio rate
- External device control: Amplitec 6/2 (auto-reconnect over USB), two StockCorner JC-4s/JC-3s tuners in parallel (MCP2221A USB-HID), SPE Expert 1.3K-FA, RF2K-S, UltraBeam RCU-06, and three rotor backends — EA7HG Visual Rotor, PstRotator, and direct Yaesu G-1000DXC via MCP2221A (5 V breakout, BST82 gate switches, position-feedback ADC)
- Up to two Yaesu radios (FT-991A and/or FTX-1, any mix) running in parallel as independent channels alongside the Thetis SDR — each with its own CAT COM port, USB audio, frequency, mode, PTT and memory channels (model auto-detected)
- MIDI controller support (desktop + Android)
- Bluetooth remote PTT (e.g. ZL-01)
- Embedded WebSDR / KiwiSDR panel with frequency sync and auto-mute on TX
- DX Cluster with spectrum overlay
- Mandatory password authentication (HMAC-SHA256) with optional TOTP 2FA
- Smart and Ultra diversity auto-null algorithms

## Documentation

**Illustrated explainers (GitHub Pages):** <https://cjenschede.github.io/ThetisLink/>

- [How a VRX works](https://cjenschede.github.io/ThetisLink/VRX-explained.html) — the virtual-receiver signal chain from radio wave to sound (NL: [Hoe een VRX werkt](https://cjenschede.github.io/ThetisLink/VRX-uitleg.html))
- [The network path](https://cjenschede.github.io/ThetisLink/Network-explained.html) — how audio, spectrum and control travel over the network (NL: [Het netwerkpad](https://cjenschede.github.io/ThetisLink/Netwerk-uitleg.html))

Included with each release:

- `Installatie.md` / `Installation.md` — installation guide (Dutch / English)
- `User-Manual.md` / `User-Manual-EN.md` — user manual (Dutch / English)
- `Technische-Referentie.md` / `Technical-Reference.md` — technical reference

## Thetis compatibility

ThetisLink talks to the radio through Thetis. It targets **Thetis v2.10.3.15**
(the latest official release by ramdor) and works with stock Thetis out of the
box. Optionally use the [PA3GHM Thetis fork](https://github.com/cjenschede/Thetis/tree/thetislink-tl2)
(branch `thetislink-tl2`) for the additional `_ex` TCI extensions used by
ThetisLink v2.4.0 (capability broadcast, per-RX filter preset, diversity
control suite, server-side DDC recenter, relaxed IQ-stream rate cap,
wideband RX audio, modulation-change filter fan-out). All
extensions are gated behind the **ThetisLink extensions** checkbox in Setup
> Network > IQ Stream; with the checkbox unchecked the fork behaves like
stock Thetis.

The Thetis fork is maintained separately from this repository. Its per-file
source headers grant the GNU General Public License "version 2 or (at your
option) any later version", corresponding to the SPDX identifier
`GPL-2.0-or-later`. For authoritative details, see that repository's own
`LICENSE`, `LICENSE-DUAL-LICENSING`, and source-file headers.

## License and attribution

ThetisLink is distributed under **GNU General Public License v2.0-or-later**.
See:

- [`LICENSE`](LICENSE) — canonical GPLv2 text
- [`NOTICE.md`](NOTICE.md) — top-level notice
- [`ATTRIBUTION.md`](ATTRIBUTION.md) — Thetis-lineage contributor attribution
  and scope of this project's derivative relationship
- [`LICENSE-COMMERCIAL.md`](LICENSE-COMMERCIAL.md) — commercial licensing
  enquiries (the GPL version is appropriate for amateur radio and personal use)

ThetisLink builds upon the work of the OpenHPSDR Thetis lineage. We acknowledge
all upstream contributors — see `ATTRIBUTION.md` for the full list.

## Support

If you find ThetisLink useful, consider buying me a coffee:

[Donate via PayPal](https://paypal.me/PA3GHM)

73 de PA3GHM
