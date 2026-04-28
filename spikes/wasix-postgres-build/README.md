# WASIX Postgres Build Spike

This spike keeps the long-term extension work inside this repo while using
upstream sources as submodules under `spikes/upstream`.

## Upstream Sources

- `spikes/upstream/pglite-build`: `electric-sql/pglite-build`, branch `portable`
- `spikes/upstream/postgres-pglite`: `electric-sql/postgres-pglite`, branch
  `REL_17_5_WASM-pglite-builder`
- `spikes/upstream/pglite`: `electric-sql/pglite`, branch `main`
- `spikes/upstream/pgvector`: `pgvector/pgvector`, current checkout used by the
  spike

The active source patch is stored at:

```sh
spikes/wasix-postgres-build/patches/postgres-pglite-wasix-dl.patch
```

It adds a `PORTNAME=wasix-dl` Postgres template, a WASIX makefile, and extends
the existing PGlite extension import/export collection so WASIX side modules can
use the same symbol-list path. The patch was first proven on `REL_17_5-pglite`;
the production roadmap requires rebasing it onto
`REL_17_5_WASM-pglite-builder` without losing the Rust protocol bridge exports.

The builder branch deleted the old `pglite/src/pglitec/pglitec.c` file used by
the first spike. The owned WASIX bridge now lives at
`spikes/wasix-postgres-build/wasix_shim/pglite_wasix_bridge.c`; it supplies the
Rust-hosted protocol buffers, protocol-fd socket shims, user/process shims, and
shared-memory shims while the upstream checkout keeps the builder branch's
`pglite-wasm` lifecycle files. Non-protocol `recv`, `send`, and `connect`
delegate back to WASIX libc; this is required for frontend tools such as the
WASIX `pg_dump` binary to reach the local Postgres server through Wasmer host
networking.

Docker wrappers do not mutate the upstream submodule directly. They first run
`prepare_patched_source.sh`, which creates an ignored git worktree at
`spikes/wasix-postgres-build/work/postgres-pglite-wasix-src` from the pinned
builder commit and applies `postgres-pglite-wasix-dl.patch`. Configure/build
commands use that patched source tree through `PGSRC`.

## Local Probes

Configure the patched Postgres tree with local `wasixcc`:

```sh
./spikes/wasix-postgres-build/configure_wasix_dl.sh
```

Compile the Postgres dynamic loader object:

```sh
make -C spikes/wasix-postgres-build/work/configure-smoke/src/backend/utils generated-header-symlinks
env HOME=/tmp/wasixcc-home PATH=/tmp/wasixcc-home/.wasixcc/bin:$PATH \
  make -C spikes/wasix-postgres-build/work/configure-smoke/src/backend/utils/fmgr dfmgr.o
```

Build a minimal Postgres-shaped extension side module:

```sh
./spikes/wasix-postgres-build/build_min_ext.sh
```

Run a WASIX dynamic-main program that `dlopen`s that extension and resolves
`Pg_magic_func` plus `pg_finfo_wasix_min_ext_add_one`:

```sh
./spikes/wasix-postgres-build/run_min_ext_dlopen.sh
```

Expected output:

```text
Pg_magic_func pointer: 0x...
pg_finfo pointer: 0x...
Postgres-shaped extension dlopen proof passed.
```

## What Is Proven

- `postgres-pglite` can configure with `--with-template=wasix-dl`.
- `dfmgr.c`, the real Postgres dynamic extension loader, compiles with the
  WASIX EH+PIC sysroot.
- A Postgres-shaped extension side module can export `Pg_magic_func`,
  `pg_finfo_*`, and a SQL-callable function.
- Wasmer/WASIX can `dlopen` that side module and resolve the extension symbols.
- The LLVM-backed Rust runner can start the PGlite dynamic main, run
  `CREATE EXTENSION vector`, create a `vector(3)` column, insert a vector value,
  and execute a distance query.

## Docker Build Loop

Docker is now the preferred repeatable path for this spike:

```sh
./spikes/wasix-postgres-build/docker_probe.sh
./spikes/wasix-postgres-build/docker_make.sh
```

`docker_probe.sh` validates the fast loop:

- builds/reuses `pglite-oxide-wasix-build:local`
- configures `postgres-pglite` with `PORTNAME=wasix-dl`
- builds generated headers and `dfmgr.o`
- builds the minimal Postgres-shaped extension side module

`docker_make.sh` defaults to the generic backend target:

```sh
make -C /work/spikes/wasix-postgres-build/work/docker-configure/src/backend all
```

The wrapper intentionally runs generated headers and `libpgport/libpgcommon`
serially before the parallel backend build. PostgreSQL's backend makefile can
otherwise race generated headers or archives when invoked directly with `-j`.

Use `FORCE_RECONFIGURE=1` after changing configure flags or the
`wasix-dl` template:

```sh
FORCE_RECONFIGURE=1 ./spikes/wasix-postgres-build/docker_make.sh
```

Build the PGlite-mode dynamic main with upstream `pglitec.c` shims:

```sh
FORCE_RECONFIGURE=1 ./spikes/wasix-postgres-build/docker_pglite.sh
```

Build pgvector as a WASIX side module against that same configured tree:

```sh
./spikes/wasix-postgres-build/docker_pgvector.sh
```

Build a representative contrib extension from the same configured tree:

```sh
./spikes/wasix-postgres-build/docker_pgtrgm.sh
```

Build `pg_dump` from the same configured tree:

```sh
./spikes/wasix-postgres-build/docker_pgdump.sh
```

`docker_pgdump.sh` links libpq statically and fails if `PQ*` symbols remain as
dynamic imports. The packaged private runner currently proves a plain-SQL
dump/restore round-trip through `PgliteServer`.

