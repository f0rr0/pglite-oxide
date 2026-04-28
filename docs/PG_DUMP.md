# pg_dump

`pglite-dump` no longer unpacks the runtime archive. That behavior was
misleading and has been removed.

The real `pg_dump` API and CLI are reserved for the WASIX pg_dump runner:

```sh
pglite-dump --root ./.pglite -- --schema-only
```

The private runner now passes a dump/restore round-trip against the packaged
WASIX `pg_dump` artifact and the same asset manifest as the runtime and
extensions. The test starts `PgliteServer`, seeds data through SQLx, runs
`pg_dump` over the local TCP Postgres protocol using Wasmer host networking,
restores the SQL into a fresh direct `Pglite`, and verifies restored data. The
private suite also covers `--schema-only`, `--quote-all-identifiers`, and a
`vector` extension dump/restore.

Planned API:

```rust
let sql = db.dump_sql(PgDumpOptions::default())?;
restored.exec(&sql, None)?;
```

The asset remains hidden from the public API until the CLI and public Rust
surface are wired to the same tested runner.
