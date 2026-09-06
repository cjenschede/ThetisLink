// SPDX-License-Identifier: GPL-2.0-or-later

package com.sdrremote.service

import android.annotation.SuppressLint
import android.bluetooth.BluetoothAdapter
import android.bluetooth.BluetoothDevice
import android.bluetooth.BluetoothGatt
import android.bluetooth.BluetoothGattCallback
import android.bluetooth.BluetoothGattCharacteristic
import android.bluetooth.BluetoothGattDescriptor
import android.bluetooth.BluetoothManager
import android.bluetooth.BluetoothProfile
import android.bluetooth.le.ScanCallback
import android.bluetooth.le.ScanResult
import android.bluetooth.le.ScanSettings
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.os.Build
import android.util.Log
import androidx.core.content.ContextCompat
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import java.util.UUID
import uniffi.sdr_remote.BleDropPolicy
import uniffi.sdr_remote.BlePttAction
import uniffi.sdr_remote.BlePttGate
import uniffi.sdr_remote.bleDropPolicy
import uniffi.sdr_remote.logLine

/**
 * A Bluetooth PTT button, from scan to transmitter.
 *
 * Two rules are kept out of this file on purpose, and both were written here
 * first and taken back out after review. The transmit decision lives in
 * [BlePttGate], and so does the question of whether a dropped link should be
 * waited for ([bleDropPolicy]). Both are tested in sdr-remote-logic without a
 * phone and without a button; this file reports what happened and carries out
 * what comes back.
 *
 * **Threading.** Every piece of mutable state below belongs to the main thread
 * and is only ever touched there. The GATT callbacks arrive on a binder thread
 * and do exactly one thing: hand their event to the main thread along with the
 * generation they belong to. An event from a superseded connection is dropped
 * on arrival, so an old link can no longer set a status or key a transmitter
 * after the user switched off or picked a different button.
 *
 * Ownership matters as much as the split: this is held by the ViewModel, not by
 * the settings screen, because a connection that dies when a screen closes is
 * not a PTT source.
 *
 * Android 12 and up. On 11 and older a BLE scan needs the location permission,
 * which is a large ask for a transmit button; see the patch brief.
 */
