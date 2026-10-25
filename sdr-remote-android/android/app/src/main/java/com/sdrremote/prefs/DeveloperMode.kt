// SPDX-License-Identifier: GPL-2.0-or-later
package com.sdrremote.prefs

import android.content.Context
import androidx.compose.ui.graphics.Color

/**
 * The developer mode on the phone (build 298; owner decision 2026-10-23): where
 * the app keeps it, and its sign. What the mode is - the code, the rule for the
 * log - is the core's `dev_mode`, reached through `setDeveloperMode` and
 * `developerCodeMatches`. Read at start by MainActivity, before anything can
 * write a developer line, and switched by the ViewModel: one key, here, for
 * both.
 */
object DeveloperMode {
    private const val KEY = "developer_mode"

    /**
     * The amber of the sign. The desktop and the server each keep their own
     * copy of the same colour: three programs, each with its own toolkit.
     */
    val COLOUR = Color(0xFFDCA000)

    fun saved(context: Context): Boolean =
        context.getSharedPreferences("thetislink", Context.MODE_PRIVATE).getBoolean(KEY, false)

    fun save(context: Context, on: Boolean) {
        context.getSharedPreferences("thetislink", Context.MODE_PRIVATE).edit().putBoolean(KEY, on).apply()
    }
}
