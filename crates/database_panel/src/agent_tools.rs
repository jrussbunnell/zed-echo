//! Database tools for agents, served over MCP from inside Echo so approvals
//! can open in the workspace. Registered as a built-in context server, which
//! hands it to Claude Code sessions and Zed's own agent alike.

use anyhow::{Context as _, Result, anyhow, bail};
use collections::HashMap;
use context_server::{
    listener::{McpServer, McpServerTool, ToolResponse},
    types::{
        Implementation, InitializeResponse, ProtocolVersion, ServerCapabilities, ToolAnnotations,
        ToolResponseContent, ToolsCapabilities, requests,
    },
};
use database::{
    BrowseRequest, Capability, Change, ChangeSet, CollectionRef, CompareOptions, CompareRow,
    CompareSide, ConnectionEntry, ConnectionState, DatabaseAgentAccess, DatabaseKind,
    DatabaseStore, DatabaseWriteApproval, FetchSource, Filter, FilterOp, ResultSet, Value,
    compare,
};
use gpui::{AnyWindowHandle, AsyncApp, BorrowAppContext as _, Entity, EntityId, Global, Task, WeakEntity, Window};
use context_server::ContextServerCommand;
use project::project_settings::ContextServerSettings;
use schemars::JsonSchema;
use serde::Deserialize;
use workspace::Workspace;

use crate::modals::{Approval, ApprovalModal};

const SERVER_NAME: &str = "echo-database";
const DEFAULT_ROW_LIMIT: usize = 100;
const MAX_ROW_LIMIT: usize = 1000;
const COMPARE_ROW_CAP: usize = 200_000;
const COMPARE_REPORTED_ROWS: usize = 50;

/// MCP servers per project, alive as long as the project.
#[derive(Default)]
struct AgentToolServers(HashMap<EntityId, (McpServer, Vec<WeakEntity<Workspace>>)>);

impl Global for AgentToolServers {}

pub(crate) fn register(workspace: &mut Workspace, window: &mut Window, cx: &mut gpui::Context<Workspace>) {
    let project = workspace.project().clone();
    if !project.read(cx).is_local() {
        // The agent of a remote project runs on the remote host, which cannot
        // reach this machine's socket.
        return;
    }
    let project_id = project.entity_id();
    let workspace_handle = cx.entity().downgrade();
    let window_handle = window.window_handle();
    if let Some(servers) = cx.try_global::<AgentToolServers>()
        && servers.0.contains_key(&project_id)
    {
        cx.update_global::<AgentToolServers, _>(|servers, _| {
            if let Some((_, workspaces)) = servers.0.get_mut(&project_id) {
                workspaces.push(workspace_handle);
            }
        });
        return;
    }
    let store = DatabaseStore::for_project(&project, cx);
    let context = ToolContext {
        store: store.downgrade(),
        workspace: workspace_handle.clone(),
        window: window_handle,
    };
    let context_servers = project.read(cx).context_server_store();
    cx.spawn(async move |_, cx| {
        let mut server = match McpServer::new(cx).await {
            Ok(server) => server,
            Err(error) => {
                log::error!("starting the database MCP server: {error:#}");
                return;
            }
        };
        server.handle_request::<requests::Initialize>(|params, _| {
            Task::ready(Ok(InitializeResponse {
                protocol_version: ProtocolVersion(params.protocol_version.0),
                capabilities: ServerCapabilities {
                    tools: Some(ToolsCapabilities { list_changed: Some(false) }),
                    ..ServerCapabilities::default()
                },
                server_info: Implementation {
                    name: SERVER_NAME.to_string(),
                    title: Some("Echo databases".to_string()),
                    version: "1".to_string(),
                    description: None,
                },
                meta: None,
            }))
        });
        server.add_tool(ListConnections(context.clone()));
        server.add_tool(Describe(context.clone()));
        server.add_tool(Query(context.clone()));
        server.add_tool(Compare(context.clone()));
        server.add_tool(Write(context.clone()));
        let socket = server.socket_path().to_string_lossy().to_string();
        cx.update(|cx| {
            context_servers.update(cx, |store, cx| {
                store.set_builtin_server(
                    SERVER_NAME.into(),
                    Some(ContextServerSettings::Stdio {
                        enabled: true,
                        remote: false,
                        command: ContextServerCommand {
                            // ponytail: macOS ships `nc -U` to bridge stdio to the
                            // socket. A bridge mode in the zed binary would cover
                            // platforms without it.
                            path: "/usr/bin/nc".into(),
                            args: vec!["-U".to_string(), socket],
                            env: None,
                            timeout: None,
                        },
                    }),
                    cx,
                );
            });
            cx.default_global::<AgentToolServers>()
                .0
                .insert(project_id, (server, vec![workspace_handle]));
        });
        cx.update(|cx| {
            cx.observe_release(&project, move |_, cx| {
                if cx.has_global::<AgentToolServers>() {
                    cx.global_mut::<AgentToolServers>().0.remove(&project_id);
                }
            })
            .detach();
        });
    })
    .detach();
}

