// The manager chat as messages: one bubble per owner message and manager reply, each with its
// author and time, task changes as muted lines, one each, and the manager's replies to them as
// lines under its name (docs/web-client-tasks.md "Project screen").
import { clock, firstSentence, html, Icon, useState } from "./lib.js";
import { Markdown } from "./markdown.js";
import { Attachments } from "./chat.js";
import { Quote } from "./reply.js";
import { NeedTags, TaskChip, withChips } from "./tasks.js";
import { CheckRow } from "./checks.js";
import { can, sessionLink, shows } from "./store.js";

// Bubbles of one author closer together than this read as one group.
const GROUP_MS = 5 * 60_000;
// A resolution this close to an event line that says it too shows once.
const NEAR_MS = 10 * 60_000;

const CLOSED = /^#\d+ was closed\b/;

// ^ A line's leading task: the chip, and the words after it (wsx-core `chat_rows` sets `task`
// only on a line that starts with it).
const leading = (row) => row.task != null && row.text.startsWith(`#${row.task} `) ? row.text.slice(`#${row.task}`.length) : null;

// The conversation in time order: the chat route's rows, which wsx-core `chat_rows` classifies
// (kind, author and name, tag, short event lines, quiet replies, asks, post, quote), with the
// resolutions, the To check items at the time each was added and the pending approvals each
// reply carries. A resolution that an event line says too ("Approved land #3", or "#3 closed"
// and "#3 was closed") shows once, as the event line (docs/web-client-tasks.md "Project screen").
// `asks`: the ids of the open questions in the newest read (wsx-core `open_asks`), also for
// rows an earlier read brought.
export function conversation(entries, resolutions, approvals = [], checks = [], asks = []) {
  const rows = entries.map((row) => ({
    ...row, at: row.unix_ms, asks: row.id != null && asks.includes(row.id),
    approvals: row.approvals && approvals.filter((item) => row.approvals.includes(item.approval)),
  }));
  const said = rows.filter((row) => row.kind === "line");
  resolutions.forEach((resolution) => {
    const near = (row) => Math.abs(row.at - resolution.unix_ms) < NEAR_MS;
    // ^ A close is a task change the events say too ("#7 closed", "#7 done"): the event line
    // about that task stays. An answer or a failed check has no event line of its own.
    const closed = CLOSED.test(resolution.text);
    const same = (row) => row.text === resolution.text || (closed && row.task === resolution.task);
    if (said.some((row) => near(row) && same(row))) return;
    rows.push({ kind: "line", at: resolution.unix_ms, key: `res-${resolution.unix_ms}-${resolution.text}`, text: resolution.text, task: resolution.task });
  });
  for (const item of checks) rows.push({ kind: "check", at: item.added_unix_ms, key: `check-${item.id}`, item });
  rows.sort((left, right) => left.at - right.at);
  // ^ Adjacent bubbles of one author group (author once, time last); quiet replies in a row
  // name the manager once.
  rows.forEach((row, index) => {
    const before = rows[index - 1];
    const after = rows[index + 1];
    if (row.kind === "quiet") row.first = !(before?.kind === "quiet" && row.at - before.at < GROUP_MS);
    if (row.kind !== "message") return;
    const same = (other, gap) => other?.kind === "message" && other.author === row.author && gap < GROUP_MS;
    row.first = !same(before, row.at - (before?.at ?? 0));
    row.last = !same(after, (after?.at ?? 0) - row.at);
  });
  return folded(rows);
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
      const rest = row.kind === "line" ? leading(row) : null;
      if (rest != null) chips.set(String(row.task), rest);
    }
    if (run[tail].kind === "quiet") run[tail].first = true;
    // ^ Keyed by the run's first row: a run that grows keeps its row and whether it is open.
    out.push({ kind: "fold", key: `fold-${run[0].key}`, at: run[tail - 1].at, rows: run.slice(0, tail), chips: [...chips] }, ...run.slice(tail));
  }
  return out;
}

// A folded run: one tap shows its rows under it, rendered by `render`, another folds them.
// `all`: open while a find runs, so its rows are searched (find.js).
export function Fold({ row, render, all = false }) {
  const [opened, setOpen] = useState(false);
  const open = opened || all;
  return html`
    <div class="fold" data-key=${row.key} data-at=${row.at}>
      <button class="fold-toggle" type="button" aria-expanded=${open} onClick=${() => setOpen(!open)}>
        ${row.chips.map(([task, rest]) => html`<span class="fold-chip" key=${task}><span class="fold-task">#${task}</span>${rest}</span>`)}
        <span class="fold-count">${open ? "Hide " : ""}${row.rows.length} updates</span>
      </button>
      ${open && row.rows.map(render)}
    </div>`;
}

// ^ Another project's manager keeps one of the `color-peer-N` colors, from its project
// (docs/web-themes.md "Tokens").
const PEER_COLORS = 4;
const peerColor = (project) => {
  let hash = 0;
  for (const char of project) hash = (hash * 31 + char.codePointAt(0)) >>> 0;
  return hash % PEER_COLORS + 1;
};

// A relative link in a reply opens a page: `files/<path>` (the session news, a manager's message about a routine) the project's files
// page at that file, for a reader who may read files; `session/<id>` (a session that needs the
// owner, docs/tasks.md "Session news") that session. One function per project, so the parse is reused.
const relatives = new Map();
const relativeOf = (project) => {
  if (!relatives.has(project)) {
    relatives.set(project, (href) => {
      if (href.startsWith("files/")) return shows("files") && can("files_read") ? `#/p/${project}/files/${href.slice(6)}` : null;
      const session = /^session\/(\d+)$/.exec(href);
      return session ? sessionLink({ id: Number(session[1]), agent: {} }) : null;
    });
  }
  return relatives.get(project);
};

