# Agent Panel Read Aloud Implementation Plan (Phase 1)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Speak assistant prose in the agent panel aloud as it streams, highlight the sentence being spoken, and seek playback to any sentence the user clicks.

**Architecture:** A new `read_aloud` crate holds all logic. A pure segmenter turns `ParsedMarkdown` into sentence-sized `Utterance`s carrying source byte ranges. A `TtsProvider` trait (Inworld first) synthesizes PCM; an `AudioSink` trait (rodio first) plays it. A GPUI entity per thread view subscribes to `AcpThreadEvent`, drives the queue, and pushes a speaking highlight into the `Markdown` entity. Both provider and sink are traits so the whole pipeline is testable without network or an audio device.

**Tech Stack:** Rust, GPUI, rodio (pinned git rev), `http_client`, Inworld TTS REST API, `pulldown_cmark` via Zed's `markdown` crate.

## Prerequisite

Phase 0 (`docs/superpowers/plans/2026-08-06-zed-echo-rebrand.md`) must be **fully verified installed** before starting. Until it is, running the binary writes to the user's real Zed thread database.

## Global Constraints

- Rust toolchain pinned to `1.95.0`. Use `~/.cargo/bin/cargo`.
- Use `./script/clippy`, never `cargo clippy`.
- Avoid `unwrap()` / `expect()` in non-test code; propagate with `?`.
- Never silently discard errors with `let _ =` on fallible operations. Use `.log_err()` (from `util::ResultExt`) where an error must be swallowed but stay visible.
- Never create `mod.rs` files. New crates declare `[lib] path = "src/<crate_name>.rs"`.
- Do not write comments that summarize code. Comments explain *why* only.
- Use full words for variable names — no abbreviations.
- In GPUI tests use `cx.background_executor().timer(..)`, never `smol::Timer::after(..)`.
- **The queue must never stall the UI on a network or device fault.** Every failure path logs and advances.
- **Settings defaults are exact:** `enabled` = `false`, `auto_play` = `true`, `provider` = `"inworld"`, `voice_id` = `"Dennis"`, `model_id` = `"inworld-tts-2"`, `speaking_rate` = `1.0`.
- The API key is never read from `settings.json`. Resolution order is `INWORLD_API_KEY` environment variable, then the GPUI keychain credential store.
- `read_aloud::Toggle` ships **unbound**. The keymap is symlinked to the real Zed's; binding a key would stomp it.

## Corrections to the design document

The design was written before the code was read. These anchors are wrong in it and correct here — **use these**:

| Design says | Actually |
|---|---|
| Integration point is `conversation_view.rs:3452` | `render_agent_markdown`'s assistant-prose caller is `ThreadView::render_markdown` at `crates/agent_ui/src/conversation_view/thread_view.rs:11439`. The `conversation_view.rs:2812` caller renders the **auth callout**, not assistant messages. |
| `player.rs` owns a rodio `Sink` from `crates/audio` | `crates/audio` exposes no sink. This rodio rev has no `Sink` type at all — the equivalent is `rodio::Player`. `Audio::ensure_output_exists()` is private, so Task 4 adds a public seam. |
| Settings go in `crates/settings` | Content structs live in `crates/settings_content/src/settings_content.rs`. |
| `set_search_highlights(&[Range<usize>])` | Real signature is `(Vec<Range<usize>>, active: Option<usize>, &mut Context<Self>)`. |
| `on_source_click` yields "the source byte offset" | Handler is `Fn(source_index: usize, click_count: usize, &mut Window, &mut App) -> bool`. Returning `true` blocks the default selection behavior. |

Two things the design assumed but did not verify, both of which **check out**:

- `AcpThreadEvent::NewEntry` and `AcpThreadEvent::EntryUpdated(usize)` exist (`crates/acp_thread/src/acp_thread.rs:2155`).
- Streaming boundary condition 2 ("the parser has closed the containing block") is directly observable: `MarkdownEvent::RootStart` / `MarkdownEvent::RootEnd(usize)` bracket every top-level block (`crates/markdown/src/parser.rs:790-793`).

One thing the design did not anticipate: `Markdown::parsed_markdown()` is gated `#[cfg(any(test, feature = "test-support"))]`. Task 2 ungates it.

## Prior art worth reading before you start

`crates/agent_ui/src/conversation_view/thread_search_bar.rs:882` — `collect_markdowns()` already maps an `AgentThreadEntry` to its `Entity<Markdown>` list, including the thought-chunk exclusion this feature needs. Task 8 mirrors its shape.

## File Structure

| File | Responsibility |
|---|---|
| `crates/read_aloud/Cargo.toml` | Crate manifest; `[lib] path = "src/read_aloud.rs"` |
| `crates/read_aloud/src/read_aloud.rs` | Crate root: settings, `init()`, the `ReadAloud` GPUI entity |
| `crates/read_aloud/src/segmenter.rs` | Pure `ParsedMarkdown` → `Vec<Utterance>`. No I/O, no GPUI. Carries the main test suite |
| `crates/read_aloud/src/provider.rs` | `Pcm`, `TtsProvider` trait, `FakeTts` |
| `crates/read_aloud/src/sink.rs` | `AudioSink` trait, `RodioSink`, `FakeSink` |
| `crates/read_aloud/src/player.rs` | Queue, prefetch, seek, position tracking |
| `crates/read_aloud/src/inworld.rs` | Inworld REST client and API key resolution |
| `crates/settings_content/src/settings_content.rs` | `ReadAloudSettingsContent` |
| `crates/markdown/src/markdown.rs` | `set_speaking_highlight` + paint; ungate `parsed_markdown()` |
| `crates/audio/src/audio_pipeline.rs` | `Audio::connect_player()` public seam |
| `crates/agent_ui/src/conversation_view/thread_view.rs` | `on_source_click` wiring, chunk-update enqueue |

`thread_view.rs` is 12,967 lines. Keep the edits there to the two small hooks in Task 9; everything else lives in `read_aloud`.

---

### Task 1: Crate skeleton and settings

**Files:**
- Create: `crates/read_aloud/Cargo.toml`
- Create: `crates/read_aloud/src/read_aloud.rs`
- Modify: `Cargo.toml` (workspace members ~line 170, workspace dependencies ~line 399)
- Modify: `crates/settings_content/src/settings_content.rs`
- Modify: `assets/settings/default.json`

**Interfaces:**
- Consumes: nothing.
- Produces: `read_aloud::ReadAloudSettings { enabled: bool, auto_play: bool, provider: String, voice_id: String, model_id: String, speaking_rate: f32 }` and `read_aloud::init(cx: &mut App)`.

- [ ] **Step 1: Write the failing test**

Create `crates/read_aloud/src/read_aloud.rs`:

```rust
mod segmenter;

use settings::{RegisterSetting, Settings};

#[derive(Clone, Debug, PartialEq, RegisterSetting)]
pub struct ReadAloudSettings {
    pub enabled: bool,
    pub auto_play: bool,
    pub provider: String,
    pub voice_id: String,
    pub model_id: String,
    pub speaking_rate: f32,
}

impl Settings for ReadAloudSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let read_aloud = content.read_aloud.as_ref();
        ReadAloudSettings {
            enabled: read_aloud.and_then(|s| s.enabled).unwrap_or(false),
            auto_play: read_aloud.and_then(|s| s.auto_play).unwrap_or(true),
            provider: read_aloud
                .and_then(|s| s.provider.clone())
                .unwrap_or_else(|| "inworld".to_string()),
            voice_id: read_aloud
                .and_then(|s| s.voice_id.clone())
                .unwrap_or_else(|| "Dennis".to_string()),
            model_id: read_aloud
                .and_then(|s| s.model_id.clone())
                .unwrap_or_else(|| "inworld-tts-2".to_string()),
            speaking_rate: read_aloud.and_then(|s| s.speaking_rate).unwrap_or(1.0),
        }
    }
}

pub fn init(cx: &mut gpui::App) {
    ReadAloudSettings::register(cx);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_inert_but_autoplay_once_enabled() {
        let settings = ReadAloudSettings::from_settings(&settings::SettingsContent::default());
        assert!(!settings.enabled, "feature must be off on a fresh profile");
        assert!(settings.auto_play, "auto_play describes behavior once enabled");
        assert_eq!(settings.provider, "inworld");
        assert_eq!(settings.voice_id, "Dennis");
        assert_eq!(settings.model_id, "inworld-tts-2");
        assert_eq!(settings.speaking_rate, 1.0);
    }
}
```

Create `crates/read_aloud/src/segmenter.rs` with a single line so the module resolves (Task 2 replaces it wholesale):

```rust
// Filled in by Task 2.
```

- [ ] **Step 2: Create the manifest**

Create `crates/read_aloud/Cargo.toml`:

```toml
[package]
name = "read_aloud"
version = "0.1.0"
edition.workspace = true
publish.workspace = true
license = "GPL-3.0-or-later"

[lints]
workspace = true

[lib]
path = "src/read_aloud.rs"
doctest = false

[dependencies]
anyhow.workspace = true
gpui.workspace = true
log.workspace = true
markdown.workspace = true
serde.workspace = true
settings.workspace = true
util.workspace = true

[dev-dependencies]
gpui = { workspace = true, features = ["test-support"] }
markdown = { workspace = true, features = ["test-support"] }
settings = { workspace = true, features = ["test-support"] }
```

- [ ] **Step 3: Register the crate in the workspace**

In the root `Cargo.toml`, add to `members` in alphabetical position (between `"crates/recent_projects"` at line 170 and `"crates/refineable"` at line 171):

```toml
    "crates/read_aloud",
```

And in the workspace `[workspace.dependencies]` section, alphabetically near `markdown = { path = "crates/markdown" }` (line 399):

```toml
read_aloud = { path = "crates/read_aloud" }
```

- [ ] **Step 4: Add the settings content struct**

In `crates/settings_content/src/settings_content.rs`, add the field to `struct SettingsContent` immediately after the `pub audio:` field (line 177):

```rust
    /// Configuration for reading agent responses aloud.
    pub read_aloud: Option<ReadAloudSettingsContent>,
```

And add the struct definition next to `AudioSettingsContent` (around line 506):

```rust
/// Configuration for reading agent responses aloud.
#[with_fallible_options]
#[derive(Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema, MergeFrom, Debug)]
pub struct ReadAloudSettingsContent {
    /// Whether to read assistant responses aloud.
    ///
    /// Default: false
    pub enabled: Option<bool>,
    /// Whether to begin speaking automatically as a response streams in.
    ///
    /// Default: true
    pub auto_play: Option<bool>,
    /// Which text-to-speech provider to use.
    ///
    /// Default: inworld
    pub provider: Option<String>,
    /// The provider voice identifier.
    ///
    /// Default: Dennis
    pub voice_id: Option<String>,
    /// The provider model identifier.
    ///
    /// Default: inworld-tts-2
    pub model_id: Option<String>,
    /// Playback rate multiplier.
    ///
    /// Default: 1.0
    pub speaking_rate: Option<f32>,
}
```

