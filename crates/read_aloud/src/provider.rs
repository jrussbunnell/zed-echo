use anyhow::{Result, anyhow};
use gpui::{App, Task};
use std::sync::{Arc, Mutex};

/// Raw uncompressed audio. Interleaved if `channels > 1`.
#[derive(Debug, Clone, PartialEq)]
pub struct Pcm {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
    pub channels: u16,
}

/// One seam so no TTS vendor is welded into the editor.
pub trait TtsProvider: Send + Sync + 'static {
    fn synthesize(&self, text: String, cx: &App) -> Task<Result<Pcm>>;
}

#[derive(Default)]
struct FakeTtsState {
    spoken: Vec<String>,
    fail_next: bool,
}

/// Test double. Produces one silent sample per character so queue ordering
/// and duration are predictable without touching the network.
#[derive(Clone, Default)]
pub struct FakeTts {
    state: Arc<Mutex<FakeTtsState>>,
}

impl FakeTts {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn spoken(&self) -> Vec<String> {
        self.state
            .lock()
            .map(|state| state.spoken.clone())
            .unwrap_or_default()
    }

    pub fn fail_next(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.fail_next = true;
        }
    }
}

impl TtsProvider for FakeTts {
    fn synthesize(&self, text: String, _cx: &App) -> Task<Result<Pcm>> {
        let Ok(mut state) = self.state.lock() else {
            return Task::ready(Err(anyhow!("FakeTts state poisoned")));
        };
        if std::mem::take(&mut state.fail_next) {
            return Task::ready(Err(anyhow!("FakeTts was told to fail")));
        }
        state.spoken.push(text.clone());
        Task::ready(Ok(Pcm {
            samples: vec![0.0; text.chars().count()],
            sample_rate: 22050,
            channels: 1,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    async fn fake_provider_returns_one_sample_per_character(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let pcm = cx
            .update(|cx| provider.synthesize("hello".to_string(), cx))
            .await
            .unwrap();
        assert_eq!(pcm.samples.len(), 5);
        assert_eq!(pcm.sample_rate, 22050);
        assert_eq!(pcm.channels, 1);
    }

    #[gpui::test]
    async fn fake_provider_records_what_it_was_asked_to_say(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        cx.update(|cx| provider.synthesize("first".to_string(), cx))
            .await
            .unwrap();
        cx.update(|cx| provider.synthesize("second".to_string(), cx))
            .await
            .unwrap();
        assert_eq!(provider.spoken(), vec!["first", "second"]);
    }

    #[gpui::test]
    async fn fake_provider_can_be_told_to_fail(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        provider.fail_next();
        let result = cx
            .update(|cx| provider.synthesize("doomed".to_string(), cx))
            .await;
        assert!(result.is_err());
    }
}
