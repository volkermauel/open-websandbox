//! Shared harness for the runtime integration tests.
//!
//! Builds an in-process axum router over an isolated tmp workdir + a strong
//! per-session key file, and drives it via `tower::ServiceExt::oneshot` so the
//! tests exercise the real HTTP extractors/middleware without a network server.

#![allow(dead_code)]

use std::path::PathBuf;

use axum::body::{Body, Bytes};
use axum::http::{Method, Request, Response, StatusCode};
use http_body_util::BodyExt;
use runtime::{build_router, AppState, RuntimeConfig, SessionKeyStore};
use tempfile::TempDir;
use tower::util::ServiceExt;

pub const STRONG_KEY: &str = "a-very-strong-and-random-runtime-key-123456";

/// An isolated runtime: tmp workdir + projected key file + built router.
pub struct Env {
    pub workdir: PathBuf,
    pub key_path: PathBuf,
    pub bearer: String,
    pub max_output_bytes: usize,
    router: axum::Router,
    _tmp: TempDir,
}

impl Env {
    /// Default env: 1 MiB output cap.
    pub fn new() -> Self {
        Self::with_max_output(1_048_576)
    }

    /// Env with a custom output cap (for the truncation boundary tests).
    pub fn with_max_output(max_output_bytes: usize) -> Self {
        Self::build(max_output_bytes, 2 * 1024 * 1024 * 1024)
    }

    /// Env with a custom snapshot/restore size cap (for the fail-on-exceed tests).
    pub fn with_max_workspace_bytes(max_workspace_bytes: u64) -> Self {
        Self::build(1_048_576, max_workspace_bytes)
    }

