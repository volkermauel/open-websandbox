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
#
# Workbench profile (openspec/changes/2026-08-29-workbench-toolchain): stage 3 is
# additionally a curated ~4 GB workbench toolchain driven by runtime/tools.json
# (single source of truth; expanded via runtime/gen-manifest.py so the image and
# the LLM capability manifest can never drift) plus pinned tarball tools
# (Node 22 LTS / PowerShell 7 / .NET SDK 8 / CFR / duckdb CLI / PSAppDeployToolkit)
# and a sudo-apt-only whitelist (see docs/toolchain.md + docs/security.md).

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
# python3 is the bootstrap for the workbench generator (gen-manifest.py expands
# tools.json into the package lists below).
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      ca-certificates \
      bash \
      tar \
      zstd \
      findutils \
      grep \
      coreutils \
      python3 \
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

# --- Workbench toolchain (openspec/changes/2026-08-29-workbench-toolchain) ----
# tools.json is the SINGLE SOURCE OF TRUTH: gen-manifest.py --print-apt/--print-pip
# expand it into the installs below, and the same file renders the capability
# manifest the LLM sees via /system. Adding a tool = edit tools.json only.
COPY runtime/tools.json /tmp/workbench/tools.json
COPY runtime/gen-manifest.py /tmp/workbench/gen-manifest.py
COPY runtime/sandbox-tools.sh /tmp/workbench/sandbox-tools.sh

# Toolchain apt set. unrar lives in the `non-free` component (freeware license,
# see NOTICE.md) — bookworm's deb822 sources only enable `main` by default.
RUN sed -i 's/^Components: main$/Components: main non-free/' /etc/apt/sources.list.d/debian.sources \
 && apt-get update \
 && apt-get install -y --no-install-recommends \
      $(python3 /tmp/workbench/gen-manifest.py --tools /tmp/workbench/tools.json --print-apt) \
      sudo \
 && rm -rf /var/lib/apt/lists/*

# System-wide Python science stack, baked at build (no first-chat pip latency).
# `--break-system-packages` installs into dist-packages; the EXTERNALLY-MANAGED
# marker is then REMOVED so tenant `pip install --user` / `--target` works
# (PEP-668 relief — documented in docs/toolchain.md). No pip prefix/cache/env
# forcing: tenants get plain upstream pip + taught recipes.
RUN python3 -m pip install --no-cache-dir --break-system-packages \
      $(python3 /tmp/workbench/gen-manifest.py --tools /tmp/workbench/tools.json --print-pip) \
 && rm -f /usr/lib/python3.*/EXTERNALLY-MANAGED \
 && ln -sf /usr/bin/fdfind /usr/local/bin/fd

# Node 22 LTS — official tarball, NOT apt (bookworm's nodejs is 18 = EOL).
# Pinned VERSION + SHA256; /opt/node/bin is on PATH via the ENV block below.
# Tarball tools below are arch-pinned: buildx sets TARGETARCH per platform
# (release.yml builds natively per arch, issue #175). Each case picks the matching
# upstream artifact + SHA256 — never a hardcoded linux-x64 (v0.1.6 arm64 release died
# executing the x86-64 node binary on aarch64). Node/.NET name assets linux-x64/arm64;
# duckdb names them amd64/arm64.
ARG TARGETARCH
ARG NODE_VERSION=v22.23.2
ARG NODE_SHA256_AMD64=d60acfe00a2932254bb0ad20e01b0d74397a0875595de719654b214f4b03f307
ARG NODE_SHA256_ARM64=fff4078c5def658577f92c88db7db3bc0072924bfb93fe52c1e744a54e94abb8
RUN set -eu \
 && case "${TARGETARCH}" in \
      amd64) triple=linux-x64;   sha="${NODE_SHA256_AMD64}" ;; \
      arm64) triple=linux-arm64; sha="${NODE_SHA256_ARM64}" ;; \
      *) echo "unsupported TARGETARCH: ${TARGETARCH}" >&2; exit 1 ;; \
    esac \
 && cd /tmp \
 && curl -fsSLO "https://nodejs.org/dist/${NODE_VERSION}/node-${NODE_VERSION}-${triple}.tar.xz" \
 && echo "${sha}  node-${NODE_VERSION}-${triple}.tar.xz" | sha256sum -c - \
 && mkdir -p /opt/node \
 && tar -xJf "node-${NODE_VERSION}-${triple}.tar.xz" -C /opt/node --strip-components=1 \
 && rm -f "node-${NODE_VERSION}-${triple}.tar.xz" \
 && export PATH=/opt/node/bin:$PATH \
 && corepack enable \
 && COREPACK_ENABLE_DOWNLOAD_PROMPT=0 corepack prepare pnpm@latest --activate \
 && COREPACK_ENABLE_DOWNLOAD_PROMPT=0 corepack prepare yarn@stable --activate

