//! A second agent session, so a question does not cost the session doing the
//! work.
//!
//! The whole point is isolation: it never enters the thread list, its answers
//! are spoken and dropped, and the working session never sees that it exists.

use acp_thread::{AcpThread, AgentConnection};
use agent_client_protocol::schema::v2 as acp;
use anyhow::{Context as _, Result, anyhow};
use futures::FutureExt as _;
use futures::channel::oneshot;
use futures::future::{BoxFuture, Shared};
use gpui::{App, Context, Entity, SharedString, Task};
use project::Project;
use std::path::Path;
use std::rc::Rc;
use util::ResultExt as _;

pub struct Sidecar {
    session: Shared<Task<Result<Entity<AcpThread>, SharedString>>>,
    thread: Option<Entity<AcpThread>>,
    failed: bool,
    /// Set once the opening instructions have been delivered, so they are not
    /// repeated on every question.
    briefed: bool,
    /// The working session the last question was about, so a question about a
    /// different one is told so.
    about: Option<acp::SessionId>,
    /// Resolves when the most recent `ask` is done. A second prompt sent while
    /// a turn runs cancels that turn, so each question waits for the one
    /// before it.
    last_ask: Shared<BoxFuture<'static, ()>>,
}

impl Sidecar {
    pub fn new(
        connection: Rc<dyn AgentConnection>,
        project: Entity<Project>,
        cx: &mut Context<Self>,
    ) -> Self {
        let created = connection.new_session(project, util::path_list::PathList::default(), cx);
        let session = cx
            .spawn(async move |this, cx| {
                let created = created.await;
                this.update(cx, |sidecar, _| match &created {
                    Ok(thread) => sidecar.thread = Some(thread.clone()),
                    Err(_) => sidecar.failed = true,
                })
                .log_err();
                created.map_err(|error| {
                    log::error!("listen: could not start a sidecar session: {error:#}");
                    SharedString::from("couldn't start a session to ask")
                })
            })
            .shared();
        Self {
            session,
            thread: None,
            failed: false,
            briefed: false,
            about: None,
            last_ask: futures::future::ready(()).boxed().shared(),
        }
    }

    pub fn is_ready(&self) -> bool {
        self.thread.is_some()
    }

    pub fn has_failed(&self) -> bool {
        self.failed
    }

    /// Whether a conversation with it is under way.
    pub fn is_briefed(&self) -> bool {
        self.briefed
    }

    /// Asks the question and resolves to the spoken answer, waiting for the
    /// session if it is still starting.
    pub fn ask(
        &mut self,
        question: String,
        about: Option<acp::SessionId>,
        cx: &mut Context<Self>,
    ) -> Task<Result<String>> {
        let session = self.session.clone();
        let previous = self.last_ask.clone();
        // Dropped when this ask ends however it ends, including being
        // cancelled, which is what releases the next one.
        let (done, finished) = oneshot::channel::<()>();
        self.last_ask = async move {
            finished.await.ok();
        }
        .boxed()
        .shared();

        cx.spawn(async move |this, cx| {
            let _done = done;
            previous.await;
            let thread = session.await.map_err(|message| anyhow!(message))?;
            let prompt = this.read_with(cx, |sidecar, _| sidecar.prompt_for(&question, &about))?;
            let (turn_start, sent) = thread.update(cx, |thread, cx| {
                let turn_start = thread.entries().len();
                let sent = thread.send(
                    vec![acp::ContentBlock::Text(acp::TextContent::new(prompt))],
                    cx,
                );
                (turn_start, sent)
            });
            sent.await?;
            this.update(cx, |sidecar, _| {
                sidecar.briefed = true;
                if about.is_some() {
                    sidecar.about = about;
                }
            })?;
            thread
                .read_with(cx, |thread, cx| answer_since(thread, turn_start, cx))
                .context("no answer")
        })
    }

    fn prompt_for(&self, question: &str, about: &Option<acp::SessionId>) -> String {
        let transcript = || {
            about
                .as_ref()
                .and_then(crate::subagent_notifications::transcript_path)
        };
        let opening = if !self.briefed {
            Some(sidecar_briefing(transcript().as_deref()))
        } else if about.is_some() && *about != self.about {
            Some(rebriefing(transcript().as_deref()))
        } else {
            None
        };
        match opening {
            Some(opening) => format!("{opening}\n\n{question}"),
            None => question.to_string(),
        }
    }
}

