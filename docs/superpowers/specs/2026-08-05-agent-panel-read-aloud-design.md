# Read Aloud for the Zed Agent Panel

**Date:** 2026-08-05
**Status:** Approved design, not yet implemented
**Repo:** `jrussbunnell/zed` (fork of `zed-industries/zed`)
**Ships as:** Zed Echo

## Problem

Assistant responses in Zed's agent panel are long. Reading every plan, summary,
and explanation is the bottleneck. The goal is to listen instead: assistant
prose is spoken aloud as it streams, and clicking any sentence jumps playback
to that point.

## Why a fork

Zed extensions run in a WASM sandbox with no GPU access and no GPUI internals.
There is no extension API for custom UI rendering
([zed#37270](https://github.com/zed-industries/zed/discussions/37270)), and no
way for an extension to observe agent panel state. The feature has to live in
the editor.

The fork is built to upstreamable standards — a provider trait rather than a
hardcoded vendor, settings-gated, no bundled credentials — so a PR to
`zed-industries/zed` stays possible. That decision is deferred until the
feature works; the fork is not blocked on it.

## Side-by-side install: "Zed Echo"

The fork must coexist with the daily-driver `Zed.app` and `Zed Preview.app`
without colliding.

Zed's `ReleaseChannel` already provides per-channel bundle identity, but **it
does not isolate data**. `crates/paths/src/paths.rs:18` is a hardcoded
`const APP_NAME: &str = "Zed"` — not channel-derived — so Dev, Preview, and
Stable all share `~/Library/Application Support/Zed` and `~/.config/zed`,
including the SQLite database holding threads and workspace state. Left
unchanged, the fork would write to the real editor's database.

The fork reuses the **Dev** channel, which is already separated from Stable and
has `poll_for_updates() == false` — it will never auto-update itself out from
under the user.

| File | Change |
|---|---|
| `crates/paths/src/paths.rs:18` | `APP_NAME` → `"ZedEcho"` — yields `~/Library/Application Support/ZedEcho` and `~/.config/zedecho`. Deliberately space-free so paths stay typeable |
| `crates/zed/Cargo.toml` `[package.metadata.bundle-dev]` | `name = "Zed Echo"`, `identifier = "dev.jrb.ZedEcho"`, `osx_url_schemes = ["zedecho"]` |
| `crates/release_channel/src/lib.rs` | Dev arms: `display_name()` → `"Zed Echo"`, `app_id()` → `"dev.jrb.ZedEcho"`, `app_identifier()` → `"Zed-Echo"` |
| `crates/zed/resources/app-icon-dev*.png` | Distinct icon so the Dock is unambiguous |
| Install step | CLI installs as `zedecho`; must not clobber `/usr/local/bin/zed` |

The URL scheme change matters independently: all four upstream channels declare
`osx_url_schemes = ["zed"]`, so an unmodified fork would contend with the real
Zed for `zed://` in LaunchServices.

**Settings are shared, state is not.** `~/.config/zedecho/settings.json` and
`keymap.json` symlink to the real Zed's copies, so keybindings, theme, and
`agent_servers` carry over and stay in sync. The database, workspace state, and
thread history remain separate.

## What already exists

Recon confirmed Zed ships the hard parts:

| Need | Existing mechanism |
|---|---|
| Click a sentence | `MarkdownElement::on_source_click()` — yields the source byte offset |
| Highlight a range | `Markdown::set_search_highlights(&[Range<usize>])` — range highlighting over source offsets |
| Sentence boundaries | `ParsedMarkdown::events()` → `&[(Range<usize>, MarkdownEvent)]` |
| Audio playback | `crates/audio` already depends on `rodio` + `cpal` |
| HTTP | `crates/http_client` |
| Credentials | Zed's keychain credential store, as used by language model providers |

The work is plumbing, not new GPUI primitives.

## Behavior

- **Auto-play.** Every assistant message begins speaking as prose streams in,
  sentence by sentence, without user action.
- **Click to seek.** Clicking any sentence in an assistant message moves
  playback to that sentence and continues from there.
- **Sentence-level highlight.** The sentence currently being spoken gets a
  subtle background that advances with playback. No word-level karaoke.
- **Prose only.** Code blocks, tables, and thinking blocks are skipped silently
  — no spoken markers. Tool calls and diffs are separate `AgentThreadEntry`
  variants, not markdown, so they are never fed to the segmenter at all.

## Architecture

New crate `crates/read_aloud/`. `conversation_view.rs` is already 11,184 lines;
the feature does not go there.

### `segmenter.rs`

Pure function: `ParsedMarkdown` → `Vec<Utterance>`.

```rust
struct Utterance {
    source_range: Range<usize>,  // bytes in the original markdown
    spoken_text: String,          // inline markup stripped
}
```

Walks `events()`. Keeps text inside Paragraph, Heading, and ListItem. Drops
CodeBlock, Table, and any range belonging to an
`AssistantMessageChunk::Thought`. `spoken_text` strips backticks, link syntax,
and emphasis markers; `source_range` still points at the original bytes, which
is what drives both click-mapping and highlighting.

No I/O, no GPUI. This unit carries the test suite.

### `provider.rs`

```rust
trait TtsProvider {
    fn synthesize(&self, text: String, cx: &App) -> Task<Result<Pcm>>;
}
```

One seam so no vendor is welded into the editor.

### `inworld.rs`

`POST https://api.inworld.ai/tts/v1/voice:stream`

- `Authorization: Basic $INWORLD_API_KEY`
- Body: `{ text, voiceId, modelId, audioConfig: { audioEncoding: "LINEAR16",
  sampleRateHertz: 22050 }, deliveryMode: "BALANCED" }`
- Response: streamed JSON, base64 audio in `result.audioContent`

`LINEAR16` is raw PCM, so rodio plays it with no decoder in the path.

### `player.rs`

Owns a rodio `Sink` obtained through `crates/audio`. Holds a FIFO of
utterances, synthesizes N+1 while N plays, and emits `Speaking(idx)` /
`Finished`.

### `read_aloud.rs`

One GPUI entity per conversation view. Subscribes to `AcpThreadEvent`, owns the
queue and current index, and drives the highlight.

## Integration points

Four edits to existing code, separate from the rebrand edits above. The rebrand
is **Phase 0** — it lands first and independently, so a buildable, installable
`Zed Echo` exists before any feature work begins.

1. **`crates/agent_ui/src/conversation_view.rs`** — `render_agent_markdown`
   (line 3452 at the pinned commit) is a builder chain. Append
   `.on_source_click(…)`; map the offset to an utterance index and call
   `player.seek_to(idx)`. ~10 lines.

2. **`crates/agent_ui/src/conversation_view.rs`** — on assistant chunk update,
   feed the segmenter and enqueue newly-complete sentences. ~40 lines.

3. **`crates/markdown/src/markdown.rs`** — add
   `set_speaking_highlight(Option<Range<usize>>)`, mirroring the existing
   search-highlight path. A **parallel** channel, not a reuse, so find-in-thread
   keeps working while audio plays. ~30 lines.

4. **`crates/settings`** — new block:

   ```jsonc
   "read_aloud": {
     "enabled": false,
     "auto_play": true,
     "provider": "inworld",
     "voice_id": "Dennis",
     "model_id": "inworld-tts-2",
     "speaking_rate": 1.0
   }
   ```

   `enabled` defaults to `false` — the feature is inert on a fresh profile,
   which is what makes it upstreamable. Turning it on is a one-time edit to
   `~/.config/zedecho/settings.json`. `auto_play` describes what happens *once
   enabled*, so the two defaults are not in conflict.

   API key resolution: `INWORLD_API_KEY` environment variable first, then Zed's
   keychain credential store. Never in settings.json.

A `read_aloud::Toggle` action stops and starts playback. It ships **unbound** —
binding it would stomp a key in the user's shared keymap, and the keymap is
symlinked to the real Zed's.

## Streaming boundaries

The main bug surface. A sentence is enqueued only when both hold:

1. A terminator (`.` `?` `!`) is followed by whitespace, and
2. The markdown parser has closed the containing block.

Condition 2 is what prevents a half-typed code fence from being spoken as
prose. Abbreviation guard suppresses splits on `e.g.`, `i.e.`, `Dr.`, ordered
list markers (`1.`), decimals, and file extensions (`.ts`, `.rs`).

## Failure modes

| Condition | Behavior |
|---|---|
| No API key | Feature inert; notify once |
| Synthesis error | Log, skip that utterance, **queue keeps moving** |
| Rate limited | Back off; fall silent rather than blocking the panel |
| No audio device | Disable; notify once |

The queue never stalls the UI on a network fault.

## Testing

- **Segmenter** (primary): code fence skipping, nested bullets, abbreviation
  guard, partial streaming input, thought-chunk exclusion.
- **Player**: a `FakeTts` provider for queue-ordering and seek behavior.
- **Integration**: one GPUI test for click → seek.

## Accepted tradeoffs

- **Auto-play bills per character on every response.** Inworld charges by
  character; this synthesizes everything the assistant says.
- **Thinking blocks stay silent.** `AssistantMessageChunk::Thought` is excluded.
- **Sentence-level highlight only.** Word-level karaoke would require mapping
  TTS word timestamps back to markdown source offsets, which diverge because
  synthesized text strips markup and expands abbreviations. Deferred.
- **Fork maintenance.** Zed ships weekly. The four integration points are
  deliberately small to keep rebases cheap.

## Out of scope

- Word-level karaoke highlighting
- Voice cloning or voice design
- Reading user messages, tool output, or terminal content
- Any provider other than Inworld in the first implementation
