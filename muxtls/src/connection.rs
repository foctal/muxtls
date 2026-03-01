use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

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
    pub opened_streams: u64,
    pub frames_sent: u64,
    pub frames_received: u64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
}

/// A live multiplexed TLS/TCP connection.
#[derive(Clone)]
pub struct Connection {
    pub(crate) shared: Arc<ConnectionShared>,
}

pub(crate) struct ConnectionShared {
    pub(crate) limits: Limits,
    pub(crate) local_parity: u64,
    pub(crate) next_local_stream_id: AtomicU64,
    pub(crate) streams: Mutex<HashMap<u64, Arc<StreamState>>>,
    pub(crate) incoming_stream_tx: mpsc::Sender<u64>,
    pub(crate) incoming_stream_rx: Mutex<mpsc::Receiver<u64>>,
    pub(crate) writer: Arc<WriterState>,
    pub(crate) closed: AtomicBool,
    pub(crate) close_notify: Notify,
    pub(crate) open_streams: Arc<Semaphore>,
    pub(crate) inbound_conn_bytes: Arc<Semaphore>,
    pub(crate) outbound_conn_bytes: Arc<Semaphore>,
    pub(crate) stats_opened_streams: AtomicU64,
    pub(crate) stats_frames_sent: AtomicU64,
    pub(crate) stats_frames_received: AtomicU64,
    pub(crate) stats_bytes_sent: AtomicU64,
    pub(crate) stats_bytes_received: AtomicU64,
}

pub(crate) struct StreamState {
    inbound: Mutex<InboundState>,
    inbound_notify: Notify,
    inbound_stream_bytes: Arc<Semaphore>,
    outbound_stream_bytes: Arc<Semaphore>,
    send_terminal: AtomicBool,
    recv_terminal: AtomicBool,
    send_handles: AtomicUsize,
    recv_handles: AtomicUsize,
    open_permit: Mutex<Option<OwnedSemaphorePermit>>,
}

struct InboundState {
    chunks: VecDeque<InboundChunk>,
    reset_error: Option<u64>,
    fin_received: bool,
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
    closing: bool,
}

pub(crate) struct WriterState {
    queues: Mutex<WriterQueues>,
    notify: Notify,
}

impl Connection {
    pub(crate) fn new<S>(stream: S, limits: Limits, is_client: bool) -> Self
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let stream: BoxIo = Box::new(stream);
        let local_parity = if is_client { 0 } else { 1 };
        let next_local_stream_id = AtomicU64::new(local_parity);

        let (incoming_stream_tx, incoming_stream_rx) = mpsc::channel(limits.max_open_streams);

        let shared = Arc::new(ConnectionShared {
            limits: limits.clone(),
            local_parity,
            next_local_stream_id,
            streams: Mutex::new(HashMap::new()),
            incoming_stream_tx,
            incoming_stream_rx: Mutex::new(incoming_stream_rx),
            writer: Arc::new(WriterState::new()),
            closed: AtomicBool::new(false),
            close_notify: Notify::new(),
            open_streams: Arc::new(Semaphore::new(limits.max_open_streams)),
            inbound_conn_bytes: Arc::new(Semaphore::new(limits.max_inbound_connection_bytes)),
            outbound_conn_bytes: Arc::new(Semaphore::new(limits.max_outbound_connection_bytes)),
            stats_opened_streams: AtomicU64::new(0),
            stats_frames_sent: AtomicU64::new(0),
            stats_frames_received: AtomicU64::new(0),
            stats_bytes_sent: AtomicU64::new(0),
            stats_bytes_received: AtomicU64::new(0),
        });

        spawn_connection_tasks(stream, shared.clone(), limits.max_frame_size);

