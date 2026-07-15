// SPDX-License-Identifier: GPL-2.0-or-later
//! Fase 2 admin dashboard HTTP API (axum), served on an INTERNAL port behind Caddy.
//!
//! Security baseline (PATCH-relay-admin-dashboard-fase2 section 11):
//! - bound to localhost only (Caddy terminates TLS + proxies /admin -> here);
//! - Argon2id admin login (verify in store); no public "create admin" - bootstrap CLI;
//! - HttpOnly + Secure + SameSite=Strict session cookie, sliding idle TTL, logout;
//! - CSRF token per session, required on mutating requests (added in increment 3);
//! - per-IP login rate-limit; responses that carry secrets are `no-store`;
//! - no wildcard CORS (same-origin only - the dashboard is served from here).

use axum::{
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{get, patch, post},
    Json, Router,
};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::store::{self, Store};

type SharedStore = Arc<Mutex<Store>>;

const SESSION_IDLE_TTL: Duration = Duration::from_secs(30 * 60);
const LOGIN_MAX_FAILS: u32 = 5;
const LOGIN_FAIL_WINDOW: Duration = Duration::from_secs(5 * 60);
const COOKIE: &str = "tl_session";

struct Session {
    csrf: String,
    expires_at: Instant,
}

#[derive(Default)]
struct LoginLimiter {
    fails: HashMap<String, (u32, Instant)>, // ip -> (count, window_start)
}

impl LoginLimiter {
    fn is_blocked(&self, ip: &str) -> bool {
        matches!(self.fails.get(ip), Some((c, t)) if *c >= LOGIN_MAX_FAILS && t.elapsed() < LOGIN_FAIL_WINDOW)
    }
    fn record_fail(&mut self, ip: &str) {
        let now = Instant::now();
        // Bound memory under a distributed login flood: once the map grows past a
        // threshold, drop entries whose window has fully expired (they no longer block).
        if self.fails.len() > 1024 {
            self.fails.retain(|_, (_, t)| t.elapsed() < LOGIN_FAIL_WINDOW);
        }
        let e = self.fails.entry(ip.to_string()).or_insert((0, now));
        if e.1.elapsed() >= LOGIN_FAIL_WINDOW {
            *e = (0, now); // window rolled over
        }
        e.0 += 1;
    }
    fn reset(&mut self, ip: &str) {
        self.fails.remove(ip);
    }
}

#[derive(Clone)]
struct AppState {
    store: SharedStore,
    /// Live room map, shared with the relay, so blocking a device can
    /// close its active connection immediately (not only on next reconnect).
    rooms: crate::Rooms,
    sessions: Arc<Mutex<HashMap<String, Session>>>,
    limiter: Arc<Mutex<LoginLimiter>>,
}

/// Serve the admin API on an internal address (e.g. 127.0.0.1:18081), behind Caddy.
pub async fn serve(store: SharedStore, rooms: crate::Rooms, listen: &str) -> anyhow::Result<()> {
    let state = AppState {
        store,
        rooms,
        sessions: Arc::new(Mutex::new(HashMap::new())),
        limiter: Arc::new(Mutex::new(LoginLimiter::default())),
    };
    let app = Router::new()
        .route("/admin", get(index))
        .route("/admin/", get(index))
        // PWA shell assets (no auth: they carry no data, only the app frame).
        .route("/admin/manifest.webmanifest", get(manifest))
        .route("/admin/sw.js", get(service_worker))
        .route("/admin/icon.svg", get(icon))
        .route("/admin/api/login", post(login))
        .route("/admin/api/logout", post(logout))
        .route("/admin/api/session", get(session_status))
        .route("/admin/api/stats", get(stats))
        .route("/admin/api/db/backup", get(db_backup))
        .route("/admin/api/stations", get(list_stations).post(create_station))
        .route(
            "/admin/api/stations/:id",
            patch(patch_station).delete(delete_station),
        )
        .route("/admin/api/stations/:id/rotate-secret", post(rotate_secret))
        .route("/admin/api/stations/:id/limits", post(set_limits))
        .route("/admin/api/stations/:id/usage", get(station_usage))
        .route("/admin/api/stations/:id/devices", get(list_station_devices))
        .route(
            "/admin/api/devices/:id",
            patch(patch_device).delete(delete_device),
        )
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(listen).await?;
    log::info!("admin API listening on {listen}");
    axum::serve(listener, app).await?;
    Ok(())
}

// --- helpers ---

fn gen_token() -> String {
    let mut buf = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut buf);
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// Real client IP from Caddy's X-Forwarded-For (all requests reach us via Caddy).
fn client_ip(headers: &HeaderMap) -> String {
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(',').next())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    let prefix = format!("{name}=");
    raw.split(';')
        .map(|p| p.trim())
        .find_map(|p| p.strip_prefix(&prefix).map(|v| v.to_string()))
}

/// Validate the session cookie; on success slide the idle TTL and return the CSRF
/// token for this session. `None` = not authenticated. Never holds the lock across
/// an await (all work is synchronous).
fn validate_session(state: &AppState, headers: &HeaderMap) -> Option<String> {
    let token = cookie_value(headers, COOKIE)?;
    let mut sessions = state.sessions.lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    sessions.retain(|_, s| s.expires_at > now);
    let s = sessions.get_mut(&token)?;
    s.expires_at = now + SESSION_IDLE_TTL;
    Some(s.csrf.clone())
}

