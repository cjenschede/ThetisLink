// SPDX-License-Identifier: GPL-2.0-or-later
package com.sdrremote.ui

import android.content.Context
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableFloatStateOf
import androidx.compose.runtime.setValue
import androidx.compose.ui.platform.LocalConfiguration
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp

/**
 * How large ThetisLink draws itself, and what that earns it.
 *
 * The owner can already make this app smaller today by turning down Android's
 * own screen zoom, and measured that it works: at the smallest system zoom the
 * transmit button still keys without trouble, and in landscape there is room for
 * far more at once. The objection was not the result but the reach - a system
 * setting shrinks every app on the phone, and only this one was meant.
 *
 * So the same arithmetic, applied to our own drawing only. Everything inside
 * ThetisLink scales: buttons, icons, text, meters, the spectrum. Nothing outside
 * it changes. It is the same idea as `zoom_factor` in the desktop client.
 *
 * Deliberately NOT excluded from the scaling: the transmit button. It shrinks
 * with the rest, on the owner's decision of 2026-09-08 - someone who chooses
 * landscape accepts the layout as it is for now, and a properly reworked
 * landscape layout is a later question. Writing that down because the opposite
 * is the obvious thing to do and a reviewer will ask.
 *
 * **Dialogs do not follow this scale, and that is measured rather than assumed.**
 * A dialog in Compose lives in a window of its own with its own composition, and
 * the density we provide does not reach it: the owner compared the MIDI dialog at
 * 100% and at 60% on 2026-09-08 and it was the same size both times.
 *
 * Left that way on purpose for now. A dialog that keeps its own size is the safer
 * half of the inconsistency - its buttons stay easy to hit while the screen behind
 * it is dense - and the inventory holds: there is exactly one hand-built dialog
 * in this app, the MIDI one, and it scrolls. Every other is a Material3
 * `AlertDialog`, which handles its own height. What it costs is consistency: a
 * setting that says "make everything smaller" leaves the dialogs alone. If that
 * turns out to bother anyone in use, the fix is to provide the same density
 * inside each dialog, and it is eleven call sites.
 */
object UiScale {

    /** Fixed steps rather than a slider: each one can be looked at and described. */
    val STEPS = listOf(1.00f, 0.85f, 0.70f, 0.60f, 0.50f, 0.42f)

    /**
     * The width, in density-independent pixels, from which the layout has room
     * to stand on its side. 600 is Android's own boundary between a phone and a
     * tablet, and it is what `values-sw600dp` would have used.
     */
    const val LANDSCAPE_MIN_DP = 600

    private const val KEY = "ui_scale"
    private const val PREFS = "thetislink"

    private var _scale by mutableFloatStateOf(1.0f)

    /** Read by the UI; recomposes everything when it changes. */
    val scale: Float get() = _scale

    fun load(context: Context) {
        val p = context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
        _scale = nearestStep(p.getFloat(KEY, 1.0f))
    }

    fun set(context: Context, value: Float) {
        val v = nearestStep(value)
        _scale = v
        context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
            .edit().putFloat(KEY, v).apply()
    }

    /**
     * A stored value that is not one of the steps - an older build, a hand-edited
     * preference, a step removed from the list later - is snapped to the nearest
     * one rather than used as it stands. A scale of zero would divide by zero in
     * the rule below, and a scale of 5 would draw one button per screen.
     */
    fun nearestStep(value: Float): Float = STEPS.minByOrNull { kotlin.math.abs(it - value) } ?: 1.0f

    /**
     * May this device turn on its side?
     *
     * The question is not "is this a tablet" but "is there room", and scaling is
     * a second way to have room. A tablet reaches it at full size (737dp measured
     * on a Galaxy Tab S7+); a phone of 360dp reaches it at 60% or smaller. Both
     * arrive at the same rule from their own side.
     *
     * `smallestWidthDp` is what Android reports for the device, so at scale 1.0
     * this answers exactly what `values-sw600dp` would have answered.
     */
    fun landscapeAllowed(smallestWidthDp: Int, scale: Float): Boolean {
        if (scale <= 0f) return false
        return smallestWidthDp / scale >= LANDSCAPE_MIN_DP
    }
}

/**
 * How tall the screen is **in the dp this composition draws in**.
 *
 * There are two kinds of dp once the app scales itself, and mixing them is
 * silent. `LocalConfiguration.screenHeightDp` is Android's figure and does not
 * move when we override the density; a `Modifier.height(...)` inside our
 * composition is in the scaled kind. Write 420.dp for a list and at 60% it is
 * physically 60% of what it was, on a screen that now has room for far more -
 * which is exactly how the chat came to leave over half the screen empty
 * (owner, 2026-09-08).
 *
 * So: any dimension that is meant as "a share of the screen" has to come from
 * here, and any dimension that is meant as "this many millimetres of glass" -
 * a touch target, a hairline - should stay a literal dp and scale along.
 *
 * A dialog is the exception, and it is measured rather than assumed: dialogs do
 * not inherit our density (see the note on this object), so inside one the two
 * kinds are the same and `LocalConfiguration` may be used directly there.
 */
@Composable
fun screenHeightInOurDp(): Dp = (LocalConfiguration.current.screenHeightDp / UiScale.scale).dp
