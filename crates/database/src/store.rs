use std::{sync::Arc, time::Duration};

use anyhow::{Context as _, Result, anyhow, bail};
use collections::{BTreeMap, HashMap};
use db::{
    kvp::KeyValueStore,
    sqlez::{domain::Domain, thread_safe_connection::ThreadSafeConnection},
    sqlez_macros::sql,
};
use gpui::{
    App, AppContext as _, AsyncApp, Context, Entity, EntityId, EventEmitter, Global, SharedString,
    Task, TaskExt as _, WeakEntity,
};
use project::{Project, trusted_worktrees::TrustedWorktrees};
use rpc::{AnyProtoClient, TypedEnvelope, proto};
use serde::{Deserialize, Serialize};
use settings::{
    DatabaseAgentAccess, DatabaseConnectionContent, DatabaseEnvironment, DatabaseKind,
    DatabaseWriteApproval, RegisterSetting, Settings, SettingsLocation,
};
use util::{ResultExt as _, rel_path::RelPath};

use crate::{
    BrowseRequest, Capability, ChangeSet, Collection, CollectionRef, CollectionSchema,
    ConnectionSpec, Engine, Filter, HistoryEntry, Page, QueryLanguage, Request, Response,
    ResultSet,
};

#[derive(Clone, Debug, PartialEq, RegisterSetting)]
pub struct DatabaseSettings {
    pub connections: BTreeMap<Arc<str>, DatabaseConnectionContent>,
    pub write_approval: DatabaseWriteApproval,
}

impl Settings for DatabaseSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let database = content.project.database.clone().unwrap_or_default();
        Self {
            connections: database.connections.unwrap_or_default(),
            write_approval: database
                .write_approval
                .unwrap_or(DatabaseWriteApproval::Preview),
        }
    }
}

/// A connection as resolved for one project.
#[derive(Clone, Debug, PartialEq)]
pub struct ConnectionEntry {
    pub name: Arc<str>,
    pub spec: ConnectionSpec,
    pub environment: DatabaseEnvironment,
    pub agent_access: DatabaseAgentAccess,
    pub write_approval: DatabaseWriteApproval,
    pub history: bool,
    /// Defined in the project's settings rather than the user's. Such
    /// connections run where the project lives (on the remote host for SSH
    /// projects); user connections always run locally.
    pub project_defined: bool,
    /// Why the connection cannot be used, if it is misconfigured.
    pub error: Option<SharedString>,
}

impl ConnectionEntry {
    pub fn needs_secret(&self) -> bool {
        matches!(
            self.spec.kind,
            DatabaseKind::Convex | DatabaseKind::ClickhouseRelay
        )
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ConnectionState {
    Disconnected,
    Connecting,
    Connected {
        capability: Capability,
        language: QueryLanguage,
    },
    NeedsSecret,
    Failed(SharedString),
}

/// Returned when a connection needs a secret that is not in the keychain.
#[derive(Debug)]
pub struct MissingSecret {
    pub connection: Arc<str>,
}

impl std::fmt::Display for MissingSecret {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{} needs a secret", self.connection)
    }
}

impl std::error::Error for MissingSecret {}

pub enum DatabaseStoreEvent {
    ConnectionsChanged,
    StateChanged(Arc<str>),
    HistoryChanged,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QueryHistoryEntry {
    pub connection: String,
    pub text: String,
    /// Seconds since the epoch.
    pub ran_at: i64,
    pub rows: Option<usize>,
    pub error: Option<String>,
}

const QUERY_HISTORY_KEY: &str = "database_query_history";
const QUERY_HISTORY_LIMIT: usize = 500;

#[derive(Default)]
struct DatabaseStores(HashMap<EntityId, Entity<DatabaseStore>>);

impl Global for DatabaseStores {}

/// The database connections of one project: their settings, state and the
/// engine (local or remote) that runs their requests.
pub struct DatabaseStore {
    project: WeakEntity<Project>,
    engine: Arc<Engine>,
    connections: Vec<ConnectionEntry>,
    states: HashMap<Arc<str>, ConnectionState>,
    collections: HashMap<Arc<str>, Vec<Collection>>,
    schemas: HashMap<(Arc<str>, CollectionRef), CollectionSchema>,
    query_history: Vec<QueryHistoryEntry>,
    indexers: HashMap<Arc<str>, Task<()>>,
    _subscriptions: Vec<gpui::Subscription>,
}

impl EventEmitter<DatabaseStoreEvent> for DatabaseStore {}

impl DatabaseStore {
    /// The store for `project`, created on first use and dropped with it.
    pub fn for_project(project: &Entity<Project>, cx: &mut App) -> Entity<Self> {
        let project_id = project.entity_id();
        if let Some(store) = cx
            .try_global::<DatabaseStores>()
            .and_then(|stores| stores.0.get(&project_id))
        {
            return store.clone();
        }
        let store = cx.new(|cx| Self::new(project, cx));
        cx.default_global::<DatabaseStores>()
            .0
            .insert(project_id, store.clone());
        cx.observe_release(project, move |_, cx| {
            if cx.has_global::<DatabaseStores>() {
                cx.global_mut::<DatabaseStores>().0.remove(&project_id);
            }
        })
        .detach();
        store
    }

