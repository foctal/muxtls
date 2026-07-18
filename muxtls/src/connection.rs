use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use futures_util::{SinkExt, StreamExt};
use muxtls_proto::{ErrorCode as ProtoErrorCode, Frame, VarInt};
use tokio::sync::{Mutex, Notify, OwnedSemaphorePermit, Semaphore, mpsc};
use tokio_util::codec::{Framed, LengthDelimitedCodec};
use tracing::{debug, info, instrument, warn};

use crate::error::{Error, Result};
use crate::limits::Limits;
use crate::stream::{RecvStream, SendStream};

trait IoStream: tokio::io::AsyncRead + tokio::io::AsyncWrite {}
impl<T> IoStream for T where T: tokio::io::AsyncRead + tokio::io::AsyncWrite {}

type BoxIo = Box<dyn IoStream + Unpin + Send + 'static>;

/// Runtime statistics for a connection.
#[derive(Debug, Clone, Copy, Default)]
pub struct ConnectionStats {
    /// Total locally and remotely initiated streams observed.
    pub opened_streams: u64,
    /// Successfully encoded frames handed to the transport.
    pub frames_sent: u64,
    /// Length-delimited frames received from the transport.
    pub frames_received: u64,
    /// Encoded frame bytes sent, excluding length prefixes and TLS overhead.
    pub bytes_sent: u64,
    /// Encoded frame bytes received, excluding length prefixes and TLS overhead.
    pub bytes_received: u64,
}

/// A live multiplexed TLS/TCP connection.
///
/// Dropping the last `Connection` handle initiates connection shutdown. Keep a
/// handle alive while using any streams created from it.
pub struct Connection {
    pub(crate) shared: Arc<ConnectionShared>,
}

pub(crate) struct ConnectionShared {
    pub(crate) limits: Limits,
    pub(crate) local_parity: u64,
    pub(crate) next_local_stream_id: AtomicU64,
    pub(crate) next_remote_stream_id: AtomicU64,
    pub(crate) open_lock: Mutex<()>,
    pub(crate) streams: Mutex<HashMap<u64, Arc<StreamState>>>,
    pub(crate) incoming_stream_tx: mpsc::Sender<u64>,
    pub(crate) incoming_stream_rx: Mutex<mpsc::Receiver<u64>>,
    pub(crate) writer: Arc<WriterState>,
    pub(crate) closed: AtomicBool,
    pub(crate) terminated: AtomicBool,
    pub(crate) close_notify: Notify,
    pub(crate) open_streams: Arc<Semaphore>,
    pub(crate) inbound_conn_bytes: Arc<Semaphore>,
    pub(crate) outbound_conn_bytes: Arc<Semaphore>,
    pub(crate) stats_opened_streams: AtomicU64,
    pub(crate) stats_frames_sent: AtomicU64,
    pub(crate) stats_frames_received: AtomicU64,
    pub(crate) stats_bytes_sent: AtomicU64,
    pub(crate) stats_bytes_received: AtomicU64,
    pub(crate) connection_handles: AtomicUsize,
}

pub(crate) struct StreamState {
    inbound: Mutex<InboundState>,
    inbound_notify: Notify,
    inbound_stream_bytes: Arc<Semaphore>,
    outbound_stream_bytes: Arc<Semaphore>,
    send_lock: Mutex<()>,
    send_terminal: AtomicBool,
    recv_terminal: AtomicBool,
    recv_discarded: AtomicBool,
    send_handles: AtomicUsize,
    recv_handles: AtomicUsize,
    open_permit: Mutex<Option<OwnedSemaphorePermit>>,
}

struct InboundState {
    chunks: VecDeque<InboundChunk>,
    reset_error: Option<u64>,
    fin_received: bool,
    connection_closed: bool,
}

struct InboundChunk {
    data: Bytes,
    _conn_permit: Option<OwnedSemaphorePermit>,
    _stream_permit: Option<OwnedSemaphorePermit>,
}

struct OutboundChunk {
    stream_id: VarInt,
    payload: Bytes,
    fin: bool,
    _conn_permit: Option<OwnedSemaphorePermit>,
    _stream_permit: Option<OwnedSemaphorePermit>,
}

struct WriterQueues {
    by_stream: HashMap<u64, VecDeque<OutboundChunk>>,
    ready: VecDeque<u64>,
    control: VecDeque<Frame>,
    close_frame: Option<Frame>,
    graceful_close: bool,
    closing: bool,
}

pub(crate) struct WriterState {
    queues: Mutex<WriterQueues>,
    notify: Notify,
}

