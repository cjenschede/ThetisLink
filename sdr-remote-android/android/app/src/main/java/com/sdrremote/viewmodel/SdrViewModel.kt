// SPDX-License-Identifier: GPL-2.0-or-later

package com.sdrremote.viewmodel

import android.app.Application
import android.util.Log
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import com.sdrremote.DxSpotInfo
import com.sdrremote.SdrUiState
import com.sdrremote.service.AudioRouting
import com.sdrremote.service.AudioService
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import uniffi.sdr_remote.SdrBridge

private const val TAG = "SdrViewModel"

private enum class PttTarget { Thetis, Yaesu0, Yaesu1 }

class SdrViewModel(application: Application) : AndroidViewModel(application) {

    private var bridge: SdrBridge? = null
    private var initError: String? = null
    val audioRouting = AudioRouting(application)

    /** True when Yaesu mode is active (Thetis audio/spectrum disabled) */
    private val _yaesuMode = MutableStateFlow(false)
    val yaesuMode: StateFlow<Boolean> = _yaesuMode.asStateFlow()

    /** Persistent Yaesu memory/menu data (survives the transient state clear) */
    private val _yaesuMemData = MutableStateFlow("")
    val yaesuMemData: StateFlow<String> = _yaesuMemData.asStateFlow()
    private val _yaesu2MemData = MutableStateFlow("")
    val yaesu2MemData: StateFlow<String> = _yaesu2MemData.asStateFlow()

    private val _state = MutableStateFlow(SdrUiState())
    val state: StateFlow<SdrUiState> = _state.asStateFlow()

    private var pollingJob: Job? = null
    // Data-besparing: abonneer ALLEEN de geselecteerde, aanwezige Yaesu-radio, en
    // alleen als het Yaesu-window open is of die radio actief beluisterd wordt. De
    // selector leunt op presence (YaesuPresence-broadcast), niet op een abonnement.
    private var yaesuWindowOpen = false
    private var subbed0 = false
    private var subbed1 = false
    private var wasConnected = false
    // Presence-autoswitch tracking (change-detect voor subscription/audio meeschakelen).
    private var prevYaesuSel = 0
    private var prevPresent0 = false
    private var prevPresent1 = false
    // Beoogd luistervolume voor de actieve Yaesu-radio; de ander staat op 0.
    // Init uit dezelfde prefs-key als de sticky "Volume:"-slider bij de PTT
    // ("local_volume" in de "thetislink"-store) zodat applyYaesuAudio() na herstart
    // het opgeslagen volume gebruikt i.p.v. de default (max).
    private var yaesuVol =
        getApplication<Application>().getSharedPreferences("thetislink", android.content.Context.MODE_PRIVATE)
            .getFloat("local_volume", 1.0f)
    private var requestedPtt = false
    private var requestedPttTarget: PttTarget? = null
    private var activePttTarget: PttTarget? = null
    private var pttSpeakerMuted = false

    init {
        try {
            // Phase C: relay-transport (voor mobiel achter CGNAT / zonder port-forward).
            // De keuze valt bij het aanmaken van de bridge; wijzigen vereist een herstart.
            val prefs = getApplication<Application>()
                .getSharedPreferences("thetislink", android.content.Context.MODE_PRIVATE)
            val relayEnabled = prefs.getBoolean("relay_enabled", false)
            val relayUrl = prefs.getString("relay_url", "") ?: ""
            val relayStation = prefs.getString("relay_station", "") ?: ""
            val relayToken = prefs.getString("relay_token", "") ?: ""
            // Stable per-install id: a reconnecting client reclaims its own relay slot
            // instead of piling up ghost slots. Generated once, then persisted.
            var relayInstance = prefs.getString("relay_instance_id", "") ?: ""
            if (relayInstance.isBlank()) {
                relayInstance = "a" + java.util.UUID.randomUUID().toString().replace("-", "")
                prefs.edit().putString("relay_instance_id", relayInstance).apply()
            }
            // Human device label (shown in relay logs/dashboard). Auto-default to the
            // device model on first run; user-editable later; may contain spaces.
            var deviceName = prefs.getString("relay_device_name", "") ?: ""
            if (deviceName.isBlank()) {
                deviceName = android.os.Build.MODEL ?: "Android"
                prefs.edit().putString("relay_device_name", deviceName).apply()
            }
            // Audio over UDP (low latency) vs wss (encrypted). Default on; matches the
            // desktop toggle. Applied at bridge creation, so a change needs an app restart.
            val relayUdpEnabled = prefs.getBoolean("relay_udp_enabled", true)
            // Record whether the relay is actually the active transport THIS session
            // (mirrors the bridge condition below). The settings dialog compares the
            // live toggle against this to show a "restart to apply" notice - symmetric
            // for turning the relay on AND off.
            val relayActiveSession = relayEnabled && relayUrl.isNotBlank() && relayStation.isNotBlank()
            prefs.edit().putBoolean("relay_active_session", relayActiveSession).apply()
            bridge = SdrBridge(relayEnabled, relayUrl, relayStation, relayToken, relayInstance, deviceName, relayUdpEnabled)
            Log.i(TAG, "SdrBridge created successfully (relay=$relayEnabled, udp=$relayUdpEnabled)")
            startPolling()
        } catch (e: Exception) {
            initError = e.message ?: "Unknown error"
            Log.e(TAG, "Failed to create SdrBridge", e)
            _state.value = SdrUiState(initError = initError)
        }
    }

