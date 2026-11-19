#!/usr/bin/env python3
"""Read-only count for docs/design/flat-chats.md (task 539): what thread data this machine holds, per project.
Totals only, no message text.   python3 -I docs/design/flat-chats-count.py"""
import collections, glob, json, os

TASKS = os.path.expanduser("~/.local/state/wsx/tasks")
THREAD_EVENTS = ("thread_open", "thread_closed", "thread_merged", "thread_moved")
chats = collections.defaultdict(collections.Counter)
for chat_dir in sorted(glob.glob(TASKS + "/chats/*/")):
    name = os.path.basename(chat_dir[:-1])
    for path in glob.glob(chat_dir + "log*.jsonl"):
        for line in open(path, errors="replace"):
            try:
                row = json.loads(line)
            except ValueError:
                continue
            kind = row.get("kind")
            chats[name]["rows"] += kind in ("message", "reply")
            chats[name]["tagged messages"] += kind == "message" and bool(row.get("thread"))
            chats[name]["tagged replies"] += kind == "reply" and bool(row.get("threads"))
            chats[name]["thread events"] += kind in THREAD_EVENTS
    try:
        meta = json.load(open(chat_dir + "chat.json"))
    except (OSError, ValueError):
        meta = {}
    chats[name]["made from another chat"] += "from" in meta
    chats[name]["threads-placed marks"] += os.path.exists(chat_dir + "threads-placed")
print("chats (rows = messages + replies):")
for name, c in chats.items():
    if c["tagged messages"] or c["tagged replies"] or c["thread events"] or c["made from another chat"]:
        print(f"  {name}: " + ", ".join(f"{k} {v}" for k, v in c.items() if v))
print("  threads-placed marks:", sum(c["threads-placed marks"] for c in chats.values()), "of", len(chats), "chats; rows in all chats:", sum(c["rows"] for c in chats.values()))
print("projects (tasks / with a thread; lines with a thread in memory/records.jsonl and search.jsonl):")
for project in sorted(glob.glob(TASKS + "/*-*/")):
    tasks = threaded = 0
    for path in glob.glob(project + "[0-9]*/task.json"):
        tasks += 1
        try:
            threaded += bool(json.load(open(path)).get("thread"))
        except ValueError:
            pass
    def lines(name):
        try:
            return sum('"thread"' in line for line in open(project + name, errors="replace"))
        except OSError:
            return 0
    if threaded or lines("memory/records.jsonl") or lines("search.jsonl"):
        print(f"  {os.path.basename(project[:-1])}: tasks {tasks}, with thread {threaded}, memory/records.jsonl {lines('memory/records.jsonl')}, search.jsonl {lines('search.jsonl')}")
