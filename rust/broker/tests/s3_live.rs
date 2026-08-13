//! Live S3 cold-tier integration test (issue #101, item **C2**).
//!
//! Exercises the real `aws-sdk-s3` [`AwsColdStore`] against a local MinIO
//! container so the production `put_object` / `get_object` / `latest_key` /
//! `delete_prefix_except` path is verified against a real S3-compatible store —
//! not just the in-memory double (`InMemoryColdStore`).
//!
//! **Env-gated**: returns (passes) unless `OWUI_S3_LIVE=1` (any of
//! `1`/`true`/`yes`/`on`), so `cargo test --workspace` stays green without the
//! MinIO container. Run it locally:
//!
//! ```text
//! docker run -d --rm --name owui-minio -p 9000:9000 \
//!   -e MINIO_ROOT_USER=minio -e MINIO_ROOT_PASSWORD=minio123 \
//!   minio/minio server /data
//! OWUI_S3_LIVE=1 cargo test -p broker --test s3_live -- --nocapture
//! ```
//!
//! Every test writes a unique key/bucket (pid + monotonic seq) so parallel test
//! threads never collide; buckets are left in place on the throwaway container.

#![forbid(unsafe_code)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use broker::{s3_namespace, s3_object_key, AwsColdStore, ColdStore};
use bytes::Bytes;
use shared::BrokerConfig;

/// Run only when the operator opted in (`OWUI_S3_LIVE=1`); otherwise every test
/// returns (passes) so a plain `cargo test` needs no MinIO.
fn gated() -> bool {
    std::env::var("OWUI_S3_LIVE").is_ok_and(|v| matches!(v.as_str(), "1" | "true" | "yes" | "on"))
}

/// Monotonic per-process counter so every test writes a unique key/bucket.
static SEQ: AtomicU64 = AtomicU64::new(0);

/// Unique suffix: `<pid>-<seq>-<tag>`.
fn uniq(tag: &str) -> String {
    let n = SEQ.fetch_add(1, Ordering::SeqCst);
    format!("{}-{n}-{tag}", std::process::id())
}

/// `BrokerConfig` pointed at a local MinIO on `:9000` with static creds. `s3_sse`
/// is left empty — dev MinIO has no SSE backend, and `AwsColdStore` only requests
/// SSE-S3 when `s3_sse` is non-empty / non-`none`.
fn minio_config(bucket: &str) -> BrokerConfig {
    BrokerConfig {
        s3_enabled: true,
        s3_endpoint: "http://localhost:9000".to_string(),
        s3_region: "us-east-1".to_string(),
        s3_bucket: bucket.to_string(),
        s3_prefix: "users".to_string(),
        s3_access_key_id: "minio".to_string(),
        s3_secret_access_key: "minio123".to_string(),
        s3_path_style: true,
        s3_sse: String::new(),
        ..Default::default()
    }
}

/// Create the bucket on MinIO via a raw S3 client. [`AwsColdStore`] assumes the
/// bucket already exists; `BucketAlreadyOwnedByYou` / `BucketAlreadyExists` is
/// tolerated so a re-run of the suite reuses the bucket. Mirrors the client
/// construction in `AwsColdStore::new` exactly.
async fn ensure_bucket(cfg: &BrokerConfig) {
    use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region};
    let conf = aws_sdk_s3::Config::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new(cfg.s3_region.clone()))
        .force_path_style(cfg.s3_path_style)
        .endpoint_url(&cfg.s3_endpoint)
        .credentials_provider(Credentials::new(
            &cfg.s3_access_key_id,
            &cfg.s3_secret_access_key,
            None,
            None,
            "static",
        ))
        .build();
    let client = aws_sdk_s3::Client::from_conf(conf);
    if let Err(e) = client.create_bucket().bucket(&cfg.s3_bucket).send().await {
        let msg = e.to_string();
        // Tolerate "already exists": a re-run of the suite reuses the bucket.
        let already =
            msg.contains("BucketAlreadyOwnedByYou") || msg.contains("BucketAlreadyExists");
        assert!(already, "create_bucket {} failed: {msg}", cfg.s3_bucket);
    }
}

