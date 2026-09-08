//! Authenticated supervisor transport. API authentication is performed by the
//! caller before reserving a node connection. This crate never accesses registry
//! storage, accepts arbitrary network targets, or grants HTTP API permissions.
pub mod socket;
mod worker;

use futures_util::future::poll_fn;
use socket::{bridge, Socket, PIPE_BUFFER};
use std::{
    collections::HashMap,
    io,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf},
    sync::{mpsc, oneshot, OwnedSemaphorePermit, Semaphore},
};
use tokio_util::{
    compat::{Compat, FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt},
    sync::CancellationToken,
};
pub use worker::run_worker;

pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");
pub const PROTOCOL: &str = "h3s.tunnel.v1";
pub const MAX_STREAMS: usize = 16;
pub const MAX_NODES: usize = 64;
pub const MAX_OPEN_STREAMS: usize = 8;
type RawStream = Compat<yamux::Stream>;
#[derive(Debug)]
pub struct TunnelStream {
    inner: RawStream,
    _permit: OwnedSemaphorePermit,
}
impl AsyncRead for TunnelStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}
impl AsyncWrite for TunnelStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, data)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}
type Reply = oneshot::Sender<io::Result<RawStream>>;
const KUBELET: u8 = 1;

pub(crate) fn mux_config() -> yamux::Config {
    let mut c = yamux::Config::default();
    c.set_max_num_streams(MAX_STREAMS)
        .set_max_connection_receive_window(Some(MAX_STREAMS * 256 * 1024))
        .set_read_after_close(false);
    c
}
fn disconnected() -> io::Error {
    io::Error::new(
        io::ErrorKind::NotConnected,
        "node supervisor is not connected",
    )
}
fn protocol_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "unexpected supervisor protocol message",
    )
}

struct Entry {
    opens: mpsc::Sender<Reply>,
    slots: Arc<Semaphore>,
    stop: CancellationToken,
}
/// One active transport per authenticated node, bounded independently of HTTP.
#[derive(Default)]
pub struct Hub {
    nodes: Mutex<HashMap<String, Entry>>,
}
#[derive(Debug, thiserror::Error)]
pub enum ReserveError {
    #[error("node supervisor already connected")]
    AlreadyConnected,
    #[error("supervisor capacity reached")]
    Capacity,
}
/// Reservation ownership covers both upgrade and socket lifetime. A failed
/// upgrade or cancellation drops it and removes exactly this active node entry.
pub struct Registration {
    hub: Arc<Hub>,
    node: String,
    opens: mpsc::Receiver<Reply>,
    stop: CancellationToken,
}
impl Drop for Registration {
    fn drop(&mut self) {
        self.stop.cancel();
        eprintln!("h3s supervisor node={}: connection released", self.node);
        self.hub
            .nodes
            .lock()
            .expect("supervisor lock")
            .remove(&self.node);
    }
}
impl Hub {
    pub fn reserve(self: &Arc<Self>, node: &str) -> Result<Registration, ReserveError> {
        let mut nodes = self.nodes.lock().expect("supervisor lock");
        if nodes.contains_key(node) {
            return Err(ReserveError::AlreadyConnected);
        }
        if nodes.len() >= MAX_NODES {
            return Err(ReserveError::Capacity);
        }
        let (tx, rx) = mpsc::channel(MAX_STREAMS);
        let stop = CancellationToken::new();
        nodes.insert(
            node.into(),
            Entry {
                opens: tx,
                slots: Arc::new(Semaphore::new(MAX_OPEN_STREAMS)),
                stop: stop.clone(),
            },
        );
        Ok(Registration {
            hub: self.clone(),
            node: node.into(),
            opens: rx,
            stop,
        })
    }
    pub fn connected(&self, node: &str) -> bool {
        self.nodes
            .lock()
            .expect("supervisor lock")
            .get(node)
            .is_some_and(|e| !e.stop.is_cancelled())
    }
    /// Called only after the API caller authorizes the requested kubelet action.
    /// The worker receives a fixed service discriminator, never a host/port/path.
    pub async fn open_kubelet(&self, node: &str) -> io::Result<TunnelStream> {
        let (sender, slots) = self
            .nodes
            .lock()
            .expect("supervisor lock")
            .get(node)
            .filter(|e| !e.stop.is_cancelled())
            .map(|e| (e.opens.clone(), e.slots.clone()))
            .ok_or_else(disconnected)?;
        let permit = slots.try_acquire_owned().map_err(|_| {
            io::Error::new(
                io::ErrorKind::WouldBlock,
                "supervisor active stream capacity reached",
            )
        })?;
        let (tx, rx) = oneshot::channel();
        sender.try_send(tx).map_err(|_| {
            io::Error::new(
                io::ErrorKind::WouldBlock,
                "supervisor open capacity reached",
            )
        })?;
        tokio::time::timeout(Duration::from_secs(5), async {
            let mut stream = rx.await.map_err(|_| disconnected())??;
            stream.write_u8(KUBELET).await?;
            stream.flush().await?;
            match stream.read_u8().await? {
                1 => Ok(TunnelStream {
                    inner: stream,
                    _permit: permit,
                }),
                0 => Err(io::Error::new(
                    io::ErrorKind::ConnectionRefused,
                    "worker kubelet is unavailable",
                )),
                _ => Err(protocol_error()),
            }
        })
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "supervisor open timed out"))?
    }
}
impl Registration {
    pub async fn serve(mut self, socket: impl Socket) -> io::Result<()> {
        eprintln!("h3s supervisor node={}: connected", self.node);
        let (io, pipe) = tokio::io::duplex(PIPE_BUFFER);
        let mut conn = yamux::Connection::new(io.compat(), mux_config(), yamux::Mode::Client);
        let mut pending: Option<Reply> = None;
        let mux = async {
            loop {
                enum Event {
                    Request(Option<Reply>),
                    Open(io::Result<RawStream>),
                    End(io::Result<()>),
                }
                let event = poll_fn(|cx| {
                    if pending.as_ref().is_some_and(|reply| reply.is_closed()) {
                        pending.take();
                    }
                    // This drives all open streams even while an outbound open waits.
                    match conn.poll_next_inbound(cx) {
                        Poll::Ready(Some(Ok(_))) => {
                            return Poll::Ready(Event::End(Err(protocol_error())))
                        }
                        Poll::Ready(Some(Err(_))) => {
                            return Poll::Ready(Event::End(Err(disconnected())))
                        }
                        Poll::Ready(None) => return Poll::Ready(Event::End(Ok(()))),
                        Poll::Pending => {}
                    }
                    if pending.is_some() {
                        if let Poll::Ready(result) = conn.poll_new_outbound(cx) {
                            return Poll::Ready(Event::Open(
                                result.map(|s| s.compat()).map_err(|_| disconnected()),
                            ));
                        }
                    } else if let Poll::Ready(request) = self.opens.poll_recv(cx) {
                        return Poll::Ready(Event::Request(request));
                    }
                    Poll::Pending
                })
                .await;
                match event {
                    Event::Request(Some(reply)) => pending = Some(reply),
                    Event::Request(None) => return Ok(()),
                    Event::Open(stream) => {
                        let _ = pending.take().expect("pending stream open").send(stream);
                    }
                    Event::End(result) => return result,
                }
            }
        };
        tokio::select! {
            _=self.stop.cancelled()=>Ok(()),
            result=mux=>result,
            result=bridge(socket,pipe)=>result,
        }
    }
}
