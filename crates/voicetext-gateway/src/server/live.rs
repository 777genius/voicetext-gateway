//! Bounded `VoiceText` protocol v2 WebSocket transport.

use std::time::Duration;

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::HeaderMap;
use axum::response::Response;
use tokio::time::{Instant, timeout};
use uuid::Uuid;
use voicetext_audio::discord_opus::DiscordOpusDecoder;
use voicetext_speech::application::live::{
    LiveCoordinator, LiveCoordinatorError, LiveCoordinatorEvent,
};
use voicetext_speech::application::ports::{
    LiveRecognitionEvent, LiveRecognitionRequest, LiveTranscript, LiveTranscriptStability,
    RecognitionFailure,
};
use voicetext_speech::domain::live::FinalizeStatus as DomainFinalizeStatus;

use crate::auth::authenticate;
use crate::contracts::live::{
    AudioFormat, ClientCommand, ClientConfig, FinalizeStatus, LiveIdentity, TranscriptSegment,
    parse_client_command, parse_client_config,
};
use crate::contracts::live_outbound::{OutboundServerMessage, serialize_server_message};

use super::error::GatewayHttpError;
use super::live_diagnostics::{recognition_failure_class, safe_provider_failure_code};
use super::metrics::GatewayMetrics;
use super::state::GatewayState;

const FINALIZE_QUIESCENCE: Duration = Duration::from_millis(100);

/// Authenticates and upgrades one bounded live transcription connection.
pub(crate) async fn upgrade(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    websocket: WebSocketUpgrade,
) -> Result<Response, GatewayHttpError> {
    authenticate(&headers, state.auth()).map_err(GatewayHttpError::unauthorized)?;
    let permit = state
        .try_acquire_live_slot()
        .ok_or_else(GatewayHttpError::rate_limited)?;
    let maximum = state.limits().live_frame_bytes;
    Ok(websocket
        .max_message_size(maximum)
        .max_frame_size(maximum)
        .on_upgrade(move |socket| async move {
            let _permit = permit;
            state.metrics().live_session();
            run(socket, state).await;
        }))
}

async fn run(mut socket: WebSocket, state: GatewayState) {
    let mut opened = match prepare_session(&mut socket, &state).await {
        Ok(opened) => opened,
        Err(error) => {
            fail_socket(&mut socket, error, state.metrics()).await;
            return;
        }
    };
    if send_message(
        &mut socket,
        OutboundServerMessage::Ready {
            session_id: Uuid::new_v4(),
            identity: opened.identity,
        },
    )
    .await
    .is_err()
    {
        state.metrics().live_failure();
        let _ = opened.coordinator.close().await;
        return;
    }

    stream(&mut socket, &state, &mut opened).await;
    let _ = opened.coordinator.close().await;
    let _ = socket.send(Message::Close(None)).await;
}

async fn prepare_session(
    socket: &mut WebSocket,
    state: &GatewayState,
) -> Result<OpenedSession, SafeLiveError> {
    let limits = state.limits();
    let config =
        receive_config(socket, limits.live_frame_bytes, limits.first_frame_timeout).await?;
    let factory = state
        .profiles()
        .live(config.identity)
        .cloned()
        .ok_or(SafeLiveError::ProfileNotConfigured)?;
    let decoder = match config.audio_format {
        AudioFormat::Opus48Khz => {
            Some(DiscordOpusDecoder::new().map_err(|_| SafeLiveError::AudioRuntimeUnavailable)?)
        }
        AudioFormat::PcmS16le16Khz => None,
    };
    let request = recognition_request(&config);
    let coordinator = LiveCoordinator::open(factory.as_ref(), request)
        .await
        .map_err(|error| SafeLiveError::from_coordinator(&error, "open"))?;
    Ok(OpenedSession {
        identity: config.identity,
        decoder,
        coordinator,
    })
}

