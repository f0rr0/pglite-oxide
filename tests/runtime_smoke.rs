use anyhow::Context;
use pglite_oxide::{
    Pglite, PgliteError, PgliteServer, QueryOptions, QueryTemplate, RowMode, format_query,
    quote_identifier,
};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

fn first_row(result: &pglite_oxide::Results) -> anyhow::Result<&serde_json::Map<String, Value>> {
    result
        .rows
        .first()
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow::anyhow!("expected first row object"))
}

fn assert_file_missing_or_without(path: &std::path::Path, needle: &str) -> anyhow::Result<()> {
    match std::fs::read_to_string(path) {
        Ok(contents) => {
            assert!(
                !contents.contains(needle),
                "{} still contained stale marker {needle:?}: {contents:?}",
                path.display()
            );
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err.into()),
    }
    Ok(())
}

#[test]
fn fresh_temporary_open_select_one() -> anyhow::Result<()> {
    let mut pg = Pglite::builder().fresh_temporary().open()?;
    let result = pg.query("SELECT 1 AS one", &[], None)?;
    assert_eq!(first_row(&result)?.get("one"), Some(&json!(1)));
    pg.close()?;
    Ok(())
}

#[test]
fn persistent_fresh_initdb_survives_restart_and_stale_state_files() -> anyhow::Result<()> {
    let root = tempfile::TempDir::new()?;
    {
        let mut pg = Pglite::builder()
            .path(root.path())
            .template_cache(false)
            .open()?;
        pg.exec("CREATE TABLE fresh_initdb(value TEXT)", None)?;
        pg.query(
            "INSERT INTO fresh_initdb(value) VALUES ($1)",
            &[json!("boot-single-ok")],
            None,
        )?;
        pg.close()?;
    }

    let pgdata = root.path().join("tmp/pglite/base");
    std::fs::write(
        pgdata.join("postmaster.pid"),
        b"stale pid from interrupted run",
    )?;
    std::fs::write(
        pgdata.join("postmaster.opts"),
        b"stale opts from interrupted run",
    )?;

    let mut reopened = Pglite::builder()
        .path(root.path())
        .template_cache(false)
        .open()?;
    let result = reopened.query("SELECT value FROM fresh_initdb", &[], None)?;
    assert_eq!(
        first_row(&result)?.get("value"),
        Some(&json!("boot-single-ok"))
    );
    assert_file_missing_or_without(&pgdata.join("postmaster.pid"), "stale pid")?;
    assert_file_missing_or_without(&pgdata.join("postmaster.opts"), "stale opts")?;
    reopened.close()?;
    Ok(())
}

#[test]
fn persistent_fresh_initdb_recovers_interrupted_pgdata_without_marker() -> anyhow::Result<()> {
    let root = tempfile::TempDir::new()?;
    let pgdata = root.path().join("tmp/pglite/base");
    std::fs::create_dir_all(&pgdata)?;
    std::fs::write(pgdata.join("postmaster.pid"), b"interrupted pid")?;
    std::fs::write(pgdata.join("partial-bootstrap.sql"), b"interrupted initdb")?;

    let mut pg = Pglite::builder()
        .path(root.path())
        .template_cache(false)
        .open()?;
    let result = pg.query("SELECT 1::int AS one", &[], None)?;
    assert_eq!(first_row(&result)?.get("one"), Some(&json!(1)));
    assert!(pgdata.join("PG_VERSION").exists());
    assert!(!pgdata.join("partial-bootstrap.sql").exists());
    assert_file_missing_or_without(&pgdata.join("postmaster.pid"), "interrupted pid")?;
    pg.close()?;
    Ok(())
}

