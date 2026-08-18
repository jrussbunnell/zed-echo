//! Per-subagent completion, read from the agent's own session transcript.
//!
//! Claude Code runs a subagent in the background by default: the spawning
//! `Agent` call returns as soon as the subagent starts, and the CLI tells the
//! model an individual subagent finished by feeding it a `<task-notification>`
//! message naming the spawning tool call and a status. Nothing carries that
//! notification to the client — the ACP adapter drops plain-text user messages
//! and keeps the SDK's task frames for its own bookkeeping — so from the wire
//! alone a background subagent's finish is invisible, and the only observable
//! end is the parent's whole turn settling.
//!
//! The notification is durable, though: it is written to the session's own
//! transcript, which sits next to the ones this client already reads. Tailing it
//! turns "one of them finished" into "this one finished, and how".
//!
//! Two identities make the match exact, both verified against live transcripts:
//! the session id this client holds for a Claude Code thread *is* the CLI's
//! session id (so `<claude config>/projects/*/<session id>.jsonl` is this
//! thread's transcript), and a derived subagent's session id is
//! `<parent>/subagent/<tool call id>` — the same tool call id the notification
//! names.

use agent_client_protocol::schema::v1 as acp;
use std::fs;
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::PathBuf;

/// How the agent said a background subagent ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SubagentOutcome {
    Completed,
    Failed,
    /// Stopped or killed — by the user, or by the turn it ran in being torn
    /// down.
    Canceled,
}

impl SubagentOutcome {
    fn from_status(status: &str) -> Option<Self> {
        match status.trim() {
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            "stopped" | "killed" => Some(Self::Canceled),
            _ => None,
        }
    }
}

/// Most recent transcript bytes scanned when a tail is opened.
///
/// A long session's transcript runs to megabytes, nearly all of it message
/// content this cares nothing about. The window only has to cover the
/// notifications of subagents still on screen, which arrive within a turn or
/// two of their spawn.
const INITIAL_SCAN_LIMIT: u64 = 2 * 1024 * 1024;

/// The transcript file for `session_id`, if the agent keeps one.
///
/// Found by globbing rather than by rebuilding the CLI's directory name from the
/// working directory: session ids are uuids, so the glob is unambiguous, and it
/// cannot drift the way a reimplementation of that name-mangling would. Returns
/// `None` for every agent that is not Claude Code, and for a Claude Code running
/// somewhere this process cannot see its files (a remote project).
pub(crate) fn transcript_path(session_id: &acp::SessionId) -> Option<PathBuf> {
    // A session id with a path separator is a derived subagent's, not a
    // session the agent knows about.
    if session_id.0.contains('/') {
        return None;
    }
    let projects = claude_config_dir()?.join("projects");
    let file_name = format!("{}.jsonl", session_id.0);
    let transcript = fs::read_dir(projects)
        .ok()?
        .filter_map(Result::ok)
        .map(|project| project.path().join(&file_name))
        .find(|transcript| transcript.is_file())?;
    Some(transcript)
}

fn claude_config_dir() -> Option<PathBuf> {
    if let Some(configured) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        return Some(PathBuf::from(configured));
    }
    Some(paths::home_dir().join(".claude"))
}

/// Reads a session transcript forward from wherever it was last left, handing
/// back the subagent outcomes it learned.
///
/// The transcript is append-only JSONL, so progress is a byte offset, and only
/// whole lines are consumed: a record still being written is left for the next
/// read rather than parsed in half.
pub(crate) struct TranscriptTail {
    path: PathBuf,
    offset: Option<u64>,
}

impl TranscriptTail {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self { path, offset: None }
    }

    /// Outcomes appended since the last read. Blocking: run it off the
    /// foreground thread.
    pub(crate) fn read_new(&mut self) -> Vec<(acp::ToolCallId, SubagentOutcome)> {
        let Ok(mut file) = fs::File::open(&self.path) else {
            return Vec::new();
        };
        let length = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
        let first_read = self.offset.is_none();
        let mut offset = self
            .offset
            .unwrap_or_else(|| length.saturating_sub(INITIAL_SCAN_LIMIT));
        // Truncated or replaced (a session file rewritten by the agent): start
        // over rather than reading from past the end.
        if offset > length {
            offset = 0;
        }
        if file.seek(SeekFrom::Start(offset)).is_err() {
            return Vec::new();
        }
        let mut new_text = String::new();
        // Lossy on purpose: the transcript is JSON-escaped ASCII wherever these
        // notifications appear, and a multi-byte character split across the read
        // boundary must not throw away the whole batch.
        let mut bytes = Vec::new();
        if file.read_to_end(&mut bytes).is_err() {
            return Vec::new();
        }
        new_text.push_str(&String::from_utf8_lossy(&bytes));

        // Only whole lines count as read; the rest is still being written.
        let complete = match new_text.rfind('\n') {
            Some(last_newline) => &new_text[..=last_newline],
            None => {
                // Nothing complete yet: leave the offset where it was so the
                // partial record is re-read once it lands.
                self.offset = Some(offset);
                return Vec::new();
            }
        };
        self.offset = Some(offset + complete.len() as u64);

        // A window that starts mid-file starts mid-record; that leading
        // fragment is not a record and must not be scanned as one.
        let scanned = if first_read && offset > 0 {
            complete
                .find('\n')
                .map_or("", |first| &complete[first + 1..])
        } else {
            complete
        };
        parse_notifications(scanned)
    }
}

