use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::io::{Cursor, Read};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result, anyhow, bail, ensure};
use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};
use tar::Archive;
use tempfile::TempDir;
use wasmer::{Engine, Module, Store};
use wasmer_wasix_eval::{
    CacheMode, EngineKind, WasmerModuleCompiler, print_engine_report_named,
};
use wasmer_types::ModuleHash;
use wasmer_wasix::{
    WasiFunctionEnv,
    runners::wasi::{PackageOrHash, RuntimeOrEngine, WasiRunner},
};
use wasmparser::{ExternalKind, Parser, Payload, TypeRef};
use zstd::stream::read::Decoder as ZstdDecoder;

const WASM_PREFIX: &str = "/";
const PGDATA_DIR: &str = "/base";
const PGLITE_EXE_PATH: &str = "/bin/pglite.wasi";
const VECTOR_GUEST_PATH: &str = "/lib/vector.so";
const VECTOR_GUEST_PATHS: &[&str] = &[VECTOR_GUEST_PATH, "/lib/postgresql/vector.so"];

fn main() -> Result<()> {
    let args = Args::parse()?;
    let repo_root = args.repo_root.canonicalize().with_context(|| {
        format!(
            "canonicalize repo root candidate {}",
            args.repo_root.display()
        )
    })?;

    let temp_parent = repo_root.join("spikes/wasix-postgres-build/build/wasmer-tmp");
    fs::create_dir_all(&temp_parent)
        .with_context(|| format!("create temp parent {}", temp_parent.display()))?;
    let work = TempDir::new_in(&temp_parent).context("create temp work dir")?;
    let pgroot = work.path().join("pgroot");
    let extroot = work.path().join("extensions");

    let pglite_wasi = if let Some(path) = args.main_wasm.clone() {
        path
    } else {
        unpack_runtime(&repo_root.join("assets/pglite-wasi.tar.zst"), &pgroot)?;
        pgroot.join("pglite/bin/pglite.wasi")
    };
    let extension_root = if args.side_so.is_some() {
        None
    } else {
        unpack_extension(&repo_root.join("assets/extensions/vector.tar.gz"), &extroot)?;
        Some(extroot.as_path())
    };
    let vector_so = if let Some(path) = args.side_so.clone() {
        path
    } else {
        extroot.join("lib/postgresql/vector.so")
    };

    ensure!(
        pglite_wasi.exists(),
        "pglite.wasi was not found at {}",
        pglite_wasi.display()
    );
    ensure!(
        vector_so.exists(),
        "vector.so was not found at {}",
        vector_so.display()
    );

    let main_label = args
        .main_wasm
        .as_ref()
        .and_then(|path| path.file_name())
        .and_then(|name| name.to_str())
        .unwrap_or("pglite.wasi")
        .to_owned();
    let pglite = inspect_wasm(main_label, &pglite_wasi)?;
    let vector = inspect_wasm("vector.so", &vector_so)?;

    print_module_report(&pglite);
    print_module_report(&vector);
    print_compatibility_report(&pglite, &vector);

    let engine = args.engine_kind.build()?;
    print_engine_report_named(args.engine_kind.name(), &engine);
    let cache_dir = args.cache_dir.clone().unwrap_or_else(|| {
        repo_root.join("spikes/wasix-postgres-build/build/wasmer-module-cache")
    });
    let compiler = WasmerModuleCompiler::new(Some(cache_dir), args.cache_mode)?;
    let mut store = Store::new(engine.clone());
    let pglite_module = compiler
        .load_or_compile(&engine, &store, &pglite.label, &pglite.bytes)?
        .module;
    let _vector_module = compiler
        .load_or_compile(&engine, &store, "vector.so", &vector.bytes)?
        .module;

    if args.skip_run {
        println!("wasmer-run: skipped by --skip-run");
    } else {
        run_current_pglite_with_wasmer(
            engine,
            &mut store,
            &repo_root,
            &pgroot,
            extension_root,
            &vector_so,
            pglite_module,
        )?;
    }

    if args.keep_temp {
        println!("kept temp dir: {}", work.keep().display());
    }

    Ok(())
}

#[derive(Debug)]
struct Args {
    repo_root: PathBuf,
    main_wasm: Option<PathBuf>,
    side_so: Option<PathBuf>,
    keep_temp: bool,
    skip_run: bool,
    cache_dir: Option<PathBuf>,
    cache_mode: CacheMode,
    engine_kind: EngineKind,
}

impl Args {
    fn parse() -> Result<Self> {
        let mut repo_root = PathBuf::from("../..");
        let mut main_wasm = None;
        let mut side_so = None;
        let mut keep_temp = false;
        let mut skip_run = false;
        let mut cache_dir = None;
        let mut cache_mode = CacheMode::Use;
        let mut engine_kind = EngineKind::Cranelift;

        let mut args = env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--repo-root" => {
                    let value = args
                        .next()
                        .ok_or_else(|| anyhow!("--repo-root requires a path"))?;
                    repo_root = PathBuf::from(value);
                }
                "--main-wasm" => {
                    let value = args
                        .next()
                        .ok_or_else(|| anyhow!("--main-wasm requires a path"))?;
                    main_wasm = Some(PathBuf::from(value));
                }
                "--side-so" => {
                    let value = args
                        .next()
                        .ok_or_else(|| anyhow!("--side-so requires a path"))?;
                    side_so = Some(PathBuf::from(value));
                }
                "--cache-dir" => {
                    let value = args
                        .next()
                        .ok_or_else(|| anyhow!("--cache-dir requires a path"))?;
                    cache_dir = Some(PathBuf::from(value));
                }
                "--cache-mode" => {
                    let value = args
                        .next()
                        .ok_or_else(|| anyhow!("--cache-mode requires use|rebuild|off"))?;
                    cache_mode = CacheMode::parse(&value)?;
                }
                "--engine" => {
                    let value = args
                        .next()
                        .ok_or_else(|| anyhow!("--engine requires cranelift|llvm|singlepass"))?;
                    engine_kind = EngineKind::parse(&value)?;
                }
                "--no-cache" => cache_mode = CacheMode::Off,
                "--keep-temp" => keep_temp = true,
                "--skip-run" => skip_run = true,
                "-h" | "--help" => {
                    println!(
                        "usage: cargo run -- [--repo-root PATH] [--main-wasm PATH] [--side-so PATH] [--engine cranelift|llvm|singlepass] [--cache-dir PATH] [--cache-mode use|rebuild|off] [--skip-run] [--keep-temp]"
                    );
                    std::process::exit(0);
                }
                other => bail!("unknown argument: {other}"),
            }
        }

        Ok(Self {
            repo_root,
            main_wasm,
            side_so,
            keep_temp,
            skip_run,
            cache_dir,
            cache_mode,
            engine_kind,
        })
    }
}

