#!/usr/bin/env python3
"""Writes the static mock pages of docs/design/chat-system.md (task 508) next to this file.

Usage: build.py   (then capture.mjs shoots them)
Each page links the real crates/wsx-web/web/app.css, the default theme's tokens (theme.css, written from
theme.toml) and mock.css (the few rules a build would add). The data is made up in the shape of real chats.
Dark is the page default; a light page sets data-scheme="light" on <html>, as the client does.
"""
import html
import pathlib
import re

HERE = pathlib.Path(__file__).parent
TOML = HERE / "../../../crates/wsx-web/web/themes/default/theme.toml"


def tokens():
    """The default theme's [tokens] and [tokens_dark] as theme.rs writes them."""
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
PATH = {
    "play": "M7 4.5v11l8.5-5.5z",
    "unchecked": "M10 3 18 17H2ZM10 8.5v3.5M10 14.5h.01",
    "back": "M12.5 4.5 7 10l5.5 5.5",
    "plus": "M10 4.5v11M4.5 10h11",
    "fail": "M5.5 5.5l9 9M14.5 5.5l-9 9",
    "search": "M8.5 3.5a5 5 0 1 0 0 10 5 5 0 0 0 0-10ZM12.2 12.2l4.3 4.3",
    "pause": "M7 4.5v11M13 4.5v11",
    "pass": "M4.5 10.5l3.5 3.5 7-8",
    "send": "M10 16V4.5M5 9l5-5 5 5",
    "up": "M5 12.5l5-5 5 5",
    "attach": "M14.5 9.5 9 15a3 3 0 0 1-4.2-4.2l6-6a2 2 0 0 1 2.8 2.8l-5.7 5.7a1 1 0 0 1-1.4-1.4L11 7.4",
}
e = html.escape


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


# name, the projects the chat involves, the message, unread, time, running tasks, items for you
CHATS = [
    ("wsx", "wsx", "Task 496 waits for your check", True, "2m ago", 3, 3),
    ("Fee table", "studio-app", "Draft ready: the table has 3 fee lines", True, "14m ago", 0, 1),
    ("machine", "dotfiles, shopfront", "Both repos are in sync", False, "52m ago", 0, 0),
    ("research", "wsx", "I found two ways to cut the poll; notes are in the log", False, "1h ago", 1, 0),
    ("studio-app", "studio-app", "Nothing to do", False, "3h ago", 0, 0),
]


def chat_row(name, projects, line, news, time, play, you):
    mark = '<span class="news-mark" role="img" aria-label="New message"></span>' if news else ""
    return (
        '<li class="row row-slide" data-news="%s"><a class="row-link chat-row" href="#"><span class="row-main"><span class="row-head">'
        '<span class="row-work">%s<span class="row-label">%s</span></span>%s<span class="time">%s</span></span>'
        '<span class="row-projects">%s</span><span class="row-context"%s>%s</span></span></a>'
        '<button class="row-remove" data-quiet type="button" aria-label="Dismiss chat" title="Dismiss">%s</button></li>'
        % ("" if news else "x", mark, e(name), trays(play, you), time, e(projects), " data-news" if news else "", e(line), icon("fail")))


def for_you_row():
    return (
        '<li class="row"><a class="row-link chat-row" href="#"><span class="row-main"><span class="row-head"><span class="row-work">'
        '<span class="row-label">For you</span></span><span class="trays"><span class="tray" data-kind="you" data-tone="attention">%s'
        '<span class="tray-count">5</span></span></span></span><span class="row-context">5 items in 3 chats: 2 questions, 1 approval, 2 checks</span></span></a></li>'
        % icon("unchecked"))


def home():
    return ('<div class="page"><div class="home-top"><h1 class="title">Home</h1><button class="icon-button new-chat" type="button" aria-label="New chat">%s</button></div>'
            '<div class="scroll"><ul class="rows home-rows" aria-label="Chats">%s%s</ul></div></div>'
            % (icon("plus"), for_you_row(), "".join(chat_row(*c) for c in CHATS)))


