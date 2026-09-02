use ogg::{PacketWriteEndInfo, PacketWriter};

use super::*;

const SERIAL: u32 = 0x51_54_01;
const SILENCE: [u8; 3] = [0xf8, 0xff, 0xfe];

fn identification_header(pre_skip: u16) -> Vec<u8> {
    let mut packet = Vec::from(*OPUS_HEAD_MAGIC);
    packet.extend_from_slice(&[1, 1]);
    packet.extend_from_slice(&pre_skip.to_le_bytes());
    packet.extend_from_slice(&SAMPLE_RATE_HZ.to_le_bytes());
    packet.extend_from_slice(&0i16.to_le_bytes());
    packet.push(0);
    packet
}

fn comment_header() -> Vec<u8> {
    let mut packet = Vec::from(*OPUS_TAGS_MAGIC);
    packet.extend_from_slice(&0u32.to_le_bytes());
    packet.extend_from_slice(&0u32.to_le_bytes());
    packet
}

fn fixture_with_headers(
    serial: u32,
    head: Vec<u8>,
    tags: Vec<u8>,
    final_granule: u64,
    end_stream: bool,
) -> Vec<u8> {
    let mut writer = PacketWriter::new(Vec::new());
    writer
        .write_packet(head, serial, PacketWriteEndInfo::EndPage, 0)
        .unwrap();
    writer
        .write_packet(tags, serial, PacketWriteEndInfo::EndPage, 0)
        .unwrap();
    writer
        .write_packet(
            Vec::from(SILENCE),
            serial,
            if end_stream {
                PacketWriteEndInfo::EndStream
            } else {
                PacketWriteEndInfo::EndPage
            },
            final_granule,
        )
        .unwrap();
    writer.into_inner()
}

fn fixture(serial: u32, final_granule: u64, end_stream: bool) -> Vec<u8> {
    fixture_with_headers(
        serial,
        identification_header(312),
        comment_header(),
        final_granule,
        end_stream,
    )
}

#[test]
fn validates_complete_mono_stream_and_rounds_duration_up() {
    let validated = validate_complete_ogg_opus(&fixture(SERIAL, 960, true)).unwrap();

    assert_eq!(validated.pre_skip_samples, 312);
    assert_eq!(validated.duration_samples, 648);
    assert_eq!(validated.duration_millis, 14);
    assert_eq!(validated.audio_packet_count, 1);
}

#[test]
fn rejects_malformed_and_truncated_containers() {
    assert_eq!(
        validate_complete_ogg_opus(b"not ogg"),
        Err(OggOpusValidationError::InvalidContainer)
    );
    let mut truncated = fixture(SERIAL, 960, true);
    truncated.pop();
    assert_eq!(
        validate_complete_ogg_opus(&truncated),
        Err(OggOpusValidationError::InvalidContainer)
    );
}

#[test]
fn rejects_checksum_corruption() {
    let mut corrupted = fixture(SERIAL, 960, true);
    let last = corrupted.last_mut().unwrap();
    *last ^= 0x01;

    assert_eq!(
        validate_complete_ogg_opus(&corrupted),
        Err(OggOpusValidationError::InvalidContainer)
    );
}

#[test]
fn validates_complete_stereo_mapping_family_zero() {
    let mut stereo = identification_header(312);
    stereo[9] = 2;

    let validated = validate_complete_ogg_opus(&fixture_with_headers(
        SERIAL,
        stereo,
        comment_header(),
        960,
        true,
    ))
    .unwrap();

    assert_eq!(validated.duration_samples, 648);
    assert_eq!(validated.audio_packet_count, 1);
}

#[test]
fn rejects_unsupported_head_and_malformed_tags() {
    let mut unsupported_channels = identification_header(312);
    unsupported_channels[9] = 3;
    assert_eq!(
        validate_complete_ogg_opus(&fixture_with_headers(
            SERIAL,
            unsupported_channels,
            comment_header(),
            960,
            true,
        )),
        Err(OggOpusValidationError::InvalidIdentificationHeader)
    );
    let mut unsupported_mapping = identification_header(312);
    unsupported_mapping[18] = 1;
    assert_eq!(
        validate_complete_ogg_opus(&fixture_with_headers(
            SERIAL,
            unsupported_mapping,
            comment_header(),
            960,
            true,
        )),
        Err(OggOpusValidationError::InvalidIdentificationHeader)
    );
    assert_eq!(
        validate_complete_ogg_opus(&fixture_with_headers(
            SERIAL,
            identification_header(312),
            Vec::from(*OPUS_TAGS_MAGIC),
            960,
            true,
        )),
        Err(OggOpusValidationError::InvalidCommentHeader)
    );
}

#[test]
fn rejects_chained_stream() {
    let mut chained = fixture(SERIAL, 960, true);
    chained.extend_from_slice(&fixture(SERIAL + 1, 960, true));

    assert_eq!(
        validate_complete_ogg_opus(&chained),
        Err(OggOpusValidationError::InvalidLogicalStream)
    );
}

#[test]
fn rejects_missing_eos() {
    assert_eq!(
        validate_complete_ogg_opus(&fixture(SERIAL, 960, false)),
        Err(OggOpusValidationError::MissingEndOfStream)
    );
}

#[test]
fn rejects_non_positive_and_impossible_final_granules() {
    assert_eq!(
        validate_complete_ogg_opus(&fixture(SERIAL, 312, true)),
        Err(OggOpusValidationError::InvalidGranulePosition)
    );
    assert_eq!(
        validate_complete_ogg_opus(&fixture(SERIAL, 961, true)),
        Err(OggOpusValidationError::InvalidGranulePosition)
    );
}

#[test]
fn rejects_trailing_bytes() {
    let mut trailing = fixture(SERIAL, 960, true);
    trailing.push(0);

    assert_eq!(
        validate_complete_ogg_opus(&trailing),
        Err(OggOpusValidationError::InvalidLogicalStream)
    );
}
