// SPDX-License-Identifier: GPL-2.0-or-later

package com.sdrremote.ui.components

import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.RowScope
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.width
import androidx.compose.material3.OutlinedButton
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Slider
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableFloatStateOf
import androidx.compose.runtime.mutableLongStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.input.nestedscroll.NestedScrollConnection
import androidx.compose.ui.input.nestedscroll.nestedScroll
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.sdrremote.R
import uniffi.sdr_remote.version
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
        VolumeSlider(label = stringResource(R.string.stats_rx_volume), initial = rxVolume, onChange = onRxVolumeChange)
        VolumeSlider(label = stringResource(R.string.stats_tx_gain), initial = txGain, maxValue = 3f, onChange = onTxGainChange)
    }
}

/**
 * Audio levels + network/audio stats - recomposes at 30fps, no sliders.
 */
@Composable
fun AudioStats(
    captureLevel: Float,
    playbackLevel: Float,
    serverBuildDiffers: Boolean,
    serverBuild: String,
    rttMs: Int,
    jitterMs: Float,
    bufferDepth: Int,
    lossPercent: Int,
    rxPackets: Long,
    yaesuAudioPackets: Long,
    yaesuJitterMs: Float,
    yaesuBufferDepth: Int,
    yaesu2AudioPackets: Long,
    yaesu2JitterMs: Float,
    yaesu2BufferDepth: Int,
    vrx1AudioPackets: Long,
    vrx1JitterMs: Float,
    vrx1BufferDepth: Int,
    vrx2AudioPackets: Long,
    vrx2JitterMs: Float,
    vrx2BufferDepth: Int,
    downKbps: Int,
    upKbps: Int,
    showThetisAudio: Boolean,
    yaesu1Visible: Boolean,
    yaesu2Visible: Boolean,
    vrx1Visible: Boolean = false,
    vrx2Visible: Boolean = false,
    // Measurement mode (open point 34): the file being written, and the switch.
    measurementFile: String? = null,
    onMeasurement: ((Boolean) -> Unit)? = null,
    // "Relay only" (build 137): grey unless the connection runs through the relay.
    relayOnly: Boolean = false,
    viaRelay: Boolean = false,
    onRelayOnly: ((Boolean) -> Unit)? = null,
    // "Force TCP" (build 146): the same kind of switch, for measuring the TCP route.
    forceTcp: Boolean = false,
    onForceTcp: ((Boolean) -> Unit)? = null,
) {
    Column(modifier = Modifier.fillMaxWidth()) {
        Text(stringResource(R.string.stats_audio_levels), fontSize = 14.sp)
        LevelMeter(label = "MIC", level = captureLevel)
        LevelMeter(label = "RX", level = playbackLevel)

        Spacer(Modifier.height(8.dp))

        Text(stringResource(R.string.stats_statistics), fontSize = 14.sp)
        StatsGrid(
            serverBuildDiffers = serverBuildDiffers,
            serverBuild = serverBuild,
            rttMs = rttMs,
            jitterMs = jitterMs,
            bufferDepth = bufferDepth,
            lossPercent = lossPercent,
            rxPackets = rxPackets,
            yaesuAudioPackets = yaesuAudioPackets,
            yaesuJitterMs = yaesuJitterMs,
            yaesuBufferDepth = yaesuBufferDepth,
            yaesu2AudioPackets = yaesu2AudioPackets,
            yaesu2JitterMs = yaesu2JitterMs,
            yaesu2BufferDepth = yaesu2BufferDepth,
            vrx1AudioPackets = vrx1AudioPackets,
            vrx1JitterMs = vrx1JitterMs,
            vrx1BufferDepth = vrx1BufferDepth,
            vrx2AudioPackets = vrx2AudioPackets,
            vrx2JitterMs = vrx2JitterMs,
            vrx2BufferDepth = vrx2BufferDepth,
            downKbps = downKbps,
            upKbps = upKbps,
            showThetisAudio = showThetisAudio,
            yaesu1Visible = yaesu1Visible,
            yaesu2Visible = yaesu2Visible,
            vrx1Visible = vrx1Visible,
            vrx2Visible = vrx2Visible,
        )
        if (onMeasurement != null) {
            Spacer(Modifier.height(4.dp))
            Row(verticalAlignment = Alignment.CenterVertically) {
                Text(stringResource(R.string.stats_measure), fontSize = 12.sp)
                Spacer(Modifier.width(8.dp))
                OutlinedButton(
                    onClick = { onMeasurement(measurementFile == null) },
                    contentPadding = PaddingValues(horizontal = 8.dp, vertical = 2.dp),
                ) {
                    Text(
                        stringResource(if (measurementFile == null) R.string.stats_measure_start else R.string.stats_measure_stop),
                        fontSize = 11.sp,
                    )
                }
                if (measurementFile != null) {
                    Spacer(Modifier.width(8.dp))
                    Text(measurementFile.substringAfterLast('/'), fontSize = 11.sp, color = Color(0xFFFFAA28))
                }
            }
            if (onRelayOnly != null) {
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Text(stringResource(R.string.stats_relay_only), fontSize = 12.sp)
                    Spacer(Modifier.width(8.dp))
                    Switch(checked = relayOnly, enabled = viaRelay, onCheckedChange = { onRelayOnly(it) })
                }
                Text(
                    stringResource(if (viaRelay) R.string.stats_relay_only_hint else R.string.stats_relay_only_no_relay),
                    fontSize = 11.sp,
                    color = Color.Gray,
                )
            }
            if (onForceTcp != null) {
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Text(stringResource(R.string.stats_force_tcp), fontSize = 12.sp)
                    Spacer(Modifier.width(8.dp))
                    Switch(checked = forceTcp, enabled = viaRelay, onCheckedChange = { onForceTcp(it) })
                }
                Text(
                    stringResource(if (viaRelay) R.string.stats_force_tcp_hint else R.string.stats_relay_only_no_relay),
                    fontSize = 11.sp,
                    color = Color.Gray,
                )
            }
        }
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
    // Peak-hold: hold the recent maximum ~1.5 s, then snap down. Mirrors the desktop
    // level bar - a white tick marks the held peak and the dB text follows it.
    var peak by remember { mutableFloatStateOf(0f) }
    var peakTimeMs by remember { mutableLongStateOf(0L) }
    val clamped = level.coerceIn(0f, 1f)
    val now = remember(level) { System.currentTimeMillis() }
    if (clamped >= peak) {
        peak = clamped
        peakTimeMs = now
    } else if (now - peakTimeMs > 1500L) {
        peak = clamped
        peakTimeMs = now
    }

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
            val fillWidth = size.width * clamped

            val barColor = when {
                clamped < 0.5f -> Color(0xFF00C800)
                clamped < 0.8f -> Color(0xFFC8C800)
                else -> Color(0xFFC80000)
            }

            drawRect(barColor, size = Size(fillWidth, size.height))

            // Peak-hold marker: thin white vertical tick at the held peak.
            if (peak > 0.01f) {
                val peakX = size.width * peak
                drawLine(
                    color = Color.White,
                    start = Offset(peakX, 0f),
                    end = Offset(peakX, size.height),
                    strokeWidth = 2f,
                )
            }
        }

        // dB text shows the PEAK-HOLD value (the held maximum), not the flickering
        // instantaneous level.
        val db = if (peak > 0.0001f) {
            (20f * log10(peak)).toInt()
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
    serverBuildDiffers: Boolean,
    serverBuild: String,
    rttMs: Int,
    jitterMs: Float,
    bufferDepth: Int,
    lossPercent: Int,
    rxPackets: Long,
    yaesuAudioPackets: Long,
    yaesuJitterMs: Float,
    yaesuBufferDepth: Int,
    yaesu2AudioPackets: Long,
    yaesu2JitterMs: Float,
    yaesu2BufferDepth: Int,
    vrx1AudioPackets: Long,
    vrx1JitterMs: Float,
    vrx1BufferDepth: Int,
    vrx2AudioPackets: Long,
    vrx2JitterMs: Float,
    vrx2BufferDepth: Int,
    downKbps: Int,
    upKbps: Int,
    showThetisAudio: Boolean,
    yaesu1Visible: Boolean,
    yaesu2Visible: Boolean,
    vrx1Visible: Boolean,
    vrx2Visible: Boolean,
) {
    Column(modifier = Modifier.padding(start = 8.dp)) {
        // All ThetisLink binaries of a release are meant to be the same build,
        // and without something that looks, that is an agreement nobody checks.
        // It only tells - being locked out of your own station at a distance by
        // a half-finished update is worse than reading this line.
        if (serverBuildDiffers) {
            Text(
                text = stringResource(
                    R.string.stats_build_mismatch,
                    serverBuild.ifEmpty { stringResource(R.string.stats_build_too_old) },
                    version(),
                ),
                fontSize = 12.sp,
                color = Color(0xFFDC9628),
            )
            Spacer(Modifier.height(6.dp))
        }
        StatRow("RTT", "$rttMs ms")
        StatRow(stringResource(R.string.stats_down), "$downKbps Kbit/s")
        StatRow(stringResource(R.string.stats_up), "$upKbps Kbit/s")
        // Loss belongs HERE, with the other facts about the connection, and not
        // in the stream table below. Every stream to this client goes down one
        // socket to one address, so there is one loss figure and it is about the
        // link. It used to sit in the "Thetis RX" row with a dash on the other
        // four, which read as "no measurement for those" - and since the meter
        // now feeds on whatever streams are running, a Yaesu-only station would
        // have shown four dashes while it was measuring the Yaesu.
        StatRow(stringResource(R.string.stats_link_loss), "$lossPercent%")

        Spacer(Modifier.height(6.dp))
        Text(stringResource(R.string.stats_audio_streams), fontSize = 13.sp, color = Color.Gray)
        AudioStreamHeader()

        var shown = false
        if (showThetisAudio) {
            shown = true
            AudioStreamRow("Thetis RX", jitterMs, bufferDepth, rxPackets)
        }
        if (yaesu1Visible) {
            shown = true
            AudioStreamRow("Yaesu 1", yaesuJitterMs, yaesuBufferDepth, yaesuAudioPackets)
        }
        if (yaesu2Visible) {
            shown = true
            AudioStreamRow("Yaesu 2", yaesu2JitterMs, yaesu2BufferDepth, yaesu2AudioPackets)
        }
        if (vrx1Visible) {
            shown = true
            AudioStreamRow("VRX1", vrx1JitterMs, vrx1BufferDepth, vrx1AudioPackets)
        }
        if (vrx2Visible) {
            shown = true
            AudioStreamRow("VRX2", vrx2JitterMs, vrx2BufferDepth, vrx2AudioPackets)
        }
        if (!shown) {
            Text(stringResource(R.string.stats_no_stream), fontSize = 12.sp, color = Color.Gray)
        }
    }
}

