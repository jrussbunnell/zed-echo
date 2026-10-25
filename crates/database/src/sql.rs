use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use async_trait::async_trait;
use zqlz_core::{Connection, ConnectionConfig, DatabaseDriver, SchemaIntrospection};

use crate::{
    BrowseRequest, Capability, Change, ChangeSet, Collection, CollectionKind, CollectionRef,
    CollectionSchema, Column, ConnectionSpec, DatabaseKind, Field, Filter, FilterOp, Page,
    QueryLanguage, Reference, ResultSet, Source, Value, default_port, kind_name,
};

pub(crate) struct SqlSource {
    kind: DatabaseKind,
    connection: Arc<dyn Connection>,
    database: Option<String>,
}

impl SqlSource {
    pub async fn connect(name: &str, spec: &ConnectionSpec, secret: Option<String>) -> Result<Self> {
        let mut config = ConnectionConfig::new(kind_name(spec.kind), name);
        config.host = spec.host.clone().unwrap_or_else(|| "localhost".to_string());
        config.port = spec.port.unwrap_or_else(|| default_port(spec.kind));
        config.database = spec.database.clone();
        config.username = spec.user.clone();
        config.password = secret;
        if let Some(ssl_mode) = &spec.ssl_mode {
            config
                .params
                .insert("ssl_mode".to_string(), driver_ssl_mode(spec.kind, ssl_mode));
        }
        let connection = match spec.kind {
            DatabaseKind::Mysql => zqlz_driver_mysql::MySqlDriver::new().connect(&config).await,
            DatabaseKind::Postgres => {
                zqlz_driver_postgres::PostgresDriver::new()
                    .connect(&config)
                    .await
            }
            _ => bail!("{} is not a SQL driver", kind_name(spec.kind)),
        }
        .with_context(|| format!("connecting to {name}"))?;
        Ok(Self {
            kind: spec.kind,
            connection,
            database: spec.database.clone(),
        })
    }

    fn introspection(&self) -> Result<&dyn SchemaIntrospection> {
        self.connection
            .as_schema_introspection()
            .context("this driver does not support schema introspection")
    }

    async fn rows(&self, sql: &str, params: &[Value]) -> Result<ResultSet> {
        let params = params.iter().map(to_driver_value).collect::<Vec<_>>();
        let result = self.connection.execute(sql, &params).await?;
        Ok(match result.result {
            Some(result) => ResultSet {
                columns: result
                    .columns
                    .iter()
                    .map(|column| Column {
                        name: column.name.clone(),
                        type_name: column.data_type.clone(),
                    })
                    .collect(),
                rows: result
                    .rows
                    .into_iter()
                    .map(|row| row.values.into_iter().map(from_driver_value).collect())
                    .collect(),
                truncated: false,
                affected_rows: None,
            },
            None => ResultSet {
                affected_rows: Some(result.affected_rows),
                ..ResultSet::default()
            },
        })
    }

    async fn statement(&self, sql: &str) -> Result<()> {
        self.connection.execute(sql, &[]).await?;
        Ok(())
    }

    fn quote(&self, identifier: &str) -> String {
        quote_identifier(self.kind, identifier)
    }

    fn table(&self, collection: &CollectionRef) -> String {
        match &collection.namespace {
            Some(namespace) => format!("{}.{}", self.quote(namespace), self.quote(&collection.name)),
            None => self.quote(&collection.name),
        }
    }

    fn placeholder(&self, index: usize) -> String {
        match self.kind {
            DatabaseKind::Postgres => format!("${index}"),
            _ => "?".to_string(),
        }
    }

