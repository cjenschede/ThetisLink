// SPDX-License-Identifier: GPL-2.0-or-later

mod bridge;
mod logging;

#[cfg(target_os = "android")]
mod audio_oboe;

pub use logging::{developer_code_matches, init_logging, log_line, log_tail, set_developer_mode};

pub use bridge::{
    ble_drop_policy, expected_route_label, filter_step, filter_width_label, ptt_button, relay_is_configured, s_meter_fraction, s_meter_text, version, yaesu_s_meter_dbm,
    tx_gain_db, tx_gain_db_max, tx_gain_db_min, tx_gain_factor, tx_gain_from_fraction, tx_gain_off_db, yaesu_mic_gain_db, yaesu_mic_gain_from_db,
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
    /// from the code but left in a comment satisfied a `contains` check. That
    /// bypass was real: the fallback survived only as
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
    /// change what the code does. All four of the first version's
    /// gates could be walked around, three of them that way.
    fn squeeze(s: &str) -> String {
        s.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    /// The per-radio preference keys are read in ONE file.
    ///
    /// This is the inverted form, and the reason is that the first version asked
    /// "does the ViewModel read this key badly", which a second reader in a third
    /// file walks straight past - and a second reader rarely appears in the file
    /// where the first one already lives.
    /// The developer mode on the phone (build 298): read at start, after the log
    /// opens and before the screen; switched in one place that also ends what
    /// only the mode starts; and its tools - with the one request for location -
    /// drawn only in it. The desktop's guards, for the app.
    #[test]
    fn the_developer_mode_is_read_at_start_and_gates_its_tools() {
        let act = squeeze(&strip_comments(&read("MainActivity.kt")));
        let set = act
            .find("uniffi.sdr_remote.setDeveloperMode(com.sdrremote.prefs.DeveloperMode.saved(this))")
            .expect("the saved mode at start");
        assert!(act.find("uniffi.sdr_remote.initLogging(").unwrap() < set, "after the log opens");
        assert!(set < act.find("setContent {").unwrap(), "before the screen");

        let vm = squeeze(&strip_comments(&read("viewmodel/SdrViewModel.kt")));
        let switch = &vm[vm.find("fun switchDeveloperMode(on: Boolean) {").expect("the switch")..];
        let switch = &switch[..switch.find(" fun ").unwrap()];
        for step in [
            "com.sdrremote.prefs.DeveloperMode.save(getApplication(), on)",
            "uniffi.sdr_remote.setDeveloperMode(on)",
            "if (!on) {",
            "if (st.measurementFile != null) setMeasurement(false)",
            "if (st.relayOnly) setRelayOnly(false)",
            "if (st.forceTcp) setForceTcp(false)",
            "_developerMode.value = on",
        ] {
            assert_eq!(switch.matches(step).count(), 1, "{step}");
        }

        // One key for the reader at start and the switch.
        let prefs = squeeze(&strip_comments(&read("prefs/DeveloperMode.kt")));
        assert_eq!(prefs.matches("getBoolean(KEY, false)").count(), 1);
        assert_eq!(prefs.matches("putBoolean(KEY, on)").count(), 1);

        // The two doors: the code in, the button out (K4, K6, K11).
        let dialog = squeeze(&strip_comments(&read("ui/components/SettingsDialog.kt")));
        for door in [
            "if (uniffi.sdr_remote.developerCodeMatches(typed)) { devCode = \"\" onDeveloperMode(true) }",
            "TextButton(onClick = { onDeveloperMode(false) })",
            "if (developerMode) { Row(verticalAlignment = Alignment.CenterVertically) { Text(\"DEV\",",
        ] {
            assert_eq!(dialog.matches(door).count(), 1, "{door}");
        }

        let screen = squeeze(&strip_comments(&read("ui/screens/MainScreen.kt")));
        // Where the screen's flag comes from: the ViewModel, and nowhere else (K2).
        assert_eq!(screen.matches("val developerMode by viewModel.developerMode.collectAsStateWithLifecycle()").count(), 1);
        assert_eq!(screen.matches("val developerMode").count(), 1);
        assert!(screen.contains("if (developerMode) { Text( \"DEV\","), "the sign in About follows the mode (K5)");
        assert!(screen.contains("if (developerMode && lastKeyEvent.isNotEmpty()) {"), "the last-key line, in developer mode only");
        assert!(screen.contains("val devMeasurement: ((Boolean) -> Unit)? = if (developerMode) onMeasurement else null"));
        assert!(screen.contains("val devRelayOnly: ((Boolean) -> Unit)? = if (developerMode) {"));
        assert!(screen.contains("val devForceTcp: ((Boolean) -> Unit)? = if (developerMode) {"));
        for (arg, n) in [
            ("onMeasurement = ", 2),
            ("onMeasurement = devMeasurement,", 2),
            ("onRelayOnly = ", 2),
            ("onRelayOnly = devRelayOnly,", 2),
            ("onForceTcp = ", 2),
            ("onForceTcp = devForceTcp,", 2),
        ] {
            assert_eq!(screen.matches(arg).count(), n, "{arg}");
        }

        // Location is asked for in one place - the measurement's switch - and
        // the mode is switched in two: the start and the ViewModel.
        let (mut asks, mut switches) = (0, 0);
        for f in all_kotlin() {
            let src = strip_comments(&std::fs::read_to_string(&f).unwrap());
            asks += src.matches("requestLocationPermission {").count();
            switches += src.matches("setDeveloperMode(").count();
        }
        assert_eq!((asks, switches), (1, 2));
    }

    #[test]
    fn the_per_radio_preference_keys_are_read_in_one_file() {
        // The one deliberate exception, with its reason in the code beside it: the
        // screen's 500 ms poll wants THIS slot and no fallback, because it is
        // watching for a change the ViewModel just wrote.
        const ALLOWED: &[(&str, &str)] = &[
            // Same text, different path - a match on the name, not on the path.
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
                    // Reads AND writes. The first version covered only the reads, but
                    // a source that covers one direction is
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
            // file is read as text, comments and all, and a fallback surviving only in a comment would have kept the
            // first version of this test green.
            let live = prefs.contains(&format!("prefs.getFloat(\"{shared_key}\""))
                || prefs.contains(&format!("prefs.getBoolean(\"{shared_key}\""));
            assert!(
                live,
                "YaesuTxPrefs no longer falls back to {shared_key} in code, so a value set before \
                 the per-radio keys existed is silently replaced by the default"
            );
        }
        // The keys without a fallback, and the names of the preset key and the store:
        // renamed, every phone loses that setting at once, silently.
        for key in ["private fun micGainKey(radio: Int) = \"yaesu_mic_gain_$radio\"",
                    "if (headset) \"eq_preset_bt_$radio\" else \"eq_preset_mic_$radio\"",
                    "const val PRESETS = \"eq_presets\"", "const val STORE = \"thetislink_eq\""] {
            assert!(prefs.contains(&squeeze(key)), "{key}");
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
        // Counting to two only says "defined, and mentioned once somewhere". That
        // leaves this open: move the call into a function that
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
            "applyLocalSettingsAtStartup is not called from init. Anywhere else means the saved              volumes, mic gain and TX AGC arrive on a connect: or, from the wizard, never, which              is the defect this patch exists for"
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
        // "matched the first hit" mistake, made inside
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
    /// The TX gain sliders (Thetis and the Yaesu mic) and the knob take their dB
    /// scale from the Rust core, and the old Kotlin scales do not come back.
    #[test]
    fn the_gain_sliders_use_the_core_scale() {
        let stats = squeeze(&strip_comments(&read("ui/components/StatsPanel.kt")));
        assert!(stats.contains(&squeeze("uniffi.sdr_remote.txGainDb(txGain)")));
        assert!(stats.contains(&squeeze("uniffi.sdr_remote.txGainFactor(it)")));
        assert!(!stats.contains(&squeeze("maxValue = 3f")), "the 0-300% Thetis slider is back");
        // One label for both sliders, rounded and not cut off (a dB from a factor
        // can sit a float step under its whole number).
        assert!(stats.contains(&squeeze("val whole = kotlin.math.round(db).toInt()")));
        assert!(stats.contains(&squeeze("if (whole >= 0) \"+\" else \"\"")), "+0 dB, as on the desktop");
        assert!(stats.contains(&squeeze("mutableFloatStateOf(kotlin.math.round(initialDb))")), "starts on a whole dB");
        assert!(stats.contains(&squeeze("text = gainDbText(db),")));
        assert!(stats.contains(&squeeze("fun gainDbText(db: Float): String = gainDbLabel(db) ?: stringResource(R.string.gain_off)")),
            "the label and the screen's log line from one function");
        assert!(stats.contains(&squeeze("if (db < uniffi.sdr_remote.txGainDbMin()) return null")), "off below the bottom, not at it");
        let devices = squeeze(&strip_comments(&read("ui/screens/ExternalDevicesScreen.kt")));
        assert!(devices.contains(&squeeze("uniffi.sdr_remote.yaesuMicGainFromDb(localMicDb)")));
        assert!(!devices.contains(&squeeze("(v * 0.4f)")), "the old 0.05x-1x mic scale is back");
        assert!(devices.contains(&squeeze("com.sdrremote.ui.components.gainDbText(localMicDb),")));
        assert!(devices.contains(&squeeze("mutableFloatStateOf(kotlin.math.round(uniffi.sdr_remote.yaesuMicGainDb(")), "starts on a whole dB");
        let main = squeeze(&strip_comments(&read("ui/screens/MainScreen.kt")));
        assert!(main.contains(&squeeze("uniffi.sdr_remote.txGainFromFraction(frac)")));
        assert!(!main.contains(&squeeze("frac * 3f")), "the knob has its own scale again");
    }

    /// A preset carries the TX chain (mic gain, compressor, AGC) behind its five
    /// bands, in one format in `YaesuTxPrefs`; the ViewModel applies it on a
    /// headset change, a radio change and a choice in the list, and the screen
    /// takes the result from `yaesuTxLoad` for the radio it shows. Until build 38
    /// the screen polled `eq_preset_pending` from an effect that kept the radio it
    /// was first composed with. And the poll of the state writes through
    /// `update`, so a radio chosen between its read and its write is kept.
    #[test]
    fn a_preset_carries_the_chain_and_the_screen_follows_the_load() {
        for path in all_kotlin() {
            let code = strip_comments(&std::fs::read_to_string(&path).expect("kotlin file"));
            assert!(!code.contains("eq_preset_pending"), "the old poll is back in {}", path.display());
            if !path.ends_with("YaesuTxPrefs.kt") {
                assert!(!code.contains("\"eq_presets\""), "the preset store spelled out in {}", path.display());
                assert!(!code.contains("\"thetislink_eq\""), "the store name spelled out in {}", path.display());
            }
        }
        // Every store is opened by a constant: a typed name opens an empty store. The one
        // literal left is the old "sdr_remote" store of the frequency display.
        let mut literal = 0;
        for path in all_kotlin() {
            let code = strip_comments(&std::fs::read_to_string(&path).expect("kotlin file"));
            literal += code.matches("getSharedPreferences(\"").count();
            if !path.ends_with("LocalAudioPrefs.kt") {
                assert!(!code.contains("\"thetislink\""), "the main store name spelled out in {}", path.display());
            }
        }
        assert_eq!(literal, 1, "a store opened by a typed name");
        assert!(strip_comments(&read("ui/components/FrequencyDisplay.kt")).contains("getSharedPreferences(\"sdr_remote\","),
            "the one typed name is the old store of the frequency display");
        assert!(squeeze(&strip_comments(&read("prefs/LocalAudioPrefs.kt"))).contains("const val STORE = \"thetislink\""));
        // Every reader of the store opens it by that name; a typed one opens an empty store.
        let opened: usize = all_kotlin().iter()
            .map(|p| strip_comments(&std::fs::read_to_string(p).expect("kotlin file")).matches("getSharedPreferences(YaesuTxPrefs.STORE,").count())
            .sum();
        assert_eq!(opened, 5, "the screen twice, the ViewModel three times");
        let prefs = squeeze(&strip_comments(&read("prefs/YaesuTxPrefs.kt")));
        assert!(prefs.contains(&squeeze("val chain = if (arr.length() >= 8) {")));
        // The order of the three on every hop: read, stored.
        // Named, so the order of Chain's declaration does not take part.
        assert!(prefs.contains(&squeeze("Chain(micGain = arr.getDouble(5).toFloat(), compressor = arr.getDouble(6).toFloat(),
            agc = arr.getDouble(7) != 0.0)")));
        for put in ["e.putFloat(micGainKey(radio), it.micGain)", "e.putFloat(compressorKey(radio), it.compressor)",
                    "e.putBoolean(txAgcKey(radio), it.agc)"] {
            assert!(prefs.contains(&squeeze(put)), "{put}");
        }
        assert!(prefs.contains(&squeeze("val arr = org.json.JSONArray()
            for (g in bands(prefs, radio)) arr.put(g.toDouble())
            arr.put(micGain(prefs, radio).toDouble())
            arr.put(compressor(prefs, radio).toDouble())
            arr.put(if (txAgc(prefs, radio)) 1 else 0)
            json.put(name, arr)
            prefs.edit().putString(PRESETS, json.toString()).apply()")), "the order a preset is written in, and that it is written");
        // Five bands on both sides: a sixth written moves the chain one place on.
        assert!(prefs.contains(&squeeze("FloatArray(5) { i -> band(prefs, radio, i) }")));
        assert!(prefs.contains(&squeeze("val bands = FloatArray(5) { i -> arr.getDouble(i).toFloat() }")));
        assert!(prefs.contains(&squeeze("json.remove(name)
            prefs.edit().putString(PRESETS, json.toString()).apply()")), "a delete is written back");
        // An unreadable store is left alone on Save and Delete.
        assert_eq!(prefs.matches(&squeeze("catch (_: Exception) { return presetNames(prefs) }")).count(), 2);
        // Wide in all four: a value of the wrong type under the key throws a
        // ClassCastException, and from a choice or a composition that is a crash.
        assert_eq!(prefs.matches("catch (_: Exception)").count(), 4);
        let vm = squeeze(&strip_comments(&read("viewmodel/SdrViewModel.kt")));
        assert!(vm.contains(&squeeze("if (preset != null) YaesuTxPrefs.applyPreset(prefs, radio, preset)")));
        assert!(vm.contains(&squeeze("if (preset?.chain != null) sendTxChain(radio, prefs)")));
        assert_eq!(vm.matches(&squeeze("publishYaesuTxLoad(radio,")).count(), 2, "the loader and the choice");
        assert!(vm.contains(&squeeze("seq = yaesuTxLoadSeq.incrementAndGet()")), "a second load of the same preset is a new event");
        // One sender of the chain, for the radio switch, the loader and the choice,
        // each value from its own key (the compressor sent as the gain is +30 dB).
        assert_eq!(vm.matches(&squeeze("sendTxChain(radio, prefs)")).count(), 3);
        for line in ["val micGain = YaesuTxPrefs.micGain(prefs, radio)", "val comp = YaesuTxPrefs.compressor(prefs, radio)",
                     "val txAgc = YaesuTxPrefs.txAgc(prefs, radio)", "yaesuTxGainSel(micGain)", "yaesuCompressor(comp.toInt())",
                     "yaesuTxAgc(txAgc)"] {
            assert!(vm.contains(&squeeze(line)), "{line}");
        }
        // ... and no second sender beside it: each call is its definition plus the one
        // in sendTxChain (the compressor's and the AGC's definitions call the bridge
        // function of the same name).
        for (call, n) in [("yaesuTxGainSel(", 2), ("yaesuCompressor(", 3), ("yaesuTxAgc(", 3)] {
            assert_eq!(vm.matches(call).count(), n, "{call}");
        }
        for call in ["bridge?.yaesuTxGain(", "bridge?.yaesu2TxGain(", "bridge?.yaesuCompressor(", "bridge?.yaesu2Compressor(",
                     "bridge?.yaesuTxAgc(", "bridge?.yaesu2TxAgc(", "bridge?.yaesuEqBand(", "bridge?.yaesu2EqBand(",
                     "bridge?.yaesuEqEnabled(", "bridge?.yaesu2EqEnabled("] {
            assert_eq!(vm.matches(call).count(), 1, "{call}: only behind the router");
        }
        assert!(vm.contains(&squeeze("Log.i(TAG, \"TX chain -> radio $radio: mic gain $micGain, compressor ${comp.toInt()}, AGC $txAgc\")")));
        assert!(vm.contains(&squeeze("not found or unreadable, nothing changed")), "a choice that finds nothing says so");
        assert!(vm.contains(&squeeze("_state.update { cur -> SdrUiState(")));
        assert!(vm.contains(&squeeze("val sel = cur.selectedRadio")));
        assert!(!vm.contains(&squeeze("val sel = _state.value.selectedRadio")));
        let declared = vm.find(&squeeze("private val _yaesuTxLoad = MutableStateFlow<YaesuTxLoad?>(null)")).expect("the load flow");
        assert!(declared < vm.find("init {").expect("init"), "set before init starts the poll on another thread");
        let screen = squeeze(&strip_comments(&read("ui/screens/ExternalDevicesScreen.kt")));
        assert!(screen.contains(&squeeze("LaunchedEffect(yaesuTxLoad, eqRadio) {")));
        assert!(screen.contains(&squeeze("if (load.radio != eqRadio) return@LaunchedEffect")));
        // The stored settings are the source, not the load (a tab switch runs the
        // effect again with the last load).
        for read in ["eqEnabled = YaesuTxPrefs.eqEnabled(eqPrefs, eqRadio)",
                     "eqGains[i].floatValue = YaesuTxPrefs.band(eqPrefs, eqRadio, i)",
                     "localMicDb = kotlin.math.round(uniffi.sdr_remote.yaesuMicGainDb(YaesuTxPrefs.micGain(eqPrefs, eqRadio)))",
                     "compLevel = YaesuTxPrefs.compressor(eqPrefs, eqRadio)", "txAgc = YaesuTxPrefs.txAgc(eqPrefs, eqRadio)"] {
            assert!(screen.contains(&squeeze(read)), "{read}");
        }
        assert!(screen.contains(&squeeze("seq=${load.seq}, on=$eqEnabled, mic gain $micText, comp ${compLevel.toInt()}, AGC $txAgc, bands $bandsText)")),
            "the screen's line says what it took");
        // As one block: the five reads come before the line, and the line's texts come
        // from what was read (above the reads it would report the state before the load).
        assert!(screen.contains(&squeeze("eqEnabled = YaesuTxPrefs.eqEnabled(eqPrefs, eqRadio)
            for (i in 0..4) eqGains[i].floatValue = YaesuTxPrefs.band(eqPrefs, eqRadio, i)
            localMicDb = kotlin.math.round(uniffi.sdr_remote.yaesuMicGainDb(YaesuTxPrefs.micGain(eqPrefs, eqRadio)))
            compLevel = YaesuTxPrefs.compressor(eqPrefs, eqRadio)
            txAgc = YaesuTxPrefs.txAgc(eqPrefs, eqRadio)
            if (load.preset.isNotBlank()) selectedPreset = load.preset
            val micText = com.sdrremote.ui.components.gainDbLabel(localMicDb) ?: \"off\"
            val bandsText = (0..4).joinToString(\" \") { String.format(java.util.Locale.ROOT, \"%.1f\", eqGains[it].floatValue) }
            android.util.Log.i(\"YaesuTab\", \"Yaesu screen took the load (radio=${load.radio}, preset=${load.preset}, \" +
                \"seq=${load.seq}, on=$eqEnabled, mic gain $micText, comp ${compLevel.toInt()}, AGC $txAgc, bands $bandsText)\")
            }
            Row(")), "the line after the reads, from the values read, and nothing after it");
        assert!(screen.contains(&squeeze("var selectedPreset by remember(eqRadio) { mutableStateOf(\"\") }")),
            "a radio switch does not keep the other radio's preset name");
        // A radio switch without a load is carried by these keys alone: without one the
        // screen shows the other radio's value while its control writes this radio's.
        for key in ["var localMicDb by remember(micRadio) {", "var compLevel by remember(compRadio) {",
                    "var eqEnabled by remember(eqRadio) {", "val eqGains = remember(eqRadio) {", "var txAgc by remember(eqRadio) {",
                    "var btPreset by remember(eqRadio) {", "var micPreset by remember(eqRadio) {"] {
            assert!(screen.contains(&squeeze(key)), "{key}");
        }
        for write in ["YaesuTxPrefs.setMicGain(micEqPrefs, micRadio, engineGain)", "YaesuTxPrefs.setCompressor(micEqPrefs, compRadio, it)",
                      "YaesuTxPrefs.setEqEnabled(eqPrefs, eqRadio, it)", "YaesuTxPrefs.setTxAgc(eqPrefs, eqRadio, it)",
                      "YaesuTxPrefs.setBand(eqPrefs, eqRadio, i, it)"] {
            assert!(screen.contains(&squeeze(write)), "{write}: each control writes the radio it reads");
        }
        // The assignment keys through YaesuTxPrefs, not spelled out.
        for path in all_kotlin() {
            if path.ends_with("YaesuTxPrefs.kt") { continue; }
            let code = strip_comments(&std::fs::read_to_string(&path).expect("kotlin file"));
            for key in ["eq_preset_bt_", "eq_preset_mic_", "eq_enabled_"] {
                assert!(!code.contains(key), "{key} spelled out in {}", path.display());
            }
        }
        assert!(screen.contains(&squeeze("onChoosePreset(name)")));
        assert!(screen.contains(&squeeze("presetNames = YaesuTxPrefs.savePreset(eqPrefs, saveName.trim(), eqRadio)")));
    }

    /// The filter width buttons ask the shared rule, and no Kotlin table of steps
    /// comes back.
    ///
    /// Until build 47 Compose had its own copy of the rule - steps, edges, nearest
    /// step - beside the one in the desktop helpers. A reported lockup lived in that
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
            // ladder itself is the thing that must not come back.
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
                    "{}: `{}`; a Kotlin copy of the filter width rule is back; use filterStep",
                    path.display(),
                    sign
                );
            }
        }
    }

    /// What blocks the station is decided and worded in Rust
    /// (`sdr_remote_logic::station_notice`); both screens that offer Connect
    /// draw that answer and decide nothing. In build 59 the rule was four
    /// if/else chains, one of which hid the audio error, and the wizard did not
    /// have it at all. A Kotlin copy of the words must not return.
    #[test]
    fn both_connect_screens_draw_the_station_notices() {
        for file in ["ui/components/ConnectionPanel.kt", "ui/screens/ConnectWizard.kt"] {
            let code = squeeze(&strip_comments(&read(file)));
            assert!(code.contains("StationNoticeLines(stationNotices)"), "{file}: no longer draws the station notices");
            assert!(code.contains("&& canConnect"), "{file}: Connect no longer asks canConnect");
        }
        // And above the device tabs, where the Amplitec is switched (build 216).
        let main = squeeze(&strip_comments(&read("ui/screens/MainScreen.kt")));
        assert!(main.contains("item { StationNoticeLines(state.stationNotices) }"), "MainScreen: no notices on the devices screen");
        // One renderer: the loop over the lines stands once, in it.
        let loops: usize = all_kotlin()
            .iter()
            .map(|p| strip_comments(&std::fs::read_to_string(p).unwrap()).matches("for (notice in stationNotices)").count())
            .sum();
        assert_eq!(loops, 1, "a second way to draw the station notices");
        for path in all_kotlin() {
            let code = strip_comments(&std::fs::read_to_string(&path).expect("kotlin file"));
            for sign in ["conn_server_tx_closed", "conn_engine_stopped", "serverTxClosed", "engineDead"] {
                assert!(!code.contains(sign), "{}: `{}`; the station notice rule is back in Kotlin", path.display(), sign);
            }
        }
    }

    /// The S reading is `sdr_remote_core::s_meter`, through the bridge (build
    /// 218): Compose had two copies that cut the dB off where the desktop
    /// rounded, so the phone said `S9+19dB` next to a desktop `S9+20dB`. The one
    /// `S9+` left in Kotlin is the reserved width of the spectrum overlay.
    #[test]
    fn kotlin_shows_the_bridge_s_reading_and_makes_none_of_its_own() {
        let mut calls = 0;
        let mut s9 = 0;
        for path in all_kotlin() {
            let code = strip_comments(&std::fs::read_to_string(&path).expect("kotlin file"));
            calls += code.matches("sMeterText(").count();
            s9 += code.matches("S9+").count();
        }
        assert_eq!(calls, 2, "the spectrum and the meter");
        assert_eq!(s9, 1, "an S reading formatted in Kotlin");
        assert!(read("ui/components/SpectrumView.kt").contains("measureText(\"S9+00dB\")"));
    }

    /// Nor its own dBm scale: the bar position and the Yaesu
    /// conversion come from the bridge, so Kotlin holds no S9 or S0 in dBm.
    #[test]
    fn kotlin_takes_the_s_meter_scale_from_the_bridge() {
        let (mut fraction, mut yaesu) = (0, 0);
        for path in all_kotlin() {
            let code = strip_comments(&std::fs::read_to_string(&path).expect("kotlin file"));
            if path.ends_with("sdr_remote.kt") { continue; }
            // Any spelling: `-73f` and `dbm+127f` too; a
            // digit or a point before it is another number.
            for dbm in ["73f", "127f"] {
                for (at, _) in code.match_indices(dbm) {
                    if code[..at].ends_with("ev.value.toFloat() / ") {
                        continue; // a MIDI controller value, 0-127 (`MainScreen.kt`)
                    }
                    let before = code[..at].chars().next_back();
                    assert!(
                        matches!(before, Some(c) if c.is_ascii_digit() || c == '.'),
                        "{dbm} in {}", path.display()
                    );
                }
            }
            assert!(!code.contains("dbmToDisplay("), "in {}", path.display());
            fraction += code.matches("sMeterFraction(").count();
            yaesu += code.matches("yaesuSMeterDbm(").count();
        }
        assert_eq!(fraction, 2, "the level and the peak");
        assert_eq!(yaesu, 1, "the Yaesu meter");
    }

    /// Another default network starts a new relay session on it; the first
    /// report, when the callback is registered, does not (build 186). The
    /// field log is the proof that it fires on a real switch.
    #[test]
    fn another_default_network_starts_a_new_relay_session() {
        let vm = squeeze(&strip_comments(&read("viewmodel/SdrViewModel.kt")));
        assert!(vm.contains(
            "override fun onAvailable(network: Network) { val before = defaultNetwork defaultNetwork = network if (before != null && before != network) { bridge?.networkChanged() } bridge?.networkAvailable() }"
        ));
        // And the bridge hands it to the monitor's change, not its "available"
        // - two names that look alike.
        let bridge = include_str!("bridge.rs");
        let at = bridge.find("pub fn network_changed(&self) {").expect("the bridge function");
        let body = &bridge[at..at + bridge[at..].find("\n    }\n").unwrap()];
        assert!(body.contains("monitor.network_changed();"));
        assert!(!body.contains("monitor.network_available();"));
    }

    /// Every way a session ends on the phone goes through one exit,
    /// sessionGone: the link dying, and a new session on a link that never went
    /// down. The exit forgets the Yaesu subscriptions (so they are asked for
    /// again), lets a held PTT go, and forgets the connection. Until build 62
    /// only the dying link did all three; the new-session edge of build 61 let
    /// the PTT go and left the subscription believed-sent, so the Yaesu stream
    /// stayed silent after a restart inside the time-out. The
    /// desktop's counterpart is the third release_all_ptt() site and the
    /// `reconnected` edge.
    #[test]
    fn every_session_end_on_the_phone_goes_through_one_exit() {
        let vm = squeeze(&strip_comments(&read("viewmodel/SdrViewModel.kt")));
        // The whole body, verbatim: `contains` on three statements let an
        // `if (false)` around each of them through.
        let exit = concat!(
            "private fun sessionGone(why: String) { ",
            "subbed0 = false subbed1 = false ",
            "if (ptt.asking() != null) { ",
            "Log.i(TAG, \"$why while transmitting: releasing PTT\") ptt.disconnected() pttEnded() } ",
            "wasConnected = false }"
        );
        assert!(vm.contains(exit), "sessionGone is not the one exit, verbatim: see the test for what it has to be");
        // From the poll's own first statement: a filter on the state between the
        // two (`?.takeIf { it.connected }`) walked past the edge with everything
        // green.
        let new_session = "val s = bridge?.getState() ?: continue if (s.sessionGeneration != sessionGenerationSeen) { sessionGenerationSeen = s.sessionGeneration sessionGone(\"New session\") }";
        let link_down = "if (!s.connected) { sessionGone(\"Connection lost\") }";
        let at_new = vm.find(new_session).expect("the new-session edge no longer goes through sessionGone");
        let at_down = vm.find(link_down).expect("the link-down branch no longer goes through sessionGone");
        // Before the connected branch, so the same poll subscribes again; after
        // it costs one poll of 33 ms, and nothing but this would notice.
        assert!(at_new < at_down, "the new-session edge has to come before the connected branch");
    }

    /// One MainActivity: each one has its own ViewModel and so its own
    /// connection, and a start that did not come from the launcher put a second
    /// one on top of the first, both connected (2026-11-01).
    #[test]
    fn the_app_has_one_main_activity() {
        let manifest = std::fs::read_to_string("android/app/src/main/AndroidManifest.xml").expect("the manifest");
        // Without the XML comments, so the line cannot survive as prose.
        let mut bare = String::new();
        let mut rest = manifest.as_str();
        while let Some(at) = rest.find("<!--") {
            bare.push_str(&rest[..at]);
            rest = &rest[at + rest[at..].find("-->").expect("a closed comment") + 3..];
        }
        bare.push_str(rest);
        let bare = squeeze(&bare);
        let at = bare.find("android:name=\".MainActivity\"").expect("the MainActivity");
        let start = bare[..at].rfind('<').unwrap();
        let tag = &bare[start..at + bare[at..].find('>').unwrap()];
        assert!(tag.starts_with("<activity "), "the name sits in the activity tag");
        assert!(tag.contains("android:launchMode=\"singleTask\""), "MainActivity is singleTask: {tag}");
        assert_eq!(bare.matches("<activity").count(), 1, "one activity or alias in the app's own manifest");
        // The APK's manifest is merged with those of the libraries: ui-tooling
        // brought an exported PreviewActivity along, in the APK that is shipped.
        // The whole file without comments, so a version after the name, the name
        // in a variable, or `name = "ui-tooling"` is seen too. The release build
        // counts the activities in the APK itself (Test-ReleasePackage).
        let gradle = strip_comments(&std::fs::read_to_string("android/app/build.gradle.kts").expect("the app's build file"));
        assert!(!gradle.contains("ui-tooling"), "no ui-tooling in the app");
    }
}
