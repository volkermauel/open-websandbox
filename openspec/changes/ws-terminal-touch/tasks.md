# Tasks

- [x] `BROKER_WS_TOUCH_INTERVAL_SECONDS` config (default 45, 0=off) + boot warning when >= park idle
- [x] `SessionTouch` throttled refresher; both relay pumps touch on frames (client typing AND watched command output)
- [x] Metric `owui_broker_ws_touches_total{direction=client|upstream}`
- [x] Chart + deploy/base env, values, schema; docs (operations/deploy/CHANGELOG)
- [x] Unit tests (first-frame touch, throttle window, disabled) + gated echo-server integration test (frames advance the annotation)
- [x] e2e `test_ws_touch.py`: claim → WS frames → `broker-last-used` advances (live-KIND verified)
- [x] Bonus fix found by the e2e ordering: draft adoption lost on claim retry (first attempt 503s before readiness dropped the in-memory plan) — now persisted as a `broker-draft-adopt-pending` marker on the Sandbox, rebuilt + executed by any later resolve, cleared one-shot; `SandboxStore::clear_annotation` added; retry unit test + KIND combined-order proof (12/12)
