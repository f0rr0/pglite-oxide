use anyhow::{Context, Result};
use sqlx::{Connection, Row};
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use tokio::time::{Duration, timeout};

struct ChildGuard(Child);

impl ChildGuard {
    fn child_mut(&mut self) -> &mut Child {
        &mut self.0
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pglite_proxy_print_uri_accepts_sqlx_connection() -> Result<()> {
    let mut child = ChildGuard(
        Command::new(env!("CARGO_BIN_EXE_pglite-proxy"))
            .args(["--temporary", "--tcp", "127.0.0.1:0", "--print-uri"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("spawn pglite-proxy")?,
    );

    let stdout = child
        .child_mut()
        .stdout
        .take()
        .context("pglite-proxy stdout pipe")?;
    let mut reader = BufReader::new(stdout);
    let mut uri = String::new();
    reader
        .read_line(&mut uri)
        .context("read pglite-proxy printed URI")?;
    let uri = uri.trim();
    assert!(
        uri.starts_with("postgresql://") || uri.starts_with("postgres://"),
        "unexpected URI: {uri}"
    );

    let mut conn = timeout(Duration::from_secs(30), sqlx::PgConnection::connect(uri))
        .await
        .context("timed out connecting to pglite-proxy")?
        .context("connect to pglite-proxy")?;
    let row = sqlx::query("SELECT $1::int4 + 1 AS answer")
        .bind(41_i32)
        .fetch_one(&mut conn)
        .await?;
    assert_eq!(row.try_get::<i32, _>("answer")?, 42);

    conn.close().await?;
    Ok(())
}
