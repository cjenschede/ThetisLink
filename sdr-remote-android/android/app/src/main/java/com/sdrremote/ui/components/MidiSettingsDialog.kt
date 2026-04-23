package com.sdrremote.ui.components

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.itemsIndexed
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.compose.ui.window.Dialog
import androidx.compose.ui.window.DialogProperties
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import com.sdrremote.service.*

@Composable
fun MidiSettingsDialog(
    midi: MidiController,
    onDismiss: () -> Unit,
) {
    var ports by remember { mutableStateOf(midi.listDevices()) }
    val connectedDevice by midi.connectedDevice.collectAsStateWithLifecycle()
    val lastEvent by midi.lastEvent.collectAsStateWithLifecycle()
    val midiEvent by midi.event.collectAsStateWithLifecycle()

    var learnForAction by remember { mutableStateOf<MidiAction?>(null) }
    var mappingsVersion by remember { mutableIntStateOf(0) }

    // Handle learn mode events
    LaunchedEffect(midiEvent) {
        val ev = midiEvent
        if (ev is MidiEvent.LearnEvent && learnForAction != null) {
            val action = learnForAction!!
            val controlType = if (ev.isNote) ControlType.Button
            else when (action) {
                MidiAction.MasterVolume, MidiAction.TxGain, MidiAction.Drive -> ControlType.Slider
                else -> ControlType.Button
            }
            midi.addMapping(MidiMapping(ev.isNote, ev.channel, ev.number, controlType, action))
            midi.saveMappings()
            learnForAction = null
            midi.learnMode = false
            mappingsVersion++
        }
    }

    Dialog(
        onDismissRequest = {
            midi.learnMode = false
            onDismiss()
        },
        properties = DialogProperties(usePlatformDefaultWidth = false),
    ) {
        Surface(
            modifier = Modifier
                .fillMaxWidth(0.95f)
                .fillMaxHeight(0.85f),
            shape = RoundedCornerShape(16.dp),
            color = MaterialTheme.colorScheme.surface,
        ) {
            Column(modifier = Modifier.padding(16.dp)) {
                Text("MIDI Controller", fontSize = 20.sp, fontWeight = FontWeight.Bold)
                Spacer(Modifier.height(12.dp))

                // Device selection
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Text("Device:", modifier = Modifier.width(60.dp))
                    Button(onClick = { ports = midi.listDevices() }, modifier = Modifier.padding(end = 8.dp)) {
                        Text("Scan")
                    }
                    if (connectedDevice.isNotEmpty()) {
                        Button(onClick = { midi.disconnect(); midi.saveMappings() }) {
                            Text("Disconnect")
                        }
                    }
                }

                if (connectedDevice.isNotEmpty()) {
                    Text("Connected: $connectedDevice", color = Color(0xFF4CAF50), fontSize = 13.sp)
                } else if (ports.isEmpty()) {
                    Text("No MIDI devices found", color = Color.Gray, fontSize = 13.sp)
                } else {
                    ports.forEach { name ->
                        Button(
                            onClick = { midi.connect(name); midi.saveMappings() },
                            modifier = Modifier.padding(vertical = 2.dp),
                        ) {
                            Text(name, fontSize = 13.sp)
                        }
                    }
                }

                Spacer(Modifier.height(8.dp))
                HorizontalDivider()
                Spacer(Modifier.height(8.dp))

                // Activity monitor
                if (lastEvent.isNotEmpty()) {
                    Text("Last MIDI: $lastEvent", fontSize = 12.sp, color = Color(0xFF90CAF9))
                    Spacer(Modifier.height(4.dp))
                }

                // Learn mode indicator
                if (learnForAction != null) {
                    Text(
                        "Press a MIDI control for: ${learnForAction!!.label}",
                        fontSize = 14.sp,
                        fontWeight = FontWeight.Bold,
                        color = Color(0xFFFFA726),
                    )
                    TextButton(onClick = {
                        learnForAction = null
                        midi.learnMode = false
                    }) {
                        Text("Cancel")
                    }
                    Spacer(Modifier.height(4.dp))
                }

                // Current mappings
                Text("Mappings:", fontWeight = FontWeight.Bold, fontSize = 14.sp)
                Spacer(Modifier.height(4.dp))

                // Force recomposition on mappingsVersion change
                key(mappingsVersion) {
                    val currentMappings = midi.mappings.toList()
                    if (currentMappings.isEmpty()) {
                        Text("No mappings configured", color = Color.Gray, fontSize = 13.sp)
                    } else {
                        LazyColumn(modifier = Modifier.weight(1f, fill = false).heightIn(max = 200.dp)) {
                            itemsIndexed(currentMappings) { idx, mapping ->
                                Row(
                                    modifier = Modifier
                                        .fillMaxWidth()
                                        .padding(vertical = 2.dp)
                                        .background(Color(0xFF2A2A2A), RoundedCornerShape(4.dp))
                                        .padding(horizontal = 8.dp, vertical = 4.dp),
                                    verticalAlignment = Alignment.CenterVertically,
                                ) {
                                    Text(
                                        "${mapping.sourceLabel()} → ${mapping.action.label} (${mapping.controlType.label})",
                                        fontSize = 12.sp,
                                        modifier = Modifier.weight(1f),
                                    )
                                    TextButton(onClick = {
                                        midi.removeMapping(idx)
                                        midi.saveMappings()
                                        mappingsVersion++
                                    }) {
                                        Text("X", color = Color.Red, fontSize = 12.sp)
                                    }
                                }
                            }
                        }
                    }
                }

                Spacer(Modifier.height(8.dp))
                HorizontalDivider()
                Spacer(Modifier.height(8.dp))

                // Add mapping buttons
                Text("Add mapping:", fontWeight = FontWeight.Bold, fontSize = 14.sp)
                Spacer(Modifier.height(4.dp))

                val actions = MidiAction.entries
                LazyColumn(modifier = Modifier.weight(1f)) {
                    items(actions.size) { idx ->
                        val action = actions[idx]
                        Button(
                            onClick = {
                                learnForAction = action
                                midi.learnMode = true
                            },
                            enabled = connectedDevice.isNotEmpty() && learnForAction == null,
                            modifier = Modifier.fillMaxWidth().padding(vertical = 1.dp),
                            contentPadding = PaddingValues(horizontal = 12.dp, vertical = 4.dp),
                        ) {
                            Text("Learn: ${action.label}", fontSize = 13.sp)
                        }
                    }
                }

                Spacer(Modifier.height(8.dp))
                Row(modifier = Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.End) {
                    TextButton(onClick = {
                        midi.learnMode = false
                        onDismiss()
                    }) {
                        Text("Close")
                    }
                }
            }
        }
    }
}
