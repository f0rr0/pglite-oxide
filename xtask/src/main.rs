use std::collections::HashSet;
use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;
use wasmparser::{Dylink0Subsection, ExternalKind, KnownCustom, Parser, Payload, TypeRef};
use zstd::stream::write::Encoder as ZstdEncoder;

const POSTGRES_PGLITE_SOURCE: &str = "postgres-pglite";
const POSTGRES_PGLITE_PATH: &str = "assets/checkouts/postgres-pglite";
const PGLITE_BUILD_SOURCE: &str = "pglite-build";
const PGLITE_BUILD_PATH: &str = "assets/checkouts/pglite-build";
const WASIX_BUILD_ROOT: &str = "assets/wasix-build";
const WASIX_DOCKER_BUILD_DIR: &str = "assets/wasix-build/work/docker-pglite";
const WASIX_PATCHED_SOURCE_DIR: &str = "assets/wasix-build/work/postgres-pglite-wasix-src";
const WASIX_BUILD_MANIFEST_PATH: &str = "assets/wasix-build/build/outputs.json";
const WASIX_PATCH_PATH: &str = "assets/wasix-build/patches/postgres-pglite-wasix-dl.patch";
const WASIX_BRIDGE_PATH: &str = "assets/wasix-build/wasix_shim/pglite_wasix_bridge.c";
const PGVECTOR_BUILD_DIR: &str = "assets/checkouts/pgvector";
const EXPECTED_POSTGRES_PGLITE_BRANCH: &str = "REL_17_5_WASM-pglite-builder";
const EXPECTED_PGLITE_BUILD_BRANCH: &str = "portable";

fn main() -> Result<()> {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("assets") => assets(args.collect()),
        Some("package-size") => package_size(args.collect()),
        Some("perf") => perf(args.collect()),
        Some("help") | None => {
            print_usage();
            Ok(())
        }
        Some(other) => bail!("unknown xtask command: {other}"),
    }
}

fn assets(args: Vec<String>) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("check") => {
            let strict_local = args.iter().any(|arg| arg == "--strict-local");
            let strict_generated = args.iter().any(|arg| arg == "--strict-generated");
            let manifest = check_sources_manifest(strict_local)?;
            check_no_legacy_runtime_shims()?;
            check_production_wasix_build_inputs()?;
            check_rust_startup_abi_boundary()?;
            check_canonical_asset_layout(strict_generated)?;
            check_generated_manifest(&manifest, strict_generated)
        }
        Some("audit-upstream") => {
            let strict = args.iter().any(|arg| arg == "--strict");
            let manifest = check_sources_manifest(false)?;
            audit_upstream_fixes(&manifest, strict)
        }
        Some("build") => {
            let manifest = check_sources_manifest(false)?;
            let profile = value_after(&args, "--profile").unwrap_or("release");
            let target = value_after(&args, "--target-triple").unwrap_or(env::consts::ARCH);
            build_asset_spine(&manifest, profile, target, &args)
        }
        Some("fetch") => {
            let manifest = load_sources_manifest()?;
            validate_sources_manifest(&manifest)?;
            fetch_pinned_sources(&manifest)
        }
        Some("release-build") => {
            let manifest = check_sources_manifest(true)?;
            let profile = value_after(&args, "--profile").unwrap_or("release");
            let target = value_after(&args, "--target-triple").unwrap_or(host_target_triple());
            release_build_assets(&manifest, profile, target, &args)
        }
        Some("package") => {
            let manifest = check_sources_manifest(false)?;
            let target = value_after(&args, "--target-triple").unwrap_or(host_target_triple());
            package_assets(&manifest, target)
        }
        Some("aot") => {
            let target = value_after(&args, "--target-triple").unwrap_or(host_target_triple());
            generate_aot_artifacts(target)
        }
        Some("source-spine") => {
            let check_patch = args.iter().any(|arg| arg == "--check-patch-applies");
            let manifest = load_sources_manifest()?;
            validate_sources_manifest(&manifest)?;
            println!("validated {} pinned asset sources", manifest.sources.len());
            check_source_spine(&manifest, true, check_patch)
        }
        Some("smoke") => run("cargo", &["test", "--workspace", "--locked", "asset_"]),
        Some(other) => bail!("unknown assets subcommand: {other}"),
        None => {
            bail!(
                "usage: cargo run -p xtask -- assets <check|audit-upstream|source-spine|fetch|build|release-build|package|smoke>"
            )
        }
    }
}

fn package_size(args: Vec<String>) -> Result<()> {
    let enforce = args.iter().any(|arg| arg == "--enforce");
    let package_dir = Path::new("target/package");
    if !package_dir.exists() {
        fs::create_dir_all(package_dir)
            .with_context(|| format!("create {}", package_dir.display()))?;
    } else {
        fs::remove_dir_all(package_dir)
            .with_context(|| format!("remove {}", package_dir.display()))?;
    }
    run(
        "cargo",
        &[
            "package",
            "--workspace",
            "--exclude",
            "xtask",
            "--locked",
            "--no-verify",
            "--allow-dirty",
        ],
    )?;

    let limit = 10 * 1024 * 1024;
    let mut failures = Vec::new();
    for entry in WalkDir::new(package_dir).max_depth(1) {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("crate") {
            continue;
        }
        let size = entry.metadata()?.len();
        println!("{} {} bytes", path.display(), size);
        if size > limit {
            failures.push((path.to_path_buf(), size));
        }
    }

    if enforce && !failures.is_empty() {
        let details = failures
            .iter()
            .map(|(path, size)| format!("{} ({size} bytes)", path.display()))
            .collect::<Vec<_>>()
            .join(", ");
        bail!("crate package size limit exceeded: {details}");
    }
    Ok(())
}

fn perf(args: Vec<String>) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("smoke") => run(
            "cargo",
            &[
                "test",
                "--workspace",
                "--locked",
                "preload",
                "--",
                "--nocapture",
            ],
        ),
        Some(other) => bail!("unknown perf subcommand: {other}"),
        None => bail!("usage: cargo run -p xtask -- perf smoke"),
    }
}

fn check_sources_manifest(strict_local: bool) -> Result<SourcesManifest> {
    let manifest = load_sources_manifest()?;
    validate_sources_manifest(&manifest)?;
    check_source_spine(&manifest, strict_local, false)?;
    println!("validated {} pinned asset sources", manifest.sources.len());
    Ok(manifest)
}

fn fetch_pinned_sources(manifest: &SourcesManifest) -> Result<()> {
    run("git", &["submodule", "sync", "--recursive"])?;
    for source in &manifest.sources {
        let Some(path) = source_checkout_path(source.name.as_str()) else {
            eprintln!(
                "warning: source '{}' has no configured checkout path; skipping fetch",
                source.name
            );
            continue;
        };
        if !path.exists() {
            run(
                "git",
                &[
                    "submodule",
                    "update",
                    "--init",
                    "--recursive",
                    path.to_str().unwrap_or_default(),
                ],
            )?;
        }
        ensure_clean_checkout(path)?;
        let mut fetch = Command::new("git");
        fetch
            .args(["fetch", "origin", &source.commit, "--depth", "1"])
            .current_dir(path);
        run_command(&mut fetch).with_context(|| format!("fetch {}", source.name))?;
        let mut checkout = Command::new("git");
        checkout
            .args(["checkout", &source.commit])
            .current_dir(path);
        run_command(&mut checkout).with_context(|| {
            format!(
                "checkout {} at {} in {}",
                source.name,
                source.commit,
                path.display()
            )
        })?;
    }
    check_source_spine(manifest, true, false)
}

fn source_checkout_path(name: &str) -> Option<&'static Path> {
    match name {
        POSTGRES_PGLITE_SOURCE => Some(Path::new(POSTGRES_PGLITE_PATH)),
        PGLITE_BUILD_SOURCE => Some(Path::new(PGLITE_BUILD_PATH)),
        "pglite" => Some(Path::new("assets/checkouts/pglite")),
        "pgvector" => Some(Path::new(PGVECTOR_BUILD_DIR)),
        "pglite-bindings" => Some(Path::new("assets/checkouts/pglite-bindings")),
        _ => None,
    }
}

fn ensure_clean_checkout(path: &Path) -> Result<()> {
    if !path.exists() {
        bail!("source checkout is missing: {}", path.display());
    }
    let status = command_output("git", &["status", "--porcelain"], path)
        .with_context(|| format!("read status for {}", path.display()))?;
    if !status.trim().is_empty() {
        bail!(
            "source checkout {} has uncommitted changes; preserve them before fetching pins",
            path.display()
        );
    }
    Ok(())
}

fn load_sources_manifest() -> Result<SourcesManifest> {
    let path = Path::new("assets/sources.toml");
    let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    toml::from_str(&text).context("parse assets/sources.toml")
}

fn validate_sources_manifest(manifest: &SourcesManifest) -> Result<()> {
    if manifest.sources.is_empty() {
        bail!("assets/sources.toml must contain at least one source pin");
    }
    ensure_eq(
        &manifest.toolchain.wasmer,
        "7.2.0-alpha.2",
        "toolchain.wasmer",
    )?;
    ensure_eq(
        &manifest.toolchain.wasmer_wasix,
        "0.702.0-alpha.2",
        "toolchain.wasmer-wasix",
    )?;
    if !manifest
        .toolchain
        .docker_image_digest
        .strip_prefix("sha256:")
        .is_some_and(|digest| digest.len() == 64 && digest.chars().all(|ch| ch.is_ascii_hexdigit()))
    {
        bail!(
            "toolchain.docker_image_digest must pin a concrete sha256 digest, got {}",
            manifest.toolchain.docker_image_digest
        );
    }
    let dockerfile = fs::read_to_string("assets/wasix-build/docker/Dockerfile")
        .context("read WASIX build Dockerfile")?;
    if !dockerfile.contains(&format!(
        "FROM ubuntu:24.04@{}",
        manifest.toolchain.docker_image_digest
    )) {
        bail!("WASIX build Dockerfile must pin the same base image digest as assets/sources.toml");
    }
    ensure_eq(
        &manifest.build.postgres_prefix,
        "/",
        "build.postgres_prefix",
    )?;
    ensure_eq(
        &manifest.build.postgres_pkglibdir,
        "/lib/postgresql",
        "build.postgres_pkglibdir",
    )?;
    ensure_eq(
        &manifest.build.postgres_sharedir,
        "/share/postgresql",
        "build.postgres_sharedir",
    )?;
    ensure_contains(
        &manifest.build.main_flags,
        "-fwasm-exceptions",
        "build.main_flags",
    )?;
    ensure_contains(
        &manifest.build.extension_flags,
        "-fwasm-exceptions",
        "build.extension_flags",
    )?;
    ensure_contains(
        &manifest.build.extension_flags,
        "-fPIC",
        "build.extension_flags",
    )?;
    ensure_contains(
        &manifest.build.extension_flags,
        "-Wl,-shared",
        "build.extension_flags",
    )?;
    ensure_eq(
        &manifest.build.archive_format,
        "tar.zst",
        "build.archive_format",
    )?;
    if !manifest.build.deterministic_archives {
        bail!("build.deterministic_archives must be true");
    }
    for source in &manifest.sources {
        if source.name.trim().is_empty()
            || source.url.trim().is_empty()
            || source.branch.trim().is_empty()
            || source.commit.len() < 40
        {
            bail!("invalid source pin in assets/sources.toml: {source:?}");
        }
    }
    let postgres = source_by_name(manifest, POSTGRES_PGLITE_SOURCE)?;
    ensure_eq(
        &postgres.branch,
        EXPECTED_POSTGRES_PGLITE_BRANCH,
        "postgres-pglite source branch",
    )?;
    let pglite_build = source_by_name(manifest, PGLITE_BUILD_SOURCE)?;
    ensure_eq(
        &pglite_build.branch,
        EXPECTED_PGLITE_BUILD_BRANCH,
        "pglite-build source branch",
    )?;
    Ok(())
}

