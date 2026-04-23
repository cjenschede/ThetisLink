// SPDX-License-Identifier: GPL-2.0-or-later

//! Coverage-registratie voor render-path vergelijking tegen de CI-gate
//! expected set.
//!
//! Elke render-helper registreert bij zijn eerste aanroep de combinatie
//! `(control, surface, channel, density, guarded)` die hij rendert. Na opstart
//! dumpt de app optioneel `target/ui-coverage.json` voor vergelijking tegen
//! `scripts/ui-coverage-expected.json` via CI-gate (`scripts/check-ui-coverage.sh`).
//!
//! **Kosten per call:** `register()` pakt een `Mutex`-lock en doet
//! `BTreeSet::insert` (O(log n) met dedup). Bij typische render-cadens
//! (60 fps × ~10 helpers) is dit in praktijk verwaarloosbaar, maar wel
//! géén zero-cost — profileer als het hot-path zichtbaar gaat kosten.

use std::collections::BTreeSet;
use std::sync::Mutex;

use super::{RxChannel, UiDensity, UiSurface};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct CoverageEntry {
    pub(crate) control: &'static str,
    pub(crate) surface: &'static str,
    pub(crate) channel: &'static str,
    pub(crate) density: &'static str,
    /// `true` als deze control-site een `add_enabled` / `dispatch`-guard op
    /// `connected` heeft; `false` voor by-design ongeguarded controls (bv.
    /// step-size selectie die offline werkt).
    pub(crate) guarded: bool,
}

static REGISTRY: Mutex<Option<BTreeSet<CoverageEntry>>> = Mutex::new(None);

/// Registreer dat `control` gerenderd is in de gegeven context.
///
/// Idempotent: dezelfde combinatie wordt maar één keer vastgelegd. Een site
/// die zowel `guarded=true` als `guarded=false` registreert produceert twee
/// rijen — dat is opzettelijk (anders zou een inconsistente helper onzichtbaar
/// blijven).
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

/// Exporteer de huidige coverage als JSON-array. `BTreeSet`-iteratie levert
/// deterministische volgorde zodat `jq -S . ui-coverage.json | diff ...`
/// stabiele output geeft.
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

/// In debug of onder `feature = "ui-coverage"`: schrijf coverage naar
/// `target/ui-coverage.json`. In release zonder feature: no-op.
pub(crate) fn dump_if_enabled() {
    #[cfg(any(debug_assertions, feature = "ui-coverage"))]
    {
        let json = export_json();
        if let Err(e) = std::fs::write("target/ui-coverage.json", json) {
            log::warn!("failed to write ui-coverage.json: {e}");
        }
    }
}
