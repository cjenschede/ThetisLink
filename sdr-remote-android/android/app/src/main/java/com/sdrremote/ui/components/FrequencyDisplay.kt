// SPDX-License-Identifier: GPL-2.0-or-later

package com.sdrremote.ui.components

import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.KeyboardActions
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import android.content.Context
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableFloatStateOf
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableLongStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.Modifier
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.nativeCanvas
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.sdrremote.R

private val FREQ_STEPS = longArrayOf(10, 100, 500, 1_000, 10_000)
private val STEP_LABELS = arrayOf("10", "100", "500", "1k", "10k")
private val MODES = arrayOf(0 to "LSB", 1 to "USB", 6 to "AM", 5 to "FM")
private const val NUM_MEMORIES = 5

private data class Memory(val freq: Long, val mode: Int)


/** Parse memory string — supports "freq:mode" (new) and plain "freq" (legacy) */
private fun parseMemories(saved: String?): Array<Memory?> {
    if (saved == null) return Array(NUM_MEMORIES) { null }
    val parts = saved.split(",")
    return Array(NUM_MEMORIES) { i ->
        val part = parts.getOrNull(i) ?: return@Array null
        val segments = part.split(":")
        val freq = segments[0].toLongOrNull()?.takeIf { it > 0 } ?: return@Array null
        val mode = segments.getOrNull(1)?.toIntOrNull() ?: 0
        Memory(freq, mode)
    }
}

private fun serializeMemories(memories: Array<Memory?>): String =
    memories.joinToString(",") { m -> if (m != null) "${m.freq}:${m.mode}" else "0" }

