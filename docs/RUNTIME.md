# Runtime and Performance Notes

`pglite-oxide` runs PGlite as a WASIX dynamic-linking module under headless
Wasmer. The end-user path is compiler-free: CI produces Wasmer LLVM AOT
artifacts for supported host targets, and applications load verified serialized
artifacts from the asset cache.

Implementation order is tracked in
[WASIX_WASMER_ROADMAP.md](WASIX_WASMER_ROADMAP.md). Runtime cleanup follows the
source/build spine and correctness phases in that roadmap.

## Runtime Layout

Each database root contains:

- `pglite/`: immutable runtime files linked or copied from the runtime cache
- `base/`: Postgres data directory
- `tmp/`: runtime scratch space
- `home/`: runtime home directory

The WASIX backend mounts the runtime root at `/`, uses `/base` as `PGDATA`, and
drives Postgres through the exported PGlite protocol bridge. Extensions are
installed into the canonical Postgres layout only when requested by the builder
or `enable_extension`.

The runtime now uses the canonical PostgreSQL layout directly:
`/share/postgresql`, `/share/postgresql/extension`, and `/lib/postgresql`.
Runtime install no longer mirrors `share/postgresql` into `share`, mirrors
`lib/postgresql` into `lib`, writes a host-generated timezone file, or rewrites
PGDATA timezone settings after extraction. `xtask assets check --strict-generated`
guards this source and asset layout so those spike shims do not return.

See [WASMER_WASIX_LEVERS.md](WASMER_WASIX_LEVERS.md) for the full list of
Wasmer 7+, WASIX, AOT, cache, dynamic-linking, and experimental snapshot levers
that should be tried before the runtime is considered optimized.

## Cache Layers

- process module cache keyed by AOT artifact hash
- persistent AOT cache keyed by manifest hash, Wasmer version, target, and
  engine identity
- runtime asset cache keyed by runtime archive hash
- extension asset cache keyed by extension archive hash
- template PGDATA cache keyed by manifest hash, init options, and extension set

Immutable files are hardlinked from caches into per-instance roots when the
filesystem supports it. The runtime falls back to copying when hardlinks fail.

## Startup Path

`Pglite::preload()` verifies and expands the target AOT artifact into the cache,
then deserializes it through headless Wasmer. Applications can call
`Pglite::preload_extensions([...])` before showing UI or entering a test loop to
warm extension artifacts as well.

The normal open path:

1. verifies asset manifest hashes
2. installs the runtime tree and PGDATA template
3. loads the target AOT artifact
4. starts the WASIX backend
5. opens the direct protocol transport

Unsupported targets fail with a clear "no AOT artifact for target" error rather
than compiling locally.

## Startup ABI

The Rust host now follows the same high-level lifecycle used by
`pglite-bindings`: instantiate the WASI/WASIX command, run `_start` once, then
drive Postgres through `pgl_initdb`, `pgl_backend`, and the protocol loop. The
JavaScript/Emscripten-era `pgl_startPGlite` and `pgl_setPGliteActive` probes are
not part of the maintained Rust ABI.

The current Rust bridge still starts the direct protocol path by sending a
startup packet through `ProcessStartupPacket`, `pgl_getMyProcPort`, and
`pgl_sendConnData`. That is deliberate for the current WASIX host boundary, but
it should remain a documented ABI layer on top of upstream PGlite lifecycle code
rather than drifting into a separate C runtime fork.

The public local-server startup contract is owned in Rust. `PgliteServer` parses
client startup packets before any SQL traffic reaches the embedded backend. It
currently accepts only the concrete supported identity advertised by
`database_url()`: `user=postgres` and `database=template1`. Unsupported startup
users or databases are rejected during startup with PostgreSQL-style
`ErrorResponse` messages and SQLSTATEs instead of being silently accepted under
the wrong database identity.

The Rust host now reflects that boundary in code:

- `PgliteLifecycleExports` owns only the upstream lifecycle entrypoints:
  `pgl_initdb` and `pgl_backend`.
- `WasixProtocolExports` owns the Rust/WASIX direct wire-protocol adapter:
  `pgl_getMyProcPort`, `ProcessStartupPacket`, `pgl_sendConnData`,
  `PostgresMainLoopOnce`, `PostgresSendReadyForQueryIfNecessary`,
  `PostgresRecoverProtocolError`, and `pgl_pq_flush`.
- `WasixPgliteIo` owns only byte movement between the Rust host and the guest
  module through `pgl_wasix_input_*` and `pgl_wasix_output_*`.

That split is intentional. The lifecycle path should converge with the
`REL_17_5_WASM-pglite-builder` / `pglite-bindings` model wherever possible.
The extra Rust-facing exports are acceptable only as WASIX transport and error
recovery ABI. `xtask assets check` fails if these exports collapse back into a
generic bucket or if the Rust host starts depending on the JavaScript/
Emscripten lifecycle probes.

The remaining C-side compatibility code is a named WASIX portability layer, not
host-side product behavior. It covers initdb boot/single pipe emulation, stable
`postgres` uid/passwd identity, socket-to-buffer transport for the Postgres wire
protocol, allowlisted socket/fd operations for the embedded protocol socket, and
single-process SysV shared-memory emulation. Unknown `popen` commands,
unsupported `system()` calls, unknown socket options, unexpected protocol file
descriptors, and unsupported `fcntl` commands fail closed instead of returning
diagnostic or magic placeholder values. Non-protocol file descriptors delegate to
the WASIX libc operation when available. The fallback `ProcessStartupPacket`
export also avoids generic "STUB" logging. The maintained source patch and
source-spine guard are checked against reintroducing the old debug-only
`#pragma TEST`, diagnostic `popen`, stub-log additions, broad socket no-ops, and
fake-success poll behavior.

