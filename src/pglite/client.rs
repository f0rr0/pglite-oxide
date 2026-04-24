use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use tempfile::TempDir;

use crate::pglite::base::PglitePaths;
use crate::pglite::builder::PgliteBuilder;
use crate::pglite::errors::PgliteError;
use crate::pglite::interface::{
    DataTransferContainer, DescribeQueryParam, DescribeQueryResult, DescribeResultField,
    ExecProtocolOptions, ExecProtocolResult, ParserMap, QueryOptions, Results, Serializer,
    SerializerMap, TypeParser,
};
use crate::pglite::parse::{parse_describe_statement_results, parse_results};
use crate::pglite::postgres_mod::PostgresMod;
use crate::pglite::transport::Transport;
use crate::pglite::types::{
    DEFAULT_PARSERS, DEFAULT_SERIALIZERS, TEXT, parse_array_text, serialize_array_value,
};
use crate::protocol::messages::{BackendMessage, DatabaseError};
use crate::protocol::parser::Parser as ProtocolParser;
use crate::protocol::serializer::{BindConfig, BindValue, PortalTarget, Serialize};

type ChannelCallback = Arc<dyn Fn(&str) + Send + Sync + 'static>;
type GlobalCallback = Arc<dyn Fn(&str, &str) + Send + Sync + 'static>;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ListenerHandle {
    channel: String,
    normalized_channel: String,
    id: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GlobalListenerHandle {
    id: u64,
}

impl ListenerHandle {
    pub fn channel(&self) -> &str {
        &self.channel
    }

    pub fn id(&self) -> u64 {
        self.id
    }
}

impl GlobalListenerHandle {
    pub fn id(&self) -> u64 {
        self.id
    }
}

struct ChannelListener {
    id: u64,
    callback: ChannelCallback,
}

struct GlobalListener {
    id: u64,
    callback: GlobalCallback,
}

/// Primary entry point for interacting with the embedded Postgres runtime.
pub struct Pglite {
    pg: PostgresMod,
    _temp_dir: Option<TempDir>,
    transport: Transport,
    parser: ProtocolParser,
    serializers: SerializerMap,
    parsers: ParserMap,
    array_types_initialized: bool,
    in_transaction: bool,
    ready: bool,
    closing: bool,
    closed: bool,
    blob_input_provided: bool,
    notify_listeners: HashMap<String, Vec<ChannelListener>>,
    global_notify_listeners: Vec<GlobalListener>,
    next_listener_id: u64,
    next_global_listener_id: u64,
}

impl Pglite {
    /// Create a builder for opening persistent or temporary PGlite databases.
    pub fn builder() -> PgliteBuilder {
        PgliteBuilder::new()
    }

