# runtime-api

## ADDED Requirements

### Requirement: LLM system prompt endpoint

The runtime SHALL serve `GET /system` returning the open-terminal system
prompt so Open Web UI can ground the model in its sandbox, with the prompt
text ported verbatim from upstream v0.12.3.

#### Scenario: grounded system prompt

- **WHEN** `GET /system` is called with a valid session
- **THEN** the response is `{"prompt": "<text>"}` where `<text>` is the
  upstream default prompt with the template variables grounded in the real
  sandbox environment (OS, kernel, arch, hostname, user, shell; the Python
  sentence only when a python3 probe succeeds), and a missing session
  returns 401

#### Scenario: operator prompt override

- **WHEN** `OPEN_TERMINAL_SYSTEM_PROMPT` is set to a template containing
  `{{var}}` placeholders
- **THEN** `GET /system` returns the template with the upstream variable set
  (`os`, `kernel`, `arch`, `hostname`, `user`, `shell`, `python_version`,
  `home`) expanded and unknown placeholders left verbatim

#### Scenario: feature flag parity

- **WHEN** `GET /api/config` is called on the runtime or through the broker
- **THEN** `features.system` is `true` alongside
  `{"terminal": true, "notebooks": false}`

### Requirement: operator info endpoint

The runtime SHALL serve `GET /info` mirroring upstream's conditional
registration.

#### Scenario: info set

- **WHEN** `OPEN_TERMINAL_INFO` is non-empty and `GET /info` is called with a
  valid session
- **THEN** the response is `{"info": "<value>"}`

#### Scenario: info unset

- **WHEN** `OPEN_TERMINAL_INFO` is unset or empty
- **THEN** `GET /info` returns 404 `{"detail": "Not Found"}` exactly like
  upstream's unregistered route

### Requirement: show-file signaling

The runtime SHALL serve `GET /files/display` as the upstream signaling
endpoint (no bytes served).

#### Scenario: display round-trip

- **WHEN** `GET /files/display?path=<p>` targets an existing workspace file
- **THEN** the response is `{"path": "<resolved absolute>", "exists": true}`
- **AND** a missing file yields `exists: false` (not 404), an escaping path
  yields 400, and a missing session yields 401

### Requirement: session-owned port proxy

The runtime SHALL proxy `/proxy/{port}[/{path}]` (GET/POST/PUT/PATCH/DELETE/
HEAD/OPTIONS) only to localhost ports owned by the session's own processes,
mirroring the upstream 0.12.2 lockdown.

#### Scenario: owned port proxied

- **WHEN** a descendant process of the runtime listens on `<port>` and a
  proxied request arrives with a valid session
- **THEN** the request is forwarded to `http://localhost:<port>/<path>` with
  the query string, method and body intact, hop-by-hop headers and the
  inbound `Authorization` stripped, and the upstream service's status, body
  and headers returned (minus transfer-encoding/connection/content-length)

#### Scenario: unowned port rejected

- **WHEN** the target port is not listening or its socket belongs to a
  non-descendant process (including the runtime itself)
- **THEN** the proxy returns 404 `{"detail": "Port not found"}`

#### Scenario: port bounds and transport errors

- **WHEN** the port is outside 1..=65535
- **THEN** the proxy returns 400 `Port must be between 1 and 65535`
  (upstream 422 — documented divergence)
- **WHEN** the owned target refuses the connection or times out
- **THEN** the proxy returns 502 `Connection refused: localhost:<port>` or
  504 `Timeout connecting to localhost:<port>` respectively

#### Scenario: ports listing parity

- **WHEN** `GET /ports` is called with a valid session
- **THEN** the response lists exactly the session-visible listening ports as
  `{"ports": [{"port", "pid", "process"}]}` (uid stripped), sorted by port —
  the same visibility set the proxy ownership check uses
