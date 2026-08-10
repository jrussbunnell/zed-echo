use gpui::{App, AppContext as _, Entity, Task};
use markdown::Markdown;
use std::time::Duration;

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

/// Roughly how fast a wrap-up is spoken. Every duty-cycle number here is
/// this rate applied to a word count.
pub const SPOKEN_WORDS_PER_SECOND: f32 = 2.5;

/// The most a turn wrap-up may ever be, whatever its budget. Past this the
/// model has written an essay, and the message's own opening is a better use
/// of the listener's attention.
pub const MAX_WRAP_UP_CHARS: usize = LARGE_TURN_WORDS * CHARS_PER_SPOKEN_WORD;

/// What one English word costs in characters, its following space included.
/// Used to turn a character bound back into the honest thing it bounds:
/// seconds of speech.
const CHARS_PER_REAL_WORD: f32 = 6.0;

/// Slack between the word ceiling the prompt *asks* for and the character
/// bound a reply is *rejected* past. Deliberately loose: rejecting a reply
/// that hit its word target but used long words costs the listener the whole
/// wrap-up and falls back to the message's own opening, which is worse than
/// letting it run a few seconds over. Seven leaves about a sixth over the
/// real average, so the bound is a backstop against an essay rather than a
/// second word counter.
const CHARS_PER_SPOKEN_WORD: usize = 7;

/// How much of a wrap-up a turn has earned.
///
/// A wrap-up that is the same length whatever happened is wrong twice over:
/// a sign-off after two tool calls is a monologue, and the same sign-off
/// after twenty minutes and a dozen files leaves the listener with less than
/// they were owed. Both were reported, in that order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WrapUpBudget {
    /// How many sentences the prompt asks for, spelled the way it is said —
    /// a small model follows "two or three sentences" more reliably than a
    /// digit.
    sentences: &'static str,
    /// The word ceiling, spelled out for the same reason.
    words: &'static str,
    /// The same ceiling as a number. This is the honest measure of how long
    /// the listener is asked to stand still for, so it is what the
    /// duty-cycle test ratchets on.
    pub max_words: usize,
}

/// Below this many tool calls the turn did essentially one thing, the step
/// lines already covered it, and the sign-off only has to close it.
const BRIEF_TURN_TOOL_CALLS: usize = 3;
/// …and a turn this short is still in the listener's short-term memory.
const BRIEF_TURN: Duration = Duration::from_secs(60);
/// At this many tool calls, or this long, the listener has heard scattered
/// status over minutes and lost the thread. The wrap-up is the only thing
/// that reassembles it, and there is genuinely more to reassemble.
pub(crate) const LONG_TURN_TOOL_CALLS: usize = 10;
pub(crate) const LONG_TURN: Duration = Duration::from_secs(5 * 60);

/// The three ceilings, in words.
///
/// These are derived from what a *spoken* sentence in this register costs —
/// twelve to fourteen words — not from a character budget:
///
/// * **Small**, twenty-five words (~10 s). One or two sentences. This is the
///   fixed value the wrap-up had before it was tiered, so nothing that was
///   already the right size got longer.
/// * **Medium**, forty words (~16 s). Two or three sentences.
/// * **Large**, fifty-five words (~22 s). The brief asks for "three or four"
///   sentences for a turn that ran twenty minutes and touched a dozen files;
///   four sentences at thirteen words is fifty-two. This is a *ceiling*, not
///   a target — the prompt asks for three or four sentences, which normally
///   lands well under it.
///
/// Twenty-two seconds is still a long time to stand still at the one moment
/// the listener wants to act, so the large tier is deliberately the smallest
/// number that fits the four sentences the brief asked for.
const SMALL_TURN_WORDS: usize = 25;
const MEDIUM_TURN_WORDS: usize = 40;
const LARGE_TURN_WORDS: usize = 55;

impl WrapUpBudget {
    /// What a turn of this size has earned. Either measure can promote a
    /// turn on its own: a turn is big because it did a lot of things, or
    /// because one of them took ten minutes, and a listener who has been
    /// waiting either way wants more than "that's done".
    pub fn for_turn(tool_calls: usize, elapsed: Option<Duration>) -> Self {
        let long = elapsed.unwrap_or_default();
        if tool_calls >= LONG_TURN_TOOL_CALLS || long >= LONG_TURN {
            Self {
                sentences: "Three or four sentences",
                words: "fifty-five",
                max_words: LARGE_TURN_WORDS,
            }
        } else if tool_calls >= BRIEF_TURN_TOOL_CALLS || long >= BRIEF_TURN {
            Self {
                sentences: "Two or three sentences",
                words: "forty",
                max_words: MEDIUM_TURN_WORDS,
            }
        } else {
            Self {
                sentences: "One sentence, two at the most",
                words: "twenty-five",
                max_words: SMALL_TURN_WORDS,
            }
        }
    }

    /// What a reply is rejected past.
    pub fn max_chars(&self) -> usize {
        self.max_words * CHARS_PER_SPOKEN_WORD
    }

    /// How long a reply that hits the word ceiling takes to say. What the
    /// prompt is aiming at.
    pub fn asked_seconds(&self) -> f32 {
        self.max_words as f32 / SPOKEN_WORDS_PER_SECOND
    }

    /// How long the *longest reply this budget will actually accept* takes to
    /// say — the character bound, not the word ceiling, because the character
    /// bound is what is enforced.
    ///
    /// This is longer than [`Self::asked_seconds`] by the slack in
    /// [`CHARS_PER_SPOKEN_WORD`], and it is the honest number: a listener
    /// held for the worst case is held for this long, not for the ask.
    pub fn max_spoken_seconds(&self) -> f32 {
        self.max_chars() as f32 / CHARS_PER_REAL_WORD / SPOKEN_WORDS_PER_SECOND
    }
}

/// The most of one tool call's command or path a prompt is given. Enough to
/// recognise `cargo test -p read_aloud --lib` or a deep path; short enough
/// that two dozen of them cannot crowd out the message they are context for.
pub(crate) const MAX_ACTION_CHARS: usize = 200;

/// The most of an agent's stated purpose that is worth speaking. Unlike the
/// prompt-facing cap this one is measured in *seconds of audio*: 160
/// characters is around ten seconds, which is already a long time to hold a
/// listener on one tool call. The longest purpose in the captured session is
/// 51 characters.
const MAX_PURPOSE_CHARS: usize = 160;

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
    /// The verb narration puts in front of a target that has none of its own.
    fn verb(self) -> &'static str {
        match self {
            NarrationKind::Read => "Reading",
            NarrationKind::Edit => "Editing",
            NarrationKind::Delete => "Deleting",
            NarrationKind::Move => "Moving",
            NarrationKind::Search => "Searching for",
            NarrationKind::Execute => "Running",
            NarrationKind::Fetch => "Fetching",
            NarrationKind::Other => "Running",
        }
    }

    /// What narration says when it knows the kind of thing the agent is
    /// doing but nothing about what it is doing it to. Never silence: a
    /// tool call the listener is not told about is the failure mode this
    /// whole path exists to avoid.
    fn unnamed_phrase(self) -> &'static str {
        match self {
            NarrationKind::Read => "Reading a file.",
            NarrationKind::Edit => "Editing a file.",
            NarrationKind::Delete => "Deleting a file.",
            NarrationKind::Move => "Moving a file.",
            NarrationKind::Search => "Running a search.",
            NarrationKind::Execute => "Running a command.",
            NarrationKind::Fetch => "Fetching a page.",
            NarrationKind::Other => "Taking a step.",
        }
    }

    /// The past-tense verb a prompt's account of the turn uses.
    fn past_verb(self) -> &'static str {
        match self {
            NarrationKind::Read => "read",
            NarrationKind::Edit => "edited",
            NarrationKind::Delete => "deleted",
            NarrationKind::Move => "moved",
            NarrationKind::Search => "searched for",
            NarrationKind::Execute => "ran",
            NarrationKind::Fetch => "fetched",
            NarrationKind::Other => "did",
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

/// How a tool call ended, as far as a listener is concerned. A failure is
/// the single most important thing a supervising listener needs to hear, so
/// it is carried all the way into the turn's wrap-up prompt.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ToolCallOutcome {
    #[default]
    Pending,
    Succeeded,
    Failed,
}

/// Whether an agent's structured input for a tool call has arrived yet.
///
/// Deliberately three states rather than an `Option`, because the
/// distinction that decides whether narration may speak is between *no
/// `rawInput` field at all* and *a `rawInput` that arrived empty* —
/// `Option::is_some()` reports both of the last two as input, and that is the
/// bug that survived three rounds of fixes. In a captured Claude Code
/// session, six of twenty-eight tool calls opened with `rawInput: {}` and the
/// title "Terminal", filled the command in a later update, and carried
/// `status: "pending"` throughout; a present-but-empty container is the only
/// signal that separates them from the twenty-two that arrived complete.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ToolCallInput {
    /// The call carried no structured input at all. The agent may simply not
    /// send any, so this is *not* treated as "more is coming".
    #[default]
    Absent,
    /// Structured input arrived and was contentless.
    Empty,
    /// Structured input arrived carrying at least one field.
    Present,
}

/// What narration can read out of an agent's `rawInput`, plus whether that
/// input has arrived at all.
///
/// `rawInput` is the agent's own tool schema, so every key here is a
/// convention rather than a contract; the common spellings are probed and
/// anything unrecognised degrades to the title. The extraction lives in this
/// crate rather than in the caller so that a captured protocol payload can be
/// fed to it verbatim in a test.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RawToolInput {
    /// `description`: the agent's plain-English statement of what the call is
    /// for. See [`ToolCallFacts::purpose`].
    pub purpose: Option<String>,
    pub command: Option<String>,
    pub path: Option<String>,
    pub url: Option<String>,
    pub query: Option<String>,
    pub input: ToolCallInput,
}

