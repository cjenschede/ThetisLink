package com.sdrremote.ui.components

import androidx.compose.foundation.gestures.awaitEachGesture
import androidx.compose.foundation.gestures.awaitFirstDown
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.width
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.Slider
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.rememberCoroutineScope
import kotlinx.coroutines.launch
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableFloatStateOf
import androidx.compose.runtime.mutableLongStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.input.nestedscroll.NestedScrollConnection
import androidx.compose.ui.input.nestedscroll.nestedScroll
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import kotlin.math.abs
import kotlin.math.atan2
import kotlin.math.cos
import kotlin.math.sin
import kotlin.math.sqrt
import kotlinx.coroutines.delay
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.padding
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.graphics.nativeCanvas

// ControlId constants matching protocol.rs
private const val CONTROL_POWER = 0x02
private const val CONTROL_NR = 0x04
private const val CONTROL_ANF = 0x05
private const val CONTROL_TX_PROFILE = 0x03
private const val CONTROL_DRIVE = 0x06
private const val CONTROL_NB = 0x38
private const val CONTROL_TUNE = 0x1F
private const val CONTROL_DIVERSITY = 0x40

@Composable
fun RadioControls(
    powerOn: Boolean,
    thetisStarting: Boolean,
    connected: Boolean,
    nrLevel: Int,
    anfOn: Boolean,
    nbLevel: Int,  // 0=off, 1=NB1, 2=NB2
    diversityEnabled: Boolean,
    diversityPhase: Float,
    diversityGainRx1: Float,
    diversityGainRx2: Float,
    diversityRef: Int,
    diversityAutonullResult: Int,
    agcEnabled: Boolean,
    txProfile: Int,
    txProfiles: List<Pair<Int, String>>,
    serverTxProfileNames: List<String>,
    driveLevel: Int,
    rf2kOperate: Boolean,
    rf2kConnected: Boolean,
    rf2kActive: Boolean,
    speState: Int,
    speConnected: Boolean,
    speActive: Boolean,
    onControl: (Int, Int) -> Unit,
    onAgcToggle: (Boolean) -> Unit,
    onRf2kOperate: (Boolean) -> Unit,
    onSpeOperate: () -> Unit,
) {
    // Stable callback for DriveSlider — prevents recomposition during polling
    val onDriveChange: (Int) -> Unit = remember { { onControl(CONTROL_DRIVE, it) } }

    // rememberUpdatedState keeps values current inside pointerInput/LaunchedEffect
    // (pointerInput(Unit) captures by value at first composition, so without this
    // the gesture handler would use stale powerOn/thetisStarting after recomposition)
    val currentPowerOn by rememberUpdatedState(powerOn)
    val currentThetisStarting by rememberUpdatedState(thetisStarting)

    Column(modifier = Modifier.fillMaxWidth()) {
        // Power, NR, ANF row
        Row(
            modifier = Modifier.fillMaxWidth(),
            horizontalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            // Power toggle with long-press shutdown (2s hold = ZZBY)
            var holdingPower by remember { mutableStateOf(false) }
            var shuttingDown by remember { mutableStateOf(false) }
            var shutdownSent by remember { mutableStateOf(false) }
            var holdStartMs by remember { mutableLongStateOf(0L) }

            // Timer: when holding 2s, trigger shutdown (ZZBY).
            // Works in both power on and power off state — ZZBY when already off is harmless,
            // and this prevents long-press on red from accidentally toggling power on.
            LaunchedEffect(holdingPower) {
                if (holdingPower) {
                    delay(2000L)
                    shuttingDown = true
                    if (!shutdownSent) {
                        shutdownSent = true
                        onControl(CONTROL_POWER, 2) // value 2 = shutdown (ZZBY)
                    }
                }
            }

            val powerColor = when {
                shuttingDown && holdingPower -> Color(0xFF960000)
                holdingPower -> Color(0xFFB48200)
                thetisStarting -> Color(0xFFB48200)
                powerOn -> Color(0xFF009600)
                else -> Color(0xFF960000)
            }
            val powerText = when {
                shuttingDown && holdingPower -> "SHUTDOWN!"
                holdingPower -> "HOLD..."
                thetisStarting -> "STARTING..."
                powerOn -> "POWER ON"
                else -> "POWER OFF"
            }

            Button(
                onClick = { }, // handled by pointerInput
                colors = ButtonDefaults.buttonColors(containerColor = powerColor),
                modifier = Modifier.pointerInput(Unit) {
                    awaitEachGesture {
                        awaitFirstDown(requireUnconsumed = false)
                        holdStartMs = System.currentTimeMillis()
                        holdingPower = true
                        shuttingDown = false
                        shutdownSent = false
                        // Wait for release
                        while (true) {
                            val event = awaitPointerEvent()
                            event.changes.forEach { it.consume() }
                            if (event.changes.all { !it.pressed }) break
                        }
                        val wasShutdown = shutdownSent
                        val holdMs = System.currentTimeMillis() - holdStartMs
                        holdingPower = false
                        shuttingDown = false
                        // Short click (<1.5s) = toggle power, long hold = ignore
                        if (!wasShutdown && holdMs < 1500 && !currentThetisStarting) {
                            val newVal = if (currentPowerOn) 0 else 1
                            onControl(CONTROL_POWER, newVal)
                        }
                    }
                },
            ) {
                Text(
                    text = powerText,
                    color = Color.White,
                    fontWeight = FontWeight.Bold,
                )
            }

            // NR cycle: OFF -> NR1 -> NR2 -> NR3 -> NR4 -> OFF
            val nrLabel = if (nrLevel == 0) "NR" else "NR$nrLevel"
            Button(
                onClick = {
                    val newVal = if (nrLevel >= 4) 0 else nrLevel + 1
                    onControl(CONTROL_NR, newVal)
                },
                colors = if (nrLevel > 0) {
                    ButtonDefaults.buttonColors(containerColor = Color(0xFF5078B4))
                } else {
                    ButtonDefaults.buttonColors(containerColor = Color(0xFF404040))
                },
            ) {
                Text(nrLabel, color = Color.White, fontWeight = if (nrLevel > 0) FontWeight.Bold else FontWeight.Normal)
            }

            // ANF toggle
            Button(
                onClick = {
                    val newVal = if (anfOn) 0 else 1
                    onControl(CONTROL_ANF, newVal)
                },
                colors = if (anfOn) {
                    ButtonDefaults.buttonColors(containerColor = Color(0xFF5078B4))
                } else {
                    ButtonDefaults.buttonColors(containerColor = Color(0xFF404040))
                },
            ) {
                Text("ANF", color = Color.White, fontWeight = if (anfOn) FontWeight.Bold else FontWeight.Normal)
            }

            // AGC toggle
            Button(
                onClick = { onAgcToggle(!agcEnabled) },
                colors = if (agcEnabled) {
                    ButtonDefaults.buttonColors(containerColor = Color(0xFF5078B4))
                } else {
                    ButtonDefaults.buttonColors(containerColor = Color(0xFF404040))
                },
            ) {
                Text("AGC", color = Color.White, fontWeight = if (agcEnabled) FontWeight.Bold else FontWeight.Normal)
            }
        }

        // Row 2: NB cycle (OFF → NB1 → NB2 → OFF)
        Row(horizontalArrangement = Arrangement.spacedBy(4.dp)) {
            val nbLabel = when (nbLevel) { 1 -> "NB1"; 2 -> "NB2"; else -> "NB" }
            Button(
                onClick = {
                    val newVal = if (nbLevel >= 2) 0 else nbLevel + 1
                    onControl(CONTROL_NB, newVal)
                },
                colors = if (nbLevel > 0) {
                    ButtonDefaults.buttonColors(containerColor = Color(0xFF5078B4))
                } else {
                    ButtonDefaults.buttonColors(containerColor = Color(0xFF404040))
                },
            ) {
                Text(nbLabel, color = Color.White, fontWeight = if (nbLevel > 0) FontWeight.Bold else FontWeight.Normal)
            }

            // Diversity toggle
            Button(
                onClick = {
                    onControl(CONTROL_DIVERSITY, if (diversityEnabled) 0 else 1)
                },
                colors = if (diversityEnabled) {
                    ButtonDefaults.buttonColors(containerColor = Color(0xFF5078B4))
                } else {
                    ButtonDefaults.buttonColors(containerColor = Color(0xFF404040))
                },
            ) {
                Text("DIV", color = Color.White, fontWeight = if (diversityEnabled) FontWeight.Bold else FontWeight.Normal)
            }
        }

        // Diversity circle plot (visible when diversity enabled)
        AnimatedVisibility(visible = diversityEnabled) {
            DiversityCirclePlot(
                phase = diversityPhase,
                gainRx1 = diversityGainRx1,
                gainRx2 = diversityGainRx2,
                ref = diversityRef,
                autonullResult = diversityAutonullResult,
                onControl = onControl,
            )
        }

        Spacer(Modifier.height(8.dp))

        // TX Profile dropdown — uses server names (TCI) or manual config (CAT)
        val effectiveProfiles = if (serverTxProfileNames.isNotEmpty()) {
            serverTxProfileNames.mapIndexed { i, name -> i to name }
        } else {
            txProfiles
        }
        if (effectiveProfiles.isNotEmpty()) {
            var profileExpanded by remember { mutableStateOf(false) }
            val profileNames = effectiveProfiles.toMap()
            val currentName = profileNames[txProfile] ?: "?"
            Row(
                modifier = Modifier.fillMaxWidth(),
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(8.dp),
            ) {
                Text("TX Profile:")
                Box {
                    Button(
                        onClick = { profileExpanded = true },
                        colors = ButtonDefaults.buttonColors(containerColor = Color(0xFF5078B4)),
                    ) {
                        Text(currentName, fontWeight = FontWeight.Bold)
                    }
                    DropdownMenu(
                        expanded = profileExpanded,
                        onDismissRequest = { profileExpanded = false },
                    ) {
                        effectiveProfiles.forEach { (idx, name) ->
                            DropdownMenuItem(
                                text = { Text(name, fontWeight = if (idx == txProfile) FontWeight.Bold else FontWeight.Normal) },
                                onClick = {
                                    onControl(CONTROL_TX_PROFILE, idx)
                                    profileExpanded = false
                                },
                            )
                        }
                    }
                }
            }
        }

        // TUNE button (Thetis carrier with PA bypass)
        if (connected && powerOn) {
            var tuneActive by remember { mutableStateOf(false) }
            var tunePaWasOperate by remember { mutableStateOf(false) }
            val coroutineScope = rememberCoroutineScope()
            Spacer(Modifier.height(4.dp))
            Row(
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(8.dp),
            ) {
                val tuneColor = if (tuneActive) Color(0xFFDC3C3C) else Color(0xFF505050)
                Button(
                    onClick = {
                        val newTune = !tuneActive
                        if (newTune) {
                            // Starting tune: bypass PA first, then delayed ZZTU1
                            tunePaWasOperate = rf2kOperate || speState == 2
                            if (rf2kOperate) onRf2kOperate(false)
                            if (speState == 2) onSpeOperate()
                            coroutineScope.launch {
                                delay(500) // Wait for PA to go standby
                                onControl(CONTROL_TUNE, 1)
                            }
                        } else {
                            // Stopping tune: ZZTU0 immediately, delayed PA restore
                            onControl(CONTROL_TUNE, 0)
                            if (tunePaWasOperate) {
                                coroutineScope.launch {
                                    delay(1000) // Wait for Thetis TX→RX switch
                                    if (rf2kConnected) onRf2kOperate(true)
                                    if (speConnected) onSpeOperate()
                                }
                                tunePaWasOperate = false
                            }
                        }
                        tuneActive = newTune
                    },
                    colors = ButtonDefaults.buttonColors(containerColor = tuneColor),
                ) {
                    Text(
                        if (tuneActive) "TUNE ON" else "TUNE",
                        color = Color.White,
                        fontWeight = if (tuneActive) FontWeight.Bold else FontWeight.Normal,
                    )
                }
                if (tuneActive) {
                    Text("Carrier ON", color = Color(0xFFFF6464))
                }
            }
        }

        Spacer(Modifier.height(4.dp))

        // Drive level slider
        DriveSlider(driveLevel = driveLevel, onDriveChange = onDriveChange)
    }
}

