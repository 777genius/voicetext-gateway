# Local secret files

Create files in this directory with mode `0600`:

- `postgres_password`: one randomly generated PostgreSQL password;
- `postgres_url`: `postgres://voicetext:<password>@postgres:5432/voicetext`;
- `gateway_token`: one random token of at least 32 bytes;
- `deepgram_api_key`: a test or user-owned Deepgram key, required by the default or Deepgram-only
  Compose configuration;
- `elevenlabs_api_key`: a test or user-owned ElevenLabs key, required by the default or
  ElevenLabs-only Compose configuration.

The default Compose configuration enables both providers. For a single-provider deployment, use
`compose.deepgram.yaml` or `compose.elevenlabs.yaml`; the unused key file is then unnecessary and is
not mounted. Never commit these files or pass their contents as command-line arguments.
