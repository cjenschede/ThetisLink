// SPDX-License-Identifier: GPL-2.0-or-later

mod bridge;
mod logging;

#[cfg(target_os = "android")]
mod audio_oboe;

pub use logging::{init_logging, log_line, log_tail};

pub use bridge::{
    ble_drop_policy, filter_step, filter_width_label, ptt_button, relay_is_configured, version,
    BleDropPolicy, BlePttAction, BridgeFilterEdges,
    BlePttGate, PhonePtt, PttButton, PttSource, PttTarget, BridgeChatAnswer, BridgeChatMessage, BridgeChatState, BridgeDxSpot, BridgeRadioState, BridgeRogerBeep,
    SdrBridge,
};

uniffi::include_scaffolding!("sdr_remote");

/// Guards on the Kotlin that this crate is the engine for.
///
/// There is no Kotlin test harness in this project and there is not going to be
/// one soon, so the app's side of the bridge has never had anything watching it.
/// These tests read the Kotlin as text. That is crude, and it is still the only
/// mechanism available - and on 2026-09-14 the absence of one cost a defect that
/// was introduced while fixing its twin: a second reader of the EQ preferences
/// copied the key and left off the migration fallback, so the engine was sent
/// five zeros while the sliders showed the operator his real curve.
#[cfg(test)]
mod kotlin_guards {
    use std::path::{Path, PathBuf};

    const KOTLIN_ROOT: &str = "android/app/src/main/java/com/sdrremote";

    fn read(rel: &str) -> String {
        std::fs::read_to_string(Path::new(KOTLIN_ROOT).join(rel))
            .unwrap_or_else(|e| panic!("{KOTLIN_ROOT}/{rel}: {e}"))
    }

