#[cfg(feature = "extensions")]
use anyhow::{Context, Result, anyhow};

#[cfg(feature = "extensions")]
pub(crate) fn runtime_archive() -> Option<&'static [u8]> {
    Some(pglite_oxide_assets::RUNTIME_ARCHIVE)
}

#[cfg(not(feature = "extensions"))]
pub(crate) fn runtime_archive() -> Option<&'static [u8]> {
    None
}

#[cfg(feature = "extensions")]
pub(crate) fn pgdata_template_archive() -> Option<&'static [u8]> {
    Some(pglite_oxide_assets::PGDATA_TEMPLATE_ARCHIVE)
}

#[cfg(not(feature = "extensions"))]
pub(crate) fn pgdata_template_archive() -> Option<&'static [u8]> {
    None
}

#[cfg(feature = "extensions")]
pub(crate) fn pgdata_template_manifest() -> Option<&'static [u8]> {
    Some(pglite_oxide_assets::PGDATA_TEMPLATE_MANIFEST)
}

#[cfg(not(feature = "extensions"))]
pub(crate) fn pgdata_template_manifest() -> Option<&'static [u8]> {
    None
}

#[cfg(feature = "extensions")]
pub(crate) fn extension_archive(sql_name: &str) -> Option<&'static [u8]> {
    pglite_oxide_assets::extension_archive(sql_name)
}

#[cfg(feature = "extensions")]
pub(crate) fn expected_runtime_archive_sha256() -> Result<String> {
    Ok(pglite_oxide_assets::manifest()
        .context("parse embedded asset manifest")?
        .runtime
        .sha256)
}

#[cfg(feature = "extensions")]
pub(crate) fn expected_extension_archive_sha256(sql_name: &str) -> Result<String> {
    pglite_oxide_assets::manifest()
        .context("parse embedded asset manifest")?
        .extensions
        .into_iter()
        .find(|extension| extension.sql_name == sql_name)
        .map(|extension| extension.sha256)
        .ok_or_else(|| anyhow!("extension asset '{sql_name}' is missing from asset manifest"))
}

#[cfg(feature = "extensions")]
pub(crate) fn expected_module_sha256(name: &str) -> Result<String> {
    let manifest = pglite_oxide_assets::manifest().context("parse embedded asset manifest")?;
    if name == "runtime:pglite" {
        return Ok(manifest.runtime.module_sha256);
    }
    if let Some(name) = name.strip_prefix("runtime-support:") {
        return manifest
            .runtime_support
            .into_iter()
            .find(|module| module.name == name)
            .map(|module| module.module_sha256)
            .ok_or_else(|| {
                anyhow!("runtime support module '{name}' is missing from asset manifest")
            });
    }
    if name == "tool:pg_dump" {
        return manifest
            .pg_dump
            .map(|module| module.module_sha256)
            .ok_or_else(|| anyhow!("pg_dump is missing from asset manifest"));
    }
    if let Some(sql_name) = name.strip_prefix("extension:") {
        return manifest
            .extensions
            .into_iter()
            .find(|extension| extension.sql_name == sql_name)
            .map(|extension| extension.module_sha256)
            .ok_or_else(|| anyhow!("extension '{sql_name}' is missing from asset manifest"));
    }
    Err(anyhow!("unknown asset module '{name}'"))
}