fn check_generated_manifest(manifest: &SourcesManifest, strict: bool) -> Result<()> {
    let path = Path::new("crates/assets/assets/manifest.json");
    if !path.exists() {
        if strict {
            bail!("generated asset manifest is missing at {}", path.display());
        }
        eprintln!(
            "warning: generated asset manifest is missing at {}",
            path.display()
        );
        return Ok(());
    }

    let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let generated: GeneratedAssetManifest =
        serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?;

    let mut drift = Vec::new();
    for source in &manifest.sources {
        match generated
            .sources
            .iter()
            .find(|generated| generated.name == source.name)
        {
            Some(generated)
                if generated.url == source.url
                    && generated.branch == source.branch
                    && generated.commit == source.commit => {}
            Some(generated) => drift.push(format!(
                "{} generated={}/{}@{} expected={}/{}@{}",
                source.name,
                generated.url,
                generated.branch,
                generated.commit,
                source.url,
                source.branch,
                source.commit
            )),
            None => drift.push(format!("{} missing from generated manifest", source.name)),
        }
    }

    if drift.is_empty() {
        println!("generated asset manifest source pins match assets/sources.toml");
        return Ok(());
    }

    let details = drift.join("; ");
    if strict {
        bail!("generated asset manifest has stale source pins: {details}");
    }
    eprintln!("warning: generated asset manifest has stale source pins: {details}");
    Ok(())
}

fn check_no_legacy_runtime_shims() -> Result<()> {
    let banned = [
        (
            "src/pglite/base.rs",
            &[
                "normalize_runtime_tree",
                "mirror_configured_share_layout",
                "mirror_configured_lib_layout",
                "normalize_pgdata_config",
                "share/timezonesets/Default",
                "write minimal timezoneset",
                "log_timezone = UTC",
                "timezone = UTC",
            ][..],
        ),
        (
            "src/pglite/postgres_mod.rs",
            &["pgl_startPGlite", "pgl_setPGliteActive"][..],
        ),
        (WASIX_BRIDGE_PATH, &["pgl_longjmp", "pgl_siglongjmp"][..]),
    ];

    let mut failures = Vec::new();
    for (path, patterns) in banned {
        let text = fs::read_to_string(path).with_context(|| format!("read {path}"))?;
        for pattern in patterns {
            if text.contains(pattern) {
                failures.push(format!(
                    "{path} contains legacy runtime shim marker {pattern:?}"
                ));
            }
        }
    }

    if !failures.is_empty() {
        bail!("{}", failures.join("; "));
    }
    println!("legacy runtime shim source guard passed");
    Ok(())
}

fn check_production_wasix_build_inputs() -> Result<()> {
    for required in [
        WASIX_PATCH_PATH,
        WASIX_BRIDGE_PATH,
        "assets/wasix-build/wasix_shim/pglite_wasix_bridge_abi_test.c",
        "assets/wasix-build/wasix_shim/pglite_wasix_shim.c",
        "assets/wasix-build/analyze_pgl_stubs.sh",
        "assets/wasix-build/configure_wasix_dl.sh",
        "assets/wasix-build/prepare_patched_source.sh",
        "assets/wasix-build/pg_config_wasix.sh",
        "assets/wasix-build/docker/Dockerfile",
        "assets/wasix-build/docker_pglite.sh",
        "assets/wasix-build/docker_runtime_support.sh",
        "assets/wasix-build/docker_pgvector.sh",
        "assets/wasix-build/docker_pgtrgm.sh",
        "assets/wasix-build/docker_pgdump.sh",
    ] {
        if !Path::new(required).exists() {
            bail!("production WASIX build input is missing: {required}");
        }
    }

    let legacy_root = ["spikes", "wasix-postgres-build"].join("/");
    let legacy_source_root = ["spikes", "upstream"].join("/");
    let production_files = [
        "xtask/src/main.rs",
        "assets/wasix-build/analyze_pgl_stubs.sh",
        "assets/wasix-build/configure_wasix_dl.sh",
        "assets/wasix-build/prepare_patched_source.sh",
        "assets/wasix-build/pg_config_wasix.sh",
        "assets/wasix-build/docker_pglite.sh",
        "assets/wasix-build/docker_runtime_support.sh",
        "assets/wasix-build/docker_pgvector.sh",
        "assets/wasix-build/docker_pgtrgm.sh",
        "assets/wasix-build/docker_pgdump.sh",
    ];
    for path in production_files {
        let text = fs::read_to_string(path).with_context(|| format!("read {path}"))?;
        if text.contains(&legacy_root) {
            bail!("{path} still depends on legacy production build root {legacy_root}");
        }
        if text.contains(&legacy_source_root) {
            bail!("{path} still depends on historical source checkout root {legacy_source_root}");
        }
    }

    println!("production WASIX build input guard passed");
    Ok(())
}

fn check_rust_startup_abi_boundary() -> Result<()> {
    let path = Path::new("src/pglite/postgres_mod.rs");
    let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;

    for marker in [
        "struct PgliteLifecycleExports",
        "struct WasixProtocolExports",
        "fn ensure_no_js_lifecycle_contract",
        "The upstream lifecycle is already running by this point",
    ] {
        if !text.contains(marker) {
            bail!(
                "{} must keep upstream lifecycle exports separate from WASIX protocol ABI; missing {marker:?}",
                path.display()
            );
        }
    }
    if text.contains("struct Exports") {
        bail!(
            "{} must not collapse PGlite lifecycle and WASIX protocol exports into a generic Exports struct",
            path.display()
        );
    }

    let lifecycle_start = text
        .find("struct PgliteLifecycleExports")
        .ok_or_else(|| anyhow!("missing PgliteLifecycleExports"))?;
    let protocol_start = text
        .find("struct WasixProtocolExports")
        .ok_or_else(|| anyhow!("missing WasixProtocolExports"))?;
    let lifecycle_block = &text[lifecycle_start..protocol_start];
    for protocol_marker in [
        "ProcessStartupPacket",
        "PostgresMainLoopOnce",
        "pgl_wasix_input",
    ] {
        if lifecycle_block.contains(protocol_marker) {
            bail!(
                "{} lifecycle export block leaked WASIX protocol marker {protocol_marker:?}",
                path.display()
            );
        }
    }

    println!("Rust startup ABI boundary guard passed");
    Ok(())
}

fn check_canonical_asset_layout(strict: bool) -> Result<()> {
    let runtime_archive = Path::new("crates/assets/assets/pglite.wasix.tar.zst");
    if !runtime_archive.exists() {
        if strict {
            bail!(
                "runtime asset archive is missing at {}",
                runtime_archive.display()
            );
        }
        eprintln!(
            "warning: runtime asset archive is missing at {}",
            runtime_archive.display()
        );
        return Ok(());
    }

    let runtime_entries = archive_entries(runtime_archive)?;
    for required in [
        "pglite/bin/pglite",
        "pglite/bin/pg_dump",
        "pglite/lib/postgresql/plpgsql.so",
        "pglite/share/postgresql/extension/plpgsql.control",
        "pglite/share/postgresql/timezone/UTC",
        "pglite/share/postgresql/timezone/America/New_York",
        "pglite/share/postgresql/timezonesets/Default",
    ] {
        if !runtime_entries.contains(required) {
            bail!(
                "runtime archive {} is missing canonical path {required}",
                runtime_archive.display()
            );
        }
    }
    for forbidden in [
        "pglite/share/extension",
        "pglite/share/timezonesets",
        "pglite/lib/plpgsql.so",
        "pglite/lib/dict_snowball.so",
    ] {
        if runtime_entries.contains(forbidden)
            || runtime_entries
                .iter()
                .any(|entry| entry.starts_with(&format!("{forbidden}/")))
        {
            bail!(
                "runtime archive {} contains non-canonical duplicate path {forbidden}",
                runtime_archive.display()
            );
        }
    }

    let extensions_dir = Path::new("crates/assets/assets/extensions");
    if extensions_dir.exists() {
        for entry in fs::read_dir(extensions_dir)
            .with_context(|| format!("read {}", extensions_dir.display()))?
        {
            let path = entry?.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("zst") {
                continue;
            }
            check_extension_archive_layout(&path)?;
        }
    } else if strict {
        bail!(
            "extension asset directory is missing at {}",
            extensions_dir.display()
        );
    }

    println!("canonical asset layout guard passed");
    Ok(())
}

fn check_extension_archive_layout(path: &Path) -> Result<()> {
    let entries = archive_entries(path)?;
    for entry in entries {
        if matches!(
            entry.as_str(),
            "lib" | "lib/postgresql" | "share" | "share/postgresql" | "share/postgresql/extension"
        ) {
            continue;
        }
        if entry.starts_with("lib/postgresql/") || entry.starts_with("share/postgresql/extension/")
        {
            continue;
        }
        bail!(
            "extension archive {} contains non-canonical path {entry}",
            path.display()
        );
    }
    Ok(())
}

fn archive_entries(path: &Path) -> Result<HashSet<String>> {
    let file = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let decoder = zstd::stream::read::Decoder::new(file)
        .with_context(|| format!("decode {}", path.display()))?;
    let mut archive = tar::Archive::new(decoder);
    let mut entries = HashSet::new();
    for entry in archive
        .entries()
        .with_context(|| format!("read entries from {}", path.display()))?
    {
        let entry = entry.with_context(|| format!("read entry from {}", path.display()))?;
        let entry_path = entry
            .path()
            .with_context(|| format!("read entry path from {}", path.display()))?;
        let entry = entry_path
            .to_str()
            .ok_or_else(|| anyhow!("archive {} has non-UTF-8 path", path.display()))?
            .trim_start_matches("./")
            .trim_end_matches('/')
            .to_string();
        if !entry.is_empty() {
            entries.insert(entry);
        }
    }
    Ok(entries)
}

fn audit_upstream_fixes(manifest: &SourcesManifest, strict: bool) -> Result<()> {
    let checkout = Path::new(POSTGRES_PGLITE_PATH);
    if !checkout.exists() {
        bail!("missing local checkout {}", checkout.display());
    }
    let postgres = source_by_name(manifest, POSTGRES_PGLITE_SOURCE)?;
    println!(
        "auditing upstream fixes against {} {}",
        postgres.branch, postgres.commit
    );

    let mut pending_required = Vec::new();
    for item in UPSTREAM_AUDIT {
        let status = if is_git_ancestor(checkout, item.commit)? {
            "included".to_owned()
        } else if let Some(replacement) = replacement_for_upstream_item(item.id)? {
            format!("replaced ({replacement})")
        } else if item.required {
            pending_required.push(item.id);
            "pending".to_owned()
        } else {
            "optional".to_owned()
        };
        println!(
            "{status:32} {} {} - {}",
            item.id, item.commit, item.description
        );
    }

    if strict && !pending_required.is_empty() {
        bail!(
            "required upstream fixes are not included in the active source branch: {}",
            pending_required.join(", ")
        );
    }
    Ok(())
}

