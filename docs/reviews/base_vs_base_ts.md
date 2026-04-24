### Review: Rust `base.rs` vs TS `base.ts`/bootstrap in `pglite.ts`

Reference TS sources: https://github.com/electric-sql/pglite/tree/main/packages/pglite/src

#### Purpose mapping

- Rust `base.rs` handles runtime provisioning (embedded tar.xz), `PGDATA` directory creation, optional extension tar installs, and exposes install/init helpers.
- TS `base.ts` is not a provisioning file; bootstrap happens in `pglite.ts` (Emscripten opts, FS bundle, initdb/backend). This review maps Rust `base.rs` to the nearest TS bootstrap responsibilities.

#### Embedded runtime vs JS bootstrap

- Rust: unpacks embedded `pglite-wasi.tar.xz` into parent of `pgroot` and validates presence of `pglite/bin/pglite.wasi` and `share/postgresql/postgres.bki`.
```114:137:/Users/sid/dev/pglite-oxide/src/pglite/base.rs
info!("unpacking embedded runtime");
let mut decoder = XzDecoder::new(*ARCHIVE_BYTES);
let mut ar = Archive::new(&mut decoder);
let unpack_target = paths.pgroot.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| paths.pgroot.clone());
ar.unpack(&unpack_target)?;
```

- TS: loads FS bundle and wasm via `PostgresModFactory` and `instantiateWasm`; no host tar unpack.
```230:247:/Users/sid/dev/pglite-oxide/tmp/pglite-ts/packages/pglite/src/pglite.ts
let emscriptenOpts: Partial<PostgresMod> = { WASM_PREFIX, arguments: args, INITIAL_MEMORY: options.initialMemory, noExitRuntime: true, instantiateWasm: (...), getPreloadedPackage: (...) }
```
```370:372:/Users/sid/dev/pglite-oxide/tmp/pglite-ts/packages/pglite/src/pglite.ts
this.mod = await PostgresModFactory(emscriptenOpts)
```

#### Paths and cluster detection

- Rust: real `pgroot/tmp/pglite/base` layout and cluster detection via host `PG_VERSION`.
```51:56:/Users/sid/dev/pglite-oxide/src/pglite/base.rs
let pgroot = base.join("tmp");
let pgdata = pgroot.join("pglite").join("base");
```
```75:82:/Users/sid/dev/pglite-oxide/src/pglite/base.rs
fn marker_cluster(&self) -> PathBuf { self.pgdata.join("PG_VERSION") }
pub fn is_cluster_initialized(&self) -> bool { self.marker_cluster().exists() }
```

- TS: checks `PG_VERSION` within its virtual FS via `FS.analyzePath`.
```388:392:/Users/sid/dev/pglite-oxide/tmp/pglite-ts/packages/pglite/src/pglite.ts
if (this.mod.FS.analyzePath(PGDATA + '/PG_VERSION').exists) { /* ... */ }
```

#### Extension installation

- Rust: supports installing extension tarballs into `pgroot/pglite` from bytes or file.
```139:157:/Users/sid/dev/pglite-oxide/src/pglite/base.rs
fn install_extension_reader<R: Read>(paths: &PglitePaths, reader: R) -> Result<()> { /* tar.gz unpack */ }
pub fn install_extension_archive(...)
pub fn install_extension_bytes(...)
```

- TS: extension bundles are registered via `pg_extensions` and compiled after module load; different mechanism (`extensionUtils.ts` and `loadExtensions`). Not directly mirrored here.

#### Install helpers

- Rust: `install_default`, `install_into`, `install_and_init`, `install_with_options` return `InstallOutcome`/`MountInfo` with host mount path.
```227:281:/Users/sid/dev/pglite-oxide/src/pglite/base.rs
pub fn install_default(...)
pub fn install_into(...)
pub fn install_and_init(...)
pub fn install_with_options(...)
```

- TS: `PGlite.create({ dataDir, ... })` returns an instance; filesystem mount paths are abstracted away by the FS layer.

#### Differences (Rust extras / TS extras)

- Rust extras:
  - Embedded tar.xz runtime unpack and validation.
  - Real host `PGDATA` directory creation.
  - Extension tarball install helpers.
  - `MountInfo` exposing host mount path and reuse flag.

- TS extras:
  - FS bundle download/selection and wasm instantiation plumbed via Emscripten options.
  - Extension bundle integration pipeline and dynamic compilation.
  - DataDir tar load (`loadDataDir`) pre-init.


