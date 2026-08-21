// SPDX-License-Identifier: GPL-2.0-or-later

package com.sdrremote.ui.screens

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.Checkbox
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableLongStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalConfiguration
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.font.FontStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.sdrremote.ChatMessageUi
import com.sdrremote.ChatOffline
import com.sdrremote.ChatUiState
import com.sdrremote.R
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale

/**
 * The chat on a phone: the consent screen, or the conversation.
 *
 * Which of the two you see is decided by the service, not by this file - no
 * agreement means no chat. What the rows say and who may correct what is
 * decided by the shared model on the Rust side, so this screen and the desktop
 * window cannot drift apart; this only lays it out for a thumb.
 */
@Composable
fun ChatScreen(
    state: ChatUiState,
    onConsent: (String) -> Unit,
    onSend: (String, Long) -> Unit,
    onEdit: (Long, String) -> Unit,
    onLeave: (Boolean) -> Unit,
    onReport: (String, String) -> Unit,
    onDismissAnswer: (Long) -> Unit,
    buildAttachment: suspend () -> String,
) {
    // Reporting a problem does not need consent (design section 4): somebody who
    // wants no part of the conversation can still report a fault, so the button
    // sits above the fork between the two screens.
    var showReport by rememberSaveable { mutableStateOf(false) }
    if (showReport) {
        ReportDialog(
            onDismiss = { showReport = false },
            onReport = onReport,
            buildAttachment = buildAttachment,
        )
    }

    Column(modifier = Modifier.fillMaxWidth().padding(horizontal = 12.dp)) {
        if (state.offline != ChatOffline.None) {
            OfflineNote(state.offline)
            Spacer(Modifier.height(8.dp))
        }
        // The administrator's answer to a report reaches you whether or not you
        // joined the chat, so it sits above both screens too.
        // Bounded, and with a way to put one aside. It had neither: an
        // unbounded column of every answer and no dismiss at all on this front
        // end, so a reader with a few answers could not see the conversation
        // and could not scroll to it either (two users, 2026-08-20).
        if (state.answers.isNotEmpty()) {
            // A third of the SCREEN, not of what the parent offers. This screen
            // is hosted in a LazyColumn item, which measures its children with
            // an unbounded height in the scroll direction, so asking the parent
            // gave Dp.Infinity - a third of that is Dp.Infinity and the clamp
            // handed back its ceiling every time. That is why it asks the
            // window instead.
            //
            // What that ceiling costs is currently nothing: this activity is
            // locked to portrait (AndroidManifest), so screenHeightDp is always
            // the tall side and 260dp is about a quarter of it. The reasoning
            // that produced this line was about landscape, where 260dp would
            // have been two thirds of the screen - and that case does not exist
            // in this app. Nobody checked the manifest before spending a review
            // round on it (2026-08-21). The line stays because asking the
            // parent was wrong regardless, and because it is already right if
            // the lock is ever lifted.
            val cap = (LocalConfiguration.current.screenHeightDp.dp / 3)
                .coerceIn(80.dp, 260.dp)
            Column(
                modifier = Modifier
                    .fillMaxWidth()
                    .heightIn(max = cap)
                    .verticalScroll(rememberScrollState())
            ) {
                for (a in state.answers) {
                    Card(modifier = Modifier.fillMaxWidth().padding(vertical = 4.dp)) {
                        Column(Modifier.padding(10.dp)) {
                            Row(verticalAlignment = Alignment.CenterVertically) {
                                Text(
                                    stringResource(R.string.chat_answer_title),
                                    fontWeight = FontWeight.Bold,
                                    fontSize = 13.sp,
                                )
                                Spacer(Modifier.weight(1f))
                                TextButton(onClick = { onDismissAnswer(a.id) }) {
                                    Text(
                                        stringResource(R.string.chat_answer_dismiss),
                                        fontSize = 12.sp,
                                    )
                                }
                            }
                            Text(a.body, fontSize = 14.sp)
                            Text(
                                clock(a.at),
                                fontSize = 11.sp,
                                color = MaterialTheme.colorScheme.onSurface.copy(alpha = 0.6f),
                            )
                        }
                    }
                }
            }
        }
        if (state.error.isNotEmpty()) {
            Text(
                state.error,
                color = Color(0xFFE08A8A),
                fontSize = 13.sp,
                modifier = Modifier.padding(vertical = 4.dp),
            )
        }

        // A report rides on the relay's ticket, exactly like the conversation
        // does. Without one it cannot be sent - and the button was live anyway,
        // so the form could be filled in and the writing went nowhere. Greyed
        // instead of hidden: the note right above says why, and a control that
        // vanishes explains nothing (owner, 2026-08-20).
        val canReport = state.offline == ChatOffline.None
        Row(verticalAlignment = Alignment.CenterVertically) {
            TextButton(onClick = { showReport = true }, enabled = canReport) {
                Text(stringResource(R.string.chat_report_button), fontSize = 13.sp)
            }
        }
        if (!canReport) {
            Text(
                stringResource(R.string.chat_report_needs_relay),
                fontSize = 12.sp,
                color = MaterialTheme.colorScheme.onSurface.copy(alpha = 0.6f),
                modifier = Modifier.padding(bottom = 4.dp),
            )
        }

        when {
            // Nothing is shown before the service has said which screen belongs
            // here: guessing wrong means showing somebody a consent form they
            // already filled in.
            !state.consentKnown -> {
                if (state.offline == ChatOffline.None) {
                    Row(
                        modifier = Modifier.fillMaxWidth().padding(24.dp),
                        horizontalArrangement = Arrangement.Center,
                    ) { CircularProgressIndicator() }
                }
            }
            !state.consented -> ConsentScreen(onConsent = onConsent)
            else -> Conversation(
                state = state,
                onSend = onSend,
                onEdit = onEdit,
                onLeave = onLeave,
            )
        }
    }
}

