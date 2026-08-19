use crate::intent::{VoiceCommand, parse_intent};
use crate::provider::{SttProvider, Transcript};
use anyhow::Result;
use futures::StreamExt as _;
use futures::channel::mpsc;
use gpui::{App, Context, EventEmitter, SharedString, Task};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How long a wake may go unanswered before the listener hands narration back.
///
/// Long enough to draw breath, short enough that a false wake from Echo's own
/// voice is a pause rather than a silence.
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

/// What the on-device listener heard.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WakeSignal {
    Woke,
    /// The speaker stopped. Ends the utterance in flight.
    UtteranceEnded,
    Failed(String),
}

/// The seam the microphone sits behind, so the state machine is testable
/// without one.
pub trait WakeSource: Send + Sync + 'static {
    fn wake_events(&self, cx: &App) -> mpsc::UnboundedReceiver<WakeSignal>;
}

#[cfg(any(test, feature = "test-support"))]
#[derive(Default)]
struct FakeWakeState {
    senders: Vec<mpsc::UnboundedSender<WakeSignal>>,
}

/// Test double for the wake listener.
#[cfg(any(test, feature = "test-support"))]
#[derive(Default)]
pub struct FakeWake {
    state: Mutex<FakeWakeState>,
}

#[cfg(any(test, feature = "test-support"))]
impl FakeWake {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn wake(&self) {
        self.emit(WakeSignal::Woke);
    }

    pub fn end_utterance(&self) {
        self.emit(WakeSignal::UtteranceEnded);
    }

    pub fn fail(&self, message: &str) {
        self.emit(WakeSignal::Failed(message.to_string()));
    }

    fn emit(&self, signal: WakeSignal) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        state
            .senders
            .retain(|sender| sender.unbounded_send(signal.clone()).is_ok());
    }
}

#[cfg(any(test, feature = "test-support"))]
impl WakeSource for FakeWake {
    fn wake_events(&self, _cx: &App) -> mpsc::UnboundedReceiver<WakeSignal> {
        let (sender, receiver) = mpsc::unbounded();
        if let Ok(mut state) = self.state.lock() {
            state.senders.push(sender);
        }
        receiver
    }
}

/// Something the owning view has to act on.
#[derive(Clone, Debug, PartialEq)]
pub enum ListenEvent {
    /// The wake word landed. The owner pauses narration on this, which is what
    /// lets the command be captured against a silent speaker.
    Woke,
    Command(VoiceCommand),
    /// A wake that came to nothing. The owner resumes narration.
    Abandoned,
    Failed(SharedString),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListenerState {
    Idle,
    Capturing,
}

pub struct Listener {
    provider: Arc<dyn SttProvider>,
    state: ListenerState,
    wake_word: String,
    /// Feeds the transcription in flight. Dropping it ends the utterance,
    /// which is how a provider learns there is no more to come.
    audio: Option<mpsc::UnboundedSender<Vec<f32>>>,
    transcription: Option<Task<()>>,
    timeout: Option<Task<()>>,
    _wake: Task<()>,
}

impl EventEmitter<ListenEvent> for Listener {}

impl Listener {
    pub fn new(
        provider: Arc<dyn SttProvider>,
        wake: Arc<dyn WakeSource>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut signals = wake.wake_events(cx);
        let wake_task = cx.spawn(async move |this, cx| {
            while let Some(signal) = signals.next().await {
                if this
                    .update(cx, |this, cx| this.handle_wake(signal, cx))
                    .is_err()
                {
                    return;
                }
            }
        });

        Self {
            provider,
            state: ListenerState::Idle,
            wake_word: String::from("echo"),
            audio: None,
            transcription: None,
            timeout: None,
            _wake: wake_task,
        }
    }

    pub fn state(&self) -> ListenerState {
        self.state
    }

    pub fn set_wake_word(&mut self, wake_word: String) {
        if !wake_word.trim().is_empty() {
            self.wake_word = wake_word;
        }
    }

    /// Feeds captured audio to the transcription in flight. Frames that arrive
    /// while nothing is being captured are dropped, which is what makes the
    /// wake word a gate rather than a label on a stream that was being
    /// uploaded anyway.
    pub fn push_audio(&self, frame: Vec<f32>) {
        if let Some(audio) = &self.audio {
            audio.unbounded_send(frame).ok();
        }
    }

