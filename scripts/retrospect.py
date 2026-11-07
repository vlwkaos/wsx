#!/usr/bin/python3
"""Retrospect: one project's spend and a look back over its manager chat, posted to the chat.

A routine runs it (docs/design/dynamic-routines-retrospect.md); it prints nothing on success, so
the run costs no manager turn, and posts its lines with `wsx task post`:

  /usr/bin/python3 scripts/retrospect.py ~/.local/state/wsx/tasks/<project-id> -p <project> [--no-model] [--wsx PATH]

Code counts: spend, owner messages that ask the same again, phrases the manager repeats, and
each reply the owner answered within 30 minutes. One model call (sonnet, high effort, the
owner's rule) judges only those pairs: which replies the owner turned down, of which kind (A-F of
docs/design/manager-reliability.md), and which text or product changes the repeats point to
(scripts/retrospect-prompt.md). The whole report goes to
<state dir>/retrospect/<unix ms>.md; the newest 14 stay.
"""
import argparse
import importlib.util
import json
import os
import re
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
_spec = importlib.util.spec_from_file_location("usage_report", os.path.join(HERE, "usage-report.py"))
usage = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(usage)

TITLE = "retrospect"
DAY_MS = 24 * 3600 * 1000
HISTORY_MS = 7 * DAY_MS          # repeats count over a week; one of them must be new
ASK_WORDS, ASK_SIMILAR, ASK_TIMES = 3, 0.4, 3
PHRASE_WORDS, PHRASE_REPLIES = 8, 5
REACTION_MS = 30 * 60 * 1000     # the owner's next message within this is the reaction
PAIRS, PAIR_CHARS, SHOWN, REPORTS = 40, 600, 3, 14
MODEL, EFFORT = "sonnet", "high"
KINDS = ["A", "B", "C", "D", "E", "F", "other", ""]
SCHEMA = {
    "type": "object", "required": ["pairs", "changes"],
    "properties": {
        "pairs": {"type": "array", "items": {
            "type": "object", "required": ["reply", "rejected", "wanted"],
            "properties": {"reply": {"type": "integer"}, "rejected": {"type": "boolean"},
                           "wanted": {"type": "string"}, "kind": {"enum": KINDS}}}},
        "changes": {"type": "array", "maxItems": SHOWN, "items": {
            "type": "object", "required": ["change", "evidence"],
            "properties": {"change": {"type": "string"},
                           "evidence": {"type": "array", "items": {"type": "integer"}}}}},
    },
}


def words(text):
    """Lowercase words without code blocks, paths and links: an attachment's path is no ask."""
    out, fenced = [], False
    for line in text.splitlines():
        if line.lstrip().startswith("```"):
            fenced = not fenced
            continue
        if fenced:
            continue
        for token in line.split():
            if "/" in token or token.startswith("~"):
                continue
            word = "".join(c for c in token if c.isalnum()).lower()
            if word:
                out.append(word)
    return out


def grams(ws, n):
    return {" ".join(ws[i:i + n]) for i in range(len(ws) - n + 1)}


def one_line(text, chars):
    text = " ".join(text.split())
    return text if len(text) <= chars else text[:chars - 1] + "…"


def ids(prefix, values):
    shown = ", ".join(f"{prefix}{v}" for v in values[:5])
    return shown + (f" and {len(values) - 5} more" if len(values) > 5 else "")


def chat(events):
    """Owner messages and the manager's own replies (a reply with usage: wsx posts have none)."""
    asks, replies = [], []
    for e in events:
        if e.get("kind") == "message" and e.get("from") == "owner":
            asks.append(e)
        elif e.get("kind") == "reply" and e.get("usage") and not e.get("quiet") \
                and (e.get("text") or "").strip() not in ("", "-"):
            replies.append(e)
    return asks, replies


def owner_repeats(asks, after):
    """Groups of ASK_TIMES or more owner messages sharing most word 3-grams, one after `after`."""
    sets = [grams(words(e["text"]), ASK_WORDS) for e in asks]
    group = list(range(len(asks)))

    def root(i):
        while group[i] != i:
            group[i] = group[group[i]]
            i = group[i]
        return i
    for a in range(len(sets)):
        for b in range(a + 1, len(sets)):
            shared = len(sets[a] & sets[b])
            if shared and shared / len(sets[a] | sets[b]) >= ASK_SIMILAR:
                group[root(a)] = root(b)
    groups = {}
    for i in range(len(asks)):
        groups.setdefault(root(i), []).append(asks[i])
    out = [{"text": one_line(min((e["text"] for e in g), key=len), 100), "ids": [e["id"] for e in g]}
           for g in groups.values() if len(g) >= ASK_TIMES and any(e["unix_ms"] > after for e in g)]
    return sorted(out, key=lambda r: -len(r["ids"]))


