//! Recognizing the wake word on device.
//!
//! # Status
//!
//! The matcher below is complete and tested. The macOS binding that feeds it
//! is **not implemented**: it is unsafe FFI that cannot be exercised without a
//! live microphone and two granted permissions, and an untested `unsafe` block
//! in the editor's audio path is worse than an absent feature. Everything
//! above this module runs against [`crate::FakeWake`], so nothing else waits
//! on it.
//!
//! # What the binding needs, verified against objc2-speech
//!
//! - `objc2-speech` depends on `objc2 >=0.6.2, <0.8.0`, so it is compatible
//!   with the workspace's `objc2 = "0.6"`.
//! - `SFSpeechRecognizer::new()`, `initWithLocale(_)`.
//! - `SFSpeechRecognizer::requestAuthorization(&DynBlock<dyn Fn(SFSpeechRecognizerAuthorizationStatus)>)`
//!   — needs the `block2` feature. A denial must surface as
//!   [`crate::WakeSignal::Failed`] naming the permission, not a silent park.
//! - `recognitionTaskWithRequest_resultHandler(&SFSpeechRecognitionRequest, &DynBlock<dyn Fn(*mut SFSpeechRecognitionResult, *mut NSError)>)`
//!   — needs `block2`, `SFSpeechRecognitionRequest`, `SFSpeechRecognitionResult`,
//!   and `SFSpeechRecognitionTask`.
//! - `SFSpeechAudioBufferRecognitionRequest::new()`,
//!   `setRequiresOnDeviceRecognition(true)`, `setShouldReportPartialResults(true)`,
//!   `appendAudioPCMBuffer(&AVAudioPCMBuffer)`, `endAudio()`.
//!
//! The sharp edge is `appendAudioPCMBuffer`: it takes an `AVAudioPCMBuffer`,
//! not a slice, so the frames from `audio::open_input_stream` cannot be
//! handed over directly. Two ways out, and the choice wants a live machine to
//! settle:
//!
//! 1. Build `AVAudioPCMBuffer`s from the rodio frames through `objc2-avf-audio`
//!    — keeps one microphone owner, costs a copy and raw-pointer work.
//! 2. Drive `AVAudioEngine::installTapOnBus` and let AVFoundation own the
//!    input — the pattern every Apple example uses, at the cost of a second
//!    input path alongside the existing cpal one.
//!
//! ponytail: `SFSpeechRecognizer` enforces a per-request audio duration limit,
//! so a continuous listener has to restart its request on a timer of roughly
//! fifty seconds. macOS 26's `SpeechAnalyzer` removes the limit and is more
//! accurate, but it is Swift-only, so reaching it means shipping a helper
//! binary rather than an objc2 binding. That is the upgrade path.

/// Whether `transcript` contains `wake_word` as a whole word.
///
/// Whole-word rather than prefix: "echoing" and "echoes" are ordinary English
/// in a conversation about a program named Echo, and every false wake costs
/// the listener a narration pause.
pub fn contains_wake_word(transcript: &str, wake_word: &str) -> bool {
    let wake_word = wake_word.trim().to_lowercase();
    if wake_word.is_empty() {
        return false;
    }
    transcript
        .to_lowercase()
        .split(|character: char| !character.is_alphanumeric())
        .any(|word| word == wake_word)
}

#[cfg(all(test, target_os = "macos"))]
mod chain_tests {
    use super::macos::{cancelled_input, for_recognizer};
    use rodio::Source as _;

    fn silence() -> rodio::source::SineWave {
        rodio::source::SineWave::new(440.0)
    }

    /// The canceller must see the APM's own format. Downmixing first is not an
    /// error — it is cancellation that quietly does nothing, which no amount of
    /// listening to the result would reveal.
    #[test]
    fn cancellation_happens_in_the_processing_module_s_format() {
        let cancelled = cancelled_input(silence(), audio::EchoCanceller::default());
        assert_eq!(cancelled.channels(), audio::CHANNEL_COUNT);
        assert_eq!(cancelled.sample_rate(), audio::SAMPLE_RATE);
    }

