use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow, bail};
use collections::HashMap;
use http_client::HttpClient;
use serde::{Deserialize, Serialize};

use crate::{
    BrowseRequest, Capability, ChangeSet, Collection, CollectionRef, CollectionSchema,
    ConnectionSpec, DatabaseKind, HistoryBatch, Page, QueryLanguage, ResultSet, Source,
    clickhouse::ClickHouseSource, convex::ConvexSource, sql::SqlSource,
};

/// A serializable request, so the same engine can run in-process or on a
/// remote host behind the project RPC.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Request {
    Connect {
        connection: String,
        spec: ConnectionSpec,
        secret: Option<String>,
    },
    Disconnect {
        connection: String,
    },
    Collections {
        connection: String,
    },
    Describe {
        connection: String,
        collection: CollectionRef,
    },
    Query {
        connection: String,
        text: String,
        limit: usize,
        read_only: bool,
    },
    Browse {
        connection: String,
        collection: CollectionRef,
        request: BrowseRequest,
    },
    RenderChanges {
        connection: String,
        changes: ChangeSet,
    },
    Apply {
        connection: String,
        changes: ChangeSet,
    },
    History {
        connection: String,
        cursor: Option<String>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Response {
    Connected {
        capability: Capability,
        language: QueryLanguage,
    },
    Disconnected,
    Collections(Vec<Collection>),
    Schema(CollectionSchema),
    Rows(ResultSet),
    Page(Page),
    Text(String),
    Applied(u64),
    History(HistoryBatch),
}

struct Connected {
    source: Arc<dyn Source>,
    capability: Capability,
    // ponytail: one request at a time per connection. The SQL drivers each hold
    // a single session, and read-only queries are bracketed by
    // START TRANSACTION / ROLLBACK on it, so concurrent requests would land
    // inside each other's transactions. A pool would lift this.
    lock: Arc<tokio::sync::Mutex<()>>,
}

/// Runs database requests. Its futures must be polled on a Tokio runtime,
/// since the MySQL and Postgres drivers are Tokio-based.
pub struct Engine {
    http_client: Arc<dyn HttpClient>,
    connections: parking_lot::Mutex<HashMap<String, Arc<Connected>>>,
}

impl Engine {
    pub fn new(http_client: Arc<dyn HttpClient>) -> Self {
        Self {
            http_client,
            connections: parking_lot::Mutex::new(HashMap::default()),
        }
    }

    pub async fn handle(&self, request: Request) -> Result<Response> {
        match request {
            Request::Connect {
                connection,
                spec,
                secret,
            } => self.connect(connection, spec, secret).await,
            Request::Disconnect { connection } => {
                self.connections.lock().remove(&connection);
                Ok(Response::Disconnected)
            }
            Request::Collections { connection } => {
                let connected = self.connected(&connection)?;
                let _guard = connected.lock.lock().await;
                Ok(Response::Collections(connected.source.collections().await?))
            }
            Request::Describe {
                connection,
                collection,
            } => {
                let connected = self.connected(&connection)?;
                let _guard = connected.lock.lock().await;
                let mut schema = connected.source.describe(&collection).await?;
                if !connected.capability.is_writable() {
                    schema.writable = Some(false);
                }
                Ok(Response::Schema(schema))
            }
            Request::Query {
                connection,
                text,
                limit,
                read_only,
            } => {
                let connected = self.connected(&connection)?;
                let _guard = connected.lock.lock().await;
                let read_only = read_only || !connected.capability.is_writable();
                Ok(Response::Rows(
                    connected.source.query(&text, limit, read_only).await?,
                ))
            }
            Request::Browse {
                connection,
                collection,
                request,
            } => {
                let connected = self.connected(&connection)?;
                let _guard = connected.lock.lock().await;
                Ok(Response::Page(
                    connected.source.browse(&collection, &request).await?,
                ))
            }
            Request::RenderChanges {
                connection,
                changes,
            } => {
                let connected = self.connected(&connection)?;
                Ok(Response::Text(connected.source.render_changes(&changes)?))
            }
            Request::Apply {
                connection,
                changes,
            } => {
                let connected = self.connected(&connection)?;
                if let Capability::ReadOnly { reason } = &connected.capability {
                    bail!("{connection} is read-only: {reason}");
                }
                let _guard = connected.lock.lock().await;
                Ok(Response::Applied(connected.source.apply(&changes).await?))
            }
            Request::History { connection, cursor } => {
                let connected = self.connected(&connection)?;
                let _guard = connected.lock.lock().await;
                Ok(Response::History(connected.source.history(cursor).await?))
            }
        }
    }

    fn connected(&self, connection: &str) -> Result<Arc<Connected>> {
        self.connections
            .lock()
            .get(connection)
            .cloned()
            .ok_or_else(|| anyhow!("{connection} is not connected"))
    }

    async fn connect(
        &self,
        connection: String,
        spec: ConnectionSpec,
        secret: Option<String>,
    ) -> Result<Response> {
        let source: Arc<dyn Source> = match spec.kind {
            DatabaseKind::Mysql | DatabaseKind::Postgres => {
                Arc::new(SqlSource::connect(&connection, &spec, secret).await?)
            }
            DatabaseKind::Clickhouse => Arc::new(ClickHouseSource::direct(
                self.http_client.clone(),
                url(&spec)?,
                spec.user.clone().unwrap_or_else(|| "default".to_string()),
                secret,
            )),
            DatabaseKind::ClickhouseRelay => Arc::new(ClickHouseSource::relay(
                self.http_client.clone(),
                url(&spec)?,
                secret.context("a ClickHouse relay needs its relay secret")?,
            )),
            DatabaseKind::Convex => Arc::new(ConvexSource::new(
                self.http_client.clone(),
                url(&spec)?,
                secret.context("a Convex connection needs an admin or deploy key")?,
            )),
        };
        let capability = if spec.read_only {
            // Still probe, so bad credentials fail on connect rather than on
            // first use.
            source.probe().await?;
            Capability::ReadOnly {
                reason: "pinned read-only in settings".to_string(),
            }
        } else {
            source.probe().await?
        };
        let language = source.query_language();
        self.connections.lock().insert(
            connection,
            Arc::new(Connected {
                source,
                capability: capability.clone(),
                lock: Arc::default(),
            }),
        );
        Ok(Response::Connected {
            capability,
            language,
        })
    }
}

fn url(spec: &ConnectionSpec) -> Result<String> {
    let url = spec
        .url
        .as_deref()
        .context("this connection needs a `url`")?
        .trim_end_matches('/');
    Ok(url.to_string())
}
