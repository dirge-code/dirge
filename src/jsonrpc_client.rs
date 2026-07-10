//! Generic request/response correlation over a framed JSON-RPC-style transport,
//! shared by the LSP ([`crate::lsp::rpc::RpcClient`]) and DAP
//! ([`crate::dap::client::DapRpc`]) clients.
//!
//! Both are mechanically the same stack: allocate a monotonic id, register a
//! pending [`oneshot`] sender, write a framed request, and run a background
//! read loop that routes each incoming frame either to a waiting request (by
//! correlation id) or to a registered notification/event handler. They differ
//! only in (a) how an incoming frame is classified as response vs notification,
//! (b) the envelope shape of an outgoing request/notification, and (c) the
//! concrete error type surfaced to callers. Those differences live in the
//! [`Protocol`] impl; this module owns everything else — including the single
//! drain-on-close path (dirge-syom) that used to be duplicated in both read
//! loops.
//!
//! Built on [`crate::jsonrpc_framing`] for the wire format.

use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncWrite};
use tokio::sync::{Mutex, oneshot};
use tokio::task::JoinHandle;
use tokio::time::timeout;

use crate::jsonrpc_framing::{decode_frame, encode_frame};

/// Cap on how long a single frame write to the peer may block. The per-request
/// `timeout` only covers the *response* (`rx`); without this, a wedged peer
/// that stops draining its stdin (full pipe) would block every caller on
/// `writer.lock()` + `write_all` indefinitely, since the writer mutex is held
/// across the await. On expiry the write future is dropped, releasing the lock.
///
/// Applied uniformly to LSP and DAP: for DAP this preserves the pre-existing
/// [`crate::dap::client`] write cap, and for LSP it is a safe, bounded
/// improvement over the previous unbounded write.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// Concrete error type surfaced to a caller of a correlation client.
/// Implemented by each protocol's own `RpcError` so callers keep matching their
/// existing variants rather than a new shared enum.
pub(crate) trait RpcErr: From<io::Error> + From<serde_json::Error> + Send + 'static {
    /// Error returned when the transport closes before a response arrives, or
    /// after the read loop has marked the client closed.
    fn connection_closed() -> Self;
    /// Error returned when a request or a frame write exceeds its deadline.
    fn timeout(duration: Duration) -> Self;
}

/// How an incoming framed message should be routed by the read loop.
pub(crate) enum Incoming<E> {
    /// A response: resolve the pending request waiting on `id` with `result`.
    Response { id: u64, result: Result<Value, E> },
    /// A notification/event: dispatch `body` to the handler registered under
    /// `key`.
    Notify { key: String, body: Value },
    /// A server→client request that the protocol wants acknowledged on the
    /// wire. The generic writes `ack` as a framed reply. Only LSP produces
    /// this today (it auto-acks reverse requests with a null result); DAP
    /// classifies anything it doesn't model as [`Incoming::Ignore`].
    ReverseRequest { ack: Value },
    /// Drop the message.
    Ignore,
}

/// Protocol-specific classification + envelope construction. The generic
/// correlation client is parameterized by an impl of this trait.
pub(crate) trait Protocol: 'static {
    type Error: RpcErr;

    /// Short name used as the tracing log prefix, e.g. `"lsp"`, `"dap"`.
    fn name() -> &'static str;

    /// Build an outgoing request envelope for `method`/`params`, stamped with
    /// the generic-allocated correlation `id`.
    fn build_request(id: u64, method: &str, params: Value) -> Value;

    /// Build an outgoing notification envelope. `id` is allocated by the
    /// generic so protocols whose notifications carry a sequence number (DAP,
    /// which frames notifications as requests) can stamp it; protocols whose
    /// notifications carry no id (LSP) simply ignore it.
    fn build_notification(id: u64, method: &str, params: Value) -> Value;

    /// Classify an incoming decoded message.
    fn classify(msg: &Value) -> Incoming<Self::Error>;
}

type Pending<E> = HashMap<u64, oneshot::Sender<Result<Value, E>>>;
type Handler = Arc<dyn Fn(Value) + Send + Sync>;