@Composable
private fun OfflineNote(offline: ChatOffline) {
    // Three situations that need three different things from the user; one word
    // covering all of them sends somebody to the maker's mailbox.
    val text = when (offline) {
        ChatOffline.NoRelay -> stringResource(R.string.chat_offline_no_relay)
        ChatOffline.NoTicket -> stringResource(R.string.chat_offline_no_ticket)
        ChatOffline.Unreachable -> stringResource(R.string.chat_offline_unreachable)
        ChatOffline.None -> ""
    }
    Text(
        text,
        fontSize = 13.sp,
        color = MaterialTheme.colorScheme.onSurface.copy(alpha = 0.7f),
        modifier = Modifier.padding(vertical = 6.dp),
    )
    // What a relay is and how to reach one. The tab is now shown without one
    // (2026-08-20), so somebody curious enough to open it has to find an
    // answer here and not a dead end - the line above only says what is
    // missing.
    if (offline != ChatOffline.None) {
        for (extra in listOf(
            stringResource(R.string.chat_offline_what_a_relay_is),
            stringResource(R.string.chat_offline_get_access),
        )) {
            Text(
                extra,
                fontSize = 13.sp,
                color = MaterialTheme.colorScheme.onSurface.copy(alpha = 0.7f),
                modifier = Modifier.padding(vertical = 6.dp),
            )
        }
    }
}