#[test]
fn persistent_fresh_initdb_recovers_interrupted_pgdata_with_incomplete_markers()
-> anyhow::Result<()> {
    let root = tempfile::TempDir::new()?;
    let pgdata = root.path().join("tmp/pglite/base");
    std::fs::create_dir_all(&pgdata)?;
    std::fs::write(pgdata.join("PG_VERSION"), b"17\n")?;
    std::fs::write(pgdata.join("partial-bootstrap.sql"), b"interrupted initdb")?;

    let mut pg = Pglite::builder()
        .path(root.path())
        .template_cache(false)
        .open()?;
    let result = pg.query("SELECT 2::int AS two", &[], None)?;
    assert_eq!(first_row(&result)?.get("two"), Some(&json!(2)));
    assert!(pgdata.join("PG_VERSION").exists());
    assert!(pgdata.join("global/pg_control").exists());
    assert!(!pgdata.join("partial-bootstrap.sql").exists());
    pg.close()?;
    Ok(())
}

#[test]
fn persistent_root_lock_rejects_second_direct_open() -> anyhow::Result<()> {
    let root = tempfile::TempDir::new()?;
    let mut first = Pglite::builder().path(root.path()).open()?;
    let err = match Pglite::builder().path(root.path()).open() {
        Ok(_) => anyhow::bail!("second open must fail while the root lock is held"),
        Err(err) => err,
    };
    assert!(format!("{err:#}").contains("PGlite root is already in use"));

    first.close()?;

    let mut reopened = Pglite::builder().path(root.path()).open()?;
    let result = reopened.query("SELECT 1::int AS one", &[], None)?;
    assert_eq!(first_row(&result)?.get("one"), Some(&json!(1)));
    reopened.close()?;
    Ok(())
}

#[test]
fn persistent_root_lock_rejects_second_server_open() -> anyhow::Result<()> {
    let root = tempfile::TempDir::new()?;
    let server = PgliteServer::builder().path(root.path()).start()?;
    let err = match PgliteServer::builder().path(root.path()).start() {
        Ok(_) => anyhow::bail!("second server must fail while the root lock is held"),
        Err(err) => err,
    };
    assert!(format!("{err:#}").contains("PGlite root is already in use"));
    server.shutdown()?;
    Ok(())
}

#[test]
fn persistent_root_lock_rejects_direct_open_while_server_runs() -> anyhow::Result<()> {
    let root = tempfile::TempDir::new()?;
    let server = PgliteServer::builder().path(root.path()).start()?;
    let err = match Pglite::builder().path(root.path()).open() {
        Ok(_) => anyhow::bail!("direct open must fail while the server owns the root lock"),
        Err(err) => err,
    };
    assert!(format!("{err:#}").contains("PGlite root is already in use"));
    server.shutdown()?;

    let mut reopened = Pglite::builder().path(root.path()).open()?;
    let result = reopened.query("SELECT 1::int AS one", &[], None)?;
    assert_eq!(first_row(&result)?.get("one"), Some(&json!(1)));
    reopened.close()?;
    Ok(())
}

