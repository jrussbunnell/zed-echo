mod inworld;
mod narration;
mod player;
mod provider;
mod segmenter;
mod sink;

pub use inworld::{
    INWORLD_CREDENTIALS_URL, InworldTts, fallback_voices, fetch_voices, resolve_api_key,
};
pub use narration::{
    MAX_STEP_LINE_CHARS, MAX_SUMMARY_CHARS, MAX_WRAP_UP_CHARS, NOTHING_TO_ADD, NarrationKind,
    SummaryModel, TRIVIAL_MESSAGE_CHARS, ToolCallFacts, ToolCallOutcome, WrapUpBudget,
    WrapUpMaterial, step_prompt, summary_prompt, wrap_up_prompt,
};

pub use player::{Player, PlayerEvent};
pub use provider::{Pcm, TtsProvider, TtsVoice, WordTiming};
pub use segmenter::{Utterance, segment};
pub use settings::{NarrationDetail, ReadAloudMode};
pub use sink::{AudioSink, RodioSink};

// Test doubles are only part of the crate's public surface under
// `test-support`, so downstream crates (e.g. agent_ui's own tests) can build
// a `ReadAloud` without a network or an audio device, the same way this
// crate's own tests do.
#[cfg(any(test, feature = "test-support"))]
pub use narration::FakeSummaryModel;
#[cfg(any(test, feature = "test-support"))]
pub use provider::FakeTts;
#[cfg(any(test, feature = "test-support"))]
pub use sink::FakeSink;

use futures::FutureExt as _;
use gpui::{
    App, AppContext as _, Context, Entity, EntityId, EventEmitter, Hsla, SharedString,
    Subscription, Task,
};
use markdown::Markdown;
use narration::NarrationQueue;
use settings::{LanguageModelSelection, RegisterSetting, Settings};
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;
use util::ResultExt as _;

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
    /// Resolved from `read_aloud.pill_colors`; `None` means the built-in
    /// purple→pink default. `(start, end)` — equal for a solid color.
    pub pill_colors: Option<(Hsla, Hsla)>,
    pub click_to_seek: bool,
    pub mode: ReadAloudMode,
    pub narrate_tool_calls: bool,
    pub narration_detail: NarrationDetail,
    pub summary_model: Option<LanguageModelSelection>,
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
            pill_colors: read_aloud
                .and_then(|s| s.pill_colors.as_deref())
                .and_then(resolve_pill_colors),
            click_to_seek: read_aloud.and_then(|s| s.click_to_seek).unwrap_or(true),
            mode: read_aloud.and_then(|s| s.mode).unwrap_or_default(),
            narrate_tool_calls: read_aloud
                .and_then(|s| s.narrate_tool_calls)
                .unwrap_or(true),
            narration_detail: read_aloud
                .and_then(|s| s.narration_detail)
                .unwrap_or_default(),
            summary_model: read_aloud.and_then(|s| s.summary_model.clone()),
        }
    }
}

/// One color → solid (both stops equal); two → gradient start and end.
/// Anything else — empty, more than two, or an unparsable entry — falls back
/// to the default palette, warning with the offending value. Resolution only
/// runs when settings (re)load, so the warning fires once per bad edit, not
/// in any hot path.
pub fn resolve_pill_colors(colors: &[String]) -> Option<(Hsla, Hsla)> {
    if colors.is_empty() || colors.len() > 2 {
        log::warn!(
            "read_aloud: pill_colors takes one or two colors, got {}; using the default palette",
            colors.len()
        );
        return None;
    }
    let mut parsed = Vec::with_capacity(colors.len());
    for color in colors {
        match parse_hex_color(color) {
            Some(parsed_color) => parsed.push(parsed_color),
            None => {
                log::warn!(
                    "read_aloud: pill_colors entry {color:?} is not a valid hex color; \
                     using the default palette"
                );
                return None;
            }
        }
    }
    match parsed.as_slice() {
        [only] => Some((*only, *only)),
        [first, second] => Some((*first, *second)),
        _ => None,
    }
}

/// `#RGB`, `#RRGGBB`, or `#RRGGBBAA`, case-insensitive, `#` optional. An
/// alpha component is accepted but ignored: the highlight layers apply their
/// own alphas so glyphs stay legible in both appearances. Public so the
/// agent-panel styling settings parse colors with identical semantics.
pub fn parse_hex_color(text: &str) -> Option<Hsla> {
    let hex = text.trim().trim_start_matches('#');
    if !hex.chars().all(|character| character.is_ascii_hexdigit()) {
        return None;
    }
    let (red, green, blue) = match hex.len() {
        3 => {
            let value = u16::from_str_radix(hex, 16).ok()?;
            let expand = |nibble: u16| (nibble * 17) as u8;
            (
                expand((value >> 8) & 0xF),
                expand((value >> 4) & 0xF),
                expand(value & 0xF),
            )
        }
        6 | 8 => (
            u8::from_str_radix(hex.get(0..2)?, 16).ok()?,
            u8::from_str_radix(hex.get(2..4)?, 16).ok()?,
            u8::from_str_radix(hex.get(4..6)?, 16).ok()?,
        ),
        _ => return None,
    };
    Some(
        gpui::Rgba {
            r: f32::from(red) / 255.,
            g: f32::from(green) / 255.,
            b: f32::from(blue) / 255.,
            a: 1.,
        }
        .into(),
    )
}

pub fn init(cx: &mut gpui::App) {
    ReadAloudSettings::register(cx);
}

/// How often the player's position is sampled to advance the highlights.
/// rodio reports position but emits no completion callback, so this is
/// polled — fast enough that the word highlight tracks speech smoothly
/// (spoken words last a few hundred milliseconds each).
const POSITION_POLL_INTERVAL: Duration = Duration::from_millis(30);

/// How long a summary generation may run before narration gives up and
/// speaks the message's opening sentences instead. Ambient status that
/// arrives late is worse than status that arrives plain.
const SUMMARY_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the agent has to go quiet before an open step is considered
/// finished.
///
/// This window is deliberately *not* on the critical path: a step's line is
/// generated as soon as the step has prose and its first real tool call, so
/// closing a step no longer gates any audio — it only decides when a
/// generation already in hand gets spoken, usually behind prose that is
/// still playing. That is why it can be generous without costing anything.
///
/// The gap it has to bridge is between one narrated call and the next, which
/// for a burst is tens of milliseconds. What it must *not* race is the wait
/// for a tool call's label to stop arriving, which is far longer; that is
/// handled directly by [`ReadAloud::note_tool_call_pending`], which re-arms
/// this timer whenever the owning view sees a call it cannot say yet.
const STEP_IDLE_WINDOW: Duration = Duration::from_millis(1_200);

/// How long a step's line may take before narration stops waiting and says
/// the templated form instead. A step's line is status: two seconds after
/// the fact it is no longer describing the present, and the terse template
/// spoken now beats a better sentence spoken late.
///
/// Losing the race also *cancels* the request rather than letting it land
/// and be discarded — same guarantee ("never speak both"), one fewer token
/// spent.
const STEP_FALLBACK_DELAY: Duration = Duration::from_secs(2);

/// How many generations one step may spend. Two, for the same reason the
/// wrap-up gets two: the first is issued the moment there is something to
/// say, and the second picks up the tool calls that joined while it ran.
const MAX_STEP_LINE_ISSUES: usize = 2;

/// How long a turn wrap-up may take. Longer than a step's budget because a
/// wrap-up is issued speculatively, while the closing message is still
/// streaming — it is racing the end of the turn, not the next sentence.
const WRAP_UP_TIMEOUT: Duration = Duration::from_secs(5);

/// How much closing prose must exist before a wrap-up is worth speculating
/// on. Below this there is not enough of the message to say anything about
/// that the turn's own step lines have not already said.
const WRAP_UP_MIN_CHARS: usize = 200;

/// How much more the closing message must grow before the wrap-up is worth
/// re-issuing with the fuller text.
const WRAP_UP_REISSUE_GROWTH: usize = 600;

/// How many times a turn may speculatively generate its wrap-up. Two: one
/// early enough to be ready, one once the message has really taken shape.
const MAX_WRAP_UP_ISSUES: usize = 2;

/// How many tool calls a turn remembers as raw material for its wrap-up.
/// A turn that does more than this has a wrap-up shaped by its prose, not
/// by an exhaustive list of every file it touched.
const MAX_TURN_ACTIVITY: usize = 24;

/// How many model calls must fail in a row, with nothing in between that
/// worked, before narration says out loud that the model is not answering.
///
/// One failure is a blip: a slow but working model losing a step's
/// two-second race counts as one, and toasting for that would be a lie.
/// Three in a row is roughly a whole turn in which the listener heard
/// nothing but templated lines and had no way to know why.
const MODEL_FAILURES_BEFORE_REPORTING: usize = 3;

/// Something the owning view has to act on rather than merely re-render.
pub enum ReadAloudEvent {
    /// Narration has asked the summary model for a line several times
    /// running and got nothing usable back. Emitted once per reader, so the
    /// view can say so once rather than per message.
    SummaryModelFailing,
}

/// One tool call, as the step that contains it remembers it.
struct StepToolCall {
    facts: ToolCallFacts,
    /// The call as the prompt should see it, captured when the call joined
    /// the step so the prompt never has to read an entity.
    description: String,
}

/// One thing the agent did this turn, as raw material for the wrap-up.
struct TurnAction {
    /// The agent's own id for the call, so a later status update finds the
    /// action it is about.
    id: String,
    /// The call as the prompt should see it, refreshed when the outcome
    /// changes: a failure is the single most important thing to carry
    /// through to the wrap-up.
    description: String,
    facts: ToolCallFacts,
}

/// The prose the agent wrote plus the tool calls it then made: one thing it
/// is doing, and the reason it gave for doing it. Closed — and turned into
/// a single spoken line — when new prose follows the tool calls, when the
/// agent goes quiet for [`STEP_IDLE_WINDOW`], or when the turn ends.
struct OpenStep {
    number: usize,
    prose: Vec<Entity<Markdown>>,
    /// The first block of each message whose prose joined this step, so
    /// closing the step can mark those messages narrated and keep the
    /// per-message summary from saying them a second time.
    prose_heads: Vec<EntityId>,
    tool_calls: Vec<StepToolCall>,
    /// The agent's own words, already spoken. The prose is the intent, it is
    /// already written, and it needs no model call — so it goes out the
    /// moment the step is real rather than waiting for the fused line. What
    /// was said is kept here so the fused line can be dropped if it would
    /// only say it again.
    spoken_prose: Option<String>,
    line: StepLine,
}

/// A step's fused line, generated speculatively while the step is still open
/// so that closing the step costs no round trip. Mirrors [`WrapUp`]: the
/// answer is held until there is a place to say it.
#[derive(Default)]
struct StepLine {
    /// How many generations this step has spent, capped at
    /// [`MAX_STEP_LINE_ISSUES`].
    issues: usize,
    /// How many tool calls the step had at the last generation, so a
    /// re-issue only happens once there is genuinely more to describe.
    issued_calls: usize,
    generating: bool,
    /// A finished line waiting for the step to close.
    ready: Option<String>,
    /// No line is coming: the model failed, timed out, said it had nothing
    /// to add, or there was none to ask.
    exhausted: bool,
}

/// The turn's closing summary, generated speculatively so that the audio is
/// ready the instant the turn ends instead of a model round trip after it.
#[derive(Default)]
struct WrapUp {
    /// How long the closing message was at the last generation, so a
    /// re-issue only happens once there is meaningfully more to say.
    issued_chars: usize,
    /// A finished wrap-up waiting for the turn to end.
    ready: Option<String>,
    task: Option<Task<()>>,
    /// The turn has ended and is waiting on the generation in `task`.
    awaiting_turn_end: bool,
    delivered: bool,
    /// No further generation will be attempted this turn, whatever the
    /// counter says: the model is unavailable, or one has already failed at
    /// the point it was needed.
    spent: bool,
}

impl WrapUp {
    /// A turn that will never have a model wrap-up, recorded so the owning
    /// view stops asking on every streaming chunk.
    fn exhausted() -> Self {
        Self {
            spent: true,
            ..Default::default()
        }
    }
}

