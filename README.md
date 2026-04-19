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

    client.insert("mydb.users",
        &serde_json::json!({ "id": "u1", "name": "John", "age": 30 })).await?;

    let rows = client.onql("select * from mydb.users where age > 18").await?;
    println!("{rows}");

    client.update("mydb.users.u1",
        &serde_json::json!({ "age": 31 })).await?;
    client.delete("mydb.users.u1").await?;

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

The `path` argument is a **dotted string**:

| Path shape | Meaning |
|------------|---------|
| `"mydb.users"` | Table (used by `insert`) |
| `"mydb.users.u1"` | Record id `u1` (used by `update` / `delete`) |

### `client.insert(path, data) -> Result<serde_json::Value>`

Insert a **single** record. `data` is any `serde::Serialize`.

```rust
use serde::Serialize;

#[derive(Serialize)]
struct User { id: String, name: String, age: u32 }

client.insert("mydb.users",
    &User { id: "u1".into(), name: "John".into(), age: 30 }).await?;
```

### `client.update(path, data)` / `client.update_with(path, data, protopass)`

Update the record at `path`.

```rust
client.update("mydb.users.u1",
    &serde_json::json!({ "age": 31 })).await?;

client.update_with("mydb.users.u1",
    &serde_json::json!({ "active": false }),
    "admin").await?;
```

### `client.delete(path)` / `client.delete_with(path, protopass)`

Delete the record at `path`.

```rust
client.delete("mydb.users.u1").await?;
```

### `client.onql(query)` / `client.onql_with(query, protopass, ctxkey, ctxvalues)`

Run a raw ONQL query.

```rust
let rows = client.onql("select * from mydb.users where age > 18").await?;
```

### `ONQLClient::build(query, values) -> String`

Replace `$1`, `$2`, … placeholders with `serde_json::Value`s. Strings are
automatically double-quoted; numbers and booleans are inlined verbatim.

```rust
let q = ONQLClient::build(
    "select * from mydb.users where name = $1 and age > $2",
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
