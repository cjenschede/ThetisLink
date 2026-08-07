// SPDX-License-Identifier: GPL-2.0-or-later

//! Coverage registration for comparing the render path against the CI-gate
//! expected set.
//!
//! On its first call, each render helper registers the combination
//! `(control, surface, channel, density, guarded)` that it renders. After startup
//! the app optionally dumps `target/ui-coverage.json` for comparison against
//! `scripts/ui-coverage-expected.json` via the CI gate (`scripts/check-ui-coverage.sh`).
//!
//! **Cost per call:** `register()` takes a `Mutex` lock and does a
//! `BTreeSet::insert` (O(log n) with dedup). At a typical render cadence
//! (60 fps × ~10 helpers) this is negligible in practice, but it is
//! not zero-cost - profile if it starts to show up on the hot path.

use std::collections::BTreeSet;
use std::sync::Mutex;

use super::{RxChannel, UiDensity, UiSurface};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct CoverageEntry {
    pub(crate) control: &'static str,
    pub(crate) surface: &'static str,
    pub(crate) channel: &'static str,
    pub(crate) density: &'static str,
    /// `true` if this control site has an `add_enabled` / `dispatch` guard on
    /// `connected`; `false` for by-design unguarded controls (e.g.
    /// step-size selection that works offline).
    pub(crate) guarded: bool,
}

static REGISTRY: Mutex<Option<BTreeSet<CoverageEntry>>> = Mutex::new(None);

/// Register that `control` was rendered in the given context.
///
/// Idempotent: the same combination is recorded only once. A site
/// that registers both `guarded=true` and `guarded=false` produces two
/// rows - that is intentional (otherwise an inconsistent helper would remain
/// invisible).
pub(crate) fn register(
    control: &'static str,
    surface: UiSurface,
    channel: RxChannel,
    density: UiDensity,
    guarded: bool,
) {
    let entry = CoverageEntry {
        control,
        surface: surface.as_str(),
        channel: channel.as_str(),
        density: density.as_str(),
        guarded,
    };
    let mut guard = REGISTRY.lock().unwrap();
    let set = guard.get_or_insert_with(BTreeSet::new);
    set.insert(entry);
}

/// Export the current coverage as a JSON array. `BTreeSet` iteration yields
/// deterministic ordering so that `jq -S . ui-coverage.json | diff ...`
/// gives stable output.
pub(crate) fn export_json() -> String {
    let guard = REGISTRY.lock().unwrap();
    let empty = BTreeSet::new();
    let set = guard.as_ref().unwrap_or(&empty);
    let mut out = String::from("[\n");
    let mut first = true;
    for entry in set {
        if !first {
            out.push_str(",\n");
        }
        first = false;
        out.push_str(&format!(
            "  {{ \"control\": \"{}\", \"surface\": \"{}\", \"channel\": \"{}\", \"density\": \"{}\", \"guarded\": {} }}",
            entry.control, entry.surface, entry.channel, entry.density, entry.guarded
        ));
    }
    out.push_str("\n]\n");
    out
}

/// In debug or under `feature = "ui-coverage"`: write coverage to
/// `target/ui-coverage.json`. In release without the feature: no-op.
pub(crate) fn dump_if_enabled() {
    #[cfg(any(debug_assertions, feature = "ui-coverage"))]
    {
        let json = export_json();
        if let Err(e) = std::fs::write("target/ui-coverage.json", json) {
            log::warn!("failed to write ui-coverage.json: {e}");
        }
    }
}