def manager_repeats(replies, after):
    """Word 8-grams in PHRASE_REPLIES or more replies, one after `after`. The 8-grams of one
    set of replies are one phrase: their longest run in the first reply."""
    texts = [words(e["text"]) for e in replies]
    seen = {}
    for i, ws in enumerate(texts):
        for g in grams(ws, PHRASE_WORDS):
            seen.setdefault(g, []).append(i)
    by_set = {}
    for g, members in seen.items():
        if len(members) >= PHRASE_REPLIES and any(replies[i]["unix_ms"] > after for i in members):
            by_set.setdefault(tuple(members), set()).add(g)
    out = []
    for members, phrase in by_set.items():
        first = texts[members[0]]
        starts = [i for i in range(len(first) - PHRASE_WORDS + 1)
                  if " ".join(first[i:i + PHRASE_WORDS]) in phrase]
        begin = end = starts[0]
        while end + 1 in starts:
            end += 1
        out.append({"text": one_line(" ".join(first[begin:end + PHRASE_WORDS]), 160),
                    "ids": [replies[i]["id"] for i in members]})
    return sorted(out, key=lambda r: -len(r["ids"]))


def reactions(events, after):
    """Each manager reply the owner answered within REACTION_MS after `after`, newest PAIRS."""
    owner, pairs, open_ = {}, [], None
    for e in events:
        if e.get("kind") == "message" and e.get("from") == "owner":
            owner[e["id"]] = e["text"]
            if open_ and e["unix_ms"] > after and e["unix_ms"] <= open_["reply"]["unix_ms"] + REACTION_MS:
                pairs.append({**open_, "next": e})
            open_ = None
        elif e.get("kind") == "reply" and e.get("usage") and not e.get("quiet") \
                and (e.get("text") or "").strip() not in ("", "-"):
            ask = next((owner[i] for i in e.get("to") or [] if i in owner), None)
            open_ = {"ask": ask, "reply": e}
    return pairs[-PAIRS:]


def judge(folder, pairs, asked, repeated):
    """One model call over the pairs and the counts. Returns (rejected {id: (wanted, kind)},
    changes, cost in USD) or raises RuntimeError."""
    with open(os.path.join(HERE, "retrospect-prompt.md"), encoding="utf-8") as f:
        prompt = f.read()
    data = {
        "pairs": [{"owner_asked": (p["ask"] or "")[:PAIR_CHARS], "reply": p["reply"]["id"],
                   "manager_replied": p["reply"]["text"][:PAIR_CHARS],
                   "owner_next": p["next"]["text"][:PAIR_CHARS]} for p in pairs],
        "owner_asked_again": asked, "manager_repeated": repeated,
    }
    argv = ["claude", "-p", "--model", MODEL, "--effort", EFFORT, "--output-format", "json",
            "--no-session-persistence", "--tools", "", "--system-prompt", prompt,
            "--json-schema", json.dumps(SCHEMA)]
    try:
        # ^ The state folder as cwd: the project's own CLAUDE.md is no part of the judgment.
        run = subprocess.run(argv, input=json.dumps(data), capture_output=True, text=True,
                             cwd=folder, timeout=600)
        answer = json.loads(run.stdout)
    except (OSError, subprocess.TimeoutExpired, ValueError) as err:
        raise RuntimeError(f"the model call failed: {err}") from err
    if answer.get("is_error") or not isinstance(answer.get("structured_output"), dict):
        raise RuntimeError(f"the model call failed: {one_line(str(answer.get('result')), 200)}")
    out = answer["structured_output"]
    rejected = {p["reply"]: (one_line(p.get("wanted", ""), 100), p.get("kind") or "other")
                for p in out.get("pairs", []) if p.get("rejected")}
    changes = [(one_line(c["change"], 200), c.get("evidence", [])) for c in out.get("changes", [])][:SHOWN]
    return rejected, changes, answer.get("total_cost_usd", 0.0)


def spend(state_dir, after):
    """Worker and manager cost since `after`, and the three costliest tasks."""
    per_task, manager = {}, 0
    for name in os.listdir(state_dir):
        folder = os.path.join(state_dir, name)
        if not name.isdigit() or not os.path.isfile(os.path.join(folder, "task.json")):
            continue
        if os.path.getmtime(folder) * 1000 < after and not any(
                os.path.getmtime(os.path.join(folder, f)) * 1000 >= after
                for f in os.listdir(folder) if f.startswith("log")):
            continue
        results = [e for e in usage.read_log(folder) if e.get("kind") == "result"]
        usage.per_turn_cost(results)
        cost = sum(e["usage"].get("cost_micro_usd", 0) for e in results
                   if e.get("usage") and e["unix_ms"] > after)
        if cost:
            per_task[int(name)] = cost
    replies = [e for e in usage.read_log(os.path.join(state_dir, "manager")) if e.get("kind") == "reply"]
    usage.per_turn_cost(replies)
    manager = sum(e["usage"].get("cost_micro_usd", 0) for e in replies if e.get("usage") and e["unix_ms"] > after)
    top = sorted(per_task.items(), key=lambda kv: -kv[1])[:SHOWN]
    return sum(per_task.values()), manager, top