    private fun startPolling() {
        pollingJob = viewModelScope.launch(Dispatchers.Default) {
            while (isActive) {
                delay(33) // ~30fps
                try {
                    val s = bridge?.getState() ?: continue
                    // Bij disconnect de abonnement-status resetten (server vergeet subs);
                    // bij (her)connect opnieuw abonneren volgens window/actief-state.
                    if (!s.connected) {
                        if (subbed0 || subbed1) { subbed0 = false; subbed1 = false }
                        wasConnected = false
                    } else if (!wasConnected) {
                        wasConnected = true
                        updateYaesuSubscriptions()
                    }
                    _state.value = SdrUiState(
                        connected = s.connected,
                        relayTransportFallback = s.relayTransportFallback,
                        pttDenied = s.pttDenied,
                        audioError = s.audioError,
                        authRejected = s.authRejected,
                        totpRequired = s.totpRequired,
                        connectStatusHeadline = s.connectStatusHeadline,
                        connectStatusAction = s.connectStatusAction,
                        connectStatusIsError = s.connectStatusIsError,
                        connectStatusIsAwaitingTotp = s.connectStatusIsAwaitingTotp,
                        rttMs = s.rttMs.toInt(),
                        jitterMs = s.jitterMs,
                        bufferDepth = s.bufferDepth.toInt(),
                        rxPackets = s.rxPackets.toLong(),
                        yaesuAudioPackets = s.yaesuAudioPackets.toLong(),
                        yaesuJitterMs = s.yaesuJitterMs,
                        yaesuBufferDepth = s.yaesuBufferDepth.toInt(),
                        yaesu2AudioPackets = s.yaesu2AudioPackets.toLong(),
                        yaesu2JitterMs = s.yaesu2JitterMs,
                        yaesu2BufferDepth = s.yaesu2BufferDepth.toInt(),
                        vrx1AudioPackets = s.vrx1AudioPackets.toLong(),
                        vrx1JitterMs = s.vrx1JitterMs,
                        vrx1BufferDepth = s.vrx1BufferDepth.toInt(),
                        vrx2AudioPackets = s.vrx2AudioPackets.toLong(),
                        vrx2JitterMs = s.vrx2JitterMs,
                        vrx2BufferDepth = s.vrx2BufferDepth.toInt(),
                        lossPercent = s.lossPercent.toInt(),
                        downKbps = s.downKbps.toInt(),
                        upKbps = s.upKbps.toInt(),
                        dxSpotsEnabled = s.dxSpotsEnabled,
                        captureLevel = s.captureLevel,
                        playbackLevel = s.playbackLevel,
                        frequencyHz = s.frequencyHz.toLong(),
                        frequencyRx2Hz = s.frequencyRx2Hz.toLong(),
                        mode = s.mode.toInt(),
                        smeter = s.smeter,
                        powerOn = s.powerOn,
                        txProfile = s.txProfile.toInt(),
                        nrLevel = s.nrLevel.toInt(),
                        anfOn = s.anfOn,
                        nbLevel = s.nbLevel.toInt(),
                        diversityEnabled = s.diversityEnabled,
                        diversityPhase = s.diversityPhase,
                        diversityGainRx1 = s.diversityGainRx1,
                        diversityGainRx2 = s.diversityGainRx2,
                        diversityRef = s.diversityRef.toInt(),
                        diversitySource = s.diversitySource.toInt(),
                        diversityAutonullResult = s.diversityAutonullResult.toInt(),
                        driveLevel = s.driveLevel.toInt(),
                        rxAfGain = s.rxAfGain.toInt(),
                        agcEnabled = s.agcEnabled,
                        otherTx = s.otherTx,
                        transmitting = _transmitting,
                        filterLowHz = s.filterLowHz,
                        filterHighHz = s.filterHighHz,
                        thetisConfigured = s.thetisConfigured,
                        thetisStarting = s.thetisStarting,
                        txProfileNames = s.txProfileNames,
                        spectrumBins = s.spectrumBins,
                        spectrumCenterHz = s.spectrumCenterHz.toLong(),
                        spectrumSpanHz = s.spectrumSpanHz.toLong(),
                        spectrumRefLevel = s.spectrumRefLevel.toInt(),
                        spectrumDbPerUnit = s.spectrumDbPerUnit.toInt(),
                        spectrumSequence = s.spectrumSequence.toInt(),
                        fullSpectrumBins = s.fullSpectrumBins,
                        fullSpectrumCenterHz = s.fullSpectrumCenterHz.toLong(),
                        fullSpectrumSpanHz = s.fullSpectrumSpanHz.toLong(),
                        fullSpectrumSequence = s.fullSpectrumSequence.toInt(),
                        amplitecConnected = s.amplitecConnected,
                        amplitecSwitchA = s.amplitecSwitchA.toInt(),
                        amplitecSwitchB = s.amplitecSwitchB.toInt(),
                        amplitecLabels = s.amplitecLabels,
                        tunerConnected = s.tunerConnected,
                        tunerState = s.tunerState.toInt(),
                        tunerCanTune = s.tunerCanTune,
                        speConnected = s.speConnected,
                        speState = s.speState.toInt(),
                        speBand = s.speBand.toInt(),
                        spePtt = s.spePtt,
                        spePowerW = s.spePowerW.toInt(),
                        speSwrX10 = s.speSwrX10.toInt(),
                        speTemp = s.speTemp.toInt(),
                        speWarning = s.speWarning.toInt(),
                        speAlarm = s.speAlarm.toInt(),
                        spePowerLevel = s.spePowerLevel.toInt(),
                        speAntenna = s.speAntenna.toInt(),
                        speInput = s.speInput.toInt(),
                        speVoltageX10 = s.speVoltageX10.toInt(),
                        speCurrentX10 = s.speCurrentX10.toInt(),
                        speAtuBypassed = s.speAtuBypassed,
                        speAvailable = s.speAvailable,
                        speActive = s.speActive,
                        rf2kConnected = s.rf2kConnected,
                        rf2kOperate = s.rf2kOperate,
                        rf2kBand = s.rf2kBand.toInt(),
                        rf2kFrequencyKhz = s.rf2kFrequencyKhz.toInt(),
                        rf2kTemperatureX10 = s.rf2kTemperatureX10.toInt(),
                        rf2kVoltageX10 = s.rf2kVoltageX10.toInt(),
                        rf2kCurrentX10 = s.rf2kCurrentX10.toInt(),
                        rf2kForwardW = s.rf2kForwardW.toInt(),
                        rf2kReflectedW = s.rf2kReflectedW.toInt(),
                        rf2kSwrX100 = s.rf2kSwrX100.toInt(),
                        rf2kMaxForwardW = s.rf2kMaxForwardW.toInt(),
                        rf2kMaxReflectedW = s.rf2kMaxReflectedW.toInt(),
                        rf2kMaxSwrX100 = s.rf2kMaxSwrX100.toInt(),
                        rf2kErrorState = s.rf2kErrorState.toInt(),
                        rf2kErrorText = s.rf2kErrorText,
                        rf2kAntennaType = s.rf2kAntennaType.toInt(),
                        rf2kAntennaNumber = s.rf2kAntennaNumber.toInt(),
                        rf2kTunerMode = s.rf2kTunerMode.toInt(),
                        rf2kTunerSetup = s.rf2kTunerSetup,
                        rf2kTunerLNh = s.rf2kTunerLNh.toInt(),
                        rf2kTunerCPf = s.rf2kTunerCPf.toInt(),
                        rf2kDriveW = s.rf2kDriveW.toInt(),
                        rf2kModulation = s.rf2kModulation,
                        rf2kMaxPowerW = s.rf2kMaxPowerW.toInt(),
                        rf2kDeviceName = s.rf2kDeviceName,
                        rf2kAvailable = s.rf2kAvailable,
                        rf2kActive = s.rf2kActive,
                        yaesuConnected = s.yaesuConnected,
                        yaesuFreqA = s.yaesuFreqA.toLong(),
                        yaesuFreqB = s.yaesuFreqB.toLong(),
                        yaesuMode = s.yaesuMode.toInt(),
                        yaesuSmeter = s.yaesuSmeter.toInt(),
                        yaesuTxActive = s.yaesuTxActive,
                        yaesuPowerOn = s.yaesuPowerOn,
                        yaesuAfGain = s.yaesuAfGain.toInt(),
                        yaesuTxPower = s.yaesuTxPower.toInt(),
                        yaesuSquelch = s.yaesuSquelch.toInt(),
                        yaesuRfGain = s.yaesuRfGain.toInt(),
                        yaesuMicGain = s.yaesuMicGain.toInt(),
                        yaesuVfoSelect = s.yaesuVfoSelect.toInt(),
                        yaesuMemoryChannel = s.yaesuMemoryChannel.toInt(),
                        yaesuSplit = s.yaesuSplit,
                        yaesuScan = s.yaesuScan,
                        playbackLevelYaesu = s.playbackLevelYaesu,
                        yaesuMemoryData = if (s.yaesuMemoryData.isNotEmpty()) {
                            _yaesuMemData.value = s.yaesuMemoryData
                            s.yaesuMemoryData
                        } else _yaesuMemData.value,
                        yaesuModel = s.yaesuModel.toInt(),
                        yaesuTunerState = s.yaesuTunerState.toInt(),
                        yaesuFeatureToggles = s.yaesuFeatureToggles,
                        yaesuFeatureLevels = s.yaesuFeatureLevels.map { it.toInt() },
                        yaesuFeatureFreqs = s.yaesuFeatureFreqs.map { it.toInt() },
                        yaesu2Connected = s.yaesu2Connected,
                        yaesu2Model = s.yaesu2Model.toInt(),
                        yaesu2TunerState = s.yaesu2TunerState.toInt(),
                        yaesu2FreqA = s.yaesu2FreqA.toLong(),
                        yaesu2FreqB = s.yaesu2FreqB.toLong(),
                        yaesu2Mode = s.yaesu2Mode.toInt(),
                        yaesu2Smeter = s.yaesu2Smeter.toInt(),
                        yaesu2TxActive = s.yaesu2TxActive,
                        yaesu2PowerOn = s.yaesu2PowerOn,
                        yaesu2AfGain = s.yaesu2AfGain.toInt(),
                        yaesu2TxPower = s.yaesu2TxPower.toInt(),
                        yaesu2Squelch = s.yaesu2Squelch.toInt(),
                        yaesu2RfGain = s.yaesu2RfGain.toInt(),
                        yaesu2MicGain = s.yaesu2MicGain.toInt(),
                        yaesu2VfoSelect = s.yaesu2VfoSelect.toInt(),
                        yaesu2MemoryChannel = s.yaesu2MemoryChannel.toInt(),
                        yaesu2Split = s.yaesu2Split,
                        yaesu2Scan = s.yaesu2Scan,
                        playbackLevelYaesu2 = s.playbackLevelYaesu2,
                        yaesu2MemoryData = if (s.yaesu2MemoryData.isNotEmpty()) {
                            _yaesu2MemData.value = s.yaesu2MemoryData
                            s.yaesu2MemoryData
                        } else _yaesu2MemData.value,
                        yaesu2FeatureToggles = s.yaesu2FeatureToggles,
                        yaesu2FeatureLevels = s.yaesu2FeatureLevels.map { it.toInt() },
                        yaesu2FeatureFreqs = s.yaesu2FeatureFreqs.map { it.toInt() },
                        // Bewaar de gekozen radio; val terug als 'ie (nog) niet connected is.
                        selectedRadio = run {
                            val sel = _state.value.selectedRadio
                            when {
                                sel == 1 && !s.yaesu2Connected && s.yaesuConnected -> 0
                                sel == 0 && !s.yaesuConnected && s.yaesu2Connected -> 1
                                else -> sel
                            }
                        },
                        ubConnected = s.ubConnected,
                        ubFrequencyKhz = s.ubFrequencyKhz.toInt(),
                        ubBand = s.ubBand.toInt(),
                        ubDirection = s.ubDirection.toInt(),
                        ubOffState = s.ubOffState,
                        ubMotorsMoving = s.ubMotorsMoving.toInt(),
                        ubMotorCompletion = s.ubMotorCompletion.toInt(),
                        ubFwMajor = s.ubFwMajor.toInt(),
                        ubFwMinor = s.ubFwMinor.toInt(),
                        ubAvailable = s.ubAvailable,
                        ubElementsMm = s.ubElementsMm.map { it.toInt() },
                        rotorConnected = s.rotorConnected,
                        rotorAngleX10 = s.rotorAngleX10.toInt(),
                        rotorRotating = s.rotorRotating,
                        rotorTargetX10 = s.rotorTargetX10.toInt(),
                        rotorAvailable = s.rotorAvailable,
                        dxSpots = s.dxSpots.map { spot ->
                            DxSpotInfo(
                                callsign = spot.callsign,
                                frequencyHz = spot.frequencyHz.toLong(),
                                mode = spot.mode,
                                spotter = spot.spotter,
                                comment = spot.comment,
                                ageSeconds = spot.ageSeconds.toInt(),
                                expirySeconds = spot.expirySeconds.toInt(),
                            )
                        },
                    )
                    // Auto-switch TX profile + Yaesu EQ on headset connect/disconnect
                    checkMicProfileSwitch()
                    checkYaesuEqAutoSwitch()
                    // Presence-autocorrectie: subscription/audio meeschakelen als een
                    // radio wegvalt/bijkomt.
                    checkYaesuPresenceAutoSwitch()
                } catch (e: Exception) {
                    Log.e(TAG, "Polling error", e)
                }
            }
        }
    }

