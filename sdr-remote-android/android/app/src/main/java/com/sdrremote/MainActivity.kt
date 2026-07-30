// SPDX-License-Identifier: GPL-2.0-or-later

package com.sdrremote

import android.Manifest
import android.content.pm.PackageManager
import android.os.Bundle
import android.view.InputDevice
import android.view.KeyEvent
import android.view.MotionEvent
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.result.contract.ActivityResultContracts
import androidx.core.content.ContextCompat
import com.sdrremote.ui.screens.MainScreen
import com.sdrremote.ui.theme.SdrRemoteTheme
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow

class MainActivity : ComponentActivity() {

    private val requestMicPermission = registerForActivityResult(
        ActivityResultContracts.RequestPermission()
    ) { /* granted or not — Oboe will fail gracefully if denied */ }

    /** Volume-up key state (for BT remote PTT) */
    private val _volumeUpHeld = MutableStateFlow(false)
    val volumeUpHeld: StateFlow<Boolean> = _volumeUpHeld.asStateFlow()

    /** When true, volume-up is captured for PTT instead of system volume */
    var volumePttEnabled: Boolean = false

    /** When true, the phone's OWN volume buttons act as PTT (never Bluetooth volume,
     *  so a headset like the Jabra keeps its own volume control). */
    var volumeKeysPttEnabled: Boolean = false

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        if (ContextCompat.checkSelfPermission(this, Manifest.permission.RECORD_AUDIO)
            != PackageManager.PERMISSION_GRANTED
        ) {
            requestMicPermission.launch(Manifest.permission.RECORD_AUDIO)
        }

        setContent {
            SdrRemoteTheme {
                MainScreen()
            }
        }
    }

    /** BT-remote PTT keys: page-turner / camera remotes (BLE HID). Volume keys are
     *  handled separately (see volumeKeysPttEnabled) so the phone's own volume rocker
     *  and a Bluetooth headset's volume keep working normally. */
    private fun isPttKey(keyCode: Int): Boolean =
        keyCode == KeyEvent.KEYCODE_CAMERA
            || keyCode == KeyEvent.KEYCODE_PAGE_UP
            || keyCode == KeyEvent.KEYCODE_PAGE_DOWN

    private fun isVolumeKey(keyCode: Int): Boolean =
        keyCode == KeyEvent.KEYCODE_VOLUME_UP
            || keyCode == KeyEvent.KEYCODE_VOLUME_DOWN

    /** True when the key comes from an external (Bluetooth) input device, e.g. a
     *  headset like the Jabra Evolve. Volume-key PTT must never steal these events. */
    private fun isExternalKey(event: KeyEvent?): Boolean =
        event?.device?.isExternal == true

    /** Decide whether a key should act as PTT right now:
     *   - BT page-turner / camera remote keys when the BT-remote-PTT toggle is on
     *   - the phone's OWN volume keys when the volume-keys-PTT toggle is on
     *     (but never volume events arriving over Bluetooth). */
    private fun pttForKey(keyCode: Int, event: KeyEvent?): Boolean =
        (volumePttEnabled && isPttKey(keyCode))
            || (volumeKeysPttEnabled && isVolumeKey(keyCode) && !isExternalKey(event))

    /** Last key event info for debug display */
    private val _lastKeyEvent = MutableStateFlow("")
    val lastKeyEvent: StateFlow<String> = _lastKeyEvent.asStateFlow()

    /** Intercept hardware keys at the earliest point, before the Compose view
     *  hierarchy/focus-navigation can consume them. PAGE_UP/PAGE_DOWN are navigation
     *  keys that never reach onKeyDown() (they get eaten by focus navigation), so a
     *  BLE HID keyboard PTT only works from here. Consuming the event (return true)
     *  also stops the screen from scrolling / the focus arrow from moving. */
    override fun dispatchKeyEvent(event: KeyEvent): Boolean {
        if (pttForKey(event.keyCode, event)) {
            when (event.action) {
                KeyEvent.ACTION_DOWN -> {
                    _volumeUpHeld.value = true
                    val name = KeyEvent.keyCodeToString(event.keyCode)
                    _lastKeyEvent.value = "DOWN: $name (${event.keyCode})"
                }
                KeyEvent.ACTION_UP -> _volumeUpHeld.value = false
            }
            return true // consume before focus navigation / scrolling
        }
        return super.dispatchKeyEvent(event)
    }

    override fun onKeyDown(keyCode: Int, event: KeyEvent?): Boolean {
        val name = KeyEvent.keyCodeToString(keyCode)
        _lastKeyEvent.value = "DOWN: $name ($keyCode)"
        android.util.Log.i("ThetisLink", "KeyDown: $name ($keyCode) device=${event?.device?.name} external=${event?.device?.isExternal}")
        if (pttForKey(keyCode, event)) {
            _volumeUpHeld.value = true
            return true // consume — don't change system volume / page
        }
        return super.onKeyDown(keyCode, event)
    }

    override fun onKeyUp(keyCode: Int, event: KeyEvent?): Boolean {
        if (pttForKey(keyCode, event)) {
            _volumeUpHeld.value = false
            return true
        }
        return super.onKeyUp(keyCode, event)
    }

    /** Intercept touch events from external BT devices (e.g. ZL-01 fingertip controller).
     *  These present as touch taps from a non-internal source. */
    override fun dispatchTouchEvent(event: MotionEvent?): Boolean {
        if (volumePttEnabled && event != null && isExternalTouchDevice(event)) {
            when (event.actionMasked) {
                MotionEvent.ACTION_DOWN -> {
                    _lastKeyEvent.value = "BT touch DOWN (${event.device?.name})"
                    _volumeUpHeld.value = true
                    return true
                }
                MotionEvent.ACTION_UP, MotionEvent.ACTION_CANCEL -> {
                    _lastKeyEvent.value = "BT touch UP (${event.device?.name})"
                    _volumeUpHeld.value = false
                    return true
                }
            }
        }
        return super.dispatchTouchEvent(event)
    }

    /** Check if a touch event comes from an external (Bluetooth) device, not the built-in screen. */
    private fun isExternalTouchDevice(event: MotionEvent): Boolean {
        val device = event.device ?: return false
        // External BT HID devices are not SOURCE_TOUCHSCREEN internal
        val isInternal = device.sources and InputDevice.SOURCE_TOUCHSCREEN != 0
                && !device.isExternal
        return !isInternal
    }
}
