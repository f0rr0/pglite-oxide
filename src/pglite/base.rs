use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, OnceLock};

use anyhow::{Context, Result, anyhow, bail};
use directories::ProjectDirs;
use flate2::read::GzDecoder;
use serde::Deserialize;
use tar::Archive;
use tracing::info;
use xz2::read::XzDecoder;

use super::postgres_mod::PostgresMod;
use tempfile::TempDir;

#[derive(Debug, Deserialize)]
struct ManifestEntry {
    path: String,
    start: usize,
    end: usize,
}

static FS_MANIFEST: LazyLock<Vec<ManifestEntry>> = LazyLock::new(|| {
    serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/assets/pglite_fs_manifest.json"
    )))
    .expect("failed to parse pglite_fs_manifest.json")
});

static TEMPLATE_CLUSTER: OnceLock<std::result::Result<Arc<TemplateCluster>, String>> =
    OnceLock::new();

#[derive(Debug)]
struct TemplateCluster {
    root: PathBuf,
    _temp_dir: TempDir,
}

pub fn load_fs_bundle() -> Result<Vec<u8>> {
    if let Ok(path) = std::env::var("PGLITE_OXIDE_FS_BUNDLE") {
        return std::fs::read(&path).with_context(|| format!("read bundle from {}", path));
    }
    let default_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets/pglite.data");
    std::fs::read(&default_path)
        .with_context(|| format!("read bundle from {}", default_path.display()))
}

#[derive(Debug, Clone)]
pub struct PglitePaths {
    pub pgroot: PathBuf,
    pub pgdata: PathBuf,
}

impl PglitePaths {
    pub fn new(app_qual: (&str, &str, &str)) -> Result<Self> {
        let pd = ProjectDirs::from(app_qual.0, app_qual.1, app_qual.2)
            .context("could not resolve app data dir")?;
        let app_dir = pd.data_dir().to_path_buf();
        Ok(Self::with_root(app_dir))
    }

    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        let base = root.into();
        let pgroot = base.join("tmp");
        let pgdata = pgroot.join("pglite").join("base");
        Self { pgroot, pgdata }
    }

    pub fn with_paths(pgroot: impl Into<PathBuf>, pgdata: impl Into<PathBuf>) -> Self {
        Self {
            pgroot: pgroot.into(),
            pgdata: pgdata.into(),
        }
    }

    pub fn mount_root(&self) -> &Path {
        &self.pgroot
    }

    pub fn with_temp_dir() -> Result<(TempDir, Self)> {
        let tmp = TempDir::new().context("create temporary directory")?;
        let paths = Self::with_root(tmp.path());
        Ok((tmp, paths))
    }

    fn marker_cluster(&self) -> PathBuf {
        self.pgdata.join("PG_VERSION")
    }

    pub fn is_cluster_initialized(&self) -> bool {
        self.marker_cluster().exists()
    }
}

fn locate_runtime_module(paths: &PglitePaths) -> Option<(PathBuf, PathBuf)> {
    let pglite_dir = paths.pgroot.join("pglite");
    if !pglite_dir.exists() {
        return None;
    }
    let pglite_bin_dir = pglite_dir.join("bin");
    let module = pglite_bin_dir.join("pglite.wasi");
    if !module.exists() {
        return None;
    }

    let share = pglite_dir.join("share").join("postgresql");
    if !share.exists() || !share.join("postgres.bki").exists() {
        return None;
    }
    Some((module, pglite_bin_dir))
}

fn ensure_runtime(paths: &PglitePaths) -> Result<bool> {
    if locate_runtime_module(paths).is_some() {
        return Ok(false);
    }

    if let Some(parent) = paths.pgroot.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create parent directory {}", parent.display()))?;
    } else {
        fs::create_dir_all(&paths.pgroot).context("create pgroot dir")?;
    }

    if install_runtime_from_tar(paths)? {
        locate_runtime_module(paths).ok_or_else(|| {
            anyhow!(
                "runtime missing: could not locate module under {} after tar install",
                paths.pgroot.display()
            )
        })?;
        return Ok(true);
    }

    info!("installing embedded filesystem bundle");
    let bundle = load_fs_bundle()?;
    install_fs_bundle(paths, &bundle)?;
    install_wasm_binary(paths)?;

    locate_runtime_module(paths).ok_or_else(|| {
        anyhow!(
            "runtime missing: could not locate module under {} after install",
            paths.pgroot.display()
        )
    })?;

    Ok(true)
}

