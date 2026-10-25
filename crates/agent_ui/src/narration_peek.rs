use std::time::Duration;

use acp_thread::{AcpThread, ThreadStatus};
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Subscription, Task,
    Window, prelude::*,
};
use ui::{Modal, ModalHeader, Section, prelude::*};
use workspace::{ModalView, Workspace};

const NARRATION_LIMIT: usize = 12;

/// A live view of what an agent has been saying and thinking, without the
/// tool calls between, and of what it is doing right now.
pub struct NarrationPeek {
    thread: Entity<AcpThread>,
    focus_handle: FocusHandle,
    _observe_thread: Subscription,
    /// Keeps the elapsed time of the current tool call moving.
    _tick: Task<()>,
}

pub fn open_narration_peek(
    workspace: &mut Workspace,
    thread: Entity<AcpThread>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    workspace.toggle_modal(window, cx, |_window, cx| NarrationPeek::new(thread, cx));
}

impl NarrationPeek {
    fn new(thread: Entity<AcpThread>, cx: &mut Context<Self>) -> Self {
        let tick = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        });
        Self {
            _observe_thread: cx.observe(&thread, |_, _, cx| cx.notify()),
            thread,
            focus_handle: cx.focus_handle(),
            _tick: tick,
        }
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }
}

/// `45s`, `2m 14s`, `1h 3m`.
pub fn format_elapsed(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m {}s", seconds / 60, seconds % 60),
        _ => format!("{}h {}m", seconds / 3600, seconds % 3600 / 60),
    }
}

/// What the agent is doing right now, in a few words, or `None` when idle.
pub fn current_activity(thread: &AcpThread, cx: &App) -> Option<SharedString> {
    if let Some(tool_call) = thread.current_tool_call() {
        let label = tool_call.label.read(cx).source();
        return Some(label.lines().next().unwrap_or_default().trim().to_string().into());
    }
    (thread.status() == ThreadStatus::Generating).then(|| "Thinking…".into())
}

impl EventEmitter<DismissEvent> for NarrationPeek {}

impl Focusable for NarrationPeek {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ModalView for NarrationPeek {}

impl Render for NarrationPeek {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let thread = self.thread.read(cx);
        let title = thread.title().unwrap_or_else(|| "Agent".into());
        let narration = thread.recent_narration(NARRATION_LIMIT, cx);
        let activity = current_activity(thread, cx).map(|activity| {
            match thread.current_tool_call() {
                Some(tool_call) => format!(
                    "{activity} · {}",
                    format_elapsed(tool_call.started_at.elapsed())
                )
                .into(),
                None => activity,
            }
        });

        let items = narration.into_iter().enumerate().map(|(ix, item)| {
            v_flex()
                .id(("narration", ix))
                .gap_0p5()
                .py_1()
                .child(
                    Label::new(if item.is_thought { "Thought" } else { "Said" })
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .child(
                    div()
                        .text_sm()
                        .when(item.is_thought, |this| {
                            this.italic().text_color(cx.theme().colors().text_muted)
                        })
                        .child(item.text),
                )
        });

        v_flex()
            .id("narration-peek")
            .key_context("NarrationPeek")
            .w(rems(40.))
            .elevation_3(cx)
            .overflow_hidden()
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::cancel))
            .child(
                Modal::new("narration-peek-modal", None)
                    .header(
                        ModalHeader::new()
                            .headline(title)
                            .description("Recent thoughts and messages, without the tool calls")
                            .show_dismiss_button(true),
                    )
                    .section(
                        Section::new().child(
                            v_flex()
                                .id("narration-peek-list")
                                .max_h(rems(28.))
                                .overflow_y_scroll()
                                .children(items),
                        ),
                    )
                    .section(
                        Section::new().child(
                            h_flex()
                                .gap_1()
                                .child(Label::new("Now:").size(LabelSize::Small).color(Color::Muted))
                                .child(
                                    Label::new(activity.unwrap_or_else(|| "Idle".into()))
                                        .size(LabelSize::Small)
                                        .truncate(),
                                ),
                        ),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_elapsed() {
        assert_eq!(format_elapsed(Duration::from_secs(45)), "45s");
        assert_eq!(format_elapsed(Duration::from_secs(134)), "2m 14s");
        assert_eq!(format_elapsed(Duration::from_secs(3780)), "1h 3m");
    }
}
