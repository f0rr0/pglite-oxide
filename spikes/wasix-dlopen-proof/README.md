# WASIX Dynamic Linking Proof

This spike verifies the long-term extension mechanism we want for
`pglite-oxide`: non-Emscripten WASM modules with native-style `dlopen` and
`dlsym`.

It intentionally does not use upstream PGlite's Emscripten artifacts. The goal
is to prove the runtime and toolchain shape that a future `postgres-pglite`
`PORTNAME=wasix-dl` build would need.

## Toolchain Used

- Wasmer 7.1.0
- wasixcc 0.4.3
- WASIX libc `v2026-03-02.1`
- WASIX LLVM `21.1.203`
- Binaryen `version_129`

The local install used during this spike lives under `/tmp`:

```sh
mkdir -p /tmp/wasixcc-home
HOME=/tmp/wasixcc-home sh /tmp/wasixcc-install.sh
```

## Run

```sh
./spikes/wasix-dlopen-proof/run.sh
```

Expected output:

```text
Hello from the main program.
Hello from the needed library.
Hello from the dlopened library, caller says: pglite-oxide
All done.
```

## Why This Matters

Postgres extensions require the same core properties:

- one shared linear memory across the main executable and extension modules
- PIC side modules with `dylink.0`
- main executable exports libc and Postgres symbols
- side modules import functions and data symbols from the main executable
- runtime `dlopen` loads an extension by path
- runtime `dlsym` returns callable function pointers and data addresses

If this proof fails, a dynamic WASIX Postgres build is not worth attempting.
If it passes, the next milestone is replacing this toy main executable with a
minimal Postgres extension loader built from `postgres-pglite`.
