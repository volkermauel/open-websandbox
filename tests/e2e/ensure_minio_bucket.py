# SPDX-FileCopyrightText: 2026 the open-websandbox contributors
# SPDX-License-Identifier: AGPL-3.0-only

"""Create the e2e MinIO bucket BEFORE any test traffic (lane pre-step, #142).

The hybrid PVC×S3 lane runs PVC tests that do not resolve the `require_s3`
fixture (which would create the bucket). But the REAPER offloads regardless of
which module is running — with the bucket missing, every offload fails and the
fail-safe keeps sandboxes alive in a churn loop that breaks unrelated tests.
Run this right after the MinIO rollout / before pytest.

Usage (lane pre-step; boto3 from requirements-test.txt):
    python tests/e2e/ensure_minio_bucket.py
"""

from __future__ import annotations

import base64
import os
import subprocess
import sys
import time
import urllib.request

SYS_NS = os.environ.get("E2E_S3_SYS_NS", "agent-sandbox-system")
PF_PORT = int(os.environ.get("E2E_S3_PF_PORT", "9000"))
BUCKET = os.environ.get("E2E_S3_BUCKET", "owsb-e2e")


def _secret_val(key: str) -> str:
    r = subprocess.run(
        ["kubectl", "-n", SYS_NS, "get", "secret", "owui-s3-creds",
         "-o", f"jsonpath={{.data.{key}}}"],
        capture_output=True, text=True, timeout=30, check=True,
    )
    return base64.b64decode(r.stdout).decode().strip()


def main() -> int:
    import boto3
    from botocore.config import Config

    pf = subprocess.Popen(
        ["kubectl", "-n", SYS_NS, "port-forward", "svc/minio", f"{PF_PORT}:9000"],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    try:
        for _ in range(30):
            try:
                with urllib.request.urlopen(
                    f"http://localhost:{PF_PORT}/minio/health/live", timeout=2
                ):
                    break
            except Exception:
                time.sleep(1)
        else:
            print(f"MinIO port-forward :{PF_PORT} never became healthy", file=sys.stderr)
            return 1

        client = boto3.client(
            "s3",
            endpoint_url=f"http://localhost:{PF_PORT}",
            aws_access_key_id=_secret_val("access-key-id"),
            aws_secret_access_key=_secret_val("secret-access-key"),
            region_name="us-east-1",
            config=Config(s3={"addressing_style": "path"}, signature_version="s3v4"),
        )
        from botocore.exceptions import ClientError

        try:
            client.create_bucket(Bucket=BUCKET)
        except ClientError as e:
            code = e.response.get("Error", {}).get("Code", "")
            if code not in ("BucketAlreadyOwnedByYou", "BucketAlreadyExists"):
                raise
        print(f"bucket {BUCKET} ready")
        return 0
    finally:
        pf.terminate()


if __name__ == "__main__":
    raise SystemExit(main())