- [ ] **Step 5: Add the default settings block**

In `assets/settings/default.json`, add after the `"audio"` block (which starts at line 576):

```jsonc
  "read_aloud": {
    "enabled": false,
    "auto_play": true,
    "provider": "inworld",
    "voice_id": "Dennis",
    "model_id": "inworld-tts-2",
    "speaking_rate": 1.0
  },
```

- [ ] **Step 6: Run the test to verify it passes**

Run: `~/.cargo/bin/cargo test -p read_aloud`

Expected: PASS — `defaults_are_inert_but_autoplay_once_enabled`.

If `content.read_aloud` does not resolve, the field in Step 4 was added to the wrong struct. It belongs on `SettingsContent`, not on a `#[serde(flatten)]` sub-struct.

- [ ] **Step 7: Verify default.json still parses**

Run: `~/.cargo/bin/cargo test -p settings`

Expected: PASS. Zed validates `default.json` against the generated JSON schema in these tests; a trailing-comma or misplaced-block error surfaces here.

- [ ] **Step 8: Commit**

```bash
git add crates/read_aloud Cargo.toml crates/settings_content/src/settings_content.rs assets/settings/default.json
git commit -m "read_aloud: Add crate skeleton and settings"
```

---

### Task 2: Segmenter

The heart of the feature and the unit that carries the test suite. Pure function, no I/O, no GPUI.

Two rules govern what gets spoken:

1. **Skip containers.** A text run is spoken only if no enclosing tag is a code block, table, HTML block, or metadata block.
2. **Only closed root blocks.** A sentence is emitted only from a root block the parser has already closed (`MarkdownEvent::RootEnd`). This is what stops a half-typed code fence from being spoken as prose while it streams.

**Files:**
- Modify: `crates/read_aloud/src/segmenter.rs` (created empty in Task 1)
- Modify: `crates/markdown/src/markdown.rs:1030` — remove the `#[cfg]` gate on `parsed_markdown()`
- Modify: `crates/read_aloud/Cargo.toml` — no change needed; `markdown` is already a dependency

**Interfaces:**
- Consumes: `markdown::parser::{MarkdownEvent, MarkdownTag}`, `markdown::ParsedMarkdown`.
- Produces:
  ```rust
  pub struct Utterance { pub source_range: Range<usize>, pub spoken_text: String }
  pub fn segment(parsed: &ParsedMarkdown) -> Vec<Utterance>
  ```
  Task 5 and Task 8 both depend on these exact names.

- [ ] **Step 1: Ungate `parsed_markdown()`**

In `crates/markdown/src/markdown.rs`, at line 1030, delete the `#[cfg(any(test, feature = "test-support"))]` attribute so the accessor is available in production:

```rust
    pub fn parsed_markdown(&self) -> &ParsedMarkdown {
        &self.parsed_markdown
    }
```

- [ ] **Step 2: Write the failing tests**

