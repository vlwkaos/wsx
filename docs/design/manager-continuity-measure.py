#!/usr/bin/python3
"""Measures behind docs/design/manager-continuity.md. Read-only.
  /usr/bin/python3 docs/design/manager-continuity-measure.py <manager folder> cost|msgs
"""
import json, os, statistics as st, sys

folder, mode = sys.argv[1], sys.argv[2]
ev = []
for n in ("log.1.jsonl", "log.jsonl"):
    p = os.path.join(folder, n)
    if os.path.exists(p):
        ev += [json.loads(l) for l in open(p, encoding="utf-8", errors="replace") if l.strip().startswith("{")]
ev.sort(key=lambda e: e["unix_ms"])
med = lambda x: round(st.median(x), 3) if x else None

# turns: (kind of turn, reply) ; kind = rot-instr / rot-size / resumed
turns, pending, prev_ctx, run = [], None, 0, 0
for e in ev:
    if e["kind"] == "rotated":
        pending = "instr" if "instructions changed" in e.get("reason", "") else ("switch" if e.get("reason") else "size")
        run = 0
        rot_tokens = e["tokens"]
        rot_prev_ctx = prev_ctx
    elif e["kind"] == "reply" and "usage" in e:
        u = e["usage"]
        ctx = u.get("context_tokens")
        if pending:
            turns.append((pending, 0, e, rot_tokens or rot_prev_ctx))
            pending = None
        else:
            run += 1
            turns.append(("resumed", run, e, None))
        if ctx: prev_ctx = ctx

def row(name, ts):
    us = [t[2]["usage"] for t in ts]
    f = lambda k: med([u.get(k, 0) for u in us])
    print(f"{name:14} n={len(ts):4} cost$={med([u['cost_micro_usd']/1e6 for u in us])} "
          f"write={f('cache_creation_tokens')} read={f('cache_read_tokens')} "
          f"ctx={f('context_tokens')} out={f('output_tokens')}")

if mode == "cost":
    for k in ("instr", "size", "switch", "resumed"):
        row(k, [t for t in turns if t[0] == k])
    row("resumed#1-3", [t for t in turns if t[0] == "resumed" and t[1] <= 3])
    # resumed turns by context band: the price of a resume at that context
    for lo, hi in ((0, 50e3), (50e3, 100e3), (100e3, 150e3), (150e3, 200e3), (200e3, 1e9)):
        row(f"res {int(lo/1e3)}-{int(hi/1e3) if hi<1e9 else 'inf'}k", [t for t in turns if t[0] == "resumed" and lo <= (t[2]["usage"].get("context_tokens") or 0) < hi])
    print("rotated tokens (size) med:", med([t[3] for t in turns if t[0] == "size"]))
    print("sum cost$ first turns instr/size:", [round(sum(t[2]['usage']['cost_micro_usd'] for t in turns if t[0]==k)/1e6,2) for k in ('instr','size')])
    # prices: least squares of cost on (input, output, read, write) tokens over resumed turns
    import numpy as np
    r = [t[2]["usage"] for t in turns if t[0] == "resumed"]
    A = np.array([[u["output_tokens"], u["cache_read_tokens"], u["cache_creation_tokens"]] for u in r], float)
    b = np.array([u["cost_micro_usd"] for u in r], float)
    print("USD per Mtok out/read/write:", np.round(np.linalg.lstsq(A, b, rcond=None)[0], 2))
    # idle gap before each turn (reply time - duration - previous reply time); >60 min = cold cache
    last, gaps = None, {}
    for e in ev:
        if e["kind"] == "reply" and "usage" in e:
            if last: gaps[id(e)] = (e["unix_ms"] - e["usage"]["duration_ms"] - last) / 6e4
            last = e["unix_ms"]
    for k in ("instr", "size", "switch", "resumed"):
        g = [(gaps[id(t[2])], t[2]["usage"]) for t in turns if t[0] == k and id(t[2]) in gaps]
        cold = [u for x, u in g if x > 60]
        print(f"{k:8} cold(>60min idle) {len(cold)} of {len(g)}; cold med cost$ {med([u['cost_micro_usd']/1e6 for u in cold])} warm {med([u['cost_micro_usd']/1e6 for x,u in g if x<=60])}")
    # what a resume would have cost: median of resumed turns with the context within 25% of the one
    # the old conversation had (rotated.tokens, else the last reply's context_tokens), same warm/cold
    for k in ("instr", "size"):
        est = []; act = []
        for t in turns:
            if t[0] != k or id(t[2]) not in gaps: continue
            c = t[3]
            cold = gaps[id(t[2])] > 60
            peers = [x[2]["usage"]["cost_micro_usd"] / 1e6 for x in turns if x[0] == "resumed" and id(x[2]) in gaps
                     and (gaps[id(x[2])] > 60) == cold and x[2]["usage"].get("context_tokens") and abs(x[2]["usage"]["context_tokens"] - c) <= 0.25 * c]
            if len(peers) >= 3:
                est.append(st.median(peers)); act.append(t[2]["usage"]["cost_micro_usd"] / 1e6)
        print(f"{k}: {len(est)} rotations with peers; actual first turns ${sum(act):.2f} vs resume ${sum(est):.2f}; ctx med {med([t[3] for t in turns if t[0]==k])}")

