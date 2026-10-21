// The manager chat as messages: one bubble per owner message, manager reply and worker
// report, each with its author and time, and task changes as grouped muted lines
// (docs/web-client.md "Project screen").
import { clock, firstSentence, html, useLayoutEffect, useRef, useState } from "./lib.js";
import { Markdown } from "./markdown.js";
import { Attachments } from "./chat.js";
import { TaskChip } from "./tasks.js";

// Bubbles of one author closer together than this read as one group.
const GROUP_MS = 5 * 60_000;
// Status lines this close together read as one line.
const LINE_MS = 60_000;

const EVENT_WORDS = { briefed: "brief ready", in_review: "in review", landed: "done" };

// One line written by wsx in its short form: `#3 ready r2: title; summary` → `#3 ready`,
// `approval a3 (land #5) approved; done` → `Approved land #5`.
export function shortLine(line) {
  const task = /^#(\d+) (\w+) r\d+:/.exec(line);
  if (task) return `#${task[1]} ${EVENT_WORDS[task[2]] ?? task[2]}`;
  const decision = /^approval \S+ \((.+?)\) (approved|denied)\b/.exec(line);
  if (decision) return `${decision[2] === "approved" ? "Approved" : "Denied"} ${decision[1]}`;
  return line.split("; ")[0];
}

