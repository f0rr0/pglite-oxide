use std::fs;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result, bail, ensure};
use directories::ProjectDirs;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use wasmer::sys::{EngineBuilder, Features};
use wasmer::{Engine, Module};
use zstd::stream::read::Decoder as ZstdDecoder;

#[cfg(feature = "extensions")]
use super::extensions::Extension;

const RUNTIME_ARTIFACT: &str = "runtime:pglite";
const EXPECTED_AOT_ENGINE: &str = "llvm-opta";
const EXPECTED_WASMER_VERSION: &str = "7.2.0-alpha.2";
const EXPECTED_WASMER_WASIX_VERSION: &str = "0.702.0-alpha.2";
const ZSTD_MAGIC: &[u8] = &[0x28, 0xb5, 0x2f, 0xfd];
static AOT_INSTALL_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

pub(crate) fn headless_engine() -> Engine {
    let mut features = Features::new();
    features.exceptions(true);
    EngineBuilder::headless()
        .set_features(Some(features))
        .engine()
        .into()
}

pub(crate) fn load_runtime_module() -> Result<(Engine, Module)> {
    let engine = headless_engine();
    let cache_path = install_artifact(RUNTIME_ARTIFACT)?;
    let module = deserialize_headless(&engine, &cache_path)?;
    Ok((engine, module))
}

pub(crate) fn preload_runtime_artifact() -> Result<()> {
    let _ = load_runtime_module()?;
    Ok(())
}

#[cfg(feature = "extensions")]
pub(crate) fn preload_extension_artifact(extension: Extension) -> Result<()> {
    let engine = headless_engine();
    let _ = load_extension_module(&engine, extension)?;
    Ok(())
}

#[cfg(feature = "extensions")]
pub(crate) fn load_extension_module(engine: &Engine, extension: Extension) -> Result<Module> {
    load_artifact_module(engine, extension.aot_name())
}

pub(crate) fn load_artifact_module(engine: &Engine, artifact_name: &str) -> Result<Module> {
    let cache_path = install_artifact(artifact_name)?;
    deserialize_headless(engine, &cache_path)
}

#[cfg(test)]
pub(crate) fn load_pg_dump_module(engine: &Engine) -> Result<Module> {
    load_artifact_module(engine, "tool:pg_dump")
}

fn install_artifact(name: &str) -> Result<PathBuf> {
    let _guard = AOT_INSTALL_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .expect("AOT install lock poisoned");
    let raw = artifact_raw_bytes(name)?;
    let hash = sha256_hex(&raw);
    let cache_path = cache_path(name, &hash)?;

    if cache_path.exists() {
        match fs::read(&cache_path) {
            Ok(existing) if sha256_hex(&existing) == hash => return Ok(cache_path),
            Ok(_) => {
                remove_file_if_exists(&cache_path).with_context(|| {
                    format!("remove stale AOT artifact {}", cache_path.display())
                })?;
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("read AOT artifact {}", cache_path.display()));
            }
        }
    }

    if let Some(parent) = cache_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create AOT cache directory {}", parent.display()))?;
    }
    let tmp_path =
        cache_path.with_extension(format!("bin.{}.{}.tmp", std::process::id(), tmp_suffix()));
    fs::write(&tmp_path, raw)
        .with_context(|| format!("write AOT artifact {}", tmp_path.display()))?;
    if let Err(err) = fs::rename(&tmp_path, &cache_path) {
        remove_file_if_exists(&tmp_path).ok();
        if cache_path.exists() {
            let existing = fs::read(&cache_path)
                .with_context(|| format!("read AOT artifact {}", cache_path.display()))?;
            if sha256_hex(&existing) == hash {
                return Ok(cache_path);
            }
        }
        return Err(err).with_context(|| {
            format!(
                "promote AOT artifact {} -> {}",
                tmp_path.display(),
                cache_path.display()
            )
        });
    }

    Ok(cache_path)
}

fn remove_file_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("remove {}", path.display())),
    }
}

fn tmp_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default()
}

fn artifact_raw_bytes(name: &str) -> Result<Vec<u8>> {
    let Some(bytes) = target_artifact_bytes(name) else {
        bail!(
            "no Wasmer LLVM AOT artifact named '{name}' is available for target {}; rebuild assets or disable this unsupported target",
            target_triple()
        );
    };
    validate_artifact_manifest(name, bytes)?;

    if bytes.starts_with(ZSTD_MAGIC) {
        let mut decoder = ZstdDecoder::new(Cursor::new(bytes))
            .with_context(|| format!("decode compressed AOT artifact '{name}'"))?;
        let mut raw = Vec::new();
        decoder
            .read_to_end(&mut raw)
            .with_context(|| format!("decompress AOT artifact '{name}'"))?;
        ensure!(
            !raw.is_empty(),
            "AOT artifact '{name}' decompressed to zero bytes"
        );
        Ok(raw)
    } else {
        Ok(bytes.to_vec())
    }
}

