// SPDX-License-Identifier: GPL-2.0-or-later
package com.sdrremote.prefs

import android.content.SharedPreferences

/**
 * The local audio settings: what they are called, and what they are when unset.
 *
 * These four are engine-local. `SetRxVolume`, `SetLocalVolume`, `SetTxGain` and
 * `SetAgcEnabled` each set one variable in the client engine and none of them
 * reaches the server - checked in `engine.rs`, not assumed. The engine is built
 * once per app start and survives every reconnect, so sending them once at
 * startup is enough, and sending them again on every connect is duplication on a
 * second trigger.
 *
 * The keys and the defaults live here for the reason the whole morning of
 * 2026-09-14 turned on: a second reader that copies the key and gets the default
 * wrong hands the engine one value while the screen shows another, and nothing
 * says so. That happened with the EQ bands; this is the same shape with four
 * more settings, so it starts in one place.
 */
object LocalAudioPrefs {
    /** The store these live in. The Yaesu TX chain uses a different one. */
    const val STORE = "thetislink"

    /** Thetis RX level, 0..1. */
    fun rxVolume(prefs: SharedPreferences): Float = prefs.getFloat("rx_volume", 0.5f)

    /**
     * Master playback gain, 0..1, applied to every channel the client plays.
     *
     * The Yaesu has its own ([`yaesuVolume`]); it used to share this key, which
     * is why adjusting one silently moved the other.
     */
    fun localVolume(prefs: SharedPreferences): Float = prefs.getFloat("local_volume", 1f)

    /** Listening level for the active Yaesu, 0..1. */
    fun yaesuVolume(prefs: SharedPreferences): Float = prefs.getFloat("yaesu_volume", 1f)

    /** Microphone gain towards the transmitter, 0..3 as the slider scales it. */
    fun txGain(prefs: SharedPreferences): Float = prefs.getFloat("tx_gain", 0.5f)

    /** The TX AGC - the transmit chain, not a Thetis receive control. */
    fun agcEnabled(prefs: SharedPreferences): Boolean = prefs.getBoolean("agc_enabled", false)

    // ---- writers ----
    //
    // A reviewer's point, and a fair one: a source that only covers the READS is
    // not the source. The writers still spelled the keys out, so a writer could
    // use a key the reader does not know - or the other way round - and the two
    // would drift with nothing to say so. Now the key text appears once per
    // setting, for both directions.

    fun setRxVolume(prefs: SharedPreferences, v: Float) =
        prefs.edit().putFloat("rx_volume", v).apply()

    fun setLocalVolume(prefs: SharedPreferences, v: Float) =
        prefs.edit().putFloat("local_volume", v).apply()

    fun setYaesuVolume(prefs: SharedPreferences, v: Float) =
        prefs.edit().putFloat("yaesu_volume", v).apply()

    fun setTxGain(prefs: SharedPreferences, v: Float) =
        prefs.edit().putFloat("tx_gain", v).apply()

    fun setAgcEnabled(prefs: SharedPreferences, v: Boolean) =
        prefs.edit().putBoolean("agc_enabled", v).apply()
}
