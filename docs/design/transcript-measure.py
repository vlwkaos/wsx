#!/usr/bin/env python3
"""Count the rows the owner sees in each chat, before and after the transcript rule.

Read-only, stdlib. Prints totals only (docs/design/transcript.md section 2).
Usage: python3 -I docs/design/transcript-measure.py [state-dir] [days] [chat ...]   (chats named last print their kinds)

"Before" mirrors `chat_rows` of wsx-core `task/chat.rs` as it was at c14c9e34: every
non-quiet reply, every peer message and every sent message is a bubble. "After" mirrors
`task::chat::bubble` (the rule of this task): a reply is a bubble only when it answers an owner
message or a recap, or is a post; a reply to events or to a peer is a quiet line; peer messages
and sent messages fold into one "Talked to <chat>" line per run with the same peer.
An open ask or an approval keeps its reply a bubble; this script cannot see them, so a reply
that asks is counted in the quiet column ("after" is an upper bound of the muted count).
"""
import collections
import json
import os
import sys

state = os.path.expanduser(sys.argv[1] if len(sys.argv) > 1 else "~/.local/state/wsx/tasks")
days = float(sys.argv[2]) if len(sys.argv) > 2 else 7
chats = os.path.join(state, "chats")


def nothing(text):
    return text.strip() in ("", "-")


def count(path):
    rows = [json.loads(line) for line in open(path) if line.strip()]
    cut = max(row["unix_ms"] for row in rows) - days * 86_400_000
    source = {}
    now, after = collections.Counter(), collections.Counter()
    last_peer = None
    for row in rows:
        kind = row["kind"]
        if kind == "message":
            source[row["id"]] = row["from"] if isinstance(row["from"], str) else "peer"
        if row["unix_ms"] < cut:
            continue
        if kind == "message":
            who = row["from"]
            if who == "owner":
                now["owner"] += 1
                after["owner"] += 1
            elif who == "events":
                lines = len(row.get("short") or []) or len([x for x in row["text"].splitlines() if x.strip()])
                now["event lines"] += lines
                after["event lines"] += lines
            elif who != "recap" and not nothing(row["text"]):
                now["peer message"] += 1
                if last_peer != who.split(":", 1)[-1]:
                    after["talked-to line"] += 1
                last_peer = who.split(":", 1)[-1]
        elif kind == "sent" and not nothing(row.get("text", "")):
            now["sent"] += 1
            if last_peer != row.get("to"):
                after["talked-to line"] += 1
            last_peer = row.get("to")
        elif kind == "reply":
            if row.get("quiet") or row.get("post"):
                key = "post" if row.get("post") else "quiet reply"
                if not nothing(row["text"]):
                    now[key] += 1
                    after[key] += 1
                continue
            if nothing(row["text"]):
                now["empty reply"] += 1
                continue
            targets = {source.get(i, "?") for i in row["to"]}
            answers = not targets or not all(t not in ("owner", "events", "recap", "?") for t in targets)
            now["reply"] += 1
            after["reply" if answers else "quiet reply"] += 1
        if kind in ("reply", "message"):
            if kind == "reply" or row["from"] in ("owner", "events"):
                last_peer = None
    return now, after


bubbles = {"owner", "reply", "post", "peer message", "sent"}  # an empty "-" reply never was one: ManagerEntry::read marks it quiet
totals = [collections.Counter(), collections.Counter()]
for chat in sorted(os.listdir(chats)):
    path = os.path.join(chats, chat, "log.jsonl")
    if not os.path.exists(path):
        continue
    now, after = count(path)
    totals[0].update(now)
    totals[1].update(after)
    before_bubbles = sum(v for k, v in now.items() if k in bubbles)
    after_bubbles = sum(v for k, v in after.items() if k in ("owner", "reply", "post"))
    if before_bubbles:
        print(f"{chat:12} bubbles {before_bubbles:5} -> {after_bubbles:5}")
    if chat in sys.argv[3:]:
        print(f"  {chat} before {dict(now)}\n  {chat} after  {dict(after)}")
print("before", dict(totals[0]))
print("after ", dict(totals[1]))
print(
    "bubbles",
    sum(v for k, v in totals[0].items() if k in bubbles),
    "->",
    sum(v for k, v in totals[1].items() if k in ("owner", "reply", "post")),
)
