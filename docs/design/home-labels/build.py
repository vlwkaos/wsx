#!/usr/bin/env python3
"""Writes the static mock pages of docs/design/home-labels.md (task 541) next to this file.

Usage: build.py   (then capture.mjs shoots them)
Each page links app.css (the one of task 510's branch: Home with the For you row and one state on every
chat row; written here by `git show`, as theme.css is written from theme.toml), theme.css and mock.css (the few
rules a build would add). Dark is the page default; a light page sets data-scheme="light", as the client does.
Option A: labels (a chip row filters, a row shows its labels, a picker sets them).
Option B: groups (a header per group, a menu moves a chat).
"""
import html
import pathlib
import re
import subprocess

HERE = pathlib.Path(__file__).parent
ROOT = HERE / "../../../"
TOML = ROOT / "crates/wsx-web/web/themes/default/theme.toml"
BRANCH = "wsx/t510"  # ^ Task 510 lands first; then app.css is main's and this line goes.
e = html.escape

(HERE / "app.css").write_text(subprocess.run(
    ["git", "show", "%s:crates/wsx-web/web/app.css" % BRANCH], cwd=ROOT, check=True, capture_output=True, text=True).stdout)
block, found = None, {"tokens": [], "tokens_dark": []}
for line in TOML.read_text().splitlines():
    if line.startswith("["):
        block = line.strip("[]")
    elif block in found and re.match(r"^[a-z0-9-]+ = ", line):
        key, value = line.split(" = ", 1)
        found[block].append("--%s:%s" % (key, value.strip().strip('"')))
(HERE / "theme.css").write_text(
    ':root{color-scheme:light;%s}\n:root:not([data-scheme="light"]),[data-scheme="dark"]{color-scheme:dark;%s}\n'
    % (";".join(found["tokens"]), ";".join(found["tokens_dark"])))

PATH = {
    "play": "M7 4.5v11l8.5-5.5z",
    "unchecked": "M10 3 18 17H2ZM10 8.5v3.5M10 14.5h.01",
    "plus": "M10 4.5v11M4.5 10h11",
    "fail": "M5.5 5.5l9 9M14.5 5.5l-9 9",
    "pass": "M4.5 10.5l3.5 3.5 7-8",
    "down": "M5 7.5l5 5 5-5",
    "right": "M7.5 5l5 5-5 5",
}


def icon(name, size=20):
    return ('<svg class="icon" viewBox="0 0 20 20" width="%d" height="%d" fill="none" stroke="currentColor" stroke-width="1.75" '
            'stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="%s" /></svg>' % (size, size, PATH[name]))


def trays(play=0, you=0):
    out = ""
    if you:
        out += '<span class="tray" data-kind="you" data-tone="attention">%s<span class="tray-count">%d</span></span>' % (icon("unchecked"), you)
    if play:
        out += '<span class="tray" data-kind="tasks">%s<span class="tray-count">%d</span></span>' % (icon("play"), play)
    return '<span class="trays">%s</span>' % out if out else ""


# The owner's chats in homeOrder (needs, then unread, then newest message). name, projects, message, unread, time, running, for you,
# labels (A), group (B).
CHATS = [
    ("wsx", "wsx", "Task 496 waits for your check", True, "2m ago", 3, 3, ("work", "ui"), "Work"),
    ("studio-app", "studio-app", "Draft ready: the table has 3 fee lines", True, "14m ago", 0, 1, ("client",), "Clients"),
    ("wsx-ui", "wsx", "Home rows share one state now", False, "38m ago", 1, 0, ("work", "ui"), "Work"),
    ("shopfront", "shopfront", "The menu page waits for your review", False, "1h ago", 0, 0, ("client",), "Clients"),
    ("research", "wsx", "I found two ways to cut the poll; notes are in the log", False, "2h ago", 0, 0, ("ideas", "work"), None),
    ("projk", "projk", "Release notes are in the log", False, "3h ago", 0, 0, ("personal",), "Personal"),
    ("dotfiles", "dotfiles", "Both repos are in sync", False, "5h ago", 0, 0, ("personal", "ops"), "Personal"),
]
LABELS = ["work", "ui", "client", "personal", "ideas", "ops"]
GROUPS = ["Work", "Clients", "Personal"]
MENU_CHAT = "wsx"


def chat_row(chat, labels=False, name_tail=""):
    name, projects, line, news, time, play, you, tags, _ = chat
    mark = '<span class="news-mark" role="img" aria-label="New message"></span>' if news else ""
    tail = '<span class="row-labels">%s</span>' % "".join("<span>%s</span>" % e(t) for t in tags) if labels else ""
    return (
        '<li class="row row-slide"><a class="row-link chat-row" href="#"><span class="row-main"><span class="row-head">'
        '<span class="row-work">%s<span class="row-label">%s</span>%s</span>%s<span class="time">%s</span></span>'
        '<span class="row-projects">%s</span><span class="row-context"%s>%s</span></span></a>'
        '<button class="row-remove" data-quiet type="button" aria-label="Dismiss chat" title="Dismiss">%s</button>'
        '<button class="row-more" type="button" aria-label="More" aria-haspopup="menu"><span aria-hidden="true">⋯</span></button></li>'
        % (mark, e(name), tail, trays(play, you), time, e(projects), " data-news" if news else "", e(line), icon("fail")))