async fn stream(socket: &mut WebSocket, state: &GatewayState, opened: &mut OpenedSession) {
    let limits = state.limits();

    let mut accepted_audio = false;
    loop {
        let raced = {
            let provider_read = opened.coordinator.receive_provider_event();
            tokio::pin!(provider_read);
            tokio::select! {
                client = socket.recv() => Raced::Client(client),
                provider = &mut provider_read => Raced::Provider(provider),
            }
        };
        let control = match raced {
            Raced::Client(client) => {
                handle_client_frame(
                    client,
                    socket,
                    &mut opened.coordinator,
                    &mut opened.decoder,
                    limits.live_frame_bytes,
                    &mut accepted_audio,
                    state.metrics(),
                )
                .await
            }
            Raced::Provider(provider) => {
                handle_provider_event(provider, socket, &mut opened.coordinator).await
            }
        };
        match control {
            Ok(LoopControl::Continue) => {}
            Ok(LoopControl::Close) => break,
            Ok(LoopControl::Finalize) => {
                let finalized = finalize(
                    socket,
                    &mut opened.coordinator,
                    accepted_audio,
                    limits.finalize_timeout,
                )
                .await;
                match finalized {
                    Ok(()) => {
                        if let Err(error) = await_client_close(
                            socket,
                            limits.live_frame_bytes,
                            limits.finalize_timeout,
                        )
                        .await
                        {
                            fail_socket(socket, error, state.metrics()).await;
                        }
                    }
                    Err(error) => fail_socket(socket, error, state.metrics()).await,
                }
                break;
            }
            Err(error) => {
                fail_socket(socket, error, state.metrics()).await;
                break;
            }
        }
    }
}

async fn await_client_close(
    socket: &mut WebSocket,
    maximum: usize,
    maximum_wait: Duration,
) -> Result<(), SafeLiveError> {
    let deadline = Instant::now() + maximum_wait;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(());
        }
        let received = match timeout(remaining, socket.recv()).await {
            Err(_) | Ok(None | Some(Ok(Message::Close(_)))) => return Ok(()),
            Ok(Some(Err(_))) => return Err(SafeLiveError::TransportClosed),
            Ok(Some(Ok(message))) => message,
        };
        match received {
            Message::Text(text) if text.len() <= maximum => {
                if parse_client_command(&text) == Ok(ClientCommand::Close) {
                    return Ok(());
                }
                return Err(SafeLiveError::InvalidCommand);
            }
            Message::Ping(payload) => socket
                .send(Message::Pong(payload))
                .await
                .map_err(|_| SafeLiveError::TransportClosed)?,
            Message::Pong(_) => {}
            Message::Text(_) => return Err(SafeLiveError::FrameTooLarge),
            Message::Binary(_) => return Err(SafeLiveError::InvalidCommand),
            Message::Close(_) => return Ok(()),
        }
    }
}

async fn receive_config(
    socket: &mut WebSocket,
    maximum: usize,
    wait: Duration,
) -> Result<ClientConfig, SafeLiveError> {
    let received = timeout(wait, socket.recv())
        .await
        .map_err(|_| SafeLiveError::ConfigTimeout)?
        .ok_or(SafeLiveError::TransportClosed)?
        .map_err(|_| SafeLiveError::TransportClosed)?;
    let Message::Text(text) = received else {
        return Err(SafeLiveError::InvalidConfig);
    };
    if text.len() > maximum {
        return Err(SafeLiveError::FrameTooLarge);
    }
    parse_client_config(&text).map_err(|_| SafeLiveError::InvalidConfig)
}

