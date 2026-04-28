use std::fs;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail, ensure};
use tokio::runtime::Runtime as TokioRuntime;
use tracing::warn;
use wasmer::{Engine, Instance, Module, Store, TypedFunction, WasmTypeList};
use wasmer_types::ModuleHash;
use wasmer_wasix::fs::WasiFsRoot;
use wasmer_wasix::runners::wasi::{PackageOrHash, RuntimeOrEngine, WasiRunner};
use wasmer_wasix::runtime::module_cache::ModuleCache;
use wasmer_wasix::runtime::module_cache::SharedCache;
use wasmer_wasix::runtime::task_manager::tokio::TokioTaskManager;
use wasmer_wasix::runtime::{PluggableRuntime, Runtime};
use wasmer_wasix::virtual_fs::null_file::NullFile;
use wasmer_wasix::{WasiFunctionEnv, virtual_fs};
use webc::metadata::annotations::Wasi;

use super::aot;
use super::base::PglitePaths;
#[cfg(feature = "extensions")]
use super::extensions::Extension;

const PGLITE_EXE_PATH: &str = "/bin/pglite";
const PGDATA_DIR: &str = "/base";
const WASM_PREFIX: &str = "/";
const RUNTIME_SIDE_MODULES: &[(&str, &str)] = &[
    ("plpgsql.so", "runtime-support:plpgsql"),
    ("dict_snowball.so", "runtime-support:dict_snowball"),
];
const JS_EMSCRIPTEN_LIFECYCLE_EXPORTS: &[&str] = &[
    concat!("pgl_start", "PGlite"),
    concat!("pgl_set", "PGliteActive"),
];

pub struct PostgresMod {
    #[cfg_attr(not(feature = "extensions"), allow(dead_code))]
    engine: Engine,
    module: Module,
    #[cfg_attr(not(feature = "extensions"), allow(dead_code))]
    tokio_runtime: TokioRuntime,
    #[cfg_attr(not(feature = "extensions"), allow(dead_code))]
    wasix_module_cache: Arc<SharedCache>,
    _wasix_runtime: Arc<dyn Runtime + Send + Sync>,
    store: Store,
    _instance: Instance,
    env: WasiFunctionEnv,
    malloc: TypedFunction<i32, i32>,
    io: WasixPgliteIo,
    lifecycle: PgliteLifecycleExports,
    protocol: WasixProtocolExports,
    paths: PglitePaths,
    cluster_ready: bool,
    backend_started: bool,
    started: bool,
}

struct PgliteLifecycleExports {
    pgl_initdb: TypedFunction<(), i32>,
    pgl_backend: TypedFunction<(), ()>,
}

struct WasixProtocolExports {
    get_port: TypedFunction<(), i32>,
    process_startup: TypedFunction<(i32, i32, i32), i32>,
    send_conn_data: TypedFunction<(), ()>,
    pq_flush: TypedFunction<(), ()>,
    main_loop: TypedFunction<(), ()>,
    send_ready: TypedFunction<(), ()>,
    recover_error: TypedFunction<(), ()>,
}

struct WasixPgliteIo {
    input_reset: TypedFunction<(), i32>,
    input_write: TypedFunction<(i32, i32), i32>,
    input_available: TypedFunction<(), i32>,
    output_reset: TypedFunction<(), i32>,
    output_len: TypedFunction<(), i32>,
    output_read: TypedFunction<(i32, i32), i32>,
}

impl PostgresMod {
    pub(crate) fn preload_module(_module_path: &std::path::Path) -> Result<()> {
        aot::preload_runtime_artifact()
    }

