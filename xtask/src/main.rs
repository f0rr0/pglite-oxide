use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail, ensure};
use directories::ProjectDirs;
use futures_util::future::try_join_all;
use pglite_oxide::{
    Pglite, PgliteServer, PhaseTiming, ProtocolStatsSnapshot, capture_phase_timings,
    disable_protocol_stats, extensions, fs_trace_snapshot, measure_phase, protocol_stats_snapshot,
    record_phase_timing, reset_fs_trace, reset_protocol_stats,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::postgres::{PgConnectOptions, PgSslMode};
use sqlx::{Connection, Executor, Row};
use walkdir::WalkDir;
use wasmparser::{Dylink0Subsection, ExternalKind, KnownCustom, Parser, Payload, TypeRef};
use zstd::stream::write::Encoder as ZstdEncoder;

mod extension_catalog;

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
const DEFAULT_ASSET_BUILD_PROFILE: &str = "release-o3";
const VALIDATE_XTASK_ENV: &str = "PGLITE_OXIDE_XTASK";
const PGVECTOR_BUILD_DIR: &str = "assets/checkouts/pgvector";
const POSTGRES_OTHER_EXTENSIONS: &str = "assets/checkouts/postgres-pglite/pglite/other_extensions";
const PGLITE_BENCHMARK_SQL_DIR: &str = "assets/checkouts/pglite/packages/benchmark/src";
const EXPECTED_POSTGRES_PGLITE_BRANCH: &str = "REL_17_5-pglite";
const EXPECTED_PGLITE_BUILD_BRANCH: &str = "portable";
const ASSET_INPUT_FINGERPRINT_PATH: &str = "assets/generated/asset-inputs.sha256";
const GENERATED_ASSETS_DIR: &str = "target/pglite-oxide/assets";
const ASSET_CRATE_PAYLOAD_DIR: &str = "crates/assets/payload";
const RELEASE_STAGE_DIR: &str = "target/pglite-oxide/release";
const LEGACY_STATIC_WASI_ARCHIVE: &str = concat!("assets/", "pglite-", "wasi.tar.zst");

#[cfg(feature = "template-runner")]
#[derive(Debug, Default)]
struct LocalOnlyPackageLoader;

#[cfg(feature = "template-runner")]
#[derive(Debug, Clone)]
struct TailCaptureFile {
    inner: std::sync::Arc<std::sync::Mutex<TailCaptureState>>,
    limit: usize,
}

#[cfg(feature = "template-runner")]
#[derive(Debug, Default)]
struct TailCaptureState {
    bytes: std::collections::VecDeque<u8>,
}

#[cfg(feature = "template-runner")]
#[derive(Debug, Clone)]
struct TailCaptureHandle {
    inner: std::sync::Arc<std::sync::Mutex<TailCaptureState>>,
}

#[cfg(feature = "template-runner")]
impl TailCaptureFile {
    fn new(limit: usize) -> (Self, TailCaptureHandle) {
        let inner = std::sync::Arc::new(std::sync::Mutex::new(TailCaptureState::default()));
        (
            Self {
                inner: inner.clone(),
                limit,
            },
            TailCaptureHandle { inner },
        )
    }

    fn push_tail(&self, bytes: &[u8]) {
        let Ok(mut state) = self.inner.lock() else {
            return;
        };
        for byte in bytes {
            state.bytes.push_back(*byte);
            while state.bytes.len() > self.limit {
                state.bytes.pop_front();
            }
        }
    }
}

#[cfg(feature = "template-runner")]
impl TailCaptureHandle {
    fn text(&self) -> String {
        let Ok(state) = self.inner.lock() else {
            return "<template output capture lock poisoned>".to_owned();
        };
        let bytes = state.bytes.iter().copied().collect::<Vec<_>>();
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

#[cfg(feature = "template-runner")]
impl wasmer_wasix::virtual_fs::AsyncSeek for TailCaptureFile {
    fn start_seek(
        self: std::pin::Pin<&mut Self>,
        _position: std::io::SeekFrom,
    ) -> std::io::Result<()> {
        Ok(())
    }

    fn poll_complete(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<u64>> {
        std::task::Poll::Ready(Ok(0))
    }
}

#[cfg(feature = "template-runner")]
impl wasmer_wasix::virtual_fs::AsyncRead for TailCaptureFile {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        _buf: &mut wasmer_wasix::virtual_fs::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

#[cfg(feature = "template-runner")]
impl wasmer_wasix::virtual_fs::AsyncWrite for TailCaptureFile {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        self.push_tail(buf);
        std::task::Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_write_vectored(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let mut total = 0;
        for buf in bufs {
            self.push_tail(buf);
            total += buf.len();
        }
        std::task::Poll::Ready(Ok(total))
    }

    fn is_write_vectored(&self) -> bool {
        true
    }
}

#[cfg(feature = "template-runner")]
impl wasmer_wasix::virtual_fs::VirtualFile for TailCaptureFile {
    fn last_accessed(&self) -> u64 {
        0
    }

    fn last_modified(&self) -> u64 {
        0
    }

    fn created_time(&self) -> u64 {
        0
    }

    fn size(&self) -> u64 {
        self.inner
            .lock()
            .map(|state| state.bytes.len() as u64)
            .unwrap_or(0)
    }

    fn set_len(&mut self, _new_size: u64) -> wasmer_wasix::virtual_fs::Result<()> {
        Ok(())
    }

    fn unlink(&mut self) -> wasmer_wasix::virtual_fs::Result<()> {
        Ok(())
    }

    fn poll_read_ready(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::task::Poll::Ready(Ok(0))
    }

    fn poll_write_ready(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::task::Poll::Ready(Ok(self.limit))
    }
}

#[cfg(feature = "template-runner")]
#[async_trait::async_trait]
impl wasmer_wasix::runtime::package_loader::PackageLoader for LocalOnlyPackageLoader {
    async fn load(
        &self,
        summary: &wasmer_wasix::runtime::resolver::PackageSummary,
    ) -> Result<webc::Container> {
        bail!(
            "WASIX template generation only supports local packages; unexpected dependency {}",
            summary.pkg.id
        )
    }

    async fn load_package_tree(
        &self,
        root: &webc::Container,
        resolution: &wasmer_wasix::runtime::resolver::Resolution,
        root_is_local_dir: bool,
    ) -> Result<wasmer_wasix::bin_factory::BinaryPackage> {
        wasmer_wasix::runtime::package_loader::load_package_tree(
            root,
            self,
            resolution,
            root_is_local_dir,
        )
        .await
    }
}

fn main() -> Result<()> {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("assets") => assets(args.collect()),
        Some("extensions") => extension_catalog::extensions(args.collect()),
        Some("release") => release(args.collect()),
        Some("package-size") => package_size(args.collect()),
        Some("perf") => perf(args.collect()),
        Some("aot-serializer") => aot_serializer(args.collect()),
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
            let release_staged = is_release_staged_workspace();
            let manifest = check_sources_manifest(strict_local)?;
            check_source_free_repo()?;
            check_no_legacy_runtime_shims()?;
            check_production_wasix_build_inputs()?;
            check_rust_startup_abi_boundary()?;
            check_canonical_asset_layout(strict_generated)?;
            check_generated_manifest(&manifest, strict_generated)?;
            if strict_generated {
                verify_asset_manifest_hashes()?;
                verify_generated_extension_surface()?;
            }
            if !release_staged {
                extension_catalog::check_catalog_file(strict_generated)?;
                extension_catalog::check_build_plan_file(strict_generated)?;
            }
            check_generated_wasix_export_list(strict_generated)
        }
        Some("verify-committed") => verify_committed_assets(),
        Some("audit-upstream") => {
            let strict = args.iter().any(|arg| arg == "--strict");
            let manifest = check_sources_manifest(false)?;
            audit_upstream_fixes(&manifest, strict)
        }
        Some("build") => {
            let manifest = check_sources_manifest(false)?;
            let profile = value_after(&args, "--profile").unwrap_or(DEFAULT_ASSET_BUILD_PROFILE);
            let target = value_after(&args, "--target-triple").unwrap_or(env::consts::ARCH);
            build_asset_spine(&manifest, profile, target, &args)
        }
        Some("template") => {
            let manifest = check_sources_manifest(false)?;
            generate_pgdata_template_asset(&manifest)
        }
        Some("fetch") => {
            let manifest = load_sources_manifest()?;
            validate_sources_manifest(&manifest)?;
            fetch_pinned_sources(&manifest)
        }
        Some("release-build") => {
            let manifest = check_sources_manifest_for_asset_build(&args)?;
            let profile = value_after(&args, "--profile").unwrap_or(DEFAULT_ASSET_BUILD_PROFILE);
            let target = value_after(&args, "--target-triple").unwrap_or(host_target_triple());
            release_build_assets(&manifest, profile, target, &args)
        }
        Some("build-host") => {
            let manifest = check_sources_manifest_for_asset_build(&args)?;
            release_build_assets(
                &manifest,
                DEFAULT_ASSET_BUILD_PROFILE,
                host_target_triple(),
                &args,
            )
        }
        Some("download") => download_assets(&args),
        Some("install-local") => install_local_assets(&args),
        Some("ci-matrix") => print_aot_ci_matrix(&args),
        Some("ci-artifacts") => print_ci_artifact_names(),
        Some("aot-targets") => print_supported_aot_targets(),
        Some("internal-packages") => print_internal_asset_packages(),
        Some("package") => {
            let manifest = check_sources_manifest(false)?;
            let target = value_after(&args, "--target-triple").unwrap_or(host_target_triple());
            package_assets(&manifest, target)
        }
        Some("package-aot") => {
            let manifest = check_sources_manifest(false)?;
            let target = value_after(&args, "--target-triple").unwrap_or(host_target_triple());
            package_aot_only(&manifest, target)
        }
        Some("check-aot") => {
            let target = value_after(&args, "--target-triple").unwrap_or(host_target_triple());
            check_aot_package_manifest(target)
        }
        Some("export-list") => {
            let write = args.iter().any(|arg| arg == "--write");
            generate_wasix_export_list(write)
        }
        Some("input-fingerprint") => {
            let write = args.iter().any(|arg| arg == "--write");
            check_or_write_asset_input_fingerprint(write)
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
        Some("smoke") => run_asset_smoke_tests(&args[1..]),
        Some(other) => bail!("unknown assets subcommand: {other}"),
        None => {
            bail!(
                "usage: cargo run -p xtask -- assets <check|verify-committed|audit-upstream|source-spine|fetch|build|template|build-host|release-build|download|install-local|ci-matrix|ci-artifacts|aot-targets|internal-packages|package|package-aot|check-aot|smoke>"
            )
        }
    }
}

fn release(args: Vec<String>) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("stage") => stage_release_workspace(),
        Some("dry-run") => {
            stage_release_workspace()?;
            run_in_release_workspace("scripts/validate.sh", &["release", "--allow-dirty"])
        }
        Some("publish") => {
            stage_release_workspace()?;
            run_in_release_workspace("scripts/validate.sh", &["release", "--allow-dirty"])?;
            bail!(
                "xtask release publish staged and validated the release workspace, but publishing still belongs to the Release workflow/release-plz until Trusted Publishing is configured"
            )
        }
        Some(other) => bail!("unknown release subcommand: {other}"),
        None => bail!("usage: cargo run -p xtask -- release <stage|dry-run|publish>"),
    }
}

fn aot_serializer(args: Vec<String>) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("serialize") => serialize_aot_cli(&args[1..]),
        Some("probe") => probe_aot_serializer_in_process(),
        Some(other) => bail!("unknown aot-serializer subcommand: {other}"),
        None => bail!(
            "usage: cargo run -p xtask --features aot-serializer -- aot-serializer <serialize|probe>"
        ),
    }
}

#[cfg(not(feature = "aot-serializer"))]
fn serialize_aot_cli(_args: &[String]) -> Result<()> {
    bail!("xtask aot-serializer requires `cargo run -p xtask --features aot-serializer -- ...`")
}

#[cfg(feature = "aot-serializer")]
fn serialize_aot_cli(args: &[String]) -> Result<()> {
    let input = value_after(args, "--input")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("--input is required"))?;
    let output = value_after(args, "--output")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("--output is required"))?;
    serialize_aot_module(&input, &output)
}

#[cfg(not(feature = "aot-serializer"))]
fn probe_aot_serializer_in_process() -> Result<()> {
    bail!(
        "xtask aot-serializer probe requires `cargo run -p xtask --features aot-serializer -- ...`"
    )
}

#[cfg(feature = "aot-serializer")]
fn probe_aot_serializer_in_process() -> Result<()> {
    let engine = llvm_aot_engine();
    let store = wasmer::Store::new(engine.clone());
    const EMPTY_WASM: &[u8] = b"\0asm\x01\0\0\0";
    let module =
        wasmer::Module::new(&store, EMPTY_WASM).context("compile LLVM AOT probe module")?;
    let serialized = module
        .serialize()
        .context("serialize LLVM AOT probe module")?;
    print_aot_engine_config(&engine);
    println!("serialized-probe-bytes: {}", serialized.len());
    Ok(())
}

#[cfg(feature = "aot-serializer")]
fn serialize_aot_module(input: &Path, output: &Path) -> Result<()> {
    let engine = llvm_aot_engine();
    print_aot_engine_config(&engine);
    println!("host-target: {}-{}", env::consts::OS, env::consts::ARCH);

    let store = wasmer::Store::new(engine);
    let bytes = fs::read(input).with_context(|| format!("read {}", input.display()))?;
    let module = wasmer::Module::new(&store, &bytes)
        .with_context(|| format!("compile {}", input.display()))?;
    let serialized = module.serialize().context("serialize module")?;

    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let file = fs::File::create(output).with_context(|| format!("create {}", output.display()))?;
    let mut encoder = ZstdEncoder::new(file, 19)
        .with_context(|| format!("create zstd encoder for {}", output.display()))?;
    let mut serialized_slice = serialized.as_ref();
    io::copy(&mut serialized_slice, &mut encoder)
        .with_context(|| format!("write {}", output.display()))?;
    encoder
        .finish()
        .with_context(|| format!("finish {}", output.display()))?;
    println!(
        "serialized {} bytes to {}",
        serialized.len(),
        output.display()
    );
    Ok(())
}

#[cfg(feature = "aot-serializer")]
fn llvm_aot_engine() -> wasmer::Engine {
    use wasmer::sys::{CompilerConfig, EngineBuilder, Features, LLVM};

    let mut features = Features::new();
    features.exceptions(true);
    let mut llvm = LLVM::default();
    if env_flag("PGLITE_OXIDE_WASMER_PERFMAP") {
        llvm.enable_perfmap();
    }
    llvm.enable_non_volatile_memops();
    llvm.enable_readonly_funcref_table();
    EngineBuilder::new(llvm)
        .set_target(Some(portable_aot_target()))
        .set_features(Some(features))
        .engine()
        .into()
}

#[cfg(feature = "aot-serializer")]
fn portable_aot_target() -> wasmer_types::target::Target {
    use wasmer_types::target::{Architecture, CpuFeature, Target, Triple};

    let triple = Triple::host();
    let mut cpu_features = CpuFeature::set();
    match triple.architecture {
        Architecture::X86_64 => {
            cpu_features.insert(CpuFeature::SSE2);
        }
        Architecture::Aarch64(_) => {
            cpu_features.insert(CpuFeature::NEON);
        }
        _ => {}
    }

    Target::new(triple, cpu_features)
}

#[cfg(feature = "aot-serializer")]
fn print_aot_engine_config(engine: &wasmer::Engine) {
    let target = portable_aot_target();
    println!("wasmer-engine: llvm");
    println!("wasmer-engine-id: {}", engine.deterministic_id());
    println!("wasmer-target-triple: {}", target.triple());
    println!(
        "wasmer-target-cpu-features: {}",
        format_aot_cpu_features(&target)
    );
    println!("wasmer-feature-exceptions: enabled");
    println!("wasmer-llvm-target-cpu: generic");
    println!("wasmer-llvm-non-volatile-memops: enabled");
    println!("wasmer-llvm-readonly-funcref-table: enabled");
}

#[cfg(feature = "aot-serializer")]
fn format_aot_cpu_features(target: &wasmer_types::target::Target) -> String {
    let mut features = target
        .cpu_features()
        .iter()
        .map(|feature| feature.to_string())
        .collect::<Vec<_>>();
    features.sort();
    if features.is_empty() {
        "none".to_owned()
    } else {
        features.join(",")
    }
}

#[cfg(feature = "aot-serializer")]
fn env_flag(name: &str) -> bool {
    env::var(name)
        .map(|value| {
            let value = value.trim();
            !value.is_empty()
                && !matches!(
                    value.to_ascii_lowercase().as_str(),
                    "0" | "false" | "no" | "off"
                )
        })
        .unwrap_or(false)
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

fn download_assets(args: &[String]) -> Result<()> {
    let targets = asset_download_targets(args)?;
    let candidates = asset_download_run_candidates(args)?;
    let mut last_error = None;

    for run_id in candidates {
        match download_assets_from_run(&run_id, &targets) {
            Ok(()) => {
                let target_list = targets.join(", ");
                println!(
                    "downloaded and installed Assets workflow artifacts from run {run_id} / {target_list}"
                );
                return Ok(());
            }
            Err(error) => {
                if args.iter().any(|arg| arg == "--latest-compatible") {
                    eprintln!(
                        "Assets workflow run {run_id} is not compatible with this checkout: {error:#}"
                    );
                    last_error = Some(error);
                    continue;
                }
                return Err(error);
            }
        }
    }

    if let Some(error) = last_error {
        Err(error).context("no compatible successful Assets workflow artifact found")
    } else {
        bail!("no successful Assets workflow artifact found")
    }
}

fn asset_download_targets(args: &[String]) -> Result<Vec<String>> {
    let all_targets = args.iter().any(|arg| arg == "--all-targets");
    let explicit_target = value_after(args, "--target-triple");
    if all_targets && explicit_target.is_some() {
        bail!("assets download accepts either --all-targets or --target-triple, not both");
    }
    if all_targets {
        Ok(supported_aot_targets()
            .iter()
            .map(|target| (*target).to_owned())
            .collect())
    } else {
        let target = explicit_target.unwrap_or(host_target_triple());
        ensure_supported_aot_target(target)?;
        Ok(vec![target.to_owned()])
    }
}

fn asset_download_run_candidates(args: &[String]) -> Result<Vec<String>> {
    let run_id = value_after(args, "--run-id");
    let sha = value_after(args, "--sha");
    let latest_compatible = args.iter().any(|arg| arg == "--latest-compatible");
    let selected_modes =
        usize::from(run_id.is_some()) + usize::from(sha.is_some()) + usize::from(latest_compatible);
    if selected_modes != 1 {
        bail!(
            "assets download requires exactly one of --run-id <id>, --sha <sha>, or --latest-compatible"
        );
    }

    if let Some(run_id) = run_id {
        return Ok(vec![run_id.to_owned()]);
    }

    if let Some(sha) = sha {
        let output = command_output(
            "gh",
            &[
                "run",
                "list",
                "--workflow",
                "Assets",
                "--commit",
                sha,
                "--status",
                "success",
                "--limit",
                "1",
                "--json",
                "databaseId",
                "--jq",
                ".[].databaseId",
            ],
            Path::new("."),
        )
        .with_context(|| format!("find successful Assets workflow run for SHA {sha}"))?;
        return parse_gh_run_ids(&output);
    }

    let branch = value_after(args, "--branch").unwrap_or("main");
    let output = command_output(
        "gh",
        &[
            "run",
            "list",
            "--workflow",
            "Assets",
            "--branch",
            branch,
            "--status",
            "success",
            "--limit",
            "20",
            "--json",
            "databaseId",
            "--jq",
            ".[].databaseId",
        ],
        Path::new("."),
    )
    .with_context(|| format!("find latest successful Assets workflow runs on {branch}"))?;
    parse_gh_run_ids(&output)
}

fn parse_gh_run_ids(output: &str) -> Result<Vec<String>> {
    let runs = output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && *line != "null")
        .map(str::to_owned)
        .collect::<Vec<_>>();
    ensure!(
        !runs.is_empty(),
        "no successful Assets workflow artifact found"
    );
    Ok(runs)
}

fn download_assets_from_run(run_id: &str, targets: &[String]) -> Result<()> {
    let download_dir = Path::new("target/pglite-oxide/downloads").join(run_id);
    if download_dir.exists() {
        fs::remove_dir_all(&download_dir)
            .with_context(|| format!("remove {}", download_dir.display()))?;
    }
    fs::create_dir_all(&download_dir)
        .with_context(|| format!("create {}", download_dir.display()))?;
    run(
        "gh",
        &[
            "run",
            "download",
            run_id,
            "--name",
            "pglite-oxide-portable-wasix",
            "--dir",
            download_dir.to_str().expect("download dir is utf-8"),
        ],
    )?;
    for target in targets {
        run(
            "gh",
            &[
                "run",
                "download",
                run_id,
                "--name",
                &format!("pglite-oxide-aot-{target}"),
                "--dir",
                download_dir.to_str().expect("download dir is utf-8"),
            ],
        )?;
    }
    verify_downloaded_asset_fingerprint(&download_dir)?;
    install_downloaded_artifacts(&download_dir, targets)?;
    for target in targets {
        install_local_assets_for_target(target)?;
    }
    Ok(())
}

fn verify_downloaded_asset_fingerprint(download_dir: &Path) -> Result<()> {
    let expected = fs::read_to_string(ASSET_INPUT_FINGERPRINT_PATH)
        .with_context(|| format!("read {}", ASSET_INPUT_FINGERPRINT_PATH))?;
    let downloaded_path = download_dir.join(ASSET_INPUT_FINGERPRINT_PATH);
    let downloaded = fs::read_to_string(&downloaded_path)
        .with_context(|| format!("read {}", downloaded_path.display()))?;
    ensure_eq(
        downloaded.trim(),
        expected.trim(),
        "downloaded asset-input fingerprint",
    )
}

fn install_downloaded_artifacts(download_dir: &Path, targets: &[String]) -> Result<()> {
    let downloaded_assets = download_dir.join(GENERATED_ASSETS_DIR);
    ensure_file(&downloaded_assets.join("manifest.json"))?;
    copy_dir_all(&downloaded_assets, Path::new(GENERATED_ASSETS_DIR))?;

    for target in targets {
        let downloaded_aot = download_dir.join("target/pglite-oxide/aot").join(target);
        ensure_file(&downloaded_aot.join("manifest.json"))?;
        copy_dir_all(&downloaded_aot, &generated_aot_dir(target))?;
    }
    Ok(())
}

fn install_local_assets(args: &[String]) -> Result<()> {
    let target = value_after(args, "--target-triple").unwrap_or(host_target_triple());
    install_local_assets_for_target(target)
}

fn install_local_assets_for_target(target: &str) -> Result<()> {
    ensure_supported_aot_target(target)?;
    let generated_assets = Path::new(GENERATED_ASSETS_DIR);
    ensure_file(&generated_assets.join("manifest.json"))?;
    check_canonical_asset_layout(true)?;
    check_generated_manifest(&load_sources_manifest()?, true)?;
    verify_asset_manifest_hashes()?;
    verify_generated_extension_surface()?;

    find_aot_artifact_dir(target)?;
    check_aot_package_manifest(target)?;
    println!("local generated assets are installed for {target}");
    Ok(())
}

fn run_asset_smoke_tests(args: &[String]) -> Result<()> {
    if let Some(arg) = args.first() {
        bail!("unknown assets smoke flag: {arg}");
    }
    run_validate_script("runtime")
}

fn stage_release_workspace() -> Result<()> {
    let stage_root = Path::new(RELEASE_STAGE_DIR);
    let workspace = stage_root.join("workspace");
    if stage_root.exists() {
        fs::remove_dir_all(stage_root)
            .with_context(|| format!("remove {}", stage_root.display()))?;
    }
    fs::create_dir_all(&workspace).with_context(|| format!("create {}", workspace.display()))?;

    let tracked = command_output(
        "git",
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ],
        Path::new("."),
    )?;
    for path in tracked.split('\0').filter(|path| !path.is_empty()) {
        let source = Path::new(path);
        let destination = workspace.join(path);
        copy_file(source, &destination)?;
    }

    let generated_assets = Path::new(GENERATED_ASSETS_DIR);
    ensure_file(&generated_assets.join("manifest.json"))?;
    copy_dir_all(generated_assets, &workspace.join(ASSET_CRATE_PAYLOAD_DIR))?;
    copy_dir_all(generated_assets, &workspace.join(GENERATED_ASSETS_DIR))?;
    update_staged_root_asset_metadata(&workspace)?;

    for target in supported_aot_targets() {
        let generated_aot = generated_aot_dir(target);
        if generated_aot.join("manifest.json").is_file() {
            copy_dir_all(
                &generated_aot,
                &workspace.join("crates/aot").join(target).join("artifacts"),
            )?;
            copy_dir_all(
                &generated_aot,
                &workspace.join("target/pglite-oxide/aot").join(target),
            )?;
        }
    }

    fs::write(
        stage_root.join("README.txt"),
        "Generated pglite-oxide release workspace.\n",
    )
    .with_context(|| format!("write {}", stage_root.join("README.txt").display()))?;
    println!("staged release workspace at {}", workspace.display());
    Ok(())
}

fn run_in_release_workspace(command: &str, args: &[&str]) -> Result<()> {
    let workspace = Path::new(RELEASE_STAGE_DIR).join("workspace");
    let mut command = command_for_host(command);
    command
        .args(args)
        .current_dir(&workspace)
        .env("PGLITE_OXIDE_RELEASE_STAGED", "1");
    run_command(&mut command)
}

