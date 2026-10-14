#!/usr/bin/env node
// Real-browser check of the page: every screen and agent state at three sizes, driven
// over the Chrome DevTools Protocol. Called by `test-web-client.py --browser`.
//   tour <site-url> <code> <empty-site-url> <code> <sessions-json> <capture-dir> [slop-audit.js]
//   down <site-url> <code> <capture-dir> [slop-audit.js]     (wsxd is stopped)
// Each capture is saved with the slop audit of /ui-view-standards beside it.
import fs from "node:fs";
import path from "node:path";
import { launch } from "./web-cdp.mjs";

const [mode, siteUrl, code, ...rest] = process.argv.slice(2);
const [emptyUrl, emptyCode, sessionsJson, captures, auditPath] = mode === "tour" ? rest : [null, null, "{}", ...rest];
const ids = JSON.parse(sessionsJson);
const { send, evaluate, until, viewport, capture, fail, cleanup, problems } = await launch({
  captures, prefix: "", timeoutSeconds: mode === "tour" ? 420 : 100,
});
const pause = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const auditSource = auditPath ? fs.readFileSync(auditPath, "utf8") : null;
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
  await capture(`${size[0]}-${name}`);
}
// Layout rules of every screen: nothing past the right edge, no raw null, one scroll container
// (the wide layout's rail is a list of its own, and a tall question sheet is bounded: both may scroll).
async function sound(label) {
  const seen = await evaluate(`(() => {
    const free = (node) => node.closest('.crumbs, .terminal-keys, .code, .xterm, .code-block pre, .table-wrap, .diagram, .question-screen');
    const wide = [...document.querySelectorAll('#app *')].filter((node) => !free(node) && node.getBoundingClientRect().right > innerWidth + 0.5);
    const scrollers = [...document.querySelectorAll('#app *')].filter((node) => node.tagName !== 'TEXTAREA' && !free(node) && !node.closest('.rail, .question')
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
const go = (hash) => evaluate(`location.hash = ${JSON.stringify(hash)}`);
const text = (selector) => evaluate(`[...document.querySelectorAll(${JSON.stringify(selector)})].map((node) => node.textContent.trim().replace(/\\s+/g, ' '))`);
const count = (selector) => evaluate(`document.querySelectorAll(${JSON.stringify(selector)}).length`);
async function key(name, keyCode, options = {}) {
  await send("Input.dispatchKeyEvent", { type: "keyDown", key: name, code: name, windowsVirtualKeyCode: keyCode, ...options });
  await send("Input.dispatchKeyEvent", { type: "keyUp", key: name, code: name, windowsVirtualKeyCode: keyCode, ...options });
}
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
  if (states("Sessions") !== "idle,shell") await fail(`Sessions shows ${JSON.stringify(sections.Sessions)}`);
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
  if ((await text(".group-name")).join() !== "design" || !(await count(".group-head .link-action"))) await fail("the Sessions group lacks its project or its New session action");
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
  await until("!!document.querySelector('.working') && !!document.querySelector('.queued')", "the working row or the queue did not show");
  const working = await evaluate(`({ state: document.querySelector('.bar-state').textContent, activity: document.querySelector('.working-activity').textContent,
    live: [...document.querySelectorAll('.steps[data-live] .step')].map((step) => step.textContent.trim().replace(/\\s+/g, ' ')),
    send: document.querySelector('.composer .button-primary').textContent, stop: [...document.querySelectorAll('.working button')].map((b) => b.textContent).join(),
    queued: document.querySelector('.queued-text').textContent, switch: [...document.querySelectorAll('.switch a')].map((a) => a.textContent.trim() + (a.getAttribute('aria-current') ? '*' : '')).join(),
    pulse: getComputedStyle(document.querySelector('.pulse')).animationName })`);
  if (working.state !== "working" || working.activity !== "List the files" || working.send !== "Queue" || working.stop !== "Stop"
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
  if (column.steps !== column.left || column.user !== column.right || column.send !== column.right || column.queued !== column.left
    || column.height !== 44 || column.radius[0] !== column.radius[1] || column.field < column.left) {
    await fail(`the chat is not one column, or the send button is not a 44 px control: ${JSON.stringify(column)}`);
  }
  console.log(`chat column at ${size[0]} px: ${JSON.stringify(column)}`);
  await sound("working chat");
  await shot("chat-working", ".composer .button-primary");

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
        under: box(document.querySelector('.bar-state')).top >= box(document.querySelector('.bar-label')).bottom - 1, state: document.querySelector('.bar-state').textContent }; })()`);
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
  // The end of the conversation stands clear of the composer.
  await evaluate("document.querySelector('.log').scrollTop = document.querySelector('.log').scrollHeight");
  await until("!document.querySelector('.jump')", "the log did not reach its end");
  const gap = await evaluate(`document.querySelector('.composer').getBoundingClientRect().top
    - [...document.querySelectorAll('.log > *')].at(-1).getBoundingClientRect().bottom`);
  if (gap < 31.5) await fail(`the last message is ${gap} px from the composer`);
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
      sheets: document.adoptedStyleSheets.length, kept: !!(await cache.match('assets/mermaid-11.17.2.js')),
      shell: (await cache.keys()).length }; })()`);
  if (seen.sources.join() !== "blob:,blob:" || seen.natural.some(([width, height]) => width < 200 || height < 100) || seen.inline) await fail(`a diagram is not a drawn image: ${JSON.stringify(seen)}`);
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
    const { Markdown } = await import(new URL('assets/markdown.js', document.baseURI));
    const { render, html } = await import(new URL('assets/lib.js', document.baseURI));
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
    return { row: row.querySelector('.row-label').textContent, count: row.querySelector('.row-context').textContent, actions: [...row.querySelectorAll('.row-actions > *')].map((n) => n.textContent).join() }; })()`);
  if (project.row !== "design" || !/^\d+ sessions$/.test(project.count) || project.actions !== "Files,New session") await fail(`the projects screen is wrong: ${JSON.stringify(project)}`);
  await sound("projects");
  await shot("projects");
  // A project opens its worktrees and their sessions; a session opens from there, and
  // Back leads to the project, then to the list.
  const listed = await evaluate("location.hash");
  await evaluate("document.querySelector('.row-open').click()");
  await until("!!document.querySelector('.worktree')", "the project screen did not open");
  const tree = await evaluate(`[...document.querySelectorAll('.worktree')].map((section) => ({ name: section.querySelector('.group-name').textContent,
    rows: [...section.querySelectorAll('.row-work')].map((node) => node.textContent), start: !!section.querySelector('.group-head .link-action') }))`);
  if (tree.length !== 2 || tree[0].name !== "main · main checkout" || tree[1].name !== "fix/login"
    || tree[1].rows.join() !== "refactor" || tree[0].rows.length < 10 || !tree.every((worktree) => worktree.start)) {
    await fail(`the project screen is wrong: ${JSON.stringify(tree)}`);
  }
  if ((await text(".bar-label")).join() !== "design") await fail("the project screen does not name its project");
  const projectHash = await evaluate("location.hash");
  await sound("project");
  await shot("project");
  await evaluate("[...document.querySelectorAll('.worktree')][1].querySelector('.row-link').click()");
  await until(`location.hash === '#/s/${ids.idle}/chat' && !!document.querySelector('.session .bar-label')`, "the session did not open from its project");
  if ((await evaluate("document.querySelector('.bar a.icon-button').getAttribute('aria-label')")) !== "Back to design") await fail("the session's back control does not lead to its project");
  await evaluate("document.querySelector('.bar a.icon-button').click()");
  await until(`location.hash === ${JSON.stringify(projectHash)} && !!document.querySelector('.worktree')`, "Back did not return to the project");
  // Chat and Terminal are views of one screen: switching adds no step to Back.
  await evaluate(`[...document.querySelectorAll('.worktree .row-link')].find((node) => node.getAttribute('href') === '#/s/${ids.shell}/chat').click()`);
  await until(`location.hash === '#/s/${ids.shell}/chat'`, "the shell session did not open from its project");
  await evaluate("document.querySelector('.switch a:not([aria-current])').click()");
  await until(`location.hash === '#/s/${ids.shell}/terminal' && !!document.querySelector('.terminal-mount')`, "the terminal view did not open");
  await evaluate("document.querySelector('.switch a:not([aria-current])').click()");
  await until(`location.hash === '#/s/${ids.shell}/chat' && !document.querySelector('.terminal-mount')`, "the chat view did not reopen");
  await evaluate("document.querySelector('.bar a.icon-button').click()");
  await until(`location.hash === ${JSON.stringify(projectHash)} && !!document.querySelector('.worktree')`, "Back after Chat and Terminal did not return to the project");
  await evaluate("document.querySelector('.bar a.icon-button').click()");
  await until(`location.hash === ${JSON.stringify(listed)} && !!document.querySelector('.row-open')`, "Back did not return to the projects");
  await evaluate("document.querySelector('.row-actions a').click()");
  await until("[...document.querySelectorAll('.files .row-label')].some((node) => node.textContent === 'README.md')", "the files screen did not list the project");
  await evaluate("[...document.querySelectorAll('.files .row-link')].find((node) => node.textContent.trim() === 'README.md').click()");
  await until("!!document.querySelector('textarea.code')", "the file did not open");
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
if (!(await count(".composer"))) await fail("the composer did not return after the answer");

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