    fn begin_read_only(&self) -> &'static str {
        match self.kind {
            DatabaseKind::Postgres => "BEGIN READ ONLY",
            _ => "START TRANSACTION READ ONLY",
        }
    }

    fn begin(&self) -> &'static str {
        match self.kind {
            DatabaseKind::Postgres => "BEGIN",
            _ => "START TRANSACTION",
        }
    }

    /// Builds the statements for a change set. With `inline`, values are
    /// rendered as literals in the same pass (for previews) instead of being
    /// bound, so a value can never be mistaken for a placeholder.
    fn statements(&self, changes: &ChangeSet, inline: bool) -> Result<Vec<(String, Vec<Value>)>> {
        let table = self.table(&changes.collection);
        changes
            .changes
            .iter()
            .map(|change| {
                let mut params = Vec::new();
                let sql = match change {
                    Change::Update { key, set } => {
                        anyhow::ensure!(!key.is_empty(), "an update needs a key");
                        anyhow::ensure!(!set.is_empty(), "an update needs at least one field");
                        let assignments = set
                            .iter()
                            .map(|(field, value)| {
                                format!("{} = {}", self.quote(field), self.bind(value, &mut params, inline))
                            })
                            .collect::<Vec<_>>()
                            .join(", ");
                        let condition = self.key_condition(key, &mut params, inline);
                        format!("UPDATE {table} SET {assignments} WHERE {condition}")
                    }
                    Change::Insert { values } => {
                        anyhow::ensure!(!values.is_empty(), "an insert needs at least one field");
                        let fields = values
                            .iter()
                            .map(|(field, _)| self.quote(field))
                            .collect::<Vec<_>>()
                            .join(", ");
                        let placeholders = values
                            .iter()
                            .map(|(_, value)| self.bind(value, &mut params, inline))
                            .collect::<Vec<_>>()
                            .join(", ");
                        format!("INSERT INTO {table} ({fields}) VALUES ({placeholders})")
                    }
                    Change::Delete { key } => {
                        anyhow::ensure!(!key.is_empty(), "a delete needs a key");
                        let condition = self.key_condition(key, &mut params, inline);
                        format!("DELETE FROM {table} WHERE {condition}")
                    }
                };
                Ok((sql, params))
            })
            .collect()
    }

    fn bind(&self, value: &Value, params: &mut Vec<Value>, inline: bool) -> String {
        if inline {
            sql_literal(self.kind, value)
        } else {
            params.push(value.clone());
            self.placeholder(params.len())
        }
    }

    fn key_condition(&self, key: &[(String, Value)], params: &mut Vec<Value>, inline: bool) -> String {
        key.iter()
            .map(|(field, value)| {
                if value.is_nullish() {
                    format!("{} IS NULL", self.quote(field))
                } else {
                    format!("{} = {}", self.quote(field), self.bind(value, params, inline))
                }
            })
            .collect::<Vec<_>>()
            .join(" AND ")
    }

    fn filter_condition(&self, filter: &Filter, params: &mut Vec<Value>) -> String {
        let field = self.quote(&filter.field);
        let mut bind = |value: Value| {
            params.push(value);
            self.placeholder(params.len())
        };
        match filter.op {
            FilterOp::IsNull => format!("{field} IS NULL"),
            FilterOp::IsNotNull => format!("{field} IS NOT NULL"),
            FilterOp::Contains => {
                let pattern = Value::Text(format!("%{}%", filter.value.display()));
                match self.kind {
                    DatabaseKind::Postgres => format!("CAST({field} AS TEXT) ILIKE {}", bind(pattern)),
                    _ => format!("{field} LIKE {}", bind(pattern)),
                }
            }
            op => {
                let operator = match op {
                    FilterOp::Eq => "=",
                    FilterOp::NotEq => "<>",
                    FilterOp::Lt => "<",
                    FilterOp::Lte => "<=",
                    FilterOp::Gt => ">",
                    _ => ">=",
                };
                format!("{field} {operator} {}", bind(filter.value.clone()))
            }
        }
    }

    async fn primary_key(&self, collection: &CollectionRef) -> Result<Vec<String>> {
        Ok(self
            .introspection()?
            .get_primary_key(collection.namespace.as_deref(), &collection.name)
            .await?
            .map(|key| key.columns)
            .unwrap_or_default())
    }

    async fn probe_mysql(&self) -> Result<Capability> {
        let flags = self
            .rows("SELECT @@global.read_only, @@global.super_read_only", &[])
            .await?;
        if let Some(row) = flags.rows.first()
            && row.iter().any(|value| value_is_true(value))
        {
            return Ok(Capability::ReadOnly {
                reason: "the server is read-only".to_string(),
            });
        }
        let mut grants = text_column(&self.rows("SHOW GRANTS", &[]).await?);
        let roles = grants.iter().filter_map(|grant| granted_role(grant)).collect::<Vec<_>>();
        if !roles.is_empty() {
            let sql = format!("SHOW GRANTS FOR CURRENT_USER() USING {}", roles.join(", "));
            grants.extend(text_column(&self.rows(&sql, &[]).await?));
        }
        Ok(if mysql_grants_allow_writes(&grants, self.database.as_deref()) {
            Capability::Writable
        } else {
            Capability::ReadOnly {
                reason: "this user has no INSERT, UPDATE or DELETE grant".to_string(),
            }
        })
    }

    async fn probe_postgres(&self) -> Result<Capability> {
        let read_only = self
            .rows(
                "SELECT pg_is_in_recovery() \
                 OR current_setting('transaction_read_only') = 'on' \
                 OR current_setting('default_transaction_read_only') = 'on'",
                &[],
            )
            .await?;
        if first_value(&read_only).is_some_and(value_is_true) {
            return Ok(Capability::ReadOnly {
                reason: "the server or session is read-only".to_string(),
            });
        }
        let writable = self
            .rows(
                "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
                 WHERE table_schema NOT IN ('pg_catalog', 'information_schema') \
                 AND table_type = 'BASE TABLE' \
                 AND has_table_privilege(quote_ident(table_schema) || '.' || quote_ident(table_name), \
                 'INSERT, UPDATE, DELETE'))",
                &[],
            )
            .await?;
        Ok(if first_value(&writable).is_some_and(value_is_true) {
            Capability::Writable
        } else {
            Capability::ReadOnly {
                reason: "this role has no INSERT, UPDATE or DELETE privilege".to_string(),
            }
        })
    }
}

