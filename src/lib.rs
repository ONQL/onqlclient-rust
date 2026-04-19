use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{oneshot, Mutex};
use tokio::task::JoinHandle;

const EOM: u8 = 0x04;
const DELIMITER: char = '\x1E';

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A parsed response frame from the ONQL server.
#[derive(Debug, Clone)]
pub struct Response {
    pub request_id: String,
    pub source: String,
    pub payload: String,
}

/// Errors returned by the ONQL client.
#[derive(Debug)]
pub enum Error {
    Connection(String),
    Timeout,
    Protocol(String),
    Io(std::io::Error),
    Json(serde_json::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Connection(msg) => write!(f, "connection error: {msg}"),
            Error::Timeout => write!(f, "request timed out"),
            Error::Protocol(msg) => write!(f, "protocol error: {msg}"),
            Error::Io(e) => write!(f, "io error: {e}"),
            Error::Json(e) => write!(f, "json error: {e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self { Error::Io(e) }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self { Error::Json(e) }
}

pub type Result<T> = std::result::Result<T, Error>;

// ---------------------------------------------------------------------------
// Shared inner state
// ---------------------------------------------------------------------------

struct Inner {
    /// One-shot senders keyed by request-id.
    pending: HashMap<String, oneshot::Sender<Response>>,
}

// ---------------------------------------------------------------------------
// ONQLClient
// ---------------------------------------------------------------------------

/// Async client for the ONQL TCP protocol.
pub struct ONQLClient {
    inner: Arc<Mutex<Inner>>,
    writer: Arc<Mutex<tokio::net::tcp::OwnedWriteHalf>>,
    reader_handle: Option<JoinHandle<()>>,
}

impl ONQLClient {
    // ----- construction ----------------------------------------------------

    /// Open a TCP connection to the ONQL server and spawn the background
    /// reader task.
    pub async fn connect(host: &str, port: u16) -> Result<Self> {
        let stream = TcpStream::connect((host, port))
            .await
            .map_err(|e| Error::Connection(format!("could not connect to {host}:{port}: {e}")))?;

        let (read_half, write_half) = stream.into_split();

        let inner = Arc::new(Mutex::new(Inner { pending: HashMap::new() }));

        let reader_inner = Arc::clone(&inner);
        let reader_handle = tokio::spawn(Self::reader_loop(read_half, reader_inner));

        Ok(ONQLClient {
            inner,
            writer: Arc::new(Mutex::new(write_half)),
            reader_handle: Some(reader_handle),
        })
    }

    // ----- background reader -----------------------------------------------

    async fn reader_loop(
        mut reader: tokio::net::tcp::OwnedReadHalf,
        inner: Arc<Mutex<Inner>>,
    ) {
        let mut buf = Vec::with_capacity(16 * 1024);

        loop {
            let mut tmp = [0u8; 8192];
            let n = match reader.read(&mut tmp).await {
                Ok(0) => break,
                Ok(n) => n,
                Err(_) => break,
            };
            buf.extend_from_slice(&tmp[..n]);

            while let Some(eom_pos) = buf.iter().position(|&b| b == EOM) {
                let frame_bytes = buf.drain(..=eom_pos).collect::<Vec<_>>();
                let frame = match std::str::from_utf8(&frame_bytes[..frame_bytes.len() - 1]) {
                    Ok(s) => s.to_owned(),
                    Err(_) => continue,
                };

                let parts: Vec<&str> = frame.splitn(3, DELIMITER).collect();
                if parts.len() != 3 { continue; }
                let rid = parts[0];
                let source = parts[1];
                let payload = parts[2];

                let mut state = inner.lock().await;
                if let Some(tx) = state.pending.remove(rid) {
                    let _ = tx.send(Response {
                        request_id: rid.to_owned(),
                        source: source.to_owned(),
                        payload: payload.to_owned(),
                    });
                }
            }
        }

        let mut state = inner.lock().await;
        state.pending.clear();
    }

    // ----- request / response ----------------------------------------------

    pub async fn send_request(
        &self,
        keyword: &str,
        payload: &str,
    ) -> Result<Response> {
        self.send_request_timeout(keyword, payload, Duration::from_secs(10))
            .await
    }

    pub async fn send_request_timeout(
        &self,
        keyword: &str,
        payload: &str,
        timeout: Duration,
    ) -> Result<Response> {
        let rid = generate_request_id();
        let (tx, rx) = oneshot::channel();

        {
            let mut state = self.inner.lock().await;
            state.pending.insert(rid.clone(), tx);
        }

        let frame = format!("{rid}{DELIMITER}{keyword}{DELIMITER}{payload}");
        let mut wire = frame.into_bytes();
        wire.push(EOM);

        {
            let mut w = self.writer.lock().await;
            w.write_all(&wire).await?;
            w.flush().await?;
        }

        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(resp)) => Ok(resp),
            Ok(Err(_)) => {
                self.inner.lock().await.pending.remove(&rid);
                Err(Error::Connection("connection lost while waiting for response".into()))
            }
            Err(_) => {
                self.inner.lock().await.pending.remove(&rid);
                Err(Error::Timeout)
            }
        }
    }

    // ----- teardown --------------------------------------------------------

    pub async fn close(mut self) -> Result<()> {
        {
            let mut w = self.writer.lock().await;
            let _ = w.shutdown().await;
        }
        if let Some(handle) = self.reader_handle.take() {
            let _ = handle.await;
        }
        {
            let mut state = self.inner.lock().await;
            state.pending.clear();
        }
        Ok(())
    }

