use crate::voice_dispatch::VoiceCandidate;
use crate::{
    DEFAULT_THREAD_TITLE, SelectPermissionGranularity,
    agent_configuration::configure_context_server_modal::default_markdown_style,
    conversation_view::thread_search_bar::{ThreadSearchBar, ThreadSearchBarEvent},
    open_abs_path_at_point,
    thread_metadata_store::{ThreadId, ThreadMetadataStore},
};
use agent_client_protocol::schema::v1 as acp;
use std::cell::RefCell;
use std::rc::Rc;

use acp_thread::{
    Elicitation, ElicitationEntryId, ElicitationStatus, PlanEntry, SandboxAuthorizationDetails,
    SandboxFallbackAuthorizationDetails, SandboxNotAppliedReason, SteerOutcome,
    decode_path_escapes,
};
use agent::{
    SandboxStatusKey, SandboxStatusRefresh, SkillLoadingIssue, SkillLoadingIssueKind,
    SkillLoadingIssuesUpdated, ThreadSandbox, VerifiedSandboxStatus,
};
use agent_settings::UserAgentsMd;
use agent_skills::MAX_SKILL_DESCRIPTION_LEN;
use cloud_api_types::{SubmitAgentThreadFeedbackBody, SubmitAgentThreadFeedbackCommentsBody};
use editor::actions::OpenExcerpts;
use sandbox::{SandboxFsPolicy, SandboxNetPolicy, SandboxPolicy};

use crate::agent_panel_styling::AgentPanelStylingSettings;
use crate::completion_provider::{AvailableSkill, PromptLocalCommand, pluralize};
use crate::message_editor::SharedSessionCapabilities;
use crate::subagents::{SubagentActivity, SubagentCounts, SubagentStatus, SubagentSummary};
use crate::ui::{
    SandboxGroup, SandboxRow, SandboxSection, SandboxStatusTooltip, TerminalSandboxWarning,
    TerminalToolHeader,
};
use crate::unicode_confusables;

use db::kvp::KeyValueStore;
use futures::StreamExt as _;
use gpui::List;
use gpui::Stateful;
use gpui::TaskExt;
use heapless::Vec as ArrayVec;
use language_model::{
    CompletionIntent, ConfiguredModel, FastModeConfirmation, LanguageModel,
    LanguageModelEffortLevel, LanguageModelId, LanguageModelProvider, LanguageModelProviderId,
    LanguageModelRegistry, LanguageModelRequest, LanguageModelRequestMessage, Role, Speed,
};
use notifications::status_toast::StatusToast;
use settings::{update_settings_file, update_settings_file_with_completion};
use ui::{
    ButtonLike, CalloutBorderPosition, Checkbox, SpinnerLabel, SpinnerVariant, SplitButton,
    SplitButtonStyle, Tab, ToggleState,
};
use util::markdown::{source_position_from_fragment, split_local_url_fragment};
use workspace::{OpenOptions, SERIALIZATION_THROTTLE_TIME, Toast, notifications::NotificationId};

use super::elicitation::{
    ElicitationCard, ElicitationCardHandlers, ElicitationFormState, should_render_elicitation,
};
use super::*;

const DATA_RETENTION_LEARN_MORE_URL: &str = "https://support.claude.com/en/articles/15425996-data-retention-practices-for-mythos-class-models";

#[derive(Default)]
struct ThreadFeedbackState {
    feedback: Option<ThreadFeedback>,
    comments_editor: Option<Entity<Editor>>,
}

impl ThreadFeedbackState {
    pub fn submit(
        &mut self,
        thread: Entity<AcpThread>,
        feedback: ThreadFeedback,
        window: &mut Window,
        cx: &mut App,
    ) {
        let Some(telemetry) = thread.read(cx).connection().telemetry() else {
            return;
        };

        let project = thread.read(cx).project().read(cx);
        let client = project.client();
        let user_store = project.user_store();
        let organization = user_store.read(cx).current_organization();

        if self.feedback == Some(feedback) {
            return;
        }

        self.feedback = Some(feedback);
        match feedback {
            ThreadFeedback::Positive => {
                self.comments_editor = None;
            }
            ThreadFeedback::Negative => {
                self.comments_editor = Some(Self::build_feedback_comments_editor(window, cx));
            }
        }
        let session_id = thread.read(cx).session_id().clone();
        let parent_session_id = thread.read(cx).parent_session_id().cloned();
        let agent_telemetry_id = thread.read(cx).connection().telemetry_id();
        let task = telemetry.thread_data(&session_id, cx);
        let rating = match feedback {
            ThreadFeedback::Positive => "positive",
            ThreadFeedback::Negative => "negative",
        };
        cx.background_spawn(async move {
            let thread = task.await?;

            client
                .cloud_client()
                .submit_agent_feedback(SubmitAgentThreadFeedbackBody {
                    organization_id: organization.map(|organization| organization.id.clone()),
                    agent: agent_telemetry_id.to_string(),
                    session_id: session_id.to_string(),
                    parent_session_id: parent_session_id.map(|id| id.to_string()),
                    rating: rating.to_string(),
                    thread,
                })
                .await?;

            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    pub fn submit_comments(&mut self, thread: Entity<AcpThread>, cx: &mut App) {
        let Some(telemetry) = thread.read(cx).connection().telemetry() else {
            return;
        };

        let Some(comments) = self
            .comments_editor
            .as_ref()
            .map(|editor| editor.read(cx).text(cx))
            .filter(|text| !text.trim().is_empty())
        else {
            return;
        };

        self.comments_editor.take();

        let project = thread.read(cx).project().read(cx);
        let client = project.client();
        let user_store = project.user_store();
        let organization = user_store.read(cx).current_organization();

        let session_id = thread.read(cx).session_id().clone();
        let agent_telemetry_id = thread.read(cx).connection().telemetry_id();
        let task = telemetry.thread_data(&session_id, cx);
        cx.background_spawn(async move {
            let thread = task.await?;

            client
                .cloud_client()
                .submit_agent_feedback_comments(SubmitAgentThreadFeedbackCommentsBody {
                    organization_id: organization.map(|organization| organization.id.clone()),
                    agent: agent_telemetry_id.to_string(),
                    session_id: session_id.to_string(),
                    comments,
                    thread,
                })
                .await?;

            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    pub fn clear(&mut self) {
        *self = Self::default()
    }

    pub fn dismiss_comments(&mut self) {
        self.comments_editor.take();
    }

    fn build_feedback_comments_editor(window: &mut Window, cx: &mut App) -> Entity<Editor> {
        let buffer = cx.new(|cx| {
            let empty_string = String::new();
            MultiBuffer::singleton(cx.new(|cx| Buffer::local(empty_string, cx)), cx)
        });

        let editor = cx.new(|cx| {
            let mut editor = Editor::new(
                editor::EditorMode::AutoHeight {
                    min_lines: 1,
                    max_lines: Some(4),
                },
                buffer,
                None,
                window,
                cx,
            );
            editor.set_placeholder_text(
                "What went wrong? Share your feedback so we can improve.",
                window,
                cx,
            );
            editor
        });

        editor.read(cx).focus_handle(cx).focus(window, cx);
        editor
    }
}

struct GeneratingSpinner {
    variant: SpinnerVariant,
}

impl GeneratingSpinner {
    fn new(variant: SpinnerVariant) -> Self {
        Self { variant }
    }
}

impl Render for GeneratingSpinner {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        SpinnerLabel::with_variant(self.variant).size(LabelSize::Small)
    }
}

#[derive(IntoElement)]
struct GeneratingSpinnerElement {
    variant: SpinnerVariant,
}

impl GeneratingSpinnerElement {
    fn new(variant: SpinnerVariant) -> Self {
        Self { variant }
    }
}

impl RenderOnce for GeneratingSpinnerElement {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let id = match self.variant {
            SpinnerVariant::Dots => "generating-spinner-view",
            SpinnerVariant::Sand => "confirmation-spinner-view",
            _ => "spinner-view",
        };
        window.with_id(id, |window| {
            window.use_state(cx, |_, _| GeneratingSpinner::new(self.variant))
        })
    }
}

pub enum AcpThreadViewEvent {
    Interacted,
}

impl EventEmitter<AcpThreadViewEvent> for ThreadView {}

/// `cat -n`-style numbered code block, already stripped of its line-number
/// prefixes and ready to render. Line numbers are guaranteed to be contiguous
/// starting at `first_number`, so we only store the first number and the line
/// count rather than allocating a per-line `Vec`.
struct ParsedCatNumberedCode {
    code: String,
    first_number: u32,
    line_count: usize,
}

fn parse_cat_numbered_markdown_code_block(markdown: &str) -> Option<ParsedCatNumberedCode> {
    let (_tag, code) = parse_single_fenced_code_block(markdown)?;
    parse_cat_numbered_code(code)
}

fn parse_single_fenced_code_block(markdown: &str) -> Option<(&str, &str)> {
    let first_non_backtick = markdown.find(|character| character != '`')?;
    if first_non_backtick < 3 {
        return None;
    }

    let fence = &markdown[..first_non_backtick];
    let after_opening_fence = &markdown[first_non_backtick..];
    let tag_end = after_opening_fence.find('\n')?;
    let tag = &after_opening_fence[..tag_end];
    let after_tag = &after_opening_fence[tag_end + 1..];
    let closing_fence = format!("\n{fence}\n");
    let code = after_tag.strip_suffix(&closing_fence)?;
    Some((tag, code))
}

/// Walks `code` exactly once: for each line it validates and strips the
/// `NNN\t` prefix, then pushes the line's content into the accumulating
/// code buffer (with `\n` between lines, no trailing newline). Verifies that
/// the line numbers form a contiguous, increasing sequence.
fn parse_cat_numbered_code(code: &str) -> Option<ParsedCatNumberedCode> {
    if code.is_empty() {
        return None;
    }

    let mut output = String::with_capacity(code.len());
    let mut first_number = None;
    let mut expected_number = None;
    let mut line_count: usize = 0;
    for raw_line in code.split_inclusive('\n') {
        let line = strip_line_ending(raw_line);
        let (number, text) = parse_cat_numbered_line(line)?;
        if let Some(expected) = expected_number {
            if number != expected {
                return None;
            }
        } else {
            first_number = Some(number);
        }
        expected_number = number.checked_add(1);
        if line_count > 0 {
            output.push('\n');
        }
        output.push_str(text);
        line_count += 1;
    }

    Some(ParsedCatNumberedCode {
        code: output,
        first_number: first_number?,
        line_count,
    })
}

fn strip_line_ending(line: &str) -> &str {
    let without_lf = line.strip_suffix('\n').unwrap_or(line);
    without_lf.strip_suffix('\r').unwrap_or(without_lf)
}

fn parse_cat_numbered_line(line: &str) -> Option<(u32, &str)> {
    let (prefix, text) = line.split_once('\t')?;
    let number = prefix.trim();
    if number.is_empty()
        || !prefix
            .chars()
            .all(|character| character == ' ' || character.is_ascii_digit())
    {
        return None;
    }

    Some((number.parse().ok()?, text))
}

fn render_cat_numbered_code_block(
    parsed: ParsedCatNumberedCode,
    language: Option<Arc<Language>>,
    markdown_style: MarkdownStyle,
    copy_button_id: String,
    cx: &App,
) -> AnyElement {
    use std::fmt::Write as _;

    let ParsedCatNumberedCode {
        code,
        first_number,
        line_count,
    } = parsed;

    // Line numbers are contiguous (verified during parsing), so the largest
    // line number is `first_number + line_count - 1`. Sizing the gutter to
    // that number's digit count means every rendered line contributes exactly
    // `gutter_width` bytes to the gutter, plus a newline between adjacent
    // lines.
    let last_number = first_number
        .saturating_add(u32::try_from(line_count.saturating_sub(1)).unwrap_or(u32::MAX));
    let gutter_width = last_number.to_string().len().max(1);
    let gutter_capacity = line_count * gutter_width + line_count.saturating_sub(1);

    let mut gutter = String::with_capacity(gutter_capacity);
    for i in 0..line_count {
        if i > 0 {
            gutter.push('\n');
        }
        let line_number = first_number.saturating_add(u32::try_from(i).unwrap_or(u32::MAX));
        // Writes to a `String` are infallible, so the `Result` can be ignored.
        let _ = write!(&mut gutter, "{line_number:>gutter_width$}");
    }

    let mut code_text_style = markdown_style.base_text_style.clone();
    code_text_style.refine(&markdown_style.code_block.text);

    let mut gutter_text_style = code_text_style.clone();
    gutter_text_style.color = cx.theme().colors().text_muted;

    let gutter_len = gutter.len();
    let gutter = StyledText::new(gutter).with_runs(vec![gutter_text_style.to_run(gutter_len)]);

    // Share `code` between syntax highlighting, the rendered `StyledText`, and
    // the copy button via a single `SharedString` (cheap `Arc` clones) instead
    // of cloning the underlying `String`.
    let code: SharedString = code.into();
    let code_runs = highlight_code_runs(&code, language.as_ref(), code_text_style, &markdown_style);
    let code_text = StyledText::new(code.clone()).with_runs(code_runs);

    let code_block_id = format!("read-file-code-block-{copy_button_id}");
    let code_scroll_id = format!("read-file-code-scroll-{copy_button_id}");
    let mut container = div()
        .id(code_block_id)
        .group("read-file-code-block")
        .relative()
        .w_full()
        .whitespace_nowrap();
    container.style().refine(&markdown_style.code_block);

    // `overflow_x_scroll` only actually scrolls when the container is laid out
    // as a flex container: in GPUI the default `Display` is `Block`, and a
    // block-level child fills its parent's content width instead of overflowing
    // it, so there is nothing for the scroll viewport to scroll. Using `flex()`
    // on the scroll wrapper plus `flex_none()` on the inner item lets the inner
    // item take its natural width (the unwrapped code), which is what overflows.
    // `restrict_scroll_to_axis` then keeps vertical wheel events flowing through
    // to the outer thread scroller. This mirrors the standard markdown
    // code-block path in `crates/markdown/src/markdown.rs`.
    let code_scroll = div()
        .id(code_scroll_id)
        .flex()
        .flex_1()
        .min_w_0()
        .overflow_x_scroll()
        .restrict_scroll_to_axis()
        .child(div().flex_none().child(code_text));

    container
        .child(
            h_flex()
                .items_start()
                .min_w_0()
                .w_full()
                .child(div().flex_none().pr_3().child(gutter))
                .child(code_scroll),
        )
        .child(
            h_flex()
                .w_4()
                .absolute()
                .top_0()
                .right_0()
                .justify_end()
                .visible_on_hover("read-file-code-block")
                .child(CopyButton::new(copy_button_id, code).tooltip_label("Copy Code")),
        )
        .into_any_element()
}

fn highlight_code_runs(
    code: &str,
    language: Option<&Arc<Language>>,
    code_text_style: TextStyle,
    markdown_style: &MarkdownStyle,
) -> Vec<TextRun> {
    if code.is_empty() {
        return Vec::new();
    }

    let Some(language) = language else {
        return vec![code_text_style.to_run(code.len())];
    };

    let mut runs = Vec::new();
    let mut offset = 0;
    for (range, highlight_id) in language.highlight_text(&Rope::from(code), 0..code.len()) {
        if range.start > offset {
            runs.push(code_text_style.to_run(range.start - offset));
        }

        let mut run_style = code_text_style.clone();
        if let Some(highlight) = markdown_style.syntax.get(highlight_id).cloned() {
            run_style = run_style.highlight(highlight);
        }
        runs.push(run_style.to_run(range.len()));
        offset = range.end;
    }

    if offset < code.len() {
        runs.push(code_text_style.to_run(code.len() - offset));
    }

    runs
}

#[cfg(test)]
mod numbered_code_block_tests {
    use super::*;

    #[test]
    fn parses_cat_numbered_markdown_code_block() {
        let parsed = parse_cat_numbered_markdown_code_block(
            "```rs zed/crates/example.rs\n     2\tfn main() {\n     3\t    println!(\"hi\");\n     4\t}\n```\n",
        )
        .expect("cat-numbered block should parse");

        assert_eq!(parsed.line_count, 3);
        assert_eq!(parsed.first_number, 2);
        assert_eq!(parsed.code, "fn main() {\n    println!(\"hi\");\n}");
    }

    #[test]
    fn parses_cat_numbered_code_with_crlf_line_endings() {
        let parsed = parse_cat_numbered_code("     1\tline one\r\n     2\tline two\r\n")
            .expect("crlf-terminated cat-numbered code should parse");

        assert_eq!(parsed.line_count, 2);
        assert_eq!(parsed.first_number, 1);
        assert_eq!(parsed.code, "line one\nline two");
    }

    #[test]
    fn rejects_non_cat_numbered_code_block() {
        assert!(parse_cat_numbered_markdown_code_block("```rs\nfn main() {}\n```\n").is_none());
    }

    #[test]
    fn rejects_non_contiguous_cat_numbers() {
        assert!(
            parse_cat_numbered_markdown_code_block(
                "```rs\n     2\tlet a = 1;\n     4\tlet b = 2;\n```\n"
            )
            .is_none()
        );
    }
}

/// Tracks the user's permission dropdown selection state for a specific tool call.
///
/// Default (no entry in the map) means the last dropdown choice is selected,
/// which is typically "Only this time".
#[derive(Clone)]
pub(crate) enum PermissionSelection {
    /// A specific choice from the dropdown (e.g., "Always for terminal", "Only this time").
    /// The index corresponds to the position in the `choices` list from `PermissionOptions`.
    Choice(usize),
    /// "Select options…" mode where individual command patterns can be toggled.
    /// Contains the indices of checked patterns in the `patterns` list.
    /// All patterns start checked when this mode is first activated.
    SelectedPatterns(Vec<usize>),
}

impl PermissionSelection {
    /// Returns the choice index if a specific dropdown choice is selected,
    /// or `None` if in per-command pattern mode.
    pub(crate) fn choice_index(&self) -> Option<usize> {
        match self {
            Self::Choice(index) => Some(*index),
            Self::SelectedPatterns(_) => None,
        }
    }

    fn is_pattern_checked(&self, index: usize) -> bool {
        match self {
            Self::SelectedPatterns(checked) => checked.contains(&index),
            _ => false,
        }
    }

    fn has_any_checked_patterns(&self) -> bool {
        match self {
            Self::SelectedPatterns(checked) => !checked.is_empty(),
            _ => false,
        }
    }

    fn toggle_pattern(&mut self, index: usize) {
        if let Self::SelectedPatterns(checked) = self {
            if let Some(pos) = checked.iter().position(|&i| i == index) {
                checked.swap_remove(pos);
            } else {
                checked.push(index);
            }
        }
    }
}

pub struct ThreadView {
    pub(crate) root_thread_id: ThreadId,
    pub session_id: acp::SessionId,
    pub parent_session_id: Option<acp::SessionId>,
    pub thread: Entity<AcpThread>,
    pub(crate) conversation: Entity<super::Conversation>,
    pub server_view: WeakEntity<ConversationView>,
    pub agent_icon: IconName,
    pub agent_icon_from_external_svg: Option<SharedString>,
    pub agent_id: AgentId,
    pub agent_display_name: SharedString,
    pub focus_handle: FocusHandle,
    pub workspace: WeakEntity<Workspace>,
    pub entry_view_state: Entity<EntryViewState>,
    pub title_editor: Entity<Editor>,
    pub config_options_view: Option<Entity<ConfigOptionsView>>,
    pub mode_selector: Option<Entity<ModeSelector>>,
    pub model_selector: Option<Entity<ModelSelectorPopover>>,
    pub profile_selector: Option<Entity<ProfileSelector>>,
    pub permission_dropdown_handle: PopoverMenuHandle<ContextMenu>,
    pub thread_retry_status: Option<RetryStatus>,
    pub(super) thread_error: Option<ThreadError>,
    pub thread_error_markdown: Option<Entity<Markdown>>,
    pub token_limit_callout_dismissed: bool,
    pub last_token_limit_telemetry: Option<acp_thread::TokenUsageRatio>,
    thread_feedback: ThreadFeedbackState,
    pub list_state: ListState,
    pub session_capabilities: SharedSessionCapabilities,
    pub expanded_tool_call_raw_inputs: HashSet<acp::ToolCallId>,
    collapsed_sandbox_authorization_details: HashSet<acp::ToolCallId>,
    collapsed_sandbox_network_details: HashSet<acp::ToolCallId>,
    /// Sandbox escalation prompts whose "surprising Unicode" warning the user
    /// has explicitly acknowledged. Until a prompt's tool call is in this set,
    /// its allow buttons stay disabled. See [`Self::sandbox_confusable_findings`].
    acknowledged_confusable_warnings: HashSet<acp::ToolCallId>,
    pub subagent_scroll_handles: RefCell<HashMap<acp::SessionId, ScrollHandle>>,
    pub edits_expanded: bool,
    pub plan_expanded: bool,
    /// Finished subagents the user has cleared from the tray. Only ever
    /// affects the tray: the subagent's card stays in the transcript and its
    /// thread stays open.
    dismissed_subagents: HashSet<acp::SessionId>,
    /// Starts expanded, unlike the other activity-bar sections: a subagent is
    /// work happening out of sight, so the point of the tray is that you don't
    /// have to go looking for it.
    pub subagents_expanded: bool,
    pub queue_expanded: bool,
    pub editor_expanded: bool,
    pub should_be_following: bool,
    pub editing_message: Option<usize>,
    pub message_queue: MessageQueue,
    pub turn_fields: TurnFields,
    pub discarded_partial_edits: HashSet<acp::ToolCallId>,
    pub is_loading_contents: bool,
    pub new_server_version_available: Option<SharedString>,
    pub resumed_without_history: bool,
    pub(crate) permission_selections: HashMap<acp::ToolCallId, PermissionSelection>,
    elicitation_form_states: HashMap<ElicitationEntryId, ElicitationFormState>,
    pub _cancel_task: Option<Task<()>>,
    _save_task: Option<Task<()>>,
    _draft_resolve_task: Option<Task<()>>,
    _sandbox_status_refresh_task: Option<Task<()>>,
    pub hovered_edited_file_buttons: Option<usize>,
    pub in_flight_prompt: Option<Vec<acp::ContentBlock>>,
    pub _subscriptions: Vec<Subscription>,
    pub message_editor: Entity<MessageEditor>,
    pub add_context_menu_handle: PopoverMenuHandle<ContextMenu>,
    pub thinking_effort_menu_handle: PopoverMenuHandle<ContextMenu>,
    pub fast_mode_menu_handle: PopoverMenuHandle<ContextMenu>,
    pub project: WeakEntity<Project>,
    /// Cache + worktree snapshot for resolving paths in markdown code spans.
    /// Cloned from the parent `ConversationView` so the cache is shared and the
    /// snapshot stays in sync via the parent's project-event subscription.
    pub(crate) code_span_resolver: AgentCodeSpanResolver,
    pub show_external_source_prompt_warning: bool,
    pub show_codex_windows_warning: bool,
    sandbox_status: Option<VerifiedSandboxStatus>,
    sandbox_status_key: Option<SandboxStatusKey>,
    pending_sandbox_status_key: Option<SandboxStatusKey>,
    pub multi_root_callout_dismissed: bool,
    pub generating_indicator_in_list: bool,
    pub skill_loading_issues: Vec<SkillLoadingIssue>,
    /// Issues the user has explicitly dismissed. Each entry is matched against
    /// emitted issues by full equality; when an issue no longer appears in the
    /// latest replacement list (because the underlying file was fixed/removed), it's
    /// dropped from this set so a future regression of the same kind would
    /// re-show.
    dismissed_skill_loading_issues: HashSet<SkillLoadingIssue>,
    pub(crate) thread_search_bar: Option<Entity<super::thread_search_bar::ThreadSearchBar>>,
    pub(crate) thread_search_visible: bool,
    /// Text-to-speech for assistant prose. `None` until the API key resolves,
    /// and forever when read aloud is disabled, keyless, or has no audio device.
    read_aloud: Option<Entity<read_aloud::ReadAloud>>,
    /// Entry count when the read-aloud subscription was installed. Entries
    /// below it are restored history or previous turns — auto-play only ever
    /// speaks prose that starts streaming while this view is live, so those
    /// are never enqueued (explicit clicks and buttons still play them).
    read_aloud_watermark: usize,
    /// The last-applied read-aloud settings, diffed against the global on
    /// every settings change so only what actually changed is re-applied.
    read_aloud_settings: read_aloud::ReadAloudSettings,
    /// The last-applied `agent_panel_styling` settings, diffed the same way.
    /// Render paths read this snapshot instead of the global.
    agent_panel_styling: AgentPanelStylingSettings,
    /// Subscriptions owned by the current read-aloud activation (the thread
    /// auto-play subscription and the reader observation). Scoped apart from
    /// `_subscriptions` so disabling read aloud drops them and a later
    /// re-enable does not stack duplicates.
    read_aloud_subscriptions: Vec<Subscription>,
    /// The in-flight activation (API-key resolution). Held so a mid-flight
    /// disable cancels it instead of letting it install a reader afterwards.
    read_aloud_activation: Option<Task<()>>,
    /// The concrete Inworld provider, kept alongside the type-erased handle
    /// inside `ReadAloud` so a settings change can re-target its voice.
    read_aloud_provider: Option<Arc<read_aloud::InworldTts>>,
    /// Set while a message relayed to this subagent has not yet produced a
    /// reply, so it reads as working rather than as the "done" its long-since
    /// completed spawn call reports.
    awaiting_subagent_reply: bool,
    /// The system-voice provider, when `read_aloud.provider` selects it. Held
    /// for the same reason as `read_aloud_provider`: applying a voice change
    /// without rebuilding the reader.
    #[cfg(target_os = "macos")]
    read_aloud_system_provider: Option<Arc<read_aloud::SystemTts>>,
    /// The provider's voice catalog, fetched lazily the first time the voice
    /// menu opens and cached for this view's lifetime.
    read_aloud_voices: Option<Vec<read_aloud::TtsVoice>>,
    read_aloud_voices_task: Option<Task<()>>,
    /// What narration knows about each tool call of the current turn. A tool
    /// call's real label — the path, the command — arrives *after* the entry
    /// does, so narration has to wait for it; this is what tells "not said
    /// yet" from "already said" without re-narrating a call whose label is
    /// later refined again. Cleared at the start of every turn.
    read_aloud_tool_calls: HashMap<acp::ToolCallId, ToolCallNarration>,
    /// Decides that tool labels have stopped changing. One timer for all of
    /// them: a burst arrives together and settles together, and the step
    /// they belong to batches them anyway.
    read_aloud_label_settle: Option<Task<()>>,
    /// Whether the "no summary model" toast has been shown. Once per view,
    /// like the log line it replaces — a toast per message would be worse
    /// than the silence it is fixing.
    read_aloud_warned_no_summary_model: bool,
    /// Whether the "the summary model is not answering" toast has been shown.
    read_aloud_warned_model_failing: bool,
    /// Where the last *delivered* catch-up left off: the entry count as of the
    /// press whose summary the listener actually heard. Everything at or after
    /// it is what they have not been told about.
    ///
    /// An entry index rather than an accumulated log, because the thread
    /// already remembers every tool call of the session with its input, its
    /// output and its status, and a second copy could only drift from it. It
    /// starts at zero rather than at the view's own watermark: the first
    /// press is meant to cover the session, including the part of it that was
    /// restored from history.
    read_aloud_catch_up_watermark: usize,
    /// Calls that were still running when the watermark passed them. They sit
    /// below it but have not finished happening, so they are pulled back into
    /// the next span — otherwise a `pnpm test` that was running at the press
    /// and fails a minute later is never reported at all, which is a hole in
    /// the one guarantee this feature exists for.
    read_aloud_catch_up_unfinished: HashSet<acp::ToolCallId>,
    /// When that watermark was set, so the catch-up's length can be sized by
    /// how long the listener has been away as well as by how much happened.
    /// `None` before the first press — the span is then the session, whose
    /// start this view did not see.
    ///
    /// On the executor's clock, not the wall's, so a test can move it.
    read_aloud_catch_up_since: Option<Instant>,
    /// The span a catch-up is being generated for, held until it is heard.
    ///
    /// The watermark advances on **delivery**, not on the press. A catch-up
    /// can be cancelled — a stop, a glance at another thread — and drops
    /// without making a sound; advancing at the press meant those entries were
    /// silently consumed, so coming back and pressing again answered "nothing
    /// new" about forty entries nobody ever heard. Cancelling in the middle of
    /// looking at a second session is the brief's own headline case.
    read_aloud_catch_up_in_flight: Option<CatchUpInFlight>,
}

/// The watermark a catch-up will move to if it is heard. See
/// [`ThreadView::read_aloud_catch_up_in_flight`].
struct CatchUpInFlight {
    watermark: usize,
    unfinished: HashSet<acp::ToolCallId>,
    asked_at: Instant,
}

/// How long a tool call's label must hold still before narration believes
/// it. A title is recomputed from the model's partial tool input on every
/// delta, so one that is still arriving keeps changing; see
/// [`ThreadView::note_read_aloud_tool_call`] for why no other signal works.
const TOOL_LABEL_SETTLE: Duration = Duration::from_millis(350);

/// One tool call's narration state.
struct ToolCallNarration {
    /// What narration would say this call is about, as of the last time it
    /// was looked at. A key equal to it after [`TOOL_LABEL_SETTLE`] has
    /// passed is one that has stopped arriving.
    ///
    /// The resolved key rather than the raw title, because the title is not
    /// what gets spoken: an edit's title and its `raw_input` path stream
    /// together, and watching the thing narration actually reads is what
    /// makes quiescence mean "the line is final".
    last_key: String,
    narrated: bool,
}

/// What a catch-up says when nothing has happened since the last press.
///
/// Short on purpose, and not the previous summary again: pressing a button and
/// hearing the same paragraph twice leaves no way to tell "nothing happened"
/// from "the button did nothing".
pub(crate) const NOTHING_NEW_SINCE_LAST_CATCH_UP: &str = "Nothing new since the last catch-up.";

/// Identifies the "read aloud is disabled" toast so repeat showings replace one
/// another instead of stacking, even across thread views.
struct ReadAloudDisabled;

/// Identifies the "narration has no working summary model" toast, deduped the
/// same way.
struct ReadAloudNarrationDegraded;

/// What a `NewEntry` event brought in, as far as narration cares.
enum NewEntryKind {
    UserMessage,
    ToolCall,
    Other,
}

/// Maps the agent protocol's tool kinds onto the families narration counts a
/// burst by, so the read-aloud crate needs no dependency on the protocol.
fn narration_kind(kind: &acp::ToolKind) -> read_aloud::NarrationKind {
    match kind {
        acp::ToolKind::Read => read_aloud::NarrationKind::Read,
        acp::ToolKind::Edit => read_aloud::NarrationKind::Edit,
        acp::ToolKind::Delete => read_aloud::NarrationKind::Delete,
        acp::ToolKind::Move => read_aloud::NarrationKind::Move,
        acp::ToolKind::Search => read_aloud::NarrationKind::Search,
        acp::ToolKind::Execute => read_aloud::NarrationKind::Execute,
        acp::ToolKind::Fetch => read_aloud::NarrationKind::Fetch,
        _ => read_aloud::NarrationKind::Other,
    }
}

/// What narration is told about one tool call.
///
/// The title is the *last* thing consulted. Zed's own tools put the command
/// or the path straight into it, but an external ACP agent writes its own
/// titles and Claude Code's are generic — "Terminal", "Read file" — with the
/// command and the path only in `raw_input` and `locations`. Narration built
/// on titles said "running terminal" once and then went silent for the rest
/// of the turn, because every following call had the same title.
///
/// `raw_input` is the agent's own tool schema, so the key it uses is a
/// convention rather than a contract; [`read_aloud::RawToolInput`] probes the
/// common spellings and anything unrecognised degrades to the title.
fn read_aloud_tool_call_facts(tool_call: &acp_thread::ToolCall) -> read_aloud::ToolCallFacts {
    let raw = read_aloud::RawToolInput::from_json(tool_call.raw_input.as_ref());
    // ACP populates `locations` independently of the title, so a call whose
    // input schema is unrecognised can still name its file.
    let path = raw.path.or_else(|| {
        tool_call
            .locations
            .first()
            .map(|location| location.path.to_string_lossy().into_owned())
            .filter(|path| !path.trim().is_empty())
    });
    read_aloud::ToolCallFacts {
        id: tool_call.id.0.to_string(),
        kind: narration_kind(&tool_call.kind),
        label: tool_call.label.clone(),
        purpose: raw.purpose,
        command: raw.command,
        path,
        url: raw.url,
        query: raw.query,
        // What the call found. `raw_output` is where a finished call's own
        // output lands — for Claude Code, the command's combined output, and
        // for a failure the exit code in front of it — so a summary can say
        // what came back instead of only what was attempted.
        output: read_aloud::tool_output(tool_call.raw_output.as_ref()),
        input: raw.input,
        outcome: read_aloud_tool_call_outcome(&tool_call.status),
    }
}

/// Why narration is looking at a tool call, which decides how much longer it
/// is willing to wait before speaking.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NarrationTrigger {
    /// The call arrived, or one of its fields changed.
    Updated,
    /// The settle timer expired without the call's line changing.
    Settled,
    /// The turn ended. Nothing more is coming for any call still waiting.
    TurnEnded,
}

/// Whether narration may speak this call now.
///
/// Pure so the decision is testable without a view, because getting it wrong
/// is what the listener hears.
///
/// A terminal status short-circuits every wait — no refinement can follow a
/// call that has finished — and so does the end of the turn.
///
/// Otherwise the call has to be *settled*: its line held still for
/// [`TOOL_LABEL_SETTLE`]. The exception, and the reason this function exists,
/// is a call whose payload arrived empty. Its line cannot change while the
/// payload is empty, so it is trivially "settled" the instant it arrives and
/// the timer speaks the placeholder — "running terminal". An empty payload
/// means *not ready yet*, and the wait continues until content arrives or a
/// backstop fires.
///
/// A command that shortens to a shell keyword waits for the same reason and
/// on the same terms. Claude Code fills `command` in one update and
/// `description` in the next, so the settle timer can expire on a call whose
/// only sayable form is "for p in" while the sentence that would have named
/// it is one message away. Waiting costs the call one more update; not
/// waiting is what the listener reported hearing. If the description never
/// comes, the queue declines to speak the keyword at all — see
/// [`read_aloud::ToolCallFacts::is_shell_noise`].
fn read_aloud_tool_call_is_ready(
    facts: &read_aloud::ToolCallFacts,
    status: &ToolCallStatus,
    trigger: NarrationTrigger,
    cx: &App,
) -> bool {
    if trigger == NarrationTrigger::TurnEnded
        || matches!(status, ToolCallStatus::Completed | ToolCallStatus::Failed)
    {
        return true;
    }
    trigger == NarrationTrigger::Settled && !facts.awaiting_input() && !facts.is_shell_noise(cx)
}

/// The rungs [`ThreadView::read_aloud_summary_model`] walks, as a function of
/// the registry alone so the order is directly testable. See that method for
/// why this order.
fn resolve_read_aloud_summary_model(
    registry: &LanguageModelRegistry,
    configured: Option<&settings::LanguageModelSelection>,
    cx: &App,
) -> Option<ConfiguredModel> {
    let usable = |model: Option<ConfiguredModel>| -> Option<ConfiguredModel> {
        model.filter(|model| model.provider.is_authenticated(cx))
    };
    usable(configured.and_then(|selection| {
        let provider =
            registry.provider(&LanguageModelProviderId::from(selection.provider.0.clone()))?;
        let model_id = LanguageModelId::from(selection.model.clone());
        let model = provider
            .provided_models(cx)
            .iter()
            .find(|model| model.id() == model_id)?
            .clone();
        Some(ConfiguredModel { provider, model })
    }))
    .or_else(|| usable(registry.thread_summary_model(cx)))
    .or_else(|| usable(registry.commit_message_model(cx)))
    .or_else(|| usable(registry.inline_assistant_model()))
}

/// Whether narration may speak *without being asked*.
///
/// `auto_play` is the setting that already means "do not start talking on your
/// own", and until now it only governed full mode's prose — narration went on
/// narrating with it off, so there was no way to have read aloud available
/// without having it running. That is precisely the arrangement the catch-up
/// button is for: `auto_play: false` plus a button is the on-demand mode, and
/// it needs no setting of its own. Everything explicit — the toggle, the
/// per-message speakers, a sentence click, [`read_aloud::SummarizeSession`] —
/// deliberately does not consult this.
fn read_aloud_narrates_unasked(cx: &App) -> bool {
    let settings = read_aloud::ReadAloudSettings::get_global(cx);
    settings.auto_play && settings.mode == read_aloud::ReadAloudMode::Narration
}

fn read_aloud_tool_call_outcome(status: &ToolCallStatus) -> read_aloud::ToolCallOutcome {
    match status {
        ToolCallStatus::Completed => read_aloud::ToolCallOutcome::Succeeded,
        ToolCallStatus::Failed => read_aloud::ToolCallOutcome::Failed,
        _ => read_aloud::ToolCallOutcome::Pending,
    }
}

/// Runs narration's message summaries through Zed's model registry. Held by
/// the reader, which owns the cancellation: dropping its task drops this
/// request with it.
struct ReadAloudSummaryModel {
    model: Arc<dyn LanguageModel>,
    provider: Arc<dyn LanguageModelProvider>,
    temperature: Option<f32>,
}

impl read_aloud::SummaryModel for ReadAloudSummaryModel {
    fn complete(&self, prompt: String, cx: &mut App) -> Task<anyhow::Result<String>> {
        let model = self.model.clone();
        let provider = self.provider.clone();
        let temperature = self.temperature;
        cx.spawn(async move |cx| {
            if let Some(authenticate) =
                cx.update(|cx| (!provider.is_authenticated(cx)).then(|| provider.authenticate(cx)))
            {
                // A failure here is not fatal on its own — the completion
                // below reports the real problem — but it is worth seeing.
                authenticate.await.log_err();
            }
            let request = LanguageModelRequest {
                thread_id: None,
                prompt_id: None,
                intent: Some(CompletionIntent::ThreadSummarization),
                messages: vec![LanguageModelRequestMessage {
                    role: Role::User,
                    content: vec![prompt.into()],
                    cache: false,
                    reasoning_details: None,
                }],
                tools: Vec::new(),
                tool_choice: None,
                stop: Vec::new(),
                temperature,
                thinking_allowed: false,
                thinking_effort: None,
                speed: None,
                compact_at_tokens: None,
            };
            let mut completion = model.stream_completion_text(request, cx).await?;
            let mut summary = String::new();
            while let Some(chunk) = completion.stream.next().await {
                summary.push_str(&chunk?);
            }
            Ok(summary)
        })
    }
}
impl Focusable for ThreadView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ThreadView {
    pub(crate) fn activation_focus_handle(&self, cx: &App) -> FocusHandle {
        if self.parent_session_id.is_some() {
            self.focus_handle.clone()
        } else {
            self.active_editor(cx).focus_handle(cx)
        }
    }
}

#[derive(Default)]
pub struct TurnFields {
    pub _turn_timer_task: Option<Task<()>>,
    pub last_turn_duration: Option<Duration>,
    pub last_turn_tokens: Option<u64>,
    pub turn_generation: usize,
    pub turn_started_at: Option<Instant>,
    pub turn_tokens: Option<u64>,
}

/// How a tool call is rendered relative to its surroundings.
///
/// `Standalone` draws its own border/margin/location header. `Embedded` is
/// hosted by a container that provides its own framing (e.g. the subagent
/// card). `Floating` is like `Embedded`, but used for the floating
/// awaiting-permission row above the message editor: the tool call's content
/// is height-capped and scrollable so the row can never grow to consume the
/// entire panel and squeeze the conversation list out of view.
#[derive(Copy, Clone, PartialEq, Eq)]
enum ToolCallLayout {
    Standalone,
    Embedded,
    Floating,
}

impl ToolCallLayout {
    /// Stable discriminant used to disambiguate element ids when the same tool
    /// call is rendered in more than one layout at once (e.g. inline in the
    /// list *and* in the floating awaiting-permission row).
    fn id_str(self) -> &'static str {
        match self {
            ToolCallLayout::Standalone => "standalone",
            ToolCallLayout::Embedded => "embedded",
            ToolCallLayout::Floating => "floating",
        }
    }
}

fn full_path_for_empty_project_path(file: &dyn language::File, cx: &App) -> Option<String> {
    if file.path().file_name().is_some() {
        return None;
    }

    let full_path = file.full_path(cx).display().to_string();
    (!full_path.is_empty()).then_some(full_path)
}

fn skill_issue_file_label(path: &std::path::Path) -> String {
    let file_name = path.file_name().and_then(|name| name.to_str());
    let parent_name = path
        .parent()
        .and_then(|parent| parent.file_name())
        .and_then(|name| name.to_str());

    match (parent_name, file_name) {
        (Some(parent_name), Some(file_name)) => format!("{parent_name}/{file_name}"),
        (_, Some(file_name)) => file_name.to_string(),
        _ => path.display().to_string(),
    }
}

pub fn open_markdown_in_workspace(
    title: String,
    markdown: String,
    workspace: Entity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) -> Task<Result<()>> {
    let markdown_language_task = workspace
        .read(cx)
        .app_state()
        .languages
        .language_for_name("Markdown");
    let project = workspace.read(cx).project().clone();

    window.spawn(cx, async move |cx| {
        let markdown_language = markdown_language_task.await?;

        let buffer = project
            .update(cx, |project, cx| {
                project.create_buffer(Some(markdown_language), false, cx)
            })
            .await?;

        buffer.update(cx, |buffer, cx| {
            buffer.set_text(markdown, cx);
            buffer.set_capability(language::Capability::ReadWrite, cx);
        });

        workspace.update_in(cx, |workspace, window, cx| {
            let buffer = cx.new(|cx| MultiBuffer::singleton(buffer, cx).with_title(title.clone()));

            workspace.add_item_to_active_pane(
                Box::new(cx.new(|cx| {
                    let mut editor =
                        Editor::for_multibuffer(buffer, Some(project.clone()), window, cx);
                    editor.set_breadcrumb_header(title);
                    editor.disable_mouse_wheel_zoom();
                    editor
                })),
                None,
                true,
                window,
                cx,
            );
        })?;
        anyhow::Ok(())
    })
}

impl ThreadView {
    pub(crate) fn new(
        root_thread_id: ThreadId,
        thread: Entity<AcpThread>,
        conversation: Entity<super::Conversation>,
        server_view: WeakEntity<ConversationView>,
        agent_icon: IconName,
        agent_icon_from_external_svg: Option<SharedString>,
        agent_id: AgentId,
        agent_display_name: SharedString,
        workspace: WeakEntity<Workspace>,
        entry_view_state: Entity<EntryViewState>,
        config_options_view: Option<Entity<ConfigOptionsView>>,
        mode_selector: Option<Entity<ModeSelector>>,
        model_selector: Option<Entity<ModelSelectorPopover>>,
        profile_selector: Option<Entity<ProfileSelector>>,
        list_state: ListState,
        session_capabilities: SharedSessionCapabilities,
        resumed_without_history: bool,
        project: WeakEntity<Project>,
        code_span_resolver: AgentCodeSpanResolver,
        thread_store: Option<Entity<ThreadStore>>,
        initial_content: Option<AgentInitialContent>,
        mut subscriptions: Vec<Subscription>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let session_id = thread.read(cx).session_id().clone();
        let parent_session_id = thread.read(cx).parent_session_id().cloned();

        let has_slash_completions = session_capabilities.read().has_slash_completions();
        let placeholder = placeholder_text(agent_display_name.as_ref(), has_slash_completions);

        let mut should_auto_submit = false;
        let mut show_external_source_prompt_warning = false;

        let message_editor = cx.new(|cx| {
            let mut editor = MessageEditor::new(
                workspace.clone(),
                project.clone(),
                thread_store,
                session_capabilities.clone(),
                agent_id.clone(),
                &placeholder,
                editor::EditorMode::AutoHeight {
                    min_lines: AgentSettings::get_global(cx).message_editor_min_lines,
                    max_lines: Some(AgentSettings::get_global(cx).set_message_editor_max_lines()),
                },
                window,
                cx,
            );
            if let Some(content) = initial_content {
                match content {
                    AgentInitialContent::ThreadSummary { session_id, title } => {
                        editor.insert_thread_summary(session_id, title, window, cx);
                    }
                    AgentInitialContent::ContentBlock {
                        blocks,
                        auto_submit,
                    } => {
                        should_auto_submit = auto_submit;
                        editor.set_message(blocks, window, cx);
                    }
                    AgentInitialContent::FromExternalSource(prompt) => {
                        show_external_source_prompt_warning = true;
                        // SECURITY: Be explicit about not auto submitting prompt from external source.
                        should_auto_submit = false;
                        editor.set_message(
                            vec![acp::ContentBlock::Text(acp::TextContent::new(
                                prompt.into_string(),
                            ))],
                            window,
                            cx,
                        );
                    }
                }
            } else if let Some(draft) = thread.read(cx).draft_prompt() {
                editor.set_message(draft.to_vec(), window, cx);
            }
            editor
        });

        let show_codex_windows_warning = cfg!(windows)
            && project.upgrade().is_some_and(|p| p.read(cx).is_local())
            && agent_id.as_ref() == "Codex";

        if let Some(project) = project.upgrade() {
            subscriptions.push(cx.subscribe(&project, {
                let resolver = code_span_resolver.clone();
                move |_this: &mut Self, _project, event: &project::Event, cx| {
                    if matches!(
                        event,
                        project::Event::WorktreeAdded(_)
                            | project::Event::WorktreeRemoved(_)
                            | project::Event::WorktreeUpdatedEntries(_, _)
                    ) {
                        resolver.clear_cache();
                        cx.notify();
                    }
                }
            }));
        }

        let title_editor = {
            let metadata = ThreadMetadataStore::try_global(cx)
                .and_then(|store| store.read(cx).entry(root_thread_id).cloned());
            let initial_title = if parent_session_id.is_none() {
                metadata.as_ref().and_then(|m| m.title())
            } else {
                thread.read(cx).title()
            }
            .unwrap_or_else(|| DEFAULT_THREAD_TITLE.into());
            let editor = cx.new(|cx| {
                let mut editor = Editor::single_line(window, cx);
                editor.set_text(initial_title, window, cx);
                editor
            });
            subscriptions.push(cx.subscribe_in(&editor, window, Self::handle_title_editor_event));
            editor
        };

        subscriptions.push(cx.subscribe_in(
            &entry_view_state,
            window,
            Self::handle_entry_view_event,
        ));

        subscriptions.push(cx.subscribe_in(
            &message_editor,
            window,
            Self::handle_message_editor_event,
        ));

        // If this thread is backed by a NativeAgent, listen for skill loading
        // issues so we can surface them as banners. The agent emits a single
        // replacement-style event per project refresh, so we overwrite our
        // local list rather than appending — this also clears stale issues
        // once a user resolves them.
        if let Some(native_connection) = thread
            .read(cx)
            .connection()
            .clone()
            .downcast::<agent::NativeAgentConnection>()
        {
            let project_id = thread.read(cx).project().entity_id();
            subscriptions.push(cx.subscribe(
                &native_connection.0,
                move |this: &mut Self, _agent, event: &SkillLoadingIssuesUpdated, cx| {
                    if event.project_id != project_id {
                        return;
                    }
                    // Drop dismissals for issues that no longer appear in the emitted
                    // list — the underlying file must have been fixed or removed, so a
                    // future regression should re-show.
                    this.dismissed_skill_loading_issues
                        .retain(|dismissed| event.issues.contains(dismissed));

                    // Show only issues that haven't been dismissed.
                    this.skill_loading_issues = event
                        .issues
                        .iter()
                        .filter(|issue| !this.dismissed_skill_loading_issues.contains(issue))
                        .cloned()
                        .collect();
                    cx.notify();
                },
            ));

            // A "no model selected" error is stale as soon as the thread has a
            // usable model
            if let Some(native_thread) = native_connection.thread(thread.read(cx).session_id(), cx)
            {
                subscriptions.push(cx.subscribe(
                    &native_thread,
                    |this: &mut Self, _thread, _event: &agent::ModelChanged, cx| {
                        if matches!(this.thread_error, Some(ThreadError::NoModelSelected)) {
                            this.clear_thread_error(cx);
                        }
                    },
                ));
            }
        }

        subscriptions.push(cx.observe(&message_editor, |this, editor, cx| {
            let is_empty = editor.read(cx).text(cx).is_empty();
            let draft_contents_task = if is_empty {
                None
            } else {
                Some(editor.update(cx, |editor, cx| editor.draft_contents(cx)))
            };
            this._draft_resolve_task = Some(cx.spawn(async move |this, cx| {
                let draft = if let Some(task) = draft_contents_task {
                    let blocks = task.await.ok().filter(|b| !b.is_empty());
                    blocks
                } else {
                    None
                };
                this.update(cx, |this, cx| {
                    this.thread.update(cx, |thread, cx| {
                        thread.set_draft_prompt(draft, cx);
                    });
                    this.schedule_save(cx);
                })
                .ok();
            }));
        }));

        let mut this = Self {
            root_thread_id,
            session_id,
            parent_session_id,
            focus_handle: cx.focus_handle(),
            thread,
            conversation,
            server_view,
            agent_icon,
            agent_icon_from_external_svg,
            agent_id,
            agent_display_name,
            workspace,
            entry_view_state,
            title_editor,
            config_options_view,
            mode_selector,
            model_selector,
            profile_selector,
            list_state,
            session_capabilities,
            resumed_without_history,
            _subscriptions: subscriptions,
            permission_dropdown_handle: PopoverMenuHandle::default(),
            thread_retry_status: None,
            thread_error: None,
            thread_error_markdown: None,
            token_limit_callout_dismissed: false,
            last_token_limit_telemetry: None,
            thread_feedback: Default::default(),
            expanded_tool_call_raw_inputs: HashSet::default(),
            collapsed_sandbox_authorization_details: HashSet::default(),
            collapsed_sandbox_network_details: HashSet::default(),
            acknowledged_confusable_warnings: HashSet::default(),
            subagent_scroll_handles: RefCell::new(HashMap::default()),
            edits_expanded: false,
            plan_expanded: false,
            dismissed_subagents: HashSet::default(),
            subagents_expanded: true,
            queue_expanded: true,
            editor_expanded: false,
            should_be_following: false,
            editing_message: None,
            message_queue: MessageQueue::default(),
            turn_fields: TurnFields::default(),
            discarded_partial_edits: HashSet::default(),
            is_loading_contents: false,
            new_server_version_available: None,
            permission_selections: HashMap::default(),
            elicitation_form_states: HashMap::default(),
            _cancel_task: None,
            _save_task: None,
            _draft_resolve_task: None,
            _sandbox_status_refresh_task: None,
            hovered_edited_file_buttons: None,
            in_flight_prompt: None,
            message_editor,
            add_context_menu_handle: PopoverMenuHandle::default(),
            thinking_effort_menu_handle: PopoverMenuHandle::default(),
            fast_mode_menu_handle: PopoverMenuHandle::default(),
            project,
            code_span_resolver,
            show_external_source_prompt_warning,
            show_codex_windows_warning,
            sandbox_status: None,
            sandbox_status_key: None,
            pending_sandbox_status_key: None,
            multi_root_callout_dismissed: false,
            generating_indicator_in_list: false,
            skill_loading_issues: Vec::new(),
            dismissed_skill_loading_issues: HashSet::default(),
            thread_search_bar: None,
            thread_search_visible: false,
            read_aloud: None,
            read_aloud_watermark: 0,
            read_aloud_settings: read_aloud::ReadAloudSettings::get_global(cx).clone(),
            agent_panel_styling: AgentPanelStylingSettings::get_global(cx).clone(),
            read_aloud_subscriptions: Vec::new(),
            read_aloud_activation: None,
            awaiting_subagent_reply: false,
            read_aloud_provider: None,
            #[cfg(target_os = "macos")]
            read_aloud_system_provider: None,
            read_aloud_voices: None,
            read_aloud_voices_task: None,
            read_aloud_tool_calls: HashMap::default(),
            read_aloud_label_settle: None,
            read_aloud_warned_no_summary_model: false,
            read_aloud_warned_model_failing: false,
            read_aloud_catch_up_watermark: 0,
            read_aloud_catch_up_unfinished: HashSet::default(),
            read_aloud_catch_up_since: None,
            read_aloud_catch_up_in_flight: None,
        };

        this.init_read_aloud(cx);
        this._subscriptions
            .push(cx.observe_global::<SettingsStore>(|this, cx| {
                this.read_aloud_settings_changed(cx);
                this.agent_panel_styling_changed(cx);
            }));
        this.sync_generating_indicator(cx);
        this.sync_editor_mode(cx);
        this.sync_existing_elicitation_states(window, cx);
        let list_state_for_scroll = this.list_state.clone();
        let thread_view = cx.entity().downgrade();

        this.list_state
            .set_scroll_handler(move |_event, _window, cx| {
                let list_state = list_state_for_scroll.clone();
                let thread_view = thread_view.clone();
                // N.B. We must defer because the scroll handler is called while the
                // ListState's RefCell is mutably borrowed. Reading logical_scroll_top()
                // directly would panic from a double borrow.
                cx.defer(move |cx| {
                    let scroll_top = list_state.logical_scroll_top();
                    let _ = thread_view.update(cx, |this, cx| {
                        if let Some(thread) = this.as_native_thread(cx) {
                            thread.update(cx, |thread, _cx| {
                                thread.set_ui_scroll_position(Some(scroll_top));
                            });
                        }
                        this.schedule_save(cx);
                    });
                });
            });

        if should_auto_submit {
            this.send(window, cx);
        }
        this
    }

    /// Read aloud is opt-in: when it is off, nothing is subscribed, nothing is
    /// built, and the panel behaves exactly as it does upstream. The
    /// `ReadAloud` entity is only constructed once an API key resolves, so an
    /// install without one never gets a provider that cannot synthesize.
    fn init_read_aloud(&mut self, cx: &mut Context<Self>) {
        if !read_aloud::ReadAloudSettings::get_global(cx).enabled {
            return;
        }
        // Subagents get their own `ThreadView`. Only the root thread reads
        // aloud, so multiple readers never talk over one another.
        if self.parent_session_id.is_some() {
            return;
        }
        // Already active or activating: nothing to redo. Re-activation
        // always passes through `shutdown_read_aloud` first, which clears
        // both.
        if self.read_aloud.is_some() || self.read_aloud_activation.is_some() {
            return;
        }

        // Resolved before anything is built: an unimplemented `provider` used
        // to fall through to Inworld, so the setting silently did nothing.
        let provider_name = match read_aloud::ReadAloudSettings::get_global(cx).resolve_provider() {
            Ok(provider_name) => provider_name,
            Err(unsupported) => {
                self.notify_read_aloud_disabled(
                    format!(
                        "`read_aloud.provider` is set to \"{unsupported}\", which is not \
                         implemented. Supported: {}.",
                        read_aloud::SUPPORTED_PROVIDERS.join(", ")
                    ),
                    cx,
                );
                return;
            }
        };

        // The system voice needs no key and no network, so it skips the whole
        // credential dance the cloud provider has to go through first.
        #[cfg(target_os = "macos")]
        if provider_name == read_aloud::SYSTEM_PROVIDER {
            self.subscribe_read_aloud(cx);
            self.start_read_aloud_with_system_voice(cx);
            return;
        }

        self.subscribe_read_aloud(cx);

        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let http_client = workspace.read(cx).client().http_client();

        self.read_aloud_activation = Some(cx.spawn(async move |this, cx| {
            let api_key = cx.update(|cx| read_aloud::resolve_api_key(cx)).await;
            let api_key = match api_key {
                Ok(api_key) => api_key,
                Err(error) => {
                    this.update(cx, |this, cx| {
                        this.read_aloud_activation = None;
                        this.notify_read_aloud_disabled(
                            format!("no Inworld API key ({error:#})"),
                            cx,
                        );
                    })
                    .log_err();
                    return;
                }
            };

            this.update(cx, |this, cx| {
                this.read_aloud_activation = None;
                let Some(player) = audio::Audio::connect_player(cx) else {
                    this.notify_read_aloud_disabled(
                        "no audio output device available".to_string(),
                        cx,
                    );
                    return;
                };

                let settings = read_aloud::ReadAloudSettings::get_global(cx);
                let provider = Arc::new(read_aloud::InworldTts::new(
                    http_client,
                    api_key,
                    settings.voice_id.clone(),
                    settings.model_id.clone(),
                ));
                let speaking_rate = settings.speaking_rate;
                let click_to_seek = settings.click_to_seek;
                let mode = settings.mode;
                let narration_detail = settings.narration_detail;

                let read_aloud = cx.new(|cx| {
                    let mut read_aloud = read_aloud::ReadAloud::new(
                        provider.clone(),
                        Box::new(read_aloud::RodioSink::new(player)),
                        Some(Self::read_aloud_sink_recovery()),
                        cx,
                    );
                    read_aloud.set_speed(speaking_rate, cx);
                    read_aloud.set_click_to_seek(click_to_seek, cx);
                    read_aloud.set_mode(mode, cx);
                    read_aloud.set_narration_detail(narration_detail, cx);
                    read_aloud
                });
                // The mini player renders off this entity's state; it already
                // notifies on every playback transition, so observing it is
                // what keeps the controls current without another timer.
                this.watch_read_aloud(&read_aloud, cx);
                this.read_aloud_provider = Some(provider);
                this.read_aloud = Some(read_aloud);
                cx.notify();
            })
            .log_err();
        }));
    }

    /// Lets narration follow the system's output device: the sink it is playing
    /// through is bound to whichever device was current when it opened, so a
    /// switch to headphones (or away from them) otherwise leaves the audio
    /// rendering somewhere the listener is not.
    fn read_aloud_sink_recovery() -> read_aloud::SinkRecovery {
        Arc::new(|cx: &mut App, stalled: bool| {
            let player = audio::Audio::reconnect_player(cx, stalled)?;
            Some(Box::new(read_aloud::RodioSink::new(player)) as Box<dyn read_aloud::AudioSink>)
        })
    }

    /// Brings up read aloud on the platform's own synthesizer.
    ///
    /// Split from the cloud path because it shares almost none of it: no API
    /// key, no HTTP client, and no model id — just an optional voice name.
    #[cfg(target_os = "macos")]
    fn start_read_aloud_with_system_voice(&mut self, cx: &mut Context<Self>) {
        let Some(player) = audio::Audio::connect_player(cx) else {
            self.notify_read_aloud_disabled("no audio output device available".to_string(), cx);
            return;
        };

        let settings = read_aloud::ReadAloudSettings::get_global(cx);
        // `voice_id` defaults to the Inworld voice name, which `say` does not
        // have. An unknown name would make every utterance fail, so anything
        // the system does not know falls back to the user's default voice.
        let voice = Some(settings.voice_id.clone());
        let speaking_rate = settings.speaking_rate;
        let click_to_seek = settings.click_to_seek;
        let mode = settings.mode;
        let narration_detail = settings.narration_detail;

        let provider = Arc::new(read_aloud::SystemTts::new(voice));
        let read_aloud = cx.new(|cx| {
            let mut read_aloud = read_aloud::ReadAloud::new(
                provider.clone(),
                Box::new(read_aloud::RodioSink::new(player)),
                Some(Self::read_aloud_sink_recovery()),
                cx,
            );
            read_aloud.set_speed(speaking_rate, cx);
            read_aloud.set_click_to_seek(click_to_seek, cx);
            read_aloud.set_mode(mode, cx);
            read_aloud.set_narration_detail(narration_detail, cx);
            read_aloud
        });
        self.watch_read_aloud(&read_aloud, cx);
        self.read_aloud_system_provider = Some(provider);
        self.read_aloud = Some(read_aloud);
        cx.notify();
    }

    /// Re-renders when the reader changes, and speaks up when it reports
    /// that narration has stopped being the product it is supposed to be.
    fn watch_read_aloud(
        &mut self,
        read_aloud: &Entity<read_aloud::ReadAloud>,
        cx: &mut Context<Self>,
    ) {
        self.read_aloud_subscriptions
            .push(cx.observe(read_aloud, |_, _, cx| cx.notify()));
        self.read_aloud_subscriptions.push(cx.subscribe(
            read_aloud,
            |this, _, event, cx| match event {
                read_aloud::ReadAloudEvent::CaughtUp => {
                    this.read_aloud_catch_up_delivered();
                }
                read_aloud::ReadAloudEvent::SummaryModelFailing => {
                    if this.read_aloud_warned_model_failing {
                        return;
                    }
                    this.read_aloud_warned_model_failing = true;
                    this.notify_read_aloud_degraded(
                        "the summary model is not answering. Check \
                         `read_aloud.summary_model` and its provider.",
                        cx,
                    );
                }
            },
        ));
    }

    /// Applies `read_aloud.*` settings changes without a restart. Voice and
    /// model re-target the provider (the next synthesis request speaks with
    /// them), the speaking rate re-times current playback, click-to-seek
    /// re-arms the affordance, and the enabled flag activates or deactivates
    /// the whole feature, and the mode switches full ↔ narration. `auto_play`
    /// and `narrate_tool_calls` need no handling here: the enqueue and
    /// narration paths read them live, so a toggle takes effect on the next
    /// turn by itself. `summary_model` is resolved per message, for the same
    /// reason. Pill colors feed the next render directly from the global.
    fn read_aloud_settings_changed(&mut self, cx: &mut Context<Self>) {
        let settings = read_aloud::ReadAloudSettings::get_global(cx).clone();
        if settings == self.read_aloud_settings {
            return;
        }
        let previous = std::mem::replace(&mut self.read_aloud_settings, settings.clone());

        if settings.enabled != previous.enabled {
            if settings.enabled {
                self.init_read_aloud(cx);
            } else {
                self.shutdown_read_aloud(cx);
            }
            // Activation snapshots the rest of the settings itself, and
            // deactivation leaves nothing to re-apply to.
            cx.notify();
            return;
        }
        if !settings.enabled {
            return;
        }

        // Switching providers has to rebuild the reader: the provider is baked
        // into the player at construction, so re-applying settings to the old
        // one would keep speaking through it.
        if settings.provider != previous.provider {
            self.shutdown_read_aloud(cx);
            self.init_read_aloud(cx);
            cx.notify();
            return;
        }
        if settings.voice_id != previous.voice_id || settings.model_id != previous.model_id {
            if let Some(provider) = &self.read_aloud_provider {
                provider.set_voice(settings.voice_id.clone(), settings.model_id.clone());
            }
            #[cfg(target_os = "macos")]
            if let Some(provider) = &self.read_aloud_system_provider {
                provider.set_voice(Some(settings.voice_id.clone()));
            }
        }
        if let Some(read_aloud) = self.read_aloud.clone() {
            if settings.speaking_rate != previous.speaking_rate {
                read_aloud.update(cx, |read_aloud, cx| {
                    read_aloud.set_speed(settings.speaking_rate, cx);
                });
            }
            if settings.click_to_seek != previous.click_to_seek {
                read_aloud.update(cx, |read_aloud, cx| {
                    read_aloud.set_click_to_seek(settings.click_to_seek, cx);
                });
            }
            if settings.mode != previous.mode {
                read_aloud.update(cx, |read_aloud, cx| {
                    read_aloud.set_mode(settings.mode, cx);
                });
            }
            if settings.narration_detail != previous.narration_detail {
                read_aloud.update(cx, |read_aloud, cx| {
                    read_aloud.set_narration_detail(settings.narration_detail, cx);
                });
            }
        }
        cx.notify();
    }

    /// Applies `agent_panel_styling` changes without a restart. Markdown
    /// styles are rebuilt every render, so notifying this view covers
    /// assistant prose, thinking, tool output, and code blocks. The
    /// user-message editors derive their text style in their own render, so
    /// they are notified individually (the composer here, the per-entry
    /// editors via the entry view state). Diff editors re-refine through the
    /// conversation view's existing settings observation.
    fn agent_panel_styling_changed(&mut self, cx: &mut Context<Self>) {
        let settings = AgentPanelStylingSettings::get_global(cx).clone();
        if settings == self.agent_panel_styling {
            return;
        }
        self.agent_panel_styling = settings;
        self.entry_view_state.update(cx, |entry_view_state, cx| {
            entry_view_state.agent_panel_styling_changed(cx);
        });
        self.message_editor.update(cx, |_, cx| cx.notify());
        cx.notify();
    }

    /// Tears read aloud down when settings disable it: playback stops, the
    /// highlights clear, and every read-aloud surface (mini player, speaker
    /// buttons, click affordance) disappears because the entity they all
    /// render from is gone. Dropping the activation task covers the window
    /// where the API key is still resolving; dropping the subscriptions
    /// keeps a later re-enable from stacking duplicates.
    fn shutdown_read_aloud(&mut self, cx: &mut Context<Self>) {
        self.read_aloud_activation = None;
        self.read_aloud_subscriptions.clear();
        self.read_aloud_provider = None;
        #[cfg(target_os = "macos")]
        {
            self.read_aloud_system_provider = None;
        }
        if let Some(read_aloud) = self.read_aloud.take() {
            read_aloud.update(cx, |read_aloud, cx| read_aloud.dismiss(cx));
        }
    }

    /// Installs the auto-play subscription, with the watermark that scopes
    /// auto-play to this view's lifetime: entries that already exist were
    /// read (or declined) in a previous life of the session, and `NewEntry`
    /// fires for the user's own reply too — without the watermark, that
    /// reply would enqueue the newest assistant markdown, which at that
    /// moment is the *previous* turn's message.
    fn subscribe_read_aloud(&mut self, cx: &mut Context<Self>) {
        self.read_aloud_watermark = self.thread.read(cx).entries().len();
        self.read_aloud_subscriptions.push(cx.subscribe(
            &self.thread,
            |this, thread, event: &AcpThreadEvent, cx| match event {
                AcpThreadEvent::NewEntry => {
                    let (entry_index, new_entry) = {
                        let entries = thread.read(cx).entries();
                        (
                            entries.len().saturating_sub(1),
                            match entries.last() {
                                Some(AgentThreadEntry::UserMessage(_)) => NewEntryKind::UserMessage,
                                Some(AgentThreadEntry::ToolCall(_)) => NewEntryKind::ToolCall,
                                _ => NewEntryKind::Other,
                            },
                        )
                    };
                    match new_entry {
                        NewEntryKind::UserMessage => {
                            // A new turn: status the user has not heard yet
                            // is about the turn they just moved on from.
                            this.read_aloud_tool_calls.clear();
                            this.read_aloud_label_settle = None;
                            if let Some(read_aloud) = this.read_aloud.clone() {
                                read_aloud
                                    .update(cx, |read_aloud, cx| read_aloud.cancel_narration(cx));
                            }
                        }
                        NewEntryKind::ToolCall => {
                            // A tool call interrupting a message is the
                            // signal that the message is done growing.
                            if let Some(previous) = entry_index.checked_sub(1) {
                                this.narrate_read_aloud_message(previous, cx);
                            }
                            this.note_read_aloud_tool_call(
                                entry_index,
                                NarrationTrigger::Updated,
                                cx,
                            );
                        }
                        NewEntryKind::Other => {}
                    }
                    this.enqueue_read_aloud(false, cx);
                }
                AcpThreadEvent::EntryUpdated(entry_ix) => {
                    // Tool calls and terminals fire this continuously while
                    // they stream; re-segmenting the whole message for each
                    // one would be quadratic foreground work for no change.
                    let updated_assistant_message = matches!(
                        thread.read(cx).entries().get(*entry_ix),
                        Some(AgentThreadEntry::AssistantMessage(_))
                    );
                    if updated_assistant_message {
                        this.enqueue_read_aloud(false, cx);
                        // Closing prose streaming as the last entry means
                        // the agent has stopped calling tools and is winding
                        // up; that is the cue to start generating the turn's
                        // wrap-up, so its audio is ready when the turn ends.
                        if *entry_ix + 1 == thread.read(cx).entries().len() {
                            this.speculate_read_aloud_wrap_up(*entry_ix, cx);
                        }
                    } else {
                        this.note_read_aloud_tool_call(*entry_ix, NarrationTrigger::Updated, cx);
                    }
                }
                AcpThreadEvent::Stopped(_) => {
                    // The message is complete: let the segmenter speak the
                    // trailing block, which it withholds while streaming.
                    this.enqueue_read_aloud(true, cx);
                    // The enqueue above is gated on `auto_play`; completeness
                    // must reach the reader regardless, or a message played
                    // explicitly during the turn stays flagged incomplete
                    // forever — leaving the mini player visible after its
                    // audio drains, with its withheld tail unspoken.
                    if let Some(read_aloud) = this.read_aloud.clone() {
                        read_aloud.update(cx, |read_aloud, cx| {
                            read_aloud.mark_tracked_message_complete(cx);
                        });
                    }
                    // A call whose label never refined, and whose status
                    // never left pending, still happened: narrate it now
                    // rather than going silent waiting for an update that is
                    // no longer coming.
                    this.read_aloud_label_settle = None;
                    this.flush_read_aloud_tool_calls(cx);
                    // The turn's last assistant message can no longer grow,
                    // so narration mode may now wrap the turn up. Earlier
                    // messages of the turn reached narration when the tool
                    // call that ended them arrived.
                    if let Some((entry_index, _)) =
                        Self::latest_assistant_markdown_in(&this.thread, cx)
                    {
                        this.finish_read_aloud_turn(entry_index, cx);
                    }
                }
                AcpThreadEvent::EntriesRemoved(range) => {
                    // A rewind or refusal truncation shifts every later
                    // entry down; a stale watermark above the regenerated
                    // turn's indices would silently disable auto-play for
                    // every turn until the count grew past it again.
                    this.read_aloud_watermark = this.read_aloud_watermark.min(range.start);
                    // Same hazard for the catch-up span: a watermark above
                    // the regenerated turn's indices would report "nothing
                    // new" about work that had just been redone.
                    this.read_aloud_catch_up_watermark =
                        this.read_aloud_catch_up_watermark.min(range.start);
                    // A span in flight is about entries that no longer exist,
                    // so it must not be committed if it lands.
                    this.read_aloud_catch_up_in_flight = None;
                    // Tool-call ids are scoped to the message they belong to,
                    // so a regenerated turn can reuse one. Remembering that
                    // the *removed* call was narrated would silence its
                    // replacement, and a remembered "unfinished" id would pull
                    // in whatever call inherited it. Both sets go; the entries
                    // are back in the span anyway, by index.
                    this.read_aloud_tool_calls.clear();
                    this.read_aloud_catch_up_unfinished.clear();
                    this.read_aloud_label_settle = None;
                }
                _ => {}
            },
        ));
    }

    fn notify_read_aloud_disabled(&self, message: String, cx: &mut Context<Self>) {
        log::warn!("read_aloud: disabled: {message}");
        if let Some(workspace) = self.workspace.upgrade() {
            workspace.update(cx, |workspace, cx| {
                workspace.show_toast(
                    Toast::new(
                        NotificationId::unique::<ReadAloudDisabled>(),
                        format!("Read aloud is disabled: {message}"),
                    ),
                    cx,
                );
            });
        }
    }

    /// The prose of the newest assistant message, if it has any. Stops at that
    /// message rather than searching further back: an older, already-finished
    /// message is never what the reader should pick up next.
    fn latest_assistant_markdown(&self, cx: &App) -> Option<Entity<Markdown>> {
        Self::latest_assistant_markdown_in(&self.thread, cx).map(|(_, markdown)| markdown)
    }

    /// Associated form of [`Self::latest_assistant_markdown`] for callbacks
    /// that hold the thread but not the view, paired with the message's
    /// entry index (which the auto-play watermark compares against).
    fn latest_assistant_markdown_in(
        thread: &Entity<AcpThread>,
        cx: &App,
    ) -> Option<(usize, Entity<Markdown>)> {
        let thread = thread.read(cx);
        let (entry_index, message) = thread.entries().iter().enumerate().rev().find_map(
            |(entry_index, entry)| match entry {
                AgentThreadEntry::AssistantMessage(message) => Some((entry_index, message)),
                _ => None,
            },
        )?;
        message
            .chunks
            .iter()
            .rev()
            .find_map(|chunk| match chunk {
                AssistantMessageChunk::Message { block, .. } => block.markdown().cloned(),
                // Thinking is deliberately never spoken.
                AssistantMessageChunk::Thought { .. } => None,
            })
            .map(|markdown| (entry_index, markdown))
    }

    /// Hands the latest assistant prose to the reader. Safe to call on every
    /// streaming update: `enqueue_markdown` only synthesizes text it has not
    /// already queued.
    fn enqueue_read_aloud(&mut self, message_complete: bool, cx: &mut Context<Self>) {
        let Some(read_aloud) = self.read_aloud.clone() else {
            return;
        };
        let settings = read_aloud::ReadAloudSettings::get_global(cx);
        if !settings.auto_play {
            return;
        }
        if settings.mode == read_aloud::ReadAloudMode::Narration {
            // Narration mode speaks status, not prose. Explicitly asking for
            // a message — the speaker buttons, a sentence click, the Toggle
            // action — still reads it in full.
            return;
        }
        let Some((entry_index, markdown)) = Self::latest_assistant_markdown_in(&self.thread, cx)
        else {
            return;
        };
        if entry_index < self.read_aloud_watermark {
            // Restored history and previous turns are never auto-played;
            // only prose that starts streaming while this view is live is.
            // In particular, the user's own reply arrives as a `NewEntry`
            // whose newest assistant markdown is still the old turn's.
            return;
        }

        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.enqueue_markdown(&markdown, message_complete, cx);
        });
    }

    /// Hands one tool call to narration, once, as soon as its label has
    /// stopped changing — and never before.
    ///
    /// A tool call's title is recomputed and re-sent on *every* delta of the
    /// model's streamed tool input (`Thread::handle_tool_use_event`), from
    /// whatever prefix of that input has arrived. What it says on the way is
    /// not a placeholder that can be recognised:
    ///
    /// * `read_file` and `terminal` produce a fixed generic string ("Read
    ///   file", or the empty string) until the input parses, then the real
    ///   title.
    /// * The **edit family** produces a *plausible but wrong* title:
    ///   `initial_title_from_partial_path` deserializes the partial input
    ///   (`partial_json_fixer::fix_json` closes the truncated string, so it
    ///   parses cleanly) and falls back to the raw prefix when it cannot be
    ///   resolved against the project. Half of `crates/read_aloud/src/…`
    ///   arrives as the title `crates/read_aloud/sr`, which reads aloud as
    ///   "Editing sr."
    ///
    /// Nor does the status help: `edit_file_tool` and `write_file_tool`
    /// declare `supports_input_streaming`, so `run_tool` — and with it the
    /// `InProgress` status — fires on the *first* partial delta, long before
    /// the path is known.
    ///
    /// There is therefore no field in the protocol that distinguishes a
    /// half-streamed title from a finished one. The only honest signal is
    /// that a title which is still arriving keeps changing, and a finished
    /// one does not: a call is narrated once its label has held still for
    /// [`TOOL_LABEL_SETTLE`]. A terminal status short-circuits the wait — no
    /// refinement can follow a call that has finished — and the turn-end
    /// sweep is the backstop for anything still waiting.
    ///
    /// That signal has one hole, and [`read_aloud_tool_call_is_ready`] is
    /// what plugs it: a call whose payload arrives *empty* has a label that
    /// cannot change, so it looks settled immediately and the timer speaks
    /// the placeholder.
    ///
    /// Dedupe is per tool-call id, not per label, so a call whose label moves
    /// again later never produces a second utterance.
    fn note_read_aloud_tool_call(
        &mut self,
        entry_index: usize,
        trigger: NarrationTrigger,
        cx: &mut Context<Self>,
    ) {
        let Some(read_aloud) = self.read_aloud.clone() else {
            return;
        };
        if !read_aloud_narrates_unasked(cx)
            || !read_aloud::ReadAloudSettings::get_global(cx).narrate_tool_calls
        {
            return;
        }
        if entry_index < self.read_aloud_watermark {
            return;
        }
        let Some(AgentThreadEntry::ToolCall(tool_call)) =
            self.thread.read(cx).entries().get(entry_index)
        else {
            return;
        };
        // A call the user refused, or that the turn's cancellation took
        // down, never happened; saying so is noise.
        if matches!(
            tool_call.status,
            ToolCallStatus::Rejected | ToolCallStatus::Canceled
        ) {
            return;
        }
        let facts = read_aloud_tool_call_facts(tool_call);
        let ready = read_aloud_tool_call_is_ready(&facts, &tool_call.status, trigger, cx);
        // The two waits have the same shape: the line cannot improve until
        // the agent sends more, so nothing is gained by timing it again.
        let awaiting_refinement = facts.awaiting_input() || facts.is_shell_noise(cx);
        let call_id = tool_call.id.clone();
        let key = facts.spoken_key(cx);

        // Whether the call has been narrated or not, how it ended — and what
        // it printed — is what the turn's wrap-up most needs to know. The
        // whole facts go over, not just the id and the outcome: the output
        // only exists on this update, and the facts the reader stored were
        // taken while the call was still pending.
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.note_tool_call_result(&facts, cx);
        });

        let state = self
            .read_aloud_tool_calls
            .entry(call_id)
            .or_insert_with(|| ToolCallNarration {
                last_key: key.clone(),
                narrated: false,
            });
        if state.narrated {
            return;
        }
        let moved = state.last_key != key;
        state.last_key = key.clone();
        if !ready {
            // Still moving, not yet still for long enough, or still empty.
            // Either way the step it belongs to must not close underneath it,
            // so the reader is told the agent is mid-action.
            self.hold_read_aloud_step(cx);
            // An empty payload — or a command that is still only a shell
            // keyword — has nothing to time: its line cannot move until more
            // arrives, and the update that brings it re-enters here and arms
            // the timer then. Re-arming on every expiry instead would spin a
            // timer for the life of the call.
            if moved || (self.read_aloud_label_settle.is_none() && !awaiting_refinement) {
                self.arm_read_aloud_label_settle(cx);
            }
            return;
        }
        if key.trim().is_empty() {
            return;
        }
        state.narrated = true;

        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.narrate_tool_call(facts, cx);
        });
    }

    /// (Re)starts the timer that decides a tool label has stopped changing.
    /// Dropping the previous task cancels it, so every fresh label pushes the
    /// decision out rather than stacking timers.
    fn arm_read_aloud_label_settle(&mut self, cx: &mut Context<Self>) {
        self.read_aloud_label_settle = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(TOOL_LABEL_SETTLE).await;
            this.update(cx, |this, cx| {
                this.read_aloud_label_settle = None;
                this.narrate_settled_read_aloud_tool_calls(cx);
            })
            .log_err();
        }));
    }

    /// Narrates every un-narrated call whose label held still while the
    /// settle timer ran. A call whose label moved in that window has already
    /// re-armed the timer and waits for the next one.
    fn narrate_settled_read_aloud_tool_calls(&mut self, cx: &mut Context<Self>) {
        let settled: Vec<usize> = {
            let entries = self.thread.read(cx).entries();
            entries
                .iter()
                .enumerate()
                .skip(self.read_aloud_watermark)
                .filter_map(|(entry_index, entry)| match entry {
                    AgentThreadEntry::ToolCall(tool_call) => self
                        .read_aloud_tool_calls
                        .get(&tool_call.id)
                        .filter(|state| {
                            !state.narrated
                                && state.last_key
                                    == read_aloud_tool_call_facts(tool_call).spoken_key(cx)
                        })
                        .map(|_| entry_index),
                    _ => None,
                })
                .collect()
        };
        for entry_index in settled {
            self.note_read_aloud_tool_call(entry_index, NarrationTrigger::Settled, cx);
        }
    }

    /// Tells the reader that the agent is mid-action even though nothing is
    /// sayable yet, so the open step does not close out from under a tool
    /// call whose label is still arriving.
    fn hold_read_aloud_step(&mut self, cx: &mut Context<Self>) {
        if let Some(read_aloud) = self.read_aloud.clone() {
            read_aloud.update(cx, |read_aloud, cx| read_aloud.note_tool_call_pending(cx));
        }
    }

    /// Narrates every tool call of this turn that is still waiting for a
    /// refinement that is not coming. Called when the turn ends.
    fn flush_read_aloud_tool_calls(&mut self, cx: &mut Context<Self>) {
        if self
            .read_aloud_tool_calls
            .values()
            .all(|state| state.narrated)
        {
            return;
        }
        let waiting: Vec<usize> = {
            let entries = self.thread.read(cx).entries();
            entries
                .iter()
                .enumerate()
                .skip(self.read_aloud_watermark)
                .filter_map(|(entry_index, entry)| match entry {
                    AgentThreadEntry::ToolCall(tool_call) => self
                        .read_aloud_tool_calls
                        .get(&tool_call.id)
                        .is_some_and(|state| !state.narrated)
                        .then_some(entry_index),
                    _ => None,
                })
                .collect()
        };
        for entry_index in waiting {
            self.note_read_aloud_tool_call(entry_index, NarrationTrigger::TurnEnded, cx);
        }
    }

    /// Hands one finished assistant message to narration. Only ever called
    /// for a message that can no longer grow — a tool call has interrupted
    /// it, or the turn has stopped.
    fn narrate_read_aloud_message(&mut self, entry_index: usize, cx: &mut Context<Self>) {
        let Some(read_aloud) = self.read_aloud.clone() else {
            return;
        };
        if !read_aloud_narrates_unasked(cx) {
            return;
        }
        if entry_index < self.read_aloud_watermark {
            // Restored history is never narrated, for the same reason it is
            // never auto-played.
            return;
        }
        let blocks =
            Self::assistant_message_markdowns(self.thread.read(cx).entries(), entry_index, cx);
        if blocks.is_empty() {
            return;
        }
        // Resolved per message rather than cached: models finish loading
        // after the panel opens, and an install that had none at startup
        // would otherwise be stuck on the fallback for the whole session.
        let summary_model = self.require_read_aloud_summary_model(cx);
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.set_summary_model(summary_model);
            read_aloud.narrate_message(blocks, cx);
        });
    }

    /// The turn is over: narration wraps it up rather than reporting one
    /// more piece of status.
    fn finish_read_aloud_turn(&mut self, entry_index: usize, cx: &mut Context<Self>) {
        let Some(read_aloud) = self.read_aloud.clone() else {
            return;
        };
        if !read_aloud_narrates_unasked(cx) {
            return;
        }
        if entry_index < self.read_aloud_watermark {
            return;
        }
        let blocks =
            Self::assistant_message_markdowns(self.thread.read(cx).entries(), entry_index, cx);
        if blocks.is_empty() {
            return;
        }
        let summary_model = self.require_read_aloud_summary_model(cx);
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.set_summary_model(summary_model);
            read_aloud.finish_turn(blocks, cx);
        });
    }

    /// Speaks a summary of everything the agent has done since this was last
    /// asked for — the whole session, the first time.
    ///
    /// The point of the feature is *not* having to listen: with several
    /// sessions running, or during a long turn nobody wants narrated at them,
    /// one press is worth twenty minutes of ambient status. So this ignores
    /// the things that decide whether narration speaks *unasked* — the mode,
    /// the stop latch, `auto_play` — and only needs read aloud to be on at
    /// all.
    ///
    /// The span is assembled from the thread's own entries rather than from
    /// anything narration accumulated: the reader throws its material away at
    /// the end of every turn, and this span deliberately crosses turns.
    pub(crate) fn summarize_read_aloud_session(&mut self, cx: &mut Context<Self>) {
        let Some(read_aloud) = self.read_aloud.clone() else {
            return;
        };
        let entry_count = self.thread.read(cx).entries().len();
        let watermark = self.read_aloud_catch_up_watermark.min(entry_count);
        let Some((span, blocks, unfinished)) = self.read_aloud_catch_up_span(watermark, cx) else {
            read_aloud.update(cx, |read_aloud, cx| {
                read_aloud.announce(NOTHING_NEW_SINCE_LAST_CATCH_UP, cx);
            });
            return;
        };
        // Held, not applied. `ReadAloudEvent::CaughtUp` commits it once the
        // listener has actually heard something; until then a second press
        // re-asks about the same span rather than being told nothing happened,
        // which is what impatient double-pressing on a button that is silent
        // for up to fifteen seconds used to produce.
        self.read_aloud_catch_up_in_flight = Some(CatchUpInFlight {
            watermark: entry_count,
            unfinished,
            asked_at: cx.background_executor().now(),
        });
        let summary_model = self.require_read_aloud_summary_model(cx);
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.set_summary_model(summary_model);
            read_aloud.catch_up(span, blocks, cx);
        });
    }

    /// Retires the span a catch-up just spoke. Called from
    /// [`read_aloud::ReadAloudEvent::CaughtUp`] and nowhere else, so a
    /// cancelled request leaves the span intact for the next press.
    fn read_aloud_catch_up_delivered(&mut self) {
        let Some(in_flight) = self.read_aloud_catch_up_in_flight.take() else {
            return;
        };
        self.read_aloud_catch_up_watermark = in_flight.watermark;
        self.read_aloud_catch_up_unfinished = in_flight.unfinished;
        self.read_aloud_catch_up_since = Some(in_flight.asked_at);
    }

    /// The material for a catch-up covering every entry from `watermark` on,
    /// and the prose blocks it should wash while speaking.
    ///
    /// `None` when there is nothing there at all — no new entries and no
    /// actions — which is what a second press in a row means. New prose with
    /// no actions behind it is *not* nothing: a turn that only wrote is still
    /// a turn the listener missed, and it goes to the model with an empty
    /// account rather than being answered with "nothing new".
    ///
    /// Separate from the press so the span itself is testable without a
    /// model, which is the half of this that can be got wrong quietly.
    fn read_aloud_catch_up_span(
        &self,
        watermark: usize,
        cx: &App,
    ) -> Option<(
        read_aloud::CatchUpSpan,
        Vec<Entity<Markdown>>,
        HashSet<acp::ToolCallId>,
    )> {
        let entries = self.thread.read(cx).entries();
        // Whatever is still running when this span retires has not finished
        // happening, so the next span gets it back — collected over everything
        // considered, including the calls carried in from last time that are
        // still going.
        let mut unfinished: HashSet<acp::ToolCallId> = HashSet::default();
        let mut calls: Vec<(acp::ToolCallId, read_aloud::ToolCallFacts)> = Vec::new();
        for (entry_index, entry) in entries.iter().enumerate() {
            // A call the user refused, or that a cancellation took down, never
            // happened — the same rule narration applies live.
            let AgentThreadEntry::ToolCall(tool_call) = entry else {
                continue;
            };
            if matches!(
                tool_call.status,
                ToolCallStatus::Rejected | ToolCallStatus::Canceled
            ) {
                continue;
            }
            // `acp_thread` mutates a tool call in place, so a call that was
            // pending when the watermark passed it keeps its index below the
            // mark while its outcome and its output arrive later. Those are
            // pulled back in by id.
            let fresh = entry_index >= watermark;
            let carried = self.read_aloud_catch_up_unfinished.contains(&tool_call.id);
            if !fresh && !carried {
                continue;
            }
            let facts = read_aloud_tool_call_facts(tool_call);
            let still_running = facts.outcome == read_aloud::ToolCallOutcome::Pending;
            if still_running {
                unfinished.insert(tool_call.id.clone());
            }
            // A carried call that is *still* running has already been reported
            // exactly as it stands, so it is not news. Without this it counted
            // as activity forever, and "nothing new since the last catch-up"
            // became unreachable for as long as anything was in flight — which
            // on a long turn is most of the time.
            if !fresh && still_running {
                continue;
            }
            calls.push((tool_call.id.clone(), facts));
        }
        if calls.is_empty() && entries.len() <= watermark {
            return None;
        }
        // Prose already covered by a previous catch-up is not re-sent, or two
        // consecutive presses summarize the same paragraph and sound like a
        // stuck record. The blocks are still handed over for the wash, which
        // is about what is on screen rather than what is being said.
        let latest = Self::latest_assistant_markdown_in(&self.thread, cx);
        let blocks = latest.as_ref().map_or_else(Vec::new, |(entry_index, _)| {
            Self::assistant_message_markdowns(entries, *entry_index, cx)
        });
        let message = match latest {
            Some((entry_index, _)) if entry_index >= watermark => blocks
                .iter()
                .map(|block| block.read(cx).source().to_string())
                .collect::<Vec<_>>()
                .join("\n\n"),
            _ => String::new(),
        };
        if calls.is_empty() && message.is_empty() {
            return None;
        }
        let actions: Vec<(read_aloud::ToolCallOutcome, String)> = calls
            .iter()
            .map(|(_, facts)| (facts.outcome, facts.description(cx)))
            .collect();
        let span = read_aloud::CatchUpSpan {
            activity: read_aloud::bounded_activity(&actions),
            files_changed: read_aloud::files_changed(calls.iter().map(|(_, facts)| facts), cx),
            message,
            still_streaming: self.thread.read(cx).status() == ThreadStatus::Generating,
            tool_calls: calls.len(),
            elapsed: self.read_aloud_catch_up_since.map(|since| {
                cx.background_executor()
                    .now()
                    .saturating_duration_since(since)
            }),
        };
        Some((span, blocks, unfinished))
    }

    #[cfg(test)]
    pub(super) fn read_aloud_catch_up_span_for_test(
        &self,
        cx: &App,
    ) -> Option<read_aloud::CatchUpSpan> {
        self.read_aloud_catch_up_span(self.read_aloud_catch_up_watermark, cx)
            .map(|(span, _, _)| span)
    }

    /// Asks narration to start the turn's wrap-up while the agent is still
    /// writing its closing message, so the audio is ready the instant the
    /// turn ends rather than a model round trip afterwards.
    ///
    /// This runs on every streamed chunk of that message, so the cheap
    /// question (`wants_wrap_up`, a handful of comparisons) is asked before
    /// the expensive one: resolving a model walks the provider registry.
    fn speculate_read_aloud_wrap_up(&mut self, entry_index: usize, cx: &mut Context<Self>) {
        let Some(read_aloud) = self.read_aloud.clone() else {
            return;
        };
        if !read_aloud_narrates_unasked(cx) {
            return;
        }
        if entry_index < self.read_aloud_watermark {
            return;
        }
        let blocks =
            Self::assistant_message_markdowns(self.thread.read(cx).entries(), entry_index, cx);
        let message_chars: usize = blocks
            .iter()
            .map(|block| block.read(cx).source().len())
            .sum();
        if !read_aloud.read(cx).wants_wrap_up(message_chars) {
            return;
        }
        let summary_model = self.read_aloud_summary_model(cx);
        read_aloud.update(cx, |read_aloud, cx| {
            read_aloud.set_summary_model(summary_model);
            read_aloud.speculate_wrap_up(blocks, message_chars, cx);
        });
    }

    /// [`Self::read_aloud_summary_model`], plus the once-per-view toast that
    /// says narration is running degraded when nothing resolves.
    ///
    /// Only the paths that are about to *speak* use this. Wrap-up
    /// speculation runs on every streamed chunk and asks the same question
    /// long before anything is said, so toasting from there could fire while
    /// providers are still authenticating.
    pub(crate) fn require_read_aloud_summary_model(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<Rc<dyn read_aloud::SummaryModel>> {
        let model = self.read_aloud_summary_model(cx);
        if model.is_none() && !self.read_aloud_warned_no_summary_model {
            self.read_aloud_warned_no_summary_model = true;
            self.notify_read_aloud_degraded(
                "no language model is available to summarize with. Set \
                 `read_aloud.summary_model`, or configure an agent model.",
                cx,
            );
        }
        model
    }

    /// Resolves the model narration condenses messages and steps with.
    ///
    /// The order prefers small and fast, because every one of these calls is
    /// on the path between the agent doing something and the listener
    /// hearing about it, and a step line that arrives after the step is over
    /// is worse than the templated one that arrives now:
    ///
    /// 1. `read_aloud.summary_model` — named for this, so it always wins.
    /// 2. `registry.thread_summary_model` — Zed's own "condense a thread into
    ///    a line" model, the closest existing job to this one. It resolves
    ///    `agent.thread_summary_model`, else the default provider's *fast*
    ///    model, else the default model, so a normal Zed install lands on a
    ///    Haiku-class model rather than whatever the panel is chatting with.
    /// 3. `registry.commit_message_model` — the other small-task model people
    ///    configure. This is the rung that catches somebody driving an
    ///    external ACP agent, who has no Zed default model at all: rung 2
    ///    resolves to nothing for them, and this is often the only model they
    ///    have set.
    /// 4. `registry.inline_assistant_model` — what this used to be, kept so
    ///    an install configured only that way is not regressed.
    ///
    /// A provider that has not authenticated is skipped rather than
    /// returned: an unauthenticated model is not a model, and pretending
    /// otherwise is what made "no model" and "broken model" look the same.
    /// `None` leaves narration on its templated fallback.
    fn read_aloud_summary_model(&self, cx: &App) -> Option<Rc<dyn read_aloud::SummaryModel>> {
        let registry = LanguageModelRegistry::try_read_global(cx)?;
        let configured = resolve_read_aloud_summary_model(
            registry,
            read_aloud::ReadAloudSettings::get_global(cx)
                .summary_model
                .as_ref(),
            cx,
        )?;
        let temperature = AgentSettings::temperature_for_model(&configured.model, cx);
        Some(Rc::new(ReadAloudSummaryModel {
            model: configured.model,
            provider: configured.provider,
            temperature,
        }))
    }

    /// Says out loud, once per view, that narration is running on its
    /// templated fallback rather than on real summaries.
    ///
    /// A log line is how this survived two days of use: the feature went on
    /// working, just as a much worse product, and nothing on screen said so.
    fn notify_read_aloud_degraded(&self, reason: &str, cx: &mut Context<Self>) {
        log::warn!("read_aloud: narration is degraded: {reason}");
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            workspace.show_toast(
                Toast::new(
                    NotificationId::unique::<ReadAloudNarrationDegraded>(),
                    format!("Read aloud is narrating without summaries: {reason}"),
                ),
                cx,
            );
        });
    }

    /// Starts, stops, or restarts reading aloud.
    ///
    /// `auto_play` gates automatic playback only and is deliberately not
    /// consulted here: asking for this explicitly always reads the newest
    /// message.
    pub(super) fn toggle_read_aloud(&mut self, cx: &mut Context<Self>) {
        let Some(read_aloud) = self.read_aloud.clone() else {
            return;
        };

        // "Speaking" here must match what the UI presents as active, not
        // just whether the poll task runs: during an idle streaming lull the
        // controls (and their X) are visible while the poll is parked, and
        // a toggle there must stop — never fall through to the load/restart
        // paths and start something.
        let presenting_playback = {
            let reader = read_aloud.read(cx);
            reader.is_speaking()
                || reader
                    .playback_state(cx)
                    .is_some_and(|state| !state.stopped)
        };
        if presenting_playback {
            read_aloud.update(cx, |read_aloud, cx| read_aloud.toggle(cx));
            return;
        }

        let Some(markdown) = self.latest_assistant_markdown(cx) else {
            // Nothing to read; `toggle` still restarts whatever is loaded.
            read_aloud.update(cx, |read_aloud, cx| read_aloud.toggle(cx));
            return;
        };

        let already_loaded = read_aloud.read(cx).speaking() == Some(&markdown);
        let message_complete = self.thread.read(cx).status() != ThreadStatus::Generating;

        read_aloud.update(cx, |read_aloud, cx| {
            if already_loaded {
                // Rewind first: a stopped player parks its synthesis cursor at
                // the end of the utterance list, so re-enqueueing on its own
                // would speak nothing. `toggle` also lifts the user-stop latch,
                // without which the enqueue below would be ignored.
                read_aloud.toggle(cx);
                // Re-segment even when this message is the one already loaded.
                // With `auto_play` off nothing else ever refreshes the
                // utterance list, and the first load can easily have landed
                // while the message had no complete sentence in it yet —
                // leaving `toggle` alone with nothing to restart, forever.
                read_aloud.enqueue_markdown(&markdown, message_complete, cx);
            } else {
                // Loading a different message is explicit intent, and must
                // override a latched stop or a dismissal — which
                // `enqueue_markdown`, the passive streaming path, honors.
                read_aloud.play_from_top(&markdown, message_complete, cx);
            }
        });
    }

    /// Called when this view stops being the active thread. A backgrounded
    /// view keeps its entity, its thread subscription, and its handle on the
    /// one shared audio player, so without this it keeps talking about a
    /// conversation the user is no longer looking at — and, in narration
    /// mode, a summary already being generated lands afterwards and washes a
    /// message that is off screen entirely.
    ///
    /// Silence lasts only as long as the user is away: `deactivate` parks
    /// playback the way a stop does but records the latch as the
    /// navigation's, so [`Self::read_aloud_activated`] can hand the user
    /// their own intent back. Leaving a thread should not disarm read aloud
    /// for the next turn you start on it.
    pub(crate) fn read_aloud_deactivated(&mut self, cx: &mut Context<Self>) {
        let Some(read_aloud) = self.read_aloud.clone() else {
            return;
        };
        read_aloud.update(cx, |read_aloud, cx| read_aloud.deactivate(cx));
    }

    /// Called when this view becomes the active thread again. Nothing
    /// resumes on its own — the next turn is simply allowed to speak.
    pub(crate) fn read_aloud_activated(&mut self, cx: &mut Context<Self>) {
        let Some(read_aloud) = self.read_aloud.clone() else {
            return;
        };
        read_aloud.update(cx, |read_aloud, cx| read_aloud.reactivate(cx));
    }

    #[cfg(test)]
    pub(super) fn set_read_aloud_for_test(&mut self, read_aloud: Entity<read_aloud::ReadAloud>) {
        self.read_aloud = Some(read_aloud);
    }

    #[cfg(test)]
    pub(super) fn read_aloud_for_test(&self) -> Option<&Entity<read_aloud::ReadAloud>> {
        self.read_aloud.as_ref()
    }

    /// Whether an activation (API-key resolution) is in flight — the tests'
    /// only evidence that a settings re-enable re-ran the activation path.
    #[cfg(test)]
    pub(super) fn read_aloud_activation_pending_for_test(&self) -> bool {
        self.read_aloud_activation.is_some()
    }

    /// Installs the real auto-play subscription (with its watermark), which
    /// `set_read_aloud_for_test` deliberately does not — most tests drive the
    /// reader explicitly and must not receive auto-play enqueues.
    #[cfg(test)]
    pub(super) fn subscribe_read_aloud_for_test(&mut self, cx: &mut Context<Self>) {
        self.subscribe_read_aloud(cx);
        // The same wiring activation installs, so what the reader reports
        // reaches the UI in tests too.
        if let Some(read_aloud) = self.read_aloud.clone() {
            self.watch_read_aloud(&read_aloud, cx);
        }
    }

    /// Floating playback controls for read aloud, docked just above the
    /// composer. While something plays (or is paused or synthesizing) it
    /// shows the full transport; after a user stop it reduces to a replay
    /// form — play, first-sentence preview, and an X that dismisses the
    /// player entirely. Buttons route through the same code paths as the
    /// `read_aloud::Toggle` and `read_aloud::TogglePause` actions, so
    /// behavior stays identical to the keyboard.
    fn render_read_aloud_mini_player(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let read_aloud = self.read_aloud.as_ref()?;
        let mode_toggle = self.render_read_aloud_mode_toggle(cx);
        let Some(state) = read_aloud.read(cx).playback_state(cx) else {
            // Nothing to control, but the mode toggle still has to be here:
            // full versus narration is a choice about how the *next* turn
            // will sound, so it must be reachable before one starts. A quiet
            // pill holds it and a speaker that reads the newest message.
            return Some(Self::float_above_composer(
                h_flex()
                    .gap_0p5()
                    .p_0p5()
                    .rounded_full()
                    .border_1()
                    .border_color(cx.theme().colors().border)
                    .bg(cx.theme().colors().elevated_surface_background)
                    .opacity(0.6)
                    .hover(|style| style.opacity(1.0))
                    .child(
                        IconButton::new("read-aloud-start", IconName::AudioOn)
                            .icon_size(IconSize::Small)
                            .icon_color(Color::Muted)
                            .tooltip(Tooltip::text("Read Aloud"))
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_read_aloud(cx))),
                    )
                    .child(self.render_read_aloud_catch_up_button(cx))
                    .children(mode_toggle)
                    .into_any_element(),
            ));
        };

        let sentence_preview = div().max_w(rems(16.)).child(
            Label::new(state.sentence_text.clone())
                .size(LabelSize::Small)
                .color(Color::Muted)
                .truncate(),
        );
        let voice_menu = self.render_read_aloud_voice_menu(cx);

        let pill = h_flex()
            .gap_1()
            .py_1()
            .px_2()
            .rounded_full()
            .border_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().elevated_surface_background)
            .shadow_md();

        let pill = if state.stopped {
            pill.child(
                IconButton::new("read-aloud-restart", IconName::PlayFilled)
                    .icon_size(IconSize::Small)
                    .style(ui::ButtonStyle::Filled)
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(read_aloud) = this.read_aloud.clone() {
                            // The same restart path the Toggle action takes:
                            // rewind the tracked message and speak it.
                            read_aloud.update(cx, |read_aloud, cx| read_aloud.toggle(cx));
                        }
                    })),
            )
            .child(sentence_preview)
            .child(voice_menu)
            .child(self.render_read_aloud_catch_up_button(cx))
            .children(mode_toggle)
            .child(
                IconButton::new("read-aloud-dismiss", IconName::Close)
                    .icon_size(IconSize::Small)
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(read_aloud) = this.read_aloud.clone() {
                            read_aloud.update(cx, |read_aloud, cx| read_aloud.dismiss(cx));
                        }
                    })),
            )
        } else {
            let at_start = state.utterance_index == 0;
            let at_end = state.utterance_index + 1 >= state.utterance_count;
            let counter = format!("{}/{}", state.utterance_index + 1, state.utterance_count);
            let play_pause_icon = if state.paused {
                IconName::PlayFilled
            } else {
                IconName::DebugPause
            };

            pill.child(
                IconButton::new("read-aloud-previous", IconName::ChevronLeft)
                    .icon_size(IconSize::Small)
                    .disabled(at_start)
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(read_aloud) = this.read_aloud.clone() {
                            read_aloud
                                .update(cx, |read_aloud, cx| read_aloud.previous_sentence(cx));
                        }
                    })),
            )
            .child(
                IconButton::new("read-aloud-play-pause", play_pause_icon)
                    .icon_size(IconSize::Small)
                    .style(ui::ButtonStyle::Filled)
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(read_aloud) = this.read_aloud.clone() {
                            read_aloud.update(cx, |read_aloud, cx| read_aloud.toggle_pause(cx));
                        }
                    })),
            )
            .child(
                IconButton::new("read-aloud-next", IconName::ChevronRight)
                    .icon_size(IconSize::Small)
                    .disabled(at_end)
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(read_aloud) = this.read_aloud.clone() {
                            read_aloud.update(cx, |read_aloud, cx| read_aloud.next_sentence(cx));
                        }
                    })),
            )
            .child(sentence_preview)
            .child(
                Label::new(counter)
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .child(voice_menu)
            .child(self.render_read_aloud_catch_up_button(cx))
            .children(mode_toggle)
            .child(
                IconButton::new("read-aloud-stop", IconName::Close)
                    .icon_size(IconSize::Small)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.toggle_read_aloud(cx);
                    })),
            )
        };

        Some(Self::float_above_composer(pill.into_any_element()))
    }

    /// "Catch me up": speaks what has happened since it was last pressed.
    ///
    /// On every form of the mini player, including the quiet pill shown when
    /// nothing is playing — that state, with `auto_play` off, *is* the
    /// on-demand experience, and a button that only appeared once something
    /// was already talking would be there exactly when it is not wanted.
    fn render_read_aloud_catch_up_button(&self, cx: &mut Context<Self>) -> AnyElement {
        // Deliberately not `ListCollapse`: that is the narration-mode toggle
        // sitting right beside it, and two identical glyphs would read as one
        // control.
        IconButton::new("read-aloud-catch-up", IconName::ThreadFromSummary)
            .icon_size(IconSize::Small)
            .icon_color(Color::Muted)
            .tooltip(Tooltip::for_action_title(
                "Catch Me Up",
                &read_aloud::SummarizeSession,
            ))
            .on_click(cx.listener(|this, _, _, cx| this.summarize_read_aloud_session(cx)))
            .into_any_element()
    }

    /// A zero-height anchor: read-aloud controls float above the composer
    /// without shifting the layout when they appear.
    fn float_above_composer(controls: AnyElement) -> AnyElement {
        div()
            .relative()
            .w_full()
            .h_0()
            .child(
                h_flex()
                    .absolute()
                    .bottom_2()
                    .left_0()
                    .right_0()
                    .justify_center()
                    .child(controls),
            )
            .into_any_element()
    }

    /// The full ↔ narration switch. Shows the mode that is on, and writes
    /// the other one to `read_aloud.mode` through the standard targeted
    /// settings edit; the settings observation then applies it live.
    fn render_read_aloud_mode_toggle(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let project = self.project.upgrade()?;
        let fs = project.read(cx).fs().clone();
        let narrating = read_aloud::ReadAloudSettings::get_global(cx).mode
            == read_aloud::ReadAloudMode::Narration;
        let (icon, tooltip) = if narrating {
            (IconName::ListCollapse, "Narrate Progress")
        } else {
            (IconName::FileTextOutlined, "Read Full Response")
        };
        Some(
            IconButton::new("read-aloud-mode", icon)
                .icon_size(IconSize::Small)
                .icon_color(if narrating {
                    Color::Accent
                } else {
                    Color::Muted
                })
                .tooltip(Tooltip::text(tooltip))
                .on_click(move |_, _, cx| {
                    let mode = if narrating {
                        read_aloud::ReadAloudMode::Full
                    } else {
                        read_aloud::ReadAloudMode::Narration
                    };
                    update_settings_file(fs.clone(), cx, move |content, _| {
                        content.read_aloud.get_or_insert_default().mode = Some(mode);
                    });
                })
                .into_any_element(),
        )
    }

    /// The compact voice switcher on the mini player: a button naming the
    /// current voice, opening the provider's catalog with that voice
    /// checked. The catalog is fetched lazily on first open and cached for
    /// the view; while it loads the menu shows a placeholder (reopening once
    /// it lands shows the list), and a failed fetch falls back to a curated
    /// list. Selecting a voice edits `read_aloud.voice_id` in the settings
    /// file; the settings observation then re-targets the provider, so the
    /// next synthesized sentence speaks with the new voice.
    fn render_read_aloud_voice_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(project) = self.project.upgrade() else {
            return div().into_any_element();
        };
        let current_voice_id = read_aloud::ReadAloudSettings::get_global(cx)
            .voice_id
            .clone();
        let voice_name = self
            .read_aloud_voices
            .as_ref()
            .and_then(|voices| {
                voices
                    .iter()
                    .find(|voice| voice.id.as_ref() == current_voice_id)
                    .map(|voice| voice.name.clone())
            })
            .unwrap_or_else(|| SharedString::from(current_voice_id.clone()));

        let this = cx.entity().downgrade();
        let fs = project.read(cx).fs().clone();
        PopoverMenu::new("read-aloud-voice")
            .trigger(
                Button::new("read-aloud-voice-button", voice_name)
                    .label_size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .anchor(gpui::Anchor::BottomLeft)
            .menu(move |window, cx| {
                let voices = this
                    .update(cx, |this, cx| {
                        this.ensure_read_aloud_voices(cx);
                        this.read_aloud_voices.clone()
                    })
                    .ok()
                    .flatten();
                let fs = fs.clone();
                let current_voice_id = current_voice_id.clone();
                Some(ContextMenu::build(
                    window,
                    cx,
                    move |mut menu, _window, _cx| {
                        let Some(voices) = voices else {
                            return menu.header("Loading voices…");
                        };
                        for voice in voices {
                            let checked = voice.id.as_ref() == current_voice_id;
                            let voice_id = voice.id.to_string();
                            let fs = fs.clone();
                            menu = menu.toggleable_entry(
                                voice.name.clone(),
                                checked,
                                IconPosition::Start,
                                None,
                                move |_window, cx| {
                                    let voice_id = voice_id.clone();
                                    update_settings_file(fs.clone(), cx, move |content, _| {
                                        content.read_aloud.get_or_insert_default().voice_id =
                                            Some(voice_id);
                                    });
                                },
                            );
                        }
                        menu
                    },
                ))
            })
            .into_any_element()
    }

    /// Starts the lazy, once-per-view voice catalog fetch. A failure ends in
    /// the curated fallback list rather than an error state, so the menu
    /// always becomes usable.
    fn ensure_read_aloud_voices(&mut self, cx: &mut Context<Self>) {
        if self.read_aloud_voices.is_some() || self.read_aloud_voices_task.is_some() {
            return;
        }
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let http_client = workspace.read(cx).client().http_client();
        self.read_aloud_voices_task = Some(cx.spawn(async move |this, cx| {
            let api_key = cx.update(|cx| read_aloud::resolve_api_key(cx)).await;
            let voices = match api_key {
                Ok(api_key) => {
                    cx.update(|cx| read_aloud::fetch_voices(http_client, api_key, cx))
                        .await
                }
                Err(error) => Err(error),
            };
            let voices = match voices {
                Ok(voices) => voices,
                Err(error) => {
                    log::error!(
                        "read_aloud: voice list fetch failed, using the fallback list: {error:#}"
                    );
                    read_aloud::fallback_voices()
                }
            };
            this.update(cx, |this, cx| {
                this.read_aloud_voices = Some(voices);
                this.read_aloud_voices_task = None;
                cx.notify();
            })
            .log_err();
        }));
    }

    /// The markdown blocks of one assistant message, in reading order.
    /// Thinking blocks are never spoken, and blank blocks render nothing.
    fn assistant_message_markdowns(
        entries: &[AgentThreadEntry],
        entry_ix: usize,
        cx: &App,
    ) -> Vec<Entity<Markdown>> {
        let Some(AgentThreadEntry::AssistantMessage(message)) = entries.get(entry_ix) else {
            return Vec::new();
        };
        message
            .chunks
            .iter()
            .filter_map(|chunk| match chunk {
                AssistantMessageChunk::Message { block, .. } => block.markdown().cloned(),
                AssistantMessageChunk::Thought { .. } => None,
            })
            .filter(|markdown| !markdown.read(cx).source().trim().is_empty())
            .collect()
    }

    /// The per-message speaker button: plays one assistant message from its
    /// top, or stops if it is the one sounding. State is recomputed at click
    /// time so a stale render cannot invert the action.
    fn toggle_read_aloud_for_message(&mut self, entry_ix: usize, cx: &mut Context<Self>) {
        let Some(read_aloud) = self.read_aloud.clone() else {
            return;
        };
        let message_markdowns =
            Self::assistant_message_markdowns(self.thread.read(cx).entries(), entry_ix, cx);
        let Some((first_block, rest)) = message_markdowns.split_first() else {
            return;
        };
        let message_complete = self.thread.read(cx).status() != ThreadStatus::Generating;
        read_aloud.update(cx, |read_aloud, cx| {
            let is_reading_this = read_aloud
                .playback_state(cx)
                .is_some_and(|state| !state.stopped)
                && read_aloud
                    .speaking()
                    .is_some_and(|speaking| message_markdowns.contains(speaking));
            if is_reading_this {
                read_aloud.stop(cx);
                return;
            }
            // A block with another block after it is finished even while the
            // turn still streams; only the trailing block can still grow.
            read_aloud.play_from_top(first_block, message_complete || !rest.is_empty(), cx);
            // The blocks after the first line up in the pending queue, so
            // the whole message plays in order.
            for (offset, block) in rest.iter().enumerate() {
                let block_complete = message_complete || offset + 1 < rest.len();
                read_aloud.enqueue_markdown(block, block_complete, cx);
            }
        });
    }

    /// Schedule a throttled save of the thread state (draft prompt, scroll position, etc.).
    /// Multiple calls within `SERIALIZATION_THROTTLE_TIME` are coalesced into a single save.
    fn schedule_save(&mut self, cx: &mut Context<Self>) {
        self._save_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(SERIALIZATION_THROTTLE_TIME)
                .await;
            this.update(cx, |this, cx| {
                if let Some(thread) = this.as_native_thread(cx) {
                    thread.update(cx, |_thread, cx| cx.notify());
                }
            })
            .ok();
        }));
    }

    pub fn handle_message_editor_event(
        &mut self,
        _editor: &Entity<MessageEditor>,
        event: &MessageEditorEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The three skill-watcher trigger points all live here:
        // - `Focus` fires when the user clicks into the input box.
        // - `SlashAutocompleteOpened` fires when the completion
        //   provider is asked for slash commands.
        // - `Send` fires when the user submits the conversation.
        // All three triggers are idempotent; firing the same one
        // repeatedly is a no-op once a scan or watch is active.
        if matches!(
            event,
            MessageEditorEvent::Focus
                | MessageEditorEvent::SlashAutocompleteOpened
                | MessageEditorEvent::Send
        ) {
            if let Some(connection) = self.as_native_connection(cx) {
                connection.ensure_skills_scan_started(cx);
                if let Some(project) = self.project.upgrade() {
                    connection.refresh_skills_for_project(project, cx);
                }
            }
        }

        match event {
            MessageEditorEvent::Send => self.send(window, cx),
            MessageEditorEvent::SendImmediately => self.interrupt_and_send(window, cx),
            MessageEditorEvent::Cancel => {
                if !self.close_thread_search(window, cx) {
                    self.cancel_generation(cx);
                }
            }
            MessageEditorEvent::Focus => {
                self.cancel_editing(&Default::default(), window, cx);
            }
            MessageEditorEvent::LostFocus => {}
            MessageEditorEvent::SlashAutocompleteOpened => {}
            MessageEditorEvent::LocalCommandInvoked(command) => {
                self.run_local_command(*command, window, cx);
            }
            MessageEditorEvent::InputAttempted { .. } => {}
            MessageEditorEvent::Edited => {}
        }
    }

    fn run_local_command(
        &mut self,
        command: PromptLocalCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match command {
            PromptLocalCommand::ThumbsUp => {
                self.handle_feedback_click(ThreadFeedback::Positive, window, cx);
                self.show_local_command_toast("Thanks for your feedback!", cx);
            }
            PromptLocalCommand::ThumbsDown => {
                self.handle_feedback_click(ThreadFeedback::Negative, window, cx);
            }
        }
    }

    fn show_local_command_toast(&self, message: impl Into<SharedString>, cx: &mut Context<Self>) {
        // Shown after positive feedback, replacing the inline button state.
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            let toast = StatusToast::new(message, cx, |this, _cx| {
                this.icon(
                    Icon::new(IconName::Check)
                        .size(IconSize::Small)
                        .color(Color::Success),
                )
            });
            workspace.toggle_status_toast(toast, cx);
        });
    }

    pub(crate) fn as_native_connection(
        &self,
        cx: &App,
    ) -> Option<Rc<agent::NativeAgentConnection>> {
        let acp_thread = self.thread.read(cx);
        acp_thread.connection().clone().downcast()
    }

    pub fn as_native_thread(&self, cx: &App) -> Option<Entity<agent::Thread>> {
        let acp_thread = self.thread.read(cx);
        self.as_native_connection(cx)?
            .thread(acp_thread.session_id(), cx)
    }

    /// Resolves the message editor's contents into content blocks. For profiles
    /// that do not enable any tools, directory mentions are expanded to inline
    /// file contents since the agent can't read files on its own.
    fn resolve_message_contents(
        &self,
        message_editor: &Entity<MessageEditor>,
        cx: &mut App,
    ) -> Task<Result<(Vec<acp::ContentBlock>, Vec<Entity<Buffer>>)>> {
        let expand = self.as_native_thread(cx).is_some_and(|thread| {
            let thread = thread.read(cx);
            AgentSettings::get_global(cx)
                .profiles
                .get(thread.profile())
                .is_some_and(|profile| profile.tools.is_empty())
        });
        message_editor.update(cx, |message_editor, cx| message_editor.contents(expand, cx))
    }

    pub fn current_model_id(&self, cx: &App) -> Option<String> {
        let selector = self.model_selector.as_ref()?;
        let model = selector.read(cx).active_model(cx)?;
        Some(model.id.to_string())
    }

    pub fn current_mode_id(&self, cx: &App) -> Option<Arc<str>> {
        if let Some(thread) = self.as_native_thread(cx) {
            Some(thread.read(cx).profile().0.clone())
        } else {
            let mode_selector = self.mode_selector.as_ref()?;
            Some(mode_selector.read(cx).mode().0)
        }
    }

    fn is_subagent(&self) -> bool {
        self.parent_session_id.is_some()
    }

    /// Returns the currently active editor, either for a message that is being
    /// edited or the editor for a new message.
    pub(crate) fn active_editor(&self, cx: &App) -> Entity<MessageEditor> {
        if let Some(index) = self.editing_message
            && let Some(editor) = self
                .entry_view_state
                .read(cx)
                .entry(index)
                .and_then(|entry| entry.message_editor())
                .cloned()
        {
            editor
        } else {
            self.message_editor.clone()
        }
    }

    pub fn has_queued_messages(&self) -> bool {
        !self.message_queue.is_empty()
    }

    // events

    pub fn handle_entry_view_event(
        &mut self,
        _: &Entity<EntryViewState>,
        event: &EntryViewEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match &event.view_event {
            ViewEvent::NewDiff(tool_call_id) => {
                if AgentSettings::get_global(cx).expand_edit_card {
                    self.entry_view_state.update(cx, |state, _cx| {
                        state.expand_tool_call(tool_call_id.clone());
                    });
                }
            }
            ViewEvent::NewTerminal(tool_call_id) => {
                if AgentSettings::get_global(cx).expand_terminal_card {
                    self.entry_view_state.update(cx, |state, _cx| {
                        state.expand_tool_call(tool_call_id.clone());
                    });
                }
            }
            ViewEvent::TerminalMovedToBackground(tool_call_id) => {
                self.entry_view_state.update(cx, |state, _cx| {
                    state.collapse_tool_call(tool_call_id);
                });
            }
            ViewEvent::MessageEditorEvent(_editor, MessageEditorEvent::Focus) => {
                if let Some(AgentThreadEntry::UserMessage(user_message)) =
                    self.thread.read(cx).entries().get(event.entry_index)
                    && self.thread.read(cx).supports_truncate(cx)
                    && user_message.client_id.is_some()
                    && !self.is_subagent()
                {
                    self.editing_message = Some(event.entry_index);
                    cx.notify();
                }
            }
            ViewEvent::MessageEditorEvent(editor, MessageEditorEvent::LostFocus) => {
                if let Some(AgentThreadEntry::UserMessage(user_message)) =
                    self.thread.read(cx).entries().get(event.entry_index)
                    && self.thread.read(cx).supports_truncate(cx)
                    && user_message.client_id.is_some()
                    && !self.is_subagent()
                {
                    if editor.read(cx).text(cx).as_str() == user_message.content.to_markdown(cx) {
                        self.editing_message = None;
                        cx.notify();
                    }
                }
            }
            ViewEvent::MessageEditorEvent(_editor, MessageEditorEvent::SendImmediately) => {}
            ViewEvent::MessageEditorEvent(editor, MessageEditorEvent::Send) => {
                if !self.is_subagent() {
                    self.regenerate(event.entry_index, editor.clone(), window, cx);
                }
            }
            ViewEvent::MessageEditorEvent(_editor, MessageEditorEvent::Cancel) => {
                self.cancel_editing(&Default::default(), window, cx);
            }
            ViewEvent::MessageEditorEvent(_editor, MessageEditorEvent::SlashAutocompleteOpened) => {
            }
            ViewEvent::MessageEditorEvent(_editor, MessageEditorEvent::LocalCommandInvoked(_)) => {}
            ViewEvent::MessageEditorEvent(_editor, MessageEditorEvent::Edited) => {}
            ViewEvent::MessageEditorEvent(_editor, MessageEditorEvent::InputAttempted { .. }) => {}
            ViewEvent::OpenDiffLocation {
                path,
                position,
                split,
            } => {
                self.open_diff_location(path, *position, *split, window, cx);
            }
        }
    }

    fn open_diff_location(
        &self,
        path: &str,
        position: Point,
        split: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(project) = self.project.upgrade() else {
            return;
        };
        let Some(project_path) = project.read(cx).find_project_path(path, cx) else {
            return;
        };

        let open_task = if split {
            self.workspace
                .update(cx, |workspace, cx| {
                    workspace.split_path(project_path, window, cx)
                })
                .log_err()
        } else {
            self.workspace
                .update(cx, |workspace, cx| {
                    workspace.open_path(project_path, None, true, window, cx)
                })
                .log_err()
        };

        let Some(open_task) = open_task else {
            return;
        };

        window
            .spawn(cx, async move |cx| {
                let item = open_task.await?;
                let Some(editor) = item.downcast::<Editor>() else {
                    return anyhow::Ok(());
                };
                editor.update_in(cx, |editor, window, cx| {
                    editor.change_selections(
                        SelectionEffects::scroll(Autoscroll::center()),
                        window,
                        cx,
                        |selections| {
                            selections.select_ranges([position..position]);
                        },
                    );
                })?;
                anyhow::Ok(())
            })
            .detach_and_log_err(cx);
    }

    // turns

    pub fn start_turn(&mut self, cx: &mut Context<Self>) -> usize {
        self.turn_fields.turn_generation += 1;
        let generation = self.turn_fields.turn_generation;
        self.turn_fields.turn_started_at = Some(Instant::now());
        self.turn_fields.last_turn_duration = None;
        self.turn_fields.last_turn_tokens = None;
        self.turn_fields.turn_tokens = Some(0);
        self.turn_fields._turn_timer_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        }));
        generation
    }

    pub fn stop_turn(&mut self, generation: usize, _cx: &mut Context<Self>) {
        if self.turn_fields.turn_generation != generation {
            return;
        }
        self.turn_fields.last_turn_duration = self
            .turn_fields
            .turn_started_at
            .take()
            .map(|started| started.elapsed());
        self.turn_fields.last_turn_tokens = self.turn_fields.turn_tokens.take();
        self.turn_fields._turn_timer_task = None;
    }

    pub fn update_turn_tokens(&mut self, cx: &App) {
        if let Some(usage) = self.thread.read(cx).token_usage() {
            if let Some(tokens) = &mut self.turn_fields.turn_tokens {
                *tokens += usage.output_tokens;
                self.emit_token_limit_telemetry_if_needed(cx);
            }
        }
    }

    fn emit_token_limit_telemetry_if_needed(&mut self, cx: &App) {
        let (ratio, agent_telemetry_id, session_id) = {
            let thread_data = self.thread.read(cx);
            let Some(token_usage) = thread_data.token_usage() else {
                return;
            };
            (
                token_usage.ratio(),
                thread_data.connection().telemetry_id(),
                thread_data.session_id().clone(),
            )
        };

        let kind = match ratio {
            acp_thread::TokenUsageRatio::Normal => {
                self.last_token_limit_telemetry = None;
                return;
            }
            acp_thread::TokenUsageRatio::Warning => "warning",
            acp_thread::TokenUsageRatio::Exceeded => "exceeded",
        };

        let should_skip = self
            .last_token_limit_telemetry
            .as_ref()
            .is_some_and(|last| *last >= ratio);
        if should_skip {
            return;
        }

        self.last_token_limit_telemetry = Some(ratio);

        telemetry::event!(
            "Agent Token Limit Warning",
            agent = agent_telemetry_id,
            session_id = session_id,
            kind = kind,
        );
    }

    // sending

    fn clear_external_source_prompt_warning(&mut self, cx: &mut Context<Self>) {
        if self.show_external_source_prompt_warning {
            self.show_external_source_prompt_warning = false;
            cx.notify();
        }
    }

    pub fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // A derived subagent has no session on the agent side, so prompting it
        // directly would name a session the agent has never heard of. Claude
        // Code's agent teams address a member by name instead, which only the
        // parent can do.
        if self.is_subagent() && self.send_to_subagent(window, cx) {
            return;
        }
        let thread = &self.thread;

        if self.is_loading_contents {
            return;
        }

        let message_editor = self.message_editor.clone();

        let is_editor_empty = message_editor.read(cx).is_empty(cx);
        let is_generating = thread.read(cx).status() != ThreadStatus::Idle;

        if is_editor_empty {
            if let Some(entry) = self.message_queue.try_fast_track(is_generating) {
                self.dispatch_queued_entry(entry, window, cx);
            }
            return;
        }

        if is_generating {
            cx.emit(AcpThreadViewEvent::Interacted);
            self.queue_message(message_editor, window, cx);
            return;
        }

        let text = message_editor.read(cx).text(cx);
        let text = text.trim();
        if text == "/login" || text == "/logout" {
            let connection = thread.read(cx).connection().clone();
            let can_login = !connection.auth_methods().is_empty();
            // Does the agent have a specific logout command? Prefer that in case they need to reset internal state.
            let logout_supported = text == "/logout"
                && self
                    .session_capabilities
                    .read()
                    .available_commands()
                    .iter()
                    .any(|available_command| available_command.name == "logout");
            if can_login && !logout_supported {
                message_editor.update(cx, |editor, cx| editor.clear(window, cx));
                self.clear_external_source_prompt_warning(cx);

                let connection = self.thread.read(cx).connection().clone();
                window.defer(cx, {
                    let server_view = self.server_view.clone();
                    move |window, cx| {
                        ConversationView::handle_auth_required(
                            server_view.clone(),
                            AuthRequired::new(),
                            connection,
                            window,
                            cx,
                        );
                    }
                });
                cx.notify();
                return;
            }
        }

        // A built-in command (e.g. `/compact`): run the bare command without
        // echoing it as a user message, and queue any trailing text the user
        // typed so it isn't silently dropped.
        let native_command =
            leading_native_command(text, self.session_capabilities.read().available_commands());
        if let Some(command_name) = native_command {
            cx.emit(AcpThreadViewEvent::Interacted);
            self.send_command_queueing_remainder(message_editor, command_name, window, cx);
            return;
        }

        cx.emit(AcpThreadViewEvent::Interacted);
        self.send_impl(message_editor, window, cx)
    }

    /// Sends a bare `/command` turn and queues everything the user typed after
    /// it as a follow-up message. The queued remainder auto-processes when the
    /// command turn stops, so e.g. `/compact do X` compacts and then runs `do X`
    /// rather than discarding it.
    fn send_command_queueing_remainder(
        &mut self,
        message_editor: Entity<MessageEditor>,
        command_name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Resolve the editor contents before clearing it: the resolve task
        // reads the editor lazily, so clearing first would wipe the contents.
        let contents = self.resolve_message_contents(&message_editor, cx);
        self.thread_error.take();
        self.thread_feedback.clear();
        self.editing_message.take();

        cx.spawn_in(window, async move |this, cx| {
            let (mut content, tracked_buffers) = contents.await?;

            cx.update(|window, cx| {
                message_editor.update(cx, |message_editor, cx| {
                    message_editor.clear(window, cx);
                });
            })?;

            // Strip the leading `/command` from the first text block; whatever
            // remains (including any later mention blocks) becomes the queued
            // follow-up message.
            if let Some(acp::ContentBlock::Text(text_content)) = content.first_mut() {
                text_content.text = strip_leading_command(&text_content.text, &command_name);
            }
            if matches!(
                content.first(),
                Some(acp::ContentBlock::Text(text)) if text.text.trim().is_empty()
            ) {
                content.remove(0);
            }

            let command_block =
                acp::ContentBlock::Text(acp::TextContent::new(format!("/{command_name}")));

            this.update_in(cx, |this, window, cx| {
                // Queue the remainder first, then start the command turn; the
                // queue auto-processes when the command turn stops.
                if !content.is_empty() {
                    this.add_to_queue(content, tracked_buffers, window, cx);
                }
                this.send_content(
                    Task::ready(Ok(Some((vec![command_block], Vec::new())))),
                    true,
                    window,
                    cx,
                );
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    pub fn send_impl(
        &mut self,
        message_editor: Entity<MessageEditor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let contents = self.resolve_message_contents(&message_editor, cx);

        self.thread_error.take();
        self.thread_feedback.clear();
        self.editing_message.take();
        // Sending a message is active engagement: un-freeze the queue if it
        // was paused by a manual stop.
        self.message_queue.resume();

        if self.should_be_following {
            self.workspace
                .update(cx, |workspace, cx| {
                    workspace.follow(CollaboratorId::Agent, window, cx);
                })
                .ok();
        }

        let contents_task = cx.spawn_in(window, async move |_this, cx| {
            let (contents, tracked_buffers) = contents.await?;

            if contents.is_empty() {
                return Ok(None);
            }

            let _ = cx.update(|window, cx| {
                message_editor.update(cx, |message_editor, cx| {
                    message_editor.clear(window, cx);
                });
            });

            Ok(Some((contents, tracked_buffers)))
        });

        self.send_content(contents_task, false, window, cx);
    }

    pub fn send_content(
        &mut self,
        contents_task: Task<anyhow::Result<Option<(Vec<acp::ContentBlock>, Vec<Entity<Buffer>>)>>>,
        is_native_command: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let session_id = self.thread.read(cx).session_id().clone();
        let parent_session_id = self.thread.read(cx).parent_session_id().cloned();
        let agent_telemetry_id = self.thread.read(cx).connection().telemetry_id();
        let is_first_message = self.thread.read(cx).entries().is_empty();
        let thread = self.thread.downgrade();

        self.is_loading_contents = true;

        let model_id = self.current_model_id(cx);
        let mode_id = self.current_mode_id(cx);
        let guard = cx.new(|_| ());
        cx.observe_release(&guard, |this, _guard, cx| {
            this.is_loading_contents = false;
            cx.notify();
        })
        .detach();

        let side = crate::agent_sidebar_side(cx);

        let task = cx.spawn_in(window, async move |this, cx| {
            let Some((contents, tracked_buffers)) = contents_task.await? else {
                return Ok(());
            };

            let generation = this.update(cx, |this, cx| {
                this.clear_external_source_prompt_warning(cx);
                let generation = this.start_turn(cx);
                this.in_flight_prompt = Some(contents.clone());
                generation
            })?;

            this.update_in(cx, |this, _window, cx| {
                this.set_editor_is_expanded(false, cx);
            })?;

            let _ = this.update(cx, |this, cx| {
                this.list_state.scroll_to_end();
                cx.notify();
            });

            let _stop_turn = defer({
                let this = this.clone();
                let mut cx = cx.clone();
                move || {
                    this.update(&mut cx, |this, cx| {
                        this.stop_turn(generation, cx);
                        cx.notify();
                    })
                    .ok();
                }
            });
            if is_first_message && thread.read_with(cx, |thread, _cx| thread.title().is_none())? {
                let text: String = contents
                    .iter()
                    .filter_map(|block| match block {
                        acp::ContentBlock::Text(text_content) => Some(text_content.text.clone()),
                        acp::ContentBlock::ResourceLink(resource_link) => {
                            Some(format!("@{}", resource_link.name))
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                let text = text.lines().next().unwrap_or("").trim();
                if !text.is_empty() {
                    let title: SharedString = util::truncate_and_trailoff(text, 200).into();
                    thread.update(cx, |thread, cx| {
                        thread.set_provisional_title(title, cx);
                    })?;
                }
            }

            let turn_start_time = Instant::now();
            let send = thread.update(cx, |thread, cx| {
                thread.action_log().update(cx, |action_log, cx| {
                    for buffer in tracked_buffers {
                        action_log.buffer_read(buffer, cx)
                    }
                });
                drop(guard);

                telemetry::event!(
                    "Agent Message Sent",
                    agent = agent_telemetry_id,
                    session = session_id,
                    parent_session_id = parent_session_id.as_ref().map(|id| id.to_string()),
                    model = model_id,
                    mode = mode_id,
                    side = side
                );

                if is_native_command {
                    thread.send_command(contents, cx)
                } else {
                    thread.send(contents, cx)
                }
            })?;

            let _ = this.update(cx, |this, cx| {
                this.sync_generating_indicator(cx);
                cx.notify();
            });

            let res = send.await;
            let turn_time_ms = turn_start_time.elapsed().as_millis();
            drop(_stop_turn);
            let status = if res.is_ok() {
                let _ = this.update(cx, |this, _| this.in_flight_prompt.take());
                "success"
            } else {
                "failure"
            };
            telemetry::event!(
                "Agent Turn Completed",
                agent = agent_telemetry_id,
                session = session_id,
                parent_session_id = parent_session_id.as_ref().map(|id| id.to_string()),
                model = model_id,
                mode = mode_id,
                status,
                turn_time_ms,
                side = side
            );
            res.map(|_| ())
        });

        cx.spawn(async move |this, cx| {
            if let Err(err) = task.await {
                this.update(cx, |this, cx| {
                    this.handle_thread_error(err, cx);
                })
                .ok();
            } else {
                this.update(cx, |this, cx| {
                    let should_be_following = this
                        .workspace
                        .update(cx, |workspace, _| {
                            workspace.is_being_followed(CollaboratorId::Agent)
                        })
                        .unwrap_or_default();
                    this.should_be_following = should_be_following;
                })
                .ok();
            }
        })
        .detach();
    }

    pub fn interrupt_and_send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let thread = &self.thread;

        if self.is_loading_contents {
            return;
        }

        cx.emit(AcpThreadViewEvent::Interacted);

        let message_editor = self.message_editor.clone();
        if thread.read(cx).status() == ThreadStatus::Idle {
            self.send_impl(message_editor, window, cx);
            return;
        }

        self.stop_current_and_send_new_message(message_editor, window, cx);
    }

    fn stop_current_and_send_new_message(
        &mut self,
        message_editor: Entity<MessageEditor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let thread = self.thread.clone();
        self.message_queue.pause();

        let cancelled = thread.update(cx, |thread, cx| thread.cancel(cx));

        cx.spawn_in(window, async move |this, cx| {
            cancelled.await;

            this.update_in(cx, |this, window, cx| {
                this.send_impl(message_editor, window, cx);
            })
            .ok();
        })
        .detach();
    }

    pub(crate) fn handle_thread_error(
        &mut self,
        error: impl Into<ThreadError>,
        cx: &mut Context<Self>,
    ) {
        let error = error.into();
        self.emit_thread_error_telemetry(&error, cx);
        self.thread_error = Some(error);
        cx.notify();
    }

    fn emit_thread_error_telemetry(&self, error: &ThreadError, cx: &mut Context<Self>) {
        let (error_kind, acp_error_code, message): (&str, Option<SharedString>, SharedString) =
            match error {
                ThreadError::PaymentRequired => (
                    "payment_required",
                    None,
                    "You reached your free usage limit. Upgrade to Zed Pro for more prompts."
                        .into(),
                ),
                ThreadError::Refusal => {
                    let model_or_agent_name = self.current_model_name(cx);
                    let message = format!(
                        "{} refused to respond to this prompt. This can happen when a model believes the prompt violates its content policy or safety guidelines, so rephrasing it can sometimes address the issue.",
                        model_or_agent_name
                    );
                    ("refusal", None, message.into())
                }
                ThreadError::DataRetentionConsentRequired => {
                    let message = format!(
                        "{} is not available with Zero Data Retention.",
                        self.current_model_name(cx)
                    );
                    ("data_retention_consent_required", None, message.into())
                }
                ThreadError::AuthenticationRequired(message) => {
                    ("authentication_required", None, message.clone())
                }
                ThreadError::RateLimitExceeded { provider } => (
                    "rate_limit_exceeded",
                    None,
                    format!("{provider}'s rate limit was reached.").into(),
                ),
                ThreadError::ServerOverloaded { provider } => (
                    "server_overloaded",
                    None,
                    format!("{provider}'s servers are temporarily unavailable.").into(),
                ),
                ThreadError::PromptTooLarge => (
                    "prompt_too_large",
                    None,
                    "Context too large for the model's context window.".into(),
                ),
                ThreadError::NoCredentials { provider } => (
                    "no_api_key",
                    None,
                    format!("No credentials configured for {provider}.").into(),
                ),
                ThreadError::StreamError { provider } => (
                    "stream_error",
                    None,
                    format!("Connection to {provider}'s API was interrupted.").into(),
                ),
                ThreadError::AuthenticationFailed { provider } => (
                    "invalid_api_key",
                    None,
                    format!("Authentication with {provider} failed.").into(),
                ),
                ThreadError::PermissionDenied { provider, message } => (
                    "permission_denied",
                    None,
                    message.clone().unwrap_or_else(|| {
                        format!(
                            "{provider}'s API rejected the request due to insufficient permissions."
                        )
                        .into()
                    }),
                ),
                ThreadError::RequestFailed => (
                    "request_failed",
                    None,
                    "Request could not be completed after multiple attempts.".into(),
                ),
                ThreadError::MaxOutputTokens => (
                    "max_output_tokens",
                    None,
                    "Model reached its maximum output length.".into(),
                ),
                ThreadError::NoModelSelected => {
                    ("no_model_selected", None, "No model selected.".into())
                }
                ThreadError::ApiError { provider } => (
                    "api_error",
                    None,
                    format!("{provider}'s API returned an unexpected error.").into(),
                ),
                ThreadError::Other {
                    acp_error_code,
                    message,
                } => ("other", acp_error_code.clone(), message.clone()),
            };

        let agent_telemetry_id = self.thread.read(cx).connection().telemetry_id();
        let session_id = self.thread.read(cx).session_id().clone();
        let parent_session_id = self
            .thread
            .read(cx)
            .parent_session_id()
            .map(|id| id.to_string());

        telemetry::event!(
            "Agent Panel Error Shown",
            agent = agent_telemetry_id,
            session_id = session_id,
            parent_session_id = parent_session_id,
            kind = error_kind,
            acp_error_code = acp_error_code,
            message = message,
        );
    }

    pub fn cancel_generation(&mut self, cx: &mut Context<Self>) {
        self.thread_retry_status.take();
        self.thread_error.take();
        self.message_queue.pause();
        self._cancel_task = Some(self.thread.update(cx, |thread, cx| thread.cancel(cx)));
        self.sync_generating_indicator(cx);
        cx.notify();
    }

    pub fn retry_generation(&mut self, cx: &mut Context<Self>) {
        self.thread_error.take();

        let thread = &self.thread;
        if !thread.read(cx).can_retry(cx) {
            return;
        }

        let task = thread.update(cx, |thread, cx| thread.retry(cx));
        cx.emit(AcpThreadViewEvent::Interacted);
        self.sync_generating_indicator(cx);
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = task.await;

            this.update(cx, |this, cx| {
                if let Err(err) = result {
                    this.handle_thread_error(err, cx);
                }
            })
        })
        .detach();
    }

    pub fn regenerate(
        &mut self,
        entry_ix: usize,
        message_editor: Entity<MessageEditor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.is_loading_contents {
            return;
        }
        let thread = self.thread.clone();

        let Some(client_id) = thread.update(cx, |thread, _| {
            thread
                .entries()
                .get(entry_ix)?
                .user_message()?
                .client_id
                .clone()
        }) else {
            return;
        };

        cx.spawn_in(window, async move |this, cx| {
            // Check if there are any edits from prompts before the one being regenerated.
            //
            // If there are, we keep/accept them since we're not regenerating the prompt that created them.
            //
            // If editing the prompt that generated the edits, they are auto-rejected
            // through the `rewind` function in the `acp_thread`.
            //
            // Subagent edits never show up as diffs in the parent thread's entries (they
            // are only forwarded to the parent's action log), so treat any earlier
            // subagent tool call as potentially having edits. Keeping all edits is a
            // no-op when the subagent didn't make any.
            let has_earlier_edits = thread.read_with(cx, |thread, _| {
                thread.entries().iter().take(entry_ix).any(|entry| {
                    entry.diffs().next().is_some()
                        || matches!(
                            entry,
                            AgentThreadEntry::ToolCall(tool_call) if tool_call.is_subagent()
                        )
                })
            });

            if has_earlier_edits {
                thread.update(cx, |thread, cx| {
                    thread.action_log().update(cx, |action_log, cx| {
                        action_log.keep_all_edits(None, cx);
                    });
                });
            }

            thread
                .update(cx, |thread, cx| thread.rewind(client_id, cx))
                .await?;
            this.update_in(cx, |thread, window, cx| {
                cx.emit(AcpThreadViewEvent::Interacted);
                thread.send_impl(message_editor, window, cx);
                thread.activation_focus_handle(cx).focus(window, cx);
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    // message queueing

    fn queue_message(
        &mut self,
        message_editor: Entity<MessageEditor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let is_idle = self.thread.read(cx).status() == acp_thread::ThreadStatus::Idle;

        if is_idle {
            self.send_impl(message_editor, window, cx);
            return;
        }

        let contents = self.resolve_message_contents(&message_editor, cx);

        cx.spawn_in(window, async move |this, cx| {
            let (content, tracked_buffers) = contents.await?;

            if content.is_empty() {
                return Ok::<(), anyhow::Error>(());
            }

            this.update_in(cx, |this, window, cx| {
                this.add_to_queue(content, tracked_buffers, window, cx);
                message_editor.update(cx, |message_editor, cx| {
                    message_editor.clear(window, cx);
                });
                cx.notify();
            })?;
            Ok(())
        })
        .detach_and_log_err(cx);
    }

    pub fn add_to_queue(
        &mut self,
        content: Vec<acp::ContentBlock>,
        tracked_buffers: Vec<Entity<Buffer>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The ID must be allocated up front so the editor event subscription
        // can capture it before the entry (which owns the subscription) exists.
        let id = self.message_queue.next_id();

        let editor = cx.new(|cx| {
            let mut editor = MessageEditor::new(
                self.workspace.clone(),
                self.project.clone(),
                None,
                self.session_capabilities.clone(),
                self.agent_id.clone(),
                "",
                EditorMode::AutoHeight {
                    min_lines: 1,
                    max_lines: Some(10),
                },
                window,
                cx,
            );
            editor.set_read_only(true, cx);
            editor.set_message(content.clone(), window, cx);
            editor
        });

        let subscription =
            cx.subscribe_in(&editor, window, move |this, _editor, event, window, cx| {
                this.handle_queue_editor_event(id, event, window, cx);
            });

        self.message_queue.enqueue(QueueEntry {
            id,
            content,
            tracked_buffers,
            steer: false,
            editor,
            _subscription: subscription,
        });
        self.sync_queue_flag_to_native_thread(cx);
        cx.notify();
    }

    fn handle_queue_editor_event(
        &mut self,
        id: QueueEntryId,
        event: &MessageEditorEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            MessageEditorEvent::InputAttempted {
                attempt,
                cursor_offset,
            } => {
                self.move_queued_message_to_main_editor(
                    id,
                    Some(attempt.clone()),
                    Some(*cursor_offset),
                    window,
                    cx,
                );
            }
            MessageEditorEvent::LostFocus => {
                self.save_queued_message(id, cx);
            }
            MessageEditorEvent::Cancel | MessageEditorEvent::Send => {
                window.focus(&self.message_editor.focus_handle(cx), cx);
            }
            MessageEditorEvent::SendImmediately => {
                self.send_queued_message_now(id, window, cx);
            }
            _ => {}
        }
    }

    fn save_queued_message(&mut self, id: QueueEntryId, cx: &mut Context<Self>) {
        let Some(entry) = self.message_queue.entry_by_id(id) else {
            return;
        };
        let contents_task = entry
            .editor
            .update(cx, |editor, cx| editor.contents(false, cx));

        cx.spawn(async move |this, cx| {
            let (content, tracked_buffers) = contents_task.await?;

            this.update(cx, |this, cx| {
                if let Some(entry) = this.message_queue.entry_by_id_mut(id) {
                    entry.content = content;
                    entry.tracked_buffers = tracked_buffers;
                }
                cx.notify();
            })?;

            Ok::<(), anyhow::Error>(())
        })
        .detach_and_log_err(cx);
    }

    pub fn remove_from_queue(
        &mut self,
        id: QueueEntryId,
        cx: &mut Context<Self>,
    ) -> Option<QueueEntry> {
        let removed = self.message_queue.remove(id);
        if removed.is_some() {
            self.sync_queue_flag_to_native_thread(cx);
        }
        removed
    }

    fn toggle_queue_entry_steer(&mut self, id: QueueEntryId, cx: &mut Context<Self>) {
        self.message_queue.toggle_steer(id);
        self.sync_queue_flag_to_native_thread(cx);
        cx.notify();
    }

    pub fn sync_queue_flag_to_native_thread(&self, cx: &mut Context<Self>) {
        if let Some(native_thread) = self.as_native_thread(cx) {
            // By default queued messages wait for the turn to fully complete.
            // Only a "steering" front message ends the turn at the next boundary.
            let end_at_boundary = self.message_queue.front_wants_steer();
            native_thread.update(cx, |thread, _| {
                thread.set_end_turn_at_next_boundary(end_at_boundary);
            });
        }
    }

    pub fn send_queued_message_now(
        &mut self,
        id: QueueEntryId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let is_generating = self.thread.read(cx).status() == acp_thread::ThreadStatus::Generating;
        if let Some(entry) = self.message_queue.send_now(id, is_generating) {
            self.dispatch_queued_entry(entry, window, cx);
        }
    }

    /// Delivers `content` into the turn already running, when the agent
    /// supports steering and there is a turn to steer.
    ///
    /// Returns whether the message was handed off. `false` leaves the caller to
    /// interrupt the turn and prompt, which is the only option for agents
    /// without the extension.
    ///
    /// This is what keeps a follow-up from stopping the thread's background
    /// subagents: they run inside the turn an interrupt would tear down.
    fn steer_queued_entry(
        &mut self,
        content: &[acp::ContentBlock],
        tracked_buffers: Vec<Entity<Buffer>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.thread.read(cx).status() != ThreadStatus::Generating {
            return false;
        }
        let content = content.to_vec();
        let Some(steer) = self
            .thread
            .update(cx, |thread, cx| thread.steer(content.clone(), cx))
        else {
            return false;
        };

        cx.spawn_in(window, async move |this, cx| {
            let outcome = steer.await;
            this.update_in(cx, |this, window, cx| match outcome {
                Ok(SteerOutcome::Injected) => {
                    this.thread.update(cx, |thread, cx| {
                        thread.action_log().update(cx, |action_log, cx| {
                            for buffer in tracked_buffers {
                                action_log.buffer_read(buffer, cx)
                            }
                        });
                    });
                    this.list_state.scroll_to_end();
                    cx.notify();
                }
                // The turn ended while the request was in flight, or steering
                // failed outright: either way the message was never delivered,
                // so send it as an ordinary prompt rather than losing it.
                outcome => {
                    if let Err(error) = outcome {
                        log::error!("failed to steer turn, sending as a prompt: {error:#}");
                    }
                    this.send_content(
                        Task::ready(Ok(Some((content, tracked_buffers)))),
                        false,
                        window,
                        cx,
                    );
                }
            })
            .log_err();
        })
        .detach();

        true
    }

    /// The shared "actually send this entry" path, used by fast-track,
    /// auto-processing on Stopped, and "Send Now". The entry must already have
    /// been removed from the queue.
    pub fn dispatch_queued_entry(
        &mut self,
        entry: QueueEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sync_queue_flag_to_native_thread(cx);

        cx.emit(AcpThreadViewEvent::Interacted);

        self.message_editor.focus_handle(cx).focus(window, cx);

        let content = entry.content;
        let tracked_buffers = entry.tracked_buffers;

        // A queued message can itself be a built-in command (e.g. the user typed
        // `/compact` while a turn was generating). Detect that so we run it as a
        // command turn without echoing it as a user message, matching the
        // non-queued path.
        let is_native_command = content
            .first()
            .and_then(|block| match block {
                acp::ContentBlock::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .and_then(|text| {
                leading_native_command(text, self.session_capabilities.read().available_commands())
            })
            .is_some();

        // Steering delivers the message into the turn already running. Falling
        // back to interrupting it is what used to stop the thread's background
        // subagents, whose work the agent runs inside that turn.
        if !is_native_command
            && self.steer_queued_entry(&content, tracked_buffers.clone(), window, cx)
        {
            return;
        }

        let cancelled = self.thread.update(cx, |thread, cx| thread.cancel(cx));

        let workspace = self.workspace.clone();

        let should_be_following = self.should_be_following;
        let contents_task = cx.spawn_in(window, async move |_this, cx| {
            cancelled.await;
            if should_be_following {
                workspace
                    .update_in(cx, |workspace, window, cx| {
                        workspace.follow(CollaboratorId::Agent, window, cx);
                    })
                    .ok();
            }

            Ok(Some((content, tracked_buffers)))
        });

        self.send_content(contents_task, is_native_command, window, cx);
    }

    pub fn move_queued_message_to_main_editor(
        &mut self,
        id: QueueEntryId,
        attempt: Option<InputAttempt>,
        cursor_offset: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(queued_message) = self.remove_from_queue(id, cx) else {
            return false;
        };
        let queued_content = queued_message.content;
        let message_editor = self.message_editor.clone();

        window.focus(&message_editor.focus_handle(cx), cx);

        let adjusted_cursor_offset = if message_editor.read(cx).is_empty(cx) {
            message_editor.update(cx, |editor, cx| {
                editor.set_message(queued_content, window, cx);
            });
            cursor_offset
        } else {
            let existing_len = message_editor.read(cx).text(cx).len();
            let separator = "\n\n";
            message_editor.update(cx, |editor, cx| {
                editor.append_message(queued_content, Some(separator), window, cx);
            });
            cursor_offset.map(|offset| existing_len + separator.len() + offset)
        };

        message_editor.update(cx, |editor, cx| {
            if let Some(offset) = adjusted_cursor_offset {
                editor.set_cursor_offset(offset, window, cx);
            }
            match attempt {
                Some(InputAttempt::Text(text)) => {
                    editor.insert_text(&text, window, cx);
                }
                Some(InputAttempt::Paste(clipboard)) => {
                    editor.paste_item(&clipboard, window, cx);
                }
                None => {}
            }
        });

        cx.notify();
        true
    }

    fn handle_message_editor_move_up(
        &mut self,
        _: &zed_actions::editor::MoveUp,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.message_editor.read(cx).is_empty(cx) {
            cx.propagate();
            return;
        }
        let Some(last_id) = self.message_queue.last_id() else {
            cx.propagate();
            return;
        };
        self.move_queued_message_to_main_editor(last_id, None, None, window, cx);
    }

    // editor methods

    pub fn expand_message_editor(
        &mut self,
        _: &ExpandMessageEditor,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.list_state.item_count() == 0 {
            return;
        }
        self.set_editor_is_expanded(!self.editor_expanded, cx);
        cx.stop_propagation();
        cx.notify();
    }

    pub fn set_editor_is_expanded(&mut self, is_expanded: bool, cx: &mut Context<Self>) {
        self.editor_expanded = is_expanded;
        self.sync_editor_mode(cx);
        cx.notify();
    }

    pub fn handle_title_editor_event(
        &mut self,
        title_editor: &Entity<Editor>,
        event: &EditorEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            EditorEvent::BufferEdited => {
                // We only want to set the title if the user has actively edited
                // it. If the title editor is not focused, we programmatically
                // changed the text, so we don't want to set the title again.
                if !title_editor.read(cx).is_focused(window) {
                    return;
                }

                let new_title = title_editor.read(cx).text(cx);
                if new_title.is_empty() {
                    return;
                }
                self.apply_renamed_title(SharedString::from(new_title), cx);
            }
            EditorEvent::Blurred => {
                if title_editor.read(cx).text(cx).is_empty() {
                    title_editor.update(cx, |editor, cx| {
                        editor.set_text(DEFAULT_THREAD_TITLE, window, cx);
                    });
                }
            }
            _ => {}
        }
    }

    /// Renames the thread, mirroring the editor text and persisting the new
    /// title. Used by callers outside of the title editor (e.g. the sidebar's
    /// inline rename) so that they go through the same persistence path as
    /// the in-thread title editor.
    pub fn rename(&mut self, title: SharedString, window: &mut Window, cx: &mut Context<Self>) {
        if self.title_editor.read(cx).text(cx) != title.as_ref() {
            self.title_editor.update(cx, |editor, cx| {
                editor.set_text(title.clone(), window, cx);
            });
        }
        self.apply_renamed_title(title, cx);
    }

    fn apply_renamed_title(&mut self, title: SharedString, cx: &mut Context<Self>) {
        if let Some(store) = ThreadMetadataStore::try_global(cx)
            && !self.is_subagent()
        {
            let thread_id = self.root_thread_id;
            store.update(cx, |store, cx| {
                store.set_title_override(thread_id, title.clone(), cx);
            });
        }
        self.thread.update(cx, |thread, cx| {
            if thread.can_set_title(cx) {
                thread.set_title(title, cx).detach_and_log_err(cx);
            }
        });
    }

    pub fn cancel_editing(&mut self, _: &ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.editing_message.take()
            && let Some(editor) = &self
                .entry_view_state
                .read(cx)
                .entry(index)
                .and_then(|e| e.message_editor())
                .cloned()
        {
            editor.update(cx, |editor, cx| {
                if let Some(user_message) = self
                    .thread
                    .read(cx)
                    .entries()
                    .get(index)
                    .and_then(|e| e.user_message())
                {
                    editor.set_message(user_message.chunks.clone(), window, cx);
                }
            })
        };
        self.message_editor.focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    pub fn authorize_tool_call(
        &mut self,
        session_id: acp::SessionId,
        tool_call_id: acp::ToolCallId,
        outcome: SelectedPermissionOutcome,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.conversation.update(cx, |conversation, cx| {
            conversation.authorize_tool_call(session_id, tool_call_id, outcome, cx);
        });
        if self.should_be_following {
            self.workspace
                .update(cx, |workspace, cx| {
                    workspace.follow(CollaboratorId::Agent, window, cx);
                })
                .ok();
        }
        cx.notify();
    }

    pub fn allow_always(&mut self, _: &AllowAlways, window: &mut Window, cx: &mut Context<Self>) {
        if self.pending_allow_blocked_by_confusables(cx) {
            return;
        }
        self.authorize_pending_tool_call(acp::PermissionOptionKind::AllowAlways, window, cx);
    }

    pub fn allow_once(&mut self, _: &AllowOnce, window: &mut Window, cx: &mut Context<Self>) {
        if self.pending_allow_blocked_by_confusables(cx) {
            return;
        }
        self.authorize_pending_with_granularity(true, window, cx);
    }

    /// Whether the currently pending permission prompt is blocked by an
    /// unacknowledged surprising-Unicode warning, so the keyboard allow
    /// shortcuts must be ignored (mirroring the disabled allow buttons).
    fn pending_allow_blocked_by_confusables(&self, cx: &Context<Self>) -> bool {
        let session_id = self.thread.read(cx).session_id().clone();
        let Some((_, tool_call_id, _)) = self
            .conversation
            .read(cx)
            .pending_tool_call(&session_id, cx)
        else {
            return false;
        };
        self.thread.read(cx).entries().iter().any(|entry| {
            matches!(
                entry,
                AgentThreadEntry::ToolCall(call)
                    if call.id == tool_call_id && self.sandbox_confusables_block_allow(call, cx)
            )
        })
    }

    pub fn reject_once(&mut self, _: &RejectOnce, window: &mut Window, cx: &mut Context<Self>) {
        self.authorize_pending_with_granularity(false, window, cx);
    }

    /// This thread's standing at the moment a spoken command lands.
    pub fn voice_candidate(&self, is_active: bool, cx: &App) -> VoiceCandidate {
        let session_id = self.thread.read(cx).session_id().clone();
        let blocked_on_approval = self
            .conversation
            .read(cx)
            .pending_tool_call_for_session(&session_id, cx)
            .is_some();
        let last_spoke_at = self
            .read_aloud
            .as_ref()
            .and_then(|read_aloud| read_aloud.read(cx).last_spoke_at());
        VoiceCandidate {
            session_id,
            blocked_on_approval,
            last_spoke_at,
            is_active,
        }
    }

    pub fn read_aloud_entity(&self) -> Option<&Entity<read_aloud::ReadAloud>> {
        self.read_aloud.as_ref()
    }

    /// Delivers spoken text to the agent, steering the turn already running
    /// rather than interrupting it when there is one.
    ///
    /// Interrupting is what used to stop a thread's background subagents,
    /// whose work the agent runs inside that turn, so the ordinary typed path
    /// prefers steering too; this follows it.
    pub fn send_voice_text(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        let content = vec![acp::ContentBlock::Text(acp::TextContent::new(text))];
        if self.steer_queued_entry(&content, Vec::new(), window, cx) {
            return;
        }
        self.send_content(
            Task::ready(Ok(Some((content, Vec::new())))),
            false,
            window,
            cx,
        );
    }

    /// What this thread is blocked on, phrased for speech.
    ///
    /// `None` when nothing is waiting, which is how the caller tells "there is
    /// nothing to approve" from "there is, and here is what it is".
    pub fn pending_tool_call_description(&self, cx: &App) -> Option<String> {
        let session_id = self.thread.read(cx).session_id().clone();
        let tool_call_id = self
            .conversation
            .read(cx)
            .pending_tool_call_for_session(&session_id, cx)?;
        let (_, tool_call) = self.thread.read(cx).tool_call(&tool_call_id)?;
        Some(tool_call.label.read(cx).source().to_string())
    }

    pub fn authorize_pending_tool_call(
        &mut self,
        kind: acp::PermissionOptionKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<()> {
        let session_id = self.thread.read(cx).session_id().clone();
        self.conversation.update(cx, |conversation, cx| {
            conversation.authorize_pending_tool_call(&session_id, kind, cx)
        })?;
        if self.should_be_following {
            self.workspace
                .update(cx, |workspace, cx| {
                    workspace.follow(CollaboratorId::Agent, window, cx);
                })
                .ok();
        }
        cx.notify();
        Some(())
    }

    fn has_pending_request_elicitation(&self, cx: &App) -> bool {
        self.server_view
            .read_with(cx, |server_view, cx| {
                server_view
                    .request_elicitation_store()
                    .is_some_and(|store| {
                        store.read(cx).elicitations().iter().any(|elicitation| {
                            matches!(elicitation.status, ElicitationStatus::Pending { .. })
                        })
                    })
            })
            .unwrap_or(false)
    }

    pub fn sync_elicitation_state_for_entry(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let elicitation_id = {
            let thread = self.thread.read(cx);
            let Some(AgentThreadEntry::Elicitation(elicitation_id)) = thread.entries().get(index)
            else {
                return;
            };
            elicitation_id.clone()
        };

        let thread = self.thread.read(cx);
        let entry = thread.elicitation(&elicitation_id).map(|(_, elicitation)| {
            (
                elicitation_id.clone(),
                matches!(elicitation.status, ElicitationStatus::Pending { .. }),
                match &elicitation.request.mode {
                    acp::ElicitationMode::Form(mode) => Some(mode.requested_schema.clone()),
                    _ => None,
                },
            )
        });

        let Some((id, is_pending, schema)) = entry else {
            return;
        };

        if is_pending
            && let Some(schema) = schema
            && !self.elicitation_form_states.contains_key(&id)
        {
            self.elicitation_form_states
                .insert(id, ElicitationFormState::new(&schema, window, cx));
        } else if !is_pending {
            self.elicitation_form_states.remove(&id);
        }
    }

    fn sync_existing_elicitation_states(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let entry_count = self.thread.read(cx).entries().len();
        for index in 0..entry_count {
            self.sync_elicitation_state_for_entry(index, window, cx);
        }
    }

    #[cfg(test)]
    pub(crate) fn has_elicitation_form_state(&self, id: &ElicitationEntryId) -> bool {
        self.elicitation_form_states.contains_key(id)
    }

    fn submit_elicitation(
        &mut self,
        elicitation_id: ElicitationEntryId,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mode = self
            .thread
            .read(cx)
            .elicitation(&elicitation_id)
            .map(|(_, elicitation)| elicitation.request.mode.clone());

        let Some(mode) = mode else {
            return;
        };

        match mode {
            acp::ElicitationMode::Form(mode) => {
                let Some(state) = self.elicitation_form_states.get_mut(&elicitation_id) else {
                    return;
                };
                let Some(submission) = state.begin_submission(cx) else {
                    return;
                };
                let schema = mode.requested_schema;
                let validation_task = cx.background_spawn(async move {
                    let result = submission.validate(&schema);
                    (submission, result)
                });
                cx.notify();
                cx.spawn(async move |this, cx| {
                    let (submission, result) = validation_task.await;
                    this.update(cx, |this, cx| {
                        let is_current = this
                            .elicitation_form_states
                            .get_mut(&elicitation_id)
                            .is_some_and(|state| {
                                state.validation_matches_current_values(&submission, cx)
                            });
                        if !is_current {
                            cx.notify();
                            return;
                        }
                        match result {
                            Ok(content) => {
                                this.respond_to_elicitation(
                                    elicitation_id,
                                    acp::CreateElicitationResponse::new(
                                        acp::ElicitationAction::Accept(
                                            acp::ElicitationAcceptAction::new().content(content),
                                        ),
                                    ),
                                    cx,
                                );
                            }
                            Err(errors) => {
                                if let Some(state) =
                                    this.elicitation_form_states.get_mut(&elicitation_id)
                                {
                                    state.set_errors(errors);
                                }
                                cx.notify();
                            }
                        }
                    })
                    .log_err();
                })
                .detach();
            }
            acp::ElicitationMode::Url(_) => {
                self.respond_to_elicitation(
                    elicitation_id,
                    acp::CreateElicitationResponse::new(acp::ElicitationAction::Accept(
                        acp::ElicitationAcceptAction::new(),
                    )),
                    cx,
                );
            }
            _ => {}
        }
    }

    fn decline_elicitation(
        &mut self,
        elicitation_id: ElicitationEntryId,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.respond_to_elicitation(
            elicitation_id,
            acp::CreateElicitationResponse::new(acp::ElicitationAction::Decline),
            cx,
        );
    }

    fn cancel_elicitation(
        &mut self,
        elicitation_id: ElicitationEntryId,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.respond_to_elicitation(
            elicitation_id,
            acp::CreateElicitationResponse::new(acp::ElicitationAction::Cancel),
            cx,
        );
    }

    fn dismiss_url_elicitation(
        &mut self,
        elicitation_id: ElicitationEntryId,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.elicitation_form_states.remove(&elicitation_id);
        self.thread.update(cx, |thread, cx| {
            thread.cancel_elicitation(&elicitation_id, cx);
        });
        cx.notify();
    }

    fn respond_to_elicitation(
        &mut self,
        elicitation_id: ElicitationEntryId,
        response: acp::CreateElicitationResponse,
        cx: &mut Context<Self>,
    ) {
        let session_id = self.session_id.clone();
        self.elicitation_form_states.remove(&elicitation_id);
        self.conversation.update(cx, |conversation, cx| {
            conversation.respond_to_elicitation(session_id, elicitation_id, response, cx);
        });
        cx.notify();
    }

    fn handle_authorize_tool_call(
        &mut self,
        action: &AuthorizeToolCall,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let tool_call_id = acp::ToolCallId::new(action.tool_call_id.clone());
        let option_id = acp::PermissionOptionId::new(action.option_id.clone());
        let option_kind = match action.option_kind.as_str() {
            "AllowOnce" => acp::PermissionOptionKind::AllowOnce,
            "AllowAlways" => acp::PermissionOptionKind::AllowAlways,
            "RejectOnce" => acp::PermissionOptionKind::RejectOnce,
            "RejectAlways" => acp::PermissionOptionKind::RejectAlways,
            _ => acp::PermissionOptionKind::AllowOnce,
        };

        let session_id = self.thread.read(cx).session_id().clone();
        self.authorize_tool_call(
            session_id,
            tool_call_id,
            SelectedPermissionOutcome::new(option_id, option_kind),
            window,
            cx,
        );
    }

    pub fn handle_select_permission_granularity(
        &mut self,
        action: &SelectPermissionGranularity,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let tool_call_id = acp::ToolCallId::new(action.tool_call_id.clone());
        self.permission_selections
            .insert(tool_call_id, PermissionSelection::Choice(action.index));

        cx.notify();
    }

    pub fn handle_toggle_command_pattern(
        &mut self,
        action: &crate::ToggleCommandPattern,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let tool_call_id = acp::ToolCallId::new(action.tool_call_id.clone());

        match self.permission_selections.get_mut(&tool_call_id) {
            Some(PermissionSelection::SelectedPatterns(checked)) => {
                // Already in pattern mode — toggle the individual pattern.
                if let Some(pos) = checked.iter().position(|&i| i == action.pattern_index) {
                    checked.swap_remove(pos);
                } else {
                    checked.push(action.pattern_index);
                }
            }
            _ => {
                // First click: activate "Select options" with all patterns checked.
                let thread = self.thread.read(cx);
                let pattern_count = thread
                    .entries()
                    .iter()
                    .find_map(|entry| {
                        if let AgentThreadEntry::ToolCall(call) = entry {
                            if call.id == tool_call_id {
                                if let ToolCallStatus::WaitingForConfirmation { options, .. } =
                                    &call.status
                                {
                                    if let PermissionOptions::DropdownWithPatterns {
                                        patterns,
                                        ..
                                    } = options
                                    {
                                        return Some(patterns.len());
                                    }
                                }
                            }
                        }
                        None
                    })
                    .unwrap_or(0);
                self.permission_selections.insert(
                    tool_call_id,
                    PermissionSelection::SelectedPatterns((0..pattern_count).collect()),
                );
            }
        }
        cx.notify();
    }

    fn authorize_pending_with_granularity(
        &mut self,
        is_allow: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<()> {
        let session_id = self.thread.read(cx).session_id().clone();
        let (returned_session_id, tool_call_id, _) = self
            .conversation
            .read(cx)
            .pending_tool_call(&session_id, cx)?;
        self.authorize_with_granularity(returned_session_id, tool_call_id, is_allow, window, cx)
    }

    fn authorize_with_granularity(
        &mut self,
        session_id: acp::SessionId,
        tool_call_id: acp::ToolCallId,
        is_allow: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<()> {
        let selection = self.permission_selections.get(&tool_call_id).cloned();
        let result = self.conversation.update(cx, |conversation, cx| {
            conversation.authorize_with_granularity(
                session_id,
                tool_call_id,
                selection.as_ref(),
                is_allow,
                cx,
            )
        });
        if self.should_be_following {
            self.workspace
                .update(cx, |workspace, cx| {
                    workspace.follow(CollaboratorId::Agent, window, cx);
                })
                .ok();
        }
        cx.notify();
        result
    }

    // edits

    pub fn keep_all(&mut self, _: &KeepAll, _window: &mut Window, cx: &mut Context<Self>) {
        let thread = &self.thread;
        let telemetry = ActionLogTelemetry::from(thread.read(cx));
        let action_log = thread.read(cx).action_log().clone();
        action_log.update(cx, |action_log, cx| {
            action_log.keep_all_edits(Some(telemetry), cx)
        });
    }

    pub fn reject_all(&mut self, _: &RejectAll, _window: &mut Window, cx: &mut Context<Self>) {
        let thread = &self.thread;
        let telemetry = ActionLogTelemetry::from(thread.read(cx));
        let action_log = thread.read(cx).action_log().clone();
        let has_changes = action_log.read(cx).changed_buffers(cx).next().is_some();

        action_log
            .update(cx, |action_log, cx| {
                action_log.reject_all_edits(Some(telemetry), cx)
            })
            .detach();

        if has_changes {
            if let Some(workspace) = self.workspace.upgrade() {
                workspace.update(cx, |workspace, cx| {
                    crate::ui::show_undo_reject_toast(workspace, action_log, cx);
                });
            }
        }
    }

    pub fn undo_last_reject(
        &mut self,
        _: &UndoLastReject,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let thread = &self.thread;
        let action_log = thread.read(cx).action_log().clone();
        action_log
            .update(cx, |action_log, cx| action_log.undo_last_reject(cx))
            .detach()
    }

    pub fn open_edited_buffer(
        &mut self,
        buffer: &Entity<Buffer>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let thread = &self.thread;

        let Some(diff) =
            AgentDiffPane::deploy(thread.clone(), self.workspace.clone(), window, cx).log_err()
        else {
            return;
        };

        diff.update(cx, |diff, cx| {
            diff.move_to_path(PathKey::for_buffer(buffer, cx), window, cx)
        })
    }

    // thread stuff

    pub fn restore_checkpoint(&mut self, client_id: &ClientUserMessageId, cx: &mut Context<Self>) {
        self.thread
            .update(cx, |thread, cx| {
                thread.restore_checkpoint(client_id.clone(), cx)
            })
            .detach_and_log_err(cx);
    }

    pub fn clear_thread_error(&mut self, cx: &mut Context<Self>) {
        self.thread_error = None;
        self.thread_error_markdown = None;
        self.token_limit_callout_dismissed = true;
        cx.notify();
    }

    fn is_following(&self, cx: &App) -> bool {
        match self.thread.read(cx).status() {
            ThreadStatus::Generating => self
                .workspace
                .read_with(cx, |workspace, _| {
                    workspace.is_being_followed(CollaboratorId::Agent)
                })
                .unwrap_or(false),
            _ => self.should_be_following,
        }
    }

    fn toggle_following(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let following = self.is_following(cx);

        self.should_be_following = !following;
        if self.thread.read(cx).status() == ThreadStatus::Generating {
            self.workspace
                .update(cx, |workspace, cx| {
                    if following {
                        workspace.unfollow(CollaboratorId::Agent, window, cx);
                    } else {
                        workspace.follow(CollaboratorId::Agent, window, cx);
                    }
                })
                .ok();
        }

        telemetry::event!("Follow Agent Selected", following = !following);
    }

    fn callout_border_position(&self) -> CalloutBorderPosition {
        if self.list_state.item_count() > 0 {
            CalloutBorderPosition::Top
        } else {
            CalloutBorderPosition::Bottom
        }
    }

    pub fn render_thread_retry_status_callout(&self, cx: &mut Context<Self>) -> Option<Callout> {
        let state = self.thread_retry_status.as_ref()?;

        if let Some(fallback_model) = acp_thread::refusal_fallback_model_from_meta(&state.meta) {
            return Some(
                Callout::new()
                    .icon(IconName::Warning)
                    .severity(Severity::Warning)
                    .title(state.last_error.clone())
                    .description(format!("Retrying with {fallback_model}"))
                    .dismiss_action(
                        IconButton::new("dismiss-refusal-fallback", IconName::Close)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Dismiss"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.thread_retry_status = None;
                                cx.notify();
                            })),
                    ),
            );
        }

        let next_attempt_in = state
            .duration
            .saturating_sub(Instant::now().saturating_duration_since(state.started_at));
        if next_attempt_in.is_zero() {
            return None;
        }

        let next_attempt_in_secs = next_attempt_in.as_secs() + 1;

        let retry_message = if state.max_attempts == 1 {
            if next_attempt_in_secs == 1 {
                "Retrying. Next attempt in 1 second.".to_string()
            } else {
                format!("Retrying. Next attempt in {next_attempt_in_secs} seconds.")
            }
        } else if next_attempt_in_secs == 1 {
            format!(
                "Retrying. Next attempt in 1 second (Attempt {} of {}).",
                state.attempt, state.max_attempts,
            )
        } else {
            format!(
                "Retrying. Next attempt in {next_attempt_in_secs} seconds (Attempt {} of {}).",
                state.attempt, state.max_attempts,
            )
        };

        Some(
            Callout::new()
                .border_position(self.callout_border_position())
                .icon(IconName::Warning)
                .severity(Severity::Warning)
                .title(state.last_error.clone())
                .description(retry_message),
        )
    }

    fn activity_bar_bg(&self, cx: &Context<Self>) -> Hsla {
        let editor_bg_color = cx.theme().colors().editor_background;
        let active_color = cx.theme().colors().element_selected;
        editor_bg_color.blend(active_color.opacity(0.3))
    }

    pub fn render_activity_bar(
        &self,
        window: &mut Window,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let thread = self.thread.read(cx);
        let action_log = thread.action_log();
        let telemetry = ActionLogTelemetry::from(thread);
        let changed_buffers = action_log.read(cx).changed_buffers(cx).collect::<Vec<_>>();
        let plan = thread.plan();
        let queue_is_empty = !self.has_queued_messages();

        let awaiting_permission = self
            .render_main_agent_awaiting_permission(window, cx)
            .or_else(|| self.render_subagents_awaiting_permission(cx));

        let subagents = self.visible_subagent_summaries(cx);

        if changed_buffers.is_empty()
            && plan.is_empty()
            && queue_is_empty
            && subagents.is_empty()
            && awaiting_permission.is_none()
        {
            return None;
        }

        // Temporarily always enable ACP edit controls. This is temporary, to lessen the
        // impact of a nasty bug that causes them to sometimes be disabled when they shouldn't
        // be, which blocks you from being able to accept or reject edits. This switches the
        // bug to be that sometimes it's enabled when it shouldn't be, which at least doesn't
        // block you from using the panel.
        let pending_edits = false;

        let plan_expanded = self.plan_expanded;
        let edits_expanded = self.edits_expanded;
        let queue_expanded = self.queue_expanded;

        // Collected rather than chained so the dividers fall between the
        // sections that actually rendered. Each section owns its own
        // summary-plus-expansion pair.
        let mut sections: Vec<AnyElement> = Vec::new();
        if let Some(awaiting_permission) = awaiting_permission {
            sections.push(awaiting_permission);
        }
        if !subagents.is_empty() {
            sections.push(self.render_subagents_section(&subagents, cx));
        }
        if !plan.is_empty() {
            sections.push(
                v_flex()
                    .child(self.render_plan_summary(plan, window, cx))
                    .when(plan_expanded, |parent| {
                        parent.child(self.render_plan_entries(plan, window, cx))
                    })
                    .into_any_element(),
            );
        }
        if !changed_buffers.is_empty() && thread.parent_session_id().is_none() {
            sections.push(
                v_flex()
                    .child(self.render_edits_summary(
                        &changed_buffers,
                        edits_expanded,
                        pending_edits,
                        cx,
                    ))
                    .when(edits_expanded, |parent| {
                        parent.child(self.render_edited_files(
                            action_log,
                            telemetry.clone(),
                            &changed_buffers,
                            pending_edits,
                            cx,
                        ))
                    })
                    .into_any_element(),
            );
        }
        if !queue_is_empty {
            sections.push(
                v_flex()
                    .child(self.render_message_queue_summary(window, cx))
                    .when(queue_expanded, |parent| {
                        parent.child(self.render_message_queue_entries(window, cx))
                    })
                    .into_any_element(),
            );
        }
        if sections.is_empty() {
            return None;
        }

        let last_section = sections.len() - 1;
        let sections = sections.into_iter().enumerate().map(|(index, section)| {
            v_flex()
                .child(section)
                .when(index < last_section, |this| {
                    this.child(Divider::horizontal().color(DividerColor::Border))
                })
                .into_any_element()
        });

        let max_content_width = AgentSettings::get_global(cx).max_content_width;
        // Drop shadows have no opaque surface to blend into on a transparent
        // window, so they render as a dark halo; only apply them when opaque.
        let opaque_window =
            cx.theme().window_background_appearance() == gpui::WindowBackgroundAppearance::Opaque;

        h_flex()
            .w_full()
            .px_2()
            .justify_center()
            .child(
                v_flex()
                    .when_some(max_content_width, |this, max_w| this.flex_basis(max_w))
                    .when(max_content_width.is_none(), |this| this.w_full())
                    .flex_shrink_1()
                    .flex_grow_0()
                    .max_w_full()
                    .bg(self.activity_bar_bg(cx))
                    .border_1()
                    .border_b_0()
                    .border_color(cx.theme().colors().border)
                    .rounded_t_md()
                    .when(opaque_window, |this| {
                        this.shadow(vec![
                            gpui::BoxShadow::new(px(1.), px(-1.), gpui::black().opacity(0.12))
                                .blur_radius(px(2.)),
                        ])
                    })
                    .children(sections),
            )
            .into_any()
            .into()
    }

    fn render_edited_files(
        &self,
        action_log: &Entity<ActionLog>,
        telemetry: ActionLogTelemetry,
        changed_buffers: &[(Entity<Buffer>, Entity<BufferDiff>)],
        pending_edits: bool,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let editor_bg_color = cx.theme().colors().editor_background;

        // Sort edited files alphabetically for consistency with Git diff view
        let mut sorted_buffers: Vec<_> = changed_buffers.iter().collect();
        sorted_buffers.sort_by(|(buffer_a, _), (buffer_b, _)| {
            let path_a = buffer_a.read(cx).file().map(|f| f.path().clone());
            let path_b = buffer_b.read(cx).file().map(|f| f.path().clone());
            path_a.cmp(&path_b)
        });

        v_flex()
            .id("edited_files_list")
            .max_h_40()
            .overflow_y_scroll()
            .child(
                v_flex().children(sorted_buffers.into_iter().enumerate().flat_map(
                    |(index, (buffer, diff))| {
                        let file = buffer.read(cx).file()?;
                        let path = file.path();
                        let path_style = file.path_style(cx);
                        let separator = file.path_style(cx).primary_separator();

                        let fallback_full_path =
                            full_path_for_empty_project_path(file.as_ref(), cx);

                        let file_path = path.parent().and_then(|parent| {
                            if parent.is_empty() {
                                None
                            } else {
                                Some(
                                    Label::new(format!(
                                        "{}{separator}",
                                        parent.display(path_style)
                                    ))
                                    .color(Color::Muted)
                                    .size(LabelSize::XSmall)
                                    .buffer_font(cx),
                                )
                            }
                        });

                        let file_name = path
                            .file_name()
                            .map(|name| {
                                Label::new(name.to_string())
                                    .size(LabelSize::XSmall)
                                    .buffer_font(cx)
                                    .ml_1()
                            })
                            .or_else(|| {
                                fallback_full_path.as_ref().map(|path| {
                                    Label::new(path.clone())
                                        .size(LabelSize::XSmall)
                                        .buffer_font(cx)
                                        .ml_1()
                                })
                            });

                        let full_path = fallback_full_path
                            .unwrap_or_else(|| path.display(path_style).to_string());

                        let file_icon = FileIcons::get_icon(path.as_std_path(), cx)
                            .map(Icon::from_path)
                            .map(|icon| icon.color(Color::Muted).size(IconSize::Small))
                            .unwrap_or_else(|| {
                                Icon::new(IconName::File)
                                    .color(Color::Muted)
                                    .size(IconSize::Small)
                            });

                        let file_stats = DiffStats::single_file(diff.read(cx));

                        let buttons = self.render_edited_files_buttons(
                            index,
                            buffer,
                            action_log,
                            &telemetry,
                            pending_edits,
                            editor_bg_color,
                            cx,
                        );

                        let element = h_flex()
                            .group("edited-code")
                            .id(("file-container", index))
                            .relative()
                            .min_w_0()
                            .p_1p5()
                            .gap_2()
                            .justify_between()
                            .bg(editor_bg_color)
                            .when(index < changed_buffers.len() - 1, |parent| {
                                parent.border_color(cx.theme().colors().border).border_b_1()
                            })
                            .child(
                                h_flex()
                                    .id(("file-name-path", index))
                                    .cursor_pointer()
                                    .pr_0p5()
                                    .gap_0p5()
                                    .rounded_xs()
                                    .child(file_icon)
                                    .children(file_name)
                                    .children(file_path)
                                    .child(
                                        DiffStat::new(
                                            "file",
                                            file_stats.lines_added as usize,
                                            file_stats.lines_removed as usize,
                                        )
                                        .label_size(LabelSize::XSmall),
                                    )
                                    .hover(|s| s.bg(cx.theme().colors().element_hover))
                                    .tooltip({
                                        move |_, cx| {
                                            Tooltip::with_meta(
                                                "Go to File",
                                                None,
                                                full_path.clone(),
                                                cx,
                                            )
                                        }
                                    })
                                    .on_click({
                                        let buffer = buffer.clone();
                                        cx.listener(move |this, _, window, cx| {
                                            this.open_edited_buffer(&buffer, window, cx);
                                        })
                                    }),
                            )
                            .child(buttons);

                        Some(element)
                    },
                )),
            )
            .into_any_element()
    }

    fn render_edited_files_buttons(
        &self,
        index: usize,
        buffer: &Entity<Buffer>,
        action_log: &Entity<ActionLog>,
        telemetry: &ActionLogTelemetry,
        pending_edits: bool,
        editor_bg_color: Hsla,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        h_flex()
            .id("edited-buttons-container")
            .visible_on_hover("edited-code")
            .absolute()
            .right_0()
            .px_1()
            .gap_1()
            .bg(editor_bg_color)
            .on_hover(cx.listener(move |this, is_hovered, _window, cx| {
                if *is_hovered {
                    this.hovered_edited_file_buttons = Some(index);
                } else if this.hovered_edited_file_buttons == Some(index) {
                    this.hovered_edited_file_buttons = None;
                }
                cx.notify();
            }))
            .child(
                Button::new("review", "Review")
                    .label_size(LabelSize::Small)
                    .on_click({
                        let buffer = buffer.clone();
                        cx.listener(move |this, _, window, cx| {
                            this.open_edited_buffer(&buffer, window, cx);
                        })
                    }),
            )
            .child(
                Button::new(("reject-file", index), "Reject")
                    .label_size(LabelSize::Small)
                    .disabled(pending_edits)
                    .on_click({
                        let buffer = buffer.clone();
                        let action_log = action_log.clone();
                        let telemetry = telemetry.clone();
                        move |_, _, cx| {
                            action_log.update(cx, |action_log, cx| {
                                action_log
                                    .reject_edits_in_ranges(
                                        buffer.clone(),
                                        vec![Anchor::min_max_range_for_buffer(
                                            buffer.read(cx).remote_id(),
                                        )],
                                        Some(telemetry.clone()),
                                        cx,
                                    )
                                    .0
                                    .detach_and_log_err(cx);
                            })
                        }
                    }),
            )
            .child(
                Button::new(("keep-file", index), "Keep")
                    .label_size(LabelSize::Small)
                    .disabled(pending_edits)
                    .on_click({
                        let buffer = buffer.clone();
                        let action_log = action_log.clone();
                        let telemetry = telemetry.clone();
                        move |_, _, cx| {
                            action_log.update(cx, |action_log, cx| {
                                action_log.keep_edits_in_range(
                                    buffer.clone(),
                                    Anchor::min_max_range_for_buffer(buffer.read(cx).remote_id()),
                                    Some(telemetry.clone()),
                                    cx,
                                );
                            })
                        }
                    }),
            )
    }

    fn collect_subagent_items_for_sessions(
        entries: &[AgentThreadEntry],
        awaiting_session_ids: &[acp::SessionId],
        cx: &App,
    ) -> Vec<(SharedString, usize)> {
        let tool_calls_by_session: HashMap<_, _> = entries
            .iter()
            .enumerate()
            .filter_map(|(entry_ix, entry)| {
                let AgentThreadEntry::ToolCall(tool_call) = entry else {
                    return None;
                };
                let info = tool_call.subagent_session_info.as_ref()?;
                let summary_text = tool_call.label.read(cx).source().to_string();
                let subagent_summary = if summary_text.is_empty() {
                    SharedString::from("Subagent")
                } else {
                    SharedString::from(summary_text)
                };
                Some((info.session_id.clone(), (subagent_summary, entry_ix)))
            })
            .collect();

        awaiting_session_ids
            .iter()
            .filter_map(|session_id| tool_calls_by_session.get(session_id).cloned())
            .collect()
    }

    /// The subagents this thread spawned, newest last. Empty for every agent
    /// that doesn't report subagent session info on its spawn tool calls.
    pub(crate) fn subagent_summaries(&self, cx: &App) -> Vec<SubagentSummary> {
        self.server_view
            .upgrade()
            .map(|server_view| {
                server_view
                    .read(cx)
                    .subagent_summaries_for_parent(&self.thread, cx)
            })
            .unwrap_or_default()
    }

    /// Where the current turn starts in this thread's transcript.
    fn current_turn_start(&self, cx: &App) -> usize {
        self.thread
            .read(cx)
            .entries()
            .iter()
            .rposition(|entry| matches!(entry, AgentThreadEntry::UserMessage(_)))
            .unwrap_or(0)
    }

    /// The subagents the tray shows: everything still working, plus finished
    /// ones from the current turn that haven't been cleared.
    ///
    /// A finished subagent from an earlier turn is history — its card is still
    /// in the transcript where it was spawned, and its thread is still open —
    /// so leaving it in the tray only crowds out the work actually in flight.
    /// Anything still running stays regardless of how old it is; losing sight
    /// of live work is the one thing the tray must not do.
    pub(crate) fn visible_subagent_summaries(&self, cx: &App) -> Vec<SubagentSummary> {
        let turn_start = self.current_turn_start(cx);
        self.subagent_summaries(cx)
            .into_iter()
            .filter(|summary| {
                summary.status.is_active()
                    || (summary.parent_entry_index >= turn_start
                        && !self.dismissed_subagents.contains(&summary.session_id))
            })
            .collect()
    }

    /// Drops `subagents` from the tray. Their cards stay in the transcript and
    /// their threads stay open; this only tidies the tray.
    pub(crate) fn clear_finished_subagents(
        &mut self,
        subagents: impl IntoIterator<Item = acp::SessionId>,
    ) {
        self.dismissed_subagents.extend(subagents);
    }

    /// Opens a subagent: navigates into its thread when it has been loaded,
    /// and otherwise scrolls this transcript to the tool call that spawned it,
    /// which is where its output lands.
    pub(crate) fn open_subagent(
        &mut self,
        summary: &SubagentSummary,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if summary.is_loaded {
            let session_id = summary.session_id.clone();
            self.server_view
                .update(cx, |server_view, cx| {
                    server_view.navigate_to_thread(session_id, window, cx);
                })
                .ok();
            return;
        }

        self.list_state.scroll_to(ListOffset {
            item_ix: summary.parent_entry_index,
            offset_in_item: px(0.0),
        });
        cx.notify();
    }

    fn render_subagents_section(
        &self,
        subagents: &[SubagentSummary],
        cx: &Context<Self>,
    ) -> AnyElement {
        let counts = SubagentCounts::from_summaries(subagents);
        let expanded = self.subagents_expanded;
        let finished: Vec<acp::SessionId> = subagents
            .iter()
            .filter(|summary| !summary.status.is_active())
            .map(|summary| summary.session_id.clone())
            .collect();
        let has_finished = !finished.is_empty();

        let summary_row = h_flex()
            .id("subagents_summary")
            .p_1()
            .w_full()
            .gap_1()
            .when(expanded, |this| {
                this.border_b_1().border_color(cx.theme().colors().border)
            })
            .child(Disclosure::new("subagents_disclosure", expanded))
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_1p5()
                    .child(
                        Icon::new(IconName::ListTree)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new("Subagents")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(counts.total.to_string())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .child(
                Label::new(counts.summary_label())
                    .size(LabelSize::Small)
                    .color(counts.summary_color()),
            )
            .when(has_finished, |this| {
                this.child(
                    IconButton::new("clear-subagents", IconName::Close)
                        .icon_size(IconSize::XSmall)
                        .shape(ui::IconButtonShape::Square)
                        .tooltip(Tooltip::text("Clear Finished Subagents"))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.clear_finished_subagents(finished.clone());
                            cx.stop_propagation();
                            cx.notify();
                        })),
                )
            })
            .on_click(cx.listener(|this, _, _, cx| {
                this.subagents_expanded = !this.subagents_expanded;
                cx.notify();
            }));

        let entries = expanded.then(|| {
            let entry_bg = cx.theme().colors().editor_background;
            let last_index = subagents.len().saturating_sub(1);

            v_flex()
                .id("subagent_list")
                .max_h_40()
                .overflow_y_scroll()
                .children(subagents.iter().enumerate().map(|(index, summary)| {
                    let group = SharedString::from(format!("subagent-row-{index}"));
                    let status = summary.status;
                    let summary = summary.clone();

                    h_flex()
                        .id(("subagent_row", index))
                        .group(&group)
                        .cursor_pointer()
                        .w_full()
                        .min_w_0()
                        .py_1()
                        .pr_2()
                        .gap_2()
                        .justify_between()
                        .bg(entry_bg)
                        // A colored rail rather than a tinted row: it reads as
                        // status at a glance without fighting the label for
                        // contrast, and stacks legibly when several run at once.
                        .border_l_2()
                        .border_color(status.color().color(cx))
                        .when(index < last_index, |this| {
                            this.border_b_1().border_color(cx.theme().colors().border)
                        })
                        .hover(|s| s.bg(cx.theme().colors().element_hover))
                        .child(
                            h_flex()
                                .min_w_0()
                                .gap_1p5()
                                .pl_1p5()
                                .child(if status == SubagentStatus::Running {
                                    Icon::new(status.icon())
                                        .size(IconSize::Small)
                                        .color(status.color())
                                        .with_rotate_animation(2)
                                        .into_any_element()
                                } else {
                                    Icon::new(status.icon())
                                        .size(IconSize::Small)
                                        .color(status.color())
                                        .into_any_element()
                                })
                                .child(
                                    Label::new(summary.label.clone())
                                        .size(LabelSize::Small)
                                        .truncate(),
                                ),
                        )
                        .child(
                            h_flex()
                                .flex_shrink_0()
                                .gap_1()
                                .child(
                                    Label::new(status.label())
                                        .size(LabelSize::XSmall)
                                        .color(status.color()),
                                )
                                .child(
                                    div().visible_on_hover(&group).child(
                                        Icon::new(IconName::ForwardArrowUp)
                                            .size(IconSize::XSmall)
                                            .color(Color::Muted),
                                    ),
                                ),
                        )
                        .tooltip({
                            let label = summary.label.clone();
                            let action = if summary.is_loaded {
                                "Open Subagent Thread"
                            } else {
                                "Go to Spawn Point"
                            };
                            move |_, cx| Tooltip::with_meta(label.clone(), None, action, cx)
                        })
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.open_subagent(&summary, window, cx);
                        }))
                }))
        });

        v_flex()
            .child(summary_row)
            .children(entries)
            .into_any_element()
    }

    fn render_subagents_awaiting_permission(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let awaiting = self.conversation.read(cx).subagents_awaiting_permission(cx);

        if awaiting.is_empty() {
            return None;
        }

        let awaiting_session_ids: Vec<_> = awaiting
            .iter()
            .map(|(session_id, _)| session_id.clone())
            .collect();

        let thread = self.thread.read(cx);
        let entries = thread.entries();
        let subagent_items =
            Self::collect_subagent_items_for_sessions(entries, &awaiting_session_ids, cx);

        if subagent_items.is_empty() {
            return None;
        }

        let item_count = subagent_items.len();

        Some(
            v_flex()
                .child(
                    h_flex()
                        .py_1()
                        .px_2()
                        .w_full()
                        .gap_1()
                        .border_b_1()
                        .border_color(cx.theme().colors().border)
                        .child(
                            Label::new("Subagents Awaiting Permission:")
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .child(Label::new(item_count.to_string()).size(LabelSize::Small)),
                )
                .child(
                    v_flex().children(subagent_items.into_iter().enumerate().map(
                        |(ix, (label, entry_ix))| {
                            let is_last = ix == item_count - 1;
                            let group = format!("group-{}", entry_ix);

                            h_flex()
                                .cursor_pointer()
                                .id(format!("subagent-permission-{}", entry_ix))
                                .group(&group)
                                .p_1()
                                .pl_2()
                                .min_w_0()
                                .w_full()
                                .gap_1()
                                .justify_between()
                                .bg(cx.theme().colors().editor_background)
                                .hover(|s| s.bg(cx.theme().colors().element_hover))
                                .when(!is_last, |this| {
                                    this.border_b_1().border_color(cx.theme().colors().border)
                                })
                                .child(
                                    h_flex()
                                        .gap_1p5()
                                        .child(
                                            Icon::new(IconName::Circle)
                                                .size(IconSize::XSmall)
                                                .color(Color::Warning),
                                        )
                                        .child(
                                            Label::new(label)
                                                .size(LabelSize::Small)
                                                .color(Color::Muted)
                                                .truncate(),
                                        ),
                                )
                                .child(
                                    div().visible_on_hover(&group).child(
                                        Label::new("Scroll to Subagent")
                                            .size(LabelSize::Small)
                                            .color(Color::Muted)
                                            .truncate(),
                                    ),
                                )
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.list_state.scroll_to(ListOffset {
                                        item_ix: entry_ix,
                                        offset_in_item: px(0.0),
                                    });
                                    cx.notify();
                                }))
                        },
                    )),
                )
                .into_any(),
        )
    }

    pub(crate) fn render_main_agent_awaiting_permission(
        &self,
        window: &Window,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        if self.is_subagent() {
            return None;
        }

        let active_session_id = self.thread.read(cx).session_id().clone();
        let conversation = self.conversation.read(cx);
        let tool_call_id = conversation.pending_tool_call_for_session(&active_session_id, cx)?;
        let pending_count = conversation.pending_tool_call_count_for_session(&active_session_id);

        let thread = self.thread.read(cx);
        let (entry_ix, tool_call) = thread.tool_call(&tool_call_id)?;

        let scroll_icon = if self.list_state.item_is_above_viewport(entry_ix)? {
            IconName::ArrowUp
        } else if self.list_state.item_is_below_viewport(entry_ix)? {
            IconName::ArrowDown
        } else {
            return None;
        };

        let focus_handle = self.focus_handle(cx);

        let card = self.render_any_tool_call(
            &active_session_id,
            entry_ix,
            tool_call,
            &focus_handle,
            ToolCallLayout::Floating,
            window,
            cx,
        );

        let label: SharedString = if pending_count > 1 {
            format!("Awaiting Confirmation ({pending_count})").into()
        } else {
            "Awaiting Confirmation".into()
        };

        let header = h_flex()
            .p_1p5()
            .pl_2()
            .w_full()
            .gap_1p5()
            .justify_between()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(
                h_flex()
                    .gap_1p5()
                    .child(
                        h_flex()
                            .w_2()
                            .justify_center()
                            .child(GeneratingSpinnerElement::new(SpinnerVariant::Sand)),
                    )
                    .child(Label::new(label).size(LabelSize::Small).color(Color::Muted)),
            )
            .child(
                Button::new("main-agent-permission-scroll-to", "Scroll")
                    .label_size(LabelSize::Small)
                    .end_icon(
                        Icon::new(scroll_icon)
                            .size(IconSize::XSmall)
                            .color(Color::Default),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.list_state.scroll_to(ListOffset {
                            item_ix: entry_ix,
                            offset_in_item: px(0.0),
                        });
                        cx.notify();
                    })),
            );

        Some(v_flex().child(header).child(card).into_any())
    }

    fn render_message_queue_summary(
        &self,
        _window: &mut Window,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let queue_count = self.message_queue.len();
        let title: SharedString = if queue_count == 1 {
            "1 Queued Message".into()
        } else {
            format!("{} Queued Messages", queue_count).into()
        };

        h_flex()
            .p_1()
            .w_full()
            .gap_1()
            .justify_between()
            .when(self.queue_expanded, |this| {
                this.border_b_1().border_color(cx.theme().colors().border)
            })
            .child(
                h_flex()
                    .id("queue_summary")
                    .gap_1()
                    .child(Disclosure::new("queue_disclosure", self.queue_expanded))
                    .child(Label::new(title).size(LabelSize::Small).color(Color::Muted))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.queue_expanded = !this.queue_expanded;
                        cx.notify();
                    })),
            )
            .child(
                Button::new("clear_queue", "Clear All")
                    .label_size(LabelSize::Small)
                    .key_binding(
                        KeyBinding::for_action(&ClearMessageQueue, cx)
                            .map(|kb| kb.size(rems_from_px(12_f32))),
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.clear_queue(cx);
                    })),
            )
            .into_any_element()
    }

    fn clear_queue(&mut self, cx: &mut Context<Self>) {
        self.message_queue.clear();
        self.sync_queue_flag_to_native_thread(cx);
        cx.notify();
    }

    fn render_plan_summary(
        &self,
        plan: &Plan,
        window: &mut Window,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let plan_expanded = self.plan_expanded;
        let stats = plan.stats();

        let title = if let Some(entry) = stats.in_progress_entry
            && !plan_expanded
        {
            h_flex()
                .cursor_default()
                .relative()
                .w_full()
                .gap_1()
                .truncate()
                .child(
                    Label::new("Current:")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().colors().text_muted)
                        .line_clamp(1)
                        .child(MarkdownElement::new(
                            entry.content.clone(),
                            plan_label_markdown_style(&entry.status, window, cx),
                        )),
                )
                .when(stats.pending > 0, |this| {
                    this.child(
                        h_flex()
                            .absolute()
                            .top_0()
                            .right_0()
                            .h_full()
                            .child(div().min_w_8().h_full().bg(linear_gradient(
                                90.,
                                linear_color_stop(self.activity_bar_bg(cx), 1.),
                                linear_color_stop(self.activity_bar_bg(cx).opacity(0.2), 0.),
                            )))
                            .child(
                                div().pr_0p5().bg(self.activity_bar_bg(cx)).child(
                                    Label::new(format!("{} left", stats.pending))
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                ),
                            ),
                    )
                })
        } else {
            let status_label = if stats.pending == 0 {
                "All Done".to_string()
            } else if stats.completed == 0 {
                format!("{} Tasks", plan.entries.len())
            } else {
                format!("{}/{}", stats.completed, plan.entries.len())
            };

            h_flex()
                .w_full()
                .gap_1()
                .justify_between()
                .child(
                    Label::new("Plan")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .child(
                    Label::new(status_label)
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .mr_1(),
                )
        };

        h_flex()
            .id("plan_summary")
            .p_1()
            .w_full()
            .gap_1()
            .when(plan_expanded, |this| {
                this.border_b_1().border_color(cx.theme().colors().border)
            })
            .child(Disclosure::new("plan_disclosure", plan_expanded))
            .child(title.flex_1())
            .child(
                IconButton::new("dismiss-plan", IconName::Close)
                    .icon_size(IconSize::XSmall)
                    .shape(ui::IconButtonShape::Square)
                    .tooltip(Tooltip::text("Clear Plan"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.thread.update(cx, |thread, cx| thread.clear_plan(cx));
                        cx.stop_propagation();
                    })),
            )
            .on_click(cx.listener(|this, _, _, cx| {
                this.plan_expanded = !this.plan_expanded;
                cx.notify();
            }))
            .into_any_element()
    }

    fn render_plan_entries(
        &self,
        plan: &Plan,
        window: &mut Window,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        v_flex()
            .id("plan_items_list")
            .max_h_40()
            .overflow_y_scroll()
            .child(
                v_flex().children(plan.entries.iter().enumerate().flat_map(|(index, entry)| {
                    let entry_bg = cx.theme().colors().editor_background;
                    let tooltip_text: SharedString =
                        entry.content.read(cx).source().to_string().into();

                    Some(
                        h_flex()
                            .id(("plan_entry_row", index))
                            .py_1()
                            .px_2()
                            .gap_2()
                            .justify_between()
                            .relative()
                            .bg(entry_bg)
                            .when(index < plan.entries.len() - 1, |parent| {
                                parent.border_color(cx.theme().colors().border).border_b_1()
                            })
                            .overflow_hidden()
                            .child(
                                h_flex()
                                    .id(("plan_entry", index))
                                    .gap_1p5()
                                    .min_w_0()
                                    .text_xs()
                                    .text_color(cx.theme().colors().text_muted)
                                    .child(match entry.status {
                                        acp::PlanEntryStatus::InProgress => {
                                            Icon::new(IconName::TodoProgress)
                                                .size(IconSize::Small)
                                                .color(Color::Accent)
                                                .with_rotate_animation(2)
                                                .into_any_element()
                                        }
                                        acp::PlanEntryStatus::Completed => {
                                            Icon::new(IconName::TodoComplete)
                                                .size(IconSize::Small)
                                                .color(Color::Success)
                                                .into_any_element()
                                        }
                                        acp::PlanEntryStatus::Pending | _ => {
                                            Icon::new(IconName::TodoPending)
                                                .size(IconSize::Small)
                                                .color(Color::Muted)
                                                .into_any_element()
                                        }
                                    })
                                    .child(MarkdownElement::new(
                                        entry.content.clone(),
                                        plan_label_markdown_style(&entry.status, window, cx),
                                    )),
                            )
                            .child(div().absolute().top_0().right_0().h_full().w_8().bg(
                                linear_gradient(
                                    90.,
                                    linear_color_stop(entry_bg, 1.),
                                    linear_color_stop(entry_bg.opacity(0.), 0.),
                                ),
                            ))
                            .tooltip(Tooltip::text(tooltip_text)),
                    )
                })),
            )
            .into_any_element()
    }

    fn render_completed_plan(
        &self,
        entries: &[PlanEntry],
        window: &Window,
        cx: &Context<Self>,
    ) -> AnyElement {
        v_flex()
            .px_5()
            .py_1p5()
            .w_full()
            .child(
                v_flex()
                    .w_full()
                    .rounded_md()
                    .border_1()
                    .border_color(self.tool_card_border_color(cx))
                    .child(
                        h_flex()
                            .px_2()
                            .py_1()
                            .gap_1()
                            .bg(self.tool_card_header_bg(cx))
                            .border_b_1()
                            .border_color(self.tool_card_border_color(cx))
                            .child(
                                Label::new("Completed Plan")
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(
                                Label::new(format!(
                                    "— {} {}",
                                    entries.len(),
                                    if entries.len() == 1 { "step" } else { "steps" }
                                ))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                            ),
                    )
                    .child(
                        v_flex().children(entries.iter().enumerate().map(|(index, entry)| {
                            h_flex()
                                .py_1()
                                .px_2()
                                .gap_1p5()
                                .when(index < entries.len() - 1, |this| {
                                    this.border_b_1().border_color(cx.theme().colors().border)
                                })
                                .child(
                                    Icon::new(IconName::TodoComplete)
                                        .size(IconSize::Small)
                                        .color(Color::Success),
                                )
                                .child(
                                    div()
                                        .max_w_full()
                                        .overflow_x_hidden()
                                        .text_xs()
                                        .text_color(cx.theme().colors().text_muted)
                                        .child(MarkdownElement::new(
                                            entry.content.clone(),
                                            default_markdown_style(window, cx),
                                        )),
                                )
                        })),
                    ),
            )
            .into_any()
    }

    fn render_context_compaction(
        &self,
        entry_ix: usize,
        compaction: &acp_thread::ContextCompaction,
        window: &Window,
        cx: &Context<Self>,
    ) -> AnyElement {
        let is_compacting = compaction.is_in_progress();
        let summary = compaction.summary.clone();
        let is_expanded = self
            .entry_view_state
            .read(cx)
            .is_compaction_expanded(entry_ix);

        let id = format!("context-compaction-{entry_ix}");
        let header_label = match compaction.status {
            acp_thread::ContextCompactionStatus::InProgress => "Compacting Context…",
            acp_thread::ContextCompactionStatus::Completed => "Context Compacted",
            acp_thread::ContextCompactionStatus::Canceled => "Compaction Canceled",
        };
        let chevron_end = if is_expanded {
            IconName::ChevronUp
        } else {
            IconName::ChevronDown
        };
        let header = h_flex()
            .gap_1()
            .w_full()
            .child(Divider::horizontal())
            .child(
                Button::new(id, header_label)
                    .label_size(LabelSize::Small)
                    .loading(is_compacting)
                    .disabled(is_compacting)
                    .start_icon(
                        Icon::new(IconName::Compact)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .when(!is_compacting, |this| {
                        this.end_icon(
                            Icon::new(chevron_end)
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                        )
                        .on_click(cx.listener(
                            move |this, _event: &ClickEvent, window, cx| {
                                this.toggle_compaction_expansion(entry_ix, window, cx);
                            },
                        ))
                    }),
            )
            .child(Divider::horizontal());

        div()
            .px_5()
            .w_full()
            .child(
                v_flex()
                    .pt_1p5()
                    .mb_1p5()
                    .gap_1p5()
                    .border_1()
                    .border_color(gpui::transparent_black())
                    .rounded_sm()
                    .child(header)
                    .when_some(summary.filter(|_| is_expanded), |this, summary| {
                        this.border_color(self.tool_card_border_color(cx))
                            .bg(cx.theme().colors().editor_background.opacity(0.2))
                            .child(
                                div()
                                    .id(("compaction-summary", entry_ix))
                                    .p_2()
                                    .text_ui(cx)
                                    .child(self.render_markdown(
                                        summary,
                                        MarkdownStyle::themed(MarkdownFont::Agent, window, cx),
                                        cx,
                                    )),
                            )
                            .child(
                                h_flex()
                                    .border_t_1()
                                    .border_color(self.tool_card_border_color(cx))
                                    .child(
                                        IconButton::new(
                                            ("compaction-summary-collapse", entry_ix),
                                            IconName::ChevronUp,
                                        )
                                        .full_width()
                                        .on_click(
                                            cx.listener(
                                                move |this, _event: &ClickEvent, window, cx| {
                                                    this.entry_view_state.update(
                                                        cx,
                                                        |state, _cx| {
                                                            state.collapse_compaction(entry_ix);
                                                        },
                                                    );
                                                    this.refresh_thread_search(window, cx);
                                                    cx.notify();
                                                },
                                            ),
                                        ),
                                    ),
                            )
                    }),
            )
            .into_any()
    }

    fn toggle_compaction_expansion(
        &mut self,
        entry_ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // If tail following is active and the entry is not yet expanded, we'll
        // want to anchor the list's scroll position, to prevent it from
        // automatically scrolling to the end of the compaction context element,
        // which would feel off, as we assume the user is trying to read it from
        // top to bottom.
        if self.list_state.is_following_tail()
            && !self
                .entry_view_state
                .read(cx)
                .is_compaction_expanded(entry_ix)
        {
            self.list_state.pause_following_tail();
        }

        self.entry_view_state.update(cx, |state, _cx| {
            state.toggle_compaction_expansion(entry_ix);
        });
        self.list_state.remeasure_items(entry_ix..entry_ix + 1);
        self.refresh_thread_search(window, cx);
        cx.notify();
    }

    fn render_edits_summary(
        &self,
        changed_buffers: &[(Entity<Buffer>, Entity<BufferDiff>)],
        expanded: bool,
        pending_edits: bool,
        cx: &Context<Self>,
    ) -> Div {
        const EDIT_NOT_READY_TOOLTIP_LABEL: &str = "Wait until file edits are complete.";

        let focus_handle = self.focus_handle(cx);

        h_flex()
            .p_1()
            .justify_between()
            .flex_wrap()
            .when(expanded, |this| {
                this.border_b_1().border_color(cx.theme().colors().border)
            })
            .child(
                h_flex()
                    .id("edits-container")
                    .cursor_pointer()
                    .gap_1()
                    .child(Disclosure::new("edits-disclosure", expanded))
                    .map(|this| {
                        if pending_edits {
                            this.child(
                                Label::new(format!(
                                    "Editing {} {}…",
                                    changed_buffers.len(),
                                    if changed_buffers.len() == 1 {
                                        "file"
                                    } else {
                                        "files"
                                    }
                                ))
                                .color(Color::Muted)
                                .size(LabelSize::Small)
                                .with_animation(
                                    "edit-label",
                                    Animation::new(Duration::from_secs(2))
                                        .repeat()
                                        .with_easing(pulsating_between(0.3, 0.7)),
                                    |label, delta| label.alpha(delta),
                                ),
                            )
                        } else {
                            let stats = DiffStats::all_files(changed_buffers.iter().cloned(), cx);
                            let dot_divider = || {
                                Label::new("•")
                                    .size(LabelSize::XSmall)
                                    .color(Color::Disabled)
                            };

                            this.child(
                                Label::new("Edits")
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(dot_divider())
                            .child(
                                Label::new(format!(
                                    "{} {}",
                                    changed_buffers.len(),
                                    if changed_buffers.len() == 1 {
                                        "file"
                                    } else {
                                        "files"
                                    }
                                ))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                            )
                            .child(dot_divider())
                            .child(DiffStat::new(
                                "total",
                                stats.lines_added as usize,
                                stats.lines_removed as usize,
                            ))
                        }
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.edits_expanded = !this.edits_expanded;
                        cx.notify();
                    })),
            )
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        IconButton::new("review-changes", IconName::ListTodo)
                            .icon_size(IconSize::Small)
                            .tooltip({
                                let focus_handle = focus_handle.clone();
                                move |_window, cx| {
                                    Tooltip::for_action_in(
                                        "Review Changes",
                                        &OpenAgentDiff,
                                        &focus_handle,
                                        cx,
                                    )
                                }
                            })
                            .on_click(cx.listener(|_, _, window, cx| {
                                window.dispatch_action(OpenAgentDiff.boxed_clone(), cx);
                            })),
                    )
                    .child(Divider::vertical().color(DividerColor::Border))
                    .child(
                        Button::new("reject-all-changes", "Reject All")
                            .label_size(LabelSize::Small)
                            .disabled(pending_edits)
                            .when(pending_edits, |this| {
                                this.tooltip(Tooltip::text(EDIT_NOT_READY_TOOLTIP_LABEL))
                            })
                            .key_binding(
                                KeyBinding::for_action_in(&RejectAll, &focus_handle.clone(), cx)
                                    .map(|kb| kb.size(rems_from_px(12_f32))),
                            )
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.reject_all(&RejectAll, window, cx);
                            })),
                    )
                    .child(
                        Button::new("keep-all-changes", "Keep All")
                            .label_size(LabelSize::Small)
                            .disabled(pending_edits)
                            .when(pending_edits, |this| {
                                this.tooltip(Tooltip::text(EDIT_NOT_READY_TOOLTIP_LABEL))
                            })
                            .key_binding(
                                KeyBinding::for_action_in(&KeepAll, &focus_handle, cx)
                                    .map(|kb| kb.size(rems_from_px(12_f32))),
                            )
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.keep_all(&KeepAll, window, cx);
                            })),
                    ),
            )
    }

    /// Applies `f` to the tool call that spawned this subagent, in the parent's
    /// transcript.
    ///
    /// That tool call is the only authoritative signal for what a subagent is
    /// doing. The subagent's own [`AcpThread::status`] is useless here: a
    /// derived subagent never owns a turn, so it reads `Idle` from the moment
    /// it is created and would render as finished before it had started.
    fn with_subagent_tool_call<R>(
        &self,
        cx: &App,
        f: impl FnOnce(&acp_thread::ToolCall) -> R,
    ) -> Option<R> {
        let parent_session_id = self.parent_session_id.as_ref()?;
        let my_session_id = self.thread.read(cx).session_id().clone();

        let parent_view = self
            .server_view
            .upgrade()?
            .read(cx)
            .thread_view(parent_session_id)?;
        let parent_view = parent_view.read(cx);
        let tool_call = parent_view
            .thread
            .read(cx)
            .tool_call_for_subagent(&my_session_id)?;
        Some(f(tool_call))
    }

    /// The name the parent addresses this subagent by, when it is a member of a
    /// Claude Code agent team.
    ///
    /// Present only when the parent named it on spawn. Without a name there is
    /// nothing for `SendMessage` to address, so the composer stays disabled
    /// rather than sending something that cannot be delivered.
    fn agent_team_name(&self, cx: &App) -> Option<SharedString> {
        self.with_subagent_tool_call(cx, acp_thread::AcpThread::agent_team_name)
            .flatten()
    }

    /// The parent's thread, which is what actually talks to the agent: a
    /// derived subagent has no session on the agent side, so a message for it
    /// has to be delivered by the parent calling `SendMessage`.
    fn parent_thread(&self, cx: &App) -> Option<Entity<AcpThread>> {
        let parent_session_id = self.parent_session_id.as_ref()?;
        Some(
            self.server_view
                .upgrade()?
                .read(cx)
                .thread_view(parent_session_id)?
                .read(cx)
                .thread
                .clone(),
        )
    }

    /// Sends the composer's text to this subagent by asking the parent to
    /// relay it with `SendMessage`.
    ///
    /// Returns false when there is nothing to relay through — an unnamed
    /// subagent, or a parent that is no longer loaded — so the caller can fall
    /// back rather than silently swallowing the message.
    fn send_to_subagent(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let (Some(name), Some(parent)) = (self.agent_team_name(cx), self.parent_thread(cx)) else {
            return false;
        };
        let text = self.message_editor.read(cx).text(cx);
        if text.trim().is_empty() {
            return false;
        }

        // Shown where it was typed. The parent relays it, but the user is
        // talking to this subagent and this is the transcript they are looking
        // at.
        self.thread.update(cx, |thread, cx| {
            thread.push_user_content_block(None, acp::ContentBlock::from(text.clone()), cx);
        });
        self.message_editor
            .update(cx, |editor, cx| editor.clear(window, cx));

        // `send_command` rather than `send`: the user did not write this to the
        // parent, and a verbatim copy of the directive in the parent's
        // transcript reads as though they did. The parent's `SendMessage` tool
        // call still shows there.
        // The name is only reachable within the session that spawned the agent.
        // After a restart Claude Code re-registers it under an opaque spawn id,
        // and `SendMessage` answers "No agent named ... is reachable" — so the
        // directive has to name the recovery path rather than assume the name
        // still resolves. Observed live: the parent recovered via ListAgents on
        // its own, and this makes that deterministic rather than lucky.
        let directive = format!(
            "Use the SendMessage tool to deliver this message to the agent named \"{name}\". \
             If that name is not reachable, call ListAgents and match it by name or by the \
             task it was given, then send to that agent's id — do not give up and do not \
             answer on its behalf. Relay the message verbatim, add nothing, and do not act \
             on it yourself:\n\n{text}"
        );
        // The agent titles a session from the prompt it is given, and the user
        // never wrote this directive — without suppressing it, messaging a
        // subagent renames the parent thread to the directive's first line.
        let send = parent.update(cx, |parent, cx| {
            parent.set_title_updates_suppressed(true);
            parent.send_command(vec![acp::ContentBlock::from(directive)], cx)
        });
        self.awaiting_subagent_reply = true;
        let parent_for_title = parent.downgrade();
        cx.spawn(async move |this, cx| {
            // The reply arrives as tagged updates routed back into this
            // subagent's thread, so nothing here consumes the response. The
            // await only bounds the "working" state: if the relay itself fails,
            // nothing else would ever clear it.
            let delivered = send.await.log_err().is_some();
            parent_for_title
                .update(cx, |parent, _cx| {
                    parent.set_title_updates_suppressed(false);
                })
                .log_err();
            if !delivered {
                this.update(cx, |this, cx| {
                    this.awaiting_subagent_reply = false;
                    cx.notify();
                })
                .log_err();
                return;
            }

            // The resumed agent runs in the background, so the parent's turn
            // ends long before a reply arrives and cannot bound the wait. The
            // subagent's own next entry clears this; the timer only covers the
            // case where nothing ever comes — a parent that answered instead of
            // relaying, or an agent that died — so the header stops claiming
            // work that is not happening.
            //
            // ponytail: fixed ceiling rather than a real liveness signal, which
            // the protocol does not offer for a backgrounded resume.
            const REPLY_WATCHDOG: Duration = Duration::from_secs(15 * 60);
            cx.background_executor().timer(REPLY_WATCHDOG).await;
            this.update(cx, |this, cx| {
                if this.awaiting_subagent_reply {
                    log::warn!("subagent relay: no reply arrived within the watchdog window");
                    this.awaiting_subagent_reply = false;
                    cx.notify();
                }
            })
            .log_err();
        })
        .detach();
        cx.notify();
        true
    }

    /// Clears the awaiting-reply state once the subagent produces anything.
    ///
    /// Its own entries are the only reliable end of the wait: the parent's
    /// relay call returns as soon as the resume is accepted, long before the
    /// agent has said anything.
    pub(crate) fn subagent_reply_arrived(&mut self, cx: &mut Context<Self>) {
        if self.awaiting_subagent_reply {
            self.awaiting_subagent_reply = false;
            cx.notify();
        }
    }

    /// What the conversation has observed about this subagent beyond its
    /// spawning tool call, which for a background subagent is the only thing
    /// that says whether it is still working. See [`SubagentActivity`].
    fn subagent_activity(&self, cx: &App) -> SubagentActivity {
        self.subagent_activity_for(&self.session_id, cx)
    }

    /// The same, for a subagent this view is *rendering* rather than being.
    fn subagent_activity_for(&self, session_id: &acp::SessionId, cx: &App) -> SubagentActivity {
        self.server_view
            .upgrade()
            .map(|server_view| server_view.read(cx).subagent_activity(session_id, cx))
            .unwrap_or_default()
    }

    fn is_subagent_canceled_or_failed(&self, cx: &App) -> bool {
        if self.subagent_activity(cx) == SubagentActivity::Canceled {
            return true;
        }
        self.with_subagent_tool_call(cx, |tool_call| {
            matches!(
                tool_call.status,
                ToolCallStatus::Canceled | ToolCallStatus::Failed | ToolCallStatus::Rejected
            )
        })
        .unwrap_or(false)
    }

    fn is_subagent_running(&self, cx: &App) -> bool {
        // A relayed message resumes an agent whose spawning tool call completed
        // long ago, so that call's status says "done" for the entire time the
        // agent is working on the reply. Awaiting a relay is the only signal
        // that it is busy again.
        if self.awaiting_subagent_reply {
            return true;
        }
        if self.subagent_activity(cx) == SubagentActivity::Live {
            return true;
        }
        self.with_subagent_tool_call(cx, |tool_call| {
            matches!(
                tool_call.status,
                ToolCallStatus::Pending
                    | ToolCallStatus::InProgress
                    | ToolCallStatus::WaitingForConfirmation { .. }
            )
        })
        .unwrap_or(false)
    }

    pub(crate) fn render_subagent_titlebar(&mut self, cx: &mut Context<Self>) -> Option<Div> {
        if self.parent_session_id.is_none() {
            return None;
        }
        let parent_session_id = self.thread.read(cx).parent_session_id()?.clone();

        let server_view = self.server_view.clone();
        let thread = self.thread.clone();
        let is_done = !self.is_subagent_running(cx);
        let is_canceled_or_failed = self.is_subagent_canceled_or_failed(cx);

        let max_content_width = AgentSettings::get_global(cx).max_content_width;

        Some(
            h_flex()
                .w_full()
                .h(Tab::container_height(cx))
                .border_b_1()
                .when(is_done && is_canceled_or_failed, |this| {
                    this.border_dashed()
                })
                .border_color(cx.theme().colors().border)
                .bg(cx.theme().colors().editor_background.opacity(0.2))
                .child(
                    h_flex()
                        .size_full()
                        .when_some(max_content_width, |this, max_w| this.max_w(max_w).mx_auto())
                        .pl_2()
                        .pr_1()
                        .flex_shrink_0()
                        .justify_between()
                        .gap_1()
                        .child(
                            h_flex()
                                .flex_1()
                                .gap_2()
                                .child(
                                    Icon::new(IconName::ForwardArrowUp)
                                        .size(IconSize::Small)
                                        .color(Color::Muted),
                                )
                                .child(self.title_editor.clone())
                                .when(is_done && is_canceled_or_failed, |this| {
                                    this.child(Icon::new(IconName::Close).color(Color::Error))
                                })
                                .when(is_done && !is_canceled_or_failed, |this| {
                                    this.child(Icon::new(IconName::Check).color(Color::Success))
                                }),
                        )
                        .child(
                            h_flex()
                                .gap_0p5()
                                .when(!is_done, |this| {
                                    this.child(
                                        IconButton::new("stop_subagent", IconName::Stop)
                                            .icon_size(IconSize::Small)
                                            .icon_color(Color::Error)
                                            .tooltip(Tooltip::text("Stop Subagent"))
                                            .on_click(move |_, _, cx| {
                                                thread.update(cx, |thread, cx| {
                                                    thread.cancel(cx).detach();
                                                });
                                            }),
                                    )
                                })
                                .child(
                                    IconButton::new("minimize_subagent", IconName::Dash)
                                        .icon_size(IconSize::Small)
                                        .tooltip(Tooltip::text("Minimize Subagent"))
                                        .on_click(move |_, window, cx| {
                                            let _ = server_view.update(cx, |server_view, cx| {
                                                server_view.navigate_to_thread(
                                                    parent_session_id.clone(),
                                                    window,
                                                    cx,
                                                );
                                            });
                                        }),
                                ),
                        ),
                ),
        )
    }

    pub(crate) fn render_message_editor(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // A subagent gets a composer only when the parent named it: the message
        // is delivered by the parent calling `SendMessage`, which addresses a
        // team member by name, so an unnamed subagent has nothing to send to
        // and its composer would swallow whatever was typed.
        if self.is_subagent() && self.agent_team_name(cx).is_none() {
            return div().into_any_element();
        }

        let focus_handle = self.message_editor.focus_handle(cx);
        let editor_bg_color = cx.theme().colors().editor_background;

        let editor_expanded = self.editor_expanded;
        let (expand_icon, expand_tooltip) = if editor_expanded {
            (IconName::Minimize, "Minimize Message Editor")
        } else {
            (IconName::Maximize, "Expand Message Editor")
        };

        let max_content_width = AgentSettings::get_global(cx).max_content_width;
        let has_messages = self.list_state.item_count() > 0;
        let fills_container = !has_messages || editor_expanded;

        h_flex()
            .py_2()
            .bg(editor_bg_color)
            .justify_center()
            .on_action(cx.listener(Self::handle_message_editor_move_up))
            .map(|this| {
                if has_messages {
                    this.on_action(cx.listener(Self::expand_message_editor))
                        .border_t_1()
                        .border_color(cx.theme().colors().border)
                        .when(editor_expanded, |this| this.h(vh(0.8, window)))
                } else {
                    this.flex_1().size_full()
                }
            })
            .child(
                v_flex()
                    .when_some(max_content_width, |this, max_w| this.flex_basis(max_w))
                    .when(max_content_width.is_none(), |this| this.w_full())
                    .min_w_0()
                    .when(fills_container, |this| this.h_full())
                    .px_2()
                    .flex_shrink_1()
                    .flex_grow_0()
                    .justify_between()
                    .gap_2()
                    .child(
                        v_flex()
                            .relative()
                            .w_full()
                            .min_h_0()
                            .when(fills_container, |this| this.flex_1())
                            .pt_1()
                            .pr_2p5()
                            .child(self.message_editor.clone())
                            .when(has_messages, |this| {
                                this.child(
                                    h_flex()
                                        .absolute()
                                        .top_0()
                                        .right_0()
                                        .opacity(0.5)
                                        .hover(|s| s.opacity(1.0))
                                        .child(
                                            IconButton::new("toggle-height", expand_icon)
                                                .icon_size(IconSize::Small)
                                                .icon_color(Color::Muted)
                                                .tooltip({
                                                    move |_window, cx| {
                                                        Tooltip::for_action_in(
                                                            expand_tooltip,
                                                            &ExpandMessageEditor,
                                                            &focus_handle,
                                                            cx,
                                                        )
                                                    }
                                                })
                                                .on_click(cx.listener(|this, _, window, cx| {
                                                    this.expand_message_editor(
                                                        &ExpandMessageEditor,
                                                        window,
                                                        cx,
                                                    );
                                                })),
                                        ),
                                )
                            }),
                    )
                    .child(
                        h_flex()
                            .w_full()
                            .min_w_0()
                            .flex_none()
                            .flex_wrap()
                            .justify_between()
                            .child(
                                h_flex()
                                    .min_w_0()
                                    .flex_wrap()
                                    .gap_0p5()
                                    .child(self.render_add_context_button(cx))
                                    .child(self.render_follow_toggle(cx))
                                    .children(self.render_fast_mode_control(cx))
                                    .children(self.render_thinking_control(cx)),
                            )
                            .child(
                                h_flex()
                                    .min_w_0()
                                    .flex_wrap()
                                    .gap_1()
                                    .children(self.render_token_usage(cx))
                                    .children(self.profile_selector.clone())
                                    .map(|this| match self.config_options_view.clone() {
                                        Some(config_view) => this.child(config_view),
                                        None => this
                                            .children(self.mode_selector.clone())
                                            .children(self.model_selector.clone()),
                                    })
                                    .child(self.render_send_button(cx)),
                            ),
                    ),
            )
            .into_any()
    }

    fn render_queue_steer_button(
        &self,
        entry_id: QueueEntryId,
        index: usize,
        is_next: bool,
        steer_on: bool,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let focus_handle = self.message_editor.focus_handle(cx);

        Button::new(("steer", index), "Steer")
            .label_size(LabelSize::Small)
            .toggle_state(steer_on)
            .selected_style(ButtonStyle::Tinted(TintColor::Accent))
            .when(is_next, |this| {
                this.key_binding(
                    KeyBinding::for_action_in(&ToggleSteerFirstQueuedMessage, &focus_handle, cx)
                        .map(|kb| kb.size(rems_from_px(12_f32))),
                )
            })
            .tooltip(move |_window, cx| {
                Tooltip::with_meta(
                    "Steer",
                    None,
                    "Interrupt the agent at its next step to send this message. \
                     When off, queued messages wait for the agent to finish.",
                    cx,
                )
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                this.toggle_queue_entry_steer(entry_id, cx);
            }))
    }

    fn render_message_queue_entries(
        &self,
        _window: &mut Window,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let message_editor = self.message_editor.read(cx);
        let focus_handle = message_editor.focus_handle(cx);

        let queue_len = self.message_queue.len();
        let can_fast_track = self.message_queue.can_fast_track();
        let is_native = self.as_native_thread(cx).is_some();

        v_flex()
            .id("message_queue_list")
            .max_h_40()
            .overflow_y_scroll()
            .children(self.message_queue.iter().enumerate().map(|(index, entry)| {
                let entry_id = entry.id;
                let editor = &entry.editor;
                let is_next = index == 0;
                let (icon_color, tooltip_text) = if is_next {
                    (Color::Accent, "Next in Queue")
                } else {
                    (Color::Muted, "In Queue")
                };

                let editor_focused = editor.focus_handle(cx).is_focused(_window);
                let keybinding_size = rems_from_px(12_f32);
                let steer_on = entry.steer;

                let min_width = rems_from_px(160_f32);

                h_flex()
                    .group("queue_entry")
                    .w_full()
                    .p_1p5()
                    .gap_1()
                    .bg(cx.theme().colors().editor_background)
                    .when(index < queue_len - 1, |this| {
                        this.border_b_1()
                            .border_color(cx.theme().colors().border_variant)
                    })
                    .child(
                        div()
                            .id("next_in_queue")
                            .child(
                                Icon::new(IconName::Circle)
                                    .size(IconSize::Small)
                                    .color(icon_color),
                            )
                            .tooltip(Tooltip::text(tooltip_text)),
                    )
                    .child(editor.clone())
                    .child(if editor_focused {
                        h_flex()
                            .gap_1()
                            .min_w(min_width)
                            .justify_end()
                            .child(
                                IconButton::new(("edit", index), IconName::Pencil)
                                    .icon_size(IconSize::Small)
                                    .tooltip(|_window, cx| {
                                        Tooltip::with_meta(
                                            "Edit Queued Message",
                                            None,
                                            "Type anything to edit",
                                            cx,
                                        )
                                    })
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.move_queued_message_to_main_editor(
                                            entry_id, None, None, window, cx,
                                        );
                                    })),
                            )
                            .when(is_native, |row| {
                                row.child(self.render_queue_steer_button(
                                    entry_id, index, is_next, steer_on, cx,
                                ))
                            })
                            .child(
                                Button::new(("send_now_focused", index), "Send Now")
                                    .label_size(LabelSize::Small)
                                    .style(ButtonStyle::Outlined)
                                    .key_binding(
                                        KeyBinding::for_action_in(
                                            &SendImmediately,
                                            &editor.focus_handle(cx),
                                            cx,
                                        )
                                        .map(|kb| kb.size(keybinding_size)),
                                    )
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.send_queued_message_now(entry_id, window, cx);
                                    })),
                            )
                    } else {
                        h_flex()
                            .when(!is_next, |this| this.visible_on_hover("queue_entry"))
                            .gap_1()
                            .min_w(min_width)
                            .justify_end()
                            .child(
                                IconButton::new(("delete", index), IconName::Trash)
                                    .icon_size(IconSize::Small)
                                    .tooltip({
                                        let focus_handle = focus_handle.clone();
                                        move |_window, cx| {
                                            if is_next {
                                                Tooltip::for_action_in(
                                                    "Remove Message from Queue",
                                                    &RemoveFirstQueuedMessage,
                                                    &focus_handle,
                                                    cx,
                                                )
                                            } else {
                                                Tooltip::simple("Remove Message from Queue", cx)
                                            }
                                        }
                                    })
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.remove_from_queue(entry_id, cx);
                                        cx.notify();
                                    })),
                            )
                            .child(
                                IconButton::new(("edit", index), IconName::Pencil)
                                    .icon_size(IconSize::Small)
                                    .tooltip({
                                        let focus_handle = focus_handle.clone();
                                        move |_window, cx| {
                                            if is_next {
                                                Tooltip::for_action_in(
                                                    "Edit",
                                                    &EditFirstQueuedMessage,
                                                    &focus_handle,
                                                    cx,
                                                )
                                            } else {
                                                Tooltip::simple("Edit", cx)
                                            }
                                        }
                                    })
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.move_queued_message_to_main_editor(
                                            entry_id, None, None, window, cx,
                                        );
                                    })),
                            )
                            .when(is_native, |row| {
                                row.child(self.render_queue_steer_button(
                                    entry_id, index, is_next, steer_on, cx,
                                ))
                            })
                            .child(
                                Button::new(("send_now", index), "Send Now")
                                    .label_size(LabelSize::Small)
                                    .when(is_next, |this| this.style(ButtonStyle::Outlined))
                                    .when(is_next && message_editor.is_empty(cx), |this| {
                                        let action: Box<dyn gpui::Action> = if can_fast_track {
                                            Box::new(Chat)
                                        } else {
                                            Box::new(SendNextQueuedMessage)
                                        };

                                        this.key_binding(
                                            KeyBinding::for_action_in(
                                                action.as_ref(),
                                                &focus_handle.clone(),
                                                cx,
                                            )
                                            .map(|kb| kb.size(keybinding_size)),
                                        )
                                    })
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.send_queued_message_now(entry_id, window, cx);
                                    })),
                            )
                    })
            }))
            .into_any_element()
    }

    fn supports_split_token_display(&self, cx: &App) -> bool {
        self.as_native_thread(cx)
            .and_then(|thread| thread.read(cx).model())
            .is_some_and(|model| model.supports_split_token_display())
    }

    fn render_token_usage(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let thread = self.thread.read(cx);
        let usage = thread.token_usage()?;
        let show_split = self.supports_split_token_display(cx);

        let cost_label = thread.cost().map(|cost| {
            let precision = if cost.amount > 0.0 && cost.amount < 0.01 {
                4
            } else {
                2
            };
            format!("{:.prec$} {}", cost.amount, cost.currency, prec = precision)
        });

        let progress_color = |ratio: f32| -> Hsla {
            if ratio >= 0.85 {
                cx.theme().status().warning
            } else {
                cx.theme().colors().text_muted
            }
        };

        let used = crate::humanize_token_count(usage.used_tokens);
        let max = crate::humanize_token_count(usage.max_tokens);
        let input_tokens_label = crate::humanize_token_count(usage.input_tokens);
        let output_tokens_label = crate::humanize_token_count(usage.output_tokens);

        let progress_ratio = if usage.max_tokens > 0 {
            usage.used_tokens as f32 / usage.max_tokens as f32
        } else {
            0.0
        };

        let ring_size = px(16.0);
        let stroke_width = px(2.);

        let percentage = format!("{}%", (progress_ratio * 100.0).round() as u32);

        let tooltip_separator_color = Color::Custom(cx.theme().colors().text_disabled.opacity(0.6));

        let (project_rules_count, project_entry_ids) = self
            .as_native_thread(cx)
            .map(|thread| {
                let project_context = thread.read(cx).project_context().read(cx);
                let project_entry_ids = project_context
                    .worktrees
                    .iter()
                    .filter_map(|wt| wt.rules_file.as_ref())
                    .map(|rf| ProjectEntryId::from_usize(rf.project_entry_id))
                    .collect::<Vec<_>>();
                let project_rules_count = project_entry_ids.len();
                (project_rules_count, project_entry_ids)
            })
            .unwrap_or_default();

        let global_agents_md_loaded = UserAgentsMd::global(cx)
            .and_then(|md| md.content())
            .is_some();

        let workspace = self.workspace.clone();

        let max_output_tokens = self
            .as_native_thread(cx)
            .and_then(|thread| thread.read(cx).model())
            .and_then(|model| model.max_output_tokens())
            .unwrap_or(0);
        let input_max_label =
            crate::humanize_token_count(usage.max_tokens.saturating_sub(max_output_tokens));
        let output_max_label = crate::humanize_token_count(max_output_tokens);

        let build_tooltip = {
            move |_window: &mut Window, cx: &mut App| {
                let percentage = percentage.clone();
                let used = used.clone();
                let max = max.clone();
                let input_tokens_label = input_tokens_label.clone();
                let output_tokens_label = output_tokens_label.clone();
                let input_max_label = input_max_label.clone();
                let output_max_label = output_max_label.clone();
                let project_entry_ids = project_entry_ids.clone();
                let workspace = workspace.clone();
                let cost_label = cost_label.clone();
                cx.new(move |_cx| TokenUsageTooltip {
                    percentage,
                    used,
                    max,
                    input_tokens: input_tokens_label,
                    output_tokens: output_tokens_label,
                    input_max: input_max_label,
                    output_max: output_max_label,
                    show_split,
                    cost_label,
                    separator_color: tooltip_separator_color,
                    global_agents_md_loaded,
                    project_rules_count,
                    project_entry_ids,
                    workspace,
                })
                .into()
            }
        };

        if show_split {
            let input_max_raw = usage.max_tokens.saturating_sub(max_output_tokens);
            let output_max_raw = max_output_tokens;

            let input_ratio = if input_max_raw > 0 {
                usage.input_tokens as f32 / input_max_raw as f32
            } else {
                0.0
            };
            let output_ratio = if output_max_raw > 0 {
                usage.output_tokens as f32 / output_max_raw as f32
            } else {
                0.0
            };

            Some(
                h_flex()
                    .id("split_token_usage")
                    .flex_shrink_0()
                    .gap_1p5()
                    .mr_1()
                    .child(
                        h_flex()
                            .gap_0p5()
                            .child(
                                Icon::new(IconName::ArrowUp)
                                    .size(IconSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .child(
                                CircularProgress::new(
                                    usage.input_tokens as f32,
                                    input_max_raw as f32,
                                    ring_size,
                                    cx,
                                )
                                .stroke_width(stroke_width)
                                .progress_color(progress_color(input_ratio)),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_0p5()
                            .child(
                                Icon::new(IconName::ArrowDown)
                                    .size(IconSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .child(
                                CircularProgress::new(
                                    usage.output_tokens as f32,
                                    output_max_raw as f32,
                                    ring_size,
                                    cx,
                                )
                                .stroke_width(stroke_width)
                                .progress_color(progress_color(output_ratio)),
                            ),
                    )
                    .hoverable_tooltip(build_tooltip)
                    .into_any_element(),
            )
        } else {
            Some(
                h_flex()
                    .id("circular_progress_tokens")
                    .mt_px()
                    .mr_1()
                    .child(
                        CircularProgress::new(
                            usage.used_tokens as f32,
                            usage.max_tokens as f32,
                            ring_size,
                            cx,
                        )
                        .stroke_width(stroke_width)
                        .progress_color(progress_color(progress_ratio)),
                    )
                    .hoverable_tooltip(build_tooltip)
                    .into_any_element(),
            )
        }
    }

    fn fast_mode_available(&self, cx: &Context<Self>) -> bool {
        self.as_native_thread(cx)
            .and_then(|thread| thread.read(cx).model())
            .map(|model| model.supports_fast_mode())
            .unwrap_or(false)
    }

    fn refresh_sandbox_status(&mut self, cx: &mut Context<Self>) -> Option<VerifiedSandboxStatus> {
        let thread = self.as_native_thread(cx)?;
        let (key, refresh) =
            thread.update(cx, |thread, cx| thread.refresh_verified_sandbox_status(cx))?;

        if self.sandbox_status_key.as_ref() == Some(&key) {
            return self.sandbox_status.clone();
        }

        match refresh {
            SandboxStatusRefresh::Ready(status) => {
                self.sandbox_status = Some(status.clone());
                self.sandbox_status_key = Some(key);
                self.pending_sandbox_status_key = None;
                Some(status)
            }
            SandboxStatusRefresh::Pending(task) => {
                if self.pending_sandbox_status_key.as_ref() != Some(&key) {
                    self.sandbox_status = None;
                    self.sandbox_status_key = None;
                    self.pending_sandbox_status_key = Some(key.clone());
                    self._sandbox_status_refresh_task = Some(cx.spawn(async move |this, cx| {
                        let status = task.await;
                        this.update(cx, |this, cx| {
                            if this.pending_sandbox_status_key.as_ref() == Some(&key) {
                                this.sandbox_status = Some(status);
                                this.sandbox_status_key = Some(key);
                                this.pending_sandbox_status_key = None;
                                cx.notify();
                            }
                        })
                        .ok();
                    }));
                }
                None
            }
        }
    }

    pub fn render_sandbox_status(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let status = self.refresh_sandbox_status(cx)?;
        let settings_sandbox = status.settings_sandbox.clone();
        let thread_sandbox = status.thread_sandbox.clone();
        let baseline = status.baseline_writable_paths;

        // The lock is struck only when the *merged* result is unsandboxed (the
        // agent runs with ambient permissions). A layer that is merely wide open
        // but still sandboxed keeps the closed lock.
        let (icon, icon_color) = if settings_sandbox
            .clone()
            .merge(thread_sandbox.clone())
            .is_unsandboxed()
        {
            (IconName::LockOff, Color::Muted)
        } else {
            (IconName::Lock, Color::Default)
        };

        let tooltip = match (settings_sandbox, thread_sandbox) {
            // No sandbox at all because the user turned it off in settings: the
            // per-thread layer is moot, so don't show it.
            (ThreadSandbox::Unsandboxed, _) => SandboxStatusTooltip::disabled_in_settings(),
            // Sandboxed by settings, but disabled for this thread: show the
            // settings scope (greyed) for context above the disabled status.
            (ThreadSandbox::Sandboxed(settings_policy), ThreadSandbox::Unsandboxed) => {
                let settings = augment_settings_sandbox_policy(&settings_policy, baseline);
                SandboxStatusTooltip::disabled_for_thread(sandbox_section(
                    "Defined in your settings:",
                    &settings,
                    true,
                ))
            }
            (
                ThreadSandbox::Sandboxed(settings_policy),
                ThreadSandbox::Sandboxed(thread_policy),
            ) => {
                let settings = augment_settings_sandbox_policy(&settings_policy, baseline);
                let thread = SandboxPolicyDisplay::from_policy(&thread_policy);
                // Omit the per-thread section when it grants nothing extra.
                let thread = (!sandbox_policy_grants_nothing(&thread))
                    .then(|| sandbox_section("Allowed for this thread:", &thread, false));
                SandboxStatusTooltip::enabled(
                    sandbox_section("Defined in your settings:", &settings, true),
                    thread,
                )
            }
        };

        Some(
            h_flex()
                .gap_1()
                .child(
                    IconButton::new("sandbox-status", icon)
                        .icon_size(IconSize::Small)
                        .icon_color(icon_color)
                        .tooltip(Tooltip::element(move |_window, _cx| {
                            tooltip.clone().into_any_element()
                        }))
                        .on_click(|_, window, cx| {
                            window.dispatch_action(
                                Box::new(zed_actions::OpenSettingsAt {
                                    path: zed_actions::AGENT_SANDBOX_SETTINGS_PATH.to_string(),
                                    target: None,
                                }),
                                cx,
                            );
                        }),
                )
                .child(Divider::vertical())
                .into_any_element(),
        )
    }

    fn render_fast_mode_control(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.fast_mode_available(cx) {
            return None;
        }

        let thread = self.as_native_thread(cx)?.read(cx);
        let is_fast = matches!(thread.speed(), Some(Speed::Fast));

        let model_identity = thread
            .model()
            .map(|model| (model.provider_id(), model.id()));

        let (tooltip_label, color, icon, new_speed) = if is_fast {
            (
                "Disable Fast Mode",
                Color::Accent,
                IconName::FastForward,
                Speed::Standard,
            )
        } else {
            (
                "Enable Fast Mode",
                Color::Custom(cx.theme().colors().icon_disabled.opacity(0.8)),
                IconName::FastForwardOff,
                Speed::Fast,
            )
        };

        let focus_handle = self.message_editor.focus_handle(cx);

        let pending_confirmation = (!is_fast)
            .then(|| self.pending_fast_mode_confirmation(cx))
            .flatten();

        let icon_button = IconButton::new("fast-mode", icon)
            .icon_size(IconSize::Small)
            .icon_color(color);

        if let Some((provider_id, model_id, confirmation)) = pending_confirmation {
            let weak_self = cx.entity().downgrade();
            let tooltip_focus = focus_handle;

            return Some(
                PopoverMenu::new("fast-mode-warning")
                    .with_handle(self.fast_mode_menu_handle.clone())
                    .trigger_with_tooltip(icon_button, move |_, cx| {
                        Tooltip::for_action_in(tooltip_label, &ToggleFastMode, &tooltip_focus, cx)
                    })
                    .menu(move |window, cx| {
                        let weak_self = weak_self.clone();
                        let confirmation = confirmation.clone();
                        let provider_id = provider_id.clone();
                        let model_id = model_id.clone();

                        Some(ContextMenu::build(window, cx, move |menu, _window, _cx| {
                            let message = confirmation.message.clone();
                            menu.custom_row(move |_window, _cx| {
                                div()
                                    .max_w_72()
                                    .child(Label::new(confirmation.title.clone()))
                                    .child(Label::new(message.clone()).color(Color::Muted))
                                    .into_any_element()
                            })
                            .separator()
                            .item(ContextMenuEntry::new("Enable Now").handler({
                                let weak_self = weak_self.clone();
                                move |_window, cx| {
                                    weak_self
                                        .update(cx, |this, cx| {
                                            this.apply_fast_mode_speed(Speed::Fast, cx);
                                        })
                                        .log_err();
                                }
                            }))
                            .item(
                                ContextMenuEntry::new("Enable and Don't Show Again").handler({
                                    let weak_self = weak_self.clone();
                                    let provider_id = provider_id.clone();
                                    let model_id = model_id;
                                    move |_window, cx| {
                                        weak_self
                                            .update(cx, |this, cx| {
                                                this.apply_fast_mode_speed(Speed::Fast, cx);
                                            })
                                            .log_err();
                                        set_fast_mode_warning_dismissed(
                                            &provider_id,
                                            &model_id,
                                            cx,
                                        );
                                    }
                                }),
                            )
                        }))
                    })
                    .offset(gpui::Point {
                        x: px(0.0),
                        y: px(-2.0),
                    })
                    .anchor(gpui::Anchor::BottomLeft)
                    .into_any_element(),
            );
        }

        let _ = model_identity;

        Some(
            icon_button
                .tooltip(move |_, cx| {
                    Tooltip::for_action_in(tooltip_label, &ToggleFastMode, &focus_handle, cx)
                })
                .on_click(cx.listener(move |this, _, _window, cx| {
                    this.apply_fast_mode_speed(new_speed, cx);
                }))
                .into_any_element(),
        )
    }

    fn pending_fast_mode_confirmation(
        &self,
        cx: &App,
    ) -> Option<(
        LanguageModelProviderId,
        LanguageModelId,
        FastModeConfirmation,
    )> {
        let thread = self.as_native_thread(cx)?.read(cx);
        let model = thread.model()?;
        let provider_id = model.provider_id();
        let model_id = model.id();
        let confirmation = LanguageModelRegistry::read_global(cx)
            .provider(&provider_id)
            .and_then(|provider| provider.fast_mode_confirmation(cx))?;
        if fast_mode_warning_dismissed(&provider_id, &model_id, cx) {
            return None;
        }
        Some((provider_id, model_id, confirmation))
    }

    fn render_thinking_control(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let thread = self.as_native_thread(cx)?.read(cx);
        let model = thread.model()?;

        let supports_thinking = model.supports_thinking();
        if !supports_thinking {
            return None;
        }

        // A toggle would be dishonest for models that always think: only
        // offer the effort selector.
        if !model.supports_disabling_thinking() {
            let effort_levels = model.supported_effort_levels();
            if effort_levels.is_empty() {
                return None;
            }
            return Some(
                self.render_effort_selector(
                    effort_levels,
                    thread.thinking_effort().cloned(),
                    true,
                    cx,
                )
                .into_any_element(),
            );
        }

        let thinking = thread.thinking_enabled();

        let (tooltip_label, icon, color) = if thinking {
            (
                "Disable Thinking Mode",
                IconName::ThinkingMode,
                Color::Accent,
            )
        } else {
            (
                "Enable Thinking Mode",
                IconName::ThinkingModeOff,
                Color::Custom(cx.theme().colors().icon_disabled.opacity(0.8)),
            )
        };

        let focus_handle = self.message_editor.focus_handle(cx);

        let thinking_toggle = IconButton::new("thinking-mode", icon)
            .icon_size(IconSize::Small)
            .icon_color(color)
            .tooltip(move |_, cx| {
                Tooltip::for_action_in(tooltip_label, &ToggleThinkingMode, &focus_handle, cx)
            })
            .on_click(cx.listener(move |this, _, _window, cx| {
                if let Some(thread) = this.as_native_thread(cx) {
                    thread.update(cx, |thread, cx| {
                        let enable_thinking = !thread.thinking_enabled();
                        thread.set_thinking_enabled(enable_thinking, cx);

                        let favorite_key = thread.model().map(|model| {
                            (model.provider_id().0.to_string(), model.id().0.to_string())
                        });
                        let fs = thread.project().read(cx).fs().clone();
                        update_settings_file(fs, cx, move |settings, _| {
                            if let Some(agent) = settings.agent.as_mut() {
                                if let Some(default_model) = agent.default_model.as_mut() {
                                    default_model.enable_thinking = enable_thinking;
                                }
                                if let Some((provider_id, model_id)) = &favorite_key {
                                    agent.update_favorite_model(
                                        provider_id,
                                        model_id,
                                        |favorite| favorite.enable_thinking = enable_thinking,
                                    );
                                }
                            }
                        });
                    });
                }
            }));

        if model.supported_effort_levels().is_empty() {
            return Some(thinking_toggle.into_any_element());
        }

        if !model.supported_effort_levels().is_empty() && !thinking {
            return Some(thinking_toggle.into_any_element());
        }

        let left_btn = thinking_toggle;
        let right_btn = self.render_effort_selector(
            model.supported_effort_levels(),
            thread.thinking_effort().cloned(),
            false,
            cx,
        );

        Some(
            SplitButton::new(left_btn, right_btn.into_any_element())
                .style(SplitButtonStyle::Transparent)
                .into_any_element(),
        )
    }

    fn render_effort_selector(
        &self,
        supported_effort_levels: Vec<LanguageModelEffortLevel>,
        selected_effort: Option<String>,
        standalone: bool,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let weak_self = cx.weak_entity();

        let default_effort_level = supported_effort_levels
            .iter()
            .find(|effort_level| effort_level.is_default)
            .cloned();

        let selected = selected_effort.and_then(|effort| {
            supported_effort_levels
                .iter()
                .find(|level| level.value == effort)
                .cloned()
        });

        let label = selected
            .clone()
            .or(default_effort_level)
            .map_or("Select Effort".into(), |effort| effort.name);

        let (label_color, icon) = if self.thinking_effort_menu_handle.is_deployed() {
            (Color::Accent, IconName::ChevronUp)
        } else {
            (Color::Muted, IconName::ChevronDown)
        };

        let focus_handle = self.message_editor.focus_handle(cx);
        let show_cycle_row = supported_effort_levels.len() > 1;

        let tooltip = Tooltip::element({
            move |_, cx| {
                let mut content = v_flex().gap_1().child(
                    h_flex()
                        .gap_2()
                        .justify_between()
                        .child(Label::new("Change Thinking Effort"))
                        .child(KeyBinding::for_action_in(
                            &ToggleThinkingEffortMenu,
                            &focus_handle,
                            cx,
                        )),
                );

                if show_cycle_row {
                    content = content.child(
                        h_flex()
                            .pt_1()
                            .gap_2()
                            .justify_between()
                            .border_t_1()
                            .border_color(cx.theme().colors().border_variant)
                            .child(Label::new("Cycle Thinking Effort"))
                            .child(KeyBinding::for_action_in(
                                &CycleThinkingEffort,
                                &focus_handle,
                                cx,
                            )),
                    );
                }

                content.into_any_element()
            }
        });

        let trigger = if standalone {
            ButtonLike::new("effort-selector-trigger").child(
                h_flex()
                    .gap_1()
                    .child(
                        Icon::new(IconName::ThinkingMode)
                            .size(IconSize::Small)
                            .color(Color::Accent),
                    )
                    .child(Label::new(label).size(LabelSize::Small).color(label_color))
                    .child(Icon::new(icon).size(IconSize::XSmall).color(Color::Muted)),
            )
        } else {
            ButtonLike::new_rounded_right("effort-selector-trigger")
                .child(Label::new(label).size(LabelSize::Small).color(label_color))
                .child(Icon::new(icon).size(IconSize::XSmall).color(Color::Muted))
        };

        PopoverMenu::new("effort-selector")
            .trigger_with_tooltip(
                trigger.selected_style(ButtonStyle::Tinted(TintColor::Accent)),
                tooltip,
            )
            .menu(move |window, cx| {
                Some(ContextMenu::build(window, cx, |mut menu, _window, _cx| {
                    menu = menu.header("Change Thinking Effort");

                    for effort_level in supported_effort_levels.clone() {
                        let is_selected = selected
                            .as_ref()
                            .is_some_and(|selected| selected.value == effort_level.value);
                        let entry = ContextMenuEntry::new(effort_level.name)
                            .toggleable(IconPosition::End, is_selected);

                        menu.push_item(entry.handler({
                            let effort = effort_level.value.clone();
                            let weak_self = weak_self.clone();
                            move |_window, cx| {
                                let effort = effort.clone();
                                weak_self
                                    .update(cx, |this, cx| {
                                        if let Some(thread) = this.as_native_thread(cx) {
                                            thread.update(cx, |thread, cx| {
                                                thread.set_thinking_effort(
                                                    Some(effort.to_string()),
                                                    cx,
                                                );

                                                let favorite_key = thread.model().map(|model| {
                                                    (
                                                        model.provider_id().0.to_string(),
                                                        model.id().0.to_string(),
                                                    )
                                                });
                                                let fs = thread.project().read(cx).fs().clone();
                                                update_settings_file(fs, cx, move |settings, _| {
                                                    if let Some(agent) = settings.agent.as_mut() {
                                                        if let Some(default_model) =
                                                            agent.default_model.as_mut()
                                                        {
                                                            default_model.effort =
                                                                Some(effort.to_string());
                                                        }
                                                        if let Some((provider_id, model_id)) =
                                                            &favorite_key
                                                        {
                                                            agent.update_favorite_model(
                                                                provider_id,
                                                                model_id,
                                                                |favorite| {
                                                                    favorite.effort =
                                                                        Some(effort.to_string())
                                                                },
                                                            );
                                                        }
                                                    }
                                                });
                                            });
                                        }
                                    })
                                    .ok();
                            }
                        }));
                    }

                    menu
                }))
            })
            .with_handle(self.thinking_effort_menu_handle.clone())
            .offset(gpui::Point {
                x: px(0.0),
                y: px(-2.0),
            })
            .anchor(gpui::Anchor::BottomLeft)
    }

    fn render_send_button(&self, cx: &mut Context<Self>) -> AnyElement {
        let message_editor = self.message_editor.read(cx);
        let is_editor_empty = message_editor.is_empty(cx);
        let focus_handle = message_editor.focus_handle(cx);

        let is_generating = self.thread.read(cx).status() != ThreadStatus::Idle;

        if self.is_loading_contents {
            div()
                .id("loading-message-content")
                .px_1()
                .tooltip(Tooltip::text("Loading Added Context…"))
                .child(loading_contents_spinner(IconSize::default()))
                .into_any_element()
        } else if is_generating && is_editor_empty {
            IconButton::new("stop-generation", IconName::Stop)
                .icon_color(Color::Error)
                .style(ButtonStyle::Tinted(TintColor::Error))
                .tooltip(move |_window, cx| {
                    Tooltip::for_action("Stop Generation", &editor::actions::Cancel, cx)
                })
                .on_click(cx.listener(|this, _event, _, cx| this.cancel_generation(cx)))
                .into_any_element()
        } else {
            let send_icon = if is_generating {
                IconName::QueueMessage
            } else {
                IconName::Send
            };
            IconButton::new("send-message", send_icon)
                .style(ButtonStyle::Filled)
                .map(|this| {
                    if is_editor_empty && !is_generating {
                        this.disabled(true).icon_color(Color::Muted)
                    } else {
                        this.icon_color(Color::Accent)
                    }
                })
                .tooltip(move |_window, cx| {
                    if is_editor_empty && !is_generating {
                        Tooltip::for_action("Type to Send", &Chat, cx)
                    } else if is_generating {
                        let focus_handle = focus_handle.clone();

                        Tooltip::element(move |_window, cx| {
                            v_flex()
                                .gap_1()
                                .child(
                                    h_flex()
                                        .gap_2()
                                        .justify_between()
                                        .child(Label::new("Queue and Send"))
                                        .child(KeyBinding::for_action_in(&Chat, &focus_handle, cx)),
                                )
                                .child(
                                    h_flex()
                                        .pt_1()
                                        .gap_2()
                                        .justify_between()
                                        .border_t_1()
                                        .border_color(cx.theme().colors().border_variant)
                                        .child(Label::new("Send Immediately"))
                                        .child(KeyBinding::for_action_in(
                                            &SendImmediately,
                                            &focus_handle,
                                            cx,
                                        )),
                                )
                                .into_any_element()
                        })(_window, cx)
                    } else {
                        Tooltip::for_action("Send Message", &Chat, cx)
                    }
                })
                .on_click(cx.listener(|this, _, window, cx| {
                    this.send(window, cx);
                }))
                .into_any_element()
        }
    }

    fn render_add_context_button(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let focus_handle = self.message_editor.focus_handle(cx);
        let weak_self = cx.weak_entity();

        PopoverMenu::new("add-context-menu")
            .trigger_with_tooltip(
                IconButton::new("add-context", IconName::Plus)
                    .icon_size(IconSize::Small)
                    .icon_color(Color::Muted),
                {
                    move |_window, cx| {
                        Tooltip::for_action_in(
                            "Add Context",
                            &OpenAddContextMenu,
                            &focus_handle,
                            cx,
                        )
                    }
                },
            )
            .anchor(gpui::Anchor::BottomLeft)
            .with_handle(self.add_context_menu_handle.clone())
            .offset(gpui::Point {
                x: px(0.0),
                y: px(-2.0),
            })
            .menu(move |window, cx| {
                weak_self
                    .update(cx, |this, cx| this.build_add_context_menu(window, cx))
                    .ok()
            })
    }

    fn build_add_context_menu(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<ContextMenu> {
        let message_editor = self.message_editor.clone();
        let workspace = self.workspace.clone();
        let session_capabilities = self.session_capabilities.read();
        let supports_images = session_capabilities.supports_images();
        let supports_embedded_context = session_capabilities.supports_embedded_context();
        let available_skills = session_capabilities.completion_skills();
        drop(session_capabilities);

        let has_editor_selection = workspace
            .upgrade()
            .and_then(|ws| {
                ws.read(cx)
                    .active_item(cx)
                    .and_then(|item| item.downcast::<Editor>())
            })
            .is_some_and(|editor| {
                editor.update(cx, |editor, cx| {
                    editor.has_non_empty_selection(&editor.display_snapshot(cx))
                })
            });

        let has_terminal_selection = workspace
            .upgrade()
            .and_then(|ws| ws.read(cx).panel::<TerminalPanel>(cx))
            .is_some_and(|panel| !panel.read(cx).terminal_selections(cx).is_empty());

        let has_selection = has_editor_selection || has_terminal_selection;

        ContextMenu::build(window, cx, move |menu, _window, _cx| {
            menu.key_context("AddContextMenu")
                .item(
                    ContextMenuEntry::new("Files & Directories")
                        .icon(IconName::File)
                        .icon_color(Color::Muted)
                        .icon_size(IconSize::XSmall)
                        .handler({
                            let message_editor = message_editor.clone();
                            move |window, cx| {
                                message_editor.focus_handle(cx).focus(window, cx);
                                message_editor.update(cx, |editor, cx| {
                                    editor.insert_context_type("file", window, cx);
                                });
                            }
                        }),
                )
                .item(
                    ContextMenuEntry::new("Symbols")
                        .icon(IconName::Code)
                        .icon_color(Color::Muted)
                        .icon_size(IconSize::XSmall)
                        .handler({
                            let message_editor = message_editor.clone();
                            move |window, cx| {
                                message_editor.focus_handle(cx).focus(window, cx);
                                message_editor.update(cx, |editor, cx| {
                                    editor.insert_context_type("symbol", window, cx);
                                });
                            }
                        }),
                )
                .item(
                    ContextMenuEntry::new("Threads")
                        .icon(IconName::Thread)
                        .icon_color(Color::Muted)
                        .icon_size(IconSize::XSmall)
                        .handler({
                            let message_editor = message_editor.clone();
                            move |window, cx| {
                                message_editor.focus_handle(cx).focus(window, cx);
                                message_editor.update(cx, |editor, cx| {
                                    editor.insert_context_type("thread", window, cx);
                                });
                            }
                        }),
                )
                .when(!available_skills.is_empty(), |this| {
                    this.submenu_with_colored_icon("Skills", IconName::Sparkle, Color::Muted, {
                        let message_editor = message_editor.clone();
                        let available_skills = available_skills.clone();
                        move |mut menu, _window, _cx| {
                            for skill in &available_skills {
                                menu = menu
                                    .item(Self::skill_menu_entry(skill, message_editor.clone()));
                            }
                            menu
                        }
                    })
                })
                .item(
                    ContextMenuEntry::new("Image")
                        .icon(IconName::Image)
                        .icon_color(Color::Muted)
                        .icon_size(IconSize::XSmall)
                        .disabled(!supports_images)
                        .handler({
                            let message_editor = message_editor.clone();
                            move |window, cx| {
                                message_editor.focus_handle(cx).focus(window, cx);
                                message_editor.update(cx, |editor, cx| {
                                    editor.add_images_from_picker(window, cx);
                                });
                            }
                        }),
                )
                .item(
                    ContextMenuEntry::new("Selection")
                        .icon(IconName::CursorIBeam)
                        .icon_color(Color::Muted)
                        .icon_size(IconSize::XSmall)
                        .disabled(!has_selection)
                        .handler({
                            move |window, cx| {
                                window.dispatch_action(
                                    zed_actions::agent::AddSelectionToThread.boxed_clone(),
                                    cx,
                                );
                            }
                        }),
                )
                .item(
                    ContextMenuEntry::new("Branch Diff")
                        .icon(IconName::GitBranch)
                        .icon_color(Color::Muted)
                        .icon_size(IconSize::XSmall)
                        .disabled(!supports_embedded_context)
                        .handler({
                            move |window, cx| {
                                message_editor.update(cx, |editor, cx| {
                                    editor.insert_branch_diff_crease(window, cx);
                                });
                            }
                        }),
                )
        })
    }

    fn skill_menu_entry(
        skill: &AvailableSkill,
        message_editor: Entity<crate::message_editor::MessageEditor>,
    ) -> ContextMenuEntry {
        let label = format!("{} ({})", skill.name, skill.source);
        let skill = skill.clone();

        ContextMenuEntry::new(label)
            .icon(IconName::Sparkle)
            .icon_color(Color::Muted)
            .icon_size(IconSize::XSmall)
            .handler(move |window, cx| {
                message_editor.focus_handle(cx).focus(window, cx);
                message_editor.update(cx, |editor, cx| {
                    editor.insert_skill_crease(&skill, window, cx);
                });
            })
    }

    fn render_follow_toggle(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let following = self.is_following(cx);

        let tooltip_label = if following {
            if self.agent_id.as_ref() == agent::ZED_AGENT_ID.as_ref() {
                format!("Stop Following the {}", self.agent_id)
            } else {
                format!("Stop Following {}", self.agent_id)
            }
        } else {
            if self.agent_id.as_ref() == agent::ZED_AGENT_ID.as_ref() {
                format!("Follow the {}", self.agent_id)
            } else {
                format!("Follow {}", self.agent_id)
            }
        };

        IconButton::new("follow-agent", IconName::Crosshair)
            .icon_size(IconSize::Small)
            .icon_color(Color::Muted)
            .toggle_state(following)
            .selected_icon_color(Some(Color::Custom(cx.theme().players().agent().cursor)))
            .tooltip(move |_window, cx| {
                if following {
                    Tooltip::for_action(tooltip_label.clone(), &Follow, cx)
                } else {
                    Tooltip::with_meta(
                        tooltip_label.clone(),
                        Some(&Follow),
                        "Track the agent's location as it reads and edits files.",
                        cx,
                    )
                }
            })
            .on_click(cx.listener(move |this, _, window, cx| {
                this.toggle_following(window, cx);
            }))
    }
}

struct TokenUsageTooltip {
    percentage: String,
    used: String,
    max: String,
    input_tokens: String,
    output_tokens: String,
    input_max: String,
    output_max: String,
    show_split: bool,
    cost_label: Option<String>,
    separator_color: Color,
    global_agents_md_loaded: bool,
    project_rules_count: usize,
    project_entry_ids: Vec<ProjectEntryId>,
    workspace: WeakEntity<Workspace>,
}

impl Render for TokenUsageTooltip {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let separator_color = self.separator_color;
        let percentage = self.percentage.clone();
        let used = self.used.clone();
        let max = self.max.clone();
        let input_tokens = self.input_tokens.clone();
        let output_tokens = self.output_tokens.clone();
        let input_max = self.input_max.clone();
        let output_max = self.output_max.clone();
        let show_split = self.show_split;
        let cost_label = self.cost_label.clone();
        let global_agents_md_loaded = self.global_agents_md_loaded;
        let project_rules_count = self.project_rules_count;
        let project_entry_ids = self.project_entry_ids.clone();
        let workspace = self.workspace.clone();

        ui::tooltip_container(cx, move |container, cx| {
            container
                .min_w_40()
                .child(
                    Label::new("Context")
                        .color(Color::Muted)
                        .size(LabelSize::Small),
                )
                .when(!show_split, |this| {
                    this.child(
                        h_flex()
                            .gap_0p5()
                            .child(Label::new(percentage.clone()))
                            .child(Label::new("\u{2022}").color(separator_color).mx_1())
                            .child(Label::new(used.clone()))
                            .child(Label::new("/").color(separator_color))
                            .child(Label::new(max.clone()).color(Color::Muted)),
                    )
                })
                .when(show_split, |this| {
                    this.child(
                        v_flex()
                            .gap_0p5()
                            .child(
                                h_flex()
                                    .gap_0p5()
                                    .child(Label::new("Input:").color(Color::Muted).mr_0p5())
                                    .child(Label::new(input_tokens))
                                    .child(Label::new("/").color(separator_color))
                                    .child(Label::new(input_max).color(Color::Muted)),
                            )
                            .child(
                                h_flex()
                                    .gap_0p5()
                                    .child(Label::new("Output:").color(Color::Muted).mr_0p5())
                                    .child(Label::new(output_tokens))
                                    .child(Label::new("/").color(separator_color))
                                    .child(Label::new(output_max).color(Color::Muted)),
                            ),
                    )
                })
                .when_some(cost_label, |this, cost_label| {
                    this.child(
                        v_flex()
                            .mt_1p5()
                            .pt_1p5()
                            .gap_0p5()
                            .border_t_1()
                            .border_color(cx.theme().colors().border_variant)
                            .child(
                                Label::new("Cost")
                                    .color(Color::Muted)
                                    .size(LabelSize::Small),
                            )
                            .child(Label::new(cost_label)),
                    )
                })
                .when(
                    global_agents_md_loaded || project_rules_count > 0,
                    move |this| {
                        this.child(
                            v_flex()
                                .mt_1p5()
                                .pt_1p5()
                                .pb_0p5()
                                .gap_0p5()
                                .border_t_1()
                                .border_color(cx.theme().colors().border_variant)
                                .child(
                                    Label::new("Rules")
                                        .color(Color::Muted)
                                        .size(LabelSize::Small),
                                )
                                .child(
                                    v_flex()
                                        .mx_neg_1()
                                        .when(global_agents_md_loaded, {
                                            let workspace = workspace.clone();
                                            move |this| {
                                                this.child(
                                                    Button::new(
                                                        "open-global-agents-md",
                                                        "1 global rule",
                                                    )
                                                    .end_icon(
                                                        Icon::new(IconName::ArrowUpRight)
                                                            .color(Color::Muted)
                                                            .size(IconSize::XSmall),
                                                    )
                                                    .on_click(move |_, window, cx| {
                                                        workspace
                                                            .update(cx, |workspace, cx| {
                                                                workspace
                                                                    .open_abs_path(
                                                                        paths::agents_file()
                                                                            .clone(),
                                                                        workspace::OpenOptions {
                                                                            focus: Some(true),
                                                                            ..Default::default()
                                                                        },
                                                                        window,
                                                                        cx,
                                                                    )
                                                                    .detach_and_log_err(cx);
                                                            })
                                                            .log_err();
                                                    }),
                                                )
                                            }
                                        })
                                        .when(project_rules_count > 0, move |this| {
                                            let workspace = workspace.clone();
                                            let project_entry_ids = project_entry_ids.clone();
                                            this.child(
                                                Button::new(
                                                    "open-project-rules",
                                                    format!(
                                                        "{} {}",
                                                        project_rules_count,
                                                        pluralize(
                                                            "project rule",
                                                            project_rules_count
                                                        )
                                                    ),
                                                )
                                                .end_icon(
                                                    Icon::new(IconName::ArrowUpRight)
                                                        .color(Color::Muted)
                                                        .size(IconSize::XSmall),
                                                )
                                                .on_click(move |_, window, cx| {
                                                    let _ =
                                                        workspace.update(cx, |workspace, cx| {
                                                            let project =
                                                                workspace.project().read(cx);
                                                            let paths = project_entry_ids
                                                                .iter()
                                                                .flat_map(|id| {
                                                                    project.path_for_entry(*id, cx)
                                                                })
                                                                .collect::<Vec<_>>();
                                                            for path in paths {
                                                                workspace
                                                                    .open_path(
                                                                        path, None, true, window,
                                                                        cx,
                                                                    )
                                                                    .detach_and_log_err(cx);
                                                            }
                                                        });
                                                }),
                                            )
                                        }),
                                ),
                        )
                    },
                )
        })
    }
}

/// A display-ready snapshot of a sandbox policy for the status tooltip.
///
/// The opaque `HostFilesystemLocation`s in a policy are stringified up front,
/// when this is built, so the tooltip state (which outlives the build and is
/// captured by the lazy tooltip closure) never holds the locations' fds open.
#[derive(Clone)]
struct SandboxPolicyDisplay {
    fs: SandboxFsDisplay,
    network: SandboxNetPolicy,
}

/// The filesystem write-access portion of a [`SandboxPolicyDisplay`].
#[derive(Clone)]
enum SandboxFsDisplay {
    Unrestricted,
    Restricted(Vec<WritableEntryDisplay>),
}

/// A single writable entry to display in the sandbox tooltip: either a real host
/// location (already stringified for display) or the Linux-only host-isolated
/// `/tmp` overlay, which has no backing host path and is purely a label.
#[derive(Clone)]
enum WritableEntryDisplay {
    Path(String),
    // Only ever constructed on Linux (the bwrap `--tmpfs /tmp` overlay), so the
    // variant is gated to match and avoid dead-code warnings elsewhere.
    #[cfg(target_os = "linux")]
    IsolatedTmp,
}

impl SandboxPolicyDisplay {
    /// Display a policy verbatim (used for the per-thread overrides, which carry
    /// no implicit baseline grants). Takes the policy by reference and stringifies
    /// its locations immediately, so no fd is retained past this call.
    fn from_policy(policy: &SandboxPolicy) -> Self {
        let fs = match &policy.fs {
            SandboxFsPolicy::Unrestricted { .. } => SandboxFsDisplay::Unrestricted,
            SandboxFsPolicy::Restricted { writable_paths, .. } => SandboxFsDisplay::Restricted(
                writable_paths
                    .iter()
                    .map(|location| {
                        WritableEntryDisplay::Path(location.untrusted_path_display().to_string())
                    })
                    .collect(),
            ),
        };
        SandboxPolicyDisplay {
            fs,
            network: policy.network.clone(),
        }
    }
}

/// Fold the always-granted baseline writable paths (the project's worktree
/// roots, derived from the same source the terminal tool uses) and, on Linux,
/// the host-isolated `/tmp` overlay into a settings policy for display. These
/// are part of what the sandbox grants whenever it's active but aren't
/// persistent-settings entries, so they're shown in the "from your settings"
/// section rather than stored. A no-op when the fs is unrestricted (rendered as
/// "All paths"), since there's nothing to scope.
fn augment_settings_sandbox_policy(
    policy: &SandboxPolicy,
    baseline: Vec<PathBuf>,
) -> SandboxPolicyDisplay {
    let fs = match &policy.fs {
        SandboxFsPolicy::Unrestricted { .. } => SandboxFsDisplay::Unrestricted,
        SandboxFsPolicy::Restricted { writable_paths, .. } => {
            // Dedup by display string. We deliberately don't open the locations'
            // fds to dedup by inode here: this is a display-only tooltip and the
            // string is the location's identity for that purpose. The string can
            // only diverge from the captured inode while a symlink-swap is
            // actively in progress, and in that case the bind validator refuses
            // to run the command at all (see the `sandbox` crate) — so showing the
            // requested path is always safe, and not worth a blocking syscall on
            // the render path.
            let mut merged: Vec<String> = Vec::new();
            let baseline_paths = baseline.iter().map(|path| path.display().to_string());
            let granted_paths = writable_paths
                .iter()
                .map(|location| location.untrusted_path_display().to_string());
            for path in baseline_paths.chain(granted_paths) {
                if !merged.contains(&path) {
                    merged.push(path);
                }
            }
            // `mut` is only needed on Linux, where the isolated `/tmp` entry is
            // pushed below.
            #[cfg_attr(not(target_os = "linux"), allow(unused_mut))]
            let mut entries: Vec<WritableEntryDisplay> =
                merged.into_iter().map(WritableEntryDisplay::Path).collect();
            // The ephemeral, host-isolated tmpfs at /tmp is Linux-specific (the
            // bwrap `--tmpfs /tmp` overlay). It's a display-only label, not a
            // real host path, so it can't be a captured location.
            #[cfg(target_os = "linux")]
            entries.push(WritableEntryDisplay::IsolatedTmp);
            SandboxFsDisplay::Restricted(entries)
        }
    };
    SandboxPolicyDisplay {
        fs,
        network: policy.network.clone(),
    }
}

fn sandbox_section(title: &str, policy: &SandboxPolicyDisplay, show_empty: bool) -> SandboxSection {
    let write_empty = fs_grants_nothing(&policy.fs);
    let network_empty = network_grants_nothing(&policy.network);
    let mut section = SandboxSection::new(title.to_string());

    if show_empty || !write_empty {
        section =
            section.group(SandboxGroup::new("Write Access").rows(sandbox_fs_rows(&policy.fs)));
    }

    if show_empty || !network_empty {
        section = section
            .group(SandboxGroup::new("Network Access").rows(sandbox_network_rows(&policy.network)));
    }

    section
}

/// Whether a policy grants nothing worth surfacing, used to decide whether to
/// show the per-thread overrides section at all.
fn sandbox_policy_grants_nothing(policy: &SandboxPolicyDisplay) -> bool {
    fs_grants_nothing(&policy.fs) && network_grants_nothing(&policy.network)
}

fn fs_grants_nothing(fs: &SandboxFsDisplay) -> bool {
    matches!(fs, SandboxFsDisplay::Restricted(entries) if entries.is_empty())
}

fn network_grants_nothing(network: &SandboxNetPolicy) -> bool {
    match network {
        SandboxNetPolicy::Blocked => true,
        SandboxNetPolicy::Restricted { allowed_domains } => allowed_domains.is_empty(),
        SandboxNetPolicy::Unrestricted => false,
    }
}

/// Rows for the write-access group: a message for the "all"/"none" cases, or one
/// row per granted path.
fn sandbox_fs_rows(fs: &SandboxFsDisplay) -> Vec<SandboxRow> {
    match fs {
        SandboxFsDisplay::Unrestricted => vec![SandboxRow::message(
            "All paths except protected Git metadata",
        )],
        SandboxFsDisplay::Restricted(entries) if entries.is_empty() => {
            vec![SandboxRow::message("None")]
        }
        SandboxFsDisplay::Restricted(entries) => entries
            .iter()
            .map(|entry| match entry {
                // The display string was captured up front.
                WritableEntryDisplay::Path(path) => SandboxRow::path(PathBuf::from(path)),
                #[cfg(target_os = "linux")]
                WritableEntryDisplay::IsolatedTmp => {
                    SandboxRow::path(PathBuf::from("/tmp (isolated)"))
                }
            })
            .collect(),
    }
}

/// Rows for the network-access group: a message for the "all"/"none" cases, or
/// one row per allowed domain.
fn sandbox_network_rows(network: &SandboxNetPolicy) -> Vec<SandboxRow> {
    match network {
        SandboxNetPolicy::Unrestricted => vec![SandboxRow::message("All domains (unrestricted)")],
        SandboxNetPolicy::Blocked => vec![SandboxRow::message("None")],
        SandboxNetPolicy::Restricted { allowed_domains } if allowed_domains.is_empty() => {
            vec![SandboxRow::message("None")]
        }
        SandboxNetPolicy::Restricted { allowed_domains } => allowed_domains
            .iter()
            .map(|domain| SandboxRow::domain(domain.clone()))
            .collect(),
    }
}

impl ThreadView {
    fn render_entries(&mut self, cx: &mut Context<Self>) -> List {
        let max_content_width = AgentSettings::get_global(cx).max_content_width;
        let centered_container = move |content: AnyElement| {
            h_flex().w_full().justify_center().child(
                div()
                    .when_some(max_content_width, |this, max_w| this.max_w(max_w))
                    .w_full()
                    .child(content),
            )
        };

        list(
            self.list_state.clone(),
            cx.processor(move |this, index: usize, window, cx| {
                let entries = this.thread.read(cx).entries();
                if let Some(entry) = entries.get(index) {
                    let rendered = this.render_entry(index, entries.len(), entry, window, cx);
                    centered_container(rendered.into_any_element()).into_any_element()
                } else if this.generating_indicator_in_list {
                    let confirmation = this.thread.read(cx).is_waiting_for_confirmation()
                        || this.has_pending_request_elicitation(cx);
                    let rendered = this.render_generating(confirmation, cx);
                    centered_container(rendered.into_any_element()).into_any_element()
                } else {
                    Empty.into_any()
                }
            }),
        )
        .with_sizing_behavior(gpui::ListSizingBehavior::Auto)
        .flex_grow_1()
    }

    fn render_entry(
        &self,
        entry_ix: usize,
        total_entries: usize,
        entry: &AgentThreadEntry,
        window: &Window,
        cx: &Context<Self>,
    ) -> AnyElement {
        let is_indented = entry.is_indented();
        let is_first_indented = is_indented
            && self
                .thread
                .read(cx)
                .entries()
                .get(entry_ix.saturating_sub(1))
                .is_none_or(|entry| !entry.is_indented());

        let mut assistant_message_is_blank = false;

        let primary = match &entry {
            AgentThreadEntry::UserMessage(message) => {
                let Some(editor) = self
                    .entry_view_state
                    .read(cx)
                    .entry(entry_ix)
                    .and_then(|entry| entry.message_editor())
                    .cloned()
                else {
                    return Empty.into_any_element();
                };

                let editing = self.editing_message == Some(entry_ix);
                let editor_focus = editor.focus_handle(cx).is_focused(window);
                let focus_border = cx.theme().colors().border_focused;
                // Drop shadows render as a dark halo on transparent windows.
                let opaque_window = cx.theme().window_background_appearance()
                    == gpui::WindowBackgroundAppearance::Opaque;

                let has_checkpoint_button = message
                    .checkpoint
                    .as_ref()
                    .is_some_and(|checkpoint| checkpoint.show);

                let is_subagent = self.is_subagent();
                let can_rewind = self.thread.read(cx).supports_truncate(cx);
                let is_editable = can_rewind && message.client_id.is_some() && !is_subagent;
                let agent_name = if is_subagent {
                    "subagents".into()
                } else {
                    self.agent_id.clone()
                };

                v_flex()
                    .id(("user_message", entry_ix))
                    .map(|this| {
                        if is_first_indented {
                            this.pt_0p5()
                        } else {
                            this.pt_2()
                        }
                    })
                    .pb_3()
                    .px_2()
                    .gap_1p5()
                    .w_full()
                    .when(is_editable && has_checkpoint_button, |this| {
                        this.children(message.client_id.clone().map(|client_id| {
                            h_flex()
                                .px_3()
                                .gap_2()
                                .child(Divider::horizontal())
                                .child(
                                    Button::new("restore-checkpoint", "Restore Checkpoint")
                                        .start_icon(Icon::new(IconName::Undo).size(IconSize::XSmall).color(Color::Muted))
                                        .label_size(LabelSize::XSmall)
                                        .color(Color::Muted)
                                        .tooltip(Tooltip::text("Restores all files in the project to the content they had at this point in the conversation."))
                                        .on_click(cx.listener(move |this, _, _window, cx| {
                                            this.restore_checkpoint(&client_id, cx);
                                        }))
                                )
                                .child(Divider::horizontal())
                        }))
                    })
                    .child(
                        div()
                            .relative()
                            .child(
                                div()
                                    .py_3()
                                    .px_2()
                                    .rounded_md()
                                    .bg(self
                                        .agent_panel_styling
                                        .user_message
                                        .background
                                        .unwrap_or(cx.theme().colors().editor_background))
                                    .border_1()
                                    .when(is_indented, |this| {
                                        this.py_2().px_2().when(opaque_window, |this| {
                                            this.shadow_sm()
                                        })
                                    })
                                    .border_color(cx.theme().colors().border)
                                    .map(|this| {
                                        if !is_editable {
                                            if is_subagent {
                                                return this.border_dashed();
                                            }
                                            return this;
                                        }
                                        if editing && editor_focus {
                                            return this.border_color(focus_border);
                                        }
                                        if editing && !editor_focus {
                                            return this.border_dashed()
                                        }
                                        this.when(opaque_window, |this| this.shadow_md())
                                            .hover(|s| {
                                                s.border_color(focus_border.opacity(0.8))
                                            })
                                    })
                                    .text_xs()
                                    .child(editor.clone().into_any_element())
                            )
                            .when(editor_focus, |this| {
                                let base_container = h_flex()
                                    .absolute()
                                    .top_neg_3p5()
                                    .right_3()
                                    .gap_1()
                                    .rounded_sm()
                                    .border_1()
                                    .border_color(cx.theme().colors().border)
                                    .bg(cx.theme().colors().editor_background)
                                    .overflow_hidden();

                                let is_loading_contents = self.is_loading_contents;
                                if is_editable {
                                    this.child(
                                        base_container
                                            .child(
                                                IconButton::new("cancel", IconName::Close)
                                                    .disabled(is_loading_contents)
                                                    .icon_color(Color::Error)
                                                    .icon_size(IconSize::XSmall)
                                                    .on_click(cx.listener(Self::cancel_editing))
                                            )
                                            .child(
                                                if is_loading_contents {
                                                    div()
                                                        .id("loading-edited-message-content")
                                                        .tooltip(Tooltip::text("Loading Added Context…"))
                                                        .child(loading_contents_spinner(IconSize::XSmall))
                                                        .into_any_element()
                                                } else {
                                                    IconButton::new("regenerate", IconName::Return)
                                                        .icon_color(Color::Muted)
                                                        .icon_size(IconSize::XSmall)
                                                        .tooltip(Tooltip::text(
                                                            "Editing will restart the thread from this point."
                                                        ))
                                                        .on_click(cx.listener({
                                                            let editor = editor.clone();
                                                            move |this, _, window, cx| {
                                                                this.regenerate(
                                                                    entry_ix, editor.clone(), window, cx,
                                                                );
                                                            }
                                                        })).into_any_element()
                                                }
                                            )
                                    )
                                } else {
                                    this.child(
                                        base_container
                                            .border_dashed()
                                            .child(IconButton::new("non_editable", IconName::PencilUnavailable)
                                                .icon_size(IconSize::Small)
                                                .icon_color(Color::Muted)
                                                .style(ButtonStyle::Transparent)
                                                .tooltip(Tooltip::element({
                                                    let agent_name = agent_name.clone();
                                                    move |_, _| {
                                                        v_flex()
                                                            .gap_1()
                                                            .child(Label::new("Unavailable Editing"))
                                                            .child(
                                                                div().max_w_64().child(
                                                                    Label::new(format!(
                                                                        "Editing previous messages is not available for {} yet.",
                                                                        agent_name
                                                                    ))
                                                                    .size(LabelSize::Small)
                                                                    .color(Color::Muted),
                                                                ),
                                                            )
                                                            .into_any_element()
                                                    }
                                                }))),
                                    )
                                }
                            }),
                    )
                    .into_any()
            }
            AgentThreadEntry::AssistantMessage(AssistantMessage {
                chunks,
                indented: _,
                is_subagent_output: _,
            }) => {
                let mut is_blank = true;
                let is_last = entry_ix + 1 == total_entries;

                let mut style = MarkdownStyle::themed(MarkdownFont::Agent, window, cx);
                // Air for the read-aloud word pill's vertical inflate.
                // Always on — applying it only during playback would reflow
                // the whole message the moment speech starts.
                style.paragraph_line_height = Some(rems(1.45));
                self.agent_panel_styling
                    .assistant_prose
                    .apply_to_markdown_style(&mut style);
                self.agent_panel_styling
                    .code_blocks
                    .apply_to_code_blocks(&mut style);
                let message_body = v_flex()
                    .w_full()
                    .gap_3()
                    .children(chunks.iter().enumerate().filter_map(
                        |(chunk_ix, chunk)| match chunk {
                            AssistantMessageChunk::Message { block, .. } => {
                                block.markdown().and_then(|md| {
                                    let this_is_blank = md.read(cx).source().trim().is_empty();
                                    is_blank = is_blank && this_is_blank;
                                    if this_is_blank {
                                        return None;
                                    }

                                    Some(
                                        self.render_speakable_markdown(
                                            md.clone(),
                                            style.clone(),
                                            cx,
                                        )
                                        .into_any_element(),
                                    )
                                })
                            }
                            AssistantMessageChunk::Thought { block, .. } => {
                                block.markdown().and_then(|md| {
                                    let this_is_blank = md.read(cx).source().trim().is_empty();
                                    is_blank = is_blank && this_is_blank;
                                    if this_is_blank {
                                        return None;
                                    }
                                    Some(
                                        self.render_thinking_block(
                                            entry_ix,
                                            chunk_ix,
                                            md.clone(),
                                            window,
                                            cx,
                                        )
                                        .into_any_element(),
                                    )
                                })
                            }
                        },
                    ))
                    .into_any();

                assistant_message_is_blank = is_blank;

                if is_blank {
                    Empty.into_any()
                } else {
                    v_flex()
                        .px_5()
                        .py_1p5()
                        .when(is_last, |this| this.pb_4())
                        .w_full()
                        .text_ui(cx)
                        .child(self.render_message_context_menu(entry_ix, message_body, cx))
                        .when_some(
                            self.entry_view_state
                                .read(cx)
                                .entry(entry_ix)
                                .and_then(|entry| entry.focus_handle(cx)),
                            |this, handle| this.track_focus(&handle),
                        )
                        .into_any()
                }
            }
            AgentThreadEntry::ToolCall(tool_call) => {
                // A canceled tool call that produced visible output is still worth
                // showing, but one that was canceled before producing anything just
                // renders as a useless "Canceled" card — hide those entirely.
                if matches!(tool_call.status, ToolCallStatus::Canceled) {
                    let has_visible_content =
                        tool_call.content.iter().any(|content| match content {
                            ToolCallContent::ContentBlock(block) => block.visible_content(cx),
                            ToolCallContent::Diff(_) | ToolCallContent::Terminal(_) => true,
                        });
                    if !has_visible_content {
                        return Empty.into_any();
                    }
                }

                let tool_call = self.render_any_tool_call(
                    self.thread.read(cx).session_id(),
                    entry_ix,
                    tool_call,
                    &self.focus_handle(cx),
                    ToolCallLayout::Standalone,
                    window,
                    cx,
                );

                if let Some(handle) = self
                    .entry_view_state
                    .read(cx)
                    .entry(entry_ix)
                    .and_then(|entry| entry.focus_handle(cx))
                {
                    tool_call.track_focus(&handle).into_any()
                } else {
                    tool_call.into_any()
                }
            }
            AgentThreadEntry::Elicitation(elicitation_id) => {
                let thread = self.thread.read(cx);
                if let Some((_, elicitation)) = thread.elicitation(elicitation_id)
                    && should_render_elicitation(elicitation)
                {
                    let elicitation = self.render_elicitation(entry_ix, elicitation, window, cx);

                    if let Some(handle) = self
                        .entry_view_state
                        .read(cx)
                        .entry(entry_ix)
                        .and_then(|entry| entry.focus_handle(cx))
                    {
                        elicitation.track_focus(&handle).into_any()
                    } else {
                        elicitation.into_any()
                    }
                } else {
                    Empty.into_any()
                }
            }
            AgentThreadEntry::CompletedPlan(entries) => {
                self.render_completed_plan(entries, window, cx)
            }
            AgentThreadEntry::ContextCompaction(compaction) => {
                self.render_context_compaction(entry_ix, compaction, window, cx)
            }
        };

        let is_subagent_output = self.is_subagent()
            && matches!(entry, AgentThreadEntry::AssistantMessage(msg) if msg.is_subagent_output);

        let primary = if is_subagent_output {
            v_flex()
                .w_full()
                .child(
                    h_flex()
                        .id("subagent_output")
                        .px_5()
                        .py_1()
                        .gap_2()
                        .child(Divider::horizontal())
                        .child(
                            h_flex()
                                .gap_1()
                                .child(
                                    Icon::new(IconName::ForwardArrowUp)
                                        .color(Color::Muted)
                                        .size(IconSize::Small),
                                )
                                .child(
                                    Label::new("Subagent Output")
                                        .size(LabelSize::Custom(self.tool_name_font_size()))
                                        .color(Color::Muted),
                                ),
                        )
                        .child(Divider::horizontal())
                        .tooltip(Tooltip::text("Everything below this line was sent as output from this subagent to the main agent.")),
                )
                .child(primary)
                .into_any_element()
        } else {
            primary
        };

        let thread = self.thread.clone();

        let primary = if is_indented {
            let line_top = if is_first_indented {
                rems_from_px(-12.0_f32)
            } else {
                rems_from_px(0.0_f32)
            };

            div()
                .relative()
                .w_full()
                .pl_5()
                .bg(cx.theme().colors().panel_background.opacity(0.2))
                .child(
                    div()
                        .absolute()
                        .left(rems_from_px(18.0_f32))
                        .top(line_top)
                        .bottom_0()
                        .w_px()
                        .bg(cx.theme().colors().border.opacity(0.6)),
                )
                .child(primary)
                .into_any_element()
        } else {
            primary
        };

        let is_generating = matches!(thread.read(cx).status(), ThreadStatus::Generating);

        let is_turn_end = Self::entry_is_finalized_turn_end(thread.read(cx).entries(), entry_ix)
            .unwrap_or(!is_generating);

        let primary = if is_turn_end && !assistant_message_is_blank {
            let user_message_index = thread
                .read(cx)
                .entries()
                .iter()
                .take(entry_ix)
                .rposition(|entry| matches!(entry, AgentThreadEntry::UserMessage(_)));

            v_flex()
                .w_full()
                .child(primary)
                .child(self.render_thread_controls(
                    &thread,
                    entry_ix,
                    Some(entry_ix),
                    entry_ix + 1 == total_entries,
                    user_message_index,
                    cx,
                ))
                .into_any_element()
        } else {
            primary
        };

        let is_assistant = matches!(entry, AgentThreadEntry::AssistantMessage(_));

        let comments_editor = self.thread_feedback.comments_editor.clone();

        let primary = if entry_ix + 1 == total_entries {
            let last_assistant_index = thread
                .read(cx)
                .entries()
                .iter()
                .rposition(|entry| matches!(entry, AgentThreadEntry::AssistantMessage(_)));

            v_flex()
                .w_full()
                .child(primary)
                .when(!is_assistant, |this| {
                    this.child(self.render_thread_controls(
                        &thread,
                        entry_ix,
                        last_assistant_index,
                        true,
                        None,
                        cx,
                    ))
                })
                .when_some(comments_editor, |this, editor| {
                    this.child(Self::render_feedback_feedback_editor(editor, cx))
                })
                .into_any_element()
        } else {
            primary
        };

        if let Some(editing_index) = self.editing_message
            && editing_index < entry_ix
        {
            let is_subagent = self.is_subagent();

            let backdrop = div()
                .id(("backdrop", entry_ix))
                .size_full()
                .absolute()
                .inset_0()
                .bg(cx.theme().colors().panel_background)
                .opacity(0.8)
                .block_mouse_except_scroll()
                .on_click(cx.listener(Self::cancel_editing));

            div()
                .relative()
                .child(primary)
                .when(!is_subagent, |this| this.child(backdrop))
                .into_any_element()
        } else {
            primary
        }
    }

    fn render_elicitation(
        &self,
        entry_ix: usize,
        elicitation: &Elicitation,
        _window: &Window,
        cx: &Context<Self>,
    ) -> Div {
        ElicitationCard::new(
            entry_ix,
            elicitation,
            self.agent_display_name.clone(),
            self.elicitation_form_states.get(&elicitation.id),
            self.elicitation_card_handlers(cx),
        )
        .render(cx)
    }

    fn elicitation_card_handlers(&self, cx: &Context<Self>) -> ElicitationCardHandlers {
        let view = cx.entity().downgrade();

        ElicitationCardHandlers::new(
            {
                let view = view.clone();
                move |elicitation_id, window, cx| {
                    view.update(cx, |this, cx| {
                        this.submit_elicitation(elicitation_id, window, cx);
                    })
                    .log_err();
                }
            },
            {
                let view = view.clone();
                move |elicitation_id, window, cx| {
                    view.update(cx, |this, cx| {
                        this.decline_elicitation(elicitation_id, window, cx);
                    })
                    .log_err();
                }
            },
            {
                let view = view.clone();
                move |elicitation_id, window, cx| {
                    view.update(cx, |this, cx| {
                        this.cancel_elicitation(elicitation_id, window, cx);
                    })
                    .log_err();
                }
            },
            {
                let view = view.clone();
                move |elicitation_id, window, cx| {
                    view.update(cx, |this, cx| {
                        this.dismiss_url_elicitation(elicitation_id, window, cx);
                    })
                    .log_err();
                }
            },
            move |_elicitation_id, url, _window, cx| cx.open_url(&url),
            {
                let view = view.clone();
                move |elicitation_id, field_name, value, cx| {
                    view.update(cx, |this, cx| {
                        if let Some(form) = this.elicitation_form_states.get_mut(&elicitation_id) {
                            form.set_boolean(&field_name, value);
                            cx.notify();
                        }
                    })
                    .log_err();
                }
            },
            {
                let view = view.clone();
                move |elicitation_id, field_name, value, cx| {
                    view.update(cx, |this, cx| {
                        if let Some(form) = this.elicitation_form_states.get_mut(&elicitation_id) {
                            form.set_single_select(&field_name, value);
                            cx.notify();
                        }
                    })
                    .log_err();
                }
            },
            move |elicitation_id, field_name, value, selected, cx| {
                view.update(cx, |this, cx| {
                    if let Some(form) = this.elicitation_form_states.get_mut(&elicitation_id) {
                        form.set_multi_select(&field_name, value, selected);
                        cx.notify();
                    }
                })
                .log_err();
            },
        )
    }

    fn render_feedback_feedback_editor(editor: Entity<Editor>, cx: &Context<Self>) -> Div {
        h_flex()
            .key_context("AgentFeedbackMessageEditor")
            .on_action(cx.listener(move |this, _: &menu::Cancel, _, cx| {
                this.thread_feedback.dismiss_comments();
                cx.notify();
            }))
            .on_action(cx.listener(move |this, _: &menu::Confirm, _window, cx| {
                this.submit_feedback_message(cx);
            }))
            .p_2()
            .mb_2()
            .mx_5()
            .gap_1()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().editor_background)
            .child(div().w_full().child(editor))
            .child(
                h_flex()
                    .child(
                        IconButton::new("dismiss-feedback-message", IconName::Close)
                            .icon_color(Color::Error)
                            .icon_size(IconSize::XSmall)
                            .shape(ui::IconButtonShape::Square)
                            .on_click(cx.listener(move |this, _, _window, cx| {
                                this.thread_feedback.dismiss_comments();
                                cx.notify();
                            })),
                    )
                    .child(
                        IconButton::new("submit-feedback-message", IconName::Return)
                            .icon_size(IconSize::XSmall)
                            .shape(ui::IconButtonShape::Square)
                            .on_click(cx.listener(move |this, _, _window, cx| {
                                this.submit_feedback_message(cx);
                            })),
                    ),
            )
    }

    /// A turn ends when no further assistant output (message or tool call)
    /// follows before the next user message, and it's finalized once a user
    /// message follows it.
    fn entry_is_finalized_turn_end(entries: &[AgentThreadEntry], entry_ix: usize) -> Option<bool> {
        if !matches!(
            entries.get(entry_ix),
            Some(AgentThreadEntry::AssistantMessage(_))
        ) {
            return Some(false);
        }

        for entry in &entries[entry_ix + 1..] {
            match entry {
                AgentThreadEntry::UserMessage(_) => return Some(true),
                AgentThreadEntry::AssistantMessage(_) | AgentThreadEntry::ToolCall(_) => {
                    return Some(false);
                }
                _ => {}
            }
        }

        None
    }

    fn render_thread_controls(
        &self,
        thread: &Entity<AcpThread>,
        entry_ix: usize,
        copy_response_index: Option<usize>,
        is_thread_bottom: bool,
        user_message_index: Option<usize>,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let is_generating = matches!(thread.read(cx).status(), ThreadStatus::Generating);
        let needs_confirmation = thread.read(cx).is_waiting_for_confirmation()
            || self.has_pending_request_elicitation(cx);

        if is_thread_bottom && (is_generating || needs_confirmation) {
            return Empty.into_any_element();
        }

        let copy_response_button = copy_response_index.map(|response_index| {
            IconButton::new(("copy_agent_response", entry_ix), IconName::Copy)
                .icon_size(IconSize::Small)
                .icon_color(Color::Muted)
                .tooltip(Tooltip::text("Copy This Agent Response"))
                .on_click(cx.listener(move |this, _, _, cx| {
                    let entries = this.thread.read(cx).entries();
                    if let Some(text) = Self::get_agent_message_content(entries, response_index, cx)
                    {
                        cx.write_to_clipboard(ClipboardItem::new_string(text));
                    }
                }))
        });

        let mut is_reading_response = false;
        let read_aloud_button = self.read_aloud.as_ref().zip(copy_response_index).and_then(
            |(read_aloud, response_index)| {
                let message_markdowns = Self::assistant_message_markdowns(
                    thread.read(cx).entries(),
                    response_index,
                    cx,
                );
                if message_markdowns.is_empty() {
                    return None;
                }
                let reader = read_aloud.read(cx);
                is_reading_response = reader
                    .playback_state(cx)
                    .is_some_and(|state| !state.stopped)
                    && reader
                        .speaking()
                        .is_some_and(|speaking| message_markdowns.contains(speaking));
                let (icon, tooltip) = if is_reading_response {
                    (IconName::Stop, "Stop Reading")
                } else {
                    (IconName::AudioOn, "Read Aloud")
                };
                Some(
                    IconButton::new(("read_aloud_response", entry_ix), icon)
                        .icon_size(IconSize::Small)
                        .icon_color(if is_reading_response {
                            Color::Accent
                        } else {
                            Color::Muted
                        })
                        .tooltip(Tooltip::text(tooltip))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.toggle_read_aloud_for_message(response_index, cx);
                        })),
                )
            },
        );

        let scroll_to_recent_user_prompt = IconButton::new(
            ("scroll_to_recent_user_prompt", entry_ix),
            IconName::UserArrowUp,
        )
        .icon_size(IconSize::Small)
        .icon_color(Color::Muted)
        .tooltip(Tooltip::text("Scroll to User Message"))
        .on_click(cx.listener(move |this, _, _, cx| {
            this.scroll_to_user_message_index(user_message_index, cx);
        }));

        let scroll_to_top = is_thread_bottom.then(|| {
            IconButton::new(("scroll_to_top", entry_ix), IconName::ArrowUp)
                .icon_size(IconSize::Small)
                .icon_color(Color::Muted)
                .tooltip(Tooltip::text("Scroll to Top"))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.scroll_to_top(cx);
                }))
        });

        let show_stats = is_thread_bottom && AgentSettings::get_global(cx).show_turn_stats;

        let last_turn_clock = show_stats
            .then(|| {
                self.turn_fields
                    .last_turn_duration
                    .filter(|&duration| duration > STOPWATCH_THRESHOLD)
                    .map(|duration| {
                        Label::new(duration_alt_display(duration))
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                    })
            })
            .flatten();

        let last_turn_tokens_label = last_turn_clock
            .is_some()
            .then(|| {
                self.turn_fields
                    .last_turn_tokens
                    .filter(|&tokens| tokens > TOKEN_THRESHOLD)
                    .map(|tokens| {
                        Label::new(format!("{} tokens", crate::humanize_token_count(tokens)))
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                    })
            })
            .flatten();

        let feedback_buttons = is_thread_bottom
            .then(|| {
                (self.is_subagent() && self.is_thread_feedback_enabled(cx)).then(|| {
                    let feedback = self.thread_feedback.feedback;
                    let tooltip_meta =
                        "Rating the thread sends all of your current conversation to the Zed team.";

                    h_flex()
                        .child(
                            IconButton::new("feedback-thumbs-up", IconName::ThumbsUp)
                                .icon_size(IconSize::Small)
                                .icon_color(match feedback {
                                    Some(ThreadFeedback::Positive) => Color::Accent,
                                    _ => Color::Muted,
                                })
                                .tooltip(move |window, cx| match feedback {
                                    Some(ThreadFeedback::Positive) => {
                                        Tooltip::text("Thanks for your feedback!")(window, cx)
                                    }
                                    _ => Tooltip::with_meta(
                                        "Helpful Response",
                                        None,
                                        tooltip_meta,
                                        cx,
                                    ),
                                })
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.handle_feedback_click(ThreadFeedback::Positive, window, cx);
                                })),
                        )
                        .child(
                            IconButton::new("feedback-thumbs-down", IconName::ThumbsDown)
                                .icon_size(IconSize::Small)
                                .icon_color(match feedback {
                                    Some(ThreadFeedback::Negative) => Color::Accent,
                                    _ => Color::Muted,
                                })
                                .tooltip(move |window, cx| match feedback {
                                    Some(ThreadFeedback::Negative) => Tooltip::text(
                                        "We appreciate your feedback and will use it to improve in the future.",
                                    )(
                                        window, cx
                                    ),
                                    _ => Tooltip::with_meta(
                                        "Not Helpful Response",
                                        None,
                                        tooltip_meta,
                                        cx,
                                    ),
                                })
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.handle_feedback_click(ThreadFeedback::Negative, window, cx);
                                })),
                        )
                })
            })
            .flatten();

        let separator_dots = || {
            Label::new("•")
                .size(LabelSize::Small)
                .color(Color::Muted)
                .alpha(0.5)
        };

        h_flex()
            .w_full()
            .py_1p5()
            .px_4()
            .justify_end()
            // While this message is being read aloud, its speaker button must
            // be findable without hovering — the whole row stays revealed.
            .opacity(if is_reading_response { 1. } else { 0.4 })
            .hover(|s| s.opacity(1.))
            .when(
                last_turn_tokens_label.is_some() || last_turn_clock.is_some(),
                |this| {
                    this.child(
                        h_flex()
                            .px_1()
                            .gap_1()
                            .when_some(last_turn_tokens_label, |this, label| {
                                this.child(label).child(separator_dots())
                            })
                            .when_some(last_turn_clock, |this, label| {
                                this.child(label).child(separator_dots())
                            }),
                    )
                },
            )
            .when_some(feedback_buttons, |this, buttons| this.child(buttons))
            .when_some(read_aloud_button, |this, button| this.child(button))
            .when_some(copy_response_button, |this, button| this.child(button))
            .child(scroll_to_recent_user_prompt)
            .when_some(scroll_to_top, |this, button| this.child(button))
            .into_any_element()
    }

    fn is_thread_feedback_enabled(&self, cx: &App) -> bool {
        util::maybe!({
            let project = self.thread.read(cx).project().read(cx);
            let user_store = project.user_store();
            if let Some(configuration) = user_store.read(cx).current_organization_configuration() {
                if !configuration.is_agent_thread_feedback_enabled {
                    return false;
                }
            }

            AgentSettings::get_global(cx).enable_feedback
                && self.thread.read(cx).connection().telemetry().is_some()
        })
    }

    // The local slash commands the message editor should currently expose.
    // Kept in sync with the availability of the corresponding actions via
    // `sync_local_commands`.
    fn available_local_commands(&self, cx: &App) -> Vec<PromptLocalCommand> {
        let mut commands = Vec::new();

        if self.is_thread_feedback_enabled(cx) {
            commands.push(PromptLocalCommand::ThumbsUp);
            commands.push(PromptLocalCommand::ThumbsDown);
        }

        commands
    }

    // Pushes the current set of available local commands to the message
    // editor so they appear in its slash-command popup.
    pub(crate) fn sync_local_commands(&self, cx: &App) {
        let commands = self.available_local_commands(cx);
        self.message_editor.read(cx).set_local_commands(commands);
    }

    fn render_request_elicitations(&self, cx: &Context<Self>) -> Vec<AnyElement> {
        let server_view = self.server_view.clone();
        let handlers_view = server_view.clone();
        server_view
            .read_with(cx, |server_view, cx| {
                let Some(connection) = server_view.request_elicitation_connection() else {
                    return Vec::new();
                };
                server_view.render_request_elicitations(&connection, handlers_view, cx)
            })
            .unwrap_or_default()
    }

    pub(crate) fn scroll_to_user_message_index(
        &mut self,
        user_message_index: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        let entries = self.thread.read(cx).entries();
        if entries.is_empty() {
            return;
        }

        // Scroll to the provided user message, or fall back to the most recent one.
        // (Fallback: if no user message exists, scroll to the bottom.)
        if let Some(ix) = user_message_index.or_else(|| {
            entries
                .iter()
                .rposition(|entry| matches!(entry, AgentThreadEntry::UserMessage(_)))
        }) {
            self.list_state.scroll_to(ListOffset {
                item_ix: ix,
                offset_in_item: px(0.0),
            });
            cx.notify();
        } else {
            self.scroll_to_end(cx);
        }
    }

    pub fn scroll_to_end(&mut self, cx: &mut Context<Self>) {
        self.list_state.scroll_to_end();
        cx.notify();
    }

    fn handle_feedback_click(
        &mut self,
        feedback: ThreadFeedback,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.thread_feedback
            .submit(self.thread.clone(), feedback, window, cx);
        cx.notify();
    }

    fn submit_feedback_message(&mut self, cx: &mut Context<Self>) {
        let thread = self.thread.clone();
        self.thread_feedback.submit_comments(thread, cx);
        cx.notify();
    }

    pub(crate) fn scroll_to_top(&mut self, cx: &mut Context<Self>) {
        self.list_state.scroll_to(ListOffset::default());
        cx.notify();
    }

    fn scroll_output_page_up(
        &mut self,
        _: &ScrollOutputPageUp,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let page_height = self.list_state.viewport_bounds().size.height;
        self.list_state.scroll_by(-page_height * 0.9);
        cx.notify();
    }

    fn scroll_output_page_down(
        &mut self,
        _: &ScrollOutputPageDown,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let page_height = self.list_state.viewport_bounds().size.height;
        self.list_state.scroll_by(page_height * 0.9);
        cx.notify();
    }

    fn scroll_output_line_up(
        &mut self,
        _: &ScrollOutputLineUp,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.list_state.scroll_by(-window.line_height() * 3.);
        cx.notify();
    }

    fn scroll_output_line_down(
        &mut self,
        _: &ScrollOutputLineDown,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.list_state.scroll_by(window.line_height() * 3.);
        cx.notify();
    }

    fn scroll_output_to_top(
        &mut self,
        _: &ScrollOutputToTop,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.scroll_to_top(cx);
    }

    fn scroll_output_to_bottom(
        &mut self,
        _: &ScrollOutputToBottom,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.scroll_to_end(cx);
    }

    fn scroll_output_to_previous_message(
        &mut self,
        _: &ScrollOutputToPreviousMessage,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let entries = self.thread.read(cx).entries();
        let current_ix = self.list_state.logical_scroll_top().item_ix;
        if let Some(target_ix) = (0..current_ix)
            .rev()
            .find(|&i| matches!(entries.get(i), Some(AgentThreadEntry::UserMessage(_))))
        {
            self.list_state.scroll_to(ListOffset {
                item_ix: target_ix,
                offset_in_item: px(0.),
            });
            cx.notify();
        }
    }

    fn scroll_output_to_next_message(
        &mut self,
        _: &ScrollOutputToNextMessage,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let entries = self.thread.read(cx).entries();
        let current_ix = self.list_state.logical_scroll_top().item_ix;
        if let Some(target_ix) = (current_ix + 1..entries.len())
            .find(|&i| matches!(entries.get(i), Some(AgentThreadEntry::UserMessage(_))))
        {
            self.list_state.scroll_to(ListOffset {
                item_ix: target_ix,
                offset_in_item: px(0.),
            });
            cx.notify();
        }
    }

    fn refresh_thread_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.thread_search_visible {
            return;
        }
        if let Some(bar) = self.thread_search_bar.clone() {
            bar.update(cx, |bar, cx| bar.update_matches(window, cx));
        }
    }

    /// Hides the thread search bar, clears its highlights, and returns focus to
    /// the message editor. Returns `true` if the search bar was visible.
    pub(crate) fn close_thread_search(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.thread_search_visible {
            return false;
        }

        if let Some(bar) = self.thread_search_bar.clone() {
            bar.update(cx, |bar, cx| bar.clear_highlights(cx));
        }

        self.thread_search_visible = false;
        self.message_editor.focus_handle(cx).focus(window, cx);
        cx.notify();
        true
    }

    pub(crate) fn toggle_search(
        &mut self,
        _: &crate::ToggleSearch,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.thread_search_bar.is_none() {
            let thread = self.thread.clone();
            let view = cx.entity().downgrade();
            let on_activate =
                Arc::new(move |entry_ix: usize, _window: &mut Window, cx: &mut App| {
                    // Avoid re-entering `ThreadView` when search navigation is forwarded
                    // from a `ThreadView` action handler.
                    let view = view.clone();
                    cx.defer(move |cx| {
                        view.update(cx, |this, cx| {
                            this.list_state.scroll_to(gpui::ListOffset {
                                item_ix: entry_ix,
                                offset_in_item: gpui::px(0.),
                            });
                            cx.notify();
                        })
                        .ok();
                    });
                });
            let search_bar = cx.new(|cx| {
                ThreadSearchBar::new(
                    thread,
                    self.entry_view_state.clone(),
                    on_activate,
                    window,
                    cx,
                )
            });
            self._subscriptions.push(cx.subscribe_in(
                &search_bar,
                window,
                |this, _bar, event, window, cx| {
                    if matches!(event, ThreadSearchBarEvent::Dismissed) {
                        this.thread_search_visible = false;
                        this.message_editor.focus_handle(cx).focus(window, cx);
                        cx.notify();
                    }
                },
            ));
            self.thread_search_bar = Some(search_bar);
        }

        // Re-focus an open bar unless it already owns focus.
        let search_bar_focused = self
            .thread_search_bar
            .as_ref()
            .is_some_and(|bar| bar.focus_handle(cx).contains_focused(window, cx));

        if self.thread_search_visible && search_bar_focused {
            if let Some(bar) = &self.thread_search_bar {
                bar.update(cx, |bar, cx| bar.clear_highlights(cx));
            }
            self.thread_search_visible = false;
            self.message_editor.focus_handle(cx).focus(window, cx);
            cx.notify();
        } else {
            self.thread_search_visible = true;
            if let Some(bar) = self.thread_search_bar.clone() {
                bar.update(cx, |bar, cx| bar.focus_and_refresh(window, cx));
            }
            cx.notify();
        }
    }

    pub fn open_thread_as_markdown(
        &self,
        workspace: Entity<Workspace>,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<()>> {
        let thread = self.thread.read(cx);
        let thread_title = thread
            .title()
            .unwrap_or_else(|| DEFAULT_THREAD_TITLE.into())
            .to_string();
        let markdown = thread.to_markdown(cx);

        open_markdown_in_workspace(thread_title, markdown, workspace, window, cx)
    }

    pub(crate) fn sync_editor_mode(&mut self, cx: &mut Context<Self>) {
        let has_messages = self.list_state.item_count() > 0;
        let v2_empty_state = !has_messages;

        if !has_messages {
            self.editor_expanded = false;
        }

        let mode = if self.editor_expanded {
            EditorMode::Full {
                scale_ui_elements_with_buffer_font_size: false,
                show_active_line_background: false,
                sizing_behavior: SizingBehavior::ExcludeOverscrollMargin,
            }
        } else if v2_empty_state {
            EditorMode::Full {
                scale_ui_elements_with_buffer_font_size: false,
                show_active_line_background: false,
                sizing_behavior: SizingBehavior::Default,
            }
        } else {
            EditorMode::AutoHeight {
                min_lines: AgentSettings::get_global(cx).message_editor_min_lines,
                max_lines: Some(AgentSettings::get_global(cx).set_message_editor_max_lines()),
            }
        };
        self.message_editor.update(cx, |editor, cx| {
            editor.set_mode(mode, cx);
        });
    }

    /// Ensures the list item count includes (or excludes) an extra item for the generating indicator
    pub(crate) fn sync_generating_indicator(&mut self, cx: &App) {
        let thread = self.thread.read(cx);

        let is_generating =
            matches!(thread.status(), ThreadStatus::Generating) && !thread.is_compacting();

        if is_generating && !self.generating_indicator_in_list {
            let entries_count = self.thread.read(cx).entries().len();
            self.list_state.splice(entries_count..entries_count, 1);
            self.generating_indicator_in_list = true;
        } else if !is_generating && self.generating_indicator_in_list {
            let entries_count = self.thread.read(cx).entries().len();
            self.list_state.splice(entries_count..entries_count + 1, 0);
            self.generating_indicator_in_list = false;
        }
    }

    fn render_generating(&self, confirmation: bool, cx: &App) -> impl IntoElement {
        let show_stats = AgentSettings::get_global(cx).show_turn_stats;
        let elapsed_label = show_stats
            .then(|| {
                self.turn_fields.turn_started_at.and_then(|started_at| {
                    let elapsed = started_at.elapsed();
                    (elapsed > STOPWATCH_THRESHOLD).then(|| duration_alt_display(elapsed))
                })
            })
            .flatten();

        let is_blocked_on_terminal_command =
            !confirmation && self.is_blocked_on_terminal_command(cx);
        let is_waiting = confirmation || self.thread.read(cx).has_in_progress_tool_calls();

        let turn_tokens_label = elapsed_label
            .is_some()
            .then(|| {
                self.turn_fields
                    .turn_tokens
                    .filter(|&tokens| tokens > TOKEN_THRESHOLD)
                    .map(|tokens| crate::humanize_token_count(tokens))
            })
            .flatten();

        let arrow_icon = if is_waiting {
            IconName::ArrowUp
        } else {
            IconName::ArrowDown
        };

        h_flex()
            .id("generating-spinner")
            .py_2()
            .px(rems_from_px(22_f32))
            .gap_2()
            .map(|this| {
                if confirmation {
                    this.child(
                        h_flex()
                            .w_2()
                            .justify_center()
                            .child(GeneratingSpinnerElement::new(SpinnerVariant::Sand)),
                    )
                    .child(
                        div().min_w(rems(8.)).child(
                            LoadingLabel::new("Awaiting Confirmation")
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        ),
                    )
                } else if is_blocked_on_terminal_command {
                    this
                } else {
                    this.child(
                        h_flex()
                            .w_2()
                            .justify_center()
                            .child(GeneratingSpinnerElement::new(SpinnerVariant::Dots)),
                    )
                }
            })
            .when_some(elapsed_label, |this, elapsed| {
                this.child(
                    Label::new(elapsed)
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            })
            .when_some(turn_tokens_label, |this, tokens| {
                this.child(
                    h_flex()
                        .gap_0p5()
                        .child(
                            Icon::new(arrow_icon)
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                        )
                        .child(
                            Label::new(format!("{} tokens", tokens))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        ),
                )
            })
            .into_any_element()
    }

    pub(crate) fn auto_expand_streaming_thought(&mut self, cx: &mut Context<Self>) {
        let thread = self.thread.clone();
        let changed = self.entry_view_state.update(cx, |state, cx| {
            let thread = thread.read(cx);
            if thread.status() != ThreadStatus::Generating {
                return false;
            }
            state.auto_expand_streaming_thought(thread, cx)
        });
        if changed {
            cx.notify();
        }
    }

    pub(crate) fn clear_auto_expand_tracking(&mut self, cx: &mut Context<Self>) {
        self.entry_view_state.update(cx, |state, _cx| {
            state.clear_auto_expand_tracking();
        });
    }

    fn toggle_thinking_block_expansion(
        &mut self,
        key: (usize, usize),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.entry_view_state.update(cx, |state, cx| {
            state.toggle_thinking_block_expansion(key, cx);
        });
        self.refresh_thread_search(window, cx);
        cx.notify();
    }

    fn render_thinking_block(
        &self,
        entry_ix: usize,
        chunk_ix: usize,
        chunk: Entity<Markdown>,
        window: &Window,
        cx: &Context<Self>,
    ) -> AnyElement {
        let header_id = SharedString::from(format!("thinking-block-header-{}", entry_ix));
        let card_header_id = SharedString::from("inner-card-header");

        let key = (entry_ix, chunk_ix);

        let entry_view_state = self.entry_view_state.read(cx);
        let (is_open, is_constrained) = entry_view_state.thinking_block_state(key, cx);
        let should_auto_scroll = entry_view_state.is_auto_expanded_thinking_block(key);
        let scroll_handle = entry_view_state
            .entry(entry_ix)
            .and_then(|entry| entry.scroll_handle_for_assistant_message_chunk(chunk_ix));

        if should_auto_scroll {
            if let Some(ref handle) = scroll_handle {
                handle.scroll_to_bottom();
            }
        }

        let panel_bg = cx.theme().colors().panel_background;

        v_flex()
            .gap_1()
            .child(
                h_flex()
                    .id(header_id)
                    .group(&card_header_id)
                    .relative()
                    .w_full()
                    .pr_1()
                    .justify_between()
                    .child(
                        h_flex()
                            .h(window.line_height() - px(2.))
                            .gap_1p5()
                            .overflow_hidden()
                            .child(
                                Icon::new(IconName::ToolThink)
                                    .size(IconSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(
                                div()
                                    .text_size(self.tool_name_font_size())
                                    .text_color(cx.theme().colors().text_muted)
                                    .child("Thinking"),
                            ),
                    )
                    .child(
                        Disclosure::new(("expand", entry_ix), is_open)
                            .opened_icon(IconName::ChevronUp)
                            .closed_icon(IconName::ChevronDown)
                            .visible_on_hover(&card_header_id)
                            .on_click(cx.listener(move |this, _event: &ClickEvent, window, cx| {
                                this.toggle_thinking_block_expansion(key, window, cx);
                            })),
                    )
                    .on_click(cx.listener(move |this, _event: &ClickEvent, window, cx| {
                        this.toggle_thinking_block_expansion(key, window, cx);
                    })),
            )
            .when(is_open, |this| {
                this.child(
                    div()
                        .when(is_constrained, |this| this.relative())
                        .child(
                            div()
                                .id(("thinking-content", chunk_ix))
                                .ml_1p5()
                                .pl_3p5()
                                .border_l_1()
                                .border_color(self.tool_card_border_color(cx))
                                .when(is_constrained, |this| this.max_h_64())
                                .when_some(scroll_handle, |this, scroll_handle| {
                                    this.track_scroll(&scroll_handle)
                                })
                                .overflow_hidden()
                                .child(self.render_markdown(
                                    chunk,
                                    {
                                        let mut style =
                                            MarkdownStyle::themed(MarkdownFont::Agent, window, cx);
                                        self.agent_panel_styling
                                            .thinking
                                            .apply_to_markdown_style(&mut style);
                                        style
                                    },
                                    cx,
                                )),
                        )
                        .when(is_constrained, |this| {
                            this.child(
                                div()
                                    .absolute()
                                    .inset_0()
                                    .size_full()
                                    .bg(linear_gradient(
                                        180.,
                                        linear_color_stop(panel_bg.opacity(0.8), 0.),
                                        linear_color_stop(panel_bg.opacity(0.), 0.1),
                                    ))
                                    .block_mouse_except_scroll(),
                            )
                        }),
                )
            })
            .into_any_element()
    }

    fn render_message_context_menu(
        &self,
        entry_ix: usize,
        message_body: AnyElement,
        cx: &Context<Self>,
    ) -> AnyElement {
        let entity = cx.entity();
        let workspace = self.workspace.clone();

        right_click_menu(format!("agent_context_menu-{}", entry_ix))
            .trigger(move |_, _, _| message_body)
            .menu(move |window, cx| {
                let focus = window.focused(cx);
                let entity = entity.clone();
                let workspace = workspace.clone();

                ContextMenu::build(window, cx, move |menu, _, cx| {
                    let this = entity.read(cx);
                    let is_at_top = this.list_state.logical_scroll_top().item_ix == 0;

                    let chunks =
                        this.thread.read(cx).entries().get(entry_ix).and_then(
                            |entry| match &entry {
                                AgentThreadEntry::AssistantMessage(msg) => Some(&msg.chunks),
                                _ => None,
                            },
                        );

                    let has_selection = chunks
                        .map(|chunks| {
                            chunks.iter().any(|chunk| {
                                let md = match chunk {
                                    AssistantMessageChunk::Message { block, .. } => {
                                        block.markdown()
                                    }
                                    AssistantMessageChunk::Thought { block, .. } => {
                                        block.markdown()
                                    }
                                };
                                md.map_or(false, |m| m.read(cx).has_selection())
                            })
                        })
                        .unwrap_or(false);

                    let context_menu_link = chunks.and_then(|chunks| {
                        chunks.iter().find_map(|chunk| {
                            let md = match chunk {
                                AssistantMessageChunk::Message { block, .. } => block.markdown(),
                                AssistantMessageChunk::Thought { block, .. } => block.markdown(),
                            };
                            md.and_then(|m| m.read(cx).context_menu_link().cloned())
                        })
                    });

                    let copy_this_agent_response =
                        ContextMenuEntry::new("Copy This Agent Response").handler({
                            let entity = entity.clone();
                            move |_, cx| {
                                entity.update(cx, |this, cx| {
                                    let entries = this.thread.read(cx).entries();
                                    if let Some(text) =
                                        Self::get_agent_message_content(entries, entry_ix, cx)
                                    {
                                        cx.write_to_clipboard(ClipboardItem::new_string(text));
                                    }
                                });
                            }
                        });

                    let scroll_item = if is_at_top {
                        ContextMenuEntry::new("Scroll to Bottom").handler({
                            let entity = entity.clone();
                            move |_, cx| {
                                entity.update(cx, |this, cx| {
                                    this.scroll_to_end(cx);
                                });
                            }
                        })
                    } else {
                        ContextMenuEntry::new("Scroll to Top").handler({
                            let entity = entity.clone();
                            move |_, cx| {
                                entity.update(cx, |this, cx| {
                                    this.scroll_to_top(cx);
                                });
                            }
                        })
                    };

                    let open_thread_as_markdown = ContextMenuEntry::new("Open Thread as Markdown")
                        .handler({
                            let entity = entity.clone();
                            let workspace = workspace.clone();
                            move |window, cx| {
                                if let Some(workspace) = workspace.upgrade() {
                                    entity
                                        .update(cx, |this, cx| {
                                            this.open_thread_as_markdown(workspace, window, cx)
                                        })
                                        .detach_and_log_err(cx);
                                }
                            }
                        });

                    menu.when_some(focus, |menu, focus| menu.context(focus))
                        .when_some(context_menu_link, |menu, url| {
                            menu.entry("Copy Link", None, move |_, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(url.to_string()));
                            })
                            .separator()
                        })
                        .action_disabled_when(
                            !has_selection,
                            "Copy Selection",
                            Box::new(markdown::CopyAsMarkdown),
                        )
                        .item(copy_this_agent_response)
                        .separator()
                        .item(scroll_item)
                        .item(open_thread_as_markdown)
                })
            })
            .into_any_element()
    }

    fn get_agent_message_content(
        entries: &[AgentThreadEntry],
        entry_index: usize,
        cx: &App,
    ) -> Option<String> {
        let entry = entries.get(entry_index)?;
        if matches!(entry, AgentThreadEntry::UserMessage(_)) {
            return None;
        }

        let start_index = (0..entry_index)
            .rev()
            .find(|&i| matches!(entries.get(i), Some(AgentThreadEntry::UserMessage(_))))
            .map(|i| i + 1)
            .unwrap_or(0);

        let end_index = (entry_index + 1..entries.len())
            .find(|&i| matches!(entries.get(i), Some(AgentThreadEntry::UserMessage(_))))
            .map(|i| i - 1)
            .unwrap_or(entries.len() - 1);

        let parts: Vec<String> = (start_index..=end_index)
            .filter_map(|i| entries.get(i))
            .filter_map(|entry| {
                if let AgentThreadEntry::AssistantMessage(message) = entry {
                    let text: String = message
                        .chunks
                        .iter()
                        .filter_map(|chunk| match chunk {
                            AssistantMessageChunk::Message { block, .. } => {
                                let markdown = block.to_markdown(cx);
                                if markdown.trim().is_empty() {
                                    None
                                } else {
                                    Some(markdown.to_string())
                                }
                            }
                            AssistantMessageChunk::Thought { .. } => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n\n");

                    if text.is_empty() { None } else { Some(text) }
                } else {
                    None
                }
            })
            .collect();

        let text = parts.join("\n\n");
        if text.is_empty() { None } else { Some(text) }
    }

    fn is_blocked_on_terminal_command(&self, cx: &App) -> bool {
        let thread = self.thread.read(cx);
        if !matches!(thread.status(), ThreadStatus::Generating) {
            return false;
        }

        let mut has_running_terminal_call = false;

        for entry in thread.entries().iter().rev() {
            match entry {
                AgentThreadEntry::UserMessage(_) => break,
                AgentThreadEntry::ToolCall(tool_call)
                    if matches!(
                        tool_call.status,
                        ToolCallStatus::InProgress | ToolCallStatus::Pending
                    ) =>
                {
                    if matches!(tool_call.kind, acp::ToolKind::Execute) {
                        has_running_terminal_call = true;
                    } else {
                        return false;
                    }
                }
                AgentThreadEntry::ToolCall(_)
                | AgentThreadEntry::Elicitation(_)
                | AgentThreadEntry::AssistantMessage(_)
                | AgentThreadEntry::CompletedPlan(_)
                | AgentThreadEntry::ContextCompaction(_) => {}
            }
        }

        has_running_terminal_call
    }

    fn render_collapsible_command(
        &self,
        group: SharedString,
        is_preview: bool,
        command: Entity<Markdown>,
        window: &Window,
        cx: &Context<Self>,
    ) -> Div {
        // The label's markdown source is a fenced code block (```\n...\n```);
        // strip the fences so the copy button yields just the command text.
        let command_source = command.read(cx).source();
        let command_text = command_source
            .strip_prefix("```\n")
            .and_then(|s| s.strip_suffix("\n```"))
            .unwrap_or(&command_source)
            .to_string();

        let mut style =
            MarkdownStyle::themed(MarkdownFont::Agent, window, cx).with_agent_buffer_font(cx);
        style.container_style.text.font_size = Some(rems_from_px(12_f32).into());
        style.container_style.text.line_height = Some(rems_from_px(17_f32).into());
        style.height_is_multiple_of_line_height = true;
        // The command renders as a fenced code block, so the tool-output
        // overrides must land on the code-block refinement to take effect.
        self.agent_panel_styling
            .tool_output
            .apply_text_to_markdown_style(&mut style);
        self.agent_panel_styling
            .tool_output
            .apply_to_code_blocks(&mut style);
        // Soft-wrap the command instead of horizontally scrolling it: the card is
        // narrow, and in scroll mode a long command wraps anyway but its wrapped
        // lines don't pick up the code block's left padding. Wrap mode lays the
        // text out as a normal block inside the padded content box, so every
        // line (wrapped or not) is padded consistently.
        style.code_block_overflow_x_scroll = false;

        let header_bg = self.tool_card_header_bg(cx);
        let run_command_label = if is_preview {
            Some(
                h_flex().h_6().child(
                    Label::new("Run Command")
                        .buffer_font(cx)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                ),
            )
        } else {
            None
        };
        // Suppress the code block's built-in copy button so we don't stack two
        // copy buttons on top of each other; the outer button below is the one
        // we want, because it copies the unfenced command text.
        let markdown_element = self
            .render_markdown(command, style, cx)
            .code_block_renderer(CodeBlockRenderer::Default {
                copy_button_visibility: CopyButtonVisibility::Hidden,
                wrap_button_visibility: markdown::WrapButtonVisibility::Hidden,
                border: false,
            });
        let copy_button_id = SharedString::from(format!("{group}-copy-command"));
        let copy_button = CopyButton::new(copy_button_id, command_text)
            .tooltip_label("Copy Command")
            .visible_on_hover(group.clone());

        v_flex()
            .group(group)
            .relative()
            .p_1p5()
            .bg(header_bg)
            .when(is_preview, |this| this.pt_1().children(run_command_label))
            .child(markdown_element)
            .child(div().absolute().top_1().right_1().child(copy_button))
    }

    fn render_terminal_tool_call(
        &self,
        active_session_id: &acp::SessionId,
        entry_ix: usize,
        terminal: &Entity<acp_thread::Terminal>,
        tool_call: &ToolCall,
        focus_handle: &FocusHandle,
        layout: ToolCallLayout,
        window: &Window,
        cx: &Context<Self>,
    ) -> AnyElement {
        let terminal_data = terminal.read(cx);
        let working_dir = terminal_data.working_dir();
        let started_at = terminal_data.started_at();

        let tool_failed = matches!(
            &tool_call.status,
            ToolCallStatus::Rejected | ToolCallStatus::Canceled | ToolCallStatus::Failed
        );

        let confirmation_options = match &tool_call.status {
            ToolCallStatus::WaitingForConfirmation { options, .. } => Some(options),
            _ => None,
        };
        let needs_confirmation = confirmation_options.is_some();

        let output = terminal_data.output();
        let command_finished = output.is_some()
            && !matches!(
                tool_call.status,
                ToolCallStatus::InProgress | ToolCallStatus::Pending
            );
        let truncated_output =
            output.is_some_and(|output| output.original_content_len > output.content.len());
        let output_line_count = output.map(|output| output.content_line_count).unwrap_or(0);

        let command_failed = command_finished
            && output.is_some_and(|o| o.exit_status.is_some_and(|status| !status.success()));

        let time_elapsed = if let Some(output) = output {
            output.ended_at.duration_since(started_at)
        } else {
            started_at.elapsed()
        };

        let header_group = SharedString::from(format!(
            "terminal-tool-header-group-{}",
            terminal.entity_id()
        ));
        let border_color = cx.theme().colors().border.opacity(0.6);

        let working_dir = working_dir
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "current directory".to_string());

        let command_element = self.render_collapsible_command(
            header_group.clone(),
            false,
            tool_call.label.clone(),
            window,
            cx,
        );

        let is_expanded = self
            .entry_view_state
            .read(cx)
            .is_tool_call_expanded(&tool_call.id);

        let truncated_tooltip = truncated_output.then(|| {
            if let Some(output) = output {
                if output_line_count + 10 > terminal::MAX_SCROLL_HISTORY_LINES {
                    format!(
                        "Output exceeded terminal max lines and was \
                         truncated, the model received the first {}.",
                        format_file_size(output.content.len() as u64, true)
                    )
                } else {
                    format!(
                        "Output is {} long, and to avoid unexpected token usage, \
                         only {} was sent back to the agent.",
                        format_file_size(output.original_content_len as u64, true),
                        format_file_size(output.content.len() as u64, true)
                    )
                }
            } else {
                "Output was truncated".to_string()
            }
        });

        let header = TerminalToolHeader::new(
            terminal.entity_id().to_string(),
            header_group,
            working_dir,
            is_expanded,
        )
        .elapsed(time_elapsed)
        .running(!command_finished && !needs_confirmation)
        .on_toggle_expand(cx.listener({
            let id = tool_call.id.clone();
            move |this, _event, window, cx| {
                this.entry_view_state.update(cx, |state, _cx| {
                    state.toggle_tool_call_expansion(&id);
                });
                this.refresh_thread_search(window, cx);
                cx.notify();
            }
        }))
        .on_stop({
            let terminal = terminal.clone();
            cx.listener(move |this, _event, _window, cx| {
                terminal.update(cx, |terminal, cx| {
                    terminal.stop_by_user(cx);
                });
                if AgentSettings::get_global(cx).cancel_generation_on_terminal_stop {
                    this.cancel_generation(cx);
                }
            })
        })
        .when_some(truncated_tooltip, |header, tooltip| {
            header.truncated(tooltip)
        })
        .when(tool_failed || command_failed, |header| {
            header.failed(
                output
                    .and_then(|o| o.exit_status)
                    .map(|status| status.code().unwrap_or(-1)),
            )
        })
        .when_some(tool_call.sandbox_not_applied.as_ref(), |header, reason| {
            header.sandbox_warning(self.sandbox_not_applied_warning(reason, cx))
        })
        .command_slot(command_element);

        let terminal_view = self
            .entry_view_state
            .read(cx)
            .entry(entry_ix)
            .and_then(|entry| entry.terminal(terminal));

        v_flex()
            .when(layout == ToolCallLayout::Standalone, |this| {
                this.my_1p5()
                    .mx_5()
                    .border_1()
                    .when(tool_failed || command_failed, |card| card.border_dashed())
                    .border_color(border_color)
                    .rounded_md()
            })
            .overflow_hidden()
            .child(header)
            .when(is_expanded && terminal_view.is_some(), |this| {
                this.child(
                    div()
                        .pt_2()
                        .border_t_1()
                        .when(tool_failed || command_failed, |card| card.border_dashed())
                        .border_color(border_color)
                        .bg(cx.theme().colors().editor_background)
                        .rounded_b_md()
                        .text_ui_sm(cx)
                        .h_full()
                        .children(terminal_view.map(|terminal_view| {
                            let element = if terminal_view
                                .read(cx)
                                .content_mode(window, cx)
                                .is_scrollable()
                            {
                                div().h_72().child(terminal_view).into_any_element()
                            } else {
                                terminal_view.into_any_element()
                            };

                            div()
                                .on_action(cx.listener(|_this, _: &NewTerminal, window, cx| {
                                    window.dispatch_action(NewThread.boxed_clone(), cx);
                                    cx.stop_propagation();
                                }))
                                .child(element)
                                .into_any_element()
                        })),
                )
            })
            .when_some(confirmation_options, |this, options| {
                let is_first = self.is_first_tool_call(active_session_id, &tool_call.id, cx);
                let allow_disabled = self.sandbox_confusables_block_allow(tool_call, cx);
                this.child(self.render_permission_buttons(
                    self.thread.read(cx).session_id().clone(),
                    is_first,
                    options,
                    entry_ix,
                    tool_call.id.clone(),
                    focus_handle,
                    allow_disabled,
                    cx,
                ))
            })
            .into_any()
    }

    fn sandbox_not_applied_warning(
        &self,
        reason: &SandboxNotAppliedReason,
        cx: &Context<Self>,
    ) -> TerminalSandboxWarning {
        // (title, detail line, docs section slug)
        let (title, detail, docs_section): (SharedString, SharedString, Option<&'static str>) =
            match reason {
                SandboxNotAppliedReason::ErrorLinuxWsl(error) => (
                    "Couldn't create a sandbox".into(),
                    error.user_facing_message().into(),
                    Some(error.docs_section()),
                ),
                SandboxNotAppliedReason::DisabledForThisThread => {
                    // The grant only exists because an earlier command failed to
                    // create a sandbox; surface that same explanation here.
                    let thread_error = self.find_thread_sandbox_error(cx);
                    let detail = thread_error
                        .as_ref()
                        .map(|error| {
                            SharedString::from(format!(
                                "Allowed for this thread after the sandbox failed: {}",
                                error.user_facing_message()
                            ))
                        })
                        .unwrap_or_else(|| {
                            "Unsandboxed execution is allowed for the rest of this thread.".into()
                        });
                    let docs_section = thread_error.as_ref().map(|error| error.docs_section());
                    ("Ran without sandbox".into(), detail, docs_section)
                }
            };

        TerminalSandboxWarning {
            title,
            detail,
            docs_url: zed_urls::sandboxing_docs(docs_section, cx).into(),
        }
    }

    /// Find the first terminal tool call in the thread whose sandbox couldn't be
    /// created, so a later "disabled for this thread" warning can reuse the same
    /// explanation of *why* the sandbox failed.
    fn find_thread_sandbox_error(&self, cx: &App) -> Option<acp_thread::LinuxWslSandboxError> {
        self.thread.read(cx).entries().iter().find_map(|entry| {
            if let AgentThreadEntry::ToolCall(tool_call) = entry
                && let Some(SandboxNotAppliedReason::ErrorLinuxWsl(error)) =
                    &tool_call.sandbox_not_applied
            {
                return Some(error.clone());
            }
            None
        })
    }

    fn is_first_tool_call(
        &self,
        active_session_id: &acp::SessionId,
        tool_call_id: &acp::ToolCallId,
        cx: &App,
    ) -> bool {
        self.conversation
            .read(cx)
            .pending_tool_call(active_session_id, cx)
            .map_or(false, |(pending_session_id, pending_tool_call_id, _)| {
                self.thread.read(cx).session_id() == &pending_session_id
                    && tool_call_id == &pending_tool_call_id
            })
    }

    fn render_any_tool_call(
        &self,
        active_session_id: &acp::SessionId,
        entry_ix: usize,
        tool_call: &ToolCall,
        focus_handle: &FocusHandle,
        layout: ToolCallLayout,
        window: &Window,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let has_terminals = tool_call.terminals().next().is_some();

        // Give every tool-call subtree a unique element-id prefix derived from
        // the globally-unique tool call id and the layout. This single wrapper
        // is what keeps all the `entry_ix`-keyed element ids inside the card
        // collision-free, even when the same tool call is rendered in multiple
        // places at once (inline list + floating awaiting-permission row) or
        // when subagent entries are inlined into the parent view's element tree.
        let container_id = ElementId::Name(SharedString::from(format!(
            "tool-call-{}-{}",
            tool_call.id.0,
            layout.id_str()
        )));

        div().w_full().id(container_id).map(|this| {
            if tool_call.is_subagent() {
                this.child(
                    self.render_subagent_tool_call(
                        active_session_id,
                        entry_ix,
                        tool_call,
                        tool_call
                            .subagent_session_info
                            .as_ref()
                            .map(|i| i.session_id.clone()),
                        focus_handle,
                        window,
                        cx,
                    ),
                )
            } else if has_terminals {
                this.children(tool_call.terminals().map(|terminal| {
                    self.render_terminal_tool_call(
                        active_session_id,
                        entry_ix,
                        terminal,
                        tool_call,
                        focus_handle,
                        layout,
                        window,
                        cx,
                    )
                }))
            } else {
                this.child(self.render_tool_call(
                    active_session_id,
                    entry_ix,
                    tool_call,
                    focus_handle,
                    layout,
                    window,
                    cx,
                ))
            }
        })
    }

    fn render_tool_call(
        &self,
        active_session_id: &acp::SessionId,
        entry_ix: usize,
        tool_call: &ToolCall,
        focus_handle: &FocusHandle,
        layout: ToolCallLayout,
        window: &Window,
        cx: &Context<Self>,
    ) -> Div {
        let has_location = tool_call.locations.len() == 1;
        let card_header_id = SharedString::from(format!("inner-tool-call-header-{entry_ix}"));

        let failed_or_canceled = match &tool_call.status {
            ToolCallStatus::Rejected | ToolCallStatus::Canceled | ToolCallStatus::Failed => true,
            _ => false,
        };

        let needs_confirmation = matches!(
            tool_call.status,
            ToolCallStatus::WaitingForConfirmation { .. }
        );
        let is_terminal_tool = matches!(tool_call.kind, acp::ToolKind::Execute);

        let is_edit =
            matches!(tool_call.kind, acp::ToolKind::Edit) || tool_call.diffs().next().is_some();

        let is_cancelled_edit = is_edit && matches!(tool_call.status, ToolCallStatus::Canceled);
        let (has_revealed_diff, tool_call_output_focus, tool_call_output_focus_handle) = tool_call
            .diffs()
            .next()
            .and_then(|diff| {
                let editor = self
                    .entry_view_state
                    .read(cx)
                    .entry(entry_ix)
                    .and_then(|entry| entry.editor_for_diff(diff))?;
                let has_revealed_diff = diff.read(cx).has_revealed_range(cx);
                let has_focus = editor.read(cx).is_focused(window);
                let focus_handle = editor.focus_handle(cx);
                Some((has_revealed_diff, has_focus, focus_handle))
            })
            .unwrap_or_else(|| (false, false, focus_handle.clone()));

        let use_card_layout = needs_confirmation || is_edit || is_terminal_tool;

        let has_image_content = tool_call.content.iter().any(|c| c.image().is_some());

        let should_show_raw_input = !is_terminal_tool && !is_edit && !has_image_content;

        let has_content = !tool_call.content.is_empty()
            || (should_show_raw_input && tool_call.raw_input.is_some());

        let is_collapsible = has_content && !needs_confirmation;
        let mut is_open = self
            .entry_view_state
            .read(cx)
            .is_tool_call_expanded(&tool_call.id);

        is_open |= needs_confirmation;

        let input_output_header = |label: SharedString| {
            Label::new(label)
                .size(LabelSize::XSmall)
                .color(Color::Muted)
                .buffer_font(cx)
        };

        let tool_output_display = if is_open {
            match &tool_call.status {
                ToolCallStatus::WaitingForConfirmation { .. } => {
                    let confirmation_content = v_flex()
                        .w_full()
                        .children(tool_call.content.iter().enumerate().map(
                            |(content_ix, content)| {
                                div()
                                    .child(self.render_tool_call_content(
                                        active_session_id,
                                        entry_ix,
                                        content,
                                        content_ix,
                                        tool_call,
                                        use_card_layout,
                                        failed_or_canceled,
                                        focus_handle,
                                        window,
                                        cx,
                                    ))
                                    .into_any_element()
                            },
                        ))
                        .when_some(
                            tool_call.sandbox_authorization_details.as_ref(),
                            |this, details| {
                                this.child(self.render_sandbox_authorization_details(
                                    entry_ix,
                                    &tool_call.id,
                                    details,
                                    window,
                                    cx,
                                ))
                            },
                        )
                        .when_some(
                            tool_call.sandbox_fallback_authorization_details.as_ref(),
                            |this, details| {
                                this.child(
                                    self.render_sandbox_fallback_authorization_details(details, cx),
                                )
                            },
                        )
                        .when(should_show_raw_input, |this| {
                            let is_raw_input_expanded =
                                self.expanded_tool_call_raw_inputs.contains(&tool_call.id);

                            let input_header = if is_raw_input_expanded {
                                "Raw Input:"
                            } else {
                                "View Raw Input"
                            };

                            this.child(
                                v_flex()
                                    .p_2()
                                    .gap_1()
                                    .border_t_1()
                                    .border_color(self.tool_card_border_color(cx))
                                    .child(
                                        h_flex()
                                            .id("disclosure_container")
                                            .pl_0p5()
                                            .gap_1()
                                            .justify_between()
                                            .rounded_xs()
                                            .hover(|s| s.bg(cx.theme().colors().element_hover))
                                            .child(input_output_header(input_header.into()))
                                            .child(
                                                Disclosure::new(
                                                    ("raw-input-disclosure", entry_ix),
                                                    is_raw_input_expanded,
                                                )
                                                .opened_icon(IconName::ChevronUp)
                                                .closed_icon(IconName::ChevronDown),
                                            )
                                            .on_click(cx.listener({
                                                let id = tool_call.id.clone();

                                                move |this: &mut Self, _, _, cx| {
                                                    if this
                                                        .expanded_tool_call_raw_inputs
                                                        .contains(&id)
                                                    {
                                                        this.expanded_tool_call_raw_inputs
                                                            .remove(&id);
                                                    } else {
                                                        this.expanded_tool_call_raw_inputs
                                                            .insert(id.clone());
                                                    }
                                                    cx.notify();
                                                }
                                            })),
                                    )
                                    .when(is_raw_input_expanded, |this| {
                                        this.children(tool_call.raw_input_markdown.clone().map(
                                            |input| {
                                                self.render_markdown(
                                                    input,
                                                    MarkdownStyle::themed(
                                                        MarkdownFont::Agent,
                                                        window,
                                                        cx,
                                                    ),
                                                    cx,
                                                )
                                            },
                                        ))
                                    }),
                            )
                        });

                    confirmation_content.into_any()
                }
                ToolCallStatus::Pending | ToolCallStatus::InProgress
                    if is_edit
                        && tool_call.content.is_empty()
                        && self.as_native_connection(cx).is_some() =>
                {
                    self.render_diff_loading(cx)
                }
                ToolCallStatus::Pending
                | ToolCallStatus::InProgress
                | ToolCallStatus::Completed
                | ToolCallStatus::Failed
                | ToolCallStatus::Canceled => v_flex()
                    .when(should_show_raw_input, |this| {
                        this.mt_1p5().w_full().child(
                            v_flex()
                                .ml(rems(0.4))
                                .px_3p5()
                                .pb_1()
                                .gap_1()
                                .border_l_1()
                                .border_color(self.tool_card_border_color(cx))
                                .child(input_output_header("Raw Input:".into()))
                                .children(tool_call.raw_input_markdown.clone().map(|input| {
                                    div().id(("tool-call-raw-input-markdown", entry_ix)).child(
                                        self.render_markdown(
                                            input,
                                            MarkdownStyle::themed(MarkdownFont::Agent, window, cx),
                                            cx,
                                        ),
                                    )
                                }))
                                .child(input_output_header("Output:".into())),
                        )
                    })
                    .children(
                        tool_call
                            .content
                            .iter()
                            .enumerate()
                            .map(|(content_ix, content)| {
                                let output_id = SharedString::from(format!(
                                    "tool-call-output-{entry_ix}-{content_ix}"
                                ));
                                div()
                                    .id(output_id.clone())
                                    .debug_selector(move || output_id.to_string())
                                    .child(self.render_tool_call_content(
                                        active_session_id,
                                        entry_ix,
                                        content,
                                        content_ix,
                                        tool_call,
                                        use_card_layout,
                                        failed_or_canceled,
                                        focus_handle,
                                        window,
                                        cx,
                                    ))
                            }),
                    )
                    .when(!use_card_layout, |this| {
                        let button_id =
                            SharedString::from(format!("tool_output-collapse-{:?}", tool_call.id));
                        let tool_call_id = tool_call.id.clone();

                        this.child(
                            div()
                                .ml(rems(0.4))
                                .px_3p5()
                                .pt_2()
                                .border_l_1()
                                .border_color(self.tool_card_border_color(cx))
                                .child(
                                    IconButton::new(button_id, IconName::ChevronUp)
                                        .full_width()
                                        .style(ButtonStyle::Outlined)
                                        .icon_color(Color::Muted)
                                        .on_click(cx.listener({
                                            move |this: &mut Self,
                                                  _,
                                                  window,
                                                  cx: &mut Context<Self>| {
                                                this.entry_view_state.update(cx, |state, _cx| {
                                                    state.collapse_tool_call(&tool_call_id);
                                                });
                                                this.refresh_thread_search(window, cx);
                                                cx.notify();
                                            }
                                        })),
                                ),
                        )
                    })
                    .into_any(),
                ToolCallStatus::Rejected => Empty.into_any(),
            }
            .into()
        } else {
            None
        };

        let permission_buttons =
            if let ToolCallStatus::WaitingForConfirmation { options, .. } = &tool_call.status {
                Some(self.render_permission_buttons(
                    self.thread.read(cx).session_id().clone(),
                    self.is_first_tool_call(active_session_id, &tool_call.id, cx),
                    options,
                    entry_ix,
                    tool_call.id.clone(),
                    focus_handle,
                    self.sandbox_confusables_block_allow(tool_call, cx),
                    cx,
                ))
            } else {
                None
            };

        let body = v_flex()
            .map(|this| {
                if is_terminal_tool {
                    this.child(self.render_collapsible_command(
                        card_header_id.clone(),
                        true,
                        tool_call.label.clone(),
                        window,
                        cx,
                    ))
                } else {
                    this.child(
                        h_flex()
                            .group(&card_header_id)
                            .relative()
                            .w_full()
                            .justify_between()
                            .when(use_card_layout, |this| {
                                this.p_0p5()
                                    .rounded_t(rems_from_px(5_f32))
                                    .bg(self.tool_card_header_bg(cx))
                            })
                            .child(self.render_tool_call_label(
                                entry_ix,
                                tool_call,
                                is_edit,
                                is_cancelled_edit,
                                has_revealed_diff,
                                use_card_layout,
                                window,
                                cx,
                            ))
                            .child(
                                h_flex()
                                    .when(is_collapsible || failed_or_canceled, |this| {
                                        let diff_for_discard = if has_revealed_diff
                                            && is_cancelled_edit
                                        {
                                            tool_call.diffs().next().cloned()
                                        } else {
                                            None
                                        };

                                        this.child(
                                            h_flex()
                                                .pr_0p5()
                                                .gap_1()
                                                .when(is_collapsible, |this| {
                                                    this.child(
                                                        Disclosure::new(
                                                            ("expand-output", entry_ix),
                                                            is_open,
                                                        )
                                                        .opened_icon(IconName::ChevronUp)
                                                        .closed_icon(IconName::ChevronDown)
                                                        .visible_on_hover(&card_header_id)
                                                        .on_click(cx.listener({
                                                            let id = tool_call.id.clone();
                                                            move |this: &mut Self,
                                                                  _,
                                                                  window,
                                                                  cx: &mut Context<Self>| {
                                                                this.entry_view_state.update(
                                                                    cx,
                                                                    |state, _cx| {
                                                                        state
                                                                            .toggle_tool_call_expansion(
                                                                                &id,
                                                                            );
                                                                    },
                                                                );
                                                                this.refresh_thread_search(window, cx);
                                                                cx.notify();
                                                            }
                                                        })),
                                                    )
                                                })
                                                .when(failed_or_canceled, |this| {
                                                    if is_cancelled_edit && !has_revealed_diff {
                                                        this.child(
                                                            div()
                                                                .id(entry_ix)
                                                                .tooltip(Tooltip::text(
                                                                    "Interrupted Edit",
                                                                ))
                                                                .child(
                                                                    Icon::new(IconName::XCircle)
                                                                        .color(Color::Muted)
                                                                        .size(IconSize::Small),
                                                                ),
                                                        )
                                                    } else if is_cancelled_edit {
                                                        this
                                                    } else {
                                                        this.child(
                                                            Icon::new(IconName::Close)
                                                                .color(Color::Error)
                                                                .size(IconSize::Small),
                                                        )
                                                    }
                                                })
                                                .when_some(diff_for_discard, |this, diff| {
                                                    let tool_call_id = tool_call.id.clone();
                                                    let is_discarded = self
                                                        .discarded_partial_edits
                                                        .contains(&tool_call_id);

                                                    this.when(!is_discarded, |this| {
                                                        this.child(
                                                            IconButton::new(
                                                                ("discard-partial-edit", entry_ix),
                                                                IconName::Undo,
                                                            )
                                                            .icon_size(IconSize::Small)
                                                            .tooltip(move |_, cx| {
                                                                Tooltip::with_meta(
                                                                    "Discard Interrupted Edit",
                                                                    None,
                                                                    "You can discard this interrupted partial edit and restore the original file content.",
                                                                    cx,
                                                                )
                                                            })
                                                            .on_click(cx.listener({
                                                                let tool_call_id =
                                                                    tool_call_id.clone();
                                                                move |this, _, _window, cx| {
                                                                    let diff_data = diff.read(cx);
                                                                    let base_text = diff_data
                                                                        .base_text()
                                                                        .clone();
                                                                    let buffer =
                                                                        diff_data.buffer().clone();
                                                                    buffer.update(
                                                                        cx,
                                                                        |buffer, cx| {
                                                                            buffer.set_text(
                                                                                base_text.as_ref(),
                                                                                cx,
                                                                            );
                                                                        },
                                                                    );
                                                                    this.discarded_partial_edits
                                                                        .insert(
                                                                            tool_call_id.clone(),
                                                                        );
                                                                    cx.notify();
                                                                }
                                                            })),
                                                        )
                                                    })
                                                }),
                                        )
                                    })
                                    .when(tool_call_output_focus, |this| {
                                        this.child(
                                            Button::new("open-file-button", "Open File")
                                                .style(ButtonStyle::Outlined)
                                                .label_size(LabelSize::Small)
                                                .key_binding(
                                                    KeyBinding::for_action_in(&OpenExcerpts, &tool_call_output_focus_handle, cx)
                                                        .map(|s| s.size(rems_from_px(12_f32))),
                                                )
                                                .on_click(|_, window, cx| {
                                                    window.dispatch_action(
                                                        Box::new(OpenExcerpts),
                                                        cx,
                                                    )
                                                }),
                                        )
                                    }),
                            )

                    )
                }
            })
            .children(tool_output_display);

        v_flex()
            .map(|this| {
                if matches!(layout, ToolCallLayout::Embedded | ToolCallLayout::Floating) {
                    this
                } else if use_card_layout {
                    this.my_1p5()
                        .rounded_md()
                        .border_1()
                        .when(failed_or_canceled, |this| this.border_dashed())
                        .border_color(self.tool_card_border_color(cx))
                        .bg(cx.theme().colors().editor_background)
                        .overflow_hidden()
                } else {
                    this.my_1()
                }
            })
            .when(layout == ToolCallLayout::Standalone, |this| {
                this.map(|this| {
                    if has_location && !use_card_layout {
                        this.ml_4()
                    } else {
                        this.ml_5()
                    }
                })
                .mr_5()
            })
            .map(|this| {
                if layout == ToolCallLayout::Floating {
                    this.child(
                        div()
                            .id(("floating-tool-call-body", entry_ix))
                            .max_h_40()
                            .overflow_y_scroll()
                            .child(body),
                    )
                } else {
                    this.child(body)
                }
            })
            .children(permission_buttons)
    }

    /// A small "Learn more" link to the sandboxing docs, deep-linked to
    /// `section` when provided. Shared by the sandbox warning and the two
    /// sandbox approval prompts so the user can always reach an explanation of
    /// what they're being asked about.
    fn render_sandbox_docs_link(
        &self,
        id: &'static str,
        section: Option<&str>,
        cx: &Context<Self>,
    ) -> AnyElement {
        let url = zed_urls::sandboxing_docs(section, cx);

        Button::new(id, "View Sandboxing Docs")
            .label_size(LabelSize::Small)
            .color(Color::Muted)
            .end_icon(
                Icon::new(IconName::ArrowUpRight)
                    .color(Color::Muted)
                    .size(IconSize::XSmall),
            )
            .tooltip({
                let url = url.clone();
                move |_, cx| Tooltip::with_meta("Open Docs", None, url.clone(), cx)
            })
            .on_click(move |_, _, cx| cx.open_url(&url))
            .into_any_element()
    }

    fn render_sandbox_authorization_details(
        &self,
        entry_ix: usize,
        tool_call_id: &acp::ToolCallId,
        details: &SandboxAuthorizationDetails,
        window: &Window,
        cx: &Context<Self>,
    ) -> AnyElement {
        let has_network = details.network_all_hosts || !details.network_hosts.is_empty();
        let has_write = details.allow_fs_write_all || !details.write_paths.is_empty();
        // The dedicated Windows-drive warning prompt is only ever sent while the
        // warning is enabled, so key the banner on the prompt itself. Keeping it
        // visible even after the "Don't show again" checkbox flips the setting
        // avoids the card disappearing out from under the user mid-decision.
        let has_windows_fs_warning = details.warn_windows_fs;
        if !has_network
            && !has_write
            && !details.unsandboxed
            && details.reason.is_empty()
            && !has_windows_fs_warning
        {
            return Empty.into_any_element();
        }

        let confusable_findings = if Self::confusable_warning_enabled(cx) {
            Self::sandbox_confusable_findings(details)
        } else {
            Vec::new()
        };

        let network_section = has_network.then(|| {
            let summary = if details.network_all_hosts {
                "any host".to_string()
            } else {
                format!(
                    "{} {}",
                    details.network_hosts.len(),
                    if details.network_hosts.len() == 1 {
                        "host"
                    } else {
                        "hosts"
                    }
                )
            };
            let has_host_list = !details.network_all_hosts && !details.network_hosts.is_empty();
            let is_open = !self
                .collapsed_sandbox_network_details
                .contains(tool_call_id);
            let mut hosts = details.network_hosts.clone();
            hosts.sort();

            v_flex()
                .child(
                    h_flex()
                        .id(("sandbox-network-details-header", entry_ix))
                        // Align text with the allow/deny button icons below,
                        // which sit at p_1 (container) + Base04 (button) ≈ px_2.
                        .px_2()
                        .py_1()
                        .justify_between()
                        .when(has_host_list, |this| {
                            this.cursor_pointer()
                                .hover(|style| style.bg(cx.theme().colors().element_hover))
                                .on_click(cx.listener({
                                    let tool_call_id = tool_call_id.clone();
                                    move |this, _event, _window, cx| {
                                        if this
                                            .collapsed_sandbox_network_details
                                            .remove(&tool_call_id)
                                        {
                                            cx.notify();
                                            return;
                                        }

                                        this.collapsed_sandbox_network_details
                                            .insert(tool_call_id.clone());
                                        cx.notify();
                                    }
                                }))
                        })
                        .child(
                            h_flex()
                                .gap_1()
                                .child(
                                    Label::new("Network access")
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                )
                                .child(
                                    Label::new("•")
                                        .size(LabelSize::XSmall)
                                        .color(Color::Disabled),
                                )
                                .child(
                                    Label::new(summary)
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                ),
                        )
                        .when(has_host_list, |this| {
                            this.child(
                                Disclosure::new(("sandbox-network-details", entry_ix), is_open)
                                    .opened_icon(IconName::ChevronUp)
                                    .closed_icon(IconName::ChevronDown),
                            )
                        }),
                )
                .when(has_host_list && is_open, |this| {
                    this.child(v_flex().children(hosts.iter().enumerate().map(
                        |(host_ix, host)| {
                            h_flex()
                                .min_w_0()
                                .px_2()
                                .py_1p5()
                                .bg(cx.theme().colors().editor_background)
                                .when(host_ix < hosts.len() - 1, |this| {
                                    this.border_b_1().border_color(cx.theme().colors().border)
                                })
                                .child(
                                    Label::new(host.clone())
                                        .size(LabelSize::XSmall)
                                        .buffer_font(cx),
                                )
                        },
                    )))
                })
        });

        let write_section = has_write.then(|| {
            let summary = if details.allow_fs_write_all {
                "unrestricted except Git metadata".to_string()
            } else {
                format!(
                    "{} {}",
                    details.write_paths.len(),
                    if details.write_paths.len() == 1 {
                        "path"
                    } else {
                        "paths"
                    }
                )
            };
            let has_path_list = !details.allow_fs_write_all && !details.write_paths.is_empty();
            let is_open = !self
                .collapsed_sandbox_authorization_details
                .contains(tool_call_id);
            let mut paths = details.write_paths.clone();
            // Sort by the path that is actually granted (the resolved canonical
            // when present, else the requested path).
            paths.sort_by(|a, b| a.canonical_or_requested().cmp(b.canonical_or_requested()));

            v_flex()
                .child(
                    h_flex()
                        .id(("sandbox-authorization-details-header", entry_ix))
                        .px_2()
                        .py_1()
                        .justify_between()
                        .when(has_path_list, |this| {
                            this.cursor_pointer()
                                .hover(|style| style.bg(cx.theme().colors().element_hover))
                                .on_click(cx.listener({
                                    let tool_call_id = tool_call_id.clone();
                                    move |this, _event, _window, cx| {
                                        if this
                                            .collapsed_sandbox_authorization_details
                                            .remove(&tool_call_id)
                                        {
                                            cx.notify();
                                            return;
                                        }

                                        this.collapsed_sandbox_authorization_details
                                            .insert(tool_call_id.clone());
                                        cx.notify();
                                    }
                                }))
                        })
                        .child(
                            h_flex()
                                .gap_1()
                                .child(
                                    Label::new("Write Access")
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                )
                                .child(
                                    Label::new("•")
                                        .size(LabelSize::XSmall)
                                        .color(Color::Disabled),
                                )
                                .child(
                                    Label::new(summary)
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                ),
                        )
                        .when(has_path_list, |this| {
                            this.child(
                                Disclosure::new(
                                    ("sandbox-authorization-details", entry_ix),
                                    is_open,
                                )
                                .opened_icon(IconName::ChevronUp)
                                .closed_icon(IconName::ChevronDown),
                            )
                        }),
                )
                .when(has_path_list && is_open, |this| {
                    this.child(v_flex().children(paths.iter().enumerate().map(
                        |(path_ix, path)| {
                            self.render_sandbox_authorization_path_row(entry_ix, path_ix, path, cx)
                        },
                    )))
                })
        });

        let unsandboxed_section = details.unsandboxed.then(|| {
            h_flex()
                .px_2()
                .py_1()
                .gap_1p5()
                .child(
                    Icon::new(IconName::Warning)
                        .color(Color::Warning)
                        .size(IconSize::Small),
                )
                .child(
                    Label::new("Runs without the OS sandbox")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
        });

        let reason_section = (!details.reason.is_empty()).then(|| {
            v_flex()
                .px_2()
                .py_1()
                .gap_0p5()
                .child(
                    Label::new("Reason")
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .child(Label::new(details.reason.clone()).size(LabelSize::Small))
        });

        // The command stays in the tool-call title above; here we show what the
        // command is asking for (paths / domains) and the agent's reason.
        v_flex()
            .border_t_1()
            .border_color(self.tool_card_border_color(cx))
            .when(has_windows_fs_warning, |this| {
                this.child(self.render_sandbox_windows_fs_warning(cx))
            })
            .when(!confusable_findings.is_empty(), |this| {
                this.child(self.render_sandbox_confusable_warning(
                    tool_call_id,
                    &confusable_findings,
                    window,
                    cx,
                ))
            })
            .children(network_section)
            .children(write_section)
            .children(unsandboxed_section)
            .children(reason_section)
            .when(!has_windows_fs_warning, |this| {
                // The Windows-drive warning banner carries its own docs link, so
                // skip the default one that every other sandbox prompt appends.
                this.child(
                    h_flex()
                        .px_1()
                        .py_0p5()
                        .child(self.render_sandbox_docs_link(
                            "sandbox-authorization-docs-link",
                            None,
                            cx,
                        )),
                )
            })
            .into_any_element()
    }

    /// Scan the hosts and paths in a sandbox escalation request for surprising
    /// Unicode characters (homoglyphs, invisible characters, bidi overrides).
    /// Returns, for each offending value, the display string shown to the user
    /// and the distinct suspicious characters it contains. Hosts are decoded from
    /// Punycode first, so the display string is the Unicode form the user should
    /// scrutinize. Empty when nothing is surprising.
    fn sandbox_confusable_findings(
        details: &SandboxAuthorizationDetails,
    ) -> Vec<(String, Vec<unicode_confusables::SuspiciousChar>)> {
        let mut findings = Vec::new();
        for host in &details.network_hosts {
            let (decoded, suspicious) = unicode_confusables::scan_host(host);
            if !suspicious.is_empty() {
                findings.push((decoded, suspicious));
            }
        }
        for granted in &details.write_paths {
            // Scan both the requested path and the resolved target (when they
            // differ), so a confusable in either the shown request or the real
            // grant destination is surfaced.
            let requested = granted.requested.display().to_string();
            let resolved = granted.canonical_or_requested().display().to_string();
            for display in [requested, resolved]
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>()
            {
                let suspicious = unicode_confusables::scan(&display);
                if !suspicious.is_empty() {
                    findings.push((display, suspicious));
                }
            }
        }
        findings
    }

    /// Whether the surprising-Unicode warning is enabled in settings (on by
    /// default). When off, prompts neither show the banner nor gate their allow
    /// buttons on it.
    fn confusable_warning_enabled(cx: &App) -> bool {
        AgentSettings::get_global(cx)
            .sandbox_permissions
            .warn_confusable_unicode
    }

    /// Whether this tool call's sandbox escalation shows surprising Unicode that
    /// the user hasn't acknowledged yet. While true, the prompt's allow buttons
    /// stay disabled so the user can't grant access to a lookalike target
    /// without first ticking the acknowledgement checkbox.
    fn sandbox_confusables_block_allow(&self, tool_call: &ToolCall, cx: &App) -> bool {
        if !Self::confusable_warning_enabled(cx) {
            return false;
        }
        let Some(details) = tool_call.sandbox_authorization_details.as_ref() else {
            return false;
        };
        if self
            .acknowledged_confusable_warnings
            .contains(&tool_call.id)
        {
            return false;
        }
        !Self::sandbox_confusable_findings(details).is_empty()
    }

    /// Red banner warning that a requested domain or path contains surprising
    /// Unicode characters, with a checkbox the user must tick to unlock the
    /// allow buttons. See [`Self::sandbox_confusables_block_allow`].
    fn render_sandbox_confusable_warning(
        &self,
        tool_call_id: &acp::ToolCallId,
        findings: &[(String, Vec<unicode_confusables::SuspiciousChar>)],
        window: &Window,
        cx: &Context<Self>,
    ) -> AnyElement {
        let acknowledged = self.acknowledged_confusable_warnings.contains(tool_call_id);
        let line_height = window.line_height();

        v_flex()
            .w_full()
            .p_2()
            .gap_2()
            .border_t_1()
            .border_color(cx.theme().status().error_border)
            .bg(cx.theme().status().error_background.opacity(0.15))
            .child(
                h_flex()
                    .w_full()
                    .gap_1p5()
                    .items_start()
                    .child(
                        h_flex()
                            .h(line_height)
                            .flex_none()
                            .justify_center()
                            .child(
                                Icon::new(IconName::Warning)
                                    .size(IconSize::Small)
                                    .color(Color::Error),
                            ),
                    )
                    .child(
                        v_flex().min_w_0().flex_1().gap_1().children(findings.iter().map(
                            |(value, suspicious)| {
                                v_flex()
                                    .min_w_0()
                                    .gap_0p5()
                                    .child(
                                        Label::new(format!(
                                            "“{value}” contains potentially surprising Unicode characters"
                                        ))
                                        .size(LabelSize::Small)
                                        .color(Color::Error),
                                    )
                                    .child(v_flex().min_w_0().pl_2().children(
                                        suspicious.iter().map(|character| {
                                            Label::new(format!("• {}", character.description()))
                                                .size(LabelSize::XSmall)
                                                .color(Color::Muted)
                                                .buffer_font(cx)
                                        }),
                                    ))
                            },
                        )),
                    )
                    .child(
                        IconButton::new("configure-confusable-warning", IconName::Settings)
                            .icon_size(IconSize::Small)
                            .icon_color(Color::Muted)
                            .tooltip(Tooltip::text("Configure unicode confusables warning"))
                            .on_click(|_, window, cx| {
                                window.dispatch_action(
                                    Box::new(zed_actions::OpenSettingsAt {
                                        path: zed_actions::AGENT_SANDBOX_SETTINGS_PATH.to_string(),
                                        target: None,
                                    }),
                                    cx,
                                );
                            }),
                    ),
            )
            .child(
                Checkbox::new(
                    SharedString::from(format!("confusable-ack-{}", tool_call_id.0)),
                    if acknowledged {
                        ToggleState::Selected
                    } else {
                        ToggleState::Unselected
                    },
                )
                .label("I understand and wish to proceed")
                .label_size(LabelSize::Small)
                .on_click(cx.listener({
                    let tool_call_id = tool_call_id.clone();
                    move |this, state: &ToggleState, _window, cx| {
                        if *state == ToggleState::Selected {
                            this.acknowledged_confusable_warnings
                                .insert(tool_call_id.clone());
                        } else {
                            this.acknowledged_confusable_warnings.remove(&tool_call_id);
                        }
                        cx.notify();
                    }
                })),
            )
            .into_any_element()
    }

    /// Whether the Windows-drive (DrvFs) weaker-guarantee warning is enabled in
    /// settings (on by default). Windows-only in effect: `warn_windows_fs` is
    /// never set on other platforms.
    fn ntfs_warning_enabled(cx: &App) -> bool {
        AgentSettings::get_global(cx)
            .sandbox_permissions
            .warn_ntfs_grants
    }

    /// Informational banner shown on a sandbox approval prompt when the command
    /// will write to a file on a Windows drive (reached inside WSL via DrvFs),
    /// whose sandbox-integrity guarantees are weaker than the distro's native
    /// filesystem. Unlike the confusable-Unicode banner this does not gate the
    /// allow buttons: the approval itself is the acknowledgement. A settings gear
    /// links to where the warning can be suppressed.
    fn render_sandbox_windows_fs_warning(&self, cx: &Context<Self>) -> AnyElement {
        v_flex()
            .w_full()
            .p_2()
            .gap_1()
            .border_t_1()
            .border_color(cx.theme().status().warning_border)
            .bg(cx.theme().status().warning_background.opacity(0.15))
            .child(
                h_flex()
                    .w_full()
                    .gap_1p5()
                    .items_start()
                    .child(
                        Icon::new(IconName::Warning)
                            .size(IconSize::Small)
                            .color(Color::Warning),
                    )
                    .child(
                        v_flex()
                            .min_w_0()
                            .flex_1()
                            .gap_0p5()
                            .child(
                                Label::new("This command can write to a file on a Windows drive")
                                    .size(LabelSize::Small)
                                    .color(Color::Warning),
                            )
                            .child(
                                Label::new(
                                    "Sandboxes with write access to a location on a Windows \
                                     drive may not provide full filesystem isolation.",
                                )
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                            )
                            .child(h_flex().child(self.render_sandbox_docs_link(
                                "sandbox-windows-fs-docs-link",
                                Some("windows"),
                                cx,
                            ))),
                    )
                    .child(
                        IconButton::new("configure-ntfs-warning", IconName::Settings)
                            .icon_size(IconSize::Small)
                            .icon_color(Color::Muted)
                            .tooltip(Tooltip::text("Configure Windows-drive warning"))
                            .on_click(|_, window, cx| {
                                window.dispatch_action(
                                    Box::new(zed_actions::OpenSettingsAt {
                                        path: zed_actions::AGENT_SANDBOX_SETTINGS_PATH.to_string(),
                                        target: None,
                                    }),
                                    cx,
                                );
                            }),
                    ),
            )
            .child(
                Checkbox::new(
                    "sandbox-windows-fs-dont-warn",
                    if Self::ntfs_warning_enabled(cx) {
                        ToggleState::Unselected
                    } else {
                        ToggleState::Selected
                    },
                )
                .label("Don't show this warning again")
                .label_size(LabelSize::Small)
                .on_click(cx.listener(|this, state: &ToggleState, _window, cx| {
                    let disable = *state == ToggleState::Selected;
                    let fs = this.thread.read(cx).project().read(cx).fs().clone();
                    update_settings_file(fs, cx, move |settings, _| {
                        settings
                            .agent
                            .get_or_insert_default()
                            .sandbox_permissions
                            .get_or_insert_default()
                            .warn_ntfs_grants = Some(!disable);
                    });
                    cx.notify();
                })),
            )
            .into_any_element()
    }

    fn render_sandbox_fallback_authorization_details(
        &self,
        details: &SandboxFallbackAuthorizationDetails,
        cx: &Context<Self>,
    ) -> AnyElement {
        // The command itself is shown in the tool-call header (a collapsible
        // command), so here we only explain *why* the sandbox couldn't be
        // created — the user needs both to decide whether to run unsandboxed.
        if details.reason.is_empty() {
            return Empty.into_any_element();
        }

        h_flex()
            .p_1p5()
            .gap_1p5()
            .items_start()
            .border_t_1()
            .border_color(self.tool_card_border_color(cx))
            .child(
                Icon::new(IconName::Warning)
                    .color(Color::Warning)
                    .size(IconSize::Small),
            )
            .child(
                v_flex()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        Label::new("Couldn't create a sandbox")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(Label::new(details.reason.clone()).size(LabelSize::Small))
                    .child(self.render_sandbox_docs_link(
                        "sandbox-fallback-docs-link",
                        details.docs_section.as_deref(),
                        cx,
                    )),
            )
            .into_any_element()
    }

    fn render_sandbox_authorization_path_row(
        &self,
        entry_ix: usize,
        path_ix: usize,
        granted: &settings::GrantedWritePath,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        // The path that is actually granted is the resolved canonical target.
        // When the request went through a symlink to a *different* target, both
        // paths are shown, each explicitly captioned, so it's unmistakable which
        // string was requested and which location write access is really granted
        // to.
        let granted_path = granted.canonical_or_requested();
        let requested_path = granted.requested.clone();
        // Grants are stored in the request's own namespace (a Windows path stays
        // `C:\...`, a WSL path stays `/...`), so a genuine symlink/junction
        // redirect is just a plain inequality between the request and its
        // resolved canonical.
        let is_redirected = granted
            .resolved
            .as_deref()
            .is_some_and(|resolved| resolved != requested_path.as_path());

        let granted_display = granted_path.display().to_string();
        let requested_display = requested_path.display().to_string();

        let captioned_path = |caption: SharedString, path: String, cx: &Context<Self>| {
            v_flex()
                .min_w_0()
                .gap_0p5()
                .child(
                    Label::new(caption)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .child(Label::new(path).size(LabelSize::Small).buffer_font(cx))
        };

        v_flex()
            .id(format!("sandbox-authorization-path-{entry_ix}-{path_ix}"))
            .min_w_0()
            .gap_1()
            .px_2()
            .py_1p5()
            .bg(cx.theme().colors().editor_background)
            .map(|this| {
                if is_redirected {
                    this.child(captioned_path("Source".into(), requested_display, cx))
                        .child(
                            Icon::new(IconName::ArrowDown)
                                .color(Color::Muted)
                                .size(IconSize::Small),
                        )
                        .child(captioned_path("Target".into(), granted_display, cx))
                } else {
                    // Not a genuine redirect: show what the user asked for (e.g.
                    // the `C:\...` path), not the internal Linux canonical.
                    this.child(captioned_path("Write Path".into(), requested_display, cx))
                }
            })
            .child(Divider::horizontal())
    }

    fn render_permission_buttons(
        &self,
        session_id: acp::SessionId,
        is_first: bool,
        options: &PermissionOptions,
        entry_ix: usize,
        tool_call_id: acp::ToolCallId,
        focus_handle: &FocusHandle,
        // When true, the "allow" choices are disabled (e.g. an unacknowledged
        // surprising-Unicode warning is showing). "Deny"/"Retry" stay enabled.
        allow_disabled: bool,
        cx: &Context<Self>,
    ) -> Div {
        match options {
            PermissionOptions::Flat(options) => self.render_permission_buttons_flat(
                session_id,
                is_first,
                options,
                entry_ix,
                tool_call_id,
                focus_handle,
                allow_disabled,
                cx,
            ),
            PermissionOptions::Dropdown(choices) => self.render_permission_buttons_with_dropdown(
                is_first,
                choices,
                None,
                entry_ix,
                session_id,
                tool_call_id,
                focus_handle,
                allow_disabled,
                cx,
            ),
            PermissionOptions::DropdownWithPatterns {
                choices,
                patterns,
                tool_name,
            } => self.render_permission_buttons_with_dropdown(
                is_first,
                choices,
                Some((patterns, tool_name)),
                entry_ix,
                session_id,
                tool_call_id,
                focus_handle,
                allow_disabled,
                cx,
            ),
        }
    }

    fn render_permission_buttons_with_dropdown(
        &self,
        is_first: bool,
        choices: &[PermissionOptionChoice],
        patterns: Option<(&[PermissionPattern], &str)>,
        entry_ix: usize,
        session_id: acp::SessionId,
        tool_call_id: acp::ToolCallId,
        focus_handle: &FocusHandle,
        allow_disabled: bool,
        cx: &Context<Self>,
    ) -> Div {
        let selection = self.permission_selections.get(&tool_call_id);

        let selected_index = selection
            .and_then(|s| s.choice_index())
            .unwrap_or_else(|| choices.len().saturating_sub(1));

        let dropdown_label: SharedString =
            if matches!(selection, Some(PermissionSelection::SelectedPatterns(_))) {
                "Always for selected commands".into()
            } else {
                choices
                    .get(selected_index)
                    .or(choices.last())
                    .map(|choice| choice.label())
                    .unwrap_or_else(|| "Only this time".into())
            };

        let dropdown = if let Some((pattern_list, tool_name)) = patterns {
            self.render_permission_granularity_dropdown_with_patterns(
                choices,
                pattern_list,
                tool_name,
                dropdown_label,
                entry_ix,
                tool_call_id.clone(),
                is_first,
                cx,
            )
        } else {
            self.render_permission_granularity_dropdown(
                choices,
                dropdown_label,
                entry_ix,
                tool_call_id.clone(),
                selected_index,
                is_first,
                cx,
            )
        };

        h_flex()
            .w_full()
            .p_1()
            .gap_2()
            .justify_between()
            .border_t_1()
            .border_color(self.tool_card_border_color(cx))
            .child(
                h_flex()
                    .gap_0p5()
                    .child(
                        Button::new(("allow-btn", entry_ix), "Allow")
                            .disabled(allow_disabled)
                            .start_icon(
                                Icon::new(IconName::Check)
                                    .size(IconSize::XSmall)
                                    .color(Color::Success),
                            )
                            .label_size(LabelSize::Small)
                            .when(is_first && !allow_disabled, |this| {
                                this.key_binding(
                                    KeyBinding::for_action_in(
                                        &AllowOnce as &dyn Action,
                                        focus_handle,
                                        cx,
                                    )
                                    .map(|kb| kb.size(rems_from_px(12_f32))),
                                )
                            })
                            .on_click(cx.listener({
                                let session_id = session_id.clone();
                                let tool_call_id = tool_call_id.clone();
                                move |this, _, window, cx| {
                                    this.authorize_with_granularity(
                                        session_id.clone(),
                                        tool_call_id.clone(),
                                        true,
                                        window,
                                        cx,
                                    );
                                }
                            })),
                    )
                    .child(
                        Button::new(("deny-btn", entry_ix), "Deny")
                            .start_icon(
                                Icon::new(IconName::Close)
                                    .size(IconSize::XSmall)
                                    .color(Color::Error),
                            )
                            .label_size(LabelSize::Small)
                            .when(is_first, |this| {
                                this.key_binding(
                                    KeyBinding::for_action_in(
                                        &RejectOnce as &dyn Action,
                                        focus_handle,
                                        cx,
                                    )
                                    .map(|kb| kb.size(rems_from_px(12_f32))),
                                )
                            })
                            .on_click(cx.listener({
                                move |this, _, window, cx| {
                                    this.authorize_with_granularity(
                                        session_id.clone(),
                                        tool_call_id.clone(),
                                        false,
                                        window,
                                        cx,
                                    );
                                }
                            })),
                    ),
            )
            .child(dropdown)
    }

    fn render_permission_granularity_dropdown(
        &self,
        choices: &[PermissionOptionChoice],
        current_label: SharedString,
        entry_ix: usize,
        tool_call_id: acp::ToolCallId,
        selected_index: usize,
        is_first: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        let menu_options: Vec<(usize, SharedString)> = choices
            .iter()
            .enumerate()
            .map(|(i, choice)| (i, choice.label()))
            .collect();

        let permission_dropdown_handle = self.permission_dropdown_handle.clone();

        PopoverMenu::new(("permission-granularity", entry_ix))
            .with_handle(permission_dropdown_handle)
            .trigger(
                Button::new(("granularity-trigger", entry_ix), current_label)
                    .end_icon(
                        Icon::new(IconName::ChevronDown)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .label_size(LabelSize::Small)
                    .when(is_first, |this| {
                        this.key_binding(
                            KeyBinding::for_action_in(
                                &crate::OpenPermissionDropdown as &dyn Action,
                                &self.focus_handle(cx),
                                cx,
                            )
                            .map(|kb| kb.size(rems_from_px(12_f32))),
                        )
                    }),
            )
            .menu(move |window, cx| {
                let tool_call_id = tool_call_id.clone();
                let options = menu_options.clone();

                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                    for (index, display_name) in options.iter() {
                        let display_name = display_name.clone();
                        let index = *index;
                        let tool_call_id_for_entry = tool_call_id.clone();
                        let is_selected = index == selected_index;
                        menu = menu.toggleable_entry(
                            display_name,
                            is_selected,
                            IconPosition::End,
                            None,
                            move |window, cx| {
                                window.dispatch_action(
                                    SelectPermissionGranularity {
                                        tool_call_id: tool_call_id_for_entry.0.to_string(),
                                        index,
                                    }
                                    .boxed_clone(),
                                    cx,
                                );
                            },
                        );
                    }

                    menu
                }))
            })
            .into_any_element()
    }

    fn render_permission_granularity_dropdown_with_patterns(
        &self,
        choices: &[PermissionOptionChoice],
        patterns: &[PermissionPattern],
        _tool_name: &str,
        current_label: SharedString,
        entry_ix: usize,
        tool_call_id: acp::ToolCallId,
        is_first: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        let default_choice_index = choices.len().saturating_sub(1);
        let menu_options: Vec<(usize, SharedString)> = choices
            .iter()
            .enumerate()
            .map(|(i, choice)| (i, choice.label()))
            .collect();

        let pattern_options: Vec<(usize, SharedString)> = patterns
            .iter()
            .enumerate()
            .map(|(i, cp)| {
                (
                    i,
                    SharedString::from(format!("Always for `{}` commands", cp.display_name)),
                )
            })
            .collect();

        let pattern_count = patterns.len();
        let permission_dropdown_handle = self.permission_dropdown_handle.clone();
        let view = cx.entity().downgrade();

        PopoverMenu::new(("permission-granularity", entry_ix))
            .with_handle(permission_dropdown_handle.clone())
            .anchor(gpui::Anchor::TopRight)
            .attach(gpui::Anchor::BottomRight)
            .trigger(
                Button::new(("granularity-trigger", entry_ix), current_label)
                    .end_icon(
                        Icon::new(IconName::ChevronDown)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .label_size(LabelSize::Small)
                    .when(is_first, |this| {
                        this.key_binding(
                            KeyBinding::for_action_in(
                                &crate::OpenPermissionDropdown as &dyn Action,
                                &self.focus_handle(cx),
                                cx,
                            )
                            .map(|kb| kb.size(rems_from_px(12_f32))),
                        )
                    }),
            )
            .menu(move |window, cx| {
                let tool_call_id = tool_call_id.clone();
                let options = menu_options.clone();
                let patterns = pattern_options.clone();
                let view = view.clone();
                let dropdown_handle = permission_dropdown_handle.clone();

                Some(ContextMenu::build_persistent(
                    window,
                    cx,
                    move |menu, _window, cx| {
                        let mut menu = menu;

                        // Read fresh selection state from the view on each rebuild.
                        let selection: Option<PermissionSelection> = view.upgrade().and_then(|v| {
                            let view = v.read(cx);
                            view.permission_selections.get(&tool_call_id).cloned()
                        });

                        let is_pattern_mode =
                            matches!(selection, Some(PermissionSelection::SelectedPatterns(_)));

                        // Granularity choices: "Always for terminal", "Only this time"
                        for (index, display_name) in options.iter() {
                            let display_name = display_name.clone();
                            let index = *index;
                            let tool_call_id_for_entry = tool_call_id.clone();
                            let is_selected = !is_pattern_mode
                                && selection
                                    .as_ref()
                                    .and_then(|s| s.choice_index())
                                    .map_or(index == default_choice_index, |ci| ci == index);

                            let view = view.clone();
                            menu = menu.toggleable_entry(
                                display_name,
                                is_selected,
                                IconPosition::End,
                                None,
                                move |_window, cx| {
                                    view.update(cx, |this, cx| {
                                        this.permission_selections.insert(
                                            tool_call_id_for_entry.clone(),
                                            PermissionSelection::Choice(index),
                                        );
                                        cx.notify();
                                    })
                                    .log_err();
                                },
                            );
                        }

                        menu = menu.separator().header("Select Options…");

                        for (pattern_index, label) in patterns.iter() {
                            let label = label.clone();
                            let pattern_index = *pattern_index;
                            let tool_call_id_for_pattern = tool_call_id.clone();
                            let is_checked = selection
                                .as_ref()
                                .is_some_and(|s| s.is_pattern_checked(pattern_index));

                            let view = view.clone();
                            menu = menu.toggleable_entry(
                                label,
                                is_checked,
                                IconPosition::End,
                                None,
                                move |_window, cx| {
                                    view.update(cx, |this, cx| {
                                        let selection = this
                                            .permission_selections
                                            .get_mut(&tool_call_id_for_pattern);

                                        match selection {
                                            Some(PermissionSelection::SelectedPatterns(_)) => {
                                                // Already in pattern mode — toggle.
                                                this.permission_selections
                                                    .get_mut(&tool_call_id_for_pattern)
                                                    .expect("just matched above")
                                                    .toggle_pattern(pattern_index);
                                            }
                                            _ => {
                                                // First click: activate pattern mode
                                                // with all patterns checked.
                                                this.permission_selections.insert(
                                                    tool_call_id_for_pattern.clone(),
                                                    PermissionSelection::SelectedPatterns(
                                                        (0..pattern_count).collect(),
                                                    ),
                                                );
                                            }
                                        }
                                        cx.notify();
                                    })
                                    .log_err();
                                },
                            );
                        }

                        let any_patterns_checked = selection
                            .as_ref()
                            .is_some_and(|s| s.has_any_checked_patterns());
                        let dropdown_handle = dropdown_handle.clone();
                        menu = menu.custom_row(move |_window, _cx| {
                            div()
                                .py_1()
                                .w_full()
                                .child(
                                    Button::new("apply-patterns", "Apply")
                                        .full_width()
                                        .style(ButtonStyle::Outlined)
                                        .label_size(LabelSize::Small)
                                        .disabled(!any_patterns_checked)
                                        .on_click({
                                            let dropdown_handle = dropdown_handle.clone();
                                            move |_event, _window, cx| {
                                                dropdown_handle.hide(cx);
                                            }
                                        }),
                                )
                                .into_any_element()
                        });

                        menu
                    },
                ))
            })
            .into_any_element()
    }

    fn render_permission_buttons_flat(
        &self,
        session_id: acp::SessionId,
        is_first: bool,
        options: &[acp::PermissionOption],
        entry_ix: usize,
        tool_call_id: acp::ToolCallId,
        focus_handle: &FocusHandle,
        allow_disabled: bool,
        cx: &Context<Self>,
    ) -> Div {
        let mut seen_kinds: ArrayVec<acp::PermissionOptionKind, 3, u8> = ArrayVec::new();

        div()
            .p_1()
            .border_t_1()
            .border_color(self.tool_card_border_color(cx))
            .w_full()
            .v_flex()
            .gap_0p5()
            .children(options.iter().map(move |option| {
                let option_id = SharedString::from(option.option_id.0.clone());
                Button::new((option_id, entry_ix), option.name.clone())
                    .map(|this| {
                        // The sandbox-fallback prompt offers a "Retry" option
                        // that re-attempts creating the sandbox; it isn't an
                        // allow/deny choice, so give it its own icon and no
                        // keybinding.
                        let is_retry = option.option_id.0.as_ref()
                            == acp_thread::SANDBOX_FALLBACK_RETRY_OPTION_ID;
                        let (icon, action) = if is_retry {
                            (
                                Icon::new(IconName::RotateCcw)
                                    .size(IconSize::XSmall)
                                    .color(Color::Muted),
                                None,
                            )
                        } else {
                            match option.kind {
                                acp::PermissionOptionKind::AllowOnce => (
                                    Icon::new(IconName::Check)
                                        .size(IconSize::XSmall)
                                        .color(Color::Success),
                                    Some(&AllowOnce as &dyn Action),
                                ),
                                acp::PermissionOptionKind::AllowAlways => (
                                    Icon::new(IconName::CheckDouble)
                                        .size(IconSize::XSmall)
                                        .color(Color::Success),
                                    if option.option_id.0.as_ref()
                                        == acp_thread::SandboxPermission::AllowThread.as_id()
                                    {
                                        None
                                    } else {
                                        Some(&AllowAlways as &dyn Action)
                                    },
                                ),
                                acp::PermissionOptionKind::RejectOnce => (
                                    Icon::new(IconName::Close)
                                        .size(IconSize::XSmall)
                                        .color(Color::Error),
                                    Some(&RejectOnce as &dyn Action),
                                ),
                                acp::PermissionOptionKind::RejectAlways | _ => (
                                    Icon::new(IconName::Close)
                                        .size(IconSize::XSmall)
                                        .color(Color::Error),
                                    None,
                                ),
                            }
                        };

                        // An "allow" choice is disabled while a surprising-Unicode
                        // warning is unacknowledged; "deny"/"retry" stay enabled.
                        let is_allow = matches!(
                            option.kind,
                            acp::PermissionOptionKind::AllowOnce
                                | acp::PermissionOptionKind::AllowAlways
                        ) && !is_retry;
                        let disabled = allow_disabled && is_allow;

                        let this = this.start_icon(icon).disabled(disabled);

                        let Some(action) = action else {
                            return this;
                        };

                        if !is_first || disabled || seen_kinds.contains(&option.kind) {
                            return this;
                        }

                        seen_kinds.push(option.kind).unwrap();

                        this.key_binding(
                            KeyBinding::for_action_in(action, focus_handle, cx)
                                .map(|kb| kb.size(rems_from_px(12_f32))),
                        )
                    })
                    .label_size(LabelSize::Small)
                    .on_click(cx.listener({
                        let tool_call_id = tool_call_id.clone();
                        let option_id = option.option_id.clone();
                        let option_kind = option.kind;
                        let session_id = session_id.clone();
                        move |this, _, window, cx| {
                            this.authorize_tool_call(
                                session_id.clone(),
                                tool_call_id.clone(),
                                SelectedPermissionOutcome::new(option_id.clone(), option_kind),
                                window,
                                cx,
                            );
                        }
                    }))
            }))
    }

    fn render_diff_loading(&self, cx: &Context<Self>) -> AnyElement {
        let bar = |n: u64, width_class: &str| {
            let bg_color = cx.theme().colors().element_active;
            let base = h_flex().h_1().rounded_full();

            let modified = match width_class {
                "w_4_5" => base.w_3_4(),
                "w_1_4" => base.w_1_4(),
                "w_2_4" => base.w_2_4(),
                "w_3_5" => base.w_3_5(),
                "w_2_5" => base.w_2_5(),
                _ => base.w_1_2(),
            };

            modified.with_animation(
                ElementId::Integer(n),
                Animation::new(Duration::from_secs(2)).repeat(),
                move |tab, delta| {
                    let delta = (delta - 0.15 * n as f32) / 0.7;
                    let delta = 1.0 - (0.5 - delta).abs() * 2.;
                    let delta = ease_in_out(delta.clamp(0., 1.));
                    let delta = 0.1 + 0.9 * delta;

                    tab.bg(bg_color.opacity(delta))
                },
            )
        };

        v_flex()
            .p_3()
            .gap_1()
            .rounded_b_md()
            .bg(cx.theme().colors().editor_background)
            .child(bar(0, "w_4_5"))
            .child(bar(1, "w_1_4"))
            .child(bar(2, "w_2_4"))
            .child(bar(3, "w_3_5"))
            .child(bar(4, "w_2_5"))
            .into_any_element()
    }

    fn render_tool_call_label(
        &self,
        entry_ix: usize,
        tool_call: &ToolCall,
        is_edit: bool,
        has_failed: bool,
        has_revealed_diff: bool,
        use_card_layout: bool,
        window: &Window,
        cx: &Context<Self>,
    ) -> Div {
        let has_location = tool_call.locations.len() == 1;
        let is_file = tool_call.kind == acp::ToolKind::Edit && has_location;
        let is_subagent_tool_call = tool_call.is_subagent();

        let file_icon = if has_location {
            FileIcons::get_icon(&tool_call.locations[0].path, cx)
                .map(|from_path| Icon::from_path(from_path).color(Color::Muted))
                .unwrap_or(Icon::new(IconName::ToolPencil).color(Color::Muted))
        } else {
            Icon::new(IconName::ToolPencil).color(Color::Muted)
        };

        let tool_icon = if is_file && has_failed && has_revealed_diff {
            div()
                .id(entry_ix)
                .tooltip(Tooltip::text("Interrupted Edit"))
                .child(DecoratedIcon::new(
                    file_icon,
                    Some(
                        IconDecoration::new(
                            IconDecorationKind::Triangle,
                            self.tool_card_header_bg(cx),
                            cx,
                        )
                        .color(cx.theme().status().warning)
                        .position(gpui::Point {
                            x: px(-2.),
                            y: px(-2.),
                        }),
                    ),
                ))
                .into_any_element()
        } else if is_file {
            div().child(file_icon).into_any_element()
        } else if is_subagent_tool_call {
            Icon::new(self.agent_icon)
                .size(IconSize::Small)
                .color(Color::Muted)
                .into_any_element()
        } else {
            Icon::new(match tool_call.kind {
                acp::ToolKind::Read => IconName::ToolSearch,
                acp::ToolKind::Edit => IconName::ToolPencil,
                acp::ToolKind::Delete => IconName::ToolDeleteFile,
                acp::ToolKind::Move => IconName::ArrowRightLeft,
                acp::ToolKind::Search => IconName::ToolSearch,
                acp::ToolKind::Execute => IconName::ToolTerminal,
                acp::ToolKind::Think => IconName::ToolThink,
                acp::ToolKind::Fetch => IconName::ToolWeb,
                acp::ToolKind::SwitchMode => IconName::ArrowRightLeft,
                acp::ToolKind::Other | _ => IconName::ToolHammer,
            })
            .size(IconSize::Small)
            .color(Color::Muted)
            .into_any_element()
        };

        let gradient_overlay = {
            div()
                .absolute()
                .top_0()
                .right_0()
                .w_12()
                .h_full()
                .map(|this| {
                    if use_card_layout {
                        this.bg(linear_gradient(
                            90.,
                            linear_color_stop(self.tool_card_header_bg(cx), 1.),
                            linear_color_stop(self.tool_card_header_bg(cx).opacity(0.2), 0.),
                        ))
                    } else {
                        this.bg(linear_gradient(
                            90.,
                            linear_color_stop(cx.theme().colors().panel_background, 1.),
                            linear_color_stop(
                                cx.theme().colors().panel_background.opacity(0.2),
                                0.,
                            ),
                        ))
                    }
                })
        };

        h_flex()
            .relative()
            .w_full()
            .h(window.line_height() - px(2.))
            .text_size(self.tool_name_font_size())
            .gap_1p5()
            .when(has_location || use_card_layout, |this| this.px_1())
            .when(has_location, |this| {
                this.cursor(CursorStyle::PointingHand)
                    .rounded(rems_from_px(3_f32)) // Concentric border radius
                    .hover(|s| s.bg(cx.theme().colors().element_hover.opacity(0.5)))
            })
            .overflow_hidden()
            .child(tool_icon)
            .child(if has_location {
                h_flex()
                    .id(("open-tool-call-location", entry_ix))
                    .w_full()
                    .map(|this| {
                        if use_card_layout {
                            this.text_color(cx.theme().colors().text)
                        } else {
                            this.text_color(cx.theme().colors().text_muted)
                        }
                    })
                    .child(self.render_markdown(
                        tool_call.label.clone(),
                        {
                            let mut style = MarkdownStyle::themed(MarkdownFont::Agent, window, cx)
                                .with_muted_text(cx);
                            style.prevent_mouse_interaction = true;
                            self.agent_panel_styling
                                .tool_output
                                .apply_text_to_markdown_style(&mut style);
                            style
                        },
                        cx,
                    ))
                    .tooltip(Tooltip::text("Go to File"))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_tool_call_location(entry_ix, 0, window, cx);
                    }))
                    .into_any_element()
            } else {
                h_flex()
                    .w_full()
                    .child(self.render_markdown(
                        tool_call.label.clone(),
                        {
                            let mut style = MarkdownStyle::themed(MarkdownFont::Agent, window, cx)
                                .with_muted_text(cx);
                            self.agent_panel_styling
                                .tool_output
                                .apply_text_to_markdown_style(&mut style);
                            style
                        },
                        cx,
                    ))
                    .into_any()
            })
            .when(!is_edit, |this| this.child(gradient_overlay))
    }

    fn open_tool_call_location(
        &self,
        entry_ix: usize,
        location_ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<()> {
        let (tool_call_location, agent_location) = self
            .thread
            .read(cx)
            .entries()
            .get(entry_ix)?
            .location(location_ix)?;

        let project_path = self
            .project
            .upgrade()?
            .read(cx)
            .find_project_path(&tool_call_location.path, cx);

        let open_task = self
            .workspace
            .update(cx, |workspace, cx| {
                if let Some(project_path) = project_path {
                    workspace.open_path(project_path, None, true, window, cx)
                } else {
                    workspace.open_abs_path(
                        tool_call_location.path.clone(),
                        OpenOptions {
                            focus: Some(true),
                            ..Default::default()
                        },
                        window,
                        cx,
                    )
                }
            })
            .log_err()?;
        window
            .spawn(cx, async move |cx| {
                let item = open_task.await?;

                let Some(active_editor) = item.downcast::<Editor>() else {
                    return anyhow::Ok(());
                };

                active_editor.update_in(cx, |editor, window, cx| {
                    let snapshot = editor.buffer().read(cx).snapshot(cx);
                    if snapshot.as_singleton().is_some()
                        && let Some(anchor) = snapshot.anchor_in_excerpt(agent_location.position)
                    {
                        editor.change_selections(Default::default(), window, cx, |selections| {
                            selections.select_anchor_ranges([anchor..anchor]);
                        })
                    } else {
                        let row = tool_call_location.line.unwrap_or_default();
                        editor.change_selections(Default::default(), window, cx, |selections| {
                            selections.select_ranges([Point::new(row, 0)..Point::new(row, 0)]);
                        })
                    }
                })?;

                anyhow::Ok(())
            })
            .detach_and_log_err(cx);

        None
    }

    fn render_tool_call_content(
        &self,
        session_id: &acp::SessionId,
        entry_ix: usize,
        content: &ToolCallContent,
        context_ix: usize,
        tool_call: &ToolCall,
        card_layout: bool,
        has_failed: bool,
        focus_handle: &FocusHandle,
        window: &Window,
        cx: &Context<Self>,
    ) -> AnyElement {
        match content {
            ToolCallContent::ContentBlock(content) => {
                if let Some((resource, markdown)) = content.embedded_resource() {
                    self.render_embedded_resource_output(
                        resource,
                        markdown.cloned(),
                        entry_ix,
                        context_ix,
                        tool_call,
                        card_layout,
                        window,
                        cx,
                    )
                } else if let Some(resource_link) = content.resource_link() {
                    self.render_resource_link(resource_link, cx)
                } else if let Some(markdown) = content.markdown() {
                    self.render_markdown_output(
                        markdown.clone(),
                        entry_ix,
                        context_ix,
                        tool_call,
                        card_layout,
                        window,
                        cx,
                    )
                } else if let Some((image, _)) = content.image() {
                    let location = tool_call.locations.first().cloned();
                    self.render_image_output(entry_ix, image.clone(), location, card_layout, cx)
                } else {
                    Empty.into_any_element()
                }
            }
            ToolCallContent::Diff(diff) => {
                self.render_diff_editor(entry_ix, diff, tool_call, has_failed, cx)
            }
            ToolCallContent::Terminal(terminal) => self.render_terminal_tool_call(
                session_id,
                entry_ix,
                terminal,
                tool_call,
                focus_handle,
                ToolCallLayout::Standalone,
                window,
                cx,
            ),
        }
    }

    fn render_embedded_resource_output(
        &self,
        resource: &acp::EmbeddedResource,
        markdown: Option<Entity<Markdown>>,
        entry_ix: usize,
        context_ix: usize,
        tool_call: &ToolCall,
        card_layout: bool,
        window: &Window,
        cx: &Context<Self>,
    ) -> AnyElement {
        if let Some(markdown) = markdown {
            return self.render_markdown_output(
                markdown,
                entry_ix,
                context_ix,
                tool_call,
                card_layout,
                window,
                cx,
            );
        }

        let uri = match &resource.resource {
            acp::EmbeddedResourceResource::BlobResourceContents(blob) => blob.uri.as_str(),
            acp::EmbeddedResourceResource::TextResourceContents(text) => text.uri.as_str(),
            _ => "",
        };

        v_flex()
            .gap_1()
            .map(|this| {
                if card_layout {
                    this.p_2().when(context_ix > 0, |this| {
                        this.border_t_1()
                            .border_color(self.tool_card_border_color(cx))
                    })
                } else {
                    this.ml(rems(0.4))
                        .px_3p5()
                        .border_l_1()
                        .border_color(self.tool_card_border_color(cx))
                }
            })
            .when(!uri.is_empty(), |this| {
                this.child(
                    Label::new(uri.to_string())
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
            })
            .into_any_element()
    }

    fn render_resource_link(
        &self,
        resource_link: &acp::ResourceLink,
        cx: &Context<Self>,
    ) -> AnyElement {
        let uri: SharedString = resource_link.uri.clone().into();
        let is_file = resource_link.uri.strip_prefix("file://");

        let Some(project) = self.project.upgrade() else {
            return Empty.into_any_element();
        };

        let label: SharedString = if let Some(abs_path) = is_file {
            // Split off an optional `#L<line>` fragment so the path still resolves.
            let (abs_path, fragment) = abs_path
                .split_once('#')
                .map_or((abs_path, None), |(path, fragment)| (path, Some(fragment)));

            let path_label = if let Some(project_path) = project
                .read(cx)
                .project_path_for_absolute_path(&Path::new(abs_path), cx)
                && let Some(worktree) = project
                    .read(cx)
                    .worktree_for_id(project_path.worktree_id, cx)
            {
                worktree
                    .read(cx)
                    .full_path(&project_path.path)
                    .to_string_lossy()
                    .to_string()
            } else {
                abs_path.to_string()
            };

            match fragment {
                Some(fragment) => format!("{path_label}#{fragment}").into(),
                None => path_label.into(),
            }
        } else {
            uri.clone()
        };

        let button_id = SharedString::from(format!("item-{}", uri));

        div()
            .ml(rems(0.4))
            .pl_2p5()
            .border_l_1()
            .border_color(self.tool_card_border_color(cx))
            .overflow_hidden()
            .child(
                Button::new(button_id, label)
                    .label_size(LabelSize::Small)
                    .color(Color::Muted)
                    .truncate(true)
                    .when(is_file.is_none(), |this| {
                        this.end_icon(
                            Icon::new(IconName::ArrowUpRight)
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                        )
                    })
                    .on_click(cx.listener({
                        let workspace = self.workspace.clone();
                        move |_, _, window, cx: &mut Context<Self>| {
                            open_link(uri.clone(), &workspace, window, cx);
                        }
                    })),
            )
            .into_any_element()
    }

    fn render_diff_editor(
        &self,
        entry_ix: usize,
        diff: &Entity<acp_thread::Diff>,
        tool_call: &ToolCall,
        has_failed: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        let tool_progress = matches!(
            &tool_call.status,
            ToolCallStatus::InProgress | ToolCallStatus::Pending
        );

        let revealed_diff_editor = if let Some(entry) =
            self.entry_view_state.read(cx).entry(entry_ix)
            && let Some(editor) = entry.editor_for_diff(diff)
            && diff.read(cx).has_revealed_range(cx)
        {
            Some(editor)
        } else {
            None
        };

        let show_top_border = !has_failed || revealed_diff_editor.is_some();

        v_flex()
            .h_full()
            .when(show_top_border, |this| {
                this.border_t_1()
                    .when(has_failed, |this| this.border_dashed())
                    .border_color(self.tool_card_border_color(cx))
            })
            .child(if let Some(editor) = revealed_diff_editor {
                editor.into_any_element()
            } else if tool_progress && self.as_native_connection(cx).is_some() {
                self.render_diff_loading(cx)
            } else {
                Empty.into_any()
            })
            .into_any()
    }

    fn render_markdown_output(
        &self,
        markdown: Entity<Markdown>,
        entry_ix: usize,
        context_ix: usize,
        tool_call: &ToolCall,
        card_layout: bool,
        window: &Window,
        cx: &Context<Self>,
    ) -> AnyElement {
        let mut markdown_style = MarkdownStyle::themed(MarkdownFont::Agent, window, cx);
        self.agent_panel_styling
            .tool_output
            .apply_to_markdown_style(&mut markdown_style);
        let output = self
            .render_numbered_read_file_output(
                markdown.clone(),
                entry_ix,
                context_ix,
                tool_call,
                markdown_style.clone(),
                cx,
            )
            .unwrap_or_else(|| {
                self.render_markdown(markdown, markdown_style, cx)
                    .into_any()
            });

        v_flex()
            .gap_2()
            .map(|this| {
                if card_layout {
                    this.p_2().when(context_ix > 0, |this| {
                        this.border_t_1()
                            .border_color(self.tool_card_border_color(cx))
                    })
                } else {
                    this.ml(rems(0.4))
                        .px_3p5()
                        .border_l_1()
                        .border_color(self.tool_card_border_color(cx))
                }
            })
            .text_xs()
            .text_color(cx.theme().colors().text_muted)
            .child(output)
            .into_any_element()
    }

    fn render_numbered_read_file_output(
        &self,
        markdown: Entity<Markdown>,
        entry_ix: usize,
        context_ix: usize,
        tool_call: &ToolCall,
        markdown_style: MarkdownStyle,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let is_read_file = tool_call
            .tool_name
            .as_ref()
            .is_some_and(|tool_name| tool_name.as_ref() == "read_file");
        if !is_read_file {
            return None;
        }

        let markdown = markdown.read(cx);
        let parsed = parse_cat_numbered_markdown_code_block(markdown.source())?;
        let language = markdown.first_code_block_language();
        Some(render_cat_numbered_code_block(
            parsed,
            language,
            markdown_style,
            format!("copy-read-file-output-{entry_ix}-{context_ix}"),
            cx,
        ))
    }

    fn render_image_output(
        &self,
        entry_ix: usize,
        image: Arc<gpui::Image>,
        location: Option<acp::ToolCallLocation>,
        card_layout: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        v_flex()
            .gap_2()
            .map(|this| {
                if card_layout {
                    this
                } else {
                    this.ml(rems(0.4))
                        .px_3p5()
                        .border_l_1()
                        .border_color(self.tool_card_border_color(cx))
                }
            })
            .when_some(location, |this, _loc| {
                this.child(
                    h_flex().w_full().justify_end().child(
                        Button::new(("go-to-file", entry_ix), "Go to File")
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.open_tool_call_location(entry_ix, 0, window, cx);
                            })),
                    ),
                )
            })
            .child(
                img(image)
                    .max_w_96()
                    .max_h_96()
                    .object_fit(ObjectFit::ScaleDown),
            )
            .into_any_element()
    }

    fn render_subagent_tool_call(
        &self,
        active_session_id: &acp::SessionId,
        entry_ix: usize,
        tool_call: &ToolCall,
        subagent_session_id: Option<acp::SessionId>,
        focus_handle: &FocusHandle,
        window: &Window,
        cx: &Context<Self>,
    ) -> Div {
        let subagent_thread_view = subagent_session_id.and_then(|session_id| {
            self.server_view
                .upgrade()
                .and_then(|server_view| server_view.read(cx).as_connected())
                .and_then(|connected| connected.threads.get(&session_id))
        });

        let content = self.render_subagent_card(
            active_session_id,
            entry_ix,
            subagent_thread_view,
            tool_call,
            focus_handle,
            window,
            cx,
        );

        v_flex().mx_5().my_1p5().gap_3().child(content)
    }

    fn render_subagent_card(
        &self,
        active_session_id: &acp::SessionId,
        entry_ix: usize,
        thread_view: Option<&Entity<ThreadView>>,
        tool_call: &ToolCall,
        focus_handle: &FocusHandle,
        window: &Window,
        cx: &Context<Self>,
    ) -> AnyElement {
        let thread = thread_view
            .as_ref()
            .map(|view| view.read(cx).thread.clone());
        let subagent_session_id = thread
            .as_ref()
            .map(|thread| thread.read(cx).session_id().clone());
        let action_log = thread.as_ref().map(|thread| thread.read(cx).action_log());
        let changed_buffers = action_log
            .map(|log| log.read(cx).changed_buffers(cx).collect::<Vec<_>>())
            .unwrap_or_default();

        let is_pending_tool_call = thread_view
            .as_ref()
            .and_then(|tv| {
                let sid = tv.read(cx).thread.read(cx).session_id();
                self.conversation.read(cx).pending_tool_call(sid, cx)
            })
            .is_some();

        let is_expanded = self
            .entry_view_state
            .read(cx)
            .is_tool_call_expanded(&tool_call.id);
        let files_changed = changed_buffers.len();
        let diff_stats = DiffStats::all_files(changed_buffers, cx);

        // A background spawn's call completes as soon as the subagent starts, so
        // the call's own status cannot say whether the subagent is still working
        // — what the conversation has heard from it since can. See
        // [`SubagentActivity`].
        let subagent_activity = thread_view
            .as_ref()
            .map(|thread_view| {
                let session_id = thread_view.read(cx).thread.read(cx).session_id().clone();
                self.subagent_activity_for(&session_id, cx)
            })
            .unwrap_or_default();

        let is_running = subagent_activity == SubagentActivity::Live
            || matches!(
                tool_call.status,
                ToolCallStatus::Pending
                    | ToolCallStatus::InProgress
                    | ToolCallStatus::WaitingForConfirmation { .. }
            );

        let is_failed = matches!(
            tool_call.status,
            ToolCallStatus::Failed | ToolCallStatus::Rejected
        );

        let is_cancelled = subagent_activity == SubagentActivity::Canceled
            || matches!(tool_call.status, ToolCallStatus::Canceled)
            || tool_call.content.iter().any(|c| match c {
                ToolCallContent::ContentBlock(block) => {
                    block.text_content(cx) == Some("User canceled")
                }
                _ => false,
            });

        let thread_title = thread
            .as_ref()
            .and_then(|t| t.read(cx).title())
            .filter(|t| !t.is_empty());
        let tool_call_label = tool_call.label.read(cx).source().to_string();
        let has_tool_call_label = !tool_call_label.is_empty();

        let has_title = thread_title.is_some() || has_tool_call_label;
        let has_no_title_or_canceled = !has_title || is_failed || is_cancelled;

        let title: SharedString = if let Some(thread_title) = thread_title {
            thread_title
        } else if !tool_call_label.is_empty() {
            tool_call_label.into()
        } else if is_cancelled {
            "Subagent Canceled".into()
        } else if is_failed {
            "Subagent Failed".into()
        } else {
            "Spawning Agent…".into()
        };

        let card_header_id = format!("subagent-header-{}", entry_ix);
        let status_icon = format!("status-icon-{}", entry_ix);
        let diff_stat_id = format!("subagent-diff-{}", entry_ix);

        let icon = h_flex().w_4().justify_center().child(if is_running {
            SpinnerLabel::new()
                .size(LabelSize::Small)
                .into_any_element()
        } else if is_cancelled {
            div()
                .id(status_icon)
                .child(
                    Icon::new(IconName::Circle)
                        .size(IconSize::Small)
                        .color(Color::Custom(
                            cx.theme().colors().icon_disabled.opacity(0.5),
                        )),
                )
                .tooltip(Tooltip::text("Subagent Cancelled"))
                .into_any_element()
        } else if is_failed {
            div()
                .id(status_icon)
                .child(
                    Icon::new(IconName::Close)
                        .size(IconSize::Small)
                        .color(Color::Error),
                )
                .tooltip(Tooltip::text("Subagent Failed"))
                .into_any_element()
        } else {
            Icon::new(IconName::Check)
                .size(IconSize::Small)
                .color(Color::Success)
                .into_any_element()
        });

        let has_expandable_content = thread
            .as_ref()
            .map_or(false, |thread| !thread.read(cx).entries().is_empty());

        let tooltip_meta_description = if is_expanded {
            "Click to Collapse"
        } else {
            "Click to Preview"
        };

        let error_message = self.subagent_error_message(&tool_call.status, tool_call, cx);

        v_flex()
            .w_full()
            .rounded_md()
            .border_1()
            .when(has_no_title_or_canceled, |this| this.border_dashed())
            .border_color(self.tool_card_border_color(cx))
            .overflow_hidden()
            .child(
                h_flex()
                    .group(&card_header_id)
                    .h_8()
                    .p_1()
                    .w_full()
                    .justify_between()
                    .when(!has_no_title_or_canceled, |this| {
                        this.bg(self.tool_card_header_bg(cx))
                    })
                    .child(
                        h_flex()
                            .id(format!("subagent-title-{}", entry_ix))
                            .px_1()
                            .min_w_0()
                            .size_full()
                            .gap_2()
                            .justify_between()
                            .rounded_sm()
                            .overflow_hidden()
                            .child(
                                h_flex()
                                    .min_w_0()
                                    .w_full()
                                    .gap_1p5()
                                    .child(icon)
                                    .child(
                                        Label::new(title.to_string())
                                            .size(LabelSize::Custom(self.tool_name_font_size()))
                                            .truncate(),
                                    )
                                    .when(files_changed > 0, |this| {
                                        this.child(
                                            Label::new(format!(
                                                "— {} {} changed",
                                                files_changed,
                                                if files_changed == 1 { "file" } else { "files" }
                                            ))
                                            .size(LabelSize::Custom(self.tool_name_font_size()))
                                            .color(Color::Muted),
                                        )
                                        .child(
                                            DiffStat::new(
                                                diff_stat_id.clone(),
                                                diff_stats.lines_added as usize,
                                                diff_stats.lines_removed as usize,
                                            )
                                            .label_size(LabelSize::Custom(
                                                self.tool_name_font_size(),
                                            )),
                                        )
                                    }),
                            )
                            .when(!has_no_title_or_canceled && !is_pending_tool_call, |this| {
                                this.tooltip(move |_, cx| {
                                    Tooltip::with_meta(
                                        title.to_string(),
                                        None,
                                        tooltip_meta_description,
                                        cx,
                                    )
                                })
                            })
                            .when(has_expandable_content && !is_pending_tool_call, |this| {
                                this.cursor_pointer()
                                    .hover(|s| s.bg(cx.theme().colors().element_hover))
                                    .child(
                                        div().visible_on_hover(card_header_id).child(
                                            Icon::new(if is_expanded {
                                                IconName::ChevronUp
                                            } else {
                                                IconName::ChevronDown
                                            })
                                            .color(Color::Muted)
                                            .size(IconSize::Small),
                                        ),
                                    )
                                    .on_click(cx.listener({
                                        let tool_call_id = tool_call.id.clone();
                                        move |this, _, window, cx| {
                                            let expanded =
                                                this.entry_view_state.update(cx, |state, _cx| {
                                                    state.toggle_tool_call_expansion(&tool_call_id);
                                                    state.is_tool_call_expanded(&tool_call_id)
                                                });
                                            this.refresh_thread_search(window, cx);
                                            telemetry::event!("Subagent Toggled", expanded);
                                            cx.notify();
                                        }
                                    }))
                            }),
                    )
                    .when(is_running && subagent_session_id.is_some(), |buttons| {
                        buttons.child(
                            IconButton::new(format!("stop-subagent-{}", entry_ix), IconName::Stop)
                                .icon_size(IconSize::Small)
                                .icon_color(Color::Error)
                                .tooltip(Tooltip::text("Stop Subagent"))
                                .when_some(
                                    thread_view
                                        .as_ref()
                                        .map(|view| view.read(cx).thread.clone()),
                                    |this, thread| {
                                        this.on_click(cx.listener(
                                            move |_this, _event, _window, cx| {
                                                telemetry::event!("Subagent Stopped");
                                                thread.update(cx, |thread, cx| {
                                                    thread.cancel(cx).detach();
                                                });
                                            },
                                        ))
                                    },
                                ),
                        )
                    }),
            )
            .when_some(thread_view, |this, thread_view| {
                let thread = &thread_view.read(cx).thread;
                let tv_session_id = thread.read(cx).session_id();
                let pending_tool_call = self
                    .conversation
                    .read(cx)
                    .pending_tool_call(tv_session_id, cx);

                let nav_session_id = tv_session_id.clone();

                let fullscreen_toggle = h_flex()
                    .id(entry_ix)
                    .py_1()
                    .w_full()
                    .justify_center()
                    .border_t_1()
                    .when(is_failed, |this| this.border_dashed())
                    .border_color(self.tool_card_border_color(cx))
                    .cursor_pointer()
                    .hover(|s| s.bg(cx.theme().colors().element_hover))
                    .child(
                        Icon::new(IconName::Maximize)
                            .color(Color::Muted)
                            .size(IconSize::Small),
                    )
                    .tooltip(Tooltip::text("Make Subagent Full Screen"))
                    .on_click(cx.listener(move |this, _event, window, cx| {
                        telemetry::event!("Subagent Maximized");
                        this.server_view
                            .update(cx, |this, cx| {
                                this.navigate_to_thread(nav_session_id.clone(), window, cx);
                            })
                            .ok();
                    }));

                if is_running && let Some((_, subagent_tool_call_id, _)) = pending_tool_call {
                    if let Some((entry_ix, tool_call)) =
                        thread.read(cx).tool_call(&subagent_tool_call_id)
                    {
                        this.child(Divider::horizontal().color(DividerColor::Border))
                            .child(thread_view.read(cx).render_any_tool_call(
                                active_session_id,
                                entry_ix,
                                tool_call,
                                focus_handle,
                                ToolCallLayout::Embedded,
                                window,
                                cx,
                            ))
                            .child(fullscreen_toggle)
                    } else {
                        this
                    }
                } else {
                    this.when(is_expanded, |this| {
                        this.child(self.render_subagent_expanded_content(
                            thread_view,
                            tool_call,
                            window,
                            cx,
                        ))
                        .when_some(error_message, |this, message| {
                            this.child(
                                Callout::new()
                                    .severity(Severity::Error)
                                    .icon(IconName::XCircle)
                                    .title(message),
                            )
                        })
                        .child(fullscreen_toggle)
                    })
                }
            })
            .into_any_element()
    }

    fn render_subagent_expanded_content(
        &self,
        thread_view: &Entity<ThreadView>,
        tool_call: &ToolCall,
        window: &Window,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        // Enough to scroll back through a subagent's recent work without
        // rendering an arbitrarily long transcript inside the parent — the
        // whole thing is one click away in the subagent's own thread.
        const MAX_PREVIEW_ENTRIES: usize = 50;

        let subagent_view = thread_view.read(cx);
        let session_id = subagent_view.thread.read(cx).session_id().clone();

        let is_canceled_or_failed = matches!(
            tool_call.status,
            ToolCallStatus::Canceled | ToolCallStatus::Failed | ToolCallStatus::Rejected
        );

        let editor_bg = cx.theme().colors().editor_background;
        let overlay = {
            div()
                .absolute()
                .inset_0()
                .size_full()
                .bg(linear_gradient(
                    180.,
                    linear_color_stop(editor_bg.opacity(0.5), 0.),
                    linear_color_stop(editor_bg.opacity(0.), 0.1),
                ))
                .block_mouse_except_scroll()
        };

        let entries = subagent_view.thread.read(cx).entries();
        let total_entries = entries.len();
        let mut entry_range = if let Some(info) = tool_call.subagent_session_info.as_ref() {
            info.message_start_index
                ..info
                    .message_end_index
                    .map(|i| (i + 1).min(total_entries))
                    .unwrap_or(total_entries)
        } else {
            0..total_entries
        };
        entry_range.start = entry_range
            .end
            .saturating_sub(MAX_PREVIEW_ENTRIES)
            .max(entry_range.start);
        let start_ix = entry_range.start;

        let scroll_handle = self
            .subagent_scroll_handles
            .borrow_mut()
            .entry(subagent_view.session_id.clone())
            .or_default()
            .clone();

        // Follow the subagent's output only while the user is already at the
        // bottom. Scrolling back to read something must not be yanked away by
        // the next chunk arriving, and a finished subagent should stay
        // wherever it was left.
        if is_scrolled_to_bottom(scroll_handle.offset().y, scroll_handle.max_offset().y) {
            scroll_handle.scroll_to_bottom();
        }

        let rendered_entries: Vec<AnyElement> = entries
            .get(entry_range)
            .unwrap_or_default()
            .iter()
            .enumerate()
            .map(|(i, entry)| {
                let actual_ix = start_ix + i;
                subagent_view.render_entry(actual_ix, total_entries, entry, window, cx)
            })
            .collect();

        v_flex()
            .w_full()
            .border_t_1()
            .when(is_canceled_or_failed, |this| this.border_dashed())
            .border_color(self.tool_card_border_color(cx))
            .overflow_hidden()
            .child(
                div()
                    .pb_1()
                    .min_h_0()
                    // Include the tool call id so the same subagent session
                    // rendered in multiple parent cards gets distinct element
                    // ids for its inlined entries (avoids duplicate a11y ids).
                    .id(format!(
                        "subagent-entries-{}-{}",
                        session_id, tool_call.id.0
                    ))
                    .track_scroll(&scroll_handle)
                    .overflow_y_scroll()
                    .flex_1()
                    .children(rendered_entries),
            )
            .h_56()
            .child(overlay)
            .into_any_element()
    }

    fn subagent_error_message(
        &self,
        status: &ToolCallStatus,
        tool_call: &ToolCall,
        cx: &App,
    ) -> Option<SharedString> {
        if matches!(status, ToolCallStatus::Failed) {
            tool_call.content.iter().find_map(|content| {
                if let ToolCallContent::ContentBlock(block) = content {
                    if let Some(source) = block.text_content(cx).filter(|source| !source.is_empty())
                    {
                        if source == "User canceled" {
                            return None;
                        } else {
                            return Some(SharedString::from(source));
                        }
                    }
                }
                None
            })
        } else {
            None
        }
    }

    fn tool_card_header_bg(&self, cx: &Context<Self>) -> Hsla {
        cx.theme()
            .colors()
            .element_background
            .blend(cx.theme().colors().editor_foreground.opacity(0.025))
    }

    fn tool_card_border_color(&self, cx: &Context<Self>) -> Hsla {
        cx.theme().colors().border.opacity(0.8)
    }

    fn tool_name_font_size(&self) -> Rems {
        rems_from_px(13_f32)
    }

    fn provider_by_name(name: &SharedString, cx: &App) -> Option<Arc<dyn LanguageModelProvider>> {
        LanguageModelRegistry::read_global(cx)
            .providers()
            .into_iter()
            .find(|provider| provider.name().0 == *name)
    }

    pub(crate) fn render_thread_error(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Div> {
        let callout = match self.thread_error.as_ref()? {
            ThreadError::Other { message, .. } => {
                self.render_any_thread_error(message.clone(), window, cx)
            }
            ThreadError::Refusal => self.render_refusal_error(cx),
            ThreadError::DataRetentionConsentRequired => {
                self.render_data_retention_consent_error(cx)
            }
            ThreadError::AuthenticationRequired(error) => {
                self.render_authentication_required_error(error.clone(), cx)
            }
            ThreadError::PaymentRequired => self.render_payment_required_error(cx),
            ThreadError::RateLimitExceeded { provider } => self.render_error_callout(
                "Rate Limit Reached",
                format!(
                    "{provider}'s rate limit was reached. Zed will retry automatically. \
                    You can also wait a moment and try again."
                )
                .into(),
                true,
                true,
                cx,
            ),
            ThreadError::ServerOverloaded { provider } => self.render_error_callout(
                "Provider Unavailable",
                format!(
                    "{provider}'s servers are temporarily unavailable. Zed will retry \
                    automatically. If the problem persists, check the provider's status page."
                )
                .into(),
                true,
                true,
                cx,
            ),
            ThreadError::PromptTooLarge => self.render_prompt_too_large_error(cx),
            ThreadError::NoCredentials { provider } => {
                let message = Self::provider_by_name(provider, cx)
                    .map(|provider| provider.missing_credentials_error_message())
                    .unwrap_or_else(|| {
                        format!("No credentials are configured for {provider}.").into()
                    });
                self.render_error_callout("Credentials Missing", message, false, true, cx)
            }
            ThreadError::StreamError { provider } => self.render_error_callout(
                "Connection Interrupted",
                format!(
                    "The connection to {provider}'s API was interrupted. Zed will retry \
                    automatically. If the problem persists, check your network connection."
                )
                .into(),
                true,
                true,
                cx,
            ),
            ThreadError::AuthenticationFailed { provider } => {
                let message = Self::provider_by_name(provider, cx)
                    .map(|provider| provider.authentication_error_message())
                    .unwrap_or_else(|| format!("Could not authenticate with {provider}.").into());
                self.render_error_callout("Authentication Failed", message, false, false, cx)
            }
            ThreadError::PermissionDenied { provider, message } => {
                let message: SharedString = message.clone().unwrap_or_else(|| {
                    format!("{provider} rejected the request due to insufficient permissions.")
                        .into()
                });

                self.render_error_callout("Permission Denied", message, false, false, cx)
            }
            ThreadError::RequestFailed => self.render_error_callout(
                "Request Failed",
                "The request could not be completed after multiple attempts. \
                Try again in a moment."
                    .into(),
                true,
                false,
                cx,
            ),
            ThreadError::MaxOutputTokens => self.render_error_callout(
                "Output Limit Reached",
                "The model stopped because it reached its maximum output length. \
                You can ask it to continue where it left off."
                    .into(),
                false,
                false,
                cx,
            ),
            ThreadError::NoModelSelected => self
                .render_model_not_available_error(cx)
                .unwrap_or_else(|| {
                    self.render_error_callout(
                        "No Model Selected",
                        "Select a model from the model picker below to get started.".into(),
                        false,
                        false,
                        cx,
                    )
                }),
            ThreadError::ApiError { provider } => self.render_error_callout(
                "API Error",
                format!(
                    "{provider}'s API returned an unexpected error. \
                    If the problem persists, try switching models or restarting Zed."
                )
                .into(),
                true,
                true,
                cx,
            ),
        };

        Some(div().child(callout.border_position(self.callout_border_position())))
    }

    fn render_refusal_error(&self, cx: &mut Context<'_, Self>) -> Callout {
        let model_or_agent_name = self.current_model_name(cx);
        let refusal_message = format!(
            "{} refused to respond to this prompt. \
            This can happen when a model believes the prompt violates its content policy \
            or safety guidelines, so rephrasing it can sometimes address the issue.",
            model_or_agent_name
        );

        Callout::new()
            .severity(Severity::Error)
            .title("Request Refused")
            .icon(IconName::XCircle)
            .description(refusal_message.clone())
            .actions_slot(self.create_copy_button(&refusal_message))
            .dismiss_action(self.dismiss_error_button(cx))
    }

    fn render_authentication_required_error(
        &self,
        error: SharedString,
        cx: &mut Context<Self>,
    ) -> Callout {
        Callout::new()
            .severity(Severity::Error)
            .title("Authentication Required")
            .icon(IconName::XCircle)
            .description(error.clone())
            .actions_slot(
                h_flex()
                    .gap_0p5()
                    .child(self.authenticate_button(cx))
                    .child(self.create_copy_button(error)),
            )
            .dismiss_action(self.dismiss_error_button(cx))
    }

    fn render_payment_required_error(&self, cx: &mut Context<Self>) -> Callout {
        const ERROR_MESSAGE: &str =
            "You reached your free usage limit. Upgrade to Zed Pro for more prompts.";

        Callout::new()
            .severity(Severity::Error)
            .icon(IconName::XCircle)
            .title("Free Usage Exceeded")
            .description(ERROR_MESSAGE)
            .actions_slot(
                h_flex()
                    .gap_0p5()
                    .child(self.upgrade_button(cx))
                    .child(self.create_copy_button(ERROR_MESSAGE)),
            )
            .dismiss_action(self.dismiss_error_button(cx))
    }

    fn render_error_callout(
        &self,
        title: &'static str,
        message: SharedString,
        show_retry: bool,
        show_copy: bool,
        cx: &mut Context<Self>,
    ) -> Callout {
        let can_resume = show_retry && self.thread.read(cx).can_retry(cx);
        let show_actions = can_resume || show_copy;

        Callout::new()
            .severity(Severity::Error)
            .icon(IconName::XCircle)
            .title(title)
            .description(message.clone())
            .when(show_actions, |callout| {
                callout.actions_slot(
                    h_flex()
                        .gap_0p5()
                        .when(can_resume, |this| this.child(self.retry_button(cx)))
                        .when(show_copy, |this| {
                            this.child(self.create_copy_button(message.clone()))
                        }),
                )
            })
            .dismiss_action(self.dismiss_error_button(cx))
    }

    fn render_model_not_available_error(&self, cx: &mut Context<Self>) -> Option<Callout> {
        let thread = self.as_native_thread(cx)?;

        let has_authenticated_provider =
            LanguageModelRegistry::read_global(cx).has_authenticated_provider(cx);

        let (title, description): (SharedString, SharedString) =
            match thread.read(cx).thread_model() {
                agent::ThreadModel::Ready(_) => return None,
                agent::ThreadModel::Unresolved(selected_model) => {
                    if let Some(provider) = LanguageModelRegistry::global(cx)
                        .read(cx)
                        .provider(&&selected_model.provider)
                    {
                        if !provider.is_authenticated(cx) {
                            (
                                format!("Failed to authenticate with {} provider", provider.name())
                                    .into(),
                                "Open the settings to configure the selected provider".into(),
                            )
                        } else {
                            (
                                format!("Model {} was not found", selected_model.model.0).into(),
                                "You may need to reconfigure authentication for this provider"
                                    .into(),
                            )
                        }
                    } else {
                        (
                            format!("Provider {} was not found", selected_model.provider).into(),
                            "Open the settings to configure providers".into(),
                        )
                    }
                }
                agent::ThreadModel::Unset => {
                    if has_authenticated_provider {
                        (
                            "No model selected".into(),
                            "Choose a different model or configure other providers to get started"
                                .into(),
                        )
                    } else {
                        (
                            "No model selected".into(),
                            "Configure a provider to get started".into(),
                        )
                    }
                }
            };

        let callout = Callout::new()
            .severity(Severity::Error)
            .icon(IconName::XCircle)
            .title(title)
            .description(description)
            .actions_slot(
                h_flex()
                    .gap_1()
                    .child(self.open_llm_providers_settings_button(cx))
                    .when(has_authenticated_provider, |this| {
                        this.child(self.open_model_selector_button(cx))
                    }),
            )
            .dismiss_action(self.dismiss_error_button(cx));

        Some(callout)
    }

    fn open_llm_providers_settings_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        Button::new("configure-llm-provider", "Configure Provider")
            .label_size(LabelSize::Small)
            .style(ButtonStyle::Filled)
            .on_click(cx.listener(|this, _, window, cx| {
                this.clear_thread_error(cx);
                window.dispatch_action(
                    Box::new(zed_actions::OpenSettingsAt {
                        path: "llm_providers".to_string(),
                        target: None,
                    }),
                    cx,
                );
            }))
    }

    fn open_model_selector_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        Button::new("open-model-selector", "Select Model")
            .label_size(LabelSize::Small)
            .style(ButtonStyle::Filled)
            .key_binding(KeyBinding::for_action(&ToggleModelSelector, cx))
            .on_click(cx.listener(|this, _, window, cx| {
                this.clear_thread_error(cx);
                window.dispatch_action(ToggleModelSelector.boxed_clone(), cx);
            }))
    }

    fn render_prompt_too_large_error(&self, cx: &mut Context<Self>) -> Callout {
        const MESSAGE: &str = "This conversation is too long for the model's context window. \
            Start a new thread or remove some attached files to continue.";

        Callout::new()
            .severity(Severity::Error)
            .icon(IconName::XCircle)
            .title("Context Too Large")
            .description(MESSAGE)
            .actions_slot(
                h_flex()
                    .gap_0p5()
                    .child(self.new_thread_button(cx))
                    .child(self.create_copy_button(MESSAGE)),
            )
            .dismiss_action(self.dismiss_error_button(cx))
    }

    fn retry_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        Button::new("retry", "Retry")
            .label_size(LabelSize::Small)
            .style(ButtonStyle::Filled)
            .on_click(cx.listener(|this, _, _, cx| {
                this.retry_generation(cx);
            }))
    }

    fn new_thread_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        Button::new("new_thread", "New Thread")
            .label_size(LabelSize::Small)
            .style(ButtonStyle::Filled)
            .on_click(cx.listener(|this, _, window, cx| {
                this.clear_thread_error(cx);
                window.dispatch_action(NewThread.boxed_clone(), cx);
            }))
    }

    fn upgrade_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        Button::new("upgrade", "Upgrade")
            .label_size(LabelSize::Small)
            .style(ButtonStyle::Tinted(ui::TintColor::Accent))
            .on_click(cx.listener({
                move |this, _, _, cx| {
                    this.clear_thread_error(cx);
                    cx.open_url(&zed_urls::upgrade_to_zed_pro_url(cx));
                }
            }))
    }

    fn authenticate_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        Button::new("authenticate", "Authenticate")
            .label_size(LabelSize::Small)
            .style(ButtonStyle::Filled)
            .on_click(cx.listener({
                move |this, _, window, cx| {
                    let server_view = this.server_view.clone();

                    this.clear_thread_error(cx);
                    if let Some(message) = this.in_flight_prompt.take() {
                        this.message_editor.update(cx, |editor, cx| {
                            editor.set_message(message, window, cx);
                        });
                    }
                    let connection = this.thread.read(cx).connection().clone();
                    window.defer(cx, |window, cx| {
                        ConversationView::handle_auth_required(
                            server_view,
                            AuthRequired::new(),
                            connection,
                            window,
                            cx,
                        );
                    })
                }
            }))
    }

    fn current_model_name(&self, cx: &App) -> SharedString {
        // For native agent (Zed Agent), use the specific model name (e.g., "Claude 3.5 Sonnet")
        // For ACP agents, use the agent name (e.g., "Claude Agent", "Gemini CLI")
        // This provides better clarity about what refused the request
        if self.as_native_connection(cx).is_some() {
            self.model_selector
                .clone()
                .and_then(|selector| selector.read(cx).active_model(cx))
                .map(|model| model.name.clone())
                .unwrap_or_else(|| SharedString::from("The model"))
        } else {
            // ACP agent - use the agent name (e.g., "Claude Agent", "Gemini CLI")
            self.agent_id.0.clone()
        }
    }

    fn render_any_thread_error(
        &mut self,
        error: SharedString,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Callout {
        let can_resume = self.thread.read(cx).can_retry(cx);

        let markdown = if let Some(markdown) = &self.thread_error_markdown {
            markdown.clone()
        } else {
            let markdown = cx.new(|cx| Markdown::new(error.clone(), None, None, cx));
            self.thread_error_markdown = Some(markdown.clone());
            markdown
        };

        let markdown_style =
            MarkdownStyle::themed(MarkdownFont::Agent, window, cx).with_muted_text(cx);
        let description = self
            .render_markdown(markdown, markdown_style, cx)
            .into_any_element();

        Callout::new()
            .severity(Severity::Error)
            .icon(IconName::XCircle)
            .title("An Error Happened")
            .description_slot(description)
            .actions_slot(
                h_flex()
                    .gap_0p5()
                    .when(can_resume, |this| {
                        this.child(
                            IconButton::new("retry", IconName::RotateCw)
                                .icon_size(IconSize::Small)
                                .tooltip(Tooltip::text("Retry Generation"))
                                .on_click(cx.listener(|this, _, _window, cx| {
                                    this.retry_generation(cx);
                                })),
                        )
                    })
                    .child(self.create_copy_button(error.to_string())),
            )
            .dismiss_action(self.dismiss_error_button(cx))
    }

    fn render_markdown(
        &self,
        markdown: Entity<Markdown>,
        style: MarkdownStyle,
        cx: &App,
    ) -> MarkdownElement {
        let list_state = self.list_state.clone();
        render_agent_markdown(
            markdown,
            style,
            &self.workspace,
            &self.code_span_resolver,
            cx,
        )
        // Zooming a diagram grows/shrinks its block; pause tail-following so the
        // viewport stays put instead of snapping back to the bottom. The list
        // resumes following on its own once the content returns to the bottom.
        .on_mermaid_zoom(move |_window, _cx| {
            list_state.pause_following_tail();
        })
    }

    /// `render_markdown` plus click-to-seek for reading aloud. Only assistant
    /// prose gets this: thinking, tool input and output, terminal labels,
    /// compaction summaries and errors all render through `render_markdown`
    /// too, and none of them is ever spoken, so clicking them must not seek.
    fn render_speakable_markdown(
        &self,
        markdown: Entity<Markdown>,
        style: MarkdownStyle,
        cx: &App,
    ) -> MarkdownElement {
        let element = self.render_markdown(markdown.clone(), style, cx);
        let Some(read_aloud) = self.read_aloud.clone() else {
            return element;
        };
        let settings = read_aloud::ReadAloudSettings::get_global(cx);
        let element = element.speaking_highlight_colors(settings.pill_colors);
        if !settings.click_to_seek {
            // No handler at all: the element then shows no hover band or
            // pointer cursor either — a disabled action gets no affordance.
            // The speaker buttons and mini player are unaffected.
            return element;
        }
        let thread = self.thread.clone();
        element.on_source_click(move |source_index, click_count, _window, cx| {
            // Double and triple clicks are word and line selection; leave them be.
            if click_count > 1 {
                return false;
            }
            // This handler runs inside the Markdown entity's own update (its
            // element's mouse listener), and seeking to a not-currently-speaking
            // message re-enqueues it, which reads that same entity — a
            // double-lease panic if done synchronously.
            let read_aloud = read_aloud.clone();
            let markdown = markdown.clone();
            let thread = thread.clone();
            cx.defer(move |cx| {
                // Only the newest assistant message of a still-generating
                // turn can grow; anything else the user clicks is complete —
                // and must be flagged so, or the player never hides after
                // it finishes (no later enqueue corrects the flag).
                let message_complete = thread.read(cx).status() != ThreadStatus::Generating
                    || Self::latest_assistant_markdown_in(&thread, cx)
                        .is_none_or(|(_, latest)| latest != markdown);
                read_aloud.update(cx, |read_aloud, cx| {
                    read_aloud.seek_to_source_index(&markdown, source_index, message_complete, cx);
                });
            });
            // The element already fires this only for true clicks (on the
            // mouse-up, after ruling out drags and selections), so there is
            // nothing left to block.
            false
        })
    }

    fn create_copy_button(&self, message: impl Into<String>) -> impl IntoElement {
        let message = message.into();

        CopyButton::new("copy-error-message", message).tooltip_label("Copy Error Message")
    }

    fn dismiss_error_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        IconButton::new("dismiss", IconName::Close)
            .icon_size(IconSize::Small)
            .tooltip(Tooltip::text("Dismiss"))
            .on_click(cx.listener({
                move |this, _, _, cx| {
                    this.clear_thread_error(cx);
                    cx.notify();
                }
            }))
    }

    fn render_resume_notice(_cx: &Context<Self>) -> AnyElement {
        let description = "This agent does not support viewing previous messages. However, your session will still continue from where you last left off.";

        Callout::new()
            .border_position(CalloutBorderPosition::Bottom)
            .severity(Severity::Info)
            .icon(IconName::Info)
            .title("Resumed Session")
            .description(description)
            .into_any_element()
    }

    fn render_codex_windows_warning(&self, cx: &mut Context<Self>) -> Callout {
        Callout::new()
            .border_position(self.callout_border_position())
            .icon(IconName::Warning)
            .severity(Severity::Warning)
            .title("Codex on Windows")
            .description("For best performance, run Codex in Windows Subsystem for Linux (WSL2)")
            .actions_slot(
                Button::new("open-wsl-modal", "Open in WSL").on_click(cx.listener({
                    move |_, _, _window, cx| {
                        #[cfg(windows)]
                        _window.dispatch_action(
                            zed_actions::wsl_actions::OpenWsl::default().boxed_clone(),
                            cx,
                        );
                        cx.notify();
                    }
                })),
            )
            .dismiss_action(
                IconButton::new("dismiss", IconName::Close)
                    .icon_size(IconSize::Small)
                    .icon_color(Color::Muted)
                    .tooltip(Tooltip::text("Dismiss Warning"))
                    .on_click(cx.listener({
                        move |this, _, _, cx| {
                            this.show_codex_windows_warning = false;
                            cx.notify();
                        }
                    })),
            )
    }

    fn render_skill_loading_issues(&self, cx: &mut Context<Self>) -> Vec<Callout> {
        let border_position = self.callout_border_position();

        let description_warnings = self
            .skill_loading_issues
            .iter()
            .filter(|issue| issue.kind == SkillLoadingIssueKind::DescriptionTooLong)
            .cloned()
            .collect::<Vec<_>>();

        let long_description_warning =
            self.render_skill_description_warnings(description_warnings, cx);

        let other_warnings = self
            .skill_loading_issues
            .iter()
            .filter(|issue| issue.kind != SkillLoadingIssueKind::DescriptionTooLong)
            .enumerate()
            .map(|(index, issue)| {
                let abs_path = issue.path.clone();
                let workspace = self.workspace.clone();
                let path_label = issue.path.display().to_string();
                let target = issue.clone();

                let title = match issue.kind {
                    SkillLoadingIssueKind::LoadFailed => "Skill Failed to Load",
                    SkillLoadingIssueKind::DescriptionTooLong => unreachable!(),
                    SkillLoadingIssueKind::CatalogBudgetExceeded => {
                        "Skill Omitted from Model Catalog"
                    }
                };

                Callout::new()
                    .icon(IconName::Warning)
                    .severity(Severity::Warning)
                    .title(title)
                    .description(format!("{}\n{path_label}", issue.message))
                    .actions_slot(
                        Button::new(("open-skill-file", index), "Open Skill")
                            .style(ButtonStyle::Outlined)
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(move |_, _, window, cx| {
                                let abs_path = abs_path.clone();
                                workspace
                                    .update(cx, |workspace, cx| {
                                        workspace
                                            .open_abs_path(
                                                abs_path,
                                                workspace::OpenOptions::default(),
                                                window,
                                                cx,
                                            )
                                            .detach_and_log_err(cx);
                                    })
                                    .ok();
                            })),
                    )
                    .dismiss_action(
                        IconButton::new(("dismiss-skill-issue", index), IconName::Close)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Dismiss"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.skill_loading_issues.retain(|issue| *issue != target);
                                this.dismissed_skill_loading_issues.insert(target.clone());
                                cx.notify();
                            })),
                    )
            })
            .collect::<Vec<_>>();

        long_description_warning
            .into_iter()
            .chain(other_warnings)
            .map(|callout| callout.border_position(border_position))
            .collect()
    }

    fn render_skill_description_warnings(
        &self,
        description_warnings: Vec<SkillLoadingIssue>,
        cx: &mut Context<Self>,
    ) -> Option<Callout> {
        if description_warnings.is_empty() {
            return None;
        }

        let warning_count = description_warnings.len();
        let title = if warning_count == 1 {
            "1 Skill Loaded with a Long Description".to_string()
        } else {
            format!("{warning_count} Skills Loaded with Long Descriptions")
        };

        let rows = description_warnings
            .iter()
            .enumerate()
            .map(|(index, issue)| {
                let abs_path = issue.path.clone();
                let workspace = self.workspace.clone();
                let full_path = issue.path.display().to_string();
                let file_label = skill_issue_file_label(&issue.path);

                ButtonLike::new(("skill-description-warning-file", index))
                    .full_width()
                    .child(
                        h_flex()
                            .w_full()
                            .gap_1()
                            .child(
                                Icon::new(IconName::Dash)
                                    .size(IconSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .child(Label::new(file_label).size(LabelSize::Small)),
                    )
                    .tooltip(move |_, cx| {
                        Tooltip::with_meta("Open Skill", None, full_path.clone(), cx)
                    })
                    .on_click(cx.listener(move |_, _, window, cx| {
                        let abs_path = abs_path.clone();
                        workspace
                            .update(cx, |workspace, cx| {
                                workspace
                                    .open_abs_path(
                                        abs_path,
                                        workspace::OpenOptions::default(),
                                        window,
                                        cx,
                                    )
                                    .detach_and_log_err(cx);
                            })
                            .ok();
                    }))
                    .into_any_element()
            })
            .collect::<Vec<_>>();

        let callout = Callout::new()
            .icon(IconName::Warning)
            .severity(Severity::Warning)
            .title(title)
            .description_slot(
                v_flex()
                    .gap_1()
                    .child(
                        Label::new(format!(
                            "Ensure skill descriptions are at most {MAX_SKILL_DESCRIPTION_LEN} bytes; longer ones may consume more model-context tokens."
                        ))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    )
                    .children(rows),
            );

        let targets = description_warnings;

        Some(
            callout.dismiss_action(
                IconButton::new("dismiss-skill-description-warnings", IconName::Close)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Dismiss"))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.skill_loading_issues
                            .retain(|issue| !targets.contains(issue));
                        for target in &targets {
                            this.dismissed_skill_loading_issues.insert(target.clone());
                        }
                        cx.notify();
                    })),
            ),
        )
    }

    fn render_external_source_prompt_warning(&self, cx: &mut Context<Self>) -> Callout {
        Callout::new()
            .border_position(self.callout_border_position())
            .icon(IconName::Warning)
            .severity(Severity::Warning)
            .title("Review Before Sending")
            .description("This prompt was pre-filled by an external link. Read it carefully before you submit it to the model.")
            .dismiss_action(
                IconButton::new("dismiss-external-source-prompt-warning", IconName::Close)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Dismiss Warning"))
                    .on_click(cx.listener({
                        move |this, _, _, cx| {
                            this.show_external_source_prompt_warning = false;
                            cx.notify();
                        }
                    })),
            )
    }

    fn render_multi_root_callout(&self, cx: &mut Context<Self>) -> Option<Callout> {
        if self.multi_root_callout_dismissed {
            return None;
        }

        if self.as_native_connection(cx).is_some() {
            return None;
        }

        if self
            .thread
            .read(cx)
            .connection()
            .supports_session_additional_directories()
        {
            return None;
        }

        let project = self.project.upgrade()?;
        let worktree_count = project.read(cx).visible_worktrees(cx).count();
        if worktree_count <= 1 {
            return None;
        }

        let work_dirs = self.thread.read(cx).work_dirs()?;
        let active_dir = work_dirs
            .ordered_paths()
            .next()
            .and_then(|p| p.file_name())
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| "one folder".to_string());

        Some(
            Callout::new()
                .severity(Severity::Warning)
                .icon(IconName::Warning)
                .title("This agent doesn't currently support multi-root workspaces")
                .description(format!(
                    "It currently only operates by default on \"{}\".",
                    active_dir
                ))
                .border_position(self.callout_border_position())
                .dismiss_action(
                    IconButton::new("dismiss-multi-root-callout", IconName::Close)
                        .icon_size(IconSize::Small)
                        .tooltip(Tooltip::text("Dismiss"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.multi_root_callout_dismissed = true;
                            cx.notify();
                        })),
                ),
        )
    }

    fn render_new_version_callout(&self, version: &SharedString, cx: &mut Context<Self>) -> Div {
        let server_view = self.server_view.clone();
        let has_version = !version.is_empty();
        let title = if has_version {
            "New Version Available"
        } else {
            "Agent Update Available"
        };
        let button_label = if has_version {
            format!("Update to v{}", version)
        } else {
            "Reconnect".to_string()
        };

        v_flex().w_full().justify_end().child(
            h_flex()
                .p_2()
                .pr_3()
                .w_full()
                .gap_1p5()
                .border_b_1()
                .border_color(cx.theme().colors().border)
                .bg(cx.theme().colors().element_background)
                .child(
                    h_flex()
                        .flex_1()
                        .gap_1p5()
                        .child(
                            Icon::new(IconName::Download)
                                .color(Color::Accent)
                                .size(IconSize::Small),
                        )
                        .child(Label::new(title).size(LabelSize::Small)),
                )
                .child(
                    Button::new("update-button", button_label)
                        .label_size(LabelSize::Small)
                        .style(ButtonStyle::Tinted(TintColor::Accent))
                        .on_click(move |_, window, cx| {
                            server_view
                                .update(cx, |view, cx| view.reset(window, cx))
                                .ok();
                        }),
                ),
        )
    }

    fn render_token_limit_callout(&self, cx: &mut Context<Self>) -> Option<Callout> {
        if self.token_limit_callout_dismissed || self.as_native_thread(cx).is_none() {
            return None;
        }

        let token_usage = self.thread.read(cx).token_usage()?;

        // When auto-compaction is available (the model's context window is large
        // enough), the thread is compacted automatically before it reaches the
        // limit, so there's no need to warn the user. Models with a context
        // window that's too small can't be auto-compacted, so we fall back to
        // the normal warning.
        if token_usage.max_tokens >= agent::MIN_COMPACTION_CONTEXT_WINDOW {
            return None;
        }

        let ratio = token_usage.ratio();

        let (severity, icon, title) = match ratio {
            acp_thread::TokenUsageRatio::Normal => return None,
            acp_thread::TokenUsageRatio::Warning => (
                Severity::Warning,
                IconName::Warning,
                "Thread reaching the token limit soon",
            ),
            acp_thread::TokenUsageRatio::Exceeded => (
                Severity::Error,
                IconName::XCircle,
                "Thread reached the token limit",
            ),
        };

        let description = "To continue, run /compact or start a new thread and @-mention this one";

        Some(
            Callout::new()
                .border_position(self.callout_border_position())
                .severity(severity)
                .icon(icon)
                .title(title)
                .description(description)
                .actions_slot(
                    h_flex().gap_0p5().child(
                        Button::new("start-new-thread", "Start New Thread")
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(|this, _, window, cx| {
                                let session_id = this.thread.read(cx).session_id().clone();
                                window.dispatch_action(
                                    crate::NewNativeAgentThreadFromSummary {
                                        from_session_id: session_id,
                                    }
                                    .boxed_clone(),
                                    cx,
                                );
                            })),
                    ),
                )
                .dismiss_action(self.dismiss_error_button(cx)),
        )
    }

    /// Returns the model to offer as a downgrade target when the current model
    /// requires data retention consent (e.g. Opus 4.8 for Fable).
    fn data_retention_fallback_model(&self, cx: &App) -> Option<Arc<dyn LanguageModel>> {
        let thread = self.as_native_thread(cx)?;
        let model = thread.read(cx).model()?.clone();
        let fallback_id = model.refusal_fallback_model_id()?;
        LanguageModelRegistry::read_global(cx)
            .available_models(cx)
            .find(|fallback| {
                fallback.provider_id() == model.provider_id()
                    && fallback.id().0.as_ref() == fallback_id
            })
    }

    fn render_data_retention_consent_error(&self, cx: &mut Context<Self>) -> Callout {
        let fallback_model = self.data_retention_fallback_model(cx);

        Callout::new()
            .severity(Severity::Warning)
            .icon(IconName::Warning)
            .title(format!(
                "Note: {} cannot be offered with Zero Data Retention.",
                self.current_model_name(cx)
            ))
            .description_slot(
                h_flex()
                    .gap_1()
                    .child(
                        Label::new("Anthropic will retain inference logs.")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        Button::new("data-retention-learn-more", "Learn More")
                            .label_size(LabelSize::Small)
                            .on_click(|_, _, cx| {
                                cx.open_url(DATA_RETENTION_LEARN_MORE_URL);
                            }),
                    ),
            )
            .actions_slot(
                h_flex()
                    .gap_0p5()
                    .when_some(fallback_model, |this, fallback| {
                        this.child(
                            Button::new(
                                "switch-data-retention-fallback",
                                format!("Switch to {}", fallback.name().0),
                            )
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.switch_to_data_retention_fallback_and_resend(cx);
                            })),
                        )
                    })
                    .child(
                        Button::new("accept-data-retention", "Accept")
                            .label_size(LabelSize::Small)
                            .style(ButtonStyle::Tinted(TintColor::Warning))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.accept_data_retention_and_resend(cx);
                            })),
                    ),
            )
            .dismiss_action(self.dismiss_error_button(cx))
    }

    fn accept_data_retention_and_resend(&mut self, cx: &mut Context<Self>) {
        let fs = self.thread.read(cx).project().read(cx).fs().clone();
        // Resume the failed turn only once the in-memory settings reflect
        // consent, otherwise the resent request would be rejected again.
        let completion = update_settings_file_with_completion(fs, cx, |settings, _| {
            settings
                .telemetry
                .get_or_insert_default()
                .anthropic_retention = Some(true);
        });
        cx.spawn(async move |this, cx| {
            completion.await??;
            this.update(cx, |this, cx| this.retry_generation(cx))?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn switch_to_data_retention_fallback_and_resend(&mut self, cx: &mut Context<Self>) {
        let Some(fallback) = self.data_retention_fallback_model(cx) else {
            return;
        };
        let model_id = acp_thread::AgentModelId::new(format!(
            "{}/{}",
            fallback.provider_id().0,
            fallback.id().0
        ));
        let session_id = self.thread.read(cx).session_id().clone();
        let Some(selector) = self
            .thread
            .read(cx)
            .connection()
            .model_selector(&session_id)
        else {
            return;
        };
        let select = selector.select_model(model_id, cx);
        cx.spawn(async move |this, cx| {
            select.await?;
            this.update(cx, |this, cx| this.retry_generation(cx))?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn open_permission_dropdown(
        &mut self,
        _: &crate::OpenPermissionDropdown,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let menu_handle = self.permission_dropdown_handle.clone();
        window.defer(cx, move |window, cx| {
            menu_handle.toggle(window, cx);
        });
    }

    fn open_add_context_menu(
        &mut self,
        _action: &OpenAddContextMenu,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let menu_handle = self.add_context_menu_handle.clone();
        window.defer(cx, move |window, cx| {
            menu_handle.toggle(window, cx);
        });
    }

    fn toggle_fast_mode(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.fast_mode_available(cx) {
            return;
        }

        let Some(thread) = self.as_native_thread(cx) else {
            return;
        };

        let current_speed = thread.read(cx).speed().unwrap_or_default();
        let new_speed = current_speed.toggle();

        if new_speed == Speed::Fast && self.pending_fast_mode_confirmation(cx).is_some() {
            let menu_handle = self.fast_mode_menu_handle.clone();
            window.defer(cx, move |window, cx| {
                menu_handle.toggle(window, cx);
            });
            return;
        }

        self.apply_fast_mode_speed(new_speed, cx);
    }

    fn apply_fast_mode_speed(&mut self, new_speed: Speed, cx: &mut Context<Self>) {
        let Some(thread) = self.as_native_thread(cx) else {
            return;
        };
        thread.update(cx, |thread, cx| {
            thread.set_speed(new_speed, cx);

            let favorite_key = thread
                .model()
                .map(|model| (model.provider_id().0.to_string(), model.id().0.to_string()));
            let fs = thread.project().read(cx).fs().clone();
            update_settings_file(fs, cx, move |settings, _| {
                if let Some(agent) = settings.agent.as_mut() {
                    if let Some(default_model) = agent.default_model.as_mut() {
                        default_model.speed = Some(new_speed);
                    }
                    if let Some((provider_id, model_id)) = &favorite_key {
                        agent.update_favorite_model(provider_id, model_id, |favorite| {
                            favorite.speed = Some(new_speed)
                        });
                    }
                }
            });
        });
    }

    fn cycle_native_agent_thinking_effort(&mut self, cx: &mut Context<Self>) {
        let Some(thread) = self.as_native_thread(cx) else {
            return;
        };

        let (effort_levels, current_effort) = {
            let thread_ref = thread.read(cx);
            let Some(model) = thread_ref.model() else {
                return;
            };
            if !model.supports_thinking() || !thread_ref.thinking_enabled() {
                return;
            }
            let effort_levels = model.supported_effort_levels();
            if effort_levels.is_empty() {
                return;
            }
            let current_effort = thread_ref.thinking_effort().cloned();
            (effort_levels, current_effort)
        };

        let current_index = current_effort.and_then(|current| {
            effort_levels
                .iter()
                .position(|level| level.value == current)
        });
        let next_index = match current_index {
            Some(index) => (index + 1) % effort_levels.len(),
            None => 0,
        };
        let next_effort = effort_levels[next_index].value.to_string();

        thread.update(cx, |thread, cx| {
            thread.set_thinking_effort(Some(next_effort.clone()), cx);

            let favorite_key = thread
                .model()
                .map(|model| (model.provider_id().0.to_string(), model.id().0.to_string()));
            let fs = thread.project().read(cx).fs().clone();
            update_settings_file(fs, cx, move |settings, _| {
                if let Some(agent) = settings.agent.as_mut() {
                    if let Some(default_model) = agent.default_model.as_mut() {
                        default_model.effort = Some(next_effort.clone());
                    }
                    if let Some((provider_id, model_id)) = &favorite_key {
                        agent.update_favorite_model(provider_id, model_id, |favorite| {
                            favorite.effort = Some(next_effort)
                        });
                    }
                }
            });
        });
    }
}

impl Render for ThreadView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Keep the message editor's local slash commands in sync with the
        // current availability of feedback/sharing, which can change between
        // renders (settings, connection state, feature flags).
        self.sync_local_commands(cx);

        let has_messages = self.list_state.item_count() > 0;
        let list_state = self.list_state.clone();

        let conversation = v_flex()
            .when(self.resumed_without_history, |this| {
                this.child(Self::render_resume_notice(cx))
            })
            .map(|this| {
                if has_messages {
                    this.flex_1()
                        .size_full()
                        .child(self.render_entries(cx))
                        .vertical_scrollbar_for(&list_state, window, cx)
                        .into_any()
                } else {
                    this.into_any()
                }
            });

        v_flex()
            .key_context("AcpThread")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &menu::Cancel, _, cx| {
                if this.parent_session_id.is_none() {
                    this.cancel_generation(cx);
                }
            }))
            .on_action(cx.listener(|this, _: &read_aloud::Toggle, _window, cx| {
                this.toggle_read_aloud(cx);
            }))
            .on_action(
                cx.listener(|this, _: &read_aloud::SummarizeSession, _window, cx| {
                    this.summarize_read_aloud_session(cx);
                }),
            )
            .on_action(
                cx.listener(|this, _: &read_aloud::TogglePause, _window, cx| {
                    if let Some(read_aloud) = this.read_aloud.clone() {
                        read_aloud.update(cx, |read_aloud, cx| read_aloud.toggle_pause(cx));
                    }
                }),
            )
            .on_action(cx.listener(
                |this, _: &super::thread_search_bar::DismissThreadSearch, window, cx| {
                    this.close_thread_search(window, cx);
                },
            ))
            // Esc can arrive as `editor::Cancel` from the query editor.
            .on_action(
                cx.listener(|this, _: &editor::actions::Cancel, window, cx| {
                    if !this.close_thread_search(window, cx) {
                        cx.propagate();
                    }
                }),
            )
            .on_action(cx.listener(
                |this, action: &super::thread_search_bar::SelectNextThreadMatch, window, cx| {
                    if !this.thread_search_visible {
                        cx.propagate();
                        return;
                    }
                    if let Some(bar) = this.thread_search_bar.clone() {
                        bar.update(cx, |bar, cx| bar.select_next_match(action, window, cx));
                    }
                },
            ))
            .on_action(cx.listener(
                |this, action: &super::thread_search_bar::SelectPreviousThreadMatch, window, cx| {
                    if !this.thread_search_visible {
                        cx.propagate();
                        return;
                    }
                    if let Some(bar) = this.thread_search_bar.clone() {
                        bar.update(cx, |bar, cx| bar.select_prev_match(action, window, cx));
                    }
                },
            ))
            .on_action(
                cx.listener(|this, _: &search::ToggleCaseSensitive, window, cx| {
                    if !this.thread_search_visible {
                        cx.propagate();
                        return;
                    }
                    if let Some(bar) = this.thread_search_bar.clone() {
                        bar.update(cx, |bar, cx| {
                            bar.toggle_case_sensitive(&search::ToggleCaseSensitive, window, cx)
                        });
                    }
                }),
            )
            .on_action(
                cx.listener(|this, _: &search::ToggleWholeWord, window, cx| {
                    if !this.thread_search_visible {
                        cx.propagate();
                        return;
                    }
                    if let Some(bar) = this.thread_search_bar.clone() {
                        bar.update(cx, |bar, cx| {
                            bar.toggle_whole_word(&search::ToggleWholeWord, window, cx)
                        });
                    }
                }),
            )
            .on_action(cx.listener(|this, _: &search::ToggleRegex, window, cx| {
                if !this.thread_search_visible {
                    cx.propagate();
                    return;
                }
                if let Some(bar) = this.thread_search_bar.clone() {
                    bar.update(cx, |bar, cx| {
                        bar.toggle_regex(&search::ToggleRegex, window, cx)
                    });
                }
            }))
            .on_action(
                cx.listener(|this, action: &search::FocusSearch, window, cx| {
                    if !this.thread_search_visible {
                        cx.propagate();
                        return;
                    }
                    if let Some(bar) = this.thread_search_bar.clone() {
                        bar.update(cx, |bar, cx| bar.focus_search(action, window, cx));
                    }
                }),
            )
            .on_action(cx.listener(|this, _: &workspace::GoBack, window, cx| {
                if let Some(parent_session_id) = this.thread.read(cx).parent_session_id().cloned() {
                    this.server_view
                        .update(cx, |view, cx| {
                            view.navigate_to_thread(parent_session_id, window, cx);
                        })
                        .ok();
                }
            }))
            .on_action(cx.listener(Self::keep_all))
            .on_action(cx.listener(Self::reject_all))
            .on_action(cx.listener(Self::undo_last_reject))
            .on_action(cx.listener(Self::allow_always))
            .on_action(cx.listener(Self::allow_once))
            .on_action(cx.listener(Self::reject_once))
            .on_action(cx.listener(Self::handle_authorize_tool_call))
            .on_action(cx.listener(Self::handle_select_permission_granularity))
            .on_action(cx.listener(Self::handle_toggle_command_pattern))
            .on_action(cx.listener(Self::open_permission_dropdown))
            .on_action(cx.listener(Self::open_add_context_menu))
            .on_action(cx.listener(Self::scroll_output_page_up))
            .on_action(cx.listener(Self::scroll_output_page_down))
            .on_action(cx.listener(Self::scroll_output_line_up))
            .on_action(cx.listener(Self::scroll_output_line_down))
            .on_action(cx.listener(Self::scroll_output_to_top))
            .on_action(cx.listener(Self::scroll_output_to_bottom))
            .on_action(cx.listener(Self::scroll_output_to_previous_message))
            .on_action(cx.listener(Self::scroll_output_to_next_message))
            .on_action(cx.listener(Self::toggle_search))
            .on_action(cx.listener(|this, _: &ToggleFastMode, window, cx| {
                this.toggle_fast_mode(window, cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleThinkingMode, _window, cx| {
                if this.thread.read(cx).status() != ThreadStatus::Idle {
                    return;
                }
                if let Some(thread) = this.as_native_thread(cx) {
                    thread.update(cx, |thread, cx| {
                        let model_allows_disabling = thread
                            .model()
                            .is_none_or(|model| model.supports_disabling_thinking());
                        if model_allows_disabling {
                            thread.set_thinking_enabled(!thread.thinking_enabled(), cx);
                        }
                    });
                }
            }))
            .on_action(cx.listener(|this, _: &CycleThinkingEffort, _window, cx| {
                if this.thread.read(cx).status() != ThreadStatus::Idle {
                    return;
                }
                if let Some(config_options_view) = this.config_options_view.clone() {
                    let handled = config_options_view.update(cx, |view, cx| {
                        view.cycle_category_option(
                            acp::SessionConfigOptionCategory::ThoughtLevel,
                            false,
                            cx,
                        )
                    });
                    if handled {
                        return;
                    }
                }
                this.cycle_native_agent_thinking_effort(cx);
            }))
            .on_action(
                cx.listener(|this, _: &ToggleThinkingEffortMenu, window, cx| {
                    if this.thread.read(cx).status() != ThreadStatus::Idle {
                        return;
                    }
                    if let Some(config_options_view) = this.config_options_view.clone() {
                        let handled = config_options_view.update(cx, |view, cx| {
                            view.toggle_category_picker(
                                acp::SessionConfigOptionCategory::ThoughtLevel,
                                window,
                                cx,
                            )
                        });
                        if handled {
                            return;
                        }
                    }
                    let menu_handle = this.thinking_effort_menu_handle.clone();
                    window.defer(cx, move |window, cx| {
                        menu_handle.toggle(window, cx);
                    });
                }),
            )
            .on_action(cx.listener(|this, _: &SendNextQueuedMessage, window, cx| {
                if let Some(id) = this.message_queue.first_id() {
                    this.send_queued_message_now(id, window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &RemoveFirstQueuedMessage, _, cx| {
                if let Some(id) = this.message_queue.first_id() {
                    this.remove_from_queue(id, cx);
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|this, _: &EditFirstQueuedMessage, window, cx| {
                if let Some(id) = this.message_queue.first_id() {
                    this.move_queued_message_to_main_editor(id, None, None, window, cx);
                }
            }))
            .on_action(
                cx.listener(|this, _: &ToggleSteerFirstQueuedMessage, _, cx| {
                    if this.as_native_thread(cx).is_none() {
                        return;
                    }
                    if let Some(id) = this.message_queue.first_id() {
                        this.toggle_queue_entry_steer(id, cx);
                    }
                }),
            )
            .on_action(cx.listener(|this, _: &ClearMessageQueue, _, cx| {
                this.clear_queue(cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleProfileSelector, window, cx| {
                if let Some(config_options_view) = this.config_options_view.clone() {
                    let handled = config_options_view.update(cx, |view, cx| {
                        view.toggle_category_picker(
                            acp::SessionConfigOptionCategory::Mode,
                            window,
                            cx,
                        )
                    });
                    if handled {
                        return;
                    }
                }

                if let Some(profile_selector) = this.profile_selector.clone() {
                    profile_selector.read(cx).menu_handle().toggle(window, cx);
                } else if let Some(mode_selector) = this.mode_selector.clone() {
                    mode_selector.read(cx).menu_handle().toggle(window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &CycleModeSelector, window, cx| {
                if this.thread.read(cx).status() != ThreadStatus::Idle {
                    return;
                }
                if let Some(config_options_view) = this.config_options_view.clone() {
                    let handled = config_options_view.update(cx, |view, cx| {
                        view.cycle_category_option(
                            acp::SessionConfigOptionCategory::Mode,
                            false,
                            cx,
                        )
                    });
                    if handled {
                        return;
                    }
                }

                if let Some(profile_selector) = this.profile_selector.clone() {
                    profile_selector.update(cx, |profile_selector, cx| {
                        profile_selector.cycle_profile(cx);
                    });
                } else if let Some(mode_selector) = this.mode_selector.clone() {
                    mode_selector.update(cx, |mode_selector, cx| {
                        mode_selector.cycle_mode(window, cx);
                    });
                }
            }))
            .on_action(cx.listener(|this, _: &ToggleModelSelector, window, cx| {
                if this.thread.read(cx).status() != ThreadStatus::Idle {
                    return;
                }
                if let Some(config_options_view) = this.config_options_view.clone() {
                    let handled = config_options_view.update(cx, |view, cx| {
                        view.toggle_category_picker(
                            acp::SessionConfigOptionCategory::Model,
                            window,
                            cx,
                        )
                    });
                    if handled {
                        return;
                    }
                }

                if let Some(model_selector) = this.model_selector.clone() {
                    model_selector
                        .update(cx, |model_selector, cx| model_selector.toggle(window, cx));
                }
            }))
            .on_action(cx.listener(|this, _: &CycleFavoriteModels, window, cx| {
                if this.thread.read(cx).status() != ThreadStatus::Idle {
                    return;
                }
                if let Some(config_options_view) = this.config_options_view.clone() {
                    let handled = config_options_view.update(cx, |view, cx| {
                        view.cycle_category_option(
                            acp::SessionConfigOptionCategory::Model,
                            true,
                            cx,
                        )
                    });
                    if handled {
                        return;
                    }
                }

                if let Some(model_selector) = this.model_selector.clone() {
                    model_selector.update(cx, |model_selector, cx| {
                        model_selector.cycle_favorite_models(window, cx);
                    });
                }
            }))
            .size_full()
            .children(self.render_subagent_titlebar(cx))
            .when_some(
                self.thread_search_visible
                    .then(|| self.thread_search_bar.clone())
                    .flatten(),
                |this, bar| this.child(bar),
            )
            .child(conversation)
            .children(self.render_multi_root_callout(cx))
            .children(self.render_activity_bar(window, cx))
            .when(self.show_external_source_prompt_warning, |this| {
                this.child(self.render_external_source_prompt_warning(cx))
            })
            .when(self.show_codex_windows_warning, |this| {
                this.child(self.render_codex_windows_warning(cx))
            })
            .children(self.render_skill_loading_issues(cx))
            .children(self.render_thread_retry_status_callout(cx))
            .children(self.render_thread_error(window, cx))
            .when_some(
                match has_messages {
                    true => None,
                    false => self.new_server_version_available.clone(),
                },
                |this, version| this.child(self.render_new_version_callout(&version, cx)),
            )
            .children(self.render_token_limit_callout(cx))
            .children(self.render_request_elicitations(cx))
            .children(self.render_read_aloud_mini_player(cx))
            .child(self.render_message_editor(window, cx))
    }
}

/// Whether a scroll position is at (or within a hair of) the bottom.
///
/// GPUI scroll offsets run from `0` at the top down to `-max_offset` at the
/// bottom, and `max_offset` is a positive magnitude — so the bottom is the
/// *most negative* offset the content allows, which is easy to get backwards.
pub(crate) fn is_scrolled_to_bottom(offset_y: Pixels, max_offset_y: Pixels) -> bool {
    const SLACK: Pixels = px(8.);
    offset_y <= SLACK - max_offset_y
}

pub(crate) fn open_link(
    url: SharedString,
    workspace: &WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(workspace) = workspace.upgrade() else {
        cx.open_url(&url);
        return;
    };

    let path_style = workspace.read(cx).path_style(cx);
    let (relative_path, fragment) = split_local_url_fragment(&url);
    if let Some(fragment) = fragment
        && !relative_path.is_empty()
        && !path_style.is_absolute(relative_path)
    {
        let project = workspace.read(cx).project().clone();
        let decoded_path = decode_path_escapes(relative_path);
        let abs_path = project.update(cx, |project, cx| {
            let resolve_path = |path: &str| {
                let project_path = project.find_project_path(path, cx)?;
                project.entry_for_path(&project_path, cx)?;
                project.absolute_path(&project_path, cx)
            };
            resolve_path(&decoded_path).or_else(|| resolve_path(relative_path))
        });
        if let Some(abs_path) = abs_path {
            let point = fragment
                .strip_prefix('L')
                .and_then(source_position_from_fragment)
                .map(|(row, _)| Point::new(row, 0));
            workspace.update(cx, |workspace, cx| {
                open_abs_path_at_point(workspace, abs_path, point, window, cx);
            });
            return;
        }
    }

    if let Some(mention) = MentionUri::parse_hyperlink(&url, path_style).log_err() {
        // Percent escapes in bare paths are ambiguous: prefer the decoded
        // interpretation, falling back to the literal one (e.g. a file
        // actually named `a%20b.rs`) only when the decoded path doesn't
        // resolve in the project but the literal one does.
        let resolves_in_project = |mention: &MentionUri, cx: &App| {
            mention.abs_path().is_some_and(|abs_path| {
                let project = workspace.read(cx).project().read(cx);
                project
                    .find_project_path(abs_path, cx)
                    .is_some_and(|path| project.entry_for_path(&path, cx).is_some())
            })
        };
        let mention = match MentionUri::parse_hyperlink_literal(&url, path_style) {
            Some(literal)
                if !resolves_in_project(&mention, cx) && resolves_in_project(&literal, cx) =>
            {
                literal
            }
            _ => mention,
        };
        workspace.update(cx, |workspace, cx| match mention {
            MentionUri::File { abs_path } => {
                open_abs_path_at_point(workspace, abs_path, None, window, cx);
            }
            MentionUri::PastedImage { .. } => {}
            MentionUri::Directory { abs_path } => {
                let project = workspace.project();
                let Some(entry_id) = project.update(cx, |project, cx| {
                    let path = project.find_project_path(abs_path, cx)?;
                    project.entry_for_path(&path, cx).map(|entry| entry.id)
                }) else {
                    return;
                };

                project.update(cx, |_, cx| {
                    cx.emit(project::Event::RevealInProjectPanel(entry_id));
                });
            }
            MentionUri::Symbol {
                abs_path: path,
                line_range,
                ..
            } => {
                open_abs_path_at_point(
                    workspace,
                    path,
                    Some(Point::new(*line_range.start(), 0)),
                    window,
                    cx,
                );
            }
            MentionUri::Selection {
                abs_path: Some(path),
                line_range,
                column,
            } => {
                open_abs_path_at_point(
                    workspace,
                    path,
                    Some(Point::new(*line_range.start(), column.unwrap_or(0))),
                    window,
                    cx,
                );
            }
            MentionUri::Selection { abs_path: None, .. } => {}
            MentionUri::Thread { id, name } => {
                if let Some(panel) = workspace.panel::<AgentPanel>(cx) {
                    panel.update(cx, |panel, cx| {
                        panel.open_thread(id, None, Some(name.into()), window, cx)
                    });
                }
            }
            MentionUri::Fetch { url } => {
                cx.open_url(url.as_str());
            }
            MentionUri::Diagnostics { .. } => {}
            MentionUri::TerminalSelection { .. } => {}
            MentionUri::GitDiff { .. } => {}
            MentionUri::MergeConflict { .. } => {}
            MentionUri::Rule { name, .. } => {
                crate::ui::open_migrated_rule(workspace, &name, window, cx);
            }
            MentionUri::Skill {
                skill_file_path, ..
            } => {
                workspace
                    .open_abs_path(
                        skill_file_path,
                        workspace::OpenOptions {
                            focus: Some(true),
                            ..Default::default()
                        },
                        window,
                        cx,
                    )
                    .detach_and_log_err(cx);
            }
        })
    } else {
        workspace.update(cx, |workspace, cx| {
            workspace.open_url_or_file(&url, None, window, cx);
        });
    }
}

/// Returns the name of the leading built-in (native-category) slash command —
/// e.g. `compact` for `/compact` or `/compact summarize the API work` — whether
/// or not the user typed any trailing text after it. Built-in commands ignore
/// trailing arguments, so the caller sends the bare command and queues any
/// remainder rather than discarding it. Commands from MCP servers and ACP
/// agents are excluded: their trailing text is a real argument the agent
/// consumes.
///
/// Native commands run a turn that produces its own thread entry, so the typed
/// command is never echoed as a user message (see `send_command_queueing_remainder`).
fn leading_native_command(
    text: &str,
    available_commands: &[acp::AvailableCommand],
) -> Option<String> {
    let rest = text.trim_start().strip_prefix('/')?;
    let name_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let name = &rest[..name_end];
    let is_native = available_commands.iter().any(|command| {
        command.name == name
            && acp_thread::command_category_from_meta(&command.meta)
                == Some(acp_thread::CommandCategory::Native)
    });
    is_native.then(|| name.to_string())
}

/// Removes a leading `/command_name` token from `text`, returning the trimmed
/// remainder. Falls back to the trimmed input if the prefix isn't present.
fn strip_leading_command(text: &str, command_name: &str) -> String {
    let trimmed = text.trim_start();
    trimmed
        .strip_prefix('/')
        .and_then(|rest| rest.strip_prefix(command_name))
        .map(|rest| rest.trim_start().to_string())
        .unwrap_or_else(|| trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use project::{FakeFs, Project};
    use serde_json::json;
    use std::path::Path;
    use util::path;
    use workspace::MultiWorkspace;

    /// A provider that has models but no credentials — the case that made
    /// "no model" and "a model that cannot be called" look the same.
    struct UnauthenticatedProvider(language_model::fake_provider::FakeLanguageModelProvider);

    impl language_model::LanguageModelProvider for UnauthenticatedProvider {
        fn id(&self) -> LanguageModelProviderId {
            self.0.id()
        }

        fn name(&self) -> language_model::LanguageModelProviderName {
            self.0.name()
        }

        fn default_model(&self, cx: &App) -> Option<Arc<dyn LanguageModel>> {
            self.0.default_model(cx)
        }

        fn default_fast_model(&self, cx: &App) -> Option<Arc<dyn LanguageModel>> {
            self.0.default_fast_model(cx)
        }

        fn provided_models(&self, cx: &App) -> Vec<Arc<dyn LanguageModel>> {
            self.0.provided_models(cx)
        }

        fn is_authenticated(&self, _: &App) -> bool {
            false
        }

        fn authenticate(&self, _: &mut App) -> Task<Result<(), language_model::AuthenticateError>> {
            Task::ready(Err(language_model::AuthenticateError::CredentialsNotFound))
        }

        fn settings_view(&self, _: &mut App) -> Option<language_model::ProviderSettingsView> {
            None
        }
    }

    fn fake_model(name: &str, cx: &App) -> ConfiguredModel {
        let provider = Arc::new(
            language_model::fake_provider::FakeLanguageModelProvider::new(
                LanguageModelProviderId::from(name.to_string()),
                language_model::LanguageModelProviderName::from(name.to_string()),
            ),
        );
        let model = provider
            .provided_models(cx)
            .first()
            .expect("the fake provider has a model")
            .clone();
        ConfiguredModel { provider, model }
    }

    /// One `tool_call` notification from the captured Claude Code session,
    /// rebuilt as the thread entry narration sees. Only the transport shim is
    /// written here; every field it copies is the capture's own.
    fn captured_tool_call(
        update: &serde_json::Value,
        status: ToolCallStatus,
        cx: &mut App,
    ) -> acp_thread::ToolCall {
        let title = update["title"].as_str().unwrap_or_default().to_string();
        acp_thread::ToolCall {
            id: acp::ToolCallId::new(update["toolCallId"].as_str().unwrap_or("call")),
            label: cx.new(|cx| markdown::Markdown::new(title.into(), None, None, cx)),
            kind: match update["kind"].as_str() {
                Some("read") => acp::ToolKind::Read,
                Some("execute") => acp::ToolKind::Execute,
                _ => acp::ToolKind::Other,
            },
            content: Vec::new(),
            status,
            locations: update["locations"]
                .as_array()
                .map(|locations| {
                    locations
                        .iter()
                        .filter_map(|location| location["path"].as_str())
                        .map(|path| acp::ToolCallLocation::new(PathBuf::from(path)))
                        .collect()
                })
                .unwrap_or_default(),
            resolved_locations: Vec::new(),
            raw_input: update.get("rawInput").cloned(),
            raw_input_markdown: None,
            raw_output: update.get("rawOutput").cloned(),
            tool_name: None,
            subagent_session_info: None,
            sandbox_authorization_details: None,
            sandbox_fallback_authorization_details: None,
            sandbox_not_applied: None,
        }
    }

    /// Every initial `tool_call` notification in the capture, in order.
    fn captured_arrivals() -> Vec<serde_json::Value> {
        let capture: serde_json::Value =
            serde_json::from_str(read_aloud::CLAUDE_CODE_TOOL_CALL_CAPTURE)
                .expect("the capture parses");
        capture["updates"]
            .as_array()
            .expect("the capture is a list of updates")
            .iter()
            .filter(|update| update["sessionUpdate"] == "tool_call")
            .cloned()
            .collect()
    }

    /// The bug the user heard for days, at the seam where it lived.
    ///
    /// Six of the capture's twenty-eight calls arrive with `rawInput: {}` and
    /// the title "Terminal". Their `raw_input` is `Some`, so the check that
    /// stood here for three rounds of fixes saw structured input; their
    /// status is `pending`, exactly like the twenty-two that arrived
    /// complete, so a status gate cannot separate them either. Only the
    /// emptiness of the payload can.
    #[gpui::test]
    fn the_capture_separates_contentless_arrivals_from_complete_ones(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let arrivals = captured_arrivals();
            assert_eq!(arrivals.len(), 28);
            let mut waiting = 0;
            let mut with_purpose = 0;
            for update in &arrivals {
                let tool_call = captured_tool_call(update, ToolCallStatus::Pending, cx);
                assert!(
                    tool_call.raw_input.is_some(),
                    "`is_some()` is true for all twenty-eight — it cannot be the gate"
                );
                let facts = read_aloud_tool_call_facts(&tool_call);
                if facts.awaiting_input() {
                    waiting += 1;
                    assert_eq!(update["title"], "Terminal");
                    assert!(
                        !read_aloud_tool_call_is_ready(
                            &facts,
                            &ToolCallStatus::Pending,
                            NarrationTrigger::Settled,
                            cx,
                        ),
                        "a contentless call must not be spoken when the timer expires"
                    );
                } else {
                    assert!(
                        read_aloud_tool_call_is_ready(
                            &facts,
                            &ToolCallStatus::Pending,
                            NarrationTrigger::Settled,
                            cx,
                        ),
                        "a complete call must not be delayed by the fix for the empty ones"
                    );
                }
                if facts.purpose.is_some() {
                    with_purpose += 1;
                }
            }
            assert_eq!(waiting, 6, "six arrive contentless");
            assert_eq!(
                with_purpose, 20,
                "twenty state their purpose on arrival; the other six state it \
                 in the refinement that fills the command in"
            );
        });
    }

    /// A call that never gets its payload is still spoken — late, but spoken.
    /// Silence is the one outcome this path may never produce.
    #[gpui::test]
    fn a_contentless_call_is_still_spoken_by_the_backstops(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let empty = captured_arrivals()
                .into_iter()
                .find(|update| update["rawInput"] == serde_json::json!({}))
                .expect("the capture has contentless arrivals");
            let facts = read_aloud_tool_call_facts(&captured_tool_call(
                &empty,
                ToolCallStatus::Pending,
                cx,
            ));
            assert!(facts.awaiting_input());
            assert!(
                read_aloud_tool_call_is_ready(
                    &facts,
                    &ToolCallStatus::Pending,
                    NarrationTrigger::TurnEnded,
                    cx,
                ),
                "the turn-end sweep speaks whatever is left"
            );
            assert!(
                read_aloud_tool_call_is_ready(
                    &facts,
                    &ToolCallStatus::Failed,
                    NarrationTrigger::Updated,
                    cx,
                ),
                "so does a terminal status, and a failure most of all"
            );
            assert!(
                !read_aloud_tool_call_is_ready(
                    &facts,
                    &ToolCallStatus::Pending,
                    NarrationTrigger::Updated,
                    cx,
                ),
                "but an ordinary update still waits for the label to settle"
            );
        });
    }

    /// The refinement trace the capture shows for a contentless call, driven
    /// through the same function the view calls.
    #[gpui::test]
    fn a_refined_payload_ends_the_wait_and_names_the_purpose(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let mut update = serde_json::json!({
                "toolCallId": "toolu_01TdExhzUnCcAbaJWY5HLrFH",
                "kind": "execute",
                "title": "Terminal",
                "rawInput": {},
            });
            let facts = read_aloud_tool_call_facts(&captured_tool_call(
                &update,
                ToolCallStatus::Pending,
                cx,
            ));
            assert!(facts.awaiting_input());

            update["rawInput"] = serde_json::json!({
                "command": "echo \"=== PRD counts ===\" && ls -1 docs/prds/",
                "description": "List PRD folders and contents",
            });
            let facts = read_aloud_tool_call_facts(&captured_tool_call(
                &update,
                ToolCallStatus::Pending,
                cx,
            ));
            assert!(!facts.awaiting_input());
            assert_eq!(
                facts.spoken_key(cx),
                "List PRD folders and contents",
                "the agent's own words, not a shortened `echo`"
            );
        });
    }

    /// The regression this whole rung ladder exists for: the user drives an
    /// external ACP agent, so Zed has no default model and no inline
    /// assistant model at all — but does have a small commit-message model
    /// sitting right there. Narration resolved nothing for two days and
    /// spoke templated lines the whole time.
    #[gpui::test]
    fn summary_model_resolution_prefers_small_configured_models(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let empty = cx.new(|_| LanguageModelRegistry::default());
            assert!(
                resolve_read_aloud_summary_model(&empty.read(cx), None, cx).is_none(),
                "with nothing configured there is genuinely no model"
            );

            let commit_only = cx.new(|cx| {
                let mut registry = LanguageModelRegistry::default();
                let commit = fake_model("commit", cx);
                registry.set_commit_message_model(Some(commit), cx);
                registry
            });
            assert_eq!(
                resolve_read_aloud_summary_model(&commit_only.read(cx), None, cx)
                    .map(|model| model.provider.id().0.to_string()),
                Some("commit".to_string()),
                "an install with no default model must still find the small \
                 model it does have"
            );

            let both = cx.new(|cx| {
                let mut registry = LanguageModelRegistry::default();
                let commit = fake_model("commit", cx);
                registry.set_commit_message_model(Some(commit), cx);
                let summary = fake_model("summary", cx);
                registry.set_thread_summary_model(Some(summary), cx);
                let inline = fake_model("inline", cx);
                registry.set_inline_assistant_model(Some(inline), cx);
                registry
            });
            assert_eq!(
                resolve_read_aloud_summary_model(&both.read(cx), None, cx).map(|model| model
                    .provider
                    .id()
                    .0
                    .to_string()),
                Some("summary".to_string()),
                "the thread-summary model is the closest existing job to this one"
            );

            let inline_only = cx.new(|cx| {
                let mut registry = LanguageModelRegistry::default();
                let inline = fake_model("inline", cx);
                registry.set_inline_assistant_model(Some(inline), cx);
                registry
            });
            assert_eq!(
                resolve_read_aloud_summary_model(&inline_only.read(cx), None, cx)
                    .map(|model| model.provider.id().0.to_string()),
                Some("inline".to_string()),
                "the rung this used to be must not regress"
            );
        });
    }

    /// A provider without credentials is not a model. Falling through to the
    /// next rung is what "no *authenticated* model" means.
    #[gpui::test]
    fn an_unauthenticated_provider_is_skipped(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let registry = cx.new(|cx| {
                let mut registry = LanguageModelRegistry::default();
                let unusable = fake_model("summary", cx);
                registry.set_thread_summary_model(
                    Some(ConfiguredModel {
                        provider: Arc::new(UnauthenticatedProvider(
                            language_model::fake_provider::FakeLanguageModelProvider::new(
                                LanguageModelProviderId::from("summary".to_string()),
                                language_model::LanguageModelProviderName::from(
                                    "summary".to_string(),
                                ),
                            ),
                        )),
                        model: unusable.model,
                    }),
                    cx,
                );
                let commit = fake_model("commit", cx);
                registry.set_commit_message_model(Some(commit), cx);
                registry
            });
            assert_eq!(
                resolve_read_aloud_summary_model(&registry.read(cx), None, cx).map(|model| model
                    .provider
                    .id()
                    .0
                    .to_string()),
                Some("commit".to_string()),
                "a provider with no credentials must not shadow one that works"
            );
        });
    }

    fn native_command(name: &str) -> acp::AvailableCommand {
        acp::AvailableCommand::new(name, "").meta(acp_thread::meta_with_command_category(
            acp_thread::CommandCategory::Native,
        ))
    }

    fn mcp_command(name: &str) -> acp::AvailableCommand {
        acp::AvailableCommand::new(name, "").meta(acp_thread::meta_with_command_category(
            acp_thread::CommandCategory::Mcp,
        ))
    }

    #[test]
    fn test_leading_native_command_matches_bare_and_with_remainder() {
        let commands = [native_command("compact"), mcp_command("deploy")];

        // Native command with trailing text.
        assert_eq!(
            leading_native_command("/compact summarize the API work", &commands),
            Some("compact".to_string())
        );
        // Leading/trailing whitespace is tolerated.
        assert_eq!(
            leading_native_command("  /compact   do x  ", &commands),
            Some("compact".to_string())
        );

        // Bare native command (no remainder) is still recognized, so it runs as
        // a command turn (without echoing a user message) rather than being sent
        // to the model as a normal prompt.
        assert_eq!(
            leading_native_command("/compact", &commands),
            Some("compact".to_string())
        );
        assert_eq!(
            leading_native_command("/compact   ", &commands),
            Some("compact".to_string())
        );

        // MCP/ACP commands are not native: their trailing text is a real
        // argument the agent consumes, and they echo as normal user messages.
        assert_eq!(leading_native_command("/deploy prod", &commands), None);
        assert_eq!(leading_native_command("/deploy", &commands), None);

        // Unknown command, or not a slash command at all.
        assert_eq!(leading_native_command("/unknown foo", &commands), None);
        assert_eq!(leading_native_command("just a message", &commands), None);
    }

    #[test]
    fn test_strip_leading_command() {
        assert_eq!(strip_leading_command("/compact do x", "compact"), "do x");
        assert_eq!(
            strip_leading_command("  /compact  do x ", "compact"),
            "do x "
        );
        // No matching prefix: returns the trimmed input unchanged.
        assert_eq!(strip_leading_command("hello", "compact"), "hello");
    }

    #[gpui::test]
    async fn test_open_link_bare_path(cx: &mut gpui::TestAppContext) {
        crate::test_support::init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({"src": {"main.rs": "first\nsecond\nthird\n"}}),
        )
        .await;

        let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let workspace_weak = workspace.downgrade();

        // Relative path — call from multi_workspace so the inner workspace entity is not locked
        multi_workspace.update_in(cx, |_, window, cx| {
            open_link("src/main.rs".into(), &workspace_weak, window, cx);
        });
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, cx| {
            let active = workspace
                .active_item(cx)
                .and_then(|item| item.project_path(cx))
                .expect("file should be open");
            assert!(*active.path == *"src/main.rs");
        });

        multi_workspace.update_in(cx, |_, window, cx| {
            open_link("src/main.rs#L2".into(), &workspace_weak, window, cx);
        });
        cx.run_until_parked();
        let editor = workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_item(cx)
                .and_then(|item| item.downcast::<Editor>())
                .expect("file should be open in an editor")
        });
        editor.update_in(cx, |editor, window, cx| {
            let snapshot = editor.snapshot(window, cx);
            assert_eq!(editor.selections.newest::<Point>(&snapshot).head().row, 1);
        });

        // Absolute path
        let abs_path: SharedString = path!("/project/src/main.rs").to_string().into();
        multi_workspace.update_in(cx, |_, window, cx| {
            open_link(abs_path, &workspace_weak, window, cx);
        });
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, cx| {
            let active = workspace
                .active_item(cx)
                .and_then(|item| item.project_path(cx))
                .expect("file should be open");
            assert!(*active.path == *"src/main.rs");
        });
    }

    #[gpui::test]
    async fn test_open_link_percent_escape_disambiguation(cx: &mut gpui::TestAppContext) {
        crate::test_support::init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                "a%20b.rs": "literal\nsecond\n",
                "a b.rs": "decoded\nsecond\n",
                "c d.rs": "first\nsecond\n",
                "e%20f.rs": "first\nsecond\n",
            }),
        )
        .await;

        let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let workspace_weak = workspace.downgrade();

        let open_link_and_active_path = |url: String, cx: &mut gpui::VisualTestContext| {
            multi_workspace.update_in(cx, |_, window, cx| {
                open_link(url.into(), &workspace_weak, window, cx);
            });
            cx.run_until_parked();
            workspace.read_with(cx, |workspace, cx| {
                workspace
                    .active_item(cx)
                    .and_then(|item| item.project_path(cx))
                    .expect("file should be open")
                    .path
            })
        };

        // Both interpretations exist: the decoded one wins.
        let path = open_link_and_active_path(path!("/project/a%20b.rs").to_string(), cx);
        assert_eq!(*path, *"a b.rs");

        // Only the decoded file exists.
        let path = open_link_and_active_path(path!("/project/c%20d.rs").to_string(), cx);
        assert_eq!(*path, *"c d.rs");

        // Only the literally-named file exists: fall back to it.
        let path = open_link_and_active_path(path!("/project/e%20f.rs").to_string(), cx);
        assert_eq!(*path, *"e%20f.rs");

        let path = open_link_and_active_path("a%20b.rs#L2".to_string(), cx);
        assert_eq!(*path, *"a b.rs");

        let path = open_link_and_active_path("c%20d.rs#L2".to_string(), cx);
        assert_eq!(*path, *"c d.rs");

        let path = open_link_and_active_path("e%20f.rs#L2".to_string(), cx);
        assert_eq!(*path, *"e%20f.rs");
        let editor = workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_item(cx)
                .and_then(|item| item.downcast::<Editor>())
                .expect("file should be open in an editor")
        });
        editor.update_in(cx, |editor, window, cx| {
            let snapshot = editor.snapshot(window, cx);
            assert_eq!(editor.selections.newest::<Point>(&snapshot).head().row, 1);
        });
    }

    #[gpui::test]
    async fn test_open_link_out_of_project_path(cx: &mut gpui::TestAppContext) {
        crate::test_support::init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/project"), json!({"src": {"main.rs": ""}}))
            .await;
        fs.insert_tree(path!("/outside"), json!({"notes.md": "one\ntwo\nthree\n"}))
            .await;

        let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let workspace_weak = workspace.downgrade();

        // A nonexistent out-of-project path opens nothing, not even an
        // empty buffer.
        multi_workspace.update_in(cx, |_, window, cx| {
            open_link(
                path!("/outside/missing.md").to_string().into(),
                &workspace_weak,
                window,
                cx,
            );
        });
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, cx| {
            assert!(
                workspace.active_item(cx).is_none(),
                "nothing should open for a nonexistent path"
            );
        });

        // An existing out-of-project file opens at the linked line.
        multi_workspace.update_in(cx, |_, window, cx| {
            open_link(
                format!("{}:2", path!("/outside/notes.md")).into(),
                &workspace_weak,
                window,
                cx,
            );
        });
        cx.run_until_parked();
        let editor = workspace.read_with(cx, |workspace, cx| {
            let item = workspace.active_item(cx).expect("file should be open");
            let project_path = item.project_path(cx).expect("item should have a path");
            let abs_path = workspace
                .project()
                .read(cx)
                .absolute_path(&project_path, cx);
            assert_eq!(
                abs_path.as_deref(),
                Some(Path::new(path!("/outside/notes.md")))
            );
            item.downcast::<Editor>().expect("should be an editor")
        });
        editor.update_in(cx, |editor, window, cx| {
            let snapshot = editor.snapshot(window, cx);
            assert_eq!(editor.selections.newest::<Point>(&snapshot).head().row, 1);
        });
    }

    /// The output capture, one merged record per call: what `acp_thread` hands
    /// the view once a call has finished.
    fn captured_output_calls() -> Vec<serde_json::Value> {
        let capture: serde_json::Value =
            serde_json::from_str(read_aloud::CLAUDE_CODE_TOOL_OUTPUT_CAPTURE)
                .expect("the output capture parses");
        let mut calls: Vec<(String, serde_json::Value)> = Vec::new();
        for update in capture["updates"]
            .as_array()
            .expect("the capture is a list of updates")
        {
            let id = update["toolCallId"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let index = match calls.iter().position(|(seen, _)| *seen == id) {
                Some(index) => index,
                None => {
                    calls.push((id, serde_json::json!({})));
                    calls.len() - 1
                }
            };
            let merged = &mut calls[index].1;
            for key in [
                "kind",
                "title",
                "rawInput",
                "rawOutput",
                "status",
                "locations",
            ] {
                if let Some(value) = update.get(key) {
                    merged[key] = value.clone();
                }
            }
        }
        calls.into_iter().map(|(_, call)| call).collect()
    }

    /// The seam this task exists for on the input side: `rawOutput` is where
    /// what a command *found* actually lives, and until now the view dropped
    /// it on the floor.
    #[gpui::test]
    fn a_finished_calls_output_reaches_narration(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let calls = captured_output_calls();
            assert_eq!(calls.len(), 91);
            let mut carried = 0;
            for call in &calls {
                let tool_call = captured_tool_call(call, ToolCallStatus::Completed, cx);
                let facts = read_aloud_tool_call_facts(&tool_call);
                if facts.output.is_some() {
                    carried += 1;
                }
            }
            assert!(
                carried >= 80,
                "the capture's calls carry output through the view; only {carried} did"
            );
        });
    }

    /// The line the user heard. A command that has arrived but has not been
    /// described yet must not be spoken when the settle timer fires — and
    /// must still be spoken once it is described.
    #[gpui::test]
    fn a_shell_keyword_is_not_ready_until_it_is_described(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let half_arrived = serde_json::json!({
                "toolCallId": "toolu_01LdeLBUQxPp2ir5aY8qkpwD",
                "kind": "execute",
                "title": "Terminal",
                "rawInput": {
                    "command": "for h in \"x-hostname-override: www.bartrhomes.com\" \
                                \"Host: www.localhost:3033\"; do echo \"=== $h ===\"; \
                                curl -s -o /tmp/rr.out; done",
                },
            });
            let facts = read_aloud_tool_call_facts(&captured_tool_call(
                &half_arrived,
                ToolCallStatus::Pending,
                cx,
            ));
            assert!(facts.is_shell_noise(cx), "{}", facts.spoken_key(cx));
            assert!(
                !read_aloud_tool_call_is_ready(
                    &facts,
                    &ToolCallStatus::Pending,
                    NarrationTrigger::Settled,
                    cx,
                ),
                "the settle timer must not speak \"for h in\""
            );

            let mut described = half_arrived.clone();
            described["rawInput"]["description"] =
                serde_json::json!("Test robots.txt with hostname override and Host header");
            let facts = read_aloud_tool_call_facts(&captured_tool_call(
                &described,
                ToolCallStatus::Pending,
                cx,
            ));
            assert!(!facts.is_shell_noise(cx));
            assert!(
                read_aloud_tool_call_is_ready(
                    &facts,
                    &ToolCallStatus::Pending,
                    NarrationTrigger::Settled,
                    cx,
                ),
                "and the update one message later is what speaks"
            );
            assert_eq!(
                facts.spoken_key(cx),
                "Test robots.txt with hostname override and Host header"
            );
        });
    }

    /// A description that never arrives must not resurrect the keyword at the
    /// turn-end sweep: the sweep forces readiness, and the queue is what
    /// refuses.
    #[gpui::test]
    fn the_turn_end_sweep_does_not_speak_a_shell_keyword(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let never_described = serde_json::json!({
                "toolCallId": "call",
                "kind": "execute",
                "title": "Terminal",
                "rawInput": { "command": "echo \"=== robots.ts ===\"; cat robots.ts" },
            });
            let facts = read_aloud_tool_call_facts(&captured_tool_call(
                &never_described,
                ToolCallStatus::Completed,
                cx,
            ));
            assert!(
                read_aloud_tool_call_is_ready(
                    &facts,
                    &ToolCallStatus::Completed,
                    NarrationTrigger::TurnEnded,
                    cx,
                ),
                "the sweep still forces the decision"
            );
            assert!(
                facts.is_shell_noise(cx),
                "and the decision is that there is nothing worth saying"
            );
        });
    }
}

const FAST_MODE_WARNING_NAMESPACE: &str = "fast-mode-warning-dismissed";

fn fast_mode_warning_id(
    provider_id: &LanguageModelProviderId,
    model_id: &LanguageModelId,
) -> String {
    format!("{}:{}", provider_id.0, model_id.0)
}

fn fast_mode_warning_dismissed(
    provider_id: &LanguageModelProviderId,
    model_id: &LanguageModelId,
    cx: &App,
) -> bool {
    KeyValueStore::global(cx)
        .scoped(FAST_MODE_WARNING_NAMESPACE)
        .read(&fast_mode_warning_id(provider_id, model_id))
        .log_err()
        .flatten()
        .is_some()
}

fn set_fast_mode_warning_dismissed(
    provider_id: &LanguageModelProviderId,
    model_id: &LanguageModelId,
    cx: &mut App,
) {
    let key = fast_mode_warning_id(provider_id, model_id);
    let kvp = KeyValueStore::global(cx);
    cx.background_spawn(async move {
        kvp.scoped(FAST_MODE_WARNING_NAMESPACE)
            .write(key, "1".to_string())
            .await
            .log_err();
    })
    .detach();
}

pub(crate) fn reset_fast_mode_warnings(cx: &mut App) {
    let kvp = KeyValueStore::global(cx);
    cx.background_spawn(async move {
        kvp.scoped(FAST_MODE_WARNING_NAMESPACE)
            .delete_all()
            .await
            .log_err();
    })
    .detach();
}
