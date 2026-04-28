#![cfg(feature = "extensions")]

use anyhow::Result;
use pglite_oxide::{Pglite, PgliteError, PgliteServer, extensions};
use serde_json::json;
use sqlx::{Connection, Row};

fn first_f64(result: &pglite_oxide::Results, column: &str) -> f64 {
    result.rows[0][column].as_f64().expect("floating result")
}

fn assert_pglite_code(err: &anyhow::Error, expected_code: &str, message_contains: &str) {
    let pg_err = err
        .downcast_ref::<PgliteError>()
        .expect("error should preserve Postgres fields");
    assert_eq!(pg_err.database_error().code.as_deref(), Some(expected_code));
    assert!(
        pg_err.database_error().message.contains(message_contains),
        "expected error message to contain {message_contains:?}, got {:?}",
        pg_err.database_error().message
    );
}

fn assert_sqlx_code(err: &sqlx::Error, expected_code: &str) {
    assert_eq!(
        err.as_database_error().and_then(|db| db.code()).as_deref(),
        Some(expected_code)
    );
}

#[test]
fn vector_extension_direct_smoke() -> Result<()> {
    let mut db = Pglite::builder()
        .temporary()
        .extension(extensions::VECTOR)
        .open()?;

    db.exec("CREATE TEMP TABLE oxide_vec (embedding vector(3))", None)?;
    db.exec("INSERT INTO oxide_vec VALUES ('[1,2,3]')", None)?;
    let result = db.query(
        "SELECT embedding <-> '[1,2,4]'::vector AS distance FROM oxide_vec",
        &[],
        None,
    )?;
    assert_eq!(first_f64(&result, "distance"), 1.0);

    let version = db.query(
        "SELECT extversion, n.nspname AS schema_name \
         FROM pg_extension e \
         JOIN pg_namespace n ON n.oid = e.extnamespace \
         WHERE e.extname = 'vector'",
        &[],
        None,
    )?;
    let extversion = version.rows[0]["extversion"]
        .as_str()
        .expect("vector extversion");
    assert!(!extversion.is_empty());
    assert_eq!(version.rows[0]["schema_name"], json!("pg_catalog"));

    let err = db
        .query(
            "SELECT 10 / $1::int4 AS impossible_after_vector",
            &[serde_json::json!(0)],
            None,
        )
        .expect_err("division by zero after vector load should fail");
    assert_pglite_code(&err, "22012", "division by zero");
    let recovered = db.query("SELECT 13::int AS recovered_after_vector_error", &[], None)?;
    assert_eq!(recovered.rows[0]["recovered_after_vector_error"], json!(13));

    let invalid_vector = db
        .query(
            "SELECT $1::vector AS embedding",
            &[json!("[hello,1]")],
            None,
        )
        .expect_err("invalid vector literal should fail inside the vector extension");
    assert_pglite_code(
        &invalid_vector,
        "22P02",
        "invalid input syntax for type vector",
    );
    let recovered = db.query(
        "SELECT 15::int AS recovered_after_invalid_vector",
        &[],
        None,
    )?;
    assert_eq!(
        recovered.rows[0]["recovered_after_invalid_vector"],
        json!(15)
    );

    let dimension_mismatch = db
        .query(
            "SELECT $1::vector <-> $2::vector AS distance",
            &[json!("[1,2]"), json!("[3]")],
            None,
        )
        .expect_err("vector distance should reject mismatched dimensions");
    assert_pglite_code(&dimension_mismatch, "22000", "different vector dimensions");
    let recovered = db.query(
        "SELECT 16::int AS recovered_after_dimension_mismatch",
        &[],
        None,
    )?;
    assert_eq!(
        recovered.rows[0]["recovered_after_dimension_mismatch"],
        json!(16)
    );

    db.close()?;
    Ok(())
}

#[test]
fn vector_extension_ports_pgvector_core_type_cases() -> Result<()> {
    let mut db = Pglite::builder()
        .temporary()
        .extension(extensions::VECTOR)
        .open()?;

    let valid = db.query(
        "SELECT \
            '[1,2,3]'::vector::text AS vector_text, \
            vector_dims('[1,2,3]'::vector)::int AS dims, \
            l2_distance('[0,0]'::vector, '[3,4]'::vector)::float8 AS distance",
        &[],
        None,
    )?;
    assert_eq!(valid.rows[0]["vector_text"], json!("[1,2,3]"));
    assert_eq!(valid.rows[0]["dims"], json!(3));
    assert_eq!(first_f64(&valid, "distance"), 5.0);

    for (sql, code, message) in [
        (
            "SELECT '[hello,1]'::vector",
            "22P02",
            "invalid input syntax for type vector",
        ),
        ("SELECT '[NaN,1]'::vector", "22000", "NaN not allowed"),
        (
            "SELECT '[1,2,3]'::vector(2)",
            "22000",
            "expected 2 dimensions, not 3",
        ),
        (
            "SELECT '[1,2]'::vector <-> '[3]'::vector",
            "22000",
            "different vector dimensions",
        ),
    ] {
        let err = match db.query(sql, &[], None) {
            Ok(_) => panic!("{sql} should fail"),
            Err(err) => err,
        };
        assert_pglite_code(&err, code, message);
        let recovered = db.query("SELECT 17::int AS recovered", &[], None)?;
        assert_eq!(recovered.rows[0]["recovered"], json!(17));
    }

    db.close()?;
    Ok(())
}

