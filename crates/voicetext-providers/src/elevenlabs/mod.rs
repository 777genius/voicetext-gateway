//! `ElevenLabs` Scribe v2 speech-to-text adapters.

mod batch;
mod batch_capabilities;
mod batch_keyterms;
mod dto;
mod live;
mod live_dto;
mod live_state;

pub use batch::{ElevenLabsBatchRecognizer, ElevenLabsConfigurationError};
pub use live::{ElevenLabsLiveConfigurationError, ElevenLabsLiveRecognizer};
