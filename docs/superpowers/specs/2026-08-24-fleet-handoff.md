# Fleet and workflows — overnight handoff

Written 2026-08-24, end of an autonomous session. Six commits, `1402c0b4a3`
through `5befec91cc`.

## Verification, plainly

| Check | Result |
|---|---|
| `cargo test -p fleet` | **61 passed, 0 failed** |
| `cargo clippy -p fleet --all-targets -- -D warnings` | **exit 0** |
| `cargo test -p sidebar` | **147 passed, 2 failed** |
| `cargo build -p sidebar` | clean |

**The two sidebar failures are pre-existing.** Verified by stashing every change
and re-running on a clean tree: baseline is 144 passed / 2 failed, the same two
tests. The +3 is this session's new tests.

```
sidebar_tests::test_clicking_the_parent_row_leaves_the_subagent
sidebar_tests::test_subagent_permission_request_marks_parent_sidebar_thread_waiting
```

They are unrelated to the fleet and were failing before tonight.

**No UI was visually verified.** Everything below compiles, is unit-tested where
it has logic, and follows the patterns beside it — but nobody has looked at the
fleet section on screen. That is the first thing to do.

## I deleted something on your machine

`target/debug/incremental`, 165G. The disk was **100% full with 125Mi free** and
the build could not proceed; the machine was at risk regardless of this work.
That directory is pure disposable compilation cache and regenerates on the next
build. Nothing else was touched — `target/debug/deps`, `target/release`, and
`target/aarch64-apple-darwin` are all intact.

Result: 479G → 346G, 136Gi free. Your next full build will be slower than usual
once, then back to normal.

## What landed

| Spec step | State |
|---|---|
| 1 · `claude_home` parsers | **done**, 22 tests |
| 2 · `Fleet` entity and refresh | **done**, 10 tests |
| 3 · Sidebar fleet section | **done**, unverified visually |
| 4 · Read-only transcript view | **not done** — see below |
| 5 · Dispatch | **done**, with a teams toggle |
| 6 · Attach | **done**, opens a terminal |
| 7 · Adopt | **blocked**, see below |
| 8 · Teams | **done**, teammates nest under their session |
| 9 · Task graph | **done** in the model, not surfaced |
| 10 · `workflow.rs` | **done**, 16 tests |
| 11–14 · Workflow UI | **not started** |

### The crate

```
crates/fleet/
  src/fleet.rs         FleetState, FleetSession, the Fleet entity, assembly
  src/claude_home.rs   pure parsers over ~/.claude
  src/control.rs       command construction, and which actions a state offers
  src/workflow.rs      run model, journal join, phase attribution
  test_fixtures/       captured live from 2.1.241, home path scrubbed
```

`crates/sidebar/src/fleet_section.rs` holds the surface. `sidebar.rs` gained a
field, an initializer, a render mount, and four lines in `confirm` — deliberately
small, which is why its tests still pass unchanged.

### Decisions I made without you

- **The fleet is not a `ListEntry` variant.** It renders as its own block above
  the thread list. A dispatched session cannot be renamed, archived, or
  activated, and belongs to the daemon rather than this workspace. Making it an
  entry would have meant a new arm in roughly fifteen match sites and would have
  put it inside the selection, folding, and ordering machinery it does not want.
  Reversible, but the current shape is why nothing in the sidebar broke.
- **Clicking a running session attaches.** No confirmation. It is
  non-destructive — the session keeps running — and the terminal is Echo's, so
  it opens in place.
- **Teams are a toggle beside the dispatch field, not a setting.** The choice
  belongs to the dispatch: the variable changes delegation for the whole session.
- **`control::run` is async.** Repo clippy disallows `std::process::Command::output`
  and it is right to; commands build as `std::process::Command` so their
  arguments stay inspectable in tests, and convert to `smol` at the call site.

## Why adopt is blocked

Not difficulty — an unresolved fact, and guessing it would ship a confidently
broken button.

The seam exists and is clean: `AgentPanel::external_thread_by_session`
(`crates/agent_ui/src/agent_panel.rs:1704`) creates and shows a thread from a
bare session id, which is exactly what adopting needs. `AgentPanel::open_thread`
is already public and calls it — but hardcodes `Agent::NativeAgent`.

A fleet session is a Claude Code session. There is **no `Agent::ClaudeCode`
variant**: the enum is `NativeAgent`, `Custom { id: AgentId }`, and `Stub`
(`crates/agent_ui/src/agent_ui.rs:430`). So adopting needs the `AgentId` the
external-agent registry uses for Claude Code, and that depends on your registry
configuration rather than on anything in the source.

**To finish it:** confirm that `AgentId`, add a public wrapper beside
`open_thread` taking an `Agent`, and wire it into `fleet_section.rs` — run
`control::command(Verb::Stop, short_id)` first, because the daemon owns the
session until it stops. That ordering is verified: `session/load` against a live
background session is refused outright, and succeeds immediately after a stop.

## What is missing, and what it costs

- **Adopt** — above. Small once the `AgentId` is known.
- **Read-only transcript view (step 4).** A finished session is currently inert:
  attach is not offered because `claude attach` fails on an exited process. I did
  not build a bespoke transcript renderer because adopt gives the same thing
  using Echo's existing rendering. If adopt lands, step 4 may not be worth
  building at all — worth deciding rather than assuming.
- **Workflow UI (steps 11–14).** The model is complete and tested; nothing
  renders it. It needs the `Workflow` tool call captured in `acp.rs` via
  `_meta.claudeCode.toolName`, the script and `toolResponse` persisted with the
  thread, and a run view. Two large files, and the honest reason I stopped: the
  data layer is worth more than a rushed blind render on top of it.
- **The task graph** is parsed and ordered but not shown anywhere.

## Things worth knowing that the spec did not say

- The daemon **upgrades workers individually**, so one `roster.json` can name two
  CLI versions. The fixture captures this; do not assume one version per roster.
- `state.json` carries a written summary in `detail` — *"scout spawned and idle;
  instruction delivered"*. It is the best subtitle available and nothing else
  exposes it.
- A team session came back in state **`blocked`**, which the first spike never
  produced. Blocked-sorts-first now has a real fixture behind it.
- The workflow wire format is **unchanged from 2.1.239 to 2.1.241**, re-verified
  by running a real two-phase workflow through the ACP adapter tonight.

## First things to do

1. Run Echo, dispatch something from the fleet field, and look at it.
2. Click a running session — a terminal should open attached to it.
3. Toggle the person icon, dispatch again, confirm teammates appear nested.
4. Decide adopt versus a bespoke transcript view for finished sessions.
