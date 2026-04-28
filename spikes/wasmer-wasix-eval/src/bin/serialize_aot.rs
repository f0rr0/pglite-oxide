use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use wasmer::{Module, Store};
use wasmer_wasix_eval::{EngineKind, print_engine_report_named};
use zstd::stream::write::Encoder as ZstdEncoder;

fn main() -> Result<()> {
    let args = Args::parse()?;
    let engine = args.engine.build()?;
    print_engine_report_named(args.engine.name(), &engine);
    let store = Store::new(engine);
    let bytes = fs::read(&args.input).with_context(|| format!("read {}", args.input.display()))?;
    let module = Module::new(&store, &bytes)
        .with_context(|| format!("compile {}", args.input.display()))?;
    let serialized = module.serialize().context("serialize module")?;

    if let Some(parent) = args.output.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let file =
        fs::File::create(&args.output).with_context(|| format!("create {}", args.output.display()))?;
    let mut encoder = ZstdEncoder::new(file, 19)
        .with_context(|| format!("create zstd encoder for {}", args.output.display()))?;
    let mut serialized_slice = serialized.as_ref();
    std::io::copy(&mut serialized_slice, &mut encoder)
        .with_context(|| format!("write {}", args.output.display()))?;
    encoder
        .finish()
        .with_context(|| format!("finish {}", args.output.display()))?;
    println!(
        "serialized {} bytes to {}",
        serialized.len(),
        args.output.display()
    );
    Ok(())
}

struct Args {
    input: PathBuf,
    output: PathBuf,
    engine: EngineKind,
}

impl Args {
    fn parse() -> Result<Self> {
        let mut input = None;
        let mut output = None;
        let mut engine = EngineKind::Llvm;
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--input" => input = args.next().map(PathBuf::from),
                "--output" => output = args.next().map(PathBuf::from),
                "--engine" => {
                    let value = args
                        .next()
                        .ok_or_else(|| anyhow::anyhow!("--engine requires a value"))?;
                    engine = EngineKind::parse(&value)?;
                }
                "-h" | "--help" => {
                    println!(
                        "usage: cargo run --features llvm-engine --bin serialize_aot -- --input MODULE --output ARTIFACT.zst [--engine llvm]"
                    );
                    std::process::exit(0);
                }
                other => anyhow::bail!("unknown argument: {other}"),
            }
        }
        Ok(Self {
            input: input.ok_or_else(|| anyhow::anyhow!("--input is required"))?,
            output: output.ok_or_else(|| anyhow::anyhow!("--output is required"))?,
            engine,
        })
    }
}
