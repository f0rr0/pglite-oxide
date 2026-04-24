use anyhow::{Context, Result};
use pglite_oxide::PgliteServer;
use sqlx::{Connection, Row};
use tokio::time::{Duration, timeout};
use tokio_postgres::NoTls;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tokio_postgres_extended_query_works() -> Result<()> {
    let server = PgliteServer::temporary_tcp()?;
    let (client, connection) = tokio_postgres::connect(&server.connection_uri(), NoTls)
        .await
        .context("connect with tokio-postgres")?;
    let connection_task = tokio::spawn(connection);

    let row = client
        .query_one("SELECT $1::int4 + 1 AS answer", &[&41_i32])
        .await
        .context("run tokio-postgres parameter query")?;
    assert_eq!(row.get::<_, i32>("answer"), 42);

    client
        .batch_execute(
            "CREATE TABLE items(value TEXT);
             INSERT INTO items(value) VALUES ('alpha');",
        )
        .await?;
    let row = client
        .query_one("SELECT value FROM items WHERE value = $1", &[&"alpha"])
        .await
        .context("run tokio-postgres table query")?;
    assert_eq!(row.get::<_, &str>(0), "alpha");

    drop(client);
    wait_for_tokio_postgres(connection_task).await?;
    server.shutdown()?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlx_query_works() -> Result<()> {
    let server = PgliteServer::temporary_tcp()?;
    let mut conn = sqlx::PgConnection::connect(&server.connection_uri())
        .await
        .context("connect with SQLx")?;

    let row = sqlx::query("SELECT $1::int4 + 1 AS answer")
        .bind(41_i32)
        .fetch_one(&mut conn)
        .await
        .context("run SQLx parameter query")?;
    assert_eq!(row.try_get::<i32, _>("answer")?, 42);

    sqlx::query("CREATE TABLE items(value TEXT)")
        .execute(&mut conn)
        .await?;
    sqlx::query("INSERT INTO items(value) VALUES ($1)")
        .bind("alpha")
        .execute(&mut conn)
        .await?;
    let row = sqlx::query("SELECT value FROM items WHERE value = $1")
        .bind("alpha")
        .fetch_one(&mut conn)
        .await?;
    assert_eq!(row.try_get::<String, _>("value")?, "alpha");

    conn.close().await?;
    server.shutdown()?;
    Ok(())
}

async fn wait_for_tokio_postgres(
    connection_task: tokio::task::JoinHandle<Result<(), tokio_postgres::Error>>,
) -> Result<()> {
    timeout(Duration::from_secs(5), connection_task).await???;
    Ok(())
}
