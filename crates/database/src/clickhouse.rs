use std::sync::{
    Arc,
    atomic::{AtomicU8, Ordering},
};

use anyhow::{Context as _, Result, bail};
use async_trait::async_trait;
use futures::AsyncReadExt as _;
use http_client::{AsyncBody, HttpClient, Method, Request as HttpRequest};
use serde::Deserialize;

use crate::{
    BrowseRequest, Capability, Change, ChangeSet, Collection, CollectionKind, CollectionRef,
    CollectionSchema, Column, Field, Filter, FilterOp, Page, QueryLanguage, ResultSet, Source,
    Value,
};

/// ClickHouse over its HTTP interface, either directly or through a query
/// relay that holds the real credentials (see Bartr's `warehouse` service).
pub(crate) struct ClickHouseSource {
    http_client: Arc<dyn HttpClient>,
    transport: Transport,
}

enum Transport {
    Direct {
        url: String,
        user: String,
        password: Option<String>,
        /// The user's `readonly` setting, learned on probe. At 1 the server
        /// rejects any setting sent with a query, so none are sent.
        readonly: AtomicU8,
    },
    Relay {
        url: String,
        secret: String,
    },
}

#[derive(Deserialize)]
struct CompactResponse {
    meta: Vec<Meta>,
    data: Vec<Vec<serde_json::Value>>,
}

#[derive(Deserialize)]
struct RelayResponse {
    #[serde(default)]
    meta: Vec<Meta>,
    #[serde(default)]
    rows: Vec<serde_json::Map<String, serde_json::Value>>,
}

#[derive(Deserialize)]
struct Meta {
    name: String,
    #[serde(rename = "type")]
    type_name: String,
}

#[derive(Deserialize)]
struct RelayError {
    error: String,
}

impl ClickHouseSource {
    pub fn direct(
        http_client: Arc<dyn HttpClient>,
        url: String,
        user: String,
        password: Option<String>,
    ) -> Self {
        Self {
            http_client,
            transport: Transport::Direct {
                url,
                user,
                password,
                readonly: AtomicU8::new(0),
            },
        }
    }

    pub fn relay(http_client: Arc<dyn HttpClient>, url: String, secret: String) -> Self {
        Self {
            http_client,
            transport: Transport::Relay { url, secret },
        }
    }

    fn is_relay(&self) -> bool {
        matches!(self.transport, Transport::Relay { .. })
    }

    async fn send(&self, request: HttpRequest<AsyncBody>) -> Result<(bool, String)> {
        let mut response = self.http_client.send(request).await?;
        let mut body = String::new();
        response.body_mut().read_to_string(&mut body).await?;
        Ok((response.status().is_success(), body))
    }

    /// Runs `sql`, sending `settings` only when the server will accept them.
    async fn run(&self, sql: &str, settings: &[(&str, String)]) -> Result<ResultSet> {
        match &self.transport {
            Transport::Direct {
                url,
                user,
                password,
                readonly,
            } => {
                let mut query = vec![("default_format", "JSONCompact".to_string())];
                if readonly.load(Ordering::Relaxed) != 1 {
                    query.push(("output_format_json_quote_64bit_integers", "0".to_string()));
                    query.extend(settings.iter().cloned());
                }
                let query = query
                    .iter()
                    .map(|(name, value)| format!("{name}={}", urlencoding::encode(value)))
                    .collect::<Vec<_>>()
                    .join("&");
                let mut request = HttpRequest::builder()
                    .method(Method::POST)
                    .uri(format!("{url}/?{query}"))
                    .header("X-ClickHouse-User", user.as_str());
                if let Some(password) = password {
                    request = request.header("X-ClickHouse-Key", password.as_str());
                }
                let (ok, body) = self
                    .send(request.body(AsyncBody::from(sql.to_string()))?)
                    .await?;
                if !ok {
                    bail!("{}", body.trim());
                }
                if body.trim().is_empty() {
                    return Ok(ResultSet::default());
                }
                let response: CompactResponse =
                    serde_json::from_str(&body).context("parsing ClickHouse response")?;
                Ok(ResultSet {
                    columns: response.meta.into_iter().map(Column::from).collect(),
                    rows: response
                        .data
                        .iter()
                        .map(|row| row.iter().map(decode_json).collect())
                        .collect(),
                    truncated: false,
                    affected_rows: None,
                })
            }
            Transport::Relay { url, secret } => {
                let body = serde_json::to_string(&serde_json::json!({ "sql": sql }))?;
                let request = HttpRequest::builder()
                    .method(Method::POST)
                    .uri(url.as_str())
                    .header("content-type", "application/json")
                    .header("x-relay-secret", secret.as_str())
                    .body(AsyncBody::from(body))?;
                let (ok, body) = self.send(request).await?;
                if !ok {
                    let message = serde_json::from_str::<RelayError>(&body)
                        .map(|error| error.error)
                        .unwrap_or(body);
                    bail!("{}", message.trim());
                }
                let response: RelayResponse =
                    serde_json::from_str(&body).context("parsing relay response")?;
                let columns = response.meta.into_iter().map(Column::from).collect::<Vec<_>>();
                let rows = response
                    .rows
                    .iter()
                    .map(|row| {
                        columns
                            .iter()
                            .map(|column| row.get(&column.name).map(decode_json).unwrap_or(Value::Missing))
                            .collect()
                    })
                    .collect();
                Ok(ResultSet {
                    columns,
                    rows,
                    truncated: false,
                    affected_rows: None,
                })
            }
        }
    }