@Composable
private fun ConsentScreen(onConsent: (String) -> Unit) {
    var name by rememberSaveable { mutableStateOf("") }
    // Never remembered between visits - an unticked box that ticks itself is not
    // a confirmation.
    var ageOk by remember { mutableStateOf(false) }
    Column(Modifier.padding(vertical = 8.dp)) {
        Text(
            stringResource(R.string.chat_consent_title),
            fontWeight = FontWeight.Bold,
            fontSize = 16.sp,
        )
        Spacer(Modifier.height(6.dp))
        Text(stringResource(R.string.chat_consent_intro), fontSize = 14.sp)
        Spacer(Modifier.height(10.dp))
        Text(stringResource(R.string.chat_consent_name_q), fontWeight = FontWeight.Bold, fontSize = 14.sp)
        OutlinedTextField(
            value = name,
            onValueChange = { name = it.take(40) },
            singleLine = true,
            label = { Text(stringResource(R.string.chat_consent_name_hint), fontSize = 12.sp) },
            modifier = Modifier.fillMaxWidth(),
        )
        // Under the field that asks for a callsign, because that is where the
        // choice is made. The desktop has said this since the screen existed;
        // the phone asked for a callsign and said nothing, which is the drift
        // the shared model was supposed to prevent and does not cover - the
        // model is shared, the texts are two copies (found in review,
        // 2026-08-18).
        Spacer(Modifier.height(4.dp))
        Text(
            stringResource(R.string.chat_consent_callsign_warning),
            fontSize = 12.sp,
            color = MaterialTheme.colorScheme.onSurface.copy(alpha = 0.75f),
        )
        Spacer(Modifier.height(10.dp))
        Text(stringResource(R.string.chat_consent_admin), fontSize = 13.sp)
        Spacer(Modifier.height(6.dp))
        Text(stringResource(R.string.chat_consent_stored), fontSize = 13.sp)
        Spacer(Modifier.height(6.dp))
        Text(stringResource(R.string.chat_consent_withdraw), fontSize = 13.sp)
        Spacer(Modifier.height(6.dp))
        // Same place as on the desktop, between withdrawing and access. It was
        // missing here while Android's consent was still recorded as version 3,
        // so the record said people had agreed to a clause this screen never
        // showed them (2026-08-16).
        Text(stringResource(R.string.chat_consent_ban), fontSize = 13.sp)
        Spacer(Modifier.height(6.dp))
        Text(stringResource(R.string.chat_consent_access), fontSize = 13.sp)
        Spacer(Modifier.height(10.dp))
        Row(verticalAlignment = Alignment.CenterVertically) {
            Checkbox(checked = ageOk, onCheckedChange = { ageOk = it })
            Text(stringResource(R.string.chat_consent_age), fontSize = 13.sp)
        }
        Spacer(Modifier.height(6.dp))
        // No pre-ticked box and no "by continuing you agree": the button is the
        // act of agreeing, and it does nothing until a name has been chosen and
        // the age confirmed.
        Button(
            onClick = { onConsent(name.trim()) },
            enabled = name.isNotBlank() && ageOk,
        ) { Text(stringResource(R.string.chat_consent_agree)) }
        Spacer(Modifier.height(6.dp))
        // Under the button, as on the desktop: the sentence that says refusing
        // costs nothing. Not decoration - it is part of what makes the consent
        // freely given, and it was in the resources without being on screen
        // (found in review, 2026-08-18).
        Text(
            stringResource(R.string.chat_consent_optional),
            fontSize = 12.sp,
            color = MaterialTheme.colorScheme.onSurface.copy(alpha = 0.75f),
        )
        Spacer(Modifier.height(12.dp))
    }
}