    /// Open a persistent PGlite database rooted at `root`, installing and initializing it if needed.
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        Self::builder().path(root.as_ref().to_path_buf()).open()
    }

    /// Open a persistent PGlite database under the platform data directory for `app_id`.
    pub fn open_app(app_id: (&str, &str, &str)) -> Result<Self> {
        Self::builder().app_id(app_id).open()
    }

    /// Create an ephemeral PGlite database whose files are removed when the instance is dropped.
    pub fn temporary() -> Result<Self> {
        Self::builder().temporary().open()
    }

    /// Create a new Pglite instance backed by the provided runtime paths.
    #[doc(hidden)]
    pub fn new(paths: PglitePaths) -> Result<Self> {
        let mut pg = PostgresMod::new(paths)?;
        pg.ensure_cluster()?;
        let transport = Transport::prepare(&mut pg)?;

        let mut instance = Self {
            pg,
            _temp_dir: None,
            transport,
            parser: ProtocolParser::new(),
            serializers: DEFAULT_SERIALIZERS.clone(),
            parsers: DEFAULT_PARSERS.clone(),
            array_types_initialized: false,
            in_transaction: false,
            ready: true,
            closing: false,
            closed: false,
            blob_input_provided: false,
            notify_listeners: HashMap::new(),
            global_notify_listeners: Vec::new(),
            next_listener_id: 1,
            next_global_listener_id: 1,
        };

        instance.exec_internal("SET search_path TO public;", None)?;
        instance.init_array_types(true)?;
        Ok(instance)
    }

    /// Execute a SQL query using the extended protocol.
    pub fn query(
        &mut self,
        sql: &str,
        params: &[Value],
        options: Option<&QueryOptions>,
    ) -> Result<Results> {
        self.check_ready()?;
        self.init_array_types(false)?;

        self.query_internal(sql, params, options)
    }

    fn query_internal(
        &mut self,
        sql: &str,
        params: &[Value],
        options: Option<&QueryOptions>,
    ) -> Result<Results> {
        let default_options = QueryOptions::default();
        let query_opts = options.unwrap_or(&default_options);

        self.handle_blob_input(query_opts.blob.as_ref())?;

        let params_snapshot: Vec<Value> = params.to_vec();
        let options_snapshot = options.cloned();
        let mut collected_messages: Vec<BackendMessage> = Vec::new();

        let mut exec_opts = ExecProtocolOptions::no_sync();
        exec_opts.on_notice = query_opts.on_notice.clone();
        exec_opts.data_transfer_container = query_opts.data_transfer_container;

        let result: Result<()> = (|| {
            let param_types = if query_opts.param_types.is_empty() {
                &[] as &[i32]
            } else {
                &query_opts.param_types
            };

            let parse_msg = Serialize::parse(None, sql, param_types);
            let ExecProtocolResult { messages } =
                self.exec_protocol(&parse_msg, exec_opts.clone())?;
            collected_messages.extend(messages);

            let describe_msg = Serialize::describe(&PortalTarget::new('S', None));
            let ExecProtocolResult { messages } =
                self.exec_protocol(&describe_msg, exec_opts.clone())?;
            let data_type_ids = parse_describe_statement_results(&messages);
            collected_messages.extend(messages);

            let bind_values = self.prepare_bind_values(params, &data_type_ids, query_opts)?;
            let bind_config = BindConfig {
                values: bind_values,
                ..Default::default()
            };
            let bind_msg = Serialize::bind(&bind_config);
            let ExecProtocolResult { messages } =
                self.exec_protocol(&bind_msg, exec_opts.clone())?;
            collected_messages.extend(messages);

            let describe_portal = Serialize::describe(&PortalTarget::new('P', None));
            let ExecProtocolResult { messages } =
                self.exec_protocol(&describe_portal, exec_opts.clone())?;
            collected_messages.extend(messages);

            let exec_msg = Serialize::execute(None);
            let ExecProtocolResult { messages } =
                self.exec_protocol(&exec_msg, exec_opts.clone())?;
            collected_messages.extend(messages);

            Ok(())
        })();

        match self.exec_protocol(&Serialize::sync(), exec_opts.clone()) {
            Ok(ExecProtocolResult { messages }) => collected_messages.extend(messages),
            Err(err) if result.is_ok() => {
                return Err(err.context(format!("failed to synchronize extended query: {sql}")));
            }
            Err(_) => {}
        }

        if let Err(err) = result {
            match err.downcast::<DatabaseError>() {
                Ok(db_err) => {
                    let enriched = PgliteError::new(db_err, sql, params_snapshot, options_snapshot);
                    return Err(enriched.into());
                }
                Err(err) => {
                    return Err(err.context(format!("failed to execute extended query: {sql}")));
                }
            }
        }

        self.finish_query(collected_messages, options)
    }

    /// Return `true` if the instance is ready for new work.
    pub fn is_ready(&self) -> bool {
        self.ready && !self.closing && !self.closed
    }

    /// Return the host-side runtime and data-directory paths backing this instance.
    #[doc(hidden)]
    pub fn paths(&self) -> &PglitePaths {
        self.pg.paths()
    }

    pub(crate) fn attach_temp_dir(&mut self, temp_dir: TempDir) {
        self._temp_dir = Some(temp_dir);
    }

    /// Return `true` if the instance has already been closed.
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// Shut down the embedded Postgres runtime.
    pub fn close(&mut self) -> Result<()> {
        if self.closed {
            return Ok(());
        }
        if self.closing {
            bail!("Pglite is closing");
        }

        self.closing = true;
        let result = {
            let options = ExecProtocolOptions {
                throw_on_error: false,
                sync_to_fs: false,
                ..ExecProtocolOptions::default()
            };

            let end_message = Serialize::end();
            let _ = self.exec_protocol(&end_message, options);
            self.sync_to_fs()
        };

        self.closing = false;
        if result.is_ok() {
            self.closed = true;
            self.ready = false;
            self.notify_listeners.clear();
            self.global_notify_listeners.clear();
        }
        result
    }

    /// Execute a simple SQL statement that may contain multiple commands.
    pub fn exec(&mut self, sql: &str, options: Option<&QueryOptions>) -> Result<Vec<Results>> {
        self.check_ready()?;
        self.init_array_types(false)?;

        self.exec_internal(sql, options)
    }

    fn exec_internal(&mut self, sql: &str, options: Option<&QueryOptions>) -> Result<Vec<Results>> {
        let options_snapshot = options.cloned();
        let default_options = QueryOptions::default();
        let exec_opts_ref = options.unwrap_or(&default_options);
        let mut exec_opts = ExecProtocolOptions::no_sync();
        exec_opts.on_notice = exec_opts_ref.on_notice.clone();
        exec_opts.data_transfer_container = exec_opts_ref.data_transfer_container;

        self.handle_blob_input(exec_opts_ref.blob.as_ref())?;

        let mut collected_messages: Vec<BackendMessage> = Vec::new();

        let result: Result<()> = (|| {
            let message = Serialize::query(sql);
            let ExecProtocolResult { messages } =
                self.exec_protocol(&message, exec_opts.clone())?;
            collected_messages.extend(messages);
            Ok(())
        })();

        match self.exec_protocol(&Serialize::sync(), exec_opts.clone()) {
            Ok(ExecProtocolResult { messages }) => collected_messages.extend(messages),
            Err(err) if result.is_ok() => {
                return Err(err.context(format!("failed to synchronize simple query: {sql}")));
            }
            Err(_) => {}
        }

        if let Err(err) = result {
            match err.downcast::<DatabaseError>() {
                Ok(db_err) => {
                    let enriched = PgliteError::new(db_err, sql, Vec::new(), options_snapshot);
                    return Err(enriched.into());
                }
                Err(err) => {
                    return Err(err.context(format!("failed to execute simple query: {sql}")));
                }
            }
        }

        self.finish_exec(collected_messages, options)
    }

    /// Register a listener for `LISTEN channel`. Returns a handle that can be used to unlisten.
    pub fn listen<F>(&mut self, channel: &str, callback: F) -> Result<ListenerHandle>
    where
        F: Fn(&str) + Send + Sync + 'static,
    {
        self.check_ready()?;
        self.init_array_types(false)?;

        let normalized = to_postgres_name(channel);
        let should_listen = match self.notify_listeners.get(&normalized) {
            Some(existing) => existing.is_empty(),
            None => true,
        };

        if should_listen {
            self.exec_internal(&format!("LISTEN {}", channel), None)?;
        }

        let callback: ChannelCallback = Arc::new(callback);
        let entry = self.notify_listeners.entry(normalized.clone()).or_default();
        let id = self.next_listener_id;
        self.next_listener_id = self.next_listener_id.wrapping_add(1);
        entry.push(ChannelListener { id, callback });

        Ok(ListenerHandle {
            channel: channel.to_string(),
            normalized_channel: normalized,
            id,
        })
    }

    /// Remove a listener corresponding to the provided handle.
    pub fn unlisten(&mut self, handle: ListenerHandle) -> Result<()> {
        if let Some(listeners) = self.notify_listeners.get_mut(&handle.normalized_channel) {
            listeners.retain(|listener| listener.id != handle.id);
            if listeners.is_empty() {
                self.notify_listeners.remove(&handle.normalized_channel);
                self.exec_internal(&format!("UNLISTEN {}", handle.channel), None)?;
            }
        }
        Ok(())
    }

    /// Remove all listeners for the specified channel.
    pub fn unlisten_channel(&mut self, channel: &str) -> Result<()> {
        let normalized = to_postgres_name(channel);
        if self.notify_listeners.remove(&normalized).is_some() {
            self.exec_internal(&format!("UNLISTEN {}", channel), None)?;
        }
        Ok(())
    }

    /// Register a global notification callback.
    pub fn on_notification<F>(&mut self, callback: F) -> GlobalListenerHandle
    where
        F: Fn(&str, &str) + Send + Sync + 'static,
    {
        let id = self.next_global_listener_id;
        self.next_global_listener_id = self.next_global_listener_id.wrapping_add(1);
        let callback: GlobalCallback = Arc::new(callback);
        self.global_notify_listeners
            .push(GlobalListener { id, callback });
        GlobalListenerHandle { id }
    }

    /// Deregister a previously registered global notification callback.
    pub fn off_notification(&mut self, handle: GlobalListenerHandle) {
        self.global_notify_listeners
            .retain(|listener| listener.id != handle.id);
    }

    /// Describe the parameter and result metadata for a SQL query.
    pub fn describe_query(
        &mut self,
        sql: &str,
        options: Option<&QueryOptions>,
    ) -> Result<DescribeQueryResult> {
        self.check_ready()?;
        self.init_array_types(false)?;

        let default_options = QueryOptions::default();
        let query_opts = options.unwrap_or(&default_options);

        let options_snapshot = options.cloned();
        let mut exec_opts = ExecProtocolOptions::no_sync();
        exec_opts.on_notice = query_opts.on_notice.clone();
        exec_opts.data_transfer_container = query_opts.data_transfer_container;

        let mut describe_messages: Vec<BackendMessage> = Vec::new();

        let result: Result<()> = (|| {
            let param_types = if query_opts.param_types.is_empty() {
                &[] as &[i32]
            } else {
                &query_opts.param_types
            };

            let parse_msg = Serialize::parse(None, sql, param_types);
            // Ignore returned messages; we just need to ensure the statement parses.
            let _ = self.exec_protocol(&parse_msg, exec_opts.clone())?;

            let describe_msg = Serialize::describe(&PortalTarget::new('S', None));
            let ExecProtocolResult { messages } =
                self.exec_protocol(&describe_msg, exec_opts.clone())?;
            describe_messages.extend(messages);

            Ok(())
        })();

        match self.exec_protocol(&Serialize::sync(), exec_opts.clone()) {
            Ok(ExecProtocolResult { messages }) => describe_messages.extend(messages),
            Err(err) if result.is_ok() => {
                return Err(err.context(format!("failed to synchronize describe query: {sql}")));
            }
            Err(_) => {}
        }

        if let Err(err) = result {
            match err.downcast::<DatabaseError>() {
                Ok(db_err) => {
                    let enriched = PgliteError::new(db_err, sql, Vec::new(), options_snapshot);
                    return Err(enriched.into());
                }
                Err(err) => {
                    return Err(err.context(format!("failed to describe query: {sql}")));
                }
            }
        }

        let param_type_ids = parse_describe_statement_results(&describe_messages);
        let query_params = param_type_ids
            .into_iter()
            .map(|oid| DescribeQueryParam {
                data_type_id: oid,
                serializer: self.serializers.get(&oid).cloned(),
            })
            .collect();

        let result_fields = describe_messages
            .iter()
            .find_map(|msg| match msg {
                BackendMessage::RowDescription(desc) => Some(
                    desc.fields
                        .iter()
                        .map(|field| DescribeResultField {
                            name: field.name.clone(),
                            data_type_id: field.data_type_id,
                            parser: self.parsers.get(&field.data_type_id).cloned(),
                        })
                        .collect::<Vec<_>>(),
                ),
                _ => None,
            })
            .unwrap_or_default();

        Ok(DescribeQueryResult {
            query_params,
            result_fields,
        })
    }

    /// Run a closure within an SQL transaction (`BEGIN .. COMMIT/ROLLBACK`).
    pub fn transaction<F, T>(&mut self, mut callback: F) -> Result<T>
    where
        F: FnMut(&mut Transaction<'_>) -> Result<T>,
    {
        self.check_ready()?;
        self.init_array_types(false)?;

        // Begin transaction
        self.run_exec_command("BEGIN")?;
        self.in_transaction = true;

        let mut tx = Transaction::new(self);
        let callback_result = callback(&mut tx);

        let txn_result = match callback_result {
            Ok(value) => {
                if !tx.closed {
                    tx.commit_internal()?;
                }
                Ok(value)
            }
            Err(err) => {
                if !tx.closed {
                    tx.rollback_internal()?;
                }
                Err(err)
            }
        };

        self.in_transaction = false;
        txn_result
    }

    /// Flush runtime writes to the underlying filesystem. Currently a no-op on the host.
    pub fn sync_to_fs(&mut self) -> Result<()> {
        let mount_root = self.pg.paths().mount_root();
        if let Ok(file) = std::fs::OpenOptions::new().read(true).open(mount_root) {
            let _ = file.sync_all();
        }
        let data_root = mount_root.join("pglite");
        if let Ok(file) = std::fs::OpenOptions::new().read(true).open(&data_root) {
            let _ = file.sync_all();
        }
        Ok(())
    }

    fn prepare_bind_values(
        &self,
        params: &[Value],
        data_type_ids: &[i32],
        options: &QueryOptions,
    ) -> Result<Vec<BindValue>> {
        if params.is_empty() {
            return Ok(Vec::new());
        }

        let mut values = Vec::with_capacity(params.len());
        let overrides = if options.serializers.is_empty() {
            None
        } else {
            Some(&options.serializers)
        };

        for (idx, value) in params.iter().enumerate() {
            if value.is_null() {
                values.push(BindValue::Null);
                continue;
            }

            let oid = data_type_ids.get(idx).copied().unwrap_or(TEXT);
            let serializer = overrides
                .and_then(|map| map.get(&oid))
                .or_else(|| self.serializers.get(&oid));

            let serialized = match serializer {
                Some(func) => func(value).with_context(|| {
                    format!("failed to serialize parameter {idx} using OID {oid}")
                })?,
                None => self.default_serialize_value(value),
            };

            values.push(BindValue::Text(serialized));
        }

        Ok(values)
    }

    fn default_serialize_value(&self, value: &Value) -> String {
        Self::default_serialize_value_static(value)
    }

    pub(crate) fn default_serialize_value_static(value: &Value) -> String {
        match value {
            Value::String(s) => s.clone(),
            Value::Number(num) => num.to_string(),
            Value::Bool(flag) => {
                if *flag {
                    "t".to_string()
                } else {
                    "f".to_string()
                }
            }
            _ => value.to_string(),
        }
    }

    fn finish_query(
        &mut self,
        messages: Vec<BackendMessage>,
        options: Option<&QueryOptions>,
    ) -> Result<Results> {
        let blob = self.get_written_blob()?;
        self.cleanup_blob()?;
        if !self.in_transaction {
            self.sync_to_fs()?;
        }
        let parsed = parse_results(&messages, &self.parsers, options, blob);
        parsed
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("query returned no result sets"))
    }

    fn finish_exec(
        &mut self,
        messages: Vec<BackendMessage>,
        options: Option<&QueryOptions>,
    ) -> Result<Vec<Results>> {
        let blob = self.get_written_blob()?;
        self.cleanup_blob()?;
        if !self.in_transaction {
            self.sync_to_fs()?;
        }
        Ok(parse_results(&messages, &self.parsers, options, blob))
    }

    fn exec_protocol(
        &mut self,
        message: &[u8],
        options: ExecProtocolOptions,
    ) -> Result<ExecProtocolResult> {
        let ExecProtocolOptions {
            sync_to_fs,
            throw_on_error,
            on_notice,
            data_transfer_container,
        } = options;

        let data = self.exec_protocol_raw(message, sync_to_fs, data_transfer_container)?;

        let mut messages = Vec::new();
        let on_notice_cb = on_notice.clone();
        if let Err(err) = self.parser.parse(&data, |msg| {
            if let BackendMessage::Error(db_err) = &msg
                && throw_on_error
            {
                return Err(anyhow!(db_err.clone()));
            }
            if let Some(callback) = on_notice_cb.as_ref()
                && let BackendMessage::Notice(notice) = &msg
            {
                callback(notice);
            }
            messages.push(msg);
            Ok(())
        }) {
            match err.downcast::<DatabaseError>() {
                Ok(db_err) => {
                    self.parser = ProtocolParser::new();
                    return Err(anyhow!(db_err));
                }
                Err(err) => return Err(err),
            }
        }

        for message in &messages {
            if let BackendMessage::Notification(note) = message {
                let key = to_postgres_name(&note.channel);
                if let Some(listeners) = self.notify_listeners.get(&key) {
                    for listener in listeners {
                        (listener.callback)(&note.payload);
                    }
                }
                for listener in &self.global_notify_listeners {
                    (listener.callback)(&note.channel, &note.payload);
                }
            }
        }

        Ok(ExecProtocolResult { messages })
    }

    fn exec_protocol_raw(
        &mut self,
        message: &[u8],
        sync_to_fs: bool,
        data_transfer_container: Option<DataTransferContainer>,
    ) -> Result<Vec<u8>> {
        let data = self
            .transport
            .send(&mut self.pg, message, data_transfer_container)?;
        if sync_to_fs {
            self.sync_to_fs()?;
        }
        Ok(data)
    }

    fn init_array_types(&mut self, force: bool) -> Result<()> {
        if self.array_types_initialized && !force {
            return Ok(());
        }

        let prev = self.array_types_initialized;
        self.array_types_initialized = true;

        let result: Result<()> = {
            let sql = "
                SELECT b.oid, b.typarray
                FROM pg_catalog.pg_type a
                LEFT JOIN pg_catalog.pg_type b ON b.oid = a.typelem
                WHERE a.typcategory = 'A'
                GROUP BY b.oid, b.typarray
                ORDER BY b.oid
            ";
            let results = self.exec(sql, None)?;
            let result_set = results
                .into_iter()
                .next()
                .ok_or_else(|| anyhow!("array type discovery returned no results"))?;

            for row in result_set.rows {
                let map = match row {
                    Value::Object(map) => map,
                    _ => continue,
                };
                let element_oid = value_to_i32(map.get("oid")).unwrap_or(0);
                let array_oid = value_to_i32(map.get("typarray")).unwrap_or(0);

                if element_oid == 0 || array_oid == 0 {
                    continue;
                }

                let element_parser = self.parsers.get(&element_oid).cloned();
                let element_serializer = self.serializers.get(&element_oid).cloned();

                let parser_clone = element_parser.clone();
                let array_parser: TypeParser = Arc::new(move |text: &str, _| {
                    parse_array_text(text, parser_clone.clone(), element_oid, array_oid)
                });
                self.parsers.insert(array_oid, array_parser);

                let serializer_clone = element_serializer.clone();
                let array_serializer: Serializer = Arc::new(move |value: &Value| {
                    serialize_array_value(value, serializer_clone.clone(), array_oid)
                });
                self.serializers.insert(array_oid, array_serializer);
            }
            Ok(())
        };

        if let Err(err) = result {
            self.array_types_initialized = prev;
            Err(err)
        } else {
            Ok(())
        }
    }

    fn run_exec_command(&mut self, sql: &str) -> Result<()> {
        self.exec_internal(sql, None).map(|_| ())
    }

    fn handle_blob_input(&mut self, blob: Option<&Vec<u8>>) -> Result<()> {
        let path = self.dev_blob_path();
        if let Some(bytes) = blob {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).with_context(|| {
                    format!("failed to create blob directory {}", parent.display())
                })?;
            }
            fs::write(&path, bytes)
                .with_context(|| format!("write blob input to {}", path.display()))?;
            self.blob_input_provided = true;
        } else {
            self.blob_input_provided = false;
            let _ = fs::remove_file(&path);
        }
        Ok(())
    }

    fn dev_blob_path(&self) -> PathBuf {
        self.pg.paths().pgroot.join("dev/blob")
    }

    fn cleanup_blob(&mut self) -> Result<()> {
        Ok(())
    }

    fn get_written_blob(&mut self) -> Result<Option<Vec<u8>>> {
        let path = self.dev_blob_path();

        if self.blob_input_provided {
            self.blob_input_provided = false;
            let _ = fs::remove_file(&path);
            return Ok(None);
        }

        match fs::read(&path) {
            Ok(data) => {
                self.blob_input_provided = false;
                let _ = fs::remove_file(&path);
                if data.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(data))
                }
            }
            Err(err) => {
                if err.kind() == io::ErrorKind::NotFound {
                    self.blob_input_provided = false;
                    Ok(None)
                } else {
                    Err(err).with_context(|| format!("read blob output from {}", path.display()))
                }
            }
        }
    }

    fn check_ready(&self) -> Result<()> {
        if self.closing {
            bail!("Pglite instance is closing");
        }
        if self.closed {
            bail!("Pglite instance is closed");
        }
        if !self.ready {
            bail!("Pglite instance is not ready");
        }
        Ok(())
    }
}

