//! Provider-neutral capability contract for one authoritative batch profile.

use super::batch::BatchIdentity;

const MEBIBYTE: usize = 1_024 * 1_024;

/// Timestamp evidence exposed by an authoritative batch result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BatchTimestampCapability {
    None,
    Segment,
}

/// Finalized transcript evidence exposed by a batch result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BatchFinalizedCapability {
    None,
    TerminalTranscript,
}

/// Language-hint policy accepted by a batch profile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BatchLanguageHints {
    Unsupported,
    Fixed(&'static str),
}

/// Stable batch input formats. Provider wire media types do not enter this contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BatchInputFormat {
    OggOpus,
    WavePcm,
}

/// Deterministic provider-neutral batch lifecycle order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BatchLifecycleEvent {
    Metadata,
    Error,
    FinalizedTranscript,
    TerminalCompletion,
}

/// Exact public and provider bounds applied before paid batch egress.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BatchProviderLimits {
    pub maximum_input_bytes: usize,
    pub maximum_key_terms: usize,
    pub maximum_key_term_bytes: usize,
    pub maximum_key_term_characters: Option<usize>,
    pub maximum_key_term_words: Option<usize>,
    pub normalize_key_term_whitespace: bool,
    pub restricted_key_term_punctuation: bool,
}

/// Narrow reusable descriptor for one batch profile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BatchCapabilityDescriptor {
    pub timestamps: BatchTimestampCapability,
    pub finalized_events: BatchFinalizedCapability,
    pub language_hints: BatchLanguageHints,
    pub diarization: bool,
    pub key_terms: bool,
    pub input_formats: &'static [BatchInputFormat],
    pub provider_limits: BatchProviderLimits,
    pub lifecycle_order: &'static [BatchLifecycleEvent],
}

/// Features requested by a caller before a batch job is admitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BatchCapabilityRequest<'a> {
    pub timestamps: bool,
    pub finalized_events: bool,
    pub language_hint: Option<&'a str>,
    pub diarization: bool,
    pub key_terms: &'a [&'a str],
    pub input_format: BatchInputFormat,
    pub input_bytes: usize,
}

/// Stable fail-closed reason for rejecting unsupported batch features.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BatchCapabilityError {
    UnsupportedTimestamps,
    UnsupportedFinalizedEvents,
    UnsupportedLanguageHint,
    UnsupportedDiarization,
    UnsupportedKeyTerms,
    UnsupportedInputFormat,
    InputLimitExceeded,
    KeyTermLimitExceeded,
    InvalidKeyTerm,
}

const INPUTS: &[BatchInputFormat] = &[BatchInputFormat::OggOpus];
const ORDER: &[BatchLifecycleEvent] = &[
    BatchLifecycleEvent::Metadata,
    BatchLifecycleEvent::Error,
    BatchLifecycleEvent::FinalizedTranscript,
    BatchLifecycleEvent::TerminalCompletion,
];

const DEEPGRAM: BatchCapabilityDescriptor = BatchCapabilityDescriptor {
    timestamps: BatchTimestampCapability::Segment,
    finalized_events: BatchFinalizedCapability::TerminalTranscript,
    language_hints: BatchLanguageHints::Fixed("multi"),
    diarization: false,
    key_terms: true,
    input_formats: INPUTS,
    provider_limits: BatchProviderLimits {
        maximum_input_bytes: 64 * MEBIBYTE,
        maximum_key_terms: 100,
        maximum_key_term_bytes: 200,
        maximum_key_term_characters: None,
        maximum_key_term_words: None,
        normalize_key_term_whitespace: false,
        restricted_key_term_punctuation: false,
    },
    lifecycle_order: ORDER,
};

const ELEVENLABS: BatchCapabilityDescriptor = BatchCapabilityDescriptor {
    provider_limits: BatchProviderLimits {
        maximum_key_term_characters: Some(49),
        maximum_key_term_words: Some(5),
        normalize_key_term_whitespace: true,
        restricted_key_term_punctuation: true,
        ..DEEPGRAM.provider_limits
    },
    ..DEEPGRAM
};

impl BatchIdentity {
    /// Returns the exact descriptor bound to this frozen batch identity.
    pub const fn capability_descriptor(self) -> &'static BatchCapabilityDescriptor {
        match self {
            Self::DeepgramNova3MultiV2 => &DEEPGRAM,
            Self::ElevenlabsScribeV2MultiV3 => &ELEVENLABS,
        }
    }
}

