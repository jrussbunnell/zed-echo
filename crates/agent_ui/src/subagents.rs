use acp_thread::{AcpThread, ToolCallStatus};
use agent_client_protocol::schema::v1 as acp;
use gpui::{App, Entity, SharedString};
use ui::{Color, IconName};

/// What a spawned subagent is currently doing, as shown in the agent panel's
/// subagent tray and in the sidebar's nested subagent rows.
///
/// This is derived from two places that can disagree: the spawning tool call in
/// the parent transcript (authoritative once the subagent finishes) and the
/// subagent's own loaded thread (authoritative while it runs). See
/// [`SubagentSummary::from_tool_call`] for how the two are reconciled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubagentStatus {
    /// Spawned, but its first turn hasn't started.
    Pending,
    Running,
    /// The subagent hit a tool call that needs the user to approve it. This
    /// blocks the subagent *and*, transitively, the parent, so it outranks
    /// every other status when both could apply.
    AwaitingApproval,
    Completed,
    Failed,
    /// Stopped by the user, or by the parent turn being canceled.
    Canceled,
}

impl SubagentStatus {
    pub fn is_active(self) -> bool {
        matches!(self, Self::Pending | Self::Running | Self::AwaitingApproval)
    }

    pub fn icon(self) -> IconName {
        match self {
            Self::Pending => IconName::TodoPending,
            Self::Running => IconName::LoadCircle,
            Self::AwaitingApproval => IconName::Warning,
            Self::Completed => IconName::Check,
            Self::Failed => IconName::XCircle,
            Self::Canceled => IconName::Close,
        }
    }

    pub fn color(self) -> Color {
        match self {
            Self::Pending => Color::Muted,
            Self::Running => Color::Accent,
            Self::AwaitingApproval => Color::Warning,
            Self::Completed => Color::Success,
            Self::Failed => Color::Error,
            Self::Canceled => Color::Muted,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Pending => "Queued",
            Self::Running => "Running",
            Self::AwaitingApproval => "Needs Approval",
            Self::Completed => "Done",
            Self::Failed => "Failed",
            Self::Canceled => "Canceled",
        }
    }
}

/// What the client has observed about a subagent that its spawning tool call
/// cannot say.
///
/// Claude Code runs a subagent in the background by default: the `Agent` call
/// returns as soon as the subagent is started, so the call reads `completed`
/// for the whole time the subagent is working. Nothing on the wire marks an
/// individual background subagent as finished — the adapter drops the SDK's
/// task frames, and the plain-text task notification the CLI feeds the parent
/// is filtered out too. What is observable is the parent's turn: it is held
/// open until every background subagent it spawned settles. So the parent going
/// idle is the end of *all* of them, and a cancel of that turn tears *all* of
/// them down.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SubagentActivity {
    /// Nothing observed beyond the spawning tool call; trust it.
    #[default]
    Unobserved,
    /// The subagent produced output after its spawning tool call had already
    /// completed, and nothing since has said it ended — so it was still working
    /// the last time anything was heard from it.
    Live,
    /// The agent reported this subagent finished.
    Finished,
    /// The agent reported this subagent failed.
    Failed,
    /// Stopped: reported killed or stopped by the agent, or the parent's turn
    /// was canceled while it was live, which tears it down agent-side.
    Canceled,
}

impl SubagentActivity {
    /// Whether this is the last word on the subagent, as opposed to a guess that
    /// a later observation can overturn.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Finished | Self::Failed | Self::Canceled)
    }
}

/// A subagent spawned by a parent thread, flattened into what the UI needs to
/// render one row or chip for it.
#[derive(Clone, Debug)]
pub struct SubagentSummary {
    pub session_id: acp::SessionId,
    /// The label the parent gave the delegated task (the spawn tool call's
    /// title), preferred over the subagent's own generated thread title because
    /// it says what the subagent was asked to do.
    pub label: SharedString,
    pub status: SubagentStatus,
    /// Tool calls in this subagent currently waiting on the user.
    pub pending_permission_count: usize,
    /// Index of the spawning tool call in the parent's entry list, for
    /// scrolling the parent transcript to it.
    pub parent_entry_index: usize,
    /// Whether the subagent's own thread has been loaded. Unloaded subagents
    /// can still be listed and navigated to; navigation loads them.
    pub is_loaded: bool,
}

