//! Certificate-only native worker tunnel. Kubernetes API RBAC cannot turn an
//! ordinary client or administrator into a node tunnel identity.
use crate::{key, transport::ConnectionContext, Api, Failure, Result};
use axum::{
    body::Body,
    extract::{ws::Message, FromRequestParts, WebSocketUpgrade},
    http::Request,
    response::Response,
};
use futures_util::{future::ready, SinkExt, StreamExt};
use h3s_auth::User;
use h3s_supervisor::{
    socket::{Frame, MAX_FRAME},
    ReserveError, PROTOCOL,
};
use std::io;

pub async fn connect(api: &Api, user: &User, request: Request<Body>) -> Result<Response> {
    let node = user.node_name().ok_or_else(|| {
        Failure::new(
            403,
            "Forbidden",
            "supervisor requires an enrolled node certificate",
        )
    })?;
    if request.method() != "GET"
        || request.uri().query().is_some()
        || request.headers().contains_key("origin")
    {
        return Err(Failure::new(
            400,
            "BadRequest",
            "supervisor requires a native WebSocket GET without query or Origin",
        ));
    }
    let protocols: Vec<_> = request
        .headers()
        .get_all("sec-websocket-protocol")
        .iter()
        .collect();
    if protocols.len() != 1 || protocols[0].to_str().ok() != Some(PROTOCOL) {
        return Err(Failure::new(
            400,
            "BadRequest",
            "unsupported supervisor protocol",
        ));
    }
    if api
        .store
        .get(&key(format!("/registry/h3s-node-identities/{node}"))?)
        .await?
        .is_none()
        || api
            .store
            .get(&key(format!("/registry/nodes/{node}"))?)
            .await?
            .is_none()
    {
        return Err(Failure::new(
            403,
            "Forbidden",
            "supervisor requires registered node enrollment",
        ));
    }
    let context = request
        .extensions()
        .get::<ConnectionContext>()
        .cloned()
        .ok_or_else(|| Failure::new(500, "InternalError", "missing connection lifecycle"))?;
    let (mut parts, _) = request.into_parts();
    let ws = WebSocketUpgrade::from_request_parts(&mut parts, &())
        .await
        .map_err(|_| Failure::new(400, "BadRequest", "invalid WebSocket upgrade"))?;
    let registration = api.supervisor.reserve(node).map_err(|e| match e {
        ReserveError::AlreadyConnected => {
            Failure::new(409, "Conflict", "node supervisor already connected")
        }
        ReserveError::Capacity => {
            Failure::new(429, "TooManyRequests", "supervisor capacity reached")
        }
    })?;
    Ok(ws
        .protocols([PROTOCOL])
        .max_frame_size(MAX_FRAME)
        .max_message_size(MAX_FRAME)
        .write_buffer_size(16 * 1024)
        .max_write_buffer_size(128 * 1024)
        .on_upgrade(move |socket| async move {
            let socket = socket
                .map(|m| {
                    match m.map_err(|_| io::Error::other("supervisor WebSocket receive failed"))? {
                        Message::Binary(b) => Ok(Frame::Binary(b)),
                        Message::Ping(b) => Ok(Frame::Ping(b)),
                        Message::Pong(b) => Ok(Frame::Pong(b)),
                        Message::Close(_) => Ok(Frame::Close),
                        Message::Text(_) => Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "unexpected WebSocket text",
                        )),
                    }
                })
                .sink_map_err(|_| io::Error::other("supervisor WebSocket send failed"))
                .with(|m: Frame| {
                    ready(Ok::<_, io::Error>(match m {
                        Frame::Binary(b) => Message::Binary(b),
                        Frame::Ping(b) => Message::Ping(b),
                        Frame::Pong(b) => Message::Pong(b),
                        Frame::Close => Message::Close(None),
                    }))
                });
            // Keep the TLS connection permit until the upgraded socket closes, and
            // terminate upgrades when the API process shuts down or serve is dropped.
            tokio::select! { _=context.shutdown.cancelled()=>{}, _=registration.serve(socket)=>{} }
            drop(context);
        }))
}
