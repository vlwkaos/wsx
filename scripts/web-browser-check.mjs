#!/usr/bin/env node
// Real-browser check of the page: every screen and agent state at three sizes, driven
// over the Chrome DevTools Protocol. Called by `test-web-client.py --browser`.
//   tour <site-url> <code> <empty-site-url> <code> <sessions-json> <capture-dir> [slop-audit.js]
//   down <site-url> <code> <capture-dir> [slop-audit.js]     (wsxd is stopped)
// Each capture is saved with the slop audit of /ui-view-standards beside it.
import fs from "node:fs";
import path from "node:path";
import { launch, mobileAudit, pause } from "./web-cdp.mjs";

const [mode, siteUrl, code, ...rest] = process.argv.slice(2);
//   restart <site-url> <code> <capture-dir> <signal-dir> [slop-audit.js]   (the caller restarts the server)
const [emptyUrl, emptyCode, sessionsJson, captures, auditPath] = mode === "tour" ? rest
  : mode === "restart" ? [null, null, "{}", rest[0], rest[2]] : [null, null, "{}", ...rest];
const ids = JSON.parse(sessionsJson);
const { send, evaluate, until, viewport, capture, fail, cleanup, problems, go, text, count, key } = await launch({
  captures, prefix: "", timeoutSeconds: mode === "tour" ? 420 : 100,
});
const auditSource = auditPath ? fs.readFileSync(auditPath, "utf8") : null;
const mobileSource = mobileAudit().source;
const SIZES = [[320, 568, 2], [390, 844, 2], [1280, 800, 1]];
let size = SIZES[0];
const phone = () => size[0] < 600;

// One capture with its slop audit. A screen may not scroll sideways and may show at
// most one filled accent control.
async function shot(name, primary) {
  const label = `${name} at ${size[0]} px`;
  if (auditSource) {
    await evaluate(`globalThis.SLOP_AUDIT_PRIMARY = ${JSON.stringify(primary ?? null)}`);
    const audit = await evaluate(auditSource);
    fs.writeFileSync(path.join(captures, `${size[0]}-${name}.slop.json`), JSON.stringify(audit, null, 1));
    if (audit.horizontalOverflow) await fail(`${label} overflows sideways`);
    if (audit.primaryAction.filledInViewport > 1) await fail(`${label} shows ${audit.primaryAction.filledInViewport} filled accent controls: ${JSON.stringify(audit.primaryAction.filled)}`);
    if (primary && !audit.primaryAction.inFirstViewport) await fail(`${label}: the primary action is not in view`);
  }
  // On a phone: no sideways scroll, no field that makes iOS zoom, and no other field in the
  // way of iOS's previous and next arrows while the composer has the focus. The rest is printed.
  if (mobileSource && phone()) {
    await evaluate("globalThis.MOBILE_AUDIT_GUTTER = 16");
    const mobile = await evaluate(mobileSource);
    fs.writeFileSync(path.join(captures, `${size[0]}-${name}.mobile.json`), JSON.stringify(mobile, null, 1));
    if (mobile.horizontalOverflow) await fail(`${label}: the page scrolls sideways (mobile audit)`);
    if (mobile.zoomingFields.count) await fail(`${label}: a field makes iOS zoom: ${JSON.stringify(mobile.zoomingFields.items)}`);
    if (mobile.focusedField?.startsWith("textarea.composer-field") && mobile.otherFocusableFields?.count) {
      await fail(`${label}: other fields can take the focus from the composer: ${JSON.stringify(mobile.otherFocusableFields.items)}`);
    }
    console.log(`mobile audit ${name} at ${mobile.viewport.join("x")}: contentShare ${mobile.contentShare}, smallTargets ${mobile.smallTargets.count} ${JSON.stringify(mobile.smallTargets.items.map((item) => `${item.label || item.el} ${item.size.join("x")}`))}, tinyText ${mobile.tinyText.count}, outsideGutter ${mobile.outsideGutter.count}`);
  }
  await capture(`${size[0]}-${name}`);
}
// The composer enlarged three times, to judge its insets and the round Send.
async function zoom(name) {
  const box = await evaluate("(({ x, y, width, height }) => ({ x, y, width, height }))(document.querySelector('.composer').getBoundingClientRect())");
  await capture(`${size[0]}-${name}-zoom`, { ...box, scale: 3 });
}
// Layout rules of every screen: nothing past the right edge, no raw null, one scroll container
// (the wide layout's rail is a list of its own, and a tall question sheet is bounded: both may scroll).
async function sound(label) {
  const seen = await evaluate(`(() => {
    const free = (node) => node.closest('.crumbs, .terminal-keys, .row-slide, .code, .xterm, .code-block pre, .table-wrap, .diagram, .question-screen');
    const wide = [...document.querySelectorAll('#app *')].filter((node) => !free(node) && node.getBoundingClientRect().right > innerWidth + 0.5);
    const scrollers = [...document.querySelectorAll('#app *')].filter((node) => node.tagName !== 'TEXTAREA' && !free(node) && !node.closest('.rail, .question, .slash')
      && /auto|scroll/.test(getComputedStyle(node).overflowY) && node.scrollHeight > node.clientHeight + 1);
    return { wide: wide.slice(0, 3).map((node) => node.className || node.tagName), scrollers: scrollers.map((node) => node.className),
      raw: /\\b(null|undefined|NaN)\\b/.test(document.querySelector('#app').innerText), page: document.scrollingElement.scrollTop + document.scrollingElement.scrollLeft };
  })()`);
  const where = `${label} at ${size[0]} px`;
  if (seen.wide.length) await fail(`${where}: elements pass the right edge: ${seen.wide}`);
  if (seen.scrollers.length > 1) await fail(`${where}: more than one scroll container: ${seen.scrollers}`);
  if (seen.raw) await fail(`${where} shows a raw null`);
  if (seen.page) await fail(`${where}: the page itself scrolled`);
}
// A focused text field on a phone must not make the browser zoom or pan:
// iOS zooms when the field's font is below 16 px.
async function steadyOnFocus(selector, label) {
  const seen = await evaluate(`(() => { const field = document.querySelector(${JSON.stringify(selector)}); field.focus();
    return { focused: document.activeElement === field, font: parseFloat(getComputedStyle(field).fontSize),
      scale: visualViewport.scale, left: document.scrollingElement.scrollLeft, top: document.scrollingElement.scrollTop }; })()`);
  if (!seen.focused || (phone() && seen.font < 16) || seen.scale !== 1 || seen.left !== 0 || seen.top !== 0) {
    await fail(`${label} zooms or scrolls the page on focus at ${size[0]} px: ${JSON.stringify(seen)}`);
  }
}
// Type into the composer as an input event does.
const typeInto = (value) => evaluate(`(() => { const field = document.querySelector('.composer-field'); field.focus();
  field.value = ${JSON.stringify(value)}; field.dispatchEvent(new Event('input', { bubbles: true })); })()`);
// Paste into a text box: a ClipboardEvent that carries a DataTransfer, as a browser sends it.
// "image" is a small PNG named "image.png", as iOS and Chrome name a pasted screenshot; any
// other value is plain text. It returns false when the page cancelled the paste.
const pasteInto = (selector, what) => evaluate(`(async () => {
  const data = new DataTransfer();
  if (${JSON.stringify(what)} === 'image') {
    const canvas = Object.assign(document.createElement('canvas'), { width: 8, height: 8 });
    canvas.getContext('2d').fillRect(0, 0, 4, 4);
    data.items.add(new File([await new Promise((done) => canvas.toBlob(done, 'image/png'))], 'image.png', { type: 'image/png' }));
  } else data.setData('text/plain', ${JSON.stringify(what)});
  const field = document.querySelector(${JSON.stringify(selector)});
  field.focus();
  return field.dispatchEvent(new ClipboardEvent('paste', { clipboardData: data, bubbles: true, cancelable: true }));
})()`);
async function pair(url, pairingCode) {
  await send("Page.navigate", { url });
  await until("!!document.querySelector('#pair-code')", "pairing form did not render");
  await sound("pairing form");
  await steadyOnFocus("#pair-code", "the pairing code field");
  await steadyOnFocus("#pair-name", "the device name field");
  await evaluate(`(() => { const input = document.querySelector('#pair-code'); input.value = ${JSON.stringify(pairingCode)}; input.form.requestSubmit(); })()`);
  await until("!!document.querySelector('.app')", "the app did not open after pairing");
  if (await evaluate("document.cookie.includes('wsx_web')")) await fail("credential cookie is readable by scripts");
}
async function resize(next) {
  size = next;
  await viewport(...next);
  await pause(250);
}

// The page's root holds the app and nothing else: index.html's "Loading…" placeholder,
// the pairing form and the error page are gone once the app runs.
async function alone(label) {
  await until("!!document.querySelector('.app')", `${label}: the app did not open`);
  const roots = await evaluate("[...document.querySelector('#app').children].map((node) => node.tagName.toLowerCase() + '.' + node.className)");
  if (roots.join() !== "div.app" || (await count(".pair"))) await fail(`${label}: the page root holds ${JSON.stringify(roots)}`);
  await sound(label);
}