    private var lastHeadsetActive: Boolean? = null

    /** Track headset state for PTT-time TX profile switch.
     *  Does NOT send profile change — that happens only in setPtt(). */
    fun checkMicProfileSwitch() {
        lastHeadsetActive = audioRouting.headsetActive
    }

    private var lastHeadsetForEq: Boolean? = null

    /** Laadt de aan de GESELECTEERDE radio + huidige audio-route (BT/mic) toegewezen
     *  EQ-preset uit prefs naar de engine, en signaleert de UI (sliders) via
     *  eq_preset_pending. Aangeroepen bij PTT (operator-wens: EQ per radio uit geheugen
     *  halen op zendmoment) én bij headset-wissel. */
    private fun loadAssignedYaesuEqPreset() {
        if (!_yaesuMode.value) return
        val prefs = getApplication<Application>().getSharedPreferences("thetislink_eq", android.content.Context.MODE_PRIVATE)
        val headsetNow = audioRouting.headsetActive
        // Per-radio toegewezen preset (slot-specifieke key): elke radio z'n eigen keuze.
        val presetName = if (headsetNow)
            prefs.getString("eq_preset_bt_${sel()}", "") ?: ""
        else
            prefs.getString("eq_preset_mic_${sel()}", "") ?: ""
        if (presetName.isNotBlank()) {
            try {
                val json = org.json.JSONObject(prefs.getString("eq_presets", "{}") ?: "{}")
                if (json.has(presetName)) {
                    val arr = json.getJSONArray(presetName)
                    for (i in 0..4) {
                        yaesuEqBandSel(i, arr.getDouble(i).toFloat())
                        prefs.edit().putFloat("eq_band_${sel()}_$i", arr.getDouble(i).toFloat()).apply()
                    }
                    // Signaleer de UI om de sliders bij te werken.
                    prefs.edit().putString("eq_preset_pending", presetName).apply()
                }
            } catch (e: Exception) {
                Log.w(TAG, "Failed to load EQ preset '$presetName'", e)
            }
        }
        // Enable EQ when headset is active OR when a preset is assigned for this radio.
        val enableEq = headsetNow || presetName.isNotBlank()
        yaesuEqEnabledSel(enableEq)
        prefs.edit().putBoolean("eq_enabled_${sel()}", enableEq).apply()
        Log.i(TAG, "Yaesu EQ geladen (radio=${sel()}, headset=$headsetNow, preset=$presetName, on=$enableEq)")
    }

