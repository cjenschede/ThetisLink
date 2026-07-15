# ThetisLink relay — wss/TLS deploy (DuckDNS + Caddy, port 443 only)

Secures the relay with `wss://` (TLS) using a free DuckDNS subdomain and a free
Let's Encrypt certificate via Caddy. Works with **only port 443** open — port 80 is
not required (Caddy uses the TLS-ALPN-01 challenge on 443).

The relay itself keeps running plain `ws` internally; Caddy terminates TLS in front.

## 1. DuckDNS subdomain (one-time, free)

1. Go to https://www.duckdns.org, log in, create a subdomain, e.g. `your-relay`.
2. Set its IP to the VPS (example address shown): `203.0.113.10`.
3. Your relay URL becomes: `wss://your-relay.duckdns.org`

## 2. VPS prerequisites

- Port **443** reachable from the internet (firewall open). Port 80 is not needed.
- Docker + Docker Compose.
- The `thetislink-relay/` directory on the VPS with the current files:
  the WHOLE `src/` (`main.rs`, `store.rs`, `admin_api.rs` — upload the directory, not
  hand-picked files, or a rebuild silently uses stale code), `Cargo.toml`, `Dockerfile`,
  `Dockerfile.caddy`, `docker-compose.yml`, `Caddyfile`, `.env`. (The Caddyfile is baked
  into a small Caddy image via `Dockerfile.caddy` — not bind-mounted — so single-file
  mount quirks can't bite.) Easiest: upload `release/thetislink-relay-source.tar.gz` and
  `tar xzf` it over the directory so every source file is guaranteed current.

## 3. Configure

```bash
cd thetislink-relay
cp .env.example .env
nano .env          # set THETISLINK_RELAY_TOKEN and RELAY_DOMAIN (your DuckDNS name)
```

## 4. Deploy

```bash
docker compose up -d --build
docker compose logs -f
```
Watch the Caddy log: it should obtain a certificate for your domain within ~30s
("certificate obtained successfully"). The relay logs "listening on 0.0.0.0:18080".

## 5. Point the clients at wss://

In each ThetisLink client (server/station, desktop, Android) relay settings:
- **Relay URL:** `wss://your-relay.duckdns.org`  (no port — wss uses 443)
- **Token / Station / Device name:** unchanged.
- Restart the app/server so it reconnects over wss.

At this stage the transport is encrypted; auth is still the shared global token.

## 6. (Optional, later) Activate per-station auth (Fase 1)

Give each station its own secret instead of the shared token:

```bash
# Create a station (prints the secret ONCE — copy it now):
docker compose exec thetislink-relay thetislink-relay station add --label PA3GHM
docker compose exec thetislink-relay thetislink-relay station list
docker compose restart          # relay picks up the registry on restart
```
Then set that secret as the **Token** in the station's server config and in each of
its clients. Repeat `station add` per station. An empty registry keeps the legacy
global-token behavior, so nothing breaks until you add the first station.

## 7. Admin dashboard (web beheer)

The relay ships a web dashboard for managing stations and devices (create/rename/
enable, rotate secrets, block/rename devices, see traffic + last-seen + IP).
Caddy routes `/admin*` to the relay's internal admin API on port `18081` (never
published to the host); the WS tunnel keeps using the root path on `18080`.

> **Security — standalone deployments:** the admin API defaults to `0.0.0.0:18081`,
> which is safe ONLY in this compose topology (the port is not host-published and the
> client IP comes from Caddy's `X-Forwarded-For`). If you run the relay outside compose
> or expose the host network, set `THETISLINK_RELAY_ADMIN_LISTEN=127.0.0.1:18081` (or keep
> it strictly behind a trusted reverse proxy) — otherwise the admin API is reachable
> directly and the XFF client IP can be spoofed.

> **Device model:** there is no per-device approval step. Any device with the station
> secret is admitted (up to `max_devices`); `block` is the manual off-switch and takes
> effect immediately (live sessions are kicked).

**Prerequisites already in the config:** the `Caddyfile` has the `handle /admin*`
route and `docker-compose.yml` exposes `18081` on the compose network. Just rebuild:

```bash
cd thetislink-relay
docker compose up -d --build
```

**Bootstrap the admin login (one-time).** The admin API only starts when the
registry DB exists, and the login needs a password (Argon2id — never stored in
plaintext). Set it inside the container, then restart so the API comes up:

```bash
docker compose exec thetislink-relay thetislink-relay set-admin-password 'CHOOSE-A-STRONG-PASSWORD'
docker compose restart thetislink-relay
docker compose logs --tail=5 thetislink-relay   # look for "admin API listening on 0.0.0.0:18081"
```

**Open the dashboard:** `https://your-relay.duckdns.org/admin`
Log in with that password. From there you can add stations (each `Nieuwe sleutel`
is shown exactly once — copy it into that station's Token), and once a device
connects it appears under its station with its traffic and last-seen.

On a phone you can use **"Add to home screen"** — it installs as a standalone app.

**Change the password later:** rerun `set-admin-password` with a new value and
`docker compose restart thetislink-relay`.

**Optional — lock the dashboard to known IPs:** uncomment the `@notadmin ... abort`
lines in the `handle /admin*` block of the `Caddyfile` and rebuild. The app's login
+ rate-limit already protect it; this is defense-in-depth.

## Troubleshooting

- **No certificate / TLS-ALPN fails:** confirm port 443 is reachable from the internet
  and the DuckDNS name resolves to the VPS IP. Fallback: DNS-01 via a custom Caddy
  image with the `caddy-dns/duckdns` module (see that module's README for the build config).
- **`RELAY_DOMAIN`/token error on `up`:** set them in `.env` first.
- **Restart clears everything:** `docker compose restart` (keeps data); `down` + `up`
  keeps the named volumes (registry + certs) too.