/// What a message narration parked on a lagging parse should do once the
/// parse lands.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ParkedMessageNarration {
    /// Condense it with the summary model, falling back to its opening.
    Summarize,
    /// Speak its opening `sentences` sentences, with no model call — the
    /// form used when a step opens and the agent's own words go straight
    /// out, and when a model has already been tried and failed.
    Plainly { sentences: usize },
}

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
    /// What `stopped_by_user` was when [`Self::deactivate`] parked this
    /// reader, so returning restores the user's own intent instead of
    /// inheriting the navigation's. `None` means this reader is not parked
    /// for a navigation.
    ///
    /// Kept as a saved value rather than a second latch so every existing
    /// check of `stopped_by_user` — the auto-play gates, the narration
    /// gates, the mini player's stopped form — keeps working unchanged;
    /// while away, a navigation stop should behave exactly like a user stop.
    stopped_before_navigation: Option<bool>,
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
    /// Mirrors `read_aloud.click_to_seek`. While false the speaking entity is
    /// handed no speakable ranges, so the markdown element shows no seek
    /// affordance (hover band, pointer cursor) — the owning view also skips
    /// wiring the click handler itself.
    click_to_seek: bool,
    /// Full mode speaks the assistant's prose; narration mode speaks a
    /// running status instead and leaves the prose on screen. Every
    /// narration field below is inert in full mode.
    mode: ReadAloudMode,
    /// Whether narration speaks a templated line per tool call, or groups
    /// prose and the tool calls it motivated into one step line and wraps
    /// the turn up at the end. Inert in full mode.
    detail: NarrationDetail,
    narration: NarrationQueue,
    /// The step being accumulated, `None` between steps.
    step: Option<OpenStep>,
    /// Steps that have closed while their line was still being generated.
    /// They wait here rather than being spoken as templates immediately,
    /// because the generation was started early enough that it is usually
    /// about to answer.
    closing_steps: HashMap<usize, OpenStep>,
    /// Closes the open step once the agent has gone quiet. Re-armed on
    /// every piece of activity, so it only ever fires on a real lull.
    step_idle_task: Option<Task<()>>,
    /// Step lines being generated, keyed by step number. Keyed rather than a
    /// single slot for the same reason summaries are: steps overlap, and a
    /// shared slot would silently drop the earlier one.
    step_tasks: HashMap<usize, Task<()>>,
    next_step_number: usize,
    /// Every tool call this turn has made, as raw material for the wrap-up.
    /// The real paths and commands rather than their spoken forms: the model
    /// gets more out of those than out of "reading player".
    turn_activity: Vec<TurnAction>,
    /// How many tool calls this turn has made, uncapped — the wrap-up's
    /// length budget is chosen from the size of the turn, and
    /// [`MAX_TURN_ACTIVITY`] would flatten every large turn into the same
    /// number.
    turn_tool_calls: usize,
    /// When this turn's first tool call arrived, on the executor's clock so
    /// tests can move it. The other half of the wrap-up's length budget: a
    /// long turn earns a longer sign-off even if it made few calls.
    turn_started_at: Option<std::time::Instant>,
    wrap_up: Option<WrapUp>,
    /// How many wrap-up generations this *turn* has spent. Kept out of
    /// [`WrapUp`] because a wrap-up is discarded whenever the agent turns out
    /// not to have finished after all, and a per-`WrapUp` counter would reset
    /// with it — letting a turn that alternates prose and tool calls spend
    /// two generations per block.
    wrap_up_issues: usize,
    /// Blocks washed while the current narration sounds, for narrations
    /// whose spoken text is not in the document (a summary). Empty
    /// otherwise, which is what makes full mode's highlighting untouched.
    narration_wash: Vec<Entity<Markdown>>,
    /// A message whose narration is waiting on its own background parse.
    /// Segmentation reads the last completed parse, which lags the source
    /// while a message is still landing, and condensing a half-parsed
    /// message would describe a fragment. The prose counterpart of
    /// `pending_completion`.
    narration_message_pending_parse: Option<(Vec<Entity<Markdown>>, ParkedMessageNarration)>,
    narration_parse_observations: Vec<Subscription>,
    summary_model: Option<Rc<dyn SummaryModel>>,
    /// Summary generations in flight, keyed by the message each one is for.
    /// Dropping a task cancels its call, so a stop, a mode switch, or a new
    /// turn can never be spoken over by a summary of what the user already
    /// moved on from.
    ///
    /// Keyed rather than a single slot because turns interleave: a message
    /// completes, a tool call fires, and the next message completes well
    /// inside the one-to-three seconds a model round trip takes. A shared
    /// slot would let the second request cancel the first, and the first is
    /// already marked summarized, so it could never be retried — a message
    /// silently lost to nothing but timing.
    summary_tasks: HashMap<EntityId, Task<()>>,
    /// Messages already summarized, keyed by their first block, so exactly
    /// one model call is ever spent per completed message.
    summarized_messages: HashSet<EntityId>,
    /// Whether the "no summary model, speaking the opening instead" warning
    /// has been logged. Logged once for this reader, not once per message.
    logged_summary_fallback: bool,
    /// Model calls that have failed since the last one that worked. An
    /// authenticated-but-erroring model is otherwise indistinguishable from
    /// no model at all: narration goes on speaking templated lines and
    /// nothing says why.
    consecutive_model_failures: usize,
    /// Whether this reader has already reported that the model is failing.
    /// Once per reader, like the log line.
    reported_model_failing: bool,
    poll_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ReadAloudEvent> for ReadAloud {}

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
            stopped_before_navigation: None,
            message_complete: false,
            pending_completion: false,
            speaking_parse_observation: None,
            click_to_seek: true,
            mode: ReadAloudMode::Full,
            detail: NarrationDetail::default(),
            narration: NarrationQueue::default(),
            step: None,
            closing_steps: HashMap::new(),
            step_idle_task: None,
            step_tasks: HashMap::new(),
            next_step_number: 0,
            turn_activity: Vec::new(),
            turn_tool_calls: 0,
            turn_started_at: None,
            wrap_up: None,
            wrap_up_issues: 0,
            narration_wash: Vec::new(),
            narration_message_pending_parse: None,
            narration_parse_observations: Vec::new(),
            summary_model: None,
            summary_tasks: HashMap::new(),
            summarized_messages: HashSet::new(),
            logged_summary_fallback: false,
            consecutive_model_failures: 0,
            reported_model_failing: false,
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
        self.step_narration_aside(cx);
        self.switch_to(markdown.clone(), message_complete, cx);
    }

    /// Hands the floor from narration back to full prose for one explicitly
    /// requested message: the queued status is dropped (it would be stale
    /// by the time it got a turn) and the whole-message wash gives way to
    /// ordinary sentence and word highlighting. The mode setting is
    /// untouched, so later messages narrate again.
    fn step_narration_aside(&mut self, cx: &mut Context<Self>) {
        self.narration.clear();
        self.clear_narration_wash(cx);
    }

    /// Stops playback where it is and latches the stop: nothing speaks again
    /// until explicit intent (a toggle, a seek, or `play_from_top`). Same
    /// path `toggle` takes while speaking.
    pub fn stop(&mut self, cx: &mut Context<Self>) {
        self.park(cx);
        self.stopped_by_user = true;
    }

    /// Parks playback and drops everything queued, without saying whose
    /// decision it was — [`Self::stop`] and [`Self::deactivate`] each add
    /// their own latch on top.
    fn park(&mut self, cx: &mut Context<Self>) {
        // `player.stop` emits `Finished`, and this entity's own
        // subscription to the player already clears the highlight in
        // response — no need to do it again here.
        self.player.update(cx, |player, cx| player.stop(cx));
        self.poll_task = None;
        // A stop is explicit intent about the whole session, not just the
        // current message: nothing waiting its turn may start either.
        self.pending.clear();
        // Including narration — status the user stopped hearing is stale by
        // the time they ask for sound again, and a summary still being
        // generated must never arrive after a stop.
        self.narration.clear();
        self.narration.forget_recent();
        self.summary_tasks.clear();
        self.drop_narration_progress();
        self.narration_message_pending_parse = None;
        self.narration_parse_observations.clear();
        self.clear_narration_wash(cx);
        // `self.speaking` is deliberately retained so the stopped-form
        // controls have a message to preview and restart.
    }

    /// Silences this reader because the user is looking at something else —
    /// another thread, or a hidden panel. Everything a stop does, but the
    /// latch it leaves is undone by [`Self::reactivate`] rather than
    /// needing explicit intent: asking someone to re-arm read aloud every
    /// time they glance at another thread is not a decision they made.
    ///
    /// A user's own stop, if one was already in force, is remembered and
    /// handed back on return — leaving is not consent to start talking
    /// again.
    pub fn deactivate(&mut self, cx: &mut Context<Self>) {
        if self.stopped_before_navigation.is_none() {
            self.stopped_before_navigation = Some(self.stopped_by_user);
        }
        self.park(cx);
        self.stopped_by_user = true;
        cx.notify();
    }

    /// Undoes [`Self::deactivate`] when the user comes back, restoring
    /// whatever their own intent had been. Nothing resumes on its own; the
    /// next turn is simply allowed to speak again.
    pub fn reactivate(&mut self, cx: &mut Context<Self>) {
        let Some(before_navigation) = self.stopped_before_navigation.take() else {
            return;
        };
        // Anything that explicitly started speaking while this thread sat in
        // the background has already lifted the latch, and knows better than
        // the value the navigation parked.
        if self.stopped_by_user {
            self.stopped_by_user = before_navigation;
            cx.notify();
        }
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
        self.halt(cx);
        self.stopped_by_user = true;
        cx.notify();
    }

    /// Stops and forgets everything the reader was doing, without saying
    /// anything about what should happen next: [`Self::dismiss`] adds the
    /// stop latch, [`Self::set_mode`] deliberately does not.
    fn halt(&mut self, cx: &mut Context<Self>) {
        self.pending.clear();
        self.narration.clear();
        self.narration.forget_recent();
        self.summary_tasks.clear();
        self.drop_narration_progress();
        self.narration_message_pending_parse = None;
        self.narration_parse_observations.clear();
        self.clear_highlight(cx);
        self.speaking = None;
        self.message_complete = false;
        self.pending_completion = false;
        self.speaking_parse_observation = None;
        self.poll_task = None;
        self.player.update(cx, |player, cx| {
            player.set_utterances(Vec::new(), cx);
            player.reset(cx);
        });
    }

    /// Mirrors the current utterance ranges into the speaking entity so its
    /// element can preview (hover) and advertise (cursor) click-to-seek.
    /// With `click_to_seek` off nothing is mirrored: a disabled action gets
    /// no affordance.
    fn push_speakable_ranges(&self, cx: &mut Context<Self>) {
        if !self.narration_wash.is_empty() {
            // While a summary speaks, the player's utterances are of text
            // that exists nowhere on screen, so they map to nothing the user
            // could click. The message being washed is what they *can* click
            // — that is the whole drill-down — so it advertises its own
            // sentences instead. Without this, narration mode offers no
            // hover band and no pointer cursor, and the gesture is invisible
            // to anyone who does not already know it is there.
            for block in self.narration_wash.clone() {
                let ranges: Vec<Range<usize>> = if self.click_to_seek {
                    segment(block.read(cx).parsed_markdown(), true)
                        .into_iter()
                        .map(|utterance| utterance.source_range)
                        .collect()
                } else {
                    Vec::new()
                };
                block.update(cx, |markdown, cx| {
                    markdown.set_speakable_ranges(ranges, cx);
                });
            }
            return;
        }
        let Some(markdown) = self.speaking.clone() else {
            return;
        };
        let ranges: Vec<Range<usize>> = if self.click_to_seek {
            self.player
                .read(cx)
                .utterances()
                .iter()
                .map(|utterance| utterance.source_range.clone())
                .collect()
        } else {
            Vec::new()
        };
        markdown.update(cx, |markdown, cx| {
            markdown.set_speakable_ranges(ranges, cx);
        });
    }

    /// Applies a `read_aloud.click_to_seek` change live: the speaking
    /// entity's mirrored ranges are re-pushed (emptied when disabling) so the
    /// hover affordance follows the setting without a restart.
    pub fn set_click_to_seek(&mut self, click_to_seek: bool, cx: &mut Context<Self>) {
        if self.click_to_seek == click_to_seek {
            return;
        }
        self.click_to_seek = click_to_seek;
        self.push_speakable_ranges(cx);
    }

    pub fn mode(&self) -> ReadAloudMode {
        self.mode
    }

    /// Applies a `read_aloud.mode` change live. Whatever is sounding is
    /// dropped and the new mode takes over from the next utterance: there
    /// is no sensible way to re-summarize prose that has already been
    /// spoken, or to expand a summary the listener already heard.
    ///
    /// Deliberately not [`Self::stop`] — that latch means "the user wants
    /// silence", and would leave the mode they just switched into mute.
    pub fn set_mode(&mut self, mode: ReadAloudMode, cx: &mut Context<Self>) {
        if self.mode == mode {
            return;
        }
        self.mode = mode;
        self.halt(cx);
        cx.notify();
    }

    /// Records whether one model call produced something narration could
    /// use, and reports a run of failures once.
    ///
    /// "Nothing usable" deliberately covers a hard error, a timeout, and a
    /// reply too long to be a status line: from the listener's seat they are
    /// the same event — a templated line where a real one should have been.
    fn note_model_result(&mut self, succeeded: bool, cx: &mut Context<Self>) {
        if succeeded {
            self.consecutive_model_failures = 0;
            return;
        }
        self.consecutive_model_failures += 1;
        if self.reported_model_failing
            || self.consecutive_model_failures < MODEL_FAILURES_BEFORE_REPORTING
        {
            return;
        }
        self.reported_model_failing = true;
        log::warn!(
            "read_aloud: {} summary model calls in a row produced nothing usable; narration is \
             running on its templated fallback",
            self.consecutive_model_failures
        );
        cx.emit(ReadAloudEvent::SummaryModelFailing);
    }

    /// Installs the model narration mode condenses finished messages with.
    /// `None` (no model configured, or none available) makes every message
    /// take the opening-sentences fallback instead.
    pub fn set_summary_model(&mut self, model: Option<Rc<dyn SummaryModel>>) {
        self.summary_model = model;
    }

    /// Applies a `read_aloud.narration_detail` change live. Like a mode
    /// change, whatever is sounding is dropped and the new granularity
    /// takes over from the next utterance: a half-accumulated step has no
    /// meaning in a mode that does not have steps.
    pub fn set_narration_detail(&mut self, detail: NarrationDetail, cx: &mut Context<Self>) {
        if self.detail == detail {
            return;
        }
        self.detail = detail;
        self.halt(cx);
        cx.notify();
    }

    /// Takes one tool call the agent has made. In `actions` detail this is
    /// spoken straight away as a templated line; in `steps` it joins the
    /// step being accumulated, and speaks as part of the one line that says
    /// what the agent is doing and why.
    ///
    /// The owning view decides *when* a call is worth taking: a tool call's
    /// structured input and its title both arrive after the entry does, so
    /// this is called once they have stopped changing.
    pub fn narrate_tool_call(&mut self, facts: ToolCallFacts, cx: &mut Context<Self>) {
        if self.mode != ReadAloudMode::Narration || self.stopped_by_user {
            return;
        }
        if facts.spoken_key(cx).trim().is_empty() {
            return;
        }
        let description = facts.description(cx);
        self.turn_tool_calls += 1;
        self.turn_started_at
            .get_or_insert_with(|| cx.background_executor().now());
        self.remember_turn_action(TurnAction {
            id: facts.id.clone(),
            description: description.clone(),
            facts: facts.clone(),
        });
        self.invalidate_speculated_wrap_up();
        match self.detail {
            NarrationDetail::Actions => {
                if !self.narration.push_tool_call(facts, cx) {
                    return;
                }
                self.start_next_narration_if_idle(cx);
            }
            NarrationDetail::Steps => {
                // A tool call only belongs to a step if the step has prose
                // for it to be the *why* of. Without one there is nothing a
                // model could add beyond rephrasing the label, so the call
                // is spoken now, in its templated form — the same timely,
                // free line `actions` gives, and the same one the queue's
                // collapse rules fold into a count when a burst arrives.
                if self.step.as_ref().is_none_or(|step| step.prose.is_empty()) {
                    if self.narration.push_tool_call(facts, cx) {
                        self.start_next_narration_if_idle(cx);
                    }
                    return;
                }
                // The first tool call is what makes the prose in front of it
                // a *step* rather than a message. That is the moment the
                // agent's own words are worth saying, and saying them costs
                // nothing: they are already written, so there is no round
                // trip between the agent deciding something and the listener
                // hearing it.
                let first_call = self
                    .step
                    .as_ref()
                    .is_some_and(|step| step.tool_calls.is_empty());
                if let Some(step) = self.step.as_mut() {
                    step.tool_calls.push(StepToolCall { facts, description });
                }
                if first_call {
                    self.speak_step_opening(cx);
                }
                self.maybe_generate_step_line(cx);
                self.arm_step_idle_timer(cx);
            }
        }
    }

    /// Records how a tool call this turn ended.
    ///
    /// Kept apart from [`Self::narrate_tool_call`] because the two happen at
    /// different times: a call is narrated as soon as what it is doing can
    /// be named, and whether it worked is only known later. "The tests
    /// failed" is the single most important thing a supervising listener
    /// needs, and nothing else in the turn's material carries it.
    pub fn note_tool_call_outcome(&mut self, id: &str, outcome: ToolCallOutcome, cx: &App) {
        let Some(action) = self
            .turn_activity
            .iter_mut()
            .find(|action| action.id == id)
            .filter(|action| action.facts.outcome != outcome)
        else {
            return;
        };
        action.facts.outcome = outcome;
        action.description = action.facts.description(cx);
    }

    /// Adds one action to the turn's account.
    ///
    /// The account itself is not capped — [`MAX_TURN_ACTIVITY`] bounds what
    /// reaches the *prompt*, not what is remembered. Capping on the way in
    /// meant that in a forty-call turn a `cargo test` that failed at call
    /// thirty-one was never recorded, so its later `Failed` status had
    /// nothing to attach to and the listener was told "that's done" about a
    /// turn whose tests were red — precisely on the long turns where the
    /// wrap-up is their only account of what happened.
    ///
    /// One small struct per tool call per turn, dropped with the turn, is
    /// strictly less than the thread already retains for the same calls.
    fn remember_turn_action(&mut self, action: TurnAction) {
        self.turn_activity.push(action);
    }

    /// The turn's account as the wrap-up prompt should see it: every failure,
    /// then as many of the most recent other calls as [`MAX_TURN_ACTIVITY`]
    /// leaves room for, in the order they happened.
    ///
    /// Failures are never the thing dropped. "The tests failed" is what a
    /// supervising listener is there for, and a turn long enough to overflow
    /// this is exactly the turn where they were not watching.
    fn wrap_up_activity(&self) -> Vec<String> {
        let failures = self
            .turn_activity
            .iter()
            .filter(|action| action.facts.outcome == ToolCallOutcome::Failed)
            .count();
        let others_kept = MAX_TURN_ACTIVITY.saturating_sub(failures);
        let mut skippable = self.turn_activity.len() - failures;
        self.turn_activity
            .iter()
            .filter(|action| {
                if action.facts.outcome == ToolCallOutcome::Failed {
                    return true;
                }
                // Keep the *last* `others_kept` of them: the oldest status is
                // the least useful in a sign-off.
                let keep = skippable <= others_kept;
                skippable -= 1;
                keep
            })
            .map(|action| action.description.clone())
            .collect()
    }

    /// Takes one finished assistant message. In `actions` detail it is
    /// condensed on its own; in `steps` it becomes the *why* of the step the
    /// tool calls after it will complete.
    ///
    /// `blocks` are the message's prose blocks in reading order. Only ever
    /// call this for a message that can no longer grow — a summary of half
    /// a message is worse than none.
    pub fn narrate_message(&mut self, blocks: Vec<Entity<Markdown>>, cx: &mut Context<Self>) {
        if self.mode != ReadAloudMode::Narration || self.stopped_by_user || blocks.is_empty() {
            return;
        }
        match self.detail {
            NarrationDetail::Actions => self.narrate_message_summary(blocks, cx),
            NarrationDetail::Steps => self.push_step_prose(blocks, cx),
        }
    }

    /// Condenses one finished message on its own — the behavior narration
    /// had before steps existed, still used by `actions` detail, by a step
    /// whose line could not be generated, and as the turn-end fallback.
    fn narrate_message_summary(&mut self, blocks: Vec<Entity<Markdown>>, cx: &mut Context<Self>) {
        if self.mode != ReadAloudMode::Narration || self.stopped_by_user {
            return;
        }
        let Some(first_block) = blocks.first() else {
            return;
        };
        // One model call per message, no matter how many completion
        // signals reach us for it.
        if self.summarized_messages.contains(&first_block.entity_id()) {
            return;
        }
        if !blocks.iter().all(|block| Self::parse_is_current(block, cx)) {
            self.wait_for_message_parse(blocks, ParkedMessageNarration::Summarize, cx);
            return;
        }
        self.summarized_messages.insert(first_block.entity_id());
        let source = Self::message_source(&blocks, cx);
        if source.trim().is_empty() {
            return;
        }
        if source.chars().count() <= TRIVIAL_MESSAGE_CHARS {
            // Condensing "Done, tests pass." costs a model call and a
            // second of latency to save nothing. Speaking the blocks
            // themselves also keeps the word highlight, which a summary
            // cannot have.
            for block in blocks {
                self.narration.push_inline(block, cx);
            }
            self.start_next_narration_if_idle(cx);
            return;
        }
        let Some(model) = self.summary_model.clone() else {
            self.narrate_opening_sentences(blocks, "no summary model is available", cx);
            return;
        };

        let prompt = narration::summary_prompt(&source);
        let message = first_block.entity_id();
        let task = cx.spawn(async move |this, cx| {
            let completion = cx.update(|cx| model.complete(prompt, cx));
            let timeout = cx.background_executor().timer(SUMMARY_TIMEOUT);
            let reply = futures::select_biased! {
                reply = completion.fuse() => reply,
                _ = timeout.fuse() => Err(anyhow::anyhow!(
                    "the summary model did not answer within {SUMMARY_TIMEOUT:?}"
                )),
            };
            this.update(cx, |this, cx| {
                this.summary_tasks.remove(&message);
                // One call per message: a failure falls back rather than
                // retrying, so a broken model costs one request per
                // message and not a stream of them.
                let summary = match &reply {
                    Ok(reply) => narration::clean_summary(reply),
                    Err(_) => None,
                };
                this.note_model_result(summary.is_some(), cx);
                match (summary, reply) {
                    (Some(summary), _) => this.narrate_summary(summary, blocks, cx),
                    (None, Ok(_)) => this.narrate_opening_sentences(
                        blocks,
                        "the summary model returned nothing speakable",
                        cx,
                    ),
                    (None, Err(error)) => {
                        this.narrate_opening_sentences(blocks, &format!("{error:#}"), cx);
                    }
                }
            })
            .log_err();
        });
        self.summary_tasks.insert(message, task);
    }

    /// Drops everything narration has lined up but not yet said, and any
    /// summary still being generated. Called when the user moves on — a new
    /// turn, a different thread — so status about what they left behind
    /// never arrives after the fact. What is sounding right now is left to
    /// finish its sentence.
    pub fn cancel_narration(&mut self, cx: &mut Context<Self>) {
        self.narration.clear();
        // A new turn is a new story: "do not repeat this" is only useful
        // about the one being told.
        self.narration.forget_recent();
        self.summary_tasks.clear();
        self.drop_narration_progress();
        self.narration_message_pending_parse = None;
        self.narration_parse_observations.clear();
        cx.notify();
    }

    /// Whether narration is holding nothing in progress. The stop gates
    /// alone would make a cancellation test pass without anything actually
    /// being cancelled, so the tests assert on the state itself.
    #[cfg(test)]
    fn narration_progress_is_idle(&self) -> bool {
        self.step.is_none()
            && self.closing_steps.is_empty()
            && self.step_idle_task.is_none()
            && self.step_tasks.is_empty()
            && self.turn_activity.is_empty()
            && self.turn_tool_calls == 0
            && self.turn_started_at.is_none()
            && self.wrap_up.is_none()
    }

    /// Drops everything narration is *accumulating*: the open step, the
    /// generations behind it, the turn's wrap-up, and the turn's raw
    /// material. Every caller that drops the queue drops this too — a step
    /// or a wrap-up that outlives the reason it was started is exactly the
    /// stale status this mode exists to avoid.
    fn drop_narration_progress(&mut self) {
        self.step = None;
        self.closing_steps.clear();
        self.step_idle_task = None;
        self.step_tasks.clear();
        self.turn_activity.clear();
        self.turn_tool_calls = 0;
        self.turn_started_at = None;
        self.wrap_up = None;
        self.wrap_up_issues = 0;
    }

    /// Parks a narration request until every block of the message has been
    /// parsed, then re-delivers it. Each block is observed: the one that
    /// lags is not necessarily the first.
    ///
    /// Only the newest such message is held. A parse lands within a frame,
    /// so a second message finishing inside that window means the agent has
    /// already moved on from the first — and late status is the thing this
    /// mode exists to avoid.
    fn wait_for_message_parse(
        &mut self,
        blocks: Vec<Entity<Markdown>>,
        then: ParkedMessageNarration,
        cx: &mut Context<Self>,
    ) {
        self.narration_parse_observations = blocks
            .iter()
            .map(|block| {
                cx.observe(block, |this, _, cx| {
                    let Some((blocks, then)) = this.narration_message_pending_parse.clone() else {
                        return;
                    };
                    if !blocks.iter().all(|block| Self::parse_is_current(block, cx)) {
                        return;
                    }
                    this.narration_message_pending_parse = None;
                    this.narration_parse_observations.clear();
                    match then {
                        ParkedMessageNarration::Summarize => {
                            this.narrate_message_summary(blocks, cx)
                        }
                        ParkedMessageNarration::Plainly { sentences } => {
                            this.narrate_message_plainly(blocks, sentences, cx)
                        }
                    }
                })
            })
            .collect();
        self.narration_message_pending_parse = Some((blocks, then));
    }

    /// Speaks a message without spending a model call on it: short messages
    /// as they stand, longer ones reduced to their opening sentences.
    ///
    /// This is where a step's templated fallback puts the prose, and where
    /// a failed wrap-up lands. It deliberately does *not* try the summary
    /// model: both callers reach it because a model has just failed or is
    /// standing aside, and a second round trip would add seconds to a line
    /// that is already late.
    fn narrate_message_plainly(
        &mut self,
        blocks: Vec<Entity<Markdown>>,
        sentences: usize,
        cx: &mut Context<Self>,
    ) {
        if self.mode != ReadAloudMode::Narration || self.stopped_by_user {
            return;
        }
        let Some(first_block) = blocks.first() else {
            return;
        };
        if self.summarized_messages.contains(&first_block.entity_id()) {
            return;
        }
        if !blocks.iter().all(|block| Self::parse_is_current(block, cx)) {
            self.wait_for_message_parse(blocks, ParkedMessageNarration::Plainly { sentences }, cx);
            return;
        }
        self.summarized_messages.insert(first_block.entity_id());
        let source = Self::message_source(&blocks, cx);
        if source.trim().is_empty() {
            return;
        }
        if source.chars().count() <= TRIVIAL_MESSAGE_CHARS {
            for block in blocks {
                self.narration.push_inline(block, cx);
            }
            self.start_next_narration_if_idle(cx);
            return;
        }
        let Some(opening) = Self::opening_sentences(&blocks, sentences, cx) else {
            return;
        };
        self.narrate_summary(opening, blocks, cx);
    }

    /// The step being accumulated, opening one if there is none.
    fn open_step(&mut self) -> &mut OpenStep {
        let number = self.next_step_number;
        let step = self.step.get_or_insert_with(|| OpenStep {
            number,
            prose: Vec::new(),
            prose_heads: Vec::new(),
            tool_calls: Vec::new(),
            spoken_prose: None,
            line: StepLine::default(),
        });
        if step.number == number {
            self.next_step_number += 1;
        }
        step
    }

    /// Adds a finished message's prose to the open step. Prose *after* tool
    /// calls is the agent starting on something new, so it closes the step
    /// it followed rather than joining it — that transition is what keeps
    /// one spoken line to one thing the agent is doing.
    fn push_step_prose(&mut self, blocks: Vec<Entity<Markdown>>, cx: &mut Context<Self>) {
        if self
            .step
            .as_ref()
            .is_some_and(|step| !step.tool_calls.is_empty())
        {
            self.close_step(cx);
        }
        let Some(head) = blocks.first().map(|block| block.entity_id()) else {
            return;
        };
        let step = self.open_step();
        if step.prose_heads.contains(&head) {
            // The same message can reach narration more than once (a tool
            // call ends it, and so does the turn); it is one step's worth of
            // prose either way.
            return;
        }
        step.prose_heads.push(head);
        step.prose.extend(blocks);
        self.arm_step_idle_timer(cx);
    }

    /// (Re)starts the quiet-window timer that closes an open step. Dropping
    /// the previous task cancels it, so activity always pushes the close
    /// out rather than stacking timers.
    ///
    /// This wakes nothing but the step: the line it produces is queued
    /// through the ordinary narration path, and the poll loop remains the
    /// one thing that drains the queue.
    fn arm_step_idle_timer(&mut self, cx: &mut Context<Self>) {
        self.step_idle_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(STEP_IDLE_WINDOW).await;
            this.update(cx, |this, cx| {
                this.step_idle_task = None;
                this.close_step(cx);
            })
            .log_err();
        }));
    }

    /// The agent is mid-action, but the action cannot be named yet: a tool
    /// call has appeared and its label is still arriving.
    ///
    /// Without this the quiet window races that wait. The step would close
    /// with no tool calls in it, fall through to a per-message summary — an
    /// extra model call the step design exists to avoid — and the call would
    /// then be spoken as a bare template with no step to belong to, which is
    /// exactly the Task 23 shape this task replaced.
    pub fn note_tool_call_pending(&mut self, cx: &mut Context<Self>) {
        if self.mode != ReadAloudMode::Narration
            || self.detail != NarrationDetail::Steps
            || self.stopped_by_user
            || self.step.is_none()
        {
            return;
        }
        // A tool call *existing* is what makes the prose in front of it a
        // step; naming it is a separate question, and a slower one. The
        // agent's own words do not have to wait on the label, and waiting
        // would put the whole settle delay on the time to the first word.
        self.speak_step_opening(cx);
        self.arm_step_idle_timer(cx);
    }

    /// Closes the open step. A no-op when no step is open.
    ///
    /// Closing costs no round trip: the line was issued when the step got
    /// its first tool call, so by now it is usually already sitting in
    /// `ready`. A step whose generation is still in flight waits in
    /// `closing_steps` rather than falling straight to the template — the
    /// generation has its own two-second budget, which started early.
    fn close_step(&mut self, cx: &mut Context<Self>) {
        self.step_idle_task = None;
        let Some(step) = self.step.take() else {
            return;
        };
        if self.mode != ReadAloudMode::Narration || self.stopped_by_user {
            return;
        }
        if step.tool_calls.is_empty() {
            // A message the agent finished without doing anything is not a
            // step, it is a message — and the per-message summary, with its
            // own prompt and its own trivial-message shortcut, is what it
            // wants. Steps exist to join an action to its reason; there is
            // no action here.
            self.narrate_message_summary(step.prose, cx);
            return;
        }
        if step.line.generating && step.line.ready.is_none() {
            self.closing_steps.insert(step.number, step);
            return;
        }
        self.speak_step_now(step, cx);
    }

    /// Speaks a step with whatever it has: the fused line if one was
    /// generated, the template otherwise. The single exit point for a step
    /// that is not going to wait any longer, so no caller can accidentally
    /// throw away a line that was already paid for.
    fn speak_step_now(&mut self, step: OpenStep, cx: &mut Context<Self>) {
        match step.line.ready.clone() {
            Some(line) => self.speak_step_line(line, step, cx),
            None => self.speak_step_template(step, cx),
        }
    }

    /// Speaks the agent's own opening words for the step that has just
    /// become real, straight away and with no model call.
    ///
    /// This is what gets the first word out inside a couple of seconds: the
    /// prose *is* the intent, it is already written, and a listener would
    /// rather hear the agent's own sentence now than a better one later. A
    /// message short enough to stand alone is spoken verbatim (which also
    /// keeps its word highlight); a longer one gives up its first sentence.
    fn speak_step_opening(&mut self, cx: &mut Context<Self>) {
        let Some(step) = self.step.as_ref() else {
            return;
        };
        if step.spoken_prose.is_some() || step.prose.is_empty() {
            return;
        }
        let prose = step.prose.clone();
        let source = Self::message_source(&prose, cx);
        // Mirror what `narrate_message_plainly` is about to say, so the
        // suppression check compares against the real thing.
        let opening = if source.chars().count() <= TRIVIAL_MESSAGE_CHARS {
            source
        } else {
            Self::opening_sentences(&prose, 1, cx).unwrap_or(source)
        };
        if let Some(step) = self.step.as_mut() {
            step.spoken_prose = Some(opening);
        }
        self.narrate_message_plainly(prose, 1, cx);
    }

    /// Starts a step's line as soon as there is something to say, rather
    /// than when the step closes. The quiet window then only decides *when*
    /// the answer is spoken, never when the work starts — the same
    /// speculative shape the turn wrap-up uses.
    ///
    /// A re-issue waits for the first generation to answer: a burst of tool
    /// calls arriving milliseconds apart would otherwise spend both of a
    /// step's generations inside a tenth of a second and describe neither.
    fn maybe_generate_step_line(&mut self, cx: &mut Context<Self>) {
        if self.mode != ReadAloudMode::Narration || self.stopped_by_user {
            return;
        }
        let Some(step) = self.step.as_ref() else {
            return;
        };
        if step.tool_calls.is_empty()
            || step.prose.is_empty()
            || step.line.generating
            || step.line.exhausted
            || step.line.issues >= MAX_STEP_LINE_ISSUES
            || step.tool_calls.len() <= step.line.issued_calls
        {
            return;
        }
        let prose = Self::message_source(&step.prose, cx);
        let Some(model) = self
            .summary_model
            .clone()
            .filter(|_| !prose.trim().is_empty())
        else {
            if let Some(step) = self.step.as_mut() {
                step.line.exhausted = true;
            }
            return;
        };
        let tool_lines: Vec<String> = step
            .tool_calls
            .iter()
            .map(|call| call.description.clone())
            .collect();
        let number = step.number;
        let calls = step.tool_calls.len();
        let prompt = narration::step_prompt(&prose, &tool_lines, &self.narration.recent_lines());
        let task = cx.spawn(async move |this, cx| {
            let completion = cx.update(|cx| model.complete(prompt, cx));
            let fallback = cx.background_executor().timer(STEP_FALLBACK_DELAY);
            // Losing the race drops the completion future, which cancels the
            // request: the late answer can neither be spoken nor paid for.
            let reply = futures::select_biased! {
                reply = completion.fuse() => reply,
                _ = fallback.fuse() => Err(anyhow::anyhow!(
                    "the summary model did not answer a step within {STEP_FALLBACK_DELAY:?}"
                )),
            };
            this.update(cx, |this, cx| {
                this.step_tasks.remove(&number);
                let line = reply
                    .log_err()
                    .and_then(|reply| narration::clean_step_line(&reply));
                this.note_model_result(line.is_some(), cx);
                this.deliver_step_line(number, line, cx);
            })
            .log_err();
        });
        self.step_tasks.insert(number, task);
        if let Some(step) = self.step.as_mut() {
            step.line.generating = true;
            step.line.issues += 1;
            step.line.issued_calls = calls;
        }
    }

    /// Takes a step generation's answer. The step is either still open — in
    /// which case the answer waits for it to close, and may yet be replaced
    /// by a re-issue covering the calls that have joined since — or it has
    /// already closed and is waiting in `closing_steps` to be spoken.
    fn deliver_step_line(&mut self, number: usize, line: Option<String>, cx: &mut Context<Self>) {
        if let Some(step) = self.step.as_mut().filter(|step| step.number == number) {
            step.line.generating = false;
            match line {
                Some(line) => step.line.ready = Some(line),
                None => step.line.exhausted = true,
            }
            // More calls may have joined while that ran; if so, the answer
            // in hand describes only part of the step.
            self.maybe_generate_step_line(cx);
            return;
        }
        let Some(mut step) = self.closing_steps.remove(&number) else {
            return;
        };
        step.line.generating = false;
        match line {
            Some(line) => self.speak_step_line(line, step, cx),
            None => self.speak_step_template(step, cx),
        }
    }

    fn speak_step_line(&mut self, line: String, step: OpenStep, cx: &mut Context<Self>) {
        if self.mode != ReadAloudMode::Narration || self.stopped_by_user {
            return;
        }
        // The agent's own words have usually gone out already. A fused line
        // that only says them again is the "choppy, repetitive" failure in
        // its other form, so it gives way to the tool calls' terse template,
        // which at least names what is actually happening.
        if let Some(spoken) = step.spoken_prose.as_deref()
            && narration::adds_nothing(&line, spoken)
        {
            log::debug!("read_aloud: a step line only restated prose already spoken; skipping it");
            self.speak_step_template(step, cx);
            return;
        }
        self.mark_narrated(&step);
        let spoken = cx.new(|cx| Markdown::new(line.into(), None, None, cx));
        // The line is about the prose the agent wrote, which is what is on
        // screen and what the drill-down click has to land on.
        self.narration.push_summary(spoken, step.prose, cx);
        self.start_next_narration_if_idle(cx);
    }

    /// The step's fallback: the prose spoken plainly, then its tool calls as
    /// templated lines. This is exactly the shape narration had before steps
    /// existed, which is what makes "no model configured" degrade to a
    /// working feature rather than to silence.
    ///
    /// When the step already spoke its opening, `narrate_message_plainly` is
    /// a no-op for it — the message is marked — so only the tool calls are
    /// added, which is exactly what is left to say.
    fn speak_step_template(&mut self, step: OpenStep, cx: &mut Context<Self>) {
        if self.mode != ReadAloudMode::Narration || self.stopped_by_user {
            return;
        }
        // Reason before action: it is the order a person tells it in.
        if !step.prose.is_empty() {
            self.narrate_message_plainly(step.prose, narration::OPENING_SENTENCES, cx);
        }
        for call in step.tool_calls {
            self.narration.push_tool_call(call.facts, cx);
        }
        self.start_next_narration_if_idle(cx);
    }

    /// Records that a step has spoken for its messages, so the per-message
    /// summary path never says them a second time.
    fn mark_narrated(&mut self, step: &OpenStep) {
        for head in &step.prose_heads {
            self.summarized_messages.insert(*head);
        }
    }

    /// The agent has done something, so it had not finished after all.
    ///
    /// A wrap-up is speculated from prose that is currently the last entry,
    /// which is true of *every* prose block while it streams — the tool call
    /// that follows it does not exist yet. Without this, a mid-turn
    /// paragraph long enough to cross the threshold produces a sign-off
    /// about the middle of the turn, which `finish_turn` then speaks as the
    /// turn's last word while discarding the status that was actually
    /// current.
    ///
    /// The generation is cancelled rather than kept: it describes a turn
    /// that had not happened yet. The turn-level issue count is deliberately
    /// *not* refunded, so an agent that alternates prose and tool calls
    /// cannot spend a generation per block; a turn that uses up its budget
    /// this way falls back to the per-message summary, which is the
    /// documented last rung.
    fn invalidate_speculated_wrap_up(&mut self) {
        let Some(wrap_up) = self.wrap_up.as_mut() else {
            return;
        };
        if wrap_up.delivered || wrap_up.awaiting_turn_end {
            return;
        }
        self.wrap_up = None;
    }

    /// Whether a speculative wrap-up is worth issuing for a closing message
    /// this long. Deliberately cheap: the owning view asks this on every
    /// streaming chunk, before it does anything more expensive.
    pub fn wants_wrap_up(&self, message_chars: usize) -> bool {
        if self.mode != ReadAloudMode::Narration
            || self.detail != NarrationDetail::Steps
            || self.stopped_by_user
        {
            return false;
        }
        // Nothing to wrap up until the agent has actually done something. A
        // turn that is only prose is a message, and the per-message summary
        // already covers it.
        if self.turn_activity.is_empty() {
            return false;
        }
        if self.wrap_up_issues >= MAX_WRAP_UP_ISSUES {
            return false;
        }
        match self.wrap_up.as_ref() {
            // Either no wrap-up yet this turn, or the last one was thrown
            // away because the agent went back to work. Both are "start
            // fresh on this message".
            None => message_chars >= WRAP_UP_MIN_CHARS,
            Some(wrap_up) => {
                !wrap_up.delivered
                    && !wrap_up.spent
                    && message_chars >= wrap_up.issued_chars + WRAP_UP_REISSUE_GROWTH
            }
        }
    }

    /// Starts generating the turn's wrap-up *before* the turn has ended, so
    /// the audio is ready the instant it does instead of a model round trip
    /// after it. Called while the agent streams its closing prose.
    ///
    /// `message_chars` is the closing message's length, which the caller has
    /// already measured for [`Self::wants_wrap_up`].
    pub fn speculate_wrap_up(
        &mut self,
        blocks: Vec<Entity<Markdown>>,
        message_chars: usize,
        cx: &mut Context<Self>,
    ) {
        if !self.wants_wrap_up(message_chars) {
            return;
        }
        self.issue_wrap_up(blocks, true, cx);
    }

    /// How much of a wrap-up this turn has earned, from how much it did and
    /// how long the listener has been waiting.
    fn wrap_up_budget(&self, cx: &App) -> WrapUpBudget {
        let elapsed = self.turn_started_at.map(|started| {
            cx.background_executor()
                .now()
                .saturating_duration_since(started)
        });
        WrapUpBudget::for_turn(self.turn_tool_calls, elapsed)
    }

    /// The files this turn changed, named once each and in the order they
    /// were first touched. A turn that edited the same file six times has
    /// changed one file, and saying so is the difference between a wrap-up
    /// that sounds like a person and one that sounds like a log.
    /// Bounded like the activity list, and for the same reason: the turn's
    /// account is remembered in full but only a prompt's worth of it is sent.
    fn files_changed_this_turn(&self, cx: &App) -> Vec<String> {
        let mut files: Vec<String> = Vec::new();
        for action in &self.turn_activity {
            if files.len() >= MAX_TURN_ACTIVITY {
                break;
            }
            if !matches!(
                action.facts.kind,
                NarrationKind::Edit | NarrationKind::Delete | NarrationKind::Move
            ) {
                continue;
            }
            let named = match action.facts.path.as_deref().map(str::trim) {
                Some(path) if !path.is_empty() => path.to_string(),
                _ => action.facts.label.read(cx).source().trim().to_string(),
            };
            let file: String = named.chars().take(narration::MAX_ACTION_CHARS).collect();
            if !file.is_empty() && !files.contains(&file) {
                files.push(file);
            }
        }
        files
    }

    fn issue_wrap_up(
        &mut self,
        blocks: Vec<Entity<Markdown>>,
        still_streaming: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(model) = self.summary_model.clone() else {
            // Recorded rather than ignored, so the owning view stops asking
            // on every streaming chunk of every message.
            self.wrap_up = Some(WrapUp::exhausted());
            return;
        };
        let message = Self::message_source(&blocks, cx);
        let issued_chars = message.len();
        let activity = self.wrap_up_activity();
        let files_changed = self.files_changed_this_turn(cx);
        let budget = self.wrap_up_budget(cx);
        let prompt = narration::wrap_up_prompt(narration::WrapUpMaterial {
            message: &message,
            still_streaming,
            activity: &activity,
            files_changed: &files_changed,
            recent: &self.narration.recent_lines(),
            budget,
        });
        let task = cx.spawn(async move |this, cx| {
            let completion = cx.update(|cx| model.complete(prompt, cx));
            let timeout = cx.background_executor().timer(WRAP_UP_TIMEOUT);
            let reply = futures::select_biased! {
                reply = completion.fuse() => reply,
                _ = timeout.fuse() => Err(anyhow::anyhow!(
                    "the summary model did not answer the wrap-up within {WRAP_UP_TIMEOUT:?}"
                )),
            };
            this.update(cx, |this, cx| {
                let line = reply
                    .log_err()
                    .and_then(|reply| narration::clean_wrap_up(&reply, budget));
                this.note_model_result(line.is_some(), cx);
                let Some(wrap_up) = this.wrap_up.as_mut() else {
                    return;
                };
                wrap_up.task = None;
                let awaiting_turn_end = wrap_up.awaiting_turn_end;
                match line {
                    Some(line) => {
                        wrap_up.ready = Some(line);
                        if awaiting_turn_end {
                            this.deliver_wrap_up(blocks, cx);
                        }
                    }
                    None if awaiting_turn_end => {
                        // No wrap-up is coming. Whatever step lines are
                        // still queued are the only account of this turn the
                        // listener will get, so they are deliberately left
                        // alone, and the closing message's own opening
                        // follows them. Going back to the model for a
                        // per-message summary would add seconds to a turn
                        // that has already ended, from a model that has just
                        // failed to answer.
                        this.wrap_up = Some(WrapUp::exhausted());
                        this.narrate_message_plainly(blocks, narration::OPENING_SENTENCES, cx);
                    }
                    None => {}
                }
            })
            .log_err();
        });
        self.wrap_up_issues += 1;
        let wrap_up = self.wrap_up.get_or_insert_with(WrapUp::default);
        wrap_up.issued_chars = issued_chars;
        wrap_up.task = Some(task);
    }

    /// Speaks the finished wrap-up, displacing anything queued behind it.
    fn deliver_wrap_up(&mut self, blocks: Vec<Entity<Markdown>>, cx: &mut Context<Self>) {
        if self.mode != ReadAloudMode::Narration || self.stopped_by_user {
            return;
        }
        let Some(wrap_up) = self.wrap_up.as_mut() else {
            return;
        };
        let Some(line) = wrap_up.ready.take() else {
            return;
        };
        wrap_up.delivered = true;
        wrap_up.awaiting_turn_end = false;
        // A superseded second speculation cannot speak, but it is a paid
        // request whose answer nobody will read: the cost rule says a
        // generation that loses is cancelled, not left running.
        wrap_up.task = None;
        // Status about work that has already finished is worse than silence
        // once the wrap-up can be said, so queued step lines give way to it.
        // Only what has not started speaking is dropped: the queue holds
        // what is *next*, never what is sounding, so nothing is cut
        // mid-word.
        self.narration.clear();
        // A step's prose parked on a lagging parse would otherwise resolve
        // afterwards and append status *behind* the sign-off, which is the
        // one thing this is supposed to make impossible.
        self.narration_message_pending_parse = None;
        self.narration_parse_observations.clear();
        if let Some(first_block) = blocks.first() {
            self.summarized_messages.insert(first_block.entity_id());
        }
        let spoken = cx.new(|cx| Markdown::new(line.into(), None, None, cx));
        self.narration.push_summary(spoken, blocks, cx);
        self.start_next_narration_if_idle(cx);
    }

    /// The turn has ended. In `actions` detail this condenses the turn's
    /// last message, exactly as before steps existed. In `steps` it delivers
    /// the wrap-up: a cohesive account of the whole turn rather than a
    /// re-run of its status lines.
    pub fn finish_turn(&mut self, blocks: Vec<Entity<Markdown>>, cx: &mut Context<Self>) {
        if self.mode != ReadAloudMode::Narration || self.stopped_by_user {
            return;
        }
        if self.detail == NarrationDetail::Actions {
            self.narrate_message_summary(blocks, cx);
            return;
        }
        self.step_idle_task = None;
        // A step line generated after the turn ended would be status about
        // work the wrap-up is about to describe. Steps that closed waiting
        // on one go with them: their tool calls are in `turn_activity`, so
        // the wrap-up still knows about them.
        self.step_tasks.clear();
        let closed_waiting: Vec<OpenStep> = self.closing_steps.drain().map(|(_, s)| s).collect();

        if self.turn_activity.is_empty() {
            // The agent wrote and did nothing: there is no turn to wrap up,
            // only a message, and the per-message summary is what a message
            // wants. `close_step` routes a prose-only step there, and the
            // second call is a no-op for the message it already covered.
            self.close_step(cx);
            self.narrate_message_summary(blocks, cx);
            return;
        }

        if self
            .wrap_up
            .as_ref()
            .is_some_and(|wrap_up| wrap_up.ready.is_some())
        {
            // The speculative issue paid off: the audio is ready before the
            // turn-end event. The open step is dropped rather than spoken —
            // the wrap-up covers it, and a status line in front of it would
            // delay the thing the listener is waiting for.
            self.step = None;
            drop(closed_waiting);
            self.deliver_wrap_up(blocks, cx);
            return;
        }

        // Nothing to say yet, so the last steps are spoken in their cheap
        // templated form rather than dropped. They keep the audio flowing
        // while the wrap-up generates, and `deliver_wrap_up` drops them
        // again if they are still queued when the wrap-up lands. Silence
        // here is the "lagged and choppy" failure this mode is trying to fix.
        for step in closed_waiting {
            self.speak_step_now(step, cx);
        }
        if let Some(step) = self.step.take() {
            // Not `speak_step_template`: the whole point of generating a
            // step's line while the step is still open is that it is usually
            // already in hand by the time anything closes the step, and the
            // turn ending is no reason to throw away a line that was paid
            // for and is about to be the listener's last word on that step.
            self.speak_step_now(step, cx);
        }

        if let Some(wrap_up) = self.wrap_up.as_mut()
            && wrap_up.task.is_some()
        {
            wrap_up.awaiting_turn_end = true;
            return;
        }

        let spent = self.wrap_up_issues >= MAX_WRAP_UP_ISSUES
            || self
                .wrap_up
                .as_ref()
                .is_some_and(|wrap_up| wrap_up.delivered || wrap_up.spent);
        if self.summary_model.is_none() || spent {
            // The documented fallback chain's last rung: the per-message
            // summary, which with no model resolves to the message's own
            // opening sentences.
            self.narrate_message_summary(blocks, cx);
            return;
        }
        self.issue_wrap_up(blocks, false, cx);
        if let Some(wrap_up) = self.wrap_up.as_mut() {
            wrap_up.awaiting_turn_end = true;
        }
    }

    fn narrate_summary(
        &mut self,
        summary: String,
        blocks: Vec<Entity<Markdown>>,
        cx: &mut Context<Self>,
    ) {
        let spoken = cx.new(|cx| Markdown::new(summary.into(), None, None, cx));
        self.narration.push_summary(spoken, blocks, cx);
        self.start_next_narration_if_idle(cx);
    }

    /// The fallback whenever a summary cannot be had: speak the message's
    /// own opening sentences. Not a summary, but it names the subject,
    /// which is most of what an ambient listener needs — and it degrades to
    /// something useful rather than to silence.
    fn narrate_opening_sentences(
        &mut self,
        blocks: Vec<Entity<Markdown>>,
        reason: &str,
        cx: &mut Context<Self>,
    ) {
        if !self.logged_summary_fallback {
            self.logged_summary_fallback = true;
            log::warn!(
                "read_aloud: narration is speaking message openings instead of summaries \
                 ({reason}); logged once per reader"
            );
        }
        let Some(opening) = Self::opening_sentences(&blocks, narration::OPENING_SENTENCES, cx)
        else {
            return;
        };
        self.narrate_summary(opening, blocks, cx);
    }

    /// A message's prose blocks joined the way a reader would see them.
    fn message_source(blocks: &[Entity<Markdown>], cx: &App) -> String {
        blocks
            .iter()
            .map(|block| block.read(cx).source().to_string())
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    /// The message's first [`narration::OPENING_SENTENCES`] spoken
    /// sentences, as source text so the substitute rules apply to it once
    /// (rather than to already-substituted text).
    fn opening_sentences(blocks: &[Entity<Markdown>], wanted: usize, cx: &App) -> Option<String> {
        let mut opening = String::new();
        let mut sentences = 0;
        for block in blocks {
            let block = block.read(cx);
            let source = block.source();
            for utterance in segment(block.parsed_markdown(), true) {
                // A stale parse can outrun the source it was made from;
                // there is no sentence to take in that case.
                let Some(text) = source.get(utterance.source_range.clone()) else {
                    continue;
                };
                if !opening.is_empty() {
                    opening.push(' ');
                }
                opening.push_str(text);
                sentences += 1;
                if sentences >= wanted {
                    break;
                }
            }
            if sentences >= wanted {
                break;
            }
        }
        (!opening.trim().is_empty()).then_some(opening)
    }

    /// Starts the next queued narration when there is a gap to put it in.
    /// Called both on queueing and from the poll loop's idle branch, so a
    /// backlog drains without a timer of its own.
    fn start_next_narration_if_idle(&mut self, cx: &mut Context<Self>) {
        if self.speaking.is_some() && !self.player.read(cx).is_idle() {
            return;
        }
        // Starting a narration also starts the poll loop, which is what
        // eventually drains everything queued behind it.
        self.start_polling(cx);
        // The full-prose FIFO owns the floor while it has anything in it —
        // a message the user explicitly asked for is not interrupted by
        // status.
        if !self.pending.is_empty() {
            return;
        }
        self.start_next_narration(cx);
    }

    /// Pops one narration and speaks it. Returns whether the poll loop
    /// should keep running — either something started, or something is
    /// queued behind a narration that has not found its voice yet.
    fn start_next_narration(&mut self, cx: &mut Context<Self>) -> bool {
        if self.mode != ReadAloudMode::Narration || self.stopped_by_user {
            return false;
        }
        if self.narration.is_empty() {
            return false;
        }
        // A narration only just handed to the player looks idle: its entity
        // is still being parsed, so there are no utterances yet to be busy
        // with. Taking the floor here would drop it before it made a sound —
        // the same truncation the cross-entity queue exists to prevent, and
        // easy to hit because a summary and the tool call after it land
        // within a tick of each other.
        if let Some(speaking) = self.speaking.clone()
            && !Self::parse_is_current(&speaking, cx)
        {
            return true;
        }
        let Some(next) = self.narration.pop() else {
            return false;
        };
        // `switch_to` clears the previous narration's wash, so the new one
        // is only recorded once it has taken over.
        self.switch_to(next.spoken, true, cx);
        self.narration_wash = next.wash;
        // `switch_to` already pushed ranges, but for the spoken entity —
        // which, with a wash set, is not the thing on screen. Re-push now
        // that the wash is known so the drill-down has its affordance.
        self.push_speakable_ranges(cx);
        true
    }

    fn clear_narration_wash(&mut self, cx: &mut Context<Self>) {
        for block in std::mem::take(&mut self.narration_wash) {
            block.update(cx, |markdown, cx| {
                markdown.set_speaking_highlight(None, cx);
                markdown.set_speaking_word_highlight(None, cx);
                // The drill-down affordance belongs to whatever narration is
                // speaking about right now, exactly as it belongs to the
                // speaking message in full mode.
                markdown.set_speakable_ranges(Vec::new(), cx);
            });
        }
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
        // overruled by it. In narration mode this is the drill-down — the
        // clicked message plays as full prose, with normal karaoke, and
        // narration resumes with the next thing the agent does.
        self.pending.clear();
        self.step_narration_aside(cx);
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
                    // Narration items drain through the same gap, so a
                    // backlog of status needs no timer of its own.
                    if this.start_next_narration(cx) {
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
        if !self.narration_wash.is_empty() {
            // A summary's words exist nowhere in the document, so there is
            // nothing to track sentence by sentence. Washing the whole
            // message instead tells the listener which message the spoken
            // line is about, which is the only mapping that exists.
            for block in self.narration_wash.clone() {
                let range = index.map(|_| 0..block.read(cx).source().len());
                block.update(cx, |markdown, cx| {
                    markdown.set_speaking_highlight(range.clone(), cx);
                    markdown.set_speaking_word_highlight(None, cx);
                });
            }
            return;
        }
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
        if !self.narration_wash.is_empty() {
            // No word pill while a summary speaks: its words map to nothing
            // on screen.
            return;
        }
        if let Some(markdown) = self.speaking.clone() {
            markdown.update(cx, |markdown, cx| {
                markdown.set_speaking_word_highlight(range, cx);
            });
        }
    }

    fn clear_highlight(&mut self, cx: &mut Context<Self>) {
        self.clear_narration_wash(cx);
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
        assert_eq!(
            settings.pill_colors, None,
            "absent means the default palette"
        );
        assert!(settings.click_to_seek, "sentence clicks seek by default");
        assert_eq!(
            settings.mode,
            ReadAloudMode::Full,
            "narration is opt-in: an existing profile keeps hearing full prose"
        );
        assert!(settings.narrate_tool_calls);
        assert_eq!(settings.summary_model, None);
    }

    /// The wire format documented in `default.json`. A typo in the serde
    /// rename would leave the setting silently unreadable, and narration
    /// would sit on its default with nothing to say why.
    #[test]
    fn narration_detail_reads_the_names_default_json_documents() {
        for (written, expected) in [
            ("\"steps\"", NarrationDetail::Steps),
            ("\"actions\"", NarrationDetail::Actions),
        ] {
            let content: settings::SettingsContent = serde_json::from_str(&format!(
                "{{ \"read_aloud\": {{ \"narration_detail\": {written} }} }}"
            ))
            .expect("the documented name deserializes");
            assert_eq!(
                ReadAloudSettings::from_settings(&content).narration_detail,
                expected
            );
        }
        assert_eq!(
            ReadAloudSettings::from_settings(&settings::SettingsContent::default())
                .narration_detail,
            NarrationDetail::Steps,
            "steps is the default, because saying why is what was asked for"
        );
    }

    #[test]
    fn pill_colors_parse_valid_hex_forms() {
        let solid = resolve_pill_colors(&["#A855F7".to_string()]).expect("one color is valid");
        assert_eq!(solid.0, solid.1, "one color paints a solid pill");

        let gradient = resolve_pill_colors(&["#A855F7".to_string(), "#EC4899".to_string()])
            .expect("two colors are valid");
        assert_ne!(gradient.0, gradient.1);

        let shorthand = resolve_pill_colors(&["#F0A".to_string()]).expect("#RGB is valid");
        let longhand = resolve_pill_colors(&["#FF00AA".to_string()]).expect("#RRGGBB is valid");
        assert_eq!(shorthand, longhand, "#RGB expands each nibble");

        assert_eq!(
            resolve_pill_colors(&["a855f7".to_string()]),
            resolve_pill_colors(&["#A855F7".to_string()]),
            "the leading # is optional and hex is case-insensitive"
        );
        assert_eq!(
            resolve_pill_colors(&["#A855F7FF".to_string()]),
            resolve_pill_colors(&["#A855F7".to_string()]),
            "an alpha component is accepted but ignored"
        );
    }

    #[test]
    fn pill_colors_fall_back_to_the_default_on_bad_input() {
        assert_eq!(resolve_pill_colors(&[]), None, "empty array");
        assert_eq!(
            resolve_pill_colors(&[
                "#111111".to_string(),
                "#222222".to_string(),
                "#333333".to_string()
            ]),
            None,
            "more than two colors"
        );
        assert_eq!(
            resolve_pill_colors(&["not a color".to_string()]),
            None,
            "non-hex text"
        );
        assert_eq!(
            resolve_pill_colors(&["#AB".to_string()]),
            None,
            "wrong length"
        );
        assert_eq!(
            resolve_pill_colors(&["#A855F7".to_string(), "#XYZXYZ".to_string()]),
            None,
            "one bad entry rejects the whole pair"
        );
    }

    #[gpui::test]
    async fn click_to_seek_gates_the_speakable_ranges(cx: &mut TestAppContext) {
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
        assert!(
            !markdown.read_with(cx, |markdown, _| markdown.speakable_ranges().is_empty()),
            "setup: the speaking entity advertises its sentences"
        );

        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.set_click_to_seek(false, cx);
        });
        assert!(
            markdown.read_with(cx, |markdown, _| markdown.speakable_ranges().is_empty()),
            "disabling click-to-seek clears the affordance immediately"
        );

        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.set_click_to_seek(true, cx);
        });
        assert!(
            !markdown.read_with(cx, |markdown, _| markdown.speakable_ranges().is_empty()),
            "re-enabling restores the affordance without a new enqueue"
        );
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

    /// A narrating reader in `actions` detail: one templated line per tool
    /// call, one summary per finished message, no step buffering. Every test
    /// below that hands over a tool call or a message and expects to hear
    /// about it immediately is pinning *that* contract, which is what
    /// `actions` now names.
    /// A tool call with nothing but a title behind it — the Zed-native
    /// shape, where the title carries the command or the path. External ACP
    /// agents send generic titles and put the content in structured fields;
    /// those are exercised by the tests that build [`ToolCallFacts`]
    /// directly.
    fn titled_call(label: Entity<Markdown>, kind: NarrationKind) -> ToolCallFacts {
        ToolCallFacts::from_label("call", label, kind)
    }

    fn narration_reader(
        provider: &FakeTts,
        sink: &FakeSink,
        cx: &mut TestAppContext,
    ) -> Entity<ReadAloud> {
        let read_aloud = steps_narration_reader(provider, sink, cx);
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.set_narration_detail(NarrationDetail::Actions, cx);
        });
        read_aloud
    }

    /// A narrating reader in the default `steps` detail.
    fn steps_narration_reader(
        provider: &FakeTts,
        sink: &FakeSink,
        cx: &mut TestAppContext,
    ) -> Entity<ReadAloud> {
        let read_aloud = cx.new({
            let provider = provider.clone();
            let sink = sink.clone();
            |cx| ReadAloud::for_test(Arc::new(provider), Box::new(sink), cx)
        });
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.set_mode(ReadAloudMode::Narration, cx);
        });
        read_aloud
    }

    /// Lets a step's quiet window pass, so the line it produces exists.
    fn let_the_step_close(cx: &mut TestAppContext) {
        cx.executor()
            .advance_clock(STEP_IDLE_WINDOW + Duration::from_millis(100));
        cx.run_until_parked();
    }

    /// Lets a step's model call miss its latency budget, so the templated
    /// fallback takes over.
    fn let_the_step_time_out(cx: &mut TestAppContext) {
        cx.executor()
            .advance_clock(STEP_FALLBACK_DELAY + Duration::from_millis(100));
        cx.run_until_parked();
    }

    fn markdown_entity(source: &str, cx: &mut TestAppContext) -> Entity<Markdown> {
        let markdown = cx.new(|cx| Markdown::new(source.to_string().into(), None, None, cx));
        cx.run_until_parked();
        markdown
    }

    /// Pins how the tool-call labels agents actually send come out. Read,
    /// edit, execute and fetch labels are re-spoken as a phrase naming just
    /// their subject; search, move, delete and the rest already read as
    /// verb-led prose and go through the segmenter in place.
    #[gpui::test]
    async fn realistic_tool_labels_are_spoken_through_the_existing_path(cx: &mut TestAppContext) {
        // The titles Zed's own tools actually send (`crates/agent/src/tools`),
        // plus the shapes an external ACP agent's terminal calls take.
        for (kind, label, expected) in [
            (
                NarrationKind::Read,
                "Read file `crates/read_aloud/src/player.rs`",
                "Reading player.",
            ),
            (
                // The line range is noise: the listener wants the file.
                NarrationKind::Read,
                "Read file `crates/read_aloud/src/player.rs` (lines 10-40)",
                "Reading player.",
            ),
            (
                // A deep path with words in its name, spoken the way the
                // brief asks for: the base name, extension dropped.
                NarrationKind::Read,
                "Read file `docs/design/convex-clickhouse-sync-design.md`",
                "Reading convex clickhouse sync design.",
            ),
            (
                // No code span at all — an agent that writes the path inline.
                NarrationKind::Read,
                "Read file crates/read_aloud/src/segmenter.rs",
                "Reading segmenter.",
            ),
            (
                NarrationKind::Edit,
                "crates/agent\\_ui/src/thread\\_view.rs",
                "Editing thread view.",
            ),
            (
                // A path with non-ASCII in its name still speaks.
                NarrationKind::Edit,
                "crates/über/día\\_uno.rs",
                "Editing día uno.",
            ),
            (
                // The placeholder that arrives before the tool's input has
                // finished streaming. It must never come out as the bare
                // past-tense fragment "read file".
                NarrationKind::Read,
                "Read file",
                "Reading a file.",
            ),
            (
                NarrationKind::Search,
                "Search files for regex `fn main`",
                "Search files for regex fn main",
            ),
            (
                // `MarkdownInlineCode` wraps the paths without escaping
                // them, so both speak as their base names.
                NarrationKind::Move,
                "Rename `crates/a/old_name.rs` to `new_name.rs`",
                "Rename old name to new name",
            ),
            (
                NarrationKind::Execute,
                "cargo test --workspace",
                "Running cargo test.",
            ),
            (
                NarrationKind::Execute,
                "git log --oneline -n 20 --stat --graph",
                "Running git log.",
            ),
            (
                // A flag right after the program leaves just the program.
                NarrationKind::Execute,
                "grep -rn \"narrate\" crates/",
                "Running grep.",
            ),
            (
                // The `cd … &&` prefix an agent adds is scaffolding.
                NarrationKind::Execute,
                "cd /Users/joshua/code/zed && cargo test -p read_aloud",
                "Running cargo test.",
            ),
            (
                // A pipeline is not recited.
                NarrationKind::Execute,
                "ps aux | grep zed | head -20",
                "Running ps aux.",
            ),
            (
                // A quoted argument is content, not identity.
                NarrationKind::Execute,
                "git commit -m \"a long commit message\"",
                "Running git commit.",
            ),
            (
                NarrationKind::Execute,
                "echo \"héllo wörld\" > /tmp/out.txt",
                "Running echo.",
            ),
            (
                NarrationKind::Execute,
                "RUST_LOG=debug ./script/clippy",
                "Running clippy.",
            ),
            (
                // Nothing sayable at all still says something.
                NarrationKind::Execute,
                "$(cat cmd.txt)",
                "Running a command.",
            ),
            (
                // The escaping used to defeat the URL substitute entirely.
                NarrationKind::Fetch,
                "Fetch https://docs.inworld.ai/tts/a\\_b",
                "Fetching docs inworld.",
            ),
            (
                NarrationKind::Other,
                "List the `crates/agent` directory's contents",
                "List the agent directory\u{2019}s contents",
            ),
        ] {
            let provider = FakeTts::new();
            let sink = FakeSink::new();
            let read_aloud = narration_reader(&provider, &sink, cx);
            // `acp_thread` builds an execute label as plain text and every
            // other kind as markdown; narration speaks whichever it is
            // handed, so the test has to make the same choice.
            let markdown = if kind == NarrationKind::Execute {
                let markdown = cx.new(|cx| Markdown::new_text(label.to_string().into(), cx));
                cx.run_until_parked();
                markdown
            } else {
                markdown_entity(label, cx)
            };
            read_aloud.update(cx, |read_aloud, cx| {
                read_aloud.narrate_tool_call(titled_call(markdown.clone(), kind), cx);
            });
            cx.run_until_parked();
            assert_eq!(
                provider.spoken(),
                vec![expected.to_string()],
                "label {label:?}"
            );
        }
    }

    #[gpui::test]
    async fn a_bare_path_label_is_washed_even_though_its_phrase_is_generated(
        cx: &mut TestAppContext,
    ) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let read_aloud = narration_reader(&provider, &sink, cx);
        let label = markdown_entity("crates/read_aloud/src/player.rs", cx);

        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.narrate_tool_call(titled_call(label.clone(), NarrationKind::Edit), cx);
        });
        cx.run_until_parked();
        let source_length = label.read_with(cx, |markdown, _| markdown.source().len());
        assert_eq!(
            label.read_with(cx, |markdown, _| markdown.speaking_highlight().cloned()),
            Some(0..source_length),
            "the tool call on screen still lights up while its phrase is spoken"
        );
    }

    #[gpui::test]
    async fn full_mode_never_narrates(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let read_aloud = cx.new({
            let provider = provider.clone();
            |cx| ReadAloud::for_test(Arc::new(provider), Box::new(sink), cx)
        });
        assert_eq!(
            read_aloud.read_with(cx, |read_aloud, _| read_aloud.mode()),
            ReadAloudMode::Full,
            "a reader starts in full mode"
        );

        let label = markdown_entity("Read player.rs", cx);
        let message = markdown_entity(&format!("{} sentence.\n", "A long ".repeat(60)), cx);
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.narrate_tool_call(titled_call(label.clone(), NarrationKind::Read), cx);
            read_aloud.narrate_message(vec![message], cx);
        });
        cx.run_until_parked();
        assert!(
            provider.spoken().is_empty(),
            "narration must be inert in full mode, got {:?}",
            provider.spoken()
        );
    }

    #[gpui::test]
    async fn the_tool_label_being_narrated_is_washed(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let read_aloud = narration_reader(&provider, &sink, cx);
        let label = markdown_entity("Read player.rs", cx);

        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.narrate_tool_call(titled_call(label.clone(), NarrationKind::Read), cx);
        });
        cx.run_until_parked();
        assert_eq!(
            label.read_with(cx, |markdown, _| markdown.speaking_highlight().cloned()),
            Some(0..14),
            "a tool label is on screen, so it washes in place"
        );
    }

    #[gpui::test]
    async fn a_narration_burst_collapses_behind_the_one_being_spoken(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let read_aloud = narration_reader(&provider, &sink, cx);
        let labels: Vec<_> = (0..5)
            .map(|index| markdown_entity(&format!("Read file_{index}.rs"), cx))
            .collect();

        for label in &labels {
            read_aloud.update(cx, |read_aloud, cx| {
                read_aloud.narrate_tool_call(titled_call(label.clone(), NarrationKind::Read), cx);
            });
            cx.run_until_parked();
        }
        assert_eq!(
            provider.spoken(),
            vec!["Reading file 0.".to_string()],
            "only the first is spoken; the rest are a backlog"
        );

        // The first label finishes, so the backlog gets its turn.
        sink.finish_one();
        cx.executor().advance_clock(POSITION_POLL_INTERVAL);
        cx.run_until_parked();
        assert_eq!(
            provider.spoken(),
            vec![
                "Reading file 0.".to_string(),
                "Reading four files.".to_string()
            ],
            "four queued reads are counted, not recited"
        );
    }

    #[gpui::test]
    async fn narration_is_dropped_while_the_user_has_stopped(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let read_aloud = narration_reader(&provider, &sink, cx);
        let first = markdown_entity("Read player.rs", cx);
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.narrate_tool_call(titled_call(first.clone(), NarrationKind::Read), cx);
        });
        cx.run_until_parked();

        read_aloud.update(cx, |read_aloud, cx| read_aloud.stop(cx));
        cx.run_until_parked();
        let spoken_when_stopped = provider.spoken();

        let second = markdown_entity("Edit segmenter.rs", cx);
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.narrate_tool_call(titled_call(second.clone(), NarrationKind::Edit), cx);
        });
        cx.run_until_parked();
        assert_eq!(
            provider.spoken(),
            spoken_when_stopped,
            "a stop latches against status too, and stale status is worse than none"
        );
    }

    #[gpui::test]
    async fn switching_modes_drops_what_is_sounding_without_latching_silence(
        cx: &mut TestAppContext,
    ) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let read_aloud = cx.new({
            let provider = provider.clone();
            |cx| ReadAloud::for_test(Arc::new(provider), Box::new(sink), cx)
        });
        let message = markdown_entity("First one. Second one.\n", cx);
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.enqueue_markdown(&message, false, cx);
        });
        cx.run_until_parked();
        assert!(read_aloud.read_with(cx, |read_aloud, _| read_aloud.is_speaking()));

        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.set_mode(ReadAloudMode::Narration, cx);
        });
        cx.run_until_parked();
        assert!(
            !read_aloud.read_with(cx, |read_aloud, _| read_aloud.is_speaking()),
            "the switch drops what was sounding rather than re-reading it"
        );
        assert_eq!(
            message.read_with(cx, |markdown, _| markdown.speaking_highlight().cloned()),
            None
        );
        assert_eq!(
            read_aloud.read_with(cx, |read_aloud, cx| read_aloud.playback_state(cx)),
            None
        );

        // The new mode takes over from the next utterance, so this must not
        // have latched anything the way a user stop does.
        let label = markdown_entity("Read player.rs", cx);
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.narrate_tool_call(titled_call(label.clone(), NarrationKind::Read), cx);
        });
        cx.run_until_parked();
        assert!(
            provider
                .spoken()
                .ends_with(&["Reading player.".to_string()]),
            "the mode just switched into must not start out mute, got {:?}",
            provider.spoken()
        );
    }

    #[gpui::test]
    async fn a_short_message_is_spoken_rather_than_summarized(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let read_aloud = narration_reader(&provider, &sink, cx);
        let model = FakeSummaryModel::new("Never asked for.");
        read_aloud.update(cx, |read_aloud, _| {
            read_aloud.set_summary_model(Some(Rc::new(model.clone())));
        });

        let message = markdown_entity("Done, tests pass.\n", cx);
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.narrate_message(vec![message], cx);
        });
        cx.run_until_parked();

        assert!(
            model.prompts().is_empty(),
            "condensing a one-liner costs more than saying it"
        );
        assert_eq!(provider.spoken(), vec!["Done, tests pass.".to_string()]);
    }

    /// Long enough to be worth a summary, with a first sentence the
    /// fallback can be recognized by.
    fn long_message_source() -> String {
        format!(
            "I moved the poll loop onto a timer. {}\n",
            "It also keeps the stop latch exactly as it was. ".repeat(6)
        )
    }

    #[gpui::test]
    async fn a_long_message_is_summarized_and_the_message_is_washed(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let read_aloud = narration_reader(&provider, &sink, cx);
        let model = FakeSummaryModel::new("It put the poll loop on a timer.");
        read_aloud.update(cx, |read_aloud, _| {
            read_aloud.set_summary_model(Some(Rc::new(model.clone())));
        });

        let message = markdown_entity(&long_message_source(), cx);
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.narrate_message(vec![message.clone()], cx);
        });
        cx.run_until_parked();

        assert_eq!(model.prompts().len(), 1, "exactly one call per message");
        assert!(
            model.prompts()[0].contains("I moved the poll loop onto a timer."),
            "the message goes to the model"
        );
        assert_eq!(
            provider.spoken(),
            vec!["It put the poll loop on a timer.".to_string()],
            "the summary is spoken instead of the prose"
        );
        let source_length = message.read_with(cx, |markdown, _| markdown.source().len());
        assert_eq!(
            message.read_with(cx, |markdown, _| markdown.speaking_highlight().cloned()),
            Some(0..source_length),
            "the whole message washes so the listener can find it on screen"
        );
        assert_eq!(
            message.read_with(cx, |markdown, _| markdown
                .speaking_word_highlight()
                .cloned()),
            None,
            "a summary's words map to nothing on screen, so there is no pill"
        );
    }

    #[gpui::test]
    async fn one_message_is_never_summarized_twice(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let read_aloud = narration_reader(&provider, &sink, cx);
        let model = FakeSummaryModel::new("It put the poll loop on a timer.");
        read_aloud.update(cx, |read_aloud, _| {
            read_aloud.set_summary_model(Some(Rc::new(model.clone())));
        });

        let message = markdown_entity(&long_message_source(), cx);
        for _ in 0..3 {
            read_aloud.update(cx, |read_aloud, cx| {
                read_aloud.narrate_message(vec![message.clone()], cx);
            });
            cx.run_until_parked();
        }
        assert_eq!(
            model.prompts().len(),
            1,
            "repeat completion signals must not repeat the cost"
        );
    }

    /// An authenticated model that never answers is indistinguishable from
    /// no model at all: narration goes on speaking templated lines, and
    /// before this the only trace was a debug log. Said once, and only after
    /// enough failures that a single slow answer cannot trigger it.
    #[gpui::test]
    async fn a_model_that_keeps_failing_is_reported_once(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let read_aloud = narration_reader(&provider, &sink, cx);
        read_aloud.update(cx, |read_aloud, _| {
            read_aloud.set_summary_model(Some(Rc::new(FakeSummaryModel::failing())));
        });
        let reports = Rc::new(std::cell::Cell::new(0usize));
        let _subscription = cx.update(|cx| {
            cx.subscribe(&read_aloud, {
                let reports = reports.clone();
                move |_, _: &ReadAloudEvent, _| reports.set(reports.get() + 1)
            })
        });

        let narrate = |index: usize, cx: &mut TestAppContext| {
            let message = markdown_entity(
                &long_message_source().replace("poll loop", &format!("part {index}")),
                cx,
            );
            read_aloud.update(cx, |read_aloud, cx| {
                read_aloud.narrate_message(vec![message], cx);
            });
            cx.run_until_parked();
        };

        for index in 0..MODEL_FAILURES_BEFORE_REPORTING - 1 {
            narrate(index, cx);
            assert_eq!(
                reports.get(),
                0,
                "one or two failures is a blip, not a broken model"
            );
        }
        narrate(MODEL_FAILURES_BEFORE_REPORTING, cx);
        assert_eq!(reports.get(), 1, "a run of failures is said out loud");
        for index in 0..3 {
            narrate(MODEL_FAILURES_BEFORE_REPORTING + 1 + index, cx);
        }
        assert_eq!(
            reports.get(),
            1,
            "and said once: a report per message is worse than the silence \
             it is fixing"
        );
    }

    /// The counter is about a run, not a total: a model that works again has
    /// stopped being broken.
    #[gpui::test]
    async fn a_working_answer_clears_the_failure_run(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let read_aloud = narration_reader(&provider, &sink, cx);
        // Fails, fails, answers, fails, fails — never three in a row.
        let model = FakeSummaryModel::sequence(&["", "", "It moved the poll loop.", "", ""]);
        read_aloud.update(cx, |read_aloud, _| {
            read_aloud.set_summary_model(Some(Rc::new(model)));
        });
        let reports = Rc::new(std::cell::Cell::new(0usize));
        let _subscription = cx.update(|cx| {
            cx.subscribe(&read_aloud, {
                let reports = reports.clone();
                move |_, _: &ReadAloudEvent, _| reports.set(reports.get() + 1)
            })
        });

        for index in 0..5 {
            let message = markdown_entity(
                &long_message_source().replace("poll loop", &format!("part {index}")),
                cx,
            );
            read_aloud.update(cx, |read_aloud, cx| {
                read_aloud.narrate_message(vec![message], cx);
            });
            cx.run_until_parked();
        }
        assert_eq!(
            reports.get(),
            0,
            "a model that answered in between is not a model that is failing"
        );
    }

    /// Regression: `summary_task` used to be a single slot, so a second
    /// message completing while the first was still being summarized
    /// cancelled the first — and the first was already marked summarized, so
    /// it could never be retried. A model round trip is one to three
    /// seconds; an agent writing a paragraph and then firing a tool inside
    /// that window is the ordinary case, not an edge one.
    #[gpui::test]
    async fn a_second_message_does_not_cancel_the_first_ones_summary(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let read_aloud = narration_reader(&provider, &sink, cx);
        let model = FakeSummaryModel::sequence(&[
            "It put the poll loop on a timer.",
            "It left the stop latch alone.",
        ]);
        model.hold();
        read_aloud.update(cx, |read_aloud, _| {
            read_aloud.set_summary_model(Some(Rc::new(model.clone())));
        });

        let first = markdown_entity(&long_message_source(), cx);
        let second = markdown_entity(
            &long_message_source().replace("poll loop", "sentence segmenter"),
            cx,
        );
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.narrate_message(vec![first], cx);
        });
        cx.run_until_parked();
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.narrate_message(vec![second], cx);
        });
        cx.run_until_parked();
        assert_eq!(
            model.prompts().len(),
            2,
            "setup: both messages have a call in flight"
        );

        model.release_all();
        cx.run_until_parked();
        for _ in 0..5 {
            sink.finish_one();
            cx.executor().advance_clock(POSITION_POLL_INTERVAL);
            cx.run_until_parked();
        }

        let spoken = provider.spoken();
        assert!(
            spoken.contains(&"It put the poll loop on a timer.".to_string()),
            "the first message's summary must not be lost to the second, got {spoken:?}"
        );
        assert!(
            spoken.contains(&"It left the stop latch alone.".to_string()),
            "and the second must arrive too, got {spoken:?}"
        );
    }

    #[gpui::test]
    async fn a_failing_summary_model_falls_back_to_the_messages_opening(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let read_aloud = narration_reader(&provider, &sink, cx);
        read_aloud.update(cx, |read_aloud, _| {
            read_aloud.set_summary_model(Some(Rc::new(FakeSummaryModel::failing())));
        });

        let message = markdown_entity(&long_message_source(), cx);
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.narrate_message(vec![message], cx);
        });
        cx.run_until_parked();

        let spoken = provider.spoken();
        assert_eq!(
            spoken,
            vec![
                "I moved the poll loop onto a timer.".to_string(),
                "It also keeps the stop latch exactly as it was.".to_string()
            ],
            "a failed summary degrades to the message's opening — two sentences, \
             not the six the message actually has"
        );
    }

    #[gpui::test]
    async fn with_no_summary_model_narration_still_says_something(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let read_aloud = narration_reader(&provider, &sink, cx);

        let message = markdown_entity(&long_message_source(), cx);
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.narrate_message(vec![message], cx);
        });
        cx.run_until_parked();
        assert!(
            provider
                .spoken()
                .first()
                .is_some_and(|spoken| spoken.starts_with("I moved the poll loop onto a timer.")),
            "no model must not mean silence, got {:?}",
            provider.spoken()
        );
    }

    #[gpui::test]
    async fn a_stop_cancels_a_summary_still_being_generated(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let read_aloud = narration_reader(&provider, &sink, cx);
        let model = FakeSummaryModel::new("It put the poll loop on a timer.");
        model.hold();
        read_aloud.update(cx, |read_aloud, _| {
            read_aloud.set_summary_model(Some(Rc::new(model.clone())));
        });

        let message = markdown_entity(&long_message_source(), cx);
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.narrate_message(vec![message], cx);
        });
        cx.run_until_parked();
        assert_eq!(model.prompts().len(), 1, "setup: the call is in flight");

        read_aloud.update(cx, |read_aloud, cx| read_aloud.stop(cx));
        model.release_all();
        cx.run_until_parked();
        assert!(
            provider.spoken().is_empty(),
            "a summary must never arrive after the user stopped, got {:?}",
            provider.spoken()
        );
    }

    #[gpui::test]
    async fn switching_back_to_full_cancels_a_summary_still_being_generated(
        cx: &mut TestAppContext,
    ) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let read_aloud = narration_reader(&provider, &sink, cx);
        let model = FakeSummaryModel::new("It put the poll loop on a timer.");
        model.hold();
        read_aloud.update(cx, |read_aloud, _| {
            read_aloud.set_summary_model(Some(Rc::new(model.clone())));
        });

        let message = markdown_entity(&long_message_source(), cx);
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.narrate_message(vec![message], cx);
        });
        cx.run_until_parked();

        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.set_mode(ReadAloudMode::Full, cx);
        });
        model.release_all();
        cx.run_until_parked();
        assert!(
            provider.spoken().is_empty(),
            "full mode must not be interrupted by narration's leftovers, got {:?}",
            provider.spoken()
        );
    }

    #[gpui::test]
    async fn a_new_turn_cancels_a_summary_still_being_generated(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let read_aloud = narration_reader(&provider, &sink, cx);
        let model = FakeSummaryModel::new("It put the poll loop on a timer.");
        model.hold();
        read_aloud.update(cx, |read_aloud, _| {
            read_aloud.set_summary_model(Some(Rc::new(model.clone())));
        });

        let message = markdown_entity(&long_message_source(), cx);
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.narrate_message(vec![message], cx);
        });
        cx.run_until_parked();

        read_aloud.update(cx, |read_aloud, cx| read_aloud.cancel_narration(cx));
        read_aloud.read_with(cx, |read_aloud, _| {
            assert!(
                read_aloud.narration_progress_is_idle(),
                "a new turn drops the previous turn's wrap-up outright"
            );
        });
        model.release_all();
        cx.run_until_parked();
        assert!(
            provider.spoken().is_empty(),
            "status about the turn the user left behind must not arrive late, got {:?}",
            provider.spoken()
        );
    }

    #[gpui::test]
    async fn a_summary_that_times_out_falls_back(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let read_aloud = narration_reader(&provider, &sink, cx);
        let model = FakeSummaryModel::new("It put the poll loop on a timer.");
        model.hold();
        read_aloud.update(cx, |read_aloud, _| {
            read_aloud.set_summary_model(Some(Rc::new(model.clone())));
        });

        let message = markdown_entity(&long_message_source(), cx);
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.narrate_message(vec![message], cx);
        });
        cx.run_until_parked();

        cx.executor().advance_clock(SUMMARY_TIMEOUT * 2);
        cx.run_until_parked();
        assert!(
            provider
                .spoken()
                .first()
                .is_some_and(|spoken| spoken.starts_with("I moved the poll loop onto a timer.")),
            "a model that never answers must not leave narration silent, got {:?}",
            provider.spoken()
        );
    }

    #[gpui::test]
    async fn clicking_a_sentence_in_narration_mode_reads_that_message_in_full(
        cx: &mut TestAppContext,
    ) {
        let provider = FakeTts::new();
        provider.emit_word_timings();
        let sink = FakeSink::new();
        let read_aloud = narration_reader(&provider, &sink, cx);
        let model = FakeSummaryModel::new("It put the poll loop on a timer.");
        read_aloud.update(cx, |read_aloud, _| {
            read_aloud.set_summary_model(Some(Rc::new(model)));
        });

        let source = "First one. Second one. Third one.\n";
        let message = markdown_entity(source, cx);
        // A summary is speaking, washing the whole message.
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.narrate_message(vec![message.clone()], cx);
        });
        cx.run_until_parked();

        let click = source.find("Second one.").expect("the test source has it");
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.seek_to_source_index(&message, click, true, cx);
        });
        cx.run_until_parked();
        cx.executor().advance_clock(POSITION_POLL_INTERVAL);
        cx.run_until_parked();

        assert!(
            provider
                .spoken()
                .ends_with(&["Second one.".to_string(), "Third one.".to_string()]),
            "the click reads the message itself, from the clicked sentence on, got {:?}",
            provider.spoken()
        );
        assert_eq!(
            message.read_with(cx, |markdown, _| markdown.speaking_highlight().cloned()),
            Some(11..22),
            "the whole-message wash gives way to ordinary sentence highlighting"
        );
        assert!(
            message
                .read_with(cx, |markdown, _| markdown
                    .speaking_word_highlight()
                    .cloned())
                .is_some(),
            "and the word pill is back"
        );
        assert_eq!(
            read_aloud.read_with(cx, |read_aloud, _| read_aloud.mode()),
            ReadAloudMode::Narration,
            "a drill-down is not a mode change"
        );
    }

    /// Regression: the drill-down is the loop this whole branch is built on,
    /// and in narration mode nothing advertised it. Speakable ranges were
    /// mirrored onto the spoken entity — a summary phrase that exists
    /// nowhere on screen — so the message the user can actually click got no
    /// hover band and no pointer cursor.
    #[gpui::test]
    async fn the_message_a_summary_is_about_advertises_its_sentences(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let read_aloud = narration_reader(&provider, &sink, cx);
        let model = FakeSummaryModel::new("It put the poll loop on a timer.");
        read_aloud.update(cx, |read_aloud, _| {
            read_aloud.set_summary_model(Some(Rc::new(model)));
        });

        let message = markdown_entity(&long_message_source(), cx);
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.narrate_message(vec![message.clone()], cx);
        });
        cx.run_until_parked();

        assert!(
            !message.read_with(cx, |markdown, _| markdown.speakable_ranges().is_empty()),
            "the washed message must advertise where the drill-down can land"
        );

        // The affordance belongs to whatever narration is speaking about
        // right now, exactly as it belongs to the speaking message in full
        // mode.
        let label = markdown_entity("Read player.rs", cx);
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.narrate_tool_call(titled_call(label.clone(), NarrationKind::Read), cx);
        });
        sink.finish_one();
        cx.executor().advance_clock(POSITION_POLL_INTERVAL);
        cx.run_until_parked();
        assert!(
            message.read_with(cx, |markdown, _| markdown.speakable_ranges().is_empty()),
            "and gives it up when narration moves on"
        );
    }

    #[gpui::test]
    async fn narration_resumes_after_a_drill_down(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let read_aloud = narration_reader(&provider, &sink, cx);
        let source = "First one. Second one.\n";
        let message = markdown_entity(source, cx);

        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.seek_to_source_index(&message, 0, true, cx);
        });
        cx.run_until_parked();
        sink.finish_one();
        sink.finish_one();
        cx.executor().advance_clock(POSITION_POLL_INTERVAL);
        cx.run_until_parked();

        let label = markdown_entity("Read player.rs", cx);
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.narrate_tool_call(titled_call(label.clone(), NarrationKind::Read), cx);
        });
        cx.run_until_parked();
        assert!(
            provider
                .spoken()
                .ends_with(&["Reading player.".to_string()]),
            "the next thing the agent does narrates again, got {:?}",
            provider.spoken()
        );
    }

    // ---- Step narration (the default `steps` detail) ----

    fn read_tool_label(path: &str, cx: &mut TestAppContext) -> Entity<Markdown> {
        markdown_entity(&format!("Read file `{path}`"), cx)
    }

    /// Drains the sink far enough that everything queued gets its turn.
    fn drain_narration(sink: &FakeSink, cx: &mut TestAppContext) {
        for _ in 0..12 {
            sink.finish_one();
            cx.executor().advance_clock(POSITION_POLL_INTERVAL);
            cx.run_until_parked();
        }
    }

    /// The heart of the feature. The agent's own words go out the moment
    /// the step is real — no model, no waiting — and the fused line that
    /// names what it is actually doing follows once the step closes.
    #[gpui::test]
    async fn a_step_joins_the_prose_to_the_tool_calls_it_explains(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let read_aloud = steps_narration_reader(&provider, &sink, cx);
        let model = FakeSummaryModel::new("Checking the poll loop to see how it is timed.");
        read_aloud.update(cx, |read_aloud, _| {
            read_aloud.set_summary_model(Some(Rc::new(model.clone())));
        });

        let message = markdown_entity(&long_message_source(), cx);
        let label = read_tool_label("crates/read_aloud/src/player.rs", cx);
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.narrate_message(vec![message], cx);
            read_aloud.narrate_tool_call(titled_call(label.clone(), NarrationKind::Read), cx);
        });
        cx.run_until_parked();
        assert_eq!(
            provider.spoken(),
            vec!["I moved the poll loop onto a timer.".to_string()],
            "the agent's own words are already written, so they are spoken \
             at once rather than held for the fused line"
        );
        assert_eq!(
            model.prompts().len(),
            1,
            "and the fused line is already being generated, before the quiet \
             window rather than after it"
        );
        let prompt = &model.prompts()[0];
        assert!(
            prompt.contains("I moved the poll loop onto a timer."),
            "the prose the agent wrote is the *why*"
        );
        assert!(
            prompt.contains("crates/read_aloud/src/player.rs"),
            "and the tool call it then made is the *what*"
        );

        let_the_step_close(cx);
        drain_narration(&sink, cx);
        assert_eq!(
            provider.spoken(),
            vec![
                "I moved the poll loop onto a timer.".to_string(),
                "Checking the poll loop to see how it is timed.".to_string(),
            ],
            "closing the step costs no round trip: the line was ready"
        );
    }

    #[gpui::test]
    async fn prose_after_tool_calls_starts_a_new_step(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let read_aloud = steps_narration_reader(&provider, &sink, cx);
        let model = FakeSummaryModel::sequence(&["Reading the player.", "Now editing it."]);
        read_aloud.update(cx, |read_aloud, _| {
            read_aloud.set_summary_model(Some(Rc::new(model.clone())));
        });

        let first = markdown_entity(&long_message_source(), cx);
        let second = markdown_entity(&long_message_source().replace("poll loop", "segmenter"), cx);
        let label = read_tool_label("crates/read_aloud/src/player.rs", cx);
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.narrate_message(vec![first], cx);
            read_aloud.narrate_tool_call(titled_call(label.clone(), NarrationKind::Read), cx);
        });
        cx.run_until_parked();

        // No clock advance: the second message alone must close the step.
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.narrate_message(vec![second], cx);
        });
        cx.run_until_parked();
        assert_eq!(
            model.prompts().len(),
            1,
            "new prose after tool calls closes the step it followed"
        );
        assert!(
            model.prompts()[0].contains("I moved the poll loop onto a timer."),
            "and the step that closed is the first one, got {:?}",
            model.prompts()
        );
    }

    #[gpui::test]
    async fn a_step_closes_when_the_turn_ends(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let read_aloud = steps_narration_reader(&provider, &sink, cx);
        let message = markdown_entity(&long_message_source(), cx);
        let label = read_tool_label("crates/read_aloud/src/player.rs", cx);

        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.narrate_message(vec![message.clone()], cx);
            read_aloud.narrate_tool_call(titled_call(label.clone(), NarrationKind::Read), cx);
        });
        cx.run_until_parked();
        assert_eq!(
            provider.spoken(),
            vec!["I moved the poll loop onto a timer.".to_string()],
            "the opening goes out at once even with no model at all"
        );

        // No model at all, so this also pins the whole fallback chain: the
        // opening already spoken, then the templated tool line — and the
        // opening is not said a second time.
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.finish_turn(vec![message], cx);
        });
        cx.run_until_parked();
        drain_narration(&sink, cx);
        assert_eq!(
            provider.spoken(),
            vec![
                "I moved the poll loop onto a timer.".to_string(),
                "Reading player.".to_string(),
            ],
            "with no model the turn still says what happened, reason first"
        );
    }

    /// The quiet window must not be on the critical path. A step's line is
    /// generated the moment the step is real, so closing the step costs no
    /// round trip — and a step that never goes quiet still has one ready.
    #[gpui::test]
    async fn the_step_line_is_generated_before_the_quiet_window(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let read_aloud = steps_narration_reader(&provider, &sink, cx);
        let model = FakeSummaryModel::new("Reading the player to see how it is timed.");
        read_aloud.update(cx, |read_aloud, _| {
            read_aloud.set_summary_model(Some(Rc::new(model.clone())));
        });

        let message = markdown_entity(&long_message_source(), cx);
        let label = read_tool_label("crates/read_aloud/src/player.rs", cx);
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.narrate_message(vec![message], cx);
            read_aloud.narrate_tool_call(titled_call(label.clone(), NarrationKind::Read), cx);
        });
        cx.run_until_parked();

        // Not one tick of the quiet window has passed.
        assert_eq!(
            model.prompts().len(),
            1,
            "the generation starts with the step, not with its close"
        );
        read_aloud.read_with(cx, |read_aloud, _| {
            assert!(
                read_aloud
                    .step
                    .as_ref()
                    .is_some_and(|step| step.line.ready.is_some()),
                "and its answer is waiting for the step to close"
            );
        });
    }

    /// A burst of calls arriving milliseconds apart must not spend both of a
    /// step's generations before either has answered.
    #[gpui::test]
    async fn a_burst_does_not_burn_both_step_generations(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let read_aloud = steps_narration_reader(&provider, &sink, cx);
        let model = FakeSummaryModel::new("Reading through the crate.");
        model.hold();
        read_aloud.update(cx, |read_aloud, _| {
            read_aloud.set_summary_model(Some(Rc::new(model.clone())));
        });

        let message = markdown_entity(&long_message_source(), cx);
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.narrate_message(vec![message], cx);
        });
        for path in [
            "crates/read_aloud/src/player.rs",
            "crates/read_aloud/src/segmenter.rs",
            "crates/read_aloud/src/sink.rs",
            "crates/read_aloud/src/provider.rs",
        ] {
            let label = read_tool_label(path, cx);
            read_aloud.update(cx, |read_aloud, cx| {
                read_aloud.narrate_tool_call(titled_call(label.clone(), NarrationKind::Read), cx);
            });
            cx.run_until_parked();
        }
        assert_eq!(
            model.prompts().len(),
            1,
            "a re-issue waits for the first answer, or a burst spends the \
             whole budget describing its first call"
        );

        model.release_all();
        cx.run_until_parked();
        assert_eq!(
            model.prompts().len(),
            MAX_STEP_LINE_ISSUES,
            "and once it answers, the calls that joined meanwhile are worth \
             one re-issue"
        );
        assert!(
            model.prompts()[1].contains("provider.rs"),
            "which covers the whole step, got {:?}",
            model.prompts()[1]
        );
    }

    /// The agent's own words have already been spoken by the time the fused
    /// line arrives, so a line that only says them again is dropped — two
    /// spoken sentences carrying the same information is the choppiness this
    /// task is fixing, in its other form.
    #[gpui::test]
    async fn a_step_line_that_only_restates_the_prose_is_dropped(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let read_aloud = steps_narration_reader(&provider, &sink, cx);
        // Near-verbatim: the model has re-said what the listener just heard.
        let model = FakeSummaryModel::new("It moved the poll loop onto a timer.");
        read_aloud.update(cx, |read_aloud, _| {
            read_aloud.set_summary_model(Some(Rc::new(model.clone())));
        });

        let message = markdown_entity(&long_message_source(), cx);
        let label = read_tool_label("crates/read_aloud/src/player.rs", cx);
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.narrate_message(vec![message], cx);
            read_aloud.narrate_tool_call(titled_call(label.clone(), NarrationKind::Read), cx);
        });
        let_the_step_close(cx);
        drain_narration(&sink, cx);

        assert_eq!(
            provider.spoken(),
            vec![
                "I moved the poll loop onto a timer.".to_string(),
                "Reading player.".to_string(),
            ],
            "the restatement gives way to the tool call's template, which at \
             least names what is actually happening"
        );
    }

    #[gpui::test]
    async fn a_model_that_says_it_has_nothing_to_add_is_believed(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let read_aloud = steps_narration_reader(&provider, &sink, cx);
        let model = FakeSummaryModel::new(NOTHING_TO_ADD);
        read_aloud.update(cx, |read_aloud, _| {
            read_aloud.set_summary_model(Some(Rc::new(model.clone())));
        });

        let message = markdown_entity(&long_message_source(), cx);
        let label = read_tool_label("crates/read_aloud/src/player.rs", cx);
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.narrate_message(vec![message], cx);
            read_aloud.narrate_tool_call(titled_call(label.clone(), NarrationKind::Read), cx);
        });
        let_the_step_close(cx);
        drain_narration(&sink, cx);

        assert!(
            !provider
                .spoken()
                .iter()
                .any(|line| line.contains(NOTHING_TO_ADD)),
            "the sentinel is an instruction, not something to say, got {:?}",
            provider.spoken()
        );
        assert!(
            provider.spoken().contains(&"Reading player.".to_string()),
            "and what it is doing is still named, got {:?}",
            provider.spoken()
        );
    }

    #[gpui::test]
    async fn a_step_with_no_prose_spends_no_model_call(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let read_aloud = steps_narration_reader(&provider, &sink, cx);
        let model = FakeSummaryModel::new("Never asked for.");
        read_aloud.update(cx, |read_aloud, _| {
            read_aloud.set_summary_model(Some(Rc::new(model.clone())));
        });

        let label = read_tool_label("crates/read_aloud/src/player.rs", cx);
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.narrate_tool_call(titled_call(label.clone(), NarrationKind::Read), cx);
        });
        cx.run_until_parked();

        assert!(
            model.prompts().is_empty(),
            "a bare tool call has no *why* for a model to add"
        );
        assert_eq!(
            provider.spoken(),
            vec!["Reading player.".to_string()],
            "and it is said now rather than after a round trip"
        );
    }

    /// The latency rule: a step's line is status, so a slow model loses to
    /// the template, and its answer is never spoken on top.
    #[gpui::test]
    async fn a_slow_step_speaks_the_template_and_never_both(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let read_aloud = steps_narration_reader(&provider, &sink, cx);
        let model = FakeSummaryModel::new("Far too late to be status.");
        model.hold();
        read_aloud.update(cx, |read_aloud, _| {
            read_aloud.set_summary_model(Some(Rc::new(model.clone())));
        });

        let message = markdown_entity(&long_message_source(), cx);
        let label = read_tool_label("crates/read_aloud/src/player.rs", cx);
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.narrate_message(vec![message], cx);
            read_aloud.narrate_tool_call(titled_call(label.clone(), NarrationKind::Read), cx);
        });
        let_the_step_close(cx);
        assert_eq!(model.prompts().len(), 1, "the step asked");
        assert_eq!(
            provider.spoken(),
            vec!["I moved the poll loop onto a timer.".to_string()],
            "the opening is out; only the fused line is waiting on the answer"
        );

        let_the_step_time_out(cx);
        drain_narration(&sink, cx);
        let after_fallback = provider.spoken();
        assert!(
            after_fallback.contains(&"Reading player.".to_string()),
            "past the budget the template speaks, got {after_fallback:?}"
        );
        assert_eq!(
            after_fallback
                .iter()
                .filter(|line| line.contains("poll loop"))
                .count(),
            1,
            "and the opening is not repeated by the fallback, got {after_fallback:?}"
        );

        model.release_all();
        cx.run_until_parked();
        drain_narration(&sink, cx);
        assert_eq!(
            provider.spoken(),
            after_fallback,
            "and the late answer is dropped rather than spoken on top"
        );
    }

    /// A step that closes while its line is still in flight waits for it
    /// rather than falling straight to the template. The generation was
    /// started early enough that it is usually about to answer, and the
    /// answer is the whole point of a step.
    #[gpui::test]
    async fn a_step_that_closes_mid_generation_still_gets_its_line(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let read_aloud = steps_narration_reader(&provider, &sink, cx);
        let model = FakeSummaryModel::new("Checking the player to see how it is timed.");
        model.hold();
        read_aloud.update(cx, |read_aloud, _| {
            read_aloud.set_summary_model(Some(Rc::new(model.clone())));
        });

        let message = markdown_entity(&long_message_source(), cx);
        let label = read_tool_label("crates/read_aloud/src/player.rs", cx);
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.narrate_message(vec![message], cx);
            read_aloud.narrate_tool_call(titled_call(label.clone(), NarrationKind::Read), cx);
        });
        // The quiet window closes the step while the answer is still coming.
        let_the_step_close(cx);
        read_aloud.read_with(cx, |read_aloud, _| {
            assert!(
                read_aloud.step.is_none() && read_aloud.closing_steps.len() == 1,
                "the step has closed and is waiting on its line"
            );
        });

        // Still inside the two-second budget.
        model.release_all();
        cx.run_until_parked();
        drain_narration(&sink, cx);
        assert_eq!(
            provider.spoken(),
            vec![
                "I moved the poll loop onto a timer.".to_string(),
                "Checking the player to see how it is timed.".to_string(),
            ],
            "the line it waited for is what gets said, not the template"
        );
    }

    #[gpui::test]
    async fn the_step_prompt_carries_what_narration_already_said(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let read_aloud = steps_narration_reader(&provider, &sink, cx);
        let model = FakeSummaryModel::new("Now editing the segmenter to match.");
        read_aloud.update(cx, |read_aloud, _| {
            read_aloud.set_summary_model(Some(Rc::new(model.clone())));
        });

        // A bare tool call speaks straight away, so it is on the record.
        let first = read_tool_label("crates/read_aloud/src/player.rs", cx);
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.narrate_tool_call(titled_call(first.clone(), NarrationKind::Read), cx);
        });
        cx.run_until_parked();

        let message = markdown_entity(&long_message_source(), cx);
        let second = read_tool_label("crates/read_aloud/src/segmenter.rs", cx);
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.narrate_message(vec![message], cx);
            read_aloud.narrate_tool_call(titled_call(second.clone(), NarrationKind::Read), cx);
        });
        let_the_step_close(cx);

        let prompt = model.prompts().pop().expect("the step asked");
        assert!(
            prompt.contains("already said"),
            "the narrator has to know what it has already said"
        );
        assert!(
            prompt.contains("Reading `crates/read_aloud/src/player.rs`."),
            "and the line itself has to be in there, got {prompt}"
        );
    }

    // ---- The turn wrap-up ----

    /// Sets up a turn that has done some work and is now writing its closing
    /// message, which is the state a wrap-up is speculated from.
    fn turn_in_progress(
        read_aloud: &Entity<ReadAloud>,
        cx: &mut TestAppContext,
    ) -> Entity<Markdown> {
        let label = read_tool_label("crates/read_aloud/src/player.rs", cx);
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.narrate_tool_call(titled_call(label.clone(), NarrationKind::Read), cx);
        });
        cx.run_until_parked();
        markdown_entity(&long_message_source(), cx)
    }

    /// The user's actual complaint about wrap-ups: "better metadata …
    /// onto what we're receiving back". The wrap-up saw the closing
    /// paragraph and a list of tool labels, and nothing guaranteed that a
    /// failed command was mentioned at all.
    #[gpui::test]
    async fn the_wrap_up_sees_the_commands_the_files_and_the_failure(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let read_aloud = steps_narration_reader(&provider, &sink, cx);
        let model = FakeSummaryModel::new("The tests fail on the segmenter.");
        read_aloud.update(cx, |read_aloud, _| {
            read_aloud.set_summary_model(Some(Rc::new(model.clone())));
        });

        // An external agent's shape throughout: generic titles, structured
        // input.
        let edit = markdown_entity("Edit file", cx);
        let terminal = markdown_entity("Terminal", cx);
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.narrate_tool_call(
                ToolCallFacts {
                    path: Some("crates/read_aloud/src/segmenter.rs".to_string()),
                    ..ToolCallFacts::from_label("edit1", edit, NarrationKind::Edit)
                },
                cx,
            );
            read_aloud.narrate_tool_call(
                ToolCallFacts {
                    command: Some("cargo test -p read_aloud".to_string()),
                    ..ToolCallFacts::from_label("run1", terminal, NarrationKind::Execute)
                },
                cx,
            );
            // The command's result lands after it was narrated, which is the
            // ordinary case.
            read_aloud.note_tool_call_outcome("run1", ToolCallOutcome::Failed, cx);
        });
        cx.run_until_parked();

        let closing = markdown_entity(&long_message_source(), cx);
        let chars = closing.read_with(cx, |markdown, _| markdown.source().len());
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.speculate_wrap_up(vec![closing], chars, cx);
        });
        cx.run_until_parked();

        let prompt = model
            .prompts()
            .first()
            .cloned()
            .expect("the wrap-up was generated");
        assert!(
            prompt.contains("cargo test -p read_aloud"),
            "the real command, not the generic title the agent sent"
        );
        assert!(
            prompt.contains("FAILED"),
            "a failed command is the single most important thing to carry through"
        );
        assert!(
            prompt.contains("Files it changed"),
            "and what the turn actually touched"
        );
        assert!(!prompt.contains("Terminal"), "got {prompt}");
    }

    /// A long turn is exactly the turn the listener was not watching, so it
    /// is the worst possible one to lose a failure from. The turn's account
    /// is capped on the way *out* to the prompt, not on the way in, and
    /// failures are never what the cap drops.
    #[gpui::test]
    async fn a_failure_late_in_a_long_turn_still_reaches_the_wrap_up(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let read_aloud = steps_narration_reader(&provider, &sink, cx);
        let model = FakeSummaryModel::new("Done.");
        read_aloud.update(cx, |read_aloud, _| {
            read_aloud.set_summary_model(Some(Rc::new(model.clone())));
        });

        // Well past MAX_TURN_ACTIVITY, with the failure late enough that the
        // old arrival-order cap had already stopped recording.
        let failing_call = MAX_TURN_ACTIVITY + 7;
        for index in 0..MAX_TURN_ACTIVITY * 2 {
            let label = markdown_entity("Terminal", cx);
            read_aloud.update(cx, |read_aloud, cx| {
                read_aloud.narrate_tool_call(
                    ToolCallFacts {
                        command: Some(format!("cargo build --bin tool_{index}")),
                        ..ToolCallFacts::from_label(
                            format!("call{index}"),
                            label,
                            NarrationKind::Execute,
                        )
                    },
                    cx,
                );
            });
        }
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.note_tool_call_outcome(
                &format!("call{failing_call}"),
                ToolCallOutcome::Failed,
                cx,
            );
        });
        cx.run_until_parked();

        let closing = markdown_entity(&long_message_source(), cx);
        let chars = closing.read_with(cx, |markdown, _| markdown.source().len());
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.speculate_wrap_up(vec![closing], chars, cx);
        });
        cx.run_until_parked();

        let prompt = model
            .prompts()
            .first()
            .cloned()
            .expect("the wrap-up was generated");
        assert!(
            prompt.contains(&format!("cargo build --bin tool_{failing_call}")),
            "the call that failed must be in the account however late it came"
        );
        assert!(
            prompt.contains("it FAILED"),
            "and it must be marked as the failure it was"
        );
        // Still bounded: the cap moved, it did not go away.
        let listed = prompt.matches("ran the command").count();
        assert!(
            listed <= MAX_TURN_ACTIVITY,
            "the account stays bounded, got {listed} actions"
        );
    }

    /// The wrap-up's length is chosen from the size of the turn: a two-call
    /// turn deserves a sentence, a long one deserves a paragraph.
    #[gpui::test]
    async fn a_bigger_turn_earns_a_longer_wrap_up(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let read_aloud = steps_narration_reader(&provider, &sink, cx);
        let model = FakeSummaryModel::new("Done.");
        read_aloud.update(cx, |read_aloud, _| {
            read_aloud.set_summary_model(Some(Rc::new(model.clone())));
        });

        let closing = turn_in_progress(&read_aloud, cx);
        let chars = closing.read_with(cx, |markdown, _| markdown.source().len());
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.speculate_wrap_up(vec![closing.clone()], chars, cx);
        });
        cx.run_until_parked();
        assert!(
            model.prompts()[0].contains("under twenty-five words"),
            "one tool call is a sentence's worth of turn"
        );

        // A new turn, with a great deal more in it.
        read_aloud.update(cx, |read_aloud, cx| read_aloud.cancel_narration(cx));
        for index in 0..narration::LONG_TURN_TOOL_CALLS {
            let label = read_tool_label(&format!("crates/read_aloud/src/file_{index}.rs"), cx);
            read_aloud.update(cx, |read_aloud, cx| {
                read_aloud.narrate_tool_call(titled_call(label, NarrationKind::Read), cx);
            });
        }
        cx.run_until_parked();
        let closing = markdown_entity(&long_message_source(), cx);
        let chars = closing.read_with(cx, |markdown, _| markdown.source().len());
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.speculate_wrap_up(vec![closing], chars, cx);
        });
        cx.run_until_parked();
        assert!(
            model.prompts()[1].contains("under fifty-five words"),
            "a turn that touched ten things has more to sign off on, got {}",
            model.prompts()[1]
        );
    }

    /// The other half of the budget: a turn can be long because one thing in
    /// it took a long time, not because it did many things.
    #[gpui::test]
    async fn a_long_slow_turn_earns_a_longer_wrap_up(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let read_aloud = steps_narration_reader(&provider, &sink, cx);
        let model = FakeSummaryModel::new("Done.");
        read_aloud.update(cx, |read_aloud, _| {
            read_aloud.set_summary_model(Some(Rc::new(model.clone())));
        });

        let closing = turn_in_progress(&read_aloud, cx);
        cx.executor()
            .advance_clock(narration::LONG_TURN + Duration::from_secs(1));
        let chars = closing.read_with(cx, |markdown, _| markdown.source().len());
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.speculate_wrap_up(vec![closing], chars, cx);
        });
        cx.run_until_parked();
        assert!(
            model.prompts()[0].contains("under fifty-five words"),
            "the listener has been waiting a long time for this, got {}",
            model.prompts()[0]
        );
    }

    /// The addendum's core requirement: the wrap-up starts generating before
    /// the turn-end event, so the audio is ready the instant the turn ends
    /// instead of a round trip afterwards.
    #[gpui::test]
    async fn the_wrap_up_is_ready_before_the_turn_ends(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let read_aloud = steps_narration_reader(&provider, &sink, cx);
        let model = FakeSummaryModel::new("That is done, and the tests pass.");
        read_aloud.update(cx, |read_aloud, _| {
            read_aloud.set_summary_model(Some(Rc::new(model.clone())));
        });
        let closing = turn_in_progress(&read_aloud, cx);
        let chars = closing.read_with(cx, |markdown, _| markdown.source().len());

        read_aloud.update(cx, |read_aloud, cx| {
            assert!(
                read_aloud.wants_wrap_up(chars),
                "closing prose after real work is the cue to start"
            );
            read_aloud.speculate_wrap_up(vec![closing.clone()], chars, cx);
        });
        cx.run_until_parked();
        assert_eq!(model.prompts().len(), 1, "generated before the turn ended");
        assert!(
            model.prompts()[0].contains("still be being written"),
            "and the model is told it is seeing a partial message"
        );
        assert!(
            !provider
                .spoken()
                .iter()
                .any(|text| text.contains("tests pass")),
            "but not spoken until the turn actually ends, got {:?}",
            provider.spoken()
        );

        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.finish_turn(vec![closing], cx);
        });
        cx.run_until_parked();
        drain_narration(&sink, cx);
        assert!(
            provider
                .spoken()
                .contains(&"That is done, and the tests pass.".to_string()),
            "the turn ends on its wrap-up, got {:?}",
            provider.spoken()
        );
        assert_eq!(
            model.prompts().len(),
            1,
            "and the ready answer costs no second call"
        );
    }

    #[gpui::test]
    async fn the_wrap_up_is_generated_at_most_twice_a_turn(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let read_aloud = steps_narration_reader(&provider, &sink, cx);
        let model = FakeSummaryModel::new("That is done.");
        read_aloud.update(cx, |read_aloud, _| {
            read_aloud.set_summary_model(Some(Rc::new(model.clone())));
        });
        let closing = turn_in_progress(&read_aloud, cx);
        let chars = closing.read_with(cx, |markdown, _| markdown.source().len());

        // Four asks with the message growing far past the re-issue
        // threshold each time.
        for growth in 0..4 {
            read_aloud.update(cx, |read_aloud, cx| {
                read_aloud.speculate_wrap_up(
                    vec![closing.clone()],
                    chars + growth * WRAP_UP_REISSUE_GROWTH * 2,
                    cx,
                );
            });
            cx.run_until_parked();
        }
        assert_eq!(
            model.prompts().len(),
            MAX_WRAP_UP_ISSUES,
            "speculation is capped, not repeated on every chunk"
        );
    }

    #[gpui::test]
    async fn a_streaming_chunk_that_adds_little_does_not_re_ask(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let read_aloud = steps_narration_reader(&provider, &sink, cx);
        let model = FakeSummaryModel::new("That is done.");
        read_aloud.update(cx, |read_aloud, _| {
            read_aloud.set_summary_model(Some(Rc::new(model.clone())));
        });
        let closing = turn_in_progress(&read_aloud, cx);
        let chars = closing.read_with(cx, |markdown, _| markdown.source().len());

        for extra in [0, 1, 10, 40] {
            read_aloud.update(cx, |read_aloud, cx| {
                read_aloud.speculate_wrap_up(vec![closing.clone()], chars + extra, cx);
            });
            cx.run_until_parked();
        }
        assert_eq!(
            model.prompts().len(),
            1,
            "a few more words is not a reason to pay again"
        );
    }

    /// Status about work that has finished is worse than silence once the
    /// wrap-up can be said — but the sentence already sounding is never cut
    /// off mid-word to get there.
    #[gpui::test]
    async fn the_wrap_up_drops_queued_status_but_not_what_is_speaking(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let read_aloud = steps_narration_reader(&provider, &sink, cx);
        let model = FakeSummaryModel::new("Everything is in place and the tests pass.");
        read_aloud.update(cx, |read_aloud, _| {
            read_aloud.set_summary_model(Some(Rc::new(model.clone())));
        });

        for path in [
            "crates/read_aloud/src/player.rs",
            "crates/read_aloud/src/segmenter.rs",
            "crates/read_aloud/src/sink.rs",
        ] {
            let label = read_tool_label(path, cx);
            read_aloud.update(cx, |read_aloud, cx| {
                read_aloud.narrate_tool_call(titled_call(label.clone(), NarrationKind::Read), cx);
            });
            cx.run_until_parked();
        }
        let speaking_when_the_turn_ended = provider.spoken();
        assert_eq!(
            speaking_when_the_turn_ended,
            vec!["Reading player.".to_string()],
            "one is sounding and the rest are queued behind it"
        );

        let closing = markdown_entity(&long_message_source(), cx);
        let chars = closing.read_with(cx, |markdown, _| markdown.source().len());
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.speculate_wrap_up(vec![closing.clone()], chars, cx);
        });
        cx.run_until_parked();
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.finish_turn(vec![closing], cx);
        });
        cx.run_until_parked();
        assert_eq!(
            provider.spoken(),
            speaking_when_the_turn_ended,
            "the sentence already sounding is left to finish"
        );

        drain_narration(&sink, cx);
        assert_eq!(
            provider.spoken(),
            vec![
                "Reading player.".to_string(),
                "Everything is in place and the tests pass.".to_string(),
            ],
            "and the queued status gives way to the wrap-up rather than \
             draining as a list of things that already happened"
        );
    }

    /// A wrap-up that cannot be had must not take the turn's status down
    /// with it: the queued step lines are the only account left.
    #[gpui::test]
    async fn a_failed_wrap_up_leaves_the_turns_status_alone(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let read_aloud = steps_narration_reader(&provider, &sink, cx);
        let model = FakeSummaryModel::failing();
        read_aloud.update(cx, |read_aloud, _| {
            read_aloud.set_summary_model(Some(Rc::new(model.clone())));
        });

        for path in [
            "crates/read_aloud/src/player.rs",
            "crates/read_aloud/src/segmenter.rs",
        ] {
            let label = read_tool_label(path, cx);
            read_aloud.update(cx, |read_aloud, cx| {
                read_aloud.narrate_tool_call(titled_call(label.clone(), NarrationKind::Read), cx);
            });
            cx.run_until_parked();
        }

        let closing = markdown_entity(&long_message_source(), cx);
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.finish_turn(vec![closing], cx);
        });
        cx.run_until_parked();
        drain_narration(&sink, cx);

        let spoken = provider.spoken();
        assert!(
            spoken.contains(&"Then `crates/read_aloud/src/segmenter.rs`.".to_string())
                || spoken.iter().any(|text| text.contains("segmenter")),
            "the status queued behind the failed wrap-up still speaks, got {spoken:?}"
        );
        assert!(
            spoken.contains(&"I moved the poll loop onto a timer.".to_string()),
            "and the closing message falls back to its own opening, got {spoken:?}"
        );
    }

    #[gpui::test]
    async fn stopping_cancels_the_step_and_the_wrap_up(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let read_aloud = steps_narration_reader(&provider, &sink, cx);
        let model = FakeSummaryModel::new("Should never be spoken.");
        model.hold();
        read_aloud.update(cx, |read_aloud, _| {
            read_aloud.set_summary_model(Some(Rc::new(model.clone())));
        });

        let message = markdown_entity(&long_message_source(), cx);
        let label = read_tool_label("crates/read_aloud/src/player.rs", cx);
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.narrate_message(vec![message.clone()], cx);
            read_aloud.narrate_tool_call(titled_call(label.clone(), NarrationKind::Read), cx);
        });
        let_the_step_close(cx);
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.speculate_wrap_up(
                vec![message.clone()],
                message.read(cx).source().len(),
                cx,
            );
        });
        cx.run_until_parked();

        let spoken_before_the_stop = provider.spoken();
        read_aloud.update(cx, |read_aloud, cx| read_aloud.stop(cx));
        read_aloud.read_with(cx, |read_aloud, _| {
            assert!(
                read_aloud.narration_progress_is_idle(),
                "a stop drops the step and the wrap-up, not just their output"
            );
        });
        model.release_all();
        cx.run_until_parked();
        let_the_step_time_out(cx);
        drain_narration(&sink, cx);
        assert_eq!(
            provider.spoken(),
            spoken_before_the_stop,
            "nothing a stop cancelled may arrive afterwards, got {:?}",
            provider.spoken()
        );
    }

    #[gpui::test]
    async fn a_new_turn_cancels_the_previous_wrap_up(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let read_aloud = steps_narration_reader(&provider, &sink, cx);
        let model = FakeSummaryModel::new("About the turn the user moved on from.");
        model.hold();
        read_aloud.update(cx, |read_aloud, _| {
            read_aloud.set_summary_model(Some(Rc::new(model.clone())));
        });
        let closing = turn_in_progress(&read_aloud, cx);
        let chars = closing.read_with(cx, |markdown, _| markdown.source().len());
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.speculate_wrap_up(vec![closing], chars, cx);
        });
        cx.run_until_parked();
        let spoken_before_the_new_turn = provider.spoken();

        read_aloud.update(cx, |read_aloud, cx| read_aloud.cancel_narration(cx));
        read_aloud.read_with(cx, |read_aloud, _| {
            assert!(
                read_aloud.narration_progress_is_idle(),
                "a new turn drops the previous turn's wrap-up outright"
            );
        });
        model.release_all();
        cx.run_until_parked();
        drain_narration(&sink, cx);
        assert_eq!(
            provider.spoken(),
            spoken_before_the_new_turn,
            "the previous turn's wrap-up must not land on the next one, got {:?}",
            provider.spoken()
        );
    }

    /// A turn with no work in it is a message, and the per-message summary
    /// is what it wants — steps do not change that.
    #[gpui::test]
    async fn a_turn_that_only_wrote_prose_is_summarized_not_wrapped_up(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let read_aloud = steps_narration_reader(&provider, &sink, cx);
        let model = FakeSummaryModel::new("It put the poll loop on a timer.");
        read_aloud.update(cx, |read_aloud, _| {
            read_aloud.set_summary_model(Some(Rc::new(model.clone())));
        });

        let message = markdown_entity(&long_message_source(), cx);
        let chars = message.read_with(cx, |markdown, _| markdown.source().len());
        read_aloud.update(cx, |read_aloud, cx| {
            assert!(
                !read_aloud.wants_wrap_up(chars),
                "there is nothing to wrap up when nothing was done"
            );
            read_aloud.narrate_message(vec![message.clone()], cx);
            read_aloud.finish_turn(vec![message], cx);
        });
        cx.run_until_parked();
        drain_narration(&sink, cx);

        assert_eq!(model.prompts().len(), 1, "one call, the message summary");
        assert!(
            model.prompts()[0].contains("did and what it decided"),
            "and it is the summary prompt, not the step or wrap-up one"
        );
        assert_eq!(
            provider.spoken(),
            vec!["It put the poll loop on a timer.".to_string()]
        );
    }

    /// Regression (review I3): the whole point of generating a step's line
    /// while the step is still open is that it is usually in hand before
    /// anything closes the step. The turn ending is not a reason to throw it
    /// away and say the template instead.
    #[gpui::test]
    async fn the_turn_ending_does_not_discard_a_ready_step_line(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let read_aloud = steps_narration_reader(&provider, &sink, cx);
        // The step's line answers; the wrap-up that follows does not. A
        // successful wrap-up would displace the step line by design, so the
        // failure is what makes the difference observable — and it is the
        // case that matters, because that is when the step line is the
        // listener's last word on that step.
        let model =
            FakeSummaryModel::sequence(&["Checking the poll loop to see how it is timed.", ""]);
        read_aloud.update(cx, |read_aloud, _| {
            read_aloud.set_summary_model(Some(Rc::new(model.clone())));
        });

        let message = markdown_entity(&long_message_source(), cx);
        let label = read_tool_label("crates/read_aloud/src/player.rs", cx);
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.narrate_message(vec![message.clone()], cx);
            read_aloud.narrate_tool_call(titled_call(label.clone(), NarrationKind::Read), cx);
        });
        cx.run_until_parked();

        // The turn ends inside the quiet window, with the line already
        // answered and no wrap-up ready.
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.finish_turn(vec![message], cx);
        });
        cx.run_until_parked();
        drain_narration(&sink, cx);

        let spoken = provider.spoken();
        assert!(
            spoken.contains(&"Checking the poll loop to see how it is timed.".to_string()),
            "the line that was paid for is the one that gets said, got {spoken:?}"
        );
        assert!(
            !spoken.contains(&"Reading player.".to_string()),
            "not the template it would have fallen back to, got {spoken:?}"
        );
    }

    /// Regression (review I2): a tool call's label takes time to stop
    /// arriving, and the owning view cannot say it until it has. The quiet
    /// window must not close the step underneath that wait — doing so paid
    /// for a per-message summary the step design exists to avoid, and then
    /// spoke the call as a bare template with no step to belong to.
    #[gpui::test]
    async fn a_tool_call_whose_label_is_still_arriving_holds_the_step_open(
        cx: &mut TestAppContext,
    ) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let read_aloud = steps_narration_reader(&provider, &sink, cx);
        let model = FakeSummaryModel::new("Checking the player to see how it is timed.");
        read_aloud.update(cx, |read_aloud, _| {
            read_aloud.set_summary_model(Some(Rc::new(model.clone())));
        });

        let message = markdown_entity(&long_message_source(), cx);
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.narrate_message(vec![message], cx);
        });
        cx.run_until_parked();

        // The label keeps arriving for well past the quiet window.
        for _ in 0..3 {
            cx.executor()
                .advance_clock(STEP_IDLE_WINDOW - Duration::from_millis(100));
            cx.run_until_parked();
            read_aloud.update(cx, |read_aloud, cx| read_aloud.note_tool_call_pending(cx));
        }
        assert!(
            model.prompts().is_empty(),
            "no per-message summary may be paid for while the step waits, got {:?}",
            model.prompts()
        );
        read_aloud.read_with(cx, |read_aloud, _| {
            assert!(read_aloud.step.is_some(), "and the step is still open");
        });

        // The label settles and the call finally joins the step it belongs to.
        let label = read_tool_label("crates/read_aloud/src/player.rs", cx);
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.narrate_tool_call(titled_call(label.clone(), NarrationKind::Read), cx);
        });
        let_the_step_close(cx);
        drain_narration(&sink, cx);
        assert_eq!(
            model.prompts().len(),
            1,
            "exactly one call, and it is the step's, got {:?}",
            model.prompts()
        );
        assert!(
            provider
                .spoken()
                .contains(&"Checking the player to see how it is timed.".to_string()),
            "and the fused line still happens, got {:?}",
            provider.spoken()
        );
    }

    /// The agent's own words must not wait on the slow question. A tool call
    /// *existing* is what makes the prose in front of it a step; naming it
    /// takes a settle window on top, and putting that on the path to the
    /// first word is most of the latency this design exists to remove.
    #[gpui::test]
    async fn the_opening_does_not_wait_for_a_tool_label_to_settle(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let read_aloud = steps_narration_reader(&provider, &sink, cx);
        let message = markdown_entity(&long_message_source(), cx);
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.narrate_message(vec![message], cx);
            // A call has appeared; its label is still arriving, so there is
            // nothing sayable about it yet.
            read_aloud.note_tool_call_pending(cx);
        });
        cx.run_until_parked();

        // Not one tick of any timer has passed.
        assert_eq!(
            provider.spoken(),
            vec!["I moved the poll loop onto a timer.".to_string()],
            "the opening goes out on the call appearing, not on it being named"
        );
    }

    /// Regression (review I1): a wrap-up is speculated from prose that is
    /// currently the last entry, which is true of *every* prose block while
    /// it streams. A mid-turn paragraph long enough to cross the threshold
    /// must not become the turn's sign-off.
    #[gpui::test]
    async fn a_tool_call_invalidates_a_wrap_up_speculated_mid_turn(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let read_aloud = steps_narration_reader(&provider, &sink, cx);
        let model = FakeSummaryModel::sequence(&[
            "Halfway through, and the batcher looks wrong.",
            "That is the bug, and the fix is yours to make.",
        ]);
        read_aloud.update(cx, |read_aloud, _| {
            read_aloud.set_summary_model(Some(Rc::new(model.clone())));
        });

        // Some work, then a mid-turn paragraph that crosses the threshold.
        let mid_turn = turn_in_progress(&read_aloud, cx);
        let chars = mid_turn.read_with(cx, |markdown, _| markdown.source().len());
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.speculate_wrap_up(vec![mid_turn], chars, cx);
        });
        cx.run_until_parked();
        assert_eq!(model.prompts().len(), 1, "the mid-turn prose speculated");

        // ...but the agent had not finished: it does more work.
        let next = read_tool_label("crates/read_aloud/src/segmenter.rs", cx);
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.narrate_tool_call(titled_call(next.clone(), NarrationKind::Read), cx);
        });
        cx.run_until_parked();

        // The real closing message.
        let closing = markdown_entity(&long_message_source().replace("poll loop", "batcher"), cx);
        let closing_chars = closing.read_with(cx, |markdown, _| markdown.source().len());
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.speculate_wrap_up(vec![closing.clone()], closing_chars, cx);
            read_aloud.finish_turn(vec![closing], cx);
        });
        cx.run_until_parked();
        drain_narration(&sink, cx);

        let spoken = provider.spoken();
        assert!(
            !spoken.contains(&"Halfway through, and the batcher looks wrong.".to_string()),
            "a sign-off about the middle of the turn must never be the last \
             word, got {spoken:?}"
        );
        assert_eq!(
            spoken.last().map(String::as_str),
            Some("That is the bug, and the fix is yours to make."),
            "the turn ends on a wrap-up about its actual ending, got {spoken:?}"
        );
    }

    #[gpui::test]
    async fn a_turn_cannot_spend_more_than_its_wrap_up_budget_however_it_is_split(
        cx: &mut TestAppContext,
    ) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let read_aloud = steps_narration_reader(&provider, &sink, cx);
        let model = FakeSummaryModel::new("A wrap-up.");
        read_aloud.update(cx, |read_aloud, _| {
            read_aloud.set_summary_model(Some(Rc::new(model.clone())));
        });
        let prose = turn_in_progress(&read_aloud, cx);
        let chars = prose.read_with(cx, |markdown, _| markdown.source().len());

        // Four rounds of "prose long enough to speculate, then more work".
        // Invalidation must not refund the budget.
        for index in 0..4 {
            read_aloud.update(cx, |read_aloud, cx| {
                read_aloud.speculate_wrap_up(vec![prose.clone()], chars, cx);
            });
            cx.run_until_parked();
            let label = read_tool_label(&format!("crates/read_aloud/src/file_{index}.rs"), cx);
            read_aloud.update(cx, |read_aloud, cx| {
                read_aloud.narrate_tool_call(titled_call(label.clone(), NarrationKind::Read), cx);
            });
            cx.run_until_parked();
        }
        assert_eq!(
            model.prompts().len(),
            MAX_WRAP_UP_ISSUES,
            "a turn that keeps going must not buy a wrap-up per paragraph"
        );
    }

    // ---- Default-mode (`steps`) coverage of the shared machinery ----
    //
    // Eighteen pre-existing tests were retargeted to `actions` detail, which
    // is where the per-tool-call contract they pin now lives. The machinery
    // underneath is shared, but `steps` is the default and is what users
    // run, so these exercise the same paths through it.

    #[gpui::test]
    async fn a_step_line_washes_the_message_it_is_about(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let read_aloud = steps_narration_reader(&provider, &sink, cx);
        let model = FakeSummaryModel::new("Checking the player to see how it is timed.");
        read_aloud.update(cx, |read_aloud, _| {
            read_aloud.set_summary_model(Some(Rc::new(model)));
        });

        let message = markdown_entity(&long_message_source(), cx);
        let label = read_tool_label("crates/read_aloud/src/player.rs", cx);
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.narrate_message(vec![message.clone()], cx);
            read_aloud.narrate_tool_call(titled_call(label.clone(), NarrationKind::Read), cx);
        });
        let_the_step_close(cx);
        // Let the opening finish so the fused line takes the floor — but not
        // so far that playback ends and clears the highlight.
        let line = "Checking the player to see how it is timed.".to_string();
        for _ in 0..4 {
            if provider.spoken().contains(&line) {
                break;
            }
            sink.finish_one();
            cx.executor().advance_clock(POSITION_POLL_INTERVAL);
            cx.run_until_parked();
        }
        assert!(
            provider.spoken().contains(&line),
            "setup: the fused line is the one speaking, got {:?}",
            provider.spoken()
        );

        let source_length = message.read_with(cx, |markdown, _| markdown.source().len());
        assert_eq!(
            message.read_with(cx, |markdown, _| markdown.speaking_highlight().cloned()),
            Some(0..source_length),
            "the whole message washes while the line about it speaks"
        );
        assert_eq!(
            message.read_with(cx, |markdown, _| markdown
                .speaking_word_highlight()
                .cloned()),
            None,
            "a generated line's words map to nothing on screen, so no pill"
        );
    }

    #[gpui::test]
    async fn clicking_a_sentence_in_steps_mode_reads_that_message_in_full(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        provider.emit_word_timings();
        let sink = FakeSink::new();
        let read_aloud = steps_narration_reader(&provider, &sink, cx);
        let model = FakeSummaryModel::new("Checking the player to see how it is timed.");
        read_aloud.update(cx, |read_aloud, _| {
            read_aloud.set_summary_model(Some(Rc::new(model)));
        });

        let source = "First one. Second one. Third one.\n";
        let message = markdown_entity(source, cx);
        let label = read_tool_label("crates/read_aloud/src/player.rs", cx);
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.narrate_message(vec![message.clone()], cx);
            read_aloud.narrate_tool_call(titled_call(label.clone(), NarrationKind::Read), cx);
        });
        let_the_step_close(cx);

        let click = source.find("Second one.").expect("the test source has it");
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.seek_to_source_index(&message, click, true, cx);
        });
        cx.run_until_parked();
        cx.executor().advance_clock(POSITION_POLL_INTERVAL);
        cx.run_until_parked();

        assert!(
            provider
                .spoken()
                .ends_with(&["Second one.".to_string(), "Third one.".to_string()]),
            "the click reads the message itself, from the clicked sentence on, got {:?}",
            provider.spoken()
        );
        assert_eq!(
            message.read_with(cx, |markdown, _| markdown.speaking_highlight().cloned()),
            Some(11..22),
            "and any whole-message wash gives way to sentence highlighting"
        );

        // Narration resumes with the next thing the agent does.
        let next = read_tool_label("crates/read_aloud/src/segmenter.rs", cx);
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.narrate_tool_call(titled_call(next.clone(), NarrationKind::Read), cx);
        });
        cx.run_until_parked();
        drain_narration(&sink, cx);
        assert!(
            provider
                .spoken()
                .contains(&"Reading segmenter.".to_string()),
            "a drill-down does not switch the mode off, got {:?}",
            provider.spoken()
        );
        assert!(
            !provider
                .spoken()
                .contains(&"Checking the player to see how it is timed.".to_string()),
            "and the status queued when the user clicked is dropped rather \
             than delivered afterwards, got {:?}",
            provider.spoken()
        );
    }

    #[gpui::test]
    async fn steps_mode_switches_to_full_and_back(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let read_aloud = steps_narration_reader(&provider, &sink, cx);
        let message = markdown_entity("First one. Second one.\n", cx);

        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.set_mode(ReadAloudMode::Full, cx);
            read_aloud.enqueue_markdown(&message, true, cx);
        });
        cx.run_until_parked();
        assert!(
            provider.spoken().contains(&"First one.".to_string()),
            "full mode reads the prose, got {:?}",
            provider.spoken()
        );

        // Switching away drops what was sounding rather than re-reading it.
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.set_mode(ReadAloudMode::Narration, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            read_aloud.read_with(cx, |read_aloud, cx| read_aloud.playback_state(cx)),
            None,
            "the prose that was playing is dropped, not left mid-sentence"
        );
        assert_eq!(
            message.read_with(cx, |markdown, _| markdown.speaking_highlight().cloned()),
            None,
            "and its highlight goes with it"
        );
        let spoken_after_the_switch = provider.spoken();
        let label = read_tool_label("crates/read_aloud/src/player.rs", cx);
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.narrate_tool_call(titled_call(label.clone(), NarrationKind::Read), cx);
        });
        cx.run_until_parked();
        drain_narration(&sink, cx);
        let mut expected = spoken_after_the_switch;
        expected.push("Reading player.".to_string());
        assert_eq!(
            provider.spoken(),
            expected,
            "switching back into the default detail speaks status, and only that"
        );
    }

    /// The message-summary fallback chain, reached in `steps` through a step
    /// that closes with no tool calls in it.
    #[gpui::test]
    async fn a_message_summary_that_times_out_falls_back_in_steps_mode(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let read_aloud = steps_narration_reader(&provider, &sink, cx);
        let model = FakeSummaryModel::new("Never arrives.");
        model.hold();
        read_aloud.update(cx, |read_aloud, _| {
            read_aloud.set_summary_model(Some(Rc::new(model.clone())));
        });

        let message = markdown_entity(&long_message_source(), cx);
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.narrate_message(vec![message], cx);
        });
        let_the_step_close(cx);
        assert_eq!(
            model.prompts().len(),
            1,
            "a message with no tool calls is a message, and asks for a summary"
        );

        cx.executor().advance_clock(SUMMARY_TIMEOUT * 2);
        cx.run_until_parked();
        assert!(
            provider
                .spoken()
                .first()
                .is_some_and(|spoken| spoken.starts_with("I moved the poll loop onto a timer.")),
            "a model that never answers must not leave the default mode silent, got {:?}",
            provider.spoken()
        );
    }

    #[gpui::test]
    async fn a_failing_summary_model_falls_back_in_steps_mode(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let read_aloud = steps_narration_reader(&provider, &sink, cx);
        read_aloud.update(cx, |read_aloud, _| {
            read_aloud.set_summary_model(Some(Rc::new(FakeSummaryModel::failing())));
        });

        let message = markdown_entity(&long_message_source(), cx);
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.narrate_message(vec![message], cx);
        });
        let_the_step_close(cx);
        drain_narration(&sink, cx);
        assert!(
            provider
                .spoken()
                .first()
                .is_some_and(|spoken| spoken.starts_with("I moved the poll loop onto a timer.")),
            "a failing model falls back to the message's own opening, got {:?}",
            provider.spoken()
        );
    }

    /// Section F, end to end: a burst of reads, a lull, prose, more tools,
    /// then the closing message. Nothing may pile up into a rapid-fire list
    /// after the quiet stretch, and the turn must end on its wrap-up.
    #[gpui::test]
    async fn a_realistic_turn_stays_current_and_ends_on_its_wrap_up(cx: &mut TestAppContext) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let read_aloud = steps_narration_reader(&provider, &sink, cx);
        let model = FakeSummaryModel::sequence(&[
            "Editing the segmenter so the sentence split matches the design.",
            "That is done, the segmenter is updated and the tests pass.",
        ]);
        read_aloud.update(cx, |read_aloud, _| {
            read_aloud.set_summary_model(Some(Rc::new(model.clone())));
        });

        // Five reads back to back, with nothing draining.
        for index in 0..5 {
            let label = read_tool_label(&format!("crates/read_aloud/src/file_{index}.rs"), cx);
            read_aloud.update(cx, |read_aloud, cx| {
                read_aloud.narrate_tool_call(titled_call(label.clone(), NarrationKind::Read), cx);
            });
            cx.run_until_parked();
        }
        read_aloud.update(cx, |read_aloud, _| {
            assert!(
                read_aloud.narration.len() <= 3,
                "the burst must stay bounded, got {}",
                read_aloud.narration.len()
            );
        });

        // A lull, then prose and the tool calls it explains.
        cx.executor().advance_clock(Duration::from_secs(3));
        cx.run_until_parked();
        let message = markdown_entity(&long_message_source(), cx);
        let edit = markdown_entity("crates/read\\_aloud/src/segmenter.rs", cx);
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.narrate_message(vec![message], cx);
            read_aloud.narrate_tool_call(titled_call(edit.clone(), NarrationKind::Edit), cx);
        });
        let_the_step_close(cx);
        assert!(
            model.prompts()[0].contains("I moved the poll loop onto a timer.")
                && model.prompts()[0].contains("segmenter.rs"),
            "the step asked about the prose and the tool call together, got {:?}",
            model.prompts()
        );
        // The audio plays while the agent thinks about what to write next.
        drain_narration(&sink, cx);

        // Two more calls right before the end, so there is a real backlog
        // for the wrap-up to displace rather than an empty queue.
        for path in [
            "crates/read_aloud/src/sink.rs",
            "crates/read_aloud/src/provider.rs",
        ] {
            let label = read_tool_label(path, cx);
            read_aloud.update(cx, |read_aloud, cx| {
                read_aloud.narrate_tool_call(titled_call(label.clone(), NarrationKind::Read), cx);
            });
            cx.run_until_parked();
        }

        // The closing message streams, and the wrap-up starts on it.
        let closing = markdown_entity(&long_message_source().replace("poll loop", "segmenter"), cx);
        let chars = closing.read_with(cx, |markdown, _| markdown.source().len());
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.speculate_wrap_up(vec![closing.clone()], chars, cx);
        });
        cx.run_until_parked();
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.finish_turn(vec![closing], cx);
        });
        cx.run_until_parked();
        drain_narration(&sink, cx);

        let spoken = provider.spoken();
        assert!(
            spoken.len() <= 6,
            "a turn's worth of narration must stay short, got {spoken:?}"
        );
        assert_eq!(
            spoken.last().map(String::as_str),
            Some("That is done, the segmenter is updated and the tests pass."),
            "the turn ends on its wrap-up, got {spoken:?}"
        );
        assert!(
            spoken.contains(
                &"Editing the segmenter so the sentence split matches the design.".to_string()
            ),
            "and the step in the middle said what it was doing and why, got {spoken:?}"
        );
        assert!(
            !spoken.iter().any(|line| line.contains("provider")),
            "status still queued when the wrap-up landed is dropped rather \
             than drained as a list after it, got {spoken:?}"
        );
        // Duty cycle, the other half of "short". Measured on this fixture at
        // thirty-seven words — about fifteen seconds at two and a half words
        // a second, over a turn of roughly thirty-five. The bound is set one
        // short utterance above that, so spoken volume cannot drift upwards
        // unnoticed the way it did when the wrap-up budget grew.
        let words: usize = spoken
            .iter()
            .map(|line| line.split_whitespace().count())
            .sum();
        assert!(
            words <= 45,
            "narration must not talk over the turn it is describing, got {words} \
             words in {spoken:?}"
        );
    }

    #[gpui::test]
    async fn switching_narration_detail_takes_effect_without_latching_silence(
        cx: &mut TestAppContext,
    ) {
        let provider = FakeTts::new();
        let sink = FakeSink::new();
        let read_aloud = steps_narration_reader(&provider, &sink, cx);
        let message = markdown_entity(&long_message_source(), cx);
        let label = read_tool_label("crates/read_aloud/src/player.rs", cx);
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.narrate_message(vec![message], cx);
            read_aloud.narrate_tool_call(titled_call(label.clone(), NarrationKind::Read), cx);
        });
        cx.run_until_parked();

        let spoken_before_the_switch = provider.spoken();
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.set_narration_detail(NarrationDetail::Actions, cx);
        });
        let_the_step_close(cx);
        assert_eq!(
            provider.spoken(),
            spoken_before_the_switch,
            "the half-accumulated step means nothing in the mode switched into, got {:?}",
            provider.spoken()
        );

        let next = read_tool_label("crates/read_aloud/src/segmenter.rs", cx);
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.narrate_tool_call(titled_call(next.clone(), NarrationKind::Read), cx);
        });
        cx.run_until_parked();
        drain_narration(&sink, cx);
        let mut expected = spoken_before_the_switch;
        expected.push("Reading segmenter.".to_string());
        assert_eq!(
            provider.spoken(),
            expected,
            "the detail just switched into must not start out mute, and must \
             say exactly the one new thing"
        );
    }
}
