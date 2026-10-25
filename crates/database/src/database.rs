mod clickhouse;
mod compare;
mod convex;
mod engine;
mod sql;
mod store;

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

pub use compare::{CompareOptions, CompareOutcome, CompareRow, CompareSide, FieldDiff, compare};
pub use engine::{Engine, Request, Response};
pub use store::{
    ConnectionEntry, ConnectionState, DatabaseHost, DatabaseSettings, DatabaseStore,
    DatabaseStoreEvent, FetchSource, MissingSecret, QueryHistoryEntry,
};
pub use settings::{
    DatabaseAgentAccess, DatabaseEnvironment, DatabaseKind, DatabaseWriteApproval,
};

/// A single cell. `Missing` is distinct from `Null`: document stores omit
/// fields, and comparing a missing field with a NULL column must not look
/// like a real difference.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Value {
    Missing,
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
    Bytes(Vec<u8>),
    Json(serde_json::Value),
}

impl Value {
    pub fn is_nullish(&self) -> bool {
        matches!(self, Value::Missing | Value::Null)
    }

    pub fn display(&self) -> String {
        match self {
            Value::Missing => String::new(),
            Value::Null => "NULL".to_string(),
            Value::Bool(value) => value.to_string(),
            Value::Int(value) => value.to_string(),
            Value::Float(value) => format_float(*value),
            Value::Text(value) => value.clone(),
            Value::Bytes(bytes) => format!("<{} bytes>", bytes.len()),
            Value::Json(value) => value.to_string(),
        }
    }

    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Value::Missing | Value::Null => serde_json::Value::Null,
            Value::Bool(value) => serde_json::Value::Bool(*value),
            Value::Int(value) => serde_json::Value::from(*value),
            Value::Float(value) => serde_json::Number::from_f64(*value)
                .map(serde_json::Value::Number)
                .unwrap_or(serde_json::Value::Null),
            Value::Text(value) => serde_json::Value::String(value.clone()),
            Value::Bytes(bytes) => {
                use base64::Engine as _;
                serde_json::Value::String(base64::engine::general_purpose::STANDARD.encode(bytes))
            }
            Value::Json(value) => value.clone(),
        }
    }

    /// Parses user input typed into a grid cell, guided by the column's
    /// current value so a number column stays a number.
    pub fn parse_like(input: &str, like: &Value) -> Value {
        if input.eq_ignore_ascii_case("null") {
            return Value::Null;
        }
        match like {
            Value::Int(_) => input
                .trim()
                .parse()
                .map(Value::Int)
                .unwrap_or_else(|_| Value::Text(input.to_string())),
            Value::Float(_) => input
                .trim()
                .parse()
                .map(Value::Float)
                .unwrap_or_else(|_| Value::Text(input.to_string())),
            Value::Bool(_) => match input.trim() {
                "true" | "1" => Value::Bool(true),
                "false" | "0" => Value::Bool(false),
                _ => Value::Text(input.to_string()),
            },
            Value::Json(_) => serde_json::from_str(input)
                .map(Value::Json)
                .unwrap_or_else(|_| Value::Text(input.to_string())),
            _ => Value::Text(input.to_string()),
        }
    }
}

fn format_float(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{value:.0}")
    } else {
        value.to_string()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Column {
    pub name: String,
    pub type_name: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ResultSet {
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<Value>>,
    /// More rows existed than were returned.
    pub truncated: bool,
    pub affected_rows: Option<u64>,
}

impl ResultSet {
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|column| column.name == name)
    }

    /// Builds a result set from documents with varying fields, using the
    /// union of their keys as columns and `Missing` for absent fields.
    pub fn from_documents(documents: Vec<serde_json::Map<String, serde_json::Value>>) -> Self {
        let mut names: Vec<String> = Vec::new();
        for document in &documents {
            for key in document.keys() {
                if !names.contains(key) {
                    names.push(key.clone());
                }
            }
        }
        sort_document_fields(&mut names);
        let rows = documents
            .iter()
            .map(|document| {
                names
                    .iter()
                    .map(|name| {
                        document
                            .get(name)
                            .map(convex::decode_value)
                            .unwrap_or(Value::Missing)
                    })
                    .collect()
            })
            .collect();
        ResultSet {
            columns: names
                .into_iter()
                .map(|name| Column {
                    name,
                    type_name: String::new(),
                })
                .collect(),
            rows,
            truncated: false,
            affected_rows: None,
        }
    }
}