fn replacement_for_upstream_item(id: &str) -> Result<Option<&'static str>> {
    match id {
        "stable-protocol-exports" => {
            ensure_file_contains_all(
                WASIX_PATCH_PATH,
                &[
                    "pgl_getMyProcPort",
                    "pgl_sendConnData",
                    "PostgresMainLoopOnce",
                    "PostgresSendReadyForQueryIfNecessary",
                    "PostgresRecoverProtocolError",
                    "ProcessStartupPacket",
                ],
            )?;
            ensure_file_contains_all(
                "src/pglite/postgres_mod.rs",
                &["PgliteLifecycleExports", "WasixProtocolExports"],
            )?;
            ensure_file_contains_all(
                "tests/client_compat.rs",
                &[
                    "sqlx_extended_query_errors_recover_after_sync",
                    "raw_wire_protocol_bind_errors_are_synchronized",
                    "postgres_control_packets_are_handled_safely",
                ],
            )?;
            Ok(Some("WASIX protocol ABI + client/raw-wire tests"))
        }
        "stable-checkpointer-disable" => {
            ensure_file_contains_all(
                WASIX_PATCH_PATH,
                &[
                    "RequestCheckpoint(CHECKPOINT_CAUSE_XLOG)",
                    "#ifndef __PGLITE__",
                    "#endif",
                ],
            )?;
            ensure_file_contains_all(
                "tests/runtime_smoke.rs",
                &["persistent_fresh_initdb_survives_restart_and_stale_state_files"],
            )?;
            Ok(Some("ported into wasix-dl patch"))
        }
        "stable-imported-memory" => {
            ensure_file_contains_all(
                "assets/wasix-build/configure_wasix_dl.sh",
                &[
                    "-sMODULE_KIND=dynamic-main",
                    "-sWASM_EXCEPTIONS=yes",
                    "-Wl,-shared",
                ],
            )?;
            ensure_file_contains_all(
                "crates/assets/assets/manifest.json",
                &["wasix-dynamic-main"],
            )?;
            Ok(Some("WASIX dynamic-main/side-module memory contract"))
        }
        "stable-postgres-user" => {
            ensure_file_contains_all(
                WASIX_BRIDGE_PATH,
                &["static char name[] = \"postgres\"", "\"/home/postgres\""],
            )?;
            ensure_file_contains_all(
                "src/pglite/postgres_mod.rs",
                &[
                    "(\"PGUSER\", \"postgres\")",
                    "(\"PGDATABASE\", \"template1\")",
                ],
            )?;
            ensure_file_contains_all(
                "tests/runtime_smoke.rs",
                &["current_user", "session_user", "Some(&json!(\"postgres\"))"],
            )?;
            Ok(Some("WASIX identity bridge + runtime smoke tests"))
        }
        _ => Ok(None),
    }
}

fn ensure_file_contains_all(path: &str, markers: &[&str]) -> Result<()> {
    let text = fs::read_to_string(path).with_context(|| format!("read {path}"))?;
    let missing = markers
        .iter()
        .copied()
        .filter(|marker| !text.contains(marker))
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        bail!(
            "{path} is missing required upstream replacement markers: {}",
            missing.join(", ")
        );
    }
    Ok(())
}

fn is_git_ancestor(checkout: &Path, commit: &str) -> Result<bool> {
    let status = Command::new("git")
        .args(["merge-base", "--is-ancestor", commit, "HEAD"])
        .current_dir(checkout)
        .status()
        .with_context(|| format!("check whether {commit} is in {}", checkout.display()))?;
    match status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => bail!("git merge-base failed for {commit} with {status}"),
    }
}

fn check_source_spine(
    manifest: &SourcesManifest,
    strict_local: bool,
    check_patch_applies: bool,
) -> Result<()> {
    let postgres = source_by_name(manifest, POSTGRES_PGLITE_SOURCE)?;
    let gitmodules_path = command_output(
        "git",
        &[
            "config",
            "-f",
            ".gitmodules",
            "--get",
            "submodule.assets/checkouts/postgres-pglite.path",
        ],
        Path::new("."),
    )
    .context("read postgres-pglite path from .gitmodules")?;
    ensure_eq(
        gitmodules_path.trim(),
        POSTGRES_PGLITE_PATH,
        ".gitmodules postgres-pglite path",
    )?;
    let gitmodules_branch = command_output(
        "git",
        &[
            "config",
            "-f",
            ".gitmodules",
            "--get",
            "submodule.assets/checkouts/postgres-pglite.branch",
        ],
        Path::new("."),
    )
    .context("read postgres-pglite branch from .gitmodules")?;
    ensure_eq(
        gitmodules_branch.trim(),
        EXPECTED_POSTGRES_PGLITE_BRANCH,
        ".gitmodules postgres-pglite branch",
    )?;
    let pglite_build = source_by_name(manifest, PGLITE_BUILD_SOURCE)?;
    let gitmodules_build_path = command_output(
        "git",
        &[
            "config",
            "-f",
            ".gitmodules",
            "--get",
            "submodule.assets/checkouts/pglite-build.path",
        ],
        Path::new("."),
    )
    .context("read pglite-build path from .gitmodules")?;
    ensure_eq(
        gitmodules_build_path.trim(),
        PGLITE_BUILD_PATH,
        ".gitmodules pglite-build path",
    )?;
    let gitmodules_build_branch = command_output(
        "git",
        &[
            "config",
            "-f",
            ".gitmodules",
            "--get",
            "submodule.assets/checkouts/pglite-build.branch",
        ],
        Path::new("."),
    )
    .context("read pglite-build branch from .gitmodules")?;
    ensure_eq(
        gitmodules_build_branch.trim(),
        EXPECTED_PGLITE_BUILD_BRANCH,
        ".gitmodules pglite-build branch",
    )?;

    let patch = Path::new(WASIX_PATCH_PATH);
    if !patch.exists() {
        bail!("missing WASIX source patch at {}", patch.display());
    }
    let patch_text =
        fs::read_to_string(patch).with_context(|| format!("read {}", patch.display()))?;
    let required_patch_markers = [
        "src/template/wasix-dl",
        "src/makefiles/Makefile.wasix-dl",
        "src/include/port/wasix-dl.h",
        "src/include/port/wasix-dl/sys/ipc.h",
        "src/include/port/wasix-dl/sys/shm.h",
        "pglite_run_initdb_boot_phase",
        "pglite_restore_stdin_after_initdb_boot",
        "pglite_run_initdb_single_phase",
        "pglite_open_initdb_pipe",
        "wasm_dl_extension_imports_dir",
        "PGLITE_WASIX_DL build personality",
        "pgl_stubs.h frontend utility replacements are only maintained for PGLITE_WASIX_DL",
    ];
    let missing_patch_markers = required_patch_markers
        .iter()
        .copied()
        .filter(|marker| !patch_text.contains(marker))
        .collect::<Vec<_>>();
    if !missing_patch_markers.is_empty() {
        bail!(
            "WASIX patch {} is missing expected source-spine entries: {}",
            patch.display(),
            missing_patch_markers.join(", ")
        );
    }
    let banned_added_patch_markers = [
        "#pragma warning \"-------------------- TEST",
        "return stderr;",
        "popen[%s]",
        "pg_pclose(%s)",
        "ProcessStartupPacket: STUB",
        "select_default_timezone(%s): STUB",
        "emscripten_extension_imports_dir :=",
    ];
    let mut banned_patch_additions = Vec::new();
    for marker in banned_added_patch_markers {
        if patch_adds_marker(&patch_text, marker) {
            banned_patch_additions.push(marker);
        }
    }
    if !banned_patch_additions.is_empty() {
        bail!(
            "WASIX patch {} reintroduces spike debug/shim additions: {}",
            patch.display(),
            banned_patch_additions.join(", ")
        );
    }
    let bridge = Path::new(WASIX_BRIDGE_PATH);
    if !bridge.exists() {
        bail!("missing WASIX PGlite bridge at {}", bridge.display());
    }
    let bridge_text =
        fs::read_to_string(bridge).with_context(|| format!("read {}", bridge.display()))?;
    if !bridge_text.contains("pgl_wasix_input_write")
        || !bridge_text.contains("pgl_recv")
        || !bridge_text.contains("pgl_shmget")
        || !bridge_text.contains("strcmp(command, \"locale -a\") != 0")
        || !bridge_text.contains("strcmp(mode, \"r\") != 0")
        || !bridge_text.contains("static char name[] = \"postgres\"")
        || !bridge_text.contains("PGLITE_PROTOCOL_FD")
        || !bridge_text.contains("pgl_write_int_sockopt")
        || !bridge_text.contains("errno = ENOPROTOOPT")
        || !bridge_text.contains("return recv(fd, buf, n, flags)")
        || !bridge_text.contains("return send(fd, buf, n, flags)")
        || !bridge_text.contains("return connect(socket, address, address_len)")
        || !bridge_text.contains("return munmap(addr, length)")
        || !bridge_text.contains("return poll(fds, nfds, timeout)")
    {
        bail!(
            "WASIX bridge {} does not contain expected protocol/socket/shared-memory/locale identity allowlisted ABI",
            bridge.display()
        );
    }
    for banned in [
        "(void) level;\n\t(void) optname;\n\t(void) optval;\n\t(void) optlen;\n\treturn 0;",
        "(void) addr;\n\t(void) len;\n\treturn 0;",
        "(void) fd;\n\t(void) flags;\n\treturn pgl_wasix_buffer_read",
        "(void) fd;\n\t(void) flags;\n\treturn pgl_wasix_buffer_write",
        "(void) addr;\n\t(void) length;\n\treturn 0;",
        "fds[i].revents = fds[i].events;",
    ] {
        if bridge_text.contains(banned) {
            bail!(
                "WASIX bridge {} reintroduced broad fake-success socket/fd behavior: {}",
                bridge.display(),
                banned.escape_debug()
            );
        }
    }
    if bridge_text.contains("return 123;") {
        bail!(
            "WASIX bridge {} reintroduced a magic successful-looking system() status",
            bridge.display()
        );
    }
    if !bridge_text.contains("pgl_system(const char *command)")
        || !bridge_text.contains("errno = ENOSYS;")
        || !bridge_text.contains("return -1;")
    {
        bail!(
            "WASIX bridge {} must fail unsupported system() calls closed with ENOSYS",
            bridge.display()
        );
    }
    let stub_analysis = Path::new("assets/wasix-build/analyze_pgl_stubs.sh");
    if !stub_analysis.exists() {
        bail!(
            "missing pgl_stubs link-symbol analysis script at {}",
            stub_analysis.display()
        );
    }
    let stub_analysis_text = fs::read_to_string(stub_analysis)
        .with_context(|| format!("read {}", stub_analysis.display()))?;
    for marker in [
        "Runtime link inputs requiring pglite-wasm ownership",
        "Frontend tool inputs requiring frontend/common ownership",
        "do not by themselves justify keeping symbols in pglite-wasm/pgl_stubs.h",
    ] {
        if !stub_analysis_text.contains(marker) {
            bail!(
                "{} must keep runtime pgl_stubs ownership separate from frontend tool symbols",
                stub_analysis.display()
            );
        }
    }
    check_wasix_bridge_abi_harness()?;
    for script in [
        "assets/wasix-build/docker_pglite.sh",
        "assets/wasix-build/docker_runtime_support.sh",
        "assets/wasix-build/docker_pgvector.sh",
        "assets/wasix-build/docker_pgtrgm.sh",
        "assets/wasix-build/docker_pgdump.sh",
    ] {
        let text = fs::read_to_string(script).with_context(|| format!("read {script}"))?;
        if !text.contains(".pglite-oxide-bridge-sha256") {
            bail!("{script} must validate the WASIX bridge hash before reusing build outputs");
        }
    }
    let docker_pglite = fs::read_to_string("assets/wasix-build/docker_pglite.sh")
        .context("read assets/wasix-build/docker_pglite.sh")?;
    if !docker_pglite.contains("/usr/sbin/zic")
        || !docker_pglite.contains("src/timezone/compiled/UTC")
    {
        bail!(
            "docker_pglite.sh must compile pinned PostgreSQL timezone data inside the pinned Docker build"
        );
    }
    let docker_pgvector = fs::read_to_string("assets/wasix-build/docker_pgvector.sh")
        .context("read assets/wasix-build/docker_pgvector.sh")?;
    if !docker_pgvector.contains("-e PGVECTOR=\"$CONTAINER_PGVECTOR\"")
        || !docker_pgvector.contains("make -s -j\"$JOBS\" -C \"$PGVECTOR\"")
    {
        bail!("docker_pgvector.sh must build the pinned pgvector checkout via the PGVECTOR input");
    }

    let checkout = Path::new(POSTGRES_PGLITE_PATH);
    if !checkout.exists() {
        if strict_local {
            bail!("missing local checkout {}", checkout.display());
        }
        eprintln!("warning: local checkout {} is missing", checkout.display());
        return Ok(());
    }

    let head = command_output("git", &["rev-parse", "HEAD"], checkout)
        .with_context(|| format!("read HEAD for {}", checkout.display()))?;
    let branch = command_output("git", &["branch", "--show-current"], checkout)
        .unwrap_or_else(|_| String::from("<detached>"));
    if strict_local && head.trim() != postgres.commit {
        bail!(
            "local {} checkout is at {}, expected {} from assets/sources.toml",
            checkout.display(),
            head.trim(),
            postgres.commit
        );
    }
    if strict_local && branch.trim() != postgres.branch {
        bail!(
            "local {} checkout is on branch '{}', expected '{}'",
            checkout.display(),
            branch.trim(),
            postgres.branch
        );
    }
    if !strict_local && head.trim() != postgres.commit {
        eprintln!(
            "warning: local {} checkout is at {}, expected {}",
            checkout.display(),
            head.trim(),
            postgres.commit
        );
    }

    let status = command_output("git", &["status", "--porcelain"], checkout)
        .with_context(|| format!("read status for {}", checkout.display()))?;
    if strict_local && !status.trim().is_empty() {
        bail!(
            "local {} checkout has uncommitted changes; preserve them as a patch before strict asset builds",
            checkout.display()
        );
    }
    if !strict_local && !status.trim().is_empty() {
        eprintln!(
            "warning: local {} checkout has uncommitted changes",
            checkout.display()
        );
    }

    let pglite_build_checkout = Path::new(PGLITE_BUILD_PATH);
    if !pglite_build_checkout.exists() {
        if strict_local {
            bail!("missing local checkout {}", pglite_build_checkout.display());
        }
        eprintln!(
            "warning: local checkout {} is missing",
            pglite_build_checkout.display()
        );
    } else {
        let build_head = command_output("git", &["rev-parse", "HEAD"], pglite_build_checkout)
            .with_context(|| format!("read HEAD for {}", pglite_build_checkout.display()))?;
        let build_branch =
            command_output("git", &["branch", "--show-current"], pglite_build_checkout)
                .unwrap_or_else(|_| String::from("<detached>"));
        if strict_local && build_head.trim() != pglite_build.commit {
            bail!(
                "local {} checkout is at {}, expected {} from assets/sources.toml",
                pglite_build_checkout.display(),
                build_head.trim(),
                pglite_build.commit
            );
        }
        if !strict_local && build_head.trim() != pglite_build.commit {
            eprintln!(
                "warning: local {} checkout is at {}, expected {}",
                pglite_build_checkout.display(),
                build_head.trim(),
                pglite_build.commit
            );
        }
        if strict_local && build_branch.trim() != pglite_build.branch {
            bail!(
                "local {} checkout is on branch '{}', expected '{}'",
                pglite_build_checkout.display(),
                build_branch.trim(),
                pglite_build.branch
            );
        }
        let build_status = command_output("git", &["status", "--porcelain"], pglite_build_checkout)
            .with_context(|| format!("read status for {}", pglite_build_checkout.display()))?;
        if strict_local && !build_status.trim().is_empty() {
            bail!(
                "local {} checkout has uncommitted changes; preserve them before strict asset builds",
                pglite_build_checkout.display()
            );
        }
        if !strict_local && !build_status.trim().is_empty() {
            eprintln!(
                "warning: local {} checkout has uncommitted changes",
                pglite_build_checkout.display()
            );
        }

        let shared_build_scripts = [
            "wasm-build/build-ext.sh",
            "wasm-build/build-pgcore.sh",
            "wasm-build/extension.sh",
            "wasm-build/getsyms.py",
            "wasm-build/linkimports.sh",
            "wasm-build/pack_extension.py",
            "wasm-build/reqsym.py",
        ];
        for relative in shared_build_scripts {
            let build_script = pglite_build_checkout.join(relative);
            let builder_branch_script = checkout.join(relative);
            let build_text = fs::read_to_string(&build_script)
                .with_context(|| format!("read {}", build_script.display()))?;
            let builder_branch_text = fs::read_to_string(&builder_branch_script)
                .with_context(|| format!("read {}", builder_branch_script.display()))?;
            if build_text != builder_branch_text {
                bail!(
                    "{} drifted between {} and {}; update the audit before changing the source spine",
                    relative,
                    pglite_build_checkout.display(),
                    checkout.display()
                );
            }
        }
    }

    let required_upstream_markers = [
        ("pglite-wasm/pg_main.c", "pgl_initdb_main"),
        ("pglite-wasm/pgl_mains.c", "InitPostgres"),
        ("pglite-wasm/interactive_one.c", "ProcessStartupPacket"),
        ("pglite-wasm/pg_proto.c", "pq_getmsgstring"),
        ("pglite-wasm/pgl_stubs.h", "ProcessStartupPacket"),
        ("wasm-build/build-pgcore.sh", "wasi-shared"),
        ("wasm-build/getsyms.py", "wasm-objdump -x"),
        ("wasm-build/linkimports.sh", "_interactive_one"),
        ("wasm-build/pack_extension.py", "PG_DIST_EXT"),
        ("src/Makefile.shlib", "LINK.shared       = wasi-shared"),
        ("src/bin/initdb/initdb.c", "WASM_USERNAME"),
        ("src/port/getopt.c", "sdk_getopt"),
        ("src/interfaces/libpq/fe-misc.c", "sdk_sock_flush"),
        ("src/backend/commands/async.c", "HandleNotifyInterrupt"),
        ("pglite/Makefile", "pg_hashids"),
        ("pglite/Makefile", "pg_uuidv7"),
    ];
    let mut missing_upstream_markers = Vec::new();
    for (relative, marker) in required_upstream_markers {
        let path = checkout.join(relative);
        let text = fs::read_to_string(&path).unwrap_or_default();
        if !text.contains(marker) {
            missing_upstream_markers.push(format!("{relative}:{marker}"));
        }
    }
    if !missing_upstream_markers.is_empty() {
        bail!(
            "local {} checkout is missing expected PGlite builder protocol/lifecycle markers: {}",
            checkout.display(),
            missing_upstream_markers.join(", ")
        );
    }

    if check_patch_applies {
        let patch_path =
            fs::canonicalize(patch).with_context(|| format!("canonicalize {}", patch.display()))?;
        let status = Command::new("git")
            .args(["apply", "--check", "--whitespace=nowarn"])
            .arg(&patch_path)
            .current_dir(checkout)
            .status()
            .with_context(|| format!("check whether {} applies", patch.display()))?;
        if !status.success() {
            bail!(
                "WASIX patch {} does not apply cleanly to {}; rebase it before Phase 1 is complete",
                patch.display(),
                checkout.display()
            );
        }
    }

    Ok(())
}

