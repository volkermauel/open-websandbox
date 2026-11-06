# broker/desktop-ws-relay

## ADDED Requirement: desktop/browser/events WebSocket relay

The broker SHALL relay the desktop/browser/event WebSockets to the resolved
sandbox's runtime over explicit routes, so the single broker-owned terminal
connection also serves the desktop and browser tabs.

### Scenario: routes and passthrough

- **WHEN** a client upgrades `GET /desktop/stream`, `GET /desktop/audio`,
  `GET /events`, or `GET /browser/view/{sid}/{token}/{channel}`
  (channel ∈ {ws, stream, vnc})
- **THEN** the broker upgrades the client socket, connects
  `ws://<pod-ip>:8888<same-path?same-query>` on the resolved sandbox, and
  relays frames bidirectionally with subprotocols passed through (the client's
  offered list is echoed and forwarded verbatim); any other channel is
  rejected pre-upgrade with 400

### Scenario: identity + auth contract identical to the terminal relay

- **WHEN** a desktop/browser/event WS open arrives
- **THEN** identity resolves from query params (`userId`, `sessionId`/`chatId`,
  `persistence`) then headers (`X-User-Id`, `X-Session-Id`) — no path fallback,
  the path carries sidecar ids — a missing user/session is a pre-upgrade 400
  identical to `/api/terminals/{id}`, and the shared secret is validated via
  the same first-message `{"type":"auth"}` handshake (4001 close on a bad
  token or timeout)

### Scenario: sandbox resolution fail-closed

- **WHEN** the relay resolves the identity's sandbox
- **THEN** it uses the same `resolve_sandbox` get-or-create + ready-poll as
  every other runtime hop, requires the per-session runtime key (1011 close
  when missing/unreadable), and authenticates the upstream upgrade with that
  key's Bearer

### Scenario: idle clock keeps live desktop sessions

- **WHEN** frames are relayed in either direction
- **THEN** both pumps refresh `broker-last-used` through the same throttled
  touch the terminal relay uses, so the reaper cannot park an open desktop
  session mid-use

### Scenario: catch-all unchanged

- **WHEN** any other path (including WS upgrades) hits the `/{*path}` catch-all
- **THEN** it behaves exactly as before — plain HTTP, hop-by-hop headers
  (including `Upgrade`) stripped; the explicit routes are the only supported
  WS path

### Scenario: bounded metrics labels

- **WHEN** the HTTP metrics middleware records a desktop/browser/event route
- **THEN** the path label collapses to `desktop_ws` (`/desktop/*`),
  `events_ws` (`/events`), or `browser_view` (`/browser/view/*`) — never the
  raw URL
