use std::{ops::Range, sync::Arc, time::Instant};

use anyhow::{Result, bail};
use collections::{HashMap, HashSet};
use database::{
    BrowseRequest, Capability, Change, ChangeSet, CollectionRef, CollectionSchema, ConnectionState,
    DatabaseEnvironment, DatabaseKind, DatabaseStore, DatabaseStoreEvent, Filter, FilterOp,
    HistoryEntry, QueryLanguage, Reference, ResultSet, Value, is_reading_statement,
};
use editor::{Editor, EditorEvent};
use gpui::{
    AbsoluteLength, App, ClickEvent, Entity, EventEmitter, FocusHandle, Focusable, Subscription,
    Task, WeakEntity, Window, actions, px,
};
use ui::{
    ColumnWidthConfig, ResizableColumnsState, Table, TableInteractionState, TableResizeBehavior,
    Tooltip, prelude::*,
};
use util::ResultExt as _;
use workspace::{Item, Workspace, item::ItemEvent};

use crate::modals::{Approval, ApprovalModal};

actions!(
    database,
    [
        /// Runs the query in the active database query tab.
        RunQuery,
    ]
);

const QUERY_LIMIT: usize = 1000;
const PAGE_SIZE: usize = 200;

enum Mode {
    Query {
        editor: Entity<Editor>,
    },
    Table {
        collection: CollectionRef,
        filter_editor: Entity<Editor>,
        filters: Vec<Filter>,
        /// Cursors of the pages before the current one, for going back.
        previous: Vec<Option<String>>,
        current: Option<String>,
        next: Option<String>,
    },
}

enum Status {
    Idle,
    Running,
    Done { elapsed_ms: u128 },
    Failed(SharedString),
}

struct Editing {
    row: usize,
    column: usize,
    editor: Entity<Editor>,
    _subscription: Subscription,
}

enum RowHistory {
    Loading,
    Loaded(Vec<HistoryEntry>),
    Failed(SharedString),
}

