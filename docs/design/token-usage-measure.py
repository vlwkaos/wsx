#!/usr/bin/python3
"""Measures for docs/design/token-usage.md. Window: tasks created and calls made from 2026-10-28.
Part 1: per-task turns (log.jsonl), per-call usage and tool_result sizes (Claude transcripts).
Part 2: cache writes per call by the gap before it. Part 3: context at the start of each step turn."""
import json, os, sys, glob, collections, datetime
ST = os.path.expanduser("~/.local/state/wsx/tasks/wsx-8cbe3f08")
PJ = os.path.expanduser("~/.claude/projects")
# ^ Claude names a transcript dir after the checkout path with / as -; set WSX_MAIN to the main checkout path
MAIN = os.environ.get("WSX_MAIN", os.getcwd()).replace("/", "-")
SINCE = datetime.datetime(2026, 10, 28).timestamp() * 1000
P = {"opus": (4, 20, 0.20, 8), "sonnet": (2, 10, 0.10, 4), "haiku": (1, 5, 0.10, 2)}
def price(model):
    for k, v in P.items():
        if model and k in model: return v
    return P["opus"]
def events(folder):
    out = []
    for n in ("log.1.jsonl", "log.jsonl"):
        p = os.path.join(folder, n)
        if os.path.exists(p):
            for l in open(p, errors="replace"):
                try: out.append(json.loads(l))
                except ValueError: pass
    return out
def blen(c):
    if isinstance(c, str): return len(c)
    if isinstance(c, list): return sum(len(x.get("text", "")) if isinstance(x, dict) else 0 for x in c)
    return 0
def transcript(path):
    calls, seen, names, results = [], set(), {}, []
    for l in open(path, errors="replace"):
        try: e = json.loads(l)
        except ValueError: continue
        m = e.get("message") or {}
        if e.get("type") == "assistant":
            for b in m.get("content") or []:
                if isinstance(b, dict) and b.get("type") == "tool_use":
                    inp = b.get("input") or {}
                    nm = b["name"]
                    if nm == "Bash":
                        cmd = inp.get("command", "")
                        nm = "Bash:check-run" if "check-run" in cmd else "Bash:cargo" if "cargo " in cmd else "Bash:read" if any(cmd.lstrip().startswith(x) for x in ("rg", "sed", "cat", "grep", "head", "ls", "git diff", "git show", "git log")) else "Bash:other"
                    names[b["id"]] = nm
            mid = m.get("id")
            if mid in seen or not m.get("usage"): continue
            seen.add(mid)
            u = m["usage"]
            calls.append({"model": m.get("model"), "in": u.get("input_tokens", 0), "w": u.get("cache_creation_input_tokens", 0), "r": u.get("cache_read_input_tokens", 0), "o": u.get("output_tokens", 0)})
        elif e.get("type") == "user" and isinstance(m.get("content"), list):
            for b in m["content"]:
                if isinstance(b, dict) and b.get("type") == "tool_result":
                    results.append((len(calls), names.get(b.get("tool_use_id"), "?"), blen(b.get("content")) / 4))
    return calls, results
def cost(c):
    pi, po, pr, pw = price(c["model"])
    return (c["in"] * pi + c["o"] * po + c["r"] * pr + c["w"] * pw) / 1e6
agg = collections.Counter(); by_tool = collections.Counter(); carried = collections.Counter(); ntool = collections.Counter()
first_r, first_w, turns_trig, turns_step, step_cost, ctx_start, tasks = [], [], collections.Counter(), collections.Counter(), collections.Counter(), [], 0
calls_per_turn = []
for folder in sorted(glob.glob(ST + "/[0-9]*")):
    try: t = json.load(open(folder + "/task.json"))
    except Exception: continue
    if t.get("created_unix_ms", 0) < SINCE or t.get("adapter", "claude") != "claude": continue
    tasks += 1
    for e in events(folder):
        if e.get("kind") == "result" and e.get("usage"):
            u = e["usage"]
            trig = e.get("trigger") or "?"; step = e.get("step") or "?"
            turns_trig[trig] += 1; turns_step[step] += 1
            step_cost[step] += u.get("cost_micro_usd", 0) / 1e6
            calls_per_turn.append(u.get("calls", 0))
    wc = t.get("working_copy", "")
    pdir = os.path.join(PJ, wc.replace("/", "-").replace(".", "-"))
    for path in glob.glob(pdir + "/*.jsonl"):
        calls, results = transcript(path)
        if not calls: continue
        first_r.append(calls[0]["r"]); first_w.append(calls[0]["w"])
        for c in calls:
            agg["calls"] += 1; agg["r"] += c["r"]; agg["w"] += c["w"]; agg["o"] += c["o"]; agg["usd"] += cost(c)
            ctx_start.append(c["r"] + c["w"] + c["in"])
        n = len(calls)
        for idx, nm, tok in results:
            by_tool[nm] += tok; ntool[nm] += 1
            carried[nm] += tok * max(n - idx, 0)  # ^ tokens re-read by later calls (upper bound: ignores compaction)
    # ^ Subagent transcripts live beside the session: <session>/subagents/agent-*.jsonl
    for path in glob.glob(pdir + "/*/subagents/*.jsonl"):
        for c in transcript(path)[0]:
            agg["sub_calls"] += 1; agg["sub_usd"] += cost(c)