    pub fn new(paths: PglitePaths) -> Result<Self> {
        ensure_runtime_dirs(&paths)?;
        let runtime_root = paths.runtime_root();
        ensure!(
            runtime_root.join("bin/pglite").exists(),
            "WASIX PGlite executable not found at {}",
            runtime_root.join("bin/pglite").display()
        );

        let (engine, module) = aot::load_runtime_module()?;
        let tokio_runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("create Tokio runtime for Wasmer/WASIX filesystem")?;
        let wasix_module_cache = Arc::new(SharedCache::new());
        preload_runtime_side_modules(&tokio_runtime, &engine, &wasix_module_cache, &runtime_root)?;
        #[cfg(feature = "extensions")]
        preload_installed_extension_side_modules(
            &tokio_runtime,
            &engine,
            &wasix_module_cache,
            &runtime_root,
        )?;
        let wasix_runtime =
            build_wasix_runtime(&tokio_runtime, &engine, wasix_module_cache.clone());
        let mut store = Store::new(engine.clone());

        let (instance, env) = instantiate_wasix_module(
            &tokio_runtime,
            &wasix_runtime,
            &mut store,
            &runtime_root,
            module.clone(),
        )?;
        seed_exported_c_string_value(&mut store, &instance, &env, "my_exec_path", PGLITE_EXE_PATH)?;
        call_wasi_start(&mut store, &instance);

        let malloc = typed_export::<i32, i32>(&mut store, &instance, "malloc")?;
        let io = WasixPgliteIo::new(&mut store, &instance)?;
        ensure_no_js_lifecycle_contract(&instance)?;
        let lifecycle = PgliteLifecycleExports::load(&mut store, &instance)?;
        let protocol = WasixProtocolExports::load(&mut store, &instance)?;

        let pg = Self {
            engine,
            module,
            tokio_runtime,
            wasix_module_cache,
            _wasix_runtime: wasix_runtime,
            store,
            _instance: instance,
            env,
            malloc,
            io,
            lifecycle,
            protocol,
            paths,
            cluster_ready: false,
            backend_started: false,
            started: false,
        };
        Ok(pg)
    }

    pub fn paths(&self) -> &PglitePaths {
        &self.paths
    }

    pub fn ensure_cluster(&mut self) -> Result<()> {
        self.initialize_cluster()?;
        self.start_backend()
    }

    pub fn initialize_cluster(&mut self) -> Result<()> {
        if self.cluster_ready {
            return Ok(());
        }

        let had_cluster = self.paths.is_cluster_initialized();
        self.init_cluster_once(had_cluster)?;

        ensure!(
            self.paths.is_cluster_initialized(),
            "PGDATA is not initialized; install the WASIX runtime assets and template before opening"
        );
        if !had_cluster {
            self.replace_process()
                .context("restart WASIX process after initdb")?;
            self.init_cluster_once(true)?;
        }
        self.cluster_ready = true;
        Ok(())
    }

    fn init_cluster_once(&mut self, had_cluster: bool) -> Result<()> {
        let rc = self
            .lifecycle
            .pgl_initdb
            .call(&mut self.store)
            .context("pgl_initdb")?;
        if rc != 0 {
            if self.paths.is_cluster_initialized() {
                if !had_cluster {
                    warn!("pgl_initdb returned status {rc}, but PG_VERSION exists; continuing");
                }
            } else {
                bail!("pgl_initdb returned non-zero status: {rc}");
            }
        }
        Ok(())
    }

    fn replace_process(&mut self) -> Result<()> {
        let runtime_root = self.paths.runtime_root();
        let mut store = Store::new(self.engine.clone());
        let (instance, env) = instantiate_wasix_module(
            &self.tokio_runtime,
            &self._wasix_runtime,
            &mut store,
            &runtime_root,
            self.module.clone(),
        )?;
        seed_exported_c_string_value(&mut store, &instance, &env, "my_exec_path", PGLITE_EXE_PATH)?;
        call_wasi_start(&mut store, &instance);

        let malloc = typed_export::<i32, i32>(&mut store, &instance, "malloc")?;
        let io = WasixPgliteIo::new(&mut store, &instance)?;
        ensure_no_js_lifecycle_contract(&instance)?;
        let lifecycle = PgliteLifecycleExports::load(&mut store, &instance)?;
        let protocol = WasixProtocolExports::load(&mut store, &instance)?;

        self.store = store;
        self._instance = instance;
        self.env = env;
        self.malloc = malloc;
        self.io = io;
        self.lifecycle = lifecycle;
        self.protocol = protocol;
        self.backend_started = false;
        self.started = false;
        Ok(())
    }