/// A query tab or a table browser: a results grid with editing and row
/// detail.
pub struct ResultsView {
    store: Entity<DatabaseStore>,
    workspace: WeakEntity<Workspace>,
    connection: Arc<str>,
    mode: Mode,
    focus_handle: FocusHandle,
    results: Option<ResultSet>,
    schema: Option<CollectionSchema>,
    status: Status,
    selected: Option<(usize, usize)>,
    editing: Option<Editing>,
    pending: HashMap<(usize, usize), Value>,
    deleted: HashSet<usize>,
    inserted: Vec<Vec<Value>>,
    history: Option<(usize, RowHistory)>,
    table_state: Entity<TableInteractionState>,
    column_widths: Entity<ResizableColumnsState>,
    task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl ResultsView {
    pub fn open_query(
        workspace: &mut Workspace,
        store: Entity<DatabaseStore>,
        connection: Arc<str>,
        text: Option<String>,
        window: &mut Window,
        cx: &mut gpui::Context<Workspace>,
    ) -> Entity<Self> {
        let workspace_handle = cx.entity().downgrade();
        let languages = workspace.project().read(cx).languages().clone();
        let view = cx.new(|cx| {
            let editor = cx.new(|cx| {
                let mut editor = Editor::auto_height(4, 16, window, cx);
                editor.set_placeholder_text("SELECT … (⌘↵ to run)", window, cx);
                editor.set_show_gutter(false, cx);
                if let Some(text) = &text {
                    editor.set_text(text.as_str(), window, cx);
                }
                editor
            });
            set_sql_language(&editor, languages, cx);
            Self::new(
                store,
                workspace_handle,
                connection,
                Mode::Query { editor },
                cx,
            )
        });
        workspace.add_item_to_active_pane(Box::new(view.clone()), None, true, window, cx);
        if text.is_some() {
            view.update(cx, |view, cx| view.run(window, cx));
        }
        view
    }

    pub fn open_table(
        workspace: &mut Workspace,
        store: Entity<DatabaseStore>,
        connection: Arc<str>,
        collection: CollectionRef,
        filters: Vec<Filter>,
        window: &mut Window,
        cx: &mut gpui::Context<Workspace>,
    ) -> Entity<Self> {
        let workspace_handle = cx.entity().downgrade();
        let view = cx.new(|cx| {
            let filter_editor = cx.new(|cx| {
                let mut editor = Editor::single_line(window, cx);
                editor.set_placeholder_text("Filter: email = 'a@x.com' and id > 10", window, cx);
                editor.set_text(format_filters(&filters), window, cx);
                editor
            });
            Self::new(
                store,
                workspace_handle,
                connection,
                Mode::Table {
                    collection,
                    filter_editor,
                    filters,
                    previous: Vec::new(),
                    current: None,
                    next: None,
                },
                cx,
            )
        });
        workspace.add_item_to_active_pane(Box::new(view.clone()), None, true, window, cx);
        view.update(cx, |view, cx| {
            view.load_schema(cx);
            view.run(window, cx);
        });
        view
    }

    fn new(
        store: Entity<DatabaseStore>,
        workspace: WeakEntity<Workspace>,
        connection: Arc<str>,
        mode: Mode,
        cx: &mut gpui::Context<Self>,
    ) -> Self {
        let subscriptions = vec![cx.subscribe(&store, |this, _, event, cx| {
            if let DatabaseStoreEvent::StateChanged(name) = event
                && *name == this.connection
            {
                cx.notify();
            }
        })];
        Self {
            store,
            workspace,
            connection,
            mode,
            focus_handle: cx.focus_handle(),
            results: None,
            schema: None,
            status: Status::Idle,
            selected: None,
            editing: None,
            pending: HashMap::default(),
            deleted: HashSet::default(),
            inserted: Vec::new(),
            history: None,
            table_state: cx.new(|cx| TableInteractionState::new(cx)),
            column_widths: cx.new(|_| {
                ResizableColumnsState::new(1, vec![px(160.)], vec![TableResizeBehavior::Resizable])
            }),
            task: None,
            _subscriptions: subscriptions,
        }
    }

    fn environment(&self, cx: &App) -> DatabaseEnvironment {
        self.store
            .read(cx)
            .entry(&self.connection)
            .map(|entry| entry.environment)
            .unwrap_or(DatabaseEnvironment::Dev)
    }

    fn kind(&self, cx: &App) -> Option<DatabaseKind> {
        self.store
            .read(cx)
            .entry(&self.connection)
            .map(|entry| entry.spec.kind)
    }

    fn is_writable(&self, cx: &App) -> bool {
        let capability_writable = matches!(
            self.store.read(cx).state(&self.connection),
            ConnectionState::Connected {
                capability: Capability::Writable,
                ..
            }
        );
        capability_writable
            && matches!(self.mode, Mode::Table { .. })
            && self.schema.as_ref().is_some_and(|schema| {
                schema.writable != Some(false) && !schema.primary_key.is_empty()
            })
    }

    fn has_changes(&self) -> bool {
        !self.pending.is_empty() || !self.deleted.is_empty() || !self.inserted.is_empty()
    }

    fn load_schema(&mut self, cx: &mut gpui::Context<Self>) {
        let Mode::Table { collection, .. } = &self.mode else {
            return;
        };
        let task = self.store.update(cx, |store, cx| {
            store.describe(&self.connection, collection, cx)
        });
        cx.spawn(async move |this, cx| {
            let schema = task.await;
            this.update(cx, |this, cx| {
                match schema {
                    Ok(schema) => this.schema = Some(schema),
                    Err(error) => log::warn!("describing table: {error:#}"),
                }
                cx.notify();
            })
            .log_err();
        })
        .detach();
    }

    pub fn run(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) {
        let numbers_are_floats = self.kind(cx) == Some(DatabaseKind::Convex);
        match &mut self.mode {
            Mode::Query { editor } => {
                let text = editor.read(cx).text(cx);
                if text.trim().is_empty() {
                    return;
                }
                let kind = self.kind(cx).unwrap_or(DatabaseKind::Mysql);
                let needs_confirmation = self.environment(cx) == DatabaseEnvironment::Prod
                    && !is_reading_statement(kind, &text);
                let approval = needs_confirmation.then(|| {
                    self.ask(
                        Approval {
                            title: format!("Run a writing statement on {}?", self.connection)
                                .into(),
                            subtitle: Some("This connection is tagged prod.".into()),
                            detail: text.clone(),
                            confirm_label: "Run".into(),
                            environment: DatabaseEnvironment::Prod,
                        },
                        window,
                        cx,
                    )
                });
                let connection = self.connection.clone();
                let store = self.store.clone();
                self.status = Status::Running;
                let started = Instant::now();
                self.task = Some(cx.spawn(async move |this, cx| {
                    if let Some(approval) = approval
                        && !approval.await.unwrap_or(false)
                    {
                        this.update(cx, |this, cx| {
                            this.status = Status::Idle;
                            cx.notify();
                        })
                        .log_err();
                        return;
                    }
                    let task = store.update(cx, |store, cx| {
                        store.query(&connection, text, QUERY_LIMIT, false, cx)
                    });
                    let result = task.await;
                    this.update(cx, |this, cx| this.show(result, started, cx))
                        .log_err();
                }));
            }
            Mode::Table {
                filter_editor,
                filters,
                current,
                ..
            } => {
                match parse_filters(&filter_editor.read(cx).text(cx), numbers_are_floats) {
                    Ok(parsed) => *filters = parsed,
                    Err(error) => {
                        self.status = Status::Failed(format!("{error:#}").into());
                        cx.notify();
                        return;
                    }
                }
                let cursor = current.clone();
                self.load_page(cursor, cx);
            }
        }
        cx.notify();
    }

    fn load_page(&mut self, cursor: Option<String>, cx: &mut gpui::Context<Self>) {
        let Mode::Table {
            collection,
            filters,
            ..
        } = &self.mode
        else {
            return;
        };
        let task = self.store.update(cx, |store, cx| {
            store.browse(
                &self.connection,
                collection,
                BrowseRequest {
                    filters: filters.clone(),
                    cursor: cursor.clone(),
                    page_size: PAGE_SIZE,
                },
                cx,
            )
        });
        self.status = Status::Running;
        let started = Instant::now();
        self.task = Some(cx.spawn(async move |this, cx| {
            let page = task.await;
            this.update(cx, |this, cx| {
                let result = page.map(|page| {
                    if let Mode::Table { current, next, .. } = &mut this.mode {
                        *current = cursor;
                        *next = page.next_cursor;
                    }
                    page.rows
                });
                this.show(result, started, cx);
            })
            .log_err();
        }));
        cx.notify();
    }

    fn show(&mut self, result: Result<ResultSet>, started: Instant, cx: &mut gpui::Context<Self>) {
        match result {
            Ok(rows) => {
                let columns = rows.columns.len().max(1);
                self.column_widths = cx.new(|_| {
                    ResizableColumnsState::new(
                        columns,
                        vec![AbsoluteLength::Pixels(px(160.)); columns],
                        vec![TableResizeBehavior::Resizable; columns],
                    )
                });
                self.results = Some(rows);
                self.status = Status::Done {
                    elapsed_ms: started.elapsed().as_millis(),
                };
            }
            Err(error) => self.status = Status::Failed(format!("{error:#}").into()),
        }
        self.selected = None;
        self.editing = None;
        self.history = None;
        self.pending.clear();
        self.deleted.clear();
        self.inserted.clear();
        cx.notify();
    }

    fn next_page(&mut self, cx: &mut gpui::Context<Self>) {
        let Mode::Table {
            previous,
            current,
            next,
            ..
        } = &mut self.mode
        else {
            return;
        };
        let Some(cursor) = next.clone() else {
            return;
        };
        previous.push(current.clone());
        self.load_page(Some(cursor), cx);
    }

    fn previous_page(&mut self, cx: &mut gpui::Context<Self>) {
        let Mode::Table { previous, .. } = &mut self.mode else {
            return;
        };
        let Some(cursor) = previous.pop() else {
            return;
        };
        self.load_page(cursor, cx);
    }

    fn ask(
        &self,
        approval: Approval,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) -> futures::channel::oneshot::Receiver<bool> {
        match self.workspace.upgrade() {
            Some(workspace) => workspace.update(cx, |workspace, cx| {
                ApprovalModal::ask(workspace, approval, window, cx)
            }),
            None => futures::channel::oneshot::channel().1,
        }
    }

    fn row_count(&self) -> usize {
        self.results
            .as_ref()
            .map_or(0, |results| results.rows.len())
            + self.inserted.len()
    }

    fn value(&self, row: usize, column: usize) -> Value {
        if let Some(value) = self.pending.get(&(row, column)) {
            return value.clone();
        }
        let Some(results) = &self.results else {
            return Value::Missing;
        };
        match results.rows.get(row) {
            Some(values) => values.get(column).cloned().unwrap_or(Value::Missing),
            None => self
                .inserted
                .get(row - results.rows.len())
                .and_then(|values| values.get(column))
                .cloned()
                .unwrap_or(Value::Missing),
        }
    }

    fn original(&self, row: usize, column: usize) -> Value {
        self.results
            .as_ref()
            .and_then(|results| results.rows.get(row))
            .and_then(|values| values.get(column))
            .cloned()
            .unwrap_or(Value::Missing)
    }

    pub fn select(&mut self, row: usize, column: usize, cx: &mut gpui::Context<Self>) {
        if self.selected.map(|(selected, _)| selected) != Some(row) {
            self.history = None;
            self.load_history(row, cx);
        }
        self.selected = Some((row, column));
        cx.notify();
    }

    fn start_editing(
        &mut self,
        row: usize,
        column: usize,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        if !self.is_writable(cx) || self.deleted.contains(&row) {
            return;
        }
        let value = self.value(row, column);
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text(
                if value.is_nullish() {
                    String::new()
                } else {
                    value.display()
                },
                window,
                cx,
            );
            editor
        });
        let subscription = cx.subscribe_in(&editor, window, |this, _, event, window, cx| {
            if matches!(event, EditorEvent::Blurred) {
                this.finish_editing(true, window, cx);
            }
        });
        window.focus(&editor.focus_handle(cx), cx);
        self.editing = Some(Editing {
            row,
            column,
            editor,
            _subscription: subscription,
        });
        cx.notify();
    }

    fn finish_editing(&mut self, keep: bool, window: &mut Window, cx: &mut gpui::Context<Self>) {
        let Some(editing) = self.editing.take() else {
            return;
        };
        if keep {
            let text = editing.editor.read(cx).text(cx);
            self.stage_edit(editing.row, editing.column, &text, cx);
        }
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    /// Records `text` as the new value of a cell, pending review.
    pub fn stage_edit(
        &mut self,
        row: usize,
        column: usize,
        text: &str,
        cx: &mut gpui::Context<Self>,
    ) {
        let original = self.original(row, column);
        let value = Value::parse_like(text, &original);
        let results_len = self
            .results
            .as_ref()
            .map_or(0, |results| results.rows.len());
        if row >= results_len {
            if let Some(cell) = self
                .inserted
                .get_mut(row - results_len)
                .and_then(|values| values.get_mut(column))
            {
                *cell = value;
            }
        } else if value == original || (text.is_empty() && original.is_nullish()) {
            self.pending.remove(&(row, column));
        } else {
            self.pending.insert((row, column), value);
        }
        cx.notify();
    }

    /// Whether a query or page load is still in flight.
    pub fn is_loading(&self) -> bool {
        matches!(self.status, Status::Running)
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut gpui::Context<Self>) {
        if self.editing.is_some() {
            self.finish_editing(true, window, cx);
        } else if let Some((row, column)) = self.selected {
            self.start_editing(row, column, window, cx);
        }
    }

    fn cancel(&mut self, _: &menu::Cancel, window: &mut Window, cx: &mut gpui::Context<Self>) {
        if self.editing.is_some() {
            self.finish_editing(false, window, cx);
        }
    }

    fn run_query(&mut self, _: &RunQuery, window: &mut Window, cx: &mut gpui::Context<Self>) {
        self.run(window, cx);
    }

    fn add_row(&mut self, cx: &mut gpui::Context<Self>) {
        let columns = self
            .results
            .as_ref()
            .map_or(0, |results| results.columns.len());
        self.inserted.push(vec![Value::Missing; columns]);
        cx.notify();
    }

    fn toggle_delete_selected(&mut self, cx: &mut gpui::Context<Self>) {
        let Some((row, _)) = self.selected else {
            return;
        };
        let results_len = self
            .results
            .as_ref()
            .map_or(0, |results| results.rows.len());
        if row >= results_len {
            self.inserted.remove(row - results_len);
            self.selected = None;
        } else if !self.deleted.remove(&row) {
            self.deleted.insert(row);
        }
        cx.notify();
    }

    fn discard_changes(&mut self, cx: &mut gpui::Context<Self>) {
        self.pending.clear();
        self.deleted.clear();
        self.inserted.clear();
        cx.notify();
    }

    fn change_set(&self) -> Result<ChangeSet> {
        let (Mode::Table { collection, .. }, Some(results), Some(schema)) =
            (&self.mode, &self.results, &self.schema)
        else {
            bail!("nothing to change");
        };
        let column = |name: &str| results.column_index(name);
        let key_for = |row: usize| -> Result<Vec<(String, Value)>> {
            schema
                .primary_key
                .iter()
                .map(|field| {
                    let index = column(field).ok_or_else(|| {
                        anyhow::anyhow!("the results do not include key field `{field}`")
                    })?;
                    Ok((field.clone(), self.original(row, index)))
                })
                .collect()
        };
        let mut changes = Vec::new();
        let mut rows_with_edits = self.pending.keys().map(|(row, _)| *row).collect::<Vec<_>>();
        rows_with_edits.sort_unstable();
        rows_with_edits.dedup();
        for row in rows_with_edits {
            if self.deleted.contains(&row) {
                continue;
            }
            let mut set = self
                .pending
                .iter()
                .filter(|((pending_row, _), _)| *pending_row == row)
                .map(|((_, index), value)| (results.columns[*index].name.clone(), value.clone()))
                .collect::<Vec<_>>();
            set.sort_by(|left, right| left.0.cmp(&right.0));
            changes.push(Change::Update {
                key: key_for(row)?,
                set,
            });
        }
        let mut deleted = self.deleted.iter().copied().collect::<Vec<_>>();
        deleted.sort_unstable();
        for row in deleted {
            changes.push(Change::Delete { key: key_for(row)? });
        }
        for values in &self.inserted {
            let values = results
                .columns
                .iter()
                .zip(values)
                .filter(|(_, value)| !matches!(value, Value::Missing))
                .map(|(column, value)| (column.name.clone(), value.clone()))
                .collect::<Vec<_>>();
            if !values.is_empty() {
                changes.push(Change::Insert { values });
            }
        }
        Ok(ChangeSet {
            collection: collection.clone(),
            changes,
        })
    }

    pub fn review_changes(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) {
        let changes = match self.change_set() {
            Ok(changes) if !changes.changes.is_empty() => changes,
            Ok(_) => return,
            Err(error) => {
                self.status = Status::Failed(format!("{error:#}").into());
                cx.notify();
                return;
            }
        };
        let connection = self.connection.clone();
        let environment = self.environment(cx);
        let store = self.store.clone();
        let preview = store.update(cx, |store, cx| {
            store.render_changes(&connection, changes.clone(), cx)
        });
        let workspace = self.workspace.clone();
        self.task = Some(cx.spawn_in(window, async move |this, cx| {
            let preview = match preview.await {
                Ok(preview) => preview,
                Err(error) => {
                    this.update(cx, |this, cx| {
                        this.status = Status::Failed(format!("{error:#}").into());
                        cx.notify();
                    })
                    .log_err();
                    return;
                }
            };
            let Ok(approval) = workspace.update_in(cx, |workspace, window, cx| {
                ApprovalModal::ask(
                    workspace,
                    Approval {
                        title: format!(
                            "Apply {} change(s) to {connection}?",
                            changes.changes.len()
                        )
                        .into(),
                        subtitle: (environment == DatabaseEnvironment::Prod)
                            .then(|| "This connection is tagged prod.".into()),
                        detail: preview,
                        confirm_label: "Apply".into(),
                        environment,
                    },
                    window,
                    cx,
                )
            }) else {
                return;
            };
            if !approval.await.unwrap_or(false) {
                return;
            }
            let apply = store.update(cx, |store, cx| store.apply(&connection, changes, cx));
            let result = apply.await;
            this.update(cx, |this, cx| match result {
                Ok(_) => {
                    let cursor = match &this.mode {
                        Mode::Table { current, .. } => current.clone(),
                        Mode::Query { .. } => None,
                    };
                    this.load_page(cursor, cx);
                }
                Err(error) => {
                    this.status = Status::Failed(format!("{error:#}").into());
                    cx.notify();
                }
            })
            .log_err();
        }));
    }

    fn load_history(&mut self, row: usize, cx: &mut gpui::Context<Self>) {
        let Some(entry) = self.store.read(cx).entry(&self.connection) else {
            return;
        };
        if entry.spec.kind != DatabaseKind::Convex || !entry.history {
            return;
        }
        let Some(id) = self
            .results
            .as_ref()
            .and_then(|results| results.column_index("_id"))
            .map(|index| self.original(row, index).display())
        else {
            return;
        };
        let task = self.store.read(cx).row_history(&self.connection, &id, cx);
        self.history = Some((row, RowHistory::Loading));
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                if this
                    .history
                    .as_ref()
                    .is_some_and(|(loading_row, _)| *loading_row == row)
                {
                    this.history = Some((
                        row,
                        match result {
                            Ok(versions) => RowHistory::Loaded(versions),
                            Err(error) => RowHistory::Failed(format!("{error:#}").into()),
                        },
                    ));
                    cx.notify();
                }
            })
            .log_err();
        })
        .detach();
    }

    fn follow_reference(
        &mut self,
        reference: &Reference,
        outgoing: bool,
        row: usize,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let Some(results) = &self.results else {
            return;
        };
        let (from_fields, to_fields, target) = if outgoing {
            (
                &reference.fields,
                &reference.target_fields,
                &reference.target,
            )
        } else {
            (
                &reference.target_fields,
                &reference.fields,
                &reference.source,
            )
        };
        let filters = from_fields
            .iter()
            .zip(to_fields)
            .filter_map(|(from, to)| {
                let value = self.original(row, results.column_index(from)?);
                Some(Filter {
                    field: to.clone(),
                    op: FilterOp::Eq,
                    value,
                })
            })
            .collect::<Vec<_>>();
        let store = self.store.clone();
        let connection = self.connection.clone();
        let target = target.clone();
        self.workspace
            .update(cx, |workspace, cx| {
                ResultsView::open_table(workspace, store, connection, target, filters, window, cx);
            })
            .log_err();
    }

    fn render_toolbar(&self, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let environment = self.environment(cx);
        let writable = self.is_writable(cx);
        let change_count = self.pending.len() + self.deleted.len() + self.inserted.len();
        let status = match &self.status {
            Status::Idle => Label::new("").into_any_element(),
            Status::Running => Label::new("Running…")
                .color(Color::Muted)
                .size(LabelSize::Small)
                .into_any_element(),
            Status::Done { elapsed_ms } => {
                let results = self.results.as_ref();
                let rows = results.map_or(0, |results| results.rows.len());
                let truncated = results.is_some_and(|results| results.truncated);
                let affected = results.and_then(|results| results.affected_rows);
                let text = match affected {
                    Some(affected) if rows == 0 => {
                        format!("{affected} rows affected · {elapsed_ms} ms")
                    }
                    _ if truncated => format!("first {rows} rows · {elapsed_ms} ms"),
                    _ => format!("{rows} rows · {elapsed_ms} ms"),
                };
                Label::new(text)
                    .color(Color::Muted)
                    .size(LabelSize::Small)
                    .into_any_element()
            }
            Status::Failed(error) => Label::new(error.clone())
                .color(Color::Error)
                .size(LabelSize::Small)
                .into_any_element(),
        };
        let mut toolbar = h_flex()
            .w_full()
            .gap_1()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(environment_badge(environment))
            .child(Label::new(self.connection.clone()).size(LabelSize::Small));
        match &self.mode {
            Mode::Query { .. } => {
                toolbar = toolbar.child(
                    Button::new("run", "Run")
                        .start_icon(Icon::new(IconName::PlayFilled).size(IconSize::Small))
                        .key_binding(ui::KeyBinding::for_action_in(
                            &RunQuery,
                            &self.focus_handle,
                            cx,
                        ))
                        .on_click(cx.listener(|this, _, window, cx| this.run(window, cx))),
                );
            }
            Mode::Table { previous, next, .. } => {
                toolbar = toolbar
                    .child(
                        IconButton::new("previous-page", IconName::ChevronLeft)
                            .disabled(previous.is_empty())
                            .tooltip(Tooltip::text("Previous page"))
                            .on_click(cx.listener(|this, _, _, cx| this.previous_page(cx))),
                    )
                    .child(
                        IconButton::new("next-page", IconName::ChevronRight)
                            .disabled(next.is_none())
                            .tooltip(Tooltip::text("Next page"))
                            .on_click(cx.listener(|this, _, _, cx| this.next_page(cx))),
                    )
                    .child(
                        IconButton::new("refresh", IconName::RotateCw)
                            .tooltip(Tooltip::text("Refresh"))
                            .on_click(cx.listener(|this, _, window, cx| this.run(window, cx))),
                    );
                if writable {
                    toolbar = toolbar
                        .child(
                            IconButton::new("add-row", IconName::Plus)
                                .tooltip(Tooltip::text("Add a row"))
                                .on_click(cx.listener(|this, _, _, cx| this.add_row(cx))),
                        )
                        .child(
                            IconButton::new("delete-row", IconName::Trash)
                                .disabled(self.selected.is_none())
                                .tooltip(Tooltip::text("Delete the selected row"))
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.toggle_delete_selected(cx)),
                                ),
                        );
                }
            }
        }
        toolbar = toolbar.child(div().flex_1()).child(status);
        if self.has_changes() {
            toolbar = toolbar
                .child(
                    Button::new("discard", "Discard")
                        .on_click(cx.listener(|this, _, _, cx| this.discard_changes(cx))),
                )
                .child(
                    Button::new("review", format!("Review {change_count} change(s)"))
                        .style(ButtonStyle::Filled)
                        .on_click(
                            cx.listener(|this, _, window, cx| this.review_changes(window, cx)),
                        ),
                );
        } else if !writable && matches!(self.mode, Mode::Table { .. }) {
            let reason = match self.store.read(cx).state(&self.connection) {
                ConnectionState::Connected {
                    capability: Capability::ReadOnly { reason },
                    ..
                } => reason,
                _ if self
                    .schema
                    .as_ref()
                    .is_some_and(|schema| schema.primary_key.is_empty()) =>
                {
                    "this table has no primary key".to_string()
                }
                _ => "read-only".to_string(),
            };
            toolbar = toolbar.child(
                div()
                    .id("read-only")
                    .child(
                        Icon::new(IconName::Lock)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .tooltip(Tooltip::text(format!("Read-only: {reason}"))),
            );
        }
        toolbar
    }

    fn render_grid(&mut self, cx: &mut gpui::Context<Self>) -> AnyElement {
        let Some(results) = &self.results else {
            return div().into_any_element();
        };
        if results.columns.is_empty() {
            return div()
                .p_4()
                .child(Label::new("No rows returned.").color(Color::Muted))
                .into_any_element();
        }
        let columns = results.columns.len();
        let headers = results
            .columns
            .iter()
            .map(|column| {
                let is_key = self
                    .schema
                    .as_ref()
                    .is_some_and(|schema| schema.primary_key.contains(&column.name));
                h_flex()
                    .gap_1()
                    .when(is_key, |this| {
                        this.child(
                            Icon::new(IconName::Hash)
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                        )
                    })
                    .child(
                        Label::new(column.name.clone())
                            .size(LabelSize::Small)
                            .weight(gpui::FontWeight::SEMIBOLD),
                    )
                    .into_any_element()
            })
            .collect::<Vec<_>>();
        Table::new(columns)
            .interactable(&self.table_state)
            .striped()
            .width_config(ColumnWidthConfig::Resizable(self.column_widths.clone()))
            .header(headers)
            .uniform_list(
                "database-results",
                self.row_count(),
                cx.processor(move |this, range: Range<usize>, _, cx| {
                    range
                        .map(|row| {
                            (0..columns)
                                .map(|column| this.render_cell(row, column, cx))
                                .collect()
                        })
                        .collect()
                }),
            )
            .into_any_element()
    }

    fn render_cell(&self, row: usize, column: usize, cx: &mut gpui::Context<Self>) -> AnyElement {
        if let Some(editing) = &self.editing
            && editing.row == row
            && editing.column == column
        {
            return div()
                .w_full()
                .child(editing.editor.clone())
                .into_any_element();
        }
        let value = self.value(row, column);
        let is_pending = self.pending.contains_key(&(row, column));
        let is_deleted = self.deleted.contains(&row);
        let is_inserted = self
            .results
            .as_ref()
            .is_some_and(|results| row >= results.rows.len());
        let is_selected = self.selected == Some((row, column));
        let color = if is_deleted {
            Color::Deleted
        } else if is_pending || is_inserted {
            Color::Modified
        } else if value.is_nullish() {
            Color::Disabled
        } else {
            Color::Default
        };
        let text = match &value {
            Value::Missing => "—".to_string(),
            value => value.display().replace('\n', " "),
        };
        let index = row * 4096 + column;
        div()
            .id(("cell", index))
            .w_full()
            .overflow_hidden()
            .when(is_selected, |this| {
                this.border_1()
                    .border_color(cx.theme().colors().border_focused)
            })
            .child(
                Label::new(text)
                    .size(LabelSize::Small)
                    .color(color)
                    .single_line()
                    .truncate()
                    .when(is_deleted, |label| label.strikethrough()),
            )
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                this.select(row, column, cx);
                if event.click_count() == 2 {
                    this.start_editing(row, column, window, cx);
                }
            }))
            .into_any_element()
    }

    fn render_detail(&self, row: usize, cx: &mut gpui::Context<Self>) -> AnyElement {
        let Some(results) = &self.results else {
            return div().into_any_element();
        };
        let mut detail = v_flex()
            .id("row-detail")
            .w(px(340.))
            .h_full()
            .overflow_y_scroll()
            .p_2()
            .gap_2()
            .border_l_1()
            .border_color(cx.theme().colors().border_variant);

        let mut fields = v_flex().gap_1().child(section_label("Fields"));
        for (index, column) in results.columns.iter().enumerate() {
            let value = self.value(row, index);
            fields = fields.child(
                v_flex()
                    .child(
                        Label::new(column.name.clone())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(match &value {
                            Value::Missing => "—".to_string(),
                            value => value.display(),
                        })
                        .size(LabelSize::Small),
                    ),
            );
        }
        detail = detail.child(fields);

        if let Some(schema) = &self.schema {
            let outgoing = schema
                .references
                .iter()
                .filter(|reference| {
                    reference.fields.iter().all(|field| {
                        results
                            .column_index(field)
                            .is_some_and(|index| !self.original(row, index).is_nullish())
                    })
                })
                .cloned()
                .collect::<Vec<_>>();
            if !outgoing.is_empty() || !schema.referenced_by.is_empty() {
                let mut context = v_flex().gap_1().child(section_label("Context"));
                for (index, reference) in outgoing.into_iter().enumerate() {
                    let label = format!(
                        "→ {} ({})",
                        reference.target.qualified_name(),
                        reference.fields.join(", ")
                    );
                    context = context.child(
                        Button::new(("references", index), label)
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.follow_reference(&reference, true, row, window, cx)
                            })),
                    );
                }
                for (index, reference) in schema.referenced_by.iter().cloned().enumerate() {
                    let label = format!(
                        "← {} ({})",
                        reference.source.qualified_name(),
                        reference.fields.join(", ")
                    );
                    context = context.child(
                        Button::new(("referenced-by", index), label)
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.follow_reference(&reference, false, row, window, cx)
                            })),
                    );
                }
                detail = detail.child(context);
            }
        }

        if let Some((history_row, history)) = &self.history
            && *history_row == row
        {
            let mut section = v_flex().gap_1().child(section_label("History"));
            match history {
                RowHistory::Loading => {
                    section = section.child(
                        Label::new("Loading…")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    );
                }
                RowHistory::Failed(error) => {
                    section = section.child(
                        Label::new(error.clone())
                            .size(LabelSize::Small)
                            .color(Color::Error),
                    );
                }
                RowHistory::Loaded(versions) if versions.is_empty() => {
                    section = section.child(
                        Label::new("No versions indexed yet.")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    );
                }
                RowHistory::Loaded(versions) => {
                    for (index, version) in versions.iter().enumerate() {
                        let older = versions.get(index + 1);
                        section = section.child(render_version(version, older));
                    }
                }
            }
            detail = detail.child(section);
        }
        detail.into_any_element()
    }
}

