use anyhow::{Context, Result, bail};
use pglite_oxide::PgliteServer;
#[cfg(feature = "extensions")]
use pglite_oxide::extensions;
use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;

#[derive(Debug)]
enum Bind {
    Tcp(SocketAddr),
    #[cfg(unix)]
    Unix(PathBuf),
}

#[derive(Debug)]
struct Args {
    root: Option<PathBuf>,
    temporary: bool,
    bind: Bind,
    print_uri: bool,
    extensions: Vec<String>,
}

fn main() -> Result<()> {
    let args = parse_args()?;
    let mut builder = if args.temporary {
        PgliteServer::builder().temporary()
    } else if let Some(root) = args.root {
        PgliteServer::builder().path(root)
    } else {
        PgliteServer::builder().path("./.pglite")
    };

    builder = match args.bind {
        Bind::Tcp(addr) => builder.tcp(addr),
        #[cfg(unix)]
        Bind::Unix(path) => builder.unix(path),
    };

    #[cfg(feature = "extensions")]
    {
        for name in &args.extensions {
            let extension = extensions::by_sql_name(name)
                .ok_or_else(|| anyhow::anyhow!("unknown bundled extension: {name}"))?;
            builder = builder.extension(extension);
        }
    }
    #[cfg(not(feature = "extensions"))]
    if !args.extensions.is_empty() {
        bail!("this pglite-proxy build was compiled without bundled extension support");
    }

    let server = builder.start()?;
    if args.print_uri {
        println!("{}", server.database_url());
    } else {
        eprintln!("listening: {}", server.database_url());
    }

    loop {
        std::thread::park();
    }
}

fn parse_args() -> Result<Args> {
    let mut root = None;
    let mut temporary = false;
    let mut print_uri = false;
    let mut extensions = Vec::new();
    let mut bind = Bind::Tcp("127.0.0.1:5432".parse().expect("valid default TCP addr"));

    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--temporary" => temporary = true,
            "--root" => {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--root requires a path"))?;
                root = Some(PathBuf::from(value));
                temporary = false;
            }
            "--tcp" => {
                let value = args.next().unwrap_or_else(|| "127.0.0.1:5432".to_string());
                bind = Bind::Tcp(
                    value
                        .parse()
                        .with_context(|| format!("parse TCP bind address {value}"))?,
                );
            }
            #[cfg(unix)]
            "--unix" | "--uds" => {
                let value = args
                    .next()
                    .unwrap_or_else(|| "/tmp/.s.PGSQL.5432".to_string());
                bind = Bind::Unix(PathBuf::from(value));
            }
            "--print-uri" => print_uri = true,
            "--extension" => {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--extension requires a name"))?;
                extensions.push(value);
            }
            "--help" | "-h" => {
                print_usage();
                std::process::exit(0);
            }
            other => bail!("unknown argument: {other}"),
        }
    }

    Ok(Args {
        root,
        temporary,
        bind,
        print_uri,
        extensions,
    })
}

fn print_usage() {
    eprintln!(
        "Usage: pglite-proxy [--temporary | --root PATH] [--tcp ADDR | --unix PATH] [--print-uri] [--extension NAME]"
    );
    eprintln!("  --temporary       Use an ephemeral database removed on exit");
    eprintln!("  --root PATH       Runtime and cluster root. Default: ./.pglite");
    eprintln!("  --tcp ADDR        Listen on TCP. Use 127.0.0.1:0 for a random port");
    #[cfg(unix)]
    eprintln!("  --unix PATH       Listen on a Unix socket path");
    eprintln!("  --print-uri       Print the PostgreSQL connection URI to stdout");
    eprintln!("  --extension NAME  Enable a bundled extension that passed the smoke suite");
}
