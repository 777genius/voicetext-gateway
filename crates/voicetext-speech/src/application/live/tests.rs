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
    fn write_audio(&self, frame: LiveAudioFrame) -> BoxFuture<'_, Result<(), RecognitionFailure>> {
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
    fn capabilities(
        &self,
    ) -> &'static crate::application::live_capabilities::LiveCapabilityDescriptor {
        panic!("application fake has no composition capabilities")
    }

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
fn coordinator_enforces_horizon_after_successful_provider_writes() {
    let (mut inside, _) = fixture();
    block_on(inside.provider_write(vec![0; 24_000])).unwrap();
    assert!(matches!(
        inside
            .apply_provider_event(Ok(Some(LiveRecognitionEvent::Transcript(LiveTranscript {
                text: "inside".into(),
                start_millis: 250,
                duration_millis: 250,
                confidence: None,
                stability: LiveTranscriptStability::Partial,
            }))))
            .unwrap(),
        Some(LiveCoordinatorEvent::Transcript(_))
    ));

    let (mut outside, _) = fixture();
    block_on(outside.provider_write(vec![0; 24_000])).unwrap();
    assert_eq!(
        outside.apply_provider_event(Ok(Some(LiveRecognitionEvent::UtteranceEnd {
            last_word_end_millis: 501,
        }))),
        Err(LiveCoordinatorError::InvalidProviderEvent(
            InvalidProviderEvent::TimestampBeyondAcceptedAudio
        ))
    );
    assert_eq!(outside.phase(), LiveSessionPhase::Failed);
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
