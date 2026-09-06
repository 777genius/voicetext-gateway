# Inherited gateway secret descriptors

Linux launchers may set exactly these four credential slots:

| Named file | Inherited descriptor |
| --- | --- |
| `VOICETEXT_BEARER_TOKEN_FILE` | `VOICETEXT_BEARER_TOKEN_FD` |
| `VOICETEXT_POSTGRES_URL_FILE` | `VOICETEXT_POSTGRES_URL_FD` |
| `VOICETEXT_DEEPGRAM_API_KEY_FILE` | `VOICETEXT_DEEPGRAM_API_KEY_FD` |
| `VOICETEXT_ELEVENLABS_API_KEY_FILE` | `VOICETEXT_ELEVENLABS_API_KEY_FD` |

FILE and FD are mutually exclusive per slot. Bearer and database require a source;
absent provider sources disable the provider. FD values are canonical decimal integers
from 3 through 2147483646, without signs, whitespace or leading zeroes. Repeated numbers
are rejected. Other platforms reject FD settings. Healthcheck does not capture secrets.
Named-file checks are unchanged: even `/proc/self/fd/N` remains a rejected symlink.

The normal process entrypoint parses configuration and captures the complete handoff
before tracing, crypto, Tokio, startup I/O or threads. Each input must be a regular file
owned by the effective UID, mode exactly 0400, opened O_RDONLY without O_PATH, with
one or zero links and raw size 1–16384 bytes. The unlinked database inode is accepted.
All inputs are validated before F_DUPFD_CLOEXEC allocation above the highest input.
Duplicates are authenticated against the original metadata. Original handoff handles
are transferred into owned Files and closed once; no close is retried. Composition
owns the duplicates and drops them after loading or on error.

Reads start at offset zero using positional reads, preserving shared parent offsets.
Device, inode, size, link count, owner, mode, mtime and ctime (including nanoseconds)
must match capture, pre-read and post-read metadata. Reads are bounded. Existing
single-terminal-LF, machine-token and UTF-8 text rules apply. Temporary plaintext is
zeroized by RAII on all paths; retained text is redacted and zeroized on drop.

The sole unsafe exception is `linux_capture` in the binary's descriptor helper.
It uses cached libc for raw validity/flag checks and duplication, then transfers fresh
or launcher-owned descriptors into standard owned Files. Library `forbid(unsafe_code)`
and workspace `deny(unsafe_code)` remain enabled. The call requires single-threaded
entrypoint custody, with no competing owner of handoff numbers.

The authenticated launcher's slot assignment supplies provenance. Metadata and a number
do not prove provenance or immutability against retained writers. The launcher must
finish writable preparation and descendant cleanup before release; metadata stability
does not replace kernel seals. No pathname reopening, secret restaging, provider
fallback, additional endpoint or testing override is introduced.