#[async_trait]
impl Source for SqlSource {
    fn query_language(&self) -> QueryLanguage {
        match self.kind {
            DatabaseKind::Postgres => QueryLanguage::Postgres,
            _ => QueryLanguage::MySql,
        }
    }

    async fn probe(&self) -> Result<Capability> {
        match self.kind {
            DatabaseKind::Postgres => self.probe_postgres().await,
            _ => self.probe_mysql().await,
        }
    }

    async fn collections(&self) -> Result<Vec<Collection>> {
        let (sql, params) = match self.kind {
            DatabaseKind::Postgres => (
                "SELECT table_schema, table_name, table_type FROM information_schema.tables \
                 WHERE table_schema NOT IN ('pg_catalog', 'information_schema') \
                 AND table_schema NOT LIKE 'pg_toast%' ORDER BY 1, 2",
                Vec::new(),
            ),
            _ => match &self.database {
                Some(database) => (
                    "SELECT table_schema AS table_schema, table_name AS table_name, \
                     table_type AS table_type FROM information_schema.tables \
                     WHERE table_schema = ? ORDER BY 1, 2",
                    vec![Value::Text(database.clone())],
                ),
                None => (
                    "SELECT table_schema AS table_schema, table_name AS table_name, \
                     table_type AS table_type FROM information_schema.tables \
                     WHERE table_schema NOT IN ('mysql', 'information_schema', \
                     'performance_schema', 'sys') ORDER BY 1, 2",
                    Vec::new(),
                ),
            },
        };
        let result = self.rows(sql, &params).await?;
        Ok(result
            .rows
            .iter()
            .filter_map(|row| {
                let [namespace, name, table_type] = row.as_slice() else {
                    return None;
                };
                Some(Collection {
                    reference: CollectionRef {
                        namespace: Some(namespace.display()),
                        name: name.display(),
                    },
                    kind: if table_type.display().contains("VIEW") {
                        CollectionKind::View
                    } else {
                        CollectionKind::Table
                    },
                })
            })
            .collect())
    }

