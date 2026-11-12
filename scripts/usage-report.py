#!/usr/bin/python3
"""Token, time and quality report for one project's manager-mode tasks.

Reads only the task state folder (docs/tasks.md "Task logs", "Manager"):
  <dir>/<n>/task.json, <dir>/<n>/log*.jsonl   worker results, checks, revisions
  <dir>/manager/log*.jsonl                     manager messages and replies
Usage fields are documented in docs/tasks.md ("Usage"). Design: docs/design/usage-audit.md.

  /usr/bin/python3 scripts/usage-report.py ~/.local/state/wsx/tasks/<project-id>... [--since DAY | --days N] [--until DAY] [--task N]
Several state folders (`~/.local/state/wsx/tasks/*`) add up; the manager table by trigger names
the event kinds that woke a turn, to measure which turns only said what wsx knew.
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

# ^ Cache price weights relative to base input tokens: Claude Code writes 1-hour cache entries
# (transcripts: `ephemeral_1h_input_tokens`, 5m always 0), priced 2x; reads 0.1x. Only used to
# split a turn's input cost into write and read shares.
WRITE_W, READ_W = 2.0, 0.10
BIG_CTX = 200_000


def own_quote(e):
    """A reply that opens with a quote the manager wrote: wsx adds the quote of the owner message
    a reply answers (`quote` is in `to`), which is not the manager's choice (docs/tasks.md "A quote")."""
    if "quote" in e:
        return e["quote"] not in e.get("to", [])
    return (e.get("text") or "").startswith("> ")


def read_log(folder):
    """Events of log.1.jsonl, log.2.jsonl ... (every roll, oldest first) then log.jsonl; bad lines
    are skipped."""
    events = []
    rolls = sorted((n for n in os.listdir(folder) if re.fullmatch(r"log\.\d+\.jsonl", n)),
                   key=lambda n: int(n.split(".")[1])) if os.path.isdir(folder) else []
    for name in rolls + ["log.jsonl"]:
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


def cost(t):
    return t["u"].get("cost_micro_usd", 0)