impl BatchCapabilityDescriptor {
    /// Rejects unsupported or out-of-bound features before a provider can be called.
    pub fn validate(
        &self,
        request: &BatchCapabilityRequest<'_>,
    ) -> Result<(), BatchCapabilityError> {
        if request.timestamps && self.timestamps == BatchTimestampCapability::None {
            return Err(BatchCapabilityError::UnsupportedTimestamps);
        }
        if request.finalized_events && self.finalized_events == BatchFinalizedCapability::None {
            return Err(BatchCapabilityError::UnsupportedFinalizedEvents);
        }
        match (request.language_hint, self.language_hints) {
            (Some(_), BatchLanguageHints::Unsupported) => {
                return Err(BatchCapabilityError::UnsupportedLanguageHint);
            }
            (Some(requested), BatchLanguageHints::Fixed(supported)) if requested != supported => {
                return Err(BatchCapabilityError::UnsupportedLanguageHint);
            }
            _ => {}
        }
        if request.diarization && !self.diarization {
            return Err(BatchCapabilityError::UnsupportedDiarization);
        }
        if !request.key_terms.is_empty() && !self.key_terms {
            return Err(BatchCapabilityError::UnsupportedKeyTerms);
        }
        if !self.input_formats.contains(&request.input_format) {
            return Err(BatchCapabilityError::UnsupportedInputFormat);
        }
        if request.input_bytes == 0
            || request.input_bytes > self.provider_limits.maximum_input_bytes
        {
            return Err(BatchCapabilityError::InputLimitExceeded);
        }
        validate_key_terms(request.key_terms, self.provider_limits)
    }
}

fn validate_key_terms(
    terms: &[&str],
    limits: BatchProviderLimits,
) -> Result<(), BatchCapabilityError> {
    if terms.len() > limits.maximum_key_terms {
        return Err(BatchCapabilityError::KeyTermLimitExceeded);
    }
    for term in terms {
        let normalized;
        let provider_term = if limits.normalize_key_term_whitespace {
            normalized = term.split_whitespace().collect::<Vec<_>>().join(" ");
            normalized.as_str()
        } else {
            term
        };
        let invalid = term.is_empty()
            || term.trim() != *term
            || term.len() > limits.maximum_key_term_bytes
            || limits
                .maximum_key_term_characters
                .is_some_and(|maximum| provider_term.chars().count() > maximum)
            || limits
                .maximum_key_term_words
                .is_some_and(|maximum| provider_term.split_whitespace().count() > maximum)
            || term.chars().any(char::is_control)
            || (limits.restricted_key_term_punctuation
                && provider_term.chars().any(|character| {
                    matches!(character, '<' | '>' | '{' | '}' | '[' | ']' | '\\')
                }));
        if invalid {
            return Err(BatchCapabilityError::InvalidKeyTerm);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn supported<'a>(key_terms: &'a [&'a str]) -> BatchCapabilityRequest<'a> {
        BatchCapabilityRequest {
            timestamps: true,
            finalized_events: true,
            language_hint: Some("multi"),
            diarization: false,
            key_terms,
            input_format: BatchInputFormat::OggOpus,
            input_bytes: 27,
        }
    }

    #[test]
    fn every_batch_profile_has_exact_supported_capabilities() {
        for identity in [
            BatchIdentity::DeepgramNova3MultiV2,
            BatchIdentity::ElevenlabsScribeV2MultiV3,
        ] {
            let descriptor = identity.capability_descriptor();
            assert_eq!(descriptor.timestamps, BatchTimestampCapability::Segment);
            assert_eq!(
                descriptor.finalized_events,
                BatchFinalizedCapability::TerminalTranscript
            );
            assert_eq!(
                descriptor.language_hints,
                BatchLanguageHints::Fixed("multi")
            );
            assert_eq!(descriptor.input_formats, [BatchInputFormat::OggOpus]);
            assert!(!descriptor.diarization);
            assert!(descriptor.validate(&supported(&["VoiceText"])).is_ok());
            assert_eq!(descriptor.lifecycle_order, ORDER);
        }
    }

    #[test]
    fn unsupported_batch_features_fail_closed() {
        let descriptor = BatchIdentity::DeepgramNova3MultiV2.capability_descriptor();
        let mut request = supported(&[]);
        request.diarization = true;
        assert_eq!(
            descriptor.validate(&request),
            Err(BatchCapabilityError::UnsupportedDiarization)
        );
        request.diarization = false;
        request.input_format = BatchInputFormat::WavePcm;
        assert_eq!(
            descriptor.validate(&request),
            Err(BatchCapabilityError::UnsupportedInputFormat)
        );
        request.input_format = BatchInputFormat::OggOpus;
        request.language_hint = Some("en");
        assert_eq!(
            descriptor.validate(&request),
            Err(BatchCapabilityError::UnsupportedLanguageHint)
        );
    }

    #[test]
    fn elevenlabs_batch_descriptor_matches_stricter_key_term_behavior() {
        let deepgram = BatchIdentity::DeepgramNova3MultiV2.capability_descriptor();
        assert!(deepgram.validate(&supported(&["<supported>"])).is_ok());
        let descriptor = BatchIdentity::ElevenlabsScribeV2MultiV3.capability_descriptor();
        assert!(
            descriptor
                .validate(&supported(&["one  two three four five"]))
                .is_ok()
        );
        assert_eq!(
            descriptor.validate(&supported(&["one two three four five six"])),
            Err(BatchCapabilityError::InvalidKeyTerm)
        );
        assert_eq!(
            descriptor.validate(&supported(&["<unsupported>"])),
            Err(BatchCapabilityError::InvalidKeyTerm)
        );
    }
}
