package com.sdrremote.ui.screens

import android.content.Context
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AlertDialog
import androidx.compose.ui.text.style.TextAlign
import kotlinx.coroutines.flow.MutableStateFlow
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Settings
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Slider
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableFloatStateOf
import androidx.compose.runtime.mutableLongStateOf
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
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.lifecycle.viewmodel.compose.viewModel
import com.sdrremote.ui.components.AudioStats
import com.sdrremote.ui.components.ConnectionPanel
import com.sdrremote.ui.components.FilterBandwidthControl
import com.sdrremote.ui.components.FrequencyDisplay
import com.sdrremote.ui.components.PttButton
import com.sdrremote.ui.components.RadioControls
import com.sdrremote.ui.components.SettingsDialog
import com.sdrremote.ui.components.SpectrumControls
import com.sdrremote.ui.components.SpectrumPlot
import com.sdrremote.ui.components.VolumeControls
import com.sdrremote.ui.components.WaterfallView
import com.sdrremote.ui.components.parseTxProfiles
import com.sdrremote.viewmodel.SdrViewModel
import uniffi.sdr_remote.version
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.SegmentedButton
import androidx.compose.material3.SegmentedButtonDefaults
import androidx.compose.material3.SingleChoiceSegmentedButtonRow
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.runtime.mutableIntStateOf
import kotlin.math.abs
import kotlin.math.ln
import kotlin.math.exp

