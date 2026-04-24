use pglite_oxide::{
    Pglite, PgliteError, QueryOptions, QueryTemplate, RowMode, format_query, quote_identifier,
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

fn first_row(result: &pglite_oxide::Results) -> anyhow::Result<&serde_json::Map<String, Value>> {
    result
        .rows
        .first()
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow::anyhow!("expected first row object"))
}

#[test]
fn runtime_smoke() -> anyhow::Result<()> {
    let mut pg = Pglite::builder().temporary().open()?;
    assert!(pg.paths().pgdata.join("PG_VERSION").exists());

    let version = pg.query(
        "SELECT current_setting('server_version_num')::int AS version_num",
        &[],
        None,
    )?;
    let version_num = first_row(&version)?
        .get("version_num")
        .and_then(Value::as_i64)
        .expect("version_num");
    assert!(
        version_num >= 170_000,
        "expected PostgreSQL 17+, got {version_num}"
    );

    pg.exec("CREATE TABLE items(value TEXT)", None)?;

    // COPY FROM '/dev/blob'
    let mut options = QueryOptions::default();
    let rows = b"alpha\nbeta\n";
    options.blob = Some(rows.to_vec());
    pg.exec("COPY items(value) FROM '/dev/blob'", Some(&options))?;

    // COPY TO '/dev/blob' and verify blob contents
    let results = pg.exec("COPY items TO '/dev/blob'", None)?;
    let blob = results
        .last()
        .and_then(|res| res.blob.as_ref())
        .expect("expected blob data from COPY TO");
    assert_eq!(std::str::from_utf8(blob)?.trim_end(), "alpha\nbeta");

    // Listen for notifications
    let events = Arc::new(Mutex::new(Vec::new()));
    let events_clone = Arc::clone(&events);
    let handle = pg.listen("test_channel", move |payload| {
        events_clone
            .lock()
            .expect("lock poisoning")
            .push(payload.to_string());
    })?;

    pg.exec("SELECT pg_notify('test_channel', 'hello world')", None)?;

    let recorded = events.lock().unwrap();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0], "hello world");
    drop(recorded);

    pg.unlisten(handle)?;

    let formatted = format_query(&mut pg, "SELECT $1::int", &[json!(42)])?;
    assert_eq!(formatted, "SELECT '42'::int");

    let mut tpl = QueryTemplate::new();
    tpl.push_sql("SELECT ");
    tpl.push_identifier("items");
    tpl.push_sql(" WHERE value = ");
    tpl.push_param(json!("alpha"));
    let templated = tpl.build();
    assert_eq!(templated.query, "SELECT \"items\" WHERE value = $1");
    assert_eq!(templated.params[0], json!("alpha"));

    assert_eq!(quote_identifier("Test"), "\"Test\"");

    let typed_sql = "SELECT \
            ($1::int + 1) AS next_int, \
            $2::bool AS flag, \
            $3::jsonb AS doc, \
            $4::text[] AS labels, \
            $5::bytea AS bytes";
    let typed = pg.query(
        typed_sql,
        &[
            json!(41),
            json!(true),
            json!({"name": "pglite", "ok": true}),
            json!(["alpha", "beta,gamma"]),
            json!([0, 1, 2, 255]),
        ],
        None,
    )?;
    let typed_row = first_row(&typed)?;
    assert_eq!(typed_row.get("next_int"), Some(&json!(42)));
    assert_eq!(typed_row.get("flag"), Some(&json!(true)));
    assert_eq!(
        typed_row.get("doc").and_then(|value| value.get("name")),
        Some(&json!("pglite"))
    );
    assert_eq!(
        typed_row.get("labels"),
        Some(&json!(["alpha", "beta,gamma"]))
    );
    assert_eq!(typed_row.get("bytes"), Some(&json!([0, 1, 2, 255])));

    let array_options = QueryOptions {
        row_mode: Some(RowMode::Array),
        ..QueryOptions::default()
    };
    let array_result = pg.query(
        "SELECT 1::int AS one, 'two'::text AS two",
        &[],
        Some(&array_options),
    )?;
    assert_eq!(array_result.rows.first(), Some(&json!([1, "two"])));

    pg.exec("CREATE TABLE tx_items(value TEXT)", None)?;
    pg.transaction(|tx| {
        tx.query(
            "INSERT INTO tx_items(value) VALUES ($1) RETURNING value",
            &[json!("committed")],
            None,
        )?;
        Ok(())
    })?;
    let rollback: anyhow::Result<()> = pg.transaction(|tx| {
        tx.exec("INSERT INTO tx_items(value) VALUES ('rolled back')", None)?;
        Err(anyhow::anyhow!("force rollback"))
    });
    assert!(rollback.is_err());
    let count = pg.query("SELECT count(*)::int AS count FROM tx_items", &[], None)?;
    assert_eq!(first_row(&count)?.get("count"), Some(&json!(1)));

    let err = pg
        .query(
            "SELECT * FROM missing_table WHERE id = $1",
            &[json!(7)],
            None,
        )
        .expect_err("missing table should fail");
    if let Some(pg_err) = err.downcast_ref::<PgliteError>() {
        assert_eq!(pg_err.query(), "SELECT * FROM missing_table WHERE id = $1");
        assert_eq!(pg_err.params(), &[json!(7)]);
        assert_eq!(pg_err.database_error().code.as_deref(), Some("42P01"));
    } else {
        let message = format!("{err:#}");
        assert!(
            message.contains(
                "failed to execute extended query: SELECT * FROM missing_table WHERE id = $1"
            ),
            "{message}"
        );
    }

    pg.close()?;
    assert!(pg.is_closed());

    let mut restarted = Pglite::temporary()?;
    let restarted_result = restarted.query("SELECT 42::int AS answer", &[], None)?;
    assert_eq!(
        first_row(&restarted_result)?.get("answer"),
        Some(&json!(42))
    );
    restarted.close()?;

    let persistent_dir = tempfile::TempDir::new()?;
    {
        let mut persisted = Pglite::builder().path(persistent_dir.path()).open()?;
        persisted.exec("CREATE TABLE persisted(value TEXT)", None)?;
        persisted.query(
            "INSERT INTO persisted(value) VALUES ($1)",
            &[json!("kept")],
            None,
        )?;
        persisted.close()?;
    }
    {
        let mut reopened = Pglite::open(persistent_dir.path())?;
        let persisted_result = reopened.query("SELECT value FROM persisted", &[], None)?;
        assert_eq!(
            first_row(&persisted_result)?.get("value"),
            Some(&json!("kept"))
        );
        reopened.close()?;
    }

    Ok(())
}
