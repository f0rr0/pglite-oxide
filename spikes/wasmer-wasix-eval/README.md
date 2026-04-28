# Wasmer/WASIX PGlite Extension Evaluation

Throwaway spike for answering one question:

Can Wasmer/WASIX run the current `pglite.wasi` runtime and/or provide a dynamic
extension path for current PGlite extension side modules such as `vector.so`?

Run from this directory:

```sh
cargo run -- --repo-root ../..
```

What it checks:

- Unpacks the current runtime archive and `vector.tar.gz` into a temp directory.
- Inspects WASM imports, exports, custom sections, and dynamic-linking markers.
- Compiles `pglite.wasi` and `vector.so` with Wasmer.
- Instantiates current `pglite.wasi` with Wasmer/WASIX using the same guest paths
  as the production Wasmtime runtime.
- Reports whether the current runtime can satisfy the side-module ABI expected by
  `vector.so`.

This is intentionally not production code and is not wired into the root crate.

## Dynamic WASIX Embedding Proof

After building the sibling proof:

```sh
../wasix-dlopen-proof/run.sh
```

run the same dynamic main and side modules through the Rust `wasmer-wasix` API:

```sh
cargo run --bin run_wasix_dl -- \
  ../wasix-dlopen-proof/build/main.wasm \
  ../wasix-dlopen-proof/build
```

This must print the main program line, the linked-library line, and the
`dlopen`ed-library line. The proof requires an EH-enabled Wasmer engine;
`Engine::default()` rejects these modules with `exceptions proposal not enabled`.

## Wasmer Runtime Setup

All Rust embedding probes default to one shared Cranelift setup:

- Wasm exception handling is enabled explicitly.
- compiled modules are cached with Wasmer/WASIX `FileSystemCache`
- the cache key is Wasmer's `ModuleHash::sha256(bytes)` plus the engine
  deterministic id and Wasmer artifact version
- cache artifacts are treated as trusted local native code, not portable wasm

`fs_probe` can also test Wasmer Singlepass with `--engine singlepass` and LLVM
with `--engine llvm --features llvm-engine`. The LLVM feature requires
`LLVM_SYS_221_PREFIX=/opt/homebrew/opt/llvm` on this machine after upgrading
Homebrew LLVM to 22.1.4.

Quick cache proof:

```sh
cargo run --bin cache_probe -- --rebuild \
  ../wasix-dlopen-proof/build/main.wasm \
  ../wasix-dlopen-proof/build/libdlopened.so
```

Observed locally:

- small dynamic main cold compile: 11.57s
- small dynamic main warm cache load: 170.98ms
- small side module cold compile: 91.05ms
- small side module warm cache load: 1.55ms

## PGlite + pgvector Link Audit

After running the WASIX Postgres build spike:

```sh
../wasix-postgres-build/docker_pglite.sh
../wasix-postgres-build/docker_pgvector.sh
cargo run --bin link_side -- \
  --cache-dir ../wasix-postgres-build/build/wasmer-module-cache \
  ../wasix-postgres-build/work/docker-pglite/src/backend/pglite \
  ../upstream/pgvector/vector.so
```

Observed locally:

- PGlite main module compiles under an EH-enabled Cranelift Wasmer engine.
- PGlite main module instantiates and exports the Postgres symbol surface.
- pgvector side module compiles under Wasmer.
- `GOT.mem` imports can be relocated from same-named main exported globals by
  creating mutable side-module globals with the exported address values.
- `GOT.func` imports can be relocated by inserting exported main functions into
  an indirect function table and passing mutable globals containing table
  indices to the side module.
- The side module then instantiates successfully. The latest parser audit found
  167 normal main exports, 17 `GOT.mem` globals, 2 `GOT.func` table entries, one
  `env.__indirect_function_table`, 4 shared host ABI imports, and 0 unresolved
  imports.
- Cold Cranelift compile: PGlite main 234.18s, pgvector 3.80s.
- Warm Cranelift cache load: PGlite main 710.23ms, pgvector 10.80ms.
- Warm instantiate/link after cache load: main instantiate 92.13ms, side
  instantiate 3.76ms.

Use the fast parser audit to inspect import resolution without recompiling the
large main module:

```sh
cargo run --bin inspect_link -- \
  ../wasix-postgres-build/work/docker-pglite/src/backend/pglite \
  ../upstream/pgvector/vector.so
```

Run the real Wasmer/WASIX loader probe with LLVM on macOS:

