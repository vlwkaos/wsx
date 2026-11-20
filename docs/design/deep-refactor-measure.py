#!/usr/bin/env python3
"""Read-only measure for docs/design/deep-refactor.md. Prints counts and sizes only, never text.

  python3 -I docs/design/deep-refactor-measure.py commands [days]   wsx task|memory calls by agents
  python3 -I docs/design/deep-refactor-measure.py stores [days]     size and writes of each store
  python3 -I docs/design/deep-refactor-measure.py rows [days]       chat log rows by kind, old row shapes

Role of a transcript (~/.claude/projects/<dir>/*.jsonl): `worker` when <dir> ends in -t<N>
(a task copy), else `chat` (managers and chat agents). Bench, probe and fixture dirs are skipped.
A call is a Bash tool_use whose command runs `wsx|$WSX|<path>/wsx [flags] task|memory <sub>`;
only real subcommand names count, so prose such as `wsx task and` in a commit message drops out.
Workers that build wsx call `wsx memory` in tests: their counts are use by development, not use.
"""
import calendar, collections, glob, json, os, re, sys, time

PJ = os.path.expanduser("~/.claude/projects")
TASKS = os.path.expanduser("~/.local/state/wsx/tasks")
DAYS = int(sys.argv[2]) if len(sys.argv) > 2 else 14
SINCE = time.time() - DAYS * 86400
SKIP = re.compile(r"bench-|private-var|private-tmp|Library-Caches|-local-state-wsx$|--work-|scratch")
CALL = re.compile(r"(?:^|[\s;&|(`'\"=])(?:\S*/)?(?:wsx|\$\{?WSX\}?)\s+(?:-\S+\s+(?:\S+\s+)?)*?(task|memory)\s+([a-z][\w-]*)")
SUBS = {
    "task": set("new list show why review steer add stop land after hold trial close hand handback dismiss check "
                "deploy release push sync commit ledger deployed digest checkin post routine manager project say chat "
                "search approve deny check-run preview preview-host land-batch host boundary-profile chat-host "
                "repo-host manager-host result".split()),
    "memory": set("search recall add update capture start supersede get import reindex status list".split()),
}
FLAG = re.compile(r"\s(--[a-z][\w-]*)")


def role(path):
    d = os.path.basename(os.path.dirname(path))
    if SKIP.search(d):
        return None
    return "worker" if re.search(r"-t\d+$", d) else "chat"


def stamp(line):
    try:
        return calendar.timegm(time.strptime(line["timestamp"][:19], "%Y-%m-%dT%H:%M:%S"))
    except (KeyError, TypeError, ValueError):
        return 0


def commands():
    subs, flags, sessions, seen = collections.Counter(), collections.Counter(), collections.Counter(), set()
    for f in glob.glob(PJ + "/*/*.jsonl"):
        r = role(f)
        if r is None or os.path.getmtime(f) < SINCE:
            continue
        active = False
        for raw in open(f, errors="replace"):
            if '"tool_use"' not in raw or "wsx" not in raw.lower():
                continue
            try:
                line = json.loads(raw)
            except ValueError:
                continue
            if stamp(line) < SINCE:
                continue
            for block in (line.get("message") or {}).get("content") or []:
                if not isinstance(block, dict) or block.get("name") != "Bash" or block.get("id") in seen:
                    continue
                seen.add(block.get("id"))
                cmd = (block.get("input") or {}).get("command", "")
                for m in CALL.finditer(cmd):
                    group, sub = m.group(1), m.group(2)
                    if sub not in SUBS[group]:
                        continue
                    active = True
                    subs[(r, group, sub)] += 1
                    segment = re.split(r"\n|;|&&|\|", cmd[m.end():], maxsplit=1)[0]
                    for flag in set(FLAG.findall(segment)):
                        flags[(r, group, sub, flag)] += 1
        sessions[r] += active
    print(f"sessions with a call, last {DAYS} d: {dict(sessions)}")
    for group in ("task", "memory"):
        print(f"\n{group}: sub  chat / worker")
        names = {s for (_, g, s) in subs if g == group}
        for s in sorted(names, key=lambda s: -(subs[("chat", group, s)] + subs[("worker", group, s)])):
            print(f"  {s:16} {subs[('chat', group, s)]:5} / {subs[('worker', group, s)]}")
    print("\nflags (chat / worker), 5 or more:")
    keys = {k[1:] for k in flags}
    for k in sorted(keys, key=lambda k: -(flags[("chat",) + k] + flags[("worker",) + k])):
        c, w = flags[("chat",) + k], flags[("worker",) + k]
        if c + w >= 5:
            print(f"  {' '.join(k):40} {c:5} / {w}")