    fn start_backend(&mut self) -> Result<()> {
        if self.backend_started {
            return Ok(());
        }
        self.lifecycle
            .pgl_backend
            .call(&mut self.store)
            .context("pgl_backend")?;
        self.backend_started = true;
        Ok(())
    }

    #[cfg(feature = "extensions")]
    pub fn preload_extension_module(&self, extension: Extension) -> Result<()> {
        let runtime_root = self.paths.runtime_root();
        let library = runtime_root
            .join("lib")
            .join("postgresql")
            .join(format!("{}.so", extension.sql_name()));
        ensure!(
            library.exists(),
            "extension library for '{}' is not installed at {}",
            extension.sql_name(),
            library.display()
        );

        seed_side_module_cache(
            &self.tokio_runtime,
            &self.engine,
            &self.wasix_module_cache,
            &library,
            extension.aot_name(),
            &format!("extension '{}'", extension.sql_name()),
        )?;
        Ok(())
    }

    pub fn send_protocol(&mut self, payload: &[u8]) -> Result<Vec<u8>> {
        self.start_protocol()?;
        let mut output = Vec::new();
        for message in split_frontend_messages(payload) {
            output.extend(self.send_one_protocol_message(message)?);
        }
        Ok(output)
    }

    fn start_protocol(&mut self) -> Result<()> {
        self.ensure_cluster()?;
        if self.started {
            return Ok(());
        }

        self.io.reset(&mut self.store)?;
        let startup = startup_packet("postgres", "template1");
        self.io
            .push_input(&mut self.store, &self.env, &self.malloc, &startup)?;

        // The upstream lifecycle is already running by this point. These calls
        // open the Rust-owned direct wire-protocol transport on top of that
        // lifecycle; they must not grow into a second backend lifecycle.
        let port = self
            .protocol
            .get_port
            .call(&mut self.store)
            .context("pgl_getMyProcPort")?;
        ensure!(port > 0, "pgl_getMyProcPort returned null");

        let status = self
            .protocol
            .process_startup
            .call(&mut self.store, port, 1, 1)
            .context("ProcessStartupPacket")?;
        if status != 0 {
            let output = self
                .io
                .take_output(&mut self.store, &self.env, &self.malloc)?;
            bail!(
                "PGlite WASIX startup packet failed with status {status}; backend output: {}",
                summarize_protocol(&output)
            );
        }

        self.protocol
            .send_conn_data
            .call(&mut self.store)
            .context("pgl_sendConnData")?;
        self.protocol
            .pq_flush
            .call(&mut self.store)
            .context("pgl_pq_flush after startup")?;
        let _ = self
            .io
            .take_output(&mut self.store, &self.env, &self.malloc)?;
        self.started = true;
        Ok(())
    }

    fn send_one_protocol_message(&mut self, payload: &[u8]) -> Result<Vec<u8>> {
        if payload.is_empty() {
            return Ok(Vec::new());
        }

        self.io.reset(&mut self.store)?;
        self.io
            .push_input(&mut self.store, &self.env, &self.malloc, payload)?;

        if let Err(err) = self.protocol.main_loop.call(&mut self.store) {
            warn!("PostgresMainLoopOnce trapped; attempting protocol recovery: {err}");
            self.recover_protocol_error(payload.len())?;
        }

        self.protocol
            .send_ready
            .call(&mut self.store)
            .context("PostgresSendReadyForQueryIfNecessary")?;
        self.protocol
            .pq_flush
            .call(&mut self.store)
            .context("pgl_pq_flush after protocol message")?;
        let output = self
            .io
            .take_output(&mut self.store, &self.env, &self.malloc)
            .context("take backend output after protocol message")?;
        if is_simple_query_message(payload) && protocol_response_contains_error(&output) {
            self.recover_non_trapping_protocol_error()?;
        }
        Ok(output)
    }

