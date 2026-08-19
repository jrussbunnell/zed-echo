# Voice Input Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give Echo an ear — an always-on wake word, a spoken command, and a dispatch into the steering, authorization, and narration plumbing the fork already has.

**Architecture:** A new `listen` crate mirrors `read_aloud`: an `SttProvider` seam with a `FakeStt` double, a `Listener` entity running a `Idle → Woke → Capturing → Dispatched` state machine, and an `intent` parser turning a transcript into a `VoiceCommand`. The crate emits events and knows nothing about threads; `agent_ui` owns the dispatch, because routing needs threads, subagents, and the approval queue. `AgentPanel` owns the single `Listener` — `ReadAloud` is per-`ThreadView`, so anything that routes *between* threads has to sit above them.

**Tech Stack:** Rust, GPUI entities and events, `rodio` microphone capture via the existing `audio` crate, Inworld streaming STT over `http_client`, `objc2-speech` for the on-device wake word.

**Spec:** `docs/superpowers/specs/2026-08-19-voice-input-design.md`

## Global Constraints

- Rust 1.97.1, via `~/.cargo/bin/cargo`. Build checks use `./script/clippy`, not `cargo clippy`.
- No `mod.rs`. New crates declare `[lib] path = "src/<name>.rs"` in `Cargo.toml`.
- No `unwrap()`, no `expect()`, no panicking indexing on non-test paths. Propagate with `?`.
- Never `let _ =` a fallible operation. Use `?`, `.log_err()`, or explicit `match`.
- Comments explain *why*, never *what*. No organizational or summary comments.
- Full words in identifiers. No abbreviations.
- In GPUI tests use `cx.background_executor().timer(..)`, never `smol::Timer::after`.
- The wake listener is `#[cfg(target_os = "macos")]`, matching `read_aloud::SystemTts`.
- Every deliberate simplification with a known ceiling gets a `ponytail:` comment naming the ceiling and the upgrade path.

---

### Task 1: Crate skeleton, `SttProvider` seam, and `FakeStt`

**Files:**
- Create: `crates/listen/Cargo.toml`
- Create: `crates/listen/src/listen.rs`
- Create: `crates/listen/src/provider.rs`
- Modify: `Cargo.toml` (workspace members ~line 170, workspace deps ~line 437)

**Interfaces:**
- Consumes: nothing.
- Produces: `listen::SttProvider`, `listen::Transcript`, `listen::FakeStt`.
  - `trait SttProvider: Send + Sync + 'static { fn transcribe(&self, audio: mpsc::UnboundedReceiver<Vec<f32>>, cx: &App) -> mpsc::UnboundedReceiver<Result<Transcript>>; }`
  - `struct Transcript { pub text: String, pub is_final: bool }`
  - `FakeStt::new()`, `FakeStt::queue_transcript(&self, text: &str)`, `FakeStt::queue_partial_then_final(&self, partial: &str, final_text: &str)`, `FakeStt::fail_next(&self)`, `FakeStt::heard(&self) -> usize`

- [ ] **Step 1: Write the failing test**

In `crates/listen/src/provider.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt as _;
    use gpui::TestAppContext;

    #[gpui::test]
    async fn fake_provider_returns_the_queued_transcript(cx: &mut TestAppContext) {
        let provider = FakeStt::new();
        provider.queue_transcript("stop the agent");
        let (audio_sender, audio) = mpsc::unbounded();
        drop(audio_sender);

        let mut transcripts = cx.update(|cx| provider.transcribe(audio, cx));
        let first = transcripts.next().await.unwrap().unwrap();

        assert_eq!(first.text, "stop the agent");
        assert!(first.is_final);
    }

    #[gpui::test]
    async fn fake_provider_streams_a_partial_before_the_final(cx: &mut TestAppContext) {
        let provider = FakeStt::new();
        provider.queue_partial_then_final("stop the", "stop the agent");
        let (audio_sender, audio) = mpsc::unbounded();
        drop(audio_sender);

        let mut transcripts = cx.update(|cx| provider.transcribe(audio, cx));
        let partial = transcripts.next().await.unwrap().unwrap();
        let final_transcript = transcripts.next().await.unwrap().unwrap();

        assert_eq!(partial.text, "stop the");
        assert!(!partial.is_final);
        assert_eq!(final_transcript.text, "stop the agent");
        assert!(final_transcript.is_final);
    }

    #[gpui::test]
    async fn fake_provider_can_be_told_to_fail(cx: &mut TestAppContext) {
        let provider = FakeStt::new();
        provider.fail_next();
        let (audio_sender, audio) = mpsc::unbounded();
        drop(audio_sender);

        let mut transcripts = cx.update(|cx| provider.transcribe(audio, cx));
        assert!(transcripts.next().await.unwrap().is_err());
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `~/.cargo/bin/cargo test -p listen`
Expected: FAIL — the package does not exist yet.

- [ ] **Step 3: Create the crate manifest**

`crates/listen/Cargo.toml`:

```toml
[package]
name = "listen"
version = "0.1.0"
edition.workspace = true
publish.workspace = true
license = "GPL-3.0-or-later"

[lints]
workspace = true

[features]
test-support = []

[lib]
path = "src/listen.rs"
doctest = false

[dependencies]
anyhow.workspace = true
audio.workspace = true
futures.workspace = true
gpui.workspace = true
http_client.workspace = true
log.workspace = true
serde_json.workspace = true
settings.workspace = true
util.workspace = true

[dev-dependencies]
gpui = { workspace = true, features = ["test-support"] }
http_client = { workspace = true, features = ["test-support"] }
settings = { workspace = true, features = ["test-support"] }
```

Register in the root `Cargo.toml`: add `"crates/listen",` to `[workspace] members` in sorted position, and `listen = { path = "crates/listen" }` to `[workspace.dependencies]` in sorted position.

- [ ] **Step 4: Write the provider seam**

`crates/listen/src/provider.rs`:

```rust
use anyhow::{Result, anyhow};
use futures::StreamExt as _;
use futures::channel::mpsc;
use gpui::{App, AppContext as _};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// One refinement of what the user said. A provider emits as many of these
/// as it likes, and exactly one with `is_final` set.
#[derive(Debug, Clone, PartialEq)]
pub struct Transcript {
    pub text: String,
    pub is_final: bool,
}

/// One seam so no speech vendor is welded into the editor, mirroring
/// [`read_aloud::TtsProvider`] on the other half of the conversation.
pub trait SttProvider: Send + Sync + 'static {
    /// Transcribes one utterance from a stream of mono `f32` frames.
    ///
    /// Transcripts are yielded as the provider refines them so a command can
    /// dispatch on the provider's own endpointing rather than after a full
    /// round trip. Dropping the receiver cancels the work; closing `audio`
    /// signals the end of the utterance.
    fn transcribe(
        &self,
        audio: mpsc::UnboundedReceiver<Vec<f32>>,
        cx: &App,
    ) -> mpsc::UnboundedReceiver<Result<Transcript>>;
}

#[derive(Default)]
struct FakeSttState {
    queued: VecDeque<Vec<Result<Transcript>>>,
    fail_next: bool,
    heard: usize,
}

/// Test double. Replays scripted transcripts without touching a microphone
/// or the network, the way `read_aloud::FakeTts` replays synthesis.
#[derive(Clone, Default)]
pub struct FakeStt {
    state: Arc<Mutex<FakeSttState>>,
}

