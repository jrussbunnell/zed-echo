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
    pub candidate: VoiceCandidate,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionMatch {
    One(acp::SessionId),
    Ambiguous(Vec<SharedString>),
    NoMatch,
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

    // Short words ("the", "one") are filler in speech, not identifying.
    let words: Vec<&str> = reference
        .split_whitespace()
        .filter(|word| word.chars().count() > 2)
        .collect();
    if words.is_empty() {
        return SessionMatch::NoMatch;
    }
    let matches: Vec<&SessionSummary> = sessions
        .iter()
        .filter(|session| {
            let title = session.title.to_lowercase();
            words.iter().all(|word| title.contains(word))
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

/// A spoken "yes" is easy to mishear, so an approval that grants access is
/// read back and only applied after the user speaks again.
#[derive(Default)]
pub struct ApprovalGate {
    read_back: Option<acp::ToolCallId>,
    user_turn_since_read_back: bool,
}

impl ApprovalGate {
    pub fn on_user_turn(&mut self) {
        self.user_turn_since_read_back = true;
    }

    pub fn request(
        &mut self,
        tool_call_id: &acp::ToolCallId,
        still_pending: bool,
        decision: Decision,
        confirm_approvals: bool,
    ) -> GateOutcome {
        if !still_pending {
            if self.read_back.as_ref() == Some(tool_call_id) {
                self.read_back = None;
            }
            return GateOutcome::AlreadyAnswered;
        }
        // Rejecting is the safe direction, so it never needs confirming.
        let needs_confirmation = match decision {
            Decision::Reject => false,
            Decision::AllowAlways => true,
            Decision::AllowOnce => confirm_approvals,
        };
        if !needs_confirmation {
            return GateOutcome::Apply;
        }
        if self.read_back.as_ref() == Some(tool_call_id) && self.user_turn_since_read_back {
            self.read_back = None;
            return GateOutcome::Apply;
        }
        if self.read_back.as_ref() != Some(tool_call_id) {
            self.read_back = Some(tool_call_id.clone());
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

    fn call(id: &str) -> acp::ToolCallId {
        acp::ToolCallId::new(id)
    }

    #[test]
    fn reject_applies_without_a_read_back() {
        let mut gate = ApprovalGate::default();
        assert_eq!(
            gate.request(&call("a"), true, Decision::Reject, true),
            GateOutcome::Apply
        );
    }

    #[test]
    fn allow_once_needs_a_user_turn_after_the_read_back_when_confirming() {
        let mut gate = ApprovalGate::default();
        let id = call("a");
        assert_eq!(
            gate.request(&id, true, Decision::AllowOnce, true),
            GateOutcome::ReadBackFirst
        );
        assert_eq!(
            gate.request(&id, true, Decision::AllowOnce, true),
            GateOutcome::ReadBackFirst
        );
        gate.on_user_turn();
        assert_eq!(
            gate.request(&id, true, Decision::AllowOnce, true),
            GateOutcome::Apply
        );
    }

    #[test]
    fn allow_once_applies_at_once_when_confirm_is_off() {
        let mut gate = ApprovalGate::default();
        assert_eq!(
            gate.request(&call("a"), true, Decision::AllowOnce, false),
            GateOutcome::Apply
        );
    }

    #[test]
    fn allow_always_needs_a_read_back_even_when_confirm_is_off() {
        let mut gate = ApprovalGate::default();
        assert_eq!(
            gate.request(&call("a"), true, Decision::AllowAlways, false),
            GateOutcome::ReadBackFirst
        );
    }

    #[test]
    fn a_confirmation_for_an_answered_prompt_does_not_carry_over() {
        let mut gate = ApprovalGate::default();
        let (a, b) = (call("a"), call("b"));
        assert_eq!(
            gate.request(&a, true, Decision::AllowOnce, true),
            GateOutcome::ReadBackFirst
        );
        gate.on_user_turn();
        assert_eq!(
            gate.request(&a, false, Decision::AllowOnce, true),
            GateOutcome::AlreadyAnswered
        );
        assert_eq!(
            gate.request(&b, true, Decision::AllowOnce, true),
            GateOutcome::ReadBackFirst
        );
    }
}
