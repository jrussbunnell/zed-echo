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
    /// Set when the user stops playback with `toggle`, cleared by anything that
    /// starts it again. Without it a stop does not stick while a message is
    /// still streaming: the next `enqueue_markdown` grows the utterance list
    /// past the cursor `stop` parked at the old end, and the player starts
    /// speaking again without being asked.
    stopped_by_user: bool,
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
            stopped_by_user: false,
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
            self.stopped_by_user = false;
        } else if self.stopped_by_user {
            // The user stopped this message. More of it arriving is not a
            // reason to start speaking again; only `toggle` or a seek is.
            return;
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
                // audio keeps playing from the sink. `reset` clears the sink
                // (dropping the old message's queued audio), cancels any
                // in-flight synthesis for the old message, and restarts
                // `next_to_synthesize` from the top of the new one.
                //
                // `seek_to(0, ..)` cannot be used here: it early-returns
                // whenever the target index is out of range, which is
                // exactly the state of the very first chunk of a new
                // streaming message — the segmenter withholds an
                // unterminated trailing fragment, so a fresh entity often
                // starts out with zero utterances. `seek_to` would then
                // leave the old message's stale audio sitting in the sink
                // until the new message finally produces its first
                // utterance. `reset` has no such guard: it always clears the
                // sink and always pumps, whether or not there is anything to
                // synthesize yet.
                //
                // A plain `stop` before `set_utterances` does not work
                // either — `set_utterances` would just clamp the resulting
                // `next_to_synthesize` right back down to the new length.
                player.reset(cx);
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
        // Clicking a sentence is a request to hear it, which overrides an
        // earlier stop.
        self.stopped_by_user = false;
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
            self.stopped_by_user = true;
            // `self.speaking` is deliberately retained so toggling back on has
            // a message to restart.
        } else if self.speaking.is_some() {
            // Restart from the top. `stop` discarded the queue position, and
            // resuming mid-sentence would need an offset into audio the sink
            // no longer holds.
            self.stopped_by_user = false;
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

    /// The message the reader is holding, whether or not it is sounding right
    /// now. Callers use this to tell "restart what is loaded" from "load
    /// something else".
    pub fn speaking(&self) -> Option<&Entity<Markdown>> {
        self.speaking.as_ref()
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
    async fn switching_to_an_entity_with_no_utterances_yet_still_drops_the_old_queue(
        cx: &mut TestAppContext,
    ) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let markdown_a =
            cx.new(|cx| Markdown::new("One. Two. Three. Four. Five.\n".into(), None, None, cx));
        // `markdown_b` starts mid-sentence, with no terminator yet — the
        // segmenter withholds an unterminated trailing fragment, so this is
        // the normal state of the very first chunk of a new streaming
        // message: zero utterances.
        let markdown_b = cx.new(|cx| Markdown::new("Alpha".into(), None, None, cx));
        cx.run_until_parked();

        let read_aloud = cx.new({
            let provider = provider.clone();
            let sink = sink.clone();
            |cx| ReadAloud::for_test(Arc::new(provider), Box::new(sink), cx)
        });

        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.enqueue_markdown(&markdown_a, false, cx);
        });
        cx.run_until_parked();
        assert!(sink.queued() > 0, "setup: A should have queued audio");

        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.enqueue_markdown(&markdown_b, false, cx);
        });
        assert_eq!(
            sink.queued(),
            0,
            "the old message's stale audio must be dropped even though the new \
             message has nothing to speak yet"
        );

        // "Alpha." finishes streaming in.
        markdown_b.update(cx, |markdown, cx| {
            markdown.append(". Beta.\n", cx);
        });
        cx.run_until_parked();
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.enqueue_markdown(&markdown_b, false, cx);
        });
        cx.run_until_parked();

        assert!(
            provider
                .spoken()
                .ends_with(&["Alpha.".to_string(), "Beta.".to_string()]),
            "the new message must be synthesized promptly once it has content, \
             with no need to wait for a drain, got {:?}",
            provider.spoken()
        );
        assert_eq!(
            markdown_b.read_with(cx, |markdown, _| markdown.speaking_highlight().cloned()),
            Some(0..6),
            "Speaking(0) must be re-emitted for the new message even though its \
             index collides with the position last reported for the old one"
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
    async fn a_user_stop_survives_more_of_the_same_message_streaming_in(
        cx: &mut TestAppContext,
    ) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let markdown = cx.new(|cx| Markdown::new("First one.\n".into(), None, None, cx));
        cx.run_until_parked();

        let read_aloud = cx.new({
            let provider = provider.clone();
            |cx| ReadAloud::for_test(Arc::new(provider), Box::new(sink), cx)
        });
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.enqueue_markdown(&markdown, false, cx);
        });
        cx.run_until_parked();
        assert!(read_aloud.read_with(cx, |read_aloud, _| read_aloud.is_speaking()));

        read_aloud.update(cx, |read_aloud, cx| read_aloud.toggle(cx));
        cx.run_until_parked();
        let spoken_when_stopped = provider.spoken();
        assert!(!read_aloud.read_with(cx, |read_aloud, _| read_aloud.is_speaking()));

        // The agent is still writing, so the same message keeps arriving.
        markdown.update(cx, |markdown, cx| {
            markdown.append("Second one. Third one.\n", cx);
        });
        cx.run_until_parked();
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.enqueue_markdown(&markdown, true, cx);
        });
        cx.run_until_parked();

        assert!(
            !read_aloud.read_with(cx, |read_aloud, _| read_aloud.is_speaking()),
            "a stop must not be undone by the rest of the message streaming in"
        );
        assert_eq!(
            provider.spoken(),
            spoken_when_stopped,
            "nothing new may be synthesized while stopped"
        );
        assert_eq!(
            markdown.read_with(cx, |markdown, _| markdown.speaking_highlight().cloned()),
            None,
            "and nothing may be highlighted"
        );

        read_aloud.update(cx, |read_aloud, cx| read_aloud.toggle(cx));
        cx.run_until_parked();
        assert!(
            read_aloud.read_with(cx, |read_aloud, _| read_aloud.is_speaking()),
            "toggling back on must still work after a stop that stuck"
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
