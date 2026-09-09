//! Deterministic validation of live timing against successfully written audio.

use crate::application::ports::{LiveTranscript, LiveTranscriptStability};

/// Maximum normalized timestamp lead over successfully provider-written audio.
///
/// Providers may report a word boundary slightly ahead of the PCM write horizon because their
/// segmentation operates in bounded windows. The application accepts at most 250 milliseconds of
/// lead and rejects larger values; it never clamps provider evidence.
pub const PROVIDER_LEAD_TOLERANCE_MILLIS: u64 = 250;

const PCM_S16LE_BYTES_PER_SAMPLE: u64 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AcceptedAudioError {
    InvalidConfiguration,
    EmptyFrame,
    MisalignedFrame,
    ArithmeticOverflow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LiveTimelineError {
    TimingOverflow,
    BeyondAcceptedAudio,
    FinalRegressionOrOverlap,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PendingAudioAcceptance {
    total_sample_frames: u64,
    horizon_millis: u64,
}

/// Provider-neutral timing policy for one live recognition session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct LiveTimeline {
    sample_rate_hz: u64,
    bytes_per_sample_frame: u64,
    accepted_sample_frames: u64,
    accepted_horizon_millis: u64,
    last_final_end_millis: Option<u64>,
}

impl LiveTimeline {
    pub(super) fn new(sample_rate_hz: u32, channels: u8) -> Result<Self, AcceptedAudioError> {
        let sample_rate_hz = u64::from(sample_rate_hz);
        let channels = u64::from(channels);
        if sample_rate_hz == 0 || channels == 0 {
            return Err(AcceptedAudioError::InvalidConfiguration);
        }
        let bytes_per_sample_frame = channels
            .checked_mul(PCM_S16LE_BYTES_PER_SAMPLE)
            .ok_or(AcceptedAudioError::ArithmeticOverflow)?;
        Ok(Self {
            sample_rate_hz,
            bytes_per_sample_frame,
            accepted_sample_frames: 0,
            accepted_horizon_millis: 0,
            last_final_end_millis: None,
        })
    }

    /// Validates a PCM write and computes its prospective horizon without accepting it.
    pub(super) fn prepare_audio_write(
        &self,
        byte_len: usize,
    ) -> Result<PendingAudioAcceptance, AcceptedAudioError> {
        if byte_len == 0 {
            return Err(AcceptedAudioError::EmptyFrame);
        }
        let byte_len =
            u64::try_from(byte_len).map_err(|_| AcceptedAudioError::ArithmeticOverflow)?;
        if byte_len % self.bytes_per_sample_frame != 0 {
            return Err(AcceptedAudioError::MisalignedFrame);
        }
        let added_sample_frames = byte_len / self.bytes_per_sample_frame;
        let total_sample_frames = self
            .accepted_sample_frames
            .checked_add(added_sample_frames)
            .ok_or(AcceptedAudioError::ArithmeticOverflow)?;
        let horizon_millis = checked_horizon_millis(total_sample_frames, self.sample_rate_hz)?;
        Ok(PendingAudioAcceptance {
            total_sample_frames,
            horizon_millis,
        })
    }

    /// Advances the horizon only after the corresponding provider write succeeds.
    pub(super) fn commit_audio_write(&mut self, acceptance: PendingAudioAcceptance) {
        debug_assert!(acceptance.total_sample_frames >= self.accepted_sample_frames);
        debug_assert!(acceptance.horizon_millis >= self.accepted_horizon_millis);
        self.accepted_sample_frames = acceptance.total_sample_frames;
        self.accepted_horizon_millis = acceptance.horizon_millis;
    }

    pub(super) fn validate_transcript(
        &mut self,
        transcript: &LiveTranscript,
    ) -> Result<(), LiveTimelineError> {
        let end_millis = transcript
            .start_millis
            .checked_add(transcript.duration_millis)
            .ok_or(LiveTimelineError::TimingOverflow)?;
        self.validate_horizon(end_millis)?;

        if transcript.stability != LiveTranscriptStability::Partial {
            if self
                .last_final_end_millis
                .is_some_and(|previous_end| transcript.start_millis < previous_end)
            {
                return Err(LiveTimelineError::FinalRegressionOrOverlap);
            }
            self.last_final_end_millis = Some(end_millis);
        }
        Ok(())
    }

    pub(super) fn validate_utterance_end(
        &self,
        last_word_end_millis: u64,
    ) -> Result<(), LiveTimelineError> {
        self.validate_horizon(last_word_end_millis)
    }

    fn validate_horizon(&self, end_millis: u64) -> Result<(), LiveTimelineError> {
        if end_millis.saturating_sub(self.accepted_horizon_millis) > PROVIDER_LEAD_TOLERANCE_MILLIS
        {
            return Err(LiveTimelineError::BeyondAcceptedAudio);
        }
        Ok(())
    }
}

fn checked_horizon_millis(
    total_sample_frames: u64,
    sample_rate_hz: u64,
) -> Result<u64, AcceptedAudioError> {
    total_sample_frames
        .checked_mul(1_000)
        .ok_or(AcceptedAudioError::ArithmeticOverflow)?
        .checked_div(sample_rate_hz)
        .ok_or(AcceptedAudioError::InvalidConfiguration)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::batch_capabilities::TimestampProvenance;

    fn transcript(
        start_millis: u64,
        duration_millis: u64,
        stability: LiveTranscriptStability,
    ) -> LiveTranscript {
        LiveTranscript {
            text: "evidence".into(),
            start_millis,
            duration_millis,
            confidence: None,
            stability,
        }
    }

    fn accept_millis(timeline: &mut LiveTimeline, millis: usize) {
        let bytes = millis * 48_000 * 2 / 1_000;
        let pending = timeline.prepare_audio_write(bytes).unwrap();
        timeline.commit_audio_write(pending);
    }

    #[test]
    fn validates_zero_odd_and_channel_misaligned_frames() {
        assert_eq!(
            LiveTimeline::new(0, 1),
            Err(AcceptedAudioError::InvalidConfiguration)
        );
        assert_eq!(
            LiveTimeline::new(48_000, 0),
            Err(AcceptedAudioError::InvalidConfiguration)
        );
        let mono = LiveTimeline::new(48_000, 1).unwrap();
        assert_eq!(
            mono.prepare_audio_write(0),
            Err(AcceptedAudioError::EmptyFrame)
        );
        assert_eq!(
            mono.prepare_audio_write(1),
            Err(AcceptedAudioError::MisalignedFrame)
        );
        let stereo = LiveTimeline::new(48_000, 2).unwrap();
        assert_eq!(
            stereo.prepare_audio_write(2),
            Err(AcceptedAudioError::MisalignedFrame)
        );
    }

    #[test]
    fn horizon_uses_checked_arithmetic_and_exact_sample_frames() {
        let mut timeline = LiveTimeline::new(48_000, 2).unwrap();
        let pending = timeline.prepare_audio_write(192_000).unwrap();
        assert_eq!(pending.horizon_millis, 1_000);
        timeline.commit_audio_write(pending);
        assert_eq!(timeline.accepted_horizon_millis, 1_000);
        assert_eq!(
            checked_horizon_millis(u64::MAX, 1),
            Err(AcceptedAudioError::ArithmeticOverflow)
        );

        timeline.accepted_sample_frames = u64::MAX;
        assert_eq!(
            timeline.prepare_audio_write(4),
            Err(AcceptedAudioError::ArithmeticOverflow)
        );
    }

    #[test]
    fn prepared_audio_does_not_advance_horizon_until_provider_success_is_committed() {
        let mut timeline = LiveTimeline::new(48_000, 1).unwrap();
        let pending = timeline.prepare_audio_write(96_000).unwrap();
        assert_eq!(
            timeline.validate_utterance_end(PROVIDER_LEAD_TOLERANCE_MILLIS + 1),
            Err(LiveTimelineError::BeyondAcceptedAudio)
        );
        timeline.commit_audio_write(pending);
        assert_eq!(timeline.validate_utterance_end(1_000), Ok(()));
    }

    #[test]
    fn accepts_tolerance_boundary_and_rejects_one_millisecond_beyond_it() {
        let mut timeline = LiveTimeline::new(48_000, 1).unwrap();
        accept_millis(&mut timeline, 1_000);
        assert_eq!(
            timeline.validate_transcript(&transcript(
                1_000,
                PROVIDER_LEAD_TOLERANCE_MILLIS,
                LiveTranscriptStability::Partial,
            )),
            Ok(())
        );
        assert_eq!(
            timeline.validate_transcript(&transcript(
                1_000,
                PROVIDER_LEAD_TOLERANCE_MILLIS + 1,
                LiveTranscriptStability::Partial,
            )),
            Err(LiveTimelineError::BeyondAcceptedAudio)
        );
    }

    #[test]
    fn rejects_future_partial_final_and_utterance_end() {
        let mut timeline = LiveTimeline::new(48_000, 1).unwrap();
        accept_millis(&mut timeline, 1_000);
        for stability in [
            LiveTranscriptStability::Partial,
            LiveTranscriptStability::SegmentFinal,
            LiveTranscriptStability::UtteranceFinal,
        ] {
            assert_eq!(
                timeline.validate_transcript(&transcript(0, 1_251, stability)),
                Err(LiveTimelineError::BeyondAcceptedAudio)
            );
        }
        assert_eq!(
            timeline.validate_utterance_end(1_251),
            Err(LiveTimelineError::BeyondAcceptedAudio)
        );
    }

    #[test]
    fn finals_are_non_overlapping_while_partial_revisions_remain_valid() {
        let mut timeline = LiveTimeline::new(48_000, 1).unwrap();
        accept_millis(&mut timeline, 2_000);
        timeline
            .validate_transcript(&transcript(500, 500, LiveTranscriptStability::SegmentFinal))
            .unwrap();
        assert_eq!(
            timeline.validate_transcript(&transcript(
                900,
                200,
                LiveTranscriptStability::UtteranceFinal,
            )),
            Err(LiveTimelineError::FinalRegressionOrOverlap)
        );
        assert_eq!(
            timeline.validate_transcript(&transcript(100, 800, LiveTranscriptStability::Partial,)),
            Ok(())
        );
        assert_eq!(
            timeline.validate_transcript(&transcript(
                1_000,
                250,
                LiveTranscriptStability::UtteranceFinal,
            )),
            Ok(())
        );
    }

    #[test]
    fn policy_is_identical_for_both_timestamp_provenance_modes() {
        for _provenance in [
            TimestampProvenance::ProviderNative,
            TimestampProvenance::GatewaySynthesizedFromAcceptedAudio,
        ] {
            let mut timeline = LiveTimeline::new(16_000, 1).unwrap();
            let pending = timeline.prepare_audio_write(32_000).unwrap();
            timeline.commit_audio_write(pending);
            assert!(
                timeline
                    .validate_transcript(&transcript(
                        0,
                        1_000,
                        LiveTranscriptStability::UtteranceFinal,
                    ))
                    .is_ok()
            );
        }
    }
}
