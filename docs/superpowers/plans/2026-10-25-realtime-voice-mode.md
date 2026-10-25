# Real-time Voice Mode Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A hotkey opens a real-time spoken conversation with Echo over Inworld Realtime. You can interrupt Echo, it uses tools to steer, start, approve and stop sessions, it speaks up on its own when something happens, and the voice-glow shows who is talking.

**Architecture:** The `listen` crate gets the wire protocol, the socket, and the audio pumps. It still knows nothing about threads. The `agent_ui` crate gets a global `VoiceMode` entity that owns one connection for the whole app. It turns tool calls into the same thread-view and panel calls that typing uses, and turns thread events into notes for the model. The `ui` crate gets a GPUI port of the voice-glow, drawn in a new workspace overlay slot.

**Tech Stack:** Rust, GPUI, `async-tungstenite` through `gpui_tokio` (as `crates/client` does), rodio on the shared output mixer, `audio::EchoCanceller`, and the Inworld Realtime protocol (OpenAI Realtime events).

**Spec:** `docs/superpowers/specs/2026-10-25-realtime-voice-mode-design.md`. Read it first. This plan argues from it.

## Global Constraints

- Endpoint `wss://api.inworld.ai/api/v1/realtime/session?key=<session-id>&protocol=realtime`, header `Authorization: Basic <api-key>`.
- The key comes from `read_aloud::resolve_api_key` (env `INWORLD_API_KEY` or the keychain under `https://api.inworld.ai`).
- Audio in and out is base64 mono PCM16 at 24 kHz. Input is sent in 80 ms chunks (1920 samples).
- `turn_detection`: `{ type: "semantic_vad", eagerness: "medium", create_response: true, interrupt_response: true }`.
- Tools never block. Each returns a short status string at once.
- Never `rodio::Player::skip_one()` or `clear()` on the main thread. To stop, replace the player (commit `d9b8341e44`).
- `listen` must not depend on `acp_thread` or `agent_ui`.
- Echo's rules (`CLAUDE.md`): no `unwrap()`, no `let _ =` on fallible calls, no `mod.rs`, full-word names, and `./script/clippy` (which denies `redundant_clone`) must exit 0.
- Unprompted remarks are at least 20 s apart, except approvals and failures.
- Idle close after `idle_timeout_seconds` (default 300) with no speech and no running session. Hard close after 30 min without user speech.
- One reconnect on drop. A second drop within 30 s closes the mode.
- Hotkey `cmd-shift-space` → `agent::ToggleVoiceMode` (macOS keymap only).
- **Task 1 is a gate.** If the key lacks Realtime access, or the first audio of a reply takes more than 1 s after the end of speech, stop and report to Joshua. Do not fall back to another provider.

## Review Focus

The conditions most likely to bite a real user that the spec implies but doesn't spell out. Each has a test in the owning task.

1. **Echo hears itself.** If cancellation runs after the downmix, Echo's own voice triggers barge-in and it interrupts itself. Expected: cancellation runs on 48 kHz stereo before any downmix. Owner: Task 3, `mic_chain_cancels_before_downmixing`.
2. **Hotkey pressed again while connecting.** Expected: the mode closes cleanly, the mic is never opened, and the wake listener and narration are restored. Owner: Task 9, `toggling_while_connecting_closes_without_opening_the_mic`.
3. **The bound window closes while voice mode is open.** Expected: the mode closes and restores everything; nothing is left listening. Owner: Task 9, `closing_the_bound_window_closes_voice_mode`.
4. **A tool names a session that has since closed.** Expected: an error string the model can say out loud, and no panic. Owner: Task 10, `a_tool_naming_a_closed_session_returns_an_error_string`.
5. **An approval is clicked on screen between Echo's read-back and the spoken yes.** Expected: the yes doesn't approve the *next* prompt. The gate is keyed by tool call id and reports "already answered". Owner: Task 6, `a_confirmation_for_an_answered_prompt_does_not_carry_over`.

---

### Task 1: Probe Inworld Realtime (gate)

Throwaway spike. Only its fixtures are kept.

**Files:**
- Create (uncommitted): `/tmp/inworld-probe/probe.py`
- Create: `crates/listen/test_data/realtime/session_created.json`, `session_updated.json`, `speech_started.json`, `output_audio_delta.json`, `output_audio_transcript_delta.json`, `input_transcription_completed.json`, `function_call_arguments_done.json`, `response_done.json`, `error.json`
- Create: `crates/listen/test_data/realtime/PROBE.md` (results)

**Interfaces:**
- Produces: the fixtures Task 2 decodes; the default model id for Task 8 (`DEFAULT_REALTIME_MODEL`).