impl FakeStt {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queues one utterance answered by a single final transcript.
    pub fn queue_transcript(&self, text: &str) {
        self.queue(vec![Ok(Transcript {
            text: text.to_string(),
            is_final: true,
        })]);
    }

    /// Queues one utterance answered by a partial and then a final, so tests
    /// can exercise the refinement path a real provider takes.
    pub fn queue_partial_then_final(&self, partial: &str, final_text: &str) {
        self.queue(vec![
            Ok(Transcript {
                text: partial.to_string(),
                is_final: false,
            }),
            Ok(Transcript {
                text: final_text.to_string(),
                is_final: true,
            }),
        ]);
    }

    pub fn fail_next(&self) {
        match self.state.lock() {
            Ok(mut state) => state.fail_next = true,
            Err(error) => log::error!("listen: FakeStt state poisoned: {error}"),
        }
    }

    /// How many utterances this provider has been asked to transcribe.
    pub fn heard(&self) -> usize {
        self.state.lock().map(|state| state.heard).unwrap_or(0)
    }

    fn queue(&self, transcripts: Vec<Result<Transcript>>) {
        match self.state.lock() {
            Ok(mut state) => state.queued.push_back(transcripts),
            Err(error) => log::error!("listen: FakeStt state poisoned: {error}"),
        }
    }
}

impl SttProvider for FakeStt {
    fn transcribe(
        &self,
        mut audio: mpsc::UnboundedReceiver<Vec<f32>>,
        cx: &App,
    ) -> mpsc::UnboundedReceiver<Result<Transcript>> {
        let (sender, receiver) = mpsc::unbounded();
        let scripted = {
            let Ok(mut state) = self.state.lock() else {
                sender
                    .unbounded_send(Err(anyhow!("FakeStt state poisoned")))
                    .ok();
                return receiver;
            };
            state.heard += 1;
            if std::mem::take(&mut state.fail_next) {
                sender
                    .unbounded_send(Err(anyhow!("FakeStt was told to fail")))
                    .ok();
                return receiver;
            }
            state.queued.pop_front()
        };

        cx.background_spawn(async move {
            // Drain the audio so a caller that feeds frames is not left
            // pushing into a channel nobody reads.
            while audio.next().await.is_some() {}
            let Some(scripted) = scripted else {
                sender
                    .unbounded_send(Err(anyhow!("FakeStt had no transcript queued")))
                    .ok();
                return;
            };
            for transcript in scripted {
                if sender.unbounded_send(transcript).is_err() {
                    return;
                }
            }
        })
        .detach();

        receiver
    }
}
```

`crates/listen/src/listen.rs`:

```rust
mod provider;