    /** Auto-load the assigned Yaesu EQ preset when the audio route (BT headset ↔ phone
     *  mic) changes. The per-radio, per-PTT load zit in loadAssignedYaesuEqPreset(). */
    private fun checkYaesuEqAutoSwitch() {
        if (!_yaesuMode.value) return
        val headsetNow = audioRouting.headsetActive
        if (headsetNow != lastHeadsetForEq) {
            loadAssignedYaesuEqPreset()
        }
        lastHeadsetForEq = headsetNow
    }

    fun connect(addr: String, password: String = "") {
        bridge?.connect(addr, password)
        AudioService.start(getApplication())
        audioRouting.start()
    }

    fun sendTotpCode(code: String) {
        bridge?.sendTotpCode(code)
    }

    fun disconnect() {
        bridge?.disconnect()
        audioRouting.stop()
        AudioService.stop(getApplication())
    }

    // Yaesu FT-991A
    fun yaesuEnable(on: Boolean) {
        _yaesuMode.value = on
        val prefs = getApplication<Application>().getSharedPreferences("thetislink", android.content.Context.MODE_PRIVATE)
        viewModelScope.launch(Dispatchers.IO) {
            if (on) {
                // First enable Yaesu stream, then mute local Thetis audio (not server-wide)
                bridge?.yaesuEnable(true)
                bridge?.yaesuReadMemories()
                delay(200)
                bridge?.setLocalVolume(0f)
                bridge?.enableSpectrum(false)
            } else {
                // First restore local Thetis audio to saved slider value, then disable Yaesu
                val savedLocalVol = prefs.getFloat("local_volume", 1f)
                bridge?.setLocalVolume(savedLocalVol)
                bridge?.enableSpectrum(true)
                bridge?.setSpectrumFps(prefs.getInt("spectrum_fps", 15).toUByte())
                bridge?.setSpectrumMaxBins(2048u) // anders default (hoge) bin-count → hoge datarate
                delay(200)
                bridge?.yaesuEnable(false)
            }
        }
    }
    fun yaesuPtt(on: Boolean) { bridge?.yaesuPtt(on) }
    fun yaesuVolume(vol: Float) { bridge?.yaesuVolume(vol) }
    fun yaesuSelectVfo(vfo: Int) { bridge?.yaesuSelectVfo(vfo.toUByte()) }
    fun yaesuRecallMemory(ch: Int) { bridge?.yaesuRecallMemory(ch.toUShort()) }
    fun yaesuFreq(hz: Long) { bridge?.yaesuFreq(hz.toULong()) }
    fun yaesuMode(mode: Int) { bridge?.yaesuMode(mode.toUByte()) }
    fun yaesuButton(id: Int) { bridge?.yaesuButton(id.toUShort()) }
    fun yaesuTxGain(gain: Float) { bridge?.yaesuTxGain(gain) }
    // EQ routeert naar de geselecteerde radio (was altijd radio 1 → EQ deed niets op
    // de FTX-1/radio 2). De engine houdt per radio een aparte EQ (yaesu_eq/yaesu2_eq).
    fun yaesuEqBandSel(band: Int, gainDb: Float) {
        if (sel() == 1) bridge?.yaesu2EqBand(band.toUByte(), gainDb) else bridge?.yaesuEqBand(band.toUByte(), gainDb)
    }
    fun yaesuEqEnabledSel(on: Boolean) {
        if (sel() == 1) bridge?.yaesu2EqEnabled(on) else bridge?.yaesuEqEnabled(on)
    }
    // Client-side TX-keten (gedeeld voor beide radio's, zelfde mic): compressor 0-100 + AGC-toggle.
    // Compressor/AGC per radio (engine heeft aparte keten per slot): routeer naar de
    // geselecteerde radio, net als EQ (*Sel).
    fun yaesuCompressor(level: Int) {
        if (sel() == 1) bridge?.yaesu2Compressor(level.toUByte()) else bridge?.yaesuCompressor(level.toUByte())
    }
    fun yaesuTxAgc(on: Boolean) {
        if (sel() == 1) bridge?.yaesu2TxAgc(on) else bridge?.yaesuTxAgc(on)
    }

