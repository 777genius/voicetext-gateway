# syntax=docker/dockerfile:1.18@sha256:dabfc0969b935b2080555ace70ee69a5261af8a8f1b4df97b9e7fbcf6722eddf

FROM rust:1.97.1-bookworm@sha256:0e2bcaef56d041a486784e54104a81aebe0da44bd03019bd70bc0401e42e4a97 AS builder

RUN apt-get update \
    && apt-get install --yes --no-install-recommends build-essential cmake ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /workspace
COPY .cargo ./.cargo
COPY Cargo.toml Cargo.lock rust-toolchain.toml rustfmt.toml ./
COPY architecture ./architecture
COPY crates ./crates
COPY deploy ./deploy
COPY xtask ./xtask
RUN --mount=type=cache,id=voicetext-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=voicetext-target,target=/workspace/target,sharing=locked \
    cargo build --locked --release --package voicetext-gateway --bin voicetext-gateway \
    && cp target/release/voicetext-gateway /workspace/voicetext-gateway

FROM builder AS verification

RUN --mount=type=cache,id=voicetext-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=voicetext-target,target=/workspace/target,sharing=locked \
    cargo fmt --check \
    && cargo xtask verify \
    && cargo clippy --workspace --all-targets --all-features --locked -- -D warnings \
    && cargo doc --workspace --no-deps --locked \
    && cargo test --workspace --locked

FROM debian:bookworm-slim@sha256:abd67ffcfa541b485a3dff59865ab629aa048a6c613e639d36e7456b0b229241 AS runtime

RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 voicetext \
    && useradd --uid 10001 --gid 10001 --no-create-home --shell /usr/sbin/nologin voicetext \
    && install -d --owner voicetext --group voicetext --mode 0700 /var/lib/voicetext/spool

COPY --from=builder /workspace/voicetext-gateway /usr/local/bin/voicetext-gateway

USER 10001:10001
WORKDIR /var/lib/voicetext
EXPOSE 8080

ENV RUST_LOG=info \
    VOICETEXT_HEALTHCHECK_URL=http://127.0.0.1:8080/health/ready

HEALTHCHECK --interval=10s --timeout=3s --start-period=20s --retries=6 \
    CMD ["sh", "-ec", "exec curl --fail --silent --show-error --max-time 2 \"${VOICETEXT_HEALTHCHECK_URL}\""]

ENTRYPOINT ["voicetext-gateway"]
