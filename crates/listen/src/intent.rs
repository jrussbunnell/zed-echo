/// What the user asked for.
///
/// `Say` carries free text; whether it steers a running turn or starts a new
/// one is not decided here, because that depends on thread state this crate
/// deliberately cannot see.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VoiceCommand {
    Say(String),
    Approve,
    Deny,
    Pause,
    Resume,
    Repeat,
    CatchUp,
    Next,
    Previous,
    /// Halt the agent's current turn.
    Interrupt,
    /// Heard something, but there is nothing to do with it.
    Never,
}

/// Phrases matched whole, after the wake word and punctuation are stripped.
///
/// Longer phrases come first so "stop the agent" is not shadowed by "stop".
///
/// ponytail: a keyword table, not a model. A model round-trip would add about
/// a second to "stop", the one command whose whole value is being instant.
/// Classification by model is the upgrade if this proves brittle in use.
const EXACT: &[(&str, VoiceCommand)] = &[
    ("stop the agent", VoiceCommand::Interrupt),
    ("stop the turn", VoiceCommand::Interrupt),
    ("cancel the turn", VoiceCommand::Interrupt),
    ("cancel that", VoiceCommand::Interrupt),
    ("interrupt", VoiceCommand::Interrupt),
    ("abort", VoiceCommand::Interrupt),
    ("stop talking", VoiceCommand::Pause),
    ("be quiet", VoiceCommand::Pause),
    ("stop", VoiceCommand::Pause),
    ("quiet", VoiceCommand::Pause),
    ("pause", VoiceCommand::Pause),
    ("hush", VoiceCommand::Pause),
    ("approve", VoiceCommand::Approve),
    ("approved", VoiceCommand::Approve),
    ("yes", VoiceCommand::Approve),
    ("yep", VoiceCommand::Approve),
    ("yeah", VoiceCommand::Approve),
    ("ok", VoiceCommand::Approve),
    ("okay", VoiceCommand::Approve),
    ("go ahead", VoiceCommand::Approve),
    ("allow it", VoiceCommand::Approve),
    ("allow", VoiceCommand::Approve),
    ("do it", VoiceCommand::Approve),
    ("deny", VoiceCommand::Deny),
    ("denied", VoiceCommand::Deny),
    ("no", VoiceCommand::Deny),
    ("nope", VoiceCommand::Deny),
    ("reject", VoiceCommand::Deny),
    ("don't", VoiceCommand::Deny),
    ("do not", VoiceCommand::Deny),
    ("resume", VoiceCommand::Resume),
    ("continue", VoiceCommand::Resume),
    ("keep going", VoiceCommand::Resume),
    ("carry on", VoiceCommand::Resume),
    ("say that again", VoiceCommand::Repeat),
    ("what was that", VoiceCommand::Repeat),
    ("repeat", VoiceCommand::Repeat),
    ("again", VoiceCommand::Repeat),
    ("catch me up", VoiceCommand::CatchUp),
    ("catch up", VoiceCommand::CatchUp),
    ("where are we", VoiceCommand::CatchUp),
    ("what's the status", VoiceCommand::CatchUp),
    ("skip ahead", VoiceCommand::Next),
    ("skip", VoiceCommand::Next),
    ("next", VoiceCommand::Next),
    ("go back", VoiceCommand::Previous),
    ("back up", VoiceCommand::Previous),
    ("previous", VoiceCommand::Previous),
    ("never mind", VoiceCommand::Never),
    ("nevermind", VoiceCommand::Never),
    ("forget it", VoiceCommand::Never),
];

/// Turns one final transcript into a command.
///
/// A leading wake word is stripped: a transcript that still carries it and one
/// from a wake listener that already consumed it both arrive here, and both
/// must mean the same thing.
pub fn parse_intent(transcript: &str, wake_word: &str) -> VoiceCommand {
    let spoken = strip_wake_word(transcript, wake_word);
    let normalized = normalize(spoken);

    if normalized.is_empty() {
        return VoiceCommand::Never;
    }

    for (phrase, command) in EXACT {
        if normalized == *phrase {
            return command.clone();
        }
    }

    VoiceCommand::Say(spoken.trim().to_string())
}

