package com.sdrremote.ui.components

import android.graphics.Bitmap
import android.graphics.Paint
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.Image
import androidx.compose.foundation.gestures.detectTapGestures
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.width
import androidx.compose.material3.Slider
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.graphics.nativeCanvas
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.sdrremote.DxSpotInfo
import kotlin.math.ceil
import kotlin.math.floor
import kotlin.math.ln
import kotlin.math.log10
import kotlin.math.pow
import kotlin.math.roundToInt

// Server spectrum encoding constants
private const val SERVER_FLOOR_DB = -150f
private const val SERVER_RANGE_DB = 120f // bins span -150 to -30 dB

/**
 * Spectrum plot: line graph of power vs frequency.
 * Matches desktop client architecture: separate display center (VFO) and bins center (server).
 * Uses pixel→frequency→bin mapping to decouple display position from bin indices.
 */
@Composable
fun SpectrumPlot(
    bins: List<UByte>,
    centerFreqHz: Long,       // bins center (DDC center at zoom=1)
    spanHz: Long,             // bins span
    displayCenterHz: Long,    // display center (VFO frequency — like waterfall)
    vfoHz: Long,              // VFO marker position (pinned during pending freq change)
    filterLowHz: Int,
    filterHighHz: Int,
    refDb: Float,
    rangeDb: Float,
    smeter: Int,
    transmitting: Boolean,
    otherTx: Boolean,
    dxSpots: List<DxSpotInfo> = emptyList(),
    onFrequencyClick: (Long) -> Unit,
    modifier: Modifier = Modifier,
) {
    if (bins.isEmpty() || spanHz == 0L) return

    val numBins = bins.size

    // Display window centered on VFO (like waterfall)
    val startHz = displayCenterHz - spanHz / 2
    val endHz = startHz + spanHz

    // Bins frequency range (may differ from display window during CTUN at zoom=1)
    val binsStartHz = centerFreqHz - spanHz / 2

    val labelStripHeight = 18f

    Canvas(
        modifier = modifier
            .fillMaxWidth()
            .height(140.dp)
            .pointerInput(spanHz, displayCenterHz) {
                detectTapGestures { offset ->
                    val frac = offset.x / size.width.toFloat()
                    val freq = startHz + (frac * spanHz).toLong()
                    if (freq > 1000L) {
                        val rounded = (freq / 1000) * 1000
                        onFrequencyClick(rounded)
                    }
                }
            }
    ) {
        val w = size.width
        val h = size.height
        val plotH = h - labelStripHeight
        val visibleSpan = spanHz.toDouble()
        val floorDb = refDb - rangeDb

        // Background
        drawRect(Color(10, 15, 30), topLeft = Offset(0f, 0f), size = Size(w, plotH))
        drawRect(Color(18, 22, 40), topLeft = Offset(0f, plotH), size = Size(w, labelStripHeight))

        // ── Dynamic grid spacing (nice numbers: 1, 2, 5 × 10^n) ──────────

        val tickSpacingHz = run {
            val raw = visibleSpan / 7.0
            val p = 10.0.pow(floor(log10(raw)))
            val n = raw / p
            val nice = if (n < 1.5) 1.0 else if (n < 3.5) 2.0 else if (n < 7.5) 5.0 else 10.0
            nice * p
        }

        val dbSpacing = run {
            val raw = rangeDb / 6f
            val p = 10f.pow(floor(log10(raw)))
            val n = raw / p
            val nice = if (n < 1.5f) 1f else if (n < 3.5f) 2f else if (n < 7.5f) 5f else 10f
            nice * p
        }

        // ── Grid lines ──────────────────────────────────────────────────────
        val gridColor = Color(60, 60, 85)
        val tickColor = Color(80, 80, 110)

        val firstTick = ceil(startHz.toDouble() / tickSpacingHz).toLong()
        val lastTick = floor(endHz.toDouble() / tickSpacingHz).toLong()
        for (tickIdx in firstTick..lastTick) {
            val freq = tickIdx.toDouble() * tickSpacingHz
            val frac = ((freq - startHz.toDouble()) / visibleSpan).toFloat()
            if (frac < 0.01f || frac > 0.99f) continue
            val x = frac * w
            drawLine(gridColor, Offset(x, 0f), Offset(x, plotH), strokeWidth = 1f)
            drawLine(tickColor, Offset(x, plotH), Offset(x, plotH + 4f), strokeWidth = 1f)
        }

        val firstDbTick = ceil(floorDb / dbSpacing).toInt()
        val lastDbTick = floor(refDb / dbSpacing).toInt()
        for (dbIdx in firstDbTick..lastDbTick) {
            val db = dbIdx.toFloat() * dbSpacing
            val frac = (refDb - db) / rangeDb
            if (frac < 0.02f || frac > 0.98f) continue
            val y = frac * plotH
            drawLine(gridColor, Offset(0f, y), Offset(w, y), strokeWidth = 1f)
        }

        // ── Filter passband background ──────────────────────────────────────
        if (vfoHz > 0 && (filterLowHz != 0 || filterHighHz != 0)) {
            val loHz = vfoHz + filterLowHz
            val hiHz = vfoHz + filterHighHz
            val loFrac = (loHz - startHz).toFloat() / spanHz.toFloat()
            val hiFrac = (hiHz - startHz).toFloat() / spanHz.toFloat()
            val loX = (loFrac * w).coerceIn(0f, w)
            val hiX = (hiFrac * w).coerceIn(0f, w)
            if (hiX > loX) {
                drawRect(
                    Color(25, 30, 55),
                    topLeft = Offset(loX, 0f),
                    size = Size(hiX - loX, plotH),
                )
                val edgeColor = Color(200, 200, 0, 120)
                if (loX > 0f) {
                    drawLine(edgeColor, Offset(loX, 0f), Offset(loX, plotH), strokeWidth = 1f)
                }
                if (hiX < w) {
                    drawLine(edgeColor, Offset(hiX, 0f), Offset(hiX, plotH), strokeWidth = 1f)
                }
            }
        }

        // ── VFO frequency label (behind spectrum) ───────────────────────────
        var vfoX: Float? = null
        var vfoTextTop = 0f
        var vfoTextBottom = 0f
        if (vfoHz > 0 && spanHz > 0) {
            val frac = (vfoHz - startHz).toFloat() / spanHz.toFloat()
            if (frac in 0f..1f) {
                val x = frac * w
                vfoX = x
                val vfoMhz = vfoHz.toDouble() / 1_000_000.0
                val vfoText = "%.3f".format(vfoMhz)
                val vfoPaint = Paint().apply {
                    color = android.graphics.Color.rgb(255, 120, 120)
                    textSize = 28f
                    textAlign = Paint.Align.CENTER
                }
                val textW = vfoPaint.measureText(vfoText)
                val bgPaint = Paint().apply {
                    color = android.graphics.Color.argb(220, 10, 15, 30)
                }
                vfoTextTop = 2f
                vfoTextBottom = 32f
                drawContext.canvas.nativeCanvas.drawRect(
                    x - textW / 2 - 4f, vfoTextTop, x + textW / 2 + 4f, vfoTextBottom, bgPaint
                )
                drawContext.canvas.nativeCanvas.drawText(vfoText, x, 26f, vfoPaint)
            }
        }

        // ── Fill under curve + spectrum line with level-dependent colors ───
        val pixelCount = w.toInt().coerceAtLeast(1)
        val hzPerBin = if (numBins > 0) visibleSpan / numBins.toDouble() else 1.0
        val binsStartD = binsStartHz.toDouble()
        val startD = startHz.toDouble()
        // Collect points with level fraction
        data class SpecPoint(val x: Float, val y: Float, val level: Float)
        val specPoints = ArrayList<SpecPoint>(pixelCount)
        for (px in 0 until pixelCount) {
            val freq0 = startD + (px.toDouble() / pixelCount) * visibleSpan
            val freq1 = startD + ((px + 1).toDouble() / pixelCount) * visibleSpan
            val b0 = ((freq0 - binsStartD) / hzPerBin).coerceAtLeast(0.0)
            val b1 = ((freq1 - binsStartD) / hzPerBin).coerceAtLeast(0.0)
            val bs = b0.toInt()
            val be = ceil(b1).toInt().coerceAtLeast(bs + 1)
            val maxVal = if (bs >= numBins) {
                0
            } else {
                var mv = 0
                for (j in bs.coerceAtLeast(0) until be.coerceAtMost(numBins)) {
                    mv = maxOf(mv, bins[j].toInt())
                }
                mv
            }
            val db = SERVER_FLOOR_DB + (maxVal.toFloat() / 255f) * SERVER_RANGE_DB
            val frac = (refDb - db) / rangeDb
            val y = (frac * plotH).coerceIn(0f, plotH)
            val level = (1f - frac).coerceIn(0f, 1f)
            specPoints.add(SpecPoint(px.toFloat(), y, level))
        }
        // Draw fill: per-column vertical line from spectrum to bottom
        for (pt in specPoints) {
            val c = spectrumLevelColor(pt.level)
            drawLine(c.copy(alpha = 0.15f), Offset(pt.x, pt.y), Offset(pt.x, plotH), strokeWidth = 1f)
        }
        // Draw spectrum line: per-segment with level color
        for (i in 1 until specPoints.size) {
            val p0 = specPoints[i - 1]
            val p1 = specPoints[i]
            val avgLevel = (p0.level + p1.level) / 2f
            val c = spectrumLevelColor(avgLevel)
            drawLine(c, Offset(p0.x, p0.y), Offset(p1.x, p1.y), strokeWidth = 2f)
        }

        // ── VFO line (on top of spectrum, interrupted at text) ──────────────
        vfoX?.let { x ->
            val vfoColor = Color(255, 50, 50, 180)
            if (vfoTextBottom > 0f) {
                if (vfoTextTop > 0f) {
                    drawLine(vfoColor, Offset(x, 0f), Offset(x, vfoTextTop), strokeWidth = 3f)
                }
                drawLine(vfoColor, Offset(x, vfoTextBottom), Offset(x, plotH), strokeWidth = 3f)
            } else {
                drawLine(vfoColor, Offset(x, 0f), Offset(x, plotH), strokeWidth = 3f)
            }
        }

        // ── Frequency labels in label strip ─────────────────────────────────
        val freqLabelPaint = Paint().apply {
            color = android.graphics.Color.rgb(220, 220, 230)
            textSize = 18f
            textAlign = Paint.Align.CENTER
        }
        for (tickIdx in firstTick..lastTick) {
            val freq = tickIdx.toDouble() * tickSpacingHz
            val frac = ((freq - startHz.toDouble()) / visibleSpan).toFloat()
            if (frac < 0.02f || frac > 0.98f) continue
            val x = frac * w
            val freqMhz = freq / 1_000_000.0
            val label = when {
                tickSpacingHz >= 1_000_000.0 -> "%.0f".format(freqMhz)
                tickSpacingHz >= 100_000.0 -> "%.1f".format(freqMhz)
                tickSpacingHz >= 10_000.0 -> "%.2f".format(freqMhz)
                tickSpacingHz >= 1_000.0 -> "%.3f".format(freqMhz)
                else -> "%.4f".format(freqMhz)
            }
            drawContext.canvas.nativeCanvas.drawText(label, x, plotH + labelStripHeight - 3f, freqLabelPaint)
        }

        // ── dB labels at left edge ──────────────────────────────────────────
        val dbLabelPaint = Paint().apply {
            color = android.graphics.Color.rgb(200, 200, 210)
            textSize = 16f
            textAlign = Paint.Align.LEFT
        }
        val dbBgPaint = Paint().apply {
            color = android.graphics.Color.argb(220, 10, 15, 30)
        }
        for (dbIdx in firstDbTick..lastDbTick) {
            val db = dbIdx.toFloat() * dbSpacing
            val frac = (refDb - db) / rangeDb
            if (frac < 0.02f || frac > 0.98f) continue
            val y = frac * plotH
            val dbText = "%.0f".format(db)
            val textW = dbLabelPaint.measureText(dbText)
            val textH = 14f
            drawContext.canvas.nativeCanvas.drawRect(
                2f, y - 1f, 2f + textW + 4f, y + textH + 1f, dbBgPaint
            )
            drawContext.canvas.nativeCanvas.drawText(dbText, 4f, y + textH - 1f, dbLabelPaint)
        }

        // ── Band markers ────────────────────────────────────────────────────
        val bands = listOf(
            1.8e6f to "160m", 3.5e6f to "80m", 7.0e6f to "40m", 10.1e6f to "30m",
            14.0e6f to "20m", 18.068e6f to "17m", 21.0e6f to "15m", 24.89e6f to "12m",
            28.0e6f to "10m", 50.0e6f to "6m",
        )
        val bandPaint = Paint().apply {
            color = android.graphics.Color.rgb(170, 140, 70)
            textSize = 16f
            textAlign = Paint.Align.CENTER
        }
        val bandBgPaint = Paint().apply {
            color = android.graphics.Color.argb(220, 10, 15, 30)
        }
        bands.forEach { (freq, label) ->
            if (freq >= startHz.toFloat() && freq <= endHz.toFloat()) {
                val frac = (freq - startHz.toFloat()) / spanHz.toFloat()
                val x = frac * w
                drawLine(Color(100, 80, 40), Offset(x, plotH - 24f), Offset(x, plotH - 12f), strokeWidth = 1f)
                val textW = bandPaint.measureText(label)
                drawContext.canvas.nativeCanvas.drawRect(
                    x - textW / 2 - 1f, plotH - 40f, x + textW / 2 + 1f, plotH - 26f, bandBgPaint
                )
                drawContext.canvas.nativeCanvas.drawText(label, x, plotH - 27f, bandPaint)
            }
        }

        // ── S-meter overlay (top-right) ─────────────────────────────────────
        val meterText = if (otherTx) {
            "TX: ${smeter / 10}W"
        } else if (transmitting) {
            "TX: ${smeter / 10}W"
        } else if (smeter <= 108) {
            "S${smeter / 12}"
        } else {
            val dbOver = ((smeter - 108f) * 60f / 152f).toInt()
            "S9+${dbOver}dB"
        }
        val meterPaint = Paint().apply {
            color = if (transmitting || otherTx) {
                android.graphics.Color.rgb(255, 80, 80)
            } else {
                android.graphics.Color.rgb(0, 220, 0)
            }
            textSize = 28f
            textAlign = Paint.Align.RIGHT
        }
        val meterBgPaint = Paint().apply {
            color = android.graphics.Color.argb(220, 10, 15, 30)
        }
        val meterW = meterPaint.measureText(meterText)
        drawContext.canvas.nativeCanvas.drawRect(
            w - meterW - 12f, 4f, w - 4f, 32f, meterBgPaint
        )
        drawContext.canvas.nativeCanvas.drawText(meterText, w - 8f, 28f, meterPaint)

        // ── DX Cluster spot markers ────────────────────────────────────────
        for (spot in dxSpots) {
            val spotFrac = (spot.frequencyHz - startHz).toFloat() / spanHz.toFloat()
            if (spotFrac < -0.01f || spotFrac > 1.01f) continue

            val x = spotFrac * w
            val expiry = spot.expirySeconds.coerceAtLeast(1)
            val ageFrac = (spot.ageSeconds.toFloat() / expiry).coerceIn(0f, 1f)

            // Fade out in last 20% of lifetime
            val alpha = if (ageFrac > 0.8f) {
                ((1f - ageFrac) / 0.2f * 255).toInt().coerceIn(0, 255)
            } else 255

            if (alpha <= 0) continue

            // Mode color
            val (cr, cg, cb) = when (spot.mode) {
                "CW" -> Triple(255, 255, 0)       // yellow
                "SSB" -> Triple(0, 255, 0)         // green
                "FT8", "FT4", "DIGI" -> Triple(0, 255, 255) // cyan
                else -> Triple(255, 255, 255)      // white
            }

            // Dashed vertical line (half alpha)
            val lineAlpha = alpha / 2
            val lineColor = Color(cr, cg, cb, lineAlpha)
            val dashLen = 6f
            var dy = 0f
            while (dy < plotH) {
                val y1 = dy
                val y2 = (dy + dashLen).coerceAtMost(plotH)
                drawLine(lineColor, Offset(x, y1), Offset(x, y2), strokeWidth = 1f)
                dy += dashLen * 2
            }

            // Sliding callsign label: top → 3/4 height over lifetime
            val labelY = ageFrac * plotH * 0.75f
            val spotPaint = Paint().apply {
                color = android.graphics.Color.argb(alpha, cr, cg, cb)
                textSize = 22f
                textAlign = Paint.Align.CENTER
                isFakeBoldText = true
            }
            val textW = spotPaint.measureText(spot.callsign)
            val bgAlpha = (alpha * 0.7f).toInt()
            val spotBgPaint = Paint().apply {
                color = android.graphics.Color.argb(bgAlpha, 10, 15, 30)
            }
            drawContext.canvas.nativeCanvas.drawRect(
                x - textW / 2 - 3f, labelY, x + textW / 2 + 3f, labelY + 24f, spotBgPaint
            )
            drawContext.canvas.nativeCanvas.drawText(spot.callsign, x, labelY + 20f, spotPaint)
        }

        // ── Span indicator (top-left) ───────────────────────────────────────
        val spanKhz = visibleSpan / 1000.0
        if (spanKhz < 1536.0) {
            val spanText = if (spanKhz < 100.0) "%.1f kHz".format(spanKhz) else "%.0f kHz".format(spanKhz)
            val spanPaint = Paint().apply {
                color = android.graphics.Color.rgb(220, 220, 80)
                textSize = 18f
                textAlign = Paint.Align.LEFT
            }
            val spanBgPaint = Paint().apply {
                color = android.graphics.Color.argb(220, 10, 15, 30)
            }
            val spanW = spanPaint.measureText(spanText)
            drawContext.canvas.nativeCanvas.drawRect(
                2f, 14f, 2f + spanW + 6f, 34f, spanBgPaint
            )
            drawContext.canvas.nativeCanvas.drawText(spanText, 4f, 30f, spanPaint)
        }
    }
}

