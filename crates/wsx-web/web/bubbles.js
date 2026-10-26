// The manager chat as messages: one bubble per owner message and manager reply, each with its
// author and time, and task changes and the manager's replies to them as grouped muted lines
// (docs/web-client-tasks.md "Project screen").
import { clock, html } from "./lib.js";
import { Markdown } from "./markdown.js";
import { Attachments } from "./chat.js";
import { Quoted, quoteOf } from "./reply.js";
import { ChatApprovals } from "./tasks.js";

// Bubbles of one author closer together than this read as one group.
const GROUP_MS = 5 * 60_000;
// Status lines this close together read as one line.
const LINE_MS = 60_000;
// A quiet reply's line: its first line, cut here.
const QUIET_CHARS = 120;

const EVENT_WORDS = { landed: "done" };

// One line written by wsx in its short form: `#3 ready r2: title; summary` → `#3 ready`,
// `approval a3 (land #5) approved; done` → `Approved land #5`, a dropped one with its reason:
// `approval a9 (discard #42) dropped; #42 is closed` → `Dropped discard #42: #42 is closed`.
export function shortLine(line) {
  const task = /^#(\d+) (\w+) r\d+:/.exec(line);
  if (task) return `#${task[1]} ${EVENT_WORDS[task[2]] ?? task[2]}`;
  const decision = /^approval \S+ \((.+?)\) (approved|denied|dropped)\b(?:; (.*))?/.exec(line);
  if (decision?.[2] === "dropped") return `Dropped ${decision[1]}: ${decision[3] ?? ""}`;
  if (decision) return `${decision[2] === "approved" ? "Approved" : "Denied"} ${decision[1]}`;
  return line.split("; ")[0];
}

// The conversation in time order: bubbles and status lines. A wsx line that a resolution
// says too ("#3 landed", "Approved land #3") shows once; lines close together join. A pending
// approval goes into the manager's reply that ended the turn which asked for it
// (docs/web-client-tasks.md "Project screen").
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
      const lines = entry.text.split("\n").filter(Boolean).map(shortLine);
      if (lines.length) rows.push({ kind: "line", at, key, parts: lines });
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
  for (const approval of approvals) {
    const reply = rows.find((row) => row.entry?.kind === "reply" && row.at >= approval.unix_ms);
    if (reply) reply.approvals = [...(reply.approvals ?? []), approval];
  }
  // ^ A reply to a turn of only events is the manager's own work: a muted line, unless it asks
  // for an approval; `-` says nothing changed for the owner (docs/tasks.md "Manager").
  const shown = rows.flatMap((row) => {
    if (!row.quiet || row.approvals) return [row];
    const first = row.entry.text.trim().split("\n")[0].trim();
    if (!first || first === "-") return [];
    const part = first.length > QUIET_CHARS ? `${first.slice(0, QUIET_CHARS - 1)}…` : first;
    return [{ kind: "line", at: row.at, key: row.key, parts: [part] }];
  });
  // ^ Adjacent lines join; adjacent bubbles of one author group (author once, time last).
  const joined = [];
  for (const row of shown) {
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
    <article class="bubble" data-side=${out ? "out" : "in"} data-speaker=${row.author.startsWith("peer:") ? "peer" : row.author}
      data-peer=${row.author.startsWith("peer:") ? peerColor(row.author.slice(5)) : null} data-first=${row.first ? "" : null} data-last=${row.last ? "" : null} data-fresh=${fresh ? "" : null}
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

// Task changes in one muted line: "#2 ready · #4 ready".
export const StatusLine = ({ parts, words, fresh }) => html`
  <p class="status-line" data-fresh=${fresh ? "" : null}>${parts.map((part, index) => html`${index ? " · " : ""}${words(part)}`)}</p>`;
