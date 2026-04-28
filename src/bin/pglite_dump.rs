use anyhow::{Result, bail};
use std::env;
use std::path::PathBuf;

#[derive(Debug)]
struct Args {
    root: PathBuf,
    passthrough: Vec<String>,
}

fn main() -> Result<()> {
    let Args { root, passthrough } = parse_args()?;
    let _ = (root, passthrough);
    bail!(
        "pglite-dump is reserved for the WASIX pg_dump runner, but that runner is not exposed until dump/restore integration passes"
    )
}

fn parse_args() -> Result<Args> {
    let mut root = PathBuf::from("./.pglite");
    let mut passthrough = Vec::new();
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--root" => {
                root = PathBuf::from(
                    args.next()
                        .ok_or_else(|| anyhow::anyhow!("--root requires a path"))?,
                );
            }
            "--help" | "-h" => {
                print_usage();
                std::process::exit(0);
            }
            "--" => {
                passthrough.extend(args);
                break;
            }
            other => passthrough.push(other.to_string()),
        }
    }
    Ok(Args { root, passthrough })
}

fn print_usage() {
    eprintln!("Usage: pglite-dump --root PATH -- [pg_dump args]");
    eprintln!(
        "The Rust/WASIX pg_dump runner is intentionally hidden until dump/restore tests pass."
    );
}
