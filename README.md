# ONQL Rust Driver

Official Rust client for the ONQL database server.

## Installation

### From crates.io

```toml
[dependencies]
onql-client = "0.1"
```

### From GitHub (latest `main`)

```toml
[dependencies]
onql-client = { git = "https://github.com/ONQL/onqlclient-rust" }
```

### Pinned to a release tag

```toml
[dependencies]
onql-client = { git = "https://github.com/ONQL/onqlclient-rust", tag = "v0.1.0" }
```

## Quick Start

```rust
use onql_client::ONQLClient;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = ONQLClient::connect("localhost", 5656).await?;

    client.insert("mydb", "users",
        &serde_json::json!({ "id": "u1", "name": "John", "age": 30 })).await?;

    let rows = client.onql("mydb.users[age>18]").await?;
    println!("{rows}");

    let q = ONQLClient::build(
        "mydb.users[id=$1].id",
        &[serde_json::json!("u1")]);
    client.update("mydb", "users",
        &serde_json::json!({ "age": 31 }), &q).await?;

    client.delete_with::<&str>("mydb", "users", "", "default", &["u1"]).await?;

    client.close().await?;
    Ok(())
}
```

## API Reference

### `ONQLClient::connect(host, port) -> Result<ONQLClient>`

Creates and returns a connected client.

### `client.send_request(keyword, payload) -> Result<Response>`

Sends a raw request frame and waits for a response.

### `client.close() -> Result<()>`

Closes the connection.

## Direct ORM-style API

On top of raw `send_request`, the client exposes convenience methods for the
`insert` / `update` / `delete` / `onql` operations. Each one builds the
standard payload envelope for you and unwraps the `{error, data}` response —
returning an `Err(Error::Protocol)` on a non-empty `error` field, or the
decoded `data` (a `serde_json::Value`) on success.

`db` is passed explicitly to `insert` / `update` / `delete`. `onql` takes a
fully-qualified ONQL expression.

`query` arguments are **ONQL expression strings**, e.g.
`mydb.users[id="u1"].id`.

### `client.insert(db, table, data) -> Result<serde_json::Value>`

Insert a **single** record. `data` is any `serde::Serialize`.

```rust
use serde::Serialize;

#[derive(Serialize)]
struct User { id: String, name: String, age: u32 }

client.insert("mydb", "users",
    &User { id: "u1".into(), name: "John".into(), age: 30 }).await?;
```

### `client.update(db, table, data, query)` / `client.update_with(db, table, data, query, protopass, ids)`

Update records matching `query` (or `ids`).

```rust
client.update("mydb", "users",
    &serde_json::json!({ "age": 31 }),
    "mydb.users[id=\"u1\"].id").await?;

client.update_with::<_, &str>("mydb", "users",
    &serde_json::json!({ "age": 31 }),
    "", "default", &["u1"]).await?;
```

### `client.delete(db, table, query)` / `client.delete_with(db, table, query, protopass, ids)`

Delete records matching `query` (or `ids`).

```rust
client.delete("mydb", "users", "mydb.users[id=\"u1\"].id").await?;

client.delete_with::<&str>("mydb", "users", "", "default", &["u1"]).await?;
```

### `client.onql(query)` / `client.onql_with(query, protopass, ctxkey, ctxvalues)`

Run a raw ONQL query.

```rust
let rows = client.onql("mydb.users[age>18]").await?;
```

### `ONQLClient::build(query, values) -> String`

Replace `$1`, `$2`, … placeholders with `serde_json::Value`s.

```rust
let q = ONQLClient::build(
    "mydb.users[name=$1 and age>$2]",
    &[serde_json::json!("John"), serde_json::json!(18)],
);
let rows = client.onql(&q).await?;
```

### `ONQLClient::process_result(raw) -> Result<serde_json::Value>`

Static helper that parses the `{error, data}` envelope.

## Protocol

```
<request_id>\x1E<keyword>\x1E<payload>\x04
```

- `\x1E` — field delimiter
- `\x04` — end-of-message marker

## License

MIT
