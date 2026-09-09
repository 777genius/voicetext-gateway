//! Strict validation of complete single-stream Ogg Opus batch uploads.

use std::io::Cursor;

use ogg::{Packet, PacketReader, PageParsingOptions};
use thiserror::Error;

use crate::discord_opus::{MAX_PACKET_BYTES, MAX_SAMPLES_PER_PACKET, SAMPLE_RATE_HZ};

/// Maximum accepted physical Ogg upload size (64 MiB).
pub const MAX_OGG_OPUS_BYTES: usize = 64 * 1024 * 1024;

/// Maximum accepted Opus comment-header packet size (1 MiB).
pub const MAX_OPUS_TAGS_BYTES: usize = 1024 * 1024;

/// Maximum accepted vendor or individual comment field size.
pub const MAX_TAG_FIELD_BYTES: usize = 64 * 1024;

/// Maximum accepted number of comment fields.
pub const MAX_TAG_COUNT: u32 = 4_096;

/// Maximum accepted decoder pre-skip for this bounded upload profile (one second).
pub const MAX_PRE_SKIP_SAMPLES: u16 = 48_000;

const OPUS_HEAD_MAGIC: &[u8; 8] = b"OpusHead";
const OPUS_TAGS_MAGIC: &[u8; 8] = b"OpusTags";
const NO_GRANULE_POSITION: u64 = u64::MAX;

/// Validated timing and packet metadata for one complete Ogg Opus upload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ValidatedOggOpus {
    /// Header pre-skip in 48 kHz samples.
    pub pre_skip_samples: u16,
    /// Authoritative playable duration from final granule position minus pre-skip.
    pub duration_samples: u64,
    /// Authoritative duration rounded up to the next whole millisecond.
    pub duration_millis: u64,
    /// Number of validated Opus audio packets.
    pub audio_packet_count: u32,
}

/// Fail-closed validation category. Display text intentionally reveals no input details.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum OggOpusValidationError {
    /// The physical upload is empty or exceeds its fixed byte bound.
    #[error("invalid Ogg Opus upload")]
    InvalidSize,
    /// Ogg framing, checksum, page sequence, or physical completeness is invalid.
    #[error("invalid Ogg Opus upload")]
    InvalidContainer,
    /// More than one logical stream, chaining, or invalid stream flags were found.
    #[error("invalid Ogg Opus upload")]
    InvalidLogicalStream,
    /// The identification header is unsupported or malformed.
    #[error("invalid Ogg Opus upload")]
    InvalidIdentificationHeader,
    /// The comment header is unsupported, malformed, or exceeds a field bound.
    #[error("invalid Ogg Opus upload")]
    InvalidCommentHeader,
    /// An audio packet is absent, oversized, or has an invalid Opus TOC sequence.
    #[error("invalid Ogg Opus upload")]
    InvalidAudioPacket,
    /// Page or final granule positions are inconsistent with packet durations.
    #[error("invalid Ogg Opus upload")]
    InvalidGranulePosition,
    /// A final end-of-stream page is absent.
    #[error("invalid Ogg Opus upload")]
    MissingEndOfStream,
}

#[derive(Clone, Copy, Debug)]
struct ContainerSummary {
    serial: u32,
    final_granule: u64,
}

