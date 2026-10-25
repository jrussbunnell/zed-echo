//! Runs the engine against real databases. Skipped unless
//! `DATABASE_LIVE_TESTS=1`; see `script/database-test-containers` for the
//! containers and seed data these expect.

use std::sync::Arc;

use database::{
    BrowseRequest, Capability, Change, ChangeSet, CollectionRef, CompareOptions, CompareSide,
    ConnectionSpec, DatabaseKind, Engine, Filter, FilterOp, Request, Response, ResultSet, Value,
    compare,
};

fn enabled() -> bool {
    std::env::var("DATABASE_LIVE_TESTS").is_ok_and(|value| value == "1")
}

fn engine() -> Engine {
    Engine::new(Arc::new(reqwest_client::ReqwestClient::new()))
}

fn spec(kind: DatabaseKind, port: u16, user: &str) -> ConnectionSpec {
    ConnectionSpec {
        kind,
        host: Some("127.0.0.1".into()),
        port: Some(port),
        database: Some("echo_test".into()),
        user: Some(user.into()),
        url: None,
        ssl_mode: Some("disable".into()),
        read_only: false,
    }
}

fn http_spec(kind: DatabaseKind, url: &str, user: Option<&str>) -> ConnectionSpec {
    ConnectionSpec {
        kind,
        host: None,
        port: None,
        database: None,
        user: user.map(str::to_string),
        url: Some(url.into()),
        ssl_mode: None,
        read_only: false,
    }
}

async fn connect(engine: &Engine, name: &str, spec: ConnectionSpec, secret: &str) -> Capability {
    match engine
        .handle(Request::Connect {
            connection: name.into(),
            spec,
            secret: Some(secret.into()),
        })
        .await
        .unwrap()
    {
        Response::Connected { capability, .. } => capability,
        other => panic!("unexpected {other:?}"),
    }
}

async fn query(
    engine: &Engine,
    name: &str,
    text: &str,
    read_only: bool,
) -> anyhow::Result<ResultSet> {
    match engine
        .handle(Request::Query {
            connection: name.into(),
            text: text.into(),
            limit: 100,
            read_only,
        })
        .await?
    {
        Response::Rows(rows) => Ok(rows),
        other => panic!("unexpected {other:?}"),
    }
}

async fn browse(
    engine: &Engine,
    name: &str,
    collection: &CollectionRef,
    filters: Vec<Filter>,
    cursor: Option<String>,
    page_size: usize,
) -> database::Page {
    match engine
        .handle(Request::Browse {
            connection: name.into(),
            collection: collection.clone(),
            request: BrowseRequest {
                filters,
                cursor,
                page_size,
            },
        })
        .await
        .unwrap()
    {
        Response::Page(page) => page,
        other => panic!("unexpected {other:?}"),
    }
}

async fn apply(engine: &Engine, name: &str, changes: ChangeSet) -> anyhow::Result<u64> {
    match engine
        .handle(Request::Apply {
            connection: name.into(),
            changes,
        })
        .await?
    {
        Response::Applied(count) => Ok(count),
        other => panic!("unexpected {other:?}"),
    }
}