The Docker memory limit observed here is about 8 GiB, so `JOBS=4` is the
default. Increase it only after a clean build proves memory headroom.

Measured on this machine:

- cold Docker image + probe: 190.42s
- corrected persistent configure warm-up: 74.55s
- cached probe loop: 5.87s
- first successful generic backend WASIX link: reached after incremental
  compile/fix passes; final relink loop: 7.21s
- output: `src/backend/postgres`, 14 MiB WebAssembly module with `dylink.0`
- first successful PGlite-mode dynamic-main build: 288.23s
- cached PGlite-mode relink after deleting only `src/backend/pglite`: 17.05s
- PGlite-mode output: `src/backend/pglite`, 14 MiB WebAssembly module with
  `dylink.0`; it exports `_start`, `PostgresMain`, `dlopen`, `dlsym`, PGlite
  lifecycle/protocol entrypoints such as `pgl_initdb` and `pgl_backend`, and the
  `pgl_*` shim symbols
- after splitting executable and shared-library linker flags, `contrib/pg_trgm`
  builds as a 65 KiB WASIX side module with `dylink.0`, `Pg_magic_func`, and
  `_PG_init`
- `pgvector` builds as a 209 KiB WASIX side module with `dylink.0`,
  `Pg_magic_func`, `_PG_init`, HNSW, IVFFlat, and vector distance symbols
- pgvector side-module parser audit: 167 normal main exports, 17 `GOT.mem`
  globals, 2 `GOT.func` table entries, one function table import, and 4 shared
  host ABI imports resolve with 0 unresolved imports
- Wasmer EH-enabled compile/instantiate audit:
  - cold main PGlite module compile was measured at 159.85-234.18s with the
    current Cranelift cache wrapper; earlier cold runs were 275-572s before the
    cache path was tightened
  - warm PGlite compiled-module cache load is now roughly 0.5-1.05s depending
    on the probe
  - main PGlite module instantiates in about 64-106ms after cache load
  - pgvector side module cold compile was measured at 3.80s
  - warm pgvector compiled-module cache load was measured at 10.80-12.65ms
  - the link harness instantiates the side module against the PGlite main module
  - `GOT.mem` imports relocate from same-named main exported address globals
  - `GOT.func` imports relocate by inserting exported main functions into the
    indirect function table and passing mutable globals containing table indices
  - the only non-Postgres imports left are the shared Emscripten/WASIX ABI
    values: `env.memory`, `env.__stack_pointer`, `env.__memory_base`, and
    `env.__table_base`
  - latest measured full link audit with warm cache: main cache load 710.23ms,
    main instantiate 92.13ms, pgvector cache load 10.80ms, side instantiate
    3.76ms
  - the real Wasmer/WASIX `dlopen` syscall loads `/lib/vector.so` from the
    PGlite dynamic-main instance
  - `dlsym` resolves `Pg_magic_func`, `pg_finfo_vector_in`, and `vector_in`
  - Postgres `dfmgr.c` reaches `load_external_function('/lib/vector.so',
    'vector_in')` and returns a nonzero function address
  - the PGlite WASIX patch now exports `ProcessStartupPacket`, adds a
    non-Emscripten protocol buffer in `pglitec.c`, and skips host executable
    self-discovery during embedded WASIX startup
  - the current LLVM SQL probe reaches `ProcessStartupPacket` and receives
    success for a startup packet
  - `SELECT 1`, `CREATE EXTENSION vector`, vector table creation, vector insert,
    and a vector distance query all succeed under Wasmer/LLVM
  - the runtime filesystem mount has been isolated with a tiny WASIX probe:
    `/bin/pglite.wasi`, `/base`, and `/base/PG_VERSION` are visible inside
    Wasmer/WASIX when the runtime tree is mounted at `/`
  - a tiny WASIX setjmp/longjmp probe isolates the current Cranelift blocker:
    Cranelift panics while compiling it on macOS, Singlepass rejects `Throw`,
    and LLVM passes it after installing Homebrew LLVM 22.1.4
  - full PGlite under LLVM cold-compiles in 450.70-572.68s with cache disabled;
    warm cache loads are about 0.5-1.8s for PGlite and 17-46ms for pgvector
  - the remaining production work is build/runtime layout cleanup and Linux CI
    backend selection, not filesystem visibility or dynamic extension ABI

## Build-Time Estimate

Machine observed locally:

- 10 logical CPUs
- Docker Desktop: 10 CPUs, about 8 GiB RAM
- about 181 GiB free on the repo filesystem

Docker is installed and running. The existing upstream CI script should still be
treated as too broad for local iteration; use the narrowed WASIX-only Docker
wrappers above.

Latest checked `pglite-build` CI on branch `portable`:

- workflow: `CI`
- latest successful run checked: `2025-07-26T01:05:20Z` to
  `2025-07-26T02:11:22Z`
- URL: https://github.com/electric-sql/pglite-build/actions/runs/16534406063
- duration: about 66 minutes
- job includes WASI build, native bindings build, and Emscripten build

Local targeted probes are fast:

- configure smoke: 52.64s measured locally
- generated headers + `dfmgr.o`: seconds
- minimal extension build + `dlopen`: seconds
- cached Docker probe: under 6s

Realistic local estimate if Docker/proot is enabled:

- full existing upstream CI matrix: 70-120 minutes cold
- isolated WASIX dynamic-main build after SDK/tool cache: 20-45 minutes
- pgvector side-module build after core install: 2-10 minutes

The next expensive run should therefore be a narrowed WASIX-only build, not the
full upstream CI matrix.