print("tasks", tasks, "calls", agg["calls"], "usd %.2f" % agg["usd"], "subagent calls", agg["sub_calls"], "usd %.2f" % agg["sub_usd"])
print("read M %.1f write M %.2f out M %.2f" % (agg["r"] / 1e6, agg["w"] / 1e6, agg["o"] / 1e6))
pr = 0.20
print("read usd share %.0f%%" % (100 * agg["r"] * pr / 1e6 / agg["usd"]))
s = sorted(ctx_start); q = lambda p: s[int(p * (len(s) - 1))]
print("context per call p50 %dk p90 %dk mean %dk" % (q(.5) / 1e3, q(.9) / 1e3, sum(s) / len(s) / 1e3))
fr = sorted(first_r); fw = sorted(first_w)
print("first call read p50 %d write p50 %d n=%d; read==0 count %d" % (fr[len(fr) // 2], fw[len(fw) // 2], len(fr), sum(1 for x in fr if x == 0)))
cp = sorted(calls_per_turn); print("turns", len(cp), "calls/turn p50", cp[len(cp) // 2], "p90", cp[int(.9 * (len(cp) - 1))])
print("turns by trigger", turns_trig.most_common(12))
print("turns by step", turns_step.most_common(), {k: round(v, 1) for k, v in step_cost.most_common()})
tot = sum(carried.values())
print("tool results: name n tokensK carriedM carried_usd")
for nm, c in carried.most_common(12):
    print("  %-16s %5d %7.0f %7.1f %7.2f" % (nm, ntool[nm], by_tool[nm] / 1e3, c / 1e6, c * pr / 1e6))
print("carried total M %.1f usd %.2f" % (tot / 1e6, tot * pr / 1e6))
print("\n== part 2: cache writes by gap")
pat = sys.argv[1] if len(sys.argv) > 1 else MAIN + "-t*"
cls = collections.Counter(); usd = collections.Counter(); gaps = collections.Counter(); models = collections.Counter()
def ts(s): return datetime.datetime.fromisoformat(s.replace("Z", "+00:00")).timestamp()
for path in glob.glob(os.path.join(PJ, pat, "*.jsonl")):
    seen, prev_t = set(), None
    for l in open(path, errors="replace"):
        try: e = json.loads(l)
        except ValueError: continue
        if e.get("type") != "assistant" or e.get("isSidechain"): continue
        m = e.get("message") or {}; u = m.get("usage")
        if not u or m.get("id") in seen or (e.get("timestamp") or "") < "2026-10-28": continue
        seen.add(m["id"]); t = ts(e["timestamp"])
        w, r = u.get("cache_creation_input_tokens", 0), u.get("cache_read_input_tokens", 0)
        ctx = w + r + u.get("input_tokens", 0)
        wp = 4 if "sonnet" in (m.get("model") or "") else 8
        gap = t - prev_t if prev_t else None
        if prev_t is None: c = "start"
        elif w > 0.5 * ctx and gap > 3600: c = "rewrite gap>1h"
        elif w > 0.5 * ctx and gap > 300: c = "rewrite gap 5-60m"
        elif w > 0.5 * ctx: c = "rewrite gap<5m"
        else: c = "incremental"
        if c.startswith("rewrite"):
            gaps["%dk ctx" % (min(ctx // 50000, 4) * 50)] += 1
        cls[c] += 1; usd[c] += w * wp / 1e6; models[m.get("model")] += 1
        prev_t = t
tot = sum(usd.values())
for c in sorted(cls): print("%-20s calls %5d write usd %7.2f (%2.0f%%)" % (c, cls[c], usd[c], 100 * usd[c] / tot))
print("total write usd %.2f" % tot, dict(gaps), dict(models))

print("\n== part 3: context at the start of a step turn (previous turn's end)")
st = collections.defaultdict(list); nc = collections.defaultdict(list)
for d in glob.glob(ST + "/[0-9]*"):
    prev = 0
    for e in events(d):
        u = e.get("usage")
        if e.get("kind") == "result" and u and e["unix_ms"] >= SINCE:
            k = e.get("step") or "?"; st[k].append(prev); nc[k].append(u.get("calls", 0)); prev = u.get("context_tokens", 0)
med = lambda x: sorted(x)[len(x) // 2] if x else 0
for k in st: print("%-8s turns %4d start ctx p50 %4dk calls p50 %d" % (k, len(st[k]), med(st[k]) / 1e3, med(nc[k])))

print("\n== part 4: gaps between calls, results over 8k tokens by tool, cost by trigger")
gb = collections.Counter(); big = collections.Counter(); tc = collections.Counter()
for path in glob.glob(os.path.join(PJ, pat, "*.jsonl")):
    seen, pt, names = set(), None, {}
    for l in open(path, errors="replace"):
        try: e = json.loads(l)
        except ValueError: continue
        if (e.get("timestamp") or "") < "2026-10-28": continue
        m = e.get("message") or {}
        if e.get("type") == "assistant":
            for b in m.get("content") or []:
                if isinstance(b, dict) and b.get("type") == "tool_use": names[b["id"]] = b["name"]
            if not m.get("usage") or m.get("id") in seen or e.get("isSidechain"): continue
            seen.add(m["id"]); t = ts(e["timestamp"])
            if pt: gb["<5m" if t - pt < 300 else "5-60m" if t - pt < 3600 else ">1h"] += 1
            pt = t
        elif e.get("type") == "user" and isinstance(m.get("content"), list):
            for b in m["content"]:
                if isinstance(b, dict) and b.get("type") == "tool_result" and len(json.dumps(b.get("content"))) > 32000:
                    big[names.get(b.get("tool_use_id"), "?")] += 1
for d in glob.glob(ST + "/[0-9]*"):
    for e in events(d):
        if e.get("kind") == "result" and e.get("usage") and e["unix_ms"] >= SINCE:
            tc[e.get("trigger") or "?"] += e["usage"].get("cost_micro_usd", 0) / 1e6
print("gaps", dict(gb)); print("results over 8k tokens", dict(big)); print("usd by trigger", {k: round(v, 1) for k, v in tc.most_common()})

print("\n== part 5: what one cold rotation costs and saves (worker logs `rotated`, both transcripts)")
def turns_of(path):
    """Main-conversation calls with time and context; a user prompt (not a tool result) starts a turn."""
    calls, seen, turn = [], set(), 0
    for l in open(path, errors="replace"):
        try: e = json.loads(l)
        except ValueError: continue
        m = e.get("message") or {}
        if e.get("isSidechain"): continue
        if e.get("type") == "user":
            c = m.get("content")
            if isinstance(c, str) or (isinstance(c, list) and not any(isinstance(b, dict) and b.get("type") == "tool_result" for b in c)):
                turn += 1
        elif e.get("type") == "assistant" and m.get("usage") and m.get("id") not in seen:
            seen.add(m["id"]); u = m["usage"]
            c = {"t": ts(e["timestamp"]) * 1000, "turn": turn, "model": m.get("model"), "in": u.get("input_tokens", 0),
                 "w": u.get("cache_creation_input_tokens", 0), "r": u.get("cache_read_input_tokens", 0), "o": u.get("output_tokens", 0)}
            c["ctx"] = c["in"] + c["w"] + c["r"]; calls.append(c)
    return calls
rows = []
for folder in sorted(glob.glob(ST + "/[0-9]*")):
    try: t = json.load(open(folder + "/task.json"))
    except Exception: continue
    if t.get("created_unix_ms", 0) < SINCE: continue
    rots = [e for e in events(folder) if e.get("kind") == "rotated" and e.get("tokens")]
    if not rots: continue
    pdir = os.path.join(PJ, t.get("working_copy", "").replace("/", "-").replace(".", "-"))
    sessions = [s for s in (turns_of(p) for p in glob.glob(pdir + "/*.jsonl")) if s]
    for rot in rots:
        R = rot["unix_ms"]
        old = max((s for s in sessions if s[0]["t"] < R - 5000), key=lambda s: s[-1]["t"] if s[-1]["t"] <= R + 5000 else 0, default=None)
        new = min((s for s in sessions if s[0]["t"] >= R - 5000), key=lambda s: s[0]["t"], default=None)
        if not old or not new: continue
        before = [c for c in old if c["t"] <= R + 5000]
        handoff = [c for c in before if c["turn"] == before[-1]["turn"]]
        first = [c for c in new if c["turn"] == new[0]["turn"]]
        pw = price(new[0]["model"])[3]; pr = price(new[0]["model"])[2]
        C0 = rot["tokens"]; start = first[-1]["ctx"]
        later = new[len(first):]
        rows.append({"task": t["id"], "C0": C0, "handoff": sum(map(cost, handoff)), "handoff_calls": len(handoff),
                     "prefix_w": new[0]["w"] * pw / 1e6, "first": sum(map(cost, first)), "first_calls": len(first),
                     "start": start, "later": len(later), "reason": rot.get("reason") or "size",
                     # ^ every later call reads C0 - start more without the rotation (growth assumed equal)
                     "saved": len(later) * max(C0 - start, 0) * pr / 1e6,
                     # ^ compaction instead: one call reads C0 and writes a summary of about 10k (Claude Code keeps the prefix cached)
                     "compact": (C0 * pr + 10_000 * price(new[0]["model"])[1] + 10_000 * pw) / 1e6})
for r in rows:
    print("t%-4s %-8s C0 %4dk handoff $%.2f (%d calls) new prefix $%.2f first turn $%.2f (%d calls, ends %dk) later calls %3d saved $%.2f compact-est $%.2f" % (
        r["task"], r["reason"][:8], r["C0"] / 1e3, r["handoff"], r["handoff_calls"], r["prefix_w"], r["first"], r["first_calls"], r["start"] / 1e3, r["later"], r["saved"], r["compact"]))
if rows:
    n = len(rows); s = lambda k: sum(r[k] for r in rows)
    print("rotations %d: handoff $%.2f, new prefix write $%.2f, first new turn $%.2f, reads saved after $%.2f, compaction estimate $%.2f" % (
        n, s("handoff"), s("prefix_w"), s("first"), s("saved"), s("compact")))
    print("per rotation: direct cost low (handoff + prefix) $%.2f, high (handoff + whole first turn) $%.2f, saved $%.2f; later calls p50 %d" % (
        (s("handoff") + s("prefix_w")) / n, (s("handoff") + s("first")) / n, s("saved") / n, sorted(r["later"] for r in rows)[n // 2]))
    for why in ("size", "idle"):
        sel = [r for r in rows if r["reason"].startswith(why)]
        if sel:
            print("  %s: %d rotations, handoff $%.2f each, saved $%.2f each" % (
                why, len(sel), sum(r["handoff"] for r in sel) / len(sel), sum(r["saved"] for r in sel) / len(sel)))
# ^ Context growth per call decides the cheapest threshold: rotating every k calls costs F/k + price * g * k / 2 a call.
def growth(paths):
    g = []
    for p in paths:
        calls = turns_of(p)
        if len(calls) >= 20 and calls[0]["t"] >= SINCE:
            g.append((max(c["ctx"] for c in calls) - calls[0]["ctx"]) / len(calls))
    return sorted(g)
wg = growth(glob.glob(os.path.join(PJ, MAIN + "-t*", "*.jsonl")))
mg = growth([p for p in glob.glob(os.path.join(PJ, MAIN, "*.jsonl"))
             if "manager" in open(p, errors="replace").read(20000) and "wsx task" in open(p, errors="replace").read(20000)])
for name, g, start in (("worker", wg, 37_000), ("manager", mg, 40_000)):
    if not g: continue
    gp = g[len(g) // 2]
    print("%s growth per call p50 %.0f tokens (n=%d)" % (name, gp, len(g)))
    for F in (0.4, 1.0):
        for T in (150_000, 200_000, 300_000, 400_000):
            k = (T - start) / gp
            print("  F $%.1f rotate at %dk: every %d calls, $%.4f a call" % (F, T / 1e3, k, F / k + 0.20e-6 * gp * k / 2))