pub use provider::{FakeStt, SttProvider, Transcript};
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `~/.cargo/bin/cargo test -p listen`
Expected: PASS — 3 tests.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml crates/listen
git commit -m "listen: Add the seam a speech provider plugs into"
```

---

### Task 2: Intent parsing

**Files:**
- Create: `crates/listen/src/intent.rs`
- Modify: `crates/listen/src/listen.rs`

**Interfaces:**
- Consumes: nothing from Task 1.
- Produces: `listen::VoiceCommand`, `listen::parse_intent(transcript: &str, wake_word: &str) -> VoiceCommand`.

`parse_intent` strips a leading wake word if present, then matches. The wake word is passed rather than assumed because it is configurable.

- [ ] **Step 1: Write the failing test**

In `crates/listen/src/intent.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> VoiceCommand {
        parse_intent(text, "echo")
    }

    #[test]
    fn the_wake_word_is_stripped_before_matching() {
        assert_eq!(parse("Echo, approve"), VoiceCommand::Approve);
        assert_eq!(parse("echo approve"), VoiceCommand::Approve);
        assert_eq!(parse("approve"), VoiceCommand::Approve);
    }

    #[test]
    fn affirmatives_and_negatives_answer_a_prompt() {
        for text in ["approve", "yes", "go ahead", "allow it", "do it"] {
            assert_eq!(parse(text), VoiceCommand::Approve, "{text}");
        }
        for text in ["deny", "no", "reject", "don't"] {
            assert_eq!(parse(text), VoiceCommand::Deny, "{text}");
        }
    }

    /// The single most consequential split in the grammar. Bare "stop" is the
    /// recoverable reading — quiet down — because a listener who meant to
    /// halt the agent can say so again, while one who lost a turn's work
    /// cannot get it back.
    #[test]
    fn bare_stop_quiets_narration_and_only_an_explicit_phrase_halts_the_agent() {
        assert_eq!(parse("stop"), VoiceCommand::Pause);
        assert_eq!(parse("stop talking"), VoiceCommand::Pause);
        assert_eq!(parse("be quiet"), VoiceCommand::Pause);

        assert_eq!(parse("stop the agent"), VoiceCommand::Interrupt);
        assert_eq!(parse("cancel that"), VoiceCommand::Interrupt);
        assert_eq!(parse("interrupt"), VoiceCommand::Interrupt);
    }

    #[test]
    fn narration_control_maps_to_its_verbs() {
        assert_eq!(parse("resume"), VoiceCommand::Resume);
        assert_eq!(parse("keep going"), VoiceCommand::Resume);
        assert_eq!(parse("say that again"), VoiceCommand::Repeat);
        assert_eq!(parse("repeat"), VoiceCommand::Repeat);
        assert_eq!(parse("catch me up"), VoiceCommand::CatchUp);
        assert_eq!(parse("skip ahead"), VoiceCommand::Next);
        assert_eq!(parse("go back"), VoiceCommand::Previous);
        assert_eq!(parse("never mind"), VoiceCommand::Never);
    }

    #[test]
    fn anything_unmatched_is_something_to_say_to_the_agent() {
        assert_eq!(
            parse("Echo, check the tests before you refactor"),
            VoiceCommand::Say("check the tests before you refactor".to_string())
        );
    }

    /// A transcript that is only a wake word is the provider hearing its own
    /// name and nothing else. Dispatching it as a message would send the word
    /// "echo" to the agent.
    #[test]
    fn a_transcript_that_is_only_the_wake_word_is_not_a_command() {
        assert_eq!(parse("Echo"), VoiceCommand::Never);
        assert_eq!(parse("  echo,  "), VoiceCommand::Never);
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `~/.cargo/bin/cargo test -p listen intent`
Expected: FAIL — `intent` module not found.

- [ ] **Step 3: Write the parser**

`crates/listen/src/intent.rs`:

```rust
/// What the user asked for.
///
/// `Say` carries free text; whether it steers a running turn or starts a new
/// one is not decided here, because that depends on thread state this crate
/// deliberately cannot see.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VoiceCommand {
    Say(String),
    Approve,
    Deny,
    Pause,
    Resume,
    Repeat,
    CatchUp,
    Next,
    Previous,
    /// Halt the agent's current turn.
    Interrupt,
    /// Heard something, but there is nothing to do with it.
    Never,
}

/// Phrases matched whole, after the wake word and punctuation are stripped.
///
/// ponytail: a keyword table, not a model. A model round-trip would add about
/// a second to "stop", the one command whose whole value is being instant.
/// Classification by model is the upgrade if this proves brittle in use.
const EXACT: &[(&str, VoiceCommand)] = &[
    ("approve", VoiceCommand::Approve),
    ("approved", VoiceCommand::Approve),
    ("yes", VoiceCommand::Approve),
    ("yep", VoiceCommand::Approve),
    ("yeah", VoiceCommand::Approve),
    ("ok", VoiceCommand::Approve),
    ("okay", VoiceCommand::Approve),
    ("go ahead", VoiceCommand::Approve),
    ("allow it", VoiceCommand::Approve),
    ("allow", VoiceCommand::Approve),
    ("do it", VoiceCommand::Approve),
    ("deny", VoiceCommand::Deny),
    ("denied", VoiceCommand::Deny),
    ("no", VoiceCommand::Deny),
    ("nope", VoiceCommand::Deny),
    ("reject", VoiceCommand::Deny),
    ("don't", VoiceCommand::Deny),
    ("do not", VoiceCommand::Deny),
    ("stop the agent", VoiceCommand::Interrupt),
    ("stop the turn", VoiceCommand::Interrupt),
    ("cancel that", VoiceCommand::Interrupt),
    ("cancel the turn", VoiceCommand::Interrupt),
    ("interrupt", VoiceCommand::Interrupt),
    ("abort", VoiceCommand::Interrupt),
    ("stop", VoiceCommand::Pause),
    ("stop talking", VoiceCommand::Pause),
    ("quiet", VoiceCommand::Pause),
    ("be quiet", VoiceCommand::Pause),
    ("pause", VoiceCommand::Pause),
    ("hush", VoiceCommand::Pause),
    ("resume", VoiceCommand::Resume),
    ("continue", VoiceCommand::Resume),
    ("keep going", VoiceCommand::Resume),
    ("carry on", VoiceCommand::Resume),
    ("repeat", VoiceCommand::Repeat),
    ("again", VoiceCommand::Repeat),
    ("say that again", VoiceCommand::Repeat),
    ("what was that", VoiceCommand::Repeat),
    ("catch me up", VoiceCommand::CatchUp),
    ("catch up", VoiceCommand::CatchUp),
    ("where are we", VoiceCommand::CatchUp),
    ("what's the status", VoiceCommand::CatchUp),
    ("skip", VoiceCommand::Next),
    ("skip ahead", VoiceCommand::Next),
    ("next", VoiceCommand::Next),
    ("go back", VoiceCommand::Previous),
    ("back up", VoiceCommand::Previous),
    ("previous", VoiceCommand::Previous),
    ("never mind", VoiceCommand::Never),
    ("nevermind", VoiceCommand::Never),
    ("forget it", VoiceCommand::Never),
];

/// Turns one final transcript into a command.
///
/// A leading wake word is stripped: a provider that heard "Echo, approve"
/// and one triggered by a wake listener that already consumed the word both
/// arrive here, and both must mean the same thing.
pub fn parse_intent(transcript: &str, wake_word: &str) -> VoiceCommand {
    let spoken = strip_wake_word(transcript, wake_word);
    let normalized = normalize(spoken);

    if normalized.is_empty() {
        return VoiceCommand::Never;
    }

    for (phrase, command) in EXACT {
        if normalized == *phrase {
            return command.clone();
        }
    }

    VoiceCommand::Say(spoken.trim().to_string())
}

/// Removes a leading wake word and whatever punctuation followed it, leaving
/// the original casing of the rest — a message to the agent is quoted, not
/// normalized.
fn strip_wake_word<'a>(transcript: &'a str, wake_word: &str) -> &'a str {
    let trimmed = transcript.trim();
    let wake_word = wake_word.trim();
    if wake_word.is_empty() {
        return trimmed;
    }
    let Some(remainder) = trimmed
        .get(..wake_word.len())
        .filter(|head| head.eq_ignore_ascii_case(wake_word))
        .and_then(|_| trimmed.get(wake_word.len()..))
    else {
        return trimmed;
    };
    // Only a word boundary counts, so "echoing the change" keeps its first
    // word rather than becoming "ing the change".
    if remainder
        .chars()
        .next()
        .is_some_and(|character| character.is_alphanumeric())
    {
        return trimmed;
    }
    remainder
        .trim_start_matches([',', '.', '!', '?', ':', ';', '—', '-'])
        .trim()
}

/// Lowercases, drops trailing punctuation, and collapses whitespace, so the
/// table can be written the way a person would say the phrase.
fn normalize(spoken: &str) -> String {
    spoken
        .trim()
        .trim_end_matches(['.', '!', '?', ',', ';', ':'])
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}
```

Add to `crates/listen/src/listen.rs`:

```rust
mod intent;
mod provider;

pub use intent::{VoiceCommand, parse_intent};
pub use provider::{FakeStt, SttProvider, Transcript};
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `~/.cargo/bin/cargo test -p listen`
Expected: PASS — 9 tests.

- [ ] **Step 5: Commit**

```bash
git add crates/listen
git commit -m "listen: Turn what was said into what was meant"
```

---

### Task 3: Settings

**Files:**
- Modify: `crates/settings_content/src/settings_content.rs` (add `listen` field beside `read_aloud` at ~line 210; add the content struct beside `ReadAloudSettingsContent` at ~line 586)
- Modify: `crates/listen/src/listen.rs`
- Modify: `assets/settings/default.json`
- Modify: `crates/listen/Cargo.toml` (no change if `settings` already present)

**Interfaces:**
- Consumes: nothing.
- Produces: `listen::ListenSettings` with fields `enabled: bool`, `provider: String`, `wake_word: String`, `confirm_approvals: bool`; `ListenSettings::resolve_provider(&self) -> Result<&'static str, String>`; `listen::init(cx: &mut gpui::App)`; consts `INWORLD_STT_PROVIDER`, `SYSTEM_STT_PROVIDER`, `SUPPORTED_STT_PROVIDERS`.

- [ ] **Step 1: Write the failing test**

In `crates/listen/src/listen.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn settings_with_provider(provider: &str) -> ListenSettings {
        ListenSettings {
            enabled: true,
            provider: provider.to_string(),
            wake_word: "echo".to_string(),
            confirm_approvals: true,
        }
    }

    #[test]
    fn a_known_provider_resolves_whatever_its_casing() {
        assert_eq!(
            settings_with_provider("Inworld").resolve_provider(),
            Ok(INWORLD_STT_PROVIDER)
        );
    }

    /// An unknown provider must not quietly become Inworld: the setting would
    /// then promise something the schema does not deliver, and the user would
    /// have no way to tell.
    #[test]
    fn an_unknown_provider_is_an_error_carrying_the_offending_value() {
        assert_eq!(
            settings_with_provider("deepgram").resolve_provider(),
            Err("deepgram".to_string())
        );
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `~/.cargo/bin/cargo test -p listen`
Expected: FAIL — `ListenSettings` not found.

- [ ] **Step 3: Add the settings content struct**

In `crates/settings_content/src/settings_content.rs`, beside the `read_aloud` field:

```rust
    pub listen: Option<ListenSettingsContent>,
```

and beside `ReadAloudSettingsContent`:

```rust
#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct ListenSettingsContent {
    /// Whether to listen for spoken commands.
    ///
    /// Default: false
    pub enabled: Option<bool>,
    /// Which speech-to-text provider transcribes a spoken command.
    ///
    /// "inworld" needs an API key in `INWORLD_API_KEY` or the keychain.
    /// "system" (macOS only) uses the on-device recognizer that already
    /// listens for the wake word, so no audio leaves the machine.
    ///
    /// Any other value disables listening with a notice rather than silently
    /// falling back to Inworld.
    ///
    /// Default: inworld
    pub provider: Option<String>,
    /// The word that marks what follows as addressed to the agent.
    ///
    /// Default: echo
    pub wake_word: Option<String>,
    /// Whether a spoken approval of a tool call is read back and confirmed
    /// before it is granted. Denials are never confirmed.
    ///
    /// Default: true
    pub confirm_approvals: Option<bool>,
}
```

Match the exact derive list and attributes on `ReadAloudSettingsContent` in this file; copy them rather than trusting the list above if they differ.

- [ ] **Step 4: Write the settings type**

In `crates/listen/src/listen.rs`:

```rust
mod intent;
mod provider;

