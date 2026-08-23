use crate::conversation::{Activity, DEFAULT_WINDOW, SILENCE_ENDS_UTTERANCE, Window, activity};
use crate::intent::{VoiceCommand, parse_intent};
use crate::provider::{SttProvider, Transcript};
use anyhow::Result;
use futures::StreamExt as _;
use futures::channel::mpsc;
use gpui::{App, Context, EventEmitter, SharedString, Task};
use std::sync::Arc;
#[cfg(any(test, feature = "test-support"))]
use std::sync::Mutex;
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
    /// The recognizer's running transcript of what it is hearing.
    ///
    /// Carried rather than kept inside the wake source because it is the only
    /// voice-activity signal available without a second framework object fed
    /// by the same audio: a transcript that grows means somebody is talking.
    PartialTranscript(String),
    /// The speaker stopped. Ends the utterance in flight.
    UtteranceEnded,
    Failed(String),
}

/// The seam the microphone sits behind, so the state machine is testable
/// without one.
pub trait WakeSource: Send + Sync + 'static {
    fn wake_events(&self, cx: &App) -> mpsc::UnboundedReceiver<WakeSignal>;

    /// Mono `f32` frames at the rate the command provider expects.
    ///
    /// The same microphone feeds both the wake listener and the command
    /// provider, so it is opened once here rather than twice by two owners
    /// that would then compete for the device.
    fn audio_frames(&self, cx: &App) -> mpsc::UnboundedReceiver<Vec<f32>>;
}

#[cfg(any(test, feature = "test-support"))]
#[derive(Default)]
struct FakeWakeState {
    senders: Vec<mpsc::UnboundedSender<WakeSignal>>,
    frames: Vec<mpsc::UnboundedSender<Vec<f32>>>,
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

    pub fn push_partial(&self, transcript: &str) {
        self.emit(WakeSignal::PartialTranscript(transcript.to_string()));
    }

    pub fn fail(&self, message: &str) {
        self.emit(WakeSignal::Failed(message.to_string()));
    }

    /// Delivers one frame of captured audio, as the real listener does
    /// continuously whether or not anything is being captured.
    pub fn push_frame(&self, frame: Vec<f32>) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        state
            .frames
            .retain(|sender| sender.unbounded_send(frame.clone()).is_ok());
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

    fn audio_frames(&self, _cx: &App) -> mpsc::UnboundedReceiver<Vec<f32>> {
        let (sender, receiver) = mpsc::unbounded();
        if let Ok(mut state) = self.state.lock() {
            state.frames.push(sender);
        }
        receiver
    }
}

/// Something the owning view has to act on.
#[derive(Clone, Debug, PartialEq)]
pub enum ListenEvent {
    /// The wake word landed.
    Woke,
    /// Somebody started talking inside an open window. The owner ducks
    /// narration on this rather than pausing it — with the canceller running,
    /// Echo's own voice is no longer in the way.
    Speaking,
    Command(VoiceCommand),
    /// A wake, or an open window, that came to nothing. The owner unducks.
    Abandoned,
    /// The window closed and the wake word is required again.
    WindowClosed,
    Failed(SharedString),
}

pub struct Listener {
    provider: Arc<dyn SttProvider>,
    window: Window,
    wake_word: String,
    /// When false the window closes after one utterance, which is the shipped
    /// walkie-talkie behavior.
    conversation: bool,
    window_duration: Duration,
    /// The last partial transcript seen, to tell growth from revision.
    last_partial: String,
    silence: Option<Task<()>>,
    window_timer: Option<Task<()>>,
    /// Feeds the transcription in flight. Dropping it ends the utterance,
    /// which is how a provider learns there is no more to come.
    audio: Option<mpsc::UnboundedSender<Vec<f32>>>,
    transcription: Option<Task<()>>,
    timeout: Option<Task<()>>,
    _wake: Task<()>,
    _audio_pump: Task<()>,
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

