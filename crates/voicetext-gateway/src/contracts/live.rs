//! `VoiceText` live protocol v2 JSON frames.
//!
//! Raw audio is intentionally absent: each audio frame remains binary transport data.

use serde::{Deserialize, Deserializer, Serialize};
use uuid::{Uuid, Variant};

use super::ContractViolation;
use super::live_capabilities::{LiveInputFormat, validate_client_profile};

const MAX_ERROR_CODE_CHARS: usize = 128;
const MAX_ERROR_MESSAGE_CHARS: usize = 2_048;
const MAX_KEYTERM_CHARS: usize = 256;
const MAX_KEYTERM_COUNT: usize = 100;
const MAX_KEYTERM_TOTAL_CHARS: usize = 8_192;
const MAX_LANGUAGE_CHARS: usize = 64;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_TRANSCRIPT_LIMIT: usize = 65_536;
const MAX_COMMAND_BYTES: usize = 1_024;

/// Exact text commands accepted after the live config frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClientCommand {
    Finalize,
    Close,
}

/// Parses a bounded control frame without accepting audio encoded as JSON.
///
/// # Errors
///
/// Returns [`ContractViolation`] for oversized JSON, unknown commands, or extra fields.
pub fn parse_client_command(json: &str) -> Result<ClientCommand, ContractViolation> {
    require(
        json.len() <= MAX_COMMAND_BYTES,
        "live command exceeds byte limit",
    )?;
    let value: serde_json::Value =
        serde_json::from_str(json).map_err(|_| ContractViolation("invalid live client command"))?;
    let object = value
        .as_object()
        .ok_or(ContractViolation("invalid live client command"))?;
    require(object.len() == 1, "live client command has extra fields")?;
    match object.get("type").and_then(serde_json::Value::as_str) {
        Some("finalize") => Ok(ClientCommand::Finalize),
        Some("close") => Ok(ClientCommand::Close),
        _ => Err(ContractViolation("unsupported live client command")),
    }
}

/// The only supported live provider/model identities.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiveIdentity {
    DeepgramNova3,
    ElevenlabsScribeV2Realtime,
}

impl LiveIdentity {
    pub const fn provider(self) -> Provider {
        match self {
            Self::DeepgramNova3 => Provider::Deepgram,
            Self::ElevenlabsScribeV2Realtime => Provider::Elevenlabs,
        }
    }

