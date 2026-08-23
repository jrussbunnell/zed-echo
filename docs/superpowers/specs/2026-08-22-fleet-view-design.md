# Echo — fleet view

Status: draft 2026-08-22. Source of truth for the `fleet` crate.

## Goal

Watch and drive many real, parallel Claude Code sessions from Echo's window
instead of a terminal. Each one is a full session with its own process, its own
git worktree, and its own transcript — not a subagent inside one conversation.

The target situation is a dozen jobs in flight and one question: *which of these
needs me?*

## What this is not

Echo is not becoming an orchestrator. Claude Code ships four ways to parallelize
work — subagents, agent view, agent teams, dynamic workflows — and building a
fifth would be re-implementing a scheduler, a process supervisor, and worktree
isolation that already exist and already work.

What Claude Code does not ship is a GUI. `claude agents` is a terminal UI: rows
hide after 30 seconds, four-plus idle agents collapse into `2 idle agents`, and
20 sessions are navigated with arrow keys. That is a terminal running out of
room, and it is the entire opportunity.

Echo has a window, a thread list that already nests agents under their parent,
and transcript rendering for exactly this file format.

## What already exists

Nothing below is invention. Everything on-disk was verified by spike against
Claude Code 2.1.239 on 2026-08-22; everything in-tree is current.

| Piece | Where |
|---|---|
| Supervisor daemon, process hosting, restart-on-sleep, version upgrade | Claude Code, `~/.claude/daemon/` |
| Automatic git worktree isolation per background session | Claude Code, `.claude/worktrees/` |
| Background dispatch / stop / respawn / logs | `claude --bg`, `claude stop|respawn|logs|rm <id>` |
| CLI transcript tailing with a byte-offset window and uuid glob | `crates/agent_ui/src/subagent_notifications.rs` |
| Nested agent rows under a parent thread | `ListEntry::Subagent`, `crates/sidebar/src/sidebar.rs:429` |
| Per-agent status → icon, color, label | `SubagentStatus`, `crates/agent_ui/src/subagents.rs` |
| Loading an arbitrary session id into a thread | `load_session`, `crates/agent_servers/src/acp.rs:1831` |
| Session id identity: Echo's ACP session id **is** the CLI session id | verified, `subagent_notifications.rs` module docs |

## Spike findings that shape the design

Three questions were open before this spec. All three were answered by probe,
and two of the answers changed the plan.

**A background session cannot be attached over ACP while it is running.**
`session/load` against a live background session is refused outright:

> Session 91214c57… is currently running as a background agent (bg). Use
> `claude agents` to find and attach to it, or add `--fork-session` to branch off
> a copy.

After `claude stop <id>`, the same `session/load` succeeds and replays the whole
transcript as ordinary `user_message_chunk` / `agent_message_chunk` updates. The
daemon owns a session while it runs; Echo can adopt it once it does not.

**Agent teams work in a background session.** They do not work over ACP: the
adapter is built on `@anthropic-ai/claude-agent-sdk`, and Claude Code does not
spawn teammates for Agent SDK sessions — a named subagent silently degrades to
an ordinary subagent. Dispatching with `CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS=1`
through the daemon spawned a real teammate, with full metadata in the team
config and a transcript in the directory Echo already reads. Routing through the
daemon is what unlocks teams; nothing else does.

**The workflow journal is not a progress feed.** `journal.jsonl` holds only
`started` / `result` records keyed by an opaque hash — a resume cache. No phase
names, no labels, no token counts, no timestamps. Rendering a workflow's phase
tree means joining three files with no label→agent mapping between them. That is
the most work of the three surfaces and the least payoff, so workflows are out
of v1.

## Decisions locked

- **Daemon-backed, not ACP-backed.** A *fleet session* is what Claude Code calls
  a `background` session: a complete Claude session hosted by the supervisor,
  with its own process, worktree, context, and transcript. "Background" names
  only the absence of an attached terminal — it is not a subagent and not a
  reduced session. This is the only path that supports agent teams, and it means
  Echo writes no scheduler, no process manager, and no worktree logic.
- **Two planes.** Live sessions are read from disk, read-only. A session becomes
  a real, interactive Echo thread only by being *adopted* — stopped, then loaded
  over ACP. There is no third state.
