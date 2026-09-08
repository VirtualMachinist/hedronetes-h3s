use crate::{
    mux_config,
    socket::{bridge, Frame, Socket, MAX_FRAME, PIPE_BUFFER},
    KUBELET, PROTOCOL,
};
use futures_util::{
    future::{poll_fn, ready},
    SinkExt, StreamExt,
};
use std::{io, net::SocketAddr, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    task::JoinSet,
};
use tokio_tungstenite::{
    connect_async_tls_with_config,
    tungstenite::{
        client::IntoClientRequest,
        protocol::{Message, WebSocketConfig},
    },
    Connector,
};
use tokio_util::compat::{FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt};
fn transport_error() -> io::Error {
    io::Error::other("supervisor WebSocket transport failed")
}

/// The production agent always selects 127.0.0.1:10250. A non-loopback target is
/// rejected even when this library is called directly. No CRI socket forwarding.
pub async fn run_worker(
    url: &str,
    tls: Arc<rustls::ClientConfig>,
    target: SocketAddr,
) -> io::Result<()> {
    if !target.ip().is_loopback() || !url.starts_with("wss://") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "supervisor requires WSS and loopback destination",
        ));
    }
    let mut request = url.into_client_request().map_err(|_| transport_error())?;
    if request
        .uri()
        .authority()
        .is_none_or(|a| a.as_str().contains('@'))
        || request.uri().path_and_query().map(|p| p.as_str()) != Some("/v1-h3s/connect")
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "supervisor requires a server authority and the fixed connect path",
        ));
    }
    request.headers_mut().insert(
        "sec-websocket-protocol",
        PROTOCOL.parse().expect("fixed protocol"),
    );
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_FRAME))
        .max_frame_size(Some(MAX_FRAME))
        .write_buffer_size(16 * 1024)
        .max_write_buffer_size(128 * 1024);
    let (socket, response) = tokio::time::timeout(
        Duration::from_secs(10),
        connect_async_tls_with_config(request, Some(config), true, Some(Connector::Rustls(tls))),
    )
    .await
    .map_err(|_| transport_error())?
    .map_err(|_| transport_error())?;
    if response
        .headers()
        .get("sec-websocket-protocol")
        .and_then(|v| v.to_str().ok())
        != Some(PROTOCOL)
    {
        return Err(transport_error());
    }
    let socket = socket
        .map(|m| match m.map_err(|_| transport_error())? {
            Message::Binary(b) => Ok(Frame::Binary(b)),
            Message::Ping(b) => Ok(Frame::Ping(b)),
            Message::Pong(b) => Ok(Frame::Pong(b)),
            Message::Close(_) => Ok(Frame::Close),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unexpected WebSocket message",
            )),
        })
        .sink_map_err(|_| transport_error())
        .with(|m: Frame| {
            ready(Ok::<_, io::Error>(match m {
                Frame::Binary(b) => Message::Binary(b),
                Frame::Ping(b) => Message::Ping(b),
                Frame::Pong(b) => Message::Pong(b),
                Frame::Close => Message::Close(None),
            }))
        });
    eprintln!("h3s supervisor connected");
    serve_worker(socket, target).await
}
pub(crate) async fn serve_worker(socket: impl Socket, target: SocketAddr) -> io::Result<()> {
    let (io, pipe) = tokio::io::duplex(PIPE_BUFFER);
    let mut conn = yamux::Connection::new(io.compat(), mux_config(), yamux::Mode::Server);
    let mut tasks = JoinSet::new();
    let mux = async {
        loop {
            tokio::select! {
                next=poll_fn(|cx|conn.poll_next_inbound(cx))=>match next {
                    Some(Ok(stream))=>{
                        if tasks.len()>=crate::MAX_STREAMS {drop(stream);continue;}
                        tasks.spawn(forward(stream,target));
                    }
                    Some(Err(_))=>return Err(transport_error()),None=>return Ok(()),
                },
                Some(_)=tasks.join_next(),if !tasks.is_empty()=>{},
            }
        }
    };
    tokio::select! { result=mux=>result,result=bridge(socket,pipe)=>result }
}
async fn forward(stream: yamux::Stream, target: SocketAddr) -> io::Result<()> {
    let mut stream = stream.compat();
    let requested = tokio::time::timeout(Duration::from_secs(3), stream.read_u8())
        .await
        .map_err(|_| transport_error())??;
    if requested != KUBELET {
        stream.write_u8(0).await?;
        stream.shutdown().await?;
        return Ok(());
    }
    let connected = tokio::time::timeout(Duration::from_secs(3), TcpStream::connect(target)).await;
    let mut socket = match connected {
        Ok(Ok(socket)) => socket,
        _ => {
            stream.write_u8(0).await?;
            stream.shutdown().await?;
            return Ok(());
        }
    };
    stream.write_u8(1).await?;
    stream.flush().await?;
    tokio::io::copy_bidirectional(&mut stream, &mut socket).await?;
    Ok(())
}
