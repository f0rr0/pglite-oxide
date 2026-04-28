#![deny(unsafe_code)]

pub const TARGET_TRIPLE: &str = "aarch64-apple-darwin";
pub const ENGINE: &str = "llvm-opta";
pub const MANIFEST_JSON: &str = include_str!("../artifacts/manifest.json");

pub fn artifact_bytes(name: &str) -> Option<&'static [u8]> {
    match name {
        "runtime:pglite" => Some(include_bytes!("../artifacts/pglite-llvm-opta.bin.zst")),
        "runtime-support:plpgsql" => Some(include_bytes!("../artifacts/plpgsql-llvm-opta.bin.zst")),
        "runtime-support:dict_snowball" => Some(include_bytes!(
            "../artifacts/dict_snowball-llvm-opta.bin.zst"
        )),
        "extension:vector" => Some(include_bytes!("../artifacts/vector-llvm-opta.bin.zst")),
        "extension:pg_trgm" => Some(include_bytes!("../artifacts/pg_trgm-llvm-opta.bin.zst")),
        "tool:pg_dump" => Some(include_bytes!("../artifacts/pg_dump-llvm-opta.bin.zst")),
        _ => None,
    }
}
