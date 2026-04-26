use std::fs::{self, File};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use tar::Archive;
use zstd::stream::read::Decoder as ZstdDecoder;

fn runtime_tar_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets/pglite-wasi.tar.zst")
}

fn unpack_tar_archive(dest_root: &Path) -> Result<()> {
    let tar_path = runtime_tar_path();
    let file =
        File::open(&tar_path).with_context(|| format!("open archive {}", tar_path.display()))?;
    let decoder = ZstdDecoder::new(file)
        .with_context(|| format!("decode zstd archive {}", tar_path.display()))?;
    let mut archive = Archive::new(decoder);

    for entry in archive.entries().context("read archive entries")? {
        let mut entry = entry.context("read archive entry")?;
        let path = entry
            .path()
            .context("read archive entry path")?
            .into_owned();
        let relative = path.strip_prefix("tmp").unwrap_or(&path);
        let dest = archive_destination(dest_root, relative)?;

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

fn archive_destination(root: &Path, archive_path: &Path) -> Result<PathBuf> {
    let mut dest = root.to_path_buf();
    for component in archive_path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => dest.push(part),
            _ => bail!("unsafe archive path {}", archive_path.display()),
        }
    }
    Ok(dest)
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let dest = args.next().unwrap_or_else(|| "./pglite-fs".to_string());
    unpack_tar_archive(&PathBuf::from(dest))
}