pub use intent::{VoiceCommand, parse_intent};
pub use provider::{FakeStt, SttProvider, Transcript};

use settings::{RegisterSetting, Settings};

pub const INWORLD_STT_PROVIDER: &str = "inworld";
pub const SYSTEM_STT_PROVIDER: &str = "system";

/// Every provider this build can actually hear through, for error messages.
pub const SUPPORTED_STT_PROVIDERS: &[&str] = &[
    INWORLD_STT_PROVIDER,
    #[cfg(target_os = "macos")]
    SYSTEM_STT_PROVIDER,
];

#[derive(Clone, Debug, PartialEq, RegisterSetting)]
pub struct ListenSettings {
    pub enabled: bool,
    pub provider: String,
    pub wake_word: String,
    pub confirm_approvals: bool,
}

impl ListenSettings {
    /// Resolves `listen.provider` to a provider that actually exists,
    /// returning the offending value when it does not, so the caller can say
    /// so instead of quietly hearing through Inworld.
    pub fn resolve_provider(&self) -> Result<&'static str, String> {
        let requested = self.provider.trim();
        SUPPORTED_STT_PROVIDERS
            .iter()
            .find(|supported| requested.eq_ignore_ascii_case(supported))
            .copied()
            .ok_or_else(|| self.provider.clone())
    }
}

impl Settings for ListenSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let listen = content.listen.as_ref();
        ListenSettings {
            enabled: listen.and_then(|settings| settings.enabled).unwrap_or(false),
            provider: listen
                .and_then(|settings| settings.provider.clone())
                .unwrap_or_else(|| INWORLD_STT_PROVIDER.to_string()),
            wake_word: listen
                .and_then(|settings| settings.wake_word.clone())
                .filter(|word| !word.trim().is_empty())
                .unwrap_or_else(|| "echo".to_string()),
            confirm_approvals: listen
                .and_then(|settings| settings.confirm_approvals)
                .unwrap_or(true),
        }
    }
}

pub fn init(cx: &mut gpui::App) {
    ListenSettings::register(cx);
}
```

- [ ] **Step 5: Add the defaults**

In `assets/settings/default.json`, after the `read_aloud` block:

```jsonc
  "listen": {
    "enabled": false,
    // "inworld" needs an API key (INWORLD_API_KEY or the keychain).
    // "system" (macOS) transcribes on device with the same recognizer that
    // listens for the wake word, so no audio leaves the machine.
    "provider": "inworld",
    // The word that marks what follows as addressed to the agent.
    "wake_word": "echo",
    // Whether a spoken approval is read back and confirmed before it is
    // granted. Denials are never confirmed — they are the safe direction.
    "confirm_approvals": true
  },
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `~/.cargo/bin/cargo test -p listen && ~/.cargo/bin/cargo check -p settings_content`
Expected: PASS — 11 tests, and `settings_content` compiles.

- [ ] **Step 7: Commit**

```bash
git add crates/listen crates/settings_content assets/settings/default.json
git commit -m "listen: Pin the wire format for listening"
```

---

### Task 4: The `Listener` state machine

**Files:**
- Create: `crates/listen/src/listener.rs`
- Modify: `crates/listen/src/listen.rs`

**Interfaces:**
- Consumes: `SttProvider`, `FakeStt`, `VoiceCommand`, `parse_intent`, `ListenSettings`.
- Produces:
  - `trait WakeSource: Send + Sync + 'static { fn wake_events(&self, cx: &App) -> mpsc::UnboundedReceiver<WakeSignal>; }`
  - `enum WakeSignal { Woke, UtteranceEnded, Failed(String) }`
  - `struct FakeWake` with `FakeWake::new()`, `wake(&self)`, `end_utterance(&self)`, `fail(&self, message: &str)`
  - `enum ListenEvent { Woke, Command(VoiceCommand), Abandoned, Failed(SharedString) }`
  - `struct Listener` with `Listener::new(provider: Arc<dyn SttProvider>, wake: Arc<dyn WakeSource>, cx: &mut Context<Self>) -> Self`, `Listener::arm_without_wake(&mut self, window: Duration, cx: &mut Context<Self>)`, `Listener::state(&self) -> ListenerState`
  - `enum ListenerState { Idle, Capturing }`
  - `impl EventEmitter<ListenEvent> for Listener {}`

The wake seam exists so the state machine is testable without a microphone; Task 7 supplies the real implementation.

- [ ] **Step 1: Write the failing test**