def bar(title, state, menu=False, items=("Rename chat", "Close chat")):
    more = ('<div class="more"><button class="icon-button more-button" type="button" aria-label="More" aria-haspopup="menu" aria-expanded="%s"><span aria-hidden="true">⋯</span></button>%s</div>'
            % ("true" if menu else "false", '<div class="more-menu" role="menu">%s</div>' % "".join('<button type="button" role="menuitem">%s</button>' % i for i in items) if menu else ""))
    return ('<header class="bar"><a class="icon-button" href="#" aria-label="Back">%s</a><div class="bar-title"><span class="bar-label">%s</span>'
            '<span class="bar-state">%s</span></div><div class="bar-tools"><button class="icon-button" type="button" aria-label="Find">%s</button>%s</div></header>'
            % (icon("back"), title, e(state), icon("search"), more))


# The chat first, then every thread that waits for you with its count, whatever its age; the rest by last activity.
THREADS = [("Chat", 0, "the chat's own messages"), ("Fee table", 2, "1 question, 1 check for you"), ("Changelog", 0, "#97 working · 20 min ago"), ("Poll cut", 0, "idle · 2 days ago")]


def strip(active):
    chips = [(n, k) for n, k, _ in THREADS]
    return ('<nav class="threads" data-above aria-label="Threads">%s<button class="thread-new" type="button" aria-label="New thread">%s</button></nav>'
            % ("".join('<a class="thread-chip" href="#" aria-current="%s">%s%s</a>' % ("true" if n == active else "false", e(n), '<span class="thread-ask">%d</span>' % k if k else "")
                       for n, k in chips), icon("plus")))


def bubble(side, text, author=None, time="", first=True, asks=False):
    return ('<div class="bubble" data-side="%s"%s%s>%s<div class="bubble-box">%s</div><span class="bubble-time">%s</span></div>'
            % (side, " data-first" if first else "", " data-asks" if asks else "",
               '<span class="bubble-author">%s</span>' % author if author else "", text, time))


def note(text):
    return '<p class="note note-row">%s</p>' % text


def pick(active):
    """Option B: the thread button of the composer row; its badge counts what waits in the other threads."""
    waits = sum(k for n, k, _ in THREADS if n != active)
    return ('<button class="pill thread-pick" type="button" aria-haspopup="dialog" aria-label="Threads"><span class="thread-pick-name">%s</span>%s%s</button>'
            % (e(active), '<span class="thread-ask">%d</span>' % waits if waits else "", icon("up")))


def sheet(active):
    """Option B: the bottom sheet; the same order as the strip, New thread last."""
    rows = "".join(
        '<li><button class="thread-opt" type="button" aria-current="%s"><span class="thread-opt-main"><span class="thread-opt-name">%s</span><span class="thread-opt-line">%s</span></span>%s</button></li>'
        % ("true" if n == active else "false", e(n), e(line), '<span class="thread-ask">%d</span>' % k if k else "") for n, k, line in THREADS)
    return ('<div class="scrim"></div><div class="thread-sheet" role="dialog" aria-label="Threads"><div class="task-sheet-top"><span class="sheet-grip"></span></div>'
            '<ul class="thread-opts">%s</ul><button class="thread-opt thread-opt-new" type="button">%s<span class="thread-opt-name">New thread</span></button></div>' % (rows, icon("plus")))


def composer(token=None, you=2, tasks=3, threads="", real=False, box=""):
    """The composer DOM of tasks.js. real: the paperclip and the placeholder the client has; box: a desktop variant (composer.css rules in mock.css)."""
    pills = ('<div class="pills" role="group"><button class="pill" type="button" data-kind="you" data-tone="attention">%s<span class="pill-count">%d</span><span class="pill-label">For you</span></button>'
             '<button class="pill" type="button" data-kind="tasks">%s<span class="pill-count">%d</span><span class="pill-label">Tasks</span></button></div>' % (icon("unchecked"), you, icon("play"), tasks))
    hint = "Message \u00b7 # task @ project / skill" if real else ("Message in " + token if token else "Message")
    return ('<div class="composer"%s><div class="composer-row"><button class="icon-button" type="button" aria-label="%s">%s</button>%s%s<div class="composer-input"><textarea class="composer-field" rows="1" aria-label="Message" placeholder="%s"></textarea></div>'
            '<button class="button button-primary composer-send" type="button" aria-label="Send"><span class="send-mark">%s</span><span class="send-text">Send</span></button></div></div>'
            % (' data-box="%s"' % box if box else "", "Attach files" if real else "Attach", icon("attach" if real else "plus"), pills, threads, hint, icon("send")))