    // ----- ORM-style API ---------------------------------------------------
    //
    // `path` is a dotted string:
    //   "mydb.users"        -> table `users` in database `mydb`
    //   "mydb.users.u1"     -> record with id `u1`

    /// Parse the standard `{error, data}` envelope.
    pub fn process_result(raw: &str) -> Result<serde_json::Value> {
        let parsed: serde_json::Value =
            serde_json::from_str(raw).map_err(|_| Error::Protocol(raw.to_owned()))?;
        if let Some(err) = parsed.get("error") {
            let err_str = err.as_str().unwrap_or("").to_string();
            if !err_str.is_empty() {
                return Err(Error::Protocol(err_str));
            }
        }
        Ok(parsed.get("data").cloned().unwrap_or(serde_json::Value::Null))
    }

    /// Insert a single record at `path` (e.g. `"mydb.users"`).
    pub async fn insert<T: serde::Serialize>(
        &self,
        path: &str,
        data: &T,
    ) -> Result<serde_json::Value> {
        let (db, table, _) = parse_path(path, false)?;
        let payload = serde_json::json!({
            "db": db,
            "table": table,
            "records": data,
        })
        .to_string();
        let resp = self.send_request("insert", &payload).await?;
        Self::process_result(&resp.payload)
    }

    /// Update the record at `path` (e.g. `"mydb.users.u1"`).
    pub async fn update<D: serde::Serialize>(
        &self,
        path: &str,
        data: &D,
    ) -> Result<serde_json::Value> {
        self.update_with(path, data, "default").await
    }

    /// Update the record at `path` with a custom `protopass`.
    pub async fn update_with<D: serde::Serialize>(
        &self,
        path: &str,
        data: &D,
        protopass: &str,
    ) -> Result<serde_json::Value> {
        let (db, table, id) = parse_path(path, true)?;
        let payload = serde_json::json!({
            "db": db,
            "table": table,
            "records": data,
            "query": "",
            "protopass": protopass,
            "ids": [id],
        })
        .to_string();
        let resp = self.send_request("update", &payload).await?;
        Self::process_result(&resp.payload)
    }

    /// Delete the record at `path` (e.g. `"mydb.users.u1"`).
    pub async fn delete(&self, path: &str) -> Result<serde_json::Value> {
        self.delete_with(path, "default").await
    }

    pub async fn delete_with(
        &self,
        path: &str,
        protopass: &str,
    ) -> Result<serde_json::Value> {
        let (db, table, id) = parse_path(path, true)?;
        let payload = serde_json::json!({
            "db": db,
            "table": table,
            "query": "",
            "protopass": protopass,
            "ids": [id],
        })
        .to_string();
        let resp = self.send_request("delete", &payload).await?;
        Self::process_result(&resp.payload)
    }

    /// Execute a raw ONQL query.
    pub async fn onql(&self, query: &str) -> Result<serde_json::Value> {
        self.onql_with(query, "default", "", &[] as &[&str]).await
    }

    pub async fn onql_with<S: serde::Serialize>(
        &self,
        query: &str,
        protopass: &str,
        ctxkey: &str,
        ctxvalues: &[S],
    ) -> Result<serde_json::Value> {
        let payload = serde_json::json!({
            "query": query,
            "protopass": protopass,
            "ctxkey": ctxkey,
            "ctxvalues": ctxvalues,
        })
        .to_string();
        let resp = self.send_request("onql", &payload).await?;
        Self::process_result(&resp.payload)
    }

    /// Replace `$1`, `$2`, ... placeholders with `serde_json::Value`s.
    pub fn build(query: &str, values: &[serde_json::Value]) -> String {
        let mut out = query.to_owned();
        for (i, v) in values.iter().enumerate() {
            let placeholder = format!("${}", i + 1);
            let replacement = match v {
                serde_json::Value::String(s) => format!("\"{s}\""),
                serde_json::Value::Bool(b)   => b.to_string(),
                serde_json::Value::Number(n) => n.to_string(),
                serde_json::Value::Null      => "null".to_owned(),
                other                        => other.to_string(),
            };
            out = out.replace(&placeholder, &replacement);
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parse `"db.table"` or `"db.table.id"` into `(db, table, id)`.
fn parse_path(path: &str, require_id: bool) -> Result<(String, String, String)> {
    if path.is_empty() {
        return Err(Error::Protocol(
            "path must be a non-empty string like \"db.table\" or \"db.table.id\"".into(),
        ));
    }
    let mut parts = path.splitn(3, '.');
    let db = parts.next().unwrap_or("").to_string();
    let table = parts.next().unwrap_or("").to_string();
    let id = parts.next().unwrap_or("").to_string();
    if db.is_empty() || table.is_empty() {
        return Err(Error::Protocol(format!(
            "path \"{path}\" must contain at least \"db.table\""
        )));
    }
    if require_id && id.is_empty() {
        return Err(Error::Protocol(format!(
            "path \"{path}\" must include a record id: \"db.table.id\""
        )));
    }
    Ok((db, table, id))
}

fn generate_request_id() -> String {
    let id = uuid::Uuid::new_v4();
    id.simple().to_string()[..8].to_owned()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_id_is_8_hex_chars() {
        let rid = generate_request_id();
        assert_eq!(rid.len(), 8);
        assert!(rid.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn path_parsing() {
        assert_eq!(parse_path("mydb.users", false).unwrap(),
                   ("mydb".into(), "users".into(), "".into()));
        assert_eq!(parse_path("mydb.users.u1", true).unwrap(),
                   ("mydb".into(), "users".into(), "u1".into()));
        assert!(parse_path("mydb", false).is_err());
        assert!(parse_path("mydb.users", true).is_err());
    }
}
