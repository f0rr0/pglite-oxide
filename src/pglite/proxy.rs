use anyhow::{Context, Result, anyhow, bail};
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, ToSocketAddrs};
#[cfg(unix)]
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc::SyncSender,
};
use std::thread;
use std::time::Duration;

use crate::pglite::base::install_into;
use crate::pglite::postgres_mod::PostgresMod;
use crate::pglite::transport::Transport;

const SSL_REQUEST_CODE: i32 = 80_877_103;
const GSSENC_REQUEST_CODE: i32 = 80_877_104;
const CANCEL_REQUEST_CODE: i32 = 80_877_102;
const PROTOCOL_3: i32 = 196_608;
const MAX_FRONTEND_MESSAGE: usize = 64 * 1024 * 1024;

/// Blocking PostgreSQL socket proxy for the embedded PGlite runtime.
///
/// The proxy intentionally runs each accepted connection on one blocking thread
/// and does not call Wasmtime from an async runtime. That avoids the nested
/// runtime panic that can happen when an async wrapper blocks inside Wasmtime.
#[derive(Debug, Clone)]
pub struct PgliteProxy {
    root: Arc<PathBuf>,
}

impl PgliteProxy {
    /// Create a proxy that stores the PGlite runtime and cluster under `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: Arc::new(root.into()),
        }
    }

    /// Return the root directory used for runtime installation and cluster data.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Serve a TCP listener forever. Connections are handled one at a time.
    pub fn serve_tcp<A>(&self, addr: A) -> Result<()>
    where
        A: ToSocketAddrs,
    {
        let listener = TcpListener::bind(addr).context("bind TCP proxy listener")?;
        self.serve_tcp_listener(listener)
    }

    /// Serve an existing TCP listener forever. Connections are handled one at a time.
    pub fn serve_tcp_listener(&self, listener: TcpListener) -> Result<()> {
        let mut backend = WireBackend::open(&self.root)?;
        for stream in listener.incoming() {
            let stream = stream.context("accept TCP proxy connection")?;
            self.handle_stream(stream, &mut backend)?;
        }
        Ok(())
    }

    pub(crate) fn serve_tcp_listener_until_ready(
        &self,
        listener: TcpListener,
        shutdown: Arc<AtomicBool>,
        ready: Option<SyncSender<Result<()>>>,
    ) -> Result<()> {
        listener
            .set_nonblocking(true)
            .context("configure TCP proxy listener as nonblocking")?;

        let mut backend = match WireBackend::open(&self.root) {
            Ok(backend) => {
                if let Some(ready) = ready {
                    let _ = ready.send(Ok(()));
                }
                backend
            }
            Err(err) => {
                let message = format!("{err:#}");
                if let Some(ready) = ready {
                    let _ = ready.send(Err(anyhow!(message.clone())));
                }
                return Err(anyhow!(message));
            }
        };
        while !shutdown.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((stream, _)) => {
                    stream
                        .set_nonblocking(false)
                        .context("configure TCP proxy stream as blocking")?;
                    self.handle_stream(stream, &mut backend)?;
                }
                Err(err) if err.kind() == ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(err) => return Err(err).context("accept TCP proxy connection"),
            }
        }

        Ok(())
    }

    /// Accept and handle one TCP connection. Intended for tests and supervised embedding.
    pub fn accept_tcp_once(&self, listener: &TcpListener) -> Result<()> {
        self.accept_tcp_connections(listener, 1)
    }

    /// Accept and handle `count` TCP connections using one embedded backend.
    pub fn accept_tcp_connections(&self, listener: &TcpListener, count: usize) -> Result<()> {
        let mut backend = WireBackend::open(&self.root)?;
        for _ in 0..count {
            let (stream, _) = listener.accept().context("accept TCP proxy connection")?;
            self.handle_stream(stream, &mut backend)?;
        }
        Ok(())
    }

    /// Serve a Unix-domain socket forever. Connections are handled one at a time.
    #[cfg(unix)]
    pub fn serve_unix(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        if path.exists() {
            std::fs::remove_file(path)
                .with_context(|| format!("remove stale socket {}", path.display()))?;
        }
        let listener = UnixListener::bind(path)
            .with_context(|| format!("bind Unix proxy socket {}", path.display()))?;
        self.serve_unix_listener(listener)
    }

    /// Serve an existing Unix-domain listener forever. Connections are handled one at a time.
    #[cfg(unix)]
    pub fn serve_unix_listener(&self, listener: UnixListener) -> Result<()> {
        let mut backend = WireBackend::open(&self.root)?;
        for stream in listener.incoming() {
            let stream = stream.context("accept Unix proxy connection")?;
            self.handle_stream(stream, &mut backend)?;
        }
        Ok(())
    }

    #[cfg(unix)]
    pub(crate) fn serve_unix_listener_until_ready(
        &self,
        listener: UnixListener,
        shutdown: Arc<AtomicBool>,
        ready: Option<SyncSender<Result<()>>>,
    ) -> Result<()> {
        listener
            .set_nonblocking(true)
            .context("configure Unix proxy listener as nonblocking")?;

        let mut backend = match WireBackend::open(&self.root) {
            Ok(backend) => {
                if let Some(ready) = ready {
                    let _ = ready.send(Ok(()));
                }
                backend
            }
            Err(err) => {
                let message = format!("{err:#}");
                if let Some(ready) = ready {
                    let _ = ready.send(Err(anyhow!(message.clone())));
                }
                return Err(anyhow!(message));
            }
        };
        while !shutdown.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((stream, _)) => {
                    stream
                        .set_nonblocking(false)
                        .context("configure Unix proxy stream as blocking")?;
                    self.handle_stream(stream, &mut backend)?;
                }
                Err(err) if err.kind() == ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(err) => return Err(err).context("accept Unix proxy connection"),
            }
        }

        Ok(())
    }

    /// Accept and handle one Unix-domain socket connection.
    #[cfg(unix)]
    pub fn accept_unix_once(&self, listener: &UnixListener) -> Result<()> {
        self.accept_unix_connections(listener, 1)
    }

    /// Accept and handle `count` Unix-domain socket connections using one embedded backend.
    #[cfg(unix)]
    pub fn accept_unix_connections(&self, listener: &UnixListener, count: usize) -> Result<()> {
        let mut backend = WireBackend::open(&self.root)?;
        for _ in 0..count {
            let (stream, _) = listener.accept().context("accept Unix proxy connection")?;
            self.handle_stream(stream, &mut backend)?;
        }
        Ok(())
    }

    fn handle_stream<S>(&self, mut stream: S, backend: &mut WireBackend) -> Result<()>
    where
        S: Read + Write,
    {
        let mut reader = FrontendMessageReader::default();
        let mut buffer = [0u8; 64 * 1024];
        let mut protocol_batch = Vec::new();

        loop {
            let read = stream.read(&mut buffer).context("read frontend socket")?;
            if read == 0 {
                flush_protocol_batch(&mut protocol_batch, backend, &mut stream)?;
                break;
            }

            let mut close_after_flush = false;
            let messages = reader.push(&buffer[..read])?;
            for message in messages {
                match classify_frontend_message(&message)? {
                    FrontendMessageKind::SslOrGssRequest => {
                        flush_protocol_batch(&mut protocol_batch, backend, &mut stream)?;
                        stream.write_all(b"N").context("write SSL refusal")?;
                    }
                    FrontendMessageKind::CancelRequest => {
                        flush_protocol_batch(&mut protocol_batch, backend, &mut stream)?;
                        close_after_flush = true;
                    }
                    FrontendMessageKind::Terminate => {
                        flush_protocol_batch(&mut protocol_batch, backend, &mut stream)?;
                        close_after_flush = true;
                    }
                    FrontendMessageKind::Startup => {
                        flush_protocol_batch(&mut protocol_batch, backend, &mut stream)?;
                        stream
                            .write_all(&startup_response())
                            .context("write startup response")?;
                    }
                    FrontendMessageKind::Protocol => {
                        let flush_after = should_flush_protocol_batch(&message);
                        protocol_batch.extend_from_slice(&message);
                        if flush_after {
                            flush_protocol_batch(&mut protocol_batch, backend, &mut stream)?;
                        }
                    }
                }
            }
            stream.flush().context("flush frontend socket")?;
            if close_after_flush {
                break;
            }
        }

        backend.rollback_connection_state();
        Ok(())
    }
}