    /// The relay's catalog endpoints live next to its query route
    /// (`/query` → `/catalog/tables`).
    async fn relay_catalog(&self, path: &str) -> Result<Option<serde_json::Value>> {
        let Transport::Relay { url, secret } = &self.transport else {
            return Ok(None);
        };
        let Some(index) = url.rfind("/query") else {
            return Ok(None);
        };
        let request = HttpRequest::builder()
            .method(Method::GET)
            .uri(format!("{}/catalog/{path}", &url[..index]))
            .header("x-relay-secret", secret.as_str())
            .body(AsyncBody::empty())?;
        let (ok, body) = self.send(request).await?;
        if !ok {
            return Ok(None);
        }
        Ok(serde_json::from_str(&body).ok())
    }

    fn statements(&self, changes: &ChangeSet) -> Result<Vec<(String, Option<String>)>> {
        let table = table_name(&changes.collection);
        changes
            .changes
            .iter()
            .map(|change| {
                Ok(match change {
                    Change::Update { key, set } => {
                        anyhow::ensure!(!key.is_empty() && !set.is_empty(), "an update needs a key and fields");
                        let condition = key_condition(key);
                        let assignments = set
                            .iter()
                            .map(|(field, value)| format!("{} = {}", quote(field), literal(value)))
                            .collect::<Vec<_>>()
                            .join(", ");
                        (
                            format!("ALTER TABLE {table} UPDATE {assignments} WHERE {condition}"),
                            Some(condition),
                        )
                    }
                    Change::Insert { values } => {
                        anyhow::ensure!(!values.is_empty(), "an insert needs fields");
                        let fields = values.iter().map(|(field, _)| quote(field)).collect::<Vec<_>>();
                        let literals = values.iter().map(|(_, value)| literal(value)).collect::<Vec<_>>();
                        (
                            format!(
                                "INSERT INTO {table} ({}) VALUES ({})",
                                fields.join(", "),
                                literals.join(", ")
                            ),
                            None,
                        )
                    }
                    Change::Delete { key } => {
                        anyhow::ensure!(!key.is_empty(), "a delete needs a key");
                        let condition = key_condition(key);
                        (format!("DELETE FROM {table} WHERE {condition}"), Some(condition))
                    }
                })
            })
            .collect()
    }
}

impl From<Meta> for Column {
    fn from(meta: Meta) -> Self {
        Column {
            name: meta.name,
            type_name: meta.type_name,
        }
    }
}

#[async_trait]
impl Source for ClickHouseSource {
    fn query_language(&self) -> QueryLanguage {
        QueryLanguage::ClickHouse
    }

    async fn probe(&self) -> Result<Capability> {
        match &self.transport {
            Transport::Relay { .. } => {
                self.run("SELECT 1", &[]).await?;
                Ok(Capability::ReadOnly {
                    reason: "queries go through a read-only relay".to_string(),
                })
            }
            Transport::Direct { readonly, .. } => {
                let result = self.run("SELECT getSetting('readonly')", &[]).await?;
                let level = result
                    .rows
                    .first()
                    .and_then(|row| row.first())
                    .map(|value| value.display().parse::<u8>().unwrap_or(0))
                    .unwrap_or(0);
                readonly.store(level, Ordering::Relaxed);
                Ok(if level == 0 {
                    Capability::Writable
                } else {
                    Capability::ReadOnly {
                        reason: format!("this user runs with readonly={level}"),
                    }
                })
            }
        }
    }