- [ ] **Step 1: Write the probe.** Python 3 with `websockets`, run as `uv run --with websockets python -I /tmp/inworld-probe/probe.py`.
  - Read the key from `INWORLD_API_KEY`, else `security find-internet-password -s api.inworld.ai -w`.
  - Connect to the endpoint with a random `key`. On `session.created`, send `session.update` with one function tool `ping(message: string)`, the turn detection above, and `audio.output.voice` from Echo's `read_aloud.voice_id` setting.
  - Send a `conversation.item.create` text "Say hi, then call ping with 'hello'." and a `response.create`.
  - Then send 3 s of a 24 kHz PCM16 WAV of speech from `say -o /tmp/inworld-probe/q.aiff "what time is it"`, converted with `afconvert -f WAVE -d LEI16@24000 -c 1`, in 80 ms appends.
  - Log every server event with a monotonic timestamp. Answer the first `function_call_arguments.done` with a `function_call_output`.
  - Record the time from the last audio append to the first `response.output_audio.delta`.
- [ ] **Step 2: Run it for each candidate model.** Try the model in the docs' example (`openai/gpt-4o-mini`) and any faster model Inworld lists for the key. If the docs page listing models needs a login, ask Joshua for the list.
- [ ] **Step 3: Gate.**
  - If the connection is refused for authorization or access, **stop**. Report the server's error to Joshua verbatim.
  - If no model has first audio within 1 s of the end of speech, **stop** and report the timings.
- [ ] **Step 4: Save fixtures.** Save one real event of each type listed under Files. Truncate base64 audio to under 200 characters and keep it valid base64.
- [ ] **Step 5: Write `PROBE.md`.** Record:
  - models tried, with first-audio latencies
  - the chosen default model
  - event names that differ from the spec's table, if any
  - the per-minute rate from Joshua's Inworld dashboard, if visible
- [ ] **Step 6: Commit**

```bash
git add crates/listen/test_data/realtime
git commit -m "listen: Record a real Inworld Realtime exchange"
```

---

### Task 2: Realtime protocol module

**Files:**
- Create: `crates/listen/src/realtime.rs`
- Modify: `crates/listen/src/listen.rs` (add `mod realtime; pub use realtime::*;`)

**Interfaces:**
- Produces:
  - `pub struct SessionConfig { pub model: String, pub voice: String, pub instructions: String, pub tools: Vec<ToolDefinition> }`
  - `pub struct ToolDefinition { pub name: &'static str, pub description: &'static str, pub parameters: serde_json::Value }`
  - `pub enum ClientEvent { SessionUpdate(SessionConfig), AppendAudio(Vec<i16>), UserText(String), FunctionOutput { call_id: String, output: String }, ResponseCreate, ResponseCancel, Truncate { item_id: String, content_index: u32, audio_end_ms: u32 } }`
  - `pub fn encode(event: &ClientEvent) -> anyhow::Result<String>`
  - `pub enum ServerEvent { SessionCreated, SpeechStarted, SpeechStopped, AudioDelta { item_id: String, content_index: u32, samples: Vec<i16> }, AssistantTranscriptDelta(String), UserTranscriptCompleted(String), FunctionCall { call_id: String, name: String, arguments: String }, ResponseDone, Error(String), Other }`
  - `pub fn decode(frame: &str) -> anyhow::Result<ServerEvent>`
  - `pub const REALTIME_SAMPLE_RATE: u32 = 24_000;` and `pub const REALTIME_URL: &str`

- [ ] **Step 1: Write the failing tests** in `realtime.rs`:

```rust
#[test]
fn every_recorded_server_event_decodes() {
    // include_str! each fixture; assert the variant, e.g.:
    assert!(matches!(decode(SPEECH_STARTED).unwrap(), ServerEvent::SpeechStarted));
    let ServerEvent::FunctionCall { call_id, name, .. } = decode(FUNCTION_CALL).unwrap() else { panic!() };
    assert!(!call_id.is_empty());
    assert_eq!(name, "ping");
}

#[test]
fn audio_round_trips_through_base64_pcm16() {
    let samples = vec![0i16, 1, -1, i16::MAX, i16::MIN];
    let frame = encode(&ClientEvent::AppendAudio(samples.clone())).unwrap();
    // decode the "audio" field back and compare to `samples`
}

#[test]
fn function_output_requires_a_call_id() {
    assert!(encode(&ClientEvent::FunctionOutput { call_id: String::new(), output: "ok".into() }).is_err());
}

#[test]
fn session_update_carries_the_pinned_turn_detection() {
    let json: serde_json::Value = serde_json::from_str(&encode(&ClientEvent::SessionUpdate(config())).unwrap()).unwrap();
    assert_eq!(json["session"]["audio"]["input"]["turn_detection"]["type"], "semantic_vad");
    assert_eq!(json["session"]["audio"]["input"]["turn_detection"]["interrupt_response"], true);
    assert_eq!(json["session"]["audio"]["input"]["format"]["rate"], 24000); // adjust to PROBE.md's accepted shape
}

#[test]
fn an_unknown_event_type_is_other_not_an_error() {
    assert!(matches!(decode(r#"{"type":"rate_limits.updated"}"#).unwrap(), ServerEvent::Other));
}
```