    /// And the recognizer must see its own, downstream of that.
    #[test]
    fn the_recognizer_is_handed_mono_at_its_own_rate() {
        let chain = for_recognizer(cancelled_input(silence(), audio::EchoCanceller::default()));
        assert_eq!(chain.channels(), rodio::nz!(1));
        assert_eq!(chain.sample_rate(), super::macos::SAMPLE_RATE_HZ);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wake_word_is_found_at_the_start_of_a_partial_transcript() {
        assert!(contains_wake_word("Echo check the tests", "echo"));
        assert!(contains_wake_word("echo, approve", "echo"));
    }

    /// The recognizer reports a growing transcript, so the wake word turns up
    /// mid-string once the user keeps talking.
    #[test]
    fn a_wake_word_is_found_after_earlier_speech() {
        assert!(contains_wake_word("so anyway echo approve", "echo"));
    }

    /// Otherwise "echoing the change" wakes the listener.
    #[test]
    fn a_word_that_merely_starts_with_the_wake_word_does_not_wake() {
        assert!(!contains_wake_word("echoing the change", "echo"));
        assert!(!contains_wake_word("recheck the echoes", "echo"));
    }

    /// An empty wake word would match every gap between characters and wake on
    /// silence.
    #[test]
    fn an_empty_wake_word_never_wakes() {
        assert!(!contains_wake_word("anything at all", ""));
        assert!(!contains_wake_word("anything at all", "   "));
    }
}

/// The on-device wake listener.
///
/// Everything Objective-C here lives on one dedicated thread. The Speech and
/// AVFoundation objects are not `Send`, and the microphone iterator blocks, so
/// a thread of its own is both required and the simplest thing that works.
#[cfg(target_os = "macos")]
mod macos {
    use super::contains_wake_word;
    use crate::listener::{WakeSignal, WakeSource};
    use anyhow::{Result, anyhow};
    use audio::RodioExt as _;
    use block2::RcBlock;
    use futures::channel::mpsc;
    use gpui::App;
    use objc2::AllocAnyThread as _;
    use objc2::rc::Retained;
    use objc2_avf_audio::{AVAudioCommonFormat, AVAudioFormat, AVAudioPCMBuffer};
    use objc2_foundation::NSError;
    use objc2_speech::{
        SFSpeechAudioBufferRecognitionRequest, SFSpeechRecognitionResult, SFSpeechRecognitionTask,
        SFSpeechRecognizer, SFSpeechRecognizerAuthorizationStatus,
    };
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    /// What the recognizer is fed and what the command provider is handed.
    /// Inworld's transcribe endpoint is told the same number.
    pub(super) const SAMPLE_RATE_HZ: std::num::NonZeroU32 = rodio::nz!(16_000);
    const SAMPLE_RATE: u32 = SAMPLE_RATE_HZ.get();

    /// Samples per buffer handed to the recognizer. About 64ms, which keeps
    /// the wake word's latency below what anyone notices without waking the
    /// audio thread for every frame.
    const FRAME_SAMPLES: usize = 1_024;

    /// `SFSpeechRecognizer` enforces a per-request audio duration limit, so a
    /// listener that must run all day restarts its request before reaching it.
    ///
    /// ponytail: this is the whole reason the listener is a loop rather than
    /// one long-lived request. macOS 26's `SpeechAnalyzer` has no such limit
    /// and is more accurate, but it is Swift-only — reaching it means shipping
    /// a helper binary rather than an objc2 binding. That is the upgrade path.
    const REQUEST_LIFETIME: Duration = Duration::from_secs(50);

    pub struct SpeechWake {
        wake_word: Arc<Mutex<String>>,
        echo_canceller: audio::EchoCanceller,
        signals: WakeSenders,
        frames: FrameSenders,
        stop: Arc<AtomicBool>,
    }

