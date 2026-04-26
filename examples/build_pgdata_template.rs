use std::env;
use std::path::PathBuf;

use anyhow::Result;
use pglite_oxide::build_pgdata_template;

fn main() -> Result<()> {
    let output_dir = env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets/prepopulated"));

    let template = build_pgdata_template(&output_dir)?;
    println!("archive: {}", template.archive_path.display());
    println!("manifest: {}", template.manifest_path.display());
    Ok(())
}