Replace the contents of `crates/read_aloud/src/segmenter.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use markdown::Markdown;

    fn utterances(source: &str, cx: &mut TestAppContext) -> Vec<Utterance> {
        let markdown = cx.new(|cx| Markdown::new_text(source.into(), cx));
        cx.run_until_parked();
        markdown.read_with(cx, |markdown, _| segment(markdown.parsed_markdown()))
    }

    fn spoken(source: &str, cx: &mut TestAppContext) -> Vec<String> {
        utterances(source, cx)
            .into_iter()
            .map(|utterance| utterance.spoken_text)
            .collect()
    }

    #[gpui::test]
    fn splits_a_paragraph_into_sentences(cx: &mut TestAppContext) {
        assert_eq!(
            spoken("First one. Second one! Third one?\n", cx),
            vec!["First one.", "Second one!", "Third one?"]
        );
    }

    #[gpui::test]
    fn source_ranges_point_at_original_bytes(cx: &mut TestAppContext) {
        let source = "Alpha. Beta.\n";
        let utterances = utterances(source, cx);
        assert_eq!(&source[utterances[0].source_range.clone()], "Alpha.");
        assert_eq!(&source[utterances[1].source_range.clone()], "Beta.");
    }

    #[gpui::test]
    fn skips_code_blocks(cx: &mut TestAppContext) {
        let source = "Before it.\n\n```rust\nlet x = 1. Not prose.\n```\n\nAfter it.\n";
        assert_eq!(spoken(source, cx), vec!["Before it.", "After it."]);
    }

    #[gpui::test]
    fn skips_tables(cx: &mut TestAppContext) {
        let source = "Intro here.\n\n| a | b |\n|---|---|\n| 1. | 2. |\n\nOutro here.\n";
        assert_eq!(spoken(source, cx), vec!["Intro here.", "Outro here."]);
    }

    #[gpui::test]
    fn speaks_headings_and_list_items(cx: &mut TestAppContext) {
        let source = "# A heading\n\n- First bullet.\n- Second bullet.\n";
        assert_eq!(
            spoken(source, cx),
            vec!["A heading", "First bullet.", "Second bullet."]
        );
    }

    #[gpui::test]
    fn speaks_nested_bullets(cx: &mut TestAppContext) {
        let source = "- Outer one.\n  - Inner one.\n";
        assert_eq!(spoken(source, cx), vec!["Outer one.", "Inner one."]);
    }

    #[gpui::test]
    fn strips_inline_markup_from_spoken_text(cx: &mut TestAppContext) {
        let source = "Use **bold** and `code` and [a link](https://example.com) here.\n";
        assert_eq!(
            spoken(source, cx),
            vec!["Use bold and code and a link here."]
        );
    }

    #[gpui::test]
    fn does_not_split_on_abbreviations(cx: &mut TestAppContext) {
        assert_eq!(
            spoken("Use e.g. this one. Or i.e. that one.\n", cx),
            vec!["Use e.g. this one.", "Or i.e. that one."]
        );
        assert_eq!(
            spoken("Ask Dr. Who about it.\n", cx),
            vec!["Ask Dr. Who about it."]
        );
    }

    #[gpui::test]
    fn does_not_split_on_decimals(cx: &mut TestAppContext) {
        assert_eq!(
            spoken("It costs 3.50 total.\n", cx),
            vec!["It costs 3.50 total."]
        );
    }

    #[gpui::test]
    fn does_not_split_on_file_extensions(cx: &mut TestAppContext) {
        assert_eq!(
            spoken("Edit main.rs and app.ts now.\n", cx),
            vec!["Edit main.rs and app.ts now."]
        );
    }

    #[gpui::test]
    fn does_not_split_on_ordered_list_markers(cx: &mut TestAppContext) {
        assert_eq!(
            spoken("1. First step here.\n2. Second step here.\n", cx),
            vec!["First step here.", "Second step here."]
        );
    }

    #[gpui::test]
    fn ignores_an_unterminated_trailing_sentence(cx: &mut TestAppContext) {
        assert_eq!(spoken("Complete one. Still typing", cx), vec!["Complete one."]);
    }

    #[gpui::test]
    fn ignores_an_unclosed_code_fence_while_streaming(cx: &mut TestAppContext) {
        let source = "Prose first.\n\n```rust\nlet total = 1. This is code.\n";
        assert_eq!(spoken(source, cx), vec!["Prose first."]);
    }

    #[gpui::test]
    fn produces_nothing_for_empty_input(cx: &mut TestAppContext) {
        assert_eq!(spoken("", cx), Vec::<String>::new());
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `~/.cargo/bin/cargo test -p read_aloud segmenter`

Expected: FAIL to compile — `cannot find function segment in this scope`, `cannot find type Utterance in this scope`.

- [ ] **Step 4: Implement the segmenter**

Prepend to `crates/read_aloud/src/segmenter.rs`, above the test module:

```rust
use markdown::{
    ParsedMarkdown,
    parser::{MarkdownEvent, MarkdownTag},
};
use std::ops::Range;

/// A single spoken unit: one sentence of assistant prose.
#[derive(Debug, Clone, PartialEq)]
pub struct Utterance {
    /// Byte range in the original markdown source. Drives both click-mapping
    /// and highlighting, so it must stay in original-source coordinates even
    /// though `spoken_text` has had markup stripped.
    pub source_range: Range<usize>,
    pub spoken_text: String,
}

/// Abbreviations that end in a period but do not end a sentence.
const ABBREVIATIONS: &[&str] = &[
    "e.g.", "i.e.", "etc.", "vs.", "cf.", "Dr.", "Mr.", "Mrs.", "Ms.", "Prof.", "St.", "approx.",
    "Fig.", "No.",
];

/// A run of spoken text paired with where it came from in the source.
struct TextRun {
    source_range: Range<usize>,
    text: String,
}

pub fn segment(parsed: &ParsedMarkdown) -> Vec<Utterance> {
    let source = parsed.source();
    let mut utterances = Vec::new();
    let mut tag_stack: Vec<MarkdownTag> = Vec::new();
    let mut runs: Vec<TextRun> = Vec::new();

    for (range, event) in parsed.events().iter() {
        match event {
            MarkdownEvent::Start(tag) => {
                if starts_spoken_block(tag) {
                    flush(&mut runs, &mut utterances);
                }
                tag_stack.push(tag.clone());
            }
            MarkdownEvent::End(_) => {
                let ended = tag_stack.pop();
                if ended.as_ref().is_some_and(starts_spoken_block) {
                    flush(&mut runs, &mut utterances);
                }
            }
            MarkdownEvent::RootEnd(_) => flush(&mut runs, &mut utterances),
            MarkdownEvent::Text | MarkdownEvent::Code => {
                if !is_muted(&tag_stack) {
                    if let Some(text) = source.get(range.clone()) {
                        runs.push(TextRun {
                            source_range: range.clone(),
                            text: text.to_string(),
                        });
                    }
                }
            }
            MarkdownEvent::SubstitutedText(text) | MarkdownEvent::SubstitutedCode(text) => {
                if !is_muted(&tag_stack) {
                    runs.push(TextRun {
                        source_range: range.clone(),
                        text: text.clone(),
                    });
                }
            }
            MarkdownEvent::SoftBreak | MarkdownEvent::HardBreak => {
                if !is_muted(&tag_stack) && !runs.is_empty() {
                    runs.push(TextRun {
                        source_range: range.clone(),
                        text: " ".to_string(),
                    });
                }
            }
            _ => {}
        }
    }

    // Anything still buffered belongs to a root block the parser has not closed
    // yet. Dropping it is what keeps a half-typed fence from being spoken.
    utterances
}

/// Root-level blocks whose text is prose worth speaking.
fn starts_spoken_block(tag: &MarkdownTag) -> bool {
    matches!(
        tag,
        MarkdownTag::Paragraph | MarkdownTag::Heading { .. } | MarkdownTag::Item
    )
}

/// True when any enclosing tag makes the text non-prose.
fn is_muted(tag_stack: &[MarkdownTag]) -> bool {
    tag_stack.iter().any(|tag| {
        matches!(
            tag,
            MarkdownTag::CodeBlock { .. }
                | MarkdownTag::HtmlBlock
                | MarkdownTag::MetadataBlock(_)
                | MarkdownTag::Table(_)
                | MarkdownTag::TableHead
                | MarkdownTag::TableRow
                | MarkdownTag::TableCell
                | MarkdownTag::Image { .. }
        )
    })
}

/// Turns the buffered runs into whole sentences, discarding any trailing
/// fragment that has no terminator yet.
fn flush(runs: &mut Vec<TextRun>, utterances: &mut Vec<Utterance>) {
    if runs.is_empty() {
        return;
    }

    let mut combined = String::new();
    // Maps each byte index in `combined` back to a source byte index.
    let mut origins: Vec<usize> = Vec::new();
    for run in runs.iter() {
        for (offset, character) in run.text.char_indices() {
            let source_index = run.source_range.start + offset.min(run.source_range.len());
            for _ in 0..character.len_utf8() {
                origins.push(source_index);
            }
        }
        combined.push_str(&run.text);
    }
    runs.clear();

    for sentence in split_sentences(&combined) {
        let text = combined[sentence.clone()].trim();
        if text.is_empty() {
            continue;
        }
        let leading = combined[sentence.clone()].len() - combined[sentence.clone()].trim_start().len();
        let start_index = sentence.start + leading;
        let end_index = start_index + text.len();
        let Some(&source_start) = origins.get(start_index) else {
            continue;
        };
        let source_end = origins
            .get(end_index.saturating_sub(1))
            .map_or(source_start, |last| last + 1);
        utterances.push(Utterance {
            source_range: source_start..source_end,
            spoken_text: text.to_string(),
        });
    }
}

/// Splits on `.`/`?`/`!` followed by whitespace, guarding abbreviations,
/// decimals, ordered-list markers, and file extensions. Any trailing fragment
/// without a terminator is dropped — it is still streaming.
fn split_sentences(text: &str) -> Vec<Range<usize>> {
    let bytes = text.as_bytes();
    let mut sentences = Vec::new();
    let mut start = 0;

    for (index, byte) in bytes.iter().enumerate() {
        if !matches!(byte, b'.' | b'?' | b'!') {
            continue;
        }
        let after = index + 1;
        let followed_by_whitespace = bytes
            .get(after)
            .is_some_and(|next| next.is_ascii_whitespace());
        if !followed_by_whitespace && after != bytes.len() {
            continue;
        }
        if *byte == b'.' && !terminates_sentence(text, index) {
            continue;
        }
        sentences.push(start..after);
        start = after;
    }

    sentences
}

/// Decides whether the period at `index` really ends a sentence.
fn terminates_sentence(text: &str, index: usize) -> bool {
    let bytes = text.as_bytes();
    let before = &text[..index + 1];

    if ABBREVIATIONS
        .iter()
        .any(|abbreviation| before.ends_with(abbreviation))
    {
        return false;
    }

    let previous = index.checked_sub(1).and_then(|i| bytes.get(i));
    let next = bytes.get(index + 1);

    // Decimals: 3.50 — digit on both sides.
    if previous.is_some_and(u8::is_ascii_digit) && next.is_some_and(u8::is_ascii_digit) {
        return false;
    }

    // Ordered list markers: "1." at the start of a run.
    if previous.is_some_and(u8::is_ascii_digit)
        && text[..index]
            .rfind(|c: char| !c.is_ascii_digit())
            .map_or(true, |i| {
                text[i..].starts_with(|c: char| c.is_whitespace()) || i + 1 == index
            })
        && text[..index].trim_start().chars().all(|c| c.is_ascii_digit())
    {
        return false;
    }

    // File extensions: word character on both sides with no space, e.g. main.rs
    if previous.is_some_and(|b| b.is_ascii_alphanumeric())
        && next.is_some_and(|b| b.is_ascii_alphabetic())
    {
        return false;
    }

    true
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `~/.cargo/bin/cargo test -p read_aloud segmenter`

Expected: PASS, all 13 tests.

If `does_not_split_on_ordered_list_markers` fails, note that `pulldown_cmark` consumes the `1. ` marker as list structure and never emits it as `Text`, so the guard may be redundant — in that case simplify `terminates_sentence` by deleting the ordered-list branch and re-run. Keep the test either way.

- [ ] **Step 6: Run clippy**

Run: `./script/clippy -p read_aloud`

Expected: no warnings.

- [ ] **Step 7: Verify the markdown crate still builds with the ungated accessor**

Run: `~/.cargo/bin/cargo build -p markdown && ~/.cargo/bin/cargo test -p markdown`

Expected: SUCCESS and PASS.

- [ ] **Step 8: Commit**

```bash
git add crates/read_aloud/src/segmenter.rs crates/markdown/src/markdown.rs
git commit -m "read_aloud: Add the prose segmenter"
```

---

### Task 3: PCM type, provider trait, and a fake

**Files:**
- Create: `crates/read_aloud/src/provider.rs`
- Modify: `crates/read_aloud/src/read_aloud.rs` (add `mod provider;`)

**Interfaces:**
- Consumes: nothing.
- Produces:
  ```rust
  pub struct Pcm { pub samples: Vec<f32>, pub sample_rate: u32, pub channels: u16 }
  pub trait TtsProvider: Send + Sync { fn synthesize(&self, text: String, cx: &App) -> Task<Result<Pcm>>; }
  pub struct FakeTts { /* test double */ }
  ```
  Tasks 5, 7, and 8 depend on these names.

- [ ] **Step 1: Write the failing test**

Create `crates/read_aloud/src/provider.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    async fn fake_provider_returns_one_sample_per_character(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let pcm = cx
            .update(|cx| provider.synthesize("hello".to_string(), cx))
            .await
            .unwrap();
        assert_eq!(pcm.samples.len(), 5);
        assert_eq!(pcm.sample_rate, 22050);
        assert_eq!(pcm.channels, 1);
    }

    #[gpui::test]
    async fn fake_provider_records_what_it_was_asked_to_say(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        cx.update(|cx| provider.synthesize("first".to_string(), cx))
            .await
            .unwrap();
        cx.update(|cx| provider.synthesize("second".to_string(), cx))
            .await
            .unwrap();
        assert_eq!(provider.spoken(), vec!["first", "second"]);
    }

    #[gpui::test]
    async fn fake_provider_can_be_told_to_fail(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        provider.fail_next();
        let result = cx
            .update(|cx| provider.synthesize("doomed".to_string(), cx))
            .await;
        assert!(result.is_err());
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `~/.cargo/bin/cargo test -p read_aloud provider`

Expected: FAIL to compile — `cannot find type FakeTts`.

- [ ] **Step 3: Implement the provider seam**

Prepend to `crates/read_aloud/src/provider.rs`:

```rust
use anyhow::{Result, anyhow};
use gpui::{App, Task};
use std::sync::{Arc, Mutex};

/// Raw uncompressed audio. Interleaved if `channels > 1`.
#[derive(Debug, Clone, PartialEq)]
pub struct Pcm {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
    pub channels: u16,
}

/// One seam so no TTS vendor is welded into the editor.
pub trait TtsProvider: Send + Sync + 'static {
    fn synthesize(&self, text: String, cx: &App) -> Task<Result<Pcm>>;
}

#[derive(Default)]
struct FakeTtsState {
    spoken: Vec<String>,
    fail_next: bool,
}

/// Test double. Produces one silent sample per character so queue ordering
/// and duration are predictable without touching the network.
#[derive(Clone, Default)]
pub struct FakeTts {
    state: Arc<Mutex<FakeTtsState>>,
}

impl FakeTts {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn spoken(&self) -> Vec<String> {
        self.state
            .lock()
            .map(|state| state.spoken.clone())
            .unwrap_or_default()
    }

    pub fn fail_next(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.fail_next = true;
        }
    }
}

impl TtsProvider for FakeTts {
    fn synthesize(&self, text: String, _cx: &App) -> Task<Result<Pcm>> {
        let Ok(mut state) = self.state.lock() else {
            return Task::ready(Err(anyhow!("FakeTts state poisoned")));
        };
        if std::mem::take(&mut state.fail_next) {
            return Task::ready(Err(anyhow!("FakeTts was told to fail")));
        }
        state.spoken.push(text.clone());
        Task::ready(Ok(Pcm {
            samples: vec![0.0; text.chars().count()],
            sample_rate: 22050,
            channels: 1,
        }))
    }
}
```

Add `mod provider;` to the top of `crates/read_aloud/src/read_aloud.rs`, above `mod segmenter;`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `~/.cargo/bin/cargo test -p read_aloud provider`

Expected: PASS, all three tests.

- [ ] **Step 5: Commit**

```bash
git add crates/read_aloud/src/provider.rs crates/read_aloud/src/read_aloud.rs
git commit -m "read_aloud: Add the TTS provider seam"
```

---

### Task 4: Audio sink seam

`crates/audio` currently exposes no way to play arbitrary samples — `Audio::play_sound` only accepts the 8-variant `Sound` enum of bundled WAVs, and `ensure_output_exists()` (which returns the `&Mixer` this feature needs) is private. This task adds the missing public seam and wraps it behind a trait so the player is testable without a sound card.

`rodio::Player::connect_new(&Mixer)` is the right primitive: it gives an independent queue with `append`, `clear`, `skip_one`, `len`, `set_speed`, and `stop`, mixed into Zed's existing output stream rather than opening a competing one.

**Files:**
- Modify: `crates/audio/src/audio_pipeline.rs`
- Create: `crates/read_aloud/src/sink.rs`
- Modify: `crates/read_aloud/src/read_aloud.rs` (add `mod sink;`)
- Modify: `crates/read_aloud/Cargo.toml` (add `audio` and `rodio`)

**Interfaces:**
- Consumes: `provider::Pcm` from Task 3.
- Produces:
  ```rust
  // in crates/audio
  impl Audio { pub fn connect_player(cx: &mut App) -> Option<rodio::Player> }

  // in crates/read_aloud
  pub trait AudioSink: 'static {
      fn append(&self, pcm: Pcm);
      fn clear(&self);
      fn queued(&self) -> usize;
      fn set_speed(&self, speed: f32);
      fn stop(&self);
  }
  pub struct RodioSink(rodio::Player);
  pub struct FakeSink { /* test double */ }
  ```
  Task 5 depends on `AudioSink` and `FakeSink`.

- [ ] **Step 1: Write the failing test**

Create `crates/read_aloud/src/sink.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn pcm(sample_count: usize) -> Pcm {
        Pcm {
            samples: vec![0.0; sample_count],
            sample_rate: 22050,
            channels: 1,
        }
    }

    #[test]
    fn fake_sink_tracks_queue_depth() {
        let sink = FakeSink::new();
        assert_eq!(sink.queued(), 0);
        sink.append(pcm(4));
        sink.append(pcm(4));
        assert_eq!(sink.queued(), 2);
    }

    #[test]
    fn fake_sink_clear_empties_the_queue() {
        let sink = FakeSink::new();
        sink.append(pcm(4));
        sink.clear();
        assert_eq!(sink.queued(), 0);
    }

    #[test]
    fn fake_sink_can_finish_one_item_at_a_time() {
        let sink = FakeSink::new();
        sink.append(pcm(1));
        sink.append(pcm(2));
        sink.finish_one();
        assert_eq!(sink.queued(), 1);
        sink.finish_one();
        assert_eq!(sink.queued(), 0);
    }

    #[test]
    fn fake_sink_records_speed_changes() {
        let sink = FakeSink::new();
        sink.set_speed(1.5);
        assert_eq!(sink.speed(), 1.5);
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `~/.cargo/bin/cargo test -p read_aloud sink`

Expected: FAIL to compile — `cannot find type FakeSink`.

- [ ] **Step 3: Add the public seam to `crates/audio`**

In `crates/audio/src/audio_pipeline.rs`, inside `impl Audio`, add immediately after `play_sound` (which ends around line 88):

```rust
    /// Connects an independent playback queue to the shared output mixer.
    ///
    /// Callers that need to play arbitrary samples use this instead of
    /// `play_sound`, which only handles the bundled `Sound` assets. Returns
    /// `None` when no output device could be opened.
    pub fn connect_player(cx: &mut App) -> Option<rodio::Player> {
        let output_audio_device = AudioSettings::get_global(cx).output_audio_device.clone();
        cx.update_default_global(|this: &mut Self, _cx| {
            let output_mixer = this
                .ensure_output_exists(output_audio_device)
                .context("Could not get output mixer")
                .log_err()?;
            Some(rodio::Player::connect_new(output_mixer))
        })
    }
```

Confirm `use anyhow::Context;` and `use util::ResultExt;` are already imported in that file — both are, at the existing `play_sound` call sites.

- [ ] **Step 4: Add the dependencies**

In `crates/read_aloud/Cargo.toml`, add to `[dependencies]`:

```toml
audio.workspace = true
rodio.workspace = true
```

- [ ] **Step 5: Implement the sink seam**

Prepend to `crates/read_aloud/src/sink.rs`:

```rust
use crate::provider::Pcm;
use std::sync::{Arc, Mutex};

/// Playback seam. `RodioSink` is the real one; `FakeSink` keeps the player's
/// tests free of any dependency on a sound card.
pub trait AudioSink: 'static {
    fn append(&self, pcm: Pcm);
    fn clear(&self);
    /// Number of utterances still queued, including the one playing.
    fn queued(&self) -> usize;
    fn set_speed(&self, speed: f32);
    fn stop(&self);
}

pub struct RodioSink(rodio::Player);

impl RodioSink {
    pub fn new(player: rodio::Player) -> Self {
        Self(player)
    }
}

impl AudioSink for RodioSink {
    fn append(&self, pcm: Pcm) {
        let Some(sample_rate) = std::num::NonZero::new(pcm.sample_rate) else {
            log::error!("read_aloud: refusing to play PCM with a zero sample rate");
            return;
        };
        let Some(channels) = std::num::NonZero::new(pcm.channels) else {
            log::error!("read_aloud: refusing to play PCM with zero channels");
            return;
        };
        self.0.append(rodio::buffer::SamplesBuffer::new(
            channels,
            sample_rate,
            pcm.samples,
        ));
    }

    fn clear(&self) {
        self.0.clear();
        // `clear` leaves the player stopped; playback must be re-armed or the
        // next `append` is silent.
        self.0.play();
    }

    fn queued(&self) -> usize {
        self.0.len()
    }

    fn set_speed(&self, speed: f32) {
        self.0.set_speed(speed);
    }

    fn stop(&self) {
        self.0.stop();
    }
}

#[derive(Default)]
struct FakeSinkState {
    queue: Vec<Pcm>,
    speed: f32,
    stopped: bool,
}

#[derive(Clone)]
pub struct FakeSink {
    state: Arc<Mutex<FakeSinkState>>,
}

impl FakeSink {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(FakeSinkState {
                queue: Vec::new(),
                speed: 1.0,
                stopped: false,
            })),
        }
    }

    /// Simulates the head of the queue finishing playback.
    pub fn finish_one(&self) {
        if let Ok(mut state) = self.state.lock() {
            if !state.queue.is_empty() {
                state.queue.remove(0);
            }
        }
    }

    pub fn speed(&self) -> f32 {
        self.state.lock().map(|state| state.speed).unwrap_or(1.0)
    }

    pub fn is_stopped(&self) -> bool {
        self.state.lock().map(|state| state.stopped).unwrap_or(false)
    }
}

