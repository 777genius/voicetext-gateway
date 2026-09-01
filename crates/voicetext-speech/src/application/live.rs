//! Runtime-independent orchestration for one provider-bound live recognition session.

use std::fmt;

use crate::application::ports::RecognitionFailure;
use crate::application::ports::{
    BoxFuture, LiveAudioFrame, LiveRecognitionEvent, LiveRecognitionRequest, LiveRecognizerFactory,
    LiveRecognizerSession, LiveTranscript,
};
use crate::domain::live::{
    FinalizeOutcome, FinalizeStatus, LiveSession, LiveSessionError, LiveSessionPhase,
    RawAudioSequence,
};

const MAX_PCM_FRAME_BYTES: usize = 64 * 1_024;
const MAX_TRANSCRIPT_CHARS: usize = 65_536;

/// Why a normalized provider event was rejected defensively.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvalidProviderEvent {
    TranscriptTooLong,
    TranscriptTimingOverflow,
    InvalidConfidence,
}

/// Failure from live-session orchestration without erasing provider retry evidence.
#[derive(Clone, Debug, PartialEq)]
pub enum LiveCoordinatorError {
    Domain(LiveSessionError),
    Recognition(RecognitionFailure),
    InvalidAudioFrame,
    InvalidProviderEvent(InvalidProviderEvent),
}

/// Provider-neutral events that transport may project to its own wire protocol.
#[derive(Clone, Debug, PartialEq)]
pub enum LiveCoordinatorEvent {
    Transcript(LiveTranscript),
    UtteranceEnd { last_word_end_millis: u64 },
}

impl From<LiveSessionError> for LiveCoordinatorError {
    fn from(error: LiveSessionError) -> Self {
        Self::Domain(error)
    }
}

/// Coordinates deterministic session state with one already-bound provider session.
pub struct LiveCoordinator {
    domain: LiveSession,
    provider: Box<dyn LiveRecognizerSession>,
}

impl fmt::Debug for LiveCoordinator {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LiveCoordinator")
            .field("domain", &self.domain)
            .field("provider", &"dyn LiveRecognizerSession")
            .finish()
    }
}

impl LiveCoordinator {
    /// Opens the exact requested provider session before making the domain session ready.
    ///
    /// # Errors
    ///
    /// Returns [`LiveCoordinatorError::Recognition`] unchanged when provider setup fails.
    pub async fn open(
        factory: &dyn LiveRecognizerFactory,
        request: LiveRecognitionRequest,
    ) -> Result<Self, LiveCoordinatorError> {
        let provider = factory
            .open(request)
            .await
            .map_err(LiveCoordinatorError::Recognition)?;
        let mut domain = LiveSession::new();
        domain.mark_ready()?;
        Ok(Self { domain, provider })
    }

    /// Current deterministic lifecycle phase.
    #[must_use]
    pub const fn phase(&self) -> LiveSessionPhase {
        self.domain.phase()
    }

    /// Number of provider-written or in-flight frames awaiting a transport ACK.
    #[must_use]
    pub fn pending_audio_count(&self) -> usize {
        self.domain.pending_audio_count()
    }

    /// Assigns a sequence and returns it only after the provider write succeeds.
    ///
    /// # Errors
    ///
    /// Rejects empty, odd-length, or oversized PCM S16LE frames before changing state. Domain
    /// transition errors are preserved. Provider failure moves the session to `Failed` and is
    /// returned unchanged as [`LiveCoordinatorError::Recognition`].
    pub async fn provider_write(
        &mut self,
        pcm_s16le: Vec<u8>,
    ) -> Result<RawAudioSequence, LiveCoordinatorError> {
        if pcm_s16le.is_empty()
            || (pcm_s16le.len() & 1) != 0
            || pcm_s16le.len() > MAX_PCM_FRAME_BYTES
        {
            return Err(LiveCoordinatorError::InvalidAudioFrame);
        }
        let sequence = self.domain.accept_audio()?;
        let frame = LiveAudioFrame {
            sequence,
            pcm_s16le,
        };
        if let Err(error) = self.provider.write_audio(frame).await {
            self.fail_after_provider_error();
            return Err(LiveCoordinatorError::Recognition(error));
        }
        self.domain.mark_provider_write_succeeded(sequence)?;
        Ok(sequence)
    }

