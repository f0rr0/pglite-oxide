### PGlite Rust vs TypeScript reference – file-by-file differences

Reference TS sources: https://github.com/electric-sql/pglite/tree/main/packages/pglite/src

This document lists what is extra/missing on either side. Code references below cite exact lines from our repo (Rust) and the cloned TS reference.

#### src/pglite/base.rs ↔ packages/pglite/src (init/provisioning)

- Rust extra: embedded runtime archive unpack (include_bytes, tar.xz) to host FS.
```91:105:/Users/sid/dev/pglite-oxide/src/pglite/base.rs
info!("unpacking embedded runtime");
let mut decoder = XzDecoder::new(*ARCHIVE_BYTES);
let mut ar = Archive::new(&mut decoder);
let unpack_target = paths
    .pgroot
    .parent()
    .map(|p| p.to_path_buf())
    .unwrap_or_else(|| paths.pgroot.clone());
ar.unpack(&unpack_target).with_context(|| {
    format!(
        "unpack embedded pglite-wasi.tar.xz into {}",
        unpack_target.display()
    )
})?;
```
- TS counterpart: engine load via factory with Emscripten opts (no host tar unpack).
```370:372:/Users/sid/dev/pglite-oxide/tmp/pglite-ts/packages/pglite/src/pglite.ts
// Load the database engine
this.mod = await PostgresModFactory(emscriptenOpts)
```

- Rust: concrete host paths model `pgroot/tmp/pglite/base`.
```34:39:/Users/sid/dev/pglite-oxide/src/pglite/base.rs
let pgroot = base.join("tmp");
let pgdata = pgroot.join("pglite").join("base");
```
- TS: virtual/adapter FS chosen at runtime.
```190:195:/Users/sid/dev/pglite-oxide/tmp/pglite-ts/packages/pglite/src/pglite.ts
const { dataDir, fsType } = parseDataDir(options.dataDir)
this.fs = await loadFs(dataDir, fsType)
```

- Cluster detection parity, different surface:
  - Rust checks host `PG_VERSION`.
```52:58:/Users/sid/dev/pglite-oxide/src/pglite/base.rs
fn marker_cluster(&self) -> PathBuf { self.pgdata.join("PG_VERSION") }
pub fn is_cluster_initialized(&self) -> bool { self.marker_cluster().exists() }
```
  - TS checks Emscripten FS path.
```388:392:/Users/sid/dev/pglite-oxide/tmp/pglite-ts/packages/pglite/src/pglite.ts
if (this.mod.FS.analyzePath(PGDATA + '/PG_VERSION').exists) { /* ... */ }
```

Missing in Rust vs TS: TS can load `loadDataDir` tar before init (we currently don’t expose a tar import API at this layer).
```376:385:/Users/sid/dev/pglite-oxide/tmp/pglite-ts/packages/pglite/src/pglite.ts
if (options.loadDataDir) {
  if (this.mod.FS.analyzePath(PGDATA + '/PG_VERSION').exists) {
    throw new Error('Database already exists, cannot load from tarball')
  }
  await loadTar(this.mod.FS, options.loadDataDir, PGDATA)
}
```

#### src/pglite/postgres_mod.rs ↔ packages/pglite/src/pglite.ts + postgresMod.ts

- Rust extra: wasmtime/WASI process setup with env and argv values.
```379:386:/Users/sid/dev/pglite-oxide/src/pglite/postgres_mod.rs
builder
    .env("PREFIX", WASM_PREFIX)
    .env("PGDATA", PGDATA_DIR)
    .env("PGUSER", "postgres")
    .env("PGDATABASE", "template1")
    .env("MODE", "REACT")
    .env("REPL", "N");
```
- TS counterpart: passes the same values via `arguments` to Emscripten.
```200:209:/Users/sid/dev/pglite-oxide/tmp/pglite-ts/packages/pglite/src/pglite.ts
const args = [
  `PGDATA=${PGDATA}`,
  `PREFIX=${WASM_PREFIX}`,
  `PGUSER=${options.username ?? 'postgres'}`,
  `PGDATABASE=${options.database ?? 'template1'}`,
  'MODE=REACT',
  'REPL=N',
  ...(this.debug ? ['-d', this.debug.toString()] : []),
]
```

