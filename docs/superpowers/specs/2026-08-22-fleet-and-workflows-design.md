# Echo — fleet and workflows

Status: draft 2026-08-22, workflows folded in 2026-08-23. Source of truth for
the `fleet` crate.

## Goal

Two surfaces for work that runs many agents at once:

- **Fleet** — many real, parallel Claude Code sessions, each with its own
  process, git worktree, and transcript. Answers *which of these needs me?*
- **Workflows** — one scripted run fanning out dozens of agents across phases.
  Answers *what is this thing actually doing, and what did each agent find?*

They share a crate because they are the same job: reading Claude Code's parallel
state off the wire and off disk, and rendering it in a window instead of a
terminal.

## What this is not

Echo is not becoming an orchestrator. Claude Code ships four ways to parallelize
work — subagents, agent view, agent teams, dynamic workflows — and building a
fifth would re-implement a scheduler, a process supervisor, and worktree
isolation that already exist and already work.

What Claude Code does not ship is a GUI. `claude agents` and `/workflows` are
terminal UIs: rows hide after 30 seconds, four-plus idle agents collapse into
`2 idle agents`, and a 40-agent run is navigated with `↑`/`↓`/`f`/`j`/`k`. That
is a terminal running out of room, and it is the entire opportunity.

Echo has a window, a thread list that already nests agents under their parent,
and transcript rendering for exactly these file formats.

## What already exists

Nothing below is invention. On-disk and on-wire behavior was verified by spike
against Claude Code 2.1.239 on 2026-08-22/23; everything in-tree is current.

| Piece | Where |
|---|---|
| Supervisor daemon: process hosting, restart-on-sleep, version upgrade | Claude Code, `~/.claude/daemon/` |
| Automatic git worktree isolation per dispatched session | Claude Code, `.claude/worktrees/` |
| Dispatch / stop / respawn / logs | `claude --bg`, `claude stop\|respawn\|logs\|rm <id>` |
| Workflow runtime, 16-way concurrency, resume cache | Claude Code, `Workflow` tool |
| CLI transcript tailing with a byte-offset window and uuid glob | `crates/agent_ui/src/subagent_notifications.rs` |
| Nested agent rows under a parent thread | `ListEntry::Subagent`, `crates/sidebar/src/sidebar.rs:429` |
| Per-agent status → icon, color, label | `SubagentStatus`, `crates/agent_ui/src/subagents.rs` |
| Loading an arbitrary session id into a thread | `load_session`, `crates/agent_servers/src/acp.rs:1831` |
| Tool-call `_meta` plumbed through to the client | `crates/agent_servers/src/acp.rs` |
| Session id identity: Echo's ACP session id **is** the CLI session id | verified, `subagent_notifications.rs` module docs |

## Spike findings that shape the design

Four questions were open. All four were answered by probe, and three changed the
plan.

**A dispatched session cannot be attached over ACP while it runs.**
`session/load` against a live one is refused outright:

> Session 91214c57… is currently running as a background agent (bg). Use
> `claude agents` to find and attach to it, or add `--fork-session` to branch off
> a copy.

After `claude stop <id>`, the same `session/load` succeeds and replays the whole
transcript as ordinary `user_message_chunk` / `agent_message_chunk` updates. The
daemon owns a session while it runs; Echo can adopt it once it does not.

**Agent teams work in a dispatched session, and only there.** They do not work
over ACP: the adapter is built on `@anthropic-ai/claude-agent-sdk`, and Claude
Code does not spawn teammates for Agent SDK sessions — a named subagent silently
degrades to an ordinary subagent. Dispatching with
`CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS=1` through the daemon spawned a real
teammate, with full metadata in the team config and a transcript in the directory
Echo already reads.

**A workflow announces itself on the wire, with everything needed to follow it.**
The launch arrives as a `tool_call` carrying `_meta.claudeCode.toolName:
"Workflow"`, then a `tool_call_update` whose `rawInput.script` is **the entire
script source** — `meta.phases` and every `agent(…, { label, phase })` call site
included. The tool response carries the rest:

```json
{ "status": "async_launched", "taskId": "w9tn1d11t", "taskType": "local_workflow",
  "workflowName": "echo-alpha-beta", "runId": "wf_11e12c55-af7",
  "summary": "Two-phase echo workflow: Alpha then Beta",
  "transcriptDir": "…/subagents/workflows/wf_11e12c55-af7",
  "scriptPath": "…/workflows/scripts/echo-alpha-beta-wf_11e12c55-af7.js" }
```

