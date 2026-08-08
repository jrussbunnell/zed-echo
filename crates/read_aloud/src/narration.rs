use gpui::{App, AppContext as _, Entity, SharedString, Task};
use markdown::Markdown;

/// How many narrations may wait their turn before a burst is collapsed into
/// a count. This cap is also the staleness bound: nothing can be more than
/// this many utterances behind what the agent is actually doing, and a
/// spoken tool label runs a second or two.
const MAX_PENDING_NARRATIONS: usize = 3;

/// Above this many characters a finished assistant message is worth a
/// summary; below it, saying the message costs less than condensing it —
/// and speaking it verbatim keeps the word highlight.
pub const TRIVIAL_MESSAGE_CHARS: usize = 200;

/// How many of a message's own sentences the no-model fallback speaks.
pub const OPENING_SENTENCES: usize = 2;

/// The most a summary may be before it stops being one. A model that
/// ignores the length instruction would otherwise reintroduce exactly the
/// problem narration mode exists to solve, so an over-long reply is
/// discarded in favor of the fallback.
pub const MAX_SUMMARY_CHARS: usize = 600;

/// The most of a message that is worth sending to the summary model. Longer
/// messages are truncated rather than skipped: the opening carries the
/// decisions, and the tail is usually code or a recap.
const MAX_SUMMARY_INPUT_CHARS: usize = 8_000;

/// The tool families narration knows how to count. The owning view maps the
/// agent protocol's own tool kinds onto these, so this crate needs no
/// dependency on it — collapsing a burst is the only thing the kind is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NarrationKind {
    Read,
    Edit,
    Delete,
    Move,
    Search,
    Execute,
    Fetch,
    Other,
}

impl NarrationKind {
    /// The verb narration puts in front of a label that has none of its own.
    fn verb(self) -> &'static str {
        match self {
            NarrationKind::Read => "Reading",
            NarrationKind::Edit => "Editing",
            NarrationKind::Delete => "Deleting",
            NarrationKind::Move => "Moving",
            NarrationKind::Search => "Searching",
            NarrationKind::Execute => "Running",
            NarrationKind::Fetch => "Fetching",
            NarrationKind::Other => "Running",
        }
    }

    /// What a collapsed run of `count` calls of this kind is spoken as.
    fn collapsed_phrase(self, count: usize) -> String {
        let count = spoken_count(count);
        match self {
            NarrationKind::Read => format!("Reading {count} files."),
            NarrationKind::Edit => format!("Editing {count} files."),
            NarrationKind::Delete => format!("Deleting {count} files."),
            NarrationKind::Move => format!("Moving {count} files."),
            NarrationKind::Search => format!("Running {count} searches."),
            NarrationKind::Execute => format!("Running {count} commands."),
            NarrationKind::Fetch => format!("Fetching {count} pages."),
            NarrationKind::Other => format!("Taking {count} more steps."),
        }
    }
}

/// Small counts read better spelled out than as digits, and every collapsed
/// burst is a small count by construction.
fn spoken_count(count: usize) -> String {
    let word = match count {
        2 => "two",
        3 => "three",
        4 => "four",
        5 => "five",
        6 => "six",
        7 => "seven",
        8 => "eight",
        9 => "nine",
        10 => "ten",
        11 => "eleven",
        12 => "twelve",
        _ => return count.to_string(),
    };
    word.to_string()
}

/// Tool-call progress, the only narration a burst may collapse. `count` is
/// 1 for a single call and grows as a collapsed burst absorbs more.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Progress {
    kind: NarrationKind,
    count: usize,
}

/// One thing narration will say.
pub(crate) struct Narration {
    /// The entity whose text is actually spoken.
    pub spoken: Entity<Markdown>,
    /// Blocks to wash while `spoken` sounds. Empty means `spoken` is itself
    /// on screen (a tool label, or a message short enough to speak
    /// verbatim) and gets the ordinary highlight treatment instead.
    pub wash: Vec<Entity<Markdown>>,
    progress: Option<Progress>,
}