def summary(state_dir, after, asked, repeated, pairs, judged, replies):
    workers, manager, top = spend(state_dir, after)
    since = time.strftime("%m-%d %H:%M", time.localtime(after / 1000))
    line = f"Since {since}: spend ${usage.usd(workers + manager)} (manager ${usage.usd(manager)})"
    if top:
        line += "; costliest " + ", ".join(f"#{n} ${usage.usd(c)}" for n, c in top)
    lines = [line]
    # ^ A reply that opens with `> ` quotes an earlier message (docs/tasks.md "Manager"): is it used?
    recent = [e for e in replies if e["unix_ms"] > after]
    if recent:
        lines.append(f"Quotes: {sum(1 for e in recent if (e['text'] or '').startswith('> '))} of {len(recent)} replies")
    lines += [f"You asked {len(r['ids'])} times: \"{r['text']}\" ({ids('m', r['ids'])})" for r in asked[:SHOWN]]
    lines += [f"The manager repeated in {len(r['ids'])} replies: \"{r['text']}\" ({ids('r', r['ids'])})"
              for r in repeated[:SHOWN]]
    if isinstance(judged, tuple):
        rejected, changes, cost = judged
        if rejected:
            kinds = [k for _, k in rejected.values()]
            counted = ", ".join(f"{k} {kinds.count(k)}" for k in KINDS if k in kinds)
            shown = "; ".join(f"r{i} ({k}, wanted {w})" for i, (w, k) in list(rejected.items())[:SHOWN])
            lines.append(f"You turned down {len(rejected)} of {len(pairs)} replies you answered ({counted}): {shown}")
        lines += [f"Change {n}: {c} ({ids('', ev)})" for n, (c, ev) in enumerate(changes, 1)]
        lines.append(f"Judged by {MODEL} ({EFFORT}): ${cost:.2f}")
    elif pairs:
        lines.append(f"{len(pairs)} replies you answered within 30 min: {judged}")
    return lines


def write_report(folder, now, lines, asked, repeated, pairs, judged):
    os.makedirs(folder, exist_ok=True)
    rejected = judged[0] if isinstance(judged, tuple) else {}
    text = ["# Retrospect", "", *lines, "", "## Asked again", ""]
    text += [f"- {len(r['ids'])} times: \"{r['text']}\" ({ids('m', r['ids'])})" for r in asked]
    text += ["", "## Repeated by the manager", ""]
    text += [f"- {len(r['ids'])} replies: \"{r['text']}\" ({ids('r', r['ids'])})" for r in repeated]
    text += ["", "## Replies and the owner's next message", ""]
    for p in pairs:
        wanted, kind = rejected.get(p["reply"]["id"], (None, None))
        verdict = f"turned down ({kind}), wanted {wanted}" if kind else "kept"
        text.append(f"- r{p['reply']['id']} ({verdict}): {one_line(p['reply']['text'], 300)}")
        text.append(f"  - m{p['next']['id']}: {one_line(p['next']['text'], 300)}")
    path = os.path.join(folder, f"{now}.md")
    with open(path, "w", encoding="utf-8") as f:
        f.write("\n".join(text) + "\n")
    reports = sorted((n for n in os.listdir(folder) if re.fullmatch(r"\d+\.md", n)), key=lambda n: int(n[:-3]))
    for old in reports[:-REPORTS]:
        os.remove(os.path.join(folder, old))
    return path


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("state_dir")
    ap.add_argument("-p", "--project", required=True)
    ap.add_argument("--no-model", action="store_true", help="count only; no model judges the replies")
    ap.add_argument("--wsx", default="wsx", help="the wsx binary that posts")
    a = ap.parse_args()
    if not os.path.isdir(os.path.join(a.state_dir, "manager")):
        sys.exit(f"retrospect: no manager folder in {a.state_dir}")
    events = usage.read_log(os.path.join(a.state_dir, "manager"))
    now = int(time.time() * 1000)
    # ^ It starts at the last retrospect, else 24 h back, as the digest does.
    after = max((e["unix_ms"] for e in events if e.get("kind") == "message"
                 and e.get("from") == "events" and e.get("text") == TITLE), default=now - DAY_MS)
    history = [e for e in events if e.get("unix_ms", 0) + HISTORY_MS > now]
    asks, replies = chat(history)
    if not any(e["unix_ms"] > after for e in asks + replies):
        return
    asked, repeated = owner_repeats(asks, after), manager_repeats(replies, after)
    pairs = reactions(history, after)
    if a.no_model:
        judged = "not judged (--no-model)"
    elif not pairs:
        judged = "none to judge"
    else:
        try:
            judged = judge(a.state_dir, pairs, asked, repeated)
        except RuntimeError as err:
            judged = str(err)
    lines = summary(a.state_dir, after, asked, repeated, pairs, judged, replies)
    path = write_report(os.path.join(a.state_dir, "retrospect"), now, lines, asked, repeated, pairs, judged)
    lines.append(f"Detail: {path}")
    post = subprocess.run([a.wsx, "task", "post", "-p", a.project, "--title", TITLE],
                          input="\n".join(lines), text=True, capture_output=True)
    if post.returncode:
        sys.exit(f"retrospect: the post failed: {post.stderr.strip()}")


if __name__ == "__main__":
    main()
