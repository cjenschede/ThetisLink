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
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
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

@Composable
fun ConnectionPanel(
    connected: Boolean,
    audioError: Boolean,
    transmitting: Boolean = false,
    paForwardW: Int = 0,
    paMaxW: Int = 0,
    paName: String = "",
    totpRequired: Boolean = false,
    onConnect: (String, String) -> Unit,
    onDisconnect: () -> Unit,
    onSendTotp: (String) -> Unit = {},
) {
    val context = LocalContext.current
    val prefs = remember { context.getSharedPreferences("thetislink", android.content.Context.MODE_PRIVATE) }
    var serverInput by rememberSaveable { mutableStateOf(prefs.getString("server_addr", "192.168.1.79:4580") ?: "192.168.1.79:4580") }

    Row(
        modifier = Modifier.fillMaxWidth(),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        OutlinedTextField(
            value = serverInput,
            onValueChange = { serverInput = it },
            label = { Text("Server") },
            singleLine = true,
            enabled = !connected,
            modifier = Modifier.weight(1f),
        )

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
            Button(
                onClick = {
                    prefs.edit().putString("server_addr", serverInput).apply()
                    onConnect(serverInput, pw)
                },
                enabled = pw.isNotBlank(),
            ) {
                Text("Connect")
            }
            if (pw.isBlank()) {
                Text("Set password in Settings", fontSize = 11.sp, color = Color(0xFFE53935))
            }
        }
    }

    // TOTP 2FA input row
    if (totpRequired) {
        var totpInput by remember { mutableStateOf("") }
        Row(
            modifier = Modifier.fillMaxWidth().padding(top = 4.dp),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            OutlinedTextField(
                value = totpInput,
                onValueChange = { if (it.length <= 6 && it.all { c -> c.isDigit() }) totpInput = it },
                label = { Text("2FA Code") },
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
