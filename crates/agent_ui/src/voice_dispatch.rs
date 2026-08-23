//! Deciding which thread a spoken command is addressed to.
//!
//! The premise of speaking to Echo is that the user is not looking at the
//! screen, so "the thread you are looking at" is not an answer. What they can
//! perceive is which thread last said something to them, and which one has
//! stopped and is waiting. Those are the two signals this ranks.
//!
//! Ranking is pure over a snapshot so it can be tested without building a
//! panel; the caller assembles the snapshot from the live conversation views.

use agent_client_protocol::schema::v1 as acp;
use std::time::Instant;

/// One thread's standing at the moment a spoken command lands.
#[derive(Clone, Debug)]
pub struct VoiceCandidate {
    pub session_id: acp::SessionId,
    pub blocked_on_approval: bool,
    pub last_spoke_at: Option<Instant>,
    pub is_active: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VoiceTarget {
    pub session_id: acp::SessionId,
}

/// Decides which thread a spoken command addresses.
///
/// Blocked first: a blocked thread is the only one that cannot make progress
/// on its own, and a blocked subagent transitively blocks its parent, so
/// answering it unblocks more than one thing. Then whoever spoke most
/// recently, which is what the user was replying to. The active thread is a
/// last resort rather than the default.
pub fn pick_voice_target(candidates: &[VoiceCandidate]) -> Option<VoiceTarget> {
    let blocked = candidates
        .iter()
        .filter(|candidate| candidate.blocked_on_approval)
        .max_by_key(|candidate| candidate.last_spoke_at);
    let spoke = candidates
        .iter()
        .filter(|candidate| candidate.last_spoke_at.is_some())
        .max_by_key(|candidate| candidate.last_spoke_at);
    let active = candidates.iter().find(|candidate| candidate.is_active);

    blocked.or(spoke).or(active).map(|candidate| VoiceTarget {
        session_id: candidate.session_id.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn candidate(id: &str) -> VoiceCandidate {
        VoiceCandidate {
            session_id: acp::SessionId::new(id),
            blocked_on_approval: false,
            last_spoke_at: None,
            is_active: false,
        }
    }

    /// A blocked thread outranks a talking one: it is the only one that cannot
    /// make progress, and a blocked subagent transitively blocks its parent.
    #[test]
    fn a_thread_blocked_on_approval_wins_over_one_that_is_speaking() {
        let now = Instant::now();
        let mut talking = candidate("talking");
        talking.last_spoke_at = Some(now);
        let mut blocked = candidate("blocked");
        blocked.blocked_on_approval = true;

        let target = pick_voice_target(&[talking, blocked]).unwrap();
        assert_eq!(target.session_id, acp::SessionId::new("blocked"));
    }

    /// Two blocked threads is the parallel-subagent case: the one that spoke
    /// most recently is the one whose question the user just heard.
    #[test]
    fn the_more_recently_heard_of_two_blocked_threads_wins() {
        let now = Instant::now();
        let mut older = candidate("older");
        older.blocked_on_approval = true;
        older.last_spoke_at = Some(now - Duration::from_secs(30));
        let mut newer = candidate("newer");
        newer.blocked_on_approval = true;
        newer.last_spoke_at = Some(now);

        let target = pick_voice_target(&[older, newer]).unwrap();
        assert_eq!(target.session_id, acp::SessionId::new("newer"));
    }

    #[test]
    fn the_most_recent_speaker_wins_when_nothing_is_blocked() {
        let now = Instant::now();
        let mut older = candidate("older");
        older.last_spoke_at = Some(now - Duration::from_secs(30));
        let mut newer = candidate("newer");
        newer.last_spoke_at = Some(now);

        let target = pick_voice_target(&[older, newer]).unwrap();
        assert_eq!(target.session_id, acp::SessionId::new("newer"));
    }

    /// Nothing has spoken yet, so there is nothing the user could have been
    /// replying to and the thread on screen is the best guess left.
    #[test]
    fn the_active_thread_is_the_fallback_when_nothing_has_spoken() {
        let mut active = candidate("active");
        active.is_active = true;

        let target = pick_voice_target(&[candidate("other"), active]).unwrap();
        assert_eq!(target.session_id, acp::SessionId::new("active"));
    }

    #[test]
    fn no_candidates_means_no_target() {
        assert!(pick_voice_target(&[]).is_none());
    }
}

use crate::agent_panel::AgentPanel;
use acp_thread::AcpThread;
use gpui::{
    Action as _, AnyWindowHandle, App, AppContext as _, Context, Entity, SharedString,
    Subscription, Window,
};
use listen::{ListenEvent, ListenSettings, Listener, VoiceCommand};
use settings::Settings as _;
use std::sync::Arc;
use std::time::Duration;
use util::ResultExt as _;

/// How long the listener stays open for a confirmation without the wake word.
///
/// The user has just been asked a question, so requiring them to address Echo
/// again to answer it would be asking them to say its name twice in one
/// exchange.
const CONFIRMATION_WINDOW: Duration = Duration::from_secs(8);

/// The wake listener, plus what it is waiting to hear back.
pub struct VoiceSession {
    listener: Entity<Listener>,
    /// Set while an approval has been read back and not yet answered.
    awaiting_confirmation: Option<acp::SessionId>,
    /// Voice arrives outside any window's event loop, but sending and
    /// authorizing both need one, so the panel's own window is kept here.
    window: AnyWindowHandle,
    /// Question and answer pairs from this window, so a follow-up like "and
    /// the tests?" means something.
    exchanges: Vec<(String, String)>,
    /// The session that answers questions, made on the first one that needs
    /// it and kept warm after.
    sidecar: Option<Sidecar>,
    _subscription: Subscription,
}

impl AgentPanel {
    /// Starts the wake listener when `listen.enabled`, and leaves it off
    /// otherwise. Called once at panel construction.
    ///
    /// The API key lives in the keychain, so this finishes asynchronously; the
    /// microphone is not opened until there is something to transcribe with.
    pub(crate) fn start_listening(&mut self, window: &Window, cx: &mut Context<Self>) {
        let settings = ListenSettings::get_global(cx).clone();
        if !settings.enabled {
            return;
        }

        let provider = match settings.resolve_provider() {
            Ok(provider) => provider,
            Err(requested) => {
                log::error!(
                    "listen: `listen.provider` is set to {requested:?}, which this build cannot \
                     hear through; listening is off"
                );
                return;
            }
        };
        if provider != listen::INWORLD_STT_PROVIDER {
            log::error!("listen: {provider} has no implementation to hear through");
            return;
        }

        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let http_client = workspace.read(cx).client().http_client();
        let window = window.window_handle();
        // Pulled here, on the main thread, because the wake listener runs on a
        // thread with no context to reach the global from — and it has to be
        // *this* canceller, the one the output mixer already feeds.
        let echo_canceller = audio::Audio::echo_canceller(cx);

        cx.spawn(async move |this, cx| {
            let api_key = cx.update(|cx| read_aloud::resolve_api_key(cx)).await;
            let api_key = match api_key {
                Ok(api_key) => api_key,
                Err(error) => {
                    log::error!("listen: no Inworld API key, so listening is off: {error}");
                    return;
                }
            };

            this.update(cx, |panel, cx| {
                let Some(wake) = build_wake_source(&settings, echo_canceller) else {
                    return;
                };
                let stt: Arc<dyn listen::SttProvider> =
                    Arc::new(listen::InworldStt::new(http_client, api_key));

                let wake_word = settings.wake_word.clone();
                let conversation = settings.conversation;
                let conversation_window = settings.conversation_window;
                let listener = cx.new(|cx| {
                    let mut listener = Listener::new(stt, wake, cx);
                    listener.set_wake_word(wake_word);
                    listener.set_conversation(conversation, conversation_window);
                    listener
                });
                let subscription = cx.subscribe(&listener, Self::handle_listen_event);

                panel.voice = Some(VoiceSession {
                    listener,
                    awaiting_confirmation: None,
                    window,
                    exchanges: Vec::new(),
                    sidecar: None,
                    _subscription: subscription,
                });
            })
            .log_err();
        })
        .detach();
    }

    fn handle_listen_event(
        &mut self,
        _listener: Entity<Listener>,
        event: &ListenEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            // Ducked rather than paused: the canceller removes Echo's own
            // voice from what the microphone hears, so narration can keep
            // going underneath somebody who is talking.
            ListenEvent::Woke | ListenEvent::Speaking => self.duck_all_narration(cx),
            ListenEvent::Abandoned => self.unduck_all_narration(cx),
            // A closed window starts clean: the next conversation should not
            // inherit half of the last one.
            ListenEvent::WindowClosed => {
                self.unduck_all_narration(cx);
                if let Some(voice) = self.voice.as_mut() {
                    voice.exchanges.clear();
                }
            }
            ListenEvent::Failed(message) => {
                log::error!("listen: {message}");
                self.say_to_the_user("I didn't catch that.", cx);
                self.unduck_all_narration(cx);
            }
            ListenEvent::Command(command) => self.dispatch_voice_command(command.clone(), cx),
        }
    }
}

impl AgentPanel {
    /// Sends a spoken command to whichever thread it was addressed to.
    fn dispatch_voice_command(&mut self, command: VoiceCommand, cx: &mut Context<Self>) {
        let awaiting = self
            .voice
            .as_mut()
            .and_then(|voice| voice.awaiting_confirmation.take());

        // A read-back approval is answered before anything else: the listener
        // was re-armed specifically for this reply, and treating a bare "yes"
        // as a message to the agent would send it the word "yes".
        if let Some(session_id) = awaiting {
            match answer_to_confirmation(&command) {
                ConfirmationAnswer::Granted => {
                    self.authorize_by_voice(&session_id, acp::PermissionOptionKind::AllowOnce, cx);
                }
                ConfirmationAnswer::Withheld => {
                    self.say_to_the_user("Left it alone.", cx);
                    self.resume_all_narration(cx);
                }
            }
            return;
        }

        let Some(target) = self.resolve_voice_target(cx) else {
            self.say_to_the_user("There is no thread to say that to.", cx);
            return;
        };
        let Some(thread_view) = self.thread_view_for_session(&target.session_id, cx) else {
            return;
        };

        match command {
            VoiceCommand::Say(text) => self.route_utterance(text, thread_view, cx),
            VoiceCommand::Approve => self.begin_approval(&target.session_id, cx),
            VoiceCommand::Deny => {
                // Never confirmed. Denial is the safe direction, and a
                // spurious one costs a single re-request.
                self.authorize_by_voice(
                    &target.session_id,
                    acp::PermissionOptionKind::RejectOnce,
                    cx,
                );
            }
            VoiceCommand::Interrupt => {
                thread_view.update(cx, |thread_view, cx| thread_view.cancel_generation(cx));
                self.say_to_the_user("Stopped.", cx);
            }
            VoiceCommand::Pause => self.pause_all_narration(cx),
            VoiceCommand::Resume => self.resume_all_narration(cx),
            VoiceCommand::Repeat => self.with_narration(&thread_view, cx, |read_aloud, cx| {
                read_aloud.previous_sentence(cx);
            }),
            VoiceCommand::Next => self.with_narration(&thread_view, cx, |read_aloud, cx| {
                read_aloud.next_sentence(cx);
            }),
            VoiceCommand::Previous => self.with_narration(&thread_view, cx, |read_aloud, cx| {
                read_aloud.previous_sentence(cx);
            }),
            VoiceCommand::CatchUp => {
                self.with_window(cx, move |window, cx| {
                    window.dispatch_action(read_aloud::SummarizeSession.boxed_clone(), cx);
                });
            }
            VoiceCommand::Never => self.unduck_all_narration(cx),
        }
    }

    /// Reads the blocked tool call back and waits for a second yes.
    ///
    /// An approval is the one command that cannot be taken back by saying
    /// something else afterwards, so it is the one command that is confirmed.
    fn begin_approval(&mut self, session_id: &acp::SessionId, cx: &mut Context<Self>) {
        let Some(thread_view) = self.thread_view_for_session(session_id, cx) else {
            return;
        };
        let Some(description) = thread_view.read(cx).pending_tool_call_description(cx) else {
            self.say_to_the_user("Nothing is waiting for approval.", cx);
            return;
        };

        if !ListenSettings::get_global(cx).confirm_approvals {
            self.authorize_by_voice(session_id, acp::PermissionOptionKind::AllowOnce, cx);
            return;
        }

        self.say_to_the_user(format!("{description}. Approve?"), cx);
        if let Some(voice) = self.voice.as_mut() {
            voice.awaiting_confirmation = Some(session_id.clone());
            voice.listener.update(cx, |listener, cx| {
                listener.arm_without_wake(CONFIRMATION_WINDOW, cx)
            });
        }
    }

    fn authorize_by_voice(
        &mut self,
        session_id: &acp::SessionId,
        kind: acp::PermissionOptionKind,
        cx: &mut Context<Self>,
    ) {
        let Some(thread_view) = self.thread_view_for_session(session_id, cx) else {
            return;
        };
        let granted = matches!(
            kind,
            acp::PermissionOptionKind::AllowOnce | acp::PermissionOptionKind::AllowAlways
        );
        self.with_window(cx, move |window, cx| {
            thread_view.update(cx, |thread_view, cx| {
                thread_view.authorize_pending_tool_call(kind, window, cx);
            });
        });
        self.say_to_the_user(if granted { "Approved." } else { "Denied." }, cx);
        self.unduck_all_narration(cx);
    }
}

impl AgentPanel {
    fn resolve_voice_target(&self, cx: &App) -> Option<VoiceTarget> {
        let active = self
            .active_conversation_view()
            .and_then(|view| view.read(cx).active_thread().cloned());
        let active_id = active.map(|view| view.entity_id());

        let candidates: Vec<VoiceCandidate> = self
            .conversation_views()
            .iter()
            .flat_map(|view| view.read(cx).thread_views())
            .map(|thread_view| {
                let is_active = active_id == Some(thread_view.entity_id());
                thread_view.read(cx).voice_candidate(is_active, cx)
            })
            .collect();

        pick_voice_target(&candidates)
    }

    fn thread_view_for_session(
        &self,
        session_id: &acp::SessionId,
        cx: &App,
    ) -> Option<Entity<crate::conversation_view::ThreadView>> {
        self.conversation_views()
            .iter()
            .find_map(|view| view.read(cx).thread_view(session_id))
    }

    /// Every reader, not just the active thread's: somebody talking is talking
    /// over all of them.
    fn duck_all_narration(&self, cx: &mut Context<Self>) {
        self.for_each_reader(cx, |read_aloud, cx| read_aloud.duck(cx));
    }

    fn unduck_all_narration(&self, cx: &mut Context<Self>) {
        self.for_each_reader(cx, |read_aloud, cx| read_aloud.unduck(cx));
    }

    /// An explicit stop still stops. Ducking is for somebody talking over
    /// narration; pausing is for somebody who asked it to be quiet.
    fn pause_all_narration(&self, cx: &mut Context<Self>) {
        self.for_each_reader(cx, |read_aloud, cx| {
            let paused = read_aloud
                .playback_state(cx)
                .is_some_and(|state| state.paused);
            if read_aloud.is_speaking() && !paused {
                read_aloud.toggle_pause(cx);
            }
        });
    }

    fn resume_all_narration(&self, cx: &mut Context<Self>) {
        self.for_each_reader(cx, |read_aloud, cx| {
            let paused = read_aloud
                .playback_state(cx)
                .is_some_and(|state| state.paused);
            if paused {
                read_aloud.toggle_pause(cx);
            }
            read_aloud.unduck(cx);
        });
    }

    fn for_each_reader(
        &self,
        cx: &mut Context<Self>,
        mut act: impl FnMut(&mut read_aloud::ReadAloud, &mut Context<read_aloud::ReadAloud>),
    ) {
        let readers: Vec<Entity<read_aloud::ReadAloud>> = self
            .conversation_views()
            .iter()
            .flat_map(|view| view.read(cx).thread_views())
            .filter_map(|thread_view| thread_view.read(cx).read_aloud_entity().cloned())
            .collect();
        for reader in readers {
            reader.update(cx, |read_aloud, cx| act(read_aloud, cx));
        }
    }

    fn with_narration(
        &self,
        thread_view: &Entity<crate::conversation_view::ThreadView>,
        cx: &mut Context<Self>,
        act: impl FnOnce(&mut read_aloud::ReadAloud, &mut Context<read_aloud::ReadAloud>),
    ) {
        let Some(reader) = thread_view.read(cx).read_aloud_entity().cloned() else {
            return;
        };
        reader.update(cx, |read_aloud, cx| act(read_aloud, cx));
    }

    /// Speaks a line back to the user through whichever reader is available.
    ///
    /// Without this a voice command that failed would be silent, which to
    /// someone not looking at the screen is indistinguishable from one that
    /// was never heard.
    fn say_to_the_user(&self, line: impl Into<SharedString>, cx: &mut Context<Self>) {
        let line = line.into();
        let reader = self
            .conversation_views()
            .iter()
            .flat_map(|view| view.read(cx).thread_views())
            .find_map(|thread_view| thread_view.read(cx).read_aloud_entity().cloned());
        let Some(reader) = reader else {
            log::info!("listen: {line}");
            return;
        };
        reader.update(cx, |read_aloud, cx| read_aloud.announce(line, cx));
    }

    /// Voice arrives outside any window's event loop, but the send and
    /// authorize paths both need one.
    fn with_window(
        &self,
        cx: &mut Context<Self>,
        act: impl FnOnce(&mut Window, &mut App) + 'static,
    ) {
        let Some(window) = self.voice.as_ref().map(|voice| voice.window) else {
            return;
        };
        window.update(cx, |_, window, cx| act(window, cx)).log_err();
    }
}

#[cfg(target_os = "macos")]
fn build_wake_source(
    settings: &ListenSettings,
    echo_canceller: audio::EchoCanceller,
) -> Option<Arc<dyn listen::WakeSource>> {
    match listen::SpeechWake::new(settings.wake_word.clone(), echo_canceller) {
        Ok(wake) => Some(Arc::new(wake)),
        Err(error) => {
            log::error!("listen: could not start the wake listener: {error}");
            None
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn build_wake_source(
    _settings: &ListenSettings,
    _echo_canceller: audio::EchoCanceller,
) -> Option<Arc<dyn listen::WakeSource>> {
    log::warn!("listen: a wake listener is only implemented on macOS; listening is off");
    None
}

/// What a spoken reply to "Approve?" amounts to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfirmationAnswer {
    Granted,
    Withheld,
}

/// Reads a reply to a read-back approval.
///
/// Only an affirmative grants. Everything else — a denial, a narration verb, a
/// fresh instruction, a misheard fragment — withholds, because the cost of
/// wrongly withholding is one repeated request and the cost of wrongly
/// granting is whatever the agent was about to do.
pub fn answer_to_confirmation(command: &VoiceCommand) -> ConfirmationAnswer {
    match command {
        VoiceCommand::Approve => ConfirmationAnswer::Granted,
        _ => ConfirmationAnswer::Withheld,
    }
}

#[cfg(test)]
mod confirmation_tests {
    use super::*;

    #[test]
    fn only_an_affirmative_grants_a_read_back_approval() {
        assert_eq!(
            answer_to_confirmation(&VoiceCommand::Approve),
            ConfirmationAnswer::Granted
        );
    }

    /// The safety property the read-back exists for. Anything that is not a
    /// plain yes leaves the tool call unauthorized, including commands that
    /// are perfectly valid on their own.
    #[test]
    fn everything_other_than_an_affirmative_withholds() {
        for command in [
            VoiceCommand::Deny,
            VoiceCommand::Never,
            VoiceCommand::Pause,
            VoiceCommand::Resume,
            VoiceCommand::CatchUp,
            VoiceCommand::Interrupt,
            VoiceCommand::Next,
            VoiceCommand::Previous,
            VoiceCommand::Repeat,
            VoiceCommand::Say("actually run the tests instead".to_string()),
        ] {
            assert_eq!(
                answer_to_confirmation(&command),
                ConfirmationAnswer::Withheld,
                "{command:?}"
            );
        }
    }
}

/// Where an utterance that the intent table did not claim should go.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
    /// Echo answers it, and the text is the answer.
    Answer(String),
    /// A question needing tools, sent to the sidecar session.
    Sidecar(String),
    /// An instruction that changes the work, sent to the working session.
    Agent(String),
}

/// The reply shape asked of the router.
///
/// One line, not JSON: a small fast model asked for one spoken sentence should
/// not also be spending tokens and latency on braces. A reply that does not
/// parse is a failure rather than a guess, because guessing here means sending
/// a question to the session doing the work — the exact thing this routing
/// exists to avoid.
pub fn parse_route(reply: &str) -> Option<Route> {
    let reply = reply.trim();
    let (destination, text) = reply.split_once(':')?;
    let text = text.trim().to_string();
    if text.is_empty() {
        return None;
    }
    match destination.trim().to_ascii_uppercase().as_str() {
        "ANSWER" => Some(Route::Answer(text)),
        "SIDECAR" => Some(Route::Sidecar(text)),
        "AGENT" => Some(Route::Agent(text)),
        _ => None,
    }
}

/// What the router is asked.
///
/// The state block is small on purpose: everything in it is already in memory,
/// and a router that has to be told the whole transcript is no longer the fast
/// path it exists to be.
pub fn router_prompt(utterance: &str, state: &str, exchanges: &[(String, String)]) -> String {
    let mut history = String::new();
    for (question, answer) in exchanges {
        history.push_str(&format!("Q: {question}\nA: {answer}\n"));
    }

    format!(
        "You route what somebody watching a coding agent just said out loud. \
         Your reply is spoken by a text-to-speech voice.\n\n\
         Reply with exactly one line, starting with one of:\n\
         ANSWER: <the answer, one or two spoken sentences>\n\
         SIDECAR: <the question, rephrased for a second agent that can read the code>\n\
         AGENT: <the instruction, as they said it>\n\n\
         Rules:\n\
         - ANSWER when the state below already answers it. Prefer this; it is instant.\n\
         - SIDECAR for anything needing the code, the files, or what was said earlier \
         in the session.\n\
         - AGENT only when they are telling the agent to do or change something.\n\
         - Never guess. If it needs looking at, that is SIDECAR, not ANSWER.\n\
         - Spoken English. No markdown, no code, no file paths read out in full.\n\n\
         Current state:\n{state}\n\n\
         {history}\
         They said: {utterance}"
    )
}

#[cfg(test)]
mod route_tests {
    use super::*;

    #[test]
    fn each_destination_parses() {
        assert_eq!(
            parse_route("ANSWER: It is running the tests."),
            Some(Route::Answer("It is running the tests.".to_string()))
        );
        assert_eq!(
            parse_route("SIDECAR: what does listener.rs do"),
            Some(Route::Sidecar("what does listener.rs do".to_string()))
        );
        assert_eq!(
            parse_route("AGENT: check the tests first"),
            Some(Route::Agent("check the tests first".to_string()))
        );
    }

    #[test]
    fn the_destination_is_matched_whatever_its_casing_or_spacing() {
        assert_eq!(
            parse_route("  answer :  fine  "),
            Some(Route::Answer("fine".to_string()))
        );
    }

    /// Guessing here means sending a question into the session doing the work,
    /// which is the exact disruption the routing exists to prevent.
    #[test]
    fn an_unparsable_reply_is_no_route_rather_than_a_guess() {
        assert_eq!(parse_route("It is running the tests."), None);
        assert_eq!(parse_route(""), None);
        assert_eq!(parse_route("MAYBE: something"), None);
        assert_eq!(parse_route("ANSWER:"), None);
        assert_eq!(parse_route("ANSWER:    "), None);
    }

    /// A colon inside the answer must not truncate it.
    #[test]
    fn only_the_first_colon_separates() {
        assert_eq!(
            parse_route("ANSWER: it failed: exit code 1"),
            Some(Route::Answer("it failed: exit code 1".to_string()))
        );
    }

    #[test]
    fn the_prompt_carries_the_state_the_history_and_the_utterance() {
        let prompt = router_prompt(
            "what is it doing",
            "running: cargo test",
            &[("earlier".to_string(), "an answer".to_string())],
        );
        assert!(prompt.contains("running: cargo test"));
        assert!(prompt.contains("Q: earlier"));
        assert!(prompt.contains("A: an answer"));
        assert!(prompt.contains("They said: what is it doing"));
    }
}

/// A second agent session, so a question does not cost the session doing the
/// work.
///
/// The whole point is isolation: it never enters the thread list, its answers
/// are spoken and dropped, and the working session never sees that it exists.
pub struct Sidecar {
    thread: Entity<AcpThread>,
    /// Set once the opening instructions have been delivered, so they are not
    /// repeated on every question.
    briefed: bool,
}

/// What the sidecar is told before its first question.
///
/// Read-only is asked for, not enforced — enforcing it would need a permission
/// policy this does not build. The mitigations are that it holds no working
/// context to damage and that its tool calls still surface for approval.
pub fn sidecar_briefing(transcript: Option<&std::path::Path>) -> String {
    let transcript = match transcript {
        Some(path) => format!(
            "\n\nThe session you are answering about writes its transcript to {}. \
             Read it when the question is about what was already said or decided.",
            path.display()
        ),
        None => String::new(),
    };

    format!(
        "You answer questions about work another agent session is doing in this \
         project, for somebody listening rather than reading. That session is \
         mid-task; you are not it.\n\n\
         Rules:\n\
         - Do not edit, create, or delete anything. Read only.\n\
         - Answer in one or two spoken sentences. Your reply is read aloud.\n\
         - Look before answering. If you did not check, say you did not.\n\
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
        assert!(briefing.contains("Read only"));
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
        assert!(briefing.contains("Read only"));
    }
}

impl AgentPanel {
    /// Decides where something spoken should go, and sends it there.
    ///
    /// Only reached for utterances the intent table did not claim, so the
    /// commands said most often never wait on this.
    fn route_utterance(
        &mut self,
        utterance: String,
        thread_view: Entity<crate::conversation_view::ThreadView>,
        cx: &mut Context<Self>,
    ) {
        let model = thread_view.update(cx, |thread_view, cx| {
            thread_view.require_read_aloud_summary_model(cx)
        });
        let Some(model) = model else {
            // No model to route with, so the utterance goes where an
            // unrouted one has always gone.
            self.send_to_agent(utterance, thread_view, cx);
            return;
        };

        let state = self.voice_state_block(cx);
        let exchanges = self
            .voice
            .as_ref()
            .map(|voice| voice.exchanges.clone())
            .unwrap_or_default();
        let prompt = router_prompt(&utterance, &state, &exchanges);

        cx.spawn(async move |this, cx| {
            let reply = cx.update(|cx| model.complete(prompt, cx)).await;
            let route = reply.ok().as_deref().and_then(parse_route);

            this.update(cx, |panel, cx| {
                panel.unduck_all_narration(cx);
                match route {
                    Some(Route::Answer(answer)) => {
                        panel.remember_exchange(utterance, answer.clone());
                        panel.say_to_the_user(answer, cx);
                    }
                    Some(Route::Sidecar(question)) => {
                        panel.ask_sidecar(utterance, question, thread_view, cx);
                    }
                    Some(Route::Agent(instruction)) => {
                        panel.send_to_agent(instruction, thread_view, cx);
                    }
                    // A router that failed or answered unparsably sends the
                    // utterance where an unrouted one has always gone. Saying
                    // nothing would be indistinguishable from not hearing it.
                    None => panel.send_to_agent(utterance, thread_view, cx),
                }
            })
            .log_err();
        })
        .detach();
    }

    fn send_to_agent(
        &mut self,
        text: String,
        thread_view: Entity<crate::conversation_view::ThreadView>,
        cx: &mut Context<Self>,
    ) {
        self.unduck_all_narration(cx);
        self.with_window(cx, move |window, cx| {
            thread_view.update(cx, |thread_view, cx| {
                thread_view.send_voice_text(text, window, cx);
            });
        });
    }

    fn remember_exchange(&mut self, question: String, answer: String) {
        let Some(voice) = self.voice.as_mut() else {
            return;
        };
        voice.exchanges.push((question, answer));
        // Bounded by the window's own length in practice; this is the backstop
        // for somebody who talks continuously for fifteen seconds.
        if voice.exchanges.len() > MAX_REMEMBERED_EXCHANGES {
            voice.exchanges.remove(0);
        }
    }

    fn voice_state_block(&self, cx: &App) -> String {
        let mut lines = Vec::new();
        for view in self.conversation_views() {
            for thread_view in view.read(cx).thread_views() {
                let thread_view = thread_view.read(cx);
                let candidate = thread_view.voice_candidate(false, cx);
                let status = if candidate.blocked_on_approval {
                    "waiting for approval"
                } else {
                    "running"
                };
                let doing = thread_view
                    .pending_tool_call_description(cx)
                    .unwrap_or_else(|| "no tool call in flight".to_string());
                lines.push(format!("- a thread is {status}: {doing}"));
            }
        }
        if lines.is_empty() {
            return "Nothing is running.".to_string();
        }
        lines.join("\n")
    }
}

/// How many question-and-answer pairs the router is reminded of.
///
/// Enough for "and the tests?" to make sense, few enough that the fast path
/// stays fast.
const MAX_REMEMBERED_EXCHANGES: usize = 4;

impl AgentPanel {
    /// Asks the question somewhere that is not the session doing the work.
    fn ask_sidecar(
        &mut self,
        utterance: String,
        question: String,
        thread_view: Entity<crate::conversation_view::ThreadView>,
        cx: &mut Context<Self>,
    ) {
        if !ListenSettings::get_global(cx).sidecar {
            self.send_to_agent(utterance, thread_view, cx);
            return;
        }

        // Said before the wait rather than after it, so several seconds of
        // silence reads as "it is looking" instead of "it did not hear me".
        self.say_to_the_user("Let me check.", cx);

        let transcript = thread_view.read(cx).voice_candidate(false, cx).session_id;
        let briefing = self.voice.as_ref().is_some_and(|voice| {
            voice
                .sidecar
                .as_ref()
                .is_some_and(|sidecar| sidecar.briefed)
        });
        let opening = if briefing {
            question
        } else {
            format!(
                "{}\n\n{question}",
                sidecar_briefing(
                    crate::subagent_notifications::transcript_path(&transcript).as_deref()
                )
            )
        };

        let Some(thread) = self.ensure_sidecar(cx) else {
            log::error!("listen: no sidecar session, so the question goes to the agent");
            self.send_to_agent(utterance, thread_view, cx);
            return;
        };

        cx.spawn(async move |this, cx| {
            let answered = thread
                .update(cx, |thread, cx| {
                    thread.send(
                        vec![acp::ContentBlock::Text(acp::TextContent::new(opening))],
                        cx,
                    )
                })
                .await
                .is_ok();

            this.update(cx, |panel, cx| {
                if let Some(voice) = panel.voice.as_mut()
                    && let Some(sidecar) = voice.sidecar.as_mut()
                {
                    sidecar.briefed = true;
                }
                if !answered {
                    panel.say_to_the_user("I couldn't check that.", cx);
                    return;
                }
                let Some(answer) = panel.last_sidecar_answer(cx) else {
                    panel.say_to_the_user("I couldn't check that.", cx);
                    return;
                };
                panel.remember_exchange(utterance, answer.clone());
                panel.say_to_the_user(answer, cx);
            })
            .log_err();
        })
        .detach();
    }

    /// The sidecar's session, made once and reused.
    fn ensure_sidecar(&mut self, cx: &mut Context<Self>) -> Option<Entity<AcpThread>> {
        if let Some(voice) = self.voice.as_ref()
            && let Some(sidecar) = voice.sidecar.as_ref()
        {
            return Some(sidecar.thread.clone());
        }
        // Creating one needs a connection and a turn of the executor, so the
        // first question is answered by the agent while it warms up. Every
        // question after this one has a session waiting.
        self.spawn_sidecar(cx);
        None
    }

    fn spawn_sidecar(&mut self, cx: &mut Context<Self>) {
        let Some(view) = self.active_conversation_view().cloned() else {
            return;
        };
        let Some(connection) = view.read(cx).connection() else {
            return;
        };
        let project = self.project.clone();

        cx.spawn(async move |this, cx| {
            let created = cx
                .update(|cx| {
                    connection.new_session(project, util::path_list::PathList::default(), cx)
                })
                .await;
            let Ok(thread) = created else {
                log::error!("listen: could not start a sidecar session");
                return;
            };
            this.update(cx, |panel, _| {
                if let Some(voice) = panel.voice.as_mut() {
                    voice.sidecar = Some(Sidecar {
                        thread,
                        briefed: false,
                    });
                }
            })
            .log_err();
        })
        .detach();
    }

    /// The sidecar's most recent assistant message, which is its answer.
    fn last_sidecar_answer(&self, cx: &App) -> Option<String> {
        let thread = self.voice.as_ref()?.sidecar.as_ref()?.thread.read(cx);
        thread.entries().iter().rev().find_map(|entry| {
            let acp_thread::AgentThreadEntry::AssistantMessage(message) = entry else {
                return None;
            };
            let text = message.to_markdown(cx);
            (!text.trim().is_empty()).then(|| text.trim().to_string())
        })
    }
}
