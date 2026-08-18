// SPDX-License-Identifier: GPL-2.0-or-later

package com.sdrremote

/**
 * The chat, as this screen needs it.
 *
 * A plain mirror of what the bridge hands over: the judgements that need the
 * shared model to make them - whose message this is, and whether it may still
 * be corrected - are answered on the Rust side, so the phone and the desktop
 * cannot end up disagreeing about them.
 */
data class ChatMessageUi(
    val id: Long,
    /** Unix seconds; the row turns it into local time. */
    val at: Long,
    /** Empty for somebody who left the chat: their words stay, they do not. */
    val name: String,
    val body: String,
    /** What this answers, or empty. */
    val replyName: String,
    val replyText: String,
    val edited: Boolean,
    val mine: Boolean,
    val canEdit: Boolean,
)

/** One answer from the administrator on a problem report. */
data class ChatAnswerUi(
    val id: Long,
    val at: Long,
    val body: String,
)

/** Why the chat is not usable, when it is not. */
enum class ChatOffline {
    /** Reachable. */
    None,

    /** No relay configured: this client has no chat, and that is a setting rather than a fault. */
    NoRelay,

    /** A relay, but it handed out no ticket - so there is no chat behind it. */
    NoTicket,

    /** Both in hand, and nothing answering. */
    Unreachable,
}

data class ChatUiState(
    val offline: ChatOffline = ChatOffline.NoRelay,
    /**
     * The service has said whether this station is in the chat. Until it has,
     * neither screen is shown - guessing wrong means showing somebody a consent
     * form they already filled in.
     */
    val consentKnown: Boolean = false,
    val consented: Boolean = false,
    val displayName: String = "",
    val unread: Int = 0,
    /** The service's own words for a refusal, or empty. */
    val error: String = "",
    val messages: List<ChatMessageUi> = emptyList(),
    val answers: List<ChatAnswerUi> = emptyList(),
)