    fn recover_protocol_error(&mut self, payload_len: usize) -> Result<()> {
        self.protocol
            .recover_error
            .call(&mut self.store)
            .context("PostgresRecoverProtocolError after protocol trap")?;

        // PostgreSQL extended-query errors skip messages until Sync. If Sync was
        // already in this host buffer, re-enter the loop to drain it and produce
        // ReadyForQuery from PostgreSQL rather than inventing one in Rust.
        let max_drain_attempts = (payload_len / 5).saturating_add(2).max(1);
        let mut drain_attempts = 0usize;
        while self.io.available(&mut self.store)? > 0 {
            drain_attempts += 1;
            ensure!(
                drain_attempts <= max_drain_attempts,
                "Postgres protocol recovery did not drain buffered input after {drain_attempts} attempts"
            );
            if let Err(drain_err) = self.protocol.main_loop.call(&mut self.store) {
                warn!("PostgresMainLoopOnce trapped while draining after recovery: {drain_err}");
                self.protocol
                    .recover_error
                    .call(&mut self.store)
                    .context("PostgresRecoverProtocolError while draining after protocol trap")?;
            }
        }
        Ok(())
    }

    fn recover_non_trapping_protocol_error(&mut self) -> Result<()> {
        self.protocol
            .recover_error
            .call(&mut self.store)
            .context("PostgresRecoverProtocolError after backend ErrorResponse")?;
        self.protocol
            .send_ready
            .call(&mut self.store)
            .context("PostgresSendReadyForQueryIfNecessary after backend ErrorResponse")?;
        self.protocol
            .pq_flush
            .call(&mut self.store)
            .context("pgl_pq_flush after backend ErrorResponse recovery")?;
        let _ = self
            .io
            .take_output(&mut self.store, &self.env, &self.malloc)?;
        Ok(())
    }
}

fn instantiate_wasix_module(
    runtime: &TokioRuntime,
    wasix_runtime: &Arc<dyn Runtime + Send + Sync>,
    store: &mut Store,
    runtime_root: &std::path::Path,
    module: Module,
) -> Result<(Instance, WasiFunctionEnv)> {
    let runtime_root = runtime_root
        .canonicalize()
        .with_context(|| format!("canonicalize runtime root {}", runtime_root.display()))?;
    let _guard = runtime.enter();
    let host_fs =
        virtual_fs::host_fs::FileSystem::new(tokio::runtime::Handle::current(), &runtime_root)
            .with_context(|| format!("create host fs rooted at {}", runtime_root.display()))?;
    let host_fs = Arc::new(host_fs) as Arc<dyn virtual_fs::FileSystem + Send + Sync>;
    let root_fs = WasiFsRoot::from_filesystem(host_fs);

    let mut runner = WasiRunner::new();
    runner.with_current_dir("/");
    if std::env::var_os("PGLITE_OXIDE_WASIX_STDIO").is_none() {
        runner
            .with_stdout(Box::<NullFile>::default())
            .with_stderr(Box::<NullFile>::default());
    }

    let wasi = Wasi::new(PGLITE_EXE_PATH);
    let mut builder = runner
        .prepare_webc_env(
            PGLITE_EXE_PATH,
            &wasi,
            PackageOrHash::Hash(ModuleHash::random()),
            RuntimeOrEngine::Runtime(wasix_runtime.clone()),
            Some(root_fs),
        )
        .context("prepare Wasmer/WASIX runner environment")?;
    add_pglite_env(&mut builder);
    add_pglite_args(&mut builder);

    builder
        .instantiate(module, store)
        .context("instantiate PGlite WASIX module")
}

