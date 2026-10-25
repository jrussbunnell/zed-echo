# Echo — real-time voice mode

Status: draft 2026-10-25. Builds on `2026-08-19-voice-input-design.md` and
`2026-08-19-conversation-mode-design.md`, both shipped. Modelled on Otto's voice
mode (`ryse-inc/landing/docs/superpowers/specs/2026-10-06-otto-voice-mode-design.md`).

Items marked **(call)** were decided by Claude while writing this, not by
Joshua. Everything else was decided in conversation.

## Intent

Talk to Echo the way you'd talk to another developer sitting next to you. You
can interrupt it. It answers in under a second, and it keeps talking with you
while agents work.

Today's voice is a walkie-talkie. You say the wake word and one command, Echo
transcribes it, routes it, and answers through narration. Questions cost a
model round trip before anything is said, and there is no way to break in
while Echo is talking. Otto showed that a real-time speech model in front, with
the agents behind it doing the work, feels like a conversation instead.

### Three ways it gets used

All three are in scope and equally important:

1. **Drop in, instruct, leave.** Open voice mode, tell a running session to
   change course, close voice mode.
2. **Start work, leave.** Open voice mode, describe a task, Echo starts a new
   session on it, close voice mode.
3. **Stay and pair.** Keep voice mode open while sessions work. Echo talks back
   as things happen, and you discuss, redirect, approve, and ask questions.

### How it talks

Like a developer pairing with you, not a narrator. It says what the agent is
doing and why, flags surprises, and asks when it's unsure. It **never** reads
scripts, diffs, SQL, file contents, or tool output aloud. "It's rewriting the
booking query to drop the N+1, about halfway" is the register. Reading
`SELECT … FROM bookings JOIN …` is the failure.

## Goals

- A hotkey toggles voice mode from anywhere in Echo.
- Interrupting Echo mid-sentence stops it at once.
- A request is acknowledged in under a second, even when the work behind it
  takes minutes. Tools never block the conversation.
- Voice can do anything a typed message can: steer or queue into a session,
  start a session, answer approval prompts, stop a session, and ask the
  sidecar.
- The voice-glow from Otto, ported to GPUI, shows who is speaking.
- Echo speaks up on its own when something happens that you'd want to know.

## Non-goals

- Replacing the wake-word path. It stays as the cheap local command path, and
  is suspended only while voice mode is open (they share the microphone).
- Voice on any platform but macOS in v1. The audio code is cross-platform, but
  only macOS is tested.
- Gemini Live in v1. The provider seam leaves room for it (see Providers).
- Voice-specific permission rules.
- Remembering conversations across voice-mode sessions. Closing starts clean.

## Anti-goals

Any of these makes it a failure even if it technically works:

- Reading code, commands, or tool output aloud.
- Doing something by voice that typing the same request couldn't do, or
  skipping a prompt that typing would hit.
- Going silent for the length of a tool run because a tool call blocked the
  conversation.
- Two Echo voices talking over each other, as with read-aloud narration plus
  the voice model.
- A microphone left open with no visible sign that it is.

## Decided

| Decision | Detail |
|---|---|
| Provider | **Inworld Realtime**, using the same Inworld key and voices as `read_aloud`. Behind a seam so Gemini Live or OpenAI Realtime can be added later. |
| Permissions | The same as typing. Each session's mode (auto, accept edits, bypass permissions, …) decides what needs approval. Approval prompts are spoken and can be answered by voice. Voice adds no rules of its own. |
| Visual | Otto's voice-glow (`ryse-inc/landing/components/ui/voice-glow.tsx`, itself a port of Libraries.dev `VoiceBeam`, MIT), ported to GPUI. |
| Entry | Cmd plus a key toggles voice mode. |
| Scope | Steering existing sessions and starting new ones are both v1. |

## Architecture

```
mic ─► AEC ─► mono 24 kHz PCM16 ─► Inworld Realtime (WebSocket) ─► 24 kHz PCM16 ─► output mixer
                                         │                                     (AEC reference)
                                         │ function_call
                                         ▼
                                 voice-mode tools  ──► thread views / agent panel
                                         ▲                 (same path as typing)
                                         │
                       session events ───┘  (proactive remarks)
```

