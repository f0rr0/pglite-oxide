# Runtime Assets

The crate embeds a WASI PGlite runtime archive, not the Emscripten `pglite.wasm`
published in the JavaScript package.

Current source:

- Runtime artifact branch: `electric-sql/pglite-build` `gh-pages`
- Runtime artifact commit: `4c78ee29513799a51d4e1f75008cf9c3f00b11e9`
- Full artifact set on that branch includes `pglite-wasi.tar.xz`, `pglite.wasi`,
  `pglite.data`, `pglite.wasm`, `pglite.js`, `pglite.cjs`, `pglite.html`,
  `bin/pg_dump.wasm`, and extension archives.

Current metadata:

- PostgreSQL runtime: `17.5`
- Upstream branch family: `electric-sql/postgres-pglite` `REL_17_5-pglite`
- Latest JS package checked: `@electric-sql/pglite@0.4.4` on April 24, 2026
- Runtime archive SHA-256: `c725235f22a4fd50fed363f4065edb151a716fa769cba66f2383b8b854e6bdb5`
- `pglite.wasi` SHA-256: `a72b96adcd4ce40c51dd7201ee76a90f1b5799f633753b9cbb3c9af7b79f8da5`
- `pglite.data` SHA-256: `791a44e2ad1d48830714fb54e8662a3372618883566a0af7fc9f6b8375ab82d1`
- Filesystem manifest SHA-256: `880c9c058f416aad6ddc33fe0a1c84f6213b40c2b378e32587d25167d2f346f5`

Update checklist:

1. Check `electric-sql/pglite-build` `gh-pages` for the latest published runtime
   artifacts.
2. Replace `assets/pglite-wasi.tar.xz`; update extracted local `assets/pglite.wasi`,
   `assets/pglite.data`, and `assets/pglite_fs_manifest.json` when using the
   filesystem-bundle fallback.
3. Update `[package.metadata.pglite-oxide.assets]` in `Cargo.toml`.
4. Run `cargo test --test runtime_smoke -- --nocapture`.
5. Run `cargo package --allow-dirty` and verify the package size.
