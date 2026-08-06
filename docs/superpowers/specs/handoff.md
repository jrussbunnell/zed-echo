# Zed Echo — read-aloud for the agent panel

## Goal
Fork of Zed that speaks assistant prose in the agent panel as it streams.
Click any sentence to seek there; the speaking sentence is highlighted.
TTS via Inworld. The user drives Claude Code through Zed's ACP agent panel
(`claude-acp`), so assistant messages render through Zed's native markdown renderer.

## Read this first
`docs/superpowers/specs/2026-08-05-agent-panel-read-aloud-design.md` — the approved
design. It is the source of truth. Do not redesign; it was brainstormed and ratified.

## Repo state
- `~/code/personal/zed`, branch `read-aloud`
- HEAD `bd19cb6276` (spec commit) on top of upstream `82878540b5`
- Remotes: `origin` = jrussbunnell/zed, `upstream` = zed-industries/zed
- Partial clone (`--filter=blob:none`) — full history, blobs on demand
- **Nothing implemented yet.** Spec only.

## Environment (verified working, do not re-litigate)
- Rust `1.95.0` via rustup, pinned by `rust-toolchain.toml`. Use `~/.cargo/bin/cargo`.
- Targets installed: aarch64-apple-darwin, wasm32-unknown-unknown, wasm32-wasip2,
  x86_64-unknown-linux-musl
- `cmake` 4.4.2 via brew (required by wasmtime-c-api-impl)
- `xcode-select -p` → `/Applications/Xcode-beta.app/Contents/Developer` (NOT Xcode.app —
  it doesn't exist on this machine). Metal compiler resolves; needed by `gpui_macos`.
- `cargo build -p zed` succeeds. Cold build done; incremental rebuilds ~1 min.

## Verified code anchors (line numbers correct at this commit)
| What | Where |
|---|---|
| `pub const APP_NAME: &str = "Zed"` | `crates/paths/src/paths.rs:18` |
| `APP_NAME_LOWERCASE` | `crates/paths/src/paths.rs:22` |
| `app_identifier()` | `crates/release_channel/src/lib.rs:45` |
| `poll_for_updates()` | `crates/release_channel/src/lib.rs:201` |
| `display_name()` | `crates/release_channel/src/lib.rs:206` |
| `app_id()` | `crates/release_channel/src/lib.rs:228` |
| `[package.metadata.bundle-dev]` | `crates/zed/Cargo.toml:283` |
| `fn render_agent_markdown` | `crates/agent_ui/src/conversation_view.rs:3452` |
| `Markdown::parsed_markdown()` | `crates/markdown/src/markdown.rs:1031` |
| `Markdown::set_search_highlights()` | `crates/markdown/src/markdown.rs:1064` |
| `Markdown::set_active_search_highlight()` | `crates/markdown/src/markdown.rs:1089` |
| `ParsedMarkdown::events()` | `crates/markdown/src/markdown.rs:1460` |
| `MarkdownElement::on_source_click()` | `crates/markdown/src/markdown.rs:1606` |

`conversation_view.rs` is 11,184 lines — put new logic in a new `crates/read_aloud/`,
not in that file.

## Decisions already locked
- Auto-play as the message streams (not click-to-start)
- Sentence-level highlight, NOT word-level karaoke
- Prose only: skip code blocks, tables, thinking blocks. Tool calls/diffs are
  separate `AgentThreadEntry` variants and never reach the segmenter.
- `TtsProvider` trait with Inworld as first impl — keeps upstreaming possible
- `read_aloud::Toggle` ships unbound (keymap is symlinked to the real Zed's)
- Settings `enabled` defaults false, `auto_play` defaults true

## Hazards
- **Do not run the built binary before Phase 0 lands.** `APP_NAME` is still `"Zed"`,
  so it writes to `~/Library/Application Support/Zed` and `~/.config/zed` — the user's
  real editor's SQLite thread database.
- All four upstream channels declare `osx_url_schemes = ["zed"]`; the fork must change
  this or it fights the real Zed in LaunchServices.
- `~/.rustup` is fragile to concurrent rustup invocations. Run them strictly serially.

## Next action
Write the implementation plan. Phase 0 = the "Zed Echo" rebrand (APP_NAME → `ZedEcho`,
bundle id `dev.jrb.ZedEcho`, scheme `zedecho://`, display name "Zed Echo", distinct icon),
landing a buildable installable app. Phase 1 = the `read_aloud` crate + four integration
edits. Details in the spec.