- Rust extra: preopen host dirs into WASI (`pgroot`→`/tmp`, `pgdata`→`/tmp/pglite/base`, optional `/dev`). TS mounts within virtual FS.
```394:407:/Users/sid/dev/pglite-oxide/src/pglite/postgres_mod.rs
builder.preopened_dir(mount_dir, DirPerms::all(), FilePerms::all(), "/tmp");
builder.preopened_dir(pgdata_dir, DirPerms::all(), FilePerms::all(), "/tmp/pglite/base");
```

- Export usage parity:
  - Rust typed calls vs TS direct exports.
```138:156:/Users/sid/dev/pglite-oxide/src/pglite/postgres_mod.rs
let rc = self.exports.pgl_initdb.call(&mut self.store, ())?;
```
```397:401:/Users/sid/dev/pglite-oxide/tmp/pglite-ts/packages/pglite/src/pglite.ts
const idb = this.mod._pgl_initdb()
```

- Transport path parity with differences in fallback:
  - Rust implements CMA only; file transport is stubbed.
```244:251:/Users/sid/dev/pglite-oxide/src/pglite/postgres_mod.rs
match self.transport {
  TransportMode::Cma { .. } => self.exec_cma(...),
  TransportMode::File => bail!("file transport is not supported yet"),
}
```
  - TS supports both CMA and file via “socketfiles”.
```585:606:/Users/sid/dev/pglite-oxide/tmp/pglite-ts/packages/pglite/src/pglite.ts
case 'cma': { mod._interactive_write(message.length); mod.HEAPU8.set(message, 1) }
case 'file': {
  const pg_lck = '/tmp/pglite/base/.s.PGSQL.5432.lck.in'
  const pg_in = '/tmp/pglite/base/.s.PGSQL.5432.in'
  mod._interactive_write(0)
  mod.FS.writeFile(pg_lck, message)
  mod.FS.rename(pg_lck, pg_in)
}
```

- Rust extra: seed `/dev/urandom` file in host FS.
```434:448:/Users/sid/dev/pglite-oxide/src/pglite/postgres_mod.rs
let urandom = dev_path.join("urandom");
if urandom.exists() { return Ok(()); }
let mut buf = [0u8; 128];
getrandom::fill(&mut buf)?;
std::fs::write(&urandom, buf)?;
```
- TS counterpart: not present; TS registers `/dev/blob` for COPY.
```259:319:/Users/sid/dev/pglite-oxide/tmp/pglite-ts/packages/pglite/src/pglite.ts
// Register /dev/blob device ... mod.FS.registerDevice(...); mod.FS.mkdev('/dev/blob', devId)
```

#### src/pglite/client.rs ↔ packages/pglite/src/pglite.ts (client surface)

- Parity: query/exec/transaction/describe, protocol steps, error wrapping.
```221:291:/Users/sid/dev/pglite-oxide/src/pglite/client.rs
// parse -> describe(S) -> bind -> describe(P) -> execute -> sync; error wrapping
```
```221:301:/Users/sid/dev/pglite-oxide/tmp/pglite-ts/packages/pglite/src/base.ts
// parse -> describe(S) -> bind -> describe(P) -> execute -> sync; error wrapping
```

- Rust extra: `sync_to_fs` best-effort syncs host directories; TS actually syncs its configured virtual/persistent FS after ops.
```487:491:/Users/sid/dev/pglite-oxide/src/pglite/client.rs
pub fn sync_to_fs(&mut self) -> Result<()> { /* best-effort host fsync */ }
```
```754:776:/Users/sid/dev/pglite-oxide/tmp/pglite-ts/packages/pglite/src/pglite.ts
await this.fs!.syncToFs(this.#relaxedDurability)
```