#[derive(Debug)]
struct ModuleInfo {
    label: String,
    path: PathBuf,
    sha256: String,
    bytes: Vec<u8>,
    has_dylink0: bool,
    custom_sections: Vec<String>,
    imports: Vec<ImportInfo>,
    exports: Vec<ExportInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ImportInfo {
    module: String,
    name: String,
    kind: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ExportInfo {
    name: String,
    kind: String,
}

fn inspect_wasm(label: impl Into<String>, path: &Path) -> Result<ModuleInfo> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let sha256 = hex_sha256(&bytes);
    let mut custom_sections = Vec::new();
    let mut imports = Vec::new();
    let mut exports = Vec::new();

    for payload in Parser::new(0).parse_all(&bytes) {
        match payload.context("parse wasm payload")? {
            Payload::CustomSection(section) => {
                custom_sections.push(section.name().to_owned());
            }
            Payload::ImportSection(section) => {
                for import in section.into_imports() {
                    let import = import.context("parse wasm import")?;
                    imports.push(ImportInfo {
                        module: import.module.to_owned(),
                        name: import.name.to_owned(),
                        kind: type_ref_name(&import.ty).to_owned(),
                    });
                }
            }
            Payload::ExportSection(section) => {
                for export in section {
                    let export = export.context("parse wasm export")?;
                    exports.push(ExportInfo {
                        name: export.name.to_owned(),
                        kind: external_kind_name(export.kind).to_owned(),
                    });
                }
            }
            _ => {}
        }
    }

    let has_dylink0 = custom_sections.iter().any(|name| name == "dylink.0");

    Ok(ModuleInfo {
        label: label.into(),
        path: path.to_path_buf(),
        sha256,
        bytes,
        has_dylink0,
        custom_sections,
        imports,
        exports,
    })
}

fn print_module_report(module: &ModuleInfo) {
    println!();
    println!("== {} ==", module.label);
    println!("path: {}", module.path.display());
    println!("sha256: {}", module.sha256);
    println!("bytes: {}", module.bytes.len());
    println!("dylink.0: {}", yes_no(module.has_dylink0));
    println!("imports: {}", module.imports.len());
    println!("exports: {}", module.exports.len());

    let custom_sections = module
        .custom_sections
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    println!(
        "custom-sections: {}",
        summarize(custom_sections.iter().map(String::as_str), 16)
    );

    let mut import_modules = BTreeMap::<&str, usize>::new();
    for import in &module.imports {
        *import_modules.entry(&import.module).or_default() += 1;
    }
    println!(
        "import-modules: {}",
        import_modules
            .iter()
            .map(|(name, count)| format!("{name}:{count}"))
            .collect::<Vec<_>>()
            .join(", ")
    );

    println!(
        "exports-sample: {}",
        summarize(module.exports.iter().map(|export| export.name.as_str()), 24)
    );
    println!(
        "imports-sample: {}",
        summarize(
            module
                .imports
                .iter()
                .map(|import| format!("{}.{}:{}", import.module, import.name, import.kind)),
            24,
        )
    );
}

fn print_compatibility_report(pglite: &ModuleInfo, vector: &ModuleInfo) {
    println!();
    println!("== dynamic extension compatibility ==");

    let pglite_exports = pglite
        .exports
        .iter()
        .map(|export| export.name.as_str())
        .collect::<BTreeSet<_>>();

    let host_abi_imports = BTreeSet::from([
        "memory",
        "__indirect_function_table",
        "__stack_pointer",
        "__memory_base",
        "__table_base",
    ]);
    let vector_env_imports = vector
        .imports
        .iter()
        .filter(|import| import.module == "env")
        .collect::<Vec<_>>();
    let missing_env_imports = vector_env_imports
        .iter()
        .filter(|import| {
            !host_abi_imports.contains(import.name.as_str())
                && !pglite_exports.contains(import.name.as_str())
        })
        .map(|import| import.name.as_str())
        .collect::<BTreeSet<_>>();
    let host_abi_import_count = vector_env_imports
        .iter()
        .filter(|import| host_abi_imports.contains(import.name.as_str()))
        .count();

    println!(
        "current pglite main module has dylink.0: {}",
        yes_no(pglite.has_dylink0)
    );
    println!(
        "vector side module has dylink.0: {}",
        yes_no(vector.has_dylink0)
    );

    for required in [
        "memory",
        "__indirect_function_table",
        "__stack_pointer",
        "__memory_base",
        "__table_base",
        "__tls_base",
    ] {
        println!(
            "main export {required}: {}",
            yes_no(pglite_exports.contains(required))
        );
    }

    println!("vector env imports: {}", vector_env_imports.len());
    println!(
        "vector env imports supplied by shared WASIX ABI/linker state: {host_abi_import_count}"
    );
    println!(
        "vector postgres-symbol env imports missing from current pglite exports: {}",
        missing_env_imports.len()
    );
    println!(
        "missing env import sample: {}",
        summarize(missing_env_imports.iter().copied(), 30)
    );

    let got_imports = vector
        .imports
        .iter()
        .filter(|import| import.module == "GOT.func" || import.module == "GOT.mem")
        .map(|import| format!("{}.{}", import.module, import.name))
        .collect::<BTreeSet<_>>();
    println!("vector GOT imports: {}", got_imports.len());
    println!(
        "vector GOT import sample: {}",
        summarize(got_imports.iter().map(String::as_str), 30)
    );

    if !pglite.has_dylink0 {
        println!(
            "verdict: current pglite.wasi is a static WASI module; Wasmer/WASIX will not treat it as a dynamic-linking main module."
        );
    } else if !missing_env_imports.is_empty() {
        println!(
            "verdict: pglite.wasi is dynamic, but it does not export enough symbols for vector.so."
        );
    } else {
        println!(
            "verdict: ABI shape looks plausible; the shared WASIX ABI imports must be supplied by the dynamic loader."
        );
    }
}

fn run_current_pglite_with_wasmer(
    engine: Engine,
    store: &mut Store,
    repo_root: &Path,
    pgroot: &Path,
    extension_root: Option<&Path>,
    vector_so: &Path,
    module: Module,
) -> Result<()> {
    println!();
    println!("== wasmer/wasi run probe ==");

    ensure_runtime_dirs(pgroot)?;
    prepare_pglite_runtime_tree(repo_root, pgroot)?;
    let pgroot = pgroot
        .canonicalize()
        .with_context(|| format!("canonicalize pgroot {}", pgroot.display()))?;
    println!("host pgroot: {}", pgroot.display());
    println!("host pgroot exists before map: {}", yes_no(pgroot.exists()));
    println!(
        "host /tmp preopen exists before map: {}",
        yes_no(pgroot.join("tmp").exists())
    );
    println!(
        "host pgdata exists before map: {}",
        yes_no(pgroot.join("tmp/pglite/base/PG_VERSION").exists())
    );
    let fs_root = pgroot.join("tmp/pglite");
    if let Some(extension_root) = extension_root {
        copy_tree(extension_root, &fs_root).with_context(|| {
            format!(
                "install extension archive tree {} into guest root {}",
                extension_root.display(),
                fs_root.display()
            )
        })?;
    } else {
        for guest_path in VECTOR_GUEST_PATHS {
            let guest_vector = fs_root.join(guest_path.strip_prefix('/').unwrap_or(guest_path));
            if let Some(parent) = guest_vector.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("create guest vector parent {}", parent.display()))?;
            }
            fs::copy(vector_so, &guest_vector).with_context(|| {
                format!(
                    "copy {} to guest extension path {}",
                    vector_so.display(),
                    guest_vector.display()
                )
            })?;
        }
        install_pgvector_extension_sql(repo_root, &fs_root)?;
    }
    mirror_configured_share_layout(&fs_root)?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("create Tokio runtime for Wasmer/WASIX host fs")?;
    let _runtime_guard = runtime.enter();

    let host_fs =
        wasmer_wasix::virtual_fs::host_fs::FileSystem::new(tokio::runtime::Handle::current(), &fs_root)
            .with_context(|| format!("create host fs rooted at {}", fs_root.display()))?;
    let host_fs =
        Arc::new(host_fs) as Arc<dyn wasmer_wasix::virtual_fs::FileSystem + Send + Sync>;

    let mut runner = WasiRunner::new();
    runner
        .with_mount("/".to_owned(), host_fs)
        .with_current_dir("/");

    let wasi = webc::metadata::annotations::Wasi::new(PGLITE_EXE_PATH);
    let mut builder = runner
        .prepare_webc_env(
            PGLITE_EXE_PATH,
            &wasi,
            PackageOrHash::Hash(ModuleHash::random()),
            RuntimeOrEngine::Engine(engine),
            None,
        )
        .context("prepare Wasmer/WASIX runner environment")?;
    add_pglite_env(&mut builder);
    add_pglite_args(&mut builder);

    let start = Instant::now();
    let (instance, _env) = match builder.instantiate(module, store) {
        Ok(value) => {
            println!(
                "wasmer-instantiate pglite.wasi: ok in {:.2?}",
                Instant::now().duration_since(start)
            );
            value
        }
        Err(err) => {
            println!(
                "wasmer-instantiate pglite.wasi: failed in {:.2?}: {err}",
                Instant::now().duration_since(start)
            );
            return Err(anyhow!(err)).context("instantiate pglite.wasi with Wasmer/WASIX");
        }
    };

    println!("wasmer-call __wasm_call_ctors: skipped; _start owns libc initialization");

    let malloc = instance
        .exports
        .get_typed_function::<i32, i32>(&mut *store, "malloc")
        .context("get malloc export")?;
    let io = WasixPgliteIo::new(store, &instance, &_env, &malloc)?;
    seed_exported_c_string_value(store, &instance, &_env, "my_exec_path", PGLITE_EXE_PATH)?;
    probe_single_user_start(store, &instance, &_env, &malloc)?;

    if let Ok(start_pglite) = instance
        .exports
        .get_typed_function::<(), ()>(&mut *store, "pgl_startPGlite")
        .or_else(|_| {
            instance
                .exports
                .get_typed_function::<(), ()>(&mut *store, "_pgl_startPGlite")
        })
    {
        match start_pglite.call(&mut *store) {
            Ok(_) => println!("wasmer-call pgl_startPGlite: ok"),
            Err(err) => println!("wasmer-call pgl_startPGlite: trapped/failed: {err}"),
        }
    } else {
        println!("wasmer-call pgl_startPGlite: export not present");
    }

    probe_sql_path(store, &instance, &_env, &malloc, io.as_ref())?;
    probe_dlopen_vector(store, &instance, &_env, &malloc)?;
    Ok(())
}

