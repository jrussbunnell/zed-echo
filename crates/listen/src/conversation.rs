//! How long Echo stays listening once it has been addressed.
//!
//! The shipped behavior is a walkie-talkie: the wake word buys exactly one
//! utterance. That is right when every utterance costs an agent turn, and
//! wrong once Echo can answer things itself — a conversation in which you say
//! its name before every sentence is not one.
//!
//! This is the window that stays open instead. It deliberately does *not*
//! remove the wake word: a microphone that is simply always accepting input
//! turns every side conversation, phone call, and person walking into the room
//! into a command.

use std::time::Duration;

/// How long the window stays open after an exchange before it closes itself.
pub const DEFAULT_WINDOW: Duration = Duration::from_secs(15);

/// How long a transcript must stop growing before the utterance is over.
///
/// The recognizer's own endpointing is more accurate and far too slow for
/// back-and-forth — it is tuned for dictation, where waiting is cheaper than
/// cutting somebody off.
///
/// ponytail: a partial-transcript heuristic, not a voice-activity detector.
/// `SFSpeechDetector` is the upgrade if this proves jumpy in a noisy room.
pub const SILENCE_ENDS_UTTERANCE: Duration = Duration::from_millis(800);

/// Whether Echo is still listening, and why.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Window {
    /// Not listening. Only the wake word gets in.
    Asleep,
    /// Addressed, and waiting for something to be said. No wake word needed.
    Listening,
    /// Somebody is talking right now.
    Capturing,
}

impl Window {
    pub fn is_open(self) -> bool {
        !matches!(self, Window::Asleep)
    }
}

/// What a transcript update means for the window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Activity {
    /// The transcript grew: somebody is talking.
    Speaking,
    /// The transcript did not grow.
    Quiet,
}

/// Whether a partial transcript represents new speech.
///
/// Compared by length rather than by equality because the recognizer revises
/// what it already heard as often as it adds to it, and a revision is not
/// somebody starting to talk again.
pub fn activity(previous: &str, current: &str) -> Activity {
    if current.trim().len() > previous.trim().len() {
        Activity::Speaking
    } else {
        Activity::Quiet
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_growing_transcript_is_somebody_talking() {
        assert_eq!(activity("check the", "check the tests"), Activity::Speaking);
        assert_eq!(activity("", "echo"), Activity::Speaking);
    }

    #[test]
    fn a_transcript_that_stopped_growing_is_quiet() {
        assert_eq!(
            activity("check the tests", "check the tests"),
            Activity::Quiet
        );
        assert_eq!(activity("check the tests", ""), Activity::Quiet);
    }

    /// The recognizer revises as often as it extends. Treating a correction as
    /// fresh speech would hold the utterance open indefinitely while it
    /// second-guessed a word.
    #[test]
    fn a_revision_of_the_same_length_is_not_new_speech() {
        assert_eq!(
            activity("recognise that", "recognize that"),
            Activity::Quiet
        );
    }

    #[test]
    fn only_asleep_is_closed() {
        assert!(!Window::Asleep.is_open());
        assert!(Window::Listening.is_open());
        assert!(Window::Capturing.is_open());
    }
}