fn install_fs_bundle(paths: &PglitePaths, bundle: &[u8]) -> Result<()> {
    for entry in FS_MANIFEST.iter() {
        let dest = manifest_entry_dest(paths, &entry.path)
            .with_context(|| format!("unsupported manifest path {}", entry.path))?;

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
            fs::File::create(&dest).with_context(|| format!("create file {}", dest.display()))?;
        } else {
            fs::write(&dest, &bundle[start..end])
                .with_context(|| format!("write {}", dest.display()))?;
        }
    }

    let password_path = paths.pgroot.join("pglite/password");
    if password_path.exists() {
        fs::write(&password_path, b"postgres\n")
            .with_context(|| format!("overwrite {}", password_path.display()))?;
    }

    Ok(())
}

fn runtime_tar_path() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("PGLITE_OXIDE_RUNTIME_TAR") {
        let candidate = PathBuf::from(path);
        if candidate.exists() {
            return Some(candidate);
        }
    }

    let tar_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets/pglite-wasi.tar.xz");
    if tar_path.exists() {
        return Some(tar_path);
    }

    None
}

fn install_runtime_from_tar(paths: &PglitePaths) -> Result<bool> {
    let Some(tar_path) = runtime_tar_path() else {
        return Ok(false);
    };

    info!("installing runtime from tar archive {}", tar_path.display());
    let file = File::open(&tar_path)
        .with_context(|| format!("open runtime archive {}", tar_path.display()))?;

    let mut decoder = XzDecoder::new(file);
    let mut archive = Archive::new(&mut decoder);
    let unpack_target = paths
        .pgroot
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| paths.pgroot.clone());
    archive.unpack(&unpack_target).with_context(|| {
        format!(
            "unpack runtime archive {} into {}",
            tar_path.display(),
            unpack_target.display()
        )
    })?;

    Ok(true)
}

fn manifest_entry_dest(paths: &PglitePaths, manifest_path: &str) -> Result<PathBuf> {
    if let Some(rest) = manifest_path.strip_prefix("/tmp/") {
        Ok(paths.pgroot.join(rest))
    } else if let Some(rest) = manifest_path.strip_prefix('/') {
        Ok(paths.pgroot.join(rest))
    } else {
        Err(anyhow!(
            "manifest path {} has unknown prefix",
            manifest_path
        ))
    }
}

fn install_wasm_binary(paths: &PglitePaths) -> Result<()> {
    let src = wasm_asset_path();
    if !src.exists() {
        bail!("missing wasm asset at {}", src.display());
    }

    let dest = paths.pgroot.join("pglite/bin/pglite.wasi");
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create directory {}", parent.display()))?;
    }

    fs::copy(&src, &dest)
        .with_context(|| format!("copy {} to {}", src.display(), dest.display()))?;
    Ok(())
}

fn wasm_asset_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets/pglite.wasi")
}

fn install_extension_reader<R: Read>(paths: &PglitePaths, reader: R) -> Result<()> {
    let mut ar = Archive::new(GzDecoder::new(reader));
    let target = paths.pgroot.join("pglite");
    std::fs::create_dir_all(&target)
        .with_context(|| format!("create extension target {}", target.display()))?;
    ar.unpack(&target)
        .with_context(|| format!("unpack extension into {}", target.display()))?;
    Ok(())
}

pub fn install_extension_archive(paths: &PglitePaths, archive_path: &Path) -> Result<()> {
    let file = std::fs::File::open(archive_path)
        .with_context(|| format!("open extension archive {}", archive_path.display()))?;
    install_extension_reader(paths, file)
}

