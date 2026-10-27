// The manager chat as messages: one bubble per owner message and manager reply, each with its
// author and time, task changes as muted lines, one each, and the manager's replies to them as
// lines under its name (docs/web-client-tasks.md "Project screen").
import { clock, firstSentence, html, useState } from "./lib.js";
import { Markdown } from "./markdown.js";
import { Attachments } from "./chat.js";
import { Quoted, quoteOf } from "./reply.js";
import { ChatApprovals, TaskChip } from "./tasks.js";

// Bubbles of one author closer together than this read as one group.
const GROUP_MS = 5 * 60_000;
// The same status line again this soon shows once.
const LINE_MS = 60_000;

const EVENT_WORDS = { landed: "done" };
const TASK = /^#(\d+) /;
const CLOSED = /^#\d+ was closed\b/;
// A line's leading task number and the words after it.
const LEADING_TASK = /^#(\d+)\b(.*)$/s;

// One line written by wsx in its short form: `#3 ready r2: title; summary` → `#3 ready`,
// `approval a3 (land #5) approved; done` → `Approved land #5`, a dropped one with its reason:
// `approval a9 (discard #42) dropped; #42 is closed` → `Dropped discard #42: #42 is closed`.
function shortLine(line) {
  const task = /^#(\d+) (\w+) r\d+:/.exec(line);
  if (task) return `#${task[1]} ${EVENT_WORDS[task[2]] ?? task[2]}`;
  const decision = /^approval \S+ \((.+?)\) (approved|denied|dropped)\b(?:; (.*))?/.exec(line);
  if (decision?.[2] === "dropped") return `Dropped ${decision[1]}: ${decision[3] ?? ""}`;
  if (decision) return `${decision[2] === "approved" ? "Approved" : "Denied"} ${decision[1]}`;
  return line.split("; ")[0];
}

// The conversation in time order: bubbles, status lines and quiet replies. A wsx line that a
// resolution says too ("Approved land #3", or "#3 closed" and "#3 was closed") shows once, as the
// wsx line, and so does a line said again within a minute. A pending approval goes into the
// manager's reply that ended the turn which asked for it (docs/web-client-tasks.md "Project screen").
export function conversation(entries, resolutions, approvals = []) {
  const rows = [];
  // ^ wsx's recap request is not shown; the manager's answer to it is tagged Recap
  // (docs/tasks.md "Recap").
  const recaps = new Set(entries.filter((entry) => entry.kind === "message" && entry.from === "recap").map((entry) => entry.id));
  entries.forEach((entry, index) => {
    const at = entry.unix_ms ?? 0;
    const key = entry.id != null ? `${entry.kind}-${entry.id}` : `n${index}`;
    if (entry.kind === "message" && entry.from === "owner") {
      rows.push({ kind: "bubble", at, key, author: "owner", entry });
    } else if (entry.kind === "message" && entry.from === "recap") {
      return;
    } else if (entry.kind === "reply") {
      rows.push({ kind: "bubble", at, key, author: "manager", entry, quiet: entry.quiet, tag: entry.to?.some((id) => recaps.has(id)) ? "Recap" : null });
    } else if (entry.kind === "message" && /^manager(-reply)?:/.test(entry.from ?? "")) {
      // ^ Another project's manager wrote (docs/tasks.md "Managers talk").
      const [from, peer] = entry.from.split(/:(.*)/s);
      rows.push({ kind: "bubble", at, key, author: `peer:${peer}`, entry, tag: from === "manager-reply" ? "Reply" : null });
    } else if (entry.kind === "sent") {
      rows.push({ kind: "bubble", at, key, author: "manager", entry, to: entry.to });
    } else if (entry.kind === "message") {
      entry.text.split("\n").filter(Boolean).forEach((line, part) => rows.push({ kind: "line", at, key: `${key}-${part}`, text: shortLine(line) }));
    } else if (entry.kind === "rotated" || entry.kind === "stopped") {
      rows.push({ kind: "note", at, key, entry });
    }
  });
  const said = rows.filter((row) => row.kind === "line");
  resolutions.forEach((resolution, index) => {
    const near = (row) => Math.abs(row.at - resolution.unix_ms) < 10 * LINE_MS;
    // ^ A close is a task change the events say too ("#7 closed", "#7 done"): the event line
    // about that task stays. An answer or a failed check has no event line of its own.
    const closed = CLOSED.test(resolution.text);
    const same = (row) => row.text === resolution.text || (closed && TASK.exec(row.text)?.[1] === String(resolution.task));
    if (said.some((row) => near(row) && same(row))) return;
    rows.push({ kind: "line", at: resolution.unix_ms, key: `res-${index}-${resolution.unix_ms}`, text: resolution.text });
  });
  rows.sort((left, right) => left.at - right.at);
  for (const approval of approvals) {
    const reply = rows.find((row) => row.entry?.kind === "reply" && row.at >= approval.unix_ms);
    if (reply) reply.approvals = [...(reply.approvals ?? []), approval];
  }
  // ^ A quiet reply (short, to a turn of only events) is the manager's own work: a muted line under its name,
  // unless it asks for an approval; `-` says nothing changed for the owner (docs/tasks.md "Manager").
  const shown = [];
  for (const row of rows) {
    const quiet = row.quiet && !row.approvals;
    const text = quiet ? row.entry.text.trim() : row.text;
    if (quiet && (!text || text === "-")) continue;
    const last = shown.at(-1);
    if (row.kind === "line" && last?.kind === "line" && last.text === row.text && row.at - last.at < LINE_MS) continue;
    shown.push(quiet ? { kind: "quiet", at: row.at, key: row.key, text } : { ...row });
  }
  // ^ Adjacent bubbles of one author group (author once, time last); quiet replies in a row
  // name the manager once.
  shown.forEach((row, index) => {
    const before = shown[index - 1];
    const after = shown[index + 1];
    if (row.kind === "quiet") row.first = !(before?.kind === "quiet" && row.at - before.at < GROUP_MS);
    if (row.kind !== "bubble") return;
    const same = (other, gap) => other?.kind === "bubble" && other.author === row.author && gap < GROUP_MS;
    row.first = !same(before, row.at - (before?.at ?? 0));
    row.last = !same(after, (after?.at ?? 0) - row.at);
  });
  return folded(shown);
}

