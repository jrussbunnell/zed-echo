mod inworld;
mod player;
mod provider;
mod segmenter;
mod sink;

pub use inworld::{INWORLD_CREDENTIALS_URL, InworldTts, resolve_api_key};
pub use player::{Player, PlayerEvent};
pub use provider::{Pcm, TtsProvider, WordTiming};
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

use gpui::{App, AppContext as _, Context, Entity, SharedString, Subscription, Task};
use markdown::Markdown;
use settings::{RegisterSetting, Settings};
use std::ops::Range;
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

/// How often the player's position is sampled to advance the highlights.
/// rodio reports position but emits no completion callback, so this is
/// polled — fast enough that the word highlight tracks speech smoothly
/// (spoken words last a few hundred milliseconds each).
const POSITION_POLL_INTERVAL: Duration = Duration::from_millis(30);

/// A snapshot of the player, thin enough to derive on every render, for the
/// mini player UI. `None` while there is nothing to control.
#[derive(Clone, Debug, PartialEq)]
pub struct PlaybackState {
    pub utterance_index: usize,
    pub utterance_count: usize,
    pub paused: bool,
    pub sentence_text: SharedString,
}

pub struct ReadAloud {
    player: Entity<Player>,
    /// The markdown entity currently being spoken, and the utterances derived
    /// from it. Kept together so the highlight can be cleared on switch.
    speaking: Option<Entity<Markdown>>,
    /// Messages that arrived while another was still sounding. Agent turns
    /// emit a fresh markdown entity after every tool call; switching the
    /// player the moment one appeared used to truncate the sentence being
    /// spoken. Instead the newcomers wait here (FIFO) until the player runs
    /// out of work for the current entity. Only bookkeeping lives here — the
    /// entity's content is re-read at switch time, and highlights are only
    /// ever driven by `speaking`, never by this queue.
    pending: Vec<(Entity<Markdown>, bool)>,
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
        // The mini player renders from `playback_state`, so anything watching
        // this entity must wake whenever the underlying player changes.
        let player_observation = cx.observe(&player, |_, _, cx| cx.notify());

