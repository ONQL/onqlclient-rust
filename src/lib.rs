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
    /// The TCP connection could not be established or was lost.
    Connection(String),
    /// The server did not reply within the allowed duration.
    Timeout,
    /// A response frame could not be parsed.
    Protocol(String),
    /// Wrapper around an I/O error.
    Io(std::io::Error),
    /// Wrapper around a JSON error.
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
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Json(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

// ---------------------------------------------------------------------------
// Subscription callback
// ---------------------------------------------------------------------------

/// A boxed, thread-safe subscription callback.
///
/// Called with `(request_id, source, payload)` for every frame the server
/// pushes on a given subscription.
pub type SubscriptionCallback =
    Box<dyn Fn(&str, &str, &str) + Send + Sync + 'static>;

// ---------------------------------------------------------------------------
// Shared inner state
// ---------------------------------------------------------------------------

struct Inner {
    /// One-shot senders keyed by request-id, used for request/response pairs.
    pending: HashMap<String, oneshot::Sender<Response>>,
    /// Subscription callbacks keyed by the subscription request-id.
    subscriptions: HashMap<String, SubscriptionCallback>,
}

// ---------------------------------------------------------------------------
// ONQLClient
// ---------------------------------------------------------------------------

/// Async client for the ONQL TCP protocol.
///
/// Spawns a background reader task that demultiplexes incoming frames and
/// dispatches them to the correct pending request or subscription callback.
pub struct ONQLClient {
    inner: Arc<Mutex<Inner>>,
    writer: Arc<Mutex<tokio::net::tcp::OwnedWriteHalf>>,
    reader_handle: Option<JoinHandle<()>>,
    db: Arc<Mutex<String>>,
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

        let inner = Arc::new(Mutex::new(Inner {
            pending: HashMap::new(),
            subscriptions: HashMap::new(),
        }));

        let reader_inner = Arc::clone(&inner);
        let reader_handle = tokio::spawn(Self::reader_loop(read_half, reader_inner));

        Ok(ONQLClient {
            inner,
            writer: Arc::new(Mutex::new(write_half)),
            reader_handle: Some(reader_handle),
            db: Arc::new(Mutex::new(String::new())),
        })
    }

    // ----- background reader -----------------------------------------------

    async fn reader_loop(
        mut reader: tokio::net::tcp::OwnedReadHalf,
        inner: Arc<Mutex<Inner>>,
    ) {
        let mut buf = Vec::with_capacity(16 * 1024);

        loop {
            // Read bytes until we have at least one complete frame (ending
            // with EOM).  We read in a loop because a single `read` may
            // return a partial frame or multiple frames at once.
            let mut tmp = [0u8; 8192];
            let n = match reader.read(&mut tmp).await {
                Ok(0) => break, // EOF – server closed
                Ok(n) => n,
                Err(_) => break,
            };
            buf.extend_from_slice(&tmp[..n]);

            // Process every complete frame in the buffer.
            while let Some(eom_pos) = buf.iter().position(|&b| b == EOM) {
                let frame_bytes = buf.drain(..=eom_pos).collect::<Vec<_>>();
                // Strip the trailing EOM byte.
                let frame = match std::str::from_utf8(&frame_bytes[..frame_bytes.len() - 1]) {
                    Ok(s) => s.to_owned(),
                    Err(_) => continue,
                };

                let parts: Vec<&str> = frame.splitn(3, DELIMITER).collect();
                if parts.len() != 3 {
                    continue;
                }
                let rid = parts[0];
                let source = parts[1];
                let payload = parts[2];

                let mut state = inner.lock().await;

                // Subscription frame?
                if state.subscriptions.contains_key(rid) {
                    if let Some(cb) = state.subscriptions.get(rid) {
                        cb(rid, source, payload);
                    }
                    continue;
                }

                // Normal request/response.
                if let Some(tx) = state.pending.remove(rid) {
                    let _ = tx.send(Response {
                        request_id: rid.to_owned(),
                        source: source.to_owned(),
                        payload: payload.to_owned(),
                    });
                }
            }
        }

        // Connection lost – wake all pending requests with an error.
        let mut state = inner.lock().await;
        // Dropping the senders will cause receivers to get a RecvError,
        // which we translate into a Connection error in `send_request`.
        state.pending.clear();
    }

    // ----- request / response ----------------------------------------------

    /// Send a request and wait for the response, using the default 10-second
    /// timeout.
    pub async fn send_request(
        &self,
        keyword: &str,
        payload: &str,
    ) -> Result<Response> {
        self.send_request_timeout(keyword, payload, Duration::from_secs(10))
            .await
    }

    /// Send a request and wait for the response with a custom timeout.
    pub async fn send_request_timeout(
        &self,
        keyword: &str,
        payload: &str,
        timeout: Duration,
    ) -> Result<Response> {
        let rid = generate_request_id();
        let (tx, rx) = oneshot::channel();

        // Register the pending request.
        {
            let mut state = self.inner.lock().await;
            state.pending.insert(rid.clone(), tx);
        }

        // Build and send the frame.
        let frame = format!("{rid}{DELIMITER}{keyword}{DELIMITER}{payload}");
        let mut wire = frame.into_bytes();
        wire.push(EOM);

        {
            let mut w = self.writer.lock().await;
            w.write_all(&wire).await?;
            w.flush().await?;
        }

        // Wait for the response (or timeout).
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(resp)) => Ok(resp),
            Ok(Err(_)) => {
                // oneshot sender was dropped -> connection lost
                self.inner.lock().await.pending.remove(&rid);
                Err(Error::Connection("connection lost while waiting for response".into()))
            }
            Err(_) => {
                self.inner.lock().await.pending.remove(&rid);
                Err(Error::Timeout)
            }
        }
    }

    // ----- subscribe / unsubscribe -----------------------------------------

    /// Open a streaming subscription.
    ///
    /// Every frame the server pushes for the returned request-id will be
    /// delivered to `callback(rid, source, payload)`.
    ///
    /// Returns the subscription request-id which can be passed to
    /// [`unsubscribe`](Self::unsubscribe) to stop receiving events.
    pub async fn subscribe<F>(
        &self,
        onquery: &str,
        query: &str,
        callback: F,
    ) -> Result<String>
    where
        F: Fn(&str, &str, &str) + Send + Sync + 'static,
    {
        let rid = generate_request_id();

        {
            let mut state = self.inner.lock().await;
            state.subscriptions.insert(rid.clone(), Box::new(callback));
        }

        let payload = serde_json::json!({
            "onquery": onquery,
            "query": query,
        })
        .to_string();

        let frame = format!("{rid}{DELIMITER}subscribe{DELIMITER}{payload}");
        let mut wire = frame.into_bytes();
        wire.push(EOM);

        {
            let mut w = self.writer.lock().await;
            w.write_all(&wire).await?;
            w.flush().await?;
        }

        Ok(rid)
    }

    /// Cancel a streaming subscription.
    ///
    /// Removes the local callback immediately and sends an `unsubscribe`
    /// frame to the server.
    pub async fn unsubscribe(&self, rid: &str) -> Result<()> {
        {
            let mut state = self.inner.lock().await;
            state.subscriptions.remove(rid);
        }

        let payload = serde_json::json!({ "rid": rid }).to_string();
        let frame = format!("{rid}{DELIMITER}unsubscribe{DELIMITER}{payload}");
        let mut wire = frame.into_bytes();
        wire.push(EOM);

        // Best-effort send; if the connection is already gone we just
        // swallow the error (the local callback was already removed).
        let send_result = {
            let mut w = self.writer.lock().await;
            w.write_all(&wire).await.and_then(|_| {
                // Can't call async flush here inline, so we return Ok and
                // flush separately.
                Ok(())
            })
        };

        if let Err(e) = send_result {
            // Connection might be closed already; not an error for unsubscribe.
            eprintln!("unsubscribe frame send failed (ignored): {e}");
        } else {
            // Flush outside the write lock scope isn't needed; write_all
            // already handed bytes to the kernel buffer.  For correctness
            // we flush inside the lock.
            let mut w = self.writer.lock().await;
            let _ = w.flush().await;
        }

        Ok(())
    }

    // ----- ORM-style API ---------------------------------------------------

    /// Set the default database name used by [`insert`](Self::insert),
    /// [`update`](Self::update), [`delete`](Self::delete), and
    /// [`onql`](Self::onql).
    pub async fn setup(&self, db: impl Into<String>) {
        *self.db.lock().await = db.into();
    }

    /// Parse the standard `{error, data}` envelope returned by the server.
    ///
    /// Returns the parsed `data` value. Returns [`Error::Protocol`] if the
    /// `error` field is non-empty, or if the payload is not valid JSON.
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

    /// Insert one record or a list of records into `table`.
    ///
    /// Accepts anything that implements [`serde::Serialize`] — pass a struct,
    /// a `serde_json::Value`, a map, or a `Vec` of any of those.
    pub async fn insert<T: serde::Serialize>(
        &self,
        table: &str,
        data: &T,
    ) -> Result<serde_json::Value> {
        let db = self.db.lock().await.clone();
        let payload = serde_json::json!({
            "db": db,
            "table": table,
            "records": data,
        })
        .to_string();
        let resp = self.send_request("insert", &payload).await?;
        Self::process_result(&resp.payload)
    }

    /// Update records in `table` matching `query`. Uses
    /// `protopass = "default"` and no explicit IDs.
    pub async fn update<D: serde::Serialize, Q: serde::Serialize>(
        &self,
        table: &str,
        data: &D,
        query: &Q,
    ) -> Result<serde_json::Value> {
        self.update_with(table, data, query, "default", &[] as &[&str])
            .await
    }

    /// Update records in `table` matching `query` with a custom `protopass`
    /// and optional explicit `ids`.
    pub async fn update_with<D: serde::Serialize, Q: serde::Serialize, S: serde::Serialize>(
        &self,
        table: &str,
        data: &D,
        query: &Q,
        protopass: &str,
        ids: &[S],
    ) -> Result<serde_json::Value> {
        let db = self.db.lock().await.clone();
        let payload = serde_json::json!({
            "db": db,
            "table": table,
            "records": data,
            "query": query,
            "protopass": protopass,
            "ids": ids,
        })
        .to_string();
        let resp = self.send_request("update", &payload).await?;
        Self::process_result(&resp.payload)
    }

    /// Delete records in `table` matching `query`. Uses
    /// `protopass = "default"` and no explicit IDs.
    pub async fn delete<Q: serde::Serialize>(
        &self,
        table: &str,
        query: &Q,
    ) -> Result<serde_json::Value> {
        self.delete_with(table, query, "default", &[] as &[&str])
            .await
    }

    /// Delete records with a custom `protopass` and optional explicit `ids`.
    pub async fn delete_with<Q: serde::Serialize, S: serde::Serialize>(
        &self,
        table: &str,
        query: &Q,
        protopass: &str,
        ids: &[S],
    ) -> Result<serde_json::Value> {
        let db = self.db.lock().await.clone();
        let payload = serde_json::json!({
            "db": db,
            "table": table,
            "query": query,
            "protopass": protopass,
            "ids": ids,
        })
        .to_string();
        let resp = self.send_request("delete", &payload).await?;
        Self::process_result(&resp.payload)
    }

    /// Execute a raw ONQL query using defaults
    /// (`protopass = "default"`, empty context).
    pub async fn onql(&self, query: &str) -> Result<serde_json::Value> {
        self.onql_with(query, "default", "", &[] as &[&str]).await
    }

    /// Execute a raw ONQL query with a custom proto-pass and context.
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

    /// Replace `$1`, `$2`, ... placeholders in `query` with a slice of
    /// [`serde_json::Value`]s. Strings are rendered as `"..."`, numbers and
    /// booleans are inlined verbatim.
    ///
    /// ```ignore
    /// let q = ONQLClient::build(
    ///     "select * from users where name = $1 and age > $2",
    ///     &[serde_json::json!("John"), serde_json::json!(18)],
    /// );
    /// ```
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

    // ----- teardown --------------------------------------------------------

    /// Gracefully close the connection.
    pub async fn close(mut self) -> Result<()> {
        // Shut down the write half so the server sees EOF.
        {
            let mut w = self.writer.lock().await;
            let _ = w.shutdown().await;
        }

        // Wait for the reader task to finish.
        if let Some(handle) = self.reader_handle.take() {
            let _ = handle.await;
        }

        // Clear any remaining state.
        {
            let mut state = self.inner.lock().await;
            state.pending.clear();
            state.subscriptions.clear();
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Generate a random 8-character hex string, matching the Python driver's
/// `uuid.uuid4().hex[:8]`.
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
    fn frame_encoding() {
        let rid = "abcd1234";
        let keyword = "query";
        let payload = r#"{"q":"SELECT 1"}"#;
        let frame = format!("{rid}{DELIMITER}{keyword}{DELIMITER}{payload}");
        let mut wire = frame.into_bytes();
        wire.push(EOM);

        // Verify delimiter and EOM bytes are present.
        let delim_count = wire.iter().filter(|&&b| b == 0x1E).count();
        assert_eq!(delim_count, 2);
        assert_eq!(*wire.last().unwrap(), EOM);
    }

    #[tokio::test]
    async fn round_trip_through_local_server() {
        // This test requires a running ONQL server; skip gracefully in CI.
        let client = ONQLClient::connect("127.0.0.1", 5656).await;
        if client.is_err() {
            eprintln!("skipping round-trip test: no server on localhost:5656");
            return;
        }
        let client = client.unwrap();
        let resp = client.send_request("ping", "").await;
        let _ = client.close().await;
        assert!(resp.is_ok());
    }
}