@Composable
private fun Conversation(
    state: ChatUiState,
    onSend: (String, Long) -> Unit,
    onEdit: (Long, String) -> Unit,
    onLeave: (Boolean) -> Unit,
) {
    var input by rememberSaveable { mutableStateOf("") }
    // Replying and correcting share one field and exclude each other - one input,
    // one intent.
    var replyTo by rememberSaveable { mutableLongStateOf(0L) }
    var replyWho by rememberSaveable { mutableStateOf("") }
    var editing by rememberSaveable { mutableLongStateOf(0L) }
    var confirmLeave by rememberSaveable { mutableStateOf(false) }

    val listState = rememberLazyListState()
    LaunchedEffect(state.messages.size) {
        if (state.messages.isNotEmpty()) listState.animateScrollToItem(state.messages.size - 1)
    }

    if (confirmLeave) {
        LeaveDialog(
            onDismiss = { confirmLeave = false },
            onLeave = { deleteMessages ->
                confirmLeave = false
                onLeave(deleteMessages)
            },
        )
    }

    Row(verticalAlignment = Alignment.CenterVertically) {
        Text(
            stringResource(R.string.chat_as_name, state.displayName),
            fontSize = 12.sp,
            color = MaterialTheme.colorScheme.onSurface.copy(alpha = 0.6f),
        )
        Spacer(Modifier.width(8.dp))
        TextButton(onClick = { confirmLeave = true }) {
            Text(stringResource(R.string.chat_leave), fontSize = 12.sp)
        }
    }

    LazyColumn(
        state = listState,
        modifier = Modifier.fillMaxWidth().heightIn(min = 120.dp, max = 420.dp),
    ) {
        items(state.messages, key = { it.id }) { m ->
            MessageRow(
                m = m,
                onReply = {
                    replyTo = m.id
                    replyWho = m.name
                    editing = 0L
                },
                onEdit = {
                    editing = m.id
                    replyTo = 0L
                    input = m.body
                },
            )
        }
    }
    if (state.messages.isEmpty()) {
        Text(
            stringResource(R.string.chat_empty),
            fontSize = 13.sp,
            color = MaterialTheme.colorScheme.onSurface.copy(alpha = 0.6f),
            modifier = Modifier.padding(vertical = 8.dp),
        )
    }

    // What you are answering (or correcting), right above where you type it -
    // the only place it can be seen at the moment it matters.
    if (replyTo != 0L) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            Text(
                stringResource(R.string.chat_replying_to, replyWho),
                fontSize = 12.sp,
                fontStyle = FontStyle.Italic,
                color = MaterialTheme.colorScheme.onSurface.copy(alpha = 0.7f),
            )
            TextButton(onClick = { replyTo = 0L }) { Text("x", fontSize = 12.sp) }
        }
    }
    if (editing != 0L) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            Text(
                stringResource(R.string.chat_editing),
                fontSize = 12.sp,
                fontStyle = FontStyle.Italic,
                color = MaterialTheme.colorScheme.onSurface.copy(alpha = 0.7f),
            )
            TextButton(onClick = { editing = 0L; input = "" }) { Text("x", fontSize = 12.sp) }
        }
    }

    Row(verticalAlignment = Alignment.Bottom, modifier = Modifier.padding(vertical = 4.dp)) {
        OutlinedTextField(
            value = input,
            onValueChange = { input = it.take(2000) },
            // Grows with the message rather than scrolling one line out of its
            // own view; beyond five rows it scrolls instead of eating the screen.
            maxLines = 5,
            label = { Text(stringResource(R.string.chat_input_hint), fontSize = 12.sp) },
            modifier = Modifier.weight(1f),
        )
        Spacer(Modifier.width(6.dp))
        Button(
            onClick = {
                val body = input.trim()
                if (body.isNotEmpty()) {
                    // The same field and the same button carry the correction;
                    // which one this is was said above it.
                    if (editing != 0L) onEdit(editing, body) else onSend(body, replyTo)
                    input = ""
                    replyTo = 0L
                    editing = 0L
                }
            },
            enabled = input.isNotBlank(),
        ) { Text(stringResource(R.string.chat_send)) }
    }
}

@Composable
private fun MessageRow(m: ChatMessageUi, onReply: () -> Unit, onEdit: () -> Unit) {
    Column(Modifier.padding(vertical = 3.dp)) {
        if (m.replyText.isNotEmpty()) {
            // What is being answered, above the answer. One line: enough to place
            // it, not enough to read the conversation twice.
            Text(
                "> " + (m.replyName.ifEmpty { stringResource(R.string.chat_left_user) }) +
                    ": " + m.replyText.take(60),
                fontSize = 12.sp,
                fontStyle = FontStyle.Italic,
                color = MaterialTheme.colorScheme.onSurface.copy(alpha = 0.65f),
                modifier = Modifier.padding(start = 12.dp),
            )
        }
        Row(verticalAlignment = Alignment.CenterVertically) {
            Text(
                clock(m.at),
                fontSize = 11.sp,
                color = MaterialTheme.colorScheme.onSurface.copy(alpha = 0.55f),
            )
            Spacer(Modifier.width(6.dp))
            Text(
                // Somebody who left the chat: their words stay so the
                // conversation reads, they do not.
                if (m.name.isEmpty()) stringResource(R.string.chat_left_user) else m.name + ":",
                fontSize = 13.sp,
                fontWeight = if (m.name.isEmpty()) FontWeight.Normal else FontWeight.Bold,
                fontStyle = if (m.name.isEmpty()) FontStyle.Italic else FontStyle.Normal,
                color = MaterialTheme.colorScheme.onSurface.copy(
                    alpha = if (m.name.isEmpty()) 0.6f else 1f,
                ),
            )
        }
        Text(m.body, fontSize = 14.sp, modifier = Modifier.padding(start = 4.dp))
        Row(verticalAlignment = Alignment.CenterVertically) {
            if (m.edited) {
                Text(
                    stringResource(R.string.chat_edited),
                    fontSize = 11.sp,
                    fontStyle = FontStyle.Italic,
                    color = MaterialTheme.colorScheme.onSurface.copy(alpha = 0.55f),
                )
            }
            TextButton(onClick = onReply) {
                Text(stringResource(R.string.chat_reply), fontSize = 12.sp)
            }
            // Only your own, and only while the window is open. A button that is
            // shown, opens the field and only then says the time has passed is an
            // invitation withdrawn after it was accepted.
            if (m.canEdit) {
                TextButton(onClick = onEdit) {
                    Text(stringResource(R.string.chat_edit), fontSize = 12.sp)
                }
            }
        }
    }
}