/// Extracts the subagent outcomes from `<task-notification>` blocks in
/// transcript text.
///
/// Scanned as text rather than parsed as JSON: the notification is prose inside
/// a message, so it arrives JSON-escaped, and the tags themselves survive that
/// escaping untouched. Background tasks that are not subagents — a backgrounded
/// shell command — are notified the same way and are left out here, because the
/// caller matches every tool call id against the subagents it knows about.
pub(crate) fn parse_notifications(text: &str) -> Vec<(acp::ToolCallId, SubagentOutcome)> {
    const OPEN: &str = "<task-notification>";
    const CLOSE: &str = "</task-notification>";

    let mut outcomes = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find(OPEN) {
        let after_open = &rest[start + OPEN.len()..];
        let (body, remainder) = match after_open.find(CLOSE) {
            Some(end) => (&after_open[..end], &after_open[end + CLOSE.len()..]),
            // Unterminated: a whole record is written at once, so this is a
            // notification whose text was cut by something other than the read
            // boundary. Take what is there.
            None => (after_open, ""),
        };
        rest = remainder;

        let Some(tool_call_id) = tag_value(body, "tool-use-id") else {
            // Only the recovery notification for a previous session's agents
            // omits it, and it names task ids this client never sees.
            continue;
        };
        let Some(outcome) = tag_value(body, "status").and_then(SubagentOutcome::from_status) else {
            continue;
        };
        outcomes.push((acp::ToolCallId::new(tool_call_id.to_string()), outcome));
    }
    outcomes
}

fn tag_value<'a>(body: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = body.find(&open)? + open.len();
    let end = body[start..].find(&close)? + start;
    let value = body[start..end].trim();
    (!value.is_empty()).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shaped like the real thing, including the JSON escaping a transcript
    /// record wraps it in.
    fn notification(tool_call_id: &str, status: &str) -> String {
        format!(
            r#"{{"type":"user","message":{{"role":"user","content":"<task-notification>\n<task-id>a9be069fa906904ed</task-id>\n<tool-use-id>{tool_call_id}</tool-use-id>\n<status>{status}</status>\n<summary>Audited the page</summary>\n</task-notification>"}}}}"#
        )
    }

    #[test]
    fn reads_outcomes_out_of_escaped_transcript_records() {
        let text = format!(
            "{}\n{}\n{}\n",
            notification("toolu_one", "completed"),
            notification("toolu_two", "failed"),
            notification("toolu_three", "killed"),
        );
        assert_eq!(
            parse_notifications(&text),
            vec![
                (
                    acp::ToolCallId::new("toolu_one"),
                    SubagentOutcome::Completed
                ),
                (acp::ToolCallId::new("toolu_two"), SubagentOutcome::Failed),
                (
                    acp::ToolCallId::new("toolu_three"),
                    SubagentOutcome::Canceled
                ),
            ]
        );
    }

    #[test]
    fn ignores_notifications_it_cannot_attribute() {
        // The resume-time recovery notice names task ids but no tool call.
        let orphan = r#"{"type":"user","message":{"content":"<task-notification>\n<task-id>a7d5a75</task-id>\n<task-id>a129eab</task-id>\n<status>stopped</status>\n</task-notification>"}}"#;
        assert_eq!(parse_notifications(orphan), vec![]);

        // A status this client has no meaning for is not an ending.
        let unknown = notification("toolu_one", "in_progress");
        assert_eq!(parse_notifications(&unknown), vec![]);
    }

    #[test]
    fn tails_only_whole_records_and_never_repeats_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        fs::write(
            &path,
            format!("{}\n", notification("toolu_one", "completed")),
        )
        .unwrap();

        let mut tail = TranscriptTail::new(path.clone());
        assert_eq!(
            tail.read_new(),
            vec![(
                acp::ToolCallId::new("toolu_one"),
                SubagentOutcome::Completed
            )]
        );
        assert_eq!(tail.read_new(), vec![], "a record is only reported once");

        // A record still being written is not read until its line lands.
        let partial = notification("toolu_two", "failed");
        let (head, tail_of_line) = partial.split_at(partial.len() / 2);
        fs::write(
            &path,
            format!("{}\n{head}", notification("toolu_one", "completed")),
        )
        .unwrap();
        assert_eq!(tail.read_new(), vec![]);

        fs::write(
            &path,
            format!(
                "{}\n{head}{tail_of_line}\n",
                notification("toolu_one", "completed")
            ),
        )
        .unwrap();
        assert_eq!(
            tail.read_new(),
            vec![(acp::ToolCallId::new("toolu_two"), SubagentOutcome::Failed)]
        );
    }

    #[test]
    fn a_derived_subagent_session_has_no_transcript_of_its_own() {
        assert!(transcript_path(&acp::SessionId::new("parent/subagent/toolu_one")).is_none());
    }
}
