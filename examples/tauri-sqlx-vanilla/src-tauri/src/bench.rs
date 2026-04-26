use std::fs;
use std::future::Future;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use pglite_oxide::{install_into, preload_runtime_module, PglitePaths, PgliteServer};
use serde::Serialize;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
use sqlx::{PgPool, Row};
use tokio::sync::Mutex as AsyncMutex;

const DEFAULT_ROW_COUNT: u32 = 10_000;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PhaseTiming {
    pub name: String,
    pub ms: f64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryTiming {
    pub label: String,
    pub iterations: usize,
    pub min_ms: f64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub max_ms: f64,
    pub mean_ms: f64,
    pub rows: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BenchReport {
    pub root: String,
    pub proxy_addr: String,
    pub cold_start: bool,
    pub pgdata_template: bool,
    pub row_count: u32,
    pub startup: Vec<PhaseTiming>,
    pub workload: Vec<PhaseTiming>,
    pub queries: Vec<QueryTiming>,
    pub total_ms: f64,
    pub notes: Vec<String>,
}

pub struct BenchState {
    root: PathBuf,
    inner: AsyncMutex<Option<DatabaseHarness>>,
}

impl BenchState {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            inner: AsyncMutex::new(None),
        }
    }

    pub async fn profile_queries(&self, fresh: bool, row_count: u32) -> Result<BenchReport> {
        let mut guard = self.inner.lock().await;
        if fresh && guard.is_some() {
            bail!(
                "fresh profile requires restarting the app so the existing pglite proxy can exit"
            );
        }

        if guard.is_none() {
            let harness = DatabaseHarness::start(self.root.clone(), fresh).await?;
            *guard = Some(harness);
        }

        let harness = guard
            .as_ref()
            .ok_or_else(|| anyhow!("database harness was not initialized"))?;
        harness.profile(row_count).await
    }
}

pub struct DatabaseHarness {
    root: PathBuf,
    database_url: String,
    pool: PgPool,
    _server: PgliteServer,
    cold_start: bool,
    startup: Vec<PhaseTiming>,
}

impl DatabaseHarness {
    pub async fn start(root: PathBuf, fresh: bool) -> Result<Self> {
        if fresh && root.exists() {
            fs::remove_dir_all(&root)
                .with_context(|| format!("remove profile dir {}", root.display()))?;
        }
        fs::create_dir_all(&root).with_context(|| format!("create {}", root.display()))?;

        let paths = PglitePaths::with_root(&root);
        let cold_start = !paths.pgdata.join("PG_VERSION").exists();
        let mut startup = Vec::new();

        let install_root = root.clone();
        time_blocking(
            &mut startup,
            "install runtime and pgdata template",
            move || install_into(&install_root).map(|_| ()),
        )
        .await?;

        let preload_paths = paths.clone();
        time_blocking(&mut startup, "load/compile wasmtime module", move || {
            preload_runtime_module(&preload_paths)
        })
        .await?;

        let server_root = root.clone();
        let server = time_blocking(&mut startup, "start pglite server", move || {
            preferred_server(server_root)
        })
        .await?;
        let database_url = server.connection_uri();

        let pool = time_async(&mut startup, "sqlx pool connect", async {
            let options =
                pg_connect_options(&server)?.application_name("pglite-oxide-tauri-sqlx-profile");

            PgPoolOptions::new()
                .max_connections(1)
                .acquire_timeout(Duration::from_secs(30))
                .connect_with(options)
                .await
                .context("connect SQLx pool to pglite proxy")
        })
        .await?;

        Ok(Self {
            root,
            database_url,
            pool,
            _server: server,
            cold_start,
            startup,
        })
    }

