//! A dynamic workflow run, reassembled from the wire and the run directory.
//!
//! Neither source is sufficient alone. The ACP tool call carries the run's
//! identity and its whole script — including `meta.phases` and every
//! `agent(…, { label, phase })` call site — but no progress. The run directory
//! carries per-agent transcripts and a journal, but the journal is a resume
//! cache: `{"type":"started","key":"v2:<sha>","agentId":"…"}` and nothing else.
//! No phase, no label, no tokens, no timing.
//!
//! So phases and labels come from the script, progress comes from disk, and the
//! two are joined by [`attribute_phase`], which is a heuristic with a named
//! failure mode rather than a guess.

use gpui::SharedString;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::claude_home;

/// A phase as the script declared it, whether or not any agent reached it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Phase {
    pub title: SharedString,
    pub detail: Option<SharedString>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentStatus {
    Running,
    Done,
    Failed,
}

/// One agent in a run.
#[derive(Clone, Debug)]
pub struct WorkflowAgent {
    pub agent_id: String,
    /// `None` renders under *Unattributed* — never under a guessed phase.
    pub phase: Option<SharedString>,
    pub label: Option<SharedString>,
    pub status: AgentStatus,
    pub prompt: SharedString,
    pub result: Option<SharedString>,
    pub tokens: u64,
    pub duration: Option<Duration>,
    pub transcript: PathBuf,
}

#[derive(Clone, Debug)]
pub struct WorkflowRun {
    pub run_id: String,
    pub name: SharedString,
    pub summary: SharedString,
    pub transcript_dir: PathBuf,
    /// The script as it arrived on the wire. The attribution source.
    pub script: SharedString,
    pub declared_phases: Vec<Phase>,
    pub agents: Vec<WorkflowAgent>,
}

impl WorkflowRun {
    pub fn total_tokens(&self) -> u64 {
        self.agents.iter().map(|agent| agent.tokens).sum()
    }

    pub fn is_running(&self) -> bool {
        self.agents
            .iter()
            .any(|agent| agent.status == AgentStatus::Running)
    }

    /// Agents grouped for display: declared phases in script order, then any
    /// phase only the agents mention, then `None` for the unattributed.
    ///
    /// Declared phases are listed even when empty, so the shape of a run is
    /// visible from its first frame rather than growing in as agents land.
    pub fn by_phase(&self) -> Vec<(Option<SharedString>, Vec<&WorkflowAgent>)> {
        let mut groups: Vec<(Option<SharedString>, Vec<&WorkflowAgent>)> = self
            .declared_phases
            .iter()
            .map(|phase| (Some(phase.title.clone()), Vec::new()))
            .collect();

        let mut unattributed = Vec::new();
        for agent in &self.agents {
            match &agent.phase {
                Some(phase) => {
                    match groups
                        .iter_mut()
                        .find(|(title, _)| title.as_ref() == Some(phase))
                    {
                        Some((_, members)) => members.push(agent),
                        None => groups.push((Some(phase.clone()), vec![agent])),
                    }
                }
                None => unattributed.push(agent),
            }
        }
        if !unattributed.is_empty() {
            groups.push((None, unattributed));
        }
        groups
    }
}

/// The `toolResponse` Claude Code returns when a workflow launches.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct WorkflowLaunch {
    #[serde(rename = "runId")]
    pub run_id: String,
    #[serde(rename = "workflowName")]
    pub workflow_name: String,
    pub summary: String,
    #[serde(rename = "transcriptDir")]
    pub transcript_dir: Option<PathBuf>,
    #[serde(rename = "scriptPath")]
    pub script_path: Option<PathBuf>,
    #[serde(rename = "taskType")]
    pub task_type: String,
}

impl WorkflowLaunch {
    /// Whether this response actually describes a local workflow run. The
    /// `Workflow` tool is not the only thing that reports `async_launched`.
    pub fn is_workflow(&self) -> bool {
        self.task_type == "local_workflow" && !self.run_id.is_empty()
    }
}

