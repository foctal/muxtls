use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::connection::{ConnectionShared, InboundChunk, StreamState};
use crate::error::{Error, Result};

type WriteFuture = Pin<Box<dyn Future<Output = Result<()>> + Send + 'static>>;
type ReadFuture = Pin<Box<dyn Future<Output = Result<Option<InboundChunk>>> + Send + 'static>>;

/// Writable half of a bidirectional stream.
///
/// `AsyncWrite` buffers at most one frame per handle. Flush before dropping
/// the handle to deliver accepted bytes. A stream direction selects either
/// chunk operations or AsyncWrite on first use; mixing these APIs returns an
/// error. Stream halves have unique ownership and cannot be cloned. Use AsyncWrite::poll_shutdown for FIN
/// after AsyncWrite, and `finish` after chunk writes.
pub struct SendStream {
    pub(crate) stream_id: u64,
    pub(crate) state: std::sync::Arc<StreamState>,
    pub(crate) shared: std::sync::Arc<ConnectionShared>,
    write_fut: Option<WriteFuture>,
    shutdown_fut: Option<WriteFuture>,
    shutdown_complete: bool,
}

/// Readable half of a bidirectional stream.
///
/// A direction selects chunk reads or AsyncRead on first use. Mixing these
/// APIs returns an error rather than skipping data. Receive halves cannot be
/// cloned, so an unread remainder always has exactly one owner.
pub struct RecvStream {
    pub(crate) stream_id: u64,
    pub(crate) state: std::sync::Arc<StreamState>,
    pub(crate) shared: std::sync::Arc<ConnectionShared>,
    read_fut: Option<ReadFuture>,
    read_buf: Option<InboundChunk>,
    read_pos: usize,
    eof: bool,
}

impl SendStream {
    pub(crate) fn new(
        stream_id: u64,
        state: std::sync::Arc<StreamState>,
        shared: std::sync::Arc<ConnectionShared>,
    ) -> Self {
        state.add_send_handle();
        Self {
            stream_id,
            state,
            shared,
            write_fut: None,
            shutdown_fut: None,
            shutdown_complete: false,
        }
    }

    /// Returns the stream identifier.
    pub fn id(&self) -> u64 {
        self.stream_id
    }

    /// Writes one chunk to this stream.
    ///
    /// The call applies per-stream and per-connection backpressure. Chunks larger
    /// than the stream byte budget are rejected instead of waiting indefinitely.
    pub fn write_chunk(&self, chunk: Bytes) -> impl Future<Output = Result<()>> + Send + '_ {
        let shared = self.shared.clone();
        let state = self.state.clone();
        let stream_id = self.stream_id;
        async move {
            select_mode(&state.send_mode, 1)?;
            shared
                .send_stream_chunk(stream_id, &state, chunk, false)
                .await
        }
    }

    /// Sends a FIN for this stream.
    pub fn finish(&self) -> impl Future<Output = Result<()>> + Send + '_ {
        let shared = self.shared.clone();
        let state = self.state.clone();
        let stream_id = self.stream_id;
        async move {
            select_mode(&state.send_mode, 1)?;
            shared
                .send_stream_chunk(stream_id, &state, Bytes::new(), true)
                .await
        }
    }

    /// Abruptly resets this stream with an application-defined code.
    pub fn reset(&self, error_code: u64) -> impl Future<Output = Result<()>> + Send + '_ {
        let shared = self.shared.clone();
        let state = self.state.clone();
        let stream_id = self.stream_id;
        async move { shared.reset_stream(stream_id, &state, error_code).await }
    }
}

impl RecvStream {
    pub(crate) fn new(
        stream_id: u64,
        state: std::sync::Arc<StreamState>,
        shared: std::sync::Arc<ConnectionShared>,
    ) -> Self {
        state.add_recv_handle();
        Self {
            stream_id,
            state,
            shared,
            read_fut: None,
            read_buf: None,
            read_pos: 0,
            eof: false,
        }
    }

    /// Returns the stream identifier.
    pub fn id(&self) -> u64 {
        self.stream_id
    }

    /// Reads the next available byte chunk. Boundaries may differ from writes.
    ///
    /// Returns `Ok(None)` when the peer has finished the stream.
    pub fn read_chunk(&self) -> impl Future<Output = Result<Option<Bytes>>> + Send + '_ {
        let state = self.state.clone();
        async move {
            select_mode(&state.recv_mode, 1)?;
            Ok(state.read_chunk().await?.map(InboundChunk::into_bytes))
        }
    }
}

impl Drop for SendStream {
    fn drop(&mut self) {
        if self.state.release_send_handle() {
            self.state
                .send_dropped
                .store(true, std::sync::atomic::Ordering::Release);
            self.shared.drop_notify.notify_one();
        }
    }
}

impl Drop for RecvStream {
    fn drop(&mut self) {
        if self.state.release_recv_handle() {
            self.state
                .recv_dropped
                .store(true, std::sync::atomic::Ordering::Release);
            self.shared.drop_notify.notify_one();
        }
    }
}