    fn new(project: &Entity<Project>, cx: &mut Context<Self>) -> Self {
        let query_history = KeyValueStore::global(cx)
            .read_kvp(QUERY_HISTORY_KEY)
            .log_err()
            .flatten()
            .and_then(|json| serde_json::from_str(&json).log_err())
            .unwrap_or_default();
        let mut subscriptions = vec![cx.observe_global::<settings::SettingsStore>(|this, cx| {
            this.reload_connections(cx);
        })];
        let worktree_store = project.read(cx).worktree_store();
        subscriptions.push(cx.observe(&worktree_store, |this, _, cx| {
            this.reload_connections(cx);
        }));
        if let Some(trusted) = TrustedWorktrees::try_get_global(cx) {
            subscriptions.push(cx.observe(&trusted, |this, _, cx| this.reload_connections(cx)));
        }
        let mut this = Self {
            project: project.downgrade(),
            engine: Arc::new(Engine::new(cx.http_client())),
            connections: Vec::new(),
            states: HashMap::default(),
            collections: HashMap::default(),
            schemas: HashMap::default(),
            query_history,
            indexers: HashMap::default(),
            _subscriptions: subscriptions,
        };
        this.reload_connections(cx);
        this
    }

    pub fn connections(&self) -> &[ConnectionEntry] {
        &self.connections
    }

    pub fn entry(&self, name: &str) -> Option<&ConnectionEntry> {
        self.connections.iter().find(|entry| &*entry.name == name)
    }

    pub fn state(&self, name: &str) -> ConnectionState {
        self.states
            .get(name)
            .cloned()
            .unwrap_or(ConnectionState::Disconnected)
    }

    pub fn collections(&self, name: &str) -> Option<&[Collection]> {
        self.collections.get(name).map(Vec::as_slice)
    }

    pub fn query_history(&self) -> &[QueryHistoryEntry] {
        &self.query_history
    }

    pub fn write_approval(&self, name: &str) -> DatabaseWriteApproval {
        self.entry(name)
            .map(|entry| entry.write_approval)
            .unwrap_or(DatabaseWriteApproval::Preview)
    }

    fn reload_connections(&mut self, cx: &mut Context<Self>) {
        let Some(project) = self.project.upgrade() else {
            return;
        };
        let connections = resolve_connections(&project, cx);
        if connections != self.connections {
            let changed = self
                .connections
                .iter()
                .filter(|entry| {
                    connections
                        .iter()
                        .find(|candidate| candidate.name == entry.name)
                        .is_none_or(|candidate| candidate.spec != entry.spec)
                })
                .map(|entry| entry.name.clone())
                .collect::<Vec<_>>();
            for name in changed {
                self.forget(&name, cx);
            }
            self.connections = connections;
            cx.emit(DatabaseStoreEvent::ConnectionsChanged);
            cx.notify();
        }
    }

    fn forget(&mut self, name: &Arc<str>, cx: &mut Context<Self>) {
        self.states.remove(name);
        self.collections.remove(name);
        self.schemas.retain(|(connection, _), _| connection != name);
        self.indexers.remove(name);
        let request = Request::Disconnect {
            connection: name.to_string(),
        };
        self.send(name, request, cx).detach_and_log_err(cx);
    }

    fn set_state(&mut self, name: &Arc<str>, state: ConnectionState, cx: &mut Context<Self>) {
        self.states.insert(name.clone(), state);
        cx.emit(DatabaseStoreEvent::StateChanged(name.clone()));
        cx.notify();
    }

    fn runs_remote(&self, name: &str, cx: &App) -> bool {
        let project_is_remote = self
            .project
            .upgrade()
            .is_some_and(|project| project.read(cx).is_via_remote_server());
        project_is_remote && self.entry(name).is_some_and(|entry| entry.project_defined)
    }