/// The narration backlog: FIFO, with three rules that keep spoken length
/// from tracking written length.
///
/// 1. A tool call whose label repeats the previous one is dropped.
/// 2. Past [`MAX_PENDING_NARRATIONS`], the last run of tool calls —
///    including any count an earlier collapse produced — becomes a single
///    count. A burst of eight reads comes out as "reading seven files"
///    followed by the one the agent is on, not eight sentences the listener
///    hears long after they stopped being true.
/// 3. Whatever survives that is hard-capped at
///    [`MAX_PENDING_NARRATIONS`], discarding the oldest. Rule 2 needs a run
///    to work on and an interleaved message/tool stream never has one, so
///    without this the backlog grows without limit.
///
/// Together these are the staleness bound: the queue can never hold more
/// than [`MAX_PENDING_NARRATIONS`] utterances, whatever the agent does.
#[derive(Default)]
pub(crate) struct NarrationQueue {
    pending: Vec<Narration>,
    /// The last tool label accepted, whether it was queued or spoken
    /// immediately, so back-to-back duplicates are suppressed across the
    /// boundary between the queue and the player.
    last_tool_label: Option<SharedString>,
}

impl NarrationQueue {
    /// Queues a tool call's own label. Returns whether anything was queued:
    /// a label identical to the previous one adds nothing to a listener.
    pub fn push_tool_call(
        &mut self,
        label: Entity<Markdown>,
        kind: NarrationKind,
        cx: &mut App,
    ) -> bool {
        let source = label.read(cx).source().clone();
        if source.trim().is_empty() || self.last_tool_label.as_ref() == Some(&source) {
            return false;
        }
        self.last_tool_label = Some(source.clone());
        let narration = match generated_phrase(&source, kind) {
            Some(phrase) => Narration {
                spoken: cx.new(|cx| Markdown::new(phrase.into(), None, None, cx)),
                wash: vec![label],
                progress: Some(Progress { kind, count: 1 }),
            },
            None => Narration {
                spoken: label,
                wash: Vec::new(),
                progress: Some(Progress { kind, count: 1 }),
            },
        };
        self.pending.push(narration);
        self.collapse_backlog(cx);
        self.enforce_cap();
        true
    }

    /// Queues text that is on screen and speaks for itself — a message
    /// short enough that summarizing it would cost more than saying it.
    pub fn push_inline(&mut self, text: Entity<Markdown>, cx: &mut App) {
        self.last_tool_label = None;
        self.pending.push(Narration {
            spoken: text,
            wash: Vec::new(),
            progress: None,
        });
        self.collapse_backlog(cx);
        self.enforce_cap();
    }

    /// Queues text that exists nowhere on screen (a model summary, or a
    /// message's opening sentences), alongside the blocks to wash so the
    /// listener can find what it is about.
    pub fn push_summary(
        &mut self,
        spoken: Entity<Markdown>,
        message: Vec<Entity<Markdown>>,
        cx: &mut App,
    ) {
        self.last_tool_label = None;
        self.pending.push(Narration {
            spoken,
            wash: message,
            progress: None,
        });
        // A burst already queued in front of this collapses rather than
        // being discarded by the cap: a count keeps the information, a
        // discard loses it.
        self.collapse_backlog(cx);
        self.enforce_cap();
    }