    /// Confirms that transport successfully sent an ACK for a provider-written frame.
    ///
    /// # Errors
    ///
    /// Returns a domain error for unknown, duplicate, or not-yet-provider-written sequences.
    pub fn ack_sent(&mut self, sequence: RawAudioSequence) -> Result<(), LiveCoordinatorError> {
        self.domain.mark_ack_sent(sequence)?;
        Ok(())
    }

    /// Starts one provider read without mutably borrowing coordinator state.
    ///
    /// This two-phase boundary lets a transport race the returned future against independent
    /// client input. When client input wins, the transport drops this future before mutating the
    /// coordinator (for example with [`Self::provider_write`]). The received result must be passed
    /// to [`Self::apply_provider_event`] so failures and finalize evidence update domain state.
    #[must_use]
    pub fn receive_provider_event(
        &self,
    ) -> BoxFuture<'_, Result<Option<LiveRecognitionEvent>, RecognitionFailure>> {
        self.provider.next_event()
    }

    /// Validates and applies one result returned by [`Self::receive_provider_event`].
    ///
    /// Provider-only finalize evidence is consumed internally. Provider and invalid-event
    /// failures make the session terminal before the error is returned.
    ///
    /// # Errors
    ///
    /// Returns provider failures unchanged, domain transition errors for out-of-phase finalize
    /// evidence, or [`LiveCoordinatorError::InvalidProviderEvent`] for invalid output bounds.
    pub fn apply_provider_event(
        &mut self,
        received: Result<Option<LiveRecognitionEvent>, RecognitionFailure>,
    ) -> Result<Option<LiveCoordinatorEvent>, LiveCoordinatorError> {
        let event = match received {
            Ok(event) => event,
            Err(error) => {
                self.fail_after_provider_error();
                return Err(LiveCoordinatorError::Recognition(error));
            }
        };
        match event {
            None => Ok(None),
            Some(LiveRecognitionEvent::Transcript(transcript)) => {
                if let Err(error) = validate_transcript(&transcript) {
                    self.fail_after_provider_error();
                    return Err(LiveCoordinatorError::InvalidProviderEvent(error));
                }
                Ok(Some(LiveCoordinatorEvent::Transcript(transcript)))
            }
            Some(LiveRecognitionEvent::UtteranceEnd {
                last_word_end_millis,
            }) => Ok(Some(LiveCoordinatorEvent::UtteranceEnd {
                last_word_end_millis,
            })),
            Some(LiveRecognitionEvent::FinalizeResultObserved) => {
                if let Err(error) = self.domain.mark_finalize_result_observed() {
                    self.fail_after_provider_error();
                    return Err(LiveCoordinatorError::Domain(error));
                }
                Ok(None)
            }
        }
    }

    /// Receives and validates the next provider-neutral event.
    ///
    /// Provider-only finalize evidence is recorded and consumed internally; transport receives
    /// only transcript or utterance events. Invalid output and stream failure are terminal.
    ///
    /// # Errors
    ///
    /// Returns provider failures unchanged, domain transition errors for out-of-phase finalize
    /// evidence, or [`LiveCoordinatorError::InvalidProviderEvent`] for invalid output bounds.
    pub async fn next_event(
        &mut self,
    ) -> Result<Option<LiveCoordinatorEvent>, LiveCoordinatorError> {
        loop {
            let received = self.receive_provider_event().await;
            let stream_ended = matches!(received, Ok(None));
            if let Some(event) = self.apply_provider_event(received)? {
                return Ok(Some(event));
            }
            if stream_ended {
                return Ok(None);
            }
        }
    }

    /// Starts provider finalization only after all provider-written frames have transport ACKs.
    ///
    /// # Errors
    ///
    /// A domain rejection prevents provider finalization. Provider failure makes the session
    /// terminal and is returned unchanged.
    pub async fn begin_finalize(&mut self) -> Result<(), LiveCoordinatorError> {
        self.domain.begin_finalize()?;
        if let Err(error) = self.provider.finalize().await {
            self.fail_after_provider_error();
            return Err(LiveCoordinatorError::Recognition(error));
        }
        Ok(())
    }

    /// Completes the externally bounded drain with truthful observed-result evidence.
    ///
    /// # Errors
    ///
    /// Returns a domain error when the status contradicts accepted audio or observed results.
    pub fn complete_finalize(
        &mut self,
        status: FinalizeStatus,
    ) -> Result<FinalizeOutcome, LiveCoordinatorError> {
        Ok(self.domain.complete_finalize(status)?)
    }

    /// Closes provider resources and deterministic state.
    ///
    /// Both close operations are attempted. A provider failure is preserved over a simultaneous
    /// domain close error because it carries external-effect evidence.
    ///
    /// # Errors
    ///
    /// Returns the provider close failure unchanged, otherwise any domain close error.
    pub async fn close(&mut self) -> Result<(), LiveCoordinatorError> {
        let provider_result = self.provider.close().await;
        let domain_result = self.domain.close();
        if let Err(error) = provider_result {
            return Err(LiveCoordinatorError::Recognition(error));
        }
        domain_result?;
        Ok(())
    }

    fn fail_after_provider_error(&mut self) {
        let _transition = self.domain.fail();
    }
}

