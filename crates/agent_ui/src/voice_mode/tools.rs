use crate::voice_dispatch::{VoiceCandidate, pick_voice_target};
use agent_client_protocol::schema::v2 as acp;
use gpui::SharedString;
use listen::ToolDefinition;
use serde_json::json;

#[derive(Clone, Debug)]
pub struct SessionSummary {
    pub session_id: acp::SessionId,
    pub title: SharedString,
    pub project: SharedString,
    pub status: SharedString,
    pub activity: Option<SharedString>,
    pub pending_approval: Option<SharedString>,
    pub is_subagent: bool,
    pub candidate: VoiceCandidate,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionMatch {
    One(acp::SessionId),
    Ambiguous(Vec<SharedString>),
    NoMatch,
}

fn words_of(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_string)
        .collect()
}

pub fn match_session(reference: &str, sessions: &[SessionSummary]) -> SessionMatch {
    let reference = reference.trim().to_lowercase();
    if reference.is_empty() {
        let candidates: Vec<VoiceCandidate> = sessions
            .iter()
            .map(|session| session.candidate.clone())
            .collect();
        return match pick_voice_target(&candidates) {
            Some(target) => SessionMatch::One(target.session_id),
            None => SessionMatch::NoMatch,
        };
    }

    if let Some(exact) = sessions
        .iter()
        .find(|session| session.title.to_lowercase() == reference)
    {
        return SessionMatch::One(exact.session_id.clone());
    }

    // Spoken references carry filler ("the booking one") that is not in titles.
    const FILLER: [&str; 6] = ["the", "one", "that", "this", "session", "thread"];
    let mut wants_subagent = false;
    let mut words: Vec<String> = Vec::new();
    for word in words_of(&reference) {
        if matches!(word.as_str(), "subagent" | "subagents") {
            wants_subagent = true;
        } else if !FILLER.contains(&word.as_str()) {
            words.push(word);
        }
    }
    if words.is_empty() && !wants_subagent {
        return SessionMatch::NoMatch;
    }
    let matches: Vec<&SessionSummary> = sessions
        .iter()
        .filter(|session| {
            if wants_subagent && !session.is_subagent {
                return false;
            }
            let title_words = words_of(&session.title);
            words.iter().all(|word| title_words.contains(word))
        })
        .collect();
    match matches.as_slice() {
        [] => SessionMatch::NoMatch,
        [only] => SessionMatch::One(only.session_id.clone()),
        several => SessionMatch::Ambiguous(
            several
                .iter()
                .map(|session| session.title.clone())
                .collect(),
        ),
    }
}

fn tool(
    name: &'static str,
    description: &'static str,
    properties: serde_json::Value,
    required: &[&str],
) -> ToolDefinition {
    ToolDefinition {
        name,
        description,
        parameters: json!({
            "type": "object",
            "properties": properties,
            "required": required,
        }),
    }
}

pub fn tool_definitions() -> Vec<ToolDefinition> {
    let session = |description: &str| json!({"type": "string", "description": description});
    let session_reference = "Which session: a title fragment such as \"the booking one\". Leave empty for the session that most recently spoke or is waiting.";
    vec![
        tool(
            "list_sessions",
            "List open sessions and subagents with their status and current activity.",
            json!({}),
            &[],
        ),
        tool(
            "peek_session",
            "Show a session's last few messages and thoughts, and what it is doing now.",
            json!({"session": session(session_reference)}),
            &["session"],
        ),
        tool(
            "tell_session",
            "Send an instruction to a session. Steers it into the running turn, or sends it as a new prompt if the session is idle.",
            json!({
                "session": session(session_reference),
                "message": {"type": "string", "description": "The instruction to give the session."},
            }),
            &["session", "message"],
        ),
        tool(
            "start_session",
            "Start a new session in the current project with the task as its first prompt.",
            json!({
                "task": {"type": "string", "description": "The first prompt for the new session."},
                "agent": {"type": "string", "description": "Which agent to use. Omit for the default."},
            }),
            &["task"],
        ),
        tool(
            "answer_approval",
            "Answer a session's pending permission prompt.",
            json!({
                "session": session(session_reference),
                "decision": {
                    "type": "string",
                    "enum": ["allow_once", "allow_always", "reject"],
                    "description": "What to answer.",
                },
            }),
            &["session", "decision"],
        ),
        tool(
            "stop_session",
            "Cancel a session's current turn.",
            json!({"session": session(session_reference)}),
            &["session"],
        ),
        tool(
            "ask_sidecar",
            "Ask a separate helper session a question about the code or about what a session did. Returns at once; the answer arrives later as a note.",
            json!({
                "question": {"type": "string", "description": "The question to ask."},
                "session": session("Optionally, the session the question is about."),
            }),
            &["question"],
        ),
        tool(
            "end_voice_mode",
            "Say goodbye and close voice mode.",
            json!({}),
            &[],
        ),
    ]
}

