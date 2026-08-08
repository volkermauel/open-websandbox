# code-standard: agent runtime image.
# Python + Node + compiler toolchain (for dynamic pip/npm installs incl. native
# extensions) + a curated set of common libraries. Non-root uid 1000.
# /workspace is the agent work dir — mounted as emptyDir (ephemeral) or PVC
# (persistent, via volumeClaimTemplates) by the SandboxTemplate at runtime.
FROM python:3.14-slim

# Toolchain + CLIs for dynamic dep installs and common agent needs.
RUN apt-get update && apt-get install -y --no-install-recommends \
      build-essential python3-dev \
      nodejs npm \
      git curl jq zip unzip ca-certificates openssh-client bzip2 \
      && rm -rf /var/lib/apt/lists/*

# Runtime server deps, then curated "agent-common" libs (warm-ready on claim).
COPY requirements-app.txt requirements-common.txt /tmp/
RUN pip install --no-cache-dir -r /tmp/requirements-app.txt \
    && pip install --no-cache-dir -r /tmp/requirements-common.txt

# Runtime server + entrypoint.
COPY server.py entrypoint.sh /app/
RUN chmod +x /app/entrypoint.sh

# micromamba: user-space (non-root) package manager backed by conda-forge.
# Lets the sandbox user `micromamba install -c conda-forge <pkg>` into a writable
# prefix without root — works under restricted PodSecurity + gVisor.
RUN curl -fsSL "https://micro.mamba.pm/api/micromamba/linux-64/latest" -o /tmp/mm.tar.bz2 \
 && tar -xjf /tmp/mm.tar.bz2 -C /usr/local/bin --strip-components=1 bin/micromamba \
 && chmod +x /usr/local/bin/micromamba && rm -f /tmp/mm.tar.bz2

# Non-root user 1000; /workspace is mounted (emptyDir/PVC) at runtime.
RUN useradd -u 1000 -m -d /home/sandbox -s /bin/bash sandbox \
 && mkdir -p /workspace /home/sandbox/.cache/pip /home/sandbox/.local /home/sandbox/.npm-global \
 && chown -R 1000:1000 /app /workspace /home/sandbox

ENV PYTHONUNBUFFERED=1 \
    PIP_CACHE_DIR=/home/sandbox/.cache/pip \
    NPM_CONFIG_PREFIX=/home/sandbox/.npm-global \
    MAMBA_ROOT_PREFIX=/packages \
    PATH=/packages/bin:/home/sandbox/.local/bin:/home/sandbox/.npm-global/bin:/usr/local/bin:/usr/bin:/bin:/usr/local/sbin:/usr/sbin \
    WORKDIR=/workspace \
    MAX_OUTPUT_BYTES=1048576 \
    DEFAULT_TIMEOUT=120 \
    MAX_TIMEOUT=600

WORKDIR /workspace
USER 1000
EXPOSE 8888
ENTRYPOINT ["/app/entrypoint.sh"]
