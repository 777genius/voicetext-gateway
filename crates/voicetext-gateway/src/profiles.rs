//! Explicit provider-profile bindings selected only by composition.

use std::fmt;
use std::sync::Arc;

use voicetext_speech::application::ports::{BatchRecognizer, LiveRecognizerFactory};

use crate::contracts::batch::BatchIdentity;
use crate::contracts::live::LiveIdentity;
use voicetext_speech::application::batch_capabilities::BatchCapabilityDescriptor;
use voicetext_speech::application::live_capabilities::LiveCapabilityDescriptor;

/// Configured recognizers for the four `VoiceText` compatibility profiles.
///
/// A request can select only an explicitly populated slot. No fallback or
/// provider substitution is performed.
#[derive(Default)]
pub struct ProfileRegistry {
    deepgram_batch: Option<Arc<dyn BatchRecognizer>>,
    elevenlabs_batch: Option<Arc<dyn BatchRecognizer>>,
    deepgram_live: Option<Arc<dyn LiveRecognizerFactory>>,
    elevenlabs_live: Option<Arc<dyn LiveRecognizerFactory>>,
}

impl ProfileRegistry {
    /// Creates an empty registry. Composition must opt each profile in.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            deepgram_batch: None,
            elevenlabs_batch: None,
            deepgram_live: None,
            elevenlabs_live: None,
        }
    }

    /// Binds exactly one batch profile.
    ///
    /// # Panics
    ///
    /// Panics when an injected adapter advertises an identity outside the closed compatibility
    /// matrix. Composition treats that as a programmer error and fails before serving traffic.
    #[must_use]
    pub fn with_batch(mut self, recognizer: Arc<dyn BatchRecognizer>) -> Self {
        match batch_identity(recognizer.capabilities())
            .expect("batch adapter advertises an unsupported capability identity")
        {
            BatchIdentity::DeepgramNova3MultiV2 => self.deepgram_batch = Some(recognizer),
            BatchIdentity::ElevenlabsScribeV2MultiV3 => self.elevenlabs_batch = Some(recognizer),
        }
        self
    }

    /// Binds exactly one live profile.
    ///
    /// # Panics
    ///
    /// Panics when an injected adapter advertises an identity outside the closed compatibility
    /// matrix. Composition treats that as a programmer error and fails before serving traffic.
    #[must_use]
    pub fn with_live(mut self, recognizer: Arc<dyn LiveRecognizerFactory>) -> Self {
        match live_identity(recognizer.capabilities())
            .expect("live adapter advertises an unsupported capability identity")
        {
            LiveIdentity::DeepgramNova3 => self.deepgram_live = Some(recognizer),
            LiveIdentity::ElevenlabsScribeV2Realtime => self.elevenlabs_live = Some(recognizer),
        }
        self
    }

    /// Returns the exact configured batch profile, without fallback.
    #[must_use]
    pub fn batch(&self, identity: BatchIdentity) -> Option<&Arc<dyn BatchRecognizer>> {
        match identity {
            BatchIdentity::DeepgramNova3MultiV2 => self.deepgram_batch.as_ref(),
            BatchIdentity::ElevenlabsScribeV2MultiV3 => self.elevenlabs_batch.as_ref(),
        }
    }

    /// Returns the descriptor bound to an enabled batch profile.
    #[must_use]
    pub fn batch_descriptor(
        &self,
        identity: BatchIdentity,
    ) -> Option<&'static BatchCapabilityDescriptor> {
        self.batch(identity)
            .map(|recognizer| recognizer.capabilities())
    }

    /// Returns the exact configured live profile, without fallback.
    #[must_use]
    pub fn live(&self, identity: LiveIdentity) -> Option<&Arc<dyn LiveRecognizerFactory>> {
        match identity {
            LiveIdentity::DeepgramNova3 => self.deepgram_live.as_ref(),
            LiveIdentity::ElevenlabsScribeV2Realtime => self.elevenlabs_live.as_ref(),
        }
    }

    /// Returns the descriptor bound to an enabled live profile.
    #[must_use]
    pub fn live_descriptor(
        &self,
        identity: LiveIdentity,
    ) -> Option<&'static LiveCapabilityDescriptor> {
        self.live(identity)
            .map(|recognizer| recognizer.capabilities())
    }

    /// True when at least one batch and one live profile are available.
    #[must_use]
    pub fn is_operational(&self) -> bool {
        (self.deepgram_batch.is_some() || self.elevenlabs_batch.is_some())
            && (self.deepgram_live.is_some() || self.elevenlabs_live.is_some())
    }
}

fn batch_identity(descriptor: &BatchCapabilityDescriptor) -> Option<BatchIdentity> {
    [
        BatchIdentity::DeepgramNova3MultiV2,
        BatchIdentity::ElevenlabsScribeV2MultiV3,
    ]
    .into_iter()
    .find(|identity| {
        descriptor.contract_version == u16::from(identity.contract_version())
            && descriptor.provider == identity.provider()
            && descriptor.model == identity.model()
            && descriptor.language == identity.language()
    })
}

fn live_identity(descriptor: &LiveCapabilityDescriptor) -> Option<LiveIdentity> {
    match (
        descriptor.protocol_version,
        descriptor.provider,
        descriptor.model,
    ) {
        (2, "deepgram", "nova-3") => Some(LiveIdentity::DeepgramNova3),
        (2, "elevenlabs", "scribe_v2_realtime") => Some(LiveIdentity::ElevenlabsScribeV2Realtime),
        _ => None,
    }
}

