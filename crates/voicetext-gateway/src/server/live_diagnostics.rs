//! Safe, bounded diagnostics for provider-neutral live failures.

use voicetext_speech::application::ports::RecognitionFailure;

pub(super) fn safe_provider_failure_code(code: &str) -> &str {
    if !code.is_empty()
        && code.len() <= 128
        && code
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
    {
        code
    } else {
        "UNSAFE_PROVIDER_FAILURE_CODE"
    }
}

pub(super) const fn recognition_failure_class(failure: &RecognitionFailure) -> &'static str {
    match failure {
        RecognitionFailure::KnownNotAccepted { .. } => "known_not_accepted",
        RecognitionFailure::KnownAcceptedTerminal { .. } => "known_accepted_terminal",
        RecognitionFailure::UnknownAfterSend { .. } => "unknown_after_send",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposes_only_bounded_machine_codes() {
        assert_eq!(
            safe_provider_failure_code("ELEVENLABS_LIVE_PROVIDER_ERROR"),
            "ELEVENLABS_LIVE_PROVIDER_ERROR"
        );
        assert_eq!(
            safe_provider_failure_code("provider returned a secret"),
            "UNSAFE_PROVIDER_FAILURE_CODE"
        );
        assert_eq!(
            safe_provider_failure_code(&"A".repeat(129)),
            "UNSAFE_PROVIDER_FAILURE_CODE"
        );
    }
}
