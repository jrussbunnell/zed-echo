use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow, bail};
use async_trait::async_trait;
use base64::Engine as _;
use collections::HashMap;
use futures::AsyncReadExt as _;
use http_client::{AsyncBody, HttpClient, Method, Request as HttpRequest};
use serde_json::{Map, Value as Json, json};

use crate::{
    BrowseRequest, Capability, Change, ChangeSet, Collection, CollectionKind, CollectionRef,
    CollectionSchema, Field, Filter, FilterOp, HistoryBatch, HistoryEntry, Page, QueryLanguage,
    Reference, ResultSet, Source, Value,
};

/// The placeholder Convex's dashboard functions read as "remove this field".
const UNDEFINED_PLACEHOLDER: &str = "__CONVEX_PLACEHOLDER_undefined_I23atX0jcndVbFgXoQZffsih7eAqktCyFjgUuAeNBtfr3ySOljPSPSEOPFgprkdBO3zXNiGEJxmJ5ZFPc5C5qKesG80QRPvlJe8vgSxAt9feLTwxTg4PHfVwUaTEJU67FDwldWmTxp1guMPwxQ2jOuhEryTBf3mQ";

const ID_ALPHABET: &[u8] = b"0123456789abcdefghjkmnpqrstvwxyz";

/// A Convex deployment (cloud or self-hosted), driven through the same system
/// functions the Convex dashboard calls with an admin key. These work on
/// every plan, unlike the streaming export API, which only `history` needs.
pub(crate) struct ConvexSource {
    http_client: Arc<dyn HttpClient>,
    url: String,
    key: String,
}

impl ConvexSource {
    pub fn new(http_client: Arc<dyn HttpClient>, url: String, key: String) -> Self {
        Self {
            http_client,
            url,
            key,
        }
    }

    async fn call(&self, kind: &str, path: &str, args: Json) -> Result<Json> {
        let body = serde_json::to_string(&json!({ "path": path, "args": args, "format": "json" }))?;
        let request = HttpRequest::builder()
            .method(Method::POST)
            .uri(format!("{}/api/{kind}", self.url))
            .header("content-type", "application/json")
            .header("authorization", format!("Convex {}", self.key))
            .body(AsyncBody::from(body))?;
        let mut response = self.http_client.send(request).await?;
        let mut text = String::new();
        response.body_mut().read_to_string(&mut text).await?;
        let parsed: Json = serde_json::from_str(&text)
            .map_err(|_| anyhow!("Convex returned {}: {}", response.status(), text.trim()))?;
        if parsed.get("status").and_then(Json::as_str) == Some("success") {
            let value = parsed.get("value").cloned().unwrap_or(Json::Null);
            // Dashboard mutations report validation failures in-band.
            if value.get("success") == Some(&Json::Bool(false)) {
                bail!(
                    "{}",
                    value.get("error").and_then(Json::as_str).unwrap_or("Convex rejected the change")
                );
            }
            Ok(value)
        } else {
            bail!(
                "{}",
                parsed
                    .get("errorMessage")
                    .or_else(|| parsed.get("message"))
                    .and_then(Json::as_str)
                    .unwrap_or(text.trim())
            )
        }
    }

    async fn get(&self, path_and_query: &str) -> Result<Json> {
        let request = HttpRequest::builder()
            .method(Method::GET)
            .uri(format!("{}{path_and_query}", self.url))
            .header("authorization", format!("Convex {}", self.key))
            .body(AsyncBody::empty())?;
        let mut response = self.http_client.send(request).await?;
        let mut text = String::new();
        response.body_mut().read_to_string(&mut text).await?;
        let parsed: Json = serde_json::from_str(&text)
            .map_err(|_| anyhow!("Convex returned {}: {}", response.status(), text.trim()))?;
        if !response.status().is_success() {
            bail!(
                "{}",
                parsed.get("message").and_then(Json::as_str).unwrap_or(text.trim())
            );
        }
        Ok(parsed)
    }