#[test]
fn persistent_root_lock_rejects_cross_process_open() -> anyhow::Result<()> {
    let root = tempfile::TempDir::new()?;
    let proxy_bin = std::env::var("CARGO_BIN_EXE_pglite-proxy")
        .context("CARGO_BIN_EXE_pglite-proxy should be set by cargo test")?;
    let mut child = Command::new(proxy_bin)
        .arg("--root")
        .arg(root.path())
        .args(["--tcp", "127.0.0.1:0", "--print-uri"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("missing pglite-proxy stdout"))?;
    let mut line = String::new();
    let read = BufReader::new(stdout).read_line(&mut line)?;
    if read == 0 {
        let status = child.wait()?;
        anyhow::bail!("pglite-proxy exited before printing URI: {status}");
    }
    assert!(line.starts_with("postgresql://"), "{line:?}");

    let err = match Pglite::builder().path(root.path()).open() {
        Ok(_) => anyhow::bail!("direct open must fail while another process owns the root lock"),
        Err(err) => err,
    };
    assert!(format!("{err:#}").contains("PGlite root is already in use"));

    child.kill().ok();
    child.wait().ok();
    Ok(())
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

    let identity = pg.query(
        "SELECT current_user AS current_user, \
                session_user AS session_user, \
                current_database() AS database_name, \
                current_setting('TimeZone') AS timezone",
        &[],
        None,
    )?;
    let identity_row = first_row(&identity)?;
    assert_eq!(identity_row.get("current_user"), Some(&json!("postgres")));
    assert_eq!(identity_row.get("session_user"), Some(&json!("postgres")));
    assert_eq!(identity_row.get("database_name"), Some(&json!("template1")));
    assert_eq!(identity_row.get("timezone"), Some(&json!("UTC")));

    pg.exec("SET TIME ZONE 'UTC'", None)?;
    let timezone_catalog = pg.query(
        "SELECT count(*)::int AS ny_zones, \
                EXTRACT(HOUR FROM TIMESTAMPTZ '2024-07-01 12:00:00+00' \
                    AT TIME ZONE 'America/New_York')::int AS ny_summer_hour, \
                EXTRACT(HOUR FROM TIMESTAMPTZ '2024-01-01 12:00:00+00' \
                    AT TIME ZONE 'America/New_York')::int AS ny_winter_hour \
         FROM pg_timezone_names \
         WHERE name = 'America/New_York'",
        &[],
        None,
    )?;
    let timezone_row = first_row(&timezone_catalog)?;
    assert_eq!(timezone_row.get("ny_zones"), Some(&json!(1)));
    assert_eq!(timezone_row.get("ny_summer_hour"), Some(&json!(8)));
    assert_eq!(timezone_row.get("ny_winter_hour"), Some(&json!(7)));

    pg.exec("SET TIME ZONE 'Missing/Zone'", None)
        .expect_err("invalid timezone should fail");
    let after_timezone_error = pg.query("SELECT 25::int AS recovered", &[], None)?;
    assert_eq!(
        first_row(&after_timezone_error)?.get("recovered"),
        Some(&json!(25))
    );
    pg.exec("SET TIME ZONE 'UTC'", None)?;

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

    let syntax_err = pg
        .exec("SELECT +", None)
        .expect_err("syntax error should fail");
    let syntax_pg_err = syntax_err
        .downcast_ref::<PgliteError>()
        .expect("syntax error should preserve Postgres error fields");
    assert_eq!(syntax_pg_err.query(), "SELECT +");
    assert_eq!(
        syntax_pg_err.database_error().code.as_deref(),
        Some("42601")
    );

    let missing_err = pg
        .query(
            "SELECT * FROM missing_table WHERE id = $1",
            &[json!(7)],
            None,
        )
        .expect_err("missing table should fail");
    let missing_pg_err = missing_err
        .downcast_ref::<PgliteError>()
        .expect("extended query error should preserve Postgres error fields");
    assert_eq!(
        missing_pg_err.query(),
        "SELECT * FROM missing_table WHERE id = $1"
    );
    assert_eq!(missing_pg_err.params(), &[json!(7)]);
    assert_eq!(
        missing_pg_err.database_error().code.as_deref(),
        Some("42P01")
    );

    let invalid_bind = pg
        .query("SELECT $1::int4 AS value", &[json!("not_an_int")], None)
        .expect_err("invalid typed parameter should fail during extended-query bind");
    let invalid_bind_pg_err = invalid_bind
        .downcast_ref::<PgliteError>()
        .expect("bind error should preserve Postgres error fields");
    assert_eq!(invalid_bind_pg_err.query(), "SELECT $1::int4 AS value");
    assert_eq!(invalid_bind_pg_err.params(), &[json!("not_an_int")]);
    assert_eq!(
        invalid_bind_pg_err.database_error().code.as_deref(),
        Some("22P02")
    );

    let wrong_param_count = pg
        .query("SELECT $1::int4 + $2::int4 AS value", &[json!(1)], None)
        .expect_err("missing parameter should fail during extended-query bind");
    let wrong_param_count_pg_err = wrong_param_count
        .downcast_ref::<PgliteError>()
        .expect("parameter count error should preserve Postgres error fields");
    assert_eq!(
        wrong_param_count_pg_err.database_error().code.as_deref(),
        Some("08P01")
    );

    let after_error = pg.query("SELECT 99::int AS recovered", &[], None)?;
    assert_eq!(first_row(&after_error)?.get("recovered"), Some(&json!(99)));

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
