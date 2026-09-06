// SPDX-License-Identifier: GPL-2.0-or-later

package com.sdrremote.ui.components

import androidx.compose.foundation.background
import androidx.compose.foundation.gestures.awaitEachGesture
import androidx.compose.foundation.gestures.awaitFirstDown
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.sdrremote.R

/**
 * PTT button with two modes:
 * - toggle=false: Push-to-talk (momentary: press=TX, release=RX)
 * - toggle=true:  Toggle (push on, push off)
 */
@Composable
fun PttButton(
    ptt: Boolean,
    busy: Boolean,
    /** Is there still a server to transmit to? */
    connected: Boolean,
    onDown: () -> Unit,
    onUp: () -> Unit,
    modifier: Modifier = Modifier,
) {
    // This button used to hold two flags of its own - `pressed` and `toggled` -
    // and showed red if either of those or the real state was on. That made it
    // the third place on this phone holding "am I transmitting", after the
    // ViewModel and the engine, and it is the one nothing could reach.
    //
    // Every one of these was that same defect arriving by a different door:
    // somebody else taking the transmitter (build 22), a stale refusal locking
    // the button (build 33), the network pulled mid-transmission (build 37), and
    // the screen going off while transmitting (build 58). Each was answered by
    // wiring one more exit into this file.
    //
    // It holds nothing now. `ptt` is the shared state, and this draws it.
    val active = ptt
    // Read inside the gesture, which outlives the composition that started it.
    val busyNow by rememberUpdatedState(busy)

    val bgColor = when {
        busy -> Color(0xFFC87800) // Orange: other client is transmitting
        active -> Color.Red
        else -> Color(0xFF3C3C3C)
    }
    val label = when {
        busy -> stringResource(R.string.ptt_tx_in_use)
        active -> "TX"
        else -> "PTT"
    }

    Column(
        modifier = modifier.fillMaxWidth(),
        horizontalAlignment = Alignment.CenterHorizontally,
    ) {
        Box(
            modifier = Modifier
                .fillMaxWidth()
                .height(80.dp)
                .clip(RoundedCornerShape(12.dp))
                .background(bgColor)
                // The handler stays installed while the radio is busy, and the
                // press is turned away inside it.
                //
                // Taking the handler away instead looked equivalent and was not:
                // when it came back - the moment the other operator let go - a
                // finger still resting on the button was read as a fresh press by
                // the new handler, while the matching release had gone to a
                // handler that no longer existed. The button then sat red with
                // nothing transmitting until it was pressed once more. About one
                // run in three, because it depends on where the finger is at the
                // instant the other station releases (owner, build 33).
                .pointerInput(Unit) {
                    awaitEachGesture {
                        awaitFirstDown(requireUnconsumed = false)
                        if (busyNow) {
                            // Swallow the whole gesture: no latch, no command,
                            // and no half-finished press left behind.
                            while (true) {
                                val event = awaitPointerEvent()
                                event.changes.forEach { it.consume() }
                                if (event.changes.all { !it.pressed }) break
                            }
                            return@awaitEachGesture
                        }
                        // One path for both modes. Whether a press latches
                        // or has to be held is not this button's business any
                        // more, so the release is always reported and the shared
                        // rule ignores it when it means nothing.
                        onDown()
                        // try/finally, because the ending of a press must not
                        // depend on this coroutine being allowed to finish. The
                        // handler is no longer taken away underneath it, but a
                        // composition can end for other reasons and this is the
                        // exit that used to skip the release.
                        try {
                            while (true) {
                                val event = awaitPointerEvent()
                                event.changes.forEach { it.consume() }
                                if (event.changes.all { !it.pressed }) break
                            }
                        } finally {
                            onUp()
                        }
                    }
                },
            contentAlignment = Alignment.Center,
        ) {
            Text(
                text = label,
                color = Color.White,
                fontSize = 32.sp,
                fontWeight = FontWeight.Bold,
            )
        }
    }
}