In `crates/listen/src/listener.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use std::sync::Arc;

    struct Harness {
        listener: Entity<Listener>,
        wake: Arc<FakeWake>,
        stt: FakeStt,
        events: Arc<Mutex<Vec<ListenEvent>>>,
    }

    fn harness(cx: &mut TestAppContext) -> Harness {
        let wake = Arc::new(FakeWake::new());
        let stt = FakeStt::new();
        let events = Arc::new(Mutex::new(Vec::new()));
        let listener = cx.new(|cx| {
            Listener::new(Arc::new(stt.clone()), wake.clone(), cx)
        });
        cx.update(|cx| {
            let events = events.clone();
            cx.subscribe(&listener, move |_, event: &ListenEvent, _| {
                if let Ok(mut events) = events.lock() {
                    events.push(event.clone());
                }
            })
            .detach();
        });
        Harness { listener, wake, stt, events }
    }

    impl Harness {
        fn events(&self) -> Vec<ListenEvent> {
            self.events.lock().map(|events| events.clone()).unwrap_or_default()
        }
    }

    #[gpui::test]
    async fn a_wake_signal_announces_itself_before_anything_is_transcribed(
        cx: &mut TestAppContext,
    ) {
        let harness = harness(cx);
        harness.wake.wake();
        cx.run_until_parked();

        assert_eq!(harness.events(), vec![ListenEvent::Woke]);
        assert_eq!(
            harness.listener.read_with(cx, |listener, _| listener.state()),
            ListenerState::Capturing
        );
    }

    #[gpui::test]
    async fn a_finished_utterance_dispatches_the_command_it_parsed(cx: &mut TestAppContext) {
        let harness = harness(cx);
        harness.stt.queue_transcript("check the tests first");
        harness.wake.wake();
        cx.run_until_parked();
        harness.wake.end_utterance();
        cx.run_until_parked();

        assert_eq!(
            harness.events(),
            vec![
                ListenEvent::Woke,
                ListenEvent::Command(VoiceCommand::Say("check the tests first".to_string())),
            ]
        );
        assert_eq!(
            harness.listener.read_with(cx, |listener, _| listener.state()),
            ListenerState::Idle
        );
    }

    /// A wake with nothing behind it is Echo hearing its own voice, or a
    /// passing conversation. It must hand narration back rather than leaving
    /// the reader parked forever.
    #[gpui::test]
    async fn a_wake_with_no_speech_behind_it_is_abandoned(cx: &mut TestAppContext) {
        let harness = harness(cx);
        harness.wake.wake();
        cx.run_until_parked();
        cx.background_executor.advance_clock(COMMAND_TIMEOUT * 2);
        cx.run_until_parked();

        assert_eq!(
            harness.events(),
            vec![ListenEvent::Woke, ListenEvent::Abandoned]
        );
        assert_eq!(
            harness.listener.read_with(cx, |listener, _| listener.state()),
            ListenerState::Idle
        );
    }

    /// A failed transcription must be audible as a failure. Silently
    /// returning to idle is indistinguishable from a listener that has
    /// stopped working.
    #[gpui::test]
    async fn a_failed_transcription_reports_rather_than_going_quiet(cx: &mut TestAppContext) {
        let harness = harness(cx);
        harness.stt.fail_next();
        harness.wake.wake();
        cx.run_until_parked();
        harness.wake.end_utterance();
        cx.run_until_parked();

        let events = harness.events();
        assert_eq!(events[0], ListenEvent::Woke);
        assert!(matches!(events[1], ListenEvent::Failed(_)), "{events:?}");
    }

    /// Mid-conversation — answering "approve?" — the user should not have to
    /// say the wake word again.
    #[gpui::test]
    async fn arming_without_a_wake_word_captures_the_next_utterance(cx: &mut TestAppContext) {
        let harness = harness(cx);
        harness.stt.queue_transcript("yes");
        harness.listener.update(cx, |listener, cx| {
            listener.arm_without_wake(Duration::from_secs(8), cx)
        });
        cx.run_until_parked();
        harness.wake.end_utterance();
        cx.run_until_parked();

        assert_eq!(
            harness.events(),
            vec![ListenEvent::Command(VoiceCommand::Approve)]
        );
    }

    /// The re-arm is a window, not a latch: if the user says nothing, the
    /// listener must go back to requiring the wake word rather than staying
    /// open to every word in the room.
    #[gpui::test]
    async fn an_unused_arm_window_closes_on_its_own(cx: &mut TestAppContext) {
        let harness = harness(cx);
        harness.listener.update(cx, |listener, cx| {
            listener.arm_without_wake(Duration::from_secs(8), cx)
        });
        cx.run_until_parked();
        cx.background_executor.advance_clock(Duration::from_secs(9));
        cx.run_until_parked();

        assert_eq!(
            harness.listener.read_with(cx, |listener, _| listener.state()),
            ListenerState::Idle
        );
        assert_eq!(harness.events(), vec![ListenEvent::Abandoned]);
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `~/.cargo/bin/cargo test -p listen listener`
Expected: FAIL — `listener` module not found.

- [ ] **Step 3: Write the state machine**

`crates/listen/src/listener.rs`:

```rust
use crate::intent::{VoiceCommand, parse_intent};
use crate::provider::{SttProvider, Transcript};
use anyhow::Result;
use futures::StreamExt as _;
use futures::channel::mpsc;
use gpui::{App, AppContext as _, Context, Entity, EventEmitter, SharedString, Task};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How long a wake may go unanswered before the listener gives narration
/// back. Long enough to draw breath, short enough that a false wake from
/// Echo's own voice is a pause and not a silence.
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

/// What the on-device listener heard.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WakeSignal {
    Woke,
    UtteranceEnded,
    Failed(String),
}

/// The seam the microphone sits behind, so the state machine is testable
/// without one. Task 7 supplies the macOS implementation.
pub trait WakeSource: Send + Sync + 'static {
    fn wake_events(&self, cx: &App) -> mpsc::UnboundedReceiver<WakeSignal>;
}

#[derive(Default)]
struct FakeWakeState {
    senders: Vec<mpsc::UnboundedSender<WakeSignal>>,
}

/// Test double for the wake listener.
#[derive(Default)]
pub struct FakeWake {
    state: Mutex<FakeWakeState>,
}

impl FakeWake {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn wake(&self) {
        self.emit(WakeSignal::Woke);
    }

    pub fn end_utterance(&self) {
        self.emit(WakeSignal::UtteranceEnded);
    }

    pub fn fail(&self, message: &str) {
        self.emit(WakeSignal::Failed(message.to_string()));
    }

    fn emit(&self, signal: WakeSignal) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        state
            .senders
            .retain(|sender| sender.unbounded_send(signal.clone()).is_ok());
    }
}

impl WakeSource for FakeWake {
    fn wake_events(&self, _cx: &App) -> mpsc::UnboundedReceiver<WakeSignal> {
        let (sender, receiver) = mpsc::unbounded();
        if let Ok(mut state) = self.state.lock() {
            state.senders.push(sender);
        }
        receiver
    }
}

/// Something the owning view has to act on.
#[derive(Clone, Debug, PartialEq)]
pub enum ListenEvent {
    /// The wake word landed. The owner pauses narration on this, which is
    /// what lets the command be captured against a silent speaker.
    Woke,
    Command(VoiceCommand),
    /// A wake that came to nothing. The owner resumes narration.
    Abandoned,
    Failed(SharedString),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListenerState {
    Idle,
    Capturing,
}

pub struct Listener {
    provider: Arc<dyn SttProvider>,
    state: ListenerState,
    wake_word: String,
    /// Feeds the in-flight transcription. Dropping it ends the utterance.
    audio: Option<mpsc::UnboundedSender<Vec<f32>>>,
    transcription: Option<Task<()>>,
    timeout: Option<Task<()>>,
    /// While set, the next utterance is captured without a wake word.
    armed_without_wake: bool,
    _wake: Task<()>,
}

impl EventEmitter<ListenEvent> for Listener {}

impl Listener {
    pub fn new(
        provider: Arc<dyn SttProvider>,
        wake: Arc<dyn WakeSource>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut signals = wake.wake_events(cx);
        let wake_task = cx.spawn(async move |this, cx| {
            while let Some(signal) = signals.next().await {
                let delivered = this.update(cx, |this, cx| this.handle_wake(signal, cx));
                if delivered.is_err() {
                    return;
                }
            }
        });

        Self {
            provider,
            state: ListenerState::Idle,
            wake_word: String::from("echo"),
            audio: None,
            transcription: None,
            timeout: None,
            armed_without_wake: false,
            _wake: wake_task,
        }
    }

    pub fn state(&self) -> ListenerState {
        self.state
    }

    pub fn set_wake_word(&mut self, wake_word: String) {
        if !wake_word.trim().is_empty() {
            self.wake_word = wake_word;
        }
    }

    /// Captures the next utterance without requiring the wake word, for the
    /// span of `window`. Used when Echo has just asked a question and the
    /// user is already mid-conversation.
    pub fn arm_without_wake(&mut self, window: Duration, cx: &mut Context<Self>) {
        self.armed_without_wake = true;
        self.begin_capture(cx);
        self.arm_timeout(window, cx);
    }