# PowerShell 7 stable — pinned tarball into /opt/powershell (on PATH).
ARG PWSH_VERSION=v7.6.5
ARG PWSH_SHA256_AMD64=b34ab3b19acac1d3d4d0d3cfdb02acf62f457b0b6a962ff008132033f7566844
ARG PWSH_SHA256_ARM64=ed4084f215d8bce2edd23aa7cb1f1e7b0818e41363a635a22065d2701b6141df
RUN set -eu \
 && case "${TARGETARCH}" in \
      amd64) triple=linux-x64;   sha="${PWSH_SHA256_AMD64}" ;; \
      arm64) triple=linux-arm64; sha="${PWSH_SHA256_ARM64}" ;; \
      *) echo "unsupported TARGETARCH: ${TARGETARCH}" >&2; exit 1 ;; \
    esac \
 && cd /tmp \
 && curl -fsSLO "https://github.com/PowerShell/PowerShell/releases/download/${PWSH_VERSION}/powershell-${PWSH_VERSION#v}-${triple}.tar.gz" \
 && echo "${sha}  powershell-${PWSH_VERSION#v}-${triple}.tar.gz" | sha256sum -c - \
 && mkdir -p /opt/powershell \
 && tar -xzf "powershell-${PWSH_VERSION#v}-${triple}.tar.gz" -C /opt/powershell \
 && rm -f "powershell-${PWSH_VERSION#v}-${triple}.tar.gz"

# .NET SDK 8 — pinned tarball into /opt/dotnet (DOTNET_ROOT; on PATH).
# dotnet tools install into /opt/dotnet-tools (on PATH, DOTNET_CLI_HOME=/opt):
#   ilspycmd pinned @ 9.1.0.7988 (10.x+ packages target newer .NET and fail on SDK 8).
#   wix: NOT installed — the `wix` dotnet tool warns at runtime that only
#   Windows is supported ("All behavior after this point is undefined").
#   Debian does not package `wixl` either (bookworm, backports and trixie all
#   ship msitools WITHOUT it — verified empirically): PSADT + nsis + msitools'
#   msiextract/msibuild are the supported Windows-packaging paths
#   (docs/toolchain.md).
ARG DOTNET_VERSION=8.0.424
ARG DOTNET_SHA256_AMD64=1a0f577b530d52e54c5a53ea0b578fd1ebe421b11bfbf9b29a5bdc413bdbad64
ARG DOTNET_SHA256_ARM64=fc86992aec3536e60383b4decbdfa762f16171b5d52e085d8c0a9491577f475d
RUN set -eu \
 && case "${TARGETARCH}" in \
      amd64) triple=linux-x64;   sha="${DOTNET_SHA256_AMD64}" ;; \
      arm64) triple=linux-arm64; sha="${DOTNET_SHA256_ARM64}" ;; \
      *) echo "unsupported TARGETARCH: ${TARGETARCH}" >&2; exit 1 ;; \
    esac \
 && cd /tmp \
 && curl -fsSLO "https://builds.dotnet.microsoft.com/dotnet/Sdk/${DOTNET_VERSION}/dotnet-sdk-${DOTNET_VERSION}-${triple}.tar.gz" \
 && echo "${sha}  dotnet-sdk-${DOTNET_VERSION}-${triple}.tar.gz" | sha256sum -c - \
 && mkdir -p /opt/dotnet /opt/dotnet-tools \
 && tar -xzf "dotnet-sdk-${DOTNET_VERSION}-${triple}.tar.gz" -C /opt/dotnet \
 && rm -f "dotnet-sdk-${DOTNET_VERSION}-${triple}.tar.gz" \
 && export DOTNET_CLI_HOME=/opt DOTNET_ROOT=/opt/dotnet PATH=/opt/dotnet:$PATH DOTNET_CLI_TELEMETRY_OPTOUT=1 DOTNET_NOLOGO=1 \
 && dotnet tool install --tool-path /opt/dotnet-tools ilspycmd --version 9.1.0.7988 \
 && chmod 0777 /opt/.dotnet   # CLI-home sentinel dir: stay writable for uid 1000 at runtime

