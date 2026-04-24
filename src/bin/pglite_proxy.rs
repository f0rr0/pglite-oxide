use anyhow::{Result, bail};
use pglite_oxide::PgliteProxy;
use std::env;
use std::path::PathBuf;

#[derive(Debug)]
enum Bind {
    Tcp(String),
    #[cfg(unix)]
    Unix(PathBuf),
}

#[derive(Debug)]
struct Args {
    root: PathBuf,
    bind: Bind,
}

fn main() -> Result<()> {
    let args = parse_args()?;
    let proxy = PgliteProxy::new(args.root);

    match args.bind {
        Bind::Tcp(addr) => {
            eprintln!("listening on tcp: {addr}");
            proxy.serve_tcp(addr)
        }
        #[cfg(unix)]
        Bind::Unix(path) => {
            eprintln!("listening on unix socket: {}", path.display());
            eprintln!("connection string: postgresql://postgres@/template1?host=/tmp");
            proxy.serve_unix(path)
        }
    }
}

fn parse_args() -> Result<Args> {
    let mut root = PathBuf::from("./.pglite");
    #[cfg(unix)]
    let mut bind = Bind::Unix(PathBuf::from("/tmp/.s.PGSQL.5432"));
    #[cfg(not(unix))]
    let mut bind = Bind::Tcp("127.0.0.1:5432".to_string());

    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--root" => {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--root requires a path"))?;
                root = PathBuf::from(value);
            }
            "--tcp" => {
                let value = args.next().unwrap_or_else(|| "127.0.0.1:5432".to_string());
                bind = Bind::Tcp(value);
            }
            #[cfg(unix)]
            "--uds" => {
                let value = args
                    .next()
                    .unwrap_or_else(|| "/tmp/.s.PGSQL.5432".to_string());
                bind = Bind::Unix(PathBuf::from(value));
            }
            "--help" | "-h" => {
                print_usage();
                std::process::exit(0);
            }
            other => bail!("unknown argument: {other}"),
        }
    }

    Ok(Args { root, bind })
}

fn print_usage() {
    eprintln!("Usage: pglite-proxy [--root PATH] [--tcp ADDR | --uds PATH]");
    eprintln!("  --root PATH  Runtime and cluster root. Default: ./.pglite");
    eprintln!("  --tcp ADDR   Listen on TCP. Default address: 127.0.0.1:5432");
    #[cfg(unix)]
    eprintln!("  --uds PATH   Listen on Unix socket. Default: /tmp/.s.PGSQL.5432");
}
