//! Bounded `VoiceText` protocol v2 WebSocket transport.

use super::error::GatewayHttpError;
use super::live_error::SafeLiveError;
use super::live_request::recognition_request;
use super::metrics::GatewayMetrics;
use super::qualification_observation::{
    LiveObservationTracker, OBSERVATION_WRITE_TIMEOUT, ObservationProfile,
};
use super::state::GatewayState;
use crate::auth::authenticate;
use crate::contracts::live::{
    AudioFormat, ClientCommand, ClientConfig, FinalizeStatus, LiveIdentity, TranscriptSegment,
    parse_client_command, parse_client_config,
};
use crate::contracts::live_outbound::{OutboundServerMessage, serialize_server_message};
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::HeaderMap;
use axum::response::Response;
use std::time::Duration;
use tokio::time::{Instant, sleep_until, timeout};
use uuid::Uuid;
use voicetext_audio::discord_opus::DiscordOpusDecoder;
use voicetext_speech::application::live::{LiveCoordinator, LiveCoordinatorEvent};
use voicetext_speech::application::live_capabilities::{LiveCapabilityRequest, LiveInputFormat};
use voicetext_speech::application::ports::{
    LiveRecognitionEvent, LiveTranscript, LiveTranscriptStability, RecognitionFailure,
};
use voicetext_speech::domain::live::FinalizeStatus as DomainFinalizeStatus;
const FINALIZE_QUIESCENCE: Duration = Duration::from_millis(100);
const PROVIDER_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const PROVIDER_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const PROVIDER_CLOSE_TIMEOUT: Duration = Duration::from_secs(5);
const CLIENT_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const LIVE_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const LIVE_SESSION_TIMEOUT: Duration = Duration::from_hours(4);
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
            close_socket(&mut socket).await;
            return;
        }
    };
    let gateway_session_id = Uuid::new_v4();
    let mut observation = state.live_observations().enabled().then(|| {
        LiveObservationTracker::new(
            opened.client_session_id,
            gateway_session_id,
            opened.profile.clone(),
        )
    });
    let terminal_status = if send_message(
        &mut socket,
        OutboundServerMessage::Ready {
            session_id: gateway_session_id,
            identity: opened.identity,
        },
    )
    .await
    .is_err()
    {
        state.metrics().live_failure();
        "ready_delivery_failed".to_owned()
    } else {
        stream(&mut socket, &state, &mut opened, observation.as_mut()).await
    };
    if let Some(observation) = observation {
        let operation = timeout(
            PROVIDER_CLOSE_TIMEOUT,
            opened.coordinator.provider_operation(),
        )
        .await
        .ok()
        .flatten();
        let record = observation.finish(operation, terminal_status);
        match timeout(
            OBSERVATION_WRITE_TIMEOUT,
            state.live_observations().observe_live(record),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(failure)) => observation_failure(&state, failure.0),
            Err(_) => observation_failure(&state, "QUALIFICATION_WRITE_TIMEOUT"),
        }
    }
    close_coordinator(&mut opened.coordinator).await;
    close_socket(&mut socket).await;
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
    let keyterms = config
        .keyterms
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    let input_format = match config.audio_format {
        AudioFormat::Opus48Khz => LiveInputFormat::Opus48KhzMono,
        AudioFormat::PcmS16le16Khz => LiveInputFormat::PcmS16Le16KhzMono,
    };
    factory
        .capabilities()
        .validate(&LiveCapabilityRequest {
            timestamps: true,
            finalized_events: true,
            language_hint: Some(&config.language),
            diarization: false,
            key_terms: &keyterms,
            input_format,
            input_frame_bytes: 2,
        })
        .map_err(|_| SafeLiveError::InvalidConfig)?;
    let request = recognition_request(&config);
    let profile = ObservationProfile {
        contract_version: request.profile.protocol_version,
        provider: request.profile.provider.clone(),
        model: request.profile.model.clone(),
        language: request.profile.language.clone(),
    };
    let coordinator = timeout(
        PROVIDER_CONNECT_TIMEOUT,
        LiveCoordinator::open(factory.as_ref(), request),
    )
    .await
    .map_err(|_| SafeLiveError::ProviderOperationTimeout)?
    .map_err(|error| SafeLiveError::from_coordinator(&error, "open"))?;
    Ok(OpenedSession {
        identity: config.identity,
        client_session_id: config.client_session_id,
        profile,
        decoder,
        coordinator,
    })
}

