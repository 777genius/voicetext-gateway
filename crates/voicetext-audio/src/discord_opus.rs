//! Stateful decoding of raw Discord Opus packets to mono PCM S16LE.

use opus::{Channels, Decoder, ErrorCode};
use thiserror::Error;

/// Discord uses the Opus native fullband sample rate.
pub const SAMPLE_RATE_HZ: u32 = 48_000;

/// Maximum legal size of one self-contained Opus packet.
pub const MAX_PACKET_BYTES: usize = 1_275;

/// Maximum samples per channel in one 120 ms Opus packet at 48 kHz.
pub const MAX_SAMPLES_PER_PACKET: usize = 5_760;

/// Maximum bytes emitted by one mono PCM S16LE frame.
pub const MAX_PCM_BYTES_PER_PACKET: usize = MAX_SAMPLES_PER_PACKET * size_of::<i16>();

/// A successfully decoded, exactly-sized mono PCM frame.
#[derive(Clone, Debug, PartialEq)]
pub struct DecodedPcmFrame {
    /// Deterministic little-endian signed 16-bit PCM bytes.
    pub pcm_s16le: Vec<u8>,
    /// Number of mono samples represented by `pcm_s16le`.
    pub samples: usize,
    /// Frame duration derived from `samples` at 48 kHz.
    pub duration_ms: f64,
}

/// Bounded initialization failure suitable for readiness reporting.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum DiscordOpusDecoderInitError {
    /// The linked libopus runtime could not initialize a mono decoder.
    #[error("libopus decoder initialization failed: {code:?}")]
    RuntimeUnavailable { code: ErrorCode },
}

/// Bounded validation or decoding failure for one raw packet.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum DiscordOpusDecodeError {
    /// Empty input is rejected rather than interpreted as packet loss concealment.
    #[error("empty Opus packets are not accepted")]
    EmptyPacket,
    /// The packet exceeds the protocol's fixed allocation bound.
    #[error("Opus packet is {actual} bytes; maximum is {maximum}")]
    PacketTooLarge { actual: usize, maximum: usize },
    /// libopus could not inspect the packet duration.
    #[error("invalid Opus packet metadata: {code:?}")]
    InvalidPacketMetadata { code: ErrorCode },
    /// The declared packet duration is outside the accepted Discord bound.
    #[error("Opus packet declares {samples} samples; maximum is {maximum}")]
    InvalidSampleCount { samples: usize, maximum: usize },
    /// libopus rejected the packet while decoding without FEC.
    #[error("Opus packet decoding failed: {code:?}")]
    DecodeFailed { code: ErrorCode },
    /// Decoder output disagreed with the packet metadata inspected immediately before decode.
    #[error("Opus decoder returned {actual} samples; expected {expected}")]
    DecodedSampleCountMismatch { expected: usize, actual: usize },
}

/// One stateful mono decoder. Create a separate instance for every live session.
#[derive(Debug)]
pub struct DiscordOpusDecoder {
    decoder: Decoder,
    samples: Vec<i16>,
}

impl DiscordOpusDecoder {
    /// Reports whether libopus can initialize the decoder used by live sessions.
    ///
    /// This is an initialization readiness probe, not a packet conformance check. The selected
    /// `opus` dependency currently builds bundled libopus, so `CMake` is required at build time.
    pub fn runtime_available() -> bool {
        Self::new().is_ok()
    }

    /// Creates isolated decoder state for one live session.
    ///
    /// # Errors
    ///
    /// Returns a bounded initialization error if libopus rejects decoder creation.
    pub fn new() -> Result<Self, DiscordOpusDecoderInitError> {
        let decoder = Decoder::new(SAMPLE_RATE_HZ, Channels::Mono).map_err(|error| {
            DiscordOpusDecoderInitError::RuntimeUnavailable { code: error.code() }
        })?;

        Ok(Self {
            decoder,
            samples: vec![0; MAX_SAMPLES_PER_PACKET],
        })
    }

