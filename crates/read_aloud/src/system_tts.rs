//! The platform's own speech synthesizer, used when no cloud TTS is configured.
//!
//! Without this, a fresh install has read aloud dark until the user goes and
//! gets an Inworld API key — the fork's headline feature, unavailable on first
//! run. The system voice is always present, needs no key, no model download,
//! and no network.
//!
//! It shells out to `say` rather than binding `AVSpeechSynthesizer` through
//! objc2. `say` is the same synthesizer, reachable without a line of unsafe, and
//! `--data-format=LEI16@22050` makes it emit exactly the WAV encoding the
//! Inworld path already decodes. The cost is word timings: `say` reports none,
//! so highlighting degrades to whole sentences, which every consumer of
//! [`Pcm::words`] already handles. Binding `writeUtterance:toMarkerCallback:`
//! would recover per-word timing and is the upgrade path if that matters.

use crate::provider::{Pcm, TtsProvider, TtsVoice, decode_linear16, strip_wav_header};
use anyhow::{Context as _, Result, anyhow};
use futures::channel::mpsc;
use gpui::{App, AppContext as _};
use std::sync::Mutex;

/// Matches the rate the Inworld path uses, so a provider swap does not change
/// the sample rate the player and sink are working in.
const SAMPLE_RATE: u32 = 22050;

pub const SYSTEM_PROVIDER: &str = "system";

pub struct SystemTts {
    /// `None` speaks with whatever voice the user selected in System Settings.
    voice: Mutex<Option<String>>,
}

impl SystemTts {
    pub fn new(voice: Option<String>) -> Self {
        Self {
            voice: Mutex::new(voice.filter(|voice| !voice.trim().is_empty())),
        }
    }

    /// Applies to the next synthesis request, matching how the Inworld provider
    /// handles a settings change.
    pub fn set_voice(&self, voice: Option<String>) {
        match self.voice.lock() {
            Ok(mut current) => *current = voice.filter(|voice| !voice.trim().is_empty()),
            Err(error) => log::error!("read_aloud: system voice selection poisoned: {error}"),
        }
    }
}

impl TtsProvider for SystemTts {
    fn synthesize(&self, text: String, cx: &App) -> mpsc::UnboundedReceiver<Result<Pcm>> {
        let (sender, receiver) = mpsc::unbounded();
        let voice = match self.voice.lock() {
            Ok(voice) => voice.clone(),
            Err(error) => {
                sender
                    .unbounded_send(Err(anyhow!("system voice selection poisoned: {error}")))
                    .ok();
                return receiver;
            }
        };

        cx.background_spawn(async move {
            // One chunk, not a stream: `say` synthesizes locally in a fraction
            // of the time a network round-trip takes, so there is no latency to
            // hide, and it only finishes the WAV header once the file is
            // complete anyway.
            sender
                .unbounded_send(speak(&text, voice.as_deref()).await)
                .ok();
        })
        .detach();

        receiver
    }
}

/// Runs `say` into a temporary WAV and decodes it.
async fn speak(text: &str, voice: Option<&str>) -> Result<Pcm> {
    if text.trim().is_empty() {
        return Err(anyhow!("nothing to speak"));
    }

    // A real file rather than a pipe: `say` backfills the RIFF length fields
    // when it closes the container, which needs a seekable destination.
    let output = tempfile::Builder::new()
        .prefix("zed-read-aloud-")
        .suffix(".wav")
        .tempfile()
        .context("creating a temporary file for the system voice")?;

    let mut command = smol::process::Command::new("/usr/bin/say");
    command
        .arg("--file-format=WAVE")
        .arg(format!("--data-format=LEI16@{SAMPLE_RATE}"))
        .arg("-o")
        .arg(output.path());
    if let Some(voice) = voice {
        command.arg("-v").arg(voice);
    }
    // `--` then the text, so an utterance starting with a dash is not read as a
    // flag. Passed as an argument rather than on stdin because `say` treats a
    // missing terminator on stdin as more input to come.
    command.arg("--").arg(text);

    let result = command
        .output()
        .await
        .context("running the system speech synthesizer")?;
    if !result.status.success() {
        return Err(anyhow!(
            "the system speech synthesizer failed: {}",
            String::from_utf8_lossy(&result.stderr).trim()
        ));
    }

    let bytes = smol::fs::read(output.path())
        .await
        .context("reading the system voice's output")?;
    let samples = decode_linear16(strip_wav_header(&bytes));
    if samples.is_empty() {
        return Err(anyhow!("the system speech synthesizer produced no audio"));
    }

    Ok(Pcm {
        samples,
        sample_rate: SAMPLE_RATE,
        channels: 1,
        // `say` reports no word timings, so highlighting falls back to whole
        // sentences. See the module docs for the upgrade path.
        words: Vec::new(),
    })
}