impl AsyncWrite for SendStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if let Err(error) = select_mode(&this.state.send_mode, 2) {
            return Poll::Ready(Err(to_io_error(error)));
        }
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }

        if this.shared.ensure_open().is_err()
            || this
                .state
                .send_terminal
                .load(std::sync::atomic::Ordering::Acquire)
            || this.shutdown_fut.is_some()
            || this.shutdown_complete
        {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "stream already shut down",
            )));
        }

        // A previous call already reported its owned chunk as accepted. Drain
        // it before accepting bytes from this call, whose buffer may differ.
        match Pin::new(&mut *this).poll_flush(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
            Poll::Ready(Ok(())) => {}
        }
        let max_payload = this.shared.max_stream_payload(this.stream_id);
        if max_payload == 0 {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "max frame size cannot encode stream data",
            )));
        }
        let chunk = Bytes::copy_from_slice(&buf[..buf.len().min(max_payload)]);
        let written = chunk.len();
        let shared = this.shared.clone();
        let state = this.state.clone();
        let stream_id = this.stream_id;
        this.write_fut = Some(Box::pin(async move {
            shared
                .send_stream_chunk(stream_id, &state, chunk, false)
                .await
        }));
        Poll::Ready(Ok(written))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if let Err(error) = select_mode(&this.state.send_mode, 2) {
            return Poll::Ready(Err(to_io_error(error)));
        }
        if let Some(fut) = this.write_fut.as_mut() {
            match fut.as_mut().poll(cx) {
                Poll::Ready(Ok(())) => {
                    this.write_fut = None;
                }
                Poll::Ready(Err(err)) => {
                    this.write_fut = None;
                    return Poll::Ready(Err(to_io_error(err)));
                }
                Poll::Pending => return Poll::Pending,
            }
        }

        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if let Err(error) = select_mode(&this.state.send_mode, 2) {
            return Poll::Ready(Err(to_io_error(error)));
        }
        if this.shutdown_complete {
            return Poll::Ready(Ok(()));
        }

        if let Some(fut) = this.write_fut.as_mut() {
            match fut.as_mut().poll(cx) {
                Poll::Ready(Ok(())) => {
                    this.write_fut = None;
                }
                Poll::Ready(Err(err)) => {
                    this.write_fut = None;
                    return Poll::Ready(Err(to_io_error(err)));
                }
                Poll::Pending => return Poll::Pending,
            }
        }

        if this.shutdown_fut.is_none() {
            let shared = this.shared.clone();
            let state = this.state.clone();
            let stream_id = this.stream_id;
            this.shutdown_fut = Some(Box::pin(async move {
                shared
                    .send_stream_chunk(stream_id, &state, Bytes::new(), true)
                    .await
            }));
        }

        let fut = this.shutdown_fut.as_mut().expect("shutdown future exists");
        match fut.as_mut().poll(cx) {
            Poll::Ready(Ok(())) => {
                this.shutdown_fut = None;
                this.shutdown_complete = true;
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(err)) => {
                this.shutdown_fut = None;
                Poll::Ready(Err(to_io_error(err)))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl AsyncRead for RecvStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if let Err(error) = select_mode(&this.state.recv_mode, 2) {
            return Poll::Ready(Err(to_io_error(error)));
        }
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }

        loop {
            if let Some(chunk) = this.read_buf.as_mut() {
                let count = chunk.data.len().min(buf.remaining());
                if count == 0 {
                    this.read_buf = None;
                    continue;
                }
                buf.put_slice(&chunk.data[..count]);
                chunk.data.advance(count);
                chunk.consume(count);
                if chunk.data.is_empty() {
                    this.read_buf = None;
                }
                return Poll::Ready(Ok(()));
            }

            if this.eof {
                return Poll::Ready(Ok(()));
            }

            if this.read_fut.is_none() {
                let state = this.state.clone();
                this.read_fut = Some(Box::pin(async move { state.read_chunk().await }));
            }

            let fut = this.read_fut.as_mut().expect("read future exists");
            match fut.as_mut().poll(cx) {
                Poll::Ready(Ok(Some(chunk))) => {
                    this.read_fut = None;
                    this.read_buf = Some(chunk);
                    this.read_pos = 0;
                }
                Poll::Ready(Ok(None)) => {
                    this.read_fut = None;
                    this.eof = true;
                    return Poll::Ready(Ok(()));
                }
                Poll::Ready(Err(err)) => {
                    this.read_fut = None;
                    return Poll::Ready(Err(to_io_error(err)));
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

fn to_io_error(err: Error) -> io::Error {
    io::Error::other(err.to_string())
}

fn select_mode(mode: &std::sync::atomic::AtomicUsize, requested: usize) -> Result<()> {
    use std::sync::atomic::Ordering;
    match mode.compare_exchange(0, requested, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => Ok(()),
        Err(existing) if existing == requested => Ok(()),
        Err(_) => Err(Error::Protocol(
            "cannot mix chunk and AsyncRead/AsyncWrite APIs on a stream direction".to_owned(),
        )),
    }
}