/// Removes a leading wake word and whatever punctuation followed it, leaving
/// the casing of the rest alone — a message to the agent is quoted, not
/// normalized.
fn strip_wake_word<'a>(transcript: &'a str, wake_word: &str) -> &'a str {
    let trimmed = transcript.trim();
    let wake_word = wake_word.trim();
    if wake_word.is_empty() {
        return trimmed;
    }
    let Some(remainder) = trimmed
        .get(..wake_word.len())
        .filter(|head| head.eq_ignore_ascii_case(wake_word))
        .and_then(|_| trimmed.get(wake_word.len()..))
    else {
        return trimmed;
    };
    // Only a word boundary counts, so "echoing the change" keeps its first
    // word rather than becoming "ing the change".
    if remainder
        .chars()
        .next()
        .is_some_and(|character| character.is_alphanumeric())
    {
        return trimmed;
    }
    remainder
        .trim_start_matches([',', '.', '!', '?', ':', ';', '—', '-'])
        .trim()
}

/// Lowercases, drops trailing punctuation, and collapses whitespace, so the
/// table can be written the way a person would say the phrase.
fn normalize(spoken: &str) -> String {
    spoken
        .trim()
        .trim_end_matches(['.', '!', '?', ',', ';', ':'])
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> VoiceCommand {
        parse_intent(text, "echo")
    }

    #[test]
    fn the_wake_word_is_stripped_before_matching() {
        assert_eq!(parse("Echo, approve"), VoiceCommand::Approve);
        assert_eq!(parse("echo approve"), VoiceCommand::Approve);
        assert_eq!(parse("approve"), VoiceCommand::Approve);
    }

    #[test]
    fn affirmatives_and_negatives_answer_a_prompt() {
        for text in ["approve", "yes", "go ahead", "allow it", "do it"] {
            assert_eq!(parse(text), VoiceCommand::Approve, "{text}");
        }
        for text in ["deny", "no", "reject", "don't"] {
            assert_eq!(parse(text), VoiceCommand::Deny, "{text}");
        }
    }

    /// The single most consequential split in the grammar. Bare "stop" is the
    /// recoverable reading — quiet down — because a listener who meant to halt
    /// the agent can say so again, while one who lost a turn's work cannot get
    /// it back.
    #[test]
    fn bare_stop_quiets_narration_and_only_an_explicit_phrase_halts_the_agent() {
        assert_eq!(parse("stop"), VoiceCommand::Pause);
        assert_eq!(parse("stop talking"), VoiceCommand::Pause);
        assert_eq!(parse("be quiet"), VoiceCommand::Pause);

        assert_eq!(parse("stop the agent"), VoiceCommand::Interrupt);
        assert_eq!(parse("cancel that"), VoiceCommand::Interrupt);
        assert_eq!(parse("interrupt"), VoiceCommand::Interrupt);
    }

    #[test]
    fn narration_control_maps_to_its_verbs() {
        assert_eq!(parse("resume"), VoiceCommand::Resume);
        assert_eq!(parse("keep going"), VoiceCommand::Resume);
        assert_eq!(parse("say that again"), VoiceCommand::Repeat);
        assert_eq!(parse("repeat"), VoiceCommand::Repeat);
        assert_eq!(parse("catch me up"), VoiceCommand::CatchUp);
        assert_eq!(parse("skip ahead"), VoiceCommand::Next);
        assert_eq!(parse("go back"), VoiceCommand::Previous);
        assert_eq!(parse("never mind"), VoiceCommand::Never);
    }

    #[test]
    fn anything_unmatched_is_something_to_say_to_the_agent() {
        assert_eq!(
            parse("Echo, check the tests before you refactor"),
            VoiceCommand::Say("check the tests before you refactor".to_string())
        );
    }

    /// A transcript that is only a wake word is the recognizer hearing Echo's
    /// own name and nothing else. Dispatching it as a message would send the
    /// word "echo" to the agent.
    #[test]
    fn a_transcript_that_is_only_the_wake_word_is_not_a_command() {
        assert_eq!(parse("Echo"), VoiceCommand::Never);
        assert_eq!(parse("  echo,  "), VoiceCommand::Never);
    }

    /// A word that merely begins with the wake word is ordinary English in a
    /// conversation about a program named Echo.
    #[test]
    fn a_word_that_merely_starts_with_the_wake_word_keeps_its_first_word() {
        assert_eq!(
            parse("echoing the change everywhere"),
            VoiceCommand::Say("echoing the change everywhere".to_string())
        );
    }
}
