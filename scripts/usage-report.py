#!/usr/bin/python3
"""Token, time and quality report for one project's manager-mode tasks.

Reads only the task state folder (docs/tasks.md "Task logs", "Manager"):
  <dir>/<n>/task.json, <dir>/<n>/log*.jsonl   worker results, checks, revisions
  <dir>/manager/log*.jsonl                     manager messages and replies
Usage fields are documented in docs/tasks.md ("Usage"). Design: docs/design/usage-audit.md.

  /usr/bin/python3 scripts/usage-report.py ~/.local/state/wsx/tasks/<project-id> [--since DAY | --days N] [--until DAY] [--task N]
The last lines are a summary, so a routine run reaches the manager as an event (docs/tasks.md "Routines").
"""
import argparse
import collections
import datetime
import json
import os
import re
import statistics
import sys

# ^ Cache price weights relative to base input tokens (Anthropic prompt caching: 5-minute
# write 1.25x, read 0.1x). Only used to split a turn's input cost into write and read shares.
WRITE_W, READ_W = 1.25, 0.10
BIG_CTX = 200_000


def read_log(folder):
    """Events of log.1.jsonl (older) then log.jsonl; bad lines are skipped."""
    events = []
    for name in ("log.1.jsonl", "log.jsonl"):
        path = os.path.join(folder, name)
        if not os.path.exists(path):
            continue
        with open(path, encoding="utf-8", errors="replace") as f:
            for line in f:
                try:
                    events.append(json.loads(line))
                except ValueError:
                    pass
    return events


def per_turn_cost(events):
    """Make `cost_micro_usd` this turn's part on lines from before `session_cost_micro_usd`
    existed (commit 94f0b26), where it held the session's running total."""
    prev = 0
    for e in events:
        u = e.get("usage")
        if not u or "session_cost_micro_usd" in u:
            continue
        total = u.get("cost_micro_usd", 0)
        u["cost_micro_usd"] = total - prev if total >= prev else total
        prev = total


def day(ms):
    return datetime.datetime.fromtimestamp(ms / 1000).strftime("%m-%d")


def usd(micro):
    return f"{micro / 1e6:.2f}"


def k(tokens):
    return f"{tokens / 1000:.0f}k" if tokens else "-"


def cache_split(u):
    """(write, read) input-equivalent token weights of one usage."""
    return u.get("cache_creation_tokens", 0) * WRITE_W, u.get("cache_read_tokens", 0) * READ_W


def table(title, head, rows):
    print(f"\n## {title}\n")
    if not rows:
        print("(none)")
        return
    cells = [[str(c) for c in r] for r in [head] + rows]
    width = [max(len(r[i]) for r in cells) for i in range(len(head))]
    for n, r in enumerate(cells):
        print("| " + " | ".join(c.ljust(width[i]) for i, c in enumerate(r)) + " |")
        if n == 0:
            print("|" + "|".join("-" * (w + 2) for w in width) + "|")


def mask(text):
    """A command's shape: paths, numbers and heredoc bodies folded so repeats group."""
    text = text.split("\n", 1)[0]
    text = re.sub(r"/Users/[^/ ]+/", "~/", text)
    text = re.sub(r"wsx-t\d+", "wsx-tN", text)
    text = re.sub(r"\d+", "N", text)
    return text[:64]


def task_turns(task, events):
    """One row per worker turn with its step, replaying docs/tasks.md "Steps":
    a `done` result moves to the next step; turns after the last step are `after`."""
    steps = [s["name"] for s in task.get("steps") or []] or ["build"]
    idx, rot_next, limit_next, last_end, trigger, turns = 0, False, False, None, None, []
    for e in events:
        kind = e.get("kind")
        if kind == "rotated":
            rot_next = True
        elif kind in ("check_run", "check_retry", "conflict", "checks") or kind == "message" and e.get("mode") != "prompt":
            trigger = e.get("mode") or kind
        elif kind == "result":
            u = e.get("usage") or {}
            # ^ a session-limit answer: its cost is the work the limit cut off; the next turn
            # resumes the same session hours later
            limit = u.get("model") == "<synthetic>"
            step = "limit hit" if limit else steps[idx] if idx < len(steps) else "after"
            start = e["unix_ms"] - u.get("duration_ms", 0)
            turns.append({"ms": e["unix_ms"], "step": step, "u": u, "status": e.get("status"),
                          "after_rotation": rot_next, "after_limit": limit_next,
                          "gap": start - last_end if last_end is not None else None,
                          "trigger": trigger or "no event (result retry, limit resume)"})
            last_end, trigger = e["unix_ms"], None
            rot_next, limit_next = False, limit
            if not limit and e.get("status") == "done" and idx < len(steps):
                idx += 1
    return turns