@Composable
fun FrequencyDisplay(
    frequencyHz: Long,
    smeter: Float,
    mode: Int,
    transmitting: Boolean,
    otherTx: Boolean,
    onFrequencyChange: (Long) -> Unit,
    onModeChange: (Int) -> Unit,
) {
    val context = LocalContext.current
    val prefs = remember { context.getSharedPreferences("sdr_remote", Context.MODE_PRIVATE) }

    var stepIndex by rememberSaveable { mutableIntStateOf(3) } // default 1 kHz
    var editing by remember { mutableStateOf(false) }
    var editText by remember { mutableStateOf("") }
    var saveMode by remember { mutableStateOf(false) }

    // Load memories from SharedPreferences (persists across app restarts)
    var memories by remember {
        mutableStateOf(parseMemories(prefs.getString("memories", null)))
    }

    Column(modifier = Modifier.fillMaxWidth()) {
        // VFO A frequency
        if (editing) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Text("VFO A:", fontSize = 18.sp, fontWeight = FontWeight.Bold)
                Spacer(Modifier.width(8.dp))
                OutlinedTextField(
                    value = editText,
                    onValueChange = { editText = it.filter { c -> c.isDigit() } },
                    singleLine = true,
                    keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Number),
                    keyboardActions = KeyboardActions(onDone = {
                        val hz = editText.toLongOrNull()
                        if (hz != null && hz > 0) onFrequencyChange(hz)
                        editing = false
                    }),
                    modifier = Modifier.width(160.dp),
                )
                Spacer(Modifier.width(4.dp))
                Text("Hz", fontSize = 18.sp, fontWeight = FontWeight.Bold)
            }
        } else {
            val freqText = if (frequencyHz > 0) {
                "VFO A:  ${formatFrequency(frequencyHz)} Hz"
            } else {
                "VFO A:  --- Hz"
            }
            Text(
                text = freqText,
                fontSize = 20.sp,
                fontWeight = FontWeight.Bold,
                color = MaterialTheme.colorScheme.onBackground,
                modifier = Modifier.clickable {
                    editing = true
                    editText = if (frequencyHz > 0) frequencyHz.toString() else ""
                },
            )
        }

        Spacer(Modifier.height(4.dp))

        // S-meter bar (3-zone: labels above + bar + labels below, with peak hold)
        SmeterBar(value = smeter, transmitting = transmitting, otherTx = otherTx)

        Spacer(Modifier.height(8.dp))

        // Step buttons row: - / steps / +
        Row(
            modifier = Modifier.fillMaxWidth(),
            horizontalArrangement = Arrangement.spacedBy(4.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Button(
                onClick = {
                    val newHz = (frequencyHz - FREQ_STEPS[stepIndex]).coerceAtLeast(0)
                    onFrequencyChange(newHz)
                },
                modifier = Modifier.weight(1f),
                contentPadding = PaddingValues(horizontal = 4.dp, vertical = 4.dp),
            ) { Text("-", fontSize = 18.sp) }

            STEP_LABELS.forEachIndexed { i, label ->
                val selected = i == stepIndex
                Button(
                    onClick = { stepIndex = i },
                    colors = if (selected) {
                        ButtonDefaults.buttonColors(containerColor = Color(0xFF5078B4))
                    } else {
                        ButtonDefaults.buttonColors(containerColor = Color(0xFF404040))
                    },
                    modifier = Modifier.weight(1f),
                    contentPadding = PaddingValues(horizontal = 4.dp, vertical = 4.dp),
                ) { Text(label, fontSize = 12.sp, maxLines = 1) }
            }

            Button(
                onClick = {
                    val newHz = frequencyHz + FREQ_STEPS[stepIndex]
                    onFrequencyChange(newHz)
                },
                modifier = Modifier.weight(1f),
                contentPadding = PaddingValues(horizontal = 4.dp, vertical = 4.dp),
            ) { Text("+", fontSize = 18.sp) }
        }

        Spacer(Modifier.height(8.dp))

        // Mode buttons
        Row(
            modifier = Modifier.fillMaxWidth(),
            horizontalArrangement = Arrangement.spacedBy(4.dp),
        ) {
            MODES.forEach { (modeVal, label) ->
                val selected = mode == modeVal
                Button(
                    onClick = { onModeChange(modeVal) },
                    colors = if (selected) {
                        ButtonDefaults.buttonColors(containerColor = Color(0xFF5078B4))
                    } else {
                        ButtonDefaults.buttonColors(containerColor = Color(0xFF404040))
                    },
                    modifier = Modifier.weight(1f),
                ) { Text(label, fontWeight = if (selected) FontWeight.Bold else FontWeight.Normal) }
            }
        }

        Spacer(Modifier.height(8.dp))

        // Memory buttons
        Row(
            modifier = Modifier.fillMaxWidth(),
            horizontalArrangement = Arrangement.spacedBy(4.dp),
        ) {
            for (i in 0 until NUM_MEMORIES) {
                val mem = memories[i]
                val label = if (mem != null) bandLabel(mem.freq).ifEmpty { "M${i + 1}" } else "M${i + 1}"
                Button(
                    onClick = {
                        if (saveMode) {
                            if (frequencyHz > 0) {
                                memories = memories.copyOf().also { it[i] = Memory(frequencyHz, mode) }
                                prefs.edit().putString("memories", serializeMemories(memories)).apply()
                            }
                            saveMode = false
                        } else if (mem != null) {
                            onFrequencyChange(mem.freq)
                            onModeChange(mem.mode)
                        }
                    },
                    colors = if (saveMode) {
                        ButtonDefaults.buttonColors(containerColor = Color(0xFF78501E))
                    } else {
                        ButtonDefaults.buttonColors(containerColor = Color(0xFF404040))
                    },
                    modifier = Modifier.weight(1f),
                ) { Text(label, fontSize = 12.sp) }
            }

            Button(
                onClick = { saveMode = !saveMode },
                colors = if (saveMode) {
                    ButtonDefaults.buttonColors(containerColor = Color(0xFF963C1E))
                } else {
                    ButtonDefaults.buttonColors(containerColor = Color(0xFF404040))
                },
                modifier = Modifier.weight(1f),
            ) { Text(stringResource(R.string.common_save), fontWeight = if (saveMode) FontWeight.Bold else FontWeight.Normal) }
        }
    }
}