async fn stream(
    socket: &mut WebSocket,
    state: &GatewayState,
    opened: &mut OpenedSession,
    mut observation: Option<&mut LiveObservationTracker>,
) -> String {
    let limits = state.limits();
    let session_deadline = Instant::now() + LIVE_SESSION_TIMEOUT;
    let mut idle_deadline = Instant::now() + LIVE_IDLE_TIMEOUT;

    let mut accepted_audio = false;
    loop {
        let raced = {
            let provider_read = opened.coordinator.receive_provider_event();
            tokio::pin!(provider_read);
            tokio::select! {
                client = socket.recv() => Raced::Client(client),
                provider = &mut provider_read => Raced::Provider(provider),
                () = sleep_until(idle_deadline) => Raced::IdleTimeout,
                () = sleep_until(session_deadline) => Raced::SessionTimeout,
            }
        };
        let control = match raced {
            Raced::Client(client) => {
                idle_deadline = Instant::now() + LIVE_IDLE_TIMEOUT;
                handle_client_frame(
                    client,
                    socket,
                    opened,
                    limits.live_frame_bytes,
                    &mut accepted_audio,
                    state.metrics(),
                    observation.as_deref_mut(),
                )
                .await
            }
            Raced::Provider(provider) => {
                idle_deadline = Instant::now() + LIVE_IDLE_TIMEOUT;
                handle_provider_event(
                    provider,
                    socket,
                    &mut opened.coordinator,
                    observation.as_deref_mut(),
                )
                .await
            }
            Raced::IdleTimeout | Raced::SessionTimeout => Err(SafeLiveError::SessionTimeout),
        };
        match control {
            Ok(LoopControl::Continue) => {}
            Ok(LoopControl::Close) => return "client_close".into(),
            Ok(LoopControl::Finalize) => {
                let finalized = finalize(
                    socket,
                    &mut opened.coordinator,
                    accepted_audio,
                    limits.finalize_timeout,
                    observation.as_deref_mut(),
                )
                .await;
                match finalized {
                    Ok(status) => return format!("finalize_{status:?}").to_ascii_lowercase(),
                    Err(error) => {
                        let terminal = error.code().to_owned();
                        fail_socket(socket, error, state.metrics()).await;
                        return terminal;
                    }
                }
            }
            Err(error) => {
                let terminal = error.code().to_owned();
                fail_socket(socket, error, state.metrics()).await;
                return terminal;
            }
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
    opened: &mut OpenedSession,
    maximum: usize,
    accepted_audio: &mut bool,
    metrics: &GatewayMetrics,
    mut observation: Option<&mut LiveObservationTracker>,
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
            let raw = frame.to_vec();
            let pcm = decode_audio(&mut opened.decoder, &raw)?;
            if let Some(observation) = observation.as_deref_mut() {
                observation.accept_frame();
            }
            let sequence = write_audio_or_cancel(socket, &mut opened.coordinator, pcm).await?;
            if let Some(observation) = observation.as_deref_mut() {
                observation.provider_written(sequence.get());
            }
            metrics.live_frame();
            send_message(
                socket,
                OutboundServerMessage::Ack {
                    seq: sequence.get(),
                },
            )
            .await?;
            opened
                .coordinator
                .ack_sent(sequence)
                .map_err(|error| SafeLiveError::from_coordinator(&error, "ack"))?;
            if let Some(observation) = observation {
                observation.ack_sent(sequence.get(), &raw);
            }
            *accepted_audio = true;
            Ok(LoopControl::Continue)
        }
        Message::Text(text) => match parse_client_command(&text) {
            Ok(ClientCommand::Finalize) => Ok(LoopControl::Finalize),
            Ok(ClientCommand::Close) => Ok(LoopControl::Close),
            Err(_) => Err(SafeLiveError::InvalidCommand),
        },
        Message::Ping(payload) => {
            timeout(CLIENT_WRITE_TIMEOUT, socket.send(Message::Pong(payload)))
                .await
                .map_err(|_| SafeLiveError::TransportClosed)?
                .map_err(|_| SafeLiveError::TransportClosed)?;
            Ok(LoopControl::Continue)
        }
        Message::Pong(_) => Ok(LoopControl::Continue),
        Message::Close(_) => Ok(LoopControl::Close),
    }
}

async fn write_audio_or_cancel(
    socket: &mut WebSocket,
    coordinator: &mut LiveCoordinator,
    pcm: Vec<u8>,
) -> Result<voicetext_speech::domain::live::RawAudioSequence, SafeLiveError> {
    let write = coordinator.provider_write(pcm);
    tokio::pin!(write);
    let deadline = Instant::now() + PROVIDER_WRITE_TIMEOUT;
    loop {
        tokio::select! {
            client = socket.recv() => match client {
                None | Some(Err(_) | Ok(Message::Close(_))) => {
                    return Err(SafeLiveError::TransportClosed);
                }
                Some(Ok(Message::Ping(payload))) => {
                    timeout(CLIENT_WRITE_TIMEOUT, socket.send(Message::Pong(payload)))
                        .await
                        .map_err(|_| SafeLiveError::TransportClosed)?
                        .map_err(|_| SafeLiveError::TransportClosed)?;
                }
                Some(Ok(Message::Pong(_))) => {}
                Some(Ok(Message::Text(_) | Message::Binary(_))) => {
                    return Err(SafeLiveError::InvalidCommand);
                }
            },
            result = &mut write => {
                return result.map_err(|error| SafeLiveError::from_coordinator(&error, "write"));
            }
            () = sleep_until(deadline) => return Err(SafeLiveError::ProviderOperationTimeout),
        }
    }
}

