# code-standard: agent runtime image.
# Python + Node + compiler toolchain (for dynamic pip/npm installs incl. native
# extensions) + a curated set of common libraries. Non-root uid 1000.
# /workspace is the agent work dir — mounted as emptyDir (ephemeral) or PVC
# (persistent, via volumeClaimTemplates) by the SandboxTemplate at runtime.
FROM python:3.12-slim

# Toolchain + CLIs for dynamic dep installs and common agent needs.
RUN apt-get update && apt-get install -y --no-install-recommends \
      build-essential python3-dev \
      nodejs npm \
      git curl jq zip unzip ca-certificates openssh-client sudo \
      && rm -rf /var/lib/apt/lists/*

# Runtime server deps, then curated "agent-common" libs (warm-ready on claim).
COPY requirements-app.txt requirements-common.txt /tmp/
RUN pip install --no-cache-dir -r /tmp/requirements-app.txt \
    && pip install --no-cache-dir -r /tmp/requirements-common.txt

# Runtime server + entrypoint.
COPY server.py entrypoint.sh /app/
RUN chmod +x /app/entrypoint.sh

# Non-root user 1000; /workspace is mounted (emptyDir/PVC) at runtime.
RUN useradd -u 1000 -m -d /home/sandbox -s /bin/bash sandbox \
 && mkdir -p /workspace /home/sandbox/.cache/pip /home/sandbox/.local /home/sandbox/.npm-global \
 && chown -R 1000:1000 /app /workspace /home/sandbox \
 && echo 'sandbox ALL=(ALL) NOPASSWD:ALL' > /etc/sudoers.d/sandbox \
 && chmod 0440 /etc/sudoers.d/sandbox

ENV PYTHONUNBUFFERED=1 \
    PIP_CACHE_DIR=/home/sandbox/.cache/pip \
    NPM_CONFIG_PREFIX=/home/sandbox/.npm-global \
    PATH=/home/sandbox/.local/bin:/home/sandbox/.npm-global/bin:/usr/local/bin:/usr/bin:/bin:/usr/local/sbin:/usr/sbin \
    WORKDIR=/workspace \
    MAX_OUTPUT_BYTES=1048576 \
    DEFAULT_TIMEOUT=120 \
    MAX_TIMEOUT=600

WORKDIR /workspace
USER 1000
EXPOSE 8888
ENTRYPOINT ["/app/entrypoint.sh"]
