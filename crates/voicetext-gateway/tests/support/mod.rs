mod batch;
mod fixture;
mod live;

use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::sync::oneshot;
use voicetext_gateway::contracts::batch::BatchIdentity;
use voicetext_gateway::contracts::live::LiveIdentity;
use voicetext_gateway::profiles::ProfileRegistry;
use voicetext_gateway::secret::MachineSecret;
use voicetext_gateway::server::{
    GatewayLimits, GatewayReadiness, GatewayState, ReadinessFailure, recover_startup, router,
};
use voicetext_speech::application::ports::{BatchAudioSpool, BatchJobStore, BoxFuture};

use batch::{FakeBatchInfrastructure, FakeBatchRecognizer};
pub use fixture::synthetic_ogg_opus;
use live::FakeLiveFactory;

pub const TOKEN: &str = "conformance-token";

#[derive(Debug)]
struct AlwaysReady;

impl GatewayReadiness for AlwaysReady {
    fn check(&self) -> BoxFuture<'_, Result<(), ReadinessFailure>> {
        Box::pin(async { Ok(()) })
    }
}

#[derive(Debug)]
pub struct TestGateway {
    address: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

impl TestGateway {
    pub async fn start() -> Self {
        let batch = Arc::new(FakeBatchInfrastructure::default());
        let jobs: Arc<dyn BatchJobStore> = batch.clone();
        let spool: Arc<dyn BatchAudioSpool> = batch;
        let batch_recognizer = Arc::new(FakeBatchRecognizer);
        let live_factory = Arc::new(FakeLiveFactory);
        let profiles = ProfileRegistry::new()
            .with_batch(
                BatchIdentity::DeepgramNova3MultiV2,
                batch_recognizer.clone(),
            )
            .with_batch(BatchIdentity::ElevenlabsScribeV2MultiV3, batch_recognizer)
            .with_live(LiveIdentity::DeepgramNova3, live_factory.clone())
            .with_live(LiveIdentity::ElevenlabsScribeV2Realtime, live_factory);
        let limits = GatewayLimits::new(
            1024 * 1024,
            64 * 1024,
            NonZeroUsize::new(4).unwrap(),
            Duration::from_secs(1),
            Duration::from_secs(1),
        )
        .unwrap();
        let state = GatewayState::new(
            MachineSecret::from_token(TOKEN.as_bytes()).unwrap(),
            jobs,
            spool,
            profiles,
            Arc::new(AlwaysReady),
            limits,
        );
        recover_startup(&state).await.unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (shutdown, receiver) = oneshot::channel();
        let task = tokio::spawn(async move {
            axum::serve(listener, router(state))
                .with_graceful_shutdown(async move {
                    let _received = receiver.await;
                })
                .await
                .unwrap();
        });
        Self {
            address,
            shutdown: Some(shutdown),
            task,
        }
    }

    pub fn http_url(&self, path: &str) -> String {
        format!("http://{}{path}", self.address)
    }

    pub fn http_origin(&self) -> String {
        format!("http://{}", self.address)
    }

    pub fn websocket_origin(&self) -> String {
        format!("ws://{}", self.address)
    }

    pub fn websocket_url(&self) -> String {
        format!("ws://{}/api/v1/transcribe/stream", self.address)
    }

    pub async fn stop(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _sent = shutdown.send(());
        }
        self.task.await.unwrap();
    }
}
