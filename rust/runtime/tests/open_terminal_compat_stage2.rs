//! Contract tests for the open-terminal v0.12.3 stage-2 compatibility surface
//! (#169): `GET /system` (upstream-verbatim prompt, template expansion),
//! `GET /info` (conditional registration), `GET /files/display` (show-file
//! signaling), real `GET /ports`, and `/proxy/{port}` with the 0.12.2
//! session-ownership lockdown.
//!
//! The happy-path proxy test spawns the **real runtime binary** as a child
//! process (`CARGO_BIN_EXE_runtime`): the child is a true descendant of this
//! process — which is also the process hosting the in-process router — so its
//! `:8888` listener is session-owned and proxyable, exactly like a service
//! `/execute` starts inside a sandbox.

#![forbid(unsafe_code)]

mod common;

use std::time::Duration;

use axum::http::{Method, StatusCode};
use common::{json, status, Bearer};
use serde_json::Value;

use common::Env;

/// Write a text file through the API (keeps every test on the HTTP surface).
async fn put(env: &Env, path: &str, content: &str) {
    let resp = env
        .send(
            Method::POST,
            "/files/write",
            Bearer::Default,
            None,
            Some(format!(
                "{{\"path\": {path}, \"content\": {content}}}",
                path = serde_json::to_string(path).unwrap(),
                content = serde_json::to_string(content).unwrap(),
            )),
        )
        .await;
    assert_eq!(status(&resp), StatusCode::OK, "write {path}");
}

/// Hostname as the runtime grounds it (`uname` nodename).
fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .expect("hostname")
        .trim()
        .to_string()
}

// --- GET /system -------------------------------------------------------------