    async fn describe(&self, collection: &CollectionRef) -> Result<CollectionSchema> {
        let introspection = self.introspection()?;
        let namespace = collection.namespace.as_deref();
        let columns = introspection.get_columns(namespace, &collection.name).await?;
        let mut primary_key = self.primary_key(collection).await?;
        if primary_key.is_empty() {
            primary_key = columns
                .iter()
                .filter(|column| column.is_primary_key)
                .map(|column| column.name.clone())
                .collect();
        }
        let references = introspection
            .get_foreign_keys(namespace, &collection.name)
            .await?
            .into_iter()
            .map(|foreign_key| Reference {
                source: collection.clone(),
                fields: foreign_key.columns,
                target: CollectionRef {
                    namespace: foreign_key
                        .referenced_schema
                        .or_else(|| collection.namespace.clone()),
                    name: foreign_key.referenced_table,
                },
                target_fields: foreign_key.referenced_columns,
            })
            .collect();
        let mut referenced_by = Vec::new();
        if let Some(all) = introspection.list_all_foreign_keys(namespace).await? {
            for (table, foreign_keys) in all {
                for foreign_key in foreign_keys {
                    let target_namespace = foreign_key
                        .referenced_schema
                        .clone()
                        .or_else(|| collection.namespace.clone());
                    if foreign_key.referenced_table == collection.name
                        && target_namespace == collection.namespace
                    {
                        referenced_by.push(Reference {
                            source: CollectionRef {
                                namespace: collection.namespace.clone(),
                                name: table.clone(),
                            },
                            fields: foreign_key.columns,
                            target: collection.clone(),
                            target_fields: foreign_key.referenced_columns,
                        });
                    }
                }
            }
        }
        let writable = match self.kind {
            DatabaseKind::Postgres => {
                let result = self
                    .rows(
                        "SELECT has_table_privilege($1, 'INSERT, UPDATE, DELETE')",
                        &[Value::Text(self.table(collection))],
                    )
                    .await?;
                first_value(&result).map(value_is_true)
            }
            _ => None,
        };
        Ok(CollectionSchema {
            fields: columns
                .into_iter()
                .map(|column| Field {
                    name: column.name,
                    type_name: column.data_type,
                    nullable: column.nullable,
                })
                .collect(),
            primary_key,
            references,
            referenced_by,
            writable,
        })
    }

    async fn query(&self, text: &str, limit: usize, read_only: bool) -> Result<ResultSet> {
        let text = text.trim().trim_end_matches(';').trim();
        anyhow::ensure!(!text.is_empty(), "the query is empty");
        let result = if read_only {
            // Three layers, because each alone has a hole: a second statement
            // could COMMIT its way out of the transaction, and MySQL commits
            // implicitly before DDL, running it outside a read-only
            // transaction unless the session itself is read-only.
            check_read_only_statement(self.kind, text)?;
            let mysql = self.kind == DatabaseKind::Mysql;
            if mysql {
                self.statement("SET SESSION TRANSACTION READ ONLY").await?;
            }
            let result = match self.statement(self.begin_read_only()).await {
                Ok(()) => {
                    let result = self.rows(text, &[]).await;
                    // Always end the transaction, even after a failed query,
                    // so the session is not left inside it.
                    self.statement("ROLLBACK").await.map(|()| result)
                }
                Err(error) => Err(error),
            };
            if mysql {
                self.statement("SET SESSION TRANSACTION READ WRITE").await?;
            }
            result??
        } else {
            self.rows(text, &[]).await?
        };
        Ok(truncate(result, limit))
    }

    async fn browse(&self, collection: &CollectionRef, request: &BrowseRequest) -> Result<Page> {
        let offset: usize = match &request.cursor {
            Some(cursor) => cursor.parse().context("invalid cursor")?,
            None => 0,
        };
        let mut params = Vec::new();
        let mut sql = format!("SELECT * FROM {}", self.table(collection));
        if !request.filters.is_empty() {
            let conditions = request
                .filters
                .iter()
                .map(|filter| self.filter_condition(filter, &mut params))
                .collect::<Vec<_>>();
            sql.push_str(" WHERE ");
            sql.push_str(&conditions.join(" AND "));
        }
        let primary_key = self.primary_key(collection).await?;
        if !primary_key.is_empty() {
            let order = primary_key
                .iter()
                .map(|field| self.quote(field))
                .collect::<Vec<_>>()
                .join(", ");
            sql.push_str(&format!(" ORDER BY {order}"));
        }
        sql.push_str(&format!(" LIMIT {} OFFSET {offset}", request.page_size + 1));
        let mut rows = self.rows(&sql, &params).await?;
        let has_more = rows.rows.len() > request.page_size;
        rows.rows.truncate(request.page_size);
        Ok(Page {
            next_cursor: has_more.then(|| (offset + request.page_size).to_string()),
            rows,
        })
    }

    fn render_changes(&self, changes: &ChangeSet) -> Result<String> {
        Ok(self
            .statements(changes, true)?
            .iter()
            .map(|(sql, _)| format!("{sql};"))
            .collect::<Vec<_>>()
            .join("\n"))
    }