fn render_version(version: &HistoryEntry, older: Option<&HistoryEntry>) -> impl IntoElement {
    let when = chrono::DateTime::from_timestamp_nanos(version.timestamp)
        .with_timezone(&chrono::Local)
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();
    let changes = if version.deleted {
        "deleted".to_string()
    } else {
        match older {
            None => "first indexed version".to_string(),
            Some(older) => {
                let empty = serde_json::Map::new();
                let new_fields = version.document.as_object().unwrap_or(&empty);
                let old_fields = older.document.as_object().unwrap_or(&empty);
                let mut changed = new_fields
                    .iter()
                    .filter(|(name, value)| old_fields.get(*name) != Some(value))
                    .map(|(name, value)| format!("{name} = {value}"))
                    .collect::<Vec<_>>();
                changed.extend(
                    old_fields
                        .keys()
                        .filter(|name| !new_fields.contains_key(*name))
                        .map(|name| format!("{name} removed")),
                );
                if changed.is_empty() {
                    "no field changes".to_string()
                } else {
                    changed.join(", ")
                }
            }
        }
    };
    v_flex()
        .child(Label::new(when).size(LabelSize::XSmall).color(Color::Muted))
        .child(Label::new(changes).size(LabelSize::Small))
}

fn section_label(text: &'static str) -> impl IntoElement {
    Label::new(text)
        .size(LabelSize::XSmall)
        .color(Color::Muted)
        .weight(gpui::FontWeight::SEMIBOLD)
}