    /// Captures the next utterance without requiring the wake word, for the
    /// span of `window`. Used when Echo has just asked a question and the user
    /// is already mid-conversation.
    pub fn arm_without_wake(&mut self, window: Duration, cx: &mut Context<Self>) {
        if self.state == ListenerState::Capturing {
            return;
        }
        self.begin_capture(cx);
        self.arm_timeout(window, cx);
    }

    fn handle_wake(&mut self, signal: WakeSignal, cx: &mut Context<Self>) {
        match signal {
            WakeSignal::Woke => {
                if self.state == ListenerState::Capturing {
                    return;
                }
                cx.emit(ListenEvent::Woke);
                self.begin_capture(cx);
                self.arm_timeout(COMMAND_TIMEOUT, cx);
            }
            WakeSignal::UtteranceEnded => {
                // Dropping the sender is the end-of-utterance signal; the
                // transcription task finishes on its own and dispatches.
                self.audio.take();
            }
            WakeSignal::Failed(message) => {
                self.abandon_capture();
                cx.emit(ListenEvent::Failed(message.into()));
            }
        }
    }

    fn begin_capture(&mut self, cx: &mut Context<Self>) {
        let (audio_sender, audio) = mpsc::unbounded();
        let mut transcripts = self.provider.transcribe(audio, cx);
        let wake_word = self.wake_word.clone();

        self.audio = Some(audio_sender);
        self.state = ListenerState::Capturing;
        self.transcription = Some(cx.spawn(async move |this, cx| {
            let mut latest: Option<Result<Transcript>> = None;
            while let Some(transcript) = transcripts.next().await {
                let settled = transcript
                    .as_ref()
                    .map(|transcript| transcript.is_final)
                    .unwrap_or(true);
                latest = Some(transcript);
                if settled {
                    break;
                }
            }
            this.update(cx, |this, cx| this.dispatch(latest, &wake_word, cx))
                .ok();
        }));
    }

    fn dispatch(
        &mut self,
        transcript: Option<Result<Transcript>>,
        wake_word: &str,
        cx: &mut Context<Self>,
    ) {
        if self.state != ListenerState::Capturing {
            return;
        }
        // Deliberately does not drop `transcription`: this runs inside that
        // task, and a task that cancels itself mid-poll is a trap for whoever
        // adds work after the dispatch.
        self.state = ListenerState::Idle;
        self.audio.take();
        self.timeout.take();

        match transcript {
            Some(Ok(transcript)) => {
                let command = parse_intent(&transcript.text, wake_word);
                if command == VoiceCommand::Never {
                    cx.emit(ListenEvent::Abandoned);
                } else {
                    cx.emit(ListenEvent::Command(command));
                }
            }
            Some(Err(error)) => cx.emit(ListenEvent::Failed(format!("{error}").into())),
            None => cx.emit(ListenEvent::Abandoned),
        }
    }

