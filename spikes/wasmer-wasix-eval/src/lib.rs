use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use wasmer::sys::{Cranelift, EngineBuilder, Features, Singlepass};
#[cfg(feature = "llvm-engine")]
use wasmer::sys::LLVM;
use wasmer::{Engine, Module, Store};
use wasmer_types::ModuleHash;
use wasmer_wasix::runtime::module_cache::{
    CacheError, FileSystemCache, ModuleCache as WasmerModuleCache,
};
use wasmer_wasix::runtime::task_manager::tokio::TokioTaskManager;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineKind {
    Cranelift,
    Singlepass,
    Llvm,
}

impl EngineKind {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "cranelift" => Ok(Self::Cranelift),
            "singlepass" => Ok(Self::Singlepass),
            "llvm" => Ok(Self::Llvm),
            other => anyhow::bail!("unknown engine '{other}', expected cranelift|llvm|singlepass"),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Cranelift => "cranelift",
            Self::Singlepass => "singlepass",
            Self::Llvm => "llvm",
        }
    }

    pub fn build(self) -> Result<Engine> {
        match self {
            Self::Cranelift => Ok(cranelift_engine()),
            Self::Singlepass => Ok(singlepass_engine()),
            #[cfg(feature = "llvm-engine")]
            Self::Llvm => Ok(llvm_engine()),
            #[cfg(not(feature = "llvm-engine"))]
            Self::Llvm => anyhow::bail!(
                "llvm engine was requested, but this spike was not built with --features llvm-engine"
            ),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheMode {
    Use,
    Rebuild,
    Off,
}

impl CacheMode {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "use" => Ok(Self::Use),
            "rebuild" => Ok(Self::Rebuild),
            "off" => Ok(Self::Off),
            other => anyhow::bail!("unknown cache mode '{other}', expected use|rebuild|off"),
        }
    }

    pub fn is_enabled(self) -> bool {
        !matches!(self, Self::Off)
    }
}

impl fmt::Display for CacheMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Use => f.write_str("use"),
            Self::Rebuild => f.write_str("rebuild"),
            Self::Off => f.write_str("off"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModuleLoadKind {
    CacheHit,
    Compiled,
}

#[derive(Debug, Clone)]
pub struct ModuleLoadReport {
    pub label: String,
    pub hash: ModuleHash,
    pub kind: ModuleLoadKind,
    pub elapsed: Duration,
    pub cache_dir: Option<PathBuf>,
}

impl ModuleLoadReport {
    pub fn print(&self) {
        let cache_suffix = self
            .cache_dir
            .as_ref()
            .map(|path| format!(" cache={}", path.display()))
            .unwrap_or_else(|| " cache=off".to_owned());
        let action = match self.kind {
            ModuleLoadKind::CacheHit => "cache-hit",
            ModuleLoadKind::Compiled => "compiled",
        };
        println!(
            "wasmer-compiler {action} {} hash={} in {:.2?}{cache_suffix}",
            self.label, self.hash, self.elapsed
        );
    }
}

pub struct CompiledModule {
    pub module: Module,
    pub report: ModuleLoadReport,
}

pub struct WasmerModuleCompiler {
    cache: Option<FileSystemCache>,
    runtime: Option<tokio::runtime::Runtime>,
    cache_dir: Option<PathBuf>,
}

impl WasmerModuleCompiler {
    pub fn new(cache_dir: impl Into<Option<PathBuf>>, mode: CacheMode) -> Result<Self> {
        let cache_dir = cache_dir.into();
        if !mode.is_enabled() {
            return Ok(Self {
                cache: None,
                runtime: None,
                cache_dir: None,
            });
        }

        let cache_dir = cache_dir.context("cache mode is enabled but no cache directory was set")?;
        if mode == CacheMode::Rebuild && cache_dir.exists() {
            fs::remove_dir_all(&cache_dir)
                .with_context(|| format!("clear module cache {}", cache_dir.display()))?;
        }
        fs::create_dir_all(&cache_dir)
            .with_context(|| format!("create module cache {}", cache_dir.display()))?;

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("create Tokio runtime for Wasmer module cache")?;
        let task_manager = Arc::new(TokioTaskManager::new(runtime.handle().clone()));
        let cache = FileSystemCache::new(cache_dir.clone(), task_manager);

        Ok(Self {
            cache: Some(cache),
            runtime: Some(runtime),
            cache_dir: Some(cache_dir),
        })
    }

    pub fn cache_dir(&self) -> Option<&Path> {
        self.cache_dir.as_deref()
    }

    pub fn load_or_compile(
        &self,
        engine: &Engine,
        store: &Store,
        label: impl Into<String>,
        bytes: &[u8],
    ) -> Result<CompiledModule> {
        let label = label.into();
        let hash = ModuleHash::sha256(bytes);

        if let (Some(cache), Some(runtime)) = (&self.cache, &self.runtime) {
            let start = Instant::now();
            match runtime.block_on(cache.load(hash, engine)) {
                Ok(module) => {
                    let report = ModuleLoadReport {
                        label,
                        hash,
                        kind: ModuleLoadKind::CacheHit,
                        elapsed: start.elapsed(),
                        cache_dir: self.cache_dir.clone(),
                    };
                    report.print();
                    return Ok(CompiledModule { module, report });
                }
                Err(CacheError::NotFound) => {}
                Err(err) => {
                    eprintln!(
                        "wasmer-compiler cache-load {} failed for hash={hash}: {err}; recompiling",
                        label
                    );
                }
            }
        }

        let start = Instant::now();
        let module =
            Module::new(store, bytes)
                .with_context(|| format!("compile {label} with selected Wasmer engine"))?;
        let report = ModuleLoadReport {
            label: label.clone(),
            hash,
            kind: ModuleLoadKind::Compiled,
            elapsed: start.elapsed(),
            cache_dir: self.cache_dir.clone(),
        };
        report.print();

        if let (Some(cache), Some(runtime)) = (&self.cache, &self.runtime) {
            if let Err(err) = runtime.block_on(cache.save(hash, engine, &module)) {
                eprintln!("wasmer-compiler cache-save {label} failed for hash={hash}: {err}");
            }
        }

        Ok(CompiledModule { module, report })
    }
}

pub fn cranelift_engine() -> Engine {
    let mut features = Features::new();
    features.exceptions(true);
    EngineBuilder::new(Cranelift::default())
        .set_features(Some(features))
        .engine()
        .into()
}

pub fn singlepass_engine() -> Engine {
    let mut features = Features::new();
    features.exceptions(true);
    EngineBuilder::new(Singlepass::default())
        .set_features(Some(features))
        .engine()
        .into()
}

#[cfg(feature = "llvm-engine")]
pub fn llvm_engine() -> Engine {
    let mut features = Features::new();
    features.exceptions(true);
    EngineBuilder::new(LLVM::default())
        .set_features(Some(features))
        .engine()
        .into()
}

pub fn print_engine_report(engine: &Engine) {
    print_engine_report_named("cranelift", engine);
}

pub fn print_engine_report_named(engine_name: &str, engine: &Engine) {
    println!("wasmer-engine: {engine_name}");
    println!("wasmer-engine-id: {}", engine.deterministic_id());
    println!("wasmer-feature-exceptions: enabled");
    println!("host-target: {}-{}", std::env::consts::OS, std::env::consts::ARCH);
}