    async fn table_names(&self) -> Result<Vec<String>> {
        let page = self
            .call(
                "query",
                "_system/cli/tables",
                json!({ "paginationOpts": { "numItems": 1000, "cursor": null } }),
            )
            .await?;
        Ok(page
            .get("page")
            .and_then(Json::as_array)
            .map(|tables| {
                tables
                    .iter()
                    .filter_map(|table| table.get("name")?.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn table_numbers(&self) -> Result<HashMap<u64, String>> {
        let mapping = self.call("query", "_system/frontend/getTableMapping", json!({})).await?;
        Ok(mapping
            .as_object()
            .map(|mapping| {
                mapping
                    .iter()
                    .filter_map(|(number, name)| Some((number.parse().ok()?, name.as_str()?.to_string())))
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Id fields declared in the active schema, as `(table, field, target)`.
    async fn schema_references(&self) -> Result<Vec<(String, String, String)>> {
        let schemas = self.call("query", "_system/frontend/getSchemas", json!({})).await?;
        let Some(active) = schemas.get("active").and_then(Json::as_str) else {
            return Ok(Vec::new());
        };
        let schema: Json = serde_json::from_str(active).context("parsing the active Convex schema")?;
        let mut references = Vec::new();
        for table in schema.get("tables").and_then(Json::as_array).into_iter().flatten() {
            let Some(table_name) = table.get("tableName").and_then(Json::as_str) else {
                continue;
            };
            let fields = table
                .get("documentType")
                .and_then(|document| document.get("value"))
                .and_then(Json::as_object);
            for (field, definition) in fields.into_iter().flatten() {
                if let Some(target) = id_target(definition.get("fieldType").unwrap_or(definition)) {
                    references.push((table_name.to_string(), field.clone(), target));
                }
            }
        }
        Ok(references)
    }

    async fn page(&self, table: &str, filters: &[Filter], cursor: Option<&str>, page_size: usize) -> Result<(Vec<Map<String, Json>>, Option<String>)> {
        let clauses = filters.iter().map(filter_clause).collect::<Result<Vec<_>>>()?;
        let filters = if clauses.is_empty() {
            Json::Null
        } else {
            let expression = serde_json::to_string(&json!({ "clauses": clauses, "order": "asc" }))?;
            Json::String(base64::engine::general_purpose::STANDARD.encode(expression))
        };
        let result = self
            .call(
                "query",
                "_system/frontend/paginatedTableDocuments",
                json!({
                    "table": table,
                    "filters": filters,
                    "paginationOpts": { "numItems": page_size, "cursor": cursor },
                }),
            )
            .await?;
        let documents = result
            .get("page")
            .and_then(Json::as_array)
            .map(|page| page.iter().filter_map(|document| document.as_object().cloned()).collect::<Vec<_>>())
            .unwrap_or_default();
        // Invalid filters come back as rows describing the error.
        if let Some(error) = documents.iter().find_map(|document| document.get("error").and_then(Json::as_str)) {
            bail!("{error}");
        }
        let done = result.get("isDone").and_then(Json::as_bool).unwrap_or(true);
        let next = result
            .get("continueCursor")
            .and_then(Json::as_str)
            .filter(|_| !done)
            .map(str::to_string);
        Ok((documents, next))
    }

    fn calls(&self, changes: &ChangeSet) -> Result<Vec<(&'static str, Json)>> {
        let table = &changes.collection.name;
        changes
            .changes
            .iter()
            .map(|change| {
                Ok(match change {
                    Change::Update { key, set } => {
                        let id = document_id(key)?;
                        let fields = set
                            .iter()
                            .map(|(field, value)| {
                                let value = match value {
                                    Value::Missing => Json::String(UNDEFINED_PLACEHOLDER.to_string()),
                                    value => encode_value(value),
                                };
                                (field.clone(), value)
                            })
                            .collect::<Map<_, _>>();
                        (
                            "_system/frontend/patchDocumentsFields",
                            json!({ "table": table, "fields": fields, "ids": [id] }),
                        )
                    }
                    Change::Insert { values } => {
                        let document = values
                            .iter()
                            .filter(|(_, value)| !matches!(value, Value::Missing))
                            .map(|(field, value)| (field.clone(), encode_value(value)))
                            .collect::<Map<_, _>>();
                        (
                            "_system/frontend/addDocument",
                            json!({ "table": table, "documents": [document] }),
                        )
                    }
                    Change::Delete { key } => (
                        "_system/frontend/deleteDocuments",
                        json!({ "toDelete": [{ "id": document_id(key)?, "tableName": table }] }),
                    ),
                })
            })
            .collect()
    }
}

#[async_trait]
impl Source for ConvexSource {
    fn query_language(&self) -> QueryLanguage {
        QueryLanguage::None
    }

    async fn probe(&self) -> Result<Capability> {
        self.table_names().await?;
        // An admin or deploy key can always write; Convex has no finer grain.
        Ok(Capability::Writable)
    }

    async fn collections(&self) -> Result<Vec<Collection>> {
        Ok(self
            .table_names()
            .await?
            .into_iter()
            .map(|name| Collection {
                reference: CollectionRef { namespace: None, name },
                kind: CollectionKind::Table,
            })
            .collect())
    }

    async fn describe(&self, collection: &CollectionRef) -> Result<CollectionSchema> {
        let table = &collection.name;
        let (sample, _) = self.page(table, &[], None, 100).await?;
        let mut fields: Vec<Field> = Vec::new();
        for document in &sample {
            for (name, value) in document {
                if !fields.iter().any(|field| &field.name == name) {
                    fields.push(Field {
                        name: name.clone(),
                        type_name: json_type_name(value).to_string(),
                        nullable: true,
                    });
                }
            }
        }
        let mut names = fields.iter().map(|field| field.name.clone()).collect::<Vec<_>>();
        crate::sort_document_fields(&mut names);
        fields.sort_by_key(|field| names.iter().position(|name| name == &field.name));

        let declared = self.schema_references().await.unwrap_or_default();
        let mut references = Vec::new();
        let mut referenced_by = Vec::new();
        for (source_table, field, target) in &declared {
            let reference = Reference {
                source: CollectionRef::new(None, source_table),
                fields: vec![field.clone()],
                target: CollectionRef::new(None, target),
                target_fields: vec!["_id".to_string()],
            };
            if source_table == table {
                references.push(reference.clone());
            }
            if target == table {
                referenced_by.push(reference);
            }
        }
        // Without a schema, infer outgoing references from sampled values:
        // a Convex id encodes the number of the table it belongs to.
        if declared.is_empty() {
            let numbers = self.table_numbers().await?;
            for field in &fields {
                if field.name == "_id" {
                    continue;
                }
                let target = sample.iter().find_map(|document| {
                    let id = document.get(&field.name)?.as_str()?;
                    numbers.get(&id_table_number(id)?).cloned()
                });
                if let Some(target) = target {
                    references.push(Reference {
                        source: collection.clone(),
                        fields: vec![field.name.clone()],
                        target: CollectionRef::new(None, &target),
                        target_fields: vec!["_id".to_string()],
                    });
                }
            }
        }
        Ok(CollectionSchema {
            fields,
            primary_key: vec!["_id".to_string()],
            references,
            referenced_by,
            writable: None,
        })
    }

    async fn query(&self, _text: &str, _limit: usize, _read_only: bool) -> Result<ResultSet> {
        bail!("Convex has no query language here; browse a table instead")
    }

    async fn browse(&self, collection: &CollectionRef, request: &BrowseRequest) -> Result<Page> {
        let (documents, next_cursor) = self
            .page(&collection.name, &request.filters, request.cursor.as_deref(), request.page_size)
            .await?;
        Ok(Page {
            rows: ResultSet::from_documents(documents),
            next_cursor,
        })
    }

    fn render_changes(&self, changes: &ChangeSet) -> Result<String> {
        Ok(self
            .calls(changes)?
            .iter()
            .map(|(path, args)| {
                let mut args = args.clone();
                // The placeholder is an implementation detail; show intent.
                redact_placeholder(&mut args);
                format!("{path} {args}")
            })
            .collect::<Vec<_>>()
            .join("\n"))
    }

    async fn apply(&self, changes: &ChangeSet) -> Result<u64> {
        let calls = self.calls(changes)?;
        for (path, args) in &calls {
            self.call("mutation", path, args.clone()).await?;
        }
        Ok(calls.len() as u64)
    }

    async fn history(&self, cursor: Option<String>) -> Result<HistoryBatch> {
        let cursor = cursor.unwrap_or_else(|| "snapshot".to_string());
        if let Some(delta_cursor) = cursor.strip_prefix("delta:") {
            let response = self
                .get(&format!("/api/document_deltas?cursor={delta_cursor}&format=json"))
                .await?;
            let next = response
                .get("cursor")
                .and_then(Json::as_i64)
                .context("document_deltas returned no cursor")?;
            return Ok(HistoryBatch {
                entries: history_entries(&response),
                cursor: format!("delta:{next}"),
                has_more: response.get("hasMore").and_then(Json::as_bool).unwrap_or(false),
            });
        }
        // "snapshot" or "snapshot:<ts>:<page cursor>": walk a consistent
        // snapshot first, then follow deltas from its timestamp.
        let mut query = "/api/list_snapshot?format=json".to_string();
        let mut parts = cursor.splitn(3, ':').skip(1);
        if let (Some(snapshot), Some(page)) = (parts.next(), parts.next()) {
            query.push_str(&format!("&snapshot={snapshot}&cursor={}", urlencoding::encode(page)));
        }
        let response = self.get(&query).await?;
        let snapshot = response
            .get("snapshot")
            .and_then(Json::as_i64)
            .context("list_snapshot returned no snapshot")?;
        let has_more = response.get("hasMore").and_then(Json::as_bool).unwrap_or(false);
        let next = match response.get("cursor").and_then(Json::as_str) {
            Some(page) if has_more => format!("snapshot:{snapshot}:{page}"),
            _ => format!("delta:{snapshot}"),
        };
        Ok(HistoryBatch {
            entries: history_entries(&response),
            cursor: next,
            has_more: true,
        })
    }
}

fn history_entries(response: &Json) -> Vec<HistoryEntry> {
    response
        .get("values")
        .and_then(Json::as_array)
        .into_iter()
        .flatten()
        .filter_map(|value| {
            let mut document = value.as_object()?.clone();
            let table = document.remove("_table")?.as_str()?.to_string();
            let timestamp = document.remove("_ts")?.as_i64()?;
            let deleted = document
                .remove("_deleted")
                .and_then(|deleted| deleted.as_bool())
                .unwrap_or(false);
            document.remove("_component");
            let id = document.get("_id")?.as_str()?.to_string();
            Some(HistoryEntry {
                table,
                id,
                timestamp,
                deleted,
                document: Json::Object(document),
            })
        })
        .collect()
}

fn redact_placeholder(value: &mut Json) {
    match value {
        Json::String(text) if text == UNDEFINED_PLACEHOLDER => *text = "<remove field>".to_string(),
        Json::Array(values) => values.iter_mut().for_each(redact_placeholder),
        Json::Object(map) => map.values_mut().for_each(redact_placeholder),
        _ => {}
    }
}

fn document_id(key: &[(String, Value)]) -> Result<String> {
    match key {
        [(field, Value::Text(id))] if field == "_id" => Ok(id.clone()),
        _ => bail!("Convex documents are changed by `_id`"),
    }
}

fn filter_clause(filter: &Filter) -> Result<Json> {
    let (op, value) = match filter.op {
        FilterOp::Eq => ("eq", encode_value(&filter.value)),
        FilterOp::NotEq => ("neq", encode_value(&filter.value)),
        FilterOp::Lt => ("lt", encode_value(&filter.value)),
        FilterOp::Lte => ("lte", encode_value(&filter.value)),
        FilterOp::Gt => ("gt", encode_value(&filter.value)),
        FilterOp::Gte => ("gte", encode_value(&filter.value)),
        FilterOp::IsNull => ("eq", Json::Null),
        FilterOp::IsNotNull => ("neq", Json::Null),
        FilterOp::Contains => bail!("Convex filters support comparisons, not substring search"),
    };
    Ok(json!({ "field": filter.field, "op": op, "value": value, "enabled": true }))
}

fn id_target(field_type: &Json) -> Option<String> {
    match field_type.get("type")?.as_str()? {
        "id" => field_type.get("tableName")?.as_str().map(str::to_string),
        // `v.optional(v.id(...))` and `v.union(v.id(...), v.null())`.
        "union" => field_type.get("value")?.as_array()?.iter().find_map(id_target),
        _ => None,
    }
}

fn json_type_name(value: &Json) -> &'static str {
    match value {
        Json::Null => "null",
        Json::Bool(_) => "boolean",
        Json::Number(_) => "number",
        Json::String(_) => "string",
        Json::Array(_) => "array",
        Json::Object(object) if object.contains_key("$integer") => "bigint",
        Json::Object(object) if object.contains_key("$bytes") => "bytes",
        Json::Object(_) => "object",
    }
}

/// The table number encoded at the front of a Convex document id.
pub(crate) fn id_table_number(id: &str) -> Option<u64> {
    if !(31..=37).contains(&id.len()) {
        return None;
    }
    let mut bits: u64 = 0;
    let mut bit_count = 0;
    let mut bytes = Vec::with_capacity(id.len() * 5 / 8);
    for character in id.bytes() {
        let digit = ID_ALPHABET.iter().position(|candidate| *candidate == character)? as u64;
        bits = (bits << 5) | digit;
        bit_count += 5;
        if bit_count >= 8 {
            bit_count -= 8;
            bytes.push((bits >> bit_count) as u8);
            bits &= (1 << bit_count) - 1;
        }
    }
    let mut number: u64 = 0;
    for (index, byte) in bytes.iter().enumerate().take(5) {
        number |= u64::from(byte & 0x7f) << (7 * index);
        if byte & 0x80 == 0 {
            // Table number, then a 16 byte internal id, then a 2 byte footer.
            return (bytes.len() == index + 1 + 16 + 2).then_some(number);
        }
    }
    None
}

/// Decodes Convex's JSON export format, where 64-bit integers and bytes are
/// tagged objects and every plain number is a float64.
pub(crate) fn decode_value(value: &Json) -> Value {
    match value {
        Json::Null => Value::Null,
        Json::Bool(value) => Value::Bool(*value),
        Json::Number(number) => Value::Float(number.as_f64().unwrap_or(f64::NAN)),
        Json::String(text) => Value::Text(text.clone()),
        Json::Object(object) if object.len() == 1 => {
            if let Some(integer) = object.get("$integer").and_then(Json::as_str) {
                let bytes = base64::engine::general_purpose::STANDARD.decode(integer).ok();
                if let Some(bytes) = bytes.and_then(|bytes| <[u8; 8]>::try_from(bytes).ok()) {
                    return Value::Int(i64::from_le_bytes(bytes));
                }
            }
            if let Some(bytes) = object.get("$bytes").and_then(Json::as_str)
                && let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(bytes)
            {
                return Value::Bytes(bytes);
            }
            Value::Json(value.clone())
        }
        other => Value::Json(other.clone()),
    }
}

fn encode_value(value: &Value) -> Json {
    match value {
        Value::Missing | Value::Null => Json::Null,
        Value::Bool(value) => Json::Bool(*value),
        Value::Int(value) => json!({
            "$integer": base64::engine::general_purpose::STANDARD.encode(value.to_le_bytes())
        }),
        Value::Float(value) => serde_json::Number::from_f64(*value)
            .map(Json::Number)
            .unwrap_or(Json::Null),
        Value::Text(text) => Json::String(text.clone()),
        Value::Bytes(bytes) => json!({ "$bytes": base64::engine::general_purpose::STANDARD.encode(bytes) }),
        Value::Json(value) => value.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_id_table_number() {
        assert_eq!(id_table_number("j571ppjzadhqaz99pz7mv7r3tx8gyc29"), Some(10001));
        assert_eq!(id_table_number("not-an-id"), None);
        assert_eq!(id_table_number("ilou0000000000000000000000000000"), None);
    }

    #[test]
    fn test_value_round_trip() {
        for value in [Value::Int(-42), Value::Bytes(vec![1, 2, 3]), Value::Float(1.5), Value::Text("x".into())] {
            assert_eq!(decode_value(&encode_value(&value)), value);
        }
    }

    #[test]
    fn test_id_target_unwraps_optional_unions() {
        let field = json!({ "type": "union", "value": [{ "type": "null" }, { "type": "id", "tableName": "users" }] });
        assert_eq!(id_target(&field), Some("users".to_string()));
    }
}