fn decode_audio(
    decoder: &mut Option<DiscordOpusDecoder>,
    frame: &[u8],
) -> Result<Vec<u8>, SafeLiveError> {
    match decoder {
        Some(decoder) => decoder
            .decode(frame)
            .map(|decoded| decoded.pcm_s16le)
            .map_err(|_| SafeLiveError::InvalidAudio),
        None if !frame.is_empty() && (frame.len() & 1) == 0 => Ok(frame.to_vec()),
        None => Err(SafeLiveError::InvalidAudio),
    }
}

async fn handle_provider_event(
    received: Result<Option<LiveRecognitionEvent>, RecognitionFailure>,
    socket: &mut WebSocket,
    coordinator: &mut LiveCoordinator,
    observation: Option<&mut LiveObservationTracker>,
) -> Result<LoopControl, SafeLiveError> {
    if let (Some(observation), Ok(Some(event))) = (observation, &received) {
        observation.provider_event(event);
    }
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
    mut observation: Option<&mut LiveObservationTracker>,
) -> Result<FinalizeStatus, SafeLiveError> {
    if coordinator.pending_audio_count() != 0 {
        return Err(SafeLiveError::PendingAcknowledgements);
    }
    begin_finalize_or_cancel(socket, coordinator, maximum_wait).await?;

    let deadline = Instant::now() + maximum_wait;
    let mut saw_result = false;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let wait = if saw_result {
            remaining.min(FINALIZE_QUIESCENCE)
        } else {
            remaining
        };
        let received = {
            let provider_read = coordinator.receive_provider_event();
            tokio::pin!(provider_read);
            tokio::select! {
                client = socket.recv() => match client {
                    None | Some(Err(_) | Ok(Message::Close(_))) => {
                        return Err(SafeLiveError::TransportClosed);
                    }
                    Some(Ok(_)) => return Err(SafeLiveError::InvalidCommand),
                },
                provider = &mut provider_read => provider,
                () = tokio::time::sleep(wait) => break,
            }
        };
        let ended = matches!(received, Ok(None));
        let observed = matches!(
            received,
            Ok(Some(LiveRecognitionEvent::FinalizeResultObserved))
        );
        if let (Some(observation), Ok(Some(event))) = (observation.as_deref_mut(), &received) {
            observation.provider_event(event);
        }
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
    let status = match outcome.status() {
        DomainFinalizeStatus::Flushed => FinalizeStatus::Flushed,
        DomainFinalizeStatus::NoProvider => FinalizeStatus::NoProvider,
        DomainFinalizeStatus::Timeout => FinalizeStatus::Timeout,
    };
    send_message(
        socket,
        OutboundServerMessage::FinalizeComplete {
            status,
            saw_result: outcome.saw_result(),
        },
    )
    .await?;
    Ok(status)
}

async fn begin_finalize_or_cancel(
    socket: &mut WebSocket,
    coordinator: &mut LiveCoordinator,
    maximum_wait: Duration,
) -> Result<(), SafeLiveError> {
    let finalize = coordinator.begin_finalize();
    tokio::pin!(finalize);
    let deadline = Instant::now() + PROVIDER_WRITE_TIMEOUT.min(maximum_wait);
    tokio::select! {
        client = socket.recv() => match client {
            None | Some(Err(_) | Ok(Message::Close(_))) => {
                Err(SafeLiveError::TransportClosed)
            }
            Some(Ok(_)) => Err(SafeLiveError::InvalidCommand),
        },
        result = &mut finalize => result
            .map_err(|error| SafeLiveError::from_coordinator(&error, "finalize_begin")),
        () = sleep_until(deadline) => Err(SafeLiveError::ProviderOperationTimeout),
    }
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
    timeout(
        CLIENT_WRITE_TIMEOUT,
        socket.send(Message::Text(json.into())),
    )
    .await
    .map_err(|_| SafeLiveError::TransportClosed)?
    .map_err(|_| SafeLiveError::TransportClosed)
}

async fn close_coordinator(coordinator: &mut LiveCoordinator) {
    let _ = timeout(PROVIDER_CLOSE_TIMEOUT, coordinator.close()).await;
}

async fn close_socket(socket: &mut WebSocket) {
    let _ = timeout(CLIENT_WRITE_TIMEOUT, socket.send(Message::Close(None))).await;
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
}

fn observation_failure(state: &GatewayState, code: &'static str) {
    state.metrics().qualification_observation_failure();
    tracing::warn!(code, "qualification observation missing");
}

enum Raced {
    Client(Option<Result<Message, axum::Error>>),
    Provider(Result<Option<LiveRecognitionEvent>, RecognitionFailure>),
    IdleTimeout,
    SessionTimeout,
}

struct OpenedSession {
    identity: LiveIdentity,
    client_session_id: Uuid,
    profile: ObservationProfile,
    decoder: Option<DiscordOpusDecoder>,
    coordinator: LiveCoordinator,
}

enum LoopControl {
    Continue,
    Finalize,
    Close,
}

#[cfg(test)]
mod tests;