/// The installed voices, filtered to English and sorted by name.
///
/// `say -v '?'` prints `Name  locale  # sample sentence`, one per line. The full
/// list is ~185 voices across dozens of languages; the Inworld catalog is
/// filtered the same way and for the same reason.
pub fn available_voices(cx: &App) -> gpui::Task<Result<Vec<TtsVoice>>> {
    cx.background_spawn(async move {
        let output = smol::process::Command::new("/usr/bin/say")
            .arg("-v")
            .arg("?")
            .output()
            .await
            .context("listing the system voices")?;
        if !output.status.success() {
            return Err(anyhow!("could not list the system voices"));
        }
        Ok(parse_voice_list(&String::from_utf8_lossy(&output.stdout)))
    })
}

fn parse_voice_list(listing: &str) -> Vec<TtsVoice> {
    let mut voices: Vec<TtsVoice> = listing
        .lines()
        .filter_map(|line| {
            // The name may contain spaces ("Eddy (English (UK))"), so split on
            // the locale rather than on whitespace: the locale is the last
            // field before the `#` comment.
            let (before_comment, _) = line.split_once('#')?;
            let mut fields = before_comment.split_whitespace().collect::<Vec<_>>();
            let locale = fields.pop()?;
            if !locale.starts_with("en") {
                return None;
            }
            let name = fields.join(" ");
            if name.is_empty() {
                return None;
            }
            Some(TtsVoice {
                id: name.clone().into(),
                name: name.into(),
            })
        })
        .collect();
    voices.sort_by(|left, right| left.name.cmp(&right.name));
    voices.dedup_by(|left, right| left.id == right.id);
    voices
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_voice_list_keeps_english_voices_and_their_full_names() {
        // Shape from a live `say -v '?'` on macOS 26, including a voice whose
        // name contains spaces and parentheses.
        let listing = "\
Albert              en_US    # Hello! My name is Albert.
Alice               it_IT    # Ciao! Mi chiamo Alice.
Eddy (English (UK)) en_GB    # Hello! My name is Eddy.
Alva                sv_SE    # Hej! Jag heter Alva.
Zoe                 en_US    # Hello! My name is Zoe.
";
        let voices = parse_voice_list(listing);
        let names: Vec<&str> = voices.iter().map(|voice| voice.name.as_ref()).collect();
        assert_eq!(
            names,
            vec!["Albert", "Eddy (English (UK))", "Zoe"],
            "non-English voices are dropped and names keep their spaces"
        );
    }

    #[test]
    fn a_listing_with_no_english_voices_is_empty_rather_than_an_error() {
        let voices = parse_voice_list("Alva  sv_SE  # Hej!\n");
        assert!(voices.is_empty());
    }

    #[test]
    fn malformed_lines_are_skipped() {
        assert!(parse_voice_list("garbage with no comment\n\n").is_empty());
    }

    /// Exercises the real synthesizer and the shared WAV decoder end to end.
    /// Skipped anywhere `say` is absent so the suite stays runnable off macOS.
    ///
    /// Deliberately not a `#[gpui::test]`: GPUI's deterministic scheduler
    /// forbids parking, and this genuinely blocks on a subprocess.
    #[test]
    fn the_system_voice_produces_decodable_audio() {
        if !std::path::Path::new("/usr/bin/say").exists() {
            return;
        }
        let pcm = smol::block_on(speak("Testing one two three.", None))
            .expect("the system voice synthesizes");

        assert_eq!(pcm.sample_rate, SAMPLE_RATE);
        assert_eq!(pcm.channels, 1);
        assert!(
            pcm.samples.len() > SAMPLE_RATE as usize / 10,
            "a four-word utterance should be more than 100ms, got {} samples",
            pcm.samples.len()
        );
        assert!(
            pcm.samples.iter().any(|sample| *sample != 0.0),
            "the decoded audio must not be silence"
        );
    }

    #[test]
    fn an_empty_utterance_is_rejected_rather_than_spawning_a_process() {
        assert!(smol::block_on(speak("   ", None)).is_err());
    }

    /// An utterance starting with a dash must be spoken, not parsed as a flag.
    #[test]
    fn leading_dashes_are_not_read_as_flags() {
        if !std::path::Path::new("/usr/bin/say").exists() {
            return;
        }
        let pcm = smol::block_on(speak("-v nonsense is just text", None))
            .expect("a leading dash is text, not a flag");
        assert!(!pcm.samples.is_empty());
    }
}
