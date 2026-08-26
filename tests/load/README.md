# Load / soak / chaos suite (issues #127, #126)

Non-functional tests for the open-websandbox broker. Both tools drive the same
broker-agnostic HTTP/WS contract as [`tests/e2e`](../e2e/) and need **no
dependencies beyond `requirements-test.txt`** (httpx + websockets).

Run them against a live broker — e.g. the KIND install from
[`docs/quickstart.md`](../../docs/quickstart.md) with `kubectl port-forward`:

```bash
kubectl -n agent-sandbox-system port-forward svc/owui-broker 8889:8080 &
export BROKER_URL=http://localhost:8889
export BROKER_SECRET="$(kubectl -n agent-sandbox-system get secret owui-broker-secret \
  -o jsonpath='{.data.shared-secret}' | base64 -d)"
```

## `loadgen.py` — soak / load

N concurrent virtual users, each with its own sandbox session, driving a mixed
workload (`/execute`, `/files/write`+`/files/read`, PTY round-trips over
`WS /api/terminals/{session}`). Prints per-op p50/p95/p99 latency, throughput,
and the error rate; exits non-zero above `--max-error-rate`.

```bash
# 30 users, 2 minutes, mixed workload:
python3 tests/load/loadgen.py --users 30 --duration 120 --csv /tmp/load.csv

# The 10k-terminal exercise (issue #126): WS only, no think time:
python3 tests/load/loadgen.py --users 10000 --duration 300 --ws-only --think 0
```

## `chaos.py` — component kill under traffic

Runs background traffic, deletes the pods of a control-plane component
(`broker` / `router` / `controller`) mid-flight, and asserts **self-healing**:
`/execute` answers again within `--max-recover` seconds and the overall error
rate stays bounded (errors *during* the kill are expected).

```bash
python3 tests/load/chaos.py --target broker
python3 tests/load/chaos.py --target router --users 5 --duration 90
```

## CI

These are **not** per-PR gates: they are manual/nightly exercises against a
dedicated cluster (a kill or a 10k-WS soak does not belong on every PR). The
functional coverage lives in [`tests/e2e`](../e2e/) (runc + gVisor + s3-tiered

+ upgrade/rollback lanes).

Both tools honor repo rule **R1**: when `KUBECONFIG` is set, every `kubectl`
call is pointed at it explicitly.
