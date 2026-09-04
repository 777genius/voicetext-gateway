use super::*;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::AsyncWrite;
use voicetext_speech::application::ports::{LiveTranscript, LiveTranscriptStability};

struct FailingWriter;

impl AsyncWrite for FailingWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        _context: &mut Context<'_>,
        _buffer: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Poll::Ready(Err(std::io::Error::other("synthetic write failure")))
    }

    fn poll_flush(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        _context: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

fn profile() -> ObservationProfile {
    ObservationProfile {
        contract_version: 2,
        provider: "deepgram".into(),
        model: "nova-3".into(),
        language: "multi".into(),
    }
}

#[test]
fn raw_digest_is_length_delimited_and_not_a_decoded_pcm_digest() {
    let client = Uuid::from_u128(1);
    let gateway = Uuid::from_u128(2);
    let mut split = LiveObservationTracker::new(client, gateway, profile());
    split.accept_frame();
    split.provider_written(1);
    split.ack_sent(1, b"a");
    split.accept_frame();
    split.provider_written(2);
    split.ack_sent(2, b"bc");
    let split = split.finish(None, "flushed".into());
    assert_eq!(
        split.acked_raw_input_digest,
        "3dd01880032c1b1c928a3a65764835558e228c981055516d6cc8a41518294ff2"
    );

    let mut joined = LiveObservationTracker::new(client, gateway, profile());
    joined.accept_frame();
    joined.provider_written(1);
    joined.ack_sent(1, b"abc");
    let joined = joined.finish(None, "flushed".into());
    assert_ne!(split.acked_raw_input_digest, joined.acked_raw_input_digest);

    let mut pcm = Sha256::new();
    pcm.update(FRAME_DIGEST_PREFIX);
    pcm.update(1_u64.to_be_bytes());
    pcm.update(4_u64.to_be_bytes());
    pcm.update([0_u8; 4]);
    assert_ne!(split.acked_raw_input_digest, hex::encode(pcm.finalize()));
}

#[test]
fn failures_are_excluded_and_ranges_report_contiguity() {
    let mut tracker =
        LiveObservationTracker::new(Uuid::from_u128(1), Uuid::from_u128(2), profile());
    tracker.accept_frame(); // failed provider write
    tracker.accept_frame();
    tracker.provider_written(2); // failed ACK
    tracker.accept_frame();
    tracker.provider_written(3);
    tracker.ack_sent(3, b"ok");
    let record = tracker.finish(None, "transport_closed".into());
    assert_eq!(record.accepted_frame_count, 3);
    assert_eq!(record.written_sequences.count, 2);
    assert_eq!(record.acked_sequences.count, 1);
    assert_eq!(record.acked_sequences.first, Some(3));
    assert!(record.acked_sequences.contiguous);
    assert_eq!(record.terminal_status, "transport_closed");
    assert!(!record.finalize_result_observed);
}

#[test]
fn sequence_gaps_remain_noncontiguous_after_later_adjacent_values() {
    let mut tracker =
        LiveObservationTracker::new(Uuid::from_u128(1), Uuid::from_u128(2), profile());
    for sequence in [1, 3, 4] {
        tracker.provider_written(sequence);
    }
    let record = tracker.finish(None, "transport_closed".into());
    assert!(!record.written_sequences.contiguous);
}

#[tokio::test]
async fn sink_is_create_only_bounded_and_rejects_unsafe_custody() {
    let temporary = tempfile::tempdir().unwrap();
    std::fs::set_permissions(temporary.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let sink = FileQualificationSink::new(temporary.path(), "campaign_1").unwrap();
    let effect = Uuid::from_u128(3);
    let mut tracker =
        LiveObservationTracker::new(Uuid::from_u128(1), Uuid::from_u128(2), profile());
    tracker.accept_frame();
    tracker.provider_written(1);
    tracker.ack_sent(1, b"SENSITIVE_AUDIO");
    tracker.provider_event(&LiveRecognitionEvent::Transcript(LiveTranscript {
        text: "SENSITIVE_TRANSCRIPT".into(),
        start_millis: 0,
        duration_millis: 1,
        confidence: None,
        stability: LiveTranscriptStability::UtteranceFinal,
    }));
    let record = tracker.finish(None, "flushed".into());
    let mut fixed = record;
    fixed.effect_id = effect;
    sink.observe_live(fixed.clone()).await.unwrap();
    let path = temporary
        .path()
        .join(format!("campaign_1-live-{effect}.json"));
    let bytes = std::fs::read(&path).unwrap();
    assert!(!bytes.windows(9).any(|value| value == b"SENSITIVE"));
    assert_eq!(
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        sink.observe_live(fixed).await,
        Err(ObservationSinkFailure("QUALIFICATION_CREATE_FAILED"))
    );

    let permissive = tempfile::tempdir().unwrap();
    std::fs::set_permissions(permissive.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(FileQualificationSink::new(permissive.path(), "campaign").is_err());

    let link_parent = tempfile::tempdir().unwrap();
    let link = link_parent.path().join("link");
    symlink(temporary.path(), &link).unwrap();
    assert!(FileQualificationSink::new(&link, "campaign").is_err());
    assert!(FileQualificationSink::new(temporary.path(), "../escape").is_err());
}

#[tokio::test]
async fn storage_failure_is_classified_and_partial_cleanup_removes_files() {
    let temporary = tempfile::tempdir().unwrap();
    let partial = temporary.path().join("partial.json");
    std::fs::write(&partial, b"partial").unwrap();
    let result = write_created_bytes(&mut FailingWriter, &partial, b"record").await;
    assert_eq!(
        result,
        Err(ObservationSinkFailure("QUALIFICATION_WRITE_FAILED"))
    );
    assert!(!partial.exists());
}