@Composable
private fun LeaveDialog(onDismiss: () -> Unit, onLeave: (Boolean) -> Unit) {
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(stringResource(R.string.chat_leave_title)) },
        text = { Text(stringResource(R.string.chat_leave_explain), fontSize = 14.sp) },
        confirmButton = {
            TextButton(onClick = { onLeave(false) }) {
                Text(stringResource(R.string.chat_leave_keep))
            }
        },
        dismissButton = {
            Row {
                TextButton(onClick = { onLeave(true) }) {
                    Text(stringResource(R.string.chat_leave_delete), color = Color(0xFFE08A8A))
                }
                TextButton(onClick = onDismiss) { Text(stringResource(R.string.chat_cancel)) }
            }
        },
    )
}

@Composable
private fun ReportDialog(
    onDismiss: () -> Unit,
    onReport: (String, String) -> Unit,
    buildAttachment: suspend () -> String,
) {
    var note by rememberSaveable { mutableStateOf("") }
    // On by default, like the desktop - a description alone rarely settles
    // anything - but nothing is read until it is ticked.
    var attach by rememberSaveable { mutableStateOf(true) }
    // NOT rememberSaveable: this holds the whole attachment, and saved state
    // travels through a Binder transaction with a one-megabyte ceiling. Losing
    // it on a rotation costs a rebuild; putting it in there costs the app.
    var preview by remember { mutableStateOf("") }
    var building by remember { mutableStateOf(false) }

    LaunchedEffect(attach) {
        if (attach && preview.isEmpty()) {
            building = true
            preview = buildAttachment()
            building = false
        }
    }

    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(stringResource(R.string.chat_report_title)) },
        text = {
            Column {
                Text(stringResource(R.string.chat_report_explain), fontSize = 13.sp)
                Spacer(Modifier.height(8.dp))
                OutlinedTextField(
                    value = note,
                    onValueChange = { note = it.take(2000) },
                    minLines = 3,
                    maxLines = 6,
                    label = { Text(stringResource(R.string.chat_report_hint), fontSize = 12.sp) },
                    modifier = Modifier.fillMaxWidth(),
                )
                Spacer(Modifier.height(8.dp))
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Checkbox(checked = attach, onCheckedChange = { attach = it })
                    Text(stringResource(R.string.chat_report_attach), fontSize = 13.sp)
                }
                if (attach) {
                    if (building) {
                        Text(
                            stringResource(R.string.chat_report_reading),
                            fontSize = 12.sp,
                            color = MaterialTheme.colorScheme.onSurface.copy(alpha = 0.65f),
                        )
                    } else {
                        // Shown before it goes, and this is not decoration: the
                        // redaction is never complete, so the last thing between
                        // a phone's log and somebody else's postbox is a person
                        // reading it (design 1.1 step 5).
                        Text(
                            stringResource(R.string.chat_report_attach_shown),
                            fontSize = 12.sp,
                            color = MaterialTheme.colorScheme.onSurface.copy(alpha = 0.65f),
                        )
                        Spacer(Modifier.height(4.dp))
                        Text(
                            preview,
                            fontSize = 10.sp,
                            modifier = Modifier
                                .fillMaxWidth()
                                .heightIn(max = 220.dp)
                                .verticalScroll(rememberScrollState()),
                        )
                    }
                }
            }
        },
        confirmButton = {
            TextButton(
                onClick = {
                    onReport(note.trim(), if (attach) preview else "")
                    onDismiss()
                },
                // Waits for the attachment it was told to send, rather than
                // quietly leaving it out.
                enabled = note.isNotBlank() && !(attach && building),
            ) { Text(stringResource(R.string.chat_report_send)) }
        },
        dismissButton = {
            TextButton(onClick = onDismiss) { Text(stringResource(R.string.chat_cancel)) }
        },
    )
}

/** A wall-clock time for a message, in the reader's own timezone. */
private fun clock(at: Long): String =
    if (at <= 0) "" else SimpleDateFormat("HH:mm", Locale.getDefault()).format(Date(at * 1000))
