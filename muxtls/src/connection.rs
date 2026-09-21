use crate::flow::ReceiveWindow;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use bytes::{Buf, Bytes, BytesMut};
use futures_util::{SinkExt, StreamExt};
use muxtls_proto::{ErrorCode as ProtoErrorCode, Frame, VarInt};
use rustls::pki_types::CertificateDer;
use tokio::sync::{Mutex, Notify, OwnedSemaphorePermit, Semaphore};
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

/// The certificate chain associated with the completed TLS handshake.
///
/// Whether the chain was cryptographically verified is determined by the TLS
/// configuration. The testing-only insecure client verifier does not establish
/// an authenticated identity.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct PeerIdentity {
    certificates: Vec<CertificateDer<'static>>,
}

type IncomingStream = (u64, Arc<StreamState>);

pub(crate) struct ConnectionShared {
    pub(crate) limits: Limits,
    pub(crate) local_parity: u64,
    pub(crate) next_local_stream_id: AtomicU64,
    pub(crate) next_remote_stream_id: AtomicU64,
    pub(crate) open_lock: Mutex<()>,
    pub(crate) streams: Mutex<HashMap<u64, Arc<StreamState>>>,
    incoming_streams: Mutex<VecDeque<IncomingStream>>,
    incoming_notify: Notify,
    pub(crate) writer: Arc<WriterState>,
    pub(crate) closed: AtomicBool,
    pub(crate) terminated: AtomicBool,
    pub(crate) close_notify: Notify,
    pub(crate) drop_notify: Notify,
    pub(crate) open_streams: Arc<Semaphore>,
    pub(crate) inbound_conn_bytes: Arc<Semaphore>,
    pub(crate) outbound_conn_bytes: Arc<Semaphore>,
    outbound_frames: Arc<Semaphore>,
    pub(crate) stats_opened_streams: AtomicU64,
    pub(crate) stats_frames_sent: AtomicU64,
    pub(crate) stats_frames_received: AtomicU64,
    pub(crate) stats_bytes_sent: AtomicU64,
    pub(crate) stats_bytes_received: AtomicU64,
    pub(crate) connection_handles: AtomicUsize,
    pub(crate) peer_identity: Option<PeerIdentity>,
    receive_window: Arc<ReceiveWindow>,
    joined: AtomicBool,
    cleanup_started: AtomicBool,
}

pub(crate) struct StreamState {
    inbound: Mutex<InboundState>,
    receive_window: Arc<ReceiveWindow>,
    peer_limit: AtomicU64,
    sent: AtomicU64,
    inbound_notify: Notify,
    inbound_stream_bytes: Arc<Semaphore>,
    outbound_stream_bytes: Arc<Semaphore>,
    outbound_stream_frames: Arc<Semaphore>,
    send_lock: Mutex<()>,
    send_notify: Notify,
    pub(crate) send_terminal: AtomicBool,
    pub(crate) send_dropped: AtomicBool,
    pub(crate) recv_dropped: AtomicBool,
    send_complete: AtomicBool,
    send_dispatched: AtomicBool,
    reset_queued: AtomicBool,
    recv_terminal: AtomicBool,
    recv_discarded: AtomicBool,
    send_handles: AtomicUsize,
    pub(crate) send_mode: AtomicUsize,
    pub(crate) recv_mode: AtomicUsize,
    recv_handles: AtomicUsize,
    open_permit: Mutex<Option<OwnedSemaphorePermit>>,
}

struct InboundState {
    chunks: VecDeque<InboundChunk>,
    reset_error: Option<u64>,
    fin_received: bool,
    connection_closed: bool,
}

pub(crate) struct InboundChunk {
    pub(crate) data: ReceiveBuffer,
    connection_window: Arc<ReceiveWindow>,
    stream_window: Arc<ReceiveWindow>,
    writer: Weak<WriterState>,
    _conn_permit: Option<OwnedSemaphorePermit>,
    _stream_permit: Option<OwnedSemaphorePermit>,
}

/// Small peer frames share a bounded page, avoiding one metadata allocation
/// per byte. Large frames keep the decoder's zero-copy Bytes ownership.
pub(crate) enum ReceiveBuffer {
    Shared(Bytes),
    Page(BytesMut),
}
impl Default for ReceiveBuffer {
    fn default() -> Self {
        Self::Shared(Bytes::new())
    }
}
impl std::ops::Deref for ReceiveBuffer {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match self {
            Self::Shared(data) => data,
            Self::Page(data) => data,
        }
    }
}
impl ReceiveBuffer {
    pub(crate) fn advance(&mut self, count: usize) {
        match self {
            Self::Shared(data) => data.advance(count),
            Self::Page(data) => data.advance(count),
        }
    }
    fn into_bytes(self) -> Bytes {
        match self {
            Self::Shared(data) => data,
            Self::Page(data) => data.freeze(),
        }
    }
}

impl InboundChunk {
    pub(crate) fn consume(&mut self, count: usize) {
        drop(
            self._conn_permit
                .as_mut()
                .and_then(|permit| permit.split(count)),
        );
        drop(
            self._stream_permit
                .as_mut()
                .and_then(|permit| permit.split(count)),
        );
        let update = self.connection_window.consume(count) | self.stream_window.consume(count);
        if update && let Some(writer) = self.writer.upgrade() {
            writer.notify.notify_one();
        }
    }
    pub(crate) fn into_bytes(mut self) -> Bytes {
        let count = self.data.len();
        self.consume(count);
        std::mem::take(&mut self.data).into_bytes()
    }
}
impl Drop for InboundChunk {
    fn drop(&mut self) {
        self.consume(self.data.len());
    }
}

