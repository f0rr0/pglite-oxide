use std::env;
use std::fs;
use std::path::PathBuf;

use tauri_sqlx_vanilla_lib::bench::BenchState;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = env::args().collect();
    let fresh = args.iter().any(|arg| arg == "--fresh");
    let row_count = args
        .iter()
        .position(|arg| arg == "--rows")
        .and_then(|index| args.get(index + 1))
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(10_000);
    let json_out = args
        .iter()
        .position(|arg| arg == "--json-out")
        .and_then(|index| args.get(index + 1))
        .map(PathBuf::from);

    let root = env::var_os("PGLITE_OXIDE_TAURI_PROFILE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| env::temp_dir().join("pglite-oxide-tauri-sqlx-profile"));

    let state = BenchState::new(root);
    let report = state.profile_queries(fresh, row_count).await?;
    let json = serde_json::to_string_pretty(&report)?;
    if let Some(path) = json_out {
        fs::write(path, &json)?;
    } else {
        println!("{json}");
    }
    Ok(())
}