    async fn apply(&self, changes: &ChangeSet) -> Result<u64> {
        let statements = self.statements(changes, false)?;
        self.statement(self.begin()).await?;
        let mut affected = 0;
        for (sql, params) in &statements {
            let result = match self.rows(sql, params).await {
                Ok(result) => result,
                Err(error) => {
                    self.statement("ROLLBACK").await?;
                    return Err(error);
                }
            };
            let rows = result.affected_rows.unwrap_or_default();
            // A key that turns out not to be unique would change every
            // matching row. Undo the whole change set instead.
            if rows > 1 {
                self.statement("ROLLBACK").await?;
                bail!("refusing to commit: `{sql}` would change {rows} rows, not one");
            }
            affected += rows;
        }
        self.statement("COMMIT").await?;
        Ok(affected)
    }
}

/// Maps Echo's TLS mode names onto each driver's spelling.
fn driver_ssl_mode(kind: DatabaseKind, mode: &str) -> String {
    let mode = mode.to_ascii_lowercase().replace('-', "_");
    match kind {
        DatabaseKind::Mysql => match mode.as_str() {
            "disable" => "DISABLED",
            "prefer" => "PREFERRED",
            "require" => "REQUIRED",
            "verify_ca" => "VERIFY_CA",
            "verify_full" => "VERIFY_IDENTITY",
            other => return other.to_ascii_uppercase(),
        }
        .to_string(),
        _ => match mode.as_str() {
            "verify_ca" => "verify-ca".to_string(),
            "verify_full" => "verify-full".to_string(),
            other => other.to_string(),
        },
    }
}

fn truncate(mut result: ResultSet, limit: usize) -> ResultSet {
    // ponytail: the drivers buffer whole results, so the limit only bounds
    // what is kept and sent on. Streaming cursors would bound memory too.
    if result.rows.len() > limit {
        result.rows.truncate(limit);
        result.truncated = true;
    }
    result
}

fn first_value(result: &ResultSet) -> Option<&Value> {
    result.rows.first().and_then(|row| row.first())
}

fn value_is_true(value: &Value) -> bool {
    match value {
        Value::Bool(value) => *value,
        Value::Int(value) => *value != 0,
        Value::Text(value) => matches!(value.as_str(), "1" | "t" | "true" | "on" | "ON"),
        _ => false,
    }
}

fn text_column(result: &ResultSet) -> Vec<String> {
    result
        .rows
        .iter()
        .filter_map(|row| row.first().map(Value::display))
        .collect()
}

pub(crate) fn quote_identifier(kind: DatabaseKind, identifier: &str) -> String {
    match kind {
        DatabaseKind::Postgres => format!("\"{}\"", identifier.replace('"', "\"\"")),
        _ => format!("`{}`", identifier.replace('`', "``")),
    }
}

/// Renders a value as a SQL literal for display. Statements are executed
/// with bound parameters, never with these literals.
fn sql_literal(kind: DatabaseKind, value: &Value) -> String {
    match value {
        Value::Missing | Value::Null => "NULL".to_string(),
        Value::Bool(value) => if *value { "TRUE" } else { "FALSE" }.to_string(),
        Value::Int(value) => value.to_string(),
        Value::Float(value) => value.to_string(),
        Value::Bytes(bytes) => {
            let hex = bytes.iter().map(|byte| format!("{byte:02x}")).collect::<String>();
            match kind {
                DatabaseKind::Postgres => format!("'\\x{hex}'"),
                _ => format!("X'{hex}'"),
            }
        }
        Value::Text(_) | Value::Json(_) => {
            let text = match value {
                Value::Text(text) => text.clone(),
                _ => value.display(),
            };
            let escaped = match kind {
                DatabaseKind::Mysql => text.replace('\\', "\\\\").replace('\'', "''"),
                _ => text.replace('\'', "''"),
            };
            format!("'{escaped}'")
        }
    }
}

/// Accepts a single statement that starts with a reading keyword. This is a
/// guard in front of the read-only transaction, not a substitute for it:
/// `WITH ... DELETE` and `EXPLAIN ANALYZE DELETE` pass here and are stopped by
/// the database.
fn check_read_only_statement(kind: DatabaseKind, text: &str) -> Result<()> {
    let tokens = top_level_tokens(kind, text);
    if let Some(position) = tokens.iter().position(|token| token == ";")
        && tokens[position + 1..].iter().any(|token| token != ";")
    {
        bail!("read-only queries run one statement at a time");
    }
    let first = tokens
        .iter()
        .find(|token| token.as_str() != "(")
        .map(|token| token.to_ascii_uppercase())
        .unwrap_or_default();
    let allowed: &[&str] = match kind {
        DatabaseKind::Postgres => &["SELECT", "WITH", "VALUES", "TABLE", "SHOW", "EXPLAIN"],
        _ => &["SELECT", "WITH", "VALUES", "TABLE", "SHOW", "DESCRIBE", "DESC", "EXPLAIN"],
    };
    if !allowed.contains(&first.as_str()) {
        bail!("only reading statements run here; `{first}` would need a writable query");
    }
    Ok(())
}