async fn handle_client_frame(
    received: Option<Result<Message, axum::Error>>,
    socket: &mut WebSocket,
    coordinator: &mut LiveCoordinator,
    decoder: &mut Option<DiscordOpusDecoder>,
    maximum: usize,
    accepted_audio: &mut bool,
    metrics: &GatewayMetrics,
) -> Result<LoopControl, SafeLiveError> {
    let message = received
        .ok_or(SafeLiveError::TransportClosed)?
        .map_err(|_| SafeLiveError::TransportClosed)?;
    match message {
        Message::Binary(frame) => {
            if frame.is_empty() || frame.len() > maximum {
                return Err(if frame.len() > maximum {
                    SafeLiveError::FrameTooLarge
                } else {
                    SafeLiveError::InvalidAudio
                });
            }
            let pcm = decode_audio(decoder, frame.to_vec())?;
            let sequence = coordinator
                .provider_write(pcm)
                .await
                .map_err(|error| SafeLiveError::from_coordinator(&error, "write"))?;
            metrics.live_frame();
            send_message(
                socket,
                OutboundServerMessage::Ack {
                    seq: sequence.get(),
                },
            )
            .await?;
            coordinator
                .ack_sent(sequence)
                .map_err(|error| SafeLiveError::from_coordinator(&error, "ack"))?;
            *accepted_audio = true;
            Ok(LoopControl::Continue)
        }
        Message::Text(text) => match parse_client_command(&text) {
            Ok(ClientCommand::Finalize) => Ok(LoopControl::Finalize),
            Ok(ClientCommand::Close) => Ok(LoopControl::Close),
            Err(_) => Err(SafeLiveError::InvalidCommand),
        },
        Message::Ping(payload) => {
            socket
                .send(Message::Pong(payload))
                .await
                .map_err(|_| SafeLiveError::TransportClosed)?;
            Ok(LoopControl::Continue)
        }
        Message::Pong(_) => Ok(LoopControl::Continue),
        Message::Close(_) => Ok(LoopControl::Close),
    }
}

fn decode_audio(
    decoder: &mut Option<DiscordOpusDecoder>,
    frame: Vec<u8>,
) -> Result<Vec<u8>, SafeLiveError> {
    match decoder {
        Some(decoder) => decoder
            .decode(&frame)
            .map(|decoded| decoded.pcm_s16le)
            .map_err(|_| SafeLiveError::InvalidAudio),
        None if !frame.is_empty() && (frame.len() & 1) == 0 => Ok(frame),
        None => Err(SafeLiveError::InvalidAudio),
    }
}

async fn handle_provider_event(
    received: Result<Option<LiveRecognitionEvent>, RecognitionFailure>,
    socket: &mut WebSocket,
    coordinator: &mut LiveCoordinator,
) -> Result<LoopControl, SafeLiveError> {
    let ended = matches!(received, Ok(None));
    let event = coordinator
        .apply_provider_event(received)
        .map_err(|error| SafeLiveError::from_coordinator(&error, "stream"))?;
    if let Some(event) = event {
        send_coordinator_event(socket, event).await?;
    }
    if ended {
        Err(SafeLiveError::ProviderClosed)
    } else {
        Ok(LoopControl::Continue)
    }
}

async fn finalize(
    socket: &mut WebSocket,
    coordinator: &mut LiveCoordinator,
    accepted_audio: bool,
    maximum_wait: Duration,
) -> Result<(), SafeLiveError> {
    if coordinator.pending_audio_count() != 0 {
        return Err(SafeLiveError::PendingAcknowledgements);
    }
    coordinator
        .begin_finalize()
        .await
        .map_err(|error| SafeLiveError::from_coordinator(&error, "finalize_begin"))?;

    let deadline = Instant::now() + maximum_wait;
    let mut saw_result = false;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let wait = if saw_result {
            remaining.min(FINALIZE_QUIESCENCE)
        } else {
            remaining
        };
        let Ok(received) = timeout(wait, coordinator.receive_provider_event()).await else {
            break;
        };
        let ended = matches!(received, Ok(None));
        let observed = matches!(
            received,
            Ok(Some(LiveRecognitionEvent::FinalizeResultObserved))
        );
        let event = coordinator
            .apply_provider_event(received)
            .map_err(|error| SafeLiveError::from_coordinator(&error, "finalize_stream"))?;
        saw_result |= observed;
        if let Some(event) = event {
            send_coordinator_event(socket, event).await?;
        }
        if ended || Instant::now() >= deadline {
            break;
        }
    }

    let requested = if saw_result {
        DomainFinalizeStatus::Flushed
    } else if accepted_audio {
        DomainFinalizeStatus::Timeout
    } else {
        DomainFinalizeStatus::NoProvider
    };
    let outcome = coordinator
        .complete_finalize(requested)
        .map_err(|error| SafeLiveError::from_coordinator(&error, "finalize_complete"))?;
    send_message(
        socket,
        OutboundServerMessage::FinalizeComplete {
            status: match outcome.status() {
                DomainFinalizeStatus::Flushed => FinalizeStatus::Flushed,
                DomainFinalizeStatus::NoProvider => FinalizeStatus::NoProvider,
                DomainFinalizeStatus::Timeout => FinalizeStatus::Timeout,
            },
            saw_result: outcome.saw_result(),
        },
    )
    .await
}

