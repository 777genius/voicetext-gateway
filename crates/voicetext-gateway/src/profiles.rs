//! Explicit provider-profile bindings selected only by composition.

use std::fmt;
use std::sync::Arc;

use voicetext_speech::application::ports::{BatchRecognizer, LiveRecognizerFactory};

use crate::contracts::batch::BatchIdentity;
use crate::contracts::batch_capabilities::BatchCapabilityDescriptor;
use crate::contracts::live::LiveIdentity;
use crate::contracts::live_capabilities::LiveCapabilityDescriptor;

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
    #[must_use]
    pub fn with_batch(
        mut self,
        identity: BatchIdentity,
        recognizer: Arc<dyn BatchRecognizer>,
    ) -> Self {
        match identity {
            BatchIdentity::DeepgramNova3MultiV2 => self.deepgram_batch = Some(recognizer),
            BatchIdentity::ElevenlabsScribeV2MultiV3 => self.elevenlabs_batch = Some(recognizer),
        }
        self
    }

    /// Binds exactly one live profile.
    #[must_use]
    pub fn with_live(
        mut self,
        identity: LiveIdentity,
        recognizer: Arc<dyn LiveRecognizerFactory>,
    ) -> Self {
        match identity {
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
            .map(|_| identity.capability_descriptor())
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
            .map(|_| identity.capability_descriptor())
    }

    /// True when at least one batch and one live profile are available.
    #[must_use]
    pub fn is_operational(&self) -> bool {
        (self.deepgram_batch.is_some() || self.elevenlabs_batch.is_some())
            && (self.deepgram_live.is_some() || self.elevenlabs_live.is_some())
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
    use voicetext_speech::application::ports::{
        BatchRecognitionRequest, BatchRecognitionResult, BoxFuture, LiveRecognitionRequest,
        LiveRecognizerSession, RecognitionFailure,
    };

    use super::*;

    #[derive(Debug)]
    struct Never;

    impl BatchRecognizer for Never {
        fn recognize(
            &self,
            _request: BatchRecognitionRequest,
        ) -> BoxFuture<'_, Result<BatchRecognitionResult, RecognitionFailure>> {
            Box::pin(async { panic!("registry test never calls recognizers") })
        }
    }

    impl LiveRecognizerFactory for Never {
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
            .with_batch(BatchIdentity::DeepgramNova3MultiV2, Arc::new(Never))
            .with_live(LiveIdentity::ElevenlabsScribeV2Realtime, Arc::new(Never));

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
            BatchIdentity::DeepgramNova3MultiV2.capability_descriptor()
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
            LiveIdentity::ElevenlabsScribeV2Realtime.capability_descriptor()
        );
    }
}