/// Splits SQL into words and punctuation outside strings, quoted
/// identifiers and comments. Only `;`, `(` and leading words matter to the
/// caller, so everything else is dropped.
fn top_level_tokens(kind: DatabaseKind, text: &str) -> Vec<String> {
    let characters = text.chars().collect::<Vec<_>>();
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < characters.len() {
        let character = characters[index];
        let next = characters.get(index + 1).copied();
        match character {
            '\'' | '"' | '`' => {
                index += 1;
                while index < characters.len() {
                    if characters[index] == '\\' && kind == DatabaseKind::Mysql {
                        index += 2;
                        continue;
                    }
                    if characters[index] == character {
                        if characters.get(index + 1) == Some(&character) {
                            index += 2;
                            continue;
                        }
                        break;
                    }
                    index += 1;
                }
                index += 1;
            }
            '-' if next == Some('-') => {
                while index < characters.len() && characters[index] != '\n' {
                    index += 1;
                }
            }
            '#' if kind == DatabaseKind::Mysql => {
                while index < characters.len() && characters[index] != '\n' {
                    index += 1;
                }
            }
            '/' if next == Some('*') => {
                index += 2;
                while index + 1 < characters.len() && !(characters[index] == '*' && characters[index + 1] == '/') {
                    index += 1;
                }
                index += 2;
            }
            '$' if kind == DatabaseKind::Postgres => {
                // Dollar quoting: $tag$ ... $tag$.
                let tag_end = characters[index + 1..]
                    .iter()
                    .position(|candidate| !(candidate.is_alphanumeric() || *candidate == '_'))
                    .map(|offset| index + 1 + offset);
                match tag_end {
                    Some(tag_end) if characters[tag_end] == '$' => {
                        let tag = characters[index..=tag_end].iter().collect::<String>();
                        let body_start = tag_end + 1;
                        let rest = characters[body_start..].iter().collect::<String>();
                        index = match rest.find(&tag) {
                            Some(offset) => body_start + rest[..offset].chars().count() + tag.chars().count(),
                            None => characters.len(),
                        };
                    }
                    _ => index += 1,
                }
            }
            ';' | '(' => {
                tokens.push(character.to_string());
                index += 1;
            }
            character if character.is_alphabetic() || character == '_' => {
                let start = index;
                while index < characters.len() && (characters[index].is_alphanumeric() || characters[index] == '_') {
                    index += 1;
                }
                tokens.push(characters[start..index].iter().collect());
            }
            _ => index += 1,
        }
    }
    tokens
}

/// Extracts the role names from a `GRANT `role`@`host` TO ...` line.
fn granted_role(grant: &str) -> Option<String> {
    let rest = grant.strip_prefix("GRANT ")?;
    if rest.contains(" ON ") {
        return None;
    }
    let roles = rest.split(" TO ").next()?;
    Some(roles.trim().to_string())
}

fn mysql_grants_allow_writes(grants: &[String], database: Option<&str>) -> bool {
    grants.iter().any(|grant| {
        let Some(rest) = grant.strip_prefix("GRANT ") else {
            return false;
        };
        let Some((privileges, rest)) = rest.split_once(" ON ") else {
            return false;
        };
        let target = rest.split(" TO ").next().unwrap_or_default().trim();
        let privileges = privileges.to_ascii_uppercase();
        let writes = privileges.split(',').any(|privilege| {
            matches!(
                privilege.trim(),
                "ALL" | "ALL PRIVILEGES" | "INSERT" | "UPDATE" | "DELETE"
            )
        });
        if !writes {
            return false;
        }
        let target_database = target.split('.').next().unwrap_or_default();
        let target_database = target_database.trim_matches('`').replace("\\_", "_");
        match database {
            None => true,
            Some(database) => target_database == "*" || target_database == database,
        }
    })
}

