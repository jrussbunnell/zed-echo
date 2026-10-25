use std::sync::Arc;

use database::{DatabaseEnvironment, DatabaseStore, QueryHistoryEntry};
use editor::Editor;
use futures::channel::oneshot;
use gpui::{
    App, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Task, WeakEntity, Window,
};
use picker::{Picker, PickerDelegate};
use ui::{ListItem, ListItemSpacing, prelude::*};
use util::ResultExt as _;
use workspace::{ModalView, Workspace};

use crate::results_view::ResultsView;

/// Asks the user to approve something specific: a change set, an agent's
/// query, or a writing statement on prod. Resolves `false` when dismissed.
pub struct ApprovalModal {
    title: SharedString,
    subtitle: Option<SharedString>,
    detail: Entity<Editor>,
    confirm_label: SharedString,
    environment: DatabaseEnvironment,
    response: Option<oneshot::Sender<bool>>,
    focus_handle: FocusHandle,
}

pub struct Approval {
    pub title: SharedString,
    pub subtitle: Option<SharedString>,
    pub detail: String,
    pub confirm_label: SharedString,
    pub environment: DatabaseEnvironment,
}

impl ApprovalModal {
    /// Opens the modal in `workspace` and resolves with the user's answer.
    pub fn ask(
        workspace: &mut Workspace,
        approval: Approval,
        window: &mut Window,
        cx: &mut gpui::Context<Workspace>,
    ) -> oneshot::Receiver<bool> {
        let (sender, receiver) = oneshot::channel();
        workspace.toggle_modal(window, cx, |window, cx| {
            let detail = cx.new(|cx| {
                let mut editor = Editor::auto_height(1, 24, window, cx);
                editor.set_text(approval.detail, window, cx);
                editor.set_read_only(true);
                editor.set_show_gutter(false, cx);
                editor
            });
            Self {
                title: approval.title,
                subtitle: approval.subtitle,
                detail,
                confirm_label: approval.confirm_label,
                environment: approval.environment,
                response: Some(sender),
                focus_handle: cx.focus_handle(),
            }
        });
        receiver
    }

    fn respond(&mut self, approved: bool, cx: &mut gpui::Context<Self>) {
        if let Some(response) = self.response.take() {
            response.send(approved).ok();
        }
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &menu::Confirm, _: &mut Window, cx: &mut gpui::Context<Self>) {
        self.respond(true, cx);
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut gpui::Context<Self>) {
        self.respond(false, cx);
    }
}

impl Drop for ApprovalModal {
    fn drop(&mut self) {
        if let Some(response) = self.response.take() {
            response.send(false).ok();
        }
    }
}

impl EventEmitter<DismissEvent> for ApprovalModal {}
impl ModalView for ApprovalModal {}

impl Focusable for ApprovalModal {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ApprovalModal {
    fn render(&mut self, _: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let is_prod = self.environment == DatabaseEnvironment::Prod;
        v_flex()
            .key_context("DatabaseApproval")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .elevation_3(cx)
            .w(rems(40.))
            .overflow_hidden()
            .child(
                v_flex()
                    .p_3()
                    .gap_1()
                    .border_b_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(
                        h_flex()
                            .gap_2()
                            .when(is_prod, |this| {
                                this.child(Icon::new(IconName::Warning).color(Color::Error))
                            })
                            .child(Headline::new(self.title.clone()).size(HeadlineSize::Small)),
                    )
                    .when_some(self.subtitle.clone(), |this, subtitle| {
                        this.child(Label::new(subtitle).color(Color::Muted).size(LabelSize::Small))
                    }),
            )
            .child(
                div()
                    .p_2()
                    .bg(cx.theme().colors().editor_background)
                    .child(self.detail.clone()),
            )
            .child(
                h_flex()
                    .p_2()
                    .gap_2()
                    .justify_end()
                    .border_t_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(
                        Button::new("reject", "Reject")
                            .key_binding(ui::KeyBinding::for_action_in(
                                &menu::Cancel,
                                &self.focus_handle,
                                cx,
                            ))
                            .on_click(cx.listener(|this, _, _, cx| this.respond(false, cx))),
                    )
                    .child(
                        Button::new("approve", self.confirm_label.clone())
                            .style(if is_prod {
                                ButtonStyle::Tinted(ui::TintColor::Error)
                            } else {
                                ButtonStyle::Filled
                            })
                            .key_binding(ui::KeyBinding::for_action_in(
                                &menu::Confirm,
                                &self.focus_handle,
                                cx,
                            ))
                            .on_click(cx.listener(|this, _, _, cx| this.respond(true, cx))),
                    ),
            )
    }
}

/// Collects a connection's password or key and stores it in the keychain.
pub struct SecretModal {
    store: Entity<DatabaseStore>,
    connection: Arc<str>,
    label: SharedString,
    editor: Entity<Editor>,
    error: Option<SharedString>,
    saving: Option<Task<()>>,
}

impl SecretModal {
    pub fn new(
        store: Entity<DatabaseStore>,
        connection: Arc<str>,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) -> Self {
        let label: SharedString = store
            .read(cx)
            .entry(&connection)
            .map(|entry| entry.spec.secret_label())
            .unwrap_or("Password")
            .into();
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text(&label, window, cx);
            editor.set_masked(true, cx);
            editor
        });
        Self {
            store,
            connection,
            label,
            editor,
            error: None,
            saving: None,
        }
    }

    fn confirm(&mut self, _: &menu::Confirm, _: &mut Window, cx: &mut gpui::Context<Self>) {
        let secret = self.editor.read(cx).text(cx);
        if secret.is_empty() {
            return;
        }
        let task = self.store.update(cx, |store, cx| {
            store.set_secret(&self.connection, secret, cx)
        });
        self.saving = Some(cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.saving = None;
                match result {
                    Ok(()) => cx.emit(DismissEvent),
                    Err(error) => {
                        this.error = Some(format!("{error:#}").into());
                        cx.notify();
                    }
                }
            })
            .log_err();
        }));
        cx.notify();
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut gpui::Context<Self>) {
        cx.emit(DismissEvent);
    }
}

