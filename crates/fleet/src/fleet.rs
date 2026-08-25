//! Echo's reader for the parallel work Claude Code is already doing.
//!
//! Claude Code runs sessions the user never sees: a supervisor daemon hosts
//! them, gives each its own git worktree, and records what they are doing under
//! `~/.claude`. This crate turns that on-disk record into values Echo can
//! render. It schedules nothing and spawns nothing.
//!
//! See `docs/superpowers/specs/2026-08-22-fleet-and-workflows-design.md`.

pub mod claude_home;
pub mod control;
pub mod workflow;

use serde::{Deserialize, Deserializer};

/// What a dispatched session is doing, as the daemon last wrote it down.
///
/// These strings are a private Claude Code implementation detail, so an
/// unrecognized one is carried through as [`FleetState::Unknown`] rather than
/// rejected: a CLI upgrade that adds a state must cost the row its icon, not
/// its existence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FleetState {
    Working,
    /// Waiting on the user — a permission prompt, or a question.
    Blocked,
    Done,
    Failed,
    Stopped,
    Unknown(String),
}

impl FleetState {
    pub fn from_wire(state: &str) -> Self {
        match state.trim() {
            "working" => Self::Working,
            "blocked" => Self::Blocked,
            "done" => Self::Done,
            "failed" => Self::Failed,
            "stopped" => Self::Stopped,
            other => Self::Unknown(other.to_string()),
        }
    }

    /// Whether this session still has work in flight.
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Working | Self::Blocked)
    }

    /// Whether the session is waiting on the user. These sort to the top of the
    /// fleet, which is the reason the surface exists.
    pub fn needs_attention(&self) -> bool {
        matches!(self, Self::Blocked)
    }

    pub fn label(&self) -> &str {
        match self {
            Self::Working => "Working",
            Self::Blocked => "Needs input",
            Self::Done => "Done",
            Self::Failed => "Failed",
            Self::Stopped => "Stopped",
            Self::Unknown(other) => other,
        }
    }
}

impl Default for FleetState {
    fn default() -> Self {
        Self::Unknown(String::new())
    }
}

impl<'de> Deserialize<'de> for FleetState {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Deliberately infallible: a state that arrives as a number, an object,
        // or anything else still has to produce a renderable row.
        let value = serde_json::Value::deserialize(deserializer)?;
        Ok(match value.as_str() {
            Some(state) => Self::from_wire(state),
            None => Self::Unknown(value.to_string()),
        })
    }
}

use agent_client_protocol::schema::v1 as acp;
use claude_home::{AgentMeta, JobState, RosterWorker, Task as TaskItem, TeamConfig};
use gpui::{App, AppContext as _, Context, Entity, Global, SharedString, Task as GpuiTask};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// How often the fleet is re-read while its section is on screen.
///
// ponytail: 2s poll of a directory of small JSON files. Move to fs::watch if a
// large fleet shows up in a profile — the readers don't change either way.
const REFRESH_INTERVAL: Duration = Duration::from_secs(2);

/// A teammate spawned inside a fleet session.
#[derive(Clone, Debug)]
pub struct Teammate {
    pub name: SharedString,
    pub agent_type: Option<SharedString>,
    pub model: Option<SharedString>,
    /// The colour the CLI assigned, so Echo can agree with the terminal.
    pub color: Option<SharedString>,
    /// Transcript on disk, if this teammate has produced one yet.
    pub transcript: Option<PathBuf>,
}

/// One dispatched session, assembled from every plane that mentions it.
#[derive(Clone, Debug)]
pub struct FleetSession {
    /// Daemon id, and the name of the `jobs/` directory.
    pub short_id: SharedString,
    /// The ACP session id this becomes when adopted.
    pub session_id: acp::SessionId,
    pub name: SharedString,
    pub cwd: Option<PathBuf>,
    pub state: FleetState,
    /// The session's own written account of what it did.
    pub detail: Option<SharedString>,
    /// The prompt it was dispatched with.
    pub intent: Option<SharedString>,
    pub tokens: u64,
    pub transcript: Option<PathBuf>,
    pub teammates: Vec<Teammate>,
    pub tasks: Vec<TaskItem>,
}