/// The last assistant message of the turn that began at `turn_start`, which
/// is the answer. Earlier turns are not looked at: a turn that only ran tools
/// has no answer, rather than the previous question's.
///
/// Built from the message chunks rather than `AssistantMessage::to_markdown`,
/// which adds a heading and the thinking, neither of which is meant to be
/// read aloud.
fn answer_since(thread: &AcpThread, turn_start: usize, cx: &App) -> Option<String> {
    let message =
        thread
            .entries()
            .get(turn_start..)?
            .iter()
            .rev()
            .find_map(|entry| match entry {
                acp_thread::AgentThreadEntry::AssistantMessage(message) => Some(message),
                _ => None,
            })?;
    let text = message
        .chunks
        .iter()
        .filter_map(|chunk| match chunk {
            acp_thread::AssistantMessageChunk::Message { block, .. } => Some(block.to_markdown(cx)),
            acp_thread::AssistantMessageChunk::Thought { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

fn transcript_sentence(path: &Path) -> String {
    format!(
        "The session you are answering about writes its transcript to {}. \
         Read it when the question is about what was already said or decided.",
        path.display()
    )
}

/// Told when the questions move to a different working session, so what the
/// sidecar learned about the last one is not taken as true of this one.
pub fn rebriefing(transcript: Option<&Path>) -> String {
    let opening = "The question is now about a different session than before; \
                   what you learned about the last one may not apply to it.";
    match transcript {
        Some(path) => format!("{opening} {}", transcript_sentence(path)),
        None => opening.to_string(),
    }
}

/// What the sidecar is told before its first question.
///
/// It is a second pair of hands, not a lookup service: it can change things,
/// and the exchange is meant to continue rather than end at one answer.
///
/// The hazard that buys is two sessions writing one working tree. Nothing
/// here prevents that — the briefing steers around it by telling the sidecar
/// what the other session is holding and to say so rather than fight over a
/// file, and its tool calls still surface for approval before they run.
pub fn sidecar_briefing(transcript: Option<&Path>) -> String {
    let transcript = match transcript {
        Some(path) => format!("\n\n{}", transcript_sentence(path)),
        None => String::new(),
    };

    format!(
        "You work alongside another agent session in this project, for somebody \
         listening rather than reading. They are talking to you by voice while \
         that other session works. It is mid-task; you are not it.\n\n\
         You can read, search, and change things, and the conversation \
         continues — expect follow-ups, and expect to be asked to act on what \
         you just said.\n\n\
         Rules:\n\
         - Answer in one or two spoken sentences. Your reply is read aloud.\n\
         - Look before answering. If you did not check, say you did not.\n\
         - The other session is editing files right now. Before you change one, \
         say which, and prefer somewhere it is not working. If it is holding \
         the file, say so instead of fighting over it.\n\
         - No markdown, no code blocks, no paths read out in full — say \"the \
         listener\" rather than \"crates/listen/src/listener.rs\".{transcript}"
    )
}

#[cfg(test)]
mod sidecar_tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn the_briefing_says_it_is_not_the_session_doing_the_work() {
        let briefing = sidecar_briefing(None);
        assert!(briefing.contains("you are not it"));
    }

    /// It is a second pair of hands, not a lookup service.
    #[test]
    fn the_briefing_lets_it_act_and_expects_the_exchange_to_continue() {
        let briefing = sidecar_briefing(None);
        assert!(briefing.contains("change things"));
        assert!(briefing.contains("follow-ups"));
        assert!(
            !briefing.contains("Read only"),
            "read-only was lifted deliberately"
        );
    }

    /// Two sessions writing one tree is the cost of letting it act. Nothing
    /// prevents it, so the briefing has to steer around it.
    #[test]
    fn the_briefing_warns_it_off_files_the_other_session_is_holding() {
        let briefing = sidecar_briefing(None);
        assert!(briefing.contains("editing files right now"));
    }

    /// Without the path the sidecar cannot answer "what did it decide
    /// earlier?", which is half of what it exists for.
    #[test]
    fn the_briefing_carries_the_transcript_path_when_there_is_one() {
        let briefing = sidecar_briefing(Some(Path::new("/tmp/session.jsonl")));
        assert!(briefing.contains("/tmp/session.jsonl"));
    }

    #[test]
    fn a_missing_transcript_leaves_the_briefing_otherwise_intact() {
        let briefing = sidecar_briefing(None);
        assert!(!briefing.contains("transcript to"));
        assert!(briefing.contains("Answer in one or two spoken sentences"));
    }

    #[test]
    fn the_rebriefing_is_one_paragraph_carrying_the_transcript() {
        let rebriefing = rebriefing(Some(Path::new("/tmp/other.jsonl")));
        assert!(rebriefing.starts_with("The question is now about a different session"));
        assert!(rebriefing.contains("/tmp/other.jsonl"));
        assert!(!rebriefing.contains('\n'));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use acp_thread::{AgentThreadEntry, StubAgentConnection};
    use agent_client_protocol::schema::v1 as acp_v1;
    use fs::FakeFs;
    use gpui::{AppContext as _, Entity, TestAppContext};
    use project::Project;
    use std::rc::Rc;

    async fn new_sidecar(
        connection: &StubAgentConnection,
        cx: &mut TestAppContext,
    ) -> Entity<Sidecar> {
        crate::test_support::init_test(cx);
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let connection: Rc<dyn acp_thread::AgentConnection> = Rc::new(connection.clone());
        cx.new(|cx| Sidecar::new(connection, project, cx))
    }

    fn reply_with(connection: &StubAgentConnection, text: &str) {
        connection.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
            acp_v1::ContentChunk::new(text.into()),
        )]);
    }

    fn ask(
        sidecar: &Entity<Sidecar>,
        question: &str,
        about: Option<&str>,
        cx: &mut TestAppContext,
    ) -> anyhow::Result<String> {
        let answer = sidecar.update(cx, |sidecar, cx| {
            sidecar.ask(question.to_string(), about.map(acp::SessionId::new), cx)
        });
        cx.run_until_parked();
        futures::FutureExt::now_or_never(answer).expect("the question was answered")
    }

    fn prompts(sidecar: &Entity<Sidecar>, cx: &mut TestAppContext) -> Vec<String> {
        sidecar.read_with(cx, |sidecar, cx| {
            let thread = sidecar
                .thread
                .as_ref()
                .expect("the session started")
                .read(cx);
            thread
                .entries()
                .iter()
                .filter_map(|entry| match entry {
                    AgentThreadEntry::UserMessage(message) => Some(
                        message
                            .content
                            .source_blocks()
                            .iter()
                            .filter_map(|block| match block {
                                acp::ContentBlock::Text(text) => Some(text.text.clone()),
                                _ => None,
                            })
                            .collect::<String>(),
                    ),
                    _ => None,
                })
                .collect()
        })
    }

    #[gpui::test]
    async fn the_first_question_waits_for_the_session_instead_of_falling_back(
        cx: &mut TestAppContext,
    ) {
        let connection = StubAgentConnection::new();
        let sidecar = new_sidecar(&connection, cx).await;
        assert!(!sidecar.read_with(cx, |sidecar, _| sidecar.is_ready()));

        reply_with(&connection, "It is green.");
        let answer = ask(&sidecar, "are the tests passing", None, cx);

        assert_eq!(answer.expect("answered"), "It is green.");
        assert!(sidecar.read_with(cx, |sidecar, _| sidecar.is_ready()));
    }

    #[gpui::test]
    async fn the_briefing_is_sent_once(cx: &mut TestAppContext) {
        let connection = StubAgentConnection::new();
        let sidecar = new_sidecar(&connection, cx).await;

        reply_with(&connection, "one");
        ask(&sidecar, "first question", None, cx).expect("answered");
        reply_with(&connection, "two");
        ask(&sidecar, "second question", None, cx).expect("answered");

        let prompts = prompts(&sidecar, cx);
        assert_eq!(prompts.len(), 2);
        assert!(prompts[0].contains(&sidecar_briefing(None)));
        assert!(prompts[0].ends_with("first question"));
        assert_eq!(prompts[1], "second question");
    }

    #[gpui::test]
    async fn asking_about_a_different_session_rebriefs(cx: &mut TestAppContext) {
        let connection = StubAgentConnection::new();
        let sidecar = new_sidecar(&connection, cx).await;

        reply_with(&connection, "one");
        ask(&sidecar, "about a", Some("session-a"), cx).expect("answered");
        reply_with(&connection, "two");
        ask(&sidecar, "about b", Some("session-b"), cx).expect("answered");
        reply_with(&connection, "three");
        ask(&sidecar, "still b", Some("session-b"), cx).expect("answered");

        let prompts = prompts(&sidecar, cx);
        let transcript =
            crate::subagent_notifications::transcript_path(&acp::SessionId::new("session-b"));
        assert!(prompts[1].starts_with(&rebriefing(transcript.as_deref())));
        assert!(prompts[1].ends_with("about b"));
        assert_eq!(prompts[2], "still b");
    }

    #[gpui::test]
    async fn the_answer_is_the_last_assistant_message(cx: &mut TestAppContext) {
        let connection = StubAgentConnection::new();
        let sidecar = new_sidecar(&connection, cx).await;

        reply_with(&connection, "the first answer");
        ask(&sidecar, "first", None, cx).expect("answered");
        reply_with(&connection, "the second answer");
        let answer = ask(&sidecar, "second", None, cx);

        assert_eq!(answer.expect("answered"), "the second answer");
    }

    fn session_id(sidecar: &Entity<Sidecar>, cx: &mut TestAppContext) -> acp::SessionId {
        sidecar.read_with(cx, |sidecar, cx| {
            sidecar
                .thread
                .as_ref()
                .expect("the session started")
                .read(cx)
                .session_id()
                .clone()
        })
    }

    fn say(
        connection: &StubAgentConnection,
        session_id: &acp::SessionId,
        text: &str,
        cx: &mut TestAppContext,
    ) {
        cx.update(|cx| {
            connection.send_update(
                session_id.clone(),
                acp_v1::SessionUpdate::AgentMessageChunk(acp_v1::ContentChunk::new(text.into())),
                cx,
            )
        });
    }

    /// A second prompt sent while a turn runs cancels that turn, so a second
    /// question asked before the first is answered must wait its turn.
    #[gpui::test]
    async fn two_questions_in_flight_each_get_their_own_answer(cx: &mut TestAppContext) {
        let connection = StubAgentConnection::new();
        let sidecar = new_sidecar(&connection, cx).await;

        let first = sidecar.update(cx, |sidecar, cx| sidecar.ask("first".into(), None, cx));
        let second = sidecar.update(cx, |sidecar, cx| sidecar.ask("second".into(), None, cx));
        cx.run_until_parked();
        assert_eq!(prompts(&sidecar, cx).len(), 1, "the second waits");
        assert!(
            connection.take_cancel_calls().is_empty(),
            "the second question must not cut the first one's turn short"
        );

        let session_id = session_id(&sidecar, cx);
        say(&connection, &session_id, "the first answer", cx);
        connection.end_turn(session_id.clone(), acp_v1::StopReason::EndTurn);
        cx.run_until_parked();
        assert_eq!(prompts(&sidecar, cx).len(), 2);

        say(&connection, &session_id, "the second answer", cx);
        connection.end_turn(session_id, acp_v1::StopReason::EndTurn);
        cx.run_until_parked();

        let first = futures::FutureExt::now_or_never(first).expect("first answered");
        let second = futures::FutureExt::now_or_never(second).expect("second answered");
        assert_eq!(first.expect("answered"), "the first answer");
        assert_eq!(second.expect("answered"), "the second answer");
    }

    #[gpui::test]
    async fn a_turn_with_no_message_is_an_error_not_the_previous_answer(cx: &mut TestAppContext) {
        let connection = StubAgentConnection::new();
        let sidecar = new_sidecar(&connection, cx).await;

        reply_with(&connection, "the first answer");
        ask(&sidecar, "first", None, cx).expect("answered");

        let second = sidecar.update(cx, |sidecar, cx| sidecar.ask("second".into(), None, cx));
        cx.run_until_parked();
        connection.end_turn(session_id(&sidecar, cx), acp_v1::StopReason::EndTurn);
        cx.run_until_parked();

        let second = futures::FutureExt::now_or_never(second).expect("second finished");
        assert_eq!(second.expect_err("no answer").to_string(), "no answer");
    }

    #[gpui::test]
    async fn a_failed_first_send_rebriefs_next_time(cx: &mut TestAppContext) {
        let connection = StubAgentConnection::new();
        let sidecar = new_sidecar(&connection, cx).await;

        let failure = connection.defer_next_prompt_response();
        let first = sidecar.update(cx, |sidecar, cx| sidecar.ask("first".into(), None, cx));
        cx.run_until_parked();
        failure
            .send(Err(anyhow!("the agent fell over")))
            .expect("the turn is waiting");
        cx.run_until_parked();
        assert!(
            futures::FutureExt::now_or_never(first)
                .expect("first finished")
                .is_err()
        );
        assert!(!sidecar.read_with(cx, |sidecar, _| sidecar.is_briefed()));

        reply_with(&connection, "an answer");
        ask(&sidecar, "second", None, cx).expect("answered");

        let prompts = prompts(&sidecar, cx);
        assert!(
            prompts
                .last()
                .expect("sent")
                .contains(&sidecar_briefing(None))
        );
    }
}
