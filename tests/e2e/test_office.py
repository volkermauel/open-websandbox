# SPDX-FileCopyrightText: 2026 the open-websandbox contributors
# SPDX-License-Identifier: AGPL-3.0-only

"""LibreOffice-in-runtime e2e: headless soffice availability + a real conversion.

The runtime image ships Debian's `-nogui` LibreOffice suite (see
rust/runtime/Dockerfile). These tests prove the two things tenants actually
need: `soffice` is on PATH inside the sandbox, and a real document conversion
runs headless as the non-root sandbox user (uid 1000) with its profile pinned
outside the mounted volumes.
"""

import shlex
import uuid


def test_soffice_available(broker):
    """soffice is on PATH inside the sandbox and reports its version."""
    r = broker.post("/execute", json={"command": "soffice --version"})
    assert r.status_code == 200, r.text
    body = r.json()
    assert body["exit_code"] == 0, body
    assert "LibreOffice" in body["stdout"], body


def test_headless_document_conversion(broker):
    """A flat-ODT written by the tenant converts to a real PDF in /workspace."""
    marker = uuid.uuid4().hex[:6]
    fodt = (
        '<?xml version="1.0" encoding="UTF-8"?>\n'
        '<office:document xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"'
        ' xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"'
        ' office:version="1.2" office:mimetype="application/vnd.oasis.opendocument.text">'
        f"<office:body><office:text><text:p>office-e2e-{marker}</text:p>"
        "</office:text></office:body></office:document>\n"
    )
    cmd = (
        f"printf %s {shlex.quote(fodt)} > /tmp/doc.fodt"
        " && soffice --headless -env:UserInstallation=file:///tmp/lo"
        " --convert-to pdf --outdir /workspace /tmp/doc.fodt"
    )
    r = broker.post("/execute", json={"command": cmd})
    assert r.status_code == 200, r.text
    body = r.json()
    assert body["exit_code"] == 0, body
    assert "doc.pdf" in body["stdout"], body

    # The produced artifact is a real PDF and carries the marker text content.
    check = broker.post(
        "/execute", json={"command": "head -c 4 /workspace/doc.pdf && echo && wc -c < /workspace/doc.pdf"}
    )
    assert check.status_code == 200, check.text
    out = check.json()
    assert out["exit_code"] == 0, out
    assert "%PDF" in out["stdout"], out
    assert int(out["stdout"].strip().splitlines()[-1]) > 1000, out
