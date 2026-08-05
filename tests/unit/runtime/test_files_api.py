"""Round-trip coverage for the open-terminal ``/files/*`` + LLM-tool surface.

write -> read -> list -> mkdir -> move -> replace -> delete is exercised end to
end, plus grep/glob (with include filters + max_results truncation), multipart
upload, download/view/archive, and the ``/upload`` ``/download`` ``/list``
``/exists`` tool handlers. The health, ports, and cwd endpoints round it out.
"""

from __future__ import annotations

import io
import zipfile



# --- misc endpoints -----------------------------------------------------------

async def test_health(client):
    r = await client.get("/")
    assert r.status_code == 200
    assert r.json() == {"status": "ok", "runtime": "code-standard"}


async def test_ports_empty(client):
    r = await client.get("/ports")
    assert r.status_code == 200
    assert r.json() == {"ports": []}


async def test_get_cwd_reports_workdir(workdir, client):
    r = await client.get("/files/cwd")
    assert r.status_code == 200
    body = r.json()
    assert body["cwd"] == workdir
    assert body["home"] == workdir


async def test_set_cwd_requires_existing_dir(workdir, client):
    r = await client.post("/files/cwd", json={"path": "nope"})
    assert r.status_code == 404
    # create then set
    await client.post("/files/mkdir", json={"path": "realdir"})
    r = await client.post("/files/cwd", json={"path": "realdir"})
    assert r.status_code == 200
    assert r.json()["cwd"].endswith("realdir")


# --- core write/read/list round-trip ------------------------------------------

async def test_write_read_round_trip(workdir, client):
    r = await client.post("/files/write", json={"path": "dir/a.txt", "content": "hello"})
    assert r.status_code == 200
    body = r.json()
    assert body["size"] == 5
    assert body["path"].endswith("dir/a.txt")

    r = await client.get("/files/read", params={"path": "dir/a.txt"})
    assert r.status_code == 200
    data = r.json()
    assert data["content"] == "hello"
    assert data["total_lines"] == 1


async def test_read_missing_is_404(workdir, client):
    r = await client.get("/files/read", params={"path": "nope.txt"})
    assert r.status_code == 404


async def test_list_entries(workdir, client):
    await client.post("/files/write", json={"path": "a.txt", "content": "aaa"})
    await client.post("/files/write", json={"path": "b.txt", "content": "bbbbb"})
    await client.post("/files/mkdir", json={"path": "sub"})

    r = await client.get("/files/list", params={"directory": "."})
    assert r.status_code == 200
    names = {e["name"] for e in r.json()["entries"]}
    assert {"a.txt", "b.txt", "sub"} <= names
    # the directory entry is typed correctly
    types = {e["name"]: e["type"] for e in r.json()["entries"]}
    assert types["sub"] == "directory"
    assert types["a.txt"] == "file"


async def test_list_missing_dir_is_404(workdir, client):
    r = await client.get("/files/list", params={"directory": "ghost"})
    assert r.status_code == 404


# --- mkdir / move / replace / delete ------------------------------------------

async def test_move(workdir, client):
    await client.post("/files/write", json={"path": "src.txt", "content": "data"})
    r = await client.post(
        "/files/move", json={"source": "src.txt", "destination": "dst.txt"}
    )
    assert r.status_code == 200
    # source gone, destination has the content
    assert (await client.get("/files/read", params={"path": "src.txt"})).status_code == 404
    r = await client.get("/files/read", params={"path": "dst.txt"})
    assert r.json()["content"] == "data"


async def test_move_collision_is_409(workdir, client):
    await client.post("/files/write", json={"path": "a", "content": "1"})
    await client.post("/files/write", json={"path": "b", "content": "2"})
    r = await client.post("/files/move", json={"source": "a", "destination": "b"})
    assert r.status_code == 409


async def test_move_missing_source_is_404(workdir, client):
    r = await client.post("/files/move", json={"source": "nope", "destination": "x"})
    assert r.status_code == 404