fn patch_adds_marker(patch_text: &str, marker: &str) -> bool {
    patch_text
        .lines()
        .any(|line| line.starts_with('+') && !line.starts_with("+++") && line.contains(marker))
}

#[cfg(unix)]
fn check_wasix_bridge_abi_harness() -> Result<()> {
    let bridge = Path::new(WASIX_BRIDGE_PATH);
    let harness = Path::new("assets/wasix-build/wasix_shim/pglite_wasix_bridge_abi_test.c");
    if !harness.exists() {
        bail!("missing WASIX bridge ABI harness at {}", harness.display());
    }

    let out_dir = Path::new("target/xtask");
    fs::create_dir_all(out_dir).with_context(|| format!("create {}", out_dir.display()))?;
    let binary = out_dir.join("pglite_wasix_bridge_abi_test");
    let cc = env::var("CC").unwrap_or_else(|_| "cc".to_owned());
    let status = Command::new(&cc)
        .args(["-std=c11", "-Wall", "-Wextra"])
        .arg(bridge)
        .arg(harness)
        .arg("-o")
        .arg(&binary)
        .status()
        .with_context(|| format!("compile WASIX bridge ABI harness with {cc}"))?;
    if !status.success() {
        bail!("WASIX bridge ABI harness compilation failed with {status}");
    }
    let status = Command::new(&binary)
        .status()
        .with_context(|| format!("run {}", binary.display()))?;
    if !status.success() {
        bail!("WASIX bridge ABI harness failed with {status}");
    }
    println!("WASIX bridge ABI harness passed");
    Ok(())
}

#[cfg(not(unix))]
fn check_wasix_bridge_abi_harness() -> Result<()> {
    eprintln!("warning: skipping POSIX WASIX bridge ABI harness on non-Unix host");
    Ok(())
}

struct BuildOutputs {
    build_dir: PathBuf,
    source_dir: PathBuf,
    package_stage: PathBuf,
    modules: Vec<BuildModuleOutput>,
}

struct BuildModuleOutput {
    name: &'static str,
    kind: &'static str,
    path: PathBuf,
    aot_file: &'static str,
}

impl BuildOutputs {
    fn discover() -> Result<Self> {
        let build_dir = PathBuf::from(WASIX_DOCKER_BUILD_DIR);
        let source_dir = PathBuf::from(WASIX_PATCHED_SOURCE_DIR);
        let package_stage = PathBuf::from(WASIX_BUILD_ROOT).join("build/package-stage");
        let modules = vec![
            BuildModuleOutput {
                name: "runtime:pglite",
                kind: "runtime",
                path: build_dir.join("src/backend/pglite"),
                aot_file: "pglite-llvm-opta.bin.zst",
            },
            BuildModuleOutput {
                name: "runtime-support:plpgsql",
                kind: "runtime-support",
                path: build_dir.join("src/pl/plpgsql/src/plpgsql.so"),
                aot_file: "plpgsql-llvm-opta.bin.zst",
            },
            BuildModuleOutput {
                name: "runtime-support:dict_snowball",
                kind: "runtime-support",
                path: build_dir.join("src/backend/snowball/dict_snowball.so"),
                aot_file: "dict_snowball-llvm-opta.bin.zst",
            },
            BuildModuleOutput {
                name: "extension:vector",
                kind: "extension",
                path: PathBuf::from(PGVECTOR_BUILD_DIR).join("vector.so"),
                aot_file: "vector-llvm-opta.bin.zst",
            },
            BuildModuleOutput {
                name: "extension:pg_trgm",
                kind: "extension",
                path: build_dir.join("contrib/pg_trgm/pg_trgm.so"),
                aot_file: "pg_trgm-llvm-opta.bin.zst",
            },
            BuildModuleOutput {
                name: "tool:pg_dump",
                kind: "tool",
                path: build_dir.join("src/bin/pg_dump/pg_dump"),
                aot_file: "pg_dump-llvm-opta.bin.zst",
            },
        ];

        let outputs = Self {
            build_dir,
            source_dir,
            package_stage,
            modules,
        };
        outputs.ensure_required_files()?;
        Ok(outputs)
    }

    fn ensure_required_files(&self) -> Result<()> {
        for module in &self.modules {
            ensure_file(&module.path)?;
        }
        ensure_file(&self.build_dir.join("src/timezone/compiled/UTC"))?;
        ensure_file(
            &self
                .build_dir
                .join("src/backend/snowball/snowball_create.sql"),
        )?;
        Ok(())
    }

    fn module_path(&self, name: &str) -> Result<&Path> {
        self.modules
            .iter()
            .find(|module| module.name == name)
            .map(|module| module.path.as_path())
            .ok_or_else(|| anyhow!("missing build output module {name}"))
    }

    fn write_manifest(&self) -> Result<()> {
        let manifest = BuildOutputManifestOut {
            format_version: 1,
            modules: self
                .modules
                .iter()
                .map(|module| {
                    Ok(BuildModuleManifestOut {
                        name: module.name.to_owned(),
                        kind: module.kind.to_owned(),
                        path: module.path.to_string_lossy().into_owned(),
                        sha256: sha256_file(&module.path)?,
                        link: read_wasm_link_metadata(&module.path)?,
                    })
                })
                .collect::<Result<Vec<_>>>()?,
        };
        for module in &manifest.modules {
            validate_module_link_metadata(module)?;
        }
        let text = serde_json::to_string_pretty(&manifest)
            .context("serialize WASIX build output manifest")?;
        let path = Path::new(WASIX_BUILD_MANIFEST_PATH);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }
        fs::write(path, format!("{text}\n")).with_context(|| format!("write {}", path.display()))
    }
}

