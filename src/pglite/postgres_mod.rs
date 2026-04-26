use anyhow::{Context, Result, anyhow, bail, ensure};
use directories::ProjectDirs;
use getrandom::fill as fill_random;
use sha2::{Digest, Sha256};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use tracing::warn;
use wasmtime::OptLevel;
use wasmtime::{
    Config, Engine, Instance, Linker, Memory, Module, Store, TypedFunc, WasmParams, WasmResults,
};
use wasmtime_wasi::p1::{WasiP1Ctx, add_to_linker_sync};
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtxBuilder};

use super::base::PglitePaths;

const WASM_PREFIX: &str = "/tmp/pglite";
const PGDATA_DIR: &str = "/tmp/pglite/base";
const WASMTIME_CACHE_VERSION: &str = "wasmtime-44";
const WASMTIME_CONFIG_ID: &str = "opt-none-wasi-p1-v1";

pub struct PostgresMod {
    _engine: Engine,
    store: Store<State>,
    _instance: Instance,
    memory: Memory,
    exports: Exports,
    paths: PglitePaths,
    transport: TransportMode,
    wire_enabled: bool,
}

enum TransportMode {
    Cma {
        buffer_addr: usize,
        buffer_len: usize,
    },
    File,
}

struct State {
    wasi: WasiP1Ctx,
}

static ENGINE: LazyLock<Engine> = LazyLock::new(build_engine);
static MODULE_CACHE: LazyLock<Mutex<std::collections::HashMap<String, Module>>> =
    LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));

fn with_wasmtime_context<T>(
    result: std::result::Result<T, wasmtime::Error>,
    context: impl fmt::Display,
) -> Result<T> {
    result.map_err(|err| anyhow!("{context}: {err}"))
}

fn build_engine() -> Engine {
    let mut config = Config::new();

    config.cranelift_opt_level(OptLevel::None);

    #[cfg(feature = "runtime-cache")]
    match wasmtime::Cache::new(wasmtime::CacheConfig::new()) {
        Ok(cache) => {
            config.cache(Some(cache));
        }
        Err(err) => {
            warn!("failed to enable Wasmtime compile cache: {err}");
        }
    }

    Engine::new(&config).expect("failed to create Wasmtime engine")
}

fn module_cache_key(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let wasm_sha256 = format!("{:x}", hasher.finalize());
    let target = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
    format!("{WASMTIME_CACHE_VERSION}-{target}-{WASMTIME_CONFIG_ID}-{wasm_sha256}")
}

fn load_module(module_path: &Path) -> Result<(Engine, Module)> {
    let bytes = fs::read(module_path)
        .with_context(|| format!("failed to read {}", module_path.display()))?;
    let key = module_cache_key(&bytes);
    let engine = ENGINE.clone();

    if let Some(module) = MODULE_CACHE
        .lock()
        .map_err(|err| anyhow!("module cache lock poisoned: {err}"))?
        .get(&key)
        .cloned()
    {
        return Ok((engine, module));
    }

    let module = match load_serialized_module(&engine, &key) {
        Ok(Some(module)) => module,
        Ok(None) => compile_and_cache_module(&engine, module_path, &bytes, &key)?,
        Err(err) => {
            warn!("failed to read compiled module cache: {err:#}");
            compile_module(&engine, module_path, &bytes)?
        }
    };
    MODULE_CACHE
        .lock()
        .map_err(|err| anyhow!("module cache lock poisoned: {err}"))?
        .insert(key, module.clone());

    Ok((engine, module))
}

fn load_serialized_module(engine: &Engine, key: &str) -> Result<Option<Module>> {
    let Some(cache_path) = serialized_module_cache_path(key) else {
        return Ok(None);
    };
    if !cache_path.exists() {
        return Ok(None);
    }

    match deserialize_trusted_module_cache_file(engine, &cache_path) {
        Ok(module) => Ok(Some(module)),
        Err(err) => {
            warn!(
                "ignoring invalid compiled module cache {}: {err}",
                cache_path.display()
            );
            let _ = fs::remove_file(&cache_path);
            Ok(None)
        }
    }
}