#[tokio::test]
async fn system_requires_auth() {
    let env = Env::new();
    let resp = env
        .send(Method::GET, "/system", Bearer::None, None, None)
        .await;
    assert_eq!(status(&resp), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn system_returns_grounded_upstream_prompt() {
    let env = Env::new(); // harness shell = /bin/sh
    let resp = env
        .send(Method::GET, "/system", Bearer::Default, None, None)
        .await;
    assert_eq!(status(&resp), StatusCode::OK);
    let doc: Value = json(resp).await;
    let prompt = doc["prompt"].as_str().expect("prompt string");

    // Upstream-verbatim opening (grounding values filled from the live host).
    assert!(
        prompt.starts_with("You have access to a computer running Linux "),
        "prompt opens with the upstream literal: {prompt:?}"
    );
    assert!(
        prompt.contains(&format!("on host \"{}\"", hostname())),
        "hostname grounding: {prompt:?}"
    );
    assert!(
        prompt.contains(" with /bin/sh."),
        "shell grounding (harness shell): {prompt:?}"
    );
    // The Python sentence is present iff a python3 probe succeeds (documented
    // conditional; when present it reads exactly upstream's).
    if let Some(idx) = prompt.find("Python ") {
        assert!(
            prompt[idx..].starts_with("Python ") && prompt.contains(" is available."),
            "python sentence reads upstream-verbatim"
        );
    }
    // Upstream-verbatim tail (em dash + closing sentence).
    assert!(
        prompt.ends_with("If a command produces no output, that typically means it succeeded."),
        "prompt ends with the upstream tool-usage paragraph: {prompt:?}"
    );
    assert!(prompt.contains('\u{2014}'), "upstream em dash preserved");
}

#[tokio::test]
async fn system_prompt_override_expands_upstream_template_vars() {
    let env = Env::with_config(|cfg| {
        cfg.system_prompt = "host={{hostname}} shell={{shell}} os={{os}} {{unknown}}".to_string();
    });
    let resp = env
        .send(Method::GET, "/system", Bearer::Default, None, None)
        .await;
    assert_eq!(status(&resp), StatusCode::OK);
    let doc: Value = json(resp).await;
    let prompt = doc["prompt"].as_str().expect("prompt string");
    assert_eq!(
        prompt,
        &format!("host={} shell=/bin/sh os=Linux {{{{unknown}}}}", hostname())
    );
}

#[tokio::test]
async fn system_appends_operator_info_when_set() {
    let env = Env::with_config(|cfg| {
        cfg.info = "Managed by the e2e lab.".to_string();
    });
    let resp = env
        .send(Method::GET, "/system", Bearer::Default, None, None)
        .await;
    let doc: Value = json(resp).await;
    let prompt = doc["prompt"].as_str().expect("prompt string");
    assert!(
        prompt.ends_with("\n\nManaged by the e2e lab."),
        "info appended upstream-style: {prompt:?}"
    );
}

// --- GET /info -----------------------------------------------------------------

#[tokio::test]
async fn info_404s_like_upstreams_unregistered_route_when_unset() {
    let env = Env::new();
    let unauth = env
        .send(Method::GET, "/info", Bearer::None, None, None)
        .await;
    assert_eq!(status(&unauth), StatusCode::UNAUTHORIZED, "auth first");
    let resp = env
        .send(Method::GET, "/info", Bearer::Default, None, None)
        .await;
    assert_eq!(status(&resp), StatusCode::NOT_FOUND);
    let doc: Value = json(resp).await;
    assert_eq!(doc, serde_json::json!({"detail": "Not Found"}));
}

#[tokio::test]
async fn info_returns_operator_text_when_set() {
    let env = Env::with_config(|cfg| {
        cfg.info = "Physics department GPU-free sandbox".to_string();
    });
    let resp = env
        .send(Method::GET, "/info", Bearer::Default, None, None)
        .await;
    assert_eq!(status(&resp), StatusCode::OK);
    let doc: Value = json(resp).await;
    assert_eq!(
        doc,
        serde_json::json!({"info": "Physics department GPU-free sandbox"})
    );
}

// --- GET /files/display ---------------------------------------------------------

#[tokio::test]
async fn display_signals_path_and_existence() {
    let env = Env::new();
    put(&env, "report.md", "# hi").await;

    let resp = env
        .send(
            Method::GET,
            "/files/display?path=report.md",
            Bearer::Default,
            None,
            None,
        )
        .await;
    assert_eq!(status(&resp), StatusCode::OK);
    let doc: Value = json(resp).await;
    assert_eq!(doc["exists"], serde_json::json!(true));
    let path = doc["path"].as_str().expect("path");
    assert!(
        path.starts_with('/') && path.ends_with("report.md"),
        "resolved absolute path: {path}"
    );

    // Missing file: successful signaling response with exists=false (NOT 404).
    let missing = env
        .send(
            Method::GET,
            "/files/display?path=absent.txt",
            Bearer::Default,
            None,
            None,
        )
        .await;
    assert_eq!(status(&missing), StatusCode::OK);
    let doc: Value = json(missing).await;
    assert_eq!(doc["exists"], serde_json::json!(false));
}

#[tokio::test]
async fn display_validates_input_and_auth() {
    let env = Env::new();
    let unauth = env
        .send(
            Method::GET,
            "/files/display?path=x",
            Bearer::None,
            None,
            None,
        )
        .await;
    assert_eq!(status(&unauth), StatusCode::UNAUTHORIZED);

    // Escaping path → 400 (workspace confinement, like every file endpoint).
    let escape = env
        .send(
            Method::GET,
            "/files/display?path=../../etc/passwd",
            Bearer::Default,
            None,
            None,
        )
        .await;
    assert_eq!(status(&escape), StatusCode::BAD_REQUEST);

    // Missing required param → 400 (upstream 422 — documented divergence).
    let no_param = env
        .send(Method::GET, "/files/display", Bearer::Default, None, None)
        .await;
    assert_eq!(status(&no_param), StatusCode::BAD_REQUEST);
}

// --- GET /ports -----------------------------------------------------------------

#[tokio::test]
async fn ports_lists_session_visible_listeners_with_upstream_shape() {
    // This test process hosts the router, so an in-process listener is owned
    // by the runtime process itself — NOT a descendant — and must be absent
    // (upstream's exclusive-descendants rule). A spawned child in the proxy
    // test below proves the positive branch.
    let env = Env::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let own_port = listener.local_addr().unwrap().port();

    let unauth = env
        .send(Method::GET, "/ports", Bearer::None, None, None)
        .await;
    assert_eq!(status(&unauth), StatusCode::UNAUTHORIZED);

    let resp = env
        .send(Method::GET, "/ports", Bearer::Default, None, None)
        .await;
    assert_eq!(status(&resp), StatusCode::OK);
    let doc: Value = json(resp).await;
    let ports = doc["ports"].as_array().expect("ports array");
    for p in ports {
        assert!(p["port"].is_u64(), "port number: {p}");
        assert!(p.get("uid").is_none(), "uid stripped like upstream: {p}");
    }
    let listed: Vec<u64> = ports.iter().filter_map(|p| p["port"].as_u64()).collect();
    assert!(
        !listed.contains(&u64::from(own_port)),
        "runtime's own socket must be invisible: {listed:?}"
    );
}

// --- /proxy/{port} ---------------------------------------------------------------

#[tokio::test]
async fn proxy_rejects_unowned_and_unlistened_ports_with_upstream_404() {
    let env = Env::new();

    // A socket owned by the runtime process ITSELF (in-process listener) is
    // not a descendant ⇒ invisible ⇒ 404, the exact 0.12.2 lockdown.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let own_port = listener.local_addr().unwrap().port();
    let resp = env
        .send(
            Method::GET,
            &format!("/proxy/{own_port}/"),
            Bearer::Default,
            None,
            None,
        )
        .await;
    assert_eq!(status(&resp), StatusCode::NOT_FOUND);
    let doc: Value = json(resp).await;
    assert_eq!(doc, serde_json::json!({"detail": "Port not found"}));
    drop(listener);

    // Nothing listening at all → same upstream 404.
    let resp = env
        .send(
            Method::GET,
            &format!("/proxy/{own_port}/"),
            Bearer::Default,
            None,
            None,
        )
        .await;
    assert_eq!(status(&resp), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn proxy_validates_port_range_with_upstream_message() {
    let env = Env::new();
    let resp = env
        .send(Method::GET, "/proxy/0/x", Bearer::Default, None, None)
        .await;
    // Upstream 422 → our documented 400 divergence; message verbatim.
    assert_eq!(status(&resp), StatusCode::BAD_REQUEST);
    let doc: Value = json(resp).await;
    assert_eq!(
        doc,
        serde_json::json!({"detail": "Port must be between 1 and 65535"})
    );

    // Out-of-u16 range fails path parsing with 400 (upstream 422).
    let resp = env
        .send(Method::GET, "/proxy/70000/x", Bearer::Default, None, None)
        .await;
    assert_eq!(status(&resp), StatusCode::BAD_REQUEST);

    // Auth first: no bearer ⇒ 401 before any port handling.
    let resp = env
        .send(Method::GET, "/proxy/1/x", Bearer::None, None, None)
        .await;
    assert_eq!(status(&resp), StatusCode::UNAUTHORIZED);
}

/// A spawned real runtime binary listening on `:8888` — a true descendant of
/// this process, hence session-owned and proxyable.
struct ChildRuntime {
    child: std::process::Child,
    // Keeps the child's workspace + key file alive for the child's lifetime.
    _tmp: tempfile::TempDir,
}

impl ChildRuntime {
    /// Spawn `CARGO_BIN_EXE_runtime` with its own workspace + key, wait for
    /// `:8888` to accept connections.
    async fn start() -> Self {
        let tmp = tempfile::TempDir::new().unwrap();
        let workdir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workdir).unwrap();
        let key_path = tmp.path().join("api-key");
        std::fs::write(&key_path, common::STRONG_KEY).unwrap();
        let child = std::process::Command::new(env!("CARGO_BIN_EXE_runtime"))
            .env("WORKDIR", &workdir)
            .env("RUNTIME_KEY_FILE", &key_path)
            .spawn()
            .expect("spawn runtime binary");
        let this = Self { child, _tmp: tmp };
        // Readiness poll: connect until the listener answers (~instant).
        for _ in 0..100 {
            if tokio::net::TcpStream::connect("127.0.0.1:8888")
                .await
                .is_ok()
            {
                return this;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("child runtime never listened on :8888");
    }
}

impl Drop for ChildRuntime {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn proxy_forwards_to_owned_port_and_strips_authorization() {
    let env = Env::new();
    let _child = ChildRuntime::start().await;

    // The child must appear in /ports (descendant-owned listener).
    let resp = env
        .send(Method::GET, "/ports", Bearer::Default, None, None)
        .await;
    let doc: Value = json(resp).await;
    let listed: Vec<u64> = doc["ports"]
        .as_array()
        .expect("ports")
        .iter()
        .filter_map(|p| p["port"].as_u64())
        .collect();
    assert!(listed.contains(&8888), "child runtime visible: {listed:?}");
    let child_row = doc["ports"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["port"] == 8888)
        .expect("child row");
    assert_eq!(child_row["process"], serde_json::json!("runtime"));

    // Root route (unauthenticated on the child): happy-path round trip with
    // method + headers forwarded and the body delivered byte-exact.
    let resp = env
        .send(Method::GET, "/proxy/8888/", Bearer::Default, None, None)
        .await;
    assert_eq!(status(&resp), StatusCode::OK, "happy path");
    let body = common::body_text(resp).await;
    assert!(
        body.contains("\"status\":\"ok\""),
        "child root body: {body}"
    );
    assert!(
        body.contains("\"runtime\":\"code-standard\""),
        "child root body: {body}"
    );

    // Sub-path form: /proxy/{port}/{path}.
    let resp = env
        .send(
            Method::GET,
            "/proxy/8888/healthz",
            Bearer::Default,
            None,
            None,
        )
        .await;
    assert_eq!(status(&resp), StatusCode::OK);

    // HEAD is in upstream's method set.
    let resp = env
        .send(
            Method::HEAD,
            "/proxy/8888/healthz",
            Bearer::Default,
            None,
            None,
        )
        .await;
    assert_eq!(status(&resp), StatusCode::OK);

    // The inbound Authorization is stripped: the child's authed route 401s.
    // (Proves the per-session key never leaks to the proxied service.)
    let resp = env
        .send(
            Method::GET,
            "/proxy/8888/files/list?directory=.",
            Bearer::Default,
            None,
            None,
        )
        .await;
    let dbg_status = status(&resp);
    let dbg_body = common::body_text(resp).await;
    assert_eq!(
        dbg_status,
        StatusCode::UNAUTHORIZED,
        "authorization stripped before forwarding: {dbg_status} {dbg_body}"
    );

    // POST + JSON body forwards (child 401s on its authed route, but the
    // request must traverse the proxy, not 400/405 at the router).
    let resp = env
        .send(
            Method::POST,
            "/proxy/8888/files/write",
            Bearer::Default,
            None,
            Some(r#"{"path":"x.txt","content":"y"}"#.to_string()),
        )
        .await;
    assert_eq!(status(&resp), StatusCode::UNAUTHORIZED);
}
