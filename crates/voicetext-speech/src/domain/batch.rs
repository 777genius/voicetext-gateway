//! Deterministic lifecycle for one provider-bound batch transcription job.

use std::fmt;
use std::num::NonZeroU32;

/// Immutable provider/model identity selected before a batch job is accepted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BatchProfile {
    contract_version: u16,
    provider: Box<str>,
    model: Box<str>,
    language: Box<str>,
}

impl BatchProfile {
    /// Creates a profile without assigning provider-specific meaning to its fields.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidBatchProfile`] when the version or identity fields are invalid.
    pub fn new(
        contract_version: u16,
        provider: impl Into<String>,
        model: impl Into<String>,
        language: impl Into<String>,
    ) -> Result<Self, InvalidBatchProfile> {
        if contract_version == 0 {
            return Err(InvalidBatchProfile::ZeroContractVersion);
        }

        let provider = provider.into();
        validate_profile_field(&provider, BatchProfileField::Provider)?;
        let model = model.into();
        validate_profile_field(&model, BatchProfileField::Model)?;
        let language = language.into();
        validate_profile_field(&language, BatchProfileField::Language)?;

        Ok(Self {
            contract_version,
            provider: provider.into_boxed_str(),
            model: model.into_boxed_str(),
            language: language.into_boxed_str(),
        })
    }

    /// Wire-contract version bound to the job.
    pub const fn contract_version(&self) -> u16 {
        self.contract_version
    }

    /// Provider identity bound to the job.
    pub fn provider(&self) -> &str {
        &self.provider
    }

    /// Model identity bound to the job.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Language identity bound to the job.
    pub fn language(&self) -> &str {
        &self.language
    }
}

fn validate_profile_field(
    value: &str,
    field: BatchProfileField,
) -> Result<(), InvalidBatchProfile> {
    if value.is_empty() || value.trim() != value || value.chars().any(char::is_control) {
        return Err(InvalidBatchProfile::InvalidIdentity { field });
    }
    Ok(())
}

/// Profile field that failed provider-neutral identity validation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BatchProfileField {
    /// Provider identity.
    Provider,
    /// Model identity.
    Model,
    /// Language identity.
    Language,
}

/// Invalid immutable profile supplied at admission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvalidBatchProfile {
    /// Contract version zero is reserved and cannot identify a contract.
    ZeroContractVersion,
    /// An identity was empty, padded with whitespace, or contained control characters.
    InvalidIdentity { field: BatchProfileField },
}

impl fmt::Display for InvalidBatchProfile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroContractVersion => formatter.write_str("contract version must be non-zero"),
            Self::InvalidIdentity { field } => {
                write!(formatter, "invalid batch profile {field:?} identity")
            }
        }
    }
}

impl std::error::Error for InvalidBatchProfile {}

/// SHA-256 request fingerprint bound to an accepted job.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BatchRequestFingerprint([u8; 32]);

impl BatchRequestFingerprint {
    /// Wraps fingerprint bytes computed by the application boundary.
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Returns the immutable fingerprint bytes.
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Stable application failure code without provider response details.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BatchFailure {
    code: Box<str>,
}

impl BatchFailure {
    /// Creates a bounded stable code suitable for durable job state.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidBatchFailure`] when the code is ambiguous or exceeds its bound.
    pub fn new(code: impl Into<String>) -> Result<Self, InvalidBatchFailure> {
        const MAX_CODE_BYTES: usize = 128;

        let code = code.into();
        if code.is_empty() || code.trim() != code {
            return Err(InvalidBatchFailure::InvalidCode);
        }
        if code.len() > MAX_CODE_BYTES {
            return Err(InvalidBatchFailure::CodeTooLong);
        }
        if code.chars().any(char::is_control) {
            return Err(InvalidBatchFailure::InvalidCode);
        }
        Ok(Self {
            code: code.into_boxed_str(),
        })
    }

    /// Returns the stable application code.
    pub fn code(&self) -> &str {
        &self.code
    }
}

/// Invalid failure evidence supplied to a transition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvalidBatchFailure {
    /// Code was empty, whitespace-padded, or contained control characters.
    InvalidCode,
    /// Code exceeded the durable domain bound.
    CodeTooLong,
}

impl fmt::Display for InvalidBatchFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCode => formatter.write_str("invalid batch failure code"),
            Self::CodeTooLong => formatter.write_str("batch failure code is too long"),
        }
    }
}