    /// Decodes one complete raw Opus packet without packet-loss concealment or FEC.
    ///
    /// # Errors
    ///
    /// Rejects empty, oversized, malformed, over-duration, and inconsistently decoded packets.
    pub fn decode(&mut self, packet: &[u8]) -> Result<DecodedPcmFrame, DiscordOpusDecodeError> {
        if packet.is_empty() {
            return Err(DiscordOpusDecodeError::EmptyPacket);
        }
        if packet.len() > MAX_PACKET_BYTES {
            return Err(DiscordOpusDecodeError::PacketTooLarge {
                actual: packet.len(),
                maximum: MAX_PACKET_BYTES,
            });
        }

        let expected_samples = self.decoder.get_nb_samples(packet).map_err(|error| {
            DiscordOpusDecodeError::InvalidPacketMetadata { code: error.code() }
        })?;
        if expected_samples == 0 || expected_samples > MAX_SAMPLES_PER_PACKET {
            return Err(DiscordOpusDecodeError::InvalidSampleCount {
                samples: expected_samples,
                maximum: MAX_SAMPLES_PER_PACKET,
            });
        }

        let decoded_samples = self
            .decoder
            .decode(packet, &mut self.samples, false)
            .map_err(|error| DiscordOpusDecodeError::DecodeFailed { code: error.code() })?;
        if decoded_samples != expected_samples {
            return Err(DiscordOpusDecodeError::DecodedSampleCountMismatch {
                expected: expected_samples,
                actual: decoded_samples,
            });
        }

        let mut pcm_s16le = Vec::with_capacity(decoded_samples * size_of::<i16>());
        for sample in &self.samples[..decoded_samples] {
            pcm_s16le.extend_from_slice(&sample.to_le_bytes());
        }
        debug_assert!(pcm_s16le.len() <= MAX_PCM_BYTES_PER_PACKET);

        let bounded_samples = u32::try_from(decoded_samples).map_err(|_| {
            DiscordOpusDecodeError::InvalidSampleCount {
                samples: decoded_samples,
                maximum: MAX_SAMPLES_PER_PACKET,
            }
        })?;
        Ok(DecodedPcmFrame {
            pcm_s16le,
            samples: decoded_samples,
            duration_ms: f64::from(bounded_samples) * 1_000.0 / f64::from(SAMPLE_RATE_HZ),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DISCORD_SILENCE_PACKET: [u8; 3] = [0xf8, 0xff, 0xfe];

    #[test]
    fn availability_probe_agrees_with_constructor() {
        if !DiscordOpusDecoder::runtime_available() {
            eprintln!("libopus is unavailable; deployment readiness must remain false");
            return;
        }
        assert_eq!(
            DiscordOpusDecoder::runtime_available(),
            DiscordOpusDecoder::new().is_ok()
        );
    }

    #[test]
    fn decodes_discord_silence_to_exact_mono_pcm() {
        let mut decoder = DiscordOpusDecoder::new().unwrap();

        let frame = decoder.decode(&DISCORD_SILENCE_PACKET).unwrap();

        assert_eq!(frame.samples, 960);
        assert!((frame.duration_ms - 20.0).abs() < f64::EPSILON);
        assert_eq!(frame.pcm_s16le.len(), 1_920);
        assert!(frame.pcm_s16le.len() <= MAX_PCM_BYTES_PER_PACKET);
    }

    #[test]
    fn rejects_empty_packet_without_invoking_packet_loss_concealment() {
        let mut decoder = DiscordOpusDecoder::new().unwrap();

        assert_eq!(
            decoder.decode(&[]),
            Err(DiscordOpusDecodeError::EmptyPacket)
        );
    }

    #[test]
    fn rejects_oversized_packet_before_libopus() {
        let mut decoder = DiscordOpusDecoder::new().unwrap();
        let packet = vec![0; MAX_PACKET_BYTES + 1];

        assert_eq!(
            decoder.decode(&packet),
            Err(DiscordOpusDecodeError::PacketTooLarge {
                actual: MAX_PACKET_BYTES + 1,
                maximum: MAX_PACKET_BYTES,
            })
        );
    }

    #[test]
    fn rejects_malformed_packet() {
        let mut decoder = DiscordOpusDecoder::new().unwrap();

        assert!(matches!(
            decoder.decode(&[0xff]),
            Err(DiscordOpusDecodeError::InvalidPacketMetadata { .. }
                | DiscordOpusDecodeError::DecodeFailed { .. })
        ));
    }

    #[test]
    fn ten_sessions_keep_independent_bounded_decoder_state() {
        let first = DiscordOpusDecoder::new().unwrap();
        let mut decoders = vec![first];
        for _ in 1..10 {
            decoders.push(DiscordOpusDecoder::new().unwrap());
        }

        for decoder in &mut decoders {
            let frame = decoder.decode(&DISCORD_SILENCE_PACKET).unwrap();
            assert_eq!(frame.samples, 960);
            assert_eq!(frame.pcm_s16le.len(), 1_920);
        }
    }
}