/// `value` is dBm in RX mode and watts in TX mode, matching the wire format
/// where `SmeterPacket.level` is signed deci-units (×10) of the same quantity.
@Composable
fun SmeterBar(value: Float, transmitting: Boolean = false, otherTx: Boolean = false) {
    // Peak hold state: instant attack, 1-second decay.  Default well below S0
    // so the very first real sample wins.
    var peakLevel by remember { mutableFloatStateOf(-200f) }
    var peakTimeMs by remember { mutableLongStateOf(0L) }

    val now = remember(value) { System.currentTimeMillis() }
    if (!transmitting && !otherTx) {
        if (value >= peakLevel) {
            peakLevel = value
            peakTimeMs = now
        } else if (now - peakTimeMs > 1000L) {
            peakLevel = value
            peakTimeMs = now
        }
    }
    // Reset peak when switching to TX
    LaunchedEffect(transmitting, otherTx) {
        if (transmitting || otherTx) { peakLevel = -200f; peakTimeMs = 0L }
    }

    // 3-zone layout: labels above (10dp) + bar (14dp) + labels below (10dp) = 34dp
    val labelH = 10f  // dp equivalent in canvas coords — scaled below
    val barH = 14f

    Canvas(
        modifier = Modifier
            .fillMaxWidth()
            .height(38.dp),
    ) {
        val density = this.size.height / 38f  // scale factor
        val topLabelH = labelH * density
        val barHeight = barH * density
        val bottomLabelY = topLabelH + barHeight
        val barTop = topLabelH
        val barBottom = topLabelH + barHeight
        val w = size.width

        // Bar background
        drawRect(Color(0xFF141414), topLeft = Offset(0f, barTop), size = Size(w, barHeight))

        val tickFont = android.graphics.Paint().apply {
            color = android.graphics.Color.GRAY
            textSize = 22f
            textAlign = android.graphics.Paint.Align.CENTER
            isAntiAlias = true
        }
        val redTickFont = android.graphics.Paint().apply {
            color = android.graphics.Color.rgb(200, 100, 100)
            textSize = 22f
            textAlign = android.graphics.Paint.Align.CENTER
            isAntiAlias = true
        }
        val centerFont = android.graphics.Paint().apply {
            color = android.graphics.Color.WHITE
            textSize = 32f
            textAlign = android.graphics.Paint.Align.CENTER
            isAntiAlias = true
        }

        if (otherTx) {
            // ── Other TX: orange bar ──
            val watts = value
            val frac = (watts / 100f).coerceIn(0f, 1f)
            drawRect(Color(0xFFCC7700), topLeft = Offset(0f, barTop), size = Size(w * frac, barHeight))
            drawContext.canvas.nativeCanvas.drawText(
                "TX in use  ${"%.0f".format(watts)} W", w / 2f, barTop + barHeight / 2f + 10f, centerFont)
        } else if (transmitting) {
            // ── TX power: red bar + watt ticks ──
            val watts = value
            val frac = (watts / 100f).coerceIn(0f, 1f)
            drawRect(Color(0xFFDC1E1E), topLeft = Offset(0f, barTop), size = Size(w * frac, barHeight))

            for (watt in intArrayOf(25, 50, 75, 100)) {
                val x = w * (watt / 100f)
                // Ticks above and below bar
                drawLine(Color.Gray, Offset(x, barTop), Offset(x, barTop + 4f * density), strokeWidth = 1f)
                drawLine(Color.Gray, Offset(x, barBottom - 4f * density), Offset(x, barBottom), strokeWidth = 1f)
                // Watt labels above bar
                drawContext.canvas.nativeCanvas.drawText("${watt}W", x, topLabelH - 2f, tickFont)
            }

            drawContext.canvas.nativeCanvas.drawText(
                "TX  ${"%.0f".format(watts)} W", w / 2f, barTop + barHeight / 2f + 10f, centerFont)
        } else {
            // ── RX S-meter bar ── `value` is dBm.
            val dbm = value
            val raw = dbmToDisplay(dbm)
            val frac = (raw / 228f).coerceIn(0f, 1f)
            val fillWidth = w * frac
            val s9Frac = 108f / 228f

            // Green up to S9, red above
            if (frac <= s9Frac) {
                drawRect(Color(0xFF00B400), topLeft = Offset(0f, barTop), size = Size(fillWidth, barHeight))
            } else {
                val greenW = w * s9Frac
                drawRect(Color(0xFF00B400), topLeft = Offset(0f, barTop), size = Size(greenW, barHeight))
                drawRect(Color(0xFFDC1E1E), topLeft = Offset(greenW, barTop), size = Size(fillWidth - greenW, barHeight))
            }

            // Peak hold needle (yellow, 2px)
            if (peakLevel > dbm) {
                val peakRaw = dbmToDisplay(peakLevel)
                val peakFrac = (peakRaw / 228f).coerceIn(0f, 1f)
                val peakX = w * peakFrac
                drawLine(Color(0xFFFFFF00), Offset(peakX, barTop), Offset(peakX, barBottom), strokeWidth = 2f)
            }

            // S-unit ticks + labels above bar (S1-S9)
            for (s in 1..9) {
                val x = w * (s * 12f / 228f)
                drawLine(Color.Gray, Offset(x, barTop), Offset(x, barTop + 4f * density), strokeWidth = 1f)
                drawLine(Color.Gray, Offset(x, barBottom - 4f * density), Offset(x, barBottom), strokeWidth = 1f)
                drawContext.canvas.nativeCanvas.drawText("$s", x, topLabelH - 2f, tickFont)
            }

            // +dB ticks + labels below bar (+10 to +60)
            for (dbOver in 10..60 step 10) {
                val tickRaw = 108f + dbOver * 2f
                val x = w * (tickRaw / 228f)
                drawLine(Color(0xFFC86464), Offset(x, barTop), Offset(x, barTop + 4f * density), strokeWidth = 1f)
                drawLine(Color(0xFFC86464), Offset(x, barBottom - 4f * density), Offset(x, barBottom), strokeWidth = 1f)
                drawContext.canvas.nativeCanvas.drawText("+$dbOver", x, size.height - 1f, redTickFont)
            }

            // S-value text centered on bar — derived directly from dBm.
            val sText = if (dbm <= -73f) {
                val sUnit = ((dbm + 127f) / 6f).toInt().coerceIn(0, 9)
                "S$sUnit"
            } else {
                val dbOver = (dbm + 73f).toInt().coerceAtLeast(0)
                "S9+$dbOver dB"
            }
            drawContext.canvas.nativeCanvas.drawText(sText, w / 2f, barTop + barHeight / 2f + 10f, centerFont)
        }
    }
}