impl FleetSession {
    /// Tasks nothing is waiting on, in list order.
    pub fn claimable_tasks(&self) -> impl Iterator<Item = &TaskItem> {
        self.tasks
            .iter()
            .filter(|task| task.is_claimable(&self.tasks))
    }

    /// Sort key: sessions waiting on the user first, then those still working,
    /// then everything finished. The whole point of the surface is that a
    /// blocked session is never below the fold.
    fn attention_rank(&self) -> u8 {
        if self.state.needs_attention() {
            0
        } else if self.state.is_active() {
            1
        } else {
            2
        }
    }
}

/// Read every dispatched session the daemon knows about.
///
/// Sessions come from the roster and from `jobs/` unioned, not from either
/// alone: a worker can be in the roster before its job directory exists, and a
/// finished job outlives its roster entry.
pub fn read_fleet(home: &Path) -> Vec<FleetSession> {
    let mut by_short_id: std::collections::BTreeMap<
        String,
        (Option<JobState>, Option<RosterWorker>),
    > = std::collections::BTreeMap::new();

    let roster_path = home.join("daemon").join("roster.json");
    if roster_path.exists() {
        match claude_home::read_roster(&roster_path) {
            Ok(workers) => {
                for worker in workers {
                    let short_id = worker.short_id.clone();
                    by_short_id.entry(short_id).or_default().1 = Some(worker);
                }
            }
            Err(error) => log::warn!("fleet: unreadable roster: {error:#}"),
        }
    }

    let jobs_root = home.join("jobs");
    if let Ok(entries) = std::fs::read_dir(&jobs_root) {
        for entry in entries.flatten() {
            let path = entry.path();
            // `pins.json` lives beside the job directories.
            if !path.is_dir() {
                continue;
            }
            let Some(short_id) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let state_path = path.join("state.json");
            if !state_path.exists() {
                continue;
            }
            match claude_home::read_job_state(&state_path) {
                Ok(state) => {
                    by_short_id.entry(short_id.to_string()).or_default().0 = Some(state);
                }
                Err(error) => log::warn!("fleet: unreadable job {short_id}: {error:#}"),
            }
        }
    }

    let mut sessions: Vec<FleetSession> = by_short_id
        .into_iter()
        .filter_map(|(short_id, (state, worker))| build_session(home, short_id, state, worker))
        .collect();

    sessions.sort_by(|left, right| {
        left.attention_rank()
            .cmp(&right.attention_rank())
            .then_with(|| left.name.cmp(&right.name))
    });
    sessions
}

fn build_session(
    home: &Path,
    short_id: String,
    state: Option<JobState>,
    worker: Option<RosterWorker>,
) -> Option<FleetSession> {
    let session_id = state
        .as_ref()
        .and_then(|state| state.session_id.clone())
        .or_else(|| worker.as_ref().and_then(|worker| worker.session_id.clone()))?;

    let name = state
        .as_ref()
        .and_then(|state| state.name.clone())
        .unwrap_or_else(|| short_id.clone());

    let cwd = state
        .as_ref()
        .and_then(|state| state.cwd.clone())
        .or_else(|| worker.as_ref().and_then(|worker| worker.cwd.clone()));

    let teammates = read_teammates(home, &session_id, cwd.as_deref());
    let tasks = claude_home::read_tasks(&home.join("tasks").join(&session_id)).unwrap_or_default();

    Some(FleetSession {
        short_id: short_id.into(),
        session_id: acp::SessionId::new(Arc::from(session_id.as_str())),
        name: name.into(),
        cwd,
        state: state
            .as_ref()
            .map(|state| state.state.clone())
            .unwrap_or_default(),
        detail: state
            .as_ref()
            .and_then(|state| state.detail.clone())
            .map(SharedString::from),
        intent: state
            .as_ref()
            .and_then(|state| state.intent.clone())
            .map(SharedString::from),
        tokens: state.as_ref().and_then(|state| state.tokens).unwrap_or(0),
        transcript: state.as_ref().and_then(|state| state.transcript.clone()),
        teammates,
        tasks,
    })
}