    pub const fn model(self) -> Model {
        match self {
            Self::DeepgramNova3 => Model::Nova3,
            Self::ElevenlabsScribeV2Realtime => Model::ScribeV2Realtime,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Deepgram,
    Elevenlabs,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub enum Model {
    #[serde(rename = "nova-3")]
    Nova3,
    #[serde(rename = "scribe_v2_realtime")]
    ScribeV2Realtime,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioFormat {
    Opus48Khz,
    PcmS16le16Khz,
}

/// A validated client config frame. No binary audio is represented here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientConfig {
    pub identity: LiveIdentity,
    pub language: String,
    pub client_session_id: Uuid,
    pub audio_format: AudioFormat,
    pub keyterms: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ConfigWire {
    #[serde(rename = "type")]
    message_type: ConfigType,
    provider: Provider,
    model: Model,
    language: String,
    capabilities: Vec<Capability>,
    channels: u8,
    protocol_v: u8,
    #[serde(deserialize_with = "deserialize_session_id")]
    client_session_id: Uuid,
    encoding: Encoding,
    sample_rate: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    keyterms: Vec<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
enum ConfigType {
    #[serde(rename = "config")]
    Config,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
enum Capability {
    #[serde(rename = "finalize_ack")]
    FinalizeAck,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
enum Encoding {
    #[serde(rename = "opus")]
    Opus,
    #[serde(rename = "pcm_s16le")]
    PcmS16le,
}

/// Parses the first live JSON frame and enforces protocol, identity, and resource bounds.
///
/// # Errors
///
/// Returns [`ContractViolation`] when JSON shape, protocol identity, audio format, or a bounded
/// string collection is invalid.
pub fn parse_client_config(json: &str) -> Result<ClientConfig, ContractViolation> {
    let wire: ConfigWire =
        serde_json::from_str(json).map_err(|_| ContractViolation("invalid live config JSON"))?;
    require(wire.protocol_v == 2, "unsupported live protocol version")?;
    require(wire.channels == 1, "live audio must be mono")?;
    require(
        wire.capabilities == [Capability::FinalizeAck],
        "invalid live capabilities",
    )?;
    let identity = match (wire.provider, wire.model) {
        (Provider::Deepgram, Model::Nova3) => LiveIdentity::DeepgramNova3,
        (Provider::Elevenlabs, Model::ScribeV2Realtime) => LiveIdentity::ElevenlabsScribeV2Realtime,
        _ => return Err(ContractViolation("live provider/model identity mismatch")),
    };
    let audio_format = match (wire.encoding, wire.sample_rate) {
        (Encoding::Opus, 48_000) => AudioFormat::Opus48Khz,
        (Encoding::PcmS16le, 16_000) => AudioFormat::PcmS16le16Khz,
        _ => return Err(ContractViolation("incompatible live audio format")),
    };
    validate_language(&wire.language)?;
    validate_keyterms(&wire.keyterms)?;
    let input_format = match audio_format {
        AudioFormat::Opus48Khz => LiveInputFormat::Opus48KhzMono,
        AudioFormat::PcmS16le16Khz => LiveInputFormat::PcmS16Le16KhzMono,
    };
    validate_client_profile(identity, &wire.language, &wire.keyterms, input_format)
        .map_err(|_| ContractViolation("unsupported live profile features"))?;
    Ok(ClientConfig {
        identity,
        language: wire.language,
        client_session_id: wire.client_session_id,
        audio_format,
        keyterms: wire.keyterms,
    })
}

/// Produces the exact client config JSON shape from validated boundary data.
///
/// # Errors
///
/// Returns [`ContractViolation`] when caller-created language or keyterm data exceeds its bounds,
/// or if serialization fails.
pub fn serialize_client_config(config: &ClientConfig) -> Result<String, ContractViolation> {
    validate_language(&config.language)?;
    validate_keyterms(&config.keyterms)?;
    let (encoding, sample_rate) = match config.audio_format {
        AudioFormat::Opus48Khz => (Encoding::Opus, 48_000),
        AudioFormat::PcmS16le16Khz => (Encoding::PcmS16le, 16_000),
    };
    let wire = ConfigWire {
        message_type: ConfigType::Config,
        provider: config.identity.provider(),
        model: config.identity.model(),
        language: config.language.clone(),
        capabilities: vec![Capability::FinalizeAck],
        channels: 1,
        protocol_v: 2,
        client_session_id: config.client_session_id,
        encoding,
        sample_rate,
        keyterms: config.keyterms.clone(),
    };
    serde_json::to_string(&wire).map_err(|_| ContractViolation("cannot serialize live config"))
}

/// Validated server JSON message projected exactly as the TypeScript client consumes it.
#[derive(Clone, Debug, PartialEq)]
pub enum ServerMessage {
    Ready {
        session_id: Uuid,
        identity: LiveIdentity,
    },
    Ack {
        seq: u64,
    },
    Partial {
        segment: TranscriptSegment,
    },
    Final(TranscriptSegment),
    SegmentFinal(TranscriptSegment),
    Error {
        code: String,
        message: String,
    },
    FinalizeComplete {
        status: FinalizeStatus,
        saw_result: bool,
    },
    UsageUpdate,
    Resumed,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TranscriptSegment {
    pub text: String,
    pub start_ms: u64,
    pub duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FinalizeStatus {
    Flushed,
    NoProvider,
    Timeout,
}

/// Parses one server frame, requiring `ready` to match the bound session identity.
///
/// # Errors
///
/// Returns [`ContractViolation`] for malformed or unsupported JSON, identity drift, inconsistent
/// finalize evidence, or fields outside the supplied and protocol-wide bounds.
pub fn parse_server_message(
    json: &str,
    expected: LiveIdentity,
    max_transcript_chars: usize,
) -> Result<ServerMessage, ContractViolation> {
    require(
        (1..=MAX_TRANSCRIPT_LIMIT).contains(&max_transcript_chars),
        "invalid transcript bound",
    )?;
    let value: serde_json::Value =
        serde_json::from_str(json).map_err(|_| ContractViolation("invalid live server JSON"))?;
    let message_type = value
        .get("type")
        .and_then(serde_json::Value::as_str)
        .ok_or(ContractViolation("live server message has no type"))?;
    match message_type {
        "ready" => parse_ready(value, expected),
        "ack" => parse_ack(value),
        "partial" => parse_partial(value, max_transcript_chars),
        "final" => parse_final(value, max_transcript_chars),
        "error" => parse_error(value),
        "finalize_complete" => parse_finalize(value),
        "usage_update" => parse_empty(&value, "usage_update", ServerMessage::UsageUpdate),
        "resumed" => parse_empty(&value, "resumed", ServerMessage::Resumed),
        _ => Err(ContractViolation("unsupported live server message type")),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadyWire {
    #[serde(rename = "type")]
    _message_type: ReadyType,
    #[serde(deserialize_with = "deserialize_session_id")]
    session_id: Uuid,
    provider: Provider,
    model: Model,
}

#[derive(Debug, Deserialize)]
enum ReadyType {
    #[serde(rename = "ready")]
    Ready,
}

fn parse_ready(
    value: serde_json::Value,
    expected: LiveIdentity,
) -> Result<ServerMessage, ContractViolation> {
    let wire: ReadyWire =
        serde_json::from_value(value).map_err(|_| ContractViolation("invalid ready message"))?;
    require(
        wire.provider == expected.provider() && wire.model == expected.model(),
        "ready identity mismatch",
    )?;
    Ok(ServerMessage::Ready {
        session_id: wire.session_id,
        identity: expected,
    })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AckWire {
    #[serde(rename = "type")]
    _message_type: AckType,
    seq: u64,
}

#[derive(Debug, Deserialize)]
enum AckType {
    #[serde(rename = "ack")]
    Ack,
}

fn parse_ack(value: serde_json::Value) -> Result<ServerMessage, ContractViolation> {
    let wire: AckWire =
        serde_json::from_value(value).map_err(|_| ContractViolation("invalid ack message"))?;
    require(
        (1..=MAX_SAFE_INTEGER).contains(&wire.seq),
        "invalid audio acknowledgement",
    )?;
    Ok(ServerMessage::Ack { seq: wire.seq })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SegmentWire {
    #[serde(rename = "type")]
    _message_type: SegmentType,
    text: String,
    start_ms: u64,
    duration_ms: u64,
    #[serde(default)]
    confidence: Option<f64>,
    #[serde(default)]
    is_segment_final: bool,
}

#[derive(Debug, Deserialize)]
enum SegmentType {
    #[serde(rename = "partial")]
    Partial,
}

fn parse_partial(
    value: serde_json::Value,
    maximum: usize,
) -> Result<ServerMessage, ContractViolation> {
    let wire: SegmentWire =
        serde_json::from_value(value).map_err(|_| ContractViolation("invalid partial message"))?;
    let segment = validate_segment(
        wire.text,
        wire.start_ms,
        wire.duration_ms,
        wire.confidence,
        maximum,
    )?;
    Ok(if wire.is_segment_final {
        ServerMessage::SegmentFinal(segment)
    } else {
        ServerMessage::Partial { segment }
    })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FinalWire {
    #[serde(rename = "type")]
    _message_type: FinalType,
    text: String,
    start_ms: u64,
    duration_ms: u64,
    #[serde(default)]
    confidence: Option<f64>,
}

#[derive(Debug, Deserialize)]
enum FinalType {
    #[serde(rename = "final")]
    Final,
}

fn parse_final(
    value: serde_json::Value,
    maximum: usize,
) -> Result<ServerMessage, ContractViolation> {
    let wire: FinalWire =
        serde_json::from_value(value).map_err(|_| ContractViolation("invalid final message"))?;
    Ok(ServerMessage::Final(validate_segment(
        wire.text,
        wire.start_ms,
        wire.duration_ms,
        wire.confidence,
        maximum,
    )?))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ErrorWire {
    #[serde(rename = "type")]
    _message_type: ErrorType,
    code: String,
    message: String,
}

#[derive(Debug, Deserialize)]
enum ErrorType {
    #[serde(rename = "error")]
    Error,
}

fn parse_error(value: serde_json::Value) -> Result<ServerMessage, ContractViolation> {
    let wire: ErrorWire =
        serde_json::from_value(value).map_err(|_| ContractViolation("invalid error message"))?;
    require(
        !wire.code.is_empty() && char_len(&wire.code) <= MAX_ERROR_CODE_CHARS,
        "invalid live error code",
    )?;
    require(
        char_len(&wire.message) <= MAX_ERROR_MESSAGE_CHARS,
        "live error message exceeds bound",
    )?;
    Ok(ServerMessage::Error {
        code: wire.code,
        message: wire.message,
    })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FinalizeWire {
    #[serde(rename = "type")]
    _message_type: FinalizeType,
    status: FinalizeStatus,
    saw_result: bool,
}

#[derive(Debug, Deserialize)]
enum FinalizeType {
    #[serde(rename = "finalize_complete")]
    FinalizeComplete,
}

fn parse_finalize(value: serde_json::Value) -> Result<ServerMessage, ContractViolation> {
    let wire: FinalizeWire = serde_json::from_value(value)
        .map_err(|_| ContractViolation("invalid finalize acknowledgement"))?;
    require(
        !matches!(
            (wire.status, wire.saw_result),
            (FinalizeStatus::Flushed, false) | (FinalizeStatus::NoProvider, true)
        ),
        "inconsistent finalize evidence",
    )?;
    Ok(ServerMessage::FinalizeComplete {
        status: wire.status,
        saw_result: wire.saw_result,
    })
}

fn parse_empty(
    value: &serde_json::Value,
    expected: &'static str,
    result: ServerMessage,
) -> Result<ServerMessage, ContractViolation> {
    let object = value
        .as_object()
        .ok_or(ContractViolation("server message is not an object"))?;
    require(
        object.len() == 1
            && object.get("type").and_then(serde_json::Value::as_str) == Some(expected),
        "unexpected fields in empty server message",
    )?;
    Ok(result)
}

fn validate_segment(
    text: String,
    start_ms: u64,
    duration_ms: u64,
    confidence: Option<f64>,
    maximum: usize,
) -> Result<TranscriptSegment, ContractViolation> {
    require(char_len(&text) <= maximum, "live transcript exceeds bound")?;
    require(
        start_ms <= MAX_SAFE_INTEGER && duration_ms <= MAX_SAFE_INTEGER,
        "live segment time exceeds safe integer",
    )?;
    require(
        confidence.is_none_or(|value| value.is_finite() && (0.0..=1.0).contains(&value)),
        "invalid live confidence",
    )?;
    Ok(TranscriptSegment {
        text,
        start_ms,
        duration_ms,
        confidence,
    })
}

fn validate_language(language: &str) -> Result<(), ContractViolation> {
    require(
        !language.trim().is_empty()
            && language.trim() == language
            && char_len(language) <= MAX_LANGUAGE_CHARS
            && !language.chars().any(char::is_control),
        "invalid live language",
    )
}

fn validate_keyterms(keyterms: &[String]) -> Result<(), ContractViolation> {
    require(
        keyterms.len() <= MAX_KEYTERM_COUNT,
        "too many live keyterms",
    )?;
    let mut total = 0_usize;
    for keyterm in keyterms {
        let length = char_len(keyterm);
        require(
            !keyterm.trim().is_empty()
                && keyterm.trim() == keyterm
                && length <= MAX_KEYTERM_CHARS
                && !keyterm.chars().any(char::is_control),
            "invalid live keyterm",
        )?;
        total = total
            .checked_add(length)
            .ok_or(ContractViolation("live keyterms exceed total bound"))?;
    }
    require(
        total <= MAX_KEYTERM_TOTAL_CHARS,
        "live keyterms exceed total bound",
    )
}

fn deserialize_session_id<'de, D>(deserializer: D) -> Result<Uuid, D::Error>
where
    D: Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    let bytes = value.as_bytes();
    let canonical = bytes.len() == 36
        && [8, 13, 18, 23]
            .into_iter()
            .all(|index| bytes[index] == b'-')
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| [8, 13, 18, 23].contains(&index) || byte.is_ascii_hexdigit());
    let uuid = Uuid::parse_str(&value).map_err(serde::de::Error::custom)?;
    if !canonical
        || uuid.get_variant() != Variant::RFC4122
        || !(1..=8).contains(&uuid.get_version_num())
    {
        return Err(serde::de::Error::custom(
            "session id must be a canonical RFC UUID",
        ));
    }
    Ok(uuid)
}

fn char_len(value: &str) -> usize {
    value.encode_utf16().count()
}

fn require(condition: bool, message: &'static str) -> Result<(), ContractViolation> {
    condition.then_some(()).ok_or(ContractViolation(message))
}