fn prepare_pglite_runtime_tree(repo_root: &Path, pgroot: &Path) -> Result<()> {
    let tmp_root = pgroot.join("tmp");
    unpack_runtime(&repo_root.join("assets/pglite-wasi.tar.zst"), &tmp_root)?;
    let pgdata = pgroot.join("tmp/pglite/base");
    if pgdata.join("PG_VERSION").exists() {
        return Ok(());
    }
    unpack_tar_safely(
        ZstdDecoder::new(
            fs::File::open(repo_root.join("assets/prepopulated/pgdata-template.tar.zst"))
                .context("open pgdata template archive")?,
        )
        .context("decode pgdata template archive")?,
        &repo_root.join("assets/prepopulated/pgdata-template.tar.zst"),
        &pgdata,
        false,
    )?;

    let config = pgdata.join("postgresql.conf");
    if config.exists() {
        let contents = fs::read_to_string(&config)
            .with_context(|| format!("read {}", config.display()))?
            .replace("log_timezone = UTC", "log_timezone = GMT")
            .replace("timezone = UTC", "timezone = GMT");
        fs::write(&config, contents).with_context(|| format!("write {}", config.display()))?;
    }
    for stale in ["postmaster.pid", "postmaster.opts"] {
        let path = pgdata.join(stale);
        if path.exists() {
            fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
        }
    }

    mirror_configured_share_layout(&tmp_root.join("pglite"))?;

    for tz_default in [
        tmp_root.join("pglite/share/postgresql/timezonesets/Default"),
        tmp_root.join("pglite/share/timezonesets/Default"),
    ] {
        if tz_default.exists() {
            fs::write(&tz_default, "UTC 0\nGMT 0\n")
                .with_context(|| format!("write minimal {}", tz_default.display()))?;
        }
    }

    Ok(())
}