if (mode === "restart") {
  const signals = rest[1];
  await resize(SIZES[1]);
  await pair(siteUrl, code);
  await alone("after pairing");
  await evaluate("navigator.serviceWorker.ready.then(() => true)");
  await until("!!navigator.serviceWorker.controller", "the service worker does not control the page");
  await send("Page.reload");
  await alone("a reload of a paired page");
  await go("#/projects");
  await until("!!document.querySelector('.row-open')", "the projects did not open");
  await send("Page.reload");
  await alone("a reload on another screen");
  await shot("reload");
  // A draft in a chat, then the server comes back as another build (docs/web-client.md "Build").
  await go("#/");
  const finished = "[...document.querySelectorAll('.row')].find((row) => /^done/.test(row.querySelector('.row-state')?.textContent.trim() ?? ''))";
  await until(`!!${finished}`, "no finished session in the inbox");
  const chatLink = await evaluate(`${finished}.querySelector('a').getAttribute('href')`);
  await go(chatLink);
  await until("!!document.querySelector('.composer-field:not([disabled])')", "the finished session's composer did not open");
  await typeInto("typed before the new build");
  const buildA = await evaluate("new URL(document.querySelector('script[type=module]').src).pathname.split('/').at(-2)");
  fs.writeFileSync(path.join(signals, "request"), "restart\n");
  for (let attempt = 0; !fs.existsSync(path.join(signals, "done")); attempt++) {
    if (attempt > 600) await fail("the server was not restarted");
    await pause(100);
  }
  // The page says so once and does not reload by itself: the draft stays in the field.
  await until("!!document.querySelector('.build-pill')", "the page did not offer the new build", 300);
  if ((await evaluate("document.querySelector('.composer-field').value")) !== "typed before the new build") await fail("the offer of a new build took the draft");
  await sound("new build offered");
  await shot("new-build");
  const buildB = (await evaluate("fetch('api/session').then((r) => r.json())")).build.id;
  if (buildB === buildA) await fail(`the server came back as the same build ${buildA}`);
  await evaluate("document.querySelector('.build-pill').click()");
  await until(`new URL(document.querySelector('script[type=module]')?.src ?? location.href).pathname.includes('/${buildB}/') && !!document.querySelector('.app')`, "Reload did not load the new build", 300);
  if (await count(".build-pill")) await fail("the new build still offers a reload");
  if ((await evaluate("document.querySelector('.composer-field')?.value")) !== "typed before the new build") await fail("the draft did not survive the reload");
  await go("#/settings");
  await until("!!document.querySelector('.facts')", "Settings shows no version");
  const facts = await evaluate("Object.fromEntries([...document.querySelectorAll('.facts dt')].map((dt) => [dt.textContent, dt.nextElementSibling.textContent]))");
  const shown = facts["Page files"] ?? facts.Build;
  if (!/^\d+\.\d+\.\d+/.test(facts.wsx) || shown !== buildB || !facts["Server started"] || await count(".settings .actions .muted code")) await fail(`Settings does not show build ${buildB}: ${JSON.stringify(facts)}`);
  await shot("settings-version");
  // Leave no draft for the later checks.
  await go(chatLink);
  await until("!!document.querySelector('.composer-field:not([disabled])')", "the chat did not reopen");
  await typeInto("");
  await send("Page.reload");
  await alone("a reload after a server restart");
  await shot("reload-after-restart");
  for (const next of [SIZES[2]]) {
    await resize(next);
    await send("Page.reload");
    await alone("a wide reload after a server restart");
  }
  // The restart cuts the open event stream once; the page reconnects by itself. A request that
  // the open chat sends while the server is down is refused once (ERR_CONNECTION_REFUSED).
  const defects = problems.filter((problem) => !/status of 401/.test(problem) && !/ERR_INCOMPLETE_CHUNKED_ENCODING \S+\/api\/events(\?page=\S+)?$/.test(problem) && !/ERR_CONNECTION_REFUSED \S+\/api\/\S+$/.test(problem));
  if (defects.length) await fail(`console errors: ${defects.join(" | ")}`);
  console.log("paired page reloaded with its service worker, on two screens, after a server restart and wide: the app alone in the page root");
  await cleanup();
  process.exit(0);
}

if (mode === "down") {
  await resize(SIZES[1]);
  await pair(siteUrl, code);
  for (const next of SIZES) {
    await resize(next);
    await until("document.querySelector('#app').innerText.includes('wsx is not running on the host')", "the page does not say that wsx is stopped");
    await sound("daemon unavailable");
    await shot("daemon-down");
  }
  console.log("page with wsxd stopped: says so at 320, 390 and 1280 px");
  await cleanup();
  process.exit(0);
}

async function inbox() {
  await go("#/");
  await until("document.querySelectorAll('.row').length >= 8", "the inbox did not list the sessions");
  const labels = await text(".label");
  if (labels.join() !== "Needs you,Running,Finished,Sessions") await fail(`inbox sections are ${labels}`);
  const sections = await evaluate(`Object.fromEntries([...document.querySelectorAll('section')].map((section) => [section.getAttribute('aria-label'),
    [...section.querySelectorAll('.row')].map((row) => ({ work: [...row.querySelector('.row-work').children].map((node) => node.textContent).join(' '),
      session: row.querySelector('.row-context')?.textContent ?? null,
      state: row.querySelector('.row-state').textContent.trim().replace(/\\s+/g, ' '), activity: row.querySelector('.row-activity')?.textContent ?? null,
      clipped: row.querySelector('.row-context').scrollWidth > row.querySelector('.row-context').clientWidth,
      looks: [...row.querySelector('.row-work').children].map((node) => getComputedStyle(node).fontWeight + ' ' + getComputedStyle(node).color) })) ]))`);
  const states = (name) => sections[name].map((row) => row.state.replace(/ ?\d.*$/, "")).sort().join();
  if (states("Needs you") !== "error,exited,needs input,needs input") await fail(`Needs you shows ${JSON.stringify(sections["Needs you"])}`);
  const running = sections.Running[0];
  if (sections.Running.length !== 1 || running.activity !== "List the files" || !/^working ?\d+[smh]/.test(running.state)) await fail(`Running shows ${JSON.stringify(sections.Running)}`);
  // The work first, project › worktree, each with its own look; the session under it.
  if (running.work !== "design › main" || running.session !== "release notes · claude") await fail(`the running row reads ${JSON.stringify(running)}`);
  if (new Set(running.looks).size !== 3) await fail(`project, separator and worktree share a look: ${running.looks}`);
  if (sections.Sessions.find((row) => row.state === "idle")?.work !== "fix/login") await fail(`a row under its project heading repeats the project: ${JSON.stringify(sections.Sessions)}`);
  if (!sections.Finished.some((row) => /^done new$/.test(row.state))) await fail(`Finished marks nothing as new: ${JSON.stringify(sections.Finished)}`);
  // A shell is listed with its project, with no state group of its own.
  if (states("Sessions") !== "idle,open" || sections.Sessions[1].session !== "shell · shell") await fail(`Sessions shows ${JSON.stringify(sections.Sessions)}`);
  if (!(await count(`.scroll a[href="#/s/${ids.shell}/terminal"]`))) await fail("the inbox does not list the shell session on its terminal");
  if (!sections["Needs you"].some((row) => row.clipped)) await fail("the long session label is not truncated with an ellipsis");
  // Opening a finished session and coming back moves no row, though it is no longer new.
  if (size === SIZES[0]) {
    const order = () => evaluate("[...document.querySelectorAll('.scroll .row-link')].map((node) => node.getAttribute('href')).join()");
    const before = await order();
    const opened = await evaluate(`(() => { const link = [...document.querySelectorAll('section[aria-label=Finished] .row-link')].find((node) => node.querySelector('.new'));
      link.click(); return link.getAttribute('href'); })()`);
    await until(`location.hash === ${JSON.stringify(opened)} && !!document.querySelector('.session .bar-label')`, "the finished session did not open");
    await until(`${JSON.stringify(opened.split("/")[2])} in JSON.parse(localStorage.getItem('wsx-seen') ?? '{}')`, "opening the chat did not record the session as seen");
    await evaluate("document.querySelector('.bar a.icon-button').click()");
    await until("location.hash === '#/' && document.querySelectorAll('.row').length >= 8", "Back did not return to the inbox");
    const after = await order();
    if (after !== before) await fail(`rows moved after a finished session was seen: ${before} then ${after}`);
    if (await evaluate(`!!document.querySelector('.scroll .row-link[href="${opened}"] .new')`)) await fail("the seen session is still new");
  }
  if (!phone() && Math.round(await evaluate("document.querySelector('.rail').getBoundingClientRect().width")) !== 300) await fail("the rail is not 300 px wide");
  if ((await text(".group-name")).join() !== "design" || !(await count(".group-head .new-session"))) await fail("the Sessions group lacks its project or its New session action");
  // A touch slide to the left shows Remove at the row's right edge; it snaps back closed.
  // The removal itself is in test-web-client.py ("session remove").
  if (phone()) {
    const removeBox = () => evaluate(`(() => { const row = document.querySelector('.scroll .row-slide'); const r = row.querySelector('.row-remove').getBoundingClientRect();
      return { scrolled: Math.round(row.scrollLeft), left: Math.round(r.left), right: Math.round(r.right), height: Math.round(r.height), name: row.querySelector(".row-remove").getAttribute("aria-label"), filled: row.hasAttribute("data-open") }; })()`);
    const closed = await removeBox();
    if (closed.scrolled !== 0 || closed.left < size[0] || closed.filled) await fail(`Remove shows before a slide: ${JSON.stringify(closed)}`);
    const start = await evaluate("(() => { const r = document.querySelector('.scroll .row-slide').getBoundingClientRect(); return [r.right - 40, r.top + r.height / 2]; })()");
    await send("Input.synthesizeScrollGesture", { x: start[0], y: start[1], xDistance: -160, yDistance: 0, gestureSourceType: "touch", speed: 800 });
    await until("document.querySelector('.scroll .row-slide').scrollLeft > 0", "a slide did not move the row");
    await pause(400);
    const open = await removeBox();
    if (open.right !== size[0] || open.height < 44 || !open.filled || !/^Remove session /.test(open.name)) await fail(`a slide did not show Remove at the edge: ${JSON.stringify(open)}`);
    await shot("inbox-remove");
    await evaluate("document.querySelector('.scroll .row-slide').scrollTo({ left: 0 })");
  }
  // The elapsed time of a working session counts on the page.
  const before = await text("section[aria-label=Running] .time");
  await pause(1200);
  if ((await text("section[aria-label=Running] .time")).join() === before.join()) await fail("the elapsed time does not count");
  if (phone()) {
    if ((await text(".tabbar a")).join() !== "Inbox,Projects,Settings" || (await text(".tabbar a[aria-current=page]")).join() !== "Inbox") await fail("the tab bar is wrong");
    const bar = await evaluate("(() => { const r = document.querySelector('.tabbar').getBoundingClientRect(); return [r.bottom, innerHeight, r.height]; })()");
    if (Math.abs(bar[0] - bar[1]) > 1 || bar[2] < 44) await fail(`the tab bar is not at the bottom edge: ${bar}`);
  } else if ((await count(".tabbar")) || !(await count(".rail")) || (await text(".placeholder")).join() !== "Choose a session.") {
    await fail("the wide layout lacks its rail or shows a tab bar");
  }
  await sound("inbox");
  await shot("inbox");
}

async function openSession(role, tab = "chat") {
  await go(`#/s/${ids[role]}/${tab}`);
  await until("!!document.querySelector('.session .bar-label')", `the ${role} session did not open`);
  if (phone() && (await count(".tabbar"))) await fail("the tab bar shows inside a session");
  const bar = await evaluate("document.querySelector('.bar').getBoundingClientRect().height");
  if (bar > 48) await fail(`the session top row is ${bar} px high`);
}

