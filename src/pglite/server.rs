use std::net::{SocketAddr, TcpListener};
#[cfg(unix)]
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc::{Receiver, sync_channel},
};
use std::thread::{self, JoinHandle};

use anyhow::{Context, Result, anyhow};
use tempfile::TempDir;

use crate::pglite::base::{install_into, install_temporary_from_template};
use crate::pglite::proxy::PgliteProxy;

/// A supervised local PostgreSQL socket backed by one embedded PGlite runtime.
///
/// This is the compatibility entry point for code that expects a PostgreSQL URL,
/// such as `tokio-postgres`, SQLx, or tools that speak the wire protocol. The
/// server owns one embedded backend, so downstream pools should use a single
/// connection.
#[derive(Debug)]
pub struct PgliteServer {
    root: PathBuf,
    _temp_dir: Option<TempDir>,
    endpoint: ServerEndpoint,
    shutdown: Arc<AtomicBool>,
    handle: Option<JoinHandle<Result<()>>>,
}

#[derive(Debug, Clone)]
enum ServerEndpoint {
    Tcp(SocketAddr),
    #[cfg(unix)]
    Unix(PathBuf),
}

impl PgliteServer {
    /// Build a local PGlite server. The default is a cached temporary database
    /// served on `127.0.0.1:0`.
    pub fn builder() -> PgliteServerBuilder {
        PgliteServerBuilder::new()
    }

    /// Start a cached temporary database on a random local TCP port.
    pub fn temporary_tcp() -> Result<Self> {
        Self::builder().temporary().start()
    }

    /// Return the root directory used for runtime files and cluster data.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Return the bound TCP address, if this server is using TCP.
    pub fn tcp_addr(&self) -> Option<SocketAddr> {
        match self.endpoint {
            ServerEndpoint::Tcp(addr) => Some(addr),
            #[cfg(unix)]
            ServerEndpoint::Unix(_) => None,
        }
    }

    /// Return the Unix-domain socket path, if this server is using UDS.
    #[cfg(unix)]
    pub fn socket_path(&self) -> Option<&Path> {
        match &self.endpoint {
            ServerEndpoint::Tcp(_) => None,
            ServerEndpoint::Unix(path) => Some(path),
        }
    }

    /// Return a PostgreSQL connection URI for the local server.
    pub fn connection_uri(&self) -> String {
        match &self.endpoint {
            ServerEndpoint::Tcp(addr) => tcp_connection_uri(*addr),
            #[cfg(unix)]
            ServerEndpoint::Unix(path) => {
                let host = path.parent().unwrap_or_else(|| Path::new("/tmp"));
                let port = parse_unix_socket_port(path).unwrap_or(5432);
                format!(
                    "postgresql://postgres@/template1?host={}&port={}&sslmode=disable",
                    percent_encode_query_value(&host.display().to_string()),
                    port
                )
            }
        }
    }

    /// Request shutdown and wait for the listener thread to exit.
    ///
    /// Close database clients before calling this method. The current proxy owns
    /// one blocking backend connection at a time, so an open client can keep the
    /// worker thread busy until it disconnects.
    pub fn shutdown(mut self) -> Result<()> {
        self.stop()
    }

    fn stop(&mut self) -> Result<()> {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            handle
                .join()
                .map_err(|_| anyhow!("pglite server thread panicked"))??;
        }
        Ok(())
    }
}

impl Drop for PgliteServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
    }
}

/// Builder for [`PgliteServer`].
#[derive(Debug, Clone)]
pub struct PgliteServerBuilder {
    root: ServerRoot,
    endpoint: ServerEndpointConfig,
}

#[derive(Debug, Clone)]
enum ServerRoot {
    Temporary { template_cache: bool },
    Path(PathBuf),
}

#[derive(Debug, Clone)]
enum ServerEndpointConfig {
    Tcp(SocketAddr),
    #[cfg(unix)]
    Unix(PathBuf),
}

impl Default for PgliteServerBuilder {
    fn default() -> Self {
        Self {
            root: ServerRoot::Temporary {
                template_cache: true,
            },
            endpoint: ServerEndpointConfig::Tcp(SocketAddr::from(([127, 0, 0, 1], 0))),
        }
    }
}

impl PgliteServerBuilder {
    /// Create a builder. Defaults to a cached temporary database on
    /// `127.0.0.1:0`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Serve a persistent database rooted at `root`.
    pub fn path(mut self, root: impl Into<PathBuf>) -> Self {
        self.root = ServerRoot::Path(root.into());
        self
    }

    /// Serve a temporary database cloned from the process-local template cache.
    pub fn temporary(mut self) -> Self {
        self.root = ServerRoot::Temporary {
            template_cache: true,
        };
        self
    }

    /// Serve a temporary database initialized without the template cache.
    pub fn fresh_temporary(mut self) -> Self {
        self.root = ServerRoot::Temporary {
            template_cache: false,
        };
        self
    }

    /// Bind the server to a TCP address.
    pub fn tcp(mut self, addr: SocketAddr) -> Self {
        self.endpoint = ServerEndpointConfig::Tcp(addr);
        self
    }