impl std::error::Error for InvalidBatchFailure {}

/// Why provider egress can no longer be classified as accepted or rejected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BatchUnknownOutcome {
    /// The submission returned an application-classified unknown outcome.
    Submission(BatchFailure),
    /// The process stopped while provider submission was in progress.
    InterruptedSubmission,
}

/// Durable lifecycle state for a batch job.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BatchJobState {
    /// Request is durable but no provider submission has begun.
    Accepted,
    /// Exactly one numbered provider submission may currently be in flight.
    Submitting { attempt: NonZeroU32 },
    /// The last attempt is known not to have been accepted and may be retried.
    Retryable {
        attempt: NonZeroU32,
        failure: BatchFailure,
    },
    /// A bounded provider-neutral result was durably committed.
    Completed { attempt: NonZeroU32 },
    /// The provider outcome is known and terminal, so no further submission is safe or useful.
    Failed {
        attempt: NonZeroU32,
        failure: BatchFailure,
    },
    /// Provider acceptance is uncertain; paid submission must never be retried.
    OutcomeUnknown {
        attempt: NonZeroU32,
        reason: BatchUnknownOutcome,
    },
}

impl BatchJobState {
    /// Returns the compact status without discarding state-specific evidence.
    pub const fn status(&self) -> BatchJobStatus {
        match self {
            Self::Accepted => BatchJobStatus::Accepted,
            Self::Submitting { .. } => BatchJobStatus::Submitting,
            Self::Retryable { .. } => BatchJobStatus::Retryable,
            Self::Completed { .. } => BatchJobStatus::Completed,
            Self::Failed { .. } => BatchJobStatus::Failed,
            Self::OutcomeUnknown { .. } => BatchJobStatus::OutcomeUnknown,
        }
    }

    /// Returns whether no legal transition can initiate another provider submission.
    pub const fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed { .. } | Self::Failed { .. } | Self::OutcomeUnknown { .. }
        )
    }
}

/// Compact job status used in transition diagnostics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BatchJobStatus {
    Accepted,
    Submitting,
    Retryable,
    Completed,
    Failed,
    OutcomeUnknown,
}

/// A provider-bound batch job with immutable admission identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BatchJob {
    profile: BatchProfile,
    fingerprint: BatchRequestFingerprint,
    state: BatchJobState,
}

impl BatchJob {
    /// Accepts a new job without initiating provider egress.
    pub const fn accept(profile: BatchProfile, fingerprint: BatchRequestFingerprint) -> Self {
        Self {
            profile,
            fingerprint,
            state: BatchJobState::Accepted,
        }
    }

    /// Restores already-validated durable state without changing its identity.
    pub const fn restore(
        profile: BatchProfile,
        fingerprint: BatchRequestFingerprint,
        state: BatchJobState,
    ) -> Self {
        Self {
            profile,
            fingerprint,
            state,
        }
    }

    /// Immutable profile selected at admission.
    pub const fn profile(&self) -> &BatchProfile {
        &self.profile
    }

    /// Immutable complete-request fingerprint selected at admission.
    pub const fn fingerprint(&self) -> BatchRequestFingerprint {
        self.fingerprint
    }

    /// Current durable lifecycle state.
    pub const fn state(&self) -> &BatchJobState {
        &self.state
    }

    /// Begins the first submission or a retry proven safe by the preceding state.
    ///
    /// # Errors
    ///
    /// Returns [`BatchTransitionError`] when submission is not legal or attempts overflow.
    pub fn begin_submission(&mut self) -> Result<NonZeroU32, BatchTransitionError> {
        let attempt = match &self.state {
            BatchJobState::Accepted => NonZeroU32::MIN,
            BatchJobState::Retryable { attempt, .. } => attempt
                .checked_add(1)
                .ok_or(BatchTransitionError::AttemptOverflow { attempt: *attempt })?,
            state => {
                return Err(BatchTransitionError::InvalidTransition {
                    from: state.status(),
                    to: BatchJobStatus::Submitting,
                });
            }
        };
        self.state = BatchJobState::Submitting { attempt };
        Ok(attempt)
    }

