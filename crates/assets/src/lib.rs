#![deny(unsafe_code)]

use serde::{Deserialize, Serialize};

pub const MANIFEST_JSON: &str = include_str!("../assets/manifest.json");
pub const RUNTIME_ARCHIVE: &[u8] = include_bytes!("../assets/pglite.wasix.tar.zst");
pub const PGDATA_TEMPLATE_ARCHIVE: &[u8] =
    include_bytes!("../assets/prepopulated/pgdata-template.tar.zst");
pub const PGDATA_TEMPLATE_MANIFEST: &[u8] =
    include_bytes!("../assets/prepopulated/pgdata-template.json");
pub const PG_DUMP_WASM: &[u8] = include_bytes!("../assets/bin/pg_dump.wasix.wasm");

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub struct AssetManifest {
    pub format_version: u32,
    pub runtime: RuntimeAsset,
    #[serde(default)]
    pub runtime_support: Vec<BinaryAsset>,
    #[serde(default)]
    pub pg_dump: Option<BinaryAsset>,
    #[serde(default)]
    pub extensions: Vec<ExtensionAsset>,
    #[serde(default)]
    pub sources: Vec<SourcePin>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub struct RuntimeAsset {
    pub archive: String,
    pub sha256: String,
    #[serde(default)]
    pub module_sha256: String,
    pub postgres_version: String,
    pub runtime_kind: String,
    #[serde(default)]
    pub link: Option<WasmLinkMetadata>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub struct BinaryAsset {
    pub name: String,
    pub path: String,
    pub sha256: String,
    #[serde(default)]
    pub module_sha256: String,
    pub size: u64,
    #[serde(default)]
    pub link: Option<WasmLinkMetadata>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub struct ExtensionAsset {
    pub name: String,
    pub sql_name: String,
    pub archive: String,
    pub sha256: String,
    #[serde(default)]
    pub module_sha256: String,
    pub size: u64,
    #[serde(default)]
    pub stable: bool,
    #[serde(default)]
    pub link: Option<WasmLinkMetadata>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub struct WasmLinkMetadata {
    pub has_dylink0: bool,
    #[serde(default)]
    pub dylink_needed: Vec<String>,
    #[serde(default)]
    pub dylink_runtime_paths: Vec<String>,
    #[serde(default)]
    pub dylink_memory: Option<WasmDylinkMemory>,
    #[serde(default)]
    pub dylink_imports: Vec<WasmDylinkSymbol>,
    #[serde(default)]
    pub dylink_exports: Vec<WasmDylinkSymbol>,
    #[serde(default)]
    pub imports: Vec<WasmImport>,
    #[serde(default)]
    pub exports: Vec<WasmExport>,
    #[serde(default)]
    pub memories: Vec<WasmMemory>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub struct WasmDylinkMemory {
    pub memory_size: u32,
    pub memory_alignment: u32,
    pub table_size: u32,
    pub table_alignment: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub struct WasmDylinkSymbol {
    pub module: Option<String>,
    pub name: String,
    pub flags: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub struct WasmImport {
    pub module: String,
    pub name: String,
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub struct WasmExport {
    pub name: String,
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub struct WasmMemory {
    pub initial_pages: u64,
    pub maximum_pages: Option<u64>,
    pub memory64: bool,
    pub shared: bool,
    pub page_size_log2: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub struct SourcePin {
    pub name: String,
    pub url: String,
    pub branch: String,
    pub commit: String,
}

pub fn manifest() -> Result<AssetManifest, serde_json::Error> {
    serde_json::from_str(MANIFEST_JSON)
}

pub fn extension_archive(name: &str) -> Option<&'static [u8]> {
    match name {
        "vector" => Some(extensions::VECTOR_ARCHIVE),
        "pg_trgm" => Some(extensions::PG_TRGM_ARCHIVE),
        _ => None,
    }
}

pub mod extensions {
    pub const VECTOR_ARCHIVE: &[u8] = include_bytes!("../assets/extensions/vector.tar.zst");
    pub const PG_TRGM_ARCHIVE: &[u8] = include_bytes!("../assets/extensions/pg_trgm.tar.zst");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_parses_and_lists_vector() {
        let manifest = manifest().expect("asset manifest should parse");
        assert_eq!(manifest.runtime.postgres_version, "17.5");
        assert_eq!(manifest.runtime.runtime_kind, "wasix-dynamic-main");
        assert!(
            manifest
                .extensions
                .iter()
                .any(|extension| extension.sql_name == "vector" && extension.stable)
        );
        assert!(
            manifest
                .extensions
                .iter()
                .any(|extension| extension.sql_name == "pg_trgm" && extension.stable)
        );
    }
}