fn build_wasix_runtime(
    runtime: &TokioRuntime,
    engine: &Engine,
    module_cache: Arc<SharedCache>,
) -> Arc<dyn Runtime + Send + Sync> {
    let _guard = runtime.enter();
    let task_manager = Arc::new(TokioTaskManager::new(runtime.handle().clone()));
    let mut wasix_runtime = PluggableRuntime::new(task_manager);
    wasix_runtime.set_engine(engine.clone());
    wasix_runtime.set_module_cache(module_cache);
    Arc::new(wasix_runtime)
}

fn preload_runtime_side_modules(
    runtime: &TokioRuntime,
    engine: &Engine,
    module_cache: &Arc<SharedCache>,
    runtime_root: &Path,
) -> Result<()> {
    let lib_dir = runtime_root.join("lib/postgresql");
    for (file_name, artifact_name) in RUNTIME_SIDE_MODULES {
        let library = lib_dir.join(file_name);
        ensure!(
            library.exists(),
            "runtime support module '{}' is not installed at {}",
            file_name,
            library.display()
        );

        seed_side_module_cache(
            runtime,
            engine,
            module_cache,
            &library,
            artifact_name,
            &format!("runtime support module '{file_name}'"),
        )?;
    }
    Ok(())
}

#[cfg(feature = "extensions")]
fn preload_installed_extension_side_modules(
    runtime: &TokioRuntime,
    engine: &Engine,
    module_cache: &Arc<SharedCache>,
    runtime_root: &Path,
) -> Result<()> {
    let lib_dir = runtime_root.join("lib/postgresql");
    for extension in super::extensions::ALL {
        let library = lib_dir.join(format!("{}.so", extension.sql_name()));
        if !library.exists() {
            continue;
        }
        seed_side_module_cache(
            runtime,
            engine,
            module_cache,
            &library,
            extension.aot_name(),
            &format!("installed extension '{}'", extension.sql_name()),
        )?;
    }
    Ok(())
}

fn seed_side_module_cache(
    runtime: &TokioRuntime,
    engine: &Engine,
    module_cache: &Arc<SharedCache>,
    library: &Path,
    artifact_name: &str,
    label: &str,
) -> Result<()> {
    let wasm =
        fs::read(library).with_context(|| format!("read side module {}", library.display()))?;
    let module_hash = ModuleHash::new(&wasm);
    let module = aot::load_artifact_module(engine, artifact_name)?;
    runtime
        .block_on(module_cache.save(module_hash, engine, &module))
        .with_context(|| format!("seed Wasmer module cache for {label} ({module_hash})"))?;
    Ok(())
}

impl PgliteLifecycleExports {
    fn load(store: &mut Store, instance: &Instance) -> Result<Self> {
        let pgl_initdb = typed_export(store, instance, "pgl_initdb")?;
        let pgl_backend = typed_export(store, instance, "pgl_backend")?;

        Ok(Self {
            pgl_initdb,
            pgl_backend,
        })
    }
}

impl WasixProtocolExports {
    fn load(store: &mut Store, instance: &Instance) -> Result<Self> {
        let get_port = typed_export(store, instance, "pgl_getMyProcPort")?;
        let process_startup = typed_export(store, instance, "ProcessStartupPacket")?;
        let send_conn_data = typed_export(store, instance, "pgl_sendConnData")?;
        let pq_flush = typed_export(store, instance, "pgl_pq_flush")?;
        let main_loop = typed_export(store, instance, "PostgresMainLoopOnce")?;
        let send_ready = typed_export(store, instance, "PostgresSendReadyForQueryIfNecessary")?;
        let recover_error = typed_export(store, instance, "PostgresRecoverProtocolError")?;

        Ok(Self {
            get_port,
            process_startup,
            send_conn_data,
            pq_flush,
            main_loop,
            send_ready,
            recover_error,
        })
    }
}

