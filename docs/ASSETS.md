# Runtime Assets

The crate embeds a WASI PGlite runtime archive, not the Emscripten `pglite.wasm`
published in the JavaScript package.

Current source:

- Runtime artifact branch: `electric-sql/pglite-build` `gh-pages`
- Runtime artifact commit: `4c78ee29513799a51d4e1f75008cf9c3f00b11e9`
- Full artifact set on that branch includes the WASI runtime archive,
  `pglite.wasm`, `pglite.js`, `pglite.cjs`, `pglite.html`, `bin/pg_dump.wasm`,
  and extension archives.
- The crate packages a recompressed `assets/pglite-wasi.tar.zst` archive and a
  bundled PGDATA template to keep the published crate under crates.io's 10 MiB
  package limit while avoiding first-run `initdb`.

Current metadata:

- PostgreSQL runtime: `17.5`
- Upstream branch family: `electric-sql/postgres-pglite` `REL_17_5-pglite`
- JS package version checked for this asset set: `@electric-sql/pglite@0.4.4`
- Runtime archive SHA-256: `f6f90bf571c7f5bc925ff22f94233893c2c740466da2ad23bcd4e8e1ea8c498a`
- Packaged `pglite.wasi` SHA-256: `ad423e536096ede1870f4802e7dbe4b49599c6e3bafc45aa9a4ddeeae2c5f4f8`
- PGDATA template archive SHA-256: `63e398b3cd4fec134d06539f064018fc2aa7fef75b9c331472f9b2d7385b913d`

Update checklist:

1. Check `electric-sql/pglite-build` `gh-pages` and `npm view
   @electric-sql/pglite version` for the latest published runtime artifacts.
2. Replace or regenerate `assets/pglite-wasi.tar.zst`.
3. Regenerate `assets/prepopulated/pgdata-template.tar.zst` and
   `assets/prepopulated/pgdata-template.json`.
4. Update `[package.metadata.pglite-oxide.assets]` in `Cargo.toml`.
5. Run `scripts/validate.sh ci` and `scripts/validate.sh release`.