pub(crate) fn environment_badge(environment: DatabaseEnvironment) -> impl IntoElement {
    let (text, color) = match environment {
        DatabaseEnvironment::Prod => ("prod", Color::Error),
        DatabaseEnvironment::Dev => ("dev", Color::Warning),
        DatabaseEnvironment::Local => ("local", Color::Success),
    };
    Label::new(text).size(LabelSize::XSmall).color(color)
}

fn set_sql_language(
    editor: &Entity<Editor>,
    languages: Arc<language::LanguageRegistry>,
    cx: &mut App,
) {
    // SQL highlighting comes from the SQL extension when it is installed;
    // otherwise the editor stays plain text.
    let editor = editor.downgrade();
    cx.spawn(async move |cx| {
        let Ok(language) = languages.language_for_name("SQL").await else {
            return;
        };
        editor
            .update(cx, |editor, cx| {
                if let Some(buffer) = editor.buffer().read(cx).as_singleton() {
                    buffer.update(cx, |buffer, cx| buffer.set_language(Some(language), cx));
                }
            })
            .log_err();
    })
    .detach();
}

impl EventEmitter<ItemEvent> for ResultsView {}

impl Focusable for ResultsView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match &self.mode {
            Mode::Query { editor } => editor.focus_handle(cx),
            Mode::Table { .. } => self.focus_handle.clone(),
        }
    }
}