    fn handle_wake(&mut self, signal: WakeSignal, cx: &mut Context<Self>) {
        match signal {
            WakeSignal::Woke => {
                if self.state == ListenerState::Capturing {
                    return;
                }
                cx.emit(ListenEvent::Woke);
                self.begin_capture(cx);
                self.arm_timeout(COMMAND_TIMEOUT, cx);
            }
            WakeSignal::UtteranceEnded => self.finish_capture(),
            WakeSignal::Failed(message) => {
                self.reset();
                cx.emit(ListenEvent::Failed(message.into()));
            }
        }
    }

    fn begin_capture(&mut self, cx: &mut Context<Self>) {
        let (audio_sender, audio) = mpsc::unbounded();
        let mut transcripts = self.provider.transcribe(audio, cx);
        let wake_word = self.wake_word.clone();

        self.audio = Some(audio_sender);
        self.state = ListenerState::Capturing;
        self.transcription = Some(cx.spawn(async move |this, cx| {
            let mut latest: Option<Result<Transcript>> = None;
            while let Some(transcript) = transcripts.next().await {
                let is_final = matches!(&transcript, Ok(transcript) if transcript.is_final);
                let is_error = transcript.is_err();
                latest = Some(transcript);
                if is_final || is_error {
                    break;
                }
            }
            this.update(cx, |this, cx| this.dispatch(latest, &wake_word, cx))
                .ok();
        }));
    }

    /// Ends the utterance by dropping the audio channel, which is how a
    /// provider learns there is no more to come.
    fn finish_capture(&mut self) {
        self.audio.take();
    }

    fn dispatch(
        &mut self,
        transcript: Option<Result<Transcript>>,
        wake_word: &str,
        cx: &mut Context<Self>,
    ) {
        self.reset();
        match transcript {
            Some(Ok(transcript)) => {
                let command = parse_intent(&transcript.text, wake_word);
                if command == VoiceCommand::Never {
                    cx.emit(ListenEvent::Abandoned);
                } else {
                    cx.emit(ListenEvent::Command(command));
                }
            }
            Some(Err(error)) => {
                cx.emit(ListenEvent::Failed(format!("{error}").into()));
            }
            None => cx.emit(ListenEvent::Abandoned),
        }
    }

    fn arm_timeout(&mut self, window: Duration, cx: &mut Context<Self>) {
        self.timeout = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(window).await;
            this.update(cx, |this, cx| {
                if this.state != ListenerState::Capturing {
                    return;
                }
                this.reset();
                cx.emit(ListenEvent::Abandoned);
            })
            .ok();
        }));
    }

    fn reset(&mut self) {
        self.state = ListenerState::Idle;
        self.armed_without_wake = false;
        self.audio.take();
        self.transcription.take();
        self.timeout.take();
    }
}
```

Add to `crates/listen/src/listen.rs`:

```rust
mod intent;
mod listener;
mod provider;

pub use intent::{VoiceCommand, parse_intent};
pub use listener::{
    COMMAND_TIMEOUT, FakeWake, ListenEvent, Listener, ListenerState, WakeSignal, WakeSource,
};
pub use provider::{FakeStt, SttProvider, Transcript};
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `~/.cargo/bin/cargo test -p listen`
Expected: PASS. If `arming_without_a_wake_word` fails because `dispatch` fires before `end_utterance`, the fake is delivering its scripted transcript before the audio channel closes — that is the fake's contract, so the assertion order in the test is what changes, not the state machine.

- [ ] **Step 5: Commit**

```bash
git add crates/listen
git commit -m "listen: Hold a command open from the wake word to the full stop"
```

---

### Task 5: Dispatch in `agent_ui` — routing, steering, and narration control

**Files:**
- Create: `crates/agent_ui/src/voice_dispatch.rs`
- Modify: `crates/agent_ui/src/agent_ui.rs` (declare the module)
- Modify: `crates/agent_ui/Cargo.toml` (add `listen.workspace = true`, and `listen = { workspace = true, features = ["test-support"] }` under dev-dependencies)

**Interfaces:**
- Consumes: `listen::VoiceCommand`.
- Produces: `VoiceTarget`, `pick_voice_target(candidates: &[VoiceCandidate]) -> Option<VoiceTarget>`, `VoiceCandidate { session_id: acp::SessionId, blocked_on_approval: bool, last_spoke_at: Option<Instant>, is_active: bool }`.

Routing is pure over a snapshot so it is testable without building a panel. The caller in Task 5b builds the snapshot from `ConversationView::threads`.

- [ ] **Step 1: Write the failing test**

In `crates/agent_ui/src/voice_dispatch.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn candidate(id: &str) -> VoiceCandidate {
        VoiceCandidate {
            session_id: acp::SessionId(id.into()),
            blocked_on_approval: false,
            last_spoke_at: None,
            is_active: false,
        }
    }

    /// A blocked thread outranks a talking one: it is the only one that
    /// cannot make progress, and a blocked subagent transitively blocks its
    /// parent.
    #[test]
    fn a_thread_blocked_on_approval_wins_over_one_that_is_speaking() {
        let now = Instant::now();
        let mut talking = candidate("talking");
        talking.last_spoke_at = Some(now);
        let mut blocked = candidate("blocked");
        blocked.blocked_on_approval = true;

        let target = pick_voice_target(&[talking, blocked]).unwrap();
        assert_eq!(target.session_id.0.as_ref(), "blocked");
    }

    #[test]
    fn the_most_recent_speaker_wins_when_nothing_is_blocked() {
        let now = Instant::now();
        let mut older = candidate("older");
        older.last_spoke_at = Some(now - Duration::from_secs(30));
        let mut newer = candidate("newer");
        newer.last_spoke_at = Some(now);

        let target = pick_voice_target(&[older, newer]).unwrap();
        assert_eq!(target.session_id.0.as_ref(), "newer");
    }

    #[test]
    fn the_active_thread_is_the_fallback_when_nothing_has_spoken() {
        let mut active = candidate("active");
        active.is_active = true;

        let target = pick_voice_target(&[candidate("other"), active]).unwrap();
        assert_eq!(target.session_id.0.as_ref(), "active");
    }

    #[test]
    fn no_candidates_means_no_target() {
        assert!(pick_voice_target(&[]).is_none());
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `~/.cargo/bin/cargo test -p agent_ui voice_dispatch`
Expected: FAIL — module not found.

- [ ] **Step 3: Write the router**

`crates/agent_ui/src/voice_dispatch.rs`:

```rust
use agent_client_protocol::schema::v1 as acp;
use std::time::Instant;

