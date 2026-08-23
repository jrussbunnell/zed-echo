# Echo — conversation mode

Status: approved 2026-08-19. Source of truth for full-duplex voice and Echo's
own conversational layer. Builds on
`2026-08-19-voice-input-design.md`, which is shipped.

## Goal

Today's voice input is a walkie-talkie: address Echo, say one command, it
dispatches, it goes back to sleep. Every question you ask costs an agent turn,
and narration stops dead whenever you speak.

This makes it a conversation. The wake word opens an exchange that stays open,
Echo answers what it can itself, and the questions that need real work go to a
session that is not the one doing your work.

The requirement behind all of it: **watch what the agent is doing and ask about
it without disturbing the session doing it.**

## What already exists

Verified in tree, and two of these correct claims made earlier in design.

| Piece | Where | Note |
|---|---|---|
| AEC reference signal | `crates/audio/src/audio_pipeline.rs:313-317` | **Already wired.** `Audio::connect_player` returns a player on the shared output mixer, which runs `process_reverse_stream` over everything just before output. Read-aloud's speech is already the reference. |
| AEC input half | `EchoCanceller::process_stream` | Applied by the livekit call path (`playback.rs:376`), **not** by `listen` |
| APM format | `audio_pipeline.rs:28` | 48 kHz, 2 channels, 10 ms buffers (`BUFFER_SIZE`) |
| Wake listener | `crates/listen/src/wake.rs` | Resamples to 16 kHz mono immediately, which is upstream of where AEC has to happen |
| Re-arm without a wake word | `Listener::arm_without_wake` | Built for approval read-backs; conversation mode generalizes it |
| Intent table | `crates/listen/src/intent.rs` | Whole-phrase matching, 7 tests |
| Narration summary model | `read_aloud.summary_model` | Resolves to the inline assistant model; reports after three consecutive failures |
| Session creation | `AgentConnection::new_session(project, work_dirs, cx)` | What the sidecar is made from |
| Transcript tailing | `crates/agent_ui/src/subagent_notifications.rs` | Already reads Claude Code session transcripts off disk |
| Inworld duplex STT | `wss://api.inworld.ai/stt/v1/transcribe:streamBidirectional` | The streaming endpoint the shipped provider does not use |

## Decisions locked

Ratified in brainstorming; do not redesign.

- **A conversation window, not an always-open mic.** The wake word opens an
  exchange; each exchange re-arms for fifteen seconds; silence closes it. A
  toggle that leaves the microphone hot turns every side conversation into
  input, and the wake word is what makes the feature survive a room with
  other people in it.
- **Table first, then model.** Controls and obvious commands match the existing
  `parse_intent` table with no model call at all. Only what falls through
  reaches a model. `stop` must never wait on a network round trip.
- **Duck, don't pause.** Narration drops rather than stopping when the user
  speaks, so clearing your throat does not cost you the thread.
- **Three destinations.** Echo answers state and control questions itself; a
  warm sidecar session answers questions about the code and the transcript; the
  working session receives only instructions that change the work.
- **The sidecar is a real agent session**, not a second retrieval stack. It has
  the same tools, so it cannot develop a different view of the repository than
  the agent has.
- **Reuse `read_aloud.summary_model`** for the local answerer rather than
  introducing a second model setting.

## Architecture

```
                        ┌── table hit ──────────────► control / command (0 calls)
mic ─► AEC ─► VAD ─► STT ─┤
                        └── falls through ─► router ─┬─► answer here      (1 call)
                                                     ├─► sidecar session  (1 + agent)
                                                     └─► working session  (steer/send)
```

New module `crates/listen/src/conversation.rs` holds the window state machine.
`crates/agent_ui/src/voice_dispatch.rs` grows the router and the sidecar; the
`listen` crate still knows nothing about threads.

### The conversation window

States: `Asleep → Listening → Capturing → Answering → Listening …`, back to
`Asleep` on a silence timeout.

- The wake word moves `Asleep → Listening`.
- Voice activity moves `Listening → Capturing` on speech, and ends the
  utterance on trailing silence rather than on the recognizer's endpointing,
  which is too slow for back-and-forth.

  Activity comes from the on-device recognizer already running: a partial
  transcript that grows means speech, and one that stops growing for 800 ms
  means the utterance ended. `SFSpeechDetector` would be more direct, but it is
  a second framework object fed by the same audio for a signal the recognizer
  already implies. ponytail: switch if the partial-transcript heuristic proves
  jumpy in a noisy room.
- After each answer the window re-arms for `conversation_window` (default 15s).
- Closing is announced by a short sound rather than speech, so the end of a
  conversation is audible without being narrated.

`arm_without_wake` becomes the `Listening` state rather than a special case, and
the approval read-back stops being a separate path.

### Full-duplex audio

The chain is reordered so cancellation happens where the APM can do it:

```
open_input_stream ─► constant_params(2ch, 48kHz) ─► 10ms buffers
   ─► EchoCanceller::process_stream ─► to_mono ─► 16kHz ─► recognizer + STT
```