impl Item for ResultsView {
    type Event = ItemEvent;

    fn tab_content_text(&self, _: usize, _: &App) -> SharedString {
        match &self.mode {
            Mode::Query { .. } => format!("Query · {}", self.connection).into(),
            Mode::Table { collection, .. } => {
                format!("{} · {}", collection.name, self.connection).into()
            }
        }
    }

    fn tab_icon(&self, _: &Window, _: &App) -> Option<Icon> {
        Some(Icon::new(match self.mode {
            Mode::Query { .. } => IconName::DatabaseZap,
            Mode::Table { .. } => IconName::Table,
        }))
    }

    fn is_dirty(&self, _: &App) -> bool {
        self.has_changes()
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

impl Render for ResultsView {
    fn render(&mut self, _: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let top = match &self.mode {
            Mode::Query { editor } => div()
                .p_2()
                .border_b_1()
                .border_color(cx.theme().colors().border_variant)
                .bg(cx.theme().colors().editor_background)
                .child(editor.clone())
                .into_any_element(),
            Mode::Table { filter_editor, .. } => h_flex()
                .p_1()
                .gap_1()
                .border_b_1()
                .border_color(cx.theme().colors().border_variant)
                .child(
                    Icon::new(IconName::Filter)
                        .size(IconSize::Small)
                        .color(Color::Muted),
                )
                .child(div().flex_1().child(filter_editor.clone()))
                .into_any_element(),
        };
        let detail_row = self
            .selected
            .map(|(row, _)| row)
            .filter(|row| *row < self.row_count());
        v_flex()
            .key_context("DatabaseResults")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::run_query))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .child(top)
            .child(self.render_toolbar(cx))
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div()
                            .flex_1()
                            .h_full()
                            .min_w_0()
                            .child(self.render_grid(cx)),
                    )
                    .when_some(detail_row, |this, row| {
                        this.child(self.render_detail(row, cx))
                    }),
            )
    }
}