    /// Records a known-not-accepted failure for which another attempt is safe.
    ///
    /// # Errors
    ///
    /// Returns [`BatchTransitionError`] unless the job is currently submitting.
    pub fn record_retryable_failure(
        &mut self,
        failure: BatchFailure,
    ) -> Result<(), BatchTransitionError> {
        let attempt = self.submission_attempt(BatchJobStatus::Retryable)?;
        self.state = BatchJobState::Retryable { attempt, failure };
        Ok(())
    }

    /// Records successful durable completion of the current submission.
    ///
    /// # Errors
    ///
    /// Returns [`BatchTransitionError`] unless the job is currently submitting.
    pub fn complete(&mut self) -> Result<(), BatchTransitionError> {
        let attempt = self.submission_attempt(BatchJobStatus::Completed)?;
        self.state = BatchJobState::Completed { attempt };
        Ok(())
    }

    /// Records a known-not-accepted terminal failure.
    ///
    /// # Errors
    ///
    /// Returns [`BatchTransitionError`] unless the job is currently submitting.
    pub fn fail(&mut self, failure: BatchFailure) -> Result<(), BatchTransitionError> {
        let attempt = self.submission_attempt(BatchJobStatus::Failed)?;
        self.state = BatchJobState::Failed { attempt, failure };
        Ok(())
    }

    /// Records an uncertain outcome after provider egress.
    ///
    /// # Errors
    ///
    /// Returns [`BatchTransitionError`] unless the job is currently submitting.
    pub fn record_unknown_outcome(
        &mut self,
        failure: BatchFailure,
    ) -> Result<(), BatchTransitionError> {
        let attempt = self.submission_attempt(BatchJobStatus::OutcomeUnknown)?;
        self.state = BatchJobState::OutcomeUnknown {
            attempt,
            reason: BatchUnknownOutcome::Submission(failure),
        };
        Ok(())
    }

    /// Converts an in-flight submission found during recovery into terminal uncertainty.
    ///
    /// Returns `true` only when a transition occurred. Repeated recovery is idempotent.
    pub fn recover_interrupted_submission(&mut self) -> bool {
        let BatchJobState::Submitting { attempt } = self.state else {
            return false;
        };
        self.state = BatchJobState::OutcomeUnknown {
            attempt,
            reason: BatchUnknownOutcome::InterruptedSubmission,
        };
        true
    }

    fn submission_attempt(&self, to: BatchJobStatus) -> Result<NonZeroU32, BatchTransitionError> {
        match self.state {
            BatchJobState::Submitting { attempt } => Ok(attempt),
            ref state => Err(BatchTransitionError::InvalidTransition {
                from: state.status(),
                to,
            }),
        }
    }
}

/// Rejected deterministic state transition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BatchTransitionError {
    /// The current state has no edge to the requested state.
    InvalidTransition {
        from: BatchJobStatus,
        to: BatchJobStatus,
    },
    /// The durable attempt counter cannot advance without wrapping.
    AttemptOverflow { attempt: NonZeroU32 },
}

impl fmt::Display for BatchTransitionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidTransition { from, to } => {
                write!(
                    formatter,
                    "batch transition from {from:?} to {to:?} is not allowed"
                )
            }
            Self::AttemptOverflow { attempt } => {
                write!(
                    formatter,
                    "batch submission attempt {attempt} cannot advance"
                )
            }
        }
    }
}