fn no_store(mut resp: Response) -> Response {
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    resp
}

// --- handlers ---

#[derive(Deserialize)]
struct LoginReq {
    password: String,
}
#[derive(Serialize)]
struct LoginResp {
    csrf: String,
}

async fn login(State(state): State<AppState>, headers: HeaderMap, Json(req): Json<LoginReq>) -> Response {
    let ip = client_ip(&headers);
    if state.limiter.lock().unwrap_or_else(|e| e.into_inner()).is_blocked(&ip) {
        log::warn!("admin login rate-limited for {ip}");
        return (StatusCode::TOO_MANY_REQUESTS, "too many attempts, try later").into_response();
    }
    let hash = {
        let store = state.store.lock().unwrap_or_else(|e| e.into_inner());
        store.admin_password_hash().unwrap_or(None)
    };
    let ok = matches!(hash, Some(ref h) if store::verify_password(&req.password, h));
    if !ok {
        state.limiter.lock().unwrap_or_else(|e| e.into_inner()).record_fail(&ip);
        log::warn!("admin login failed for {ip}");
        return (StatusCode::UNAUTHORIZED, "invalid credentials").into_response();
    }
    state.limiter.lock().unwrap_or_else(|e| e.into_inner()).reset(&ip);
    let token = gen_token();
    let csrf = gen_token();
    state.sessions.lock().unwrap_or_else(|e| e.into_inner()).insert(
        token.clone(),
        Session { csrf: csrf.clone(), expires_at: Instant::now() + SESSION_IDLE_TTL },
    );
    log::info!("admin login OK for {ip}");
    let cookie = format!(
        "{COOKIE}={token}; HttpOnly; Secure; SameSite=Strict; Path=/admin; Max-Age={}",
        SESSION_IDLE_TTL.as_secs()
    );
    let mut resp = Json(LoginResp { csrf }).into_response();
    resp.headers_mut().insert(header::SET_COOKIE, cookie.parse().unwrap());
    no_store(resp)
}

async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Response {
    // CSRF on logout too, for baseline consistency: a cross-site page should not be able
    // to force-logout the admin (SameSite=Strict already blocks it, this is belt-and-braces).
    if let Err(resp) = require_csrf(&state, &headers) {
        return resp;
    }
    if let Some(token) = cookie_value(&headers, COOKIE) {
        state.sessions.lock().unwrap_or_else(|e| e.into_inner()).remove(&token);
    }
    let cookie = format!("{COOKIE}=; HttpOnly; Secure; SameSite=Strict; Path=/admin; Max-Age=0");
    let mut resp = StatusCode::NO_CONTENT.into_response();
    resp.headers_mut().insert(header::SET_COOKIE, cookie.parse().unwrap());
    resp
}

#[derive(Serialize)]
struct SessionResp {
    authenticated: bool,
    /// Present only when authenticated. Returned so a reloaded page (whose session
    /// cookie still authenticates) can recover the CSRF token for mutations without a
    /// fresh login. Safe: SameSite=Strict stops a cross-site page from sending the
    /// cookie, and the same-origin policy stops it from reading this JSON.
    csrf: Option<String>,
}
async fn session_status(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let csrf = validate_session(&state, &headers);
    no_store(
        Json(SessionResp {
            authenticated: csrf.is_some(),
            csrf,
        })
        .into_response(),
    )
}

#[derive(Serialize)]
struct StationDto {
    id: i64,
    label: String,
    owner: String,
    enabled: bool,
    created_at: i64,
    // Limits (None = no limit) + live figures for the dashboard.
    max_devices: Option<i64>,
    max_clients: Option<i64>,
    max_monthly_bytes: Option<i64>,
    month_bytes: i64,
    devices: i64,
    connected_now: i64,
}
async fn list_stations(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if validate_session(&state, &headers).is_none() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let ym = crate::year_month(now_unix());
    // Collect all DB-side facts under one lock (no await while the lock is held).
    let facts: Vec<(store::StationRow, (Option<i64>, Option<i64>, Option<i64>), i64, i64)> = {
        let store = state.store.lock().unwrap_or_else(|e| e.into_inner());
        store
            .list()
            .unwrap_or_default()
            .into_iter()
            .map(|r| {
                let limits = store.station_limits(r.id).unwrap_or((None, None, None));
                let month = store.station_month_bytes(r.id, &ym).unwrap_or(0);
                let devices = store.count_enabled_devices(r.id).unwrap_or(0);
                (r, limits, month, devices)
            })
            .collect()
    };
    let mut dto = Vec::with_capacity(facts.len());
    for (r, limits, month_bytes, devices) in facts {
        let connected_now =
            crate::room_client_count(&state.rooms, &format!("db:{}", r.id)).await as i64;
        dto.push(StationDto {
            id: r.id,
            label: r.label,
            owner: r.owner,
            enabled: r.enabled,
            created_at: r.created_at,
            max_devices: limits.0,
            max_clients: limits.1,
            max_monthly_bytes: limits.2,
            month_bytes,
            devices,
            connected_now,
        });
    }
    Json(dto).into_response()
}