fn validate_module_link_metadata(module: &BuildModuleManifestOut) -> Result<()> {
    if module.link.exports.is_empty() {
        bail!("{} has no WASM exports", module.name);
    }

    match module.kind.as_str() {
        "runtime" => {
            let required = [
                "pgl_initdb",
                "pgl_backend",
                "pgl_getMyProcPort",
                "ProcessStartupPacket",
                "pgl_sendConnData",
                "pgl_pq_flush",
                "PostgresMainLoopOnce",
                "PostgresSendReadyForQueryIfNecessary",
                "PostgresRecoverProtocolError",
                "pgl_wasix_input_reset",
                "pgl_wasix_input_write",
                "pgl_wasix_input_available",
                "pgl_wasix_output_reset",
                "pgl_wasix_output_len",
                "pgl_wasix_output_read",
            ];
            let missing = required
                .iter()
                .copied()
                .filter(|export| !has_wasm_export(&module.link, export))
                .collect::<Vec<_>>();
            if !missing.is_empty() {
                bail!(
                    "{} is missing required Rust/WASIX ABI exports: {}",
                    module.name,
                    missing.join(", ")
                );
            }
            for banned in ["pgl_startPGlite", "pgl_setPGliteActive"] {
                if has_wasm_export(&module.link, banned) {
                    bail!(
                        "{} exports JS/Emscripten lifecycle entrypoint {banned}",
                        module.name
                    );
                }
            }
        }
        "runtime-support" | "extension" => {
            if !module.link.has_dylink0 {
                bail!("{} is not a WASM dynamic-linking side module", module.name);
            }
            if module.link.imports.is_empty() && module.link.dylink_imports.is_empty() {
                bail!(
                    "{} has no imports; side-module linkage is suspicious",
                    module.name
                );
            }
        }
        "tool" => {}
        other => bail!("{} has unknown build output kind {other}", module.name),
    }

    Ok(())
}

fn validate_build_output_link_closure(outputs: &BuildOutputs) -> Result<()> {
    let runtime = outputs
        .modules
        .iter()
        .find(|module| module.kind == "runtime")
        .ok_or_else(|| anyhow!("build outputs are missing runtime module"))?;
    let runtime_link = read_wasm_link_metadata(&runtime.path)?;
    let runtime_exports = runtime_link
        .exports
        .iter()
        .flat_map(|export| {
            let name = export.name.trim_start_matches('_').to_owned();
            [export.name.clone(), name]
        })
        .collect::<HashSet<_>>();

    let mut failures = Vec::new();
    for module in outputs
        .modules
        .iter()
        .filter(|module| matches!(module.kind, "runtime-support" | "extension"))
    {
        let link = read_wasm_link_metadata(&module.path)?;
        for import in &link.imports {
            if !import_should_resolve_from_runtime(import) {
                continue;
            }
            let normalized = import.name.trim_start_matches('_');
            if !runtime_exports.contains(import.name.as_str())
                && !runtime_exports.contains(normalized)
            {
                failures.push(format!(
                    "{} imports {}.{}",
                    module.name, import.module, import.name
                ));
            }
        }
    }

    if !failures.is_empty() {
        bail!(
            "WASIX dynamic-link closure has unresolved side-module imports: {}",
            failures.join(", ")
        );
    }
    Ok(())
}

fn import_should_resolve_from_runtime(import: &WasmImportOut) -> bool {
    match import.module.as_str() {
        "env" | "GOT.func" | "GOT.mem" => !matches!(
            import.name.as_str(),
            "__indirect_function_table"
                | "__memory_base"
                | "__stack_pointer"
                | "__table_base"
                | "memory"
        ),
        _ => false,
    }
}

fn has_wasm_export(link: &WasmLinkMetadataOut, name: &str) -> bool {
    link.exports
        .iter()
        .any(|export| export.name == name || export.name == format!("_{name}"))
}

fn build_asset_spine(
    _manifest: &SourcesManifest,
    profile: &str,
    target: &str,
    args: &[String],
) -> Result<()> {
    let execute = args.iter().any(|arg| arg == "--execute")
        || env::var("PGLITE_OXIDE_EXECUTE_ASSET_BUILD").as_deref() == Ok("1");

    println!("asset build inputs validated");
    println!("profile={profile}");
    println!("target-triple={target}");

    let commands = [
        "assets/wasix-build/docker_pglite.sh",
        "assets/wasix-build/docker_runtime_support.sh",
        "assets/wasix-build/docker_pgvector.sh",
        "assets/wasix-build/docker_pgtrgm.sh",
        "assets/wasix-build/docker_pgdump.sh",
    ];

    if !execute {
        println!("source-spine build is ready but not executed by default");
        println!("run with --execute or PGLITE_OXIDE_EXECUTE_ASSET_BUILD=1 to invoke:");
        for command in commands {
            println!("  {command}");
        }
        println!("follow with `assets package` and `assets aot` to refresh publishable artifacts");
        return Ok(());
    }

    for command in commands {
        run("bash", &[command])?;
    }

    let outputs = BuildOutputs::discover()?;
    outputs.write_manifest()?;
    validate_build_output_link_closure(&outputs)?;
    println!("wrote WASIX build output manifest to {WASIX_BUILD_MANIFEST_PATH}");
    Ok(())
}

fn release_build_assets(
    manifest: &SourcesManifest,
    profile: &str,
    target: &str,
    args: &[String],
) -> Result<()> {
    if args.iter().any(|arg| arg == "--fetch") {
        fetch_pinned_sources(manifest)?;
    }

    let mut build_args = vec![
        "build".to_owned(),
        "--profile".to_owned(),
        profile.to_owned(),
        "--target-triple".to_owned(),
        target.to_owned(),
        "--execute".to_owned(),
    ];
    build_args.extend(
        args.iter()
            .filter(|arg| {
                matches!(
                    arg.as_str(),
                    "--skip-build" | "--skip-aot" | "--skip-package-size"
                )
            })
            .cloned(),
    );

    if !args.iter().any(|arg| arg == "--skip-build") {
        build_asset_spine(manifest, profile, target, &build_args)?;
    } else {
        eprintln!("warning: skipping WASIX rebuild by request");
    }

    let outputs = BuildOutputs::discover()?;
    outputs.write_manifest()?;
    validate_build_output_link_closure(&outputs)?;

    if !args.iter().any(|arg| arg == "--skip-aot") {
        generate_aot_artifacts(target)?;
    } else {
        eprintln!("warning: skipping AOT generation by request");
    }

    package_assets(manifest, target)?;
    check_canonical_asset_layout(true)?;
    check_generated_manifest(manifest, true)?;
    check_aot_package_manifest(target)?;

    if !args.iter().any(|arg| arg == "--skip-package-size") {
        package_size(vec!["--enforce".to_owned()])?;
    }

    Ok(())
}

fn generate_aot_artifacts(target: &str) -> Result<()> {
    let outputs = BuildOutputs::discover()?;
    let source_dir = Path::new("assets/wasix-build/build/aot").join(target);
    fs::create_dir_all(&source_dir).with_context(|| format!("create {}", source_dir.display()))?;

    for module in &outputs.modules {
        let output = source_dir.join(module.aot_file);
        generate_one_aot_artifact(&module.path, &output)?;
    }
    Ok(())
}

fn generate_one_aot_artifact(input: &Path, output: &Path) -> Result<()> {
    ensure_file(input)?;
    let input =
        fs::canonicalize(input).with_context(|| format!("canonicalize {}", input.display()))?;
    let output = if output.is_absolute() {
        output.to_path_buf()
    } else {
        env::current_dir()
            .context("read current directory")?
            .join(output)
    };
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }

    let mut command = Command::new("cargo");
    command
        .args([
            "run",
            "--features",
            "llvm-engine",
            "--bin",
            "serialize_aot",
            "--",
            "--input",
        ])
        .arg(&input)
        .arg("--output")
        .arg(output)
        .args(["--engine", "llvm"])
        .current_dir("spikes/wasmer-wasix-eval");
    if env::var_os("LLVM_SYS_221_PREFIX").is_none() && Path::new("/opt/homebrew/opt/llvm").exists()
    {
        command.env("LLVM_SYS_221_PREFIX", "/opt/homebrew/opt/llvm");
    }
    run_command(&mut command)
        .with_context(|| format!("generate AOT artifact for {}", input.display()))
}

fn package_assets(manifest: &SourcesManifest, target: &str) -> Result<()> {
    let outputs = BuildOutputs::discover()?;
    outputs.write_manifest()?;
    validate_build_output_link_closure(&outputs)?;
    let build = &outputs.build_dir;
    let source = &outputs.source_dir;
    let stage = &outputs.package_stage;

    if stage.exists() {
        fs::remove_dir_all(stage).with_context(|| format!("remove {}", stage.display()))?;
    }
    fs::create_dir_all(stage).with_context(|| format!("create {}", stage.display()))?;

    let runtime_stage = stage.join("runtime/pglite");
    stage_runtime_tree(build, source, &runtime_stage)?;
    let runtime_archive = Path::new("crates/assets/assets/pglite.wasix.tar.zst");
    deterministic_tar_zst(&runtime_stage, Path::new("pglite"), runtime_archive)?;

    let pg_dump = Path::new("crates/assets/assets/bin/pg_dump.wasix.wasm");
    copy_file(outputs.module_path("tool:pg_dump")?, pg_dump)?;

    let vector_stage = stage.join("extensions/vector");
    stage_vector_extension(&vector_stage)?;
    let vector_archive = Path::new("crates/assets/assets/extensions/vector.tar.zst");
    deterministic_tar_zst(&vector_stage, Path::new(""), vector_archive)?;

    let pg_trgm_stage = stage.join("extensions/pg_trgm");
    stage_pg_trgm_extension(source, build, &pg_trgm_stage)?;
    let pg_trgm_archive = Path::new("crates/assets/assets/extensions/pg_trgm.tar.zst");
    deterministic_tar_zst(&pg_trgm_stage, Path::new(""), pg_trgm_archive)?;

    package_aot_artifacts(target, &outputs, manifest)?;
    write_asset_manifest(
        manifest,
        outputs.module_path("runtime:pglite")?,
        runtime_archive,
        pg_dump,
        &[
            BinaryPackage {
                name: "plpgsql",
                path: outputs.module_path("runtime-support:plpgsql")?,
                runtime_path: "lib/postgresql/plpgsql.so",
            },
            BinaryPackage {
                name: "dict_snowball",
                path: outputs.module_path("runtime-support:dict_snowball")?,
                runtime_path: "lib/postgresql/dict_snowball.so",
            },
        ],
        &[
            ExtensionPackage {
                name: "pgvector",
                sql_name: "vector",
                archive: "extensions/vector.tar.zst",
                path: vector_archive,
                module_path: outputs.module_path("extension:vector")?,
                stable: true,
            },
            ExtensionPackage {
                name: "pg_trgm",
                sql_name: "pg_trgm",
                archive: "extensions/pg_trgm.tar.zst",
                path: pg_trgm_archive,
                module_path: outputs.module_path("extension:pg_trgm")?,
                stable: true,
            },
        ],
    )?;
    update_pgdata_template_manifest(outputs.module_path("runtime:pglite")?)?;

    println!("packaged runtime assets into crates/assets/assets");
    println!("packaged {target} AOT artifacts when present");
    Ok(())
}

