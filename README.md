# ONQL Rust Driver

Official Rust client for the ONQL database server.

## Installation

### From crates.io

Add to your `Cargo.toml`:

```toml
[dependencies]
onql-client = "0.1"
```

Or via the CLI:

```bash
cargo add onql-client
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

    // Execute a query
    let result = client.send_request("onql",
        r#"{"db":"mydb","table":"users","query":"name = \"John\""}"#
    ).await?;
    println!("{}", result.payload);

    // Subscribe to live updates
    let rid = client.subscribe("", r#"name = "John""#, |rid, keyword, payload| {
        println!("Update: {}", payload);
    }).await?;

    // Unsubscribe
    client.unsubscribe(&rid).await?;

    client.close().await?;
    Ok(())
}
```

## API Reference

### `ONQLClient::connect(host, port) -> Result<ONQLClient>`

Creates and returns a connected client.

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `host` | `&str` | `"localhost"` | Server hostname |
| `port` | `u16` | `5656` | Server port |

### `client.send_request(keyword, payload) -> Result<Response>`

Sends a request and waits for a response.

### `client.subscribe(onquery, query, callback) -> Result<String>`

Opens a streaming subscription. Returns the subscription ID.

### `client.unsubscribe(rid) -> Result<()>`

Stops receiving events for a subscription.

### `client.close() -> Result<()>`

Closes the connection.

## Direct ORM-style API

On top of raw `send_request`, the client exposes convenience methods for the
common `insert` / `update` / `delete` / `onql` operations. Each one builds the
standard payload envelope for you and unwraps the `{error, data}` response
automatically — returning an `Err(Error::Protocol)` on a non-empty `error`
field, or the decoded `data` (a `serde_json::Value`) on success.

Call `client.setup(db)` once to bind a default database name; every subsequent
`insert` / `update` / `delete` call will use it.

### `client.setup(db)`

Sets the default database.

```rust
client.setup("mydb").await;
```

### `client.insert(table, data) -> Result<serde_json::Value>`

Insert one record or a list of records. `data` can be any `serde::Serialize`
type — a struct, a `serde_json::Value`, a map, or a `Vec` of any of those.

```rust
use serde::Serialize;

#[derive(Serialize)]
struct User { name: String, age: u32 }

client.insert("users", &User { name: "John".into(), age: 30 }).await?;
client.insert("users", &serde_json::json!([
    { "name": "A" },
    { "name": "B" }
])).await?;
```

### `client.update(table, data, query) -> Result<serde_json::Value>`
### `client.update_with(table, data, query, protopass, ids) -> Result<serde_json::Value>`

Update records matching `query`. The shorter form uses
`protopass = "default"` and no explicit IDs.

```rust
client.update(
    "users",
    &serde_json::json!({ "age": 31 }),
    &serde_json::json!({ "name": "John" })
).await?;

client.update_with(
    "users",
    &serde_json::json!({ "active": false }),
    &serde_json::json!({ "id": "u1" }),
    "admin",
    &["u1"],
).await?;
```

### `client.delete(table, query) -> Result<serde_json::Value>`
### `client.delete_with(table, query, protopass, ids) -> Result<serde_json::Value>`

Delete records matching `query`. Same semantics as `update` / `update_with`.

```rust
client.delete("users", &serde_json::json!({ "active": false })).await?;
```

### `client.onql(query) -> Result<serde_json::Value>`
### `client.onql_with(query, protopass, ctxkey, ctxvalues) -> Result<serde_json::Value>`

Run a raw ONQL query. The server's `{error, data}` envelope is unwrapped.

```rust
let rows = client.onql("select * from users where age > 18").await?;

let rows = client.onql_with(
    "select * from users where org = $ctxval",
    "default",
    "org",
    &["org-1"],
).await?;
```

### `ONQLClient::build(query, values) -> String`

Replace `$1`, `$2`, … placeholders with `serde_json::Value`s. Strings are
automatically double-quoted; numbers and booleans are inlined verbatim.

```rust
let q = ONQLClient::build(
    "select * from users where name = $1 and age > $2",
    &[serde_json::json!("John"), serde_json::json!(18)],
);
// -> select * from users where name = "John" and age > 18
let rows = client.onql(&q).await?;
```

### `ONQLClient::process_result(raw) -> Result<serde_json::Value>`

Static helper that parses the standard `{error, data}` server envelope.
Returns `Err(Error::Protocol)` on a non-empty `error`; returns the decoded
`data` on success. Useful when you prefer to build payloads yourself.

### Full example

```rust
use onql_client::ONQLClient;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = ONQLClient::connect("localhost", 5656).await?;
    client.setup("mydb").await;

    client.insert("users", &serde_json::json!({ "name": "John", "age": 30 })).await?;

    let q = ONQLClient::build(
        "select * from users where age >= $1",
        &[serde_json::json!(18)],
    );
    let rows = client.onql(&q).await?;
    println!("{rows}");

    client.update(
        "users",
        &serde_json::json!({ "age": 31 }),
        &serde_json::json!({ "name": "John" }),
    ).await?;
    client.delete("users", &serde_json::json!({ "name": "John" })).await?;

    client.close().await?;
    Ok(())
}
```

## Protocol

The client communicates over TCP using a delimiter-based message format:

```
<request_id>\x1E<keyword>\x1E<payload>\x04
```

- `\x1E` — field delimiter
- `\x04` — end-of-message marker

## License

MIT