    /// The keychain entry for a connection's secret. Connections that run on
    /// a remote host get their own entry per host, so a secret the user stored
    /// for local use is never forwarded to a remote machine.
    fn credential_key(&self, entry: &ConnectionEntry, cx: &App) -> String {
        let key = entry.spec.credential_key();
        if !self.runs_remote(&entry.name, cx) {
            return key;
        }
        let host = self
            .project
            .upgrade()
            .and_then(|project| project.read(cx).remote_client())
            .map(|client| client.read(cx).connection_options().host())
            .unwrap_or_default();
        format!("{key}?via={host}")
    }

    /// Sends a raw engine request, without connecting first.
    fn send(&self, name: &str, request: Request, cx: &App) -> Task<Result<Response>> {
        if self.runs_remote(name, cx) {
            let Some(client) = self
                .project
                .upgrade()
                .and_then(|project| project.read(cx).remote_client())
            else {
                return Task::ready(Err(anyhow!("the remote project is disconnected")));
            };
            let client = client.read(cx).proto_client();
            return cx.background_spawn(send_remote(client, request));
        }
        let engine = self.engine.clone();
        gpui_tokio::Tokio::spawn_result(cx, async move { engine.handle(request).await })
    }

    pub fn connect(&mut self, name: &str, cx: &mut Context<Self>) -> Task<Result<()>> {
        let Some(entry) = self.entry(name).cloned() else {
            return Task::ready(Err(anyhow!("no connection named {name}")));
        };
        if let Some(error) = &entry.error {
            return Task::ready(Err(anyhow!("{name}: {error}")));
        }
        self.set_state(&entry.name, ConnectionState::Connecting, cx);
        let credentials = database_credentials(cx);
        let key = self.credential_key(&entry, cx);
        cx.spawn(async move |this, cx| {
            let secret = credentials
                .read_credentials(&key, cx)
                .await?
                .map(|(_, secret)| String::from_utf8(secret))
                .transpose()
                .context("the stored secret is not valid UTF-8")?;
            if secret.is_none() && entry.needs_secret() {
                this.update(cx, |this, cx| {
                    this.set_state(&entry.name, ConnectionState::NeedsSecret, cx)
                })?;
                return Err(MissingSecret {
                    connection: entry.name.clone(),
                }
                .into());
            }
            let had_secret = secret.is_some();
            let request = Request::Connect {
                connection: entry.name.to_string(),
                spec: entry.spec.clone(),
                secret,
            };
            let response = this.update(cx, |this, cx| this.send(&entry.name, request, cx))?.await;
            this.update(cx, |this, cx| match response {
                Ok(Response::Connected {
                    capability,
                    language,
                }) => {
                    this.set_state(
                        &entry.name,
                        ConnectionState::Connected {
                            capability,
                            language,
                        },
                        cx,
                    );
                    this.refresh_collections(&entry.name, cx).detach_and_log_err(cx);
                    if entry.history && entry.spec.kind == DatabaseKind::Convex {
                        this.start_history_index(entry.clone(), cx);
                    }
                    Ok(())
                }
                Ok(other) => Err(anyhow!("unexpected response {other:?}")),
                Err(error) if !had_secret && is_authentication_error(&error) => {
                    this.set_state(&entry.name, ConnectionState::NeedsSecret, cx);
                    Err(MissingSecret {
                        connection: entry.name.clone(),
                    }
                    .into())
                }
                Err(error) => {
                    this.set_state(
                        &entry.name,
                        ConnectionState::Failed(format!("{error:#}").into()),
                        cx,
                    );
                    Err(error)
                }
            })?
        })
    }

    /// Stores `secret` in the keychain for `name`, then connects.
    pub fn set_secret(
        &mut self,
        name: &str,
        secret: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let Some(entry) = self.entry(name).cloned() else {
            return Task::ready(Err(anyhow!("no connection named {name}")));
        };
        let credentials = database_credentials(cx);
        let key = self.credential_key(&entry, cx);
        cx.spawn(async move |this, cx| {
            credentials
                .write_credentials(
                    &key,
                    entry.spec.user.as_deref().unwrap_or("echo"),
                    secret.as_bytes(),
                    cx,
                )
                .await?;
            this.update(cx, |this, cx| this.connect(&entry.name, cx))?
                .await
        })
    }

    pub fn forget_secret(&mut self, name: &str, cx: &mut Context<Self>) -> Task<Result<()>> {
        let Some(entry) = self.entry(name).cloned() else {
            return Task::ready(Err(anyhow!("no connection named {name}")));
        };
        let key = self.credential_key(&entry, cx);
        self.forget(&entry.name, cx);
        let credentials = database_credentials(cx);
        cx.spawn(async move |_, cx| credentials.delete_credentials(&key, cx).await)
    }