/// Teammates come from the team config, and their transcripts from the
/// session's own `subagents/` directory. A team with only a lead has none.
fn read_teammates(home: &Path, session_id: &str, cwd: Option<&Path>) -> Vec<Teammate> {
    let config_path = home
        .join("teams")
        .join(claude_home::team_dir_name(session_id))
        .join("config.json");
    if !config_path.exists() {
        return Vec::new();
    }
    let config: TeamConfig = match claude_home::read_team_config(&config_path) {
        Ok(config) => config,
        Err(error) => {
            log::warn!("fleet: unreadable team config for {session_id}: {error:#}");
            return Vec::new();
        }
    };

    let subagents = cwd.map(|cwd| transcript_dir(home, cwd, session_id).join("subagents"));

    config
        .teammates()
        .map(|member| Teammate {
            name: member.name.clone().into(),
            agent_type: member.agent_type.clone().map(SharedString::from),
            model: member.model.clone().map(SharedString::from),
            color: member.color.clone().map(SharedString::from),
            transcript: subagents
                .as_ref()
                .and_then(|directory| find_teammate_transcript(directory, &member.name)),
        })
        .collect()
}

/// Claude Code mangles a working directory into a project directory name by
/// replacing every path separator with a dash.
pub fn transcript_dir(home: &Path, cwd: &Path, session_id: &str) -> PathBuf {
    let mangled: String = cwd
        .to_string_lossy()
        .chars()
        .map(|character| if character == '/' { '-' } else { character })
        .collect();
    home.join("projects").join(mangled).join(session_id)
}

/// A teammate's transcript is `agent-<agentId>.jsonl`, and the agent id is not
/// the teammate's name — so the `.meta.json` beside each one is what names it.
fn find_teammate_transcript(subagents: &Path, name: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir(subagents).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|extension| extension != "json") {
            continue;
        }
        let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !file_name.ends_with(".meta.json") {
            continue;
        }
        let meta: AgentMeta = match claude_home::read_agent_meta(&path) {
            Ok(meta) => meta,
            Err(_) => continue,
        };
        if meta.name.as_deref() == Some(name) {
            return Some(path.with_extension("").with_extension("jsonl"));
        }
    }
    None
}

/// The fleet, refreshed while anything is looking at it.
pub struct Fleet {
    home: Option<PathBuf>,
    sessions: Vec<FleetSession>,
    refresh: Option<GpuiTask<()>>,
    /// Number of surfaces currently showing the fleet. Polling runs only while
    /// this is above zero, so a collapsed section costs nothing.
    watchers: usize,
}

struct GlobalFleet(Entity<Fleet>);

impl Global for GlobalFleet {}

impl Fleet {
    pub fn new() -> Self {
        Self {
            home: claude_home::claude_home(),
            sessions: Vec::new(),
            refresh: None,
            watchers: 0,
        }
    }

    /// The shared fleet, created on first use.
    pub fn global(cx: &mut App) -> Entity<Self> {
        if let Some(existing) = cx.try_global::<GlobalFleet>() {
            return existing.0.clone();
        }
        let fleet = cx.new(|_| Self::new());
        cx.set_global(GlobalFleet(fleet.clone()));
        fleet
    }

    pub fn sessions(&self) -> &[FleetSession] {
        &self.sessions
    }

    /// Sessions grouped by working directory, groups in first-seen order so the
    /// blocked-first ordering survives grouping.
    pub fn by_directory(&self) -> Vec<(Option<PathBuf>, Vec<&FleetSession>)> {
        let mut groups: Vec<(Option<PathBuf>, Vec<&FleetSession>)> = Vec::new();
        for session in &self.sessions {
            match groups.iter_mut().find(|(cwd, _)| cwd == &session.cwd) {
                Some((_, members)) => members.push(session),
                None => groups.push((session.cwd.clone(), vec![session])),
            }
        }
        groups
    }

    pub fn needs_attention_count(&self) -> usize {
        self.sessions
            .iter()
            .filter(|session| session.state.needs_attention())
            .count()
    }