@Composable
private fun AudioStreamHeader() {
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .padding(top = 2.dp, bottom = 1.dp),
        horizontalArrangement = Arrangement.spacedBy(6.dp),
    ) {
        AudioCell(stringResource(R.string.stats_col_stream), 0.26f, header = true)
        AudioCell(stringResource(R.string.stats_col_jitter), 0.20f, header = true)
        AudioCell(stringResource(R.string.stats_col_buffer), 0.18f, header = true)
        AudioCell(stringResource(R.string.stats_col_packets), 0.24f, header = true)
    }
}

@Composable
private fun AudioStreamRow(label: String, jitterMs: Float, bufferDepth: Int, packets: Long) {
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .padding(vertical = 1.dp),
        horizontalArrangement = Arrangement.spacedBy(6.dp),
    ) {
        AudioCell(label, 0.26f)
        AudioCell("%.1f ms".format(jitterMs), 0.20f)
        AudioCell(bufferDepth.toString(), 0.18f)
        AudioCell(packets.toString(), 0.24f)
    }
}

@Composable
private fun RowScope.AudioCell(text: String, weight: Float, header: Boolean = false) {
    Text(
        text = text,
        modifier = Modifier.weight(weight),
        fontSize = if (header) 11.sp else 12.sp,
        color = if (header) Color.Gray else Color.Unspecified,
    )
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