fn ensure_no_js_lifecycle_contract(instance: &Instance) -> Result<()> {
    for name in JS_EMSCRIPTEN_LIFECYCLE_EXPORTS {
        ensure!(
            instance.exports.get_function(name).is_err()
                && instance.exports.get_function(&format!("_{name}")).is_err(),
            "WASIX runtime exported JS/Emscripten lifecycle entrypoint {name}; Rust hosts must use _start + pgl_initdb + pgl_backend plus the explicit WASIX protocol ABI"
        );
    }
    Ok(())
}

impl WasixPgliteIo {
    fn new(store: &mut Store, instance: &Instance) -> Result<Self> {
        let io = Self {
            input_reset: typed_export(store, instance, "pgl_wasix_input_reset")?,
            input_write: typed_export(store, instance, "pgl_wasix_input_write")?,
            input_available: typed_export(store, instance, "pgl_wasix_input_available")?,
            output_reset: typed_export(store, instance, "pgl_wasix_output_reset")?,
            output_len: typed_export(store, instance, "pgl_wasix_output_len")?,
            output_read: typed_export(store, instance, "pgl_wasix_output_read")?,
        };
        io.reset(store)?;
        Ok(io)
    }

    fn reset(&self, store: &mut Store) -> Result<()> {
        ensure!(
            self.input_reset
                .call(&mut *store)
                .context("pgl_wasix_input_reset")?
                == 0,
            "pgl_wasix_input_reset failed"
        );
        ensure!(
            self.output_reset
                .call(&mut *store)
                .context("pgl_wasix_output_reset")?
                == 0,
            "pgl_wasix_output_reset failed"
        );
        Ok(())
    }

    fn push_input(
        &self,
        store: &mut Store,
        env: &WasiFunctionEnv,
        malloc: &TypedFunction<i32, i32>,
        bytes: &[u8],
    ) -> Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        let ptr = write_bytes(store, env, malloc, bytes)?;
        let written = self
            .input_write
            .call(&mut *store, ptr, bytes.len() as i32)
            .context("pgl_wasix_input_write")?;
        ensure!(
            written == bytes.len() as i32,
            "pgl_wasix_input_write wrote {written}, expected {}",
            bytes.len()
        );
        Ok(())
    }

    fn available(&self, store: &mut Store) -> Result<i32> {
        let available = self
            .input_available
            .call(store)
            .context("pgl_wasix_input_available")?;
        ensure!(
            available >= 0,
            "pgl_wasix_input_available returned negative length {available}"
        );
        Ok(available)
    }

    fn take_output(
        &self,
        store: &mut Store,
        env: &WasiFunctionEnv,
        malloc: &TypedFunction<i32, i32>,
    ) -> Result<Vec<u8>> {
        let len = self
            .output_len
            .call(&mut *store)
            .context("pgl_wasix_output_len")?;
        ensure!(
            len >= 0,
            "pgl_wasix_output_len returned negative length {len}"
        );
        if len == 0 {
            return Ok(Vec::new());
        }
        let ptr = malloc
            .call(&mut *store, len)
            .context("malloc for pgl_wasix_output_read")?;
        ensure!(ptr > 0, "malloc returned null for output read");
        let read = self
            .output_read
            .call(&mut *store, ptr, len)
            .context("pgl_wasix_output_read")?;
        ensure!(
            read >= 0 && read <= len,
            "invalid pgl_wasix_output_read length {read}"
        );

        let mut bytes = vec![0u8; read as usize];
        let view = env
            .data(&*store)
            .try_memory_view(&*store)
            .context("get WASIX memory view")?;
        view.read(ptr as u64, &mut bytes)
            .with_context(|| format!("read SQL output at 0x{ptr:x}"))?;
        ensure!(
            self.output_reset
                .call(&mut *store)
                .context("pgl_wasix_output_reset after read")?
                == 0,
            "pgl_wasix_output_reset after read failed"
        );
        Ok(bytes)
    }
}

