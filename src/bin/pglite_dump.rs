use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use tar::Archive;
use xz2::read::XzDecoder;

#[derive(Debug, Deserialize)]
struct ManifestEntry {
    path: String,
    start: usize,
    end: usize,
}

fn read_manifest() -> Result<Vec<ManifestEntry>> {
    let manifest_str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/assets/pglite_fs_manifest.json"
    ));
    let entries: Vec<ManifestEntry> =
        serde_json::from_str(manifest_str).context("failed to parse pglite_fs_manifest.json")?;
    Ok(entries)
}

fn read_bundle() -> Result<Vec<u8>> {
    let bundle_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets/pglite.data");
    let bytes = fs::read(&bundle_path)
        .with_context(|| format!("failed to read {}", bundle_path.display()))?;
    Ok(bytes)
}

fn runtime_tar_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets/pglite-wasi.tar.xz")
}

fn map_dest(root: &Path, manifest_path: &str) -> Result<PathBuf> {
    if let Some(rest) = manifest_path
        .strip_prefix('/')
        .and_then(|p| p.strip_prefix("tmp/"))
    {
        Ok(root.join(rest))
    } else if let Some(rest) = manifest_path.strip_prefix('/').map(|s| s.to_string()) {
        Ok(root.join(rest))
    } else {
        bail!("unsupported manifest path: {}", manifest_path)
    }
}

fn write_entry(root: &Path, bundle: &[u8], entry: &ManifestEntry) -> Result<()> {
    let dest = map_dest(root, &entry.path)?;
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create directory {}", parent.display()))?;
    }

    let start = entry.start;
    let end = entry.end;
    if start > end || end > bundle.len() {
        bail!(
            "manifest entry {} has invalid bounds {}..{} (bundle len {})",
            entry.path,
            start,
            end,
            bundle.len()
        );
    }

    if start == end {
        // empty file
        fs::File::create(&dest).with_context(|| format!("create file {}", dest.display()))?;
    } else {
        let mut file =
            fs::File::create(&dest).with_context(|| format!("create file {}", dest.display()))?;
        file.write_all(&bundle[start..end])
            .with_context(|| format!("write file {}", dest.display()))?;
    }
    Ok(())
}

fn unpack_tar_archive(dest_root: &Path) -> Result<()> {
    let tar_path = runtime_tar_path();
    let file =
        File::open(&tar_path).with_context(|| format!("open archive {}", tar_path.display()))?;
    let decoder = XzDecoder::new(file);
    let mut archive = Archive::new(decoder);

    for entry in archive.entries().context("read archive entries")? {
        let mut entry = entry.context("read archive entry")?;
        let path = entry
            .path()
            .context("read archive entry path")?
            .into_owned();
        let dest = match path.strip_prefix("tmp") {
            Ok(rest) => dest_root.join(rest),
            Err(_) => dest_root.join(path),
        };

        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create directory {}", parent.display()))?;
        }

        entry
            .unpack(&dest)
            .with_context(|| format!("unpack {}", dest.display()))?;
    }

    Ok(())
}

fn run(dest_root: &Path) -> Result<()> {
    if !PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("assets/pglite.data")
        .exists()
    {
        return unpack_tar_archive(dest_root);
    }

    let manifest = read_manifest()?;
    let bundle = read_bundle()?;

    for entry in manifest.iter() {
        write_entry(dest_root, &bundle, entry)
            .with_context(|| format!("extract {}", entry.path))?;
    }
    Ok(())
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let dest = args.next().unwrap_or_else(|| "./pglite-fs".to_string());
    let dest_path = PathBuf::from(dest);
    run(&dest_path)
}
