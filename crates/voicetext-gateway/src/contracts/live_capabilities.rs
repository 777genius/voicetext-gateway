//! Provider-neutral capability contract for one derived live profile.

use super::live::LiveIdentity;

/// Timestamp evidence exposed by live transcript events.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiveTimestampCapability {
    None,
    Segment,
}

/// Finalized transcript evidence exposed during a live session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiveFinalizedCapability {
    None,
    SegmentAndUtterance,
}

/// Language-hint syntax accepted by a live profile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiveLanguageHints {
    Unsupported,
    AsciiCode {
        maximum_bytes: usize,
        hyphen_at_edges: bool,
    },
}

/// Stable live input formats before provider-specific encoding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiveInputFormat {
    Opus48KhzMono,
    PcmS16Le16KhzMono,
    PcmS16Le48KhzMono,
}

/// Deterministic provider-neutral live lifecycle order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiveLifecycleEvent {
    Metadata,
    Error,
    Finalize,
    FinalizedTranscript,
    TerminalCompletion,
}

/// Exact public and provider bounds applied before live provider egress.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LiveProviderLimits {
    pub maximum_input_frame_bytes: usize,
    pub maximum_key_terms: usize,
    pub maximum_key_term_bytes: Option<usize>,
    pub maximum_key_term_characters: Option<usize>,
    pub maximum_key_term_total_characters: usize,
    pub normalize_key_term_whitespace: bool,
}

/// Narrow reusable descriptor for one live profile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LiveCapabilityDescriptor {
    pub timestamps: LiveTimestampCapability,
    pub finalized_events: LiveFinalizedCapability,
    pub language_hints: LiveLanguageHints,
    pub diarization: bool,
    pub key_terms: bool,
    pub input_formats: &'static [LiveInputFormat],
    pub provider_limits: LiveProviderLimits,
    pub lifecycle_order: &'static [LiveLifecycleEvent],
}

/// Features requested before opening a paid live provider session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LiveCapabilityRequest<'a> {
    pub timestamps: bool,
    pub finalized_events: bool,
    pub language_hint: Option<&'a str>,
    pub diarization: bool,
    pub key_terms: &'a [&'a str],
    pub input_format: LiveInputFormat,
    pub input_frame_bytes: usize,
}

/// Stable fail-closed reason for rejecting unsupported live features.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiveCapabilityError {
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

const INPUTS: &[LiveInputFormat] = &[
    LiveInputFormat::Opus48KhzMono,
    LiveInputFormat::PcmS16Le16KhzMono,
];
const ORDER: &[LiveLifecycleEvent] = &[
    LiveLifecycleEvent::Metadata,
    LiveLifecycleEvent::Error,
    LiveLifecycleEvent::Finalize,
    LiveLifecycleEvent::FinalizedTranscript,
    LiveLifecycleEvent::TerminalCompletion,
];

const DEEPGRAM: LiveCapabilityDescriptor = LiveCapabilityDescriptor {
    timestamps: LiveTimestampCapability::Segment,
    finalized_events: LiveFinalizedCapability::SegmentAndUtterance,
    language_hints: LiveLanguageHints::AsciiCode {
        maximum_bytes: 10,
        hyphen_at_edges: false,
    },
    diarization: false,
    key_terms: true,
    input_formats: INPUTS,
    provider_limits: LiveProviderLimits {
        maximum_input_frame_bytes: 64 * 1_024,
        maximum_key_terms: 100,
        maximum_key_term_bytes: Some(256),
        maximum_key_term_characters: None,
        maximum_key_term_total_characters: 8_192,
        normalize_key_term_whitespace: false,
    },
    lifecycle_order: ORDER,
};

const ELEVENLABS: LiveCapabilityDescriptor = LiveCapabilityDescriptor {
    timestamps: LiveTimestampCapability::None,
    language_hints: LiveLanguageHints::AsciiCode {
        maximum_bytes: 10,
        hyphen_at_edges: true,
    },
    provider_limits: LiveProviderLimits {
        maximum_key_terms: 50,
        maximum_key_term_bytes: None,
        maximum_key_term_characters: Some(20),
        normalize_key_term_whitespace: true,
        ..DEEPGRAM.provider_limits
    },
    ..DEEPGRAM
};

impl LiveIdentity {
    /// Returns the exact descriptor bound to this frozen live identity.
    pub const fn capability_descriptor(self) -> &'static LiveCapabilityDescriptor {
        match self {
            Self::DeepgramNova3 => &DEEPGRAM,
            Self::ElevenlabsScribeV2Realtime => &ELEVENLABS,
        }
    }
}

impl LiveCapabilityDescriptor {
    /// Rejects unsupported or out-of-bound features before a provider can be called.
    pub fn validate(&self, request: &LiveCapabilityRequest<'_>) -> Result<(), LiveCapabilityError> {
        if request.timestamps && self.timestamps == LiveTimestampCapability::None {
            return Err(LiveCapabilityError::UnsupportedTimestamps);
        }
        if request.finalized_events && self.finalized_events == LiveFinalizedCapability::None {
            return Err(LiveCapabilityError::UnsupportedFinalizedEvents);
        }
        if let Some(language) = request.language_hint {
            validate_language(language, self.language_hints)?;
        }
        if request.diarization && !self.diarization {
            return Err(LiveCapabilityError::UnsupportedDiarization);
        }
        if !request.key_terms.is_empty() && !self.key_terms {
            return Err(LiveCapabilityError::UnsupportedKeyTerms);
        }
        if !self.input_formats.contains(&request.input_format) {
            return Err(LiveCapabilityError::UnsupportedInputFormat);
        }
        if request.input_frame_bytes == 0
            || request.input_frame_bytes > self.provider_limits.maximum_input_frame_bytes
        {
            return Err(LiveCapabilityError::InputLimitExceeded);
        }
        validate_key_terms(request.key_terms, self.provider_limits)
    }
}

