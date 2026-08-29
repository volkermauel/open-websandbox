# Tasks: open-terminal compatibility stage 2

## 1. Runtime endpoints

- [x] 1.1 `config.rs`: `info` (env `OPEN_TERMINAL_INFO`) + `system_prompt`
      (env `OPEN_TERMINAL_SYSTEM_PROMPT`) knobs; defaults test updated
- [x] 1.2 `system.rs`: verbatim upstream default prompt + `{{var}}` template
      expansion (upstream key set), grounding (uname/USER/SHELL/HOME/python
      probe), `GET /system` + `GET /info` handlers (info unset ⇒ 404
      `Not Found`); unit-pinned prompt literal
- [x] 1.3 `files/io.rs`: `display_file` (`{path, exists}` signaling contract)
- [x] 1.4 `ports.rs`: `/proc/net/tcp{,6}` + socket-inode + descendant-PID
      visibility; real `GET /ports` (`{port, pid, process}`, uid stripped);
      `port_proxy` (7 methods, ownership 404 `Port not found`, range message,
      header strips, 502/504 upstream strings); `state.rs` shared reqwest
      client; `Cargo.toml` reqwest no-default-features + nix `utsname`
- [x] 1.5 `app.rs` + `openapi.rs`: routes registered; `api_config`
      `system: true`; OpenAPI paths/schemas (system+proxy excluded like
      upstream's `include_in_schema=False`; display + info documented)

## 2. Broker

- [x] 2.1 `Features.system = true`; parity test + OpenAPI snapshot regen

## 3. Tests

- [x] 3.1 Contract tests `open_terminal_compat_stage2.rs` (system/info/
      display/ports/proxy happy-path via spawned runtime binary child,
      unowned-port 404, port-range 400, authorization stripped)
- [x] 3.2 e2e `test_open_terminal_compat.py`: stage-2 surface through the
      relay + `system: true` flip; own-port :8888 lockdown check

## 4. Docs & release

- [x] 4.1 `docs/compatibility.md`: four new matrix rows, /ports row updated,
      system-prompt provenance note, flag-flip note, divergences
- [x] 4.2 `CHANGELOG.md` `[Unreleased]` `## Added`

## 5. Gates

- [x] 5.1 `cargo fmt --check` + `cargo clippy --all-targets -- -D warnings` +
      `cargo test --workspace`
- [x] 5.2 `openspec validate`; `mkdocs build --strict`
- [x] 5.3 broker OpenAPI snapshot regenerated; PR (closes #169 stage 2)
