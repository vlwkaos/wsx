#!/usr/bin/env python3
"""Writes the static mock pages of docs/design/task-sheet/README.md (task 533) next to this file.

Usage: build.py   (then capture.mjs shoots and measures them)
The helpers (tokens, icon, bar, bubble, page) are those of docs/design/chat-system/build.py: its source up to its page table is run here with
this folder as its home, so nothing is copied and its own pages are not rewritten. Before = the real app.css of HEAD when the task began
(git show, frozen as before.css) with the real DOM of task.js and story.js; after = the real app.css plus sheet.css.
"""
import html
import pathlib
import subprocess

HERE = pathlib.Path(__file__).resolve().parent
ROOT = HERE / "../../../"
source = (HERE / "../chat-system/build.py").read_text().split("\nPAGES = {")[0]
ns = {"__file__": str(HERE / "build.py"), "__name__": "chat_system_helpers"}
exec(compile(source, "chat-system/build.py", "exec"), ns)
ns["PATH"]["chevron"] = "M7.5 4.5 13 10l-5.5 5.5"
icon, bubble, page, e = ns["icon"], ns["bubble"], ns["page"], html.escape

BEFORE = "fe0a7c25"
(HERE / "before.css").write_text(subprocess.run(["git", "show", "%s:crates/wsx-web/web/app.css" % BEFORE], cwd=ROOT, check=True, capture_output=True, text=True).stdout)

# The four states: the words are wsx-core's task::status words; the worker's text is made up in the shape of a real one.
STATES = {
    "ready": dict(
        id=530, title="Web: chat box container like the Home box", tone="accent", label="Ready, waiting for the manager's review", age="23 min", model="sonnet high",
        goal="Goal.", steps=("plan", "build", "review"), step=3, now=None, flow=("Lands into", "main"),
        summary=["Your correction needs no new mockup: the after pictures already have no band, and the measurement now proves it. I changed no product file, as planned. The band rules stay on main until you pick a variant. Question: strip inside the card (A) or above it (B)? Also: is a card with the box inside what you want?",
                 "Decided: no new mockup (the after pictures already show it)."],
        sentence="Ready, waiting for the manager's review for 23 min: checks 4 of 4 passed. The manager reads the diff, then lands it or asks for changes.",
        tail="Checks 4 of 4 passed. The manager reads the diff, then lands it or asks for changes.",
        actions=[("Land", True), ("Take over", False)], report="4 checks passed · 3 left open", messages=1),
    "working": dict(
        id=98, title="Fee table: round to whole cents", tone="working", running=True, label="Working on build (2 of 3)", age="6 min", model="sonnet high",
        goal="Round every fee line to whole cents and show the total.", steps=("plan", "build", "review"), step=1, now="editing fees.rs", flow=("Lands into", "main"),
        summary=[], sentence="Working on build (2 of 3) for 6 min. Its checks run after its result.", tail="Its checks run after its result.",
        actions=[("Take over", False)], report="", messages=0),
    "owner": dict(
        id=96, title="Fee table: add the setup fee line", tone="attention", label="Waiting for your answer", age="16 min", model="sonnet high",
        goal="Add the setup fee line to the invoice page.", steps=("plan", "build", "review"), step=3, now=None, flow=("Lands into", "main"),
        ask="Which fee line goes first, setup or monthly?",
        summary=["Built the fee table on the invoice page with the setup, monthly and total lines. I put setup first. Which line goes first is your call.", "Decided: setup first, because the invoice reads top to bottom as it is charged."],
        sentence="Waiting for your answer for 16 min: Which fee line goes first, setup or monthly? The work is ready and checked. Answer the question, or land it as it is.",
        tail="The work is ready and checked. Answer the question, or land it as it is.",
        actions=[("Answer", True), ("Land", False), ("Take over", False)], report="2 checks passed · 1 left open", messages=0),
    "landed": dict(
        id=97, title="Changelog for 0.43", tone="muted", label="Landed", age="2 h ago", bar_state="done", model="",
        goal="Write the 0.43 changelog section from the merged tasks.", steps=(), step=0, now=None, flow=None,
        summary=["Added the 0.43 section to the changelog: 12 entries, grouped by contract, each with the task number.", "Decided: one section per release (the older ones stay as they were)."],
        sentence="", tail="", actions=[], report="3 checks passed", messages=0),
}


