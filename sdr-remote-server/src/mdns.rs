// SPDX-License-Identifier: GPL-2.0-or-later

//! mDNS service-advertise (PATCH-3 local-network discovery).
//!
//! Publishes `_thetislink._udp.local.` so desktop and Android clients on
//! the same LAN can find the server without the user having to type an
//! IP address. Failure is silent - manual IP entry remains the always-on
//! fallback for cross-subnet / VPN / internet scenarios.
//!
//! TXT records:
//!   version=<sdr-remote-core::VERSION>
//!   name=<friendly_name>   (omitted when no name configured; clients fall
//!                           back to the instance name = hostname)

use std::collections::HashMap;

use anyhow::{Context, Result};
use log::{info, warn};
use mdns_sd::{ServiceDaemon, ServiceInfo};

/// RFC-6763 service-type advertised by the server.
pub const SERVICE_TYPE: &str = "_thetislink._udp.local.";

/// RAII-wrapper around a single mDNS advertise. Drop / explicit `shutdown()`
/// deregisters the service and stops the daemon thread.
pub struct MdnsAdvertiser {
    daemon: ServiceDaemon,
    fullname: String,
}

impl MdnsAdvertiser {
    /// Start advertising the server on the local network.
    ///
    /// `port` is the UDP port the ThetisLink server is bound to.
    /// `friendly_name` is an optional human-readable label
    /// (e.g. "Shack PC"). When `None`, the OS hostname is used as the
    /// instance name and no `name` TXT key is published.
    pub fn start(port: u16, friendly_name: Option<&str>) -> Result<Self> {
        let daemon = ServiceDaemon::new().context("create mDNS daemon")?;
        let hostname = detect_hostname();
        // Instance name = friendly_name if given, else hostname. Spec allows
        // arbitrary UTF-8 here; mdns-sd handles escaping.
        let instance = friendly_name.unwrap_or(&hostname);
        let service_hostname = format!("{}.local.", sanitize_for_hostname(&hostname));

        let mut props: HashMap<String, String> = HashMap::new();
        props.insert("version".to_string(), sdr_remote_core::VERSION.to_string());
        if let Some(name) = friendly_name {
            // Only publish the `name` key when actually configured - avoids
            // a misleading TXT key that just duplicates the instance name.
            props.insert("name".to_string(), name.to_string());
        }

        // Empty addrs + `enable_addr_auto()` lets mdns-sd pick up every
        // local interface (and follow IP-change events). Avoids the
        // multi-NIC "advertised on the wrong interface" pitfall.
        let service_info = ServiceInfo::new(
            SERVICE_TYPE,
            instance,
            &service_hostname,
            "",
            port,
            props,
        )
        .context("build mDNS ServiceInfo")?
        .enable_addr_auto();

        let fullname = service_info.get_fullname().to_string();
        daemon.register(service_info).context("register mDNS service")?;
        info!(
            "mDNS: advertising '{}' on port {} ({})",
            instance, port, SERVICE_TYPE
        );

        Ok(Self { daemon, fullname })
    }

    /// Explicit graceful shutdown: deregister + stop daemon.
    /// Drop will also call this, but explicit shutdown gives the caller a
    /// chance to log unregister failures (warnings only - best-effort).
    pub fn shutdown(self) {
        let fullname = self.fullname.clone();
        match self.daemon.unregister(&fullname) {
            Ok(rx) => {
                // Block briefly for the goodbye-packet to be flushed.
                let _ = rx.recv_timeout(std::time::Duration::from_millis(500));
            }
            Err(e) => warn!("mDNS unregister failed: {}", e),
        }
        if let Err(e) = self.daemon.shutdown() {
            warn!("mDNS daemon shutdown failed: {}", e);
        }
        info!("mDNS: advertise stopped");
    }
}

fn detect_hostname() -> String {
    // Windows sets COMPUTERNAME; POSIX shells set HOSTNAME; fall back to
    // a stable generic. No external syscall - sufficient for an mDNS label.
    std::env::var("COMPUTERNAME")
        .ok()
        .or_else(|| std::env::var("HOSTNAME").ok())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "thetislink-server".to_string())
}

/// Strip characters that are not safe for an mDNS host label.
/// RFC 1035 host labels are A-Z, 0-9, '-'. mDNS allows UTF-8 in the
/// instance name but the *hostname* part is stricter.
fn sanitize_for_hostname(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if s.is_empty() {
        "thetislink-server".to_string()
    } else {
        s
    }
}