fn stage_runtime_tree(build: &Path, source: &Path, runtime: &Path) -> Result<()> {
    let bin = runtime.join("bin");
    let lib = runtime.join("lib/postgresql");
    let share = runtime.join("share/postgresql");
    fs::create_dir_all(&bin).with_context(|| format!("create {}", bin.display()))?;
    fs::create_dir_all(&lib).with_context(|| format!("create {}", lib.display()))?;
    fs::create_dir_all(&share).with_context(|| format!("create {}", share.display()))?;

    copy_file(&build.join("src/backend/pglite"), &bin.join("pglite"))?;
    copy_file(&build.join("src/bin/pg_dump/pg_dump"), &bin.join("pg_dump"))?;
    fs::write(bin.join("postgres"), [])
        .with_context(|| format!("write {}", bin.join("postgres").display()))?;
    fs::write(bin.join("initdb"), [])
        .with_context(|| format!("write {}", bin.join("initdb").display()))?;
    fs::write(runtime.join("password"), b"password\n")
        .with_context(|| format!("write {}", runtime.join("password").display()))?;

    copy_file(
        &build.join("src/include/catalog/postgres.bki"),
        &share.join("postgres.bki"),
    )?;
    copy_file(
        &build.join("src/include/catalog/system_constraints.sql"),
        &share.join("system_constraints.sql"),
    )?;
    for relative in [
        "src/backend/catalog/system_functions.sql",
        "src/backend/catalog/system_views.sql",
        "src/backend/catalog/information_schema.sql",
        "src/backend/catalog/sql_features.txt",
        "src/backend/libpq/pg_hba.conf.sample",
        "src/backend/libpq/pg_ident.conf.sample",
        "src/backend/utils/misc/postgresql.conf.sample",
    ] {
        let source_path = source.join(relative);
        let file_name = source_path
            .file_name()
            .ok_or_else(|| anyhow!("source file has no name: {}", source_path.display()))?;
        copy_file(&source_path, &share.join(file_name))?;
    }

    copy_file(
        &build.join("src/backend/snowball/snowball_create.sql"),
        &share.join("snowball_create.sql"),
    )?;
    copy_file(
        &build.join("src/backend/snowball/dict_snowball.so"),
        &lib.join("dict_snowball.so"),
    )?;
    copy_file(
        &build.join("src/pl/plpgsql/src/plpgsql.so"),
        &lib.join("plpgsql.so"),
    )?;

    let extension_dir = share.join("extension");
    fs::create_dir_all(&extension_dir)
        .with_context(|| format!("create {}", extension_dir.display()))?;
    for relative in [
        "src/pl/plpgsql/src/plpgsql.control",
        "src/pl/plpgsql/src/plpgsql--1.0.sql",
    ] {
        let source_path = source.join(relative);
        let file_name = source_path
            .file_name()
            .ok_or_else(|| anyhow!("source file has no name: {}", source_path.display()))?;
        copy_file(&source_path, &extension_dir.join(file_name))?;
    }

    copy_tree_filtered(
        &source.join("src/backend/tsearch/dicts"),
        &share.join("tsearch_data"),
        None,
    )?;
    copy_tree_filtered(
        &source.join("src/timezone/tznames"),
        &share.join("timezonesets"),
        Some(&["Makefile", "meson.build", "README"]),
    )?;
    stage_timezone_database(source, build, &share)?;
    Ok(())
}

fn stage_timezone_database(source: &Path, build: &Path, share: &Path) -> Result<()> {
    let tzdata = source.join("src/timezone/data/tzdata.zi");
    ensure_file(&tzdata)?;
    let compiled_timezone_dir = build.join("src/timezone/compiled");

    let timezone_dir = share.join("timezone");
    if timezone_dir.exists() {
        fs::remove_dir_all(&timezone_dir)
            .with_context(|| format!("remove {}", timezone_dir.display()))?;
    }
    fs::create_dir_all(&timezone_dir)
        .with_context(|| format!("create {}", timezone_dir.display()))?;
    copy_tree_filtered(&compiled_timezone_dir, &timezone_dir, None).with_context(|| {
        format!(
            "copy compiled PostgreSQL timezone database from {}",
            compiled_timezone_dir.display()
        )
    })?;

    for required in ["UTC", "GMT", "Etc/UTC", "America/New_York"] {
        let path = timezone_dir.join(required);
        if !path.is_file() {
            bail!(
                "compiled PostgreSQL timezone database is missing required zone {}",
                path.display()
            );
        }
    }
    Ok(())
}

fn stage_vector_extension(stage: &Path) -> Result<()> {
    let source = Path::new(PGVECTOR_BUILD_DIR);
    fs::create_dir_all(stage.join("lib/postgresql"))
        .with_context(|| format!("create {}", stage.join("lib/postgresql").display()))?;
    fs::create_dir_all(stage.join("share/postgresql/extension")).with_context(|| {
        format!(
            "create {}",
            stage.join("share/postgresql/extension").display()
        )
    })?;
    copy_file(
        &source.join("vector.so"),
        &stage.join("lib/postgresql/vector.so"),
    )?;
    copy_file(
        &source.join("vector.control"),
        &stage.join("share/postgresql/extension/vector.control"),
    )?;
    for entry in sorted_files(&source.join("sql"))? {
        let file_name = entry
            .file_name()
            .ok_or_else(|| anyhow!("SQL file has no name: {}", entry.display()))?;
        copy_file(
            &entry,
            &stage.join("share/postgresql/extension").join(file_name),
        )?;
    }
    Ok(())
}

fn stage_pg_trgm_extension(source: &Path, build: &Path, stage: &Path) -> Result<()> {
    let extension_source = source.join("contrib/pg_trgm");
    fs::create_dir_all(stage.join("lib/postgresql"))
        .with_context(|| format!("create {}", stage.join("lib/postgresql").display()))?;
    fs::create_dir_all(stage.join("share/postgresql/extension")).with_context(|| {
        format!(
            "create {}",
            stage.join("share/postgresql/extension").display()
        )
    })?;
    copy_file(
        &build.join("contrib/pg_trgm/pg_trgm.so"),
        &stage.join("lib/postgresql/pg_trgm.so"),
    )?;
    copy_file(
        &extension_source.join("pg_trgm.control"),
        &stage.join("share/postgresql/extension/pg_trgm.control"),
    )?;
    for entry in sorted_files(&extension_source)? {
        let Some(name) = entry.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name.starts_with("pg_trgm--") && name.ends_with(".sql") {
            copy_file(&entry, &stage.join("share/postgresql/extension").join(name))?;
        }
    }
    Ok(())
}

fn package_aot_artifacts(
    target: &str,
    outputs: &BuildOutputs,
    sources: &SourcesManifest,
) -> Result<()> {
    let source_dir = Path::new("assets/wasix-build/build/aot").join(target);
    if !source_dir.exists() {
        eprintln!(
            "warning: AOT source directory {} is missing; skipping AOT packaging",
            source_dir.display()
        );
        return Ok(());
    }

    let crate_dir = Path::new("crates/aot").join(target);
    let artifacts_dir = crate_dir.join("artifacts");
    fs::create_dir_all(&artifacts_dir)
        .with_context(|| format!("create {}", artifacts_dir.display()))?;

    let artifacts = [
        ("runtime:pglite", "pglite-llvm-opta.bin.zst"),
        ("runtime-support:plpgsql", "plpgsql-llvm-opta.bin.zst"),
        (
            "runtime-support:dict_snowball",
            "dict_snowball-llvm-opta.bin.zst",
        ),
        ("extension:vector", "vector-llvm-opta.bin.zst"),
        ("extension:pg_trgm", "pg_trgm-llvm-opta.bin.zst"),
        ("tool:pg_dump", "pg_dump-llvm-opta.bin.zst"),
    ];
    let mut manifest_artifacts = Vec::new();
    for (name, file) in artifacts {
        let source = source_dir.join(file);
        if !source.exists() {
            eprintln!("warning: missing AOT artifact {}", source.display());
            continue;
        }
        let destination = artifacts_dir.join(file);
        copy_file(&source, &destination)?;
        let module_sha256 = outputs
            .modules
            .iter()
            .find(|module| module.name == name)
            .map(|module| sha256_file(&module.path))
            .transpose()?
            .ok_or_else(|| anyhow!("missing build output module {name} for AOT manifest"))?;
        manifest_artifacts.push(AotManifestArtifact {
            name: name.to_owned(),
            path: format!("artifacts/{file}"),
            sha256: sha256_file(&destination)?,
            module_sha256,
            compressed: true,
        });
    }

    let manifest = AotManifest {
        format_version: 1,
        target_triple: target.to_owned(),
        engine: "llvm-opta".to_owned(),
        wasmer_version: sources.toolchain.wasmer.clone(),
        wasmer_wasix_version: sources.toolchain.wasmer_wasix.clone(),
        artifacts: manifest_artifacts,
    };
    let manifest_json =
        serde_json::to_string_pretty(&manifest).context("serialize AOT manifest")?;
    fs::write(
        artifacts_dir.join("manifest.json"),
        format!("{manifest_json}\n"),
    )
    .with_context(|| format!("write {}", artifacts_dir.join("manifest.json").display()))?;
    write_aot_lib(&crate_dir.join("src/lib.rs"), target, &manifest_json)?;
    Ok(())
}

fn write_aot_lib(path: &Path, target: &str, manifest_json: &str) -> Result<()> {
    let manifest: AotManifest =
        serde_json::from_str(manifest_json).context("parse generated AOT manifest")?;
    let mut cases = String::new();
    for artifact in &manifest.artifacts {
        let file = artifact
            .path
            .strip_prefix("artifacts/")
            .ok_or_else(|| anyhow!("AOT artifact path must start with artifacts/"))?;
        let one_line = format!(
            "        {:?} => Some(include_bytes!(\"../artifacts/{}\")),\n",
            artifact.name, file
        );
        if one_line.trim_end().len() <= 100 {
            cases.push_str(&one_line);
        } else {
            cases.push_str(&format!(
                "        {:?} => Some(include_bytes!(\n            \"../artifacts/{}\"\n        )),\n",
                artifact.name, file
            ));
        }
    }
    if cases.is_empty() {
        cases.push_str("        _ => None,\n");
    } else {
        cases.push_str("        _ => None,\n");
    }

    let text = format!(
        "#![deny(unsafe_code)]\n\npub const TARGET_TRIPLE: &str = {:?};\npub const ENGINE: &str = \"llvm-opta\";\npub const MANIFEST_JSON: &str = include_str!(\"../artifacts/manifest.json\");\n\npub fn artifact_bytes(name: &str) -> Option<&'static [u8]> {{\n    match name {{\n{}    }}\n}}\n",
        target, cases
    );
    fs::write(path, text).with_context(|| format!("write {}", path.display()))
}

fn check_aot_package_manifest(target: &str) -> Result<()> {
    let outputs = BuildOutputs::discover()?;
    let crate_dir = Path::new("crates/aot").join(target);
    let manifest_path = crate_dir.join("artifacts/manifest.json");
    ensure_file(&manifest_path)?;
    let text = fs::read_to_string(&manifest_path)
        .with_context(|| format!("read {}", manifest_path.display()))?;
    let manifest: AotManifest = serde_json::from_str(&text)
        .with_context(|| format!("parse {}", manifest_path.display()))?;
    ensure_eq(
        &manifest.target_triple,
        target,
        "AOT manifest target-triple",
    )?;
    ensure_eq(&manifest.engine, "llvm-opta", "AOT manifest engine")?;
    ensure_eq(
        &manifest.wasmer_version,
        "7.2.0-alpha.2",
        "AOT manifest wasmer-version",
    )?;
    ensure_eq(
        &manifest.wasmer_wasix_version,
        "0.702.0-alpha.2",
        "AOT manifest wasmer-wasix-version",
    )?;

    for artifact in &manifest.artifacts {
        let path = crate_dir.join(&artifact.path);
        ensure_file(&path)?;
        let actual_hash = sha256_file(&path)?;
        ensure_eq(
            &actual_hash,
            &artifact.sha256,
            &format!("AOT artifact {} sha256", artifact.name),
        )?;
        let module = outputs
            .modules
            .iter()
            .find(|module| module.name == artifact.name)
            .ok_or_else(|| anyhow!("AOT manifest references unknown module {}", artifact.name))?;
        let module_hash = sha256_file(&module.path)?;
        ensure_eq(
            &module_hash,
            &artifact.module_sha256,
            &format!("AOT artifact {} source module sha256", artifact.name),
        )?;
    }
    Ok(())
}