async function chats() {
  // Working: the running tool is in the working row only, with the elapsed time and Stop;
  // the step list holds the finished steps; a message waits.
  await openSession("working");
  // ^ The top row's model comes from the chat's data one render after the working row.
  await until("!!document.querySelector('.working') && !!document.querySelector('.queued') && !!document.querySelector('.bar-model')", "the working row, the queue or the model did not show");
  const working = await evaluate(`({ state: [...document.querySelector('.bar-state').childNodes].find((node) => node.nodeType === 3).textContent, model: document.querySelector('.bar-model')?.textContent ?? '', activity: document.querySelector('.working-activity').textContent,
    live: [...document.querySelectorAll('.steps[data-live] .step')].map((step) => step.textContent.trim().replace(/\\s+/g, ' ')),
    send: document.querySelector('.composer .button-primary').textContent, stop: [...document.querySelectorAll('.working button')].map((b) => b.textContent).join(),
    queued: document.querySelector('.queued-text').textContent, switch: [...document.querySelectorAll('.switch a')].map((a) => a.textContent.trim() + (a.getAttribute('aria-current') ? '*' : '')).join(),
    pulse: getComputedStyle(document.querySelector('.pulse')).animationName })`);
  if (working.state !== "working" || working.model !== " · claude-opus-5-5" || working.activity !== "List the files" || working.send !== "Queue" || working.stop !== "Stop"
    || working.queued !== "Also mention the new terminal in the notes" || working.switch !== "Chat*,Terminal"
    || working.live.join() !== "Thinking" || working.pulse !== "pulse") {
    await fail(`the working chat is wrong: ${JSON.stringify(working)}`);
  }
  // One column: the user's message ends, and the composer's button ends, where the agent's
  // text column ends; the button is as high as any control.
  const column = await evaluate(`(() => { const edge = (selector, side) => Math.round(document.querySelector(selector).getBoundingClientRect()[side]);
    const log = document.querySelector('.log'); const style = getComputedStyle(log);
    return { left: Math.round(log.getBoundingClientRect().left + parseFloat(style.paddingLeft)), right: Math.round(log.getBoundingClientRect().right - parseFloat(style.paddingRight)),
      steps: edge('.steps', 'left'), user: edge('.message-user', 'right'), send: edge('.composer .button', 'right'), field: edge('.composer-field', 'left'), queued: edge('.queued-text', 'left'),
      height: document.querySelector('.composer .button').getBoundingClientRect().height, control: document.querySelector('.composer-field').getBoundingClientRect().height,
      radius: [getComputedStyle(document.querySelector('.composer .button')).borderRadius, getComputedStyle(document.querySelector('.message-user')).borderRadius],
      chars: Math.round((log.clientWidth - parseFloat(style.paddingLeft) - parseFloat(style.paddingRight)) / parseFloat(getComputedStyle(document.documentElement).fontSize || 16)) }; })()`);
  // On a phone Send sits inside the composer card, one border and one space-1 from its edge.
  if (column.steps !== column.left || column.user !== column.right || column.queued !== column.left
    || (phone() ? column.right - column.send < 0 || column.right - column.send > 6 : column.send !== column.right)
    || column.height !== 44 || column.radius[0] !== column.radius[1] || column.field < column.left) {
    await fail(`the chat is not one column, or the send button is not a 44 px control: ${JSON.stringify(column)}`);
  }
  console.log(`chat column at ${size[0]} px: ${JSON.stringify(column)}`);
  await sound("working chat");
  // The working row is the end of the log: the same clear gap follows it.
  await evaluate("document.querySelector('.log').scrollTop = document.querySelector('.log').scrollHeight");
  const workingGap = await evaluate("document.querySelector('.log-wrap').getBoundingClientRect().bottom - document.querySelector('.working').getBoundingClientRect().bottom");
  if (workingGap < 63.5) await fail(`the working row is ${workingGap} px from the end of the log`);
  await shot("chat-working", ".composer .button-primary");

  // A phone gives the text field its own line, then attach and Send; a wide window has one line.
  const composer = async () => evaluate(`(() => { const box = (selector) => document.querySelector(selector).getBoundingClientRect();
    const row = box('.composer-row'), field = box('.composer-field'), send = box('.composer .button'), attach = box('.composer-row > .icon-button');
    return { row: row.width, field: field.width, below: Math.round(send.top - field.bottom), circleBelow: Math.round(box('.send-mark').top - field.bottom), line: Math.abs((attach.top + attach.bottom) / 2 - (send.top + send.bottom) / 2),
      attach: [attach.width, attach.height], send: [send.width, send.height],
      height: Math.round(box('.composer').height), round: getComputedStyle(document.querySelector('.send-mark')).borderRadius,
      // The card's inner insets: the field's text, the paperclip, and the circle's right and bottom gaps.
      ...(() => { const edge = parseFloat(getComputedStyle(document.querySelector('.composer-row')).borderLeftWidth), mark = box('.send-mark'), clip = box('.composer-row > .icon-button svg');
        const text = field.left + parseFloat(getComputedStyle(document.querySelector('.composer-field')).paddingLeft);
        return { insets: [text - row.left, clip.left - row.left, field.top - row.top, row.right - mark.right, row.bottom - mark.bottom].map((gap) => Math.round(gap - edge)) }; })() }; })()`);
  const layout = await composer();
  // On a phone the row is the composer card: one 8 px inset on every side (the field's text,
  // the paperclip, the circle's right and bottom gaps), and every hit area is 44 by 44.
  if (phone() ? layout.insets.some((gap) => gap !== 8) || layout.circleBelow < 0 || layout.line > 1
      || Math.min(...layout.attach, ...layout.send) < 44 || layout.round !== "50%"
    : layout.below >= 0) {
    await fail(`the composer layout is wrong at ${size[0]} px: ${JSON.stringify(layout)}`);
  }
  if (size === SIZES[1]) {
    await openSession("idle");
    await until("!!document.querySelector('.composer-field:not([disabled])') && !!document.querySelector('.log .message, .log .empty')", "the idle composer is not ready");
    const empty = await composer();
    // Compact like the reference: an empty composer of at most 87 px.
    if (empty.height > 87) await fail(`the empty phone composer is ${empty.height} px high`);
    console.log(`empty phone composer at 390 px: ${empty.height} px, attach ${empty.attach.join("x")}, send ${empty.send.join("x")}`);
    // The field is short; a tap anywhere on the card outside its buttons focuses it.
    await evaluate("document.activeElement?.blur(); document.querySelector('.composer-row').dispatchEvent(new PointerEvent('pointerdown', { bubbles: true, cancelable: true }))");
    if (!(await evaluate("document.activeElement === document.querySelector('.composer-field')"))) await fail("a tap on the composer card did not focus the field");
    await shot("composer");
    await zoom("composer");
    await typeInto("Check the login redirect on Safari.\nThen the cookie settings.\nThen write the notes.");
    const typed = await composer();
    if (typed.insets.some((gap) => gap !== 8) || typed.circleBelow < 0 || typed.line > 1) await fail(`three lines break the composer: ${JSON.stringify(typed)}`);
    await shot("composer-text");
    await zoom("composer-text");
    await typeInto("");
  }

  // A long conversation: opens at its end; scrolled up, a jump button shows.
  await openSession("long");
  await until("document.querySelectorAll('.message').length > 40", "the long conversation did not load");
  const long = await evaluate(`(() => { const log = document.querySelector('.log');
    return { end: log.scrollHeight - log.scrollTop - log.clientHeight, wide: log.scrollWidth - log.clientWidth,
      steps: [...document.querySelectorAll('details.steps > summary')].slice(-4).map((node) => node.textContent.trim()),
      user: Math.round(document.querySelector('.message-user').getBoundingClientRect().width / log.clientWidth * 100),
      jump: !!document.querySelector('.jump'), labels: document.querySelector('.log').innerText.includes('Agent') }; })()`);
  if (long.end > 2 || long.wide > 0 || long.jump || long.labels) await fail(`the long chat does not open at its end, or a 300-character word widens it: ${JSON.stringify(long)}`);
  if (!long.steps.every((line) => /^\d+ steps?: [A-Z]/.test(line)) || long.user > 86) await fail(`steps or the user message are wrong: ${JSON.stringify(long)}`);
  await evaluate("(() => { const log = document.querySelector('.log'); log.scrollTop = log.scrollHeight / 2; log.dispatchEvent(new Event('scroll')); })()");
  await until("!!document.querySelector('.jump')", "no jump button after scrolling up");
  // The jump button sits inside the conversation, at the height of the end gap.
  const jump = await evaluate(`(() => { const wrap = document.querySelector('.log-wrap').getBoundingClientRect(); const jump = document.querySelector('.jump').getBoundingClientRect();
    return { above: wrap.bottom - jump.bottom, height: jump.height, gap: parseFloat(getComputedStyle(document.querySelector('.log')).paddingBottom) }; })()`);
  if (jump.above < 0 || jump.above + jump.height > jump.gap) await fail(`the jump button is not inside the end gap: ${JSON.stringify(jump)}`);
  await sound("long chat scrolled up");
  await shot("chat-long-scrolled");
  await evaluate("document.querySelector('.jump').click()");
  await until("!document.querySelector('.jump') && (() => { const log = document.querySelector('.log'); return log.scrollHeight - log.scrollTop - log.clientHeight < 2; })()", "Jump to latest did not go to the end");
  // A disclosure lists each step with its state word; a failed call says so.
  await evaluate("[...document.querySelectorAll('details.steps')].forEach((node) => { node.open = true; })");
  if (!(await count(".step-state[data-tone=danger]")) || !(await count(".step-title"))) await fail("an opened steps line does not list its steps, or a failed call is not marked");

  // Blocked: the agent's own dialog stands where the composer was, with a button per
  // choice. Nothing is preselected or filled: an approval is a deliberate tap.
  for (const [role, capture, title, choices] of [
    ["blocked", "chat-question", "Which option should I take for the login redirect?", ["1Keep the session cookie", "2Use a bearer token", "3Ask the server on each request", "4Type something else"]],
    ["permission", "chat-permission", "Remove the build folder", ["1Yes", "2Yes, and don't ask again for rm commands in this project", "3No, and tell Claude what to do differently (esc)"]],
  ]) {
    await openSession(role);
    await until("!!document.querySelector('.question .question-option')", `the question of the ${role} session did not show`);
    const asked = await evaluate(`(() => { const sheet = document.querySelector('.question'); const screen = sheet.querySelector('.question-screen');
      const box = (node) => node.getBoundingClientRect(); const inner = sheet.clientWidth - parseFloat(getComputedStyle(sheet).paddingLeft) - parseFloat(getComputedStyle(sheet).paddingRight);
      const options = [...sheet.querySelectorAll('.question-option')];
      return { title: sheet.querySelector('.question-title').textContent, choices: options.map((option) => option.textContent.trim().replace(/\\s+/g, ' ')),
        heights: options.map((option) => box(option).height), widths: options.map((option) => Math.round(box(option).width - inner)),
        filled: document.querySelectorAll('.button-primary').length, focused: sheet.contains(document.activeElement) && document.activeElement !== screen,
        keys: [...sheet.querySelectorAll('.question-keycap')].map((key) => key.getAttribute('aria-label')).join(), capHeights: [...sheet.querySelectorAll('.question-keycap')].map((key) => box(key).height),
        actions: [...sheet.querySelectorAll('.question-keys .link-action')].map((node) => node.textContent).join(), terminal: sheet.querySelector('.question-keys a').getAttribute('href'),
        composer: !!document.querySelector('.composer, .attention'), bottom: Math.round(innerHeight - box(sheet).bottom), log: Math.round(box(document.querySelector('.log')).height),
        screen: { rows: screen.textContent.split('\\n').length, font: getComputedStyle(screen).fontFamily.split(',')[0], space: getComputedStyle(screen).whiteSpace,
          wide: screen.scrollWidth > screen.clientWidth, cursor: screen.textContent.includes('❯ 1.'), high: box(screen).height },
        under: box(document.querySelector('.bar-state')).top >= box(document.querySelector('.bar-label')).bottom - 1, state: [...document.querySelector('.bar-state').childNodes].find((node) => node.nodeType === 3).textContent }; })()`);
    if (asked.title !== title || asked.choices.join("|") !== choices.join("|") || asked.heights.some((height) => height < 44) || asked.widths.some((width) => width !== 0)
      || asked.filled || asked.focused || asked.composer || asked.keys !== "Enter,Escape,Up,Down" || asked.actions !== "Type an answer,Open terminal" || !asked.terminal.endsWith("/terminal")
      || (phone() && asked.capHeights.some((height) => height < 44))) {
      await fail(`the question sheet of ${role} is wrong: ${JSON.stringify(asked)}`);
    }
    // The agent's rows keep their columns: the block scrolls inside itself, box drawing does not wrap.
    if (!/JetBrains Mono/.test(asked.screen.font) || asked.screen.space !== "pre" || !asked.screen.cursor || asked.screen.high > 180 || (size[0] === 320 && !asked.screen.wide)) await fail(`the question's screen block is wrong: ${JSON.stringify(asked.screen)}`);
    if (asked.bottom !== 0 || asked.log < 60 || asked.state !== "needs input" || (phone() && !asked.under)) await fail(`the question sheet is not anchored under a visible conversation: ${JSON.stringify(asked)}`);
    await sound(`question of ${role}`);
    await shot(capture);
  }
  // The free-text field shows only on demand.
  if (await count(".question-text")) await fail("the answer field shows before it is asked for");
  await evaluate("[...document.querySelectorAll('.question-keys .link-action')].find((node) => node.textContent === 'Type an answer').click()");
  await until("document.activeElement?.matches('.question-text input')", "Type an answer did not open a focused field");
  await steadyOnFocus(".question-text input", "the answer field");
  await sound("question with the answer field");
  await shot("chat-permission-typing");

  // Empty: one sentence, and the composer ready.
  await openSession("idle");
  await until("!!document.querySelector('.composer-field')", "the idle chat did not show");
  if (size !== SIZES[2] || !(await count(".message"))) {
    await until("!!document.querySelector('.log .empty')", "the empty chat does not say so");
    if (!(await text(".log .empty")).join().startsWith("No messages yet")) await fail("the empty chat does not say so");
  }
  if ((await text(".composer .button-primary")).join() !== "Send") await fail("the send button of an idle agent does not read Send");
  await sound("empty chat");
  await shot("chat-empty", ".composer .button-primary");
  await steadyOnFocus(".composer-field", "the chat field");
  await send("Input.insertText", { text: "line one" });
  const value = () => evaluate("document.querySelector('.composer-field').value");
  if (phone()) {
    // A touch keyboard's Enter is a newline; nothing is sent.
    await key("Enter", 13, { text: "\r" });
    if ((await value()) !== "line one\n" || (await count(".message"))) await fail(`Enter on a touch device did not add a newline: ${JSON.stringify(await value())}`);
    await evaluate("(() => { const field = document.querySelector('.composer-field'); field.value = ''; field.dispatchEvent(new Event('input', { bubbles: true })); })()");
  } else {
    // A hardware keyboard: Shift+Enter is a newline, a key of an IME composition never sends, Enter sends.
    await key("Enter", 13, { text: "\r", modifiers: 8 });
    if ((await value()) !== "line one\n") await fail(`Shift+Enter did not add a newline: ${JSON.stringify(await value())}`);
    await key("Backspace", 8);
    await evaluate("document.querySelector('.composer-field').dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', isComposing: true, bubbles: true, cancelable: true }))");
    await pause(200);
    if ((await value()) !== "line one" || (await count(".message"))) await fail("Enter during an IME composition sent the message");
    await key("Enter", 13, { text: "\r" });
    await until("[...document.querySelectorAll('.message:not(.message-user) .markdown')].some((node) => node.textContent === 'echo: line one')", "Enter did not send, or the reply did not arrive");
    if ((await value()) !== "") await fail("the field kept its text after sending");
  }
}

