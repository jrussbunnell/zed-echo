use std::{ops::Range, sync::Arc};

use anyhow::{Result, anyhow};
use database::{
    CollectionRef, CompareOptions, CompareOutcome, CompareRow, CompareSide, DatabaseKind,
    DatabaseStore, FetchSource, QueryLanguage, ResultSet, compare,
};
use db::kvp::KeyValueStore;
use editor::Editor;
use gpui::{App, Entity, EventEmitter, FocusHandle, Focusable, Task, WeakEntity, Window};
use serde::{Deserialize, Serialize};
use ui::{
    ContextMenu, DropdownMenu, DropdownStyle, Table, TableInteractionState, Tooltip, prelude::*,
};
use util::ResultExt as _;
use workspace::{Item, Workspace, item::ItemEvent};

use crate::results_view::{parse_filters, query_language};

/// Rows are read whole on both sides; past this the comparison refuses
/// rather than report a partial (and misleading) result.
const COMPARE_ROW_CAP: usize = 200_000;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SavedSide {
    pub connection: String,
    /// A table name with an optional `where` filter, or a SQL query.
    pub source: String,
    /// Comma-separated key fields.
    pub key: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SavedCompare {
    pub name: String,
    pub left: SavedSide,
    pub right: SavedSide,
    /// `left_field=right_field` pairs, comma-separated. Empty compares
    /// fields with the same name.
    pub fields: String,
}

fn saved_compares_key(project_key: &str) -> String {
    format!("database_saved_compares:{project_key}")
}

pub fn load_saved_compares(project_key: &str, cx: &App) -> Vec<SavedCompare> {
    KeyValueStore::global(cx)
        .read_kvp(&saved_compares_key(project_key))
        .log_err()
        .flatten()
        .and_then(|json| serde_json::from_str(&json).log_err())
        .unwrap_or_default()
}

fn store_saved_compares(
    project_key: &str,
    compares: &[SavedCompare],
    cx: &App,
) -> Task<Result<()>> {
    let key = saved_compares_key(project_key);
    let json = serde_json::to_string(compares);
    let kvp = KeyValueStore::global(cx);
    cx.background_spawn(async move { kvp.write_kvp(key, json?).await })
}

struct SideInputs {
    connection: Option<Arc<str>>,
    source: Entity<Editor>,
    key: Entity<Editor>,
}

#[derive(Clone, Copy, PartialEq)]
enum Show {
    Differences,
    All,
}

struct Comparison {
    outcome: CompareOutcome,
    left: ResultSet,
    right: ResultSet,
    /// Where each side's rows live, when they came from a table, so a row
    /// can be opened in its own table.
    left_target: Option<Target>,
    right_target: Option<Target>,
}

#[derive(Clone)]
struct Target {
    connection: Arc<str>,
    collection: CollectionRef,
    key: Vec<String>,
}

/// Keyed diff of two row sets, from one connection or two.
pub struct CompareView {
    store: Entity<DatabaseStore>,
    workspace: WeakEntity<Workspace>,
    project_key: String,
    left: SideInputs,
    right: SideInputs,
    fields: Entity<Editor>,
    name: Entity<Editor>,
    show: Show,
    comparison: Option<Comparison>,
    status: Option<(SharedString, bool)>,
    table_state: Entity<TableInteractionState>,
    focus_handle: FocusHandle,
    task: Option<Task<()>>,
}

impl CompareView {
    pub fn open(
        workspace: &mut Workspace,
        store: Entity<DatabaseStore>,
        project_key: String,
        saved: Option<SavedCompare>,
        window: &mut Window,
        cx: &mut gpui::Context<Workspace>,
    ) -> Entity<Self> {
        let workspace_handle = cx.entity().downgrade();
        let view = cx.new(|cx| {
            let saved = saved.clone().unwrap_or_default();
            let editor = |text: &str,
                          placeholder: &str,
                          window: &mut Window,
                          cx: &mut gpui::Context<Self>| {
                cx.new(|cx| {
                    let mut editor = Editor::single_line(window, cx);
                    editor.set_placeholder_text(placeholder, window, cx);
                    editor.set_text(text, window, cx);
                    editor
                })
            };
            let connection = |name: &str| (!name.is_empty()).then(|| Arc::<str>::from(name));
            Self {
                left: SideInputs {
                    connection: connection(&saved.left.connection),
                    source: editor(
                        &saved.left.source,
                        "seller  ·  seller where active = true  ·  SELECT …",
                        window,
                        cx,
                    ),
                    key: editor(&saved.left.key, "key fields, e.g. id", window, cx),
                },
                right: SideInputs {
                    connection: connection(&saved.right.connection),
                    source: editor(&saved.right.source, "ryseSellerUserLinks", window, cx),
                    key: editor(
                        &saved.right.key,
                        "key fields, e.g. ryseSellerId",
                        window,
                        cx,
                    ),
                },
                fields: editor(
                    &saved.fields,
                    "fields to compare: email=email, created_at=createdAt (empty: same names)",
                    window,
                    cx,
                ),
                name: editor(&saved.name, "name to save as", window, cx),
                store,
                workspace: workspace_handle,
                project_key,
                show: Show::Differences,
                comparison: None,
                status: None,
                table_state: cx.new(|cx| TableInteractionState::new(cx)),
                focus_handle: cx.focus_handle(),
                task: None,
            }
        });
        workspace.add_item_to_active_pane(Box::new(view.clone()), None, true, window, cx);
        if saved.is_some() {
            view.update(cx, |view, cx| view.run(cx));
        }
        view
    }

    /// Whether either side is still being read.
    pub fn is_loading(&self) -> bool {
        self.task.is_some()
            && self.comparison.is_none()
            && !self.status.as_ref().is_some_and(|(_, is_error)| *is_error)
    }

    fn saved(&self, cx: &App) -> SavedCompare {
        let side = |inputs: &SideInputs| SavedSide {
            connection: inputs.connection.as_deref().unwrap_or_default().to_string(),
            source: inputs.source.read(cx).text(cx),
            key: inputs.key.read(cx).text(cx),
        };
        SavedCompare {
            name: self.name.read(cx).text(cx).trim().to_string(),
            left: side(&self.left),
            right: side(&self.right),
            fields: self.fields.read(cx).text(cx),
        }
    }

    fn save(&mut self, cx: &mut gpui::Context<Self>) {
        let saved = self.saved(cx);
        if saved.name.is_empty() {
            self.status = Some(("Name the comparison to save it.".into(), true));
            cx.notify();
            return;
        }
        let mut compares = load_saved_compares(&self.project_key, cx);
        compares.retain(|existing| existing.name != saved.name);
        compares.push(saved.clone());
        compares.sort_by(|left, right| left.name.cmp(&right.name));
        store_saved_compares(&self.project_key, &compares, cx).detach_and_log_err(cx);
        self.status = Some((format!("Saved \"{}\".", saved.name).into(), false));
        self.store.update(cx, |_, cx| cx.notify());
        cx.notify();
    }

    fn fetch(
        &self,
        side: &SideInputs,
        key: &[String],
        cx: &mut gpui::Context<Self>,
    ) -> Result<(Task<Result<ResultSet>>, Option<Target>)> {
        let connection = side
            .connection
            .clone()
            .ok_or_else(|| anyhow!("choose a connection on both sides"))?;
        let text = side.source.read(cx).text(cx);
        let source = fetch_source(
            &text,
            query_language(self.store.read(cx), &connection),
            self.store
                .read(cx)
                .entry(&connection)
                .map(|entry| entry.spec.kind),
        )?;
        let target = match &source {
            FetchSource::Collection { collection, .. } => Some(Target {
                connection: connection.clone(),
                collection: collection.clone(),
                key: key.to_vec(),
            }),
            FetchSource::Query(_) => None,
        };
        let task = self.store.update(cx, |store, cx| {
            store.fetch_all(&connection, source, COMPARE_ROW_CAP, cx)
        });
        Ok((task, target))
    }

    fn open_row(
        &mut self,
        target: &Target,
        key: &[database::Value],
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let filters = target
            .key
            .iter()
            .zip(key)
            .map(|(field, value)| database::Filter {
                field: field.clone(),
                op: database::FilterOp::Eq,
                value: value.clone(),
            })
            .collect();
        let store = self.store.clone();
        let target = target.clone();
        self.workspace
            .update(cx, |workspace, cx| {
                crate::ResultsView::open_table(
                    workspace,
                    store,
                    target.connection,
                    target.collection,
                    filters,
                    window,
                    cx,
                );
            })
            .log_err();
    }

    fn run(&mut self, cx: &mut gpui::Context<Self>) {
        let keys = |inputs: &SideInputs, cx: &App| -> Vec<String> {
            inputs
                .key
                .read(cx)
                .text(cx)
                .split(',')
                .map(|field| field.trim().to_string())
                .filter(|field| !field.is_empty())
                .collect()
        };
        let left_key = keys(&self.left, cx);
        let right_key = keys(&self.right, cx);
        let fields = self
            .fields
            .read(cx)
            .text(cx)
            .split(',')
            .filter_map(|pair| {
                let pair = pair.trim();
                if pair.is_empty() {
                    return None;
                }
                Some(match pair.split_once('=') {
                    Some((left, right)) => (left.trim().to_string(), right.trim().to_string()),
                    None => (pair.to_string(), pair.to_string()),
                })
            })
            .collect::<Vec<_>>();
        let fetches = self
            .fetch(&self.left, &left_key, cx)
            .and_then(|left| Ok((left, self.fetch(&self.right, &right_key, cx)?)));
        let ((left, left_target), (right, right_target)) = match fetches {
            Ok(fetches) => fetches,
            Err(error) => {
                self.status = Some((format!("{error:#}").into(), true));
                cx.notify();
                return;
            }
        };
        self.status = Some(("Reading both sides…".into(), false));
        self.task = Some(cx.spawn(async move |this, cx| {
            let (left, right) = futures::join!(left, right);
            let result = (|| -> Result<Comparison> {
                let (left, right) = (left?, right?);
                let outcome = compare(
                    CompareSide {
                        rows: &left,
                        key: &left_key,
                    },
                    CompareSide {
                        rows: &right,
                        key: &right_key,
                    },
                    &CompareOptions { fields },
                )?;
                Ok(Comparison {
                    outcome,
                    left,
                    right,
                    left_target,
                    right_target,
                })
            })();
            this.update(cx, |this, cx| {
                match result {
                    Ok(comparison) => {
                        let outcome = &comparison.outcome;
                        let mut summary = format!(
                            "{} only on the left · {} only on the right · {} differ · {} identical",
                            outcome.left_only(),
                            outcome.right_only(),
                            outcome.changed(),
                            outcome.identical()
                        );
                        if outcome.duplicate_left_keys + outcome.duplicate_right_keys > 0 {
                            summary.push_str(&format!(
                                " · duplicate keys: {} left, {} right (first row kept)",
                                outcome.duplicate_left_keys, outcome.duplicate_right_keys
                            ));
                        }
                        this.status = Some((summary.into(), false));
                        this.comparison = Some(comparison);
                    }
                    Err(error) => this.status = Some((format!("{error:#}").into(), true)),
                }
                cx.notify();
            })
            .log_err();
        }));
        cx.notify();
    }

    fn visible_rows(&self) -> Vec<usize> {
        let Some(comparison) = &self.comparison else {
            return Vec::new();
        };
        comparison
            .outcome
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| match self.show {
                Show::All => true,
                Show::Differences => {
                    !matches!(row, CompareRow::Matched { diffs, .. } if diffs.is_empty())
                }
            })
            .map(|(index, _)| index)
            .collect()
    }

    fn connection_menu(
        &self,
        left: bool,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) -> Entity<ContextMenu> {
        let names = self
            .store
            .read(cx)
            .connections()
            .iter()
            .map(|entry| entry.name.clone())
            .collect::<Vec<_>>();
        let this = cx.entity().downgrade();
        ContextMenu::build(window, cx, move |mut menu, _, _| {
            for name in names {
                let this = this.clone();
                menu = menu.entry(name.to_string(), None, move |_, cx| {
                    let name = name.clone();
                    this.update(cx, |this, cx| {
                        if left {
                            this.left.connection = Some(name);
                        } else {
                            this.right.connection = Some(name);
                        }
                        cx.notify();
                    })
                    .log_err();
                });
            }
            menu
        })
    }

    fn render_side(
        &self,
        left: bool,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) -> impl IntoElement {
        let inputs = if left { &self.left } else { &self.right };
        let label = inputs
            .connection
            .as_deref()
            .map(str::to_string)
            .unwrap_or_else(|| "Choose connection".to_string());
        v_flex()
            .flex_1()
            .gap_1()
            .child(
                Label::new(if left { "Left" } else { "Right" })
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .child(
                DropdownMenu::new(
                    if left {
                        "left-connection"
                    } else {
                        "right-connection"
                    },
                    label,
                    self.connection_menu(left, window, cx),
                )
                .style(DropdownStyle::Outlined),
            )
            .child(input(inputs.source.clone(), cx))
            .child(input(inputs.key.clone(), cx))
    }

    fn render_results(&self, cx: &mut gpui::Context<Self>) -> AnyElement {
        let Some(comparison) = &self.comparison else {
            return div().into_any_element();
        };
        let rows = self.visible_rows();
        let count = rows.len();
        if count == 0 {
            return div()
                .p_4()
                .child(
                    Label::new(if comparison.outcome.rows.is_empty() {
                        "Both sides are empty."
                    } else {
                        "No differences."
                    })
                    .color(Color::Muted),
                )
                .into_any_element();
        }
        Table::new(3)
            .interactable(&self.table_state)
            .striped()
            .width_config(ui::ColumnWidthConfig::explicit(vec![
                gpui::DefiniteLength::Fraction(0.2),
                gpui::DefiniteLength::Fraction(0.15),
                gpui::DefiniteLength::Fraction(0.65),
            ]))
            .header(vec!["Key", "Status", "Differences"])
            .uniform_list(
                "compare-rows",
                count,
                cx.processor(move |this, range: Range<usize>, _, cx| {
                    let Some(comparison) = &this.comparison else {
                        return Vec::new();
                    };
                    range
                        .filter_map(|index| rows.get(index))
                        .filter_map(|index| Some((*index, comparison.outcome.rows.get(*index)?)))
                        .map(|(index, row)| render_compare_row(index, row, comparison, cx))
                        .collect()
                }),
            )
            .into_any_element()
    }
}

fn render_compare_row(
    index: usize,
    row: &CompareRow,
    comparison: &Comparison,
    cx: &mut gpui::Context<CompareView>,
) -> Vec<AnyElement> {
    let key = match row {
        CompareRow::LeftOnly { key, .. }
        | CompareRow::RightOnly { key, .. }
        | CompareRow::Matched { key, .. } => key.clone(),
    };
    let key_text = |key: &[database::Value]| {
        key.iter()
            .map(|value| value.display())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let open_button =
        |id: &'static str,
         label: &'static str,
         target: Option<Target>,
         cx: &mut gpui::Context<CompareView>| {
            let key = key.clone();
            target.map(|target| {
                IconButton::new((id, index), IconName::ArrowUpRight)
                    .icon_size(IconSize::XSmall)
                    .tooltip(Tooltip::text(label))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_row(&target, &key, window, cx)
                    }))
            })
        };
    let has_left = !matches!(row, CompareRow::RightOnly { .. });
    let has_right = !matches!(row, CompareRow::LeftOnly { .. });
    let key_cell = h_flex()
        .gap_1()
        .child(Label::new(key_text(&key)).size(LabelSize::Small))
        .children(open_button(
            "open-left",
            "Open the left row",
            comparison.left_target.clone().filter(|_| has_left),
            cx,
        ))
        .children(open_button(
            "open-right",
            "Open the right row",
            comparison.right_target.clone().filter(|_| has_right),
            cx,
        ))
        .into_any_element();
    let mut cells = match row {
        CompareRow::LeftOnly { row, .. } => vec![
            Label::new("left only")
                .size(LabelSize::Small)
                .color(Color::Deleted)
                .into_any_element(),
            Label::new(summarize_row(&comparison.left, *row))
                .size(LabelSize::Small)
                .color(Color::Muted)
                .single_line()
                .truncate()
                .into_any_element(),
        ],
        CompareRow::RightOnly { row, .. } => vec![
            Label::new("right only")
                .size(LabelSize::Small)
                .color(Color::Created)
                .into_any_element(),
            Label::new(summarize_row(&comparison.right, *row))
                .size(LabelSize::Small)
                .color(Color::Muted)
                .single_line()
                .truncate()
                .into_any_element(),
        ],
        CompareRow::Matched { diffs, .. } => {
            let real = diffs
                .iter()
                .filter(|diff| !diff.representation_only)
                .count();
            let (status, color) = match (real, diffs.len()) {
                (0, 0) => ("identical", Color::Muted),
                (0, _) => ("type differs", Color::Muted),
                _ => ("differs", Color::Modified),
            };
            let differences = h_flex()
                .gap_3()
                .children(diffs.iter().map(|diff| {
                    let field = if diff.left_field == diff.right_field {
                        diff.left_field.clone()
                    } else {
                        format!("{}/{}", diff.left_field, diff.right_field)
                    };
                    Label::new(format!(
                        "{field}: {} → {}",
                        diff.left.display(),
                        diff.right.display()
                    ))
                    .size(LabelSize::Small)
                    .color(if diff.representation_only {
                        Color::Muted
                    } else {
                        Color::Default
                    })
                    .single_line()
                }))
                .into_any_element();
            vec![
                Label::new(status)
                    .size(LabelSize::Small)
                    .color(color)
                    .into_any_element(),
                differences,
            ]
        }
    };
    cells.insert(0, key_cell);
    cells
}