#[allow(unsafe_code)]
fn deserialize_trusted_module_cache_file(
    engine: &Engine,
    cache_path: &Path,
) -> wasmtime::Result<Module> {
    // SAFETY: Wasmtime compiled modules are only deserialized from this crate's
    // private cache directory, and the file name is keyed by the runtime WASM
    // SHA-256, Wasmtime major version, target, and config id. Corrupt or stale
    // files are discarded and rebuilt by the caller.
    unsafe { Module::deserialize_file(engine, cache_path) }
}

fn compile_and_cache_module(
    engine: &Engine,
    module_path: &Path,
    bytes: &[u8],
    key: &str,
) -> Result<Module> {
    let module = compile_module(engine, module_path, bytes)?;
    let Some(cache_path) = serialized_module_cache_path(key) else {
        return Ok(module);
    };

    if let Err(err) = write_serialized_module(&module, &cache_path) {
        warn!(
            "failed to write compiled module cache {}: {err:#}",
            cache_path.display()
        );
    }
    Ok(module)
}

fn compile_module(engine: &Engine, module_path: &Path, bytes: &[u8]) -> Result<Module> {
    with_wasmtime_context(
        Module::from_binary(engine, bytes),
        format!("failed to compile {}", module_path.display()),
    )
}

fn serialized_module_cache_path(key: &str) -> Option<PathBuf> {
    ProjectDirs::from("dev", "pglite-oxide", "pglite-oxide").map(|dirs| {
        dirs.cache_dir()
            .join("cwasm")
            .join(format!("pglite-{key}.cwasm"))
    })
}

fn write_serialized_module(module: &Module, cache_path: &Path) -> Result<()> {
    if let Some(parent) = cache_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create module cache dir {}", parent.display()))?;
    }
    let bytes = with_wasmtime_context(module.serialize(), "serialize compiled pglite module")?;
    let tmp_path = cache_path.with_extension("cwasm.tmp");
    fs::write(&tmp_path, bytes)
        .with_context(|| format!("write compiled module cache {}", tmp_path.display()))?;
    fs::rename(&tmp_path, cache_path).with_context(|| {
        format!(
            "promote compiled module cache {} -> {}",
            tmp_path.display(),
            cache_path.display()
        )
    })?;
    Ok(())
}

struct Exports {
    pgl_initdb: TypedFunc<(), i32>,
    pgl_backend: TypedFunc<(), ()>,
    use_wire: TypedFunc<i32, ()>,
    interactive_write: TypedFunc<i32, ()>,
    interactive_one: TypedFunc<(), ()>,
    interactive_read: TypedFunc<(), i32>,
    get_channel: TypedFunc<(), i32>,
    get_buffer_size: TypedFunc<i32, i32>,
    get_buffer_addr: TypedFunc<i32, i32>,
}

impl PostgresMod {
    pub(crate) fn preload_module(module_path: &Path) -> Result<()> {
        let _ = load_module(module_path)?;
        Ok(())
    }

    pub fn new(paths: PglitePaths) -> Result<Self> {
        let module_path = paths.pgroot.join("pglite/bin/pglite.wasi");

        if !module_path.exists() {
            return Err(anyhow!(
                "pglite.wasi binary not found at {}",
                module_path.display()
            ));
        }

        let (engine, module) = load_module(&module_path)?;

        let mut linker: Linker<State> = Linker::new(&engine);
        with_wasmtime_context(
            add_to_linker_sync(&mut linker, |state| &mut state.wasi),
            "failed to add WASI to linker",
        )?;

        let wasi = build_wasi_ctx(&paths)?;
        let mut store = Store::new(&engine, State { wasi });

        let instance = with_wasmtime_context(
            linker.instantiate(&mut store, &module),
            "failed to instantiate pglite module",
        )?;

        let memory = instance
            .get_memory(&mut store, "memory")
            .context("pglite module is missing exported memory")?;

        if let Ok(start) = instance.get_typed_func::<(), ()>(&mut store, "_start")
            && let Err(err) = start.call(&mut store, ())
        {
            warn!("_start trapped during startup and was ignored: {err}");
        }

        let exports = Exports::load(&mut store, &instance)?;

        let channel_id = with_wasmtime_context(
            exports.get_channel.call(&mut store, ()),
            "call _get_channel",
        )?;
        let transport = if channel_id >= 0 {
            let addr = with_wasmtime_context(
                exports.get_buffer_addr.call(&mut store, channel_id),
                "call _get_buffer_addr",
            )?;
            let len = with_wasmtime_context(
                exports.get_buffer_size.call(&mut store, channel_id),
                "call _get_buffer_size",
            )?;
            ensure!(addr >= 0, "interactive buffer address is negative: {addr}");
            ensure!(len >= 0, "interactive buffer length is negative: {len}");
            TransportMode::Cma {
                buffer_addr: addr as usize,
                buffer_len: len as usize,
            }
        } else {
            TransportMode::File
        };

        Ok(Self {
            _engine: engine,
            store,
            _instance: instance,
            memory,
            exports,
            paths,
            transport,
            wire_enabled: false,
        })
    }