    fn arm_timeout(&mut self, window: Duration, cx: &mut Context<Self>) {
        self.timeout = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(window).await;
            this.update(cx, |this, cx| {
                if this.state != ListenerState::Capturing {
                    return;
                }
                this.abandon_capture();
                cx.emit(ListenEvent::Abandoned);
            })
            .ok();
        }));
    }

    /// Tears down a capture without dispatching, cancelling the transcription
    /// rather than letting it land after the listener has moved on.
    fn abandon_capture(&mut self) {
        self.state = ListenerState::Idle;
        self.audio.take();
        self.transcription.take();
        self.timeout.take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::FakeStt;
    use gpui::{AppContext as _, Entity, TestAppContext};

    struct Harness {
        listener: Entity<Listener>,
        wake: Arc<FakeWake>,
        stt: FakeStt,
        events: Arc<Mutex<Vec<ListenEvent>>>,
    }

    impl Harness {
        fn events(&self) -> Vec<ListenEvent> {
            self.events
                .lock()
                .map(|events| events.clone())
                .unwrap_or_default()
        }

        fn state(&self, cx: &mut TestAppContext) -> ListenerState {
            self.listener.read_with(cx, |listener, _| listener.state())
        }
    }

    fn harness(cx: &mut TestAppContext) -> Harness {
        let wake = Arc::new(FakeWake::new());
        let stt = FakeStt::new();
        let events = Arc::new(Mutex::new(Vec::new()));
        let listener = cx.new(|cx| Listener::new(Arc::new(stt.clone()), wake.clone(), cx));
        cx.update(|cx| {
            let events = events.clone();
            cx.subscribe(&listener, move |_, event: &ListenEvent, _| {
                if let Ok(mut events) = events.lock() {
                    events.push(event.clone());
                }
            })
            .detach();
        });
        Harness {
            listener,
            wake,
            stt,
            events,
        }
    }

    #[gpui::test]
    async fn a_wake_signal_announces_itself_before_anything_is_transcribed(
        cx: &mut TestAppContext,
    ) {
        let harness = harness(cx);
        harness.wake.wake();
        cx.run_until_parked();

        assert_eq!(harness.events(), vec![ListenEvent::Woke]);
        assert_eq!(harness.state(cx), ListenerState::Capturing);
    }

    #[gpui::test]
    async fn a_finished_utterance_dispatches_the_command_it_parsed(cx: &mut TestAppContext) {
        let harness = harness(cx);
        harness.stt.queue_transcript("check the tests first");
        harness.wake.wake();
        cx.run_until_parked();
        harness.wake.end_utterance();
        cx.run_until_parked();

        assert_eq!(
            harness.events(),
            vec![
                ListenEvent::Woke,
                ListenEvent::Command(VoiceCommand::Say("check the tests first".to_string())),
            ]
        );
        assert_eq!(harness.state(cx), ListenerState::Idle);
    }

    /// A wake with nothing behind it is Echo hearing its own voice, or a
    /// passing conversation. It must hand narration back rather than leaving
    /// the reader parked forever.
    #[gpui::test]
    async fn a_wake_with_no_speech_behind_it_is_abandoned(cx: &mut TestAppContext) {
        let harness = harness(cx);
        harness.wake.wake();
        cx.run_until_parked();
        cx.background_executor.advance_clock(COMMAND_TIMEOUT * 2);
        cx.run_until_parked();

        assert_eq!(
            harness.events(),
            vec![ListenEvent::Woke, ListenEvent::Abandoned]
        );
        assert_eq!(harness.state(cx), ListenerState::Idle);
    }

    /// A failed transcription must be audible as a failure. Returning quietly
    /// to idle is indistinguishable from a listener that has stopped working.
    #[gpui::test]
    async fn a_failed_transcription_reports_rather_than_going_quiet(cx: &mut TestAppContext) {
        let harness = harness(cx);
        harness.stt.fail_next();
        harness.wake.wake();
        cx.run_until_parked();
        harness.wake.end_utterance();
        cx.run_until_parked();

        let events = harness.events();
        assert_eq!(events[0], ListenEvent::Woke);
        assert!(matches!(events[1], ListenEvent::Failed(_)), "{events:?}");
        assert_eq!(harness.state(cx), ListenerState::Idle);
    }

    /// Mid-conversation — answering "approve?" — the user should not have to
    /// say the wake word again.
    #[gpui::test]
    async fn arming_without_a_wake_word_captures_the_next_utterance(cx: &mut TestAppContext) {
        let harness = harness(cx);
        harness.stt.queue_transcript("yes");
        harness.listener.update(cx, |listener, cx| {
            listener.arm_without_wake(Duration::from_secs(8), cx)
        });
        cx.run_until_parked();
        harness.wake.end_utterance();
        cx.run_until_parked();

        assert_eq!(
            harness.events(),
            vec![ListenEvent::Command(VoiceCommand::Approve)]
        );
    }

    /// The re-arm is a window, not a latch: if the user says nothing the
    /// listener must go back to requiring the wake word rather than staying
    /// open to every word in the room.
    #[gpui::test]
    async fn an_unused_arm_window_closes_on_its_own(cx: &mut TestAppContext) {
        let harness = harness(cx);
        harness.listener.update(cx, |listener, cx| {
            listener.arm_without_wake(Duration::from_secs(8), cx)
        });
        cx.run_until_parked();
        cx.background_executor.advance_clock(Duration::from_secs(9));
        cx.run_until_parked();

        assert_eq!(harness.state(cx), ListenerState::Idle);
        assert_eq!(harness.events(), vec![ListenEvent::Abandoned]);
    }

    /// Audio captured before a wake must not reach a provider. The wake word
    /// is the gate, not a label on a stream that was being uploaded anyway.
    #[gpui::test]
    async fn frames_pushed_while_idle_reach_no_provider(cx: &mut TestAppContext) {
        let harness = harness(cx);
        harness
            .listener
            .read_with(cx, |listener, _| listener.push_audio(vec![0.1; 128]));
        cx.run_until_parked();

        assert_eq!(harness.stt.heard(), 0);
        assert_eq!(harness.events(), vec![]);
    }
}
