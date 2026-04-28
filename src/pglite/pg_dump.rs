use std::io::{Read, Seek, Write};
use std::net::SocketAddr;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context as TaskContext, Poll};

use anyhow::{Context, Result, anyhow};
use tempfile::TempDir;
use wasmer::Store;
use wasmer_types::ModuleHash;
use wasmer_wasix::runners::wasi::{RuntimeOrEngine, WasiRunner};
use wasmer_wasix::runtime::task_manager::tokio::TokioTaskManager;
use wasmer_wasix::virtual_fs::{self, AsyncRead, AsyncSeek, AsyncWrite};
use wasmer_wasix::{LocalNetworking, PluggableRuntime, VirtualFile};

use crate::pglite::aot;

pub(crate) fn dump_server_sql(
    runtime_root: &Path,
    addr: SocketAddr,
    extra_args: &[&str],
) -> Result<String> {
    let pg_dump_wasm = runtime_root.join("bin").join("pg_dump");
    let wasm = std::fs::read(&pg_dump_wasm)
        .with_context(|| format!("read WASIX pg_dump module {}", pg_dump_wasm.display()))?;
    let engine = aot::headless_engine();
    let module = aot::load_pg_dump_module(&engine)?;
    let _store = Store::new(engine.clone());

    let fs_root = TempDir::new().context("create pg_dump WASIX filesystem root")?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("create Tokio runtime for WASIX pg_dump")?;
    let (host_fs, wasix_runtime) = {
        let _runtime_guard = runtime.enter();
        let host_fs =
            virtual_fs::host_fs::FileSystem::new(tokio::runtime::Handle::current(), fs_root.path())
                .with_context(|| {
                    format!(
                        "create host filesystem rooted at {}",
                        fs_root.path().display()
                    )
                })?;
        let host_fs = Arc::new(host_fs) as Arc<dyn virtual_fs::FileSystem + Send + Sync>;
        let mut wasix_runtime = PluggableRuntime::new(Arc::new(TokioTaskManager::new(
            tokio::runtime::Handle::current(),
        )));
        wasix_runtime.set_engine(engine.clone());
        wasix_runtime.set_networking_implementation(LocalNetworking::new());
        (host_fs, wasix_runtime)
    };

    let output_path = "/host/out.sql";
    let port = addr.port().to_string();
    let host = match addr {
        SocketAddr::V4(addr) => addr.ip().to_string(),
        SocketAddr::V6(addr) => addr.ip().to_string(),
    };
    let mut args = vec![
        "-U".to_owned(),
        "postgres".to_owned(),
        "-h".to_owned(),
        host,
        "-p".to_owned(),
        port,
        "--inserts".to_owned(),
        "-j".to_owned(),
        "1".to_owned(),
        "-f".to_owned(),
        output_path.to_owned(),
    ];
    args.extend(extra_args.iter().map(|arg| (*arg).to_owned()));
    args.push("template1".to_owned());

    let stdout = Arc::new(Mutex::new(Vec::new()));
    let stderr = Arc::new(Mutex::new(Vec::new()));
    let mut runner = WasiRunner::new();
    runner
        .with_mount("/host".to_owned(), host_fs)
        .with_current_dir("/")
        .with_args(args)
        .with_envs([
            ("PGUSER", "postgres"),
            ("PGPASSWORD", "password"),
            ("PGSSLMODE", "disable"),
        ])
        .with_stdout(Box::new(CaptureFile::new(Arc::clone(&stdout))))
        .with_stderr(Box::new(CaptureFile::new(Arc::clone(&stderr))));
    runner
        .run_wasm(
            RuntimeOrEngine::Runtime(Arc::new(wasix_runtime)),
            "pg_dump",
            module,
            ModuleHash::sha256(&wasm),
        )
        .map_err(|err| {
            let stderr = String::from_utf8_lossy(&stderr.lock().expect("stderr capture poisoned"))
                .trim()
                .to_owned();
            if stderr.is_empty() {
                anyhow!(err)
            } else {
                anyhow!("{err}; pg_dump stderr: {stderr}")
            }
        })
        .context("run WASIX pg_dump")?;

    match std::fs::read_to_string(fs_root.path().join("out.sql")) {
        Ok(sql) => Ok(sql),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let stdout = stdout.lock().expect("stdout capture poisoned");
            if stdout.is_empty() {
                Err(err).with_context(|| {
                    format!(
                        "read pg_dump output {}",
                        fs_root.path().join("out.sql").display()
                    )
                })
            } else {
                String::from_utf8(stdout.clone()).context("decode pg_dump stdout as UTF-8")
            }
        }
        Err(err) => Err(err).with_context(|| {
            format!(
                "read pg_dump output {}",
                fs_root.path().join("out.sql").display()
            )
        }),
    }
}