- **Dispatched sessions only in v1.** `claude agents --json` reports two kinds:
  `background` (daemon-hosted — the fleet) and `interactive` (a `claude` the user
  started in a terminal). Only the first is included. The second is the user's
  own terminal windows, which Echo cannot drive and which would be noise in the
  list; excluding them also keeps the refresh loop to pure disk reads with no
  subprocess. This bounds *what appears in the list*, not how much runs in
  parallel: the fleet is N sessions, each of which may run its own team of
  teammates and its own subagents.
- **A new `fleet` crate for the mechanism, sidebar for the surface.** Same shape
  as `listen`: the crate is pure and testable, the UI wiring lives where the UI
  already is. `sidebar.rs` is 8,591 lines and gets rows, not readers.
- **Workflows deferred.** They already run through Echo today, unsurfaced. A
  second spec, after the journal question is solved.

## Architecture

New crate `crates/fleet/`, `[lib] path = "src/fleet.rs"`.

| File | Holds |
|---|---|
| `src/fleet.rs` | `Fleet` entity, refresh loop, dispatch/stop/respawn/adopt, public types |
| `src/claude_home.rs` | Pure parsers over `~/.claude`. Path in, value out. No process spawning, no GPUI. |

Splitting the parsers out is what makes this testable: every format below is a
private Claude Code implementation detail, so the parsers are the part most
likely to break on a CLI upgrade and the part that must be exercised by fixtures.

### Data planes

| Plane | Source | Documented? |
|---|---|---|
| Roster | `~/.claude/daemon/roster.json` — `workers{}` keyed by short id, each with `pid`, `sessionId`, `cwd`, `cliVersion`, `dispatch.launch.args` | No |
| Detail | `~/.claude/jobs/<id>/state.json` — `state`, `detail`, `tokens`, `output.result`, `intent`, `children`, `resumeSessionId`, `linkScanPath` | No |
| Activity | `~/.claude/jobs/<id>/timeline.jsonl` — one record per state transition: `{at, state, detail, text}` | No |
| Transcript | `state.json.linkScanPath` → `~/.claude/projects/<mangled-cwd>/<session>.jsonl` | No |
| Team | `~/.claude/teams/session-<first 8 of session id>/config.json` — `leadSessionId`, `members[]` | Yes |
| Teammate transcript | `<project>/<session>/subagents/agent-<agentId>.jsonl` + `.meta.json` | No |
| Tasks | `~/.claude/tasks/<full session uuid>/<n>.json` — `subject`, `status`, `blocks`, `blockedBy` | Partly — docs say the directory is keyed by team name; it is keyed by session uuid |

`state.json` is the richest of these and the least supported. It carries a
written summary of what the session did (`detail: "scout spawned and idle;
instruction delivered"`) that nothing else exposes.

### Refresh

Poll `~/.claude/jobs/*/state.json` and `daemon/roster.json` every 2 seconds while
the fleet section is expanded; stop when it is collapsed or the panel is hidden.

```
// ponytail: 2s poll of a directory of small JSON files. Move to fs::watch if
// a large fleet shows up in a profile — the parsers don't change either way.
```

Not a filesystem watch, because `state.json` is rewritten on every transition and
the debounce logic would cost more than the read. Not a subprocess, because
excluding interactive sessions means nothing needs `claude agents --json` on the
hot path.

### Types

```rust
pub struct FleetSession {
    pub short_id: String,          // "91214c57" — daemon id, also the jobs dir
    pub session_id: acp::SessionId,// full uuid; the ACP session id on adopt
    pub name: SharedString,        // --name, or CLI-generated
    pub cwd: PathBuf,
    pub state: FleetState,
    pub detail: Option<SharedString>, // the CLI's own written summary
    pub intent: SharedString,         // the dispatch prompt
    pub tokens: u64,
    pub transcript: Option<PathBuf>,
    pub team: Option<Team>,
    pub tasks: Vec<Task>,
}

pub enum FleetState { Working, Blocked, Done, Failed, Stopped, Unknown(String) }
```

`Unknown(String)` is load-bearing. These formats are private and will gain
states; an unrecognized value renders as itself and never panics.

## Surface

A **Fleet** section in the sidebar, above the project's threads, present only
when `~/.claude/jobs/` is non-empty or a dispatch has been made.

- One row per session: name, state icon, `detail` as subtitle, cwd when it
  differs from the active project, token count.