    pub fn disconnect(&mut self, name: &str, cx: &mut Context<Self>) {
        if let Some(entry) = self.entry(name).cloned() {
            self.forget(&entry.name, cx);
            cx.notify();
        }
    }

    /// Sends `request`, connecting first if needed.
    pub fn request(
        &mut self,
        name: &str,
        request: Request,
        cx: &mut Context<Self>,
    ) -> Task<Result<Response>> {
        let connected = matches!(self.state(name), ConnectionState::Connected { .. });
        let name: Arc<str> = name.into();
        let connect = (!connected).then(|| self.connect(&name, cx));
        cx.spawn(async move |this, cx| {
            if let Some(connect) = connect {
                connect.await?;
            }
            this.update(cx, |this, cx| this.send(&name, request, cx))?
                .await
        })
    }

    pub fn refresh_collections(
        &mut self,
        name: &str,
        cx: &mut Context<Self>,
    ) -> Task<Result<Vec<Collection>>> {
        let name: Arc<str> = name.into();
        let request = self.request(
            &name,
            Request::Collections {
                connection: name.to_string(),
            },
            cx,
        );
        cx.spawn(async move |this, cx| {
            let Response::Collections(collections) = request.await? else {
                bail!("unexpected response");
            };
            this.update(cx, |this, cx| {
                this.collections.insert(name.clone(), collections.clone());
                cx.emit(DatabaseStoreEvent::StateChanged(name));
                cx.notify();
            })?;
            Ok(collections)
        })
    }

    pub fn describe(
        &mut self,
        name: &str,
        collection: &CollectionRef,
        cx: &mut Context<Self>,
    ) -> Task<Result<CollectionSchema>> {
        let key: (Arc<str>, CollectionRef) = (name.into(), collection.clone());
        if let Some(schema) = self.schemas.get(&key) {
            return Task::ready(Ok(schema.clone()));
        }
        let request = self.request(
            name,
            Request::Describe {
                connection: name.to_string(),
                collection: collection.clone(),
            },
            cx,
        );
        cx.spawn(async move |this, cx| {
            let Response::Schema(schema) = request.await? else {
                bail!("unexpected response");
            };
            this.update(cx, |this, _| {
                this.schemas.insert(key, schema.clone());
            })?;
            Ok(schema)
        })
    }

    /// Runs a query and records it in the query history.
    pub fn query(
        &mut self,
        name: &str,
        text: String,
        limit: usize,
        read_only: bool,
        cx: &mut Context<Self>,
    ) -> Task<Result<ResultSet>> {
        let request = self.request(
            name,
            Request::Query {
                connection: name.to_string(),
                text: text.clone(),
                limit,
                read_only,
            },
            cx,
        );
        let name = name.to_string();
        cx.spawn(async move |this, cx| {
            let result = match request.await {
                Ok(Response::Rows(rows)) => Ok(rows),
                Ok(other) => Err(anyhow!("unexpected response {other:?}")),
                Err(error) => Err(error),
            };
            this.update(cx, |this, cx| {
                this.record_query(
                    QueryHistoryEntry {
                        connection: name,
                        text,
                        ran_at: chrono::Utc::now().timestamp(),
                        rows: result.as_ref().ok().map(|rows| rows.rows.len()),
                        error: result.as_ref().err().map(|error| format!("{error:#}")),
                    },
                    cx,
                )
            })?;
            result
        })
    }

    fn record_query(&mut self, entry: QueryHistoryEntry, cx: &mut Context<Self>) {
        self.query_history
            .retain(|existing| existing.connection != entry.connection || existing.text != entry.text);
        self.query_history.insert(0, entry);
        self.query_history.truncate(QUERY_HISTORY_LIMIT);
        let json = serde_json::to_string(&self.query_history);
        let kvp = KeyValueStore::global(cx);
        cx.background_spawn(async move {
            kvp.write_kvp(QUERY_HISTORY_KEY.to_string(), json?).await
        })
        .detach_and_log_err(cx);
        cx.emit(DatabaseStoreEvent::HistoryChanged);
        cx.notify();
    }

    pub fn browse(
        &mut self,
        name: &str,
        collection: &CollectionRef,
        request: BrowseRequest,
        cx: &mut Context<Self>,
    ) -> Task<Result<Page>> {
        let request = self.request(
            name,
            Request::Browse {
                connection: name.to_string(),
                collection: collection.clone(),
                request,
            },
            cx,
        );
        cx.background_spawn(async move {
            match request.await? {
                Response::Page(page) => Ok(page),
                other => Err(anyhow!("unexpected response {other:?}")),
            }
        })
    }

