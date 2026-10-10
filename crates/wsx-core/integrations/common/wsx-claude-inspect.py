"""Private opt-in hook metadata only. See docs/claude-inspection.md."""
import importlib.util
import json
import os
import re
import select
import shutil
import stat
import subprocess
import sys
import time
from pathlib import Path

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parent
VERSION = re.compile(r"[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?")
ACTIONS = {"idle", "working", "blocked", "done", "error", "unknown", "session", "detached"}


def protected(path, directory=False, private=False):
    try:
        entry = path.lstat()
        kind = stat.S_ISDIR if directory else stat.S_ISREG
        return (kind(entry.st_mode) and entry.st_uid == os.geteuid()
                and not entry.st_mode & (0o077 if private else 0o022))
    except OSError:
        return False


def probe(binary, arguments, limit):
    # ^ Read-only CLI paths, bounded bytes/time, no inherited stdin or stderr.
    process = subprocess.Popen([binary] + arguments, stdin=subprocess.DEVNULL,
                               stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    try:
        os.set_blocking(process.stdout.fileno(), False)
        deadline = time.monotonic() + 0.75
        output = bytearray()
        while len(output) <= limit:
            remaining = deadline - time.monotonic()
            if remaining <= 0 or not select.select([process.stdout], [], [], remaining)[0]:
                return None
            data = os.read(process.stdout.fileno(), limit + 1 - len(output))
            if not data:
                if process.wait(timeout=max(0.01, deadline - time.monotonic())) != 0:
                    return None
                return bytes(output) if len(output) <= limit else None
            output.extend(data)
        return None
    finally:
        if process.poll() is None:
            process.kill()
        process.wait()
        process.stdout.close()


def version(value):
    return value if isinstance(value, str) and len(value) <= 64 and VERSION.fullmatch(value) else "unknown"


def core_version(value):
    return tuple(map(int, value.split("-")[0].split("+")[0].split(".")))


def main():
    build, integration, action, reporter, status, reason = sys.argv[1:]
    if (action not in ACTIONS or not status.isascii() or not status.isdigit()
            or len(status) > 3 or int(status) > 255 or not integration.isascii()
            or not integration.isdigit() or len(integration) > 10
            or int(integration) > 4294967295 or version(build) == "unknown"):
        return
    if reason not in {"reported", "no_pane", "metadata_skipped", "stale_runtime",
                      "pane_not_found", "daemon_unreachable", "report_failed"}:
        reason = "report_failed"
    if not protected(ROOT, directory=True):
        return
    marker = ROOT / "wsx-inspect-enabled"
    enabled_by = "environment" if os.environ.get("WSX_INSPECT") == "1" else "hook_marker"
    if enabled_by == "hook_marker":
        if not protected(marker, private=True) or marker.stat().st_size != 0:
            return
    packet = {
        "schema_version": 1, "phase": "hook_process", "enabled_by": enabled_by,
        "hook_build": version(build), "hook_integration": int(integration), "action": action,
        "pane": "unknown", "runtime_generation": "present" if os.environ.get("WSX_RUNTIME_GENERATION") else "missing",
        "child_session_marker": "present" if "CLAUDE_CODE_CHILD_SESSION" in os.environ else "absent",
        "force_persistence_marker": "present" if "CLAUDE_CODE_FORCE_SESSION_PERSISTENCE" in os.environ else "absent",
        "report_attempted": reason not in ("no_pane", "metadata_skipped"),
        "report_exit": int(status), "error_code": None,
        "reporter_version_observed": "unknown", "daemon_version_observed": "unknown",
        "daemon_revision_observed": None, "agent_launch_version": "unknown",
        "agent_startup_environment": "unknown", "warnings": [],
    }
    pane = os.environ.get("WSX_PANE_ID", "")
    if re.fullmatch(r"[0-9]{1,20}", pane):
        packet["pane"] = pane
    if int(status) != 0:
        # Never echo a message, prompt, path or other captured CLI output.
        failure = sys.stdin.buffer.read(4096).decode("utf-8", errors="replace")
        code = re.match(r"Error: ([a-z][a-z0-9_]{0,63}):", failure)
        packet["error_code"] = code.group(1) if code else reason
    elif not packet["report_attempted"]:
        packet["error_code"] = reason
    try:
        helper = ROOT / "wsx-reporter.py"
        if not protected(helper):
            raise OSError("untrusted helper")
        spec = importlib.util.spec_from_file_location("wsx_reporter", helper)
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        candidate = reporter if os.path.isabs(reporter) else shutil.which(reporter)
        binary = module._executable(candidate, os.geteuid()) if candidate else None
        if binary:
            observed = probe(binary, ["--version"], 128)
            if observed is not None and observed.startswith(b"wsx "):
                packet["reporter_version_observed"] = version(observed[4:].decode("ascii").strip())
            # ^ Do not assume an old CLI bypasses eager daemon bootstrap.
            # Only these source-backed versions have verified read-only dispatch.
            if packet["reporter_version_observed"] in ("0.30.0", build):
                observed = probe(binary, ["runtime", "status", "--json"], 8192)
                runtime = json.loads(observed) if observed is not None else {}
                lifecycle = runtime.get("lifecycle") or {}
                packet["daemon_version_observed"] = version(lifecycle.get("version"))
                revision = lifecycle.get("daemon_revision")
                if type(revision) is int and 0 <= revision <= 4294967295:
                    packet["daemon_revision_observed"] = revision
            else:
                packet["warnings"].append("runtime_probe_skipped_for_unverified_reporter")
    except (OSError, ValueError, TypeError, AttributeError, subprocess.SubprocessError):
        pass
    current = packet["reporter_version_observed"]
    if current == "unknown":
        packet["warnings"].append("reporter_version_unknown")
    elif core_version(current) < core_version(build):
        packet["warnings"].append("older_reporter_observed")
    elif current != build:
        packet["warnings"].append("different_reporter_observed")
    if packet["runtime_generation"] == "missing":
        packet["warnings"].append("missing_runtime_generation")
    if packet["error_code"] == "stale_runtime":
        packet["warnings"].append("runtime_generation_rejected")
    print("wsx inspect " + json.dumps(packet, separators=(",", ":")))


if __name__ == "__main__":
    main()