async def test_replace_single(workdir, client):
    await client.post("/files/write", json={"path": "r.txt", "content": "hello world"})
    r = await client.post(
        "/files/replace",
        json={"path": "r.txt", "replacements": [{"target": "hello", "replacement": "goodbye"}]},
    )
    assert r.status_code == 200
    r = await client.get("/files/read", params={"path": "r.txt"})
    assert r.json()["content"] == "goodbye world"


async def test_replace_requires_unique_unless_allow_multiple(workdir, client):
    await client.post("/files/write", json={"path": "d.txt", "content": "x x x"})
    r = await client.post(
        "/files/replace",
        json={"path": "d.txt", "replacements": [{"target": "x", "replacement": "y"}]},
    )
    assert r.status_code == 400  # 3 occurrences, allow_multiple false
    r = await client.post(
        "/files/replace",
        json={
            "path": "d.txt",
            "replacements": [{"target": "x", "replacement": "y", "allow_multiple": True}],
        },
    )
    assert r.status_code == 200
    assert (await client.get("/files/read", params={"path": "d.txt"})).json()["content"] == "y y y"


async def test_replace_target_not_found(workdir, client):
    await client.post("/files/write", json={"path": "n.txt", "content": "abc"})
    r = await client.post(
        "/files/replace",
        json={"path": "n.txt", "replacements": [{"target": "zzz", "replacement": "q"}]},
    )
    assert r.status_code == 400


async def test_delete_file_and_dir(workdir, client):
    await client.post("/files/write", json={"path": "del.txt", "content": "x"})
    await client.post("/files/mkdir", json={"path": "deldir/sub"})
    r = await client.delete("/files/delete", params={"path": "del.txt"})
    assert r.status_code == 200
    assert r.json()["type"] == "file"
    r = await client.delete("/files/delete", params={"path": "deldir"})
    assert r.status_code == 200
    assert r.json()["type"] == "directory"
    assert (await client.get("/files/exists/deldir")).json()["exists"] is False


async def test_delete_missing_is_404(workdir, client):
    r = await client.delete("/files/delete", params={"path": "ghost"})
    assert r.status_code == 404


# --- grep / glob --------------------------------------------------------------

async def _seed_tree(client):
    await client.post("/files/write", json={"path": "a.txt", "content": "foo bar\nsecond"})
    await client.post("/files/write", json={"path": "b.txt", "content": "baz foo"})
    await client.post("/files/write", json={"path": "c.py", "content": "foo = 1"})
    await client.post("/files/mkdir", json={"path": "pkg"})


async def test_grep_basic_and_include_filter(workdir, client):
    await _seed_tree(client)
    r = await client.get("/files/grep", params={"query": "foo", "path": "."})
    assert r.status_code == 200
    files = {split_name(m["file"]) for m in r.json()["matches"]}
    assert {"a.txt", "b.txt", "c.py"} <= files

    # include filter restricts to .txt only
    r = await client.get("/files/grep", params={"query": "foo", "path": ".", "include": "*.txt"})
    files = {split_name(m["file"]) for m in r.json()["matches"]}
    assert files == {"a.txt", "b.txt"}
    assert split_name not in files  # sanity


async def test_grep_literal_mode(workdir, client):
    await client.post("/files/write", json={"path": "r.txt", "content": "a.b.c (literal)"})
    # regex=True would treat "." as wildcard; regex=False must match the literal dot
    r = await client.get("/files/grep", params={"query": "a.b.c", "path": ".", "regex": "false"})
    assert r.status_code == 200
    assert any("r.txt" in m["file"] for m in r.json()["matches"])
    # as a regex it would still match here too; ensure case_insensitive works
    r = await client.get(
        "/files/grep", params={"query": "LITERAL", "path": ".", "case_insensitive": "true"}
    )
    assert any("r.txt" in m["file"] for m in r.json()["matches"])