// ^ Event lines and quiet replies in a row, three or more, fold into one row: each task of the
// run once with its newest words, and how many rows a tap shows. The run's last event line and
// the replies after it stay below the fold (docs/web-client-tasks.md "Project screen").
function folded(rows) {
  const out = [];
  for (let start = 0; start < rows.length;) {
    let end = start;
    while (end < rows.length && (rows[end].kind === "line" || rows[end].kind === "quiet")) end++;
    if (end === start) {
      out.push(rows[start++]);
      continue;
    }
    const run = rows.slice(start, end);
    start = end;
    const lastLine = run.findLastIndex((row) => row.kind === "line");
    const tail = lastLine < 0 ? run.length - 1 : lastLine;
    if (run.length < 3 || tail < 2) {
      out.push(...run);
      continue;
    }
    const chips = new Map();
    for (const row of run) {
      const [, task, rest] = row.kind === "line" ? LEADING_TASK.exec(row.text) ?? [] : [];
      if (task) chips.set(task, rest);
    }
    if (run[tail].kind === "quiet") run[tail].first = true;
    // ^ Keyed by the run's first row: a run that grows keeps its row and whether it is open.
    out.push({ kind: "fold", key: `fold-${run[0].key}`, at: run[tail - 1].at, rows: run.slice(0, tail), chips: [...chips] }, ...run.slice(tail));
  }
  return out;
}