#[derive(Deserialize)]
struct LimitsReq {
    max_devices: Option<i64>,
    max_clients: Option<i64>,
    max_monthly_bytes: Option<i64>,
}
async fn set_limits(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Json(req): Json<LimitsReq>,
) -> Response {
    if let Err(r) = require_csrf(&state, &headers) {
        return r;
    }
    // Treat non-positive / absent as "no limit" (NULL) - forgiving for empty fields.
    let norm = |v: Option<i64>| v.filter(|&x| x > 0);
    let ok = {
        let store = state.store.lock().unwrap_or_else(|e| e.into_inner());
        store
            .set_station_limits(
                id,
                norm(req.max_devices),
                norm(req.max_clients),
                norm(req.max_monthly_bytes),
            )
            .unwrap_or(false)
    };
    if ok {
        StatusCode::NO_CONTENT.into_response()
    } else {
        StatusCode::NOT_FOUND.into_response()
    }
}

#[derive(Serialize)]
struct MonthUsage {
    ym: String,
    bytes: i64,
}
#[derive(Serialize)]
struct UsageResp {
    ym: String,
    month_bytes: i64,
    history: Vec<MonthUsage>,
}
async fn station_usage(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Response {
    if validate_session(&state, &headers).is_none() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let ym = crate::year_month(now_unix());
    let (month_bytes, history) = {
        let store = state.store.lock().unwrap_or_else(|e| e.into_inner());
        let m = store.station_month_bytes(id, &ym).unwrap_or(0);
        let h = store.station_month_history(id, 12).unwrap_or_default();
        (m, h)
    };
    Json(UsageResp {
        ym,
        month_bytes,
        history: history
            .into_iter()
            .map(|(ym, bytes)| MonthUsage { ym, bytes })
            .collect(),
    })
    .into_response()
}

#[derive(Serialize)]
struct StatsResp {
    stations: i64,
    devices: i64,
}
async fn stats(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if validate_session(&state, &headers).is_none() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let (stations, devices) = {
        let store = state.store.lock().unwrap_or_else(|e| e.into_inner());
        (store.count().unwrap_or(0), store.device_count().unwrap_or(0))
    };
    Json(StatsResp { stations, devices }).into_response()
}

/// Full-database backup download (admin only). Streams a consistent SQLite snapshot so
/// the registry - station keys, monthly usage, device settings - survives a VPS loss or
/// crash. Requires a valid session AND CSRF: a whole-DB export is the most sensitive
/// endpoint, so we demand the anti-CSRF token even though it is a GET (defense-in-depth).
async fn db_backup(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(resp) = require_csrf(&state, &headers) {
        return resp;
    }
    // Audit trail: a full-DB export is the most sensitive action; record who took it.
    log::info!("admin DB backup downloaded by {}", client_ip(&headers));
    // VACUUM INTO is blocking (disk I/O + full copy). Run it on the blocking pool so it
    // never stalls an async worker thread - critical on a single-vCPU VPS, where stalling
    // the one worker would put an audible gap in the live audio relay.
    let store = state.store.clone();
    let snapshot = tokio::task::spawn_blocking(move || {
        let store = store.lock().unwrap_or_else(|e| e.into_inner());
        store.snapshot_bytes()
    })
    .await;
    let data = match snapshot {
        Ok(Ok(d)) => d,
        Ok(Err(err)) => {
            log::warn!("db backup snapshot failed: {err:#}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "backup failed").into_response();
        }
        Err(err) => {
            log::warn!("db backup task join failed: {err:#}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "backup failed").into_response();
        }
    };
    let fname = format!("thetislink-relay-backup-{}.db", now_unix());
    let mut resp = data.into_response();
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, "application/octet-stream".parse().unwrap());
    h.insert(
        header::CONTENT_DISPOSITION,
        format!("attachment; filename=\"{fname}\"").parse().unwrap(),
    );
    h.insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    resp
}

fn now_unix() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Require a valid session AND a matching CSRF token - for every mutating request
/// (POST/PATCH/DELETE). GET endpoints only need a valid session.
fn require_csrf(state: &AppState, headers: &HeaderMap) -> Result<(), Response> {
    let csrf = validate_session(state, headers)
        .ok_or_else(|| StatusCode::UNAUTHORIZED.into_response())?;
    let sent = headers
        .get("x-csrf-token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if sent == csrf {
        Ok(())
    } else {
        Err((StatusCode::FORBIDDEN, "missing or invalid CSRF token").into_response())
    }
}

// --- station mutations ---

#[derive(Deserialize)]
struct CreateStationReq {
    label: String,
    owner: Option<String>,
}
/// Response for create/rotate - the secret is shown EXACTLY ONCE here (no-store),
/// never returned again by any list/detail endpoint (security baseline).
#[derive(Serialize)]
struct SecretResp {
    id: i64,
    secret: String,
}

async fn create_station(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<CreateStationReq>,
) -> Response {
    if let Err(r) = require_csrf(&state, &headers) {
        return r;
    }
    let label = req.label.trim().to_string();
    if label.is_empty() || label.len() > 64 {
        return (StatusCode::BAD_REQUEST, "invalid label").into_response();
    }
    let owner = req.owner.unwrap_or_default();
    let secret = store::generate_secret();
    let id = {
        let store = state.store.lock().unwrap_or_else(|e| e.into_inner());
        store.add(&label, &owner, &secret, now_unix())
    };
    match id {
        Ok(id) => no_store(Json(SecretResp { id, secret }).into_response()),
        Err(e) => {
            log::error!("create station: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[derive(Deserialize)]
struct PatchStationReq {
    label: Option<String>,
    enabled: Option<bool>,
}
async fn patch_station(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Json(req): Json<PatchStationReq>,
) -> Response {
    if let Err(r) = require_csrf(&state, &headers) {
        return r;
    }
    if let Some(l) = req.label.as_deref() {
        let l = l.trim();
        if l.is_empty() || l.len() > 64 {
            return (StatusCode::BAD_REQUEST, "invalid label").into_response();
        }
    }
    {
        let store = state.store.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(l) = req.label.as_deref() {
            let _ = store.set_label(id, l.trim());
        }
        if let Some(en) = req.enabled {
            let _ = store.set_enabled(id, en);
        }
    }
    StatusCode::NO_CONTENT.into_response()
}

async fn rotate_secret(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = require_csrf(&state, &headers) {
        return r;
    }
    let secret = store::generate_secret();
    let ok = state
        .store
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .rotate_secret(id, &secret)
        .unwrap_or(false);
    if ok {
        no_store(Json(SecretResp { id, secret }).into_response())
    } else {
        StatusCode::NOT_FOUND.into_response()
    }
}

async fn delete_station(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = require_csrf(&state, &headers) {
        return r;
    }
    let ok = state
        .store
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(id)
        .unwrap_or(false);
    if ok {
        StatusCode::NO_CONTENT.into_response()
    } else {
        StatusCode::NOT_FOUND.into_response()
    }
}

// --- devices ---

#[derive(Serialize)]
struct DeviceDto {
    id: i64,
    install_id: String, // shortened (never the full 128-char id)
    enroll_seq: i64,
    platform: String,
    name: Option<String>,
    enabled: bool,
    first_seen: i64,
    last_seen: i64,
    sessions: i64,
    bytes_total: i64,
    last_ip: Option<String>,
}
async fn list_station_devices(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Response {
    if validate_session(&state, &headers).is_none() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let rows = state
        .store
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .list_devices(id)
        .unwrap_or_default();
    let dto: Vec<DeviceDto> = rows
        .into_iter()
        .map(|r| DeviceDto {
            id: r.id,
            install_id: r.install_id.chars().take(12).collect(),
            enroll_seq: r.enroll_seq,
            platform: r.platform,
            name: r.name,
            enabled: r.enabled,
            first_seen: r.first_seen,
            last_seen: r.last_seen,
            sessions: r.sessions,
            bytes_total: r.bytes_total,
            last_ip: r.last_ip,
        })
        .collect();
    Json(dto).into_response()
}

#[derive(Deserialize)]
struct PatchDeviceReq {
    name: Option<String>,
    enabled: Option<bool>,
}
async fn patch_device(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Json(req): Json<PatchDeviceReq>,
) -> Response {
    if let Err(r) = require_csrf(&state, &headers) {
        return r;
    }
    if let Some(n) = req.name.as_deref() {
        if n.trim().len() > 40 {
            return (StatusCode::BAD_REQUEST, "name too long").into_response();
        }
    }
    let info = {
        let store = state.store.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = req.name.as_deref() {
            let _ = store.rename_device(id, n.trim());
        }
        if let Some(en) = req.enabled {
            let _ = store.set_device_enabled(id, en);
        }
        // Read back the resulting state to decide on an immediate kick.
        store.device_admit_info(id).unwrap_or(None)
    };
    // If the device was just blocked, close any live session now instead of waiting
    // for its next reconnect (which the connect gate would refuse anyway).
    if let Some((station_id, install_id, false)) = info {
        let n = crate::kick_device(&state.rooms, station_id, &install_id).await;
        if n > 0 {
            log::info!("kicked {n} live session(s) for device {id} (blocked)");
        }
    }
    StatusCode::NO_CONTENT.into_response()
}

async fn delete_device(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = require_csrf(&state, &headers) {
        return r;
    }
    let ok = state
        .store
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove_device(id)
        .unwrap_or(false);
    if ok {
        StatusCode::NO_CONTENT.into_response()
    } else {
        StatusCode::NOT_FOUND.into_response()
    }
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

// --- PWA shell assets (installable "add to home screen") ---

async fn manifest() -> Response {
    ([(header::CONTENT_TYPE, "application/manifest+json")], MANIFEST_JSON).into_response()
}
async fn service_worker() -> Response {
    ([(header::CONTENT_TYPE, "text/javascript")], SERVICE_WORKER_JS).into_response()
}
async fn icon() -> Response {
    ([(header::CONTENT_TYPE, "image/svg+xml")], ICON_SVG).into_response()
}

const MANIFEST_JSON: &str = r##"{
  "name": "ThetisLink Relay beheer",
  "short_name": "TL Relay",
  "start_url": "/admin/",
  "scope": "/admin/",
  "display": "standalone",
  "background_color": "#111111",
  "theme_color": "#1b3a5b",
  "icons": [
    { "src": "/admin/icon.svg", "sizes": "any", "type": "image/svg+xml", "purpose": "any maskable" }
  ]
}"##;

// Minimal service worker: cache the app shell for installability/offline frame,
// but never cache the API (always live). Scope defaults to /admin/ (its own dir).
const SERVICE_WORKER_JS: &str = r##"const C='tl-admin-v1';
const SHELL=['/admin/','/admin/icon.svg','/admin/manifest.webmanifest'];
self.addEventListener('install',e=>{e.waitUntil(caches.open(C).then(c=>c.addAll(SHELL)).then(()=>self.skipWaiting()));});
self.addEventListener('activate',e=>{e.waitUntil(caches.keys().then(k=>Promise.all(k.filter(x=>x!==C).map(x=>caches.delete(x)))).then(()=>self.clients.claim()));});
self.addEventListener('fetch',e=>{
  const u=new URL(e.request.url);
  if(u.pathname.startsWith('/admin/api/'))return; // API: network only
  e.respondWith(fetch(e.request).catch(()=>caches.match(e.request).then(r=>r||caches.match('/admin/'))));
});"##;

const ICON_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 512 512">
<rect width="512" height="512" rx="96" fill="#1b3a5b"/>
<g fill="none" stroke="#7fb2ff" stroke-width="20" stroke-linecap="round">
<path d="M188 196a96 96 0 0 0 0 120"/><path d="M148 164a150 150 0 0 0 0 184"/>
<path d="M324 196a96 96 0 0 1 0 120"/><path d="M364 164a150 150 0 0 1 0 184"/>
</g><circle cx="256" cy="256" r="34" fill="#fff"/>
<text x="256" y="430" font-family="system-ui,sans-serif" font-size="72" font-weight="700" fill="#fff" text-anchor="middle">TL</text>
</svg>"##;

/// Full mobile-friendly dashboard (increment 5): login, live stats, station CRUD
/// (create / rename / enable / rotate-secret / delete) and, per station, its device
/// list with block / rename / delete and traffic/last-seen columns. All
/// mutations carry the CSRF token; secrets are revealed exactly once in an overlay.
const INDEX_HTML: &str = r##"<!doctype html><html lang="nl"><head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1,viewport-fit=cover">
<meta name="theme-color" content="#1b3a5b">
<link rel="manifest" href="/admin/manifest.webmanifest">
<link rel="icon" href="/admin/icon.svg">
<link rel="apple-touch-icon" href="/admin/icon.svg">
<title>ThetisLink Relay - beheer</title>
<style>
 :root{--bg:#111;--fg:#eee;--card:#1a1a1a;--line:#333;--accent:#2d6cb5;--muted:#999}
 *{box-sizing:border-box}
 body{font-family:system-ui,-apple-system,Segoe UI,Roboto,sans-serif;margin:0;background:var(--bg);color:var(--fg)}
 header{background:#1b3a5b;padding:14px 16px;font-weight:600;display:flex;justify-content:space-between;align-items:center;position:sticky;top:0;z-index:5}
 header .sub{font-weight:400;font-size:13px;color:#b9cde6}
 main{max-width:820px;margin:0 auto;padding:16px}
 input,button,select{font-size:16px;padding:9px 11px;border-radius:8px;border:1px solid #444;background:#222;color:var(--fg)}
 button{background:var(--accent);color:#fff;border:0;cursor:pointer}
 button:hover{filter:brightness(1.1)} button:active{filter:brightness(.92)}
 button.danger{background:#8a2f2f} button.ghost{background:#333}
 .card{background:var(--card);border:1px solid var(--line);border-radius:12px;padding:14px;margin:12px 0}
 .row{display:flex;justify-content:space-between;align-items:center;gap:8px;flex-wrap:wrap}
 .acts{display:flex;gap:8px;flex-wrap:wrap;margin-top:10px}
 .acts.sm button{font-size:13px;padding:6px 9px}
 .badge{font-size:12px;padding:2px 8px;border-radius:20px;background:#333;color:#ddd}
 .badge.on{background:#1f5130;color:#bff0cf} .badge.off{background:#5a2a2a;color:#f2c9c9}
 .badge.warn{background:#5a4a1e;color:#f2e2b0}
 .devices{margin-top:10px;overflow-x:auto}
 table{width:100%;border-collapse:collapse;font-size:14px;min-width:520px}
 th,td{text-align:left;padding:7px 8px;border-bottom:1px solid var(--line);vertical-align:top}
 th{color:var(--muted);font-weight:600}
 .muted{color:var(--muted)} .sm{font-size:12px} .err{color:#e88}
 .newform{display:flex;gap:8px;flex-wrap:wrap;align-items:center}
 .newform input{flex:1;min-width:140px}
 .overlay{position:fixed;inset:0;background:rgba(0,0,0,.7);display:flex;align-items:center;justify-content:center;padding:16px;z-index:10}
 /* [hidden] must beat the display:flex above, else the modal shows empty on load. */
 .overlay[hidden]{display:none}
 .overlay .box{background:var(--card);border:1px solid var(--line);border-radius:12px;padding:18px;max-width:560px;width:100%}
 .secret{font-family:ui-monospace,Consolas,monospace;word-break:break-all;background:#000;border:1px solid var(--line);border-radius:8px;padding:10px;margin:10px 0;font-size:13px}
 .loginbox{max-width:340px;margin:8vh auto;text-align:center}
 .loginbox input{width:100%;margin:8px 0}
 .usage{margin-top:8px}
 .limrow{display:block;margin:10px 0} .limrow input{width:100%;margin-top:4px}
</style></head><body>
<header><span>ThetisLink Relay <span class="sub">beheer</span></span>
 <span style="flex:1"></span>
 <button id="backupbtn" class="ghost" hidden title="Download een volledige kopie van de database (stations, sleutels, verbruik, instellingen)">Backup DB</button>
 <button id="logoutbtn" class="ghost" hidden>Uitloggen</button></header>
<main>
 <div id="login" hidden>
   <div class="loginbox card">
     <p>Log in met het admin-wachtwoord.</p>
     <input id="pw" type="password" placeholder="wachtwoord" autocomplete="current-password">
     <button id="loginbtn">Inloggen</button>
     <p id="loginerr" class="err"></p>
   </div>
 </div>
 <div id="dash" hidden>
   <p class="muted" id="stats"></p>
   <div class="card">
     <div class="newform">
       <input id="newlabel" placeholder="Nieuw station (naam/call)" maxlength="64">
       <input id="newowner" placeholder="Eigenaar (optioneel)" maxlength="64">
       <button id="newbtn">Toevoegen</button>
     </div>
   </div>
   <div id="stations"></div>
 </div>
</main>
<div id="secmodal" class="overlay" hidden><div class="box">
  <h3>Sleutel voor <span id="seclabel"></span></h3>
  <p class="muted sm">Bewaar deze sleutel nu - hij wordt hierna nooit meer getoond (alleen de hash blijft bewaard).</p>
  <div id="secval" class="secret"></div>
  <div class="acts"><button id="seccopy">Kopieer</button><button id="secclose" class="ghost">Sluiten</button></div>
</div></div>
<div id="limmodal" class="overlay" hidden><div class="box">
  <h3>Limieten voor <span id="limlabel"></span></h3>
  <p class="muted sm">Leeg = geen limiet. De datalimiet is zacht: een lopende sessie mag doorlopen, nieuwe verbindingen worden geweigerd tot de volgende maand.</p>
  <label class="limrow">Max goedgekeurde apparaten<input id="limdev" type="number" min="0" inputmode="numeric" placeholder="geen limiet"></label>
  <label class="limrow">Max tegelijk verbonden<input id="limcli" type="number" min="0" inputmode="numeric" placeholder="geen limiet"></label>
  <label class="limrow">Max data per maand (MB)<input id="limdata" type="number" min="0" inputmode="numeric" placeholder="geen limiet"></label>
  <div id="limhist" class="muted sm"></div>
  <div class="acts"><button id="limsave">Opslaan</button><button id="limclose" class="ghost">Annuleren</button></div>
</div></div>
<script>
const $=s=>document.querySelector(s);
let csrf=null;
let limId=null;
const expanded=new Set();

async function api(p,o={}){
  o.credentials='same-origin';
  const m=(o.method||'GET').toUpperCase();
  if(m!=='GET'){o.headers=Object.assign({'Content-Type':'application/json','X-CSRF-Token':csrf||''},o.headers||{});}
  return fetch('/admin/api/'+p,o);
}
function esc(s){return (s==null?'':''+s).replace(/[<>&"']/g,c=>({'<':'&lt;','>':'&gt;','&':'&amp;','"':'&quot;',"'":'&#39;'}[c]));}
function fmtBytes(n){n=Number(n)||0;if(n<1024)return n+' B';const u=['KB','MB','GB','TB'];let i=-1;do{n/=1024;i++;}while(n>=1024&&i<u.length-1);return n.toFixed(1)+' '+u[i];}
function relTime(t){t=Number(t)||0;if(!t)return '-';const d=Math.floor(Date.now()/1000)-t;if(d<0)return 'zojuist';if(d<60)return d+' s';if(d<3600)return Math.floor(d/60)+' min';if(d<86400)return Math.floor(d/3600)+' u';return Math.floor(d/86400)+' d';}

async function refresh(){
  let s; try{s=await (await api('session')).json();}catch(e){return;}
  const authed=!!s.authenticated;
  $('#login').hidden=authed; $('#dash').hidden=!authed; $('#logoutbtn').hidden=!authed;
  $('#backupbtn').hidden=!authed;
  if(!authed){csrf=null;return;}
  csrf=s.csrf;
  try{const st=await (await api('stats')).json();$('#stats').textContent=st.stations+' stations - '+st.devices+' devices';}catch(e){}
  await loadStations();
}

async function loadStations(){
  let rows; try{rows=await (await api('stations')).json();}catch(e){return;}
  if(!Array.isArray(rows)){return;}
  const host=$('#stations'); host.innerHTML='';
  if(!rows.length){host.innerHTML='<p class="muted">Nog geen stations. Voeg er hierboven een toe.</p>';return;}
  for(const r of rows){
    const card=document.createElement('div'); card.className='card'; card.innerHTML=stationHtml(r);
    host.appendChild(card);
    if(expanded.has(r.id)){await loadDevices(r.id,card.querySelector('.devices'));}
  }
}
function stationHtml(r){
  const datamb=r.max_monthly_bytes?Math.round(r.max_monthly_bytes/1048576):'';
  const usage='<div class="usage muted sm">verbruik deze maand: <b>'+fmtBytes(r.month_bytes)+'</b>'+
    (r.max_monthly_bytes?(' van '+fmtBytes(r.max_monthly_bytes)):'')+
    ' - apparaten: '+r.devices+(r.max_devices?(' / '+r.max_devices):'')+
    ' - verbonden: '+r.connected_now+(r.max_clients?(' / '+r.max_clients):'')+'</div>';
  return '<div class="row"><div><b>'+esc(r.label)+'</b> <span class="muted sm">#'+r.id+(r.owner?' - '+esc(r.owner):'')+'</span></div>'+
   '<span class="badge '+(r.enabled?'on':'off')+'">'+(r.enabled?'actief':'uit')+'</span></div>'+
   usage+
   '<div class="acts">'+
   '<button data-act="devices" data-id="'+r.id+'">'+(expanded.has(r.id)?'Verberg devices':'Devices')+'</button>'+
   '<button class="ghost" data-act="limits" data-id="'+r.id+'" data-label="'+esc(r.label)+'" data-dev="'+(r.max_devices||'')+'" data-cli="'+(r.max_clients||'')+'" data-datamb="'+datamb+'">Limieten</button>'+
   '<button class="ghost" data-act="toggle-station" data-id="'+r.id+'" data-enabled="'+r.enabled+'">'+(r.enabled?'Zet uit':'Zet aan')+'</button>'+
   '<button class="ghost" data-act="rename-station" data-id="'+r.id+'" data-label="'+esc(r.label)+'">Naam</button>'+
   '<button data-act="rotate" data-id="'+r.id+'" data-label="'+esc(r.label)+'">Nieuwe sleutel</button>'+
   '<button class="danger" data-act="del-station" data-id="'+r.id+'" data-label="'+esc(r.label)+'">Verwijder</button>'+
   '</div><div class="devices"></div>';
}
async function loadDevices(id,host){
  host.innerHTML='<p class="muted sm">Laden...</p>';
  let rows; try{rows=await (await api('stations/'+id+'/devices')).json();}catch(e){host.innerHTML='<p class="err">Kon devices niet laden.</p>';return;}
  if(!rows.length){host.innerHTML='<p class="muted sm">Nog geen devices aangemeld.</p>';return;}
  host.innerHTML='<table><thead><tr><th>#</th><th>Naam</th><th>Platf.</th><th>Verkeer</th><th>Laatst</th><th>Status</th><th></th></tr></thead><tbody>'+rows.map(deviceRow).join('')+'</tbody></table>';
}
function deviceRow(d){
  const name=d.name?esc(d.name):'<span class="muted">('+esc(d.install_id)+')</span>';
  const st=d.enabled?'<span class="badge on">actief</span>':'<span class="badge off">geblokkeerd</span>';
  return '<tr><td>'+d.enroll_seq+'</td>'+
   '<td>'+name+'<div class="muted sm">'+d.sessions+'x - '+esc(d.last_ip||'')+'</div></td>'+
   '<td>'+esc(d.platform||'?')+'</td>'+
   '<td>'+fmtBytes(d.bytes_total)+'</td>'+
   '<td>'+relTime(d.last_seen)+'</td>'+
   '<td>'+st+'</td>'+
   '<td class="acts sm">'+
   '<button class="ghost" data-act="toggle-device" data-id="'+d.id+'" data-enabled="'+d.enabled+'">'+(d.enabled?'Blokkeer':'Deblokkeer')+'</button>'+
   '<button class="ghost" data-act="rename-device" data-id="'+d.id+'" data-name="'+esc(d.name||'')+'">Naam</button>'+
   '<button class="danger" data-act="del-device" data-id="'+d.id+'" data-name="'+esc(d.name||d.install_id)+'">x</button>'+
   '</td></tr>';
}

document.addEventListener('click',async e=>{
  const b=e.target.closest('button[data-act]'); if(!b)return;
  const id=b.dataset.id, act=b.dataset.act;
  try{
   if(act==='devices'){const n=+id; if(expanded.has(n))expanded.delete(n);else expanded.add(n); await loadStations(); return;}
   if(act==='limits'){openLimits(id,b.dataset.label,b.dataset.dev,b.dataset.cli,b.dataset.datamb); return;}
   if(act==='toggle-station'){await api('stations/'+id,{method:'PATCH',body:JSON.stringify({enabled:b.dataset.enabled!=='true'})}); await loadStations(); return;}
   if(act==='rename-station'){const v=prompt('Nieuwe naam voor station:',b.dataset.label); if(v&&v.trim()){await api('stations/'+id,{method:'PATCH',body:JSON.stringify({label:v.trim()})}); await loadStations();} return;}
   if(act==='rotate'){if(!confirm('Nieuwe sleutel voor "'+b.dataset.label+'"? De oude sleutel werkt daarna NIET meer.'))return; const r=await api('stations/'+id+'/rotate-secret',{method:'POST'}); if(r.ok){const j=await r.json(); showSecret(j.secret,b.dataset.label);}else alert('Sleutel roteren mislukt.'); return;}
   if(act==='del-station'){if(!confirm('Station "'+b.dataset.label+'" verwijderen? Dit kan niet ongedaan worden gemaakt.'))return; await api('stations/'+id,{method:'DELETE'}); await loadStations(); return;}
   if(act==='toggle-device'){await api('devices/'+id,{method:'PATCH',body:JSON.stringify({enabled:b.dataset.enabled!=='true'})}); await loadStations(); return;}
   if(act==='rename-device'){const v=prompt('Naam voor dit device:',b.dataset.name); if(v!=null){await api('devices/'+id,{method:'PATCH',body:JSON.stringify({name:v.trim()})}); await loadStations();} return;}
   if(act==='del-device'){if(!confirm('Device "'+b.dataset.name+'" verwijderen?'))return; await api('devices/'+id,{method:'DELETE'}); await loadStations(); return;}
  }catch(err){alert('Actie mislukt.');}
});

$('#loginbtn').onclick=async()=>{
  $('#loginerr').textContent='';
  let r; try{r=await api('login',{method:'POST',body:JSON.stringify({password:$('#pw').value})});}catch(e){$('#loginerr').textContent='Netwerkfout.';return;}
  if(r.ok){const j=await r.json();csrf=j.csrf;$('#pw').value='';refresh();}
  else{$('#loginerr').textContent=r.status===429?'Te veel pogingen, wacht even.':'Onjuist wachtwoord.';}
};
$('#pw').addEventListener('keydown',e=>{if(e.key==='Enter')$('#loginbtn').click();});
$('#logoutbtn').onclick=async()=>{await api('logout',{method:'POST'});csrf=null;refresh();};
$('#backupbtn').onclick=async()=>{
  const b=$('#backupbtn'); const old=b.textContent; b.disabled=true; b.textContent='Bezig...';
  try{
    // GET, but this whole-DB export demands the CSRF token (server-side require_csrf).
    const r=await fetch('/admin/api/db/backup',{credentials:'same-origin',headers:{'X-CSRF-Token':csrf||''}});
    if(!r.ok){alert('Backup mislukt ('+r.status+')');return;}
    const blob=await r.blob();
    const cd=r.headers.get('Content-Disposition')||'';
    const m=cd.match(/filename="?([^"]+)"?/);
    const name=m?m[1]:'thetislink-relay-backup.db';
    const url=URL.createObjectURL(blob);
    const a=document.createElement('a'); a.href=url; a.download=name;
    document.body.appendChild(a); a.click(); a.remove(); URL.revokeObjectURL(url);
  }catch(e){alert('Backup mislukt: '+e);}
  finally{b.disabled=false; b.textContent=old;}
};
$('#newbtn').onclick=async()=>{
  const label=$('#newlabel').value.trim(); if(!label){$('#newlabel').focus();return;}
  const owner=$('#newowner').value.trim();
  const r=await api('stations',{method:'POST',body:JSON.stringify({label,owner})});
  if(r.ok){const j=await r.json();$('#newlabel').value='';$('#newowner').value='';await loadStations();showSecret(j.secret,label);}
  else alert('Station aanmaken mislukt.');
};
function showSecret(secret,label){$('#seclabel').textContent=label;$('#secval').textContent=secret;$('#secmodal').hidden=false;}
$('#seccopy').onclick=async()=>{try{await navigator.clipboard.writeText($('#secval').textContent);$('#seccopy').textContent='Gekopieerd';setTimeout(()=>$('#seccopy').textContent='Kopieer',1500);}catch(e){}};
$('#secclose').onclick=()=>{$('#secmodal').hidden=true;};

function openLimits(id,label,dev,cli,datamb){
  limId=id; $('#limlabel').textContent=label;
  $('#limdev').value=dev||''; $('#limcli').value=cli||''; $('#limdata').value=datamb||'';
  $('#limhist').textContent='';
  $('#limmodal').hidden=false;
  api('stations/'+id+'/usage').then(r=>r.json()).then(u=>{
    if(u.history&&u.history.length){$('#limhist').innerHTML='Historie: '+u.history.map(h=>esc(h.ym)+' '+fmtBytes(h.bytes)).join(' - ');}
  }).catch(()=>{});
}
$('#limsave').onclick=async()=>{
  const dev=parseInt($('#limdev').value,10), cli=parseInt($('#limcli').value,10), mb=parseInt($('#limdata').value,10);
  const body={max_devices:isNaN(dev)?null:dev, max_clients:isNaN(cli)?null:cli, max_monthly_bytes:isNaN(mb)?null:Math.round(mb*1048576)};
  const r=await api('stations/'+limId+'/limits',{method:'POST',body:JSON.stringify(body)});
  if(r.ok){$('#limmodal').hidden=true;await loadStations();} else alert('Opslaan mislukt.');
};
$('#limclose').onclick=()=>{$('#limmodal').hidden=true;};

if('serviceWorker' in navigator){navigator.serviceWorker.register('/admin/sw.js').catch(()=>{});}
refresh();
// Auto-refresh so usage figures stay near-live; pause while a modal is open so
// it never disturbs editing a secret or the limits form.
setInterval(()=>{ if($('#secmodal').hidden && $('#limmodal').hidden) refresh(); }, 10000);
</script>
</body></html>"##;