impl Drop for Pglite {
    fn drop(&mut self) {
        if !self.closed {
            let _ = self.close();
        }
    }
}

fn to_postgres_name(input: &str) -> String {
    if input.starts_with('"') && input.ends_with('"') && input.len() >= 2 {
        input[1..input.len() - 1].to_string()
    } else {
        input.to_lowercase()
    }
}

fn value_to_i32(value: Option<&Value>) -> Option<i32> {
    match value? {
        Value::Number(number) => number.as_i64().map(|value| value as i32),
        Value::String(string) => string.parse::<i32>().ok(),
        _ => None,
    }
}

/// Transaction handle used within [`Pglite::transaction`].
pub struct Transaction<'a> {
    client: &'a mut Pglite,
    closed: bool,
}

impl<'a> Transaction<'a> {
    fn new(client: &'a mut Pglite) -> Self {
        Self {
            client,
            closed: false,
        }
    }

    fn commit_internal(&mut self) -> Result<()> {
        self.ensure_open()?;
        self.client.exec_internal("COMMIT", None)?;
        self.closed = true;
        Ok(())
    }

    fn rollback_internal(&mut self) -> Result<()> {
        self.ensure_open()?;
        self.client.exec_internal("ROLLBACK", None)?;
        self.closed = true;
        Ok(())
    }

    fn ensure_open(&self) -> Result<()> {
        if self.closed {
            bail!("transaction is already closed");
        }
        Ok(())
    }

    pub fn query(
        &mut self,
        sql: &str,
        params: &[Value],
        options: Option<&QueryOptions>,
    ) -> Result<Results> {
        self.ensure_open()?;
        self.client.query_internal(sql, params, options)
    }

    pub fn exec(&mut self, sql: &str, options: Option<&QueryOptions>) -> Result<Vec<Results>> {
        self.ensure_open()?;
        self.client.exec_internal(sql, options)
    }

    pub fn commit(&mut self) -> Result<()> {
        self.commit_internal()
    }

    pub fn rollback(&mut self) -> Result<()> {
        self.rollback_internal()
    }

    pub fn is_closed(&self) -> bool {
        self.closed
    }

    pub fn closed(&self) -> bool {
        self.closed
    }
}