The voice model is **a front, not a brain**. It handles turn-taking, small
talk, acknowledgements, and spoken summaries. It has no repository access of
its own. Questions about code go to the existing sidecar session. Instructions
go to the working sessions. This is the same split as Otto's.

### Where the code goes

| Piece | Location |
|---|---|
| Global voice-mode entity | `crates/agent_ui/src/voice_mode.rs`, set as a GPUI global |
| Protocol: event types, encode/decode, session config | `crates/listen/src/realtime.rs` (new) |
| Socket and audio pumps | `crates/listen/src/realtime_session.rs` (new) |
| Tools, proactive feed, mode lifecycle | `crates/agent_ui/src/voice_mode.rs` (new), alongside `voice_dispatch.rs` |
| Glow element | `crates/ui/src/components/ai/voice_glow.rs` (new) |
| Action and keybinding | `agent::ToggleVoiceMode` in `agent_ui.rs`; `assets/keymaps/default-macos.json` |
| Settings | `voice_mode` block, registered in `listen` next to `ListenSettings` |

`listen` still knows nothing about threads, as the August design requires. All
thread logic lives in `agent_ui`.

## Provider: Inworld Realtime

Verified against Inworld's docs on 2026-10-25. The service is in research
preview.

| Fact | Value |
|---|---|
| Endpoint | `wss://api.inworld.ai/api/v1/realtime/session?key=<session-id>&protocol=realtime` |
| Auth | `Authorization: Basic <api-key>`. Echo is a desktop app, so no ephemeral token is needed. Key from `read_aloud::resolve_api_key`. |
| Protocol | OpenAI Realtime events plus Inworld extensions under `providerData` |
| Flow | `session.created` → `session.update` → audio/text in → events out |
| Config | `model`, `instructions`, `output_modalities`, `audio.output.{voice,model,speed}`, `audio.input.turn_detection`, `tools` |
| Audio | Base64 mono PCM16 at 24 kHz in both directions; send 60–100 ms chunks |
| Audio in | `input_audio_buffer.append` |
| Audio out | `response.output_audio.delta` / `.done` |
| Transcripts | `conversation.item.input_audio_transcription.delta/.completed`; `response.output_audio_transcript.delta` |
| Barge-in | `turn_detection: { type: semantic_vad, interrupt_response: true }`; `input_audio_buffer.speech_started`; `conversation.item.truncate { item_id, content_index, audio_end_ms }` |
| Tool call | `response.function_call_arguments.done { call_id, name, arguments }` |
| Tool result | `conversation.item.create { item: { type: function_call_output, call_id, output } }`. The server continues on its own unless `providerData.auto_tool_response: false`. |
| Text in | `conversation.item.create` with `input_text`, then `response.create` |
| Cancel | `response.cancel` |

The socket uses `async-tungstenite` through `gpui_tokio`, the same way
`client` reaches it. `inworld_streaming.rs` explains why August stopped short
of this. That reasoning was about half a second of transcription latency;
here the socket is the whole feature.

### Session config

- `model`: from the `voice_mode.model` setting. **(call)** The default is the
  fastest capable model the key has access to, chosen during step 1 of the
  build. A voice front needs latency more than depth.
- `audio.output.voice`: `voice_mode.voice`, falling back to
  `read_aloud.voice_id`, so narration and conversation sound like one Echo.
- `turn_detection`: `semantic_vad`, eagerness `medium`, `create_response: true`,
  `interrupt_response: true`.
- `instructions`: the persona (below) plus a state block refreshed on each
  `session.update`.

### Providers

A `RealtimeVoice` trait wraps connect/send/events, with `InworldRealtime` as
the only v1 implementation. Because Inworld speaks the OpenAI protocol, an
OpenAI Realtime implementation is mostly an endpoint and auth change. Gemini
Live has its own protocol and would be a real second implementation. **(call)**
The seam is just the trait and a `voice_mode.provider` setting accepting only
`"inworld"`; nothing more is built for it in v1.

## Audio

**In:** open the input stream the way the wake listener does, then
`EchoCanceller::process_stream` at 48 kHz stereo (the canceller the output
mixer already feeds, per the August design), downmix to mono, resample to
24 kHz, PCM16, base64, and send an `input_audio_buffer.append` every 80 ms.

