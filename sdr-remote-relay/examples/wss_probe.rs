// SPDX-License-Identifier: GPL-2.0-or-later
//! Diagnostic: try to open a WebSocket to a (w)ss:// URL via tokio-tungstenite +
//! rustls, exactly as the relay client does. Prints the precise error/panic.
//! Run: cargo run -p sdr-remote-relay --example wss_probe -- wss://host

fn main() {
    let url = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "wss://your-relay.duckdns.org".to_string());
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        eprintln!("connecting to {url} ...");
        match tokio::time::timeout(
            std::time::Duration::from_secs(15),
            tokio_tungstenite::connect_async(&url),
        )
        .await
        {
            Ok(Ok((_ws, resp))) => eprintln!("OK: WebSocket connected, status {:?}", resp.status()),
            Ok(Err(e)) => eprintln!("ERR: {e}\nDEBUG: {e:?}"),
            Err(_) => eprintln!("TIMEOUT after 15s (handshake never completed)"),
        }
    });
}
