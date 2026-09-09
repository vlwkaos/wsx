#!/usr/bin/env python3
"""Reference worktree-review provider. No Git mutations; see docs/worktree-review.md."""
import hashlib
import json
import os
import subprocess
import selectors
import time
import sys

LIMIT = 1024 * 1024


class ReviewError(Exception):
    def __init__(self, code, message):
        self.code, self.message = code, message


def git(root, *args, allowed=(0,)):
    # The WSX host enforces the aggregate deadline/output limit and process group cleanup.
    child = subprocess.Popen(
        ["git", "--no-pager", "-C", root, *args],
        stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        env=dict(os.environ, GIT_TERMINAL_PROMPT="0", GIT_OPTIONAL_LOCKS="0", GIT_LITERAL_PATHSPECS="1"),
    )
    streams = {child.stdout: bytearray(), child.stderr: bytearray()}
    deadline = time.monotonic() + 2
    try:
        with selectors.DefaultSelector() as selector:
            for pipe in streams:
                os.set_blocking(pipe.fileno(), False)
                selector.register(pipe, selectors.EVENT_READ)
            while selector.get_map():
                if time.monotonic() >= deadline:
                    raise ReviewError("unavailable", "Git query timed out")
                for key, _ in selector.select(0.02):
                    chunk = os.read(key.fileobj.fileno(), 8192)
                    if not chunk:
                        selector.unregister(key.fileobj)
                    else:
                        streams[key.fileobj].extend(chunk)
                        if len(streams[key.fileobj]) > LIMIT:
                            raise ReviewError("limit_exceeded", "Git output exceeds review limit")
        if child.wait(timeout=max(0.01, deadline - time.monotonic())) not in allowed:
            raise ReviewError("unavailable", "Git query failed")
        return bytes(streams[child.stdout])
    finally:
        if child.poll() is None:
            child.kill()
        child.wait()
        child.stdout.close()
        child.stderr.close()


def safe_path(path):
    if not path or path.startswith("/") or any(p in (".", "..", "") for p in path.split("/")):
        raise ReviewError("unavailable", "Unsupported Git path")
    if any(ord(c) < 32 or ord(c) == 127 for c in path):
        raise ReviewError("unavailable", "Unsupported Git path")
    return path


def path_metadata(root, path):
    parts = safe_path(path).split("/")
    fd = os.open(root, os.O_RDONLY | os.O_DIRECTORY)
    try:
        for part in parts[:-1]:
            next_fd = os.open(part, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=fd)
            os.close(fd)
            fd = next_fd
        return os.stat(parts[-1], dir_fd=fd, follow_symlinks=False)
    finally:
        os.close(fd)


def file_bytes(root, path):
    # Open each component relative to an already-open directory without symlink following.
    parts = safe_path(path).split("/")
    fd = os.open(root, os.O_RDONLY | os.O_DIRECTORY)
    try:
        for part in parts[:-1]:
            next_fd = os.open(part, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=fd)
            os.close(fd)
            fd = next_fd
        child = os.open(parts[-1], os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=fd)
        with os.fdopen(child, "rb") as source:
            import stat
            if not stat.S_ISREG(os.fstat(source.fileno()).st_mode):
                raise ReviewError("unavailable", "Not a regular file")
            content = source.read(LIMIT + 1)
            if len(content) > LIMIT:
                raise ReviewError("limit_exceeded", "File exceeds review limit")
            return content
    finally:
        os.close(fd)


def observe(root):
    raw = git(root, "status", "--porcelain=v1", "-z", "--untracked-files=all")
    head = git(root, "rev-parse", "--verify", "HEAD", allowed=(0, 128)).strip()
    index = git(root, "ls-files", "--stage", "-z")
    digest = hashlib.sha256(head + b"\0" + index + b"\0" + raw)
    submodules = {record.split(b"\t", 1)[1].decode("utf-8") for record in index.split(b"\0") if record.startswith(b"160000 ")}
    entries, pieces, i = [], raw.split(b"\0"), 0
    statuses = {"A": "added", "D": "deleted", "R": "renamed", "C": "copied", "T": "type_changed"}
    while i < len(pieces) and pieces[i]:
        record = pieces[i]
        i += 1
        xy = record[:2].decode("ascii")
        path = safe_path(record[3:].decode("utf-8"))
        old = path
        if "R" in xy or "C" in xy:
            old = safe_path(pieces[i].decode("utf-8"))
            i += 1
        status = "untracked" if xy == "??" else ("unmerged" if "U" in xy or xy in ("AA", "DD") else next((v for k, v in statuses.items() if k in xy), "modified"))
        kind, content = "text", None
        try:
            content = file_bytes(root, path)
            digest.update(content)
            if b"\0" in content:
                kind = "binary"
        except FileNotFoundError:
            digest.update(b"missing")
        except (OSError, ReviewError) as error:
            kind = "oversized" if isinstance(error, ReviewError) and error.code == "limit_exceeded" else "unreadable"
            try:
                metadata = path_metadata(root, path)
                digest.update(f"{kind}:{metadata.st_mode}:{metadata.st_size}:{metadata.st_mtime_ns}".encode())
            except OSError:
                digest.update(kind.encode())
        if path in submodules:
            kind = "submodule"
        entries.append({"file_id": path, "old_path": None if status in ("added", "untracked") else old,
                        "new_path": None if status == "deleted" else path, "status": status,
                        "content_kind": kind, "additions": None, "deletions": None})
        if len(entries) > 1000:
            raise ReviewError("limit_exceeded", "Too many changed files")
    if head:
        stats = git(root, "diff", "--no-ext-diff", "--no-textconv", "--numstat", "-z", "--no-renames", head.decode("ascii"), "--")
        counts = {}
        for record in stats.split(b"\0"):
            if record:
                added, deleted, name = record.split(b"\t", 2)
                counts[name.decode("utf-8")] = (int(added) if added != b"-" else None, int(deleted) if deleted != b"-" else None)
        for entry in entries:
            selected = [counts[p] for p in dict.fromkeys((entry["old_path"], entry["new_path"])) if p in counts]
            if selected:
                entry["additions"] = sum(c[0] for c in selected) if all(c[0] is not None for c in selected) else None
                entry["deletions"] = sum(c[1] for c in selected) if all(c[1] is not None for c in selected) else None
    for entry in entries:
        if entry["status"] == "untracked" and entry["content_kind"] == "text":
            content = file_bytes(root, entry["file_id"])
            entry["additions"] = content.count(b"\n") + int(bool(content) and not content.endswith(b"\n"))
            entry["deletions"] = 0
    return digest.hexdigest(), entries, head.decode("ascii")


