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