    pub fn pop(&mut self) -> Option<Narration> {
        if self.pending.is_empty() {
            return None;
        }
        Some(self.pending.remove(0))
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    pub fn clear(&mut self) {
        self.pending.clear();
        self.last_tool_label = None;
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.pending.len()
    }

    /// Replaces the last run of tool-call narrations with a single count
    /// once the backlog outgrows the cap. Only one run is touched, and
    /// nothing is reordered, so a summary already queued keeps its place in
    /// the story.
    ///
    /// The run does not have to be at the end: a summary landing behind a
    /// burst should still let the burst collapse in front of it.
    fn collapse_backlog(&mut self, cx: &mut App) {
        if self.pending.len() <= MAX_PENDING_NARRATIONS {
            return;
        }
        let Some(run_end) = self
            .pending
            .iter()
            .rposition(|narration| narration.progress.is_some())
        else {
            return;
        };
        let run_start = self.pending[..run_end]
            .iter()
            .rposition(|narration| narration.progress.is_none())
            .map_or(0, |index| index + 1);
        if run_end + 1 - run_start < 2 {
            return;
        }
        let run: Vec<Narration> = self.pending.drain(run_start..=run_end).collect();
        let mut count = 0;
        let mut kind = None;
        for narration in &run {
            let Some(progress) = narration.progress else {
                continue;
            };
            count += progress.count;
            kind = match kind {
                None => Some(progress.kind),
                Some(existing) if existing == progress.kind => Some(existing),
                Some(_) => Some(NarrationKind::Other),
            };
        }
        let kind = kind.unwrap_or(NarrationKind::Other);
        let phrase = kind.collapsed_phrase(count);
        let text = cx.new(|cx| Markdown::new(phrase.into(), None, None, cx));
        self.pending.insert(
            run_start,
            Narration {
                spoken: text,
                wash: Vec::new(),
                progress: Some(Progress { kind, count }),
            },
        );
    }

    /// The backlog's hard bound, whatever it is made of.
    ///
    /// Collapsing only merges a *run* of tool calls, so a turn that
    /// alternates short messages and tool calls — an agent narrating itself
    /// in prose between steps — has no run longer than one and would
    /// otherwise grow without limit: twenty queued utterances is half a
    /// minute of speech describing things that stopped being true long
    /// before the listener hears them. The oldest go first, because they are
    /// the stalest; ambient status is worth nothing if it is not current.
    fn enforce_cap(&mut self) {
        while self.pending.len() > MAX_PENDING_NARRATIONS {
            let dropped = self.pending.remove(0);
            if dropped.progress.is_some() {
                log::debug!("read_aloud: dropped the oldest tool narration to keep status current");
            } else {
                // A message narration is a summary that was paid for, or a
                // short message's own words, and it will not be retried: the
                // message stays marked summarized, deliberately, because
                // exactly one model call per message is the cost rule.
                // Losing it silently is what makes that indistinguishable
                // from a bug, so it is said out loud.
                log::warn!(
                    "read_aloud: narration fell far enough behind to drop a queued message \
                     summary; it will not be retried"
                );
            }
        }
    }
}

/// The phrase to speak in place of a label that does not read aloud well on
/// its own. `None` — the common case — means the label speaks for itself
/// and is spoken, and highlighted, in place.
///
/// Two labels need this. A terminal call's label is the raw command, and
/// `acp_thread` hands it over as plain text (links only), which the
/// segmenter has nothing to say about at all. An edit or write call's label
/// is nothing but a file path, with no verb, and reads out one path
/// component at a time.
///
/// Both go into the phrase as a code span, so the substitute rules that
/// already turn `player.rs` into "player" and `cargo test -p read_aloud`
/// into "cargo test p read" in prose do the same work here — no new
/// speech logic, and the terse form is the point.
fn generated_phrase(label: &str, kind: NarrationKind) -> Option<String> {
    let label = label.trim();
    // A backtick would close the code span early and speak the remainder
    // as prose; the label is better off spoken as it stands.
    if label.is_empty() || label.contains('`') {
        return None;
    }
    if kind == NarrationKind::Execute {
        return Some(format!("{} `{label}`.", kind.verb()));
    }
    if label.contains(char::is_whitespace) || !label.contains(['/', '\\', '.']) {
        return None;
    }
    // Edit titles reach us markdown-escaped, and a backslash inside the
    // code span would be spoken rather than ignored.
    let path = unescape_markdown_punctuation(label);
    Some(format!("{} `{path}`.", kind.verb()))
}

/// Drops the backslashes markdown escaping added, leaving the path as the
/// user sees it.
fn unescape_markdown_punctuation(text: &str) -> String {
    let mut unescaped = String::with_capacity(text.len());
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '\\'
            && characters
                .peek()
                .is_some_and(|next| next.is_ascii_punctuation())
        {
            continue;
        }
        unescaped.push(character);
    }
    unescaped
}