    pub async fn profile(&self, row_count: u32) -> Result<BenchReport> {
        let total = Instant::now();
        let row_count = normalize_row_count(row_count);
        let mut workload = Vec::new();

        time_async(&mut workload, "first query", async {
            let row = sqlx::query("select 1::int as value")
                .fetch_one(&self.pool)
                .await?;
            let _: i32 = row.try_get("value")?;
            Result::<()>::Ok(())
        })
        .await?;

        time_async(&mut workload, "create table", async {
            sqlx::query("drop table if exists perf_events")
                .execute(&self.pool)
                .await?;
            sqlx::query(
                r#"
                create table perf_events (
                    id bigserial primary key,
                    bucket integer not null,
                    label text not null,
                    amount double precision not null,
                    payload text not null,
                    created_at timestamptz not null default now()
                )
                "#,
            )
            .execute(&self.pool)
            .await?;
            Result::<()>::Ok(())
        })
        .await?;

        time_async(&mut workload, "seed rows", async {
            let insert_sql = format!(
                r#"
                insert into perf_events (bucket, label, amount, payload)
                select
                    (g % 64)::integer,
                    'event-' || g::text,
                    (g::double precision * 0.125),
                    repeat(md5(g::text), 2)
                from generate_series(1, {row_count}) as g
                "#
            );
            sqlx::query(&insert_sql).execute(&self.pool).await?;
            Result::<()>::Ok(())
        })
        .await?;

        time_async(&mut workload, "create indexes", async {
            sqlx::query("create index perf_events_bucket_idx on perf_events(bucket)")
                .execute(&self.pool)
                .await?;
            sqlx::query("create index perf_events_label_idx on perf_events(label)")
                .execute(&self.pool)
                .await?;
            Result::<()>::Ok(())
        })
        .await?;

        time_async(&mut workload, "transaction insert batch", async {
            let mut tx = self.pool.begin().await?;
            for index in 0..100i32 {
                sqlx::query(
                    "insert into perf_events (bucket, label, amount, payload) values ($1, $2, $3, $4)",
                )
                .bind(index % 64)
                .bind(format!("txn-event-{index}"))
                .bind(index as f64)
                .bind(format!("txn-payload-{index}"))
                .execute(&mut *tx)
                .await?;
            }
            tx.commit().await?;
            Result::<()>::Ok(())
        })
        .await?;

        let mut queries = Vec::new();
        queries.push(
            measure_iterations("point lookup by primary key", 60, |iteration| async move {
                let id = (iteration as i64 % row_count as i64) + 1;
                let row = sqlx::query("select payload from perf_events where id = $1")
                    .bind(id)
                    .fetch_one(&self.pool)
                    .await?;
                let _: String = row.try_get("payload")?;
                Result::<i64>::Ok(1)
            })
            .await?,
        );
        queries.push(
            measure_iterations("prepared lookup loop", 60, |iteration| async move {
                let label = format!("event-{}", (iteration as u32 % row_count) + 1);
                let row = sqlx::query("select id from perf_events where label = $1")
                    .bind(label)
                    .fetch_optional(&self.pool)
                    .await?;
                Result::<i64>::Ok(if row.is_some() { 1 } else { 0 })
            })
            .await?,
        );
        queries.push(
            measure_iterations("indexed aggregate by bucket", 25, |_| async move {
                let rows = sqlx::query(
                    r#"
                    select bucket, count(*)::bigint as count, avg(amount)::double precision as avg
                    from perf_events
                    group by bucket
                    order by bucket
                    "#,
                )
                .fetch_all(&self.pool)
                .await?;
                for row in &rows {
                    let _: i32 = row.try_get("bucket")?;
                    let _: i64 = row.try_get("count")?;
                    let _: f64 = row.try_get("avg")?;
                }
                Result::<i64>::Ok(rows.len() as i64)
            })
            .await?,
        );
        queries.push(
            measure_iterations("prefix text search limit 25", 30, |iteration| async move {
                let needle = format!("event-{}%", iteration % 10);
                let rows = sqlx::query(
                    "select id, label from perf_events where label like $1 order by id limit 25",
                )
                .bind(needle)
                .fetch_all(&self.pool)
                .await?;
                for row in &rows {
                    let _: i64 = row.try_get("id")?;
                    let _: String = row.try_get("label")?;
                }
                Result::<i64>::Ok(rows.len() as i64)
            })
            .await?,
        );
        queries.push(
            measure_iterations("single-bucket update", 12, |iteration| async move {
                let bucket = (iteration % 64) as i32;
                let result =
                    sqlx::query("update perf_events set amount = amount + 1 where bucket = $1")
                        .bind(bucket)
                        .execute(&self.pool)
                        .await?;
                Result::<i64>::Ok(result.rows_affected() as i64)
            })
            .await?,
        );

        let notes = vec![
            "The Tauri window is allowed to paint before this command initializes the database."
                .to_string(),
            "Fresh starts use the bundled prepopulated PGDATA template before the backend session starts.".to_string(),
            "SQLx is configured with one PostgreSQL connection because the embedded pglite runtime is single-process."
                .to_string(),
            "The SQLx pool connection phase includes the first backend wire-protocol handshake."
                .to_string(),
            "Use the cold-start number for first launch and the query timings for steady-state UX."
                .to_string(),
        ];

        Ok(BenchReport {
            root: self.root.display().to_string(),
            proxy_addr: self.database_url.clone(),
            cold_start: self.cold_start,
            pgdata_template: true,
            row_count,
            startup: self.startup.clone(),
            workload,
            queries,
            total_ms: elapsed_ms(total),
            notes,
        })
    }
}