/// Shared correlation state. Fields are `pub(crate)` so the thin LSP/DAP
/// adapter structs — and their behavior-preservation tests — can reach the same
/// fields they did before extraction (e.g. inspecting `pending`).
pub(crate) struct Inner<E> {
    pub(crate) next_id: AtomicU64,
    pub(crate) pending: Mutex<Pending<E>>,
    pub(crate) handlers: Mutex<HashMap<String, Handler>>,
    pub(crate) writer: Mutex<Box<dyn AsyncWrite + Send + Unpin>>,
    pub(crate) closed: AtomicBool,
}

/// Spawn the background read loop over `reader`/`writer` and return the shared
/// [`Inner`] handle plus the reader's [`JoinHandle`] (it ends when the peer
/// closes the stream).
pub(crate) fn new<P, R, W>(
    reader: R,
    writer: W,
) -> (Arc<Inner<P::Error>>, JoinHandle<io::Result<()>>)
where
    P: Protocol,
    R: AsyncBufRead + Send + Unpin + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
{
    let inner = Arc::new(Inner::<P::Error> {
        next_id: AtomicU64::new(1),
        pending: Mutex::new(HashMap::new()),
        handlers: Mutex::new(HashMap::new()),
        writer: Mutex::new(Box::new(writer)),
        closed: AtomicBool::new(false),
    });
    let task = tokio::spawn(read_loop::<P, R>(inner.clone(), reader));
    (inner, task)
}

/// A failed or timed-out frame write may have left a partial frame in the
/// pipe; a Content-Length stream cannot resync after that. Mark the client
/// closed and fail every pending waiter so no later request writes over the
/// desynced stream and in-flight callers don't burn their full timeout.
/// (dirge-j0zx)
async fn fail_transport<E: RpcErr>(inner: &Inner<E>) {
    inner.closed.store(true, Ordering::SeqCst);
    let mut pending = inner.pending.lock().await;
    for (_, sender) in pending.drain() {
        let _ = sender.send(Err(E::connection_closed()));
    }
}

/// Send a request and await its response. Shared by the LSP/DAP adapters.
///
/// Tiny race window if a peer close interleaves with a request: the `closed`
/// check + insert + write are not atomic against the read loop draining pending
/// entries on EOF. In that case the request waits for its own timeout rather
/// than failing instantly with `connection_closed()`. Callers should treat both
/// terminations as terminal.
pub(crate) async fn request<P, Params, R>(
    inner: &Inner<P::Error>,
    method: &str,
    params: Params,
    request_timeout: Duration,
) -> Result<R, P::Error>
where
    P: Protocol,
    Params: serde::Serialize,
    R: serde::de::DeserializeOwned,
{
    if inner.closed.load(Ordering::SeqCst) {
        return Err(P::Error::connection_closed());
    }
    let id = inner.next_id.fetch_add(1, Ordering::SeqCst);
    let (tx, rx) = oneshot::channel();
    inner.pending.lock().await.insert(id, tx);

    let body = P::build_request(id, method, serde_json::to_value(params)?);
    let bytes = serde_json::to_vec(&body)?;
    let send_result = timeout(WRITE_TIMEOUT, async {
        let mut writer = inner.writer.lock().await;
        encode_frame(&mut *writer, &bytes).await
    })
    .await;
    match send_result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            fail_transport(inner).await;
            return Err(P::Error::from(e));
        }
        Err(_) => {
            fail_transport(inner).await;
            return Err(P::Error::timeout(WRITE_TIMEOUT));
        }
    }

    let value = match timeout(request_timeout, rx).await {
        Ok(Ok(result)) => result?,
        Ok(Err(_)) => {
            inner.pending.lock().await.remove(&id);
            return Err(P::Error::connection_closed());
        }
        Err(_) => {
            inner.pending.lock().await.remove(&id);
            return Err(P::Error::timeout(request_timeout));
        }
    };
    Ok(serde_json::from_value(value)?)
}

/// Fire-and-forget notification.
pub(crate) async fn notify<P, Params>(
    inner: &Inner<P::Error>,
    method: &str,
    params: Params,
) -> Result<(), P::Error>
where
    P: Protocol,
    Params: serde::Serialize,
{
    if inner.closed.load(Ordering::SeqCst) {
        return Err(P::Error::connection_closed());
    }
    let id = inner.next_id.fetch_add(1, Ordering::SeqCst);
    let body = P::build_notification(id, method, serde_json::to_value(params)?);
    let bytes = serde_json::to_vec(&body)?;
    match timeout(WRITE_TIMEOUT, async {
        let mut writer = inner.writer.lock().await;
        encode_frame(&mut *writer, &bytes).await
    })
    .await
    {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => {
            fail_transport(inner).await;
            Err(P::Error::from(e))
        }
        Err(_) => {
            fail_transport(inner).await;
            Err(P::Error::timeout(WRITE_TIMEOUT))
        }
    }
}

