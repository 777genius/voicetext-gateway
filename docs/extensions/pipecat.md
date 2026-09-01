# Future Pipecat adapter

Pipecat is intentionally not implemented in V1. A future adapter is expected to run as an optional
Python sidecar and implement the same provider-neutral application ports as native adapters.

It must pass the complete provider conformance suite before release, including batch/live capability
separation, explicit provider identity, multilingual configuration, repeated keyterms, timestamps,
partial and immutable final events, raw Discord Opus normalization, and a truthful terminal finalize
acknowledgement. An adapter that cannot prove a capability must not advertise it.

No fork is assumed. If an upstream library does not expose a required terminal event, the adapter
remains experimental until a narrow wrapper or accepted upstream change passes conformance.