    /// Reads every row of a collection (following pages) or of a query, for
    /// comparing. Fails rather than returning a partial set past `cap` rows,
    /// since a partial set would make rows look one-sided.
    pub fn fetch_all(
        &mut self,
        name: &str,
        source: FetchSource,
        cap: usize,
        cx: &mut Context<Self>,
    ) -> Task<Result<ResultSet>> {
        let name: Arc<str> = name.into();
        cx.spawn(async move |this, cx| match source {
            FetchSource::Query(text) => {
                let rows = this
                    .update(cx, |this, cx| this.query(&name, text, cap, true, cx))?
                    .await?;
                if rows.truncated {
                    bail!("{name} returned more than {cap} rows; narrow the query to compare it");
                }
                Ok(rows)
            }
            FetchSource::Collection { collection, filters } => {
                let mut all = ResultSet::default();
                let mut cursor = None;
                loop {
                    let page = this
                        .update(cx, |this, cx| {
                            this.browse(
                                &name,
                                &collection,
                                BrowseRequest {
                                    filters: filters.clone(),
                                    cursor: cursor.clone(),
                                    page_size: 1000,
                                },
                                cx,
                            )
                        })?
                        .await?;
                    merge_rows(&mut all, page.rows);
                    if all.rows.len() > cap {
                        bail!(
                            "{} has more than {cap} rows; add a filter to compare it",
                            collection.qualified_name()
                        );
                    }
                    match page.next_cursor {
                        Some(next) => cursor = Some(next),
                        None => return Ok(all),
                    }
                }
            }
        })
    }

    pub fn render_changes(
        &mut self,
        name: &str,
        changes: ChangeSet,
        cx: &mut Context<Self>,
    ) -> Task<Result<String>> {
        let request = self.request(
            name,
            Request::RenderChanges {
                connection: name.to_string(),
                changes,
            },
            cx,
        );
        cx.background_spawn(async move {
            match request.await? {
                Response::Text(text) => Ok(text),
                other => Err(anyhow!("unexpected response {other:?}")),
            }
        })
    }

    pub fn apply(
        &mut self,
        name: &str,
        changes: ChangeSet,
        cx: &mut Context<Self>,
    ) -> Task<Result<u64>> {
        let collection = changes.collection.clone();
        let request = self.request(
            name,
            Request::Apply {
                connection: name.to_string(),
                changes,
            },
            cx,
        );
        let name: Arc<str> = name.into();
        cx.spawn(async move |this, cx| {
            let count = match request.await? {
                Response::Applied(count) => count,
                other => bail!("unexpected response {other:?}"),
            };
            this.update(cx, |this, _| {
                this.schemas.remove(&(name, collection));
            })?;
            Ok(count)
        })
    }

    /// Versions of one document, newest first, from the local change index.
    pub fn row_history(
        &self,
        name: &str,
        id: &str,
        cx: &App,
    ) -> Task<Result<Vec<HistoryEntry>>> {
        let Some(entry) = self.entry(name) else {
            return Task::ready(Err(anyhow!("no connection named {name}")));
        };
        if !entry.history {
            return Task::ready(Err(anyhow!(
                "row history is off for {name}; set `\"history\": true` on the connection"
            )));
        }
        let source = entry.spec.credential_key();
        let id = id.to_string();
        let db = DatabaseRowHistory::global(cx);
        cx.background_spawn(async move { db.versions(&source, &id) })
    }

