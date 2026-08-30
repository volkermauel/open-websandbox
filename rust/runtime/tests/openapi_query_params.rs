//! Regression contract for the `/files/read` 400 "missing field `path`" incident.
//!
//! Root cause: every query-extractor struct (`Query<ReadQuery>` et al.) was
//! derived `IntoParams` WITHOUT `#[into_params(parameter_in = Query)]`, so
//! utoipa defaulted the whole parameter set to `in: "path"` in the served
//! `/openapi.json`. Spec-driven tool callers bind parameters strictly by their
//! declared location: `in: "path"` params are substituted into `{slots}` of the
//! path template. `/files/read` has NO slots, so `path`/`start_line`/`end_line`
//! had nowhere to go and were silently dropped — the runtime then received
//! `GET /files/read` with an EMPTY query and axum's `Query<ReadQuery>`
//! extractor rejected it with `400 missing field \`path\``. Terminal-UI
//! callers build the query by hand and always worked, which is why the
//! failure looked intermittent (it depends on which client surface builds
//! the request).
//!
//! The main test replays exactly that spec-driven request construction
//! against the runtime's own generated OpenAPI document and must succeed.

#![forbid(unsafe_code)]

mod common;

use std::collections::BTreeMap;

use axum::http::{Method, StatusCode};
use utoipa::openapi::path::ParameterIn;

use common::Bearer;

/// Build the request URI a spec-driven caller derives from `doc` for
/// `path_tmpl` + `values`: every parameter declared `in: "path"` is substituted
/// into its `{slot}` (a declared path param whose slot does not exist in the
/// template is silently dropped — the Open Web UI tool-discovery behavior this
/// regression guards against), every parameter declared `in: "query"` is
/// appended to the query string. Values for names the spec does not declare
/// are never sent (spec-driven callers send only what the spec describes).
fn spec_driven_uri(
    doc: &utoipa::openapi::OpenApi,
    path_tmpl: &str,
    method: &Method,
    values: &BTreeMap<&str, &str>,
) -> String {
    let item = doc
        .paths
        .paths
        .get(path_tmpl)
        .unwrap_or_else(|| panic!("path {path_tmpl} missing from OpenAPI document"));
    let op = match *method {
        Method::GET => item.get.as_ref(),
        Method::POST => item.post.as_ref(),
        Method::DELETE => item.delete.as_ref(),
        Method::PUT => item.put.as_ref(),
        _ => panic!("unsupported method in test helper"),
    }
    .unwrap_or_else(|| panic!("{method} operation missing for {path_tmpl}"));
    let mut uri = path_tmpl.to_string();
    let mut query: Vec<String> = Vec::new();
    let params = op.parameters.clone().unwrap_or_default();
    for param in &params {
        let value = values
            .get(param.name.as_str())
            .copied()
            .unwrap_or_else(|| panic!("test must supply a value for param {}", param.name));
        match param.parameter_in {
            ParameterIn::Path => {
                let slot = format!("{{{}}}", param.name);
                if let Some(pos) = uri.find(&slot) {
                    uri.replace_range(pos..pos + slot.len(), value);
                }
                // else: the spec declared the parameter in the path, but the
                // template carries no matching slot — a spec-driven caller has
                // nowhere to put the value and drops it.
            }
            ParameterIn::Query => query.push(format!("{}={}", param.name, value)),
            ParameterIn::Header | ParameterIn::Cookie => {
                unreachable!("Open Web UI tool discovery only binds path/query params")
            }
        }
    }
    if query.is_empty() {
        uri
    } else {
        format!("{uri}?{}", query.join("&"))
    }
}

/// The incident, replayed: a spec-driven `read_file` tool call built from the
/// generated OpenAPI document must reach the handler WITH its query intact.
#[tokio::test]
async fn spec_driven_files_read_call_carries_the_query() {
    let env = common::Env::new();
    // The file the tool asks for (content smaller than end_line is fine — the
    // handler clamps the slice).
    env.send(
        Method::POST,
        "/files/write",
        Bearer::Default,
        None,
        Some(r#"{"path":"slides_text.txt","content":"line-1\nline-2\n"}"#.into()),
    )
    .await;

    let doc = runtime::openapi::openapi_document();
    let mut values = BTreeMap::new();
    values.insert("path", "slides_text.txt");
    values.insert("start_line", "1");
    values.insert("end_line", "470");
    let uri = spec_driven_uri(&doc, "/files/read", &Method::GET, &values);

    let resp = env
        .send(Method::GET, &uri, Bearer::Default, None, None)
        .await;
    let status = resp.status();
    let body = common::body_text(resp).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "spec-driven GET {uri} failed — parameters were dropped (declared in the wrong \
         location by the OpenAPI document): {body}"
    );
}

/// Every query-extractor parameter of the GET tool surface must be declared
/// `in: "query"` in the generated document (path params stay `in: "path"`).
#[test]
fn query_extractor_params_are_declared_in_query() {
    let doc = runtime::openapi::openapi_document();
    // (path template, parameters that live in the query string)
    let expected_query: &[(&str, &[&str])] = &[
        ("/files/read", &["path", "start_line", "end_line"]),
        ("/files/list", &["directory"]),
        ("/files/view", &["path"]),
        ("/files/display", &["path"]),
        (
            "/files/grep",
            &[
                "query",
                "path",
                "regex",
                "case_insensitive",
                "include",
                "max_results",
            ],
        ),
        ("/files/glob", &["pattern", "path", "type", "max_results"]),
        (
            "/files/search",
            &["query", "path", "limit", "type", "show_hidden"],
        ),
        (
            "/files/matches",
            &["query", "path", "show_hidden", "offset", "limit"],
        ),
    ];
    for (tmpl, names) in expected_query {
        let item = doc
            .paths
            .paths
            .get(*tmpl)
            .unwrap_or_else(|| panic!("{tmpl} missing from document"));
        let op = item
            .get
            .as_ref()
            .unwrap_or_else(|| panic!("GET {tmpl} missing"));
        let params = op.parameters.clone().unwrap_or_default();
        for name in *names {
            let param = params
                .iter()
                .find(|p| p.name == *name)
                .unwrap_or_else(|| panic!("param {name} missing from GET {tmpl}"));
            assert!(
                param.parameter_in == ParameterIn::Query,
                "GET {tmpl} param {name} must be declared in the query string"
            );
        }
    }
    // Genuine path parameters keep their location.
    let serve = doc.paths.paths["/files/serve/{file_path}"]
        .get
        .as_ref()
        .expect("GET /files/serve")
        .parameters
        .clone()
        .unwrap_or_default();
    let file_path = serve
        .iter()
        .find(|p| p.name == "file_path")
        .expect("file_path param");
    assert!(file_path.parameter_in == ParameterIn::Path);
}