**Out:** decode `response.output_audio.delta`, and append to a rodio `Player`
on the shared output mixer, so Echo's own voice is the canceller's reference
and the mic doesn't hear it. Use `read_aloud::RodioSink`, or its approach:
replace the player to stop, never `skip_one()` or `clear()` (see commit
`d9b8341e44`).

**Barge-in:** on `input_audio_buffer.speech_started` while audio is playing:
1. Drop local playback at once, by replacing the player.
2. Send `conversation.item.truncate` with the item id and `audio_end_ms` equal
   to the milliseconds actually played, so the model knows what you heard.

The server cancels its own response because `interrupt_response` is set.

**Levels for the glow:** RMS of each mic frame (input) and of each output
chunk as it is played (output), published about 20 times a second.

### One voice at a time (call)

Pausing isn't enough. `pause_all_narration` only pauses readers that are
speaking at that moment, and resuming would replay a backlog. So:
- `read_aloud` gains an app-wide **suppressed** flag. While it's set, readers
  drop new narration instead of queueing it, and anything playing stops. On
  close, the flag clears and readers continue from live, with no backlog.
  Readers you paused by hand stay paused.
- `say_to_the_user` (`"Approved."`, `"Stopped."`) becomes a note to the voice
  model while voice mode is open, so Echo says it in the conversation's own
  voice.
- Every window's wake-word listener is suspended, and resumes on close.

### One voice mode per app (call)

There's one microphone, so voice mode is app-wide: a global entity holds the
socket, mic, and playback.
- It opens bound to the window where the hotkey was pressed. The glow, caption
  and pill draw there.
- Its tools and the proactive feed see the threads of every open agent panel,
  so a session in another window can still be steered. The state block labels
  each session with its project.
- `start_session` starts in the bound window's panel.
- Pressing the hotkey in another window moves the binding there instead of
  closing the mode. Closing the bound window closes the mode.

## Persona

The voice model's instructions, in substance:

- You are Echo, pairing with Joshua out loud. Agents in Echo do the work, and
  you talk about it and direct it.
- Speak like a colleague: short, plain sentences, one idea at a time. Never
  read code, commands, paths longer than a filename, SQL, diffs, or tool
  output. Summarize what it means.
- Acknowledge a request immediately, then act through a tool.
- When asked about code or "why did it do X", use `ask_sidecar`. Don't guess.
- Before stopping a session or rejecting an approval, say what you're about to
  do in one short sentence. **(call)** The prompt asks for this but nothing
  enforces it. The permission modes are the real guarantee.
- Session events arrive as bracketed notes. Mention one only if it's worth
  interrupting a quiet room for.

The state block lists open sessions with title, agent, status, current
activity, elapsed time, and pending approval, from the same data as
`narration_peek::current_activity` and `AcpThread::recent_narration`.

## Tools

Every tool returns at once with a short status string. Work that takes time
reports back later as a session event.

| Tool | Arguments | Does | Path it takes |
|---|---|---|---|
| `list_sessions` | — | Open sessions and subagents with status and activity | The state block's data |
| `peek_session` | `session` | Last few narration lines and current activity | `recent_narration`, `current_activity` |
| `tell_session` | `session`, `message` | Steers into a running turn, or sends when idle | `ThreadView::send_voice_text`, the same as today's voice `Say` |
| `start_session` | `task`, `agent?` | New thread in the active project with `task` as its first prompt | `AgentPanel::external_thread` with initial content, as `new_agent_thread_with_external_source_prompt` does |
| `answer_approval` | `session`, `decision` (`allow_once` / `allow_always` / `reject`) | Answers the pending prompt | `authorize_by_voice`'s path |
| `stop_session` | `session` | Cancels the session's turn | The thread view's cancel, as the Stop button does |
| `ask_sidecar` | `question` | Asks the warm sidecar session; the answer comes back as an event | `voice_dispatch` sidecar (`ensure_sidecar`, briefing) |
| `end_voice_mode` | — | Says goodbye and closes the mode | Same as the hotkey |

`session` is matched loosely: a title fragment, "the booking one", "the
subagent", or empty. Empty uses `voice_dispatch::pick_voice_target`. A blocked
session comes first, then whichever spoke last, then the active one. An
ambiguous match returns the candidates as the tool result so the model can
ask. **(call)** Fuzzy title matching is done in Rust, not by the model.

