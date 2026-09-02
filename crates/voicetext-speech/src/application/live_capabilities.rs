//! Provider-neutral capability contract for one derived live profile.

use super::batch_capabilities::{TextLengthUnit, TimestampProvenance};

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

/// Exact public and provider bounds applied before live provider egress.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LiveProviderLimits {
    pub maximum_public_input_frame_bytes: usize,
    pub maximum_input_frame_bytes: usize,
    pub maximum_key_terms: usize,
    pub maximum_key_term_bytes: Option<usize>,
    pub maximum_public_key_term_utf16_units: usize,
    pub maximum_key_term_characters: Option<usize>,
    pub key_term_character_unit: Option<TextLengthUnit>,
    pub maximum_public_key_term_total_utf16_units: usize,
    pub normalize_key_term_whitespace: bool,
}

/// Narrow reusable descriptor for one live profile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LiveCapabilityDescriptor {
    pub protocol_version: u16,
    pub provider: &'static str,
    pub model: &'static str,
    pub timestamps: LiveTimestampCapability,
    pub timestamp_provenance: TimestampProvenance,
    pub finalized_events: LiveFinalizedCapability,
    pub language_hints: LiveLanguageHints,
    pub diarization: bool,
    pub key_terms: bool,
    pub input_formats: &'static [LiveInputFormat],
    pub provider_limits: LiveProviderLimits,
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
            || request.input_frame_bytes > self.provider_limits.maximum_public_input_frame_bytes
            || request.input_frame_bytes > self.provider_limits.maximum_input_frame_bytes
        {
            return Err(LiveCapabilityError::InputLimitExceeded);
        }
        validate_key_terms(request.key_terms, self.provider_limits)
    }
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
        let characters = term.encode_utf16().count();
        let normalized;
        let provider_characters = if limits.normalize_key_term_whitespace {
            normalized = term.split_whitespace().collect::<Vec<_>>().join(" ");
            text_length(&normalized, limits.key_term_character_unit)
        } else {
            text_length(term, limits.key_term_character_unit)
        };
        total = total
            .checked_add(characters)
            .ok_or(LiveCapabilityError::KeyTermLimitExceeded)?;
        let invalid = term.is_empty()
            || term.trim() != *term
            || term.chars().any(char::is_control)
            || characters > limits.maximum_public_key_term_utf16_units
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
    if total > limits.maximum_public_key_term_total_utf16_units {
        Err(LiveCapabilityError::KeyTermLimitExceeded)
    } else {
        Ok(())
    }
}

fn text_length(value: &str, unit: Option<TextLengthUnit>) -> usize {
    match unit {
        Some(TextLengthUnit::UnicodeScalars) => value.chars().count(),
        Some(TextLengthUnit::Utf16CodeUnits) | None => value.encode_utf16().count(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INPUTS: &[LiveInputFormat] = &[
        LiveInputFormat::Opus48KhzMono,
        LiveInputFormat::PcmS16Le16KhzMono,
    ];

    fn descriptor() -> LiveCapabilityDescriptor {
        LiveCapabilityDescriptor {
            protocol_version: 2,
            provider: "test",
            model: "model",
            timestamps: LiveTimestampCapability::Segment,
            timestamp_provenance: TimestampProvenance::GatewaySynthesizedFromAcceptedAudio,
            finalized_events: LiveFinalizedCapability::SegmentAndUtterance,
            language_hints: LiveLanguageHints::AsciiCode {
                maximum_bytes: 10,
                hyphen_at_edges: true,
            },
            diarization: false,
            key_terms: true,
            input_formats: INPUTS,
            provider_limits: LiveProviderLimits {
                maximum_public_input_frame_bytes: 64,
                maximum_input_frame_bytes: 128,
                maximum_key_terms: 2,
                maximum_key_term_bytes: None,
                maximum_public_key_term_utf16_units: 256,
                maximum_key_term_characters: Some(5),
                key_term_character_unit: Some(TextLengthUnit::UnicodeScalars),
                maximum_public_key_term_total_utf16_units: 10,
                normalize_key_term_whitespace: true,
            },
        }
    }

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
    fn checked_descriptor_accepts_its_supported_surface() {
        assert!(descriptor().validate(&supported(&["Voice"])).is_ok());
    }

    #[test]
    fn unsupported_live_features_fail_closed() {
        let mut descriptor = descriptor();
        let mut request = supported(&[]);
        descriptor.timestamps = LiveTimestampCapability::None;
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
        let descriptor = descriptor();
        let mut request = supported(&["x"]);
        request.language_hint = Some("not$valid");
        assert_eq!(
            descriptor.validate(&request),
            Err(LiveCapabilityError::UnsupportedLanguageHint)
        );
        let long = "x".repeat(21);
        let long_terms = [&*long];
        request.language_hint = Some("multi");
        request.key_terms = &long_terms;
        assert_eq!(
            descriptor.validate(&request),
            Err(LiveCapabilityError::InvalidKeyTerm)
        );
        let astral = "😀😀😀😀😀";
        let astral_terms = [astral];
        request.key_terms = &astral_terms;
        assert!(descriptor.validate(&request).is_ok());
    }
}
