package com.sdrremote.service

import android.content.Context
import android.media.midi.*
import android.os.Handler
import android.os.Looper
import android.util.Log
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow

private const val TAG = "MidiController"

/** Actions that can be mapped to MIDI controls. */
enum class MidiAction(val label: String, val configKey: String) {
    Ptt("PTT", "ptt"),
    MasterVolume("Master Volume", "master_volume"),
    TxGain("TX Gain", "tx_gain"),
    Drive("Drive", "drive"),
    NrToggle("NR Toggle", "nr_toggle"),
    AnfToggle("ANF Toggle", "anf_toggle"),
    PowerToggle("Power Toggle", "power_toggle"),
    MicAgcToggle("Mic AGC Toggle", "mic_agc_toggle");

    companion object {
        fun fromConfigKey(key: String): MidiAction? = entries.find { it.configKey == key }
    }
}

/** How the MIDI control behaves. */
enum class ControlType(val label: String, val configKey: String) {
    Button("Button", "button"),
    Slider("Slider", "slider");

    companion object {
        fun fromConfigKey(key: String): ControlType? = entries.find { it.configKey == key }
    }
}

/** One MIDI → action mapping. */
data class MidiMapping(
    val isNote: Boolean,
    val channel: Int,
    val number: Int,
    val controlType: ControlType,
    val action: MidiAction,
) {
    fun toConfig(): String {
        val msgType = if (isNote) "note" else "cc"
        return "$msgType:$channel:$number:${controlType.configKey}:${action.configKey}"
    }

    fun sourceLabel(): String {
        val msg = if (isNote) "Note" else "CC"
        return "$msg ch${channel + 1} #$number"
    }

    companion object {
        fun fromConfig(s: String): MidiMapping? {
            val parts = s.split(":")
            if (parts.size != 5) return null
            val isNote = when (parts[0]) { "note" -> true; "cc" -> false; else -> return null }
            val channel = parts[1].toIntOrNull() ?: return null
            val number = parts[2].toIntOrNull() ?: return null
            val controlType = ControlType.fromConfigKey(parts[3]) ?: return null
            val action = MidiAction.fromConfigKey(parts[4]) ?: return null
            return MidiMapping(isNote, channel, number, controlType, action)
        }
    }
}

/** Events sent from MIDI to the UI. */
sealed class MidiEvent {
    data class ButtonEvent(val action: MidiAction, val velocity: Int) : MidiEvent()
    data class SliderEvent(val action: MidiAction, val value: Int) : MidiEvent()
    data class LearnEvent(val isNote: Boolean, val channel: Int, val number: Int, val value: Int) : MidiEvent()
}

/** Manages Android MIDI input/output for USB controllers. */
class MidiController(private val context: Context) {
    private val manager: MidiManager? = context.getSystemService(Context.MIDI_SERVICE) as? MidiManager
    private val handler = Handler(Looper.getMainLooper())

    private var openDevice: MidiDevice? = null
    private var inputPort: MidiInputPort? = null   // For sending LED feedback TO the device
    private var outputPort: MidiOutputPort? = null  // For receiving messages FROM the device

    private val _connectedDevice = MutableStateFlow("")
    val connectedDevice: StateFlow<String> = _connectedDevice.asStateFlow()

    private val _lastEvent = MutableStateFlow("")
    val lastEvent: StateFlow<String> = _lastEvent.asStateFlow()

    private val _event = MutableStateFlow<MidiEvent?>(null)
    val event: StateFlow<MidiEvent?> = _event.asStateFlow()

    var learnMode = false
    var mappings = mutableListOf<MidiMapping>()

    /** List available MIDI device names. */
    @Suppress("DEPRECATION")
    fun listDevices(): List<String> {
        val mgr = manager ?: return emptyList()
        return mgr.devices.map { it.properties.getString(MidiDeviceInfo.PROPERTY_NAME) ?: "Unknown" }
    }

    /** Connect to a MIDI device by name. */
    @Suppress("DEPRECATION")
    fun connect(deviceName: String) {
        disconnect()
        val mgr = manager ?: return
        val info = mgr.devices.find {
            (it.properties.getString(MidiDeviceInfo.PROPERTY_NAME) ?: "") == deviceName
        } ?: run {
            Log.w(TAG, "MIDI device '$deviceName' not found")
            return
        }

        mgr.openDevice(info, { device ->
            if (device == null) {
                Log.w(TAG, "Failed to open MIDI device")
                return@openDevice
            }
            openDevice = device

            // Open output port (receive from device) — port 0
            if (info.outputPortCount > 0) {
                val port = device.openOutputPort(0)
                port?.connect(object : MidiReceiver() {
                    override fun onSend(data: ByteArray, offset: Int, count: Int, timestamp: Long) {
                        handleMessage(data, offset, count)
                    }
                })
                outputPort = port
            }

            // Open input port (send to device for LEDs) — port 0
            if (info.inputPortCount > 0) {
                inputPort = device.openInputPort(0)
            }

            _connectedDevice.value = deviceName
            Log.i(TAG, "MIDI connected: $deviceName")
        }, handler)
    }

