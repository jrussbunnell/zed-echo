mod inworld;
mod player;
mod provider;
mod segmenter;
mod sink;

pub use inworld::{INWORLD_CREDENTIALS_URL, InworldTts, resolve_api_key};
pub use player::{Player, PlayerEvent};
pub use provider::{Pcm, TtsProvider};
pub use segmenter::{Utterance, segment};
pub use sink::{AudioSink, RodioSink};

// Test doubles are only part of the crate's public surface under
// `test-support`, so downstream crates (e.g. agent_ui's own tests) can build
// a `ReadAloud` without a network or an audio device, the same way this
// crate's own tests do.
#[cfg(any(test, feature = "test-support"))]
pub use provider::FakeTts;
#[cfg(any(test, feature = "test-support"))]
pub use sink::FakeSink;

use gpui::{AppContext as _, Context, Entity, Subscription, Task};
use markdown::Markdown;
use settings::{RegisterSetting, Settings};
use std::sync::Arc;
use std::time::Duration;

gpui::actions!(
    read_aloud,
    [
        /// Starts or stops reading the assistant's response aloud.
        Toggle,
        /// Pauses or resumes reading aloud, keeping the current position.
        TogglePause
    ]
);

#[derive(Clone, Debug, PartialEq, RegisterSetting)]
pub struct ReadAloudSettings {
    pub enabled: bool,
    pub auto_play: bool,
    pub provider: String,
    pub voice_id: String,
    pub model_id: String,
    pub speaking_rate: f32,
}

impl Settings for ReadAloudSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let read_aloud = content.read_aloud.as_ref();
        ReadAloudSettings {
            enabled: read_aloud.and_then(|s| s.enabled).unwrap_or(false),
            auto_play: read_aloud.and_then(|s| s.auto_play).unwrap_or(true),
            provider: read_aloud
                .and_then(|s| s.provider.clone())
                .unwrap_or_else(|| "inworld".to_string()),
            voice_id: read_aloud
                .and_then(|s| s.voice_id.clone())
                .unwrap_or_else(|| "Dennis".to_string()),
            model_id: read_aloud
                .and_then(|s| s.model_id.clone())
                .unwrap_or_else(|| "inworld-tts-2".to_string()),
            speaking_rate: read_aloud.and_then(|s| s.speaking_rate).unwrap_or(1.0),
        }
    }
}

pub fn init(cx: &mut gpui::App) {
    ReadAloudSettings::register(cx);
}

/// How often the player's queue depth is sampled to advance the highlight.
/// rodio reports depth but emits no completion callback, so this is polled.
const POSITION_POLL_INTERVAL: Duration = Duration::from_millis(100);

