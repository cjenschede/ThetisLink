# ThetisLink

> **Current release: [v2.7.0](https://github.com/cjenschede/ThetisLink/releases/tag/v2.7.0)** —
> **Virtual receivers now start reliably and stay clean after retuning**: three separate faults
> sat under one symptom, and a VRX also stays inside the DDC band with the spectrum drawing the
> band edge honestly instead of silently rescaling. **CTCSS and DCS can be set from the client**
> on the FT-991A — per memory channel, all 104 DCS codes, with a *Read tones* action that walks
> the channels and puts the radio back where it was. The **audio-level bars were rebuilt**: they
> measure the link rather than your volume slider, fall back to zero when a stream stops, and the
> **Yaesu receive path is now calibrated per radio model** — the FT-991A plays about 16 dB
> quieter than before, so set its volume higher than you are used to. The **full-band spectrum
> row is shared** between an RX and the VRX on the same receiver, which fills the waterfall
> history after tuning; a checkbox turns it off and roughly halves the spectrum data. The
> **channel buttons say what they do** now (channel name as a heading, `audio` and `window`),
> the audio switch also sits inside every channel window, and the **master volume really is a
> master** — it applies to all six channels. Plus **Thetis autostart** from client and Android,
> **several clients on one PC** via named profiles, and fixes for a MIDI wheel running a Yaesu
> away and a relay that mistook a dead UDP path for a recovered one.
> Illustrated explainers are online — see **Documentation** below.
> **Backwards-compatible** — TL wire-protocol `VERSION` 3 unchanged; interoperates
> with v2.6.x. **Stock Thetis v2.10.3.15 suffices — no fork change required** for these
> features (the PA3GHM fork adds the extended-IQ feature-set).
> Download `ThetisLink-2.7.0.zip` from the
> [Releases page](https://github.com/cjenschede/ThetisLink/releases) — the ZIP
> contains both Windows binaries, the Android APK, all manuals,
> `LICENSE` and `SHA256SUMS.txt`. SBOM and third-party license artefacts are
> attached to the same release as separate download assets.

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

## Try it over the internet — the PA3GHM test relay

ThetisLink can be used over the internet in two ways: **directly**, if you can forward a port
on your router, or **through a relay**, where the station and the client each make an outgoing
connection to a rendezvous server that pairs them. The relay path needs no port-forward and no
fixed IP address.

You can host a relay yourself — the source is attached to every release and the manual walks
through it. But to lower the threshold, **PA3GHM runs a relay you can be added to on request.**

**This is likely for you if:**

- your internet connection sits behind CGNAT, or you use mobile data, and an incoming
  connection simply is not possible;
- you cannot or would rather not open a port on your router;
- you would like to see whether remote operation suits you before setting up a VPS of your own.

**Please read this first.** It is an **experimental setup with a limited number of places**,
run as a hobby alongside the project. There is no guarantee of availability, capacity or
continuity, and a place may be reclaimed if the load calls for it. It is meant for trying
things out and for operators who have no alternative — not as permanent infrastructure for a
station you depend on. If ThetisLink becomes part of your regular operating, hosting your own
relay is the better answer, and the project supports that fully.

**Requesting access:** send an email to **PA3GHM@gmail.com** with your callsign and a short
note about your setup (which radio, and why a direct connection is not an option for you).
You will receive the connection details and a station key.

73, PA3GHM

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
