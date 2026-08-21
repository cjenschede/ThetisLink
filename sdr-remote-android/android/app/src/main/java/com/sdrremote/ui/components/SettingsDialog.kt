// SPDX-License-Identifier: GPL-2.0-or-later

package com.sdrremote.ui.components

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.ui.platform.LocalConfiguration
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Slider
import androidx.compose.material3.Switch
import androidx.compose.ui.Alignment
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.width
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.material3.SegmentedButton
import androidx.compose.material3.SegmentedButtonDefaults
import androidx.compose.material3.SingleChoiceSegmentedButtonRow
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.compose.ui.res.stringResource
import com.sdrremote.R

@OptIn(androidx.compose.material3.ExperimentalMaterial3Api::class)
@Composable
fun SettingsDialog(
    connected: Boolean,
    headsetActive: Boolean,
    headsetName: String?,
    audioMode: Int, // 0=Auto, 1=Speaker, 2=Headset
    onAudioModeChange: (Int) -> Unit,
    txProfileNames: List<String> = emptyList(),
    onTxProfileChange: (Int) -> Unit = {},
    smeterSource: Int = 1,
    onSmeterSourceChange: (Int) -> Unit = {},
    dxSpotsEnabled: Boolean = true,
    dxClusterAvailable: Boolean = true,
    onDxSpotsEnabledChange: (Boolean) -> Unit = {},
    // The roger beep: pitch, length, level, whether FM counts, and a tick per
    // channel. The tone and the rules live in the shared engine this app
    // already runs; this is only the way to say what it should do.
    // Which beep channels this station actually has. A beep is a thing you
    // put ON something, so a channel that is not there gets no tick, and
    // with none of them the whole section goes.
    rogerThetisPresent: Boolean = true,
    rogerRadio1Present: Boolean = true,
    rogerRadio2Present: Boolean = true,
    // Named by the shared rule on the Rust side, never guessed here.
    radio1Label: String = "Yaesu 1",
    radio2Label: String = "Yaesu 2",
    onRogerChange: (Float, Float, Int, Boolean, Boolean, Boolean, Boolean) -> Unit = { _, _, _, _, _, _, _ -> },
    onReboot: () -> Unit,
    onShutdown: () -> Unit,
    onDismiss: () -> Unit,
) {
    val context = LocalContext.current
    val prefs = remember { context.getSharedPreferences("thetislink", android.content.Context.MODE_PRIVATE) }
    var password by remember { mutableStateOf(prefs.getString("password", "") ?: "") }
    var rebootConfirm by remember { mutableStateOf(false) }
    var rogerFreq by remember { mutableStateOf(prefs.getFloat("roger_freq_hz", 1000f)) }
    var rogerVol by remember { mutableStateOf(prefs.getFloat("roger_volume", 0.25f)) }
    var rogerMs by remember { mutableStateOf(prefs.getInt("roger_duration_ms", 150)) }
    var rogerFm by remember { mutableStateOf(prefs.getBoolean("roger_include_fm", true)) }
    var rogerThetis by remember { mutableStateOf(prefs.getBoolean("roger_on_thetis", false)) }
    var rogerRadio1 by remember { mutableStateOf(prefs.getBoolean("roger_on_radio1", false)) }
    var rogerRadio2 by remember { mutableStateOf(prefs.getBoolean("roger_on_radio2", false)) }
    // Saved and sent in one place, so a setting cannot reach the engine without
    // also surviving a restart - which is the way round it went wrong on the
    // desktop (build 67).
    fun saveRoger() {
        prefs.edit()
            .putFloat("roger_freq_hz", rogerFreq)
            .putFloat("roger_volume", rogerVol)
            .putInt("roger_duration_ms", rogerMs)
            .putBoolean("roger_include_fm", rogerFm)
            .putBoolean("roger_on_thetis", rogerThetis)
            .putBoolean("roger_on_radio1", rogerRadio1)
            .putBoolean("roger_on_radio2", rogerRadio2)
            .apply()
        onRogerChange(rogerFreq, rogerVol, rogerMs, rogerFm, rogerThetis, rogerRadio1, rogerRadio2)
    }

    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(stringResource(R.string.settings_title)) },
        text = {
            val maxHeight = (LocalConfiguration.current.screenHeightDp * 0.6f).dp
            Column(modifier = Modifier
                .heightIn(max = maxHeight)
                .verticalScroll(rememberScrollState())) {
                // Password
                Text(stringResource(R.string.settings_server_password), fontSize = 14.sp)
                Spacer(Modifier.height(4.dp))
                OutlinedTextField(
                    value = password,
                    onValueChange = { password = it },
                    label = { Text(stringResource(R.string.settings_password)) },
                    singleLine = true,
                    visualTransformation = PasswordVisualTransformation(),
                    modifier = Modifier.fillMaxWidth(),
                )
                Spacer(Modifier.height(4.dp))
                if (password.isBlank()) {
                    Text(stringResource(R.string.settings_password_required), fontSize = 12.sp, color = Color(0xFFE53935))
                }

                // Relay connection (Phase C): for clients that cannot port-forward.
                // Read once at app start; changing it takes effect after a restart.
                Spacer(Modifier.height(12.dp))
                HorizontalDivider()
                Spacer(Modifier.height(8.dp))
                var relayEnabled by remember { mutableStateOf(prefs.getBoolean("relay_enabled", false)) }
                var relayUrl by remember { mutableStateOf(prefs.getString("relay_url", "") ?: "") }
                var relayStation by remember { mutableStateOf(prefs.getString("relay_station", "") ?: "") }
                var relayToken by remember { mutableStateOf(prefs.getString("relay_token", "") ?: "") }
                var relayDeviceName by remember { mutableStateOf(prefs.getString("relay_device_name", "") ?: "") }
                // Whether the relay is the active transport THIS session (written by the
                // ViewModel at bridge creation). Used to show a restart notice when the
                // live toggle differs - symmetric for turning the relay on AND off.
                val relayActiveSession = remember { prefs.getBoolean("relay_active_session", false) }
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Text(stringResource(R.string.settings_connect_via_relay), fontSize = 14.sp)
                    Spacer(Modifier.width(8.dp))
                    Switch(
                        checked = relayEnabled,
                        onCheckedChange = { relayEnabled = it; prefs.edit().putBoolean("relay_enabled", it).apply() },
                    )
                }
                Text(
                    stringResource(R.string.settings_relay_hint),
                    fontSize = 11.sp,
                    color = Color.Gray,
                )
                // Dynamic restart notice: appears when the current relay config differs
                // from what is active this session (turning it on OR off).
                // Asked of the shared rule rather than worked out here: it counts
                // the token too, and leaving it out meant this notice stayed away
                // at the moment the relay actually became usable.
                // Remembered per set of values: this crosses into the native
                // library, and it sits in a composable that redraws on every
                // keystroke in the four fields above it.
                val relayConfiguredNow = remember(relayEnabled, relayUrl, relayStation, relayToken) {
                    uniffi.sdr_remote.relayIsConfigured(relayEnabled, relayUrl, relayStation, relayToken)
                }
                if (relayConfiguredNow != relayActiveSession) {
                    Text(
                        if (relayActiveSession) stringResource(R.string.settings_relay_saved_stop)
                        else stringResource(R.string.settings_relay_saved_start),
                        fontSize = 11.sp,
                        color = Color(0xFFC8A028),
                    )
                }
                Spacer(Modifier.height(4.dp))
                OutlinedTextField(
                    value = relayUrl,
                    onValueChange = { relayUrl = it; prefs.edit().putString("relay_url", it).apply() },
                    label = { Text(stringResource(R.string.settings_relay_url)) },
                    placeholder = { Text("ws://relay.example.com:18080") },
                    singleLine = true,
                    modifier = Modifier.fillMaxWidth(),
                )
                Spacer(Modifier.height(4.dp))
                OutlinedTextField(
                    value = relayStation,
                    onValueChange = { relayStation = it; prefs.edit().putString("relay_station", it).apply() },
                    label = { Text(stringResource(R.string.settings_station_name)) },
                    singleLine = true,
                    modifier = Modifier.fillMaxWidth(),
                )
                Spacer(Modifier.height(4.dp))
                OutlinedTextField(
                    value = relayToken,
                    onValueChange = { relayToken = it; prefs.edit().putString("relay_token", it).apply() },
                    label = { Text(stringResource(R.string.settings_token)) },
                    singleLine = true,
                    visualTransformation = PasswordVisualTransformation(),
                    modifier = Modifier.fillMaxWidth(),
                )
                Spacer(Modifier.height(4.dp))
                OutlinedTextField(
                    value = relayDeviceName,
                    onValueChange = { relayDeviceName = it; prefs.edit().putString("relay_device_name", it).apply() },
                    label = { Text(stringResource(R.string.settings_device_name)) },
                    singleLine = true,
                    supportingText = { Text(stringResource(R.string.settings_device_name_hint)) },
                    modifier = Modifier.fillMaxWidth(),
                )
                Spacer(Modifier.height(8.dp))
                var relayUdpEnabled by remember { mutableStateOf(prefs.getBoolean("relay_udp_enabled", true)) }
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Text(stringResource(R.string.settings_audio_udp), fontSize = 14.sp)
                    Spacer(Modifier.width(8.dp))
                    Switch(
                        checked = relayUdpEnabled,
                        onCheckedChange = { relayUdpEnabled = it; prefs.edit().putBoolean("relay_udp_enabled", it).apply() },
                    )
                }
                Text(
                    stringResource(R.string.settings_audio_udp_hint),
                    fontSize = 11.sp,
                    color = Color.Gray,
                )

                // PTT mode
                Spacer(Modifier.height(12.dp))
                HorizontalDivider()
                Spacer(Modifier.height(8.dp))
                Text(stringResource(R.string.settings_ptt_mode), fontSize = 14.sp)
                Spacer(Modifier.height(4.dp))
                var pttToggle by remember { mutableStateOf(prefs.getBoolean("ptt_toggle", false)) }
                SingleChoiceSegmentedButtonRow(modifier = Modifier.fillMaxWidth()) {
                    SegmentedButton(
                        selected = !pttToggle,
                        onClick = { pttToggle = false; prefs.edit().putBoolean("ptt_toggle", false).apply() },
                        shape = SegmentedButtonDefaults.itemShape(index = 0, count = 2),
                    ) { Text(stringResource(R.string.settings_push_to_talk), fontSize = 12.sp) }
                    SegmentedButton(
                        selected = pttToggle,
                        onClick = { pttToggle = true; prefs.edit().putBoolean("ptt_toggle", true).apply() },
                        shape = SegmentedButtonDefaults.itemShape(index = 1, count = 2),
                    ) { Text(stringResource(R.string.settings_toggle), fontSize = 12.sp) }
                }

                // Volume button PTT (BT remote)
                Spacer(Modifier.height(8.dp))
                var volumePtt by remember { mutableStateOf(prefs.getBoolean("volume_ptt", false)) }
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Text(stringResource(R.string.settings_bt_remote_ptt), fontSize = 14.sp)
                    Spacer(Modifier.weight(1f))
                    Switch(
                        checked = volumePtt,
                        onCheckedChange = { volumePtt = it; prefs.edit().putBoolean("volume_ptt", it).apply() },
                    )
                }
                Text(stringResource(R.string.settings_bt_remote_hint), fontSize = 11.sp, color = Color.Gray)

                // Phone volume buttons as PTT (never Bluetooth — headset keeps its own volume)
                Spacer(Modifier.height(8.dp))
                var volumeKeysPtt by remember { mutableStateOf(prefs.getBoolean("volume_keys_ptt", false)) }
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Text(stringResource(R.string.settings_volume_keys_ptt), fontSize = 14.sp)
                    Spacer(Modifier.weight(1f))
                    Switch(
                        checked = volumeKeysPtt,
                        onCheckedChange = { volumeKeysPtt = it; prefs.edit().putBoolean("volume_keys_ptt", it).apply() },
                    )
                }
                Text(stringResource(R.string.settings_volume_keys_hint), fontSize = 11.sp, color = Color.Gray)

                // DX-cluster spot stream - data-saving toggle voor metered links.
                // Only where there is a cluster to switch off: a server with no
                // callsign, or with the cluster off, can never send a spot, and
                // the switch sat there ON regardless - promising a stream that
                // could not come (owner, 2026-08-20).
                if (dxClusterAvailable) {
                    Spacer(Modifier.height(12.dp))
                    HorizontalDivider()
                    Spacer(Modifier.height(8.dp))
                    Row(verticalAlignment = Alignment.CenterVertically) {
                        Text(stringResource(R.string.settings_dx_spots), fontSize = 14.sp)
                        Spacer(Modifier.weight(1f))
                        Switch(
                            checked = dxSpotsEnabled,
                            onCheckedChange = { onDxSpotsEnabledChange(it) },
                        )
                    }
                    Text(stringResource(R.string.settings_dx_spots_hint), fontSize = 11.sp, color = Color.Gray)
                }

                // Roger beep - a tone at the end of a transmission, sent while
                // the transmitter is still keyed. Releasing PTT holds it for the
                // length set here, so the far end actually hears it.
                if (rogerThetisPresent || rogerRadio1Present || rogerRadio2Present) {
                    Spacer(Modifier.height(12.dp))
                    HorizontalDivider()
                    Spacer(Modifier.height(8.dp))
                    Text(stringResource(R.string.settings_roger), fontSize = 14.sp)
                    Text(stringResource(R.string.settings_roger_hint), fontSize = 11.sp, color = Color.Gray)
                    Spacer(Modifier.height(6.dp))
                    Row(verticalAlignment = Alignment.CenterVertically) {
                        if (rogerThetisPresent) {
                            Text("Thetis", fontSize = 13.sp)
                            Switch(checked = rogerThetis, onCheckedChange = {
                                rogerThetis = it; saveRoger()
                            })
                            Spacer(Modifier.weight(1f))
                        }
                        if (rogerRadio1Present) {
                            Text(radio1Label, fontSize = 13.sp)
                            Switch(checked = rogerRadio1, onCheckedChange = {
                                rogerRadio1 = it; saveRoger()
                            })
                            Spacer(Modifier.weight(1f))
                        }
                        if (rogerRadio2Present) {
                            Text(radio2Label, fontSize = 13.sp)
                            Switch(checked = rogerRadio2, onCheckedChange = {
                                rogerRadio2 = it; saveRoger()
                            })
                        }
                    }
                    Text("${rogerFreq.toInt()} Hz", fontSize = 12.sp, color = Color.Gray)
                    Slider(
                        value = rogerFreq,
                        onValueChange = { rogerFreq = it; saveRoger() },
                        valueRange = 300f..2700f,
                    )
                    Text("${rogerMs} ms", fontSize = 12.sp, color = Color.Gray)
                    Slider(
                        value = rogerMs.toFloat(),
                        onValueChange = { rogerMs = it.toInt(); saveRoger() },
                        valueRange = 50f..1500f,
                    )
                    Text(stringResource(R.string.settings_roger_volume) + " " +
                         String.format("%.2f", rogerVol), fontSize = 12.sp, color = Color.Gray)
                    Slider(
                        value = rogerVol,
                        onValueChange = { rogerVol = it; saveRoger() },
                        valueRange = 0f..1f,
                    )
                    Row(verticalAlignment = Alignment.CenterVertically) {
                        Text(stringResource(R.string.settings_roger_fm), fontSize = 13.sp)
                        Spacer(Modifier.weight(1f))
                        Switch(checked = rogerFm, onCheckedChange = { rogerFm = it; saveRoger() })
                    }
                }

                // Audio routing
                Spacer(Modifier.height(12.dp))
                HorizontalDivider()
                Spacer(Modifier.height(8.dp))
                Text(stringResource(R.string.settings_audio_routing), fontSize = 14.sp)
                Spacer(Modifier.height(4.dp))
                SingleChoiceSegmentedButtonRow(modifier = Modifier.fillMaxWidth()) {
                    val labels = listOf(
                        stringResource(R.string.settings_audio_auto),
                        stringResource(R.string.settings_audio_speaker),
                        stringResource(R.string.settings_audio_headset),
                    )
                    labels.forEachIndexed { index, label ->
                        SegmentedButton(
                            selected = audioMode == index,
                            onClick = { onAudioModeChange(index) },
                            shape = SegmentedButtonDefaults.itemShape(index = index, count = labels.size),
                        ) { Text(label, fontSize = 12.sp) }
                    }
                }
                Spacer(Modifier.height(4.dp))
                val statusText = if (headsetActive && headsetName != null) {
                    stringResource(R.string.settings_headset_named, headsetName)
                } else if (headsetActive) {
                    stringResource(R.string.settings_headset_active)
                } else {
                    stringResource(R.string.settings_handsfree_speaker)
                }
                Text(statusText, fontSize = 12.sp, color = if (headsetActive) Color(0xFF00C800) else Color.Gray)

                // S-meter source — mirrors Thetis Multimeter Sig/Avg/MaxBin
                // selection. Same setting for both RX1 and RX2.
                Spacer(Modifier.height(12.dp))
                HorizontalDivider()
                Spacer(Modifier.height(8.dp))
                Text(stringResource(R.string.settings_smeter_source), fontSize = 14.sp)
                Spacer(Modifier.height(4.dp))
                SingleChoiceSegmentedButtonRow(modifier = Modifier.fillMaxWidth()) {
                    val labels = listOf("Sig", "Avg", "MaxBin")
                    labels.forEachIndexed { index, label ->
                        SegmentedButton(
                            selected = smeterSource == index,
                            onClick = { onSmeterSourceChange(index) },
                            shape = SegmentedButtonDefaults.itemShape(index = index, count = labels.size),
                        ) { Text(label, fontSize = 12.sp) }
                    }
                }
                Text(stringResource(R.string.settings_smeter_hint), fontSize = 11.sp, color = Color.Gray)

                // Mic → TX Profile mapping (phone mic + BT headset)
                if (txProfileNames.isNotEmpty()) {
                    Spacer(Modifier.height(12.dp))
                    HorizontalDivider()
                    Spacer(Modifier.height(8.dp))
                    Text(stringResource(R.string.settings_mic_tx_profile), fontSize = 14.sp)
                    Spacer(Modifier.height(4.dp))

                    val micLabels = listOf(
                        stringResource(R.string.settings_mic_phone),
                        stringResource(R.string.settings_mic_bt),
                    )
                    val micKeys = listOf("android_mic", "android_bt")
                    micLabels.forEachIndexed { i, label ->
                        var selectedProfile by remember {
                            mutableStateOf(prefs.getString("mic_profile_${micKeys[i]}", "") ?: "")
                        }
                        Row(verticalAlignment = Alignment.CenterVertically) {
                            Text(label, fontSize = 12.sp, modifier = Modifier.weight(0.4f))
                            var expanded by remember { mutableStateOf(false) }
                            Box(modifier = Modifier.weight(0.6f)) {
                                OutlinedButton(onClick = { expanded = true }, modifier = Modifier.fillMaxWidth()) {
                                    Text(
                                        if (selectedProfile.isEmpty()) stringResource(R.string.settings_none) else selectedProfile,
                                        fontSize = 11.sp, maxLines = 1
                                    )
                                }
                                DropdownMenu(expanded = expanded, onDismissRequest = { expanded = false }) {
                                    DropdownMenuItem(
                                        text = { Text(stringResource(R.string.settings_none)) },
                                        onClick = {
                                            selectedProfile = ""
                                            prefs.edit().putString("mic_profile_${micKeys[i]}", "").apply()
                                            expanded = false
                                        }
                                    )
                                    txProfileNames.forEachIndexed { idx, name ->
                                        DropdownMenuItem(
                                            text = { Text(name, fontSize = 12.sp) },
                                            onClick = {
                                                selectedProfile = name
                                                prefs.edit().putString("mic_profile_${micKeys[i]}", name).apply()
                                                expanded = false
                                            }
                                        )
                                    }
                                }
                            }
                        }
                    }
                    Text(stringResource(R.string.settings_mic_tx_hint), fontSize = 11.sp, color = Color.Gray)
                }

                Spacer(Modifier.height(12.dp))
                HorizontalDivider()
                Spacer(Modifier.height(8.dp))
                Text(stringResource(R.string.settings_mic_gate_delay), fontSize = 14.sp)
                Spacer(Modifier.height(4.dp))
                val gateLabels = listOf(
                    stringResource(R.string.settings_gate_thetis_mic),
                    stringResource(R.string.settings_gate_yaesu_mic),
                    stringResource(R.string.settings_mic_bt),
                )
                val gateKeys = listOf("thetis_android_mic", "yaesu_android_mic", "android_bt")
                val gateDefaults = listOf(0, 100, 0)
                gateLabels.forEachIndexed { i, label ->
                    var delayText by remember {
                        mutableStateOf(prefs.getInt("mic_gate_delay_ms_${gateKeys[i]}", gateDefaults[i]).toString())
                    }
                    Row(verticalAlignment = Alignment.CenterVertically) {
                        Text(label, fontSize = 12.sp, modifier = Modifier.weight(0.55f))
                        OutlinedTextField(
                            value = delayText,
                            onValueChange = { raw ->
                                val filtered = raw.filter { it.isDigit() }.take(3)
                                val value = filtered.toIntOrNull()?.coerceIn(0, 800) ?: 0
                                delayText = if (filtered.isEmpty()) "" else value.toString()
                                prefs.edit().putInt("mic_gate_delay_ms_${gateKeys[i]}", value).apply()
                            },
                            label = { Text("ms") },
                            singleLine = true,
                            keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Number),
                            modifier = Modifier.weight(0.45f),
                        )
                    }
                }
                Text(
                    stringResource(R.string.settings_mic_gate_hint),
                    fontSize = 11.sp,
                    color = Color.Gray,
                )

                if (connected) {
                    Spacer(Modifier.height(12.dp))
                    HorizontalDivider()
                    Spacer(Modifier.height(12.dp))

                    if (rebootConfirm) {
                        Text(stringResource(R.string.settings_remote_server_pc), color = Color.Red, fontSize = 14.sp)
                        Spacer(Modifier.height(8.dp))
                        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                            Button(
                                onClick = {
                                    onReboot()
                                    rebootConfirm = false
                                    onDismiss()
                                },
                                colors = ButtonDefaults.buttonColors(containerColor = Color(0xFFC80000)),
                            ) {
                                Text(stringResource(R.string.settings_reboot), color = Color.White)
                            }
                            Button(
                                onClick = {
                                    onShutdown()
                                    rebootConfirm = false
                                    onDismiss()
                                },
                                colors = ButtonDefaults.buttonColors(containerColor = Color(0xFF800000)),
                            ) {
                                Text(stringResource(R.string.settings_shutdown), color = Color.White)
                            }
                        }
                        Spacer(Modifier.height(4.dp))
                        TextButton(onClick = { rebootConfirm = false }) {
                            Text(stringResource(R.string.common_cancel))
                        }
                    } else {
                        Button(
                            onClick = { rebootConfirm = true },
                            colors = ButtonDefaults.buttonColors(containerColor = Color(0xFF404040)),
                        ) {
                            Text(stringResource(R.string.settings_remote_reboot_shutdown), color = Color.White)
                        }
                    }
                }
            }
        },
        confirmButton = {
            TextButton(onClick = {
                prefs.edit().putString("password", password).apply()
                onDismiss()
            }) { Text(stringResource(R.string.common_save)) }
        },
        dismissButton = {
            TextButton(onClick = onDismiss) { Text(stringResource(R.string.common_cancel)) }
        },
    )
}

/** Parse "21:Normaal,25:Remote" into list of (index, name) pairs. */
fun parseTxProfiles(str: String): List<Pair<Int, String>> {
    if (str.isBlank()) return emptyList()
    return str.split(",").mapNotNull { entry ->
        val parts = entry.trim().split(":", limit = 2)
        if (parts.size == 2) {
            val idx = parts[0].trim().toIntOrNull()
            val name = parts[1].trim()
            if (idx != null && name.isNotEmpty()) idx to name else null
        } else null
    }
}
