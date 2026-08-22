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
                let Some(wake) = build_wake_source(&settings) else {
                    return;
                };
                let stt: Arc<dyn listen::SttProvider> =
                    Arc::new(listen::InworldStt::new(http_client, api_key));

                let wake_word = settings.wake_word.clone();
                let listener = cx.new(|cx| {
                    let mut listener = Listener::new(stt, wake, cx);
                    listener.set_wake_word(wake_word);
                    listener
                });
                let subscription = cx.subscribe(&listener, Self::handle_listen_event);

                panel.voice = Some(VoiceSession {
                    listener,
                    awaiting_confirmation: None,
                    window,
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
            // Pausing here is what lets the command be captured against a
            // silent speaker, which is the whole reason no echo canceller is
            // needed yet.
            ListenEvent::Woke => self.pause_all_narration(cx),
            ListenEvent::Abandoned => self.resume_all_narration(cx),
            ListenEvent::Failed(message) => {
                log::error!("listen: {message}");
                self.say_to_the_user("I didn't catch that.", cx);
                self.resume_all_narration(cx);
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
            VoiceCommand::Say(text) => {
                self.resume_all_narration(cx);
                self.with_window(cx, move |window, cx| {
                    thread_view.update(cx, |thread_view, cx| {
                        thread_view.send_voice_text(text, window, cx);
                    });
                });
            }
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
            VoiceCommand::Never => self.resume_all_narration(cx),
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
        self.resume_all_narration(cx);
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

    /// Every reader, not just the active thread's: the point of pausing is
    /// that the room goes quiet so a command can be heard.
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
fn build_wake_source(settings: &ListenSettings) -> Option<Arc<dyn listen::WakeSource>> {
    match listen::SpeechWake::new(settings.wake_word.clone()) {
        Ok(wake) => Some(Arc::new(wake)),
        Err(error) => {
            log::error!("listen: could not start the wake listener: {error}");
            None
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn build_wake_source(_settings: &ListenSettings) -> Option<Arc<dyn listen::WakeSource>> {
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
