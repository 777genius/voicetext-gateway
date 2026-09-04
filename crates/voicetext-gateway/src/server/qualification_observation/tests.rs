use super::*;
use std::os::unix::fs::{PermissionsExt, symlink};
use voicetext_audio::discord_opus::DiscordOpusDecoder;
use voicetext_speech::application::ports::{BatchSegment, LiveTranscript, LiveTranscriptStability};
use voicetext_speech::domain::batch::BatchProfile;

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
}

#[test]
fn raw_opus_digest_is_pinned_and_differs_from_its_decoded_pcm() {
    let raw_opus = [0xf8, 0xff, 0xfe];
    let pcm = DiscordOpusDecoder::new()
        .unwrap()
        .decode(&raw_opus)
        .unwrap()
        .pcm_s16le;
    assert_ne!(raw_opus.as_slice(), pcm.as_slice());

    let mut raw = LiveObservationTracker::new(Uuid::from_u128(1), Uuid::from_u128(2), profile());
    raw.ack_sent(7, &raw_opus);
    let raw = raw.finish(None, "flushed".into());
    assert_eq!(
        raw.acked_raw_input_digest,
        "2b4ecbe64e033098006635d87ae235953186114ea396f9886f2f8d3b463fdedd"
    );

    let mut decoded =
        LiveObservationTracker::new(Uuid::from_u128(1), Uuid::from_u128(2), profile());
    decoded.ack_sent(7, &pcm);
    let decoded = decoded.finish(None, "flushed".into());
    assert_ne!(raw.acked_raw_input_digest, decoded.acked_raw_input_digest);
}

#[test]
fn normalized_batch_and_live_digest_vectors_are_pinned() {
    let batch = BatchRecognitionResult {
        profile: BatchProfile::new(3, "elevenlabs", "scribe_v2", "multi").unwrap(),
        text: "hello Ω".into(),
        duration_millis: 1_234,
        provider_duration_millis: None,
        segments: vec![BatchSegment {
            start_millis: 12,
            end_millis: 345,
            text: "hello".into(),
            confidence: Some(0.75),
            speaker: Some("speaker-1".into()),
        }],
        readable_segments: None,
        provider_reference: None,
    };
    assert_eq!(
        batch_result_digest(&batch),
        "4a727a16f05905bd115d667de9ec608d1f87ce8821372f1d318effa530d8953e"
    );

    let mut live = LiveObservationTracker::new(Uuid::from_u128(1), Uuid::from_u128(2), profile());
    live.provider_event(&LiveRecognitionEvent::Transcript(LiveTranscript {
        text: "hello Ω".into(),
        start_millis: 12,
        duration_millis: 333,
        confidence: Some(0.75),
        stability: LiveTranscriptStability::SegmentFinal,
    }));
    live.provider_event(&LiveRecognitionEvent::UtteranceEnd {
        last_word_end_millis: 345,
    });
    live.provider_event(&LiveRecognitionEvent::FinalizeResultObserved);
    let live = live.finish(None, "flushed".into());
    assert_eq!(
        live.result_digest,
        "eec9a3372009a5d6f595cdb1d84071714ca7704a552f90988a91afe76435bdde"
    );
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
    assert!(record.provider_operation.is_none());
    assert_eq!(
        record.acked_raw_input_digest,
        "d2729c9799013e3c1d1209b22ec80d8cca15a9a2d57772da700b2ecccfed3048"
    );
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
