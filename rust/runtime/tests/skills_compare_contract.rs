//! Contract for the OWUI v0.11.4 surface (issue #195): terminal-owned Agent
//! Skills (`GET /skills`, `/skills/read?name=`, `/skills/{name}`) and the
//! read-only text comparison (`POST /files/compare`).

#![forbid(unsafe_code)]

mod common;

use axum::http::{Method, StatusCode};

use common::Bearer;

fn write_skill(env: &common::Env, root: &str, dir: &str, skill_md: &str) {
    let base = env.workdir.join(root).join(dir);
    std::fs::create_dir_all(&base).unwrap();
    std::fs::write(base.join("SKILL.md"), skill_md).unwrap();
}

#[tokio::test]
async fn skills_list_requires_auth() {
    let env = common::Env::new();
    let resp = env
        .send(Method::GET, "/skills", Bearer::None, None, None)
        .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn skills_discovery_scan_and_dedup() {
    let env = common::Env::new();
    write_skill(
        &env,
        ".agents/skills",
        "deploy",
        "---\nname: deploy\nstate: >\n  folded value\nstate2: |\n  literal\n  lines\ndescription: Deploys things\n---\n\nDo the deploy.\n",
    );
    // Same name in a later root (.cptr/skills) must lose the dedup race.
    write_skill(
        &env,
        ".cptr/skills",
        "deploy",
        "---\nname: deploy\ndescription: Imposter\n---\n",
    );
    // No frontmatter description -> skipped.
    write_skill(&env, ".agents/skills", "silent", "---\nname: silent\n---\n");
    // Dot-directory -> skipped.
    write_skill(
        &env,
        ".agents/skills",
        ".hidden",
        "---\nname: hidden\ndescription: x\n---\n",
    );

    let resp = env
        .send(Method::GET, "/skills", Bearer::Default, None, None)
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = common::json(resp).await;
    let skills = body.as_array().expect("array");
    assert_eq!(skills.len(), 1, "one skill after dedup + skips: {body}");
    let skill = &skills[0];
    assert_eq!(skill["name"], "deploy");
    assert_eq!(skill["description"], "Deploys things");
    assert_eq!(skill["id"], "terminal:deploy");
    assert_eq!(skill["scope"], "global");
    assert_eq!(skill["source"], "terminal");
    assert!(skill["location"]
        .as_str()
        .unwrap()
        .ends_with(".agents/skills/deploy/SKILL.md"));
}

#[tokio::test]
async fn skill_read_both_shapes() {
    let env = common::Env::new();
    write_skill(
        &env,
        ".claude/skills",
        "greet",
        "---\nname: greet\ndescription: Says hi\n---\n\nGreeting body.\n",
    );
    std::fs::create_dir_all(env.workdir.join(".claude/skills/greet/scripts")).unwrap();
    std::fs::write(
        env.workdir.join(".claude/skills/greet/scripts/run.sh"),
        "#!/bin/sh\n",
    )
    .unwrap();
    std::fs::write(env.workdir.join(".claude/skills/greet/notes.txt"), "n\n").unwrap();

    for path in ["/skills/read?name=greet", "/skills/greet"] {
        let resp = env
            .send(Method::GET, path, Bearer::Default, None, None)
            .await;
        assert_eq!(resp.status(), StatusCode::OK, "{path}");
        let body: serde_json::Value = common::json(resp).await;
        assert_eq!(body["content"], "Greeting body.", "{path}");
        let mut resources = body["resources"].as_array().unwrap().clone();
        resources.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
        assert_eq!(resources.len(), 2, "{path}");
        assert_eq!(resources[0], "notes.txt");
        assert_eq!(resources[1], "scripts/run.sh");
    }

    // Unknown name -> 404 (upstream "Skill not found").
    let resp = env
        .send(
            Method::GET,
            "/skills/read?name=nope",
            Bearer::Default,
            None,
            None,
        )
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        common::json::<serde_json::Value>(resp).await["detail"],
        "Skill not found"
    );
}

