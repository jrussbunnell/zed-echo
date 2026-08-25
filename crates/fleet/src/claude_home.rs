//! Parsers for the files Claude Code keeps under `~/.claude`.
//!
//! Every function here takes a path or a string and returns a value. Nothing
//! spawns a process, touches GPUI, or holds state, so the whole module is
//! exercised by fixtures with no daemon and no Claude Code installed.
//!
//! None of these formats are documented, and only the team config is even
//! mentioned in Claude Code's docs. They are therefore parsed leniently: every
//! optional field goes through [`lenient`], which yields `None` for a field
//! whose shape changed rather than failing the whole record. A CLI upgrade
//! should cost a column, not the feature.

use anyhow::{Context as _, Result};
use serde::{Deserialize, Deserializer, de::DeserializeOwned};
use std::path::{Path, PathBuf};

use crate::FleetState;

/// Root of Claude Code's state directory, honoring `CLAUDE_CONFIG_DIR`.
pub fn claude_home() -> Option<PathBuf> {
    if let Some(configured) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        let path = PathBuf::from(configured);
        if !path.as_os_str().is_empty() {
            return Some(path);
        }
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".claude"))
}

/// Deserialize a field, yielding `None` if it is present but the wrong shape.
///
/// The whole point of this module's tolerance. `Option<T>` alone is not enough:
/// serde still fails the record when a present field cannot be converted.
fn lenient<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).ok())
}

/// `~/.claude/jobs/<short id>/state.json` — the richest thing the daemon writes,
/// and the only place a session's own summary of its work appears.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct JobState {
    pub state: FleetState,
    /// The session's written account of what it did, e.g. "replied with FX_OK
    /// as requested". Nothing else exposes this.
    #[serde(deserialize_with = "lenient")]
    pub detail: Option<String>,
    /// The prompt the session was dispatched with.
    #[serde(deserialize_with = "lenient")]
    pub intent: Option<String>,
    #[serde(deserialize_with = "lenient")]
    pub tokens: Option<u64>,
    #[serde(rename = "sessionId", deserialize_with = "lenient")]
    pub session_id: Option<String>,
    #[serde(rename = "daemonShort", deserialize_with = "lenient")]
    pub short_id: Option<String>,
    #[serde(deserialize_with = "lenient")]
    pub name: Option<String>,
    #[serde(deserialize_with = "lenient")]
    pub cwd: Option<PathBuf>,
    /// Path to this session's CLI transcript. Named for the link scanner that
    /// writes it, not for us, but it is the transcript path all the same.
    #[serde(rename = "linkScanPath", deserialize_with = "lenient")]
    pub transcript: Option<PathBuf>,
}

pub fn read_job_state(path: &Path) -> Result<JobState> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("reading job state at {}", path.display()))?;
    serde_json::from_str(&contents)
        .with_context(|| format!("parsing job state at {}", path.display()))
}

/// One entry from `~/.claude/daemon/roster.json`.
///
/// The roster is the fallback identity for a session: a worker can be listed
/// here with no `jobs/<id>/state.json` beside it, and still has to render.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct RosterWorker {
    pub short_id: String,
    #[serde(deserialize_with = "lenient")]
    pub session_id: Option<String>,
    #[serde(deserialize_with = "lenient")]
    pub cwd: Option<PathBuf>,
    /// Workers can disagree: the daemon upgrades them individually, so two
    /// sessions in one roster may report different CLI versions.
    #[serde(deserialize_with = "lenient")]
    pub cli_version: Option<String>,
    #[serde(deserialize_with = "lenient")]
    pub pid: Option<u32>,
}