    impl SpeechWake {
        /// Starts listening. The microphone opens immediately, but nothing
        /// leaves the machine until the wake word is heard.
        ///
        /// `echo_canceller` must be the one the output mixer is already
        /// feeding as its reference; a fresh one has nothing to cancel
        /// against.
        pub fn new(wake_word: String, echo_canceller: audio::EchoCanceller) -> Result<Self> {
            let this = Self {
                wake_word: Arc::new(Mutex::new(wake_word)),
                echo_canceller,
                signals: Arc::default(),
                frames: Arc::default(),
                stop: Arc::new(AtomicBool::new(false)),
            };
            this.spawn_listener()?;
            Ok(this)
        }

        pub fn set_wake_word(&self, wake_word: String) {
            if wake_word.trim().is_empty() {
                return;
            }
            match self.wake_word.lock() {
                Ok(mut current) => *current = wake_word,
                Err(error) => log::error!("listen: wake word poisoned: {error}"),
            }
        }

        fn spawn_listener(&self) -> Result<()> {
            let wake_word = self.wake_word.clone();
            let signals = self.signals.clone();
            let frames = self.frames.clone();
            let stop = self.stop.clone();
            let echo_canceller = self.echo_canceller.clone();

            std::thread::Builder::new()
                .name("listen-wake".into())
                .spawn(move || {
                    if let Err(error) =
                        run_listener(wake_word, echo_canceller, &signals, &frames, &stop)
                    {
                        log::error!("listen: the wake listener stopped: {error}");
                        emit(&signals, WakeSignal::Failed(format!("{error}")));
                    }
                })
                .map_err(|error| anyhow!("starting the wake listener: {error}"))?;
            Ok(())
        }
    }

    impl Drop for SpeechWake {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
        }
    }

    impl WakeSource for SpeechWake {
        fn wake_events(&self, _cx: &App) -> mpsc::UnboundedReceiver<WakeSignal> {
            let (sender, receiver) = mpsc::unbounded();
            match self.signals.lock() {
                Ok(mut senders) => senders.push(sender),
                Err(error) => log::error!("listen: wake subscribers poisoned: {error}"),
            }
            receiver
        }

        fn audio_frames(&self, _cx: &App) -> mpsc::UnboundedReceiver<Vec<f32>> {
            let (sender, receiver) = mpsc::unbounded();
            match self.frames.lock() {
                Ok(mut senders) => senders.push(sender),
                Err(error) => log::error!("listen: frame subscribers poisoned: {error}"),
            }
            receiver
        }
    }

    fn emit(senders: &WakeSenders, signal: WakeSignal) {
        let Ok(mut senders) = senders.lock() else {
            return;
        };
        senders.retain(|sender| sender.unbounded_send(signal.clone()).is_ok());
    }

    fn emit_frame(senders: &FrameSenders, frame: Vec<f32>) {
        let Ok(mut senders) = senders.lock() else {
            return;
        };
        senders.retain(|sender| sender.unbounded_send(frame.clone()).is_ok());
    }

    /// Blocks until permission is granted or refused.
    ///
    /// A refusal is reported rather than parked on: a listener that has
    /// silently stopped listening is indistinguishable from one that works,
    /// right up until the moment somebody needs it.
    fn await_authorization() -> Result<()> {
        let (sender, receiver) = std::sync::mpsc::channel();
        let handler = RcBlock::new(move |status: SFSpeechRecognizerAuthorizationStatus| {
            sender.send(status).ok();
        });
        unsafe { SFSpeechRecognizer::requestAuthorization(&handler) };

        match receiver.recv_timeout(Duration::from_secs(60)) {
            Ok(SFSpeechRecognizerAuthorizationStatus::Authorized) => Ok(()),
            Ok(SFSpeechRecognizerAuthorizationStatus::Denied) => Err(anyhow!(
                "speech recognition is denied for Echo in System Settings › Privacy & Security › Speech Recognition"
            )),
            Ok(SFSpeechRecognizerAuthorizationStatus::Restricted) => {
                Err(anyhow!("speech recognition is restricted on this device"))
            }
            Ok(_) => Err(anyhow!("speech recognition was not authorized")),
            Err(error) => Err(anyhow!(
                "waiting for speech recognition permission: {error}"
            )),
        }
    }