// One message. `home` turns its text runs into task chips; a message of another project's
// manager (not a role) reads its bare `#n` as that project's task.
export function Bubble({ project, row, words: home, fresh, onNeed }) {
  const out = row.author === "owner";
  const peer = row.author.startsWith("peer:") ? row.author.slice(5) : null;
  const words = peer != null && !peer.startsWith("role:") ? withChips(project, peer) : home;
  const id = `bubble-${row.key}`;
  // ^ A post with a level (`wsx task post --level warn|bad`) says it in its tag (docs/web-client-tasks.md "Task notifications").
  const tag = row.level ? (row.level === "bad" ? "Bad" : "Warn") : row.tag;
  // ^ In a chat's small space the stand-in mark is an icon; an asking reply is set off by its border, and its words are in the aria-label.
  const mark = row.provisional ? ["unchecked", "Stand-in, not checked"] : null;
  // ^ A message quotes an earlier one in the owner's format (docs/tasks.md "Manager").
  const quoted = row.quoted != null && html`<${Quote} quote=${row.quoted} id=${row.quoted_id} />`;
  const body = out
    ? html`<div class="bubble-body" id=${id}>${quoted}${row.text && html`<p class="prose">${words ? words(row.text) : row.text}</p>`}</div><${Attachments} files=${row.attachments} />`
    : html`<div class="bubble-body" id=${id}>${quoted}<${Markdown} text=${row.text} words=${words} relative=${relativeOf(project)} prose /></div>
      ${onNeed && row.approvals?.length > 0 && html`<${NeedTags} items=${row.approvals} onShow=${onNeed} />`}`;
  return html`
    <article class="bubble" data-side=${out ? "out" : "in"} data-speaker=${peer == null ? row.author : "peer"}
      data-peer=${peer == null ? null : peerColor(peer)} data-first=${row.first ? "" : null} data-last=${row.last ? "" : null} data-fresh=${fresh ? "" : null}
      data-at=${row.pending ? null : row.at} data-key=${row.key} data-in=${out || row.pending ? null : ""}
      data-quote=${row.quote ?? null} data-id=${row.quote != null ? row.id ?? null : null}
      data-pending=${row.pending ? "" : null} data-asks=${row.asks ? "" : null} data-routine=${row.post ? "" : null} data-level=${row.level ?? null}
      aria-label=${`${row.name}, ${clock(row.at)}${row.asks ? ", Asks you" : row.level ? `, ${tag}` : row.post ? ", Routine" : ""}`}>
      ${row.first && !out && html`<p class="bubble-author" aria-hidden="true">${row.name}</p>`}
      <div class="bubble-box">${mark && html`<p class="bubble-tag" role="img" aria-label=${mark[1]} title=${mark[1]}><${Icon} name=${mark[0]} /></p>`}${tag && html`<p class="bubble-tag">${tag}</p>`}${body}</div>
      ${row.last && html`<p class="bubble-time" aria-hidden="true">${row.pending ? "Sending…" : clock(row.at)}</p>`}
    </article>`;
}

// A To check item in the chat: from the manager, with what to check, its capture and Try, Fail,
// Skip and Pass, or its result once decided (checks.js `CheckRow`). It counts as a new message.
export function CheckBubble({ project, row, names, fresh, onMention }) {
  return html`
    <article class="bubble check-bubble" data-side="in" data-speaker="manager" data-first="" data-last=""
      data-fresh=${fresh ? "" : null} data-at=${row.at} data-key=${row.key} data-in=""
      aria-label=${`${names.manager}, to check, ${clock(row.at)}`}>
      <p class="bubble-author" aria-hidden="true">${names.manager}</p>
      <div class="bubble-box"><p class="bubble-tag">To check</p><${CheckRow} project=${project} item=${row.item} onMention=${onMention} tag="div" /></div>
      <p class="bubble-time" aria-hidden="true">${clock(row.at)}</p>
    </article>`;
}

// The manager writes: three dots, still where the user asked for less motion.
export const Dots = () => html`<span class="typing-dot" aria-hidden="true"></span><span class="typing-dot" aria-hidden="true"></span><span class="typing-dot" aria-hidden="true"></span>`;
export const Typing = ({ name }) => html`
  <article class="bubble" data-side="in" data-speaker="manager" data-first="" data-last="" role="status">
    <p class="bubble-author" aria-hidden="true">${name}</p>
    <div class="bubble-box typing"><span class="visually-hidden">${name} is typing</span><${Dots} /></div>
  </article>`;

// One task change in a muted line: its task's chip, then its words as plain text ("#2 ready",
// "Approved land #3").
export function StatusLine({ project, row, fresh }) {
  const rest = leading(row);
  return html`<p class="status-line" data-fresh=${fresh ? "" : null}>${rest != null ? html`<${TaskChip} project=${project} id=${row.task} />${rest}` : row.text}</p>`;
}

// The manager's reply to events: a muted line under its name, in full when it is one sentence,
// else its first sentence and More.
export function QuietReply({ row, fresh }) {
  const [open, setOpen] = useState(false);
  // ^ Its first sentence, or its first line when that ends sooner, never cut inside.
  const first = firstSentence(row.text, Infinity);
  const whole = open || first === row.text;
  return html`
    <article class="reply-line" data-speaker="manager" data-fresh=${fresh ? "" : null} data-key=${row.key} data-at=${row.at} aria-label=${`${row.name}, ${clock(row.at)}`}>
      ${row.first && html`<p class="bubble-author" aria-hidden="true">${row.name}</p>`}
      <p class="reply-text">${whole ? row.text : first}${!whole && html` <button class="link-action" type="button" onClick=${() => setOpen(true)}>More</button>`}</p>
    </article>`;
}
