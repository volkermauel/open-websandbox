#!/bin/sh
# PID-1 wrapper for the local nofile KIND node image (Dockerfile.node-nofile). Raises the
# soft nofile limit to the hard limit before exec'ing the stock kindest/node entrypoint,
# so systemd → containerd → kubelet → every pod inherits a sane fd ceiling.
#
# Needed ONLY on hosts whose docker daemon caps container soft nofile at 1024 (kube-proxy
# / kindnet then crash with "too many open files"). A no-op when soft already equals hard
# (sane hosts / GHA runners), which is why CI uses plain kindest/node instead.
ulimit -Sn "$(ulimit -Hn)" 2>/dev/null || true
exec /usr/local/bin/entrypoint "$@"