    fn start_history_index(&mut self, entry: ConnectionEntry, cx: &mut Context<Self>) {
        let db = DatabaseRowHistory::global(cx);
        let source = entry.spec.credential_key();
        let name = entry.name.clone();
        let task = cx.spawn(async move |this, cx| {
            let mut cursor = db.cursor(&source).log_err().flatten();
            loop {
                let request = Request::History {
                    connection: name.to_string(),
                    cursor: cursor.clone(),
                };
                let Ok(response) = this.update(cx, |this, cx| this.send(&name, request, cx)) else {
                    return;
                };
                match response.await {
                    Ok(Response::History(batch)) => {
                        let caught_up = batch.cursor.starts_with("delta:") && !batch.has_more;
                        let count = batch.entries.len();
                        let next = batch.cursor.clone();
                        let db = db.clone();
                        let source = source.clone();
                        let stored = cx
                            .background_spawn(async move {
                                db.store(source, batch.entries, batch.cursor).await
                            })
                            .await;
                        if let Err(error) = stored {
                            log::error!("storing database history for {name}: {error:#}");
                            return;
                        }
                        cursor = Some(next);
                        if count > 0 {
                            this.update(cx, |_, cx| cx.emit(DatabaseStoreEvent::HistoryChanged))
                                .ok();
                        }
                        if caught_up {
                            cx.background_executor().timer(Duration::from_secs(10)).await;
                        }
                    }
                    Ok(other) => {
                        log::error!("unexpected history response {other:?}");
                        return;
                    }
                    Err(error) => {
                        // Usually the streaming export API being unavailable
                        // on this plan; retrying often would only spam it.
                        log::warn!("database history for {name}: {error:#}");
                        cx.background_executor().timer(Duration::from_secs(300)).await;
                    }
                }
            }
        });
        self.indexers.insert(entry.name, task);
    }
}

/// Database passwords and deploy keys always go to the system keychain. The
/// Dev channel Echo ships on would otherwise use Zed's development
/// credentials file, which is plaintext; stateless runs (visual tests) keep
/// that file so they never touch the real keychain.
fn database_credentials(cx: &App) -> Arc<dyn credentials_provider::CredentialsProvider> {
    if *zed_env_vars::ZED_STATELESS {
        zed_credentials_provider::global(cx)
    } else {
        zed_credentials_provider::keychain()
    }
}

/// Whether a connect failure looks like missing or wrong credentials, so a
/// connection with no stored secret can ask for one instead of just failing.
fn is_authentication_error(error: &anyhow::Error) -> bool {
    let message = format!("{error:#}").to_lowercase();
    ["access denied", "authentication", "password", "unauthorized", "401", "403"]
        .iter()
        .any(|needle| message.contains(needle))
}

/// What to read for a comparison.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum FetchSource {
    Query(String),
    Collection {
        collection: CollectionRef,
        filters: Vec<Filter>,
    },
}

/// Appends `page` to `all`, widening the columns when documents disagree.
fn merge_rows(all: &mut ResultSet, page: ResultSet) {
    if all.columns.is_empty() {
        *all = page;
        return;
    }
    let mapping = page
        .columns
        .iter()
        .map(|column| match all.column_index(&column.name) {
            Some(index) => index,
            None => {
                all.columns.push(column.clone());
                for row in &mut all.rows {
                    row.push(crate::Value::Missing);
                }
                all.columns.len() - 1
            }
        })
        .collect::<Vec<_>>();
    for row in page.rows {
        let mut merged = vec![crate::Value::Missing; all.columns.len()];
        for (value, index) in row.into_iter().zip(&mapping) {
            merged[*index] = value;
        }
        all.rows.push(merged);
    }
}

async fn send_remote(client: AnyProtoClient, request: Request) -> Result<Response> {
    let response = client
        .request(proto::DatabaseRequest {
            project_id: proto::REMOTE_SERVER_PROJECT_ID,
            payload: serde_json::to_string(&request)?,
        })
        .await?;
    Ok(serde_json::from_str(&response.payload)?)
}

/// Resolves connections from user settings and the settings of each trusted
/// worktree.
fn resolve_connections(project: &Entity<Project>, cx: &mut App) -> Vec<ConnectionEntry> {
    let global = DatabaseSettings::get_global(cx).clone();
    let mut resolved: BTreeMap<Arc<str>, ConnectionEntry> = global
        .connections
        .iter()
        .map(|(name, content)| {
            (
                name.clone(),
                entry_from_content(name, content, global.write_approval, false),
            )
        })
        .collect();

    let worktree_store = project.read(cx).worktree_store();
    let worktree_ids = project
        .read(cx)
        .visible_worktrees(cx)
        .map(|worktree| worktree.read(cx).id())
        .collect::<Vec<_>>();
    let trusted = TrustedWorktrees::try_get_global(cx);
    for worktree_id in worktree_ids {
        // A cloned repository must not loosen agent access to a user's
        // databases, so untrusted worktrees contribute nothing.
        let is_trusted = trusted.as_ref().is_none_or(|trusted| {
            trusted.update(cx, |trusted, cx| {
                trusted.can_trust(&worktree_store, worktree_id, cx)
            })
        });
        if !is_trusted {
            continue;
        }
        let settings = DatabaseSettings::get(
            Some(SettingsLocation {
                worktree_id,
                path: RelPath::empty(),
            }),
            cx,
        );
        for (name, content) in &settings.connections {
            if content == global.connections.get(name).unwrap_or(&DatabaseConnectionContent::default())
                && global.connections.contains_key(name)
            {
                continue;
            }
            let entry = match global.connections.get(name) {
                Some(user) => {
                    // Overrides may change policy, never the endpoint the
                    // user's stored secret is sent to.
                    let mut merged = content.clone();
                    merged.kind = user.kind;
                    merged.host = user.host.clone();
                    merged.port = user.port;
                    merged.database = user.database.clone();
                    merged.user = user.user.clone();
                    merged.url = user.url.clone();
                    merged.ssl_mode = user.ssl_mode.clone();
                    // A read-only pin and the environment (which drives prod
                    // confirmations) can only be tightened by a project.
                    merged.read_only = Some(
                        user.read_only.unwrap_or(false) || content.read_only.unwrap_or(false),
                    );
                    merged.environment = user.environment;
                    entry_from_content(name, &merged, settings.write_approval, false)
                }
                None => entry_from_content(name, content, settings.write_approval, true),
            };
            resolved.insert(name.clone(), entry);
        }
    }
    resolved.into_values().collect()
}

fn entry_from_content(
    name: &Arc<str>,
    content: &DatabaseConnectionContent,
    default_write_approval: DatabaseWriteApproval,
    project_defined: bool,
) -> ConnectionEntry {
    let environment = content.environment.unwrap_or(DatabaseEnvironment::Dev);
    let kind = content.kind;
    let error = match kind {
        None => Some("set `kind`".into()),
        Some(DatabaseKind::Mysql | DatabaseKind::Postgres) if content.host.is_none() => {
            Some("set `host`".into())
        }
        Some(DatabaseKind::Clickhouse | DatabaseKind::ClickhouseRelay | DatabaseKind::Convex)
            if content.url.is_none() =>
        {
            Some("set `url`".into())
        }
        _ => None,
    };
    ConnectionEntry {
        name: name.clone(),
        spec: ConnectionSpec {
            kind: kind.unwrap_or(DatabaseKind::Mysql),
            host: content.host.clone(),
            port: content.port,
            database: content.database.clone(),
            user: content.user.clone(),
            url: content.url.clone(),
            ssl_mode: content.ssl_mode.clone(),
            read_only: content.read_only.unwrap_or(false)
                || kind == Some(DatabaseKind::ClickhouseRelay),
        },
        environment,
        agent_access: content.agent_access.unwrap_or(match environment {
            DatabaseEnvironment::Prod => DatabaseAgentAccess::Off,
            DatabaseEnvironment::Dev => DatabaseAgentAccess::Approve,
            DatabaseEnvironment::Local => DatabaseAgentAccess::Allow,
        }),
        write_approval: content.write_approval.unwrap_or(default_write_approval),
        history: content.history.unwrap_or(false),
        project_defined,
        error,
    }
}

/// The local index of Convex document versions behind row history.
pub struct DatabaseRowHistory(ThreadSafeConnection);

impl Domain for DatabaseRowHistory {
    const NAME: &str = stringify!(DatabaseRowHistory);