    async fn collections(&self) -> Result<Vec<Collection>> {
        if let Some(catalog) = self.relay_catalog("tables").await?
            && let Some(tables) = catalog.get("tables").and_then(|tables| tables.as_array())
        {
            return Ok(tables
                .iter()
                .filter_map(|table| {
                    Some(Collection {
                        reference: CollectionRef::new(
                            Some(table.get("database")?.as_str()?),
                            table.get("table")?.as_str()?,
                        ),
                        kind: collection_kind(table.get("engine").and_then(|engine| engine.as_str())),
                    })
                })
                .collect());
        }
        let result = self
            .run(
                "SELECT database, name, engine FROM system.tables \
                 WHERE database NOT IN ('system', 'INFORMATION_SCHEMA', 'information_schema') \
                 ORDER BY database, name",
                &[],
            )
            .await?;
        Ok(result
            .rows
            .iter()
            .filter_map(|row| {
                let [database, name, engine] = row.as_slice() else {
                    return None;
                };
                Some(Collection {
                    reference: CollectionRef::new(Some(&database.display()), &name.display()),
                    kind: collection_kind(Some(&engine.display())),
                })
            })
            .collect())
    }

    async fn describe(&self, collection: &CollectionRef) -> Result<CollectionSchema> {
        let database = collection.namespace.clone().unwrap_or_else(|| "default".to_string());
        if self.is_relay() {
            let path = format!(
                "describe-table?database={}&table={}",
                urlencoding::encode(&database),
                urlencoding::encode(&collection.name)
            );
            if let Some(description) = self.relay_catalog(&path).await?
                && let Some(columns) = description.get("columns").and_then(|columns| columns.as_array())
            {
                return Ok(CollectionSchema {
                    fields: columns
                        .iter()
                        .filter_map(|column| {
                            let type_name = column.get("type")?.as_str()?.to_string();
                            Some(Field {
                                name: column.get("name")?.as_str()?.to_string(),
                                nullable: type_name.starts_with("Nullable("),
                                type_name,
                            })
                        })
                        .collect(),
                    writable: Some(false),
                    ..CollectionSchema::default()
                });
            }
        }
        let sql = format!(
            "SELECT name, type, is_in_primary_key FROM system.columns \
             WHERE database = {} AND table = {} ORDER BY position",
            literal(&Value::Text(database)),
            literal(&Value::Text(collection.name.clone()))
        );
        let result = self.run(&sql, &[]).await?;
        let mut schema = CollectionSchema::default();
        for row in &result.rows {
            let [name, type_name, in_primary_key] = row.as_slice() else {
                continue;
            };
            let type_name = type_name.display();
            if in_primary_key.display() == "1" {
                schema.primary_key.push(name.display());
            }
            schema.fields.push(Field {
                name: name.display(),
                nullable: type_name.starts_with("Nullable("),
                type_name,
            });
        }
        if schema.fields.is_empty() {
            bail!("{} not found", collection.qualified_name());
        }
        if self.is_relay() {
            schema.writable = Some(false);
        }
        Ok(schema)
    }

    async fn query(&self, text: &str, limit: usize, read_only: bool) -> Result<ResultSet> {
        let text = text.trim().trim_end_matches(';').trim();
        anyhow::ensure!(!text.is_empty(), "the query is empty");
        let mut settings = vec![
            ("max_result_rows", (limit + 1).to_string()),
            ("result_overflow_mode", "break".to_string()),
        ];
        if read_only {
            // readonly=2 still lets this request carry the limit settings
            // above; readonly=1 would reject them.
            settings.push(("readonly", "2".to_string()));
        }
        let mut result = self.run(text, &settings).await?;
        if result.rows.len() > limit {
            result.rows.truncate(limit);
            result.truncated = true;
        }
        Ok(result)
    }

    async fn browse(&self, collection: &CollectionRef, request: &BrowseRequest) -> Result<Page> {
        let offset: usize = match &request.cursor {
            Some(cursor) => cursor.parse().context("invalid cursor")?,
            None => 0,
        };
        let mut sql = format!("SELECT * FROM {}", table_name(collection));
        if !request.filters.is_empty() {
            let conditions = request.filters.iter().map(filter_condition).collect::<Vec<_>>();
            sql.push_str(&format!(" WHERE {}", conditions.join(" AND ")));
        }
        sql.push_str(&format!(" LIMIT {} OFFSET {offset}", request.page_size + 1));
        let mut rows = self.run(&sql, &[]).await?;
        let has_more = rows.rows.len() > request.page_size;
        rows.rows.truncate(request.page_size);
        Ok(Page {
            next_cursor: has_more.then(|| (offset + request.page_size).to_string()),
            rows,
        })
    }

