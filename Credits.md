# ThetisLink Credits

## License

ThetisLink is licensed under **GPL-2.0-or-later**. See `LICENSE` for the full text and `NOTICE.md` for the licensing summary.

Copyright © 2025-2026 Chiron van der Burgt — PA3GHM.

Source: https://github.com/cjenschede/ThetisLink

Based on the Thetis SDR lineage (FlexRadio PowerSDR → OpenHPSDR Thetis). Upstream contributors and the full provenance chain are documented in `ATTRIBUTION.md`.

## Author

**Chiron van der Burgt — PA3GHM**

## Special Thanks

- **Richie (ramdor)** — Thetis SDR development, TCI protocol extensions, and ongoing support

## Open Source Libraries

### Rust (Desktop + Server + Android native)

| Library | Purpose | License |
|---------|---------|---------|
| tokio | Async runtime | MIT |
| eframe / egui | Desktop GUI framework | MIT / Apache-2.0 |
| cpal | Cross-platform audio I/O | Apache-2.0 |
| audiopus | Opus audio codec bindings | MIT |
| rubato | Sample rate conversion | MIT |
| rustfft | FFT spectrum processing | MIT / Apache-2.0 |
| ringbuf | Lock-free ring buffers | MIT / Apache-2.0 |
| tokio-tungstenite | TCI WebSocket client | MIT |
| serialport | Serial port (Yaesu CAT) | MIT |
| midir | MIDI controller support | MIT |
| num_enum | Enum ↔ integer derive | MIT / Apache-2.0 |
| anyhow | Error handling | MIT / Apache-2.0 |
| log / env_logger | Logging | MIT / Apache-2.0 |
| bytemuck | Safe transmute for audio buffers | MIT / Apache-2.0 / Zlib |
| hmac / sha2 / rand | Authentication (HMAC-SHA256) | MIT / Apache-2.0 |
| wry | WebView (embedded WebSDR) | MIT / Apache-2.0 |

### Android (Kotlin)

| Library | Purpose |
|---------|---------|
| Jetpack Compose | Android UI framework |
| UniFFI | Rust ↔ Kotlin FFI bridge |
| Oboe (AAudio) | Low-latency Android audio |
| Material 3 | UI components and theming |

## Protocols & External Services

- **TCI** (Transceiver Control Interface) — Expert Electronics / Thetis
- **DX Spider** — DX cluster telnet protocol
- **HPSDR / OpenHPSDR Protocol 2** — SDR hardware communication
- **WebSDR** (PA3FWM) / **KiwiSDR** — CatSync frequency synchronization targets

## Hardware Support

| Device | Interface |
|--------|-----------|
| Apache Labs ANAN 7000DLE | TCI (via Thetis) |
| Yaesu FT-991A | Serial CAT + USB Audio |
| RF2K-S Power Amplifier | HTTP API |
| SPE Expert 1.3K-FA | Serial |
| JC-4s Antenna Tuner | Serial (DTR signaling) |
| UltraBeam RCU-06 | Serial |
| Amplitec 6/2 Antenna Switch | Serial |
| EA7HG Visual Rotor | UDP |