    const MIGRATIONS: &[&str] = &[sql!(
        CREATE TABLE database_row_history (
            source TEXT NOT NULL,
            table_name TEXT NOT NULL,
            document_id TEXT NOT NULL,
            timestamp INTEGER NOT NULL,
            deleted INTEGER NOT NULL,
            document TEXT NOT NULL,
            PRIMARY KEY (source, document_id, timestamp)
        ) STRICT;
        CREATE TABLE database_history_cursor (
            source TEXT PRIMARY KEY,
            cursor TEXT NOT NULL
        ) STRICT;
    )];
}

db::static_connection!(DatabaseRowHistory, []);

impl DatabaseRowHistory {
    fn cursor(&self, source: &str) -> Result<Option<String>> {
        self.select_row_bound::<&str, String>(sql!(
            SELECT cursor FROM database_history_cursor WHERE source = ?
        ))?(source)
    }

    // ponytail: versions are kept forever. Prune by age if the index grows
    // large enough to matter.
    async fn store(&self, source: String, entries: Vec<HistoryEntry>, cursor: String) -> Result<()> {
        self.write(move |connection| {
            let mut insert = connection.exec_bound::<(&str, &str, &str, i64, bool, String)>(sql!(
                INSERT OR REPLACE INTO database_row_history
                    (source, table_name, document_id, timestamp, deleted, document)
                VALUES (?, ?, ?, ?, ?, ?)
            ))?;
            for entry in &entries {
                insert((
                    &source,
                    &entry.table,
                    &entry.id,
                    entry.timestamp,
                    entry.deleted,
                    entry.document.to_string(),
                ))?;
            }
            connection.exec_bound::<(&str, &str)>(sql!(
                INSERT OR REPLACE INTO database_history_cursor (source, cursor) VALUES (?, ?)
            ))?((&source, &cursor))
        })
        .await
    }