impl Connection {
    pub(crate) fn new<S>(
        stream: S,
        limits: Limits,
        is_client: bool,
        keepalive_interval: Option<Duration>,
        idle_timeout: Option<Duration>,
    ) -> Result<Self>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        limits.validate()?;
        let stream: BoxIo = Box::new(stream);
        let local_parity = if is_client { 0 } else { 1 };
        let next_local_stream_id = AtomicU64::new(local_parity);
        let next_remote_stream_id = AtomicU64::new(1 - local_parity);

        let (incoming_stream_tx, incoming_stream_rx) = mpsc::channel(limits.max_open_streams);

        let shared = Arc::new(ConnectionShared {
            limits: limits.clone(),
            local_parity,
            next_local_stream_id,
            next_remote_stream_id,
            open_lock: Mutex::new(()),
            streams: Mutex::new(HashMap::new()),
            incoming_stream_tx,
            incoming_stream_rx: Mutex::new(incoming_stream_rx),
            writer: Arc::new(WriterState::new()),
            closed: AtomicBool::new(false),
            terminated: AtomicBool::new(false),
            close_notify: Notify::new(),
            open_streams: Arc::new(Semaphore::new(limits.max_open_streams)),
            inbound_conn_bytes: Arc::new(Semaphore::new(limits.max_inbound_connection_bytes)),
            outbound_conn_bytes: Arc::new(Semaphore::new(limits.max_outbound_connection_bytes)),
            stats_opened_streams: AtomicU64::new(0),
            stats_frames_sent: AtomicU64::new(0),
            stats_frames_received: AtomicU64::new(0),
            stats_bytes_sent: AtomicU64::new(0),
            stats_bytes_received: AtomicU64::new(0),
            connection_handles: AtomicUsize::new(1),
        });

        spawn_connection_tasks(
            stream,
            shared.clone(),
            limits.max_frame_size,
            keepalive_interval,
            idle_timeout,
        );

        Ok(Self { shared })
    }

    /// Opens a new bidirectional stream initiated by the local endpoint.
    #[instrument(skip(self), level = "debug")]
    pub async fn open_bi(&self) -> Result<(SendStream, RecvStream)> {
        self.shared.ensure_open()?;
        let _open_guard = self.shared.open_lock.lock().await;
        self.shared.ensure_open()?;

        let permit = self
            .shared
            .open_streams
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| Error::ConnectionClosed)?;

        let stream_id = take_stream_id(&self.shared.next_local_stream_id)?;

        let state = Arc::new(StreamState::new(
            self.shared.limits.max_inbound_stream_bytes,
            self.shared.limits.max_outbound_stream_bytes,
            permit,
        ));

        self.shared
            .streams
            .lock()
            .await
            .insert(stream_id, state.clone());
        let announced = self
            .shared
            .writer
            .enqueue_control(Frame::OpenStream {
                stream_id: VarInt::from_u64(stream_id)
                    .map_err(|e| Error::Protocol(e.to_string()))?,
            })
            .await;
        if !announced {
            self.shared.streams.lock().await.remove(&stream_id);
            return Err(Error::ConnectionClosed);
        }
        self.shared
            .stats_opened_streams
            .fetch_add(1, Ordering::Relaxed);

        debug!(stream_id, "opened local stream");
        Ok((
            SendStream::new(stream_id, state.clone(), self.shared.clone()),
            RecvStream::new(stream_id, state, self.shared.clone()),
        ))
    }

    /// Accepts the next peer-initiated bidirectional stream.
    pub async fn accept_bi(&self) -> Result<(SendStream, RecvStream)> {
        let mut rx = self.shared.incoming_stream_rx.lock().await;
        let closed = self.shared.close_notify.notified();
        self.shared.ensure_open()?;
        let stream_id = tokio::select! {
            stream_id = rx.recv() => stream_id.ok_or(Error::ConnectionClosed)?,
            () = closed => return Err(Error::ConnectionClosed),
        };
        drop(rx);

        let state = {
            let streams = self.shared.streams.lock().await;
            streams
                .get(&stream_id)
                .cloned()
                .ok_or(Error::ConnectionClosed)?
        };

        Ok((
            SendStream::new(stream_id, state.clone(), self.shared.clone()),
            RecvStream::new(stream_id, state, self.shared.clone()),
        ))
    }

    /// Sends a connection close frame and shuts down the connection.
    pub async fn close(&self, reason: impl Into<String>) -> Result<()> {
        let reason = reason.into();
        self.shared.validate_close_reason(0, &reason)?;
        self.shared.initiate_close(0, reason).await
    }

    /// Waits until connection tasks have observed terminal shutdown.
    pub async fn wait_closed(&self) {
        loop {
            let notified = self.shared.close_notify.notified();
            if self.shared.terminated.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }

    /// Returns whether new connection and stream operations are rejected.
    pub fn is_closed(&self) -> bool {
        self.shared.closed.load(Ordering::Acquire)
    }

    /// Returns runtime counters.
    pub fn stats(&self) -> ConnectionStats {
        ConnectionStats {
            opened_streams: self.shared.stats_opened_streams.load(Ordering::Relaxed),
            frames_sent: self.shared.stats_frames_sent.load(Ordering::Relaxed),
            frames_received: self.shared.stats_frames_received.load(Ordering::Relaxed),
            bytes_sent: self.shared.stats_bytes_sent.load(Ordering::Relaxed),
            bytes_received: self.shared.stats_bytes_received.load(Ordering::Relaxed),
        }
    }
}

