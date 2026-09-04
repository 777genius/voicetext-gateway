//! Deterministic live-transcription session state.

use std::collections::BTreeSet;

/// Maximum number of accepted audio frames that may await a client ACK.
pub const MAX_PENDING_AUDIO_SEQUENCES: usize = 256;

/// One server-assigned raw-audio sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct RawAudioSequence(u64);

impl RawAudioSequence {
    /// Returns the positive wire sequence number.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Observable lifecycle phase of a live transcription session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveSessionPhase {
    AwaitingReady,
    Streaming,
    Finalizing,
    Finalized,
    Closed,
    Failed,
}

/// Requested terminal status for a bounded finalize operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinalizeStatus {
    Flushed,
    NoProvider,
    Timeout,
}

/// A finalize result whose status and evidence have been validated by the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FinalizeOutcome {
    status: FinalizeStatus,
    saw_result: bool,
}

impl FinalizeOutcome {
    /// Returns the validated finalize status.
    #[must_use]
    pub const fn status(self) -> FinalizeStatus {
        self.status
    }

    /// Reports whether a provider result was observed during the finalize drain.
    #[must_use]
    pub const fn saw_result(self) -> bool {
        self.saw_result
    }
}

/// Operation attempted during an invalid lifecycle phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveSessionOperation {
    BecomeReady,
    AcceptAudio,
    ConfirmProviderWrite,
    ConfirmAcknowledgement,
    BeginFinalize,
    ObserveFinalizeResult,
    CompleteFinalize,
    Close,
    Fail,
}

/// Deterministic rejection from a live-session state transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveSessionError {
    InvalidTransition {
        phase: LiveSessionPhase,
        operation: LiveSessionOperation,
    },
    PendingAudioLimitReached,
    AudioSequenceExhausted,
    UnknownAudioSequence(RawAudioSequence),
    ProviderWriteAlreadyConfirmed(RawAudioSequence),
    ProviderWriteNotConfirmed(RawAudioSequence),
    FinalizeHasPendingAudio,
    FlushedWithoutResult,
    NoProviderAfterAudio,
    NoProviderAfterResult,
}

/// Provider-neutral state of one bounded live transcription session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveSession {
    phase: LiveSessionPhase,
    pending_audio: BTreeSet<RawAudioSequence>,
    provider_written_audio: BTreeSet<RawAudioSequence>,
    last_sequence: u64,
    accepted_audio: bool,
    finalize_result_observed: bool,
}

impl Default for LiveSession {
    fn default() -> Self {
        Self {
            phase: LiveSessionPhase::AwaitingReady,
            pending_audio: BTreeSet::new(),
            provider_written_audio: BTreeSet::new(),
            last_sequence: 0,
            accepted_audio: false,
            finalize_result_observed: false,
        }
    }
}

impl LiveSession {
    /// Creates a session that cannot accept audio until readiness is confirmed.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the current lifecycle phase.
    #[must_use]
    pub const fn phase(&self) -> LiveSessionPhase {
        self.phase
    }

    /// Returns whether the session has ever accepted a raw-audio frame.
    #[must_use]
    pub const fn has_accepted_audio(&self) -> bool {
        self.accepted_audio
    }

    /// Returns the number of accepted frames that have not been acknowledged to the client.
    #[must_use]
    pub fn pending_audio_count(&self) -> usize {
        self.pending_audio.len()
    }