pub struct ReadAloud {
    player: Entity<Player>,
    /// The markdown entity currently being spoken, and the utterances derived
    /// from it. Kept together so the highlight can be cleared on switch.
    speaking: Option<Entity<Markdown>>,
    poll_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl ReadAloud {
    #[cfg(any(test, feature = "test-support"))]
    pub fn for_test(
        provider: Arc<dyn TtsProvider>,
        sink: Box<dyn AudioSink>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::build(provider, sink, cx)
    }

    pub fn new(
        provider: Arc<dyn TtsProvider>,
        sink: Box<dyn AudioSink>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::build(provider, sink, cx)
    }

    fn build(
        provider: Arc<dyn TtsProvider>,
        sink: Box<dyn AudioSink>,
        cx: &mut Context<Self>,
    ) -> Self {
        let player = cx.new(|cx| Player::new(provider, sink, cx));
        let subscription = cx.subscribe(&player, |this, _player, event, cx| match event {
            PlayerEvent::Speaking(index) => this.highlight_utterance(Some(*index), cx),
            PlayerEvent::Finished => this.highlight_utterance(None, cx),
        });

        Self {
            player,
            speaking: None,
            poll_task: None,
            _subscriptions: vec![subscription],
        }
    }

    /// Segments a markdown entity and hands the utterances to the player.
    /// Safe to call repeatedly as content streams in — the player only
    /// synthesizes what it has not already queued.
    pub fn enqueue_markdown(
        &mut self,
        markdown: &Entity<Markdown>,
        message_complete: bool,
        cx: &mut Context<Self>,
    ) {
        let switched_entity = self.speaking.as_ref() != Some(markdown);
        if switched_entity {
            self.clear_highlight(cx);
            self.speaking = Some(markdown.clone());
        }

        let utterances = segment(markdown.read(cx).parsed_markdown(), message_complete);
        self.player.update(cx, |player, cx| {
            player.set_utterances(utterances, cx);
            if switched_entity {
                // `set_utterances` alone is not enough on a switch: it only
                // clamps `next_to_synthesize` down when it exceeds the new
                // utterance count, so if the new message happens to have at
                // least as many utterances as the old one had already
                // reached, `pump` thinks synthesis is caught up and the new
                // message is never synthesized while the old message's
                // audio keeps playing from the sink. `seek_to(0, ..)` clears
                // the sink (dropping the old message's queued audio),
                // cancels any in-flight synthesis for the old message, and
                // restarts `next_to_synthesize` from the top of the new one.
                // A plain `stop` before `set_utterances` does not work
                // either — `set_utterances` would just clamp the resulting
                // `next_to_synthesize` right back down to the new length.
                player.seek_to(0, cx);
            }
        });
        self.start_polling(cx);
    }

    pub fn seek_to_source_index(
        &mut self,
        markdown: &Entity<Markdown>,
        source_index: usize,
        cx: &mut Context<Self>,
    ) {
        if self.speaking.as_ref() != Some(markdown) {
            self.enqueue_markdown(markdown, false, cx);
        }

        let target = self
            .player
            .read(cx)
            .utterances()
            .iter()
            .position(|utterance| {
                utterance.source_range.contains(&source_index)
                    || utterance.source_range.start > source_index
            });

        if let Some(index) = target {
            self.player
                .update(cx, |player, cx| player.seek_to(index, cx));
            self.start_polling(cx);
        }
    }

    pub fn toggle(&mut self, cx: &mut Context<Self>) {
        if self.is_speaking() {
            // `stop` emits `Finished`, and this entity's own subscription to
            // the player already clears the highlight in response — no need
            // to do it again here.
            self.player.update(cx, |player, cx| player.stop(cx));
            self.poll_task = None;
            // `self.speaking` is deliberately retained so toggling back on has
            // a message to restart.
        } else if self.speaking.is_some() {
            // Restart from the top. `stop` discarded the queue position, and
            // resuming mid-sentence would need an offset into audio the sink
            // no longer holds.
            self.player.update(cx, |player, cx| player.seek_to(0, cx));
            self.start_polling(cx);
        }
    }

    /// Pause/resume, holding queue position — the counterpart to `toggle`,
    /// which stops and restarts from the top.
    pub fn toggle_pause(&mut self, cx: &mut Context<Self>) {
        self.player.update(cx, |player, _cx| {
            if player.is_paused() {
                player.resume();
            } else {
                player.pause();
            }
        });
    }

    pub fn set_speed(&mut self, speed: f32, cx: &mut Context<Self>) {
        self.player
            .update(cx, |player, _cx| player.set_speed(speed));
    }

    pub fn is_speaking(&self) -> bool {
        self.poll_task.is_some()
    }

    fn start_polling(&mut self, cx: &mut Context<Self>) {
        if self.poll_task.is_some() {
            return;
        }
        self.poll_task = Some(cx.spawn(async move |this, cx| {
            loop {
                let Ok(still_playing) = this.update(cx, |this, cx| {
                    this.player.update(cx, |player, cx| {
                        player.poll_position(cx);
                        player.speaking_index().is_some()
                    })
                }) else {
                    return;
                };
                if !still_playing {
                    this.update(cx, |this, _cx| this.poll_task = None).ok();
                    return;
                }
                cx.background_executor().timer(POSITION_POLL_INTERVAL).await;
            }
        }));
    }

    fn highlight_utterance(&mut self, index: Option<usize>, cx: &mut Context<Self>) {
        let Some(markdown) = self.speaking.clone() else {
            return;
        };
        let range = index.and_then(|index| {
            self.player
                .read(cx)
                .utterances()
                .get(index)
                .map(|utterance| utterance.source_range.clone())
        });
        markdown.update(cx, |markdown, cx| {
            markdown.set_speaking_highlight(range, cx);
        });
    }

    fn clear_highlight(&mut self, cx: &mut Context<Self>) {
        if let Some(markdown) = self.speaking.clone() {
            markdown.update(cx, |markdown, cx| {
                markdown.set_speaking_highlight(None, cx);
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::FakeTts;
    use crate::sink::FakeSink;
    use gpui::TestAppContext;
    use markdown::Markdown;

    #[test]
    fn defaults_are_inert_but_autoplay_once_enabled() {
        let settings = ReadAloudSettings::from_settings(&settings::SettingsContent::default());
        assert!(!settings.enabled, "feature must be off on a fresh profile");
        assert!(
            settings.auto_play,
            "auto_play describes behavior once enabled"
        );
        assert_eq!(settings.provider, "inworld");
        assert_eq!(settings.voice_id, "Dennis");
        assert_eq!(settings.model_id, "inworld-tts-2");
        assert_eq!(settings.speaking_rate, 1.0);
    }

    #[gpui::test]
    async fn speaks_a_markdown_entity_and_highlights_the_current_sentence(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let markdown =
            cx.new(|cx| Markdown::new("First one. Second one.\n".into(), None, None, cx));
        cx.run_until_parked();

        let read_aloud = cx.new({
            let provider = provider.clone();
            |cx| ReadAloud::for_test(Arc::new(provider), Box::new(sink), cx)
        });

        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.enqueue_markdown(&markdown, false, cx);
        });
        cx.run_until_parked();

        assert_eq!(provider.spoken(), vec!["First one.", "Second one."]);
        assert_eq!(
            markdown.read_with(cx, |markdown, _| markdown.speaking_highlight().cloned()),
            Some(0..10),
            "the first sentence should be highlighted"
        );
    }

    #[gpui::test]
    async fn enqueuing_a_different_entity_speaks_it_and_never_highlights_the_old_one(
        cx: &mut TestAppContext,
    ) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let markdown_a =
            cx.new(|cx| Markdown::new("One. Two. Three. Four. Five.\n".into(), None, None, cx));
        let markdown_b = cx.new(|cx| Markdown::new("Alpha. Beta.\n".into(), None, None, cx));
        cx.run_until_parked();

        let read_aloud = cx.new({
            let provider = provider.clone();
            |cx| ReadAloud::for_test(Arc::new(provider), Box::new(sink), cx)
        });

        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.enqueue_markdown(&markdown_a, false, cx);
        });
        cx.run_until_parked();

        // Switch to a different entity while `markdown_a` is still mid-queue
        // (the prefetch window only synthesizes 2 of its 5 utterances).
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.enqueue_markdown(&markdown_b, false, cx);
        });
        cx.run_until_parked();

        assert!(
            provider
                .spoken()
                .ends_with(&["Alpha.".to_string(), "Beta.".to_string()]),
            "the new message must still be synthesized after switching entities, got {:?}",
            provider.spoken()
        );
        assert_eq!(
            markdown_a.read_with(cx, |markdown, _| markdown.speaking_highlight().cloned()),
            None,
            "the old message must never be highlighted after switching away from it"
        );
        assert_eq!(
            markdown_b.read_with(cx, |markdown, _| markdown.speaking_highlight().cloned()),
            Some(0..6),
            "the new message's first sentence should be highlighted"
        );
    }

    #[gpui::test]
    async fn clicking_a_sentence_seeks_to_it(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let markdown =
            cx.new(|cx| Markdown::new("First one. Second one.\n".into(), None, None, cx));
        cx.run_until_parked();

        let read_aloud = cx.new(|cx| ReadAloud::for_test(Arc::new(provider), Box::new(sink), cx));
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.enqueue_markdown(&markdown, false, cx);
        });
        cx.run_until_parked();

        // Byte 13 falls inside "Second one."
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.seek_to_source_index(&markdown, 13, cx);
        });
        cx.run_until_parked();

        assert_eq!(
            markdown.read_with(cx, |markdown, _| markdown.speaking_highlight().cloned()),
            Some(11..22),
            "clicking the second sentence should move the highlight there"
        );
    }

    #[gpui::test]
    async fn toggle_stops_then_restarts(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let markdown = cx.new(|cx| Markdown::new("Only one.\n".into(), None, None, cx));
        cx.run_until_parked();

        let read_aloud = cx.new(|cx| ReadAloud::for_test(Arc::new(provider), Box::new(sink), cx));
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.enqueue_markdown(&markdown, false, cx);
        });
        cx.run_until_parked();
        assert!(read_aloud.read_with(cx, |read_aloud, _| read_aloud.is_speaking()));

        read_aloud.update(cx, |read_aloud, cx| read_aloud.toggle(cx));
        cx.run_until_parked();

        assert!(!read_aloud.read_with(cx, |read_aloud, _| read_aloud.is_speaking()));
        assert_eq!(
            markdown.read_with(cx, |markdown, _| markdown.speaking_highlight().cloned()),
            None
        );

        read_aloud.update(cx, |read_aloud, cx| read_aloud.toggle(cx));
        cx.run_until_parked();

        assert!(
            read_aloud.read_with(cx, |read_aloud, _| read_aloud.is_speaking()),
            "toggling back on must restart the message, not latch off"
        );
        assert_eq!(
            markdown.read_with(cx, |markdown, _| markdown.speaking_highlight().cloned()),
            Some(0..9),
            "restart begins at the first sentence"
        );
    }

    #[gpui::test]
    async fn toggle_pause_holds_position_and_highlight(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let markdown =
            cx.new(|cx| Markdown::new("First one. Second one.\n".into(), None, None, cx));
        cx.run_until_parked();

        let read_aloud = cx.new({
            let sink = sink.clone();
            |cx| ReadAloud::for_test(Arc::new(provider), Box::new(sink), cx)
        });
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.enqueue_markdown(&markdown, false, cx);
        });
        cx.run_until_parked();

        read_aloud.update(cx, |read_aloud, cx| read_aloud.toggle_pause(cx));
        assert!(sink.is_paused());
        assert_eq!(
            markdown.read_with(cx, |markdown, _| markdown.speaking_highlight().cloned()),
            Some(0..10),
            "pausing keeps the highlight in place"
        );

        read_aloud.update(cx, |read_aloud, cx| read_aloud.toggle_pause(cx));
        assert!(!sink.is_paused());
        assert_eq!(
            markdown.read_with(cx, |markdown, _| markdown.speaking_highlight().cloned()),
            Some(0..10)
        );
    }
}