impl Clone for Connection {
    fn clone(&self) -> Self {
        self.shared
            .connection_handles
            .fetch_add(1, Ordering::Relaxed);
        Self {
            shared: self.shared.clone(),
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        if self
            .shared
            .connection_handles
            .fetch_sub(1, Ordering::AcqRel)
            != 1
        {
            return;
        }

        let shared = self.shared.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let _ = shared
                    .initiate_close(0, "last connection handle dropped".to_owned())
                    .await;
            });
        }
    }
}

impl ConnectionShared {
    pub(crate) fn ensure_open(&self) -> Result<()> {
        if self.closed.load(Ordering::Acquire) {
            Err(Error::ConnectionClosed)
        } else {
            Ok(())
        }
    }

    pub(crate) async fn send_stream_chunk(
        self: &Arc<Self>,
        stream_id: u64,
        state: &Arc<StreamState>,
        payload: Bytes,
        fin: bool,
    ) -> Result<()> {
        self.ensure_open()?;
        let _send_guard = state.send_lock.lock().await;
        self.ensure_open()?;

        if state.send_terminal.load(Ordering::Acquire) {
            return Err(Error::Protocol(
                "stream send side already closed".to_owned(),
            ));
        }

        let proto_stream_id =
            VarInt::from_u64(stream_id).map_err(|e| Error::Protocol(e.to_string()))?;
        let encoded_len = Frame::Stream {
            stream_id: proto_stream_id,
            fin,
            payload: payload.clone(),
        }
        .encoded_len()?;
        if encoded_len > self.limits.max_frame_size {
            return Err(Error::LimitExceeded(format!(
                "encoded stream frame size {encoded_len} exceeds max frame size {}",
                self.limits.max_frame_size
            )));
        }

        let payload_len = payload.len();

        let conn_permit = if payload_len == 0 {
            None
        } else {
            Some(
                self.outbound_conn_bytes
                    .clone()
                    .acquire_many_owned(payload_len as u32)
                    .await
                    .map_err(|_| Error::ConnectionClosed)?,
            )
        };

        let stream_permit = if payload_len == 0 {
            None
        } else {
            Some(
                state
                    .outbound_stream_bytes
                    .clone()
                    .acquire_many_owned(payload_len as u32)
                    .await
                    .map_err(|_| Error::ConnectionClosed)?,
            )
        };

        self.ensure_open()?;
        let enqueued = self
            .writer
            .enqueue_data(
                stream_id,
                OutboundChunk {
                    stream_id: proto_stream_id,
                    payload,
                    fin,
                    _conn_permit: conn_permit,
                    _stream_permit: stream_permit,
                },
            )
            .await;
        if !enqueued {
            return Err(Error::ConnectionClosed);
        }

        if fin {
            state.send_terminal.store(true, Ordering::Release);
            self.try_retire_stream(stream_id).await;
        }

        Ok(())
    }

    pub(crate) async fn reset_stream(
        self: &Arc<Self>,
        stream_id: u64,
        state: &Arc<StreamState>,
        error_code: u64,
    ) -> Result<()> {
        self.ensure_open()?;
        let _send_guard = state.send_lock.lock().await;
        self.ensure_open()?;
        let proto_stream_id =
            VarInt::from_u64(stream_id).map_err(|e| Error::Protocol(e.to_string()))?;
        let error_code = ProtoErrorCode::from_u64(error_code)?;
        if state.send_terminal.swap(true, Ordering::AcqRel) {
            return Ok(());
        }

        let enqueued = self
            .writer
            .enqueue_control(Frame::ResetStream {
                stream_id: proto_stream_id,
                error_code,
            })
            .await;
        if !enqueued {
            return Err(Error::ConnectionClosed);
        }
        self.try_retire_stream(stream_id).await;
        Ok(())
    }

