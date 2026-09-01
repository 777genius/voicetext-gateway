# ADR-0003: Defer Pipecat to a future optional adapter

Status: accepted

## Decision

Pipecat is not a dependency, sidecar, adapter, or provider implementation in the first release.
Native Rust Deepgram and ElevenLabs adapters are the qualified paths.

A future Pipecat bridge may implement the public batch/live application ports only after it passes
the same conformance suite for language, keyterms, timestamps, raw Discord Opus normalization,
partial/final events, provider identity, and explicit finalize completion. Python and Pipecat types
cannot enter domain or application code.