pub fn install_extension_bytes(paths: &PglitePaths, bytes: &[u8]) -> Result<()> {
    install_extension_reader(paths, std::io::Cursor::new(bytes))
}

fn ensure_pgdata(paths: &PglitePaths) -> Result<()> {
    if !paths.pgdata.exists() {
        fs::create_dir_all(&paths.pgdata).with_context(|| {
            format!(
                "failed to create initial pgdata directory at {}",
                paths.pgdata.display()
            )
        })?;
    }
    Ok(())
}

pub fn ensure_cluster(paths: &PglitePaths) -> Result<()> {
    if paths.marker_cluster().exists() {
        return Ok(());
    }

    ensure_runtime(paths)?;
    ensure_pgdata(paths)?;

    let mut pg = PostgresMod::new(paths.clone())?;
    pg.ensure_cluster()
}

#[derive(Debug)]
pub struct InstallOutcome {
    pub paths: PglitePaths,
    pub unpacked_runtime: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct InstallOptions {
    pub ensure_cluster: bool,
}

impl Default for InstallOptions {
    fn default() -> Self {
        Self {
            ensure_cluster: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct MountInfo {
    mount: PathBuf,
    paths: PglitePaths,
    reused_existing: bool,
}

impl MountInfo {
    pub fn into_paths(self) -> PglitePaths {
        self.paths
    }

    pub fn mount(&self) -> &Path {
        &self.mount
    }

    pub fn paths(&self) -> &PglitePaths {
        &self.paths
    }

    pub fn reused_existing(&self) -> bool {
        self.reused_existing
    }
}

pub fn install_default(app_id: (&str, &str, &str)) -> Result<InstallOutcome> {
    let paths = PglitePaths::new(app_id)?;
    install_into_internal(paths)
}

pub fn install_into(root: &Path) -> Result<InstallOutcome> {
    let paths = PglitePaths::with_root(root);
    install_into_internal(paths)
}

pub(crate) fn install_temporary_from_template() -> Result<(TempDir, InstallOutcome)> {
    let template = template_cluster()?;
    let temp_dir = TempDir::new().context("create temporary pglite directory")?;
    copy_dir_filtered(&template.root, temp_dir.path())?;

    let outcome = InstallOutcome {
        paths: PglitePaths::with_root(temp_dir.path()),
        unpacked_runtime: false,
    };
    Ok((temp_dir, outcome))
}

fn install_into_internal(paths: PglitePaths) -> Result<InstallOutcome> {
    let unpacked_runtime = ensure_runtime(&paths)?;
    ensure_pgdata(&paths)?;
    Ok(InstallOutcome {
        paths,
        unpacked_runtime,
    })
}

pub fn install_and_init(app_id: (&str, &str, &str)) -> Result<MountInfo> {
    let outcome = install_default(app_id)?;
    if !outcome.paths.marker_cluster().exists() {
        ensure_cluster(&outcome.paths)?;
    }
    Ok(MountInfo {
        mount: outcome.paths.pgroot.clone(),
        paths: outcome.paths,
        reused_existing: !outcome.unpacked_runtime,
    })
}

pub fn install_and_init_in<P: AsRef<Path>>(root: P) -> Result<MountInfo> {
    let outcome = install_into(root.as_ref())?;
    if !outcome.paths.marker_cluster().exists() {
        ensure_cluster(&outcome.paths)?;
    }
    Ok(MountInfo {
        mount: outcome.paths.pgroot.clone(),
        paths: outcome.paths,
        reused_existing: !outcome.unpacked_runtime,
    })
}

pub fn install_with_options(paths: PglitePaths, options: InstallOptions) -> Result<MountInfo> {
    let unpacked_runtime = ensure_runtime(&paths)?;
    ensure_pgdata(&paths)?;
    if options.ensure_cluster && !paths.marker_cluster().exists() {
        ensure_cluster(&paths)?;
    }
    Ok(MountInfo {
        mount: paths.pgroot.clone(),
        paths,
        reused_existing: !unpacked_runtime,
    })
}

fn template_cluster() -> Result<Arc<TemplateCluster>> {
    TEMPLATE_CLUSTER
        .get_or_init(|| {
            build_template_cluster()
                .map(Arc::new)
                .map_err(|err| format!("{err:#}"))
        })
        .clone()
        .map_err(|message| anyhow!(message))
}

fn build_template_cluster() -> Result<TemplateCluster> {
    let temp_dir = TempDir::new().context("create pglite template cluster directory")?;
    let outcome = install_into(temp_dir.path())?;
    ensure_cluster(&outcome.paths)?;

    Ok(TemplateCluster {
        root: temp_dir.path().to_path_buf(),
        _temp_dir: temp_dir,
    })
}

fn copy_dir_filtered(src: &Path, dest: &Path) -> Result<()> {
    fs::create_dir_all(dest).with_context(|| format!("create directory {}", dest.display()))?;

    for entry in fs::read_dir(src).with_context(|| format!("read directory {}", src.display()))? {
        let entry = entry.with_context(|| format!("read entry under {}", src.display()))?;
        let file_name = entry.file_name();
        if should_skip_template_entry(&file_name) {
            continue;
        }

        let src_path = entry.path();
        let dest_path = dest.join(&file_name);
        let file_type = entry
            .file_type()
            .with_context(|| format!("stat {}", src_path.display()))?;

        if file_type.is_dir() {
            copy_dir_filtered(&src_path, &dest_path)?;
        } else if file_type.is_file() {
            if let Some(parent) = dest_path.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("create directory {}", parent.display()))?;
            }
            fs::copy(&src_path, &dest_path).with_context(|| {
                format!("copy {} to {}", src_path.display(), dest_path.display())
            })?;
        } else if file_type.is_symlink() {
            copy_symlink(&src_path, &dest_path)?;
        }
    }

    Ok(())
}

fn should_skip_template_entry(file_name: &OsStr) -> bool {
    let name = file_name.to_string_lossy();
    name.starts_with(".s.PGSQL.") || name == "postmaster.pid"
}

#[cfg(unix)]
fn copy_symlink(src: &Path, dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create directory {}", parent.display()))?;
    }
    let target = fs::read_link(src).with_context(|| format!("read symlink {}", src.display()))?;
    std::os::unix::fs::symlink(&target, dest)
        .with_context(|| format!("create symlink {} -> {}", dest.display(), target.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn copy_symlink(src: &Path, dest: &Path) -> Result<()> {
    let target = fs::read_link(src).with_context(|| format!("read symlink {}", src.display()))?;
    let target_path = if target.is_absolute() {
        target
    } else {
        src.parent().unwrap_or_else(|| Path::new(".")).join(target)
    };

    if target_path.is_dir() {
        copy_dir_filtered(&target_path, dest)
    } else {
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create directory {}", parent.display()))?;
        }
        fs::copy(&target_path, dest)
            .with_context(|| format!("copy {} to {}", target_path.display(), dest.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_copy_keeps_cluster_files_and_skips_runtime_state() -> Result<()> {
        let source = TempDir::new()?;
        let pgdata = source.path().join("tmp/pglite/base");
        fs::create_dir_all(&pgdata)?;
        fs::write(pgdata.join("PG_VERSION"), b"17\n")?;
        fs::write(pgdata.join("postmaster.pid"), b"stale pid")?;
        fs::write(source.path().join(".s.PGSQL.5432"), b"socket")?;
        fs::write(source.path().join(".s.PGSQL.5432.lock"), b"lock")?;

        let dest = TempDir::new()?;
        copy_dir_filtered(source.path(), dest.path())?;

        assert!(dest.path().join("tmp/pglite/base/PG_VERSION").exists());
        assert!(!dest.path().join("tmp/pglite/base/postmaster.pid").exists());
        assert!(!dest.path().join(".s.PGSQL.5432").exists());
        assert!(!dest.path().join(".s.PGSQL.5432.lock").exists());
        Ok(())
    }
}