    type WakeSenders = Arc<Mutex<Vec<mpsc::UnboundedSender<WakeSignal>>>>;
    type FrameSenders = Arc<Mutex<Vec<mpsc::UnboundedSender<Vec<f32>>>>>;

    fn run_listener(
        wake_word: Arc<Mutex<String>>,
        echo_canceller: audio::EchoCanceller,
        signals: &WakeSenders,
        frames: &FrameSenders,
        stop: &AtomicBool,
    ) -> Result<()> {
        await_authorization()?;

        let recognizer = unsafe { SFSpeechRecognizer::new() };
        if !unsafe { recognizer.supportsOnDeviceRecognition() } {
            return Err(anyhow!(
                "this Mac cannot recognize speech on device, and Echo will not send \
                 continuous audio to a server to find a wake word"
            ));
        }

        let format = build_format()?;
        let mut microphone = cancelling_microphone(echo_canceller)?;

        // Each iteration is one recognition request, retired before it reaches
        // the framework's duration limit and immediately replaced.
        while !stop.load(Ordering::Relaxed) {
            let session = RecognitionSession::start(&recognizer, signals, wake_word.clone())?;
            let started = Instant::now();

            while !stop.load(Ordering::Relaxed) && started.elapsed() < REQUEST_LIFETIME {
                let mut frame = Vec::with_capacity(FRAME_SAMPLES);
                for _ in 0..FRAME_SAMPLES {
                    match microphone.next() {
                        Some(sample) => frame.push(sample),
                        None => {
                            session.finish();
                            return Err(anyhow!("the microphone stopped producing audio"));
                        }
                    }
                }

                session.append(&format, &frame)?;
                emit_frame(frames, frame);
            }

            session.finish();
        }

        Ok(())
    }

    /// The microphone, with the speaker's own sound removed.
    ///
    /// Order matters and is not obvious: the APM works in 48 kHz stereo over
    /// 10 ms buffers, so cancellation has to happen before the downmix to the
    /// 16 kHz mono the recognizer wants. Downmixing first destroys the
    /// stereo relationship the canceller matches against, and the result is
    /// not an error — it is cancellation that silently does nothing.
    fn cancelling_microphone(echo_canceller: audio::EchoCanceller) -> Result<impl rodio::Source> {
        let microphone = audio::open_input_stream(None)
            .map_err(|error| anyhow!("opening the microphone: {error}"))?;
        Ok(for_recognizer(cancelled_input(microphone, echo_canceller)))
    }

    /// Echo-cancelled audio, still in the APM's own format.
    ///
    /// Split from [`for_recognizer`] so each stage's format is a checkable
    /// contract rather than a comment. Downmixing before this point destroys
    /// the stereo relationship the canceller matches against, and the result
    /// is not an error — it is cancellation that silently does nothing.
    pub(super) fn cancelled_input<S: rodio::Source>(
        source: S,
        mut echo_canceller: audio::EchoCanceller,
    ) -> impl rodio::Source {
        use rodio::cpal::Sample as _;

        source
            .constant_params(audio::CHANNEL_COUNT, audio::SAMPLE_RATE)
            .process_buffer::<{ audio::BUFFER_SIZE }, _>(move |buffer| {
                let mut cancelled: [i16; audio::BUFFER_SIZE] =
                    buffer.map(|sample| sample.to_sample());
                if echo_canceller.process_stream(&mut cancelled).is_err() {
                    // A failed frame leaves un-cancelled audio rather than
                    // silence: the wake word still has to be heard through it.
                    return;
                }
                for (sample, cancelled) in buffer.iter_mut().zip(cancelled) {
                    *sample = cancelled.to_sample();
                }
            })
    }

    /// What the recognizer and the command provider both want.
    pub(super) fn for_recognizer<S: rodio::Source>(source: S) -> impl rodio::Source {
        source
            .possibly_disconnected_channels_to_mono()
            .constant_samplerate(SAMPLE_RATE_HZ)
    }

