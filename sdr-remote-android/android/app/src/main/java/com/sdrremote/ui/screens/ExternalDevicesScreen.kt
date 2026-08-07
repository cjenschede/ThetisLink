// SPDX-License-Identifier: GPL-2.0-or-later

package com.sdrremote.ui.screens

import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.ui.platform.LocalContext
import androidx.compose.foundation.clickable
import androidx.compose.foundation.gestures.detectTapGestures
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.StrokeCap
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.graphics.TransformOrigin
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.layout.layout
import androidx.compose.ui.unit.Constraints
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.drawText
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.rememberTextMeasurer
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.sdrremote.R
import com.sdrremote.SdrUiState
import kotlin.math.atan2
import kotlin.math.cos
import kotlin.math.sin
import kotlin.math.sqrt

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun ExternalDevicesScreen(
    state: SdrUiState,
    onSetSwitchA: (Int) -> Unit,
    onSetSwitchB: (Int) -> Unit,
    onSpeOperate: () -> Unit = {},
    onSpeTune: () -> Unit = {},
    onSpeAntenna: () -> Unit = {},
    onSpeInput: () -> Unit = {},
    onSpePower: () -> Unit = {},
    onSpeOff: () -> Unit = {},
    onSpePowerOn: () -> Unit = {},
    onSpeDriveDown: () -> Unit = {},
    onSpeDriveUp: () -> Unit = {},
    onTunerTune: () -> Unit = {},
    onTunerAbort: () -> Unit = {},
    onRf2kOperate: (Boolean) -> Unit = {},
    onRf2kTune: () -> Unit = {},
    onRf2kAnt1: () -> Unit = {},
    onRf2kAnt2: () -> Unit = {},
    onRf2kAnt3: () -> Unit = {},
    onRf2kAnt4: () -> Unit = {},
    onRf2kAntExt: () -> Unit = {},
    onRf2kErrorReset: () -> Unit = {},
    onRf2kClose: () -> Unit = {},
    onRf2kDriveUp: () -> Unit = {},
    onRf2kDriveDown: () -> Unit = {},
    onRf2kTunerMode: (UByte) -> Unit = {},
    onRf2kTunerBypass: (Boolean) -> Unit = {},
    onRf2kTunerReset: () -> Unit = {},
    onRf2kTunerStore: () -> Unit = {},
    onRf2kTunerLUp: () -> Unit = {},
    onRf2kTunerLDown: () -> Unit = {},
    onRf2kTunerCUp: () -> Unit = {},
    onRf2kTunerCDown: () -> Unit = {},
    onRf2kTunerK: () -> Unit = {},
    onUbRetract: () -> Unit = {},
    onUbSetFrequency: (Int, Int) -> Unit = { _, _ -> },
    onUbReadElements: () -> Unit = {},
    onRotorGoTo: (Int) -> Unit = {},
    onRotorStop: () -> Unit = {},
    onRotorCw: () -> Unit = {},
    onRotorCcw: () -> Unit = {},
    onYaesuEnable: (Boolean) -> Unit = {},
    onYaesuPowerOnOff: (Boolean) -> Unit = {},
    onYaesuPtt: (Boolean) -> Unit = {},
    onYaesuVolume: (Float) -> Unit = {},
    onYaesuSelectVfo: (Int) -> Unit = {},
    onYaesuMode: (Int) -> Unit = {},
    onYaesuButton: (Int) -> Unit = {},
    onYaesuRecallMemory: (Int) -> Unit = {},
    onYaesuControl: (Int, Int) -> Unit = { _, _ -> }, // (controlId, value)
    onYaesuFreq: (Long) -> Unit = {},
    onYaesuSelectRadio: (Int) -> Unit = {},
    onYaesuDspControl: (Int, Int) -> Unit = { _, _ -> },
    onYaesuEqBand: (Int, Float) -> Unit = { _, _ -> },
    onYaesuEqEnabled: (Boolean) -> Unit = {},
    onYaesuCompressor: (Int) -> Unit = {},
    onYaesuTxAgc: (Boolean) -> Unit = {},
    onYaesuTxGain: (Float) -> Unit = {},
    yaesuActive: Boolean = false,
    selectedTab: Int = 0,
    onTabChange: (Int) -> Unit = {},
    modifier: Modifier = Modifier,
) {
    val hasAmplitec = state.amplitecConnected || state.amplitecSwitchA > 0
    val hasTuner = state.tunerConnected
    val hasSpe = state.speAvailable && state.speActive
    val hasRf2k = state.rf2kAvailable && state.rf2kActive
    val hasUltraBeam = state.ubAvailable
    val hasRotor = state.rotorAvailable
    val hasYaesu = state.yaesuConnected || state.yaesu2Connected

    if (!hasAmplitec && !hasTuner && !hasSpe && !hasRf2k && !hasUltraBeam && !hasRotor && !hasYaesu) {
        Column(
            modifier = modifier
                .fillMaxSize()
                .padding(16.dp),
            horizontalAlignment = Alignment.CenterHorizontally,
            verticalArrangement = Arrangement.Center,
        ) {
            Text(stringResource(R.string.dev_no_devices), style = MaterialTheme.typography.titleMedium)
            Spacer(Modifier.height(8.dp))
            Text(
                stringResource(R.string.dev_configure_hint),
                style = MaterialTheme.typography.bodyMedium,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }
        return
    }

    // Build tab list based on available devices
    val tabs = remember(hasAmplitec, hasTuner, hasSpe, hasRf2k, hasUltraBeam, hasRotor, hasYaesu) {
        buildList {
            if (hasAmplitec) add("Amplitec" to 0)
            if (hasTuner) add("JC-4s" to 1)
            if (hasSpe) add("SPE" to 2)
            if (hasRf2k) add("RF2K-S" to 3)
            if (hasUltraBeam) add("UBeam" to 4)
            if (hasRotor) add("Rotor" to 5)
            if (hasYaesu) add("Yaesu" to 6)
        }
    }

    // Ensure selected tab is valid
    val validTab = tabs.firstOrNull { it.second == selectedTab } ?: tabs.firstOrNull()
    if (validTab != null && validTab.second != selectedTab) {
        onTabChange(validTab.second)
    }

    Column(
        modifier = modifier
            .fillMaxSize()
            .padding(16.dp),
    ) {
        // Tab selector (2 rows of 3)
        if (tabs.size > 1) {
            val row1 = tabs.take(3)
            val row2 = tabs.drop(3)
            SingleChoiceSegmentedButtonRow(modifier = Modifier.fillMaxWidth()) {
                row1.forEachIndexed { index, (label, id) ->
                    SegmentedButton(
                        selected = selectedTab == id,
                        onClick = { onTabChange(id) },
                        shape = SegmentedButtonDefaults.itemShape(index = index, count = row1.size),
                    ) { Text(label, fontSize = 11.sp, maxLines = 1) }
                }
            }
            if (row2.isNotEmpty()) {
                Spacer(Modifier.height(2.dp))
                SingleChoiceSegmentedButtonRow(modifier = Modifier.fillMaxWidth()) {
                    row2.forEachIndexed { index, (label, id) ->
                        SegmentedButton(
                            selected = selectedTab == id,
                            onClick = { onTabChange(id) },
                            shape = SegmentedButtonDefaults.itemShape(index = index, count = row2.size),
                        ) { Text(label, fontSize = 11.sp, maxLines = 1) }
                    }
                }
            }
            Spacer(Modifier.height(8.dp))
        }

        // Content
        when (selectedTab) {
            0 -> AmplitecTab(state, onSetSwitchA, onSetSwitchB)
            1 -> TunerTab(state, onTunerTune, onTunerAbort)
            2 -> SpeExpertTab(state, onSpeOperate, onSpeTune, onSpeAntenna, onSpeInput, onSpePower, onSpeOff, onSpePowerOn, onSpeDriveDown, onSpeDriveUp)
            3 -> Rf2kTab(state, onRf2kOperate, onRf2kTune, onRf2kAnt1, onRf2kAnt2, onRf2kAnt3, onRf2kAnt4, onRf2kAntExt, onRf2kErrorReset, onRf2kClose, onRf2kDriveUp, onRf2kDriveDown, onRf2kTunerMode, onRf2kTunerBypass, onRf2kTunerReset, onRf2kTunerStore, onRf2kTunerLUp, onRf2kTunerLDown, onRf2kTunerCUp, onRf2kTunerCDown, onRf2kTunerK)
            4 -> UltraBeamTab(state, onUbSetFrequency, onUbRetract, onUbReadElements)
            5 -> RotorTab(state, onRotorGoTo, onRotorStop, onRotorCw, onRotorCcw)
            6 -> YaesuTab(state, onYaesuEnable, onYaesuPtt, onYaesuVolume, onYaesuSelectVfo, onYaesuMode, onYaesuButton, onYaesuRecallMemory, onYaesuControl, onYaesuFreq, yaesuActive, onYaesuEqBand, onYaesuEqEnabled, onYaesuTxGain, onYaesuSelectRadio, onYaesuDspControl, onYaesuCompressor, onYaesuTxAgc, onPowerOnOff = onYaesuPowerOnOff)
        }
    }
}

@Composable
private fun AmplitecTab(
    state: SdrUiState,
    onSetSwitchA: (Int) -> Unit,
    onSetSwitchB: (Int) -> Unit,
) {
    val labels = remember(state.amplitecLabels) {
        if (state.amplitecLabels.isNotEmpty()) {
            state.amplitecLabels.split(",")
        } else {
            (1..12).map { it.toString() }
        }
    }

    fun labelA(pos: Int): String = labels.getOrElse(pos - 1) { pos.toString() }
    fun labelB(pos: Int): String = labels.getOrElse(pos + 5) { pos.toString() }

    // Header
    Row(
        modifier = Modifier.fillMaxWidth(),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.SpaceBetween,
    ) {
        Text(
            "Amplitec 6/2",
            style = MaterialTheme.typography.titleLarge,
            fontWeight = FontWeight.Bold,
        )
        StatusIndicator(state.amplitecConnected)
    }

    HorizontalDivider(modifier = Modifier.padding(vertical = 8.dp))

    // Poort A — TX+RX
    Row(verticalAlignment = Alignment.CenterVertically) {
        Text(
            stringResource(R.string.dev_port_a),
            fontWeight = FontWeight.Bold,
            style = MaterialTheme.typography.titleSmall,
        )
        if (state.amplitecSwitchA > 0) {
            Text(
                stringResource(R.string.dev_amplitec_current, labelA(state.amplitecSwitchA)),
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }
    }
    Spacer(Modifier.height(4.dp))
    Row(horizontalArrangement = Arrangement.spacedBy(6.dp)) {
        for (pos in 1..6) {
            val isActive = state.amplitecSwitchA == pos
            val isBlocked = state.amplitecSwitchB == pos
            AmplitecAntennaButton(
                pos = pos,
                alias = labelA(pos),
                enabled = state.amplitecConnected,
                isActive = isActive,
                isBlocked = isBlocked,
                onClick = { onSetSwitchA(pos) },
                modifier = Modifier.weight(1f),
            )
        }
    }

    Spacer(Modifier.height(12.dp))

    // Poort B — RX
    Row(verticalAlignment = Alignment.CenterVertically) {
        Text(
            stringResource(R.string.dev_port_b),
            fontWeight = FontWeight.Bold,
            style = MaterialTheme.typography.titleSmall,
        )
        if (state.amplitecSwitchB > 0) {
            Text(
                stringResource(R.string.dev_amplitec_current, labelB(state.amplitecSwitchB)),
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }
    }
    Spacer(Modifier.height(4.dp))
    Row(horizontalArrangement = Arrangement.spacedBy(6.dp)) {
        for (pos in 1..6) {
            val isActive = state.amplitecSwitchB == pos
            val isBlocked = state.amplitecSwitchA == pos
            AmplitecAntennaButton(
                pos = pos,
                alias = labelB(pos),
                enabled = state.amplitecConnected,
                isActive = isActive,
                isBlocked = isBlocked,
                onClick = { onSetSwitchB(pos) },
                modifier = Modifier.weight(1f),
            )
        }
    }
}

/**
 * Twee-regelige antenne-knop — visueel afgestemd op de server- en
 * desktop-versie (`antenna_button` in `amplitec.rs` / `devices.rs`).
 * Bovenste regel: `Ant<N>` (klein, identifier).
 * Onderste regel: alias (groter, functionele naam) — alleen als de
 * alias afwijkt van de Ant<N>-default, anders wordt het visueel een
 * dubbele regel.
 *
 * Geen rename hier: labels worden alleen server-side beheerd.
 */
@Composable
private fun AmplitecAntennaButton(
    pos: Int,
    alias: String,
    enabled: Boolean,
    isActive: Boolean,
    isBlocked: Boolean,
    onClick: () -> Unit,
    modifier: Modifier = Modifier,
) {
    val posText = "Ant$pos"
    val showAlias = alias.isNotBlank() && alias != posText && alias != pos.toString()
    val colors = when {
        isActive -> ButtonDefaults.buttonColors(
            containerColor = Color(0xFF64A0E6),
            contentColor = Color.White,
        )
        isBlocked -> ButtonDefaults.outlinedButtonColors(contentColor = Color.Gray)
        else -> ButtonDefaults.outlinedButtonColors()
    }
    Button(
        onClick = onClick,
        enabled = enabled,
        colors = colors,
        contentPadding = PaddingValues(horizontal = 6.dp, vertical = 4.dp),
        modifier = modifier,
    ) {
        Column(
            horizontalAlignment = Alignment.CenterHorizontally,
            modifier = Modifier.fillMaxWidth(),
        ) {
            Text(
                posText,
                fontSize = 10.sp,
                fontWeight = FontWeight.Normal,
                maxLines = 1,
            )
            if (showAlias) {
                Text(
                    alias,
                    fontSize = 13.sp,
                    fontWeight = if (isActive) FontWeight.Bold else FontWeight.Medium,
                    maxLines = 1,
                )
            }
        }
    }
}

@Composable
private fun TunerTab(
    state: SdrUiState,
    onTunerTune: () -> Unit,
    onTunerAbort: () -> Unit,
) {
    val amber = Color(0xFFFFAA28)

    // Header
    Row(
        modifier = Modifier.fillMaxWidth(),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.SpaceBetween,
    ) {
        Text(
            stringResource(R.string.dev_tuner_title),
            style = MaterialTheme.typography.titleLarge,
            fontWeight = FontWeight.Bold,
        )
        StatusIndicator(state.tunerConnected)
    }

    HorizontalDivider(modifier = Modifier.padding(vertical = 8.dp))

    // Status
    val oliveGreen = Color(0xFF78A028)
    val stateText = when (state.tunerState) {
        1 -> "Tuning..."
        2 -> "Tune OK"
        3 -> "Timeout"
        4 -> "Aborted"
        5 -> "Done~ (already tuned)"
        else -> "Idle"
    }
    val stateColor = when (state.tunerState) {
        1 -> Color(0xFF3C78DC) // Blue
        2 -> Color(0xFF32B432) // Green
        3, 4 -> amber
        5 -> oliveGreen
        else -> MaterialTheme.colorScheme.onSurfaceVariant
    }

    Row(
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        Text(stringResource(R.string.dev_status_label))
        Text(
            stateText,
            color = stateColor,
            fontWeight = FontWeight.Bold,
            fontSize = 18.sp,
        )
    }

    Spacer(Modifier.height(16.dp))

    // Tune + Abort buttons
    val canStart = state.tunerConnected && state.tunerCanTune
            && (state.tunerState == 0 || state.tunerState == 2 || state.tunerState == 5)
    val tuneColor = when (state.tunerState) {
        1 -> Color(0xFF3C78DC) // Tuning = blue
        2 -> Color(0xFF32B432) // Done OK = green
        3, 4 -> amber // Timeout/Aborted = amber
        5 -> oliveGreen // Done assumed = olive green
        else -> Color(0xFF505050) // Idle = grey
    }
    val tuneText = when (state.tunerState) {
        1 -> "Tuning..."
        2 -> "Tune OK"
        3, 4 -> "Tune X"
        5 -> "Tune ~"
        else -> "Tune"
    }

    Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
        Button(
            onClick = onTunerTune,
            enabled = canStart,
            colors = ButtonDefaults.buttonColors(containerColor = tuneColor),
            modifier = Modifier.height(48.dp).width(140.dp),
        ) {
            Text(tuneText, color = Color.White, fontWeight = FontWeight.Bold, fontSize = 16.sp)
        }

        Button(
            onClick = onTunerAbort,
            enabled = state.tunerState == 1,
            modifier = Modifier.height(48.dp),
        ) {
            Text(stringResource(R.string.dev_abort), fontSize = 14.sp)
        }
    }

    if (!state.tunerCanTune && state.tunerConnected) {
        Spacer(Modifier.height(8.dp))
        Text(
            stringResource(R.string.dev_tuner_unavailable),
            color = amber,
            fontSize = 14.sp,
        )
    }
}

@Composable
private fun SpeExpertTab(
    state: SdrUiState,
    onSpeOperate: () -> Unit,
    onSpeTune: () -> Unit,
    onSpeAntenna: () -> Unit,
    onSpeInput: () -> Unit,
    onSpePower: () -> Unit,
    onSpeOff: () -> Unit,
    onSpePowerOn: () -> Unit,
    onSpeDriveDown: () -> Unit,
    onSpeDriveUp: () -> Unit,
) {
    val amber = Color(0xFFFFAA28)

    // Header
    Row(
        modifier = Modifier.fillMaxWidth(),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.SpaceBetween,
    ) {
        Text(
            "SPE Expert 1.3K-FA",
            style = MaterialTheme.typography.titleLarge,
            fontWeight = FontWeight.Bold,
        )
        if (state.speActive) {
            Text(stringResource(R.string.dev_active), color = Color(0xFF4CAF50), fontWeight = FontWeight.Bold, fontSize = 12.sp)
        } else {
            Text(stringResource(R.string.dev_inactive), color = Color(0xFF9E9E9E), fontSize = 12.sp)
        }
        StatusIndicator(state.speConnected)
    }

    HorizontalDivider(modifier = Modifier.padding(vertical = 8.dp))

    // Warning / Alarm (prominent, above everything)
    if (state.speAlarm != 'N'.code && state.speAlarm != 0) {
        Text(
            "ALARM: ${state.speAlarm.toChar()}",
            color = Color(0xFFF44336),
            fontWeight = FontWeight.Bold,
            fontSize = 16.sp,
        )
        Spacer(Modifier.height(4.dp))
    } else if (state.speWarning != 'N'.code && state.speWarning != 0) {
        Text(
            "Warning: ${state.speWarning.toChar()}",
            color = amber,
            fontWeight = FontWeight.Bold,
            fontSize = 16.sp,
        )
        Spacer(Modifier.height(4.dp))
    }

    // Row 1: Power On/Off | Operate (state+color) | Tune
    Row(
        horizontalArrangement = Arrangement.spacedBy(8.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        // Power — shows current state
        if (!state.speConnected || state.speState == 0) {
            Button(
                onClick = onSpePowerOn,
                colors = ButtonDefaults.buttonColors(containerColor = Color(0xFF787878)),
                modifier = Modifier.height(44.dp),
            ) {
                Text(stringResource(R.string.dev_power_off), color = Color.White, fontWeight = FontWeight.Bold)
            }
        } else {
            Button(
                onClick = onSpeOff,
                colors = ButtonDefaults.buttonColors(containerColor = Color(0xFF009600)),
                modifier = Modifier.height(44.dp),
            ) {
                Text(stringResource(R.string.dev_power_on), color = Color.White, fontWeight = FontWeight.Bold)
            }
        }

        // Operate/Standby — shows current state with color
        val (opText, opColor) = when (state.speState) {
            2 -> stringResource(R.string.dev_operate) to Color(0xFF32B432)
            1 -> stringResource(R.string.dev_standby) to amber
            else -> stringResource(R.string.dev_off) to Color(0xFF787878)
        }
        Button(
            onClick = onSpeOperate,
            enabled = state.speConnected,
            colors = ButtonDefaults.buttonColors(containerColor = opColor),
            modifier = Modifier.height(44.dp),
        ) {
            Text(opText, color = Color.White, fontWeight = FontWeight.Bold)
        }

        // Tune
        Button(
            onClick = onSpeTune,
            enabled = state.speConnected && state.speState == 2,
            modifier = Modifier.height(44.dp),
        ) {
            Text("Tune")
        }
    }

    Spacer(Modifier.height(8.dp))

    // Row 2: Ant{N} | In {N} | Low/Mid/High
    Row(
        horizontalArrangement = Arrangement.spacedBy(8.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        val bypassSuffix = if (state.speAtuBypassed) "b" else ""
        Button(
            onClick = onSpeAntenna,
            enabled = state.speConnected,
            modifier = Modifier.height(40.dp),
        ) {
            Text("Ant${state.speAntenna}$bypassSuffix")
        }

        Button(
            onClick = onSpeInput,
            enabled = state.speConnected,
            modifier = Modifier.height(40.dp),
        ) {
            Text("In ${state.speInput}")
        }

        val powerLevelText = when (state.spePowerLevel) {
            0 -> stringResource(R.string.dev_low)
            1 -> stringResource(R.string.dev_mid)
            2 -> stringResource(R.string.dev_high)
            else -> "?"
        }
        Button(
            onClick = onSpePower,
            enabled = state.speConnected,
            modifier = Modifier.height(40.dp),
        ) {
            Text(powerLevelText)
        }
    }

    Spacer(Modifier.height(4.dp))

    // Row 3: Drive -/+/%
    val driveEnabled = state.speConnected && state.speState == 2 && state.speActive
    Row(
        horizontalArrangement = Arrangement.spacedBy(8.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Button(
            onClick = onSpeDriveDown,
            enabled = driveEnabled,
            modifier = Modifier.height(40.dp),
            contentPadding = PaddingValues(horizontal = 12.dp),
        ) {
            Text(stringResource(R.string.dev_drive_down))
}
        Text(
            "${state.driveLevel}%",
            fontWeight = FontWeight.Bold,
            fontSize = 16.sp,
        )
        Button(
            onClick = onSpeDriveUp,
            enabled = driveEnabled,
            modifier = Modifier.height(40.dp),
            contentPadding = PaddingValues(horizontal = 12.dp),
        ) {
            Text(stringResource(R.string.dev_drive_up))
        }
    }

    Spacer(Modifier.height(8.dp))

    // Power bar with peak hold
    var peakPower by remember { mutableIntStateOf(0) }
    var peakTime by remember { mutableLongStateOf(0L) }
    val now = System.currentTimeMillis()
    if (state.spePowerW > peakPower) {
        peakPower = state.spePowerW
        peakTime = now
    } else if (now - peakTime > 1000) {
        peakPower = state.spePowerW
        peakTime = now
    }

    // Auto-scale: L=500W, M=1000W, H=1500W
    val maxW = when (state.spePowerLevel) {
        0 -> 500f
        1 -> 1000f
        else -> 1500f
    }
    val frac = (state.spePowerW / maxW).coerceIn(0f, 1f)
    val peakFrac = (peakPower / maxW).coerceIn(0f, 1f)
    val barColor = when {
        frac > 0.9f -> Color(0xFFFF5050)
        frac > 0.7f -> amber
        else -> Color(0xFF32B432)
    }

    Box(
        modifier = Modifier
            .fillMaxWidth()
            .height(24.dp),
    ) {
        Canvas(modifier = Modifier.fillMaxSize()) {
            // Background
            drawRect(Color(0xFF323232))
            // Fill
            drawRect(barColor, size = Size(size.width * frac, size.height))
            // Peak hold marker
            if (peakFrac > 0.01f) {
                val peakX = size.width * peakFrac
                drawLine(
                    Color.White,
                    start = Offset(peakX, 0f),
                    end = Offset(peakX, size.height),
                    strokeWidth = 3f,
                )
            }
        }
        if (state.spePowerW > 0) {
            Text(
                "${state.spePowerW}W",
                color = Color.White,
                fontWeight = FontWeight.Bold,
                fontSize = 14.sp,
                modifier = Modifier.align(Alignment.Center),
            )
        }
    }

    // Division labels
    val divisions = when (state.spePowerLevel) {
        0 -> listOf("0", "100", "200", "300", "400", "500")
        1 -> listOf("0", "200", "400", "600", "800", "1k")
        else -> listOf("0", "300", "600", "900", "1.2k", "1.5k")
    }
    Row(
        modifier = Modifier.fillMaxWidth(),
        horizontalArrangement = Arrangement.SpaceBetween,
    ) {
        divisions.forEach { label ->
            Text(label, fontSize = 9.sp, color = MaterialTheme.colorScheme.onSurfaceVariant)
        }
    }

    Spacer(Modifier.height(8.dp))
    HorizontalDivider()
    Spacer(Modifier.height(8.dp))

    // Telemetry: Band | TX/RX | W | SWR | Temp | Voltage | Current
    Row(
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(12.dp),
    ) {
        Text(speBandName(state.speBand), fontWeight = FontWeight.Bold, fontSize = 16.sp)
        if (state.spePtt) {
            Text("TX", color = Color(0xFFF44336), fontWeight = FontWeight.Bold, fontSize = 16.sp)
            val swr = state.speSwrX10 / 10f
            val swrColor = when {
                swr > 3f -> Color(0xFFF44336)
                swr > 2f -> amber
                else -> MaterialTheme.colorScheme.onSurface
            }
            Text("SWR ${String.format("%.1f", swr)}", color = swrColor, fontWeight = FontWeight.Bold)
        } else {
            Text("RX", color = Color(0xFF4CAF50), fontWeight = FontWeight.Bold, fontSize = 16.sp)
        }
    }

    Spacer(Modifier.height(4.dp))

    Row(
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(12.dp),
    ) {
        Text("${state.speTemp}\u00B0C", color = MaterialTheme.colorScheme.onSurfaceVariant)
        Text("${state.speVoltageX10 / 10f}V", color = MaterialTheme.colorScheme.onSurfaceVariant)
        Text("${String.format("%.1f", state.speCurrentX10 / 10f)}A", color = MaterialTheme.colorScheme.onSurfaceVariant)
    }
}

@Composable
private fun StatusIndicator(connected: Boolean) {
    Row(verticalAlignment = Alignment.CenterVertically) {
        val color = if (connected) Color(0xFF4CAF50) else Color(0xFFF44336)
        val text = if (connected) stringResource(R.string.dev_online) else stringResource(R.string.dev_offline)
        Text(text, fontSize = 14.sp, color = color, fontWeight = FontWeight.Bold)
    }
}

private fun speBandName(band: Int): String = when (band) {
    0 -> "160m"
    1 -> "80m"
    2 -> "60m"
    3 -> "40m"
    4 -> "30m"
    5 -> "20m"
    6 -> "17m"
    7 -> "15m"
    8 -> "12m"
    9 -> "10m"
    10 -> "6m"
    else -> "?"
}

private fun rf2kBandName(band: Int): String = when (band) {
    0 -> "6m"
    1 -> "10m"
    2 -> "12m"
    3 -> "15m"
    4 -> "17m"
    5 -> "20m"
    6 -> "30m"
    7 -> "40m"
    8 -> "60m"
    9 -> "80m"
    10 -> "160m"
    else -> "?"
}

@Composable
private fun Rf2kTab(
    state: SdrUiState,
    onRf2kOperate: (Boolean) -> Unit,
    onRf2kTune: () -> Unit,
    onRf2kAnt1: () -> Unit,
    onRf2kAnt2: () -> Unit,
    onRf2kAnt3: () -> Unit,
    onRf2kAnt4: () -> Unit,
    onRf2kAntExt: () -> Unit,
    onRf2kErrorReset: () -> Unit,
    onRf2kClose: () -> Unit,
    onRf2kDriveUp: () -> Unit,
    onRf2kDriveDown: () -> Unit,
    onRf2kTunerMode: (UByte) -> Unit,
    onRf2kTunerBypass: (Boolean) -> Unit,
    onRf2kTunerReset: () -> Unit,
    onRf2kTunerStore: () -> Unit,
    onRf2kTunerLUp: () -> Unit,
    onRf2kTunerLDown: () -> Unit,
    onRf2kTunerCUp: () -> Unit,
    onRf2kTunerCDown: () -> Unit,
    onRf2kTunerK: () -> Unit,
) {
    val amber = Color(0xFFFFAA28)
    var showFwCloseConfirm by remember { mutableStateOf(false) }

    // Header
    Row(
        modifier = Modifier.fillMaxWidth(),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.SpaceBetween,
    ) {
        val title = if (state.rf2kDeviceName.isNotEmpty()) "RF2K-S (${state.rf2kDeviceName})" else "RF2K-S"
        Text(
            title,
            style = MaterialTheme.typography.titleLarge,
            fontWeight = FontWeight.Bold,
        )
        if (state.rf2kActive) {
            Text(stringResource(R.string.dev_active), color = Color(0xFF4CAF50), fontWeight = FontWeight.Bold, fontSize = 12.sp)
        } else {
            Text(stringResource(R.string.dev_inactive), color = Color(0xFF9E9E9E), fontSize = 12.sp)
        }
        StatusIndicator(state.rf2kConnected)
    }

    HorizontalDivider(modifier = Modifier.padding(vertical = 8.dp))

    // Error bar
    if (state.rf2kErrorState != 0) {
        Row(
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            val errorText = state.rf2kErrorText.ifEmpty { stringResource(R.string.dev_error_state, state.rf2kErrorState) }
            Text(
                errorText,
                color = Color(0xFFF44336),
                fontWeight = FontWeight.Bold,
                fontSize = 14.sp,
            )
            Button(
                onClick = onRf2kErrorReset,
                colors = ButtonDefaults.buttonColors(containerColor = Color(0xFFC62828)),
                contentPadding = PaddingValues(horizontal = 12.dp, vertical = 4.dp),
            ) {
                Text(stringResource(R.string.dev_reset), color = Color.White)
            }
        }
        Spacer(Modifier.height(4.dp))
    }

    // Row 1: Operate/Standby + Tune + FW Close
    Row(
        horizontalArrangement = Arrangement.spacedBy(8.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        val (opText, opColor) = if (state.rf2kOperate) {
            stringResource(R.string.dev_operate) to Color(0xFF32B432)
        } else {
            stringResource(R.string.dev_standby) to amber
        }
        Button(
            onClick = { onRf2kOperate(!state.rf2kOperate) },
            enabled = state.rf2kConnected,
            colors = ButtonDefaults.buttonColors(containerColor = opColor),
            modifier = Modifier.height(44.dp),
        ) {
            Text(opText, color = Color.White, fontWeight = FontWeight.Bold)
        }

        Button(
            onClick = onRf2kTune,
            enabled = state.rf2kConnected && state.rf2kOperate,
            modifier = Modifier.height(44.dp),
        ) {
            Text("Tune")
        }

        Spacer(Modifier.weight(1f))

        Button(
            onClick = { showFwCloseConfirm = true },
            enabled = state.rf2kConnected,
            colors = ButtonDefaults.buttonColors(containerColor = Color(0xFFC62828)),
            modifier = Modifier.height(44.dp),
        ) {
            Text(stringResource(R.string.dev_fw_close), color = Color.White)
        }
    }

    // FW Close confirmation dialog
    if (showFwCloseConfirm) {
        AlertDialog(
            onDismissRequest = { showFwCloseConfirm = false },
            title = { Text(stringResource(R.string.dev_fw_close_confirm_title)) },
            text = { Text(stringResource(R.string.dev_fw_close_confirm_body)) },
            confirmButton = {
                Button(
                    onClick = {
                        onRf2kClose()
                        showFwCloseConfirm = false
                    },
                    colors = ButtonDefaults.buttonColors(containerColor = Color(0xFFC62828)),
                ) { Text(stringResource(R.string.common_yes), color = Color.White) }
            },
            dismissButton = {
                Button(onClick = { showFwCloseConfirm = false }) { Text(stringResource(R.string.common_no)) }
            },
        )
    }

    Spacer(Modifier.height(4.dp))

    // Power bar with peak hold
    var peakPower by remember { mutableIntStateOf(0) }
    var peakTime by remember { mutableLongStateOf(0L) }
    val now = System.currentTimeMillis()
    if (state.rf2kForwardW > peakPower) {
        peakPower = state.rf2kForwardW
        peakTime = now
    } else if (now - peakTime > 1000) {
        peakPower = state.rf2kForwardW
        peakTime = now
    }

    // Auto-scale: 200, 500, 1000, 1500W
    val maxW = when {
        state.rf2kMaxForwardW > 1000 -> 1500f
        state.rf2kMaxForwardW > 500 -> 1000f
        state.rf2kMaxForwardW > 200 -> 500f
        else -> 200f
    }
    val frac = (state.rf2kForwardW / maxW).coerceIn(0f, 1f)
    val peakFrac = (peakPower / maxW).coerceIn(0f, 1f)
    val barColor = when {
        frac > 0.9f -> Color(0xFFFF5050)
        frac > 0.7f -> amber
        else -> Color(0xFF32B432)
    }

    Box(
        modifier = Modifier
            .fillMaxWidth()
            .height(24.dp),
    ) {
        Canvas(modifier = Modifier.fillMaxSize()) {
            drawRect(Color(0xFF323232))
            drawRect(barColor, size = Size(size.width * frac, size.height))
            if (peakFrac > 0.01f) {
                val peakX = size.width * peakFrac
                drawLine(
                    Color.White,
                    start = Offset(peakX, 0f),
                    end = Offset(peakX, size.height),
                    strokeWidth = 3f,
                )
            }
        }
        if (state.rf2kForwardW > 0) {
            Text(
                "${state.rf2kForwardW}W",
                color = Color.White,
                fontWeight = FontWeight.Bold,
                fontSize = 14.sp,
                modifier = Modifier.align(Alignment.Center),
            )
        }
    }

    // Division labels
    val step = (maxW / 5).toInt()
    Row(
        modifier = Modifier.fillMaxWidth(),
        horizontalArrangement = Arrangement.SpaceBetween,
    ) {
        for (i in 0..5) {
            val watts = step * i
            val label = if (watts >= 1000) "${watts / 1000}k" else "$watts"
            Text(label, fontSize = 9.sp, color = MaterialTheme.colorScheme.onSurfaceVariant)
        }
    }

    Spacer(Modifier.height(4.dp))

    // Tuner controls
    val tunerEditEnabled = state.rf2kConnected && !state.rf2kOperate && state.rf2kForwardW < 30
    val isManual = state.rf2kTunerMode == 2
    val tunerModeText = when (state.rf2kTunerMode) {
        0 -> "OFF"
        1 -> "BYP"
        2 -> "MAN"
        3, 5 -> "TUNING"
        4 -> "AUTO"
        else -> "?"
    }
    val tunerColor = when (state.rf2kTunerMode) {
        3, 5 -> amber
        4 -> Color(0xFF32B432)
        2 -> Color(0xFF64A0FF)
        else -> MaterialTheme.colorScheme.onSurface
    }
    Row(
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(4.dp),
    ) {
        Text(stringResource(R.string.dev_tuner_label, tunerModeText), color = tunerColor, fontWeight = FontWeight.Bold, fontSize = 13.sp)
        // MAN/AUTO toggle — shows current state
        if (state.rf2kTunerMode == 2 || state.rf2kTunerMode == 4) {
            val toggleText = if (isManual) stringResource(R.string.dev_manual) else stringResource(R.string.dev_auto)
            Button(
                onClick = { onRf2kTunerMode(if (isManual) 1u else 0u) },
                enabled = tunerEditEnabled,
                colors = ButtonDefaults.buttonColors(containerColor = Color(0xFF64A0E6)),
                contentPadding = PaddingValues(horizontal = 8.dp, vertical = 0.dp),
                modifier = Modifier.height(28.dp),
            ) { Text(toggleText, fontSize = 11.sp, fontWeight = FontWeight.Bold, color = Color.White) }
        }
        // Bypass — shows current state with color
        val isBypass = state.rf2kTunerMode == 1 || state.rf2kTunerSetup == "BYPASS"
        val bypColor = if (isBypass) Color(0xFFFFAA28) else ButtonDefaults.buttonColors().containerColor
        Button(
            onClick = { onRf2kTunerBypass(!isBypass) },
            enabled = tunerEditEnabled,
            colors = ButtonDefaults.buttonColors(containerColor = bypColor),
            contentPadding = PaddingValues(horizontal = 8.dp, vertical = 0.dp),
            modifier = Modifier.height(28.dp),
        ) {
            Text(stringResource(R.string.dev_bypass), fontSize = 11.sp,
                fontWeight = if (isBypass) FontWeight.Bold else FontWeight.Normal,
                color = if (isBypass) Color.White else Color.Unspecified)
        }
        // Reset + Store (manual only)
        Button(
            onClick = onRf2kTunerReset,
            enabled = tunerEditEnabled && isManual,
            contentPadding = PaddingValues(horizontal = 8.dp, vertical = 0.dp),
            modifier = Modifier.height(28.dp),
        ) { Text(stringResource(R.string.dev_reset), fontSize = 11.sp) }
        Button(
            onClick = onRf2kTunerStore,
            enabled = tunerEditEnabled && isManual,
            contentPadding = PaddingValues(horizontal = 8.dp, vertical = 0.dp),
            modifier = Modifier.height(28.dp),
        ) { Text(stringResource(R.string.dev_store), fontSize = 11.sp) }
    }

    // Manual L/C/K controls
    if (isManual) {
        Row(
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(4.dp),
        ) {
            if (state.rf2kTunerSetup.isNotEmpty()) {
                Text(state.rf2kTunerSetup, fontSize = 12.sp)
            }
            Button(
                onClick = onRf2kTunerK,
                enabled = tunerEditEnabled,
                contentPadding = PaddingValues(horizontal = 8.dp, vertical = 0.dp),
                modifier = Modifier.height(28.dp),
            ) { Text("K", fontSize = 11.sp) }
            Text("L:${state.rf2kTunerLNh}", fontSize = 12.sp)
            Button(
                onClick = onRf2kTunerLDown,
                enabled = tunerEditEnabled,
                contentPadding = PaddingValues(horizontal = 6.dp, vertical = 0.dp),
                modifier = Modifier.height(28.dp),
            ) { Text("−", fontSize = 11.sp) }
            Button(
                onClick = onRf2kTunerLUp,
                enabled = tunerEditEnabled,
                contentPadding = PaddingValues(horizontal = 6.dp, vertical = 0.dp),
                modifier = Modifier.height(28.dp),
            ) { Text("+", fontSize = 11.sp) }
            Text("C:${state.rf2kTunerCPf}", fontSize = 12.sp)
            Button(
                onClick = onRf2kTunerCDown,
                enabled = tunerEditEnabled,
                contentPadding = PaddingValues(horizontal = 6.dp, vertical = 0.dp),
                modifier = Modifier.height(28.dp),
            ) { Text("−", fontSize = 11.sp) }
            Button(
                onClick = onRf2kTunerCUp,
                enabled = tunerEditEnabled,
                contentPadding = PaddingValues(horizontal = 6.dp, vertical = 0.dp),
                modifier = Modifier.height(28.dp),
            ) { Text("+", fontSize = 11.sp) }
        }
    } else {
        Row(
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            if (state.rf2kTunerSetup.isNotEmpty()) {
                Text(state.rf2kTunerSetup, fontSize = 12.sp)
            }
            if (state.rf2kTunerLNh > 0 || state.rf2kTunerCPf > 0) {
                Text("L:${state.rf2kTunerLNh}nH C:${state.rf2kTunerCPf}pF", fontSize = 12.sp)
            }
        }
    }

    Spacer(Modifier.height(4.dp))

    // Drive row
    Row(
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        val modColor = when (state.rf2kModulation) {
            "SSB" -> Color(0xFF64A0FF)
            "AM" -> amber
            "CONT" -> Color(0xFF32B432)
            else -> MaterialTheme.colorScheme.onSurface
        }
        if (state.rf2kModulation.isNotEmpty()) {
            Text(state.rf2kModulation, color = modColor, fontWeight = FontWeight.Bold)
        }
        Text(stringResource(R.string.dev_drive_watts, state.rf2kDriveW), fontWeight = FontWeight.Bold, fontSize = 16.sp)

        val driveEnabled = state.rf2kConnected && state.rf2kOperate && state.rf2kActive
        Button(
            onClick = onRf2kDriveDown,
            enabled = driveEnabled,
            modifier = Modifier.height(36.dp),
            contentPadding = PaddingValues(horizontal = 12.dp),
        ) {
            Text("-")
        }
        Button(
            onClick = onRf2kDriveUp,
            enabled = driveEnabled,
            modifier = Modifier.height(36.dp),
            contentPadding = PaddingValues(horizontal = 12.dp),
        ) {
            Text("+")
        }
    }

    Spacer(Modifier.height(8.dp))
    HorizontalDivider()
    Spacer(Modifier.height(8.dp))

    // Telemetry
    Row(
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(12.dp),
    ) {
        val swr = state.rf2kSwrX100 / 100f
        val swrColor = when {
            swr > 3f -> Color(0xFFF44336)
            swr > 2f -> amber
            else -> MaterialTheme.colorScheme.onSurface
        }
        Text("SWR ${String.format("%.2f", swr)}", color = swrColor, fontWeight = FontWeight.Bold)
        Text("${String.format("%.1f", state.rf2kTemperatureX10 / 10f)}\u00B0C", color = MaterialTheme.colorScheme.onSurfaceVariant)
        Text("${String.format("%.1f", state.rf2kVoltageX10 / 10f)}V", color = MaterialTheme.colorScheme.onSurfaceVariant)
        Text("${String.format("%.1f", state.rf2kCurrentX10 / 10f)}A", color = MaterialTheme.colorScheme.onSurfaceVariant)
    }

    if (state.rf2kReflectedW > 0) {
        Spacer(Modifier.height(4.dp))
        Text(stringResource(R.string.dev_reflected, state.rf2kReflectedW), color = MaterialTheme.colorScheme.onSurfaceVariant)
    }

    Spacer(Modifier.height(12.dp))

    // Antenna selection (at bottom)
    Row(
        horizontalArrangement = Arrangement.spacedBy(4.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text(stringResource(R.string.dev_antenna_label), fontWeight = FontWeight.Bold, fontSize = 13.sp)
        val intAnt = state.rf2kAntennaType == 0
        for ((nr, onClick) in listOf(1 to onRf2kAnt1, 2 to onRf2kAnt2, 3 to onRf2kAnt3, 4 to onRf2kAnt4)) {
            val isActive = intAnt && state.rf2kAntennaNumber == nr
            Button(
                onClick = onClick,
                enabled = state.rf2kConnected,
                colors = if (isActive) {
                    ButtonDefaults.buttonColors(containerColor = Color(0xFF32B432))
                } else {
                    ButtonDefaults.outlinedButtonColors()
                },
                contentPadding = PaddingValues(horizontal = 8.dp, vertical = 0.dp),
                modifier = Modifier.defaultMinSize(minHeight = 32.dp),
            ) {
                Text("$nr", fontWeight = if (isActive) FontWeight.Bold else FontWeight.Normal)
            }
        }

        val extActive = state.rf2kAntennaType == 1
        Button(
            onClick = onRf2kAntExt,
            enabled = state.rf2kConnected,
            colors = if (extActive) {
                ButtonDefaults.buttonColors(containerColor = Color(0xFF32B432))
            } else {
                ButtonDefaults.outlinedButtonColors()
            },
            contentPadding = PaddingValues(horizontal = 8.dp, vertical = 0.dp),
            modifier = Modifier.defaultMinSize(minHeight = 32.dp),
        ) {
            Text("Ext", fontWeight = if (extActive) FontWeight.Bold else FontWeight.Normal)
        }

        Spacer(Modifier.width(6.dp))
        Text(rf2kBandName(state.rf2kBand), fontWeight = FontWeight.Bold, fontSize = 16.sp)
        if (state.rf2kFrequencyKhz > 0) {
            Text("${state.rf2kFrequencyKhz} kHz", fontSize = 14.sp)
        }
    }
}

private fun ubBandName(band: Int): String = when (band) {
    0 -> "6m"
    1 -> "10m"
    2 -> "12m"
    3 -> "15m"
    4 -> "17m"
    5 -> "20m"
    6 -> "30m"
    7 -> "40m"
    8 -> "60m"
    9 -> "80m"
    10 -> "160m"
    else -> "?"
}

@Composable
/**
 * Determine which VFO frequency to use for UltraBeam tracking,
 * based on which Amplitec port has the UltraBeam connected.
 * Returns (frequencyHz, label) — e.g. (14200000, "VFO A") or (7100000, "VFO B").
 * Falls back to VFO A if no Amplitec active or UltraBeam not found in labels.
 */
private fun ubTrackVfo(state: SdrUiState): Pair<Long, String> {
    if (state.amplitecLabels.isNotEmpty()) {
        val parts = state.amplitecLabels.split(",")
        for (i in 0 until 6) {
            if (i < parts.size) {
                val lower = parts[i].lowercase()
                if ("ultrabeam" in lower || "ultra beam" in lower || "ub" in lower) {
                    val ubPos = (i + 1)
                    if (state.amplitecSwitchB == ubPos) {
                        return Pair(state.frequencyRx2Hz, "VFO B")
                    }
                    if (state.amplitecSwitchA == ubPos) {
                        return Pair(state.frequencyHz, "VFO A")
                    }
                    break
                }
            }
        }
    }
    return Pair(state.frequencyHz, "VFO A")
}

@Composable
private fun UltraBeamTab(
    state: SdrUiState,
    onUbSetFrequency: (Int, Int) -> Unit,
    onUbRetract: () -> Unit,
    onUbReadElements: () -> Unit,
) {
    var showRetractConfirm by remember { mutableStateOf(false) }
    var ubAutoTrack by remember { mutableStateOf(false) }
    var ubLastAutoKhz by remember { mutableIntStateOf(0) }

    // Auto-track: send UltraBeam frequency when VFO changes >= 25 kHz
    if (ubAutoTrack && state.ubConnected) {
        val (trackHz, _) = ubTrackVfo(state)
        val trackKhz = (trackHz / 1000).toInt()
        val diff = kotlin.math.abs(trackKhz - ubLastAutoKhz)
        if (trackKhz in 1800..54000 && diff >= 25) {
            LaunchedEffect(trackKhz) {
                ubLastAutoKhz = trackKhz
                onUbSetFrequency(trackKhz, state.ubDirection)
            }
        }
    }

    // Header
    Row(
        modifier = Modifier.fillMaxWidth(),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.SpaceBetween,
    ) {
        Text(
            "UltraBeam RCU-06",
            style = MaterialTheme.typography.titleLarge,
            fontWeight = FontWeight.Bold,
        )
        if (state.ubFwMajor > 0) {
            Text(
                "FW ${state.ubFwMajor}.${state.ubFwMinor}",
                fontSize = 12.sp,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }
        StatusIndicator(state.ubConnected)
    }

    HorizontalDivider(modifier = Modifier.padding(vertical = 8.dp))

    // Frequency display
    if (state.ubFrequencyKhz > 0) {
        Row(
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            val freqMhz = state.ubFrequencyKhz / 1000f
            Text(
                "${String.format("%.3f", freqMhz)} MHz",
                fontSize = 28.sp,
                fontWeight = FontWeight.Bold,
            )
            Text(
                ubBandName(state.ubBand),
                fontSize = 20.sp,
            )
        }
        Spacer(Modifier.height(8.dp))
    }

    // Direction buttons
    Row(
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        Text(stringResource(R.string.dev_ub_dir), fontWeight = FontWeight.Bold)
        val dirs = listOf(stringResource(R.string.dev_ub_normal) to 0, "180\u00B0" to 1, stringResource(R.string.dev_ub_bidir) to 2)
        for ((label, dir) in dirs) {
            val isActive = state.ubDirection == dir
            Button(
                onClick = { onUbSetFrequency(state.ubFrequencyKhz, dir) },
                enabled = state.ubConnected,
                colors = if (isActive) {
                    ButtonDefaults.buttonColors(containerColor = Color(0xFF32B432))
                } else {
                    ButtonDefaults.outlinedButtonColors()
                },
                contentPadding = PaddingValues(horizontal = 12.dp, vertical = 4.dp),
            ) {
                Text(label, fontWeight = if (isActive) FontWeight.Bold else FontWeight.Normal)
            }
        }
    }

    Spacer(Modifier.height(8.dp))

    // Sync VFO + Auto track
    Row(
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        val (trackHz, trackLabel) = ubTrackVfo(state)
        val trackKhz = (trackHz / 1000).toInt()

        Button(
            onClick = {
                if (trackKhz in 1800..54000) {
                    ubLastAutoKhz = trackKhz
                    onUbSetFrequency(trackKhz, state.ubDirection)
                }
            },
            enabled = state.ubConnected && trackKhz in 1800..54000,
            contentPadding = PaddingValues(horizontal = 12.dp, vertical = 4.dp),
        ) {
            Text(stringResource(R.string.dev_ub_sync, trackLabel))
        }

        Row(verticalAlignment = Alignment.CenterVertically) {
            Checkbox(
                checked = ubAutoTrack,
                onCheckedChange = {
                    ubAutoTrack = it
                    if (it) {
                        // Initialize last auto khz to current to prevent immediate jump
                        ubLastAutoKhz = trackKhz
                    }
                },
            )
            Text(stringResource(R.string.dev_auto), fontSize = 14.sp)
        }
    }

    Spacer(Modifier.height(8.dp))

    // Frequency step buttons
    Row(
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(4.dp),
    ) {
        Text(stringResource(R.string.dev_step), fontWeight = FontWeight.Bold)
        val steps = listOf("-100" to -100, "-25" to -25, "+25" to 25, "+100" to 100)
        for ((label, step) in steps) {
            Button(
                onClick = {
                    val newKhz = (state.ubFrequencyKhz + step).coerceIn(1800, 54000)
                    onUbSetFrequency(newKhz, state.ubDirection)
                },
                enabled = state.ubConnected && state.ubFrequencyKhz > 0,
                contentPadding = PaddingValues(horizontal = 8.dp, vertical = 4.dp),
                modifier = Modifier.weight(1f),
            ) {
                Text(label, fontSize = 12.sp)
            }
        }
    }

    // Per-motor moving + progress bar (alleen tonen bij beweging).
    // ubMotorsMoving is een bitfield: bit 0 = motor 1, bit 1 = motor 2.
    // De progress-balk is een gedeelde waarde; de RCU-06 deelt geen
    // afzonderlijke voortgang per motor.
    if (state.ubMotorsMoving != 0) {
        Spacer(Modifier.height(8.dp))
        val progress = (state.ubMotorCompletion / 60f).coerceIn(0f, 1f)
        val m1Active = (state.ubMotorsMoving.toInt() and 0x01) != 0
        val m2Active = (state.ubMotorsMoving.toInt() and 0x02) != 0
        val activeColor = Color(0xFFFFAA28)
        val idleColor = Color(0xFF646464)
        Row(
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            Text(
                "M1",
                color = if (m1Active) activeColor else idleColor,
                fontWeight = FontWeight.Bold,
            )
            Text(
                "M2",
                color = if (m2Active) activeColor else idleColor,
                fontWeight = FontWeight.Bold,
            )
            LinearProgressIndicator(
                progress = { progress },
                modifier = Modifier.weight(1f).height(12.dp),
            )
            Text("${(progress * 100).toInt()}%", fontSize = 12.sp)
        }
    }

    Spacer(Modifier.height(8.dp))

    // Band presets
    Text(stringResource(R.string.dev_band), fontWeight = FontWeight.Bold)
    Spacer(Modifier.height(4.dp))
    val presets = listOf(
        "40m" to 7100, "30m" to 10125, "20m" to 14175, "17m" to 18118,
        "15m" to 21225, "12m" to 24940, "10m" to 28500, "6m" to 50150,
    )
    // Two rows of band presets
    Row(horizontalArrangement = Arrangement.spacedBy(4.dp)) {
        for ((name, centerKhz) in presets.take(4)) {
            Button(
                onClick = { onUbSetFrequency(centerKhz, state.ubDirection) },
                enabled = state.ubConnected,
                contentPadding = PaddingValues(horizontal = 8.dp, vertical = 4.dp),
                modifier = Modifier.weight(1f),
            ) {
                Text(name, fontSize = 11.sp)
            }
        }
    }
    Spacer(Modifier.height(4.dp))
    Row(horizontalArrangement = Arrangement.spacedBy(4.dp)) {
        for ((name, centerKhz) in presets.drop(4)) {
            Button(
                onClick = { onUbSetFrequency(centerKhz, state.ubDirection) },
                enabled = state.ubConnected,
                contentPadding = PaddingValues(horizontal = 8.dp, vertical = 4.dp),
                modifier = Modifier.weight(1f),
            ) {
                Text(name, fontSize = 11.sp)
            }
        }
    }

    Spacer(Modifier.height(8.dp))

    // Retract + Read Elements
    Row(
        horizontalArrangement = Arrangement.spacedBy(8.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Button(
            onClick = { showRetractConfirm = true },
            enabled = state.ubConnected,
            colors = ButtonDefaults.buttonColors(containerColor = Color(0xFFC62828)),
        ) {
            Text(stringResource(R.string.dev_retract), color = Color.White)
}

        Button(
            onClick = onUbReadElements,
            enabled = state.ubConnected,
        ) {
            Text(stringResource(R.string.dev_read_elements))
        }
    }

    // Retract confirmation dialog
    if (showRetractConfirm) {
        AlertDialog(
            onDismissRequest = { showRetractConfirm = false },
            title = { Text(stringResource(R.string.dev_retract_confirm_title)) },
            text = { Text(stringResource(R.string.dev_retract_confirm_body)) },
            confirmButton = {
                Button(
                    onClick = {
                        onUbRetract()
                        showRetractConfirm = false
                    },
                    colors = ButtonDefaults.buttonColors(containerColor = Color(0xFFC62828)),
                ) { Text(stringResource(R.string.common_yes), color = Color.White) }
            },
            dismissButton = {
                Button(onClick = { showRetractConfirm = false }) { Text(stringResource(R.string.common_no)) }
            },
        )
    }

    // Element lengths (read-only)
    if (state.ubElementsMm.any { it > 0 }) {
        Spacer(Modifier.height(8.dp))
        HorizontalDivider()
        Spacer(Modifier.height(8.dp))
        Text(stringResource(R.string.dev_element_lengths), fontWeight = FontWeight.Bold)
        Spacer(Modifier.height(4.dp))
        Row(
            horizontalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            state.ubElementsMm.forEachIndexed { i, mm ->
                Text("E${i + 1}: $mm", fontSize = 13.sp)
            }
        }
    }
}

@Composable
private fun RotorTab(
    state: SdrUiState,
    onGoTo: (Int) -> Unit,
    onStop: () -> Unit,
    onCw: () -> Unit,
    onCcw: () -> Unit,
) {
    var gotoInput by remember { mutableStateOf("") }
    val connected = state.rotorConnected
    val angleDeg = state.rotorAngleX10 / 10f
    val targetDeg = if (state.rotorRotating) state.rotorTargetX10 / 10f else null

    Column(
        modifier = Modifier.fillMaxWidth(),
        horizontalAlignment = Alignment.CenterHorizontally,
    ) {
        // Header
        Row(
            modifier = Modifier.fillMaxWidth(),
            horizontalArrangement = Arrangement.SpaceBetween,
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Text("Rotor", fontSize = 20.sp, fontWeight = FontWeight.Bold)
            Text(
                if (connected) stringResource(R.string.dev_online) else stringResource(R.string.dev_offline),
                color = if (connected) Color(0xFF4CAF50) else Color(0xFFF44336),
                fontWeight = FontWeight.Bold,
            )
        }

        Spacer(Modifier.height(12.dp))

        // Compass circle
        CompassCircle(
            angleDeg = angleDeg,
            targetDeg = targetDeg,
            connected = connected,
            onGoTo = onGoTo,
        )

        Spacer(Modifier.height(12.dp))

        // Stop + GoTo
        Row(
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            Button(
                onClick = onStop,
                enabled = connected,
                colors = ButtonDefaults.buttonColors(containerColor = MaterialTheme.colorScheme.error),
            ) { Text(stringResource(R.string.dev_stop)) }

            Text(stringResource(R.string.dev_goto))
            OutlinedTextField(
                value = gotoInput,
                onValueChange = { gotoInput = it },
                modifier = Modifier.width(80.dp),
                keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Decimal),
                singleLine = true,
            )
            Button(
                onClick = {
                    val deg = gotoInput.toFloatOrNull()
                    if (deg != null && deg in 0f..360f) {
                        onGoTo((deg * 10).toInt())
                    }
                },
                enabled = connected,
            ) { Text(stringResource(R.string.dev_go)) }
        }
    }
}

@Composable
private fun CompassCircle(
    angleDeg: Float,
    targetDeg: Float?,
    connected: Boolean,
    onGoTo: (Int) -> Unit,
) {
    val textMeasurer = rememberTextMeasurer()
    val needleColor = Color(0xFF32C832)
    val targetColor = Color(0xFFFFC828)
    val ringColor = Color(0xFF888888)
    val tickColor = Color(0xFF666666)
    val northColor = Color(0xFFFF5050)
    val labelColor = Color(0xFFAAAAAA)
    val angleTextColor = Color.White

    Canvas(
        modifier = Modifier
            .size(260.dp)
            .pointerInput(connected) {
                if (!connected) return@pointerInput
                detectTapGestures { offset ->
                    val cx = size.width / 2f
                    val cy = size.height / 2f
                    val dx = offset.x - cx
                    val dy = offset.y - cy
                    val dist = sqrt(dx * dx + dy * dy)
                    if (dist > 30f) {
                        var deg = Math.toDegrees(atan2(dy.toDouble(), dx.toDouble())).toFloat() + 90f
                        if (deg < 0f) deg += 360f
                        if (deg >= 360f) deg -= 360f
                        val angleX10 = (deg * 10f).toInt().coerceIn(0, 3600)
                        onGoTo(angleX10)
                    }
                }
            }
    ) {
        val cx = size.width / 2f
        val cy = size.height / 2f
        val radius = size.minDimension * 0.42f

        // Background circle
        drawCircle(color = Color(0xFF1A1A1A), radius = radius + 4f, center = Offset(cx, cy))
        drawCircle(
            color = ringColor,
            radius = radius,
            center = Offset(cx, cy),
            style = Stroke(width = 2f),
        )

        // Ticks every 30 degrees
        for (i in 0 until 12) {
            val deg = i * 30f
            val rad = Math.toRadians((deg - 90f).toDouble())
            val cosR = cos(rad).toFloat()
            val sinR = sin(rad).toFloat()
            val outerR = radius
            val innerR = if (deg % 90f == 0f) radius - 12f else radius - 7f
            val strokeW = if (deg % 90f == 0f) 1.5f else 0.8f
            drawLine(
                color = tickColor,
                start = Offset(cx + cosR * innerR, cy + sinR * innerR),
                end = Offset(cx + cosR * outerR, cy + sinR * outerR),
                strokeWidth = strokeW,
            )
        }

        // N/E/S/W labels
        val labels = listOf("N" to 0f, "E" to 90f, "S" to 180f, "W" to 270f)
        for ((label, deg) in labels) {
            val rad = Math.toRadians((deg - 90f).toDouble())
            val lx = cx + cos(rad).toFloat() * (radius + 18f)
            val ly = cy + sin(rad).toFloat() * (radius + 18f)
            val color = if (label == "N") northColor else labelColor
            val result = textMeasurer.measure(
                label,
                style = TextStyle(fontSize = 14.sp, fontWeight = FontWeight.Bold, color = color),
            )
            drawText(
                result,
                topLeft = Offset(lx - result.size.width / 2f, ly - result.size.height / 2f),
            )
        }

        // Target line
        if (targetDeg != null) {
            val rad = Math.toRadians((targetDeg - 90f).toDouble())
            val cosR = cos(rad).toFloat()
            val sinR = sin(rad).toFloat()
            drawLine(
                color = targetColor,
                start = Offset(cx + cosR * radius * 0.3f, cy + sinR * radius * 0.3f),
                end = Offset(cx + cosR * (radius - 14f), cy + sinR * (radius - 14f)),
                strokeWidth = 3f,
                cap = StrokeCap.Round,
            )
        }

        // Current angle needle
        val needleRad = Math.toRadians((angleDeg - 90f).toDouble())
        val needleCos = cos(needleRad).toFloat()
        val needleSin = sin(needleRad).toFloat()
        drawLine(
            color = needleColor,
            start = Offset(cx, cy),
            end = Offset(cx + needleCos * (radius - 6f), cy + needleSin * (radius - 6f)),
            strokeWidth = 4f,
            cap = StrokeCap.Round,
        )
        drawCircle(color = needleColor, radius = 6f, center = Offset(cx, cy))

        // Angle text below center
        val angleText = "%.1f\u00B0".format(angleDeg)
        val angleResult = textMeasurer.measure(
            angleText,
            style = TextStyle(fontSize = 20.sp, fontWeight = FontWeight.Bold, color = angleTextColor),
        )
        drawText(
            angleResult,
            topLeft = Offset(cx - angleResult.size.width / 2f, cy + radius * 0.45f),
        )
    }
}

// ========== Yaesu FT-991A ==========

@Composable
private fun YaesuTab(
    fullState: SdrUiState,
    onEnable: (Boolean) -> Unit,
    onPtt: (Boolean) -> Unit,
    onVolume: (Float) -> Unit,
    onSelectVfo: (Int) -> Unit,
    onMode: (Int) -> Unit,
    onButton: (Int) -> Unit,
    onRecallMemory: (Int) -> Unit,
    onControl: (Int, Int) -> Unit, // (controlId, value)
    onFreqChange: (Long) -> Unit = {},
    yaesuActive: Boolean = false,
    onEqBand: (Int, Float) -> Unit = { _, _ -> },
    onEqEnabled: (Boolean) -> Unit = {},
    onTxGain: (Float) -> Unit = {},
    onSelectRadio: (Int) -> Unit = {},
    onDspControl: (Int, Int) -> Unit = { _, _ -> }, // getypt YaesuControl-kanaal (control, value)
    onCompressor: (Int) -> Unit = {}, // client-side spraakcompressor 0-100 (gedeeld)
    onTxAgc: (Boolean) -> Unit = {},  // client-side TX-AGC aan/uit (gedeeld)
    onPowerOnOff: (Boolean) -> Unit = {}, // radio power on/off (CAT PS) - alleen 991A klikbaar
) {
    // Android bedient één Yaesu-radio tegelijk. Bij radio2 tonen we de yaesu2_*-state
    // via het bestaande yaesu_*-veld, zodat de rest van deze composable ongewijzigd blijft.
    // De on*-callbacks routeren al naar de geselecteerde radio (ViewModel *Sel-functies).
    val state = if (fullState.selectedRadio == 1) fullState.copy(
        yaesuConnected = fullState.yaesu2Connected,
        yaesuModel = fullState.yaesu2Model,
        yaesuTunerState = fullState.yaesu2TunerState,
        yaesuHiSwr = fullState.yaesu2HiSwr,
        yaesuFreqA = fullState.yaesu2FreqA,
        yaesuFreqB = fullState.yaesu2FreqB,
        yaesuMode = fullState.yaesu2Mode,
        yaesuSmeter = fullState.yaesu2Smeter,
        yaesuTxActive = fullState.yaesu2TxActive,
        yaesuPowerOn = fullState.yaesu2PowerOn,
        yaesuAfGain = fullState.yaesu2AfGain,
        yaesuTxPower = fullState.yaesu2TxPower,
        yaesuTxPowerMax = fullState.yaesu2TxPowerMax,
        yaesuSquelch = fullState.yaesu2Squelch,
        yaesuRfGain = fullState.yaesu2RfGain,
        yaesuMicGain = fullState.yaesu2MicGain,
        yaesuVfoSelect = fullState.yaesu2VfoSelect,
        yaesuMemoryChannel = fullState.yaesu2MemoryChannel,
        yaesuSplit = fullState.yaesu2Split,
        yaesuScan = fullState.yaesu2Scan,
        yaesuMemoryData = fullState.yaesu2MemoryData,
        yaesuFeatureToggles = fullState.yaesu2FeatureToggles,
        yaesuFeatureLevels = fullState.yaesu2FeatureLevels,
        yaesuFeatureFreqs = fullState.yaesu2FeatureFreqs,
    ) else fullState
    val context = LocalContext.current
    val modeNames = listOf("LSB", "USB", "DSB", "CW-L", "CW-U", "FM", "AM", "DIGU", "SPEC", "DIGL", "SAM", "DRM", "C4FM")
    val modeName = modeNames.getOrElse(state.yaesuMode) { "?" }
    val vfoLabel = when (state.yaesuVfoSelect) { 0 -> "VFO"; 1 -> "MEM"; 2 -> "M-Tune"; else -> "?" }
    val isMemory = state.yaesuVfoSelect == 1

    // Parse memory data (tab-separated, header: Ch, RxFreq, TxFreq, Offset, Dir, Mode, TxMode, Name, Tone, CTCSS, ...)
    val memData = state.yaesuMemoryData
    val isMenuData = memData.startsWith("MENU:")
    data class YaesuMem(
        val ch: String, val name: String, val rxFreq: String, val mode: String,
        val dir: String, val tone: String,
        /// Everything beyond frequency/mode/name that the source actually
        /// reported, pre-formatted as one line. Empty columns are left out:
        /// the radio reports only part of these over CAT, and "not reported"
        /// must not read as "Off".
        val details: String,
    )
    val memChannels = remember(memData) {
        if (memData.isNotEmpty() && !isMenuData) {
            val rows = memData.lines()
            // Column positions come from the header line, so the server can add
            // or reorder columns without silently shifting what is shown here.
            val colIndex = rows.firstOrNull()
                ?.split("	")
                ?.withIndex()
                ?.associate { (i, n) -> n.trim().lowercase() to i }
                ?: emptyMap()
            rows.drop(1).mapNotNull { line -> // drop header
                val p = line.split("	")
                if (p.size >= 8) {
                    fun col(header: String) =
                        colIndex[header.lowercase()]?.let { p.getOrElse(it) { "" }.trim() } ?: ""
                    val ch = col("Channel Number")
                    val rxFreq = col("Receive Frequency")
                    val mode = col("Operating Mode")
                    val name = col("Name")
                    val dir = col("Offset Direction")
                    val tone = col("Tone Mode")
                    // Repeater shift and CTCSS/DCS only apply in the FM-ish
                    // modes; elsewhere the stored tone mode does nothing.
                    val usesTone = mode in setOf("FM", "FM-N", "DATA-FM", "C4FM")
                    val details = buildList {
                        if (usesTone && tone.isNotEmpty() && tone != "None") {
                            val toneValue = when {
                                tone.startsWith("Tone") || tone == "T SQL" -> col("CTCSS")
                                tone.startsWith("DCS") || tone == "D Code" -> col("DCS")
                                else -> ""
                            }
                            add(if (toneValue.isNotEmpty()) "$tone $toneValue" else tone)
                        }
                        if (usesTone && dir.isNotEmpty() && dir != "Simplex") {
                            add("Shift $dir ${col("Offset Frequency")}".trim())
                        }
                        for ((label, header) in EXTRA_MEM_COLUMNS) {
                            val v = col(header)
                            if (v.isNotEmpty()) add("$label $v")
                        }
                    }.joinToString("  ·  ")
                    if (ch.isNotEmpty() && rxFreq.isNotEmpty()) {
                        YaesuMem(ch, name, rxFreq, mode, dir, tone, details)
                    } else null
                } else null
            }
        } else emptyList()
    }
    val menuItems = remember(memData) {
        if (isMenuData) {
            memData.removePrefix("MENU:").lines().filter { it.isNotBlank() }
        } else emptyList()
    }

    Column {
        // Header: enable + power status
        Row(
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            val modelName = if (state.yaesuModel == 1) "FTX-1" else "FT-991A"
            Text("Yaesu $modelName", fontWeight = FontWeight.Bold, fontSize = 16.sp)
            Spacer(Modifier.weight(1f))
            val powerColor = if (state.yaesuPowerOn) Color(0xFF00C800) else Color(0xFF808080)
            // 991A: klikbaar aan/standby (PS0=standby, USB/CAT blijft -> remote weer
            // aan). FTX-1: label-only, want die gaat echt uit (USB weg, niet remote aan).
            if (state.yaesuModel == 0) {
                Text(
                    if (state.yaesuPowerOn) "ON" else "STBY",
                    color = powerColor,
                    fontWeight = FontWeight.Bold,
                    modifier = Modifier.clickable { onPowerOnOff(!state.yaesuPowerOn) },
                )
            } else {
                Text(
                    if (state.yaesuPowerOn) "ON" else "OFF",
                    color = powerColor,
                    fontWeight = FontWeight.Bold,
                )
            }
            Spacer(Modifier.width(8.dp))
            val txColor = if (state.yaesuTxActive) Color(0xFFFF4040) else Color(0xFF00C800)
            Text(if (state.yaesuTxActive) "TX" else "RX", color = txColor, fontWeight = FontWeight.Bold)
            // Zelfde rood als de desktop TL_SWR_ALERT_TEXT en de SWR-uitlezing >3.0.
            val swrRed = Color(0xFFFF5050)
            if (state.yaesuHiSwr) {
                Spacer(Modifier.width(8.dp))
                Text(stringResource(R.string.dev_high_swr), color = swrRed, fontWeight = FontWeight.Bold)
            }
            // Het alarm klinkt voor beide radio's, maar deze tab toont er één. Benoem
            // de andere radio expliciet, anders hoor je een piep zonder zichtbare oorzaak.
            val otherHiSwr = if (fullState.selectedRadio == 1) fullState.yaesuHiSwr else fullState.yaesu2HiSwr
            if (otherHiSwr) {
                val otherModel = if (fullState.selectedRadio == 1) fullState.yaesuModel else fullState.yaesu2Model
                val otherName = if (otherModel == 1) "FTX-1" else "FT-991A"
                Spacer(Modifier.width(8.dp))
                Text(stringResource(R.string.dev_high_swr_other, otherName), color = swrRed, fontWeight = FontWeight.Bold)
            }
        }

        HorizontalDivider(modifier = Modifier.padding(vertical = 6.dp))

        // Radio-selector: alleen tonen als BEIDE radio's connected zijn (geconfigureerd +
        // actief op de server). Android bedient er één tegelijk.
        if (fullState.yaesuConnected && fullState.yaesu2Connected) {
            val r1 = if (fullState.yaesuModel == 1) "FTX-1" else "FT-991A"
            val r2 = if (fullState.yaesu2Model == 1) "FTX-1" else "FT-991A"
            val sel = fullState.selectedRadio
            val switchLocked = fullState.yaesuTxActive || fullState.yaesu2TxActive
            val activeColor = ButtonDefaults.buttonColors(containerColor = Color(0xFF005AC8))
            Column(modifier = Modifier.padding(bottom = 6.dp)) {
                Row(
                    verticalAlignment = Alignment.CenterVertically,
                    horizontalArrangement = Arrangement.spacedBy(8.dp),
                ) {
                    Text(stringResource(R.string.dev_radio_label))
                    Button(
                        onClick = { onSelectRadio(0) },
                        enabled = !switchLocked,
                        colors = if (sel == 0) activeColor else ButtonDefaults.outlinedButtonColors(),
                    ) { Text("1 - $r1") }
                    Button(
                        onClick = { onSelectRadio(1) },
                        enabled = !switchLocked,
                        colors = if (sel == 1) activeColor else ButtonDefaults.outlinedButtonColors(),
                    ) { Text("2 - $r2") }
                }
                if (switchLocked) {
                    Text(stringResource(R.string.dev_radio_locked), fontSize = 11.sp, color = Color.Gray)
                }
            }
        }

        // Enable toggle (Yaesu audio). Met Thetis = "Yaesu active" (zet Thetis-audio
        // uit); zonder geconfigureerde Thetis is dit puur de audio-aan/uit-knop.
        Row(verticalAlignment = Alignment.CenterVertically) {
            Text(if (fullState.thetisConfigured) stringResource(R.string.dev_yaesu_active_label) else stringResource(R.string.dev_audio_label))
            Spacer(Modifier.width(8.dp))
            Switch(
                checked = yaesuActive,
                onCheckedChange = { onEnable(it) },
            )
            if (yaesuActive) {
                Spacer(Modifier.width(8.dp))
                Text(if (fullState.thetisConfigured) stringResource(R.string.dev_thetis_off) else stringResource(R.string.dev_audio_on),
                    fontSize = 12.sp, color = Color.Gray)
            }
        }

        Spacer(Modifier.height(6.dp))

        // Frequency display — click VFO A to enter frequency (VFO mode only)
        var showFreqDialog by remember { mutableStateOf(false) }

        if (showFreqDialog) {
            var freqInput by remember {
                mutableStateOf(
                    if (state.yaesuFreqA > 0) "%.5f".format(state.yaesuFreqA / 1_000_000.0) else ""
                )
            }
            AlertDialog(
                onDismissRequest = { showFreqDialog = false },
                title = { Text(stringResource(R.string.dev_freq_title)) },
                text = {
                    OutlinedTextField(
                        value = freqInput,
                        onValueChange = { freqInput = it },
                        singleLine = true,
                        keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Decimal),
                        modifier = Modifier.fillMaxWidth(),
                    )
                },
                confirmButton = {
                    TextButton(onClick = {
                        val mhz = freqInput.replace(",", ".").toDoubleOrNull()
                        if (mhz != null && mhz > 0) {
                            onFreqChange((mhz * 1_000_000).toLong())
                        }
                        showFreqDialog = false
                    }) { Text(stringResource(R.string.common_ok)) }
                },
                dismissButton = {
                    TextButton(onClick = { showFreqDialog = false }) { Text(stringResource(R.string.common_cancel)) }
                },
            )
        }

        Row(
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            Column {
                Text("VFO A", fontSize = 11.sp, color = Color.Gray)
                // VFO-mode: de grote tikbare digit-tuner hieronder is de frequentie
                // (geen dubbele weergave). Memory-mode: hier de frequentie + kanaalnaam
                // (er is geen digit-tuner in memory-mode).
                if (isMemory) {
                    Text(formatFreqMhz(state.yaesuFreqA), fontSize = 20.sp, fontWeight = FontWeight.Bold)
                    if (memChannels.isNotEmpty()) {
                        val mem = memChannels.firstOrNull { it.ch == "${state.yaesuMemoryChannel}" }
                        if (mem != null && mem.name.isNotEmpty()) {
                            Text(mem.name, fontSize = 26.sp, color = Color(0xFF4488FF), fontWeight = FontWeight.Bold)
                        }
                    }
                }
            }
            Column {
                Text("VFO B", fontSize = 11.sp, color = Color.Gray)
                Text(formatFreqMhz(state.yaesuFreqB), fontSize = 14.sp)
            }
            Spacer(Modifier.weight(1f))
            Column(horizontalAlignment = Alignment.End) {
                Text(modeName, fontWeight = FontWeight.Bold, fontSize = 16.sp)
                Row(horizontalArrangement = Arrangement.spacedBy(4.dp)) {
                    Text(vfoLabel, fontSize = 13.sp, color = Color.Gray)
                    if (state.yaesuMemoryChannel > 0 && state.yaesuVfoSelect == 1) {
                        Text("CH ${state.yaesuMemoryChannel}", fontSize = 13.sp, color = Color(0xFF4488FF))
                    }
                }
            }
        }

        // Touch-afstemming: tik boven/onder een cijfer = omhoog/omlaag; lang indrukken
        // = frequentie typen. + stapknoppen (−/+ met stapgrootte). Alleen in VFO-mode.
        // Dit is de enige frequentieweergave in VFO-mode (geen dubbele bovenaan meer).
        if (!isMemory && state.yaesuFreqA > 0) {
            Spacer(Modifier.height(4.dp))
            YaesuDigitTuner(
                state.yaesuFreqA,
                onLongPress = { showFreqDialog = true },
            ) { delta ->
                onFreqChange((state.yaesuFreqA + delta).coerceAtLeast(0))
            }
            Text(stringResource(R.string.dev_digit_tuner_hint), fontSize = 10.sp, color = Color.Gray)
            YaesuFreqStepper { delta ->
                onFreqChange((state.yaesuFreqA + delta).coerceAtLeast(0))
            }
        }

        // Yaesu S-meter (same layout as Thetis). The Yaesu CAT scale is a raw
        // 0..255 display unit that historically lined up with the old 0..228
        // ThetisLink scale; we re-use the inverse of `dbmToDisplay` so the
        // widget can present it on the same dBm axis as Thetis without a
        // second widget variant.
        val yaesuDbm = if (state.yaesuSmeter <= 108) {
            state.yaesuSmeter.toFloat() / 2f - 127f
        } else {
            (state.yaesuSmeter - 108).toFloat() * 0.5f - 73f
        }
        com.sdrremote.ui.components.SmeterBar(
            value = yaesuDbm,
            transmitting = state.yaesuTxActive,
            otherTx = false,
        )

        Spacer(Modifier.height(8.dp))

        // Active state
        val activeMode = state.yaesuMode
        val activeColor = Color(0xFF1565C0) // blue for active buttons
        val inactiveColor = Color(0xFF555555) // grey for inactive buttons

        // Row 1: A, B, A⇔B, V/M
        Row(horizontalArrangement = Arrangement.spacedBy(6.dp)) {
            val grey = ButtonDefaults.buttonColors(containerColor = inactiveColor)
            Button(onClick = { onSelectVfo(0) }, modifier = Modifier.weight(1f),
                colors = grey, contentPadding = PaddingValues(4.dp)) { Text("A", fontSize = 12.sp) }
            Button(onClick = { onSelectVfo(1) }, modifier = Modifier.weight(1f),
                colors = grey, contentPadding = PaddingValues(4.dp)) { Text("B", fontSize = 12.sp) }
            Button(onClick = { onSelectVfo(2) }, modifier = Modifier.weight(1f),
                colors = grey, contentPadding = PaddingValues(4.dp)) { Text("A⇔B", fontSize = 12.sp) }
            Button(onClick = { onSelectVfo(3) }, modifier = Modifier.weight(1f),
                colors = if (isMemory) ButtonDefaults.buttonColors(containerColor = activeColor)
                    else ButtonDefaults.buttonColors(containerColor = inactiveColor),
                contentPadding = PaddingValues(4.dp)) { Text("V/M", fontSize = 12.sp) }
        }

        Spacer(Modifier.height(4.dp))

        // Row 2: Mode buttons (blue = active)
        Row(horizontalArrangement = Arrangement.spacedBy(3.dp)) {
            val modes = listOf("LSB" to 0, "USB" to 1, "CW-L" to 3, "CW-U" to 4, "FM" to 5, "AM" to 6, "DIG" to 7)
            modes.forEach { (label, code) ->
                Button(onClick = { onMode(code) }, modifier = Modifier.weight(1f),
                    colors = if (activeMode == code) ButtonDefaults.buttonColors(containerColor = activeColor)
                        else ButtonDefaults.buttonColors(containerColor = inactiveColor),
                    contentPadding = PaddingValues(2.dp)) { Text(label, fontSize = 10.sp) }
            }
        }

        Spacer(Modifier.height(4.dp))

        // Row 3: Band + Mem navigation + Scan toggle
        Row(horizontalArrangement = Arrangement.spacedBy(6.dp)) {
            val grey2 = ButtonDefaults.buttonColors(containerColor = inactiveColor)
            Button(onClick = { onButton(5) }, modifier = Modifier.weight(1f),
                colors = grey2, contentPadding = PaddingValues(4.dp)) { Text("Band▲", fontSize = 11.sp) }
            Button(onClick = { onButton(6) }, modifier = Modifier.weight(1f),
                colors = grey2, contentPadding = PaddingValues(4.dp)) { Text("Band▼", fontSize = 11.sp) }
            Button(onClick = { onButton(9) }, modifier = Modifier.weight(1f),
                colors = grey2, contentPadding = PaddingValues(4.dp)) { Text("Mem▲", fontSize = 11.sp) }
            Button(onClick = { onButton(10) }, modifier = Modifier.weight(1f),
                colors = grey2, contentPadding = PaddingValues(4.dp)) { Text("Mem▼", fontSize = 11.sp) }
            Button(onClick = {
                onButton(if (!state.yaesuScan) 1 else 2)
            }, modifier = Modifier.weight(1f),
                colors = if (state.yaesuScan) ButtonDefaults.buttonColors(containerColor = activeColor)
                    else ButtonDefaults.buttonColors(containerColor = inactiveColor),
                contentPadding = PaddingValues(4.dp)) { Text(stringResource(R.string.dev_scan), fontSize = 11.sp) }
        }

        Spacer(Modifier.height(4.dp))

        // Interne ATU: momentane Tune (band-gated HF+6m, <54 MHz) + ATU aan/uit-toggle.
        // tuner_state komt van de radio (AC;-poll): 0=uit, 1=aan, 2=tunend. Waarden
        // 3=Tune-start / 15=ATU-aan / 4=ATU-uit; de server vertaalt per model (991A/FTX-1).
        Row(horizontalArrangement = Arrangement.spacedBy(6.dp)) {
            // ATU: HF+6m (<54 MHz) en niet in FM (mode 5).
            val atuAvail = state.yaesuFreqA in 1L until 54_000_000L && state.yaesuMode != 5
            val canTune = state.yaesuConnected && atuAvail
            val tuning = state.yaesuTunerState == 2
            val atuOn = state.yaesuTunerState == 1
            Button(
                onClick = { onButton(3) },
                enabled = canTune,
                modifier = Modifier.weight(1f),
                colors = if (tuning) ButtonDefaults.buttonColors(containerColor = Color(0xFFB40000))
                    else ButtonDefaults.buttonColors(containerColor = inactiveColor),
                contentPadding = PaddingValues(4.dp),
            ) { Text(if (tuning) "Tuning…" else "Tune", fontSize = 11.sp) }
            Button(
                onClick = { onButton(if (atuOn) 4 else 15) },
                enabled = atuAvail,
                modifier = Modifier.weight(1f),
                colors = if (atuOn) ButtonDefaults.buttonColors(containerColor = activeColor)
                    else ButtonDefaults.buttonColors(containerColor = inactiveColor),
                contentPadding = PaddingValues(4.dp),
            ) { Text("ATU", fontSize = 11.sp) }
            Spacer(Modifier.weight(3f)) // uitlijnen met de 5-koloms band-rij
        }

        Spacer(Modifier.height(8.dp))

        // Sliders — sync from server state, local override on drag
        // ControlId hex: Squelch=0x29, RfGain=0x2A, MicGain=0x2B, RfPower=0x2C
        // NB: het luistervolume zit in de sticky "Volume:"-slider bij de PTT (altijd
        // zichtbaar, master voor Thetis én de geselecteerde Yaesu). De vroegere "Vol:"-
        // slider hier was een duplicaat (zelfde yaesuVolumeSel) en is verwijderd.
        // Sync sliders from server state
        var squelch by remember { mutableFloatStateOf(state.yaesuSquelch.toFloat()) }
        var rfGain by remember { mutableFloatStateOf(state.yaesuRfGain.toFloat()) }
        var micGain by remember { mutableFloatStateOf(state.yaesuMicGain.toFloat()) }
        var rfPower by remember { mutableFloatStateOf(state.yaesuTxPower.toFloat()) }
        LaunchedEffect(state.yaesuSquelch) { squelch = state.yaesuSquelch.toFloat() }
        LaunchedEffect(state.yaesuRfGain) { rfGain = state.yaesuRfGain.toFloat() }
        LaunchedEffect(state.yaesuMicGain) { micGain = state.yaesuMicGain.toFloat() }
        LaunchedEffect(state.yaesuTxPower) { if (state.yaesuTxPower > 0) rfPower = state.yaesuTxPower.toFloat() }

        // Squelch
        Row(verticalAlignment = Alignment.CenterVertically) {
            Text("SQL:", fontSize = 12.sp, modifier = Modifier.width(40.dp))
            Slider(value = squelch, onValueChange = { squelch = it; onControl(0x29, it.toInt()) },
                valueRange = 0f..100f, modifier = Modifier.weight(1f))
            Text("${squelch.toInt()}", fontSize = 11.sp, modifier = Modifier.width(36.dp))
        }
        // RF Gain
        Row(verticalAlignment = Alignment.CenterVertically) {
            Text("RF:", fontSize = 12.sp, modifier = Modifier.width(40.dp))
            Slider(value = rfGain, onValueChange = { rfGain = it; onControl(0x2A, it.toInt()) },
                valueRange = 0f..255f, modifier = Modifier.weight(1f))
            Text("${rfGain.toInt()}", fontSize = 11.sp, modifier = Modifier.width(36.dp))
        }
        // Mic Gain (local client-side, before Opus encoding). Stored per radio
        // as the internal engine multiplier; UI display 0.5 maps to internal 0.2.
        val micEqPrefs = remember { context.getSharedPreferences("thetislink_eq", android.content.Context.MODE_PRIVATE) }
        fun micDisplayToEngine(v: Float): Float = (v * 0.4f).coerceIn(0.02f, 0.4f)
        fun micEngineToDisplay(v: Float): Float = (v / 0.4f).coerceIn(0.05f, 1f)
        val micRadio = fullState.selectedRadio
        var localMicGain by remember(micRadio) {
            mutableFloatStateOf(micEngineToDisplay(micEqPrefs.getFloat("yaesu_mic_gain_$micRadio", 0.2f)))
        }
        // No push on radio switch here: this tab is not always composed, so the
        // engine would keep the previous radio's gain. SdrViewModel.syncTxChainForRadio
        // does it from the state stream; the slider below still pushes on change.
        Row(verticalAlignment = Alignment.CenterVertically) {
            Text("Mic:", fontSize = 12.sp, modifier = Modifier.width(40.dp))
            Slider(value = localMicGain, onValueChange = {
                localMicGain = it
                val engineGain = micDisplayToEngine(it)
                onTxGain(engineGain)
                micEqPrefs.edit().putFloat("yaesu_mic_gain_$micRadio", engineGain).apply()
            }, valueRange = 0.05f..1f, modifier = Modifier.weight(1f))
            Text("${String.format("%.1f", localMicGain)}x", fontSize = 11.sp, modifier = Modifier.width(36.dp))
        }
        // Client-side TX-compressor (voor Yaesu-USB-audio; radio-shaping wordt op USB gebypassed).
        // Bewust HIER, inline naast Mic-gain: in het EQ-blok (lager) kreeg deze slider geen
        // touch (gemeten: comp onChange 0x, terwijl mic/DSP-sliders wel werken). Zelfde
        // context als de werkende Mic-slider → wel sleepbaar. State in YaesuTab-scope.
        // Per radio (net als EQ): laadt bij radio-wissel de compressor van die radio en
        // stuurt 'm naar de engine (onCompressor routeert naar de geselecteerde radio).
        // Migratie-fallback op de oude gedeelde key.
        val compRadio = state.selectedRadio
        var compLevel by remember(compRadio) {
            mutableFloatStateOf(micEqPrefs.getFloat("yaesu_comp_$compRadio", micEqPrefs.getFloat("yaesu_comp", 0f)))
        }
        // Radio-switch push lives in SdrViewModel.syncTxChainForRadio - see above.
        Row(verticalAlignment = Alignment.CenterVertically) {
            Text("Comp:", fontSize = 12.sp, modifier = Modifier.width(40.dp))
            Slider(value = compLevel, onValueChange = {
                compLevel = it
                onCompressor(it.toInt())
                micEqPrefs.edit().putFloat("yaesu_comp_$compRadio", it).apply()
            }, valueRange = 0f..100f, modifier = Modifier.weight(1f))
            Text("${compLevel.toInt()}", fontSize = 11.sp, modifier = Modifier.width(36.dp))
        }
        // RF Power - sliderbereik 5..max voor de huidige band (uit EX max-power menus);
        // 0/onbekend -> 100. Klemt net als de desktop zodat je de band-limiet niet overschrijdt.
        val pwrMax = if (state.yaesuTxPowerMax.toInt() >= 5) state.yaesuTxPowerMax.toFloat() else 100f
        Row(verticalAlignment = Alignment.CenterVertically) {
            Text("PWR:", fontSize = 12.sp, modifier = Modifier.width(40.dp))
            Slider(value = rfPower.coerceIn(5f, pwrMax), onValueChange = { rfPower = it; onControl(0x2C, it.toInt()) },
                valueRange = 5f..pwrMax, modifier = Modifier.weight(1f))
            Text("${rfPower.coerceIn(5f, pwrMax).toInt()}W", fontSize = 11.sp, modifier = Modifier.width(36.dp))
        }

        // DSP/functie-controls (Fase 2) — inklapbaar, gedeeld voor de geselecteerde radio.
        Spacer(Modifier.height(8.dp))
        YaesuDspSection(state, onDspControl)

        // 5-band EQ (persistent via SharedPreferences)
        Spacer(Modifier.height(8.dp))
        val context = LocalContext.current
        val eqPrefs = remember { context.getSharedPreferences("thetislink_eq", android.content.Context.MODE_PRIVATE) }
        // Onafhankelijke EQ per radio: keys krijgen de slot-index (0/1). Migratie: valt
        // terug op de oude gedeelde key als er nog geen radio-specifieke waarde is, zodat
        // de bestaande curve behouden blijft en daarna per radio kan divergeren.
        val eqRadio = state.selectedRadio
        var eqEnabled by remember(eqRadio) {
            mutableStateOf(eqPrefs.getBoolean("eq_enabled_$eqRadio", eqPrefs.getBoolean("eq_enabled", false)))
        }
        val bandLabels = listOf("100", "300", "1k", "2.5k", "4k")
        val eqGains = remember(eqRadio) {
            Array(5) { i -> mutableFloatStateOf(eqPrefs.getFloat("eq_band_${eqRadio}_$i", eqPrefs.getFloat("eq_band_$i", 0f))) }
        }
        // Client-side TX-keten AGC-toggle, per radio (net als EQ + compressor).
        var txAgc by remember(eqRadio) {
            mutableStateOf(eqPrefs.getBoolean("yaesu_tx_agc_$eqRadio", eqPrefs.getBoolean("yaesu_tx_agc", false)))
        }

        // Herappliceer de EQ naar de (nieuw) geselecteerde radio: bij eerste compositie
        // én bij radio-wissel. onEqBand/onEqEnabled routeren naar de geselecteerde radio
        // (ViewModel *Sel), zodat de EQ ook op radio 2 (FTX-1) landt en niet alleen radio 1.
        LaunchedEffect(state.selectedRadio) {
            onEqEnabled(eqEnabled)
            for (i in 0..4) onEqBand(i, eqGains[i].floatValue)
        }
        // Sync EQ enabled from SharedPreferences (auto-switch by ViewModel) — per radio.
        LaunchedEffect(eqRadio) {
            while (true) {
                kotlinx.coroutines.delay(500)
                val prefsVal = eqPrefs.getBoolean("eq_enabled_$eqRadio", eqEnabled)
                if (prefsVal != eqEnabled) {
                    eqEnabled = prefsVal
                    onEqEnabled(prefsVal)
                }
            }
        }
        // Herstel AGC naar de engine bij eerste compositie én bij radio-wissel
        // (onTxAgc routeert naar de geselecteerde radio).
        LaunchedEffect(eqRadio) {
            onTxAgc(txAgc)
        }

        // De verticale EQ-gain-sliders zijn inklapbaar achter een pijltje: bij het
        // scrollen door het scherm werden ze anders per ongeluk versteld. Default
        // ingeklapt, zodat scrollen ze nooit raakt; klik op "EQ" om ze te tonen.
        // Per-radio (geen gedeelde instelling): eigen klap-stand voor radio 0 en 1.
        var eqExp0 by rememberSaveable { mutableStateOf(false) }
        var eqExp1 by rememberSaveable { mutableStateOf(false) }
        val eqSlidersExpanded = if (state.selectedRadio == 1) eqExp1 else eqExp0
        Row(verticalAlignment = Alignment.CenterVertically) {
            Checkbox(checked = eqEnabled, onCheckedChange = {
                eqEnabled = it; onEqEnabled(it)
                eqPrefs.edit().putBoolean("eq_enabled_$eqRadio", it).apply()
            })
            Text(
                if (eqSlidersExpanded) "▼ EQ" else "▶ EQ",
                fontSize = 13.sp, fontWeight = FontWeight.Bold,
                modifier = Modifier.clickable { if (state.selectedRadio == 1) eqExp1 = !eqExp1 else eqExp0 = !eqExp0 }.padding(vertical = 4.dp),
            )
        }

        // Client-side TX-keten: AGC-toggle per radio (radio-processing werkt niet op USB).
        // EQ → compressor → AGC → gain. De compressor-slider staat bij Mic-gain (boven).
        Row(verticalAlignment = Alignment.CenterVertically) {
            Checkbox(checked = txAgc, onCheckedChange = {
                txAgc = it; onTxAgc(it); eqPrefs.edit().putBoolean("yaesu_tx_agc_$eqRadio", it).apply()
            })
            Text("AGC", fontSize = 13.sp, fontWeight = FontWeight.Bold)
        }

        // EQ Presets
        val presetKey = "eq_presets"
        var presetNames by remember { mutableStateOf<List<String>>(
            try {
                val json = org.json.JSONObject(eqPrefs.getString(presetKey, "{}") ?: "{}")
                json.keys().asSequence().toList().sorted()
            } catch (_: Exception) { emptyList() }
        ) }
        var selectedPreset by remember { mutableStateOf("") }
        var presetMenuExpanded by remember { mutableStateOf(false) }
        var showSaveDialog by remember { mutableStateOf(false) }
        // Preset-toewijzing (BT/Mic auto-keuze) wordt per radio onthouden: bij radio-wissel
        // herladen uit de slot-specifieke key. Dit is het kern-doel — elke radio z'n eigen
        // gekozen EQ-preset (operator 2026-07-04).
        var btPreset by remember(eqRadio) { mutableStateOf(eqPrefs.getString("eq_preset_bt_$eqRadio", "") ?: "") }
        var micPreset by remember(eqRadio) { mutableStateOf(eqPrefs.getString("eq_preset_mic_$eqRadio", "") ?: "") }

        // Helper: load a preset by name into sliders + engine
        fun loadPreset(name: String) {
            if (name.isBlank()) return
            try {
                val json = org.json.JSONObject(eqPrefs.getString(presetKey, "{}") ?: "{}")
                if (!json.has(name)) return
                val arr = json.getJSONArray(name)
                for (i in 0..4) {
                    val g = arr.getDouble(i).toFloat()
                    eqGains[i].floatValue = g
                    onEqBand(i, g)
                    eqPrefs.edit().putFloat("eq_band_${eqRadio}_$i", g).apply()
                }
                selectedPreset = name
            } catch (_: Exception) {}
        }

        // Sync preset from ViewModel auto-switch (BT connect/disconnect)
        LaunchedEffect(Unit) {
            while (true) {
                kotlinx.coroutines.delay(500)
                val pending = eqPrefs.getString("eq_preset_pending", null)
                if (pending != null) {
                    eqPrefs.edit().remove("eq_preset_pending").apply()
                    loadPreset(pending)
                }
            }
        }

        Row(
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(4.dp),
            modifier = Modifier.fillMaxWidth(),
        ) {
            Box(modifier = Modifier.weight(1f)) {
                OutlinedButton(
                    onClick = { presetMenuExpanded = true },
                    modifier = Modifier.fillMaxWidth(),
                    contentPadding = PaddingValues(horizontal = 8.dp, vertical = 4.dp),
                ) {
                    Text(
                        if (selectedPreset.isEmpty()) stringResource(R.string.dev_presets) else selectedPreset,
                        fontSize = 11.sp,
                        maxLines = 1,
                    )
                }
                DropdownMenu(
                    expanded = presetMenuExpanded,
                    onDismissRequest = { presetMenuExpanded = false },
                ) {
                    presetNames.forEach { name ->
                        val tag = buildString {
                            if (name == btPreset) append(" [BT]")
                            if (name == micPreset) append(" [Mic]")
                        }
                        DropdownMenuItem(
                            text = {
                                Row(verticalAlignment = Alignment.CenterVertically) {
                                    Text(name, fontSize = 12.sp)
                                    if (tag.isNotEmpty()) {
                                        Text(tag, fontSize = 10.sp, color = Color(0xFF1565C0))
                                    }
                                }
                            },
                            onClick = {
                                presetMenuExpanded = false
                                loadPreset(name)
                            },
                        )
                    }
                }
            }
            Button(
                onClick = { showSaveDialog = true },
                contentPadding = PaddingValues(horizontal = 8.dp, vertical = 4.dp),
            ) { Text(stringResource(R.string.common_save), fontSize = 11.sp) }
            Button(
                onClick = {
                    if (selectedPreset.isNotEmpty()) {
                        try {
                            val json = org.json.JSONObject(eqPrefs.getString(presetKey, "{}") ?: "{}")
                            json.remove(selectedPreset)
                            eqPrefs.edit().putString(presetKey, json.toString()).apply()
                            presetNames = json.keys().asSequence().toList().sorted()
                            // Clear auto-assign if deleted preset was assigned
                            if (selectedPreset == btPreset) {
                                btPreset = ""; eqPrefs.edit().putString("eq_preset_bt_$eqRadio", "").apply()
                            }
                            if (selectedPreset == micPreset) {
                                micPreset = ""; eqPrefs.edit().putString("eq_preset_mic_$eqRadio", "").apply()
                            }
                            selectedPreset = ""
                        } catch (_: Exception) {}
                    }
                },
                enabled = selectedPreset.isNotEmpty(),
                colors = ButtonDefaults.buttonColors(containerColor = Color(0xFFC62828)),
                contentPadding = PaddingValues(horizontal = 8.dp, vertical = 4.dp),
            ) { Text(stringResource(R.string.dev_del), fontSize = 11.sp, color = Color.White) }
        }

        // Auto-assign row: BT / Mic buttons
        Row(
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(4.dp),
            modifier = Modifier.fillMaxWidth(),
        ) {
            Text(stringResource(R.string.dev_auto_label), fontSize = 10.sp, color = Color.Gray, modifier = Modifier.width(32.dp))
            OutlinedButton(
                onClick = {
                    if (selectedPreset.isNotEmpty()) {
                        btPreset = selectedPreset
                        eqPrefs.edit().putString("eq_preset_bt_$eqRadio", selectedPreset).apply()
                    }
                },
                enabled = selectedPreset.isNotEmpty(),
                contentPadding = PaddingValues(horizontal = 6.dp, vertical = 2.dp),
                modifier = Modifier.weight(1f),
                colors = if (selectedPreset.isNotEmpty() && selectedPreset == btPreset)
                    ButtonDefaults.outlinedButtonColors(containerColor = Color(0xFF1565C0).copy(alpha = 0.15f))
                else ButtonDefaults.outlinedButtonColors(),
            ) {
                Text(
                    if (btPreset.isEmpty()) stringResource(R.string.dev_bt_headset_none) else "BT: $btPreset",
                    fontSize = 10.sp, maxLines = 1,
                )
            }
            OutlinedButton(
                onClick = {
                    if (selectedPreset.isNotEmpty()) {
                        micPreset = selectedPreset
                        eqPrefs.edit().putString("eq_preset_mic_$eqRadio", selectedPreset).apply()
                    }
                },
                enabled = selectedPreset.isNotEmpty(),
                contentPadding = PaddingValues(horizontal = 6.dp, vertical = 2.dp),
                modifier = Modifier.weight(1f),
                colors = if (selectedPreset.isNotEmpty() && selectedPreset == micPreset)
                    ButtonDefaults.outlinedButtonColors(containerColor = Color(0xFF1565C0).copy(alpha = 0.15f))
                else ButtonDefaults.outlinedButtonColors(),
            ) {
                Text(
                    if (micPreset.isEmpty()) "Mic: -" else "Mic: $micPreset",
                    fontSize = 10.sp, maxLines = 1,
                )
            }
        }

        if (showSaveDialog) {
            var saveName by remember { mutableStateOf(selectedPreset) }
            AlertDialog(
                onDismissRequest = { showSaveDialog = false },
                title = { Text(stringResource(R.string.dev_save_eq_preset)) },
                text = {
                    OutlinedTextField(
                        value = saveName,
                        onValueChange = { saveName = it },
                        singleLine = true,
                        label = { Text(stringResource(R.string.dev_name)) },
                        modifier = Modifier.fillMaxWidth(),
                    )
                },
                confirmButton = {
                    TextButton(onClick = {
                        if (saveName.isNotBlank()) {
                            try {
                                val json = org.json.JSONObject(eqPrefs.getString(presetKey, "{}") ?: "{}")
                                val arr = org.json.JSONArray()
                                for (i in 0..4) arr.put(eqGains[i].floatValue.toDouble())
                                json.put(saveName.trim(), arr)
                                eqPrefs.edit().putString(presetKey, json.toString()).apply()
                                presetNames = json.keys().asSequence().toList().sorted()
                                selectedPreset = saveName.trim()
                            } catch (_: Exception) {}
                        }
                        showSaveDialog = false
                    }) { Text(stringResource(R.string.common_save)) }
                },
                dismissButton = {
                    TextButton(onClick = { showSaveDialog = false }) { Text(stringResource(R.string.common_cancel)) }
                },
            )
        }

        if (eqSlidersExpanded) {
            Row(
                modifier = Modifier.fillMaxWidth(),
                horizontalArrangement = Arrangement.SpaceEvenly,
                verticalAlignment = Alignment.Bottom,
            ) {
                bandLabels.forEachIndexed { i, label ->
                    Column(horizontalAlignment = Alignment.CenterHorizontally, modifier = Modifier.width(56.dp)) {
                        Text("${eqGains[i].floatValue.toInt()}", fontSize = 10.sp)
                        // Vertical slider: the EQ column has plenty of vertical room, so the
                        // gain sliders read like a real graphic-EQ (up = boost) instead of
                        // cramped horizontal ones (feedback #4).
                        VerticalSlider(
                            value = eqGains[i].floatValue,
                            onValueChange = {
                                eqGains[i].floatValue = it; onEqBand(i, it)
                                eqPrefs.edit().putFloat("eq_band_${eqRadio}_$i", it).apply()
                            },
                            valueRange = -12f..12f,
                            modifier = Modifier.width(40.dp).height(75.dp),
                        )
                        Text(label, fontSize = 10.sp)
                    }
                }
            }
        }

        Spacer(Modifier.height(8.dp))

        // Memory + Settings buttons with loading feedback
        var memLoading by rememberSaveable { mutableStateOf(false) }
        var settingsLoading by rememberSaveable { mutableStateOf(false) }

        Row(horizontalArrangement = Arrangement.spacedBy(6.dp)) {
            val memColor = if (memLoading) ButtonDefaults.buttonColors(containerColor = Color(0xFF1565C0))
                else ButtonDefaults.buttonColors()
            Button(onClick = {
                memLoading = true
                onControl(0x25, 0)
            }, modifier = Modifier.weight(1f), colors = memColor,
                contentPadding = PaddingValues(4.dp)) {
                Text(if (memLoading) stringResource(R.string.dev_loading) else stringResource(R.string.dev_load_memories), fontSize = 11.sp)
            }
            val setColor = if (settingsLoading) ButtonDefaults.buttonColors(containerColor = Color(0xFF1565C0))
                else ButtonDefaults.buttonColors()
            Button(onClick = {
                settingsLoading = true
                onControl(0x2E, 0)
            }, modifier = Modifier.weight(1f), colors = setColor,
                contentPadding = PaddingValues(4.dp)) {
                Text(if (settingsLoading) stringResource(R.string.dev_loading) else stringResource(R.string.dev_load_settings), fontSize = 11.sp)
            }
        }

        // Reset loading state when data arrives
        LaunchedEffect(memData) {
            if (memData.isNotEmpty() && !isMenuData) memLoading = false
            if (isMenuData) settingsLoading = false
        }
        // Timeout fallback
        LaunchedEffect(memLoading) {
            if (memLoading) { kotlinx.coroutines.delay(10000); memLoading = false }
        }
        LaunchedEffect(settingsLoading) {
            if (settingsLoading) { kotlinx.coroutines.delay(10000); settingsLoading = false }
        }

        // Memory channel list (scrollable, inklapbaar achter een pijltje - geldt voor de
        // geselecteerde radio, dus zowel 991A als FTX-1).
        // Per-radio klap-stand (geen gedeelde instelling).
        var memExp0 by rememberSaveable { mutableStateOf(false) }
        var memExp1 by rememberSaveable { mutableStateOf(false) }
        val memListExpanded = if (state.selectedRadio == 1) memExp1 else memExp0
        if (memChannels.isNotEmpty()) {
            Spacer(Modifier.height(8.dp))
            Text(
                (if (memListExpanded) "▼ " else "▶ ") + stringResource(R.string.dev_memory_channels, memChannels.size),
                fontWeight = FontWeight.Bold, fontSize = 13.sp,
                modifier = Modifier
                    .fillMaxWidth()
                    .clickable { if (state.selectedRadio == 1) memExp1 = !memExp1 else memExp0 = !memExp0 }
                    .padding(vertical = 4.dp),
            )
            if (memListExpanded) {
            Spacer(Modifier.height(4.dp))
            Column(
                modifier = Modifier
                    .heightIn(max = 300.dp)
                    .fillMaxWidth()
                    .verticalScroll(rememberScrollState())
            ) {
                memChannels.forEach { mem ->
                    // Tik op een rij = kanaal oproepen (ControlId::YaesuRecallMemory).
                    // Het actieve kanaal krijgt een blauwe achtergrond, zodat de lijst
                    // laat zien waar de radio staat.
                    val chNum = mem.ch.toIntOrNull()
                    val isActive = chNum != null &&
                        chNum == state.yaesuMemoryChannel && state.yaesuVfoSelect == 1
                    Column(
                        modifier = Modifier
                            .fillMaxWidth()
                            .then(
                                if (chNum != null) Modifier.clickable { onRecallMemory(chNum) }
                                else Modifier
                            )
                            .background(if (isActive) Color(0xFF1A3A5A) else Color.Transparent)
                            .padding(vertical = 5.dp),
                    ) {
                        Row(horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                            Text(mem.ch, fontSize = 12.sp, modifier = Modifier.width(22.dp), color = Color.Gray)
                            Text(mem.name, fontSize = 12.sp, modifier = Modifier.width(80.dp), fontWeight = FontWeight.Bold, maxLines = 1)
                            Text(mem.rxFreq, fontSize = 12.sp, modifier = Modifier.weight(1f), maxLines = 1)
                            Text(mem.mode, fontSize = 11.sp, color = Color.Gray, maxLines = 1)
                        }
                        // Second line with the settings the radio did report -
                        // absent when it reported none, so no empty row appears.
                        if (mem.details.isNotEmpty()) {
                            Text(
                                mem.details,
                                fontSize = 10.sp,
                                color = Color(0xFF9A9A9A),
                                modifier = Modifier.padding(start = 28.dp, top = 1.dp),
                            )
                        }
                    }
                }
            }
            }
        }

        // Menu settings list (scrollable)
        if (menuItems.isNotEmpty()) {
            Spacer(Modifier.height(8.dp))
            Text(stringResource(R.string.dev_ex_menu, menuItems.size), fontWeight = FontWeight.Bold, fontSize = 13.sp)
            Spacer(Modifier.height(4.dp))
            Column(
                modifier = Modifier
                    .heightIn(max = 300.dp)
                    .fillMaxWidth()
                    .verticalScroll(rememberScrollState())
            ) {
                menuItems.forEach { item ->
                    val parts = item.split(":", limit = 2)
                    val num = parts.getOrElse(0) { "" }.trim().toIntOrNull() ?: 0
                    val value = parts.getOrElse(1) { "" }.trim()
                    val name = YAESU_MENU_NAMES[num] ?: stringResource(R.string.dev_menu_num, num)
                    Row(
                        modifier = Modifier.fillMaxWidth().padding(vertical = 1.dp),
                        horizontalArrangement = Arrangement.spacedBy(6.dp),
                    ) {
                        Text("$num", fontSize = 11.sp, modifier = Modifier.width(28.dp), color = Color.Gray)
                        Text(name, fontSize = 11.sp, modifier = Modifier.weight(1f))
                        Text(value, fontSize = 11.sp, fontWeight = FontWeight.Bold)
                    }
                }
            }
        }
    }
}

/// Memory-list columns beyond frequency/mode/name, as (label, header name) in
/// the tab-separated memory blob the server sends, looked up BY NAME.
/// Ham abbreviations for the label, untranslated like the rest of the radio
/// jargon; the second entry must match the server's column header exactly.
///
/// Never by position: the blob has three readers (server writes, desktop
/// parses, this parses), and a column added in the middle used to shift every
/// label here onto the wrong value, silently.
private val EXTRA_MEM_COLUMNS = listOf(
    "Narrow" to "Narrow", "Skip" to "Skip", "ATT" to "Attenuator", "Tuner" to "Tuner",
    "AGC" to "AGC", "NB" to "Noise Blanker", "IPO" to "IPO", "DNR" to "DNR", "Step" to "Step",
)

private val YAESU_MENU_NAMES = mapOf(
    1 to "AGC FAST DELAY", 2 to "AGC MID DELAY", 3 to "AGC SLOW DELAY", 4 to "HOME FUNCTION",
    5 to "MY CALL INDICATION", 6 to "DISPLAY COLOR", 7 to "DIMMER LED", 8 to "DIMMER TFT",
    9 to "BAR MTR PEAK HOLD", 10 to "DVS RX OUT LVL", 11 to "DVS TX OUT LVL", 12 to "KEYER TYPE",
    13 to "KEYER DOT/DASH", 14 to "CW WEIGHT", 15 to "BEACON INTERVAL", 16 to "NUMBER STYLE",
    17 to "CONTEST NUMBER", 23 to "NB WIDTH", 24 to "NB REJECTION", 25 to "NB LEVEL",
    26 to "BEEP LEVEL", 31 to "CAT RATE", 35 to "QUICK SPLIT FREQ", 36 to "TX TOT",
    39 to "REF FREQ ADJ", 40 to "CLAR MODE", 41 to "AM LCUT FREQ", 43 to "AM HCUT FREQ",
    45 to "AM MIC SELECT", 50 to "CW LCUT FREQ", 52 to "CW HCUT FREQ", 55 to "CW AUTO MODE",
    56 to "CW BK-IN TYPE", 57 to "CW BK-IN DELAY", 62 to "DATA MODE", 66 to "DATA LCUT FREQ",
    68 to "DATA HCUT FREQ", 70 to "DATA IN SELECT", 74 to "FM MIC SELECT", 79 to "FM PKT MODE",
    86 to "DCS POLARITY", 92 to "RTTY LCUT FREQ", 94 to "RTTY HCUT FREQ", 102 to "SSB LCUT FREQ",
    104 to "SSB HCUT FREQ", 106 to "SSB MIC SELECT", 108 to "SSB PTT SELECT", 110 to "SSB TX BPF",
    111 to "APF WIDTH", 112 to "CONTOUR LEVEL", 113 to "CONTOUR WIDTH", 114 to "IF NOTCH WIDTH",
    115 to "SCP DISPLAY MODE", 116 to "SCP SPAN FREQ", 117 to "SPECTRUM COLOR", 118 to "WATERFALL COLOR",
    137 to "HF TX MAX PWR", 138 to "50M TX MAX PWR", 139 to "144M TX MAX PWR", 140 to "430M TX MAX PWR",
    141 to "TUNER SELECT", 142 to "VOX SELECT", 143 to "VOX GAIN", 144 to "VOX DELAY",
)

// ========== Yaesu DSP/functie-controls (Fase 2) ==========
// Getypt YaesuControl-kanaal: onDspControl(control-index, value). Feature-state komt uit
// state.yaesuFeature* (al geremapt naar de geselecteerde radio). Per-model verschillen
// (NB/DNR 991A toggle+slider vs FTX-1 slider, AMC FTX-1-only, APF CW-only) server-side +
// hier in de UI. Sliders sturen op release (onValueChangeFinished) → geen CAT-burst.
private val DSP_BLUE = Color(0xFF005AC8)

@Composable
private fun YaesuDspSection(state: SdrUiState, onDspControl: (Int, Int) -> Unit) {
    // Per-radio klap-stand (geen gedeelde instelling); rememberSaveable zoals EQ/geheugenlijst.
    var dspExp0 by rememberSaveable { mutableStateOf(false) }
    var dspExp1 by rememberSaveable { mutableStateOf(false) }
    val expanded = if (state.selectedRadio == 1) dspExp1 else dspExp0
    val toggles = state.yaesuFeatureToggles
    val levels = state.yaesuFeatureLevels
    val freqs = state.yaesuFeatureFreqs
    fun bit(n: Int) = (toggles and (1u shl n)) != 0u
    fun lvl(n: Int) = levels.getOrElse(n) { 0 }
    fun frq(n: Int) = freqs.getOrElse(n) { 0 }
    val isFtx1 = state.yaesuModel == 1
    val isCw = state.yaesuMode == 3 || state.yaesuMode == 4
    // Band/mode-beschikbaarheid (FT-991A/FTX-1): IPO/ATT alleen op HF+6m (<54 MHz);
    // NB/DNF/Notch/Contour niet in FM (mode 5); NAR/AGC/DNR werken volgens de 991A OM
    // wel in FM. BK-IN is CW-only. Buiten bereik -> uitgrijzen.
    val isFm = state.yaesuMode == 5 || state.yaesuMode == 12  // 12 = C4FM (FM-familie)
    val hf6m = state.yaesuFreqA in 1L until 54_000_000L

    Column {
        Text(
            (if (expanded) "▼ " else "▶ ") + stringResource(R.string.dev_dsp_functions),
            fontWeight = FontWeight.Bold, fontSize = 14.sp,
            modifier = Modifier.fillMaxWidth().clickable { if (state.selectedRadio == 1) dspExp1 = !dspExp1 else dspExp0 = !dspExp0 }.padding(vertical = 4.dp),
        )
        if (expanded) {
            // Toggles: ATT / BK-IN / NAR / DNF
            Row(horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                DspToggleBtn("ATT", bit(0), hf6m) { onDspControl(0, if (bit(0)) 0 else 1) }
                DspToggleBtn("BK-IN", bit(1), isCw) { onDspControl(1, if (bit(1)) 0 else 1) }
                DspToggleBtn("NAR", bit(2)) { onDspControl(2, if (bit(2)) 0 else 1) }
                DspToggleBtn("DNF", bit(3), !isFm) { onDspControl(3, if (bit(3)) 0 else 1) }
            }
            // AGC + IPO cyclus (label toont de stand, geen blauw)
            Row(horizontalArrangement = Arrangement.spacedBy(6.dp), modifier = Modifier.padding(top = 4.dp)) {
                val agc = lvl(6)
                val agcLbl = when (agc) { 1 -> "FAST"; 2 -> "MID"; 3 -> "SLOW"; else -> "AUTO" }
                OutlinedButton(onClick = { onDspControl(6, if (agc in 1..3) agc + 1 else 1) }) {
                    Text("AGC:$agcLbl", fontSize = 12.sp)
                }
                val ipo = lvl(7)
                val ipoLbl = when (ipo) { 1 -> "AMP1"; 2 -> "AMP2"; else -> "IPO" }
                OutlinedButton(onClick = { onDspControl(7, (ipo + 1) % 3) }, enabled = hf6m) { Text(ipoLbl, fontSize = 12.sp) }
            }
            // Niveaus: NB/DNR (991A toggle+slider, FTX-1 slider), AMC (FTX-1-only).
            // Proc (radio-speech-processor) verwijderd: doet niets op USB-audio (de
            // radio-shaping wordt gebypassed op REAR/USB). Client-side EQ/AGC vervangen 't.
            // NB niet in FM (tester-bevestigd); DNR/AMC blijven wel bruikbaar (991A OM
            // beperkt DNR niet tot niet-FM; AMC is TX-audio, FTX-1).
            DspLevelRow("NB", if (isFtx1) null else 13, bit(13), 8, lvl(8), 0f, 10f, onDspControl, enabled = !isFm)
            DspLevelRow("DNR", if (isFtx1) null else 14, bit(14), 9, lvl(9), 0f, 10f, onDspControl)
            if (isFtx1) DspLevelRow("AMC", null, false, 11, lvl(11), 1f, 100f, onDspControl)
            // Contour / APF / Notch: aan/uit + frequentie (APF alleen in CW; Contour/Notch niet in FM)
            DspFreqRow("Contour", 15, bit(15), 18, frq(0), 10f, 3200f, !isFm && !isCw, onDspControl)
            DspFreqRow("APF", 16, bit(16), 19, frq(1), 0f, 50f, isCw, onDspControl)
            DspFreqRow("Notch", 17, bit(17), 20, frq(2), 1f, 320f, !isFm, onDspControl)

            // Clarifier (Fase 3): RIT/XIT aan/uit (bits 21/22) + offset (freqs[3], signed) + stap.
            val ritOn = bit(21)
            val xitOn = bit(22)
            val off = frq(3).let { if (it > 32767) it - 65536 else it } // i16-as-u16 → signed Hz
            Row(
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(6.dp),
                modifier = Modifier.padding(top = 6.dp),
            ) {
                DspToggleBtn("RIT", ritOn) { onDspControl(21, if (ritOn) 0 else 1) }
                DspToggleBtn("XIT", xitOn) { onDspControl(22, if (xitOn) 0 else 1) }
                // Alleen een actieve (RIT/XIT aan) offset oranje tonen; de 991A kan een
                // opgeslagen P3-offset teruggeven terwijl de clarifier uit staat.
                val clarActive = ritOn || xitOn
                Text(
                    if (clarActive && off != 0) "%+d Hz".format(off) else "+0 Hz",
                    fontSize = 12.sp, fontWeight = FontWeight.Bold,
                    color = if (clarActive && off != 0) Color(0xFFFFAA28) else Color(0xFF808080),
                )
            }
            Row(horizontalArrangement = Arrangement.spacedBy(4.dp), modifier = Modifier.padding(top = 2.dp)) {
                for ((lbl, step) in listOf("-100" to -100, "-10" to -10)) {
                    OutlinedButton(onClick = { onDspControl(24, step) },
                        contentPadding = PaddingValues(horizontal = 8.dp, vertical = 2.dp)) { Text(lbl, fontSize = 11.sp) }
                }
                OutlinedButton(onClick = { onDspControl(23, 0) },
                    contentPadding = PaddingValues(horizontal = 8.dp, vertical = 2.dp)) { Text("Clr", fontSize = 11.sp) }
                for ((lbl, step) in listOf("+10" to 10, "+100" to 100)) {
                    OutlinedButton(onClick = { onDspControl(24, step) },
                        contentPadding = PaddingValues(horizontal = 8.dp, vertical = 2.dp)) { Text(lbl, fontSize = 11.sp) }
                }
            }
        }
    }
}

@Composable
private fun DspToggleBtn(label: String, on: Boolean, enabled: Boolean = true, onClick: () -> Unit) {
    Button(
        onClick = onClick,
        enabled = enabled,
        colors = if (on) ButtonDefaults.buttonColors(containerColor = DSP_BLUE)
                 else ButtonDefaults.outlinedButtonColors(),
        contentPadding = PaddingValues(horizontal = 10.dp, vertical = 2.dp),
    ) { Text(label, fontSize = 12.sp) }
}

@Composable
private fun DspLevelRow(
    label: String,
    toggleCtrl: Int?,   // 991A on/off control-index; null = alleen slider
    toggleOn: Boolean,
    levelCtrl: Int, levelValue: Int, min: Float, max: Float,
    onDspControl: (Int, Int) -> Unit,
    enabled: Boolean = true,
) {
    // Bewezen patroon (zoals squelch/RF-gain): stuur op onValueChange, sync uit server-state.
    var v by remember { mutableFloatStateOf(levelValue.toFloat()) }
    LaunchedEffect(levelValue) { v = levelValue.toFloat().coerceIn(min, max) }
    Row(verticalAlignment = Alignment.CenterVertically, modifier = Modifier.padding(top = 2.dp)) {
        if (toggleCtrl != null) {
            DspToggleBtn(label, toggleOn, enabled) { onDspControl(toggleCtrl, if (toggleOn) 0 else 1) }
        } else {
            Text(label, fontSize = 12.sp, modifier = Modifier.width(56.dp))
        }
        Slider(
            value = v.coerceIn(min, max),
            onValueChange = { v = it; onDspControl(levelCtrl, it.toInt()) },
            valueRange = min..max,
            enabled = enabled,
            modifier = Modifier.weight(1f),
        )
        Text("${v.toInt()}", fontSize = 11.sp, modifier = Modifier.width(36.dp))
    }
}

@Composable
private fun DspFreqRow(
    label: String, toggleCtrl: Int, on: Boolean, freqCtrl: Int, freqValue: Int,
    min: Float, max: Float, enabled: Boolean, onDspControl: (Int, Int) -> Unit,
) {
    var v by remember { mutableFloatStateOf(freqValue.toFloat()) }
    LaunchedEffect(freqValue) { v = freqValue.toFloat().coerceIn(min, max) }
    Row(verticalAlignment = Alignment.CenterVertically, modifier = Modifier.padding(top = 2.dp)) {
        Button(
            onClick = { onDspControl(toggleCtrl, if (on) 0 else 1) },
            enabled = enabled,
            colors = if (on) ButtonDefaults.buttonColors(containerColor = DSP_BLUE)
                     else ButtonDefaults.outlinedButtonColors(),
            contentPadding = PaddingValues(horizontal = 8.dp, vertical = 2.dp),
        ) { Text(label, fontSize = 11.sp) }
        Slider(
            value = v.coerceIn(min, max),
            onValueChange = { v = it; onDspControl(freqCtrl, it.toInt()) },
            valueRange = min..max,
            enabled = enabled,
            modifier = Modifier.weight(1f).padding(start = 6.dp),
        )
        Text("${v.toInt()}", fontSize = 11.sp, modifier = Modifier.width(44.dp))
    }
}

// Cijfer-tik afstemming (Fase 4): elke cijferpositie is aantikbaar; bovenste helft =
// die decade omhoog, onderste helft omlaag. Groeperings-punten zijn niet aantikbaar.
@Composable
private fun YaesuDigitTuner(freqHz: Long, onLongPress: () -> Unit, onDelta: (Long) -> Unit) {
    if (freqHz <= 0) return
    val cb by rememberUpdatedState(onDelta)
    val lp by rememberUpdatedState(onLongPress)
    val s = freqHz.toString()
    val n = s.length
    Row(verticalAlignment = Alignment.CenterVertically) {
        s.forEachIndexed { idx, ch ->
            // Groeperings-punt vóór elke groep van 3 (van rechts).
            if (idx > 0 && (n - idx) % 3 == 0) {
                Text(".", fontSize = 30.sp, fontWeight = FontWeight.Bold, color = Color.Gray)
            }
            val power = n - 1 - idx
            var st = 1L
            repeat(power) { st *= 10 }
            val step = st
            Box(
                modifier = Modifier
                    .pointerInput(power) {
                        // Tik boven/onder = decade omhoog/omlaag; lang indrukken = typen.
                        detectTapGestures(
                            onTap = { offset -> cb(if (offset.y < size.height / 2f) step else -step) },
                            onLongPress = { lp() },
                        )
                    }
                    // Grotere cijfers + bredere/hogere tik-zone voor vinger-bediening.
                    .padding(horizontal = 6.dp, vertical = 6.dp),
            ) {
                Text(ch.toString(), fontSize = 30.sp, fontWeight = FontWeight.Bold)
            }
        }
        Text(" Hz", fontSize = 13.sp, color = Color.Gray)
    }
}

// Touch-stapper (Fase 4): −/+ met instelbare stapgrootte (10 Hz … 1 MHz).
@Composable
private fun YaesuFreqStepper(onDelta: (Long) -> Unit) {
    var stepHz by rememberSaveable { mutableStateOf(1000L) }
    val steps = listOf("10" to 10L, "100" to 100L, "1k" to 1_000L, "10k" to 10_000L, "100k" to 100_000L, "1M" to 1_000_000L)
    // Vol-breedte + gewogen knoppen: Material-knoppen hebben een min-breedte (~58dp)
    // waardoor 8 knoppen naast elkaar op een telefoon niet passen (de + viel eraf).
    // weight(1f) overschrijft die min-breedte zodat alles altijd past en zichtbaar is.
    Row(
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(2.dp),
        modifier = Modifier.fillMaxWidth().padding(top = 2.dp),
    ) {
        Button(onClick = { onDelta(-stepHz) },
            modifier = Modifier.weight(1f),
            contentPadding = PaddingValues(horizontal = 2.dp, vertical = 2.dp)) {
            Text("−", fontSize = 15.sp, maxLines = 1)
        }
        for ((lbl, s) in steps) {
            val sel = stepHz == s
            OutlinedButton(
                onClick = { stepHz = s },
                modifier = Modifier.weight(1f),
                colors = if (sel) ButtonDefaults.buttonColors(containerColor = DSP_BLUE) else ButtonDefaults.outlinedButtonColors(),
                contentPadding = PaddingValues(horizontal = 2.dp, vertical = 2.dp),
            ) { Text(lbl, fontSize = 10.sp, maxLines = 1, softWrap = false) }
        }
        Button(onClick = { onDelta(stepHz) },
            modifier = Modifier.weight(1f),
            contentPadding = PaddingValues(horizontal = 2.dp, vertical = 2.dp)) {
            Text("+", fontSize = 15.sp, maxLines = 1)
        }
    }
}

private fun formatFreqMhz(hz: Long): String {
    if (hz == 0L) return "---"
    val mhz = hz / 1_000_000
    val khz = (hz % 1_000_000) / 1_000
    val sub = (hz % 1_000) / 10
    return "%d.%03d.%02d".format(mhz, khz, sub)
}

/**
 * A vertical Material slider. Compose ships only a horizontal Slider, so we render
 * a normal one and rotate it 270 degrees, swapping the measured width/height so it
 * occupies a tall-and-narrow box and hit-testing stays correct. Dragging up raises
 * the value (bottom = min, top = max) — the natural direction for a graphic EQ.
 * The caller sizes the on-screen footprint via `modifier` (e.g. width x height).
 */
@Composable
private fun VerticalSlider(
    value: Float,
    onValueChange: (Float) -> Unit,
    valueRange: ClosedFloatingPointRange<Float>,
    modifier: Modifier = Modifier,
) {
    Slider(
        value = value,
        onValueChange = onValueChange,
        valueRange = valueRange,
        modifier = modifier
            .graphicsLayer {
                rotationZ = 270f
                transformOrigin = TransformOrigin(0f, 0f)
            }
            .layout { measurable, constraints ->
                val placeable = measurable.measure(
                    Constraints(
                        minWidth = constraints.minHeight,
                        maxWidth = constraints.maxHeight,
                        minHeight = constraints.minWidth,
                        maxHeight = constraints.maxWidth,
                    )
                )
                layout(placeable.height, placeable.width) {
                    placeable.place(-placeable.width, 0)
                }
            },
    )
}
