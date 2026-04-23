package com.sdrremote.ui.components

import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Slider
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableFloatStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.input.nestedscroll.NestedScrollConnection
import androidx.compose.ui.input.nestedscroll.nestedScroll
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import kotlin.math.log10

// Consumes vertical scroll on slider rows so LazyColumn doesn't intercept horizontal drags
private val SliderNestedScrollConnection = object : NestedScrollConnection {
    override fun onPreScroll(available: Offset, source: androidx.compose.ui.input.nestedscroll.NestedScrollSource): Offset {
        return Offset(0f, available.y)
    }
}

/**
 * Volume sliders only — put in separate LazyColumn item to avoid 30fps recomposition.
 */
@Composable
fun VolumeControls(
    rxVolume: Float,
    txGain: Float,
    onRxVolumeChange: (Float) -> Unit,
    onTxGainChange: (Float) -> Unit,
) {
    Column(modifier = Modifier.fillMaxWidth()) {
        VolumeSlider(label = "RX Volume", initial = rxVolume, onChange = onRxVolumeChange)
        VolumeSlider(label = "TX Gain", initial = txGain, maxValue = 3f, onChange = onTxGainChange)
    }
}

/**
 * Audio levels + network stats — recomposes at 30fps, no sliders.
 */
@Composable
fun AudioStats(
    captureLevel: Float,
    playbackLevel: Float,
    rttMs: Int,
    jitterMs: Float,
    bufferDepth: Int,
    lossPercent: Int,
    rxPackets: Long,
) {
    Column(modifier = Modifier.fillMaxWidth()) {
        Text("Audio Levels:", fontSize = 14.sp)
        LevelMeter(label = "MIC", level = captureLevel)
        LevelMeter(label = "RX", level = playbackLevel)

        Spacer(Modifier.height(8.dp))

        Text("Statistics:", fontSize = 14.sp)
        StatsGrid(
            rttMs = rttMs,
            jitterMs = jitterMs,
            bufferDepth = bufferDepth,
            lossPercent = lossPercent,
            rxPackets = rxPackets,
        )
    }
}

@Composable
private fun VolumeSlider(label: String, initial: Float, maxValue: Float = 1f, logarithmic: Boolean = false, onChange: (Float) -> Unit) {
    // For logarithmic sliders: map between linear slider position (0..1) and log value (0.001..max)
    val logMin = 0.001f
    fun valueToSlider(v: Float): Float = if (logarithmic && maxValue > 0f) {
        val clamped = v.coerceIn(logMin, maxValue)
        (kotlin.math.ln(clamped) - kotlin.math.ln(logMin)) /
            (kotlin.math.ln(maxValue) - kotlin.math.ln(logMin))
    } else v / maxValue
    fun sliderToValue(s: Float): Float = if (logarithmic) {
        kotlin.math.exp(kotlin.math.ln(logMin) + s * (kotlin.math.ln(maxValue) - kotlin.math.ln(logMin)))
    } else s * maxValue

    var sliderPos by remember(initial) { mutableFloatStateOf(valueToSlider(initial)) }

    Row(
        modifier = Modifier
            .fillMaxWidth()
            .nestedScroll(SliderNestedScrollConnection),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text(
            text = "$label:",
            modifier = Modifier.weight(0.25f),
            fontSize = 14.sp,
        )
        Slider(
            value = sliderPos,
            onValueChange = { sliderPos = it },
            onValueChangeFinished = { onChange(sliderToValue(sliderPos)) },
            valueRange = 0f..1f,
            modifier = Modifier.weight(0.55f),
        )
        Text(
            text = "${(sliderToValue(sliderPos) * 100).toInt()}%",
            modifier = Modifier.weight(0.2f),
            fontSize = 14.sp,
        )
    }
}

@Composable
private fun LevelMeter(label: String, level: Float) {
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .padding(vertical = 2.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        Text(text = "$label:", modifier = Modifier.weight(0.15f), fontSize = 13.sp)

        Canvas(
            modifier = Modifier
                .weight(0.65f)
                .height(16.dp)
                .background(Color(0xFF1E1E1E), RoundedCornerShape(2.dp)),
        ) {
            val clamped = level.coerceIn(0f, 1f)
            val fillWidth = size.width * clamped

            val barColor = when {
                clamped < 0.5f -> Color(0xFF00C800)
                clamped < 0.8f -> Color(0xFFC8C800)
                else -> Color(0xFFC80000)
            }

            drawRect(barColor, size = Size(fillWidth, size.height))
        }

        val db = if (level > 0.0001f) {
            (20f * log10(level)).toInt()
        } else {
            -80
        }
        Text(
            text = "$db dB",
            modifier = Modifier.weight(0.2f),
            fontSize = 11.sp,
        )
    }
}

@Composable
private fun StatsGrid(
    rttMs: Int,
    jitterMs: Float,
    bufferDepth: Int,
    lossPercent: Int,
    rxPackets: Long,
) {
    Column(modifier = Modifier.padding(start = 8.dp)) {
        StatRow("RTT", "$rttMs ms")
        StatRow("Jitter", "${"%.1f".format(jitterMs)} ms")
        StatRow("Buffer", "$bufferDepth frames")
        StatRow("Loss", "$lossPercent%")
        StatRow("RX packets", "$rxPackets")
    }
}

@Composable
private fun StatRow(label: String, value: String) {
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .padding(vertical = 1.dp),
    ) {
        Text(text = "$label:", modifier = Modifier.weight(0.4f), fontSize = 13.sp, color = Color.Gray)
        Text(text = value, modifier = Modifier.weight(0.6f), fontSize = 13.sp)
    }
}
