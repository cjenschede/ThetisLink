// SPDX-License-Identifier: GPL-2.0-or-later
//
//! ThetisLink chat — phase 1: one text channel.
//!
//! Phase 0 was a container that only reported its version, so that a chat deploy
//! could be shown to leave the relay running before anything worth risking was
//! inside it. That held, so this is what goes in it.
//!
//! What it serves: consent, one shared channel, and leaving again. Everything
//! else the design describes — the diagnosis postbox, moderation, Android — is a
//! later phase and deliberately absent here.
//!
//! See `docs/internal/DESIGN-relay-chat.md` §2 for the separation and §2.5 for
//! why the version handshake here is its own, independent of the ThetisLink wire
//! protocol: the two update separately, so a shared version number would force
//! them to move together again.

mod admin_page;
mod admin_session;
mod guard;
mod postbox;
mod relay_login;
mod http;
mod routes;
mod store;
mod ticket;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use log::{error, info, warn};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};

/// The chat's own protocol version, deliberately not ThetisLink's `VERSION`.
///
/// A client that speaks a higher number than this must fall back rather than
/// break, and so must this server (design §2.5). Phase 0 has nothing to
/// negotiate yet, but the number ships from the start so a client can ask before
/// there is anything to ask about.
const CHAT_PROTOCOL: u32 = 1;

const PKG_VERSION: &str = env!("CARGO_PKG_VERSION");

/// How long a single request may take from accept to response.
///
/// Phase 0 serves one tiny answer, so this is not about throughput: it is about
/// a connection that opens and then says nothing holding a task forever. Design
/// §2.2 puts the real limits on the container and on Caddy; this is the one the
/// process can enforce by itself.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Largest request this phase will read.
///
#[tokio::main]
async fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let listen = parse_listen(std::env::args().skip(1));
    info!(
        "ThetisLink chat v{} (protocol {}) - phase 2, chat + diagnosis postbox",
        PKG_VERSION, CHAT_PROTOCOL
    );

    let keys = keys_from_env();
    if keys.is_empty() {
        // Refusing to start is the only safe reading: with no key every ticket
        // fails, so the service would answer nothing but 403 while looking
        // healthy. A container that will not start is a problem somebody sees.
        error!("THETISLINK_CHAT_KEYS is not set - refusing to start without it");
        std::process::exit(1);
    }
    info!("{} signing key(s) accepted", keys.len());

    let db = std::env::var("THETISLINK_CHAT_DB").unwrap_or_else(|_| "/data/chat.db".to_string());
    let store = match store::Store::open(&db) {
        Ok(s) => s,
        Err(e) => {
            error!("cannot open {}: {}", db, e);
            std::process::exit(1);
        }
    };
    info!("store at {}", db);

    // The administrator credential, separate from any station ticket: reading
    // other people's reports is a different authority from being in the chat.
    // Absent means the postbox simply has no administrator side, and those paths
    // answer 404 like any other unknown one.
    let admin_token = std::env::var("THETISLINK_CHAT_ADMIN_TOKEN").ok().filter(|t| !t.trim().is_empty());
    if admin_token.is_none() {
        info!("no admin token set - the diagnosis postbox has no administrator side");
    }

    let app = Arc::new(routes::App {
        store: std::sync::Mutex::new(store),
        guard: std::sync::Mutex::new(guard::Guard::default()),
        keys,
        relay_admin: Some(relay_login::relay_admin_addr()),
        sessions: std::sync::Mutex::new(admin_session::Sessions::default()),
        admin_token,
    });

    // Housekeeping on its own clock rather than on the request path: a message
    // must never wait for a prune, and this runs on a machine that is also
    // carrying audio.
    {
        let app = Arc::clone(&app);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(3600));
            loop {
                tick.tick().await;
                let now = unix_now();
                let Ok(store) = app.store.lock() else {
                    // Said rather than skipped in silence. This is the one
                    // place in the service that swallowed a poisoned lock
                    // without a word, and it is the loop whose whole purpose
                    // is to be visible - the table would simply stop growing
                    // and nobody could find out why.
                    warn!("housekeeping skipped: the store lock is poisoned");
                    continue;
                };
                {
                    let mut ok = true;
                    let messages = match store.prune(now) {
                        Ok((by_age, by_size)) => by_age + by_size,
                        Err(e) => {
                            warn!("pruning failed: {}", e);
                            ok = false;
                            0
                        }
                    };
                    // The postbox has its own expiry and was never on this
                    // timer, so an uncollected report sat there for good - the
                    // design says thirty days (section 1.4).
                    let postbox = match postbox::prune(&store.conn, now) {
                        Ok(counts) => counts,
                        Err(e) => {
                            warn!("pruning the postbox failed: {}", e);
                            ok = false;
                            postbox::Counts::default()
                        }
                    };
                    // Said every round, including the rounds that remove
                    // nothing. The parts above only speak when they delete
                    // something, and the shortest period here is thirty days -
                    // so on a young service the silence of a working timer and
                    // the silence of a timer that never started read exactly
                    // alike. This is the line that tells them apart.
                    let run = store::Housekeeping {
                        at: now,
                        messages,
                        reports: postbox.reports,
                        replies: postbox.delivered_replies + postbox.undelivered_replies,
                        markers: postbox.markers,
                        ok,
                    };
                    if run.ok {
                        info!(
                            "housekeeping ran: {} message(s), {} report(s), {} answer(s), {} marker(s) removed",
                            run.messages, run.reports, run.replies, run.markers,
                        );
                    } else {
                        warn!("housekeeping ran but did not finish - see the errors above");
                    }
                    // And written down, so this can be watched from the admin
                    // page. Reading it out of a container's log means an ssh
                    // session and remembering the syntax, which is the kind of
                    // check that stops being done.
                    if let Err(e) = store.record_housekeeping(run) {
                        warn!("recording the housekeeping round failed: {}", e);
                    }
                }
            }
        });
    }

    let listener = match TcpListener::bind(listen).await {
        Ok(l) => l,
        Err(e) => {
            error!("cannot bind {}: {}", listen, e);
            std::process::exit(1);
        }
    };
    info!("listening on {}", listen);

    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                let app = Arc::clone(&app);
                tokio::spawn(async move {
                    // A slow or silent peer must not outlive its usefulness.
                    match tokio::time::timeout(REQUEST_TIMEOUT, serve(stream, app)).await {
                        Ok(Err(e)) => warn!("{}: {}", peer, e),
                        Err(_) => warn!("{}: request timed out", peer),
                        Ok(Ok(())) => {}
                    }
                });
            }
            // An accept error is not a reason to stop listening: one refused
            // connection should not take the service down.
            Err(e) => warn!("accept failed: {}", e),
        }
    }
}