pub fn read_roster(path: &Path) -> Result<Vec<RosterWorker>> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("reading roster at {}", path.display()))?;
    let root: serde_json::Value = serde_json::from_str(&contents)
        .with_context(|| format!("parsing roster at {}", path.display()))?;

    let Some(workers) = root.get("workers").and_then(|workers| workers.as_object()) else {
        return Ok(Vec::new());
    };

    let mut parsed = Vec::with_capacity(workers.len());
    for (short_id, worker) in workers {
        parsed.push(RosterWorker {
            short_id: short_id.clone(),
            session_id: worker
                .get("sessionId")
                .and_then(|value| value.as_str())
                .map(str::to_string),
            cwd: worker
                .get("cwd")
                .and_then(|value| value.as_str())
                .map(PathBuf::from),
            cli_version: worker
                .get("cliVersion")
                .and_then(|value| value.as_str())
                .map(str::to_string),
            pid: worker
                .get("pid")
                .and_then(serde_json::Value::as_u64)
                .and_then(|pid| u32::try_from(pid).ok()),
        });
    }
    parsed.sort_by(|left, right| left.short_id.cmp(&right.short_id));
    Ok(parsed)
}

/// One line of `~/.claude/jobs/<id>/timeline.jsonl`: a state transition, with
/// the text the session produced at that moment.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct TimelineEntry {
    pub state: FleetState,
    #[serde(deserialize_with = "lenient")]
    pub at: Option<String>,
    #[serde(deserialize_with = "lenient")]
    pub detail: Option<String>,
    #[serde(deserialize_with = "lenient")]
    pub text: Option<String>,
}

/// Parse newline-delimited JSON, skipping records that cannot be read.
///
/// Kept separate from the file read so a tail can hand in the bytes it just
/// pulled from an offset without going back to disk.
pub fn parse_jsonl<T: DeserializeOwned>(contents: &str) -> Vec<T> {
    contents
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| match serde_json::from_str(line) {
            Ok(entry) => Some(entry),
            Err(error) => {
                log::debug!("skipping unreadable jsonl record: {error}");
                None
            }
        })
        .collect()
}

pub fn read_timeline(path: &Path) -> Result<Vec<TimelineEntry>> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("reading timeline at {}", path.display()))?;
    Ok(parse_jsonl(&contents))
}

/// The directory name Claude Code derives for a session's team.
pub fn team_dir_name(session_id: &str) -> String {
    let prefix: String = session_id.chars().take(8).collect();
    format!("session-{prefix}")
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct TeamConfig {
    pub name: String,
    #[serde(rename = "leadSessionId", deserialize_with = "lenient")]
    pub lead_session_id: Option<String>,
    pub members: Vec<TeamMember>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct TeamMember {
    pub name: String,
    #[serde(rename = "agentId", deserialize_with = "lenient")]
    pub agent_id: Option<String>,
    /// `team-lead` for the lead; the spawned type, or absent, for a teammate.
    #[serde(rename = "agentType", deserialize_with = "lenient")]
    pub agent_type: Option<String>,
    #[serde(deserialize_with = "lenient")]
    pub model: Option<String>,
    #[serde(deserialize_with = "lenient")]
    pub color: Option<String>,
    #[serde(deserialize_with = "lenient")]
    pub prompt: Option<String>,
}

impl TeamMember {
    /// The lead is the session itself, not a teammate to render beneath it.
    pub fn is_lead(&self) -> bool {
        self.agent_type.as_deref() == Some("team-lead")
    }
}

impl TeamConfig {
    pub fn teammates(&self) -> impl Iterator<Item = &TeamMember> {
        self.members.iter().filter(|member| !member.is_lead())
    }
}

pub fn read_team_config(path: &Path) -> Result<TeamConfig> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("reading team config at {}", path.display()))?;
    serde_json::from_str(&contents)
        .with_context(|| format!("parsing team config at {}", path.display()))
}

