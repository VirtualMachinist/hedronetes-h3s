use axum::{Extension, Router};
use h3s_auth::User;
use hyper_util::{
    rt::{TokioExecutor, TokioIo},
    server::conn::auto,
    service::TowerToHyperService,
};
use std::{future::Future, io, sync::Arc, time::Duration};
use tokio::{
    net::TcpListener,
    sync::{OwnedSemaphorePermit, Semaphore},
    task::JoinSet,
};
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub(crate) struct Peer(pub Option<User>);

#[derive(Clone)]
pub(crate) struct ConnectionContext {
    pub shutdown: CancellationToken,
    _permit: Arc<OwnedSemaphorePermit>,
}

/// Identity is derived from the completed rustls handshake, never from headers.
/// Bound accepted connections and TLS handshake time; shutdown drops all peers.
pub async fn serve(
    listener: TcpListener,
    tls: rustls::ServerConfig,
    router: Router,
    shutdown: impl Future<Output = ()>,
) -> io::Result<()> {
    let lifecycle = CancellationToken::new();
    let _cancel_on_drop = lifecycle.clone().drop_guard();
    let acceptor = TlsAcceptor::from(Arc::new(tls));
    let permits = Arc::new(Semaphore::new(256));
    let mut tasks = JoinSet::new();
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            _=&mut shutdown=>break,
            Some(_)=tasks.join_next(),if !tasks.is_empty()=>{},
            accepted=listener.accept()=>{
                let (stream,_)=accepted?;
                let Ok(permit)=permits.clone().try_acquire_owned() else{drop(stream);continue;};
                let acceptor=acceptor.clone();let router=router.clone();
                let context=ConnectionContext {shutdown:lifecycle.clone(),_permit:Arc::new(permit)};
                tasks.spawn(async move {
                    let Ok(Ok(stream))=tokio::time::timeout(Duration::from_secs(10),acceptor.accept(stream)).await else{return;};
                    let user=match stream.get_ref().1.peer_certificates().and_then(|c|c.first()) {
                        Some(cert)=>match User::from_verified_certificate(cert.as_ref()){Ok(user)=>Some(user),Err(_)=>return},None=>None,
                    };
                    let service=TowerToHyperService::new(router.layer(Extension(Peer(user))).layer(Extension(context)));
                    let _=auto::Builder::new(TokioExecutor::new()).serve_connection_with_upgrades(TokioIo::new(stream),service).await;
                });
            }
        }
    }
    lifecycle.cancel();
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    Ok(())
}