fn summarize_row(rows: &ResultSet, row: usize) -> String {
    rows.rows
        .get(row)
        .map(|values| {
            rows.columns
                .iter()
                .zip(values)
                .filter(|(_, value)| !value.is_nullish())
                .take(6)
                .map(|(column, value)| format!("{}={}", column.name, value.display()))
                .collect::<Vec<_>>()
                .join("  ")
        })
        .unwrap_or_default()
}

fn input(editor: Entity<Editor>, cx: &App) -> impl IntoElement {
    div()
        .px_1()
        .py_0p5()
        .border_1()
        .rounded_sm()
        .border_color(cx.theme().colors().border)
        .child(editor)
}

/// A side's source text is SQL when the connection has a query language and
/// the text reads like a statement; otherwise it is `table [where filter]`.
fn fetch_source(
    text: &str,
    language: QueryLanguage,
    kind: Option<DatabaseKind>,
) -> Result<FetchSource> {
    let text = text.trim();
    anyhow::ensure!(!text.is_empty(), "enter a table or query on both sides");
    let first_word = text
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    if language != QueryLanguage::None && matches!(first_word.as_str(), "SELECT" | "WITH" | "(") {
        return Ok(FetchSource::Query(text.to_string()));
    }
    let lowered = text.to_ascii_lowercase();
    let (table, filter) = match lowered.find(" where ") {
        Some(index) => (&text[..index], &text[index + " where ".len()..]),
        None => (text, ""),
    };
    let table = table.trim();
    let collection = match table.split_once('.') {
        Some((namespace, name)) if kind != Some(DatabaseKind::Convex) => {
            CollectionRef::new(Some(namespace), name)
        }
        _ => CollectionRef::new(None, table),
    };
    Ok(FetchSource::Collection {
        collection,
        filters: parse_filters(filter, kind == Some(DatabaseKind::Convex))?,
    })
}