# PSAppDeployToolkit module (pinned, ModuleOnly zip) into /opt/psmodules
# (PSModulePath via ENV); CFR java decompiler jar -> /opt/tools/cfr.jar with a
# `cfr` wrapper; duckdb CLI (pinned release) -> /opt/tools.
ARG PSADT_VERSION=4.1.8
ARG PSADT_SHA256=80b45fb8e518ee98774e16e44599d470e4069a0695a6a359022f954edaa83360
ARG CFR_VERSION=0.152
ARG CFR_SHA256=f686e8f3ded377d7bc87d216a90e9e9512df4156e75b06c655a16648ae8765b2
ARG DUCKDB_VERSION=v1.5.5
ARG DUCKDB_SHA256_AMD64=08c0ca117111fcede14239d0093792352befdc174218c344d232c13279643d05
ARG DUCKDB_SHA256_ARM64=02163197027a42149147364d31fa67cac82108517a4be43304a1cc226eaef07a
RUN cd /tmp \
 && curl -fsSLO "https://github.com/PSAppDeployToolkit/PSAppDeployToolkit/releases/download/${PSADT_VERSION}/PSAppDeployToolkit_ModuleOnly.zip" \
 && echo "${PSADT_SHA256}  PSAppDeployToolkit_ModuleOnly.zip" | sha256sum -c - \
 && mkdir -p /opt/psmodules /opt/tools \
 && unzip -q PSAppDeployToolkit_ModuleOnly.zip -d /tmp/psadt \
 && mv /tmp/psadt/PSAppDeployToolkit /opt/psmodules/ \
 && rm -rf /tmp/psadt PSAppDeployToolkit_ModuleOnly.zip \
 && curl -fsSLO "https://github.com/leibnitz27/cfr/releases/download/${CFR_VERSION}/cfr-${CFR_VERSION}.jar" \
 && echo "${CFR_SHA256}  cfr-${CFR_VERSION}.jar" | sha256sum -c - \
 && mv "cfr-${CFR_VERSION}.jar" /opt/tools/cfr.jar \
 && printf '#!/bin/sh\nexec java -jar /opt/tools/cfr.jar "$@"\n' > /usr/local/bin/cfr \
 && chmod 0755 /usr/local/bin/cfr \
 && case "${TARGETARCH}" in \
      amd64) darch=amd64; dsha="${DUCKDB_SHA256_AMD64}" ;; \
      arm64) darch=arm64; dsha="${DUCKDB_SHA256_ARM64}" ;; \
      *) echo "unsupported TARGETARCH: ${TARGETARCH}" >&2; exit 1 ;; \
    esac \
 && curl -fsSLO "https://github.com/duckdb/duckdb/releases/download/${DUCKDB_VERSION}/duckdb_cli-linux-${darch}.zip" \
 && echo "${dsha}  duckdb_cli-linux-${darch}.zip" | sha256sum -c - \
 && unzip -j -o "duckdb_cli-linux-${darch}.zip" duckdb -d /opt/tools \
 && chmod 0755 /opt/tools/duckdb \
 && rm -f "duckdb_cli-linux-${darch}.zip"

