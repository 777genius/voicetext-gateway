# Local secret files

Create files in this directory with mode `0600`, owned by the unprivileged account that runs
Compose:

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

Compose bind mounts preserve host ownership, so directly mounting a host `0600` file is not
portable to the gateway's fixed UID/GID `10001`. The supplied `secret-init` service is the portable
mount/init pattern: it has no network, briefly reads the host-owned files, installs copies owned by
`10001:10001` with mode `0400` into the private `gateway-secrets` volume, then exits. The gateway
mounts that volume read-only. Only the gateway service identity can read the copies; provider keys
and the service token never enter the environment. Rootful Docker daemon administrators retain the
usual ability to inspect host files and volumes.

As an alternative on a host with POSIX ACLs, grant only the container identity read access and bind
mount the file read-only, for example `setfacl -m u:10001:r deploy/secrets/gateway_token`. Verify
with `getfacl`, retain mode `0600`, and do not grant a group or `other` access. Rootless engines map
container IDs, so prefer the supplied init-volume pattern unless the engine's host UID mapping is
known and tested.