impl fmt::Debug for ProfileRegistry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProfileRegistry")
            .field("deepgram_batch", &self.deepgram_batch.is_some())
            .field("elevenlabs_batch", &self.elevenlabs_batch.is_some())
            .field("deepgram_live", &self.deepgram_live.is_some())
            .field("elevenlabs_live", &self.elevenlabs_live.is_some())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use voicetext_speech::application::batch_capabilities::{
        BatchFinalizedCapability, BatchInputFormat, BatchLanguageHints, BatchProviderLimits,
        BatchTimestampCapability, TimestampProvenance,
    };
    use voicetext_speech::application::live_capabilities::{
        LiveFinalizedCapability, LiveInputFormat, LiveLanguageHints, LiveProviderLimits,
        LiveTimestampCapability,
    };
    use voicetext_speech::application::ports::{
        BatchRecognitionRequest, BatchRecognitionResult, BoxFuture, LiveRecognitionRequest,
        LiveRecognizerSession, RecognitionFailure,
    };

    use super::*;

    const BATCH_INPUTS: &[BatchInputFormat] = &[BatchInputFormat::OggOpus];
    const BATCH: BatchCapabilityDescriptor = BatchCapabilityDescriptor {
        contract_version: 2,
        provider: "deepgram",
        model: "nova-3",
        language: "multi",
        timestamps: BatchTimestampCapability::Segment,
        timestamp_provenance: TimestampProvenance::ProviderNative,
        finalized_events: BatchFinalizedCapability::TerminalTranscript,
        language_hints: BatchLanguageHints::Fixed("multi"),
        diarization: false,
        key_terms: true,
        input_formats: BATCH_INPUTS,
        provider_limits: BatchProviderLimits {
            maximum_public_input_bytes: 1,
            maximum_input_bytes: 1,
            maximum_key_terms: 0,
            maximum_key_term_bytes: 0,
            maximum_key_term_characters: None,
            key_term_character_unit: None,
            maximum_key_term_words: None,
            normalize_key_term_whitespace: false,
            restricted_key_term_punctuation: false,
        },
    };
    const LIVE_INPUTS: &[LiveInputFormat] = &[LiveInputFormat::Opus48KhzMono];
    const LIVE: LiveCapabilityDescriptor = LiveCapabilityDescriptor {
        protocol_version: 2,
        provider: "elevenlabs",
        model: "scribe_v2_realtime",
        timestamps: LiveTimestampCapability::Segment,
        timestamp_provenance: TimestampProvenance::GatewaySynthesizedFromAcceptedAudio,
        finalized_events: LiveFinalizedCapability::SegmentAndUtterance,
        language_hints: LiveLanguageHints::AsciiCode {
            maximum_bytes: 10,
            hyphen_at_edges: true,
        },
        diarization: false,
        key_terms: true,
        input_formats: LIVE_INPUTS,
        provider_limits: LiveProviderLimits {
            maximum_public_input_frame_bytes: 1,
            maximum_input_frame_bytes: 1,
            maximum_key_terms: 0,
            maximum_key_term_bytes: None,
            maximum_public_key_term_utf16_units: 1,
            maximum_key_term_characters: None,
            key_term_character_unit: None,
            maximum_public_key_term_total_utf16_units: 0,
            normalize_key_term_whitespace: false,
        },
    };

    #[derive(Debug)]
    struct Never;

    impl BatchRecognizer for Never {
        fn capabilities(&self) -> &'static BatchCapabilityDescriptor {
            &BATCH
        }

        fn recognize(
            &self,
            _request: BatchRecognitionRequest,
        ) -> BoxFuture<'_, Result<BatchRecognitionResult, RecognitionFailure>> {
            Box::pin(async { panic!("registry test never calls recognizers") })
        }
    }

    impl LiveRecognizerFactory for Never {
        fn capabilities(&self) -> &'static LiveCapabilityDescriptor {
            &LIVE
        }

        fn open(
            &self,
            _request: LiveRecognitionRequest,
        ) -> BoxFuture<'_, Result<Box<dyn LiveRecognizerSession>, RecognitionFailure>> {
            Box::pin(async { panic!("registry test never opens sessions") })
        }
    }

    #[test]
    fn exact_slots_never_fall_back() {
        let registry = ProfileRegistry::new()
            .with_batch(Arc::new(Never))
            .with_live(Arc::new(Never));

        assert!(
            registry
                .batch(BatchIdentity::DeepgramNova3MultiV2)
                .is_some()
        );
        assert!(
            registry
                .batch(BatchIdentity::ElevenlabsScribeV2MultiV3)
                .is_none()
        );
        assert!(registry.live(LiveIdentity::DeepgramNova3).is_none());
        assert!(
            registry
                .live(LiveIdentity::ElevenlabsScribeV2Realtime)
                .is_some()
        );
        assert!(registry.is_operational());
        assert_eq!(
            registry
                .batch_descriptor(BatchIdentity::DeepgramNova3MultiV2)
                .unwrap(),
            &BATCH
        );
        assert!(
            registry
                .batch_descriptor(BatchIdentity::ElevenlabsScribeV2MultiV3)
                .is_none()
        );
        assert_eq!(
            registry
                .live_descriptor(LiveIdentity::ElevenlabsScribeV2Realtime)
                .unwrap(),
            &LIVE
        );
    }
}
