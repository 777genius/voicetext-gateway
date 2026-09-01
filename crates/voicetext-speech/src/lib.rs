//! Provider-neutral speech-to-text domain and application boundaries.
//!
//! This crate deliberately contains no provider, transport, persistence, configuration, or
//! runtime implementation. Consumers supply those details through application-owned ports.

#![forbid(unsafe_code)]

pub mod application;
pub mod domain;
