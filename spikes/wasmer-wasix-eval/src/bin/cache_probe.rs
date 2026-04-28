use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use wasmer::Store;
use wasmer_wasix_eval::{
    CacheMode, ModuleLoadKind, WasmerModuleCompiler, cranelift_engine, print_engine_report,
};

fn main() -> Result<()> {
    let args = Args::parse()?;
    let engine = cranelift_engine();
    print_engine_report(&engine);

    let inputs = args
        .modules
        .iter()
        .map(|path| {
            let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
            Ok((path.clone(), bytes))
        })
        .collect::<Result<Vec<_>>>()?;

    println!("cache-dir: {}", args.cache_dir.display());
    let first = run_pass(
        &engine,
        &args.cache_dir,
        args.first_mode,
        "first-pass",
        &inputs,
    )?;
    let second = run_pass(
        &engine,
        &args.cache_dir,
        CacheMode::Use,
        "second-pass",
        &inputs,
    )?;

    let second_hits = second
        .iter()
        .filter(|kind| **kind == ModuleLoadKind::CacheHit)
        .count();
    println!(
        "cache-probe verdict: second pass cache hits {second_hits}/{}",
        second.len()
    );
    if second_hits != second.len() {
        anyhow::bail!("not every module loaded from cache on the second pass");
    }

    let first_compiles = first
        .iter()
        .filter(|kind| **kind == ModuleLoadKind::Compiled)
        .count();
    println!("cache-probe first-pass compiled modules: {first_compiles}");
    Ok(())
}

fn run_pass(
    engine: &wasmer::Engine,
    cache_dir: &PathBuf,
    mode: CacheMode,
    label: &str,
    inputs: &[(PathBuf, Vec<u8>)],
) -> Result<Vec<ModuleLoadKind>> {
    println!();
    println!("== {label} cache-mode={mode} ==");
    let store = Store::new(engine.clone());
    let compiler = WasmerModuleCompiler::new(Some(cache_dir.clone()), mode)?;
    let mut outcomes = Vec::with_capacity(inputs.len());
    for (path, bytes) in inputs {
        let label = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("module");
        let compiled = compiler.load_or_compile(engine, &store, label, bytes)?;
        outcomes.push(compiled.report.kind);
    }
    Ok(outcomes)
}

struct Args {
    modules: Vec<PathBuf>,
    cache_dir: PathBuf,
    first_mode: CacheMode,
}

impl Args {
    fn parse() -> Result<Self> {
        let mut modules = Vec::new();
        let mut cache_dir = PathBuf::from("../wasix-postgres-build/build/wasmer-module-cache");
        let mut first_mode = CacheMode::Use;

        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--cache-dir" => {
                    let value = args
                        .next()
                        .ok_or_else(|| anyhow::anyhow!("--cache-dir requires a path"))?;
                    cache_dir = PathBuf::from(value);
                }
                "--rebuild" => first_mode = CacheMode::Rebuild,
                "-h" | "--help" => {
                    println!(
                        "usage: cargo run --bin cache_probe -- [--cache-dir PATH] [--rebuild] [WASM_OR_SO ...]"
                    );
                    std::process::exit(0);
                }
                value => modules.push(PathBuf::from(value)),
            }
        }

        if modules.is_empty() {
            modules.push(PathBuf::from(
                "../wasix-dlopen-proof/build/main.wasm",
            ));
            modules.push(PathBuf::from(
                "../wasix-dlopen-proof/build/libdlopened.so",
            ));
        }

        Ok(Self {
            modules,
            cache_dir,
            first_mode,
        })
    }
}