def hunks_from_patch(patch):
    import re
    hunks, current = [], None
    for line in patch.decode("utf-8").split("\n"):
        if line.startswith("@@ "):
            match = re.match(r"@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@(.*)", line)
            if not match:
                raise ReviewError("unavailable", "Unsupported hunk header")
            a, b, c, d, heading = match.groups()
            current = {"old_start": int(a), "old_count": int(b or 1), "new_start": int(c),
                       "new_count": int(d or 1), "heading": heading.strip(), "lines": []}
            hunks.append(current)
        elif current is not None and line:
            if line.startswith("\\ No newline"):
                current["lines"].append({"kind": "no_newline"})
            elif line[0] in " +-":
                current["lines"].append({"kind": {" ": "context", "+": "addition", "-": "deletion"}[line[0]], "text": line[1:]})
    return hunks


def review(request):
    root = request["worktree_path"]
    if request["api_version"] != 1 or request["comparison"] != "working_against_head":
        raise ReviewError("unsupported", "Unsupported review comparison")
    snapshot, files, head = observe(root)
    if request["operation"] == "list_files":
        cap = min(1000, max(1, request["limits"]["files"]))
        return "files", {"snapshot": snapshot, "comparison_label": "Working changes against HEAD",
                         "files": files[:cap], "omitted_files": max(0, len(files) - cap), "truncated": len(files) > cap}
    if request["operation"] != "file_diff":
        raise ReviewError("unsupported", "Unsupported operation")
    if snapshot != request["snapshot"]:
        raise ReviewError("stale_snapshot", "Changes updated; refresh required")
    file = next((f for f in files if f["file_id"] == request["file_id"]), None)
    if file is None:
        raise ReviewError("stale_snapshot", "File no longer changed")
    kind, hunks = file["content_kind"], []
    if kind == "text":
        if file["status"] == "untracked" or (not head and file["status"] == "added"):
            content = file_bytes(root, file["file_id"]).decode("utf-8")
            values = content.splitlines()
            if values:
                lines = [{"kind": "addition", "text": line} for line in values]
                if not content.endswith("\n"):
                    lines.append({"kind": "no_newline"})
                hunks = [{"old_start": 0, "old_count": 0, "new_start": 1, "new_count": len(values), "heading": "", "lines": lines}]
        elif head:
            paths = list(dict.fromkeys(p for p in (file["old_path"], file["new_path"]) if p))
            patch = git(root, "diff", "--no-ext-diff", "--no-textconv", "--no-color", "--no-renames", "--unified=3", head, "--", *paths)
            hunks = hunks_from_patch(patch)
            if b"Binary files " in patch:
                kind = "binary"
    if observe(root)[0] != snapshot:
        raise ReviewError("stale_snapshot", "Changes updated; refresh required")
    kept, count = [], 0
    for hunk in hunks:
        if len(kept) >= request["limits"]["hunks"] or count + len(hunk["lines"]) > request["limits"]["lines"]:
            break
        kept.append(hunk)
        count += len(hunk["lines"])
    return "diff", {"snapshot": snapshot, "file_id": file["file_id"], "content_kind": kind,
                    "hunks": kept, "truncated": len(kept) != len(hunks)}


def main():
    request = {}
    try:
        raw = sys.stdin.buffer.read(16385)
        if len(raw) > 16384:
            raise ReviewError("invalid_request", "Request too large")
        request = json.loads(raw)
        result, data = review(request)
    except ReviewError as error:
        result, data = "error", {"code": error.code, "message": error.message}
    except (OSError, ValueError, KeyError, TypeError, subprocess.TimeoutExpired):
        result, data = "error", {"code": "unavailable", "message": "Cannot read this comparison"}
    response = json.dumps({"api_version": 1, "request_id": request.get("request_id", ""), "result": result, "data": data})
    if len(response.encode()) > LIMIT:
        response = json.dumps({"api_version": 1, "request_id": request.get("request_id", ""), "result": "error", "data": {"code": "limit_exceeded", "message": "Diff too large"}})
    print(response)


if __name__ == "__main__":
    main()