// An agent's answer is markdown: built as nodes from the parser's tokens, never as HTML.
async function markdown() {
  await openSession("markdown");
  await until("!!document.querySelector('.markdown table') && !!document.querySelector('.code-block .tok-keyword')", "the markdown answer did not render");
  const seen = await evaluate(`(() => { const root = document.querySelector('.message:not(.message-user) .markdown');
    const all = (selector) => [...root.querySelectorAll(selector)];
    const size = (selector) => parseFloat(getComputedStyle(root.querySelector(selector)).fontSize);
    const pre = root.querySelector('.code-block pre'); const wrap = root.querySelector('.table-wrap');
    return { h1: root.querySelector('h1').textContent, sizes: [size('h1'), size('h2'), size('h3'), size('h4'), parseFloat(getComputedStyle(document.documentElement).getPropertyValue('--font-size-title'))],
      marks: ['strong', 'em', 's', 'blockquote', 'ol ul ol', 'hr'].map((selector) => all(selector).length),
      starts: all('ol').map((list) => list.start), tasks: all('li.task input').map((box) => [box.checked, box.disabled]),
      links: all('a').map((link) => [link.getAttribute('href'), link.target, link.rel]),
      live: all('img, b, script, iframe, [onerror], [style]').length, alt: all('.image-alt').map((node) => node.textContent),
      raw: root.innerText.includes('<b>Raw HTML stays text</b>') && root.innerText.includes('[script link](javascript:alert(1))') && root.innerText.includes('a local link stay'),
      code: { font: getComputedStyle(pre.querySelector('code')).fontFamily.split(',')[0], scrolls: pre.scrollWidth > pre.clientWidth, language: root.querySelector('.code-language').textContent,
        radius: getComputedStyle(root.querySelector('.code-block')).borderRadius, copy: all('.code-head button').map((button) => button.textContent).join() },
      table: { head: all('th').map((cell) => cell.textContent).join(), sticky: getComputedStyle(root.querySelector('th')).position, scrolls: wrap.scrollWidth > wrap.clientWidth,
        right: getComputedStyle(all('td')[2]).textAlign },
      user: document.querySelector('.message-user .prose').textContent, plain: !document.querySelector('.message-user .markdown'),
      remote: performance.getEntriesByType('resource').filter((entry) => !entry.name.startsWith(location.origin)).map((entry) => entry.name) }; })()`);
  const [h1, h2, h3, h4, title] = seen.sizes;
  if (seen.h1 !== "Release plan" || !(title > h1 && h1 > h2 && h2 > h3 && h3 > h4) || seen.marks.some((count) => !count)) await fail(`markdown headings or marks are wrong: ${JSON.stringify(seen)}`);
  if (seen.starts.join() !== "1,1,1" && seen.starts.join() !== "1,1") await fail(`ordered lists start at ${seen.starts}`);
  if (JSON.stringify(seen.tasks) !== "[[true,true],[false,true]]") await fail(`task marks are ${JSON.stringify(seen.tasks)}`);
  // Only http, https and mailto become links; a link opens in a new tab and tells the target nothing.
  if (seen.links.length !== 3 || !seen.links.every(([href, target, rel]) => /^https:\/\/example\.com\//.test(href) && target === "_blank" && rel === "noopener noreferrer")) await fail(`links are ${JSON.stringify(seen.links)}`);
  if (seen.live || !seen.raw || seen.alt.join() !== "A remote picture" || seen.remote.length) await fail(`a message produced live markup or a remote request: ${JSON.stringify(seen)}`);
  if (!/JetBrains Mono/.test(seen.code.font) || !seen.code.scrolls || seen.code.language !== "rust" || seen.code.copy !== "Copy,Copy") await fail(`the code block is wrong: ${JSON.stringify(seen.code)}`);
  if (seen.table.head !== "Part,Change,Files,Tests,Bytes,Status" || seen.table.sticky !== "sticky" || seen.table.right !== "right" || (phone() && !seen.table.scrolls)) await fail(`the table is wrong: ${JSON.stringify(seen.table)}`);
  if (seen.user !== "say markdown.md" || !seen.plain) await fail("the person's message is not plain text");
  // Prose keeps its reading width; a table or a code block may take the width of the
  // conversation minus the gutter, centered on the reading column, and no wider.
  const blocks = await evaluate(`(() => { const box = (node) => node.getBoundingClientRect();
    const root = document.querySelector('.message:not(.message-user) .markdown'); const log = box(document.querySelector('.log-wrap'));
    const column = box(root); const gutter = parseFloat(getComputedStyle(document.documentElement).getPropertyValue('--space-4'));
    const of = (node) => { const b = box(node); return { left: b.left, right: b.right, width: b.width }; };
    return { column: of(root), inner: [log.left + gutter, log.right - gutter], paragraph: of(root.querySelector('p')),
      code: [...root.querySelectorAll(':scope > .code-block')].map(of), table: of(root.querySelector(':scope > .table-wrap')) }; })()`);
  const centered = (block) => Math.abs((block.left + block.right) / 2 - (blocks.column.left + blocks.column.right) / 2) <= 1;
  const inside = (block) => block.left >= blocks.inner[0] - 1 && block.right <= blocks.inner[1] + 1;
  const [longCode, plainCode] = blocks.code;
  if (blocks.paragraph.width > blocks.column.width + 1 || ![longCode, plainCode, blocks.table].every((block) => inside(block) && centered(block) && block.width >= blocks.column.width - 1)
    || Math.abs(longCode.width - (blocks.inner[1] - blocks.inner[0])) > 1 || Math.abs(plainCode.width - blocks.column.width) > 1
    || (!phone() && longCode.width <= blocks.column.width + 100)) {
    await fail(`wide blocks do not use the conversation's width: ${JSON.stringify(blocks)}`);
  }
  // The end of the conversation stands clear of the composer.
  await evaluate("document.querySelector('.log').scrollTop = document.querySelector('.log').scrollHeight");
  await until("!document.querySelector('.jump')", "the log did not reach its end");
  const gap = await evaluate(`document.querySelector('.composer').getBoundingClientRect().top
    - [...document.querySelectorAll('.log > *')].at(-1).getBoundingClientRect().bottom`);
  if (gap < 63.5) await fail(`the last message is ${gap} px from the composer`);
  await sound("markdown answer at its end");
  await shot("chat-end");
  await evaluate("document.querySelector('.log').scrollTop = 0");
  await sound("markdown answer");
  await shot("chat-markdown");
  await evaluate("document.querySelector('.table-wrap').scrollIntoView({ block: 'center' })");
  await sound("markdown table");
  await shot("chat-markdown-table");
  // Copy says what happened. Headless Chrome may refuse the clipboard; both answers are words.
  await evaluate("document.querySelector('.code-head button').click()");
  await until("/^(Copied|Copy failed)$/.test(document.querySelector('.code-head button').textContent)", "Copy did not answer");
}

// A compacted conversation: its summary is marked as a summary and shown whole, without
// Claude's fixed words around it.
async function summary(light = false) {
  await openSession("compacted");
  await until("!!document.querySelector('.summary .markdown h2') && document.querySelectorAll('.message').length === 2", "the compacted conversation did not render");
  const seen = await evaluate(`(() => { const body = document.querySelector('.summary-body');
    return { first: document.querySelector('.log > :not(.note)').className, label: document.querySelector('.summary-label').textContent,
      heading: body.querySelector('h2').textContent, last: [...body.querySelectorAll('li')].at(-1)?.textContent,
      buttons: document.querySelectorAll('.summary button').length, text: body.textContent }; })()`);
  if (seen.first !== "summary" || seen.label !== "Summary of the earlier conversation" || seen.heading !== "Goal"
    || seen.last !== "Step 12 of the fix is done." || seen.buttons || /continued from a previous|If you need specific/.test(seen.text)) {
    await fail(`the summary is not shown whole: ${JSON.stringify(seen)}`);
  }
  await sound("compacted conversation");
  await shot(light ? "chat-summary-light" : "chat-summary");
}

// A mermaid block is drawn by the lazily loaded library and shown as an image; the page's
// CSP stays as it is, and a block that does not parse shows its source.
async function diagrams(light = false) {
  await openSession("diagrams");
  await until("document.querySelectorAll('.diagram img').length === 2 && !!document.querySelector('.diagram-failed .code-block')", "the diagrams did not draw");
  const seen = await evaluate(`(async () => { const images = [...document.querySelectorAll('.diagram img')];
    await Promise.all(images.map((image) => image.decode()));
    const frame = document.querySelector('.diagram-scroll');
    const cache = await caches.open((await caches.keys())[0]);
    return { sources: images.map((image) => image.src.slice(0, 5)), natural: images.map((image) => [image.naturalWidth, image.naturalHeight]),
      inline: document.querySelectorAll('.markdown svg, .markdown style, .markdown [style]').length,
      shown: images.map((image) => Math.round(image.getBoundingClientRect().width)), ratio: images.map((image) => image.getBoundingClientRect().width / image.getBoundingClientRect().height),
      column: frame.clientWidth, more: [...document.querySelectorAll('.diagram-scroll')].map((node) => node.dataset.more ?? ''),
      mask: getComputedStyle(frame).maskImage || getComputedStyle(frame).webkitMaskImage, failed: document.querySelector('.diagram-failed').textContent.includes('Diagram could not be drawn'),
      source: document.querySelector('.diagram-failed pre').textContent, left: document.querySelectorAll('body > :not(#app):not(noscript):not(script)').length,
      sheets: document.adoptedStyleSheets.length, kept: !!(await cache.match(new URL('mermaid-11.17.2.js', document.querySelector('script[type=module]').src).href)),
      shell: (await cache.keys()).length }; })()`);
  if (seen.sources.join() !== "blob:,blob:" || seen.natural.some(([width, height]) => width < 200 || height < 100) || seen.inline) await fail(`a diagram is not a drawn image: ${JSON.stringify(seen)}`);
  // A diagram's box is the conversation's width minus the gutter, wider than the reading column on a wide screen.
  const box = await evaluate(`(() => { const gutter = parseFloat(getComputedStyle(document.documentElement).getPropertyValue('--space-4'));
    return { frame: document.querySelector('.diagram-scroll').getBoundingClientRect().width, inner: document.querySelector('.log-wrap').getBoundingClientRect().width - 2 * gutter,
      column: document.querySelector('.message:not(.message-user) .markdown').getBoundingClientRect().width }; })()`);
  if (Math.abs(box.frame - box.inner) > 1 || (!phone() && box.frame <= box.column + 100)) await fail(`the diagram box is not the conversation's width: ${JSON.stringify(box)}`);
  // The whole diagram shows first: scaled down to the column, never up, with its shape kept.
  // Only where its text would fall under 10 px does it stay wider and scroll, with a fade
  // on the side that holds more.
  for (const [index, [width, height]] of seen.natural.entries()) {
    const least = Math.round(width * 10 / 15);
    const expected = Math.min(width, Math.max(seen.column, least));
    const wide = expected > seen.column;
    if (Math.abs(seen.shown[index] - expected) > 1 || Math.abs(seen.ratio[index] - width / height) > 0.02 || seen.more[index] !== (wide ? "end" : "")
      || (wide && index === 0 && !/gradient/.test(seen.mask))) {
      await fail(`diagram ${index} is not fitted to the column: ${JSON.stringify(seen)}`);
    }
  }
  if (!seen.failed || !seen.source.includes("A -->") || seen.left || seen.sheets) await fail(`the broken diagram or the page after drawing is wrong: ${JSON.stringify(seen)}`);
  if (!seen.kept) await fail("the service worker did not keep the diagram library after its first use");
  // The diagrams drew after the conversation showed at its end: it is still at its end
  // (docs/web-client-chat.md "Following the end").
  await pause(100);
  const end = await evaluate("(() => { const log = document.querySelector('.log'); return log.scrollHeight - log.scrollTop - log.clientHeight; })()");
  if (end > 2) await fail(`the conversation is ${end} px above its end after its diagrams drew`);
  await sound("diagrams");
  await evaluate("document.querySelector('.log').scrollTop = 0");
  await shot(light ? "chat-diagrams-light" : "chat-diagrams");
  await evaluate("document.querySelector('.diagram-failed').scrollIntoView({ block: 'end' })");
  await shot(light ? "chat-diagram-broken-light" : "chat-diagram-broken");
  if (light) return;
  // Full screen: opened by a tap, zoomed by a pinch (touch) or the buttons, closed by Escape.
  await evaluate("document.querySelector('.diagram-open').click()");
  await until("!!document.querySelector('dialog.viewer[open] img')", "the full-screen diagram did not open");
  const width = () => evaluate("document.querySelector('.viewer img').getBoundingClientRect().width");
  const fitted = await width();
  if (fitted > size[0] + 0.5) await fail(`the full-screen diagram does not fit at first: ${fitted}`);
  if (phone()) {
    const points = (gap) => [{ x: size[0] / 2 - gap, y: 300, id: 1 }, { x: size[0] / 2 + gap, y: 300, id: 2 }];
    await send("Input.dispatchTouchEvent", { type: "touchStart", touchPoints: points(30) });
    await send("Input.dispatchTouchEvent", { type: "touchMove", touchPoints: points(60) });
    await send("Input.dispatchTouchEvent", { type: "touchMove", touchPoints: points(90) });
    await send("Input.dispatchTouchEvent", { type: "touchEnd", touchPoints: [] });
  } else {
    await evaluate("document.querySelector('.viewer [aria-label=\"Zoom in\"]').click()");
  }
  await until(`document.querySelector('.viewer img').getBoundingClientRect().width > ${fitted * 1.4}`, "the full-screen diagram did not zoom");
  if ((await evaluate("visualViewport.scale")) !== 1) await fail("the page itself zoomed");
  await shot("chat-diagram-full");
  await key("Escape", 27);
  await until("!document.querySelector('dialog.viewer')", "Escape did not close the full-screen diagram");
  if (!(await evaluate("document.activeElement.classList.contains('diagram-open')"))) await fail("focus did not return to the diagram");
}

// A 200 KB answer on a slow phone: one parse, measured with the processor slowed four times.
async function largeAnswer() {
  await openSession("large");
  await until("document.querySelectorAll('.markdown .code-block').length > 100", "the large answer did not render");
  await send("Emulation.setCPUThrottlingRate", { rate: 4 });
  const cost = await evaluate(`(async () => {
    const { Markdown } = await import(new URL('markdown.js', document.querySelector('script[type=module]').src));
    const { render, html } = await import(new URL('lib.js', document.querySelector('script[type=module]').src));
    const data = await (await fetch(new URL('api/chat/${ids.large}/timeline', document.baseURI))).json();
    const text = data.items.findLast((item) => item.kind === 'assistant').text;
    const host = document.createElement('div');
    host.className = 'message';
    document.querySelector('.log').append(host);
    let started = performance.now();
    render(html\`<\${Markdown} text=\${text} />\`, host);
    host.getBoundingClientRect();
    const first = performance.now() - started;
    started = performance.now();
    render(html\`<\${Markdown} text=\${text} />\`, host);
    const again = performance.now() - started;
    render(null, host);
    host.remove();
    return { bytes: new TextEncoder().encode(text).length, first: Math.round(first), again: Math.round(again * 10) / 10 };
  })()`);
  await send("Emulation.setCPUThrottlingRate", { rate: 1 });
  if (cost.bytes < 200_000 || cost.first > 1500 || cost.again > 20) await fail(`a large answer is too slow: ${JSON.stringify(cost)}`);
  console.log(`large answer at ${size[0]} px, processor slowed 4 times: ${JSON.stringify(cost)}`);
}

async function others() {
  await go("#/projects");
  await until("!!document.querySelector('.row-actions')", "the projects screen did not open");
  const project = await evaluate(`(() => { const row = document.querySelector('.row-actions').closest('.row');
    return { row: row.querySelector('.row-label').textContent, count: row.querySelector('.row-context').textContent, actions: [...row.querySelectorAll('.row-actions > *')].map((n) => n.matches('.new-session') ? n.getAttribute('aria-label') : n.textContent).join() }; })()`);
  if (project.row !== "design" || !/^\d+ sessions$/.test(project.count) || project.actions !== "Files,New session in design") await fail(`the projects screen is wrong: ${JSON.stringify(project)}`);
  await sound("projects");
  await shot("projects");
  // A project opens its worktrees and their sessions; a session opens from there, and
  // Back leads to the project, then to the list.
  const listed = await evaluate("location.hash");
  await evaluate("document.querySelector('.row-open').click()");
  await until("!!document.querySelector('.worktree')", "the project screen did not open");
  const tree = await evaluate(`[...document.querySelectorAll('.worktree')].map((section) => ({ name: section.querySelector('.group-name').textContent,
    rows: [...section.querySelectorAll('.row-work')].map((node) => node.textContent), start: section.querySelector('.group-head .new-session')?.getAttribute('aria-label') }))`);
  if (tree.length !== 2 || tree[0].name !== "main · main checkout" || tree[1].name !== "fix/login"
    || tree[1].rows.join() !== "refactor" || tree[0].rows.length < 10 || tree.map((worktree) => worktree.start).join() !== "New session in main,New session in fix/login") {
    await fail(`the project screen is wrong: ${JSON.stringify(tree)}`);
  }
  if ((await text(".bar-label")).join() !== "design") await fail("the project screen does not name its project");
  const projectHash = await evaluate("location.hash");
  await sound("project");
  // Each + names what it starts, has a 44 px hit area, and takes no more height than a line of text.
  const plus = await evaluate(`[...document.querySelectorAll('.worktree .new-session')].map((button) => { const box = button.getBoundingClientRect(), head = button.parentElement.getBoundingClientRect();
    return { title: button.title === button.getAttribute('aria-label'), size: [Math.round(box.width), Math.round(box.height)], head: Math.round(head.height), right: Math.round(innerWidth - box.right), word: button.textContent.trim() }; })`);
  if (!plus.length || plus.some((button) => !button.title || button.size.join() !== "44,44" || button.head > 40 || button.right !== 4 || button.word)) await fail(`a New session + is wrong: ${JSON.stringify(plus)}`);
  await shot("project");
  // A shell is marked as one and opens on the terminal, with no chat to switch to.
  const shellRow = await evaluate(`(() => { const link = document.querySelector('.worktree a[href^="#/s/${ids.shell}/"]'); const row = link?.closest('.row');
    return row && { href: link.getAttribute('href'), context: row.querySelector('.row-context')?.textContent, state: row.querySelector('.row-state').textContent.trim() }; })()`);
  if (shellRow?.href !== `#/s/${ids.shell}/terminal` || shellRow.context !== "shell" || shellRow.state !== "open") await fail(`the shell row is wrong: ${JSON.stringify(shellRow)}`);
  await evaluate(`document.querySelector('.worktree a[href^="#/s/${ids.shell}/"]').click()`);
  await until("!!document.querySelector('.terminal-mount')", "the shell did not open on its terminal");
  if (await count(".switch, .chat")) await fail("the shell offers a chat");
  await evaluate("document.querySelector('.bar a.icon-button').click()");
  await until(`location.hash === ${JSON.stringify(projectHash)} && !!document.querySelector('.worktree')`, "Back from the shell did not return to the project");
  await evaluate("[...document.querySelectorAll('.worktree')][1].querySelector('.row-link').click()");
  await until(`location.hash === '#/s/${ids.idle}/chat' && !!document.querySelector('.session .bar-label')`, "the session did not open from its project");
  if ((await evaluate("document.querySelector('.bar a.icon-button').getAttribute('aria-label')")) !== "Back to design") await fail("the session's back control does not lead to its project");
  // Chat and Terminal are views of one screen: switching adds no step to Back.
  await evaluate("document.querySelector('.switch a:not([aria-current])').click()");
  await until(`location.hash === '#/s/${ids.idle}/terminal' && !!document.querySelector('.terminal-mount')`, "the terminal view did not open");
  await evaluate("document.querySelector('.switch a:not([aria-current])').click()");
  await until(`location.hash === '#/s/${ids.idle}/chat' && !document.querySelector('.terminal-mount')`, "the chat view did not reopen");
  await evaluate("document.querySelector('.bar a.icon-button').click()");
  await until(`location.hash === ${JSON.stringify(projectHash)} && !!document.querySelector('.worktree')`, "Back after Chat and Terminal did not return to the project");
  await evaluate("document.querySelector('.bar a.icon-button').click()");
  await until(`location.hash === ${JSON.stringify(listed)} && !!document.querySelector('.row-open')`, "Back did not return to the projects");
  await evaluate("document.querySelector('.row-actions a').click()");
  await until("[...document.querySelectorAll('.files .row-label')].some((node) => node.textContent === 'README.md')", "the files screen did not list the project");
  await evaluate("[...document.querySelectorAll('.files .row-link')].find((node) => node.textContent.trim() === 'README.md').click()");
  // A markdown file opens rendered; Source shows the editor (docs/web-client.md "Files").
  await until("document.querySelector('.file-markdown h1')?.textContent === 'design' && !!document.querySelector('.file-source[hidden] textarea.code')", "the markdown file did not open rendered");
  await sound("rendered file");
  await shot("files-markdown");
  await evaluate("[...document.querySelectorAll('.files .bar button')].find((node) => node.textContent === 'Source').click()");
  await until("!document.querySelector('.file-markdown') && document.querySelector('textarea.code')?.offsetHeight > 0", "Source did not show the editor");
  await steadyOnFocus("textarea.code", "the file editor");
  await sound("files");
  await shot("files", ".files .button-primary");
  await go("#/settings");
  await until("!!document.querySelector('input[name=scheme]')", "settings did not open");
  await sound("settings");
  await shot("settings");
}

// --- The tour.
await resize(SIZES[0]);
await pair(siteUrl, code);
const background = await evaluate("getComputedStyle(document.body).backgroundColor");
if (background !== "rgb(10, 11, 13)") await fail(`the page is not dark by default: ${background}`);
for (const next of SIZES) {
  await resize(next);
  await inbox();
  await chats();
  await markdown();
  await summary();
  await diagrams();
  await others();
}
await resize(SIZES[1]);
await largeAnswer();
await go("#/settings");
await until("!!document.querySelector('input[name=scheme]')", "settings did not reopen after the large answer");

// Once, at 390 px: the device's light choice, the dialog, reduced motion, the installed shell.
await resize(SIZES[1]);
await evaluate("document.querySelector('input[name=scheme][value=light]').click()");
if ((await evaluate("getComputedStyle(document.body).backgroundColor")) !== "rgb(247, 247, 248)" || (await evaluate("document.documentElement.dataset.scheme")) !== "light") await fail("choosing light did not change the scheme");
await sound("settings in the light scheme");
await shot("settings-light");
// The changed views in the light scheme, at the phone and the wide size.
for (const next of [SIZES[1], SIZES[2]]) {
  await resize(next);
  await go("#/");
  await until("document.querySelectorAll('.row').length >= 8", "the inbox did not open in the light scheme");
  await shot("inbox-light");
  await go("#/projects");
  await until("!!document.querySelector('.row-open')", "the projects did not open in the light scheme");
  await shot("projects-light");
  await evaluate("document.querySelector('.row-open').click()");
  await until("!!document.querySelector('.worktree')", "the project did not open in the light scheme");
  await shot("project-light");
  await openSession("markdown");
  await until("!!document.querySelector('.markdown table')", "the markdown answer did not render in the light scheme");
  await evaluate("document.querySelector('.log').scrollTop = document.querySelector('.log').scrollHeight");
  await until("!document.querySelector('.jump')", "the log did not reach its end in the light scheme");
  await shot("chat-end-light");
  await evaluate("document.querySelector('.table-wrap').scrollIntoView({ block: 'center' })");
  await shot("chat-markdown-table-light");
  await openSession("diagrams");
  await until("document.querySelectorAll('.diagram img').length === 2", "the diagrams did not draw in the light scheme");
  await shot("chat-diagrams-light");
  await summary(true);
  await openSession("long");
  await until("document.querySelectorAll('.message').length > 4 && !!document.querySelector('.composer-field:not([disabled])')", "the conversation or the composer is not ready in the light scheme");
  await typeInto("/");
  await until("document.querySelectorAll('.slash-option').length === 6", "the / list did not open in the light scheme");
  await shot("chat-commands-light");
  await typeInto("");
  if (next === SIZES[1]) {
    await openSession("idle");
    await until("!!document.querySelector('.composer-field:not([disabled])') && !!document.querySelector('.log .message, .log .empty')", "the idle composer is not ready in the light scheme");
    await shot("composer-light");
    await zoom("composer-light");
    await typeInto("Check the login redirect on Safari.\nThen the cookie settings.\nThen write the notes.");
    await shot("composer-text-light");
    await zoom("composer-text-light");
    await typeInto("");
  }
}
await resize(SIZES[1]);
// One answer, by its button: the sheet says Sent until the agent goes on, then the reply shows.
await openSession("blocked");
await until("document.querySelectorAll('.question .question-option').length === 4", "the question did not show before answering");
await evaluate("document.querySelectorAll('.question-option')[1].click()");
await until("document.querySelector('.question-keys .note')?.textContent === 'Sent'", "the sheet did not say Sent");
if (!(await evaluate("[...document.querySelectorAll('.question-option, .question-keycap')].every((button) => button.disabled)"))) await fail("an answered question still takes answers");
await sound("answered question");
await shot("chat-question-sent");
await until("!document.querySelector('.question') && [...document.querySelectorAll('.message:not(.message-user) .markdown')].some((node) => node.textContent === 'You chose: Use a bearer token')", "the answer did not reach the agent, or the sheet stayed");
// ^ Claude ends the turn after the answer; the composer returns with that report.
await until("!!document.querySelector('.composer')", "the composer did not return after the answer");

// A drawn diagram follows the scheme, and markdown reads in both.
await diagrams(true);
await openSession("markdown");
await until("!!document.querySelector('.markdown table')", "the markdown answer did not render in the light scheme");
await evaluate("document.querySelector('.log').scrollTop = 0");
await shot("chat-markdown-light");
await go("#/settings");
await until("!!document.querySelector('input[name=scheme]')", "settings did not reopen");
await evaluate("document.querySelector('input[name=scheme][value=dark]').click()");
if ((await evaluate("getComputedStyle(document.body).backgroundColor")) !== background) await fail("choosing dark did not restore the dark scheme");
// The unpair confirmation: opened by keyboard, focus on Cancel, Escape closes it, focus returns.
await evaluate("document.querySelector('.button-danger').focus()");
await key("Enter", 13, { text: "\r" });
await until("!!document.querySelector('dialog.sheet[open]')", "the dialog did not open");
if ((await evaluate("document.activeElement.textContent")) !== "Cancel") await fail("dialog focus is not on Cancel");
await shot("dialog");
await key("Escape", 27);
await until("!document.querySelector('dialog')", "Escape did not close the dialog");
if (!(await evaluate("document.activeElement.classList.contains('button-danger')"))) await fail("focus did not return to the opener");
if (!(await evaluate("fetch('api/session').then((r) => r.ok)"))) await fail("cancel unpaired the device");

await send("Emulation.setEmulatedMedia", { features: [{ name: "prefers-reduced-motion", value: "reduce" }] });
await openSession("working");
await until("!!document.querySelector('.pulse')", "the working row did not show");
if ((await evaluate("getComputedStyle(document.querySelector('.pulse')).animationName")) !== "none") await fail("reduced motion is not respected");
await send("Emulation.setEmulatedMedia", { features: [] });

// Slash commands: `/` opens the agent's list, typing filters it, the arrows move, Escape
// closes, Tab or a tap picks. One pick is sent, and the agent gets exactly "/compact".
// ^ The phone pass sends /compact, and the fixture compacts "long": later waits see the kept
// answer and the command, two messages.
for (const next of [SIZES[1], SIZES[2]]) {
  await resize(next);
  await openSession("long");
  await until("document.querySelectorAll('.message').length > 1 && !!document.querySelector('.composer-field:not([disabled])')", "the conversation or the composer is not ready for a command");
  await typeInto("/");
  await until("document.querySelectorAll('.slash-option').length === 6", "the / list did not open");
  const names = await text(".slash-name");
  for (const expected of ["/compact", "/clear", "/review-diff", "/deploy-notes", "/zebra", "/fixture:hello"]) {
    if (!names.includes(expected)) await fail(`the / list lacks ${expected}: ${names}`);
  }
  if (names[0] !== "/compact" || (await evaluate("document.querySelector('.slash-option[aria-selected=true] .slash-source').textContent")) !== "built-in") await fail(`the / list does not start with the built-ins: ${names}`);
  const field = () => evaluate("(() => { const node = document.querySelector('.composer-field'); return [node.getAttribute('aria-expanded'), node.getAttribute('aria-activedescendant'), node.value]; })()");
  await key("ArrowDown", 40);
  if ((await field()).slice(0, 2).join() !== "true,slash-1") await fail(`ArrowDown did not move in the list: ${await field()}`);
  // The list stands above the text field, on a phone too.
  const above = await evaluate("document.querySelector('.composer-field').getBoundingClientRect().top - document.querySelector('.slash').getBoundingClientRect().bottom");
  if (above < 0) await fail(`the / list covers the text field: ${above}`);
  await sound("command list");
  await shot("chat-commands");
  await key("Escape", 27);
  await until("!document.querySelector('.slash')", "Escape did not close the list");
  if ((await field())[2] !== "/") await fail("Escape changed the text");
  await typeInto("/com");
  await until("document.querySelectorAll('.slash-option').length === 1", "/com did not filter the list");
  if ((await text(".slash-name")).join() !== "/compact") await fail(`/com shows ${await text(".slash-name")}`);
  await shot("chat-commands-filtered");
  if (phone()) await evaluate("document.querySelector('.slash-option').click()");
  else await key("Tab", 9);
  await until("document.querySelector('.composer-field').value === '/compact ' && !document.querySelector('.slash')", "picking did not fill /compact");
  // The picked command is one token in the accent colour, over the field's own text.
  const token = await evaluate(`(() => { const token = document.querySelector('.composer-token'); const field = document.querySelector('.composer-field');
    const probe = Object.assign(document.createElement('span'), { style: 'color: var(--color-accent)' }); document.body.append(probe);
    const accent = getComputedStyle(probe).color; probe.remove();
    return { text: token?.textContent, color: token && getComputedStyle(token).color, accent, field: getComputedStyle(field).color,
      offset: token ? Math.round(token.getBoundingClientRect().left - field.getBoundingClientRect().left - parseFloat(getComputedStyle(field).paddingLeft)) : null }; })()`);
  if (token.text !== "/compact" || token.color !== token.accent || token.field !== "rgba(0, 0, 0, 0)" || token.offset !== 0) await fail(`the picked command is not a token: ${JSON.stringify(token)}`);
  if (phone()) {
    await shot("chat-command-token");
    await evaluate("document.documentElement.dataset.scheme = 'light'");
    await shot("chat-command-token-light");
    await evaluate("delete document.documentElement.dataset.scheme");
  } else {
    // Backspace right after the token removes the whole command; the rest stays plain text.
    await typeInto("/compact keep the notes");
    await evaluate("document.querySelector('.composer-field').setSelectionRange(8, 8)");
    await key("Backspace", 8);
    const left = await evaluate("({ value: document.querySelector('.composer-field').value, token: !!document.querySelector('.composer-token') })");
    if (left.value !== " keep the notes" || left.token) await fail(`Backspace after the token left ${JSON.stringify(left)}`);
    await typeInto("/compactly");
    if (await count(".composer-token")) await fail("a word that only starts like a command became a token");
  }
  if (phone()) {
    await evaluate("document.querySelector('.composer .button-primary').click()");
    // The fixture compacts as Claude 2.1.300 does: the summary, then the command as typed.
    await until("[...document.querySelectorAll('.message-user:not([data-pending]) .prose')].at(-1)?.textContent === '/compact'"
      + " && [...document.querySelectorAll('.summary-body')].some((node) => node.textContent.includes('Run the tests.'))", "the agent did not compact on /compact");
  } else {
    await typeInto("");
  }
}

// The `/` list with the keyboard up (the visual viewport shrinks, the layout viewport does not)
// and on a short screen: at most 3.5 rows of a name and a muted description, part of the
// conversation in view, and the composer at the bottom of what is visible.
const keyboard = (height) => evaluate(`(() => {
  const viewport = window.visualViewport;
  delete viewport.height;
  if (${height} !== null) Object.defineProperty(viewport, "height", { configurable: true, get: () => ${height} });
  viewport.dispatchEvent(new Event("resize"));
})()`);
async function commandsAbove(label, tag) {
  await openSession("long");
  await until("document.querySelectorAll('.message').length > 1 && !!document.querySelector('.composer-field:not([disabled])')", `${label}: the composer is not ready`);
  // The conversation with the keyboard up and nothing typed yet.
  await typeInto("");
  await shot(`chat-typing-${tag}`);
  // A note of the list is one muted line at the end of the list's own box.
  await typeInto("/zzz");
  await until("!!document.querySelector('.slash-note')", `${label}: no note for a filter without matches`);
  const note = await evaluate(`(() => { const box = (node) => node.getBoundingClientRect(); const note = document.querySelector('.slash-note'), list = document.querySelector('.slash');
    return { inside: list.contains(note), lines: Math.round(box(note).height / parseFloat(getComputedStyle(note).lineHeight)),
      between: box(document.querySelector('.composer-row')).top - box(list).bottom }; })()`);
  if (!note.inside || note.lines !== 1 || note.between > 9) await fail(`${label}: the list's note is not one line inside the list: ${JSON.stringify(note)}`);
  await typeInto("/");
  await until("document.querySelectorAll('.slash-option').length === 6", `${label}: the / list did not open`);
  const seen = await evaluate(`(() => { const box = (selector) => document.querySelector(selector).getBoundingClientRect();
    return { list: Math.round(box('.slash').height), row: Math.round(box('.slash-option').height), log: Math.round(box('.log-wrap').height),
      composer: Math.round(box('.composer').bottom), visible: Math.round(visualViewport.offsetTop + visualViewport.height), scrolled: document.scrollingElement.scrollTop,
      below: Math.round(box('.slash-option .slash-description').top - box('.slash-option .slash-name').bottom) }; })()`);
  if (seen.list > seen.row * 3.5 + 2 || seen.log < 60 || Math.abs(seen.composer - seen.visible) > 1 || seen.scrolled !== 0 || seen.below < 0) {
    await fail(`${label}: the / list does not fit above the keyboard: ${JSON.stringify(seen)}`);
  }
  console.log(`/ list ${label}: ${JSON.stringify(seen)}`);
  // While the composer has the focus no other form field takes part in iOS's previous and
  // next arrows; after it, every field has its own tab order again.
  const others = "[...document.querySelectorAll('input, select, textarea')].filter((node) => node !== document.querySelector('.composer-field'))";
  if (!(await evaluate(`document.activeElement === document.querySelector('.composer-field') && ${others}.every((node) => node.tabIndex === -1)`))) {
    await fail(`${label}: another form field can take the focus from the composer`);
  }
  await evaluate("document.querySelector('.composer-field').blur()");
  if (await evaluate(`${others}.some((node) => node.hasAttribute('data-tab-index'))`)) await fail(`${label}: the other fields did not get their tab order back`);
  await evaluate("document.querySelector('.composer-field').focus()");
}
await resize(SIZES[1]);
await keyboard(420);
await commandsAbove("with the keyboard up at 390x844", "keyboard");
await shot("chat-commands-keyboard");
await evaluate("document.documentElement.dataset.scheme = 'light'");
await shot("chat-commands-keyboard-light");
await evaluate("delete document.documentElement.dataset.scheme");
await typeInto("");
// Pinned to the visual viewport only while a field takes typing. iOS may not report that the
// keyboard closed: the visual viewport still says 420 px, but the inbox has the full screen.
const bodyHeight = "Math.round(document.body.getBoundingClientRect().height)";
if ((await evaluate(bodyHeight)) !== 420) await fail(`the page is not on the keyboard while typing: ${await evaluate(bodyHeight)} px`);
await evaluate("document.activeElement.blur()");
await go("#/");
await until("document.querySelectorAll('.row').length > 0", "the inbox did not open after typing");
await pause(500);
const after = await evaluate(`({ body: ${bodyHeight}, screen: innerHeight, tabs: Math.round(document.querySelector('.tabbar').getBoundingClientRect().bottom), keyboard: document.body.hasAttribute('data-keyboard') })`);
if (after.body !== after.screen || after.tabs !== after.screen || after.keyboard) await fail(`the inbox keeps the keyboard's height: ${JSON.stringify(after)}`);
await shot("inbox-after-keyboard");
await keyboard(null);
await resize([390, 420, 2]);
await commandsAbove("on a 390x420 screen", "short");
await shot("chat-commands-short");
await evaluate("document.documentElement.dataset.scheme = 'light'");
await shot("chat-commands-short-light");
await evaluate("delete document.documentElement.dataset.scheme");
await typeInto("");
await resize(SIZES[1]);

// A turn that writes on a long conversation, on a slow phone: what the page pays for each
// change while the agent works (docs/web-client.md "Live updates").
await resize(SIZES[1]);
await openSession("large");
await until("document.querySelectorAll('.markdown .code-block').length > 100", "the large conversation did not render");
await send("Performance.enable");
const metric = async (name) => (await send("Performance.getMetrics")).metrics.find((item) => item.name === name).value;
await evaluate(`(async () => {
  const { events } = await import(new URL('store.js', document.querySelector('script[type=module]').src));
  window.__events = {};
  for (const type of ["history", "exchange", "workspace"]) events.addEventListener(type, () => { window.__events[type] = (window.__events[type] ?? 0) + 1; });
  performance.setResourceTimingBufferSize(10000);
  window.__streamStart = performance.now();
  window.__longTasks = 0;
  new PerformanceObserver((list) => { window.__longTasks += list.getEntries().length; }).observe({ type: "longtask" });
})()`);
await send("Emulation.setCPUThrottlingRate", { rate: 4 });
const busyBefore = await metric("TaskDuration");
const started = Date.now();
await evaluate(`fetch(new URL('api/chat/${ids.large}/send', document.baseURI), { method: 'POST', headers: { 'content-type': 'application/json' },
  body: JSON.stringify({ text: 'stream 80' }) }).then((r) => r.status)`);
await until("[...document.querySelectorAll('.message .markdown')].some((node) => node.textContent === 'The stream is done.')", "the streamed turn did not finish on the page", 900);
const seconds = (Date.now() - started) / 1000;
const busy = (await metric("TaskDuration")) - busyBefore;
const stream = await evaluate(`(() => {
  const during = performance.getEntriesByType('resource').filter((entry) => entry.startTime > window.__streamStart);
  const loads = during.filter((entry) => entry.name.includes('/timeline'));
  const events = during.filter((entry) => entry.name.includes('/api/events')).length;
  const sum = (values) => values.reduce((total, value) => total + value, 0);
  return { loads: loads.length, bytes: Math.round(sum(loads.map((entry) => entry.encodedBodySize)) / Math.max(1, loads.length)),
    serverMs: Math.round(sum(loads.map((entry) => entry.responseStart - entry.requestStart)) / Math.max(1, loads.length)),
    longTasks: window.__longTasks, events: window.__events, streams: events };
})()`);
await send("Emulation.setCPUThrottlingRate", { rate: 1 });
Object.assign(stream, { seconds: Math.round(seconds), busyShare: Math.round((busy / seconds) * 100) / 100,
  busyPerLoadMs: Math.round((busy * 1000) / Math.max(1, stream.loads)) });
console.log(`streamed turn on a 200 KB conversation at 390 px, processor slowed 4 times: ${JSON.stringify(stream)}`);
if (stream.bytes > 65_536) await fail(`each change still fetches the whole timeline: ${JSON.stringify(stream)}`);

// A message the agent did not get says so where it was sent, with wsxd's reason, and
// can be sent again. The done agent holds an unsent line it cannot stash.
await openSession("done");
await until("!!document.querySelector('.composer-field:not([disabled])')", "the done session's composer is not ready");
// A waiting message that was not delivered: by the composer, its reason in words on a line
// of its own in the danger colour, with Copy, Send again and Remove as icons.
await until("!!document.querySelector('.queued[data-failed] .queued-reason')", "the failed waiting message does not show");
const stuck = await evaluate(`(() => { const row = document.querySelector('.queued[data-failed]'); const reason = row.querySelector('.queued-reason');
  const box = (node) => node.getBoundingClientRect();
  return { reason: reason.textContent, color: getComputedStyle(reason).color, danger: getComputedStyle(document.documentElement).getPropertyValue('--color-danger').trim(),
    actions: [...row.querySelectorAll('button')].map((button) => button.title).join(), own: box(reason).top >= box(row.querySelector('.queued-text')).bottom - 1,
    whole: reason.scrollWidth <= reason.clientWidth, above: box(row).bottom <= box(document.querySelector('.composer')).top + 1 }; })()`);
// A delivered message of the conversation never reads as not delivered.
if (await evaluate("[...document.querySelectorAll('.log .message-user:not([data-pending]) .note')].some((note) => note.textContent.includes('Not delivered'))")) await fail("a delivered message says it was not delivered");
if (stuck.reason !== "Not delivered to the agent: prompt was not sent because fresh empty input was not observed" || stuck.actions !== "Copy,Send again,Remove"
  || !stuck.own || !stuck.whole || !stuck.above) await fail(`the failed waiting message is easy to miss: ${JSON.stringify(stuck)}`);
await shot("chat-queue-failed");
await evaluate("document.documentElement.dataset.scheme = 'light'");
await shot("chat-queue-failed-light");
await evaluate("delete document.documentElement.dataset.scheme");
await typeInto("Please check the notes");
await evaluate("document.querySelector('.composer .button-primary').click()");
// ^ Every message goes through the session's queue: one the agent cannot take waits there,
// with wsxd's reason (docs/web-client-chat.md "Queue and Stop").
const failedRow = "[...document.querySelectorAll('.queued[data-failed]')].find((row) => row.querySelector('.queued-text').textContent === 'Please check the notes')";
const notDelivered = `${failedRow}?.querySelector('.queued-reason')?.textContent ?? ''`;
await until(`(${notDelivered}).startsWith('Not delivered to the agent: prompt was not sent because fresh empty input was not observed')`, "a failed delivery did not say so");
await sound("message not delivered");
await shot("chat-not-delivered");
const round = () => evaluate(`fetch(new URL('api/chat/${ids.done}/timeline', document.baseURI)).then((r) => r.json()).then((data) => data.exchange.round)`);
const before = await round();
await evaluate(`[...${failedRow}.querySelectorAll('button')].find((button) => button.title === 'Send again').click()`);
await until(`fetch(new URL('api/chat/${ids.done}/timeline', document.baseURI)).then((r) => r.json()).then((data) => data.exchange.round === ${before + 1})`, "Send again did not send the message again");
await until(`(${notDelivered}).startsWith('Not delivered')`, "the message sent again did not keep its reason");

// Files: each shows at once while it uploads and Send waits; a photo larger than 2000 px
// is made smaller before it leaves the phone; the draft is still there after a reload.
await send("Network.emulateNetworkConditions", { offline: false, latency: 100, downloadThroughput: 1_000_000, uploadThroughput: 40_000 });
await typeInto("kept after a reload");
const original = await evaluate(`(async () => {
  const canvas = Object.assign(document.createElement('canvas'), { width: 3000, height: 2000 });
  const context = canvas.getContext('2d');
  for (let x = 0; x < 3000; x += 50) for (let y = 0; y < 2000; y += 50) {
    context.fillStyle = 'hsl(' + ((x * 7 + y * 13) % 360) + ' 60% 50%)';
    context.fillRect(x, y, 50, 50);
  }
  const blob = await new Promise((done) => canvas.toBlob(done, 'image/png'));
  const input = document.querySelector('.composer input[type=file]');
  const files = new DataTransfer();
  files.items.add(new File([blob], 'IMG_0001.png', { type: 'image/png' }));
  input.files = files.files;
  input.dispatchEvent(new Event('change', { bubbles: true }));
  return blob.size;
})()`);
await until("!!document.querySelector('.chips [data-uploading]')", "the file did not show while it uploads");
if (!(await evaluate("document.querySelector('.composer .button-primary').disabled"))) await fail("Send is not disabled while a file uploads");
await shot("chat-uploading");
await until("!document.querySelector('.chips [data-uploading]') && document.querySelectorAll('.chips li').length === 1", "the upload did not finish", 600);
await send("Network.emulateNetworkConditions", { offline: false, latency: 0, downloadThroughput: -1, uploadThroughput: -1 });
const sent = await evaluate(`(async () => {
  const attachment = JSON.parse(localStorage.getItem('wsx-drafts'))['${ids.done}'].attachments[0];
  const picture = await createImageBitmap(await (await fetch(new URL(attachment.url, document.baseURI))).blob());
  return { name: attachment.name, size: attachment.size, edge: Math.max(picture.width, picture.height) };
})()`);
if (sent.edge !== 2000 || sent.size >= original || !/^IMG_0001\.(jpg|png)$/.test(sent.name)) await fail(`the photo was not made smaller: ${JSON.stringify({ original, ...sent })}`);
await send("Page.reload");
await until("!!document.querySelector('.composer-field:not([disabled])')", "the done session did not reopen after a reload");
await until(`(${notDelivered}).startsWith('Not delivered')`, "the message that was not delivered is gone after a reload");
const kept = await evaluate("({ text: document.querySelector('.composer-field').value, files: [...document.querySelectorAll('.chips li span:first-child')].map((node) => node.textContent) })");
if (kept.text !== "kept after a reload" || kept.files.length !== 1 || !kept.files[0].startsWith(sent.name)) await fail(`the draft was lost in a reload: ${JSON.stringify(kept)}`);
// Leave no draft behind: removing the file and the text forgets it on this device too.
await evaluate("document.querySelector('.chips button').click()");
await typeInto("");
const left = await evaluate(`(JSON.parse(localStorage.getItem('wsx-drafts')) ?? {})['${ids.done}']`);
if (left?.text || left?.attachments.length) await fail(`an emptied draft was kept on the device: ${JSON.stringify(left)}`);
console.log(`photo of ${original} bytes and 3000 px sent as ${sent.size} bytes and ${sent.edge} px`);

// A pasted screenshot attaches as the attach button does, with a name of its own, and Send
// carries it. A text paste stays the browser's: a synthetic paste inserts nothing, so the
// check is that the page did not cancel it and added no file.
if (!(await pasteInto(".composer-field", "pasted words"))) await fail("a text paste was cancelled");
await pause(300);
if (await evaluate("document.querySelectorAll('.chips li').length")) await fail("a text paste added a file");
if (await pasteInto(".composer-field", "image")) await fail("a pasted image was left to the text box");
await until("document.querySelectorAll('.chips li').length === 1 && !document.querySelector('.chips [data-uploading]')", "the pasted image did not upload", 100);
const pasted = await evaluate(`JSON.parse(localStorage.getItem('wsx-drafts'))['${ids.done}'].attachments[0]`);
if (!/^pasted-\d{14}\.(png|jpg)$/.test(pasted.name)) await fail(`the pasted image is named ${pasted.name}`);
await evaluate(`(() => { window.sentBodies = []; const original = window.fetch;
  window.fetch = (url, init) => { if (String(url).includes('/send')) sentBodies.push(JSON.parse(init.body)); return original(url, init); }; })()`);
await typeInto("a pasted screenshot");
const beforePaste = await round();
await evaluate("document.querySelector('.composer .button-primary').click()");
await until(`fetch(new URL('api/chat/${ids.done}/timeline', document.baseURI)).then((r) => r.json()).then((data) => data.exchange.round === ${beforePaste + 1})`, "the message with a pasted image was not sent");
const carried = await evaluate("window.sentBodies");
if (carried.length !== 1 || JSON.stringify(carried[0].attachments) !== JSON.stringify([pasted.hash])) await fail(`Send carried ${JSON.stringify(carried)}, not ${pasted.hash}`);
await until("!document.querySelector('.chips li')", "the sent pasted image stayed in the composer");

// Sub-path surfaces as the browser resolved them.
const facts = await evaluate(`(async () => {
  const registration = await navigator.serviceWorker.ready;
  return { scope: registration.scope, requests: performance.getEntriesByType('resource').map((entry) => new URL(entry.name).pathname) };
})()`);
const base = new URL(siteUrl).pathname;
if (facts.scope !== siteUrl) await fail(`service worker scope is ${facts.scope}`);
const stray = facts.requests.filter((request) => !request.startsWith(base));
if (stray.length) await fail(`requests left the base path: ${stray.join(", ")}`);
if (!facts.requests.includes(`${base}api/session`)) await fail("no API request under the base path");

// The first, unpaired load answers 401 by design; any other console error is a defect.
const defects = problems.filter((problem) => !/status of 401/.test(problem));
if (defects.length) await fail(`console errors: ${defects.join(" | ")}`);

// A site with no session: one sentence and the create action.
await pair(emptyUrl, emptyCode);
for (const next of SIZES) {
  await resize(next);
  await until("(document.querySelector('.empty')?.textContent ?? '').includes('No session')", "the empty inbox did not show");
  const empty = (await text(".empty")).join();
  if (empty !== "No session is open. Start one") await fail(`the empty inbox says ${JSON.stringify(empty)}`);
  await sound("empty inbox");
  await shot("inbox-empty");
}

// Offline, last: the installed shell of the first site still loads and says what is wrong.
await resize(SIZES[1]);
await send("Page.navigate", { url: siteUrl });
await until("!!document.querySelector('.app')", "the first site did not reopen");
await evaluate("navigator.serviceWorker.ready.then(() => true)");
await send("Network.emulateNetworkConditions", { offline: true, latency: 0, downloadThroughput: 0, uploadThroughput: 0 });
await send("Page.reload");
await until("/Cannot reach the server/.test(document.querySelector('#app')?.textContent ?? '')", "offline shell did not render");
await shot("offline");

console.log("page at 320x568, 390x844 and 1280x800: inbox with a session in each state, working chat with a running tool and a waiting message, long chat with jump button, markdown answer with table and code, mermaid diagrams drawn as images and full screen, a 200 KB answer, question sheets for a permission dialog and a four-option question and one answer sent, empty chat and its Enter rules, projects, files, settings, light scheme, dialog, reduced motion, base-path requests only, offline shell, empty inbox; no sideways overflow, one scroll container, at most one filled accent control");
await cleanup();
process.exit(0);