/**
 * Spectrum display controls: Ref, Range, Zoom, Pan, Waterfall contrast.
 */
@Composable
fun SpectrumControls(
    refDb: Float,
    rangeDb: Float,
    zoom: Float,
    pan: Float,
    contrast: Float,
    autoRefEnabled: Boolean,
    onRefDbChange: (Float) -> Unit,
    onRangeDbChange: (Float) -> Unit,
    onZoomChange: (Float) -> Unit,
    onPanChange: (Float) -> Unit,
    onContrastChange: (Float) -> Unit,
    onAutoRefToggle: (Boolean) -> Unit,
    modifier: Modifier = Modifier,
) {
    // Row 1: Zoom + Pan
    Row(
        modifier = modifier.fillMaxWidth(),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(4.dp),
    ) {
        Text("Zoom", fontSize = 11.sp)
        Slider(
            value = ln(zoom.coerceIn(1f, 1024f)) / ln(1024f),
            onValueChange = { t ->
                val newZoom = 1024f.pow(t).coerceIn(1f, 1024f)
                onZoomChange(newZoom)
            },
            modifier = Modifier.weight(1f),
        )
        Text("${zoom.roundToInt()}x", fontSize = 11.sp)

        if (zoom > 1.1f) {
            Spacer(Modifier.width(4.dp))
            Text("Pan", fontSize = 11.sp)
            val maxPan = (0.5f - 0.5f / zoom) * 0.05f // Reduced range for mobile
            Slider(
                value = (pan + maxPan) / (2 * maxPan),
                onValueChange = { t ->
                    val newPan = (t * 2 * maxPan) - maxPan
                    onPanChange(newPan.coerceIn(-maxPan, maxPan))
                },
                modifier = Modifier.weight(1f),
            )
        }
    }

    // Row 2: Ref + Auto + Range
    Row(
        modifier = Modifier.fillMaxWidth(),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(4.dp),
    ) {
        Text("Ref", fontSize = 11.sp)
        Slider(
            value = refDb,
            onValueChange = onRefDbChange,
            enabled = !autoRefEnabled,
            valueRange = -90f..0f,
            steps = 17, // 5dB increments
            modifier = Modifier.weight(1f),
        )
        Text("${refDb.roundToInt()}", fontSize = 11.sp)
        Text("Auto", fontSize = 11.sp)
        androidx.compose.material3.Checkbox(
            checked = autoRefEnabled,
            onCheckedChange = onAutoRefToggle,
        )
        Spacer(Modifier.width(4.dp))
        Text("Range", fontSize = 11.sp)
        Slider(
            value = rangeDb,
            onValueChange = onRangeDbChange,
            valueRange = 20f..130f,
            steps = 21, // 5dB increments: (130-20)/5 - 1 = 21
            modifier = Modifier.weight(1f),
        )
        Text("${rangeDb.roundToInt()}", fontSize = 11.sp)
    }

    // Row 3: Waterfall contrast
    Row(
        modifier = Modifier.fillMaxWidth(),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(4.dp),
    ) {
        Text("WF Contrast", fontSize = 11.sp)
        Slider(
            value = ln(contrast.coerceIn(0.1f, 10f)) / ln(10f),
            onValueChange = { t ->
                val newContrast = 10f.pow(t).coerceIn(0.1f, 10f)
                onContrastChange(newContrast)
            },
            valueRange = -1f..1f,  // log10(0.1)=-1, log10(10)=1, center=1.0
            modifier = Modifier.weight(1f),
        )
        Text("%.1f".format(contrast), fontSize = 11.sp)
    }
}

