# Tauri Usage

Use `pglite-oxide` from Rust state, not from the webview. The main value is a
sidecar-free local Postgres runtime that your commands, background tasks, and
Rust libraries can share.

## Direct Embedded API

Use `Pglite` when your Rust code owns the database calls:

```rust,no_run
use pglite_oxide::Pglite;
use serde_json::json;
use tauri::State;
use std::sync::Mutex;

struct Db(Mutex<Pglite>);

#[tauri::command]
fn add_item(db: State<'_, Db>, value: String) -> Result<(), String> {
    let mut db = db.0.lock().map_err(|err| err.to_string())?;
    db.query(
        "INSERT INTO items(value) VALUES ($1)",
        &[json!(value)],
        None,
    )
    .map_err(|err| err.to_string())?;
    Ok(())
}
```

Open the database under your app data directory during setup:

```rust,no_run
use pglite_oxide::Pglite;

let db = Pglite::builder()
    .app("com", "example", "desktop-app")
    .open()?;
```

## Existing Postgres Clients

Use `PgliteServer` when another crate expects a PostgreSQL URL:

```rust,no_run
use pglite_oxide::PgliteServer;

let server = PgliteServer::builder()
    .path("./.pglite")
    .start()?;

let database_url = server.connection_uri();
```

Configure SQLx, `tokio-postgres`, Diesel, or a framework pool with one
connection. The current runtime is a single embedded backend, not a multi-user
Postgres server.

## Practical Limits

- Keep database access serialized unless you are only using one client
  connection.
- Prefer `Pglite` over `PgliteServer` when you do not need a PostgreSQL URL.
- Use `Pglite::temporary()` or `PgliteServer::temporary_tcp()` for tests; both
  use the template-cluster cache by default.
- Mobile targets need separate validation. The current crate targets desktop
  Rust with Wasmtime.
