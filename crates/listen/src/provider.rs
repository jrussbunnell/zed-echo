use anyhow::Result;
#[cfg(any(test, feature = "test-support"))]
use anyhow::anyhow;
#[cfg(any(test, feature = "test-support"))]
use futures::StreamExt as _;
use futures::channel::mpsc;
use gpui::App;
#[cfg(any(test, feature = "test-support"))]
use gpui::AppContext as _;
#[cfg(any(test, feature = "test-support"))]
use std::collections::VecDeque;
#[cfg(any(test, feature = "test-support"))]
use std::sync::{Arc, Mutex};

/// One refinement of what the user said. A provider emits as many of these as
/// it likes and exactly one with `is_final` set.
#[derive(Debug, Clone, PartialEq)]
pub struct Transcript {
    pub text: String,
    pub is_final: bool,
}

/// One seam so no speech vendor is welded into the editor, mirroring
/// `read_aloud::TtsProvider` on the other half of the conversation.
pub trait SttProvider: Send + Sync + 'static {
    /// Transcribes one utterance from a stream of mono `f32` frames.
    ///
    /// Transcripts are yielded as the provider refines them, so a command can
    /// dispatch on the provider's own endpointing rather than after a full
    /// round trip. Closing `audio` signals the end of the utterance; dropping
    /// the returned receiver cancels the work.
    fn transcribe(
        &self,
        audio: mpsc::UnboundedReceiver<Vec<f32>>,
        cx: &App,
    ) -> mpsc::UnboundedReceiver<Result<Transcript>>;
}

#[cfg(any(test, feature = "test-support"))]
#[derive(Default)]
struct FakeSttState {
    queued: VecDeque<Vec<Result<Transcript>>>,
    fail_next: bool,
    heard: usize,
}

/// Test double. Replays scripted transcripts without touching a microphone or
/// the network, the way `read_aloud::FakeTts` replays synthesis.
#[cfg(any(test, feature = "test-support"))]
#[derive(Clone, Default)]
pub struct FakeStt {
    state: Arc<Mutex<FakeSttState>>,
}

#[cfg(any(test, feature = "test-support"))]
impl FakeStt {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queues one utterance answered by a single final transcript.
    pub fn queue_transcript(&self, text: &str) {
        self.queue(vec![Ok(Transcript {
            text: text.to_string(),
            is_final: true,
        })]);
    }

    /// Queues one utterance answered by a partial and then a final, so tests
    /// can exercise the refinement path a real provider takes.
    pub fn queue_partial_then_final(&self, partial: &str, final_text: &str) {
        self.queue(vec![
            Ok(Transcript {
                text: partial.to_string(),
                is_final: false,
            }),
            Ok(Transcript {
                text: final_text.to_string(),
                is_final: true,
            }),
        ]);
    }

    pub fn fail_next(&self) {
        match self.state.lock() {
            Ok(mut state) => state.fail_next = true,
            Err(error) => log::error!("listen: FakeStt state poisoned: {error}"),
        }
    }

    /// How many utterances this provider has been asked to transcribe.
    pub fn heard(&self) -> usize {
        self.state.lock().map(|state| state.heard).unwrap_or(0)
    }

    fn queue(&self, transcripts: Vec<Result<Transcript>>) {
        match self.state.lock() {
            Ok(mut state) => state.queued.push_back(transcripts),
            Err(error) => log::error!("listen: FakeStt state poisoned: {error}"),
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
impl SttProvider for FakeStt {
    fn transcribe(
        &self,
        mut audio: mpsc::UnboundedReceiver<Vec<f32>>,
        cx: &App,
    ) -> mpsc::UnboundedReceiver<Result<Transcript>> {
        let (sender, receiver) = mpsc::unbounded();
        let scripted = {
            let Ok(mut state) = self.state.lock() else {
                sender
                    .unbounded_send(Err(anyhow!("FakeStt state poisoned")))
                    .ok();
                return receiver;
            };
            state.heard += 1;
            if std::mem::take(&mut state.fail_next) {
                sender
                    .unbounded_send(Err(anyhow!("FakeStt was told to fail")))
                    .ok();
                return receiver;
            }
            state.queued.pop_front()
        };

        cx.background_spawn(async move {
            // Drain the frames so a caller that feeds audio is not left
            // pushing into a channel nobody reads. The utterance is over when
            // this returns.
            while audio.next().await.is_some() {}
            let Some(scripted) = scripted else {
                sender
                    .unbounded_send(Err(anyhow!("FakeStt had no transcript queued")))
                    .ok();
                return;
            };
            for transcript in scripted {
                if sender.unbounded_send(transcript).is_err() {
                    return;
                }
            }
        })
        .detach();

        receiver
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    async fn fake_provider_returns_the_queued_transcript(cx: &mut TestAppContext) {
        let provider = FakeStt::new();
        provider.queue_transcript("stop the agent");
        let (audio_sender, audio) = mpsc::unbounded();
        drop(audio_sender);

        let mut transcripts = cx.update(|cx| provider.transcribe(audio, cx));
        let first = transcripts.next().await.unwrap().unwrap();

        assert_eq!(first.text, "stop the agent");
        assert!(first.is_final);
        assert_eq!(provider.heard(), 1);
    }

    #[gpui::test]
    async fn fake_provider_streams_a_partial_before_the_final(cx: &mut TestAppContext) {
        let provider = FakeStt::new();
        provider.queue_partial_then_final("stop the", "stop the agent");
        let (audio_sender, audio) = mpsc::unbounded();
        drop(audio_sender);

        let mut transcripts = cx.update(|cx| provider.transcribe(audio, cx));
        let partial = transcripts.next().await.unwrap().unwrap();
        let final_transcript = transcripts.next().await.unwrap().unwrap();

        assert_eq!(partial.text, "stop the");
        assert!(!partial.is_final);
        assert_eq!(final_transcript.text, "stop the agent");
        assert!(final_transcript.is_final);
    }

    #[gpui::test]
    async fn fake_provider_can_be_told_to_fail(cx: &mut TestAppContext) {
        let provider = FakeStt::new();
        provider.fail_next();
        let (audio_sender, audio) = mpsc::unbounded();
        drop(audio_sender);

        let mut transcripts = cx.update(|cx| provider.transcribe(audio, cx));
        assert!(transcripts.next().await.unwrap().is_err());
    }
}
