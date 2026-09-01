# OSS extraction and secret-safety policy

The public repository starts with a clean history. The private VoiceText SaaS Git history is never
copied or made public.

Only provider-neutral mechanics, provider adapters, bounded audio validation, and independently
reviewed tests may be selectively reimplemented or copied. Before public publication:

1. confirm repository-owner authorization and the `MIT OR Apache-2.0` license;
2. review every extracted file for SaaS, customer, billing, infrastructure, and private endpoint
   coupling;
3. scan the complete working tree and Git history for secrets;
4. reject `.env`, credentials, tokens, private keys, production URLs, customer audio, and logs;
5. generate an SBOM and run dependency license and vulnerability checks;
6. verify that Docker secrets are file-mounted and logs redact authorization and provider bodies.

Provider-backed tests use test-only credentials and synthetic audio. Credential values are never
passed through chat, command arguments, committed fixtures, or test output.