    fn build(max_output_bytes: usize, max_workspace_bytes: u64) -> Self {
        let tmp = TempDir::new().expect("tmp dir");
        let workdir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workdir).expect("mkdir workdir");
        let key_path = tmp.path().join("api-key");
        std::fs::write(&key_path, STRONG_KEY).expect("write key");
        let config = RuntimeConfig {
            workdir: workdir.clone(),
            max_output_bytes,
            max_workspace_bytes,
            runtime_key_file: key_path.clone(),
            shell: "/bin/sh".to_string(),
            ..RuntimeConfig::default()
        };
        let key_store = SessionKeyStore::new(&key_path);
        let state = AppState::new(config, key_store);
        let router = build_router(state);
        Self {
            workdir,
            key_path,
            bearer: STRONG_KEY.to_string(),
            max_output_bytes,
            router,
            _tmp: tmp,
        }
    }

    /// Overwrite the key file (rotate-on-resume) and return the new key.
    pub fn rotate_key(&self) -> String {
        let new = format!("rotated-{}", unique_token());
        std::fs::write(&self.key_path, &new).expect("rotate key");
        new
    }

    /// Write an arbitrary value to the key file.
    pub fn set_key(&self, value: &str) {
        std::fs::write(&self.key_path, value).expect("set key");
    }

    /// Send a request, injecting the default Bearer + optional subdir + JSON body.
    pub async fn send(
        &self,
        method: Method,
        uri: &str,
        bearer: Bearer<'_>,
        subdir: Option<&str>,
        body: Option<String>,
    ) -> Response<Body> {
        self.send_raw(method, uri, bearer, subdir, body).await
    }

    /// Send with explicit bearer control.
    pub async fn send_raw(
        &self,
        method: Method,
        uri: &str,
        bearer: Bearer<'_>,
        subdir: Option<&str>,
        body: Option<String>,
    ) -> Response<Body> {
        let mut builder = Request::builder().method(method).uri(uri);
        match bearer {
            Bearer::Default => {
                builder = builder.header("authorization", format!("Bearer {}", self.bearer));
            }
            Bearer::Explicit(v) => {
                builder = builder.header("authorization", format!("Bearer {v}"));
            }
            Bearer::None => {}
        }
        if let Some(s) = subdir {
            builder = builder.header("x-workspace-subdir", s);
        }
        let req = match body {
            Some(b) => builder
                .header("content-type", "application/json")
                .body(Body::from(b))
                .unwrap(),
            None => builder.body(Body::empty()).unwrap(),
        };
        self.router.clone().oneshot(req).await.expect("oneshot")
    }

    /// Send a raw byte body with an explicit content type (e.g. `multipart/form-data`
    /// with a boundary) + optional subdir. Used by the PR-B-5 upload/archive tests.
    pub async fn send_typed(
        &self,
        method: Method,
        uri: &str,
        bearer: Bearer<'_>,
        subdir: Option<&str>,
        content_type: &str,
        body: Vec<u8>,
    ) -> Response<Body> {
        let mut builder = Request::builder().method(method).uri(uri);
        match bearer {
            Bearer::Default => {
                builder = builder.header("authorization", format!("Bearer {}", self.bearer));
            }
            Bearer::Explicit(v) => {
                builder = builder.header("authorization", format!("Bearer {v}"));
            }
            Bearer::None => {}
        }
        if let Some(s) = subdir {
            builder = builder.header("x-workspace-subdir", s);
        }
        let req = builder
            .header("content-type", content_type)
            .body(Body::from(body))
            .unwrap();
        self.router.clone().oneshot(req).await.expect("oneshot")
    }

    /// Send a request with a raw (non-JSON) byte body and no subdir, used for
    /// `/restore` which streams a zstd tarball as the request body.
    pub async fn send_bytes(
        &self,
        method: Method,
        uri: &str,
        bearer: Bearer<'_>,
        body: Vec<u8>,
    ) -> Response<Body> {
        let mut builder = Request::builder().method(method).uri(uri);
        match bearer {
            Bearer::Default => {
                builder = builder.header("authorization", format!("Bearer {}", self.bearer));
            }
            Bearer::Explicit(v) => {
                builder = builder.header("authorization", format!("Bearer {v}"));
            }
            Bearer::None => {}
        }
        let req = builder
            .header("content-type", "application/zstd")
            .body(Body::from(body))
            .unwrap();
        self.router.clone().oneshot(req).await.expect("oneshot")
    }

    /// Send a raw `axum::body::Body` (e.g. a streaming body that errors mid-stream
    /// to simulate a client disconnect) with the default Bearer + `application/zstd`.
    pub async fn send_body(
        &self,
        method: Method,
        uri: &str,
        bearer: Bearer<'_>,
        body: Body,
    ) -> Response<Body> {
        let mut builder = Request::builder().method(method).uri(uri);
        match bearer {
            Bearer::Default => {
                builder = builder.header("authorization", format!("Bearer {}", self.bearer));
            }
            Bearer::Explicit(v) => {
                builder = builder.header("authorization", format!("Bearer {v}"));
            }
            Bearer::None => {}
        }
        let req = builder
            .header("content-type", "application/zstd")
            .body(body)
            .unwrap();
        self.router.clone().oneshot(req).await.expect("oneshot")
    }
}

/// Bearer control for [`Env::send`].
pub enum Bearer<'a> {
    /// Inject the default (correct) key.
    Default,
    /// Inject an explicit token.
    Explicit(&'a str),
    /// No Authorization header.
    None,
}

fn unique_token() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::SeqCst)
}

/// Read a response body to a String.
pub async fn body_text(resp: Response<Body>) -> String {
    let bytes = body_bytes(resp).await;
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Read a response body to raw bytes.
pub async fn body_bytes(resp: Response<Body>) -> Bytes {
    resp.into_body()
        .collect()
        .await
        .expect("collect")
        .to_bytes()
}

/// Status of a response.
pub fn status(resp: &Response<Body>) -> StatusCode {
    resp.status()
}

/// `IntoResponse` on the handler-level types isn't used here; this is a tiny
/// helper to assert an exact status without carrying the response further.
pub async fn assert_status(resp: Response<Body>, expected: StatusCode) {
    let got = status(&resp);
    if got != expected {
        let body = body_text(resp).await;
        panic!("expected {expected}, got {got}: {body}");
    }
}

/// Parse a JSON response body into `T`.
pub async fn json<T: serde::de::DeserializeOwned>(resp: Response<Body>) -> T {
    let text = body_text(resp).await;
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("json decode failed: {e}\n{text}"))
}