    // ── Radio-selectie & audio-routing (Android bedient één Yaesu-radio tegelijk) ──
    // Beide slots zijn na connect geabonneerd (discovery). "Yaesu active" en de selector
    // regelen de audio puur via volume (alleen de actieve+gekozen radio hoorbaar) + Thetis-mute.
    private fun sel(): Int = _state.value.selectedRadio

    /** Alleen de actieve+geselecteerde Yaesu-radio hoorbaar; de ander (en beide bij inactief) stil. */
    private fun applyYaesuAudio() {
        val active = _yaesuMode.value
        val s = sel()
        val targetVolume = if (pttSpeakerMuted) 0f else yaesuVol
        bridge?.yaesuVolume(if (active && s == 0) targetVolume else 0f)
        bridge?.yaesu2Volume(if (active && s == 1) targetVolume else 0f)
    }

    /** Yaesu-window open/dicht (data-besparing): open → abonneer de geselecteerde,
     *  aanwezige radio; dicht → alleen een actief-beluisterde radio blijft, rest af. */
    fun setYaesuWindowOpen(open: Boolean) {
        if (yaesuWindowOpen == open) return
        yaesuWindowOpen = open
        updateYaesuSubscriptions()
    }

    /** Eén stream (databesparing, PATCH-android-yaesu-presence-datasaver): abonneer
     *  ALLEEN de geselecteerde radio, en alleen als 'ie aanwezig is (presence komt
     *  los uit YaesuPresence, niet uit de stream) én het window open is of 'ie actief
     *  beluisterd wordt. De selector leunt op presence, niet op een abonnement, dus
     *  "beide abonneren voor de selector" is niet meer nodig. Meldt alleen bij wijziging. */
    private fun updateYaesuSubscriptions() {
        val active = _yaesuMode.value
        val s = sel()
        val st = _state.value
        val want0 = (s == 0) && st.yaesuConnected && (yaesuWindowOpen || active)
        val want1 = (s == 1) && st.yaesu2Connected && (yaesuWindowOpen || active)
        viewModelScope.launch(Dispatchers.IO) {
            val changed = (want0 != subbed0) || (want1 != subbed1)
            if (want0 != subbed0) {
                subbed0 = want0
                if (want0) { bridge?.yaesuVolume(0f); bridge?.yaesuEnable(true); bridge?.yaesuReadMemories() }
                else bridge?.yaesuEnable(false)
            }
            if (want1 != subbed1) {
                subbed1 = want1
                if (want1) { bridge?.yaesu2Volume(0f); bridge?.yaesu2Enable(true); bridge?.yaesu2ReadMemories() }
                else bridge?.yaesu2Enable(false)
            }
            // L5: 1-stream-invariant observeerbaar (≤1 van beide true) + accidentele switch.
            if (changed) Log.i(TAG, "Yaesu sub slot0=$want0 slot1=$want1 (sel=$s active=$active)")
            applyYaesuAudio()
        }
    }

