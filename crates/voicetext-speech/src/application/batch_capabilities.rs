//! Provider-neutral capability contract for one authoritative batch profile.

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

/// Exact public and provider bounds applied before paid batch egress.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BatchProviderLimits {
    /// Gateway admission bound, before durable storage.
    pub maximum_public_input_bytes: usize,
    /// Upstream provider bound checked again immediately before egress.
    pub maximum_input_bytes: usize,
    pub maximum_key_terms: usize,
    pub maximum_key_term_bytes: usize,
    pub maximum_key_term_characters: Option<usize>,
    pub key_term_character_unit: Option<TextLengthUnit>,
    pub maximum_key_term_words: Option<usize>,
    pub normalize_key_term_whitespace: bool,
    pub restricted_key_term_punctuation: bool,
}

/// Explicit unit used by a provider text limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextLengthUnit {
    UnicodeScalars,
    Utf16CodeUnits,
}

/// Provenance of timestamps projected by the gateway.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimestampProvenance {
    ProviderNative,
    GatewaySynthesizedFromAcceptedAudio,
}

/// Narrow reusable descriptor for one batch profile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BatchCapabilityDescriptor {
    pub contract_version: u16,
    pub provider: &'static str,
    pub model: &'static str,
    pub language: &'static str,
    pub timestamps: BatchTimestampCapability,
    pub timestamp_provenance: TimestampProvenance,
    pub finalized_events: BatchFinalizedCapability,
    pub language_hints: BatchLanguageHints,
    pub diarization: bool,
    pub key_terms: bool,
    pub input_formats: &'static [BatchInputFormat],
    pub provider_limits: BatchProviderLimits,
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
            || request.input_bytes > self.provider_limits.maximum_public_input_bytes
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
            || exceeds_text_limit(
                provider_term,
                limits.maximum_key_term_characters,
                limits.key_term_character_unit,
            )
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

fn exceeds_text_limit(value: &str, maximum: Option<usize>, unit: Option<TextLengthUnit>) -> bool {
    match (maximum, unit) {
        (None, None) => false,
        (Some(maximum), Some(TextLengthUnit::UnicodeScalars)) => value.chars().count() > maximum,
        (Some(maximum), Some(TextLengthUnit::Utf16CodeUnits)) => {
            value.encode_utf16().count() > maximum
        }
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INPUTS: &[BatchInputFormat] = &[BatchInputFormat::OggOpus];

    fn descriptor() -> BatchCapabilityDescriptor {
        BatchCapabilityDescriptor {
            contract_version: 1,
            provider: "test",
            model: "model",
            language: "multi",
            timestamps: BatchTimestampCapability::Segment,
            timestamp_provenance: TimestampProvenance::ProviderNative,
            finalized_events: BatchFinalizedCapability::TerminalTranscript,
            language_hints: BatchLanguageHints::Fixed("multi"),
            diarization: false,
            key_terms: true,
            input_formats: INPUTS,
            provider_limits: BatchProviderLimits {
                maximum_public_input_bytes: 64,
                maximum_input_bytes: 128,
                maximum_key_terms: 2,
                maximum_key_term_bytes: 200,
                maximum_key_term_characters: Some(5),
                key_term_character_unit: Some(TextLengthUnit::UnicodeScalars),
                maximum_key_term_words: Some(2),
                normalize_key_term_whitespace: true,
                restricted_key_term_punctuation: true,
            },
        }
    }

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
    fn checked_descriptor_accepts_its_supported_surface() {
        assert!(descriptor().validate(&supported(&["Voice"])).is_ok());
    }

    #[test]
    fn unsupported_batch_features_fail_closed() {
        let descriptor = descriptor();
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
    fn scalar_provider_limit_is_distinct_from_public_utf16_accounting() {
        let descriptor = descriptor();
        assert_eq!(
            descriptor.validate(&supported(&["one two three"])),
            Err(BatchCapabilityError::InvalidKeyTerm)
        );
        assert_eq!(
            descriptor.validate(&supported(&["<unsupported>"])),
            Err(BatchCapabilityError::InvalidKeyTerm)
        );
        assert!(descriptor.validate(&supported(&["😀😀😀😀😀"])).is_ok());
    }
}