/// Validates the feature-bearing fields in a live client config before session opening.
pub fn validate_client_profile(
    identity: LiveIdentity,
    language: &str,
    key_terms: &[String],
    input_format: LiveInputFormat,
) -> Result<(), LiveCapabilityError> {
    let terms = key_terms.iter().map(String::as_str).collect::<Vec<_>>();
    identity
        .capability_descriptor()
        .validate(&LiveCapabilityRequest {
            timestamps: false,
            finalized_events: true,
            language_hint: Some(language),
            diarization: false,
            key_terms: &terms,
            input_format,
            input_frame_bytes: 1,
        })
}

fn validate_language(language: &str, policy: LiveLanguageHints) -> Result<(), LiveCapabilityError> {
    let LiveLanguageHints::AsciiCode {
        maximum_bytes,
        hyphen_at_edges,
    } = policy
    else {
        return Err(LiveCapabilityError::UnsupportedLanguageHint);
    };
    let valid = !language.is_empty()
        && language.len() <= maximum_bytes
        && language
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        && (hyphen_at_edges || (!language.starts_with('-') && !language.ends_with('-')));
    if valid {
        Ok(())
    } else {
        Err(LiveCapabilityError::UnsupportedLanguageHint)
    }
}

fn validate_key_terms(
    terms: &[&str],
    limits: LiveProviderLimits,
) -> Result<(), LiveCapabilityError> {
    if terms.len() > limits.maximum_key_terms {
        return Err(LiveCapabilityError::KeyTermLimitExceeded);
    }
    let mut total = 0_usize;
    for term in terms {
        let characters = term.chars().count();
        let normalized;
        let provider_characters = if limits.normalize_key_term_whitespace {
            normalized = term.split_whitespace().collect::<Vec<_>>().join(" ");
            normalized.chars().count()
        } else {
            characters
        };
        total = total
            .checked_add(characters)
            .ok_or(LiveCapabilityError::KeyTermLimitExceeded)?;
        let invalid = term.is_empty()
            || term.trim() != *term
            || term.chars().any(char::is_control)
            || limits
                .maximum_key_term_bytes
                .is_some_and(|maximum| term.len() > maximum)
            || limits
                .maximum_key_term_characters
                .is_some_and(|maximum| provider_characters > maximum);
        if invalid {
            return Err(LiveCapabilityError::InvalidKeyTerm);
        }
    }
    if total > limits.maximum_key_term_total_characters {
        Err(LiveCapabilityError::KeyTermLimitExceeded)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn supported<'a>(key_terms: &'a [&'a str]) -> LiveCapabilityRequest<'a> {
        LiveCapabilityRequest {
            timestamps: false,
            finalized_events: true,
            language_hint: Some("multi"),
            diarization: false,
            key_terms,
            input_format: LiveInputFormat::Opus48KhzMono,
            input_frame_bytes: 2,
        }
    }

    #[test]
    fn every_live_profile_has_exact_supported_capabilities() {
        let deepgram = LiveIdentity::DeepgramNova3.capability_descriptor();
        let elevenlabs = LiveIdentity::ElevenlabsScribeV2Realtime.capability_descriptor();
        for descriptor in [deepgram, elevenlabs] {
            assert_eq!(
                descriptor.finalized_events,
                LiveFinalizedCapability::SegmentAndUtterance
            );
            assert_eq!(descriptor.input_formats, INPUTS);
            assert!(!descriptor.diarization);
            assert!(descriptor.validate(&supported(&["VoiceText"])).is_ok());
            assert_eq!(descriptor.lifecycle_order, ORDER);
        }
        assert_eq!(deepgram.timestamps, LiveTimestampCapability::Segment);
        assert_eq!(elevenlabs.timestamps, LiveTimestampCapability::None);
    }

    #[test]
    fn unsupported_live_features_fail_closed() {
        let descriptor = LiveIdentity::ElevenlabsScribeV2Realtime.capability_descriptor();
        let mut request = supported(&[]);
        request.timestamps = true;
        assert_eq!(
            descriptor.validate(&request),
            Err(LiveCapabilityError::UnsupportedTimestamps)
        );
        request.timestamps = false;
        request.diarization = true;
        assert_eq!(
            descriptor.validate(&request),
            Err(LiveCapabilityError::UnsupportedDiarization)
        );
        request.diarization = false;
        request.input_format = LiveInputFormat::PcmS16Le48KhzMono;
        assert_eq!(
            descriptor.validate(&request),
            Err(LiveCapabilityError::UnsupportedInputFormat)
        );
    }

    #[test]
    fn live_descriptors_match_provider_language_and_key_term_limits() {
        let deepgram = LiveIdentity::DeepgramNova3.capability_descriptor();
        let mut request = supported(&["x"]);
        request.language_hint = Some("-en");
        assert_eq!(
            deepgram.validate(&request),
            Err(LiveCapabilityError::UnsupportedLanguageHint)
        );

        let elevenlabs = LiveIdentity::ElevenlabsScribeV2Realtime.capability_descriptor();
        let long = "x".repeat(21);
        let long_terms = [&*long];
        request.language_hint = Some("multi");
        request.key_terms = &long_terms;
        assert_eq!(
            elevenlabs.validate(&request),
            Err(LiveCapabilityError::InvalidKeyTerm)
        );
    }
}