impl SubagentSummary {
    /// Builds a summary from the spawning tool call plus, when it has been
    /// loaded, the subagent's own thread.
    ///
    /// Precedence matters here. A pending permission request wins outright: the
    /// subagent is stopped waiting for the user, and that is the only status
    /// that asks something of them. Otherwise the tool call's terminal states
    /// (failed, rejected, canceled, completed) win, because the tool call
    /// outlives the loaded thread and is what the transcript will show after a
    /// reload. Only while the tool call is still in flight do we fall back to
    /// the live thread's status.
    pub fn from_tool_call(
        parent_entry_index: usize,
        label: SharedString,
        session_id: acp::SessionId,
        tool_call_status: &ToolCallStatus,
        subagent_thread: Option<&Entity<AcpThread>>,
        pending_permission_count: usize,
        activity: SubagentActivity,
        cx: &App,
    ) -> Self {
        let status = if pending_permission_count > 0 {
            SubagentStatus::AwaitingApproval
        } else if activity.is_terminal() {
            // The agent said how this one ended, which outranks a spawning call
            // that says only that the subagent was started.
            match activity {
                SubagentActivity::Failed => SubagentStatus::Failed,
                SubagentActivity::Canceled => SubagentStatus::Canceled,
                _ => SubagentStatus::Completed,
            }
        } else {
            match tool_call_status {
                ToolCallStatus::Failed => SubagentStatus::Failed,
                ToolCallStatus::Rejected | ToolCallStatus::Canceled => SubagentStatus::Canceled,
                // A background spawn's call completes as soon as the subagent
                // starts, so "completed" only means finished when nothing has
                // been heard from the subagent since.
                ToolCallStatus::Completed if activity == SubagentActivity::Live => {
                    SubagentStatus::Running
                }
                ToolCallStatus::Completed => SubagentStatus::Completed,
                ToolCallStatus::Pending
                | ToolCallStatus::InProgress
                | ToolCallStatus::WaitingForConfirmation { .. } => match subagent_thread {
                    Some(thread) => match thread.read(cx).status() {
                        acp_thread::ThreadStatus::Generating => SubagentStatus::Running,
                        acp_thread::ThreadStatus::Idle => {
                            if thread.read(cx).had_error() {
                                SubagentStatus::Failed
                            } else if thread.read(cx).entries().is_empty() {
                                SubagentStatus::Pending
                            } else {
                                // Idle with a transcript but an unfinished tool
                                // call: the turn returned and the parent hasn't
                                // recorded the result yet.
                                SubagentStatus::Running
                            }
                        }
                    },
                    None => match tool_call_status {
                        ToolCallStatus::Pending => SubagentStatus::Pending,
                        _ => SubagentStatus::Running,
                    },
                },
            }
        };

        Self {
            session_id,
            label,
            status,
            pending_permission_count,
            parent_entry_index,
            is_loaded: subagent_thread.is_some(),
        }
    }
}

/// Counts of subagents by broad state, for the collapsed summary line.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SubagentCounts {
    pub total: usize,
    pub running: usize,
    pub awaiting_approval: usize,
    pub failed: usize,
}

impl SubagentCounts {
    pub fn from_statuses(statuses: impl IntoIterator<Item = SubagentStatus>) -> Self {
        let mut counts = Self::default();
        for status in statuses {
            counts.total += 1;
            match status {
                SubagentStatus::Pending | SubagentStatus::Running => counts.running += 1,
                SubagentStatus::AwaitingApproval => counts.awaiting_approval += 1,
                SubagentStatus::Failed => counts.failed += 1,
                SubagentStatus::Completed | SubagentStatus::Canceled => {}
            }
        }
        counts
    }

    pub fn from_summaries(summaries: &[SubagentSummary]) -> Self {
        Self::from_statuses(summaries.iter().map(|summary| summary.status))
    }

    /// The one-line status shown next to "Subagents" when the tray is
    /// collapsed, leading with whatever most needs attention.
    pub fn summary_label(&self) -> String {
        if self.awaiting_approval > 0 {
            format!("{} Need Approval", self.awaiting_approval)
        } else if self.running > 0 {
            format!("{} Running", self.running)
        } else if self.failed > 0 {
            format!("{} Failed", self.failed)
        } else if self.total == 1 {
            "1 Done".to_string()
        } else {
            format!("{} Done", self.total)
        }
    }

    /// The status color for the tray header, matching `summary_label`'s lead.
    pub fn summary_color(&self) -> Color {
        if self.awaiting_approval > 0 {
            Color::Warning
        } else if self.running > 0 {
            Color::Accent
        } else if self.failed > 0 {
            Color::Error
        } else {
            Color::Muted
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counts(statuses: &[SubagentStatus]) -> SubagentCounts {
        let summaries: Vec<_> = statuses
            .iter()
            .enumerate()
            .map(|(index, status)| SubagentSummary {
                session_id: acp::SessionId::new(format!("session-{index}")),
                label: "Subagent".into(),
                status: *status,
                pending_permission_count: 0,
                parent_entry_index: index,
                is_loaded: true,
            })
            .collect();
        SubagentCounts::from_summaries(&summaries)
    }

    #[test]
    fn summary_leads_with_what_needs_attention() {
        let running_and_waiting = counts(&[
            SubagentStatus::Running,
            SubagentStatus::AwaitingApproval,
            SubagentStatus::Failed,
        ]);
        assert_eq!(running_and_waiting.summary_label(), "1 Need Approval");
        assert_eq!(running_and_waiting.summary_color(), Color::Warning);

        let running = counts(&[SubagentStatus::Running, SubagentStatus::Failed]);
        assert_eq!(running.summary_label(), "1 Running");
        assert_eq!(running.summary_color(), Color::Accent);

        let failed = counts(&[SubagentStatus::Completed, SubagentStatus::Failed]);
        assert_eq!(failed.summary_label(), "1 Failed");
        assert_eq!(failed.summary_color(), Color::Error);

        let done = counts(&[SubagentStatus::Completed, SubagentStatus::Canceled]);
        assert_eq!(done.summary_label(), "2 Done");
        assert_eq!(done.summary_color(), Color::Muted);

        assert_eq!(
            counts(&[SubagentStatus::Completed]).summary_label(),
            "1 Done"
        );
    }

    #[test]
    fn pending_counts_as_running_so_the_tray_does_not_read_as_finished() {
        let just_spawned = counts(&[SubagentStatus::Pending]);
        assert_eq!(just_spawned.running, 1);
        assert_eq!(just_spawned.summary_label(), "1 Running");
    }
}
