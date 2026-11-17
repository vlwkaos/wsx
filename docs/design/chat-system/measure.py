#!/usr/bin/env python3
"""Read-only measure for docs/design/chat-system.md (task 508). Totals only, never message text, no model call.

  python3 -I docs/design/chat-system/measure.py tags [days]    owner messages a rule can tie to a subject
  python3 -I docs/design/chat-system/measure.py chats [days]   chats, repos per chat, talk between chats
  python3 -I docs/design/chat-system/measure.py roots [days]   life of root tasks of the biggest chat; open at once
  python3 -I docs/design/chat-system/measure.py asks [days]    replies with an ask, per chat and per day

Store: ~/.local/state/wsx/tasks (`chats/<id>/log.jsonl`, `chats/<id>/chat.json`, `<repo-id>/<n>/task.json`).
"""
import collections, glob, json, os, re, sys, time

TASKS = os.path.expanduser("~/.local/state/wsx/tasks")
DAYS = int(sys.argv[2]) if len(sys.argv) > 2 else 14
NOW = time.time()
SINCE = NOW - DAYS * 86400
BIG = ("wsx", "wsx-8cbe3f08")


def rows(path):
    out = []
    try:
        for line in open(path, errors="replace"):
            try:
                out.append(json.loads(line))
            except ValueError:
                pass
    except OSError:
        pass
    return out


def chats():
    for d in sorted(glob.glob(TASKS + "/chats/*/")):
        yield os.path.basename(d[:-1]), d


def owner_messages(log):
    return [r for r in log if r.get("kind") == "message" and r.get("from") == "owner" and r["unix_ms"] / 1000 >= SINCE]


def tags():
    total = quoted = named = either = 0
    per = {}
    lengths = []
    for name, d in chats():
        ow = owner_messages(rows(d + "log.jsonl"))
        q = [r for r in ow if r.get("quote") or r.get("check")]
        n = [r for r in ow if re.search(r"#\d+", r["text"])]
        e = [r for r in ow if r in q or r in n]
        per[name] = (len(ow), len(q), len(e))
        total += len(ow); quoted += len(q); named += len(n); either += len(e)
        lengths += [len(r["text"]) for r in ow]
    lengths.sort()
    print(f"owner messages, last {DAYS} d: {total}; with a quote or a check: {quoted} ({100 * quoted / total:.1f}%); "
          f"with a #n: {named}; either: {either} ({100 * either / total:.1f}%); median {lengths[len(lengths) // 2]} characters")
    for name in ("wsx", "studio-app", "ir", "pr-review", "shopfront"):
        o, q, e = per.get(name, (0, 0, 0))
        print(f"  {name:12} owner {o:4}  quote/check {q:3}  either {e:3} ({100 * e / max(o, 1):.0f}%)")


def chat_list():
    total = collections.Counter()
    print(f"chats: {len(list(chats()))}")
    for name, d in chats():
        log = rows(d + "log.jsonl")
        try:
            meta = json.load(open(d + "chat.json"))
        except (OSError, ValueError):
            meta = {}
        peer = [r for r in log if r.get("kind") == "message" and re.match(r"(chat|chat-reply|manager|manager-reply):", r.get("from", "")) and r["unix_ms"] / 1000 >= SINCE]
        sent = [r for r in log if r.get("kind") == "sent" and r["unix_ms"] / 1000 >= SINCE]
        replies = [r for r in peer if "reply" in r["from"]]
        total.update(owner=len(owner_messages(log)), peer=len(peer), sent=len(sent), replies=len(replies),
                     short=sum(len(r["text"]) < 200 for r in replies), several=len(meta.get("repos", [])) > 1,
                     cut_out=bool(meta.get("from")))
    print(f"  repos on one chat above 1: {total['several']}; made from another chat's message (`from`): {total['cut_out']}")
    print(f"  last {DAYS} d: owner messages {total['owner']}; messages from other chats {total['peer']} "
          f"(replies {total['replies']}, under 200 characters {total['short']}); `sent` rows {total['sent']}")


def roots():
    spans, life = [], []
    for p in glob.glob(f"{TASKS}/{BIG[1]}/*/task.json"):
        try:
            t = json.load(open(p))
        except (OSError, ValueError):
            continue
        if t.get("parent") or t.get("after") or (t.get("created_unix_ms") or 0) / 1000 < SINCE:
            continue
        start = t["created_unix_ms"] / 1000
        done = t.get("state") in ("landed", "closed")
        end = (t.get("state_unix_ms") or 0) / 1000 if done else NOW
        spans.append((start, end))
        if done:
            life.append((end - start) / 3600)
    life.sort()
    print(f"chat {BIG[0]}: root tasks made in {DAYS} d: {len(spans)}, finished {len(life)}; life median {life[len(life) // 2]:.1f} h, "
          f"p90 {life[int(len(life) * .9)]:.1f} h; over 24 h: {sum(x > 24 for x in life)}")
    # ^ A thread that stays open 48 h after its last task lands (24 h quiet, 24 h after the reminder):
    # root tasks a day times 2 days, as a first reading of how many threads the old rule would hold.
    print(f"  made a day: {len(spans) / DAYS:.1f}; open at once if each stayed 48 h after landing: about {len(spans) / DAYS * 2:.0f}")


def asks():
    total = collections.Counter()
    for name, d in chats():
        log = rows(d + "log.jsonl")
        n = sum(1 for r in log if r.get("kind") == "reply" and r.get("asks") and r["unix_ms"] / 1000 >= SINCE)
        if n:
            total[name] = n
    print(f"replies with an ask, last {DAYS} d: {sum(total.values())} ({sum(total.values()) / DAYS:.1f} a day); "
          + ", ".join(f"{k} {v}" for k, v in total.most_common(5)))


if __name__ == "__main__":
    {"tags": tags, "chats": chat_list, "roots": roots, "asks": asks}[sys.argv[1] if len(sys.argv) > 1 else "tags"]()