    /// Start refreshing. Balanced by [`Fleet::release`].
    pub fn watch(&mut self, cx: &mut Context<Self>) {
        self.watchers += 1;
        if self.refresh.is_some() {
            return;
        }
        let Some(home) = self.home.clone() else {
            return;
        };
        self.refresh = Some(cx.spawn(async move |this, cx| {
            loop {
                let sessions = cx
                    .background_executor()
                    .spawn({
                        let home = home.clone();
                        async move { read_fleet(&home) }
                    })
                    .await;
                let still_watched = this.update(cx, |this, cx| {
                    let changed = this.sessions.len() != sessions.len()
                        || this.sessions.iter().zip(&sessions).any(|(before, after)| {
                            before.short_id != after.short_id
                                || before.state != after.state
                                || before.detail != after.detail
                                || before.tokens != after.tokens
                        });
                    this.sessions = sessions;
                    if changed {
                        cx.notify();
                    }
                    this.watchers > 0
                });
                match still_watched {
                    Ok(true) => {}
                    // Either every watcher went away or the entity is gone.
                    _ => break,
                }
                cx.background_executor().timer(REFRESH_INTERVAL).await;
            }
        }));
    }

    /// Stop refreshing once the last watcher goes away.
    pub fn release(&mut self, _cx: &mut Context<Self>) {
        self.watchers = self.watchers.saturating_sub(1);
        if self.watchers == 0 {
            self.refresh = None;
        }
    }