    /// Returns pending sequence numbers in ascending order.
    pub fn pending_audio_sequences(&self) -> impl Iterator<Item = RawAudioSequence> + '_ {
        self.pending_audio.iter().copied()
    }

    /// Moves an awaiting session into streaming after its provider is ready.
    ///
    /// # Errors
    ///
    /// Returns [`LiveSessionError::InvalidTransition`] unless the session is awaiting readiness.
    pub fn mark_ready(&mut self) -> Result<(), LiveSessionError> {
        self.require_phase(
            LiveSessionPhase::AwaitingReady,
            LiveSessionOperation::BecomeReady,
        )?;
        self.phase = LiveSessionPhase::Streaming;
        Ok(())
    }

    /// Accepts one raw-audio frame and assigns its positive sequence number.
    ///
    /// # Errors
    ///
    /// Returns an error when the session is not streaming, 256 frames are already pending, or the
    /// sequence space is exhausted.
    pub fn accept_audio(&mut self) -> Result<RawAudioSequence, LiveSessionError> {
        self.require_phase(
            LiveSessionPhase::Streaming,
            LiveSessionOperation::AcceptAudio,
        )?;
        if self.pending_audio.len() == MAX_PENDING_AUDIO_SEQUENCES {
            return Err(LiveSessionError::PendingAudioLimitReached);
        }

        let value = self
            .last_sequence
            .checked_add(1)
            .ok_or(LiveSessionError::AudioSequenceExhausted)?;
        let sequence = RawAudioSequence(value);
        let inserted = self.pending_audio.insert(sequence);
        debug_assert!(inserted, "newly assigned sequence must be unique");
        self.last_sequence = value;
        self.accepted_audio = true;
        Ok(sequence)
    }

    /// Records successful provider egress for one pending frame.
    ///
    /// A client ACK is still forbidden until [`Self::mark_ack_sent`] completes the second step.
    ///
    /// # Errors
    ///
    /// Returns an error unless the session is streaming and the sequence is pending exactly once.
    pub fn mark_provider_write_succeeded(
        &mut self,
        sequence: RawAudioSequence,
    ) -> Result<(), LiveSessionError> {
        self.require_phase(
            LiveSessionPhase::Streaming,
            LiveSessionOperation::ConfirmProviderWrite,
        )?;
        if !self.pending_audio.contains(&sequence) {
            return Err(LiveSessionError::UnknownAudioSequence(sequence));
        }
        if !self.provider_written_audio.insert(sequence) {
            return Err(LiveSessionError::ProviderWriteAlreadyConfirmed(sequence));
        }
        Ok(())
    }

    /// Records that the client ACK was sent for a provider-written frame.
    ///
    /// # Errors
    ///
    /// Returns an error unless the frame is pending and its provider write already succeeded.
    pub fn mark_ack_sent(&mut self, sequence: RawAudioSequence) -> Result<(), LiveSessionError> {
        self.require_phase(
            LiveSessionPhase::Streaming,
            LiveSessionOperation::ConfirmAcknowledgement,
        )?;
        if !self.pending_audio.contains(&sequence) {
            return Err(LiveSessionError::UnknownAudioSequence(sequence));
        }
        if !self.provider_written_audio.remove(&sequence) {
            return Err(LiveSessionError::ProviderWriteNotConfirmed(sequence));
        }
        let removed = self.pending_audio.remove(&sequence);
        debug_assert!(
            removed,
            "a provider-written sequence must remain pending until ACK"
        );
        Ok(())
    }

    /// Begins the bounded finalize drain after every accepted frame has been acknowledged.
    ///
    /// # Errors
    ///
    /// Returns an error unless the session is streaming with no pending frames.
    pub fn begin_finalize(&mut self) -> Result<(), LiveSessionError> {
        self.require_phase(
            LiveSessionPhase::Streaming,
            LiveSessionOperation::BeginFinalize,
        )?;
        if !self.pending_audio.is_empty() {
            return Err(LiveSessionError::FinalizeHasPendingAudio);
        }
        debug_assert!(self.provider_written_audio.is_empty());
        self.finalize_result_observed = false;
        self.phase = LiveSessionPhase::Finalizing;
        Ok(())
    }

    /// Records one result observed during the bounded finalize drain.
    ///
    /// # Errors
    ///
    /// Returns [`LiveSessionError::InvalidTransition`] unless finalization is in progress.
    pub fn mark_finalize_result_observed(&mut self) -> Result<(), LiveSessionError> {
        self.require_phase(
            LiveSessionPhase::Finalizing,
            LiveSessionOperation::ObserveFinalizeResult,
        )?;
        self.finalize_result_observed = true;
        Ok(())
    }

    /// Completes finalization and derives a truthful wire outcome from observed session evidence.
    ///
    /// # Errors
    ///
    /// Returns an error unless finalization is active and the requested status agrees with the
    /// session's accepted-audio and observed-result evidence.
    pub fn complete_finalize(
        &mut self,
        status: FinalizeStatus,
    ) -> Result<FinalizeOutcome, LiveSessionError> {
        self.require_phase(
            LiveSessionPhase::Finalizing,
            LiveSessionOperation::CompleteFinalize,
        )?;

        match status {
            FinalizeStatus::Flushed if !self.finalize_result_observed => {
                return Err(LiveSessionError::FlushedWithoutResult);
            }
            FinalizeStatus::NoProvider if self.accepted_audio => {
                return Err(LiveSessionError::NoProviderAfterAudio);
            }
            FinalizeStatus::NoProvider if self.finalize_result_observed => {
                return Err(LiveSessionError::NoProviderAfterResult);
            }
            FinalizeStatus::Flushed | FinalizeStatus::NoProvider | FinalizeStatus::Timeout => {}
        }

        let outcome = FinalizeOutcome {
            status,
            saw_result: self.finalize_result_observed,
        };
        self.phase = LiveSessionPhase::Finalized;
        Ok(outcome)
    }

    /// Closes any non-terminal session without rewriting an existing failure.
    ///
    /// # Errors
    ///
    /// Returns [`LiveSessionError::InvalidTransition`] when the session is already closed or failed.
    pub fn close(&mut self) -> Result<(), LiveSessionError> {
        if matches!(
            self.phase,
            LiveSessionPhase::Closed | LiveSessionPhase::Failed
        ) {
            return Err(self.invalid_transition(LiveSessionOperation::Close));
        }
        self.phase = LiveSessionPhase::Closed;
        Ok(())
    }

    /// Marks any non-terminal session as failed.
    ///
    /// # Errors
    ///
    /// Returns [`LiveSessionError::InvalidTransition`] when the session is already closed or failed.
    pub fn fail(&mut self) -> Result<(), LiveSessionError> {
        if matches!(
            self.phase,
            LiveSessionPhase::Closed | LiveSessionPhase::Failed
        ) {
            return Err(self.invalid_transition(LiveSessionOperation::Fail));
        }
        self.phase = LiveSessionPhase::Failed;
        Ok(())
    }

    fn require_phase(
        &self,
        expected: LiveSessionPhase,
        operation: LiveSessionOperation,
    ) -> Result<(), LiveSessionError> {
        if self.phase == expected {
            return Ok(());
        }
        Err(self.invalid_transition(operation))
    }

    const fn invalid_transition(&self, operation: LiveSessionOperation) -> LiveSessionError {
        LiveSessionError::InvalidTransition {
            phase: self.phase,
            operation,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn streaming_session() -> LiveSession {
        let mut session = LiveSession::new();
        session.mark_ready().unwrap();
        session
    }

    fn acknowledge(session: &mut LiveSession, sequence: RawAudioSequence) {
        session.mark_provider_write_succeeded(sequence).unwrap();
        session.mark_ack_sent(sequence).unwrap();
    }

    #[test]
    fn awaits_ready_before_accepting_audio() {
        let mut session = LiveSession::new();

        assert_eq!(session.phase(), LiveSessionPhase::AwaitingReady);
        assert_eq!(
            session.accept_audio(),
            Err(LiveSessionError::InvalidTransition {
                phase: LiveSessionPhase::AwaitingReady,
                operation: LiveSessionOperation::AcceptAudio,
            })
        );

        session.mark_ready().unwrap();
        assert_eq!(session.phase(), LiveSessionPhase::Streaming);
    }

    #[test]
    fn bounds_pending_audio_and_assigns_positive_sequences() {
        let mut session = streaming_session();

        for expected in 1..=MAX_PENDING_AUDIO_SEQUENCES as u64 {
            assert_eq!(session.accept_audio().unwrap().get(), expected);
        }

        assert_eq!(session.pending_audio_count(), MAX_PENDING_AUDIO_SEQUENCES);
        assert_eq!(
            session.accept_audio(),
            Err(LiveSessionError::PendingAudioLimitReached)
        );
    }

    #[test]
    fn ack_requires_a_successful_provider_write() {
        let mut session = streaming_session();
        let sequence = session.accept_audio().unwrap();

        assert_eq!(
            session.mark_ack_sent(sequence),
            Err(LiveSessionError::ProviderWriteNotConfirmed(sequence))
        );
        assert_eq!(session.pending_audio_count(), 1);

        session.mark_provider_write_succeeded(sequence).unwrap();
        session.mark_ack_sent(sequence).unwrap();
        assert_eq!(session.pending_audio_count(), 0);
    }

    #[test]
    fn acknowledgements_may_complete_out_of_order() {
        let mut session = streaming_session();
        let first = session.accept_audio().unwrap();
        let second = session.accept_audio().unwrap();

        acknowledge(&mut session, second);
        assert_eq!(
            session.pending_audio_sequences().collect::<Vec<_>>(),
            vec![first]
        );
        acknowledge(&mut session, first);
        assert_eq!(session.pending_audio_count(), 0);
    }

    #[test]
    fn finalize_requires_every_frame_to_be_acknowledged() {
        let mut session = streaming_session();
        let sequence = session.accept_audio().unwrap();
        session.mark_provider_write_succeeded(sequence).unwrap();

        assert_eq!(
            session.begin_finalize(),
            Err(LiveSessionError::FinalizeHasPendingAudio)
        );

        session.mark_ack_sent(sequence).unwrap();
        session.begin_finalize().unwrap();
        assert_eq!(session.phase(), LiveSessionPhase::Finalizing);
    }

    #[test]
    fn flushed_requires_an_observed_finalize_result() {
        let mut session = streaming_session();
        session.begin_finalize().unwrap();

        assert_eq!(
            session.complete_finalize(FinalizeStatus::Flushed),
            Err(LiveSessionError::FlushedWithoutResult)
        );

        session.mark_finalize_result_observed().unwrap();
        let outcome = session.complete_finalize(FinalizeStatus::Flushed).unwrap();
        assert_eq!(outcome.status(), FinalizeStatus::Flushed);
        assert!(outcome.saw_result());
        assert_eq!(session.phase(), LiveSessionPhase::Finalized);
    }

    #[test]
    fn no_provider_is_valid_only_without_accepted_audio_or_results() {
        let mut empty = streaming_session();
        empty.begin_finalize().unwrap();
        let outcome = empty.complete_finalize(FinalizeStatus::NoProvider).unwrap();
        assert_eq!(outcome.status(), FinalizeStatus::NoProvider);
        assert!(!outcome.saw_result());

        let mut with_audio = streaming_session();
        let sequence = with_audio.accept_audio().unwrap();
        acknowledge(&mut with_audio, sequence);
        with_audio.begin_finalize().unwrap();
        assert_eq!(
            with_audio.complete_finalize(FinalizeStatus::NoProvider),
            Err(LiveSessionError::NoProviderAfterAudio)
        );

        let mut with_result = streaming_session();
        with_result.begin_finalize().unwrap();
        with_result.mark_finalize_result_observed().unwrap();
        assert_eq!(
            with_result.complete_finalize(FinalizeStatus::NoProvider),
            Err(LiveSessionError::NoProviderAfterResult)
        );
    }

    #[test]
    fn timeout_truthfully_reports_whether_a_result_was_seen() {
        let mut without_result = streaming_session();
        without_result.begin_finalize().unwrap();
        let outcome = without_result
            .complete_finalize(FinalizeStatus::Timeout)
            .unwrap();
        assert_eq!(outcome.status(), FinalizeStatus::Timeout);
        assert!(!outcome.saw_result());

        let mut with_result = streaming_session();
        with_result.begin_finalize().unwrap();
        with_result.mark_finalize_result_observed().unwrap();
        let outcome = with_result
            .complete_finalize(FinalizeStatus::Timeout)
            .unwrap();
        assert!(outcome.saw_result());
    }

    #[test]
    fn close_and_fail_are_terminal_and_do_not_overwrite_each_other() {
        let mut closed = streaming_session();
        closed.close().unwrap();
        assert_eq!(closed.phase(), LiveSessionPhase::Closed);
        assert!(closed.fail().is_err());

        let mut failed = streaming_session();
        failed.fail().unwrap();
        assert_eq!(failed.phase(), LiveSessionPhase::Failed);
        assert!(failed.close().is_err());
        assert!(failed.accept_audio().is_err());
    }
}