private val SliderNestedScrollConnection = object : NestedScrollConnection {
    override fun onPreScroll(available: Offset, source: androidx.compose.ui.input.nestedscroll.NestedScrollSource): Offset {
        return Offset(0f, available.y)
    }
}

@Composable
private fun DriveSlider(driveLevel: Int, onDriveChange: (Int) -> Unit) {
    var localDrive by remember(driveLevel) { mutableFloatStateOf(driveLevel.toFloat()) }

    Row(
        modifier = Modifier
            .fillMaxWidth()
            .nestedScroll(SliderNestedScrollConnection),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text("Drive:", modifier = Modifier.weight(0.2f))
        Slider(
            value = localDrive,
            onValueChange = { localDrive = it },
            onValueChangeFinished = { onDriveChange(localDrive.toInt()) },
            valueRange = 0f..100f,
            modifier = Modifier.weight(0.6f),
        )
        Text("${localDrive.toInt()}%", modifier = Modifier.weight(0.2f))
    }
}

// Filter bandwidth presets per mode category
private val SSB_PRESETS = intArrayOf(1800, 2100, 2400, 2700, 3000, 3300, 3600, 4000)
private val CW_PRESETS = intArrayOf(50, 100, 250, 500, 1000)
private val AM_PRESETS = intArrayOf(4000, 6000, 8000, 10000, 12000)
private val FM_PRESETS = intArrayOf(8000, 12000, 16000)

