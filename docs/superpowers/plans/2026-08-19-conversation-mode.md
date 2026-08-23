# Conversation Mode Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Turn Echo's walkie-talkie voice input into a conversation that answers what it can itself and sends the rest somewhere that is not the session doing your work.

**Architecture:** The mic chain gains echo cancellation at the APM's own format so the microphone can stay live while Echo speaks. A conversation window generalizes the existing approval re-arm. Utterances route through the existing intent table first, then one fast model call that either answers or names a destination — the working session for instructions, a warm sidecar session for questions that need tools.

**Spec:** `docs/superpowers/specs/2026-08-19-conversation-mode-design.md`

## Global Constraints

Same as the voice-input plan: Rust 1.97.1 via `~/.cargo/bin/cargo`, `./script/clippy` not `cargo clippy`, no `mod.rs`, no `unwrap`/`expect` off test paths, never `let _ =` a fallible call, comments say *why*, full words in identifiers, GPUI executor timers in tests, `ponytail:` on every deliberate ceiling.

Verified anchors:
- `RodioExt::process_buffer::<N, F>` where `F: FnMut(&mut [Sample; N])` — the mutating adapter AEC needs
- `audio::{CHANNEL_COUNT, SAMPLE_RATE}` = 2 channels, 48 kHz; `audio_pipeline::BUFFER_SIZE` = 10 ms, **not currently re-exported from `audio.rs`**
- `Audio` is a GPUI `Global` with `pub echo_canceller: EchoCanceller` (`Clone` over `Arc<Mutex<_>>`)
- `async-tungstenite = "0.31.0"` in the workspace, used by `rpc` and `client`
- `AgentConnection::new_session(project, work_dirs, cx)`

---

### Task 1: Cancel the echo on the way in

**Files:** `crates/audio/src/audio.rs` (export `BUFFER_SIZE`), `crates/listen/src/wake.rs`, `crates/agent_ui/src/voice_dispatch.rs`

The reference half is already wired through the shared output mixer. This adds the input half, and it must run **before** the downmix — the APM wants 48 kHz stereo in 10 ms buffers, and the chain currently drops to 16 kHz mono immediately.

```rust
audio::open_input_stream(None)?
    .constant_params(audio::CHANNEL_COUNT, audio::SAMPLE_RATE)
    .process_buffer::<{ audio::BUFFER_SIZE }, _>(move |buffer| {
        let mut cancelled: [i16; audio::BUFFER_SIZE] = buffer.map(|s| s.to_sample());
        canceller.process_stream(&mut cancelled).log_err();
        for (sample, out) in buffer.iter_mut().zip(cancelled) {
            *sample = out.to_sample();
        }
    })
    .possibly_disconnected_channels_to_mono()
    .constant_samplerate(SAMPLE_RATE_HZ)
```

`SpeechWake::new` takes the canceller; the caller pulls it off the `Audio` global, since the wake thread has no `cx`.

- [ ] Re-export `BUFFER_SIZE` from `audio.rs`
- [ ] Thread the canceller into `SpeechWake::new`
- [ ] Reorder the chain as above
- [ ] Test: the chain hands `process_stream` 48 kHz stereo before any downmix. Getting this backwards cancels nothing and is invisible without hardware, which is exactly why it needs a test
- [ ] Commit

---

### Task 2: Duck instead of stopping

**Files:** `crates/read_aloud/src/read_aloud.rs`, `crates/read_aloud/src/sink.rs`

`AudioSink` gains `set_volume(f32)`; `RodioSink` forwards to the rodio player, `FakeSink` records it. `ReadAloud::duck()` and `unduck()` set 0.2 and 1.0.

Ducking rather than pausing means clearing your throat does not cost you the thread.

- [ ] `set_volume` on the sink trait and both implementations
- [ ] `duck`/`unduck` on `ReadAloud`
- [ ] Test: ducking leaves playback running and restores the prior volume
- [ ] Commit

---

### Task 3: The conversation window

**Files:** `crates/listen/src/conversation.rs` (new), `crates/listen/src/listener.rs`

States `Asleep → Listening → Capturing → Answering → Listening`, closing to `Asleep` after `conversation_window`.

Voice activity comes from the recognizer's partial transcript: growth means speech, 800 ms without growth ends the utterance. `WakeSignal` gains `PartialTranscript(String)` so the listener can see growth without the wake source deciding anything.