fn mirror_configured_share_layout(fs_root: &Path) -> Result<()> {
    let share_root = fs_root.join("share");
    let upstream_share = share_root.join("postgresql");
    if upstream_share.exists() {
        copy_tree(&upstream_share, &share_root).with_context(|| {
            format!(
                "mirror {} into configured share root {}",
                upstream_share.display(),
                share_root.display()
            )
        })?;
    }

    Ok(())
}

fn install_pgvector_extension_sql(repo_root: &Path, fs_root: &Path) -> Result<()> {
    let source_root = repo_root.join("spikes/upstream/pgvector");
    let destination = fs_root.join("share/postgresql/extension");
    fs::create_dir_all(&destination)
        .with_context(|| format!("create {}", destination.display()))?;

    fs::copy(source_root.join("vector.control"), destination.join("vector.control"))
        .context("copy pgvector control file")?;

    for entry in fs::read_dir(source_root.join("sql")).context("read pgvector sql dir")? {
        let entry = entry.context("read pgvector sql dir entry")?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("sql") {
            fs::copy(&path, destination.join(entry.file_name())).with_context(|| {
                format!(
                    "copy pgvector sql {} to {}",
                    path.display(),
                    destination.display()
                )
            })?;
        }
    }

    Ok(())
}

fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    for entry in fs::read_dir(source).with_context(|| format!("read {}", source.display()))? {
        let entry = entry.with_context(|| format!("read entry in {}", source.display()))?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        let file_type = entry
            .file_type()
            .with_context(|| format!("read file type for {}", source_path.display()))?;

        if file_type.is_dir() {
            fs::create_dir_all(&destination_path)
                .with_context(|| format!("create {}", destination_path.display()))?;
            copy_tree(&source_path, &destination_path)?;
        } else if file_type.is_file() {
            if let Some(parent) = destination_path.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("create {}", parent.display()))?;
            }
            fs::copy(&source_path, &destination_path).with_context(|| {
                format!(
                    "copy {} to {}",
                    source_path.display(),
                    destination_path.display()
                )
            })?;
        }
    }

    Ok(())
}

fn probe_single_user_start(
    store: &mut Store,
    instance: &wasmer::Instance,
    env: &WasiFunctionEnv,
    malloc: &wasmer::TypedFunction<i32, i32>,
) -> Result<()> {
    if let Ok(set_active) = instance
        .exports
        .get_typed_function::<i32, i32>(&mut *store, "pgl_setPGliteActive")
        .or_else(|_| {
            instance
                .exports
                .get_typed_function::<i32, i32>(&mut *store, "_pgl_setPGliteActive")
        })
    {
        match set_active.call(&mut *store, 1) {
            Ok(value) => println!("wasmer-call pgl_setPGliteActive(1): returned {value}"),
            Err(err) => println!("wasmer-call pgl_setPGliteActive(1): trapped/failed: {err}"),
        }
    }

    if let Ok(start) = instance
        .exports
        .get_typed_function::<(), ()>(&mut *store, "_start")
    {
        match start.call(&mut *store) {
            Ok(()) => println!("wasmer-call _start single-user: returned"),
            Err(err) => println!("wasmer-call _start single-user: trapped/failed: {err}"),
        }
    } else if let Ok(main) = instance
        .exports
        .get_typed_function::<(i32, i32), i32>(&mut *store, "__main_argc_argv")
    {
        let args = pglite_start_args_with_program();
        let argv = write_argv(store, env, malloc, &args)?;
        match main.call(&mut *store, args.len() as i32, argv) {
            Ok(code) => println!("wasmer-call __main_argc_argv single-user: returned {code}"),
            Err(err) => println!("wasmer-call __main_argc_argv single-user: trapped/failed: {err}"),
        }
    } else if let Ok(main_void) = instance
        .exports
        .get_typed_function::<(), i32>(&mut *store, "__main_void")
    {
        match main_void.call(&mut *store) {
            Ok(code) => println!("wasmer-call __main_void single-user: returned {code}"),
            Err(err) => println!("wasmer-call __main_void single-user: trapped/failed: {err}"),
        }
    } else {
        println!("wasmer-call __main_void/_start/__main_argc_argv: export not present");
        return Ok(());
    }
    for name in ["DataDir", "ConfigFileName", "HbaFileName", "IdentFileName"] {
        if let Some(value) = read_exported_c_string_pointer(store, instance, env, name)? {
            println!("wasmer-global {name}: {value}");
        }
    }
    for name in ["my_exec_path", "pkglib_path"] {
        if let Some(value) = read_exported_c_string_value(store, instance, env, name)? {
            println!("wasmer-global {name}: {value}");
        }
    }
    for name in [
        "GOT.data.internal.my_exec_path",
        "GOT.data.internal.pkglib_path",
        "GOT.data.internal.ConfigFileName",
    ] {
        if let Some(value) = read_exported_c_string_pointer(store, instance, env, name)? {
            println!("wasmer-global {name}: {value}");
        }
    }

    Ok(())
}