private fun presetsForMode(mode: Int): IntArray = when (mode) {
    0, 1, 7, 9 -> SSB_PRESETS      // LSB, USB, DIGU, DIGL
    3, 4 -> CW_PRESETS              // CWL, CWU
    2, 6, 10, 11 -> AM_PRESETS      // DSB, AM, SAM, DRM
    5 -> FM_PRESETS                  // FM
    else -> SSB_PRESETS
}

private fun isCwMode(mode: Int): Boolean = mode == 3 || mode == 4

private fun formatBandwidth(hz: Int, cw: Boolean): String {
    return if (cw || hz < 1000) {
        "$hz Hz"
    } else {
        val khz = hz / 1000f
        if (khz == khz.toInt().toFloat()) {
            "${khz.toInt()} kHz"
        } else {
            "${"%.1f".format(khz)} kHz"
        }
    }
}

private fun closestPresetIndex(presets: IntArray, bw: Int): Int {
    var bestIdx = 0
    var bestDist = abs(presets[0] - bw)
    for (i in 1 until presets.size) {
        val dist = abs(presets[i] - bw)
        if (dist < bestDist) {
            bestDist = dist
            bestIdx = i
        }
    }
    return bestIdx
}

/**
 * Calculate filter edges respecting mode sideband rules.
 * USB/DIGU: anchor low edge (min 25 Hz), expand upward.
 * LSB/DIGL: anchor high edge (max -25 Hz), expand downward.
 * CW: keep center within sideband.
 * AM/SAM/DSB/DRM/FM: symmetric around 0.
 */