/// Parses `field = value and other > 3`. Values may be quoted strings,
/// numbers, `true`, `false` or `null`; `~` means "contains".
pub(crate) fn parse_filters(text: &str, numbers_are_floats: bool) -> Result<Vec<Filter>> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(Vec::new());
    }
    split_clauses(text)
        .into_iter()
        .map(|clause| parse_clause(clause.trim(), numbers_are_floats))
        .collect()
}

fn split_clauses(text: &str) -> Vec<&str> {
    let mut clauses = Vec::new();
    let mut quote = None;
    let mut start = 0;
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        match quote {
            Some(open) if byte == open => quote = None,
            Some(_) => {}
            None if byte == b'\'' || byte == b'"' => quote = Some(byte),
            None if text[index..].len() >= 5
                && text.is_char_boundary(index + 5)
                && text[index..index + 5].eq_ignore_ascii_case(" and ") =>
            {
                clauses.push(&text[start..index]);
                start = index + 5;
                index += 5;
                continue;
            }
            None => {}
        }
        index += 1;
    }
    clauses.push(&text[start..]);
    clauses
}

fn parse_clause(clause: &str, numbers_are_floats: bool) -> Result<Filter> {
    let field_end = clause
        .find(|character: char| {
            !(character.is_alphanumeric() || matches!(character, '_' | '.' | '$'))
        })
        .unwrap_or(clause.len());
    let field = clause[..field_end].to_string();
    if field.is_empty() {
        bail!("expected a field name in `{clause}`");
    }
    let rest = clause[field_end..].trim_start();
    let lowered = rest.to_ascii_lowercase();
    if lowered == "is null" {
        return Ok(Filter {
            field,
            op: FilterOp::IsNull,
            value: Value::Null,
        });
    }
    if lowered == "is not null" {
        return Ok(Filter {
            field,
            op: FilterOp::IsNotNull,
            value: Value::Null,
        });
    }
    let operators = [
        ("!=", FilterOp::NotEq),
        ("<>", FilterOp::NotEq),
        ("<=", FilterOp::Lte),
        (">=", FilterOp::Gte),
        ("=", FilterOp::Eq),
        ("<", FilterOp::Lt),
        (">", FilterOp::Gt),
        ("~", FilterOp::Contains),
    ];
    let Some((symbol, op)) = operators
        .iter()
        .find(|(symbol, _)| rest.starts_with(symbol))
    else {
        bail!("expected an operator (=, !=, <, <=, >, >=, ~, is null) in `{clause}`");
    };
    let literal = rest[symbol.len()..].trim();
    Ok(Filter {
        field,
        op: *op,
        value: parse_literal(literal, numbers_are_floats),
    })
}