def for_you_row():
    return (
        '<li class="row"><a class="row-link chat-row" href="#"><span class="row-main"><span class="row-head"><span class="row-work">'
        '<span class="row-label">For you</span></span><span class="trays"><span class="tray" data-kind="you" data-tone="attention">%s'
        '<span class="tray-count">5</span></span></span></span><span class="row-context">5 items in 3 chats: 2 questions, 1 approval, 2 checks</span></span></a></li>'
        % icon("unchecked"))


def top():
    return '<div class="home-top"><h1 class="title">Home</h1><button class="icon-button new-chat" type="button" aria-label="New chat">%s</button></div>' % icon("plus")


# ---- A: labels ----
def chips(active):
    return ('<nav class="threads label-chips" aria-label="Labels">%s</nav>'
            % "".join('<button class="thread-chip" type="button" aria-current="%s">%s</button>' % ("true" if n == active else "false", e(n)) for n in ["All"] + LABELS))


def home_a(active="All"):
    shown = [c for c in CHATS if active == "All" or active in c[7]]
    return ('<div class="page">%s%s<div class="scroll"><ul class="rows home-rows" aria-label="Chats">%s%s</ul></div></div>'
            % (top(), chips(active), for_you_row(), "".join(chat_row(c, labels=True) for c in shown)))


def tick(on, radio=False):
    return '<span class="pick-tick%s">%s</span>' % (" radio" if radio else "", icon("pass") if on else "")


def sheet(title, options, field, radio):
    rows = "".join('<li><button class="pick-opt" type="button" role="%s" aria-checked="%s">%s<span>%s</span></button></li>'
                   % ("menuitemradio" if radio else "menuitemcheckbox", "true" if on else "false", tick(on, radio), e(n)) for n, on in options)
    return ('<div class="scrim"></div><div class="pick-sheet" role="dialog" aria-label="%s"><div class="pick-top"><span class="sheet-grip"></span></div>'
            '<div class="pick-title">%s</div><ul class="pick-opts">%s</ul>'
            '<form class="pick-new"><input class="input" type="text" placeholder="%s" aria-label="%s"><button class="button button-primary" type="button">Add</button></form></div>'
            % (e(title), e(title), rows, field, field))


def picker_a():
    mine = next(c for c in CHATS if c[0] == MENU_CHAT)[7]
    return home_a() + sheet(MENU_CHAT, [(n, n in mine) for n in LABELS], "New label", False)


# ---- B: groups ----
def head(name, count, folded, news=False):
    return ('<li class="row-band group-head"><button class="group-toggle" type="button" aria-expanded="%s">%s<span class="group-name">%s</span>'
            '<span class="group-count">%d</span>%s</button></li>'
            % ("false" if folded else "true", icon("right" if folded else "down", 16), e(name), count,
               '<span class="news-mark" role="img" aria-label="New message"></span>' if news else ""))


def home_b(folded=()):
    out = for_you_row()
    for group in GROUPS + [None]:
        members = [c for c in CHATS if c[8] == group]
        shut = group in folded
        out += head(group or "Ungrouped", len(members), shut, news=shut and any(c[3] for c in members))
        if not shut:
            out += "".join(chat_row(c) for c in members)
    return '<div class="page">%s<div class="scroll"><ul class="rows home-rows" aria-label="Chats">%s</ul></div></div>' % (top(), out)


def move_b():
    mine = next(c for c in CHATS if c[0] == MENU_CHAT)[8]
    return home_b() + sheet(MENU_CHAT, [(g, g == mine) for g in GROUPS] + [("Ungrouped", mine is None)], "New group", True)


def page(file, body, light, label):
    scheme = ' data-scheme="light"' if light else ""
    (HERE / file).write_text(
        '<!doctype html>\n<html lang="en"%s><head><meta charset="utf-8"><title>%s</title><meta name="viewport" content="width=device-width, initial-scale=1">'
        '<link rel="stylesheet" href="app.css"><link rel="stylesheet" href="theme.css"><link rel="stylesheet" href="mock.css"></head>'
        '<body data-theme="default"><div id="app"><div class="app"><main class="screen">%s</main></div></div>'
        '<script>if (innerWidth >= 900) document.querySelector(".app").setAttribute("data-wide", "")</script></body></html>\n' % (scheme, label, body))


# A dark page shows the whole list; a light page shows the state the owner is in after using it: the filter "work" (A), the group "Clients" folded (B).
for light in (False, True):
    tag = "light" if light else "dark"
    page("home-a-%s.html" % tag, home_a("work" if light else "All"), light, "Home, option A: labels")
    page("home-b-%s.html" % tag, home_b(("Clients",) if light else ()), light, "Home, option B: groups")
page("picker-a-dark.html", picker_a(), False, "Option A: the label picker")
page("move-b-dark.html", move_b(), False, "Option B: the move menu")