No globbing, no guessing: Echo is handed the run id, the run directory, the
script path, and the declared phases at launch.

**The workflow journal is a resume cache, not a progress feed.** `journal.jsonl`
holds only `started` / `result` records keyed by an opaque `v2:<sha256>` and an
`agentId`. `agent-<id>.meta.json` is `{"agentType":"workflow-subagent",
"spawnDepth":1}` — no label, no phase. Verified identical on 2.1.239 and on an
older run. Phase titles, labels, token totals, and elapsed time live in the CLI
process's memory, which is why workflow resume only works within one session.

The consequence is a specific, bounded gap: everything *per agent* is on disk,
and the phase each agent belongs to is not. See [Phase attribution](#phase-attribution).

## Decisions locked

- **Daemon-backed, not ACP-backed, for the fleet.** A *fleet session* is what
  Claude Code calls a `background` session: a complete Claude session hosted by
  the supervisor, with its own process, worktree, context, and transcript.
  "Background" names only the absence of an attached terminal — it is not a
  subagent and not a reduced session. This is the only path that supports agent
  teams, and it means Echo writes no scheduler, no process manager, and no
  worktree logic.
- **Three levels of access to a fleet session, not two.** *Read* tails the
  transcript on disk. *Attach* runs `claude attach` in a terminal Echo owns —
  interactive, and the session keeps running. *Adopt* stops the session and loads
  it over ACP, which is the only way it becomes a real Echo thread. Attach exists
  because talking to a session should not require killing it, and adopt exists
  because Echo's own thread UI cannot be had any other way.
- **Dispatched sessions only in v1.** `claude agents --json` reports two kinds:
  `background` (daemon-hosted — the fleet) and `interactive` (a `claude` the user
  started in a terminal). Only the first is included. The second is the user's
  own terminal windows, which Echo cannot drive and which would be noise in the
  list; excluding them also keeps the refresh loop to pure disk reads with no
  subprocess. This bounds *what appears in the list*, not how much runs in
  parallel: the fleet is N sessions, each of which may run its own team of
  teammates and its own subagents.
- **Workflows are wire-triggered and disk-followed.** The launch and the script
  come from the ACP tool call; live progress comes from tailing the run
  directory. Neither source is sufficient alone.
- **Phase attribution is a labelled heuristic, never a silent guess.** An agent
  Echo cannot place renders under *Unattributed*, not under a wrong phase.
- **A new `fleet` crate for the mechanism, existing UI crates for the surface.**
  Same shape as `listen`: the crate is pure and testable, the UI wiring lives
  where the UI already is. `sidebar.rs` is 8,591 lines and gets rows, not
  readers.

## Architecture

New crate `crates/fleet/`, `[lib] path = "src/fleet.rs"`.

| File | Holds |
|---|---|
| `src/fleet.rs` | `Fleet` entity, refresh loop, dispatch/stop/respawn/adopt, shared types |
| `src/claude_home.rs` | Pure parsers over `~/.claude`. Path in, value out. No process spawning, no GPUI. |
| `src/workflow.rs` | `WorkflowRun` model, script parsing, journal/transcript join, phase attribution |

Splitting the parsers out is what makes this testable: every format below is a
private Claude Code implementation detail, so the parsers are the part most
likely to break on a CLI upgrade and the part that must be exercised by fixtures.

---

# Part 1 — Fleet

## Data planes

| Plane | Source | Documented? |
|---|---|---|
| Roster | `~/.claude/daemon/roster.json` — `workers{}` keyed by short id, each with `pid`, `sessionId`, `cwd`, `cliVersion`, `dispatch.launch.args` | No |
| Detail | `~/.claude/jobs/<id>/state.json` — `state`, `detail`, `tokens`, `output.result`, `intent`, `children`, `resumeSessionId`, `linkScanPath` | No |
| Activity | `~/.claude/jobs/<id>/timeline.jsonl` — one record per state transition: `{at, state, detail, text}` | No |
| Transcript | `state.json.linkScanPath` → `~/.claude/projects/<mangled-cwd>/<session>.jsonl` | No |
| Team | `~/.claude/teams/session-<first 8 of session id>/config.json` — `leadSessionId`, `members[]` | Yes |
| Teammate transcript | `<project>/<session>/subagents/agent-<agentId>.jsonl` + `.meta.json` | No |
| Tasks | `~/.claude/tasks/<full session uuid>/<n>.json` — `subject`, `status`, `blocks`, `blockedBy` | Partly — docs say the directory is keyed by team name; it is keyed by session uuid |

`state.json` is the richest and the least supported. It carries a written summary
of what the session did (`detail: "scout spawned and idle; instruction
delivered"`) that nothing else exposes.

## Refresh

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

## Types

```rust
pub struct FleetSession {
    pub short_id: String,           // "91214c57" — daemon id, also the jobs dir
    pub session_id: acp::SessionId, // full uuid; the ACP session id on adopt
    pub name: SharedString,         // --name, or CLI-generated
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

A prompt field at the section header sends `claude --bg --name <n> <prompt>` in
the active project's root, optionally with
`CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS=1`.

Teams stay behind a setting, default off: the variable changes delegation for the
whole session — any subagent Claude names becomes a teammate — and teams are
experimental, with no session resumption and a fixed lead.

### Talking to a session

Three levels of access, because a running session and a finished one admit
different things. All three are offered; none is a fallback for a failure of
another.

| Level | Mechanism | Session keeps running |
|---|---|---|
| **Read** | Tail the transcript at `linkScanPath` | Yes |
| **Attach** | `claude attach <short_id>` in a terminal Echo owns | Yes |
| **Adopt** | `claude stop`, then ACP `load_session` | No |

**Attach** is the answer to *"can I talk to one of these?"*, and it is the only
one that is both interactive and non-destructive. There is no `claude send` or
`claude message`: the CLI's hidden subcommands are exactly `attach`, `logs`,
`stop`, `respawn`, and `rm`, so `attach` is the sole supported way to put a turn
into a live session from outside it.

Echo is well placed for it. It is a Zed fork with a first-class terminal
(`crates/terminal_view`, `TerminalPanel::spawn_task`), and the sidebar already
renders `ListEntry::Terminal` rows beside threads and subagents
(`crates/sidebar/src/sidebar.rs:430`). So attaching opens a terminal entry
**nested under the session's own row**, next to its teammates — the session's
real interface, in place, without leaving Echo and without stopping anything.

What it is not: Echo's own thread UI. The attached view is Claude Code's TUI, so
it does not get Echo's transcript rendering, narration, or voice. That is the
honest trade for talking to a session that is still working, and it is why adopt
still exists.

**Adopt** turns a session into a real Echo thread, and requires stopping it
first, so it is explicit and never automatic:

| Session state | Offered |
|---|---|
| `done`, `stopped`, `failed` | **Open in Echo** — `claude stop` (no-op if exited), then ACP `load_session` |
| `working`, `blocked` | **Attach** for a live conversation. Adopting is offered as **Stop and open**, with the stop named in the button. |

Writing directly into a teammate's mailbox at
`~/.claude/teams/<team>/inboxes/<agent>.json` would be a second way to reach a
running session, and is rejected for v1: Claude Code tells the recipient the
message came from another agent rather than from the user, so it cannot approve
a permission prompt or carry consent, and a malformed entry is a private-format
corruption Echo would be causing. `attach` delivers a real user turn; the mailbox
delivers a nudge.

`--fork-session` would allow adopting a live session by branching a copy, at the
cost of two divergent transcripts for one piece of work. Not in v1.

---

# Part 2 — Workflows

## Lifecycle

| Stage | Source | What Echo gets |
|---|---|---|
| Launch | ACP `tool_call`, `_meta.claudeCode.toolName == "Workflow"` | A run exists; render a placeholder immediately |
| Script | `tool_call_update.rawInput.script` | Full source: `meta.name`, `meta.description`, `meta.phases[]`, and every `agent()` call site with its `label` / `phase` |
| Identity | `_meta.claudeCode.toolResponse` | `runId`, `workflowName`, `summary`, `transcriptDir`, `scriptPath` |
| Progress | tail `transcriptDir/journal.jsonl` | `started` / `result` per `agentId`, in start order |
| Detail | `transcriptDir/agent-<agentId>.jsonl` | Prompt, tool calls, result, `usage` per assistant message, timestamps |

`agentId` in the journal is exactly the `agent-<agentId>.jsonl` filename, so the
journal-to-transcript join is exact. Tokens and duration are computed from the
transcript rather than read: sum `usage` across assistant records, and take
last-minus-first `timestamp`.

## Phase attribution

The one thing not on disk. The script *does* carry it — `phase('Alpha')`
statements and `{ phase: 'Alpha', label: 'alpha' }` options sit next to the
prompt each agent receives — and the script arrives on the wire.

Echo resolves an agent to a phase by locating that agent's first prompt (the
first record of its transcript) as a substring of the script source, then taking
the nearest preceding `phase(` or `phase:` annotation. No JavaScript is parsed;
this is a text search over a string Echo already holds.

```
// ponytail: substring match, nearest preceding phase marker. Fails for prompts
// built at runtime (template literals over a list); those render Unattributed.
// Upgrade path is asking Claude Code to persist label+phase in agent meta.json.
```

It fails cleanly and in a known direction: a dynamically-built prompt will not
appear literally in the script, so its agent lands under **Unattributed** — a
real group in the UI, never a wrong phase. Every agent is always shown; only its
grouping is uncertain.

Declared phases from `meta.phases` are rendered whether or not any agent has been
attributed to them, so a run's shape is visible from the first frame.

## Types

```rust
pub struct WorkflowRun {
    pub run_id: String,               // "wf_11e12c55-af7"
    pub name: SharedString,
    pub summary: SharedString,
    pub transcript_dir: PathBuf,
    pub script: SharedString,          // from the wire; the attribution source
    pub declared_phases: Vec<Phase>,   // meta.phases, in order
    pub agents: Vec<WorkflowAgent>,
    pub launched_by: acp::SessionId,   // the thread that ran the tool call
}

pub struct WorkflowAgent {
    pub agent_id: String,
    pub phase: Option<SharedString>,   // None renders as Unattributed
    pub label: Option<SharedString>,
    pub status: AgentStatus,           // Running | Done | Failed
    pub prompt: SharedString,
    pub result: Option<SharedString>,
    pub tokens: u64,
    pub duration: Option<Duration>,
}
```

## Surface

A workflow run belongs to the thread that launched it, so it nests there rather
than in the fleet:

- **In the thread** — the `Workflow` tool call renders as a live run card: name,
  summary, phase pills, `12/40 agents`, token total, elapsed. Replaces today's
  bare `Workflow` tool-call row.
- **In the sidebar** — the run nests under its thread like a subagent, with its
  agent count and status.
- **Run view** — a dedicated pane, the surface a terminal cannot give you:

```
Recon ▸ 3 agents · 148k tokens · 2m 14s          ████████░░ done
  competitive teardown      done    52k   1m 48s
  tech foundation           done    47k   2m 14s
  user jobs-to-be-done      done    49k   1m 52s
Concepts ▸ 4 agents · 210k · running             ██████░░░░ 2/4
  concept: MVP-first        running 38k   —
  …
Unattributed ▸ 1 agent
```

Selecting an agent opens its full transcript in the existing read-only viewer —
its prompt, every tool call, and its result. That is the thing `/workflows`
cannot do well in a terminal: read four agents' reasoning side by side.

Phases render in `meta.phases` order; agents within a phase in start order.

### Launching one

The adapter stamps `origin: { kind: "human" }` on prompts, which is the exact
gate the `ultracode` keyword checks — verified in the adapter source. So Echo can
launch workflows today with no protocol work: a **Run as workflow** toggle on the
message editor prefixes the prompt with `ultracode: `.

### Controls

Pause, resume, stop, and restart-agent are keys in the CLI's `/workflows` view
(`p`, `x`, `r`), not tool calls, and no ACP method carries them. v1 is
observe-only, with **Stop** available through the parent thread's existing cancel
path — cancelling the turn tears down the run. Finer control is deferred rather
than faked.

## Failure modes

| Situation | Behavior |
|---|---|
| `claude` not on PATH | Fleet section hidden. Dispatch surfaces the reason rather than failing silently. |
| Daemon not running | Empty fleet, not an error. The first dispatch starts it. |
| `state.json` schema changed by a CLI upgrade | Per-field tolerance: an unparsable field is `None`, an unknown state is `Unknown`. A session missing `state.json` still renders from `roster.json`. |
| `jobs/<id>` left after `claude rm` | Rows come from `roster.json` ∪ `jobs/`; an entry in neither the roster nor with a terminal state older than the retention sweep is dropped. |
| Session stopped outside Echo | Next poll reconciles. No cached authority. |
| Attach to a session whose process already exited | `claude attach` fails; the terminal shows its error. Offer **Open in Echo** instead, which works on an exited session. |
| Adopt races the daemon | `load_session` is the source of truth: if it returns the bg-guard error, the session did not stop; report it and stay read-only. |
| Transcript is megabytes | Same bounded-window tail as `subagent_notifications.rs`. |
| Workflow tool call arrives without a `toolResponse` | The run is unlocatable; render the tool call as it renders today and do not fabricate a run. |
| `journal.jsonl` absent or empty | Run card shows declared phases and `0 agents`; agents appear as transcripts land. |
| Agent transcript present, no journal record | Render it as running. The journal lags the transcript, never leads it. |
| Prompt not found in the script | Agent renders under **Unattributed**. Never dropped, never misfiled. |
| Echo restarts mid-run | The run dir is on disk and the thread persists; re-derive from `transcriptDir`. The script is lost with the tool call unless persisted — so persist `run_id`, `transcript_dir`, and `script` with the thread, or reread `scriptPath`. |

The largest risk across both parts is that every high-value field lives in an
undocumented format. The mitigation is that each surface degrades rather than
breaks: `roster.json` alone gives a working fleet view, and a workflow with no
attribution is still a complete list of agents with prompts, results, and
transcripts.

## Testing

Fixtures captured from the spike — a real `state.json`, `roster.json`,
`timeline.jsonl`, team `config.json`, teammate `.meta.json`, a two-agent
`journal.jsonl`, and the `echo-alpha-beta` script — go in
`crates/fleet/test_fixtures/`. Every parser is a pure function over a path or a
string, so the suite runs with no daemon, no network, and no Claude Code
installed.

Fleet:

- A `state.json` with an unrecognized `state` renders `Unknown`, does not panic.
- A session in `roster.json` with no `jobs/` dir still renders.
- A team `config.json` with only a `team-lead` produces no teammate rows.
- A task set with `blockedBy` produces the right blocked ordering.
- Tail resumption from a byte offset does not re-emit seen records.

Workflows:

- `meta.phases` extracts in declaration order from a real script.
- An agent whose prompt appears verbatim attributes to the nearest preceding
  phase marker.
- An agent whose prompt is absent attributes to `None`, and renders.
- A prompt appearing twice in the script attributes to the first match and is
  flagged ambiguous rather than silently split.
- Journal `result` before the transcript's last record still yields `Done`.
- Tokens sum across multiple assistant records in one transcript.

One end-to-end test that actually dispatches and one that actually runs a
workflow, both behind `RUN_FLEET_E2E=1` and excluded from CI. They cost tokens
and need a real daemon.

## Build order

Fleet and workflows are independent after step 1 and can be built in either
order, or in parallel.

1. `claude_home.rs` parsers, against fixtures. No UI.
2. `Fleet` entity and the refresh loop. Assert against fixtures on a temp dir.
3. Sidebar fleet section, read-only: rows, grouping, blocked-first ordering.
4. Read-only transcript view, reusing the existing tail.
5. Dispatch, stop, respawn.
6. Attach: a terminal entry running `claude attach`, nested under the session.
7. Adopt.
8. Teams: the env var at dispatch, `config.json` members as nested rows,
   teammate transcripts.
9. Task graph under a session.
10. `workflow.rs`: script parsing, journal/transcript join, phase attribution.
    Fixtures only, no UI.
11. Detect the `Workflow` tool call, capture script and `toolResponse`, persist
    with the thread.
12. Run card in the thread, replacing the bare tool-call row.
13. Run view: phases, agents, per-agent transcript.
14. **Run as workflow** toggle on the message editor.

Steps 1–4 are a complete, useful, read-only fleet view and ship alone. Steps
10–13 are a complete workflow viewer and ship alone.

## Not in v1

- **Interactive session listing.** `claude agents --json` sees the user's own
  terminals; that is a different product than a managed fleet.
- **Workflow pause / resume / restart-agent.** Keystrokes in the CLI's own view
  with no protocol equivalent. Observe-only until there is one.
- **Saving a run as a command.** `/workflows` `s` writes to `.claude/workflows/`;
  Echo can offer it once the run view exists.
- **Voice.** `listen` should eventually answer *"what's blocked?"* across the
  fleet and *"how far along is the audit?"* across a run — the natural payoff of
  Echo having both. `FleetState` and `WorkflowRun` exposing status is the whole
  hook needed; the routing is a later change.
- **Cross-session messaging** between fleet sessions.
- **Worktree management UI.** The daemon creates and cleans them.
- **`--fork-session`** adoption of a live session.