async fn exercise_sql(kind: DatabaseKind, port: u16, namespace: &str) {
    let engine = engine();
    assert_eq!(
        connect(
            &engine,
            "admin",
            spec(
                kind,
                port,
                if kind == DatabaseKind::Mysql {
                    "root"
                } else {
                    "postgres"
                }
            ),
            "echo"
        )
        .await,
        Capability::Writable
    );
    assert!(matches!(
        connect(&engine, "reader", spec(kind, port, "reader"), "reader").await,
        Capability::ReadOnly { .. }
    ));

    let Response::Collections(collections) = engine
        .handle(Request::Collections {
            connection: "admin".into(),
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    let seller = CollectionRef::new(Some(namespace), "seller");
    assert!(
        collections
            .iter()
            .any(|collection| collection.reference == seller),
        "{collections:?}"
    );

    let Response::Schema(schema) = engine
        .handle(Request::Describe {
            connection: "admin".into(),
            collection: seller.clone(),
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(schema.primary_key, vec!["id".to_string()]);
    assert!(
        schema
            .referenced_by
            .iter()
            .any(|reference| reference.source.name == "property"
                && reference.fields == vec!["seller_id".to_string()]),
        "{schema:?}"
    );

    let rows = query(
        &engine,
        "admin",
        "SELECT id, email FROM seller ORDER BY id;",
        false,
    )
    .await
    .unwrap();
    assert_eq!(rows.rows.len(), 3);
    assert_eq!(rows.rows[0][1], Value::Text("a@x.com".into()));

    // A read-only query on a writable connection must not write.
    let attempt = query(
        &engine,
        "admin",
        "INSERT INTO seller (id, email) VALUES (99, 'no@x.com')",
        true,
    )
    .await;
    assert!(attempt.is_err(), "read-only query wrote: {attempt:?}");
    let count = query(&engine, "admin", "SELECT COUNT(*) FROM seller", false)
        .await
        .unwrap();
    assert!(matches!(count.rows[0][0], Value::Int(3)), "{count:?}");
    // Escapes: DDL (MySQL commits implicitly before it) and statement chains.
    for escape in [
        "CREATE TABLE escaped (id INT)",
        "COMMIT; CREATE TABLE escaped (id INT)",
        "SELECT 1; DROP TABLE property",
        "/* SELECT */ DROP TABLE property",
    ] {
        assert!(
            query(&engine, "admin", escape, true).await.is_err(),
            "{escape} ran read-only"
        );
    }
    let escaped = query(
        &engine,
        "admin",
        "SELECT COUNT(*) FROM information_schema.tables WHERE table_name = 'escaped'",
        false,
    )
    .await
    .unwrap();
    assert!(matches!(escaped.rows[0][0], Value::Int(0)), "{escaped:?}");
    query(&engine, "admin", "SELECT COUNT(*) FROM property", false)
        .await
        .unwrap();
    // And the session is writable again afterwards.
    query(&engine, "admin", "SELECT 1", false).await.unwrap();
    query(&engine, "admin", "CREATE TABLE scratch (id INT)", false)
        .await
        .unwrap();
    query(&engine, "admin", "DROP TABLE scratch", false)
        .await
        .unwrap();

    let first = browse(&engine, "admin", &seller, Vec::new(), None, 2).await;
    assert_eq!(first.rows.rows.len(), 2);
    let second = browse(
        &engine,
        "admin",
        &seller,
        Vec::new(),
        first.next_cursor.clone(),
        2,
    )
    .await;
    assert_eq!(second.rows.rows.len(), 1);
    assert!(second.next_cursor.is_none());
    let filtered = browse(
        &engine,
        "admin",
        &CollectionRef::new(Some(namespace), "property"),
        vec![Filter {
            field: "seller_id".into(),
            op: FilterOp::Eq,
            value: Value::Int(1),
        }],
        None,
        50,
    )
    .await;
    assert_eq!(filtered.rows.rows.len(), 2);

    let changes = ChangeSet {
        collection: seller.clone(),
        changes: vec![Change::Update {
            key: vec![("id".into(), Value::Int(2))],
            set: vec![("phone".into(), Value::Text("it's 5".into()))],
        }],
    };
    let Response::Text(preview) = engine
        .handle(Request::RenderChanges {
            connection: "admin".into(),
            changes: changes.clone(),
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    assert!(preview.contains("'it''s 5'"), "{preview}");
    assert_eq!(apply(&engine, "admin", changes).await.unwrap(), 1);
    let phone = query(
        &engine,
        "admin",
        "SELECT phone FROM seller WHERE id = 2",
        false,
    )
    .await
    .unwrap();
    assert_eq!(phone.rows[0][0], Value::Text("it's 5".into()));

    assert!(
        apply(
            &engine,
            "reader",
            ChangeSet {
                collection: seller.clone(),
                changes: vec![Change::Delete {
                    key: vec![("id".into(), Value::Int(2))]
                }],
            }
        )
        .await
        .is_err()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_mysql() {
    if !enabled() {
        return;
    }
    exercise_sql(DatabaseKind::Mysql, 13306, "echo_test").await;

    // A key matching two rows must roll the whole change set back.
    let engine = engine();
    connect(
        &engine,
        "admin",
        spec(DatabaseKind::Mysql, 13306, "root"),
        "echo",
    )
    .await;
    let error = apply(
        &engine,
        "admin",
        ChangeSet {
            collection: CollectionRef::new(Some("echo_test"), "tag"),
            changes: vec![Change::Update {
                key: vec![("name".into(), Value::Text("dup".into()))],
                set: vec![("color".into(), Value::Text("green".into()))],
            }],
        },
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("2 rows"), "{error}");
    let colors = query(
        &engine,
        "admin",
        "SELECT color FROM tag ORDER BY color",
        false,
    )
    .await
    .unwrap();
    assert_eq!(
        colors.rows,
        vec![
            vec![Value::Text("blue".into())],
            vec![Value::Text("red".into())]
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_postgres() {
    if !enabled() {
        return;
    }
    exercise_sql(DatabaseKind::Postgres, 15432, "public").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_clickhouse() {
    if !enabled() {
        return;
    }
    let engine = engine();
    let url = "http://127.0.0.1:18123";
    assert_eq!(
        connect(
            &engine,
            "admin",
            http_spec(DatabaseKind::Clickhouse, url, Some("default")),
            "echo"
        )
        .await,
        Capability::Writable
    );
    assert!(matches!(
        connect(
            &engine,
            "reader",
            http_spec(DatabaseKind::Clickhouse, url, Some("reader")),
            "reader"
        )
        .await,
        Capability::ReadOnly { .. }
    ));
    let rows = query(
        &engine,
        "reader",
        "SELECT id, kind FROM default.events ORDER BY id",
        false,
    )
    .await
    .unwrap();
    assert_eq!(rows.rows.len(), 3);
    assert_eq!(rows.rows[0][0], Value::Int(1));

    for escape in [
        "INSERT INTO default.events VALUES (9, 9, 'x')",
        "CREATE TABLE default.escaped (id UInt8) ENGINE = Memory",
        "INSERT INTO default.events SETTINGS readonly = 0 VALUES (9, 9, 'x')",
    ] {
        assert!(
            query(&engine, "admin", escape, true).await.is_err(),
            "{escape} ran read-only"
        );
    }
    let count = query(
        &engine,
        "admin",
        "SELECT count() FROM default.events",
        false,
    )
    .await
    .unwrap();
    assert_eq!(count.rows[0][0], Value::Int(3));

    let Response::Schema(schema) = engine
        .handle(Request::Describe {
            connection: "reader".into(),
            collection: CollectionRef::new(Some("default"), "events"),
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(schema.primary_key, vec!["id".to_string()]);
    let page = browse(
        &engine,
        "reader",
        &CollectionRef::new(Some("default"), "events"),
        vec![Filter {
            field: "seller_id".into(),
            op: FilterOp::Eq,
            value: Value::Int(1),
        }],
        None,
        10,
    )
    .await;
    assert_eq!(page.rows.rows.len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_convex() {
    if !enabled() {
        return;
    }
    let Ok(key) = std::fs::read_to_string("/tmp/echo-db-convex-key") else {
        panic!("generate a key with script/database-test-containers");
    };
    let engine = engine();
    let url = "http://127.0.0.1:13210";
    assert_eq!(
        connect(
            &engine,
            "atlas",
            http_spec(DatabaseKind::Convex, url, None),
            key.trim()
        )
        .await,
        Capability::Writable
    );

    let links = CollectionRef::new(None, "ryseSellerUserLinks");
    let first = browse(&engine, "atlas", &links, Vec::new(), None, 2).await;
    assert_eq!(first.rows.columns[0].name, "_id");
    assert!(first.next_cursor.is_some());
    let filtered = browse(
        &engine,
        "atlas",
        &links,
        vec![Filter {
            field: "ryseSellerId".into(),
            op: FilterOp::Eq,
            value: Value::Float(2.0),
        }],
        None,
        10,
    )
    .await;
    assert_eq!(filtered.rows.rows.len(), 1);
    let id_index = filtered.rows.column_index("_id").unwrap();
    let id = filtered.rows.rows[0][id_index].clone();

    let changes = ChangeSet {
        collection: links.clone(),
        changes: vec![Change::Update {
            key: vec![("_id".into(), id.clone())],
            set: vec![("email".into(), Value::Text("b2@x.com".into()))],
        }],
    };
    assert_eq!(apply(&engine, "atlas", changes).await.unwrap(), 1);
    let after = browse(
        &engine,
        "atlas",
        &links,
        vec![Filter {
            field: "_id".into(),
            op: FilterOp::Eq,
            value: id,
        }],
        None,
        10,
    )
    .await;
    let email = after.rows.column_index("email").unwrap();
    assert_eq!(after.rows.rows[0][email], Value::Text("b2@x.com".into()));

    // History: walk the snapshot, then deltas.
    let mut cursor = None;
    let mut entries = 0;
    for _ in 0..10 {
        let Response::History(batch) = engine
            .handle(Request::History {
                connection: "atlas".into(),
                cursor: cursor.clone(),
            })
            .await
            .unwrap()
        else {
            panic!()
        };
        entries += batch.entries.len();
        let done = batch.cursor.starts_with("delta:") && !batch.has_more;
        cursor = Some(batch.cursor);
        if done {
            break;
        }
    }
    assert!(entries >= 3, "{entries}");
    assert!(cursor.unwrap().starts_with("delta:"));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_compare_mysql_against_convex() {
    if !enabled() {
        return;
    }
    let key = std::fs::read_to_string("/tmp/echo-db-convex-key").unwrap();
    let engine = engine();
    connect(
        &engine,
        "ryse",
        spec(DatabaseKind::Mysql, 13306, "reader"),
        "reader",
    )
    .await;
    connect(
        &engine,
        "atlas",
        http_spec(DatabaseKind::Convex, "http://127.0.0.1:13210", None),
        key.trim(),
    )
    .await;
    let left = query(&engine, "ryse", "SELECT id, email FROM seller", true)
        .await
        .unwrap();
    let right = browse(
        &engine,
        "atlas",
        &CollectionRef::new(None, "ryseSellerUserLinks"),
        Vec::new(),
        None,
        100,
    )
    .await
    .rows;
    let left_key = ["id".to_string()];
    let right_key = ["ryseSellerId".to_string()];
    let outcome = compare(
        CompareSide {
            rows: &left,
            key: &left_key,
        },
        CompareSide {
            rows: &right,
            key: &right_key,
        },
        &CompareOptions {
            fields: vec![("email".into(), "email".into())],
        },
    )
    .unwrap();
    // Seller 1 matches, 2 and 3 exist on both sides; nothing is one-sided.
    assert_eq!(outcome.left_only(), 0, "{outcome:?}");
    assert_eq!(outcome.right_only(), 0, "{outcome:?}");
}