/**
 * Waterfall display: scrolling bitmap colored by signal power.
 * Hybrid rendering: high-res extracted view + full DDC fallback (matches desktop).
 * Newest data at top, scrolls downward.
 */
@Composable
fun WaterfallView(
    fullBins: List<UByte>,
    fullCenterHz: Long,
    fullSpanHz: Long,
    fullSequence: Int,
    viewBins: List<UByte>,
    viewCenterHz: Long,
    viewSpanHz: Long,
    vfoHz: Long,
    zoom: Float,
    pan: Float,
    contrast: Float,
    refDb: Float,
    rangeDb: Float,
    ringBuffer: WaterfallRingBuffer,
    onFrequencyClick: (Long) -> Unit = {},
    modifier: Modifier = Modifier,
) {
    if (fullBins.isEmpty() && viewBins.isEmpty()) return

    val state = ringBuffer

    // Re-render when zoom/pan/contrast/data changes (push happens in MainScreen)
    val bitmap = remember(fullSequence, zoom, pan, contrast, refDb, rangeDb) {
        state.render(360, fullSpanHz.toInt(), vfoHz, zoom, pan, contrast, refDb, rangeDb)
    }

    // Display span accounts for zoom/pan (same as desktop waterfall)
    val displaySpanHz = if (zoom > 0f) fullSpanHz.toDouble() / zoom else fullSpanHz.toDouble()
    val displayCenterHz = vfoHz.toDouble() + pan.toDouble() * fullSpanHz.toDouble()
    val displayStartHz = displayCenterHz - displaySpanHz / 2.0

    Image(
        bitmap = bitmap.asImageBitmap(),
        contentDescription = "Waterfall",
        modifier = modifier
            .fillMaxWidth()
            .height(100.dp)
            .pointerInput(displayStartHz, displaySpanHz) {
                detectTapGestures { offset ->
                    val frac = offset.x / size.width.toFloat()
                    val freq = displayStartHz + frac * displaySpanHz
                    if (freq > 1000.0) {
                        val rounded = (freq.toLong() / 1000) * 1000
                        onFrequencyClick(rounded)
                    }
                }
            },
        contentScale = ContentScale.FillBounds,
    )
}