impl Default for FakeSink {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioSink for FakeSink {
    fn append(&self, pcm: Pcm) {
        if let Ok(mut state) = self.state.lock() {
            state.stopped = false;
            state.queue.push(pcm);
        }
    }

    fn clear(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.queue.clear();
        }
    }

    fn queued(&self) -> usize {
        self.state.lock().map(|state| state.queue.len()).unwrap_or(0)
    }

    fn set_speed(&self, speed: f32) {
        if let Ok(mut state) = self.state.lock() {
            state.speed = speed;
        }
    }

    fn stop(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.queue.clear();
            state.stopped = true;
        }
    }
}
```

Add `mod sink;` to `crates/read_aloud/src/read_aloud.rs`.

- [ ] **Step 6: Run the tests to verify they pass**

Run: `~/.cargo/bin/cargo test -p read_aloud sink`

Expected: PASS, all four tests.

- [ ] **Step 7: Verify the audio crate still builds**

Run: `~/.cargo/bin/cargo build -p audio && ./script/clippy -p audio -p read_aloud`

Expected: SUCCESS, no warnings.

- [ ] **Step 8: Commit**

```bash
git add crates/audio/src/audio_pipeline.rs crates/read_aloud/src/sink.rs crates/read_aloud/src/read_aloud.rs crates/read_aloud/Cargo.toml
git commit -m "read_aloud: Add an arbitrary-PCM audio seam"
```

---

### Task 5: Player

Owns the utterance queue. Synthesizes ahead by one, advances position as the sink drains, and supports seeking to an arbitrary index.

Position is derived, not tracked independently: `current_index = enqueued_count - sink.queued()`. This is the only reliable signal, because `rodio::Player` reports queue depth but emits no completion callback.

**Files:**
- Create: `crates/read_aloud/src/player.rs`
- Modify: `crates/read_aloud/src/read_aloud.rs` (add `mod player;`)

**Interfaces:**
- Consumes: `segmenter::Utterance`, `provider::{Pcm, TtsProvider, FakeTts}`, `sink::{AudioSink, FakeSink}`.
- Produces:
  ```rust
  pub enum PlayerEvent { Speaking(usize), Finished }
  pub struct Player { .. }
  impl Player {
      pub fn new(provider: Arc<dyn TtsProvider>, sink: Box<dyn AudioSink>, cx: &mut Context<Self>) -> Self;
      pub fn set_utterances(&mut self, utterances: Vec<Utterance>, cx: &mut Context<Self>);
      pub fn seek_to(&mut self, index: usize, cx: &mut Context<Self>);
      pub fn stop(&mut self, cx: &mut Context<Self>);
      pub fn speaking_index(&self) -> Option<usize>;
      pub fn set_speed(&mut self, speed: f32);
  }
  impl EventEmitter<PlayerEvent> for Player {}
  ```
  Task 8 depends on all of these.

- [ ] **Step 1: Write the failing tests**

Create `crates/read_aloud/src/player.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::FakeTts;
    use crate::sink::FakeSink;
    use gpui::TestAppContext;

    fn utterance(text: &str, start: usize) -> Utterance {
        Utterance {
            source_range: start..start + text.len(),
            spoken_text: text.to_string(),
        }
    }

    fn setup(cx: &mut TestAppContext) -> (Entity<Player>, FakeTts, FakeSink) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let player = cx.new({
            let provider = provider.clone();
            let sink = sink.clone();
            |cx| Player::new(Arc::new(provider), Box::new(sink), cx)
        });
        (player, provider, sink)
    }

    #[gpui::test]
    async fn synthesizes_in_queue_order(cx: &mut TestAppContext) {
        let (player, provider, _sink) = setup(cx);
        player.update(cx, |player, cx| {
            player.set_utterances(
                vec![utterance("First.", 0), utterance("Second.", 7)],
                cx,
            );
        });
        cx.run_until_parked();
        assert_eq!(provider.spoken(), vec!["First.", "Second."]);
    }

    #[gpui::test]
    async fn reports_the_speaking_index(cx: &mut TestAppContext) {
        let (player, _provider, sink) = setup(cx);
        player.update(cx, |player, cx| {
            player.set_utterances(
                vec![utterance("One.", 0), utterance("Two.", 5)],
                cx,
            );
        });
        cx.run_until_parked();
        assert_eq!(player.read_with(cx, |player, _| player.speaking_index()), Some(0));

        sink.finish_one();
        player.update(cx, |player, cx| player.poll_position(cx));
        assert_eq!(player.read_with(cx, |player, _| player.speaking_index()), Some(1));
    }

    #[gpui::test]
    async fn seek_discards_the_queue_and_resumes_from_the_target(cx: &mut TestAppContext) {
        let (player, provider, sink) = setup(cx);
        player.update(cx, |player, cx| {
            player.set_utterances(
                vec![utterance("A.", 0), utterance("B.", 3), utterance("C.", 6)],
                cx,
            );
        });
        cx.run_until_parked();

        player.update(cx, |player, cx| player.seek_to(2, cx));
        cx.run_until_parked();

        assert_eq!(player.read_with(cx, |player, _| player.speaking_index()), Some(2));
        assert_eq!(sink.queued(), 1, "seek must discard everything before the target");
        assert_eq!(
            provider.spoken().last().map(String::as_str),
            Some("C."),
            "seek must synthesize the target utterance"
        );
    }

    #[gpui::test]
    async fn a_synthesis_failure_does_not_stall_the_queue(cx: &mut TestAppContext) {
        let (player, provider, _sink) = setup(cx);
        provider.fail_next();
        player.update(cx, |player, cx| {
            player.set_utterances(
                vec![utterance("Doomed.", 0), utterance("Survivor.", 8)],
                cx,
            );
        });
        cx.run_until_parked();
        assert_eq!(
            provider.spoken(),
            vec!["Survivor."],
            "the failed utterance is skipped and the queue keeps moving"
        );
    }

    #[gpui::test]
    async fn stop_clears_everything(cx: &mut TestAppContext) {
        let (player, _provider, sink) = setup(cx);
        player.update(cx, |player, cx| {
            player.set_utterances(vec![utterance("A.", 0), utterance("B.", 3)], cx);
        });
        cx.run_until_parked();

        player.update(cx, |player, cx| player.stop(cx));
        assert!(sink.is_stopped());
        assert_eq!(player.read_with(cx, |player, _| player.speaking_index()), None);
    }

    #[gpui::test]
    async fn appending_utterances_does_not_resynthesize_earlier_ones(cx: &mut TestAppContext) {
        let (player, provider, _sink) = setup(cx);
        player.update(cx, |player, cx| {
            player.set_utterances(vec![utterance("First.", 0)], cx);
        });
        cx.run_until_parked();

        player.update(cx, |player, cx| {
            player.set_utterances(
                vec![utterance("First.", 0), utterance("Second.", 7)],
                cx,
            );
        });
        cx.run_until_parked();

        assert_eq!(
            provider.spoken(),
            vec!["First.", "Second."],
            "streaming appends must not re-speak what was already queued"
        );
    }

    #[gpui::test]
    async fn speed_is_forwarded_to_the_sink(cx: &mut TestAppContext) {
        let (player, _provider, sink) = setup(cx);
        player.update(cx, |player, _cx| player.set_speed(1.25));
        assert_eq!(sink.speed(), 1.25);
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `~/.cargo/bin/cargo test -p read_aloud player`

Expected: FAIL to compile — `cannot find type Player`.

- [ ] **Step 3: Implement the player**

Prepend to `crates/read_aloud/src/player.rs`:

```rust
use crate::provider::TtsProvider;
use crate::segmenter::Utterance;
use crate::sink::AudioSink;
use gpui::{Context, Entity, EventEmitter, Task};
use std::sync::Arc;
use util::ResultExt as _;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayerEvent {
    Speaking(usize),
    Finished,
}

pub struct Player {
    provider: Arc<dyn TtsProvider>,
    sink: Box<dyn AudioSink>,
    utterances: Vec<Utterance>,
    /// Index of the next utterance to synthesize.
    next_to_synthesize: usize,
    /// Index of the utterance at the head of the sink's queue.
    queue_head: usize,
    last_reported: Option<usize>,
    synthesis: Option<Task<()>>,
}

impl EventEmitter<PlayerEvent> for Player {}

impl Player {
    pub fn new(
        provider: Arc<dyn TtsProvider>,
        sink: Box<dyn AudioSink>,
        _cx: &mut Context<Self>,
    ) -> Self {
        Self {
            provider,
            sink,
            utterances: Vec::new(),
            next_to_synthesize: 0,
            queue_head: 0,
            last_reported: None,
            synthesis: None,
        }
    }

    /// Replaces the utterance list. Utterances already synthesized keep their
    /// place, so a streaming append only synthesizes what is new.
    pub fn set_utterances(&mut self, utterances: Vec<Utterance>, cx: &mut Context<Self>) {
        self.utterances = utterances;
        if self.next_to_synthesize > self.utterances.len() {
            self.next_to_synthesize = self.utterances.len();
        }
        self.pump(cx);
    }

    pub fn seek_to(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.utterances.len() {
            return;
        }
        self.sink.clear();
        self.queue_head = index;
        self.next_to_synthesize = index;
        self.last_reported = None;
        self.pump(cx);
    }

    pub fn stop(&mut self, cx: &mut Context<Self>) {
        self.synthesis = None;
        self.sink.stop();
        self.queue_head = self.utterances.len();
        self.next_to_synthesize = self.utterances.len();
        self.last_reported = None;
        cx.emit(PlayerEvent::Finished);
        cx.notify();
    }

    pub fn set_speed(&mut self, speed: f32) {
        self.sink.set_speed(speed);
    }

    pub fn speaking_index(&self) -> Option<usize> {
        if self.sink.queued() == 0 {
            return None;
        }
        (self.queue_head < self.utterances.len()).then_some(self.queue_head)
    }

    pub fn utterances(&self) -> &[Utterance] {
        &self.utterances
    }

    /// Recomputes the speaking index from how far the sink has drained.
    /// Called on a timer by the owning entity, and directly in tests.
    pub fn poll_position(&mut self, cx: &mut Context<Self>) {
        let queued = self.sink.queued();
        let head = self.next_to_synthesize.saturating_sub(queued);
        if head != self.queue_head {
            self.queue_head = head;
        }

        let speaking = self.speaking_index();
        if speaking != self.last_reported {
            self.last_reported = speaking;
            match speaking {
                Some(index) => cx.emit(PlayerEvent::Speaking(index)),
                None => cx.emit(PlayerEvent::Finished),
            }
            cx.notify();
        }
    }

    /// Keeps one utterance synthesized ahead of the one playing.
    fn pump(&mut self, cx: &mut Context<Self>) {
        const PREFETCH: usize = 2;

        if self.synthesis.is_some() {
            return;
        }
        if self.next_to_synthesize >= self.utterances.len() {
            return;
        }
        if self.sink.queued() >= PREFETCH {
            return;
        }

        let index = self.next_to_synthesize;
        let Some(utterance) = self.utterances.get(index) else {
            return;
        };
        let text = utterance.spoken_text.clone();
        let provider = self.provider.clone();

        self.synthesis = Some(cx.spawn(async move |this, cx| {
            let synthesized = match cx.update(|cx| provider.synthesize(text, cx)) {
                Ok(task) => task.await,
                Err(error) => Err(error),
            };

            this.update(cx, |this, cx| {
                this.synthesis = None;
                this.next_to_synthesize = this.next_to_synthesize.max(index + 1);
                match synthesized {
                    Ok(pcm) => this.sink.append(pcm),
                    Err(error) => {
                        // A failed utterance is skipped, never retried, and never
                        // allowed to block the ones behind it.
                        log::warn!("read_aloud: synthesis failed, skipping: {error:#}");
                    }
                }
                this.poll_position(cx);
                this.pump(cx);
            })
            .log_err();
        }));
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `~/.cargo/bin/cargo test -p read_aloud player`

Expected: PASS, all seven tests.

If `a_synthesis_failure_does_not_stall_the_queue` fails because `next_to_synthesize` did not advance past the failure, confirm the `max(index + 1)` assignment runs on **both** the `Ok` and `Err` arms — it is deliberately outside the `match`.

- [ ] **Step 5: Run clippy**

Run: `./script/clippy -p read_aloud`

Expected: no warnings.

- [ ] **Step 6: Commit**

```bash
git add crates/read_aloud/src/player.rs crates/read_aloud/src/read_aloud.rs
git commit -m "read_aloud: Add the utterance player"
```

---

### Task 6: Speaking highlight in the markdown renderer

A **parallel** channel to the existing search highlight, not a reuse — find-in-thread must keep working while audio plays.

The existing search path is the template: field at `crates/markdown/src/markdown.rs:479`, setters at `:1064`, paint at `:2055`, paint call at `:3176`.

**Files:**
- Modify: `crates/markdown/src/markdown.rs`

**Interfaces:**
- Consumes: nothing.
- Produces:
  ```rust
  impl Markdown {
      pub fn set_speaking_highlight(&mut self, range: Option<Range<usize>>, cx: &mut Context<Self>);
      pub fn speaking_highlight(&self) -> Option<&Range<usize>>;
  }
  ```
  Task 8 depends on these.

- [ ] **Step 1: Write the failing test**

In `crates/markdown/src/markdown.rs`, add to the existing `#[cfg(test)] mod tests` (next to `test_active_search_highlight_uses_match_index` at line 4700):

```rust
    #[gpui::test]
    fn test_speaking_highlight_is_independent_of_search(cx: &mut TestAppContext) {
        let markdown = cx.new(|cx| Markdown::new_text("one two three".into(), cx));
        markdown.update(cx, |markdown, cx| {
            markdown.set_search_highlights(vec![0..3], Some(0), cx);
            markdown.set_speaking_highlight(Some(4..7), cx);

            assert_eq!(markdown.search_highlights(), &[0..3]);
            assert_eq!(markdown.speaking_highlight(), Some(&(4..7)));

            markdown.clear_search_highlights(cx);
            assert_eq!(
                markdown.speaking_highlight(),
                Some(&(4..7)),
                "clearing search must not clear the speaking highlight"
            );

            markdown.set_speaking_highlight(None, cx);
            assert_eq!(markdown.speaking_highlight(), None);
        });
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `~/.cargo/bin/cargo test -p markdown test_speaking_highlight`

Expected: FAIL to compile — `no method named set_speaking_highlight`.

- [ ] **Step 3: Add the field**

In `crates/markdown/src/markdown.rs`, after `active_search_highlight: Option<usize>,` (line 480):

```rust
    speaking_highlight: Option<Range<usize>>,
```

And in the constructor's initializer list, after `active_search_highlight: None,` (line 675):

```rust
            speaking_highlight: None,
```

- [ ] **Step 4: Add the accessors**

After `active_search_highlight()` (which ends around line 1103):

```rust
    /// A parallel channel to the search highlight so find-in-thread keeps
    /// working while audio plays.
    pub fn set_speaking_highlight(&mut self, range: Option<Range<usize>>, cx: &mut Context<Self>) {
        if self.speaking_highlight != range {
            self.speaking_highlight = range;
            cx.notify();
        }
    }

    pub fn speaking_highlight(&self) -> Option<&Range<usize>> {
        self.speaking_highlight.as_ref()
    }
```

- [ ] **Step 5: Paint it**

After `paint_search_highlights` (which ends around line 2086), add:

```rust
    fn paint_speaking_highlight(
        &self,
        rendered_text: &RenderedText,
        window: &mut Window,
        cx: &mut App,
    ) {
        let markdown = self.markdown.read(cx);
        let Some(range) = markdown.speaking_highlight.clone() else {
            return;
        };
        let color = cx.theme().colors().editor_document_highlight_read_background;

        let highlight_bounds =
            rendered_text.bounds_for_sorted_source_ranges(std::iter::once((0usize, range)));
        for (_, bounds) in highlight_bounds {
            window.paint_quad(quad(
                bounds,
                Pixels::ZERO,
                color,
                Edges::default(),
                Hsla::transparent_black(),
                BorderStyle::default(),
            ));
        }
    }
```

And call it at line 3176, immediately **before** the search highlight so an active search match paints on top:

```rust
        self.paint_speaking_highlight(&rendered_markdown.text, window, cx);
        self.paint_search_highlights(&rendered_markdown.text, window, cx);
        self.paint_selection(&rendered_markdown.text, window, cx);
```

- [ ] **Step 6: Run the test to verify it passes**

Run: `~/.cargo/bin/cargo test -p markdown test_speaking_highlight`

Expected: PASS.

- [ ] **Step 7: Verify nothing else in the markdown crate regressed**

Run: `~/.cargo/bin/cargo test -p markdown && ./script/clippy -p markdown`

Expected: PASS, no warnings.

If `editor_document_highlight_read_background` does not exist on the theme's color struct, pick another subtle background from `cx.theme().colors()` — grep `crates/theme/src/` for available fields. Do not reuse `search_match_background`; the whole point is that the two channels are visually distinguishable.

- [ ] **Step 8: Commit**

```bash
git add crates/markdown/src/markdown.rs
git commit -m "markdown: Add a speaking highlight channel"
```

---

### Task 7: Inworld provider and API key resolution

`POST https://api.inworld.ai/tts/v1/voice:stream` returns streamed JSON with base64 `LINEAR16` audio in `result.audioContent`. `LINEAR16` is raw little-endian signed 16-bit PCM, so no decoder is needed — just a widening conversion to `f32`.

The key is never read from `settings.json`. Resolution order: `INWORLD_API_KEY` environment variable, then the GPUI keychain credential store.

**Files:**
- Create: `crates/read_aloud/src/inworld.rs`
- Modify: `crates/read_aloud/src/read_aloud.rs` (add `mod inworld;`)
- Modify: `crates/read_aloud/Cargo.toml`

**Interfaces:**
- Consumes: `provider::{Pcm, TtsProvider}` from Task 3.
- Produces:
  ```rust
  pub const INWORLD_CREDENTIALS_URL: &str = "https://api.inworld.ai";
  pub struct InworldTts { .. }
  impl InworldTts {
      pub fn new(client: Arc<dyn HttpClient>, api_key: String, voice_id: String, model_id: String) -> Self;
  }
  pub fn resolve_api_key(cx: &App) -> Task<Result<String>>;
  pub fn decode_linear16(bytes: &[u8]) -> Vec<f32>;   // pub(crate), tested directly
  ```
  Task 8 depends on `InworldTts::new` and `resolve_api_key`.

- [ ] **Step 1: Add the dependencies**

In `crates/read_aloud/Cargo.toml`, add to `[dependencies]`:

```toml
base64.workspace = true
futures.workspace = true
http_client.workspace = true
serde_json.workspace = true
```

- [ ] **Step 2: Write the failing tests**

Create `crates/read_aloud/src/inworld.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_linear16_to_normalized_floats() {
        // i16 little-endian: 0, 32767, -32768
        let bytes = [0x00, 0x00, 0xff, 0x7f, 0x00, 0x80];
        let samples = decode_linear16(&bytes);
        assert_eq!(samples.len(), 3);
        assert!((samples[0] - 0.0).abs() < 1e-6);
        assert!((samples[1] - 1.0).abs() < 1e-4);
        assert!((samples[2] + 1.0).abs() < 1e-4);
    }

    #[test]
    fn ignores_a_trailing_odd_byte() {
        let samples = decode_linear16(&[0x00, 0x00, 0x01]);
        assert_eq!(samples.len(), 1, "a dangling byte is not half a sample");
    }

    #[test]
    fn extracts_audio_from_streamed_json_lines() {
        // "AAA=" is base64 for two zero bytes -> one zero sample.
        let body = "{\"result\":{\"audioContent\":\"AAA=\"}}\n\
                    {\"result\":{\"audioContent\":\"AAA=\"}}\n";
        let samples = collect_audio_content(body).unwrap();
        assert_eq!(samples.len(), 2);
    }

    #[test]
    fn tolerates_blank_and_malformed_lines() {
        let body = "\n{\"result\":{\"audioContent\":\"AAA=\"}}\nnot json\n{}\n";
        let samples = collect_audio_content(body).unwrap();
        assert_eq!(samples.len(), 1, "one good line still yields its audio");
    }

    #[test]
    fn errors_when_the_response_contains_no_audio() {
        assert!(collect_audio_content("{}\n").is_err());
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `~/.cargo/bin/cargo test -p read_aloud inworld`

Expected: FAIL to compile — `cannot find function decode_linear16`.

- [ ] **Step 4: Implement the provider**

Prepend to `crates/read_aloud/src/inworld.rs`:

```rust
use crate::provider::{Pcm, TtsProvider};
use anyhow::{Context as _, Result, anyhow};
use base64::Engine as _;
use futures::AsyncReadExt as _;
use gpui::{App, Task};
use http_client::{AsyncBody, HttpClient, Method, Request as HttpRequest};
use std::sync::Arc;

pub const INWORLD_API_URL: &str = "https://api.inworld.ai/tts/v1/voice:stream";
pub const INWORLD_CREDENTIALS_URL: &str = "https://api.inworld.ai";
const INWORLD_API_KEY_VAR: &str = "INWORLD_API_KEY";
const SAMPLE_RATE: u32 = 22050;
const RATE_LIMIT_MAX_RETRIES: u32 = 2;
const RATE_LIMIT_INITIAL_BACKOFF: std::time::Duration = std::time::Duration::from_millis(500);

pub struct InworldTts {
    client: Arc<dyn HttpClient>,
    api_key: String,
    voice_id: String,
    model_id: String,
}

impl InworldTts {
    pub fn new(
        client: Arc<dyn HttpClient>,
        api_key: String,
        voice_id: String,
        model_id: String,
    ) -> Self {
        Self {
            client,
            api_key,
            voice_id,
            model_id,
        }
    }
}

impl TtsProvider for InworldTts {
    fn synthesize(&self, text: String, cx: &App) -> Task<Result<Pcm>> {
        let client = self.client.clone();
        let api_key = self.api_key.clone();
        let voice_id = self.voice_id.clone();
        let model_id = self.model_id.clone();

        let executor = cx.background_executor().clone();
        cx.background_spawn(async move {
            let body = serde_json::json!({
                "text": text,
                "voiceId": voice_id,
                "modelId": model_id,
                "audioConfig": {
                    "audioEncoding": "LINEAR16",
                    "sampleRateHertz": SAMPLE_RATE,
                },
                "deliveryMode": "BALANCED",
            });
            let body = serde_json::to_string(&body)?;

            let mut backoff = RATE_LIMIT_INITIAL_BACKOFF;
            for attempt in 0..=RATE_LIMIT_MAX_RETRIES {
                let request = HttpRequest::builder()
                    .method(Method::POST)
                    .uri(INWORLD_API_URL)
                    .header("Content-Type", "application/json")
                    .header("Authorization", format!("Basic {}", api_key.trim()))
                    .body(AsyncBody::from(body.clone()))?;

                let mut response = client.send(request).await?;
                let status = response.status();
                let mut text_body = String::new();
                response.body_mut().read_to_string(&mut text_body).await?;

                if status.is_success() {
                    return Ok(Pcm {
                        samples: collect_audio_content(&text_body)?,
                        sample_rate: SAMPLE_RATE,
                        channels: 1,
                    });
                }

                // Back off on rate limits, but give up quickly rather than
                // holding the queue. A dropped utterance is better than a
                // stalled panel.
                if status == http_client::StatusCode::TOO_MANY_REQUESTS
                    && attempt < RATE_LIMIT_MAX_RETRIES
                {
                    log::warn!("read_aloud: Inworld rate limited, retrying in {backoff:?}");
                    executor.timer(backoff).await;
                    backoff *= 2;
                    continue;
                }

                return Err(anyhow!(
                    "Inworld TTS returned {status}: {}",
                    text_body.trim()
                ));
            }

            Err(anyhow!("Inworld TTS exhausted rate-limit retries"))
        })
    }
}

/// Walks the streamed JSON-lines body, concatenating every `result.audioContent`
/// chunk. Malformed lines are skipped rather than failing the whole utterance.
fn collect_audio_content(body: &str) -> Result<Vec<f32>> {
    let mut samples = Vec::new();
    let mut found_any = false;

    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(encoded) = value
            .get("result")
            .and_then(|result| result.get("audioContent"))
            .and_then(|content| content.as_str())
        else {
            continue;
        };
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .context("Inworld returned audioContent that is not valid base64")?;
        samples.extend(decode_linear16(&decoded));
        found_any = true;
    }

    if !found_any {
        return Err(anyhow!("Inworld response contained no audio content"));
    }
    Ok(samples)
}

