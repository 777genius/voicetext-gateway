use voicetext_speech::application::batch_capabilities::{
    BatchCapabilityDescriptor, BatchCapabilityRequest, BatchFinalizedCapability, BatchInputFormat,
    BatchLanguageHints, BatchProviderLimits, BatchTimestampCapability, TimestampProvenance,
};
use voicetext_speech::application::ports::BatchRecognitionRequest;

use super::batch::{CONTRACT_VERSION, LANGUAGE, MODEL, PROVIDER};

const INPUT_FORMATS: &[BatchInputFormat] = &[BatchInputFormat::OggOpus];
pub(super) const CAPABILITIES: BatchCapabilityDescriptor = BatchCapabilityDescriptor {
    contract_version: CONTRACT_VERSION,
    provider: PROVIDER,
    model: MODEL,
    language: LANGUAGE,
    timestamps: BatchTimestampCapability::Segment,
    timestamp_provenance: TimestampProvenance::ProviderNative,
    finalized_events: BatchFinalizedCapability::TerminalTranscript,
    language_hints: BatchLanguageHints::Fixed(LANGUAGE),
    diarization: false,
    key_terms: true,
    input_formats: INPUT_FORMATS,
    provider_limits: BatchProviderLimits {
        maximum_public_input_bytes: 64 * 1_024 * 1_024,
        maximum_input_bytes: 64 * 1_024 * 1_024,
        maximum_key_terms: 100,
        maximum_key_term_bytes: 200,
        maximum_key_term_characters: None,
        key_term_character_unit: None,
        maximum_key_term_words: None,
        normalize_key_term_whitespace: false,
        restricted_key_term_punctuation: false,
    },
};

pub(super) fn matches(request: &BatchRecognitionRequest) -> bool {
    let terms = request
        .keyterms
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    CAPABILITIES
        .validate(&BatchCapabilityRequest {
            timestamps: true,
            finalized_events: true,
            language_hint: Some(request.profile.language()),
            diarization: false,
            key_terms: &terms,
            input_format: BatchInputFormat::OggOpus,
            input_bytes: request.audio.len(),
        })
        .is_ok()
}
