//! Sandbox lifecycle CRUD via the in-process HTTP harness (stub store, no cluster).

#![forbid(unsafe_code)]

mod common;

use axum::http::{Method, StatusCode};
use common::{json, status, Bearer, Env, BASE_TEMPLATE};
use shared::{Profile, Sandbox, SandboxSpec};

fn create_body(name: &str) -> String {
    serde_json::json!({
        "templateName": BASE_TEMPLATE,
        "name": name,
        "userId": "user-1",
        "sessionId": "chat-1",
        "profile": "persistent"
    })
    .to_string()
}

#[tokio::test]
async fn create_then_get_sandbox() {
    let env = Env::new();
    // Create.
    let resp = env
        .send(
            Method::POST,
            "/api/sandboxes",
            Bearer::Default,
            Some(create_body("owui-c-aaa")),
        )
        .await;
    assert_eq!(status(&resp), StatusCode::CREATED, "create should be 201");

    let created: serde_json::Value = json(resp).await;
    assert_eq!(created["apiVersion"], "agents.x-k8s.io/v1beta1");
    assert_eq!(created["kind"], "Sandbox");
    assert_eq!(created["metadata"]["name"], "owui-c-aaa");
    assert_eq!(created["metadata"]["namespace"], "agent-sandbox-runtime");
    assert_eq!(
        created["metadata"]["labels"]["app.kubernetes.io/managed-by"],
        "owui-broker"
    );
    assert_eq!(
        created["metadata"]["labels"]["broker-profile"],
        "persistent"
    );
    assert!(created["metadata"]["annotations"]["broker-last-used"].is_string());
    assert_eq!(created["metadata"]["annotations"]["broker-user"], "user-1");
    assert_eq!(
        created["metadata"]["annotations"]["broker-session"],
        "chat-1"
    );
    assert_eq!(created["spec"]["operatingMode"], "Running");
    assert_eq!(created["spec"]["shutdownPolicy"], "Retain");
    // The profile label was stamped onto the cloned pod template.
    assert_eq!(
        created["spec"]["podTemplate"]["metadata"]["labels"]["profile"],
        "persistent"
    );

    // Get.
    let resp = env
        .send(
            Method::GET,
            "/api/sandboxes/owui-c-aaa",
            Bearer::Default,
            None,
        )
        .await;
    assert_eq!(status(&resp), StatusCode::OK);
    let got: serde_json::Value = json(resp).await;
    assert_eq!(got["metadata"]["name"], "owui-c-aaa");
}

#[tokio::test]
async fn create_uses_default_profile_when_omitted() {
    let env = Env::new();
    let body = serde_json::json!({
        "templateName": BASE_TEMPLATE,
        "name": "owui-default",
        "userId": "u",
        "sessionId": "s"
    })
    .to_string();
    let resp = env
        .send(Method::POST, "/api/sandboxes", Bearer::Default, Some(body))
        .await;
    assert_eq!(status(&resp), StatusCode::CREATED);
    let created: serde_json::Value = json(resp).await;
    // Config default is `persistent`.
    assert_eq!(
        created["metadata"]["labels"]["broker-profile"],
        "persistent"
    );
}

#[tokio::test]
async fn create_is_idempotent_on_conflict() {
    let env = Env::new();
    let first = env
        .send(
            Method::POST,
            "/api/sandboxes",
            Bearer::Default,
            Some(create_body("owui-c-conf")),
        )
        .await;
    assert_eq!(status(&first), StatusCode::CREATED);

    // Second create of the same name → 200 with the existing object.
    let second = env
        .send(
            Method::POST,
            "/api/sandboxes",
            Bearer::Default,
            Some(create_body("owui-c-conf")),
        )
        .await;
    assert_eq!(status(&second), StatusCode::OK);
    let body: serde_json::Value = json(second).await;
    assert_eq!(body["metadata"]["name"], "owui-c-conf");
}

