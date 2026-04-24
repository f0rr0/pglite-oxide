## PGlite Rust Port Plan (Non-Web Parity)

The TypeScript reference (`packages/pglite/src`) is the specification. We port every runtime behaviour except browser-specific storage adapters (IDBFS, OPFS) and worker glue. This plan tracks remaining work to reach parity.

### Completed
- Runtime provisioning / installation: unpack embedded archive into `<root>/tmp/pglite`, ensure `/dev` and `/tmp/pglite/base`, seed PGDATA if missing. Mirrors TS `base.ts`, `pglite.ts` init.
- WASI bootstrapping: env/argv limited to `PREFIX`, `PGDATA`, `PGUSER`, `PGDATABASE`, `MODE`, `REPL`; CMA channel as default transport.
- Array type discovery and (de)serialisation: query `pg_type`, register array parsers/serialisers, replicate JS helpers.
- Query/exec/describe flows: end-to-end parity with TS base class (extended/simple query, transaction helpers, error enrichment).
- Blob handling (`/dev/blob`): accept `QueryOptions.blob` input and surface output blobs in results.
- Notification delivery: `listen`/`unlisten` APIs, channel/global callbacks, dispatch from protocol parser.
- Lifecycle tracking: `is_ready`, `is_closed`, `close`, `Drop`, internal ready/closing flags.
- Extension helpers: `install_extension_archive` / `install_extension_bytes` unpack `.tar.gz` bundles into the runtime FS.
- Host filesystem sync after each operation and fallback file transport channel to mirror TS behaviour.
- `load_fs_bundle` exposes the dynamic FS bundle (mirrors JS loader override).

### Remaining Features
1. **COPY entry points & helpers**
   - TS currently lacks dedicated `copyFrom`/`copyTo` helpers; the wasm runtime already supports `/dev/blob` paths.
   - Decide whether to expose Rust convenience functions now or wait for the reference. For strict parity, defer until TS lands them.

2. **Notification API refinements**
   - TS normalises channels via `toPostgresName`; confirm all call sites use the same helper (done).
   - Provide ability to list active listeners (optional; TS doesn’t expose this).
   - Ensure `listen` invoked inside transactions mirrors TS behaviour (errors bubble up; transaction state toggled).

3. **Live / vector / pg_ivm extensions**
   - TS exposes optional Live Query, vector, and pg_ivm helpers. Investigate scope:
     - `packages/pglite/src/live/**`
     - `packages/pglite/src/vector/**`
     - `packages/pglite/src/pg_ivm/**`
   - Determine whether to port now or later (these rely on additional wasm assets / JS host features).

4. **Template utilities & SQL tagging**
   - Basic helpers (`QueryTemplate`, `quote_identifier`, `format_query`) implemented; revisit once richer DSL is needed.

5. **Filesystem adapters (NodeFS, MemoryFS, etc.)**
   - In TS these provide storage backends. For Rust we currently use host filesystem only. Document parity and optional future work:
     - NodeFS swap-in => host FS (already default)
     - MemoryFS / IDBFS / OPFS (web-only) – out of scope.

6. **Extensions loading**
   - TS `extensionUtils.ts` loads tarballs into the wasm FS. Our installer copies bundled extensions from `assets/extensions`. Check parity for runtime `install_extension_bytes/install_extension_archive`.

7. **Error types & utilities**
   - Port `errors.ts` helpers, `makePGliteError` details (already mirrored in `PgliteError`, but review field coverage).

8. **Polyfills / workers**
   - TS polyfills (indirect eval, blank) and worker entry points are inapplicable; document explicitly as out of scope.

### Next Steps
1. **Live / vector features**  
   Skeleton modules exist but return "not supported"; port full behaviour (triggers, workers) when feasible.

2. **Memory-style backends**  
   Evaluate whether exposing dedicated in-memory paths beyond temporary directories is necessary.

3. **Testing**  
   - Expand beyond smoke test: create integration tests for array handling, blob COPY round-trips, notifications.
   - Mirror TS test expectations where practical.

Update this plan as each item is implemented or explicitly descoped.