fn to_driver_value(value: &Value) -> zqlz_core::Value {
    match value {
        Value::Missing | Value::Null => zqlz_core::Value::Null,
        Value::Bool(value) => zqlz_core::Value::Bool(*value),
        Value::Int(value) => zqlz_core::Value::Int64(*value),
        Value::Float(value) => zqlz_core::Value::Float64(*value),
        Value::Text(value) => zqlz_core::Value::String(value.clone()),
        Value::Bytes(value) => zqlz_core::Value::Bytes(value.clone()),
        Value::Json(value) => zqlz_core::Value::Json(value.clone()),
    }
}

fn from_driver_value(value: zqlz_core::Value) -> Value {
    use zqlz_core::Value as Driver;
    match value {
        Driver::Null => Value::Null,
        Driver::Bool(value) => Value::Bool(value),
        Driver::Int8(value) => Value::Int(value.into()),
        Driver::Int16(value) => Value::Int(value.into()),
        Driver::Int32(value) => Value::Int(value.into()),
        Driver::Int64(value) => Value::Int(value),
        Driver::Float32(value) => Value::Float(value.into()),
        Driver::Float64(value) => Value::Float(value),
        Driver::Decimal(value) => Value::Text(value),
        Driver::String(value) => Value::Text(value),
        Driver::Bytes(value) => match String::from_utf8(value) {
            Ok(text) => Value::Text(text),
            Err(error) => Value::Bytes(error.into_bytes()),
        },
        Driver::Uuid(value) => Value::Text(value.to_string()),
        Driver::Date(value) => Value::Text(value.to_string()),
        Driver::Time(value) => Value::Text(value.to_string()),
        Driver::DateTime(value) => Value::Text(value.format("%Y-%m-%d %H:%M:%S%.f").to_string()),
        Driver::DateTimeUtc(value) => Value::Text(value.to_rfc3339()),
        Driver::Json(value) => Value::Json(value),
        Driver::Array(values) => Value::Json(serde_json::Value::Array(
            values
                .into_iter()
                .map(|value| from_driver_value(value).to_json())
                .collect(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mysql_grants() {
        let grants = |lines: &[&str]| lines.iter().map(|line| line.to_string()).collect::<Vec<_>>();
        assert!(!mysql_grants_allow_writes(
            &grants(&["GRANT SELECT ON *.* TO `reader`@`%`"]),
            Some("ryse")
        ));
        assert!(mysql_grants_allow_writes(
            &grants(&["GRANT ALL PRIVILEGES ON *.* TO `root`@`%`"]),
            Some("ryse")
        ));
        assert!(mysql_grants_allow_writes(
            &grants(&["GRANT SELECT, INSERT, UPDATE ON `ryse`.* TO `app`@`%`"]),
            Some("ryse")
        ));
        assert!(!mysql_grants_allow_writes(
            &grants(&["GRANT SELECT, INSERT ON `other`.* TO `app`@`%`"]),
            Some("ryse")
        ));
        assert!(mysql_grants_allow_writes(
            &grants(&["GRANT DELETE ON `ryse\\_db`.`seller` TO `app`@`%`"]),
            Some("ryse_db")
        ));
        assert_eq!(
            granted_role("GRANT `app_writer`@`%` TO `joshua`@`%`"),
            Some("`app_writer`@`%`".to_string())
        );
        assert_eq!(granted_role("GRANT SELECT ON *.* TO `u`@`%`"), None);
    }

    #[test]
    fn test_read_only_statement_check() {
        let check = |text: &str| check_read_only_statement(DatabaseKind::Postgres, text);
        assert!(check("SELECT 1").is_ok());
        assert!(check("  -- note\n (SELECT ';' FROM t) ;  ").is_ok());
        assert!(check("WITH x AS (SELECT 1) SELECT * FROM x").is_ok());
        assert!(check("SELECT $$;$$").is_ok());
        assert!(check("COMMIT; DROP TABLE seller").is_err());
        assert!(check("SELECT 1; DELETE FROM seller").is_err());
        assert!(check("DROP TABLE seller").is_err());
        assert!(check("/* SELECT */ DELETE FROM seller").is_err());
        assert!(check_read_only_statement(DatabaseKind::Mysql, "SHOW TABLES").is_ok());
        assert!(check_read_only_statement(DatabaseKind::Mysql, "SELECT 'it\\'s;' # x").is_ok());
        assert!(check_read_only_statement(DatabaseKind::Mysql, "SELECT 1 # ;\n; DROP TABLE t").is_err());
    }
}