fn parse_literal(literal: &str, numbers_are_floats: bool) -> Value {
    let unquoted = literal
        .strip_prefix('\'')
        .and_then(|rest| rest.strip_suffix('\''))
        .or_else(|| {
            literal
                .strip_prefix('"')
                .and_then(|rest| rest.strip_suffix('"'))
        });
    if let Some(text) = unquoted {
        return Value::Text(text.to_string());
    }
    match literal.to_ascii_lowercase().as_str() {
        "null" => return Value::Null,
        "true" => return Value::Bool(true),
        "false" => return Value::Bool(false),
        _ => {}
    }
    if !numbers_are_floats && let Ok(integer) = literal.parse::<i64>() {
        return Value::Int(integer);
    }
    match literal.parse::<f64>() {
        Ok(number) => Value::Float(number),
        Err(_) => Value::Text(literal.to_string()),
    }
}

fn format_filters(filters: &[Filter]) -> String {
    filters
        .iter()
        .map(|filter| {
            let value = match &filter.value {
                Value::Text(text) => format!("'{text}'"),
                value => value.display(),
            };
            match filter.op {
                FilterOp::IsNull => format!("{} is null", filter.field),
                FilterOp::IsNotNull => format!("{} is not null", filter.field),
                op => {
                    let symbol = match op {
                        FilterOp::Eq => "=",
                        FilterOp::NotEq => "!=",
                        FilterOp::Lt => "<",
                        FilterOp::Lte => "<=",
                        FilterOp::Gt => ">",
                        FilterOp::Gte => ">=",
                        _ => "~",
                    };
                    format!("{} {symbol} {value}", filter.field)
                }
            }
        })
        .collect::<Vec<_>>()
        .join(" and ")
}

