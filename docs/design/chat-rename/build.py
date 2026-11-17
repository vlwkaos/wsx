#!/usr/bin/env python3
"""Writes the mock pages of docs/design/chat-rename.md (task 496) next to this file.

Usage: build.py
Each page links the real crates/wsx-web/web/app.css, the default theme's stylesheet and mock.css (the
rules a build would add). Dark is the page default; `?light` is not needed: a light page sets
data-scheme="light" on <html>, as the client does (ui.js applyScheme).
"""
import html
import pathlib
import re

HERE = pathlib.Path(__file__).parent
THEME = "theme.css"
TOML = HERE / "../../../crates/wsx-web/web/themes/default/theme.toml"


def tokens():
    """The default theme's [tokens] and [tokens_dark] as theme.rs writes them: light on :root, dark unless data-scheme=light."""
    block, found = None, {"tokens": [], "tokens_dark": []}
    for line in TOML.read_text().splitlines():
        if line.startswith("["):
            block = line.strip("[]")
        elif block in found and re.match(r"^[a-z0-9-]+ = ", line):
            key, value = line.split(" = ", 1)
            found[block].append("--%s:%s" % (key, value.strip().strip('"')))
    return found


css = tokens()
(HERE / THEME).write_text(
    ':root{color-scheme:light;%s}\n:root:not([data-scheme="light"]),[data-scheme="dark"]{color-scheme:dark;%s}\n'
    % (";".join(css["tokens"]), ";".join(css["tokens_dark"])))
PATH = {
    "play": "M7 4.5v11l8.5-5.5z",
    "unchecked": "M10 3 18 17H2ZM10 8.5v3.5M10 14.5h.01",
    "back": "M12.5 4.5 7 10l5.5 5.5",
    "plus": "M10 4.5v11M4.5 10h11",
    "fail": "M5.5 5.5l9 9M14.5 5.5l-9 9",
    "search": "M8.5 3.5a5 5 0 1 0 0 10 5 5 0 0 0 0-10ZM12.2 12.2l4.3 4.3",
}


def icon(name):
    return ('<svg class="icon" viewBox="0 0 20 20" width="20" height="20" fill="none" stroke="currentColor" stroke-width="1.75" '
            'stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="%s" /></svg>' % PATH[name])


def trays(play=0, you=0):
    out = ""
    if you:
        out += '<span class="tray" data-kind="you" data-tone="attention">%s<span class="tray-count">%d</span></span>' % (icon("unchecked"), you)
    if play:
        out += '<span class="tray" data-kind="tasks">%s<span class="tray-count">%d</span></span>' % (icon("play"), play)
    return '<span class="trays">%s</span>' % out if out else ""


# id, shown name (title or none), project, is the project's home chat, line 2, unread, time, trays
CHATS = [
    dict(id="wsx", title=None, project="wsx", home=True, line="Task 496 waits for your check", news=True, time="2m ago", play=3, you=1),
    dict(id="web-income", title="Fee table", project="studio-app", home=False, line="Draft ready: the table has 3 fee lines", news=True, time="14m ago", play=0, you=0),
    dict(id="research", title=None, project="wsx", home=False, line="I found two ways to cut the poll; notes are in the log", news=False, time="1h ago", play=1, you=0),
    dict(id="studio-app", title=None, project="studio-app", home=True, line="Nothing to do", news=False, time="3h ago", play=0, you=0),
    dict(id="design-review", title=None, project="studio-app", home=False, line="The spacing on the invoice page is fixed", news=False, time="2d ago", play=0, you=0),
]
# B groups a project's chats under its home chat; the others keep the order above.
ORDER_B = ["wsx", "research", "studio-app", "web-income", "design-review"]


def shown(chat):
    return chat["title"] or (chat["project"] if chat["home"] else chat["id"])


def mark(chat):
    return '<span class="chat-mark" aria-hidden="true">%s</span>' % html.escape(chat["project"][:2].upper())


def row(chat, option):
    name, project = (chat["id"] if option == "before" else shown(chat)), chat["project"]
    news = '<span class="news-mark" role="img" aria-label="New message"></span>' if chat["news"] else ""
    tag = ""
    line = chat["line"]
    lead = ""
    klass = "row row-slide"
    if option == "before" and not chat["home"]:
        line = "%s · %s" % (project, line)
    if option == "a" and name != project:
        tag = '<span class="row-tag">%s</span>' % html.escape(project)
    if option == "b" and not chat["home"]:
        klass += " row-child"
    if option == "c":
        lead = mark(chat)
    label = "" if chat["home"] else ' aria-label="Chat %s of %s"' % (html.escape(name), html.escape(project))
    return (
        '<li class="%s" data-news="%s"><a class="row-link %s" href="#"%s>%s<span class="row-main"><span class="row-head">'
        '<span class="row-work">%s<span class="row-label">%s</span>%s</span>%s<span class="time">%s</span></span>'
        '<span class="row-context"%s>%s</span></span></a>'
        '<button class="row-remove" data-quiet type="button" aria-label="Dismiss chat" title="Dismiss">%s</button></li>'
        % (klass, "" if chat["news"] else "x", "home-row" if chat["home"] else "chat-row", label, lead, news, html.escape(name), tag,
           trays(chat["play"], chat["you"]), chat["time"], " data-news" if chat["news"] else "", html.escape(line), icon("fail")))


