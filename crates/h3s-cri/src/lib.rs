//! Native CRI v1 client. The socket is selected by local operator configuration;
//! no network endpoint, Docker API, ambient proxy or protocol fallback is used.
use hyper_util::rt::TokioIo;
use std::{
    path::{Component, Path, PathBuf},
    time::Duration,
};
use tonic::{
    service::interceptor::InterceptedService,
    transport::{Channel, Endpoint},
    Request, Status,
};

pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");
#[allow(clippy::doc_lazy_continuation)]
pub mod v1 {
    tonic::include_proto!("runtime.v1");
}
const MAX_MESSAGE: usize = 16 * 1024 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const RUNTIME_TIMEOUT: Duration = Duration::from_secs(30);
const IMAGE_TIMEOUT: Duration = Duration::from_secs(180);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("CRI endpoint must be an absolute Unix socket path without parent traversal")]
    Endpoint,
    #[error("CRI Unix socket connection failed")]
    Connect,
    #[error("CRI request failed with code {0}")]
    Rpc(tonic::Code),
    #[error("runtime does not implement CRI v1")]
    Unsupported,
}
impl From<Status> for Error {
    fn from(value: Status) -> Self {
        Self::Rpc(value.code())
    }
}
#[derive(Clone)]
pub struct Deadline(Duration);
impl tonic::service::Interceptor for Deadline {
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        request.set_timeout(self.0);
        Ok(request)
    }
}
pub type RuntimeClient =
    v1::runtime_service_client::RuntimeServiceClient<InterceptedService<Channel, Deadline>>;
pub type ImageClient =
    v1::image_service_client::ImageServiceClient<InterceptedService<Channel, Deadline>>;