#[derive(Debug)]
struct CaptureFile {
    buffer: Arc<Mutex<Vec<u8>>>,
}

impl CaptureFile {
    fn new(buffer: Arc<Mutex<Vec<u8>>>) -> Self {
        Self { buffer }
    }
}

impl VirtualFile for CaptureFile {
    fn last_accessed(&self) -> u64 {
        0
    }

    fn last_modified(&self) -> u64 {
        0
    }

    fn created_time(&self) -> u64 {
        0
    }

    fn size(&self) -> u64 {
        self.buffer.lock().expect("capture lock poisoned").len() as u64
    }

    fn set_len(&mut self, _new_size: u64) -> Result<(), wasmer_wasix::FsError> {
        Err(wasmer_wasix::FsError::PermissionDenied)
    }

    fn unlink(&mut self) -> Result<(), wasmer_wasix::FsError> {
        Ok(())
    }

    fn poll_read_ready(
        self: Pin<&mut Self>,
        _cx: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<usize>> {
        Poll::Ready(Ok(0))
    }

    fn poll_write_ready(
        self: Pin<&mut Self>,
        _cx: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<usize>> {
        Poll::Ready(Ok(8192))
    }
}

impl AsyncRead for CaptureFile {
    fn poll_read(
        self: Pin<&mut Self>,
        _cx: &mut TaskContext<'_>,
        _buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for CaptureFile {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Poll::Ready(self.write(buf))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

impl AsyncSeek for CaptureFile {
    fn start_seek(self: Pin<&mut Self>, _position: std::io::SeekFrom) -> std::io::Result<()> {
        Ok(())
    }

    fn poll_complete(
        self: Pin<&mut Self>,
        _cx: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<u64>> {
        Poll::Ready(Ok(0))
    }
}

impl Read for CaptureFile {
    fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
        Ok(0)
    }
}

impl Write for CaptureFile {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.buffer
            .lock()
            .expect("capture lock poisoned")
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Seek for CaptureFile {
    fn seek(&mut self, _pos: std::io::SeekFrom) -> std::io::Result<u64> {
        Ok(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pglite::Pglite;
    #[cfg(feature = "extensions")]
    use crate::pglite::extensions;
    use crate::pglite::server::PgliteServer;
    use serde_json::json;
    use sqlx::{Connection, Executor, Row};

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pg_dump_round_trip_plain_sql() -> Result<()> {
        let server = PgliteServer::temporary_tcp()?;
        let mut conn = sqlx::PgConnection::connect(&server.database_url())
            .await
            .context("connect to PGlite server")?;
        conn.execute(
            "CREATE TABLE dump_items(id INTEGER PRIMARY KEY, value TEXT);
             CREATE INDEX dump_items_value_idx ON dump_items(value);
             CREATE SEQUENCE dump_items_seq START WITH 10;
             CREATE VIEW dump_item_values AS SELECT value FROM dump_items;
             INSERT INTO dump_items(id, value) VALUES (1, 'alpha'), (2, 'beta');
             SELECT nextval('dump_items_seq');",
        )
        .await
        .context("seed pg_dump source data")?;
        drop(conn);

        let addr = server.tcp_addr().context("server should be TCP")?;
        let runtime_root = server.root().join("tmp/pglite");
        let dump_runtime_root = runtime_root.clone();
        let dump =
            tokio::task::spawn_blocking(move || dump_server_sql(&dump_runtime_root, addr, &[]))
                .await
                .context("join pg_dump task")??;

        assert!(dump.contains("PostgreSQL database dump"));
        assert!(
            dump.contains("CREATE TABLE public.dump_items"),
            "dump did not contain dump_items table DDL:\n{dump}"
        );
        assert!(dump.contains("CREATE INDEX dump_items_value_idx"));
        assert!(dump.contains("CREATE SEQUENCE public.dump_items_seq"));
        assert!(dump.contains("CREATE VIEW public.dump_item_values"));
        assert!(dump.contains("INSERT INTO"));

        let schema_runtime_root = runtime_root.clone();
        let schema_only = tokio::task::spawn_blocking(move || {
            dump_server_sql(&schema_runtime_root, addr, &["--schema-only"])
        })
        .await
        .context("join schema-only pg_dump task")??;
        assert!(schema_only.contains("CREATE TABLE public.dump_items"));
        assert!(
            !schema_only.contains("INSERT INTO public.dump_items"),
            "schema-only dump unexpectedly contained data:\n{schema_only}"
        );

        let quote_runtime_root = runtime_root.clone();
        let quoted = tokio::task::spawn_blocking(move || {
            dump_server_sql(&quote_runtime_root, addr, &["--quote-all-identifiers"])
        })
        .await
        .context("join quoted pg_dump task")??;
        assert!(quoted.contains("CREATE TABLE \"public\".\"dump_items\""));
        assert!(quoted.contains("INSERT INTO \"public\".\"dump_items\""));

        let mut usable = sqlx::PgConnection::connect(&server.database_url())
            .await
            .context("reconnect after pg_dump")?;
        let row = sqlx::query("SELECT count(*)::int4 AS count FROM public.dump_items")
            .fetch_one(&mut usable)
            .await
            .context("server should remain usable after pg_dump")?;
        assert_eq!(row.try_get::<i32, _>("count")?, 2);
        usable.close().await?;

        server.shutdown()?;

        tokio::task::spawn_blocking(move || -> Result<()> {
            let mut restored = Pglite::builder().temporary().open()?;
            restored.exec(&dump, None).context("restore pg_dump SQL")?;
            let result = restored.query(
                "SELECT value FROM public.dump_items WHERE id = $1",
                &[json!(2)],
                None,
            )?;
            let value = result
                .rows
                .first()
                .and_then(|row| row.get("value"))
                .cloned();
            assert_eq!(value, Some(json!("beta")));
            let view = restored.query(
                "SELECT count(*)::int AS count FROM public.dump_item_values",
                &[],
                None,
            )?;
            assert_eq!(view.rows[0]["count"], json!(2));
            let sequence = restored.query(
                "SELECT nextval('public.dump_items_seq')::int AS next_value",
                &[],
                None,
            )?;
            assert_eq!(sequence.rows[0]["next_value"], json!(11));
            restored.close()?;
            Ok(())
        })
        .await
        .context("join restore task")??;
        Ok(())
    }

    #[cfg(feature = "extensions")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pg_dump_round_trip_vector_extension() -> Result<()> {
        let server = PgliteServer::builder()
            .temporary()
            .extension(extensions::VECTOR)
            .start()?;
        let mut conn = sqlx::PgConnection::connect(&server.database_url())
            .await
            .context("connect to vector-enabled PGlite server")?;
        conn.execute(
            "CREATE TABLE vector_dump_items(id INTEGER PRIMARY KEY, embedding vector(3));
             INSERT INTO vector_dump_items(id, embedding) VALUES (1, '[1,2,3]');",
        )
        .await
        .context("seed vector pg_dump source data")?;
        drop(conn);

        let addr = server.tcp_addr().context("server should be TCP")?;
        let runtime_root = server.root().join("tmp/pglite");
        let dump = tokio::task::spawn_blocking(move || dump_server_sql(&runtime_root, addr, &[]))
            .await
            .context("join vector pg_dump task")??;
        server.shutdown()?;

        assert!(
            dump.contains("CREATE EXTENSION IF NOT EXISTS vector"),
            "dump did not contain vector extension DDL:\n{dump}"
        );
        assert!(dump.contains("CREATE TABLE public.vector_dump_items"));
        assert!(dump.contains("'[1,2,3]'"));

        tokio::task::spawn_blocking(move || -> Result<()> {
            let mut restored = Pglite::builder()
                .temporary()
                .extension(extensions::VECTOR)
                .open()?;
            restored
                .exec(&dump, None)
                .context("restore vector dump SQL")?;
            let result = restored.query(
                "SELECT embedding <-> '[1,2,4]'::vector AS distance \
                 FROM public.vector_dump_items WHERE id = $1",
                &[json!(1)],
                None,
            )?;
            let distance = result
                .rows
                .first()
                .and_then(|row| row.get("distance"))
                .and_then(|value| value.as_f64());
            assert_eq!(distance, Some(1.0));
            restored.close()?;
            Ok(())
        })
        .await
        .context("join vector restore task")??;
        Ok(())
    }
}
