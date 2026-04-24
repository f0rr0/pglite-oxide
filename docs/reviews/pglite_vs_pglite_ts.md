### Review: Rust `client.rs` vs TS `pglite.ts`

Reference TS sources: https://github.com/electric-sql/pglite/tree/main/packages/pglite/src

#### Scope match

- Both implement the public client surface: query, exec, transaction, describe, protocol execution, notifications, and array type discovery.

#### Initialization and engine wiring

- Rust constructs `Pglite` with a prepared `PostgresMod` and transport.
```86:112:/Users/sid/dev/pglite-oxide/src/pglite/client.rs
impl Pglite {
    /// Create a new Pglite instance backed by the provided runtime paths.
    pub fn new(paths: PglitePaths) -> Result<Self> {
        let mut pg = PostgresMod::new(paths)?;
        pg.ensure_cluster()?;
        let transport = Transport::prepare(&mut pg)?;
        let mut instance = Self { /* fields */ };
        instance.exec_internal("SET search_path TO public;", None)?;
        instance.init_array_types(true)?;
        Ok(instance)
    }
}
```

- TS loads the module via `PostgresModFactory`, sets args/env, then calls `_pgl_initdb()` and `_pgl_backend()`.
```370:447:/Users/sid/dev/pglite-oxide/tmp/pglite-ts/packages/pglite/src/pglite.ts
this.mod = await PostgresModFactory(emscriptenOpts)
await this.fs!.initialSyncFs()
// (optional) load data dir tar, check PG_VERSION
const idb = this.mod._pgl_initdb()
// ... interpret flags ...
this.mod._pgl_backend()
await this.syncToFs()
```

#### Query (extended protocol) flow

- Rust: parse → describe(S) → bind → describe(P) → execute → sync; errors wrapped into `PgliteError`.
```127:204:/Users/sid/dev/pglite-oxide/src/pglite/client.rs
fn query_internal(&mut self, sql: &str, params: &[Value], options: Option<&QueryOptions>) -> Result<Results> {
    // build ExecProtocolOptions
    // parse
    // describe(S) and read param OIDs
    // bind with serialized params
    // describe(P)
    // execute
    // sync and parse results; wrap DatabaseError -> PgliteError
}
```

- TS: same flow wrapped in `BasePGlite`.
```221:301:/Users/sid/dev/pglite-oxide/tmp/pglite-ts/packages/pglite/src/base.ts
// parse → describe(S) → bind → describe(P) → execute → sync; DatabaseError -> makePGliteError
```

#### Simple query flow

- Rust: `exec_internal` sends simple query, syncs, wraps errors.
```254:291:/Users/sid/dev/pglite-oxide/src/pglite/client.rs
fn exec_internal(&mut self, sql: &str, options: Option<&QueryOptions>) -> Result<Vec<Results>> { /* ... */ }
```

- TS: `#runExec` mirrors the same.
```310:352:/Users/sid/dev/pglite-oxide/tmp/pglite-ts/packages/pglite/src/base.ts
async #runExec(query: string, options?: QueryOptions): Promise<Array<Results>> { /* ... */ }
```

#### Protocol execution wrappers

- Rust: `exec_protocol` parses wire data with `ProtocolParser`, handles `throw_on_error`, invokes `on_notice`, and fans out notifications to listeners.
```583:637:/Users/sid/dev/pglite-oxide/src/pglite/client.rs
fn exec_protocol(&mut self, message: &[u8], options: ExecProtocolOptions) -> Result<ExecProtocolResult> { /* ... */ }
```

- TS: `execProtocol` parses via ProtocolParser with same semantics.
```689:744:/Users/sid/dev/pglite-oxide/tmp/pglite-ts/packages/pglite/src/pglite.ts
async execProtocol(message: Uint8Array, { syncToFs, throwOnError, onNotice }: ExecProtocolOptions = {}) { /* ... */ }
```

#### Protocol raw and transport selection

- Rust: `exec_protocol_raw` delegates to `transport.send`, then optionally calls `sync_to_fs()`.
```639:652:/Users/sid/dev/pglite-oxide/src/pglite/client.rs
fn exec_protocol_raw(&mut self, message: &[u8], sync_to_fs: bool, data_transfer_container: Option<DataTransferContainer>) -> Result<Vec<u8>> {
    let data = self.transport.send(&mut self.pg, message, data_transfer_container)?;
    if sync_to_fs { self.sync_to_fs()?; }
    Ok(data)
}
```

- TS: `execProtocolRawSync` selects CMA vs file, drives `_interactive_*`, reads result; `execProtocolRaw` optionally syncs FS.
```578:682:/Users/sid/dev/pglite-oxide/tmp/pglite-ts/packages/pglite/src/pglite.ts
execProtocolRawSync(message: Uint8Array, options = {}) { /* cma/file branches, interactive_one/read */ }
async execProtocolRaw(message: Uint8Array, { syncToFs = true, dataTransferContainer }: ExecProtocolOptions = {}) { /* ... */ }
```

#### Array types discovery

- Rust runs a SQL query in `init_array_types`, creates parsers/serializers for discovered arrays.
```654:721:/Users/sid/dev/pglite-oxide/src/pglite/client.rs
fn init_array_types(&mut self, force: bool) -> Result<()> { /* SELECT oid, typarray ... */ }
```

- TS: `BasePGlite._initArrayTypes()` mirrors it.
```116:135:/Users/sid/dev/pglite-oxide/tmp/pglite-ts/packages/pglite/src/base.ts
async _initArrayTypes({ force = false } = {}) { /* SELECT oid, typarray ... */ }
```

#### Notifications API

- Rust: `listen`, `unlisten`, global listeners; invokes callbacks during `exec_protocol`.
```293:367:/Users/sid/dev/pglite-oxide/src/pglite/client.rs
pub fn listen<F>(&mut self, channel: &str, callback: F) -> Result<ListenerHandle> { /* ... */ }
```

- TS: `listen`, `unlisten`, `onNotification`, `offNotification` with similar behavior.
```787:873:/Users/sid/dev/pglite-oxide/tmp/pglite-ts/packages/pglite/src/pglite.ts
onNotification(callback) { /* ... */ } offNotification(callback) { /* ... */ }
```

#### Differences (Rust extras / TS extras)

- Rust extras:
  - `sync_to_fs()` is a best-effort host filesystem sync, not a TS persistent-storage sync.
  - Blob I/O writes/reads a host path `pgroot/dev/blob`.
```745:777:/Users/sid/dev/pglite-oxide/src/pglite/client.rs
fn get_written_blob(&mut self) -> Result<Option<Vec<u8>>> { /* reads pgroot/dev/blob */ }
```

- TS extras:
  - Real FS sync via filesystem backends after each op.
```754:776:/Users/sid/dev/pglite-oxide/tmp/pglite-ts/packages/pglite/src/pglite.ts
await this.fs!.syncToFs(this.#relaxedDurability)
```
  - Virtual `/dev/blob` device registered in FS instead of host path.
```259:319:/Users/sid/dev/pglite-oxide/tmp/pglite-ts/packages/pglite/src/pglite.ts
mod.FS.registerDevice(devId, devOpt); mod.FS.mkdev('/dev/blob', devId)
```