        Self { shared }
    }

    /// Opens a new bidirectional stream initiated by the local endpoint.
    #[instrument(skip(self), level = "debug")]
    pub async fn open_bi(&self) -> Result<(SendStream, RecvStream)> {
        self.shared.ensure_open()?;

        let permit = self
            .shared
            .open_streams
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| Error::ConnectionClosed)?;

        let stream_id = self
            .shared
            .next_local_stream_id
            .fetch_add(2, Ordering::Relaxed);

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
        self.shared.ensure_open()?;

        let mut rx = self.shared.incoming_stream_rx.lock().await;
        let stream_id = rx.recv().await.ok_or(Error::ConnectionClosed)?;
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
        self.shared.initiate_close(0, reason.into()).await
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

        if payload.len() > self.limits.max_frame_size {
            return Err(Error::LimitExceeded(format!(
                "payload size {} exceeds max frame size {}",
                payload.len(),
                self.limits.max_frame_size
            )));
        }

        if state.send_terminal.load(Ordering::Acquire) {
            return Err(Error::Protocol(
                "stream send side already closed".to_owned(),
            ));
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

        self.writer
            .enqueue_data(
                stream_id,
                OutboundChunk {
                    stream_id: VarInt::from_u64(stream_id)
                        .map_err(|e| Error::Protocol(e.to_string()))?,
                    payload,
                    fin,
                    _conn_permit: conn_permit,
                    _stream_permit: stream_permit,
                },
            )
            .await;

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
        if state.send_terminal.swap(true, Ordering::AcqRel) {
            return Ok(());
        }

        self.writer
            .enqueue_control(Frame::ResetStream {
                stream_id: VarInt::from_u64(stream_id)
                    .map_err(|e| Error::Protocol(e.to_string()))?,
                error_code: ProtoErrorCode::from_u64(error_code)?,
            })
            .await;
        self.try_retire_stream(stream_id).await;
        Ok(())
    }

    pub(crate) async fn handle_last_send_drop(
        self: &Arc<Self>,
        stream_id: u64,
        state: &Arc<StreamState>,
    ) {
        if !state.send_terminal.swap(true, Ordering::AcqRel)
            && !self.closed.load(Ordering::Acquire)
            && let (Ok(stream_id), Ok(error_code)) =
                (VarInt::from_u64(stream_id), ProtoErrorCode::from_u64(0))
        {
            self.writer
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
        state.mark_recv_terminal().await;
        self.try_retire_stream(stream_id).await;
    }

    pub(crate) async fn initiate_close(
        self: &Arc<Self>,
        error_code: u64,
        reason: String,
    ) -> Result<()> {
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
        self.close_notify.notify_waiters();
        Ok(())
    }

    async fn on_remote_stream_frame(
        self: &Arc<Self>,
        stream_id: u64,
        payload: Bytes,
        fin: bool,
    ) -> Result<()> {
        let state = self.get_or_create_remote_stream(stream_id).await?;

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
        let state = {
            let streams = self.streams.lock().await;
            streams.get(&stream_id).cloned()
        }
        .ok_or_else(|| Error::Protocol(format!("reset for unknown stream id {stream_id}")))?;

        state.mark_reset(error_code).await;
        state.mark_recv_terminal().await;
        self.try_retire_stream(stream_id).await;
        Ok(())
    }

    async fn get_or_create_remote_stream(
        self: &Arc<Self>,
        stream_id: u64,
    ) -> Result<Arc<StreamState>> {
        if let Some(state) = self.streams.lock().await.get(&stream_id).cloned() {
            return Ok(state);
        }

        if stream_id % 2 == self.local_parity {
            return Err(Error::Protocol(format!(
                "peer opened stream with invalid parity: {stream_id}"
            )));
        }

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
            streams.insert(stream_id, state.clone());
        }

        self.stats_opened_streams.fetch_add(1, Ordering::Relaxed);

        if self.incoming_stream_tx.send(stream_id).await.is_err() {
            return Err(Error::ConnectionClosed);
        }

        debug!(stream_id, "accepted remote stream");
        Ok(state)
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
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        self.close_notify.notify_waiters();
        let mut streams = self.streams.lock().await;
        for (_, stream) in streams.iter() {
            stream.mark_connection_closed().await;
        }
        streams.clear();
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
                closing: false,
            }),
            notify: Notify::new(),
        }
    }

    async fn enqueue_data(&self, stream_id: u64, chunk: OutboundChunk) {
        let mut queues = self.queues.lock().await;
        let q = queues.by_stream.entry(stream_id).or_default();
        let was_empty = q.is_empty();
        q.push_back(chunk);
        if was_empty {
            queues.ready.push_back(stream_id);
        }
        drop(queues);
        self.notify.notify_one();
    }

    async fn enqueue_control(&self, frame: Frame) {
        let mut queues = self.queues.lock().await;
        queues.control.push_back(frame);
        drop(queues);
        self.notify.notify_one();
    }

    async fn enqueue_close(&self, frame: Frame) {
        let mut queues = self.queues.lock().await;
        queues.control.push_back(frame);
        queues.closing = true;
        drop(queues);
        self.notify.notify_waiters();
    }

    async fn next_frame(&self) -> Option<Frame> {
        loop {
            let notified = self.notify.notified();
            let mut queues = self.queues.lock().await;

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
            }),
            inbound_notify: Notify::new(),
            inbound_stream_bytes: Arc::new(Semaphore::new(max_inbound_stream_bytes)),
            outbound_stream_bytes: Arc::new(Semaphore::new(max_outbound_stream_bytes)),
            send_terminal: AtomicBool::new(false),
            recv_terminal: AtomicBool::new(false),
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
        let payload_len = payload.len();

        let conn_permit = if payload_len == 0 {
            None
        } else {
            Some(
                shared
                    .inbound_conn_bytes
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
                self.inbound_stream_bytes
                    .clone()
                    .acquire_many_owned(payload_len as u32)
                    .await
                    .map_err(|_| Error::ConnectionClosed)?,
            )
        };

        let mut inbound = self.inbound.lock().await;
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
        if inbound.reset_error.is_none() {
            inbound.reset_error = Some(0);
        }
        drop(inbound);
        self.inbound_notify.notify_waiters();
    }

    async fn release_open_permit(&self) {
        let mut permit = self.open_permit.lock().await;
        *permit = None;
    }

    async fn discard_inbound(&self) {
        let mut inbound = self.inbound.lock().await;
        inbound.chunks.clear();
        inbound.fin_received = true;
        drop(inbound);
        self.inbound_notify.notify_waiters();
    }

    pub(crate) async fn read_chunk(&self) -> Result<Option<Bytes>> {
        loop {
            let notified = self.inbound_notify.notified();
            let mut inbound = self.inbound.lock().await;

            if let Some(error_code) = inbound.reset_error {
                return Err(Error::StreamReset(error_code));
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

fn spawn_connection_tasks(stream: BoxIo, shared: Arc<ConnectionShared>, max_frame_size: usize) {
    let mut codec = LengthDelimitedCodec::builder();
    codec.max_frame_length(max_frame_size);
    codec.length_field_type::<u32>();

    let framed = Framed::new(stream, codec.new_codec());
    let (mut sink, mut source) = framed.split();

    let reader_shared = shared.clone();
    tokio::spawn(async move {
        info!("reader task started");
        while let Some(item) = source.next().await {
            match item {
                Ok(bytes) => {
                    reader_shared.record_received(bytes.len());
                    if let Err(error) = handle_incoming_frame(reader_shared.clone(), bytes).await {
                        warn!(%error, "reader task failed while handling frame");
                        let _ = reader_shared
                            .initiate_close(1, format!("protocol/runtime error: {error}"))
                            .await;
                        break;
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

    let writer_shared = shared.clone();
    tokio::spawn(async move {
        info!("writer task started");
        while let Some(frame) = writer_shared.writer.next_frame().await {
            let mut encoded = BytesMut::new();
            match frame.encode(&mut encoded) {
                Ok(()) => {
                    writer_shared.record_sent(encoded.len());
                    if let Err(error) = sink.send(encoded.freeze()).await {
                        warn!(%error, "writer send failure");
                        break;
                    }

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

async fn handle_incoming_frame(shared: Arc<ConnectionShared>, bytes: BytesMut) -> Result<()> {
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
        Frame::Ping => {
            debug!("received ping");
        }
        Frame::ConnectionClose { error_code, reason } => {
            info!(error_code = error_code.into_inner(), reason = %reason, "received remote close");
            shared.mark_closed().await;
        }
    }

    Ok(())
}