/// Phase titles declared in the script's `meta` block, in order.
///
/// A deliberately small scan rather than a JavaScript parse: the block is a
/// pure literal by contract, and anything cleverer would be a parser to
/// maintain for one object.
pub fn parse_declared_phases(script: &str) -> Vec<Phase> {
    let Some(phases_at) = script.find("phases:") else {
        return Vec::new();
    };
    let after = &script[phases_at..];
    let Some(open) = after.find('[') else {
        return Vec::new();
    };

    // Walk to the matching bracket so a `]` inside a string cannot end it early.
    let bytes = after.as_bytes();
    let mut depth = 0usize;
    let mut quote: Option<u8> = None;
    let mut close = None;
    for (index, byte) in bytes.iter().enumerate().skip(open) {
        match (quote, byte) {
            (Some(open_quote), byte) if *byte == open_quote => {
                let escaped = index > 0 && bytes[index - 1] == b'\\';
                if !escaped {
                    quote = None;
                }
            }
            (Some(_), _) => {}
            (None, b'\'' | b'"' | b'`') => quote = Some(*byte),
            (None, b'[') => depth += 1,
            (None, b']') => {
                depth -= 1;
                if depth == 0 {
                    close = Some(index);
                    break;
                }
            }
            _ => {}
        }
    }
    let Some(close) = close else {
        return Vec::new();
    };

    let body = &after[open..=close];
    let mut phases = Vec::new();
    let mut cursor = 0usize;
    while let Some(found) = body[cursor..].find("title:") {
        let start = cursor + found + "title:".len();
        let Some(title) = first_string_literal(&body[start..]) else {
            break;
        };
        let detail_region = &body[start..];
        let detail = detail_region
            .find("detail:")
            .and_then(|at| first_string_literal(&detail_region[at + "detail:".len()..]));
        phases.push(Phase {
            title: title.into(),
            detail: detail.map(SharedString::from),
        });
        cursor = start;
    }
    phases
}

/// Read the first single, double, or backtick quoted string in `source`.
fn first_string_literal(source: &str) -> Option<String> {
    let bytes = source.as_bytes();
    let open = bytes
        .iter()
        .position(|byte| matches!(byte, b'\'' | b'"' | b'`'))?;
    let quote = bytes[open];
    let mut value = String::new();
    let mut index = open + 1;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'\\' && index + 1 < bytes.len() {
            value.push(bytes[index + 1] as char);
            index += 2;
            continue;
        }
        if byte == quote {
            return Some(value);
        }
        value.push(byte as char);
        index += 1;
    }
    None
}

/// Attribute an agent to a phase by finding its prompt in the script.
///
/// The runtime never writes down which agent belongs to which phase, and the
/// journal's key is a hash of the prompt and options, so it cannot be inverted.
/// What the script does have is the prompt literal sitting next to its
/// `phase:` option and after its `phase('…')` statement.
///
/// Returns `None` for a prompt built at runtime — a template literal over a
/// list will not appear verbatim — which is the intended failure. An agent that
/// cannot be placed is shown under *Unattributed*, never under a wrong phase.
pub fn attribute_phase(script: &str, prompt: &str) -> Option<SharedString> {
    let needle = prompt_needle(prompt)?;
    let at = script.find(&needle)?;

    // An inline `phase: '…'` option on this call wins: it is the runtime's own
    // answer, not an inference from position.
    let tail_end = script.len().min(at + needle.len() + 400);
    let tail = &script[at + needle.len()..tail_end];
    if let Some(option_at) = tail.find("phase:")
        && let Some(title) = first_string_literal(&tail[option_at + "phase:".len()..])
    {
        return Some(title.into());
    }

    // Otherwise the nearest preceding `phase('…')` statement.
    let head = &script[..at];
    let statement_at = head.rfind("phase(")?;
    first_string_literal(&head[statement_at + "phase(".len()..]).map(SharedString::from)
}

/// The label the script gave this agent, found the same way.
pub fn attribute_label(script: &str, prompt: &str) -> Option<SharedString> {
    let needle = prompt_needle(prompt)?;
    let at = script.find(&needle)?;
    let tail_end = script.len().min(at + needle.len() + 400);
    let tail = &script[at + needle.len()..tail_end];
    let option_at = tail.find("label:")?;
    first_string_literal(&tail[option_at + "label:".len()..]).map(SharedString::from)
}

/// A prefix of the prompt long enough to be unambiguous and short enough to
/// survive the script wrapping it. Prompts shorter than this are used whole.
const NEEDLE_LIMIT: usize = 120;

fn prompt_needle(prompt: &str) -> Option<String> {
    let trimmed = prompt.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut needle = String::new();
    for character in trimmed.chars() {
        if needle.len() >= NEEDLE_LIMIT {
            break;
        }
        // A newline in the prompt is escaped in the script literal, so the raw
        // text will not match past it.
        if character == '\n' || character == '\r' {
            break;
        }
        needle.push(character);
    }
    if needle.trim().is_empty() {
        None
    } else {
        Some(needle)
    }
}