#[tokio::test]
async fn create_unknown_template_is_404() {
    let env = Env::new();
    let body = serde_json::json!({
        "templateName": "does-not-exist",
        "name": "owui-c-x"
    })
    .to_string();
    let resp = env
        .send(Method::POST, "/api/sandboxes", Bearer::Default, Some(body))
        .await;
    assert_eq!(status(&resp), StatusCode::NOT_FOUND);
    assert!(json::<serde_json::Value>(resp).await["detail"]
        .as_str()
        .unwrap()
        .contains("does-not-exist"));
}

#[tokio::test]
async fn create_rejects_malformed_body() {
    let env = Env::new();
    // Missing required `name` field.
    let body = serde_json::json!({"templateName": BASE_TEMPLATE}).to_string();
    let resp = env
        .send(Method::POST, "/api/sandboxes", Bearer::Default, Some(body))
        .await;
    assert_eq!(status(&resp), StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn get_missing_sandbox_is_404() {
    let env = Env::new();
    let resp = env
        .send(Method::GET, "/api/sandboxes/ghost", Bearer::Default, None)
        .await;
    assert_eq!(status(&resp), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn list_sandboxes_and_filter_by_label() {
    let env = Env::new();

    // Seed two sandboxes directly into the store with different profiles.
    let make = |name: &str, profile: Profile| {
        let mut s = Sandbox::new(
            name,
            SandboxSpec {
                template_name: None,
                operating_mode: None,
                shutdown_policy: None,
                pod_template: None,
            },
        );
        s.metadata.namespace = Some("agent-sandbox-runtime".into());
        s.metadata.labels = Some(
            [
                (
                    "app.kubernetes.io/managed-by".to_string(),
                    "owui-broker".to_string(),
                ),
                ("broker-profile".to_string(), profile.as_str().to_string()),
            ]
            .into_iter()
            .collect(),
        );
        s
    };
    env.store
        .insert_sandbox(make("owui-a", Profile::Persistent));
    env.store.insert_sandbox(make("owui-b", Profile::Ephemeral));

    // List all.
    let resp = env
        .send(Method::GET, "/api/sandboxes", Bearer::Default, None)
        .await;
    assert_eq!(status(&resp), StatusCode::OK);
    let items: Vec<serde_json::Value> = json(resp).await;
    assert_eq!(items.len(), 2, "both seeded sandboxes listed");

    // Filter by broker-profile=ephemeral.
    let resp = env
        .send(
            Method::GET,
            "/api/sandboxes?labelSelector=broker-profile%3Dephemeral",
            Bearer::Default,
            None,
        )
        .await;
    assert_eq!(status(&resp), StatusCode::OK);
    let items: Vec<serde_json::Value> = json(resp).await;
    assert_eq!(items.len(), 1, "only the ephemeral sandbox matches");
    assert_eq!(items[0]["metadata"]["name"], "owui-b");
}

#[tokio::test]
async fn delete_sandbox_is_idempotent() {
    let env = Env::new();
    // Seed, then delete → 204.
    let s = Sandbox::new(
        "owui-del",
        SandboxSpec {
            template_name: None,
            operating_mode: None,
            shutdown_policy: None,
            pod_template: None,
        },
    );
    env.store.insert_sandbox(s);

    let resp = env
        .send(
            Method::DELETE,
            "/api/sandboxes/owui-del",
            Bearer::Default,
            None,
        )
        .await;
    assert_eq!(status(&resp), StatusCode::NO_CONTENT);

    // Deleting again (now absent) is still 204 (404-tolerant).
    let resp = env
        .send(
            Method::DELETE,
            "/api/sandboxes/owui-del",
            Bearer::Default,
            None,
        )
        .await;
    assert_eq!(status(&resp), StatusCode::NO_CONTENT);

    // And the get now 404s.
    let resp = env
        .send(
            Method::GET,
            "/api/sandboxes/owui-del",
            Bearer::Default,
            None,
        )
        .await;
    assert_eq!(status(&resp), StatusCode::NOT_FOUND);
}
