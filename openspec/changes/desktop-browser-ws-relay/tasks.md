# Tasks: desktop/browser/events WebSocket relay on the broker

## 1. Broker

- [x] 1.1 `terminal.rs`: factor `authenticate_first_message` (first-message
      auth + metric + 4001 close) and `resolve_and_key` (resolve_sandbox +
      mandatory per-session runtime key, 1011 close) out of `relay`; add a
      header-carrying relay core so desktop routes can forward client
      subprotocols upstream without changing the existing test seams
- [x] 1.2 `desktop.rs` (new): `ws_upstream_url` (override seam, http→ws scheme),
      `offered_protocols`, channel validation (ws/stream/vnc), generic
      post-upgrade relay (auth → resolve+key → upstream with Bearer +
      subprotocols → touches both directions), four route handlers
- [x] 1.3 `app.rs`: register `/desktop/stream`, `/desktop/audio`, `/events`,
      `/browser/view/{sid}/{token}/{channel}` on the gated router (catch-all
      untouched)
- [x] 1.4 `metrics.rs`: `route_label` → `desktop_ws` / `events_ws` /
      `browser_view` + tests

## 2. Tests

- [x] 2.1 In-process: each new route rejects a missing-identity WS-upgrade
      request with the SAME 400 + detail as `/api/terminals/{id}`; browser
      route rejects an unknown channel pre-upgrade
- [x] 2.2 Unit: upstream URL preserves path+query verbatim (default pod-IP
      target + override seam), subprotocol header parsing
- [x] 2.3 `OWUI_WS_LIVE`-gated: full relay through the real router — echo
      upstream asserts upgrade path+query + per-session Bearer + subprotocol;
      frames round-trip; wrong first-message token closes 4001

## 3. Docs & release

- [x] 3.1 `CHANGELOG.md` `[Unreleased]` `## Added`
- [x] 3.2 OpenSpec spec delta (broker/desktop-ws-relay)

## 4. Gates

- [x] 4.1 `cargo fmt --check` + `cargo clippy -p broker --all-targets -- -D
      warnings` + `cargo test -p broker` green; commit on `feat/desktop` (no push)