    /** Presence-autocorrectie: het selectedRadio-veld wordt al in de state-mapping
     *  gecorrigeerd als de geselecteerde radio wegvalt en de ander aanwezig is. Bij
     *  één stream moeten de subscription + audio dán meeschakelen.
     *  Draait per poll maar handelt alleen bij een echte wijziging (geen coroutine-spam). */
    private fun checkYaesuPresenceAutoSwitch() {
        val st = _state.value
        val sel = st.selectedRadio
        val p0 = st.yaesuConnected
        val p1 = st.yaesu2Connected
        if (sel != prevYaesuSel || p0 != prevPresent0 || p1 != prevPresent1) {
            val selChanged = sel != prevYaesuSel
            prevYaesuSel = sel
            prevPresent0 = p0
            prevPresent1 = p1
            updateYaesuSubscriptions() // (bevat presence-gate; nul present → nul streams)
            applyYaesuAudio()
            // Bij een (auto)wissel van radio ook de EQ van de nieuwe radio uit het
            // geheugen laden, net als bij handmatige selectRadio().
            if (selChanged) loadAssignedYaesuEqPreset()
        }
    }

    /** "Yaesu active": schakel tussen Thetis en de geselecteerde Yaesu-radio (audio-routing). */
    fun setYaesuActive(on: Boolean) {
        _yaesuMode.value = on
        updateYaesuSubscriptions() // actieve radio blijft geabonneerd ook als window dichtgaat
        val prefs = getApplication<Application>().getSharedPreferences("thetislink", android.content.Context.MODE_PRIVATE)
        viewModelScope.launch(Dispatchers.IO) {
            if (on) {
                applyYaesuAudio()
                bridge?.setLocalVolume(0f)   // mute lokale Thetis-audio
                bridge?.enableSpectrum(false)
            } else {
                val savedLocalVol = prefs.getFloat("local_volume", 1f)
                bridge?.setLocalVolume(savedLocalVol)
                bridge?.enableSpectrum(true)
                bridge?.setSpectrumFps(prefs.getInt("spectrum_fps", 15).toUByte())
                bridge?.setSpectrumMaxBins(2048u) // anders default (hoge) bin-count → hoge datarate
                applyYaesuAudio()            // beide Yaesu-radio's stil
            }
        }
    }

    /** Kies radio1 (0) of radio2 (1); verplaatst de audio naar de gekozen radio als actief. */
    fun selectRadio(slot: Int) {
        val st = _state.value
        if (st.yaesuTxActive || st.yaesu2TxActive) {
            Log.i(TAG, "Radio switch ignored during TX")
            return
        }
        if (st.selectedRadio == slot) return
        _state.value = st.copy(selectedRadio = slot)
        updateYaesuSubscriptions() // bij window-dicht+actief: abonneer de nieuwe, meld de oude af
        applyYaesuAudio()
        // EQ van de nu-geselecteerde radio uit het geheugen laden: sliders tonen die
        // waarden en de engine zendt ermee bij PTT (operator-wens: per radio z'n eigen EQ).
        loadAssignedYaesuEqPreset()
    }

