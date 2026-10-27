//! Response deadlines measure pending IO, not total transfer duration.

use std::{
    io,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::TcpStream,
    time::Sleep,
};

pub(super) struct ProgressIo {
    stream: TcpStream,
    idle: Duration,
    pending: Option<Pin<Box<Sleep>>>,
}

impl ProgressIo {
    pub(super) fn new(stream: TcpStream, idle: Duration) -> Self {
        Self { stream, idle, pending: None }
    }

    fn stalled(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        use std::future::Future;
        let sleep = self.pending.get_or_insert_with(|| Box::pin(tokio::time::sleep(self.idle)));
        if sleep.as_mut().poll(cx).is_ready() {
            Poll::Ready(Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "response write made no progress",
            )))
        } else {
            Poll::Pending
        }
    }
}

impl AsyncRead for ProgressIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}

impl AsyncWrite for ProgressIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match Pin::new(&mut self.stream).poll_write(cx, buf) {
            Poll::Ready(result) => {
                if result.as_ref().is_ok_and(|count| *count != 0) {
                    self.pending = None;
                }
                Poll::Ready(result)
            }
            Poll::Pending => match self.stalled(cx) {
                Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
                _ => Poll::Pending,
            },
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match Pin::new(&mut self.stream).poll_flush(cx) {
            Poll::Ready(result) => {
                // TCP flush is a no-op and cannot count as progress on a blocked write.
                Poll::Ready(result)
            }
            Poll::Pending => self.stalled(cx),
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn progressing_tcp_writes_can_outlast_the_idle_interval() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let peer = TcpStream::connect(listener.local_addr().unwrap()).await.unwrap();
        let (stream, _) = listener.accept().await.unwrap();
        socket2::SockRef::from(&stream).set_send_buffer_size(4096).unwrap();
        let mut writer = ProgressIo::new(stream, Duration::from_millis(200));
        let expected = 16 * 65536;
        let reader = tokio::spawn(async move {
            let mut peer = peer;
            let mut received = 0;
            let mut chunk = vec![0; 65536];
            while received < expected {
                let count = peer.read(&mut chunk).await.unwrap();
                assert!(count != 0);
                assert!(chunk[..count].iter().all(|byte| *byte == 7));
                received += count;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            received
        });
        let started = std::time::Instant::now();
        for _ in 0..16 {
            writer.write_all(&[7; 65536]).await.unwrap();
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(reader.await.unwrap(), expected);
        assert!(started.elapsed() > Duration::from_millis(200));
    }
}