class BlePttController(
    private val context: Context,
    /** Called on the main thread. True keys the transmitter, false releases it. */
    private val onKey: (Boolean) -> Unit,
    /**
     * The link is gone, whatever the reason.
     *
     * Separate from [onKey] because a release and a link loss are different
     * things: in toggle mode a release means nothing at all, and a link loss
     * must still stop the transmitter. That distinction used to be made inside
     * this file, by keeping a flag of whether it was toggled on - a flag no
     * exit could reach (owner, build 59).
     */
    private val onGone: () -> Unit,
) {
    companion object {
        /** The button's notify service. Measured, see the patch brief. */
        val SERVICE_UUID: UUID = UUID.fromString("0000ffe0-0000-1000-8000-00805f9b34fb")

        /** Client Characteristic Configuration - the standard notify switch. */
        private val CCCD_UUID: UUID = UUID.fromString("00002902-0000-1000-8000-00805f9b34fb")

        private const val TAG = "BlePtt"

        /** Android 12. Below this the scan needs a location permission. */
        const val MIN_SDK = Build.VERSION_CODES.S

        fun supported(): Boolean = Build.VERSION.SDK_INT >= MIN_SDK
    }

    /** What the user sees while picking a button. */
    data class Found(
        val address: String,
        val name: String,
        val rssi: Int,
        /** Advertises the service this button uses - almost certainly the one. */
        val likely: Boolean,
    )

    enum class Status {
        Off,
        Scanning,
        Connecting,
        Connected,

        /**
         * The link is gone and Android is watching for it to come back.
         *
         * Not an error state, and not the same as [Off]: the button is expected
         * to return and nothing needs doing. The transmitter was released the
         * moment the link went, so waiting here is free.
         */
        Waiting,
    }

    private val main = CoroutineScope(Dispatchers.Main)
    private val gate = BlePttGate()

    private val _status = MutableStateFlow(Status.Off)
    val status: StateFlow<Status> = _status.asStateFlow()

    private val _found = MutableStateFlow<List<Found>>(emptyList())
    val found: StateFlow<List<Found>> = _found.asStateFlow()

    /** The last thing worth telling the user, and the log. */
    private val _detail = MutableStateFlow("")
    val detail: StateFlow<String> = _detail.asStateFlow()

    // --- main-thread state. Nothing below is read or written anywhere else. ---

    private var gatt: BluetoothGatt? = null
    private var scanning = false

    /**
     * Whether this button is supposed to be connected.
     *
     * The difference between a link that fell away and a link that was ended on
     * purpose. Only the first is worth waiting for.
     */
    private var wanted = false

    /** True while an unwanted drop is waiting to be picked up again. */
    private var returning = false

    /**
     * Which connection attempt is the current one.
     *
     * Bumped on every connect and every deliberate disconnect. Each callback
     * carries the generation it was created for, and anything that does not
     * match is a message from a connection nobody is listening to any more.
     *
     * It is not a count of connections and the log should not be read as one:
     * connect() disconnects first, so a single reconnect spends two, and
     * opening the picker spends another. Seen going 2 -> 5 for one
     * re-selection, which is exactly right and looks wrong.
     * Without this a slow callback from a replaced client could still set a
     * status or key the transmitter, which is what a review blocked phase 2 on.
     */
    private var generation = 0

    /**
     * The button we are meant to be on, remembered across an adapter cycle.
     *
     * Switching Bluetooth off destroys the client we hold; coming back needs a
     * fresh one, and a fresh one needs the address.
     */
    private var lastAddress: String? = null

    /**
     * Bluetooth itself being switched off and on.
     *
     * This is not the same event as the link dropping, and that is the whole
     * reason it is here. Walking out of range ends the connection and Android
     * reports it, so everything downstream happens. Switching the adapter off
     * tears the stack down underneath us: the connection callback may never
     * come, so nothing let go of the button, nothing stopped the transmitter,
     * and the status went on saying "connected" about a link that no longer
     * existed. The owner found that by switching Bluetooth off and on: the
     * button never came back, and picking it from the list again was the only
     * cure (2026-09-04).
     *
     * Watching the adapter is right whether or not the callback also arrives.
     * `Gone` is unconditional, so hearing it twice changes nothing.
     */
    private val adapterWatcher = object : BroadcastReceiver() {
        override fun onReceive(context: Context?, intent: Intent?) {
            if (intent?.action != BluetoothAdapter.ACTION_STATE_CHANGED) return
            when (intent.getIntExtra(BluetoothAdapter.EXTRA_STATE, BluetoothAdapter.ERROR)) {
                // TURNING_OFF as well as OFF: it arrives first, and the point is
                // to let go of the transmitter before the stack is gone.
                BluetoothAdapter.STATE_TURNING_OFF, BluetoothAdapter.STATE_OFF -> adapterOff()
                BluetoothAdapter.STATE_ON -> adapterOn()
            }
        }
    }

    init {
        ContextCompat.registerReceiver(
            context,
            adapterWatcher,
            IntentFilter(BluetoothAdapter.ACTION_STATE_CHANGED),
            ContextCompat.RECEIVER_NOT_EXPORTED,
        )
    }

    @SuppressLint("MissingPermission")
    private fun adapterOff() {
        if (gatt == null && !wanted) return
        // A link loss like any other, and this is the one the connection
        // callback cannot be relied on to report.
        //
        // The generation is bumped rather than reused, and begin() is what
        // reports the release. Closing the client below does not silence it:
        // a callback already in flight arrives afterwards, and with the old
        // generation still current it would be treated as live - a press from a
        // client that no longer exists (review finding).
        val gen = ++generation
        act(gate.begin(gen.toULong()), 0u, "bluetooth switched off")
        // The client cannot survive the adapter going away, so there is nothing
        // to re-arm: connect() on it would wait for a stack that has been
        // rebuilt underneath it. Let it go and come back with a fresh one.
        gatt?.let {
            it.disconnect()
            it.close()
        }
        gatt = null
        returning = false
        scanning = false
        _status.value = if (wanted) Status.Waiting else Status.Off
        say("bluetooth switched off - let go of the button")
    }

    private fun adapterOn() {
        val address = lastAddress
        if (!wanted || address == null) {
            say("bluetooth back on")
            return
        }
        say("bluetooth back on - reconnecting to $address")
        connect(address)
    }

    /**
     * Give up the adapter watch. Called when the ViewModel goes, not on an
     * ordinary disconnect - the watch belongs to this object's whole life.
     */
    fun close() {
        disconnect()
        try {
            context.unregisterReceiver(adapterWatcher)
        } catch (e: IllegalArgumentException) {
            // Already gone. Nothing to do, and nothing worth a line.
        }
    }

    private val adapter: BluetoothAdapter?
        get() = (context.getSystemService(Context.BLUETOOTH_SERVICE) as? BluetoothManager)?.adapter

    // ---------------------------------------------------------------- scanning

    @SuppressLint("MissingPermission")
    fun startScan() {
        val scanner = adapter?.takeIf { it.isEnabled }?.bluetoothLeScanner
        if (scanner == null) {
            // Named for the path, not just the cause. Both refusals used to
            // write the same line, so a log could not say which of the two had
            // spoken - and a message that cannot name its own path is evidence
            // for neither (review finding, round 4).
            note("Bluetooth is off - not scanning")
            return
        }
        if (scanning) return
        // Let go of the button before looking for it, and this is not a detail:
        // a connected peripheral does not advertise, so it cannot be found. And
        // after a drop there is a background connect still waiting for it,
        // which grabs the device the moment it advertises again - before any
        // scan can report it.
        //
        // Between them that covers every state the controller can be in after
        // the first successful connect, which is why the button was never in
        // the list a second time (owner, build 9).
        if (gatt != null || wanted) {
            say("letting go before scanning - a connected button does not advertise")
            disconnect()
        }
        _found.value = emptyList()
        _status.value = Status.Scanning
        scanning = true
        // No filter. The advertisement does not have to carry the service UUID,
        // and a button that never shows up in the list cannot be picked - so
        // everything with a name is listed and the likely ones sort first.
        scanner.startScan(
            null,
            ScanSettings.Builder().setScanMode(ScanSettings.SCAN_MODE_LOW_LATENCY).build(),
            scanCallback,
        )
        say("scanning for buttons")
    }

    @SuppressLint("MissingPermission")
    fun stopScan() {
        if (!scanning) return
        scanning = false
        adapter?.bluetoothLeScanner?.stopScan(scanCallback)
        // Stopping was the one thing in this file that left no trace. That the
        // scan ended could only be inferred from the reconnect that follows it
        // on the same path, and inferring is how a scan runs all night without
        // anyone being able to show it. The count comes along because the next
        // question after "did it stop" is always "what did it see".
        say("scan stopped after ${_found.value.size} device(s) found")
        if (_status.value == Status.Scanning) _status.value = Status.Off
    }

    private val scanCallback = object : ScanCallback() {
        @SuppressLint("MissingPermission")
        override fun onScanResult(callbackType: Int, result: ScanResult) {
            val name = result.device?.name ?: result.scanRecord?.deviceName ?: return
            if (name.isBlank()) return
            val address = result.device?.address ?: return
            val likely = result.scanRecord?.serviceUuids
                ?.any { it.uuid == SERVICE_UUID } == true
            val entry = Found(address, name, result.rssi, likely)
            main.launch {
                _found.value = (_found.value.filter { it.address != address } + entry)
                    .sortedWith(compareByDescending<Found> { it.likely }.thenByDescending { it.rssi })
            }
        }

        override fun onScanFailed(errorCode: Int) {
            main.launch {
                scanning = false
                // The status has to come along. Leaving it on Scanning made
                // stopScan() return early - it begins with !scanning - so the
                // reset never ran, and the cleanup on leaving the picker found
                // a status that was not Off and skipped the reconnect. The
                // button had already been let go of to scan at all, so it just
                // stayed that way (review finding, round 4).
                if (_status.value == Status.Scanning) _status.value = Status.Off
                note("scan failed ($errorCode)")
            }
        }
    }

    // -------------------------------------------------------------- connecting

    @SuppressLint("MissingPermission")
    fun connect(address: String) {
        stopScan()
        val device: BluetoothDevice = try {
            // takeIf { isEnabled }, the same guard startScan has. An adapter
            // object exists while Bluetooth is switched off, so without this
            // the connect went ahead: status Connecting, wanted true, and a
            // GATT attempt that cannot succeed. Reached by opening the picker
            // with Bluetooth off and then closing the settings dialog - the
            // scan returns early, so the restore path reads a status set by a
            // path that never ran (review finding, round 4).
            adapter?.takeIf { it.isEnabled }?.getRemoteDevice(address)
                ?: run { note("Bluetooth is off - not connecting"); return }
        } catch (e: IllegalArgumentException) {
            note("bad address: $address")
            return
        }
        disconnect()
        wanted = true
        lastAddress = address
        val gen = ++generation
        // The gate decides staleness now, so it has to know which attempt is
        // current. Everything still in flight from an older one is ignored
        // there, where a test can reach it - stale() below only keeps the
        // Android side tidy.
        act(gate.begin(gen.toULong()), 0u, "new attempt")
        _status.value = Status.Connecting
        say("connecting to $address (gen $gen)")
        // autoConnect stays false for the first attempt: it is direct and fast,
        // and someone is looking at the screen waiting for it. The automatic
        // half happens on the way back - see onState.
        gatt = device.connectGatt(context, false, callbackFor(gen), BluetoothDevice.TRANSPORT_LE)
    }

    /**
     * Let go, on purpose.
     *
     * Goes through the gate like every other way of losing the link, because
     * "the user switched it off while holding the button" is exactly as capable
     * of leaving a transmitter keyed as walking out of range.
     */
    @SuppressLint("MissingPermission")
    fun disconnect() {
        wanted = false
        returning = false
        // Bumped even when there is nothing to close: an attempt that has not
        // called back yet still belongs to the old generation.
        val gen = ++generation
        say("switched off, button held=${gate.held()}")
        // begin() rather than disconnected(): a deliberate stop also makes
        // everything in flight stale, and it releases in the same move.
        act(gate.begin(gen.toULong()), 0u, "switched off")
        val g = gatt ?: return
        gatt = null
        g.disconnect()
        g.close()
        _status.value = Status.Off
    }

    /**
     * Drop any press this button is holding, without touching the link.
     *
     * For the paths that end a transmission without ending the Bluetooth
     * connection - the server disconnecting, the feature being switched off.
     * A press that survives one of those would be a press nobody can release,
     * because the button only ever reports changes.
     */
    fun releaseHeld(reason: String) {
        act(gate.disconnected(generation.toULong()), 0u, reason)
    }

    /** For a log line: is the physical button down? */
    fun held(): Boolean = gate.held()

    // --------------------------------------------------------------- callbacks

    /**
     * A callback bound to one connection attempt.
     *
     * It does no work of its own beyond reading the event out of its arguments:
     * everything else happens on the main thread, tagged with the generation it
     * came from.
     */
    private fun callbackFor(gen: Int) = object : BluetoothGattCallback() {
        override fun onConnectionStateChange(g: BluetoothGatt, status: Int, newState: Int) {
            main.launch { onState(gen, g, status, newState) }
        }

        override fun onServicesDiscovered(g: BluetoothGatt, status: Int) {
            main.launch { onServices(gen, g) }
        }

        override fun onCharacteristicChanged(
            g: BluetoothGatt,
            ch: BluetoothGattCharacteristic,
            value: ByteArray,
        ) {
            val b = value.firstOrNull() ?: return
            main.launch { onByte(gen, b.toUByte()) }
        }

        // Android 12 does not call the overload above. The owner's phone runs
        // 13, so this path has never executed anywhere - it is the whole data
        // path on an older phone, and nobody has ever run it.
        @Deprecated("Kept for Android 12, which does not call the value overload")
        @Suppress("DEPRECATION")
        override fun onCharacteristicChanged(g: BluetoothGatt, ch: BluetoothGattCharacteristic) {
            val b = ch.value?.firstOrNull() ?: return
            main.launch { onByte(gen, b.toUByte()) }
        }
    }

    /** True when this event belongs to a connection nobody is listening to. */
    @SuppressLint("MissingPermission")
    private fun stale(gen: Int, g: BluetoothGatt?): Boolean {
        if (gen == generation) return false
        say("ignoring gen $gen (now $generation)")
        g?.close()
        return true
    }

    @SuppressLint("MissingPermission")
    private fun onState(gen: Int, g: BluetoothGatt, status: Int, newState: Int) {
        if (stale(gen, g)) return
        when (newState) {
            BluetoothProfile.STATE_CONNECTED -> {
                _status.value = Status.Connecting
                act(gate.connected(gen.toULong()), 0u, "connected")
                g.discoverServices()
            }

            BluetoothProfile.STATE_DISCONNECTED -> {
                // The line the whole design hangs on. No release byte can
                // arrive on a link that is gone, so this is what stops the
                // transmitter. Measured at 2 to 4 seconds out of range, which
                // fits the 4000 ms supervision timeout.
                //
                // The status code is deliberately not read. On the owner's
                // phone an established link that falls away and a local
                // teardown both report 0, so it cannot tell them apart.
                val policy = bleDropPolicy(wanted)
                val wasHeld = gate.held()
                say(
                    "link lost (status $status), button held=$wasHeld" +
                        if (policy == BleDropPolicy.RECONNECT) ", waiting for it to come back" else "",
                )
                act(gate.disconnected(gen.toULong()), 0u, "link lost")

                if (policy == BleDropPolicy.RECONNECT) {
                    // Re-arm rather than close. Calling connect() on an existing
                    // client is the documented way to get the background
                    // reconnect: Android watches for the device and links up on
                    // its own when it advertises again, which this button does
                    // by itself for at least five minutes.
                    //
                    // close() here instead would be the end of it - the client
                    // is gone and nothing is watching any more.
                    returning = true
                    _status.value = Status.Waiting
                    g.connect()
                } else {
                    _status.value = Status.Off
                    g.close()
                }
            }
        }
    }

    @SuppressLint("MissingPermission")
    private fun onServices(gen: Int, g: BluetoothGatt) {
        if (stale(gen, g)) return
        val service = g.getService(SERVICE_UUID)
        if (service == null) {
            // What happens when someone picks the wrong device out of the
            // unfiltered scan list. Not an edge case - the list shows every
            // named device on purpose.
            note("no PTT service on this device")
            return
        }
        // The first characteristic under the service that can notify - not a
        // fixed number. The number turned out to be right, but it was an
        // assumption for a while, and this way it never has to be one.
        val ch = service.characteristics.firstOrNull {
            it.properties and BluetoothGattCharacteristic.PROPERTY_NOTIFY != 0
        }
        if (ch == null) {
            note("PTT service has nothing to listen to")
            return
        }
        g.setCharacteristicNotification(ch, true)
        val cccd = ch.getDescriptor(CCCD_UUID)
        if (cccd != null) {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
                g.writeDescriptor(cccd, BluetoothGattDescriptor.ENABLE_NOTIFICATION_VALUE)
            } else {
                @Suppress("DEPRECATION")
                cccd.value = BluetoothGattDescriptor.ENABLE_NOTIFICATION_VALUE
                @Suppress("DEPRECATION")
                g.writeDescriptor(cccd)
            }
        } else {
            // Without a CCCD notifications cannot be switched on at all, and
            // the button would sit there looking connected and doing nothing.
            note("no notify descriptor - the button will stay silent")
        }
        val wasReturning = returning
        returning = false
        _status.value = Status.Connected
        note(
            if (wasReturning) "button back on its own, on ${ch.uuid}"
            else "button connected on ${ch.uuid}",
        )
    }

    private fun onByte(gen: Int, b: UByte) {
        // No stale() here on purpose: the gate is the one that judges this now,
        // and it has a test for exactly this case. A press from a replaced
        // connection comes back as Nothing.
        act(gate.notification(gen.toULong(), b), b, "notification")
    }

    // ----------------------------------------------------------------- acting

    private fun act(action: BlePttAction, byte: UByte, why: String) {
        when (action) {
            // Key first, log second, and the order is not cosmetic. say() goes
            // over the uniffi boundary into the Rust logger, which takes a lock
            // and writes to a file - real I/O, in front of the transmit command,
            // on a path this project puts before everything else. Introduced in
            // build 7 and caught in review the same day.
            BlePttAction.KEY_DOWN -> {
                onKey(true)
                say("key down ($why)")
            }
            BlePttAction.KEY_UP -> {
                onKey(false)
                say("key up ($why), was transmitting")
            }
            // The link went away. Always acted on, even when this side did
            // not think the button was down - in toggle mode it never is while
            // the radio is on the air.
            BlePttAction.GONE -> {
                onGone()
                say("gone ($why)")
            }
            BlePttAction.UNRECOGNISED -> {
                // This device already sent more than was documented: that is how
                // its second notification channel turned up. Say so, change
                // nothing.
                say("unknown byte 0x%02X from the button, ignored".format(byte.toInt()))
            }
            BlePttAction.NOTHING -> {}
        }
    }

    /**
     * One line, into the system log **and** into ThetisLink's own log file.
     *
     * All of this used to go to logcat alone, which meant the owner could never
     * see any of it and a diagnosis needed a cable - and a cable suppresses the
     * very power saving the overnight test is about. The Rust logger writes to
     * both, so one call across the bridge closes it.
     */
    private fun say(text: String) {
        Log.i(TAG, text)
        runCatching { logLine("$TAG: $text") }
    }

    /** As [say], and also worth putting on the screen. */
    private fun note(text: String) {
        say(text)
        _detail.value = text
    }
}
