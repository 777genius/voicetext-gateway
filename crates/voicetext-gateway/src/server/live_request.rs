//! Provider-neutral live request projection kept outside the wire contract.

use crate::contracts::live::{AudioFormat, ClientConfig, LiveIdentity};
use voicetext_speech::application::ports::{LiveProfile, LiveRecognitionRequest};

pub(super) fn recognition_request(config: &ClientConfig) -> LiveRecognitionRequest {
    let (provider, model) = match config.identity {
        LiveIdentity::DeepgramNova3 => ("deepgram", "nova-3"),
        LiveIdentity::ElevenlabsScribeV2Realtime => ("elevenlabs", "scribe_v2_realtime"),
    };
    LiveRecognitionRequest {
        profile: LiveProfile {
            protocol_version: 2,
            provider: provider.into(),
            model: model.into(),
            language: config.language.clone(),
        },
        sample_rate_hz: match config.audio_format {
            AudioFormat::Opus48Khz => 48_000,
            AudioFormat::PcmS16le16Khz => 16_000,
        },
        channels: 1,
        keyterms: config.keyterms.clone(),
    }
}