private fun calcFilterEdges(mode: Int, filterLow: Int, filterHigh: Int, newBw: Int): Pair<Int, Int> {
    return when (mode) {
        1, 7 -> {   // USB, DIGU
            val low = filterLow.coerceAtLeast(25)
            Pair(low, low + newBw)
        }
        0, 9 -> {   // LSB, DIGL
            val high = filterHigh.coerceAtMost(-25)
            Pair(high - newBw, high)
        }
        3, 4 -> {   // CWL, CWU
            val center = (filterLow + filterHigh) / 2
            Pair(center - newBw / 2, center + newBw / 2)
        }
        else -> {   // AM, SAM, DSB, DRM, FM
            Pair(-newBw / 2, newBw / 2)
        }
    }
}

@Composable
fun FilterBandwidthControl(
    filterLowHz: Int,
    filterHighHz: Int,
    mode: Int,
    onFilterChange: (low: Int, high: Int) -> Unit,
) {
    val presets = remember(mode) { presetsForMode(mode) }
    val cw = remember(mode) { isCwMode(mode) }
    val currentBw = filterHighHz - filterLowHz
    val currentIdx = remember(currentBw, mode) { closestPresetIndex(presets, currentBw) }

    Row(
        modifier = Modifier.fillMaxWidth(),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.Center,
    ) {
        Button(
            onClick = {
                val newIdx = (currentIdx - 1).coerceAtLeast(0)
                val (low, high) = calcFilterEdges(mode, filterLowHz, filterHighHz, presets[newIdx])
                onFilterChange(low, high)
            },
            enabled = currentIdx > 0,
            colors = ButtonDefaults.buttonColors(containerColor = Color(0xFF404040)),
        ) {
            Text("-", fontWeight = FontWeight.Bold, fontSize = 18.sp)
        }

        Spacer(Modifier.width(12.dp))

        Text(
            text = formatBandwidth(presets[currentIdx], cw),
            fontWeight = FontWeight.Bold,
            fontSize = 16.sp,
            textAlign = TextAlign.Center,
            modifier = Modifier.width(80.dp),
        )

        Spacer(Modifier.width(12.dp))

        Button(
            onClick = {
                val newIdx = (currentIdx + 1).coerceAtMost(presets.size - 1)
                val (low, high) = calcFilterEdges(mode, filterLowHz, filterHighHz, presets[newIdx])
                onFilterChange(low, high)
            },
            enabled = currentIdx < presets.size - 1,
            colors = ButtonDefaults.buttonColors(containerColor = Color(0xFF404040)),
        ) {
            Text("+", fontWeight = FontWeight.Bold, fontSize = 18.sp)
        }
    }
}

