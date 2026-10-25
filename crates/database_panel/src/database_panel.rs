mod agent_tools;
mod compare_view;
mod modals;
mod results_view;

use std::{ops::Range, sync::Arc};

use collections::{BTreeMap, HashSet};
use database::{
    Capability, Collection, ConnectionState, DatabaseKind, DatabaseStore, MissingSecret,
    QueryLanguage,
};
use gpui::{
    Action, AnyElement, App, AsyncWindowContext, DismissEvent, Entity, EventEmitter, FocusHandle,
    Focusable, MouseDownEvent, Pixels, Point, Subscription, UniformListScrollHandle, WeakEntity,
    Window, actions, anchored, deferred, px, uniform_list,
};
use picker::Picker;
use ui::{ContextMenu, ListItem, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

pub use compare_view::{CompareView, SavedCompare, SavedSide};
pub use modals::ApprovalModal;
pub use results_view::ResultsView;

use crate::{
    compare_view::load_saved_compares,
    modals::{QueryHistoryDelegate, SecretModal},
    results_view::{environment_badge, query_language},
};

actions!(
    database_panel,
    [
        /// Toggles focus on the database panel.
        ToggleFocus,
        /// Opens a keyed comparison of two tables or queries.
        OpenCompare,
        /// Searches the database query history.
        QueryHistory,
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<DatabasePanel>(window, cx);
        });
        workspace.register_action(|workspace, _: &OpenCompare, window, cx| {
            let store = DatabaseStore::for_project(workspace.project(), cx);
            let project_key = project_key(workspace, cx);
            CompareView::open(workspace, store, project_key, None, window, cx);
        });
        workspace.register_action(|workspace, _: &QueryHistory, window, cx| {
            let store = DatabaseStore::for_project(workspace.project(), cx);
            let handle = cx.entity().downgrade();
            workspace.toggle_modal(window, cx, |window, cx| {
                Picker::uniform_list(QueryHistoryDelegate::new(handle, store, cx), window, cx)
            });
        });
        if let Some(window) = window {
            agent_tools::register(workspace, window, cx);
        }
    })
    .detach();
}

/// Identifies a project for saved comparisons: its first worktree's path.
fn project_key(workspace: &Workspace, cx: &App) -> String {
    workspace
        .project()
        .read(cx)
        .visible_worktrees(cx)
        .next()
        .map(|worktree| worktree.read(cx).abs_path().to_string_lossy().to_string())
        .unwrap_or_default()
}

#[derive(Clone, Debug, PartialEq)]
enum Row {
    Connection(Arc<str>),
    Namespace(Arc<str>, String),
    Collection(Arc<str>, Collection),
    Note(Arc<str>, SharedString),
    ComparesHeader,
    Compare(SavedCompare),
}

pub struct DatabasePanel {
    workspace: WeakEntity<Workspace>,
    store: Entity<DatabaseStore>,
    focus_handle: FocusHandle,
    position: DockPosition,
    expanded: HashSet<Arc<str>>,
    collapsed_namespaces: HashSet<(Arc<str>, String)>,
    rows: Vec<Row>,
    project_key: String,
    scroll_handle: UniformListScrollHandle,
    context_menu: Option<(Entity<ContextMenu>, Point<Pixels>, Subscription)>,
    _subscriptions: Vec<Subscription>,
}

