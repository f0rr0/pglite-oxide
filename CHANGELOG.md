# Changelog

All notable changes to this project will be documented in this file.

This project follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

## [0.1.0] - 2026-04-24

- Initial repository release.

[Unreleased]: https://github.com/f0rr0/pglite-oxide/compare/0.2.0...HEAD
[0.2.0]: https://github.com/f0rr0/pglite-oxide/compare/0.1.0...0.2.0
[0.1.0]: https://github.com/f0rr0/pglite-oxide/releases/tag/0.1.0