/// `--listen <addr>`, defaulting to the port Caddy proxies `/chat*` to.
fn parse_listen(args: impl Iterator<Item = String>) -> SocketAddr {
    let args: Vec<String> = args.collect();
    let want = args
        .windows(2)
        .find(|w| w[0] == "--listen")
        .map(|w| w[1].clone());
    match want {
        Some(a) => a.parse().unwrap_or_else(|_| {
            warn!("cannot parse --listen {:?}, using the default", a);
            default_listen()
        }),
        None => default_listen(),
    }
}

fn default_listen() -> SocketAddr {
    "0.0.0.0:18082".parse().expect("the default address parses")
}

async fn serve(mut stream: TcpStream, app: Arc<routes::App>) -> std::io::Result<()> {
    let Some(req) = http::read_request(&mut stream).await else {
        // Not a request we can serve. Most of what an open port receives is not.
        stream.write_all(&http::response("400 Bad Request", "{}")).await?;
        return stream.flush().await;
    };
    let now = unix_now();
    let reply = routes::route_async(&app, &req, now).await;
    stream
        .write_all(&http::response_with(
            reply.status,
            reply.content_type,
            &reply.extra_headers,
            &reply.body,
        ))
        .await?;
    stream.flush().await
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The signing keys the relay uses, from the environment.
///
/// Never from the repository (design §3.3). Comma-separated so two can be live
/// at once during a rotation: the relay switches to the new one while the chat
/// still accepts both, and only then is the old one dropped — no relay restart
/// anywhere in that sequence.
fn keys_from_env() -> Vec<Vec<u8>> {
    match std::env::var("THETISLINK_CHAT_KEYS") {
        Ok(v) => v
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.as_bytes().to_vec())
            .collect(),
        Err(_) => Vec::new(),
    }
}