    /// Every .kt file under the app package, so a gate cannot be walked around by
    /// moving the line to a file the gate never heard of.
    fn all_kotlin() -> Vec<PathBuf> {
        fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
            for entry in std::fs::read_dir(dir).expect("kotlin source tree") {
                let p = entry.expect("dir entry").path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().map_or(false, |e| e == "kt") {
                    out.push(p);
                }
            }
        }
        let mut out = Vec::new();
        walk(Path::new(KOTLIN_ROOT), &mut out);
        out
    }

    /// Drop Kotlin comments.
    ///
    /// The files are read as text, comments and all, so a rule that was deleted
    /// from the code but left in a comment satisfied a `contains` check. A
    /// reviewer named that bypass and it was real: the fallback survived only as
    /// prose and the gate stayed green.
    ///
    /// Crude by design - a `//` inside a string literal takes the rest of that
    /// line with it. That makes the gate stricter, never looser, so the failure
    /// mode is a test that complains rather than one that misses something.
    fn strip_comments(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let mut chars = s.chars().peekable();
        let mut in_block = false;
        while let Some(c) = chars.next() {
            if in_block {
                if c == '*' && chars.peek() == Some(&'/') {
                    chars.next();
                    in_block = false;
                }
                continue;
            }
            if c == '/' {
                match chars.peek() {
                    Some('/') => {
                        for n in chars.by_ref() {
                            if n == '\n' {
                                out.push('\n');
                                break;
                            }
                        }
                        continue;
                    }
                    Some('*') => {
                        chars.next();
                        in_block = true;
                        continue;
                    }
                    _ => {}
                }
            }
            out.push(c);
        }
        out
    }

    /// Collapse every run of whitespace to one space.
    ///
    /// A gate that matches source text including its indentation is defeated by a
    /// formatter, by a line break in a different place, or by CRLF - none of which
    /// change what the code does. A reviewer walked around all four of the first
    /// version's gates, three of them that way.
    fn squeeze(s: &str) -> String {
        s.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    /// The per-radio preference keys are read in ONE file.
    ///
    /// This is the inverted form, and the reason is that the first version asked
    /// "does the ViewModel read this key badly", which a second reader in a third
    /// file walks straight past - and a second reader rarely appears in the file
    /// where the first one already lives. The shape is borrowed from
    /// check-text-hygiene.ps1, which has carried a self-attribution exception for
    /// the same reason for months.
    #[test]
    fn the_per_radio_preference_keys_are_read_in_one_file() {
        // The one deliberate exception, with its reason in the code beside it: the
        // screen's 500 ms poll wants THIS slot and no fallback, because it is
        // watching for a change the ViewModel just wrote.
        const ALLOWED: &[(&str, &str)] = &[
            // Same text, different path - the trap working rule 16 is about.
            // MidiAction.TxGain carries "tx_gain" as the identifier of a MIDI
            // mapping, stored under its own prefix. It is not the volume
            // preference and must not be routed through LocalAudioPrefs.
            ("service/MidiController.kt", "TxGain(\"TX Gain\", \"tx_gain\")"),
            (
            "ui/screens/ExternalDevicesScreen.kt",
            "eqPrefs.getBoolean(\"eq_enabled_$eqRadio\", eqEnabled)",
        )];
        let keys = [
            // the per-radio TX chain (YaesuTxPrefs)
            "eq_band_", "eq_enabled_", "yaesu_tx_agc_", "yaesu_comp_", "yaesu_mic_gain_",
            // the local audio settings of the startup block (LocalAudioPrefs)
            "rx_volume", "local_volume", "tx_gain", "agc_enabled", "yaesu_volume",
        ];
        for path in all_kotlin() {
            let rel = path
                .strip_prefix(KOTLIN_ROOT)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if rel.starts_with("prefs/") {
                continue; // the helpers ARE the source
            }
            let squeezed = squeeze(&strip_comments(&read(&rel)));
            for key in keys {
                for (i, _) in squeezed.match_indices(key) {
                    let start = i.saturating_sub(60);
                    let before = &squeezed[start..i];
                    // Reads AND writes. The first version covered only the reads, and
                    // a reviewer put it plainly: a source that covers one direction is
                    // not the source. A writer that spells the key out can use one the
                    // reader has never heard of, and the two drift with nothing to say
                    // so.
                    let touches_prefs = ["getFloat", "getBoolean", "getInt", "getString",
                                         "putFloat", "putBoolean", "putInt", "putString"]
                        .iter()
                        .any(|f| before.contains(f));
                    if !touches_prefs {
                        continue;
                    }
                    let window = &squeezed[start..(i + 60).min(squeezed.len())];
                    let excused = ALLOWED
                        .iter()
                        .any(|(f, snippet)| *f == rel && window.contains(&squeeze(snippet)));
                    assert!(
                        excused,
                        "{rel} reads {key} directly. Read it through YaesuTxPrefs, which carries \
                         the fallback on the pre-slot key; a raw read hands the engine a default \
                         while the sliders show the operator his real value. Context: ...{window}..."
                    );
                }
            }
        }
    }

    /// The fallbacks in that one file still exist, as code.
    #[test]
    fn the_helper_still_falls_back_to_the_pre_slot_keys() {
        let prefs = squeeze(&strip_comments(&read("prefs/YaesuTxPrefs.kt")));
        for (slot_key, shared_key) in [
            ("eq_band_${radio}_$index", "eq_band_$index"),
            ("eq_enabled_$radio", "eq_enabled"),
            ("yaesu_tx_agc_$radio", "yaesu_tx_agc"),
            ("yaesu_comp_$radio", "yaesu_comp"),
        ] {
            assert!(prefs.contains(slot_key), "YaesuTxPrefs lost the per-radio key {slot_key}");
            // A live ARGUMENT, not a leftover variable and not a line of prose: the
            // file is read as text, comments and all, and a reviewer pointed out
            // that a fallback surviving only in a comment would have kept the
            // first version of this test green.
            let live = prefs.contains(&format!("prefs.getFloat(\"{shared_key}\""))
                || prefs.contains(&format!("prefs.getBoolean(\"{shared_key}\""));
            assert!(
                live,
                "YaesuTxPrefs no longer falls back to {shared_key} in code, so a value set before \
                 the per-radio keys existed is silently replaced by the default"
            );
        }
    }

    /// The saved local settings reach the engine at APP START, not at a connect.
    ///
    /// Task A, open since 2026-09-12. The four are engine-local and the engine
    /// outlives every connection, so once is enough - but that once used to be
    /// three places, none of which was the start of the app, and the connect
    /// button in the wizard sent none of them at all. Whoever set the app up for
    /// the first time ran on the engine's defaults.
    ///
    /// Wired from `init` and NOT from the connect button, which is the whole
    /// distinction: a local setting is not a subscription.
    #[test]
    fn the_saved_local_settings_are_sent_at_app_start() {
        let vm = squeeze(&strip_comments(&read("viewmodel/SdrViewModel.kt")));
        // The call has to be IN the init block, and that is the whole claim.
        //
        // Counting to two only says "defined, and mentioned once somewhere". A
        // reviewer showed what that leaves open: move the call into a function that
        // runs on connect, or into a private fun nobody calls, and the count is
        // still two. Task A would be entirely back - wizard included - with this
        // test green. So the source is sliced to the init block first.
        let init_start = vm.find("init {").expect("SdrViewModel has no init block");
        let init_len = vm[init_start..]
            .find("startPolling()")
            .expect("init no longer starts the polling loop");
        let init_block = &vm[init_start..init_start + init_len];
        assert!(
            init_block.contains("applyLocalSettingsAtStartup()"),
            "applyLocalSettingsAtStartup is not called from init. Anywhere else means the saved              volumes, mic gain and TX AGC arrive on a connect - or, from the wizard, never, which              is the defect this patch exists for"
        );
        assert_eq!(
            vm.matches("applyLocalSettingsAtStartup()").count(),
            2,
            "expected the definition and exactly one call; a second caller is a second trigger"
        );
        for setter in ["setRxVolume(rx)", "setLocalVolume(local)", "setTxGain(tx)", "setAgcEnabled(agc)"] {
            assert!(
                vm.contains(setter),
                "the startup block no longer sends {setter}; a saved setting that only takes                  effect after you touch it is not a saved setting"
            );
        }
        // And not back on the connect button. Sliced to that lambda: the sliders
        // legitimately call these setters elsewhere, so a blanket ban would be
        // wrong - and the first version checked ONE of the four and bound itself to
        // the argument name, which a rename would have walked past.
        let main = squeeze(&strip_comments(&read("ui/screens/MainScreen.kt")));
        //
        // EVERY onConnect handler, not the first one found. There are two - the
        // wizard's and the main screen's - and slicing to the first checked the
        // wizard, which never pushed these anyway. A mutation that put a setter
        // back on the main screen's button stayed green, and that is the same
        // "matched the first hit" mistake working rule 16 is about, made inside
        // the test written to catch it.
        let handlers: Vec<usize> = main.match_indices("onConnect = {").map(|(i, _)| i).collect();
        assert_eq!(handlers.len(), 2, "expected the wizard's connect handler and the main screen's");
        for start in handlers {
            let len = main[start..]
                .find("onDisconnect")
                .expect("an onConnect handler is no longer followed by onDisconnect");
            let block = &main[start..start + len];
            for setter in ["setRxVolume(", "setLocalVolume(", "setTxGain(", "setAgcEnabled("] {
                assert!(
                    !block.contains(setter),
                    "a connect handler pushes {setter} again: a second trigger for something the                      engine keeps anyway. If it is needed there, the startup block is not doing                      its job"
                );
            }
        }
    }

    /// The EQ switch and the curve leave together, in that order, unconditionally.
    ///
    /// Pinned as a SHAPE and not as two positions. Position alone is walked around
    /// by wrapping the band loop in a condition of its own - the text order does
    /// not change, and that is exactly the defect this was written for.
    #[test]
    fn the_eq_switch_never_leaves_without_its_curve() {
        let vm = squeeze(&strip_comments(&read("viewmodel/SdrViewModel.kt")));
        let required = squeeze(
            "if (enableEq) {
                for (i in 0..4) yaesuEqBandSel(i, bands[i])
            }
            yaesuEqEnabledSel(enableEq)",
        );
        assert!(
            vm.contains(&required),
            "SdrViewModel.kt no longer sends the bands and then the switch as one block. Sending \
             the switch without the bands turns the EQ on over whatever curve the engine happened \
             to hold; sending them after leaves a window with the wrong one. Any other condition \
             around the bands is the original defect in a new spelling."
        );
        assert_eq!(
            vm.matches("yaesuEqEnabledSel(").count(),
            2,
            "expected exactly two mentions in the ViewModel: the definition and the one call in \
             that block"
        );
    }

    /// The AGC is pushed from the state stream, and from nowhere else but its own
    /// checkbox.
    #[test]
    fn the_agc_is_pushed_from_the_state_stream_and_not_from_the_screen() {
        let vm = squeeze(&strip_comments(&read("viewmodel/SdrViewModel.kt")));
        assert!(
            vm.contains("yaesuTxAgc(txAgc)"),
            "the TX chain no longer pushes the AGC, so it is back to screen-only - and with the \
             screen closed the engine never hears about a switch that sits in the transmit path"
        );
        let screen = squeeze(&strip_comments(&read("ui/screens/ExternalDevicesScreen.kt")));
        assert!(
            !screen.contains(&squeeze("LaunchedEffect(eqRadio) { onTxAgc(txAgc) }")),
            "the screen pushes the AGC again beside the state stream: two pushers on one trigger"
        );
    }
    /// The filter width buttons ask the shared rule, and no Kotlin table of steps
    /// comes back.
    ///
    /// Until build 47 Compose had its own copy of the rule - steps, edges, nearest
    /// step - beside the one in the desktop helpers. Report 74 locked up in that
    /// copy. The rule now lives in `sdr_remote_logic::filter_width`; this watches
    /// the one place that could quietly grow a second one again, across every
    /// file, since a copy would not necessarily land where the old one was.
    #[test]
    fn the_filter_width_buttons_ask_the_shared_rule() {
        let controls = squeeze(&strip_comments(&read("ui/components/RadioControls.kt")));
        assert!(
            controls.contains("filterStep(") && controls.contains("filterWidthLabel("),
            "the filter width buttons no longer ask the bridge for the step and the label"
        );
        for path in all_kotlin() {
            let code = strip_comments(&std::fs::read_to_string(&path).expect("kotlin file"));
            // Whitespace out entirely, not squeezed: `intArrayOf(<newline>1800` and
            // `listOf(1800` both walked past a check on `intArrayOf(1800`, and the
            // ladder itself is the thing that must not come back (review finding, 2026-09-16).
            let dense: String = code.split_whitespace().collect();
            for sign in [
                "1800,2100,2400",
                "calcFilterEdges",
                "closestPresetIndex",
                // The label too: its Kotlin version formatted with the phone's
                // locale and wrote "2,7 kHz" in Dutch.
                "formatBandwidth",
            ] {
                assert!(
                    !dense.contains(sign),
                    "{}: `{}` - a Kotlin copy of the filter width rule is back; use filterStep",
                    path.display(),
                    sign
                );
            }
        }
    }
}