impl DatabasePanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> anyhow::Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, window, cx| {
            Self::new(workspace, window, cx)
        })
    }

    fn new(
        workspace: &mut Workspace,
        _: &mut Window,
        cx: &mut gpui::Context<Workspace>,
    ) -> Entity<Self> {
        let store = DatabaseStore::for_project(workspace.project(), cx);
        let project_key = project_key(workspace, cx);
        let workspace_handle = cx.entity().downgrade();
        cx.new(|cx| {
            let subscriptions =
                vec![cx.observe(&store, |this: &mut Self, _, cx| this.rebuild_rows(cx))];
            let mut this = Self {
                workspace: workspace_handle,
                store,
                focus_handle: cx.focus_handle(),
                position: DockPosition::Left,
                expanded: HashSet::default(),
                collapsed_namespaces: HashSet::default(),
                rows: Vec::new(),
                project_key,
                scroll_handle: UniformListScrollHandle::new(),
                context_menu: None,
                _subscriptions: subscriptions,
            };
            this.rebuild_rows(cx);
            this
        })
    }

    fn rebuild_rows(&mut self, cx: &mut gpui::Context<Self>) {
        let store = self.store.read(cx);
        let mut rows = Vec::new();
        for entry in store.connections() {
            let name = entry.name.clone();
            rows.push(Row::Connection(name.clone()));
            if !self.expanded.contains(&name) {
                continue;
            }
            if let Some(error) = &entry.error {
                rows.push(Row::Note(name, error.clone()));
                continue;
            }
            match store.state(&name) {
                ConnectionState::Connecting => rows.push(Row::Note(name, "Connecting…".into())),
                ConnectionState::NeedsSecret => rows.push(Row::Note(
                    name,
                    "Needs a secret: right-click → Set Secret".into(),
                )),
                ConnectionState::Failed(error) => rows.push(Row::Note(name, error)),
                ConnectionState::Disconnected => {}
                ConnectionState::Connected { .. } => match store.collections(&name) {
                    None => rows.push(Row::Note(name, "Loading…".into())),
                    Some([]) => rows.push(Row::Note(name, "No tables".into())),
                    Some(collections) => {
                        let mut by_namespace: BTreeMap<Option<String>, Vec<&Collection>> =
                            BTreeMap::default();
                        for collection in collections {
                            by_namespace
                                .entry(collection.reference.namespace.clone())
                                .or_default()
                                .push(collection);
                        }
                        let group = by_namespace.len() > 1;
                        for (namespace, collections) in by_namespace {
                            if group && let Some(namespace) = &namespace {
                                let key = (name.clone(), namespace.clone());
                                rows.push(Row::Namespace(name.clone(), namespace.clone()));
                                if self.collapsed_namespaces.contains(&key) {
                                    continue;
                                }
                            }
                            for collection in collections {
                                rows.push(Row::Collection(name.clone(), collection.clone()));
                            }
                        }
                    }
                },
            }
        }
        let compares = load_saved_compares(&self.project_key, cx);
        if !compares.is_empty() {
            rows.push(Row::ComparesHeader);
            rows.extend(compares.into_iter().map(Row::Compare));
        }
        self.rows = rows;
        cx.notify();
    }

    pub fn toggle_connection(
        &mut self,
        name: &Arc<str>,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        if !self.expanded.remove(name) {
            self.expanded.insert(name.clone());
            if matches!(
                self.store.read(cx).state(name),
                ConnectionState::Disconnected | ConnectionState::Failed(_)
            ) {
                self.connect(name, window, cx);
            }
        }
        self.rebuild_rows(cx);
    }

    fn connect(&mut self, name: &Arc<str>, window: &mut Window, cx: &mut gpui::Context<Self>) {
        let task = self.store.update(cx, |store, cx| store.connect(name, cx));
        let name = name.clone();
        cx.spawn_in(window, async move |this, cx| {
            if let Err(error) = task.await
                && error.downcast_ref::<MissingSecret>().is_some()
            {
                this.update_in(cx, |this, window, cx| this.prompt_secret(&name, window, cx))
                    .log_err();
            }
        })
        .detach();
    }

    fn prompt_secret(
        &mut self,
        name: &Arc<str>,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let store = self.store.clone();
        let name = name.clone();
        self.workspace
            .update(cx, |workspace, cx| {
                workspace.toggle_modal(window, cx, |window, cx| {
                    SecretModal::new(store, name, window, cx)
                });
            })
            .log_err();
    }

    fn open_collection(
        &mut self,
        name: &Arc<str>,
        collection: &Collection,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let store = self.store.clone();
        let name = name.clone();
        let reference = collection.reference.clone();
        self.workspace
            .update(cx, |workspace, cx| {
                ResultsView::open_table(workspace, store, name, reference, Vec::new(), window, cx);
            })
            .log_err();
    }

    fn new_query(&mut self, name: &Arc<str>, window: &mut Window, cx: &mut gpui::Context<Self>) {
        let store = self.store.clone();
        let name = name.clone();
        self.workspace
            .update(cx, |workspace, cx| {
                ResultsView::open_query(workspace, store, name, None, window, cx);
            })
            .log_err();
    }

    fn open_compare(
        &mut self,
        saved: Option<SavedCompare>,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let store = self.store.clone();
        let project_key = self.project_key.clone();
        self.workspace
            .update(cx, |workspace, cx| {
                CompareView::open(workspace, store, project_key, saved, window, cx);
            })
            .log_err();
    }

    fn deploy_context_menu(
        &mut self,
        position: Point<Pixels>,
        name: Arc<str>,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let store = self.store.read(cx);
        let connected = matches!(store.state(&name), ConnectionState::Connected { .. });
        let has_language = query_language(store, &name) != QueryLanguage::None;
        let this = cx.entity().downgrade();
        let menu = ContextMenu::build(window, cx, move |menu, _, _| {
            let action =
                |handler: fn(&mut Self, &Arc<str>, &mut Window, &mut gpui::Context<Self>)| {
                    let this = this.clone();
                    let name = name.clone();
                    move |window: &mut Window, cx: &mut App| {
                        this.update(cx, |this, cx| handler(this, &name, window, cx))
                            .log_err();
                    }
                };
            menu.entry(
                if connected { "Reconnect" } else { "Connect" },
                None,
                action(|this, name, window, cx| this.connect(name, window, cx)),
            )
            .when(connected, |menu| {
                menu.entry(
                    "Disconnect",
                    None,
                    action(|this, name, _, cx| {
                        this.store
                            .update(cx, |store, cx| store.disconnect(name, cx));
                    }),
                )
                .entry(
                    "Refresh Tables",
                    None,
                    action(|this, name, _, cx| {
                        this.store
                            .update(cx, |store, cx| store.refresh_collections(name, cx))
                            .detach_and_log_err(cx);
                    }),
                )
            })
            .when(has_language, |menu| {
                menu.entry(
                    "New Query",
                    None,
                    action(|this, name, window, cx| this.new_query(name, window, cx)),
                )
            })
            .entry(
                "Compare…",
                None,
                action(|this, _, window, cx| this.open_compare(None, window, cx)),
            )
            .separator()
            .entry(
                "Set Secret…",
                None,
                action(|this, name, window, cx| this.prompt_secret(name, window, cx)),
            )
            .entry(
                "Forget Secret",
                None,
                action(|this, name, _, cx| {
                    this.store
                        .update(cx, |store, cx| store.forget_secret(name, cx))
                        .detach_and_log_err(cx);
                }),
            )
        });
        window.focus(&menu.focus_handle(cx), cx);
        let subscription = cx.subscribe(&menu, |this, _, _: &DismissEvent, cx| {
            this.context_menu.take();
            cx.notify();
        });
        self.context_menu = Some((menu, position, subscription));
        cx.notify();
    }

    fn render_row(&self, index: usize, cx: &mut gpui::Context<Self>) -> AnyElement {
        let Some(row) = self.rows.get(index).cloned() else {
            return div().into_any_element();
        };
        let store = self.store.read(cx);
        match row {
            Row::Connection(name) => {
                let Some(entry) = store.entry(&name) else {
                    return div().into_any_element();
                };
                let state = store.state(&name);
                let (state_icon, state_color, state_text): (IconName, Color, SharedString) =
                    match &state {
                        ConnectionState::Connected {
                            capability: Capability::Writable,
                            ..
                        } => (
                            IconName::Check,
                            Color::Success,
                            "Connected, writable".into(),
                        ),
                        ConnectionState::Connected {
                            capability: Capability::ReadOnly { reason },
                            ..
                        } => (
                            IconName::Lock,
                            Color::Muted,
                            format!("Connected, read-only: {reason}").into(),
                        ),
                        ConnectionState::Connecting => {
                            (IconName::ArrowCircle, Color::Muted, "Connecting".into())
                        }
                        ConnectionState::NeedsSecret => {
                            (IconName::Warning, Color::Warning, "Needs a secret".into())
                        }
                        ConnectionState::Failed(error) => {
                            (IconName::XCircle, Color::Error, error.clone())
                        }
                        ConnectionState::Disconnected => {
                            (IconName::Disconnected, Color::Muted, "Not connected".into())
                        }
                    };
                let kind = match entry.spec.kind {
                    DatabaseKind::Mysql => "MySQL",
                    DatabaseKind::Postgres => "Postgres",
                    DatabaseKind::Clickhouse => "ClickHouse",
                    DatabaseKind::ClickhouseRelay => "ClickHouse relay",
                    DatabaseKind::Convex => "Convex",
                };
                let access = format!("agents: {:?}", entry.agent_access).to_lowercase();
                let click_name = name.clone();
                let menu_name = name.clone();
                ListItem::new(("connection", index))
                    .toggle(Some(self.expanded.contains(&name)))
                    .start_slot(
                        Icon::new(IconName::DatabaseZap)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        h_flex()
                            .gap_1p5()
                            .child(Label::new(name.clone()))
                            .child(environment_badge(entry.environment))
                            .child(Label::new(kind).size(LabelSize::XSmall).color(Color::Muted)),
                    )
                    .end_slot(
                        div()
                            .id(("connection-state", index))
                            .child(
                                Icon::new(state_icon)
                                    .size(IconSize::Small)
                                    .color(state_color),
                            )
                            .tooltip(Tooltip::text(format!("{state_text} · {access}"))),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.toggle_connection(&click_name, window, cx)
                    }))
                    .on_secondary_mouse_down(cx.listener(
                        move |this, event: &MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            this.deploy_context_menu(event.position, menu_name.clone(), window, cx);
                        },
                    ))
                    .into_any_element()
            }
            Row::Namespace(name, namespace) => {
                let key = (name.clone(), namespace.clone());
                let open = !self.collapsed_namespaces.contains(&key);
                ListItem::new(("namespace", index))
                    .indent_level(1)
                    .toggle(Some(open))
                    .start_slot(
                        Icon::new(IconName::Folder)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(Label::new(namespace).size(LabelSize::Small))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !this.collapsed_namespaces.remove(&key) {
                            this.collapsed_namespaces.insert(key.clone());
                        }
                        this.rebuild_rows(cx);
                    }))
                    .into_any_element()
            }
            Row::Collection(name, collection) => {
                let indent = if collection.reference.namespace.is_some()
                    && self
                        .rows
                        .iter()
                        .any(|row| matches!(row, Row::Namespace(owner, _) if *owner == name))
                {
                    2
                } else {
                    1
                };
                let label = collection.reference.name.clone();
                let is_view = collection.kind == database::CollectionKind::View;
                ListItem::new(("collection", index))
                    .indent_level(indent)
                    .start_slot(
                        Icon::new(if is_view {
                            IconName::Eye
                        } else {
                            IconName::Table
                        })
                        .size(IconSize::Small)
                        .color(Color::Muted),
                    )
                    .child(Label::new(label).size(LabelSize::Small))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_collection(&name, &collection, window, cx)
                    }))
                    .into_any_element()
            }
            Row::Note(_, text) => ListItem::new(("note", index))
                .indent_level(1)
                .child(
                    Label::new(text)
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .single_line()
                        .truncate(),
                )
                .into_any_element(),
            Row::ComparesHeader => div()
                .px_2()
                .pt_3()
                .pb_1()
                .child(
                    Label::new("Saved comparisons")
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .into_any_element(),
            Row::Compare(saved) => {
                let label = saved.name.clone();
                ListItem::new(("compare", index))
                    .start_slot(
                        Icon::new(IconName::Diff)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(Label::new(label).size(LabelSize::Small))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_compare(Some(saved.clone()), window, cx)
                    }))
                    .into_any_element()
            }
        }
    }

    fn render_empty(&self, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        v_flex()
            .p_3()
            .gap_2()
            .child(Label::new("No database connections"))
            .child(
                Label::new("Add them under `database.connections` in your settings or a project's .zed/settings.json. Secrets are asked for on connect and kept in the keychain.")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(
                div()
                    .p_2()
                    .rounded_sm()
                    .bg(cx.theme().colors().editor_background)
                    .child(
                        Label::new(EXAMPLE_SETTINGS)
                            .size(LabelSize::XSmall)
                            .buffer_font(cx),
                    ),
            )
            .child(
                Button::new("open-settings", "Open Settings")
                    .on_click(|_, window, cx| window.dispatch_action(zed_actions::OpenSettingsFile.boxed_clone(), cx)),
            )
    }
}