Approvals keep the ordinary UI prompt on screen. Answering by voice and
clicking are the same action, so whichever comes first wins.

**Confirming approvals (call).** `listen.confirm_approvals` (default on) still
applies, and Rust enforces it, not the prompt:
- `answer_approval` takes effect only if one of your speech turns
  (`input_audio_transcription.completed`) arrived after the prompt was
  described to you.
- Concretely, the tool's first call for a given pending prompt returns the
  description and "confirm with the user". A call after your next turn
  applies the decision.
- `allow_always` by voice requires the same confirmation even when
  `confirm_approvals` is off, because it can't be taken back.
- `reject` never needs confirmation.

### The sidecar in voice mode (call)

Voice mode owns its sidecar, separate from the wake listener's `VoiceSession`:
- The `Sidecar` struct and `ensure_sidecar` move out of `VoiceSession` into
  something both paths can hold.
- The sidecar is created on the first `ask_sidecar`. The tool returns
  "asking" at once, and the answer arrives as a note. A question never falls
  back to a working session. If the sidecar can't start, the tool says so.
- `ask_sidecar` takes an optional `session`, resolved like the other tools.
  The sidecar is briefed with that session's transcript path. Asking about a
  different session sends a short re-briefing before the question.
- `listen.sidecar` doesn't apply in voice mode; there is nowhere else for a
  question to go.

## Proactive remarks

Echo subscribes to the threads in the active workspace and turns events into
bracketed notes sent with `conversation.item.create`.

| Event | Example note |
|---|---|
| Approval needed | `[booking-fix needs approval: run migration on staging]` |
| Turn finished | `[booking-fix finished: <first sentence of its last message>]` |
| Turn failed / error | `[booking-fix failed: <error>]` |
| Subagent finished or failed | `[subagent "Research alternatives" finished]` |
| Sidecar answered | `[sidecar answer: …]` |
| Milestone | `[booking-fix: <agent message after ≥ 60 s of tool calls>]` |

A note is followed by `response.create` only if:
- you aren't speaking,
- Echo isn't mid-response, and
- the chattiness level allows it.

Notes that aren't spoken still go into the model's context, so "what's
happening?" gets a current answer.

Chattiness (`voice_mode.chattiness`):

| Level | Speaks up for |
|---|---|
| `quiet` | Approvals, failures, sidecar answers |
| `milestones` **(call: default)** | The above, plus turns finished and subagents finished |
| `commentary` | Everything, including milestones |

Unprompted remarks are at least 20 seconds apart, except approvals and
failures. Notes that arrive within that gap are combined into one.

The coalescing and gating rules are a pure function over a queue of events
and a clock, so they can be tested without audio.

## The glow

A GPUI element ported from `voice-glow.tsx`, keeping its geometry, envelope,
and colour split:

- Seven lobes fanned from the bottom centre, at the same x, width, and height
  on a 350 px design width, scaled to the element's width.
- Three bands per voice, following the level with attack `[0.06, 0.09, 0.13]`
  and release `[0.30, 0.40, 0.55]` seconds. `GAIN` stays as the calibration
  knob.
- Input lobes are teal or blue, output lobes violet. **(call)** The colours
  come from the theme's player/accent colours where they fit, and fall back to
  fixed teal/violet.
- An idle breath (`0.3 + 0.06·sin`) while connected and silent. Sideways flow
  with phase driven by level. Edge fade.
- Each lobe is a blurred, rounded quad drawn in `paint`, with no DOM. The
  element calls `request_animation_frame` only while voice mode is open.
- Reduced motion (the macOS accessibility setting) gets the resting shape,
  painted once.

**Placement (call):** across the bottom edge of the workspace window, about
120 px tall, overlaid and ignoring mouse input. Above it, centered:
- a one-line caption showing the latest user or Echo transcript, which fades
  after 4 seconds, and
- a small status pill ("Listening", "Echo", "Connecting…") whose ✕ ends the
  mode.

The pill is what keeps the microphone from ever being open without a visible
sign.

## Lifecycle