fn write_asset_manifest(
    sources: &SourcesManifest,
    runtime_module: &Path,
    runtime_archive: &Path,
    pg_dump: &Path,
    runtime_support: &[BinaryPackage<'_>],
    extensions: &[ExtensionPackage<'_>],
) -> Result<()> {
    let manifest = AssetManifestOut {
        format_version: 1,
        runtime: RuntimeAssetOut {
            archive: "pglite.wasix.tar.zst".to_owned(),
            sha256: sha256_file(runtime_archive)?,
            module_sha256: sha256_file(runtime_module)?,
            postgres_version: "17.5".to_owned(),
            runtime_kind: "wasix-dynamic-main".to_owned(),
            link: read_wasm_link_metadata(runtime_module)?,
        },
        runtime_support: runtime_support
            .iter()
            .map(|module| {
                Ok(BinaryAssetOut {
                    name: module.name.to_owned(),
                    path: module.runtime_path.to_owned(),
                    sha256: sha256_file(module.path)?,
                    module_sha256: sha256_file(module.path)?,
                    size: fs::metadata(module.path)
                        .with_context(|| format!("metadata {}", module.path.display()))?
                        .len(),
                    link: read_wasm_link_metadata(module.path)?,
                })
            })
            .collect::<Result<Vec<_>>>()?,
        pg_dump: Some(BinaryAssetOut {
            name: "pg_dump".to_owned(),
            path: "bin/pg_dump.wasix.wasm".to_owned(),
            sha256: sha256_file(pg_dump)?,
            module_sha256: sha256_file(pg_dump)?,
            size: fs::metadata(pg_dump)
                .with_context(|| format!("metadata {}", pg_dump.display()))?
                .len(),
            link: read_wasm_link_metadata(pg_dump)?,
        }),
        extensions: extensions
            .iter()
            .map(|extension| {
                Ok(ExtensionAssetOut {
                    name: extension.name.to_owned(),
                    sql_name: extension.sql_name.to_owned(),
                    archive: extension.archive.to_owned(),
                    sha256: sha256_file(extension.path)?,
                    module_sha256: sha256_file(extension.module_path)?,
                    size: fs::metadata(extension.path)
                        .with_context(|| format!("metadata {}", extension.path.display()))?
                        .len(),
                    stable: extension.stable,
                    link: read_wasm_link_metadata(extension.module_path)?,
                })
            })
            .collect::<Result<Vec<_>>>()?,
        sources: sources.sources.clone(),
    };

    let text = serde_json::to_string_pretty(&manifest).context("serialize asset manifest")?;
    fs::write("crates/assets/assets/manifest.json", format!("{text}\n"))
        .context("write crates/assets/assets/manifest.json")?;
    update_root_asset_metadata(&manifest, &sha256_file(runtime_module)?)
}

fn update_pgdata_template_manifest(runtime_module: &Path) -> Result<()> {
    let manifest_path = Path::new("crates/assets/assets/prepopulated/pgdata-template.json");
    if !manifest_path.exists() {
        eprintln!(
            "warning: PGDATA template manifest {} is missing",
            manifest_path.display()
        );
        return Ok(());
    }
    let text = fs::read_to_string(manifest_path)
        .with_context(|| format!("read {}", manifest_path.display()))?;
    let mut manifest: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("parse {}", manifest_path.display()))?;
    manifest["wasmSha256"] = serde_json::Value::String(sha256_file(runtime_module)?);
    let archive = fs::read("crates/assets/assets/prepopulated/pgdata-template.tar.zst")
        .context("read embedded PGDATA template archive")?;
    manifest["archiveSha256"] = serde_json::Value::String(sha256_bytes(&archive));
    let output =
        serde_json::to_string_pretty(&manifest).context("serialize PGDATA template manifest")?;
    fs::write(manifest_path, format!("{output}\n"))
        .with_context(|| format!("write {}", manifest_path.display()))
}

fn update_root_asset_metadata(
    manifest: &AssetManifestOut,
    runtime_module_sha256: &str,
) -> Result<()> {
    let path = Path::new("Cargo.toml");
    let mut text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    text = replace_metadata_value(text, "runtime-archive-sha256", &manifest.runtime.sha256);
    text = replace_metadata_value(text, "pglite-wasix-sha256", runtime_module_sha256);
    let pgdata_template = Path::new("crates/assets/assets/prepopulated/pgdata-template.tar.zst");
    if pgdata_template.exists() {
        text = replace_metadata_value(
            text,
            "pgdata-template-archive-sha256",
            &sha256_file(pgdata_template)?,
        );
    }
    if let Some(pg_dump) = &manifest.pg_dump {
        text = replace_metadata_value(text, "pg-dump-wasix-sha256", &pg_dump.sha256);
    }
    fs::write(path, text).with_context(|| format!("write {}", path.display()))
}

fn replace_metadata_value(mut text: String, key: &str, value: &str) -> String {
    let needle = format!("{key} = \"");
    let Some(start) = text.find(&needle) else {
        eprintln!("warning: Cargo.toml metadata key '{key}' is missing; not updating it");
        return text;
    };
    let value_start = start + needle.len();
    let Some(relative_end) = text[value_start..].find('"') else {
        return text;
    };
    text.replace_range(value_start..value_start + relative_end, value);
    text
}

fn deterministic_tar_zst(source_root: &Path, archive_root: &Path, output: &Path) -> Result<()> {
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let file = fs::File::create(output).with_context(|| format!("create {}", output.display()))?;
    let encoder =
        ZstdEncoder::new(file, 19).with_context(|| format!("create zstd {}", output.display()))?;
    let mut builder = tar::Builder::new(encoder);
    append_tree(&mut builder, source_root, source_root, archive_root)?;
    let encoder = builder.into_inner().context("finish tar stream")?;
    encoder
        .finish()
        .with_context(|| format!("finish {}", output.display()))?;
    Ok(())
}

fn append_tree<W: io::Write>(
    builder: &mut tar::Builder<W>,
    root: &Path,
    current: &Path,
    archive_root: &Path,
) -> Result<()> {
    let relative = current
        .strip_prefix(root)
        .with_context(|| format!("strip {} from {}", root.display(), current.display()))?;
    let archive_path = if relative.as_os_str().is_empty() {
        archive_root.to_path_buf()
    } else {
        archive_root.join(relative)
    };

    if !archive_path.as_os_str().is_empty() {
        let mut header = tar::Header::new_gnu();
        header.set_mtime(0);
        header.set_uid(0);
        header.set_gid(0);
        header.set_username("root").ok();
        header.set_groupname("root").ok();
        if current.is_dir() {
            header.set_entry_type(tar::EntryType::Directory);
            header.set_mode(0o755);
            header.set_size(0);
            header.set_cksum();
            builder
                .append_data(&mut header, &archive_path, io::empty())
                .with_context(|| format!("append directory {}", archive_path.display()))?;
        } else if current.is_file() {
            let bytes = fs::read(current).with_context(|| format!("read {}", current.display()))?;
            header.set_entry_type(tar::EntryType::Regular);
            header.set_mode(if is_executable(current) { 0o755 } else { 0o644 });
            header.set_size(bytes.len() as u64);
            header.set_cksum();
            builder
                .append_data(&mut header, &archive_path, bytes.as_slice())
                .with_context(|| format!("append file {}", archive_path.display()))?;
        }
    }

    if current.is_dir() {
        for child in sorted_children(current)? {
            append_tree(builder, root, &child, archive_root)?;
        }
    }
    Ok(())
}

fn copy_tree_filtered(
    source: &Path,
    destination: &Path,
    skip_names: Option<&[&str]>,
) -> Result<()> {
    fs::create_dir_all(destination).with_context(|| format!("create {}", destination.display()))?;
    for entry in sorted_files(source)? {
        let relative = entry
            .strip_prefix(source)
            .with_context(|| format!("strip {} from {}", source.display(), entry.display()))?;
        if let Some(file_name) = relative.file_name().and_then(|name| name.to_str()) {
            if skip_names
                .map(|names| names.iter().any(|skip| *skip == file_name))
                .unwrap_or(false)
            {
                continue;
            }
        }
        copy_file(&entry, &destination.join(relative))?;
    }
    Ok(())
}

fn sorted_children(path: &Path) -> Result<Vec<PathBuf>> {
    let mut children = fs::read_dir(path)
        .with_context(|| format!("read directory {}", path.display()))?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("read child in {}", path.display()))?;
    children.sort();
    Ok(children)
}

fn sorted_files(path: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in WalkDir::new(path) {
        let entry = entry.with_context(|| format!("walk {}", path.display()))?;
        if entry.file_type().is_file() {
            files.push(entry.path().to_path_buf());
        }
    }
    files.sort();
    Ok(files)
}

fn copy_file(source: &Path, destination: &Path) -> Result<()> {
    ensure_file(source)?;
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    fs::copy(source, destination)
        .with_context(|| format!("copy {} -> {}", source.display(), destination.display()))?;
    Ok(())
}

fn ensure_file(path: &Path) -> Result<()> {
    if !path.is_file() {
        bail!("expected file missing: {}", path.display());
    }
    Ok(())
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path)
        .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.eq_ignore_ascii_case("exe"))
        .unwrap_or(false)
}

fn sha256_file(path: &Path) -> Result<String> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    Ok(sha256_bytes(&bytes))
}

fn sha256_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn read_wasm_link_metadata(path: &Path) -> Result<WasmLinkMetadataOut> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let mut metadata = WasmLinkMetadataOut {
        has_dylink0: false,
        dylink_needed: Vec::new(),
        dylink_runtime_paths: Vec::new(),
        dylink_memory: None,
        dylink_imports: Vec::new(),
        dylink_exports: Vec::new(),
        imports: Vec::new(),
        exports: Vec::new(),
        memories: Vec::new(),
    };

    for payload in Parser::new(0).parse_all(&bytes) {
        match payload.with_context(|| format!("parse {}", path.display()))? {
            Payload::ImportSection(reader) => {
                for import in reader.into_imports() {
                    let import =
                        import.with_context(|| format!("read import from {}", path.display()))?;
                    metadata.imports.push(WasmImportOut {
                        module: import.module.to_owned(),
                        name: import.name.to_owned(),
                        kind: type_ref_kind(import.ty).to_owned(),
                    });
                }
            }
            Payload::ExportSection(reader) => {
                for export in reader {
                    let export =
                        export.with_context(|| format!("read export from {}", path.display()))?;
                    metadata.exports.push(WasmExportOut {
                        name: export.name.to_owned(),
                        kind: external_kind_name(export.kind).to_owned(),
                    });
                }
            }
            Payload::MemorySection(reader) => {
                for memory in reader {
                    let memory =
                        memory.with_context(|| format!("read memory from {}", path.display()))?;
                    metadata.memories.push(wasm_memory_out(memory));
                }
            }
            Payload::CustomSection(section) if section.name() == "dylink.0" => {
                metadata.has_dylink0 = true;
                let KnownCustom::Dylink0(reader) = section.as_known() else {
                    bail!("{} contains an unreadable dylink.0 section", path.display());
                };
                for subsection in reader {
                    match subsection
                        .with_context(|| format!("read dylink.0 from {}", path.display()))?
                    {
                        Dylink0Subsection::MemInfo(info) => {
                            metadata.dylink_memory = Some(WasmDylinkMemoryOut {
                                memory_size: info.memory_size,
                                memory_alignment: info.memory_alignment,
                                table_size: info.table_size,
                                table_alignment: info.table_alignment,
                            });
                        }
                        Dylink0Subsection::Needed(needed) => {
                            metadata
                                .dylink_needed
                                .extend(needed.into_iter().map(str::to_owned));
                        }
                        Dylink0Subsection::RuntimePath(paths) => {
                            metadata
                                .dylink_runtime_paths
                                .extend(paths.into_iter().map(str::to_owned));
                        }
                        Dylink0Subsection::ImportInfo(imports) => {
                            metadata
                                .dylink_imports
                                .extend(imports.into_iter().map(|import| WasmDylinkSymbolOut {
                                    module: Some(import.module.to_owned()),
                                    name: import.field.to_owned(),
                                    flags: import.flags.bits(),
                                }));
                        }
                        Dylink0Subsection::ExportInfo(exports) => {
                            metadata
                                .dylink_exports
                                .extend(exports.into_iter().map(|export| WasmDylinkSymbolOut {
                                    module: None,
                                    name: export.name.to_owned(),
                                    flags: export.flags.bits(),
                                }));
                        }
                        Dylink0Subsection::Unknown { .. } => {}
                    }
                }
            }
            _ => {}
        }
    }

    metadata.dylink_needed.sort();
    metadata.dylink_needed.dedup();
    metadata.dylink_runtime_paths.sort();
    metadata.dylink_runtime_paths.dedup();
    metadata.dylink_imports.sort_by(|left, right| {
        (left.module.as_deref(), left.name.as_str(), left.flags).cmp(&(
            right.module.as_deref(),
            right.name.as_str(),
            right.flags,
        ))
    });
    metadata.dylink_exports.sort_by(|left, right| {
        (left.module.as_deref(), left.name.as_str(), left.flags).cmp(&(
            right.module.as_deref(),
            right.name.as_str(),
            right.flags,
        ))
    });
    metadata.imports.sort_by(|left, right| {
        (left.module.as_str(), left.name.as_str(), left.kind.as_str()).cmp(&(
            right.module.as_str(),
            right.name.as_str(),
            right.kind.as_str(),
        ))
    });
    metadata.exports.sort_by(|left, right| {
        (left.name.as_str(), left.kind.as_str()).cmp(&(right.name.as_str(), right.kind.as_str()))
    });
    metadata.memories.sort_by(|left, right| {
        (
            left.initial_pages,
            left.maximum_pages,
            left.memory64,
            left.shared,
            left.page_size_log2,
        )
            .cmp(&(
                right.initial_pages,
                right.maximum_pages,
                right.memory64,
                right.shared,
                right.page_size_log2,
            ))
    });

    Ok(metadata)
}

