//! Bounded WebSocket byte transport with active peer liveness checks.
use bytes::Bytes;
use futures_util::{Sink, SinkExt, Stream, StreamExt};
use std::{
    io,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, DuplexStream},
    sync::{mpsc, watch},
    time::Instant,
};

pub const MAX_FRAME: usize = 64 * 1024;
pub const PIPE_BUFFER: usize = 64 * 1024;
/// Only binary application frames are accepted. Text never becomes tunnel data.
pub enum Frame {
    Binary(Bytes),
    Ping(Bytes),
    Pong(Bytes),
    Close,
}
pub trait Socket:
    Stream<Item = io::Result<Frame>> + Sink<Frame, Error = io::Error> + Unpin + Send
{
}
impl<T> Socket for T where
    T: Stream<Item = io::Result<Frame>> + Sink<Frame, Error = io::Error> + Unpin + Send
{
}
fn timed_out() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "supervisor transport stalled")
}

pub async fn bridge(socket: impl Socket, pipe: DuplexStream) -> io::Result<()> {
    let (mut sink, mut source) = socket.split();
    let (mut read, mut write) = tokio::io::split(pipe);
    let (control_tx, mut control_rx) = mpsc::channel(4);
    let (ack_tx, ack_rx) = watch::channel(Instant::now());
    let nonce = Arc::new(AtomicU64::new(0));
    let expected = nonce.clone();
    let receive = async move {
        while let Some(frame) = source.next().await {
            match frame? {
                Frame::Binary(data) => {
                    if data.len() > MAX_FRAME {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "oversized tunnel frame",
                        ));
                    }
                    tokio::time::timeout(Duration::from_secs(10), write.write_all(&data))
                        .await
                        .map_err(|_| timed_out())??;
                }
                Frame::Ping(data) => {
                    if data.len() > 125 {
                        return Err(io::Error::new(io::ErrorKind::InvalidData, "oversized ping"));
                    }
                    control_tx
                        .send(Frame::Pong(data))
                        .await
                        .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))?;
                }
                Frame::Pong(data) => {
                    if data.as_ref() == expected.load(Ordering::Relaxed).to_be_bytes() {
                        ack_tx.send_replace(Instant::now());
                    }
                }
                Frame::Close => return Ok(()),
            }
        }
        Ok(())
    };
    let transmit = async move {
        let mut tick = tokio::time::interval(Duration::from_secs(10));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut buffer = vec![0u8; 16 * 1024];
        loop {
            let frame = tokio::select! {
                _=tick.tick()=>{
                    if ack_rx.borrow().elapsed()>Duration::from_secs(30) {return Err(timed_out());}
                    let n=nonce.fetch_add(1,Ordering::Relaxed).wrapping_add(1);
                    Frame::Ping(Bytes::copy_from_slice(&n.to_be_bytes()))
                }
                Some(frame)=control_rx.recv()=>frame,
                count=read.read(&mut buffer)=>{
                    let count=count?;if count==0 {return Ok(());}
                    Frame::Binary(Bytes::copy_from_slice(&buffer[..count]))
                }
            };
            tokio::time::timeout(Duration::from_secs(10), sink.send(frame))
                .await
                .map_err(|_| timed_out())??;
        }
    };
    tokio::select! { result=receive=>result,result=transmit=>result }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        pin::Pin,
        sync::Mutex,
        task::{Context, Poll},
    };
    struct Silent {
        received: mpsc::Receiver<io::Result<Frame>>,
        sent: Arc<Mutex<Vec<Frame>>>,
    }
    impl Stream for Silent {
        type Item = io::Result<Frame>;
        fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            self.received.poll_recv(cx)
        }
    }
    impl Sink<Frame> for Silent {
        type Error = io::Error;
        fn poll_ready(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn start_send(self: Pin<&mut Self>, frame: Frame) -> io::Result<()> {
            self.sent.lock().unwrap().push(frame);
            Ok(())
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }
    #[tokio::test(start_paused = true)]
    async fn missing_matching_pong_terminates_a_stalled_connection() {
        let (tx, rx) = mpsc::channel(4);
        let sent = Arc::new(Mutex::new(Vec::new()));
        let (_peer, pipe) = tokio::io::duplex(PIPE_BUFFER);
        let task = tokio::spawn(bridge(
            Silent {
                received: rx,
                sent: sent.clone(),
            },
            pipe,
        ));
        tokio::task::yield_now().await;
        assert!(sent
            .lock()
            .unwrap()
            .iter()
            .any(|f| matches!(f, Frame::Ping(_))));
        // Unsolicited/stale acknowledgements do not keep a dead peer alive.
        tx.send(Ok(Frame::Pong(Bytes::copy_from_slice(&0u64.to_be_bytes()))))
            .await
            .unwrap();
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(41)).await;
        assert_eq!(
            task.await.unwrap().unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
    }
}
