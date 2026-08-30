//! Broker-side regression for the `/files/read` 400 "missing field `path`"
//! incident (see `rust/runtime/tests/openapi_query_params.rs` for the runtime
//! half and the full narrative).
//!
//! Open Web UI's tool discovery fetches the MERGED document served at
//! `/openapi.json` and binds tool-call parameters by their declared location.
//! Every query-extractor parameter must therefore be declared `in: "query"`
//! here — a spec-driven caller drops `in: "path"` parameters that have no
//! `{slot}` in the path template, which is how `GET /files/read` reached the
//! runtime with an empty query and failed with `400 missing field \`path\``.

#![forbid(unsafe_code)]

use broker::openapi::openapi_document;
use utoipa::openapi::path::ParameterIn;

fn params(doc: &utoipa::openapi::OpenApi, tmpl: &str, method: &str) -> Vec<(String, ParameterIn)> {
    let item = &doc.paths.paths[tmpl];
    let op = match method.to_ascii_lowercase().as_str() {
        "get" => item.get.as_ref(),
        "post" => item.post.as_ref(),
        "delete" => item.delete.as_ref(),
        "put" => item.put.as_ref(),
        _ => panic!("unsupported method {method} in test helper"),
    }
    .unwrap_or_else(|| panic!("{method} {tmpl} missing from merged document"));
    op.parameters
        .clone()
        .unwrap_or_default()
        .into_iter()
        .map(|p| (p.name, p.parameter_in))
        .collect()
}

#[test]
fn merged_document_declares_query_params_in_query() {
    let doc = openapi_document();
    // The incident endpoint: every ReadQuery field must be a query param.
    for (name, loc) in params(&doc, "/files/read", "get") {
        assert!(
            loc == ParameterIn::Query,
            "GET /files/read param {name} must be in the query string"
        );
    }
    // The merged doc carries all nine corrected structs — spot-check each
    // endpoint's full parameter set plus the broker's own label selector.
    let cases: &[(&str, &str, &[&str])] = &[
        ("/files/read", "get", &["path", "start_line", "end_line"]),
        ("/files/list", "get", &["directory"]),
        ("/files/view", "get", &["path"]),
        ("/files/display", "get", &["path"]),
        (
            "/files/grep",
            "get",
            &[
                "query",
                "path",
                "regex",
                "case_insensitive",
                "include",
                "max_results",
            ],
        ),
        (
            "/files/glob",
            "get",
            &["pattern", "path", "type", "max_results"],
        ),
        (
            "/files/search",
            "get",
            &["query", "path", "limit", "type", "show_hidden"],
        ),
        (
            "/files/matches",
            "get",
            &["query", "path", "show_hidden", "offset", "limit"],
        ),
        ("/files/upload", "post", &["directory"]),
        ("/api/sandboxes", "get", &["labelSelector"]),
    ];
    for (tmpl, method, names) in cases {
        let declared = params(&doc, tmpl, method);
        for name in *names {
            let (_, loc) = declared
                .iter()
                .find(|(n, _)| n == name)
                .unwrap_or_else(|| panic!("param {name} missing from {method} {tmpl}"));
            assert!(
                *loc == ParameterIn::Query,
                "{method} {tmpl} param {name} must be in the query string"
            );
        }
    }
    // Genuine path-template parameters keep `in: "path"`.
    for (tmpl, method, name) in [
        ("/files/serve/{file_path}", "get", "file_path"),
        ("/api/sandboxes/{name}", "get", "name"),
    ] {
        let (_, loc) = params(&doc, tmpl, method)
            .into_iter()
            .find(|(n, _)| n == name)
            .unwrap_or_else(|| panic!("param {name} missing from {method} {tmpl}"));
        assert!(loc == ParameterIn::Path, "{method} {tmpl} {name}");
    }
}
