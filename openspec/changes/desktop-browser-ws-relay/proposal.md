# Proposal: desktop/browser/events WebSocket relay on the broker

## Why

OWUI's desktop/browser tabs and event stream must ride the SAME one terminal
connection the broker already owns (per-chat sandbox, S3 snapshot/record, warm
pool). A 2026-11-05 spike (ns `agent-sandbox-runtime`, restricted PSA) proved a
second container in the sandbox pod (`open-terminal:0.16.1-desktop`,
`:8000`) serves `/api/config`, `/desktop`, and the browser/desktop/event
surfaces. The in-sandbox runtime will proxy those to the sidecar (`DESKTOP_URL`
/ `DESKTOP_API_KEY`, runtime lane). The broker's catch-all proxy is plain HTTP
and strips `Upgrade` (proxy.rs `HOP`), so today the desktop/event WebSockets
cannot pass: they need explicit WS relay routes on the broker.

## What Changes

- **Broker (this change)** — four explicit WS relay routes mirroring the
  terminal relay contract (`/api/terminals/{id}`):
  - `GET /desktop/stream`
  - `GET /desktop/audio`
  - `GET /events`
  - `GET /browser/view/{sid}/{token}/{channel}` (channel ∈ {ws, stream, vnc},
    validated pre-upgrade; the path is forwarded verbatim)
  Each route: identity from query params / headers (userId, sessionId/chatId,
  persistence — `terminal_identity`, NO path-session fallback since the path
  carries sidecar ids, not the chat id), first-message shared-secret auth
  (4001 close on bad token/timeout), `resolve_sandbox` get-or-create +
  ready-poll, mandatory per-session runtime key (fail-closed 1011), upstream
  `ws://<pod-ip>:8888<same-path?same-query>` with the per-session Bearer,
  subprotocol passthrough (client's offered list echoed by axum + forwarded
  upstream verbatim), #158 idle-clock touch both directions.
- **Auth/resolution helpers factored** out of `terminal.rs::relay` so the new
  routes share (not duplicate) the first-message auth + resolve-and-key logic.
- **Metrics**: `route_label` collapses the new paths to stable labels
  `desktop_ws` / `events_ws` / `browser_view` (bounded cardinality).
- **Catch-all semantics unchanged** — it still strips `Upgrade`; the explicit
  routes are the supported path for WS.

## Out of scope

- Runtime-side `/desktop`* / `/events` / `/browser`* proxying to the sidecar
  (runtime lane; `DESKTOP_URL`, `DESKTOP_API_KEY`).
- Chart sidecar injection + env wiring (chart lane).
- The open-terminal-k8s-proxy removal (only after this path passes verification).
- OpenAPI: the WS relays are opaque passthroughs, omitted from `paths` like the
  terminal WS upgrade (issue #75 Q5).

## Gates

`cargo fmt --check`, `cargo clippy -p broker --all-targets -- -D warnings`,
`cargo test -p broker` — green before commit. Tests: in-process parity
rejections (each new route 400s a missing-identity request exactly like the
terminal route), upstream URL path+query preservation (pure helpers),
route-label collapse, and an `OWUI_WS_LIVE`-gated full relay test (echo
upstream asserting upgrade path+query, per-session Bearer, subprotocol echo;
4001 close on a bad token).
