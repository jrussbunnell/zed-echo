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

/// The most a single step line may be. One spoken sentence of ambient
/// status; anything longer is the model ignoring the brief, and the terse
/// templated form beats a paragraph that arrives late.
pub const MAX_STEP_LINE_CHARS: usize = 220;

/// The most of a message that is worth sending to the summary model. Longer
/// messages are truncated rather than skipped: the opening carries the
/// decisions, and the tail is usually code or a recap.
const MAX_SUMMARY_INPUT_CHARS: usize = 8_000;

/// How many already-spoken lines a prompt carries. Enough for the model to
/// avoid repeating itself over a burst of steps, short enough not to crowd
/// out the material it is meant to condense.
pub const RECENT_LINES: usize = 5;

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
    /// The kind of the last tool call accepted, so a run of the same kind
    /// elides its verb ("Reading player. Then segmenter.") instead of
    /// chanting it. Cleared by anything that is not a tool call, because the
    /// elision only reads as continuation when it directly follows.
    last_tool_kind: Option<NarrationKind>,
    /// What narration has committed to saying, newest last, capped at
    /// [`RECENT_LINES`]. Prompts carry this so a step line does not repeat
    /// the one before it and the wrap-up does not recite the whole turn.
    ///
    /// Recorded when a line is *queued* rather than when it is spoken: a
    /// prompt built while the queue drains has to know about lines that are
    /// about to sound, or every step in a burst reads as the first one.
    recent: std::collections::VecDeque<String>,
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
        let continuing = self.last_tool_kind == Some(kind);
        self.last_tool_label = Some(source.clone());
        self.last_tool_kind = Some(kind);
        let narration = match generated_phrase(&source, kind, continuing) {
            Some(phrase) => {
                self.remember(&phrase);
                Narration {
                    spoken: cx.new(|cx| Markdown::new(phrase.into(), None, None, cx)),
                    wash: vec![label],
                    progress: Some(Progress { kind, count: 1 }),
                }
            }
            None => {
                self.remember(&source);
                Narration {
                    spoken: label,
                    wash: Vec::new(),
                    progress: Some(Progress { kind, count: 1 }),
                }
            }
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
        self.last_tool_kind = None;
        self.remember(&text.read(cx).source().clone());
        self.pending.push(Narration {
            spoken: text,
            wash: Vec::new(),
            progress: None,
        });
        self.collapse_backlog(cx);
        self.enforce_cap();
    }

    /// Queues text that exists nowhere on screen (a model summary, a step
    /// line, a turn wrap-up, or a message's opening sentences), alongside the
    /// blocks to wash so the listener can find what it is about.
    pub fn push_summary(
        &mut self,
        spoken: Entity<Markdown>,
        message: Vec<Entity<Markdown>>,
        cx: &mut App,
    ) {
        self.last_tool_label = None;
        self.last_tool_kind = None;
        self.remember(&spoken.read(cx).source().clone());
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
        self.last_tool_kind = None;
    }

    /// The lines narration has recently committed to saying, oldest first.
    pub fn recent_lines(&self) -> Vec<String> {
        self.recent.iter().cloned().collect()
    }

    /// Drops the narrator's memory of what it has said. Called when the
    /// story restarts — a new turn, a different thread, a mode switch —
    /// because "do not repeat this" is only useful about the same story.
    ///
    /// Deliberately separate from [`Self::clear`]: dropping stale queued
    /// status at the end of a turn must not also make the wrap-up forget
    /// what it is not supposed to repeat.
    pub fn forget_recent(&mut self) {
        self.recent.clear();
    }

    fn remember(&mut self, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        while self.recent.len() >= RECENT_LINES {
            self.recent.pop_front();
        }
        self.recent.push_back(line.to_string());
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
/// its own. `None` means the label speaks for itself and is spoken, and
/// highlighted, in place.
///
/// Four kinds need a phrase of their own:
///
/// * **Read** — the label is `Read file \`path\` (lines 3-9)`. Read out it
///   is "read file player (lines 3-9)": the verb is past tense, the line
///   range is noise, and after a burst it chants.
/// * **Edit** — the label is nothing but a markdown-escaped file path, with
///   no verb at all, and reads out one path component at a time.
/// * **Execute** — the label is the raw command, and `acp_thread` hands it
///   over as plain text (links only), which the segmenter produces *no*
///   utterances from. Reciting a whole command line reads badly besides
///   ("cd slash users slash…"), so only the program and its most meaningful
///   argument are spoken.
/// * **Fetch** — the label is `Fetch <url>` with the URL markdown-escaped,
///   and the escapes defeat the URL substitute ("Fetch example_b").
///
/// Search, Move, Delete and the rest already read as verb-led prose
/// ("Rename thread view to conversation view") and are left in place, where
/// they keep their sentence and word highlighting.
///
/// Everything a phrase names goes in as a code span, so the substitute rules
/// that already turn `player.rs` into "player" and a URL into its host do
/// the work — no new speech logic, and the terse form is the point.
///
/// `continuing` means the previous narration was a tool call of this same
/// kind. The verb is elided then ("Reading player. Then segmenter."), which
/// is what keeps a run of reads from chanting.
fn generated_phrase(label: &str, kind: NarrationKind, continuing: bool) -> Option<String> {
    let label = label.trim();
    if label.is_empty() {
        return None;
    }
    let lead = |target: &str| {
        if continuing {
            format!("Then `{target}`.")
        } else {
            format!("{} `{target}`.", kind.verb())
        }
    };
    match kind {
        NarrationKind::Execute => Some(match spoken_command(label) {
            Some(command) => lead(&command),
            // Silence is the one thing this must never be: an Execute label
            // reaches the segmenter as plain text and says nothing at all.
            None => "Running a command.".to_string(),
        }),
        NarrationKind::Read | NarrationKind::Edit => Some(match label_path(label) {
            Some(path) => lead(&path),
            // Only reachable from a placeholder label ("Read file"), which
            // reads as the bare past-tense fragment the listener complained
            // about. Saying less is better than saying that.
            None => format!("{} a file.", kind.verb()),
        }),
        NarrationKind::Fetch => label_url(label).map(|url| lead(&url)),
        _ => None,
    }
}

/// The path a Read or Edit label is about. Read labels carry it in a code
/// span (`Read file \`crates/read_aloud/src/player.rs\``); Edit labels *are*
/// the path, markdown-escaped. `None` when the label carries no path — a
/// placeholder that arrived before the tool's input finished streaming.
fn label_path(label: &str) -> Option<String> {
    if let Some(span) = code_span(label) {
        let span = span.trim();
        if !span.is_empty() && !span.contains(char::is_whitespace) {
            return Some(span.to_string());
        }
    }
    // No code span: either the label *is* the path (an edit title), or an
    // agent wrote it inline. Take the last path-like token either way,
    // rather than only accepting a label that is nothing else — a label
    // with a stray word in it should still name the file.
    unescape_markdown_punctuation(label)
        .split_whitespace()
        .rev()
        .find(|token| is_path_like(token))
        .map(|token| {
            token
                .trim_matches(|c: char| c == '"' || c == '\'')
                .to_string()
        })
        .filter(|token| !token.is_empty() && !token.contains('`'))
}

/// Whether a token names a file: it has a directory separator, or an
/// extension short enough to be one.
fn is_path_like(token: &str) -> bool {
    if token.contains(['/', '\\']) {
        return true;
    }
    match token.rsplit_once('.') {
        Some((stem, extension)) => {
            !stem.is_empty()
                && !extension.is_empty()
                && extension.len() <= 5
                && extension.chars().all(|c| c.is_ascii_alphanumeric())
        }
        None => false,
    }
}

/// The URL a Fetch label is about, with the markdown escaping undone so the
/// URL substitute recognizes it.
fn label_url(label: &str) -> Option<String> {
    let unescaped = unescape_markdown_punctuation(label);
    let url = unescaped
        .split_whitespace()
        .find(|token| token.contains("://"))?;
    (!url.contains('`')).then(|| url.to_string())
}

/// One tool call as a *prompt* should see it, which is not how it should be
/// spoken: the model gets more out of the real path or the real command than
/// out of the terse form the listener hears, and the prompts forbid it from
/// reading either out. Only the kinds whose label does not name its own
/// action get a verb added.
pub(crate) fn tool_call_description(label: &str, kind: NarrationKind) -> String {
    let label = label.trim();
    match kind {
        NarrationKind::Execute => format!("ran the command `{label}`"),
        NarrationKind::Edit => format!("edited {}", unescape_markdown_punctuation(label)),
        _ => label.to_string(),
    }
}

/// The content of a label's first inline code span.
fn code_span(label: &str) -> Option<&str> {
    let (_, after) = label.split_once('`')?;
    let (span, _) = after.split_once('`')?;
    (!span.trim().is_empty()).then(|| span.trim())
}

/// How many tokens of a command are worth saying. "cargo test", "npm run
/// build" — enough to name the job, short enough that a pipeline is not
/// recited.
const MAX_COMMAND_TOKENS: usize = 3;

/// The sayable part of a shell command: the program and its most meaningful
/// arguments, stopping at the first flag, quoted argument, or shell
/// operator. `cd … && cargo test -p read_aloud` becomes "cargo test";
/// `ps aux | grep zed` becomes "ps aux"; `grep -rn "x" crates/` becomes
/// "grep".
///
/// `None` when nothing sayable survives, which the caller turns into
/// "Running a command." rather than silence.
fn spoken_command(command: &str) -> Option<String> {
    let mut words = tokenize_command(strip_directory_change(command.trim()));
    // Environment assignments in front of the program name are setup, not
    // the job: `RUST_LOG=debug cargo test` is "cargo test".
    while words
        .first()
        .is_some_and(|first| is_environment_assignment(first))
    {
        words.remove(0);
    }
    let mut tokens: Vec<String> = Vec::new();
    for word in words {
        if tokens.len() >= MAX_COMMAND_TOKENS || !is_sayable_command_token(&word) {
            break;
        }
        match spoken_command_token(&word) {
            Some(token) => tokens.push(token),
            // A token that reduces to nothing (a bare `.`, a glob) is not
            // worth saying, but the program in front of it still is.
            None => break,
        }
    }
    (!tokens.is_empty()).then(|| tokens.join(" "))
}

/// Drops any number of `cd <somewhere> &&` (or `;`) prefixes, which are
/// scaffolding an agent adds and nobody wants read out.
fn strip_directory_change(command: &str) -> &str {
    let mut rest = command.trim();
    loop {
        let Some(after_cd) = rest
            .strip_prefix("cd ")
            .or_else(|| rest.strip_prefix("pushd "))
        else {
            return rest;
        };
        let Some(separator) = after_cd.find("&&").or_else(|| after_cd.find(';')) else {
            return rest;
        };
        let skip = if after_cd[separator..].starts_with("&&") {
            2
        } else {
            1
        };
        let Some(next) = after_cd.get(separator + skip..) else {
            return rest;
        };
        rest = next.trim();
    }
}

/// Splits a command on whitespace, keeping a quoted argument together as one
/// token (still quoted, so the caller can tell it apart from a bare word).
fn tokenize_command(command: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    for character in command.chars() {
        match quote {
            Some(open) => {
                current.push(character);
                if character == open {
                    quote = None;
                }
            }
            None if character == '"' || character == '\'' => {
                quote = Some(character);
                current.push(character);
            }
            None if character.is_whitespace() => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            None => current.push(character),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn is_environment_assignment(token: &str) -> bool {
    match token.split_once('=') {
        Some((name, _)) => {
            !name.is_empty()
                && name
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '_')
        }
        None => false,
    }
}

/// Whether a token is still part of naming the job. A flag, a quoted
/// argument, a shell operator, a redirection, a glob, or a variable all mean
/// the interesting part is over.
fn is_sayable_command_token(token: &str) -> bool {
    if token.starts_with('-') || token.starts_with('"') || token.starts_with('\'') {
        return false;
    }
    !token.contains([
        '|', '&', ';', '>', '<', '*', '$', '(', ')', '{', '}', '[', ']', '`', '\\',
    ])
}

/// One command token as a person would say it: a path reduced to its final
/// component, an extension dropped. `None` when nothing is left.
fn spoken_command_token(token: &str) -> Option<String> {
    let token = token.trim_start_matches("./");
    let component = token.rsplit('/').find(|piece| !piece.is_empty())?;
    let component = match component.rsplit_once('.') {
        Some((stem, extension))
            if !stem.is_empty()
                && !extension.is_empty()
                && extension.len() <= 4
                && extension.chars().all(|c| c.is_ascii_alphanumeric()) =>
        {
            stem
        }
        _ => component,
    };
    let sayable = component
        .chars()
        .any(|character| character.is_alphanumeric());
    sayable.then(|| component.to_string())
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

/// The already-said block every narration prompt carries, or an empty string
/// when there is nothing to avoid repeating. This is the single biggest
/// lever on whether a run of lines sounds like one person talking rather
/// than a log being read out.
fn already_said(recent: &[String]) -> String {
    if recent.is_empty() {
        return String::new();
    }
    let lines = recent
        .iter()
        .map(|line| format!("- {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!("You have already said this out loud, and must not repeat it:\n{lines}\n\n")
}

/// The prompt for one step — the prose the agent wrote plus the tool calls it
/// then made — condensed into the single line a colleague would say while
/// doing it.
///
/// The shape is deliberate: action first, reason second ("Checking the sync
/// design spec to see how the pipeline stages line up"), because ambient
/// status is only useful if the first two words already say what is
/// happening.
pub fn step_prompt(prose: &str, tool_lines: &[String], recent: &[String]) -> String {
    let prose = truncate_chars(prose.trim(), MAX_SUMMARY_INPUT_CHARS);
    let wrote = if prose.is_empty() {
        String::new()
    } else {
        format!("What it wrote:\n---\n{prose}\n---\n\n")
    };
    let did = if tool_lines.is_empty() {
        String::new()
    } else {
        format!("What it then did:\n{}\n\n", bulleted(tool_lines))
    };
    format!(
        "You are narrating a coding agent's work out loud, the way a colleague sitting \
         next to someone would talk them through what they are doing.\n\n\
         Say what the agent is doing right now and why.\n\n\
         Rules:\n\
         - One sentence, under twenty words, present tense.\n\
         - Start with the action, then the reason: \"Checking the sync design spec to see \
         how the pipeline stages line up.\"\n\
         - Spoken English only. No markdown, no code, no backticks, no command lines, no \
         file paths — name a file the way you would say it aloud (\"the sync design spec\", \
         not \"docs/sync_design.md\").\n\
         - No preamble, no sign-off, no quotes around the reply.\n\n\
         {already}{wrote}{did}\
         Reply with the one sentence and nothing else.",
        already = already_said(recent),
    )
}

/// The prompt for the turn wrap-up: what was done, what was found, and what
/// the listener has to act on.
///
/// `message` is the closing message *so far* — this is issued speculatively,
/// before the turn-end event, so that the audio is ready the instant the turn
/// ends rather than a round trip after it. The prompt says so, because a
/// model told it is seeing a partial message writes a wrap-up that still
/// stands up when the last sentence never arrives.
pub fn wrap_up_prompt(
    message: &str,
    activity: &[String],
    recent: &[String],
    still_streaming: bool,
) -> String {
    let message = truncate_chars(message.trim(), MAX_SUMMARY_INPUT_CHARS);
    let closing = if message.is_empty() {
        String::new()
    } else {
        let caveat = if still_streaming {
            " (it may still be being written; work with what is here)"
        } else {
            ""
        };
        format!("Its closing message{caveat}:\n---\n{message}\n---\n\n")
    };
    let did = if activity.is_empty() {
        String::new()
    } else {
        format!("What it did this turn:\n{}\n\n", bulleted(activity))
    };
    format!(
        "You are narrating a coding agent's work out loud to someone who is not looking at \
         the screen. The turn is finishing, so give them the wrap-up.\n\n\
         Say what was done, what was found or decided, and anything they have to act on.\n\n\
         Rules:\n\
         - Two or three short sentences. Two is usually enough.\n\
         - Spoken English only. No markdown, no code, no backticks, no command lines, no \
         lists, no file paths spelled out.\n\
         - Build on what you have already said rather than repeating it — \"that's done, \
         and the tests pass\", not the whole story again.\n\
         - No preamble, no sign-off, no quotes around the reply.\n\n\
         {already}{did}{closing}\
         Reply with the wrap-up and nothing else.",
        already = already_said(recent),
    )
}

fn bulleted(lines: &[String]) -> String {
    lines
        .iter()
        .map(|line| format!("- {line}"))
        .collect::<Vec<_>>()
        .join("\n")
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

/// Makes a step line speakable, or rejects it. Same cleaning as
/// [`clean_summary`] against a much tighter bound: a step line is one
/// sentence of ambient status, and a model that answers with a paragraph has
/// reintroduced the verbosity narration exists to escape — the templated
/// fallback says less and says it now.
pub fn clean_step_line(reply: &str) -> Option<String> {
    let cleaned = clean_summary(reply)?;
    if cleaned.chars().count() > MAX_STEP_LINE_CHARS {
        log::warn!(
            "read_aloud: the summary model answered a step with {} characters, past the \
             {MAX_STEP_LINE_CHARS} a status line may be; speaking the templated form instead",
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
            "Then `file_4.rs`.",
            "what the agent is doing right now is worth naming, and a run of \
             the same kind elides the verb rather than chanting it"
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

    #[test]
    fn a_step_line_that_is_really_a_paragraph_is_rejected() {
        assert_eq!(
            clean_step_line("Checking the sync design spec.").as_deref(),
            Some("Checking the sync design spec.")
        );
        // Between the step bound and the summary bound, so this exercises
        // the step's own cap rather than the one it inherits.
        let paragraph = "word ".repeat(MAX_STEP_LINE_CHARS / 4);
        assert!(paragraph.chars().count() > MAX_STEP_LINE_CHARS);
        assert!(paragraph.chars().count() < MAX_SUMMARY_CHARS);
        assert!(clean_summary(&paragraph).is_some());
        assert_eq!(clean_step_line(&paragraph), None);
    }

    #[test]
    fn the_step_prompt_asks_for_the_action_and_the_reason() {
        let prompt = step_prompt(
            "Now I need to see how the pipeline stages line up.",
            &["Read file `docs/sync-design.md`".to_string()],
            &["Reading the migration notes.".to_string()],
        );
        assert!(prompt.contains("Now I need to see how the pipeline stages line up."));
        assert!(prompt.contains("docs/sync-design.md"));
        assert!(prompt.contains("Reading the migration notes."));
        assert!(prompt.contains("already said"));
        assert!(prompt.contains("One sentence"));
        assert!(prompt.contains("then the reason"));
    }

    #[test]
    fn the_wrap_up_prompt_says_when_the_message_is_unfinished() {
        let streaming = wrap_up_prompt("The tests all pass now", &[], &[], true);
        assert!(streaming.contains("still be being written"));
        let finished = wrap_up_prompt("The tests all pass now.", &[], &[], false);
        assert!(!finished.contains("still be being written"));
    }

    #[test]
    fn the_wrap_up_prompt_carries_the_turn_and_forbids_repeating_it() {
        let prompt = wrap_up_prompt(
            "Everything is wired up.",
            &["ran the command `cargo test -p read_aloud`".to_string()],
            &["Running cargo test.".to_string()],
            false,
        );
        assert!(prompt.contains("cargo test -p read_aloud"));
        assert!(prompt.contains("Running cargo test."));
        assert!(prompt.contains("must not repeat"));
        assert!(prompt.contains("Build on what you have already said"));
    }

    #[test]
    fn a_prompt_with_nothing_said_yet_carries_no_empty_section() {
        let prompt = step_prompt("Some prose.", &[], &[]);
        assert!(!prompt.contains("already said"));
        assert!(!prompt.contains("What it then did"));
    }

    /// A run of the same kind elides its verb rather than chanting it. The
    /// listener hears "Reading player. Then segmenter.", which is how a
    /// person would say it.
    #[gpui::test]
    async fn a_run_of_the_same_kind_elides_the_verb(cx: &mut TestAppContext) {
        let mut queue = NarrationQueue::default();
        let first = markdown("Read file `crates/read_aloud/src/player.rs`", cx);
        let second = markdown("Read file `crates/read_aloud/src/segmenter.rs`", cx);
        let command = markdown("cargo test", cx);
        let third = markdown("Read file `crates/read_aloud/src/sink.rs`", cx);
        cx.run_until_parked();

        // Popped as they go, the way the player drains them: four queued at
        // once would collapse into a count instead.
        let mut spoken = Vec::new();
        for (label, kind) in [
            (first, NarrationKind::Read),
            (second, NarrationKind::Read),
            (command, NarrationKind::Execute),
            (third, NarrationKind::Read),
        ] {
            cx.update(|cx| queue.push_tool_call(label, kind, cx));
            let narration = queue.pop().expect("the call was queued");
            spoken.push(
                narration
                    .spoken
                    .read_with(cx, |markdown, _| markdown.source().to_string()),
            );
        }
        assert_eq!(
            spoken,
            vec![
                "Reading `crates/read_aloud/src/player.rs`.".to_string(),
                "Then `crates/read_aloud/src/segmenter.rs`.".to_string(),
                "Running `cargo test`.".to_string(),
                "Reading `crates/read_aloud/src/sink.rs`.".to_string(),
            ],
            "the verb comes back once something else has been said in between"
        );
    }

    #[gpui::test]
    async fn the_narrator_remembers_what_it_committed_to_saying(cx: &mut TestAppContext) {
        let mut queue = NarrationQueue::default();
        let label = markdown("Read file `crates/read_aloud/src/player.rs`", cx);
        let summary = markdown("It moved the poll loop onto a timer.", cx);
        let message = markdown("Long message body.", cx);
        cx.run_until_parked();

        cx.update(|cx| {
            queue.push_tool_call(label, NarrationKind::Read, cx);
            queue.push_summary(summary, vec![message], cx);
        });
        assert_eq!(
            queue.recent_lines(),
            vec![
                "Reading `crates/read_aloud/src/player.rs`.".to_string(),
                "It moved the poll loop onto a timer.".to_string(),
            ]
        );

        // Dropping stale status at the end of a turn must not also make the
        // wrap-up forget what it is not supposed to repeat.
        queue.clear();
        assert_eq!(queue.recent_lines().len(), 2);
        queue.forget_recent();
        assert!(queue.recent_lines().is_empty());
    }

    #[gpui::test]
    async fn the_narrators_memory_is_bounded(cx: &mut TestAppContext) {
        let mut queue = NarrationQueue::default();
        let labels: Vec<_> = (0..RECENT_LINES + 4)
            .map(|index| markdown(&format!("Read file `crates/a/file_{index}.rs`"), cx))
            .collect();
        cx.run_until_parked();
        cx.update(|cx| {
            for label in labels {
                queue.push_tool_call(label, NarrationKind::Read, cx);
            }
        });
        assert_eq!(queue.recent_lines().len(), RECENT_LINES);
    }
}