/// Cloneable channels reconnect to the same explicitly configured local socket.
/// Runtime RPCs have a 30-second cap; image RPCs have a 180-second cap. Limits
/// apply locally and are sent as gRPC deadlines. Bodies are bounded to 16 MiB.
#[derive(Clone)]
pub struct Cri {
    runtime: RuntimeClient,
    images: ImageClient,
    version: v1::VersionResponse,
}
fn socket_path(endpoint: &str) -> Result<PathBuf, Error> {
    let path = Path::new(endpoint.strip_prefix("unix://").unwrap_or(endpoint));
    if !path.is_absolute()
        || path.components().any(|part| part == Component::ParentDir)
        || path.as_os_str().as_encoded_bytes().contains(&0)
    {
        return Err(Error::Endpoint);
    }
    Ok(path.to_owned())
}
async fn channel(path: PathBuf, timeout: Duration) -> Result<Channel, Error> {
    // The HTTP authority is internal to gRPC. This connector always opens the
    // chosen Unix socket; it never resolves or connects to this hostname.
    let endpoint = Endpoint::from_static("http://h3s-cri.invalid")
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(timeout)
        .concurrency_limit(16)
        .buffer_size(32);
    let connect = endpoint.connect_with_connector(tower::service_fn(move |_| {
        let path = path.clone();
        async move {
            tokio::net::UnixStream::connect(path)
                .await
                .map(TokioIo::new)
        }
    }));
    tokio::time::timeout(CONNECT_TIMEOUT, connect)
        .await
        .map_err(|_| Error::Connect)?
        .map_err(|_| Error::Connect)
}
impl Cri {
    pub async fn connect(endpoint: &str) -> Result<Self, Error> {
        Self::with_timeouts(endpoint, RUNTIME_TIMEOUT, IMAGE_TIMEOUT).await
    }
    async fn with_timeouts(
        endpoint: &str,
        runtime_timeout: Duration,
        image_timeout: Duration,
    ) -> Result<Self, Error> {
        let path = socket_path(endpoint)?;
        let mut runtime = v1::runtime_service_client::RuntimeServiceClient::with_interceptor(
            channel(path.clone(), runtime_timeout).await?,
            Deadline(runtime_timeout),
        )
        .max_decoding_message_size(MAX_MESSAGE)
        .max_encoding_message_size(MAX_MESSAGE);
        let version = runtime
            .version(v1::VersionRequest {
                version: "0.1.0".into(),
            })
            .await?
            .into_inner();
        if version.runtime_api_version != "v1" {
            return Err(Error::Unsupported);
        }
        let images = v1::image_service_client::ImageServiceClient::with_interceptor(
            channel(path, image_timeout).await?,
            Deadline(image_timeout),
        )
        .max_decoding_message_size(MAX_MESSAGE)
        .max_encoding_message_size(MAX_MESSAGE);
        Ok(Self {
            runtime,
            images,
            version,
        })
    }
    pub fn version(&self) -> &v1::VersionResponse {
        &self.version
    }
    pub fn runtime(&self) -> RuntimeClient {
        self.runtime.clone()
    }
    pub fn images(&self) -> ImageClient {
        self.images.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio_stream::wrappers::UnixListenerStream;
    use tonic::{Code, Response};
    struct ServerTask(tokio::task::JoinHandle<()>);
    impl Drop for ServerTask {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    struct Fixture {
        api: &'static str,
        delay: Duration,
        observed: Arc<std::sync::atomic::AtomicBool>,
    }
    #[tonic::async_trait]
    impl v1::runtime_service_server::RuntimeService for Fixture {
        async fn version(
            &self,
            request: Request<v1::VersionRequest>,
        ) -> Result<Response<v1::VersionResponse>, Status> {
            assert_eq!(request.get_ref().version, "0.1.0");
            assert!(request.metadata().contains_key("grpc-timeout"));
            self.observed
                .store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(Response::new(v1::VersionResponse {
                version: "0.1.0".into(),
                runtime_name: "protocol-fixture".into(),
                runtime_version: "1.0.0".into(),
                runtime_api_version: self.api.into(),
            }))
        }
        async fn status(
            &self,
            request: Request<v1::StatusRequest>,
        ) -> Result<Response<v1::StatusResponse>, Status> {
            assert!(request.metadata().contains_key("grpc-timeout"));
            tokio::time::sleep(self.delay).await;
            Ok(Response::new(v1::StatusResponse {
                status: Some(v1::RuntimeStatus {
                    conditions: vec![v1::RuntimeCondition {
                        r#type: "RuntimeReady".into(),
                        status: true,
                        ..Default::default()
                    }],
                }),
                ..Default::default()
            }))
        }
    }
    async fn fixture(
        root: &Path,
        api: &'static str,
        delay: Duration,
    ) -> (String, ServerTask, Arc<std::sync::atomic::AtomicBool>) {
        let path = root.join("cri.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let observed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let service = Fixture {
            api,
            delay,
            observed: observed.clone(),
        };
        let task = tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(v1::runtime_service_server::RuntimeServiceServer::new(
                    service,
                ))
                .serve_with_incoming(UnixListenerStream::new(listener))
                .await
                .unwrap();
        });
        (
            format!("unix://{}", path.display()),
            ServerTask(task),
            observed,
        )
    }
    #[tokio::test]
    async fn actual_unix_grpc_negotiates_v1_and_sends_deadlines() {
        let dir = tempfile::tempdir().unwrap();
        let (endpoint, _task, observed) = fixture(dir.path(), "v1", Duration::ZERO).await;
        let cri = Cri::connect(&endpoint).await.unwrap();
        assert_eq!(cri.version().runtime_name, "protocol-fixture");
        assert!(observed.load(std::sync::atomic::Ordering::SeqCst));
        let status = cri
            .runtime()
            .status(v1::StatusRequest { verbose: false })
            .await
            .unwrap()
            .into_inner();
        assert!(status.status.unwrap().conditions[0].status);
        assert_eq!(
            cri.runtime()
                .list_containers(v1::ListContainersRequest::default())
                .await
                .unwrap_err()
                .code(),
            Code::Unimplemented
        );
    }
    #[tokio::test]
    async fn unsupported_protocol_missing_socket_and_network_endpoints_fail() {
        for endpoint in [
            "http://127.0.0.1:2375",
            "tcp://127.0.0.1:1234",
            "unix://host/path",
            "relative.sock",
            "/tmp/../runtime.sock",
        ] {
            assert!(matches!(Cri::connect(endpoint).await, Err(Error::Endpoint)));
        }
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            Cri::connect(dir.path().join("missing.sock").to_str().unwrap()).await,
            Err(Error::Connect)
        ));
        let (endpoint, _task, _) = fixture(dir.path(), "v1alpha2", Duration::ZERO).await;
        assert!(matches!(
            Cri::connect(&endpoint).await,
            Err(Error::Unsupported)
        ));
    }
    #[tokio::test]
    async fn stalled_runtime_rpc_has_a_local_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let (endpoint, _task, _) = fixture(dir.path(), "v1", Duration::from_secs(5)).await;
        let cri = Cri::with_timeouts(
            &endpoint,
            Duration::from_millis(100),
            Duration::from_secs(1),
        )
        .await
        .unwrap();
        let response = tokio::time::timeout(
            Duration::from_secs(2),
            cri.runtime().status(v1::StatusRequest { verbose: false }),
        )
        .await
        .unwrap();
        assert!(matches!(
            response.unwrap_err().code(),
            Code::Cancelled | Code::DeadlineExceeded
        ));
    }
    #[test]
    fn runtime_errors_do_not_echo_untrusted_rpc_details() {
        let error = Error::from(Status::internal("credential-sensitive-fixture"));
        assert!(!format!("{error:?} {error}").contains("credential-sensitive-fixture"));
    }
}