def tasks_in(rows):
    return ('<div class="reservoir inline thread-tasks"><ul class="reservoir-list" aria-label="Tasks of this thread">%s</ul></div>'
            % "".join(task_row(num, title, glyph, tone, now, True) for num, title, glyph, tone, now in rows))


def chat_trunk(option):
    body = (bubble("out", "What is left before the 0.43 release?", time="14:01")
            + bubble("in", "Two things: the changelog (#97, working) and your check of #96. Nothing else blocks the tag.", "manager", "14:02")
            + note('<span class="chip">#94</span> landed · 2 of 2 checks')
            + note("Talked to research · Show")
            + note("Opened the thread <b>Fee table</b> from your message of 13:40 · #96")
            + note('<b>Fee table</b> waits for you · 1 question, 1 check · <a href="#">Open</a>')
            + note("<b>Changelog</b>: #97 working, nothing waits for you"))
    return '<div class="page manager">%s<div class="chat"><div class="scroll chat-body">%s</div>%s%s</div></div>' % (
        bar("wsx", "", menu=False), body, strip("Chat") if option == "a" else "", composer(you=3, threads=pick("Chat") if option == "b" else ""))


def thread_page(option, menu=False, open_sheet=False):
    body = (tasks_in([(96, "Fee table: add the setup fee line", "unchecked", "attention", "waits for your check · 2 of 2 checks"),
                      (98, "Fee table: round to whole cents", "play", "working", "working · build 2/3")])
            + note("Thread opened 13:40 · 1 question and 1 check for you")
            + bubble("out", "Put the fee table in the invoice page too.", time="13:40")
            + bubble("in", "Started #96 for the invoice page and #98 for the rounding. One question: which fee line goes first, setup or monthly?", "manager", "13:44", asks=True)
            + note('<span class="chip">#98</span> working · build 2/3'))
    return '<div class="page manager">%s<div class="chat"><div class="scroll chat-body">%s</div>%s%s</div>%s</div>' % (
        bar('<span class="bar-parent">wsx ›</span> Fee table', "open since 13:40", menu=menu, items=("Rename thread", "Merge into…", "Close thread")),
        body, strip("Fee table") if option == "a" else "", composer(token="Fee table", you=1, tasks=2, threads=pick("Fee table") if option == "b" else ""),
        sheet("Fee table") if open_sheet else "")


def composer_page(box):
    """Task 512: the desktop composer. box "" is today's (no thread strip: threads do not exist yet); "a" and "b" are the options."""
    body = (bubble("out", "What is left before the 0.43 release?", time="14:01")
            + bubble("in", "Two things: the changelog (#97, working) and your check of #96. Nothing else blocks the tag.", "manager", "14:02")
            + note('<span class="chip">#94</span> landed \u00b7 2 of 2 checks')
            + bubble("out", "Start #99 after #97 lands, and tell me when the tag is ready.", time="14:05")
            + bubble("in", "Done: #99 waits for #97. I will write here when the tag is ready.", "manager", "14:05"))
    return '<div class="page manager">%s<div class="chat"><div class="scroll chat-body">%s</div>%s%s</div></div>' % (
        bar("wsx", ""), body, strip("Chat") if box else "", composer(you=3, real=True, box=box))