fn pglite_start_args() -> [&'static str; 19] {
    [
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
    ]
}

fn pglite_start_args_with_program() -> [&'static str; 20] {
    [
        PGLITE_EXE_PATH,
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
    ]
}

struct WasixPgliteIo {
    input_reset: wasmer::TypedFunction<(), i32>,
    input_write: wasmer::TypedFunction<(i32, i32), i32>,
    output_reset: wasmer::TypedFunction<(), i32>,
    output_len: wasmer::TypedFunction<(), i32>,
    output_read: wasmer::TypedFunction<(i32, i32), i32>,
}

impl WasixPgliteIo {
    fn new(
        store: &mut Store,
        instance: &wasmer::Instance,
        env: &WasiFunctionEnv,
        malloc: &wasmer::TypedFunction<i32, i32>,
    ) -> Result<Option<Self>> {
        let Ok(input_reset) =
            typed_export::<(), i32>(store, instance, "pgl_wasix_input_reset")
        else {
            println!("wasmer-call pgl_wasix_* SQL I/O: exports not present");
            return Ok(None);
        };
        let input_write = typed_export::<(i32, i32), i32>(
            store,
            instance,
            "pgl_wasix_input_write",
        )
        .context("get pgl_wasix_input_write export")?;
        let output_reset =
            typed_export::<(), i32>(store, instance, "pgl_wasix_output_reset")
                .context("get pgl_wasix_output_reset export")?;
        let output_len = typed_export::<(), i32>(store, instance, "pgl_wasix_output_len")
            .context("get pgl_wasix_output_len export")?;
        let output_read =
            typed_export::<(i32, i32), i32>(store, instance, "pgl_wasix_output_read")
                .context("get pgl_wasix_output_read export")?;

        let io = Self {
            input_reset,
            input_write,
            output_reset,
            output_len,
            output_read,
        };
        io.reset(store)?;
        let smoke = io.take_output(store, env, malloc)?;
        println!(
            "wasmer-call pgl_wasix_* SQL I/O: ready; initial buffered output={} bytes",
            smoke.len()
        );
        Ok(Some(io))
    }