impl RawToolInput {
    pub fn from_json(raw_input: Option<&serde_json::Value>) -> Self {
        /// The keys an agent might put a shell command under.
        const COMMAND_KEYS: &[&str] = &["command", "cmd", "script", "shell_command"];
        /// …a file path under. `path` is Zed's own edit and read tools;
        /// `file_path` is Claude Code's.
        const PATH_KEYS: &[&str] = &["file_path", "path", "abs_path", "absolute_path", "filename"];
        const URL_KEYS: &[&str] = &["url", "uri"];
        /// …a search's subject under. `regex` is Zed's grep tool; `pattern`
        /// is Claude Code's Grep and Glob.
        const QUERY_KEYS: &[&str] = &["pattern", "regex", "query", "glob"];
        /// …a statement of intent under.
        const PURPOSE_KEYS: &[&str] = &["description", "purpose", "intent", "explanation"];

        let object = raw_input.and_then(serde_json::Value::as_object);
        let input = match (raw_input, object) {
            (None, _) | (Some(serde_json::Value::Null), _) => ToolCallInput::Absent,
            (Some(_), Some(object)) if object.is_empty() => ToolCallInput::Empty,
            (Some(_), _) => ToolCallInput::Present,
        };
        let field = |keys: &[&str]| -> Option<String> {
            let object = object?;
            keys.iter().find_map(|key| {
                object
                    .get(*key)
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
            })
        };
        Self {
            purpose: field(PURPOSE_KEYS),
            command: field(COMMAND_KEYS),
            path: field(PATH_KEYS),
            url: field(URL_KEYS),
            query: field(QUERY_KEYS),
            input,
        }
    }
}

/// What narration knows about one tool call.
///
/// The title is deliberately the *last* source consulted. Zed's own tools
/// put the command or the path straight into the title, but an external ACP
/// agent titles its own calls and those titles are generic — Claude Code
/// sends "Terminal" and "Read file" and carries the real content only in
/// `raw_input` and `locations`. Narration built on titles therefore said
/// "running terminal" once and then went silent, because every subsequent
/// call had the same title. Structured fields are protocol data and mean the
/// same thing whoever sent them; a title is a display string.
#[derive(Clone)]
pub struct ToolCallFacts {
    /// The agent's own id for the call, so a later status update can find
    /// the account of it this produced.
    pub id: String,
    pub kind: NarrationKind,
    /// The agent's own title. Still what is spoken for the kinds whose
    /// titles read as prose, and the fallback for every kind.
    pub label: Entity<Markdown>,
    /// The agent's own one-line statement of what this call is *for*, from
    /// `rawInput.description` — "Search almanac for launch readiness" rather
    /// than `codealmanac search "launch readiness" --limit 10 2>&1 | head`.
    ///
    /// This outranks every other source because it is the only one that
    /// describes intent instead of mechanism, and it is the thing a listener
    /// who cannot see the screen actually needs. It costs no model call and
    /// adds no latency: it arrives in the same payload as the command.
    pub purpose: Option<String>,
    /// The command an Execute call runs.
    pub command: Option<String>,
    /// The file a Read, Edit, Delete or Move call is about.
    pub path: Option<String>,
    /// The page a Fetch call retrieves.
    pub url: Option<String>,
    /// What a Search call is looking for.
    pub query: Option<String>,
    /// Whether the agent's structured input has arrived. See
    /// [`Self::awaiting_input`].
    pub input: ToolCallInput,
    pub outcome: ToolCallOutcome,
}

impl ToolCallFacts {
    /// A call with nothing structured behind it: what a caller that only has
    /// a title can supply.
    pub fn from_label(id: impl Into<String>, label: Entity<Markdown>, kind: NarrationKind) -> Self {
        Self {
            id: id.into(),
            kind,
            label,
            purpose: None,
            command: None,
            path: None,
            url: None,
            query: None,
            input: ToolCallInput::default(),
            outcome: ToolCallOutcome::default(),
        }
    }

    /// Whether this call has announced itself without yet saying anything
    /// about what it is doing — an empty payload that the agent has promised
    /// to fill.
    ///
    /// The caller must not narrate such a call on a timer: the whole point of
    /// the settle timer is to wait out a label that is still changing, and an
    /// empty payload's label does not change, so the timer expires on the
    /// placeholder and speaks it. That is "running terminal". Status is no
    /// help either — all twenty-eight calls in the capture, complete and
    /// empty alike, said `pending`.
    ///
    /// Only [`ToolCallInput::Empty`] waits. [`ToolCallInput::Absent`] means
    /// the agent sent no structured input at all, and waiting on input that
    /// was never promised would delay every such call to the end of the turn.
    pub fn awaiting_input(&self) -> bool {
        self.input == ToolCallInput::Empty
            && self.stated_purpose().is_none()
            && self.structured_target().is_none()
    }

    /// What narration's line for this call is *about* — the command, the
    /// path, the URL, the pattern, or (when nothing structured exists) the
    /// title itself.
    ///
    /// This is what duplicate suppression compares, and what the owning view
    /// watches for quiescence. Comparing titles is what made several distinct
    /// commands sharing one generic title collapse into a single utterance.
    pub fn spoken_key(&self, cx: &App) -> String {
        let label = self.label.read(cx).source().trim().to_string();
        match tool_call_target(
            self.kind,
            self.stated_purpose().as_deref(),
            self.structured_target(),
            &label,
        ) {
            ToolCallTarget::Stated(purpose) => purpose,
            ToolCallTarget::Named(target) => target,
            ToolCallTarget::Unnamed => self.kind.unnamed_phrase().to_string(),
            ToolCallTarget::Label => label,
        }
    }

    /// This call as a *prompt* should see it, which is not how it should be
    /// spoken: the model gets more out of the real path or the real command
    /// than out of the terse form the listener hears, and the prompts forbid
    /// it from reading either out.
    ///
    /// Bounded, because this is agent-controlled text now that structured
    /// input is preferred over the title. A generic title was short by
    /// definition; a `raw_input` command is whatever the agent felt like
    /// running, and Claude Code's Bash tool routinely carries multi-line
    /// heredoc scripts. Two dozen of those would be tens of kilobytes of
    /// prompt on a path that has a two-second race to lose.
    pub(crate) fn description(&self, cx: &App) -> String {
        let label = self.label.read(cx).source().trim().to_string();
        // The agent's own statement of purpose is strictly better prompt
        // input than the command that implements it: it is shorter, it is
        // about intent, and it does not spend the prompt on a heredoc. For
        // the file-shaped kinds the file is still the better fact, so a
        // purpose only stands in where there is no structured target at all —
        // the same precedence [`tool_call_target`] uses.
        let purpose = self
            .stated_purpose()
            .filter(|_| self.kind == NarrationKind::Execute || self.structured_target().is_none());
        if let Some(purpose) = purpose {
            let described = match self.kind {
                NarrationKind::Execute => format!("ran a command — {purpose}"),
                kind => format!("{} something — {purpose}", kind.past_verb()),
            };
            return match self.outcome {
                ToolCallOutcome::Failed => format!("{described} — it FAILED"),
                ToolCallOutcome::Pending | ToolCallOutcome::Succeeded => described,
            };
        }
        let target = self
            .structured_target()
            .map(|target| truncate_chars(target, MAX_ACTION_CHARS));
        let described = match (self.kind, target) {
            (NarrationKind::Execute, Some(command)) => format!("ran the command `{command}`"),
            (NarrationKind::Search, Some(query)) => format!("searched for `{query}`"),
            (kind, Some(target)) => format!("{} {target}", kind.past_verb()),
            (NarrationKind::Execute, None) if !label.is_empty() => {
                format!(
                    "ran the command `{}`",
                    truncate_chars(&label, MAX_ACTION_CHARS)
                )
            }
            (NarrationKind::Edit, None) if !label.is_empty() => format!(
                "edited {}",
                truncate_chars(&unescape_markdown_punctuation(&label), MAX_ACTION_CHARS)
            ),
            (_, None) if !label.is_empty() => truncate_chars(&label, MAX_ACTION_CHARS).to_string(),
            (kind, None) => format!("{} something", kind.past_verb()),
        };
        match self.outcome {
            ToolCallOutcome::Failed => format!("{described} — it FAILED"),
            ToolCallOutcome::Pending | ToolCallOutcome::Succeeded => described,
        }
    }

    /// What duplicate suppression compares — which is what the listener will
    /// actually *hear*, not what the line is derived from.
    ///
    /// For a file, that is the spoken form of the path: `a/foo.rs` and
    /// `b/foo.rs` are different files but both come out as "Reading foo", and
    /// hearing "Reading foo. Then foo." tells nobody anything. The owning
    /// view deliberately keeps watching [`Self::spoken_key`] instead, because
    /// quiescence has to notice a streaming path growing even while its final
    /// component stands still.
    pub(crate) fn heard_key(&self, cx: &App) -> String {
        let key = self.spoken_key(cx);
        if !matches!(
            self.kind,
            NarrationKind::Read | NarrationKind::Edit | NarrationKind::Delete | NarrationKind::Move
        ) {
            return key;
        }
        crate::segmenter::spoken_path_component(&key).unwrap_or(key)
    }

    /// [`Self::purpose`] in a state fit to be spoken: whitespace collapsed to
    /// single spaces, trailing sentence punctuation removed so a period can be
    /// added back uniformly, and bounded.
    ///
    /// Bounded because this is agent-controlled prose on a path with a
    /// two-second budget. The longest description in the captured session is
    /// 51 characters, so the cap is headroom rather than a working limit;
    /// anything past it is a runaway that would be read aloud in full.
    /// A purpose with no letters or digits in it is not a sentence, and
    /// speaking it produces no audio at all while still counting as the
    /// call's line — so the call goes unheard *and* takes its neighbours with
    /// it, because duplicate suppression compares the same empty string every
    /// time. Such a purpose is discarded and the ordinary resolution runs.
    fn stated_purpose(&self) -> Option<String> {
        let purpose = self.purpose.as_deref()?;
        let collapsed = purpose.split_whitespace().collect::<Vec<_>>().join(" ");
        let trimmed = collapsed.trim_end_matches(['.', '!', ';', ',', ' ']);
        if !trimmed.chars().any(char::is_alphanumeric) {
            return None;
        }
        Some(truncate_chars(trimmed, MAX_PURPOSE_CHARS).to_string())
    }