/// The `.meta.json` beside a subagent or teammate transcript.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct AgentMeta {
    #[serde(deserialize_with = "lenient")]
    pub name: Option<String>,
    #[serde(rename = "agentType", deserialize_with = "lenient")]
    pub agent_type: Option<String>,
    #[serde(deserialize_with = "lenient")]
    pub model: Option<String>,
    #[serde(deserialize_with = "lenient")]
    pub color: Option<String>,
    #[serde(deserialize_with = "lenient")]
    pub description: Option<String>,
    /// `in_process_teammate` for a teammate, absent for a plain subagent.
    #[serde(rename = "taskKind", deserialize_with = "lenient")]
    pub task_kind: Option<String>,
    #[serde(rename = "teamName", deserialize_with = "lenient")]
    pub team_name: Option<String>,
}

impl AgentMeta {
    pub fn is_teammate(&self) -> bool {
        self.task_kind.as_deref() == Some("in_process_teammate")
    }
}

pub fn read_agent_meta(path: &Path) -> Result<AgentMeta> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("reading agent meta at {}", path.display()))?;
    serde_json::from_str(&contents)
        .with_context(|| format!("parsing agent meta at {}", path.display()))
}

/// One file from `~/.claude/tasks/<session uuid>/<n>.json`.
///
/// Keyed by session id, not by team name — Claude Code's docs say team name and
/// are wrong; verified against live directories.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Task {
    pub id: String,
    #[serde(deserialize_with = "lenient")]
    pub subject: Option<String>,
    #[serde(deserialize_with = "lenient")]
    pub description: Option<String>,
    pub status: String,
    /// Tasks this one must wait for.
    #[serde(rename = "blockedBy")]
    pub blocked_by: Vec<String>,
    /// Tasks waiting on this one.
    pub blocks: Vec<String>,
}

impl Task {
    pub fn is_completed(&self) -> bool {
        self.status == "completed"
    }

    /// Whether a teammate could pick this up right now: it is still waiting to
    /// be started, and every task it depends on has finished.
    ///
    /// A blocker that is no longer on disk counts as satisfied. The task list
    /// is swept on the same retention schedule as transcripts, so treating a
    /// missing dependency as unresolved would strand the rest of the list.
    pub fn is_claimable(&self, all: &[Task]) -> bool {
        if self.status != "pending" {
            return false;
        }
        self.blocked_by.iter().all(|blocker| {
            all.iter()
                .find(|task| &task.id == blocker)
                .is_none_or(Task::is_completed)
        })
    }
}

/// Read a session's task list. Files are named by id, so they are sorted
/// numerically where possible to match the order the CLI assigns.
pub fn read_tasks(directory: &Path) -> Result<Vec<Task>> {
    let entries = std::fs::read_dir(directory)
        .with_context(|| format!("reading task directory {}", directory.display()))?;

    let mut tasks = Vec::new();
    for entry in entries {
        let path = entry
            .with_context(|| format!("listing task directory {}", directory.display()))?
            .path();
        if path.extension().is_none_or(|extension| extension != "json") {
            continue;
        }
        let contents = std::fs::read_to_string(&path)
            .with_context(|| format!("reading task at {}", path.display()))?;
        match serde_json::from_str::<Task>(&contents) {
            Ok(task) => tasks.push(task),
            // One malformed task must not hide the rest of the list.
            Err(error) => log::warn!("skipping unreadable task {}: {error}", path.display()),
        }
    }

    tasks.sort_by(
        |left, right| match (left.id.parse::<u64>(), right.id.parse::<u64>()) {
            (Ok(left_id), Ok(right_id)) => left_id.cmp(&right_id),
            _ => left.id.cmp(&right.id),
        },
    );
    Ok(tasks)
}

/// One record of a workflow run's `journal.jsonl`.
///
/// This is a resume cache, not a progress feed: it carries no phase, no label,
/// and no token count. `agent_id` is the only join it offers, and it is exact —
/// it names the `agent-<agent_id>.jsonl` transcript in the same directory.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct JournalEntry {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(rename = "agentId")]
    pub agent_id: String,
    /// Hash of the agent's prompt and options. Opaque, and not invertible.
    #[serde(deserialize_with = "lenient")]
    pub key: Option<String>,
    #[serde(deserialize_with = "lenient")]
    pub result: Option<serde_json::Value>,
}