    /** Disconnect the current MIDI device. */
    fun disconnect() {
        outputPort?.close()
        outputPort = null
        inputPort?.close()
        inputPort = null
        openDevice?.close()
        openDevice = null
        _connectedDevice.value = ""
    }

    val isConnected: Boolean get() = openDevice != null

    /** Send LED on/off for a mapped action. */
    fun sendLed(action: MidiAction, on: Boolean) {
        val port = inputPort ?: return
        val mapping = mappings.find { it.action == action && it.controlType == ControlType.Button } ?: return
        val velocity = if (on) 127 else 0
        val msg = if (mapping.isNote) {
            byteArrayOf((0x90 or (mapping.channel and 0x0F)).toByte(), mapping.number.toByte(), velocity.toByte())
        } else {
            byteArrayOf((0xB0 or (mapping.channel and 0x0F)).toByte(), mapping.number.toByte(), velocity.toByte())
        }
        try {
            port.send(msg, 0, msg.size)
        } catch (e: Exception) {
            Log.w(TAG, "Failed to send MIDI LED: $e")
        }
    }

    /** Add a mapping (replaces existing for same source). */
    fun addMapping(mapping: MidiMapping) {
        mappings.removeAll { it.isNote == mapping.isNote && it.channel == mapping.channel && it.number == mapping.number }
        mappings.add(mapping)
    }

    /** Remove mapping at index. */
    fun removeMapping(index: Int) {
        if (index in mappings.indices) mappings.removeAt(index)
    }

    /** Save mappings to SharedPreferences. */
    fun saveMappings() {
        val prefs = context.getSharedPreferences("thetislink", Context.MODE_PRIVATE)
        val editor = prefs.edit()
        // Clear old mappings
        var i = 0
        while (prefs.contains("midi_map_$i")) {
            editor.remove("midi_map_$i")
            i++
        }
        // Write new
        mappings.forEachIndexed { idx, m -> editor.putString("midi_map_$idx", m.toConfig()) }
        editor.putString("midi_device", _connectedDevice.value)
        editor.apply()
    }

    /** Load mappings from SharedPreferences. */
    fun loadMappings() {
        val prefs = context.getSharedPreferences("thetislink", Context.MODE_PRIVATE)
        mappings.clear()
        var i = 0
        while (true) {
            val s = prefs.getString("midi_map_$i", null) ?: break
            MidiMapping.fromConfig(s)?.let { mappings.add(it) }
            i++
        }
    }

    /** Get saved device name from preferences. */
    fun getSavedDevice(): String {
        val prefs = context.getSharedPreferences("thetislink", Context.MODE_PRIVATE)
        return prefs.getString("midi_device", "") ?: ""
    }

    /** Auto-connect to saved device if available. */
    fun autoConnect() {
        val saved = getSavedDevice()
        if (saved.isNotEmpty() && listDevices().contains(saved)) {
            connect(saved)
        }
    }

    private fun handleMessage(data: ByteArray, offset: Int, count: Int) {
        if (count < 1) return
        val status = data[offset].toInt() and 0xFF
        val msgType = status and 0xF0
        val channel = status and 0x0F

        val (isNote, number, value) = when {
            msgType == 0x90 && count >= 3 -> Triple(true, data[offset + 1].toInt() and 0x7F, data[offset + 2].toInt() and 0x7F)
            msgType == 0x80 && count >= 3 -> Triple(true, data[offset + 1].toInt() and 0x7F, 0)
            msgType == 0xB0 && count >= 3 -> Triple(false, data[offset + 1].toInt() and 0x7F, data[offset + 2].toInt() and 0x7F)
            else -> return
        }

        val label = "${if (isNote) "Note" else "CC"} ch${channel + 1} #$number val=$value"
        _lastEvent.value = label

        // Learn mode
        if (learnMode) {
            _event.value = MidiEvent.LearnEvent(isNote, channel, number, value)
            return
        }

        // Find matching mapping
        for (mapping in mappings) {
            if (mapping.isNote != isNote) continue
            if (mapping.channel != channel) continue
            if (mapping.number != number) continue

            val event = when (mapping.controlType) {
                ControlType.Button -> MidiEvent.ButtonEvent(mapping.action, value)
                ControlType.Slider -> MidiEvent.SliderEvent(mapping.action, value)
            }
            _event.value = event
            return
        }
    }

    fun close() {
        disconnect()
    }
}
