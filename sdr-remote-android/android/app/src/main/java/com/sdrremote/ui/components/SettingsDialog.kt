package com.sdrremote.ui.components

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.ui.platform.LocalConfiguration
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Switch
import androidx.compose.ui.Alignment
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
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