fn preferred_server(root: PathBuf) -> Result<PgliteServer> {
    let builder = PgliteServer::builder().path(&root);
    #[cfg(unix)]
    {
        builder.unix(root.join(".s.PGSQL.5432")).start()
    }
    #[cfg(not(unix))]
    {
        builder.start()
    }
}

fn pg_connect_options(server: &PgliteServer) -> Result<PgConnectOptions> {
    let options = PgConnectOptions::new()
        .username("postgres")
        .database("template1")
        .ssl_mode(PgSslMode::Disable);

    #[cfg(unix)]
    if let Some(path) = server.socket_path() {
        let dir = path
            .parent()
            .ok_or_else(|| anyhow!("Unix socket {} has no parent", path.display()))?;
        return Ok(options.host("localhost").socket(dir).port(5432));
    }

    let addr = server
        .tcp_addr()
        .ok_or_else(|| anyhow!("PGlite server did not expose a TCP address"))?;
    Ok(options.host(&addr.ip().to_string()).port(addr.port()))
}

fn normalize_row_count(row_count: u32) -> u32 {
    if row_count == 0 {
        return DEFAULT_ROW_COUNT;
    }
    row_count.clamp(100, 50_000)
}

async fn time_blocking<T, F>(
    timings: &mut Vec<PhaseTiming>,
    name: impl Into<String>,
    op: F,
) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    let name = name.into();
    let started = Instant::now();
    let result = tokio::task::spawn_blocking(op)
        .await
        .context("join blocking profile phase")?;
    timings.push(PhaseTiming {
        name,
        ms: elapsed_ms(started),
    });
    result
}

async fn time_async<T, Fut>(
    timings: &mut Vec<PhaseTiming>,
    name: impl Into<String>,
    fut: Fut,
) -> Result<T>
where
    Fut: Future<Output = Result<T>>,
{
    let name = name.into();
    let started = Instant::now();
    let result = fut.await;
    timings.push(PhaseTiming {
        name,
        ms: elapsed_ms(started),
    });
    result
}

async fn measure_iterations<F, Fut>(
    label: &str,
    iterations: usize,
    mut op: F,
) -> Result<QueryTiming>
where
    F: FnMut(usize) -> Fut,
    Fut: Future<Output = Result<i64>>,
{
    let mut samples = Vec::with_capacity(iterations);
    let mut rows = 0;

    for index in 0..iterations {
        let started = Instant::now();
        rows = op(index).await?;
        samples.push(elapsed_ms(started));
    }

    samples.sort_by(|left, right| left.total_cmp(right));
    let total: f64 = samples.iter().sum();
    let mean = total / samples.len() as f64;

    Ok(QueryTiming {
        label: label.to_string(),
        iterations,
        min_ms: samples[0],
        p50_ms: percentile(&samples, 0.50),
        p95_ms: percentile(&samples, 0.95),
        max_ms: *samples.last().unwrap_or(&0.0),
        mean_ms: mean,
        rows,
    })
}

fn percentile(sorted: &[f64], percentile: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let index = ((sorted.len() - 1) as f64 * percentile).round() as usize;
    sorted[index]
}

fn elapsed_ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
}
