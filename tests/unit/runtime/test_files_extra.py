"""Error-path + branch coverage for the ``/files/*`` and LLM-tool surface.

Every error here is produced by a REAL filesystem condition (no mocking of the
filesystem): a parent path that is a file (→ ENOTDIR / EEXIST), a mode-0 file
or directory (→ EACCES), or a broken symlink (→ ENOENT on stat). The only
function mocked is ``server.os.stat`` for the genuine TOCTOU race in ``_entry``
(which cannot be raced deterministically).
"""

from __future__ import annotations

import io
import os
import zipfile

import server  # type: ignore[import-not-found]  # resolved via conftest sys.path insert

# --- /files/list branches ----------------------------------------------------

async def test_list_blank_directory_defaults_to_root(workdir, client):
    # "" and "null" (any case) must be treated as ".".
    for val in ("", "null", "NULL", "   "):
        r = await client.get("/files/list", params={"directory": val})
        assert r.status_code == 200, val
        assert r.json()["dir"].endswith("workspace") or r.json()["dir"] == workdir


async def test_list_oserror_is_500(workdir, client, tmp_path):
    # A directory we cannot read (mode 000) → os.listdir raises PermissionError.
    nodir = os.path.join(workdir, "nodir")
    os.mkdir(nodir)
    os.chmod(nodir, 0o000)
    try:
        r = await client.get("/files/list", params={"directory": "nodir"})
        assert r.status_code == 500
    finally:
        os.chmod(nodir, 0o700)  # restore so teardown can remove it


async def test_list_entry_oserror_is_skipped(workdir, client, monkeypatch):
    # _entry's TOCTOU OSError path: force os.stat to fail for one entry only.
    await client.post("/files/write", json={"path": "good.txt", "content": "x"})
    await client.post("/files/write", json={"path": "vanish.txt", "content": "y"})
    real_stat = server.os.stat

    def flaky_stat(p, *a, **k):
        if str(p).endswith("vanish.txt"):
            raise OSError("vanished (TOCTOU)")
        return real_stat(p, *a, **k)

    monkeypatch.setattr(server.os, "stat", flaky_stat)
    r = await client.get("/files/list", params={"directory": "."})
    assert r.status_code == 200
    names = {e["name"] for e in r.json()["entries"]}
    assert "good.txt" in names
    assert "vanish.txt" not in names  # the OSError entry is dropped, not fatal


# --- /files/read branches ----------------------------------------------------

async def test_read_image_returns_bytes(workdir, client):
    # An image/* file is returned as a raw Response with the right media type.
    # Write real bytes directly (bypass /files/write's utf-8 text encoding).
    png = b"\x89PNG\r\n\x1a\n" + b"\x00" * 32  # extension drives the mime
    path = os.path.join(workdir, "pic.png")
    with open(path, "wb") as f:
        f.write(png)
    r = await client.get("/files/read", params={"path": "pic.png"})
    assert r.status_code == 200
    assert r.headers["content-type"].startswith("image/png")
    assert r.content.startswith(b"\x89PNG")


async def test_read_image_oserror_is_500(workdir, client, tmp_path):
    # Image branch whose open() fails: chmod the png to 000.
    path = os.path.join(workdir, "pic.png")
    with open(path, "wb") as f:
        f.write(b"\x89PNG")
    os.chmod(path, 0o000)
    try:
        r = await client.get("/files/read", params={"path": "pic.png"})
        assert r.status_code == 500
    finally:
        os.chmod(path, 0o600)


async def test_read_text_oserror_is_500(workdir, client):
    # Non-image file whose open('r') fails: chmod 000.
    path = os.path.join(workdir, "secret.txt")
    with open(path, "w") as f:
        f.write("topsecret")
    os.chmod(path, 0o000)
    try:
        r = await client.get("/files/read", params={"path": "secret.txt"})
        assert r.status_code == 500
    finally:
        os.chmod(path, 0o600)


async def test_read_directory_as_file_is_404(workdir, client):
    # isfile(dir) is False → 404 (File not found), not an OSError.
    await client.post("/files/mkdir", json={"path": "adir"})
    r = await client.get("/files/read", params={"path": "adir"})
    assert r.status_code == 404


# --- /files/write, mkdir, delete OSError branches ----------------------------

