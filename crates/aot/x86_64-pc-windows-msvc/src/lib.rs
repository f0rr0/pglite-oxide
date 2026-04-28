#![deny(unsafe_code)]

pub const TARGET_TRIPLE: &str = "x86_64-pc-windows-msvc";
pub const ENGINE: &str = "llvm-opta";
pub const MANIFEST_JSON: &str = r#"{"format-version":1,"target-triple":"x86_64-pc-windows-msvc","engine":"llvm-opta","artifacts":[]}"#;

pub fn artifact_bytes(_name: &str) -> Option<&'static [u8]> {
    None
}
