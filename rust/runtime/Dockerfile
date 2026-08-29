# Runtime image — multi-stage Rust build → debian:bookworm-slim (D13).
#
# Unlike the broker, the runtime CANNOT be distroless: it shells out to real CLI
# tools for its hardened surface —
#   /execute   -> $SHELL (default /bin/bash)        [execute.rs]
#   /snapshot  -> `find` | `tar` | `zstd` pipeline  [snapshot.rs]
#   /restore   -> `zstd -d` | `tar -xf`             [snapshot.rs]
#   /files/grep is implemented in Rust (regex), but `grep`/`find` are kept for any
#   tenant `/execute` script that expects a normal POSIX userland.
# So the final stage is `debian:bookworm-slim` + the minimal apt set those code paths
# need (D13: keep a debian base) plus the office toolchain (LibreOffice nogui,
# see stage 3).
#
# Image name/tag are owned by the chart helper `open-websandbox.image`
# (`.Values.sandboxTemplate.runtimeRepository` = `open-websandbox-runtime`) — built
# tagged `open-websandbox-runtime:v0.1.0` and `kind load`-ed for e2e, exactly as the
# Python image was (D12 drop-in: same name/tag, swapped contents).

# syntax=docker/dockerfile:1.7

# --- Stage 1: build deps (cached layer) --------------------------------------
FROM rust:1.98-bookworm AS deps
WORKDIR /ws
COPY Cargo.toml Cargo.lock ./
COPY shared/Cargo.toml shared/Cargo.toml
COPY broker/Cargo.toml broker/Cargo.toml
COPY runtime/Cargo.toml runtime/Cargo.toml
RUN mkdir -p shared/src broker/src runtime/src \
 && printf '#![allow(dead_code)]\n' > shared/src/lib.rs \
 && printf 'fn main() {}\n' > broker/src/main.rs \
 && printf '#![allow(dead_code)]\n' > broker/src/lib.rs \
 && printf 'fn main() {}\n' > runtime/src/main.rs \
 && printf '#![allow(dead_code)]\n' > runtime/src/lib.rs \
 && cargo build --release -p runtime

# --- Stage 2: build the real runtime binary ----------------------------------
FROM rust:1.98-bookworm AS builder
WORKDIR /ws
COPY --from=deps /ws/target target
COPY Cargo.toml Cargo.lock ./
COPY shared shared
COPY broker broker
COPY runtime runtime
RUN touch shared/src/lib.rs broker/src/main.rs broker/src/lib.rs runtime/src/main.rs runtime/src/lib.rs \
 && cargo build --release -p runtime

# --- Stage 3: debian runtime -------------------------------------------------
# bookworm-slim = glibc + apt, the smallest debian that still hosts a real userland.
# `--no-install-recommends` keeps the layer to exactly the packages requested.
FROM debian:bookworm-slim
LABEL org.opencontainers.image.title="open-websandbox-runtime" \
      org.opencontainers.image.description="Rust sandbox runtime (axum + tokio): hardened /execute + /files/* + /snapshot + /restore + terminals."

# CA certs first (apt over HTTPS needs them), then the CLI surface the runtime shells
# out to. coreutils/findutils/grep cover the POSIX tools tenant /execute scripts assume.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      ca-certificates \
      bash \
      tar \
      zstd \
      findutils \
      grep \
      coreutils \
 && rm -rf /var/lib/apt/lists/*

# Office toolchain: LibreOffice nogui variants (headless `soffice` conversions,
# no GUI toolkit deps) + metric-compatible core fonts. Debian's `-nogui` binary
# packages keep the layer ~70 MiB smaller than the full suite and avoid pulling
# X/GTK; installed size is ~750 MiB — the single biggest line in this image, and
# an accepted cost of document support. MPL-2.0 upstream (see NOTICE.md).
# soffice needs a writable profile dir; the sandbox HOME (/home/sandbox) is a
# mounted volume, and conversions can pin it via
#   -env:UserInstallation=file:///tmp/lo
# when run from tenant /execute scripts.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      libreoffice-nogui \
      fonts-liberation \
      fonts-dejavu-core \
 && rm -rf /var/lib/apt/lists/*

# Non-root uid 1000 — matches the chart's sandboxTemplate.uid (1000) and the Python
# runtime's `sandbox` user, so the pod securityContext (runAsUser: 1000) binds the
# same identity the emptyDirs are fsGroup-chowned to.
RUN useradd -u 1000 -m -d /home/sandbox -s /bin/bash sandbox \
 && mkdir -p /workspace \
 && chown -R 1000:1000 /workspace /home/sandbox

# The release binary from stage 2. Owned by root (read+exec for all); the pod runs
# it as uid 1000 via the chart securityContext, so it must NOT live under /workspace
# (an emptyDir mount) or /home/sandbox (also mounted). /usr/local/bin is image-local.
COPY --from=builder /ws/target/release/runtime /usr/local/bin/runtime

# Env parity with the Python runtime image (D12). The Rust config (runtime/src/config.rs)
# reads the SAME names with the SAME defaults, so these are belt-and-braces for tenant
# scripts + standalone runs; the chart's SandboxTemplate env block overrides the
# per-sandbox knobs (MAX_TIMEOUT/MAX_PROCS/...) at runtime.
ENV WORKDIR=/workspace \
    SHELL=/bin/bash \
    MAX_OUTPUT_BYTES=1048576 \
    DEFAULT_TIMEOUT=120 \
    MAX_TIMEOUT=600 \
    MAX_PROCS=256
# RUNTIME_KEY_FILE is intentionally NOT set as an env var: it would surface in
# /proc/1/environ (the isolation e2e flags KEY-named env vars). The Rust runtime's
# built-in default (/etc/runtime-key/api-key) is used instead — same as the Python
# runtime, which also omits this env (issue #50: no credential-ish env leaks).

WORKDIR /workspace
USER 1000
EXPOSE 8888
ENTRYPOINT ["/usr/local/bin/runtime"]