#[test]
fn vector_extension_install_is_demand_driven_idempotent_and_persistent() -> Result<()> {
    let root = tempfile::TempDir::new()?;
    {
        let mut db = Pglite::builder().path(root.path()).open()?;
        assert!(
            !db.paths()
                .pgroot
                .join("pglite")
                .join("lib/postgresql/vector.so")
                .exists(),
            "vector side module should not be installed before it is requested"
        );

        db.enable_extension(extensions::VECTOR)?;
        db.enable_extension(extensions::VECTOR)?;
        assert!(
            db.paths()
                .pgroot
                .join("pglite")
                .join("lib/postgresql/vector.so")
                .exists(),
            "vector side module should be installed after enable_extension"
        );

        let installed = db.query(
            "SELECT count(*)::int AS count FROM pg_extension WHERE extname = 'vector'",
            &[],
            None,
        )?;
        assert_eq!(installed.rows[0]["count"], json!(1));
        db.close()?;
    }

    {
        let mut reopened = Pglite::builder().path(root.path()).open()?;
        let result = reopened.query("SELECT '[1,2,3]'::vector::text AS value", &[], None)?;
        assert_eq!(result.rows[0]["value"], json!("[1,2,3]"));
        reopened.close()?;
    }

    Ok(())
}

#[test]
fn pg_trgm_extension_direct_smoke() -> Result<()> {
    let mut db = Pglite::builder()
        .temporary()
        .extension(extensions::PG_TRGM)
        .open()?;

    let result = db.query(
        "SELECT similarity('postgres', 'postgrex') AS score",
        &[],
        None,
    )?;
    assert!(first_f64(&result, "score") > 0.5);

    let installed = db.query(
        "SELECT count(*)::int AS count, max(n.nspname) AS schema_name \
         FROM pg_extension e \
         JOIN pg_namespace n ON n.oid = e.extnamespace \
         WHERE e.extname = 'pg_trgm'",
        &[],
        None,
    )?;
    assert_eq!(installed.rows[0]["count"], json!(1));
    assert_eq!(installed.rows[0]["schema_name"], json!("pg_catalog"));

    db.close()?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_trgm_extension_server_sqlx_smoke() -> Result<()> {
    let server = PgliteServer::builder()
        .temporary()
        .extension(extensions::PG_TRGM)
        .start()?;
    let mut conn = sqlx::PgConnection::connect(&server.connection_uri()).await?;

    let row = sqlx::query("SELECT similarity('postgres', 'postgrex')::float8 AS score")
        .fetch_one(&mut conn)
        .await?;
    assert!(row.try_get::<f64, _>("score")? > 0.5);

    let row =
        sqlx::query("SELECT count(*)::int4 AS count FROM pg_extension WHERE extname = 'pg_trgm'")
            .fetch_one(&mut conn)
            .await?;
    assert_eq!(row.try_get::<i32, _>("count")?, 1);

    conn.close().await?;
    server.shutdown()?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn vector_extension_server_sqlx_smoke() -> Result<()> {
    let server = PgliteServer::builder()
        .temporary()
        .extension(extensions::VECTOR)
        .start()?;
    let mut conn = sqlx::PgConnection::connect(&server.connection_uri()).await?;

    sqlx::query("CREATE TABLE oxide_vec_server (embedding vector(3))")
        .execute(&mut conn)
        .await?;
    sqlx::query("INSERT INTO oxide_vec_server VALUES ('[1,2,3]')")
        .execute(&mut conn)
        .await?;
    let row =
        sqlx::query("SELECT embedding <-> '[1,2,4]'::vector AS distance FROM oxide_vec_server")
            .fetch_one(&mut conn)
            .await?;

    assert_eq!(row.try_get::<f64, _>("distance")?, 1.0);

    let err = sqlx::query("SELECT 10 / $1::int4 AS impossible_after_vector")
        .bind(0_i32)
        .fetch_one(&mut conn)
        .await
        .expect_err("division by zero after vector load should fail");
    assert_sqlx_code(&err, "22012");
    let row = sqlx::query("SELECT 14::int4 AS recovered_after_vector_error")
        .fetch_one(&mut conn)
        .await?;
    assert_eq!(row.try_get::<i32, _>("recovered_after_vector_error")?, 14);

    let err = sqlx::query("SELECT $1::text::vector AS embedding")
        .bind("[hello,1]")
        .fetch_one(&mut conn)
        .await
        .expect_err("invalid vector input through SQLx should fail in the vector extension");
    assert_sqlx_code(&err, "22P02");
    let row = sqlx::query("SELECT 18::int4 AS recovered_after_invalid_vector")
        .fetch_one(&mut conn)
        .await?;
    assert_eq!(row.try_get::<i32, _>("recovered_after_invalid_vector")?, 18);

    let err = sqlx::query("SELECT $1::text::vector <-> $2::text::vector AS distance")
        .bind("[1,2]")
        .bind("[3]")
        .fetch_one(&mut conn)
        .await
        .expect_err("vector distance should reject mismatched dimensions through SQLx");
    assert_sqlx_code(&err, "22000");
    let row = sqlx::query("SELECT 19::int4 AS recovered_after_dimension_mismatch")
        .fetch_one(&mut conn)
        .await?;
    assert_eq!(
        row.try_get::<i32, _>("recovered_after_dimension_mismatch")?,
        19
    );

    conn.close().await?;
    server.shutdown()?;
    Ok(())
}