/// Validates a complete RFC 7845 single-stream mono or stereo Ogg Opus upload.
///
/// # Errors
///
/// Returns a non-descriptive typed category for any size, container, header, packet, stream, or
/// granule violation.
pub fn validate_complete_ogg_opus(
    input: &[u8],
) -> Result<ValidatedOggOpus, OggOpusValidationError> {
    let container = scan_container(input)?;
    let mut options = PageParsingOptions::default();
    options.verify_checksum = true;
    let mut reader = PacketReader::new_with_page_parse_opts(Cursor::new(input), options);

    let head = next_packet(
        &mut reader,
        OggOpusValidationError::InvalidIdentificationHeader,
    )?;
    if head.stream_serial() != container.serial
        || !head.first_in_stream()
        || !head.first_in_page()
        || !head.last_in_page()
        || head.last_in_stream()
        || head.absgp_page() != 0
    {
        return Err(OggOpusValidationError::InvalidIdentificationHeader);
    }
    let pre_skip = validate_identification_header(&head.data)?;

    let tags = next_packet(&mut reader, OggOpusValidationError::InvalidCommentHeader)?;
    if tags.stream_serial() != container.serial
        || tags.first_in_stream()
        || !tags.last_in_page()
        || tags.last_in_stream()
        || tags.absgp_page() != 0
    {
        return Err(OggOpusValidationError::InvalidCommentHeader);
    }
    validate_comment_header(&tags.data)?;

    let mut timing = AudioTiming::new(pre_skip);
    while let Some(packet) = reader
        .read_packet()
        .map_err(|_| OggOpusValidationError::InvalidContainer)?
    {
        if packet.stream_serial() != container.serial || packet.first_in_stream() {
            return Err(OggOpusValidationError::InvalidLogicalStream);
        }
        timing.accept(&packet)?;
    }

    timing.finish(container.final_granule)
}

fn scan_container(input: &[u8]) -> Result<ContainerSummary, OggOpusValidationError> {
    if input.is_empty() || input.len() > MAX_OGG_OPUS_BYTES {
        return Err(OggOpusValidationError::InvalidSize);
    }

    let mut offset = 0usize;
    let mut serial = None;
    let mut expected_sequence = 0u32;
    let mut previous_granule = 0u64;
    let mut saw_position = false;
    let mut final_granule = None;

    while offset < input.len() {
        let header_end = offset
            .checked_add(27)
            .filter(|end| *end <= input.len())
            .ok_or(OggOpusValidationError::InvalidContainer)?;
        let header = &input[offset..header_end];
        if &header[..4] != b"OggS" || header[4] != 0 || header[5] & !0x07 != 0 {
            return Err(OggOpusValidationError::InvalidContainer);
        }

        let flags = header[5];
        let granule = read_u64_le(&header[6..14]);
        let page_serial = read_u32_le(&header[14..18]);
        let sequence = read_u32_le(&header[18..22]);
        let segment_count = usize::from(header[26]);
        let table_end = header_end
            .checked_add(segment_count)
            .filter(|end| *end <= input.len())
            .ok_or(OggOpusValidationError::InvalidContainer)?;
        let segments = &input[header_end..table_end];
        let body_len = segments
            .iter()
            .map(|value| usize::from(*value))
            .sum::<usize>();
        offset = table_end
            .checked_add(body_len)
            .filter(|end| *end <= input.len())
            .ok_or(OggOpusValidationError::InvalidContainer)?;
        let has_completed_packet = segments.iter().any(|value| *value < u8::MAX);
        if has_completed_packet == (granule == NO_GRANULE_POSITION) {
            return Err(OggOpusValidationError::InvalidGranulePosition);
        }

        match serial {
            None if flags & 0x02 != 0 && flags & 0x01 == 0 => serial = Some(page_serial),
            Some(value) if value == page_serial && flags & 0x02 == 0 => {}
            _ => return Err(OggOpusValidationError::InvalidLogicalStream),
        }
        if sequence != expected_sequence {
            return Err(OggOpusValidationError::InvalidContainer);
        }
        expected_sequence = expected_sequence
            .checked_add(1)
            .ok_or(OggOpusValidationError::InvalidContainer)?;

        if granule != NO_GRANULE_POSITION {
            if saw_position && granule < previous_granule {
                return Err(OggOpusValidationError::InvalidGranulePosition);
            }
            previous_granule = granule;
            saw_position = true;
        }

        if flags & 0x04 != 0 {
            if offset != input.len() || granule == NO_GRANULE_POSITION {
                return Err(OggOpusValidationError::InvalidLogicalStream);
            }
            final_granule = Some(granule);
        }
    }

    Ok(ContainerSummary {
        serial: serial.ok_or(OggOpusValidationError::InvalidContainer)?,
        final_granule: final_granule.ok_or(OggOpusValidationError::MissingEndOfStream)?,
    })
}