The initdb path is intentionally still single-process. The WASIX patch names the
two special phases as `pglite_run_initdb_boot_phase` and
`pglite_run_initdb_single_phase`, with `pglite_restore_stdin_after_initdb_boot`
owning stdin restoration after the boot script is replayed. The initdb
`popen()` replacement is explicitly `PGLITE_WASIX_DL`-only and only accepts the
expected boot/single commands; anything else returns `ENOSYS`. The frontend
utility replacements in `pglite-wasm/pgl_stubs.h` are also gated to
`PGLITE_WASIX_DL`, so they are treated as a maintained build-personality surface
rather than generic upstream stubs.

The `send_ready_for_query` state in `pglite-wasm/pg_proto.c` remains a protocol
contract. It is covered by SQLx and tokio-postgres tests for Parse errors,
SQLx Bind errors, Execute errors, SQLSTATE preservation, Sync recovery, and
successful pipelined extended queries. The server compatibility suite also
checks raw wire-protocol Bind errors with exact `ErrorResponse -> ReadyForQuery`
synchronization, SSLRequest refusal, and safe CancelRequest close.

Current status:

| Area | Status before release |
| --- | --- |
| Extended-protocol ReadyForQuery coordination | Owned for the current SQLx/tokio-postgres path; broader raw-wire fuzzing is future hardening, not an unresolved shim. |
| Initdb boot/single in one process | Owned for current runtime behavior; smoke tests cover fresh initdb, restart, stale runtime-state cleanup, interrupted PGDATA cleanup with missing or incomplete cluster markers, and direct/server persistent root locking. |
| Boot/single `popen()` replacement | Owned as a WASIX-only portability layer; future cleanup should be driven by a call-site/link audit, not behavior guessing. |
| Frontend utility replacements | Link-analysis owned; `analyze_pgl_stubs.sh` now separates runtime link inputs from frontend tool inputs such as `pg_dump`, so tool symbols do not accidentally justify keeping broad `pglite-wasm` replacements forever. Memory helpers match upstream frontend allocation semantics. Delete unused replacements only after the runtime report proves they are not referenced. |
| WASIX host bridge | Owned as host ABI; broad fake-success socket/fd behavior is removed. Protocol fd `recv`/`send` stay buffer-backed, while non-protocol `recv`/`send`/`connect` delegate to WASIX libc so frontend tools like `pg_dump` can use Wasmer host networking. `xtask assets source-spine` compiles and runs a focused C ABI harness for locale, passwd, socket/fd, poll, mmap, and SysV shared-memory behavior. |

Runtime and extension archive extraction is intentionally conservative. It
rejects parent traversal, absolute paths, symlinks, hardlinks, device nodes, and
other non-file/non-directory entries before unpacking. Published assets should
still be generated by `xtask`, but the runtime no longer trusts tar entry types
just because an archive is bundled.

The correctness suite and upstream test sources are tracked in
[TESTING.md](TESTING.md).

## Protocol Error Recovery

`PostgresMainLoopOnce` can surface a PostgreSQL `ERROR` unwind to Wasmer as a
host trap before the embedded loop resumes at its `sigsetjmp` boundary. The
runtime treats that as an explicit ABI boundary:

- C exports `PostgresRecoverProtocolError`
- Rust calls it only after a `PostgresMainLoopOnce` trap
- C emits the original PostgreSQL `ErrorData` through `EmitErrorReport()`
- Rust re-enters the loop to drain already-buffered messages until `Sync`, so
  `ReadyForQuery` still comes from PostgreSQL state

Rust must not synthesize SQLSTATEs or scrape stderr. Stale runtime artifacts
that do not export `PostgresRecoverProtocolError` should fail to load.

The dependency invariant is "no backend compiler or local compile path for end
users", not "zero Wasmer compiler metadata crates". Current Wasmer `sys`/
headless loading may still pull the base `wasmer-compiler` crate. CI should
instead fail on `wasmer-compiler-llvm`, `wasmer-compiler-cranelift`, `llvm-sys`,
Wasmtime, and any avoidable `wasmer-wasix` host-networking surface in the normal
`extensions` feature set.

## Performance Gates

CI records and gates:

- first open with bundled AOT artifact
- warm second open
- `CREATE EXTENSION vector` plus first vector query
- direct API query
- `PgliteServer` plus SQLx connection
- private WASIX `pg_dump` dump/restore through `PgliteServer`

Initial blocking thresholds are first open under 5s on GitHub Ubuntu, warm open
under 1s, and vector enable plus first query under 2s. After a stable baseline,
CI should block regressions greater than 25%.

The current WIP is slower than those targets. See [PERFORMANCE.md](PERFORMANCE.md)
for the latest measured first-query latency and the distinction between the
proved AOT/WASIX architecture and the unfinished cache/template work.

## Server Limits

`PgliteServer` exposes one embedded backend through the Postgres wire protocol.
Set SQLx, `tokio-postgres`, Diesel, SeaORM, or framework pools to one
connection. Generated connection URIs include `sslmode=disable`.

## Runtime Assets

Runtime asset provenance is tracked in [ASSETS.md](ASSETS.md). The build and
AOT generation pipeline is owned by `xtask` and asset CI.