async def test_grep_max_results_truncates(workdir, client):
    # many matches across several files
    for i in range(10):
        await client.post("/files/write", json={"path": f"f{i}.txt", "content": "needle\nneedle"})
    r = await client.get("/files/grep", params={"query": "needle", "path": ".", "max_results": 3})
    assert r.status_code == 200
    body = r.json()
    assert body["truncated"] is True
    assert len(body["matches"]) == 3


async def test_grep_invalid_regex_is_400(workdir, client):
    r = await client.get("/files/grep", params={"query": "(unclosed", "path": "."})
    assert r.status_code == 400


async def test_glob_files_and_dirs(workdir, client):
    await _seed_tree(client)
    r = await client.get("/files/glob", params={"pattern": "*.txt", "path": ".", "type": "file"})
    assert r.status_code == 200
    names = {m["path"] for m in r.json()["matches"]}
    assert {"a.txt", "b.txt"} <= names
    assert "c.py" not in names
    # directory glob
    r = await client.get("/files/glob", params={"pattern": "*", "path": ".", "type": "directory"})
    dirs = {m["path"] for m in r.json()["matches"]}
    assert "pkg" in dirs
    assert "a.txt" not in dirs


# --- multipart upload / download / view / archive -----------------------------

async def test_upload_then_read(workdir, client):
    files = {"file": ("up.txt", b"uploaded-bytes", "text/plain")}
    r = await client.post("/files/upload", files=files, data={"directory": "uploads"})
    assert r.status_code == 200
    body = r.json()
    assert body["size"] == len(b"uploaded-bytes")
    assert body["path"].endswith("uploads/up.txt")
    r = await client.get("/files/read", params={"path": "uploads/up.txt"})
    assert r.json()["content"] == "uploaded-bytes"


async def test_files_view_returns_raw_bytes(workdir, client):
    await client.post("/files/write", json={"path": "v.bin", "content": "ABCDEF"})
    r = await client.get("/files/view", params={"path": "v.bin"})
    assert r.status_code == 200
    assert r.content == b"ABCDEF"


async def test_archive_zips_multiple_paths(workdir, client):
    await client.post("/files/write", json={"path": "p1.txt", "content": "one"})
    await client.post("/files/write", json={"path": "d/inner.txt", "content": "two"})
    r = await client.post("/files/archive", json={"paths": ["p1.txt", "d"]})
    assert r.status_code == 200
    assert r.headers["content-type"] == "application/zip"
    zf = zipfile.ZipFile(io.BytesIO(r.content))
    names = zf.namelist()
    assert "p1.txt" in names
    assert any(n.startswith("d/") and n.endswith("inner.txt") for n in names)
    assert zf.read("p1.txt") == b"one"


async def test_archive_empty_paths_is_400(workdir, client):
    r = await client.post("/files/archive", json={"paths": []})
    assert r.status_code == 400


# --- LLM-tool surface (/upload /download /list /exists) -----------------------

async def test_tool_upload_download_list_exists(workdir, client):
    files = {"file": ("tool.txt", b"tool-payload", "text/plain")}
    r = await client.post("/upload", files=files)
    assert r.status_code == 200
    assert r.json()["bytes"] == len(b"tool-payload")

    # exists
    r = await client.get("/exists/tool.txt")
    assert r.status_code == 200
    body = r.json()
    assert body["exists"] is True and body["is_file"] is True

    # download
    r = await client.get("/download/tool.txt")
    assert r.status_code == 200
    assert r.content == b"tool-payload"

    # list
    r = await client.get("/list/.")
    assert r.status_code == 200
    names = {e["name"] for e in r.json()["entries"]}
    assert "tool.txt" in names


async def test_tool_exists_missing(workdir, client):
    r = await client.get("/exists/ghost")
    assert r.json() == {"exists": False, "is_file": False, "is_dir": False}


# --- helper -------------------------------------------------------------------

def split_name(path: str) -> str:
    """Last path component of an absolute resolved file path."""
    return path.rsplit("/", 1)[-1]