struct OutboundChunk {
    state: Arc<StreamState>,
    _frame_permit: OwnedSemaphorePermit,
    _stream_frame_permit: OwnedSemaphorePermit,
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
    peer_settings: bool,
    peer_max_frame: usize,
    peer_initial_stream_limit: u64,
    peer_limit: u64,
    sent: u64,
    control_turn: bool,
    receive_windows: HashMap<u64, Weak<StreamState>>,
}

pub(crate) struct WriterState {
    max_control_frames: usize,
    queues: Mutex<WriterQueues>,
    notify: Notify,
    receive_window: Arc<ReceiveWindow>,
}

impl Connection {
    pub(crate) fn new<S>(
        stream: S,
        limits: Limits,
        is_client: bool,
        peer_certificates: Option<Vec<CertificateDer<'static>>>,
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

        let receive_window = Arc::new(ReceiveWindow::new(limits.max_inbound_connection_bytes));
        let writer = Arc::new(WriterState::new(
            limits.max_control_frames,
            receive_window.clone(),
        ));
        {
            let mut queues = writer.queues.try_lock().expect("new queue");
            queues.control.push_back(Frame::Settings {
                max_data: VarInt::from_u64(limits.max_inbound_connection_bytes as u64)
                    .expect("validated limit"),
                max_stream_data: VarInt::from_u64(limits.max_inbound_stream_bytes as u64)
                    .expect("validated limit"),
                max_frame_size: VarInt::from_u64(limits.max_frame_size as u64)
                    .expect("validated limit"),
            });
        }
        let shared = Arc::new(ConnectionShared {
            limits: limits.clone(),
            local_parity,
            next_local_stream_id,
            next_remote_stream_id,
            open_lock: Mutex::new(()),
            streams: Mutex::new(HashMap::new()),
            incoming_streams: Mutex::new(VecDeque::new()),
            incoming_notify: Notify::new(),
            writer,
            receive_window,
            joined: AtomicBool::new(false),
            cleanup_started: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            terminated: AtomicBool::new(false),
            close_notify: Notify::new(),
            drop_notify: Notify::new(),
            open_streams: Arc::new(Semaphore::new(limits.max_open_streams)),
            inbound_conn_bytes: Arc::new(Semaphore::new(limits.max_inbound_connection_bytes)),
            outbound_conn_bytes: Arc::new(Semaphore::new(limits.max_outbound_connection_bytes)),
            outbound_frames: Arc::new(Semaphore::new(limits.max_queued_outbound_frames)),
            stats_opened_streams: AtomicU64::new(0),
            stats_frames_sent: AtomicU64::new(0),
            stats_frames_received: AtomicU64::new(0),
            stats_bytes_sent: AtomicU64::new(0),
            stats_bytes_received: AtomicU64::new(0),
            connection_handles: AtomicUsize::new(1),
            peer_identity: peer_certificates.map(|certificates| PeerIdentity { certificates }),
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
    /// Cancellation before completion does not consume a stream ID or publish a stream.
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

        // Acquire every fallible/async resource before assigning an ID. Once
        // assigned, publish state and OPEN atomically without a cancellation point.
        let mut streams = self.shared.streams.lock().await;
        let mut queues = self.shared.writer.queues.lock().await;
        self.shared.ensure_open()?;
        if queues.closing || queues.control.len() >= self.shared.writer.max_control_frames {
            drop(queues);
            drop(streams);
            self.shared.mark_closed().await;
            return Err(Error::ConnectionClosed);
        }
        let stream_id = take_stream_id(&self.shared.next_local_stream_id)?;
        let state = Arc::new(StreamState::new(
            self.shared.limits.max_inbound_stream_bytes,
            self.shared.limits.max_outbound_stream_bytes,
            permit,
            self.shared.limits.max_queued_outbound_frames / self.shared.limits.max_open_streams,
        ));
        queues.control.push_back(Frame::OpenStream {
            stream_id: VarInt::from_u64(stream_id).map_err(|e| Error::Protocol(e.to_string()))?,
        });
        queues
            .receive_windows
            .insert(stream_id, Arc::downgrade(&state));
        streams.insert(stream_id, state.clone());
        drop(queues);
        drop(streams);
        self.shared.writer.notify.notify_one();
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
    /// Cancellation does not consume an incoming stream before returning it.
    pub async fn accept_bi(&self) -> Result<(SendStream, RecvStream)> {
        loop {
            let incoming = self.shared.incoming_notify.notified();
            let closed = self.shared.close_notify.notified();
            self.shared.ensure_open()?;
            if let Some((stream_id, state)) = self.shared.incoming_streams.lock().await.pop_front()
            {
                return Ok((
                    SendStream::new(stream_id, state.clone(), self.shared.clone()),
                    RecvStream::new(stream_id, state, self.shared.clone()),
                ));
            }
            tokio::select! {
                () = incoming => {},
                () = closed => self.shared.ensure_open()?,
            }
        }
    }

    /// Sends a connection close frame and shuts down the connection.
    pub async fn close(&self, reason: impl Into<String>) -> Result<()> {
        let reason = reason.into();
        self.shared.validate_close_reason(0, &reason)?;
        self.shared.initiate_close(0, reason).await
    }

    /// Immediately cancels transport I/O. Use `wait_closed` to await resource release.
    pub fn abort(&self) {
        self.shared.closed.store(true, Ordering::Release);
        self.shared.terminated.store(true, Ordering::Release);
        self.shared.close_notify.notify_waiters();
        self.shared.writer.notify.notify_waiters();
    }

    /// Waits until the connection task group has exited and released the transport.
    pub async fn wait_closed(&self) {
        loop {
            let notified = self.shared.close_notify.notified();
            if self.shared.joined.load(Ordering::Acquire) {
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

    /// Returns the peer certificate chain from the completed TLS handshake.
    ///
    /// The first certificate is the peer's end-entity certificate. Servers
    /// configured without client authentication return `None` for clients that
    /// did not present a certificate.
    pub fn peer_identity(&self) -> Option<&PeerIdentity> {
        self.shared.peer_identity.as_ref()
    }
}

impl PeerIdentity {
    /// Returns the peer certificate chain, with the end-entity certificate first.
    pub fn certificates(&self) -> &[CertificateDer<'static>] {
        &self.certificates
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

        self.shared.close_notify.notify_waiters();
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
        tokio::select! {
            biased;
            () = async {
                loop {
                    let notified = state.send_notify.notified();
                    if state.send_terminal.load(Ordering::Acquire) { break; }
                    notified.await;
                }
            } => Err(Error::Protocol("stream send side closed".to_owned())),
            result = self.queue_stream_chunk(stream_id, state, payload, fin) => result,
        }
    }

    async fn queue_stream_chunk(
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
        if payload_len > self.limits.max_outbound_stream_bytes {
            return Err(Error::LimitExceeded(
                "chunk exceeds the outbound stream byte budget".to_owned(),
            ));
        }
        // Empty non-FIN writes carry no stream data and must not create an
        // unlimited queue of entries without byte permits.
        if payload_len == 0 && !fin {
            return Ok(());
        }

        let stream_frame_permit = state
            .outbound_stream_frames
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| Error::ConnectionClosed)?;

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

        let frame_permit = self
            .outbound_frames
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| Error::ConnectionClosed)?;

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

        self.ensure_open()?;
        let enqueued = self
            .writer
            .enqueue_data(
                stream_id,
                OutboundChunk {
                    state: state.clone(),
                    _frame_permit: frame_permit,
                    _stream_frame_permit: stream_frame_permit,
                    stream_id: proto_stream_id,
                    payload,
                    fin,
                    _conn_permit: conn_permit,
                    _stream_permit: stream_permit,
                },
            )
            .await;
        if !enqueued {
            return Err(Error::Protocol(
                "stream or connection closed before enqueue".to_owned(),
            ));
        }

        if fin {
            state.send_terminal.store(true, Ordering::Release);
            state.send_notify.notify_waiters();
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
        let proto_stream_id =
            VarInt::from_u64(stream_id).map_err(|e| Error::Protocol(e.to_string()))?;
        let error_code = ProtoErrorCode::from_u64(error_code)?;
        let mut queues = self.writer.queues.lock().await;
        if state.send_dispatched.load(Ordering::Acquire)
            || state.reset_queued.load(Ordering::Acquire)
        {
            return Ok(());
        }
        if queues.closing || queues.control.len() >= self.writer.max_control_frames {
            return Err(Error::ConnectionClosed);
        }
        state.reset_queued.store(true, Ordering::Release);
        queues.by_stream.remove(&stream_id);
        queues.ready.retain(|id| *id != stream_id);
        queues.control.push_back(Frame::ResetStream {
            stream_id: proto_stream_id,
            error_code,
        });
        state.send_terminal.store(true, Ordering::Release);
        state.outbound_stream_bytes.close();
        state.send_notify.notify_waiters();
        drop(queues);
        self.writer.notify.notify_one();
        Ok(())
    }

    pub(crate) async fn handle_last_send_drop(
        self: &Arc<Self>,
        stream_id: u64,
        state: &Arc<StreamState>,
    ) {
        if !state.send_terminal.load(Ordering::Acquire) {
            let _ = self.reset_stream(stream_id, state, 0).await;
        }
        self.try_retire_stream(stream_id).await;
    }

    pub(crate) async fn handle_last_recv_drop(
        self: &Arc<Self>,
        stream_id: u64,
        state: &Arc<StreamState>,
    ) {
        state.discard_inbound().await;
        if !state.recv_terminal.load(Ordering::Acquire)
            && !self.closed.load(Ordering::Acquire)
            && !self
                .writer
                .enqueue_control(Frame::StopSending {
                    stream_id: VarInt::from_u64(stream_id).expect("valid stream ID"),
                })
                .await
        {
            self.mark_closed().await;
        }
        self.try_retire_stream(stream_id).await;
    }

    pub(crate) async fn initiate_close(
        self: &Arc<Self>,
        error_code: u64,
        reason: String,
    ) -> Result<()> {
        let reason = self.fit_close_reason(error_code, reason)?;
        let frame = Frame::ConnectionClose {
            error_code: ProtoErrorCode::from_u64(error_code)?,
            reason,
        };
        let mut queues = self.writer.queues.lock().await;
        if self.closed.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        if error_code != 0 {
            queues.by_stream.clear();
            queues.ready.clear();
            queues.control.clear();
        }
        queues.close_frame = Some(frame);
        queues.graceful_close = error_code == 0;
        queues.closing = true;
        drop(queues);
        self.open_streams.close();
        self.outbound_conn_bytes.close();
        self.outbound_frames.close();
        self.close_notify.notify_waiters();
        self.writer.notify.notify_waiters();
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
        low.min(self.limits.max_outbound_stream_bytes)
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
        self.receive_window.receive(payload.len())?;
        state.receive_window.receive(payload.len())?;
        if self.receive_window.needs_update() || state.receive_window.needs_update() {
            self.writer.notify.notify_one();
        }

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
            self.limits.max_queued_outbound_frames / self.limits.max_open_streams,
        ));

        {
            let mut streams = self.streams.lock().await;
            if streams.contains_key(&stream_id) {
                return Err(Error::Protocol(format!(
                    "peer reused active stream id {stream_id}"
                )));
            }
            self.writer
                .queues
                .lock()
                .await
                .receive_windows
                .insert(stream_id, Arc::downgrade(&state));
            streams.insert(stream_id, state.clone());
        }
        self.next_remote_stream_id.store(next, Ordering::Release);

        self.stats_opened_streams.fetch_add(1, Ordering::Relaxed);

        let mut incoming = self.incoming_streams.lock().await;
        if incoming.len() >= self.limits.max_open_streams {
            return Err(Error::LimitExceeded(
                "incoming stream queue is full".to_owned(),
            ));
        }
        incoming.push_back((stream_id, state));
        drop(incoming);
        self.incoming_notify.notify_one();

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

        if !state.send_complete.load(Ordering::Acquire)
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
            self.writer
                .queues
                .lock()
                .await
                .receive_windows
                .remove(&stream_id);
        }
        debug!(stream_id, "stream reached terminal state");
    }

    async fn wait_terminated(&self) {
        loop {
            let notified = self.close_notify.notified();
            if self.terminated.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }

    async fn mark_closed(&self) {
        self.closed.store(true, Ordering::Release);
        self.terminated.store(true, Ordering::Release);
        self.close_notify.notify_waiters();
        if self.cleanup_started.swap(true, Ordering::AcqRel) {
            return;
        }
        self.open_streams.close();
        self.inbound_conn_bytes.close();
        self.outbound_conn_bytes.close();
        self.outbound_frames.close();
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
    fn new(max_control_frames: usize, receive_window: Arc<ReceiveWindow>) -> Self {
        Self {
            max_control_frames,
            receive_window,
            queues: Mutex::new(WriterQueues {
                by_stream: HashMap::new(),
                ready: VecDeque::new(),
                control: VecDeque::new(),
                close_frame: None,
                graceful_close: false,
                closing: false,
                peer_settings: false,
                peer_max_frame: Limits::MIN_FRAME_SIZE,
                peer_initial_stream_limit: 0,
                peer_limit: 0,
                sent: 0,
                control_turn: true,
                receive_windows: HashMap::new(),
            }),
            notify: Notify::new(),
        }
    }

    async fn enqueue_data(&self, stream_id: u64, chunk: OutboundChunk) -> bool {
        let mut queues = self.queues.lock().await;
        if queues.closing || chunk.state.send_terminal.load(Ordering::Acquire) {
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
        if matches!(frame, Frame::Ping)
            && queues
                .control
                .iter()
                .any(|frame| matches!(frame, Frame::Ping))
        {
            return true;
        }
        if queues.control.len() >= self.max_control_frames {
            return false;
        }
        queues.control.push_back(frame);
        drop(queues);
        self.notify.notify_one();
        true
    }

    async fn shutdown(&self) {
        let mut queues = self.queues.lock().await;
        queues.by_stream.clear();
        queues.ready.clear();
        queues.control.clear();
        queues.receive_windows.clear();
        queues.close_frame = None;
        queues.graceful_close = false;
        queues.closing = true;
        drop(queues);
        self.notify.notify_waiters();
    }

    async fn next_frame(&self) -> Option<Frame> {
        loop {
            let notified = self.notify.notified();
            let mut queues = self.queues.lock().await;
            if queues.closing && !queues.graceful_close {
                return take_close_frame(&mut queues);
            }
            // SETTINGS and OPEN must precede dependent traffic. Alternate other
            // control work with one eligible data frame to avoid starvation.
            let urgent = matches!(
                queues.control.front(),
                Some(Frame::Settings { .. } | Frame::OpenStream { .. })
            );
            if queues.control_turn || urgent {
                if let Some(frame) = queues.control.pop_front() {
                    queues.control_turn = false;
                    return Some(frame);
                }
                if let Some(maximum) = self.receive_window.update() {
                    queues.control_turn = false;
                    return Some(Frame::MaxData { maximum });
                }
                let update = queues.receive_windows.iter().find_map(|(id, state)| {
                    let state = state.upgrade()?;
                    if state.recv_terminal.load(Ordering::Acquire)
                        || state.recv_discarded.load(Ordering::Acquire)
                    {
                        return None;
                    }
                    state
                        .receive_window
                        .update()
                        .map(|maximum| Frame::MaxStreamData {
                            stream_id: VarInt::from_u64(*id).expect("valid stream ID"),
                            maximum,
                        })
                });
                if let Some(frame) = update {
                    queues.control_turn = false;
                    return Some(frame);
                }
            }
            let conn_credit = queues.peer_limit.saturating_sub(queues.sent);
            let initial_stream_limit = queues.peer_initial_stream_limit;
            let max_frame = queues.peer_max_frame;
            let settings = queues.peer_settings;
            for _ in 0..queues.ready.len() {
                let id = queues.ready.pop_front().expect("ready stream");
                let q = queues.by_stream.get_mut(&id).expect("ready queue");
                let chunk = q.front_mut().expect("nonempty queue");
                let sent = chunk.state.sent.load(Ordering::Acquire);
                let stream_credit = chunk
                    .state
                    .peer_limit
                    .load(Ordering::Acquire)
                    .max(initial_stream_limit)
                    .saturating_sub(sent);
                // Use this stream's header width without allocating according
                // to a peer-provided maximum.
                let capacity = max_frame - 2 - chunk.stream_id.encoded_len();
                let header = VarInt::from_u64(capacity as u64)
                    .expect("bounded frame size")
                    .encoded_len();
                let count = chunk.payload.len().min(
                    conn_credit
                        .min(stream_credit)
                        .min((capacity - header) as u64) as usize,
                );
                if !settings || (count == 0 && !chunk.payload.is_empty()) {
                    queues.ready.push_back(id);
                    continue;
                }
                let payload = chunk.payload.split_to(count);
                chunk.state.sent.fetch_add(count as u64, Ordering::AcqRel);
                let fin = chunk.fin && chunk.payload.is_empty();
                if fin {
                    chunk.state.send_dispatched.store(true, Ordering::Release);
                }
                let stream_id = chunk.stream_id;
                if chunk.payload.is_empty() {
                    q.pop_front();
                }
                if q.is_empty() {
                    queues.by_stream.remove(&id);
                } else {
                    queues.ready.push_back(id);
                }
                queues.sent += count as u64;
                queues.control_turn = true;
                return Some(Frame::Stream {
                    stream_id,
                    fin,
                    payload,
                });
            }
            if !queues.control_turn {
                queues.control_turn = true;
                drop(queues);
                continue;
            }
            if queues.closing && queues.by_stream.is_empty() {
                return take_close_frame(&mut queues);
            }
            drop(queues);
            notified.await;
        }
    }
}

fn take_close_frame(queues: &mut WriterQueues) -> Option<Frame> {
    let mut frame = queues.close_frame.take()?;
    if let Frame::ConnectionClose { error_code, reason } = &mut frame {
        let maximum = queues
            .peer_max_frame
            .saturating_sub(1 + error_code.encoded_len() + 8);
        if reason.len() > maximum {
            let mut end = maximum;
            while !reason.is_char_boundary(end) {
                end -= 1;
            }
            reason.truncate(end);
        }
    }
    Some(frame)
}

impl StreamState {
    fn new(
        max_inbound_stream_bytes: usize,
        max_outbound_stream_bytes: usize,
        permit: OwnedSemaphorePermit,
        max_outbound_frames: usize,
    ) -> Self {
        Self {
            receive_window: Arc::new(ReceiveWindow::new(max_inbound_stream_bytes)),
            peer_limit: AtomicU64::new(0),
            sent: AtomicU64::new(0),
            inbound: Mutex::new(InboundState {
                chunks: VecDeque::new(),
                reset_error: None,
                fin_received: false,
                connection_closed: false,
            }),
            inbound_notify: Notify::new(),
            inbound_stream_bytes: Arc::new(Semaphore::new(max_inbound_stream_bytes)),
            outbound_stream_bytes: Arc::new(Semaphore::new(max_outbound_stream_bytes)),
            outbound_stream_frames: Arc::new(Semaphore::new(max_outbound_frames)),
            send_lock: Mutex::new(()),
            send_notify: Notify::new(),
            send_terminal: AtomicBool::new(false),
            send_dropped: AtomicBool::new(false),
            recv_dropped: AtomicBool::new(false),
            send_complete: AtomicBool::new(false),
            send_dispatched: AtomicBool::new(false),
            reset_queued: AtomicBool::new(false),
            recv_terminal: AtomicBool::new(false),
            recv_discarded: AtomicBool::new(false),
            send_handles: AtomicUsize::new(0),
            send_mode: AtomicUsize::new(0),
            recv_mode: AtomicUsize::new(0),
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
            shared.receive_window.consume(payload.len());
            shared.writer.notify.notify_one();
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
            drop(conn_permit);
            drop(stream_permit);
            shared.receive_window.consume(payload.len());
            shared.writer.notify.notify_one();
            return Ok(());
        }
        if inbound.fin_received {
            return Err(Error::Protocol("received stream data after FIN".to_owned()));
        }

        let page_size = shared.limits.max_inbound_stream_bytes.min(16 * 1024);
        if let Some(tail) = inbound.chunks.back_mut()
            && let ReceiveBuffer::Page(page) = &mut tail.data
            && page.len() + payload.len() <= page_size
        {
            page.extend_from_slice(&payload);
            tail._conn_permit
                .as_mut()
                .expect("nonempty page")
                .merge(conn_permit.expect("nonempty payload"));
            tail._stream_permit
                .as_mut()
                .expect("nonempty page")
                .merge(stream_permit.expect("nonempty payload"));
            drop(inbound);
            self.inbound_notify.notify_one();
            return Ok(());
        }
        let data = if payload.len() < page_size.div_ceil(2) {
            let mut page = BytesMut::with_capacity(page_size);
            page.extend_from_slice(&payload);
            ReceiveBuffer::Page(page)
        } else {
            ReceiveBuffer::Shared(payload)
        };
        inbound.chunks.push_back(InboundChunk {
            data,
            connection_window: shared.receive_window.clone(),
            stream_window: self.receive_window.clone(),
            writer: Arc::downgrade(&shared.writer),
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
        // The application still owns its receive handle. Preserve already
        // accepted bytes and FIN even if the peer closes before it reads them.
        inbound.connection_closed = true;
        drop(inbound);
        self.inbound_stream_bytes.close();
        self.outbound_stream_bytes.close();
        self.outbound_stream_frames.close();
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

    pub(crate) async fn read_chunk(&self) -> Result<Option<InboundChunk>> {
        loop {
            let notified = self.inbound_notify.notified();
            let mut inbound = self.inbound.lock().await;

            if let Some(error_code) = inbound.reset_error {
                return Err(Error::StreamReset(error_code));
            }

            if let Some(chunk) = inbound.chunks.pop_front() {
                return Ok(Some(chunk));
            }

            if inbound.fin_received || self.recv_terminal.load(Ordering::Acquire) {
                inbound.fin_received = true;
                return Ok(None);
            }

            if inbound.connection_closed {
                return Err(Error::ConnectionClosed);
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

#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub fn fuzz_connection_state(is_client: bool, frames: &[Vec<u8>]) {
    use tokio::io::AsyncWriteExt;

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("fuzz runtime");

    runtime.block_on(async {
        let (transport, mut peer) = tokio::io::duplex(128 * 1024);
        let limits = Limits {
            max_frame_size: 1024,
            max_open_streams: 16,
            max_inbound_connection_bytes: 4096,
            max_outbound_connection_bytes: 4096,
            max_inbound_stream_bytes: 512,
            max_outbound_stream_bytes: 512,
            ..Limits::default()
        };
        let connection = Connection::new(transport, limits, is_client, None, None, None)
            .expect("fuzz connection");

        for frame in frames.iter().take(64) {
            let Ok(frame_len) = u32::try_from(frame.len()) else {
                continue;
            };
            if peer.write_all(&frame_len.to_be_bytes()).await.is_err()
                || peer.write_all(frame).await.is_err()
            {
                break;
            }
        }
        drop(peer);

        tokio::time::timeout(Duration::from_secs(1), connection.wait_closed())
            .await
            .expect("connection tasks must terminate after transport EOF");
    });
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

    let mut tasks = tokio::task::JoinSet::new();
    let cleanup_shared = shared.clone();
    tasks.spawn(async move {
        loop {
            tokio::select! {
                biased;
                () = cleanup_shared.wait_terminated() => break,
                () = cleanup_shared.drop_notify.notified() => {}
            }
            let streams: Vec<_> = cleanup_shared
                .streams
                .lock()
                .await
                .iter()
                .map(|(id, state)| (*id, state.clone()))
                .collect();
            for (id, state) in streams {
                if state.send_dropped.swap(false, Ordering::AcqRel) {
                    cleanup_shared.handle_last_send_drop(id, &state).await;
                }
                if state.recv_dropped.swap(false, Ordering::AcqRel) {
                    cleanup_shared.handle_last_recv_drop(id, &state).await;
                }
            }
        }
    });
    let reader_shared = shared.clone();
    tasks.spawn(async move {
        info!("reader task started");
        loop {
            let item = tokio::select! {
                biased;
                () = reader_shared.wait_terminated() => break,
                item = async {
                    match idle_timeout {
                        Some(timeout) => match tokio::time::timeout(timeout, source.next()).await {
                            Ok(item) => item,
                            Err(_) => {
                                warn!(?timeout, "connection idle timeout elapsed");
                                let _ = reader_shared.initiate_close(1, "connection idle timeout elapsed".to_owned()).await;
                                None
                            }
                        },
                        None => source.next().await,
                    }
                } => item,
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
        tasks.spawn(async move {
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
                            keepalive_shared.mark_closed().await;
                            break;
                        }
                    }
                    () = closed => break,
                }
            }
        });
    }

    let writer_shared = shared.clone();
    tasks.spawn(async move {
        info!("writer task started");
        while let Some(frame) = writer_shared.writer.next_frame().await {
            let mut encoded = BytesMut::new();
            match frame.encode(&mut encoded) {
                Ok(()) => {
                    let encoded_len = encoded.len();
                    let result = tokio::select! {
                        biased;
                        () = writer_shared.wait_terminated() => break,
                        result = sink.send(encoded.freeze()) => result,
                    };
                    if let Err(error) = result {
                        warn!(%error, "writer send failure");
                        break;
                    }
                    writer_shared.record_sent(encoded_len);
                    let terminal_id = match &frame {
                        Frame::Stream {
                            stream_id,
                            fin: true,
                            ..
                        }
                        | Frame::ResetStream { stream_id, .. } => Some(stream_id.into_inner()),
                        _ => None,
                    };
                    if let Some(id) = terminal_id {
                        if let Some(state) = writer_shared.streams.lock().await.get(&id) {
                            state.send_complete.store(true, Ordering::Release);
                        }
                        writer_shared.try_retire_stream(id).await;
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

        tokio::select! {
            biased;
            () = writer_shared.wait_terminated() => {},
            result = sink.close() => {
                if let Err(error) = result { warn!(%error, "writer sink close failed"); }
            }
        }

        writer_shared.mark_closed().await;
        info!("writer task exited");
    });
    tokio::spawn(async move {
        let deadline = async {
            loop {
                let notified = shared.close_notify.notified();
                if shared.closed.load(Ordering::Acquire) {
                    break;
                }
                if shared.connection_handles.load(Ordering::Acquire) == 0 {
                    let _ = shared
                        .initiate_close(0, "last connection handle dropped".to_owned())
                        .await;
                    break;
                }
                notified.await;
            }
            tokio::time::sleep(shared.limits.drain_timeout).await;
            shared.mark_closed().await;
        };
        tokio::pin!(deadline);
        let mut expired = false;
        while !tasks.is_empty() {
            tokio::select! {
                result = tasks.join_next() => {
                    if result.is_some_and(|result| result.is_err()) { shared.mark_closed().await; }
                }
                () = &mut deadline, if !expired => { expired = true; }
            }
        }
        shared.mark_closed().await;
        // Release queued incoming stream handles as well as both I/O halves.
        shared.incoming_streams.lock().await.clear();
        shared.joined.store(true, Ordering::Release);
        shared.close_notify.notify_waiters();
    });
}

async fn handle_incoming_frame(shared: Arc<ConnectionShared>, bytes: BytesMut) -> Result<bool> {
    let mut bytes = bytes.freeze();
    let frame = Frame::decode(&mut bytes)?;

    if !matches!(frame, Frame::Settings { .. }) && !shared.writer.queues.lock().await.peer_settings
    {
        return Err(Error::Protocol(
            "SETTINGS must be the first frame".to_owned(),
        ));
    }
    match frame {
        Frame::Settings {
            max_data,
            max_stream_data,
            max_frame_size,
        } => {
            let mut queues = shared.writer.queues.lock().await;
            if queues.peer_settings
                || max_frame_size.into_inner() < Limits::MIN_FRAME_SIZE as u64
                || max_frame_size.into_inner() > u32::MAX as u64
            {
                return Err(Error::Protocol("invalid or repeated SETTINGS".to_owned()));
            }
            queues.peer_settings = true;
            queues.peer_limit = max_data.into_inner();
            queues.peer_initial_stream_limit = max_stream_data.into_inner();
            queues.peer_max_frame = max_frame_size.into_inner() as usize;
            drop(queues);
            shared.writer.notify.notify_one();
        }
        Frame::MaxData { maximum } => {
            let mut queues = shared.writer.queues.lock().await;
            queues.peer_limit = queues.peer_limit.max(maximum.into_inner());
            drop(queues);
            shared.writer.notify.notify_one();
        }
        Frame::MaxStreamData { stream_id, maximum } => {
            let id = stream_id.into_inner();
            let streams = shared.streams.lock().await;
            if let Some(state) = streams.get(&id) {
                state
                    .peer_limit
                    .fetch_max(maximum.into_inner(), Ordering::AcqRel);
            } else {
                let next = if id % 2 == shared.local_parity {
                    &shared.next_local_stream_id
                } else {
                    &shared.next_remote_stream_id
                };
                if id >= next.load(Ordering::Acquire) {
                    return Err(Error::Protocol("credit for unopened stream".to_owned()));
                }
            }
            shared.writer.notify.notify_one();
        }
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
        Frame::StopSending { stream_id } => {
            let id = stream_id.into_inner();
            let state = shared.streams.lock().await.get(&id).cloned();
            if let Some(state) = state {
                shared.reset_stream(id, &state, 0).await?;
            } else {
                let next = if id % 2 == shared.local_parity {
                    &shared.next_local_stream_id
                } else {
                    &shared.next_remote_stream_id
                };
                if id >= next.load(Ordering::Acquire) {
                    return Err(Error::Protocol(
                        "STOP_SENDING for unopened stream".to_owned(),
                    ));
                }
            }
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
        let state = StreamState::new(1, 1, permit, 1);

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
    #[tokio::test]
    async fn outbound_budget_rejects_impossible_chunks_and_ignores_empty_writes() {
        use bytes::Bytes;
        use std::future::Future;
        use std::task::{Context, Poll, Waker};
        let (io, _peer) = tokio::io::duplex(64);
        let limits = crate::Limits {
            max_outbound_stream_bytes: 8,
            ..Default::default()
        };
        let conn = super::Connection::new(io, limits, true, None, None, None).unwrap();
        let (send, _recv) = conn.open_bi().await.unwrap();
        let mut oversized = Box::pin(send.write_chunk(Bytes::from_static(b"123456789")));
        let mut cx = Context::from_waker(Waker::noop());
        assert!(matches!(
            oversized.as_mut().poll(&mut cx),
            Poll::Ready(Err(Error::LimitExceeded(_)))
        ));
        for _ in 0..1000 {
            send.write_chunk(Bytes::new()).await.unwrap();
        }
        assert!(conn.shared.writer.queues.lock().await.by_stream.is_empty());
    }

    #[tokio::test]
    async fn cancelled_write_never_reports_a_previous_buffers_length() {
        use std::pin::Pin;
        use std::task::{Context, Poll, Waker};
        use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
        let (io, _peer) = tokio::io::duplex(64);
        let conn = super::Connection::new(io, Default::default(), true, None, None, None).unwrap();
        let (mut send, mut recv) = conn.open_bi().await.unwrap();
        let permits = conn
            .shared
            .outbound_conn_bytes
            .clone()
            .acquire_many_owned(conn.shared.limits.max_outbound_connection_bytes as u32)
            .await
            .unwrap();
        let mut cx = Context::from_waker(Waker::noop());
        assert!(matches!(
            Pin::new(&mut send).poll_write(&mut cx, b"first-buffer"),
            Poll::Ready(Ok(12))
        ));
        assert!(
            Pin::new(&mut send)
                .poll_write(&mut cx, b"cancelled")
                .is_pending()
        );
        drop(permits);
        assert!(matches!(
            Pin::new(&mut send).poll_write(&mut cx, b"x"),
            Poll::Ready(Ok(1))
        ));
        assert!(matches!(
            Pin::new(&mut send).poll_flush(&mut cx),
            Poll::Ready(Ok(()))
        ));
        let queues = conn.shared.writer.queues.lock().await;
        let chunks = &queues.by_stream[&send.id()];
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].payload.as_ref(), b"first-buffer");
        assert_eq!(chunks[1].payload.as_ref(), b"x");
        let mut empty = ReadBuf::new(&mut []);
        assert!(matches!(
            Pin::new(&mut recv).poll_read(&mut cx, &mut empty),
            Poll::Ready(Ok(()))
        ));
    }
    #[tokio::test]
    async fn terminal_shutdown_releases_reader_even_when_peer_is_silent() {
        let (io, _silent_peer) = tokio::io::duplex(1024);
        let conn = super::Connection::new(io, Default::default(), true, None, None, None).unwrap();
        let shared = std::sync::Arc::downgrade(&conn.shared);
        conn.close("done").await.unwrap();
        conn.wait_closed().await;
        drop(conn);
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while shared.upgrade().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("all connection tasks must release their state");
    }
    #[tokio::test]
    async fn control_queue_is_bounded_even_when_the_writer_cannot_progress() {
        let writer =
            super::WriterState::new(2, std::sync::Arc::new(crate::flow::ReceiveWindow::new(32)));
        assert!(
            writer
                .enqueue_control(muxtls_proto::Frame::OpenStream {
                    stream_id: VarInt::from_u64(0).unwrap()
                })
                .await
        );
        assert!(
            writer
                .enqueue_control(muxtls_proto::Frame::OpenStream {
                    stream_id: VarInt::from_u64(0).unwrap()
                })
                .await
        );
        for _ in 0..1000 {
            assert!(
                !writer
                    .enqueue_control(muxtls_proto::Frame::OpenStream {
                        stream_id: VarInt::from_u64(0).unwrap()
                    })
                    .await
            );
        }
        assert_eq!(writer.queues.lock().await.control.len(), 2);
        assert!(writer.next_frame().await.is_some());
        assert!(
            writer
                .enqueue_control(muxtls_proto::Frame::OpenStream {
                    stream_id: VarInt::from_u64(0).unwrap()
                })
                .await
        );
    }
    #[tokio::test]
    async fn cancelled_open_does_not_consume_ids_or_leak_streams() {
        use std::future::Future;
        use std::sync::atomic::Ordering;
        use std::task::{Context, Waker};
        let (io, _peer) = tokio::io::duplex(1024);
        let conn = super::Connection::new(io, Default::default(), true, None, None, None).unwrap();
        let queue_guard = conn.shared.writer.queues.lock().await;
        {
            let mut open = Box::pin(conn.open_bi());
            assert!(
                open.as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
        }
        assert_eq!(conn.shared.next_local_stream_id.load(Ordering::Acquire), 0);
        assert!(conn.shared.streams.lock().await.is_empty());
        assert_eq!(
            conn.shared.open_streams.available_permits(),
            conn.shared.limits.max_open_streams
        );
        drop(queue_guard);
        let (send, _recv) = conn.open_bi().await.unwrap();
        assert_eq!(send.id(), 0);
    }
    #[tokio::test]
    async fn accepting_a_queued_stream_has_no_post_dequeue_cancellation_point() {
        use std::future::Future;
        use std::task::{Context, Poll, Waker};
        let (io, _peer) = tokio::io::duplex(1024);
        let conn = super::Connection::new(io, Default::default(), true, None, None, None).unwrap();
        conn.shared.on_remote_open(1).await.unwrap();
        let _streams_guard = conn.shared.streams.lock().await;
        let mut accept = Box::pin(conn.accept_bi());
        match accept
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
        {
            Poll::Ready(Ok((send, recv))) => {
                assert_eq!(send.id(), 1);
                assert_eq!(recv.id(), 1);
            }
            _ => panic!("a dequeued stream must be returned without another await"),
        }
    }
}

#[cfg(test)]
#[path = "regression_tests.rs"]
mod regression_tests;