/**
 * Waterfall ring buffer: stores full DDC + extracted view rows.
 * Renders using hybrid approach (high-res view where available, full DDC elsewhere).
 */
class WaterfallRingBuffer(private val height: Int) {
    private val fullRows = Array(height) { ByteArray(0) }
    private val fullCenters = IntArray(height)
    private val viewRows = Array(height) { ByteArray(0) }
    private val viewCenters = IntArray(height)
    private val viewSpans = IntArray(height)
    private var writeIdx = 0
    private var count = 0
    private var lastSeq = -1
    private var cachedBitmap: Bitmap? = null

    fun push(
        fullBins: List<UByte>, fullCenterHz: Int, fullSpanHz: Int, sequence: Int,
        viewBins: List<UByte>, viewCenterHz: Int, viewSpanHz: Int,
    ) {
        if (fullBins.isEmpty() || fullSpanHz == 0 || sequence == lastSeq) return
        lastSeq = sequence
        fullRows[writeIdx] = ByteArray(fullBins.size) { fullBins[it].toByte() }
        fullCenters[writeIdx] = fullCenterHz
        viewRows[writeIdx] = ByteArray(viewBins.size) { viewBins[it].toByte() }
        viewCenters[writeIdx] = viewCenterHz
        viewSpans[writeIdx] = viewSpanHz
        writeIdx = (writeIdx + 1) % height
        if (count < height) count++
    }

