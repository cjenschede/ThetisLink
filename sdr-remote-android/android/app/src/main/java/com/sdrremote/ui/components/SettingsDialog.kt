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
    onDxSpotsEnabledChange: (Boolean) -> Unit = {},
    onReboot: () -> Unit,
    onShutdown: () -> Unit,
    onDismiss: () -> Unit,
) {
    val context = LocalContext.current
    val prefs = remember { context.getSharedPreferences("thetislink", android.content.Context.MODE_PRIVATE) }
    var password by remember { mutableStateOf(prefs.getString("password", "") ?: "") }
    var rebootConfirm by remember { mutableStateOf(false) }

    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("Settings") },
        text = {
            val maxHeight = (LocalConfiguration.current.screenHeightDp * 0.6f).dp
            Column(modifier = Modifier
                .heightIn(max = maxHeight)
                .verticalScroll(rememberScrollState())) {
                // Password
                Text("Server password:", fontSize = 14.sp)
                Spacer(Modifier.height(4.dp))
                OutlinedTextField(
                    value = password,
                    onValueChange = { password = it },
                    label = { Text("Password") },
                    singleLine = true,
                    visualTransformation = PasswordVisualTransformation(),
                    modifier = Modifier.fillMaxWidth(),
                )
                Spacer(Modifier.height(4.dp))
                if (password.isBlank()) {
                    Text("Password is required to connect", fontSize = 12.sp, color = Color(0xFFE53935))
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
                    Text("Connect via relay:", fontSize = 14.sp)
                    Spacer(Modifier.width(8.dp))
                    Switch(
                        checked = relayEnabled,
                        onCheckedChange = { relayEnabled = it; prefs.edit().putBoolean("relay_enabled", it).apply() },
                    )
                }
                Text(
                    "For clients that cannot port-forward (e.g. mobile). Restart the app to apply.",
                    fontSize = 11.sp,
                    color = Color.Gray,
                )
                // Dynamic restart notice: appears when the current relay config differs
                // from what is active this session (turning it on OR off).
                val relayConfiguredNow = relayEnabled && relayUrl.isNotBlank() && relayStation.isNotBlank()
                if (relayConfiguredNow != relayActiveSession) {
                    Text(
                        if (relayActiveSession) "Saved - restart the app to stop using the relay"
                        else "Saved - restart the app to connect via relay",
                        fontSize = 11.sp,
                        color = Color(0xFFC8A028),
                    )
                }
                Spacer(Modifier.height(4.dp))
                OutlinedTextField(
                    value = relayUrl,
                    onValueChange = { relayUrl = it; prefs.edit().putString("relay_url", it).apply() },
                    label = { Text("Relay URL") },
                    placeholder = { Text("ws://relay.example.com:18080") },
                    singleLine = true,
                    modifier = Modifier.fillMaxWidth(),
                )
                Spacer(Modifier.height(4.dp))
                OutlinedTextField(
                    value = relayStation,
                    onValueChange = { relayStation = it; prefs.edit().putString("relay_station", it).apply() },
                    label = { Text("Station name") },
                    singleLine = true,
                    modifier = Modifier.fillMaxWidth(),
                )
                Spacer(Modifier.height(4.dp))
                OutlinedTextField(
                    value = relayToken,
                    onValueChange = { relayToken = it; prefs.edit().putString("relay_token", it).apply() },
                    label = { Text("Token") },
                    singleLine = true,
                    visualTransformation = PasswordVisualTransformation(),
                    modifier = Modifier.fillMaxWidth(),
                )
                Spacer(Modifier.height(4.dp))
                OutlinedTextField(
                    value = relayDeviceName,
                    onValueChange = { relayDeviceName = it; prefs.edit().putString("relay_device_name", it).apply() },
                    label = { Text("Device name") },
                    singleLine = true,
                    supportingText = { Text("Shown in relay logs. Restart the app to apply.") },
                    modifier = Modifier.fillMaxWidth(),
                )
                Spacer(Modifier.height(8.dp))
                var relayUdpEnabled by remember { mutableStateOf(prefs.getBoolean("relay_udp_enabled", true)) }
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Text("Audio over UDP (low latency):", fontSize = 14.sp)
                    Spacer(Modifier.width(8.dp))
                    Switch(
                        checked = relayUdpEnabled,
                        onCheckedChange = { relayUdpEnabled = it; prefs.edit().putBoolean("relay_udp_enabled", it).apply() },
                    )
                }
                Text(
                    "On: lowest-latency audio over UDP. Off: audio stays on the encrypted relay channel. Restart the app to apply.",
                    fontSize = 11.sp,
                    color = Color.Gray,
                )

                // PTT mode
                Spacer(Modifier.height(12.dp))
                HorizontalDivider()
                Spacer(Modifier.height(8.dp))
                Text("PTT mode:", fontSize = 14.sp)
                Spacer(Modifier.height(4.dp))
                var pttToggle by remember { mutableStateOf(prefs.getBoolean("ptt_toggle", false)) }
                SingleChoiceSegmentedButtonRow(modifier = Modifier.fillMaxWidth()) {
                    SegmentedButton(
                        selected = !pttToggle,
                        onClick = { pttToggle = false; prefs.edit().putBoolean("ptt_toggle", false).apply() },
                        shape = SegmentedButtonDefaults.itemShape(index = 0, count = 2),
                    ) { Text("Push to talk", fontSize = 12.sp) }
                    SegmentedButton(
                        selected = pttToggle,
                        onClick = { pttToggle = true; prefs.edit().putBoolean("ptt_toggle", true).apply() },
                        shape = SegmentedButtonDefaults.itemShape(index = 1, count = 2),
                    ) { Text("Toggle", fontSize = 12.sp) }
                }

                // Volume button PTT (BT remote)
                Spacer(Modifier.height(8.dp))
                var volumePtt by remember { mutableStateOf(prefs.getBoolean("volume_ptt", false)) }
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Text("BT Remote = PTT:", fontSize = 14.sp)
                    Spacer(Modifier.weight(1f))
                    Switch(
                        checked = volumePtt,
                        onCheckedChange = { volumePtt = it; prefs.edit().putBoolean("volume_ptt", it).apply() },
                    )
                }
                Text("BT page turner or camera remote as PTT", fontSize = 11.sp, color = Color.Gray)

                // DX-cluster spot stream — data-saving toggle voor metered links
                Spacer(Modifier.height(12.dp))
                HorizontalDivider()
                Spacer(Modifier.height(8.dp))
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Text("DX spots:", fontSize = 14.sp)
                    Spacer(Modifier.weight(1f))
                    Switch(
                        checked = dxSpotsEnabled,
                        onCheckedChange = { onDxSpotsEnabledChange(it) },
                    )
                }
                Text("Receive DX-cluster spots (off = save data)", fontSize = 11.sp, color = Color.Gray)

                // Audio routing
                Spacer(Modifier.height(12.dp))
                HorizontalDivider()
                Spacer(Modifier.height(8.dp))
                Text("Audio routing:", fontSize = 14.sp)
                Spacer(Modifier.height(4.dp))
                SingleChoiceSegmentedButtonRow(modifier = Modifier.fillMaxWidth()) {
                    val labels = listOf("Auto", "Speaker", "Headset")
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
                    "Headset: $headsetName"
                } else if (headsetActive) {
                    "Headset active"
                } else {
                    "Handsfree speaker mode"
                }
                Text(statusText, fontSize = 12.sp, color = if (headsetActive) Color(0xFF00C800) else Color.Gray)

                // S-meter source — mirrors Thetis Multimeter Sig/Avg/MaxBin
                // selection. Same setting for both RX1 and RX2.
                Spacer(Modifier.height(12.dp))
                HorizontalDivider()
                Spacer(Modifier.height(8.dp))
                Text("S-meter source:", fontSize = 14.sp)
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
                Text("Avg matches Thetis Multimeter Avg (recommended)", fontSize = 11.sp, color = Color.Gray)

                // Mic → TX Profile mapping (phone mic + BT headset)
                if (txProfileNames.isNotEmpty()) {
                    Spacer(Modifier.height(12.dp))
                    HorizontalDivider()
                    Spacer(Modifier.height(8.dp))
                    Text("Mic → TX Profile:", fontSize = 14.sp)
                    Spacer(Modifier.height(4.dp))

                    val micLabels = listOf("Phone mic", "BT headset")
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
                                        if (selectedProfile.isEmpty()) "(none)" else selectedProfile,
                                        fontSize = 11.sp, maxLines = 1
                                    )
                                }
                                DropdownMenu(expanded = expanded, onDismissRequest = { expanded = false }) {
                                    DropdownMenuItem(
                                        text = { Text("(none)") },
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
                    Text("Auto-switches TX profile when mic changes", fontSize = 11.sp, color = Color.Gray)
                }

                Spacer(Modifier.height(12.dp))
                HorizontalDivider()
                Spacer(Modifier.height(8.dp))
                Text("Mic gate delay (experimental):", fontSize = 14.sp)
                Spacer(Modifier.height(4.dp))
                val gateLabels = listOf("Thetis phone mic", "Yaesu phone mic", "BT headset")
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
                    "PTT is immediate; mic audio opens after this delay. Test range: 0-800 ms.",
                    fontSize = 11.sp,
                    color = Color.Gray,
                )

                if (connected) {
                    Spacer(Modifier.height(12.dp))
                    HorizontalDivider()
                    Spacer(Modifier.height(12.dp))

                    if (rebootConfirm) {
                        Text("Remote server PC:", color = Color.Red, fontSize = 14.sp)
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
                                Text("Reboot", color = Color.White)
                            }
                            Button(
                                onClick = {
                                    onShutdown()
                                    rebootConfirm = false
                                    onDismiss()
                                },
                                colors = ButtonDefaults.buttonColors(containerColor = Color(0xFF800000)),
                            ) {
                                Text("Shutdown", color = Color.White)
                            }
                        }
                        Spacer(Modifier.height(4.dp))
                        TextButton(onClick = { rebootConfirm = false }) {
                            Text("Cancel")
                        }
                    } else {
                        Button(
                            onClick = { rebootConfirm = true },
                            colors = ButtonDefaults.buttonColors(containerColor = Color(0xFF404040)),
                        ) {
                            Text("Remote Reboot / Shutdown", color = Color.White)
                        }
                    }
                }
            }
        },
        confirmButton = {
            TextButton(onClick = {
                prefs.edit().putString("password", password).apply()
                onDismiss()
            }) { Text("Save") }
        },
        dismissButton = {
            TextButton(onClick = onDismiss) { Text("Cancel") }
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