    // Slot-geroute bediening voor de UI (dispatcht op selectedRadio):
    fun yaesuPttSel(on: Boolean) { requestPtt(on, selectedYaesuPttTarget()) }
    fun yaesuVolumeSel(vol: Float) { yaesuVol = vol; applyYaesuAudio() }
    fun yaesuFreqSel(hz: Long) { if (sel() == 0) bridge?.yaesuFreq(hz.toULong()) else bridge?.yaesu2Freq(hz.toULong()) }
    fun yaesuModeSel(mode: Int) { if (sel() == 0) bridge?.yaesuMode(mode.toUByte()) else bridge?.yaesu2Mode(mode.toUByte()) }
    fun yaesuButtonSel(id: Int) { if (sel() == 0) bridge?.yaesuButton(id.toUShort()) else bridge?.yaesu2Button(id.toUShort()) }
    fun yaesuSelectVfoSel(vfo: Int) { if (sel() == 0) bridge?.yaesuSelectVfo(vfo.toUByte()) else bridge?.yaesu2SelectVfo(vfo.toUByte()) }
    fun yaesuRecallMemorySel(ch: Int) { if (sel() == 0) bridge?.yaesuRecallMemory(ch.toUShort()) else bridge?.yaesu2RecallMemory(ch.toUShort()) }
    /** Getypte DSP/functie-control voor de geselecteerde radio (Fase 2/3). */
    fun yaesuControlSel(control: Int, value: Int) { bridge?.yaesuControl(sel().toUByte(), control.toUByte(), value.toUShort()) }
    /** ControlId-kanaal (squelch/rfgain/power/read) voor de geselecteerde radio: +0x60 voor radio2. */
    fun yaesuSetControlSel(controlId: Int, value: Int) {
        val id = if (sel() == 1) controlId + 0x60 else controlId
        bridge?.setControl(id.toUByte(), value.toUShort())
    }
    fun yaesuTxGainSel(gain: Float) { if (sel() == 0) bridge?.yaesuTxGain(gain) else bridge?.yaesu2TxGain(gain) }

    fun setAudioMode(mode: AudioRouting.Mode) {
        audioRouting.forceMode = mode
    }

    private var _transmitting = false

    private fun selectedYaesuPttTarget(): PttTarget =
        if (sel() == 1) PttTarget.Yaesu1 else PttTarget.Yaesu0

    private fun currentMicGateDelayMs(target: PttTarget): Int {
        val prefs = getApplication<Application>().getSharedPreferences("thetislink", android.content.Context.MODE_PRIVATE)
        val key = when {
            audioRouting.headsetActive -> "mic_gate_delay_ms_android_bt"
            target == PttTarget.Thetis -> "mic_gate_delay_ms_thetis_android_mic"
            else -> "mic_gate_delay_ms_yaesu_android_mic"
        }
        val defaultMs = when {
            audioRouting.headsetActive -> 0
            target == PttTarget.Thetis -> 0
            else -> 100
        }
        return prefs.getInt(key, defaultMs).coerceIn(0, 800)
    }

    private fun applyPttStartSideEffects() {
        val prefs = getApplication<Application>().getSharedPreferences("thetislink", android.content.Context.MODE_PRIVATE)
        val key = if (audioRouting.headsetActive) "mic_profile_android_bt" else "mic_profile_android_mic"
        val profileName = prefs.getString(key, "") ?: ""
        if (profileName.isNotEmpty()) {
            val profiles = _state.value.txProfileNames
            val idx = profiles.indexOf(profileName)
            if (idx >= 0) {
                bridge?.setControl(0x03u, idx.toUShort())
            }
        }
    }

    private fun setPttSpeakerMuted(mute: Boolean) {
        if (pttSpeakerMuted == mute) return
        pttSpeakerMuted = mute
        bridge?.setPlaybackMute(mute)
        // On release, recompute Yaesu volume: applyYaesuAudio() may have run mid-TX
        // (e.g. a volume-slider drag) and zeroed the server-side Yaesu volume while
        // pttSpeakerMuted was true; without this it would stay 0 after release until
        // the next applyYaesuAudio() call, leaving Yaesu RX silent post-TX.
        if (!mute) applyYaesuAudio()
    }

    private fun sendActualPtt(target: PttTarget, active: Boolean) {
        if (active) {
            activePttTarget = target
        } else if (activePttTarget == target) {
            activePttTarget = null
        }
        when (target) {
            PttTarget.Thetis -> bridge?.setPtt(active)
            PttTarget.Yaesu0 -> bridge?.yaesuPtt(active)
            PttTarget.Yaesu1 -> bridge?.yaesu2Ptt(active)
        }
    }

    private fun requestPtt(active: Boolean, target: PttTarget) {
        if (active) {
            if (requestedPtt && requestedPttTarget == target) return
            activePttTarget?.takeIf { it != target }?.let { sendActualPtt(it, false) }
            requestedPtt = true
            requestedPttTarget = target
            _transmitting = true
            setPttSpeakerMuted(true)
            bridge?.setMicGateDelayMs(currentMicGateDelayMs(target).toUInt())
            applyPttStartSideEffects()
            sendActualPtt(target, true)
            return
        }

        if (!requestedPtt && activePttTarget == null) return
        requestedPtt = false
        requestedPttTarget = null
        _transmitting = false
        activePttTarget?.let { sendActualPtt(it, false) }
        setPttSpeakerMuted(false)
    }