#[derive(Clone)]
struct ToolContext {
    store: WeakEntity<DatabaseStore>,
    workspace: WeakEntity<Workspace>,
    window: AnyWindowHandle,
}

impl ToolContext {
    fn store(&self) -> Result<Entity<DatabaseStore>> {
        self.store.upgrade().context("the project was closed")
    }

    /// The connection, if agents may use it at all.
    fn entry(&self, connection: &str, cx: &AsyncApp) -> Result<ConnectionEntry> {
        let store = self.store()?;
        let entry = store
            .read_with(cx, |store, _| store.entry(connection).cloned())
            .ok_or_else(|| anyhow!("no connection named `{connection}`; call list_connections"))?;
        if entry.agent_access == DatabaseAgentAccess::Off {
            bail!(
                "`{connection}` is not available to agents. The user can allow it with \"agent_access\" in the connection's settings."
            );
        }
        Ok(entry)
    }

    async fn ensure_connected(&self, connection: &str, cx: &mut AsyncApp) -> Result<(Capability, DatabaseKind)> {
        let store = self.store()?;
        let state = store.read_with(cx, |store, _| store.state(connection));
        if !matches!(state, ConnectionState::Connected { .. }) {
            store.update(cx, |store, cx| store.connect(connection, cx)).await?;
        }
        store.read_with(cx, |store, _| match store.state(connection) {
            ConnectionState::Connected { capability, .. } => Ok((
                capability,
                store.entry(connection).map(|entry| entry.spec.kind).unwrap_or(DatabaseKind::Mysql),
            )),
            _ => Err(anyhow!("`{connection}` is not connected")),
        })
    }

    /// Asks the user in the workspace; errors when they reject.
    async fn approve(&self, approval: Approval, cx: &mut AsyncApp) -> Result<()> {
        let workspace = self.workspace.clone();
        let receiver = self
            .window
            .update(cx, |_, window, cx| {
                workspace.update(cx, |workspace, cx| ApprovalModal::ask(workspace, approval, window, cx))
            })
            .context("the Echo window was closed")??;
        if receiver.await.unwrap_or(false) {
            Ok(())
        } else {
            bail!("the user rejected this request")
        }
    }

    async fn collection(&self, connection: &str, table: &str, cx: &mut AsyncApp) -> Result<CollectionRef> {
        let store = self.store()?;
        let known = store.read_with(cx, |store, _| store.collections(connection).map(<[_]>::to_vec));
        let collections = match known {
            Some(collections) => collections,
            None => store.update(cx, |store, cx| store.refresh_collections(connection, cx)).await?,
        };
        let (namespace, name) = match table.split_once('.') {
            Some((namespace, name)) => (Some(namespace), name),
            None => (None, table),
        };
        let matches = collections
            .iter()
            .filter(|collection| {
                collection.reference.name == name
                    && namespace.is_none_or(|namespace| collection.reference.namespace.as_deref() == Some(namespace))
            })
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [only] => Ok(only.reference.clone()),
            [] => bail!("no table `{table}` in `{connection}`; call describe without a table to list them"),
            _ => bail!("`{table}` is ambiguous in `{connection}`; qualify it as schema.table"),
        }
    }
}

fn text(text: String) -> ToolResponse<()> {
    ToolResponse {
        content: vec![ToolResponseContent::Text { text }],
        structured_content: (),
    }
}

fn rows_json(rows: &ResultSet) -> serde_json::Value {
    serde_json::json!({
        "columns": rows.columns.iter().map(|column| &column.name).collect::<Vec<_>>(),
        "rows": rows.rows.iter().map(|row| row.iter().map(Value::to_json).collect::<Vec<_>>()).collect::<Vec<_>>(),
        "truncated": rows.truncated,
    })
}

