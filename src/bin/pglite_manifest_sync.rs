use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use regex::Regex;
use serde::Serialize;

#[derive(Debug, Serialize)]
struct ManifestEntryOut<'a> {
    path: &'a str,
    start: usize,
    end: usize,
}

fn main() -> Result<()> {
    let js_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "./pglite.js".to_string());
    let out_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets/pglite_fs_manifest.json");

    let js = fs::read_to_string(&js_path).with_context(|| format!("read {}", js_path))?;

    // Extract the JSON array passed to loadPackage({ files: [...] })
    let re = Regex::new(r#"loadPackage\(\{\s*\"files\":\s*(\[[^\]]*\])\s*,\s*\"remote_package_size\"\s*:\s*\d+\s*\}\)"#)
        .expect("invalid regex");
    let caps = re
        .captures(&js)
        .ok_or_else(|| anyhow!("failed to locate files array in {}", js_path))?;
    let files_json = caps.get(1).unwrap().as_str();

    // Parse the array of { filename, start, end }
    #[derive(serde::Deserialize)]
    struct FileRec {
        filename: String,
        start: usize,
        end: usize,
    }
    let files: Vec<FileRec> = serde_json::from_str(files_json).context("parse files array json")?;

    // Convert to our manifest format
    let out: Vec<ManifestEntryOut> = files
        .iter()
        .map(|f| ManifestEntryOut {
            path: &f.filename,
            start: f.start,
            end: f.end,
        })
        .collect();

    let pretty = serde_json::to_string_pretty(&out).context("serialize manifest json")?;
    fs::write(&out_path, pretty + "\n").with_context(|| format!("write {}", out_path.display()))?;

    println!("updated {} from {}", out_path.display(), js_path);
    Ok(())
}