fn perf(args: Vec<String>) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("cold") => perf_cold(&args[1..]),
        Some("warm") => perf_warm(&args[1..]),
        Some("bench") => perf_bench(&args[1..]),
        Some("prepared-updates") => perf_prepared_updates(&args[1..]),
        Some("diagnose-indexed-update") => perf_diagnose_indexed_update(),
        Some("diagnose-speed-hotspots") => perf_diagnose_speed_hotspots(),
        Some("diagnose-speed-cases") => perf_diagnose_speed_cases(&args[1..]),
        Some("diagnose-buffer-cache") => perf_diagnose_buffer_cache(),
        Some("native-postgres") => perf_native_postgres(&args[1..]),
        Some("pglite-nodefs-sqlx") => perf_pglite_nodefs_sqlx(&args[1..]),
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
        None => bail!(
            "usage: cargo run -p xtask -- perf <cold|warm|bench|prepared-updates|native-postgres|pglite-nodefs-sqlx|diagnose-indexed-update|diagnose-speed-hotspots|diagnose-speed-cases|diagnose-buffer-cache|smoke> [--reset-cache]"
        ),
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ColdPerfReport {
    wasmer_version: &'static str,
    wasmer_wasix_version: &'static str,
    cache_reset_requested: bool,
    cache_dir: String,
    cache_state_at_start: &'static str,
    measurement_model: &'static str,
    operations: Vec<PerfOperation>,
    experiments: Vec<ColdPerfExperiment>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PerfOperation {
    name: &'static str,
    description: &'static str,
    cache_state_before: String,
    process_state_before: &'static str,
    root_state: &'static str,
    query_state: &'static str,
    workload: &'static str,
    primary_latency_phase: &'static str,
    primary_latency_micros: u128,
    elapsed_micros: u128,
    correct: bool,
    phases: Vec<PhaseTiming>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WarmPerfReport {
    wasmer_version: &'static str,
    wasmer_wasix_version: &'static str,
    query_iterations: usize,
    connection_iterations: usize,
    measurement_model: &'static str,
    operations: Vec<PerfOperation>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BenchmarkReport {
    wasmer_version: &'static str,
    wasmer_wasix_version: &'static str,
    source_model: &'static str,
    measurement_model: &'static str,
    rtt_iterations: usize,
    speed_scale: f64,
    preload_micros: u128,
    runs: Vec<BenchmarkRun>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BenchmarkRun {
    suite: &'static str,
    mode: &'static str,
    description: &'static str,
    open_micros: u128,
    connect_micros: Option<u128>,
    setup_micros: u128,
    tests: Vec<BenchmarkTestResult>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BenchmarkTestResult {
    id: &'static str,
    label: String,
    unit: &'static str,
    operation_count: usize,
    sample_count: usize,
    trimmed_sample_count: usize,
    elapsed_micros: u128,
    average_micros: Option<f64>,
    min_micros: Option<u128>,
    p50_micros: Option<u128>,
    p95_micros: Option<u128>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PreparedUpdateReport {
    source_model: &'static str,
    measurement_model: &'static str,
    gate_model: Option<&'static str>,
    rows: usize,
    runs: Vec<PreparedUpdateRun>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PreparedUpdateRun {
    mode: &'static str,
    description: &'static str,
    protocol_stats: Option<ProtocolStatsSnapshot>,
    tests: Vec<PreparedUpdateTest>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PreparedUpdateTest {
    id: &'static str,
    label: &'static str,
    open_micros: u128,
    connect_micros: u128,
    setup_micros: u128,
    prepare_micros: Option<u128>,
    elapsed_micros: u128,
    operation_count: usize,
    average_micros: f64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct IndexedUpdateDiagnosticReport {
    source_model: &'static str,
    measurement_model: &'static str,
    cases: Vec<IndexedUpdateDiagnosticCase>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct IndexedUpdateDiagnosticCase {
    name: &'static str,
    description: &'static str,
    setup_micros: u128,
    elapsed_micros: u128,
    operation_count: usize,
    stats_before: serde_json::Value,
    stats_after: serde_json::Value,
    fs_trace: serde_json::Value,
    phases: Vec<PhaseTiming>,
}

#[derive(Debug, Serialize)]
struct SpeedHotspotDiagnosticReport {
    source_model: &'static str,
    measurement_model: &'static str,
    cases: Vec<SpeedHotspotDiagnosticCase>,
}

#[derive(Debug, Serialize)]
struct SpeedHotspotDiagnosticCase {
    id: String,
    label: String,
    setup_micros: u128,
    elapsed_micros: u128,
    operation_count: usize,
    fs_trace: serde_json::Value,
    phases: Vec<PhaseTiming>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BufferCacheDiagnosticReport {
    source_model: &'static str,
    measurement_model: &'static str,
    cases: Vec<BufferCacheDiagnosticCase>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BufferCacheDiagnosticCase {
    id: String,
    label: String,
    setup_micros: u128,
    settings: serde_json::Value,
    relation_sizes: serde_json::Value,
    statements: Vec<BufferCacheDiagnosticStatement>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BufferCacheDiagnosticStatement {
    sql: String,
    elapsed_micros: u128,
    explain_rows: serde_json::Value,
    fs_trace: serde_json::Value,
    phases: Vec<PhaseTiming>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ColdPerfExperiment {
    name: &'static str,
    status: &'static str,
    implementation_risk: &'static str,
    artifact_size_impact: &'static str,
    notes: &'static str,
}

fn perf_cold(args: &[String]) -> Result<()> {
    let reset_cache = args.iter().any(|arg| arg == "--reset-cache");
    for arg in args {
        if arg != "--reset-cache" {
            bail!("unknown perf cold flag: {arg}");
        }
    }

    let cache_dir = pglite_oxide_cache_dir()?;
    let cache_state_at_start = if reset_cache {
        if cache_dir.exists() {
            fs::remove_dir_all(&cache_dir)
                .with_context(|| format!("reset pglite-oxide cache {}", cache_dir.display()))?;
        }
        "cold_absent_after_reset"
    } else if cache_dir.exists() {
        "existing"
    } else {
        "cold_absent"
    };

    let mut operations = Vec::new();

    operations.push(capture_operation(
        "process_cold_runtime_preload",
        "First explicit runtime preload in this xtask process. With --reset-cache, this includes first-install cache bootstrap.",
        cache_state_at_start,
        "cold",
        "internal_preload_temp_root",
        "not_a_query",
        "runtime_preload",
        "operation.total",
        Pglite::preload,
    )?);
    operations.push(capture_operation(
        "process_warm_new_temp_direct_first_query",
        "First direct query for a newly opened temporary database after runtime preload in the same process.",
        "warm_after_runtime_preload",
        "warm",
        "new_temporary_root",
        "first_query_after_open",
        "direct_select_with_bind",
        "visible.direct_open_to_first_query",
        run_direct_select_one,
    )?);
    operations.push(capture_operation(
        "process_warm_second_new_temp_direct_first_query",
        "Repeat first direct query for a second newly opened temporary database in the same warm process.",
        "warm_after_runtime_preload",
        "warm",
        "second_new_temporary_root",
        "first_query_after_open",
        "direct_select_with_bind",
        "visible.direct_open_to_first_query",
        run_direct_select_one,
    )?);
    operations.push(capture_operation(
        "process_warm_vector_preload",
        "Explicit preload of the representative extension artifact after runtime preload.",
        "warm_after_runtime_preload",
        "warm",
        "internal_preload_temp_root",
        "not_a_query",
        "vector_extension_preload",
        "operation.total",
        || Pglite::preload_extensions([extensions::VECTOR]),
    )?);
    operations.push(capture_operation(
        "process_warm_new_temp_direct_vector_first_query",
        "First vector-backed direct query for a newly opened temporary database after vector preload.",
        "warm_after_vector_preload",
        "warm",
        "new_temporary_root_with_requested_vector",
        "first_extension_backed_query_after_open",
        "direct_vector_distance",
        "visible.direct_open_to_first_query",
        run_direct_vector_query,
    )?);
    operations.push(capture_operation(
        "process_warm_new_temp_server_tokio_postgres_first_query",
        "First tokio-postgres query against a new temporary PgliteServer in the warm process.",
        "warm_after_runtime_preload",
        "warm",
        "new_temporary_server_root",
        "first_client_query_after_server_start",
        "tokio_postgres_select_with_bind",
        "visible.server_start_to_first_tokio_postgres_query",
        || {
            let visible_started = Instant::now();
            let server = measure_phase("server.start", PgliteServer::temporary_tcp)?;
            let uri = server.database_url();
            let runtime = measure_phase("client.tokio_runtime_create", || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .context("create perf tokio runtime")
            })?;
            runtime.block_on(async move {
                let started = Instant::now();
                let (client, connection) = tokio_postgres::connect(&uri, tokio_postgres::NoTls)
                    .await
                    .context("connect tokio-postgres to PGliteServer")?;
                record_phase_timing("client.tokio_postgres_connect", started.elapsed());
                let connection_handle = tokio::spawn(connection);
                let started = Instant::now();
                let row = client
                    .query_one("SELECT $1::int4 + 1 AS answer", &[&41_i32])
                    .await
                    .context("run first tokio-postgres query")?;
                record_phase_timing("client.tokio_postgres_first_query", started.elapsed());
                let answer: i32 = row.get("answer");
                if answer != 42 {
                    bail!("server query returned {answer}, expected 42");
                }
                drop(client);
                connection_handle
                    .await
                    .context("join tokio-postgres connection task")?
                    .context("tokio-postgres connection task")?;
                Ok::<_, anyhow::Error>(())
            })?;
            record_phase_timing(
                "visible.server_start_to_first_tokio_postgres_query",
                visible_started.elapsed(),
            );
            measure_phase("operation.shutdown", || server.shutdown())
        },
    )?);
    operations.push(capture_operation(
        "process_warm_new_temp_server_sqlx_first_query",
        "First SQLx query against a new temporary PgliteServer in the warm process.",
        "warm_after_runtime_preload",
        "warm",
        "new_temporary_server_root",
        "first_client_query_after_server_start",
        "sqlx_select_with_bind",
        "visible.server_start_to_first_sqlx_query",
        run_server_sqlx_select_one,
    )?);
    operations.push(capture_operation(
        "process_warm_new_temp_server_sqlx_vector_first_query",
        "First vector-backed SQLx query against a new extension-enabled temporary PgliteServer.",
        "warm_after_vector_preload",
        "warm",
        "new_temporary_server_root_with_requested_vector",
        "first_extension_backed_client_query_after_server_start",
        "sqlx_vector_distance",
        "visible.server_start_to_first_sqlx_query",
        || {
            let visible_started = Instant::now();
            let server = measure_phase("server.start", || {
                PgliteServer::builder()
                    .temporary()
                    .extension(extensions::VECTOR)
                    .start()
            })?;
            let uri = server.database_url();
            let runtime = measure_phase("client.tokio_runtime_create", || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .context("create perf tokio runtime")
            })?;
            runtime.block_on(async move {
                let started = Instant::now();
                let mut conn = sqlx::PgConnection::connect(&uri)
                    .await
                    .context("connect SQLx to extension-enabled PGliteServer")?;
                record_phase_timing("client.sqlx_extension_connect", started.elapsed());
                let started = Instant::now();
                let row = sqlx::query("SELECT '[1,2,3]'::vector <-> '[1,2,4]'::vector AS distance")
                    .fetch_one(&mut conn)
                    .await
                    .context("run first SQLx extension-backed query")?;
                record_phase_timing("client.sqlx_extension_first_query", started.elapsed());
                let distance: f64 = row.try_get("distance").context("read vector distance")?;
                if distance != 1.0 {
                    bail!("SQLx vector query returned {distance}, expected 1.0");
                }
                conn.close().await.context("close SQLx connection")?;
                Ok::<_, anyhow::Error>(())
            })?;
            record_phase_timing(
                "visible.server_start_to_first_sqlx_query",
                visible_started.elapsed(),
            );
            measure_phase("operation.shutdown", || server.shutdown())
        },
    )?);
    let preinstalled_extension_root = unique_perf_root("server-sqlx-preinstalled-extension")?;
    {
        let mut db = Pglite::builder()
            .path(&preinstalled_extension_root)
            .extension(extensions::VECTOR)
            .open()
            .context("prepare preinstalled extension perf root")?;
        db.close()
            .context("close preinstalled extension perf root")?;
    }
    operations.push(capture_operation(
        "process_warm_existing_persistent_server_sqlx_vector_first_query",
        "Diagnostic first vector-backed SQLx query against an existing persistent root where vector was already installed.",
        "warm_after_vector_preload",
        "warm",
        "existing_persistent_root_with_preinstalled_vector",
        "first_client_query_after_server_start",
        "sqlx_vector_distance",
        "visible.server_start_to_first_sqlx_query",
        || {
            let visible_started = Instant::now();
            let server = measure_phase("server.start", || {
                PgliteServer::builder()
                    .path(&preinstalled_extension_root)
                    .extension(extensions::VECTOR)
                    .start()
            })?;
            let uri = server.database_url();
            let runtime = measure_phase("client.tokio_runtime_create", || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .context("create perf tokio runtime")
            })?;
            runtime.block_on(async move {
                let started = Instant::now();
                let mut conn = sqlx::PgConnection::connect(&uri)
                    .await
                    .context("connect SQLx to preinstalled-extension PGliteServer")?;
                record_phase_timing("client.sqlx_extension_connect", started.elapsed());
                let started = Instant::now();
                let row = sqlx::query("SELECT '[1,2,3]'::vector <-> '[1,2,4]'::vector AS distance")
                    .fetch_one(&mut conn)
                    .await
                    .context("run first SQLx preinstalled-extension query")?;
                record_phase_timing("client.sqlx_extension_first_query", started.elapsed());
                let distance: f64 = row.try_get("distance").context("read vector distance")?;
                if distance != 1.0 {
                    bail!("SQLx vector query returned {distance}, expected 1.0");
                }
                conn.close().await.context("close SQLx connection")?;
                Ok::<_, anyhow::Error>(())
            })?;
            record_phase_timing(
                "visible.server_start_to_first_sqlx_query",
                visible_started.elapsed(),
            );
            measure_phase("operation.shutdown", || server.shutdown())
        },
    )?);
    let _ = fs::remove_dir_all(&preinstalled_extension_root);

    let report = ColdPerfReport {
        wasmer_version: "7.2.0-alpha.2",
        wasmer_wasix_version: "0.702.0-alpha.2",
        cache_reset_requested: reset_cache,
        cache_dir: cache_dir.display().to_string(),
        cache_state_at_start,
        measurement_model: "Operations run sequentially in one xtask process. 'Warm' means process/runtime/module caches have been warmed by earlier operations; 'first query' means first query after opening that operation's new database root or server.",
        operations,
        experiments: vec![
            ColdPerfExperiment {
                name: "wasmer_webassembly_exceptions",
                status: "production_invariant",
                implementation_risk: "medium",
                artifact_size_impact: "required",
                notes: "the runtime and WASIX build require WebAssembly exception handling; no non-EH fallback or opt-out is supported",
            },
            ColdPerfExperiment {
                name: "wasix_dynamic_linking_flags",
                status: "production_invariant",
                implementation_risk: "medium",
                artifact_size_impact: "required",
                notes: "main modules use dynamic-main flags and extension/tool side modules use PIC shared-module flags from the same configured tree",
            },
            ColdPerfExperiment {
                name: "process_wide_headless_engine_and_module_cache",
                status: "implemented",
                implementation_risk: "low",
                artifact_size_impact: "none",
                notes: "main and side modules are cached by artifact hash inside the process",
            },
            ColdPerfExperiment {
                name: "persistent_raw_aot_cache",
                status: "implemented",
                implementation_risk: "low",
                artifact_size_impact: "none",
                notes: "compressed AOT artifacts expand once to a manifest raw-SHA-keyed cache path; subsequent processes use fast receipt verification before mmap/native deserialization; full content hashing is only enabled with PGLITE_OXIDE_AOT_VERIFY=full",
            },
            ColdPerfExperiment {
                name: "mmap_native_deserialization",
                status: "mainline_measured_in_this_run",
                implementation_risk: "medium",
                artifact_size_impact: "none",
                notes: "runtime uses Wasmer native mmapped deserialization as the only production AOT loading path",
            },
            ColdPerfExperiment {
                name: "shared_wasix_runtime_and_module_cache",
                status: "implemented",
                implementation_risk: "medium",
                artifact_size_impact: "none",
                notes: "runtime infrastructure is shared while Store, Instance, WASI env, mounts, and protocol state remain per database",
            },
            ColdPerfExperiment {
                name: "template_clone_hardlink_reflink_copy",
                status: "implemented",
                implementation_risk: "medium",
                artifact_size_impact: "none",
                notes: "immutable runtime files hardlink first; mutable PGDATA uses archive install by default, with per-file reflink available through PGLITE_OXIDE_TEMPLATE_REFLINK",
            },
            ColdPerfExperiment {
                name: "eager_pgdata_template_overlay",
                status: "mainline_measured_in_this_run",
                implementation_risk: "medium",
                artifact_size_impact: "none",
                notes: "mounts the cached initialized PGDATA template as lower /base and copies individual files into the per-instance upper only before mutating opens",
            },
            ColdPerfExperiment {
                name: "mountfs_overlay_runtime_root",
                status: "mainline_measured_in_this_run",
                implementation_risk: "medium",
                artifact_size_impact: "none",
                notes: "serves immutable runtime files from the shared cached lower root and keeps only mutable state plus requested extension assets in the per-root upper root",
            },
            ColdPerfExperiment {
                name: "snapshot_journaling",
                status: "scouted_not_promoted",
                implementation_risk: "high",
                artifact_size_impact: "unknown",
                notes: "Wasmer 7.2 exposes WASIX journal and process snapshot APIs, while StoreSnapshot captures store globals only; promotion requires an isolated restore correctness suite for direct protocol, server mode, extensions, PGDATA, fd state, and mount state",
            },
            ColdPerfExperiment {
                name: "asyncify",
                status: "production_excluded",
                implementation_risk: "high",
                artifact_size_impact: "unknown",
                notes: "not used in production artifacts; only an isolated snapshot/journaling experiment may enable it if Wasm EH plus WASIX journaling cannot support the required control-flow restore path",
            },
        ],
    };

    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

fn perf_warm(args: &[String]) -> Result<()> {
    let mut query_iterations = 100usize;
    let mut connection_iterations = 20usize;
    let mut cursor = 0usize;
    while cursor < args.len() {
        match args[cursor].as_str() {
            "--iterations" => {
                cursor += 1;
                let value = args
                    .get(cursor)
                    .ok_or_else(|| anyhow!("--iterations requires a value"))?;
                query_iterations = value
                    .parse()
                    .with_context(|| format!("parse --iterations value {value:?}"))?;
            }
            "--connections" => {
                cursor += 1;
                let value = args
                    .get(cursor)
                    .ok_or_else(|| anyhow!("--connections requires a value"))?;
                connection_iterations = value
                    .parse()
                    .with_context(|| format!("parse --connections value {value:?}"))?;
            }
            other => bail!("unknown perf warm flag: {other}"),
        }
        cursor += 1;
    }
    if query_iterations == 0 {
        bail!("--iterations must be greater than zero");
    }
    if connection_iterations == 0 {
        bail!("--connections must be greater than zero");
    }

    let mut operations = Vec::new();
    operations.push(capture_operation(
        "warm_process_preload",
        "Warm runtime and representative extension artifacts before steady-state workloads.",
        "existing",
        "warm",
        "process_cache",
        "not_a_query",
        "runtime_and_extension_preload",
        "operation.total",
        || {
            Pglite::preload()?;
            Pglite::preload_extensions([extensions::VECTOR])
        },
    )?);
    operations.push(capture_operation(
        "warm_direct_repeated_scalar_queries",
        "Repeated direct API scalar extended-protocol queries on one already-open temporary database.",
        "warm_after_preload",
        "warm",
        "long_lived_temporary_direct_root",
        "steady_state_queries",
        "direct_select_with_bind",
        "warm.direct_repeated_scalar_queries.total",
        || run_direct_repeated_selects(query_iterations),
    )?);
    operations.push(capture_operation(
        "warm_direct_transaction_batch",
        "Repeated direct API scalar queries inside one transaction on an already-open temporary database.",
        "warm_after_preload",
        "warm",
        "long_lived_temporary_direct_root",
        "steady_state_transaction_batch",
        "direct_transaction_select_with_bind",
        "warm.direct_transaction_batch.total",
        || run_direct_transaction_batch(query_iterations),
    )?);
    operations.push(capture_operation(
        "warm_direct_repeated_vector_queries",
        "Repeated direct API extension-backed queries on one already-open extension-enabled temporary database.",
        "warm_after_vector_preload",
        "warm",
        "long_lived_temporary_direct_root_with_vector",
        "steady_state_extension_queries",
        "direct_vector_distance",
        "warm.direct_repeated_vector_queries.total",
        || run_direct_repeated_vector_queries(query_iterations),
    )?);
    operations.push(capture_operation(
        "warm_server_sqlx_single_connection_repeated_queries",
        "Repeated SQLx queries over one connection to one long-lived temporary server.",
        "warm_after_preload",
        "warm",
        "long_lived_temporary_server_root",
        "steady_state_single_connection_queries",
        "sqlx_select_with_bind",
        "warm.server_sqlx_single_connection_repeated_queries.total",
        || run_server_sqlx_single_connection_repeated_queries(query_iterations),
    )?);
    operations.push(capture_operation(
        "warm_server_sqlx_repeated_connections",
        "Repeated SQLx connect-query-close cycles against one long-lived temporary server.",
        "warm_after_preload",
        "warm",
        "long_lived_temporary_server_root",
        "steady_state_repeated_connections",
        "sqlx_connect_query_close",
        "warm.server_sqlx_repeated_connections.total",
        || run_server_sqlx_repeated_connections(connection_iterations),
    )?);
    operations.push(capture_operation(
        "warm_server_sqlx_vector_single_connection_repeated_queries",
        "Repeated SQLx extension-backed queries over one connection to one long-lived extension-enabled temporary server.",
        "warm_after_vector_preload",
        "warm",
        "long_lived_temporary_server_root_with_vector",
        "steady_state_extension_queries",
        "sqlx_vector_distance",
        "warm.server_sqlx_vector_single_connection_repeated_queries.total",
        || run_server_sqlx_vector_single_connection_repeated_queries(query_iterations),
    )?);
    operations.push(capture_operation(
        "warm_server_tokio_postgres_single_connection_repeated_queries",
        "Repeated tokio-postgres queries over one connection to one long-lived temporary server.",
        "warm_after_preload",
        "warm",
        "long_lived_temporary_server_root",
        "steady_state_single_connection_queries",
        "tokio_postgres_select_with_bind",
        "warm.server_tokio_postgres_single_connection_repeated_queries.total",
        || run_server_tokio_postgres_single_connection_repeated_queries(query_iterations),
    )?);

    let report = WarmPerfReport {
        wasmer_version: "7.2.0-alpha.2",
        wasmer_wasix_version: "0.702.0-alpha.2",
        query_iterations,
        connection_iterations,
        measurement_model: "Operations run after explicit process preload. Each workload opens one database/server, performs one warmup query where relevant, then records only the repeated steady-state section as the primary latency phase. Open and shutdown phases remain in the phase list for context.",
        operations,
    };

    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BenchmarkSuiteFilter {
    All,
    Rtt,
    Speed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BenchmarkModeFilter {
    All,
    Direct,
    ServerSqlx,
    ServerTokioPostgresSimple,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NativePostgresClientMode {
    TokioPostgresSimple,
    Sqlx,
}

impl BenchmarkSuiteFilter {
    fn includes(self, suite: &'static str) -> bool {
        matches!(
            (self, suite),
            (Self::All, _) | (Self::Rtt, "rtt") | (Self::Speed, "speed")
        )
    }
}

impl BenchmarkModeFilter {
    fn includes(self, mode: &'static str) -> bool {
        matches!(
            (self, mode),
            (Self::All, _)
                | (Self::Direct, "direct")
                | (Self::ServerSqlx, "server_sqlx")
                | (
                    Self::ServerTokioPostgresSimple,
                    "server_tokio_postgres_simple"
                )
        )
    }
}

fn perf_bench(args: &[String]) -> Result<()> {
    let mut suite = BenchmarkSuiteFilter::All;
    let mut mode = BenchmarkModeFilter::All;
    let mut rtt_iterations = 100usize;
    let mut speed_scale = 1.0f64;
    let mut speed_sql_source = SpeedSqlSource::Generated;
    let mut cursor = 0usize;
    while cursor < args.len() {
        match args[cursor].as_str() {
            "--suite" => {
                cursor += 1;
                let value = args
                    .get(cursor)
                    .ok_or_else(|| anyhow!("--suite requires a value"))?;
                suite = match value.as_str() {
                    "all" => BenchmarkSuiteFilter::All,
                    "rtt" | "roundtrip" | "round-trip" => BenchmarkSuiteFilter::Rtt,
                    "speed" | "sqlite" | "sqlite-suite" => BenchmarkSuiteFilter::Speed,
                    other => bail!("unknown --suite value {other:?}; use all, rtt, or speed"),
                };
            }
            "--mode" => {
                cursor += 1;
                let value = args
                    .get(cursor)
                    .ok_or_else(|| anyhow!("--mode requires a value"))?;
                mode = match value.as_str() {
                    "all" => BenchmarkModeFilter::All,
                    "direct" => BenchmarkModeFilter::Direct,
                    "server-sqlx" | "server_sqlx" | "sqlx" | "server" => {
                        BenchmarkModeFilter::ServerSqlx
                    }
                    "server-tokio-postgres-simple"
                    | "server_tokio_postgres_simple"
                    | "tokio-postgres-simple"
                    | "tokio_postgres_simple"
                    | "tokio-postgres"
                    | "tokio_postgres" => BenchmarkModeFilter::ServerTokioPostgresSimple,
                    other => {
                        bail!(
                            "unknown --mode value {other:?}; use all, direct, server-sqlx, or server-tokio-postgres-simple"
                        )
                    }
                };
            }
            "--iterations" => {
                cursor += 1;
                let value = args
                    .get(cursor)
                    .ok_or_else(|| anyhow!("--iterations requires a value"))?;
                rtt_iterations = value
                    .parse()
                    .with_context(|| format!("parse --iterations value {value:?}"))?;
            }
            "--scale" => {
                cursor += 1;
                let value = args
                    .get(cursor)
                    .ok_or_else(|| anyhow!("--scale requires a value"))?;
                speed_scale = value
                    .parse()
                    .with_context(|| format!("parse --scale value {value:?}"))?;
            }
            "--speed-source" => {
                cursor += 1;
                let value = args
                    .get(cursor)
                    .ok_or_else(|| anyhow!("--speed-source requires a value"))?;
                speed_sql_source = match value.as_str() {
                    "generated" | "local" => SpeedSqlSource::Generated,
                    "pglite" | "pglite-vendored" | "upstream" => SpeedSqlSource::PgliteVendored,
                    other => {
                        bail!("unknown --speed-source value {other:?}; use generated or pglite")
                    }
                };
            }
            other => bail!("unknown perf bench flag: {other}"),
        }
        cursor += 1;
    }
    if rtt_iterations == 0 {
        bail!("--iterations must be greater than zero");
    }
    if !speed_scale.is_finite() || speed_scale <= 0.0 {
        bail!("--scale must be a finite positive number");
    }
    if speed_sql_source == SpeedSqlSource::PgliteVendored
        && (speed_scale - 1.0).abs() > f64::EPSILON
    {
        bail!("--speed-source pglite uses fixed upstream SQL files and requires --scale 1");
    }

    let preload_started = Instant::now();
    Pglite::preload()?;
    let preload_micros = preload_started.elapsed().as_micros();

    let mut runs = Vec::new();
    if suite.includes("rtt") && mode.includes("direct") {
        runs.push(run_rtt_direct_benchmark(rtt_iterations)?);
    }
    if suite.includes("rtt") && mode.includes("server_sqlx") {
        runs.push(run_rtt_server_sqlx_benchmark(rtt_iterations)?);
    }
    if suite.includes("rtt") && mode.includes("server_tokio_postgres_simple") {
        runs.push(run_rtt_server_tokio_postgres_simple_benchmark(
            rtt_iterations,
        )?);
    }
    if suite.includes("speed") && mode.includes("direct") {
        runs.push(run_speed_direct_benchmark(speed_scale, speed_sql_source)?);
    }
    if suite.includes("speed") && mode.includes("server_sqlx") {
        runs.push(run_speed_server_sqlx_benchmark(
            speed_scale,
            speed_sql_source,
        )?);
    }
    ensure!(
        !runs.is_empty(),
        "selected benchmark filter produced no runs"
    );

    let report = BenchmarkReport {
        wasmer_version: "7.2.0-alpha.2",
        wasmer_wasix_version: "0.702.0-alpha.2",
        source_model: speed_sql_source.source_model(),
        measurement_model: "Database/server open and setup are measured separately. Test timings start immediately before each SQL execution call and end after that execution completes. RTT tests sort samples, discard the lowest and highest 10% when possible, and report trimmed averages in microseconds.",
        rtt_iterations,
        speed_scale,
        preload_micros,
        runs,
    };
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

fn perf_native_postgres(args: &[String]) -> Result<()> {
    let mut postgres_bin = env::var("PGLITE_OXIDE_NATIVE_POSTGRES")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("postgres"));
    let mut initdb_bin = env::var("PGLITE_OXIDE_NATIVE_INITDB")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("initdb"));
    let mut suite = BenchmarkSuiteFilter::Speed;
    let mut speed_sql_source = SpeedSqlSource::PgliteVendored;
    let mut rtt_iterations = 100usize;
    let mut client_mode = NativePostgresClientMode::TokioPostgresSimple;
    let mut cursor = 0usize;
    while cursor < args.len() {
        match args[cursor].as_str() {
            "--postgres-bin" => {
                cursor += 1;
                postgres_bin = PathBuf::from(
                    args.get(cursor)
                        .ok_or_else(|| anyhow!("--postgres-bin requires a value"))?,
                );
            }
            "--initdb-bin" => {
                cursor += 1;
                initdb_bin = PathBuf::from(
                    args.get(cursor)
                        .ok_or_else(|| anyhow!("--initdb-bin requires a value"))?,
                );
            }
            "--suite" => {
                cursor += 1;
                let value = args
                    .get(cursor)
                    .ok_or_else(|| anyhow!("--suite requires a value"))?;
                suite = match value.as_str() {
                    "all" => BenchmarkSuiteFilter::All,
                    "rtt" | "roundtrip" | "round-trip" => BenchmarkSuiteFilter::Rtt,
                    "speed" | "sqlite" | "sqlite-suite" => BenchmarkSuiteFilter::Speed,
                    other => bail!("unknown --suite value {other:?}; use all, rtt, or speed"),
                };
            }
            "--iterations" => {
                cursor += 1;
                let value = args
                    .get(cursor)
                    .ok_or_else(|| anyhow!("--iterations requires a value"))?;
                rtt_iterations = value
                    .parse()
                    .with_context(|| format!("parse --iterations value {value:?}"))?;
            }
            "--speed-source" => {
                cursor += 1;
                let value = args
                    .get(cursor)
                    .ok_or_else(|| anyhow!("--speed-source requires a value"))?;
                speed_sql_source = match value.as_str() {
                    "generated" | "local" => SpeedSqlSource::Generated,
                    "pglite" | "pglite-vendored" | "upstream" => SpeedSqlSource::PgliteVendored,
                    other => {
                        bail!("unknown --speed-source value {other:?}; use generated or pglite")
                    }
                };
            }
            "--client" => {
                cursor += 1;
                let value = args
                    .get(cursor)
                    .ok_or_else(|| anyhow!("--client requires a value"))?;
                client_mode = match value.as_str() {
                    "tokio-postgres-simple"
                    | "tokio_postgres_simple"
                    | "tokio-postgres"
                    | "tokio_postgres"
                    | "simple"
                    | "simple-query" => NativePostgresClientMode::TokioPostgresSimple,
                    "sqlx" => NativePostgresClientMode::Sqlx,
                    other => {
                        bail!("unknown --client value {other:?}; use tokio-postgres-simple or sqlx")
                    }
                };
            }
            other => bail!("unknown perf native-postgres flag: {other}"),
        }
        cursor += 1;
    }
    ensure!(rtt_iterations > 0, "--iterations must be greater than zero");

    let native_open_started = Instant::now();
    let native = NativePostgres::start(&postgres_bin, &initdb_bin)?;
    let native_open_micros = native_open_started.elapsed().as_micros();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("create native Postgres benchmark Tokio runtime")?;
    let runs = runtime.block_on(async {
        match client_mode {
            NativePostgresClientMode::TokioPostgresSimple => {
                let mut config = tokio_postgres::Config::new();
                configure_native_postgres_client(&mut config, &native);
                let connect_started = Instant::now();
                let (client, connection) = config
                    .connect(tokio_postgres::NoTls)
                    .await
                    .context("connect to native Postgres benchmark cluster")?;
                let connection_task = tokio::spawn(async move {
                    if let Err(err) = connection.await {
                        eprintln!("native Postgres benchmark connection error: {err}");
                    }
                });
                let connect_micros = connect_started.elapsed().as_micros();

                let mut runs = Vec::new();
                if suite.includes("rtt") {
                    runs.push(
                        run_native_postgres_rtt_benchmark(
                            &client,
                            rtt_iterations,
                            native_open_micros,
                            connect_micros,
                        )
                        .await?,
                    );
                }
                if suite.includes("speed") {
                    runs.push(
                        run_native_postgres_speed_benchmark(
                            &client,
                            speed_sql_source,
                            native_open_micros,
                            connect_micros,
                        )
                        .await?,
                    );
                }
                drop(client);
                connection_task.await.ok();
                Ok::<_, anyhow::Error>(runs)
            }
            NativePostgresClientMode::Sqlx => {
                let connect_started = Instant::now();
                let mut conn =
                    sqlx::PgConnection::connect_with(&native_postgres_sqlx_options(&native))
                        .await
                        .context("connect SQLx native Postgres benchmark client")?;
                let connect_micros = connect_started.elapsed().as_micros();

                let mut runs = Vec::new();
                if suite.includes("rtt") {
                    runs.push(
                        run_native_postgres_rtt_sqlx_benchmark(
                            &mut conn,
                            rtt_iterations,
                            native_open_micros,
                            connect_micros,
                        )
                        .await?,
                    );
                }
                if suite.includes("speed") {
                    runs.push(
                        run_native_postgres_speed_sqlx_benchmark(
                            &mut conn,
                            speed_sql_source,
                            native_open_micros,
                            connect_micros,
                        )
                        .await?,
                    );
                }
                conn.close()
                    .await
                    .context("close SQLx native Postgres benchmark client")?;
                Ok::<_, anyhow::Error>(runs)
            }
        }
    })?;

    let report = BenchmarkReport {
        wasmer_version: "native-postgres",
        wasmer_wasix_version: "native-postgres",
        source_model: speed_sql_source.source_model(),
        measurement_model: match client_mode {
            NativePostgresClientMode::TokioPostgresSimple => {
                "Native Postgres control. xtask starts a temporary local cluster with PGlite-parity startup GUCs and sends each benchmark SQL file as one simple-query buffer through tokio-postgres simple_query. This intentionally avoids psql -f because psql splits files client-side."
            }
            NativePostgresClientMode::Sqlx => {
                "Native Postgres control. xtask starts a temporary local cluster with PGlite-parity startup GUCs and runs the benchmark SQL through one long-lived SQLx connection."
            }
        },
        rtt_iterations,
        speed_scale: 1.0,
        preload_micros: 0,
        runs,
    };
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

async fn run_native_postgres_rtt_benchmark(
    client: &tokio_postgres::Client,
    iterations: usize,
    open_micros: u128,
    connect_micros: u128,
) -> Result<BenchmarkRun> {
    let setup_started = Instant::now();
    client
        .simple_query(rtt_setup_sql())
        .await
        .context("execute native Postgres RTT setup")?;
    let setup_micros = setup_started.elapsed().as_micros();

    let mut tests = Vec::new();
    for case in rtt_cases() {
        let mut samples = Vec::with_capacity(iterations);
        for _ in 0..iterations {
            let started = Instant::now();
            client
                .simple_query(&case.sql)
                .await
                .with_context(|| format!("execute native Postgres RTT benchmark {}", case.id))?;
            samples.push(started.elapsed().as_micros());
        }
        tests.push(samples_result(
            case.id,
            format!("Test {}: {}", case.id, case.label),
            "milliseconds",
            iterations,
            samples,
        ));
    }

    Ok(BenchmarkRun {
        suite: "rtt",
        mode: "native_postgres",
        description: "Native Postgres over Unix socket using tokio-postgres simple_query.",
        open_micros,
        connect_micros: Some(connect_micros),
        setup_micros,
        tests,
    })
}

async fn run_native_postgres_speed_benchmark(
    client: &tokio_postgres::Client,
    sql_source: SpeedSqlSource,
    open_micros: u128,
    connect_micros: u128,
) -> Result<BenchmarkRun> {
    client
        .simple_query(
            "DROP TABLE IF EXISTS t1 CASCADE;\
             DROP TABLE IF EXISTS t2 CASCADE;\
             DROP TABLE IF EXISTS t2_1 CASCADE;\
             DROP TABLE IF EXISTS t3 CASCADE;\
             DROP TABLE IF EXISTS t3_1 CASCADE;",
        )
        .await
        .context("clear native Postgres speed benchmark tables")?;

    let mut tests = Vec::new();
    for case in speed_cases(1.0, sql_source)? {
        let started = Instant::now();
        client
            .simple_query(&case.sql)
            .await
            .with_context(|| format!("execute native Postgres speed benchmark {}", case.id))?;
        tests.push(single_sample_result(
            case.id,
            case.label,
            "seconds",
            case.operation_count,
            started.elapsed(),
        ));
    }
    Ok(BenchmarkRun {
        suite: "speed",
        mode: "native_postgres",
        description: "Native Postgres speed suite over Unix socket using tokio-postgres simple_query.",
        open_micros,
        connect_micros: Some(connect_micros),
        setup_micros: 0,
        tests,
    })
}

fn native_postgres_sqlx_options(native: &NativePostgres) -> PgConnectOptions {
    PgConnectOptions::new_without_pgpass()
        .host("127.0.0.1")
        .port(native.port)
        .username("postgres")
        .database("postgres")
        .ssl_mode(PgSslMode::Disable)
}

fn perf_pglite_nodefs_sqlx(args: &[String]) -> Result<()> {
    let mut database_url: Option<String> = None;
    let mut suite = BenchmarkSuiteFilter::Speed;
    let mut speed_sql_source = SpeedSqlSource::PgliteVendored;
    let mut rtt_iterations = 100usize;
    let mut open_micros = 0u128;
    let mut cursor = 0usize;
    while cursor < args.len() {
        match args[cursor].as_str() {
            "--database-url" => {
                cursor += 1;
                database_url = Some(
                    args.get(cursor)
                        .ok_or_else(|| anyhow!("--database-url requires a value"))?
                        .to_owned(),
                );
            }
            "--open-micros" => {
                cursor += 1;
                let value = args
                    .get(cursor)
                    .ok_or_else(|| anyhow!("--open-micros requires a value"))?;
                open_micros = value
                    .parse()
                    .with_context(|| format!("parse --open-micros value {value:?}"))?;
            }
            "--suite" => {
                cursor += 1;
                let value = args
                    .get(cursor)
                    .ok_or_else(|| anyhow!("--suite requires a value"))?;
                suite = match value.as_str() {
                    "all" => BenchmarkSuiteFilter::All,
                    "rtt" | "roundtrip" | "round-trip" => BenchmarkSuiteFilter::Rtt,
                    "speed" | "sqlite" | "sqlite-suite" => BenchmarkSuiteFilter::Speed,
                    other => bail!("unknown --suite value {other:?}; use all, rtt, or speed"),
                };
            }
            "--iterations" => {
                cursor += 1;
                let value = args
                    .get(cursor)
                    .ok_or_else(|| anyhow!("--iterations requires a value"))?;
                rtt_iterations = value
                    .parse()
                    .with_context(|| format!("parse --iterations value {value:?}"))?;
            }
            "--speed-source" => {
                cursor += 1;
                let value = args
                    .get(cursor)
                    .ok_or_else(|| anyhow!("--speed-source requires a value"))?;
                speed_sql_source = match value.as_str() {
                    "generated" | "local" => SpeedSqlSource::Generated,
                    "pglite" | "pglite-vendored" | "upstream" => SpeedSqlSource::PgliteVendored,
                    other => {
                        bail!("unknown --speed-source value {other:?}; use generated or pglite")
                    }
                };
            }
            other => bail!("unknown perf pglite-nodefs-sqlx flag: {other}"),
        }
        cursor += 1;
    }
    ensure!(rtt_iterations > 0, "--iterations must be greater than zero");
    let database_url = database_url.ok_or_else(|| anyhow!("--database-url is required"))?;

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("create PGlite NodeFS SQLx benchmark Tokio runtime")?;
    let runs = runtime.block_on(async {
        let connect_started = Instant::now();
        let mut conn = sqlx::PgConnection::connect(&database_url)
            .await
            .context("connect SQLx client to PGlite NodeFS socket server")?;
        let connect_micros = connect_started.elapsed().as_micros();

        let mut runs = Vec::new();
        if suite.includes("rtt") {
            runs.push(
                run_pglite_nodefs_rtt_sqlx_benchmark(
                    &mut conn,
                    rtt_iterations,
                    open_micros,
                    connect_micros,
                )
                .await?,
            );
        }
        if suite.includes("speed") {
            runs.push(
                run_pglite_nodefs_speed_sqlx_benchmark(
                    &mut conn,
                    speed_sql_source,
                    open_micros,
                    connect_micros,
                )
                .await?,
            );
        }
        conn.close()
            .await
            .context("close SQLx PGlite NodeFS benchmark client")?;
        Ok::<_, anyhow::Error>(runs)
    })?;

    let report = BenchmarkReport {
        wasmer_version: "node-pglite",
        wasmer_wasix_version: "node-pglite",
        source_model: speed_sql_source.source_model(),
        measurement_model: "Upstream PGlite control. A Node process starts @electric-sql/pglite with NodeFS persistence and @electric-sql/pglite-socket, then xtask runs the benchmark SQL through one long-lived SQLx connection.",
        rtt_iterations,
        speed_scale: 1.0,
        preload_micros: 0,
        runs,
    };
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

async fn run_native_postgres_rtt_sqlx_benchmark(
    conn: &mut sqlx::PgConnection,
    iterations: usize,
    open_micros: u128,
    connect_micros: u128,
) -> Result<BenchmarkRun> {
    let setup_started = Instant::now();
    conn.execute(rtt_setup_sql())
        .await
        .context("execute native Postgres RTT setup over SQLx")?;
    let setup_micros = setup_started.elapsed().as_micros();

    let mut tests = Vec::new();
    for case in rtt_cases() {
        let mut samples = Vec::with_capacity(iterations);
        for _ in 0..iterations {
            let started = Instant::now();
            conn.execute(case.sql.as_str()).await.with_context(|| {
                format!(
                    "execute native Postgres RTT benchmark {} over SQLx",
                    case.id
                )
            })?;
            samples.push(started.elapsed().as_micros());
        }
        tests.push(samples_result(
            case.id,
            format!("Test {}: {}", case.id, case.label),
            "milliseconds",
            iterations,
            samples,
        ));
    }

    Ok(BenchmarkRun {
        suite: "rtt",
        mode: "native_postgres_sqlx",
        description: "Native Postgres over TCP using one long-lived SQLx connection.",
        open_micros,
        connect_micros: Some(connect_micros),
        setup_micros,
        tests,
    })
}

async fn run_pglite_nodefs_rtt_sqlx_benchmark(
    conn: &mut sqlx::PgConnection,
    iterations: usize,
    open_micros: u128,
    connect_micros: u128,
) -> Result<BenchmarkRun> {
    let setup_started = Instant::now();
    conn.execute(rtt_setup_sql())
        .await
        .context("execute PGlite NodeFS RTT setup over SQLx")?;
    let setup_micros = setup_started.elapsed().as_micros();

    let mut tests = Vec::new();
    for case in rtt_cases() {
        let mut samples = Vec::with_capacity(iterations);
        for _ in 0..iterations {
            let started = Instant::now();
            conn.execute(case.sql.as_str()).await.with_context(|| {
                format!("execute PGlite NodeFS RTT benchmark {} over SQLx", case.id)
            })?;
            samples.push(started.elapsed().as_micros());
        }
        tests.push(samples_result(
            case.id,
            format!("Test {}: {}", case.id, case.label),
            "milliseconds",
            iterations,
            samples,
        ));
    }

    Ok(BenchmarkRun {
        suite: "rtt",
        mode: "pglite_nodefs_sqlx",
        description: "Upstream PGlite NodeFS over the Postgres wire protocol using one long-lived SQLx connection.",
        open_micros,
        connect_micros: Some(connect_micros),
        setup_micros,
        tests,
    })
}

async fn run_native_postgres_speed_sqlx_benchmark(
    conn: &mut sqlx::PgConnection,
    sql_source: SpeedSqlSource,
    open_micros: u128,
    connect_micros: u128,
) -> Result<BenchmarkRun> {
    conn.execute(
        "DROP TABLE IF EXISTS t1 CASCADE;\
         DROP TABLE IF EXISTS t2 CASCADE;\
         DROP TABLE IF EXISTS t2_1 CASCADE;\
         DROP TABLE IF EXISTS t3 CASCADE;\
         DROP TABLE IF EXISTS t3_1 CASCADE;",
    )
    .await
    .context("clear native Postgres speed benchmark tables over SQLx")?;

    let mut tests = Vec::new();
    for case in speed_cases(1.0, sql_source)? {
        let started = Instant::now();
        conn.execute(case.sql.as_str()).await.with_context(|| {
            format!(
                "execute native Postgres speed benchmark {} over SQLx",
                case.id
            )
        })?;
        tests.push(single_sample_result(
            case.id,
            case.label,
            "seconds",
            case.operation_count,
            started.elapsed(),
        ));
    }
    Ok(BenchmarkRun {
        suite: "speed",
        mode: "native_postgres_sqlx",
        description: "Native Postgres speed suite over TCP using one SQLx connection.",
        open_micros,
        connect_micros: Some(connect_micros),
        setup_micros: 0,
        tests,
    })
}

async fn run_pglite_nodefs_speed_sqlx_benchmark(
    conn: &mut sqlx::PgConnection,
    sql_source: SpeedSqlSource,
    open_micros: u128,
    connect_micros: u128,
) -> Result<BenchmarkRun> {
    conn.execute(
        "DROP TABLE IF EXISTS t1 CASCADE;\
         DROP TABLE IF EXISTS t2 CASCADE;\
         DROP TABLE IF EXISTS t2_1 CASCADE;\
         DROP TABLE IF EXISTS t3 CASCADE;\
         DROP TABLE IF EXISTS t3_1 CASCADE;",
    )
    .await
    .context("clear PGlite NodeFS speed benchmark tables over SQLx")?;

    let mut tests = Vec::new();
    for case in speed_cases(1.0, sql_source)? {
        let started = Instant::now();
        conn.execute(case.sql.as_str()).await.with_context(|| {
            format!(
                "execute PGlite NodeFS speed benchmark {} over SQLx",
                case.id
            )
        })?;
        tests.push(single_sample_result(
            case.id,
            case.label,
            "seconds",
            case.operation_count,
            started.elapsed(),
        ));
    }
    Ok(BenchmarkRun {
        suite: "speed",
        mode: "pglite_nodefs_sqlx",
        description: "Upstream PGlite NodeFS speed suite over TCP using one SQLx connection.",
        open_micros,
        connect_micros: Some(connect_micros),
        setup_micros: 0,
        tests,
    })
}

fn perf_prepared_updates(args: &[String]) -> Result<()> {
    let mut rows = 25_000usize;
    let mut skip_native = false;
    let mut gate = false;
    let mut cursor = 0usize;
    while cursor < args.len() {
        match args[cursor].as_str() {
            "--skip-native" => {
                skip_native = true;
            }
            "--gate" => {
                gate = true;
            }
            "--rows" => {
                cursor += 1;
                let value = args
                    .get(cursor)
                    .ok_or_else(|| anyhow!("--rows requires a value"))?;
                rows = value
                    .parse()
                    .with_context(|| format!("parse --rows value {value:?}"))?;
            }
            other => bail!("unknown perf prepared-updates flag: {other}"),
        }
        cursor += 1;
    }
    ensure!(rows > 0, "--rows must be greater than zero");

    Pglite::preload()?;
    let numeric_updates = parsed_numeric_updates(rows)?;
    let text_updates = parsed_text_updates(rows)?;
    ensure!(
        numeric_updates.len() == rows && text_updates.len() == rows,
        "prepared update parser returned fewer rows than requested"
    );

    let mut runs = vec![
        pglite_prepared_update_run(
            "pglite_server_sqlx",
            "PgliteServer over TCP using SQLx parameterized queries and SQLx statement cache.",
            || run_pglite_sqlx_prepared_update_tests(&numeric_updates, &text_updates),
        )?,
        pglite_prepared_update_run(
            "pglite_server_tcp_tokio_postgres_prepared",
            "PgliteServer over TCP using tokio-postgres explicit prepared statements.",
            || {
                run_pglite_tokio_prepared_update_tests(
                    &numeric_updates,
                    &text_updates,
                    PglitePreparedEndpoint::Tcp,
                    PreparedExecution::Sequential,
                )
            },
        )?,
        pglite_prepared_update_run(
            "pglite_server_tcp_tokio_postgres_pipelined_prepared",
            "PgliteServer over TCP using tokio-postgres explicit prepared statements with all update futures pipelined inside one transaction.",
            || {
                run_pglite_tokio_prepared_update_tests(
                    &numeric_updates,
                    &text_updates,
                    PglitePreparedEndpoint::Tcp,
                    PreparedExecution::Pipelined,
                )
            },
        )?,
    ];
    #[cfg(unix)]
    {
        runs.push(pglite_prepared_update_run(
            "pglite_server_unix_tokio_postgres_prepared",
            "PgliteServer over Unix socket using tokio-postgres explicit prepared statements.",
            || {
                run_pglite_tokio_prepared_update_tests(
                    &numeric_updates,
                    &text_updates,
                    PglitePreparedEndpoint::Unix,
                    PreparedExecution::Sequential,
                )
            },
        )?);
        runs.push(pglite_prepared_update_run(
            "pglite_server_unix_tokio_postgres_pipelined_prepared",
            "PgliteServer over Unix socket using tokio-postgres explicit prepared statements with all update futures pipelined inside one transaction.",
            || run_pglite_tokio_prepared_update_tests(
                &numeric_updates,
                &text_updates,
                PglitePreparedEndpoint::Unix,
                PreparedExecution::Pipelined,
            ),
        )?);
    }
    if !skip_native {
        let native_postgres = env::var("PGLITE_OXIDE_NATIVE_POSTGRES")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("postgres"));
        let native_initdb = env::var("PGLITE_OXIDE_NATIVE_INITDB")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("initdb"));
        runs.push(PreparedUpdateRun {
            mode: "native_tokio_postgres_prepared",
            description: "Native Postgres over Unix socket using tokio-postgres explicit prepared statements.",
            protocol_stats: None,
            tests: run_native_prepared_update_tests(
                &native_postgres,
                &native_initdb,
                &numeric_updates,
                &text_updates,
                PreparedExecution::Sequential,
            )?,
        });
        runs.push(PreparedUpdateRun {
            mode: "native_tokio_postgres_pipelined_prepared",
            description: "Native Postgres over Unix socket using tokio-postgres explicit prepared statements with all update futures pipelined inside one transaction.",
            protocol_stats: None,
            tests: run_native_prepared_update_tests(
                &native_postgres,
                &native_initdb,
                &numeric_updates,
                &text_updates,
                PreparedExecution::Pipelined,
            )?,
        });
    }

    let report = PreparedUpdateReport {
        source_model: "Exact PGlite benchmark2/benchmark6 setup plus update values parsed from benchmark9 and benchmark10.",
        measurement_model: "Each test uses a fresh database, creates the same indexed t2 table, prepares one parameterized UPDATE statement, then executes N updates inside one transaction. PGlite server runs use one local server per test; native Postgres uses a temporary Unix-socket cluster with the same benchmark GUCs as perf native-postgres.",
        gate_model: gate.then_some("Optional local regression gate for pglite-oxide server prepared-update transport: SQLx and sequential tokio-postgres must stay below 5s per 25k rows, pipelined tokio-postgres must stay below 1.5s per 25k rows, non-COPY prepared traffic must not use streaming handoff, and pipelined prepared traffic must stay batched. Thresholds scale linearly with --rows."),
        rows,
        runs,
    };

    println!("{}", serde_json::to_string_pretty(&report)?);
    if gate {
        validate_prepared_update_gate(&report)?;
    }
    Ok(())
}

fn pglite_prepared_update_run(
    mode: &'static str,
    description: &'static str,
    run: impl FnOnce() -> Result<Vec<PreparedUpdateTest>>,
) -> Result<PreparedUpdateRun> {
    reset_protocol_stats();
    let tests = match run() {
        Ok(tests) => tests,
        Err(err) => {
            disable_protocol_stats();
            return Err(err);
        }
    };
    let protocol_stats = Some(protocol_stats_snapshot());
    disable_protocol_stats();
    Ok(PreparedUpdateRun {
        mode,
        description,
        protocol_stats,
        tests,
    })
}

fn validate_prepared_update_gate(report: &PreparedUpdateReport) -> Result<()> {
    let scale = report.rows as f64 / 25_000_f64;
    for run in &report.runs {
        let Some(base_limit_micros) = prepared_update_limit_micros(run.mode) else {
            continue;
        };
        let limit = (base_limit_micros as f64 * scale).ceil() as u128;
        for test in &run.tests {
            ensure!(
                test.elapsed_micros <= limit,
                "prepared-update gate failed for {} {}: {:.3}ms > {:.3}ms",
                run.mode,
                test.id,
                test.elapsed_micros as f64 / 1_000.0,
                limit as f64 / 1_000.0
            );
        }
        if let Some(stats) = run.protocol_stats.as_ref() {
            ensure!(
                stats.streaming_copy_handoffs == 0,
                "prepared-update gate failed for {}: non-COPY traffic used streaming handoff",
                run.mode
            );
        }
        if run.mode.contains("pipelined") {
            let stats = run
                .protocol_stats
                .as_ref()
                .context("missing protocol stats for pipelined prepared-update run")?;
            ensure!(
                stats.protocol_batches < 1_000,
                "prepared-update gate failed for {}: pipelined traffic was not batched ({} protocol batches)",
                run.mode,
                stats.protocol_batches
            );
        }
    }
    Ok(())
}

fn prepared_update_limit_micros(mode: &str) -> Option<u128> {
    if mode.starts_with("native_") {
        return None;
    }
    if mode.contains("pipelined") {
        Some(1_500_000)
    } else {
        Some(5_000_000)
    }
}

fn run_pglite_sqlx_prepared_update_tests(
    numeric_updates: &[(i32, i32)],
    text_updates: &[(i32, String)],
) -> Result<Vec<PreparedUpdateTest>> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("create prepared-update SQLx Tokio runtime")?;

    let numeric = run_pglite_sqlx_prepared_update_case(
        &runtime,
        "numeric_indexed",
        "Parameterized numeric UPDATEs with indexes on lookup and updated columns",
        "UPDATE t2 SET b=$1 WHERE a=$2",
        PreparedUpdateValues::Numeric(numeric_updates),
    )?;
    let text = run_pglite_sqlx_prepared_update_case(
        &runtime,
        "text_indexed",
        "Parameterized text UPDATEs with indexes on lookup and numeric column",
        "UPDATE t2 SET c=$1 WHERE a=$2",
        PreparedUpdateValues::Text(text_updates),
    )?;
    Ok(vec![numeric, text])
}

enum PreparedUpdateValues<'a> {
    Numeric(&'a [(i32, i32)]),
    Text(&'a [(i32, String)]),
}

impl PreparedUpdateValues<'_> {
    fn len(&self) -> usize {
        match self {
            Self::Numeric(values) => values.len(),
            Self::Text(values) => values.len(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PreparedExecution {
    Sequential,
    Pipelined,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PglitePreparedEndpoint {
    Tcp,
    #[cfg(unix)]
    Unix,
}

fn run_pglite_sqlx_prepared_update_case(
    runtime: &tokio::runtime::Runtime,
    id: &'static str,
    label: &'static str,
    sql: &'static str,
    values: PreparedUpdateValues<'_>,
) -> Result<PreparedUpdateTest> {
    let open_started = Instant::now();
    let server = PgliteServer::temporary_tcp()?;
    let open_micros = open_started.elapsed().as_micros();
    let uri = server.database_url();
    let operation_count = values.len();

    let test = runtime.block_on(async {
        let connect_started = Instant::now();
        let mut conn = sqlx::PgConnection::connect(&uri)
            .await
            .context("connect SQLx prepared-update client")?;
        let connect_micros = connect_started.elapsed().as_micros();

        let setup_started = Instant::now();
        conn.execute(read_pglite_benchmark_sql("2")?.as_str())
            .await
            .context("execute prepared-update SQLx setup benchmark2")?;
        conn.execute(read_pglite_benchmark_sql("6")?.as_str())
            .await
            .context("execute prepared-update SQLx setup benchmark6")?;
        let setup_micros = setup_started.elapsed().as_micros();

        let prepare_started = Instant::now();
        let _statement = conn
            .prepare(sql)
            .await
            .with_context(|| format!("prepare SQLx statement {sql}"))?;
        let prepare_micros = prepare_started.elapsed().as_micros();

        let elapsed = measure_async_transaction_sqlx(&mut conn, sql, values).await?;
        conn.close()
            .await
            .context("close SQLx prepared-update client")?;

        Ok::<_, anyhow::Error>(PreparedUpdateTest {
            id,
            label,
            open_micros,
            connect_micros,
            setup_micros,
            prepare_micros: Some(prepare_micros),
            elapsed_micros: elapsed.as_micros(),
            operation_count,
            average_micros: elapsed.as_micros() as f64 / operation_count as f64,
        })
    })?;
    server.shutdown()?;
    Ok(test)
}

async fn measure_async_transaction_sqlx(
    conn: &mut sqlx::PgConnection,
    sql: &'static str,
    values: PreparedUpdateValues<'_>,
) -> Result<Duration> {
    let started = Instant::now();
    conn.execute("BEGIN")
        .await
        .context("begin SQLx transaction")?;
    match values {
        PreparedUpdateValues::Numeric(values) => {
            for (lookup, value) in values {
                sqlx::query(sql)
                    .bind(*value)
                    .bind(*lookup)
                    .execute(&mut *conn)
                    .await
                    .context("execute SQLx prepared numeric update")?;
            }
        }
        PreparedUpdateValues::Text(values) => {
            for (lookup, value) in values {
                sqlx::query(sql)
                    .bind(value.as_str())
                    .bind(*lookup)
                    .execute(&mut *conn)
                    .await
                    .context("execute SQLx prepared text update")?;
            }
        }
    }
    conn.execute("COMMIT")
        .await
        .context("commit SQLx transaction")?;
    Ok(started.elapsed())
}

fn run_pglite_tokio_prepared_update_tests(
    numeric_updates: &[(i32, i32)],
    text_updates: &[(i32, String)],
    endpoint: PglitePreparedEndpoint,
    execution: PreparedExecution,
) -> Result<Vec<PreparedUpdateTest>> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("create prepared-update tokio-postgres runtime")?;

    Ok(vec![
        run_pglite_tokio_prepared_update_case(
            &runtime,
            "numeric_indexed",
            "Parameterized numeric UPDATEs with indexes on lookup and updated columns",
            "UPDATE t2 SET b=$1 WHERE a=$2",
            numeric_updates,
            None,
            endpoint,
            execution,
        )?,
        run_pglite_tokio_prepared_update_case(
            &runtime,
            "text_indexed",
            "Parameterized text UPDATEs with indexes on lookup and numeric column",
            "UPDATE t2 SET c=$1 WHERE a=$2",
            &[],
            Some(text_updates),
            endpoint,
            execution,
        )?,
    ])
}

#[allow(clippy::too_many_arguments)]
fn run_pglite_tokio_prepared_update_case(
    runtime: &tokio::runtime::Runtime,
    id: &'static str,
    label: &'static str,
    sql: &'static str,
    numeric_updates: &[(i32, i32)],
    text_updates: Option<&[(i32, String)]>,
    endpoint: PglitePreparedEndpoint,
    execution: PreparedExecution,
) -> Result<PreparedUpdateTest> {
    let open_started = Instant::now();
    let server = start_prepared_update_pglite_server(endpoint)?;
    let open_micros = open_started.elapsed().as_micros();
    let connection = pglite_prepared_update_connection(&server, endpoint)?;
    #[cfg(unix)]
    let cleanup_socket_dir = match &connection {
        PreparedPgliteConnection::Tcp(_) => None,
        PreparedPgliteConnection::Unix { socket_dir, .. } => Some(socket_dir.clone()),
    };

    let test = runtime.block_on(async {
        let mut config = tokio_postgres::Config::new();
        config.user("postgres").dbname("template1");
        match &connection {
            PreparedPgliteConnection::Tcp(addr) => {
                config.host(addr.ip().to_string()).port(addr.port());
            }
            #[cfg(unix)]
            PreparedPgliteConnection::Unix { socket_dir, port } => {
                config.host_path(socket_dir).port(*port);
            }
        }
        let connect_started = Instant::now();
        let (client, connection) = config
            .connect(tokio_postgres::NoTls)
            .await
            .context("connect tokio-postgres prepared-update client")?;
        let connection_task = tokio::spawn(async move {
            if let Err(err) = connection.await {
                eprintln!("prepared-update pglite connection error: {err}");
            }
        });
        let connect_micros = connect_started.elapsed().as_micros();

        let result = run_tokio_prepared_update_case_on_client(
            &client,
            id,
            label,
            sql,
            numeric_updates,
            text_updates,
            execution,
            open_micros,
            connect_micros,
        )
        .await;
        drop(client);
        let _ = connection_task.await;
        result
    })?;
    server.shutdown()?;
    #[cfg(unix)]
    if let Some(socket_dir) = cleanup_socket_dir {
        let _ = fs::remove_dir_all(socket_dir);
    }
    Ok(test)
}

fn start_prepared_update_pglite_server(endpoint: PglitePreparedEndpoint) -> Result<PgliteServer> {
    match endpoint {
        PglitePreparedEndpoint::Tcp => PgliteServer::temporary_tcp(),
        #[cfg(unix)]
        PglitePreparedEndpoint::Unix => {
            let socket_dir = env::current_dir()
                .context("read current directory")?
                .join("target/perf")
                .join(format!(
                    "pglite-prepared-unix-{}-{}",
                    std::process::id(),
                    now_micros()?
                ));
            let port = 5432;
            let socket_path = socket_dir.join(format!(".s.PGSQL.{port}"));
            PgliteServer::builder()
                .temporary()
                .unix(socket_path)
                .start()
        }
    }
}

enum PreparedPgliteConnection {
    Tcp(std::net::SocketAddr),
    #[cfg(unix)]
    Unix {
        socket_dir: PathBuf,
        port: u16,
    },
}

fn pglite_prepared_update_connection(
    server: &PgliteServer,
    endpoint: PglitePreparedEndpoint,
) -> Result<PreparedPgliteConnection> {
    match endpoint {
        PglitePreparedEndpoint::Tcp => {
            let addr = server
                .tcp_addr()
                .ok_or_else(|| anyhow!("prepared-update PgliteServer did not bind TCP"))?;
            Ok(PreparedPgliteConnection::Tcp(addr))
        }
        #[cfg(unix)]
        PglitePreparedEndpoint::Unix => {
            let socket_path = server
                .socket_path()
                .ok_or_else(|| anyhow!("prepared-update PgliteServer did not bind Unix socket"))?;
            let socket_dir = socket_path
                .parent()
                .ok_or_else(|| anyhow!("prepared-update Unix socket has no parent directory"))?
                .to_path_buf();
            let port = socket_path
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.strip_prefix(".s.PGSQL."))
                .ok_or_else(|| {
                    anyhow!(
                        "prepared-update Unix socket path is not libpq-shaped: {}",
                        socket_path.display()
                    )
                })?
                .parse()
                .context("parse prepared-update Unix socket port")?;
            Ok(PreparedPgliteConnection::Unix { socket_dir, port })
        }
    }
}

fn run_native_prepared_update_tests(
    postgres_bin: &Path,
    initdb_bin: &Path,
    numeric_updates: &[(i32, i32)],
    text_updates: &[(i32, String)],
    execution: PreparedExecution,
) -> Result<Vec<PreparedUpdateTest>> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("create native prepared-update Tokio runtime")?;

    Ok(vec![
        run_native_prepared_update_case(
            &runtime,
            postgres_bin,
            initdb_bin,
            "numeric_indexed",
            "Parameterized numeric UPDATEs with indexes on lookup and updated columns",
            "UPDATE t2 SET b=$1 WHERE a=$2",
            numeric_updates,
            None,
            execution,
        )?,
        run_native_prepared_update_case(
            &runtime,
            postgres_bin,
            initdb_bin,
            "text_indexed",
            "Parameterized text UPDATEs with indexes on lookup and numeric column",
            "UPDATE t2 SET c=$1 WHERE a=$2",
            &[],
            Some(text_updates),
            execution,
        )?,
    ])
}

#[allow(clippy::too_many_arguments)]
fn run_native_prepared_update_case(
    runtime: &tokio::runtime::Runtime,
    postgres_bin: &Path,
    initdb_bin: &Path,
    id: &'static str,
    label: &'static str,
    sql: &'static str,
    numeric_updates: &[(i32, i32)],
    text_updates: Option<&[(i32, String)]>,
    execution: PreparedExecution,
) -> Result<PreparedUpdateTest> {
    let open_started = Instant::now();
    let native = NativePostgres::start(postgres_bin, initdb_bin)?;
    let open_micros = open_started.elapsed().as_micros();

    runtime.block_on(async {
        let mut config = tokio_postgres::Config::new();
        configure_native_postgres_client(&mut config, &native);
        let connect_started = Instant::now();
        let (client, connection) = config
            .connect(tokio_postgres::NoTls)
            .await
            .context("connect native prepared-update client")?;
        let connection_task = tokio::spawn(async move {
            if let Err(err) = connection.await {
                eprintln!("native prepared-update connection error: {err}");
            }
        });
        let connect_micros = connect_started.elapsed().as_micros();

        let result = run_tokio_prepared_update_case_on_client(
            &client,
            id,
            label,
            sql,
            numeric_updates,
            text_updates,
            execution,
            open_micros,
            connect_micros,
        )
        .await;
        drop(client);
        let _ = connection_task.await;
        result
    })
}

#[allow(clippy::too_many_arguments)]
async fn run_tokio_prepared_update_case_on_client(
    client: &tokio_postgres::Client,
    id: &'static str,
    label: &'static str,
    sql: &'static str,
    numeric_updates: &[(i32, i32)],
    text_updates: Option<&[(i32, String)]>,
    execution: PreparedExecution,
    open_micros: u128,
    connect_micros: u128,
) -> Result<PreparedUpdateTest> {
    let setup_started = Instant::now();
    client
        .simple_query(&read_pglite_benchmark_sql("2")?)
        .await
        .context("execute prepared-update setup benchmark2")?;
    client
        .simple_query(&read_pglite_benchmark_sql("6")?)
        .await
        .context("execute prepared-update setup benchmark6")?;
    let setup_micros = setup_started.elapsed().as_micros();

    let prepare_started = Instant::now();
    let statement = client
        .prepare(sql)
        .await
        .with_context(|| format!("prepare tokio-postgres statement {sql}"))?;
    let prepare_micros = prepare_started.elapsed().as_micros();

    let started = Instant::now();
    client
        .simple_query("BEGIN")
        .await
        .context("begin tokio-postgres prepared-update transaction")?;
    let operation_count = if let Some(text_updates) = text_updates {
        match execution {
            PreparedExecution::Sequential => {
                for (lookup, value) in text_updates {
                    let params: [&(dyn tokio_postgres::types::ToSql + Sync); 2] = [value, lookup];
                    client
                        .execute(&statement, &params)
                        .await
                        .context("execute tokio-postgres prepared text update")?;
                }
            }
            PreparedExecution::Pipelined => {
                let updates = text_updates.iter().map(|(lookup, value)| {
                    let statement = &statement;
                    async move {
                        let params: [&(dyn tokio_postgres::types::ToSql + Sync); 2] =
                            [value, lookup];
                        client.execute(statement, &params).await
                    }
                });
                try_join_all(updates)
                    .await
                    .context("execute pipelined tokio-postgres prepared text updates")?;
            }
        }
        text_updates.len()
    } else {
        match execution {
            PreparedExecution::Sequential => {
                for (lookup, value) in numeric_updates {
                    let params: [&(dyn tokio_postgres::types::ToSql + Sync); 2] = [value, lookup];
                    client
                        .execute(&statement, &params)
                        .await
                        .context("execute tokio-postgres prepared numeric update")?;
                }
            }
            PreparedExecution::Pipelined => {
                let updates = numeric_updates.iter().map(|(lookup, value)| {
                    let statement = &statement;
                    async move {
                        let params: [&(dyn tokio_postgres::types::ToSql + Sync); 2] =
                            [value, lookup];
                        client.execute(statement, &params).await
                    }
                });
                try_join_all(updates)
                    .await
                    .context("execute pipelined tokio-postgres prepared numeric updates")?;
            }
        }
        numeric_updates.len()
    };
    client
        .simple_query("COMMIT")
        .await
        .context("commit tokio-postgres prepared-update transaction")?;
    let elapsed = started.elapsed();

    Ok(PreparedUpdateTest {
        id,
        label,
        open_micros,
        connect_micros,
        setup_micros,
        prepare_micros: Some(prepare_micros),
        elapsed_micros: elapsed.as_micros(),
        operation_count,
        average_micros: elapsed.as_micros() as f64 / operation_count as f64,
    })
}

fn parsed_numeric_updates(limit: usize) -> Result<Vec<(i32, i32)>> {
    let sql = read_pglite_benchmark_sql("9")?;
    let mut updates = Vec::with_capacity(limit);
    for line in sql.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("UPDATE t2 SET b=") else {
            continue;
        };
        let rest = rest
            .strip_suffix(';')
            .ok_or_else(|| anyhow!("numeric update line is missing semicolon: {line}"))?;
        let (value, lookup) = rest
            .split_once(" WHERE a=")
            .ok_or_else(|| anyhow!("numeric update line has unexpected shape: {line}"))?;
        updates.push((lookup.parse()?, value.parse()?));
        if updates.len() == limit {
            break;
        }
    }
    ensure!(
        updates.len() == limit,
        "benchmark9 only contained {} update rows; requested {limit}",
        updates.len()
    );
    Ok(updates)
}

fn parsed_text_updates(limit: usize) -> Result<Vec<(i32, String)>> {
    let sql = read_pglite_benchmark_sql("10")?;
    let mut updates = Vec::with_capacity(limit);
    for line in sql.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("UPDATE t2 SET c='") else {
            continue;
        };
        let rest = rest
            .strip_suffix(';')
            .ok_or_else(|| anyhow!("text update line is missing semicolon: {line}"))?;
        let (value, lookup) = rest
            .split_once("' WHERE a=")
            .ok_or_else(|| anyhow!("text update line has unexpected shape: {line}"))?;
        updates.push((lookup.parse()?, value.to_owned()));
        if updates.len() == limit {
            break;
        }
    }
    ensure!(
        updates.len() == limit,
        "benchmark10 only contained {} update rows; requested {limit}",
        updates.len()
    );
    Ok(updates)
}

struct NativePostgres {
    child: Child,
    root: PathBuf,
    socket_dir: PathBuf,
    port: u16,
}

impl NativePostgres {
    fn start(postgres_bin: &Path, initdb_bin: &Path) -> Result<Self> {
        let root = env::current_dir()
            .context("read current directory")?
            .join("target/perf")
            .join(format!(
                "native-postgres-{}-{}",
                std::process::id(),
                now_micros()?
            ));
        let data_dir = root.join("data");
        let socket_dir = root.join("socket");
        fs::create_dir_all(&data_dir).with_context(|| format!("create {}", data_dir.display()))?;
        fs::create_dir_all(&socket_dir)
            .with_context(|| format!("create {}", socket_dir.display()))?;

        let init_status = Command::new(initdb_bin)
            .arg("-D")
            .arg(&data_dir)
            .args([
                "-A",
                "trust",
                "-U",
                "postgres",
                "--encoding=UTF8",
                "--no-instructions",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .status()
            .with_context(|| format!("spawn native initdb {}", initdb_bin.display()))?;
        ensure!(
            init_status.success(),
            "native initdb failed with {init_status}"
        );

        let port = 55432 + (std::process::id() % 1000) as u16;
        let log_path = root.join("postgres.log");
        let log = fs::File::create(&log_path)
            .with_context(|| format!("create native Postgres log {}", log_path.display()))?;
        let mut command = Command::new(postgres_bin);
        command.arg("-D").arg(&data_dir);
        #[cfg(unix)]
        {
            command
                .arg("-h")
                .arg("127.0.0.1")
                .arg("-k")
                .arg(&socket_dir);
        }
        #[cfg(not(unix))]
        {
            command.arg("-h").arg("127.0.0.1");
        }
        let child = command
            .arg("-p")
            .arg(port.to_string())
            .args([
                "-F",
                "-c",
                "fsync=off",
                "-c",
                "synchronous_commit=on",
                "-c",
                "shared_buffers=128MB",
                "-c",
                "wal_buffers=4MB",
                "-c",
                "min_wal_size=80MB",
                "-c",
                "max_worker_processes=1",
                "-c",
                "max_parallel_workers=0",
                "-c",
                "max_parallel_workers_per_gather=0",
                "-c",
                "autovacuum=off",
                "-c",
                "log_checkpoints=off",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::from(log))
            .spawn()
            .with_context(|| format!("spawn native postgres {}", postgres_bin.display()))?;

        let mut native = Self {
            child,
            root,
            socket_dir,
            port,
        };
        native.wait_ready(&log_path)?;
        Ok(native)
    }

    fn wait_ready(&mut self, log_path: &Path) -> Result<()> {
        #[cfg(unix)]
        let socket_path = self.socket_dir.join(format!(".s.PGSQL.{}", self.port));
        let start = Instant::now();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("create native Postgres readiness Tokio runtime")?;
        let mut last_probe_error = None;
        while start.elapsed() < Duration::from_secs(15) {
            if let Some(status) = self.child.try_wait().context("poll native postgres")? {
                let log = fs::read_to_string(log_path).unwrap_or_default();
                bail!("native postgres exited early with {status}; log:\n{log}");
            }
            #[cfg(unix)]
            let transport_ready = socket_path.exists();
            #[cfg(not(unix))]
            let transport_ready = true;
            if transport_ready {
                match runtime.block_on(self.probe_ready()) {
                    Ok(()) => return Ok(()),
                    Err(err) => last_probe_error = Some(err),
                }
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let log = fs::read_to_string(log_path).unwrap_or_default();
        let probe = last_probe_error
            .map(|err| format!("last readiness probe error: {err}\n"))
            .unwrap_or_default();
        bail!("native postgres did not become ready; {probe}log:\n{log}");
    }

    async fn probe_ready(&self) -> Result<()> {
        let mut config = tokio_postgres::Config::new();
        configure_native_postgres_client(&mut config, self);
        let (client, connection) = config
            .connect(tokio_postgres::NoTls)
            .await
            .context("connect readiness probe")?;
        let connection_task = tokio::spawn(async move {
            let _ = connection.await;
        });
        let query_result = client
            .simple_query("SELECT 1")
            .await
            .context("run readiness probe query");
        drop(client);
        connection_task.abort();
        query_result.map(|_| ())
    }
}

fn configure_native_postgres_client(config: &mut tokio_postgres::Config, native: &NativePostgres) {
    config.user("postgres").dbname("postgres").port(native.port);
    #[cfg(unix)]
    {
        config.host_path(&native.socket_dir);
    }
    #[cfg(not(unix))]
    {
        config.host("127.0.0.1");
    }
}

impl Drop for NativePostgres {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            terminate_child_gracefully(&mut self.child);
            if self.child.try_wait().ok().flatten().is_none() {
                let _ = self.child.kill();
            }
            let _ = self.child.wait();
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn terminate_child_gracefully(child: &mut Child) {
    #[cfg(unix)]
    {
        let _ = Command::new("kill")
            .arg("-TERM")
            .arg(child.id().to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(5) {
            if child.try_wait().ok().flatten().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
    #[cfg(not(unix))]
    {
        let _ = child;
    }
}

fn perf_diagnose_indexed_update() -> Result<()> {
    Pglite::preload()?;

    let benchmark2 = read_pglite_benchmark_sql("2")?;
    let benchmark6 = read_pglite_benchmark_sql("6")?;
    let benchmark9 = read_pglite_benchmark_sql("9")?;
    let benchmark10 = read_pglite_benchmark_sql("10")?;
    let unlogged_benchmark2 = benchmark2.replace("CREATE TABLE", "CREATE UNLOGGED TABLE");
    let lookup_index_only = "CREATE INDEX i2a ON t2(a);\n";

    let cases = vec![
        run_indexed_update_diagnostic_case(
            "exact_numeric_indexed",
            "PGlite benchmark2 + benchmark6, then exact benchmark9 numeric updates",
            &[benchmark2.as_str(), benchmark6.as_str()],
            &benchmark9,
            25_000,
        )?,
        run_indexed_update_diagnostic_case(
            "exact_text_indexed",
            "PGlite benchmark2 + benchmark6, then exact benchmark10 text updates",
            &[benchmark2.as_str(), benchmark6.as_str()],
            &benchmark10,
            25_000,
        )?,
        run_indexed_update_diagnostic_case(
            "numeric_lookup_index_only",
            "PGlite benchmark2 + index on lookup column a only, then exact benchmark9 numeric updates",
            &[benchmark2.as_str(), lookup_index_only],
            &benchmark9,
            25_000,
        )?,
        run_indexed_update_diagnostic_case(
            "text_lookup_index_only",
            "PGlite benchmark2 + index on lookup column a only, then exact benchmark10 text updates",
            &[benchmark2.as_str(), lookup_index_only],
            &benchmark10,
            25_000,
        )?,
        run_indexed_update_diagnostic_case(
            "numeric_unlogged_indexed",
            "PGlite benchmark2 rewritten to UNLOGGED + benchmark6, then exact benchmark9 numeric updates",
            &[unlogged_benchmark2.as_str(), benchmark6.as_str()],
            &benchmark9,
            25_000,
        )?,
        run_indexed_update_diagnostic_case(
            "text_unlogged_indexed",
            "PGlite benchmark2 rewritten to UNLOGGED + benchmark6, then exact benchmark10 text updates",
            &[unlogged_benchmark2.as_str(), benchmark6.as_str()],
            &benchmark10,
            25_000,
        )?,
        run_indexed_update_diagnostic_case(
            "text_after_numeric_indexed",
            "PGlite benchmark2 + benchmark6 + exact benchmark9 numeric updates, then exact benchmark10 text updates",
            &[
                benchmark2.as_str(),
                benchmark6.as_str(),
                benchmark9.as_str(),
            ],
            &benchmark10,
            25_000,
        )?,
        run_indexed_update_diagnostic_case(
            "text_after_numeric_vacuumed",
            "PGlite benchmark2 + benchmark6 + exact benchmark9 numeric updates + VACUUM t2, then exact benchmark10 text updates",
            &[
                benchmark2.as_str(),
                benchmark6.as_str(),
                benchmark9.as_str(),
                "VACUUM t2;\n",
            ],
            &benchmark10,
            25_000,
        )?,
        run_indexed_update_diagnostic_case(
            "text_after_numeric_vacuum_full",
            "PGlite benchmark2 + benchmark6 + exact benchmark9 numeric updates + VACUUM FULL t2, then exact benchmark10 text updates",
            &[
                benchmark2.as_str(),
                benchmark6.as_str(),
                benchmark9.as_str(),
                "VACUUM FULL t2;\n",
            ],
            &benchmark10,
            25_000,
        )?,
        run_indexed_update_diagnostic_case(
            "set_based_numeric_indexed",
            "PGlite benchmark2 + benchmark6, then one set-based numeric update that changes every row",
            &[benchmark2.as_str(), benchmark6.as_str()],
            "BEGIN;\nUPDATE t2 SET b = b + 1;\nCOMMIT;\n",
            1,
        )?,
        run_indexed_update_diagnostic_case(
            "set_based_text_indexed",
            "PGlite benchmark2 + benchmark6, then one set-based text update that changes every row",
            &[benchmark2.as_str(), benchmark6.as_str()],
            "BEGIN;\nUPDATE t2 SET c = c || ' updated';\nCOMMIT;\n",
            1,
        )?,
    ];

    let report = IndexedUpdateDiagnosticReport {
        source_model: "Exact PGlite benchmark SQL files from assets/checkouts/pglite/packages/benchmark/src plus controlled variants.",
        measurement_model: "Each case opens a fresh temporary database, runs setup outside the measured section, then records the measured update SQL and internal Rust/WASIX phase timings.",
        cases,
    };
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

fn perf_diagnose_speed_hotspots() -> Result<()> {
    perf_diagnose_speed_ids(&["9", "10", "11", "14"])
}

fn perf_diagnose_speed_cases(args: &[String]) -> Result<()> {
    let mut ids: Option<Vec<String>> = None;
    for arg in args {
        if let Some(raw_ids) = arg.strip_prefix("--ids=") {
            let parsed = raw_ids
                .split(',')
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>();
            if parsed.is_empty() {
                bail!("--ids must contain at least one speed benchmark id");
            }
            ids = Some(parsed);
        } else {
            bail!("unknown perf diagnose-speed-cases flag: {arg}");
        }
    }

    let cases = speed_cases(1.0, SpeedSqlSource::PgliteVendored)?;
    let selected_ids = match ids {
        Some(ids) => ids,
        None => cases.iter().map(|case| case.id.to_owned()).collect(),
    };
    let selected_refs = selected_ids.iter().map(String::as_str).collect::<Vec<_>>();
    perf_diagnose_speed_ids(&selected_refs)
}

fn perf_diagnose_speed_ids(ids: &[&str]) -> Result<()> {
    Pglite::preload()?;
    let cases = speed_cases(1.0, SpeedSqlSource::PgliteVendored)?;
    let mut diagnostics = Vec::new();
    for id in ids {
        diagnostics.push(run_speed_hotspot_diagnostic_case(&cases, id)?);
    }

    let report = SpeedHotspotDiagnosticReport {
        source_model: "Exact PGlite benchmark SQL files from assets/checkouts/pglite/packages/benchmark/src.",
        measurement_model: "Each case opens a fresh temporary database, runs all earlier PGlite speed tests outside the measured section, then records the selected speed-test SQL, FS trace, and internal Rust/WASIX phase timings.",
        cases: diagnostics,
    };
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

fn perf_diagnose_buffer_cache() -> Result<()> {
    Pglite::preload()?;
    let cases = speed_cases(1.0, SpeedSqlSource::PgliteVendored)?;
    let diagnostics = vec![
        run_buffer_cache_diagnostic_case(
            &cases,
            "11",
            &[
                "BEGIN",
                "INSERT INTO t1 SELECT b,a,c FROM t2",
                "INSERT INTO t2 SELECT b,a,c FROM t1",
                "COMMIT",
            ],
        )?,
        run_buffer_cache_diagnostic_case(&cases, "14", &["INSERT INTO t2 SELECT * FROM t1"])?,
    ];

    let report = BufferCacheDiagnosticReport {
        source_model: "Exact PGlite benchmark SQL files from assets/checkouts/pglite/packages/benchmark/src.",
        measurement_model: "Each case opens a fresh temporary database, runs all earlier PGlite speed tests outside the measured section, then executes EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) for the target data-moving statements.",
        cases: diagnostics,
    };
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

fn run_buffer_cache_diagnostic_case(
    cases: &[SpeedCase],
    id: &str,
    statements: &[&str],
) -> Result<BufferCacheDiagnosticCase> {
    let target_index = cases
        .iter()
        .position(|case| case.id == id)
        .ok_or_else(|| anyhow!("unknown speed hotspot case {id}"))?;
    let target = &cases[target_index];

    let mut db = Pglite::builder()
        .temporary()
        .open()
        .with_context(|| format!("open buffer-cache diagnostic database for {}", target.id))?;

    let setup_started = Instant::now();
    for setup_case in &cases[..target_index] {
        db.exec(&setup_case.sql, None)
            .with_context(|| format!("run buffer-cache setup case {}", setup_case.id))?;
    }
    let setup_micros = setup_started.elapsed().as_micros();

    let settings = exec_rows_json(
        &mut db,
        "SELECT current_setting('shared_buffers') AS shared_buffers, current_setting('fsync') AS fsync, current_setting('synchronous_commit') AS synchronous_commit, current_setting('wal_buffers') AS wal_buffers, current_setting('work_mem') AS work_mem",
    )?;
    let relation_sizes = exec_rows_json(
        &mut db,
        "SELECT relname, pg_relation_size(oid)::bigint AS bytes FROM pg_class WHERE relname IN ('t1', 't2', 'i2a', 'i2b') ORDER BY relname",
    )?;

    let mut explained = Vec::new();
    for statement in statements {
        if matches!(*statement, "BEGIN" | "COMMIT") {
            let (result, phases) = capture_phase_timings(|| {
                let started = Instant::now();
                let result = db.exec(statement, None);
                (result, started.elapsed())
            });
            let (result, elapsed) = result;
            result.with_context(|| format!("run transaction control statement {statement}"))?;
            explained.push(BufferCacheDiagnosticStatement {
                sql: (*statement).to_owned(),
                elapsed_micros: elapsed.as_micros(),
                explain_rows: serde_json::Value::Null,
                fs_trace: serde_json::Value::Null,
                phases,
            });
            continue;
        }

        reset_fs_trace();
        let explain_sql = format!("EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) {statement}");
        let (result, phases) = capture_phase_timings(|| {
            let started = Instant::now();
            let result = db.exec(&explain_sql, None);
            (result, started.elapsed())
        });
        let (result, elapsed) = result;
        let result = result.with_context(|| format!("run buffer-cache explain for {statement}"))?;
        let fs_trace = serde_json::to_value(fs_trace_snapshot())?;
        explained.push(BufferCacheDiagnosticStatement {
            sql: (*statement).to_owned(),
            elapsed_micros: elapsed.as_micros(),
            explain_rows: results_to_json(result),
            fs_trace,
            phases,
        });
    }

    db.close()
        .with_context(|| format!("close buffer-cache diagnostic database for {}", target.id))?;

    Ok(BufferCacheDiagnosticCase {
        id: target.id.to_owned(),
        label: target.label.clone(),
        setup_micros,
        settings,
        relation_sizes,
        statements: explained,
    })
}

fn exec_rows_json(db: &mut Pglite, sql: &str) -> Result<serde_json::Value> {
    let results = db.exec(sql, None)?;
    Ok(results_to_json(results))
}

fn results_to_json(results: Vec<pglite_oxide::Results>) -> serde_json::Value {
    serde_json::Value::Array(
        results
            .into_iter()
            .map(|result| {
                serde_json::json!({
                    "fields": result
                        .fields
                        .into_iter()
                        .map(|field| {
                            serde_json::json!({
                                "name": field.name,
                                "dataTypeId": field.data_type_id,
                            })
                        })
                        .collect::<Vec<_>>(),
                    "rows": result.rows,
                    "affectedRows": result.affected_rows,
                })
            })
            .collect(),
    )
}

fn run_speed_hotspot_diagnostic_case(
    cases: &[SpeedCase],
    id: &str,
) -> Result<SpeedHotspotDiagnosticCase> {
    let target_index = cases
        .iter()
        .position(|case| case.id == id)
        .ok_or_else(|| anyhow!("unknown speed hotspot case {id}"))?;
    let target = &cases[target_index];

    let mut db = Pglite::builder()
        .temporary()
        .open()
        .with_context(|| format!("open speed hotspot diagnostic database for {}", target.id))?;

    let setup_started = Instant::now();
    for setup_case in &cases[..target_index] {
        db.exec(&setup_case.sql, None)
            .with_context(|| format!("run speed hotspot setup case {}", setup_case.id))?;
    }
    let setup_micros = setup_started.elapsed().as_micros();

    reset_fs_trace();
    let (result, phases) = capture_phase_timings(|| {
        let started = Instant::now();
        let result = db.exec(&target.sql, None);
        (result, started.elapsed())
    });
    let (result, elapsed) = result;
    result.with_context(|| format!("run speed hotspot measured case {}", target.id))?;
    let fs_trace = serde_json::to_value(fs_trace_snapshot())?;
    db.close()
        .with_context(|| format!("close speed hotspot diagnostic database for {}", target.id))?;

    Ok(SpeedHotspotDiagnosticCase {
        id: target.id.to_owned(),
        label: target.label.clone(),
        setup_micros,
        elapsed_micros: elapsed.as_micros(),
        operation_count: target.operation_count,
        fs_trace,
        phases,
    })
}

fn read_pglite_benchmark_sql(id: &str) -> Result<String> {
    let path = Path::new(PGLITE_BENCHMARK_SQL_DIR).join(format!("benchmark{id}.sql"));
    fs::read_to_string(&path)
        .with_context(|| format!("read PGlite benchmark SQL {}", path.display()))
}

fn run_indexed_update_diagnostic_case(
    name: &'static str,
    description: &'static str,
    setup_sql: &[&str],
    measured_sql: &str,
    operation_count: usize,
) -> Result<IndexedUpdateDiagnosticCase> {
    let mut db = Pglite::builder()
        .temporary()
        .open()
        .with_context(|| format!("open diagnostic database for {name}"))?;

    let setup_started = Instant::now();
    for sql in setup_sql {
        db.exec(sql, None)
            .with_context(|| format!("run diagnostic setup for {name}"))?;
    }
    let setup_micros = setup_started.elapsed().as_micros();
    let stats_before = indexed_update_stats(&mut db)
        .with_context(|| format!("collect diagnostic pre-stats for {name}"))?;

    reset_fs_trace();
    let (result, phases) = capture_phase_timings(|| {
        let started = Instant::now();
        let result = db.exec(measured_sql, None);
        (result, started.elapsed())
    });
    let (result, elapsed) = result;
    result.with_context(|| format!("run diagnostic measured SQL for {name}"))?;
    let fs_trace = serde_json::to_value(fs_trace_snapshot())?;
    let stats_after = indexed_update_stats(&mut db)
        .with_context(|| format!("collect diagnostic post-stats for {name}"))?;
    db.close()
        .with_context(|| format!("close diagnostic database for {name}"))?;

    Ok(IndexedUpdateDiagnosticCase {
        name,
        description,
        setup_micros,
        elapsed_micros: elapsed.as_micros(),
        operation_count,
        stats_before,
        stats_after,
        fs_trace,
        phases,
    })
}

fn indexed_update_stats(db: &mut Pglite) -> Result<serde_json::Value> {
    let result = db.query(
        "SELECT \
             pg_relation_size('t2'::regclass)::text AS t2_size, \
             pg_relation_size('i2a'::regclass)::text AS i2a_size, \
             coalesce(pg_relation_size(to_regclass('i2b')), 0)::text AS i2b_size, \
             coalesce((SELECT n_tup_upd FROM pg_stat_user_tables WHERE relname = 't2'), 0)::text AS n_tup_upd, \
             coalesce((SELECT n_tup_hot_upd FROM pg_stat_user_tables WHERE relname = 't2'), 0)::text AS n_tup_hot_upd, \
             coalesce((SELECT n_dead_tup FROM pg_stat_user_tables WHERE relname = 't2'), 0)::text AS n_dead_tup",
        &[],
        None,
    )?;
    Ok(result
        .rows
        .into_iter()
        .next()
        .unwrap_or(serde_json::Value::Null))
}

struct RttCase {
    id: &'static str,
    label: &'static str,
    sql: String,
}

struct SpeedCase {
    id: &'static str,
    label: String,
    sql: String,
    operation_count: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SpeedSqlSource {
    Generated,
    PgliteVendored,
}

impl SpeedSqlSource {
    fn source_model(self) -> &'static str {
        match self {
            SpeedSqlSource::Generated => {
                "Mirrors the two PGlite benchmark families documented at https://pglite.dev/benchmarks: trimmed-average CRUD round-trip microbenchmarks and a SQLite speedtest-style SQL suite. The speed suite is generated locally instead of vendoring PGlite's generated SQL files."
            }
            SpeedSqlSource::PgliteVendored => {
                "Mirrors the two PGlite benchmark families documented at https://pglite.dev/benchmarks: trimmed-average CRUD round-trip microbenchmarks and the exact SQL files from assets/checkouts/pglite/packages/benchmark/src."
            }
        }
    }
}

fn run_rtt_direct_benchmark(iterations: usize) -> Result<BenchmarkRun> {
    let open_started = Instant::now();
    let mut db = Pglite::builder().temporary().open()?;
    let open_micros = open_started.elapsed().as_micros();

    let setup_started = Instant::now();
    db.exec(rtt_setup_sql(), None)?;
    let setup_micros = setup_started.elapsed().as_micros();

    let mut tests = Vec::new();
    for case in rtt_cases() {
        tests.push(run_rtt_case(iterations, &case, |sql| {
            db.exec(sql, None)?;
            Ok(())
        })?);
    }
    db.close()?;

    Ok(BenchmarkRun {
        suite: "rtt",
        mode: "direct",
        description: "PGlite direct Rust API, matching PGlite's in-process exec-style benchmark shape.",
        open_micros,
        connect_micros: None,
        setup_micros,
        tests,
    })
}

fn run_rtt_server_sqlx_benchmark(iterations: usize) -> Result<BenchmarkRun> {
    let open_started = Instant::now();
    let server = benchmark_pglite_server()?;
    let open_micros = open_started.elapsed().as_micros();
    let uri = server.database_url();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("create benchmark Tokio runtime")?;

    let (connect_micros, setup_micros, tests) = runtime.block_on(async {
        let connect_started = Instant::now();
        let mut conn = sqlx::PgConnection::connect(&uri)
            .await
            .context("connect SQLx benchmark client")?;
        let connect_micros = connect_started.elapsed().as_micros();

        let setup_started = Instant::now();
        conn.execute(rtt_setup_sql())
            .await
            .context("execute RTT setup over SQLx")?;
        let setup_micros = setup_started.elapsed().as_micros();

        let mut tests = Vec::new();
        for case in rtt_cases() {
            let mut samples = Vec::with_capacity(iterations);
            for _ in 0..iterations {
                let started = Instant::now();
                conn.execute(case.sql.as_str())
                    .await
                    .with_context(|| format!("execute RTT benchmark {} over SQLx", case.id))?;
                samples.push(started.elapsed().as_micros());
            }
            tests.push(samples_result(
                case.id,
                format!("Test {}: {}", case.id, case.label),
                "milliseconds",
                iterations,
                samples,
            ));
        }
        conn.close().await.context("close SQLx benchmark client")?;
        Ok::<_, anyhow::Error>((connect_micros, setup_micros, tests))
    })?;
    server.shutdown()?;

    Ok(BenchmarkRun {
        suite: "rtt",
        mode: "server_sqlx",
        description: "PGliteServer over the Postgres wire protocol using one long-lived SQLx connection.",
        open_micros,
        connect_micros: Some(connect_micros),
        setup_micros,
        tests,
    })
}

fn run_rtt_server_tokio_postgres_simple_benchmark(iterations: usize) -> Result<BenchmarkRun> {
    let open_started = Instant::now();
    let server = benchmark_pglite_server()?;
    let open_micros = open_started.elapsed().as_micros();
    let uri = server.database_url();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("create tokio-postgres simple RTT runtime")?;

    let (connect_micros, setup_micros, tests) = runtime.block_on(async {
        let connect_started = Instant::now();
        let (client, connection) = tokio_postgres::connect(&uri, tokio_postgres::NoTls)
            .await
            .context("connect tokio-postgres simple RTT client")?;
        let connection_handle = tokio::spawn(connection);
        let connect_micros = connect_started.elapsed().as_micros();

        let setup_started = Instant::now();
        client
            .batch_execute(rtt_setup_sql())
            .await
            .context("execute RTT setup over tokio-postgres simple-query protocol")?;
        let setup_micros = setup_started.elapsed().as_micros();

        let mut tests = Vec::new();
        for case in rtt_cases() {
            let mut samples = Vec::with_capacity(iterations);
            for _ in 0..iterations {
                let started = Instant::now();
                client.batch_execute(&case.sql).await.with_context(|| {
                    format!(
                        "execute RTT benchmark {} over tokio-postgres simple-query protocol",
                        case.id
                    )
                })?;
                samples.push(started.elapsed().as_micros());
            }
            tests.push(samples_result(
                case.id,
                format!("Test {}: {}", case.id, case.label),
                "milliseconds",
                iterations,
                samples,
            ));
        }

        drop(client);
        connection_handle
            .await
            .context("join tokio-postgres simple RTT connection task")?
            .context("tokio-postgres simple RTT connection task")?;
        Ok::<_, anyhow::Error>((connect_micros, setup_micros, tests))
    })?;
    server.shutdown()?;

    Ok(BenchmarkRun {
        suite: "rtt",
        mode: "server_tokio_postgres_simple",
        description: "PGliteServer over the Postgres wire protocol using one long-lived tokio-postgres connection and the simple-query protocol without SQLx.",
        open_micros,
        connect_micros: Some(connect_micros),
        setup_micros,
        tests,
    })
}

fn run_speed_direct_benchmark(scale: f64, sql_source: SpeedSqlSource) -> Result<BenchmarkRun> {
    let open_started = Instant::now();
    let mut db = Pglite::builder().temporary().open()?;
    let open_micros = open_started.elapsed().as_micros();

    let mut tests = Vec::new();
    for case in speed_cases(scale, sql_source)? {
        let started = Instant::now();
        db.exec(&case.sql, None)
            .with_context(|| format!("execute speed benchmark {}", case.id))?;
        tests.push(single_sample_result(
            case.id,
            case.label,
            "seconds",
            case.operation_count,
            started.elapsed(),
        ));
    }
    db.close()?;

    Ok(BenchmarkRun {
        suite: "speed",
        mode: "direct",
        description: "Generated SQLite speedtest-style SQL suite through PGlite direct Rust API.",
        open_micros,
        connect_micros: None,
        setup_micros: 0,
        tests,
    })
}

fn run_speed_server_sqlx_benchmark(scale: f64, sql_source: SpeedSqlSource) -> Result<BenchmarkRun> {
    let open_started = Instant::now();
    let server = benchmark_pglite_server()?;
    let open_micros = open_started.elapsed().as_micros();
    let uri = server.database_url();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("create benchmark Tokio runtime")?;

    let (connect_micros, tests) = runtime.block_on(async {
        let connect_started = Instant::now();
        let mut conn = sqlx::PgConnection::connect(&uri)
            .await
            .context("connect SQLx speed benchmark client")?;
        let connect_micros = connect_started.elapsed().as_micros();

        let mut tests = Vec::new();
        for case in speed_cases(scale, sql_source)? {
            let started = Instant::now();
            conn.execute(case.sql.as_str())
                .await
                .with_context(|| format!("execute speed benchmark {} over SQLx", case.id))?;
            tests.push(single_sample_result(
                case.id,
                case.label,
                "seconds",
                case.operation_count,
                started.elapsed(),
            ));
        }
        conn.close()
            .await
            .context("close SQLx speed benchmark client")?;
        Ok::<_, anyhow::Error>((connect_micros, tests))
    })?;
    server.shutdown()?;

    Ok(BenchmarkRun {
        suite: "speed",
        mode: "server_sqlx",
        description: "Generated SQLite speedtest-style SQL suite through one SQLx connection to PgliteServer.",
        open_micros,
        connect_micros: Some(connect_micros),
        setup_micros: 0,
        tests,
    })
}

fn benchmark_pglite_server() -> Result<PgliteServer> {
    PgliteServer::builder()
        .temporary()
        .database("postgres")
        .start()
}

fn rtt_setup_sql() -> &'static str {
    "\
CREATE TABLE t1 (id SERIAL PRIMARY KEY NOT NULL, a INTEGER);
CREATE TABLE t2 (id SERIAL PRIMARY KEY NOT NULL, a TEXT);
"
}

fn rtt_cases() -> Vec<RttCase> {
    vec![
        RttCase {
            id: "1",
            label: "insert small row",
            sql: "INSERT INTO t1 (a) VALUES (1);".to_owned(),
        },
        RttCase {
            id: "2",
            label: "select small row",
            sql: "SELECT * FROM t1 WHERE id = 333;".to_owned(),
        },
        RttCase {
            id: "3",
            label: "update small row",
            sql: "UPDATE t1 SET a = 2 WHERE id = 666;".to_owned(),
        },
        RttCase {
            id: "4",
            label: "delete small row",
            sql: "DELETE FROM t1 WHERE id IN (SELECT id FROM t1 LIMIT 1);".to_owned(),
        },
        RttCase {
            id: "5",
            label: "insert 1kb row",
            sql: format!("INSERT INTO t2 (a) VALUES ('{}');", "a".repeat(1_000)),
        },
        RttCase {
            id: "6",
            label: "select 1kb row",
            sql: "SELECT * FROM t2 WHERE id IN (SELECT id FROM t2 LIMIT 1);".to_owned(),
        },
        RttCase {
            id: "7",
            label: "update 1kb row",
            sql: format!("UPDATE t2 SET a = '{}' WHERE id = 1;", "a".repeat(1_000)),
        },
        RttCase {
            id: "8",
            label: "delete 1kb row",
            sql: "DELETE FROM t2 WHERE id IN (SELECT id FROM t2 LIMIT 1);".to_owned(),
        },
        RttCase {
            id: "9",
            label: "insert 10kb row",
            sql: format!("INSERT INTO t2 (a) VALUES ('{}');", "a".repeat(10_000)),
        },
        RttCase {
            id: "10",
            label: "select 10kb row",
            sql: "SELECT * FROM t2 WHERE id IN (SELECT id FROM t2 LIMIT 1);".to_owned(),
        },
        RttCase {
            id: "11",
            label: "update 10kb row",
            sql: format!("UPDATE t2 SET a = '{}' WHERE id = 1;", "a".repeat(10_000)),
        },
        RttCase {
            id: "12",
            label: "delete 10kb row",
            sql: "DELETE FROM t2 WHERE id IN (SELECT id FROM t2 LIMIT 1);".to_owned(),
        },
    ]
}

fn run_rtt_case(
    iterations: usize,
    case: &RttCase,
    mut execute: impl FnMut(&str) -> Result<()>,
) -> Result<BenchmarkTestResult> {
    let mut samples = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let started = Instant::now();
        execute(&case.sql).with_context(|| format!("execute RTT benchmark {}", case.id))?;
        samples.push(started.elapsed().as_micros());
    }
    Ok(samples_result(
        case.id,
        format!("Test {}: {}", case.id, case.label),
        "milliseconds",
        iterations,
        samples,
    ))
}

fn samples_result(
    id: &'static str,
    label: String,
    unit: &'static str,
    operation_count: usize,
    samples: Vec<u128>,
) -> BenchmarkTestResult {
    let elapsed_micros = samples.iter().sum();
    let mut sorted = samples;
    sorted.sort_unstable();
    let trim = if sorted.len() >= 10 {
        sorted.len() / 10
    } else {
        0
    };
    let trimmed = &sorted[trim..sorted.len() - trim];
    let average = trimmed.iter().sum::<u128>() as f64 / trimmed.len() as f64;
    let p50 = percentile_sorted(&sorted, 0.50);
    let p95 = percentile_sorted(&sorted, 0.95);
    BenchmarkTestResult {
        id,
        label,
        unit,
        operation_count,
        sample_count: sorted.len(),
        trimmed_sample_count: trimmed.len(),
        elapsed_micros,
        average_micros: Some(average),
        min_micros: sorted.first().copied(),
        p50_micros: p50,
        p95_micros: p95,
    }
}

fn single_sample_result(
    id: &'static str,
    label: String,
    unit: &'static str,
    operation_count: usize,
    elapsed: Duration,
) -> BenchmarkTestResult {
    let elapsed_micros = elapsed.as_micros();
    BenchmarkTestResult {
        id,
        label,
        unit,
        operation_count,
        sample_count: 1,
        trimmed_sample_count: 1,
        elapsed_micros,
        average_micros: None,
        min_micros: Some(elapsed_micros),
        p50_micros: Some(elapsed_micros),
        p95_micros: Some(elapsed_micros),
    }
}

fn percentile_sorted(sorted: &[u128], percentile: f64) -> Option<u128> {
    if sorted.is_empty() {
        return None;
    }
    let idx = ((sorted.len() - 1) as f64 * percentile).round() as usize;
    sorted.get(idx).copied()
}

fn speed_cases(scale: f64, sql_source: SpeedSqlSource) -> Result<Vec<SpeedCase>> {
    let insert_1k = scaled_count(1_000, scale);
    let insert_25k = scaled_count(25_000, scale);
    let select_100 = scaled_count(100, scale);
    let select_5k = scaled_count(5_000, scale);
    let update_1k = scaled_count(1_000, scale);
    let update_25k = scaled_count(25_000, scale);
    let refill_12k = scaled_count(12_000, scale);
    let mut cases = vec![
        SpeedCase {
            id: "1",
            label: format!("Test 1: {insert_1k} INSERTs"),
            sql: speed_create_and_insert("t1", insert_1k, false, false),
            operation_count: insert_1k,
        },
        SpeedCase {
            id: "2",
            label: format!("Test 2: {insert_25k} INSERTs in a transaction"),
            sql: speed_create_and_insert("t2", insert_25k, true, false),
            operation_count: insert_25k,
        },
        SpeedCase {
            id: "2.1",
            label: format!("Test 2.1: {insert_25k} INSERTs in single statement"),
            sql: speed_create_and_insert("t2_1", insert_25k, true, true),
            operation_count: insert_25k,
        },
        SpeedCase {
            id: "3",
            label: format!("Test 3: {insert_25k} INSERTs into an indexed table"),
            sql: speed_indexed_create_and_insert("t3", "i3", insert_25k, false),
            operation_count: insert_25k,
        },
        SpeedCase {
            id: "3.1",
            label: format!("Test 3.1: {insert_25k} INSERTs into an indexed table in single statement"),
            sql: speed_indexed_create_and_insert("t3_1", "i3_1", insert_25k, true),
            operation_count: insert_25k,
        },
        SpeedCase {
            id: "4",
            label: format!("Test 4: {select_100} SELECTs without an index"),
            sql: speed_select_range("t2", select_100, 100),
            operation_count: select_100,
        },
        SpeedCase {
            id: "5",
            label: format!("Test 5: {select_100} SELECTs on a string comparison"),
            sql: speed_select_like("t2", select_100),
            operation_count: select_100,
        },
        SpeedCase {
            id: "6",
            label: "Test 6: Creating indexes".to_owned(),
            sql: "CREATE INDEX i2a ON t2(a);\nCREATE INDEX i2b ON t2(b);\n".to_owned(),
            operation_count: 2,
        },
        SpeedCase {
            id: "7",
            label: format!("Test 7: {select_5k} SELECTs with an index"),
            sql: speed_select_range("t2", select_5k, 100),
            operation_count: select_5k,
        },
        SpeedCase {
            id: "8",
            label: format!("Test 8: {update_1k} UPDATEs without an index"),
            sql: speed_update_t1(update_1k),
            operation_count: update_1k,
        },
        SpeedCase {
            id: "9",
            label: format!("Test 9: {update_25k} UPDATEs with an index"),
            sql: speed_update_t2_numeric(update_25k),
            operation_count: update_25k,
        },
        SpeedCase {
            id: "10",
            label: format!("Test 10: {update_25k} text UPDATEs with an index"),
            sql: speed_update_t2_text(update_25k),
            operation_count: update_25k,
        },
        SpeedCase {
            id: "11",
            label: "Test 11: INSERTs from a SELECT".to_owned(),
            sql: "BEGIN;\nINSERT INTO t1 SELECT b,a,c FROM t2;\nINSERT INTO t2 SELECT b,a,c FROM t1;\nCOMMIT;\n".to_owned(),
            operation_count: 2,
        },
        SpeedCase {
            id: "12",
            label: "Test 12: DELETE without an index".to_owned(),
            sql: "DELETE FROM t2 WHERE c LIKE '%fifty%';\n".to_owned(),
            operation_count: 1,
        },
        SpeedCase {
            id: "13",
            label: "Test 13: DELETE with an index".to_owned(),
            sql: "DELETE FROM t2 WHERE a > 10 AND a < 20000;\n".to_owned(),
            operation_count: 1,
        },
        SpeedCase {
            id: "14",
            label: "Test 14: A big INSERT after a big DELETE".to_owned(),
            sql: "INSERT INTO t2 SELECT * FROM t1;\n".to_owned(),
            operation_count: 1,
        },
        SpeedCase {
            id: "15",
            label: format!("Test 15: A big DELETE followed by {refill_12k} small INSERTs"),
            sql: speed_delete_and_refill_t1(refill_12k),
            operation_count: refill_12k + 1,
        },
        SpeedCase {
            id: "16",
            label: "Test 16: DROP TABLE".to_owned(),
            sql: "DROP TABLE t1;\nDROP TABLE t2;\nDROP TABLE t3;\nDROP TABLE t2_1;\nDROP TABLE t3_1;\n".to_owned(),
            operation_count: 5,
        },
    ];

    if sql_source == SpeedSqlSource::PgliteVendored {
        let benchmark_dir = Path::new(PGLITE_BENCHMARK_SQL_DIR);
        for case in &mut cases {
            let path = benchmark_dir.join(format!("benchmark{}.sql", case.id));
            case.sql = fs::read_to_string(&path)
                .with_context(|| format!("read PGlite benchmark SQL {}", path.display()))?;
        }
    }

    Ok(cases)
}

fn scaled_count(base: usize, scale: f64) -> usize {
    ((base as f64 * scale).round() as usize).max(1)
}

fn speed_create_and_insert(
    table: &str,
    rows: usize,
    transaction: bool,
    single_statement: bool,
) -> String {
    let mut sql = String::new();
    if transaction {
        sql.push_str("BEGIN;\n");
    }
    sql.push_str(&format!(
        "CREATE TABLE {table}(a INTEGER, b INTEGER, c VARCHAR(100));\n"
    ));
    if single_statement {
        sql.push_str(&format!("INSERT INTO {table} VALUES\n"));
        for row in 1..=rows {
            if row > 1 {
                sql.push_str(",\n");
            }
            sql.push_str(&speed_row_values(row, row));
        }
        sql.push_str(";\n");
    } else {
        append_insert_rows(&mut sql, table, rows, 0);
    }
    if transaction {
        sql.push_str("COMMIT;\n");
    }
    sql
}

fn speed_indexed_create_and_insert(
    table: &str,
    index: &str,
    rows: usize,
    single_statement: bool,
) -> String {
    let mut sql = String::new();
    sql.push_str("BEGIN;\n");
    sql.push_str(&format!(
        "CREATE TABLE {table}(a INTEGER, b INTEGER, c VARCHAR(100));\n"
    ));
    sql.push_str(&format!("CREATE INDEX {index} ON {table}(c);\n"));
    if single_statement {
        sql.push_str(&format!("INSERT INTO {table} VALUES\n"));
        for row in 1..=rows {
            if row > 1 {
                sql.push_str(",\n");
            }
            sql.push_str(&speed_row_values(row, row + 17));
        }
        sql.push_str(";\n");
    } else {
        append_insert_rows(&mut sql, table, rows, 17);
    }
    sql.push_str("COMMIT;\n");
    sql
}

fn append_insert_rows(sql: &mut String, table: &str, rows: usize, seed_offset: usize) {
    for row in 1..=rows {
        sql.push_str(&format!(
            "INSERT INTO {table} VALUES{};\n",
            speed_row_values(row, row + seed_offset)
        ));
    }
}

fn speed_row_values(row: usize, seed: usize) -> String {
    let value = deterministic_benchmark_value(seed);
    format!("({row}, {value}, '{}')", synthetic_benchmark_text(value))
}

fn speed_select_range(table: &str, count: usize, width: usize) -> String {
    let mut sql = String::from("BEGIN;\n");
    for step in 0..count {
        let low = step * width;
        let high = low + width;
        sql.push_str(&format!(
            "SELECT count(*), avg(b) FROM {table} WHERE b >= {low} AND b < {high};\n"
        ));
    }
    sql.push_str("COMMIT;\n");
    sql
}

fn speed_select_like(table: &str, count: usize) -> String {
    const WORDS: &[&str] = &[
        "one",
        "two",
        "three",
        "four",
        "five",
        "six",
        "seven",
        "eight",
        "nine",
        "ten",
        "eleven",
        "twelve",
        "thirteen",
        "fourteen",
        "fifteen",
        "sixteen",
        "seventeen",
        "eighteen",
        "nineteen",
        "twenty",
    ];
    let mut sql = String::from("BEGIN;\n");
    for step in 0..count {
        let word = WORDS[step % WORDS.len()];
        sql.push_str(&format!(
            "SELECT count(*), avg(b) FROM {table} WHERE c LIKE '%{word}%';\n"
        ));
    }
    sql.push_str("COMMIT;\n");
    sql
}

fn speed_update_t1(count: usize) -> String {
    let mut sql = String::from("BEGIN;\n");
    for step in 0..count {
        let low = step * 10;
        let high = low + 10;
        sql.push_str(&format!(
            "UPDATE t1 SET b = b * 2 WHERE a >= {low} AND a < {high};\n"
        ));
    }
    sql.push_str("COMMIT;\n");
    sql
}

fn speed_update_t2_numeric(count: usize) -> String {
    let mut sql = String::from("BEGIN;\n");
    for row in 1..=count {
        let value = deterministic_benchmark_value(row + 101);
        sql.push_str(&format!("UPDATE t2 SET b = {value} WHERE a = {row};\n"));
    }
    sql.push_str("COMMIT;\n");
    sql
}

fn speed_update_t2_text(count: usize) -> String {
    let mut sql = String::from("BEGIN;\n");
    for row in 1..=count {
        let value = deterministic_benchmark_value(row + 202);
        sql.push_str(&format!(
            "UPDATE t2 SET c = '{}' WHERE a = {row};\n",
            synthetic_benchmark_text(value)
        ));
    }
    sql.push_str("COMMIT;\n");
    sql
}

fn speed_delete_and_refill_t1(count: usize) -> String {
    let mut sql = String::from("BEGIN;\nDELETE FROM t1;\n");
    append_insert_rows(&mut sql, "t1", count, 303);
    sql.push_str("COMMIT;\n");
    sql
}

fn deterministic_benchmark_value(seed: usize) -> usize {
    ((seed as u64)
        .wrapping_mul(1_103_515_245)
        .wrapping_add(12_345)
        % 100_000) as usize
}

fn synthetic_benchmark_text(value: usize) -> String {
    const WORDS: &[&str] = &[
        "zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten",
        "twenty", "thirty", "forty", "fifty", "sixty", "seventy", "eighty", "ninety",
    ];
    format!(
        "{} {} {} {}",
        WORDS[value % WORDS.len()],
        WORDS[(value / 7) % WORDS.len()],
        WORDS[(value / 97) % WORDS.len()],
        value
    )
}

#[allow(clippy::too_many_arguments)]
fn capture_operation(
    name: &'static str,
    description: &'static str,
    cache_state_before: impl Into<String>,
    process_state_before: &'static str,
    root_state: &'static str,
    query_state: &'static str,
    workload: &'static str,
    primary_latency_phase: &'static str,
    operation: impl FnOnce() -> Result<()>,
) -> Result<PerfOperation> {
    let started = Instant::now();
    let (result, phases) = capture_phase_timings(operation);
    let elapsed_micros = started.elapsed().as_micros();
    result?;
    let primary_latency_micros = phases
        .iter()
        .rev()
        .find(|phase| phase.name == primary_latency_phase)
        .map(|phase| phase.elapsed_micros)
        .unwrap_or(elapsed_micros);
    Ok(PerfOperation {
        name,
        description,
        cache_state_before: cache_state_before.into(),
        process_state_before,
        root_state,
        query_state,
        workload,
        primary_latency_phase,
        primary_latency_micros,
        elapsed_micros,
        correct: true,
        phases,
    })
}

fn pglite_oxide_cache_dir() -> Result<PathBuf> {
    ProjectDirs::from("dev", "pglite-oxide", "pglite-oxide")
        .context("could not resolve pglite-oxide cache directory")
        .map(|dirs| dirs.cache_dir().to_path_buf())
}

fn run_direct_select_one() -> Result<()> {
    let visible_started = Instant::now();
    let mut db = Pglite::builder().temporary().open()?;
    let result = db.query(
        "SELECT $1::int4 + 1 AS answer",
        &[serde_json::json!(41)],
        None,
    )?;
    ensure_json_int(&result.rows[0]["answer"], 42)?;
    record_phase_timing(
        "visible.direct_open_to_first_query",
        visible_started.elapsed(),
    );
    measure_phase("operation.close", || db.close())
}

fn run_direct_vector_query() -> Result<()> {
    let visible_started = Instant::now();
    let mut db = Pglite::builder()
        .temporary()
        .extension(extensions::VECTOR)
        .open()?;
    let result = db.query(
        "SELECT '[1,2,3]'::vector <-> '[1,2,4]'::vector AS distance",
        &[],
        None,
    )?;
    if result.rows[0]["distance"].as_f64().is_none() {
        bail!("extension-backed query did not return a float distance");
    }
    record_phase_timing(
        "visible.direct_open_to_first_query",
        visible_started.elapsed(),
    );
    measure_phase("operation.close", || db.close())
}

fn run_server_sqlx_select_one() -> Result<()> {
    let visible_started = Instant::now();
    let server = measure_phase("server.start", PgliteServer::temporary_tcp)?;
    let uri = server.database_url();
    let runtime = measure_phase("client.tokio_runtime_create", || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("create perf tokio runtime")
    })?;
    runtime.block_on(async move {
        let started = Instant::now();
        let mut conn = sqlx::PgConnection::connect(&uri)
            .await
            .context("connect SQLx to PGliteServer")?;
        record_phase_timing("client.sqlx_connect", started.elapsed());
        let started = Instant::now();
        let row = sqlx::query("SELECT $1::int4 + 1 AS answer")
            .bind(41_i32)
            .fetch_one(&mut conn)
            .await
            .context("run first SQLx query")?;
        record_phase_timing("client.sqlx_first_query", started.elapsed());
        let answer: i32 = row.try_get("answer").context("read SQLx answer")?;
        if answer != 42 {
            bail!("SQLx server query returned {answer}, expected 42");
        }
        conn.close().await.context("close SQLx connection")?;
        Ok::<_, anyhow::Error>(())
    })?;
    record_phase_timing(
        "visible.server_start_to_first_sqlx_query",
        visible_started.elapsed(),
    );
    measure_phase("operation.shutdown", || server.shutdown())
}

fn run_direct_repeated_selects(iterations: usize) -> Result<()> {
    let mut db = Pglite::builder().temporary().open()?;
    run_direct_scalar_query(&mut db, 41)?;
    let started = Instant::now();
    for value in 0..iterations {
        run_direct_scalar_query(&mut db, value as i32)?;
    }
    record_total_and_average(
        "warm.direct_repeated_scalar_queries.total",
        "warm.direct_repeated_scalar_queries.avg",
        started.elapsed(),
        iterations,
    );
    measure_phase("operation.close", || db.close())
}

fn run_direct_transaction_batch(iterations: usize) -> Result<()> {
    let mut db = Pglite::builder().temporary().open()?;
    run_direct_scalar_query(&mut db, 41)?;
    let started = Instant::now();
    db.transaction(|tx| {
        for value in 0..iterations {
            let result = tx.query(
                "SELECT $1::int4 + 1 AS answer",
                &[serde_json::json!(value as i32)],
                None,
            )?;
            ensure_json_int(&result.rows[0]["answer"], value as i64 + 1)?;
        }
        Ok(())
    })?;
    record_total_and_average(
        "warm.direct_transaction_batch.total",
        "warm.direct_transaction_batch.avg",
        started.elapsed(),
        iterations,
    );
    measure_phase("operation.close", || db.close())
}

fn run_direct_repeated_vector_queries(iterations: usize) -> Result<()> {
    let mut db = Pglite::builder()
        .temporary()
        .extension(extensions::VECTOR)
        .open()?;
    run_direct_vector_distance_query(&mut db)?;
    let started = Instant::now();
    for _ in 0..iterations {
        run_direct_vector_distance_query(&mut db)?;
    }
    record_total_and_average(
        "warm.direct_repeated_vector_queries.total",
        "warm.direct_repeated_vector_queries.avg",
        started.elapsed(),
        iterations,
    );
    measure_phase("operation.close", || db.close())
}

fn run_direct_scalar_query(db: &mut Pglite, value: i32) -> Result<()> {
    let result = db.query(
        "SELECT $1::int4 + 1 AS answer",
        &[serde_json::json!(value)],
        None,
    )?;
    ensure_json_int(&result.rows[0]["answer"], value as i64 + 1)
}

fn run_direct_vector_distance_query(db: &mut Pglite) -> Result<()> {
    let result = db.query(
        "SELECT '[1,2,3]'::vector <-> '[1,2,4]'::vector AS distance",
        &[],
        None,
    )?;
    if result.rows[0]["distance"].as_f64().is_none() {
        bail!("extension-backed query did not return a float distance");
    }
    Ok(())
}

fn run_server_sqlx_single_connection_repeated_queries(iterations: usize) -> Result<()> {
    let server = measure_phase("server.start", PgliteServer::temporary_tcp)?;
    let uri = server.database_url();
    let runtime = measure_phase("client.tokio_runtime_create", || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("create perf tokio runtime")
    })?;
    runtime.block_on(async move {
        let mut conn = sqlx::PgConnection::connect(&uri)
            .await
            .context("connect SQLx to PGliteServer")?;
        run_sqlx_scalar_query(&mut conn, 41).await?;
        let started = Instant::now();
        for value in 0..iterations {
            run_sqlx_scalar_query(&mut conn, value as i32).await?;
        }
        record_total_and_average(
            "warm.server_sqlx_single_connection_repeated_queries.total",
            "warm.server_sqlx_single_connection_repeated_queries.avg",
            started.elapsed(),
            iterations,
        );
        conn.close().await.context("close SQLx connection")?;
        Ok::<_, anyhow::Error>(())
    })?;
    measure_phase("operation.shutdown", || server.shutdown())
}

fn run_server_sqlx_repeated_connections(iterations: usize) -> Result<()> {
    let server = measure_phase("server.start", PgliteServer::temporary_tcp)?;
    let uri = server.database_url();
    let runtime = measure_phase("client.tokio_runtime_create", || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("create perf tokio runtime")
    })?;
    runtime.block_on(async move {
        let started = Instant::now();
        for value in 0..iterations {
            let mut conn = sqlx::PgConnection::connect(&uri)
                .await
                .context("connect SQLx to PGliteServer")?;
            run_sqlx_scalar_query(&mut conn, value as i32).await?;
            conn.close().await.context("close SQLx connection")?;
        }
        record_total_and_average(
            "warm.server_sqlx_repeated_connections.total",
            "warm.server_sqlx_repeated_connections.avg",
            started.elapsed(),
            iterations,
        );
        Ok::<_, anyhow::Error>(())
    })?;
    measure_phase("operation.shutdown", || server.shutdown())
}

fn run_server_sqlx_vector_single_connection_repeated_queries(iterations: usize) -> Result<()> {
    let server = measure_phase("server.start", || {
        PgliteServer::builder()
            .temporary()
            .extension(extensions::VECTOR)
            .start()
    })?;
    let uri = server.database_url();
    let runtime = measure_phase("client.tokio_runtime_create", || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("create perf tokio runtime")
    })?;
    runtime.block_on(async move {
        let mut conn = sqlx::PgConnection::connect(&uri)
            .await
            .context("connect SQLx to extension-enabled PGliteServer")?;
        run_sqlx_vector_query(&mut conn).await?;
        let started = Instant::now();
        for _ in 0..iterations {
            run_sqlx_vector_query(&mut conn).await?;
        }
        record_total_and_average(
            "warm.server_sqlx_vector_single_connection_repeated_queries.total",
            "warm.server_sqlx_vector_single_connection_repeated_queries.avg",
            started.elapsed(),
            iterations,
        );
        conn.close().await.context("close SQLx connection")?;
        Ok::<_, anyhow::Error>(())
    })?;
    measure_phase("operation.shutdown", || server.shutdown())
}

fn run_server_tokio_postgres_single_connection_repeated_queries(iterations: usize) -> Result<()> {
    let server = measure_phase("server.start", PgliteServer::temporary_tcp)?;
    let uri = server.database_url();
    let runtime = measure_phase("client.tokio_runtime_create", || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("create perf tokio runtime")
    })?;
    runtime.block_on(async move {
        let (client, connection) = tokio_postgres::connect(&uri, tokio_postgres::NoTls)
            .await
            .context("connect tokio-postgres to PGliteServer")?;
        let connection_handle = tokio::spawn(connection);
        run_tokio_postgres_scalar_query(&client, 41).await?;
        let started = Instant::now();
        for value in 0..iterations {
            run_tokio_postgres_scalar_query(&client, value as i32).await?;
        }
        record_total_and_average(
            "warm.server_tokio_postgres_single_connection_repeated_queries.total",
            "warm.server_tokio_postgres_single_connection_repeated_queries.avg",
            started.elapsed(),
            iterations,
        );
        drop(client);
        connection_handle
            .await
            .context("join tokio-postgres connection task")?
            .context("tokio-postgres connection task")?;
        Ok::<_, anyhow::Error>(())
    })?;
    measure_phase("operation.shutdown", || server.shutdown())
}

async fn run_sqlx_scalar_query(conn: &mut sqlx::PgConnection, value: i32) -> Result<()> {
    let row = sqlx::query("SELECT $1::int4 + 1 AS answer")
        .bind(value)
        .fetch_one(conn)
        .await
        .context("run SQLx scalar query")?;
    let answer: i32 = row.try_get("answer").context("read SQLx answer")?;
    ensure!(answer == value + 1, "SQLx query returned {answer}");
    Ok(())
}

async fn run_sqlx_vector_query(conn: &mut sqlx::PgConnection) -> Result<()> {
    let row = sqlx::query("SELECT '[1,2,3]'::vector <-> '[1,2,4]'::vector AS distance")
        .fetch_one(conn)
        .await
        .context("run SQLx vector query")?;
    let distance: f64 = row.try_get("distance").context("read vector distance")?;
    ensure!(distance == 1.0, "SQLx vector query returned {distance}");
    Ok(())
}

async fn run_tokio_postgres_scalar_query(
    client: &tokio_postgres::Client,
    value: i32,
) -> Result<()> {
    let row = client
        .query_one("SELECT $1::int4 + 1 AS answer", &[&value])
        .await
        .context("run tokio-postgres scalar query")?;
    let answer: i32 = row.get("answer");
    ensure!(
        answer == value + 1,
        "tokio-postgres query returned {answer}"
    );
    Ok(())
}

fn record_total_and_average(
    total_name: &'static str,
    average_name: &'static str,
    elapsed: Duration,
    iterations: usize,
) {
    record_phase_timing(total_name, elapsed);
    let average = elapsed.as_micros() / iterations as u128;
    record_phase_timing(
        average_name,
        Duration::from_micros(average.try_into().unwrap_or(u64::MAX)),
    );
}

fn unique_perf_root(name: &str) -> Result<PathBuf> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("read system clock for perf root")?
        .as_nanos();
    let root = env::temp_dir().join(format!("pglite-oxide-{name}-{}-{now}", std::process::id()));
    if root.exists() {
        fs::remove_dir_all(&root)
            .with_context(|| format!("remove stale perf root {}", root.display()))?;
    }
    fs::create_dir_all(&root).with_context(|| format!("create perf root {}", root.display()))?;
    Ok(root)
}

fn ensure_json_int(value: &serde_json::Value, expected: i64) -> Result<()> {
    let Some(actual) = value.as_i64() else {
        bail!("expected integer JSON value {expected}, got {value}");
    };
    if actual != expected {
        bail!("expected integer JSON value {expected}, got {actual}");
    }
    Ok(())
}

fn check_sources_manifest(strict_local: bool) -> Result<SourcesManifest> {
    let manifest = load_sources_manifest()?;
    validate_sources_manifest(&manifest)?;
    if strict_local {
        check_source_spine(&manifest, true, false)?;
    }
    println!("validated {} pinned asset sources", manifest.sources.len());
    Ok(manifest)
}

fn check_sources_manifest_for_asset_build(args: &[String]) -> Result<SourcesManifest> {
    let manifest = load_sources_manifest()?;
    validate_sources_manifest(&manifest)?;
    if args.iter().any(|arg| arg == "--fetch") {
        fetch_pinned_sources(&manifest)?;
    } else {
        check_source_spine(&manifest, true, false)?;
    }
    println!("validated {} pinned asset sources", manifest.sources.len());
    Ok(manifest)
}

fn fetch_pinned_sources(manifest: &SourcesManifest) -> Result<()> {
    for source in &manifest.sources {
        let Some(path) = source_checkout_path(source.name.as_str()) else {
            eprintln!(
                "warning: source '{}' has no configured checkout path; skipping fetch",
                source.name
            );
            continue;
        };
        if !path.exists() || !path.join(".git").exists() {
            init_source_checkout(source, path)?;
        }
        ensure_clean_checkout(source, path)?;
        ensure_source_remote(path, source)?;
        let mut fetch = Command::new("git");
        fetch
            .args(["fetch", "--depth", "1", "origin", &source.commit])
            .current_dir(path);
        run_command(&mut fetch).with_context(|| format!("fetch {}", source.name))?;
        let mut checkout = Command::new("git");
        checkout
            .args(["checkout", "-B", &source.branch, &source.commit])
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

fn init_source_checkout(source: &SourcePin, path: &Path) -> Result<()> {
    if path.exists() && !path.join(".git").exists() {
        if path.read_dir()?.next().is_none() {
            fs::remove_dir_all(path)
                .with_context(|| format!("remove empty source placeholder {}", path.display()))?;
        } else {
            bail!(
                "source checkout path {} exists but is not a git checkout; remove it or move it aside",
                path.display()
            );
        }
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }

    let mut command = Command::new("git");
    command.arg("init").arg(path);
    run_command(&mut command)
        .with_context(|| format!("initialize source checkout {}", path.display()))?;
    ensure_source_remote(path, source)
}

fn ensure_source_remote(path: &Path, source: &SourcePin) -> Result<()> {
    let remotes = command_output("git", &["remote"], path)
        .with_context(|| format!("read git remotes for {}", path.display()))?;
    let mut command = Command::new("git");
    if remotes.lines().any(|remote| remote == "origin") {
        command.args(["remote", "set-url", "origin", &source.url]);
    } else {
        command.args(["remote", "add", "origin", &source.url]);
    }
    command.current_dir(path);
    run_command(&mut command).with_context(|| {
        format!(
            "configure origin remote for {} at {}",
            source.name,
            path.display()
        )
    })
}

fn source_checkout_path(name: &str) -> Option<&'static Path> {
    match name {
        POSTGRES_PGLITE_SOURCE => Some(Path::new(POSTGRES_PGLITE_PATH)),
        PGLITE_BUILD_SOURCE => Some(Path::new(PGLITE_BUILD_PATH)),
        "pglite" => Some(Path::new("assets/checkouts/pglite")),
        "pgvector" => Some(Path::new(PGVECTOR_BUILD_DIR)),
        "pgtap" => Some(Path::new("assets/checkouts/pgtap")),
        "pg_ivm" => Some(Path::new("assets/checkouts/pg_ivm")),
        "pg_uuidv7" => Some(Path::new("assets/checkouts/pg_uuidv7")),
        "pg_hashids" => Some(Path::new("assets/checkouts/pg_hashids")),
        "age" => Some(Path::new("assets/checkouts/age")),
        "pg_textsearch" => Some(Path::new("assets/checkouts/pg_textsearch")),
        "postgis" => Some(Path::new("assets/checkouts/postgis")),
        "pglite-bindings" => Some(Path::new("assets/checkouts/pglite-bindings")),
        _ => None,
    }
}

fn ensure_clean_checkout(source: &SourcePin, path: &Path) -> Result<()> {
    if !path.exists() {
        bail!("source checkout is missing: {}", path.display());
    }
    let status = source_checkout_status_for_source(source.name.as_str(), path)
        .with_context(|| format!("read status for {}", path.display()))?;
    if !status.trim().is_empty() {
        bail!(
            "source checkout {} ({}) has uncommitted changes; preserve them before fetching pins",
            path.display(),
            source.name
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
    ensure_no_flag_contains(&manifest.build.main_flags, "asyncify", "build.main_flags")?;
    ensure_contains(
        &manifest.build.extension_flags,
        "-fwasm-exceptions",
        "build.extension_flags",
    )?;
    ensure_no_flag_contains(
        &manifest.build.extension_flags,
        "asyncify",
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
    let path = Path::new(GENERATED_ASSETS_DIR).join("manifest.json");
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

    let text = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
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

fn verify_committed_assets() -> Result<()> {
    check_source_free_repo()?;
    let manifest = load_sources_manifest()?;
    validate_sources_manifest(&manifest)?;
    check_no_legacy_runtime_shims()?;
    check_production_wasix_build_inputs()?;
    check_rust_startup_abi_boundary()?;
    check_or_write_asset_input_fingerprint(false)?;
    check_no_committed_portable_asset_blobs()?;
    check_no_committed_aot_artifacts()?;
    check_aot_crate_templates(&manifest)?;
    verify_generated_extension_surface_if_available()?;
    check_source_controlled_wasix_export_list()?;
    println!("source-controlled asset inputs and crate templates passed");
    Ok(())
}

fn check_source_free_repo() -> Result<()> {
    if Path::new(".gitmodules").exists() {
        bail!("tracked upstream source checkouts are not allowed: remove .gitmodules");
    }
    if is_release_staged_workspace() && !Path::new(".git").exists() {
        return Ok(());
    }
    for path in [
        "assets/checkouts",
        "assets/wasix-build/build",
        "assets/wasix-build/work",
        GENERATED_ASSETS_DIR,
        RELEASE_STAGE_DIR,
    ] {
        let tracked = command_output("git", &["ls-files", path], Path::new("."))?;
        if !tracked.trim().is_empty() {
            bail!(
                "{path} contains tracked generated/source checkout files:\n{}",
                tracked.trim()
            );
        }
    }
    Ok(())
}

fn is_release_staged_workspace() -> bool {
    env::var_os("PGLITE_OXIDE_RELEASE_STAGED").as_deref() == Some(std::ffi::OsStr::new("1"))
}

fn check_no_committed_portable_asset_blobs() -> Result<()> {
    let tracked = command_output(
        "git",
        &[
            "ls-files",
            ASSET_CRATE_PAYLOAD_DIR,
            LEGACY_STATIC_WASI_ARCHIVE,
            "assets/bin",
            "assets/prepopulated",
            "assets/extensions/*.tar.gz",
        ],
        Path::new("."),
    )?;
    if !tracked.trim().is_empty() {
        bail!(
            "portable WASIX asset payloads must be generated by CI/release and must not be committed:\n{}",
            tracked.trim()
        );
    }
    println!("committed repo contains no portable WASIX asset blobs");
    Ok(())
}

fn check_or_write_asset_input_fingerprint(write: bool) -> Result<()> {
    let fingerprint = asset_input_fingerprint()?;
    let path = Path::new(ASSET_INPUT_FINGERPRINT_PATH);
    if write {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }
        fs::write(path, format!("{fingerprint}\n"))
            .with_context(|| format!("write {}", path.display()))?;
        println!("wrote {}", path.display());
        return Ok(());
    }

    let expected = fs::read_to_string(path).with_context(|| {
        format!(
            "read {}; run `cargo run -p xtask -- assets input-fingerprint --write` after refreshing assets",
            path.display()
        )
    })?;
    ensure_eq(
        fingerprint.as_str(),
        expected.trim(),
        "committed asset input fingerprint",
    )
}

fn asset_input_fingerprint() -> Result<String> {
    let tracked = command_output(
        "git",
        &[
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "assets/sources.toml",
            "assets/extensions.promoted.toml",
            "assets/extensions.smoke.toml",
            "assets/wasix-build",
            "crates/assets/Cargo.toml",
            "crates/assets/build.rs",
            "crates/assets/src",
            "crates/aot",
            "xtask/src/main.rs",
            "xtask/src/extension_catalog.rs",
        ],
        Path::new("."),
    )?;
    let mut files = tracked
        .lines()
        .filter(|line| {
            Path::new(line).exists()
                && !line.starts_with("assets/wasix-build/build/")
                && !line.starts_with("assets/wasix-build/work/")
        })
        .map(str::to_owned)
        .collect::<Vec<_>>();
    files.sort();
    files.dedup();
    if files.is_empty() {
        bail!("no tracked asset input files found");
    }

    let mut hasher = Sha256::new();
    for file in files {
        let bytes = asset_input_fingerprint_bytes(&file)?;
        hasher.update(file.as_bytes());
        hasher.update([0]);
        hasher.update(sha256_bytes(&bytes).as_bytes());
        hasher.update([0]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn asset_input_fingerprint_bytes(file: &str) -> Result<Vec<u8>> {
    let bytes = fs::read(file).with_context(|| format!("read {file}"))?;
    if !is_internal_asset_package_manifest(file) {
        return Ok(bytes);
    }

    let text = String::from_utf8(bytes).with_context(|| format!("read {file} as UTF-8"))?;
    Ok(normalize_internal_asset_package_manifest(&text).into_bytes())
}

fn is_internal_asset_package_manifest(file: &str) -> bool {
    file == "crates/assets/Cargo.toml"
        || (file.starts_with("crates/aot/") && file.ends_with("/Cargo.toml"))
}

fn normalize_internal_asset_package_manifest(text: &str) -> String {
    let mut normalized = String::with_capacity(text.len());
    let mut in_package = false;

    for chunk in text.split_inclusive('\n') {
        let line = chunk.strip_suffix('\n').unwrap_or(chunk);
        let logical = line.strip_suffix('\r').unwrap_or(line);
        let trimmed = logical.trim();
        if trimmed.starts_with('[') {
            in_package = trimmed == "[package]";
        }

        if in_package && is_toml_key(logical, "version") {
            let indent_len = logical.len() - logical.trim_start().len();
            normalized.push_str(&logical[..indent_len]);
            normalized.push_str("version = \"<release-version>\"");
            if line.ends_with('\r') {
                normalized.push('\r');
            }
            if chunk.ends_with('\n') {
                normalized.push('\n');
            }
        } else {
            normalized.push_str(chunk);
        }
    }

    normalized
}

fn is_toml_key(line: &str, key: &str) -> bool {
    line.trim_start()
        .strip_prefix(key)
        .is_some_and(|rest| rest.trim_start().starts_with('='))
}

fn verify_asset_manifest_hashes() -> Result<()> {
    let manifest_path = Path::new(GENERATED_ASSETS_DIR).join("manifest.json");
    let text = fs::read_to_string(&manifest_path)
        .with_context(|| format!("read {}", manifest_path.display()))?;
    let manifest: AssetManifestOut =
        serde_json::from_str(&text).context("parse generated asset manifest")?;
    let base = Path::new(GENERATED_ASSETS_DIR);

    let runtime_archive = base.join(&manifest.runtime.archive);
    verify_file_sha256(
        &runtime_archive,
        &manifest.runtime.sha256,
        "runtime archive",
    )?;
    let runtime_module = archive_entry_bytes(&runtime_archive, "pglite/bin/pglite")?;
    ensure_eq(
        &sha256_bytes(&runtime_module),
        &manifest.runtime.module_sha256,
        "runtime module sha256",
    )?;
    for module in &manifest.runtime_support {
        let bytes = archive_entry_bytes(&runtime_archive, &format!("pglite/{}", module.path))?;
        ensure_eq(
            &sha256_bytes(&bytes),
            &module.sha256,
            &format!("runtime support {} sha256", module.name),
        )?;
        ensure_eq(
            &sha256_bytes(&bytes),
            &module.module_sha256,
            &format!("runtime support {} module sha256", module.name),
        )?;
    }

    if let Some(pg_dump) = &manifest.pg_dump {
        verify_file_sha256(&base.join(&pg_dump.path), &pg_dump.sha256, "pg_dump wasm")?;
        ensure_eq(
            &pg_dump.sha256,
            &pg_dump.module_sha256,
            "pg_dump module sha256",
        )?;
    }
    if let Some(initdb) = &manifest.initdb {
        verify_file_sha256(&base.join(&initdb.path), &initdb.sha256, "initdb wasm")?;
        ensure_eq(
            &initdb.sha256,
            &initdb.module_sha256,
            "initdb module sha256",
        )?;
    }

    for extension in &manifest.extensions {
        let archive = base.join(&extension.archive);
        verify_file_sha256(
            &archive,
            &extension.sha256,
            &format!("extension {} archive", extension.sql_name),
        )?;
        if let Some(native_module) = &extension.native_module {
            let entry = format!("lib/postgresql/{native_module}");
            let bytes = archive_entry_bytes(&archive, &entry)?;
            ensure_eq(
                &sha256_bytes(&bytes),
                &extension.module_sha256,
                &format!("extension {} module sha256", extension.sql_name),
            )?;
        }
    }

    let pgdata_archive = base.join("prepopulated/pgdata-template.tar.zst");
    verify_pgdata_template_hash(&pgdata_archive)?;
    if let Some(template) = &manifest.pgdata_template {
        verify_file_sha256(
            &base.join(&template.archive),
            &template.sha256,
            "PGDATA template",
        )?;
        ensure_file(&base.join(&template.manifest))?;
        ensure_eq(
            &template.runtime_module_sha256,
            &manifest.runtime.module_sha256,
            "PGDATA template runtime module sha256",
        )?;
        if let Some(initdb) = &manifest.initdb {
            ensure_eq(
                &template.initdb_module_sha256,
                &initdb.module_sha256,
                "PGDATA template initdb module sha256",
            )?;
        }
    }

    if is_release_staged_workspace() {
        verify_root_asset_metadata(&manifest, &manifest.runtime.module_sha256)?;
        verify_file_sha256(
            &pgdata_archive,
            &cargo_metadata_value("pgdata-template-archive-sha256")?,
            "PGDATA template archive metadata",
        )?;
    }

    println!("generated asset hashes match manifests");
    Ok(())
}

fn verify_pgdata_template_hash(pgdata_archive: &Path) -> Result<()> {
    let manifest_path = Path::new(GENERATED_ASSETS_DIR).join("prepopulated/pgdata-template.json");
    ensure!(
        manifest_path.exists() && pgdata_archive.exists(),
        "generated assets must include the bundled PGDATA template required by the default runtime; expected both {} and {}",
        manifest_path.display(),
        pgdata_archive.display()
    );
    let text = fs::read_to_string(&manifest_path)
        .with_context(|| format!("read {}", manifest_path.display()))?;
    let manifest: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("parse {}", manifest_path.display()))?;
    let expected = manifest
        .get("archiveSha256")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow!("{} is missing archiveSha256", manifest_path.display()))?;
    verify_file_sha256(pgdata_archive, expected, "PGDATA template archive")?;
    Ok(())
}

fn verify_root_asset_metadata(
    manifest: &AssetManifestOut,
    runtime_module_sha256: &str,
) -> Result<()> {
    verify_metadata_value(
        "runtime-archive-sha256",
        &manifest.runtime.sha256,
        "runtime archive metadata",
    )?;
    verify_metadata_value(
        "pglite-wasix-sha256",
        runtime_module_sha256,
        "runtime module metadata",
    )?;
    if let Some(pg_dump) = &manifest.pg_dump {
        verify_metadata_value("pg-dump-wasix-sha256", &pg_dump.sha256, "pg_dump metadata")?;
    }
    if let Some(initdb) = &manifest.initdb {
        verify_metadata_value("initdb-wasix-sha256", &initdb.sha256, "initdb metadata")?;
    }
    Ok(())
}

fn verify_metadata_value(key: &str, expected: &str, field: &str) -> Result<()> {
    let actual = cargo_metadata_value(key)?;
    ensure_eq(&actual, expected, field)
}

fn cargo_metadata_value(key: &str) -> Result<String> {
    let text = fs::read_to_string("Cargo.toml").context("read Cargo.toml")?;
    let needle = format!("{key} = \"");
    let start = text
        .find(&needle)
        .ok_or_else(|| anyhow!("Cargo.toml metadata key '{key}' is missing"))?
        + needle.len();
    let end = text[start..]
        .find('"')
        .ok_or_else(|| anyhow!("Cargo.toml metadata key '{key}' is unterminated"))?;
    Ok(text[start..start + end].to_owned())
}

fn verify_file_sha256(path: &Path, expected: &str, field: &str) -> Result<()> {
    ensure_file(path)?;
    let actual = sha256_file(path)?;
    ensure_eq(&actual, expected, field)
}

fn archive_entry_bytes(archive_path: &Path, entry_name: &str) -> Result<Vec<u8>> {
    let file =
        fs::File::open(archive_path).with_context(|| format!("open {}", archive_path.display()))?;
    let decoder = zstd::stream::read::Decoder::new(file)
        .with_context(|| format!("create zstd decoder for {}", archive_path.display()))?;
    let mut archive = tar::Archive::new(decoder);
    for entry in archive
        .entries()
        .with_context(|| format!("read {}", archive_path.display()))?
    {
        let mut entry =
            entry.with_context(|| format!("read entry from {}", archive_path.display()))?;
        let path = entry
            .path()
            .with_context(|| format!("read path from {}", archive_path.display()))?
            .to_string_lossy()
            .trim_start_matches("./")
            .to_owned();
        if path == entry_name {
            let mut bytes = Vec::new();
            io::copy(&mut entry, &mut bytes)
                .with_context(|| format!("read {entry_name} from {}", archive_path.display()))?;
            return Ok(bytes);
        }
    }
    bail!(
        "{} is missing archive entry {entry_name}",
        archive_path.display()
    )
}

fn check_no_committed_aot_artifacts() -> Result<()> {
    let tracked = command_output("git", &["ls-files", "crates/aot"], Path::new("."))?;
    let committed_artifacts = tracked
        .lines()
        .filter(|path| path.contains("/artifacts/"))
        .collect::<Vec<_>>();
    if !committed_artifacts.is_empty() {
        bail!(
            "native AOT artifacts must be generated by CI and must not be committed:\n{}",
            committed_artifacts.join("\n")
        );
    }
    println!("committed repo contains no native AOT artifact blobs");
    Ok(())
}

fn check_aot_crate_templates(sources: &SourcesManifest) -> Result<()> {
    let expected = supported_aot_targets();
    for target in expected {
        let crate_dir = Path::new("crates/aot").join(target);
        ensure_file(&crate_dir.join("Cargo.toml"))?;
        ensure_file(&crate_dir.join("README.md"))?;
        ensure_file(&crate_dir.join("build.rs"))?;
        let lib = crate_dir.join("src/lib.rs");
        ensure_file(&lib)?;

        let cargo_toml = fs::read_to_string(crate_dir.join("Cargo.toml"))
            .with_context(|| format!("read {}/Cargo.toml", crate_dir.display()))?;
        if !cargo_toml.contains("\"build.rs\"") || !cargo_toml.contains("\"artifacts/**\"") {
            bail!(
                "{} must include build.rs and generated artifacts/** when CI materializes the AOT crate",
                crate_dir.join("Cargo.toml").display()
            );
        }

        let lib_text =
            fs::read_to_string(&lib).with_context(|| format!("read {}", lib.display()))?;
        for required in [
            "#![deny(unsafe_code)]",
            "include!(concat!(env!(\"OUT_DIR\")",
        ] {
            if !lib_text.contains(required) {
                bail!("{} is not a source-only AOT crate template", lib.display());
            }
        }
        if lib_text.contains("include_bytes!") || lib_text.contains("include_str!(\"../artifacts/")
        {
            bail!(
                "{} embeds generated AOT artifacts; generated artifacts belong only in CI/release workspaces",
                lib.display()
            );
        }
        let build_rs = fs::read_to_string(crate_dir.join("build.rs"))
            .with_context(|| format!("read {}/build.rs", crate_dir.display()))?;
        for required in [
            "PGLITE_OXIDE_GENERATED_AOT_DIR",
            "target/pglite-oxide/aot",
            "wasmer-version",
            sources.toolchain.wasmer.as_str(),
            "wasmer-wasix-version",
            sources.toolchain.wasmer_wasix.as_str(),
        ] {
            if !build_rs.contains(required) {
                bail!(
                    "{} build.rs is missing source-only AOT marker {required}",
                    crate_dir.display()
                );
            }
        }
    }
    println!("AOT crates are source-only templates for CI-generated release artifacts");
    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct AotTargetSpec {
    triple: &'static str,
    runner_os: &'static str,
    package: &'static str,
    llvm_url: &'static str,
}

#[derive(Debug, Serialize)]
struct AotCiMatrix {
    include: Vec<AotCiTarget>,
}

#[derive(Debug, Serialize)]
struct AotCiTarget {
    os: &'static str,
    target: &'static str,
    package: &'static str,
    artifact: String,
    llvm_url: &'static str,
}

fn aot_target_specs() -> &'static [AotTargetSpec] {
    &[
        AotTargetSpec {
            triple: "aarch64-apple-darwin",
            runner_os: "macos-15",
            package: "pglite-oxide-aot-aarch64-apple-darwin",
            llvm_url: "https://github.com/wasmerio/llvm-custom-builds/releases/download/22.x/llvm-darwin-aarch64.tar.xz",
        },
        AotTargetSpec {
            triple: "x86_64-unknown-linux-gnu",
            runner_os: "ubuntu-latest",
            package: "pglite-oxide-aot-x86_64-unknown-linux-gnu",
            llvm_url: "https://github.com/wasmerio/llvm-custom-builds/releases/download/22.x/llvm-linux-amd64.tar.xz",
        },
        AotTargetSpec {
            triple: "aarch64-unknown-linux-gnu",
            runner_os: "ubuntu-24.04-arm",
            package: "pglite-oxide-aot-aarch64-unknown-linux-gnu",
            llvm_url: "https://github.com/wasmerio/llvm-custom-builds/releases/download/22.x/llvm-linux-aarch64.tar.xz",
        },
        AotTargetSpec {
            triple: "x86_64-pc-windows-msvc",
            runner_os: "windows-latest",
            package: "pglite-oxide-aot-x86_64-pc-windows-msvc",
            llvm_url: "https://github.com/wasmerio/llvm-custom-builds/releases/download/22.x/llvm-windows-amd64.tar.xz",
        },
    ]
}

fn supported_aot_targets() -> Vec<&'static str> {
    aot_target_specs().iter().map(|spec| spec.triple).collect()
}

fn aot_artifact_name(target: &str) -> String {
    format!("pglite-oxide-aot-{target}")
}

fn portable_wasix_artifact_name() -> &'static str {
    "pglite-oxide-portable-wasix"
}

fn print_supported_aot_targets() -> Result<()> {
    for spec in aot_target_specs() {
        println!("{}", spec.triple);
    }
    Ok(())
}

fn print_internal_asset_packages() -> Result<()> {
    println!("pglite-oxide-assets");
    for spec in aot_target_specs() {
        println!("{}", spec.package);
    }
    Ok(())
}

fn print_ci_artifact_names() -> Result<()> {
    println!("{}", portable_wasix_artifact_name());
    for spec in aot_target_specs() {
        println!("{}", aot_artifact_name(spec.triple));
    }
    Ok(())
}

fn print_aot_ci_matrix(args: &[String]) -> Result<()> {
    let requested = value_after(args, "--target")
        .or_else(|| value_after(args, "--target-triple"))
        .unwrap_or("all");
    let github_output = args.iter().any(|arg| arg == "--github-output");
    let targets = aot_target_specs()
        .iter()
        .filter(|spec| requested == "all" || requested == spec.triple)
        .map(|spec| AotCiTarget {
            os: spec.runner_os,
            target: spec.triple,
            package: spec.package,
            artifact: aot_artifact_name(spec.triple),
            llvm_url: spec.llvm_url,
        })
        .collect::<Vec<_>>();
    ensure!(
        !targets.is_empty(),
        "unsupported native AOT target: {requested}"
    );
    let matrix = AotCiMatrix { include: targets };
    let json = serde_json::to_string(&matrix).context("serialize AOT CI matrix")?;
    if github_output {
        println!("matrix={json}");
    } else {
        println!("{}", serde_json::to_string_pretty(&matrix)?);
    }
    Ok(())
}

fn ensure_supported_aot_target(target: &str) -> Result<()> {
    if aot_target_specs().iter().any(|spec| spec.triple == target) {
        return Ok(());
    }
    bail!(
        "unsupported AOT target {target}; supported targets are {}",
        supported_aot_targets().join(", ")
    )
}

fn verify_generated_extension_surface() -> Result<()> {
    let manifest_path = Path::new(GENERATED_ASSETS_DIR).join("manifest.json");
    let manifest_text = fs::read_to_string(&manifest_path)
        .with_context(|| format!("read {}", manifest_path.display()))?;
    let manifest: AssetManifestOut =
        serde_json::from_str(&manifest_text).context("parse committed asset manifest")?;
    let catalog_text = fs::read_to_string("assets/generated/extensions.catalog.json")
        .context("read assets/generated/extensions.catalog.json")?;
    let catalog: serde_json::Value =
        serde_json::from_str(&catalog_text).context("parse generated extension catalog")?;
    let generated = fs::read_to_string("src/pglite/generated_extensions.rs")
        .context("read src/pglite/generated_extensions.rs")?;

    let mut promoted_constants = BTreeMap::new();
    for entry in catalog
        .get("extensions")
        .and_then(|value| value.as_array())
        .ok_or_else(|| anyhow!("extension catalog is missing extensions array"))?
    {
        let promoted = entry
            .pointer("/promotion/promoted")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        if !promoted {
            continue;
        }
        let sql_name = entry
            .get("sql-name")
            .and_then(|value| value.as_str())
            .ok_or_else(|| anyhow!("promoted extension is missing sql-name"))?;
        let rust_constant = entry
            .get("rust-constant")
            .and_then(|value| value.as_str())
            .ok_or_else(|| anyhow!("promoted extension {sql_name} is missing rust-constant"))?;
        promoted_constants.insert(sql_name.to_owned(), rust_constant.to_owned());
    }

    let manifest_sql_names = manifest
        .extensions
        .iter()
        .map(|extension| extension.sql_name.clone())
        .collect::<BTreeSet<_>>();
    let catalog_sql_names = promoted_constants.keys().cloned().collect::<BTreeSet<_>>();
    if manifest_sql_names != catalog_sql_names {
        bail!(
            "promoted extension catalog and asset manifest disagree: manifest-only={:?} catalog-only={:?}",
            manifest_sql_names
                .difference(&catalog_sql_names)
                .collect::<Vec<_>>(),
            catalog_sql_names
                .difference(&manifest_sql_names)
                .collect::<Vec<_>>()
        );
    }

    for extension in &manifest.extensions {
        let rust_constant = promoted_constants.get(&extension.sql_name).ok_or_else(|| {
            anyhow!(
                "extension {} missing from promoted catalog",
                extension.sql_name
            )
        })?;
        for (needle, description) in [
            (
                format!("pub const {rust_constant}: Extension ="),
                "public extension constant",
            ),
            (format!("    {rust_constant},"), "extensions::ALL entry"),
            (format!("{:?}", extension.sql_name), "extension SQL name"),
            (format!("{:?}", extension.archive), "extension archive path"),
        ] {
            if !generated.contains(&needle) {
                bail!("generated extension API is stale: missing {description} {needle}");
            }
        }
        for status in [
            &extension.smoke_status.direct,
            &extension.smoke_status.server,
            &extension.smoke_status.restart,
        ] {
            ensure_eq(
                status,
                "passed",
                &format!("extension {} smoke status", extension.sql_name),
            )?;
        }
    }
    println!("generated extension API matches asset manifest and catalog");
    Ok(())
}

fn verify_generated_extension_surface_if_available() -> Result<()> {
    let manifest_path = Path::new(GENERATED_ASSETS_DIR).join("manifest.json");
    if !manifest_path.exists() {
        eprintln!(
            "warning: generated asset manifest is unavailable at {}; skipping generated extension manifest parity in source-only verification",
            manifest_path.display()
        );
        return Ok(());
    }
    verify_generated_extension_surface()
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
            &[
                "\"pgl_initdb\"",
                "\"pgl_backend\"",
                "PostgresRecoverProtocolError",
            ][..],
        ),
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
        "assets/wasix-build/wasix_shim/pglite_wasix_initdb_shim_abi_test.c",
        "assets/wasix-build/wasix_shim/pglite_wasix_shim.c",
        "assets/wasix-build/analyze_pgl_stubs.sh",
        "assets/wasix-build/configure_wasix_dl.sh",
        "assets/wasix-build/docker_wasix_env.sh",
        "assets/wasix-build/profile_flags.sh",
        "assets/wasix-build/prepare_patched_source.sh",
        "assets/wasix-build/pg_config_wasix.sh",
        "assets/wasix-build/docker/Dockerfile",
        "assets/wasix-build/docker_pglite.sh",
        "assets/wasix-build/docker_runtime_support.sh",
        "assets/wasix-build/docker_pgxs_extensions.sh",
        "assets/wasix-build/docker_contrib_extensions.sh",
        "assets/wasix-build/docker_pgdump.sh",
        "assets/wasix-build/docker_initdb.sh",
        "assets/wasix-build/wasix_shim/pglite_wasix_initdb_shim.c",
    ] {
        if !Path::new(required).exists() {
            bail!("production WASIX build input is missing: {required}");
        }
    }

    let production_files = [
        "xtask/src/main.rs",
        "assets/wasix-build/analyze_pgl_stubs.sh",
        "assets/wasix-build/configure_wasix_dl.sh",
        "assets/wasix-build/docker_wasix_env.sh",
        "assets/wasix-build/profile_flags.sh",
        "assets/wasix-build/prepare_patched_source.sh",
        "assets/wasix-build/pg_config_wasix.sh",
        "assets/wasix-build/docker_pglite.sh",
        "assets/wasix-build/docker_runtime_support.sh",
        "assets/wasix-build/docker_pgxs_extensions.sh",
        "assets/wasix-build/docker_contrib_extensions.sh",
        "assets/wasix-build/docker_pgdump.sh",
        "assets/wasix-build/docker_initdb.sh",
        "assets/wasix-build/wasix_shim/pglite_wasix_initdb_shim.c",
    ];
    for path in production_files {
        let text = fs::read_to_string(path).with_context(|| format!("read {path}"))?;
        if path == "assets/wasix-build/configure_wasix_dl.sh"
            && text.contains("--disable-spinlocks")
        {
            bail!(
                "{path} disables PostgreSQL spinlocks; WASIX builds must use the toolchain atomics path"
            );
        }
    }
    ensure_file_contains_all(
        "assets/wasix-build/docker_wasix_env.sh",
        &[
            "WASIX_HOME:=/opt/wasixcc-home/.wasixcc",
            "ln -s \"$WASIX_HOME\" \"$HOME/.wasixcc\"",
            "export PATH=\"$WASIX_HOME/bin:$PATH\"",
        ],
    )?;
    for path in [
        "assets/wasix-build/docker_pglite.sh",
        "assets/wasix-build/docker_runtime_support.sh",
        "assets/wasix-build/docker_pgxs_extensions.sh",
        "assets/wasix-build/docker_contrib_extensions.sh",
        "assets/wasix-build/docker_pgdump.sh",
        "assets/wasix-build/analyze_pgl_stubs.sh",
    ] {
        ensure_file_contains_all(path, &["docker_wasix_env.sh"])?;
    }

    ensure_file_contains_all(
        "assets/wasix-build/profile_flags.sh",
        &[
            "release)",
            "-O2 -g0",
            "release-o3)",
            "-O3 -g0 -flto=thin",
            "-flto=thin",
            "release-os)",
            "-Os -g0",
            "release-oz)",
            "-Oz -g0",
            "--converge:--strip-debug:--strip-producers",
            "WASIXCC_RUN_WASM_OPT",
            "WASIXCC_WASM_OPT_FLAGS",
            "PGLITE_OXIDE_ALLOW_ASYNCIFY_EXPERIMENT",
            "PGLITE_OXIDE_WASIX_BACKEND_TIMING",
            "production WASIX artifacts require WebAssembly exceptions",
        ],
    )?;
    ensure_file_contains_all(
        "assets/wasix-build/configure_wasix_dl.sh",
        &[
            "profile_flags.sh",
            "PGLITE_OXIDE_PROFILE_CFLAGS",
            "-sWASM_EXCEPTIONS=yes",
            "-sPIC=yes",
            "-Dlongjmp=pgl_longjmp",
            "-Dsiglongjmp=pgl_siglongjmp",
            "-DPGLITE_WASIX_BACKEND_TIMING",
            "-sMODULE_KIND=dynamic-main",
            "-Wl,-shared",
            "LDFLAGS_EX=\"$MAIN_LDFLAGS$LDFLAGS_EXTRA\"",
            "LDFLAGS_SL=\"$SIDE_MODULE_LDFLAGS\"",
        ],
    )?;
    ensure_file_contains_all(
        WASIX_BRIDGE_PATH,
        &[
            "pgl_backend_timing_reset",
            "pgl_backend_timing_start",
            "pgl_backend_timing_end",
            "pgl_backend_timing_elapsed_us",
            "CLOCK_MONOTONIC",
            "#ifdef PGLITE_WASIX_BACKEND_TIMING",
            "pgl_set_force_host_error_recovery",
            "force_host_error_recovery",
            "Hosts without that support",
            "pgl_setPGliteActive",
            "pgl_longjmp",
            "pgl_siglongjmp",
            "memcmp(env, (void *) postgresmain_sigjmp_buf, sizeof(jmp_buf)) == 0",
            "pgl_run_atexit_funcs",
        ],
    )?;
    ensure_file_contains_all(
        WASIX_PATCH_PATH,
        &[
            "#if defined(PGLITE_WASIX_DL) && defined(PGLITE_WASIX_BACKEND_TIMING)",
            "PGL_BACKEND_TIMING_CREATE_SHARED_MEMORY",
            "PGL_BACKEND_TIMING_RELATION_CACHE_PHASE3",
            "PGL_BACKEND_TIMING_INITIALIZE_ACL",
            "PGL_BACKEND_TIMING_EXEC_SIMPLE_QUERY",
            "PGL_BACKEND_TIMING_EXEC_PORTAL_RUN",
            "PGLITE_HOST_EXPORT(\"pgl_startPGlite\")",
            "PGLITE_HOST_EXPORT(\"PostgresMainLongJmp\")",
        ],
    )?;
    ensure_file_contains_all(
        "assets/wasix-build/docker_pglite.sh",
        &[
            "PGLITE_OXIDE_BUILD_PROFILE",
            "PGLITE_OXIDE_WASIX_BACKEND_TIMING",
            ".pglite-oxide-build-profile",
            "pglite_oxide_wasix_profile_signature",
        ],
    )?;
    ensure_file_not_contains_any(
        "assets/wasix-build/configure_wasix_dl.sh",
        &["ASYNCIFY", "-sASYNCIFY"],
    )?;

    println!("production WASIX build input guard passed");
    Ok(())
}

fn check_rust_startup_abi_boundary() -> Result<()> {
    let path = Path::new("src/pglite/postgres_mod.rs");
    let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;

    for marker in [
        "struct PgliteLifecycleExports",
        "struct WasixProtocolExports",
        "fn ensure_integrated_pglite_contract",
        "fn record_backend_c_timings",
        "pgl_backend_timing_reset",
        "pgl_backend_timing_elapsed_us",
        "host_requires_process_exit_error_recovery",
        "pgl_set_force_host_error_recovery",
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
    for lifecycle_marker in [
        "wasi_start",
        "set_force_host_error_recovery",
        "set_active",
        "start_pglite",
    ] {
        if !lifecycle_block.contains(lifecycle_marker) {
            bail!(
                "{} must drive the integrated PGlite lifecycle; missing {lifecycle_marker:?}",
                path.display()
            );
        }
    }

    println!("Rust startup ABI boundary guard passed");
    Ok(())
}

fn check_canonical_asset_layout(strict: bool) -> Result<()> {
    let runtime_archive = Path::new(GENERATED_ASSETS_DIR).join("pglite.wasix.tar.zst");
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

    let runtime_entries = archive_entries(&runtime_archive)?;
    for required in [
        "pglite/bin/pglite",
        "pglite/bin/postgres",
        "pglite/bin/pg_dump",
        "pglite/bin/initdb",
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

    let extensions_dir = Path::new(GENERATED_ASSETS_DIR).join("extensions");
    if extensions_dir.exists() {
        for entry in fs::read_dir(&extensions_dir)
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
            "lib"
                | "lib/postgresql"
                | "share"
                | "share/postgresql"
                | "share/postgresql/extension"
                | "share/postgresql/tsearch_data"
        ) {
            continue;
        }
        if entry.starts_with("lib/postgresql/")
            || entry.starts_with("share/postgresql/extension/")
            || entry.starts_with("share/postgresql/tsearch_data/")
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
                    "src/backend/tcop/postgres.c",
                    "PGLITE_HOST_EXPORT(\"pgl_startPGlite\")",
                    "PGLITE_HOST_EXPORT(\"PostgresMainLongJmp\")",
                    "__attribute__((export_name(\"ProcessStartupPacket\"))) int",
                ],
            )?;
            let patch_text = fs::read_to_string(WASIX_PATCH_PATH)
                .with_context(|| format!("read {WASIX_PATCH_PATH}"))?;
            if patch_adds_marker(&patch_text, "ProcessStartupPacket: STUB") {
                bail!("WASIX patch must not add a stub ProcessStartupPacket");
            }
            ensure_file_contains_all(
                "src/pglite/postgres_mod.rs",
                &["PgliteLifecycleExports", "WasixProtocolExports"],
            )?;
            ensure_file_not_contains_any(
                "src/pglite/postgres_mod.rs",
                &[
                    "apply_direct_startup_gucs",
                    "pgl_apply_default_gucs",
                    "PostgresRecoverProtocolError",
                ],
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
        "stable-external-checkpointer" => {
            ensure_file_contains_all(
                WASIX_PATCH_PATH,
                &[
                    "src/backend/postmaster/checkpointer.c",
                    "RequestCheckpoint(int flags)",
                    "#ifndef __PGLITE__",
                    "if (!IsPostmasterEnvironment)",
                ],
            )?;
            ensure_file_contains_all(
                "tests/performance_smoke.rs",
                &["cached_extension_template_opens_without_startup_xlog_recovery"],
            )?;
            Ok(Some(
                "ported in-process checkpoint behavior into wasix-dl patch",
            ))
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
                Path::new(GENERATED_ASSETS_DIR).join("manifest.json"),
                &["wasix-dynamic-main"],
            )?;
            Ok(Some("WASIX dynamic-main/side-module memory contract"))
        }
        "stable-memory-stack" => {
            ensure_file_contains_all(
                "assets/wasix-build/configure_wasix_dl.sh",
                &["-sSTACK_SIZE=8MB", "-sINITIAL_MEMORY=128MB"],
            )?;
            Ok(Some(
                "WASIX build profile pins stack and initial memory sizing",
            ))
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
        "stable-initdb-single-no-exit" => {
            ensure_file_contains_all(
                "assets/wasix-build/configure_wasix_dl.sh",
                &[
                    "-Dexit=pgl_exit",
                    "-Dlongjmp=pgl_longjmp",
                    "-Dsiglongjmp=pgl_siglongjmp",
                ],
            )?;
            ensure_file_contains_all(
                "tests/runtime_smoke.rs",
                &[
                    "persistent_fresh_initdb_survives_restart_and_stale_state_files",
                    "persistent_fresh_initdb_recovers_interrupted_pgdata_without_marker",
                    "persistent_fresh_initdb_recovers_interrupted_pgdata_with_incomplete_markers",
                ],
            )?;
            Ok(Some(
                "WASIX bridge follows upstream PGlite single-user process-exit/longjmp lifecycle",
            ))
        }
        "stable-atexit-single-cleanup" => {
            ensure_file_contains_all(
                WASIX_BRIDGE_PATH,
                &["pgl_atexit", "pgl_run_atexit_funcs", "pgl_exit(int status)"],
            )?;
            Ok(Some(
                "WASIX bridge stores atexit handlers and lets Rust close run them explicitly",
            ))
        }
        "stable-postmaster-environment" => {
            ensure_file_contains_all(
                WASIX_PATCH_PATH,
                &["IsPostmasterEnvironment = true", "pgl_startPGlite"],
            )?;
            Ok(Some(
                "uses upstream PGlite pgl_startPGlite postmaster-environment setup",
            ))
        }
        "stable-timer-cleanup" => {
            ensure_file_contains_all(
                WASIX_BRIDGE_PATH,
                &[
                    "pgl_clear_interval_timer",
                    "setitimer(ITIMER_REAL",
                    "pgl_exit(int status)",
                ],
            )?;
            Ok(Some("WASIX process-exit bridge clears interval timers"))
        }
        _ => Ok(None),
    }
}

fn ensure_file_contains_all(path: impl AsRef<Path>, markers: &[&str]) -> Result<()> {
    let path = path.as_ref();
    let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let missing = markers
        .iter()
        .copied()
        .filter(|marker| !text.contains(marker))
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        bail!(
            "{} is missing required upstream replacement markers: {}",
            path.display(),
            missing.join(", ")
        );
    }
    Ok(())
}

fn ensure_file_not_contains_any(path: &str, markers: &[&str]) -> Result<()> {
    let text = fs::read_to_string(path).with_context(|| format!("read {path}"))?;
    let present = markers
        .iter()
        .copied()
        .filter(|marker| text.contains(marker))
        .collect::<Vec<_>>();
    if !present.is_empty() {
        bail!(
            "{path} contains production-excluded markers: {}",
            present.join(", ")
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

fn check_all_manifest_source_checkouts(
    manifest: &SourcesManifest,
    strict_local: bool,
) -> Result<()> {
    for source in &manifest.sources {
        let Some(path) = source_checkout_path(source.name.as_str()) else {
            if strict_local {
                bail!("source '{}' has no configured checkout path", source.name);
            }
            eprintln!(
                "warning: source '{}' has no configured checkout path",
                source.name
            );
            continue;
        };
        if !path.join(".git").exists() {
            if strict_local {
                bail!("missing local checkout {}", path.display());
            }
            eprintln!("warning: local checkout {} is missing", path.display());
            continue;
        }
        let head = command_output("git", &["rev-parse", "HEAD"], path)
            .with_context(|| format!("read HEAD for {}", path.display()))?;
        if head.trim() != source.commit {
            if strict_local {
                bail!(
                    "local {} checkout is at {}, expected {} from assets/sources.toml",
                    path.display(),
                    head.trim(),
                    source.commit
                );
            }
            eprintln!(
                "warning: local {} checkout is at {}, expected {}",
                path.display(),
                head.trim(),
                source.commit
            );
        }
        let branch = command_output("git", &["branch", "--show-current"], path)
            .unwrap_or_else(|_| String::from("<detached>"));
        if strict_local && branch.trim() != source.branch {
            bail!(
                "local {} checkout is on branch '{}', expected '{}'",
                path.display(),
                branch.trim(),
                source.branch
            );
        }
        let status = source_checkout_status_for_source(source.name.as_str(), path)
            .with_context(|| format!("read status for {}", path.display()))?;
        if !status.trim().is_empty() {
            if strict_local {
                bail!(
                    "local {} checkout ({}) has uncommitted changes; preserve them before strict asset builds",
                    path.display(),
                    source.name
                );
            }
            eprintln!(
                "warning: local {} checkout ({}) has uncommitted changes",
                path.display(),
                source.name
            );
        }
    }
    Ok(())
}

fn check_source_spine(
    manifest: &SourcesManifest,
    strict_local: bool,
    check_patch_applies: bool,
) -> Result<()> {
    let postgres = source_by_name(manifest, POSTGRES_PGLITE_SOURCE)?;
    let pglite_build = source_by_name(manifest, PGLITE_BUILD_SOURCE)?;
    check_source_free_repo()?;
    check_all_manifest_source_checkouts(manifest, strict_local)?;

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
        "src/backend/tcop/postgres.c",
        "src/backend/tcop/backend_startup.c",
        "__attribute__((export_name(\"ProcessStartupPacket\"))) int",
        "PGLITE_HOST_EXPORT(\"pgl_startPGlite\")",
        "PGLITE_HOST_EXPORT(\"PostgresMainLongJmp\")",
        "PGL_BACKEND_TIMING_INIT_POSTGRES",
        "PGL_BACKEND_TIMING_SHARED_MEMORY",
        "PGL_BACKEND_TIMING_EXEC_SIMPLE_QUERY",
        "wasm_dl_extension_imports_dir",
        "PGLITE_WASIX_DL",
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
        "pglite-wasm/",
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
        "Runtime link inputs requiring WASIX host ABI ownership",
        "Frontend tool inputs requiring frontend/common ownership",
        "do not by themselves justify adding symbols to the production WASIX bridge",
    ] {
        if !stub_analysis_text.contains(marker) {
            bail!(
                "{} must keep runtime pgl_stubs ownership separate from frontend tool symbols",
                stub_analysis.display()
            );
        }
    }
    check_wasix_bridge_abi_harness()?;
    check_wasix_initdb_shim_abi_harness()?;
    for script in [
        "assets/wasix-build/docker_pglite.sh",
        "assets/wasix-build/docker_runtime_support.sh",
        "assets/wasix-build/docker_pgxs_extensions.sh",
        "assets/wasix-build/docker_contrib_extensions.sh",
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
    let docker_pgxs = fs::read_to_string("assets/wasix-build/docker_pgxs_extensions.sh")
        .context("read assets/wasix-build/docker_pgxs_extensions.sh")?;
    if !docker_pgxs.contains(extension_catalog::build_plan_pgxs_path())
        || !docker_pgxs.contains("PG_CONFIG=/work/assets/wasix-build/pg_config_wasix.sh")
        || !docker_pgxs.contains("make -s -j\"$JOBS\"")
    {
        bail!("docker_pgxs_extensions.sh must build PGXS extensions from the generated plan");
    }
    let docker_contrib = fs::read_to_string("assets/wasix-build/docker_contrib_extensions.sh")
        .context("read assets/wasix-build/docker_contrib_extensions.sh")?;
    if !docker_contrib.contains(extension_catalog::build_plan_contrib_path())
        || !docker_contrib.contains("make -s -j\"$JOBS\" -C \"$BUILD_DIR/contrib/$contrib_dir\"")
    {
        bail!("docker_contrib_extensions.sh must build contrib extensions from the generated plan");
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

    let status = source_checkout_status_for_source(postgres.name.as_str(), checkout)
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
        let build_status =
            source_checkout_status_for_source(pglite_build.name.as_str(), pglite_build_checkout)
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

        ensure_file(&pglite_build_checkout.join("wasm-build/build-ext.sh"))?;
    }

    let required_upstream_markers = [
        ("build-pglite.sh", "-Dlongjmp=pgl_longjmp"),
        ("build-pglite.sh", "-Dsiglongjmp=pgl_siglongjmp"),
        ("build-pglite.sh", "-sSTACK_SIZE=8MB"),
        ("build-pglite.sh", "-sINITIAL_MEMORY=128MB"),
        ("pglite/src/pglitec/pglitec.c", "pgl_setPGliteActive"),
        ("pglite/src/pglitec/pglitec.c", "pgl_longjmp"),
        ("pglite/src/pglitec/pglitec.c", "pgl_run_atexit_funcs"),
        (
            "pglite/static/included.pglite.exports",
            "PostgresMainLongJmp",
        ),
        ("src/backend/tcop/postgres.c", "pgl_startPGlite"),
        ("src/backend/tcop/postgres.c", "PostgresMainLoopOnce"),
        ("src/backend/tcop/postgres.c", "PostgresMainLongJmp"),
        ("src/backend/tcop/backend_startup.c", "ProcessStartupPacket"),
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

fn source_checkout_status(path: &Path) -> Result<String> {
    command_output("git", &["status", "--porcelain"], path)
}

fn source_checkout_status_for_source(name: &str, path: &Path) -> Result<String> {
    if name == POSTGRES_PGLITE_SOURCE {
        return command_output(
            "git",
            &["status", "--porcelain", "--ignore-submodules=all"],
            path,
        );
    }
    source_checkout_status(path)
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
        .stdout(Stdio::null())
        .status()
        .with_context(|| format!("run {}", binary.display()))?;
    if !status.success() {
        bail!("WASIX bridge ABI harness failed with {status}");
    }
    println!("WASIX bridge ABI harness passed");
    Ok(())
}

#[cfg(unix)]
fn check_wasix_initdb_shim_abi_harness() -> Result<()> {
    let shim = Path::new("assets/wasix-build/wasix_shim/pglite_wasix_initdb_shim.c");
    let harness = Path::new("assets/wasix-build/wasix_shim/pglite_wasix_initdb_shim_abi_test.c");
    if !harness.exists() {
        bail!(
            "missing WASIX initdb shim ABI harness at {}",
            harness.display()
        );
    }

    let out_dir = Path::new("target/xtask");
    fs::create_dir_all(out_dir).with_context(|| format!("create {}", out_dir.display()))?;
    let binary = out_dir.join("pglite_wasix_initdb_shim_abi_test");
    let cc = env::var("CC").unwrap_or_else(|_| "cc".to_owned());
    let status = Command::new(&cc)
        .args(["-std=c11", "-Wall", "-Wextra"])
        .arg(shim)
        .arg(harness)
        .arg("-o")
        .arg(&binary)
        .status()
        .with_context(|| format!("compile {}", harness.display()))?;
    if !status.success() {
        bail!("failed to compile {}", harness.display());
    }

    let status = Command::new(&binary)
        .status()
        .with_context(|| format!("run {}", binary.display()))?;
    if !status.success() {
        bail!("WASIX initdb shim ABI harness failed");
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_wasix_initdb_shim_abi_harness() -> Result<()> {
    println!("skipping WASIX initdb shim ABI harness on non-Unix host");
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
    name: String,
    kind: String,
    path: PathBuf,
    aot_file: String,
}

impl BuildOutputs {
    fn discover() -> Result<Self> {
        let build_dir = PathBuf::from(WASIX_DOCKER_BUILD_DIR);
        let source_dir = PathBuf::from(WASIX_PATCHED_SOURCE_DIR);
        let package_stage = PathBuf::from(WASIX_BUILD_ROOT).join("build/package-stage");
        let mut modules = vec![
            BuildModuleOutput {
                name: "runtime:pglite".to_owned(),
                kind: "runtime".to_owned(),
                path: build_dir.join("src/backend/pglite"),
                aot_file: "pglite-llvm-opta.bin.zst".to_owned(),
            },
            BuildModuleOutput {
                name: "runtime-support:plpgsql".to_owned(),
                kind: "runtime-support".to_owned(),
                path: build_dir.join("src/pl/plpgsql/src/plpgsql.so"),
                aot_file: "plpgsql-llvm-opta.bin.zst".to_owned(),
            },
            BuildModuleOutput {
                name: "runtime-support:dict_snowball".to_owned(),
                kind: "runtime-support".to_owned(),
                path: build_dir.join("src/backend/snowball/dict_snowball.so"),
                aot_file: "dict_snowball-llvm-opta.bin.zst".to_owned(),
            },
            BuildModuleOutput {
                name: "tool:pg_dump".to_owned(),
                kind: "tool".to_owned(),
                path: build_dir.join("src/bin/pg_dump/pg_dump"),
                aot_file: "pg_dump-llvm-opta.bin.zst".to_owned(),
            },
            BuildModuleOutput {
                name: "tool:initdb".to_owned(),
                kind: "tool".to_owned(),
                path: build_dir.join("src/bin/initdb/initdb"),
                aot_file: "initdb-llvm-opta.bin.zst".to_owned(),
            },
        ];
        for extension in extension_catalog::promoted_build_specs()? {
            if extension.module_file.is_some() {
                modules.push(BuildModuleOutput {
                    name: format!("extension:{}", extension.sql_name),
                    kind: "extension".to_owned(),
                    path: extension_build_module_path(&build_dir, &extension)?,
                    aot_file: format!("{}-llvm-opta.bin.zst", extension_aot_file_stem(&extension)),
                });
            }
        }

        let outputs = Self {
            build_dir,
            source_dir,
            package_stage,
            modules,
        };
        outputs.ensure_required_files()?;
        Ok(outputs)
    }

    fn discover_for_aot() -> Result<Self> {
        if !Path::new(WASIX_PATCHED_SOURCE_DIR).exists() {
            return Self::from_packaged_assets();
        }
        Self::discover().or_else(|build_err| {
            eprintln!(
                "warning: transient WASIX build tree unavailable for AOT packaging: {build_err:#}"
            );
            Self::from_packaged_assets()
        })
    }

    fn from_packaged_assets() -> Result<Self> {
        let manifest = read_asset_manifest()?;
        let base = PathBuf::from("assets/wasix-build/build/aot-inputs");
        if base.exists() {
            fs::remove_dir_all(&base).with_context(|| format!("remove {}", base.display()))?;
        }
        fs::create_dir_all(&base).with_context(|| format!("create {}", base.display()))?;

        let assets_base = Path::new(GENERATED_ASSETS_DIR);
        let runtime_archive = assets_base.join(&manifest.runtime.archive);
        let runtime_path = base.join("runtime/pglite");
        write_bytes_file(
            &runtime_path,
            &archive_entry_bytes(&runtime_archive, "pglite/bin/pglite")?,
        )?;

        let mut modules = vec![BuildModuleOutput {
            name: "runtime:pglite".to_owned(),
            kind: "runtime".to_owned(),
            path: runtime_path,
            aot_file: "pglite-llvm-opta.bin.zst".to_owned(),
        }];

        for support in &manifest.runtime_support {
            let path = base.join("runtime-support").join(&support.name);
            write_bytes_file(
                &path,
                &archive_entry_bytes(&runtime_archive, &format!("pglite/{}", support.path))?,
            )?;
            modules.push(BuildModuleOutput {
                name: format!("runtime-support:{}", support.name),
                kind: "runtime-support".to_owned(),
                path,
                aot_file: format!("{}-llvm-opta.bin.zst", support.name),
            });
        }

        if let Some(pg_dump) = &manifest.pg_dump {
            let path = base.join("tools/pg_dump");
            copy_file(&assets_base.join(&pg_dump.path), &path)?;
            modules.push(BuildModuleOutput {
                name: "tool:pg_dump".to_owned(),
                kind: "tool".to_owned(),
                path,
                aot_file: "pg_dump-llvm-opta.bin.zst".to_owned(),
            });
        }
        if let Some(initdb) = &manifest.initdb {
            let path = base.join("tools/initdb");
            copy_file(&assets_base.join(&initdb.path), &path)?;
            modules.push(BuildModuleOutput {
                name: "tool:initdb".to_owned(),
                kind: "tool".to_owned(),
                path,
                aot_file: "initdb-llvm-opta.bin.zst".to_owned(),
            });
        }

        for extension in &manifest.extensions {
            let Some(native_module) = extension.native_module.as_deref() else {
                continue;
            };
            if extension.module_sha256.is_empty() {
                continue;
            }
            let entry = format!("lib/postgresql/{native_module}");
            let path = base
                .join("extensions")
                .join(&extension.sql_name)
                .join(native_module);
            write_bytes_file(
                &path,
                &archive_entry_bytes(&assets_base.join(&extension.archive), &entry)?,
            )?;
            modules.push(BuildModuleOutput {
                name: format!("extension:{}", extension.sql_name),
                kind: "extension".to_owned(),
                path,
                aot_file: format!("{}-llvm-opta.bin.zst", extension.sql_name.replace('/', "_")),
            });
        }

        Ok(Self {
            build_dir: base.clone(),
            source_dir: base.clone(),
            package_stage: base,
            modules,
        })
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
            build_profile: fs::read_to_string(self.build_dir.join(".pglite-oxide-build-profile"))
                .context("read WASIX build profile signature")?,
            modules: self
                .modules
                .iter()
                .map(|module| {
                    Ok(BuildModuleManifestOut {
                        name: module.name.clone(),
                        kind: module.kind.clone(),
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

fn write_bytes_file(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    fs::write(path, bytes).with_context(|| format!("write {}", path.display()))
}

fn extension_build_module_path(
    build_dir: &Path,
    extension: &extension_catalog::PromotedExtensionBuildSpec,
) -> Result<PathBuf> {
    let module_file = extension
        .module_file
        .as_deref()
        .ok_or_else(|| anyhow!("extension {} has no native module", extension.sql_name))?;
    match extension.build_kind.as_str() {
        "postgres-contrib" => {
            let contrib_dir = extension
                .contrib_dir
                .as_deref()
                .ok_or_else(|| anyhow!("contrib extension {} has no contrib_dir", extension.id))?;
            Ok(build_dir
                .join("contrib")
                .join(contrib_dir)
                .join(module_file))
        }
        "pgxs-external" => Ok(pgxs_extension_build_dir(build_dir, extension).join(module_file)),
        "postgis" => Ok(Path::new(POSTGRES_OTHER_EXTENSIONS)
            .join(&extension.id)
            .join(module_file)),
        other => bail!(
            "promoted extension {} has unsupported build kind {other}",
            extension.sql_name
        ),
    }
}

fn pgxs_extension_build_dir(
    build_dir: &Path,
    extension: &extension_catalog::PromotedExtensionBuildSpec,
) -> PathBuf {
    build_dir.join("pgxs").join(&extension.id)
}

fn extension_aot_file_stem(extension: &extension_catalog::PromotedExtensionBuildSpec) -> String {
    extension.sql_name.replace('/', "_")
}

fn validate_build_profile_outputs(outputs: &BuildOutputs, profile: &str) -> Result<()> {
    let signature_path = outputs.build_dir.join(".pglite-oxide-build-profile");
    let signature = fs::read_to_string(&signature_path)
        .with_context(|| format!("read {}", signature_path.display()))?;
    let profile_line = format!("profile={profile}");
    if !signature.lines().any(|line| line == profile_line) {
        bail!(
            "WASIX build profile signature does not match requested profile {profile}: {}",
            signature_path.display()
        );
    }

    if profile.starts_with("release") {
        let cflags = signature
            .lines()
            .find_map(|line| line.strip_prefix("cflags="))
            .unwrap_or_default();
        let has_release_opt = ["-O2", "-O3", "-Os", "-Oz"]
            .iter()
            .any(|flag| cflags.split_whitespace().any(|part| part == *flag));
        if !has_release_opt || !cflags.split_whitespace().any(|part| part == "-g0") {
            bail!(
                "release WASIX profile must include an optimizing -O flag and -g0; got cflags={cflags:?}"
            );
        }

        let makefile = outputs.build_dir.join("src/Makefile.global");
        let makefile_text = fs::read_to_string(&makefile)
            .with_context(|| format!("read {}", makefile.display()))?;
        if !["-O2", "-O3", "-Os", "-Oz"]
            .iter()
            .any(|flag| makefile_text.contains(flag))
        {
            bail!(
                "release WASIX build did not propagate optimization flags into {}",
                makefile.display()
            );
        }
    }

    Ok(())
}

fn validate_module_link_metadata(module: &BuildModuleManifestOut) -> Result<()> {
    if module.link.exports.is_empty() {
        bail!("{} has no WASM exports", module.name);
    }

    match module.kind.as_str() {
        "runtime" => {
            let missing = required_runtime_abi_exports()
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
            for banned in ["pgl_initdb", "pgl_backend", "PostgresRecoverProtocolError"] {
                if has_wasm_export(&module.link, banned) {
                    bail!(
                        "{} exports legacy builder-branch lifecycle entrypoint {banned}",
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
        .filter(|module| matches!(module.kind.as_str(), "runtime-support" | "extension"))
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

fn generate_wasix_export_list(write: bool) -> Result<()> {
    let output = wasix_export_list_text()?;
    if write {
        let path = Path::new("assets/generated/wasix-dl.exports");
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }
        fs::write(path, output).with_context(|| format!("write {}", path.display()))?;
    } else {
        print!("{output}");
    }
    Ok(())
}

fn check_generated_wasix_export_list(strict: bool) -> Result<()> {
    let expected = match wasix_export_list_text() {
        Ok(expected) => expected,
        Err(err) if !strict => {
            eprintln!("warning: skipping generated WASIX export-list check: {err:#}");
            return Ok(());
        }
        Err(err) => return Err(err).context("generate expected WASIX export list"),
    };
    let path = Path::new("assets/generated/wasix-dl.exports");
    if !path.exists() {
        if strict {
            bail!(
                "generated WASIX export list is missing at {}; run `cargo run -p xtask -- assets export-list --write`",
                path.display()
            );
        }
        eprintln!(
            "warning: generated WASIX export list is missing at {}",
            path.display()
        );
        return Ok(());
    }
    let actual = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    if actual != expected {
        if strict {
            bail!(
                "generated WASIX export list is stale at {}; run `cargo run -p xtask -- assets export-list --write`",
                path.display()
            );
        }
        eprintln!(
            "warning: generated WASIX export list is stale at {}",
            path.display()
        );
    }
    Ok(())
}

fn check_source_controlled_wasix_export_list() -> Result<()> {
    let path = Path::new("assets/generated/wasix-dl.exports");
    ensure_file(path)?;
    let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    ensure!(
        !text.trim().is_empty(),
        "{} must not be empty",
        path.display()
    );
    for symbol in [
        "ProcessStartupPacket",
        "PostgresMainLoopOnce",
        "PostgresMainLongJmp",
        "PostgresSendReadyForQueryIfNecessary",
        "pgl_getMyProcPort",
        "pgl_pq_flush",
        "pgl_sendConnData",
        "pgl_setPGliteActive",
        "pgl_set_force_host_error_recovery",
        "pgl_startPGlite",
        "pgl_wasix_input_write",
        "pgl_wasix_output_read",
        "malloc",
        "free",
    ] {
        ensure!(
            text.lines().any(|line| line == symbol),
            "{} is missing required runtime/protocol export symbol {symbol}",
            path.display()
        );
    }
    let mut previous: Option<&str> = None;
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        if let Some(previous) = previous {
            ensure!(
                previous <= line,
                "{} must stay sorted for deterministic reviews; {previous} appears before {line}",
                path.display()
            );
        }
        previous = Some(line);
    }
    println!("source-controlled WASIX export-list guard passed");
    Ok(())
}

fn wasix_export_list_text() -> Result<String> {
    if Path::new(WASIX_BUILD_MANIFEST_PATH).exists() {
        let manifest = read_build_output_manifest()?;
        return wasix_export_list_from_modules(&manifest.modules);
    }
    if Path::new(GENERATED_ASSETS_DIR)
        .join("manifest.json")
        .exists()
    {
        let manifest = read_asset_manifest()?;
        let modules = build_output_modules_from_asset_manifest(&manifest);
        return wasix_export_list_from_modules(&modules);
    }

    let outputs = BuildOutputs::discover()?;
    let modules = outputs
        .modules
        .iter()
        .map(|module| {
            Ok(BuildModuleManifestOut {
                name: module.name.clone(),
                kind: module.kind.clone(),
                path: module.path.to_string_lossy().into_owned(),
                sha256: String::new(),
                link: read_wasm_link_metadata(&module.path)?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    wasix_export_list_from_modules(&modules)
}

fn read_build_output_manifest() -> Result<BuildOutputManifestOut> {
    let path = Path::new(WASIX_BUILD_MANIFEST_PATH);
    let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))
}

fn read_asset_manifest() -> Result<AssetManifestOut> {
    read_asset_manifest_from(Path::new(GENERATED_ASSETS_DIR))
}

fn read_asset_manifest_from(asset_dir: &Path) -> Result<AssetManifestOut> {
    let path = asset_dir.join("manifest.json");
    let text = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))
}

fn build_output_modules_from_asset_manifest(
    manifest: &AssetManifestOut,
) -> Vec<BuildModuleManifestOut> {
    let mut modules = vec![BuildModuleManifestOut {
        name: "runtime:pglite".to_owned(),
        kind: "runtime".to_owned(),
        path: manifest.runtime.archive.clone(),
        sha256: manifest.runtime.module_sha256.clone(),
        link: manifest.runtime.link.clone(),
    }];

    modules.extend(
        manifest
            .runtime_support
            .iter()
            .map(|module| BuildModuleManifestOut {
                name: format!("runtime-support:{}", module.name),
                kind: "runtime-support".to_owned(),
                path: module.path.clone(),
                sha256: module.module_sha256.clone(),
                link: module.link.clone(),
            }),
    );

    if let Some(pg_dump) = &manifest.pg_dump {
        modules.push(BuildModuleManifestOut {
            name: "tool:pg_dump".to_owned(),
            kind: "tool".to_owned(),
            path: pg_dump.path.clone(),
            sha256: pg_dump.module_sha256.clone(),
            link: pg_dump.link.clone(),
        });
    }
    if let Some(initdb) = &manifest.initdb {
        modules.push(BuildModuleManifestOut {
            name: "tool:initdb".to_owned(),
            kind: "tool".to_owned(),
            path: initdb.path.clone(),
            sha256: initdb.module_sha256.clone(),
            link: initdb.link.clone(),
        });
    }

    modules.extend(manifest.extensions.iter().filter_map(|extension| {
        extension.link.clone().map(|link| BuildModuleManifestOut {
            name: format!("extension:{}", extension.sql_name),
            kind: "extension".to_owned(),
            path: extension.archive.clone(),
            sha256: extension.module_sha256.clone(),
            link,
        })
    }));

    modules
}

fn wasix_export_list_from_modules(modules: &[BuildModuleManifestOut]) -> Result<String> {
    for module in modules {
        validate_module_link_metadata(module)?;
    }

    let runtime = modules
        .iter()
        .find(|module| module.kind == "runtime")
        .ok_or_else(|| anyhow!("build outputs are missing runtime module"))?;
    let runtime_exports = wasm_export_name_set(&runtime.link);
    let mut required_exports = BTreeSet::<String>::new();
    let mut unresolved = Vec::new();

    for abi_export in required_runtime_abi_exports().iter().copied() {
        let normalized = abi_export.trim_start_matches('_');
        if runtime_exports.contains(abi_export) {
            required_exports.insert(abi_export.to_owned());
        } else if runtime_exports.contains(normalized) {
            required_exports.insert(normalized.to_owned());
        } else {
            unresolved.push(format!("runtime ABI export {abi_export}"));
        }
    }

    for module in modules
        .iter()
        .filter(|module| matches!(module.kind.as_str(), "runtime-support" | "extension"))
    {
        for import in &module.link.imports {
            if !import_should_resolve_from_runtime(import) {
                continue;
            }
            let normalized = import.name.trim_start_matches('_');
            if runtime_exports.contains(import.name.as_str()) {
                required_exports.insert(import.name.clone());
            } else if runtime_exports.contains(normalized) {
                required_exports.insert(normalized.to_owned());
            } else {
                unresolved.push(format!(
                    "{} imports {}.{}",
                    module.name, import.module, import.name
                ));
            }
        }
    }

    if !unresolved.is_empty() {
        bail!(
            "cannot generate WASIX dynamic-link export list with unresolved imports: {}",
            unresolved.join(", ")
        );
    }

    Ok(required_exports.into_iter().collect::<Vec<_>>().join("\n") + "\n")
}

fn required_runtime_abi_exports() -> &'static [&'static str] {
    &[
        "_start",
        "pgl_setPGliteActive",
        "pgl_startPGlite",
        "pgl_getMyProcPort",
        "ProcessStartupPacket",
        "pgl_sendConnData",
        "pgl_pq_flush",
        "pq_buffer_remaining_data",
        "PostgresMainLoopOnce",
        "PostgresSendReadyForQueryIfNecessary",
        "PostgresMainLongJmp",
        "pgl_set_protocol_stdio",
        "pgl_set_force_host_error_recovery",
        "pgl_wasix_input_reset",
        "pgl_wasix_input_write",
        "pgl_wasix_input_available",
        "pgl_wasix_output_reset",
        "pgl_wasix_output_len",
        "pgl_wasix_output_read",
    ]
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

fn wasm_export_name_set(link: &WasmLinkMetadataOut) -> HashSet<String> {
    link.exports
        .iter()
        .flat_map(|export| {
            let normalized = export.name.trim_start_matches('_').to_owned();
            [export.name.clone(), normalized]
        })
        .collect()
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
        "assets/wasix-build/docker_initdb.sh",
        "assets/wasix-build/docker_pgxs_extensions.sh",
        "assets/wasix-build/docker_contrib_extensions.sh",
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

    for script in commands {
        let mut command = Command::new("bash");
        command
            .arg(script)
            .env("PGLITE_OXIDE_BUILD_PROFILE", profile);
        run_command(&mut command)?;
    }

    let outputs = BuildOutputs::discover()?;
    validate_build_profile_outputs(&outputs, profile)?;
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
    validate_build_profile_outputs(&outputs, profile)?;
    outputs.write_manifest()?;
    validate_build_output_link_closure(&outputs)?;

    let skip_aot = args.iter().any(|arg| arg == "--skip-aot");
    package_assets_with_options(manifest, target, false)?;
    check_canonical_asset_layout(true)?;
    check_generated_manifest(manifest, true)?;

    if !skip_aot {
        generate_aot_artifacts(target)?;
        package_aot_artifacts(target, &outputs, manifest)?;
        check_aot_package_manifest(target)?;
    } else {
        eprintln!("warning: skipping AOT generation by request");
    }

    if !args.iter().any(|arg| arg == "--skip-package-size") {
        package_size(vec!["--enforce".to_owned()])?;
    }

    Ok(())
}

fn generate_aot_artifacts(target: &str) -> Result<()> {
    let outputs = BuildOutputs::discover_for_aot()?;
    let source_dir = Path::new("assets/wasix-build/build/aot").join(target);
    fs::create_dir_all(&source_dir).with_context(|| format!("create {}", source_dir.display()))?;
    let serializer = ensure_aot_serializer_binary()?;

    for module in &outputs.modules {
        let output = source_dir.join(&module.aot_file);
        generate_one_aot_artifact(&serializer, &module.path, &output)?;
    }
    Ok(())
}

fn package_aot_only(manifest: &SourcesManifest, target: &str) -> Result<()> {
    let outputs = BuildOutputs::discover_for_aot()?;
    package_aot_artifacts(target, &outputs, manifest)?;
    check_aot_package_manifest(target)
}

fn ensure_aot_serializer_binary() -> Result<PathBuf> {
    let mut command = Command::new("cargo");
    command
        .args([
            "build",
            "-p",
            "xtask",
            "--release",
            "--locked",
            "--features",
            "aot-serializer",
        ])
        .env("CARGO_INCREMENTAL", "0");
    if env::var_os("LLVM_SYS_221_PREFIX").is_none() && Path::new("/opt/homebrew/opt/llvm").exists()
    {
        command.env("LLVM_SYS_221_PREFIX", "/opt/homebrew/opt/llvm");
    }
    configure_windows_llvm_aot_link(&mut command);
    run_command(&mut command).context("build maintainer AOT serializer")?;

    let target_dir = env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target"));
    let target_dir = if target_dir.is_absolute() {
        target_dir
    } else {
        env::current_dir()
            .context("read current directory")?
            .join(target_dir)
    };
    let serializer = target_dir
        .join("release")
        .join(format!("xtask{}", env::consts::EXE_SUFFIX));
    ensure_file(&serializer)?;
    Ok(serializer)
}

fn generate_one_aot_artifact(serializer: &Path, input: &Path, output: &Path) -> Result<()> {
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

    let mut command = Command::new(serializer);
    command
        .args(["aot-serializer", "serialize", "--input"])
        .arg(&input)
        .arg("--output")
        .arg(output)
        .env("CARGO_INCREMENTAL", "0");
    if env::var_os("LLVM_SYS_221_PREFIX").is_none() && Path::new("/opt/homebrew/opt/llvm").exists()
    {
        command.env("LLVM_SYS_221_PREFIX", "/opt/homebrew/opt/llvm");
    }
    configure_windows_llvm_aot_link(&mut command);
    run_command(&mut command)
        .with_context(|| format!("generate AOT artifact for {}", input.display()))
}

fn configure_windows_llvm_aot_link(command: &mut Command) {
    if !cfg!(windows) {
        return;
    }

    let Some(prefix) = env::var_os("LLVM_SYS_221_PREFIX").or_else(|| env::var_os("LLVM_PATH"))
    else {
        return;
    };
    let llvm_lib = PathBuf::from(prefix).join("lib");
    if llvm_lib.is_dir() {
        let mut lib = llvm_lib.display().to_string();
        if let Some(existing) = env::var_os("LIB").and_then(|value| value.into_string().ok())
            && !existing.is_empty()
        {
            lib.push(';');
            lib.push_str(&existing);
        }
        command.env("LIB", lib);
    }
}

fn package_assets(manifest: &SourcesManifest, target: &str) -> Result<()> {
    package_assets_with_options(manifest, target, true)
}

fn package_assets_with_options(
    manifest: &SourcesManifest,
    target: &str,
    include_aot: bool,
) -> Result<()> {
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
    let assets_dir = Path::new(GENERATED_ASSETS_DIR);
    if assets_dir.exists() {
        fs::remove_dir_all(assets_dir)
            .with_context(|| format!("remove {}", assets_dir.display()))?;
    }
    fs::create_dir_all(assets_dir).with_context(|| format!("create {}", assets_dir.display()))?;

    let runtime_archive = assets_dir.join("pglite.wasix.tar.zst");
    deterministic_tar_zst(&runtime_stage, Path::new("pglite"), &runtime_archive)?;

    let pg_dump = assets_dir.join("bin/pg_dump.wasix.wasm");
    copy_file(outputs.module_path("tool:pg_dump")?, &pg_dump)?;
    let initdb = assets_dir.join("bin/initdb.wasix.wasm");
    copy_file(outputs.module_path("tool:initdb")?, &initdb)?;

    let extension_packages = package_promoted_extensions(source, build, stage, &outputs)?;
    let extension_package_refs = extension_packages
        .iter()
        .map(|extension| ExtensionPackage {
            name: extension.name.as_str(),
            sql_name: extension.sql_name.as_str(),
            archive: extension.archive.as_str(),
            path: extension.path.as_path(),
            module_path: extension.module_path.as_deref(),
            native_module: extension.native_module.as_deref(),
            stable: extension.stable,
        })
        .collect::<Vec<_>>();

    if include_aot {
        package_aot_artifacts(target, &outputs, manifest)?;
    }
    generate_pgdata_template_from_runtime_stage(manifest, &outputs, &runtime_stage, assets_dir)?;
    write_asset_manifest(
        manifest,
        outputs.module_path("runtime:pglite")?,
        &runtime_archive,
        &pg_dump,
        &initdb,
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
        &extension_package_refs,
    )?;

    println!("packaged runtime assets into {GENERATED_ASSETS_DIR}");
    if include_aot {
        println!("packaged {target} AOT artifacts");
    } else {
        println!("skipped {target} AOT artifact packaging by request");
    }
    Ok(())
}

fn generate_pgdata_template_asset(manifest: &SourcesManifest) -> Result<()> {
    let outputs = BuildOutputs::discover()?;
    let stage_root = outputs.package_stage.join("template-runtime");
    if stage_root.exists() {
        fs::remove_dir_all(&stage_root)
            .with_context(|| format!("remove {}", stage_root.display()))?;
    }
    stage_runtime_tree(&outputs.build_dir, &outputs.source_dir, &stage_root)?;
    generate_pgdata_template_from_runtime_stage(
        manifest,
        &outputs,
        &stage_root,
        Path::new(GENERATED_ASSETS_DIR),
    )
}

fn generate_pgdata_template_from_runtime_stage(
    manifest: &SourcesManifest,
    outputs: &BuildOutputs,
    runtime_stage: &Path,
    assets_dir: &Path,
) -> Result<()> {
    let output_dir = assets_dir.join("prepopulated");
    if output_dir.exists() {
        fs::remove_dir_all(&output_dir)
            .with_context(|| format!("remove {}", output_dir.display()))?;
    }
    fs::create_dir_all(&output_dir).with_context(|| format!("create {}", output_dir.display()))?;

    let work_root = assets_dir.join("template-work");
    if work_root.exists() {
        fs::remove_dir_all(&work_root)
            .with_context(|| format!("remove {}", work_root.display()))?;
    }
    fs::create_dir_all(&work_root).with_context(|| format!("create {}", work_root.display()))?;

    run_wasix_initdb_template(manifest, outputs, runtime_stage, &work_root)?;

    let pgdata = work_root.join("pgdata");
    ensure!(
        pgdata.join("PG_VERSION").is_file() && pgdata.join("global/pg_control").is_file(),
        "WASIX initdb did not create a complete PGDATA template at {}",
        pgdata.display()
    );
    clean_generated_pgdata_template(&pgdata)?;

    let archive = output_dir.join("pgdata-template.tar.zst");
    deterministic_tar_zst(&pgdata, Path::new(""), &archive)?;
    let manifest_path = output_dir.join("pgdata-template.json");
    let manifest_json = serde_json::json!({
        "architectureIndependent": true,
        "archiveSha256": sha256_file(&archive)?,
        "catalogVersion": postgres_catalog_version(&outputs.source_dir)?,
        "generatedBy": "wasix-initdb",
        "initProfile": default_initdb_profile(),
        "initdbSha256": sha256_file(outputs.module_path("tool:initdb")?)?,
        "postgresVersion": "17",
        "sourcePinsSha256": source_pins_sha256(manifest)?,
        "wasmerVersion": manifest.toolchain.wasmer,
        "wasmSha256": sha256_file(outputs.module_path("runtime:pglite")?)?,
    });
    fs::write(
        &manifest_path,
        format!("{}\n", serde_json::to_string_pretty(&manifest_json)?),
    )
    .with_context(|| format!("write {}", manifest_path.display()))?;
    fs::remove_dir_all(&work_root).with_context(|| format!("remove {}", work_root.display()))?;
    Ok(())
}

#[cfg(feature = "template-runner")]
fn run_wasix_initdb_template(
    _manifest: &SourcesManifest,
    _outputs: &BuildOutputs,
    runtime_stage: &Path,
    work_root: &Path,
) -> Result<()> {
    use std::sync::Arc;

    use wasmer::Engine;
    use wasmer_wasix::bin_factory::BinaryPackage;
    use wasmer_wasix::runners::wasi::{RuntimeOrEngine, WasiRunner};
    use wasmer_wasix::runtime::task_manager::tokio::TokioTaskManager;
    use wasmer_wasix::runtime::{PluggableRuntime, Runtime};
    use wasmer_wasix::virtual_fs;
    use wasmer_wasix::virtual_fs::null_file::NullFile;

    let package_dir = work_root.join("package");
    let package_root = work_root.join("root");
    let pgdata_root = work_root.join("pgdata");
    fs::create_dir_all(package_dir.join("modules"))
        .with_context(|| format!("create {}", package_dir.join("modules").display()))?;
    fs::create_dir_all(&pgdata_root)
        .with_context(|| format!("create {}", pgdata_root.display()))?;
    copy_tree_filtered(runtime_stage, &package_root, None)?;
    copy_file(
        &runtime_stage.join("bin/initdb"),
        &package_dir.join("modules/initdb.wasm"),
    )?;
    copy_file(
        &runtime_stage.join("bin/pglite"),
        &package_dir.join("modules/postgres.wasm"),
    )?;
    let wasmer_toml = r#"
[package]
name = "pglite-oxide/initdb-template"
version = "0.0.0"
description = "pglite-oxide generated PGDATA template builder"

[[module]]
name = "initdb"
source = "modules/initdb.wasm"
abi = "wasi"

[[module]]
name = "postgres"
source = "modules/postgres.wasm"
abi = "wasi"

[[command]]
name = "initdb"
module = "initdb"

[[command]]
name = "postgres"
module = "postgres"
"#;
    fs::write(package_dir.join("wasmer.toml"), wasmer_toml)
        .with_context(|| format!("write {}", package_dir.join("wasmer.toml").display()))?;

    let tokio_runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("create Tokio runtime for WASIX initdb template generation")?;
    let _guard = tokio_runtime.enter();
    let engine = Engine::default();
    let task_manager = Arc::new(TokioTaskManager::new(tokio_runtime.handle().clone()));
    let mut runtime = PluggableRuntime::new(task_manager);
    runtime.set_engine(engine.clone());
    runtime.set_package_loader(LocalOnlyPackageLoader);
    let runtime: Arc<dyn Runtime + Send + Sync> = Arc::new(runtime);
    let package = tokio_runtime
        .block_on(BinaryPackage::from_dir(&package_dir, runtime.as_ref()))
        .context("load WASIX initdb package")?;
    let root_fs = Arc::new(
        virtual_fs::host_fs::FileSystem::new(tokio_runtime.handle().clone(), &package_root)
            .with_context(|| {
                format!(
                    "create WASIX template root filesystem at {}",
                    package_root.display()
                )
            })?,
    ) as Arc<dyn virtual_fs::FileSystem + Send + Sync>;
    let pgdata_fs = Arc::new(
        virtual_fs::host_fs::FileSystem::new(tokio_runtime.handle().clone(), &pgdata_root)
            .with_context(|| {
                format!(
                    "create WASIX template PGDATA filesystem at {}",
                    pgdata_root.display()
                )
            })?,
    ) as Arc<dyn virtual_fs::FileSystem + Send + Sync>;

    let (stdout_file, stdout_capture) = TailCaptureFile::new(64 * 1024);
    let (stderr_file, stderr_capture) = TailCaptureFile::new(64 * 1024);
    let run_result = {
        let mut runner = WasiRunner::new();
        runner.with_current_dir("/");
        runner.with_mount("/".to_owned(), root_fs);
        runner.with_mount("/base".to_owned(), pgdata_fs);
        runner.with_args(default_initdb_args());
        runner.with_envs([
            ("PGDATA", "/base"),
            ("PGSYSCONFDIR", "/base"),
            ("HOME", "/home/postgres"),
            ("USER", "postgres"),
            ("LOGNAME", "postgres"),
            ("PGCLIENTENCODING", "UTF8"),
            ("PATH", "/bin"),
            ("LC_CTYPE", "C.UTF-8"),
            ("TZ", "UTC"),
            ("PGTZ", "UTC"),
            ("PG_COLOR", "never"),
        ]);
        runner.with_stdin(Box::<NullFile>::default());
        runner.with_stdout(Box::new(stdout_file));
        runner.with_stderr(Box::new(stderr_file));
        runner.run_command("initdb", &package, RuntimeOrEngine::Runtime(runtime))
    };
    let stdout = stdout_capture.text();
    let stderr = stderr_capture.text();
    if env::var_os("PGLITE_OXIDE_TEMPLATE_LOG").is_some() || run_result.is_err() {
        print_captured_wasix_output("initdb stdout", &stdout);
        print_captured_wasix_output("initdb stderr", &stderr);
    }
    run_result.context("run WASIX initdb to generate PGDATA template")
}

#[cfg(feature = "template-runner")]
fn print_captured_wasix_output(label: &str, output: &str) {
    if output.trim().is_empty() {
        eprintln!("{label}: <empty>");
    } else {
        eprintln!("--- {label} ---");
        eprint!("{output}");
        if !output.ends_with('\n') {
            eprintln!();
        }
        eprintln!("--- end {label} ---");
    }
}

#[cfg(not(feature = "template-runner"))]
fn run_wasix_initdb_template(
    _manifest: &SourcesManifest,
    _outputs: &BuildOutputs,
    _runtime_stage: &Path,
    _work_root: &Path,
) -> Result<()> {
    bail!(
        "`assets template` and template generation during release-build require `cargo run -p xtask --features template-runner -- ...` so xtask has a maintainer-only Wasmer compiler backend"
    )
}

#[cfg_attr(not(feature = "template-runner"), allow(dead_code))]
fn default_initdb_args() -> Vec<&'static str> {
    vec![
        "--allow-group-access",
        "--encoding",
        "UTF8",
        "--locale=C.UTF-8",
        "--locale-provider=libc",
        "--auth=trust",
        "-D",
        "/base",
    ]
}

fn default_initdb_profile() -> &'static str {
    "allow-group-access,encoding=UTF8,locale=C.UTF-8,locale-provider=libc,auth=trust"
}

fn clean_generated_pgdata_template(pgdata: &Path) -> Result<()> {
    for name in ["postmaster.pid", "postmaster.opts"] {
        let path = pgdata.join(name);
        if path.exists() {
            fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
        }
    }
    Ok(())
}

fn package_promoted_extensions(
    source: &Path,
    build: &Path,
    stage: &Path,
    outputs: &BuildOutputs,
) -> Result<Vec<OwnedExtensionPackage>> {
    let mut packages = Vec::new();
    for extension in extension_catalog::promoted_build_specs()? {
        let extension_stage = stage.join("extensions").join(&extension.sql_name);
        stage_promoted_extension(source, build, &extension, &extension_stage)?;
        let archive_path = Path::new(GENERATED_ASSETS_DIR).join(&extension.archive);
        deterministic_tar_zst(&extension_stage, Path::new(""), &archive_path)?;
        packages.push(OwnedExtensionPackage {
            name: extension.display_name,
            sql_name: extension.sql_name.clone(),
            archive: extension.archive.clone(),
            path: archive_path,
            module_path: if extension.module_file.is_some() {
                Some(
                    outputs
                        .module_path(&format!("extension:{}", extension.sql_name))?
                        .to_path_buf(),
                )
            } else {
                None
            },
            native_module: extension.module_file.clone(),
            stable: extension.stable,
        });
    }
    Ok(packages)
}

fn stage_promoted_extension(
    source: &Path,
    build: &Path,
    extension: &extension_catalog::PromotedExtensionBuildSpec,
    stage: &Path,
) -> Result<()> {
    match extension.build_kind.as_str() {
        "postgres-contrib" => stage_contrib_extension(source, build, extension, stage),
        "pgxs-external" => stage_pgxs_style_extension(build, extension, stage),
        other => bail!(
            "promoted extension {} has unsupported packaging build kind {other}",
            extension.sql_name
        ),
    }
}

fn stage_pgxs_style_extension(
    build: &Path,
    extension: &extension_catalog::PromotedExtensionBuildSpec,
    stage: &Path,
) -> Result<()> {
    let source = Path::new(&extension.source_dir);
    let build_dir = pgxs_extension_build_dir(build, extension);
    let sql_name = extension.sql_name.as_str();
    let extension_sql_dir = stage.join("share/postgresql/extension");
    fs::create_dir_all(stage.join("share/postgresql/extension"))
        .with_context(|| format!("create {}", extension_sql_dir.display()))?;
    if let Some(module_file) = &extension.module_file {
        fs::create_dir_all(stage.join("lib/postgresql"))
            .with_context(|| format!("create {}", stage.join("lib/postgresql").display()))?;
        copy_file(
            &build_dir.join(module_file),
            &stage.join("lib/postgresql").join(module_file),
        )?;
    }
    if extension.lifecycle.create_extension || extension.control_file.is_some() {
        let control_file = extension
            .control_file
            .as_deref()
            .map(Path::new)
            .filter(|path| path.is_file())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| source.join(format!("{sql_name}.control")));
        copy_file(
            &control_file,
            &stage
                .join("share/postgresql/extension")
                .join(control_file.file_name().unwrap_or_default()),
        )?;
    }
    let mut copied_root_sql = copy_extension_sql_files(&build_dir, sql_name, &extension_sql_dir)?;
    if !copied_root_sql {
        copied_root_sql = copy_extension_sql_files(source, sql_name, &extension_sql_dir)?;
    }
    if !copied_root_sql {
        let copied_build_sql_dir =
            copy_extension_sql_dir(&build_dir.join("sql"), &extension_sql_dir)?;
        if !copied_build_sql_dir {
            copy_extension_sql_dir(&source.join("sql"), &extension_sql_dir)?;
        }
    }
    if extension.id == "age" {
        let age_sql = extension_sql_dir.join("age--1.7.0.sql");
        let age_sql_text =
            fs::read_to_string(&age_sql).with_context(|| format!("read {}", age_sql.display()))?;
        ensure!(
            age_sql_text.contains("CREATE TYPE graphid"),
            "{} must contain AGE graphid type definition",
            age_sql.display()
        );
        ensure!(
            !age_sql_text
                .lines()
                .any(|line| line.trim() == "PASSEDBYVALUE,"),
            "{} still declares graphid PASSEDBYVALUE for wasm32/WASIX; rebuild AGE with SIZEOF_DATUM=4",
            age_sql.display()
        );
    }
    Ok(())
}

fn copy_extension_sql_files(source: &Path, sql_name: &str, destination: &Path) -> Result<bool> {
    if !source.is_dir() {
        return Ok(false);
    }
    let mut copied = false;
    for entry in sorted_children(source)? {
        if !entry.is_file() {
            continue;
        }
        let Some(name) = entry.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if (name.starts_with(&format!("{sql_name}--")) || name == format!("{sql_name}.sql"))
            && name.ends_with(".sql")
        {
            copy_file(&entry, &destination.join(name))?;
            copied = true;
        }
    }
    Ok(copied)
}

fn copy_extension_sql_dir(source: &Path, destination: &Path) -> Result<bool> {
    if !source.is_dir() {
        return Ok(false);
    }
    let mut copied = false;
    for entry in sorted_files(source)? {
        if entry.extension().and_then(|ext| ext.to_str()) != Some("sql") {
            continue;
        }
        let file_name = entry
            .file_name()
            .ok_or_else(|| anyhow!("SQL file has no name: {}", entry.display()))?;
        copy_file(&entry, &destination.join(file_name))?;
        copied = true;
    }
    Ok(copied)
}

fn stage_contrib_extension(
    source: &Path,
    build: &Path,
    extension: &extension_catalog::PromotedExtensionBuildSpec,
    stage: &Path,
) -> Result<()> {
    let contrib_dir = extension
        .contrib_dir
        .as_deref()
        .ok_or_else(|| anyhow!("contrib extension {} has no contrib_dir", extension.id))?;
    let extension_source = source.join("contrib").join(contrib_dir);
    fs::create_dir_all(stage.join("share/postgresql/extension")).with_context(|| {
        format!(
            "create {}",
            stage.join("share/postgresql/extension").display()
        )
    })?;
    if let Some(module_file) = &extension.module_file {
        fs::create_dir_all(stage.join("lib/postgresql"))
            .with_context(|| format!("create {}", stage.join("lib/postgresql").display()))?;
        copy_file(
            &build.join("contrib").join(contrib_dir).join(module_file),
            &stage.join("lib/postgresql").join(module_file),
        )?;
    }
    if extension.lifecycle.create_extension || extension.control_file.is_some() {
        let control_file = extension
            .control_file
            .as_deref()
            .map(Path::new)
            .filter(|path| path.is_file())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| extension_source.join(format!("{}.control", extension.sql_name)));
        copy_file(
            &control_file,
            &stage
                .join("share/postgresql/extension")
                .join(control_file.file_name().unwrap_or_default()),
        )?;
    }
    for entry in sorted_children(&extension_source)? {
        if !entry.is_file() {
            continue;
        }
        let Some(name) = entry.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if (name.starts_with(&format!("{}--", extension.sql_name))
            || name == format!("{}.sql", extension.sql_name))
            && name.ends_with(".sql")
        {
            copy_file(&entry, &stage.join("share/postgresql/extension").join(name))?;
        } else if name.ends_with(".rules") {
            let tsearch_data = stage.join("share/postgresql/tsearch_data");
            fs::create_dir_all(&tsearch_data)
                .with_context(|| format!("create {}", tsearch_data.display()))?;
            copy_file(&entry, &tsearch_data.join(name))?;
        }
    }
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
    copy_file(&build.join("src/backend/pglite"), &bin.join("postgres"))?;
    copy_file(&build.join("src/bin/pg_dump/pg_dump"), &bin.join("pg_dump"))?;
    copy_file(&build.join("src/bin/initdb/initdb"), &bin.join("initdb"))?;
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

fn package_aot_artifacts(
    target: &str,
    outputs: &BuildOutputs,
    sources: &SourcesManifest,
) -> Result<()> {
    let source_dir = Path::new("assets/wasix-build/build/aot").join(target);
    if !source_dir.exists() {
        bail!(
            "AOT source directory {} is missing; run `cargo run -p xtask -- assets aot --target-triple {target}` before packaging",
            source_dir.display()
        );
    }

    let artifacts_dir = generated_aot_dir(target);
    if artifacts_dir.exists() {
        fs::remove_dir_all(&artifacts_dir)
            .with_context(|| format!("remove {}", artifacts_dir.display()))?;
    }
    fs::create_dir_all(&artifacts_dir)
        .with_context(|| format!("create {}", artifacts_dir.display()))?;

    let mut manifest_artifacts = Vec::new();
    for module in &outputs.modules {
        let name = module.name.as_str();
        let file = module.aot_file.as_str();
        let source = source_dir.join(file);
        if !source.exists() {
            bail!(
                "missing AOT artifact {}; run AOT generation for target {target} before packaging",
                source.display()
            );
        }
        let destination = artifacts_dir.join(file);
        copy_file(&source, &destination)?;
        let raw_artifact = decode_zstd_file(&destination)
            .with_context(|| format!("decode AOT artifact {}", destination.display()))?;
        let module_sha256 = outputs
            .modules
            .iter()
            .find(|module| module.name == name)
            .map(|module| sha256_file(&module.path))
            .transpose()?
            .ok_or_else(|| anyhow!("missing build output module {name} for AOT manifest"))?;
        manifest_artifacts.push(AotManifestArtifact {
            name: name.to_owned(),
            path: file.to_owned(),
            sha256: sha256_file(&destination)?,
            raw_sha256: sha256_bytes(&raw_artifact),
            raw_size: raw_artifact.len() as u64,
            module_sha256,
            compressed: true,
        });
    }
    ensure!(
        !manifest_artifacts.is_empty(),
        "AOT packaging produced an empty manifest for {target}"
    );

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
    Ok(())
}

fn check_aot_package_manifest(target: &str) -> Result<()> {
    let outputs = BuildOutputs::discover_for_aot()?;
    let artifacts_dir = find_aot_artifact_dir(target)?;
    let manifest_path = artifacts_dir.join("manifest.json");
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
    ensure!(
        !manifest.artifacts.is_empty(),
        "AOT manifest {} contains no artifacts",
        manifest_path.display()
    );

    for artifact in &manifest.artifacts {
        let path = artifacts_dir.join(&artifact.path);
        ensure_file(&path)?;
        let actual_hash = sha256_file(&path)?;
        ensure_eq(
            &actual_hash,
            &artifact.sha256,
            &format!("AOT artifact {} sha256", artifact.name),
        )?;
        if artifact.compressed {
            let raw = decode_zstd_file(&path)
                .with_context(|| format!("decode AOT artifact {}", path.display()))?;
            ensure_eq(
                &sha256_bytes(&raw),
                &artifact.raw_sha256,
                &format!("AOT artifact {} raw sha256", artifact.name),
            )?;
            let actual_raw_size = raw.len() as u64;
            if actual_raw_size != artifact.raw_size {
                bail!(
                    "AOT artifact {} raw size mismatch: expected {} got {}",
                    artifact.name,
                    artifact.raw_size,
                    actual_raw_size
                );
            }
        }
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

fn generated_aot_dir(target: &str) -> PathBuf {
    Path::new("target/pglite-oxide/aot").join(target)
}

fn crate_aot_artifact_dir(target: &str) -> PathBuf {
    Path::new("crates/aot").join(target).join("artifacts")
}

fn find_aot_artifact_dir(target: &str) -> Result<PathBuf> {
    let generated = generated_aot_dir(target);
    if generated.join("manifest.json").is_file() {
        return Ok(generated);
    }
    let crate_dir = crate_aot_artifact_dir(target);
    if crate_dir.join("manifest.json").is_file() {
        return Ok(crate_dir);
    }
    bail!(
        "missing AOT artifacts for {target}; expected {} or {}",
        generated.display(),
        crate_dir.display()
    )
}

fn write_asset_manifest(
    sources: &SourcesManifest,
    runtime_module: &Path,
    runtime_archive: &Path,
    pg_dump: &Path,
    initdb: &Path,
    runtime_support: &[BinaryPackage<'_>],
    extensions: &[ExtensionPackage<'_>],
) -> Result<()> {
    let runtime_link = read_wasm_link_metadata(runtime_module)?;
    let runtime_exports = wasm_export_name_set(&runtime_link);
    let extension_metadata = extension_catalog::manifest_metadata_by_sql_name()?;
    let manifest = AssetManifestOut {
        format_version: 1,
        runtime: RuntimeAssetOut {
            archive: "pglite.wasix.tar.zst".to_owned(),
            sha256: sha256_file(runtime_archive)?,
            module_sha256: sha256_file(runtime_module)?,
            postgres_version: "17.5".to_owned(),
            runtime_kind: "wasix-dynamic-main".to_owned(),
            link: runtime_link.clone(),
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
        initdb: Some(BinaryAssetOut {
            name: "initdb".to_owned(),
            path: "bin/initdb.wasix.wasm".to_owned(),
            sha256: sha256_file(initdb)?,
            module_sha256: sha256_file(initdb)?,
            size: fs::metadata(initdb)
                .with_context(|| format!("metadata {}", initdb.display()))?
                .len(),
            link: read_wasm_link_metadata(initdb)?,
        }),
        pgdata_template: Some(pgdata_template_asset_out(
            sources,
            runtime_module,
            initdb,
            &Path::new(GENERATED_ASSETS_DIR).join("prepopulated/pgdata-template.tar.zst"),
            &Path::new(GENERATED_ASSETS_DIR).join("prepopulated/pgdata-template.json"),
        )?),
        extensions: extensions
            .iter()
            .map(|extension| {
                let link = extension
                    .module_path
                    .map(read_wasm_link_metadata)
                    .transpose()?;
                let metadata = extension_metadata.get(extension.sql_name).ok_or_else(|| {
                    anyhow!(
                        "extension {} is missing from generated extension catalog",
                        extension.sql_name
                    )
                })?;
                let mut core_exports_required = Vec::new();
                let mut unresolved_imports = Vec::new();
                if let Some(link) = &link {
                    for import in &link.imports {
                        if !import_should_resolve_from_runtime(import) {
                            continue;
                        }
                        let normalized = import.name.trim_start_matches('_');
                        if runtime_exports.contains(import.name.as_str()) {
                            core_exports_required.push(import.name.clone());
                        } else if runtime_exports.contains(normalized) {
                            core_exports_required.push(normalized.to_owned());
                        } else {
                            unresolved_imports.push(import.clone());
                        }
                    }
                }
                core_exports_required.sort();
                core_exports_required.dedup();
                Ok(ExtensionAssetOut {
                    name: extension.name.to_owned(),
                    sql_name: extension.sql_name.to_owned(),
                    source_kind: metadata.source_kind.clone(),
                    archive: extension.archive.to_owned(),
                    sha256: sha256_file(extension.path)?,
                    module_sha256: extension
                        .module_path
                        .map(sha256_file)
                        .transpose()?
                        .unwrap_or_default(),
                    native_module: extension.native_module.map(str::to_owned),
                    size: fs::metadata(extension.path)
                        .with_context(|| format!("metadata {}", extension.path.display()))?
                        .len(),
                    stable: extension.stable,
                    control_files: metadata.control_files.clone(),
                    dependencies: metadata.dependencies.clone(),
                    native_dependencies: metadata.native_dependencies.clone(),
                    load_order: metadata.load_order.clone(),
                    lifecycle: ExtensionLifecycleOut {
                        create_extension: metadata.lifecycle.create_extension,
                        create_schema: metadata.lifecycle.create_schema.clone(),
                        load_sql: metadata.lifecycle.load_sql.clone(),
                        post_create_sql: metadata.lifecycle.post_create_sql.clone(),
                        startup_config: metadata.lifecycle.startup_config.clone(),
                        preload_required: metadata.lifecycle.preload_required,
                        restart_required: metadata.lifecycle.restart_required,
                        shared_memory_required: metadata.lifecycle.shared_memory_required,
                    },
                    extension_imports: link
                        .as_ref()
                        .map(|link| link.imports.clone())
                        .unwrap_or_default(),
                    core_exports_required,
                    unresolved_imports,
                    installed_files: archive_file_list(extension.path)?,
                    smoke_status: ExtensionSmokeStatusOut {
                        promoted: metadata.smoke_status.promoted,
                        direct: metadata.smoke_status.direct.clone(),
                        server: metadata.smoke_status.server.clone(),
                        restart: metadata.smoke_status.restart.clone(),
                        dump_restore: metadata.smoke_status.dump_restore.clone(),
                    },
                    link,
                })
            })
            .collect::<Result<Vec<_>>>()?,
        sources: sources.sources.clone(),
    };

    let text = serde_json::to_string_pretty(&manifest).context("serialize asset manifest")?;
    let manifest_path = Path::new(GENERATED_ASSETS_DIR).join("manifest.json");
    fs::write(&manifest_path, format!("{text}\n"))
        .with_context(|| format!("write {}", manifest_path.display()))?;
    Ok(())
}

fn pgdata_template_asset_out(
    sources: &SourcesManifest,
    runtime_module: &Path,
    initdb_module: &Path,
    archive: &Path,
    manifest: &Path,
) -> Result<PgDataTemplateAssetOut> {
    ensure_file(archive)?;
    ensure_file(manifest)?;
    let manifest_text =
        fs::read_to_string(manifest).with_context(|| format!("read {}", manifest.display()))?;
    let manifest_json: serde_json::Value = serde_json::from_str(&manifest_text)
        .with_context(|| format!("parse {}", manifest.display()))?;
    Ok(PgDataTemplateAssetOut {
        archive: "prepopulated/pgdata-template.tar.zst".to_owned(),
        manifest: "prepopulated/pgdata-template.json".to_owned(),
        sha256: sha256_file(archive)?,
        size: fs::metadata(archive)
            .with_context(|| format!("metadata {}", archive.display()))?
            .len(),
        runtime_module_sha256: sha256_file(runtime_module)?,
        initdb_module_sha256: sha256_file(initdb_module)?,
        source_pins_sha256: source_pins_sha256(sources)?,
        postgres_version: "17".to_owned(),
        catalog_version: manifest_json
            .get("catalogVersion")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown")
            .to_owned(),
        init_profile: default_initdb_profile().to_owned(),
        wasmer_version: sources.toolchain.wasmer.clone(),
    })
}

fn source_pins_sha256(sources: &SourcesManifest) -> Result<String> {
    let pins = serde_json::to_vec(&sources.sources).context("serialize source pins")?;
    Ok(sha256_bytes(&pins))
}

fn postgres_catalog_version(source_dir: &Path) -> Result<String> {
    let path = source_dir.join("src/include/catalog/catversion.h");
    let text = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("#define CATALOG_VERSION_NO") {
            let value = rest.trim();
            if !value.is_empty() {
                return Ok(value.to_owned());
            }
        }
    }
    bail!("{} does not define CATALOG_VERSION_NO", path.display())
}

fn update_staged_root_asset_metadata(workspace: &Path) -> Result<()> {
    let asset_dir = workspace.join(GENERATED_ASSETS_DIR);
    let manifest = read_asset_manifest_from(&asset_dir)?;
    let runtime_archive = asset_dir.join(&manifest.runtime.archive);
    let runtime_module = archive_entry_bytes(&runtime_archive, "pglite/bin/pglite")?;
    update_root_asset_metadata_in(
        workspace,
        &asset_dir,
        &manifest,
        &sha256_bytes(&runtime_module),
    )
}

fn update_root_asset_metadata_in(
    workspace: &Path,
    asset_dir: &Path,
    manifest: &AssetManifestOut,
    runtime_module_sha256: &str,
) -> Result<()> {
    let path = workspace.join("Cargo.toml");
    let mut text = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    text = replace_metadata_value(text, "runtime-archive-sha256", &manifest.runtime.sha256);
    text = replace_metadata_value(text, "pglite-wasix-sha256", runtime_module_sha256);
    let pgdata_template = asset_dir.join("prepopulated/pgdata-template.tar.zst");
    if pgdata_template.exists() {
        text = replace_metadata_value(
            text,
            "pgdata-template-archive-sha256",
            &sha256_file(&pgdata_template)?,
        );
    }
    if let Some(pg_dump) = &manifest.pg_dump {
        text = replace_metadata_value(text, "pg-dump-wasix-sha256", &pg_dump.sha256);
    }
    if let Some(initdb) = &manifest.initdb {
        text = replace_metadata_value(text, "initdb-wasix-sha256", &initdb.sha256);
    }
    fs::write(&path, text).with_context(|| format!("write {}", path.display()))
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

fn archive_file_list(path: &Path) -> Result<Vec<String>> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let raw = if bytes.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        let mut decoder = zstd::stream::read::Decoder::new(std::io::Cursor::new(bytes))
            .with_context(|| format!("create zstd decoder for {}", path.display()))?;
        let mut raw = Vec::new();
        io::copy(&mut decoder, &mut raw)
            .with_context(|| format!("decompress {}", path.display()))?;
        raw
    } else {
        bytes
    };
    let mut archive = tar::Archive::new(std::io::Cursor::new(raw));
    let mut files = Vec::new();
    for entry in archive
        .entries()
        .with_context(|| format!("read tar entries from {}", path.display()))?
    {
        let entry = entry.with_context(|| format!("read tar entry from {}", path.display()))?;
        if entry.header().entry_type().is_file() {
            files.push(
                entry
                    .path()
                    .with_context(|| format!("read tar path from {}", path.display()))?
                    .to_string_lossy()
                    .replace('\\', "/"),
            );
        }
    }
    files.sort();
    Ok(files)
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
        if let Some(file_name) = relative.file_name().and_then(|name| name.to_str())
            && skip_names
                .map(|names| names.contains(&file_name))
                .unwrap_or(false)
        {
            continue;
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

fn copy_dir_all(source: &Path, destination: &Path) -> Result<()> {
    if destination.exists() {
        fs::remove_dir_all(destination)
            .with_context(|| format!("remove {}", destination.display()))?;
    }
    fs::create_dir_all(destination).with_context(|| format!("create {}", destination.display()))?;
    for entry in WalkDir::new(source) {
        let entry = entry.with_context(|| format!("walk {}", source.display()))?;
        let path = entry.path();
        let relative = path
            .strip_prefix(source)
            .with_context(|| format!("strip {} from {}", source.display(), path.display()))?;
        if relative.as_os_str().is_empty() {
            continue;
        }
        let output = destination.join(relative);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&output).with_context(|| format!("create {}", output.display()))?;
        } else if entry.file_type().is_file() {
            copy_file(path, &output)?;
        }
    }
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

fn decode_zstd_file(path: &Path) -> Result<Vec<u8>> {
    let file = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut decoder = zstd::stream::read::Decoder::new(file)
        .with_context(|| format!("create zstd decoder for {}", path.display()))?;
    let mut raw = Vec::new();
    io::copy(&mut decoder, &mut raw).with_context(|| format!("decompress {}", path.display()))?;
    Ok(raw)
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

fn ensure_no_flag_contains(values: &[String], forbidden: &str, field: &str) -> Result<()> {
    let forbidden_lower = forbidden.to_ascii_lowercase();
    if let Some(value) = values
        .iter()
        .find(|value| value.to_ascii_lowercase().contains(&forbidden_lower))
    {
        bail!("{field} must not contain '{forbidden}', got '{value}'");
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

fn now_micros() -> Result<u128> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before UNIX_EPOCH")?
        .as_micros())
}

fn value_after<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.windows(2)
        .find(|window| window[0] == name)
        .map(|window| window[1].as_str())
}

fn run(command: &str, args: &[&str]) -> Result<()> {
    let mut command = command_for_host(command);
    command.args(args);
    run_command(&mut command)
}

fn run_validate_script(mode: &str) -> Result<()> {
    let xtask = env::current_exe().context("resolve current xtask executable")?;
    let mut command = command_for_host("scripts/validate.sh");
    command.arg(mode).env(VALIDATE_XTASK_ENV, xtask);
    run_command(&mut command)
}

fn command_for_host(command: &str) -> Command {
    if cfg!(windows)
        && Path::new(command)
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("sh"))
    {
        let mut shell = Command::new(windows_bash_path());
        shell.arg("--noprofile").arg("--norc");
        shell.arg(command);
        return shell;
    }
    Command::new(command)
}

#[cfg(windows)]
fn windows_bash_path() -> PathBuf {
    for path in [
        r"C:\Program Files\Git\bin\bash.exe",
        r"C:\Program Files\Git\usr\bin\bash.exe",
    ] {
        let path = PathBuf::from(path);
        if path.is_file() {
            return path;
        }
    }
    PathBuf::from("bash")
}

#[cfg(not(windows))]
fn windows_bash_path() -> &'static str {
    "bash"
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
    eprintln!("  cargo run -p xtask -- assets verify-committed");
    eprintln!("  cargo run -p xtask -- assets audit-upstream [--strict]");
    eprintln!("  cargo run -p xtask -- assets source-spine [--check-patch-applies]");
    eprintln!("  cargo run -p xtask -- assets fetch");
    eprintln!("  cargo run -p xtask --features aot-serializer -- assets build-host");
    eprintln!("  cargo run -p xtask -- assets download --sha <sha> --target-triple <triple>");
    eprintln!("  cargo run -p xtask -- assets download --run-id <id> --all-targets");
    eprintln!(
        "  cargo run -p xtask -- assets download --latest-compatible --target-triple <triple>"
    );
    eprintln!("  cargo run -p xtask -- assets install-local --target-triple <triple>");
    eprintln!("  cargo run -p xtask -- assets ci-matrix [--target <all|triple>] [--github-output]");
    eprintln!("  cargo run -p xtask -- assets ci-artifacts");
    eprintln!("  cargo run -p xtask -- assets aot-targets");
    eprintln!("  cargo run -p xtask -- assets internal-packages");
    eprintln!("  cargo run -p xtask -- assets input-fingerprint --write");
    eprintln!(
        "  cargo run -p xtask -- assets build --profile release-o3 --target-triple <triple> [--execute]"
    );
    eprintln!("  cargo run -p xtask --features template-runner -- assets template");
    eprintln!(
        "  cargo run -p xtask --features template-runner -- assets release-build --profile release-o3 --target-triple <triple> [--fetch]"
    );
    eprintln!("  cargo run -p xtask -- assets aot --target-triple <triple>");
    eprintln!(
        "  cargo run -p xtask --features aot-serializer -- assets package [--target-triple <triple>]"
    );
    eprintln!("  cargo run -p xtask -- assets export-list [--write]");
    eprintln!("  cargo run -p xtask -- assets smoke");
    eprintln!("  cargo run -p xtask -- release stage");
    eprintln!("  cargo run -p xtask -- release dry-run");
    eprintln!("  cargo run -p xtask -- release publish");
    eprintln!("  cargo run -p xtask -- extensions discover [--write]");
    eprintln!("  cargo run -p xtask -- extensions build-plan [--write|--check]");
    eprintln!("  cargo run -p xtask -- extensions generate");
    eprintln!("  cargo run -p xtask -- extensions check");
    eprintln!("  cargo run -p xtask -- package-size --enforce");
    eprintln!("  cargo run -p xtask -- perf cold [--reset-cache]");
    eprintln!("  cargo run -p xtask -- perf warm [--iterations N] [--connections N]");
    eprintln!(
        "  cargo run -p xtask -- perf bench [--suite all|rtt|speed] [--mode all|direct|server-sqlx|server-tokio-postgres-simple] [--iterations N] [--scale N]"
    );
    eprintln!("  cargo run -p xtask -- perf prepared-updates [--rows N] [--skip-native] [--gate]");
    eprintln!(
        "  cargo run -p xtask -- perf native-postgres [--suite all|rtt|speed] [--client tokio-postgres-simple|sqlx]"
    );
    eprintln!(
        "  cargo run -p xtask -- perf pglite-nodefs-sqlx --database-url URL --open-micros N [--suite all|rtt|speed]"
    );
    eprintln!("  cargo run -p xtask -- perf diagnose-speed-hotspots");
    eprintln!("  cargo run -p xtask -- perf diagnose-speed-cases [--ids=1,6,12,16]");
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
    module_path: Option<&'a Path>,
    native_module: Option<&'a str>,
    stable: bool,
}

struct OwnedExtensionPackage {
    name: String,
    sql_name: String,
    archive: String,
    path: PathBuf,
    module_path: Option<PathBuf>,
    native_module: Option<String>,
    stable: bool,
}

struct BinaryPackage<'a> {
    name: &'a str,
    path: &'a Path,
    runtime_path: &'a str,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
struct BuildOutputManifestOut {
    format_version: u32,
    build_profile: String,
    modules: Vec<BuildModuleManifestOut>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
struct BuildModuleManifestOut {
    name: String,
    kind: String,
    path: String,
    sha256: String,
    link: WasmLinkMetadataOut,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
struct AssetManifestOut {
    format_version: u32,
    runtime: RuntimeAssetOut,
    runtime_support: Vec<BinaryAssetOut>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pg_dump: Option<BinaryAssetOut>,
    #[serde(skip_serializing_if = "Option::is_none")]
    initdb: Option<BinaryAssetOut>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pgdata_template: Option<PgDataTemplateAssetOut>,
    extensions: Vec<ExtensionAssetOut>,
    sources: Vec<SourcePin>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
struct RuntimeAssetOut {
    archive: String,
    sha256: String,
    module_sha256: String,
    postgres_version: String,
    runtime_kind: String,
    link: WasmLinkMetadataOut,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
struct BinaryAssetOut {
    name: String,
    path: String,
    sha256: String,
    module_sha256: String,
    size: u64,
    link: WasmLinkMetadataOut,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
struct PgDataTemplateAssetOut {
    archive: String,
    manifest: String,
    sha256: String,
    size: u64,
    runtime_module_sha256: String,
    initdb_module_sha256: String,
    source_pins_sha256: String,
    postgres_version: String,
    catalog_version: String,
    init_profile: String,
    wasmer_version: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
struct ExtensionAssetOut {
    name: String,
    sql_name: String,
    source_kind: String,
    archive: String,
    sha256: String,
    module_sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    native_module: Option<String>,
    size: u64,
    stable: bool,
    control_files: Vec<String>,
    dependencies: Vec<String>,
    native_dependencies: Vec<String>,
    load_order: Vec<String>,
    lifecycle: ExtensionLifecycleOut,
    extension_imports: Vec<WasmImportOut>,
    core_exports_required: Vec<String>,
    unresolved_imports: Vec<WasmImportOut>,
    installed_files: Vec<String>,
    smoke_status: ExtensionSmokeStatusOut,
    #[serde(skip_serializing_if = "Option::is_none")]
    link: Option<WasmLinkMetadataOut>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
struct ExtensionLifecycleOut {
    create_extension: bool,
    create_schema: Option<String>,
    load_sql: Vec<String>,
    post_create_sql: Vec<String>,
    startup_config: Vec<String>,
    preload_required: bool,
    restart_required: bool,
    shared_memory_required: bool,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
struct ExtensionSmokeStatusOut {
    promoted: bool,
    direct: String,
    server: String,
    restart: String,
    dump_restore: String,
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
    raw_sha256: String,
    raw_size: u64,
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
        id: "stable-foundation",
        commit: "01792c31a62b7045eb22e93d7dad022bb64b1184",
        description: "REL_17_5-pglite pinned source used by @electric-sql/pglite 0.4.5",
        required: true,
    },
    UpstreamAuditItem {
        id: "builder-age",
        commit: "c7c530a",
        description: "builder branch AGE extension source and packaging reference",
        required: false,
    },
    UpstreamAuditItem {
        id: "builder-pgdump",
        commit: "f5f1005",
        description: "builder branch backend pg_dump work reference",
        required: false,
    },
    UpstreamAuditItem {
        id: "builder-pgcrypto",
        commit: "bee4a36",
        description: "builder branch pgcrypto backend work reference",
        required: false,
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
        description: "stable branch disables WAL-fill checkpointer requests",
        required: true,
    },
    UpstreamAuditItem {
        id: "stable-external-checkpointer",
        commit: "ebb22839ae6fc3837d24e949626075175f5281fd",
        description: "stable branch disables external checkpointer dependency in PGlite",
        required: true,
    },
    UpstreamAuditItem {
        id: "stable-imported-memory",
        commit: "0c98d7c9c9bd3b0d01cb6728c4802b705f05ee54",
        description: "stable branch imported memory build fix",
        required: true,
    },
    UpstreamAuditItem {
        id: "stable-memory-stack",
        commit: "9ebefd39f8d4d16b1bea9992ed03c19d43b9d956",
        description: "stable branch adjusts initial memory and stack sizing",
        required: true,
    },
    UpstreamAuditItem {
        id: "stable-postgres-user",
        commit: "ac31093ac4d9291a167c11a1eac9dc956d4fab77",
        description: "stable branch default postgres user and home",
        required: true,
    },
    UpstreamAuditItem {
        id: "stable-initdb-single-no-exit",
        commit: "a679d34cc89848bc1c46b32e4449203b6b2a2320",
        description: "stable branch keeps initdb single-user phase from exiting process state",
        required: true,
    },
    UpstreamAuditItem {
        id: "stable-atexit-single-cleanup",
        commit: "f8ab9b9f13ef9a094afac993006f24edd6aa3357",
        description: "stable branch removes PGlite atexit handler replay during embedded restart",
        required: true,
    },
    UpstreamAuditItem {
        id: "stable-postmaster-environment",
        commit: "50354221668b9a5d2f9cf79cd4bc93fa68ef923d",
        description: "stable branch marks PGlite single-user mode as postmaster environment",
        required: true,
    },
    UpstreamAuditItem {
        id: "stable-timer-cleanup",
        commit: "e01963726df03e4700de48b69d1ac16ea5e20bef",
        description: "stable branch clears timers on embedded process exit",
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