impl std::error::Error for BatchTransitionError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile() -> BatchProfile {
        BatchProfile::new(2, "provider-a", "model-a", "multi").unwrap()
    }

    fn fingerprint() -> BatchRequestFingerprint {
        BatchRequestFingerprint::from_bytes([7; 32])
    }

    fn failure(code: &str) -> BatchFailure {
        BatchFailure::new(code).unwrap()
    }

    #[test]
    fn admission_identity_remains_immutable_across_transitions() {
        let expected_profile = profile();
        let expected_fingerprint = fingerprint();
        let mut job = BatchJob::accept(expected_profile.clone(), expected_fingerprint);

        job.begin_submission().unwrap();
        job.record_retryable_failure(failure("CAPACITY_BUSY"))
            .unwrap();
        job.begin_submission().unwrap();
        job.complete().unwrap();

        assert_eq!(job.profile(), &expected_profile);
        assert_eq!(job.fingerprint(), expected_fingerprint);
    }

    #[test]
    fn retryable_submission_advances_attempt_without_changing_evidence() {
        let mut job = BatchJob::accept(profile(), fingerprint());
        assert_eq!(job.begin_submission().unwrap().get(), 1);
        job.record_retryable_failure(failure("KNOWN_NOT_ACCEPTED"))
            .unwrap();
        assert!(matches!(
            job.state(),
            BatchJobState::Retryable { attempt, failure }
                if attempt.get() == 1 && failure.code() == "KNOWN_NOT_ACCEPTED"
        ));

        assert_eq!(job.begin_submission().unwrap().get(), 2);
        assert_eq!(
            job.state(),
            &BatchJobState::Submitting {
                attempt: NonZeroU32::new(2).unwrap()
            }
        );
    }

    #[test]
    fn completed_and_failed_states_are_terminal() {
        let mut completed = BatchJob::accept(profile(), fingerprint());
        completed.begin_submission().unwrap();
        completed.complete().unwrap();
        assert!(completed.state().is_terminal());
        assert!(completed.begin_submission().is_err());

        let mut failed = BatchJob::accept(profile(), fingerprint());
        failed.begin_submission().unwrap();
        failed.fail(failure("INVALID_AUDIO")).unwrap();
        assert!(failed.state().is_terminal());
        assert!(failed.begin_submission().is_err());
    }

    #[test]
    fn unknown_outcome_is_terminal_and_cannot_retry() {
        let mut job = BatchJob::accept(profile(), fingerprint());
        job.begin_submission().unwrap();
        job.record_unknown_outcome(failure("UPSTREAM_UNCERTAIN"))
            .unwrap();

        assert!(matches!(
            job.state(),
            BatchJobState::OutcomeUnknown {
                attempt,
                reason: BatchUnknownOutcome::Submission(failure)
            } if attempt.get() == 1 && failure.code() == "UPSTREAM_UNCERTAIN"
        ));
        assert_eq!(
            job.begin_submission(),
            Err(BatchTransitionError::InvalidTransition {
                from: BatchJobStatus::OutcomeUnknown,
                to: BatchJobStatus::Submitting,
            })
        );
    }

    #[test]
    fn interrupted_submission_recovers_to_unknown_exactly_once() {
        let mut job = BatchJob::accept(profile(), fingerprint());
        job.begin_submission().unwrap();

        assert!(job.recover_interrupted_submission());
        assert!(!job.recover_interrupted_submission());
        assert_eq!(
            job.state(),
            &BatchJobState::OutcomeUnknown {
                attempt: NonZeroU32::MIN,
                reason: BatchUnknownOutcome::InterruptedSubmission,
            }
        );
        assert!(job.begin_submission().is_err());
    }

    #[test]
    fn only_submitting_can_record_an_attempt_outcome() {
        let original = BatchJob::accept(profile(), fingerprint());
        let mut job = original.clone();

        assert!(job.record_retryable_failure(failure("TRY_LATER")).is_err());
        assert!(job.complete().is_err());
        assert!(job.fail(failure("BAD_REQUEST")).is_err());
        assert!(job.record_unknown_outcome(failure("UNKNOWN")).is_err());
        assert_eq!(job, original);
    }

    #[test]
    fn attempt_counter_never_wraps() {
        let state = BatchJobState::Retryable {
            attempt: NonZeroU32::MAX,
            failure: failure("KNOWN_NOT_ACCEPTED"),
        };
        let mut job = BatchJob::restore(profile(), fingerprint(), state.clone());

        assert_eq!(
            job.begin_submission(),
            Err(BatchTransitionError::AttemptOverflow {
                attempt: NonZeroU32::MAX,
            })
        );
        assert_eq!(job.state(), &state);
    }

    #[test]
    fn identities_and_failure_codes_reject_ambiguous_values() {
        assert_eq!(
            BatchProfile::new(0, "provider", "model", "multi"),
            Err(InvalidBatchProfile::ZeroContractVersion)
        );
        assert_eq!(
            BatchProfile::new(2, " provider", "model", "multi"),
            Err(InvalidBatchProfile::InvalidIdentity {
                field: BatchProfileField::Provider,
            })
        );
        assert_eq!(
            BatchFailure::new(" "),
            Err(InvalidBatchFailure::InvalidCode)
        );
        assert_eq!(
            BatchFailure::new("x".repeat(129)),
            Err(InvalidBatchFailure::CodeTooLong)
        );
    }
}
