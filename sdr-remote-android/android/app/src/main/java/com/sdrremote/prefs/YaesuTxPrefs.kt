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
 * ViewModel read through this. The second reader is the moment to make one
 * source, not later.
 */
object YaesuTxPrefs {

    /** The store these live in. */
    const val STORE = "thetislink_eq"

    /** The five EQ band gains in dB for `radio`, in the order the sliders show. */
    fun bands(prefs: SharedPreferences, radio: Int): FloatArray =
        FloatArray(5) { i -> band(prefs, radio, i) }

    fun band(prefs: SharedPreferences, radio: Int, index: Int): Float =
        prefs.getFloat(bandKey(radio, index), prefs.getFloat("eq_band_$index", 0f))

    fun eqEnabled(prefs: SharedPreferences, radio: Int): Boolean =
        prefs.getBoolean(eqEnabledKey(radio), prefs.getBoolean("eq_enabled", false))

    fun txAgc(prefs: SharedPreferences, radio: Int): Boolean =
        prefs.getBoolean(txAgcKey(radio), prefs.getBoolean("yaesu_tx_agc", false))

    fun compressor(prefs: SharedPreferences, radio: Int): Float =
        prefs.getFloat(compressorKey(radio), prefs.getFloat("yaesu_comp", 0f))

    /**
     * Mic gain as the engine wants it. No older shared key for this one - it was
     * per radio from the start - so the default stands on its own.
     */
    fun micGain(prefs: SharedPreferences, radio: Int): Float =
        prefs.getFloat(micGainKey(radio), 0.2f)

    // ---- writers ----
    //
    // The same point as for the other helper: a source that covers
    // only the READS is not the source. A writer that spells the key out can use
    // one the reader has never heard of, and the two drift with nothing to say so.
    // The key text now appears once per setting, for both directions.

    fun setBand(prefs: SharedPreferences, radio: Int, index: Int, gainDb: Float) =
        prefs.edit().putFloat(bandKey(radio, index), gainDb).apply()

    fun setEqEnabled(prefs: SharedPreferences, radio: Int, on: Boolean) =
        prefs.edit().putBoolean(eqEnabledKey(radio), on).apply()

    fun setTxAgc(prefs: SharedPreferences, radio: Int, on: Boolean) =
        prefs.edit().putBoolean(txAgcKey(radio), on).apply()

    fun setCompressor(prefs: SharedPreferences, radio: Int, v: Float) =
        prefs.edit().putFloat(compressorKey(radio), v).apply()

    fun setMicGain(prefs: SharedPreferences, radio: Int, v: Float) =
        prefs.edit().putFloat(micGainKey(radio), v).apply()

    private fun bandKey(radio: Int, index: Int) = "eq_band_${radio}_$index"
    private fun eqEnabledKey(radio: Int) = "eq_enabled_$radio"
    private fun txAgcKey(radio: Int) = "yaesu_tx_agc_$radio"
    private fun compressorKey(radio: Int) = "yaesu_comp_$radio"
    private fun micGainKey(radio: Int) = "yaesu_mic_gain_$radio"

    // ---- presets ----
    //
    // One JSON array per name under PRESETS: the five band gains, and since
    // build 38 the TX chain behind them (mic gain, compressor, AGC as 0 or 1), so
    // an older app still reads its first five. A preset from before carries no
    // chain, and applying it leaves the chain as it is - as on the desktop, where
    // a profile from before build 33 keeps the current chain.

    const val PRESETS = "eq_presets"

    class Chain(val micGain: Float, val compressor: Float, val agc: Boolean) {
        override fun toString() = "mic gain $micGain, comp ${compressor.toInt()}, AGC $agc"
    }

    class Preset(val bands: FloatArray, val chain: Chain?)

    fun presetNames(prefs: SharedPreferences): List<String> = try {
        org.json.JSONObject(prefs.getString(PRESETS, "{}") ?: "{}").keys().asSequence().toList().sorted()
    } catch (_: Exception) { emptyList() }

    fun preset(prefs: SharedPreferences, name: String): Preset? = try {
        val json = org.json.JSONObject(prefs.getString(PRESETS, "{}") ?: "{}")
        if (!json.has(name)) null else {
            val arr = json.getJSONArray(name)
            val bands = FloatArray(5) { i -> arr.getDouble(i).toFloat() }
            val chain = if (arr.length() >= 8) {
                Chain(micGain = arr.getDouble(5).toFloat(), compressor = arr.getDouble(6).toFloat(),
                    agc = arr.getDouble(7) != 0.0)
            } else null
            Preset(bands, chain)
        }
    } catch (_: Exception) { null }

    /** Stores `radio`'s current bands and TX chain as preset `name`; returns the names after it.
     *  A store that cannot be read is left alone: saving into an empty one would drop
     *  every other name. */
    fun savePreset(prefs: SharedPreferences, name: String, radio: Int): List<String> {
        val json = try { org.json.JSONObject(prefs.getString(PRESETS, "{}") ?: "{}") } catch (_: Exception) { return presetNames(prefs) }
        val arr = org.json.JSONArray()
        for (g in bands(prefs, radio)) arr.put(g.toDouble())
        arr.put(micGain(prefs, radio).toDouble())
        arr.put(compressor(prefs, radio).toDouble())
        arr.put(if (txAgc(prefs, radio)) 1 else 0)
        json.put(name, arr)
        prefs.edit().putString(PRESETS, json.toString()).apply()
        return json.keys().asSequence().toList().sorted()
    }

    /** Removes preset `name`; returns the names after it. A store that cannot be read is left alone. */
    fun deletePreset(prefs: SharedPreferences, name: String): List<String> {
        val json = try { org.json.JSONObject(prefs.getString(PRESETS, "{}") ?: "{}") } catch (_: Exception) { return presetNames(prefs) }
        json.remove(name)
        prefs.edit().putString(PRESETS, json.toString()).apply()
        return json.keys().asSequence().toList().sorted()
    }

    /** Writes a preset into `radio`'s stored settings, in one commit: the bands,
     *  and the chain when it has one. */
    fun applyPreset(prefs: SharedPreferences, radio: Int, p: Preset) {
        val e = prefs.edit()
        for (i in p.bands.indices) e.putFloat(bandKey(radio, i), p.bands[i])
        p.chain?.let {
            e.putFloat(micGainKey(radio), it.micGain)
            e.putFloat(compressorKey(radio), it.compressor)
            e.putBoolean(txAgcKey(radio), it.agc)
        }
        e.apply()
    }

    /** The name of the preset assigned to `radio` for the current audio route. */
    fun assignedPreset(prefs: SharedPreferences, radio: Int, headset: Boolean): String =
        prefs.getString(assignedKey(radio, headset), "") ?: ""

    /** Assigns preset `name` to `radio` for the headset or the phone mic; "" clears it. */
    fun setAssignedPreset(prefs: SharedPreferences, radio: Int, headset: Boolean, name: String) =
        prefs.edit().putString(assignedKey(radio, headset), name).apply()

    private fun assignedKey(radio: Int, headset: Boolean) = if (headset) "eq_preset_bt_$radio" else "eq_preset_mic_$radio"
}