/// LINEAR16 is little-endian signed 16-bit PCM. A trailing odd byte is not a
/// sample and is discarded.
fn decode_linear16(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(2)
        .map(|pair| i16::from_le_bytes([pair[0], pair[1]]) as f32 / 32768.0)
        .collect()
}

/// Environment variable first, then the keychain. Never `settings.json`.
pub fn resolve_api_key(cx: &App) -> Task<Result<String>> {
    if let Ok(key) = std::env::var(INWORLD_API_KEY_VAR) {
        let key = key.trim().to_string();
        if !key.is_empty() {
            return Task::ready(Ok(key));
        }
    }

    let credentials = cx.read_credentials(INWORLD_CREDENTIALS_URL);
    cx.background_spawn(async move {
        let (_username, secret) = credentials
            .await?
            .context("No Inworld API key found. Set INWORLD_API_KEY or store one in the keychain.")?;
        Ok(String::from_utf8(secret)?.trim().to_string())
    })
}
```

Add `mod inworld;` to `crates/read_aloud/src/read_aloud.rs`.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `~/.cargo/bin/cargo test -p read_aloud inworld`

Expected: PASS, all five tests.

- [ ] **Step 6: Run clippy**

Run: `./script/clippy -p read_aloud`

Expected: no warnings.

- [ ] **Step 7: Commit**

```bash
git add crates/read_aloud/src/inworld.rs crates/read_aloud/src/read_aloud.rs crates/read_aloud/Cargo.toml
git commit -m "read_aloud: Add the Inworld TTS provider"
```

---

### Task 8: The `ReadAloud` entity

One entity per thread view. Owns the `Player`, maps assistant chunks to their `Entity<Markdown>`, re-segments on chunk updates, and pushes the speaking highlight.

Chunk mapping mirrors `collect_markdowns()` in `crates/agent_ui/src/conversation_view/thread_search_bar.rs:882`, minus the thought and tool-call branches — this feature speaks `AssistantMessageChunk::Message` only.

**Files:**
- Modify: `crates/read_aloud/src/read_aloud.rs`
- Modify: `crates/read_aloud/Cargo.toml`

**Interfaces:**
- Consumes: `segmenter::segment`, `player::{Player, PlayerEvent}`, `provider::TtsProvider`, `sink::AudioSink`, `markdown::Markdown::set_speaking_highlight` (Task 6).
- Produces:
  ```rust
  pub struct ReadAloud { .. }
  impl ReadAloud {
      pub fn new(provider: Arc<dyn TtsProvider>, sink: Box<dyn AudioSink>, cx: &mut Context<Self>) -> Self;
      pub fn enqueue_markdown(&mut self, markdown: &Entity<Markdown>, cx: &mut Context<Self>);
      pub fn seek_to_source_index(&mut self, markdown: &Entity<Markdown>, source_index: usize, cx: &mut Context<Self>);
      pub fn toggle(&mut self, cx: &mut Context<Self>);
      pub fn is_speaking(&self) -> bool;
  }
  ```
  Task 9 depends on all four methods.

This entity does **not** subscribe to `AcpThread` itself. `ThreadView` already has that subscription and calls `enqueue_markdown`, which keeps `read_aloud` free of an `acp_thread` dependency and keeps it testable with nothing but a `Markdown` entity.

- [ ] **Step 1: Write the failing tests**

Add to `crates/read_aloud/src/read_aloud.rs`, replacing the existing `mod tests` block:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::FakeTts;
    use crate::sink::FakeSink;
    use gpui::TestAppContext;
    use markdown::Markdown;

    #[test]
    fn defaults_are_inert_but_autoplay_once_enabled() {
        let settings = ReadAloudSettings::from_settings(&settings::SettingsContent::default());
        assert!(!settings.enabled, "feature must be off on a fresh profile");
        assert!(settings.auto_play, "auto_play describes behavior once enabled");
        assert_eq!(settings.provider, "inworld");
        assert_eq!(settings.voice_id, "Dennis");
        assert_eq!(settings.model_id, "inworld-tts-2");
        assert_eq!(settings.speaking_rate, 1.0);
    }

    #[gpui::test]
    async fn speaks_a_markdown_entity_and_highlights_the_current_sentence(
        cx: &mut TestAppContext,
    ) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let markdown = cx.new(|cx| Markdown::new_text("First one. Second one.\n".into(), cx));
        cx.run_until_parked();

        let read_aloud = cx.new({
            let provider = provider.clone();
            let sink = sink.clone();
            |cx| ReadAloud::for_test(Arc::new(provider), Box::new(sink), cx)
        });

        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.enqueue_markdown(&markdown, cx);
        });
        cx.run_until_parked();

        assert_eq!(provider.spoken(), vec!["First one.", "Second one."]);
        assert_eq!(
            markdown.read_with(cx, |markdown, _| markdown.speaking_highlight().cloned()),
            Some(0..10),
            "the first sentence should be highlighted"
        );
    }

    #[gpui::test]
    async fn clicking_a_sentence_seeks_to_it(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let markdown = cx.new(|cx| Markdown::new_text("First one. Second one.\n".into(), cx));
        cx.run_until_parked();

        let read_aloud = cx.new({
            let provider = provider.clone();
            let sink = sink.clone();
            |cx| ReadAloud::for_test(Arc::new(provider), Box::new(sink), cx)
        });
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.enqueue_markdown(&markdown, cx);
        });
        cx.run_until_parked();

        // Byte 13 falls inside "Second one."
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.seek_to_source_index(&markdown, 13, cx);
        });
        cx.run_until_parked();

        assert_eq!(
            markdown.read_with(cx, |markdown, _| markdown.speaking_highlight().cloned()),
            Some(11..22),
            "clicking the second sentence should move the highlight there"
        );
    }

    #[gpui::test]
    async fn toggle_stops_then_restarts(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let markdown = cx.new(|cx| Markdown::new_text("Only one.\n".into(), cx));
        cx.run_until_parked();

        let read_aloud = cx.new({
            let provider = provider.clone();
            let sink = sink.clone();
            |cx| ReadAloud::for_test(Arc::new(provider), Box::new(sink), cx)
        });
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.enqueue_markdown(&markdown, cx);
        });
        cx.run_until_parked();
        assert!(read_aloud.read_with(cx, |read_aloud, _| read_aloud.is_speaking()));

        read_aloud.update(cx, |read_aloud, cx| read_aloud.toggle(cx));
        cx.run_until_parked();

        assert!(!read_aloud.read_with(cx, |read_aloud, _| read_aloud.is_speaking()));
        assert_eq!(
            markdown.read_with(cx, |markdown, _| markdown.speaking_highlight().cloned()),
            None
        );

        read_aloud.update(cx, |read_aloud, cx| read_aloud.toggle(cx));
        cx.run_until_parked();

        assert!(
            read_aloud.read_with(cx, |read_aloud, _| read_aloud.is_speaking()),
            "toggling back on must restart the message, not latch off"
        );
        assert_eq!(
            markdown.read_with(cx, |markdown, _| markdown.speaking_highlight().cloned()),
            Some(0..9),
            "restart begins at the first sentence"
        );
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `~/.cargo/bin/cargo test -p read_aloud`

Expected: FAIL to compile — `cannot find function for_test`.

- [ ] **Step 3: Implement the entity**

Add to `crates/read_aloud/src/read_aloud.rs`, above the test module:

```rust
mod inworld;
mod player;
mod provider;
mod segmenter;
mod sink;

