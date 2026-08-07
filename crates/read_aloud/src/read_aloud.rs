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
///
/// `stopped` is the reduced replay form shown after a user stop: playback is
/// parked, `utterance_index` is 0, and `sentence_text` previews the tracked
/// message's first sentence so a play button can offer to restart it.
#[derive(Clone, Debug, PartialEq)]
pub struct PlaybackState {
    pub utterance_index: usize,
    pub utterance_count: usize,
    pub paused: bool,
    pub stopped: bool,
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
    /// Whether the tracked entity's message had finished streaming as of the
    /// last segmentation. While it has not, an idle player is a lull (speech
    /// outran the stream), not the end — the mini player stays visible
    /// through it instead of blinking out.
    message_complete: bool,
    /// Completion arrived while the entity's background parse still lagged
    /// its source. Finalizing that stale parse would speak its trailing
    /// sentence fragment — a mid-stream chunk boundary, not a sentence end —
    /// so the completion waits here until the parse catches up (see the
    /// observation installed in `switch_to`).
    pending_completion: bool,
    /// Watches the speaking entity so a deferred completion can be applied
    /// the moment its parse lands; nothing else re-runs segmentation after
    /// the turn's final thread event.
    speaking_parse_observation: Option<Subscription>,
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
            message_complete: false,
            pending_completion: false,
            speaking_parse_observation: None,
            poll_task: None,
            _subscriptions: vec![subscription, player_observation],
        }
    }

    /// Whether the entity's background parse has caught up with its source.
    /// Segmentation reads the last *completed* parse, which lags appends;
    /// mid-stream that parse often ends at a chunk boundary in the middle of
    /// a sentence, so treating it as the complete message would finalize a
    /// fragment (the "…and I'll" cut).
    fn parse_is_current(markdown: &Entity<Markdown>, cx: &App) -> bool {
        let markdown = markdown.read(cx);
        markdown.parsed_markdown().source() == markdown.source()
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
                if self.speaking.is_none() {
                    // Dismissed: the user closed the stopped controls, so
                    // passive streaming must not resurrect them. Only
                    // explicit intent (a seek or `play_from_top`) leaves
                    // this state.
                    return;
                }
                // The user's stop outlives the block that was sounding when
                // they pressed it: one agent turn keeps arriving as fresh
                // entities (a new one after every tool call), and none of
                // them may restart speech — only explicit intent lifts the
                // latch. The newest entity is still tracked (quietly, parked
                // and silent) so the stopped-form controls preview it and a
                // restart plays the message the user actually sees.
                self.clear_highlight(cx);
                self.speaking = Some(markdown.clone());
                // Completion state and the parse observation are per-entity,
                // exactly as in `switch_to`. Without re-homing them here, a
                // completion parked against this entity's lagging parse
                // would wait on a notification from the *previous* entity —
                // which never comes — leaving the message incomplete: its
                // withheld tail unspoken on replay, and the controls never
                // hiding after the audio drained.
                self.pending_completion = false;
                self.observe_speaking_parse(markdown, cx);
                self.track_quietly(message_complete, cx);
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
            // reason to start speaking again; only explicit intent is — but
            // the parked preview keeps tracking what a restart would say.
            self.track_quietly(message_complete, cx);
            return;
        }
        self.refresh_current(message_complete, cx);
    }

    /// Re-segments the tracked entity without starting playback: the stop
    /// latch stays, nothing is synthesized (the pump `set_utterances` spawns
    /// is dropped before it can run), and the player ends parked exactly as
    /// `Player::stop` leaves it. This keeps the stopped-form mini player's
    /// first-sentence preview — and the content a later restart speaks —
    /// current while a stopped turn keeps streaming.
    fn track_quietly(&mut self, message_complete: bool, cx: &mut Context<Self>) {
        let Some(markdown) = self.speaking.clone() else {
            return;
        };
        let message_complete = self.resolve_completeness(&markdown, message_complete, cx);
        let utterances = segment(markdown.read(cx).parsed_markdown(), message_complete);
        self.player.update(cx, |player, cx| {
            player.set_utterances(utterances, cx);
            player.stop(cx);
        });
        self.message_complete = message_complete;
        self.push_speakable_ranges(cx);
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
        // Completion state is per-entity: a switch starts from this call's
        // own knowledge, not the previous entity's deferred flag.
        self.pending_completion = false;

        let Some(markdown) = self.speaking.clone() else {
            return;
        };
        self.observe_speaking_parse(&markdown, cx);
        let message_complete = self.resolve_completeness(&markdown, message_complete, cx);
        self.message_complete = message_complete;
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
        let message_complete = self.resolve_completeness(&markdown, message_complete, cx);
        let utterances = segment(markdown.read(cx).parsed_markdown(), message_complete);
        self.player
            .update(cx, |player, cx| player.set_utterances(utterances, cx));
        self.message_complete = message_complete;
        self.push_speakable_ranges(cx);
        self.start_polling(cx);
    }

    /// Watches the newly tracked entity so a completion parked against its
    /// lagging parse (`pending_completion`) is re-delivered when the parse
    /// lands. Must be installed wherever `speaking` is assigned: an
    /// observation left on a previous entity waits on a notification that
    /// never comes.
    fn observe_speaking_parse(&mut self, markdown: &Entity<Markdown>, cx: &mut Context<Self>) {
        self.speaking_parse_observation = Some(cx.observe(markdown, |this, markdown, cx| {
            if this.pending_completion
                && this.speaking.as_ref() == Some(&markdown)
                && Self::parse_is_current(&markdown, cx)
            {
                this.pending_completion = false;
                this.mark_tracked_message_complete(cx);
            }
        }));
    }

    /// Resolves the completeness to segment with. Completion against a parse
    /// that still lags the source is deferred — `pending_completion` holds it
    /// (surviving later incomplete enqueues) until the parse catches up and
    /// the observation installed by `observe_speaking_parse` re-delivers it.
    fn resolve_completeness(
        &mut self,
        markdown: &Entity<Markdown>,
        message_complete: bool,
        cx: &App,
    ) -> bool {
        let requested = message_complete || self.pending_completion;
        let effective = requested && Self::parse_is_current(markdown, cx);
        self.pending_completion = requested && !effective;
        effective
    }

    /// Explicit request to hear one message from its beginning. Overrides
    /// everything passive: the pending queue, a latched stop, a dismissal,
    /// and whatever is currently sounding.
    pub fn play_from_top(
        &mut self,
        markdown: &Entity<Markdown>,
        message_complete: bool,
        cx: &mut Context<Self>,
    ) {
        self.pending.clear();
        self.switch_to(markdown.clone(), message_complete, cx);
    }

    /// Stops playback where it is and latches the stop: nothing speaks again
    /// until explicit intent (a toggle, a seek, or `play_from_top`). Same
    /// path `toggle` takes while speaking.
    pub fn stop(&mut self, cx: &mut Context<Self>) {
        // `stop` emits `Finished`, and this entity's own subscription to
        // the player already clears the highlight in response — no need
        // to do it again here.
        self.player.update(cx, |player, cx| player.stop(cx));
        self.poll_task = None;
        self.stopped_by_user = true;
        // A stop is explicit intent about the whole session, not just the
        // current message: nothing waiting its turn may start either.
        self.pending.clear();
        // `self.speaking` is deliberately retained so the stopped-form
        // controls have a message to preview and restart.
    }

    /// A completeness signal that arrives outside the enqueue path: the turn
    /// ended, so the tracked message — whichever it is — can no longer grow.
    /// Re-segments it as complete so its withheld tail can speak and, once
    /// the audio drains, the controls can hide, without starting anything a
    /// latched stop or a dismissal is holding back. Needed because the
    /// auto-play gate keeps the ordinary enqueue path from ever delivering
    /// the turn-end flag when auto-play is off.
    pub fn mark_tracked_message_complete(&mut self, cx: &mut Context<Self>) {
        if self.speaking.is_none() || self.message_complete {
            return;
        }
        if self.stopped_by_user {
            self.track_quietly(true, cx);
        } else {
            self.refresh_current(true, cx);
        }
        cx.notify();
    }

    /// Hides the player UI entirely: stops tracking, drops the parked
    /// utterances, and keeps the stop latch so passive streaming can bring
    /// back neither the controls nor audio. Purely playback-UI state — the
    /// thread and settings are untouched. "Dismissed" is the latch with
    /// nothing tracked; any explicit playback request leaves it.
    pub fn dismiss(&mut self, cx: &mut Context<Self>) {
        self.pending.clear();
        self.clear_highlight(cx);
        self.speaking = None;
        self.stopped_by_user = true;
        self.message_complete = false;
        self.pending_completion = false;
        self.speaking_parse_observation = None;
        self.poll_task = None;
        self.player.update(cx, |player, cx| {
            player.set_utterances(Vec::new(), cx);
            player.reset(cx);
        });
        cx.notify();
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

    /// `message_complete` carries the caller's knowledge of whether this
    /// message can still grow. It matters beyond segmentation: a message
    /// left flagged incomplete keeps the controls visible after its audio
    /// drains (the streaming-lull rule), and a clicked message often gets
    /// no later enqueue to ever correct the flag.
    pub fn seek_to_source_index(
        &mut self,
        markdown: &Entity<Markdown>,
        source_index: usize,
        message_complete: bool,
        cx: &mut Context<Self>,
    ) {
        // A click is explicit intent: whatever was waiting its turn is
        // overruled by it.
        self.pending.clear();
        if self.speaking.as_ref() != Some(markdown) {
            self.switch_to(markdown.clone(), message_complete, cx);
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
            if message_complete && !self.message_complete {
                // The click knows more than the last enqueue did: the
                // message is done growing, so its withheld tail may speak
                // and the controls may hide once the audio drains. Only ever
                // upgraded — segmentation appends the tail, never reorders,
                // so the target index stays valid.
                self.refresh_current(true, cx);
            }
            self.player
                .update(cx, |player, cx| player.seek_to(index, cx));
            self.start_polling(cx);
        }
    }

    pub fn toggle(&mut self, cx: &mut Context<Self>) {
        // "Speaking", for stop purposes, is what the UI presents as active —
        // not just whether the poll task is running. During an idle lull
        // (speech outran a still-streaming message) the poll task has
        // parked but the controls still show playback, and a toggle there
        // must stop, not fall through to the restart arm and start the
        // message over.
        let presenting_playback = self.playback_state(cx).is_some_and(|state| !state.stopped);
        if self.is_speaking() || presenting_playback {
            self.stop(cx);
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
    /// hidden: nothing loaded, dismissed, or playback naturally finished. A
    /// user stop keeps a reduced `stopped` state alive so restarting stays
    /// one click away.
    pub fn playback_state(&self, cx: &App) -> Option<PlaybackState> {
        self.speaking.as_ref()?;
        let player = self.player.read(cx);
        let utterance_count = player.utterances().len();
        if self.stopped_by_user {
            let utterance = player.utterances().first()?;
            return Some(PlaybackState {
                utterance_index: 0,
                utterance_count,
                paused: false,
                stopped: true,
                sentence_text: SharedString::new(utterance.spoken_text.as_str()),
            });
        }
        let utterance_index = player.speaking_index().or_else(|| {
            // Two reasons the sink can be empty mid-message: synthesis is
            // catching up, or speech outran a still-streaming message (an
            // idle lull). Either way the controls must not blink out, so the
            // utterance next in line stands in.
            ((!player.is_idle() || !self.message_complete) && utterance_count > 0)
                .then(|| player.next_to_synthesize().min(utterance_count - 1))
        })?;
        let utterance = player.utterances().get(utterance_index)?;
        Some(PlaybackState {
            utterance_index,
            utterance_count,
            paused: player.is_paused(),
            stopped: false,
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
            read_aloud.seek_to_source_index(&markdown_b, 0, false, cx);
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
            read_aloud.seek_to_source_index(&markdown, 13, false, cx);
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
            read_aloud.seek_to_source_index(&markdown, dead_click, false, cx);
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
            read_aloud.seek_to_source_index(&markdown, live_click, false, cx);
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
            read_aloud.seek_to_source_index(&markdown_a, click, false, cx);
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
        assert_eq!(
            read_aloud.read_with(cx, |read_aloud, cx| read_aloud.playback_state(cx)),
            Some(PlaybackState {
                utterance_index: 0,
                utterance_count: 2,
                paused: false,
                stopped: true,
                sentence_text: "Alpha.".into(),
            }),
            "the stopped-form controls preview the newest (quietly tracked) message"
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
            read_aloud.enqueue_markdown(&markdown, true, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            read_aloud.read_with(cx, |read_aloud, cx| read_aloud.playback_state(cx)),
            Some(PlaybackState {
                utterance_index: 0,
                utterance_count: 2,
                paused: false,
                stopped: false,
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

    #[gpui::test]
    async fn play_from_top_overrides_stop_and_pending_queue(cx: &mut TestAppContext) {
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
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.enqueue_markdown(&markdown_b, false, cx);
        });
        cx.run_until_parked();

        // Explicit intent interrupts A immediately and empties the queue.
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.play_from_top(&markdown_b, true, cx);
        });
        cx.run_until_parked();
        assert!(
            provider
                .spoken()
                .ends_with(&["Alpha.".to_string(), "Beta.".to_string()]),
            "play_from_top must speak the requested message from its start, got {:?}",
            provider.spoken()
        );
        assert_eq!(
            markdown_a.read_with(cx, |markdown, _| markdown.speaking_highlight().cloned()),
            None
        );
        assert_eq!(
            markdown_b.read_with(cx, |markdown, _| markdown.speaking_highlight().cloned()),
            Some(0..6)
        );

        // Drain B: with the queue cleared, nothing (i.e. not A) follows it.
        let spoken_after_switch = provider.spoken();
        for _ in 0..3 {
            sink.finish_one();
            cx.executor().advance_clock(POSITION_POLL_INTERVAL);
            cx.run_until_parked();
        }
        assert_eq!(
            provider.spoken(),
            spoken_after_switch,
            "the pending queue must have been cleared by play_from_top"
        );

        // And it overrides a latched stop too.
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.enqueue_markdown(&markdown_a, true, cx);
        });
        cx.run_until_parked();
        read_aloud.update(cx, |read_aloud, cx| read_aloud.toggle(cx));
        cx.run_until_parked();
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.play_from_top(&markdown_a, true, cx);
        });
        cx.run_until_parked();
        assert!(
            read_aloud.read_with(cx, |read_aloud, _| read_aloud.is_speaking()),
            "play_from_top must lift a latched stop"
        );
        assert_eq!(
            markdown_a.read_with(cx, |markdown, _| markdown.speaking_highlight().cloned()),
            Some(0..4),
            "and it restarts at the first sentence"
        );
    }

    #[gpui::test]
    async fn stopping_leaves_a_restartable_stopped_state(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let markdown =
            cx.new(|cx| Markdown::new("First one. Second one.\n".into(), None, None, cx));
        cx.run_until_parked();

        let read_aloud = cx.new(|cx| ReadAloud::for_test(Arc::new(provider), Box::new(sink), cx));
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.enqueue_markdown(&markdown, true, cx);
        });
        cx.run_until_parked();

        // The speaker-button stop path and `toggle` while speaking share
        // `stop`; either way the reduced stopped form must remain.
        read_aloud.update(cx, |read_aloud, cx| read_aloud.stop(cx));
        cx.run_until_parked();
        assert_eq!(
            read_aloud.read_with(cx, |read_aloud, cx| read_aloud.playback_state(cx)),
            Some(PlaybackState {
                utterance_index: 0,
                utterance_count: 2,
                paused: false,
                stopped: true,
                sentence_text: "First one.".into(),
            }),
            "a stop must leave a restartable stopped state, not hide the player"
        );

        // The stopped-form play button routes through `toggle`'s restart.
        read_aloud.update(cx, |read_aloud, cx| read_aloud.toggle(cx));
        cx.run_until_parked();
        assert!(read_aloud.read_with(cx, |read_aloud, _| read_aloud.is_speaking()));
        assert_eq!(
            read_aloud
                .read_with(cx, |read_aloud, cx| read_aloud.playback_state(cx))
                .map(|state| (state.stopped, state.utterance_index)),
            Some((false, 0)),
            "restarting leaves the stopped form and begins at the top"
        );
    }

    #[gpui::test]
    async fn dismiss_hides_the_player_until_explicit_intent(cx: &mut TestAppContext) {
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
        read_aloud.update(cx, |read_aloud, cx| read_aloud.toggle(cx));
        cx.run_until_parked();
        let spoken_when_stopped = provider.spoken();
        assert!(
            read_aloud
                .read_with(cx, |read_aloud, cx| read_aloud.playback_state(cx))
                .is_some_and(|state| state.stopped),
            "setup: the stop leaves the stopped form visible"
        );

        read_aloud.update(cx, |read_aloud, cx| read_aloud.dismiss(cx));
        assert_eq!(
            read_aloud.read_with(cx, |read_aloud, cx| read_aloud.playback_state(cx)),
            None,
            "dismiss hides the player entirely"
        );

        // Passive streaming — of the old message or a new block — must not
        // resurrect the controls or start audio.
        markdown_a.update(cx, |markdown, cx| markdown.append("Three.\n", cx));
        cx.run_until_parked();
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.enqueue_markdown(&markdown_a, false, cx);
            read_aloud.enqueue_markdown(&markdown_b, false, cx);
        });
        cx.executor().advance_clock(POSITION_POLL_INTERVAL);
        cx.run_until_parked();
        assert_eq!(
            read_aloud.read_with(cx, |read_aloud, cx| read_aloud.playback_state(cx)),
            None,
            "passive updates must not undo a dismissal"
        );
        assert!(!read_aloud.read_with(cx, |read_aloud, _| read_aloud.is_speaking()));
        assert_eq!(provider.spoken(), spoken_when_stopped);

        // Explicit intent leaves the dismissed state.
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.play_from_top(&markdown_b, true, cx);
        });
        cx.run_until_parked();
        assert!(read_aloud.read_with(cx, |read_aloud, _| read_aloud.is_speaking()));
        assert_eq!(
            read_aloud
                .read_with(cx, |read_aloud, cx| read_aloud.playback_state(cx))
                .map(|state| state.stopped),
            Some(false)
        );
    }

    #[gpui::test]
    async fn the_player_state_survives_a_streaming_lull(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let markdown = cx.new(|cx| Markdown::new("First one.\n".into(), None, None, cx));
        cx.run_until_parked();

        let read_aloud = cx.new({
            let sink = sink.clone();
            |cx| ReadAloud::for_test(Arc::new(provider), Box::new(sink), cx)
        });
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.enqueue_markdown(&markdown, false, cx);
        });
        cx.run_until_parked();

        // Speech outruns the stream: everything segmented so far has played,
        // but the message is not complete — the controls must hold on.
        sink.finish_one();
        cx.executor().advance_clock(POSITION_POLL_INTERVAL);
        cx.run_until_parked();
        assert_eq!(
            read_aloud
                .read_with(cx, |read_aloud, cx| read_aloud.playback_state(cx))
                .map(|state| (state.stopped, state.utterance_index)),
            Some((false, 0)),
            "an idle lull mid-stream must not hide the controls"
        );

        // Completion with nothing further to speak is a real finish: hide.
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.enqueue_markdown(&markdown, true, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            read_aloud.read_with(cx, |read_aloud, cx| read_aloud.playback_state(cx)),
            None,
            "a naturally finished, complete message leaves nothing to control"
        );
    }

    #[gpui::test]
    async fn a_click_into_another_message_of_a_completed_thread_hides_the_player_when_done(
        cx: &mut TestAppContext,
    ) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let markdown_a = cx.new(|cx| Markdown::new("One.\n".into(), None, None, cx));
        let markdown_b = cx.new(|cx| Markdown::new("Alpha. Beta.\n".into(), None, None, cx));
        cx.run_until_parked();

        let read_aloud = cx.new({
            let sink = sink.clone();
            |cx| ReadAloud::for_test(Arc::new(provider), Box::new(sink), cx)
        });
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.enqueue_markdown(&markdown_a, true, cx);
        });
        cx.run_until_parked();
        sink.finish_one();
        cx.executor().advance_clock(POSITION_POLL_INTERVAL);
        cx.run_until_parked();
        assert_eq!(
            read_aloud.read_with(cx, |read_aloud, cx| read_aloud.playback_state(cx)),
            None,
            "setup: the finished newest message hides the player"
        );

        // The thread is done generating; the user clicks a sentence in an
        // *older* message. No thread event will ever enqueue again, so the
        // click itself must carry the completeness.
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.seek_to_source_index(&markdown_b, 0, true, cx);
        });
        cx.run_until_parked();
        assert!(
            read_aloud
                .read_with(cx, |read_aloud, cx| read_aloud.playback_state(cx))
                .is_some_and(|state| !state.stopped),
            "the clicked message plays"
        );

        sink.finish_one();
        sink.finish_one();
        cx.executor().advance_clock(POSITION_POLL_INTERVAL);
        cx.run_until_parked();
        assert_eq!(
            read_aloud.read_with(cx, |read_aloud, cx| read_aloud.playback_state(cx)),
            None,
            "when the clicked message finishes, the player must hide — not \
             stand in at the last utterance forever"
        );
        assert!(!read_aloud.read_with(cx, |read_aloud, _| read_aloud.is_speaking()));
    }

    #[gpui::test]
    async fn toggle_stops_instead_of_restarting_during_a_streaming_lull(cx: &mut TestAppContext) {
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
        sink.finish_one();
        cx.executor().advance_clock(POSITION_POLL_INTERVAL);
        cx.run_until_parked();

        // The lull: the poll task has parked, but the controls (with their
        // X) are still presented.
        assert!(!read_aloud.read_with(cx, |read_aloud, _| read_aloud.is_speaking()));
        assert!(
            read_aloud
                .read_with(cx, |read_aloud, cx| read_aloud.playback_state(cx))
                .is_some_and(|state| !state.stopped),
            "setup: the lull keeps the controls visible"
        );

        let spoken_before = provider.spoken();
        read_aloud.update(cx, |read_aloud, cx| read_aloud.toggle(cx));
        cx.executor().advance_clock(POSITION_POLL_INTERVAL);
        cx.run_until_parked();

        assert_eq!(
            read_aloud
                .read_with(cx, |read_aloud, cx| read_aloud.playback_state(cx))
                .map(|state| state.stopped),
            Some(true),
            "a toggle during the lull must stop into the reduced form, not restart"
        );
        assert!(!read_aloud.read_with(cx, |read_aloud, _| read_aloud.is_speaking()));
        assert_eq!(
            provider.spoken(),
            spoken_before,
            "nothing may be resynthesized by a stop"
        );
    }

    #[gpui::test]
    async fn marking_the_tracked_message_complete_releases_the_tail_and_the_player(
        cx: &mut TestAppContext,
    ) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let markdown =
            cx.new(|cx| Markdown::new("First one. Trailing tail".into(), None, None, cx));
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
        sink.finish_one();
        cx.executor().advance_clock(POSITION_POLL_INTERVAL);
        cx.run_until_parked();
        assert!(
            read_aloud
                .read_with(cx, |read_aloud, cx| read_aloud.playback_state(cx))
                .is_some(),
            "setup: still flagged incomplete, so the controls hold on"
        );

        // The turn ends. With auto-play off no enqueue ever delivers the
        // flag, so this signal is all that stands between the player and
        // an immortal pill (and an unspoken tail).
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.mark_tracked_message_complete(cx);
        });
        cx.run_until_parked();
        assert_eq!(
            provider.spoken(),
            vec!["First one.", "Trailing tail"],
            "completion releases the withheld tail"
        );

        sink.finish_one();
        cx.executor().advance_clock(POSITION_POLL_INTERVAL);
        cx.run_until_parked();
        assert_eq!(
            read_aloud.read_with(cx, |read_aloud, cx| read_aloud.playback_state(cx)),
            None,
            "and once the tail drains, the player hides"
        );
    }

    #[gpui::test]
    async fn a_completion_racing_the_parser_does_not_cut_the_sentence(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let markdown = cx.new(|cx| Markdown::new("".into(), None, None, cx));
        cx.run_until_parked();

        let read_aloud = cx.new({
            let provider = provider.clone();
            |cx| ReadAloud::for_test(Arc::new(provider), Box::new(sink), cx)
        });

        // A chunk lands that ends mid-sentence; its parse completes.
        markdown.update(cx, |markdown, cx| {
            markdown.append(
                "Sounds good. When you\u{2019}ve got it, drop the file anywhere in the repo \
                 (one big square PNG \u{2265}1024\u{d7}1024 is enough) and I\u{2019}ll",
                cx,
            );
        });
        cx.run_until_parked();
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.enqueue_markdown(&markdown, false, cx);
        });
        cx.run_until_parked();
        assert_eq!(provider.spoken(), vec!["Sounds good."]);

        // The rest of the sentence arrives together with the turn's end —
        // the completion is processed while the background parse still
        // reflects the old chunk boundary after "I'll".
        markdown.update(cx, |markdown, cx| {
            markdown.append(" generate both sizes and rebundle.", cx);
        });
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.enqueue_markdown(&markdown, true, cx);
        });
        cx.run_until_parked();

        assert_eq!(
            provider.spoken(),
            vec![
                "Sounds good.".to_string(),
                "When you\u{2019}ve got it, drop the file anywhere in the repo (one big square \
                 PNG \u{2265}1024\u{d7}1024 is enough) and I\u{2019}ll generate both sizes and \
                 rebundle."
                    .to_string(),
            ],
            "the sentence must be spoken whole once the parse catches up — \
             never finalized at the stale parse's chunk boundary"
        );
    }

    #[gpui::test]
    async fn a_parked_completion_survives_a_stop_latched_entity_switch(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let markdown_a = cx.new(|cx| Markdown::new("One.\n".into(), None, None, cx));
        let markdown_b = cx.new(|cx| Markdown::new("".into(), None, None, cx));
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
        read_aloud.update(cx, |read_aloud, cx| read_aloud.toggle(cx));
        cx.run_until_parked();

        // The turn continues while stopped: the next block arrives and is
        // tracked quietly (the stop-latched switch path).
        markdown_b.update(cx, |markdown, cx| {
            markdown.append("Alpha. And then I", cx);
        });
        cx.run_until_parked();
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.enqueue_markdown(&markdown_b, false, cx);
        });
        cx.run_until_parked();

        // Its final chunk and the turn's end land in one burst, while the
        // background parse still lags: the completion parks. When the parse
        // lands, the observation must deliver it — for THIS entity, not the
        // one that was speaking before the stop.
        markdown_b.update(cx, |markdown, cx| {
            markdown.append(" will finish.", cx);
        });
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.enqueue_markdown(&markdown_b, true, cx);
        });
        cx.run_until_parked();

        // Replay from the stopped form (the mini player's play button routes
        // through toggle's restart arm).
        read_aloud.update(cx, |read_aloud, cx| read_aloud.toggle(cx));
        cx.run_until_parked();
        assert!(
            provider
                .spoken()
                .ends_with(&["Alpha.".to_string(), "And then I will finish.".to_string()]),
            "the replay must speak the delivered tail, not the parked cut, got {:?}",
            provider.spoken()
        );

        sink.finish_one();
        sink.finish_one();
        cx.executor().advance_clock(POSITION_POLL_INTERVAL);
        cx.run_until_parked();
        assert_eq!(
            read_aloud.read_with(cx, |read_aloud, cx| read_aloud.playback_state(cx)),
            None,
            "the delivered completion lets the controls hide once the audio drains"
        );
    }
}
