# ThetisLink

> **Current release: [v2.11.0](https://github.com/cjenschede/ThetisLink/releases/tag/v2.11.0)** —
> **One microphone can reach several transmitters, and a cable that comes loose no longer
> leaves a carrier behind.** With more than one transmitter — a Thetis and a radio, or two
> radios — only one of them could be on the air. **Multi-TX** sends the same microphone to
> every keyed transmitter at once; it is a checkbox in the Server tab, off by default, and it
> appears once there is more than one transmitter to spread over. Alongside it, the busy sign
> now belongs to each transmitter separately instead of being one answer for the station, and
> a refusal says **which** radio it is about and why — so turning one of them down no longer
> lets go of the other.
> **A radio that loses its USB while transmitting no longer transmits into nothing.** Keying
> over CAT is a latch: the radio holds the last thing it was told, and the audio rides on that
> same cable, so the other station was left hearing a bare carrier with the radio's window
> gone from the screen. The window stays now and its transmit control reads **USB LOST**; the
> transmission is ended as soon as the cable returns, and if it stays away past the radio's own
> time-out the server lets the transmitter go by itself. Keying up again takes a fresh press,
> on purpose.
> **Every PTT control now follows one rule**, on the desktop and on the phone. Mouse, spacebar,
> MIDI, the on-screen button and a Bluetooth button each used to keep their own idea of whether
> you were transmitting, which showed as a button that stayed red after the transmission had
> ended or a press that fired later than you meant it. The phone also gains a **Bluetooth PTT
> button** and a way to **leave that says goodbye**, where swiping the app away used to leave
> the server waiting fifteen seconds for a silence.
> **An answer you put aside now stays aside** — it belonged to the machine you were sitting at,
> so the same answer came back on the phone; it belongs to the station now.
> **Upgrading keeps your configuration**, and **stepping back to 2.10.0 works**: nothing here
> writes a file an older version cannot read.
> **Backwards-compatible** — the refusal packet keeps its four bytes and the protocol version is
> unchanged; two spare flag bits now say which transmitter a refusal is about, and zero means
> "not stated", which is what every earlier server sends. A newer client then falls back to what
> it did before, and both directions are held by tests. **Stock Thetis v2.10.3.15 suffices — no
> fork change required.**
> Download `ThetisLink-2.11.0.zip` from the
> [Releases page](https://github.com/cjenschede/ThetisLink/releases) — the ZIP
> contains both Windows binaries, the Android APK, all manuals,
> `LICENSE` and `SHA256SUMS.txt`. SBOM and third-party license artefacts are
> attached to the same release as separate download assets.

Remote control for Thetis over the network: audio, spectrum, PTT and full radio
control via TCI WebSocket. ThetisLink talks to Thetis and never to the radio behind
it, so the radio Thetis drives is the radio you operate — developed here on an
Apache Labs ANAN, with other stations reporting Hermes-Lite 2 and Red Pitaya. What
a radio offers is what Thetis offers for it: a single-receiver radio has no RX2, and
the spectrum width follows the sample rate that radio can deliver. A Yaesu FT-991A
or FTX-1 is a separate matter — those connect straight to the server over CAT and
USB audio, one model at a time, and are named individually below.

## Components

- **ThetisLink Server** — runs on the Thetis PC (Windows), controls the radio via TCI
- **ThetisLink Client** — desktop client (Windows binary; also builds from source on macOS, experimental) with spectrum, waterfall and full control
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
- Bluetooth PTT on the phone, of two kinds. A **BLE transmit button** of the YPC21 / PTT-Z01 class is found, connected and reconnected by the app itself under **Settings > BT PTT button** — no pairing in the Android settings, it keeps working behind a locked screen because the button is in your hand, and a button that goes out of range releases the transmitter instead of leaving it keyed (Android 12 or newer). Shutter-style buttons (e.g. ZL-01), which present themselves as an external touch device, still work as they did — those need the screen awake
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