# Non-root uid 1000 — matches the chart's sandboxTemplate.uid (1000) and the Python
# runtime's `sandbox` user, so the pod securityContext (runAsUser: 1000) binds the
# same identity the emptyDirs are fsGroup-chowned to.
RUN useradd -u 1000 -m -d /home/sandbox -s /bin/bash sandbox \
 && mkdir -p /workspace \
 && chown -R 1000:1000 /workspace /home/sandbox

# sudo: passwordless, apt-get verbs ONLY (update/install/remove/purge/upgrade/
# full-upgrade/clean/autoremove). Nothing else gets sudo — npm/pip stay
# user-mode by design; apt writes the ephemeral per-pod rootfs (docs/security.md).
# `visudo -c` fails the build on any syntax slip.
RUN printf '%s\n' \
      'sandbox ALL=(root) NOPASSWD: /usr/bin/apt-get update, /usr/bin/apt-get update *, /usr/bin/apt-get install *, /usr/bin/apt-get remove *, /usr/bin/apt-get purge *, /usr/bin/apt-get upgrade *, /usr/bin/apt-get full-upgrade *, /usr/bin/apt-get clean, /usr/bin/apt-get clean *, /usr/bin/apt-get autoremove *' \
      'Defaults logfile="/var/log/sudo.log"' \
      > /etc/sudoers.d/sandbox \
 && chmod 440 /etc/sudoers.d/sandbox \
 && visudo -c \
 && install -o root -g root -m 644 /dev/null /var/log/sudo.log

# The release binary from stage 2. Owned by root (read+exec for all); the pod runs
# it as uid 1000 via the chart securityContext, so it must NOT live under /workspace
# (an emptyDir mount) or /home/sandbox (also mounted). /usr/local/bin is image-local.
COPY --from=builder /ws/target/release/runtime /usr/local/bin/runtime

# Env parity with the Python runtime image (D12). The Rust config (runtime/src/config.rs)
# reads the SAME names with the SAME defaults, so these are belt-and-braces for tenant
# scripts + standalone runs; the chart's SandboxTemplate env block overrides the
# per-sandbox knobs (MAX_TIMEOUT/MAX_PROCS/...) at runtime.
# The /opt PATH additions expose the tarball toolchain (node/npm/pnpm/yarn, pwsh,
# dotnet, ilspycmd, duckdb); SANDBOX_TOOLS_MANIFEST makes /system append the baked
# capability manifest + workspace conventions (empty value disables the append).
# RUNTIME_KEY_FILE is intentionally NOT set as an env var: it would surface in
# /proc/1/environ (the isolation e2e flags KEY-named env vars). The Rust runtime's
# built-in default (/etc/runtime-key/api-key) is used instead — same as the Python
# runtime, which also omits this env (issue #50: no credential-ish env leaks).
ENV WORKDIR=/workspace \
    SHELL=/bin/bash \
    MAX_OUTPUT_BYTES=1048576 \
    DEFAULT_TIMEOUT=120 \
    MAX_TIMEOUT=600 \
    MAX_PROCS=256 \
    PATH=/opt/node/bin:/opt/powershell:/opt/dotnet:/opt/dotnet-tools:/opt/tools:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
    DOTNET_ROOT=/opt/dotnet \
    DOTNET_CLI_HOME=/opt \
    PSModulePath=/opt/psmodules \
    SANDBOX_TOOLS_MANIFEST=/usr/local/share/sandbox-capabilities.md

# sandbox-tools (manifest + live delta + conventions) and the capability manifest
# itself — generated LAST so the version probes and the dpkg base count reflect
# the final image.
RUN install -m 0755 /tmp/workbench/sandbox-tools.sh /usr/local/bin/sandbox-tools \
 && python3 /tmp/workbench/gen-manifest.py \
      --tools /tmp/workbench/tools.json \
      --output /usr/local/share/sandbox-capabilities.md \
 && rm -rf /tmp/workbench

WORKDIR /workspace
USER 1000
EXPOSE 8888
ENTRYPOINT ["/usr/local/bin/runtime"]
