use super::*;
use voicetext_speech::application::live::LiveCoordinatorError;

#[test]
fn pcm_requires_complete_sixteen_bit_samples() {
    assert_eq!(decode_audio(&mut None, vec![1, 2]).unwrap(), vec![1, 2]);
    assert!(decode_audio(&mut None, vec![1]).is_err());
    assert!(decode_audio(&mut None, Vec::new()).is_err());
}

#[test]
fn maps_transcript_stability_without_identity_drift() {
    let message = transcript_message(LiveTranscript {
        text: "hello".into(),
        start_millis: 4,
        duration_millis: 5,
        confidence: Some(0.75),
        stability: LiveTranscriptStability::SegmentFinal,
    });
    assert!(matches!(message, OutboundServerMessage::SegmentFinal(_)));
}

#[test]
fn provider_failure_mapping_executes_safe_diagnostics() {
    let failure = LiveCoordinatorError::Recognition(RecognitionFailure::KnownAcceptedTerminal {
        code: "ELEVENLABS_LIVE_PROVIDER_ERROR".into(),
        provider_reference: None,
    });
    assert!(matches!(
        SafeLiveError::from_coordinator(&failure, "stream"),
        SafeLiveError::ProviderTerminal
    ));
}

#[tokio::test(start_paused = true)]
async fn session_lifetime_deadline_is_absolute_when_idle_activity_resets() {
    let opened = Instant::now();
    let session_deadline = opened + LIVE_SESSION_TIMEOUT;
    let mut idle_deadline = opened + LIVE_IDLE_TIMEOUT;
    assert!(idle_deadline < session_deadline);
    for _ in 0..3 {
        tokio::time::advance(LIVE_IDLE_TIMEOUT / 2).await;
        idle_deadline = Instant::now() + LIVE_IDLE_TIMEOUT;
        assert!(idle_deadline < session_deadline);
    }
    tokio::time::advance(session_deadline - Instant::now()).await;
    sleep_until(session_deadline).await;
    assert_eq!(Instant::now(), session_deadline);
}