    fn versions(&self, source: &str, id: &str) -> Result<Vec<HistoryEntry>> {
        let rows = self.select_bound::<(&str, &str), (String, String, i64, bool, String)>(sql!(
            SELECT table_name, document_id, timestamp, deleted, document
            FROM database_row_history
            WHERE source = ? AND document_id = ?
            ORDER BY timestamp DESC
        ))?((source, id))?;
        Ok(rows
            .into_iter()
            .map(|(table, id, timestamp, deleted, document)| HistoryEntry {
                table,
                id,
                timestamp,
                deleted,
                document: serde_json::from_str(&document).unwrap_or(serde_json::Value::Null),
            })
            .collect())
    }
}

/// Runs database requests on the host that owns a remote project.
pub struct DatabaseHost {
    engine: Arc<Engine>,
}

impl DatabaseHost {
    pub fn new(http_client: Arc<dyn http_client::HttpClient>) -> Self {
        Self {
            engine: Arc::new(Engine::new(http_client)),
        }
    }

    pub fn init(session: &AnyProtoClient) {
        session.add_entity_request_handler(Self::handle_request);
    }

    async fn handle_request(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::DatabaseRequest>,
        cx: AsyncApp,
    ) -> Result<proto::DatabaseResponse> {
        let request: Request = serde_json::from_str(&envelope.payload.payload)?;
        let engine = this.read_with(&cx, |this, _| this.engine.clone());
        let response = cx
            .update(|cx| {
                gpui_tokio::Tokio::spawn_result(cx, async move { engine.handle(request).await })
            })
            .await?;
        Ok(proto::DatabaseResponse {
            payload: serde_json::to_string(&response)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::FakeFs;
    use gpui::{BorrowAppContext as _, TestAppContext};
    use serde_json::json;
    use settings::SettingsStore;
    use util::path;

    fn init(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            release_channel::init(semver::Version::new(0, 0, 0), cx);
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.project.database = Some(settings::DatabaseSettingsContent {
                        connections: Some(BTreeMap::from_iter([(
                            Arc::from("ryse-prod"),
                            DatabaseConnectionContent {
                                kind: Some(DatabaseKind::Mysql),
                                host: Some("replica.internal".into()),
                                user: Some("reader".into()),
                                environment: Some(DatabaseEnvironment::Prod),
                                read_only: Some(true),
                                ..Default::default()
                            },
                        )])),
                        write_approval: None,
                    });
                });
            });
        });
    }

    async fn project_with_overrides(cx: &mut TestAppContext) -> Entity<Project> {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/atlas"),
            json!({
                ".zed": {
                    "settings.json": r#"{
                        "database": {
                            "connections": {
                                "ryse-prod": { "agent_access": "allow", "host": "evil.example.com", "read_only": false, "environment": "local" },
                                "atlas": { "kind": "convex", "url": "https://atlas.convex.cloud", "environment": "local" }
                            }
                        }
                    }"#
                }
            }),
        )
        .await;
        let project = Project::test(fs, [path!("/atlas").as_ref()], cx).await;
        cx.run_until_parked();
        project
    }

    #[gpui::test]
    async fn test_project_overrides_change_policy_not_endpoints(cx: &mut TestAppContext) {
        init(cx);
        let project = project_with_overrides(cx).await;
        let connections = cx.update(|cx| resolve_connections(&project, cx));

        let ryse = connections.iter().find(|entry| &*entry.name == "ryse-prod").unwrap();
        assert_eq!(ryse.spec.host.as_deref(), Some("replica.internal"));
        assert_eq!(ryse.agent_access, DatabaseAgentAccess::Allow);
        assert!(ryse.spec.read_only, "a project cannot lift a read-only pin");
        assert_eq!(ryse.environment, DatabaseEnvironment::Prod);
        assert!(!ryse.project_defined);

        let atlas = connections.iter().find(|entry| &*entry.name == "atlas").unwrap();
        assert!(atlas.project_defined);
        assert_eq!(atlas.agent_access, DatabaseAgentAccess::Allow);
        assert_eq!(atlas.write_approval, DatabaseWriteApproval::Preview);
    }

    #[gpui::test]
    async fn test_untrusted_worktrees_contribute_nothing(cx: &mut TestAppContext) {
        init(cx);
        cx.update(|cx| project::trusted_worktrees::init(Default::default(), cx));
        let project = project_with_overrides(cx).await;
        let connections = cx.update(|cx| resolve_connections(&project, cx));

        assert_eq!(connections.len(), 1, "{connections:?}");
        assert_eq!(connections[0].agent_access, DatabaseAgentAccess::Off);
    }
}
