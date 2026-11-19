#!/usr/bin/env python3
"""Writes the mock pages of docs/design/client-plugins.md section 4 (task 543) next to this file.

Usage: build.py   (then capture.mjs shoots them)
Each page links the real crates/wsx-web/web/app.css, the default theme's tokens (theme.css, written from
theme.toml) and mock.css (the few rules a build would add). The data is the sample of studio-app's
console/accounts/mockups/b-plugin.png. Dark is the page default; a light page sets data-scheme="light".
Pages: <name>-<dark|light>-<390|1280>.html. 1280 pages set data-wide on .app, as the client does.
"""
import html
import pathlib
import re

HERE = pathlib.Path(__file__).parent
TOML = HERE / "../../../crates/wsx-web/web/themes/default/theme.toml"
e = html.escape


def tokens():
    block, found = None, {"tokens": [], "tokens_dark": []}
    for line in TOML.read_text().splitlines():
        if line.startswith("["):
            block = line.strip("[]")
        elif block in found and re.match(r"^[a-z0-9-]+ = ", line):
            key, value = line.split(" = ", 1)
            found[block].append("--%s:%s" % (key, value.strip().strip('"')))
    return found


css = tokens()
(HERE / "theme.css").write_text(
    ':root{color-scheme:light;%s}\n:root:not([data-scheme="light"]),[data-scheme="dark"]{color-scheme:dark;%s}\n'
    % (";".join(css["tokens"]), ";".join(css["tokens_dark"])))
PATH = {"fail": "M5.5 5.5l9 9M14.5 5.5l-9 9", "back": "M12.5 4.5 7 10l5.5 5.5", "search": "M8.5 3.5a5 5 0 1 0 0 10 5 5 0 0 0 0-10ZM12.2 12.2l4.3 4.3"}


def icon(name):
    return ('<svg class="icon" viewBox="0 0 20 20" width="20" height="20" fill="none" stroke="currentColor" stroke-width="1.75" '
            'stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="%s" /></svg>' % PATH[name])


CMD = "services/publishing/bin/publish.cjs"
TONE = {"posting": "accent", "broken": "danger", "created": "attention", "planned": "attention"}
CHANNELS = [
    ("United States, English", [("story-shorts-us-yt", "posting"), ("story-shorts-us-tt", "posting"), ("story-shorts-us-ig", "broken"),
                                ("money-explainer-us-yt", "posting")]),
    ("Mexico, Spanish", [("story-shorts-mx-yt", "created")]),
    ("Japan, Japanese", [("story-shorts-jp-yt", "planned")]),
]
HEALTH_OK = "story-shorts-us-yt   ok\nstory-shorts-us-tt   ok\nmoney-explainer-us-yt ok"
HEALTH_BAD = "story-shorts-us-yt   ok\nstory-shorts-us-tt   ok\nstory-shorts-us-ig   failed: login expired\nmoney-explainer-us-yt ok"


def word(state):
    return '<span class="word" data-tone="%s">%s</span>' % (TONE[state], state)


def channel_list(selected=None, wide=False):
    groups = ""
    for name, rows in CHANNELS:
        items = "".join(
            '<li class="row"><a class="row-link" href="#"%s><span class="row-main"><span class="row-label">%s</span></span>%s</a></li>'
            % (' aria-current="page"' if label == selected else "", label, word(state)) for label, state in rows)
        groups += '<section class="group"><div class="group-head"><h2 class="group-name">%s</h2></div><ul class="rows">%s</ul></section>' % (name, items)
    more = '<div class="actions"><button class="link-action" type="button">Open a channel</button></div>'
    return '<nav class="plugin-list" aria-label="Channels">%s%s</nav>' % (groups, more)