/// Register a handler for an incoming notification/event keyed by `method`.
/// Replaces any previously-registered handler for the same key.
pub(crate) async fn register_notification<E>(inner: &Inner<E>, method: &str, handler: Handler) {
    inner
        .handlers
        .lock()
        .await
        .insert(method.to_string(), handler);
}

/// The single shared read loop. Pumps framed messages, classifies each via
/// [`Protocol::classify`], and routes it. On EOF or a non-EOF decode error it
/// marks the client closed and drains every pending waiter with
/// `connection_closed()` (dirge-syom) so in-flight requests fail promptly
/// instead of burning their full response timeout.
pub(crate) async fn read_loop<P, R>(inner: Arc<Inner<P::Error>>, mut reader: R) -> io::Result<()>
where
    P: Protocol,
    R: AsyncBufRead + Send + Unpin,
{
    let name = P::name();
    let mut exit_err: Option<io::Error> = None;
    loop {
        let frame = match decode_frame(&mut reader).await {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                // Clean shutdown — peer closed.
                break;
            }
            Err(e) => {
                tracing::warn!("{name}: read loop aborting on decode error: {e}");
                exit_err = Some(e);
                break;
            }
        };
        let msg: Value = match serde_json::from_slice(&frame) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("{name}: skipping non-JSON frame: {e}");
                continue;
            }
        };
        dispatch::<P>(&inner, msg).await;
    }
    // Stream closed — fail any pending requests and mark closed.
    inner.closed.store(true, Ordering::SeqCst);
    let mut pending = inner.pending.lock().await;
    for (_, sender) in pending.drain() {
        let _ = sender.send(Err(P::Error::connection_closed()));
    }
    drop(pending);
    match exit_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

