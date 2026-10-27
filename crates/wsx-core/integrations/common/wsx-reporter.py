"""Resolve removed reporters without daemon startup. See docs/agent-reporting.md."""
import os
import stat
import sys
import tempfile


def _executable(candidate, uid):
    entry = os.lstat(candidate)
    resolved = os.path.realpath(candidate)
    target = os.stat(resolved)
    parent = os.stat(os.path.dirname(resolved))
    if (entry.st_uid not in (0, uid) or not stat.S_ISREG(target.st_mode)
            or target.st_uid not in (0, uid) or target.st_mode & 0o022
            or not target.st_mode & 0o111 or not stat.S_ISDIR(parent.st_mode)
            or parent.st_uid not in (0, uid) or parent.st_mode & 0o022):
        return None
    return resolved


def _state_dir():
    # ^ runtime::state_dir (docs/daemon-discovery.md): an empty or relative value is ignored.
    state, home = os.environ.get("XDG_STATE_HOME", ""), os.environ.get("HOME", "")
    if os.path.isabs(state):
        return os.path.join(state, "wsx")
    if os.path.isabs(home):
        return os.path.join(home, ".local/state/wsx")
    return os.path.join(tempfile.gettempdir(), "wsx")


def resolve_reporter(original):
    if not os.path.isabs(original):
        return None
    socket = os.environ.get("WSX_SOCKET") or os.path.join(_state_dir(), "wsx.sock")
    if not os.path.isabs(socket):
        return None
    try:
        uid = os.geteuid()
        directory = os.lstat(os.path.dirname(socket))
        if not stat.S_ISDIR(directory.st_mode) or directory.st_uid != uid or directory.st_mode & 0o077:
            return None
        stable = os.path.splitext(socket)[0] + ".reporter"
        try:
            entry = os.lstat(stable)
            if not stat.S_ISLNK(entry.st_mode) or entry.st_uid != uid:
                return None
            resolved = _executable(stable, uid)
            return resolved if resolved != original else None
        except FileNotFoundError:
            pass
        # Recover before an older daemon can publish its entry, or after keg removal.
        for directory in os.environ.get("PATH", "").split(os.pathsep)[:32]:
            if not os.path.isabs(directory):
                continue
            try:
                resolved = _executable(os.path.join(directory, "wsx"), uid)
                if resolved and resolved != original:
                    return resolved
            except OSError:
                pass
    except OSError:
        pass
    return None


if __name__ == "__main__":
    resolved = resolve_reporter(sys.argv[1]) if len(sys.argv) == 2 else None
    if resolved:
        print(resolved)