def item(label, ctx, glyph, tone, actions):
    acts = "".join('<button class="button need-action" type="button" data-tone="%s">%s</button>' % (t, e(w)) for w, t in actions)
    return ('<li class="need"><div class="row-link"><span class="row-main row-open"><span class="row-label">%s</span><span class="row-context"><span class="row-glyph" data-tone="%s">%s</span>%s</span></span>'
            '<span class="row-actions">%s</span></div></li>' % (e(label), tone, icon(glyph), e(ctx), acts))


def band(text, kind="you"):
    return '<li class="reservoir-band" data-band="%s">%s</li>' % (kind, e(text))


def for_you():
    rows = (band("wsx › Fee table")
            + item("Which fee line goes first, setup or monthly?", "asks you · 16 min", "unchecked", "attention", [("Answer", "accent")])
            + item("Check #96: the invoice page shows the setup fee", "to check · passes if the page shows 3 lines", "pass", "attention", [("Pass", "accent"), ("Fail", "danger")])
            + band("wsx › trunk")
            + item("Approve land #97", "approval · changelog for 0.43", "unchecked", "attention", [("Approve", "accent"), ("Deny", "danger")])
            + band("studio-app › Voice")
            + item("Pause rule: 2 s or 3 s?", "asks you · 3 h", "unchecked", "attention", [("Answer", "accent")])
            + band("machine › trunk")
            + item("#12 failed: 1 of 2 checks", "dotfiles · the sync check failed", "fail", "danger", [("Open", "")]))
    return ('<div class="page manager"><header class="bar"><a class="icon-button" href="#" aria-label="Back">%s</a><div class="bar-title"><span class="bar-label">For you</span>'
            '<span class="bar-state">5 items</span></div></header><div class="scroll"><div class="reservoir inline"><ul class="reservoir-list" aria-label="For you">%s</ul></div></div></div>'
            % (icon("back"), rows))


def task_row(num, title, glyph, tone, now, inline):
    id_ = '<span class="row-id">#%d</span>' % num
    return ('<li class="need"><div class="row-link">%s<span class="row-main row-open"><span class="row-label">%s%s</span><span class="row-context">'
            '<span class="row-glyph" data-tone="%s">%s</span>%s</span></span></div></li>'
            % ("" if inline else '<span class="row-num">#%d</span>' % num, e(title), " " + id_ if inline else "", tone, icon(glyph), e(now)))


def tasks_list(after):
    you = [(96, "Fee table: add the setup fee line", "unchecked", "attention", "waits for your check"),
           (12, "Sync the shell profile", "fail", "danger", "failed · 1 of 2 checks")]
    moving = [(98, "Fee table: round to whole cents", "play", "working", "build 2/3"), (97, "Changelog for 0.43", "play", "working", "CI check-run 12 min")]
    waiting = [(99, "Release tag 0.43", "pause", "muted", "after #97")]
    rows = ""
    if after:
        rows += ('<li class="need"><a class="row-link row-wait" href="#"><span class="row-main row-open"><span class="row-label">2 wait for you</span>'
                 '<span class="row-context"><span class="row-glyph" data-tone="attention">%s</span>Open For you</span></span></a></li>' % icon("unchecked"))
    else:
        rows += band("For you") + "".join(task_row(*r, inline=False) for r in you)
    rows += band("Moving", "moving") + "".join(task_row(*r, inline=after) for r in moving)
    rows += band("Waiting", "waiting") + "".join(task_row(*r, inline=after) for r in waiting)
    return ('<div class="page manager"><header class="bar"><a class="icon-button" href="#" aria-label="Back">%s</a><div class="bar-title"><span class="bar-label">Tasks</span>'
            '<span class="bar-state">wsx</span></div></header><div class="scroll"><div class="reservoir%s"><ul class="reservoir-list" aria-label="Tasks">%s</ul></div></div></div>'
            % (icon("back"), " inline" if after else "", rows))


