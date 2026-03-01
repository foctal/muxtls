use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::connection::{ConnectionShared, StreamState};
use crate::error::{Error, Result};

type WriteFuture = Pin<Box<dyn Future<Output = Result<()>> + Send + 'static>>;
type ReadFuture = Pin<Box<dyn Future<Output = Result<Option<Bytes>>> + Send + 'static>>;

/// Writable half of a bidirectional stream.
pub struct SendStream {
    pub(crate) stream_id: u64,
    pub(crate) state: std::sync::Arc<StreamState>,
    pub(crate) shared: std::sync::Arc<ConnectionShared>,
    write_fut: Option<WriteFuture>,
    write_len: usize,
    shutdown_fut: Option<WriteFuture>,
}

/// Readable half of a bidirectional stream.
pub struct RecvStream {
    pub(crate) stream_id: u64,
    pub(crate) state: std::sync::Arc<StreamState>,
    pub(crate) shared: std::sync::Arc<ConnectionShared>,
    read_fut: Option<ReadFuture>,
    read_buf: Option<Bytes>,
    read_pos: usize,
    eof: bool,
}

impl Clone for SendStream {
    fn clone(&self) -> Self {
        self.state.add_send_handle();
        Self {
            stream_id: self.stream_id,
            state: self.state.clone(),
            shared: self.shared.clone(),
            write_fut: None,
            write_len: 0,
            shutdown_fut: None,
        }
    }
}

impl Clone for RecvStream {
    fn clone(&self) -> Self {
        self.state.add_recv_handle();
        Self {
            stream_id: self.stream_id,
            state: self.state.clone(),
            shared: self.shared.clone(),
            read_fut: None,
            read_buf: None,
            read_pos: 0,
            eof: false,
        }
    }
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
            write_len: 0,
            shutdown_fut: None,
        }
    }

    /// Returns the stream identifier.
    pub fn id(&self) -> u64 {
        self.stream_id
    }

    /// Writes one chunk to this stream.
    ///
    /// The call applies per-stream and per-connection backpressure.
    pub fn write_chunk(&self, chunk: Bytes) -> impl Future<Output = Result<()>> + Send + 'static {
        let shared = self.shared.clone();
        let state = self.state.clone();
        let stream_id = self.stream_id;
        async move {
            shared
                .send_stream_chunk(stream_id, &state, chunk, false)
                .await
        }
    }

    /// Sends a FIN for this stream.
    pub fn finish(&self) -> impl Future<Output = Result<()>> + Send + 'static {
        let shared = self.shared.clone();
        let state = self.state.clone();
        let stream_id = self.stream_id;
        async move {
            shared
                .send_stream_chunk(stream_id, &state, Bytes::new(), true)
                .await
        }
    }

    /// Abruptly resets this stream with an application-defined code.
    pub fn reset(&self, error_code: u64) -> impl Future<Output = Result<()>> + Send + 'static {
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

    /// Reads the next received chunk.
    ///
    /// Returns `Ok(None)` when the peer has finished the stream.
    pub fn read_chunk(&self) -> impl Future<Output = Result<Option<Bytes>>> + Send + 'static {
        let state = self.state.clone();
        async move { state.read_chunk().await }
    }
}

impl Drop for SendStream {
    fn drop(&mut self) {
        if !self.state.release_send_handle() {
            return;
        }

        let shared = self.shared.clone();
        let state = self.state.clone();
        let stream_id = self.stream_id;

        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                shared.handle_last_send_drop(stream_id, &state).await;
            });
        }
    }
}

impl Drop for RecvStream {
    fn drop(&mut self) {
        if !self.state.release_recv_handle() {
            return;
        }

        let shared = self.shared.clone();
        let state = self.state.clone();
        let stream_id = self.stream_id;

        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                shared.handle_last_recv_drop(stream_id, &state).await;
            });
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
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }

        if this.shutdown_fut.is_some() {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "stream already shut down",
            )));
        }

        if this.write_fut.is_none() {
            let chunk = Bytes::copy_from_slice(buf);
            this.write_len = chunk.len();
            let shared = this.shared.clone();
            let state = this.state.clone();
            let stream_id = this.stream_id;
            this.write_fut = Some(Box::pin(async move {
                shared
                    .send_stream_chunk(stream_id, &state, chunk, false)
                    .await
            }));
        }

        let fut = this.write_fut.as_mut().expect("write future exists");
        match fut.as_mut().poll(cx) {
            Poll::Ready(Ok(())) => {
                this.write_fut = None;
                let written = this.write_len;
                this.write_len = 0;
                Poll::Ready(Ok(written))
            }
            Poll::Ready(Err(err)) => {
                this.write_fut = None;
                this.write_len = 0;
                Poll::Ready(Err(to_io_error(err)))
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if let Some(fut) = this.write_fut.as_mut() {
            match fut.as_mut().poll(cx) {
                Poll::Ready(Ok(())) => {
                    this.write_fut = None;
                    this.write_len = 0;
                }
                Poll::Ready(Err(err)) => {
                    this.write_fut = None;
                    this.write_len = 0;
                    return Poll::Ready(Err(to_io_error(err)));
                }
                Poll::Pending => return Poll::Pending,
            }
        }

        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();

        if let Some(fut) = this.write_fut.as_mut() {
            match fut.as_mut().poll(cx) {
                Poll::Ready(Ok(())) => {
                    this.write_fut = None;
                    this.write_len = 0;
                }
                Poll::Ready(Err(err)) => {
                    this.write_fut = None;
                    this.write_len = 0;
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

        loop {
            if let Some(chunk) = this.read_buf.as_ref() {
                let available = chunk.len().saturating_sub(this.read_pos);
                if available == 0 {
                    this.read_buf = None;
                    this.read_pos = 0;
                    continue;
                }

                let to_copy = available.min(buf.remaining());
                if to_copy == 0 {
                    return Poll::Ready(Ok(()));
                }

                buf.put_slice(&chunk[this.read_pos..this.read_pos + to_copy]);
                this.read_pos += to_copy;

                if this.read_pos >= chunk.len() {
                    this.read_buf = None;
                    this.read_pos = 0;
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