fn typed_export<Args, Rets>(
    store: &mut Store,
    instance: &Instance,
    name: &str,
) -> Result<TypedFunction<Args, Rets>>
where
    Args: WasmTypeList,
    Rets: WasmTypeList,
{
    instance
        .exports
        .get_typed_function::<Args, Rets>(&mut *store, name)
        .or_else(|_| {
            instance
                .exports
                .get_typed_function::<Args, Rets>(&mut *store, &format!("_{name}"))
        })
        .with_context(|| format!("get {name} export"))
}

fn call_wasi_start(store: &mut Store, instance: &Instance) {
    // Match the pglite-bindings lifecycle: initialize the WASI command once,
    // then drive Postgres through pgl_initdb/pgl_backend and the protocol ABI.
    if let Ok(start) = instance
        .exports
        .get_typed_function::<(), ()>(&mut *store, "_start")
    {
        if let Err(err) = start.call(&mut *store) {
            warn!("_start trapped during WASIX startup and was ignored: {err}");
        }
    }
}

fn add_pglite_env(builder: &mut wasmer_wasix::WasiEnvBuilder) {
    for (key, value) in [
        ("PREFIX", WASM_PREFIX),
        ("PGDATA", PGDATA_DIR),
        ("PGUSER", "postgres"),
        ("PGDATABASE", "template1"),
        ("MODE", "REACT"),
        ("REPL", "N"),
        ("PGSYSCONFDIR", WASM_PREFIX),
        ("PGCLIENTENCODING", "UTF8"),
        ("LC_CTYPE", "C.UTF-8"),
        ("TZ", "UTC"),
        ("PGTZ", "UTC"),
        ("PG_COLOR", "never"),
    ] {
        builder.add_env(key, value);
    }
}

fn add_pglite_args(builder: &mut wasmer_wasix::WasiEnvBuilder) {
    for arg in [
        "--single",
        "-F",
        "-O",
        "-j",
        "-c",
        "search_path=public",
        "-c",
        "exit_on_error=false",
        "-c",
        "log_checkpoints=false",
        "-c",
        "max_worker_processes=0",
        "-c",
        "max_parallel_workers=0",
        "-c",
        "max_parallel_workers_per_gather=0",
        "-D",
        PGDATA_DIR,
        "template1",
    ] {
        builder.add_arg(arg);
    }
}

fn ensure_runtime_dirs(paths: &PglitePaths) -> Result<()> {
    for path in [
        paths.runtime_root(),
        paths.pgdata.clone(),
        paths.runtime_root().join("home"),
        paths.runtime_root().join("dev"),
        paths.runtime_root().join("dev/shm"),
        paths.runtime_root().join("tmp"),
    ] {
        fs::create_dir_all(&path).with_context(|| format!("create {}", path.display()))?;
    }

    let urandom = paths.runtime_root().join("dev/urandom");
    if !urandom.exists() {
        fs::write(&urandom, [42u8; 128]).with_context(|| format!("seed {}", urandom.display()))?;
    }
    for name in ["null", "stdout", "stderr", "zero"] {
        let path = paths.runtime_root().join("dev").join(name);
        if !path.exists() {
            fs::write(&path, []).with_context(|| format!("create {}", path.display()))?;
        }
    }
    Ok(())
}

fn startup_packet(user: &str, database: &str) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&196608i32.to_be_bytes());
    for (key, value) in [
        ("user", user),
        ("database", database),
        ("client_encoding", "UTF8"),
        ("DateStyle", "ISO, MDY"),
        ("TimeZone", "UTC"),
    ] {
        body.extend_from_slice(key.as_bytes());
        body.push(0);
        body.extend_from_slice(value.as_bytes());
        body.push(0);
    }
    body.push(0);

    let mut packet = Vec::with_capacity(body.len() + 4);
    packet.extend_from_slice(&((body.len() + 4) as i32).to_be_bytes());
    packet.extend_from_slice(&body);
    packet
}