/// Converts a JSON argument into a cell value. Convex numbers are float64
/// unless tagged, so plain JSON numbers stay floats there.
fn value_from_json(value: &serde_json::Value, kind: DatabaseKind) -> Value {
    match value {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(value) => Value::Bool(*value),
        serde_json::Value::Number(number) => match (kind, number.as_i64()) {
            (DatabaseKind::Convex, _) | (_, None) => Value::Float(number.as_f64().unwrap_or(f64::NAN)),
            (_, Some(integer)) => Value::Int(integer),
        },
        serde_json::Value::String(text) => Value::Text(text.clone()),
        other => Value::Json(other.clone()),
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
struct FilterInput {
    /// Field name.
    field: String,
    /// One of eq, neq, lt, lte, gt, gte, contains, is_null, is_not_null.
    op: String,
    /// Value to compare against; omit for is_null and is_not_null.
    value: Option<serde_json::Value>,
}

fn filters(inputs: &[FilterInput], kind: DatabaseKind) -> Result<Vec<Filter>> {
    inputs
        .iter()
        .map(|input| {
            let op = match input.op.as_str() {
                "eq" => FilterOp::Eq,
                "neq" => FilterOp::NotEq,
                "lt" => FilterOp::Lt,
                "lte" => FilterOp::Lte,
                "gt" => FilterOp::Gt,
                "gte" => FilterOp::Gte,
                "contains" => FilterOp::Contains,
                "is_null" => FilterOp::IsNull,
                "is_not_null" => FilterOp::IsNotNull,
                other => bail!("unknown filter op `{other}`"),
            };
            Ok(Filter {
                field: input.field.clone(),
                op,
                value: input
                    .value
                    .as_ref()
                    .map(|value| value_from_json(value, kind))
                    .unwrap_or(Value::Null),
            })
        })
        .collect()
}

#[derive(Clone)]
struct ListConnections(ToolContext);

/// Lists the databases this project's agents may use, with each one's engine, environment, access policy and whether it accepts writes.
#[derive(Deserialize, JsonSchema)]
struct ListConnectionsInput {}

impl McpServerTool for ListConnections {
    type Input = ListConnectionsInput;
    type Output = ();
    const NAME: &'static str = "list_connections";

    fn annotations(&self) -> ToolAnnotations {
        read_only_annotations("List database connections")
    }

    async fn run(&self, _: Self::Input, cx: &mut AsyncApp) -> Result<ToolResponse<()>> {
        let store = self.0.store()?;
        let listing = store.read_with(cx, |store, _| {
            store
                .connections()
                .iter()
                .filter(|entry| entry.agent_access != DatabaseAgentAccess::Off)
                .map(|entry| {
                    let state = match store.state(&entry.name) {
                        ConnectionState::Connected { capability: Capability::Writable, .. } => "connected, writable".to_string(),
                        ConnectionState::Connected { capability: Capability::ReadOnly { reason }, .. } => format!("connected, read-only ({reason})"),
                        _ => "not connected (connects on first use)".to_string(),
                    };
                    serde_json::json!({
                        "name": entry.name.as_ref(),
                        "kind": database::kind_name(entry.spec.kind),
                        "environment": format!("{:?}", entry.environment).to_lowercase(),
                        "agent_access": format!("{:?}", entry.agent_access).to_lowercase(),
                        "state": state,
                        "query": if entry.spec.kind == DatabaseKind::Convex { "browse tables with filters" } else { "SQL" },
                    })
                })
                .collect::<Vec<_>>()
        });
        Ok(text(serde_json::to_string_pretty(&listing)?))
    }
}

#[derive(Clone)]
struct Describe(ToolContext);

/// Describes a database connection: lists its tables, or, given a table, that table's fields, primary key and references to and from other tables.
#[derive(Deserialize, JsonSchema)]
struct DescribeInput {
    /// Connection name from list_connections.
    connection: String,
    /// Table to describe (schema.table where needed). Omit to list tables.
    table: Option<String>,
}

impl McpServerTool for Describe {
    type Input = DescribeInput;
    type Output = ();
    const NAME: &'static str = "describe";

    fn annotations(&self) -> ToolAnnotations {
        read_only_annotations("Describe a database or table")
    }

    async fn run(&self, input: Self::Input, cx: &mut AsyncApp) -> Result<ToolResponse<()>> {
        self.0.entry(&input.connection, cx)?;
        self.0.ensure_connected(&input.connection, cx).await?;
        let store = self.0.store()?;
        let Some(table) = input.table else {
            let collections = store
                .update(cx, |store, cx| store.refresh_collections(&input.connection, cx))
                .await?;
            let names = collections
                .iter()
                .map(|collection| collection.reference.qualified_name())
                .collect::<Vec<_>>();
            return Ok(text(names.join("\n")));
        };
        let collection = self.0.collection(&input.connection, &table, cx).await?;
        let schema = store
            .update(cx, |store, cx| store.describe(&input.connection, &collection, cx))
            .await?;
        Ok(text(serde_json::to_string_pretty(&schema)?))
    }
}

#[derive(Clone)]
struct Query(ToolContext);

/// Reads rows. For SQL databases pass `sql`: one reading statement, run in a read-only transaction, so writes are refused. Or pass `table` with optional `filters` to browse a table (the only option for Convex).
#[derive(Deserialize, JsonSchema)]
struct QueryInput {
    /// Connection name from list_connections.
    connection: String,
    /// A single reading SQL statement.
    sql: Option<String>,
    /// A table to browse instead of running SQL.
    table: Option<String>,
    /// Filters for `table`, combined with AND.
    filters: Option<Vec<FilterInput>>,
    /// Maximum rows to return (default 100, at most 1000).
    limit: Option<usize>,
}

impl McpServerTool for Query {
    type Input = QueryInput;
    type Output = ();
    const NAME: &'static str = "query";

    fn annotations(&self) -> ToolAnnotations {
        read_only_annotations("Read rows from a database")
    }

    async fn run(&self, input: Self::Input, cx: &mut AsyncApp) -> Result<ToolResponse<()>> {
        let entry = self.0.entry(&input.connection, cx)?;
        let (_, kind) = self.0.ensure_connected(&input.connection, cx).await?;
        let limit = input.limit.unwrap_or(DEFAULT_ROW_LIMIT).clamp(1, MAX_ROW_LIMIT);
        let store = self.0.store()?;
        let filters = filters(input.filters.as_deref().unwrap_or_default(), kind)?;
        let (description, rows) = match (&input.sql, &input.table) {
            (Some(sql), _) => {
                if entry.agent_access == DatabaseAgentAccess::Approve {
                    self.0
                        .approve(
                            Approval {
                                title: format!("Let the agent query {}?", entry.name).into(),
                                subtitle: Some(format!("{:?} · read-only", entry.environment).to_lowercase().into()),
                                detail: sql.clone(),
                                confirm_label: "Run".into(),
                                environment: entry.environment,
                            },
                            cx,
                        )
                        .await?;
                }
                let rows = store
                    .update(cx, |store, cx| store.query(&input.connection, sql.clone(), limit, true, cx))
                    .await?;
                (sql.clone(), rows)
            }
            (None, Some(table)) => {
                let collection = self.0.collection(&input.connection, table, cx).await?;
                let description = format!("{} {filters:?}", collection.qualified_name());
                if entry.agent_access == DatabaseAgentAccess::Approve {
                    self.0
                        .approve(
                            Approval {
                                title: format!("Let the agent read {}?", entry.name).into(),
                                subtitle: Some(format!("{:?}", entry.environment).to_lowercase().into()),
                                detail: description.clone(),
                                confirm_label: "Read".into(),
                                environment: entry.environment,
                            },
                            cx,
                        )
                        .await?;
                }
                let page = store
                    .update(cx, |store, cx| {
                        store.browse(
                            &input.connection,
                            &collection,
                            BrowseRequest { filters, cursor: None, page_size: limit },
                            cx,
                        )
                    })
                    .await?;
                let mut rows = page.rows;
                rows.truncated = page.next_cursor.is_some();
                (description, rows)
            }
            (None, None) => bail!("pass `sql` or `table`"),
        };
        log::info!("agent read {} rows from {}: {description}", rows.rows.len(), input.connection);
        Ok(text(serde_json::to_string(&rows_json(&rows))?))
    }
}

#[derive(Clone)]
struct Compare(ToolContext);

#[derive(Deserialize, JsonSchema)]
struct CompareSideInput {
    /// Connection name from list_connections.
    connection: String,
    /// A reading SQL statement for this side.
    sql: Option<String>,
    /// A table for this side instead of SQL.
    table: Option<String>,
    /// Filters for `table`.
    filters: Option<Vec<FilterInput>>,
    /// Key fields that identify a row on this side.
    key: Vec<String>,
}

#[derive(Deserialize, JsonSchema)]
struct FieldPair {
    left: String,
    right: String,
}

/// Compares two row sets by key, within one database or across engines (for example MySQL against Convex). Reports rows missing from either side and fields whose values differ; representation-only differences (1 vs 1.0, DATETIME vs epoch ms, NULL vs missing) are flagged separately.
#[derive(Deserialize, JsonSchema)]
struct CompareInput {
    left: CompareSideInput,
    right: CompareSideInput,
    /// Fields to compare; omit to compare fields with the same name.
    fields: Option<Vec<FieldPair>>,
}

impl McpServerTool for Compare {
    type Input = CompareInput;
    type Output = ();
    const NAME: &'static str = "compare";

    fn annotations(&self) -> ToolAnnotations {
        read_only_annotations("Compare rows across databases")
    }

    async fn run(&self, input: Self::Input, cx: &mut AsyncApp) -> Result<ToolResponse<()>> {
        let mut fetched = Vec::new();
        for side in [&input.left, &input.right] {
            let entry = self.0.entry(&side.connection, cx)?;
            let (_, kind) = self.0.ensure_connected(&side.connection, cx).await?;
            let source = match (&side.sql, &side.table) {
                (Some(sql), _) => FetchSource::Query(sql.clone()),
                (None, Some(table)) => FetchSource::Collection {
                    collection: self.0.collection(&side.connection, table, cx).await?,
                    filters: filters(side.filters.as_deref().unwrap_or_default(), kind)?,
                },
                (None, None) => bail!("each side needs `sql` or `table`"),
            };
            if entry.agent_access == DatabaseAgentAccess::Approve {
                self.0
                    .approve(
                        Approval {
                            title: format!("Let the agent read {} for a comparison?", entry.name).into(),
                            subtitle: Some(format!("{:?}", entry.environment).to_lowercase().into()),
                            detail: format!("{source:?}"),
                            confirm_label: "Read".into(),
                            environment: entry.environment,
                        },
                        cx,
                    )
                    .await?;
            }
            let store = self.0.store()?;
            let rows = store
                .update(cx, |store, cx| store.fetch_all(&side.connection, source, COMPARE_ROW_CAP, cx))
                .await?;
            fetched.push(rows);
        }
        let right = fetched.pop().context("missing right side")?;
        let left = fetched.pop().context("missing left side")?;
        let outcome = compare(
            CompareSide { rows: &left, key: &input.left.key },
            CompareSide { rows: &right, key: &input.right.key },
            &CompareOptions {
                fields: input
                    .fields
                    .unwrap_or_default()
                    .into_iter()
                    .map(|pair| (pair.left, pair.right))
                    .collect(),
            },
        )?;
        let differences = outcome
            .rows
            .iter()
            .filter(|row| row.has_real_difference())
            .take(COMPARE_REPORTED_ROWS)
            .map(|row| match row {
                CompareRow::LeftOnly { key, .. } => serde_json::json!({ "key": key.iter().map(Value::to_json).collect::<Vec<_>>(), "status": "left_only" }),
                CompareRow::RightOnly { key, .. } => serde_json::json!({ "key": key.iter().map(Value::to_json).collect::<Vec<_>>(), "status": "right_only" }),
                CompareRow::Matched { key, diffs, .. } => serde_json::json!({
                    "key": key.iter().map(Value::to_json).collect::<Vec<_>>(),
                    "status": "differs",
                    "fields": diffs.iter().filter(|diff| !diff.representation_only).map(|diff| serde_json::json!({
                        "left_field": diff.left_field,
                        "right_field": diff.right_field,
                        "left": diff.left.to_json(),
                        "right": diff.right.to_json(),
                    })).collect::<Vec<_>>(),
                }),
            })
            .collect::<Vec<_>>();
        let report = serde_json::json!({
            "left_rows": left.rows.len(),
            "right_rows": right.rows.len(),
            "left_only": outcome.left_only(),
            "right_only": outcome.right_only(),
            "differ": outcome.changed(),
            "identical": outcome.identical(),
            "duplicate_left_keys": outcome.duplicate_left_keys,
            "duplicate_right_keys": outcome.duplicate_right_keys,
            "compared_fields": outcome.compared_fields,
            "first_differences": differences,
        });
        Ok(text(serde_json::to_string_pretty(&report)?))
    }
}

#[derive(Clone)]
struct Write(ToolContext);

#[derive(Deserialize, JsonSchema)]
struct ChangeInput {
    /// insert, update or delete.
    op: String,
    /// The row's primary key fields and values (update and delete). Use `_id` for Convex.
    key: Option<serde_json::Map<String, serde_json::Value>>,
    /// Field values to set (update) or the new row (insert).
    values: Option<serde_json::Map<String, serde_json::Value>>,
}

/// Changes rows in one table. Each change is an insert, an update, or a delete addressed by primary key, and each update or delete may touch at most one row. The user approves the exact statements before they run unless they chose to rely on your permission prompt.
#[derive(Deserialize, JsonSchema)]
struct WriteInput {
    /// Connection name from list_connections.
    connection: String,
    /// Table to change (schema.table where needed).
    table: String,
    changes: Vec<ChangeInput>,
}

impl McpServerTool for Write {
    type Input = WriteInput;
    type Output = ();
    const NAME: &'static str = "write";

    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            title: Some("Change rows in a database".to_string()),
            read_only_hint: Some(false),
            destructive_hint: Some(true),
            idempotent_hint: Some(false),
            open_world_hint: Some(false),
        }
    }

    async fn run(&self, input: Self::Input, cx: &mut AsyncApp) -> Result<ToolResponse<()>> {
        let entry = self.0.entry(&input.connection, cx)?;
        let (capability, kind) = self.0.ensure_connected(&input.connection, cx).await?;
        if let Capability::ReadOnly { reason } = capability {
            bail!("`{}` is read-only: {reason}", input.connection);
        }
        let collection = self.0.collection(&input.connection, &input.table, cx).await?;
        let assignments = |map: &Option<serde_json::Map<String, serde_json::Value>>| {
            map.iter()
                .flatten()
                .map(|(field, value)| (field.clone(), value_from_json(value, kind)))
                .collect::<Vec<_>>()
        };
        let changes = input
            .changes
            .iter()
            .map(|change| {
                Ok(match change.op.as_str() {
                    "insert" => Change::Insert { values: assignments(&change.values) },
                    "update" => Change::Update {
                        key: assignments(&change.key),
                        set: assignments(&change.values),
                    },
                    "delete" => Change::Delete { key: assignments(&change.key) },
                    other => bail!("unknown op `{other}`; use insert, update or delete"),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        anyhow::ensure!(!changes.is_empty(), "no changes given");
        let changes = ChangeSet { collection, changes };
        let store = self.0.store()?;
        let preview = store
            .update(cx, |store, cx| store.render_changes(&input.connection, changes.clone(), cx))
            .await?;
        // Agent writes are always approved: in Echo with the exact statements
        // (`preview`), or by the agent's own permission prompt (`prompt`).
        if entry.write_approval == DatabaseWriteApproval::Preview {
            self.0
                .approve(
                    Approval {
                        title: format!("Let the agent change {}?", entry.name).into(),
                        subtitle: Some(format!("{:?} · {} change(s)", entry.environment, changes.changes.len()).to_lowercase().into()),
                        detail: preview.clone(),
                        confirm_label: "Apply".into(),
                        environment: entry.environment,
                    },
                    cx,
                )
                .await?;
        }
        let count = store
            .update(cx, |store, cx| store.apply(&input.connection, changes, cx))
            .await?;
        Ok(text(format!("Applied {count} change(s):\n{preview}")))
    }
}

fn read_only_annotations(title: &str) -> ToolAnnotations {
    ToolAnnotations {
        title: Some(title.to_string()),
        read_only_hint: Some(true),
        destructive_hint: Some(false),
        idempotent_hint: Some(true),
        open_world_hint: Some(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_json_numbers_follow_the_engine() {
        let number = serde_json::json!(2);
        assert_eq!(value_from_json(&number, DatabaseKind::Mysql), Value::Int(2));
        assert_eq!(value_from_json(&number, DatabaseKind::Convex), Value::Float(2.0));
        assert_eq!(value_from_json(&serde_json::json!(2.5), DatabaseKind::Mysql), Value::Float(2.5));
    }
}