async def test_write_oserror_is_400(workdir, client):
    # Parent is a file → makedirs(dirname) raises FileExistsError.
    await client.post("/files/write", json={"path": "block", "content": "x"})  # block is a file
    r = await client.post("/files/write", json={"path": "block/child", "content": "y"})
    assert r.status_code == 400


async def test_mkdir_oserror_is_400(workdir, client):
    # Parent is a file → makedirs raises NotADirectoryError.
    await client.post("/files/write", json={"path": "block", "content": "x"})
    r = await client.post("/files/mkdir", json={"path": "block/sub"})
    assert r.status_code == 400


async def test_delete_dir_oserror_is_400(workdir, client, tmp_path):
    # A non-writable dir containing a file: rmtree cannot unlink the child →
    # PermissionError.
    d = os.path.join(workdir, "prot")
    os.mkdir(d)
    open(os.path.join(d, "child.txt"), "w").close()
    os.chmod(d, 0o500)  # no write/unlink permission
    try:
        r = await client.delete("/files/delete", params={"path": "prot"})
        assert r.status_code == 400
    finally:
        os.chmod(d, 0o700)


# --- /files/move OSError branch ---------------------------------------------

async def test_move_oserror_is_400(workdir, client):
    # dst parent is a file → shutil.move raises OSError.
    await client.post("/files/write", json={"path": "block", "content": "x"})  # block is a file
    await client.post("/files/write", json={"path": "src.txt", "content": "data"})
    r = await client.post(
        "/files/move", json={"source": "src.txt", "destination": "block/child"}
    )
    assert r.status_code == 400


# --- /files/replace: not-a-file + both OSError branches ----------------------

async def test_replace_directory_is_404(workdir, client):
    await client.post("/files/mkdir", json={"path": "adir"})
    r = await client.post(
        "/files/replace", json={"path": "adir", "replacements": [{"target": "x", "replacement": "y"}]}
    )
    assert r.status_code == 404


async def test_replace_read_oserror_is_400(workdir, client):
    path = os.path.join(workdir, "r.txt")
    with open(path, "w") as f:
        f.write("hello")
    os.chmod(path, 0o000)
    try:
        r = await client.post(
            "/files/replace",
            json={"path": "r.txt", "replacements": [{"target": "hello", "replacement": "hi"}]},
        )
        assert r.status_code == 400
    finally:
        os.chmod(path, 0o600)


async def test_replace_write_oserror_is_400(workdir, client):
    # Read succeeds (file is readable), but the file itself is read-only →
    # open('wb') raises PermissionError. (Write needs write perm on the FILE,
    # not the parent dir.)
    path = os.path.join(workdir, "w.txt")
    with open(path, "w") as f:
        f.write("hello")
    os.chmod(path, 0o444)  # read-only file
    try:
        r = await client.post(
            "/files/replace",
            json={"path": "w.txt", "replacements": [{"target": "hello", "replacement": "hi"}]},
        )
        assert r.status_code == 400
    finally:
        os.chmod(path, 0o600)


# --- _apply_replacement line-scoped branches (387-396) -----------------------

async def test_replace_line_scoped(workdir, client):
    await client.post(
        "/files/write", json={"path": "lines.txt", "content": "one\ntwo\nthree"}
    )
    # scope the replacement to line 2 only
    r = await client.post(
        "/files/replace",
        json={
            "path": "lines.txt",
            "replacements": [
                {"target": "two", "replacement": "TWO", "start_line": 2, "end_line": 2}
            ],
        },
    )
    assert r.status_code == 200
    content = (await client.get("/files/read", params={"path": "lines.txt"})).json()["content"]
    assert content == "one\nTWO\nthree"


async def test_replace_line_scoped_open_end(workdir, client):
    # start_line set, end_line None → end = len(lines).
    await client.post(
        "/files/write", json={"path": "lines.txt", "content": "a\nb\nc\nd"}
    )
    r = await client.post(
        "/files/replace",
        json={
            "path": "lines.txt",
            "replacements": [
                {"target": "c", "replacement": "C", "start_line": 3}
            ],
        },
    )
    assert r.status_code == 200
    content = (await client.get("/files/read", params={"path": "lines.txt"})).json()["content"]
    assert content == "a\nb\nC\nd"