def detail(running=False, back=False, compact=False):
    taken = [("story-shorts-us-yt", "UK_Charlotte", "quiet-cinema"), ("story-shorts-mx-yt", "es voice 2", "paper-cut")]
    if compact:
        head, rows = "<th>Taken by</th><th>Voice, look</th>", "".join("<tr><td>%s</td><td>%s<br>%s</td></tr>" % row for row in taken)
        rows += '<tr><td><strong>this channel</strong></td><td data-tone="attention">pick both</td></tr>'
    else:
        head, rows = "<th>Taken by</th><th>Voice</th><th>Look</th>", "".join("<tr><td>%s</td><td>%s</td><td>%s</td></tr>" % row for row in taken)
        rows += '<tr><td><strong>this channel</strong></td><td data-tone="attention">pick one</td><td data-tone="attention">pick one</td></tr>'
    health = ('<button class="button" type="button" disabled>Running…</button>' if running
              else '<button class="button" type="button">Run health</button>')
    return (
        '<div class="plugin-detail">%s'
        '<h2 class="plugin-title"><span>story-shorts-jp-yt</span>%s</h2>'
        '<p class="muted">YouTube Shorts, Japan, Japanese, topic stories. Needed by the-keys (ja).</p>'
        '<h3 class="group-name">Keep apart from other YouTube channels</h3>'
        '<div class="table-wrap" tabindex="0"><table><thead><tr>%s</tr></thead><tbody>%s</tbody></table></div>'
        '<h3 class="group-name">Open it</h3><ol class="steps">'
        '<li class="step"><input type="checkbox" checked aria-label="Planned"><span class="step-text">Planned</span></li>'
        '<li class="step"><input type="checkbox" aria-label="Create the channel"><span class="step-text">Create the channel on YouTube <a href="#">↗</a></span></li>'
        '<li class="step"><input type="checkbox" aria-label="Log in once"><span class="step-text">Log in once</span>'
        '<button class="button" type="button">Open its profile</button></li>'
        '<li class="step"><input type="checkbox" aria-label="Handle"><label class="step-text step-field"><span>Handle</span>'
        '<input class="input" type="text" placeholder="@storyshorts_jp" autocomplete="off"></label><button class="button button-primary" type="button">Link</button></li>'
        '<li class="step"><input type="checkbox" aria-label="Check the login"><span class="step-text">Check the login</span>%s</li></ol></div>'
        % ('<a class="link-action plugin-back" href="#">%s All channels</a>' % icon("back") if back else "", word("planned"), head, rows, health))


def card(command, state, tone, age, text=None, note=None):
    body = '<div class="code-block"><pre>%s</pre></div>' % e(text) if text else ""
    note = '<p class="muted">%s</p>' % note if note else ""
    return (
        '<section class="run-card" aria-label="Result"><div class="run-head"><code class="run-cmd">%s</code>'
        '<span class="word" data-tone="%s">%s</span><span class="muted run-age">%s</span>'
        '<button class="icon-button" type="button" aria-label="Close" title="Close">%s</button></div>%s%s</section>' % (e(command), tone, state, age, icon("fail"), note, body))


def manager(menu=False):
    items = "".join('<a role="menuitem" href="#">%s</a>' % name for name in ("Sessions", "Usage", "Media", "Files", "Channels"))
    more = ('<div class="more"><button class="icon-button more-button" type="button" aria-label="More" aria-haspopup="menu" aria-expanded="%s"><span aria-hidden="true">⋯</span></button>%s</div>'
            % ("true" if menu else "false", '<div class="more-menu" role="menu">%s</div>' % items if menu else ""))
    bubbles = (
        '<div class="bubble" data-side="in" data-first><span class="bubble-author">manager</span><div class="bubble-box">'
        'story-shorts-jp-yt is planned. Steps 2 and 3 are yours: open Channels.</div><span class="bubble-time">14:02</span></div>'
        '<div class="bubble" data-side="out" data-first><div class="bubble-box">Which channel is broken?</div><span class="bubble-time">14:05</span></div>'
        '<div class="bubble" data-side="in" data-first><span class="bubble-author">manager</span><div class="bubble-box">'
        'story-shorts-us-ig: its login expired.</div><span class="bubble-time">14:05</span></div>')
    return (
        '<div class="page manager"><header class="bar"><a class="icon-button" href="#" aria-label="Back to home">%s</a>'
        '<div class="bar-title"><span class="bar-label">studio-app</span></div><div class="bar-tools"><button class="icon-button" type="button" aria-label="Find">%s</button>%s</div></header>'
        '<div class="chat"><div class="scroll chat-body">%s</div></div></div>' % (icon("back"), icon("search"), more, bubbles))