pub use inworld::{INWORLD_CREDENTIALS_URL, InworldTts, resolve_api_key};
pub use player::{Player, PlayerEvent};
pub use provider::{Pcm, TtsProvider};
pub use segmenter::{Utterance, segment};
pub use sink::{AudioSink, RodioSink};

use gpui::{Context, Entity, Subscription, Task};
use markdown::Markdown;
use std::sync::Arc;
use std::time::Duration;

/// How often the player's queue depth is sampled to advance the highlight.
/// rodio reports depth but emits no completion callback, so this is polled.
const POSITION_POLL_INTERVAL: Duration = Duration::from_millis(100);

pub struct ReadAloud {
    player: Entity<Player>,
    /// The markdown entity currently being spoken, and the utterances derived
    /// from it. Kept together so the highlight can be cleared on switch.
    speaking: Option<Entity<Markdown>>,
    poll_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl ReadAloud {
    #[cfg(any(test, feature = "test-support"))]
    pub fn for_test(
        provider: Arc<dyn TtsProvider>,
        sink: Box<dyn AudioSink>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::build(provider, sink, cx)
    }

    pub fn new(
        provider: Arc<dyn TtsProvider>,
        sink: Box<dyn AudioSink>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::build(provider, sink, cx)
    }

    fn build(
        provider: Arc<dyn TtsProvider>,
        sink: Box<dyn AudioSink>,
        cx: &mut Context<Self>,
    ) -> Self {
        let player = cx.new(|cx| Player::new(provider, sink, cx));
        let subscription = cx.subscribe(&player, |this, _player, event, cx| match event {
            PlayerEvent::Speaking(index) => this.highlight_utterance(Some(*index), cx),
            PlayerEvent::Finished => this.highlight_utterance(None, cx),
        });

        Self {
            player,
            speaking: None,
            poll_task: None,
            _subscriptions: vec![subscription],
        }
    }

    /// Segments a markdown entity and hands the utterances to the player.
    /// Safe to call repeatedly as content streams in — the player only
    /// synthesizes what it has not already queued.
    pub fn enqueue_markdown(&mut self, markdown: &Entity<Markdown>, cx: &mut Context<Self>) {
        if self.speaking.as_ref() != Some(markdown) {
            self.clear_highlight(cx);
            self.speaking = Some(markdown.clone());
        }

        let utterances = segment(markdown.read(cx).parsed_markdown());
        self.player.update(cx, |player, cx| {
            player.set_utterances(utterances, cx);
        });
        self.start_polling(cx);
    }

    pub fn seek_to_source_index(
        &mut self,
        markdown: &Entity<Markdown>,
        source_index: usize,
        cx: &mut Context<Self>,
    ) {
        if self.speaking.as_ref() != Some(markdown) {
            self.enqueue_markdown(markdown, cx);
        }

        let target = self.player.read(cx).utterances().iter().position(|utterance| {
            utterance.source_range.contains(&source_index)
                || utterance.source_range.start > source_index
        });

        if let Some(index) = target {
            self.player.update(cx, |player, cx| player.seek_to(index, cx));
            self.start_polling(cx);
        }
    }

    pub fn toggle(&mut self, cx: &mut Context<Self>) {
        if self.is_speaking() {
            self.player.update(cx, |player, cx| player.stop(cx));
            self.poll_task = None;
            self.clear_highlight(cx);
            // `self.speaking` is deliberately retained so toggling back on has
            // a message to restart.
        } else if self.speaking.is_some() {
            // Restart from the top. `stop` discarded the queue position, and
            // resuming mid-sentence would need an offset into audio the sink
            // no longer holds.
            self.player.update(cx, |player, cx| player.seek_to(0, cx));
            self.start_polling(cx);
        }
    }

    pub fn is_speaking(&self) -> bool {
        self.poll_task.is_some()
    }

    fn start_polling(&mut self, cx: &mut Context<Self>) {
        if self.poll_task.is_some() {
            return;
        }
        self.poll_task = Some(cx.spawn(async move |this, cx| {
            loop {
                let Ok(still_playing) = this.update(cx, |this, cx| {
                    this.player.update(cx, |player, cx| {
                        player.poll_position(cx);
                        player.speaking_index().is_some()
                    })
                }) else {
                    return;
                };
                if !still_playing {
                    this.update(cx, |this, _cx| this.poll_task = None).ok();
                    return;
                }
                cx.background_executor().timer(POSITION_POLL_INTERVAL).await;
            }
        }));
    }

    fn highlight_utterance(&mut self, index: Option<usize>, cx: &mut Context<Self>) {
        let Some(markdown) = self.speaking.clone() else {
            return;
        };
        let range = index.and_then(|index| {
            self.player
                .read(cx)
                .utterances()
                .get(index)
                .map(|utterance| utterance.source_range.clone())
        });
        markdown.update(cx, |markdown, cx| {
            markdown.set_speaking_highlight(range, cx);
        });
    }

    fn clear_highlight(&mut self, cx: &mut Context<Self>) {
        if let Some(markdown) = self.speaking.clone() {
            markdown.update(cx, |markdown, cx| {
                markdown.set_speaking_highlight(None, cx);
            });
        }
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `~/.cargo/bin/cargo test -p read_aloud`

Expected: PASS, all tests across the crate.

If `speaks_a_markdown_entity_and_highlights_the_current_sentence` reports `None` for the highlight, the `Speaking(0)` event fired before `self.speaking` was set — confirm `enqueue_markdown` assigns `self.speaking` *before* calling `set_utterances`.

- [ ] **Step 5: Run clippy**

Run: `./script/clippy -p read_aloud`

Expected: no warnings.

- [ ] **Step 6: Commit**

```bash
git add crates/read_aloud/src/read_aloud.rs
git commit -m "read_aloud: Add the per-thread read-aloud entity"
```

---

### Task 9: Wire it into the agent panel

Two small hooks in `thread_view.rs`, the `Toggle` action, and app init.

`ThreadView::render_markdown` at `crates/agent_ui/src/conversation_view/thread_view.rs:11439` is the assistant-prose builder chain — this is where `.on_source_click()` goes. The assistant-message branch at `:6306` is where new content is enqueued.

**Files:**
- Modify: `crates/agent_ui/src/conversation_view/thread_view.rs`
- Modify: `crates/agent_ui/src/agent_ui.rs` (action definition)
- Modify: `crates/agent_ui/Cargo.toml`
- Modify: `crates/zed/src/main.rs` (init)
- Modify: `crates/zed/Cargo.toml`

**Interfaces:**
- Consumes: everything from Tasks 1–8.
- Produces: `read_aloud::Toggle` action, shipped **unbound**.

- [ ] **Step 1: Add the dependencies**

In `crates/agent_ui/Cargo.toml`, add to `[dependencies]`:

```toml
read_aloud.workspace = true
```

In `crates/zed/Cargo.toml`, add to `[dependencies]`:

```toml
read_aloud.workspace = true
```

- [ ] **Step 2: Define the action**

Define it in the `read_aloud` crate rather than `agent_ui`, so the Rust path and the user-facing action name are both `read_aloud::Toggle`. Add to `crates/read_aloud/src/read_aloud.rs`, next to the other `pub use` lines:

```rust
gpui::actions!(
    read_aloud,
    [
        /// Starts or stops reading the assistant's response aloud.
        Toggle
    ]
);
```

`agent_ui` already depends on `read_aloud` from Step 1, so it refers to this as `read_aloud::Toggle`.

Ship it unbound. Do **not** add it to any file under `assets/keymaps/` — the user's keymap is symlinked to the real Zed's, and binding a key would stomp it. `agent_ui` already declares `ToggleNewThreadMenu`, `ToggleOptionsMenu`, and `ToggleProfileSelector`; a bare `Toggle` in the `read_aloud` namespace does not collide with any of them.

- [ ] **Step 3: Hold the entity on `ThreadView`**

Add a field to `struct ThreadView` in `crates/agent_ui/src/conversation_view/thread_view.rs`:

```rust
    read_aloud: Option<Entity<read_aloud::ReadAloud>>,
```

In `ThreadView::new`'s struct initializer, set it to `None`:

```rust
            read_aloud: None,
```

The entity is built asynchronously in Step 6, once the API key resolves. Until then — and forever, if there is no key or no audio device — the field stays `None` and the panel behaves exactly as stock Zed.

- [ ] **Step 4: Hook click-to-seek**

In `ThreadView::render_markdown` (line 11439), append to the builder chain returned by `render_agent_markdown`:

```rust
    fn render_markdown(
        &self,
        markdown: Entity<Markdown>,
        style: MarkdownStyle,
        cx: &App,
    ) -> MarkdownElement {
        let list_state = self.list_state.clone();
        let read_aloud = self.read_aloud.clone();
        let clicked_markdown = markdown.clone();
        render_agent_markdown(
            markdown,
            style,
            &self.workspace,
            &self.code_span_resolver,
            cx,
        )
        .on_mermaid_zoom(move |_window, _cx| {
            list_state.pause_following_tail();
        })
        .on_source_click(move |source_index, click_count, _window, cx| {
            if click_count > 1 {
                return false;
            }
            let Some(read_aloud) = read_aloud.as_ref() else {
                return false;
            };
            read_aloud.update(cx, |read_aloud, cx| {
                read_aloud.seek_to_source_index(&clicked_markdown, source_index, cx);
            });
            // Returning false leaves text selection working as normal.
            false
        })
    }
```

Returning `false` is deliberate: seeking must not break click-drag text selection in the transcript.

- [ ] **Step 5: Hook streaming enqueue**

In the `AgentThreadEntry::AssistantMessage` branch (line 6306), inside the `AssistantMessageChunk::Message` arm — the one that already calls `self.render_markdown(md.clone(), style.clone(), cx)` — the render path has only `&App`, so enqueue from the thread subscription instead.

Find `ThreadView`'s existing `cx.subscribe(&thread, ...)` handler and add to its match:

```rust
            AcpThreadEvent::NewEntry | AcpThreadEvent::EntryUpdated(_) => {
                this.enqueue_read_aloud(cx);
            }
```

Add the method to `impl ThreadView`:

```rust
    fn enqueue_read_aloud(&mut self, cx: &mut Context<Self>) {
        let Some(read_aloud) = self.read_aloud.clone() else {
            return;
        };
        if !read_aloud::ReadAloudSettings::get_global(cx).auto_play {
            return;
        }

        let Some(markdown) = self.thread.read(cx).entries().iter().rev().find_map(|entry| {
            let AgentThreadEntry::AssistantMessage(message) = entry else {
                return None;
            };
            message.chunks.iter().rev().find_map(|chunk| match chunk {
                // Thought chunks are deliberately never spoken.
                AssistantMessageChunk::Message { block, .. } => block.markdown().cloned(),
                AssistantMessageChunk::Thought { .. } => None,
            })
        }) else {
            return;
        };

        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.enqueue_markdown(&markdown, cx);
        });
    }
```

- [ ] **Step 6: Build the entity once the API key resolves**

Constructing `ReadAloud` only after the key is in hand avoids a provider that exists but cannot synthesize. Add this at the end of `ThreadView::new`, after the struct is constructed and `cx` is a `Context<Self>`:

```rust
        if read_aloud::ReadAloudSettings::get_global(cx).enabled {
            let http_client = workspace.read(cx).client().http_client();
            cx.spawn(async move |this, cx| {
                let api_key = match cx.update(read_aloud::resolve_api_key) {
                    Ok(task) => task.await,
                    Err(error) => Err(error),
                };
                let api_key = match api_key {
                    Ok(api_key) => api_key,
                    Err(error) => {
                        // Failure mode: no API key. Inert, reported once, per view.
                        log::warn!("read_aloud: disabled, no API key available: {error:#}");
                        return;
                    }
                };

                this.update(cx, |this, cx| {
                    // Failure mode: no audio device. Inert, reported once.
                    let Some(player) = audio::Audio::connect_player(cx) else {
                        log::warn!("read_aloud: disabled, no audio output device available");
                        return;
                    };
                    let settings = read_aloud::ReadAloudSettings::get_global(cx);
                    let provider = read_aloud::InworldTts::new(
                        http_client,
                        api_key,
                        settings.voice_id.clone(),
                        settings.model_id.clone(),
                    );
                    let speaking_rate = settings.speaking_rate;
                    let entity = cx.new(|cx| {
                        let mut read_aloud = read_aloud::ReadAloud::new(
                            Arc::new(provider),
                            Box::new(read_aloud::RodioSink::new(player)),
                            cx,
                        );
                        read_aloud.set_speed(speaking_rate);
                        read_aloud
                    });
                    this.read_aloud = Some(entity);
                    cx.notify();
                })
                .log_err();
            })
            .detach();
        }
```

Add the matching passthrough to `ReadAloud` in `crates/read_aloud/src/read_aloud.rs`:

```rust
    pub fn set_speed(&mut self, speed: f32, cx: &mut Context<Self>) {
        self.player
            .update(cx, |player, _cx| player.set_speed(speed));
    }
```

The call inside the `cx.new` closure above is therefore `read_aloud.set_speed(speaking_rate, cx)`.

Note `resolve_api_key` is passed to `cx.update` by name — its signature is `fn(&App) -> Task<Result<String>>`, which matches `AsyncApp::update`'s closure parameter directly.

- [ ] **Step 7: Handle the Toggle action**

On `ThreadView`'s root element render, add:

```rust
            .on_action(cx.listener(|this, _: &read_aloud::Toggle, _window, cx| {
                if let Some(read_aloud) = this.read_aloud.clone() {
                    read_aloud.update(cx, |read_aloud, cx| read_aloud.toggle(cx));
                }
            }))
```

- [ ] **Step 8: Initialize the crate**

In `crates/zed/src/main.rs`, next to the other `::init(cx)` calls (around line 703, near `web_search::init(cx)`):

```rust
        read_aloud::init(cx);
```

- [ ] **Step 9: Build and check**

Run: `~/.cargo/bin/cargo build -p zed`

Expected: SUCCESS.

Run: `./script/clippy -p read_aloud -p agent_ui -p markdown -p audio`

Expected: no warnings.

- [ ] **Step 10: Run the full affected test set**

Run: `~/.cargo/bin/cargo test -p read_aloud -p markdown -p paths -p release_channel`

Expected: PASS.

Run: `~/.cargo/bin/cargo test -p agent_ui`

Expected: PASS. `agent_ui`'s suite is large; if pre-existing failures appear, confirm they also fail on `git stash` before treating them as yours.

- [ ] **Step 11: Manual verification**

```bash
export INWORLD_API_KEY="<your key>"
./script/bundle-mac -d -i
open "/Applications/Zed Echo.app"
```

In Zed Echo, set `"read_aloud": { "enabled": true }` in settings, open the agent panel with `claude-acp`, and send a prompt.

Verify, in order:
1. Assistant prose begins speaking as it streams.
2. The sentence being spoken carries a visible background that advances.
3. Code blocks, tables, and thinking blocks are silent — no spoken markers.
4. Clicking a later sentence jumps playback there and moves the highlight.
5. Click-dragging still selects text normally.
6. Find-in-thread still highlights matches while audio plays.
7. With `INWORLD_API_KEY` unset, the panel behaves exactly as stock Zed — no hangs, no errors in the UI.

- [ ] **Step 12: Commit**

```bash
git add crates/agent_ui crates/zed/src/main.rs crates/zed/Cargo.toml crates/read_aloud
git commit -m "agent_ui: Read assistant responses aloud"
```

---

## Done criteria

- `~/.cargo/bin/cargo test -p read_aloud -p markdown -p audio -p agent_ui` passes.
- `./script/clippy` reports no warnings on the touched crates.
- With `enabled: false` (the default), the agent panel is byte-for-byte unchanged in behavior.
- With `enabled: true` and a key present, all seven manual checks in Task 9 Step 11 hold.
- No `unwrap()` in non-test code; no `let _ =` on a fallible call.

## Failure-mode coverage

Measured against the design's failure table:

| Condition | Design says | This plan does | Where |
|---|---|---|---|
| No API key | Inert; notify once | Inert; **logged** once per view, not a UI notification | Task 9 Step 6 |
| Synthesis error | Log, skip, queue keeps moving | Exactly that | Task 5 `pump`, tested |
| Rate limited | Back off; fall silent rather than block | 2 retries with 500ms doubling backoff, then drop the utterance | Task 7 Step 4 |
| No audio device | Disable; notify once | Disabled; **logged** once per view, not a UI notification | Task 9 Step 6 |

**Deliberate simplification, flagged for your call:** "notify once" is implemented as a `log::warn!`, not a `Workspace::show_notification` toast. A toast requires threading a `Workspace` handle into the failure path and a dedup flag to keep it from firing per thread view. Since `enabled` defaults to `false`, anyone who has turned this on knows they turned it on, and the log line is discoverable. Say the word if you want real toasts and it becomes a small addition to Task 9.

## Known deferrals

Carried forward from the design, deliberately out of scope:

- Word-level karaoke highlighting.
- Reading user messages, tool output, or terminal content.
- Any provider other than Inworld.
- Auto-play bills per character on every response — Inworld charges by character and this synthesizes everything the assistant says.

## Suggested `.rules` additions

Per `CLAUDE.md`, do not edit `.rules` inline. Propose these in the PR description:

- `crates/paths/src/paths.rs`'s `APP_NAME` is bound to the binary name by a `const _: () = assert!` in `crates/zed/src/main.rs`. Changing one without the other is a compile error, not a runtime surprise.
- Assistant-message markdown renders through `ThreadView::render_markdown` in `conversation_view/thread_view.rs`, not through the same-named method in `conversation_view.rs` — that one renders the auth callout.
