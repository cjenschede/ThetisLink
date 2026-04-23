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
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp

/**
 * PTT button with two modes:
 * - toggle=false: Push-to-talk (momentary: press=TX, release=RX)
 * - toggle=true:  Toggle (push on, push off)
 */
@Composable
fun PttButton(
    ptt: Boolean,
    pttDenied: Boolean,
    toggle: Boolean = false,
    onPttChange: (Boolean) -> Unit,
    modifier: Modifier = Modifier,
) {
    var pressed by remember { mutableStateOf(false) }
    var toggled by remember { mutableStateOf(false) }
    val localActive = if (toggle) toggled else pressed
    val active = localActive || ptt

    val bgColor = when {
        pttDenied -> Color(0xFFC87800) // Orange: other client is transmitting
        active -> Color.Red
        else -> Color(0xFF3C3C3C)
    }
    val label = when {
        pttDenied -> "TX in use"
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
                .then(if (pttDenied) Modifier else Modifier.pointerInput(toggle) {
                    awaitEachGesture {
                        awaitFirstDown(requireUnconsumed = false)
                        if (toggle) {
                            toggled = !toggled
                            onPttChange(toggled)
                            while (true) {
                                val event = awaitPointerEvent()
                                event.changes.forEach { it.consume() }
                                if (event.changes.all { !it.pressed }) break
                            }
                        } else {
                            pressed = true
                            onPttChange(true)
                            while (true) {
                                val event = awaitPointerEvent()
                                event.changes.forEach { it.consume() }
                                if (event.changes.all { !it.pressed }) break
                            }
                            pressed = false
                            onPttChange(false)
                        }
                    }
                }),
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
