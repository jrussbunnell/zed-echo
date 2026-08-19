# Echo — voice input

Status: approved 2026-08-19. Source of truth for the `listen` crate.

## Goal

Echo narrates the agent's work but has no way to hear a reply. This closes the
loop: an always-on wake word, a spoken command, and a dispatch into the agent
plumbing the fork already has. The target situation is *away from the machine* —
pacing, cooking, walking — with the at-desk case falling out for free.

Half-duplex today; full-duplex after this.

## What already exists

Nothing here is invention; the pieces are in tree and verified.

| Piece | Where |
|---|---|
| Mic capture (`rodio::microphone::Microphone`) | `audio::open_input_stream`, `crates/audio/src/audio_pipeline.rs:230` |
| WebRTC echo canceller | `crates/audio/src/audio_pipeline/echo_canceller.rs` |
| Mid-turn steering, end to end | `crates/agent_servers/src/acp.rs:2180` → `crates/agent_ui/src/conversation_view/thread_view.rs:4249` |
| Tool authorization | `AcpThread::authorize_tool_call`, `crates/acp_thread/src/acp_thread.rs:3816` |
| Per-subagent status incl. `AwaitingApproval` | `crates/agent_ui/src/subagents.rs` |
| Spoken tool-call facts | `ToolCallFacts`, `crates/read_aloud/src/narration.rs` |
| Mic entitlement + usage strings | `crates/zed/resources/zed.entitlements`, `resources/info/Permissions.plist` |

The bundle already declares `NSMicrophoneUsageDescription`,
`NSSpeechRecognitionUsageDescription`, and
`com.apple.security.device.audio-input`. No bundle work.

## Decisions locked

Ratified in brainstorming; do not redesign.

- **Always-on listening with a wake word**, not a conversation window. A window
  that opens only while Echo speaks leaves the user mute in the two moments they
  most need to talk: a long silent tool run, and the idle after a turn ends.
  Away-from-machine means initiating, not only responding.
- **On-device wake word, cloud STT for commands.** Apple's Speech framework
  listens continuously (free, private, no key); the utterance after the wake word
  streams to Inworld STT, which handles code-heavy phrasing better.
- **Four capabilities in v1**: steer a running turn, send when idle, answer
  permission prompts, control narration.
- **Confirm approvals only.** Messages, steering, and narration control fire
  immediately — all recoverable. An approval is not.
- **Route to whatever last spoke or is blocking**, not to the active thread. The
  user cannot see which thread is active.
- **Approach A**: a new `listen` crate mirroring `read_aloud`, with dispatch in
  `agent_ui`. Not folded into `read_aloud` (already 7,529 lines), not inlined
  into `conversation_view` (already +9,627 in this fork).

## No acoustic echo cancellation in v1

The mic is live while Echo speaks, so the mic hears Echo. The obvious answer is
to wire `EchoCanceller::process_reverse_stream` with the sink's played samples —
nothing taps them today, so that is real work.

It is also avoidable. Detecting the wake word pauses narration *before* the
command is spoken, so the command lands against a silent speaker. Only the wake
word itself must survive Echo's own voice, and a false wake costs a brief
narration pause that is self-evident and auto-resumes.

AEC is the upgrade path, taken if false wakes prove common in practice.

## Architecture

New crate `crates/listen/`, `[lib] path = "src/listen.rs"`.

| File | Holds |
|---|---|
| `listen.rs` | `Listener` entity, `ListenSettings`, `ListenEvent`, `init` |
| `provider.rs` | `SttProvider` trait, `Transcript`, `FakeStt` |
| `wake.rs` | On-device wake listener, `#[cfg(target_os = "macos")]` |
| `inworld_stt.rs` | Inworld streaming STT |
| `intent.rs` | transcript → `VoiceCommand` |

Dependencies: `audio`, `http_client`, `gpui`, `settings`, `util`, `anyhow`,
`futures`, `serde_json`, `log`. Deliberately **not** `read_aloud` or `agent_ui`.

Direction of control:

```
listen::Listener  --ListenEvent-->  agent_ui  -->  read_aloud (pause/resume)
                                          \-->  AcpThread   (steer/send/authorize)
```

`agent_ui` owns both entities already, so the wiring lives there and neither
crate learns about the other. This is also why routing is not in `listen`:
routing needs threads, subagents, and the approval queue.

### `SttProvider`

Mirrors `TtsProvider`. Streaming, because a command should dispatch on the
provider's endpointing rather than after a full round trip.

```rust
pub trait SttProvider: Send + Sync + 'static {
    /// Streams transcripts for one utterance as they refine. The final item
    /// is `is_final`. Dropping the receiver cancels the work.
    fn transcribe(
        &self,
        audio: mpsc::UnboundedReceiver<Vec<f32>>,
        cx: &App,
    ) -> mpsc::UnboundedReceiver<Result<Transcript>>;
}

pub struct Transcript { pub text: String, pub is_final: bool }
```

`FakeStt` is the test double, mirroring `FakeTts`: scripted transcripts, a
`fail_next`, and a hold/release for latency. Every test below runs on it.

## Pipeline

States: `Idle → Woke → Capturing → Dispatched`, and back to `Idle`.

1. Mic opened once through `audio::open_input_stream` when `listen.enabled`,
   owned by `Listener` for the process lifetime of the setting.
2. Frames feed `SFSpeechRecognizer` with `requiresOnDeviceRecognition = true`.
   The recognition request restarts on a ~50s timer: the framework enforces a
   per-request audio duration limit. A `ponytail:` comment names macOS 26's
   `SpeechAnalyzer` as the upgrade — better accuracy and no duration limit, but
   Swift-only, so it needs a helper binary rather than an objc2 binding.
