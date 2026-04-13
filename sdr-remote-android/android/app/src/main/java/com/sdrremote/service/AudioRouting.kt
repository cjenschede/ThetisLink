package com.sdrremote.service

import android.content.Context
import android.media.AudioDeviceCallback
import android.media.AudioDeviceInfo
import android.media.AudioManager
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.util.Log

/**
 * Manages audio routing for ThetisLink:
 * - Without BT headset: handsfree speaker + phone mic (default, no mode change)
 * - With BT headset: BT speaker + BT mic via SCO/HFP
 *
 * Auto-detects headset connect/disconnect. Manual override via [forceMode].
 * Only sets MODE_IN_COMMUNICATION when BT SCO is active.
 */
class AudioRouting(context: Context) {

    enum class Mode { AUTO, SPEAKER, HEADSET }

    private val audioManager = context.getSystemService(Context.AUDIO_SERVICE) as AudioManager
    private val handler = Handler(Looper.getMainLooper())
    private var started = false

    /** Current forced mode (AUTO = follow headset detection) */
    var forceMode: Mode = Mode.AUTO
        set(value) {
            field = value
            if (started) routeToPreferred()
        }

    /** True when audio is routed to a BT headset */
    var headsetActive: Boolean = false
        private set

    /** Name of connected BT headset, or null */
    var headsetName: String? = null
        private set

    private val deviceCallback = object : AudioDeviceCallback() {
        override fun onAudioDevicesAdded(addedDevices: Array<out AudioDeviceInfo>) {
            if (started) routeToPreferred()
        }

        override fun onAudioDevicesRemoved(removedDevices: Array<out AudioDeviceInfo>) {
            if (started) routeToPreferred()
        }
    }

    fun start() {
        started = true
        audioManager.registerAudioDeviceCallback(deviceCallback, handler)
        routeToPreferred()
    }

    fun stop() {
        started = false
        audioManager.unregisterAudioDeviceCallback(deviceCallback)
        disableBtSco()
        headsetActive = false
        headsetName = null
    }

    private fun routeToPreferred() {
        val btDevice = findBtDevice()
        val useBt = when (forceMode) {
            Mode.AUTO -> btDevice != null
            Mode.HEADSET -> btDevice != null
            Mode.SPEAKER -> false
        }

        if (useBt) {
            enableBtSco()
            headsetActive = true
            headsetName = btDevice?.productName?.toString() ?: "Bluetooth"
            Log.i(TAG, "Routed to BT: $headsetName")
        } else {
            disableBtSco()
            headsetActive = false
            headsetName = null
            Log.i(TAG, "Routed to handsfree speaker")
        }
    }

    private fun findBtDevice(): AudioDeviceInfo? {
        if (Build.VERSION.SDK_INT >= 31) {
            val devices = audioManager.availableCommunicationDevices
            return devices.firstOrNull { it.type == AudioDeviceInfo.TYPE_BLUETOOTH_SCO }
                ?: devices.firstOrNull { it.type == AudioDeviceInfo.TYPE_BLE_HEADSET }
        } else {
            // Legacy: can't enumerate, just check if SCO is available
            @Suppress("DEPRECATION")
            return if (audioManager.isBluetoothScoAvailableOffCall) {
                // Return a dummy non-null to indicate BT is available
                audioManager.getDevices(AudioManager.GET_DEVICES_OUTPUTS)
                    .firstOrNull { it.type == AudioDeviceInfo.TYPE_BLUETOOTH_SCO }
            } else null
        }
    }

    private fun enableBtSco() {
        audioManager.mode = AudioManager.MODE_IN_COMMUNICATION
        if (Build.VERSION.SDK_INT >= 31) {
            val bt = findBtDevice()
            if (bt != null) {
                audioManager.setCommunicationDevice(bt)
            }
        } else {
            @Suppress("DEPRECATION")
            audioManager.startBluetoothSco()
            @Suppress("DEPRECATION")
            audioManager.isBluetoothScoOn = true
            @Suppress("DEPRECATION")
            audioManager.isSpeakerphoneOn = false
        }
    }

    private fun disableBtSco() {
        if (Build.VERSION.SDK_INT >= 31) {
            audioManager.clearCommunicationDevice()
        } else {
            @Suppress("DEPRECATION")
            audioManager.isBluetoothScoOn = false
            @Suppress("DEPRECATION")
            audioManager.stopBluetoothSco()
            @Suppress("DEPRECATION")
            audioManager.isSpeakerphoneOn = true
        }
        audioManager.mode = AudioManager.MODE_NORMAL
    }

    companion object {
        private const val TAG = "AudioRouting"
    }
}