    fn render_changes(&self, changes: &ChangeSet) -> Result<String> {
        if self.is_relay() {
            bail!("a relay connection is read-only");
        }
        Ok(self
            .statements(changes)?
            .iter()
            .map(|(sql, _)| format!("{sql};"))
            .collect::<Vec<_>>()
            .join("\n"))
    }

    async fn apply(&self, changes: &ChangeSet) -> Result<u64> {
        if self.is_relay() {
            bail!("a relay connection is read-only");
        }
        let table = table_name(&changes.collection);
        let statements = self.statements(changes)?;
        // ClickHouse has no transactions and mutations report no row counts,
        // so check every key matches at most one row before changing anything.
        for (_, condition) in &statements {
            if let Some(condition) = condition {
                let count = self
                    .run(&format!("SELECT count() FROM {table} WHERE {condition}"), &[])
                    .await?;
                let matches = count
                    .rows
                    .first()
                    .and_then(|row| row.first())
                    .map(|value| value.display().parse::<u64>().unwrap_or(u64::MAX))
                    .unwrap_or(u64::MAX);
                if matches > 1 {
                    bail!("refusing to change {table}: `{condition}` matches {matches} rows, not one");
                }
            }
        }
        for (sql, _) in &statements {
            self.run(sql, &[("mutations_sync", "2".to_string())]).await?;
        }
        Ok(statements.len() as u64)
    }
}

fn collection_kind(engine: Option<&str>) -> CollectionKind {
    match engine {
        Some(engine) if engine.contains("View") => CollectionKind::View,
        _ => CollectionKind::Table,
    }
}

fn quote(identifier: &str) -> String {
    format!("`{}`", identifier.replace('\\', "\\\\").replace('`', "\\`"))
}

fn table_name(collection: &CollectionRef) -> String {
    match &collection.namespace {
        Some(namespace) => format!("{}.{}", quote(namespace), quote(&collection.name)),
        None => quote(&collection.name),
    }
}

fn literal(value: &Value) -> String {
    match value {
        Value::Missing | Value::Null => "NULL".to_string(),
        Value::Bool(value) => if *value { "true" } else { "false" }.to_string(),
        Value::Int(value) => value.to_string(),
        Value::Float(value) => value.to_string(),
        Value::Bytes(bytes) => format!(
            "unhex('{}')",
            bytes.iter().map(|byte| format!("{byte:02x}")).collect::<String>()
        ),
        Value::Text(_) | Value::Json(_) => {
            let text = match value {
                Value::Text(text) => text.clone(),
                _ => value.display(),
            };
            format!("'{}'", text.replace('\\', "\\\\").replace('\'', "\\'"))
        }
    }
}

fn key_condition(key: &[(String, Value)]) -> String {
    key.iter()
        .map(|(field, value)| {
            if value.is_nullish() {
                format!("{} IS NULL", quote(field))
            } else {
                format!("{} = {}", quote(field), literal(value))
            }
        })
        .collect::<Vec<_>>()
        .join(" AND ")
}

fn filter_condition(filter: &Filter) -> String {
    let field = quote(&filter.field);
    let value = literal(&filter.value);
    match filter.op {
        FilterOp::Eq => format!("{field} = {value}"),
        FilterOp::NotEq => format!("{field} != {value}"),
        FilterOp::Lt => format!("{field} < {value}"),
        FilterOp::Lte => format!("{field} <= {value}"),
        FilterOp::Gt => format!("{field} > {value}"),
        FilterOp::Gte => format!("{field} >= {value}"),
        FilterOp::Contains => format!(
            "positionCaseInsensitive(toString({field}), {}) > 0",
            literal(&Value::Text(filter.value.display()))
        ),
        FilterOp::IsNull => format!("{field} IS NULL"),
        FilterOp::IsNotNull => format!("{field} IS NOT NULL"),
    }
}

fn decode_json(value: &serde_json::Value) -> Value {
    match value {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(value) => Value::Bool(*value),
        serde_json::Value::Number(number) => number
            .as_i64()
            .map(Value::Int)
            .or_else(|| number.as_f64().map(Value::Float))
            .unwrap_or_else(|| Value::Text(number.to_string())),
        serde_json::Value::String(text) => Value::Text(text.clone()),
        other => Value::Json(other.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_literals_escape_quotes_and_backslashes() {
        assert_eq!(literal(&Value::Text("it's \\".into())), "'it\\'s \\\\'");
        assert_eq!(quote("we`ird"), "`we\\`ird`");
        assert_eq!(
            key_condition(&[("id".into(), Value::Int(1)), ("deleted_at".into(), Value::Null)]),
            "`id` = 1 AND `deleted_at` IS NULL"
        );
    }
}