#[tokio::test]
async fn compare_reports_hunks_and_counts() {
    let env = common::Env::new();
    let original: Vec<String> = (0..10).map(|i| format!("line{i}")).collect();
    let mut revised = original.clone();
    revised[5] = "line5 changed".to_string();
    std::fs::write(env.workdir.join("a.txt"), original.join("\n")).unwrap();
    std::fs::write(env.workdir.join("b.txt"), revised.join("\n")).unwrap();

    let resp = env
        .send(
            Method::POST,
            "/files/compare",
            Bearer::Default,
            None,
            Some(r#"{"original":"a.txt","revised":"b.txt"}"#.into()),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = common::json(resp).await;
    assert_eq!(body["additions"], 1);
    assert_eq!(body["deletions"], 1);
    let hunks = body["hunks"].as_array().expect("hunks");
    assert_eq!(hunks.len(), 1);
    // Change at index 5, 3 context lines either side: @@ -3,7 +3,7 @@
    assert_eq!(hunks[0]["header"], "@@ -3,7 +3,7 @@");
    assert_eq!(hunks[0]["lines"].as_array().unwrap().len(), 8); // 3 ctx + del + add + 3 ctx
    assert_eq!(body["original"]["name"], "a.txt");
    assert_eq!(body["revised"]["notices"], serde_json::json!([]));

    // The removed/added pair carries intraline segments.
    let lines = hunks[0]["lines"].as_array().unwrap();
    let added = lines
        .iter()
        .find(|l| l["type"] == "added")
        .expect("added line");
    assert!(added["segments"]
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s["changed"] == true && s["text"].as_str().unwrap().trim() == "changed"));
}

#[tokio::test]
async fn compare_ignore_whitespace_and_identical() {
    let env = common::Env::new();
    std::fs::write(env.workdir.join("a.txt"), "one\ntwo  spaced\n").unwrap();
    std::fs::write(env.workdir.join("b.txt"), "one\ntwo spaced\n").unwrap();

    // Identical inputs: no hunks.
    let resp = env
        .send(
            Method::POST,
            "/files/compare",
            Bearer::Default,
            None,
            Some(r#"{"original":"a.txt","revised":"a.txt"}"#.into()),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = common::json(resp).await;
    assert_eq!(body["hunks"], serde_json::json!([]));
    assert_eq!(body["additions"], 0);

    // ignore_whitespace collapses the difference.
    let resp = env
        .send(
            Method::POST,
            "/files/compare",
            Bearer::Default,
            None,
            Some(r#"{"original":"a.txt","revised":"b.txt","ignore_whitespace":true}"#.into()),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = common::json(resp).await;
    assert_eq!(body["hunks"], serde_json::json!([]));
}

#[tokio::test]
async fn compare_error_contracts() {
    let env = common::Env::new();
    std::fs::write(env.workdir.join("ok.txt"), "text\n").unwrap();
    std::fs::write(env.workdir.join("bin.dat"), [0x00u8, 0x01, 0x02]).unwrap();

    // Missing file -> 422 (upstream worker-error parity).
    let resp = env
        .send(
            Method::POST,
            "/files/compare",
            Bearer::Default,
            None,
            Some(r#"{"original":"missing.txt","revised":"ok.txt"}"#.into()),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let detail = common::json::<serde_json::Value>(resp).await["detail"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(detail.contains("missing.txt: File not found."), "{detail}");

    // Binary -> 422.
    let resp = env
        .send(
            Method::POST,
            "/files/compare",
            Bearer::Default,
            None,
            Some(r#"{"original":"bin.dat","revised":"ok.txt"}"#.into()),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);

    // Path escape -> 400 (repo-wide confinement contract, cf. /files/* D11).
    let resp = env
        .send(
            Method::POST,
            "/files/compare",
            Bearer::Default,
            None,
            Some(r#"{"original":"../../etc/passwd","revised":"ok.txt"}"#.into()),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}