/// What a single agent transcript says about itself.
#[derive(Debug, Default)]
struct TranscriptFacts {
    prompt: String,
    tokens: u64,
    first_timestamp: Option<String>,
    last_timestamp: Option<String>,
}

#[derive(Deserialize)]
struct TranscriptRecord {
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(default)]
    message: Option<TranscriptMessage>,
}

#[derive(Deserialize)]
struct TranscriptMessage {
    #[serde(default)]
    content: Option<serde_json::Value>,
    #[serde(default)]
    usage: Option<TranscriptUsage>,
}

#[derive(Deserialize)]
struct TranscriptUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
}

impl TranscriptUsage {
    fn total(&self) -> u64 {
        self.input_tokens
            + self.output_tokens
            + self.cache_creation_input_tokens
            + self.cache_read_input_tokens
    }
}

fn read_transcript_facts(path: &Path) -> TranscriptFacts {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return TranscriptFacts::default();
    };
    let records: Vec<TranscriptRecord> = claude_home::parse_jsonl(&contents);

    let mut facts = TranscriptFacts::default();
    for record in &records {
        if let Some(timestamp) = &record.timestamp {
            if facts.first_timestamp.is_none() {
                facts.first_timestamp = Some(timestamp.clone());
            }
            facts.last_timestamp = Some(timestamp.clone());
        }
        if let Some(message) = &record.message {
            if let Some(usage) = &message.usage {
                facts.tokens += usage.total();
            }
            // The run's first user record carries the prompt the script gave it.
            if record.kind == "user" && facts.prompt.is_empty() {
                facts.prompt = message
                    .content
                    .as_ref()
                    .map(flatten_content)
                    .unwrap_or_default();
            }
        }
    }
    facts
}