fn validate_transcript(transcript: &LiveTranscript) -> Result<(), InvalidProviderEvent> {
    if transcript.text.chars().count() > MAX_TRANSCRIPT_CHARS {
        return Err(InvalidProviderEvent::TranscriptTooLong);
    }
    if transcript
        .start_millis
        .checked_add(transcript.duration_millis)
        .is_none()
    {
        return Err(InvalidProviderEvent::TranscriptTimingOverflow);
    }
    if transcript
        .confidence
        .is_some_and(|value| !value.is_finite() || !(0.0..=1.0).contains(&value))
    {
        return Err(InvalidProviderEvent::InvalidConfidence);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::future::Future;
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll, Waker};

    use super::*;
    use crate::application::ports::{BoxFuture, LiveProfile, LiveTranscriptStability};

    #[derive(Debug, Default)]
    struct FakeState {
        writes: Vec<LiveAudioFrame>,
        events: VecDeque<Result<Option<LiveRecognitionEvent>, RecognitionFailure>>,
        write_failure: Option<RecognitionFailure>,
        finalize_failure: Option<RecognitionFailure>,
        close_failure: Option<RecognitionFailure>,
        next_event_pending: bool,
        finalize_calls: usize,
        close_calls: usize,
    }

    #[derive(Clone, Debug)]
    struct FakeSession(Arc<Mutex<FakeState>>);

    impl LiveRecognizerSession for FakeSession {
        fn write_audio(
            &self,
            frame: LiveAudioFrame,
        ) -> BoxFuture<'_, Result<(), RecognitionFailure>> {
            Box::pin(async move {
                let mut state = self.0.lock().unwrap();
                if let Some(error) = state.write_failure.clone() {
                    return Err(error);
                }
                state.writes.push(frame);
                Ok(())
            })
        }

        fn next_event(
            &self,
        ) -> BoxFuture<'_, Result<Option<LiveRecognitionEvent>, RecognitionFailure>> {
            Box::pin(async move {
                if self.0.lock().unwrap().next_event_pending {
                    std::future::pending::<()>().await;
                }
                self.0
                    .lock()
                    .unwrap()
                    .events
                    .pop_front()
                    .unwrap_or(Ok(None))
            })
        }

        fn finalize(&self) -> BoxFuture<'_, Result<(), RecognitionFailure>> {
            Box::pin(async move {
                let mut state = self.0.lock().unwrap();
                state.finalize_calls += 1;
                state.finalize_failure.clone().map_or(Ok(()), Err)
            })
        }

        fn close(&self) -> BoxFuture<'_, Result<(), RecognitionFailure>> {
            Box::pin(async move {
                let mut state = self.0.lock().unwrap();
                state.close_calls += 1;
                state.close_failure.clone().map_or(Ok(()), Err)
            })
        }
    }

    #[derive(Debug)]
    struct FakeFactory {
        session: FakeSession,
        failure: Option<RecognitionFailure>,
    }

    impl LiveRecognizerFactory for FakeFactory {
        fn open(
            &self,
            _request: LiveRecognitionRequest,
        ) -> BoxFuture<'_, Result<Box<dyn LiveRecognizerSession>, RecognitionFailure>> {
            Box::pin(async move {
                if let Some(error) = self.failure.clone() {
                    return Err(error);
                }
                Ok(Box::new(self.session.clone()) as Box<dyn LiveRecognizerSession>)
            })
        }
    }

    fn failure() -> RecognitionFailure {
        RecognitionFailure::KnownAcceptedTerminal {
            code: "PROVIDER_FAILED".into(),
            provider_reference: None,
        }
    }

    fn request() -> LiveRecognitionRequest {
        LiveRecognitionRequest {
            profile: LiveProfile {
                protocol_version: 2,
                provider: "provider".into(),
                model: "model".into(),
                language: "multi".into(),
            },
            sample_rate_hz: 48_000,
            channels: 1,
            keyterms: Vec::new(),
        }
    }

    fn fixture() -> (LiveCoordinator, Arc<Mutex<FakeState>>) {
        let state = Arc::new(Mutex::new(FakeState::default()));
        let factory = FakeFactory {
            session: FakeSession(Arc::clone(&state)),
            failure: None,
        };
        (
            block_on(LiveCoordinator::open(&factory, request())).unwrap(),
            state,
        )
    }

    #[test]
    fn open_preserves_provider_failure() {
        let state = Arc::new(Mutex::new(FakeState::default()));
        let factory = FakeFactory {
            session: FakeSession(state),
            failure: Some(failure()),
        };
        assert!(matches!(
            block_on(LiveCoordinator::open(&factory, request())),
            Err(LiveCoordinatorError::Recognition(_))
        ));
    }

    #[test]
    fn provider_success_precedes_ack_and_ack_order_is_independent() {
        let (mut coordinator, state) = fixture();
        let first = block_on(coordinator.provider_write(vec![0, 0])).unwrap();
        let second = block_on(coordinator.provider_write(vec![1, 0])).unwrap();
        assert_eq!(coordinator.pending_audio_count(), 2);
        assert_eq!(state.lock().unwrap().writes.len(), 2);
        coordinator.ack_sent(second).unwrap();
        coordinator.ack_sent(first).unwrap();
        assert_eq!(coordinator.pending_audio_count(), 0);
        assert!(coordinator.ack_sent(first).is_err());
    }

    #[test]
    fn dropping_pending_provider_read_releases_coordinator_for_client_audio() {
        let (mut coordinator, state) = fixture();
        state.lock().unwrap().next_event_pending = true;

        let mut provider_read = coordinator.receive_provider_event();
        let waker = Waker::noop();
        let mut context = Context::from_waker(waker);
        assert!(provider_read.as_mut().poll(&mut context).is_pending());

        // A transport's client-input branch drops the losing provider read before mutation.
        drop(provider_read);
        assert_eq!(coordinator.phase(), LiveSessionPhase::Streaming);
        let sequence = block_on(coordinator.provider_write(vec![0, 0])).unwrap();
        coordinator.ack_sent(sequence).unwrap();
        assert_eq!(state.lock().unwrap().writes.len(), 1);
    }

    #[test]
    fn provider_write_failure_is_terminal_and_returns_no_sequence() {
        let (mut coordinator, state) = fixture();
        assert_eq!(
            block_on(coordinator.provider_write(vec![0])),
            Err(LiveCoordinatorError::InvalidAudioFrame)
        );
        assert_eq!(coordinator.pending_audio_count(), 0);
        assert_eq!(coordinator.phase(), LiveSessionPhase::Streaming);
        state.lock().unwrap().write_failure = Some(failure());
        assert!(matches!(
            block_on(coordinator.provider_write(vec![0, 0])),
            Err(LiveCoordinatorError::Recognition(_))
        ));
        assert_eq!(coordinator.phase(), LiveSessionPhase::Failed);
    }

    #[test]
    fn pending_ack_prevents_provider_finalize() {
        let (mut coordinator, state) = fixture();
        let _sequence = block_on(coordinator.provider_write(vec![0, 0])).unwrap();
        assert!(matches!(
            block_on(coordinator.begin_finalize()),
            Err(LiveCoordinatorError::Domain(
                LiveSessionError::FinalizeHasPendingAudio
            ))
        ));
        assert_eq!(state.lock().unwrap().finalize_calls, 0);
    }

    #[test]
    fn provider_finalize_failure_is_terminal() {
        let (mut coordinator, state) = fixture();
        state.lock().unwrap().finalize_failure = Some(failure());
        assert!(matches!(
            block_on(coordinator.begin_finalize()),
            Err(LiveCoordinatorError::Recognition(_))
        ));
        assert_eq!(coordinator.phase(), LiveSessionPhase::Failed);
    }

    #[test]
    fn observed_result_allows_flushed_and_timeout_reports_evidence() {
        let (mut coordinator, state) = fixture();
        block_on(coordinator.begin_finalize()).unwrap();
        state
            .lock()
            .unwrap()
            .events
            .push_back(Ok(Some(LiveRecognitionEvent::FinalizeResultObserved)));
        assert_eq!(block_on(coordinator.next_event()).unwrap(), None);
        let outcome = coordinator
            .complete_finalize(FinalizeStatus::Flushed)
            .unwrap();
        assert!(outcome.saw_result());

        let (mut timeout, _) = fixture();
        block_on(timeout.begin_finalize()).unwrap();
        let outcome = timeout.complete_finalize(FinalizeStatus::Timeout).unwrap();
        assert!(!outcome.saw_result());
    }

    #[test]
    fn provider_and_invalid_event_failures_are_terminal() {
        let (mut coordinator, state) = fixture();
        state.lock().unwrap().events.push_back(Err(failure()));
        assert!(matches!(
            block_on(coordinator.next_event()),
            Err(LiveCoordinatorError::Recognition(_))
        ));
        assert_eq!(coordinator.phase(), LiveSessionPhase::Failed);

        let (mut invalid, state) = fixture();
        state
            .lock()
            .unwrap()
            .events
            .push_back(Ok(Some(LiveRecognitionEvent::Transcript(LiveTranscript {
                text: "x".into(),
                start_millis: u64::MAX,
                duration_millis: 1,
                confidence: Some(0.5),
                stability: LiveTranscriptStability::Partial,
            }))));
        assert!(matches!(
            block_on(invalid.next_event()),
            Err(LiveCoordinatorError::InvalidProviderEvent(
                InvalidProviderEvent::TranscriptTimingOverflow
            ))
        ));
        assert_eq!(invalid.phase(), LiveSessionPhase::Failed);
    }

    #[test]
    fn close_attempts_both_and_preserves_provider_failure() {
        let (mut coordinator, state) = fixture();
        state.lock().unwrap().close_failure = Some(failure());
        assert!(matches!(
            block_on(coordinator.close()),
            Err(LiveCoordinatorError::Recognition(_))
        ));
        assert_eq!(coordinator.phase(), LiveSessionPhase::Closed);
        assert_eq!(state.lock().unwrap().close_calls, 1);
    }

    fn block_on<F: Future>(future: F) -> F::Output {
        let mut future = Box::pin(future);
        let waker = Waker::noop();
        let mut context = Context::from_waker(waker);
        loop {
            if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
                return output;
            }
        }
    }
}