pub fn state_block(sessions: &[SessionSummary]) -> String {
    sessions
        .iter()
        .map(|session| {
            let mut line = format!(
                "- {} ({}): {}",
                session.title, session.project, session.status
            );
            if let Some(activity) = &session.activity {
                line.push_str(&format!("; {activity}"));
            }
            if let Some(approval) = &session.pending_approval {
                line.push_str(&format!("; needs approval: {approval}"));
            }
            line
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    AllowOnce,
    AllowAlways,
    Reject,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GateOutcome {
    Apply,
    ReadBackFirst,
    AlreadyAnswered,
}

/// A spoken "yes" is easy to mishear, and the voice model reads untrusted
/// session text, so every allow is read back and applied only after the user
/// speaks again. The read-back is bound to the exact session, prompt and
/// decision that was read, so it cannot confirm a different one.
#[derive(Default)]
pub struct ApprovalGate {
    read_back: Option<(acp::SessionId, acp::ToolCallId, Decision)>,
    user_turn_since_read_back: bool,
}

impl ApprovalGate {
    pub fn on_user_turn(&mut self) {
        self.user_turn_since_read_back = true;
    }

    pub fn request(
        &mut self,
        session_id: &acp::SessionId,
        tool_call_id: &acp::ToolCallId,
        still_pending: bool,
        decision: Decision,
    ) -> GateOutcome {
        let key = (session_id.clone(), tool_call_id.clone(), decision);
        if !still_pending {
            if self.read_back.as_ref() == Some(&key) {
                self.read_back = None;
            }
            return GateOutcome::AlreadyAnswered;
        }
        // Rejecting is the safe direction, so it never needs confirming.
        if decision == Decision::Reject {
            return GateOutcome::Apply;
        }
        if self.read_back.as_ref() == Some(&key) && self.user_turn_since_read_back {
            self.read_back = None;
            return GateOutcome::Apply;
        }
        if self.read_back.as_ref() != Some(&key) {
            self.read_back = Some(key);
            self.user_turn_since_read_back = false;
        }
        GateOutcome::ReadBackFirst
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(id: &str, title: &str) -> SessionSummary {
        SessionSummary {
            session_id: acp::SessionId::new(id),
            title: title.to_string().into(),
            project: "echo".into(),
            status: "idle".into(),
            activity: None,
            pending_approval: None,
            is_subagent: false,
            candidate: VoiceCandidate {
                session_id: acp::SessionId::new(id),
                blocked_on_approval: false,
                last_spoke_at: None,
                is_active: false,
            },
        }
    }

    #[test]
    fn an_exact_title_beats_a_partial_one() {
        let sessions = [summary("a", "Fix booking"), summary("b", "Fix booking N+1")];
        assert_eq!(
            match_session("fix booking", &sessions),
            SessionMatch::One(acp::SessionId::new("a"))
        );
    }

    #[test]
    fn a_fragment_matches_by_words() {
        let sessions = [summary("a", "Fix booking N+1"), summary("b", "Add search")];
        assert_eq!(
            match_session("booking", &sessions),
            SessionMatch::One(acp::SessionId::new("a"))
        );
    }

    #[test]
    fn an_empty_reference_uses_the_voice_target() {
        let mut blocked = summary("b", "Add search");
        blocked.candidate.blocked_on_approval = true;
        let sessions = [summary("a", "Fix booking"), blocked];
        assert_eq!(
            match_session("  ", &sessions),
            SessionMatch::One(acp::SessionId::new("b"))
        );
    }

    #[test]
    fn two_partial_matches_are_ambiguous_and_list_titles() {
        let sessions = [
            summary("a", "Fix booking N+1"),
            summary("b", "Booking emails"),
        ];
        assert_eq!(
            match_session("booking", &sessions),
            SessionMatch::Ambiguous(vec!["Fix booking N+1".into(), "Booking emails".into()])
        );
    }

    #[test]
    fn no_match_is_no_match() {
        let sessions = [summary("a", "Fix booking")];
        assert_eq!(match_session("search", &sessions), SessionMatch::NoMatch);
    }

    #[test]
    fn tool_definitions_name_all_eight_tools() {
        let names: Vec<&str> = tool_definitions().iter().map(|tool| tool.name).collect();
        assert_eq!(
            names,
            [
                "list_sessions",
                "peek_session",
                "tell_session",
                "start_session",
                "answer_approval",
                "stop_session",
                "ask_sidecar",
                "end_voice_mode"
            ]
        );
    }

    #[test]
    fn state_block_lists_one_line_per_session() {
        let mut session = summary("a", "Fix booking");
        session.activity = Some("editing".into());
        session.pending_approval = Some("run migration".into());
        assert_eq!(
            state_block(&[session]),
            "- Fix booking (echo): idle; editing; needs approval: run migration"
        );
    }

    #[test]
    fn the_booking_one_matches_the_booking_session() {
        let sessions = [summary("a", "Fix booking N+1"), summary("b", "Add search")];
        assert_eq!(
            match_session("the booking one", &sessions),
            SessionMatch::One(acp::SessionId::new("a"))
        );
    }

    #[test]
    fn the_subagent_matches_the_only_subagent() {
        let mut child = summary("b", "Research alternatives");
        child.is_subagent = true;
        let sessions = [summary("a", "Fix booking"), child];
        assert_eq!(
            match_session("the subagent", &sessions),
            SessionMatch::One(acp::SessionId::new("b"))
        );
    }

    #[test]
    fn two_subagents_are_ambiguous_for_the_subagent() {
        let mut first = summary("a", "Research alternatives");
        first.is_subagent = true;
        let mut second = summary("b", "Check docs");
        second.is_subagent = true;
        assert_eq!(
            match_session("the subagent", &[first, second]),
            SessionMatch::Ambiguous(vec!["Research alternatives".into(), "Check docs".into()])
        );
    }

    #[test]
    fn matching_is_by_whole_word() {
        let sessions = [summary("a", "Prefix cache")];
        assert_eq!(match_session("fix", &sessions), SessionMatch::NoMatch);
    }

    fn call(id: &str) -> acp::ToolCallId {
        acp::ToolCallId::new(id)
    }

    fn session(id: &str) -> acp::SessionId {
        acp::SessionId::new(id)
    }

    #[test]
    fn reject_applies_without_a_read_back() {
        let mut gate = ApprovalGate::default();
        assert_eq!(
            gate.request(&session("s"), &call("a"), true, Decision::Reject),
            GateOutcome::Apply
        );
    }

    #[test]
    fn allow_once_needs_a_user_turn_after_the_read_back() {
        let mut gate = ApprovalGate::default();
        let (s, id) = (session("s"), call("a"));
        assert_eq!(
            gate.request(&s, &id, true, Decision::AllowOnce),
            GateOutcome::ReadBackFirst
        );
        assert_eq!(
            gate.request(&s, &id, true, Decision::AllowOnce),
            GateOutcome::ReadBackFirst
        );
        gate.on_user_turn();
        assert_eq!(
            gate.request(&s, &id, true, Decision::AllowOnce),
            GateOutcome::Apply
        );
    }

    #[test]
    fn allow_once_needs_a_read_back_even_when_confirm_is_off() {
        let mut gate = ApprovalGate::default();
        assert_eq!(
            gate.request(&session("s"), &call("a"), true, Decision::AllowOnce),
            GateOutcome::ReadBackFirst
        );
    }

    #[test]
    fn allow_always_needs_a_read_back() {
        let mut gate = ApprovalGate::default();
        assert_eq!(
            gate.request(&session("s"), &call("a"), true, Decision::AllowAlways),
            GateOutcome::ReadBackFirst
        );
    }

    #[test]
    fn an_allow_once_read_back_does_not_confirm_allow_always() {
        let mut gate = ApprovalGate::default();
        let (s, id) = (session("s"), call("a"));
        gate.request(&s, &id, true, Decision::AllowOnce);
        gate.on_user_turn();
        assert_eq!(
            gate.request(&s, &id, true, Decision::AllowAlways),
            GateOutcome::ReadBackFirst
        );
        assert_eq!(
            gate.request(&s, &id, true, Decision::AllowAlways),
            GateOutcome::ReadBackFirst
        );
    }

    #[test]
    fn a_read_back_for_one_session_does_not_confirm_the_same_id_in_another() {
        let mut gate = ApprovalGate::default();
        let id = call("a");
        gate.request(&session("one"), &id, true, Decision::AllowOnce);
        gate.on_user_turn();
        assert_eq!(
            gate.request(&session("two"), &id, true, Decision::AllowOnce),
            GateOutcome::ReadBackFirst
        );
    }

    #[test]
    fn a_confirmation_for_an_answered_prompt_does_not_carry_over() {
        let mut gate = ApprovalGate::default();
        let s = session("s");
        let (a, b) = (call("a"), call("b"));
        assert_eq!(
            gate.request(&s, &a, true, Decision::AllowOnce),
            GateOutcome::ReadBackFirst
        );
        gate.on_user_turn();
        assert_eq!(
            gate.request(&s, &a, false, Decision::AllowOnce),
            GateOutcome::AlreadyAnswered
        );
        assert_eq!(
            gate.request(&s, &b, true, Decision::AllowOnce),
            GateOutcome::ReadBackFirst
        );
    }
}
