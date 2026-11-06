//! The listener of the web app counts each connection once, when it closes. A connection
//! that sent no byte counts as `no_request`. A connection that sent at least one byte counts
//! as `requests`.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::telemetry::count_web_connection;

/// A stream that remembers if it read any byte. It counts the connection when it drops.
pub(crate) struct CountedStream<S> {
    inner: S,
    read_any: bool,
}

impl<S> CountedStream<S> {
    pub(crate) fn new(inner: S) -> Self {
        Self {
            inner,
            read_any: false,
        }
    }
}

impl<S> Drop for CountedStream<S> {
    fn drop(&mut self) {
        count_web_connection(if self.read_any {
            "requests"
        } else {
            "no_request"
        });
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncRead for CountedStream<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let poll = Pin::new(&mut self.inner).poll_read(cx, buf);
        if matches!(poll, Poll::Ready(Ok(()))) && buf.filled().len() > before {
            self.read_any = true;
        }
        poll
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncWrite for CountedStream<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
}

/// A listener that gives each accepted connection as a `CountedStream`.
pub(crate) struct CountedListener<L> {
    inner: L,
}

impl<L> CountedListener<L> {
    pub(crate) fn new(inner: L) -> Self {
        Self { inner }
    }
}

impl<L: axum::serve::Listener> axum::serve::Listener for CountedListener<L> {
    type Io = CountedStream<L::Io>;
    type Addr = L::Addr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        let (io, addr) = self.inner.accept().await;
        (CountedStream::new(io), addr)
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}