// The conversation in time order: bubbles and status lines. A wsx line that a resolution
// says too ("#3 landed", "Approved land #3") shows once; lines close together join.
export function conversation(entries, resolutions) {
  const rows = [];
  entries.forEach((entry, index) => {
    const at = entry.unix_ms ?? 0;
    const key = entry.id != null ? `${entry.kind}-${entry.id}` : `n${index}`;
    if (entry.kind === "message" && entry.from === "owner") {
      rows.push({ kind: "bubble", at, key, author: "owner", entry });
    } else if (entry.kind === "reply") {
      rows.push({ kind: "bubble", at, key, author: "manager", entry });
    } else if (entry.kind === "message" && /^manager(-reply)?:/.test(entry.from ?? "")) {
      // ^ Another project's manager wrote (docs/tasks.md "Managers talk").
      const [from, peer] = entry.from.split(/:(.*)/s);
      rows.push({ kind: "bubble", at, key, author: `peer:${peer}`, entry, tag: from === "manager-reply" ? "Reply" : null });
    } else if (entry.kind === "sent") {
      rows.push({ kind: "bubble", at, key, author: "manager", entry, to: entry.to });
    } else if (entry.kind === "message") {
      // ^ The status lines first, so they join the settled lines just before them.
      const reports = entry.reports ?? [];
      const reported = new Set(reports.map((report) => report.task));
      const lines = entry.text.split("\n").filter(Boolean)
        .filter((line) => !reported.has(Number(/^#(\d+) /.exec(line)?.[1])))
        .map(shortLine);
      if (lines.length) rows.push({ kind: "line", at, key, parts: lines });
      for (const report of reports) {
        rows.push({ kind: "bubble", at, key: `${key}-r${report.task}`, author: `worker-${report.task}`, report });
      }
    } else if (entry.kind === "rotated" || entry.kind === "stopped") {
      rows.push({ kind: "note", at, key, entry });
    }
  });
  const said = rows.filter((row) => row.kind === "line");
  resolutions.forEach((resolution, index) => {
    const near = (row) => Math.abs(row.at - resolution.unix_ms) < 10 * LINE_MS;
    if (said.some((row) => near(row) && row.parts.includes(resolution.text))) return;
    rows.push({ kind: "line", at: resolution.unix_ms, key: `res-${index}-${resolution.unix_ms}`, parts: [resolution.text] });
  });
  rows.sort((left, right) => left.at - right.at);
  // ^ Adjacent lines join; adjacent bubbles of one author group (author once, time last).
  const joined = [];
  for (const row of rows) {
    const last = joined.at(-1);
    if (row.kind === "line" && last?.kind === "line" && row.at - last.at < LINE_MS) {
      last.parts = [...last.parts, ...row.parts.filter((part) => !last.parts.includes(part))];
      last.at = row.at;
      continue;
    }
    joined.push({ ...row });
  }
  joined.forEach((row, index) => {
    if (row.kind !== "bubble") return;
    const before = joined[index - 1];
    const after = joined[index + 1];
    const same = (other, gap) => other?.kind === "bubble" && other.author === row.author && gap < GROUP_MS;
    row.first = !same(before, row.at - (before?.at ?? 0));
    row.last = !same(after, (after?.at ?? 0) - row.at);
  });
  return joined;
}

// A done task's report: its first sentence, and the whole summary on Show all.
function Sentence({ id, text, words }) {
  const [open, setOpen] = useState(false);
  const sentence = firstSentence(text);
  return html`
    <div class="bubble-body" id=${id}>
      ${open ? html`<${Markdown} text=${text} words=${words} prose />` : html`<p class="prose">${words(sentence)}</p>`}
    </div>
    ${sentence !== text.trim() && html`
      <button class="link-action bubble-more" type="button" aria-expanded=${open} aria-controls=${id} onClick=${() => setOpen(!open)}>
        ${open ? "Show less" : "Show all"}
      </button>`}`;
}

// A long message folds after about twelve lines; "Show all" opens it.
function Fold({ id, children }) {
  const body = useRef();
  const [open, setOpen] = useState(false);
  const [long, setLong] = useState(false);
  useLayoutEffect(() => {
    if (!open) setLong(body.current.scrollHeight > body.current.clientHeight + 1);
  });
  return html`
    <div class="bubble-body" id=${id} ref=${body} data-clamped=${!open && long ? "" : undefined} data-open=${open ? "" : undefined}>${children}</div>
    ${(long || open) && html`
      <button class="link-action bubble-more" type="button" aria-expanded=${open} aria-controls=${id} onClick=${() => setOpen(!open)}>
        ${open ? "Show less" : "Show all"}
      </button>`}`;
}

// `names`: the manager's name ("Manager" until `tasks.manager.name` is set) and other
// projects' set names; a peer without one reads "Manager <project>" (docs/tasks.md "Config").
const peerName = (names, project) => names?.peers?.[project] ?? `Manager ${project}`;
const authorName = (author, names) => author === "owner" ? "You" : author === "manager" ? names?.manager ?? "Manager"
  : author.startsWith("peer:") ? peerName(names, author.slice(5)) : `#${author.slice(7)} worker`;

// One message. `words` turns its text runs into task chips.
export function Bubble({ project, row, words, names }) {
  const out = row.author === "owner";
  const id = `bubble-${row.key}`;
  const tag = row.to != null ? `${row.entry.reply ? "Reply to" : "To"} ${peerName(names, row.to)}` : row.tag;
  let body;
  if (row.report) {
    const { report } = row;
    body = html`
      <p class="bubble-head"><${TaskChip} project=${project} id=${report.task} />${" "}<span class="bubble-state">· ${report.head}</span></p>
      ${report.state === "landed" ? html`<${Sentence} id=${id} text=${report.summary} words=${words} />`
        : html`<${Fold} id=${id}><${Markdown} text=${report.summary} words=${words} prose /></${Fold}>`}`;
  } else if (out) {
    body = html`<${Fold} id=${id}><p class="prose">${words(row.entry.text)}</p></${Fold}><${Attachments} files=${row.entry.attachments} />`;
  } else {
    body = html`<${Fold} id=${id}><${Markdown} text=${row.entry.text} words=${words} prose /></${Fold}>`;
  }
  return html`
    <article class="bubble" data-side=${out ? "out" : "in"} data-first=${row.first ? "" : null} data-last=${row.last ? "" : null}
      data-pending=${row.pending ? "" : null} aria-label=${`${authorName(row.author, names)}, ${clock(row.at)}`}>
      ${row.first && !out && html`<p class="bubble-author" aria-hidden="true">${authorName(row.author, names)}</p>`}
      <div class="bubble-box">${tag && html`<p class="bubble-tag">${tag}</p>`}${body}</div>
      ${row.last && html`<p class="bubble-time" aria-hidden="true">${row.pending ? "Sending…" : clock(row.at)}</p>`}
    </article>`;
}

// The manager writes: three dots, still where the user asked for less motion.
export const Typing = ({ name = "Manager" }) => html`
  <article class="bubble" data-side="in" data-first="" data-last="" role="status">
    <p class="bubble-author" aria-hidden="true">${name}</p>
    <div class="bubble-box typing"><span class="visually-hidden">${name} is typing</span>
      <span class="typing-dot" aria-hidden="true"></span><span class="typing-dot" aria-hidden="true"></span><span class="typing-dot" aria-hidden="true"></span>
    </div>
  </article>`;

// Task changes in one muted line: "#2 ready · #4 ready".
export const StatusLine = ({ parts, words }) => html`
  <p class="status-line">${parts.map((part, index) => html`${index ? " · " : ""}${words(part)}`)}</p>`;