fn split_frontend_messages(payload: &[u8]) -> Vec<&[u8]> {
    let mut messages = Vec::new();
    let mut cursor = 0usize;
    while cursor < payload.len() {
        let Some(len) = frontend_message_len(&payload[cursor..]) else {
            return vec![payload];
        };
        if len == 0 || cursor + len > payload.len() {
            return vec![payload];
        }
        messages.push(&payload[cursor..cursor + len]);
        cursor += len;
    }
    messages
}

fn is_simple_query_message(payload: &[u8]) -> bool {
    payload.first() == Some(&b'Q')
}

fn protocol_response_contains_error(response: &[u8]) -> bool {
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

fn frontend_message_len(buffer: &[u8]) -> Option<usize> {
    if buffer.is_empty() {
        return Some(0);
    }
    if buffer[0] == 0 {
        if buffer.len() < 4 {
            return None;
        }
        let len = i32::from_be_bytes(buffer[0..4].try_into().ok()?);
        return (len >= 8).then_some(len as usize);
    }
    if buffer.len() < 5 {
        return None;
    }
    let len = i32::from_be_bytes(buffer[1..5].try_into().ok()?);
    (len >= 4).then_some(1 + len as usize)
}

fn seed_exported_c_string_value(
    store: &mut Store,
    instance: &Instance,
    env: &WasiFunctionEnv,
    name: &str,
    value: &str,
) -> Result<()> {
    let Ok(global) = instance.exports.get_global(name) else {
        return Ok(());
    };
    let wasmer::Value::I32(ptr) = global.get(&mut *store) else {
        return Ok(());
    };
    if ptr <= 0 {
        return Ok(());
    }
    let mut bytes = value.as_bytes().to_vec();
    bytes.push(0);
    let view = env
        .data(&*store)
        .try_memory_view(&*store)
        .context("get WASIX memory view")?;
    view.write(ptr as u64, &bytes)
        .with_context(|| format!("seed {name} at 0x{ptr:x}"))?;
    Ok(())
}

fn write_bytes(
    store: &mut Store,
    env: &WasiFunctionEnv,
    malloc: &TypedFunction<i32, i32>,
    bytes: &[u8],
) -> Result<i32> {
    let ptr = malloc
        .call(&mut *store, bytes.len() as i32)
        .context("malloc for guest bytes")?;
    ensure!(ptr > 0, "malloc returned null for guest bytes");
    let view = env
        .data(&*store)
        .try_memory_view(&*store)
        .context("get WASIX memory view")?;
    view.write(ptr as u64, bytes)
        .with_context(|| format!("write guest bytes at 0x{ptr:x}"))?;
    Ok(ptr)
}

fn summarize_protocol(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return "0 bytes".to_owned();
    }

    let mut cursor = 0usize;
    let mut messages = Vec::new();
    while cursor + 5 <= bytes.len() {
        let tag = bytes[cursor] as char;
        let len = i32::from_be_bytes([
            bytes[cursor + 1],
            bytes[cursor + 2],
            bytes[cursor + 3],
            bytes[cursor + 4],
        ]);
        if len < 4 {
            messages.push(format!("{tag}(bad-len:{len})"));
            break;
        }
        let end = cursor + 1 + len as usize;
        if end > bytes.len() {
            messages.push(format!("{tag}(truncated:{len})"));
            break;
        }
        messages.push(format!("{tag}({} bytes)", len - 4));
        cursor = end;
    }
    if cursor < bytes.len() {
        messages.push(format!("tail:{} bytes", bytes.len() - cursor));
    }
    format!("{} bytes [{}]", bytes.len(), messages.join(", "))
}