async fn send_coordinator_event(
    socket: &mut WebSocket,
    event: LiveCoordinatorEvent,
) -> Result<(), SafeLiveError> {
    match event {
        LiveCoordinatorEvent::Transcript(transcript) => {
            send_message(socket, transcript_message(transcript)).await
        }
        LiveCoordinatorEvent::UtteranceEnd { .. } => Ok(()),
    }
}

fn transcript_message(transcript: LiveTranscript) -> OutboundServerMessage {
    let segment = TranscriptSegment {
        text: transcript.text,
        start_ms: transcript.start_millis,
        duration_ms: transcript.duration_millis,
        confidence: transcript.confidence.map(f64::from),
    };
    match transcript.stability {
        LiveTranscriptStability::Partial => OutboundServerMessage::Partial(segment),
        LiveTranscriptStability::SegmentFinal => OutboundServerMessage::SegmentFinal(segment),
        LiveTranscriptStability::UtteranceFinal => OutboundServerMessage::Final(segment),
    }
}

async fn send_message(
    socket: &mut WebSocket,
    message: OutboundServerMessage,
) -> Result<(), SafeLiveError> {
    let json =
        serialize_server_message(&message).map_err(|_| SafeLiveError::InvalidProviderEvent)?;
    socket
        .send(Message::Text(json.into()))
        .await
        .map_err(|_| SafeLiveError::TransportClosed)
}

async fn fail_socket(socket: &mut WebSocket, error: SafeLiveError, metrics: &GatewayMetrics) {
    metrics.live_failure();
    let _ = send_message(
        socket,
        OutboundServerMessage::Error {
            code: error.code().into(),
            message: error.message().into(),
        },
    )
    .await;
    let _ = socket.send(Message::Close(None)).await;
}

fn recognition_request(config: &ClientConfig) -> LiveRecognitionRequest {
    let (provider, model) = match config.identity {
        LiveIdentity::DeepgramNova3 => ("deepgram", "nova-3"),
        LiveIdentity::ElevenlabsScribeV2Realtime => ("elevenlabs", "scribe_v2_realtime"),
    };
    LiveRecognitionRequest {
        profile: voicetext_speech::application::ports::LiveProfile {
            protocol_version: 2,
            provider: provider.into(),
            model: model.into(),
            language: config.language.clone(),
        },
        sample_rate_hz: match config.audio_format {
            AudioFormat::Opus48Khz => 48_000,
            AudioFormat::PcmS16le16Khz => 16_000,
        },
        channels: 1,
        keyterms: config.keyterms.clone(),
    }
}

enum Raced {
    Client(Option<Result<Message, axum::Error>>),
    Provider(Result<Option<LiveRecognitionEvent>, RecognitionFailure>),
}

struct OpenedSession {
    identity: LiveIdentity,
    decoder: Option<DiscordOpusDecoder>,
    coordinator: LiveCoordinator,
}

enum LoopControl {
    Continue,
    Finalize,
    Close,
}

#[derive(Clone, Copy, Debug)]
enum SafeLiveError {
    ConfigTimeout,
    InvalidConfig,
    InvalidCommand,
    InvalidAudio,
    FrameTooLarge,
    ProfileNotConfigured,
    AudioRuntimeUnavailable,
    PendingAcknowledgements,
    ProviderUnavailable,
    ProviderTerminal,
    ProviderOutcomeUnknown,
    ProviderClosed,
    InvalidProviderEvent,
    TransportClosed,
}