fn validate_identification_header(data: &[u8]) -> Result<u16, OggOpusValidationError> {
    if data.len() != 19
        || &data[..8] != OPUS_HEAD_MAGIC
        || data[8] != 1
        || !matches!(data[9], 1 | 2)
        || data[18] != 0
    {
        return Err(OggOpusValidationError::InvalidIdentificationHeader);
    }
    let pre_skip = read_u16_le(&data[10..12]);
    if pre_skip > MAX_PRE_SKIP_SAMPLES {
        return Err(OggOpusValidationError::InvalidIdentificationHeader);
    }
    Ok(pre_skip)
}

fn validate_comment_header(data: &[u8]) -> Result<(), OggOpusValidationError> {
    if data.len() < 16 || data.len() > MAX_OPUS_TAGS_BYTES || &data[..8] != OPUS_TAGS_MAGIC {
        return Err(OggOpusValidationError::InvalidCommentHeader);
    }
    let mut offset = 8usize;
    let vendor_len = read_bounded_tag_length(data, &mut offset)?;
    offset = offset
        .checked_add(vendor_len)
        .filter(|end| *end <= data.len())
        .ok_or(OggOpusValidationError::InvalidCommentHeader)?;
    let count = read_u32_at(data, &mut offset)?;
    if count > MAX_TAG_COUNT {
        return Err(OggOpusValidationError::InvalidCommentHeader);
    }
    for _ in 0..count {
        let comment_len = read_bounded_tag_length(data, &mut offset)?;
        offset = offset
            .checked_add(comment_len)
            .filter(|end| *end <= data.len())
            .ok_or(OggOpusValidationError::InvalidCommentHeader)?;
    }
    Ok(())
}

fn read_bounded_tag_length(
    data: &[u8],
    offset: &mut usize,
) -> Result<usize, OggOpusValidationError> {
    let length = usize::try_from(read_u32_at(data, offset)?)
        .map_err(|_| OggOpusValidationError::InvalidCommentHeader)?;
    if length > MAX_TAG_FIELD_BYTES {
        return Err(OggOpusValidationError::InvalidCommentHeader);
    }
    Ok(length)
}

fn read_u32_at(data: &[u8], offset: &mut usize) -> Result<u32, OggOpusValidationError> {
    let end = offset
        .checked_add(4)
        .filter(|end| *end <= data.len())
        .ok_or(OggOpusValidationError::InvalidCommentHeader)?;
    let value = read_u32_le(&data[*offset..end]);
    *offset = end;
    Ok(value)
}

fn next_packet(
    reader: &mut PacketReader<Cursor<&[u8]>>,
    missing: OggOpusValidationError,
) -> Result<Packet, OggOpusValidationError> {
    reader
        .read_packet()
        .map_err(|_| OggOpusValidationError::InvalidContainer)?
        .ok_or(missing)
}

#[derive(Debug)]
struct AudioTiming {
    pre_skip: u16,
    packet_count: u32,
    page_samples: u64,
    previous_page_granule: Option<u64>,
    saw_eos: bool,
}

impl AudioTiming {
    const fn new(pre_skip: u16) -> Self {
        Self {
            pre_skip,
            packet_count: 0,
            page_samples: 0,
            previous_page_granule: None,
            saw_eos: false,
        }
    }

