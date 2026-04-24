## Strict Parity Guidance (Non-Web Runtime)

The `packages/pglite` TypeScript sources define the runtime contract. The Rust port must replicate observable behaviour exactly—unless a feature is explicitly web-only (IDB, OPFS, workers), the flow, data, and surface must match the reference.

### Core principles

1. **Clone the JS contract**
   - Public APIs, default options, error semantics, result structures, protocol steps, and type conversion must produce the same outcomes as the TS reference.
   - Treat the TS implementation as the spec: every change or extension must be traceable back to matching TS code.

2. **No additional behaviour**
   - Never add CLI flags, env vars, filesystem operations, sockets, or configuration knobs that the TS runtime does not use.
   - If the wasm module exposes capabilities unused in TS (e.g., file transport, extra exports), leave them inactive until the reference adopts them.

3. **Match process bootstrap**
   - Pass through exactly the `NAME=value` arguments the TS loader supplies (`PGDATA`, `PREFIX`, `PGUSER`, `PGDATABASE`, `MODE`, `REPL`); do not invent alternate argv/env forms.
   - Rely on the same runtime resources (shared memory CMA, wasm `Memory`) and keep WASI scaffolding invisible to consumers.

4. **Mirror filesystem semantics**
   - Use the same layout as the embedded archive (`/tmp/pglite/...`), create only the directories the TS runtime expects, and avoid host-specific migrations or markers.
   - Do not expose or depend on host-only paths (e.g., `/dev`, `.s.PGSQL.5432`) beyond what the TS code already implies.

5. **Entropy and devices**
   - Source randomness the same way TS does (via the wasm module’s existing hooks). Do not seed host pseudo devices or add alternate entropy paths unless the reference changes.

6. **Justify unavoidable differences**
   - If platform constraints force divergence, document the reason, limiting scope, and confirm that observable behaviour remains identical.

### When adding/changing code

- Inspect the TS module first; port its behaviour verbatim (minus web-only features).
- Confirm that each public change has a counterpart in the TS reference.
- Keep test cases and fixtures aligned with TS expectations (results, errors, types).

This file is a standing reminder: **mirror the TypeScript runtime everywhere except web-specific layers.**
