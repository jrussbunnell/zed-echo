# Echo — live preview, and a browser later

Status: **idea capture, 2026-08-23. Not approved, not scheduled, no build order.**
Written to preserve the technical findings while they are fresh.

## The idea

The app you are building, rendered inside Echo, with its console and network
wired to the agent working on it.

## Why it is worth doing

You already have `chrome-devtools` and `playwright` MCP servers configured, so an
agent can drive a real Chrome today: navigate, click, screenshot, read the
console, list network requests. That half is solved.

What is missing is that **the agent's Chrome and your eyes are looking at
different browsers**. The agent drives an instance you cannot see; you check the
work in a window Echo knows nothing about. Every loop goes through alt-tab and a
description.

The thesis is one browser with two consumers: the agent drives it and reads its
console, you watch the same pixels, and when something breaks you are both
looking at the same thing. *The agent sees its own bugs* is the payoff, and it
only works if there is one instance.

## What GPUI can and cannot do

Verified against the tree on 2026-08-23.

**There is no web content path in GPUI.** No `WKWebView`, no `wry`, no CEF, no
Servo. Zed draws every pixel itself to one GPU surface. `crates/gpui_web` is
GPUI compiled *to* WASM — the opposite direction, and no help here.

Embedding a real engine therefore means overlaying a native `NSView` (and a
`WebView2`, and a `WebKitGTK`) on top of that surface, which fights z-order,
clipping, scroll containers, and occlusion. It is the classic native-view-in-a-
custom-renderer problem: solvable, per-platform, and genuinely gnarly.

**But a preview does not need an embedded engine.** Three things already in the
tree make a much cheaper route work:

| Already present | Why it matters |
|---|---|
| `async-tungstenite = "0.31.0"` | The Chrome DevTools Protocol is JSON-RPC over a WebSocket. The transport is already a dependency. |
| `image` crate with decode features | Screencast frames arrive as base64 JPEG. |
| `RenderImage` / `ImageSource` in GPUI | Drawing a stream of frames is drawing an image, which GPUI already does well. |

And there is direct precedent for embedding a foreign process and rendering its
output inline: `crates/repl` runs a Jupyter kernel and draws the images it
returns (`crates/repl/src/outputs/image.rs`). A browser process is structurally
the same shape.

## Sketch

Chrome runs offscreen. Echo opens one CDP WebSocket to it and uses roughly six
methods:

- `Page.startScreencast` → `Page.screencastFrame` events carrying JPEG frames,
  acknowledged with `Page.screencastFrameAck`. Decode, draw with `img()`.
- `Input.dispatchMouseEvent` / `Input.dispatchKeyEvent` to forward interaction.
- `Runtime.consoleAPICalled` and `Network.responseReceived` for the console and
  network panes — **on the same connection**, which is what makes one browser
  serve both consumers.

No native view. No platform-specific code. Cross-platform for free.

## The ceiling, and the seam to a real browser

Screencast is compressed JPEG at roughly 10–20fps. That is fine for *did the
layout break, did the button move, what did the console say*. It is not fine for
a surface you live in: no smooth scrolling, text is compression-blurred, input
has a round trip of latency.

So the dromaki-shaped ambition — tabs, address bar, history, a browser you
actually browse in — needs the other approach, a natively composited engine. The
preview work does not get you there, and pretending otherwise would be the
mistake.

What *does* carry over is worth being precise about:

| Layer | Reusable for a real browser? |
|---|---|
| CDP client, session management, lifecycle | **Yes** — the control plane is the same |
| Console and network capture, agent wiring | **Yes** — the reason to build it at all |
| Input event translation (GPUI → CDP) | **Mostly** — retargeted at a native view |
| Screencast frame transport and decode | **No** — thrown away entirely |

Build the preview knowing the frame transport is disposable and the control
plane is not. That is the seam.

## What this is not

- Not a browser. One URL, yours, and no navigation chrome beyond reload.
- Not a replacement for the MCP servers. The agent keeps driving through them;
  this shares the instance rather than adding a second control path.
- Not tabs, history, bookmarks, extensions, or profiles.

## Open questions

- Who owns the Chrome process — Echo, or the MCP server that is already
  launching one? Sharing an instance the MCP owns is cheaper but couples Echo to
  that server's lifecycle.
- Does the agent get the preview's console automatically, or on request? Piping
  every `console.log` into a turn is a context cost with no ceiling.
- Headless with screencast, or headful offscreen? Headless renders differently
  for some CSS, which makes a preview that lies.
- What happens on a dev-server restart, a port change, or a compile error page.

## Related

- [Fleet and workflows](2026-08-22-fleet-and-workflows-design.md) — unrelated
  mechanically, but the same instinct: Echo showing you work that currently only
  exists in another process's terminal.
- `~/code/personal/dromaki` — the browser design this would eventually feed. Docs
  only at time of writing; no code.