/// Transcript content is a bare string for a simple prompt and a block array
/// once it carries tool results.
fn flatten_content(content: &serde_json::Value) -> String {
    match content {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(|text| text.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// ISO-8601 timestamps as the CLI writes them, differenced without pulling in a
/// date library for one subtraction.
fn duration_between(start: &str, end: &str) -> Option<Duration> {
    let start = parse_epoch_millis(start)?;
    let end = parse_epoch_millis(end)?;
    end.checked_sub(start).map(Duration::from_millis)
}

fn parse_epoch_millis(timestamp: &str) -> Option<u64> {
    // "2026-08-23T04:23:10.000Z"
    let (date, rest) = timestamp.split_once('T')?;
    let mut date_parts = date.split('-');
    let year: i64 = date_parts.next()?.parse().ok()?;
    let month: i64 = date_parts.next()?.parse().ok()?;
    let day: i64 = date_parts.next()?.parse().ok()?;

    let time = rest.trim_end_matches('Z');
    let (clock, fraction) = match time.split_once('.') {
        Some((clock, fraction)) => (clock, fraction),
        None => (time, "0"),
    };
    let mut clock_parts = clock.split(':');
    let hour: u64 = clock_parts.next()?.parse().ok()?;
    let minute: u64 = clock_parts.next()?.parse().ok()?;
    let second: u64 = clock_parts.next()?.parse().ok()?;
    let millis: u64 = format!("{fraction:0<3}")[..3].parse().ok()?;

    // Days since the civil epoch (Howard Hinnant's algorithm).
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146097 + day_of_era - 719468;

    let seconds = days.checked_mul(86400)? as u64 + hour * 3600 + minute * 60 + second;
    Some(seconds * 1000 + millis)
}

/// Read a run's current state from its directory, using `script` for phases.
///
/// The journal leads for status and the transcripts lead for everything else. A
/// transcript with no journal record is a running agent: the journal lags the
/// transcript, never leads it.
pub fn read_run(
    run_id: String,
    name: SharedString,
    summary: SharedString,
    transcript_dir: PathBuf,
    script: SharedString,
) -> WorkflowRun {
    let declared_phases = parse_declared_phases(&script);

    let mut started_order: Vec<String> = Vec::new();
    let mut results: BTreeMap<String, Option<serde_json::Value>> = BTreeMap::new();

    let journal_path = transcript_dir.join("journal.jsonl");
    if let Ok(entries) = claude_home::read_journal(&journal_path) {
        for entry in entries {
            if entry.is_started() && !started_order.contains(&entry.agent_id) {
                started_order.push(entry.agent_id.clone());
            }
            if entry.is_result() {
                results.insert(entry.agent_id.clone(), entry.result.clone());
            }
        }
    }

    // A transcript can exist before its journal record does.
    if let Ok(entries) = std::fs::read_dir(&transcript_dir) {
        let mut seen: Vec<String> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let Some(agent_id) = name
                .strip_prefix("agent-")
                .and_then(|rest| rest.strip_suffix(".jsonl"))
            else {
                continue;
            };
            if !started_order.iter().any(|known| known == agent_id) {
                seen.push(agent_id.to_string());
            }
        }
        seen.sort();
        started_order.extend(seen);
    }

    let agents = started_order
        .into_iter()
        .map(|agent_id| {
            let transcript = transcript_dir.join(claude_home::agent_transcript_name(&agent_id));
            let facts = read_transcript_facts(&transcript);
            let result = results.get(&agent_id);

            let status = match result {
                Some(_) => AgentStatus::Done,
                None => AgentStatus::Running,
            };
            // A journal result of `null` is an agent that died or was stopped.
            let status = match result {
                Some(None) => AgentStatus::Failed,
                _ => status,
            };

            let duration = match (&facts.first_timestamp, &facts.last_timestamp) {
                (Some(start), Some(end)) if status != AgentStatus::Running => {
                    duration_between(start, end)
                }
                _ => None,
            };

            WorkflowAgent {
                phase: attribute_phase(&script, &facts.prompt),
                label: attribute_label(&script, &facts.prompt),
                status,
                prompt: facts.prompt.clone().into(),
                result: result
                    .and_then(|result| result.as_ref())
                    .map(render_result)
                    .map(SharedString::from),
                tokens: facts.tokens,
                duration,
                transcript,
                agent_id,
            }
        })
        .collect();

    WorkflowRun {
        run_id,
        name,
        summary,
        transcript_dir,
        script,
        declared_phases,
        agents,
    }
}

/// A journal result is whatever the agent returned: a bare string when the
/// agent had no schema, an object when it did.
fn render_result(result: &serde_json::Value) -> String {
    match result {
        serde_json::Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("test_fixtures")
            .join(name)
    }

    fn script() -> SharedString {
        std::fs::read_to_string(fixture("workflow-script.js"))
            .expect("script fixture")
            .into()
    }

    fn run() -> WorkflowRun {
        read_run(
            "wf_a5c7c1f4-08b".into(),
            "alpha-beta".into(),
            "Two-phase smoke test".into(),
            fixture("run"),
            script(),
        )
    }

    #[test]
    fn reads_declared_phases_in_script_order() {
        let phases = parse_declared_phases(&script());
        assert_eq!(phases.len(), 2);
        assert_eq!(phases[0].title, "Alpha");
        assert_eq!(phases[1].title, "Beta");
        assert_eq!(
            phases[0].detail.as_deref(),
            Some("single agent replies ALPHA_DONE")
        );
    }

    #[test]
    fn a_script_without_a_meta_block_declares_no_phases() {
        assert!(parse_declared_phases("const x = await agent('hi')").is_empty());
    }

    #[test]
    fn a_bracket_inside_a_string_does_not_end_the_phase_list() {
        let script = r#"export const meta = {
  phases: [
    { title: 'Scan ]', detail: 'has a bracket' },
    { title: 'Fix' },
  ],
}"#;
        let phases = parse_declared_phases(script);
        assert_eq!(phases.len(), 2, "the literal ] must not truncate the list");
        assert_eq!(phases[0].title, "Scan ]");
        assert_eq!(phases[1].title, "Fix");
    }

    #[test]
    fn attributes_an_agent_by_its_inline_phase_option() {
        let phase = attribute_phase(
            &script(),
            "Reply with exactly the word BETA_DONE and nothing else. Do not use any tools.",
        );
        assert_eq!(phase.as_deref(), Some("Beta"));
    }

    #[test]
    fn falls_back_to_the_nearest_preceding_phase_statement() {
        let script = "phase('Recon')\nconst a = await agent('look around the repo')\n";
        assert_eq!(
            attribute_phase(script, "look around the repo").as_deref(),
            Some("Recon")
        );
    }

    #[test]
    fn a_prompt_built_at_runtime_is_unattributed_not_misfiled() {
        // The whole reason Unattributed exists: a template literal over a list
        // never appears verbatim in the script.
        let script = "phase('Audit')\nawait pipeline(files, f => agent(`Audit ${f} for auth`))";
        assert_eq!(
            attribute_phase(script, "Audit src/routes/billing.ts for auth"),
            None
        );
    }

    #[test]
    fn an_empty_prompt_is_unattributed() {
        assert_eq!(attribute_phase(&script(), ""), None);
        assert_eq!(attribute_phase(&script(), "   "), None);
    }

    #[test]
    fn reads_the_label_the_script_gave_an_agent() {
        let label = attribute_label(
            &script(),
            "Reply with exactly the word ALPHA_DONE and nothing else. Do not use any tools.",
        );
        assert_eq!(label.as_deref(), Some("alpha"));
    }

    #[test]
    fn joins_the_journal_to_transcripts_in_start_order() {
        let run = run();
        assert_eq!(run.agents.len(), 2);
        assert_eq!(run.agents[0].agent_id, "af5784b3849ea373d");
        assert_eq!(run.agents[0].result.as_deref(), Some("ALPHA_DONE"));
        assert_eq!(run.agents[1].result.as_deref(), Some("BETA_DONE"));
        assert!(
            run.agents
                .iter()
                .all(|agent| agent.status == AgentStatus::Done)
        );
        assert!(!run.is_running());
    }

    #[test]
    fn sums_tokens_across_every_assistant_record() {
        let run = run();
        // Each fixture agent has two assistant records; neither may be dropped.
        assert!(run.agents[0].tokens > 0);
        assert_eq!(
            run.total_tokens(),
            run.agents.iter().map(|a| a.tokens).sum::<u64>()
        );
        assert!(run.total_tokens() > run.agents[0].tokens);
    }

    #[test]
    fn measures_duration_from_first_to_last_record() {
        let run = run();
        let alpha = run.agents[0]
            .duration
            .expect("a finished agent has a duration");
        assert_eq!(alpha, Duration::from_millis(32_500));
    }

    #[test]
    fn groups_declared_phases_even_when_empty() {
        let empty = read_run(
            "wf_none".into(),
            "alpha-beta".into(),
            "".into(),
            fixture("does-not-exist"),
            script(),
        );
        let groups = empty.by_phase();
        assert_eq!(
            groups.len(),
            2,
            "a run's shape shows before any agent lands"
        );
        assert!(groups.iter().all(|(_, agents)| agents.is_empty()));
    }

    #[test]
    fn unattributed_agents_group_last() {
        let mut run = run();
        run.agents[1].phase = None;
        let groups = run.by_phase();
        let last = groups.last().expect("a group");
        assert!(last.0.is_none(), "Unattributed sorts after every phase");
        assert_eq!(last.1.len(), 1);
    }

    #[test]
    fn a_transcript_without_a_journal_record_is_running() {
        let directory = tempfile::tempdir().expect("temp dir");
        std::fs::copy(
            fixture("run/agent-af5784b3849ea373d.jsonl"),
            directory.path().join("agent-af5784b3849ea373d.jsonl"),
        )
        .expect("copy transcript");
        // No journal at all: the journal lags the transcript, never leads it.
        let run = read_run(
            "wf_partial".into(),
            "alpha-beta".into(),
            "".into(),
            directory.path().to_path_buf(),
            script(),
        );
        assert_eq!(run.agents.len(), 1);
        assert_eq!(run.agents[0].status, AgentStatus::Running);
        assert!(run.is_running());
        assert!(
            run.agents[0].duration.is_none(),
            "a running agent has no duration"
        );
    }

    #[test]
    fn recognizes_a_workflow_launch_and_rejects_other_tasks() {
        let launch: WorkflowLaunch = serde_json::from_str(
            r#"{"status":"async_launched","taskType":"local_workflow","runId":"wf_1",
                "workflowName":"audit","summary":"s","transcriptDir":"/tmp/a","scriptPath":"/tmp/b"}"#,
        )
        .expect("parses");
        assert!(launch.is_workflow());
        assert_eq!(launch.run_id, "wf_1");

        let background_bash: WorkflowLaunch =
            serde_json::from_str(r#"{"status":"async_launched","taskType":"bash"}"#)
                .expect("parses");
        assert!(
            !background_bash.is_workflow(),
            "not every async task is a run"
        );
    }

    #[test]
    fn parses_the_timestamps_the_cli_writes() {
        assert_eq!(
            duration_between("2026-08-23T04:23:10.000Z", "2026-08-23T04:23:42.500Z"),
            Some(Duration::from_millis(32_500))
        );
        // Across a day boundary, which the civil-days arithmetic has to survive.
        assert_eq!(
            duration_between("2026-08-23T23:59:59.000Z", "2026-08-24T00:00:01.000Z"),
            Some(Duration::from_millis(2_000))
        );
        assert_eq!(
            duration_between("nonsense", "2026-08-23T04:23:10.000Z"),
            None
        );
    }
}