const EXAMPLE_SETTINGS: &str = r#""database": {
  "connections": {
    "ryse-local": {
      "kind": "mysql", "host": "127.0.0.1",
      "user": "root", "database": "ryse",
      "environment": "local"
    },
    "atlas": {
      "kind": "convex",
      "url": "https://<deployment>.convex.cloud",
      "environment": "prod"
    }
  }
}"#;

impl EventEmitter<PanelEvent> for DatabasePanel {}

impl Focusable for DatabasePanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Panel for DatabasePanel {
    fn persistent_name() -> &'static str {
        "Database Panel"
    }

    fn panel_key() -> &'static str {
        "DatabasePanel"
    }

    fn position(&self, _: &Window, _: &App) -> DockPosition {
        self.position
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(
        &mut self,
        position: DockPosition,
        _: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        self.position = position;
        cx.notify();
    }

    fn default_size(&self, _: &Window, _: &App) -> Pixels {
        px(280.)
    }

    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        Some(IconName::DatabaseZap)
    }

    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("Databases")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        8
    }
}

impl Render for DatabasePanel {
    fn render(&mut self, _: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let is_empty = self.store.read(cx).connections().is_empty();
        v_flex()
            .key_context("DatabasePanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .child(
                h_flex()
                    .px_2()
                    .py_1()
                    .gap_1()
                    .border_b_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(
                        Label::new("Databases")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(div().flex_1())
                    .child(
                        IconButton::new("compare", IconName::Diff)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Compare tables or queries"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_compare(None, window, cx)
                            })),
                    )
                    .child(
                        IconButton::new("history", IconName::HistoryRerun)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Query history"))
                            .on_click(|_, window, cx| {
                                window.dispatch_action(QueryHistory.boxed_clone(), cx)
                            }),
                    )
                    .child(
                        IconButton::new("settings", IconName::Settings)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Edit connections"))
                            .on_click(|_, window, cx| {
                                window.dispatch_action(
                                    zed_actions::OpenSettingsFile.boxed_clone(),
                                    cx,
                                )
                            }),
                    ),
            )
            .map(|this| {
                if is_empty {
                    this.child(self.render_empty(cx))
                } else {
                    this.child(
                        uniform_list(
                            "database-tree",
                            self.rows.len(),
                            cx.processor(|this, range: Range<usize>, _, cx| {
                                range.map(|index| this.render_row(index, cx)).collect()
                            }),
                        )
                        .track_scroll(&self.scroll_handle)
                        .flex_1(),
                    )
                }
            })
            .children(self.context_menu.as_ref().map(|(menu, position, _)| {
                deferred(
                    anchored()
                        .position(*position)
                        .anchor(gpui::Anchor::TopLeft)
                        .child(menu.clone()),
                )
                .with_priority(1)
            }))
    }
}