    fn reset(&self, store: &mut Store) -> Result<()> {
        ensure!(
            self.input_reset.call(&mut *store).context("pgl_wasix_input_reset")? == 0,
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
        malloc: &wasmer::TypedFunction<i32, i32>,
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

    fn take_output(
        &self,
        store: &mut Store,
        env: &WasiFunctionEnv,
        malloc: &wasmer::TypedFunction<i32, i32>,
    ) -> Result<Vec<u8>> {
        let len = self.output_len.call(&mut *store).context("pgl_wasix_output_len")?;
        ensure!(len >= 0, "pgl_wasix_output_len returned negative length {len}");
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
        ensure!(read >= 0 && read <= len, "invalid pgl_wasix_output_read length {read}");

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
    instance: &wasmer::Instance,
    name: &str,
) -> Result<wasmer::TypedFunction<Args, Rets>>
where
    Args: wasmer::WasmTypeList,
    Rets: wasmer::WasmTypeList,
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

fn probe_sql_path(
    store: &mut Store,
    instance: &wasmer::Instance,
    env: &WasiFunctionEnv,
    malloc: &wasmer::TypedFunction<i32, i32>,
    io: Option<&WasixPgliteIo>,
) -> Result<()> {
    let Some(io) = io else {
        println!("wasmer-sql-probe: skipped; pgl_wasix_* I/O exports are unavailable");
        return Ok(());
    };

    println!();
    println!("== pglite sql probe ==");

    let Ok(get_port) = typed_export::<(), i32>(store, instance, "pgl_getMyProcPort") else {
        println!("wasmer-sql-probe: skipped; pgl_getMyProcPort export not present");
        return Ok(());
    };
    let process_startup = typed_export::<(i32, i32, i32), i32>(
        store,
        instance,
        "ProcessStartupPacket",
    )
    .context("get ProcessStartupPacket export")?;
    let send_conn_data =
        typed_export::<(), ()>(store, instance, "pgl_sendConnData").context("get pgl_sendConnData")?;
    let pq_flush = typed_export::<(), ()>(store, instance, "pgl_pq_flush")
        .context("get pgl_pq_flush")?;
    let main_loop = typed_export::<(), ()>(store, instance, "PostgresMainLoopOnce")
        .context("get PostgresMainLoopOnce")?;
    let send_ready = typed_export::<(), ()>(
        store,
        instance,
        "PostgresSendReadyForQueryIfNecessary",
    )
    .context("get PostgresSendReadyForQueryIfNecessary")?;

    io.reset(store)?;
    io.push_input(store, env, malloc, &startup_packet("postgres", "template1"))?;
    let port = get_port.call(&mut *store).context("pgl_getMyProcPort")?;
    ensure!(port > 0, "pgl_getMyProcPort returned null");
    let startup_status = process_startup
        .call(&mut *store, port, 1, 1)
        .context("ProcessStartupPacket")?;
    println!("wasmer-sql-probe ProcessStartupPacket: returned {startup_status}");
    if startup_status != 0 {
        println!(
            "wasmer-sql-probe startup output: {}",
            summarize_protocol(&io.take_output(store, env, malloc)?)
        );
        return Ok(());
    }
    send_conn_data.call(&mut *store).context("pgl_sendConnData")?;
    pq_flush.call(&mut *store).context("pgl_pq_flush after startup")?;
    println!(
        "wasmer-sql-probe startup output: {}",
        summarize_protocol(&io.take_output(store, env, malloc)?)
    );

    for sql in [
        "SELECT 1;",
        "CREATE EXTENSION IF NOT EXISTS vector;",
        "CREATE TEMP TABLE oxide_vec (embedding vector(3));",
        "INSERT INTO oxide_vec VALUES ('[1,2,3]');",
        "SELECT embedding <-> '[1,2,4]'::vector AS distance FROM oxide_vec;",
    ] {
        io.reset(store)?;
        io.push_input(store, env, malloc, &simple_query_packet(sql))?;
        match main_loop.call(&mut *store) {
            Ok(_) => println!("wasmer-sql-probe query `{sql}`: main loop returned"),
            Err(err) => {
                println!("wasmer-sql-probe query `{sql}`: main loop trapped/failed: {err}");
                if let Ok(longjmp) =
                    typed_export::<(), ()>(store, instance, "PostgresMainLongJmp")
                {
                    match longjmp.call(&mut *store) {
                        Ok(_) => println!("wasmer-sql-probe PostgresMainLongJmp: ok"),
                        Err(err) => println!(
                            "wasmer-sql-probe PostgresMainLongJmp trapped/failed: {err}"
                        ),
                    }
                }
            }
        }
        send_ready
            .call(&mut *store)
            .context("PostgresSendReadyForQueryIfNecessary")?;
        pq_flush.call(&mut *store).context("pgl_pq_flush after query")?;
        let output = io.take_output(store, env, malloc)?;
        println!(
            "wasmer-sql-probe query `{sql}` output: {}",
            summarize_protocol(&output)
        );
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

fn simple_query_packet(sql: &str) -> Vec<u8> {
    let mut body = sql.as_bytes().to_vec();
    body.push(0);

    let mut packet = Vec::with_capacity(body.len() + 5);
    packet.push(b'Q');
    packet.extend_from_slice(&((body.len() + 4) as i32).to_be_bytes());
    packet.extend_from_slice(&body);
    packet
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
        let body = &bytes[cursor + 5..end];
        messages.push(match tag {
            'C' => format!("C({})", cstr_lossy(body)),
            'E' => format!("E({})", protocol_error_summary(body)),
            'N' => format!("N({})", protocol_error_summary(body)),
            'R' => {
                let code = body
                    .get(0..4)
                    .map(|bytes| i32::from_be_bytes(bytes.try_into().unwrap()))
                    .unwrap_or(-1);
                format!("R({code})")
            }
            'S' => format!("S({})", cstr_lossy(body)),
            'T' => "T(row-description)".to_owned(),
            'D' => "D(data-row)".to_owned(),
            'Z' => body
                .first()
                .map(|status| format!("Z({})", *status as char))
                .unwrap_or_else(|| "Z".to_owned()),
            other => format!("{other}({} bytes)", body.len()),
        });
        cursor = end;
    }
    if cursor < bytes.len() {
        messages.push(format!("tail:{} bytes", bytes.len() - cursor));
    }
    format!("{} bytes [{}]", bytes.len(), messages.join(", "))
}

fn protocol_error_summary(body: &[u8]) -> String {
    let mut severity = None;
    let mut message = None;
    let mut cursor = 0usize;
    while cursor < body.len() {
        let field = body[cursor];
        cursor += 1;
        if field == 0 {
            break;
        }
        let start = cursor;
        while cursor < body.len() && body[cursor] != 0 {
            cursor += 1;
        }
        let value = String::from_utf8_lossy(&body[start..cursor]).into_owned();
        if cursor < body.len() {
            cursor += 1;
        }
        match field as char {
            'S' => severity = Some(value),
            'M' => message = Some(value),
            _ => {}
        }
    }
    match (severity, message) {
        (Some(severity), Some(message)) => format!("{severity}: {message}"),
        (_, Some(message)) => message,
        _ => format!("{} bytes", body.len()),
    }
}

fn cstr_lossy(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|byte| *byte == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

fn read_exported_c_string_pointer(
    store: &mut Store,
    instance: &wasmer::Instance,
    env: &WasiFunctionEnv,
    name: &str,
) -> Result<Option<String>> {
    let Ok(global) = instance.exports.get_global(name) else {
        return Ok(None);
    };
    let wasmer::Value::I32(slot) = global.get(&mut *store) else {
        return Ok(Some("<non-i32 global>".to_owned()));
    };
    if slot <= 0 {
        return Ok(Some("null slot".to_owned()));
    }

    let mut pointer_bytes = [0u8; 4];
    let view = env
        .data(&*store)
        .try_memory_view(&*store)
        .context("get WASIX memory view")?;
    view.read(slot as u64, &mut pointer_bytes)
        .with_context(|| format!("read {name} pointer slot at 0x{slot:x}"))?;
    let ptr = i32::from_le_bytes(pointer_bytes);
    if ptr <= 0 {
        return Ok(Some(format!("slot=0x{slot:x} ptr=null")));
    }
    Ok(Some(format!(
        "slot=0x{slot:x} ptr=0x{ptr:x} {}",
        read_c_string(store, env, ptr as u64)?
    )))
}

fn read_exported_c_string_value(
    store: &mut Store,
    instance: &wasmer::Instance,
    env: &WasiFunctionEnv,
    name: &str,
) -> Result<Option<String>> {
    let Ok(global) = instance.exports.get_global(name) else {
        return Ok(None);
    };
    let wasmer::Value::I32(ptr) = global.get(&mut *store) else {
        return Ok(Some("<non-i32 global>".to_owned()));
    };
    if ptr <= 0 {
        return Ok(Some("ptr=null".to_owned()));
    }
    Ok(Some(format!(
        "ptr=0x{ptr:x} {}",
        read_c_string(store, env, ptr as u64)?
    )))
}

fn seed_exported_c_string_value(
    store: &mut Store,
    instance: &wasmer::Instance,
    env: &WasiFunctionEnv,
    name: &str,
    value: &str,
) -> Result<()> {
    let Ok(global) = instance.exports.get_global(name) else {
        println!("wasmer-global {name}: seed skipped; export not present");
        return Ok(());
    };
    let wasmer::Value::I32(ptr) = global.get(&mut *store) else {
        println!("wasmer-global {name}: seed skipped; non-i32 global");
        return Ok(());
    };
    if ptr <= 0 {
        println!("wasmer-global {name}: seed skipped; null pointer");
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
    println!("wasmer-global {name}: seeded {value}");
    Ok(())
}

fn probe_dlopen_vector(
    store: &mut Store,
    instance: &wasmer::Instance,
    env: &WasiFunctionEnv,
    malloc: &wasmer::TypedFunction<i32, i32>,
) -> Result<()> {
    const RTLD_NOW: i32 = 2;
    const RTLD_GLOBAL: i32 = 0x100;

    let dlopen = instance
        .exports
        .get_typed_function::<(i32, i32), i32>(&mut *store, "dlopen")
        .context("get dlopen export")?;
    let dlsym = instance
        .exports
        .get_typed_function::<(i32, i32), i32>(&mut *store, "dlsym")
        .context("get dlsym export")?;

    let path = write_c_string(store, env, &malloc, VECTOR_GUEST_PATH)?;
    let handle = dlopen
        .call(&mut *store, path, RTLD_NOW | RTLD_GLOBAL)
        .with_context(|| format!("call dlopen('{VECTOR_GUEST_PATH}')"))?;
    if handle == 0 {
        bail!(
            "dlopen('{VECTOR_GUEST_PATH}') returned null: {}",
            dlerror(store, instance, env)?
        );
    }
    println!("wasmer-call dlopen {VECTOR_GUEST_PATH}: handle=0x{handle:x}");

    for symbol_name in ["Pg_magic_func", "pg_finfo_vector_in", "vector_in"] {
        let symbol = write_c_string(store, env, &malloc, symbol_name)?;
        let address = dlsym
            .call(&mut *store, handle, symbol)
            .with_context(|| format!("call dlsym('{symbol_name}')"))?;
        if address == 0 {
            bail!(
                "dlsym('{symbol_name}') returned null: {}",
                dlerror(store, instance, env)?
            );
        }
        println!("wasmer-call dlsym {symbol_name}: 0x{address:x}");
    }

    probe_dfmgr_load_external_function(store, instance, env, &malloc)?;

    Ok(())
}

fn probe_dfmgr_load_external_function(
    store: &mut Store,
    instance: &wasmer::Instance,
    env: &WasiFunctionEnv,
    malloc: &wasmer::TypedFunction<i32, i32>,
) -> Result<()> {
    let Ok(load_external_function) = instance
        .exports
        .get_typed_function::<(i32, i32, i32, i32), i32>(&mut *store, "load_external_function")
    else {
        println!("wasmer-call load_external_function: export not present");
        return Ok(());
    };

    let filename = write_c_string(store, env, malloc, VECTOR_GUEST_PATH)?;
    let funcname = write_c_string(store, env, malloc, "vector_in")?;
    match load_external_function.call(&mut *store, filename, funcname, 1, 0) {
        Ok(address) if address != 0 => {
            println!(
                "wasmer-call load_external_function {VECTOR_GUEST_PATH} vector_in: 0x{address:x}"
            );
        }
        Ok(_) => {
            println!(
                "wasmer-call load_external_function {VECTOR_GUEST_PATH} vector_in: returned null"
            );
        }
        Err(err) => {
            println!(
                "wasmer-call load_external_function {VECTOR_GUEST_PATH} vector_in: trapped/failed: {err}"
            );
        }
    }

    Ok(())
}

fn write_argv(
    store: &mut Store,
    env: &WasiFunctionEnv,
    malloc: &wasmer::TypedFunction<i32, i32>,
    args: &[&str],
) -> Result<i32> {
    let mut ptrs = Vec::with_capacity(args.len() + 1);
    for arg in args {
        ptrs.push(write_c_string(store, env, malloc, arg)?);
    }
    ptrs.push(0);

    let bytes_len = ptrs.len() * std::mem::size_of::<i32>();
    let argv = malloc
        .call(&mut *store, bytes_len as i32)
        .context("malloc argv")?;
    ensure!(argv > 0, "malloc returned null for argv");

    let bytes = ptrs
        .into_iter()
        .flat_map(i32::to_le_bytes)
        .collect::<Vec<_>>();
    let view = env
        .data(&*store)
        .try_memory_view(&*store)
        .context("get WASIX memory view")?;
    view.write(argv as u64, &bytes)
        .with_context(|| format!("write argv at 0x{argv:x}"))?;
    Ok(argv)
}

fn write_c_string(
    store: &mut Store,
    env: &WasiFunctionEnv,
    malloc: &wasmer::TypedFunction<i32, i32>,
    value: &str,
) -> Result<i32> {
    let mut bytes = value.as_bytes().to_vec();
    bytes.push(0);
    write_bytes(store, env, malloc, &bytes)
        .with_context(|| format!("write c string '{value}'"))
}

fn write_bytes(
    store: &mut Store,
    env: &WasiFunctionEnv,
    malloc: &wasmer::TypedFunction<i32, i32>,
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
    view.write(ptr as u64, &bytes)
        .with_context(|| format!("write guest bytes at 0x{ptr:x}"))?;
    Ok(ptr)
}

fn dlerror(store: &mut Store, instance: &wasmer::Instance, env: &WasiFunctionEnv) -> Result<String> {
    let Ok(dlerror) = instance.exports.get_typed_function::<(), i32>(&mut *store, "dlerror") else {
        return Ok("dlerror export not present".to_owned());
    };
    let ptr = dlerror.call(&mut *store).context("call dlerror")?;
    if ptr == 0 {
        return Ok("dlerror returned null".to_owned());
    }
    read_c_string(store, env, ptr as u64)
}

fn read_c_string(store: &mut Store, env: &WasiFunctionEnv, ptr: u64) -> Result<String> {
    let view = env
        .data(&*store)
        .try_memory_view(&*store)
        .context("get WASIX memory view")?;
    let mut bytes = Vec::new();
    for offset in 0..4096 {
        let mut byte = [0u8];
        view.read(ptr + offset, &mut byte)
            .with_context(|| format!("read dlerror byte at 0x{:x}", ptr + offset))?;
        if byte[0] == 0 {
            return Ok(String::from_utf8_lossy(&bytes).into_owned());
        }
        bytes.push(byte[0]);
    }
    Ok(format!("unterminated string at 0x{ptr:x}"))
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
    for arg in pglite_start_args() {
        builder.add_arg(arg);
    }
}

fn ensure_runtime_dirs(pgroot: &Path) -> Result<()> {
    fs::create_dir_all(pgroot.join("tmp/pglite/base"))
        .with_context(|| format!("create {}", pgroot.join("tmp/pglite/base").display()))?;
    fs::create_dir_all(pgroot.join("tmp/pglite/home"))
        .with_context(|| format!("create {}", pgroot.join("tmp/pglite/home").display()))?;
    fs::create_dir_all(pgroot.join("tmp/pglite/dev"))
        .with_context(|| format!("create {}", pgroot.join("tmp/pglite/dev").display()))?;
    fs::create_dir_all(pgroot.join("tmp"))
        .with_context(|| format!("create {}", pgroot.join("tmp").display()))?;

    let urandom = pgroot.join("tmp/pglite/dev/urandom");
    if !urandom.exists() {
        fs::write(&urandom, [42u8; 128]).with_context(|| format!("seed {}", urandom.display()))?;
    }

    Ok(())
}

fn unpack_runtime(archive_path: &Path, destination: &Path) -> Result<()> {
    let file = fs::File::open(archive_path)
        .with_context(|| format!("open runtime archive {}", archive_path.display()))?;
    let decoder = ZstdDecoder::new(file)
        .with_context(|| format!("decode runtime archive {}", archive_path.display()))?;
    unpack_tar_safely(decoder, archive_path, destination, true)
}

fn unpack_extension(archive_path: &Path, destination: &Path) -> Result<()> {
    let file = fs::File::open(archive_path)
        .with_context(|| format!("open extension archive {}", archive_path.display()))?;
    let decoder = GzDecoder::new(file);
    unpack_tar_safely(decoder, archive_path, destination, false)
}

fn unpack_tar_safely<R: Read>(
    reader: R,
    archive_path: &Path,
    destination: &Path,
    strip_tmp_prefix: bool,
) -> Result<()> {
    fs::create_dir_all(destination)
        .with_context(|| format!("create unpack destination {}", destination.display()))?;
    let mut archive = Archive::new(reader);

    for entry in archive
        .entries()
        .with_context(|| format!("read entries from {}", archive_path.display()))?
    {
        let mut entry =
            entry.with_context(|| format!("read entry from {}", archive_path.display()))?;
        let path = entry
            .path()
            .with_context(|| format!("read entry path from {}", archive_path.display()))?
            .into_owned();

        let relative = if strip_tmp_prefix {
            path.strip_prefix("tmp").unwrap_or(&path)
        } else {
            path.as_path()
        };
        let dest = safe_destination(destination, relative)?;

        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create directory {}", parent.display()))?;
        }
        entry
            .unpack(&dest)
            .with_context(|| format!("unpack {} to {}", path.display(), dest.display()))?;
    }

    Ok(())
}

fn safe_destination(root: &Path, archive_path: &Path) -> Result<PathBuf> {
    let mut dest = root.to_path_buf();
    for component in archive_path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => dest.push(part),
            _ => bail!("unsafe archive path {}", archive_path.display()),
        }
    }
    Ok(dest)
}

fn type_ref_name(ty: &TypeRef) -> &'static str {
    match ty {
        TypeRef::Func(_) => "function",
        TypeRef::FuncExact(_) => "function",
        TypeRef::Table(_) => "table",
        TypeRef::Memory(_) => "memory",
        TypeRef::Global(_) => "global",
        TypeRef::Tag(_) => "tag",
    }
}

fn external_kind_name(kind: ExternalKind) -> &'static str {
    match kind {
        ExternalKind::Func => "function",
        ExternalKind::FuncExact => "function",
        ExternalKind::Table => "table",
        ExternalKind::Memory => "memory",
        ExternalKind::Global => "global",
        ExternalKind::Tag => "tag",
    }
}

fn summarize<S, I>(values: I, max: usize) -> String
where
    S: AsRef<str>,
    I: IntoIterator<Item = S>,
{
    let values = values
        .into_iter()
        .map(|value| value.as_ref().to_owned())
        .collect::<Vec<_>>();
    if values.is_empty() {
        return "(none)".to_owned();
    }

    let mut rendered = values.iter().take(max).cloned().collect::<Vec<_>>();
    if values.len() > max {
        rendered.push(format!("... +{}", values.len() - max));
    }
    rendered.join(", ")
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn hex_sha256(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(Cursor::new(bytes).get_ref());
    format!("{:x}", hasher.finalize())
}