    pub(crate) async fn handle_last_send_drop(
        self: &Arc<Self>,
        stream_id: u64,
        state: &Arc<StreamState>,
    ) {
        let _send_guard = state.send_lock.lock().await;
        if !state.send_terminal.swap(true, Ordering::AcqRel)
            && !self.closed.load(Ordering::Acquire)
            && let (Ok(stream_id), Ok(error_code)) =
                (VarInt::from_u64(stream_id), ProtoErrorCode::from_u64(0))
        {
            let _ = self
                .writer
                .enqueue_control(Frame::ResetStream {
                    stream_id,
                    error_code,
                })
                .await;
        }

        self.try_retire_stream(stream_id).await;
    }

    pub(crate) async fn handle_last_recv_drop(
        self: &Arc<Self>,
        stream_id: u64,
        state: &Arc<StreamState>,
    ) {
        state.discard_inbound().await;
        self.try_retire_stream(stream_id).await;
    }

    pub(crate) async fn initiate_close(
        self: &Arc<Self>,
        error_code: u64,
        reason: String,
    ) -> Result<()> {
        let reason = self.fit_close_reason(error_code, reason)?;
        if self.closed.swap(true, Ordering::AcqRel) {
            return Ok(());
        }

        info!(error_code, reason = %reason, "closing connection");
        self.writer
            .enqueue_close(Frame::ConnectionClose {
                error_code: ProtoErrorCode::from_u64(error_code)?,
                reason,
            })
            .await;
        Ok(())
    }

    fn validate_close_reason(&self, error_code: u64, reason: &str) -> Result<()> {
        let error_code = ProtoErrorCode::from_u64(error_code)?;
        let reason_len = u64::try_from(reason.len())
            .ok()
            .and_then(|len| VarInt::from_u64(len).ok())
            .ok_or_else(|| Error::LimitExceeded("close reason is too large".to_owned()))?;
        let encoded_len = 1 + error_code.encoded_len() + reason_len.encoded_len() + reason.len();
        if encoded_len > self.limits.max_frame_size {
            return Err(Error::LimitExceeded(format!(
                "encoded close frame size {encoded_len} exceeds max frame size {}",
                self.limits.max_frame_size
            )));
        }
        Ok(())
    }

    fn fit_close_reason(&self, error_code: u64, reason: String) -> Result<String> {
        if self.validate_close_reason(error_code, &reason).is_ok() {
            return Ok(reason);
        }

        let fallback = "connection error".to_owned();
        if self.validate_close_reason(error_code, &fallback).is_ok() {
            Ok(fallback)
        } else {
            self.validate_close_reason(error_code, "")?;
            Ok(String::new())
        }
    }

    pub(crate) fn max_stream_payload(&self, stream_id: u64) -> usize {
        let mut low = 0usize;
        let mut high = self.limits.max_frame_size.min(u32::MAX as usize);
        while low < high {
            let middle = low + (high - low).div_ceil(2);
            let fits = VarInt::from_u64(stream_id)
                .ok()
                .and_then(|stream_id| {
                    let payload_len = u64::try_from(middle)
                        .ok()
                        .and_then(|len| VarInt::from_u64(len).ok())?;
                    Some(1 + stream_id.encoded_len() + 1 + payload_len.encoded_len() + middle)
                })
                .is_some_and(|len| len <= self.limits.max_frame_size);
            if fits {
                low = middle;
            } else {
                high = middle - 1;
            }
        }
        low
    }

    async fn on_remote_stream_frame(
        self: &Arc<Self>,
        stream_id: u64,
        payload: Bytes,
        fin: bool,
    ) -> Result<()> {
        let state = self
            .streams
            .lock()
            .await
            .get(&stream_id)
            .cloned()
            .ok_or_else(|| Error::Protocol(format!("data for unknown stream id {stream_id}")))?;

        ensure_peer_send_open(&state, stream_id, "stream frame")?;

        if !payload.is_empty() {
            state.push_inbound(self, payload).await?;
        }

        if fin {
            state.mark_recv_terminal().await;
            self.try_retire_stream(stream_id).await;
        }

        Ok(())
    }

