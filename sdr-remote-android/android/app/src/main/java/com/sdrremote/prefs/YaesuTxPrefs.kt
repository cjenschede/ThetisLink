// SPDX-License-Identifier: GPL-2.0-or-later
package com.sdrremote.prefs

import android.content.SharedPreferences

/**
 * Where the per-radio TX-chain settings live, and how to read them.
 *
 * Every one of these keys is written per radio slot and falls back to an older
 * shared key that has no slot in it, from before the two radios could differ.
 * The fallback is not decoration: an operator who set his EQ before that change
 * and has not touched a slider since still has his curve under the old key
 * alone.
 *
 * That fallback used to be spelled out at each reading site. On 2026-09-14 a
 * patch added a second reader for the bands - in the ViewModel, so the engine
 * would finally be told which curve to use - and left the fallback off. The
 * screen then showed the operator's curve while the engine was being sent five
 * zeros, and because the new code sends the bands BEFORE switching the EQ on,
 * that would have overwritten a correct engine state with a flat one. Worse than
 * the defect it was fixing.
 *
 * So the keys and their fallbacks live here, once, and both the screen and the
 * ViewModel read through this. Working rule 15: the second reader is the moment
 * to make one source, not later.
 */
object YaesuTxPrefs {

    /** The five EQ band gains in dB for `radio`, in the order the sliders show. */
    fun bands(prefs: SharedPreferences, radio: Int): FloatArray =
        FloatArray(5) { i -> band(prefs, radio, i) }

    fun band(prefs: SharedPreferences, radio: Int, index: Int): Float =
        prefs.getFloat("eq_band_${radio}_$index", prefs.getFloat("eq_band_$index", 0f))

    fun eqEnabled(prefs: SharedPreferences, radio: Int): Boolean =
        prefs.getBoolean("eq_enabled_$radio", prefs.getBoolean("eq_enabled", false))

    fun txAgc(prefs: SharedPreferences, radio: Int): Boolean =
        prefs.getBoolean("yaesu_tx_agc_$radio", prefs.getBoolean("yaesu_tx_agc", false))

    fun compressor(prefs: SharedPreferences, radio: Int): Float =
        prefs.getFloat("yaesu_comp_$radio", prefs.getFloat("yaesu_comp", 0f))

    /**
     * Mic gain as the engine wants it. No older shared key for this one - it was
     * per radio from the start - so the default stands on its own.
     */
    fun micGain(prefs: SharedPreferences, radio: Int): Float =
        prefs.getFloat("yaesu_mic_gain_$radio", 0.2f)

    // ---- writers ----
    //
    // The same point a reviewer made about the other helper: a source that covers
    // only the READS is not the source. A writer that spells the key out can use
    // one the reader has never heard of, and the two drift with nothing to say so.
    // The key text now appears once per setting, for both directions.

    fun setBand(prefs: SharedPreferences, radio: Int, index: Int, gainDb: Float) =
        prefs.edit().putFloat("eq_band_${radio}_$index", gainDb).apply()

    /** All five at once, in one commit to the store. */
    fun setBands(prefs: SharedPreferences, radio: Int, gains: FloatArray, alsoPending: String? = null) {
        val e = prefs.edit()
        for (i in gains.indices) e.putFloat("eq_band_${radio}_$i", gains[i])
        if (alsoPending != null) e.putString("eq_preset_pending", alsoPending)
        e.apply()
    }

    fun setEqEnabled(prefs: SharedPreferences, radio: Int, on: Boolean) =
        prefs.edit().putBoolean("eq_enabled_$radio", on).apply()

    fun setTxAgc(prefs: SharedPreferences, radio: Int, on: Boolean) =
        prefs.edit().putBoolean("yaesu_tx_agc_$radio", on).apply()

    fun setCompressor(prefs: SharedPreferences, radio: Int, v: Float) =
        prefs.edit().putFloat("yaesu_comp_$radio", v).apply()

    fun setMicGain(prefs: SharedPreferences, radio: Int, v: Float) =
        prefs.edit().putFloat("yaesu_mic_gain_$radio", v).apply()

    /** The name of the preset assigned to `radio` for the current audio route. */
    fun assignedPreset(prefs: SharedPreferences, radio: Int, headset: Boolean): String {
        val key = if (headset) "eq_preset_bt_$radio" else "eq_preset_mic_$radio"
        return prefs.getString(key, "") ?: ""
    }
}