/// One thread's standing at the moment a spoken command lands.
#[derive(Clone, Debug)]
pub struct VoiceCandidate {
    pub session_id: acp::SessionId,
    pub blocked_on_approval: bool,
    pub last_spoke_at: Option<Instant>,
    pub is_active: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VoiceTarget {
    pub session_id: acp::SessionId,
}

/// Decides which thread a spoken command addresses.
///
/// Priority is blocked, then most recently spoken, then active. The user
/// cannot see which thread is active — that is the whole premise — so the
/// active thread is a last resort rather than the default.
pub fn pick_voice_target(candidates: &[VoiceCandidate]) -> Option<VoiceTarget> {
    let blocked = candidates
        .iter()
        .filter(|candidate| candidate.blocked_on_approval)
        .max_by_key(|candidate| candidate.last_spoke_at);
    let spoke = candidates
        .iter()
        .filter(|candidate| candidate.last_spoke_at.is_some())
        .max_by_key(|candidate| candidate.last_spoke_at);
    let active = candidates.iter().find(|candidate| candidate.is_active);

    blocked
        .or(spoke)
        .or(active)
        .map(|candidate| VoiceTarget {
            session_id: candidate.session_id.clone(),
        })
}
```

Declare `mod voice_dispatch;` in `crates/agent_ui/src/agent_ui.rs` beside the other module declarations.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `~/.cargo/bin/cargo test -p agent_ui voice_dispatch`
Expected: PASS — 4 tests.

- [ ] **Step 5: Commit**

```bash
git add crates/agent_ui
git commit -m "agent_ui: Decide which thread a spoken command is addressed to"
```

---

### Task 6: Inworld streaming speech-to-text

**Files:**
- Create: `crates/listen/src/inworld_stt.rs`
- Modify: `crates/listen/src/listen.rs`

**Interfaces:**
- Consumes: `SttProvider`, `Transcript`.
- Produces: `InworldStt::new(client: Arc<dyn HttpClient>, api_key: String) -> Self`, `INWORLD_STT_URL`.

Mirrors `read_aloud::InworldTts`: a background task issues the request and forwards each decoded line. Audio is encoded LINEAR16 at 16 kHz, which is what the endpoint expects and what the microphone stream is resampled to.

- [ ] **Step 1: Write the failing test**

In `crates/listen/src/inworld_stt.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_transcript_line_is_parsed_into_its_text_and_finality() {
        let line = r#"{"result":{"transcript":"check the tests","isFinal":true}}"#;
        let parsed = parse_transcript_line(line).unwrap().unwrap();
        assert_eq!(parsed.text, "check the tests");
        assert!(parsed.is_final);
    }

    #[test]
    fn a_partial_line_is_not_final() {
        let line = r#"{"result":{"transcript":"check the","isFinal":false}}"#;
        let parsed = parse_transcript_line(line).unwrap().unwrap();
        assert_eq!(parsed.text, "check the");
        assert!(!parsed.is_final);
    }