    pub fn paths(&self) -> &PglitePaths {
        &self.paths
    }

    pub fn ensure_cluster(&mut self) -> Result<()> {
        let had_cluster = self.paths.is_cluster_initialized();
        // PGlite uses this export for runtime setup as well as first-time
        // cluster creation, so existing clusters still need the call.
        let rc = self
            .exports
            .pgl_initdb
            .call(&mut self.store, ())
            .map_err(|err| anyhow!("failed to execute _pgl_initdb: {err}"))?;

        if rc != 0 {
            if self.paths.is_cluster_initialized() {
                if !had_cluster {
                    warn!("_pgl_initdb returned status {rc}, but PG_VERSION exists; continuing");
                }
                return Ok(());
            }
            return Err(anyhow!("_pgl_initdb returned non-zero status: {}", rc));
        }

        if !self.paths.is_cluster_initialized() {
            return Err(anyhow!(
                "_pgl_initdb returned success but PG_VERSION is missing"
            ));
        }

        Ok(())
    }

    pub fn buffer_addr(&self) -> Option<usize> {
        match self.transport {
            TransportMode::Cma { buffer_addr, .. } => Some(buffer_addr),
            TransportMode::File => None,
        }
    }

    pub fn buffer_len(&self) -> Option<usize> {
        match self.transport {
            TransportMode::Cma { buffer_len, .. } => Some(buffer_len),
            TransportMode::File => None,
        }
    }

    pub fn write_memory(&mut self, offset: usize, data: &[u8]) -> Result<()> {
        self.memory
            .write(&mut self.store, offset, data)
            .with_context(|| format!("write {} bytes at 0x{offset:x}", data.len()))
    }

    pub fn read_memory(&mut self, offset: usize, buf: &mut [u8]) -> Result<()> {
        self.memory
            .read(&mut self.store, offset, buf)
            .with_context(|| format!("read {} bytes at 0x{offset:x}", buf.len()))
    }

    pub fn interactive_write(&mut self, len: i32) -> Result<()> {
        self.exports
            .interactive_write
            .call(&mut self.store, len)
            .map_err(|err| anyhow!("call _interactive_write: {err}"))?;
        Ok(())
    }

    pub fn interactive_one(&mut self) -> Result<()> {
        self.exports
            .interactive_one
            .call(&mut self.store, ())
            .map_err(|err| anyhow!("call _interactive_one: {err}"))?;
        Ok(())
    }

    pub fn interactive_read(&mut self) -> Result<i32> {
        self.exports
            .interactive_read
            .call(&mut self.store, ())
            .map_err(|err| anyhow!("call _interactive_read: {err}"))
    }

    pub fn use_wire(&mut self, enabled: bool) -> Result<()> {
        self.exports
            .use_wire
            .call(&mut self.store, if enabled { 1 } else { 0 })
            .map_err(|err| anyhow!("call _use_wire: {err}"))?;
        self.wire_enabled = enabled;
        Ok(())
    }

    pub fn backend(&mut self) -> Result<()> {
        self.exports
            .pgl_backend
            .call(&mut self.store, ())
            .map_err(|err| anyhow!("call _pgl_backend: {err}"))?;
        Ok(())
    }
}