        Self {
            player,
            speaking: None,
            pending: Vec::new(),
            stopped_by_user: false,
            poll_task: None,
            _subscriptions: vec![subscription, player_observation],
        }
    }

    /// Segments a markdown entity and hands the utterances to the player.
    /// Safe to call repeatedly as content streams in — the player only
    /// synthesizes what it has not already queued. A *different* entity
    /// arriving while the current one still has audio or synthesis under way
    /// does not interrupt it: the newcomer waits in a FIFO until the current
    /// entity's utterances finish (see `pending`).
    pub fn enqueue_markdown(
        &mut self,
        markdown: &Entity<Markdown>,
        message_complete: bool,
        cx: &mut Context<Self>,
    ) {
        let switched_entity = self.speaking.as_ref() != Some(markdown);
        if switched_entity {
            if self.stopped_by_user {
                // The user's stop outlives the block that was sounding when
                // they pressed it: one agent turn keeps arriving as fresh
                // entities (a new one after every tool call), and none of
                // them may restart speech — only `toggle` or a seek lifts
                // the latch. The newest entity is still tracked (quietly,
                // with the player left empty and silent) so an explicit
                // toggle later restarts from the message the user actually
                // sees, exactly as it does after a same-entity stop.
                self.clear_highlight(cx);
                self.speaking = Some(markdown.clone());
                self.player.update(cx, |player, cx| {
                    player.set_utterances(Vec::new(), cx);
                    player.reset(cx);
                });
                return;
            }
            if self.speaking.is_some() && !self.player.read(cx).is_idle() {
                if let Some(entry) = self
                    .pending
                    .iter_mut()
                    .find(|(pending, _)| pending == markdown)
                {
                    entry.1 = message_complete;
                } else {
                    log::debug!("read_aloud: queued a new entity while another is still speaking");
                    self.pending.push((markdown.clone(), message_complete));
                }
                return;
            }
            self.switch_to(markdown.clone(), message_complete, cx);
            return;
        }
        if self.stopped_by_user {
            // The user stopped this message. More of it arriving is not a
            // reason to start speaking again; only `toggle` or a seek is.
            return;
        }
        self.refresh_current(message_complete, cx);
    }

    /// Makes `markdown` the speaking entity and restarts the player from its
    /// top, dropping whatever the previous entity still had queued. The
    /// pending FIFO is left alone: callers decide whether the switch consumes
    /// it (finishing naturally) or overrides it (explicit user intent).
    fn switch_to(
        &mut self,
        markdown: Entity<Markdown>,
        message_complete: bool,
        cx: &mut Context<Self>,
    ) {
        self.clear_highlight(cx);
        self.speaking = Some(markdown);
        self.stopped_by_user = false;

        let Some(markdown) = self.speaking.clone() else {
            return;
        };
        let utterances = segment(markdown.read(cx).parsed_markdown(), message_complete);
        self.player.update(cx, |player, cx| {
            player.set_utterances(utterances, cx);
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
        });
        self.push_speakable_ranges(cx);
        self.start_polling(cx);
    }

    /// Re-segments the current entity in place — the streaming-append path.
    fn refresh_current(&mut self, message_complete: bool, cx: &mut Context<Self>) {
        let Some(markdown) = self.speaking.clone() else {
            return;
        };
        let utterances = segment(markdown.read(cx).parsed_markdown(), message_complete);
        self.player
            .update(cx, |player, cx| player.set_utterances(utterances, cx));
        self.push_speakable_ranges(cx);
        self.start_polling(cx);
    }

    /// Mirrors the current utterance ranges into the speaking entity so its
    /// element can preview (hover) and advertise (cursor) click-to-seek.
    fn push_speakable_ranges(&self, cx: &mut Context<Self>) {
        let Some(markdown) = self.speaking.clone() else {
            return;
        };
        let ranges: Vec<Range<usize>> = self
            .player
            .read(cx)
            .utterances()
            .iter()
            .map(|utterance| utterance.source_range.clone())
            .collect();
        markdown.update(cx, |markdown, cx| {
            markdown.set_speakable_ranges(ranges, cx);
        });
    }

    pub fn seek_to_source_index(
        &mut self,
        markdown: &Entity<Markdown>,
        source_index: usize,
        cx: &mut Context<Self>,
    ) {
        // A click is explicit intent: whatever was waiting its turn is
        // overruled by it.
        self.pending.clear();
        if self.speaking.as_ref() != Some(markdown) {
            self.switch_to(markdown.clone(), false, cx);
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
            // Clicking a sentence is a request to hear it, which overrides an
            // earlier stop. Only a click that lands on something speakable
            // counts: clicking past the last utterance — a trailing code
            // block, a table, or the tail the segmenter is still withholding —
            // does nothing audible, and must not quietly re-arm a message the
            // user stopped.
            self.stopped_by_user = false;
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
            // A stop is explicit intent about the whole session, not just the
            // current message: nothing waiting its turn may start either.
            self.pending.clear();
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
        self.player.update(cx, |player, cx| {
            if player.is_paused() {
                player.resume();
            } else {
                player.pause();
            }
            // Pause state lives in the sink, which cannot notify; the mini
            // player's play/pause glyph re-renders off this.
            cx.notify();
        });
    }

    /// What the mini player needs to render, `None` when it should be
    /// hidden: nothing loaded, playback finished, or stopped.
    pub fn playback_state(&self, cx: &App) -> Option<PlaybackState> {
        self.speaking.as_ref()?;
        let player = self.player.read(cx);
        let utterance_count = player.utterances().len();
        let utterance_index = player.speaking_index().or_else(|| {
            // The sink can be momentarily empty while synthesis catches up;
            // the utterance being synthesized stands in so the controls do
            // not blink out mid-message.
            (!player.is_idle() && utterance_count > 0)
                .then(|| player.next_to_synthesize().min(utterance_count - 1))
        })?;
        let utterance = player.utterances().get(utterance_index)?;
        Some(PlaybackState {
            utterance_index,
            utterance_count,
            paused: player.is_paused(),
            sentence_text: SharedString::new(utterance.spoken_text.as_str()),
        })
    }

    /// Seeks one utterance back, clamped at the start.
    pub fn previous_sentence(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.playback_state(cx) else {
            return;
        };
        let Some(target) = state.utterance_index.checked_sub(1) else {
            return;
        };
        self.player
            .update(cx, |player, cx| player.seek_to(target, cx));
        self.start_polling(cx);
    }

    /// Seeks one utterance forward, clamped at the end.
    pub fn next_sentence(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.playback_state(cx) else {
            return;
        };
        let target = state.utterance_index + 1;
        if target >= state.utterance_count {
            return;
        }
        self.player
            .update(cx, |player, cx| player.seek_to(target, cx));
        self.start_polling(cx);
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
                let Ok(keep_polling) = this.update(cx, |this, cx| {
                    let (word_range, idle) = this.player.update(cx, |player, cx| {
                        player.poll_position(cx);
                        (player.current_word_source_range(), player.is_idle())
                    });
                    this.highlight_word(word_range, cx);
                    // The loop lives as long as the player has *work* — audio
                    // queued, synthesis in flight, or utterances awaiting
                    // synthesis — not merely audio. A momentarily-empty sink
                    // (playback outrunning a slow stream or a slow provider)
                    // used to kill the loop here, and with it the only thing
                    // that pumps synthesis once the prefetched audio drained:
                    // playback then stalled mid-message until the next
                    // enqueue happened to arrive.
                    if !idle {
                        return true;
                    }
                    if !this.pending.is_empty() {
                        let (markdown, message_complete) = this.pending.remove(0);
                        log::debug!("read_aloud: switched to a queued entity");
                        this.switch_to(markdown, message_complete, cx);
                        return true;
                    }
                    this.poll_task = None;
                    false
                }) else {
                    return;
                };
                if !keep_polling {
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
            let sentence_gone = range.is_none();
            markdown.set_speaking_highlight(range, cx);
            // Playback finishing must not strand a lit word; while a sentence
            // is still speaking, the word layer is driven by the poll loop.
            if sentence_gone {
                markdown.set_speaking_word_highlight(None, cx);
            }
        });
    }

    /// Word-level companion to `highlight_utterance`, driven every poll tick
    /// rather than on utterance changes. The markdown entity only notifies
    /// when the range actually changes, so ticking this at the poll rate is
    /// cheap while the same word keeps sounding.
    fn highlight_word(&mut self, range: Option<Range<usize>>, cx: &mut Context<Self>) {
        if let Some(markdown) = self.speaking.clone() {
            markdown.update(cx, |markdown, cx| {
                markdown.set_speaking_word_highlight(range, cx);
            });
        }
    }

    fn clear_highlight(&mut self, cx: &mut Context<Self>) {
        if let Some(markdown) = self.speaking.clone() {
            markdown.update(cx, |markdown, cx| {
                markdown.set_speaking_highlight(None, cx);
                markdown.set_speaking_word_highlight(None, cx);
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
    async fn a_different_entity_waits_its_turn_instead_of_truncating_speech(
        cx: &mut TestAppContext,
    ) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let markdown_a = cx.new(|cx| Markdown::new("One. Two.\n".into(), None, None, cx));
        let markdown_b = cx.new(|cx| Markdown::new("Alpha. Beta.\n".into(), None, None, cx));
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
        let queued_before = sink.queued();
        assert!(queued_before > 0, "setup: A should have queued audio");

        // B arrives mid-speech — the agent moved on to its next message
        // block — and must wait rather than cut A off.
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.enqueue_markdown(&markdown_b, false, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            sink.queued(),
            queued_before,
            "A's queued audio must not be dropped when B arrives"
        );
        assert_eq!(
            provider.spoken(),
            vec!["One.", "Two."],
            "B must not be synthesized while A is still speaking"
        );
        assert_eq!(
            markdown_a.read_with(cx, |markdown, _| markdown.speaking_highlight().cloned()),
            Some(0..4),
            "A keeps the highlight while B waits"
        );

        // A finishes; the queue pops and B speaks from its start.
        sink.finish_one();
        sink.finish_one();
        cx.executor().advance_clock(POSITION_POLL_INTERVAL);
        cx.run_until_parked();
        cx.executor().advance_clock(POSITION_POLL_INTERVAL);
        cx.run_until_parked();

        assert!(
            provider
                .spoken()
                .ends_with(&["Alpha.".to_string(), "Beta.".to_string()]),
            "B must be synthesized once A finishes, got {:?}",
            provider.spoken()
        );
        assert_eq!(
            markdown_a.read_with(cx, |markdown, _| markdown.speaking_highlight().cloned()),
            None,
            "the finished message must never stay highlighted"
        );
        assert_eq!(
            markdown_b.read_with(cx, |markdown, _| markdown.speaking_highlight().cloned()),
            Some(0..6),
            "the popped message's first sentence should be highlighted"
        );
    }

    #[gpui::test]
    async fn a_click_switch_to_an_entity_with_no_utterances_yet_still_drops_the_old_queue(
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

        // A click is explicit intent, so unlike a mid-speech enqueue it
        // switches immediately.
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.seek_to_source_index(&markdown_b, 0, cx);
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
    async fn word_highlight_tracks_the_playback_position(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        provider.emit_word_timings();
        let sink = FakeSink::new();
        let markdown = cx.new(|cx| Markdown::new("Alpha beta.\n".into(), None, None, cx));
        cx.run_until_parked();

        let read_aloud = cx.new({
            let sink = sink.clone();
            |cx| ReadAloud::for_test(Arc::new(provider), Box::new(sink), cx)
        });
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.enqueue_markdown(&markdown, false, cx);
        });
        cx.run_until_parked();
        cx.executor().advance_clock(POSITION_POLL_INTERVAL);
        cx.run_until_parked();

        assert_eq!(
            markdown.read_with(cx, |markdown, _| markdown
                .speaking_word_highlight()
                .cloned()),
            Some(0..5),
            "at position zero the first word is lit"
        );

        // FakeTts words are 100ms apart; 150ms is inside the second word.
        sink.set_position(Duration::from_millis(150));
        cx.executor().advance_clock(POSITION_POLL_INTERVAL);
        cx.run_until_parked();
        assert_eq!(
            markdown.read_with(cx, |markdown, _| markdown
                .speaking_word_highlight()
                .cloned()),
            Some(6..11),
            "the pill moves to 'beta.' as playback advances"
        );

        sink.finish_one();
        cx.executor().advance_clock(POSITION_POLL_INTERVAL);
        cx.run_until_parked();
        assert_eq!(
            markdown.read_with(cx, |markdown, _| markdown
                .speaking_word_highlight()
                .cloned()),
            None,
            "finishing playback must not strand a lit word"
        );
        assert_eq!(
            markdown.read_with(cx, |markdown, _| markdown.speaking_highlight().cloned()),
            None
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
    async fn a_user_stop_survives_more_of_the_same_message_streaming_in(cx: &mut TestAppContext) {
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
    async fn a_click_that_lands_on_nothing_speakable_does_not_lift_a_user_stop(
        cx: &mut TestAppContext,
    ) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let source = "First one. Second one.\n\n```\nlet x = 1;\n```\n";
        let markdown = cx.new(|cx| Markdown::new(source.into(), None, None, cx));
        cx.run_until_parked();

        let read_aloud = cx.new({
            let provider = provider.clone();
            |cx| ReadAloud::for_test(Arc::new(provider), Box::new(sink), cx)
        });
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.enqueue_markdown(&markdown, false, cx);
        });
        cx.run_until_parked();

        read_aloud.update(cx, |read_aloud, cx| read_aloud.toggle(cx));
        cx.run_until_parked();
        let spoken_when_stopped = provider.spoken();
        assert!(!read_aloud.read_with(cx, |read_aloud, _| read_aloud.is_speaking()));

        // Code blocks are never spoken, so a click inside one falls past the
        // last utterance and there is nothing to seek to.
        let dead_click = source.find("let x").expect("test source has a code block");
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.seek_to_source_index(&markdown, dead_click, cx);
        });
        cx.run_until_parked();
        assert!(
            !read_aloud.read_with(cx, |read_aloud, _| read_aloud.is_speaking()),
            "a click with nothing to seek to must not start playback"
        );

        // The agent keeps writing. The stop must still hold.
        markdown.update(cx, |markdown, cx| {
            markdown.append("Third one. Fourth one.\n", cx);
        });
        cx.run_until_parked();
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.enqueue_markdown(&markdown, true, cx);
        });
        cx.run_until_parked();

        assert!(
            !read_aloud.read_with(cx, |read_aloud, _| read_aloud.is_speaking()),
            "a click that did nothing must not have re-armed the stopped message"
        );
        assert_eq!(
            provider.spoken(),
            spoken_when_stopped,
            "nothing new may be synthesized while stopped"
        );

        // A click that does land on a sentence is a real request to hear it.
        let live_click = source
            .find("Second one.")
            .expect("test source has a sentence");
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.seek_to_source_index(&markdown, live_click, cx);
        });
        cx.run_until_parked();
        assert!(
            read_aloud.read_with(cx, |read_aloud, _| read_aloud.is_speaking()),
            "clicking an actual sentence must still start playback"
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

    #[gpui::test]
    async fn streaming_updates_to_the_current_entity_apply_while_another_waits(
        cx: &mut TestAppContext,
    ) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let markdown_a = cx.new(|cx| Markdown::new("One. ".into(), None, None, cx));
        let markdown_b = cx.new(|cx| Markdown::new("Alpha.\n".into(), None, None, cx));
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
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.enqueue_markdown(&markdown_b, false, cx);
        });
        cx.run_until_parked();

        // A is still streaming; its updates must keep applying live even
        // though B is waiting.
        markdown_a.update(cx, |markdown, cx| markdown.append("Two.\n", cx));
        cx.run_until_parked();
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.enqueue_markdown(&markdown_a, true, cx);
        });
        cx.run_until_parked();

        for _ in 0..3 {
            sink.finish_one();
            cx.executor().advance_clock(POSITION_POLL_INTERVAL);
            cx.run_until_parked();
        }
        assert_eq!(
            provider.spoken(),
            vec!["One.", "Two.", "Alpha."],
            "A's late sentence speaks before the waiting message"
        );
    }

    #[gpui::test]
    async fn dedupe_updates_a_pending_entitys_message_complete(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let markdown_a = cx.new(|cx| Markdown::new("One.\n".into(), None, None, cx));
        let markdown_b =
            cx.new(|cx| Markdown::new("Alpha. Tail without terminator".into(), None, None, cx));
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

        // B queues while streaming, then its final chunk marks it complete.
        // Re-enqueueing must update the pending record, not duplicate it.
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.enqueue_markdown(&markdown_b, false, cx);
            read_aloud.enqueue_markdown(&markdown_b, true, cx);
        });
        cx.run_until_parked();

        for _ in 0..3 {
            sink.finish_one();
            cx.executor().advance_clock(POSITION_POLL_INTERVAL);
            cx.run_until_parked();
        }
        assert_eq!(
            provider.spoken(),
            vec!["One.", "Alpha.", "Tail without terminator"],
            "the pending message must speak once (no duplicate) and, being \
             complete, include its unterminated tail"
        );
    }

    #[gpui::test]
    async fn a_seek_clears_the_pending_queue(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let source = "One. Two.\n";
        let markdown_a = cx.new(|cx| Markdown::new(source.into(), None, None, cx));
        let markdown_b = cx.new(|cx| Markdown::new("Alpha.\n".into(), None, None, cx));
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
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.enqueue_markdown(&markdown_b, false, cx);
        });
        cx.run_until_parked();

        // The user clicks a sentence: explicit intent overrides the queue.
        let click = source.find("Two.").expect("test source has a sentence");
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.seek_to_source_index(&markdown_a, click, cx);
        });
        cx.run_until_parked();

        for _ in 0..3 {
            sink.finish_one();
            cx.executor().advance_clock(POSITION_POLL_INTERVAL);
            cx.run_until_parked();
        }
        assert!(
            !provider.spoken().iter().any(|text| text == "Alpha."),
            "the queued message must not speak after a seek overruled it, got {:?}",
            provider.spoken()
        );
    }

    #[gpui::test]
    async fn a_stop_clears_the_pending_queue(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let markdown_a = cx.new(|cx| Markdown::new("One. Two.\n".into(), None, None, cx));
        let markdown_b = cx.new(|cx| Markdown::new("Alpha.\n".into(), None, None, cx));
        cx.run_until_parked();

        let read_aloud = cx.new({
            let provider = provider.clone();
            |cx| ReadAloud::for_test(Arc::new(provider), Box::new(sink), cx)
        });
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.enqueue_markdown(&markdown_a, false, cx);
        });
        cx.run_until_parked();
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.enqueue_markdown(&markdown_b, false, cx);
        });
        cx.run_until_parked();

        read_aloud.update(cx, |read_aloud, cx| read_aloud.toggle(cx));
        cx.run_until_parked();
        let spoken_when_stopped = provider.spoken();

        for _ in 0..3 {
            cx.executor().advance_clock(POSITION_POLL_INTERVAL);
            cx.run_until_parked();
        }
        assert!(
            !read_aloud.read_with(cx, |read_aloud, _| read_aloud.is_speaking()),
            "a stop must stick even with a message waiting its turn"
        );
        assert_eq!(
            provider.spoken(),
            spoken_when_stopped,
            "the queued message must not speak after a stop"
        );
    }

    #[gpui::test]
    async fn a_stop_sticks_while_the_next_block_keeps_streaming(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let markdown_a = cx.new(|cx| Markdown::new("One. Two.\n".into(), None, None, cx));
        let markdown_b = cx.new(|cx| Markdown::new("Alpha. ".into(), None, None, cx));
        cx.run_until_parked();

        let read_aloud = cx.new({
            let provider = provider.clone();
            |cx| ReadAloud::for_test(Arc::new(provider), Box::new(sink), cx)
        });
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.enqueue_markdown(&markdown_a, false, cx);
        });
        cx.run_until_parked();
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.enqueue_markdown(&markdown_b, false, cx);
        });
        cx.run_until_parked();

        read_aloud.update(cx, |read_aloud, cx| read_aloud.toggle(cx));
        cx.run_until_parked();
        let spoken_when_stopped = provider.spoken();

        // The turn keeps going: the block that was waiting its turn streams
        // more chunks, each re-entering `enqueue_markdown` as a *different*
        // entity against a now-idle player. None of that may undo the stop.
        markdown_b.update(cx, |markdown, cx| markdown.append("Beta.\n", cx));
        cx.run_until_parked();
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.enqueue_markdown(&markdown_b, false, cx);
        });
        cx.executor().advance_clock(POSITION_POLL_INTERVAL);
        cx.run_until_parked();
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.enqueue_markdown(&markdown_b, true, cx);
        });
        cx.executor().advance_clock(POSITION_POLL_INTERVAL);
        cx.run_until_parked();

        assert!(
            !read_aloud.read_with(cx, |read_aloud, _| read_aloud.is_speaking()),
            "a stop must survive the rest of the turn arriving as new entities"
        );
        assert_eq!(
            provider.spoken(),
            spoken_when_stopped,
            "nothing new may be synthesized while stopped"
        );
        assert_eq!(
            markdown_b.read_with(cx, |markdown, _| markdown.speaking_highlight().cloned()),
            None,
            "and nothing may be highlighted"
        );

        // An explicit toggle afterwards restarts from the newest message —
        // the view layer's toggle path calls `toggle` then re-enqueues, and
        // the quiet tracking above made B the loaded message.
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.toggle(cx);
            read_aloud.enqueue_markdown(&markdown_b, true, cx);
        });
        cx.run_until_parked();
        assert!(
            read_aloud.read_with(cx, |read_aloud, _| read_aloud.is_speaking()),
            "toggling back on must still work after the stop stuck"
        );
        assert!(
            provider
                .spoken()
                .ends_with(&["Alpha.".to_string(), "Beta.".to_string()]),
            "the restart reads the newest message from its top, got {:?}",
            provider.spoken()
        );
        assert_eq!(
            markdown_b.read_with(cx, |markdown, _| markdown.speaking_highlight().cloned()),
            Some(0..6)
        );
    }

    #[gpui::test]
    async fn playback_resumes_when_speech_outpaces_a_slow_stream(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let markdown = cx.new(|cx| Markdown::new("First one.\n".into(), None, None, cx));
        cx.run_until_parked();

        let read_aloud = cx.new({
            let provider = provider.clone();
            let sink = sink.clone();
            |cx| ReadAloud::for_test(Arc::new(provider), Box::new(sink), cx)
        });
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.enqueue_markdown(&markdown, false, cx);
        });
        cx.run_until_parked();
        assert_eq!(provider.spoken(), vec!["First one."]);

        // Speech finishes everything segmented so far; the reader parks.
        sink.finish_one();
        cx.executor().advance_clock(POSITION_POLL_INTERVAL);
        cx.run_until_parked();
        assert!(!read_aloud.read_with(cx, |read_aloud, _| read_aloud.is_speaking()));

        // More of the SAME message arrives, and — like the real provider mid
        // HTTP request — synthesis is still in flight when the reader's next
        // tick lands on an empty sink. It must keep waiting for the audio,
        // not park forever.
        provider.hold();
        markdown.update(cx, |markdown, cx| {
            markdown.append("Second one. Third one. Fourth one. Fifth one.\n", cx);
        });
        cx.run_until_parked();
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.enqueue_markdown(&markdown, false, cx);
        });
        cx.executor().advance_clock(POSITION_POLL_INTERVAL);
        cx.run_until_parked();
        provider.release_all();
        cx.run_until_parked();

        // Only the prefetch window is queued up front; draining it must keep
        // pulling the rest of the message through with no further enqueues.
        for _ in 0..4 {
            sink.finish_one();
            cx.executor().advance_clock(POSITION_POLL_INTERVAL);
            cx.run_until_parked();
        }
        assert_eq!(
            provider.spoken(),
            vec![
                "First one.",
                "Second one.",
                "Third one.",
                "Fourth one.",
                "Fifth one."
            ],
            "every sentence must speak even though the stream was slower than speech"
        );

        // And repeatedly: the same stall-and-resume must work again.
        provider.hold();
        markdown.update(cx, |markdown, cx| markdown.append("Sixth one.\n", cx));
        cx.run_until_parked();
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.enqueue_markdown(&markdown, false, cx);
        });
        cx.executor().advance_clock(POSITION_POLL_INTERVAL);
        cx.run_until_parked();
        provider.release_all();
        cx.executor().advance_clock(POSITION_POLL_INTERVAL);
        cx.run_until_parked();
        assert_eq!(
            provider.spoken().last().map(String::as_str),
            Some("Sixth one.")
        );

        // A trailing fragment withheld while streaming still speaks once the
        // message completes.
        markdown.update(cx, |markdown, cx| markdown.append("Trailing tail", cx));
        cx.run_until_parked();
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.enqueue_markdown(&markdown, true, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            provider.spoken().last().map(String::as_str),
            Some("Trailing tail"),
            "message completion must release the withheld tail"
        );
    }

    #[gpui::test]
    async fn playback_state_reflects_the_player(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let markdown =
            cx.new(|cx| Markdown::new("First one. Second one.\n".into(), None, None, cx));
        cx.run_until_parked();

        let read_aloud = cx.new({
            let sink = sink.clone();
            |cx| ReadAloud::for_test(Arc::new(provider), Box::new(sink), cx)
        });
        assert_eq!(
            read_aloud.read_with(cx, |read_aloud, cx| read_aloud.playback_state(cx)),
            None,
            "nothing loaded means nothing to control"
        );

        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.enqueue_markdown(&markdown, false, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            read_aloud.read_with(cx, |read_aloud, cx| read_aloud.playback_state(cx)),
            Some(PlaybackState {
                utterance_index: 0,
                utterance_count: 2,
                paused: false,
                sentence_text: "First one.".into(),
            })
        );

        read_aloud.update(cx, |read_aloud, cx| read_aloud.toggle_pause(cx));
        assert_eq!(
            read_aloud
                .read_with(cx, |read_aloud, cx| read_aloud.playback_state(cx))
                .map(|state| state.paused),
            Some(true)
        );
        read_aloud.update(cx, |read_aloud, cx| read_aloud.toggle_pause(cx));

        sink.finish_one();
        sink.finish_one();
        cx.executor().advance_clock(POSITION_POLL_INTERVAL);
        cx.run_until_parked();
        assert_eq!(
            read_aloud.read_with(cx, |read_aloud, cx| read_aloud.playback_state(cx)),
            None,
            "a finished message leaves nothing to control"
        );
    }

    #[gpui::test]
    async fn previous_and_next_sentence_clamp_at_the_ends(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let markdown = cx.new(|cx| Markdown::new("One. Two.\n".into(), None, None, cx));
        cx.run_until_parked();

        let read_aloud = cx.new({
            let provider = provider.clone();
            |cx| ReadAloud::for_test(Arc::new(provider), Box::new(sink), cx)
        });
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.enqueue_markdown(&markdown, false, cx);
        });
        cx.run_until_parked();
        let index = |cx: &mut TestAppContext| {
            read_aloud
                .read_with(cx, |read_aloud, cx| read_aloud.playback_state(cx))
                .map(|state| state.utterance_index)
        };
        assert_eq!(index(cx), Some(0));

        let spoken_before = provider.spoken();
        read_aloud.update(cx, |read_aloud, cx| read_aloud.previous_sentence(cx));
        cx.run_until_parked();
        assert_eq!(index(cx), Some(0), "previous at the start is a no-op");
        assert_eq!(
            provider.spoken(),
            spoken_before,
            "a clamped seek must not resynthesize anything"
        );

        read_aloud.update(cx, |read_aloud, cx| read_aloud.next_sentence(cx));
        cx.run_until_parked();
        assert_eq!(index(cx), Some(1));

        let spoken_before = provider.spoken();
        read_aloud.update(cx, |read_aloud, cx| read_aloud.next_sentence(cx));
        cx.run_until_parked();
        assert_eq!(index(cx), Some(1), "next at the end is a no-op");
        assert_eq!(provider.spoken(), spoken_before);

        read_aloud.update(cx, |read_aloud, cx| read_aloud.previous_sentence(cx));
        cx.run_until_parked();
        assert_eq!(index(cx), Some(0), "previous seeks back to the start");
    }
}
