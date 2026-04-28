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

#[cfg(feature = "extensions")]
use crate::pglite::assets;
#[cfg(feature = "extensions")]
use crate::pglite::base::install_bundled_extension_bytes;
use crate::pglite::base::install_into;
#[cfg(feature = "extensions")]
use crate::pglite::extensions::{Extension, create_extension_sql};
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
/// and does not call into the WASIX backend from an async runtime. That avoids
/// nested runtime panics when an async wrapper blocks inside the embedded engine.
#[derive(Debug, Clone)]
pub struct PgliteProxy {
    root: Arc<PathBuf>,
    #[cfg(feature = "extensions")]
    extensions: Arc<Vec<Extension>>,
}

impl PgliteProxy {
    /// Create a proxy that stores the PGlite runtime and cluster under `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: Arc::new(root.into()),
            #[cfg(feature = "extensions")]
            extensions: Arc::new(Vec::new()),
        }
    }

    /// Enable bundled extensions in the proxy backend before accepting clients.
    #[cfg(feature = "extensions")]
    pub(crate) fn with_extensions(mut self, extensions: Vec<Extension>) -> Self {
        self.extensions = Arc::new(extensions);
        self
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
        let mut backend = WireBackend::open(&self.root, self.extensions())?;
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

        let mut backend = match WireBackend::open(&self.root, self.extensions()) {
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
        let mut backend = WireBackend::open(&self.root, self.extensions())?;
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
        let mut backend = WireBackend::open(&self.root, self.extensions())?;
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

        let mut backend = match WireBackend::open(&self.root, self.extensions()) {
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
        let mut backend = WireBackend::open(&self.root, self.extensions())?;
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
                        match startup_response_for(&message)? {
                            StartupResponse::Accept(response) => stream
                                .write_all(&response)
                                .context("write startup response")?,
                            StartupResponse::Reject(response) => {
                                stream
                                    .write_all(&response)
                                    .context("write startup rejection")?;
                                close_after_flush = true;
                            }
                        }
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

    #[cfg(feature = "extensions")]
    fn extensions(&self) -> &[Extension] {
        self.extensions.as_slice()
    }

    #[cfg(not(feature = "extensions"))]
    fn extensions(&self) -> &[()] {
        &[]
    }
}

struct WireBackend {
    pg: PostgresMod,
    transport: Transport,
}

impl WireBackend {
    #[cfg(feature = "extensions")]
    fn open(root: &Path, extensions: &[Extension]) -> Result<Self> {
        let outcome = install_into(root)?;
        for extension in extensions {
            let bytes = assets::extension_archive(extension.sql_name()).ok_or_else(|| {
                anyhow!(
                    "extension asset '{}' is not bundled in this pglite-oxide build",
                    extension.sql_name()
                )
            })?;
            install_bundled_extension_bytes(&outcome.paths, extension.sql_name(), bytes)?;
        }
        let mut pg = PostgresMod::new(outcome.paths)?;
        for extension in extensions {
            pg.preload_extension_module(*extension)?;
        }
        pg.ensure_cluster()?;
        let transport = Transport::prepare(&mut pg)?;
        let mut backend = Self { pg, transport };
        backend.enable_extensions(extensions)?;
        backend
            .reset_session_state()
            .context("initialize proxy backend session state")?;
        Ok(backend)
    }

    #[cfg(not(feature = "extensions"))]
    fn open(root: &Path, _extensions: &[()]) -> Result<Self> {
        let outcome = install_into(root)?;
        let mut pg = PostgresMod::new(outcome.paths)?;
        pg.ensure_cluster()?;
        let transport = Transport::prepare(&mut pg)?;
        let mut backend = Self { pg, transport };
        backend
            .reset_session_state()
            .context("initialize proxy backend session state")?;
        Ok(backend)
    }

    #[cfg(feature = "extensions")]
    fn enable_extensions(&mut self, extensions: &[Extension]) -> Result<()> {
        for extension in extensions {
            let sql = create_extension_sql(*extension);
            let response = self
                .send(&simple_query_message(&sql))
                .with_context(|| format!("enable bundled extension '{}'", extension.sql_name()))?;
            if response.first() == Some(&b'E') {
                bail!(
                    "enable bundled extension '{}' returned a Postgres error",
                    extension.sql_name()
                );
            }
        }
        Ok(())
    }

    fn send(&mut self, message: &[u8]) -> Result<Vec<u8>> {
        self.transport.send(&mut self.pg, message, None)
    }

    fn reject_copy_from_stdin(&mut self) -> Result<Vec<u8>> {
        self.send(&simple_query_message(
            "DO $$ BEGIN RAISE EXCEPTION USING \
             ERRCODE = '0A000', \
             MESSAGE = 'COPY FROM STDIN requires streaming protocol support and is not supported by pglite-oxide server mode yet'; \
             END $$",
        ))
    }

    fn synchronize_after_simple_query_error(&mut self) -> Result<()> {
        let _ = self.send(&sync_message())?;
        Ok(())
    }

    fn rollback_connection_state(&mut self) {
        let _ = self.reset_session_state();
    }

    fn reset_session_state(&mut self) -> Result<()> {
        for sql in [
            "ROLLBACK",
            "DISCARD ALL",
            "SET search_path TO public",
            "SET TIME ZONE 'UTC'",
        ] {
            let response = self.send(&simple_query_message(sql))?;
            if response.first() == Some(&b'E') {
                bail!("reset proxy backend session state failed while running {sql}");
            }
        }
        Ok(())
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

#[derive(Debug, Default, PartialEq, Eq)]
struct StartupParams {
    user: Option<String>,
    database: Option<String>,
}

enum StartupResponse {
    Accept(Vec<u8>),
    Reject(Vec<u8>),
}

fn startup_response_for(message: &[u8]) -> Result<StartupResponse> {
    let params = parse_startup_params(message)?;
    let user = params.user.as_deref().unwrap_or("postgres");
    let database = params.database.as_deref().unwrap_or(user);

    if user != "postgres" {
        return Ok(StartupResponse::Reject(startup_error_response(
            "28000",
            "pglite-oxide local server only supports user \"postgres\"",
        )));
    }
    if database != "template1" {
        return Ok(StartupResponse::Reject(startup_error_response(
            "3D000",
            "pglite-oxide local server only supports database \"template1\"",
        )));
    }

    Ok(StartupResponse::Accept(startup_response()))
}

fn parse_startup_params(message: &[u8]) -> Result<StartupParams> {
    if message.len() < 8 {
        bail!("startup packet is too short");
    }
    let code = i32::from_be_bytes(message[4..8].try_into().unwrap());
    if code != PROTOCOL_3 {
        bail!("startup packet has unsupported protocol code {code}");
    }

    let mut cursor = 8usize;
    let mut params = StartupParams::default();
    while cursor < message.len() {
        if message[cursor] == 0 {
            break;
        }
        let key = read_startup_cstring(message, &mut cursor)?;
        if cursor >= message.len() {
            bail!("startup packet key {key:?} is missing a value");
        }
        let value = read_startup_cstring(message, &mut cursor)?;
        match key.as_str() {
            "user" => params.user = Some(value),
            "database" => params.database = Some(value),
            _ => {}
        }
    }
    Ok(params)
}

fn read_startup_cstring(message: &[u8], cursor: &mut usize) -> Result<String> {
    let start = *cursor;
    let end = message[start..]
        .iter()
        .position(|byte| *byte == 0)
        .map(|offset| start + offset)
        .ok_or_else(|| anyhow!("startup packet contains an unterminated string"))?;
    *cursor = end + 1;
    String::from_utf8(message[start..end].to_vec())
        .context("startup packet contains non-UTF-8 parameter")
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

    let is_simple_query = is_simple_query_message(protocol_batch);
    let response = if simple_query_contains_copy_from_stdin(protocol_batch) {
        backend.reject_copy_from_stdin()?
    } else {
        backend.send(protocol_batch)?
    };
    if is_simple_query && response_contains_error(&response) {
        backend.synchronize_after_simple_query_error()?;
    }
    protocol_batch.clear();
    if !response.is_empty() {
        stream
            .write_all(&response)
            .context("write backend response")?;
    }

    Ok(())
}

fn is_simple_query_message(message: &[u8]) -> bool {
    message.first() == Some(&b'Q')
}

fn simple_query_contains_copy_from_stdin(message: &[u8]) -> bool {
    let Some(sql) = simple_query_sql(message) else {
        return false;
    };
    sql_contains_copy_from_stdin(sql)
}

fn simple_query_sql(message: &[u8]) -> Option<&str> {
    if !is_simple_query_message(message) || message.len() < 6 {
        return None;
    }
    let len = i32::from_be_bytes(message[1..5].try_into().ok()?);
    if len < 5 {
        return None;
    }
    let len = len as usize;
    if len.checked_add(1)? != message.len() || *message.last()? != 0 {
        return None;
    }
    std::str::from_utf8(&message[5..message.len() - 1]).ok()
}

fn sql_contains_copy_from_stdin(sql: &str) -> bool {
    let mut in_copy_statement = false;
    let mut saw_from = false;

    for token in sql_word_tokens(sql) {
        if token == ";" {
            in_copy_statement = false;
            saw_from = false;
            continue;
        }
        if !in_copy_statement {
            in_copy_statement = token == "COPY";
            saw_from = false;
            continue;
        }
        if saw_from && token == "STDIN" {
            return true;
        }
        saw_from = token == "FROM";
    }

    false
}

fn sql_word_tokens(sql: &str) -> Vec<String> {
    let bytes = sql.as_bytes();
    let mut tokens = Vec::new();
    let mut cursor = 0usize;

    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\'' => cursor = skip_single_quoted(bytes, cursor),
            b'"' => cursor = skip_double_quoted(bytes, cursor),
            b'-' if bytes.get(cursor + 1) == Some(&b'-') => {
                cursor = skip_line_comment(bytes, cursor + 2);
            }
            b'/' if bytes.get(cursor + 1) == Some(&b'*') => {
                cursor = skip_block_comment(bytes, cursor + 2);
            }
            b'$' => {
                if let Some(next) = skip_dollar_quoted(bytes, cursor) {
                    cursor = next;
                } else {
                    cursor += 1;
                }
            }
            b';' => {
                tokens.push(";".to_owned());
                cursor += 1;
            }
            byte if byte.is_ascii_alphabetic() || byte == b'_' => {
                let start = cursor;
                cursor += 1;
                while cursor < bytes.len()
                    && (bytes[cursor].is_ascii_alphanumeric() || bytes[cursor] == b'_')
                {
                    cursor += 1;
                }
                tokens.push(sql[start..cursor].to_ascii_uppercase());
            }
            _ => cursor += 1,
        }
    }

    tokens
}

fn skip_single_quoted(bytes: &[u8], mut cursor: usize) -> usize {
    cursor += 1;
    while cursor < bytes.len() {
        if bytes[cursor] == b'\'' {
            cursor += 1;
            if bytes.get(cursor) == Some(&b'\'') {
                cursor += 1;
                continue;
            }
            break;
        }
        cursor += 1;
    }
    cursor
}

fn skip_double_quoted(bytes: &[u8], mut cursor: usize) -> usize {
    cursor += 1;
    while cursor < bytes.len() {
        if bytes[cursor] == b'"' {
            cursor += 1;
            if bytes.get(cursor) == Some(&b'"') {
                cursor += 1;
                continue;
            }
            break;
        }
        cursor += 1;
    }
    cursor
}

fn skip_line_comment(bytes: &[u8], mut cursor: usize) -> usize {
    while cursor < bytes.len() && bytes[cursor] != b'\n' {
        cursor += 1;
    }
    cursor
}

fn skip_block_comment(bytes: &[u8], mut cursor: usize) -> usize {
    while cursor + 1 < bytes.len() {
        if bytes[cursor] == b'*' && bytes[cursor + 1] == b'/' {
            return cursor + 2;
        }
        cursor += 1;
    }
    bytes.len()
}

fn skip_dollar_quoted(bytes: &[u8], cursor: usize) -> Option<usize> {
    let mut end = cursor + 1;
    while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
        end += 1;
    }
    if bytes.get(end) != Some(&b'$') {
        return None;
    }
    let delimiter = &bytes[cursor..=end];
    let body_start = end + 1;
    bytes[body_start..]
        .windows(delimiter.len())
        .position(|window| window == delimiter)
        .map(|offset| body_start + offset + delimiter.len())
}

fn response_contains_error(response: &[u8]) -> bool {
    let mut cursor = 0usize;
    while cursor + 5 <= response.len() {
        let tag = response[cursor];
        let len = i32::from_be_bytes(response[cursor + 1..cursor + 5].try_into().unwrap());
        if len < 4 {
            return false;
        }
        let total = 1usize.saturating_add(len as usize);
        if cursor + total > response.len() {
            return false;
        }
        if tag == b'E' {
            return true;
        }
        cursor += total;
    }
    false
}

fn startup_response() -> Vec<u8> {
    let mut response = Vec::new();
    push_authentication_ok(&mut response);
    push_parameter_status(&mut response, "server_version", "17.5");
    push_parameter_status(&mut response, "server_encoding", "UTF8");
    push_parameter_status(&mut response, "client_encoding", "UTF8");
    push_parameter_status(&mut response, "DateStyle", "ISO, MDY");
    push_parameter_status(&mut response, "TimeZone", "UTC");
    push_parameter_status(&mut response, "integer_datetimes", "on");
    push_backend_key_data(&mut response, 0, 0);
    push_ready_for_query(&mut response, b'I');
    response
}

fn startup_error_response(sqlstate: &str, message: &str) -> Vec<u8> {
    let mut body = Vec::new();
    push_error_field(&mut body, b'S', "FATAL");
    push_error_field(&mut body, b'V', "FATAL");
    push_error_field(&mut body, b'C', sqlstate);
    push_error_field(&mut body, b'M', message);
    body.push(0);

    let mut response = Vec::with_capacity(body.len() + 5);
    response.push(b'E');
    response.extend_from_slice(&((body.len() + 4) as i32).to_be_bytes());
    response.extend_from_slice(&body);
    response
}

fn push_error_field(out: &mut Vec<u8>, field: u8, value: &str) {
    out.push(field);
    out.extend_from_slice(value.as_bytes());
    out.push(0);
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

fn sync_message() -> [u8; 5] {
    [b'S', 0, 0, 0, 4]
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
    fn parses_startup_user_and_database() -> Result<()> {
        let message = startup_packet(&[("user", "postgres"), ("database", "template1")]);
        let params = parse_startup_params(&message)?;
        assert_eq!(params.user.as_deref(), Some("postgres"));
        assert_eq!(params.database.as_deref(), Some("template1"));
        Ok(())
    }

    #[test]
    fn startup_response_accepts_only_supported_identity() -> Result<()> {
        let accepted = startup_response_for(&startup_packet(&[
            ("user", "postgres"),
            ("database", "template1"),
        ]))?;
        assert!(matches!(accepted, StartupResponse::Accept(_)));

        let rejected_user = startup_response_for(&startup_packet(&[
            ("user", "alice"),
            ("database", "template1"),
        ]))?;
        match rejected_user {
            StartupResponse::Reject(response) => {
                assert_error_code(&response, "28000");
                assert!(String::from_utf8_lossy(&response).contains("postgres"));
            }
            StartupResponse::Accept(_) => panic!("unsupported user must be rejected"),
        }

        let rejected_database = startup_response_for(&startup_packet(&[
            ("user", "postgres"),
            ("database", "postgres"),
        ]))?;
        match rejected_database {
            StartupResponse::Reject(response) => {
                assert_error_code(&response, "3D000");
                assert!(String::from_utf8_lossy(&response).contains("template1"));
            }
            StartupResponse::Accept(_) => panic!("unsupported database must be rejected"),
        }

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

    #[test]
    fn response_error_detection_scans_backend_messages() {
        let mut response = Vec::new();
        push_parameter_status(&mut response, "TimeZone", "UTC");
        response.push(b'E');
        response.extend_from_slice(&6_i32.to_be_bytes());
        response.extend_from_slice(b"S\0");
        push_ready_for_query(&mut response, b'I');

        assert!(response_contains_error(&response));
        assert!(!response_contains_error(&startup_response()));
    }

    #[test]
    fn copy_from_stdin_detection_ignores_literals_comments_and_quoted_identifiers() {
        assert!(sql_contains_copy_from_stdin(
            "CREATE TABLE items(value text); COPY items(value) FROM STDIN WITH CSV"
        ));
        assert!(sql_contains_copy_from_stdin(
            "/* comment */ copy public.items from stdin"
        ));
        assert!(!sql_contains_copy_from_stdin(
            "SELECT 'COPY items FROM STDIN' AS text"
        ));
        assert!(!sql_contains_copy_from_stdin(
            "SELECT $$ COPY items FROM STDIN $$ AS text"
        ));
        assert!(!sql_contains_copy_from_stdin("COPY items TO STDOUT"));
        assert!(!sql_contains_copy_from_stdin(
            "COPY items FROM '/tmp/input.csv'"
        ));
        assert!(!sql_contains_copy_from_stdin(
            "SELECT \"copy\" FROM stdin_table"
        ));
    }

    fn startup_packet(params: &[(&str, &str)]) -> Vec<u8> {
        let mut message = Vec::new();
        message.extend_from_slice(&[0, 0, 0, 0]);
        message.extend_from_slice(&PROTOCOL_3.to_be_bytes());
        for (key, value) in params {
            message.extend_from_slice(key.as_bytes());
            message.push(0);
            message.extend_from_slice(value.as_bytes());
            message.push(0);
        }
        message.push(0);
        let len = message.len() as i32;
        message[..4].copy_from_slice(&len.to_be_bytes());
        message
    }

    fn assert_error_code(response: &[u8], expected: &str) {
        assert_eq!(response.first(), Some(&b'E'));
        let code = response
            .windows(expected.len())
            .any(|window| window == expected.as_bytes());
        assert!(code, "response did not contain SQLSTATE {expected}");
    }
}
