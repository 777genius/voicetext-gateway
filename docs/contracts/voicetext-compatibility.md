# VoiceText compatibility contract

The canonical consumer is Discord Meeting Assistant's checked-in TypeScript VoiceText adapter. The
gateway must pass independent black-box tests against that client; matching shapes by inspection is
not sufficient.

## Batch profiles

- Contract v2: `deepgram / nova-3 / multi`.
- Contract v3: `elevenlabs / scribe_v2 / multi`.
- `POST /api/v1/transcribe/batch` accepts the exact deterministic multipart request.
- `GET /api/v1/transcribe/batch/{job_id}` returns the exact pending, failed, or completed identity.
- A 64-character lowercase hexadecimal idempotency key binds the complete request fingerprint.
- Exact replay returns the same job and result. A different fingerprint returns HTTP 409.

## Live profiles

- Protocol v2: `deepgram / nova-3 / multi` or
  `elevenlabs / scribe_v2_realtime / multi`.
- The first frame is config; `ready` with exact provider/model precedes audio.
- Each binary frame is one raw Discord mono 48 kHz Opus packet, not RTP, Ogg, base64, or JSON.
- ACK is emitted only after the corresponding bounded provider write succeeds.
- Finalize begins only after every accepted frame has an ACK.
- `flushed` requires an observed provider result and `saw_result: true`.
- `no_provider` is valid only for a session that accepted no audio.
- `timeout` is a retryable derived-live failure and never changes authoritative batch evidence.

Legacy protocol v1, base64 audio, public-user auth, quota messages, and live resume are not part of
the Discord compatibility surface for V1.
