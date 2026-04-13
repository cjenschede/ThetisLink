# ThetisLink

Remote control for ANAN 7000DLE SDR with Thetis. Audio, spectrum, PTT and full radio control over the network via TCI WebSocket.

## Components

- **ThetisLink Server** - runs on the Thetis PC (Windows), controls the radio via TCI
- **ThetisLink Client** - desktop client (Windows) with spectrum, waterfall and full control
- **ThetisLink Android** - mobile client app

## Features

- Real-time bidirectional audio (Opus codec, minimal latency)
- Spectrum and waterfall display (up to 1536 kHz with PA3GHM Thetis fork)
- Full RX2/VFO-B support with diversity reception
- External device control: Amplitec 6/2, JC-4s tuner, SPE Expert 1.3K-FA, RF2K-S, UltraBeam RCU-06, EA7HG Visual Rotor
- Yaesu FT-991A as second radio (CAT + USB audio)
- MIDI controller support (desktop + Android)
- DX Cluster with spectrum overlay
- Mandatory password authentication (HMAC-SHA256) with optional TOTP 2FA
- Smart and Ultra diversity auto-null algorithms

## Documentation

See the `docs/` folder and the release zip for:
- `Installatie.md` - Installation guide (Dutch)
- `User-Manual.md` - User manual (Dutch)
- `Technische-Referentie.md` - Technical reference (Dutch)

## Thetis Compatibility

Requires **Thetis v2.10.3.13** (by ramdor). Optionally use the [PA3GHM Thetis fork](https://github.com/cjenschede/Thetis/tree/thetislink-tci-extended) for extended TCI control, eliminating the need for CAT.

## Support

If you find ThetisLink useful, consider buying me a coffee:

[Donate via PayPal](https://paypal.me/PA3GHM)

## License

See [LICENSE](LICENSE). Free for personal and amateur radio use. Commercial use prohibited without permission.

73 de PA3GHM