- [ ] **Step 2: Run them and confirm they fail.** `cargo test -p listen realtime` fails: `decode`/`encode` are not defined.
- [ ] **Step 3: Implement** with `serde_json::json!` for encoding and `serde_json::Value` matching on `type` for decoding. `UserText` encodes as `conversation.item.create` with `input_text`. Use field shapes from the fixtures where they differ from the spec, and note it in a comment.
- [ ] **Step 4: Run** `cargo test -p listen realtime`. Expect PASS.
- [ ] **Step 5: Commit** `listen: Speak the Inworld Realtime protocol`

---

### Task 3: Audio for real-time voice

**Files:**
- Create: `crates/listen/src/realtime_audio.rs`
- Modify: `crates/listen/src/listen.rs`, `crates/listen/Cargo.toml` (add `rodio.workspace = true` if missing)

**Interfaces:**
- Consumes: `audio::open_input_stream`, `audio::EchoCanceller`, `audio::{CHANNEL_COUNT, SAMPLE_RATE}`; the chain in `wake.rs:351-380` as the model.
- Produces:
  - `pub fn to_realtime_frame(cancelled_48k_stereo: &[f32]) -> Vec<i16>` downmixes to mono and resamples 48→24 kHz.
  - `pub fn rms(samples: &[i16]) -> f32` returns 0–1.
  - `pub struct VoiceLevels { input: AtomicU32, output: AtomicU32 }` with `set_input(f32)`, `set_output(f32)`, `input() -> f32`, `output() -> f32`. Uses `f32::to_bits`, and is shared as `Arc<VoiceLevels>`.
  - `pub struct Microphone` (drop stops capture) and `pub fn start_microphone(echo_canceller: audio::EchoCanceller, levels: Arc<VoiceLevels>) -> anyhow::Result<(Microphone, futures::channel::mpsc::UnboundedReceiver<Vec<i16>>)>`. It yields 1920-sample frames.
  - `pub struct VoicePlayback` with `new(mixer: rodio::mixer::Mixer, levels: Arc<VoiceLevels>)`, `append(&mut self, item_id: &str, content_index: u32, samples: Vec<i16>)`, `stop(&mut self) -> Option<PlayedAudio>`, and `is_playing(&self) -> bool`.
  - `pub struct PlayedAudio { pub item_id: String, pub content_index: u32, pub audio_end_ms: u32 }`