`arm_without_wake` becomes the `Listening` state rather than a special case.

- [ ] `WakeSignal::PartialTranscript`; `FakeWake::push_partial`
- [ ] Window state machine with the silence timer
- [ ] Tests: wake opens; an exchange re-arms; silence closes; explicit stop closes immediately; a partial that grows ducks narration
- [ ] Commit

---

### Task 4: Streaming transcription — FORMAT ONLY

`crates/listen/src/inworld_streaming.rs`. The config frame, the audio frame,
and the transcript parser are complete, with 7 tests.

**The socket is deliberately not wired.** No API key here, so nothing about the
exchange can be checked against the real endpoint; `async_tungstenite`'s
connector in this workspace runs on tokio, which `listen` does not have and
which every other user reaches through `gpui_tokio`; and what it buys over the
buffered provider that already works is the tail of one utterance.

Unverifiable protocol code behind a new runtime dependency, for half a second,
was not a trade worth making unattended. Pinning the format means wiring it is
an afternoon against a known-good encoding rather than reverse-engineering.

**Remaining:** open the socket through `gpui_tokio`, send `config_frame` then
`audio_frame`s, feed `parse_frame` output into the existing `SttProvider`
stream, and fall back to the buffered provider when the socket will not open.

---

### Task 5: Routing

**Files:** `crates/agent_ui/src/voice_dispatch.rs`

Table first — a `parse_intent` hit dispatches with no model call, so `stop` never waits on a network round trip. Everything else goes to one call to `read_aloud.summary_model` with the utterance, a compact state block, and this window's prior exchanges.

Reply is one line, `ANSWER: …` / `SIDECAR: …` / `AGENT: …`. Unparsable is a failure, not a guess.

- [ ] `parse_route` as a pure function over a model reply, with tests for each destination, an unparsable reply, and an unknown destination
- [ ] State block assembly from live thread state
- [ ] Within-window exchange history, bounded by the window
- [ ] Test: every phrase in the intent table dispatches without touching the model, asserted by a provider that fails if called
- [ ] Commit

---

### Task 6: The sidecar session

**Files:** `crates/agent_ui/src/voice_dispatch.rs`

Created on the first question that needs it from `AgentConnection::new_session` against the same project, kept warm, disposed when conversation mode turns off. Never enters the thread list; its answers are spoken and dropped.

Its opening prompt says it is answering questions about work another session is doing, that it must not edit anything, and where that session's transcript is — the path `subagent_notifications.rs` already derives.

Read-only is asked for, not enforced. That is a stated limitation, not an oversight.

- [ ] Lifecycle: created once, reused, disposed on mode off
- [ ] Opening prompt with the transcript path
- [ ] Spoken acknowledgment before forwarding, so the wait is explained
- [ ] Tests with a fake connection: created once across two questions; disposed on mode off; a failure to start falls back to the agent
- [ ] Commit

---

### Task 7: Settings

**Files:** `crates/settings_content/src/settings_content.rs`, `crates/listen/src/listen.rs`, `assets/settings/default.json`

```jsonc
"listen": {
  "conversation": false,
  "conversation_window_seconds": 15,
  "duck_to": 0.2,
  "streaming": true,
  "sidecar": true
}
```

Every one absent leaves today's behavior unchanged.

- [ ] Content struct, `ListenSettings` fields, defaults
- [ ] Test: defaults reproduce today's behavior
- [ ] Commit

---

### Task 8: Verify — DONE

`agent_ui` 494, `listen` 50, `read_aloud` 273. `./script/clippy` exit 0 after
four runs — this workspace **denies** `redundant_clone` rather than warning, so
code whose tests pass will still fail the lint gate. `cargo build -p zed` exit 0.

The flaky `thread_metadata_store` migration test bit twice; passed alone both
times, as its own note says it will.

### Original task 8: Verify

- [ ] `cargo test -p listen -p agent_ui -p read_aloud`
- [ ] `./script/clippy` with a real exit-code capture — `status` is read-only in zsh, do not name a variable that
- [ ] `cargo fmt`
- [ ] `cargo build -p zed`
- [ ] Re-run any single `agent_ui` failure alone before believing it: `thread_metadata_store::tests::test_migrate_thread_remote_connections_backfills_from_workspace_db` races the shared workspaces DB