def home(option):
    chats = CHATS
    if option == "b":
        chats = [next(c for c in CHATS if c["id"] == i) for i in ORDER_B]
    rows = "".join(row(c, option) for c in chats)
    return (
        '<div class="page"><div class="home-top"><h1 class="title">Home</h1>'
        '<button class="icon-button new-chat" type="button" aria-label="New chat" title="New chat">%s</button></div>'
        '<div class="scroll"><ul class="rows home-rows" aria-label="Chats">%s</ul></div></div>' % (icon("plus"), rows))


def header(option, menu=False):
    chat = CHATS[1]
    name, project, cid = shown(chat), chat["project"], chat["id"]
    if option == "before":
        title = '<span class="bar-label">%s</span>' % cid
    elif option == "a":
        title = '<span class="bar-label">%s</span><span class="bar-state">%s</span>' % (html.escape(name), project)
    elif option == "b":
        title = '<span class="bar-label"><span class="bar-parent">%s ›</span> %s</span>' % (project, html.escape(name))
    else:
        title = '<span class="bar-mark">%s</span><span class="bar-text"><span class="bar-label">%s</span><span class="bar-state">%s</span></span>' % (
            mark(chat), html.escape(name), project)
    items = ('<button type="button" role="menuitem">Rename chat</button>' if option != "before" else "") + '<button type="button" role="menuitem">Close chat</button>'
    more = ('<div class="more"><button class="icon-button more-button" type="button" aria-label="More" aria-haspopup="menu" aria-expanded="%s"><span aria-hidden="true">⋯</span></button>%s</div>'
            % ("true" if menu else "false", '<div class="more-menu" role="menu">%s</div>' % items if menu else ""))
    chat_body = (
        '<div class="scroll chat-body"><div class="bubble" data-side="in" data-first><span class="bubble-author">manager</span><div class="bubble-box">'
        'Draft ready: the table has 3 fee lines. Which one goes first?</div><span class="bubble-time">14:02</span></div>'
        '<div class="bubble" data-side="out" data-first><div class="bubble-box">The setup fee first.</div><span class="bubble-time">14:05</span></div></div>'
        )
    return (
        '<div class="page manager"><header class="bar"><a class="icon-button" href="#" aria-label="Back to home">%s</a>'
        '<div class="bar-title%s">%s</div><div class="bar-tools"><button class="icon-button" type="button" aria-label="Find">%s</button>%s</div></header>'
        '<div class="chat">%s</div></div>' % (icon("back"), " bar-title-mark" if option == "c" else "", title, icon("search"), more, chat_body))


def sheet():
    return (
        '<div class="page">%s<dialog class="sheet" open aria-labelledby="rename-title"><form novalidate>'
        '<h2 class="sheet-title" id="rename-title">Rename chat</h2>'
        '<label class="field">Name<input class="input" type="text" value="Fee table" maxlength="40" autocomplete="off" />'
        '<span class="field-note">Shown on Home and in the chat. The chat keeps its id web-income, so links still work.</span></label>'
        '<div class="actions sheet-actions"><button class="button" type="button">Cancel</button>'
        '<button class="button button-primary" type="submit">Save</button></div></form></dialog></div>' % home("a"))


def page(file, body, light, label):
    scheme = ' data-scheme="light"' if light else ""
    text = ('<!doctype html>\n<html lang="en"%s><head><meta charset="utf-8"><title>%s</title>'
            '<meta name="viewport" content="width=device-width, initial-scale=1">'
            '<link rel="stylesheet" href="../../../crates/wsx-web/web/app.css"><link rel="stylesheet" href="%s">'
            '<link rel="stylesheet" href="mock.css"></head><body data-theme="default"><div id="app"><div class="app"><main class="screen">%s</main></div></div></body></html>\n'
            % (scheme, label, THEME, body))
    (HERE / file).write_text(text)


for light in (False, True):
    tone = "light" if light else "dark"
    for option in ("before", "a", "b", "c"):
        page("%s-home-%s.html" % (option, tone), home(option), light, "Home, option %s" % option)
        page("%s-chat-%s.html" % (option, tone), header(option, menu=option != "before"), light, "Chat header, option %s" % option)
    page("rename-sheet-%s.html" % tone, sheet(), light, "Rename sheet")