fn type_ref_kind(ty: TypeRef) -> &'static str {
    match ty {
        TypeRef::Func(_) | TypeRef::FuncExact(_) => "func",
        TypeRef::Table(_) => "table",
        TypeRef::Memory(_) => "memory",
        TypeRef::Global(_) => "global",
        TypeRef::Tag(_) => "tag",
    }
}

fn external_kind_name(kind: ExternalKind) -> &'static str {
    match kind {
        ExternalKind::Func | ExternalKind::FuncExact => "func",
        ExternalKind::Table => "table",
        ExternalKind::Memory => "memory",
        ExternalKind::Global => "global",
        ExternalKind::Tag => "tag",
    }
}

fn wasm_memory_out(memory: wasmparser::MemoryType) -> WasmMemoryOut {
    WasmMemoryOut {
        initial_pages: memory.initial,
        maximum_pages: memory.maximum,
        memory64: memory.memory64,
        shared: memory.shared,
        page_size_log2: memory.page_size_log2,
    }
}

fn host_target_triple() -> &'static str {
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

fn source_by_name<'a>(manifest: &'a SourcesManifest, name: &str) -> Result<&'a SourcePin> {
    manifest
        .sources
        .iter()
        .find(|source| source.name == name)
        .ok_or_else(|| anyhow!("assets/sources.toml is missing source '{name}'"))
}

fn ensure_eq(actual: &str, expected: &str, field: &str) -> Result<()> {
    if actual != expected {
        bail!("{field} must be '{expected}', got '{actual}'");
    }
    Ok(())
}

fn ensure_contains(values: &[String], expected: &str, field: &str) -> Result<()> {
    if !values.iter().any(|value| value == expected) {
        bail!("{field} must contain '{expected}'");
    }
    Ok(())
}

fn command_output(command: &str, args: &[&str], cwd: &Path) -> Result<String> {
    let output = Command::new(command)
        .args(args)
        .current_dir(cwd)
        .stderr(Stdio::inherit())
        .output()
        .map_err(|err| anyhow!("failed to spawn {command}: {err}"))?;
    if !output.status.success() {
        bail!("{command} {} failed with {}", args.join(" "), output.status);
    }
    String::from_utf8(output.stdout).context("command output was not valid UTF-8")
}

fn value_after<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.windows(2)
        .find(|window| window[0] == name)
        .map(|window| window[1].as_str())
}

fn run(command: &str, args: &[&str]) -> Result<()> {
    let mut command = Command::new(command);
    command.args(args);
    run_command(&mut command)
}

fn run_command(command: &mut Command) -> Result<()> {
    let status = command
        .status()
        .map_err(|err| anyhow!("failed to spawn command: {err}"))?;
    if !status.success() {
        bail!("command failed with {status}");
    }
    Ok(())
}

fn print_usage() {
    eprintln!("usage:");
    eprintln!("  cargo run -p xtask -- assets check [--strict-local] [--strict-generated]");
    eprintln!("  cargo run -p xtask -- assets audit-upstream [--strict]");
    eprintln!("  cargo run -p xtask -- assets source-spine [--check-patch-applies]");
    eprintln!("  cargo run -p xtask -- assets fetch");
    eprintln!(
        "  cargo run -p xtask -- assets build --profile release --target-triple <triple> [--execute]"
    );
    eprintln!(
        "  cargo run -p xtask -- assets release-build --profile release --target-triple <triple> [--fetch]"
    );
    eprintln!("  cargo run -p xtask -- assets aot --target-triple <triple>");
    eprintln!("  cargo run -p xtask -- assets package [--target-triple <triple>]");
    eprintln!("  cargo run -p xtask -- assets smoke");
    eprintln!("  cargo run -p xtask -- package-size --enforce");
    eprintln!("  cargo run -p xtask -- perf smoke");
}

#[derive(Debug, Deserialize)]
struct SourcesManifest {
    toolchain: Toolchain,
    build: BuildConfig,
    sources: Vec<SourcePin>,
}

#[derive(Debug, Deserialize)]
struct GeneratedAssetManifest {
    #[serde(default)]
    sources: Vec<SourcePin>,
}

#[derive(Debug, Deserialize)]
struct Toolchain {
    wasmer: String,
    #[serde(rename = "wasmer-wasix")]
    wasmer_wasix: String,
    #[allow(dead_code)]
    wasixcc: String,
    #[allow(dead_code)]
    llvm: String,
    #[allow(dead_code)]
    docker_image: String,
    #[allow(dead_code)]
    docker_image_digest: String,
}

#[derive(Debug, Deserialize)]
struct BuildConfig {
    postgres_prefix: String,
    postgres_pkglibdir: String,
    postgres_sharedir: String,
    main_flags: Vec<String>,
    extension_flags: Vec<String>,
    archive_format: String,
    deterministic_archives: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct SourcePin {
    name: String,
    url: String,
    branch: String,
    commit: String,
}

struct ExtensionPackage<'a> {
    name: &'a str,
    sql_name: &'a str,
    archive: &'a str,
    path: &'a Path,
    module_path: &'a Path,
    stable: bool,
}

struct BinaryPackage<'a> {
    name: &'a str,
    path: &'a Path,
    runtime_path: &'a str,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "kebab-case")]
struct BuildOutputManifestOut {
    format_version: u32,
    modules: Vec<BuildModuleManifestOut>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "kebab-case")]
struct BuildModuleManifestOut {
    name: String,
    kind: String,
    path: String,
    sha256: String,
    link: WasmLinkMetadataOut,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "kebab-case")]
struct AssetManifestOut {
    format_version: u32,
    runtime: RuntimeAssetOut,
    runtime_support: Vec<BinaryAssetOut>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pg_dump: Option<BinaryAssetOut>,
    extensions: Vec<ExtensionAssetOut>,
    sources: Vec<SourcePin>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "kebab-case")]
struct RuntimeAssetOut {
    archive: String,
    sha256: String,
    module_sha256: String,
    postgres_version: String,
    runtime_kind: String,
    link: WasmLinkMetadataOut,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "kebab-case")]
struct BinaryAssetOut {
    name: String,
    path: String,
    sha256: String,
    module_sha256: String,
    size: u64,
    link: WasmLinkMetadataOut,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "kebab-case")]
struct ExtensionAssetOut {
    name: String,
    sql_name: String,
    archive: String,
    sha256: String,
    module_sha256: String,
    size: u64,
    stable: bool,
    link: WasmLinkMetadataOut,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
struct WasmLinkMetadataOut {
    has_dylink0: bool,
    dylink_needed: Vec<String>,
    dylink_runtime_paths: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dylink_memory: Option<WasmDylinkMemoryOut>,
    dylink_imports: Vec<WasmDylinkSymbolOut>,
    dylink_exports: Vec<WasmDylinkSymbolOut>,
    imports: Vec<WasmImportOut>,
    exports: Vec<WasmExportOut>,
    memories: Vec<WasmMemoryOut>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
struct WasmDylinkMemoryOut {
    memory_size: u32,
    memory_alignment: u32,
    table_size: u32,
    table_alignment: u32,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
struct WasmDylinkSymbolOut {
    module: Option<String>,
    name: String,
    flags: u32,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
struct WasmImportOut {
    module: String,
    name: String,
    kind: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
struct WasmExportOut {
    name: String,
    kind: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
struct WasmMemoryOut {
    initial_pages: u64,
    maximum_pages: Option<u64>,
    memory64: bool,
    shared: bool,
    page_size_log2: Option<u32>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
struct AotManifest {
    format_version: u32,
    target_triple: String,
    engine: String,
    wasmer_version: String,
    wasmer_wasix_version: String,
    artifacts: Vec<AotManifestArtifact>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
struct AotManifestArtifact {
    name: String,
    path: String,
    sha256: String,
    module_sha256: String,
    compressed: bool,
}

struct UpstreamAuditItem {
    id: &'static str,
    commit: &'static str,
    description: &'static str,
    required: bool,
}

const UPSTREAM_AUDIT: &[UpstreamAuditItem] = &[
    UpstreamAuditItem {
        id: "builder-foundation",
        commit: "51e222cc5f799675b8dd098f5cb7bf46cbad75a2",
        description: "REL_17_5_WASM-pglite-builder head with AGE _invoke exports",
        required: true,
    },
    UpstreamAuditItem {
        id: "builder-age",
        commit: "c7c530a",
        description: "builder branch AGE extension source and packaging",
        required: true,
    },
    UpstreamAuditItem {
        id: "builder-pgdump",
        commit: "f5f1005",
        description: "builder branch backend pg_dump work",
        required: true,
    },
    UpstreamAuditItem {
        id: "builder-pgcrypto",
        commit: "bee4a36",
        description: "builder branch pgcrypto backend work",
        required: true,
    },
    UpstreamAuditItem {
        id: "stable-protocol-exports",
        commit: "a58ae720b72b0a350babe4e22652467253217e11",
        description: "stable branch PGlite protocol exports and startup HBA load",
        required: true,
    },
    UpstreamAuditItem {
        id: "stable-checkpointer-disable",
        commit: "01792c31a62b7045eb22e93d7dad022bb64b1184",
        description: "stable branch checkpointer disable",
        required: true,
    },
    UpstreamAuditItem {
        id: "stable-imported-memory",
        commit: "0c98d7c",
        description: "stable branch imported memory build fix",
        required: true,
    },
    UpstreamAuditItem {
        id: "stable-postgres-user",
        commit: "ac31093",
        description: "stable branch default postgres user and home",
        required: true,
    },
    UpstreamAuditItem {
        id: "stable-is-transaction-block",
        commit: "6c76f5e",
        description: "stable branch IsTransactionBlock export",
        required: false,
    },
    UpstreamAuditItem {
        id: "stable-postgis",
        commit: "d0f2748",
        description: "stable branch PostGIS backend proof",
        required: false,
    },
];