fn validate_artifact_manifest(name: &str, bytes: &[u8]) -> Result<()> {
    let manifest = target_aot_manifest()?;
    ensure!(
        manifest.target_triple == target_triple(),
        "AOT manifest target mismatch: manifest={} actual={}",
        manifest.target_triple,
        target_triple()
    );
    ensure!(
        manifest.engine == EXPECTED_AOT_ENGINE,
        "AOT manifest engine mismatch: manifest={} expected={EXPECTED_AOT_ENGINE}",
        manifest.engine
    );
    ensure!(
        manifest.wasmer_version == EXPECTED_WASMER_VERSION,
        "AOT manifest Wasmer version mismatch: manifest={} expected={EXPECTED_WASMER_VERSION}",
        manifest.wasmer_version
    );
    ensure!(
        manifest.wasmer_wasix_version == EXPECTED_WASMER_WASIX_VERSION,
        "AOT manifest wasmer-wasix version mismatch: manifest={} expected={EXPECTED_WASMER_WASIX_VERSION}",
        manifest.wasmer_wasix_version
    );

    let artifact = manifest
        .artifacts
        .iter()
        .find(|artifact| artifact.name == name)
        .ok_or_else(|| anyhow::anyhow!("AOT manifest does not list artifact '{name}'"))?;
    let actual_hash = sha256_hex(bytes);
    ensure!(
        actual_hash.eq_ignore_ascii_case(&artifact.sha256),
        "AOT artifact '{name}' hash mismatch: manifest={} actual={actual_hash}",
        artifact.sha256
    );

    #[cfg(feature = "extensions")]
    {
        let expected_module = super::assets::expected_module_sha256(name)?;
        ensure!(
            expected_module.eq_ignore_ascii_case(&artifact.module_sha256),
            "AOT artifact '{name}' source module hash mismatch: manifest={} assets={expected_module}",
            artifact.module_sha256
        );
    }

    Ok(())
}

fn target_aot_manifest() -> Result<AotManifest> {
    let Some(json) = target_aot_manifest_json() else {
        bail!(
            "no Wasmer LLVM AOT manifest is available for target {}; rebuild assets or disable this unsupported target",
            target_triple()
        );
    };
    serde_json::from_str(json).context("parse bundled AOT manifest")
}

fn cache_path(name: &str, hash: &str) -> Result<PathBuf> {
    let safe_name = name.replace([':', '/', '\\'], "-");
    let dirs = ProjectDirs::from("dev", "pglite-oxide", "pglite-oxide")
        .context("could not resolve pglite-oxide cache directory")?;
    Ok(dirs
        .cache_dir()
        .join("wasmer-aot")
        .join(target_triple())
        .join(format!("{safe_name}-{hash}.bin")))
}

#[allow(unsafe_code)]
fn deserialize_headless(engine: &Engine, path: &Path) -> Result<Module> {
    // SAFETY: artifacts are package-owned Wasmer native code. Before this point
    // compressed artifacts are expanded into a private cache path keyed by the
    // raw artifact SHA-256; stale or corrupted files are replaced.
    unsafe {
        Module::deserialize_from_file(engine, path)
            .with_context(|| format!("deserialize Wasmer AOT artifact {}", path.display()))
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn target_triple() -> &'static str {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        return "aarch64-apple-darwin";
    }
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    {
        return "x86_64-apple-darwin";
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        return "x86_64-unknown-linux-gnu";
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    {
        return "aarch64-unknown-linux-gnu";
    }
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    {
        return "x86_64-pc-windows-msvc";
    }
    #[allow(unreachable_code)]
    "unsupported"
}

fn target_artifact_bytes(_name: &str) -> Option<&'static [u8]> {
    #[cfg(all(feature = "extensions", target_os = "macos", target_arch = "aarch64"))]
    {
        return pglite_oxide_aot_aarch64_apple_darwin::artifact_bytes(_name);
    }
    #[cfg(all(feature = "extensions", target_os = "macos", target_arch = "x86_64"))]
    {
        return pglite_oxide_aot_x86_64_apple_darwin::artifact_bytes(_name);
    }
    #[cfg(all(feature = "extensions", target_os = "linux", target_arch = "x86_64"))]
    {
        return pglite_oxide_aot_x86_64_unknown_linux_gnu::artifact_bytes(_name);
    }
    #[cfg(all(feature = "extensions", target_os = "linux", target_arch = "aarch64"))]
    {
        return pglite_oxide_aot_aarch64_unknown_linux_gnu::artifact_bytes(_name);
    }
    #[cfg(all(feature = "extensions", target_os = "windows", target_arch = "x86_64"))]
    {
        return pglite_oxide_aot_x86_64_pc_windows_msvc::artifact_bytes(_name);
    }
    #[allow(unreachable_code)]
    None
}

fn target_aot_manifest_json() -> Option<&'static str> {
    #[cfg(all(feature = "extensions", target_os = "macos", target_arch = "aarch64"))]
    {
        return Some(pglite_oxide_aot_aarch64_apple_darwin::MANIFEST_JSON);
    }
    #[cfg(all(feature = "extensions", target_os = "macos", target_arch = "x86_64"))]
    {
        return Some(pglite_oxide_aot_x86_64_apple_darwin::MANIFEST_JSON);
    }
    #[cfg(all(feature = "extensions", target_os = "linux", target_arch = "x86_64"))]
    {
        return Some(pglite_oxide_aot_x86_64_unknown_linux_gnu::MANIFEST_JSON);
    }
    #[cfg(all(feature = "extensions", target_os = "linux", target_arch = "aarch64"))]
    {
        return Some(pglite_oxide_aot_aarch64_unknown_linux_gnu::MANIFEST_JSON);
    }
    #[cfg(all(feature = "extensions", target_os = "windows", target_arch = "x86_64"))]
    {
        return Some(pglite_oxide_aot_x86_64_pc_windows_msvc::MANIFEST_JSON);
    }
    #[allow(unreachable_code)]
    None
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct AotManifest {
    target_triple: String,
    engine: String,
    wasmer_version: String,
    wasmer_wasix_version: String,
    artifacts: Vec<AotManifestArtifact>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct AotManifestArtifact {
    name: String,
    sha256: String,
    #[allow(dead_code)]
    module_sha256: String,
}