def area(task):
    """A task's kind for before/after comparison: the area of its first declared or touched path in a folder."""
    paths = (task.get("files") or []) + (task.get("paths") or [])
    path = next((p for p in paths if "/" in p.strip("*/")), paths[0] if paths else "")
    parts = path.split("/")
    return parts[1] if parts[0] == "crates" and len(parts) > 1 else parts[0] or "?"


def task_row(task, events, turns):
    first = events[0]["unix_ms"] if events else task.get("created_unix_ms", 0)
    landed = next((e["unix_ms"] for e in events if e.get("kind") == "landed"), None)
    revisions = sum(1 for e in events if e.get("kind") == "revision")
    adds = sum(1 for e in events if e.get("kind") == "message" and e.get("mode") == "add"
               and any(p.get("kind") == "revision" and p["unix_ms"] < e["unix_ms"] for p in events))
    failed = sum(1 for e in events if e.get("kind") in ("checks", "check_run")
                 and any(r.get("exit") != 0 for r in e.get("results") or []))
    wait, queued = 0, None
    for e in events:
        if e.get("kind") == "check_queued" and queued is None:
            queued = e["unix_ms"]
        elif e.get("kind") in ("checks", "check_run") and queued is not None:
            wait += e["unix_ms"] - queued
            queued = None
    ctx = [t["u"].get("context_tokens", 0) for t in turns]
    models = collections.Counter(t["u"].get("model") for t in turns if t["u"].get("model"))
    return {
        "id": task["id"], "title": (task.get("title") or "")[:28], "state": task.get("state", "?"),
        "model": (models.most_common(1)[0][0] if models else task.get("model", "?")).replace("claude-", ""),
        "steps": len(task.get("steps") or []), "turns": len(turns),
        "cost": sum(t["u"].get("cost_micro_usd", 0) for t in turns),
        "dur": sum(t["u"].get("duration_ms", 0) for t in turns),
        "ctx_max": max(ctx, default=0), "ctx_trail": " ".join(k(c) for c in ctx[-6:]),
        "rot": sum(1 for e in events if e.get("kind") == "rotated"),
        "limits": sum(1 for e in events if e.get("kind") == "result"
                      and (e.get("usage") or {}).get("model") == "<synthetic>"),
        "rev": revisions, "sendback": adds, "failed": failed, "check_wait": wait,
        "wall": (landed - first) if landed else None, "day": day(first), "kind": area(task),
    }