struct WireBackend {
    pg: PostgresMod,
    transport: Transport,
}

impl WireBackend {
    fn open(root: &Path) -> Result<Self> {
        let outcome = install_into(root)?;
        let mut pg = PostgresMod::new(outcome.paths)?;
        pg.ensure_cluster()?;
        let transport = Transport::prepare(&mut pg)?;
        Ok(Self { pg, transport })
    }

    fn send(&mut self, message: &[u8]) -> Result<Vec<u8>> {
        self.transport.send(&mut self.pg, message, None)
    }

    fn rollback_connection_state(&mut self) {
        let _ = self.send(&simple_query_message("ROLLBACK"));
    }
}

#[derive(Default)]
struct FrontendMessageReader {
    buffer: Vec<u8>,
}

impl FrontendMessageReader {
    fn push(&mut self, input: &[u8]) -> Result<Vec<Vec<u8>>> {
        self.buffer.extend_from_slice(input);
        let mut messages = Vec::new();

        loop {
            let Some(message_len) = frontend_message_len(&self.buffer)? else {
                break;
            };
            let message = self.buffer.drain(..message_len).collect();
            messages.push(message);
        }

        Ok(messages)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FrontendMessageKind {
    Protocol,
    Startup,
    SslOrGssRequest,
    CancelRequest,
    Terminate,
}

fn frontend_message_len(buffer: &[u8]) -> Result<Option<usize>> {
    if buffer.len() < 4 {
        return Ok(None);
    }

    if buffer[0] == 0 {
        let len = i32::from_be_bytes(buffer[0..4].try_into().unwrap());
        if len < 8 {
            bail!("invalid startup packet length {len}");
        }
        let len = len as usize;
        if len > MAX_FRONTEND_MESSAGE {
            bail!("startup packet length {len} exceeds limit");
        }
        return Ok((buffer.len() >= len).then_some(len));
    }

    if buffer.len() < 5 {
        return Ok(None);
    }
    let len = i32::from_be_bytes(buffer[1..5].try_into().unwrap());
    if len < 4 {
        bail!("invalid frontend message length {len}");
    }
    let total = 1usize
        .checked_add(len as usize)
        .ok_or_else(|| anyhow!("frontend message length overflow"))?;
    if total > MAX_FRONTEND_MESSAGE {
        bail!("frontend message length {total} exceeds limit");
    }
    Ok((buffer.len() >= total).then_some(total))
}

fn classify_frontend_message(message: &[u8]) -> Result<FrontendMessageKind> {
    if message.is_empty() {
        bail!("empty frontend message");
    }

    if message[0] == 0 {
        if message.len() < 8 {
            bail!("startup/control packet is too short");
        }
        let code = i32::from_be_bytes(message[4..8].try_into().unwrap());
        return Ok(match code {
            SSL_REQUEST_CODE | GSSENC_REQUEST_CODE => FrontendMessageKind::SslOrGssRequest,
            CANCEL_REQUEST_CODE => FrontendMessageKind::CancelRequest,
            PROTOCOL_3 => FrontendMessageKind::Startup,
            other => bail!("unsupported startup/control packet code {other}"),
        });
    }

    if message[0] == b'X' {
        return Ok(FrontendMessageKind::Terminate);
    }

    Ok(FrontendMessageKind::Protocol)
}

fn should_flush_protocol_batch(message: &[u8]) -> bool {
    matches!(message.first(), Some(b'Q' | b'S' | b'H'))
}

fn flush_protocol_batch<S>(
    protocol_batch: &mut Vec<u8>,
    backend: &mut WireBackend,
    stream: &mut S,
) -> Result<()>
where
    S: Write,
{
    if protocol_batch.is_empty() {
        return Ok(());
    }

    let response = backend.send(protocol_batch)?;
    protocol_batch.clear();
    if !response.is_empty() {
        stream
            .write_all(&response)
            .context("write backend response")?;
    }

    Ok(())
}

fn startup_response() -> Vec<u8> {
    let mut response = Vec::new();
    push_authentication_ok(&mut response);
    push_parameter_status(&mut response, "server_version", "17.5");
    push_parameter_status(&mut response, "server_encoding", "UTF8");
    push_parameter_status(&mut response, "client_encoding", "UTF8");
    push_parameter_status(&mut response, "DateStyle", "ISO, MDY");
    push_parameter_status(&mut response, "integer_datetimes", "on");
    push_backend_key_data(&mut response, 0, 0);
    push_ready_for_query(&mut response, b'I');
    response
}

fn push_authentication_ok(out: &mut Vec<u8>) {
    out.push(b'R');
    out.extend_from_slice(&8_i32.to_be_bytes());
    out.extend_from_slice(&0_i32.to_be_bytes());
}

fn push_parameter_status(out: &mut Vec<u8>, key: &str, value: &str) {
    out.push(b'S');
    let len = 4 + key.len() + 1 + value.len() + 1;
    out.extend_from_slice(&(len as i32).to_be_bytes());
    out.extend_from_slice(key.as_bytes());
    out.push(0);
    out.extend_from_slice(value.as_bytes());
    out.push(0);
}

fn push_backend_key_data(out: &mut Vec<u8>, process_id: i32, secret_key: i32) {
    out.push(b'K');
    out.extend_from_slice(&12_i32.to_be_bytes());
    out.extend_from_slice(&process_id.to_be_bytes());
    out.extend_from_slice(&secret_key.to_be_bytes());
}

fn push_ready_for_query(out: &mut Vec<u8>, status: u8) {
    out.push(b'Z');
    out.extend_from_slice(&5_i32.to_be_bytes());
    out.push(status);
}

fn simple_query_message(sql: &str) -> Vec<u8> {
    let mut message = Vec::with_capacity(sql.len() + 6);
    message.push(b'Q');
    message.extend_from_slice(&((sql.len() + 5) as i32).to_be_bytes());
    message.extend_from_slice(sql.as_bytes());
    message.push(0);
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontend_reader_buffers_split_messages() -> Result<()> {
        let query = b"Q\0\0\0\rSELECT 1\0";
        let mut reader = FrontendMessageReader::default();
        assert!(reader.push(&query[..3])?.is_empty());
        let messages = reader.push(&query[3..])?;
        assert_eq!(messages, vec![query.to_vec()]);
        Ok(())
    }

    #[test]
    fn frontend_reader_splits_batched_messages() -> Result<()> {
        let mut batch = Vec::new();
        batch.extend_from_slice(b"Q\0\0\0\rSELECT 1\0");
        batch.extend_from_slice(b"X\0\0\0\x04");

        let mut reader = FrontendMessageReader::default();
        let messages = reader.push(&batch)?;
        assert_eq!(messages.len(), 2);
        assert_eq!(
            classify_frontend_message(&messages[0])?,
            FrontendMessageKind::Protocol
        );
        assert_eq!(
            classify_frontend_message(&messages[1])?,
            FrontendMessageKind::Terminate
        );
        Ok(())
    }

    #[test]
    fn classify_ssl_request() -> Result<()> {
        let mut message = Vec::new();
        message.extend_from_slice(&8_i32.to_be_bytes());
        message.extend_from_slice(&SSL_REQUEST_CODE.to_be_bytes());
        assert_eq!(
            classify_frontend_message(&message)?,
            FrontendMessageKind::SslOrGssRequest
        );
        Ok(())
    }

    #[test]
    fn classify_startup_request() -> Result<()> {
        let mut message = Vec::new();
        message.extend_from_slice(&8_i32.to_be_bytes());
        message.extend_from_slice(&PROTOCOL_3.to_be_bytes());
        assert_eq!(
            classify_frontend_message(&message)?,
            FrontendMessageKind::Startup
        );
        Ok(())
    }

    #[test]
    fn protocol_batch_flushes_on_client_boundaries() {
        assert!(should_flush_protocol_batch(b"Q\0\0\0\rSELECT 1\0"));
        assert!(should_flush_protocol_batch(b"S\0\0\0\x04"));
        assert!(should_flush_protocol_batch(b"H\0\0\0\x04"));
        assert!(!should_flush_protocol_batch(b"P\0\0\0\x04"));
        assert!(!should_flush_protocol_batch(b"B\0\0\0\x04"));
        assert!(!should_flush_protocol_batch(b"D\0\0\0\x04"));
        assert!(!should_flush_protocol_batch(b"E\0\0\0\x04"));
    }
}