impl EventEmitter<DismissEvent> for SecretModal {}
impl ModalView for SecretModal {}

impl Focusable for SecretModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Render for SecretModal {
    fn render(&mut self, _: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("DatabaseSecret")
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .elevation_3(cx)
            .w(rems(28.))
            .p_3()
            .gap_2()
            .child(Headline::new(format!("{}: {}", self.connection, self.label)).size(HeadlineSize::Small))
            .child(
                div()
                    .p_1()
                    .border_1()
                    .rounded_sm()
                    .border_color(cx.theme().colors().border)
                    .child(self.editor.clone()),
            )
            .child(
                Label::new("Stored in the system keychain, never in settings.")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .when(self.saving.is_some(), |this| {
                this.child(Label::new("Connecting…").size(LabelSize::Small).color(Color::Muted))
            })
            .when_some(self.error.clone(), |this, error| {
                this.child(Label::new(error).size(LabelSize::Small).color(Color::Error))
            })
    }
}

pub struct QueryHistoryDelegate {
    workspace: WeakEntity<Workspace>,
    store: Entity<DatabaseStore>,
    entries: Vec<QueryHistoryEntry>,
    matches: Vec<usize>,
    selected: usize,
}

impl QueryHistoryDelegate {
    pub fn new(workspace: WeakEntity<Workspace>, store: Entity<DatabaseStore>, cx: &App) -> Self {
        let entries = store.read(cx).query_history().to_vec();
        let matches = (0..entries.len()).collect();
        Self {
            workspace,
            store,
            entries,
            matches,
            selected: 0,
        }
    }
}

impl PickerDelegate for QueryHistoryDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "database query history"
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected
    }

    fn set_selected_index(&mut self, ix: usize, _: &mut Window, _: &mut gpui::Context<Picker<Self>>) {
        self.selected = ix;
    }

    fn placeholder_text(&self, _: &mut Window, _: &mut App) -> Arc<str> {
        "Search queries…".into()
    }

    fn update_matches(
        &mut self,
        query: String,
        _: &mut Window,
        _: &mut gpui::Context<Picker<Self>>,
    ) -> Task<()> {
        let query = query.to_lowercase();
        self.matches = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                query.is_empty()
                    || entry.text.to_lowercase().contains(&query)
                    || entry.connection.to_lowercase().contains(&query)
            })
            .map(|(index, _)| index)
            .collect();
        self.selected = 0;
        Task::ready(())
    }

    fn confirm(&mut self, _: bool, window: &mut Window, cx: &mut gpui::Context<Picker<Self>>) {
        let Some(entry) = self
            .matches
            .get(self.selected)
            .and_then(|index| self.entries.get(*index))
            .cloned()
        else {
            return;
        };
        let store = self.store.clone();
        self.workspace
            .update(cx, |workspace, cx| {
                ResultsView::open_query(workspace, store, entry.connection.into(), Some(entry.text), window, cx);
            })
            .log_err();
        cx.emit(DismissEvent);
    }

    fn dismissed(&mut self, _: &mut Window, cx: &mut gpui::Context<Picker<Self>>) {
        cx.emit(DismissEvent);
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _: &mut Window,
        _: &mut gpui::Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let entry = self.entries.get(*self.matches.get(ix)?)?;
        let when = chrono::DateTime::from_timestamp(entry.ran_at, 0)
            .map(|time| time.with_timezone(&chrono::Local).format("%b %-d %H:%M").to_string())
            .unwrap_or_default();
        let outcome = match (&entry.rows, &entry.error) {
            (_, Some(_)) => "failed".to_string(),
            (Some(rows), None) => format!("{rows} rows"),
            (None, None) => String::new(),
        };
        let first_line = entry.text.lines().next().unwrap_or_default().to_string();
        Some(
            ListItem::new(ix)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(
                    v_flex()
                        .child(Label::new(first_line).single_line().truncate())
                        .child(
                            Label::new(format!("{} · {when} · {outcome}", entry.connection))
                                .size(LabelSize::Small)
                                .color(if entry.error.is_some() { Color::Error } else { Color::Muted }),
                        ),
                ),
        )
    }
}
