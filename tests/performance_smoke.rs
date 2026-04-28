use anyhow::Result;
use pglite_oxide::Pglite;
use serde_json::json;
use std::time::Instant;

fn first_int(result: &pglite_oxide::Results, column: &str) -> i64 {
    result.rows[0][column].as_i64().expect("integer result")
}

#[test]
fn preload_runtime_then_open_smoke() -> Result<()> {
    let preload_started = Instant::now();
    Pglite::preload()?;
    let preload_elapsed = preload_started.elapsed();

    let open_started = Instant::now();
    let mut db = Pglite::builder().temporary().open()?;
    let open_elapsed = open_started.elapsed();

    let result = db.query("SELECT $1::int + 1 AS answer", &[json!(41)], None)?;
    assert_eq!(first_int(&result, "answer"), 42);
    db.close()?;

    eprintln!(
        "preload_runtime_then_open_smoke preload_ms={} open_ms={}",
        preload_elapsed.as_millis(),
        open_elapsed.as_millis()
    );
    Ok(())
}