    /// Bind the server to a Unix-domain socket path.
    #[cfg(unix)]
    pub fn unix(mut self, path: impl Into<PathBuf>) -> Self {
        self.endpoint = ServerEndpointConfig::Unix(path.into());
        self
    }

    /// Install the runtime if needed, initialize the cluster, and start serving.
    pub fn start(self) -> Result<PgliteServer> {
        let (root, temp_dir) = match self.root {
            ServerRoot::Path(root) => {
                install_into(&root)?;
                (root, None)
            }
            ServerRoot::Temporary { template_cache } => {
                if template_cache {
                    let (root, temp_dir) = prepare_cached_temporary_root()?;
                    (root, Some(temp_dir))
                } else {
                    let temp_dir = TempDir::new().context("create temporary pglite directory")?;
                    install_into(temp_dir.path())?;
                    (temp_dir.path().to_path_buf(), Some(temp_dir))
                }
            }
        };

        let shutdown = Arc::new(AtomicBool::new(false));
        let proxy = PgliteProxy::new(root.clone());

        let (endpoint, handle) = match self.endpoint {
            ServerEndpointConfig::Tcp(addr) => start_tcp(proxy, addr, shutdown.clone())?,
            #[cfg(unix)]
            ServerEndpointConfig::Unix(path) => start_unix(proxy, path, shutdown.clone())?,
        };

        Ok(PgliteServer {
            root,
            _temp_dir: temp_dir,
            endpoint,
            shutdown,
            handle: Some(handle),
        })
    }
}

fn start_tcp(
    proxy: PgliteProxy,
    addr: SocketAddr,
    shutdown: Arc<AtomicBool>,
) -> Result<(ServerEndpoint, JoinHandle<Result<()>>)> {
    let listener = TcpListener::bind(addr).context("bind PGlite TCP server")?;
    let addr = listener.local_addr().context("read PGlite TCP address")?;
    let (ready_tx, ready_rx) = sync_channel(1);
    let handle = thread::spawn(move || {
        proxy.serve_tcp_listener_until_ready(listener, shutdown, Some(ready_tx))
    });
    wait_until_ready(&ready_rx)?;
    Ok((ServerEndpoint::Tcp(addr), handle))
}

fn tcp_connection_uri(addr: SocketAddr) -> String {
    match addr {
        SocketAddr::V4(addr) => {
            format!(
                "postgresql://postgres@{}:{}/template1?sslmode=disable",
                addr.ip(),
                addr.port()
            )
        }
        SocketAddr::V6(addr) => {
            format!(
                "postgresql://postgres@[{}]:{}/template1?sslmode=disable",
                addr.ip(),
                addr.port()
            )
        }
    }
}

fn prepare_cached_temporary_root() -> Result<(PathBuf, TempDir)> {
    run_blocking("pglite-template-cache", || {
        let (temp_dir, _outcome) = install_temporary_from_template()?;
        Ok((temp_dir.path().to_path_buf(), temp_dir))
    })
}

fn run_blocking<T, F>(name: &'static str, f: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    thread::Builder::new()
        .name(name.to_string())
        .spawn(f)
        .with_context(|| format!("spawn {name} worker"))?
        .join()
        .map_err(|_| anyhow!("{name} worker panicked"))?
}

#[cfg(unix)]
fn start_unix(
    proxy: PgliteProxy,
    path: PathBuf,
    shutdown: Arc<AtomicBool>,
) -> Result<(ServerEndpoint, JoinHandle<Result<()>>)> {
    if path.exists() {
        std::fs::remove_file(&path)
            .with_context(|| format!("remove stale socket {}", path.display()))?;
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create socket directory {}", parent.display()))?;
    }

    let listener = UnixListener::bind(&path)
        .with_context(|| format!("bind PGlite Unix socket {}", path.display()))?;
    let endpoint = ServerEndpoint::Unix(path);
    let (ready_tx, ready_rx) = sync_channel(1);
    let handle = thread::spawn(move || {
        proxy.serve_unix_listener_until_ready(listener, shutdown, Some(ready_tx))
    });
    wait_until_ready(&ready_rx)?;
    Ok((endpoint, handle))
}

fn wait_until_ready(ready_rx: &Receiver<Result<()>>) -> Result<()> {
    ready_rx
        .recv()
        .context("PGlite server thread exited before reporting readiness")?
}

#[cfg(unix)]
fn parse_unix_socket_port(path: &Path) -> Option<u16> {
    let name = path.file_name()?.to_str()?;
    name.strip_prefix(".s.PGSQL.")?.parse().ok()
}

#[cfg(unix)]
fn percent_encode_query_value(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if matches!(
            byte,
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/'
        ) {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

#[cfg(all(test, unix))]
mod tests {
    use super::percent_encode_query_value;

    #[test]
    fn unix_socket_uri_host_is_query_encoded() {
        assert_eq!(
            percent_encode_query_value("/tmp/Application Support/pglite"),
            "/tmp/Application%20Support/pglite"
        );
    }
}