async def test_replace_line_scoped_start_only_default(workdir, client):
    # start_line None + end_line set → start defaults to 1-1 = 0.
    await client.post(
        "/files/write", json={"path": "lines.txt", "content": "x\ny\nz"}
    )
    r = await client.post(
        "/files/replace",
        json={
            "path": "lines.txt",
            "replacements": [{"target": "x", "replacement": "X", "end_line": 1}],
        },
    )
    assert r.status_code == 200
    content = (await client.get("/files/read", params={"path": "lines.txt"})).json()["content"]
    assert content == "X\ny\nz"


async def test_replace_line_scoped_inverted_range_noop(workdir, client):
    # start_line beyond end_line → start >= end → content returned unchanged.
    await client.post(
        "/files/write", json={"path": "lines.txt", "content": "one\ntwo\nthree"}
    )
    r = await client.post(
        "/files/replace",
        json={
            "path": "lines.txt",
            "replacements": [
                {"target": "two", "replacement": "TWO", "start_line": 5, "end_line": 2}
            ],
        },
    )
    assert r.status_code == 200
    content = (await client.get("/files/read", params={"path": "lines.txt"})).json()["content"]
    assert content == "one\ntwo\nthree"  # untouched


# --- grep branches -----------------------------------------------------------

async def test_grep_missing_path_is_404(workdir, client):
    r = await client.get("/files/grep", params={"query": "x", "path": "ghost"})
    assert r.status_code == 404


async def test_grep_single_file_path(workdir, client):
    # _walk_files(root) where root is a FILE → returns [root].
    await client.post("/files/write", json={"path": "only.txt", "content": "needle here"})
    await client.post("/files/write", json={"path": "other.txt", "content": "no match"})
    r = await client.get("/files/grep", params={"query": "needle", "path": "only.txt"})
    assert r.status_code == 200
    files = {m["file"].rsplit("/", 1)[-1] for m in r.json()["matches"]}
    assert files == {"only.txt"}


async def test_grep_unreadable_file_is_skipped(workdir, client):
    # One unreadable file in the walk → open() raises OSError → continue.
    await client.post("/files/write", json={"path": "ok.txt", "content": "needle"})
    bad = os.path.join(workdir, "bad.txt")
    with open(bad, "w") as f:
        f.write("needle")
    os.chmod(bad, 0o000)
    try:
        r = await client.get("/files/grep", params={"query": "needle", "path": "."})
        assert r.status_code == 200
        files = {m["file"].rsplit("/", 1)[-1] for m in r.json()["matches"]}
        assert "ok.txt" in files
        assert "bad.txt" not in files  # silently skipped
    finally:
        os.chmod(bad, 0o600)


# --- glob branches -----------------------------------------------------------

async def test_glob_missing_path_is_404(workdir, client):
    r = await client.get("/files/glob", params={"pattern": "*", "path": "ghost"})
    assert r.status_code == 404


async def test_glob_type_file_skips_directories(workdir, client):
    await client.post("/files/write", json={"path": "a.txt", "content": "x"})
    await client.post("/files/mkdir", json={"path": "pkg"})
    r = await client.get("/files/glob", params={"pattern": "*", "path": ".", "type": "file"})
    names = {m["path"] for m in r.json()["matches"]}
    assert "a.txt" in names
    assert "pkg" not in names  # the directory match is skipped (continue)


async def test_glob_broken_symlink_is_skipped(workdir, client):
    # A broken symlink: os.stat raises → continue.
    os.symlink("/nonexistent/xyz", os.path.join(workdir, "dangling"))
    await client.post("/files/write", json={"path": "real.txt", "content": "x"})
    r = await client.get("/files/glob", params={"pattern": "*", "path": "."})
    assert r.status_code == 200
    names = {m["path"] for m in r.json()["matches"]}
    assert "real.txt" in names
    assert "dangling" not in names


async def test_glob_max_results_truncates(workdir, client):
    for i in range(5):
        await client.post("/files/write", json={"path": f"f{i}.txt", "content": "x"})
    r = await client.get("/files/glob", params={"pattern": "*.txt", "path": ".", "max_results": 1})
    assert r.status_code == 200
    body = r.json()
    assert body["truncated"] is True
    assert len(body["matches"]) == 1


# --- /files/upload branches --------------------------------------------------

async def test_upload_into_existing_dir(workdir, client):
    # Covers the isdir-True branch (skip makedirs) on a pre-existing dir.
    await client.post("/files/mkdir", json={"path": "existing"})
    files = {"file": ("up.txt", b"data", "text/plain")}
    r = await client.post("/files/upload", files=files, params={"directory": "existing"})
    assert r.status_code == 200
    assert r.json()["size"] == len(b"data")


