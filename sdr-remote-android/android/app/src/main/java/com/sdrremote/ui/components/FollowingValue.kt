// SPDX-License-Identifier: GPL-2.0-or-later

package com.sdrremote.ui.components

import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.Stable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableFloatStateOf
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.runtime.setValue
import kotlinx.coroutines.delay

/**
 * A slider value that shows what the server says: the operator's value while
 * dragging and for [FOLLOW_GRACE_MS] after the release, then the server's.
 *
 * A value the radio refuses or something else sets back (a PA in Operate
 * holds the drive) leaves the server's value as it was, so a slider that
 * only follows a *change* of it kept showing the operator's value for good.
 * The grace keeps the slider still while a slow echo is on its way (hpsdr-rs
 * reports a drive change 0.6 to 1 s late, a Yaesu value comes with the next
 * CAT poll); a change from the server inside it is shown at once.
 *
 * Stable, and it has to stay so: material3 keys a slider's state on its
 * `onValueChangeFinished`, and that lambda captures this object; without
 * stability every recomposition makes a new lambda, a new slider state, and a
 * drag that breaks off under the finger. `@Stable` is a promise the compiler
 * takes, not a check: every field that changes must be Compose state. A plain
 * `var` added here keeps the drag whole but makes the promise false, and the
 * slider can then miss an update.
 */
@Stable
class FollowingValue internal constructor(initial: Float) {
    var value by mutableFloatStateOf(initial)
        internal set
    internal var dragging by mutableStateOf(false)
    internal var released by mutableIntStateOf(0)

    /** The operator moves the slider. */
    fun drag(to: Float) {
        dragging = true
        value = to
    }

    /** The operator lets go: the server's value comes back after the grace. */
    fun release() {
        dragging = false
        released++
    }
}

const val FOLLOW_GRACE_MS = 2000L

/** [server] is `null` while the server's value is not known; the slider then keeps what it shows. */
@Composable
fun rememberFollowingValue(server: Float?): FollowingValue {
    val v = remember { FollowingValue(server ?: 0f) }
    val latest by rememberUpdatedState(server)
    LaunchedEffect(server) {
        if (!v.dragging && server != null) v.value = server
    }
    LaunchedEffect(v.released) {
        if (v.released == 0) return@LaunchedEffect
        delay(FOLLOW_GRACE_MS)
        val now = latest
        if (!v.dragging && now != null) v.value = now
    }
    return v
}
