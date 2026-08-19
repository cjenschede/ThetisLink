# ThetisLink

> **Current release: [v2.9.1](https://github.com/cjenschede/ThetisLink/releases/tag/v2.9.1)** —
> **A dropout sounds like the band again, and switching audio off is silent.** When audio
> hiccups, ThetisLink fills the gap the way it always claimed to — but concealment had been
> running on the wrong decoder, so with wideband audio on it produced silence instead, and only
> the first channel even tried. Every stream now decodes, corrects and conceals in its own
> format; both radios, both VRX and RX2 conceal at all, which they never did. That sound is the
> codec's own concealment and nothing added to it: extrapolated from your signal, which is why
> it passes for your own receiver — though the codec needs to have been decoding for a while
> before it can fill a gap at all, so early in a session a dropout is silence instead. See the
> changelog. And switching a channel off with the audio button is quiet
> at once — it never was, in any earlier version.
> **A phone that changes network gets its audio back by itself**: switching between WiFi and
> mobile data used to leave the controls working and the sound gone until the app was killed.
> **Chat and problem reporting** arrive for stations on a relay — one room shared with the
> other users of it, and a button that sends a report straight to the administrator with
> your log attached, cleaned first and shown to you before it goes. Both are optional and
> neither is needed to operate; without a relay there is nothing new to see and nothing to
> switch off. Also: the server now says **why a radio is missing** when Windows hands its COM
> port to another program, and a recording made during a dropout is no longer shorter than what
> you heard. A station whose **only incoming audio is a radio** also holds its connection
> better — a Yaesu without Thetis, or a Thetis station with its own audio switched off: audio
> from a radio now counts as a sign of life, which it did not before. And **60m** is
> recognised as a band at last — the button stayed grey, and 60m could not hold a band
> memory of its own.
> **Coming from 2.8.x?** Read the 2.9.0 section of the changelog as well — it contains the one
> change in this line that takes something away: the Android app is no longer debuggable, so
> reading its log with `adb run-as` no longer works. The app keeps its own log file instead.
> **Backwards-compatible** — since 2.9.0 the wire protocol gains two packet types for fetching
> a connected server's log (`0x35`, `0x36`); an older peer that knows neither simply never asks
> and never answers. 2.9.1 changes nothing on the wire. **Stock Thetis v2.10.3.x suffices — no
> fork change required.**
> Download `ThetisLink-2.9.1.zip` from the
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
- Chat and problem reporting for stations on the same relay (see below); both optional,
  and neither needed to operate

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

**What comes with it, since 2.9.0.** Stations on this relay also get a chat room shared with
the other users of it, and a **Report a problem** button that goes straight to me with your log
and settings attached - cleaned first, and you see exactly what travels before it is sent.
Reporting works whether or not you join the chat; they are separate choices. Both live on the
relay, so without one there is nothing to see, and everything else in ThetisLink works as it
always did.

The same applies to these as to the relay itself: they run because I enjoy running them. I may
decline a request or stop the service, and a no needs no explanation. What is kept, and for how
long, is on the screen before you agree to anything - and one thing worth knowing in advance: a
callsign appears in a public register with your name and address, so you can pick any other name
to appear under.

Running your own relay? The chat service is a separate container and its source is in this
repository, so you can put it beside your own.

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
(branch `thetislink-tl2`) for the additional `_ex` TCI extensions ThetisLink can
use (capability broadcast, per-RX filter preset, diversity control suite,
server-side DDC recenter, relaxed IQ-stream rate cap, wideband RX audio,
modulation-change filter fan-out). These arrived across several releases, so the
install guide names the fork build that carries all of them. All
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
