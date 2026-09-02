pub(crate) mod batch;
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
    GatewayLimits, GatewayReadiness, GatewayState, ReadinessFailure, reconcile_startup, router,
    start_startup_recovery,
};
use voicetext_speech::application::batch_capabilities::{
    BatchCapabilityDescriptor, BatchFinalizedCapability, BatchInputFormat, BatchLanguageHints,
    BatchProviderLimits, BatchTimestampCapability, TimestampProvenance,
};
use voicetext_speech::application::live_capabilities::{
    LiveCapabilityDescriptor, LiveFinalizedCapability, LiveInputFormat, LiveLanguageHints,
    LiveProviderLimits, LiveTimestampCapability,
};
use voicetext_speech::application::ports::{
    BatchAudioSpool, BatchJobStore, BoxFuture, LiveRecognizerFactory,
};

use batch::{FakeBatchInfrastructure, FakeBatchRecognizer};
pub use fixture::synthetic_ogg_opus;
use live::FakeLiveFactory;

pub const TOKEN: &str = "conformance-service-token-00000001";
const BATCH_INPUTS: &[BatchInputFormat] = &[BatchInputFormat::OggOpus];
const LIVE_INPUTS: &[LiveInputFormat] = &[
    LiveInputFormat::Opus48KhzMono,
    LiveInputFormat::PcmS16Le16KhzMono,
];

pub(crate) fn batch_capabilities(identity: BatchIdentity) -> &'static BatchCapabilityDescriptor {
    const DEEPGRAM: BatchCapabilityDescriptor = test_batch(2, "deepgram", "nova-3");
    const ELEVENLABS: BatchCapabilityDescriptor = test_batch(3, "elevenlabs", "scribe_v2");
    match identity {
        BatchIdentity::DeepgramNova3MultiV2 => &DEEPGRAM,
        BatchIdentity::ElevenlabsScribeV2MultiV3 => &ELEVENLABS,
    }
}

const fn test_batch(
    contract_version: u16,
    provider: &'static str,
    model: &'static str,
) -> BatchCapabilityDescriptor {
    BatchCapabilityDescriptor {
        contract_version,
        provider,
        model,
        language: "multi",
        timestamps: BatchTimestampCapability::Segment,
        timestamp_provenance: TimestampProvenance::ProviderNative,
        finalized_events: BatchFinalizedCapability::TerminalTranscript,
        language_hints: BatchLanguageHints::Fixed("multi"),
        diarization: false,
        key_terms: true,
        input_formats: BATCH_INPUTS,
        provider_limits: BatchProviderLimits {
            maximum_public_input_bytes: 1024 * 1024,
            maximum_input_bytes: 1024 * 1024,
            maximum_key_terms: 100,
            maximum_key_term_bytes: 256,
            maximum_key_term_characters: None,
            key_term_character_unit: None,
            maximum_key_term_words: None,
            normalize_key_term_whitespace: false,
            restricted_key_term_punctuation: false,
        },
    }
}

pub(crate) fn live_capabilities(identity: LiveIdentity) -> &'static LiveCapabilityDescriptor {
    const DEEPGRAM: LiveCapabilityDescriptor = test_live("deepgram", "nova-3");
    const ELEVENLABS: LiveCapabilityDescriptor = test_live("elevenlabs", "scribe_v2_realtime");
    match identity {
        LiveIdentity::DeepgramNova3 => &DEEPGRAM,
        LiveIdentity::ElevenlabsScribeV2Realtime => &ELEVENLABS,
    }
}

const fn test_live(provider: &'static str, model: &'static str) -> LiveCapabilityDescriptor {
    LiveCapabilityDescriptor {
        protocol_version: 2,
        provider,
        model,
        timestamps: LiveTimestampCapability::Segment,
        timestamp_provenance: TimestampProvenance::ProviderNative,
        finalized_events: LiveFinalizedCapability::SegmentAndUtterance,
        language_hints: LiveLanguageHints::AsciiCode {
            maximum_bytes: 10,
            hyphen_at_edges: true,
        },
        diarization: false,
        key_terms: true,
        input_formats: LIVE_INPUTS,
        provider_limits: LiveProviderLimits {
            maximum_public_input_frame_bytes: 64 * 1024,
            maximum_input_frame_bytes: 64 * 1024,
            maximum_key_terms: 100,
            maximum_key_term_bytes: Some(256),
            maximum_public_key_term_utf16_units: 256,
            maximum_key_term_characters: None,
            key_term_character_unit: None,
            maximum_public_key_term_total_utf16_units: 8192,
            normalize_key_term_whitespace: false,
        },
    }
}

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
        Self::start_with_live(Arc::new(FakeLiveFactory(LiveIdentity::DeepgramNova3))).await
    }

    pub async fn start_with_live(live_factory: Arc<dyn LiveRecognizerFactory>) -> Self {
        let batch = Arc::new(FakeBatchInfrastructure::default());
        let jobs: Arc<dyn BatchJobStore> = batch.clone();
        let spool: Arc<dyn BatchAudioSpool> = batch;
        let deepgram_batch = Arc::new(FakeBatchRecognizer(BatchIdentity::DeepgramNova3MultiV2));
        let elevenlabs_batch = Arc::new(FakeBatchRecognizer(
            BatchIdentity::ElevenlabsScribeV2MultiV3,
        ));
        let other_live = match live_factory.capabilities().provider {
            "deepgram" => LiveIdentity::ElevenlabsScribeV2Realtime,
            "elevenlabs" => LiveIdentity::DeepgramNova3,
            provider => panic!("unsupported test live provider {provider}"),
        };
        let profiles = ProfileRegistry::new()
            .with_batch(deepgram_batch)
            .with_batch(elevenlabs_batch)
            .with_live(live_factory)
            .with_live(Arc::new(FakeLiveFactory(other_live)));
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
        let recovery = reconcile_startup(&state).await.unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (shutdown, receiver) = oneshot::channel();
        start_startup_recovery(&state, recovery);
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