    fun render(outWidth: Int, fullSpanHz: Int, vfoHz: Long, zoom: Float, pan: Float, contrast: Float, refDb: Float = 0f, rangeDb: Float = 120f): Bitmap {
        if (cachedBitmap == null || cachedBitmap!!.width != outWidth || cachedBitmap!!.height != height) {
            cachedBitmap?.recycle()
            cachedBitmap = Bitmap.createBitmap(outWidth, height, Bitmap.Config.ARGB_8888)
        }
        val bitmap = cachedBitmap!!
        val pixels = IntArray(outWidth * height)

        if (count == 0 || fullSpanHz == 0) {
            bitmap.setPixels(pixels, 0, outWidth, 0, 0, outWidth, height)
            return bitmap
        }

        val zoomF = zoom.toDouble().coerceAtLeast(1.0)
        val ddcSpanF = fullSpanHz.toDouble()
        val displayCenterHz = vfoHz.toDouble() + pan.toDouble() * ddcSpanF
        val displaySpanHz = ddcSpanF / zoomF
        val displayStartHz = displayCenterHz - displaySpanHz / 2.0
        val pxHzStep = displaySpanHz / outWidth.toDouble()

        for (row in 0 until height) {
            if (row >= count) continue
            val srcRowIdx = (writeIdx + height - 1 - row) % height

            val rowFull = fullRows[srcRowIdx]
            val rowFullCenter = fullCenters[srcRowIdx].toDouble()
            val rowView = viewRows[srcRowIdx]
            val rowViewCenter = viewCenters[srcRowIdx].toDouble()
            val rowViewSpan = viewSpans[srcRowIdx].toDouble()

            if (rowFull.isEmpty() || rowFullCenter == 0.0) continue

            val fullLen = rowFull.size.toDouble()
            val fullHzPerBin = ddcSpanF / fullLen
            val fullStartHz = rowFullCenter - ddcSpanF / 2.0

            val hasView = rowView.isNotEmpty() && rowViewSpan > 0.0
            val viewLen = if (hasView) rowView.size.toDouble() else 0.0
            val viewHzPerBin = if (hasView) rowViewSpan / viewLen else 1.0
            val viewStartHz = if (hasView) rowViewCenter - rowViewSpan / 2.0 else 0.0
            val viewEndHz = if (hasView) rowViewCenter + rowViewSpan / 2.0 else 0.0

            val dstStart = row * outWidth

            for (px in 0 until outWidth) {
                val pxStartHz = displayStartHz + px * pxHzStep
                val pxEndHz = pxStartHz + pxHzStep
                val pxMidHz = (pxStartHz + pxEndHz) / 2.0

                val maxVal = if (hasView && pxMidHz >= viewStartHz && pxMidHz < viewEndHz) {
                    // High-res extracted view
                    val b0f = (pxStartHz - viewStartHz) / viewHzPerBin
                    val b1f = (pxEndHz - viewStartHz) / viewHzPerBin
                    val b0 = b0f.toInt().coerceAtLeast(0)
                    val b1 = kotlin.math.ceil(b1f).toInt().coerceAtLeast(b0 + 1).coerceAtMost(rowView.size)
                    var mv = 0
                    for (j in b0 until b1) mv = maxOf(mv, rowView[j].toInt() and 0xFF)
                    mv
                } else {
                    // Full DDC fallback
                    val b0f = (pxStartHz - fullStartHz) / fullHzPerBin
                    val b1f = (pxEndHz - fullStartHz) / fullHzPerBin
                    val b0 = b0f.toInt()
                    val b1 = kotlin.math.ceil(b1f).toInt().coerceAtLeast(b0 + 1)
                    if (b1 <= 0 || b0 >= rowFull.size) continue
                    val b0c = b0.coerceAtLeast(0)
                    val b1c = b1.coerceAtMost(rowFull.size)
                    var mv = 0
                    for (j in b0c until b1c) mv = maxOf(mv, rowFull[j].toInt() and 0xFF)
                    mv
                }

                // Same normalization as spectrum: raw→dB→frac→color
                val db = SERVER_FLOOR_DB + (maxVal.toFloat() / 255f) * SERVER_RANGE_DB
                val frac = ((refDb - db) / rangeDb).coerceIn(0f, 1f)
                val level = (1f - frac).pow(1f / contrast).coerceIn(0f, 1f)
                val floor = 0.25f
                val mapped = floor + level * (1f - floor)
                pixels[dstStart + px] = waterfallColor((mapped * 255).toInt())
            }
        }

        bitmap.setPixels(pixels, 0, outWidth, 0, 0, outWidth, height)
        return bitmap
    }
}