async def test_upload_makedirs_oserror_is_400(workdir, client):
    # directory points at an existing FILE → makedirs raises FileExistsError.
    await client.post("/files/write", json={"path": "block", "content": "x"})
    files = {"file": ("up.txt", b"data", "text/plain")}
    r = await client.post("/files/upload", files=files, params={"directory": "block"})
    assert r.status_code == 400


async def test_upload_write_oserror_is_400(workdir, client):
    # Existing read-only dir → open('wb') raises PermissionError.
    rodir = os.path.join(workdir, "rodir")
    os.mkdir(rodir)
    os.chmod(rodir, 0o500)
    files = {"file": ("up.txt", b"data", "text/plain")}
    try:
        r = await client.post("/files/upload", files=files, params={"directory": "rodir"})
        assert r.status_code == 400
    finally:
        os.chmod(rodir, 0o700)


async def test_upload_blank_directory_defaults_to_root(workdir, client):
    files = {"file": ("up.txt", b"data", "text/plain")}
    r = await client.post("/files/upload", files=files, params={"directory": ""})
    assert r.status_code == 200
    assert r.json()["path"].endswith("up.txt")


# --- /files/archive branch ---------------------------------------------------

async def test_archive_missing_path_is_404(workdir, client):
    r = await client.post("/files/archive", json={"paths": ["ghost"]})
    assert r.status_code == 404


async def test_archive_single_file_names_zip(workdir, client):
    await client.post("/files/write", json={"path": "only.txt", "content": "one"})
    r = await client.post("/files/archive", json={"paths": ["only.txt"]})
    assert r.status_code == 200
    assert "only.txt" in r.headers["content-disposition"]
    zf = zipfile.ZipFile(io.BytesIO(r.content))
    assert "only.txt" in zf.namelist()


# --- LLM-tool surface error paths -------------------------------------------

async def test_tool_upload_oserror_is_400(workdir, client):
    # Make the workspace root read-only so open(full, "wb") fails.
    os.chmod(workdir, 0o500)
    files = {"file": ("up.txt", b"data", "text/plain")}
    try:
        r = await client.post("/upload", files=files)
        assert r.status_code == 400
    finally:
        os.chmod(workdir, 0o700)


async def test_tool_download_directory_is_404(workdir, client):
    await client.post("/files/mkdir", json={"path": "adir"})
    r = await client.get("/download/adir")
    assert r.status_code == 404


async def test_tool_list_missing_is_404(workdir, client):
    r = await client.get("/list/ghost")
    assert r.status_code == 404


async def test_tool_list_oserror_is_500(workdir, client):
    nodir = os.path.join(workdir, "nodir")
    os.mkdir(nodir)
    os.chmod(nodir, 0o000)
    try:
        r = await client.get("/list/nodir")
        assert r.status_code == 500
    finally:
        os.chmod(nodir, 0o700)


async def test_tool_list_broken_symlink_entry_skipped(workdir, client):
    # A broken symlink inside a listable dir → per-entry os.stat raises → skip.
    os.symlink("/nonexistent/xyz", os.path.join(workdir, "dangling"))
    await client.post("/files/write", json={"path": "real.txt", "content": "x"})
    r = await client.get("/list/")
    assert r.status_code == 200
    names = {e["name"] for e in r.json()["entries"]}
    assert "real.txt" in names
    assert "dangling" not in names


# --- /files/view error path --------------------------------------------------

async def test_files_view_missing_is_404(workdir, client):
    r = await client.get("/files/view", params={"path": "ghost"})
    assert r.status_code == 404


async def test_files_view_directory_is_404(workdir, client):
    await client.post("/files/mkdir", json={"path": "adir"})
    r = await client.get("/files/view", params={"path": "adir"})
    assert r.status_code == 404


# --- _request_base makedirs OSError (subdir over a file) ---------------------

async def test_subdir_over_file_is_500(workdir, client):
    # A file named "block" in WORKDIR: X-Workspace-Subdir=block makes
    # _request_base try makedirs(WORKDIR/block) where block is a file → 500.
    await client.post("/files/write", json={"path": "block", "content": "x"})
    r = await client.get("/files/cwd", headers={"X-Workspace-Subdir": "block"})
    assert r.status_code == 500