    async fn on_remote_reset(self: &Arc<Self>, stream_id: u64, error_code: u64) -> Result<()> {
        let state = self
            .streams
            .lock()
            .await
            .get(&stream_id)
            .cloned()
            .ok_or_else(|| Error::Protocol(format!("reset for unknown stream id {stream_id}")))?;

        ensure_peer_send_open(&state, stream_id, "reset")?;

        state.mark_reset(error_code).await;
        state.mark_recv_terminal().await;
        self.try_retire_stream(stream_id).await;
        Ok(())
    }

    async fn on_remote_open(self: &Arc<Self>, stream_id: u64) -> Result<()> {
        if stream_id % 2 == self.local_parity {
            return Err(Error::Protocol(format!(
                "peer opened stream with invalid parity: {stream_id}"
            )));
        }

        let next_expected = self.next_remote_stream_id.load(Ordering::Acquire);
        if stream_id != next_expected {
            return Err(Error::Protocol(format!(
                "expected peer stream id {next_expected}, received {stream_id}"
            )));
        }
        let next = stream_id + 2;

        let permit = self
            .open_streams
            .clone()
            .try_acquire_owned()
            .map_err(|_| Error::LimitExceeded("max open streams reached".to_owned()))?;

        let state = Arc::new(StreamState::new(
            self.limits.max_inbound_stream_bytes,
            self.limits.max_outbound_stream_bytes,
            permit,
        ));

        {
            let mut streams = self.streams.lock().await;
            if streams.contains_key(&stream_id) {
                return Err(Error::Protocol(format!(
                    "peer reused active stream id {stream_id}"
                )));
            }
            streams.insert(stream_id, state.clone());
        }
        self.next_remote_stream_id.store(next, Ordering::Release);

        self.stats_opened_streams.fetch_add(1, Ordering::Relaxed);

        if self.incoming_stream_tx.send(stream_id).await.is_err() {
            return Err(Error::ConnectionClosed);
        }

        debug!(stream_id, "accepted remote stream");
        Ok(())
    }

    async fn try_retire_stream(&self, stream_id: u64) {
        let maybe_state = {
            let streams = self.streams.lock().await;
            streams.get(&stream_id).cloned()
        };

        let Some(state) = maybe_state else {
            return;
        };

        if !state.send_terminal.load(Ordering::Acquire)
            || !state.recv_terminal.load(Ordering::Acquire)
        {
            return;
        }

        state.release_open_permit().await;
        let mut streams = self.streams.lock().await;
        if let Some(current) = streams.get(&stream_id)
            && Arc::ptr_eq(current, &state)
            && state.send_terminal.load(Ordering::Acquire)
            && state.recv_terminal.load(Ordering::Acquire)
        {
            streams.remove(&stream_id);
        }
        debug!(stream_id, "stream reached terminal state");
    }

    async fn mark_closed(&self) {
        self.closed.store(true, Ordering::Release);
        if self.terminated.swap(true, Ordering::AcqRel) {
            return;
        }
        self.open_streams.close();
        self.inbound_conn_bytes.close();
        self.outbound_conn_bytes.close();
        self.writer.shutdown().await;
        self.close_notify.notify_waiters();
        let streams = {
            let mut streams = self.streams.lock().await;
            std::mem::take(&mut *streams)
        };
        for (_, stream) in streams {
            stream.mark_connection_closed().await;
        }
    }

    fn record_sent(&self, payload_len: usize) {
        self.stats_frames_sent.fetch_add(1, Ordering::Relaxed);
        self.stats_bytes_sent
            .fetch_add(payload_len as u64, Ordering::Relaxed);
    }

    fn record_received(&self, payload_len: usize) {
        self.stats_frames_received.fetch_add(1, Ordering::Relaxed);
        self.stats_bytes_received
            .fetch_add(payload_len as u64, Ordering::Relaxed);
    }
}

impl WriterState {
    fn new() -> Self {
        Self {
            queues: Mutex::new(WriterQueues {
                by_stream: HashMap::new(),
                ready: VecDeque::new(),
                control: VecDeque::new(),
                close_frame: None,
                graceful_close: false,
                closing: false,
            }),
            notify: Notify::new(),
        }
    }

    async fn enqueue_data(&self, stream_id: u64, chunk: OutboundChunk) -> bool {
        let mut queues = self.queues.lock().await;
        if queues.closing {
            return false;
        }
        let q = queues.by_stream.entry(stream_id).or_default();
        let was_empty = q.is_empty();
        q.push_back(chunk);
        if was_empty {
            queues.ready.push_back(stream_id);
        }
        drop(queues);
        self.notify.notify_one();
        true
    }