/// A one-shot text completion, used to condense a finished assistant
/// message. A trait so this crate stays free of any model provider: the
/// agent panel supplies an implementation backed by the language model
/// registry, and tests supply a fake.
///
/// Deliberately not `Send + Sync`, unlike [`crate::TtsProvider`]: this is
/// only ever called from the foreground with an `App` in hand, and the
/// model handles the panel has are themselves foreground-only.
pub trait SummaryModel {
    fn complete(&self, prompt: String, cx: &mut App) -> Task<anyhow::Result<String>>;
}

/// The prompt narration mode sends for a finished assistant message. It
/// asks for what the agent *did and decided* rather than a precis of the
/// prose, because the prose is already on screen — the spoken line exists
/// to tell someone who is not looking whether they need to.
pub fn summary_prompt(message: &str) -> String {
    let message = truncate_chars(message, MAX_SUMMARY_INPUT_CHARS);
    format!(
        "You are narrating a coding agent's progress out loud to someone who is not \
         looking at the screen.\n\n\
         Summarize what the agent did and what it decided in the message below.\n\n\
         Rules:\n\
         - At most two sentences. Use one if one will do.\n\
         - Plain spoken English, present tense, no preamble and no sign-off.\n\
         - No markdown, no code, no command lines, no lists.\n\
         - Name a file only when the file is the point.\n\
         - Say what changed and what was chosen, not how it was written.\n\n\
         Message:\n\
         ---\n\
         {message}\n\
         ---\n\n\
         Reply with the summary and nothing else."
    )
}

/// Makes a model reply speakable, or rejects it. Line structure and
/// wrapping punctuation are stripped so the text segments as the one or two
/// sentences it claims to be; an empty or over-long reply is no summary at
/// all and the caller falls back.
pub fn clean_summary(reply: &str) -> Option<String> {
    let mut cleaned = String::with_capacity(reply.len());
    for word in reply.split_whitespace() {
        if !cleaned.is_empty() {
            cleaned.push(' ');
        }
        cleaned.push_str(word);
    }
    let cleaned = cleaned
        .trim_matches(|character: char| {
            character == '"' || character == '\'' || character == '`' || character.is_whitespace()
        })
        .to_string();
    if cleaned.is_empty() {
        return None;
    }
    if cleaned.chars().count() > MAX_SUMMARY_CHARS {
        log::warn!(
            "read_aloud: the summary model replied with {} characters, past the {MAX_SUMMARY_CHARS} \
             a summary may be; speaking the message's opening instead",
            cleaned.chars().count()
        );
        return None;
    }
    Some(cleaned)
}

/// Truncates on a character boundary, so a long message costs a bounded
/// number of tokens instead of being skipped entirely.
fn truncate_chars(text: &str, limit: usize) -> &str {
    match text.char_indices().nth(limit) {
        Some((index, _)) => text.get(..index).unwrap_or(text),
        None => text,
    }
}

#[cfg(any(test, feature = "test-support"))]
#[derive(Default)]
struct FakeSummaryModelState {
    prompts: Vec<String>,
    /// Replies handed out in call order, the last one repeating. Empty
    /// fails every call.
    replies: Vec<String>,
    hold: bool,
    held: Vec<futures::channel::oneshot::Sender<()>>,
}