pub(crate) fn query_language(store: &DatabaseStore, connection: &str) -> QueryLanguage {
    match store.state(connection) {
        ConnectionState::Connected { language, .. } => language,
        _ => match store.entry(connection).map(|entry| entry.spec.kind) {
            Some(DatabaseKind::Convex) => QueryLanguage::None,
            Some(DatabaseKind::Postgres) => QueryLanguage::Postgres,
            Some(DatabaseKind::Clickhouse | DatabaseKind::ClickhouseRelay) => {
                QueryLanguage::ClickHouse
            }
            _ => QueryLanguage::MySql,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_filters() {
        let filters = parse_filters(
            "email = 'a and b@x.com' AND id >= 10 and phone is null",
            false,
        )
        .unwrap();
        assert_eq!(filters.len(), 3);
        assert_eq!(filters[0].value, Value::Text("a and b@x.com".into()));
        assert_eq!(filters[1].op, FilterOp::Gte);
        assert_eq!(filters[1].value, Value::Int(10));
        assert_eq!(filters[2].op, FilterOp::IsNull);
        assert_eq!(
            parse_filters("ryseSellerId = 2", true).unwrap()[0].value,
            Value::Float(2.0)
        );
        assert!(parse_filters("= 3", false).is_err());
        assert!(parse_filters("id ?? 3", false).is_err());
        let round_trip = format_filters(&filters);
        assert_eq!(parse_filters(&round_trip, false).unwrap(), filters);
    }
}