    /// Re-read now, without waiting for the next tick. Used after an action
    /// that is known to have changed the fleet.
    pub fn refresh_now(&mut self, cx: &mut Context<Self>) {
        let Some(home) = self.home.clone() else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let sessions = cx
                .background_executor()
                .spawn(async move { read_fleet(&home) })
                .await;
            this.update(cx, |this, cx| {
                this.sessions = sessions;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}

impl Default for Fleet {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DONE_SESSION: &str = "888caac0-731a-4a10-80c4-9133d5f14877";
    const BLOCKED_SESSION: &str = "e9b5670a-0f01-4df8-9ddf-547b333d2703";

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("test_fixtures")
            .join(name)
    }

    /// Build a `~/.claude` from the captured fixtures, so assembly is exercised
    /// against the shapes the daemon actually writes.
    fn fake_home() -> tempfile::TempDir {
        let home = tempfile::tempdir().expect("temp dir");
        let root = home.path();

        std::fs::create_dir_all(root.join("daemon")).expect("daemon dir");
        std::fs::copy(fixture("roster.json"), root.join("daemon/roster.json")).expect("roster");

        for (short_id, state) in [
            ("888caac0", "state-done.json"),
            ("e9b5670a", "state-blocked.json"),
        ] {
            let job = root.join("jobs").join(short_id);
            std::fs::create_dir_all(&job).expect("job dir");
            std::fs::copy(fixture(state), job.join("state.json")).expect("state");
        }
        std::fs::copy(
            fixture("timeline.jsonl"),
            root.join("jobs/e9b5670a/timeline.jsonl"),
        )
        .expect("timeline");
        // The daemon keeps this beside the job directories; it is not a session.
        std::fs::write(root.join("jobs/pins.json"), "{}").expect("pins");

        let team = root.join("teams").join("session-e9b5670a");
        std::fs::create_dir_all(&team).expect("team dir");
        std::fs::copy(fixture("team-config.json"), team.join("config.json")).expect("team config");

        let tasks = root.join("tasks").join(BLOCKED_SESSION);
        std::fs::create_dir_all(&tasks).expect("tasks dir");
        for entry in std::fs::read_dir(fixture("tasks")).expect("task fixtures") {
            let path = entry.expect("task fixture").path();
            let name = path.file_name().expect("task file name");
            std::fs::copy(&path, tasks.join(name)).expect("task");
        }
        home
    }

    #[test]
    fn assembles_sessions_from_roster_and_jobs() {
        let home = fake_home();
        let sessions = read_fleet(home.path());
        assert_eq!(sessions.len(), 2);
        assert!(
            sessions.iter().all(|session| !session.name.is_empty()),
            "every session renders with a name"
        );
    }

    #[test]
    fn pins_json_is_not_a_session() {
        let home = fake_home();
        let sessions = read_fleet(home.path());
        assert!(
            !sessions
                .iter()
                .any(|session| session.short_id == "pins.json"),
            "a file beside the job directories is not a session"
        );
    }

    #[test]
    fn a_blocked_session_sorts_above_a_finished_one() {
        let home = fake_home();
        let sessions = read_fleet(home.path());
        assert_eq!(
            sessions[0].session_id.0.as_ref(),
            BLOCKED_SESSION,
            "the session waiting on the user comes first"
        );
        assert!(sessions[0].state.needs_attention());
        assert_eq!(sessions[1].session_id.0.as_ref(), DONE_SESSION);
    }

    #[test]
    fn a_session_in_the_roster_without_a_job_directory_still_renders() {
        let home = fake_home();
        // Drop the job directory but leave the roster entry, which is what a
        // worker looks like between dispatch and its first state write.
        std::fs::remove_dir_all(home.path().join("jobs/888caac0")).expect("remove job");

        let sessions = read_fleet(home.path());
        let recovered = sessions
            .iter()
            .find(|session| session.short_id == "888caac0")
            .expect("roster alone is enough to render a row");
        assert_eq!(recovered.state, FleetState::Unknown(String::new()));
        assert_eq!(recovered.name, "888caac0", "falls back to the daemon id");
    }

    #[test]
    fn an_empty_home_is_an_empty_fleet_not_an_error() {
        let home = tempfile::tempdir().expect("temp dir");
        assert!(read_fleet(home.path()).is_empty());
    }

    #[test]
    fn resolves_teammates_from_the_team_config() {
        let home = fake_home();
        let sessions = read_fleet(home.path());
        let lead = sessions
            .iter()
            .find(|session| session.session_id.0.as_ref() == BLOCKED_SESSION)
            .expect("blocked session present");

        assert_eq!(lead.teammates.len(), 1, "the lead is not its own teammate");
        assert_eq!(lead.teammates[0].name, "scout");
        assert_eq!(lead.teammates[0].model.as_deref(), Some("opus[1m]"));
    }

    #[test]
    fn a_session_without_a_team_has_no_teammates() {
        let home = fake_home();
        let sessions = read_fleet(home.path());
        let solo = sessions
            .iter()
            .find(|session| session.session_id.0.as_ref() == DONE_SESSION)
            .expect("done session present");
        assert!(solo.teammates.is_empty());
    }

    #[test]
    fn carries_the_sessions_own_account_of_its_work() {
        let home = fake_home();
        let sessions = read_fleet(home.path());
        let done = sessions
            .iter()
            .find(|session| session.session_id.0.as_ref() == DONE_SESSION)
            .expect("done session present");
        assert_eq!(
            done.detail.as_deref(),
            Some("replied with FX_OK as requested")
        );
        assert!(done.transcript.is_some());
    }

    #[test]
    fn reads_the_task_graph_for_a_session() {
        let home = fake_home();
        let sessions = read_fleet(home.path());
        let with_tasks = sessions
            .iter()
            .find(|session| session.session_id.0.as_ref() == BLOCKED_SESSION)
            .expect("blocked session present");

        assert!(!with_tasks.tasks.is_empty());
        // Fixture 14 waits on 4 and 13; neither is completed in the set, so it
        // must not be offered as claimable.
        let claimable: Vec<_> = with_tasks
            .claimable_tasks()
            .map(|task| task.id.clone())
            .collect();
        assert!(
            !claimable.contains(&"14".to_string()),
            "a task with unresolved dependencies is not claimable"
        );
    }

    #[test]
    fn groups_by_working_directory_without_losing_blocked_first() {
        let sessions = vec![
            FleetSession {
                short_id: "a".into(),
                session_id: acp::SessionId::new(Arc::from("a")),
                name: "alpha".into(),
                cwd: Some(PathBuf::from("/one")),
                state: FleetState::Done,
                detail: None,
                intent: None,
                tokens: 0,
                transcript: None,
                teammates: Vec::new(),
                tasks: Vec::new(),
            },
            FleetSession {
                short_id: "b".into(),
                session_id: acp::SessionId::new(Arc::from("b")),
                name: "beta".into(),
                cwd: Some(PathBuf::from("/two")),
                state: FleetState::Blocked,
                detail: None,
                intent: None,
                tokens: 0,
                transcript: None,
                teammates: Vec::new(),
                tasks: Vec::new(),
            },
        ];
        let mut fleet = Fleet::new();
        fleet.sessions = sessions;
        fleet
            .sessions
            .sort_by_key(|session| session.attention_rank());

        let groups = fleet.by_directory();
        assert_eq!(groups.len(), 2);
        assert_eq!(
            groups[0].0.as_deref(),
            Some(Path::new("/two")),
            "the group holding the blocked session leads"
        );
        assert_eq!(fleet.needs_attention_count(), 1);
    }
}