```sh
LLVM_SYS_221_PREFIX=/opt/homebrew/opt/llvm \
cargo run --features llvm-engine --bin wasmer-wasix-eval -- \
  --repo-root ../.. \
  --engine llvm \
  --cache-dir ../wasix-postgres-build/build/wasmer-module-cache \
  --main-wasm ../wasix-postgres-build/work/docker-pglite/src/backend/pglite \
  --side-so ../upstream/pgvector/vector.so
```

Observed locally:

- The PGlite dynamic-main module instantiates under `WasiEnv` with a host
  filesystem rooted at the probe PGROOT.
- The actual WASIX `dlopen` path loads `/lib/vector.so`.
- `dlsym` resolves `Pg_magic_func`, `pg_finfo_vector_in`, and `vector_in`.
- Postgres `dfmgr.c` also works at the function-loader level:
  `load_external_function('/lib/vector.so', 'vector_in')` returns the same
  nonzero address.
- After exporting `ProcessStartupPacket` for WASIX and adding an in-module
  PGlite protocol buffer, the SQL probe can drive a startup packet directly and
  `ProcessStartupPacket` returns `0`.
- `SELECT 1` returns a row through the Postgres wire-protocol path.
- `CREATE EXTENSION IF NOT EXISTS vector` returns `CREATE EXTENSION`.
- `CREATE TEMP TABLE oxide_vec (embedding vector(3))`, vector insert, and a
  distance query all succeed.
- With a warm LLVM cache, the latest full probe loaded the PGlite main module in
  1.81s, loaded pgvector in 46.21ms, and instantiated PGlite in 351.35ms.
- Earlier warm LLVM cache runs loaded the main module around 0.5-0.8s and
  pgvector around 15-23ms.
- Cold LLVM compiles are not acceptable for a test loop: full PGlite took
  450.70-572.68s with cache disabled.

The current remaining work is productizing the proof, not proving the core ABI:
replace spike-only filesystem mirroring with a correct build/runtime layout,
run the same proof on Linux, and decide whether production should prefer
Cranelift where upstream EH is complete or LLVM everywhere.

Backend status:

- A tiny WASIX setjmp/longjmp probe compiled with the same toolchain panics
  Wasmer Cranelift on macOS during compilation at
  `cranelift-codegen-0.131.0/src/machinst/lower.rs:1107`.
- The same probe on Wasmer Singlepass fails with
  `not yet implemented: Throw { tag_index: 0 }`.
- The same probe on Wasmer LLVM succeeds after installing Homebrew LLVM 22.1.4:
  compile time was 2.70s with `--cache-mode off`.
- Full PGlite under Wasmer LLVM now passes startup, extension creation, dynamic
  pgvector loading, and vector query execution in this spike.

Upstream status checked on April 28, 2026:

- Wasmer 7.0 release notes list Cranelift exception handling and full WASIX
  dynamic linking.
- `wasmerio/wasmer#5962` merged Linux Cranelift EH support using
  `libunwind`/`libgcc` integration.
- `wasmerio/wasmer#6419` is still open for macOS Cranelift EH; it calls out the
  missing compact unwind support and internal calling-convention work.
- Wasmer's feature table lists exceptions for Cranelift and LLVM but not
  Singlepass, so Singlepass is not a candidate for this PGlite build.

## Filesystem and SJLJ Probes

Build the tiny filesystem and setjmp probes with the Docker WASIX toolchain:

```sh
docker run --rm -v "$PWD/../..":/work -w /work pglite-oxide-wasix-build:local \
  sh -lc '/opt/wasixcc-home/.wasixcc/bin/wasixcc -O0 -g0 \
  spikes/wasix-postgres-build/fs_probe/fs_probe.c \
  -o spikes/wasix-postgres-build/build/fs-probe/fs_probe.wasm'

docker run --rm -v "$PWD/../..":/work -w /work pglite-oxide-wasix-build:local \
  sh -lc '/opt/wasixcc-home/.wasixcc/bin/wasixcc -O0 -g0 \
  spikes/wasix-postgres-build/fs_probe/sjlj_probe.c \
  -o spikes/wasix-postgres-build/build/fs-probe/sjlj_probe_plain.wasm'
```

Run the filesystem probe against a runtime root mounted at `/`:

```sh
cargo run --bin fs_probe -- \
  --wasm ../wasix-postgres-build/build/fs-probe/fs_probe.wasm \
  --fs-root ../wasix-postgres-build/build/fs-probe-root/tmp/pglite \
  --mount / \
  --cwd / \
  --program /bin/fs_probe.wasi
```

Expected proof points:

```text
stat /bin/pglite.wasi: ok
access /bin/pglite.wasi X_OK: ok
stat /base/PG_VERSION: ok
```
