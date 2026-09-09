//! Runtime-independent orchestration for one provider-bound live recognition session.

use std::fmt;

use crate::application::live_timeline::{AcceptedAudioError, LiveTimeline, LiveTimelineError};
use crate::application::ports::RecognitionFailure;
use crate::application::ports::{
    BoxFuture, LiveAudioFrame, LiveRecognitionEvent, LiveRecognitionRequest, LiveRecognizerFactory,
    LiveRecognizerSession, LiveTranscript, ProviderOperation,
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
    TimestampBeyondAcceptedAudio,
    FinalTimestampRegressionOrOverlap,
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
    timeline: LiveTimeline,
}

impl fmt::Debug for LiveCoordinator {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LiveCoordinator")
            .field("domain", &self.domain)
            .field("provider", &"dyn LiveRecognizerSession")
            .field("timeline", &self.timeline)
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
        let timeline = LiveTimeline::new(request.sample_rate_hz, request.channels)
            .map_err(|_| LiveCoordinatorError::InvalidAudioFrame)?;
        let provider = factory
            .open(request)
            .await
            .map_err(LiveCoordinatorError::Recognition)?;
        let mut domain = LiveSession::new();
        domain.mark_ready()?;
        Ok(Self {
            domain,
            provider,
            timeline,
        })
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
    /// Rejects empty, oversized, arithmetic-invalid, or non-channel-aligned PCM S16LE frames
    /// before changing state. The accepted-audio horizon advances only after provider success.
    /// Domain transition errors are preserved. Provider failure moves the session to `Failed` and
    /// is returned unchanged as [`LiveCoordinatorError::Recognition`].
    pub async fn provider_write(
        &mut self,
        pcm_s16le: Vec<u8>,
    ) -> Result<RawAudioSequence, LiveCoordinatorError> {
        if pcm_s16le.len() > MAX_PCM_FRAME_BYTES {
            return Err(LiveCoordinatorError::InvalidAudioFrame);
        }
        let pending_timeline = self
            .timeline
            .prepare_audio_write(pcm_s16le.len())
            .map_err(map_audio_error)?;
        let sequence = self.domain.accept_audio()?;
        let frame = LiveAudioFrame {
            sequence,
            pcm_s16le,
        };
        if let Err(error) = self.provider.write_audio(frame).await {
            self.fail_after_provider_error();
            return Err(LiveCoordinatorError::Recognition(error));
        }
        self.timeline.commit_audio_write(pending_timeline);
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

    /// Returns the actual typed provider operation currently known by the bound session.
    pub fn provider_operation(&self) -> BoxFuture<'_, Option<ProviderOperation>> {
        self.provider.provider_operation()
    }

    /// Validates and applies one result returned by [`Self::receive_provider_event`].
    ///
    /// Provider-only finalize evidence is consumed internally. Provider and invalid-event
    /// failures make the session terminal before the error is returned.
    ///
    /// # Errors
    ///
    /// Returns provider failures unchanged, domain transition errors for out-of-phase finalize
    /// evidence, or [`LiveCoordinatorError::InvalidProviderEvent`] for invalid output bounds,
    /// timestamps beyond accepted audio, or incompatible final ordering.
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
                if let Err(error) = self.timeline.validate_transcript(&transcript) {
                    self.fail_after_provider_error();
                    return Err(LiveCoordinatorError::InvalidProviderEvent(
                        map_timeline_error(error),
                    ));
                }
                Ok(Some(LiveCoordinatorEvent::Transcript(transcript)))
            }
            Some(LiveRecognitionEvent::UtteranceEnd {
                last_word_end_millis,
            }) => {
                if let Err(error) = self.timeline.validate_utterance_end(last_word_end_millis) {
                    self.fail_after_provider_error();
                    return Err(LiveCoordinatorError::InvalidProviderEvent(
                        map_timeline_error(error),
                    ));
                }
                Ok(Some(LiveCoordinatorEvent::UtteranceEnd {
                    last_word_end_millis,
                }))
            }
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

fn map_audio_error(_error: AcceptedAudioError) -> LiveCoordinatorError {
    LiveCoordinatorError::InvalidAudioFrame
}

const fn map_timeline_error(error: LiveTimelineError) -> InvalidProviderEvent {
    match error {
        LiveTimelineError::TimingOverflow => InvalidProviderEvent::TranscriptTimingOverflow,
        LiveTimelineError::BeyondAcceptedAudio => {
            InvalidProviderEvent::TimestampBeyondAcceptedAudio
        }
        LiveTimelineError::FinalRegressionOrOverlap => {
            InvalidProviderEvent::FinalTimestampRegressionOrOverlap
        }
    }
}

#[cfg(test)]
mod tests;