    fn build_format() -> Result<Retained<AVAudioFormat>> {
        unsafe {
            AVAudioFormat::initWithCommonFormat_sampleRate_channels_interleaved(
                AVAudioFormat::alloc(),
                AVAudioCommonFormat::PCMFormatFloat32,
                SAMPLE_RATE as f64,
                1,
                false,
            )
        }
        .ok_or_else(|| anyhow!("describing the microphone's audio format"))
    }

    /// One recognition request and the task draining it.
    struct RecognitionSession {
        request: Retained<SFSpeechAudioBufferRecognitionRequest>,
        task: Retained<SFSpeechRecognitionTask>,
    }

    impl RecognitionSession {
        fn start(
            recognizer: &SFSpeechRecognizer,
            signals: &WakeSenders,
            wake_word: Arc<Mutex<String>>,
        ) -> Result<Self> {
            let request = unsafe { SFSpeechAudioBufferRecognitionRequest::new() };
            unsafe {
                // On device, always: this listener runs continuously, and
                // shipping every word spoken near the machine to a server to
                // look for one wake word is not a trade this makes.
                request.setRequiresOnDeviceRecognition(true);
                request.setShouldReportPartialResults(true);
            }

            // The handler runs on the framework's own queue, so everything it
            // touches is behind a lock or a channel.
            let signals = signals.clone();
            let woke = Arc::new(AtomicBool::new(false));
            let handler = RcBlock::new(
                move |result: *mut SFSpeechRecognitionResult, error: *mut NSError| {
                    if !error.is_null() {
                        // A request that ends at its duration limit reports an
                        // error; the loop is already replacing it, so this is
                        // logged rather than surfaced as a failure.
                        log::debug!("listen: a recognition request ended");
                        return;
                    }
                    let Some(result) = (unsafe { result.as_ref() }) else {
                        return;
                    };

                    let transcript =
                        unsafe { result.bestTranscription().formattedString() }.to_string();
                    let heard = wake_word
                        .lock()
                        .map(|wake_word| contains_wake_word(&transcript, &wake_word))
                        .unwrap_or(false);

                    if heard && !woke.swap(true, Ordering::Relaxed) {
                        emit(&signals, WakeSignal::Woke);
                    }

                    // Forwarded whether or not the wake word was heard: inside
                    // an open conversation window this is the only signal that
                    // somebody has started talking, and the window is opened by
                    // the listener, which knows things this handler does not.
                    emit(&signals, WakeSignal::PartialTranscript(transcript));
                    if unsafe { result.isFinal() } {
                        if woke.swap(false, Ordering::Relaxed) {
                            emit(&signals, WakeSignal::UtteranceEnded);
                        }
                    }
                },
            );

            let task =
                unsafe { recognizer.recognitionTaskWithRequest_resultHandler(&request, &handler) };
            Ok(Self { request, task })
        }

        fn append(&self, format: &AVAudioFormat, frame: &[f32]) -> Result<()> {
            let buffer = unsafe {
                AVAudioPCMBuffer::initWithPCMFormat_frameCapacity(
                    AVAudioPCMBuffer::alloc(),
                    format,
                    frame.len() as u32,
                )
            }
            .ok_or_else(|| anyhow!("allocating an audio buffer for the recognizer"))?;

            unsafe {
                let channels = buffer.floatChannelData();
                if channels.is_null() {
                    return Err(anyhow!("the audio buffer exposed no float channel"));
                }
                // Mono and non-interleaved, so there is exactly one channel
                // pointer and `frame.len()` samples behind it — the capacity
                // just requested.
                let channel = (*channels).as_ptr();
                std::ptr::copy_nonoverlapping(frame.as_ptr(), channel, frame.len());
                buffer.setFrameLength(frame.len() as u32);
                self.request.appendAudioPCMBuffer(&buffer);
            }
            Ok(())
        }

        fn finish(&self) {
            unsafe {
                self.request.endAudio();
                self.task.finish();
            }
        }
    }
}

#[cfg(target_os = "macos")]
pub use macos::SpeechWake;
