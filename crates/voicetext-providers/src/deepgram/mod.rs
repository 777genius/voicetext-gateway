//! Deepgram Nova-3 speech-to-text adapters.

mod batch;
mod dto;
mod live;
mod live_dto;
mod timeline;

pub use batch::{DeepgramBatchRecognizer, DeepgramConfigurationError};
pub use live::{DeepgramLiveConfigurationError, DeepgramLiveRecognizer};