- **Open:** `agent::ToggleVoiceMode`, bound to `cmd-shift-space` on macOS.
  **(call)** Nothing in Echo's keymaps uses it. If macOS claims it on
  Joshua's machine, the builder picks another and notes it. On open: suspend
  the wake listener, pause narration, show the glow in "Connecting…", connect,
  send `session.update`, then start the mic.
- **Close:** the hotkey again, the ✕ on the pill, the `end_voice_mode` tool,
  or the idle timeout. Close the socket, stop the mic, drop playback, hide the
  glow, resume narration and the wake listener. A short sound plays on close,
  as August's conversation window does.
- **Idle timeout:** `voice_mode.idle_timeout_seconds` **(call: default 300)**.
  No speech from either side and no session running for that long closes the
  mode. A running session doesn't keep it open forever on its own: after
  **(call)** 30 minutes with no speech from you, it closes regardless.
- **Reconnect:** if the socket drops, reconnect once with the same
  instructions plus a summary of the last few exchanges. If it drops again
  within 30 seconds, close and say why in a toast.

## Settings

```jsonc
"voice_mode": {
  "provider": "inworld",
  "model": null,               // null → the default chosen in build step 1
  "voice": null,               // null → read_aloud.voice_id
  "chattiness": "milestones",  // quiet | milestones | commentary
  "idle_timeout_seconds": 300
}
```

Voice mode needs the Inworld key that `read_aloud` already uses. With no key,
the hotkey shows a toast saying so and does nothing else.

## Failure modes

| Failure | Behavior |
|---|---|
| No API key | Toast; mode doesn't open |
| Socket won't open, or the key lacks Realtime access | Toast with the server's error; mode closes |
| Socket drops | One reconnect, as above |
| Tool arguments don't parse | Return an error string as the tool output so the model can recover out loud |
| Session reference ambiguous or missing | Return the candidates; the model asks |
| Mic unavailable | Toast; mode closes |
| Output device changes | Reuse `read_aloud`'s sink-recovery path (`reconnect_output_mixer`) |

## Testing

- **Protocol:** encode/decode for every event type used, against fixture JSON
  copied from the docs. `function_call_output` requires a non-empty `call_id`.
- **Tool dispatch:** the session matcher (exact, fragment, empty → target,
  ambiguous) as a pure function. Each tool against `FakeAgentConnection`
  threads takes the same path as the typed action.
- **Proactive feed:** gating by chattiness, the 20-second gap, combining notes,
  silence while you or Echo are speaking, and approvals and failures bypassing
  the gap.
- **Barge-in:** `audio_end_ms` computed from what was played, not what was
  received.
- **Glow:** the envelope follower and lobe transform as pure functions, checked
  against values computed from the TS original.
- **One voice:** opening voice mode pauses narration and suspends the wake
  listener, and closing restores both.
- **Manual, with a real key** (no hardware test is possible): connect, talk,
  interrupt mid-sentence, steer a running session, start a new one, approve by
  voice, and confirm Echo's own voice doesn't trigger barge-in.

## Build order

1. **Probe (spike).** Connect with Joshua's key, confirm Realtime access, list
   the models the key can use, and record a real exchange (audio round trip
   plus one tool call) as fixtures. Pick the default model. Everything after
   this depends on it working. **If the key lacks Realtime access, or replies
   are too slow to acknowledge in under a second, stop and report to Joshua.**
   There is no automatic fallback to another provider.
2. Protocol module and fixtures.
3. Socket and audio pumps, including barge-in. The mode opens with no tools,
   just talking.
4. Tools.
5. Glow, caption, and pill.
6. Proactive feed.
7. Settings, idle timeout, reconnect, and polish.

Steps 3–5 give a usable "talk to Echo" before tools are complete.

## Left to the builder

- Exact resampler. `rodio` already has one in-tree, and August's wake listener
  resamples.
- How the state block is refreshed. Re-sending `session.update` on change is
  simplest; a context note is the alternative.
- The close sound.
- Exact wording of the persona prompt, within the rules above.

## Risks

- **Research preview.** Event names or behavior may change. The protocol module
  is the only place that knows the wire format.
- **Cost.** Not published on the pages read. The idle timeout and the
  30-minute cap bound it. Step 1 should note the rate from Joshua's dashboard.
- **Concurrent sessions** are limited by plan. Voice mode holds one.
