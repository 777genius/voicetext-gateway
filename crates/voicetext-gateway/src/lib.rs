//! Exact, bounded `VoiceText` transport contracts.
//!
//! Provider SDK, runtime, persistence, and business models deliberately stay outside this crate.

#![forbid(unsafe_code)]

pub mod auth;
pub mod config;
pub mod contracts;
pub mod identity;
pub mod profiles;
pub mod secret;
pub mod server;
pub mod storage;