// ── Diversity circle plot ──────────────────────────────────────────────

private const val CONTROL_DIVERSITY_PHASE = 0x45
private const val CONTROL_DIVERSITY_GAIN_RX1 = 0x43
private const val CONTROL_DIVERSITY_GAIN_RX2 = 0x44
private const val CONTROL_DIVERSITY_AUTONULL = 0x4A

@Composable
private fun DiversityCirclePlot(
    phase: Float,
    gainRx1: Float,
    gainRx2: Float,
    ref: Int,
    autonullResult: Int,
    onControl: (Int, Int) -> Unit,
) {
    val gainMax = 5f
    val nonRefGain = if (ref == 1) gainRx2 else gainRx1

    Column(
        modifier = Modifier.fillMaxWidth().padding(top = 4.dp),
        horizontalAlignment = Alignment.CenterHorizontally,
    ) {
        // Circle plot
        val plotSize = 180f
        Canvas(modifier = Modifier.size(plotSize.dp)) {
            val cx = size.width / 2f
            val cy = size.height / 2f
            val radius = (size.width / 2f) * 0.9f

            val gridColor = Color(0xFF444466)
            val axisColor = Color(0xFF555588)
            val vectorColor = Color(0xFF00CC88)

            // Concentric gain circles
            for (i in 1..4) {
                val r = radius * i / 4f
                drawCircle(gridColor, r, center = Offset(cx, cy), style = Stroke(1f))
            }
            // Cross axes
            drawLine(axisColor, Offset(cx - radius, cy), Offset(cx + radius, cy), strokeWidth = 1f)
            drawLine(axisColor, Offset(cx, cy - radius), Offset(cx, cy + radius), strokeWidth = 1f)

            // Phase vector
            val phaseRad = Math.toRadians(phase.toDouble()).toFloat()
            val gainNorm = (nonRefGain / gainMax).coerceIn(0f, 1f)
            val tipX = cx + cos(phaseRad) * radius * gainNorm
            val tipY = cy - sin(phaseRad) * radius * gainNorm

            // Vector line
            drawLine(vectorColor, Offset(cx, cy), Offset(tipX, tipY), strokeWidth = 3f)
            // Tip circle
            drawCircle(vectorColor, 6f, center = Offset(tipX, tipY))
        }

        // Readout
        Text(
            text = "Phase: %.1f°  Gain: %.3f".format(phase, nonRefGain),
            color = Color(0xFFC8C8DC),
            fontSize = 12.sp,
        )

        Spacer(Modifier.height(4.dp))

        // Smart Auto Null button with result display
        var autoNullActive by remember { mutableStateOf(false) }
        var seenZero by remember { mutableStateOf(false) }
        var improvementDb by remember { mutableStateOf<Float?>(null) }

        // Detect done via 0→result transition:
        // Server resets to 0 at start, then sets result when done
        if (autoNullActive) {
            if (autonullResult == 0) {
                seenZero = true
            } else if (seenZero) {
                val improvement = (autonullResult - 32000).toFloat() / 10f
                improvementDb = improvement
                autoNullActive = false
                seenZero = false
            }
        }

        Row(
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            Button(
                onClick = {
                    if (!autoNullActive) {
                        autoNullActive = true
                        seenZero = false
                        improvementDb = null
                        onControl(CONTROL_DIVERSITY_AUTONULL, 1)
                    }
                },
                colors = if (autoNullActive) {
                    ButtonDefaults.buttonColors(containerColor = Color(0xFF5078B4))
                } else {
                    ButtonDefaults.buttonColors(containerColor = Color(0xFF404040))
                },
            ) {
                Text(
                    if (autoNullActive) "Smart Null..." else "Smart Null",
                    color = Color.White,
                )
            }

            // Show improvement result
            improvementDb?.let { db ->
                val color = if (db > 0.5f) Color(0xFF00CC00) else Color(0xFFCC6600)
                Text(
                    "%+.1f dB".format(db),
                    color = color,
                    fontSize = 14.sp,
                )
            }
        }

        // Timeout after 60s
        LaunchedEffect(autoNullActive) {
            if (autoNullActive) {
                delay(60000)
                autoNullActive = false
            }
        }
    }
}