        let mut frames = wake.audio_frames(cx);
        let audio_task = cx.spawn(async move |this, cx| {
            while let Some(frame) = frames.next().await {
                if this.update(cx, |this, _| this.push_audio(frame)).is_err() {
                    return;
                }
            }
        });

        Self {
            provider,
            window: Window::Asleep,
            wake_word: String::from("echo"),
            conversation: false,
            window_duration: DEFAULT_WINDOW,
            last_partial: String::new(),
            silence: None,
            window_timer: None,
            audio: None,
            transcription: None,
            timeout: None,
            _wake: wake_task,
            _audio_pump: audio_task,
        }
    }

    pub fn window(&self) -> Window {
        self.window
    }

    /// Turns the walkie-talkie into a conversation: the window stays open for
    /// `window_duration` after each exchange rather than closing immediately.
    pub fn set_conversation(&mut self, conversation: bool, window_duration: Duration) {
        self.conversation = conversation;
        self.window_duration = window_duration;
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
    fn push_audio(&self, frame: Vec<f32>) {
        if let Some(audio) = &self.audio {
            audio.unbounded_send(frame).ok();
        }
    }

    /// Captures the next utterance without requiring the wake word, for the
    /// span of `window`. Used when Echo has just asked a question and the user
    /// is already mid-conversation.
    pub fn arm_without_wake(&mut self, window: Duration, cx: &mut Context<Self>) {
        if self.window == Window::Capturing {
            return;
        }
        // Opens the window rather than starting a capture: transcribing
        // against silence while the user gathers their thoughts spends a
        // request on nothing, and the utterance still begins the moment they
        // actually speak.
        self.window = Window::Listening;
        self.last_partial.clear();
        self.window_timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(window).await;
            this.update(cx, |this, cx| {
                if this.window != Window::Listening {
                    return;
                }
                this.window = Window::Asleep;
                this.last_partial.clear();
                cx.emit(ListenEvent::Abandoned);
            })
            .ok();
        }));
    }

    fn handle_wake(&mut self, signal: WakeSignal, cx: &mut Context<Self>) {
        match signal {
            WakeSignal::Woke => {
                if self.window == Window::Capturing {
                    return;
                }
                cx.emit(ListenEvent::Woke);
                self.begin_capture(cx);
                self.arm_timeout(COMMAND_TIMEOUT, cx);
            }
            WakeSignal::PartialTranscript(transcript) => {
                self.note_partial(transcript, cx);
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

    /// Turns the recognizer's running transcript into the two facts the window
    /// needs: somebody started talking, and somebody stopped.
    fn note_partial(&mut self, transcript: String, cx: &mut Context<Self>) {
        let speaking = activity(&self.last_partial, &transcript) == Activity::Speaking;
        self.last_partial = transcript;
        if !speaking {
            return;
        }

        match self.window {
            // Outside a window the wake word is the only way in, so growth
            // here is somebody talking near the machine, not to it.
            Window::Asleep => return,
            Window::Listening => {
                cx.emit(ListenEvent::Speaking);
                self.begin_capture(cx);
                self.window_timer.take();
            }
            Window::Capturing => {}
        }

        // Every word restarts the clock, so the utterance ends on a real
        // pause rather than on a fixed budget.
        self.silence = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SILENCE_ENDS_UTTERANCE).await;
            this.update(cx, |this, _| {
                if this.window == Window::Capturing {
                    this.audio.take();
                }
            })
            .ok();
        }));
    }

    /// Where the window goes once an exchange is over.
    fn close_or_rearm(&mut self, cx: &mut Context<Self>) {
        if !self.conversation {
            self.window = Window::Asleep;
            return;
        }
        self.window = Window::Listening;
        self.last_partial.clear();
        let window_duration = self.window_duration;
        self.window_timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(window_duration).await;
            this.update(cx, |this, cx| {
                if this.window != Window::Listening {
                    return;
                }
                this.window = Window::Asleep;
                this.last_partial.clear();
                cx.emit(ListenEvent::WindowClosed);
            })
            .ok();
        }));
    }

    fn begin_capture(&mut self, cx: &mut Context<Self>) {
        let (audio_sender, audio) = mpsc::unbounded();
        let mut transcripts = self.provider.transcribe(audio, cx);
        let wake_word = self.wake_word.clone();

        self.audio = Some(audio_sender);
        self.window = Window::Capturing;
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
        if self.window != Window::Capturing {
            return;
        }
        // Deliberately does not drop `transcription`: this runs inside that
        // task, and a task that cancels itself mid-poll is a trap for whoever
        // adds work after the dispatch.
        self.audio.take();
        self.timeout.take();
        self.silence.take();
        self.close_or_rearm(cx);

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
                if this.window != Window::Capturing {
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
        self.window = Window::Asleep;
        self.audio.take();
        self.transcription.take();
        self.timeout.take();
        self.silence.take();
        self.window_timer.take();
        self.last_partial.clear();
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

        fn window(&self, cx: &mut TestAppContext) -> Window {
            self.listener.read_with(cx, |listener, _| listener.window())
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
        assert_eq!(harness.window(cx), Window::Capturing);
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
        assert_eq!(harness.window(cx), Window::Asleep);
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
        assert_eq!(harness.window(cx), Window::Asleep);
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
        assert_eq!(harness.window(cx), Window::Asleep);
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
        harness.wake.push_partial("yes");
        cx.run_until_parked();
        harness.wake.end_utterance();
        cx.run_until_parked();

        assert_eq!(
            harness.events(),
            vec![
                ListenEvent::Speaking,
                ListenEvent::Command(VoiceCommand::Approve),
            ]
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

        assert_eq!(harness.window(cx), Window::Asleep);
        assert_eq!(harness.events(), vec![ListenEvent::Abandoned]);
    }

    /// The point of conversation mode. Without it, answering a follow-up means
    /// saying Echo's name again, which is not a conversation.
    #[gpui::test]
    async fn a_conversation_window_stays_open_after_an_exchange(cx: &mut TestAppContext) {
        let harness = harness(cx);
        harness.listener.update(cx, |listener, _| {
            listener.set_conversation(true, Duration::from_secs(15))
        });
        harness.stt.queue_transcript("what is it doing");
        harness.wake.wake();
        cx.run_until_parked();
        harness.wake.end_utterance();
        cx.run_until_parked();

        assert_eq!(
            harness.window(cx),
            Window::Listening,
            "the window should still be open for a follow-up"
        );
    }

    /// And a follow-up needs no wake word — speech alone starts the capture.
    #[gpui::test]
    async fn a_follow_up_inside_the_window_needs_no_wake_word(cx: &mut TestAppContext) {
        let harness = harness(cx);
        harness.listener.update(cx, |listener, _| {
            listener.set_conversation(true, Duration::from_secs(15))
        });
        harness.stt.queue_transcript("first");
        harness.stt.queue_transcript("and the tests");
        harness.wake.wake();
        cx.run_until_parked();
        harness.wake.end_utterance();
        cx.run_until_parked();

        harness.wake.push_partial("and the tests");
        cx.run_until_parked();
        harness.wake.end_utterance();
        cx.run_until_parked();

        assert_eq!(
            harness.stt.heard(),
            2,
            "the follow-up should be transcribed"
        );
        assert!(
            harness
                .events()
                .contains(&ListenEvent::Command(VoiceCommand::Say(
                    "and the tests".to_string()
                )))
        );
    }

    /// A window that never closed would leave the microphone accepting input
    /// indefinitely, which is the thing the wake word exists to prevent.
    #[gpui::test]
    async fn a_silent_window_closes_itself(cx: &mut TestAppContext) {
        let harness = harness(cx);
        harness.listener.update(cx, |listener, _| {
            listener.set_conversation(true, Duration::from_secs(15))
        });
        harness.stt.queue_transcript("what is it doing");
        harness.wake.wake();
        cx.run_until_parked();
        harness.wake.end_utterance();
        cx.run_until_parked();

        cx.background_executor
            .advance_clock(Duration::from_secs(16));
        cx.run_until_parked();

        assert_eq!(harness.window(cx), Window::Asleep);
        assert!(harness.events().contains(&ListenEvent::WindowClosed));
    }

    /// With conversation mode off, the shipped walkie-talkie behavior is
    /// unchanged: one utterance per wake word.
    #[gpui::test]
    async fn without_conversation_mode_the_window_closes_immediately(cx: &mut TestAppContext) {
        let harness = harness(cx);
        harness.stt.queue_transcript("what is it doing");
        harness.wake.wake();
        cx.run_until_parked();
        harness.wake.end_utterance();
        cx.run_until_parked();

        assert_eq!(harness.window(cx), Window::Asleep);
    }

    /// Speech inside a window ducks narration. Outside one it must not, or
    /// every conversation in the room would quiet the agent.
    #[gpui::test]
    async fn speech_ducks_only_inside_an_open_window(cx: &mut TestAppContext) {
        let harness = harness(cx);
        harness.wake.push_partial("somebody talking nearby");
        cx.run_until_parked();
        assert_eq!(harness.events(), vec![], "asleep, so nothing was addressed");

        harness.listener.update(cx, |listener, cx| {
            listener.arm_without_wake(Duration::from_secs(8), cx)
        });
        harness.stt.queue_transcript("pause");
        cx.run_until_parked();
        harness.wake.push_partial("pause");
        cx.run_until_parked();

        assert!(harness.events().contains(&ListenEvent::Speaking));
    }

    /// The utterance ends on a real pause rather than a fixed budget, so a
    /// slow speaker is not cut off mid-sentence.
    #[gpui::test]
    async fn an_utterance_ends_once_the_transcript_stops_growing(cx: &mut TestAppContext) {
        let harness = harness(cx);
        harness.listener.update(cx, |listener, cx| {
            listener.arm_without_wake(Duration::from_secs(30), cx)
        });
        harness.stt.queue_transcript("check the tests first");
        cx.run_until_parked();

        harness.wake.push_partial("check");
        cx.run_until_parked();
        cx.background_executor
            .advance_clock(SILENCE_ENDS_UTTERANCE / 2);
        harness.wake.push_partial("check the tests first");
        cx.run_until_parked();

        cx.background_executor
            .advance_clock(SILENCE_ENDS_UTTERANCE * 2);
        cx.run_until_parked();

        assert!(
            harness
                .events()
                .contains(&ListenEvent::Command(VoiceCommand::Say(
                    "check the tests first".to_string()
                )))
        );
    }

    /// The mirror of the gate: once woken, the frames the microphone was
    /// already producing must actually reach the provider, or the command is
    /// transcribed from silence.
    #[gpui::test]
    async fn frames_pushed_while_capturing_reach_the_provider(cx: &mut TestAppContext) {
        let harness = harness(cx);
        harness.stt.queue_transcript("approve");
        harness.wake.wake();
        cx.run_until_parked();

        harness.wake.push_frame(vec![0.1; 128]);
        cx.run_until_parked();
        harness.wake.end_utterance();
        cx.run_until_parked();

        assert_eq!(harness.stt.heard(), 1);
        assert_eq!(
            harness.events(),
            vec![
                ListenEvent::Woke,
                ListenEvent::Command(VoiceCommand::Approve),
            ]
        );
    }

    /// Audio captured before a wake must not reach a provider. The wake word
    /// is the gate, not a label on a stream that was being uploaded anyway.
    #[gpui::test]
    async fn frames_pushed_while_idle_reach_no_provider(cx: &mut TestAppContext) {
        let harness = harness(cx);
        harness.wake.push_frame(vec![0.1; 128]);
        cx.run_until_parked();

        assert_eq!(harness.stt.heard(), 0);
        assert_eq!(harness.events(), vec![]);
    }
}