def rows_of(path):
    for raw in open(path, errors="replace"):
        try:
            yield json.loads(raw)
        except ValueError:
            continue


def ms(row):
    for key in ("unix_ms", "updated", "at"):
        v = row.get(key)
        if isinstance(v, (int, float)):
            return v if v > 1e12 else v * 1000
    return 0


def stores():
    kinds = collections.defaultdict(lambda: [0, 0, 0, 0])  # bytes, files, written in window, rows in window
    for root, _, files in os.walk(TASKS):
        rel = os.path.relpath(root, TASKS).split(os.sep)
        for name in files:
            p = os.path.join(root, name)
            try:
                st = os.stat(p)
            except OSError:
                continue
            if rel[0] == "chats":
                kind = "chats/<chat>/" + ("scratch/*" if "scratch" in rel else "/".join(rel[2:] + [name]))
            elif len(rel) > 1 and rel[1].isdigit():
                kind = "<project>/<n>/" + ("captures/*" if "captures" in rel else re.sub(r"\d+", "N", name))
            else:
                kind = "<project>/" + "/".join(rel[1:] + [name])
            kind = re.sub(r"log\.\d+\.jsonl", "log.N.jsonl", re.sub(r"[0-9a-f]{12,}", "H", kind))
            k = kinds[kind]
            k[0] += st.st_size
            k[1] += 1
            k[2] += st.st_mtime >= SINCE
            if name.endswith(".jsonl"):
                k[3] += sum(1 for r in rows_of(p) if ms(r) >= SINCE * 1000)
    print(f"{'store':52} {'KiB':>8} {'files':>6} {'written':>7} {'rows':>7}  (written, rows: last {DAYS} d)")
    for kind, (b, n, w, r) in sorted(kinds.items(), key=lambda x: -x[1][0])[:45]:
        print(f"{kind[:52]:52} {b // 1024:8} {n:6} {w:7} {r:7}")
    recs = collections.Counter()
    for p in glob.glob(TASKS + "/*/memory/records.jsonl"):
        last = {}
        for r in rows_of(p):
            last[r.get("slug")] = r
        for r in last.values():
            recs[(r.get("kind"), r.get("status", "active"), str(r.get("source", "")).split(":")[0])] += 1
    print("\nrecords now (kind, status, source form):")
    for k, n in recs.most_common():
        print(f"  {n:5} {k}")
    digest = collections.Counter(r.get("kind") for p in glob.glob(TASKS + "/*/search.jsonl") for r in rows_of(p))
    print(f"\nsearch.jsonl rows by kind: {dict(digest.most_common())}")


def rows():
    kinds, last = collections.Counter(), {}
    for p in glob.glob(TASKS + "/chats/*/log*.jsonl"):
        for r in rows_of(p):
            author = str(r.get("from") or "").split(":")[0]
            key = (r.get("kind"), author)
            if ms(r) >= SINCE * 1000:
                kinds[key] += 1
            if r.get("kind") == "ping" or author in ("manager", "manager-reply"):
                last[key] = max(last.get(key, 0), ms(r))
    print(f"chat log rows, last {DAYS} d (kind, author):")
    for k, n in kinds.most_common():
        print(f"  {n:6} {k}")
    print("old row shapes, newest:")
    for k, v in sorted(last.items()):
        print(f"  {k}: {time.strftime('%Y-%m-%d %H:%M', time.localtime(v / 1000))}")


if __name__ == "__main__":
    {"commands": commands, "stores": stores, "rows": rows}[sys.argv[1] if len(sys.argv) > 1 else "commands"]()