The canceller is the same instance the output mixer holds — it is `Clone` over
an `Arc<Mutex<_>>`, and both halves must share one APM or cancellation does
nothing.

This is what lets the microphone stay live while Echo speaks, which is what
makes ducking possible instead of pausing.

### Streaming transcription

A second `SttProvider`, `InworldStreamingStt`, over
`transcribe:streamBidirectional`. The trait already takes a frame stream and
returns refining transcripts, so this is a provider, not a redesign; the
buffered one stays as the fallback when a WebSocket cannot be opened.

Partial transcripts drive the barge-in decision: the moment a partial is
non-empty, narration ducks.

### Routing

1. **Table.** `parse_intent` as it is today. A hit dispatches immediately.
2. **Router call.** One call to the summary model with the utterance, a compact
   state block, and this window's exchanges. One round trip, not two: it
   replies with a destination *and*, when the destination is `answer`, the
   answer itself.

   The reply is one line, `DESTINATION: text`, where destination is `ANSWER`,
   `SIDECAR`, or `AGENT`. A line that does not parse is treated as a failure
   rather than guessed at. JSON was rejected because a small fast model asked
   for one spoken sentence should not also be spending tokens on braces.
3. **Destinations.**
   - `answer` — spoken directly.
   - `sidecar` — forwarded to the warm session; Echo says a short
     acknowledgment first so the wait is explained rather than dead air.
   - `agent` — steered into the working turn, or sent when idle, exactly as
     `VoiceCommand::Say` does today.

The state block is small on purpose: which threads are running, the current
tool call and file, elapsed time, subagent statuses, and the last few narration
lines. Everything in it is already in memory.

The router also sees the exchanges so far **in this window** — the question and
the answer, not the reasoning. Without them "and what about the tests?" is
unanswerable, and a conversation whose every turn is context-free is not one.
The history dies with the window; it is bounded by the window's length rather
than by a token count.

### The sidecar session

Created on the first question that needs it, from
`AgentConnection::new_session` against the same project. Kept warm for the life
of the conversation mode.

It is told, in its first prompt, that it is answering questions about work
another session is doing, that it must not edit anything, and where that
session's transcript is on disk — the path `subagent_notifications.rs` already
derives.

It never appears in the thread list. Its answers are spoken and dropped; the
working session never sees them.

**Read-only is asked for, not enforced.** Enforcing it means a permission
policy this design does not build. The mitigation is that the sidecar is a
separate session with no working context to damage, and the user still sees
tool-call approvals for anything it tries.

### Ducking

`read_aloud` gains `duck(fraction)` and `unduck()`, applied at the sink's
existing volume control rather than by pausing. Speech ducks to 20%, restores
when the utterance ends and nothing was dispatched.

An explicit `stop` still pauses outright.

## Settings

```jsonc
"listen": {
  "conversation": false,          // opt in to the window staying open
  "conversation_window_seconds": 15,
  "duck_to": 0.2,
  "streaming": true,              // duplex STT when the provider supports it
  "sidecar": true                 // answer deep questions off-session
}
```

Added to the existing `listen` block. Every one of them off or absent leaves
today's behavior exactly as it is.

## Failure modes

| Failure | Behavior |
|---|---|
| WebSocket will not open | Fall back to the buffered provider, log once, keep listening |
| Router call fails | Treat the utterance as `agent` — forwarding a question is recoverable, silence is not |
| Router returns an unknown destination | Same as a failure |
| Sidecar will not start | Say so once, forward to the agent instead |
| Sidecar is slow | The acknowledgment already played; no second filler |
| AEC unavailable on the platform | The fake implementation is a no-op, so the window still works with pausing rather than ducking |
| VAD never fires | The fifteen-second window closes on its own |

## Testing

- **Window state machine** on `FakeWake` + `FakeStt`: wake opens, exchange
  re-arms, silence closes, an explicit stop closes immediately.
- **Router** as a pure function over a scripted model reply: each destination,
  an unparsable reply, an unknown destination.
- **Table precedence**: every phrase in the intent table dispatches without a
  model call. Asserted by a provider that fails the test if called.
- **Ducking**: narration volume drops on a non-empty partial and restores when
  the utterance dispatches elsewhere.
- **AEC ordering**: a test that the mic chain presents 48 kHz stereo to
  `process_stream` before any downmix, since getting this backwards cancels
  nothing and is invisible without hardware.
- Sidecar lifecycle with a fake connection: created once, reused, disposed on
  mode off.

## Build order

1. AEC chain reorder — no behavior change, verifiable by ordering test
2. Ducking in `read_aloud`
3. Conversation window state machine
4. Streaming STT provider
5. Router, with the table taking precedence
6. Sidecar session
7. Settings and defaults

Each stands alone: 1 and 2 improve today's mode without any of the rest.

## Not in scope

Interrupting Echo mid-answer · speaker identification · a wake-word-free mode ·
conversation history across windows (a closed window starts clean) · enforcing the sidecar's read-only intent · any platform but
macOS for the wake listener.