- Sessions grouped by cwd, matching the sidebar's existing `ProjectHeader`.
- Teammates nest under their session, reusing `ListEntry::Subagent`'s row
  rendering — the `.meta.json` beside each teammate transcript already carries
  `name`, `agentType`, `model`, and `color`.
- Selecting a row opens a **read-only** transcript view fed by the on-disk
  `.jsonl`, tailed the same way `subagent_notifications.rs` tails a session.
- Blocked sessions sort to the top. That is the whole point of the surface.

### Dispatch

A prompt field at the section header. Sends `claude --bg --name <n> <prompt>` in
the active project's root. Optionally with `CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS=1`
when the fleet-teams setting is on.

Teams stay behind a setting, default off: the variable changes delegation for
the whole session — any subagent Claude names becomes a teammate — and teams are
experimental with no session resumption and a fixed lead.

### Adopt

Turning a fleet session into a real Echo thread requires stopping it first, so
it is explicit and never automatic:

| Session state | Offered |
|---|---|
| `done`, `stopped`, `failed` | **Open in Echo** — `claude stop` (no-op if exited), then ACP `session/load` |
| `working`, `blocked` | Read-only view only. Adopting is offered as **Stop and open**, with the stop named in the button. |

`--fork-session` would allow adopting a live session by branching a copy, at the
cost of two divergent transcripts for one piece of work. Not in v1.

## Failure modes

| Situation | Behavior |
|---|---|
| `claude` not on PATH | Section hidden. Dispatch surfaces the reason rather than failing silently. |
| Daemon not running | Empty fleet, not an error. The first dispatch starts it. |
| `state.json` schema changed by a CLI upgrade | Per-field tolerance: an unparsable field is `None`, an unknown state is `Unknown`. A session missing `state.json` still renders from `roster.json`. |
| `jobs/<id>` left after `claude rm` | Rows come from `roster.json` ∪ `jobs/`; an entry in neither the roster nor with a terminal state older than the retention sweep is dropped. |
| Session stopped outside Echo | Next poll reconciles. No cached authority. |
| Adopt races the daemon | `session/load` is the source of truth: if it returns the bg-guard error, the session did not stop; report it and stay read-only. |
| Transcript is megabytes | Same bounded-window tail as `subagent_notifications.rs`. |

The single largest risk is that every high-value field lives in an undocumented
format. The mitigation is that `roster.json` and `claude agents --json` are
enough on their own for a degraded but working fleet view, so a schema break
costs detail, not the feature.

## Testing

Fixtures captured from the spike — a real `state.json`, `roster.json`,
`timeline.jsonl`, team `config.json`, and teammate `.meta.json` — go in
`crates/fleet/test_fixtures/`. Every parser is a pure function over a path, so
the suite runs with no daemon, no network, and no Claude Code installed.

Cases that must be covered:

- A `state.json` with an unrecognized `state` renders `Unknown`, does not panic.
- A session present in `roster.json` but with no `jobs/` dir still renders.
- A team `config.json` with only a `team-lead` produces no teammate rows.
- A task set with `blockedBy` produces the right blocked ordering.
- Tail resumption from a byte offset does not re-emit already-seen records.

One end-to-end test that actually dispatches, behind `RUN_FLEET_E2E=1`, and
excluded from CI. It costs tokens and needs a real daemon.

## Build order

1. `claude_home.rs` parsers, against fixtures. No UI.
2. `Fleet` entity and the refresh loop. Assert against fixtures on a temp dir.
3. Sidebar section, read-only. Rows, grouping, blocked-first ordering.
4. Read-only transcript view, reusing the existing tail.
5. Dispatch, stop, respawn.
6. Adopt.
7. Teams: the env var at dispatch, `config.json` members as nested rows,
   teammate transcripts.
8. Task graph under a session.

Steps 1–4 are a complete, useful, read-only fleet view and can ship alone.

## Not in v1

- **Dynamic workflows.** Already run through Echo unsurfaced. Separate spec once
  the phase/agent join is solved.
- **Interactive session listing.** `claude agents --json` sees the user's own
  terminals; that is a different product than a managed fleet.
- **Voice.** `listen` should eventually answer *"what's blocked?"* across the
  fleet — the natural payoff of Echo having both. `FleetSession` exposing state
  is the whole hook needed; the routing is a later change.
- **Cross-session messaging** between fleet sessions.
- **Worktree management UI.** The daemon creates and cleans them.
- **`--fork-session`** adoption of a live session.