    /// The structured field this kind cares about, trimmed and non-empty.
    fn structured_target(&self) -> Option<&str> {
        let field = match self.kind {
            NarrationKind::Execute => &self.command,
            NarrationKind::Read
            | NarrationKind::Edit
            | NarrationKind::Delete
            | NarrationKind::Move => &self.path,
            NarrationKind::Fetch => &self.url,
            NarrationKind::Search => &self.query,
            NarrationKind::Other => &None,
        };
        field.as_deref().map(str::trim).filter(|it| !it.is_empty())
    }
}

/// Resolution order for an Execute call: the agent's stated purpose first,
/// its command second, its own title third, the kind's templated phrase last.
/// For every other kind, structured input first, the title second, the
/// templated phrase last — with the stated purpose displacing the templated
/// phrase, since anything the agent wrote beats a phrase that names nothing.
///
/// Purpose leads for Execute because a command is *mechanism*: "Running grep"
/// tells a listener who cannot see the screen almost nothing, while "Searching
/// almanac for launch readiness" is the whole point of the call. It does not
/// lead for the file-shaped kinds, where the file is the point and the agent
/// sends no description anyway.
///
/// **Every** kind ends at the templated phrase, never at nothing. A kind that
/// falls through to a title it does not have is the reported bug all over
/// again in a different costume: an agent whose search tool puts its subject
/// under a key nothing here recognises (`{"searchTerm": …}`) issues three
/// calls all titled "Search", and comparing those titles suppresses two of
/// them. The templated phrase is a poor line, but it is a line, and it is the
/// same one for every such call so the *first* is still spoken.
fn tool_call_target(
    kind: NarrationKind,
    purpose: Option<&str>,
    structured: Option<&str>,
    label: &str,
) -> ToolCallTarget {
    // A title only speaks for itself if there is one.
    let titled = || {
        if label.trim().is_empty() {
            ToolCallTarget::Unnamed
        } else {
            ToolCallTarget::Label
        }
    };
    let stated = || purpose.map(|purpose| ToolCallTarget::Stated(purpose.to_string()));
    let resolved = match kind {
        NarrationKind::Execute => {
            return stated().unwrap_or_else(|| {
                structured
                    .and_then(spoken_command)
                    .or_else(|| spoken_command(label))
                    .map_or(ToolCallTarget::Unnamed, ToolCallTarget::Named)
            });
        }
        NarrationKind::Read | NarrationKind::Edit => structured
            .map(str::to_string)
            .or_else(|| label_path(label))
            .map_or(ToolCallTarget::Unnamed, ToolCallTarget::Named),
        // Zed's own delete and move titles are already verb-led prose
        // ("Rename thread view to conversation view") and read better than
        // anything narration would generate, so the title keeps precedence
        // over nothing — only structured input displaces it.
        NarrationKind::Delete | NarrationKind::Move | NarrationKind::Search => {
            structured.map_or_else(titled, |target| ToolCallTarget::Named(target.to_string()))
        }
        NarrationKind::Fetch => structured
            .map(str::to_string)
            .or_else(|| label_url(label))
            .map_or_else(titled, ToolCallTarget::Named),
        NarrationKind::Other => titled(),
    };
    match resolved {
        ToolCallTarget::Unnamed => stated().unwrap_or(ToolCallTarget::Unnamed),
        resolved => resolved,
    }
}

