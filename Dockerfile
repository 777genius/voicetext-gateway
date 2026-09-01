# syntax=docker/dockerfile:1.12@sha256:93bfd3b68c109427185cd78b4779fc82b484b0b7618e36d0f104d4d801e66d25

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
    && apt-get install --yes --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 voicetext \
    && useradd --uid 10001 --gid 10001 --no-create-home --shell /usr/sbin/nologin voicetext \
    && install -d --owner voicetext --group voicetext --mode 0700 /var/lib/voicetext/spool

COPY --from=builder /workspace/voicetext-gateway /usr/local/bin/voicetext-gateway

USER 10001:10001
WORKDIR /var/lib/voicetext
EXPOSE 8080

ENV RUST_LOG=info

HEALTHCHECK --interval=10s --timeout=3s --start-period=20s --retries=6 \
    CMD ["voicetext-gateway", "healthcheck"]

ENTRYPOINT ["voicetext-gateway"]
