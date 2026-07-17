// SPDX-License-Identifier: GPL-2.0-or-later

package com.sdrremote.ui.components

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.sdrremote.nsd.MdnsDiscovery

@Composable
fun ConnectionPanel(
    connected: Boolean,
    audioError: Boolean,
    transmitting: Boolean = false,
    paForwardW: Int = 0,
    paMaxW: Int = 0,
    paName: String = "",
    // PATCH-1: connect-status text rendered in Rust via i18n.rs; Kotlin/Compose
    // just displays. headline = main user-visible line, action = optional hint
    // ("" when no hint). is_error = render red, otherwise neutral. is_awaiting_totp
    // = show 2FA input row.
    connectStatusHeadline: String = "",
    connectStatusAction: String = "",
    connectStatusIsError: Boolean = false,
    connectStatusIsAwaitingTotp: Boolean = false,
    onConnect: (String, String) -> Unit,
    onDisconnect: () -> Unit,
    onSendTotp: (String) -> Unit = {},
) {
    val context = LocalContext.current
    val prefs = remember { context.getSharedPreferences("thetislink", android.content.Context.MODE_PRIVATE) }
    var serverInput by rememberSaveable { mutableStateOf(prefs.getString("server_addr", "192.168.1.79:4580") ?: "192.168.1.79:4580") }

    // When the relay is the active route the direct server-IP is not relevant
    // (the relay decides the destination via station/token), so we replace the
    // IP field with the relay destination. Mirrors the bridge's relay-tunnel
    // condition (relay enabled + url + station all set).
    val relayEnabled = prefs.getBoolean("relay_enabled", false)
    val relayUrl = prefs.getString("relay_url", "") ?: ""
    val relayStation = prefs.getString("relay_station", "") ?: ""
    val viaRelay = relayEnabled && relayUrl.isNotBlank() && relayStation.isNotBlank()

    // PATCH-3: NSD-based mDNS discovery — only runs while ConnectionPanel
    // is in the composition AND the user is not yet connected. Multicast
    // lock is acquired in start() and released in stop(); leaving the
    // panel (compose dispose) tears the discovery down so the radio stack
    // doesn't keep the multicast lock held all session long.
    val mdns = remember { MdnsDiscovery(context) }
    DisposableEffect(connected) {
        if (!connected) mdns.start()
        onDispose { mdns.stop() }
    }
    val discoveredServers by mdns.servers.collectAsState()
    var discoveryMenuOpen by remember { mutableStateOf(false) }

    Row(
        modifier = Modifier.fillMaxWidth(),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        if (viaRelay) {
            Column(modifier = Modifier.weight(1f)) {
                Text("Via relay", fontSize = 11.sp, color = Color(0xFF989898))
                Text(
                    if (relayStation.isNotBlank()) relayStation else "(station)",
                    fontSize = 14.sp,
                    fontWeight = FontWeight.Bold,
                )
            }
        } else {
            OutlinedTextField(
                value = serverInput,
                onValueChange = { serverInput = it },
                label = { Text("Server") },
                singleLine = true,
                enabled = !connected,
                modifier = Modifier.weight(1f),
            )
        }

        if (connected) {
            val btnColor = if (audioError) Color(0xFFC62828) else Color(0xFF666666)
            Button(
                onClick = onDisconnect,
                colors = ButtonDefaults.buttonColors(containerColor = btnColor),
            ) {
                Text("Disconnect")
            }
        } else {
            val pw = prefs.getString("password", "") ?: ""
            // PATCH-1 smoke-test fix (2026-05-12): disable Connect while mid-auth.
            // AwaitingTotp = user must press Verify; pressing Connect again would
            // regress engine to "Connecting" and the server's PendingTotp session
            // would never recover.
            Button(
                onClick = {
                    prefs.edit().putString("server_addr", serverInput).apply()
                    onConnect(serverInput, pw)
                },
                enabled = pw.isNotBlank() && !connectStatusIsAwaitingTotp,
            ) {
                Text("Connect")
            }
            if (pw.isBlank()) {
                Text("Set password in Settings", fontSize = 11.sp, color = Color(0xFFE53935))
            }
        }
    }

    // PATCH-3: dropdown with mDNS-discovered servers. Empty list = nothing
    // to render; user still has the manual IP field above as the always-on
    // fallback. List appears progressively as scans resolve, so we don't
    // gate Connect on it.
    if (!connected && !viaRelay && discoveredServers.isNotEmpty()) {
        Row(
            modifier = Modifier.fillMaxWidth().padding(top = 4.dp),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            Text("Found:", fontSize = 12.sp)
            Box {
                TextButton(onClick = { discoveryMenuOpen = true }) {
                    Text("Choose discovered server (${discoveredServers.size})")
                }
                DropdownMenu(
                    expanded = discoveryMenuOpen,
                    onDismissRequest = { discoveryMenuOpen = false },
                ) {
                    discoveredServers.forEach { srv ->
                        DropdownMenuItem(
                            text = { Text(srv.displayLabel()) },
                            onClick = {
                                serverInput = srv.addrPort
                                discoveryMenuOpen = false
                            },
                        )
                    }
                }
            }
        }
    }

    // PATCH-1: connect-status display.
    // - AwaitingTotp: show 2FA input row + the headline ("Enter 2FA code") and action hint
    // - Failed: show headline in red + action hint as small grey text
    // - Other (Disconnected/Connecting/Connected): no extra row here; status row below shows colored badge
    if (connectStatusIsAwaitingTotp) {
        var totpInput by remember { mutableStateOf("") }
        Row(
            modifier = Modifier.fillMaxWidth().padding(top = 4.dp),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            OutlinedTextField(
                value = totpInput,
                onValueChange = { if (it.length <= 6 && it.all { c -> c.isDigit() }) totpInput = it },
                label = { Text(if (connectStatusHeadline.isNotEmpty()) connectStatusHeadline else "2FA Code") },
                singleLine = true,
                modifier = Modifier.weight(1f),
            )
            Button(
                onClick = { onSendTotp(totpInput); totpInput = "" },
                enabled = totpInput.length == 6,
            ) {
                Text("Verify")
            }
        }
        if (connectStatusAction.isNotEmpty()) {
            Text(
                text = connectStatusAction,
                color = Color(0xFF989898),
                fontSize = 11.sp,
                modifier = Modifier.padding(top = 2.dp, start = 4.dp),
            )
        }
    } else if (connectStatusIsError && connectStatusHeadline.isNotEmpty()) {
        Text(
            text = connectStatusHeadline,
            color = Color(0xFFDC2828),
            fontSize = 14.sp,
            fontWeight = FontWeight.Bold,
            modifier = Modifier.padding(top = 4.dp, start = 4.dp),
        )
        if (connectStatusAction.isNotEmpty()) {
            Text(
                text = connectStatusAction,
                color = Color(0xFF989898),
                fontSize = 11.sp,
                modifier = Modifier.padding(top = 2.dp, start = 4.dp),
            )
        }
    }

    // Status row with optional PA power bar
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .padding(top = 4.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        val statusColor = if (connected) Color(0xFF00C800) else Color(0xFFC80000)
        val statusText = if (connected) "Connected" else "Disconnected"
        Text(text = statusText, color = statusColor, fontSize = 14.sp)

        if (audioError) {
            Text(
                text = "Audio error — reconnecting...",
                color = Color(0xFFFFA500),
                fontSize = 12.sp,
            )
        } else if (transmitting && paMaxW > 0 && paForwardW > 0) {
            // PA power bar during TX
            val frac = (paForwardW.toFloat() / paMaxW).coerceIn(0f, 1f)
            val barColor = if (frac > 0.9f) Color(0xFFF44336) else if (frac > 0.7f) Color(0xFFFFA500) else Color(0xFF32B432)
            Box(
                modifier = Modifier
                    .weight(1f)
                    .height(16.dp)
                    .clip(RoundedCornerShape(4.dp))
                    .background(Color(0xFF2A2A2A)),
            ) {
                Box(
                    modifier = Modifier
                        .fillMaxWidth(frac)
                        .height(16.dp)
                        .background(barColor),
                )
                Text(
                    text = "${paForwardW}W $paName",
                    color = Color.White,
                    fontSize = 11.sp,
                    fontWeight = FontWeight.Bold,
                    modifier = Modifier.align(Alignment.Center),
                )
            }
        }
    }
}
