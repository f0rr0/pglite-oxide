use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, ensure};
use wasmer_types::ModuleHash;
use wasmer_wasix_eval::{
    CacheMode, WasmerModuleCompiler, cranelift_engine, print_engine_report,
};
use wasmer_wasix::runners::MappedDirectory;
use wasmer_wasix::runners::wasi::{RuntimeOrEngine, WasiRunner};

fn main() -> Result<()> {
    let args = Args::parse()?;
    let wasm = args
        .wasm
        .canonicalize()
        .with_context(|| format!("canonicalize {}", args.wasm.display()))?;
    let lib_dir = args
        .lib_dir
        .canonicalize()
        .with_context(|| format!("canonicalize {}", args.lib_dir.display()))?;

    ensure!(wasm.exists(), "main wasm does not exist: {}", wasm.display());
    ensure!(
        lib_dir.join("libneeded.so").exists(),
        "expected libneeded.so in {}",
        lib_dir.display()
    );
    ensure!(
        lib_dir.join("libdlopened.so").exists(),
        "expected libdlopened.so in {}",
        lib_dir.display()
    );

    let bytes = fs::read(&wasm).with_context(|| format!("read {}", wasm.display()))?;
    let engine = cranelift_engine();
    print_engine_report(&engine);
    let store = wasmer::Store::new(engine.clone());
    let compiler = WasmerModuleCompiler::new(args.cache_dir.clone(), args.cache_mode)?;
    let module = compiler
        .load_or_compile(&engine, &store, "wasix-dlopen-proof", &bytes)
        .context("compile dynamic WASIX main module")?
        .module;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("create Tokio runtime for host directory mapping")?;
    let _runtime_guard = runtime.enter();

    let mut runner = WasiRunner::new();
    runner.with_current_dir("/lib");
    runner.with_mapped_directories([MappedDirectory {
        host: lib_dir.clone(),
        guest: "/lib".to_owned(),
    }]);

    println!("running {} with /lib -> {}", wasm.display(), lib_dir.display());
    runner
        .run_wasm(
            RuntimeOrEngine::Engine(engine),
            "wasix-dlopen-proof",
            module,
            ModuleHash::sha256(&bytes),
        )
        .map_err(|err| anyhow!(err))
        .context("run dynamic WASIX proof through wasmer-wasix Rust API")?;

    Ok(())
}

struct Args {
    wasm: PathBuf,
    lib_dir: PathBuf,
    cache_dir: Option<PathBuf>,
    cache_mode: CacheMode,
}

impl Args {
    fn parse() -> Result<Self> {
        let default_lib_dir = Path::new("../wasix-dlopen-proof/build").to_path_buf();
        let mut wasm = None;
        let mut lib_dir = None;
        let mut cache_dir = Some(PathBuf::from(
            "../wasix-postgres-build/build/wasmer-module-cache",
        ));
        let mut cache_mode = CacheMode::Use;

        let mut args = env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
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
                "--no-cache" => {
                    cache_dir = None;
                    cache_mode = CacheMode::Off;
                }
                "-h" | "--help" => {
                    println!(
                        "usage: cargo run --bin run_wasix_dl -- [--cache-dir PATH] [--cache-mode use|rebuild|off] [MAIN_WASM] [LIB_DIR]"
                    );
                    std::process::exit(0);
                }
                value if wasm.is_none() => wasm = Some(PathBuf::from(value)),
                value if lib_dir.is_none() => lib_dir = Some(PathBuf::from(value)),
                other => anyhow::bail!("unknown argument: {other}"),
            }
        }

        let wasm = wasm.unwrap_or_else(|| default_lib_dir.join("main.wasm"));
        let lib_dir = lib_dir.unwrap_or(default_lib_dir);

        Ok(Self {
            wasm,
            lib_dir,
            cache_dir,
            cache_mode,
        })
    }
}