/// What narration's line for a call is about, before a verb goes in front.
enum ToolCallTarget {
    /// The agent said what the call is for, in its own words. That sentence
    /// *is* the line — no verb goes in front of it and it is not a code span.
    Stated(String),
    /// A concrete thing: the command, the path, the URL, the pattern.
    Named(String),
    /// Nothing concrete could be resolved, and the title does not read as
    /// prose either, so the kind's templated phrase is all there is.
    Unnamed,
    /// The agent's own title reads well enough to be spoken — and
    /// highlighted — in place.
    Label,
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
/// 1. A tool call about the same thing as the previous one is dropped.
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
    /// What the last tool call accepted was — its kind, and what it was
    /// *about* — whether it was queued or spoken immediately, so back-to-back
    /// duplicates are suppressed across the boundary between the queue and
    /// the player.
    ///
    /// The resolved target, never the title: an external agent gives every
    /// terminal command the title "Terminal", and comparing titles turned a
    /// run of different commands into one utterance followed by silence.
    ///
    /// The kind is part of the comparison because doing two *different
    /// things* to files that sound alike is two things to say. Reading
    /// `crates/a/Cargo.toml` and then editing `crates/b/Cargo.toml` both
    /// reduce to "Cargo", and comparing the target alone silently dropped
    /// the edit — a real action going unspoken, which is the original
    /// complaint in miniature.
    last_tool_key: Option<(NarrationKind, String)>,
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
    /// Queues one tool call. Returns whether anything was queued: a call
    /// about the same thing as the one before it adds nothing to a listener.
    pub fn push_tool_call(&mut self, facts: ToolCallFacts, cx: &mut App) -> bool {
        let label = facts.label.read(cx).source().trim().to_string();
        let kind = facts.kind;
        let key = facts.heard_key(cx);
        if key.trim().is_empty()
            || self
                .last_tool_key
                .as_ref()
                .is_some_and(|(last_kind, last_key)| *last_kind == kind && *last_key == key)
        {
            return false;
        }
        let continuing = self.last_tool_kind == Some(kind);
        self.last_tool_key = Some((kind, key));
        self.last_tool_kind = Some(kind);
        let narration = match generated_phrase(
            kind,
            facts.stated_purpose().as_deref(),
            facts.structured_target(),
            &label,
            continuing,
        ) {
            Some(phrase) => {
                self.remember(&phrase);
                Narration {
                    spoken: cx.new(|cx| Markdown::new(phrase.into(), None, None, cx)),
                    wash: vec![facts.label],
                    progress: Some(Progress { kind, count: 1 }),
                }
            }
            None => {
                self.remember(&label);
                Narration {
                    spoken: facts.label,
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
        self.last_tool_key = None;
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
        self.last_tool_key = None;
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
        self.last_tool_key = None;
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

/// The phrase to speak for one tool call. `None` means the agent's own title
/// speaks for itself and is spoken, and highlighted, in place.
///
/// What is spoken comes from [`ToolCallFacts::target`], which prefers the
/// call's structured input over its title. The title is only consulted for
/// the kinds whose titles Zed's own tools fill in usefully:
///
/// * **Read** — the title is `Read file \`path\` (lines 3-9)`. Read out it
///   is "read file player (lines 3-9)": the verb is past tense, the line
///   range is noise, and after a burst it chants.
/// * **Edit** — the title is nothing but a markdown-escaped file path, with
///   no verb at all, and reads out one path component at a time.
/// * **Execute** — the title is the raw command, and `acp_thread` hands it
///   over as plain text (links only), which the segmenter produces *no*
///   utterances from. Reciting a whole command line reads badly besides
///   ("cd slash users slash…"), so only the program and its most meaningful
///   argument are spoken.
/// * **Fetch** — the title is `Fetch <url>` with the URL markdown-escaped,
///   and the escapes defeat the URL substitute ("Fetch example_b").
///
/// Move and Delete already read as verb-led prose ("Rename thread view to
/// conversation view") and are left in place unless structured input names
/// the file, where they keep their sentence and word highlighting.
///
/// Everything a phrase names goes in as a code span, so the substitute rules
/// that already turn `player.rs` into "player" and a URL into its host do
/// the work — no new speech logic, and the terse form is the point.
///
/// `continuing` means the previous narration was a tool call of this same
/// kind. The verb is elided then ("Reading player. Then segmenter."), which
/// is what keeps a run of reads from chanting.
fn generated_phrase(
    kind: NarrationKind,
    purpose: Option<&str>,
    structured: Option<&str>,
    label: &str,
    continuing: bool,
) -> Option<String> {
    let lead = |target: &str| {
        if continuing {
            format!("Then `{target}`.")
        } else {
            format!("{} `{target}`.", kind.verb())
        }
    };
    match tool_call_target(kind, purpose, structured, label) {
        ToolCallTarget::Stated(purpose) => Some(format!("{}.", spoken_purpose(&purpose))),
        ToolCallTarget::Named(target) => Some(lead(&target)),
        // Silence is the one thing this must never be: a listener told
        // nothing cannot tell a quiet agent from a broken feature.
        ToolCallTarget::Unnamed => Some(kind.unnamed_phrase().to_string()),
        ToolCallTarget::Label => None,
    }
}

/// An agent's stated purpose as a narration line.
///
/// Agents write these in the imperative ("Search almanac for launch
/// readiness") or as a bare noun phrase ("Recent commits and unpushed
/// count") — nineteen and seven respectively across the twenty-six in the
/// captured session. Read out verbatim, the imperative ones sound like
/// instructions *to the listener*, which is exactly backwards in an ear-only
/// mode whose whole job is reporting what the agent is doing. So the leading
/// verb is put into the present participle: "Searching almanac for launch
/// readiness."
///
/// The conversion is a lookup, not morphology. English participle spelling
/// (drop the `e`, double the consonant, `-ie` becomes `-ying`) has enough
/// exceptions that a rule would eventually mangle a word in the user's ear,
/// and a mangled word is worse than a slightly stiff sentence. A first word
/// that is not in the table — every noun phrase, and any verb not measured —
/// is spoken exactly as written.
fn spoken_purpose(purpose: &str) -> String {
    /// Imperative-to-participle pairs. Every entry is a verb measured in the
    /// captured session or one of the handful of shell-task verbs an agent
    /// reaches for constantly. Extending it is safe; the fallback is
    /// verbatim.
    const PARTICIPLES: &[(&str, &str)] = &[
        ("add", "Adding"),
        ("build", "Building"),
        ("check", "Checking"),
        ("collect", "Collecting"),
        ("compare", "Comparing"),
        ("compute", "Computing"),
        ("confirm", "Confirming"),
        ("count", "Counting"),
        ("create", "Creating"),
        ("delete", "Deleting"),
        ("extract", "Extracting"),
        ("fetch", "Fetching"),
        ("find", "Finding"),
        ("gather", "Gathering"),
        ("generate", "Generating"),
        ("get", "Getting"),
        ("inspect", "Inspecting"),
        ("install", "Installing"),
        ("list", "Listing"),
        ("look", "Looking"),
        ("measure", "Measuring"),
        ("move", "Moving"),
        ("print", "Printing"),
        ("read", "Reading"),
        ("remove", "Removing"),
        ("rename", "Renaming"),
        ("run", "Running"),
        ("scan", "Scanning"),
        ("search", "Searching"),
        ("show", "Showing"),
        ("test", "Testing"),
        ("update", "Updating"),
        ("verify", "Verifying"),
        ("write", "Writing"),
    ];
    let Some((first, rest)) = purpose.split_once(' ') else {
        // A one-word purpose is a label, not a sentence; converting it would
        // read worse than leaving it ("Cleanup." not "Cleaning up.").
        return purpose.to_string();
    };
    let lowercased = first.to_lowercase();
    match PARTICIPLES
        .iter()
        .find(|(imperative, _)| *imperative == lowercased)
    {
        Some((_, participle)) => format!("{participle} {rest}"),
        None => purpose.to_string(),
    }
}

/// The path a Read or Edit label is about. Read labels carry it in a code
/// span (`Read file \`crates/read_aloud/src/player.rs\``); Edit labels *are*
/// the path, markdown-escaped. `None` when the label carries no path — a
/// placeholder that arrived before the tool's input finished streaming.
fn label_path(label: &str) -> Option<String> {
    if let Some(span) = code_span(label) {
        // Unescaped like the bare-path branch: a backslash surviving into
        // the code span would be spoken rather than ignored.
        let span = unescape_markdown_punctuation(span.trim());
        if !span.is_empty() && !span.contains(char::is_whitespace) {
            return Some(span);
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
                .trim_matches(|character: char| character == '"' || character == '\'')
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
                && extension
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric())
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
        // The *earliest* separator ends the `cd`, not whichever kind is
        // searched for first: `cd /x; ls && grep` runs `ls`, not `grep`.
        let separator = match (after_cd.find("&&"), after_cd.find(';')) {
            (Some(and), Some(semicolon)) => and.min(semicolon),
            (Some(only), None) | (None, Some(only)) => only,
            (None, None) => return rest,
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
                && extension
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric()) =>
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
         looking at the screen. Your reply is spoken by a text-to-speech voice, so it \
         must read as something a person would say.\n\n\
         Summarize what the agent did and what it decided in the message below.\n\n\
         Rules:\n\
         - At most two sentences and thirty-five words. Use one sentence if one will do.\n\
         - Say what changed and what was chosen, not how it was written.\n\
         - If anything failed or is still broken, say that first.\n\
         - Spoken English. No markdown, no backticks, no code, no command lines, no \
         lists, no headings.\n\
         - Name a file the way you would say it aloud (\"the segmenter\", not \
         \"crates/read_aloud/src/segmenter.rs\"), and only when the file is the point.\n\
         - Start with the substance. Never open with \"Here's\", \"This message\", \
         \"The agent\", \"In summary\", or a restatement of the question.\n\
         - Reply with the summary alone: no preamble, no sign-off, no quotation marks \
         around it.\n\n\
         Good reply: \"It moved the poll loop onto a timer and left the stop latch \
         alone.\"\n\
         Bad reply: \"Here's a summary: the agent has updated `player.rs`...\"\n\n\
         Message:\n\
         ---\n\
         {message}\n\
         ---\n\n\
         Reply with the summary and nothing else."
    )
}

/// What a model is told to reply when the step's prose has already said
/// everything worth saying.
pub const NOTHING_TO_ADD: &str = "nothing to add";

/// Words too common to count as content when deciding whether one line
/// merely restates another.
const FILLER_WORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "before", "but", "by", "for", "from", "how", "i",
    "if", "in", "into", "is", "it", "its", "let", "me", "of", "on", "or", "so", "that", "the",
    "then", "there", "this", "to", "up", "was", "what", "when", "where", "which", "will", "with",
];

/// Whether `line` would only say again what `already_said` already said.
///
/// Two spoken sentences in a row that carry the same information is the
/// "choppy and repetitive" failure in its other form, and it is easy to hit
/// now that a step's own prose goes out before its fused line. The model is
/// asked to say so itself ([`NOTHING_TO_ADD`]); this is the backstop for
/// when it does not, and it is deliberately conservative — a line is only
/// dropped when *almost all* of its content words were already spoken.
pub fn adds_nothing(line: &str, already_said: &str) -> bool {
    let content = |text: &str| -> Vec<String> {
        text.split(|character: char| !character.is_alphanumeric())
            .filter(|word| !word.is_empty())
            .map(str::to_lowercase)
            .filter(|word| word.len() > 2 && !FILLER_WORDS.contains(&word.as_str()))
            .collect()
    };
    let line_words = content(line);
    if line_words.is_empty() {
        return true;
    }
    if line
        .trim()
        .trim_end_matches(['.', '!'])
        .eq_ignore_ascii_case(NOTHING_TO_ADD)
    {
        return true;
    }
    let said = content(already_said);
    let repeated = line_words.iter().filter(|word| said.contains(word)).count();
    // Three quarters, rather than all of it: a line that adds one new word
    // to a sentence the listener just heard is still a repetition.
    repeated * 4 >= line_words.len() * 3
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
         next to someone would talk them through what they are doing. Your reply is \
         spoken by a text-to-speech voice.\n\n\
         The agent's own words have usually just been read out already. Say what it is \
         actually *doing* now — the files, the commands — and why, without saying those \
         words again.\n\n\
         Rules:\n\
         - One sentence, under fifteen words, present tense, ending in a full stop.\n\
         - Start with the action, then the reason.\n\
         - Spoken English only. No markdown, no code, no backticks, no command lines, no \
         file paths — name a file the way you would say it aloud (\"the sync design spec\", \
         not \"docs/sync_design.md\").\n\
         - Never open with \"The agent\", \"It is\", \"Currently\", or \"Here\".\n\
         - If everything worth saying is already in what you have said out loud, reply \
         with exactly: {NOTHING_TO_ADD}\n\
         - No preamble, no sign-off, no quotes around the reply.\n\n\
         Good reply: \"Checking the sync design spec to see how the pipeline stages line \
         up.\"\n\
         Good reply: \"Running the read-aloud tests to see what the change broke.\"\n\
         Bad reply: \"The agent is currently reading `docs/sync_design.md`.\"\n\n\
         {already}{wrote}{did}\
         Reply with the one sentence and nothing else.",
        already = already_said(recent),
    )
}

/// Everything the turn's wrap-up is written from.
///
/// Deliberately more than the closing prose: what a supervising listener
/// needs is what actually *happened* — which commands ran, which files
/// changed, and above all whether anything failed. A wrap-up written from
/// the closing paragraph alone repeats a paragraph they are about to be able
/// to read, and an agent's closing paragraph is not reliably the place a
/// failure gets mentioned.
pub struct WrapUpMaterial<'a> {
    /// The closing message *so far*. The wrap-up is issued speculatively,
    /// before the turn-end event, so the audio is ready the instant the turn
    /// ends rather than a round trip after it.
    pub message: &'a str,
    /// Whether that message may still grow. The prompt says so, because a
    /// model told it is seeing a partial message writes a wrap-up that still
    /// stands up when the last sentence never arrives.
    pub still_streaming: bool,
    /// Every tool call of the turn, with its real command or path and
    /// whether it failed.
    pub activity: &'a [String],
    /// The files the turn changed, named once each.
    pub files_changed: &'a [String],
    /// What narration has already said out loud, so the wrap-up builds on it
    /// instead of reciting the turn again.
    pub recent: &'a [String],
    /// How much of a wrap-up this turn has earned.
    pub budget: WrapUpBudget,
}

/// The prompt for the turn wrap-up: what was done, what was found, and what
/// the listener has to act on.
pub fn wrap_up_prompt(material: WrapUpMaterial<'_>) -> String {
    let message = truncate_chars(material.message.trim(), MAX_SUMMARY_INPUT_CHARS);
    let closing = if message.is_empty() {
        String::new()
    } else {
        let caveat = if material.still_streaming {
            " (it may still be being written; work with what is here)"
        } else {
            ""
        };
        format!("Its closing message{caveat}:\n---\n{message}\n---\n\n")
    };
    let did = if material.activity.is_empty() {
        String::new()
    } else {
        format!(
            "What it did this turn, in order:\n{}\n\n",
            bulleted(material.activity)
        )
    };
    let changed = if material.files_changed.is_empty() {
        String::new()
    } else {
        format!(
            "Files it changed:\n{}\n\n",
            bulleted(material.files_changed)
        )
    };
    format!(
        "You are narrating a coding agent's work out loud to someone who is not looking at \
         the screen. The turn is finishing, so give them the wrap-up. Your reply is spoken \
         by a text-to-speech voice.\n\n\
         Say what was done, what was found or decided, and anything they have to act on.\n\n\
         Rules:\n\
         - {sentences}, and under {words} words in total.\n\
         - If anything failed — a command, a test, a build — say that first and say what \
         failed. It is the most important thing they need to hear.\n\
         - Otherwise lead with the outcome, then what it took.\n\
         - Spoken English only. No markdown, no code, no backticks, no command lines, no \
         lists, no file paths spelled out — name a file the way you would say it aloud.\n\
         - Build on what you have already said rather than repeating it — \"that's done, \
         and the tests pass\", not the whole story again.\n\
         - Never open with \"Here's\", \"In summary\", \"The agent\", or \"To wrap up\".\n\
         - No preamble, no sign-off, no quotes around the reply.\n\n\
         Good reply: \"The read-aloud tests fail — two of them, on the segmenter. \
         Everything else is wired up and building.\"\n\
         Bad reply: \"To wrap up, the agent has made changes to several files.\"\n\n\
         {already}{did}{changed}{closing}\
         Reply with the wrap-up and nothing else.",
        sentences = material.budget.sentences,
        words = material.budget.words,
        already = already_said(material.recent),
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

/// Makes a turn wrap-up speakable, or rejects it. Same cleaning as
/// [`clean_summary`] against the bound this turn's budget allows: a wrap-up
/// the listener has to sit through is the verbosity this mode exists to
/// escape, arriving at the one moment they are actually waiting.
pub fn clean_wrap_up(reply: &str, budget: WrapUpBudget) -> Option<String> {
    let cleaned = clean_summary(reply)?;
    if cleaned.chars().count() > budget.max_chars() {
        log::warn!(
            "read_aloud: the summary model answered the wrap-up with {} characters, past the {} \
             this turn's sign-off may be; speaking the message's opening instead",
            cleaned.chars().count(),
            budget.max_chars()
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

    /// A call with nothing but a title behind it — the Zed-native shape, and
    /// the shape every one of these queue tests exercised before external
    /// agents were taken into account.
    fn titled(label: Entity<Markdown>, kind: NarrationKind) -> ToolCallFacts {
        ToolCallFacts::from_label("call", label, kind)
    }

    #[gpui::test]
    async fn a_repeated_tool_label_is_not_said_twice(cx: &mut TestAppContext) {
        let mut queue = NarrationQueue::default();
        let first = markdown("Read player.rs", cx);
        let same = markdown("Read player.rs", cx);
        let other = markdown("Read segmenter.rs", cx);
        cx.run_until_parked();

        cx.update(|cx| {
            assert!(queue.push_tool_call(titled(first, NarrationKind::Read), cx));
            assert!(
                !queue.push_tool_call(titled(same, NarrationKind::Read), cx),
                "an identical label back to back tells the listener nothing new"
            );
            assert!(queue.push_tool_call(titled(other, NarrationKind::Read), cx));
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
                queue.push_tool_call(titled(label, NarrationKind::Read), cx);
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
                queue.push_tool_call(titled(label, NarrationKind::Read), cx);
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
                queue.push_tool_call(titled(label, kind), cx);
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
                queue.push_tool_call(titled(label, NarrationKind::Read), cx);
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
                queue.push_tool_call(titled(label, NarrationKind::Read), cx);
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
                queue.push_tool_call(titled(label, NarrationKind::Read), cx);
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

    /// The budget a small turn gets, so the shortest bound is the one under
    /// test.
    fn brief_budget() -> WrapUpBudget {
        WrapUpBudget::for_turn(1, None)
    }

    #[test]
    fn a_wrap_up_that_is_really_a_speech_is_rejected() {
        let budget = brief_budget();
        let sign_off = "That is the bug. The fix is a flush on shutdown, and that is your call.";
        assert_eq!(clean_wrap_up(sign_off, budget).as_deref(), Some(sign_off));
        let speech = "word ".repeat(budget.max_chars() / 4);
        assert!(speech.chars().count() > budget.max_chars());
        assert!(speech.chars().count() < MAX_SUMMARY_CHARS);
        assert!(
            clean_summary(&speech).is_some(),
            "this exercises the wrap-up's own bound, not the one it inherits"
        );
        assert_eq!(clean_wrap_up(&speech, budget), None);
    }

    /// The user reported both failures in turn: a fifteen-second monologue
    /// after a small turn, and then a sign-off that was "not sufficient"
    /// after a large one. A single fixed length cannot be right for both.
    #[test]
    fn the_wrap_up_budget_scales_with_the_turn() {
        let brief = WrapUpBudget::for_turn(2, Some(Duration::from_secs(20)));
        let standard = WrapUpBudget::for_turn(5, Some(Duration::from_secs(90)));
        let full = WrapUpBudget::for_turn(14, Some(Duration::from_secs(90)));
        assert!(brief.max_chars() < standard.max_chars());
        assert!(standard.max_chars() < full.max_chars());
        assert_eq!(full.max_chars(), MAX_WRAP_UP_CHARS);

        // Either measure promotes on its own: a turn can be big because it
        // did a lot, or because one thing in it took a long time.
        assert_eq!(
            WrapUpBudget::for_turn(2, Some(Duration::from_secs(20 * 60))),
            full,
            "a twenty-minute turn earns a full wrap-up however few calls it made"
        );
        assert_eq!(
            WrapUpBudget::for_turn(20, None),
            full,
            "and so does a turn that touched twenty things in a minute"
        );

        // A reply that fits a large turn is rejected for a small one.
        let paragraph = "word ".repeat(50);
        assert!(paragraph.chars().count() > brief.max_chars());
        assert!(paragraph.chars().count() < full.max_chars());
        assert!(clean_wrap_up(&paragraph, full).is_some());
        assert_eq!(clean_wrap_up(&paragraph, brief), None);
    }

    /// Structured input beats the title for every kind that has one, and the
    /// title is still there when it does not.
    #[test]
    fn structured_input_names_what_a_generic_title_cannot() {
        for (kind, structured, label, expected) in [
            (
                NarrationKind::Execute,
                Some("cd /repo && cargo test -p read_aloud"),
                "Terminal",
                Some("Running `cargo test`."),
            ),
            (
                NarrationKind::Read,
                Some("/repo/crates/read_aloud/src/player.rs"),
                "Read file",
                Some("Reading `/repo/crates/read_aloud/src/player.rs`."),
            ),
            (
                NarrationKind::Edit,
                Some("/repo/crates/read_aloud/src/sink.rs"),
                "Update",
                Some("Editing `/repo/crates/read_aloud/src/sink.rs`."),
            ),
            (
                NarrationKind::Search,
                Some("TODO"),
                "Grep",
                Some("Searching for `TODO`."),
            ),
            (
                NarrationKind::Fetch,
                Some("https://docs.inworld.ai/tts"),
                "Fetch",
                Some("Fetching `https://docs.inworld.ai/tts`."),
            ),
            // Nothing structured, and a title with nothing in it either: the
            // kind's templated phrase, never silence.
            (NarrationKind::Execute, None, "", Some("Running a command.")),
            // Zed's own move title reads as prose and keeps precedence.
            (
                NarrationKind::Move,
                None,
                "Rename thread view to conversation view",
                None,
            ),
        ] {
            assert_eq!(
                generated_phrase(kind, None, structured, label, false).as_deref(),
                expected,
                "{kind:?} with structured {structured:?} and title {label:?}"
            );
        }
    }

    /// The same defect one kind over. An agent whose search tool puts its
    /// subject under a key nothing here recognises issues three calls all
    /// titled "Search"; before, those fell through to the title, compared
    /// equal, and two of the three went silent. Every kind now ends at a
    /// templated phrase rather than at nothing, so the first is always said.
    #[test]
    fn every_kind_has_something_to_say_when_nothing_resolves() {
        for kind in [
            NarrationKind::Read,
            NarrationKind::Edit,
            NarrationKind::Delete,
            NarrationKind::Move,
            NarrationKind::Search,
            NarrationKind::Execute,
            NarrationKind::Fetch,
            NarrationKind::Other,
        ] {
            // No structured input and no title at all: the shape an
            // unrecognised tool schema plus a placeholder produces.
            assert_eq!(
                generated_phrase(kind, None, None, "", false).as_deref(),
                Some(kind.unnamed_phrase()),
                "{kind:?} must still have a line when nothing resolves"
            );
        }
    }

    /// And with a generic title present, the title is still what is spoken —
    /// the templated phrase is the floor, not a replacement.
    #[gpui::test]
    async fn a_generic_title_is_spoken_once_and_then_suppressed(cx: &mut TestAppContext) {
        let mut queue = NarrationQueue::default();
        let first = markdown("Search", cx);
        let second = markdown("Search", cx);
        cx.run_until_parked();

        cx.update(|cx| {
            assert!(
                queue.push_tool_call(
                    ToolCallFacts::from_label("a", first, NarrationKind::Search),
                    cx
                ),
                "the first call is always said, whatever its title"
            );
            assert!(
                !queue.push_tool_call(
                    ToolCallFacts::from_label("b", second, NarrationKind::Search),
                    cx
                ),
                "and an indistinguishable second one adds nothing"
            );
        });
    }

    /// The bug the user hit: an external agent gives every shell command the
    /// same generic title, so suppressing on the title turned a run of
    /// different commands into one utterance followed by silence.
    #[gpui::test]
    async fn distinct_commands_sharing_one_title_are_all_spoken(cx: &mut TestAppContext) {
        let mut queue = NarrationQueue::default();
        let labels: Vec<_> = (0..3).map(|_| markdown("Terminal", cx)).collect();
        cx.run_until_parked();

        let mut spoken = Vec::new();
        for (label, command) in labels.into_iter().zip(["git status", "cargo test", "ls"]) {
            let facts = ToolCallFacts {
                command: Some(command.to_string()),
                ..ToolCallFacts::from_label("call", label, NarrationKind::Execute)
            };
            cx.update(|cx| {
                assert!(
                    queue.push_tool_call(facts, cx),
                    "{command} is a different thing to say than the one before it"
                );
            });
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
                "Running `git status`.".to_string(),
                "Then `cargo test`.".to_string(),
                "Then `ls`.".to_string(),
            ]
        );
    }

    /// The other half: the same command twice running is still one thing to
    /// say, even though the resolved text is now what is compared.
    #[gpui::test]
    async fn the_same_command_twice_is_not_said_twice(cx: &mut TestAppContext) {
        let mut queue = NarrationQueue::default();
        let first = markdown("Terminal", cx);
        let second = markdown("Terminal", cx);
        cx.run_until_parked();

        cx.update(|cx| {
            let facts = |label| ToolCallFacts {
                command: Some("cargo test -p read_aloud".to_string()),
                ..ToolCallFacts::from_label("call", label, NarrationKind::Execute)
            };
            assert!(queue.push_tool_call(facts(first), cx));
            assert!(
                !queue.push_tool_call(facts(second), cx),
                "the same command back to back tells the listener nothing new"
            );
        });
        assert_eq!(queue.len(), 1);
    }

    /// Two different files with the same name sound identical, so saying
    /// both is saying the same thing twice.
    #[gpui::test]
    async fn two_files_that_sound_alike_are_only_said_once(cx: &mut TestAppContext) {
        let mut queue = NarrationQueue::default();
        let first = markdown("Read file", cx);
        let second = markdown("Read file", cx);
        let third = markdown("Read file", cx);
        cx.run_until_parked();

        cx.update(|cx| {
            let facts = |id: &str, label, path: &str| ToolCallFacts {
                path: Some(path.to_string()),
                ..ToolCallFacts::from_label(id, label, NarrationKind::Read)
            };
            assert!(queue.push_tool_call(facts("a", first, "crates/a/foo.rs"), cx));
            assert!(
                !queue.push_tool_call(facts("b", second, "crates/b/foo.rs"), cx),
                "\"Reading foo. Then foo.\" tells the listener nothing the \
                 first line did not"
            );
            assert!(
                queue.push_tool_call(facts("c", third, "crates/a/bar.rs"), cx),
                "a genuinely different-sounding file is still said"
            );
        });
    }

    /// Reducing a path to what is heard must not collapse two *different
    /// actions* on files that sound alike. `Cargo.toml` in two crates is the
    /// ordinary case, and dropping the edit is a real action going unspoken
    /// — the original complaint in miniature.
    #[gpui::test]
    async fn doing_two_different_things_to_similar_names_is_two_things_to_say(
        cx: &mut TestAppContext,
    ) {
        let mut queue = NarrationQueue::default();
        let read = markdown("Read file", cx);
        let edit = markdown("Edit file", cx);
        cx.run_until_parked();

        cx.update(|cx| {
            assert!(queue.push_tool_call(
                ToolCallFacts {
                    path: Some("crates/a/Cargo.toml".to_string()),
                    ..ToolCallFacts::from_label("a", read, NarrationKind::Read)
                },
                cx
            ));
            assert!(
                queue.push_tool_call(
                    ToolCallFacts {
                        path: Some("crates/b/Cargo.toml".to_string()),
                        ..ToolCallFacts::from_label("b", edit, NarrationKind::Edit)
                    },
                    cx
                ),
                "editing is not reading, however alike the two files sound"
            );
        });
        assert_eq!(queue.len(), 2);
    }

    /// Structured input is agent-controlled and can be enormous — Claude
    /// Code's Bash tool routinely carries multi-line heredoc scripts. Two
    /// dozen of those would be tens of kilobytes of prompt on a path with a
    /// two-second race to lose.
    #[gpui::test]
    async fn an_enormous_command_is_bounded_before_it_reaches_a_prompt(cx: &mut TestAppContext) {
        let label = markdown("Terminal", cx);
        cx.run_until_parked();
        let facts = ToolCallFacts {
            command: Some(format!(
                "cat <<'EOF' > out.txt\n{}\nEOF",
                "x".repeat(20_000)
            )),
            ..ToolCallFacts::from_label("call", label, NarrationKind::Execute)
        };
        cx.update(|cx| {
            let description = facts.description(cx);
            assert!(
                description.chars().count() <= MAX_ACTION_CHARS + 40,
                "one action must not crowd out the message it is context for, \
                 got {} characters",
                description.chars().count()
            );
            assert!(
                description.contains("cat"),
                "and the front of it, which is the part that identifies the \
                 command, survives"
            );
        });
    }

    /// A prompt sees the real command and the real path — and, above all,
    /// that something failed.
    #[gpui::test]
    async fn a_prompt_sees_the_real_command_and_whether_it_failed(cx: &mut TestAppContext) {
        let label = markdown("Terminal", cx);
        cx.run_until_parked();
        let facts = ToolCallFacts {
            command: Some("cargo test -p read_aloud".to_string()),
            ..ToolCallFacts::from_label("call", label, NarrationKind::Execute)
        };
        cx.update(|cx| {
            assert_eq!(
                facts.description(cx),
                "ran the command `cargo test -p read_aloud`",
                "never the generic title, which tells a model nothing"
            );
            let failed = ToolCallFacts {
                outcome: ToolCallOutcome::Failed,
                ..facts.clone()
            };
            assert!(
                failed.description(cx).contains("FAILED"),
                "a failure is the one thing a supervising listener must hear"
            );
        });
    }

    #[test]
    fn a_cd_prefix_stops_at_the_earliest_separator() {
        // `cd /x; ls && grep …` runs `ls`; searching for `&&` first would
        // strip straight through it and announce the wrong program.
        assert_eq!(
            generated_phrase(
                NarrationKind::Execute,
                None,
                None,
                "cd /x; ls -la && grep foo",
                false
            )
            .as_deref(),
            Some("Running `ls`.")
        );
        assert_eq!(
            generated_phrase(
                NarrationKind::Execute,
                None,
                None,
                "cd /x && cargo test",
                false
            )
            .as_deref(),
            Some("Running `cargo test`.")
        );
    }

    #[test]
    fn a_code_span_label_is_unescaped_too() {
        // A backslash surviving into the code span would be spoken.
        assert_eq!(
            generated_phrase(
                NarrationKind::Read,
                None,
                None,
                "Read file `crates/read\\_aloud/src/player.rs`",
                false
            )
            .as_deref(),
            Some("Reading `crates/read_aloud/src/player.rs`.")
        );
    }

    #[test]
    fn a_line_that_only_restates_what_was_said_adds_nothing() {
        let said = "I moved the poll loop onto a timer.";
        assert!(adds_nothing("It moved the poll loop onto a timer.", said));
        assert!(adds_nothing("Moving the poll loop to a timer.", said));
        assert!(adds_nothing(NOTHING_TO_ADD, said));
        assert!(adds_nothing("Nothing to add.", said));
        assert!(adds_nothing("   ", said));
    }

    #[test]
    fn a_line_that_names_the_work_is_kept() {
        let said = "Let me look at how the sync pipeline is put together before guessing.";
        assert!(!adds_nothing(
            "Reading the sync design and the batcher to see how batches are formed.",
            said
        ));
        assert!(!adds_nothing("Running the sync tests.", said));
        // The check is conservative: adding one word to a sentence the
        // listener just heard is still a repetition.
        assert!(adds_nothing(
            "Looking at how the sync pipeline is put together.",
            said
        ));
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
        assert!(prompt.contains("One sentence, under fifteen words"));
        assert!(prompt.contains("then the reason"));
        assert!(
            prompt.contains(NOTHING_TO_ADD),
            "the model is given a way to say the prose already covered it"
        );
    }

    fn wrap_up_material<'a>(
        message: &'a str,
        activity: &'a [String],
        files_changed: &'a [String],
        recent: &'a [String],
        still_streaming: bool,
    ) -> WrapUpMaterial<'a> {
        WrapUpMaterial {
            message,
            still_streaming,
            activity,
            files_changed,
            recent,
            budget: brief_budget(),
        }
    }

    #[test]
    fn the_wrap_up_prompt_says_when_the_message_is_unfinished() {
        let streaming = wrap_up_prompt(wrap_up_material(
            "The tests all pass now",
            &[],
            &[],
            &[],
            true,
        ));
        assert!(streaming.contains("still be being written"));
        let finished = wrap_up_prompt(wrap_up_material(
            "The tests all pass now.",
            &[],
            &[],
            &[],
            false,
        ));
        assert!(!finished.contains("still be being written"));
    }

    #[test]
    fn the_wrap_up_prompt_carries_the_turn_and_forbids_repeating_it() {
        let prompt = wrap_up_prompt(wrap_up_material(
            "Everything is wired up.",
            &["ran the command `cargo test -p read_aloud`".to_string()],
            &[],
            &["Running cargo test.".to_string()],
            false,
        ));
        assert!(prompt.contains("cargo test -p read_aloud"));
        assert!(prompt.contains("Running cargo test."));
        assert!(prompt.contains("must not repeat"));
        assert!(prompt.contains("Build on what you have already said"));
        assert!(
            prompt.contains("under twenty-five words"),
            "a two-call turn has earned a sentence, not a speech"
        );
    }

    /// "The tests failed" is the single most important thing a supervising
    /// listener needs, and the closing paragraph is not reliably where an
    /// agent mentions it. The prompt has to see the failure itself, and be
    /// told to lead with it.
    #[test]
    fn the_wrap_up_prompt_leads_on_a_failure_and_names_the_files() {
        let prompt = wrap_up_prompt(wrap_up_material(
            "That should do it.",
            &[
                "edited crates/read_aloud/src/segmenter.rs".to_string(),
                "ran the command `cargo test -p read_aloud` — it FAILED".to_string(),
            ],
            &["crates/read_aloud/src/segmenter.rs".to_string()],
            &[],
            false,
        ));
        assert!(prompt.contains("it FAILED"));
        assert!(prompt.contains("say that first"));
        assert!(prompt.contains("Files it changed"));
        assert!(prompt.contains("crates/read_aloud/src/segmenter.rs"));
    }

    /// Duty cycle, as a ratchet. Expressed in the unit the listener actually
    /// experiences: seconds of speech at the one moment they are waiting to
    /// act on the result.
    ///
    /// Both numbers are checked, because they are different promises. The
    /// *ask* is what the prompt tells the model to write; the *ceiling* is
    /// the longest reply the code will actually accept, which is longer by
    /// the slack in the character bound. Asserting only the ask would let the
    /// enforced ceiling drift while the test stayed green — the guarantee
    /// would read tighter than the code makes.
    ///
    /// These are an argument, not a measurement, so the test exists to make
    /// raising one deliberate.
    #[test]
    fn no_wrap_up_tier_is_a_monologue() {
        for (tool_calls, asked, ceiling) in [(1, 10.0, 12.0), (5, 16.0, 19.0), (20, 22.0, 26.0)] {
            let budget = WrapUpBudget::for_turn(tool_calls, None);
            assert!(
                budget.asked_seconds() <= asked,
                "a {tool_calls}-call turn's wrap-up asks for {:.1}s, past the {asked}s argued \
                 for it — raise this only with a reason",
                budget.asked_seconds()
            );
            assert!(
                budget.max_spoken_seconds() <= ceiling,
                "and will accept a reply running {:.1}s, past the {ceiling}s ceiling — the \
                 enforced bound must not drift away from the ask",
                budget.max_spoken_seconds()
            );
        }
    }

    /// The prompt asks for the length this turn has earned, not a constant.
    #[test]
    fn the_wrap_up_prompt_asks_for_the_length_the_turn_earned() {
        let of = |tool_calls| {
            wrap_up_prompt(WrapUpMaterial {
                message: "Done.",
                still_streaming: false,
                activity: &[],
                files_changed: &[],
                recent: &[],
                budget: WrapUpBudget::for_turn(tool_calls, None),
            })
        };
        assert!(of(1).contains("under twenty-five words"));
        assert!(of(5).contains("under forty words"));
        assert!(of(20).contains("under fifty-five words"));
    }

    /// Prompts written for a fake model can afford to be vague. A small fast
    /// model needs to be told what *not* to do, and shown the shape.
    #[test]
    fn every_prompt_forbids_preamble_and_shows_the_shape() {
        for prompt in [
            summary_prompt("I moved the poll loop onto a timer."),
            step_prompt("Some prose.", &["read the segmenter".to_string()], &[]),
            wrap_up_prompt(wrap_up_material("Done.", &[], &[], &[], false)),
        ] {
            assert!(
                prompt.contains("Good reply:"),
                "a small model follows an example better than a description"
            );
            assert!(prompt.contains("No preamble") || prompt.contains("no preamble"));
            assert!(
                prompt.contains("Never open with"),
                "\"The agent is currently...\" is what these models do unprompted"
            );
            assert!(
                prompt.contains("backticks"),
                "spoken aloud, a backtick is a noise"
            );
        }
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
            cx.update(|cx| queue.push_tool_call(titled(label, kind), cx));
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
            queue.push_tool_call(titled(label, NarrationKind::Read), cx);
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
                queue.push_tool_call(titled(label, NarrationKind::Read), cx);
            }
        });
        assert_eq!(queue.recent_lines().len(), RECENT_LINES);
    }

    /// One `tool_call` or `tool_call_update` from the captured session, with
    /// only the fields narration reads.
    struct CapturedUpdate {
        kind: Option<NarrationKind>,
        title: Option<String>,
        status: Option<String>,
        raw_input: Option<serde_json::Value>,
        locations: usize,
    }

    /// The capture, in arrival order. Parsed rather than transcribed: three
    /// rounds of fixes to this path were written against an *inferred*
    /// protocol shape and all three missed, so nothing here is retyped by
    /// hand.
    fn captured_updates() -> Vec<CapturedUpdate> {
        let capture: serde_json::Value =
            serde_json::from_str(crate::CLAUDE_CODE_TOOL_CALL_CAPTURE).expect("the capture parses");
        capture["updates"]
            .as_array()
            .expect("the capture is a list of updates")
            .iter()
            .map(|update| CapturedUpdate {
                kind: update["kind"].as_str().map(|kind| match kind {
                    "read" => NarrationKind::Read,
                    "edit" => NarrationKind::Edit,
                    "delete" => NarrationKind::Delete,
                    "move" => NarrationKind::Move,
                    "search" => NarrationKind::Search,
                    "execute" => NarrationKind::Execute,
                    "fetch" => NarrationKind::Fetch,
                    _ => NarrationKind::Other,
                }),
                title: update["title"].as_str().map(str::to_string),
                status: update["status"].as_str().map(str::to_string),
                raw_input: update.get("rawInput").cloned(),
                locations: update["locations"].as_array().map_or(0, Vec::len),
            })
            .collect()
    }

    /// The measurement everything else in this file's tool-call handling now
    /// rests on. If a future capture replaces this one and these numbers move,
    /// the design premises move with them and every test below is suspect.
    #[test]
    fn the_capture_says_what_the_design_assumes() {
        let updates = captured_updates();
        let arrivals: Vec<_> = updates
            .iter()
            .filter(|update| update.kind.is_some() && update.status.is_some())
            .collect();
        // Only the initial notifications carry both a kind and a status.
        assert_eq!(arrivals.len(), 28, "the session made twenty-eight calls");
        assert!(
            arrivals
                .iter()
                .all(|arrival| arrival.status.as_deref() == Some("pending")),
            "every call arrives `pending`, complete or not — which is exactly \
             why a status-based gate cannot work"
        );
        let empty: Vec<_> = arrivals
            .iter()
            .filter(|arrival| {
                RawToolInput::from_json(arrival.raw_input.as_ref()).input == ToolCallInput::Empty
            })
            .collect();
        assert_eq!(empty.len(), 6, "six of the twenty-eight arrive contentless");
        assert!(
            empty
                .iter()
                .all(|arrival| arrival.title.as_deref() == Some("Terminal")),
            "the contentless ones are all titled `Terminal`"
        );
        assert!(
            arrivals.iter().all(|arrival| arrival.raw_input.is_some()),
            "and `is_some()` is true for all twenty-eight, contentless or not: \
             the check that survived three rounds of fixes cannot tell them apart"
        );
        assert!(
            arrivals
                .iter()
                .filter(|arrival| arrival.kind == Some(NarrationKind::Read))
                .all(|arrival| arrival.locations > 0),
            "a read arrives complete: a file path, a real title, and populated \
             `locations`. Nothing about that path needed changing."
        );
        let with_purpose = updates
            .iter()
            .filter(|update| {
                RawToolInput::from_json(update.raw_input.as_ref())
                    .purpose
                    .is_some()
            })
            .count();
        assert!(
            with_purpose >= 26,
            "the agent states a purpose for its own calls, and we were throwing \
             it away; got {with_purpose}"
        );
    }

    /// Only `execute` calls carry a description. `read` carries a file path
    /// and populated `locations` and needs none; no other kind appears in the
    /// capture at all. Asserted so a later reader does not have to take the
    /// grep on faith.
    #[test]
    fn only_execute_calls_state_a_purpose() {
        let mut kinds_with_purpose = std::collections::BTreeSet::new();
        let mut kinds_seen = std::collections::BTreeSet::new();
        let mut current = None;
        for update in captured_updates() {
            current = update.kind.or(current);
            let Some(kind) = current else { continue };
            kinds_seen.insert(format!("{kind:?}"));
            if RawToolInput::from_json(update.raw_input.as_ref())
                .purpose
                .is_some()
            {
                kinds_with_purpose.insert(format!("{kind:?}"));
            }
        }
        assert_eq!(
            kinds_seen,
            ["Execute".to_string(), "Read".to_string()].into(),
            "the capture only exercises two kinds"
        );
        assert_eq!(
            kinds_with_purpose,
            ["Execute".to_string()].into(),
            "a purpose only ever arrives on an execute call"
        );
    }

    /// `Option::is_some()` reports a present-but-empty container as present.
    /// That is the whole bug.
    #[test]
    fn an_empty_payload_is_not_the_same_as_a_missing_one() {
        assert_eq!(
            RawToolInput::from_json(None).input,
            ToolCallInput::Absent,
            "an agent that sends no structured input at all is not waiting to"
        );
        let empty = serde_json::json!({});
        assert!(Some(&empty).is_some(), "the trap, stated");
        assert_eq!(
            RawToolInput::from_json(Some(&empty)).input,
            ToolCallInput::Empty
        );
        let present = serde_json::json!({"command": "ls -la"});
        assert_eq!(
            RawToolInput::from_json(Some(&present)).input,
            ToolCallInput::Present
        );
    }

    /// The exact three-step trace the capture shows for a contentless call,
    /// replayed. The listener heard "running terminal" because the middle
    /// step — the only one that changes anything — used to arrive after the
    /// settle timer had already spoken.
    #[gpui::test]
    async fn a_contentless_execute_call_waits_for_its_command(cx: &mut TestAppContext) {
        let terminal = markdown("Terminal", cx);
        let refined = markdown("echo \"=== PRD counts ===\" && ls -1 docs/prds/", cx);
        cx.run_until_parked();
        let facts = |label: Entity<Markdown>, raw: serde_json::Value| {
            let raw = RawToolInput::from_json(Some(&raw));
            ToolCallFacts {
                purpose: raw.purpose,
                command: raw.command,
                input: raw.input,
                ..ToolCallFacts::from_label("empty", label, NarrationKind::Execute)
            }
        };
        let arrival = facts(terminal.clone(), serde_json::json!({}));
        assert!(
            arrival.awaiting_input(),
            "`rawInput: {{}}` with the title `Terminal` has nothing to say yet"
        );
        cx.update(|cx| {
            assert_eq!(
                arrival.spoken_key(cx),
                "Terminal",
                "and if it is spoken anyway, this is the word the user heard"
            );
        });

        let raw_command = facts(
            terminal,
            serde_json::json!({"command": "cd /Users/j/bartr && echo \"=== PRD counts ===\""}),
        );
        assert!(
            !raw_command.awaiting_input(),
            "the first refinement carries a command, so the wait is over"
        );

        let described = facts(
            refined,
            serde_json::json!({
                "command": "echo \"=== PRD counts ===\" && ls -1 docs/prds/",
                "description": "List PRD folders and contents",
            }),
        );
        assert!(!described.awaiting_input());
        cx.update(|cx| {
            assert_eq!(described.spoken_key(cx), "List PRD folders and contents");
        });
        let mut queue = NarrationQueue::default();
        cx.update(|cx| assert!(queue.push_tool_call(described, cx)));
        assert_eq!(
            queue.recent_lines(),
            &["Listing PRD folders and contents.".to_string()],
        );
    }

    /// Every purpose in the capture, and the line it becomes. Nineteen are
    /// imperative and take a participle; seven are noun phrases and are
    /// spoken exactly as the agent wrote them.
    #[test]
    fn every_captured_purpose_reads_as_narration() {
        for (purpose, expected) in [
            (
                "List PRD folders and contents",
                "Listing PRD folders and contents",
            ),
            ("List repo root and docs", "Listing repo root and docs"),
            (
                "Search almanac for launch readiness",
                "Searching almanac for launch readiness",
            ),
            (
                "Check roadmap/product doc sizes and launch mentions",
                "Checking roadmap/product doc sizes and launch mentions",
            ),
            (
                "Compute checkbox progress for in-progress PRDs",
                "Computing checkbox progress for in-progress PRDs",
            ),
            (
                "Checkbox completion per in-progress and review PRD",
                "Checkbox completion per in-progress and review PRD",
            ),
            (
                "Search almanac for launch blockers and list topics",
                "Searching almanac for launch blockers and list topics",
            ),
            (
                "Find launch references in almanac",
                "Finding launch references in almanac",
            ),
            (
                "Recent commits and unpushed count",
                "Recent commits and unpushed count",
            ),
            (
                "Check waitlist backend existence",
                "Checking waitlist backend existence",
            ),
            (
                "Check review PRD remaining items",
                "Checking review PRD remaining items",
            ),
            (
                "Check shipped PRDs and waitlist wizard state",
                "Checking shipped PRDs and waitlist wizard state",
            ),
            (
                "Inspect v2 marketing components and CTA targets",
                "Inspecting v2 marketing components and CTA targets",
            ),
            (
                "List app routes for marketing surface",
                "Listing app routes for marketing surface",
            ),
            (
                "Collect marketing link targets",
                "Collecting marketing link targets",
            ),
            ("Extract marketing hrefs", "Extracting marketing hrefs"),
            (
                "Extract marketing hrefs (quoted glob)",
                "Extracting marketing hrefs (quoted glob)",
            ),
            (
                "Look for WorkOS pages in almanac",
                "Looking for WorkOS pages in almanac",
            ),
            (
                "Search remember history for WorkOS 500",
                "Searching remember history for WorkOS 500",
            ),
            (
                "Full marketing route list and sitemap entries",
                "Full marketing route list and sitemap entries",
            ),
            (
                "Checkbox progress per in-progress PRD",
                "Checkbox progress per in-progress PRD",
            ),
            (
                "Almanac structure and README",
                "Almanac structure and README",
            ),
            ("Find almanac pages", "Finding almanac pages"),
            ("App route structure", "App route structure"),
        ] {
            assert_eq!(spoken_purpose(purpose), expected, "purpose {purpose:?}");
        }
    }

    /// The whole capture, replayed. Every narrated call must produce a line
    /// that says what the agent is doing — and they must differ, because a
    /// run of identical lines is silence with extra steps.
    #[gpui::test]
    async fn replaying_the_capture_produces_distinct_meaningful_lines(cx: &mut TestAppContext) {
        // Each call's facts as of its *last* update, which is what narration
        // ends up speaking once the contentless ones have been refined.
        let mut final_state: Vec<(NarrationKind, String, serde_json::Value)> = Vec::new();
        let mut current: Option<usize> = None;
        for update in captured_updates() {
            match (update.kind, update.status.as_deref()) {
                (Some(kind), Some(_)) => {
                    current = Some(final_state.len());
                    final_state.push((
                        kind,
                        update.title.clone().unwrap_or_default(),
                        update.raw_input.clone().unwrap_or(serde_json::Value::Null),
                    ));
                }
                _ => {
                    let Some(index) = current.and_then(|index| final_state.get_mut(index)) else {
                        continue;
                    };
                    if let Some(title) = update.title.clone() {
                        index.1 = title;
                    }
                    if let Some(raw_input) = update.raw_input.clone() {
                        index.2 = raw_input;
                    }
                }
            }
        }
        assert_eq!(final_state.len(), 28);

        let labels: Vec<_> = final_state
            .iter()
            .map(|(_, title, _)| markdown(title, cx))
            .collect();
        cx.run_until_parked();
        let mut queue = NarrationQueue::default();
        let mut spoken = Vec::new();
        cx.update(|cx| {
            for (index, ((kind, _, raw_input), label)) in final_state.iter().zip(labels).enumerate()
            {
                let raw = RawToolInput::from_json(Some(raw_input));
                let facts = ToolCallFacts {
                    purpose: raw.purpose,
                    command: raw.command,
                    path: raw.path,
                    url: raw.url,
                    query: raw.query,
                    input: raw.input,
                    ..ToolCallFacts::from_label(index.to_string(), label, *kind)
                };
                assert!(
                    !facts.awaiting_input(),
                    "every call in the capture is eventually refined; #{index} was not"
                );
                if queue.push_tool_call(facts, cx) {
                    spoken.push(queue.recent_lines().last().cloned().unwrap_or_default());
                }
            }
        });

        assert!(
            !spoken
                .iter()
                .any(|line| line.to_lowercase().contains("terminal")),
            "the reported bug: {spoken:#?}"
        );
        assert!(
            !spoken
                .iter()
                .any(|line| line == NarrationKind::Execute.unnamed_phrase()),
            "no call should fall through to the templated phrase: {spoken:#?}"
        );
        assert_eq!(
            spoken.len(),
            28,
            "every one of the twenty-eight calls says something: {spoken:#?}"
        );
        assert!(
            spoken.windows(2).all(|pair| pair[0] != pair[1]),
            "no line repeats the one before it: {spoken:#?}"
        );
        // Two purposes genuinely recur later in the session ("List PRD
        // folders and contents", "Recent commits and unpushed count"). They
        // are far apart, so suppressing them would leave the listener with
        // silence where a real call happened.
        let distinct: std::collections::BTreeSet<_> = spoken.iter().collect();
        assert_eq!(distinct.len(), 26, "{spoken:#?}");
        assert!(
            spoken.contains(&"Searching almanac for launch readiness.".to_string()),
            "{spoken:#?}"
        );
        assert!(
            spoken.contains(
                &"Reading `/Users/joshuar.bunnell/code/bartrhomes/bartr/PRODUCT.md`.".to_string()
            ),
            "a read still narrates its file: {spoken:#?}"
        );
    }

    /// The prompts get the purpose too — it is shorter than the command and
    /// it is about intent, which is what a summarizing model needs.
    #[gpui::test]
    async fn a_prompts_account_of_a_call_uses_the_stated_purpose(cx: &mut TestAppContext) {
        let label = markdown("Terminal", cx);
        cx.run_until_parked();
        let facts = ToolCallFacts {
            purpose: Some("Search almanac for launch readiness".to_string()),
            command: Some(
                "codealmanac search \"launch readiness\" --limit 10 2>&1 | head -50".to_string(),
            ),
            input: ToolCallInput::Present,
            ..ToolCallFacts::from_label("call", label, NarrationKind::Execute)
        };
        cx.update(|cx| {
            assert_eq!(
                facts.description(cx),
                "ran a command — Search almanac for launch readiness"
            );
            let failed = ToolCallFacts {
                outcome: ToolCallOutcome::Failed,
                ..facts.clone()
            };
            assert!(
                failed.description(cx).ends_with("— it FAILED"),
                "the failure guarantee survives the new source"
            );
        });
    }

    /// A read's file is still the point; a description would not improve it,
    /// and the capture sends none.
    #[gpui::test]
    async fn a_purpose_does_not_displace_a_file(cx: &mut TestAppContext) {
        let label = markdown("Read PRODUCT.md", cx);
        cx.run_until_parked();
        let facts = ToolCallFacts {
            purpose: Some("Understand the product".to_string()),
            path: Some("/repo/PRODUCT.md".to_string()),
            input: ToolCallInput::Present,
            ..ToolCallFacts::from_label("call", label, NarrationKind::Read)
        };
        cx.update(|cx| {
            assert_eq!(facts.spoken_key(cx), "/repo/PRODUCT.md");
            assert_eq!(facts.description(cx), "read /repo/PRODUCT.md");
        });
    }

    /// …but it beats saying nothing. A kind that resolves to its templated
    /// phrase takes the agent's words instead.
    #[gpui::test]
    async fn a_purpose_displaces_a_templated_phrase(cx: &mut TestAppContext) {
        let label = markdown("", cx);
        cx.run_until_parked();
        let facts = ToolCallFacts {
            purpose: Some("Find every caller of the poll loop".to_string()),
            input: ToolCallInput::Present,
            ..ToolCallFacts::from_label("call", label, NarrationKind::Search)
        };
        cx.update(|cx| {
            assert_eq!(facts.spoken_key(cx), "Find every caller of the poll loop");
        });
    }

    /// A purpose that is nothing but punctuation is worse than no purpose:
    /// it produces no audio, and every call carrying it compares equal, so
    /// the whole run is suppressed and the turn goes silent.
    #[gpui::test]
    async fn a_purpose_with_nothing_to_say_is_discarded(cx: &mut TestAppContext) {
        let label = markdown("Terminal", cx);
        cx.run_until_parked();
        let facts = ToolCallFacts {
            purpose: Some("…".to_string()),
            command: Some("cargo test -p read_aloud".to_string()),
            input: ToolCallInput::Present,
            ..ToolCallFacts::from_label("call", label, NarrationKind::Execute)
        };
        cx.update(|cx| {
            assert_eq!(
                facts.spoken_key(cx),
                "cargo test",
                "the command still names the call"
            );
        });
    }

    /// Agent-controlled prose on a path with a two-second budget.
    #[gpui::test]
    async fn a_runaway_purpose_is_bounded(cx: &mut TestAppContext) {
        let label = markdown("Terminal", cx);
        cx.run_until_parked();
        let facts = ToolCallFacts {
            purpose: Some("Check ".repeat(400)),
            input: ToolCallInput::Present,
            ..ToolCallFacts::from_label("call", label, NarrationKind::Execute)
        };
        cx.update(|cx| {
            assert!(facts.spoken_key(cx).chars().count() <= MAX_PURPOSE_CHARS);
        });
    }
}