impl Exports {
    fn load(store: &mut Store<State>, instance: &Instance) -> Result<Self> {
        fn get_typed<P, R>(
            store: &mut Store<State>,
            instance: &Instance,
            names: &[&str],
        ) -> Result<TypedFunc<P, R>>
        where
            P: WasmParams,
            R: WasmResults,
        {
            for name in names {
                if let Ok(func) = instance.get_typed_func::<P, R>(&mut *store, name) {
                    return Ok(func);
                }
            }
            bail!("missing expected export {:?}", names)
        }

        let pgl_initdb = get_typed(store, instance, &["_pgl_initdb", "pgl_initdb"])?;
        let pgl_backend = get_typed(store, instance, &["_pgl_backend", "pgl_backend"])?;
        let use_wire = get_typed(store, instance, &["_use_wire", "use_wire"])?;
        let interactive_write = get_typed(
            store,
            instance,
            &["_interactive_write", "interactive_write"],
        )?;
        let interactive_one = get_typed(store, instance, &["_interactive_one", "interactive_one"])?;
        let interactive_read =
            get_typed(store, instance, &["_interactive_read", "interactive_read"])?;
        let get_channel = get_typed(store, instance, &["_get_channel", "get_channel"])?;
        let get_buffer_size = get_typed(store, instance, &["_get_buffer_size", "get_buffer_size"])?;
        let get_buffer_addr = get_typed(store, instance, &["_get_buffer_addr", "get_buffer_addr"])?;

        Ok(Self {
            pgl_initdb,
            pgl_backend,
            use_wire,
            interactive_write,
            interactive_one,
            interactive_read,
            get_channel,
            get_buffer_size,
            get_buffer_addr,
        })
    }
}

fn build_wasi_ctx(paths: &PglitePaths) -> Result<WasiP1Ctx> {
    ensure_runtime_dirs(paths)?;

    let mut builder = WasiCtxBuilder::new();

    builder
        .env("PREFIX", WASM_PREFIX)
        .env("PGDATA", PGDATA_DIR)
        .env("PGUSER", "postgres")
        .env("PGDATABASE", "template1")
        .env("MODE", "REACT")
        .env("REPL", "N")
        .env("PGSYSCONFDIR", WASM_PREFIX)
        .env("PGCLIENTENCODING", "UTF8")
        .env("LC_CTYPE", "C.UTF-8")
        .env("TZ", "UTC")
        .env("PGTZ", "UTC")
        .env("PG_COLOR", "never");

    builder.arg(format!("PGDATA={}", PGDATA_DIR));
    builder.arg(format!("PREFIX={}", WASM_PREFIX));
    builder.arg("PGUSER=postgres");
    builder.arg("PGDATABASE=template1");
    builder.arg("MODE=REACT");
    builder.arg("REPL=N");

    let host_tmp = paths.pgroot.clone();
    builder
        .preopened_dir(&host_tmp, "/tmp", DirPerms::all(), FilePerms::all())
        .map_err(|err| anyhow!("failed to preopen {} as /tmp: {err}", host_tmp.display()))?;

    let home_path = paths.pgroot.join("home");
    if !home_path.exists() {
        fs::create_dir_all(&home_path)
            .with_context(|| format!("failed to create {}", home_path.display()))?;
    }
    builder
        .preopened_dir(&home_path, "/home", DirPerms::all(), FilePerms::all())
        .map_err(|err| anyhow!("failed to preopen {} as /home: {err}", home_path.display()))?;

    builder
        .preopened_dir(
            &paths.pgdata,
            "/tmp/pglite/base",
            DirPerms::all(),
            FilePerms::all(),
        )
        .map_err(|err| {
            anyhow!(
                "failed to preopen {} as /tmp/pglite/base: {err}",
                paths.pgdata.display()
            )
        })?;

    let dev_path = paths.pgroot.join("dev");
    builder
        .preopened_dir(&dev_path, "/dev", DirPerms::all(), FilePerms::all())
        .map_err(|err| anyhow!("failed to preopen {} as /dev: {err}", dev_path.display()))?;

    Ok(builder.build_p1())
}

fn ensure_runtime_dirs(paths: &PglitePaths) -> Result<()> {
    let dev_path = paths.pgroot.join("dev");
    if !dev_path.exists() {
        std::fs::create_dir_all(&dev_path)
            .with_context(|| format!("failed to create {}", dev_path.display()))?;
    }
    let urandom = dev_path.join("urandom");
    if !urandom.exists() {
        let mut buf = [0u8; 128];
        fill_random(&mut buf).context("seed urandom")?;
        std::fs::write(&urandom, buf)
            .with_context(|| format!("failed to seed {}", urandom.display()))?;
    }

    if !paths.pgdata.exists() {
        std::fs::create_dir_all(&paths.pgdata)
            .with_context(|| format!("failed to create {}", paths.pgdata.display()))?;
    }

    Ok(())
}
