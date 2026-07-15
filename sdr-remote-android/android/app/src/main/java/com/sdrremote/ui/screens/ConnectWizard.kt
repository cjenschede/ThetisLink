// SPDX-License-Identifier: GPL-2.0-or-later

package com.sdrremote.ui.screens

import android.content.Context
import androidx.activity.compose.BackHandler
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.Checkbox
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.sdrremote.nsd.MdnsDiscovery

/** PATCH-4: first-run connection wizard for Android. Mirrors the
 *  desktop's `wizard.rs` flow (Discover → Password → 2FA → Connecting →
 *  Success) but lives natively on Compose so the system back-button,
 *  multicast lock, and SharedPreferences all integrate properly. */
enum class WizardStep {
    DiscoverServer,
    EnterPassword,
    Verifying,
    AwaitingTotp,
    Verifying2fa,
    Success,
}

sealed interface WizardOutcome {
    data object Continue : WizardOutcome
    data object SkipToManual : WizardOutcome
    data object Finished : WizardOutcome
}

@Composable
fun ConnectWizard(
    initialServer: String,
    initialPassword: String,
    connected: Boolean,
    awaitingTotp: Boolean,
    connectStatusIsError: Boolean,
    connectStatusHeadline: String,
    connectStatusAction: String,
    onConnect: (String, String) -> Unit,
    onSendTotp: (String) -> Unit,
    onDisconnect: () -> Unit,
    onSkip: () -> Unit,
    onFinished: (String, String) -> Unit,
) {
    val context = LocalContext.current
    var step by rememberSaveable { mutableStateOf(WizardStep.DiscoverServer) }
    var serverInput by rememberSaveable { mutableStateOf(initialServer) }
    var passwordInput by rememberSaveable { mutableStateOf(initialPassword) }
    var passwordVisible by rememberSaveable { mutableStateOf(false) }
    var totpInput by rememberSaveable { mutableStateOf("") }
    var skipConfirmOpen by rememberSaveable { mutableStateOf(false) }

    // Drive step transitions off the same connect-status fields the
    // ConnectionPanel uses. Same source-of-truth as PATCH-1.
    LaunchedEffect(connected, awaitingTotp, connectStatusIsError) {
        when {
            connected && step != WizardStep.Success -> step = WizardStep.Success
            awaitingTotp && step != WizardStep.AwaitingTotp -> step = WizardStep.AwaitingTotp
            connectStatusIsError -> {
                // Fail rolls back to the most fix-able pre-state.
                step = if (step == WizardStep.Verifying2fa || step == WizardStep.AwaitingTotp) {
                    WizardStep.AwaitingTotp
                } else {
                    WizardStep.EnterPassword
                }
            }
        }
    }

    // mDNS browse — same lifecycle pattern as ConnectionPanel: start
    // while wizard is composed, release multicast lock on dispose.
    val mdns = remember { MdnsDiscovery(context) }
    DisposableEffect(Unit) {
        mdns.start()
        onDispose { mdns.stop() }
    }
    val discoveredServers by mdns.servers.collectAsState()
    var dropdownOpen by remember { mutableStateOf(false) }

    // System back-button: step > 1 goes back one step; step 1 asks to
    // skip to manual setup.
    BackHandler(enabled = step != WizardStep.Success) {
        when (step) {
            WizardStep.DiscoverServer -> skipConfirmOpen = true
            WizardStep.EnterPassword, WizardStep.Verifying -> {
                step = WizardStep.DiscoverServer
            }
            WizardStep.AwaitingTotp, WizardStep.Verifying2fa -> {
                onDisconnect()
                step = WizardStep.EnterPassword
            }
            WizardStep.Success -> { /* not reachable: enabled=false */ }
        }
    }

    if (skipConfirmOpen) {
        AlertDialog(
            onDismissRequest = { skipConfirmOpen = false },
            title = { Text("Skip wizard?") },
            text = { Text("Jump straight to the regular connect screen. Your config is not updated, so the wizard will appear again next time you start the app.") },
            confirmButton = {
                TextButton(onClick = {
                    skipConfirmOpen = false
                    onSkip()
                }) { Text("Skip") }
            },
            dismissButton = {
                TextButton(onClick = { skipConfirmOpen = false }) { Text("Stay") }
            },
        )
    }

    val (stepIdx, totalSteps, stepLabel) = when (step) {
        WizardStep.DiscoverServer -> Triple(1, 4, "Find the server")
        WizardStep.EnterPassword, WizardStep.Verifying -> Triple(2, 4, "Enter password")
        WizardStep.AwaitingTotp, WizardStep.Verifying2fa -> Triple(3, 4, "2FA code")
        WizardStep.Success -> Triple(4, 4, "Connected")
    }

    Column(
        modifier = Modifier
            .fillMaxSize()
            .padding(16.dp)
            .verticalScroll(rememberScrollState()),
        verticalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        Row(
            modifier = Modifier.fillMaxWidth(),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            Text(
                "Step $stepIdx of $totalSteps: $stepLabel",
                style = MaterialTheme.typography.titleMedium,
                modifier = Modifier.weight(1f),
            )
            TextButton(onClick = { skipConfirmOpen = true }) {
                Text("Skip", fontSize = 12.sp)
            }
        }
        LinearProgressIndicator(
            progress = { stepIdx.toFloat() / totalSteps.toFloat() },
            modifier = Modifier.fillMaxWidth(),
        )

        when (step) {
            WizardStep.DiscoverServer -> {
                Text("Pick a server from the list or enter the address manually.")
                if (discoveredServers.isNotEmpty()) {
                    Row(verticalAlignment = Alignment.CenterVertically) {
                        Text("Found:", fontSize = 12.sp)
                        Spacer(Modifier.width(8.dp))
                        TextButton(onClick = { dropdownOpen = true }) {
                            Text("Discovered (${discoveredServers.size})")
                        }
                        DropdownMenu(
                            expanded = dropdownOpen,
                            onDismissRequest = { dropdownOpen = false },
                        ) {
                            discoveredServers.forEach { srv ->
                                DropdownMenuItem(
                                    text = { Text(srv.displayLabel()) },
                                    onClick = {
                                        serverInput = srv.addrPort
                                        dropdownOpen = false
                                    },
                                )
                            }
                        }
                    }
                } else {
                    Text(
                        "Scanning local network…",
                        fontSize = 11.sp,
                        color = Color(0xFF989898),
                    )
                }
                OutlinedTextField(
                    value = serverInput,
                    onValueChange = { serverInput = it },
                    label = { Text("Server") },
                    singleLine = true,
                    modifier = Modifier.fillMaxWidth(),
                )
                Button(
                    onClick = { step = WizardStep.EnterPassword },
                    enabled = serverInput.isNotBlank(),
                ) { Text("Next") }
            }
            WizardStep.EnterPassword -> {
                Text("Enter the server password. Ask the operator of the server PC for it.")
                OutlinedTextField(
                    value = passwordInput,
                    onValueChange = { passwordInput = it },
                    label = { Text("Password") },
                    singleLine = true,
                    visualTransformation = if (passwordVisible)
                        androidx.compose.ui.text.input.VisualTransformation.None
                    else
                        androidx.compose.ui.text.input.PasswordVisualTransformation(),
                    modifier = Modifier.fillMaxWidth(),
                )
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Checkbox(
                        checked = passwordVisible,
                        onCheckedChange = { passwordVisible = it },
                    )
                    Text("Show password", fontSize = 12.sp)
                }
                Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    TextButton(onClick = { step = WizardStep.DiscoverServer }) {
                        Text("Back")
                    }
                    Button(
                        onClick = {
                            onConnect(serverInput, passwordInput)
                            step = WizardStep.Verifying
                        },
                        enabled = passwordInput.isNotBlank(),
                    ) { Text("Connect") }
                }
            }
            WizardStep.Verifying -> {
                Row(verticalAlignment = Alignment.CenterVertically) {
                    CircularProgressIndicator()
                    Spacer(Modifier.width(12.dp))
                    Text("Connecting…")
                }
            }
            WizardStep.AwaitingTotp -> {
                Text("Open your authenticator app and enter the 6-digit code.")
                OutlinedTextField(
                    value = totpInput,
                    onValueChange = { if (it.length <= 6 && it.all { c -> c.isDigit() }) totpInput = it },
                    label = { Text("2FA code") },
                    singleLine = true,
                    modifier = Modifier.fillMaxWidth(),
                )
                Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    TextButton(onClick = {
                        onDisconnect()
                        step = WizardStep.EnterPassword
                    }) { Text("Back") }
                    Button(
                        onClick = {
                            onSendTotp(totpInput)
                            totpInput = ""
                            step = WizardStep.Verifying2fa
                        },
                        enabled = totpInput.length == 6,
                    ) { Text("Verify") }
                }
            }
            WizardStep.Verifying2fa -> {
                Row(verticalAlignment = Alignment.CenterVertically) {
                    CircularProgressIndicator()
                    Spacer(Modifier.width(12.dp))
                    Text("Verifying 2FA code…")
                }
            }
            WizardStep.Success -> {
                Text(
                    "Connected!",
                    fontSize = 22.sp,
                    color = Color(0xFF32B432),
                )
                Text("Next time you start the app the wizard is skipped automatically.")
                Button(onClick = { onFinished(serverInput, passwordInput) }) {
                    Text("Done")
                }
            }
        }

        // Sticky error footer (same i18n text Rust produces for the panel)
        if (connectStatusIsError && connectStatusHeadline.isNotEmpty()) {
            Spacer(Modifier.height(8.dp))
            Text(
                connectStatusHeadline,
                color = Color(0xFFDC2828),
                fontSize = 16.sp,
                style = MaterialTheme.typography.titleSmall,
            )
            if (connectStatusAction.isNotEmpty()) {
                Text(connectStatusAction, fontSize = 13.sp)
            }
        }
    }
}

/**
 * SharedPreferences-based first-run gate. `successful_connects` mirrors
 * the desktop `thetislink-client.conf` field — first-run = counter == 0.
 */
object WizardPrefs {
    private const val FILE = "thetislink"
    private const val KEY = "successful_connects"

    fun isFirstRun(context: Context): Boolean {
        val prefs = context.getSharedPreferences(FILE, Context.MODE_PRIVATE)
        return prefs.getInt(KEY, 0) == 0
    }

    fun markSuccessful(context: Context) {
        val prefs = context.getSharedPreferences(FILE, Context.MODE_PRIVATE)
        if (prefs.getInt(KEY, 0) >= 1) return
        prefs.edit().putInt(KEY, 1).apply()
    }
}