/// Map 8-bit power value to ARGB color (waterfall colormap)
/// Black → Blue → Cyan → Yellow → Red → White
private fun waterfallColor(value: Int): Int {
    val v = value / 255f
    val (r, g, b) = when {
        v < 0.2f -> Triple(0f, 0f, v / 0.2f)
        v < 0.4f -> Triple(0f, (v - 0.2f) / 0.2f, 1f)
        v < 0.6f -> {
            val t = (v - 0.4f) / 0.2f
            Triple(t, 1f, 1f - t)
        }
        v < 0.8f -> Triple(1f, 1f - (v - 0.6f) / 0.2f, 0f)
        else -> {
            val t = (v - 0.8f) / 0.2f
            Triple(1f, t, t)
        }
    }
    return android.graphics.Color.rgb((r * 255).toInt(), (g * 255).toInt(), (b * 255).toInt())
}

private fun Float.pow(exponent: Float): Float = this.toDouble().pow(exponent.toDouble()).toFloat()

/// Map normalized level (0=weak, 1=strong) to Compose Color.
/// Same colormap as desktop: waterfall colors with floor=0.25
private fun spectrumLevelColor(level: Float): Color {
    val floor = 0.25f
    val mapped = floor + level.coerceIn(0f, 1f) * (1f - floor)
    val v = mapped
    val (r, g, b) = when {
        v < 0.2f -> Triple(0f, 0f, v / 0.2f)
        v < 0.4f -> Triple(0f, (v - 0.2f) / 0.2f, 1f)
        v < 0.6f -> {
            val t = (v - 0.4f) / 0.2f
            Triple(t, 1f, 1f - t)
        }
        v < 0.8f -> Triple(1f, 1f - (v - 0.6f) / 0.2f, 0f)
        else -> {
            val t = (v - 0.8f) / 0.2f
            Triple(1f, t, t)
        }
    }
    return Color(r, g, b)
}