    /// The stream carries keepalives and metadata alongside transcripts.
    /// Treating one as an empty transcript would dispatch an empty command.
    #[test]
    fn a_line_with_no_transcript_yields_nothing_rather_than_an_empty_command() {
        assert!(parse_transcript_line(r#"{"result":{}}"#).unwrap().is_none());
        assert!(parse_transcript_line("").unwrap().is_none());
    }

    #[test]
    fn an_error_line_is_reported_rather_than_ignored() {
        let line = r#"{"error":{"message":"quota exceeded"}}"#;
        assert!(parse_transcript_line(line).is_err());
    }

    #[test]
    fn frames_encode_as_little_endian_signed_sixteen_bit() {
        let encoded = encode_linear16(&[0.0, 1.0, -1.0]);
        assert_eq!(encoded, vec![0, 0, 0xFF, 0x7F, 0x00, 0x80]);
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `~/.cargo/bin/cargo test -p listen inworld`
Expected: FAIL — module not found.

- [ ] **Step 3: Write the provider**

`crates/listen/src/inworld_stt.rs`, following the shape of `crates/read_aloud/src/inworld.rs`:

```rust
use crate::provider::{SttProvider, Transcript};
use anyhow::{Context as _, Result, anyhow};
use base64::Engine as _;
use futures::channel::mpsc;
use futures::io::BufReader;
use futures::{AsyncBufReadExt as _, StreamExt as _};
use gpui::{App, AppContext as _};
use http_client::{AsyncBody, HttpClient, Method, Request as HttpRequest};
use std::sync::Arc;

pub const INWORLD_STT_URL: &str = "https://api.inworld.ai/stt/v1/recognize:stream";

/// What the endpoint expects, and what the microphone stream is resampled to
/// before it gets here.
const SAMPLE_RATE: u32 = 16_000;

pub struct InworldStt {
    client: Arc<dyn HttpClient>,
    api_key: String,
}

impl InworldStt {
    pub fn new(client: Arc<dyn HttpClient>, api_key: String) -> Self {
        Self { client, api_key }
    }
}

impl SttProvider for InworldStt {
    fn transcribe(
        &self,
        audio: mpsc::UnboundedReceiver<Vec<f32>>,
        cx: &App,
    ) -> mpsc::UnboundedReceiver<Result<Transcript>> {
        let (sender, receiver) = mpsc::unbounded();
        let client = self.client.clone();
        let api_key = self.api_key.clone();

        cx.background_spawn(async move {
            if let Err(error) = stream_utterance(client, api_key, audio, sender.clone()).await {
                sender.unbounded_send(Err(error)).ok();
            }
        })
        .detach();

        receiver
    }
}

/// Collects the utterance, issues the request, and forwards each decoded
/// line.
///
/// ponytail: the utterance is buffered and sent as one request rather than
/// streamed frame by frame over a duplex connection. A spoken command runs a
/// second or two, so the latency this costs is the tail of the utterance, not
/// the whole of it — and it avoids a WebSocket client this crate would
/// otherwise be the only user of. Duplex streaming is the upgrade if the
/// wait becomes noticeable.
async fn stream_utterance(
    client: Arc<dyn HttpClient>,
    api_key: String,
    mut audio: mpsc::UnboundedReceiver<Vec<f32>>,
    sender: mpsc::UnboundedSender<Result<Transcript>>,
) -> Result<()> {
    let mut samples = Vec::new();
    while let Some(frame) = audio.next().await {
        samples.extend(frame);
    }
    if samples.is_empty() {
        return Err(anyhow!("no audio was captured"));
    }

    let body = serde_json::json!({
        "audio": {
            "content": base64::engine::general_purpose::STANDARD
                .encode(encode_linear16(&samples)),
        },
        "config": {
            "audioEncoding": "LINEAR16",
            "sampleRateHertz": SAMPLE_RATE,
            "languageCode": "en-US",
        },
    });

    let request = HttpRequest::builder()
        .method(Method::POST)
        .uri(INWORLD_STT_URL)
        .header("Authorization", format!("Basic {api_key}"))
        .header("Content-Type", "application/json")
        .body(AsyncBody::from(serde_json::to_string(&body)?))
        .context("building the transcription request")?;

    let response = client
        .send(request)
        .await
        .context("sending the transcription request")?;
    if !response.status().is_success() {
        return Err(anyhow!(
            "the transcription service returned {}",
            response.status()
        ));
    }

    let mut lines = BufReader::new(response.into_body()).lines();
    while let Some(line) = lines.next().await {
        let line = line.context("reading the transcription stream")?;
        match parse_transcript_line(&line) {
            Ok(Some(transcript)) => {
                if sender.unbounded_send(Ok(transcript)).is_err() {
                    return Ok(());
                }
            }
            Ok(None) => {}
            Err(error) => return Err(error),
        }
    }

    Ok(())
}

/// LINEAR16 is little-endian signed 16-bit PCM, which is the inverse of what
/// `read_aloud` decodes on the way back.
fn encode_linear16(samples: &[f32]) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(samples.len() * 2);
    for sample in samples {
        let clamped = sample.clamp(-1.0, 1.0);
        let scaled = (clamped * i16::MAX as f32) as i16;
        encoded.extend_from_slice(&scaled.to_le_bytes());
    }
    encoded
}

/// `Ok(None)` for a line that carries no transcript — a keepalive or a
/// metadata frame. An empty transcript would otherwise dispatch an empty
/// command.
fn parse_transcript_line(line: &str) -> Result<Option<Transcript>> {
    let line = line.trim();
    if line.is_empty() {
        return Ok(None);
    }
    let value: serde_json::Value =
        serde_json::from_str(line).context("parsing a transcription line")?;

    if let Some(message) = value
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(|message| message.as_str())
    {
        return Err(anyhow!("the transcription service failed: {message}"));
    }

    let Some(result) = value.get("result") else {
        return Ok(None);
    };
    let Some(text) = result.get("transcript").and_then(|text| text.as_str()) else {
        return Ok(None);
    };
    if text.trim().is_empty() {
        return Ok(None);
    }

    Ok(Some(Transcript {
        text: text.to_string(),
        is_final: result
            .get("isFinal")
            .and_then(|final_flag| final_flag.as_bool())
            .unwrap_or(false),
    }))
}
```

Add `base64.workspace = true` to `crates/listen/Cargo.toml`, and export from `listen.rs`:

```rust
mod inworld_stt;
pub use inworld_stt::{INWORLD_STT_URL, InworldStt};
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `~/.cargo/bin/cargo test -p listen`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/listen
git commit -m "listen: Hear a command through Inworld"
```

---

### Task 7: The on-device wake listener

**Files:**
- Create: `crates/listen/src/wake.rs`
- Modify: `crates/listen/src/listen.rs`
- Modify: `crates/listen/Cargo.toml`
- Modify: root `Cargo.toml` (add `objc2-speech` to `[workspace.dependencies]` matching the `objc2 = "0.6"` generation)

**Interfaces:**
- Consumes: `WakeSource`, `WakeSignal`.
- Produces: `SpeechWake::new(wake_word: String) -> Result<Self>` behind `#[cfg(target_os = "macos")]`.

This is the only task that cannot be verified headless. It lands last so nothing else waits on it.

- [ ] **Step 1: Add the dependency and confirm it resolves**

Add to root `Cargo.toml` `[workspace.dependencies]`:

```toml
objc2-speech = "0.3"
```

Run: `~/.cargo/bin/cargo tree -p listen 2>&1 | head -20` after adding `objc2-speech.workspace = true` under a `[target.'cfg(target_os = "macos")'.dependencies]` section in `crates/listen/Cargo.toml`.
Expected: the version resolves against `objc2 0.6`. If it does not, pin the version that does — check `cargo search objc2-speech` and the `objc2` version in its manifest. A mismatched objc2 generation produces type errors that look like unrelated trait failures, so resolve this before writing any binding code.

- [ ] **Step 2: Write the authorization and wake-word logic that can be tested**

The framework calls cannot run in a test, but the wake-word match over a partial transcript can:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wake_word_is_found_at_the_start_of_a_partial_transcript() {
        assert!(contains_wake_word("Echo check the tests", "echo"));
        assert!(contains_wake_word("echo, approve", "echo"));
    }

    /// The recognizer emits a growing transcript, so the wake word appears
    /// mid-string once the user keeps talking.
    #[test]
    fn a_wake_word_is_found_after_earlier_speech() {
        assert!(contains_wake_word("so anyway echo approve", "echo"));
    }

    /// Otherwise "echoing the change" wakes the listener.
    #[test]
    fn a_word_that_merely_starts_with_the_wake_word_does_not_wake() {
        assert!(!contains_wake_word("echoing the change", "echo"));
        assert!(!contains_wake_word("recheck the echoes", "echo"));
    }
}
```

- [ ] **Step 3: Implement `contains_wake_word`**

```rust
/// Whether `transcript` contains `wake_word` as a whole word.
///
/// Whole-word rather than prefix: "echoing" and "echoes" are ordinary English
/// in a conversation about a program named Echo, and each false wake costs
/// the listener a narration pause.
fn contains_wake_word(transcript: &str, wake_word: &str) -> bool {
    let wake_word = wake_word.trim().to_lowercase();
    if wake_word.is_empty() {
        return false;
    }
    transcript
        .to_lowercase()
        .split(|character: char| !character.is_alphanumeric())
        .any(|word| word == wake_word)
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `~/.cargo/bin/cargo test -p listen wake`
Expected: PASS — 3 tests.

- [ ] **Step 5: Implement `SpeechWake`**

Behind `#[cfg(target_os = "macos")]`, holding an `SFSpeechRecognizer` with `requiresOnDeviceRecognition = true` on an `SFSpeechAudioBufferRecognitionRequest`, fed from `audio::open_input_stream`. On each partial result, `contains_wake_word` decides whether to emit `WakeSignal::Woke`; the recognizer's own endpointing emits `WakeSignal::UtteranceEnded`. Authorization is requested through `SFSpeechRecognizer::requestAuthorization`; a denial emits `WakeSignal::Failed` naming the permission rather than parking silently.

A `ponytail:` comment records that the recognition request is restarted on a
~50s timer because the framework enforces a per-request audio duration limit,
and that macOS 26's `SpeechAnalyzer` removes the limit and improves accuracy
but is Swift-only, so it needs a helper binary rather than an objc2 binding.

- [ ] **Step 6: Verify by hand**

Build and run the app, enable `listen.enabled`, grant both permissions when prompted, and confirm: the wake word pauses narration, a command dispatches, and silence after a wake resumes narration within five seconds.

- [ ] **Step 7: Commit**

```bash
git add crates/listen Cargo.toml
git commit -m "listen: Hear the wake word without sending anything anywhere"
```

---

### Task 8: CI

**Files:**
- Modify: `.github/workflows/echo_tests.yml`

- [ ] **Step 1: Fix the stale branch trigger and add the crate**

The `push` trigger names `read-aloud`, a branch renamed to `echo`, so pushes to the working branch have run no tests. Change the branch list to `main` and `echo`, and add `-p listen` to the test invocation beside the other fork-owned crates.

- [ ] **Step 2: Run the fork's test scope locally**

Run: `~/.cargo/bin/cargo test -p listen -p read_aloud -p agent_ui`
Expected: PASS.

- [ ] **Step 3: Run the lints**

Run: `./script/clippy` and `~/.cargo/bin/cargo fmt --check`
Expected: clean.

- [ ] **Step 4: Commit**

```bash
git add .github/workflows/echo_tests.yml
git commit -m "ci: Run the fork's tests on the branch it actually uses"
```

---

## Deferred

Wiring the `Listener` into `AgentPanel` — constructing it from settings, subscribing to `ListenEvent`, building the `VoiceCandidate` snapshot from `ConversationView::threads`, and performing steer/send/authorize/narration-control — is Task 5b and depends on Tasks 4, 5, and 7 all landing. It is deliberately not specified in code here: the panel's ownership of conversation views is the part of this fork most likely to have moved by the time it is written, so it gets its own pass with fresh reading rather than a plan written against today's line numbers.
