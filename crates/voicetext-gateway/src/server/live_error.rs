//! Secret-safe live transport failures.

use voicetext_speech::application::live::LiveCoordinatorError;
use voicetext_speech::application::ports::RecognitionFailure;

use super::live_diagnostics::{recognition_failure_class, safe_provider_failure_code};

#[derive(Clone, Copy, Debug)]
pub(super) enum SafeLiveError {
    ConfigTimeout,
    InvalidConfig,
    InvalidCommand,
    InvalidAudio,
    FrameTooLarge,
    ProfileNotConfigured,
    AudioRuntimeUnavailable,
    PendingAcknowledgements,
    ProviderUnavailable,
    ProviderTerminal,
    ProviderOutcomeUnknown,
    ProviderClosed,
    InvalidProviderEvent,
    TransportClosed,
    ProviderOperationTimeout,
    SessionTimeout,
}

impl SafeLiveError {
    pub(super) fn from_coordinator(error: &LiveCoordinatorError, phase: &'static str) -> Self {
        if let LiveCoordinatorError::Recognition(failure) = error {
            tracing::warn!(
                phase,
                provider_code = safe_provider_failure_code(failure.code()),
                failure_class = recognition_failure_class(failure),
                "live provider operation failed"
            );
        }
        match error {
            LiveCoordinatorError::Recognition(RecognitionFailure::KnownNotAccepted { .. }) => {
                Self::ProviderUnavailable
            }
            LiveCoordinatorError::Recognition(RecognitionFailure::KnownAcceptedTerminal {
                ..
            }) => Self::ProviderTerminal,
            LiveCoordinatorError::Recognition(RecognitionFailure::UnknownAfterSend { .. }) => {
                Self::ProviderOutcomeUnknown
            }
            LiveCoordinatorError::InvalidAudioFrame => Self::InvalidAudio,
            LiveCoordinatorError::InvalidProviderEvent(_) => Self::InvalidProviderEvent,
            LiveCoordinatorError::Domain(_) => Self::InvalidCommand,
        }
    }

    pub(super) const fn code(self) -> &'static str {
        match self {
            Self::ConfigTimeout => "CONFIG_TIMEOUT",
            Self::InvalidConfig => "INVALID_CONFIG",
            Self::InvalidCommand => "INVALID_COMMAND",
            Self::InvalidAudio => "INVALID_AUDIO",
            Self::FrameTooLarge => "FRAME_TOO_LARGE",
            Self::ProfileNotConfigured => "PROFILE_NOT_CONFIGURED",
            Self::AudioRuntimeUnavailable => "AUDIO_RUNTIME_UNAVAILABLE",
            Self::PendingAcknowledgements => "PENDING_ACKNOWLEDGEMENTS",
            Self::ProviderUnavailable => "PROVIDER_UNAVAILABLE",
            Self::ProviderTerminal => "PROVIDER_TERMINAL",
            Self::ProviderOutcomeUnknown => "PROVIDER_OUTCOME_UNKNOWN",
            Self::ProviderClosed => "PROVIDER_CLOSED",
            Self::InvalidProviderEvent => "INVALID_PROVIDER_EVENT",
            Self::TransportClosed => "TRANSPORT_CLOSED",
            Self::ProviderOperationTimeout => "PROVIDER_TIMEOUT",
            Self::SessionTimeout => "TIMEOUT",
        }
    }

    pub(super) const fn message(self) -> &'static str {
        match self {
            Self::ConfigTimeout => "Live configuration timed out",
            Self::InvalidConfig => "Live configuration is invalid",
            Self::InvalidCommand => "Live command is invalid",
            Self::InvalidAudio => "Live audio frame is invalid",
            Self::FrameTooLarge => "Live frame exceeds the configured limit",
            Self::ProfileNotConfigured => "Requested live profile is not configured",
            Self::AudioRuntimeUnavailable => "Live audio decoder is unavailable",
            Self::PendingAcknowledgements => "Audio acknowledgements are still pending",
            Self::ProviderUnavailable => "Live provider is temporarily unavailable",
            Self::ProviderTerminal => "Live provider rejected the session",
            Self::ProviderOutcomeUnknown => "Live provider outcome is unknown",
            Self::ProviderClosed => "Live provider closed unexpectedly",
            Self::InvalidProviderEvent => "Live provider returned an invalid event",
            Self::TransportClosed => "Live transport closed",
            Self::ProviderOperationTimeout => "Live provider operation timed out",
            Self::SessionTimeout => "Live session timed out",
        }
    }
}