def write_share(turns):
    """Cache writes' part of the cache-priced input of these turns."""
    w = sum(t["u"].get("cache_creation_tokens", 0) for t in turns) * WRITE_W
    r = sum(t["u"].get("cache_read_tokens", 0) for t in turns) * READ_W
    return f"{w / max(w + r, 1):.0%}"


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
    """One row per worker turn with its step and trigger. A result that names them
    (docs/tasks.md "Usage") is read as logged; an older one replays docs/tasks.md "Steps":
    a `done` result moves to the next step; turns after the last step are `after`."""
    steps = [s["name"] for s in task.get("steps") or []] or ["build"]
    idx, rot_next, limit_next, last_end, trigger, turns = 0, False, False, None, None, []
    for e in events:
        kind = e.get("kind")
        if kind == "rotated":
            rot_next = True
        elif kind in ("check_run", "check_retry", "conflict", "checks") or kind == "message" and e.get("mode") != "prompt":
            trigger = e.get("mode") or kind
        elif kind == "ping":
            # ^ a no-op turn that kept the cache warm during a wait: no step, no result
            u = e.get("usage") or {}
            start = e["unix_ms"] - u.get("duration_ms", 0)
            turns.append({"ms": e["unix_ms"], "step": "ping", "u": u, "status": None,
                          "after_rotation": False, "after_limit": False,
                          "gap": start - last_end if last_end is not None else None,
                          "logged": True, "trigger": "ping", "reexec": False})
            last_end = e["unix_ms"]
        elif kind == "result":
            u = e.get("usage") or {}
            # ^ a session-limit answer: its cost is the work the limit cut off; the next turn
            # resumes the same session hours later
            limit = u.get("model") == "<synthetic>"
            step = "limit hit" if limit else steps[idx] if idx < len(steps) else "after"
            if "trigger" in e:
                step, trigger = e.get("step") or "no steps", e["trigger"]
            start = e["unix_ms"] - u.get("duration_ms", 0)
            turns.append({"ms": e["unix_ms"], "step": step, "u": u, "status": e.get("status"),
                          "after_rotation": rot_next, "after_limit": limit_next,
                          "gap": start - last_end if last_end is not None else None,
                          "logged": "trigger" in e, "reexec": bool(e.get("reexec")),
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
        "cost": sum(map(cost, turns)),
        "dur": sum(t["u"].get("duration_ms", 0) for t in turns),
        "ctx_max": max(ctx, default=0), "ctx_trail": " ".join(k(c) for c in ctx[-6:]),
        "rot": sum(1 for e in events if e.get("kind") == "rotated"),
        "limits": sum(1 for t in turns if t["step"] == "limit hit"),
        "rev": revisions, "sendback": adds, "failed": failed, "check_wait": wait,
        "wall": (landed - first) if landed else None, "day": day(first), "kind": area(task),
    }


# ^ Event kinds that start no turn (`state_wakes`, docs/tasks.md "Manager"): a turn they join is
# named by its other events.
ALONG = ("landed", "closed", "stopped", "waiting:limit", "waiting:disk")
# ^ `wsx task` verbs that only read.
READ_VERBS = ("ledger", "show", "chat", "log", "status")


def event_kind(line):
    """An events line's kind: `ready`, `waiting:owner`, `stopped` (a wait on owner after a stop by
    request), `deploy succeeded`, `routine nightly failed`, `approval push approved`."""
    m = re.match(r"#\d+ (\w+) r\d+: ", line)
    if m:
        if m.group(1) != "waiting":
            return m.group(1)
        wait = re.search(r"; waits on (\w+): ([^;]*)", line)
        if not wait:
            return "waiting"
        return "stopped" if wait.group(1) == "owner" and "stopped by request" in wait.group(2) \
            else "waiting:" + wait.group(1)
    m = re.match(r"approval \S+ \((\w+)\) (\w+)", line)
    if m:
        return "approval " + " ".join(m.groups())
    m = re.match(r"(deploy|release) of \S+ (\w+)", line) or re.match(r"(routine \S+) (\w+)", line)
    return " ".join(m.groups()) if m else line.split(" ", 1)[0]


def manager_turns(events):
    """Each reply or ping with usage: `kind` by who wrote its messages, `trigger` the event kind
    that woke an events turn, `acted` whether it ran a `wsx task` verb that changes something."""
    messages = {e["id"]: e for e in events if e.get("kind") == "message" and "id" in e}
    out, steps = [], []
    for e in events:
        if e.get("kind") == "delivered":
            steps = []
        elif e.get("kind") == "agent":
            steps.append(e.get("text", ""))
        if e.get("kind") not in ("reply", "ping") or not e.get("usage"):
            continue
        to = [messages[i] for i in e.get("to") or [] if i in messages]
        froms = {messages.get(i, {}).get("from", "?") for i in e.get("to") or []}
        kind = "ping" if e["kind"] == "ping" else "owner" if "owner" in froms else \
            "recap" if "recap" in froms else "events" if froms == {"events"} else "other"
        trigger = kind
        if kind == "events":
            # ^ An output line of a routine or deploy is indented.
            kinds = [event_kind(line) for m in to for line in m.get("text", "").split("\n")
                     if line and not line.startswith(" ")]
            trigger = next((k for k in kinds if k not in ALONG), kinds[0] if kinds else "events")
        text = (e.get("text") or "").strip()
        verbs = [m.group(1) for step in steps for m in re.finditer(r"\bwsx task (\w+)", step)]
        out.append({"ms": e["unix_ms"], "kind": kind, "trigger": trigger, "u": e["usage"],
                    "quiet": e.get("quiet", False), "acted": any(v not in READ_VERBS for v in verbs),
                    "nothing": text in ("", "-"), "line": text not in ("", "-") and "\n" not in text})
        if e["kind"] == "reply":
            steps = []
    return out


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("state_dir", nargs="+", help="a project's task state folder; several add up")
    ap.add_argument("--since", help="first local day, YYYY-MM-DD")
    ap.add_argument("--days", type=int, help="the last N days; a fixed argv for a routine")
    ap.add_argument("--until", help="last local day, YYYY-MM-DD; with --since compares two windows")
    ap.add_argument("--task", type=int, help="print every turn of this task")
    ap.add_argument("--top", type=int, default=25, help="rows in the per-task and repeat tables")
    a = ap.parse_args()
    for state_dir in a.state_dir:
        if not os.path.isdir(state_dir):
            sys.exit(f"usage-report: no such folder: {state_dir}")
    if a.days:
        a.since = (datetime.date.today() - datetime.timedelta(days=a.days - 1)).isoformat()
    try:
        since = datetime.datetime.strptime(a.since, "%Y-%m-%d").timestamp() * 1000 if a.since else 0
        until = (datetime.datetime.strptime(a.until, "%Y-%m-%d") + datetime.timedelta(days=1)).timestamp() * 1000 \
            if a.until else float("inf")
    except ValueError as err:
        sys.exit(f"usage-report: {err}")

    tasks, all_turns, rotations, repeats = [], [], [], collections.Counter()
    for folder in [os.path.join(d, name) for d in a.state_dir for name in os.listdir(d)]:
        name = os.path.basename(folder)
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
        all_turns += turns
        rotations += [e for e in events if e.get("kind") == "rotated"]
        tasks.append(task_row(task, events, turns))
        repeats.update(mask(e.get("text", "")) for e in events if e.get("kind") == "agent")
        if a.task == task["id"]:
            table(f"Task {task['id']} turns", ["day", "step", "trigger", "status", "cost $", "ctx", "calls", "cache w/r tok", "rot",
                                               "limit", "model"],
                  [[day(t["ms"]), t["step"], t["trigger"], t["status"], usd(cost(t)), k(t["u"].get("context_tokens", 0)), t["u"].get("calls", "-"),
                    f"{k(t['u'].get('cache_creation_tokens', 0))}/{k(t['u'].get('cache_read_tokens', 0))}",
                    "y" if t["after_rotation"] else "", "y" if t["after_limit"] else "", t["u"].get("model", "")]
                   for t in turns])
    mevents, mturns, quotes = [], [], []
    for state_dir in a.state_dir:
        chats = os.path.join(state_dir, "chats")
        managers = [("manager", os.path.join(state_dir, "manager"))]
        if os.path.isdir(chats):
            managers += [(f"chat {c}", os.path.join(chats, c)) for c in sorted(os.listdir(chats))]
        for who, folder in managers:
            said = [e for e in read_log(folder) if e.get("kind") == "reply" and since <= e.get("unix_ms", 0) < until
                    and not e.get("post") and (e.get("text") or "").strip() not in ("", "-")]
            quoting = sum(1 for e in said if own_quote(e))
            if said:
                quotes.append([os.path.basename(os.path.normpath(state_dir)), who, len(said), quoting,
                               f"{100 * quoting / len(said):.1f}%"])
        events = read_log(os.path.join(state_dir, "manager"))
        per_turn_cost(e for e in events if e.get("kind") == "reply")
        events = [e for e in events if since <= e.get("unix_ms", 0) < until]
        mevents += events
        # ^ Message ids are a project's own.
        mturns += manager_turns(events)

    table("Quotes (replies that say something, and those that open with a quote the manager wrote, per manager)",
          ["project", "manager", "replies", "quoting", "share"], quotes)

    # Per day
    days, wturns = collections.defaultdict(collections.Counter), collections.defaultdict(list)
    for t in all_turns:
        d = days[day(t["ms"])]
        d["wcost"] += cost(t)
        d["wturns"] += 1
        d["big"] += t["u"].get("context_tokens", 0) > BIG_CTX
        wturns[day(t["ms"])].append(t)
    for t in mturns:
        d = days[day(t["ms"])]
        d["mcost"] += cost(t)
        d["mturns"] += 1
    for r in tasks:
        days[r["day"]]["tasks"] += 1
        days[r["day"]]["landed"] += r["wall"] is not None
    table("Per day (worker cost from results, manager cost from replies; a task counts on its first day)",
          ["day", "tasks", "landed", "worker $", "w turns", "turns/task", "$/turn", "ctx>200k turns", "cache write share",
           "manager $", "m turns"],
          [[dk, d["tasks"], d["landed"], usd(d["wcost"]), d["wturns"], f"{d['wturns'] / max(d['tasks'], 1):.1f}",
            usd(d["wcost"] / max(d["wturns"], 1)), d["big"],
            write_share(wturns[dk]), usd(d["mcost"]), d["mturns"]] for dk, d in sorted(days.items())])

    # Per step and per model
    def group(key, title, turns=all_turns):
        g = collections.defaultdict(list)
        for t in turns:
            g[key(t)].append(t)
        rows = []
        for name, ts in sorted(g.items(), key=lambda x: -sum(map(cost, x[1]))):
            total = sum(map(cost, ts))
            ctx = [t["u"]["context_tokens"] for t in ts if t["u"].get("context_tokens")]
            # ^ `calls` as logged; on older lines each model call re-reads the context, so
            # cache reads / context ~ calls in the turn
            calls = [t["u"]["calls"] if t["u"].get("calls") else t["u"].get("cache_read_tokens", 0) / t["u"]["context_tokens"]
                     for t in ts if t["u"].get("calls") or t["u"].get("context_tokens")]
            rows.append([name, len(ts), usd(total), usd(total / len(ts)),
                         k(statistics.median(ctx)) if ctx else "-",
                         f"{statistics.median(t['u'].get('duration_ms', 0) for t in ts) / 60000:.1f}",
                         f"{statistics.median(calls):.0f}" if calls else "-",
                         write_share(ts)])
        table(title, ["", "turns", "cost $", "$/turn", "median ctx", "median min", "~calls/turn", "cache write share"], rows)
    # ^ A step's cost is only the turns the step itself started: an owner message, restart,
    # check-run answer, rotation or conflict that came while the record still named the step is
    # listed by its trigger, not under the step (it was real work, not the step's).
    def step_key(t):
        own = t["trigger"] == "step" or not t["logged"] and t["trigger"].startswith("no event")
        return t["step"] if own else f"[{t['trigger']} turns]"
    group(step_key, "Per worker step (the turns each step started; [x turns] = turns started by x "
          "during any step; older lines replay steps from task.json, after = check retries, adds, conflicts)")
    group(lambda t: (t["u"].get("model") or "unrecorded").replace("claude-", ""), "Per worker model")
    group(lambda t: t["trigger"], "What started each turn (logged `trigger`)", [t for t in all_turns if t["logged"]])
    # ^ A turn a deploy re-exec carried keeps its real trigger (`reexec` on the result); only
    # lines from before that logged `restart`, which was the label of any of them.
    carried = [t for t in all_turns if t["reexec"]]
    if carried:
        print(f"\n{len(carried)} turns came through a deploy re-exec, ${usd(sum(map(cost, carried)))}; "
              "they are in the table above under their own trigger")
    group(lambda t: t["trigger"], "What started each `after` turn on older lines (latest event since the task's previous turn)",
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
          [[kind, len(ts), usd(sum(map(cost, ts))), usd(sum(map(cost, ts)) / len(ts)),
            k(statistics.median([t["u"]["context_tokens"] for t in ts if t["u"].get("context_tokens")] or [0])),
            sum(t["quiet"] for t in ts), write_share(ts)]
           for kind, ts in sorted(g.items())])
    g = collections.defaultdict(list)
    for t in mturns:
        g[t["trigger"]].append(t)
    table("Manager turns by trigger (the owner, a peer, or the event kind that woke it)",
          ["trigger", "turns", "cost $", "no action cmd", "- or empty", "one line"],
          [[trigger, len(ts), usd(sum(map(cost, ts))), sum(not t["acted"] for t in ts),
            sum(t["nothing"] for t in ts), sum(t["line"] for t in ts)]
           for trigger, ts in sorted(g.items(), key=lambda x: -sum(map(cost, x[1])))])
    print(f"\nManager rotations: {sum(1 for e in mevents if e.get('kind') == 'rotated')}")

    repeats.update("manager: " + mask(e.get("text", "")) for e in mevents if e.get("kind") == "agent")
    table("Repeated agent commands (agent log lines, numbers and paths masked)", ["count", "command shape"],
          [[n, s] for s, n in repeats.most_common(a.top)])

    # ^ Summary last: a routine's event carries the last 10 lines of its output.
    landed = [r for r in tasks if r["wall"]]
    wcost, mcost = sum(map(cost, all_turns)), sum(map(cost, mturns))
    big = sum(cost(t) for t in all_turns if t["u"].get("context_tokens", 0) > BIG_CTX)
    cold = sum(cost(t) for t in all_turns if (t["gap"] or 0) >= 3_600_000)
    print("\n## Summary\n")
    print(f"window {a.since or 'start'} .. {a.until or 'now'}: tasks {len(tasks)}, landed {len(landed)}, "
          f"worker ${usd(wcost)} in {len(all_turns)} turns, manager ${usd(mcost)} in {len(mturns)} turns")
    if landed:
        _, per_task, turns, wall, wait, back, failed = medians(landed)
        print(f"per landed task: ${per_task}, {turns} turns, {wall} h wall, {wait} min check wait, "
              f"{back} send-backs, {failed} failed check runs")
    print(f"turns over {BIG_CTX // 1000}k context: ${usd(big)}; turns after > 1 h idle (cold cache): ${usd(cold)}")
    idle = sum(1 for e in rotations if (e.get("reason") or "").startswith("idle"))
    print(f"worker rotations: {len(rotations) - idle} by size, {idle} after idle; "
          f"turns with a logged trigger: {sum(t['logged'] for t in all_turns)} of {len(all_turns)}")


if __name__ == "__main__":
    main()