3. A wake match in the partial transcript emits `ListenEvent::Woke`. `agent_ui`
   pauses narration on it. Matching is case-insensitive on the configured
   `wake_word` at an utterance boundary.
4. Subsequent frames stream to the command `SttProvider`.
5. The utterance ends on the recognizer's endpointing. The final transcript goes
   through `intent`, and `ListenEvent::Command(VoiceCommand)` is emitted.
6. No speech within 5s of the wake → cancel, emit `ListenEvent::Abandoned`,
   `agent_ui` resumes narration.

## Intent

```rust
pub enum VoiceCommand {
    Say(String),   // free text; the dispatcher decides steer vs send
    Approve, Deny,
    Pause, Resume, Repeat, CatchUp, Next, Previous,
    Interrupt,     // cancel the agent's turn
    Never,         // "never mind"
}
```

A keyword matcher over the leading words, not a model. A model round-trip adds
about a second to `stop`, which is the single command that most needs to be
instant. Unmatched input is `Say`. A `ponytail:` comment records that LLM
classification is the upgrade if the matcher proves brittle.

Bare `"stop"` means **stop narration**, not stop the agent: the immediate,
recoverable reading. Interrupting a turn requires `"stop the agent"`,
`"cancel that"`, or `"interrupt"`.

## Routing

Resolved in `agent_ui` at dispatch time, in priority order:

1. Any thread blocked on approval. Blocking outranks talking, and
   `SubagentStatus::AwaitingApproval` already encodes that a blocked subagent
   transitively blocks its parent.
2. The thread that most recently spoke through `read_aloud`.
3. The active thread.

Given a target, `Say` dispatches by turn state: a running turn goes to
`thread.steer()`, an idle one to an ordinary send. `SteerOutcome::NeedsPrompt`
falls back to a send rather than dropping the message.

## Approval confirmation

On `Approve` with a tool call pending:

1. Echo speaks it back from the existing `ToolCallFacts` — "Claude wants to run
   cargo test. Approve?"
2. The listener re-arms for 8s **without** requiring the wake word; the user is
   already mid-conversation. `agent_ui` asks for this through
   `Listener::arm_without_wake(Duration)` — the one control `agent_ui` pushes
   into the listener, everything else being an event out of it.
3. A second affirmative calls `authorize_tool_call`. Anything else, or the
   timeout, leaves it unauthorized and Echo says so.

`Deny` is not confirmed. It is the safe direction, and a spurious deny costs one
re-request.

`confirm_approvals: false` collapses step 1-3 into a direct authorize, for a user
who has decided the read-back is not worth the latency.

## Settings

```jsonc
"listen": {
  "enabled": false,
  "provider": "inworld",   // command STT; "system" = on-device throughout
  "wake_word": "echo",
  "input_device": null,
  "confirm_approvals": true
}
```

Same shape as `ReadAloudSettings`, including a `resolve_provider` that returns an
error for an unknown provider rather than silently serving Inworld.

## Failure modes

| Failure | Behavior |
|---|---|
| Mic permission denied | Spoken and visible notice; the listener parks. Never a silent no-op |
| Speech recognition unauthorized | Same, naming which permission |
| Device disappears or switches | The stall-watchdog pattern read_aloud uses for output: reopen, bounded retries, then park with a notice |
| STT request fails | "I didn't catch that", resume narration. Never guess at a command |
| Empty or whitespace transcript | Treated as a failed STT request |
| Wake fires on Echo's own voice | Narration pauses, no command follows, the 5s timeout resumes it |
| Provider unknown | Listener disabled with a notice, mirroring read_aloud |

No `let _ =` on any fallible path here. Every failure reaches a surface the user
can perceive, because a listener that has silently stopped listening is
indistinguishable from one that is working until the moment it matters.

## Testing

Everything except `wake.rs` is testable headless on `FakeStt`.

- **Intent**: table test over phrasings, including the `stop` split and every
  narration verb.
- **Pipeline**: wake → capture → dispatch on `FakeStt`; the 5s abandonment path;
  a failed transcription resuming narration.
- **Routing**: a GPUI test with a blocked thread *and* a speaking thread,
  asserting the blocked one wins; steer-vs-send by turn state; the
  `NeedsPrompt` fallback.
- **Approval**: no `authorize_tool_call` without the second affirmative; timeout
  leaves it unauthorized; `confirm_approvals: false` authorizes directly.
- `listen` joins `echo_tests.yml`, whose push trigger still names the renamed
  `read-aloud` branch and is fixed in the same change.

`wake.rs` gets a thin seam (`WakeListener` trait) so the pipeline tests drive it
with a fake; the real binding is verified by hand against a live mic.

## Build order

The Apple Speech binding is the only part that cannot be verified headless, so
it lands last and nothing else depends on its internals.

1. Crate skeleton, `SttProvider`, `FakeStt`, settings
2. `intent.rs` and its table test
3. `Listener` state machine against a fake wake seam
4. `agent_ui` dispatch: routing, steer-vs-send, narration control
5. Approval confirmation
6. `inworld_stt.rs`
7. `wake.rs` — the real objc2-speech binding
8. CI

## Not in v1

Acoustic echo cancellation and true barge-in while Echo is still speaking ·
addressing a subagent by name · local command STT · voice identification ·
a wake listener on Linux or Windows, which get nothing, matching `system_tts`.