def page(file, body, light, label, extra=""):
    scheme = ' data-scheme="light"' if light else ""
    (HERE / file).write_text(
        '<!doctype html>\n<html lang="en"%s><head><meta charset="utf-8"><title>%s</title><meta name="viewport" content="width=device-width, initial-scale=1">'
        '<link rel="stylesheet" href="../../../crates/wsx-web/web/app.css"><link rel="stylesheet" href="theme.css"><link rel="stylesheet" href="mock.css">%s</head>'
        '<body data-theme="default"><div id="app"><div class="app"><main class="screen">%s</main></div></div>'
        '<script>if (innerWidth >= 900) { document.querySelector(".app").setAttribute("data-wide", ""); document.querySelectorAll(".tabbar").forEach((n) => n.remove()) }</script></body></html>\n' % (scheme, label, extra, body))


PAGES = {
    "home": (home(), "Home"),
    "chat-a": (chat_trunk("a"), "A chat, option A: thread strip above the composer"),
    "thread-a": (thread_page("a"), "A thread, option A"),
    "chat-b": (chat_trunk("b"), "A chat, option B: thread button in the composer row"),
    "thread-b": (thread_page("b"), "A thread, option B"),
    "thread-b-sheet": (thread_page("b", open_sheet=True), "A thread, option B, sheet open"),
    "thread-menu": (thread_page("a", menu=True), "A thread window, menu open"),
    "composer-before": (composer_page(""), "Desktop composer today"),
    "composer-a": (composer_page("a"), "Desktop composer, option A: one box, two rows"),
    "composer-b": (composer_page("b"), "Desktop composer, option B: one single-line box"),
    "foryou": (for_you(), "For you, all chats"),
    "tasks-before": (tasks_list(False), "Tasks list today"),
    "tasks-after": (tasks_list(True), "Tasks list, one For you list"),
}
for light in (False, True):
    for name, (body, label) in PAGES.items():
        page("%s-%s.html" % (name, "light" if light else "dark"), body, light, label)


# ---- Task 519: round corners and the composer without a band, before and after, side by side ----
# before = the real app.css; after = the same plus round.css, which is the build's CSS change written as overrides:
#   1. the three radius tokens retuned, the Home menu (tab bar: pill 2 x sheet, tab = sheet = 16 px) is the family's size;
#   2. the ad-hoc radii (half of key, a fraction of space, half-round bar ends) go to a token;
#   3. the composer rules of a phone (`.app:not([data-wide])`) lose that condition: one rule on every width, copied here from app.css.
APP = (HERE / "../../../crates/wsx-web/web/app.css").read_text()
APP_NOCOMMENT = re.sub(r"/\*.*?\*/", "", APP, flags=re.S)
PHONE_COMPOSER = [
    re.sub(r"\.app:not\(\[data-wide\]\) ?", ".app ", m.group(0)).replace("padding: var(--space-1) var(--space-4);", "padding: var(--space-1) var(--column-inset, var(--space-4));")
    for m in re.finditer(r"[^{}]*\.app:not\(\[data-wide\]\)[^{}]*\{[^{}]*\}", APP_NOCOMMENT)
    if re.search(r"composer|send-mark|send-text", m.group(0).split("{")[0])]
(HERE / "round.css").write_text(
    "/* Task 519, after: the build's CSS as overrides of app.css (see build.py). */\n"
    ":root { --radius-key: 12px; --radius-control: 16px; --radius-sheet: 16px; }\n"
    ".markdown :not(pre) > code, .state-chip, .task-chip, .markdown .task-chip { border-radius: var(--radius-key); }\n"
    ".usage-legend i, .usage-parts dt i { border-radius: var(--radius-key); }\n"
    "/* the composer: one rule on every width */\n" + "\n".join(" ".join(r.split()) for r in PHONE_COMPOSER) + "\n")
print("composer rules made one:", len(PHONE_COMPOSER))

TABS = {"Home": "M3 9.5 10 4l7 5.5V16H3Z", "Tasks": PATH["play"], "Projects": "M3 5h5l2 2h7v8H3Z", "Settings": "M4 6h12M4 10h12M4 14h12"}


