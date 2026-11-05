// SPDX-License-Identifier: GPL-2.0-or-later

//! mDNS service-browse for local-network ThetisLink server discovery.
//! Runs in a background thread, publishes results via a
//! `Mutex<Vec<DiscoveredServer>>` that the egui thread reads each frame.
//!
//! Failure is silent: if mDNS doesn't work (no multicast, firewall,
//! cross-subnet) the user can always still type an IP by hand.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::thread;

use log::{debug, info, warn};
use mdns_sd::{ServiceDaemon, ServiceEvent};

/// Service-type browsed for. Must match the server's `mdns.rs`.
pub const SERVICE_TYPE: &str = "_thetislink._udp.local.";

/// One discovered ThetisLink server on the local network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredServer {
    /// Instance name as advertised by the server (`Shack PC` or hostname).
    pub instance: String,
    /// Optional human-readable label from the `name` TXT key.
    pub friendly_name: Option<String>,
    /// Best address:port to connect to (first IPv4 if available, else IPv6).
    pub addr_port: String,
    /// Server-advertised protocol/version string (from TXT `version=`).
    pub version: Option<String>,
}

impl DiscoveredServer {
    /// Display label for a dropdown row. Prefers friendly_name, falls back
    /// to instance, always includes the addr:port so two same-named servers
    /// on different subnets are distinguishable.
    pub fn display_label(&self) -> String {
        let name = self.friendly_name.as_deref().unwrap_or(&self.instance);
        format!("{} ({})", name, self.addr_port)
    }
}

/// Handle to a running browse task. Dropping the handle stops the browse
/// (the daemon-thread observes the Arc strong-count via the
/// `running` Arc<AtomicBool> wrapper).
pub struct BrowseHandle {
    pub results: Arc<Mutex<Vec<DiscoveredServer>>>,
    _daemon: Option<ServiceDaemon>, // kept alive so the browse keeps running
}

impl BrowseHandle {
    /// Spawn a background browse for `_thetislink._udp.local.`. Returns a
    /// handle holding the daemon + the shared result list. UI thread reads
    /// `results` each frame; never blocks on it.
    pub fn start() -> Self {
        let results: Arc<Mutex<Vec<DiscoveredServer>>> = Arc::new(Mutex::new(Vec::new()));
        let daemon = match ServiceDaemon::new() {
            Ok(d) => d,
            Err(e) => {
                warn!("mDNS browse: daemon init failed ({}); discovery disabled", e);
                return Self { results, _daemon: None };
            }
        };
        let receiver = match daemon.browse(SERVICE_TYPE) {
            Ok(rx) => rx,
            Err(e) => {
                warn!("mDNS browse: subscribe failed ({}); discovery disabled", e);
                return Self { results, _daemon: None };
            }
        };
        info!("mDNS browse started for {}", SERVICE_TYPE);
        let results_thread = results.clone();
        thread::Builder::new()
            .name("thetislink-mdns-browse".to_string())
            .spawn(move || {
                while let Ok(event) = receiver.recv() {
                    match event {
                        ServiceEvent::ServiceResolved(info) => {
                            // Pick the first non-loopback IP, preferring IPv4.
                            let addrs = info.get_addresses();
                            let v4 = addrs.iter().find(|a| a.is_ipv4() && !a.is_loopback());
                            let v6 = addrs.iter().find(|a| a.is_ipv6() && !a.is_loopback());
                            let ip = v4.or(v6).map(|s| s.to_ip_addr());
                            let Some(ip) = ip else { continue };
                            let addr_port = format!("{}:{}", ip, info.get_port());
                            // Pull TXT records into a HashMap for friendly_name / version
                            let mut txt: HashMap<String, String> = HashMap::new();
                            for prop in info.get_properties().iter() {
                                txt.insert(
                                    prop.key().to_lowercase(),
                                    prop.val_str().to_string(),
                                );
                            }
                            let instance = info
                                .get_fullname()
                                .split('.')
                                .next()
                                .unwrap_or("")
                                .to_string();
                            let entry = DiscoveredServer {
                                instance: instance.clone(),
                                friendly_name: txt.get("name").cloned(),
                                addr_port,
                                version: txt.get("version").cloned(),
                            };
                            debug!("mDNS: resolved {:?}", entry);
                            let mut list = results_thread.lock().unwrap();
                            // Replace any prior entry with the same fullname.
                            if let Some(slot) =
                                list.iter_mut().find(|d| d.instance == entry.instance)
                            {
                                *slot = entry;
                            } else {
                                list.push(entry);
                            }
                        }
                        ServiceEvent::ServiceRemoved(_type, fullname) => {
                            let instance =
                                fullname.split('.').next().unwrap_or("").to_string();
                            debug!("mDNS: removed {}", instance);
                            let mut list = results_thread.lock().unwrap();
                            list.retain(|d| d.instance != instance);
                        }
                        _ => {}
                    }
                }
                info!("mDNS browse thread exited");
            })
            .ok();
        Self {
            results,
            _daemon: Some(daemon),
        }
    }

    /// Snapshot of currently-known servers, sorted by display label so the
    /// UI dropdown is stable across frames.
    pub fn snapshot(&self) -> Vec<DiscoveredServer> {
        let mut list = self.results.lock().map(|g| g.clone()).unwrap_or_default();
        list.sort_by(|a, b| a.display_label().cmp(&b.display_label()));
        list
    }

}