/// Test double for [`SummaryModel`]. Records every prompt so cost
/// discipline is checkable, and can hold a call in flight the way a real
/// one does so cancellation is testable.
#[cfg(any(test, feature = "test-support"))]
#[derive(Clone, Default)]
pub struct FakeSummaryModel {
    state: std::sync::Arc<std::sync::Mutex<FakeSummaryModelState>>,
}

#[cfg(any(test, feature = "test-support"))]
impl FakeSummaryModel {
    pub fn new(reply: &str) -> Self {
        Self::sequence(&[reply])
    }

    /// Answers each call with the next reply, repeating the last once they
    /// run out — so a test with two messages in flight can tell their
    /// summaries apart.
    pub fn sequence(replies: &[&str]) -> Self {
        let model = Self::default();
        if let Ok(mut state) = model.state.lock() {
            state.replies = replies.iter().map(|reply| reply.to_string()).collect();
        }
        model
    }

    /// A model that always fails, for the fallback paths.
    pub fn failing() -> Self {
        Self::default()
    }

    pub fn prompts(&self) -> Vec<String> {
        self.state
            .lock()
            .map(|state| state.prompts.clone())
            .unwrap_or_default()
    }

    /// Parks every subsequent call until [`Self::release_all`].
    pub fn hold(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.hold = true;
        }
    }

    pub fn release_all(&self) {
        let held = {
            let Ok(mut state) = self.state.lock() else {
                return;
            };
            state.hold = false;
            std::mem::take(&mut state.held)
        };
        for sender in held {
            sender.send(()).ok();
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
impl SummaryModel for FakeSummaryModel {
    fn complete(&self, prompt: String, cx: &mut App) -> Task<anyhow::Result<String>> {
        let (gate, call) = {
            let Ok(mut state) = self.state.lock() else {
                return Task::ready(Err(anyhow::anyhow!("FakeSummaryModel state poisoned")));
            };
            state.prompts.push(prompt);
            let call = state.prompts.len() - 1;
            let gate = state.hold.then(|| {
                let (sender, receiver) = futures::channel::oneshot::channel();
                state.held.push(sender);
                receiver
            });
            (gate, call)
        };
        let state = self.state.clone();
        cx.background_spawn(async move {
            if let Some(gate) = gate {
                gate.await.ok();
            }
            let Ok(state) = state.lock() else {
                anyhow::bail!("FakeSummaryModel state poisoned");
            };
            state
                .replies
                .get(call.min(state.replies.len().saturating_sub(1)))
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("FakeSummaryModel was asked to fail"))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    fn markdown(text: &str, cx: &mut TestAppContext) -> Entity<Markdown> {
        cx.new(|cx| Markdown::new(text.to_string().into(), None, None, cx))
    }

    #[gpui::test]
    async fn a_repeated_tool_label_is_not_said_twice(cx: &mut TestAppContext) {
        let mut queue = NarrationQueue::default();
        let first = markdown("Read player.rs", cx);
        let same = markdown("Read player.rs", cx);
        let other = markdown("Read segmenter.rs", cx);
        cx.run_until_parked();

        cx.update(|cx| {
            assert!(queue.push_tool_call(first, NarrationKind::Read, cx));
            assert!(
                !queue.push_tool_call(same, NarrationKind::Read, cx),
                "an identical label back to back tells the listener nothing new"
            );
            assert!(queue.push_tool_call(other, NarrationKind::Read, cx));
        });
        assert_eq!(queue.len(), 2);
    }

    #[gpui::test]
    async fn a_burst_past_the_cap_collapses_into_a_count(cx: &mut TestAppContext) {
        let mut queue = NarrationQueue::default();
        let labels: Vec<_> = (0..5)
            .map(|index| markdown(&format!("Read file_{index}.rs"), cx))
            .collect();
        cx.run_until_parked();

        cx.update(|cx| {
            for label in labels {
                queue.push_tool_call(label, NarrationKind::Read, cx);
            }
        });
        cx.run_until_parked();

        assert_eq!(
            queue.len(),
            2,
            "the backlog collapses, leaving the newest call still named"
        );
        let collapsed = queue.pop().expect("the collapsed burst is queued");
        assert_eq!(
            collapsed
                .spoken
                .read_with(cx, |markdown, _| markdown.source().to_string()),
            "Reading four files."
        );
        let newest = queue.pop().expect("the newest call survives the collapse");
        assert_eq!(
            newest
                .spoken
                .read_with(cx, |markdown, _| markdown.source().to_string()),
            "Read file_4.rs",
            "what the agent is doing right now is worth naming"
        );
    }

    #[gpui::test]
    async fn a_sustained_burst_folds_back_into_one_count(cx: &mut TestAppContext) {
        let mut queue = NarrationQueue::default();
        let labels: Vec<_> = (0..8)
            .map(|index| markdown(&format!("Read file_{index}.rs"), cx))
            .collect();
        cx.run_until_parked();

        cx.update(|cx| {
            for label in labels {
                queue.push_tool_call(label, NarrationKind::Read, cx);
            }
        });
        cx.run_until_parked();

        let collapsed = queue.pop().expect("the collapsed burst is queued");
        assert_eq!(
            collapsed
                .spoken
                .read_with(cx, |markdown, _| markdown.source().to_string()),
            "Reading seven files.",
            "a second collapse absorbs the count the first one produced"
        );
    }

    #[gpui::test]
    async fn a_mixed_burst_collapses_without_claiming_a_kind(cx: &mut TestAppContext) {
        let mut queue = NarrationQueue::default();
        let labels: Vec<_> = (0..4)
            .map(|index| markdown(&format!("Step {index}"), cx))
            .collect();
        cx.run_until_parked();

        cx.update(|cx| {
            for (index, label) in labels.into_iter().enumerate() {
                let kind = if index.is_multiple_of(2) {
                    NarrationKind::Read
                } else {
                    NarrationKind::Execute
                };
                queue.push_tool_call(label, kind, cx);
            }
        });
        cx.run_until_parked();

        let collapsed = queue.pop().expect("the collapsed burst is queued");
        assert_eq!(
            collapsed
                .spoken
                .read_with(cx, |markdown, _| markdown.source().to_string()),
            "Taking four more steps."
        );
    }

    /// Regression: collapsing only ever merged a run of tool calls, and an
    /// agent that narrates itself in short prose between steps produces
    /// `[message, tool, message, tool, …]` — every run is length one, so
    /// nothing ever collapsed and the backlog grew without limit. Twenty
    /// queued utterances is half a minute of speech about things that
    /// stopped being true long before the listener hears them.
    #[gpui::test]
    async fn an_interleaved_message_and_tool_stream_stays_bounded(cx: &mut TestAppContext) {
        let mut queue = NarrationQueue::default();
        let mut entities = Vec::new();
        for index in 0..10 {
            entities.push((
                markdown(&format!("Message {index}."), cx),
                markdown(&format!("Read file_{index}.rs"), cx),
            ));
        }
        cx.run_until_parked();

        cx.update(|cx| {
            for (message, label) in entities {
                queue.push_inline(message, cx);
                queue.push_tool_call(label, NarrationKind::Read, cx);
            }
        });
        cx.run_until_parked();

        assert!(
            queue.len() <= MAX_PENDING_NARRATIONS,
            "the backlog must stay bounded whatever it is made of, got {}",
            queue.len()
        );
        // What survives is the newest: the oldest status is the stalest.
        let newest = queue.pop().expect("something is queued");
        let spoken = newest
            .spoken
            .read_with(cx, |markdown, _| markdown.source().to_string());
        assert!(
            spoken.contains('9') || spoken.contains('8'),
            "the survivors must be the most recent status, got {spoken:?}"
        );
    }

    /// A burst that a summary lands behind must still collapse: the run to
    /// merge is not always at the very end of the queue.
    #[gpui::test]
    async fn a_burst_collapses_even_once_a_summary_is_queued_behind_it(cx: &mut TestAppContext) {
        let mut queue = NarrationQueue::default();
        let labels: Vec<_> = (0..3)
            .map(|index| markdown(&format!("Read file_{index}.rs"), cx))
            .collect();
        let summary = markdown("It moved the poll loop onto a timer.", cx);
        let message = markdown("Long message body.", cx);
        cx.run_until_parked();

        cx.update(|cx| {
            for label in labels {
                queue.push_tool_call(label, NarrationKind::Read, cx);
            }
        });
        cx.update(|cx| queue.push_summary(summary, vec![message], cx));
        cx.run_until_parked();

        assert_eq!(queue.len(), 2);
        let collapsed = queue.pop().expect("the burst is still first");
        assert_eq!(
            collapsed
                .spoken
                .read_with(cx, |markdown, _| markdown.source().to_string()),
            "Reading three files.",
            "the run collapses in place rather than being discarded by the cap"
        );
        let summary = queue.pop().expect("the summary follows it");
        assert_eq!(
            summary
                .spoken
                .read_with(cx, |markdown, _| markdown.source().to_string()),
            "It moved the poll loop onto a timer."
        );
    }

    #[gpui::test]
    async fn a_queued_summary_keeps_its_place_when_a_burst_collapses(cx: &mut TestAppContext) {
        let mut queue = NarrationQueue::default();
        let summary = markdown("It switched the poll loop to a timer.", cx);
        let message = markdown("Long message body.", cx);
        let labels: Vec<_> = (0..4)
            .map(|index| markdown(&format!("Read file_{index}.rs"), cx))
            .collect();
        cx.run_until_parked();

        cx.update(|cx| queue.push_summary(summary, vec![message], cx));
        cx.update(|cx| {
            for label in labels {
                queue.push_tool_call(label, NarrationKind::Read, cx);
            }
        });
        cx.run_until_parked();

        let first = queue.pop().expect("the summary is still first");
        assert_eq!(
            first
                .spoken
                .read_with(cx, |markdown, _| markdown.source().to_string()),
            "It switched the poll loop to a timer.",
            "only the trailing run collapses, so the story keeps its order"
        );
        let second = queue.pop().expect("the collapsed burst follows it");
        assert_eq!(
            second
                .spoken
                .read_with(cx, |markdown, _| markdown.source().to_string()),
            "Reading three files."
        );
    }

    #[test]
    fn the_prompt_carries_the_message_and_the_length_rule() {
        let prompt = summary_prompt("I rewrote the poll loop.");
        assert!(prompt.contains("I rewrote the poll loop."));
        assert!(prompt.contains("At most two sentences"));
        assert!(prompt.contains("did and what it decided"));
    }

    #[test]
    fn the_prompt_truncates_an_enormous_message_instead_of_skipping_it() {
        let message = "x".repeat(MAX_SUMMARY_INPUT_CHARS * 2);
        let prompt = summary_prompt(&message);
        assert!(prompt.contains(&"x".repeat(MAX_SUMMARY_INPUT_CHARS)));
        assert!(!prompt.contains(&"x".repeat(MAX_SUMMARY_INPUT_CHARS + 1)));
    }

    #[test]
    fn a_reply_is_flattened_and_unquoted() {
        assert_eq!(
            clean_summary("  \"It moved the poll loop\nonto a timer.\"  ").as_deref(),
            Some("It moved the poll loop onto a timer.")
        );
    }

    #[test]
    fn an_empty_or_enormous_reply_is_no_summary() {
        assert_eq!(clean_summary("   \n  "), None);
        assert_eq!(clean_summary(&"word ".repeat(MAX_SUMMARY_CHARS)), None);
    }
}