async fn dispatch<P: Protocol>(inner: &Arc<Inner<P::Error>>, msg: Value) {
    match P::classify(&msg) {
        Incoming::Response { id, result } => {
            let sender = inner.pending.lock().await.remove(&id);
            if let Some(sender) = sender {
                let _ = sender.send(result);
            }
        }
        Incoming::Notify { key, body } => {
            // Clone the handler and release the lock before invoking, so a
            // slow or re-entrant handler can't stall the read loop or deadlock
            // by re-locking `handlers`.
            let handler = inner.handlers.lock().await.get(&key).cloned();
            if let Some(handler) = handler {
                handler(body);
            }
        }
        Incoming::ReverseRequest { ack } => {
            if let Ok(bytes) = serde_json::to_vec(&ack) {
                // dirge-dxpn: bound the ack write with WRITE_TIMEOUT like
                // every other write. This ack is issued FROM the read loop
                // while holding the writer mutex; an unbounded write to a
                // peer whose stdin pipe is full (it is flooding stdout, not
                // draining) blocks the read loop forever — reading stops, so
                // the peer never drains its stdin, a mutual pipe deadlock
                // that wedges the connection permanently. On expiry the
                // write future is dropped, releasing the lock so the loop
                // resumes reading.
                match timeout(WRITE_TIMEOUT, async {
                    let mut writer = inner.writer.lock().await;
                    encode_frame(&mut *writer, &bytes).await
                })
                .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        tracing::warn!("{}: reverse-request ack write failed: {e}", P::name());
                    }
                    Err(_) => {
                        tracing::warn!(
                            "{}: reverse-request ack write timed out after {:?}; \
                             dropping ack to unblock the read loop",
                            P::name(),
                            WRITE_TIMEOUT,
                        );
                    }
                }
            }
        }
        Incoming::Ignore => {
            tracing::warn!("{}: ignoring frame", P::name());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::io::{AsyncRead, AsyncWrite, BufReader, ReadBuf};

    /// A writer whose writes never complete — models a peer whose stdin
    /// pipe is full (it is flooding stdout, not draining it). A framed
    /// write parks on it forever (dirge-dxpn).
    struct BlockingWriter;
    impl AsyncWrite for BlockingWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            _: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Pending
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Pending
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[derive(Debug, PartialEq)]
    enum TestErr {
        Closed,
        Timeout,
        Io,
        Serde,
    }

    impl From<io::Error> for TestErr {
        fn from(_: io::Error) -> Self {
            TestErr::Io
        }
    }
    impl From<serde_json::Error> for TestErr {
        fn from(_: serde_json::Error) -> Self {
            TestErr::Serde
        }
    }
    impl RpcErr for TestErr {
        fn connection_closed() -> Self {
            TestErr::Closed
        }
        fn timeout(_d: Duration) -> Self {
            TestErr::Timeout
        }
    }

    struct TestProto;
    impl Protocol for TestProto {
        type Error = TestErr;
        fn name() -> &'static str {
            "test"
        }
        fn build_request(id: u64, method: &str, params: Value) -> Value {
            serde_json::json!({"id": id, "method": method, "params": params})
        }
        fn build_notification(id: u64, method: &str, params: Value) -> Value {
            serde_json::json!({"id": id, "method": method, "params": params})
        }
        // Classify anything as a reverse request so the dxpn ack test can
        // drive `dispatch` directly. `write_failure_closes_transport` parks
        // its reader, so `classify` is never reached there.
        fn classify(_msg: &Value) -> Incoming<TestErr> {
            Incoming::ReverseRequest {
                ack: serde_json::json!({"id": 1, "result": null}),
            }
        }
    }

    /// Always fails a write so `request`/`notify` hit their write-failure arm.
    struct FailingWriter;
    impl AsyncWrite for FailingWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Err(io::Error::new(io::ErrorKind::BrokenPipe, "boom")))
        }
        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    /// Never yields bytes and never returns EOF, so the read loop parks
    /// forever and never sets `closed` itself — the test must observe the
    /// write path setting it.
    struct ParkedReader;
    impl AsyncRead for ParkedReader {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Pending
        }
    }

    /// dirge-dxpn: the reverse-request ack is written from inside the read
    /// loop while holding the writer mutex. Against a peer whose stdin pipe
    /// is full the write blocks; because reading has stopped, the peer can
    /// never drain — a mutual pipe deadlock that used to wedge the loop
    /// permanently (and made concurrent `request()` callers eat the
    /// writer-lock timeout). The ack write is now bounded by WRITE_TIMEOUT,
    /// so `dispatch` returns (dropping the ack) instead of hanging.
    #[tokio::test(start_paused = true)]
    async fn reverse_request_ack_is_bounded_by_write_timeout() {
        let inner = Arc::new(Inner::<TestErr> {
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            handlers: Mutex::new(HashMap::new()),
            writer: Mutex::new(Box::new(BlockingWriter)),
            closed: AtomicBool::new(false),
        });
        // Guard well past WRITE_TIMEOUT. With the fix the inner deadline
        // fires first (virtual clock) and dispatch completes; without it
        // dispatch parks forever and this outer guard fires → Err.
        let result = tokio::time::timeout(
            WRITE_TIMEOUT + Duration::from_secs(30),
            dispatch::<TestProto>(&inner, Value::Null),
        )
        .await;
        assert!(
            result.is_ok(),
            "reverse-request ack must not block the read loop indefinitely",
        );
    }

    #[tokio::test]
    async fn write_failure_closes_transport() {
        let (inner, task) = new::<TestProto, _, _>(BufReader::new(ParkedReader), FailingWriter);

        // First request hits the failing writer; the transport must be marked
        // closed so no later request writes a fresh frame over the now-desynced
        // Content-Length stream. (dirge-j0zx)
        let r = request::<TestProto, _, Value>(
            &inner,
            "m",
            serde_json::json!({}),
            Duration::from_secs(5),
        )
        .await;
        assert!(r.is_err());

        // This is the assertion that fails on current (unfixed) code.
        assert!(
            inner.closed.load(Ordering::SeqCst),
            "write failure must close the transport"
        );

        // A subsequent request must fail fast with connection_closed() rather
        // than attempting another write over the desynced stream.
        let r2 = request::<TestProto, _, Value>(
            &inner,
            "m",
            serde_json::json!({}),
            Duration::from_secs(5),
        )
        .await;
        assert!(
            matches!(r2, Err(TestErr::Closed)),
            "second request must fail fast as closed"
        );

        task.abort();
    }
}