impl EventEmitter<ItemEvent> for CompareView {}

impl Focusable for CompareView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for CompareView {
    type Event = ItemEvent;

    fn tab_content_text(&self, _: usize, cx: &App) -> SharedString {
        let name = self.name.read(cx).text(cx);
        if name.trim().is_empty() {
            "Compare".into()
        } else {
            format!("Compare · {}", name.trim()).into()
        }
    }

    fn tab_icon(&self, _: &Window, _: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Diff))
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

impl Render for CompareView {
    fn render(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let show = self.show;
        v_flex()
            .key_context("DatabaseCompare")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .child(
                v_flex()
                    .p_2()
                    .gap_2()
                    .border_b_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(
                        h_flex()
                            .gap_3()
                            .items_start()
                            .child(self.render_side(true, window, cx))
                            .child(self.render_side(false, window, cx)),
                    )
                    .child(input(self.fields.clone(), cx))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("compare", "Compare")
                                    .style(ButtonStyle::Filled)
                                    .start_icon(Icon::new(IconName::Diff).size(IconSize::Small))
                                    .on_click(cx.listener(|this, _, _, cx| this.run(cx))),
                            )
                            .child(
                                Button::new(
                                    "show",
                                    match show {
                                        Show::Differences => "Showing differences",
                                        Show::All => "Showing all rows",
                                    },
                                )
                                .tooltip(Tooltip::text("Toggle identical rows"))
                                .on_click(cx.listener(
                                    |this, _, _, cx| {
                                        this.show = match this.show {
                                            Show::Differences => Show::All,
                                            Show::All => Show::Differences,
                                        };
                                        cx.notify();
                                    },
                                )),
                            )
                            .child(div().flex_1())
                            .child(div().w(rems(14.)).child(input(self.name.clone(), cx)))
                            .child(
                                Button::new("save", "Save")
                                    .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
                            ),
                    )
                    .when_some(self.status.clone(), |this, (status, is_error)| {
                        this.child(
                            Label::new(status)
                                .size(LabelSize::Small)
                                .color(if is_error { Color::Error } else { Color::Muted }),
                        )
                    }),
            )
            .child(div().flex_1().min_h_0().child(self.render_results(cx)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fetch_source() {
        assert_eq!(
            fetch_source(
                "SELECT id FROM seller",
                QueryLanguage::MySql,
                Some(DatabaseKind::Mysql)
            )
            .unwrap(),
            FetchSource::Query("SELECT id FROM seller".into())
        );
        let FetchSource::Collection {
            collection,
            filters,
        } = fetch_source(
            "ryse.seller where active = true",
            QueryLanguage::MySql,
            Some(DatabaseKind::Mysql),
        )
        .unwrap()
        else {
            panic!("expected a collection");
        };
        assert_eq!(collection, CollectionRef::new(Some("ryse"), "seller"));
        assert_eq!(filters.len(), 1);
        let FetchSource::Collection { collection, .. } = fetch_source(
            "ryseSellerUserLinks",
            QueryLanguage::None,
            Some(DatabaseKind::Convex),
        )
        .unwrap() else {
            panic!("expected a collection");
        };
        assert_eq!(collection, CollectionRef::new(None, "ryseSellerUserLinks"));
    }
}
