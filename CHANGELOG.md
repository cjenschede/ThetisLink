# Changelog

All notable changes to ThetisLink are documented in this file. The format is
loosely based on [Keep a Changelog](https://keepachangelog.com/) and the
project follows [Semantic Versioning](https://semver.org/). Public releases
are tagged on the [`cjenschede/ThetisLink`](https://github.com/cjenschede/ThetisLink)
mirror and shipped as zipped binary bundles via that repository's Releases
page; this file is the user-facing summary so an upgrade-decision can be made
from one document.

For the protocol-level technical reference and the in-depth multi-tuner
hardware notes, see `docs-book/src/technical-reference.md` and
`docs-book/src/user-manual-en.md` (English) or
`docs-book/src/technische-referentie.md` and `docs-book/src/user-manual.md`
(Dutch).

---

## [2.9.0] — 2026-08-18 (A dropout sounds like the band again · surviving a network change · chat and problem reports on a relay)

> **Who this release changes things for.** Read this bit and skip the rest if it
> does not apply to you.
>
> - **Everyone.** A short interruption sounds like band noise again instead of
>   silence, on every channel rather than only the first - concealment had been
>   running on the wrong decoder, quietly, for as long as wideband audio has
>   existed. And a phone that changes between WiFi and mobile data gets its audio
>   back by itself, which it did not before.
> - **Stations on a relay.** A chat room shared with the other users of it, and a
>   button that reports a problem straight to the administrator with your log
>   attached - cleaned, and shown to you before it goes. Both optional; neither
>   is needed to operate.
> - **Nobody has to do anything.** No settings change, no configuration is
>   invalidated, and a station without a relay sees none of the new surface.
>
### Chat and problem reporting — an invitation, and what it is not

Stations on a relay get two things that work independently of each other.

**A room** shared with the people using the same relay. One room, no channels, no
private messages, no files. Messages carry a time and you can answer one message
in particular. Your own typo is yours to fix for fifteen minutes.

**A report button** that sends a problem straight to the administrator with your
log and settings attached — cleaned, and you see exactly what travels before it
is sent. That works whether or not you join the conversation: reporting and
chatting are separate choices.

**What it is not.** It is not a service. The relay this runs on is PA3GHM's own
server: he pays for it, he runs it, and it is there for as long as he enjoys
running it. **He may decline a request and he may stop the service.** That is
written here because it is fairer said in advance than afterwards.

Want to join? Ask PA3GHM at **pa3ghm@gmail.com**, with your callsign and a short
note about your setup. A no is a valid answer and needs no explanation.

**Without a relay** you see none of this, and the rest of ThetisLink works exactly
as before — audio, PTT, spectrum, the radio. The chat is deliberately the least
important thing on the screen and must never hold anything else up. Running your
own relay? The chat service is a separate container and its source is in this
repository, so you can put it beside your own.

**Before you agree to anything** you get a screen telling you what is kept, for
how long, and who the administrator is. You choose the name you appear under —
and worth knowing in advance: a callsign appears in a public register with your
name and address, so pick something else if you would rather not share that.
Without agreeing, the rest of ThetisLink works as usual; only the conversation is
unavailable. You can withdraw at any time.

> **Feature release.** The wire protocol gains **two packet types**
> (`0x35` `ServerReportRequest`, `0x36` `ServerReportPart`) for fetching a connected
> server's log from a client; everything else is unchanged and an older peer that
> knows neither simply never asks and never answers. A new service runs beside the
> relay in its own container — a chat update cannot stop the relay. Stock Thetis
> v2.10.3.x remains sufficient.

### Added
- **A log the phone keeps for itself.** Android's system log is one ring buffer
  shared with the whole device, so on a busy phone ThetisLink's own lines are
  evicted within minutes — a fault reproduced this morning could not be read
  back this afternoon, which is exactly the kind that only happens on mobile
  data. The app now writes the same lines to a file of its own as well, in the
  same shape and on the same clock as the desktop log, so the two can be laid
  side by side in one report. It lives in the app's private storage, is capped
  at 2 MB with one older copy, and goes when the app is uninstalled. It holds no
  passwords, keys or access codes, and no longer the relay address either: that
  is written as `<relay>` from the start, on the desktop too, so the one thing a
  report must never carry is not sitting in plain text in a file. A report sends
  the last stretch of it, cleaned as always, and you see it before it goes.
- **The chat entrance is only offered where there is a chat behind it.** Most
  stations never run a relay, and the chat lives on one. The Android tab used to
  be there for everyone — a third of the tab row leading nowhere. It now appears
  only for a station that goes through a relay, and the relay has to be switched
  on rather than merely filled in, because the chat needs the ticket the relay
  hands out on connecting. A relay that carries no chat is deliberately a
  different case: the entrance stays, and the screen behind it says why it is
  empty.
- **The chat, on Android.** The phone gets the same chat as the desktop: the
  consent screen, the conversation with times and replies, correcting your own
  typo, leaving with or without your messages, and the unread count on the tab
  so a message finds you rather than the other way round. It is one tab beside
  *Radio* and *Devices*, and while it is open the spectrum pauses like it does
  on the devices screen — the chat is the least important thing on that screen
  and stays that way. Reporting a problem works without joining, and the
  administrator's answer arrives in the same place. A report from the phone
  carries a log like the desktop's does — see *A log the phone keeps for
  itself* below.
- **A typo in the chat is yours to fix, for fifteen minutes.** An *edit* button
  on your own messages puts the text back in the input field; Send saves the
  correction, and every reader's copy is brought up to date with a *(edited)*
  marker on it. After fifteen minutes the conversation owns the text — others
  have answered what it said — and the button simply disappears rather than
  opening a field the service would refuse. Only your own messages, judged by
  station rather than by name, and a message whose author left cannot be edited
  by anyone.
- **A missing radio now says why, when the server can tell.** Windows gives a
  COM port to one program at a time, and other 991A/FTX-1 control software
  left running in the background is the single most common reason a radio
  "is not there" in ThetisLink. The server now recognises the two nameable
  failures when it tries to open the port — *in use by another program* and
  *does not exist* — shows them under the port selector in its own GUI, writes
  them in the log, and sends them with the radio presence so the desktop client
  shows the reason in the Devices tab instead of nothing. The presence packet
  gains two additive bytes for this; older clients and servers are unaffected
  in both directions.
- **Chat, in the server GUI and the desktop client.** One room for the users of a
  relay, behind a *Chat* button. Messages carry a time, and *reply* answers one
  message in particular, shown above the answer as a quoted line — one level deep,
  so a conversation stays a list you read from top to bottom. The chat hangs off
  the relay connection; without a relay there is nothing to see, and a chat that is
  down leaves audio, PTT, spectrum and the radio entirely alone.
- **The server is a station, not an accessory.** It gets the same window as the
  client rather than a smaller one: it runs beside Thetis and drives the radio
  whether or not anybody has a client open. Both draw the same component, so the
  two cannot drift apart, and the consent text exists once.
- **Report a problem.** Your description is the report — what you were doing, what
  you expected, what happened instead — and the log plus settings are an attachment
  with a tickbox. It is on by default, and nothing is read from your disk while it
  is off. You see exactly what will be sent before it goes.
- **Reporting works without joining the chat.** Joining means agreeing to a name
  others see; a report is a private line to the administrator. Somebody who wants no
  part of the conversation can still report a fault, and the answer still reaches
  them.
- **A report can carry the connected server's log as well.** A second tickbox, shown
  only when there is a server to ask. The server cleans its own files before a byte
  leaves and sends them in numbered parts, so an incomplete transfer says
  "163 of 171 parts" instead of attaching a shorter log that looks whole.
- **An answer comes back.** The administrator's reply appears at the top of the chat
  window, whether or not you joined the conversation. One way and short; for
  anything needing a conversation there is the e-mail address on the consent screen.
- **A postbox page for the administrator**, served by the chat container at
  `/chat/admin`: the list, one report at a time, an answer back, a download under a
  name you choose, and remove-when-collected. Same password as the relay
  administration, and arriving from that page needs no second login.

### Changed
- **The spectrum pan range now follows the span instead of a fixed factor.**
  One rule for RX1, RX2 and both VRX windows, with the tighter of two limits
  winning: never more than the window you are looking at (half a screen either
  way, so more zoom means less pan and the slider means the same thing at every
  zoom), and never past the edge of the spectrum there is (so below about 2x the
  pan cannot reach a whole window — there is nothing beyond the edge to pan to,
  and at 1x there is no pan at all). It replaces a flat five-percent factor
  unrelated to either span or zoom: at 8x it offered a fifth of what fits, at
  32x more than the window was wide.
- **The relay dashboard no longer calls a connected device "last seen two days
  ago".** "Last seen" freezes at session start, so a server that stays
  connected for days read as long gone while the connection counter said
  otherwise. A device with a live session now shows a green *connected* badge
  with "since ..." under it; the frozen time remains for devices that are
  actually gone.
- **Settings in a report are chosen by family rather than one by one.** 158 of 165
  client settings and 121 of 124 server settings now travel; the ones left behind
  are the password, the relay token and the relay URLs. An absolute veto on any name
  containing `password`, `secret`, `token`, `key`, `url`, `instance`, `credential`
  or `auth` is checked first and wins — every sensitive key matches a safe family on
  its prefix, so without it the families would have shipped the lot.
- **Addresses inside settings are scrubbed rather than withheld.** `tci=<ip>:40001`
  still says the link is configured and on which port, which is half of what
  explains a server problem.
- **The consent screen names the administrator** — who they are and where to reach
  them for access or removal — and asks for the age confirmation as a tickbox.
  Consent text version 2.

### Fixed
- **The phone's consent screen now warns about a callsign, as the desktop
  always did.** It asks for "callsign or nickname" and said nothing about a
  callsign appearing in a public register with name and address - so the one
  place that decision is made was the one place the warning was missing. The
  model behind the chat is shared between desktop and phone; the texts are two
  copies, and this is what that costs. A test now fails when a consent line
  exists on one platform and not the other.

  **Upgrading from 2.8.0:** a client older than this records its agreement
  against whatever version the service names, because it echoes that number
  back rather than sending its own. An agreement logged as version 3 by an
  older client was therefore given to the version 3 text without the removal
  clause. Anyone who agreed on 2.8.0 and wants to see the current text can
  leave the chat and rejoin.
- **A dropout sounds like the band again, on every channel.** Concealment — the
  noise that stands in for audio while a link hiccups — always ran through the
  narrowband decoder, so with wideband audio switched on it ran on a decoder
  that had never heard anything and produced silence. Error correction had the
  same fault, which is why it came and went: the server only switches error
  correction on when packets are being lost, and while it ran it fed that
  decoder enough history to conceal with. Each stream now decodes, corrects and
  conceals in its own format, and both radios, both VRX and RX2 conceal at all,
  which they never did. Opus itself is inaudible after about a quarter of a
  second, measured, so a longer gap is carried by generated noise at the
  stream's own noise floor, three decibels under it. Recordings made during a
  dropout are no longer shorter than what was heard; the level meters
  deliberately still ignore concealed audio, because that bar answers "is
  anything still arriving".
- **Audio survives a phone changing network.** Switching between WiFi and mobile
  data left the control channel working and the audio gone until the app was
  killed. Two faults, one on each side. In the app, a second relay monitor could
  start while the first was still running; both carry the same install id, the
  relay gives a returning client its own slot back and closes the older
  connection, and the two then evicted each other every five seconds for as long
  as the app ran. In the relay, the connection that dies after a network change
  revoked the audio capability by client id — and that id had just been handed
  to the connection replacing it, so the live client lost the key it had been
  issued twelve seconds earlier. Capabilities now belong to a connection, and a
  connection serial is never reused.
- **Receive audio no longer turns rough after the first transmission.** With the
  wideband option on, RX audio was clean from a fresh server and slightly rough
  from the first PTT onwards, until the server was restarted — a phone, which
  listens to the narrowband stream, never heard it, and switching the option off
  made it clean at once. The wideband resampler and encoder carry state across
  the pause a transmission makes and were never told the stream had stopped;
  they are now rebuilt when it resumes, which is what a server restart did
  minus the restart.
- **Transmitting no longer buys permanent RX audio latency.** Thetis pauses its
  receive audio while transmitting and hands over what it held in one burst
  afterwards. The mixer takes one frame per 20 ms tick and never catches up, so
  that burst stayed in the buffer for the rest of the session — measured at a
  station as one frame before transmitting, two after the first, five after the
  next, and never coming down. Each transmission cost twenty to eighty
  milliseconds that nothing gave back. After a pause the backlog is now dropped
  rather than carried, and the log says how much was dropped.
- **The VFO and filter markers now follow the pan.** They were placed at "the
  centre of the view minus the pan offset", which is the VFO only when the
  client knows the full span — and it only learns that once the full-band row
  has been switched on. Until then the offset was multiplied by zero, so the
  markers sat in the middle of the screen while the spectrum panned underneath
  them. That is also why switching the full-band row on once made the pan
  behave for the rest of the session. The markers are now the VFO itself,
  smoothed on the same clock as the view so the two still travel together while
  tuning.
- **The FT-991A power button works at once instead of a minute later.** A radio
  on standby keeps its CAT port alive — that is how it can be switched on at
  all — but answers nothing else. ThetisLink still ran its connect-time reads
  against it: 117 memory channels, 20 tones and 153 menu items, all into
  timeouts, well over a minute of them. A *power on* pressed during that minute
  waited in the queue behind it, and the radio duly woke up when the futile
  reading finished. Measured twice at a station: click at 22:57:40, radio on at
  22:59:01. Nothing is read from a radio that reports itself off now; the reads
  keep their turn and happen the moment it is on. Also stops the session
  starting with unreadable values recorded as zeroes.
- **A second report no longer carries the first one's server log.** The server's
  log was fetched when the tickbox was ticked and never again, so reopening the
  form with the box still on attached whatever had arrived last time — labelled
  "received" and looking perfectly fresh. A report about a fault could thus
  arrive with a server log from before the test, which is to say with no
  evidence in it. Opening the form now asks the server again, and Send waits
  for the answer as it already did.
- **An Android report now carries ThetisLink's own log rather than Android's.**
  The log was filtered to the app's process, which also returns everything the
  framework says about it — window, input method and renderer chatter, in such
  volume that the app's own lines were a rounding error. It is filtered to
  ThetisLink's own tags now, with crashes kept.
- **An open memory table no longer refuses to show what the server holds.** The
  client held back every list the server pushed while that table was open, not
  only while it had unsaved edits — so somebody watching the table, which is
  usually why it is open, never saw an update at all. A restored FTX-1 tone sat
  in the server's list for minutes while the client showed the old one. Unsaved
  edits still win over an incoming push; nothing else does.
- **A restored FTX-1 tone now also reaches the radio.** The kept tones came
  back into the list correctly, but the set had already been given the tone the
  list held a second earlier — read straight off the radio, which for this model
  is the 100.0 Hz it falls back to. The list said 77.0 and the radio transmitted
  100.0. The tone keeper now waits until the connect-time reads are in, and
  applies the list again as soon as the kept tones have been merged.
- **An FTX-1's memory tones now survive a restart.** That radio cannot store a
  CTCSS tone in a memory channel over CAT, so ThetisLink holds it in the
  server's list and applies it to the set when the channel is recalled — but
  that list lived in memory only, so a server restart read the radio again,
  found no tones, and the operator's work was gone. The tones are now kept in a
  small file beside the server's configuration, per slot and stamped with the
  radio model so a different radio in that slot cannot inherit them. The
  FT-991A is untouched: it stores its own tones, and there the radio remains
  the thing that knows.
- **A report carries more of what a diagnosis needs.** The log tail went from
  200 kB to 1 MB — the old size was set against a postbox limit that has since
  gone up, and a fault that started an hour earlier had already scrolled out of
  view. The server's own memory list for each radio travels with it too, which
  is the one thing a report could not show and precisely what a missing tone
  turns on.
- **A radio that took a moment to answer was dropped for the whole session.**
  The server GUI gave a radio five seconds to be found, while looking for one
  on a silent port can take six — a radio that is simply switched off. When it
  ran over, that slot was discarded and no client saw the radio again until a
  restart, *and* the half-built radio kept a thread running: opening the port,
  reading memories, writing convincing log lines about a radio nothing was
  connected to. The wait is now twenty seconds (it guards against a hung
  driver, not against a slow radio), an abandoned radio stops its threads, and
  the log says plainly that the slot is gone until a restart.
- **The problem-report form did not close after a successful send.** Nothing
  visibly happened, so the obvious thing to do was press Send again — and
  again. One reporter sent fifteen copies that way and then hit the day's
  limit, at which point reporting the actual fault was impossible. The form now
  closes on the service's confirmation and says the report was sent, as it did
  before the shared-model refactor lost that step.
- **The reporting limits are now stated before the work, not after it.** How
  many reports a station may still send today travels with the chat state, so
  the form says "none left today" when it opens instead of at the send button
  after a description has been written and a log read. The refusal is also
  shown *in* the form rather than only in the conversation behind it.
- **Those limits went up where they were too tight**: 100 reports per station
  per day (was 15 — one station has three front ends now), 4 MB per report (was
  512 kB, and a report carrying both a client and a server log passes that), 8
  MB at the request layer and at the TLS front (both were 1 MB), and a 200 MB
  postbox backlog (was 20 MB). A body over the ceiling now gets a *413* saying
  so and leaves a line in the service log; it used to be dropped without a word,
  which reads exactly like a relay that is down.
- **A radio that was off at server start could still end up in the wrong
  dialect.** The probe that reads `ID;` when the port finally opens asked once;
  the first read after opening a serial port routinely comes back empty, and
  when it did, an FTX-1 was driven as an FT-991A for the whole session — no
  memory channels, no menu values, V/M and Mem+/- dead. It now drains the port
  and asks up to three times, exactly as the startup detection has always done.
  Found through a problem report from the field, with the server log attached.
- **A pasted table no longer reads as a wall of empty boxes in the desktop
  chat.** The standard UI font covers little beyond latin-1, so line-drawing
  characters came out as tofu on the desktop while a phone showed the table
  fine. Someone else's message is not ours to write, so those characters are
  now drawn as the ASCII they came from (`|`, `-`, `+`), along with the curly
  quotes, dashes and ellipses a word processor leaves behind. Display only —
  what was sent is unchanged, and every other front end still shows the
  original.
- **A problem report from Android can carry the log and settings after all.**
  It said no log could travel, which was true of a file but not of the log
  itself: an app may read its own system log without any permission, and the
  settings sit in its own preferences. Both are now offered with a tickbox, on
  by default, cleaned by the same redaction the desktop uses and shown in full
  before sending — the preview is the safeguard, on a phone as much as anywhere.
- **The chat input grows with the message.** A longer message no longer scrolls
  out of its own single line; the field grows to about five rows (then scrolls),
  so everything can be read back before it goes. Enter still sends — a
  deliberate line break is Shift+Enter.
- **The Send button of the problem-report form no longer needs a window
  resize.** Inside a small chat window the form could spawn so small that the
  buttons fell off the bottom. Everything above the buttons now scrolls, and the
  button row stays in reach at any window size.
- **Switching the FT-991A on from standby now follows the ritual the radio
  demands.** Yaesu's CAT manual is explicit: power-ON needs dummy data first,
  then a pause of one to two seconds, then the command. ThetisLink sent the bare
  command, which a sleeping set ignores — the power button only worked after the
  radio had been switched on by hand once. Found by the second real user. The
  FTX-1 has no CAT power-on at all (its PS command only knows OFF); ThetisLink
  now says so in the log instead of sending a command the set cannot obey.
- **The relay address examples now show `wss://`.** Both the client and the
  server GUI hinted `ws://relay.example.com:18080` in the empty relay field —
  a form that cannot reach a relay behind TLS. The first user who copied it
  could not connect. It reads `wss://relay.example.com` now.
- **The PTT watchdog no longer takes the radio's word for whether we are asking
  to transmit.** Its timer was disarmed whenever the radio's `TX;` answer read
  "not transmitting" — and the FT-991A's `TX;` answer is known-unreliable, so a
  single spurious answer mid-transmission could silently switch off the
  time-out protection for the rest of that transmission, on exactly the radio
  that has no other net. Whether ThetisLink is asking for TX is something it
  knows without asking the radio, and that is what arms the timer now.
- **A radio that was off at server start is no longer driven with the wrong
  dialect all session.** A silent port is assumed to be an FT-991A; when the
  radio is switched on later and answers `ID;` with a model this build knows,
  the serial thread now adopts that radio's own dialect on the spot — memory
  channels, menu reads and the client's radio panel follow. Previously the
  mismatch was a single log warning and the assumption stuck until a server
  restart.
- **The unread counter on the Chat button can now actually appear.** New
  messages were only fetched while the chat window was open, and an open window
  clears the counter — so the number the design promised could never be seen.
  A closed window now checks once per half minute (members only); open windows
  keep the 3-second rhythm.
- **Building a server report no longer holds up audio and PTT.** Reading and
  cleaning the 200 kB log, and pacing its two hundred datagrams onto the wire,
  ran inside the same loop that carries transmit audio — a report requested
  during somebody's transmission stalled it audibly. The work now runs on its
  own thread; at most one at a time, as before.
- **Sending a report can no longer silently drop the server's log.** With the
  server-log box ticked and the transfer still underway, the Send button now
  waits (saying why) instead of sending a report without the half that was asked
  for. A transfer that failed does not block sending — the report itself then
  records that the log was requested and did not arrive.
- **"0 of 0 parts" now says what it means.** Asking the server for its log twice
  within 20 seconds is refused server-side by design; the client used to show a
  transfer failure with zero everywhere. It now says the server answers once per
  20 seconds and to try again shortly.
- **The reply-quote marker is now the ">" of quoted e-mail** instead of an arrow
  glyph the standard UI font cannot draw on every machine.
- **Agreeing to the consent text always looped back to "read it again".** The window
  showed consent text version 2, but the agreement was sent carrying version 1 — the
  number lived in a place the sending thread could not see — so the service refused
  it, the window asked the reader to start over, and no amount of reading ever got
  anyone into the chat. The version now travels with the agreement itself. Found by
  the first person who tried to join.
- **An unrecognised radio ID was called an FTX-1.** A station with a single FT-991A
  was driven all session as an FTX-1 — no memory channels, no menu values, the IF
  frame parsed as gibberish — because the first `ID;` after opening a port came back
  garbled and the guess stuck. The port is drained after opening, the ID is asked up
  to three times, and an unrecognised code is no longer a model.
- **The two radio slots disagreed about what to assume** when no radio answered:
  slot 0 said FT-991A and slot 1 said FTX-1, so the same fault behaved differently
  depending on which slot a radio sat in. Both assume the 991A dialect now, which is
  what the documentation always said.
- **`sdr-remote-theme` and `sdr-remote-layout` were never published.** Both have been
  listed as workspace members since they were created and neither was ever copied to
  the public repository, so a public clone could not resolve the workspace and could
  not build at all. The coherence check now reads the member list from the staged
  manifest instead of a hand-kept list.
- **The Android app is no longer readable over a USB cable.** Every release so far
  was built in a mode that let anyone with a cable read the app's private storage -
  which holds the relay token, the saved password and the log file. That mode is
  off. The signing key is unchanged, so this installs over an existing app as an
  ordinary update and nothing needs reinstalling. One consequence: reading the log
  with `adb run-as` no longer works. The log still travels with a problem report,
  which was always the route that mattered for a phone that is not plugged into
  anything.

---

## [2.8.0] — 2026-08-08 (Radio data ready the moment you connect · UI scale · arrangement memories · Android audio fixed)

> **Feature release.** The wire protocol stays **VERSION = 3** and there are **no new
> control ids**: what changed is the *value* on two existing ones, chosen so old and new
> peers keep working in both directions (see the Technical Reference). Stock Thetis
> v2.10.3.x remains sufficient; no Thetis-fork change is required. Desktop client, Windows
> server and Android client are all rebuilt.

### Fixed
- **Android: a Yaesu radio stayed silent.** Switching on *Yaesu active* mutes the Thetis
  audio, and the Android app did that by setting the **master volume** to zero — which since
  v2.7.0 also covers the Yaesu path, so the switch silenced the radio it had just switched
  on. The level meters kept moving throughout, because they are measured before the volume is
  applied, which made it look like a radio or squelch problem. Thetis is now silenced with
  the client-only RX volumes. **This affected the released v2.7.0 APK.**
- **Android: the memory list disappeared behind the EX menu.** Both travelled through one
  field, told apart by a prefix; now that the server pushes both on connect they arrived
  together and the second replaced the first. They have their own field now.
- **FTX-1: changing frequency on a memory channel snapped back a few seconds later.**
  The escape from memory to VFO sends `MA`, which only COPIES the channel into the VFO.
  The FT-991A leaves memory operation on that; the FTX-1 does not, so it kept working
  from the channel and put its own frequency back at its own pace while ThetisLink showed
  VFO. Leaving is a separate command there (`VM` with parameters, P1=0 MAIN P2=00 VFO).
  The **V/M button** shared this snap-back and is fixed with it - but see the entry
  below for the more serious fault it also had.
- **The V/M button overwrote a memory channel.** One click stored whatever the VFO
  happened to hold into the current memory channel, destroying what was there. It was
  wired to a bare `VM;`, which both CAT manuals give as a WRITE - *VFO-A to memory
  channel* on the FT-991A, *MAIN-side to memory channel* on the FTX-1 - and not as the
  toggle its label suggests. There was no confirmation and nothing in the log, so a
  channel could be gone without anyone noticing. **This affected every release since
  v2.0.0.**
  The toggle now leaves memory with `MA` (plus `VM000;` on an FTX-1) and enters it with
  a plain recall, and writes nothing at all. The parameterised `VM P1 P2P2` on the same
  manual page is the mode switch; the bare form is the write.
- **FTX-1: after transmitting FM from a memory channel you were left in VFO on the
  wrong frequency.** The mode restore after PTT-off recalls the channel, and that recall
  used the three-digit `MC` form, which this radio rejects. Nothing showed it: the
  transmission itself had worked and the failure was one silent command at the end.
  Every other FTX-1 path already used the five-digit form.
- **Coming back to the memory channel you had just left did not work** on either radio -
  you had to select a different one first. Clicking a channel recalled it *and* opened the
  row editor, so the row turned into a form and the one row you most wanted to click again
  had nothing left to click. Colour now says where the radio is rather than which row was
  last touched: **green** the radio is on this channel, **amber** the channel you left when
  you tuned into VFO, click it to come back. The whole row is the target, not the channel
  number alone; double-click opens the editor, which also gained a **Close** - it had no
  way out at all before.
- **The memory row offered two controls that led nowhere.** Its unlabelled "x" read as a
  close button but removed the row, and could not do more than that: a memory channel
  cannot be erased over CAT on either radio. And the **offset column was a dropdown of ten
  fixed values** matching neither radio, which the server never read back when writing -
  so every choice made there went nowhere. Neither radio stores a shift *amount* per
  channel; the record holds the direction and the size is a per-band menu setting (991A
  80-83, FTX-1 EX 010316-010319). The column is display-only now, derived from transmit
  minus receive frequency.
- **ThetisLink kept transmitting after the radio had stopped.** Set a TX time-out timer
  on the radio, hold PTT past it, and the radio drops back to receive while ThetisLink
  goes on showing TX and sending transmit audio - because nothing ever told it what the
  radio was doing. Two independent releases now cover it. The **FTX-1 reports its real
  transmit state** (`RI` P4, already in the 200 ms poll, so no extra CAT traffic), which
  catches any cause at all: the timer, a fault, or a hand on the set's own PTT. It waits
  for four consecutive answers so a single garbled frame can never cut a live
  transmission. And **both radios expose their TX time-out timer in the EX menu** (991A
  036, FTX-1 030112), read once when the radio connects, so ThetisLink stops 1.5 s before
  the radio does - first, so you hear a clean end rather than a cut. The FT-991A has no
  reliable transmit readback of its own, so the timer is its only net. Nothing was added
  to the PTT-on path; keying is as fast as it was.
- **The PTT latch stayed held after the radio let go.** The button went grey, because it
  follows the reported state, but the latch behind it did not: the next click only
  released a PTT that had already stopped, so it took a *second* click to key again. It
  now lets go on a confirmed transmitting -> not-transmitting edge, for both radios.
- **The EX menu dropdown silently dropped choices it could not parse.** The FT-991A TX
  time-out timer offered nothing but "OFF" and so could not be set from ThetisLink at
  all: its encoding is `00:OFF 01-30 min`, and the parser kept only the parts containing
  a colon, binning the range and 30 of the 31 settings with it. The same split cut any
  label at its first space, so `6:SKY BLUE` was offered as "SKY" - a colour the radio
  does not have. Six menus were affected: TX TOT (36) and WIRES DG-ID (153) lost a whole
  range; DISPLAY COLOR (6), CW FREQ DISPLAY (59), SPECTRUM COLOR (117) and WATERFALL
  COLOR (118) had truncated labels.
- **FTX-1: the tones in the list were replaced by 100.0 Hz on every connect.** The
  server reads the tones from the radio when it connects and merges them into the list.
  This radio cannot store a tone in a memory channel and reports 100.0 Hz for one that
  has none - which is exactly what the CAT memory-write command (`MW`) leaves behind on
  every channel it writes. So each connect replaced the real tones with the damage, in
  the very copy ThetisLink uses to put the tone back on the radio each time it lands on
  a channel (see the FTX-1 tone entry below). For the FTX-1 the list is the truth now: a read fills an empty
  tone (one set on the radio's own front panel does work) but never overwrites a value
  already there. The FT-991A stores tones properly and is unchanged - for it a read is
  the truth.
- **A client that dropped without warning and came straight back got nothing.** State
  is pushed once per subscriber and then only on change, with the recipient lists pruned
  against the connected clients. That works when a client leaves cleanly, because it is
  gone from that list - but a client that drops silently (a crash, a network blip, a
  kill) and returns on the same address inside the 15-second session timeout never left,
  so it stayed marked as already-served. A freshly started client then sat with an empty
  memory table and EX menu until the slow safety net came round, up to a minute later.
  Joining is now counted, so anyone arriving is served regardless of what the address
  list says.
- **FTX-1: the tones now work in practice, and writing memories asks first.** This radio
  cannot *store* a tone in a memory channel over CAT, and that is documented rather than
  merely observed: `MW` carries a field for the tone **mode** but none for the tone
  **number** (P9 is `00: (Fixed)`), and `MW` is set-only so there is nothing to read back.
  Writing a memory therefore also resets that channel's tone to 100.0 Hz **in the radio**,
  which is what was behind "the repeater stopped opening after a write". ThetisLink now
  keeps the tones in its own list and re-applies the right one every time the radio lands
  on a channel, so transmitting through ThetisLink works normally. The limit is stated
  plainly rather than glossed over: **this holds only while ThetisLink is connected** - the
  radio on its own will transmit 100.0 Hz on channels that were written. Because a write
  costs those tones, **FTX-1 memory writing is off by default** and refused until the
  condition is accepted in the server settings (Yaesu tab). Reading is never gated, and the
  FT-991A - which stores tones properly - is unaffected.
- **Android: the MIC meter stayed at -80 dB while transmitting.** The Yaesu TX level is
  measured in the shared engine but was never carried across to Android, and the Yaesu panel
  showed the Thetis capture level instead - which reads silence while a Yaesu is keyed. Audio
  went out and stations heard it; only the meter was blind. **This affected the released v2.7.0
  APK.**
- **Android: the Volume slider quietly attenuated the Yaesu.** That slider is the Thetis level
  in practice, since the Yaesu panel has its own, but it drove the master - which covers every
  path. A setting that is right for Thetis then left a Yaesu barely audible while its own
  slider read full open. The two levels are stored separately now and the slider reloads when
  you switch, so it shows the level it is actually controlling.
- **Android: FTX-1 EX settings showed a bare number.** The name lookup read the FTX-1's
  six-digit address as an FT-991A menu number and always missed. A table of 440 labels was
  added, generated from the chart the desktop already uses.
- **The tones no longer disappear from the memory list.** They are not part of the bulk read,
  so every fresh read produced a list with empty tone columns and replaced the good one —
  pressing *Read radio* lost them just as effectively. A read now carries over the tones
  already known, but only for channels whose frequency is unchanged.
- **FT-991A: the memory read no longer comes up one channel short.** The CTCSS pre-read's
  answer could land in the first memory query, after which every channel received the
  previous channel's answer and the last was never collected — a list one channel short with
  silently shifted data.
- **The master volume no longer snaps back to 100 %** (desktop), and a saved window layout is
  restored correctly on a display that is not at 100 % scale.

### Added
- **The radio's data is there the moment you connect.** Memory channels, tones and EX
  settings are read **once when the radio connects** and kept by the server; every client is
  then served from that copy instead of making the radio walk its channels again. That walk
  took about a second on the FT-991A and several on the FTX-1 (405 EX values), during which
  no other CAT command could get through. *Consequence:* a channel or setting changed **on
  the radio itself** is noticed after pressing **Read radio**, which always fetches fresh.
- **UI scale, 50 %–150 %, in both the client and the server GUI.** It scales the contents of
  the windows, not their position, so a saved layout survives a change of scale. `Ctrl +` /
  `Ctrl -` do the same and are remembered.
- **Arrangement memories.** Five slots that hold a whole layout: *Store* records where every
  open window is, *Recall* puts them all back and reopens a window that was closed. Real
  positions are stored rather than the grid, so a layout fine-tuned by hand comes back as you
  left it. A recall made before a radio is connected completes itself when that radio
  appears. The painted matrix now survives a restart as well, and the grid goes up to 18×18.
- Both memory lists **start empty** instead of loading a saved file, so what is on screen
  demonstrably came from the radio. *Load file* still loads a saved list on request.

### Changed
- **One push mechanism for every kind of state.** State that used to be re-sent whenever the
  *number* of clients changed is now tracked per client, so a client arriving while another
  leaves can no longer be missed. Everything gains a slow full resend as a safety net,
  because a push is not guaranteed to arrive: the small state every 10 s, the larger blocks
  every 60 s. Recorded in full in the project's internal design note.
- The window arranger is **one implementation shared by the client and the server GUI**
  instead of two copies that had drifted apart; the server GUI gains the UI scale, the 18×18
  grid and the arrangement memories it lacked.

## [2.7.0] — 2026-08-07 (VRX audio reliability · Yaesu CTCSS/DCS from the client · rebuilt audio-level meters · shared full-band spectrum row)

> **Feature release.** The wire protocol stays **VERSION = 3**: everything added here is
> **additive** (three new control ids), so a v2.7.0 client keeps working with a v2.6.x server
> and vice versa — the new options simply stay inactive on an older peer. Stock Thetis
> v2.10.3.x remains sufficient; no Thetis-fork change is required. Desktop client, Windows
> server and Android client are all rebuilt.

### Added
- **CTCSS and DCS can be set from the client.** A memory channel's tone can now be read from
  the radio and written back to it, for the FT-991A. *Read tones* walks the memory channels that
  actually have a tone mode and fills the list; a scan is paused for the walk and resumed
  afterwards, and the radio returns to the channel it was on. DCS codes work the same way as
  CTCSS tones (all 104 codes).
  **FTX-1 limitation:** that radio exposes no safe CAT route for storing a tone — the write path
  is deliberately disabled there rather than left to corrupt a memory channel. Reading works.
- **Optional full-band spectrum row, shared by RX and VRX.** A checkbox in the *Server* tab
  ("Full-band spectrum row per (V)RX chain") controls the extra full-DDC row that sits underneath
  the zoomed view. It is what keeps a waterfall's history filled after tuning or zooming. One row
  per receiver chain now serves both the RX window and the VRX riding that same DDC, instead of
  only the RX window — so a VRX waterfall no longer shows gaps after retuning, and the case where
  the row was missing entirely (VRX on, RX spectrum off) is covered. Switching it off roughly
  halves the spectrum bandwidth per chain; every waterfall then follows its own view.
- **Thetis can be started automatically when the client launches** (desktop and Android). If
  Thetis is not running yet, the server starts it on request rather than leaving the client on a
  connect error.
- **Several ThetisLink clients on one PC.** Named profiles keep settings, window positions and
  audio devices apart; a bare profile name is accepted as well as `--profile`. A single-instance
  guard stops a second client from silently fighting the first over the connection, audio and
  spectrum — it now reports "already running" and exits.
- **The client log carries timestamps.** Anyone who sends a log in for support notices it at once,
  and it makes a client log comparable with the server log line by line.
- **The Amplitec power-limit table now lives on the server**, editable in the Amplitec window
  under port B — configuration belongs with the hardware. The client shows it read-only.

### Changed
- **The audio-level bars measure the link, not the volume slider.** They are taken *before* the
  volume is applied, so a channel that is simply turned down no longer reads as a dead stream.
  Every bar also falls back to zero within about half a second once its stream stops, instead of
  freezing on its last value — which is what makes the bar usable for spotting a stream that is
  not arriving at all.
- **The Yaesu receive path is calibrated per radio model.** The two USB CODECs deliver line
  levels about 14 dB apart; both the meter and the playback gain now use the same per-model
  constant. **After the upgrade the FT-991A sounds about 16 dB quieter — set its volume higher than
  you are used to.** It used to run that much hot, which was audible at the very bottom of the
  volume slider where every other channel had already gone quiet.
- **The channel controls say what they do.** Each channel is a block with the channel name as a
  heading and two buttons under it, *audio* and *venster* (window) — previously the channel name
  itself was the audio switch, above two different words ("spec", "win") for the same window
  toggle. The audio switch also appears inside every channel window, so a channel can be muted
  from either place.
- **The "VRX" button next to *Arrange* is gone.** It toggled both VRX windows at once from a
  third place; every channel now has its own *venster* button in its own block, so the shortcut
  was a second way to do the same thing — and the one that said least about what it would do.
- **The master volume is really a master.** It applies to every playback channel (RX1, RX2, both
  VRX channels and both Yaesu slots) instead of RX only, and it no longer changes identity into
  the VFO A volume depending on which windows are open. RX1 gained its own `VFO A:` slider in the
  Radio tab for that (not to be confused with `RX1 Vol:` on the Thetis tab, which drives Thetis's
  own volume). With a single audio channel the slider is simply labelled *Volume*.
  **On upgrade:** the master used to affect RX only. If it was not at maximum, VRX and Yaesu audio
  is now quieter than you are used to — on the FT-991A that lands on top of the recalibration
  above, so check that channel first.
- **VRX tuning follows the configured step.** The band edge is derived from hardware widths and
  therefore lands on an arbitrary frequency; stepping into it now stops on the last point that is
  still on the step grid, instead of parking the readout on a stray remainder. The scroll step is
  a fixed 1 kHz like RX, where it used to shrink with the zoom factor.
- **The connect error names Thetis**, not "the radio" — with a Yaesu attached, "radio not
  connected" pointed at the wrong thing.
- **The server GUI is translated** (English / Nederlands / Deutsch / Français). A handful of
  status and error strings are still Dutch.

### Fixed
- **VRX audio now starts reliably and stays clean after retuning.** Three independent causes sat
  under one symptom: a stale DDC centre could keep a channel permanently distorted after a missed
  push (the centre is re-read periodically now); the relay's duplicate filter ignored the channel
  id, so VRX1 and VRX2 ate each other's frames; and a channel that was switched off and on again
  restarted its sequence, which the same filter read as already-delivered — the wait before audio
  appeared was exactly as long as the previous listening session.
- **A VRX stays inside the DDC band, and the spectrum shows the edge honestly.** Tuning stops
  where the audio stops, and the spectrum draws the requested window with empty bins outside the
  DDC instead of silently rescaling — so the display no longer suggests signal beyond what can be
  heard.
- **Silence is no longer mistaken for a broken UDP path**, and a resumed audio stream plays
  immediately instead of waiting out its old sequence.
- **Yaesu memory handling.** The client shows what the radio actually reports instead of
  inventing fields; irrelevant fields for the current mode are shown as dashes; clicking a memory
  recalls it on the radio whose list was clicked (the FTX-1 needs a different recall form);
  writing while a scan is running now works; and an FTX-1 channel is no longer re-stored after
  setting its tone, which used to overwrite the channel with the current VFO.
- **A MIDI wheel no longer runs a Yaesu away.** Every tick used to become its own CAT write over
  the serial link, with a round trip each; turning faster than the radio could keep up filled a
  queue that went on stepping for minutes after the knob had stopped — and blocked other CAT
  traffic while it drained. Only the last frequency of each pass is sent now, the same
  coalescing the Thetis VFO already had. Requires the new client: the coalescing sits there, so a
  v2.6.x client against a v2.7.0 server keeps the old behaviour.
- **A VRX runtime is created at the rate its filter calls for**, instead of being built narrowband
  and immediately torn down again on the first frame.
- **Windows and layout.** Yaesu pop-outs reopen at their saved position after a reconnect and are
  gated on radio presence rather than on the audio flag; the RX1 inline/pop-out choice and the
  Yaesu chip toggles survive a restart; split geometry returns correctly after a join.
- **No more empty RX1 spectrum window in a Yaesu-only setup.** The window is gated on Thetis
  being configured, so a setup without it no longer opens a window with nothing in it.
- **The VRX window choice survives a restart.** It is derived from the high-resolution-spectrum
  setting instead of starting closed every time.
- **RX2 spectrum peak-hold decays at the same rate as RX1** (shared constant).

### Internal
- The refactor track continued: the per-channel spectrum model now carries RX1 and RX2 as well as
  the VRX channels, with one shared auto-reference derivation for all four; the Yaesu memory blob
  has a single owner; the server's control dispatch has no catch-all left, so the compiler forces
  every control to be handled; the Yaesu server code and the server GUI were split out of their
  god-files; and every pop-out goes through one shared lifecycle helper.
- A hardware-free audio harness pins six measurement properties (levels before volume, VRX
  parity, per-model Yaesu meter *and* playback calibration, TX peak after gain, and every channel
  falling back to zero when its stream stops), so a future unification cannot silently undo an
  operator-measured correction.
- The design rule for where a control belongs is written down: the core is a task — RX1 receiving
  and transmitting — not a device class.

---

## [2.6.0] — 2026-07-31 (Window-arranger matrix (client + server) · multilingual server GUI · analog s-meter rework · Yaesu memory & standby fixes)

> **Feature release.** All changes are **client- or server-local** (UI, layout and CAT-timing
> logic) — there is **no wire-protocol change**. The protocol stays **VERSION = 3** and is fully
> backward-compatible with v2.5.x, so a v2.6.0 client and an older server (or vice versa) keep
> working. Stock Thetis v2.10.3.x is sufficient (no Thetis-fork change required). The desktop
> client, the Windows server and the Android client are all rebuilt; the Android change is the
> peak-hold tick on the audio-level bars — no protocol impact.

### Added
- **"Arrange windows" is now a matrix placer (up to 12×12).** The *Schik* panel was rebuilt: pick a
  grid size with a drag-picker, select a window and **paint it onto the cells** — a single window may
  span several adjacent cells and is stretched over that block. **Drag from the first to the last
  cell to fill a whole rectangle in one gesture** (with a live preview). The main window can now be
  arranged too, and the palette lists every server-available window (a window that is off is opened
  automatically when you apply). Per monitor its own grid.
- **The window-arranger is now also in the server GUI.** A *Schik* button (Running mode) arranges the
  server's own device windows — main window plus tuner, Amplitec, SPE, RF2K-S, UltraBeam and rotor —
  over a grid (up to 12×12), with the same rectangle-drag selection and multi-monitor support. The
  palette only lists backends that are actually running.
- **The server GUI is now multilingual (English / Nederlands / Deutsch / Français).** A language
  picker in *Settings* switches the interface live and remembers the choice. The whole Settings
  screen and the device pop-outs (tuner, Amplitec, SPE, UltraBeam, rotor, macro editor) are
  translated; product names, ham abbreviations and units stay as-is. Default is Dutch. *(The desktop
  and Android clients were already multilingual since v2.5.0.)*

### Changed
- **Analog s-meter reworked for a constant, more readable shape.** The meter now keeps a **fixed
  width/height ratio** regardless of window size (it scales uniformly instead of distorting), uses
  only the arc zone (the empty bottom is dropped), sits tightly around the scale so **"1" and "+60"
  are just fully visible**, and grows larger for RX/VRX when there is room — filling the full row
  height instead of staying under-sized.
- **Peak-hold on the audio-level bars.** Each audio-level bar (and the Android meters) now shows a
  brief max-hold tick (~1.5 s) and the dB read-out follows the held peak.
- **EQ on/off is shown next to the Equalizer chevron** (FT-991A + FTX-1), so the equaliser state is
  visible without expanding the section — matching the Android layout.

### Fixed
- **Turning the FT-991A on from the client is no longer delayed ~30–40 s.** When the radio was in
  standby, the memory auto-read on connect ground through all 117 channels (each timing out) and
  blocked the CAT thread, so a *power-on* click only took effect after the read finished. The read
  now aborts within ~1 s once the radio is detected as not responding, so power-on is processed
  immediately; a non-responding read no longer clears the shown memory list either.
- **FT-991A memory is read reliably and no longer shows a stale count.** The memory auto-read fires on
  radio detection (independent of the audio-enable), so with audio off it shows the radio (23) rather
  than the loaded file (24).
- **FTX-1 memory reads reliably and auto-loads on USB detection**, matching the 991A.
- **Collapsible sections remember their state across restart.** The RX-bandwidth, Amplitec power-cap
  and FTX-1 memory/settings chevrons now persist open/closed, so startup matches how you left it.
- **RX1 "Auto ref" now sticks.** A config save during a TX-spectrum override no longer writes the
  temporary TX default, so the auto-reference toggle keeps the setting you chose.

## [2.5.0] — 2026-07-30 (Independent channels · VRX windows + window-arranger · per-channel s-meter · Yaesu power & per-band TX power · Yaesu mode-gating + memory→VFO · multilingual (EN/NL/DE/FR) · Android parity)

> **Feature release.** Several **additive, backward-compatible** wire fields were added
> (`tx_power_max`, `Rx1Enable`, `YaesuStateEnable`, `YaesuPowerOnOff`, and a `SINGLE_RECEIVER`
> state flag) —
> old clients ignore them and the new client falls back gracefully against an old server, so
> there is no protocol break. The wire protocol stays **VERSION = 3** and is
> backward-compatible with v2.4.x. The later additions in this release (multilingual UI,
> Yaesu control gating, memory→VFO escape, and reading the 991A clarifier offset back) are
> client- or server-local or reuse existing fields, so they add **no further wire change**.
> The server-side features below need the 2.5.0 **server**; the
> desktop and Android clients are both updated. Stock Thetis v2.10.3.x is sufficient (no
> Thetis-fork change required for these features).
>
> **Relay packaging.** The relay ships as a **source + Docker-Compose bundle**
> (`thetislink-relay-source.tar.gz`, a checksummed release asset in `SHA256SUMS.txt`) —
> **not** a prebuilt binary or container image. Self-host it with
> `docker compose up -d --build`. It is only needed for internet remote access behind
> CGNAT / without a port-forward; a direct (LAN or port-forward) connection does not use it.

### Added
- **Every channel now has its own independent audio and spectrum switches.** RX1, RX2, VRX1 and
  VRX2 each get a separate audio-tick and spectrum-toggle, so you can run any combination — for
  example only a VRX spectrum with no RX audio, to save bandwidth. A VRX is now a fully standalone
  channel with its **own spectrum and s-meter**, no longer tied to whether its parent RX is on.
- **VRX1 and VRX2 are separate, independently-placeable windows** (previously one combined window).
- **"Arrange windows" drag-grid.** A new *Schik* button opens a panel where you drag the open
  spectrum/Yaesu windows into rows and snap them onto the screen in one click (side-by-side,
  stacked, 2×2, 2-plus-1, …). Layouts are **per monitor**: pick a screen, arrange its windows,
  apply everything at once.
- **Per-channel s-meter style.** Click any s-meter to switch it between the analog arc and the bar;
  the choice is remembered per channel (RX1/RX2/VRX1/VRX2/Yaesu 1/Yaesu 2). The separate
  "S: Analog/Bar" button is gone.
- **Yaesu radios on the main screen.** Each connected Yaesu shows a compact chip (audio on/off +
  open-window button) at the top, so you no longer have to go to Devices to enable it.
- **Yaesu audio can be muted while the control window stays live.** The audio subscription and the
  radio-state subscription are now separate server-side, so muting a Yaesu keeps its frequency,
  s-meter and CAT updating in the open window (and stops the audio bandwidth).
- **Yaesu power control.** The FT-991A gets a clickable **Power / Standby** button (CAT `PS` —
  standby keeps the USB/CAT alive so the radio can be woken remotely). The FTX-1 shows the power
  state as a label only, because it powers off completely (USB drops, so it cannot be woken
  remotely).
- **Yaesu TX-power slider now matches the radio's real per-band maximum.** The server reads the
  FT-991A's max-power menus (EX137–140) and the slider runs 5 W…max for the current band (e.g. 50 W
  on 2 m/70 cm, your EX value on HF). The FTX-1 follows the same per-band limits per head.
- **"Single receiver" server setting.** For radios with only one receiver you can switch RX2 off in
  the server; clients then hide RX2 and VRX2 everywhere.
- **Consistent "Yaesu N: type" naming** (e.g. *Yaesu 1: 991A*) across the whole UI — main screen,
  device panels, window titles, and the server-status tab.
- **SWR alarm.** A red **HIGH SWR** indicator plus an audible warning tone when the radio reports a
  high SWR during transmit — on both the desktop and Android.
- **The server settings window now follows the same light/dark theme** as the client.
- **Android:** when no Thetis is configured, the Radio tab shows the Yaesu view directly (with the
  "Yaesu active" toggle re-labelled as the audio on/off), and the FT-991A gets the same
  Power/Standby button as the desktop. The Yaesu **EQ sliders are now half-height** (more compact
  panel), and you can **recall a memory channel by tapping it** in the list. The EQ sliders and the
  memory-channel list are **collapsible behind a chevron** (so you don't nudge them while scrolling),
  and the app now **exits cleanly** when swiped away.
- **The apps are now multilingual.** The interface is **English by default**, with **Dutch, German
  and French** translations. The Android app follows your phone's language automatically (falling
  back to English if it is set to another language); the desktop client has a **language picker** in
  the server tab. Ham-radio terminology, mode names and product names are deliberately left
  untranslated.
- **Changing frequency on a Yaesu in memory mode now slides it to VFO.** When you tune an FT-991A
  or FTX-1 that is sitting on a memory channel, the radio copies that channel to VFO-A, keeps its
  mode, and follows your new frequency — so you spin off a memory channel seamlessly instead of the
  tune being ignored.

### Changed
- **The server-status tab is now dynamic:** it shows only the audio levels, streams, recording
  options and data-usage of the components that are actually active, instead of a fixed list.
- **The channel chips moved to their own row** (RX1 aligned under the VFO-A volume) so the main
  window can be made narrower.
- **Yaesu controls now grey out when they don't apply to the current band or mode.** Verified
  against the FT-991A operating manual: IPO/ATT/ATU are HF–50 MHz only, BK-IN/APF are CW-only,
  Contour is greyed in CW, and so on — so you can see at a glance what the radio will accept. The
  FT-991A's **CW/CW-R modes are now labelled CW-L/CW-U** to match the sideband.

### Fixed
- A series of coupling bugs where a channel's **spectrum secretly depended on its audio** being on:
  a VRX spectrum showing empty without VRX audio, the loss metric jumping to 100 % on a VRX-only
  setup (which then dropped the spectrum), RX2 audio disappearing when RX1 audio was turned off,
  the RX2 spectrum centring on the wrong frequency without RX2 audio, and RX2 audio volume being
  muted by the pop-out. Spectrum-without-audio now works correctly for every channel.
- **Pop-out window positions now restore reliably** after snapping, closing and reopening
  (RX1/RX2/VRX/Yaesu windows) — previously some close paths lost the saved position.
- **The Yaesu TX-power slider no longer bounces** after you release it (it now waits for the radio
  to confirm your value before following the read-back).
- Stale **HIGH SWR** is cleared when a radio disconnects.
- **The client now follows the CLAR (clarifier) knob on the FT-991A itself.** The 991A does not
  report its clarifier offset directly, so it is now read from the IF status — turning the physical
  CLAR knob (including in memory mode) is reflected in the client's RIT/XIT display.
- **A transient network reset no longer stops the server.** `recv_from` errors such as
  ConnectionReset/Aborted — common on Windows when a client drops — are handled instead of being
  fatal.
- **Audio error-recovery now rebuilds the resampler**, fixing a case where audio could stay broken
  after a device glitch until a restart.



## [2.4.4] — 2026-07-17 (WebSDR per radio · Yaesu Mem+/Mem- empty-skip · spacebar-PTT in Yaesu windows · no phantom Thetis-PTT)

> **Patch release.** No wire-protocol change (`VERSION` stays 3, fully interoperable with
> v2.4.x). Stock Thetis v2.10.3.15 is sufficient — no Thetis-fork change. Desktop updated;
> Android functionally unchanged, APK rebuilt at 2.4.4.

### Added
- **The WebSDR selection is now remembered per radio.** Thetis, the FT-991A and the FTX-1 each
  keep their own WebSDR URL (and highlighted favourite), so choosing a WebSDR for one radio no
  longer changes it for the others. The favourites list stays a shared pool, and the single
  embedded window shows the WebSDR of whichever radio you open it for. Existing configs migrate:
  the old single URL applies to all three at first, then they can be set apart.

### Fixed
- **Yaesu Mem+ / Mem- now skips empty memory channels.** Previously it stepped the channel
  number by ±1 and got stuck on a gap (the FT-991A rejects a recall of an empty channel). It now
  jumps to the next/previous **filled** channel (using the list from the last memory read, with
  wrap-around), so one click always moves one populated channel — on the FT-991A and the FTX-1.
- **The spacebar now keys the Yaesu radio when its pop-out window has focus.** The radio-1/radio-2
  windows are separate viewports with their own keyboard input, so the main-window spacebar-PTT
  handler never saw them. Spacebar in a radio window now keys that radio (combined with the mouse
  PTT, respecting the TX-in-use lock and PTT spike-protection).
- **The main-window PTT button no longer lights up when no Thetis is configured.** With no Thetis
  present the Thetis PTT keys nothing, so the button now stays grey/disabled and the
  spacebar/mouse/MIDI can no longer turn it red.

### Changed
- **Deleting a memory row now shows a short popup** instead of a permanent line above the table,
  making clear the row is removed from the local list only. A memory channel cannot be erased
  from the radio over CAT — that is only possible on the radio's own front panel (hold
  F(M-LIST), select the channel, tap ERASE).

## [2.4.3] — 2026-07-17 (Relay connection clarity · client colour-coding · slider mouse-wheel)

> **Patch release.** No wire-protocol change (`VERSION` stays 3, fully interoperable with
> v2.4.x). Stock Thetis v2.10.3.15 is sufficient — no Thetis-fork change. Desktop and Android
> both updated; the APK is rebuilt at 2.4.3.

### Changed
- **When connected through the relay, the connection area no longer shows the direct server
  IP** (which is not the actual route — the relay decides the destination via station/token,
  so the IP was misleading). It now shows **"Via relay: &lt;station&gt;"** plus the live relay
  status on desktop, and the relay destination on Android. Direct connections are unchanged.
- **The server's client list colour-codes each client by connection type** — direct clients in
  ThetisLink blue, relayed clients in cyan (with a "(relay)" tag and a small legend). The amber
  authenticating/stale cue still takes precedence.

### Added
- **Mouse-wheel scroll on every desktop slider.** Hovering a slider and scrolling now nudges it
  by one step per notch (volumes/gains/squelch/RIT-XIT/CW/drive/monitor, diversity gain/phase,
  the VRX and RX1/RX2 spectrum ref/range/zoom/pan/waterfall controls, and all Yaesu 1 & 2
  controls). Disabled read-out sliders do not scroll.
- **A restart notice when toggling the relay on _or_ off** (previously only shown when turning
  it on) — desktop and the Android settings page — so it is clear the change takes effect after
  a restart.

### Notes
- The documentation now states explicitly that **the server supports both connection methods at
  the same time**: with a relay configured, each client independently chooses direct or relay,
  so one server can serve a mix of direct and relayed clients concurrently. The installation
  overview now shows both methods with their own diagram.

## [2.4.2] — 2026-07-16 (Recorded-audio playback to the radio: clean modulation · Thetis TX-EQ bypass · play-volume)

> **Patch release.** One additive wire-protocol addition only (a new client→server control
> `ThetisTxeq = 0x90`); `VERSION` stays 3, so a direct connection stays fully interoperable
> with v2.4.0 / v2.4.1 (an older peer simply ignores the new control). Stock Thetis v2.10.3.15
> is sufficient — no Thetis-fork change. Android is functionally unchanged; the APK is rebuilt
> at 2.4.2.

### Fixed
- **Recorded audio transmitted through the radio was overmodulated / distorted.** Playback to
  the transmitter (TX inject) ran through the live-microphone processing chain — 5-band EQ,
  compressor, AGC and the 4× mic-gain boost — so an already line-level recording clipped.
  Playback now bypasses the mic chain entirely for Thetis and both Yaesu radios: the recording
  goes out clean at line level (only the play-volume scaling is applied). Live-microphone
  transmit is unchanged.
- **Playback to the 2nd Yaesu (FTX-1 / radio 2) did not come through.** The WAV-TX inject path
  was missing the radio-2 PTT case, so a recording only reached the main radio and the first
  Yaesu.
- **RX audio was muted on all receivers during transmit.** Pressing PTT on one receiver
  silenced the audio of every other receiver in that client. RX audio now stays audible during
  TX; the internal-speaker mute (to suppress the PTT plop) applies only when PTT
  spike-protection is enabled.
- **State was not reset after a playback ended or on disconnect.** The live microphone could
  stay mic-chain-bypassed after a recording finished, and Thetis TX-EQ could be left off after
  a manual disconnect or app shutdown mid-playback. Both are now reset / restored on teardown.

### Added
- **Thetis TX-EQ is bypassed automatically while a recording is played to the main radio,** and
  restored afterwards — mirroring Thetis's own record/playback behaviour, so the transmit
  profile's EQ does not colour the recording. The server reads the operator's actual TX-EQ
  setting first and restores that exact state (falls back to on if it cannot read it).
- **Play-volume slider** next to Play/Stop (0–2×) to trim the level of a recording sent to the
  transmitter.
- **Transmit level meter during playback** — the audio bar shows the level of the audio
  actually being transmitted while a recording plays, instead of the (muted) microphone.

### Notes
- Recordings at a sample rate other than 8 kHz or 16 kHz (only possible with an externally
  imported WAV — ThetisLink records only 8/16 kHz) are now refused with a clear log message
  instead of being played back at the wrong speed.

## [2.4.1] — 2026-07-16 (Rotor link screen · Settings visibility · recorded-audio TX playback rate · FT-991A memory 100+)

> **Patch release.** Bug fixes only, no protocol or feature changes. Fully interoperable
> with v2.4.0; wire protocol `VERSION` stays 3. Stock Thetis v2.10.3.15 is sufficient.

### Fixed
- **Rotor (MCP2221A) could not be linked from a clean config.** The MCP2221A section had no
  "link an existing board" step for the rotor — tuners already had one — so an
  already-programmed `rot_` board could not be attached to the rotor slot from a fresh
  `thetislink-server.conf`; it stayed amber even though the scan detected the board. A link
  row (MCP-serial dropdown) now writes the rotor entry and selects the MCP2221A rotor backend
  in one action; "Add board → Rotor" also sets the backend.
- **Settings button disappeared after an MCP2221A scan.** The status/scan list could push the
  bottom Settings button out of view (the panel itself does not scroll), forcing an Exit +
  restart. The bottom controls now reserve the correct height, so Exit and Settings stay
  visible regardless of the list length.
- **Recorded audio played back too slowly / stuttering through the radio.** Playback to the
  transmitter (TX inject) ignored the recording's sample rate: a 16 kHz recording played at
  half speed on the main radio, and any recording played 3–6× too slow through a Yaesu.
  The TX-inject path is now sample-rate-aware (speaker playback was already correct).
  Recording itself was already correct for every channel (RX1/RX2/Yaesu 1-2/VRX1-2).
- **FT-991A memory channels 100–117 were not read.** The memory read stopped at channel 099,
  so the FT-991A PMS channels (100–117, i.e. P-1L…P-9U) never appeared in the memory editor —
  a channel stored at 100+ went missing. The read now covers the full CAT range 001–117 (the
  write side already accepted it). The FTX-1 is unaffected: its regular memory is 001–099 and
  its PMS uses a different (`P-01L`) addressing, so that path stays at 099.

## [2.4.0] — 2026-07-15 (Relay v2 UDP audio + auto TCP fallback · Yaesu SSB-over-USB & TX processing · full Android Yaesu parity · expanded monitoring · desktop themes)

> **A broad release.** Beyond the relay, this version brings remote **SSB transmit over
> the Yaesu USB audio**, client-side **TX compressor/AGC** and **clarifier** for the Yaesu,
> a large **Android Yaesu parity** push (dual-radio, full DSP panel, touch tuning, internal
> ATU), **mobile data-saving**, and a much **expanded connection monitor**.
>
> **Compatibility.** The TL wire-protocol `VERSION` stays 3 (additive), so a direct
> (LAN / port-forward) connection is fully backwards-compatible. The **relay** path is a
> coordinated upgrade: the relay, server and client (and Android) should all run 2.4.0
> together; a mixed setup keeps working but stays on the reliable wss/TCP audio path.
> Stock Thetis v2.10.3.15 is sufficient; no Thetis-fork change.

### Relay v2 — low-latency audio through the VPS relay
- **Audio + PTT over UDP.** Relayed audio now travels over bare UDP (no retransmit /
  head-of-line blocking) instead of wss/TCP, matching the direct-connection latency feel.
  Control and spectrum stay on the encrypted wss channel.
- **Automatic UDP↔wss fallback (make-before-break).** When a network blocks or degrades
  UDP, the audio auto-falls-back to the reliable wss/TCP path within ~2 s and returns to
  UDP once it recovers (with hysteresis so it never flaps). During transmit the audio is
  sent over both paths, so PTT is never lost. A small **transport indicator** ("UDP" /
  "TCP fallback") shows which path is active — on desktop and Android. There can be a
  brief (~0.5–1.5 s) gap only on a sudden total UDP loss; gradual degradation is seamless.
- **UDP capability-token rotation.** The per-session UDP token has a bounded TTL and is
  rotated over wss before it expires, so an active session never drops and no token
  outlives its lifetime. A choice of UDP (low latency) vs wss (encrypted) audio is exposed
  as a setting on desktop, server and Android.
- **Admin dashboard (web).** Manage stations and devices (block / rename), see per-device
  and per-station monthly usage, set data/device/client quotas, and download a one-click
  consistent **database backup**. Argon2id login, CSRF protection, per-IP login rate-limit;
  the API is internal-only behind the TLS reverse proxy.

### Yaesu — SSB transmit over the USB audio
- **SSB over the USB CODEC.** The Yaesu radios now transmit **SSB** using the streamed USB
  microphone audio, not just FM/DATA. On the **FT-991A** this switches SSB MIC SELECT=REAR
  + PORT SELECT=USB per PTT (restored on release); the **FTX-1** uses its own internal
  automatic modulation source, which ThetisLink leaves untouched. Previously SSB TX
  required the local hand mic.
- **FM auto-DATA unchanged; SSB does not use DATA.** The automatic FM → DATA-FM switch (for
  USB-mic FM TX) is unchanged. SSB stays in the normal SSB mode with the REAR/USB routing
  above — the earlier SSB → DATA-LSB/USB approach was reverted (carrier offset + narrow
  data filters are unsuitable for speech).
- **Hybrid per-PTT routing + Exit.** SSB USB-routing is applied per-PTT by default and
  restored between overs (presence-based restore in opt-out mode); an explicit **Exit**
  button returns the radio to its fixed MIC/DATA baseline. The TX-audio output is retried
  until the USB CODEC device is free.

### Yaesu — TX audio processing & clarifier
- **Client-side compressor + AGC, per radio.** A transmit compressor and an AGC toggle run
  in the client audio engine for the Yaesu TX branch (desktop and Android), independently
  per radio, alongside the existing per-radio TX EQ. Includes FTX-1 TX-mute and
  volume-restore fixes and a clean **AGC cycle** (FAST → MID → SLOW → AUTO).
- **Clarifier (RIT/XIT).** Clarifier control for both radios — RIT/XIT enable, offset
  steps and clear.

### Android — Yaesu parity
- **Dual-radio on Android.** A radio 1 / radio 2 selector brings the second Yaesu to the
  Android client, with per-radio PTT/volume routing.
- **Full DSP panel.** Collapsible DSP controls (ATT/AGC/NB/NR/IPO/Contour/APF/Notch/Proc/
  AMC) with adjustable sliders.
- **Touch frequency tuning.** A large tappable digit tuner plus a stepper for direct
  on-screen tuning of the Yaesu.
- **Internal ATU** (Tune + ATU on/off) and the **clarifier** (RIT/XIT + offset) on Android.

### Mobile data-saving
- **Yaesu-only clients skip the Thetis RX stream.** A client that only listens to a Yaesu
  radio no longer receives the Thetis RX audio, cutting mobile data.
- **On-demand Yaesu streaming.** Yaesu data is streamed only while its window is open/
  active (short spectrum grace on resume); a dynamic presence-push keeps a single Yaesu
  stream alive when shared.

### WebSDR
- **Reload button** for a quick recovery of the embedded WebSDR/KiwiSDR view after a
  network interruption.

### Desktop theming
- **Selectable UI themes** — Classic (light), Dark, Slate — plus a **Custom** theme with
  live colour pickers for background, widgets, text and the slider knob. Applied
  immediately and persisted.

### Connection monitoring
- **Greatly expanded per-stream statistics.** The Statistics panel now reports every audio
  stream separately — Thetis RX, Yaesu 1/2 and VRX1/VRX2 — each with its own jitter,
  jitter-buffer depth and packet count (and loss where the transport exposes it), alongside
  RTT and up/down bandwidth. The Down (RX) figure expands into a per-packet-type breakdown
  of the recent window, and the relay transport ("UDP" / "TCP fallback") is shown inline.
  On both desktop and Android.
- **Server-side bandwidth breakdown.** The server Status panel shows per-client down/up
  bandwidth, split into audio / spectrum / other, so it is clear where the traffic goes.

### Robustness fixes
- **Main window self-heal.** A window position saved on a since-disconnected or rearranged
  second monitor no longer opens off-screen — it falls back to the primary monitor.
- **Spectrum toggle no longer cuts RX1 audio** for clients that also have a Yaesu radio
  configured.
- **Jitter-buffer recovery.** The per-stream jitter buffers re-baseline on a large backward
  sequence jump (stream restart) and reset cleanly on disconnect, avoiding stuck audio.
- **Spectrum safety net.** A conservative default `max_bins` (2048) guards against an
  oversized spectrum request; the Android spectrum grace/`max_bins` handshake is honoured
  on connect and resume.

### Compliance / hygiene
- Refreshed dependency **SBOM** + **THIRD-PARTY-LICENSES** bundle; all licenses verified
  GPL-2.0-or-later compatible. Source line-endings normalized to LF; internal development
  markers removed from product source.

---

## [2.3.0] — 2026-06-27 (Synchronous AM (SAM-PLL) + AM auto-tune + TX modulation bandwidth)

> **Backwards-compatible with 2.1.x / 2.2.0.** Wire-protocol `VERSION` stays 3 —
> the new VRX-AFC and TX-filter packet/control types are purely additive
> (`0x2A`/`0x2B`, control `0x75`–`0x79`) and are sent only to clients that
> support them, so older clients keep working. Stock Thetis (v2.10.3.14+) is
> sufficient for the TX-bandwidth feature; no Thetis-fork update is required for
> this release. The Android client is unchanged this release (it has no VRX);
> the bundled APK is rebuilt at 2.3.0. Download `ThetisLink-2.3.0.zip` from the
> [Releases page](https://github.com/cjenschede/ThetisLink/releases) — the ZIP
> contains both Windows binaries, the Android APK, the PDF manuals, `LICENSE`
> and `SHA256SUMS.txt`. SBOM and third-party license artefacts are attached to
> the same release as separate assets.

### Added — Synchronous AM with a carrier-tracking PLL (SAM)

The VRX **SAM** mode is now a real synchronous-AM demodulator: a
critically-damped (ζ=1.0) carrier-tracking PLL locks onto the AM carrier and
demodulates against the recovered phase, mirroring Thetis/WDSP `amd.c`. This
removes the beat-note of the previous pseudo-SAM when the tuning is a few Hz
off and stays clean through selective fading. Capture range ±3 kHz.

### Added — AM auto-tune-to-carrier (AFC) + per-VRX audio rate

In SAM with auto-tune enabled, the listen frequency continuously follows the AM
carrier onto exact zero-beat (the client VFO follows). The tracker is a
two-speed, noise-robust AFC (fast pull-in when far out, slow ~2 s tracking near
the carrier, 5 Hz deadband) that holds a strong/wide carrier without hunting and
preserves the lock across an NB↔WB audio-rate rebuild. Each VRX gets its own
audio-rate selector — **NB (8 kHz) / WB (16 kHz) / Auto** — independently per
channel; Auto widens to 16 kHz when the filter is opened past ~4 kHz.

### Added — Settable TX modulation bandwidth (desktop, Thetis tab)

The main-radio TX modulation bandwidth is now adjustable from the desktop
client's Thetis tab: **Follow RX bandwidth** (TX mirrors the RX filter 1:1,
manual fields greyed) or independent **Low/High** edges. Range 0–8 kHz (TX audio
is 16 kS/s, so the audio passband tops out at 8 kHz; a wider RX filter is flagged
and clamped). In symmetric modes (AM/SAM/DSB/FM) the RX spectrum filter edges now
mirror, so dragging one edge moves both sides — matching how Thetis enforces a
symmetric filter.

### Fixed

- During PTT, mode changes are no longer forwarded to Thetis — works around a
  Thetis desync where a mode change mid-transmit updated the indicator but not
  the actual mode.
- **Follow RX bandwidth** is now available immediately on connect: the server
  reads the TX filter band at TCI connect (`tx_filter_band_ex`) instead of only
  learning it when Thetis first changes it, so a server restart is no longer
  needed for the feature to work.
- **Pop-out windows on a disconnected monitor** are recovered automatically: the
  client validates each saved pop-out position against the live monitor layout
  (Windows) and opens off-screen windows on the primary monitor instead. A manual
  **"Recenter windows"** button (Server tab) is also available.
- AFC handoff is clamp-aware at the ±3 kHz capture edge (no offset double-count
  or drift).

## [2.2.0] — 2026-06-18 (Virtual receivers + dual-radio FT-991A/FTX-1)

> **Backwards-compatible with 2.1.x.** Wire-protocol `VERSION` stays 3 — the new
> VRX and second-radio packet types are purely additive (`0x21`–`0x29`) and are
> sent only to clients that explicitly subscribe, so a v2.1.x client keeps working
> and never receives the new types. Pair with **Thetis fork PA3GHM TL2-4** for the
> full feature-set; stock Thetis remains supported. Download
> `ThetisLink-2.2.0.zip` from the
> [Releases page](https://github.com/cjenschede/ThetisLink/releases) — the ZIP
> contains both Windows binaries, the Android APK, all PDF manuals, `LICENSE` and
> `SHA256SUMS.txt`. SBOM and third-party license artefacts are attached to the
> same release as separate download assets.

### Added — Virtual receivers (VRX)

Two independent **virtual receivers** — VRX1 on RX1/VFO-A and VRX2 on RX2/VFO-B —
are carved out of the wideband DDC I/Q stream by an FFT channelizer (new
`vrx-rs` crate). Each VRX has its own listen frequency, mode (USB/LSB/AM/SAM/FM),
filter, high-resolution spectrum + waterfall and S-meter, shown together in a
joint pop-out window and mixed into the main audio alongside RX1/RX2/Yaesu.
Audio is Opus narrowband (8 kHz) or wideband (16 kHz). Per-DDC-bucket frequency
memory and full state persistence (enable/frequency/mode/filter) across
reconnects.

A browser-readable, illustrated explanation of the whole VRX signal chain — from
radio wave to sound — is published on GitHub Pages:
**[How a VRX works](https://cjenschede.github.io/ThetisLink/VRX-explained.html)**
(English) · **[Hoe een VRX werkt](https://cjenschede.github.io/ThetisLink/VRX-uitleg.html)**
(Nederlands), with a companion document on the server → client network path.

### Added — Second radio (FT-991A + FTX-1, dual-radio)

A second Yaesu radio can run alongside the first as an **independent channel**
(slot 1), each with its own CAT serial port, USB audio, frequency, mode, PTT and
memory. The radio model is **auto-detected** from the CAT `ID;` response
(`0670` = FT-991A, `0840` = FTX-1); a bring-up probe logs a warning if the
detected model does not match the configured slot (possible USB-enumeration
swap). New additive packet types carry the slot-1 audio/state/frequency/memory,
plus a `RadioInfo` broadcast so dual-radio-aware clients label the panels
correctly. The Yaesu **FTX-1 WIRES-X** EX-menu fields are added to the EX editor.
Two identically-named `USB Audio CODEC` devices can be disambiguated with a
**`#N` index suffix** in the audio-device selector.

### Added — FTX-1 software squelch

The FTX-1's hardware squelch does not gate its USB audio, so an FM channel
streams noise continuously. A **server-side software squelch** now polls the
radio's busy state (`RI`) and fades the audio to silence when the squelch is
closed — **FM-family modes only**; SSB/CW/AM/data always pass through (where the
busy flag is meaningless).

### Added — Switchable radio RX bandwidth + dynamic recording rate

One client switch now sets the **RX audio bandwidth** (narrow 8 kHz / wide
16 kHz) for the Thetis receiver, the VRX channels and the connected Yaesu radios
together (receive only; transmit stays wideband). WAV recording sample-rate
auto-scales with that setting.

### Fixed — VRX traffic isolation for older clients

VRX audio (`AudioVrx`) and high-resolution VRX spectrum (`SpectrumVrx1/2`) are
now gated by **per-client subscription** (mirroring the second-radio gate), so a
v2.1.x client that never enables VRX receives none of the new packet types — no
parse errors, no log-spam, no wasted bandwidth. The FM demodulator's phase
discriminator was corrected to use a full-quadrant `atan2`.

## [2.1.1] — 2026-06-07 (PstRotator + Log4OM direct rotor control)

> **Backwards-compatible with 2.1.0.** Wire-protocol unchanged. Adds a
> parallel UDP+TCP listener on the server so PstRotator (any mode) or
> Log4OM (via PstRotator-emulation) can command the active rotor backend
> directly. Existing TCI-client rotor control is unchanged.

### Added — PstRotator listener (parallel input source)

The server now opens a combined UDP + TCP listener (default port
12001, configurable via `pstrotator_listen_enabled` / `pstrotator_listen_port`
in `thetislink-server.conf`) that accepts rotor commands from PstRotator
or any PstRotator-compatible application. Commands are routed through
the active rotor backend (EA7HG, PstRotator-outgoing, or Adafruit
MCP2221A) — so PstRotator can drive a G-1000DXC connected via the
Adafruit breakout without any intermediate hardware.

Supported protocol formats (auto-detected per packet):

- **Yaesu GS-232A / GS-232B** (text): `M<nnn>\r` (goto), `S\r` (stop),
  `C\r` (query → `+<nnn>\r`), `C2\r` (query → `+0aaa+0eee\r`)
- **Prosistel binary / EA7HG**: `\x02AG<nnn>\r` or `AAG<nnn>\r` (goto),
  `\x02A?\r` or `AA?\r` (query → `\x02A,?,<nnn>,<R|B>\r`),
  `\x02AG999\r` or `AAR\r` (stop)
- **PstRotator native XML**: `<PST><AZIMUTH>nnn.n</AZIMUTH></PST>`
  (goto), `<PST>AZ?</PST>` (query → `AZ:<nnn.n>\r`),
  `<PST><STOP>1</STOP></PST>` (stop)
- **AZ-text broadcast**: `AZ:nnn.n\r` (PstRotator's simulator output,
  treated as feedback within 30 s of a real goto to avoid override)

The TCP path also pushes TL2-originated targets back to PstRotator
(`M<nnn>\r` or `\x02AG<nnn>\r` depending on detected protocol), so
PstRotator's compass shows the same target indicator regardless of
which side initiated the move. **Note:** PstRotator's client-mode UI
may not visualise externally-pushed targets — this is a protocol-side
limitation of GS-232A / Prosistel, not a TL2 issue.

### Added — Log4OM direct (PstRotator-emulation)

Log4OM does not natively support `rotctld` or other generic rotor
protocols — its only rotor option is PstRotator. To drive the rotor
without a PstRotator instance running, point Log4OM's PstRotator
settings at the TL2 server:

1. In Log4OM: **Settings → External Services → PstRotator** (or
   equivalent rotator-control panel)
2. Set **Host** to the TL2 server's IP (e.g. `192.168.1.97`) — change
   from `localhost` / `127.0.0.1`
3. Set **Port** to TL2's PstRotator listener port (default `12001`)
4. Stop PstRotator on the Win4OM PC if it is running (no longer needed)

Log4OM now sends `<PST><AZIMUTH>nnn</AZIMUTH></PST>` directly to TL2.
TL2 acts as a drop-in PstRotator replacement. Metadata tags Log4OM
also sends (`<CALL>`, `<NAME>`, `<QTH>`, `<FREQUENCY>`, `<MODE>`,
`<GRID>`, `<COMMENT>`, `<COUNTRY>`, `<CONTINENT>`) are silently
ignored — no parse-fail warnings in the server log.

### Fixed — Rotor target oscillation when PstRotator simulator broadcasts AZ:nn

When PstRotator's "UDP output" was enabled in parallel with the
EA7HG-UDP controller, PstRotator's internal rotor simulator
broadcast `AZ:nn\r` packets ~1 Hz that the listener interpreted as
new goto commands. Each simulator step pulled the rotor to that
position, causing visible stepwise oscillation. The listener now
classifies AZ-broadcasts that arrive within 30 s of a real
`AAG`/`M` goto as simulator-feedback and silently drops them.
AZ-broadcasts outside that window continue to work as goto commands
for AZ-only PstRotator output configurations.

### Changed — Server-log volume

The high-frequency raw-packet log lines (`PstRotator listen RX from
...`) are now emitted at `debug!` level instead of `info!`. The
default-level log shows only actionable rotor events
(`compass X° → mech Y°` on a real goto, connect/disconnect, parse
warnings on truly unknown packets). Use `RUST_LOG=debug` to restore
the full RX visibility for diagnostics.

### Fixed — Rotor direction-reversal ramp protection

When a running GoTo received a new target on the opposite side of the
compass (delta sign flip), the Adafruit MCP2221A backend previously
flipped the CW/CCW gates while the DAC was still at full speed,
causing the motor to slam from full-power one direction to full-power
the other. The poll-tick now detects a direction mismatch between the
desired rotation and the active gate; while `current_dac` is above
the dead-band it leaves the gates alone and ramps the DAC down to
zero first. Once stopped, the gates switch to the new direction and
the normal soft-start ramps back up. Existing `ramp_pct_per_sec`
controls both phases — no separate reversal rate.

### Fixed — Yaesu FT-991A memory write

"Write radio" from the FT-991A memory-edit window previously reported
success in the server log but the radio silently rejected the writes,
and a follow-up "Read radio" surfaced the unchanged state. Three
underlying issues, all addressed:

- **UDP packet-reorder race.** The client sends the tab-text data
  (~2.7 kB, IP-fragmented) and the write-trigger control (8 B) in
  quick succession. The control routinely overtook the data on the
  wire, so the server saw a trigger without data and dropped it. A
  latch on the trigger now fires the write when the data arrives,
  regardless of order.
- **MT frame P9 violation.** The CTCSS-tone index was emitted in the
  P9 field where the FT-991A spec requires literal `"00"`. Any
  channel with `Tone ENC` (or any non-default tone-mode) was silently
  rejected. P9 is now hard-coded to `"00"`; the per-channel
  CTCSS-tone *frequency* is no longer transmitted via MT.
- **FM force-mapped to DATA-FM on storage.** All `FM` / `FM-N` /
  `DATA-FM` / `C4FM` channels were stored as DATA-FM, leaving the
  radio in DATA-FM after every Write-radio cycle (USB-mic only,
  no local mic). The mode-mapping now round-trips correctly: `FM`
  stays `FM`, `FM-N` stays `FM-N`, `AM-N` stays `AM-N`, `C4FM` stays
  `C4FM`. The runtime FM ↔ DATA-FM swap during remote PTT
  (`set_ptt()`) is unchanged.

**Note:** the per-channel CTCSS *frequency* is no longer written via
MT — only the tone-mode (on/off, ENC/DCS) propagates. Set the CTCSS
frequency from the radio's front-panel menu (or wait for a follow-up
patch that drives the dedicated `CN` command). Tone-mode aan/uit
works as expected.

---

## [2.1.0] — 2026-06 (Yaesu rotor MCP2221A backend, wideband Thetis RX, Amplitec reliability)

> **Backwards-compatible with 2.0.4.** Wire-protocol unchanged — a
> 2.0.4 client talks to a 2.1.0 server (and vice versa) without
> issues. The new rotor backend, wideband RX opt-in and Amplitec
> reconnect logic are all server-side; clients see them through the
> existing TCI/Rotor/Amplitec channels. Pair 2.1.0 with the matching
> Thetis-fork build **PA3GHM TL2-4** to unlock the full feature-set;
> stock Thetis remains supported via the standard fallback paths.

### Added — Yaesu G-1000DXC rotor via Adafruit MCP2221A

A third rotor backend joins the existing **EA7HG** and **PstRotator**
options, driving a Yaesu G-1000DXC's EXT CONTROL port directly from
a Adafruit MCP2221A breakout (5 V mod) without any intermediate
controller PCB or third-party software. The on-board MCP2221A speaks
GPIO (CW/CCW gates via BST82 low-side switches), DAC (speed) and ADC
(position feedback) over USB-HID; ThetisLink does all the control
logic in-process.

- **Soft-start / soft-stop ramp** — configurable acceleration
  (`ramp_pct_per_sec`, 1–200 %/s, default 50 %/s) protects heavy mast
  hardware. The GoTo soft-stop landing computes a deceleration
  distance from the current speed + ramp-rate and reaches the target
  within ±1° without overshoot.
- **Adaptive sample rate** — ADC polled at 30 Hz during motion
  (33 ms tick, intentionally off the 50/60 Hz mains-ripple multiples)
  with a 10-sample median filter for control loop responsiveness, and
  at 1 Hz when idle with a 60-sample median for a calm position
  display.
- **Shortest-route option** for rotors with overlap range
  (`max_deg > 360`): with the checkbox enabled, a GoTo from e.g. 350°
  to 30° picks the 40° CW path through the overlap zone instead of
  the 320° CCW path through the dead band.
- **Manual override** — the server-UI's CW/CCW test buttons and
  speed-slider take precedence over the ramp loop while you debug
  hardware; the ramp resumes control when the next client GoTo
  arrives.
- **Calibration wizard** — "Park CCW" / "Park CW" buttons capture the
  Yaesu position-pin voltage at the mechanical endpoints; the linear
  mapping survives the slightly above-spec voltage range some
  G-1000DXC units exhibit (up to ~7.5 V on pin 4, well above the
  schema-documented 4.5 V).

The client side stays unchanged: the existing Rotor window (compass
circle, GoTo input, Stop button) drives the new backend through the
same `Rotor` facade as EA7HG and PstRotator.

### Added — Optional wideband Thetis RX audio

A new server checkbox **"ThetisLink extensions WB RX"** lifts the
fixed 48 kHz RX-audio rate when paired with a Thetis-fork that
supports the wideband-IQ extension. Owners with capable network and
desktop hardware can now stream RX at the wider rate without giving
up the standard fallback for stock Thetis. Default off — the existing
narrow-band path is unchanged.

### Added — Modular multi-tuner wizard

The server's MCP2221A tuner-bridge section now supports multiple tuner
slots driven from a `Vec<TunerConfig>` schema. Each slot is added via
a board-scan wizard that classifies detected USB devices (Tuner vs
Rotor vs Unprogrammed) and writes the chosen function to the board's
EEPROM. Per-slot rename, delete and threshold-slider; the surrounding
**MCP2221A** section is now collapsible and its expanded/collapsed
state persists across restarts.

### Fixed — Amplitec reconnect after power cycle

The Amplitec 6/2 serial worker thread no longer dies on the first
USB-error. It loops with a 5-second retry, marks the device as
disconnected during the outage and reconnects automatically when the
controller comes back online. The Amplitec window now also appears
even when the device is offline at server start — previously a missed
COM-port at boot made the whole UI section invisible until a server
restart.

### Fixed — RX2 mode-switch filter restore

Switching RX2 to a mode the client had never seen (USB → CW for
example) restored the filter edges from the new mode's defaults
instead of carrying over the obsolete previous mode's filter. A
one-line guard in the client's modulation handler honours the
server's filter-band update during the switch instead of overwriting
it with stale state.

### Fixed — RX2 spectrum filter-drag isolation

Per-channel filter-edge drag keys decoupled the RX1 and RX2 drag
state, so a filter-edge drag on RX2 no longer pulls RX1's filter
along by accident.

### Fixed — Yaesu EQ profile mic-gain persistence

The Yaesu FT-991A equalizer profile now saves the mic-gain slider
together with the band/treble levels; switching profiles or
restarting the client preserves the slider value.

### Fixed — Yaesu TX resampler aliasing

Sharper anti-alias filter on the client's Yaesu TX audio resampler;
high-frequency artefacts in the transmitted audio are reduced.

### Fixed — Server status panel scroll-jump

The Status panel's "Active clients" and "Recent connect attempts"
sections briefly shrank to a 1-line "snapshot busy…" placeholder
whenever the SessionManager lock was contended. The lost rows pulled
the scrollable content above any expanded section underneath, so
scrolling down to inspect the MCP2221A panel kept jumping back up.
Snapshots are now cached and reused on contention; the layout stays
stable across renders.

### Fixed — Graceful server auto-restart

Auto-restart now runs the hardware-Arc Drop handlers before the new
process spawns, releasing cpal audio streams and the TCI WebSocket
cleanly. Audio on the new instance works on the first try instead of
requiring a manual stop+start cycle.

### Fixed — UltraBeam element-lengths at connect

Initial element-length read on connect; the UI no longer briefly
shows zeros for the first ~300 ms after the UltraBeam controller
appears on the network.

### Fixed — Yaesu audio cold-start fail-soft

Yaesu output stream retry-loop + de-duplicated retry logs prevent the
"audio device disappeared at boot" failure mode where the server
gave up after one attempt; the first poll after the device enumerates
correctly now succeeds quietly.

### Changed — UI polish across server and client

- All `CollapsingHeader` widgets replaced by a custom `chevron_label`
  with a geometric triangle marker, ASCII-only to avoid the egui
  default font's missing-glyph tofu on some Windows setups
- Server Settings tab now wrapped in a `ScrollArea` so the panel
  stays usable on smaller displays
- Amplitec antenna-button two-line layout with prominent alias label;
  rename via right-click context menu; auto-scale buttons on long
  names
- Client frequency-digit hover blocks the parent `ScrollArea` so
  mouse-wheel digit edits no longer scroll the surrounding panel
- Rotor poll-thread log noise demoted to `debug!` (per-tick
  `set_direction` and 5-second ADC stats) — no longer floods the
  default server log

### Compliance

- New driver module `mcp2221_yaesu_rotor.rs` carries an
  `SPDX-License-Identifier: GPL-2.0-or-later` header. The vendored
  `mcp2221-hal` crate keeps its original MIT/Apache-2 dual license
  alongside ThetisLink's GPL-2.0-or-later distribution as before.
- SBOM (`compliance/sbom.spdx.json`) and third-party-licenses bundle
  regenerated for v2.1.0.
- No new third-party crate additions on top of v2.0.4 beyond what the
  workspace already depended on.

### Hardware reference — Yaesu G-1000DXC + MCP2221A

For owners building the same setup: the rotor printje uses a
**1.8 kΩ + 2.2 kΩ** divider (ratio 1.818) on the position-feedback
pin, mapping the 0–4.8 V (or higher, depending on unit) Yaesu output
into the MCP2221A's internal 4.096 V ADC reference with a safe
margin. The initial 1.8 kΩ + 10 kΩ design (ratio 1.18) clipped above
~365° on some units; rebuild with 2.2 kΩ if you observe ADC
saturation past the 365° mark. A 10 µF cap parallel to the 2.2 kΩ
suppresses the 100 Hz mains-rectifier ripple visible on the position
signal. Recalibrate with **Park CCW** + **Park CW** after any divider
change.

---

## [2.0.4] — 2026-05 (bandwidth toolkit, preventive TX-inhibit, power-cap, PstRotator)

> **Backwards-compatible with 2.0.3.** Wire-protocol additive only —
> one new control ID (`DxSpotsEnabled`); older clients ignore it,
> older servers default the new behaviour to ON. Mix v2.0.3 and
> v2.0.4 freely while you roll out, but pair v2.0.4 with the
> matching Thetis-fork build to unlock the full feature-set.

### Added — Preventive RX-only TX-inhibit (Thetis-fork TL2-3 required)

A new chokepoint between ThetisLink and Thetis stops the radio from
transmitting on an antenna position marked as **RX-only**, before TX
can briefly come up. When the fork-side ThetisLink extensions are
enabled, ThetisLink drives Thetis' "Receive only" flag directly via
the new `rx_only_ex` TCI command — MOX, spacebar, hardware-PTT and
VOX are all refused at the source instead of being flipped back
reactively. The reactive ZZTX0 catch-all remains the safety floor
for stock Thetis (no fork extensions) and for any path the
preventive gate cannot reach.

- Server-side state machine handles takeover, level-maintain and
  release, including a bootstrap-stale clear so a leftover
  `RXOnly=true` from a previous session is wiped within ~1 ms after
  the cap is detected.
- The Thetis-fork `RXOnly` setter now broadcasts a TCI push-notify on
  every real transition, so external Setup → "Receive only" toggles
  are visible to ThetisLink in real time (was: only on TCI SET/GET
  echoes, which left ThetisLink with a stale cache).
- Server-side dedup on the `rx_only_ex` notification keeps the log
  clean when fork-broadcast and handler-echo arrive together.

Requires Thetis fork **PA3GHM TL2-3** or newer for the full
preventive path. Stock Thetis falls back to the reactive ZZTX0
catch-all without any user action.

### Added — Reactive RF power-cap per antenna position

Per-Amplitec-A position the server enforces a maximum forward-power
(`amplitec_max_w`) by sending the PA's own `DriveDown` button (SPE
Expert or RF2K-S) — not ZZPC, which the PA pushes back through the
TCI loop. Mode-multipliers are applied universally: SSB/CW × 1.0,
AM × 0.5, FM/DIG × 0.4. A counter remembers how many DriveDowns
were sent on the active position and restores them as DriveUps when
the user switches to a different position.

- New first-class GUI editor in the Amplitec tab (6 rows × max W
  + TX-blocked checkbox) replaces the previous file-edit workflow.
- Rate-limited to one DriveDown per second to let the PA-meter
  settle; brief CW-bursts under that interval may pass the cap
  (reactive only — preventive coverage exists on RX-only positions).
- Tuner first-config: the server UI now shows tuner slots without
  requiring an existing instance, breaking the catch-22 for new
  installs.

### Added — PstRotator UDP/XML rotor backend

Native PstRotator support alongside the existing rotctl-TCP backend.
Per-installation choice via `rotor_backend = pstrotator` in the
server config. Integer-degree AZIMUTH commands; AZ/EL replies parsed
fallible; offline-timeout marks status `false` cleanly. Host field
is a **numeric IP address** — no DNS resolution. mDNS troubleshooting
notes added to the manuals.

### Added — Editable WebSDR favorite names

Favorites in the WebSDR list now have an explicit Edit-toggle so a
rename commits on Done / loss of focus and survives reconnect.

### Added — Server-tab bandwidth monitor + DX-spots opt-out

The desktop Server tab now shows the live UDP bandwidth in both
directions:

- **Down (RX)** and **Up (TX)** in Kbit/s, updated every 500 ms.
- Click on Down to expand a per-stream breakdown (audio, spectrum,
  S-meter, DX-spots, …) refreshed every 5 s.
- A **DX spots ontvangen** checkbox lets you opt out of the DX-cluster
  spot stream on metered links. The Android client has the same
  switch in Settings.

The monitor counts UDP application-payload bytes; the operating-
system network meter typically reads 1.5–2× higher because it
includes IP/UDP/Ethernet headers. The Android DX-spots toggle
resets to ON when the app restarts (no preference persistence).

### Fixed — DX-cluster spot broadcast storm (~90 Kbit/s → ~6 Kbit/s)

The server used to re-send all cached DX-cluster spots to every
client on every equipment-tick (5 Hz). With ~100 spots in cache this
consumed ~90 Kbit/s steady-state on each client. The broadcast now
sends only new spots per tick and triggers a full age-refresh every
10 s — about 15× less data without a user-visible change.

### Fixed — Server log spam

Two periodic log sources became state-change-driven:

- `PowerCap state` only logs when `(pos, mode, pa_in_operate, cap)`
  transitions — used to fire every 2 s regardless. PA-meter fluctuations
  are intentionally excluded from the snapshot so they don't reintroduce
  the spam.
- `DX Cluster` reconnect now emits one line per failure plus one
  line on recovery (`reconnected after N failed attempts`) instead of
  the previous three lines per backoff cycle.

### Notes for upgraders

- A v2.0.3 client connects to a v2.0.4 server without problems; the
  new `DxSpotsEnabled` control is simply unused, default = ON.
- A v2.0.4 client connects to a v2.0.3 server without problems; the
  opt-out toggle is harmless (server ignores the unknown control)
  but you cannot turn off the spot stream on the older server.
- The new preventive TX-inhibit only activates when paired with a
  **Thetis fork build PA3GHM TL2-3 or newer**. Stock Thetis remains
  fully supported via the reactive ZZTX0 fallback that already
  existed in v2.0.3.

---

## [2.0.3] — 2026-05 (multi-tuner + wire-protocol breaking change)

> **Breaking change — wire-protocol version bumped from 2 → 3.**
> A v2.0.3 server and a v2.0.2 client (or vice versa) are not compatible:
> the S-meter payload layout was rearranged to support multi-source
> subscriptions (Sig peak-hold, Avg true-mean, MaxBin) and an S9-frequency
> band-shift. Mismatched pairs are detected in the handshake and the user
> sees a localised `ProtocolVersionMismatch` modal ("Server is too old" /
> "Client is too old") instead of garbled audio or a silent connect failure.
> Upgrade server, desktop client and Android client together.

### Added — multi-tuner runtime via Adafruit MCP2221A USB-HID

- Up to **two physical StockCorner JC-4s / JC-3s tuners in parallel**, each
  driven through its own MCP2221A breakout (replaces the v2.0.2 serial-port
  RTS/CTS flow). JC-4s and JC-3s share the same control protocol; the model
  alias is cosmetic.
- **Per-tuner status panel rows** with: connection state, MCP serial
  dropdown, Amplitec-A position binding, live yellow-wire voltage,
  threshold slider (0.5 V – 4.5 V, default 2.25 V), hysteresis slider
  (0.1 V – 2.0 V, default 0.50 V), and the derived `active < … V` /
  `idle > … V` edge display. An amber **⚠ clamped** warning appears when
  the slider combination falls outside the physically reachable yellow
  range so the user sees that the configuration would never trigger.
- **USB board scan + "Program serial"** UI: identify anonymous boards by
  HID path, give each one a unique serial that survives a USB-replug.
- **USB auto-reconnect** (5 s retry interval) for tuner bridges that drop
  the link after first connecting — no server restart needed to recover
  from a cable replug or hub reset.
- **Collapsible MCP2221A section** at the bottom of the status panel with
  its open/closed state persisted across server restarts
  (`mcp2221_section_expanded` config key).

### Added — S-meter and TCI sensor layer

- Multi-source S-meter subscription via the `rx_channel_sensors_ex` TCI
  payload: peak-hold ("Sig"), true-mean ("Avg"), and the single highest
  FFT-bin in the passband ("MaxBin") are all cached server-side; clients
  pick the source they want via `SmeterSource`.
- **S9-frequency band shift** (HF vs VHF/UHF S-meter scale) honours the
  Thetis-fork-provided crossover frequency (`s9_frequency_ex`); falls
  back to 50 MHz against stock Thetis.
- FWD-power continues to update during TX with Sig/MaxBin subscription
  active (previous bundle showed zero forward power when the client
  switched away from the Avg source).

### Added — CTUN, MIDI, Spectrum

- **CTUN coupled-recenter mirror**: server now syncs the second RX
  spectrum so rapid VFO-A tuning does not leave RX2 lagging.
- **MIDI client-side VFO coalesce** + auto-recenter ownership handshake
  with the Thetis fork: extreme MIDI wheel input no longer fills the
  VFO queue, and the fork-side smooth-scroll guard now also works when
  no ThetisLink server is connected.
- Connect-time RX1/RX2 spectrum **balancing** so both RX paths come up
  at roughly the same moment; Auto-FFT retuned to ~25 FPS.

### Added — Persistence and UI polish

- PA `active_pa` choice and per-PA pre-Operate drive snapshots survive
  a non-graceful shutdown (process kill / power loss) without waiting
  for the next `start_server()` write.
- RF2K-S drive restore guard: no `ZZPC000;` is sent if the snapshot
  is missing.
- Status-panel **protocol version-mismatch** banner: previously the
  mismatch was silent; now there is a visible row when a v2.0.2 client
  contacts the v2.0.3 server (or vice versa).
- Scrollable Radio tab, resizable spectrum split, persisted pop-out
  window geometry.

### Changed

- Tuner removal: the v2.0.2 `assume_tuned` checkbox, `TUNER_DONE_ASSUMED`
  state (5) and the 500 ms assume-deadline pad were retired now that
  feedback-driven tune-detection works reliably in production. The
  client/Android paths that still recognise state 5 are harmless dead
  paths and will be removed in v2.0.4.
- Voltage divider on the yellow tune-status wire: both R1 and R2
  moved from 10 kΩ to **1 MΩ** to reduce loading on the JC-Control
  LED circuit. Ratio (1:1, ×2 in voltage) and the threshold defaults are
  unchanged. The full wiring schema is documented separately and
  available on request.

### Fixed

- **Config RMW race** in the status-panel write paths: per-tuner MCP
  serial, Amplitec-pos, threshold and hysteresis edits now go through
  `config::modify_config(|c| …)` so the load/mutate/save sequence is
  atomic under `CONFIG_LOCK` — closing the same race that was fixed for
  the RF2K drive snapshot earlier in the v2.0.3 cycle but had been
  reintroduced by the new tuner UI.
- **ADC dedup** in the tuner thread: the bridge `snapshot()` rate-limits
  USB ADC polls to 100 ms while the tuner thread runs the active/idle
  edge loops at 25 ms. The thread now checks the
  `DebugSnapshot.last_adc_at` timestamp and only counts a consecutive
  edge when the timestamp has advanced — defeating the "count the same
  cached sample twice" failure mode that defeated noise rejection.
- **Threshold/hysteresis edge-clamp**: slider combinations like
  threshold 0.5 V + hysteresis 2.0 V used to produce an unreachable
  active edge at −0.5 V. The bridge now clamps the computed edges to
  the physically reachable range `[0, ADC_VREF × divider]` and exposes
  `edges_clamped: bool` so the UI can show an amber warning instead of
  silently locking the tuner.

### Compliance

- `vendor/mcp2221-hal/` v0.1.0 (Copyright © 2025 Rob Wells, MIT branch
  of the dual-licensed `MIT OR Apache-2.0` source) added as a vendored
  Rust crate; attribution added to `NOTICE.md` and the full MIT license
  text is reproduced in `compliance/THIRD-PARTY-LICENSES.html` and the
  SPDX SBOM at `compliance/sbom.spdx.json`. The Apache-2.0 alternative
  is deliberately not used because Apache-2.0's patent-grant clauses
  are not compatible with the `GPL-2.0-or-later` licence ThetisLink
  itself is distributed under.
- All `compliance/*` artefacts regenerated for the v2.0.3 binary set.

---

## [2.0.2] — 2026-05 (log-spam hotfix)

- Server-side `DiversityPhaseEx`, `DiversityGainEx` and
  `DiversityGainMultiEx` TCI notifications now log INFO only on a real
  value change. Thetis pushes these at every diversity tick (~10–20 Hz)
  which previously filled the server log at hundreds of thousands of
  lines per session.
- Functional behaviour and wire protocol unchanged (`VERSION = 2`) —
  fully interoperable with v2.0.0 and v2.0.1.

## [2.0.1] — 2026-05 (connect-experience release)

- First-run 4-step setup wizard (Find server → Password → 2FA →
  Connected).
- mDNS local-network discovery — clients auto-find servers on the same
  WiFi/LAN.
- Nine differentiated connect states with platform-aware NL/EN hints
  including a smart `TciUnreachable` hint that knows whether Thetis is
  running, starting up, or fully stopped.
- Server status panel: bind address, TCI status, active clients with
  RTT / loss / jitter, audio routing chips, recent connect attempts.
- Server-side RX2 audio-filter fix (no more phantom CH2 stream when RX2
  is off).
- "Restart setup wizard" button.
- Wire protocol unchanged (`VERSION = 2`) — fully interoperable with
  v2.0.0.

## [2.0.0] — 2026-04 (TL2 release)

- Yaesu FT-991A auto-DFM PTT toggle (FM ↔ DATA-FM with memory restore).
- Server-side CTUN auto-recenter (Thetis-fork `auto_recenter_ex`).
- Live diversity null-circle broadcast (Smart / Ultra).
- Filter-preset push (F1..VAR2/NONE), per-RX DDC sample rate
  (48..1536 kHz), `tci_caps_ex` capability broadcast.
- DX cluster click-to-tune, SWR display in TX meter.
- CW keyer + macros over TCI.
- Single-TCI-only architecture — separate CAT connection retired.
- **Wire-protocol VERSION bumped from 1 → 2.**

## [1.0.0] — 2026-03

- First public release on [`cjenschede/ThetisLink`](https://github.com/cjenschede/ThetisLink).

## [0.5.0]

- Yaesu FT-991A support, Bluetooth headset (Android), diversity-receive
  fix, TCI control elements, RF2K-S reset, PTT modes, DX cluster.

## [0.4.9]

- Wideband Opus TX, device-switch fix.

## [0.4.2]

- Configurable FFT size, dynamic spectrum bins, Android power-button fix.

## [0.4.1]

- WebSDR / KiwiSDR integration, frequency sync, TX-spectrum auto-override.

## [0.4.0]

- TCI WebSocket, waterfall click-to-tune (Android).

## [0.3.2]

- MIDI controller support, PTT toggle with LED, mic AGC.

## [0.3.1]

- Band memory, FM filter fix, macOS client.

## [0.3.0]

- Full RX2 / VFO-B support, DDC spectrum + waterfall.