impl SafeLiveError {
    fn from_coordinator(error: &LiveCoordinatorError, phase: &'static str) -> Self {
        if let LiveCoordinatorError::Recognition(failure) = error {
            tracing::warn!(
                phase,
                provider_code = safe_provider_failure_code(failure.code()),
                failure_class = recognition_failure_class(failure),
                "live provider operation failed"
            );
        }
        match error {
            LiveCoordinatorError::Recognition(RecognitionFailure::KnownNotAccepted { .. }) => {
                Self::ProviderUnavailable
            }
            LiveCoordinatorError::Recognition(RecognitionFailure::KnownAcceptedTerminal {
                ..
            }) => Self::ProviderTerminal,
            LiveCoordinatorError::Recognition(RecognitionFailure::UnknownAfterSend { .. }) => {
                Self::ProviderOutcomeUnknown
            }
            LiveCoordinatorError::InvalidAudioFrame => Self::InvalidAudio,
            LiveCoordinatorError::InvalidProviderEvent(_) => Self::InvalidProviderEvent,
            LiveCoordinatorError::Domain(_) => Self::InvalidCommand,
        }
    }

    const fn code(self) -> &'static str {
        match self {
            Self::ConfigTimeout => "CONFIG_TIMEOUT",
            Self::InvalidConfig => "INVALID_CONFIG",
            Self::InvalidCommand => "INVALID_COMMAND",
            Self::InvalidAudio => "INVALID_AUDIO",
            Self::FrameTooLarge => "FRAME_TOO_LARGE",
            Self::ProfileNotConfigured => "PROFILE_NOT_CONFIGURED",
            Self::AudioRuntimeUnavailable => "AUDIO_RUNTIME_UNAVAILABLE",
            Self::PendingAcknowledgements => "PENDING_ACKNOWLEDGEMENTS",
            Self::ProviderUnavailable => "PROVIDER_UNAVAILABLE",
            Self::ProviderTerminal => "PROVIDER_TERMINAL",
            Self::ProviderOutcomeUnknown => "PROVIDER_OUTCOME_UNKNOWN",
            Self::ProviderClosed => "PROVIDER_CLOSED",
            Self::InvalidProviderEvent => "INVALID_PROVIDER_EVENT",
            Self::TransportClosed => "TRANSPORT_CLOSED",
        }
    }

    const fn message(self) -> &'static str {
        match self {
            Self::ConfigTimeout => "Live configuration timed out",
            Self::InvalidConfig => "Live configuration is invalid",
            Self::InvalidCommand => "Live command is invalid",
            Self::InvalidAudio => "Live audio frame is invalid",
            Self::FrameTooLarge => "Live frame exceeds the configured limit",
            Self::ProfileNotConfigured => "Requested live profile is not configured",
            Self::AudioRuntimeUnavailable => "Live audio decoder is unavailable",
            Self::PendingAcknowledgements => "Audio acknowledgements are still pending",
            Self::ProviderUnavailable => "Live provider is temporarily unavailable",
            Self::ProviderTerminal => "Live provider rejected the session",
            Self::ProviderOutcomeUnknown => "Live provider outcome is unknown",
            Self::ProviderClosed => "Live provider closed unexpectedly",
            Self::InvalidProviderEvent => "Live provider returned an invalid event",
            Self::TransportClosed => "Live transport closed",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pcm_requires_complete_sixteen_bit_samples() {
        assert_eq!(decode_audio(&mut None, vec![1, 2]).unwrap(), vec![1, 2]);
        assert!(decode_audio(&mut None, vec![1]).is_err());
        assert!(decode_audio(&mut None, Vec::new()).is_err());
    }

    #[test]
    fn maps_transcript_stability_without_identity_drift() {
        let message = transcript_message(LiveTranscript {
            text: "hello".into(),
            start_millis: 4,
            duration_millis: 5,
            confidence: Some(0.75),
            stability: LiveTranscriptStability::SegmentFinal,
        });
        assert!(matches!(message, OutboundServerMessage::SegmentFinal(_)));
    }

    #[test]
    fn provider_failure_mapping_executes_safe_diagnostics() {
        let failure =
            LiveCoordinatorError::Recognition(RecognitionFailure::KnownAcceptedTerminal {
                code: "ELEVENLABS_LIVE_PROVIDER_ERROR".into(),
                provider_reference: None,
            });
        assert!(matches!(
            SafeLiveError::from_coordinator(&failure, "stream"),
            SafeLiveError::ProviderTerminal
        ));
    }
}