def mark(s):
    return '<span class="mark" data-tone="%s"%s aria-hidden="true"></span>' % (s["tone"], " data-running" if s.get("running") else "")


def top(s, bar_state):
    state = '<span class="bar-state" data-tone="muted">%s</span>' % s["bar_state"] if bar_state and s.get("bar_state") else ""
    return ('<header class="bar"><button class="icon-button" type="button" aria-label="Back to the chat">%s</button><div class="bar-title">'
            '<span class="row-title"><span class="row-label">%s</span><span class="task-number">#%d</span></span>%s</div></header>' % (icon("back"), e(s["title"]), s["id"], state))


def steps(s):
    done = lambda i: i < s["step"]
    return '<ol class="progress-steps" aria-label="Workflow steps">%s</ol>' % "".join(
        '<li%s%s>%s%s</li>' % (" data-done" if done(i) else "", ' aria-current="step"' if i == s["step"] else "", icon("pass") if done(i) else "", n) for i, n in enumerate(s["steps"]))


def flow(s):
    return '<dl class="facts task-flow" aria-label="Where it goes"><dt>%s</dt><dd>%s</dd></dl>' % tuple(s["flow"]) if s["flow"] else ""


def buttons(s, small=False):
    return "".join('<button class="button%s" type="button">%s</button>' % (" button-primary" if primary and small else "", w) for w, primary in s["actions"])


def folds(s):
    out = ""
    if s["report"]:
        out += '<details class="task-section task-fold"><summary class="task-heading fold-head">%s Report<span class="fold-note"> · %s</span></summary></details>' % (icon("chevron"), s["report"])
    if s["messages"]:
        out += '<details class="task-section task-fold"><summary class="task-heading fold-head">%s Messages<span class="fold-note"> · %d</span></summary></details>' % (icon("chevron"), s["messages"])
    return out + '<details class="task-section timeline"><summary class="task-heading fold-head">%s History</summary></details>' % icon("chevron")


def md(paragraphs):
    return '<div class="markdown">%s</div>' % "".join("<p>%s</p>" % e(p) for p in paragraphs)


def story_before(s):
    """The DOM of story.js TaskStory and task.js today."""
    goal = '<p class="story-goal">%s</p>' % e(s["goal"])
    if s["steps"]:
        age = "" if s["tone"] == "accent" or s["id"] == 96 else '<span class="progress-age">%s</span>' % s["age"]  # no age once the task ended (story.js `ended`)
        head = ('<p class="progress-head">%s<span class="progress-health" data-tone="%s">%s</span>%s%s</p>'
                % (mark(s), s["tone"], e(s["label"]), '<span class="progress-model" title="Model">%s</span>' % s["model"] if s["model"] else "", age))
        now = '<p class="progress-now">%s</p>' % s["now"] if s["now"] else ""
        top_block = '<div class="progress">%s%s%s%s</div>' % (head, goal, steps(s), now)
    else:
        top_block = goal
    lead = s["summary"][:1]
    summary = ('<div class="story-summary">%s%s</div>' % (md(lead), '<button class="link-action story-more" type="button">More</button>' if len(s["summary"]) > 1 else "")) if lead else ""
    act = ""
    if s["sentence"] or s["actions"]:
        act = '<div class="story-act">%s%s</div>' % ('<p class="story-next">%s</p>' % e(s["sentence"]) if s["sentence"] else "",
                                                     '<div class="task-next" role="group" aria-label="Actions"><div class="actions">%s</div></div>' % buttons(s) if s["actions"] else "")
    return '<section class="story" aria-label="Where this task stands">%s%s%s%s</section>' % (top_block, flow(s), summary, act)