def sheet(inner, wide):
    grip = '<div class="task-sheet-top"><span class="sheet-grip" aria-hidden="true"></span></div>'
    return (
        '<div class="scrim"></div><dialog class="task-sheet" open aria-label="Channels">%s<div class="task-page page">'
        '<header class="bar"><button class="icon-button" type="button" aria-label="Back to the chat">%s</button>'
        '<div class="bar-title"><span class="bar-label">Channels</span><span class="bar-state">studio-app</span></div></header>%s</div></dialog>'
        % (grip, icon("back"), inner))


def plugin(wide, view, tail=""):
    """The frame's content, then the host's result card under it. `view`: list, detail or both."""
    if wide:
        body = '<div class="plugin-wide">%s<div class="scroll">%s</div></div>' % (channel_list("story-shorts-jp-yt", True), detail(compact=True))
    elif view == "list":
        body = '<div class="scroll">%s</div>' % channel_list()
    else:
        body = '<div class="scroll">%s</div>' % detail(back=True)
    return '<div class="plugin">%s%s</div>' % (body, tail)


def confirm(command, effect, name):
    return (
        '<div class="scrim scrim-top"></div><dialog class="sheet dlg" open aria-labelledby="dialog-title">'
        '<h2 class="sheet-title" id="dialog-title">Run %s?</h2><div class="code-block"><pre>%s</pre></div>'
        '<p class="muted">%s</p><div class="actions sheet-actions"><button class="button" type="button" autofocus>Cancel</button>'
        '<button class="button button-primary" type="button">Run</button></div></dialog>' % (name, e(command), effect))


def page(file, body, light, wide, label):
    text = ('<!doctype html>\n<html lang="en"%s><head><meta charset="utf-8"><title>%s</title>'
            '<meta name="viewport" content="width=device-width, initial-scale=1">'
            '<link rel="stylesheet" href="../../../crates/wsx-web/web/app.css"><link rel="stylesheet" href="theme.css">'
            '<link rel="stylesheet" href="mock.css"></head><body data-theme="default"><div id="app"><div class="app"%s><main class="screen">%s</main></div></div></body></html>\n'
            % (' data-scheme="light"' if light else "", label, " data-wide" if wide else "", body))
    (HERE / file).write_text(text)


for light in (False, True):
    tone = "light" if light else "dark"
    for wide in (False, True):
        size = "1280" if wide else "390"
        suffix = "-%s-%s.html" % (tone, size)
        run = lambda *args: page("%s" % args[0] + suffix, args[1], light, wide, args[0])
        # a: the project's menu
        run("a-menu", manager(menu=True))
        # b: the sheet, a list and a detail on a phone, both on a wide screen
        run("b-list", manager() + sheet(plugin(wide, "list"), wide))
        if not wide:
            run("b-detail", manager() + sheet(plugin(wide, "detail"), wide))
        # c: the tap, then an ok result of a read action
        cmd = "%s authorize story-shorts-jp-yt" % CMD
        run("c-tap", manager() + sheet(plugin(wide, "detail"), wide) + confirm(cmd, "Opens Chrome on the host Mac", "authorize"))
        run("c-result", manager() + sheet(plugin(wide, "detail", card("publish.cjs health", "ok", "accent", "2 s ago", HEALTH_OK)), wide))
        # d: a failed read action with its text, a failed action that shows no text
        run("d-failed", manager() + sheet(plugin(wide, "detail", card("publish.cjs health", "failed (exit 2)", "danger", "4 s ago", HEALTH_BAD)), wide))
        run("d-silent", manager() + sheet(plugin(wide, "detail", card("publish.cjs authorize story-shorts-jp-yt", "failed (exit 1)", "danger", "9 s ago",
                                                                          note="No output is shown for this action.")), wide))
