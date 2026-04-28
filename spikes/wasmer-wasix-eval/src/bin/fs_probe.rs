use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use wasmer::Store;
use wasmer_types::ModuleHash;
use wasmer_wasix::runners::wasi::{RuntimeOrEngine, WasiRunner};

use wasmer_wasix_eval::{CacheMode, EngineKind, WasmerModuleCompiler};

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut wasm_path = None;
    let mut fs_root = None;
    let mut mount = "/".to_owned();
    let mut cwd = "/".to_owned();
    let mut program = "/bin/fs_probe.wasi".to_owned();
    let mut guest_args = Vec::new();
    let mut guest_env = Vec::new();
    let mut cache_mode = CacheMode::Use;
    let mut cache_dir = PathBuf::from("spikes/wasix-postgres-build/build/wasmer-module-cache");
    let mut engine_kind = EngineKind::Cranelift;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--wasm" => wasm_path = args.next().map(PathBuf::from),
            "--fs-root" => fs_root = args.next().map(PathBuf::from),
            "--mount" => mount = args.next().context("--mount requires a value")?,
            "--cwd" => cwd = args.next().context("--cwd requires a value")?,
            "--program" => program = args.next().context("--program requires a value")?,
            "--arg" => guest_args.push(args.next().context("--arg requires a value")?),
            "--env" => {
                let value = args.next().context("--env requires KEY=VALUE")?;
                let (key, value) = value
                    .split_once('=')
                    .context("--env requires KEY=VALUE")?;
                guest_env.push((key.to_owned(), value.to_owned()));
            }
            "--cache-mode" => {
                cache_mode =
                    CacheMode::parse(&args.next().context("--cache-mode requires a value")?)?;
            }
            "--cache-dir" => {
                cache_dir = PathBuf::from(args.next().context("--cache-dir requires a value")?);
            }
            "--engine" => {
                engine_kind =
                    EngineKind::parse(&args.next().context("--engine requires a value")?)?;
            }
            "--help" | "-h" => {
                print_usage();
                return Ok(());
            }
            other => anyhow::bail!("unknown argument {other}; use --help"),
        }
    }

    let wasm_path = wasm_path.context("--wasm is required")?;
    let fs_root = fs_root.context("--fs-root is required")?.canonicalize()?;
    let wasm = std::fs::read(&wasm_path)
        .with_context(|| format!("read wasm module {}", wasm_path.display()))?;

    println!("probe wasm: {}", wasm_path.display());
    println!("host fs root: {}", fs_root.display());
    println!("guest mount: {mount}");
    println!("guest cwd: {cwd}");
    println!("program: {program}");
    println!("guest args: {}", guest_args.join(" "));

    let engine = engine_kind.build()?;
    let store = Store::new(engine.clone());
    let compiler = WasmerModuleCompiler::new(Some(cache_dir), cache_mode)?;
    let module = compiler
        .load_or_compile(&engine, &store, "fs_probe", &wasm)?
        .module;

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
        .with_mount(mount, host_fs)
        .with_current_dir(cwd)
        .with_args(guest_args)
        .with_envs(guest_env);
    runner
        .run_wasm(
            RuntimeOrEngine::Engine(engine),
            &program,
            module,
            ModuleHash::sha256(&wasm),
        )
        .context("run filesystem probe under Wasmer/WASIX")?;

    Ok(())
}

fn print_usage() {
    println!(
        "usage: fs_probe --wasm <path> --fs-root <path> [--mount /] [--cwd /] [--program /bin/fs_probe.wasi]"
    );
}