// Logarithmic volume mapping (0.001..3.0, allows boost up to 300%)
private const val LOG_VOL_MIN = 0.001f
private const val LOG_VOL_MAX = 3.0f
private fun volToSlider(v: Float): Float {
    val clamped = v.coerceIn(LOG_VOL_MIN, LOG_VOL_MAX)
    return (ln(clamped) - ln(LOG_VOL_MIN)) / (ln(LOG_VOL_MAX) - ln(LOG_VOL_MIN))
}
private fun sliderToVol(s: Float): Float {
    return exp(ln(LOG_VOL_MIN) + s * (ln(LOG_VOL_MAX) - ln(LOG_VOL_MIN)))
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun MainScreen(viewModel: SdrViewModel = viewModel()) {
    val state by viewModel.state.collectAsStateWithLifecycle()
    val yaesuActive by viewModel.yaesuMode.collectAsStateWithLifecycle()
    val context = LocalContext.current
    val prefs = remember { context.getSharedPreferences("thetislink", Context.MODE_PRIVATE) }

    // Volume-up button as PTT (BT remote)
    val activity = context as? com.sdrremote.MainActivity
    val volumePttEnabled = prefs.getBoolean("volume_ptt", false)
    LaunchedEffect(volumePttEnabled) { activity?.volumePttEnabled = volumePttEnabled }
    val volumeUpHeld by (activity?.volumeUpHeld ?: MutableStateFlow(false)).collectAsStateWithLifecycle()
    val lastKeyEvent by (activity?.lastKeyEvent ?: MutableStateFlow("")).collectAsStateWithLifecycle()

    // Volume state — loaded from SharedPreferences, persists across restarts
    val rxVolumeState = rememberSaveable { mutableFloatStateOf(prefs.getFloat("rx_volume", 0.5f)) }
    val localVolumeState = rememberSaveable { mutableFloatStateOf(prefs.getFloat("local_volume", 1f)) }
    val txGainState = rememberSaveable { mutableFloatStateOf(prefs.getFloat("tx_gain", 0.5f)) }

    // AGC state — loaded from SharedPreferences, persists across restarts
    var agcEnabled by rememberSaveable { mutableStateOf(prefs.getBoolean("agc_enabled", false)) }

    // TX profiles from SharedPreferences
    var txProfilesStr by rememberSaveable {
        mutableStateOf(prefs.getString("tx_profiles", "21:Normaal,25:Remote") ?: "")
    }
    val txProfiles = remember(txProfilesStr) { parseTxProfiles(txProfilesStr) }

    // Sync RX volume slider with Thetis value (server broadcasts ZZLA)
    // Skip in Yaesu mode to prevent Thetis audio from re-activating
    val serverRxAfGain = state.rxAfGain
    LaunchedEffect(serverRxAfGain) {
        if (serverRxAfGain > 0 && !yaesuActive) {
            val serverVolume = serverRxAfGain / 100f
            rxVolumeState.floatValue = serverVolume
            viewModel.setRxVolume(serverVolume)
        }
    }

    // Spectrum state
    var spectrumEnabled by rememberSaveable { mutableStateOf(prefs.getBoolean("spectrum_enabled", false)) }
    val spectrumZoomState = rememberSaveable { mutableFloatStateOf(prefs.getFloat("spectrum_zoom", 20f)) }
    val spectrumPanState = rememberSaveable { mutableFloatStateOf(prefs.getFloat("spectrum_pan", 0f)) }
    val spectrumRefState = rememberSaveable { mutableFloatStateOf(prefs.getFloat("spectrum_ref", -30f)) }
    val spectrumRangeState = rememberSaveable { mutableFloatStateOf(prefs.getFloat("spectrum_range", 80f)) }
    val waterfallContrastState = rememberSaveable { mutableFloatStateOf(prefs.getFloat("waterfall_contrast", 1f)) }
    val waterfallRingBuffer = remember { com.sdrremote.ui.components.WaterfallRingBuffer(100) }

    // Push waterfall rows even when WaterfallView is off-screen
    LaunchedEffect(state.fullSpectrumSequence) {
        if (state.fullSpectrumBins.isNotEmpty() || state.spectrumBins.isNotEmpty()) {
            waterfallRingBuffer.push(
                state.fullSpectrumBins, state.fullSpectrumCenterHz.toInt(),
                state.fullSpectrumSpanHz.toInt(), state.fullSpectrumSequence,
                state.spectrumBins, state.spectrumCenterHz.toInt(),
                state.spectrumSpanHz.toInt(),
            )
        }
    }

    // Auto ref level
    var autoRefEnabled by rememberSaveable { mutableStateOf(prefs.getBoolean("auto_ref_enabled", false)) }
    var autoRefValue by remember { mutableFloatStateOf(spectrumRefState.floatValue) }
    var autoRefFrames by remember { mutableIntStateOf(0) }
    var autoRefInitialized by remember { mutableStateOf(false) }

    // TX spectrum override — save/restore ref + range + auto on PTT transitions
    var txSavedRef by remember { mutableStateOf<Float?>(null) }
    var txSavedRange by remember { mutableStateOf<Float?>(null) }
    var txSavedAutoRef by remember { mutableStateOf<Boolean?>(null) }

    LaunchedEffect(state.transmitting) {
        if (state.transmitting) {
            // Entering TX: save current ref+range+auto, set TX defaults
            txSavedRef = spectrumRefState.floatValue
            txSavedRange = spectrumRangeState.floatValue
            txSavedAutoRef = autoRefEnabled
            autoRefEnabled = false
            spectrumRefState.floatValue = -30f
            spectrumRangeState.floatValue = 120f
        } else {
            // Leaving TX: restore ref+range immediately, auto_ref after 200ms
            txSavedRef?.let { spectrumRefState.floatValue = it; txSavedRef = null }
            txSavedRange?.let { spectrumRangeState.floatValue = it; txSavedRange = null }
            val savedAuto = txSavedAutoRef
            txSavedAutoRef = null
            if (savedAuto != null) {
                kotlinx.coroutines.delay(500)
                autoRefEnabled = savedAuto
                if (savedAuto) {
                    autoRefFrames = 0
                    autoRefInitialized = false
                }
            }
        }
    }

    // Per-band WF contrast tracking
    var currentBand by remember { mutableStateOf<String?>(null) }

    // Auto ref level calculation — runs on each new spectrum frame
    val spectrumSeq = state.spectrumSequence
    LaunchedEffect(spectrumSeq) {
        if (!autoRefEnabled) return@LaunchedEffect
        val bins = state.spectrumBins
        if (bins.isEmpty()) return@LaunchedEffect
        val spanHz = state.spectrumSpanHz
        if (spanHz == 0L) return@LaunchedEffect

        val numBins = bins.size.toFloat()
        val hzPerBin = spanHz.toDouble() / numBins.toDouble()
        val startHz = state.spectrumCenterHz.toDouble() - spanHz.toDouble() / 2.0
        val filterLoHz = state.frequencyHz.toDouble() + state.filterLowHz.toDouble()
        val filterHiHz = state.frequencyHz.toDouble() + state.filterHighHz.toDouble()
        val filterLoBin = ((filterLoHz - startHz) / hzPerBin).toInt()
        val filterHiBin = ((filterHiHz - startHz) / hzPerBin).toInt()

        var sumDb = 0.0
        var count = 0
        for (i in bins.indices) {
            if (i in filterLoBin..filterHiBin) continue
            val db = -150.0 + (bins[i].toInt().toDouble() / 255.0) * 120.0
            sumDb += db
            count++
        }
        if (count > 0) {
            val avgDb = sumDb / count
            val target = (avgDb + spectrumRangeState.floatValue - 5.0).toFloat()
            if (!autoRefInitialized) {
                autoRefValue = target
                autoRefInitialized = true
            } else {
                val alpha = if (autoRefFrames < 45) 0.10f else 0.002f
                autoRefValue = alpha * target + (1f - alpha) * autoRefValue
            }
            spectrumRefState.floatValue = autoRefValue
            autoRefFrames++
        }
    }

    // Per-band WF contrast tracking — runs on frequency change
    val freqHz = state.frequencyHz
    LaunchedEffect(freqHz) {
        val newBand = freqToBand(freqHz)
        if (newBand != currentBand) {
            // Save current contrast for old band
            currentBand?.let { old ->
                prefs.edit().putFloat("wf_contrast_$old", waterfallContrastState.floatValue).apply()
            }
            // Load contrast for new band
            newBand?.let { nb ->
                val savedContrast = prefs.getFloat("wf_contrast_$nb", 1f)
                waterfallContrastState.floatValue = savedContrast
            }
            // Reset auto-ref to fast convergence
            if (autoRefEnabled) {
                autoRefFrames = 0
                autoRefInitialized = false
            }
            currentBand = newBand
        }
    }

    // Pending frequency: prevents VFO marker bounce during frequency changes.
    // While pending, VFO marker is pinned at spectrum center.
    var pendingFreq by remember { mutableLongStateOf(0L) }

    // Clear pending when spectrum center or VFO catches up to the new frequency
    // (During CTUN, spectrum center stays fixed but VFO changes — check both)
    val specCenterHz = state.spectrumCenterHz
    val currentVfoHz = state.frequencyHz
    LaunchedEffect(specCenterHz, currentVfoHz, pendingFreq) {
        if (pendingFreq > 0) {
            val deltaCenter = if (specCenterHz > 0) abs(specCenterHz - pendingFreq) else Long.MAX_VALUE
            val deltaVfo = if (currentVfoHz > 0) abs(currentVfoHz - pendingFreq) else Long.MAX_VALUE
            if (deltaCenter < 500 || deltaVfo < 500) {
                pendingFreq = 0L
            }
        }
    }

    // Send volume + spectrum state to engine whenever connection becomes active.
    val connected = state.connected
    LaunchedEffect(connected) {
        if (connected) {
            viewModel.setLocalVolume(localVolumeState.floatValue)
            viewModel.setTxGain(txGainState.floatValue)
            if (spectrumEnabled) {
                viewModel.enableSpectrum(true)
                viewModel.setSpectrumFps(5)
                viewModel.setSpectrumZoom(spectrumZoomState.floatValue)
                viewModel.setSpectrumPan(spectrumPanState.floatValue)
            }
        }
    }

    // Track DDC span for dynamic zoom calculation
    var lastFullSpanHz by remember { mutableLongStateOf(0L) }

    // Reset zoom when Thetis comes online (power OFF→ON)
    val powerOn = state.powerOn
    LaunchedEffect(powerOn) {
        if (powerOn) {
            lastFullSpanHz = 0L // Reset so first spectrum packet triggers zoom
            spectrumPanState.floatValue = 0f
            prefs.edit().putFloat("spectrum_pan", 0f).apply()
            viewModel.setSpectrumPan(0f)
        }
    }

    // Dynamic zoom: when DDC span becomes known, scale zoom proportionally
    val fullSpanHz = state.fullSpectrumSpanHz
    LaunchedEffect(fullSpanHz) {
        if (fullSpanHz > 0 && lastFullSpanHz == 0L) {
            val defaultZoom = (fullSpanHz.toFloat() / 48000f).coerceIn(1f, 1024f)
            spectrumZoomState.floatValue = defaultZoom
            spectrumPanState.floatValue = 0f
            prefs.edit().putFloat("spectrum_zoom", defaultZoom).putFloat("spectrum_pan", 0f).apply()
            viewModel.setSpectrumZoom(defaultZoom)
            viewModel.setSpectrumPan(0f)
        }
        lastFullSpanHz = fullSpanHz
    }

    var showSettings by remember { mutableStateOf(false) }
    var showAbout by remember { mutableStateOf(false) }

    // MIDI controller
    val midi = remember { com.sdrremote.service.MidiController(context) }
    var midiPtt by remember { mutableStateOf(false) }
    var midiPorts by remember { mutableStateOf(midi.listDevices()) }
    var showMidiSettings by remember { mutableStateOf(false) }

    // Load saved MIDI mappings and auto-connect
    LaunchedEffect(Unit) {
        midi.loadMappings()
        midi.autoConnect()
    }

    // Process MIDI events
    val midiEvent by midi.event.collectAsStateWithLifecycle()
    LaunchedEffect(midiEvent) {
        val ev = midiEvent ?: return@LaunchedEffect
        when (ev) {
            is com.sdrremote.service.MidiEvent.ButtonEvent -> {
                val pressed = ev.velocity > 0
                when (ev.action) {
                    com.sdrremote.service.MidiAction.Ptt -> if (pressed) {
                        midiPtt = !midiPtt
                        viewModel.setPtt(midiPtt)
                        midi.sendLed(com.sdrremote.service.MidiAction.Ptt, midiPtt)
                    }
                    com.sdrremote.service.MidiAction.NrToggle -> if (pressed) {
                        val newVal = if (state.nrLevel >= 4) 0 else state.nrLevel + 1
                        viewModel.setControl(0x04, newVal)
                    }
                    com.sdrremote.service.MidiAction.AnfToggle -> if (pressed) {
                        viewModel.setControl(0x05, if (state.anfOn) 0 else 1)
                    }
                    com.sdrremote.service.MidiAction.PowerToggle -> if (pressed) {
                        viewModel.setControl(0x02, if (state.powerOn) 0 else 1)
                    }
                    com.sdrremote.service.MidiAction.MicAgcToggle -> if (pressed) {
                        viewModel.setAgcEnabled(!state.agcEnabled)
                    }
                    else -> {}
                }
            }
            is com.sdrremote.service.MidiEvent.SliderEvent -> {
                val frac = ev.value.toFloat() / 127f
                when (ev.action) {
                    com.sdrremote.service.MidiAction.MasterVolume -> {
                        rxVolumeState.floatValue = frac
                        viewModel.setRxVolume(frac)
                    }
                    com.sdrremote.service.MidiAction.TxGain -> {
                        txGainState.floatValue = frac * 3f
                        viewModel.setTxGain(frac * 3f)
                    }
                    com.sdrremote.service.MidiAction.Drive -> {
                        viewModel.setControl(0x06, (frac * 100).toInt())
                    }
                    else -> {}
                }
            }
            is com.sdrremote.service.MidiEvent.LearnEvent -> { /* handled in settings dialog */ }
        }
    }

    // Turn off MIDI PTT LED when disconnecting
    LaunchedEffect(connected) {
        if (!connected && midiPtt) {
            midiPtt = false
            midi.sendLed(com.sdrremote.service.MidiAction.Ptt, false)
        }
    }

    // Stable callbacks — same lambda reference across recompositions.
    val onRxVolumeChange: (Float) -> Unit = remember {
        { v ->
            rxVolumeState.floatValue = v
            viewModel.setRxVolume(v)
            prefs.edit().putFloat("rx_volume", v).apply()
        }
    }
    val onLocalVolumeChange: (Float) -> Unit = remember {
        { v ->
            localVolumeState.floatValue = v
            if (viewModel.yaesuMode.value) {
                viewModel.yaesuVolume(v)
            } else {
                viewModel.setLocalVolume(v)
            }
            prefs.edit().putFloat("local_volume", v).apply()
        }
    }
    val onTxGainChange: (Float) -> Unit = remember {
        { v ->
            txGainState.floatValue = v
            viewModel.setTxGain(v)
            prefs.edit().putFloat("tx_gain", v).apply()
        }
    }
    val onControl: (Int, Int) -> Unit = remember {
        { id, value -> viewModel.setControl(id, value) }
    }

    // Display VFO: pin at spectrum center while frequency change is pending
    val displayVfo = if (pendingFreq > 0L && state.spectrumCenterHz > 0) {
        state.spectrumCenterHz
    } else {
        state.frequencyHz
    }

    if (showSettings) {
        val routing = viewModel.audioRouting
        SettingsDialog(
            connected = state.connected,
            headsetActive = routing.headsetActive,
            headsetName = routing.headsetName,
            audioMode = routing.forceMode.ordinal,
            onAudioModeChange = { viewModel.setAudioMode(com.sdrremote.service.AudioRouting.Mode.entries[it]) },
            txProfileNames = state.txProfileNames,
            onTxProfileChange = { viewModel.setControl(0x03, it) },
            onReboot = { viewModel.serverReboot() },
            onShutdown = { viewModel.serverShutdown() },
            onDismiss = { showSettings = false },
        )
    }

    if (showAbout) {
        val versionName = try { context.packageManager.getPackageInfo(context.packageName, 0).versionName } catch (_: Exception) { "?" }
        AlertDialog(
            onDismissRequest = { showAbout = false },
            title = { Text("About ThetisLink") },
            text = {
                Column(modifier = Modifier.fillMaxWidth().verticalScroll(rememberScrollState())) {
                    Text("ThetisLink", fontSize = 20.sp, fontWeight = FontWeight.Bold, modifier = Modifier.fillMaxWidth(), textAlign = TextAlign.Center)
                    Text("v$versionName", fontSize = 14.sp, modifier = Modifier.fillMaxWidth(), textAlign = TextAlign.Center)
                    Spacer(Modifier.height(4.dp))
                    Text("Remote control for\nThetis SDR + Yaesu FT-991A", fontSize = 13.sp, modifier = Modifier.fillMaxWidth(), textAlign = TextAlign.Center)
                    Spacer(Modifier.height(10.dp))
                    HorizontalDivider()
                    Spacer(Modifier.height(6.dp))
                    Text("Author", fontWeight = FontWeight.Bold, fontSize = 13.sp)
                    Text("Chiron van der Burgt — PA3GHM", fontSize = 12.sp)
                    Spacer(Modifier.height(6.dp))
                    Text("Special Thanks", fontWeight = FontWeight.Bold, fontSize = 13.sp)
                    Text("Richie (ramdor) — Thetis SDR, TCI extensions", fontSize = 12.sp)
                    Spacer(Modifier.height(6.dp))
                    Text("Protocols", fontWeight = FontWeight.Bold, fontSize = 13.sp)
                    Text("TCI — Expert Electronics / Thetis", fontSize = 11.sp)
                    Text("DX Spider — DX cluster", fontSize = 11.sp)
                    Text("HPSDR / OpenHPSDR Protocol 2", fontSize = 11.sp)
                    Text("WebSDR (PA3FWM) / KiwiSDR — CatSync", fontSize = 11.sp)
                    Spacer(Modifier.height(6.dp))
                    Text("Hardware", fontWeight = FontWeight.Bold, fontSize = 13.sp)
                    for ((dev, iface) in listOf(
                        "ANAN 7000DLE" to "TCI",
                        "Yaesu FT-991A" to "Serial + USB Audio",
                        "RF2K-S PA" to "HTTP",
                        "SPE Expert 1.3K-FA" to "Serial",
                        "JC-4s Tuner" to "Serial",
                        "UltraBeam RCU-06" to "Serial",
                        "Amplitec 6/2" to "Serial",
                        "EA7HG Rotor" to "UDP",
                    )) {
                        Row(modifier = Modifier.fillMaxWidth()) {
                            Text(dev, fontSize = 11.sp, modifier = Modifier.weight(0.55f))
                            Text(iface, fontSize = 11.sp, color = Color.Gray, modifier = Modifier.weight(0.45f))
                        }
                    }
                    Spacer(Modifier.height(6.dp))
                    Text("Libraries", fontWeight = FontWeight.Bold, fontSize = 13.sp)
                    for ((lib, purpose) in listOf(
                        "tokio" to "Async runtime",
                        "egui" to "Desktop GUI",
                        "cpal / Oboe" to "Audio I/O",
                        "audiopus" to "Opus codec",
                        "rubato" to "Resampling",
                        "rustfft" to "FFT spectrum",
                        "UniFFI" to "Rust-Kotlin bridge",
                        "Jetpack Compose" to "Android UI",
                    )) {
                        Row(modifier = Modifier.fillMaxWidth()) {
                            Text(lib, fontSize = 11.sp, modifier = Modifier.weight(0.45f))
                            Text(purpose, fontSize = 11.sp, color = Color.Gray, modifier = Modifier.weight(0.55f))
                        }
                    }
                    Spacer(Modifier.height(6.dp))
                    Text("License", fontWeight = FontWeight.Bold, fontSize = 13.sp)
                    Text("GPL-2.0-or-later (see LICENSE)", fontSize = 11.sp)
                    Text("Copyright © 2025-2026 Chiron van der Burgt", fontSize = 11.sp)
                    Text("Source: github.com/cjenschede/ThetisLink", fontSize = 11.sp)
                    Text("Based on the Thetis SDR lineage — see ATTRIBUTION.md", fontSize = 11.sp)
                }
            },
            confirmButton = {
                TextButton(onClick = { showAbout = false }) { Text("Close") }
            }
        )
    }

    if (showMidiSettings) {
        com.sdrremote.ui.components.MidiSettingsDialog(
            midi = midi,
            onDismiss = { showMidiSettings = false },
        )
    }

    // Tuner: track the frequency at which the last successful tune was done
    var tunerTuneFreq by remember { mutableLongStateOf(0L) }
    var lastTunerState by remember { mutableStateOf(0) }
    // Record tune frequency on real tune (TUNING → DONE_OK/DONE_ASSUMED) or first
    // done-state after connect (tunerTuneFreq still 0). Ignores the fake
    // IDLE → done-state transitions from the server's stale override.
    val tunerState = state.tunerState
    LaunchedEffect(tunerState) {
        val isDone = tunerState == 2 || tunerState == 5
        if (isDone && (lastTunerState == 1 || tunerTuneFreq == 0L)) {
            tunerTuneFreq = state.frequencyHz
        }
        lastTunerState = tunerState
    }

    var showDevices by rememberSaveable { mutableStateOf(false) }
    var deviceSubTab by rememberSaveable { mutableIntStateOf(0) }

    // Auto-switch to Devices when Yaesu is activated
    LaunchedEffect(yaesuActive) {
        if (yaesuActive) showDevices = true
    }

    Surface(
        modifier = Modifier.fillMaxSize(),
        color = MaterialTheme.colorScheme.background,
    ) {
        Column(modifier = Modifier.fillMaxSize()) {
            // Scrollable content
            LazyColumn(
                modifier = Modifier
                    .weight(1f)
                    .fillMaxWidth()
                    .padding(horizontal = 12.dp)
                    .padding(top = 12.dp),
            ) {
                item {
                    Row(
                        modifier = Modifier.fillMaxWidth(),
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        if (state.rf2kActive && state.rf2kErrorState != 0) {
                            Button(
                                onClick = { viewModel.rf2kErrorReset() },
                                colors = ButtonDefaults.buttonColors(containerColor = Color(0xFFC62828)),
                                contentPadding = PaddingValues(horizontal = 12.dp, vertical = 4.dp),
                                modifier = Modifier.padding(bottom = 4.dp),
                            ) {
                                Text(
                                    "RF2K-S Reset",
                                    color = Color.White,
                                    fontWeight = FontWeight.Bold,
                                    fontSize = 14.sp,
                                )
                            }
                        } else {
                            Text(
                                text = "ThetisLink v${version()}",
                                fontSize = 16.sp,
                                color = MaterialTheme.colorScheme.onSurface.copy(alpha = 0.6f),
                                modifier = Modifier.padding(bottom = 4.dp),
                            )
                        }
                        Spacer(Modifier.weight(1f))
                        SingleChoiceSegmentedButtonRow {
                            SegmentedButton(
                                selected = !showDevices,
                                onClick = { showDevices = false },
                                shape = SegmentedButtonDefaults.itemShape(index = 0, count = 2),
                            ) { Text("Radio", fontSize = 12.sp) }
                            SegmentedButton(
                                selected = showDevices,
                                onClick = { showDevices = true },
                                shape = SegmentedButtonDefaults.itemShape(index = 1, count = 2),
                            ) { Text("Devices", fontSize = 12.sp) }
                        }
                    }
                }

                if (showDevices) {
                    item {
                        ExternalDevicesScreen(
                            state = state,
                            onSetSwitchA = { viewModel.setAmplitecSwitchA(it) },
                            onSetSwitchB = { viewModel.setAmplitecSwitchB(it) },
                            onSpeOperate = { viewModel.speOperate() },
                            onSpeTune = { viewModel.speTune() },
                            onSpeAntenna = { viewModel.speAntenna() },
                            onSpeInput = { viewModel.speInput() },
                            onSpePower = { viewModel.spePower() },
                            onSpeOff = { viewModel.speOff() },
                            onSpePowerOn = { viewModel.spePowerOn() },
                            onSpeDriveDown = { viewModel.speDriveDown() },
                            onSpeDriveUp = { viewModel.speDriveUp() },
                            onTunerTune = { viewModel.tunerTune() },
                            onTunerAbort = { viewModel.tunerAbort() },
                            onRf2kOperate = { viewModel.rf2kOperate(it) },
                            onRf2kTune = { viewModel.rf2kTune() },
                            onRf2kAnt1 = { viewModel.rf2kAnt1() },
                            onRf2kAnt2 = { viewModel.rf2kAnt2() },
                            onRf2kAnt3 = { viewModel.rf2kAnt3() },
                            onRf2kAnt4 = { viewModel.rf2kAnt4() },
                            onRf2kAntExt = { viewModel.rf2kAntExt() },
                            onRf2kErrorReset = { viewModel.rf2kErrorReset() },
                            onRf2kClose = { viewModel.rf2kClose() },
                            onRf2kDriveUp = { viewModel.rf2kDriveUp() },
                            onRf2kDriveDown = { viewModel.rf2kDriveDown() },
                            onRf2kTunerMode = { viewModel.rf2kTunerMode(it) },
                            onRf2kTunerBypass = { viewModel.rf2kTunerBypass(it) },
                            onRf2kTunerReset = { viewModel.rf2kTunerReset() },
                            onRf2kTunerStore = { viewModel.rf2kTunerStore() },
                            onRf2kTunerLUp = { viewModel.rf2kTunerLUp() },
                            onRf2kTunerLDown = { viewModel.rf2kTunerLDown() },
                            onRf2kTunerCUp = { viewModel.rf2kTunerCUp() },
                            onRf2kTunerCDown = { viewModel.rf2kTunerCDown() },
                            onRf2kTunerK = { viewModel.rf2kTunerK() },
                            onUbRetract = { viewModel.ubRetract() },
                            onUbSetFrequency = { khz, dir -> viewModel.ubSetFrequency(khz, dir) },
                            onUbReadElements = { viewModel.ubReadElements() },
                            onRotorGoTo = { angleX10 -> viewModel.rotorGoTo(angleX10) },
                            onRotorStop = { viewModel.rotorStop() },
                            onRotorCw = { viewModel.rotorCw() },
                            onRotorCcw = { viewModel.rotorCcw() },
                            onYaesuEnable = { viewModel.yaesuEnable(it) },
                            onYaesuPtt = { viewModel.yaesuPtt(it) },
                            onYaesuVolume = { viewModel.yaesuVolume(it) },
                            onYaesuSelectVfo = { viewModel.yaesuSelectVfo(it) },
                            onYaesuMode = { viewModel.yaesuMode(it) },
                            onYaesuButton = { viewModel.yaesuButton(it) },
                            onYaesuRecallMemory = { viewModel.yaesuRecallMemory(it) },
                            onYaesuControl = { id, value -> viewModel.setControl(id, value) },
                            onYaesuFreq = { viewModel.yaesuFreq(it) },
                            onYaesuEqBand = { band, gain -> viewModel.yaesuEqBand(band, gain) },
                            onYaesuEqEnabled = { viewModel.yaesuEqEnabled(it) },
                            onYaesuTxGain = { viewModel.yaesuTxGain(it) },
                            yaesuActive = yaesuActive,
                            selectedTab = deviceSubTab,
                            onTabChange = { deviceSubTab = it },
                        )
                    }
                } else {

                if (state.initError != null) {
                    item {
                        Text(
                            text = "Native library error: ${state.initError}",
                            color = Color.Red,
                            fontSize = 14.sp,
                            modifier = Modifier.fillMaxWidth().padding(bottom = 8.dp),
                        )
                    }
                }

                item {
                    // Determine active PA power for status bar
                    val paForwardW = when {
                        state.rf2kConnected && state.rf2kOperate -> state.rf2kForwardW
                        state.speConnected -> state.spePowerW
                        else -> 0
                    }
                    val paMaxW = when {
                        state.rf2kConnected && state.rf2kOperate -> state.rf2kMaxPowerW.coerceAtLeast(100)
                        state.speConnected -> 3000
                        else -> 0
                    }
                    val paName = when {
                        state.rf2kConnected && state.rf2kOperate -> "RF2K-S"
                        state.speConnected -> "SPE"
                        else -> ""
                    }

                    ConnectionPanel(
                        connected = state.connected,
                        audioError = state.audioError,
                        transmitting = state.transmitting,
                        paForwardW = paForwardW,
                        paMaxW = paMaxW,
                        paName = paName,
                        onConnect = { addr, password ->
                            spectrumZoomState.floatValue = 20f
                            spectrumPanState.floatValue = 0f
                            prefs.edit().putFloat("spectrum_zoom", 20f).putFloat("spectrum_pan", 0f).apply()
                            viewModel.connect(addr, password)
                            viewModel.setRxVolume(rxVolumeState.floatValue)
                            viewModel.setLocalVolume(localVolumeState.floatValue)
                            viewModel.setTxGain(txGainState.floatValue)
                            viewModel.setAgcEnabled(agcEnabled)
                        },
                        onDisconnect = { viewModel.disconnect() },
                        totpRequired = state.totpRequired,
                        onSendTotp = { code -> viewModel.sendTotpCode(code) },
                    )
                }

                // Debug: show last key event (for BT remote discovery)
                if (lastKeyEvent.isNotEmpty()) {
                    item {
                        Text(
                            text = "Key: $lastKeyEvent",
                            fontSize = 11.sp,
                            color = Color(0xFF888888),
                            modifier = Modifier.padding(start = 4.dp),
                        )
                    }
                }

                item {
                    Spacer(Modifier.height(8.dp))
                    HorizontalDivider()
                    Spacer(Modifier.height(8.dp))
                }

                if (yaesuActive) {
                    item {
                        Text(
                            "Yaesu active — go to Devices",
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                            modifier = Modifier.padding(16.dp),
                        )
                    }
                }

                if (!yaesuActive) { item {
                    FrequencyDisplay(
                        frequencyHz = state.frequencyHz,
                        smeter = state.smeter,
                        mode = state.mode,
                        transmitting = state.transmitting,
                        otherTx = state.otherTx,
                        onFrequencyChange = { hz ->
                            pendingFreq = hz
                            viewModel.setFrequency(hz)
                        },
                        onModeChange = { viewModel.setMode(it) },
                    )
                    Spacer(Modifier.height(4.dp))
                    FilterBandwidthControl(
                        filterLowHz = state.filterLowHz,
                        filterHighHz = state.filterHighHz,
                        mode = state.mode,
                        onFilterChange = { low, high ->
                            viewModel.setControl(0x0B, low.toShort().toInt() and 0xFFFF)
                            viewModel.setControl(0x0C, high.toShort().toInt() and 0xFFFF)
                        },
                    )
                }

                // Spectrum toggle + display
                item {
                    Spacer(Modifier.height(8.dp))
                    Row {
                        Button(
                            onClick = {
                                spectrumEnabled = !spectrumEnabled
                                viewModel.enableSpectrum(spectrumEnabled)
                                if (spectrumEnabled) {
                                    viewModel.setSpectrumFps(5)
                                    viewModel.setSpectrumMaxBins(2048)
                                }
                                prefs.edit().putBoolean("spectrum_enabled", spectrumEnabled).apply()
                            },
                            colors = if (spectrumEnabled) {
                                ButtonDefaults.buttonColors(containerColor = MaterialTheme.colorScheme.primary)
                            } else {
                                ButtonDefaults.buttonColors(containerColor = MaterialTheme.colorScheme.surfaceVariant)
                            },
                        ) {
                            Text("Spectrum")
                        }
                    }
                }

                if (spectrumEnabled && state.spectrumBins.isNotEmpty()) {
                    item {
                        SpectrumPlot(
                            bins = state.spectrumBins,
                            centerFreqHz = state.spectrumCenterHz,
                            spanHz = state.spectrumSpanHz,
                            displayCenterHz = if (state.fullSpectrumSpanHz > 0)
                                state.frequencyHz + (spectrumPanState.floatValue * state.fullSpectrumSpanHz).toLong()
                            else state.frequencyHz,  // display center = VFO + pan (matches waterfall)
                            vfoHz = displayVfo,                    // VFO marker (pinned during pending)
                            filterLowHz = state.filterLowHz,
                            filterHighHz = state.filterHighHz,
                            refDb = spectrumRefState.floatValue,
                            rangeDb = spectrumRangeState.floatValue,
                            smeter = state.smeter,
                            transmitting = state.transmitting,
                            otherTx = state.otherTx,
                            dxSpots = state.dxSpots,
                            onFrequencyClick = { hz ->
                                pendingFreq = hz
                                viewModel.setFrequency(hz)
                            },
                        )
                    }
                    item {
                        WaterfallView(
                            fullBins = state.fullSpectrumBins,
                            fullCenterHz = state.fullSpectrumCenterHz,
                            fullSpanHz = state.fullSpectrumSpanHz,
                            fullSequence = state.fullSpectrumSequence,
                            viewBins = state.spectrumBins,
                            viewCenterHz = state.spectrumCenterHz,
                            viewSpanHz = state.spectrumSpanHz,
                            vfoHz = state.frequencyHz,
                            zoom = spectrumZoomState.floatValue,
                            pan = spectrumPanState.floatValue,
                            contrast = waterfallContrastState.floatValue,
                            refDb = spectrumRefState.floatValue,
                            rangeDb = spectrumRangeState.floatValue,
                            ringBuffer = waterfallRingBuffer,
                            onFrequencyClick = { hz ->
                                pendingFreq = hz
                                viewModel.setFrequency(hz)
                            },
                        )
                    }
                    item {
                        SpectrumControls(
                            refDb = spectrumRefState.floatValue,
                            rangeDb = spectrumRangeState.floatValue,
                            zoom = spectrumZoomState.floatValue,
                            pan = spectrumPanState.floatValue,
                            contrast = waterfallContrastState.floatValue,
                            autoRefEnabled = autoRefEnabled,
                            onRefDbChange = { v ->
                                spectrumRefState.floatValue = v
                                prefs.edit().putFloat("spectrum_ref", v).apply()
                            },
                            onRangeDbChange = { v ->
                                spectrumRangeState.floatValue = v
                                if (autoRefEnabled) {
                                    autoRefFrames = 0
                                    autoRefInitialized = false
                                }
                                prefs.edit().putFloat("spectrum_range", v).apply()
                            },
                            onZoomChange = { z ->
                                spectrumZoomState.floatValue = z
                                val maxPan = (0.5f - 0.5f / z) * 0.05f
                                spectrumPanState.floatValue = spectrumPanState.floatValue.coerceIn(-maxPan, maxPan)
                                viewModel.setSpectrumZoom(z)
                                viewModel.setSpectrumPan(spectrumPanState.floatValue)
                                prefs.edit().putFloat("spectrum_zoom", z).putFloat("spectrum_pan", spectrumPanState.floatValue).apply()
                            },
                            onPanChange = { p ->
                                spectrumPanState.floatValue = p
                                viewModel.setSpectrumPan(p)
                                prefs.edit().putFloat("spectrum_pan", p).apply()
                            },
                            onContrastChange = { c ->
                                waterfallContrastState.floatValue = c
                                prefs.edit().putFloat("waterfall_contrast", c).apply()
                                // Also save per-band
                                currentBand?.let { band ->
                                    prefs.edit().putFloat("wf_contrast_$band", c).apply()
                                }
                            },
                            onAutoRefToggle = { enabled ->
                                autoRefEnabled = enabled
                                if (enabled) {
                                    autoRefFrames = 0
                                    autoRefInitialized = false
                                }
                                prefs.edit().putBoolean("auto_ref_enabled", enabled).apply()
                            },
                        )
                    }
                }

                item {
                    Spacer(Modifier.height(8.dp))
                    HorizontalDivider()
                    Spacer(Modifier.height(8.dp))
                }

                item {
                    RadioControls(
                        powerOn = state.powerOn,
                        thetisStarting = state.thetisStarting,
                        connected = state.connected,
                        nrLevel = state.nrLevel,
                        anfOn = state.anfOn,
                        nbLevel = state.nbLevel,
                        diversityEnabled = state.diversityEnabled,
                        diversityPhase = state.diversityPhase,
                        diversityGainRx1 = state.diversityGainRx1,
                        diversityGainRx2 = state.diversityGainRx2,
                        diversityRef = state.diversityRef,
                        diversityAutonullResult = state.diversityAutonullResult,
                        agcEnabled = agcEnabled,
                        txProfile = state.txProfile,
                        txProfiles = txProfiles,
                        serverTxProfileNames = state.txProfileNames,
                        driveLevel = state.driveLevel,
                        rf2kOperate = state.rf2kOperate,
                        rf2kConnected = state.rf2kConnected,
                        rf2kActive = state.rf2kActive,
                        speState = state.speState,
                        speConnected = state.speConnected,
                        speActive = state.speActive,
                        onControl = onControl,
                        onAgcToggle = {
                            agcEnabled = it
                            viewModel.setAgcEnabled(it)
                            prefs.edit().putBoolean("agc_enabled", it).apply()
                        },
                        onRf2kOperate = { viewModel.rf2kOperate(it) },
                        onSpeOperate = { viewModel.speOperate() },
                    )
                }

                item {
                    Spacer(Modifier.height(8.dp))
                    HorizontalDivider()
                    Spacer(Modifier.height(8.dp))
                }

                // Volume sliders (RX Volume + TX Gain)
                item {
                    VolumeControls(
                        rxVolume = rxVolumeState.floatValue,
                        txGain = txGainState.floatValue,
                        onRxVolumeChange = onRxVolumeChange,
                        onTxGainChange = onTxGainChange,
                    )
                }

                // Audio levels + stats
                item {
                    AudioStats(
                        captureLevel = state.captureLevel,
                        playbackLevel = state.playbackLevel,
                        rttMs = state.rttMs,
                        jitterMs = state.jitterMs,
                        bufferDepth = state.bufferDepth,
                        lossPercent = state.lossPercent,
                        rxPackets = state.rxPackets,
                    )
                }

                item {
                    Spacer(Modifier.height(8.dp))
                    HorizontalDivider()
                    Spacer(Modifier.height(8.dp))
                    Row {
                        TextButton(onClick = { showSettings = true }) {
                            Icon(Icons.Default.Settings, contentDescription = null)
                            Spacer(Modifier.padding(start = 4.dp))
                            Text("Settings")
                        }
                        Spacer(Modifier.width(8.dp))
                        TextButton(onClick = { showMidiSettings = true }) {
                            Text("MIDI")
                        }
                        Spacer(Modifier.width(8.dp))
                        TextButton(onClick = { showAbout = true }) {
                            Text("About")
                        }
                    }
                }

                } // end of !yaesuActive
                } // end of !showDevices (radio screen)
            }

            // Sticky bottom bar: local volume + PTT (always visible)
            HorizontalDivider()
            Column(
                modifier = Modifier
                    .fillMaxWidth()
                    .padding(horizontal = 12.dp, vertical = 8.dp),
            ) {
                Row(
                    modifier = Modifier.fillMaxWidth(),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    Text(
                        text = "Volume:",
                        modifier = Modifier.weight(0.25f),
                        fontSize = 14.sp,
                    )
                    Slider(
                        value = volToSlider(localVolumeState.floatValue),
                        onValueChange = { localVolumeState.floatValue = sliderToVol(it) },
                        onValueChangeFinished = { onLocalVolumeChange(localVolumeState.floatValue) },
                        valueRange = 0f..1f,
                        modifier = Modifier.weight(0.55f),
                    )
                    Text(
                        text = "${(localVolumeState.floatValue * 100).toInt()}%",
                        modifier = Modifier.weight(0.2f),
                        fontSize = 14.sp,
                    )
                }
                Spacer(Modifier.height(4.dp))
                Row(
                    modifier = Modifier.fillMaxWidth(),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    // Tune button (visible when tuner available on JC-4s antenna, hidden in Yaesu mode)
                    if (state.tunerCanTune && state.tunerConnected && !yaesuActive) {
                        val freqDelta = if (tunerTuneFreq > 0 && state.frequencyHz > 0) {
                            abs(state.frequencyHz - tunerTuneFreq)
                        } else {
                            Long.MAX_VALUE // Never tuned = always stale
                        }
                        val stale = freqDelta > 25_000 // >25kHz = needs retune

                        val tuneColor = when {
                            state.tunerState == 1 -> Color(0xFF3C78DC) // Tuning = blue
                            state.tunerState == 2 && !stale -> Color(0xFF32B432) // Done OK + in range = green
                            state.tunerState == 5 && !stale -> Color(0xFF78A028) // Done assumed + in range = olive green
                            state.tunerState == 3 || state.tunerState == 4 -> Color(0xFFDCA028) // Timeout/Aborted = orange
                            else -> Color(0xFF505050) // Idle or stale = grey
                        }
                        val tuneText = when {
                            state.tunerState == 1 -> "Tune..."
                            state.tunerState == 2 && !stale -> "Tune \u2713"
                            state.tunerState == 5 && !stale -> "Tune ~"
                            state.tunerState == 3 || state.tunerState == 4 -> "Tune \u2717"
                            else -> "Tune"
                        }

                        Button(
                            onClick = {
                                if (state.tunerState == 1) {
                                    viewModel.tunerAbort()
                                } else {
                                    viewModel.tunerTune()
                                }
                            },
                            colors = ButtonDefaults.buttonColors(containerColor = tuneColor),
                            shape = RoundedCornerShape(8.dp),
                            modifier = Modifier.width(80.dp).height(56.dp),
                        ) {
                            Text(tuneText, color = Color.White, fontSize = 14.sp)
                        }
                        Spacer(Modifier.width(8.dp))
                    }
                    // PTT button takes remaining space
                    val pttToggleMode = prefs.getBoolean("ptt_toggle", false)

                    // BT remote PTT: toggle or momentary based on ptt_toggle setting
                    var lastVolumeUp by remember { mutableStateOf(false) }
                    var btToggled by remember { mutableStateOf(false) }
                    LaunchedEffect(volumeUpHeld) {
                        if (volumePttEnabled && volumeUpHeld != lastVolumeUp) {
                            lastVolumeUp = volumeUpHeld
                            if (pttToggleMode) {
                                // Toggle: only act on press (down), ignore release
                                if (volumeUpHeld) {
                                    btToggled = !btToggled
                                    viewModel.setPtt(btToggled)
                                }
                            } else {
                                viewModel.setPtt(volumeUpHeld)
                            }
                        }
                    }

                    PttButton(
                        ptt = midiPtt || volumeUpHeld || state.transmitting,
                        pttDenied = state.pttDenied || state.otherTx,
                        toggle = pttToggleMode,
                        onPttChange = { pressed ->
                            if (!pressed && midiPtt) {
                                midiPtt = false
                                midi.sendLed(com.sdrremote.service.MidiAction.Ptt, false)
                            }
                            viewModel.setPtt(pressed || midiPtt || volumeUpHeld)
                        },
                        modifier = Modifier.weight(1f),
                    )
                }
            }
        }
    }
}

/** Determine amateur band from frequency (null if outside bands) */
private fun freqToBand(hz: Long): String? = when (hz) {
    in 1_800_000..2_000_000 -> "160m"
    in 3_500_000..3_800_000 -> "80m"
    in 7_000_000..7_200_000 -> "40m"
    in 10_100_000..10_150_000 -> "30m"
    in 14_000_000..14_350_000 -> "20m"
    in 18_068_000..18_168_000 -> "17m"
    in 21_000_000..21_450_000 -> "15m"
    in 24_890_000..24_990_000 -> "12m"
    in 28_000_000..29_700_000 -> "10m"
    in 50_000_000..52_000_000 -> "6m"
    else -> null
}