    async fn enqueue_control(&self, frame: Frame) -> bool {
        let mut queues = self.queues.lock().await;
        if queues.closing {
            return false;
        }
        queues.control.push_back(frame);
        drop(queues);
        self.notify.notify_one();
        true
    }

    async fn enqueue_close(&self, frame: Frame) {
        let mut queues = self.queues.lock().await;
        let graceful = matches!(
            &frame,
            Frame::ConnectionClose {
                error_code,
                ..
            } if error_code.into_inner() == 0
        );
        if !graceful {
            queues.by_stream.clear();
            queues.ready.clear();
            queues.control.clear();
        }
        queues.close_frame = Some(frame);
        queues.graceful_close = graceful;
        queues.closing = true;
        drop(queues);
        self.notify.notify_waiters();
    }

    async fn shutdown(&self) {
        let mut queues = self.queues.lock().await;
        queues.by_stream.clear();
        queues.ready.clear();
        queues.control.clear();
        queues.graceful_close = false;
        queues.closing = true;
        drop(queues);
        self.notify.notify_waiters();
    }

    async fn next_frame(&self) -> Option<Frame> {
        loop {
            let notified = self.notify.notified();
            let mut queues = self.queues.lock().await;

            if queues.closing
                && !queues.graceful_close
                && let Some(frame) = queues.close_frame.take()
            {
                return Some(frame);
            }

            if let Some(frame) = queues.control.pop_front() {
                return Some(frame);
            }

            if let Some(stream_id) = queues.ready.pop_front()
                && let Some(q) = queues.by_stream.get_mut(&stream_id)
                && let Some(chunk) = q.pop_front()
            {
                let queue_empty = q.is_empty();
                if queue_empty {
                    queues.by_stream.remove(&stream_id);
                } else {
                    queues.ready.push_back(stream_id);
                }

                return Some(Frame::Stream {
                    stream_id: chunk.stream_id,
                    fin: chunk.fin,
                    payload: chunk.payload,
                });
            }

            if queues.closing {
                if let Some(frame) = queues.close_frame.take() {
                    return Some(frame);
                }
                return None;
            }

            drop(queues);
            notified.await;
        }
    }
}

impl StreamState {
    fn new(
        max_inbound_stream_bytes: usize,
        max_outbound_stream_bytes: usize,
        permit: OwnedSemaphorePermit,
    ) -> Self {
        Self {
            inbound: Mutex::new(InboundState {
                chunks: VecDeque::new(),
                reset_error: None,
                fin_received: false,
                connection_closed: false,
            }),
            inbound_notify: Notify::new(),
            inbound_stream_bytes: Arc::new(Semaphore::new(max_inbound_stream_bytes)),
            outbound_stream_bytes: Arc::new(Semaphore::new(max_outbound_stream_bytes)),
            send_lock: Mutex::new(()),
            send_terminal: AtomicBool::new(false),
            recv_terminal: AtomicBool::new(false),
            recv_discarded: AtomicBool::new(false),
            send_handles: AtomicUsize::new(0),
            recv_handles: AtomicUsize::new(0),
            open_permit: Mutex::new(Some(permit)),
        }
    }