def story_after(s):
    """The title is the headline; the state once; the actions on their own line; the rest in More."""
    state = ('<p class="story-state">%s<span class="progress-health" data-tone="%s">%s</span> <span class="progress-age">· %s</span></p>'
             % (mark(s), s["tone"], e(s["label"]), s["age"]))
    ask = '<p class="story-ask">%s</p>' % e(s["ask"]) if s.get("ask") else ""
    working = (steps(s) + ('<p class="progress-now">%s</p>' % s["now"] if s["now"] else "")) if s.get("running") else ""
    acts = '<div class="story-actions" role="group" aria-label="Actions">%s</div>' % buttons(s, True) if s["actions"] else ""
    inside = "".join(filter(None, [
        '<p class="progress-model" title="Model">%s</p>' % s["model"] if s["model"] else "", flow(s), '<p>%s</p>' % e(s["tail"]) if s["tail"] else "", md(s["summary"]) if s["summary"] else ""]))
    more = '<details class="task-fold story-fold"><summary class="fold-head">%s More</summary><div class="story-fold-body">%s</div></details>' % (icon("chevron"), inside)
    return '<section class="story" aria-label="Where this task stands">%s%s%s%s%s</section>' % (state, ask, working, acts, more)


def sheet(s, after):
    story = story_after(s) if after else story_before(s)
    inner = '<div class="page task-page">%s<div class="scroll">%s%s</div></div>' % (top(s, not after), story, folds(s))
    chat = ('<div class="page manager">%s<div class="chat"><div class="scroll chat-body">%s</div></div></div>'
            % (ns["bar"]("wsx", ""), bubble("out", "Is #%d ready?" % s["id"], time="14:01") + bubble("in", "Yes. It waits for the manager's review.", "manager", "14:02")))
    return '%s<div class="sheet-scrim"></div><dialog class="task-sheet" open><div class="task-sheet-top"><span class="sheet-grip"></span></div>%s</dialog>' % (chat, inner)


def write(name, body, light, label, after):
    page(name, body, light, label)
    text = (HERE / name).read_text()
    app = '<link rel="stylesheet" href="../../../crates/wsx-web/web/app.css">'
    css = app + '<link rel="stylesheet" href="sheet.css">' if after else '<link rel="stylesheet" href="before.css">'
    (HERE / name).write_text(text.replace('<link rel="stylesheet" href="mock.css">', "", 1).replace(app, css + '<link rel="stylesheet" href="mock.css">', 1))


WRAP = ('<!doctype html>\n<html lang="en"%s><head><meta charset="utf-8"><title>%s</title><style>body{margin:0;background:#888;font:600 14px system-ui;color:#fff}'
        '.pair{display:flex;gap:20px;padding:10px}.pair div{display:flex;flex-direction:column;gap:6px}iframe{border:0;width:%dpx;height:%dpx;background:#000}</style></head>'
        '<body><div class="pair"><div>Before (today)<iframe src="x-sheet-%s-before-%s.html"></iframe></div><div>After<iframe src="x-sheet-%s-after-%s.html"></iframe></div></div></body></html>\n')
LABEL = {"ready": "A ready task", "working": "A working task", "owner": "A task waiting for the owner", "landed": "A landed task"}
for light in (False, True):
    tag = "light" if light else "dark"
    for name, s in STATES.items():
        for after in (False, True):
            write("x-sheet-%s-%s-%s.html" % (name, "after" if after else "before", tag), sheet(s, after), light, "%s, %s" % (LABEL[name], "after" if after else "before"), after)
        for width, height in ((390, 844), (1280, 800)):
            (HERE / ("sheet-%s-%s-%d.html" % (name, tag, width))).write_text(WRAP % (' data-scheme="light"' if light else "", LABEL[name], width, height, name, tag, name, tag))