def manager_turns(events):
    origin = {e["id"]: e.get("from", "?") for e in events if e.get("kind") == "message" and "id" in e}
    out = []
    for e in events:
        if e.get("kind") != "reply" or not e.get("usage"):
            continue
        froms = {origin.get(i, "?") for i in e.get("to") or []}
        kind = "owner" if "owner" in froms else "recap" if "recap" in froms else \
            "events" if froms == {"events"} else "other"
        out.append({"ms": e["unix_ms"], "kind": kind, "u": e["usage"], "quiet": e.get("quiet", False)})
    return out


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("state_dir")
    ap.add_argument("--since", help="first local day, YYYY-MM-DD")
    ap.add_argument("--days", type=int, help="the last N days; a fixed argv for a routine")
    ap.add_argument("--until", help="last local day, YYYY-MM-DD; with --since compares two windows")
    ap.add_argument("--task", type=int, help="print every turn of this task")
    ap.add_argument("--top", type=int, default=25, help="rows in the per-task and repeat tables")
    a = ap.parse_args()
    if not os.path.isdir(a.state_dir):
        sys.exit(f"usage-report: no such folder: {a.state_dir}")
    try:
        since = datetime.datetime.strptime(a.since, "%Y-%m-%d").timestamp() * 1000 if a.since else 0
        if a.days:
            a.since = (datetime.date.today() - datetime.timedelta(days=a.days - 1)).isoformat()
            since = datetime.datetime.strptime(a.since, "%Y-%m-%d").timestamp() * 1000
        until = (datetime.datetime.strptime(a.until, "%Y-%m-%d") + datetime.timedelta(days=1)).timestamp() * 1000 \
            if a.until else float("inf")
    except ValueError as err:
        sys.exit(f"usage-report: {err}")

    tasks, all_turns, repeats = [], [], collections.Counter()
    for name in os.listdir(a.state_dir):
        folder = os.path.join(a.state_dir, name)
        if not name.isdigit() or not os.path.exists(os.path.join(folder, "task.json")):
            continue
        try:
            with open(os.path.join(folder, "task.json")) as f:
                task = json.load(f)
        except (OSError, ValueError) as err:
            print(f"usage-report: skipped task {name}: {err}", file=sys.stderr)
            continue
        events = read_log(folder)
        per_turn_cost(e for e in events if e.get("kind") == "result")
        if not events or not since <= events[0]["unix_ms"] < until:
            continue
        turns = task_turns(task, events)
        for t in turns:
            t["task"] = task["id"]
        all_turns += turns
        tasks.append(task_row(task, events, turns))
        repeats.update(mask(e.get("text", "")) for e in events if e.get("kind") == "agent")
        if a.task == task["id"]:
            table(f"Task {task['id']} turns", ["day", "step", "status", "cost $", "ctx", "cache w/r tok", "rot", "limit", "model"],
                  [[day(t["ms"]), t["step"], t["status"], usd(t["u"].get("cost_micro_usd", 0)), k(t["u"].get("context_tokens", 0)),
                    f"{k(t['u'].get('cache_creation_tokens', 0))}/{k(t['u'].get('cache_read_tokens', 0))}",
                    "y" if t["after_rotation"] else "", "y" if t["after_limit"] else "", t["u"].get("model", "")]
                   for t in turns])
    mevents = read_log(os.path.join(a.state_dir, "manager"))
    per_turn_cost(e for e in mevents if e.get("kind") == "reply")
    mevents = [e for e in mevents if since <= e.get("unix_ms", 0) < until]
    mturns = manager_turns(mevents)

    # Per day
    days = collections.defaultdict(lambda: collections.Counter())
    for t in all_turns:
        d = days[day(t["ms"])]
        d["wcost"] += t["u"].get("cost_micro_usd", 0)
        d["wturns"] += 1
        w, r = cache_split(t["u"])
        d["w"] += w
        d["r"] += r
        d["big"] += t["u"].get("context_tokens", 0) > BIG_CTX
    for t in mturns:
        d = days[day(t["ms"])]
        d["mcost"] += t["u"].get("cost_micro_usd", 0)
        d["mturns"] += 1
    for r in tasks:
        days[r["day"]]["tasks"] += 1
        days[r["day"]]["landed"] += r["wall"] is not None
    table("Per day (worker cost from results, manager cost from replies; a task counts on its first day)",
          ["day", "tasks", "landed", "worker $", "w turns", "turns/task", "$/turn", "ctx>200k turns", "cache write share",
           "manager $", "m turns"],
          [[dk, d["tasks"], d["landed"], usd(d["wcost"]), d["wturns"], f"{d['wturns'] / max(d['tasks'], 1):.1f}",
            usd(d["wcost"] / max(d["wturns"], 1)), d["big"],
            f"{d['w'] / max(d['w'] + d['r'], 1):.0%}", usd(d["mcost"]), d["mturns"]] for dk, d in sorted(days.items())])

    # Per step and per model
    def group(key, title, turns=all_turns):
        g = collections.defaultdict(list)
        for t in turns:
            g[key(t)].append(t)
        rows = []
        for name, ts in sorted(g.items(), key=lambda x: -sum(t["u"].get("cost_micro_usd", 0) for t in x[1])):
            cost = sum(t["u"].get("cost_micro_usd", 0) for t in ts)
            ctx = [t["u"]["context_tokens"] for t in ts if t["u"].get("context_tokens")]
            w = sum(cache_split(t["u"])[0] for t in ts)
            # ^ each model call re-reads the context, so cache reads / context ~ calls in the turn
            calls = [t["u"].get("cache_read_tokens", 0) / t["u"]["context_tokens"] for t in ts if t["u"].get("context_tokens")]
            rows.append([name, len(ts), usd(cost), usd(cost / len(ts)),
                         k(statistics.median(ctx)) if ctx else "-",
                         f"{statistics.median(t['u'].get('duration_ms', 0) for t in ts) / 60000:.1f}",
                         f"{statistics.median(calls):.0f}" if calls else "-",
                         f"{w / max(w + sum(cache_split(t['u'])[1] for t in ts), 1):.0%}"])
        table(title, ["", "turns", "cost $", "$/turn", "median ctx", "median min", "~calls/turn", "cache write share"], rows)
    group(lambda t: t["step"], "Per worker step (replayed from task.json steps; after = check retries, adds, conflicts)")
    group(lambda t: (t["u"].get("model") or "unrecorded").replace("claude-", ""), "Per worker model")
    group(lambda t: t["trigger"], "What started each `after` turn (latest event since the task's previous turn)",
          [t for t in all_turns if t["step"] == "after"])
    group(lambda t: "limit hit" if t["step"] == "limit hit" else "after rotation" if t["after_rotation"]
          else "after session limit" if t["after_limit"]
          else "ctx > 200k" if t["u"].get("context_tokens", 0) > BIG_CTX else "other", "Worker turns by condition")
    group(lambda t: "first turn" if t["gap"] is None else "idle < 5 min" if t["gap"] < 300_000
          else "idle 5-60 min" if t["gap"] < 3_600_000 else "idle > 1 h",
          "Worker turns by idle time since the task's previous turn (cache expiry)")

    # Per task
    rows = sorted(tasks, key=lambda r: -r["cost"])[:a.top]
    table(f"Per task (top {a.top} by cost)",
          ["#", "title", "day", "state", "model", "steps", "turns", "cost $", "agent min", "max ctx", "last ctx", "rot", "limit",
           "rev", "send-back", "failed checks", "check wait min", "wall to land h"],
          [[r["id"], r["title"], r["day"], r["state"], r["model"], r["steps"], r["turns"], usd(r["cost"]), f"{r['dur'] / 60000:.0f}",
            k(r["ctx_max"]), r["ctx_trail"], r["rot"], r["limits"], r["rev"], r["sendback"], r["failed"],
            f"{r['check_wait'] / 60000:.0f}", f"{r['wall'] / 3.6e6:.1f}" if r["wall"] else "-"] for r in rows])
    def medians(rs):
        return [len(rs), usd(statistics.median(r["cost"] for r in rs)), statistics.median(r["turns"] for r in rs),
                f"{statistics.median(r['wall'] for r in rs) / 3.6e6:.1f}",
                f"{statistics.median(r['check_wait'] for r in rs) / 60000:.0f}",
                f"{sum(r['sendback'] for r in rs) / len(rs):.1f}", f"{sum(r['failed'] for r in rs) / len(rs):.1f}"]
    kinds = collections.defaultdict(list)
    for r in tasks:
        if r["wall"]:
            kinds[r["kind"]].append(r)
    table("Landed tasks by kind (area of the first declared path; medians, then per-task means)",
          ["kind", "landed", "$/task", "turns", "wall h", "check wait min", "send-backs", "failed checks"],
          [[kind] + medians(rs) for kind, rs in sorted(kinds.items(), key=lambda x: -len(x[1]))])

    # Manager turns
    g = collections.defaultdict(list)
    for t in mturns:
        g[t["kind"]].append(t)
    table("Manager turns by kind (reply.to -> message.from)",
          ["kind", "turns", "cost $", "$/turn", "median ctx", "quiet", "cache write share"],
          [[kind, len(ts), usd(sum(t["u"].get("cost_micro_usd", 0) for t in ts)),
            usd(sum(t["u"].get("cost_micro_usd", 0) for t in ts) / len(ts)),
            k(statistics.median([t["u"]["context_tokens"] for t in ts if t["u"].get("context_tokens")] or [0])),
            sum(t["quiet"] for t in ts),
            f"{sum(cache_split(t['u'])[0] for t in ts) / max(sum(sum(cache_split(t['u'])) for t in ts), 1):.0%}"]
           for kind, ts in sorted(g.items())])
    print(f"\nManager rotations: {sum(1 for e in mevents if e.get('kind') == 'rotated')}")

    repeats.update("manager: " + mask(e.get("text", "")) for e in mevents if e.get("kind") == "agent")
    table("Repeated agent commands (agent log lines, numbers and paths masked)", ["count", "command shape"],
          [[n, s] for s, n in repeats.most_common(a.top)])

    # ^ Summary last: a routine's event carries the last 10 lines of its output.
    landed = [r for r in tasks if r["wall"]]
    wcost = sum(t["u"].get("cost_micro_usd", 0) for t in all_turns)
    mcost = sum(t["u"].get("cost_micro_usd", 0) for t in mturns)
    big = sum(t["u"].get("cost_micro_usd", 0) for t in all_turns if t["u"].get("context_tokens", 0) > BIG_CTX)
    cold = sum(t["u"].get("cost_micro_usd", 0) for t in all_turns if (t["gap"] or 0) >= 3_600_000)
    print("\n## Summary\n")
    print(f"window {a.since or 'start'} .. {a.until or 'now'}: tasks {len(tasks)}, landed {len(landed)}, "
          f"worker ${usd(wcost)} in {len(all_turns)} turns, manager ${usd(mcost)} in {len(mturns)} turns")
    if landed:
        n, cost, turns, wall, wait, back, failed = medians(landed)
        print(f"per landed task: ${cost}, {turns} turns, {wall} h wall, {wait} min check wait, "
              f"{back} send-backs, {failed} failed check runs")
    print(f"turns over {BIG_CTX // 1000}k context: ${usd(big)}; turns after > 1 h idle (cold cache): ${usd(cold)}")


if __name__ == "__main__":
    main()