impl JournalEntry {
    pub fn is_started(&self) -> bool {
        self.kind == "started"
    }

    pub fn is_result(&self) -> bool {
        self.kind == "result"
    }
}

pub fn read_journal(path: &Path) -> Result<Vec<JournalEntry>> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("reading workflow journal at {}", path.display()))?;
    Ok(parse_jsonl(&contents))
}

/// Transcript filename for an agent named in a journal record.
pub fn agent_transcript_name(agent_id: &str) -> String {
    format!("agent-{agent_id}.jsonl")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("test_fixtures")
            .join(name)
    }

    #[test]
    fn reads_a_finished_job() {
        let state = read_job_state(&fixture("state-done.json")).expect("fixture parses");
        assert_eq!(state.state, FleetState::Done);
        assert_eq!(
            state.detail.as_deref(),
            Some("replied with FX_OK as requested")
        );
        assert_eq!(state.short_id.as_deref(), Some("888caac0"));
        assert!(
            state.transcript.is_some(),
            "a finished job names its transcript"
        );
        assert!(!state.state.is_active());
    }

    #[test]
    fn a_blocked_job_asks_for_attention() {
        let state = read_job_state(&fixture("state-blocked.json")).expect("fixture parses");
        assert_eq!(state.state, FleetState::Blocked);
        assert!(
            state.state.needs_attention(),
            "blocked sessions sort to the top"
        );
        assert!(state.state.is_active());
        assert!(state.tokens.unwrap_or(0) > 0);
    }

    #[test]
    fn an_unrecognized_state_survives_as_itself() {
        // A CLI upgrade that adds a state must cost the row its icon, not its
        // existence.
        let state: JobState =
            serde_json::from_str(r#"{"state":"hibernating","detail":"zzz"}"#).expect("parses");
        assert_eq!(state.state, FleetState::Unknown("hibernating".into()));
        assert_eq!(state.state.label(), "hibernating");
        assert_eq!(state.detail.as_deref(), Some("zzz"));
    }

    #[test]
    fn a_field_of_the_wrong_shape_is_dropped_not_fatal() {
        let state: JobState = serde_json::from_str(
            r#"{"state":"done","tokens":{"nested":"object"},"detail":["not","a","string"],
                "daemonShort":"abc12345"}"#,
        )
        .expect("a wrong-shaped field must not fail the record");
        assert_eq!(state.state, FleetState::Done);
        assert_eq!(state.tokens, None);
        assert_eq!(state.detail, None);
        assert_eq!(state.short_id.as_deref(), Some("abc12345"));
    }

    #[test]
    fn a_state_that_is_not_a_string_still_renders() {
        let state: JobState = serde_json::from_str(r#"{"state":42}"#).expect("parses");
        assert_eq!(state.state, FleetState::Unknown("42".into()));
    }

    #[test]
    fn an_empty_job_state_is_not_an_error() {
        let state: JobState = serde_json::from_str("{}").expect("parses");
        assert_eq!(state.state, FleetState::Unknown(String::new()));
        assert_eq!(state.detail, None);
    }

    #[test]
    fn reads_the_roster_and_tolerates_version_skew() {
        let workers = read_roster(&fixture("roster.json")).expect("fixture parses");
        assert_eq!(workers.len(), 2);
        assert!(workers.iter().all(|worker| worker.session_id.is_some()));

        // The daemon upgrades workers individually, so one roster can name two
        // CLI versions. Neither is authoritative for the other.
        let versions: Vec<_> = workers
            .iter()
            .filter_map(|worker| worker.cli_version.clone())
            .collect();
        assert_eq!(versions.len(), 2);
    }

    #[test]
    fn a_roster_without_workers_is_empty_not_an_error() {
        let path = fixture("roster.json");
        let empty = std::env::temp_dir().join("fleet-empty-roster.json");
        std::fs::write(&empty, r#"{"proto":1,"workers":{}}"#).expect("writes");
        assert!(read_roster(&empty).expect("parses").is_empty());
        std::fs::remove_file(&empty).ok();

        // And a roster missing the key entirely, which is what a freshly
        // stopped daemon can leave behind.
        let missing = std::env::temp_dir().join("fleet-missing-workers.json");
        std::fs::write(&missing, r#"{"proto":1}"#).expect("writes");
        assert!(read_roster(&missing).expect("parses").is_empty());
        std::fs::remove_file(&missing).ok();

        assert!(path.exists(), "fixture still present");
    }

    #[test]
    fn reads_a_timeline_of_transitions() {
        let entries = read_timeline(&fixture("timeline.jsonl")).expect("fixture parses");
        assert!(!entries.is_empty());
        assert!(
            entries.iter().any(|entry| entry.detail.is_some()),
            "a transition carries the session's own account of it"
        );
    }

    #[test]
    fn jsonl_skips_a_bad_record_and_keeps_the_rest() {
        let entries: Vec<TimelineEntry> = parse_jsonl(
            "{\"state\":\"working\"}\nnot json at all\n\n{\"state\":\"done\",\"text\":\"ok\"}\n",
        );
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].state, FleetState::Working);
        assert_eq!(entries[1].text.as_deref(), Some("ok"));
    }

    #[test]
    fn derives_the_team_directory_from_a_session_id() {
        assert_eq!(
            team_dir_name("e9b5670a-0f01-4df8-9ddf-547b333d2703"),
            "session-e9b5670a"
        );
        // Short ids must not panic on the take(8).
        assert_eq!(team_dir_name("abc"), "session-abc");
        assert_eq!(team_dir_name(""), "session-");
    }

    #[test]
    fn separates_teammates_from_the_lead() {
        let config = read_team_config(&fixture("team-config.json")).expect("fixture parses");
        assert_eq!(config.name, "session-e9b5670a");
        assert_eq!(config.members.len(), 2);

        let teammates: Vec<_> = config.teammates().collect();
        assert_eq!(
            teammates.len(),
            1,
            "the lead is the session, not a row under it"
        );
        assert_eq!(teammates[0].name, "scout");
        assert_eq!(teammates[0].model.as_deref(), Some("opus[1m]"));
        assert_eq!(teammates[0].color.as_deref(), Some("blue"));
    }

    #[test]
    fn a_team_of_one_has_no_teammates() {
        let config: TeamConfig = serde_json::from_str(
            r#"{"name":"session-abc","members":[{"name":"team-lead","agentType":"team-lead"}]}"#,
        )
        .expect("parses");
        assert_eq!(config.teammates().count(), 0);
    }

    #[test]
    fn recognizes_a_teammate_from_its_meta() {
        let meta = read_agent_meta(&fixture("teammate-meta.json")).expect("fixture parses");
        assert!(meta.is_teammate());
        assert_eq!(meta.name.as_deref(), Some("scout"));
        assert_eq!(meta.team_name.as_deref(), Some("session-e9b5670a"));
    }

    #[test]
    fn a_workflow_agents_meta_carries_no_label_or_phase() {
        // Pinning the gap the spec's phase attribution exists to work around:
        // if this ever starts carrying a label, the substring heuristic can go.
        let meta: AgentMeta =
            serde_json::from_str(r#"{"agentType":"workflow-subagent","spawnDepth":1}"#)
                .expect("parses");
        assert_eq!(meta.agent_type.as_deref(), Some("workflow-subagent"));
        assert!(!meta.is_teammate());
        assert_eq!(meta.name, None);
    }

    #[test]
    fn reads_tasks_in_id_order() {
        let tasks = read_tasks(&fixture("tasks")).expect("fixture parses");
        assert!(tasks.len() >= 2);

        let ids: Vec<_> = tasks.iter().map(|task| task.id.as_str()).collect();
        let mut sorted = ids.clone();
        sorted.sort_by_key(|id| id.parse::<u64>().unwrap_or(u64::MAX));
        assert_eq!(ids, sorted, "10 sorts after 2, not before it");
    }

    #[test]
    fn a_task_waiting_on_an_unfinished_task_is_not_claimable() {
        let tasks = vec![
            Task {
                id: "1".into(),
                status: "in_progress".into(),
                blocks: vec!["2".into()],
                ..Default::default()
            },
            Task {
                id: "2".into(),
                status: "pending".into(),
                blocked_by: vec!["1".into()],
                ..Default::default()
            },
            Task {
                id: "3".into(),
                status: "pending".into(),
                ..Default::default()
            },
        ];
        assert!(!tasks[1].is_claimable(&tasks), "1 has not finished");
        assert!(tasks[2].is_claimable(&tasks), "nothing blocks 3");
        assert!(
            !tasks[0].is_claimable(&tasks),
            "an in-progress task has already been claimed"
        );
    }

    #[test]
    fn a_task_blocked_on_something_completed_is_claimable() {
        let tasks = vec![
            Task {
                id: "1".into(),
                status: "completed".into(),
                ..Default::default()
            },
            Task {
                id: "2".into(),
                status: "pending".into(),
                blocked_by: vec!["1".into()],
                ..Default::default()
            },
        ];
        assert!(tasks[1].is_claimable(&tasks));
    }

    #[test]
    fn a_task_blocked_on_a_task_that_no_longer_exists_is_claimable() {
        // Otherwise a swept dependency strands the rest of the list forever.
        let tasks = vec![Task {
            id: "2".into(),
            status: "pending".into(),
            blocked_by: vec!["ghost".into()],
            ..Default::default()
        }];
        assert!(tasks[0].is_claimable(&tasks));
    }

    #[test]
    fn a_completed_task_is_never_claimable() {
        let tasks = vec![Task {
            id: "1".into(),
            status: "completed".into(),
            ..Default::default()
        }];
        assert!(!tasks[0].is_claimable(&tasks));
    }

    #[test]
    fn joins_journal_records_to_their_transcripts() {
        let entries = read_journal(&fixture("journal.jsonl")).expect("fixture parses");
        assert!(!entries.is_empty());

        let started: Vec<_> = entries.iter().filter(|entry| entry.is_started()).collect();
        let results: Vec<_> = entries.iter().filter(|entry| entry.is_result()).collect();
        assert_eq!(
            started.len(),
            results.len(),
            "every agent started and finished"
        );

        // The agent id is the join, and it is exact.
        let directory = fixture("journal.jsonl");
        let directory = directory.parent().expect("fixture has a parent");
        for entry in &started {
            assert!(!entry.agent_id.is_empty());
            let name = agent_transcript_name(&entry.agent_id);
            assert!(name.starts_with("agent-") && name.ends_with(".jsonl"));
            assert!(directory.exists());
        }
    }

    #[test]
    fn claude_home_prefers_an_explicit_config_dir() {
        // Not parallel-safe with other env-mutating tests; kept as the only one.
        let original = std::env::var_os("CLAUDE_CONFIG_DIR");
        unsafe { std::env::set_var("CLAUDE_CONFIG_DIR", "/tmp/somewhere-else") };
        assert_eq!(claude_home(), Some(PathBuf::from("/tmp/somewhere-else")));

        unsafe { std::env::set_var("CLAUDE_CONFIG_DIR", "") };
        assert!(
            claude_home().is_none_or(|path| path != Path::new("")),
            "an empty override falls back to HOME"
        );

        match original {
            Some(value) => unsafe { std::env::set_var("CLAUDE_CONFIG_DIR", value) },
            None => unsafe { std::env::remove_var("CLAUDE_CONFIG_DIR") },
        }
    }
}