if mode == "msgs":
    import re
    owner = re.compile(r"\bi said\b|\bi told\b|\bi asked\b|\bi already\b|\bwe (already )?(decided|agreed|discussed)\b|what happened to|\bremember\b|\bagain\b|\balready\b|as before|didn.t i\b|i just (said|told)|아까|말했|이미|다시", re.I)
    mgr = re.compile(r"no (record|context|memory)|not in my (notes|context)|don.t (have|see|remember)|which (one|task|message)|what do you mean|remind me|could you (repeat|clarify)|new conversation|fresh conversation|lost|don.t know (what|which)", re.I)
    nrot = 0
    for i, e in enumerate(ev):
        if e["kind"] != "rotated": continue
        nrot += 1
        lab = ("instr" if "instructions changed" in e.get("reason", "") else "switch" if e.get("reason") else "size")
        n = 0; seen_own = 0
        for x in ev[i + 1:]:
            if x["kind"] == "rotated": break
            if x["kind"] == "message" and x["from"] == "owner":
                n += 1
                if n <= 3 and owner.search(x["text"]):
                    print(f"OWNER #{x['id']} ({lab}, after-rot owner msg {n}): {x['text'][:260]!r}")
            if x["kind"] == "reply" and seen_own < 2:
                seen_own += 1
                if mgr.search(x["text"][:1500]):
                    print(f"MGR   reply#{x['id']} ({lab}, reply {seen_own}): {x['text'][:260]!r}")
            if n > 3 and seen_own >= 2: break

if mode == "more":
    # cost per model call by context band (turns that log `calls`), warm only
    last, gp = None, {}
    for e in ev:
        if e["kind"] == "reply" and "usage" in e:
            if last: gp[id(e)] = (e["unix_ms"] - e["usage"]["duration_ms"] - last) / 6e4
            last = e["unix_ms"]
    for lo, hi in ((0, 60e3), (60e3, 100e3), (100e3, 150e3), (150e3, 1e9)):
        us = [t[2]["usage"] for t in turns if t[0] == "resumed" and t[2]["usage"].get("calls") and lo <= t[2]["usage"].get("context_tokens", 0) < hi and gp.get(id(t[2]), 0) <= 60]
        print(f"warm resumed ctx {int(lo/1e3)}-{int(hi/1e3) if hi<1e9 else 'inf'}k: n={len(us)} cost/call$ {med([u['cost_micro_usd']/1e6/u['calls'] for u in us])} cost/turn$ {med([u['cost_micro_usd']/1e6 for u in us])}")
    for lo, hi in ((0, 60e3), (60e3, 100e3), (100e3, 1e9)):
        us = [t[2]["usage"] for t in turns if t[0] == "resumed" and lo <= t[2]["usage"].get("context_tokens", 0) < hi and gp.get(id(t[2]), 0) > 60]
        print(f"COLD resumed ctx {int(lo/1e3)}-{int(hi/1e3) if hi<1e9 else 'inf'}k: n={len(us)} cost/turn$ {med([u['cost_micro_usd']/1e6 for u in us])} write {med([u['cache_creation_tokens'] for u in us])}")
    # owner messages within the first 2 turns after a rotation
    w = {"instr": 0, "size": 0}; tot = {"instr": 0, "size": 0}
    for i, e in enumerate(ev):
        if e["kind"] != "rotated" or e.get("reason", "") and "instructions" not in e["reason"]: continue
        k = "instr" if e.get("reason") else "size"; tot[k] += 1; r = 0
        for x in ev[i + 1:]:
            if x["kind"] == "rotated": break
            if x["kind"] == "reply" and "usage" in x: r += 1
            if x["kind"] == "message" and x["from"] == "owner": w[k] += 1; break
            if r >= 2: break
    print("rotations with an owner message before the 2nd reply:", w, "of", tot)