- [ ] **Step 1: Write the failing tests:**
  - `mic_chain_cancels_before_downmixing`: a fake canceller (a `process_stream` closure recording the buffer's channel count and rate) sees `CHANNEL_COUNT == 2` at `SAMPLE_RATE == 48_000`. Factor the chain as `fn cancel_then_convert(buffer, cancel: impl FnMut(&mut [f32]) -> Result<()>) -> Vec<i16>` so it is testable.
  - `to_realtime_frame_halves_the_sample_count`: 3840 interleaved stereo samples (20 ms at 48 kHz stereo is 1920) give 480 samples. Assert `input.len() / 2 / 2`.
  - `audio_end_ms_counts_what_was_played_not_received`: append 24 000 samples (1 s), advance the playback clock 400 ms (inject `played_samples: Arc<AtomicU64>` that the source increments; tests bump it), then `stop()` gives `audio_end_ms == 400`.
  - `stop_replaces_the_player`: after `stop()`, `append` plays again (`is_playing()` is true), and no `skip_one` is called (code review; the assert is behavioral).
  - `rms_of_silence_is_zero_and_full_scale_is_one`.
- [ ] **Step 2: Run** `cargo test -p listen realtime_audio`. Expect FAIL.
- [ ] **Step 3: Implement.**
  - Microphone: copy `wake.rs`'s `open_input_stream → constant_params → process_stream` chain, then `to_realtime_frame`, then buffer to 1920 samples.
  - Playback: a rodio `Player` connected to the mixer. Wrap each appended source in a counter that updates `played_samples` and the output RMS in `VoiceLevels`. `stop()` drops and reconnects the player.
  - Resampler: rodio's `UniformSourceIterator` or a 2:1 average. Either is acceptable since 48→24 is an exact 2:1 ratio.
- [ ] **Step 4: Run** `cargo test -p listen realtime_audio`. Expect PASS.
- [ ] **Step 5: Commit** `listen: Capture and play audio for real-time voice`

---

### Task 4: Realtime connection and provider seam

**Files:**
- Create: `crates/listen/src/realtime_session.rs`
- Modify: `crates/listen/Cargo.toml` (`async-tungstenite = { workspace = true, features = ["tokio", "tokio-rustls-manual-roots"] }`, `gpui_tokio.workspace = true`, `uuid.workspace = true`, `base64` if missing), `crates/listen/src/listen.rs`

**Interfaces:**
- Consumes: `encode`, `decode`, `ClientEvent`, `ServerEvent`, `SessionConfig` (Task 2).
- Produces:
  - `pub struct RealtimeConnection { pub sender: mpsc::UnboundedSender<ClientEvent>, pub events: mpsc::UnboundedReceiver<ServerEvent>, _task: gpui::Task<()> }`. Dropping it closes the socket. When the socket closes for any reason, `events` yields `ServerEvent::Error("closed: …")` and then ends.
  - `pub trait RealtimeVoice: 'static { fn connect(&self, config: SessionConfig, cx: &mut gpui::App) -> gpui::Task<anyhow::Result<RealtimeConnection>>; }`
  - `pub struct InworldRealtime { api_key: String }` with `new(api_key)`, implementing `RealtimeVoice`. It sends `SessionUpdate(config)` after `session.created`.
  - `#[cfg(any(test, feature = "test-support"))] pub struct FakeRealtime` with `connect` returning a connection whose other end the test holds. Methods: `fake.sent() -> Vec<ClientEvent>`, `fake.emit(ServerEvent)`, `fake.drop_connection()`, `fake.connections() -> usize`, `fake.fail_next_connect(message: &str)`. `ClientEvent` derives `Clone, Debug, PartialEq`.

- [ ] **Step 1: Write the failing tests:**
  - `fake_connection_delivers_events_and_records_sends`.
  - `dropping_the_fake_connection_ends_events_with_an_error`.
  - `#[ignore] inworld_realtime_connects_with_a_real_key` (needs `INWORLD_API_KEY`). It connects and expects `SessionCreated` within 5 s.
- [ ] **Step 2: Run** `cargo test -p listen realtime_session`. Expect FAIL.
- [ ] **Step 3: Implement `InworldRealtime`.**
  - Inside `gpui_tokio::Tokio::spawn_result`, build an `http::Request` with the `Authorization` header and open it with `async_tungstenite::tokio::connect_async`. Model it on `client.rs:1386-1450`.
  - One task reads frames and decodes them into the `events` channel. Another drains `sender`, encodes, and writes.
  - The tests use only `FakeRealtime`.
- [ ] **Step 4: Run** `cargo test -p listen realtime_session`, then `INWORLD_API_KEY=… cargo test -p listen realtime_session -- --ignored`. Expect PASS.
- [ ] **Step 5: Commit** `listen: Open an Inworld Realtime session`

---

### Task 5: Voice-mode settings

**Files:**
- Modify: `crates/settings_content/src/settings_content.rs` (add `pub voice_mode: Option<VoiceModeSettingsContent>` next to `listen`, and the struct after `ListenSettingsContent`), `crates/listen/src/listen.rs`, `assets/settings/default.json` (if `listen` has a block there, add a sibling)

**Interfaces:**
- Produces:
  - `pub struct VoiceModeSettings { pub provider: String, pub model: String, pub voice: Option<String>, pub chattiness: Chattiness, pub idle_timeout: Duration }`, registered in `listen::init`.
  - `pub enum Chattiness { Quiet, Milestones, Commentary }` with `Default = Milestones`.
  - `pub const DEFAULT_REALTIME_MODEL: &str = "<from PROBE.md>";`
  - Content fields, all `Option`: `provider`, `model`, `voice`, `chattiness` (`"quiet" | "milestones" | "commentary"`), `idle_timeout_seconds`.

- [ ] **Step 1: Write the failing tests:**
  - `voice_mode_defaults`: no block gives provider `inworld`, model `DEFAULT_REALTIME_MODEL`, voice `None`, `Milestones`, 300 s.
  - `a_zero_idle_timeout_falls_back_to_the_default`: 0 and negative values become 300 s.
  - `an_unknown_provider_is_reported` via `resolve_provider()`, mirroring `ListenSettings`.
- [ ] **Step 2: Run** `cargo test -p listen voice_mode`. Expect FAIL.
- [ ] **Step 3: Implement**, following `ListenSettings::from_settings`.
- [ ] **Step 4: Run** `cargo test -p listen voice_mode`. Expect PASS.
- [ ] **Step 5: Commit** `listen: Add voice-mode settings`

---

### Task 6: Pure voice-mode logic (session matcher, approval gate, feed)

**Files:**
- Create: `crates/agent_ui/src/voice_mode.rs` (module root; it only declares `mod feed; mod tools;` and re-exports in this task)
- Create: `crates/agent_ui/src/voice_mode/tools.rs`, `crates/agent_ui/src/voice_mode/feed.rs`
- Modify: `crates/agent_ui/src/agent_ui.rs` (`mod voice_mode;`)

**Interfaces:**
- Consumes: `voice_dispatch::{VoiceCandidate, pick_voice_target}`, `listen::{Chattiness, ToolDefinition}`.
- Produces in `tools.rs`:
  - `pub struct SessionSummary { pub session_id: acp::SessionId, pub title: SharedString, pub project: SharedString, pub status: SharedString, pub activity: Option<SharedString>, pub pending_approval: Option<SharedString>, pub candidate: VoiceCandidate }`
  - `pub enum SessionMatch { One(acp::SessionId), Ambiguous(Vec<SharedString>), NoMatch }`
  - `pub fn match_session(reference: &str, sessions: &[SessionSummary]) -> SessionMatch`. Empty or whitespace uses `pick_voice_target` over the candidates. Otherwise a case-insensitive word match on title: all reference words longer than 2 letters must appear. An exact title wins over partial matches.
  - `pub fn tool_definitions() -> Vec<ToolDefinition>`, the eight tools from the spec with their argument names: `session`, `message`, `task`, `agent`, `decision` (enum `allow_once`/`allow_always`/`reject`), `question`.
  - `pub fn state_block(sessions: &[SessionSummary]) -> String`, one line per session: `- <title> (<project>): <status>; <activity>; needs approval: <x>`.
  - `pub enum Decision { AllowOnce, AllowAlways, Reject }`
  - `pub enum GateOutcome { Apply, ReadBackFirst, AlreadyAnswered }`
  - `pub struct ApprovalGate` with `fn on_user_turn(&mut self)` and `fn request(&mut self, tool_call_id: &acp::ToolCallId, still_pending: bool, decision: Decision, confirm_approvals: bool) -> GateOutcome`.
- Produces in `feed.rs`:
  - `pub enum SessionEvent { ApprovalNeeded { session: SharedString, what: SharedString }, TurnFinished { session: SharedString, summary: SharedString }, Failed { session: SharedString, error: SharedString }, SubagentFinished { label: SharedString, failed: bool }, SidecarAnswer(SharedString), Milestone { session: SharedString, text: SharedString } }`
  - `pub struct Note { pub text: String, pub speak: bool }`
  - `pub struct Feed` with `new(chattiness: Chattiness)`, `push(&mut self, event: SessionEvent, now: Instant)`, `poll(&mut self, now: Instant, user_speaking: bool, echo_speaking: bool) -> Option<Note>`.
  - `pub const MIN_REMARK_GAP: Duration = Duration::from_secs(20);`

- [ ] **Step 1: Write the failing tests.**
  - tools.rs:
    - `an_exact_title_beats_a_partial_one`
    - `a_fragment_matches_by_words` ("booking" matches "Fix booking N+1")
    - `an_empty_reference_uses_the_voice_target` (a blocked session wins)
    - `two_partial_matches_are_ambiguous_and_list_titles`
    - `no_match_is_no_match`
    - `tool_definitions_name_all_eight_tools` (exact names: `list_sessions, peek_session, tell_session, start_session, answer_approval, stop_session, ask_sidecar, end_voice_mode`)
    - `reject_applies_without_a_read_back`
    - `allow_once_needs_a_user_turn_after_the_read_back_when_confirming` (first `request` gives `ReadBackFirst`, a second without `on_user_turn` gives `ReadBackFirst`, then `on_user_turn` and `request` gives `Apply`)
    - `allow_once_applies_at_once_when_confirm_is_off`
    - `allow_always_needs_a_read_back_even_when_confirm_is_off`
    - `a_confirmation_for_an_answered_prompt_does_not_carry_over` (the read-back is for call A, then `still_pending=false` for A gives `AlreadyAnswered`, then call B gives `ReadBackFirst`)
  - feed.rs:
    - `quiet_speaks_only_approvals_failures_and_sidecar_answers` (a `TurnFinished` gives a Note with `speak == false`)
    - `milestones_speaks_finished_turns_but_not_milestones`
    - `commentary_speaks_milestones`
    - `remarks_are_at_least_twenty_seconds_apart`
    - `approvals_and_failures_skip_the_gap`
    - `notes_in_the_gap_are_combined_into_one` (two `TurnFinished` give one Note whose text contains both)
    - `nothing_is_spoken_while_the_user_or_echo_is_talking` (the Note is held, not dropped, then delivered on the next quiet `poll`)
- [ ] **Step 2: Run** `cargo test -p agent_ui voice_mode`. Expect FAIL.
- [ ] **Step 3: Implement.**
  - Note texts follow the spec's table, bracketed: `[booking-fix needs approval: run migration on staging]`.
  - `ApprovalGate` stores `Option<acp::ToolCallId>` for "read back, awaiting turn" and a bool for "user turn since".
- [ ] **Step 4: Run** `cargo test -p agent_ui voice_mode`. Expect PASS.
- [ ] **Step 5: Commit** `agent_ui: Decide what voice mode says and which session it means`

---

### Task 7: Narration suppression in read-aloud

**Files:**
- Modify: `crates/read_aloud/src/read_aloud.rs`

**Interfaces:**
- Produces:
  - `pub fn set_suppressed(suppressed: bool, cx: &mut App)` (a `Global`)
  - `pub fn is_suppressed(cx: &App) -> bool`
  - These entry points return early without queueing while suppressed: `enqueue_markdown`, `narrate_message`, `narrate_tool_call`, `announce`, `finish_turn`, `catch_up`, `speculate_wrap_up`.

- [ ] **Step 1: Write the failing tests** (use the existing `ReadAloud::for_test` with `FakeTts` and `FakeSink`):
  - `suppressed_narration_is_dropped_not_queued`: `set_suppressed(true)`, then `narrate_message` gives `FakeSink` 0 queued and `FakeTts` 0 spoken.
  - `clearing_suppression_does_not_replay_the_backlog`: narrate while suppressed, `set_suppressed(false)`, `run_until_parked`, and nothing is spoken.
  - `a_reader_paused_by_hand_stays_paused_after_suppression`: `toggle_pause` before, suppress, unsuppress, and `playback_state.paused` stays true.
- [ ] **Step 2: Run** `cargo test -p read_aloud suppress`. Expect FAIL.
- [ ] **Step 3: Implement.** A `struct NarrationSuppressed(bool); impl Global`, with a guard at the top of each listed method. Stopping what's already playing is done by `VoiceMode` in Task 9 (calls `stop`), not here.
- [ ] **Step 4: Run** `cargo test -p read_aloud`. All pass.
- [ ] **Step 5: Commit** `read_aloud: Stay quiet while voice mode speaks`

---

### Task 8: Sidecar as its own entity

**Files:**
- Create: `crates/agent_ui/src/sidecar.rs`
- Modify: `crates/agent_ui/src/voice_dispatch.rs` (`Sidecar`, `sidecar_briefing`, `ensure_sidecar`, `spawn_sidecar`, `last_sidecar_answer` move out; `VoiceSession.sidecar` becomes `Option<Entity<Sidecar>>`), `crates/agent_ui/src/agent_ui.rs`

**Interfaces:**
- Consumes: `AgentConnection::new_session`, `subagent_notifications::transcript_path`.
- Produces:
  - `pub struct Sidecar` with `pub fn new(connection: Rc<dyn AgentConnection>, project: Entity<Project>, cx: &mut Context<Self>) -> Self`. Session creation starts immediately.
  - `pub fn ask(&mut self, question: String, about: Option<acp::SessionId>, cx: &mut Context<Self>) -> Task<Result<String>>`. This awaits session creation and never falls back.
  - `pub fn is_ready(&self) -> bool`
  - `pub fn sidecar_briefing(transcript: Option<&Path>) -> String` (moved unchanged)
  - `pub fn rebriefing(transcript: Option<&Path>) -> String`, one paragraph: "The question is now about a different session …" plus the transcript sentence.

- [ ] **Step 1: Write the failing tests** with `FakeAgentConnection`:
  - `the_first_question_waits_for_the_session_instead_of_falling_back`
  - `the_briefing_is_sent_once` (the first prompt contains the `sidecar_briefing` text, the second doesn't)
  - `asking_about_a_different_session_rebriefs` (`about` A, then B: the second prompt starts with `rebriefing`)
  - `the_answer_is_the_last_assistant_message`
  - Keep `voice_dispatch`'s existing `sidecar_tests` passing (moved with the function).
- [ ] **Step 2: Run** `cargo test -p agent_ui sidecar`. Expect FAIL.
- [ ] **Step 3: Implement.** Port `ask_sidecar`'s body into `Sidecar::ask`. In the wake path, `ensure_sidecar` becomes "create the entity if missing". Keep its current first-question fallback to the agent there: the wake path's behavior doesn't change. Only voice mode relies on `ask` waiting.
- [ ] **Step 4: Run** `cargo test -p agent_ui sidecar voice_dispatch`. Expect PASS.
- [ ] **Step 5: Commit** `agent_ui: Let any voice path own a sidecar`

---

### Task 9: VoiceMode lifecycle, overlay, glow, and talking with no tools

**Files:**
- Create: `crates/ui/src/components/ai/voice_glow.rs` (and export it in `crates/ui/src/components/ai.rs`)
- Modify: `crates/agent_ui/src/voice_mode.rs` (the `VoiceMode` global entity and `VoiceOverlay` view)
- Modify: `crates/workspace/src/workspace.rs`
  - field `voice_overlay: Option<AnyView>`
  - `pub fn set_voice_overlay(&mut self, overlay: Option<AnyView>, cx: &mut Context<Self>)`
  - rendered after `toast_layer`, absolutely positioned at bottom 0, full width, not blocking mouse input except on the pill
- Modify: `crates/agent_ui/src/agent_ui.rs` (`ToggleVoiceMode` action; register the handler in `init`), `crates/agent_ui/src/voice_dispatch.rs` (`pub(crate) fn stop_listening(&mut self)` sets `self.voice = None`; `start_listening` already exists; `say_to_the_user` checks `VoiceMode::is_open(cx)` and forwards with `VoiceMode::note(line, cx)`)
- Modify: `assets/keymaps/default-macos.json` (global context: `"cmd-shift-space": "agent::ToggleVoiceMode"`)

**Interfaces:**
- Consumes: Tasks 2–5, 7. `read_aloud::{resolve_api_key, set_suppressed, ReadAloudSettings}`, `audio::{Audio::output_mixer, Audio::echo_canceller}`, `thread_worktree_archive::all_open_workspaces`.
- Produces:
  - `pub struct VoiceGlow` (a `RenderOnce` element): `VoiceGlow::new(levels: Arc<listen::VoiceLevels>)`.
  - Pure helpers in `voice_glow.rs`: `pub fn follow(current: f32, target: f32, dt: f32, attack: f32, release: f32) -> f32` and `pub fn lobe_paint(lobe_index: usize, phase: f32, bands_input: [f32; 3], bands_output: [f32; 3], elapsed: f32, scale: f32) -> LobePaint { x: f32, scale_x: f32, scale_y: f32, opacity: f32 }`. The constants (`LOBES`, `ATTACK`, `RELEASE`, `GAIN = 2.5`, `DESIGN_WIDTH = 350`) are copied from `ryse-inc/landing/components/ui/voice-glow.tsx`.
  - `pub enum VoiceModeState { Connecting, Live, Closing }`
  - `pub struct VoiceMode` (a global `Entity` via `impl Global for GlobalVoiceMode(Entity<VoiceMode>)`):
    - `pub fn toggle(window: &mut Window, cx: &mut App)`
    - `pub fn is_open(cx: &App) -> bool`
    - `pub fn note(text: impl Into<SharedString>, cx: &mut App)` (sends `UserText` + `ResponseCreate`)
    - `fn close(&mut self, reason: Option<SharedString>, cx: &mut Context<Self>)`
  - `VoiceMode::new_for_test(provider: Rc<dyn listen::RealtimeVoice>, cx)` so tests inject `FakeRealtime`.

- [ ] **Step 1: Write the failing tests.**
  - glow, in `voice_glow.rs`:
    - `follow_attacks_faster_than_it_releases`
    - `lobe_paint_matches_the_typescript_original`: lobe 0, phase 0, input bands `[0.5; 3]`, elapsed 0, scale 1 gives the x, scales and opacity computed by hand from the TS formulas. Write the numbers into the test.
  - voice mode, in `voice_mode.rs` with `FakeRealtime`, `FakeAgentConnection`, a test workspace, and `listen.enabled` on with `FakeWake`:
    - `opening_suppresses_narration_and_suspends_listening`: after toggle, `read_aloud::is_suppressed`, every panel's `voice.is_none()`, and the state is `Connecting` then `Live` after `SessionCreated`.
    - `closing_restores_narration_and_listening`
    - `toggling_while_connecting_closes_without_opening_the_mic` (inject a microphone factory that counts opens; it must be 0)
    - `closing_the_bound_window_closes_voice_mode`
    - `assistant_audio_is_played_and_speech_interrupts_it`: emit `AudioDelta`, then `SpeechStarted`. Sent events contain `Truncate { audio_end_ms }` equal to the playback's played ms, and playback is stopped.
    - `say_to_the_user_becomes_a_note_while_open`: `UserText("Approved.")` is sent and `FakeTts` speaks nothing.
    - `no_api_key_shows_a_toast_and_stays_closed`
- [ ] **Step 2: Run** `cargo test -p ui voice_glow` and `cargo test -p agent_ui voice_mode`. Expect FAIL.
- [ ] **Step 3: Implement.**
  - **Toggle:** resolve the key. Build `SessionConfig`:
    - model and voice from settings, voice falling back to `ReadAloudSettings::voice_id`
    - instructions from `persona()` plus `state_block`
    - tools empty in this task
  - **Opening:** suppress narration and `stop` every reader. Call `stop_listening` on every `AgentPanel` across `all_open_workspaces`. Install the overlay on the bound window's workspace. Connect. On `SessionCreated`, start the microphone and pump frames as `AppendAudio`.
  - **Events:**
    - `AudioDelta` goes to `VoicePlayback::append`.
    - `SpeechStarted` while `is_playing`: `stop()`, then send `Truncate`.
    - Transcript deltas update the caption.
  - **Closing** reverses each opening step, plays the close sound, and calls `start_listening` on each panel.
  - **Overlay:** `VoiceGlow` about 120 px high, a caption that fades after 4 s, and a status pill with ✕ that calls `toggle`.
  - **`persona()`:** the spec's Persona bullets as a `const &str`. Joshua's name stays in it.
  - **Glow paint:** each lobe is a rounded `div` with a large `box_shadow` blur and absolute transform. Call `window.request_animation_frame()` while open. If `cx.reduce_motion()`, paint the resting shape and don't request frames.
- [ ] **Step 4: Run** `cargo test -p ui voice_glow`, `cargo test -p agent_ui voice_mode`, `cargo test -p workspace`. Expect PASS.
- [ ] **Step 5: Manual check with a real key.**
  - Run `cargo run -p zed --bin zedecho`. Press cmd-shift-space, talk, and confirm Echo answers within about 1 s.
  - Interrupt it mid-sentence. It stops.
  - Play narration-length audio from Echo while silent and confirm it doesn't interrupt itself.
  - If macOS swallows cmd-shift-space, pick another binding and note it in the commit.
- [ ] **Step 6: Run `./script/clippy`.** Exit 0.
- [ ] **Step 7: Commit** `agent_ui: Talk to Echo in real time`

---

### Task 10: Voice-mode tools

**Files:**
- Modify: `crates/agent_ui/src/voice_mode.rs` (dispatch), `crates/agent_ui/src/voice_mode/tools.rs` (if helpers are needed)
- Modify: `crates/agent_ui/src/agent_panel.rs`: add `pub(crate) fn start_session_by_voice(&mut self, task: String, agent: Option<crate::Agent>, window: &mut Window, cx: &mut Context<Self>)`. It calls `external_thread(agent, None, None, None, Some(AgentInitialContent::ContentBlock { blocks: vec![text(task)], auto_submit: true }), true, AgentThreadSource::<existing voice-ish variant or Panel>, window, cx)`.

**Interfaces:**
- Consumes:
  - Task 6: `match_session`, `tool_definitions`, `ApprovalGate`, `SessionSummary`
  - Task 8: `Sidecar::ask`
  - `ThreadView::{send_voice_text, authorize_pending_tool_call, cancel_generation, pending_tool_call_description, voice_candidate}`
  - `narration_peek::current_activity`, `AcpThread::recent_narration`
- Produces: `fn handle_function_call(&mut self, call_id: String, name: String, arguments: String, window: &mut Window, cx: &mut Context<Self>)`. It always sends exactly one `FunctionOutput` for `call_id`.

- [ ] **Step 1: Write the failing tests** with `FakeRealtime` emitting `FunctionCall`:
  - `tell_session_sends_to_the_named_thread` (the fake thread receives the text)
  - `start_session_opens_a_thread_with_the_task_as_its_first_prompt`
  - `stop_session_cancels_the_turn`
  - `answer_approval_reads_back_then_applies_after_the_users_turn` (`FunctionOutput` asks to confirm, then `UserTranscriptCompleted`, a second call, and the tool call is authorized)
  - `ask_sidecar_returns_at_once_and_the_answer_arrives_as_a_note` (the `FunctionOutput` is "asking", and a later `UserText` contains `[sidecar answer:`)
  - `an_ambiguous_session_returns_the_candidates`
  - `a_tool_naming_a_closed_session_returns_an_error_string`
  - `unparsable_arguments_return_an_error_string`
  - `end_voice_mode_closes_after_its_output_is_sent`
  - `every_function_call_gets_exactly_one_output`
- [ ] **Step 2: Run** `cargo test -p agent_ui voice_mode`. Expect FAIL.
- [ ] **Step 3: Implement.**
  - Build `Vec<SessionSummary>` from every `ThreadView` across `all_open_workspaces`, including subagents.
  - Dispatch by name, and send `FunctionOutput` before any async work.
  - Session-changing tools run inside the bound window via `window.update`.
  - Set `tools: tool_definitions()` in the `SessionConfig`.
  - Re-send `SessionUpdate` with a fresh `state_block` when the set of sessions or their status changes. Debounce this to at most once per 2 s.
- [ ] **Step 4: Run** `cargo test -p agent_ui voice_mode`. Expect PASS.
- [ ] **Step 5: Manual check.** By voice:
  - steer a running session
  - start a new one
  - approve a prompt (with the read-back)
  - stop a session
  - ask "why did it do X"
- [ ] **Step 6: Run `./script/clippy`.** Exit 0.
- [ ] **Step 7: Commit** `agent_ui: Let voice mode steer, start, approve, and stop sessions`

---

### Task 11: Proactive remarks, timeouts, and reconnect

**Files:**
- Modify: `crates/agent_ui/src/voice_mode.rs`

**Interfaces:**
- Consumes: Task 6 `Feed`, `SessionEvent`; `listen::VoiceModeSettings`.
- Produces:
  - Subscriptions to every `AcpThread` across open panels while voice mode is open. These map thread status changes, permission requests, errors, and subagent completions to `SessionEvent`.
  - A milestone is an assistant message after at least 60 s of tool calls.
  - A 1 s poll task drives `Feed::poll` and sends `UserText` + `ResponseCreate` when `speak`, or only `UserText` otherwise.

- [ ] **Step 1: Write the failing tests:**
  - `a_finished_turn_is_spoken_at_milestones`
  - `quiet_keeps_finished_turns_in_context_only` (`UserText` sent with no `ResponseCreate`)
  - `idle_timeout_closes_when_nothing_is_running` (advance the clock 300 s with no speech)
  - `a_running_session_keeps_it_open_until_thirty_minutes_without_user_speech`
  - `one_drop_reconnects_with_the_recent_exchanges` (`connections() == 2`, and the second config's instructions contain the last exchange)
  - `a_second_drop_within_thirty_seconds_closes_with_a_toast`
- [ ] **Step 2: Run** `cargo test -p agent_ui voice_mode`. Expect FAIL.
- [ ] **Step 3: Implement.**
  - Track `last_user_speech: Instant` and `last_any_speech: Instant`.
  - Keep the last 4 exchanges, pairing user transcripts with assistant transcripts, for the reconnect summary.
  - Use `cx.background_executor().timer` for the clocks, so tests drive them with `advance_clock`.
- [ ] **Step 4: Run** `cargo test -p agent_ui voice_mode`. Expect PASS.
- [ ] **Step 5: Final checks.**
  - `./script/clippy` exits 0.
  - `cargo test -p listen -p read_aloud -p agent_ui -p ui -p workspace` passes. Re-run alone any flaky thread-metadata test before blaming the change.
- [ ] **Step 6: Manual check.** Leave voice mode open while a session works. Echo mentions the finish and an approval, and stays quiet otherwise. Wait 5 idle minutes and it closes with the sound.
- [ ] **Step 7: Commit** `agent_ui: Speak up when sessions need you, and close when idle`
