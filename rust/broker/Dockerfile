# Broker image — multi-stage Rust build → distroless (D13).
#
# The broker is a pure control-plane binary: it talks to the kube API (in-cluster
# service-account token + projected CA) and HTTP-proxies to the router/runtime. It
# shells out to NOTHING (no tar/zstd/bash/grep — those live in the runtime image), so
# a distroless final stage is the smallest, most defensible surface. The only runtime
# asset beyond the static binary is a CA bundle, kept for any outbound TLS the kube-rs
# rustls chain does not satisfy from the projected service-account CA (e.g. an external
# OTLP/S3 endpoint presented with a public cert).
#
# Image name/tag are owned by the chart helper `open-websandbox.image`
# (repo `open-websandbox-broker`) — this Dockerfile is built tagged
# `open-websandbox-broker:v0.1.0` and `kind load`-ed for e2e, exactly as the Python
# image was (D12 drop-in: same name/tag, swapped contents).

# syntax=docker/dockerfile:1.7

# --- Stage 1: build deps (cached layer) --------------------------------------
# Resolve + compile the dependency graph ONCE into a reusable layer so source-only
# changes rebuild in seconds. Cargo's fingerprinting makes this a no-op when
# Cargo.lock is unchanged across builds.
FROM rust:1.97-bookworm AS deps
WORKDIR /ws
# Copy the lockfile + every manifest first so a source edit does not invalidate the
# deps cache. (Cargo needs the workspace root + member manifests + lock to plan.)
COPY Cargo.toml Cargo.lock ./
COPY shared/Cargo.toml shared/Cargo.toml
COPY broker/Cargo.toml broker/Cargo.toml
COPY runtime/Cargo.toml runtime/Cargo.toml
# Stub the crates so `cargo build` can compile deps without the real sources. `mkdir`
# + empty lib.rs/bin is enough; the real sources are copied in the builder stage.
RUN mkdir -p shared/src broker/src runtime/src \
 && printf '#![allow(dead_code)]\n' > shared/src/lib.rs \
 && printf 'fn main() {}\n' > broker/src/main.rs \
 && printf '#![allow(dead_code)]\n' > broker/src/lib.rs \
 && printf 'fn main() {}\n' > runtime/src/main.rs \
 && printf '#![allow(dead_code)]\n' > runtime/src/lib.rs \
 && cargo build --release -p broker

# --- Stage 2: build the real broker binary -----------------------------------
FROM rust:1.97-bookworm AS builder
WORKDIR /ws
# Re-apply the deps layer, then overlay the real sources and rebuild. Touching the
# broker sources recompiles only `broker` (deps + shared are cached from stage 1).
COPY --from=deps /ws/target target
COPY Cargo.toml Cargo.lock ./
COPY shared shared
COPY broker broker
COPY runtime runtime
RUN touch shared/src/lib.rs broker/src/main.rs broker/src/lib.rs runtime/src/main.rs runtime/src/lib.rs \
 && cargo build --release -p broker

# --- Stage 3: distroless runtime ---------------------------------------------
# `cc-debian12` ships glibc + ca-certificates + a non-root user `nonroot` (uid 65532)
# and nothing else — no shell, no package manager, no interpreters. The broker needs
# exactly that: a dynamically-linked Rust binary (links glibc) + a CA bundle.
FROM gcr.io/distroless/cc-debian12:nonroot
LABEL org.opencontainers.image.title="open-websandbox-broker" \
      org.opencontainers.image.description="Rust sandbox broker (axum + kube-rs) — Open WebUI front door + Kubernetes Sandbox lifecycle."
COPY --from=builder /ws/target/release/broker /usr/local/bin/broker
# distroless cc-debian12 already bundles ca-certificates; no extra install step.
USER nonroot:nonroot
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/broker"]