// A folded run: one tap shows its rows under it, rendered by `render`, another folds them.
export function Fold({ row, render }) {
  const [open, setOpen] = useState(false);
  return html`
    <div class="fold" data-key=${row.key} data-at=${row.at}>
      <button class="fold-toggle" type="button" aria-expanded=${open} onClick=${() => setOpen(!open)}>
        ${row.chips.map(([task, rest]) => html`<span class="fold-chip" key=${task}><span class="fold-task">#${task}</span>${rest}</span>`)}
        <span class="fold-count">${open ? "Hide " : ""}${row.rows.length} updates</span>
      </button>
      ${open && row.rows.map(render)}
    </div>`;
}

// `names`: the manager's and its peers' names as the server resolves them (docs/tasks.md "Config").
const authorName = (author, names) => author === "owner" ? "You" : author === "manager" ? names.manager
  : names.peers[author.slice(5)];

// ^ Another project's manager keeps one of the `color-peer-N` colors, from its project
// (docs/web-themes.md "Tokens").
const PEER_COLORS = 4;
const peerColor = (project) => {
  let hash = 0;
  for (const char of project) hash = (hash * 31 + char.codePointAt(0)) >>> 0;
  return hash % PEER_COLORS + 1;
};

// One message. `words` turns its text runs into task chips.
export function Bubble({ project, row, words, names, fresh }) {
  const out = row.author === "owner";
  const peer = row.author.startsWith("peer:") ? row.author.slice(5) : null;
  const id = `bubble-${row.key}`;
  const tag = row.to != null ? `${row.entry.reply ? "Reply to" : "To"} ${names.peers[row.to]}` : row.tag;
  let body;
  if (out) {
    body = html`<div class="bubble-body" id=${id}><${Quoted} text=${row.entry.text} words=${words} /></div><${Attachments} files=${row.entry.attachments} />`;
  } else {
    body = html`<div class="bubble-body" id=${id}><${Markdown} text=${row.entry.text} words=${words} prose /></div>
      ${row.approvals && html`<${ChatApprovals} project=${project} items=${row.approvals} />`}`;
  }
  return html`
    <article class="bubble" data-side=${out ? "out" : "in"} data-speaker=${peer == null ? row.author : "peer"}
      data-peer=${peer == null ? null : peerColor(peer)} data-first=${row.first ? "" : null} data-last=${row.last ? "" : null} data-fresh=${fresh ? "" : null}
      data-at=${row.pending ? null : row.at} data-key=${row.key} data-in=${out || row.pending ? null : ""}
      data-quote=${quoteOf(row.entry.text) || null}
      data-pending=${row.pending ? "" : null} aria-label=${`${authorName(row.author, names)}, ${clock(row.at)}`}>
      ${row.first && !out && html`<p class="bubble-author" aria-hidden="true">${authorName(row.author, names)}</p>`}
      <div class="bubble-box">${tag && html`<p class="bubble-tag">${tag}</p>`}${body}</div>
      ${row.last && html`<p class="bubble-time" aria-hidden="true">${row.pending ? "Sending…" : clock(row.at)}</p>`}
    </article>`;
}

// The manager writes: three dots, still where the user asked for less motion.
export const Dots = () => html`<span class="typing-dot" aria-hidden="true"></span><span class="typing-dot" aria-hidden="true"></span><span class="typing-dot" aria-hidden="true"></span>`;
export const Typing = ({ name, leaving }) => html`
  <article class="bubble" data-side="in" data-speaker="manager" data-first="" data-last="" role="status" data-leaving=${leaving ? "" : null}>
    <p class="bubble-author" aria-hidden="true">${name}</p>
    <div class="bubble-box typing"><span class="visually-hidden">${name} is typing</span><${Dots} /></div>
  </article>`;

// One task change in a muted line: its task's chip, then its words as plain text ("#2 ready",
// "Approved land #3").
export function StatusLine({ project, text, fresh }) {
  const [, task, rest] = LEADING_TASK.exec(text) ?? [];
  return html`<p class="status-line" data-fresh=${fresh ? "" : null}>${task ? html`<${TaskChip} project=${project} id=${Number(task)} />${rest}` : text}</p>`;
}

// The manager's reply to events: a muted line under its name, in full when it is one sentence,
// else its first sentence and More.
export function QuietReply({ row, name, fresh }) {
  const [open, setOpen] = useState(false);
  // ^ Its first sentence, or its first line when that ends sooner, never cut inside.
  const first = firstSentence(row.text, Infinity);
  const whole = open || first === row.text;
  return html`
    <article class="reply-line" data-speaker="manager" data-fresh=${fresh ? "" : null} data-key=${row.key} data-at=${row.at} aria-label=${`${name}, ${clock(row.at)}`}>
      ${row.first && html`<p class="bubble-author" aria-hidden="true">${name}</p>`}
      <p class="reply-text">${whole ? row.text : first}${!whole && html` <button class="link-action" type="button" onClick=${() => setOpen(true)}>More</button>`}</p>
    </article>`;
}
