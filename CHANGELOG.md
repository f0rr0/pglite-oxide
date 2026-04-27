# Changelog

All notable changes to this project will be documented in this file.

This project follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.3.0] - 2026-04-26

### Breaking

- optimize startup and add Tauri SQLx profiler ([#9](https://github.com/f0rr0/pglite-oxide/pull/9))

- `PgliteRuntimeOptions::default` now selects the optimized embedded-template
  startup path.
- `ensure_cluster` now requires runtime options.
- Runtime packaging now uses the uncompressed `pglite-wasi.tar` asset.

### Added

- Reusable Wasmtime engine/module caching and on-disk compiled `.cwasm` cache
  support for faster startup.
- Embedded prepopulated PGDATA template with manifest validation.
- Vanilla Tauri v2 SQLx profiler example with release-mode workload reporting.
- Repo hooks for Conventional Commit validation, formatting, and pre-push checks.

### Changed

- Quieted WASI stdio by default and prefer Unix sockets where available.
- Streamlined runtime, release, contributing, and usage docs around the optimized
  default path.

### Fixed

- Hardened proxy frontend framing for SSL requests, cancel requests,
  split/coalesced packets, and extended-query batching.

## [0.2.0] - 2026-04-24

### Added

- modernize embedded PGlite API and OSS tooling ([#3](https://github.com/f0rr0/pglite-oxide/pull/3))

- Added the high-level `Pglite` and `PgliteServer` APIs for direct embedded use
  and PostgreSQL client compatibility.
- Added process-local template cluster reuse for fast temporary databases, with
  `fresh_temporary` escape hatches for initialization-specific tests.
- Added SQLx and `tokio-postgres` compatibility coverage, runtime/proxy smoke
  tests, CI, cargo-deny policy checks, Conventional Commit validation, and
  documented runtime asset provenance.
- Improved the blocking proxy/server path for extended-protocol clients,
  readiness handling, and socket mode behavior.

## [0.1.0] - 2025-09-27

- Initial repository release.

[Unreleased]: https://github.com/f0rr0/pglite-oxide/compare/0.3.0...HEAD
[0.3.0]: https://github.com/f0rr0/pglite-oxide/compare/0.2.0...0.3.0
[0.2.0]: https://github.com/f0rr0/pglite-oxide/compare/0.1.0...0.2.0
[0.1.0]: https://github.com/f0rr0/pglite-oxide/releases/tag/0.1.0