/// Mirror of `sdr_remote_core::dbm_to_display` — maps dBm onto the 0..228
/// visual range. S0=-127 dBm, S9=-73 dBm, 6 dB per S-unit, 2 raw units per dB
/// throughout (S1..S9 and the S9+dB zone use the same scaling).
private fun dbmToDisplay(dbm: Float): Float {
    return if (dbm <= -73f) {
        ((dbm + 127f) * 2f).coerceIn(0f, 108f)
    } else {
        (108f + (dbm + 73f) * 2f).coerceIn(108f, 228f)
    }
}

private fun formatFrequency(hz: Long): String {
    val s = hz.toString()
    val sb = StringBuilder()
    for (i in s.indices) {
        if (i > 0 && (s.length - i) % 3 == 0) sb.append('.')
        sb.append(s[i])
    }
    return sb.toString()
}

private fun bandLabel(hz: Long): String = when (hz) {
    in 1_800_000..1_999_999 -> "160m"
    in 3_500_000..3_999_999 -> "80m"
    // Wide on purpose, and the same range the desktop uses: 60m is channelised
    // and the channels differ per country. Label only.
    in 5_250_000..5_449_999 -> "60m"
    in 7_000_000..7_299_999 -> "40m"
    in 10_100_000..10_149_999 -> "30m"
    in 14_000_000..14_349_999 -> "20m"
    in 18_068_000..18_167_999 -> "17m"
    in 21_000_000..21_449_999 -> "15m"
    in 24_890_000..24_989_999 -> "12m"
    in 28_000_000..29_699_999 -> "10m"
    in 50_000_000..53_999_999 -> "6m"
    else -> ""
}