    fun setPtt(active: Boolean) {
        val target = if (_yaesuMode.value) selectedYaesuPttTarget() else PttTarget.Thetis
        requestPtt(active, target)
    }
    fun setDxSpotsEnabled(enabled: Boolean) { bridge?.setDxSpotsEnabled(enabled) }
    fun setRxVolume(volume: Float) { bridge?.setRxVolume(volume) }
    fun setLocalVolume(volume: Float) {
        // In Yaesu mode, local Thetis audio must stay muted
        if (_yaesuMode.value) {
            bridge?.setLocalVolume(0f)
        } else {
            bridge?.setLocalVolume(volume)
        }
    }
    fun setTxGain(gain: Float) { bridge?.setTxGain(gain) }
    fun setFrequency(hz: Long) { bridge?.setFrequency(hz.toULong()) }
    fun setMode(mode: Int) { bridge?.setMode(mode.toUByte()) }
    fun setControl(controlId: Int, value: Int) { bridge?.setControl(controlId.toUByte(), value.toUShort()) }
    fun setAgcEnabled(enabled: Boolean) { bridge?.setAgcEnabled(enabled) }
    fun enableSpectrum(enabled: Boolean) { bridge?.enableSpectrum(enabled) }
    /** Spectrum aan/uit voor de 30 s screen-grace (punt 1). Bij aanzetten ook de
     *  opgeslagen FPS herstellen (zoals bij het verlaten van Yaesu-mode). */
    fun setSpectrumActive(enabled: Boolean) {
        if (enabled) {
            val prefs = getApplication<Application>().getSharedPreferences("thetislink", android.content.Context.MODE_PRIVATE)
            bridge?.enableSpectrum(true)
            bridge?.setSpectrumFps(prefs.getInt("spectrum_fps", 15).toUByte())
            bridge?.setSpectrumMaxBins(2048u) // anders default (hoge) bin-count → hoge datarate
        } else {
            bridge?.enableSpectrum(false)
        }
    }
    fun setSpectrumFps(fps: Int) { bridge?.setSpectrumFps(fps.toUByte()) }
    fun setSpectrumMaxBins(bins: Int) { bridge?.setSpectrumMaxBins(bins.toUShort()) }
    fun setSpectrumZoom(zoom: Float) { bridge?.setSpectrumZoom(zoom) }
    fun setSpectrumPan(pan: Float) { bridge?.setSpectrumPan(pan) }
    fun setAmplitecSwitchA(pos: Int) { bridge?.setAmplitecSwitchA(pos.toUByte()) }
    fun setAmplitecSwitchB(pos: Int) { bridge?.setAmplitecSwitchB(pos.toUByte()) }
    fun tunerTune() { bridge?.tunerTune() }
    fun tunerAbort() { bridge?.tunerAbort() }
    fun speOperate() { bridge?.speOperate() }
    fun speTune() { bridge?.speTune() }
    fun speAntenna() { bridge?.speAntenna() }
    fun speInput() { bridge?.speInput() }
    fun spePower() { bridge?.spePower() }
    fun speBandUp() { bridge?.speBandUp() }
    fun speBandDown() { bridge?.speBandDown() }
    fun speOff() { bridge?.speOff() }
    fun spePowerOn() { bridge?.spePowerOn() }
    fun speDriveDown() { bridge?.speDriveDown() }
    fun speDriveUp() { bridge?.speDriveUp() }
    fun rf2kOperate(on: Boolean) { bridge?.rf2kOperate(on) }
    fun rf2kTune() { bridge?.rf2kTune() }
    fun rf2kAnt1() { bridge?.rf2kAnt1() }
    fun rf2kAnt2() { bridge?.rf2kAnt2() }
    fun rf2kAnt3() { bridge?.rf2kAnt3() }
    fun rf2kAnt4() { bridge?.rf2kAnt4() }
    fun rf2kAntExt() { bridge?.rf2kAntExt() }
    fun rf2kErrorReset() { bridge?.rf2kErrorReset() }
    fun rf2kClose() { bridge?.rf2kClose() }
    fun rf2kDriveUp() { bridge?.rf2kDriveUp() }
    fun rf2kDriveDown() { bridge?.rf2kDriveDown() }
    fun rf2kTunerMode(mode: UByte) { bridge?.rf2kTunerMode(mode) }
    fun rf2kTunerBypass(on: Boolean) { bridge?.rf2kTunerBypass(on) }
    fun rf2kTunerReset() { bridge?.rf2kTunerReset() }
    fun rf2kTunerStore() { bridge?.rf2kTunerStore() }
    fun rf2kTunerLUp() { bridge?.rf2kTunerLUp() }
    fun rf2kTunerLDown() { bridge?.rf2kTunerLDown() }
    fun rf2kTunerCUp() { bridge?.rf2kTunerCUp() }
    fun rf2kTunerCDown() { bridge?.rf2kTunerCDown() }
    fun rf2kTunerK() { bridge?.rf2kTunerK() }
    fun ubRetract() { bridge?.ubRetract() }
    fun ubSetFrequency(khz: Int, direction: Int) { bridge?.ubSetFrequency(khz.toUShort(), direction.toUByte()) }
    fun ubReadElements() { bridge?.ubReadElements() }
    fun rotorGoTo(angleX10: Int) { bridge?.rotorGoto(angleX10.toUShort()) }
    fun rotorStop() { bridge?.rotorStop() }
    fun rotorCw() { bridge?.rotorCw() }
    fun rotorCcw() { bridge?.rotorCcw() }
    fun serverReboot() { bridge?.serverReboot() }
    fun serverShutdown() { bridge?.serverShutdown() }

    override fun onCleared() {
        pollingJob?.cancel()
        bridge?.shutdown()
        super.onCleared()
    }
}
