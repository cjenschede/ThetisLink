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

    private val _state = MutableStateFlow(SdrUiState())
    val state: StateFlow<SdrUiState> = _state.asStateFlow()

    private var pollingJob: Job? = null

    init {
        try {
            bridge = SdrBridge()
            Log.i(TAG, "SdrBridge created successfully")
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
                    _state.value = SdrUiState(
                        connected = s.connected,
                        pttDenied = s.pttDenied,
                        audioError = s.audioError,
                        authRejected = s.authRejected,
                        totpRequired = s.totpRequired,
                        rttMs = s.rttMs.toInt(),
                        jitterMs = s.jitterMs,
                        bufferDepth = s.bufferDepth.toInt(),
                        rxPackets = s.rxPackets.toLong(),
                        lossPercent = s.lossPercent.toInt(),
                        captureLevel = s.captureLevel,
                        playbackLevel = s.playbackLevel,
                        frequencyHz = s.frequencyHz.toLong(),
                        frequencyRx2Hz = s.frequencyRx2Hz.toLong(),
                        mode = s.mode.toInt(),
                        smeter = s.smeter.toInt(),
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

    /** Auto-enable Yaesu EQ when BT headset is active, disable for phone mic.
     *  Loads the assigned EQ preset for the active audio route.
     *  Also applies on first check (initial state). */
    private fun checkYaesuEqAutoSwitch() {
        if (!_yaesuMode.value) return
        val headsetNow = audioRouting.headsetActive
        if (headsetNow != lastHeadsetForEq) {
            val prefs = getApplication<Application>().getSharedPreferences("thetislink_eq", android.content.Context.MODE_PRIVATE)
            // Pick the assigned preset for this audio route
            val presetName = if (headsetNow)
                prefs.getString("eq_preset_bt", "") ?: ""
            else
                prefs.getString("eq_preset_mic", "") ?: ""
            // Load preset bands into engine
            if (presetName.isNotBlank()) {
                try {
                    val json = org.json.JSONObject(prefs.getString("eq_presets", "{}") ?: "{}")
                    if (json.has(presetName)) {
                        val arr = json.getJSONArray(presetName)
                        for (i in 0..4) {
                            bridge?.yaesuEqBand(i.toUByte(), arr.getDouble(i).toFloat())
                        }
                        // Signal UI to update sliders
                        prefs.edit().putString("eq_preset_pending", presetName).apply()
                        for (i in 0..4) {
                            prefs.edit().putFloat("eq_band_$i", arr.getDouble(i).toFloat()).apply()
                        }
                    }
                } catch (e: Exception) {
                    Log.w(TAG, "Failed to load EQ preset '$presetName'", e)
                }
            }
            // Enable EQ when headset is active OR when mic preset is assigned
            val enableEq = headsetNow || presetName.isNotBlank()
            bridge?.yaesuEqEnabled(enableEq)
            prefs.edit().putBoolean("eq_enabled", enableEq).apply()
            Log.i(TAG, "Yaesu EQ auto-${if (enableEq) "enabled" else "disabled"} (headset=$headsetNow, preset=$presetName)")
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
    fun yaesuEqBand(band: Int, gainDb: Float) { bridge?.yaesuEqBand(band.toUByte(), gainDb) }
    fun yaesuEqEnabled(on: Boolean) { bridge?.yaesuEqEnabled(on) }

    fun setAudioMode(mode: AudioRouting.Mode) {
        audioRouting.forceMode = mode
    }

    private var _transmitting = false

    fun setPtt(active: Boolean) {
        _transmitting = active
        // Auto-switch TX profile for current mic on PTT activation
        if (active) {
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
        if (_yaesuMode.value) {
            bridge?.yaesuPtt(active)
        } else {
            bridge?.setPtt(active)
        }
    }
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