- COPY blob handling parity with different surface:
  - Rust writes/reads `/dev/blob` via host FS path (`pgroot/dev/blob`).
```723:743:/Users/sid/dev/pglite-oxide/src/pglite/client.rs
fn dev_blob_path(&self) -> PathBuf { self.pg.paths().pgroot.join("dev/blob") }
```
  - TS implements a virtual `/dev/blob` device within Emscripten FS.
```259:319:/Users/sid/dev/pglite-oxide/tmp/pglite-ts/packages/pglite/src/pglite.ts
mod.FS.registerDevice(devId, devOpt); mod.FS.mkdev('/dev/blob', devId)
```

#### src/pglite/parse.rs ↔ packages/pglite/src/parse.ts

- Parity: build Results from backend messages; same rowMode semantics; affectedRows logic.
```11:66:/Users/sid/dev/pglite-oxide/src/pglite/parse.rs
pub fn parse_results(...)
```
```15:87:/Users/sid/dev/pglite-oxide/tmp/pglite-ts/packages/pglite/src/parse.ts
export function parseResults(...)
```

#### src/pglite/interface.rs ↔ packages/pglite/src/interface.ts

- Parity: QueryOptions, ExecProtocolOptions, Results/Describe types.
```32:41:/Users/sid/dev/pglite-oxide/src/pglite/interface.rs
pub struct QueryOptions { row_mode, parsers, serializers, blob, param_types, on_notice, data_transfer_container }
```
```23:37:/Users/sid/dev/pglite-oxide/tmp/pglite-ts/packages/pglite/src/interface.ts
export interface QueryOptions { rowMode, parsers, serializers, blob, onNotice, paramTypes }
```

- Difference: Rust strong types and Arc callbacks; TS uses structural types.

#### src/pglite/errors.rs ↔ packages/pglite/src/errors.ts

- Parity: enrich DatabaseError with query/params/options.
```9:31:/Users/sid/dev/pglite-oxide/src/pglite/errors.rs
pub struct PgliteError { source, query, params, query_options }
```
```10:21:/Users/sid/dev/pglite-oxide/tmp/pglite-ts/packages/pglite/src/errors.ts
export function makePGliteError({ e, query, params, options }) { /* attach */ }
```

#### src/pglite/types.rs ↔ packages/pglite/src/types.ts

- Parity: default parsers/serializers, array parser/serializer, OID constants.
```34:36:/Users/sid/dev/pglite-oxide/src/pglite/types.rs
pub static DEFAULT_PARSERS ... DEFAULT_SERIALIZERS ...
```
```184:188:/Users/sid/dev/pglite-oxide/tmp/pglite-ts/packages/pglite/src/types.ts
export const parsers = defaultHandlers.parsers; export const serializers = defaultHandlers.serializers
```

- Differences:
  - Rust returns JSON for unknown types; TS returns raw string.
  - TS has broader OID coverage; Rust includes a focused subset.

#### src/pglite/transport.rs ↔ pglite.ts execProtocolRaw

- Rust: CMA implemented; file transport unimplemented.
```35:53:/Users/sid/dev/pglite-oxide/src/pglite/transport.rs
match self { Transport::Cma { .. } => send_cma(...), Transport::File => bail!(...) }
```
- TS: supports CMA and file via socketfiles inside FS.
```585:647:/Users/sid/dev/pglite-oxide/tmp/pglite-ts/packages/pglite/src/pglite.ts
// cma and file branches
```

### Summary of extras/missing

- Rust extras: embedded tar unpack; WASI preopens; `/dev/urandom` file; host `/dev/blob` path; strong typing and explicit error structs.
- TS extras: virtual FS abstraction with idb/node/memory backends; file transport fallback; extension bundle plumbing and dynamic FS bundle loader; real FS sync after each op.