    pub(crate) fn add_send_handle(&self) {
        self.send_handles.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn add_recv_handle(&self) {
        self.recv_handles.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn release_send_handle(&self) -> bool {
        self.release_handle(&self.send_handles)
    }

    pub(crate) fn release_recv_handle(&self) -> bool {
        self.release_handle(&self.recv_handles)
    }

    fn release_handle(&self, counter: &AtomicUsize) -> bool {
        let mut current = counter.load(Ordering::Acquire);
        loop {
            if current == 0 {
                return false;
            }
            match counter.compare_exchange_weak(
                current,
                current - 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return current == 1,
                Err(observed) => current = observed,
            }
        }
    }

    async fn push_inbound(
        self: &Arc<Self>,
        shared: &Arc<ConnectionShared>,
        payload: Bytes,
    ) -> Result<()> {
        if self.recv_discarded.load(Ordering::Acquire) {
            return Ok(());
        }
        let payload_len = payload.len();

        let conn_permit = if payload_len == 0 {
            None
        } else {
            Some(
                shared
                    .inbound_conn_bytes
                    .clone()
                    .try_acquire_many_owned(payload_len as u32)
                    .map_err(|_| {
                        Error::LimitExceeded(
                            "maximum buffered inbound connection bytes reached".to_owned(),
                        )
                    })?,
            )
        };

        let stream_permit = if payload_len == 0 {
            None
        } else {
            Some(
                self.inbound_stream_bytes
                    .clone()
                    .try_acquire_many_owned(payload_len as u32)
                    .map_err(|_| {
                        Error::LimitExceeded(
                            "maximum buffered inbound stream bytes reached".to_owned(),
                        )
                    })?,
            )
        };

        let mut inbound = self.inbound.lock().await;
        if self.recv_discarded.load(Ordering::Acquire) {
            return Ok(());
        }
        if inbound.fin_received {
            return Err(Error::Protocol("received stream data after FIN".to_owned()));
        }

        inbound.chunks.push_back(InboundChunk {
            data: payload,
            _conn_permit: conn_permit,
            _stream_permit: stream_permit,
        });
        drop(inbound);

        self.inbound_notify.notify_one();
        Ok(())
    }

    async fn mark_reset(&self, error_code: u64) {
        let mut inbound = self.inbound.lock().await;
        inbound.reset_error = Some(error_code);
        inbound.chunks.clear();
        drop(inbound);
        self.inbound_notify.notify_waiters();
    }

    async fn mark_recv_terminal(&self) {
        let mut inbound = self.inbound.lock().await;
        inbound.fin_received = true;
        drop(inbound);
        self.recv_terminal.store(true, Ordering::Release);
        self.inbound_notify.notify_waiters();
    }

    async fn mark_connection_closed(&self) {
        let mut inbound = self.inbound.lock().await;
        inbound.connection_closed = true;
        inbound.chunks.clear();
        drop(inbound);
        self.inbound_stream_bytes.close();
        self.outbound_stream_bytes.close();
        self.inbound_notify.notify_waiters();
    }

    async fn release_open_permit(&self) {
        let mut permit = self.open_permit.lock().await;
        *permit = None;
    }

    async fn discard_inbound(&self) {
        self.recv_discarded.store(true, Ordering::Release);
        let mut inbound = self.inbound.lock().await;
        inbound.chunks.clear();
        drop(inbound);
    }

    pub(crate) async fn read_chunk(&self) -> Result<Option<Bytes>> {
        loop {
            let notified = self.inbound_notify.notified();
            let mut inbound = self.inbound.lock().await;

            if let Some(error_code) = inbound.reset_error {
                return Err(Error::StreamReset(error_code));
            }

            if inbound.connection_closed {
                return Err(Error::ConnectionClosed);
            }

            if let Some(chunk) = inbound.chunks.pop_front() {
                let data = chunk.data.clone();
                return Ok(Some(data));
            }

            if inbound.fin_received || self.recv_terminal.load(Ordering::Acquire) {
                inbound.fin_received = true;
                return Ok(None);
            }

            drop(inbound);
            notified.await;
        }
    }
}

fn take_stream_id(next_stream_id: &AtomicU64) -> Result<u64> {
    next_stream_id
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |id| {
            (id <= VarInt::MAX).then_some(id + 2)
        })
        .map_err(|_| Error::StreamIdExhausted)
}

fn ensure_peer_send_open(state: &StreamState, stream_id: u64, frame: &str) -> Result<()> {
    if state.recv_terminal.load(Ordering::Acquire) {
        Err(Error::Protocol(format!(
            "{frame} after peer send side closed for stream id {stream_id}"
        )))
    } else {
        Ok(())
    }
}

fn spawn_connection_tasks(
    stream: BoxIo,
    shared: Arc<ConnectionShared>,
    max_frame_size: usize,
    keepalive_interval: Option<Duration>,
    idle_timeout: Option<Duration>,
) {
    let mut codec = LengthDelimitedCodec::builder();
    codec.max_frame_length(max_frame_size);
    codec.length_field_type::<u32>();

    let framed = Framed::new(stream, codec.new_codec());
    let (mut sink, mut source) = framed.split();

    let reader_shared = shared.clone();
    tokio::spawn(async move {
        info!("reader task started");
        loop {
            let item = match idle_timeout {
                Some(timeout) => match tokio::time::timeout(timeout, source.next()).await {
                    Ok(item) => item,
                    Err(_) => {
                        warn!(?timeout, "connection idle timeout elapsed");
                        let _ = reader_shared
                            .initiate_close(1, "connection idle timeout elapsed".to_owned())
                            .await;
                        break;
                    }
                },
                None => source.next().await,
            };
            let Some(item) = item else {
                break;
            };

            match item {
                Ok(bytes) => {
                    reader_shared.record_received(bytes.len());
                    match handle_incoming_frame(reader_shared.clone(), bytes).await {
                        Ok(true) => {}
                        Ok(false) => break,
                        Err(error) => {
                            warn!(%error, "reader task failed while handling frame");
                            let _ = reader_shared
                                .initiate_close(1, format!("protocol/runtime error: {error}"))
                                .await;
                            break;
                        }
                    }
                }
                Err(error) => {
                    warn!(%error, "reader task decode failure");
                    break;
                }
            }
        }

        reader_shared.mark_closed().await;
        info!("reader task exited");
    });

    if let Some(interval) = keepalive_interval {
        let keepalive_shared = shared.clone();
        tokio::spawn(async move {
            let start = tokio::time::Instant::now() + interval;
            let mut ticker = tokio::time::interval_at(start, interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

            loop {
                let closed = keepalive_shared.close_notify.notified();
                if keepalive_shared.closed.load(Ordering::Acquire) {
                    break;
                }

                tokio::select! {
                    _ = ticker.tick() => {
                        if !keepalive_shared.writer.enqueue_control(Frame::Ping).await {
                            break;
                        }
                    }
                    () = closed => break,
                }
            }
        });
    }

    let writer_shared = shared.clone();
    tokio::spawn(async move {
        info!("writer task started");
        while let Some(frame) = writer_shared.writer.next_frame().await {
            let mut encoded = BytesMut::new();
            match frame.encode(&mut encoded) {
                Ok(()) => {
                    let encoded_len = encoded.len();
                    if let Err(error) = sink.send(encoded.freeze()).await {
                        warn!(%error, "writer send failure");
                        break;
                    }
                    writer_shared.record_sent(encoded_len);

                    if let Frame::ConnectionClose { .. } = frame {
                        break;
                    }
                }
                Err(error) => {
                    warn!(%error, "writer encode failure");
                    break;
                }
            }
        }

        if let Err(error) = sink.close().await {
            warn!(%error, "writer sink close failed");
        }

        writer_shared.mark_closed().await;
        info!("writer task exited");
    });
}

async fn handle_incoming_frame(shared: Arc<ConnectionShared>, bytes: BytesMut) -> Result<bool> {
    let mut bytes = bytes.freeze();
    let frame = Frame::decode(&mut bytes)?;

    match frame {
        Frame::Stream {
            stream_id,
            fin,
            payload,
        } => {
            shared
                .on_remote_stream_frame(stream_id.into_inner(), payload, fin)
                .await?;
        }
        Frame::ResetStream {
            stream_id,
            error_code,
        } => {
            shared
                .on_remote_reset(stream_id.into_inner(), error_code.into_inner())
                .await?;
        }
        Frame::OpenStream { stream_id } => {
            shared.on_remote_open(stream_id.into_inner()).await?;
        }
        Frame::Ping => {
            debug!("received ping");
        }
        Frame::ConnectionClose { error_code, reason } => {
            info!(error_code = error_code.into_inner(), reason = %reason, "received remote close");
            shared.mark_closed().await;
            return Ok(false);
        }
    }

    Ok(true)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU64;

    use muxtls_proto::VarInt;

    use super::{StreamState, ensure_peer_send_open, take_stream_id};
    use crate::Error;

    #[test]
    fn final_representable_stream_ids_can_be_allocated() {
        let even = AtomicU64::new(VarInt::MAX - 1);
        assert_eq!(
            take_stream_id(&even).expect("final even stream id"),
            VarInt::MAX - 1
        );
        assert!(matches!(
            take_stream_id(&even),
            Err(Error::StreamIdExhausted)
        ));

        let odd = AtomicU64::new(VarInt::MAX);
        assert_eq!(
            take_stream_id(&odd).expect("final odd stream id"),
            VarInt::MAX
        );
        assert!(matches!(
            take_stream_id(&odd),
            Err(Error::StreamIdExhausted)
        ));
    }

    #[tokio::test]
    async fn frames_after_peer_send_terminal_are_rejected() {
        let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
        let permit = semaphore.acquire_owned().await.expect("stream permit");
        let state = StreamState::new(1, 1, permit);

        ensure_peer_send_open(&state, 7, "stream frame").expect("open receive direction");
        state
            .recv_terminal
            .store(true, std::sync::atomic::Ordering::Release);

        let stream_error =
            ensure_peer_send_open(&state, 7, "stream frame").expect_err("frame after FIN");
        assert!(
            matches!(stream_error, Error::Protocol(message) if message.contains("stream frame"))
        );

        let reset_error = ensure_peer_send_open(&state, 7, "reset").expect_err("reset after FIN");
        assert!(matches!(reset_error, Error::Protocol(message) if message.contains("reset")));
    }
}