def tabbar():
    def tab(name, now):
        d = TABS[name]
        count = ' data-count="3"' if name == "Tasks" else ""
        return ('<a href="#" aria-current="%s"><span class="tab-icon"%s><svg class="icon" viewBox="0 0 20 20" width="20" height="20" fill="none" stroke="currentColor" stroke-width="1.75" '
                'stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="%s" /></svg></span><span class="tab-label">%s</span></a>' % ("page" if now else "false", count, d, name))
    return '<nav class="tabbar" aria-label="Screens"><div class="tabbar-pill">%s</div></nav>' % "".join(tab(n, n == "Home") for n in TABS)


def chat_plain(menu=False):
    body = (bubble("out", "What is left before the 0.43 release?", time="14:01")
            + bubble("in", "Two things: the changelog (#97, working) and your check of #96. Nothing else blocks the tag.", "manager", "14:02")
            + note('<span class="chip">#94</span> landed \u00b7 2 of 2 checks')
            + bubble("out", "Start #99 after #97 lands, and tell me when the tag is ready.", time="14:05")
            + bubble("in", "Done: #99 waits for #97. I will write here when the tag is ready. See `wsx task show 99`.", "manager", "14:05"))
    return '<div class="page manager">%s<div class="chat"><div class="scroll chat-body">%s</div>%s</div></div>' % (
        bar("wsx", "", menu=menu, items=("Rename chat", "Close chat")), body, composer(you=3, real=True))


def grouped_tasks():
    html_ = tasks_list(False)
    # the For you band is one card: first and last row carry the data-group the client sets
    rows = html_.split('<li class="need">')
    out = [rows[0]]
    for i, r in enumerate(rows[1:], 1):
        group = {1: ' data-group="first"', 2: ' data-group="last"'}.get(i, "")
        out.append('<li class="need"%s>%s' % (group, r))
    return "".join(out)


def new_chat_sheet():
    return (home() + '<dialog class="sheet" open style="position:fixed;inset:auto;top:50%;left:50%;transform:translate(-50%,-50%);margin:0"><form>'
            '<h2 class="sheet-title">New chat</h2><label class="field">Project<select class="input"><option>wsx</option></select></label>'
            '<label class="field">Name<input class="input" placeholder="research"><span class="field-note">Lowercase letters, digits and dashes.</span></label>'
            '<label class="field">Goal (optional)<textarea class="input" rows="3" placeholder="What the chat is for"></textarea></label>'
            '<div class="actions sheet-actions"><button class="button" type="button">Cancel</button><button class="button button-primary" type="submit">Create</button></div></form></dialog>')


ROUND = {"home": (home() + tabbar(), "Home and its menu"), "chat": (chat_plain(), "A chat with the composer"),
         "chat-menu": (chat_plain(menu=True), "A chat, the more menu open"), "tasks": (grouped_tasks(), "The Tasks page"),
         "sheet": (new_chat_sheet(), "Home, the New chat sheet")}
WRAP = ('<!doctype html>\n<html lang="en"%s><head><meta charset="utf-8"><title>%s</title><style>body{margin:0;background:#888;font:600 14px system-ui;color:#fff}'
        '.pair{display:flex;gap:20px;padding:10px}.pair div{display:flex;flex-direction:column;gap:6px}iframe{border:0;width:%dpx;height:%dpx;background:#000}</style></head>'
        '<body><div class="pair"><div>Before (app.css today)<iframe src="x-%s-before-%s.html"></iframe></div><div>After (round corners, one composer)<iframe src="x-%s-after-%s.html"></iframe></div></div></body></html>\n')
for light in (False, True):
    tag = "light" if light else "dark"
    for name, (body, label) in ROUND.items():
        page("x-%s-before-%s.html" % (name, tag), body, light, label + ", before")
        page("x-%s-after-%s.html" % (name, tag), body, light, label + ", after", '<link rel="stylesheet" href="round.css">')
        for width, height in ((390, 844), (1280, 800)):
            (HERE / ("round-%s-%s-%d.html" % (name, tag, width))).write_text(WRAP % (' data-scheme="light"' if light else "", label, width, height, name, tag, name, tag))
