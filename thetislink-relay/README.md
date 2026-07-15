# ThetisLink Relay Prototype

ThetisLink Relay is a small self-hosted WebSocket relay for the outbound-only connection model.
Both sides connect out to the VPS relay, so the radio/server site does not need router port-forwarding.

Current status: working. It forwards WebSocket frames between one `station` peer (the radio site) and up to several `client` peers that share the same `station` and `token`. Each client is assigned a small numeric id so the station can serve them independently at the same time; audio, spectrum, and control frames are tunnelled through the relay so a client that cannot port-forward still reaches the station.

## What Users Need

- A VPS or server reachable from the internet.
- Docker + Docker Compose.
- One public TCP port, for example `18080` for testing or `443` later behind TLS.
- One shared secret token.

## Quick Start From Source

Upload or clone only this `thetislink-relay` directory on the VPS.

```bash
cd thetislink-relay
cp .env.example .env
nano .env

docker compose up -d --build
docker compose logs -f
```

Stop:

```bash
docker compose down
```

## .env Settings

```bash
THETISLINK_RELAY_TOKEN=change-this-long-random-token
THETISLINK_RELAY_PORT=18080
RUST_LOG=info
```

Generate a stronger token on Linux:

```bash
openssl rand -hex 32
```

Use the same token in the TL server and TL client once relay transport is integrated.

## Test From Two Terminals

Terminal 1:

```powershell
cargo run -p thetislink-relay -- --connect ws://203.0.113.10:18080 --station test --role station --token test-token
```

Terminal 2:

```powershell
cargo run -p thetislink-relay -- --connect ws://203.0.113.10:18080 --station test --role client --token test-token
```

Type in either terminal. The message should appear in the other terminal. The integrated TL server/client also exchange small `TLR2` heartbeat frames so their UIs can show that a peer is present.

## Manual Docker Run

```bash
docker build -t thetislink-relay:dev .
docker run --rm -d \
  --name thetislink-relay \
  --restart unless-stopped \
  -p 18080:18080 \
  -e THETISLINK_RELAY_TOKEN=change-this-token \
  thetislink-relay:dev
```

## Useful Operations

Show status:

```bash
docker compose ps
docker compose logs --tail=100
```

Restart:

```bash
docker compose restart
```

Update after replacing source files:

```bash
docker compose up -d --build
```

## Security Notes

- This prototype uses a shared token. Keep it private.
- Do not run it publicly without TLS for real use. For port `443`, put it behind a TLS reverse proxy such as Caddy, nginx, or Traefik.
- If you expose plain `ws://` during testing, assume anyone on the network path could observe traffic.
- A reconnecting station replaces the previous station for the same station/token. Multiple clients coexist; when a room already holds the maximum number of clients, further clients are rejected with `ERR relay full`.

## Future Packaging

Once stable, this can be distributed as either:

1. Source folder: user runs `docker compose up -d --build`.
2. Prebuilt image: user runs `docker compose up -d` with `image: ghcr.io/.../thetislink-relay:<version>`.

The prebuilt image route is easier for non-technical users, but the source-folder route is enough for early testers.