/// A fresh `AwsColdStore` + its own unique bucket for one test.
async fn store(tag: &str) -> AwsColdStore {
    let bucket = uniq(&format!("b-{tag}"));
    let cfg = minio_config(&bucket);
    ensure_bucket(&cfg).await;
    AwsColdStore::new(&cfg)
}

/// Poll `latest_key(ns)` until it returns `want`, for up to ~5s, so the test is
/// robust to MinIO's brief list-consistency lag without sleeping the full time.
async fn assert_latest_becomes(store: &AwsColdStore, ns: &str, want: &str) {
    for _ in 0..25 {
        if let Some(got) = store.latest_key(ns).await.expect("latest_key") {
            if got == want {
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let got = store.latest_key(ns).await.expect("latest_key");
    panic!("latest_key under {ns} never became {want:?}, last = {got:?}");
}

#[tokio::test]
async fn put_then_get_roundtrips_body() {
    if !gated() {
        eprintln!("skipped: set OWUI_S3_LIVE=1 to run the live S3 test");
        return;
    }
    let store = store("putget").await;
    let key = format!("users/{}/putget-{}.tar.zst", uniq("u"), uniq("k"));
    let body = Bytes::from_static(b"fresh-snapshot-bytes");

    store
        .put_object(&key, body.clone(), 7)
        .await
        .expect("put_object");
    assert_eq!(
        store.get_object(&key).await.expect("get_object"),
        body,
        "get_object returns exactly what was put"
    );
}

#[tokio::test]
async fn latest_key_picks_newest_versioned_key() {
    if !gated() {
        eprintln!("skipped: set OWUI_S3_LIVE=1 to run the live S3 test");
        return;
    }
    let store = store("latest").await;
    let ns = s3_namespace("users", "u", "chat-latest");
    let older = s3_object_key("users", "u", "chat-latest", 1_699_000_000);
    let newer = s3_object_key("users", "u", "chat-latest", 1_700_000_000);

    store
        .put_object(&older, Bytes::from_static(b"old"), 7)
        .await
        .expect("put older");
    store
        .put_object(&newer, Bytes::from_static(b"new"), 7)
        .await
        .expect("put newer");

    // Lexical max == chronological (zero-padded ts) — the restore path relies on it.
    assert_latest_becomes(&store, &ns, &newer).await;
}

#[tokio::test]
async fn delete_prefix_except_keeps_skip_and_removes_rest() {
    if !gated() {
        eprintln!("skipped: set OWUI_S3_LIVE=1 to run the live S3 test");
        return;
    }
    let store = store("del").await;
    let ns = s3_namespace("users", "u", "chat-del");
    let keep = s3_object_key("users", "u", "chat-del", 1_700_000_000);
    let old = s3_object_key("users", "u", "chat-del", 1_690_000_000);
    // A snapshot under a DIFFERENT namespace must be left untouched.
    let other = s3_object_key("users", "other", "chat-x", 1_700_000_000);

    store
        .put_object(&keep, Bytes::from_static(b"k"), 7)
        .await
        .expect("put keep");
    store
        .put_object(&old, Bytes::from_static(b"o"), 7)
        .await
        .expect("put old");
    store
        .put_object(&other, Bytes::from_static(b"x"), 7)
        .await
        .expect("put other");

    let deleted = store
        .delete_prefix_except(&ns, Some(&keep))
        .await
        .expect("delete_prefix_except");
    assert_eq!(deleted, 1, "only `old` under the namespace was removed");
    assert!(store.get_object(&keep).await.is_ok(), "`keep` survives");
    assert!(store.get_object(&old).await.is_err(), "`old` removed");
    assert!(
        store.get_object(&other).await.is_ok(),
        "`other` (different namespace) untouched"
    );
}
