# ThetisLink

> **Current release: [v2.10.0](https://github.com/cjenschede/ThetisLink/releases/tag/v2.10.0)** —
> **A first start on the server claims nothing, and two radios of the same type stay apart.** With no
> configuration file yet, the server opens bare: every optional device off, the second
> receiver off, no windows opening by themselves. You switch on what you have connected
> instead of switching off what you do not. It opens in the display language of the machine
> where ThetisLink has that translation — Dutch, German or French — and in English otherwise,
> and that applies to a first start only: once there is a configuration file, the language in
> it is yours and stays. German and French also reach the connect screen, the wizard and the
> status line under them, which were English whatever you had chosen.
> **Two Yaesu radios can now hold different settings.** Three of them — switch to SSB on PTT,
> permission to write memory channels, which side of the USB audio to take — were stored once
> and applied to both, so granting a permission on one radio granted it on the other for good.
> They are per radio now, and ThetisLink works out *what* a radio is by asking the port rather
> than by which slot it sits in: an FTX-1 in the first slot used to get an FT-991A's menu.
> Also: **rows that do not apply are no longer shown** — the roger beep lists only the channels
> this station has, the DX spots switch is gone where the server has no cluster, and a radio
> slot is called "Yaesu 1" until the server says what it is instead of showing a model name
> that was a startup guess. The **chat** now explains what a relay does and is reachable
> whether or not you have one; it says plainly that whoever runs a relay may refuse and may
> stop. **An answer from the administrator that you click away stays away** — remembered per
> machine, so putting one aside on the phone leaves it standing in the server window. And the
> strip that shows those answers is bounded and scrolls, so it no longer takes the whole
> window: it had no limit and no scrollbar, and a few unread answers hid the chat and the
> report button behind them.
> And **a settings file that cannot be read is no longer overwritten** — locked by a
> backup or a virus scanner, it used to read as "nothing has ever been configured here".
> **Upgrading keeps your configuration**: the three settings that were shared hand their answer
> to both radios. **Stepping back to 2.9.1 does not** — it does not know the per-radio keys, so
> keep a copy of your `.conf` if you want that option.
> **Backwards-compatible** — since 2.9.0 the wire protocol gains two packet types for fetching
> a connected server's log (`0x35`, `0x36`); an older peer that knows neither simply never asks
> and never answers. 2.9.1 and 2.10.0 change nothing on the wire. **Stock Thetis v2.10.3.x
> suffices — no fork change required.**
> Download `ThetisLink-2.10.0.zip` from the
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