/// `_id` first, `_creationTime` last, everything else alphabetical: the same
/// order the Convex dashboard uses, so rows read the same in both places.
fn sort_document_fields(names: &mut [String]) {
    names.sort_by(|left, right| {
        let rank = |name: &str| match name {
            "_id" => 0,
            "_creationTime" => 2,
            _ => 1,
        };
        rank(left).cmp(&rank(right)).then_with(|| left.cmp(right))
    });
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct CollectionRef {
    /// Schema (Postgres), database (MySQL, ClickHouse); `None` for Convex.
    pub namespace: Option<String>,
    pub name: String,
}

impl CollectionRef {
    pub fn new(namespace: Option<&str>, name: &str) -> Self {
        Self {
            namespace: namespace.map(str::to_string),
            name: name.to_string(),
        }
    }

    pub fn qualified_name(&self) -> String {
        match &self.namespace {
            Some(namespace) => format!("{namespace}.{}", self.name),
            None => self.name.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CollectionKind {
    Table,
    View,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Collection {
    pub reference: CollectionRef,
    pub kind: CollectionKind,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Field {
    pub name: String,
    pub type_name: String,
    pub nullable: bool,
}

/// `fields` of one collection point at `target_fields` of `target`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Reference {
    pub source: CollectionRef,
    pub fields: Vec<String>,
    pub target: CollectionRef,
    pub target_fields: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CollectionSchema {
    pub fields: Vec<Field>,
    pub primary_key: Vec<String>,
    /// References from this collection to others.
    pub references: Vec<Reference>,
    /// References from other collections to this one.
    pub referenced_by: Vec<Reference>,
    /// Whether this collection accepts writes, when the engine can tell
    /// cheaply. `None` defers to the connection's capability.
    pub writable: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Capability {
    Writable,
    ReadOnly { reason: String },
}

impl Capability {
    pub fn is_writable(&self) -> bool {
        matches!(self, Capability::Writable)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FilterOp {
    Eq,
    NotEq,
    Lt,
    Lte,
    Gt,
    Gte,
    Contains,
    IsNull,
    IsNotNull,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Filter {
    pub field: String,
    pub op: FilterOp,
    pub value: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BrowseRequest {
    pub filters: Vec<Filter>,
    pub cursor: Option<String>,
    pub page_size: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Page {
    pub rows: ResultSet,
    pub next_cursor: Option<String>,
}

pub type Assignments = Vec<(String, Value)>;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Change {
    Update { key: Assignments, set: Assignments },
    Insert { values: Assignments },
    Delete { key: Assignments },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChangeSet {
    pub collection: CollectionRef,
    pub changes: Vec<Change>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub table: String,
    pub id: String,
    /// Nanoseconds since the epoch.
    pub timestamp: i64,
    pub deleted: bool,
    pub document: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HistoryBatch {
    pub entries: Vec<HistoryEntry>,
    /// Opaque; pass back to continue from here.
    pub cursor: String,
    pub has_more: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum QueryLanguage {
    MySql,
    Postgres,
    ClickHouse,
    /// No free-form queries; collections are browsed with filters.
    None,
}

/// Everything needed to open a connection except the secret.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ConnectionSpec {
    pub kind: DatabaseKind,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub database: Option<String>,
    pub user: Option<String>,
    pub url: Option<String>,
    pub ssl_mode: Option<String>,
    pub read_only: bool,
}

impl ConnectionSpec {
    /// Identifies the endpoint and account, so two projects defining the same
    /// database under different names share one keychain entry.
    pub fn credential_key(&self) -> String {
        match self.kind {
            DatabaseKind::Mysql | DatabaseKind::Postgres => format!(
                "zed-database://{}/{}@{}:{}/{}",
                kind_name(self.kind),
                self.user.as_deref().unwrap_or_default(),
                self.host.as_deref().unwrap_or("localhost"),
                self.port.unwrap_or_else(|| default_port(self.kind)),
                self.database.as_deref().unwrap_or_default(),
            ),
            DatabaseKind::Clickhouse => format!(
                "zed-database://clickhouse/{}@{}",
                self.user.as_deref().unwrap_or("default"),
                self.url.as_deref().unwrap_or_default(),
            ),
            DatabaseKind::ClickhouseRelay | DatabaseKind::Convex => format!(
                "zed-database://{}/{}",
                kind_name(self.kind),
                self.url.as_deref().unwrap_or_default(),
            ),
        }
    }

    /// The label shown in the keychain prompt for this connection's secret.
    pub fn secret_label(&self) -> &'static str {
        match self.kind {
            DatabaseKind::Mysql | DatabaseKind::Postgres | DatabaseKind::Clickhouse => "Password",
            DatabaseKind::ClickhouseRelay => "Relay secret",
            DatabaseKind::Convex => "Admin or deploy key",
        }
    }
}

pub fn kind_name(kind: DatabaseKind) -> &'static str {
    match kind {
        DatabaseKind::Mysql => "mysql",
        DatabaseKind::Postgres => "postgres",
        DatabaseKind::Clickhouse => "clickhouse",
        DatabaseKind::ClickhouseRelay => "clickhouse_relay",
        DatabaseKind::Convex => "convex",
    }
}

pub fn default_port(kind: DatabaseKind) -> u16 {
    match kind {
        DatabaseKind::Mysql => 3306,
        DatabaseKind::Postgres => 5432,
        DatabaseKind::Clickhouse | DatabaseKind::ClickhouseRelay | DatabaseKind::Convex => 0,
    }
}

#[async_trait]
pub(crate) trait Source: Send + Sync {
    fn query_language(&self) -> QueryLanguage;
    /// Determines what the credentials may do. Called once on connect.
    async fn probe(&self) -> Result<Capability>;
    async fn collections(&self) -> Result<Vec<Collection>>;
    async fn describe(&self, collection: &CollectionRef) -> Result<CollectionSchema>;
    /// Runs a free-form query, returning at most `limit` rows. With
    /// `read_only`, the engine itself must refuse writes, not just the
    /// credentials.
    async fn query(&self, text: &str, limit: usize, read_only: bool) -> Result<ResultSet>;
    async fn browse(&self, collection: &CollectionRef, request: &BrowseRequest) -> Result<Page>;
    /// The exact statements (or patch) `apply` would run, for approval.
    fn render_changes(&self, changes: &ChangeSet) -> Result<String>;
    async fn apply(&self, changes: &ChangeSet) -> Result<u64>;
    async fn history(&self, _cursor: Option<String>) -> Result<HistoryBatch> {
        anyhow::bail!("this database does not expose a change history")
    }
}