    fn accept(&mut self, packet: &Packet) -> Result<(), OggOpusValidationError> {
        if self.saw_eos || packet.data.is_empty() || packet.data.len() > MAX_PACKET_BYTES {
            return Err(OggOpusValidationError::InvalidAudioPacket);
        }
        let samples = opus::packet::get_nb_samples(&packet.data, SAMPLE_RATE_HZ)
            .map_err(|_| OggOpusValidationError::InvalidAudioPacket)?;
        if samples == 0 || samples > MAX_SAMPLES_PER_PACKET {
            return Err(OggOpusValidationError::InvalidAudioPacket);
        }
        let samples =
            u64::try_from(samples).map_err(|_| OggOpusValidationError::InvalidAudioPacket)?;
        if packet.first_in_page() {
            self.page_samples = 0;
        }
        self.page_samples = self
            .page_samples
            .checked_add(samples)
            .ok_or(OggOpusValidationError::InvalidGranulePosition)?;
        self.packet_count = self
            .packet_count
            .checked_add(1)
            .ok_or(OggOpusValidationError::InvalidAudioPacket)?;

        if packet.last_in_page() {
            self.validate_page_granule(packet)?;
        }
        self.saw_eos = packet.last_in_stream();
        Ok(())
    }

    fn validate_page_granule(&mut self, packet: &Packet) -> Result<(), OggOpusValidationError> {
        let granule = packet.absgp_page();
        if granule == NO_GRANULE_POSITION {
            return Err(OggOpusValidationError::InvalidGranulePosition);
        }
        match self.previous_page_granule {
            None if packet.last_in_stream() => {
                if granule < u64::from(self.pre_skip) || granule > self.page_samples {
                    return Err(OggOpusValidationError::InvalidGranulePosition);
                }
            }
            // This bounded profile accepts only streams whose PCM timeline starts at zero. For a
            // non-final first audio page, RFC 7845 granule position is therefore exactly the sum
            // of the completed packets on that page; accepting a larger absolute offset would
            // turn an unrelated timeline origin into apparent recording duration.
            None if granule != self.page_samples => {
                return Err(OggOpusValidationError::InvalidGranulePosition);
            }
            Some(previous) => {
                let maximum = previous
                    .checked_add(self.page_samples)
                    .ok_or(OggOpusValidationError::InvalidGranulePosition)?;
                if packet.last_in_stream() && (granule <= previous || granule > maximum) {
                    return Err(OggOpusValidationError::InvalidGranulePosition);
                }
                if !packet.last_in_stream() && granule != maximum {
                    return Err(OggOpusValidationError::InvalidGranulePosition);
                }
            }
            None => {}
        }
        self.previous_page_granule = Some(granule);
        Ok(())
    }

    fn finish(self, final_granule: u64) -> Result<ValidatedOggOpus, OggOpusValidationError> {
        if self.packet_count == 0 {
            return Err(OggOpusValidationError::InvalidAudioPacket);
        }
        if !self.saw_eos {
            return Err(OggOpusValidationError::MissingEndOfStream);
        }
        if self.previous_page_granule != Some(final_granule) {
            return Err(OggOpusValidationError::InvalidGranulePosition);
        }
        let duration_samples = final_granule
            .checked_sub(u64::from(self.pre_skip))
            .filter(|duration| *duration > 0)
            .ok_or(OggOpusValidationError::InvalidGranulePosition)?;
        let duration_millis = duration_samples
            .checked_mul(1_000)
            .and_then(|value| value.checked_add(u64::from(SAMPLE_RATE_HZ) - 1))
            .map(|value| value / u64::from(SAMPLE_RATE_HZ))
            .ok_or(OggOpusValidationError::InvalidGranulePosition)?;

        Ok(ValidatedOggOpus {
            pre_skip_samples: self.pre_skip,
            duration_samples,
            duration_millis,
            audio_packet_count: self.packet_count,
        })
    }
}

fn read_u16_le(bytes: &[u8]) -> u16 {
    u16::from_le_bytes([bytes[0], bytes[1]])
}

fn read_u32_le(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn read_u64_le(bytes: &[u8]) -> u64 {
    u64::from_le_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ])
}

#[cfg(test)]
mod tests;
