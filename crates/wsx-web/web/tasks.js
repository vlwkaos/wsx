// Manager mode on a project: the manager chat as messages with short task chips, what needs
// the owner and what runs in one reservoir over the composer, the `#` task picker, and a
// task's or the project page's sheet over the chat (docs/web-client-tasks.md). Fixed actions
// go straight to wsx.
import { api } from "./api.js";
import { html, Icon, useEffect, useFollowEnd, useLayoutEffect, useRef, useState } from "./lib.js";
import { can, events, leaveFor, readMap, shows, state, touch, update, useStore, writeMap } from "./store.js";
import { Back, toast } from "./ui.js";
import { AttachButton, FileChips, grow, isolate, pasteFiles, release, Suggestions, uploadFiles } from "./chat.js";
import { Bubble, conversation, StatusLine, Typing } from "./bubbles.js";
import { TaskPage } from "./task.js";
import { ProjectPage } from "./page.js";
import { ChecksPage } from "./checks.js";

// Motion only where the user allows it (the no-preference block of app.css); under reduce,
// nothing moves and nothing waits for an animation to end.
const calm = () => matchMedia("(prefers-reduced-motion: reduce)").matches;
const TICK_MS = 3500;
const LEAVE_MS = 200;

// A count that ticks: it is a new element when it changes, which plays its entry.
const Tick = ({ value }) => html`<span class="tick" key=${value}>${value}</span>`;

// `list` with the items that just left kept in place for LEAVE_MS, to play their exit:
// `[{ item, key, leaving }]`.
function useLeaving(list, keyOf) {
  const [ghosts, setGhosts] = useState([]);
  const last = useRef(list);
  useEffect(() => {
    const keys = new Set(list.map(keyOf));
    const gone = calm() ? [] : last.current.filter((item) => !keys.has(keyOf(item))).map((item) => ({ item, key: keyOf(item), at: last.current.indexOf(item), leaving: true }));
    last.current = list;
    setGhosts((now) => {
      const kept = now.filter((entry) => !keys.has(entry.key));
      return kept.length === now.length && !gone.length ? now : [...kept, ...gone];
    });
    if (gone.length) setTimeout(() => setGhosts((now) => now.filter((entry) => !gone.includes(entry))), LEAVE_MS);
  }, [list.map(keyOf).join()]);
  const rows = list.map((item) => ({ item, key: keyOf(item), leaving: false }));
  for (const ghost of ghosts) rows.splice(Math.min(ghost.at, rows.length), 0, ghost);
  return rows;
}

// True while `flag` holds and for LEAVE_MS after, so a thing can fade out.
function useLingering(flag) {
  const [lingers, setLingers] = useState(false);
  const was = useRef(flag);
  useEffect(() => {
    if (was.current && !flag && !calm()) {
      setLingers(true);
      setTimeout(() => setLingers(false), LEAVE_MS);
    }
    was.current = flag;
  }, [flag]);
  return flag || lingers;
}

// Every granted project's overview, or one project's. `missing` reads one project only when
// the read of every project (at the stream's open) ended without it.
let readingAll = null;
export async function loadTasks(project, { missing = false } = {}) {
  try {
    if (project == null) {
      readingAll = api("api/tasks");
      const all = await readingAll;
      all.forEach(noteDecisions);
      update({ tasks: Object.fromEntries(all.map((overview) => [overview.project, overview])) });
    } else {
      if (missing) {
        if (!readingAll) return;
        await readingAll.catch(() => {});
        if (state.tasks[project]) return;
      }
      const overview = await api(`api/tasks/${project}`);
      noteDecisions(overview);
      update({ tasks: { ...state.tasks, [project]: overview } });
    }
  } catch { /* the next tasks event brings it */ }
}

// The three states the owner sees, and the tone that repeats each (docs/web-client-tasks.md);
// the server maps the store's states to them. Working has its own color.
const STAGES = {
  working: ["working", "working"], needs_you: ["needs you", "attention"], done: ["done", "muted"], closed: ["closed", "muted"],
};
export const taskStage = (task) => STAGES[task?.stage] ?? [task?.stage ?? "", "muted"];
export const taskLink = (project, task) => `#/p/${project}/t/${task}`;

// A task the agents work on: its chip and its reservoir row pulse.
export const running = (task) => task?.stage === "working";

// `#12` in any text: the number and a state dot, the state in its accessible name and the
// title on hover. The title shows in the sheet and the reservoir, not in the text.
export function TaskChip({ project, id }) {
  const store = useStore();
  const task = store.tasks[project]?.tasks.find((candidate) => candidate.id === id);
  // ^ A task that stops running settles its dot once instead of snapping from the pulse.
  const live = running(task);
  const was = useRef(live);
  const [settle, setSettle] = useState(false);
  useEffect(() => {
    const stopped = was.current && !live;
    was.current = live;
    if (stopped && !calm()) {
      setSettle(true);
      setTimeout(() => setSettle(false), LEAVE_MS * 3);
    }
  }, [live]);
  if (!task) return html`<a class="task-chip" href=${taskLink(project, id)}>#${id}</a>`;
  const [word, tone] = taskStage(task);
  return html`
    <a class="task-chip" href=${taskLink(project, id)} title=${task.title} aria-label=${`#${id}, ${word}`}>
      <span class="mark" data-tone=${tone} data-running=${live ? "" : null} data-settle=${settle ? "" : null} aria-hidden="true"></span>#${id}
    </a>`;
}

// One text-to-nodes function per project, so the markdown of a message is parsed once:
// `#n` becomes a chip, and "project page" a link to the page.
const chipWords = new Map();
export function withChips(project) {
  if (!chipWords.has(project)) {
    chipWords.set(project, (text) => text.split(/(#\d+\b|\bproject page\b)/i).map((part) =>
      /^#\d+$/.test(part) ? html`<${TaskChip} project=${project} id=${Number(part.slice(1))} />`
        : /^project page$/i.test(part) ? html`<a class="page-link" href=${`#/p/${project}/page`}>${part}</a>` : part));
  }
  return chipWords.get(project);
}

// What a waiting task waits for, in words: "needs to reach crates.io", "waits for disk
// space", "stopped", "needs an answer" (docs/tasks.md "Store").
const waitWords = (item) => item.asks?.length ? `needs to ${askWords(item.asks)}`
  : item.waiting?.on === "disk" ? "waits for disk space"
  : item.waiting?.reason === "stopped by request" ? "stopped" : "needs an answer";
const KIND_TONE = { waiting: "attention", failed: "danger", approval: "attention", land: "accent" };
// One open item in words, as the owner would read it: "#4 needs an answer", "Approve land #3".
export const needWords = (item) => item.kind === "approval" ? item.label
  : item.kind === "waiting" ? `#${item.task} ${waitWords(item)}`
  : `#${item.task} ${item.kind === "land" ? "is ready to land" : "failed"}`;
// What a waiting task asks for, never a command: "reach crates.io and write ~/.cargo"
// (docs/web-client-tasks.md). A grant body names one ask.
export function askWords(asks) {
  const words = asks.map((ask) => ask.kind === "network" ? `reach ${ask.host}` : `write ${ask.path}`);
  return words.length < 2 ? words.join("") : `${words.slice(0, -1).join(", ")} and ${words.at(-1)}`;
}
export const grantBody = (ask, project) => ask.kind === "network" ? { host: ask.host, project } : { path: ask.path, project };
export const needTone = (item) => KIND_TONE[item.kind];
const itemKey = (item) => item.kind === "approval" ? item.approval : `${item.kind}-${item.task}`;

// Drafts and outcomes of actions, by `<project>/<item>`, kept across the cards' re-renders.
const pending = new Map();
const actionOf = (key) => {
  if (!pending.has(key)) pending.set(key, { field: null, text: "", busy: false, error: "" });
  return pending.get(key);
};

// Actions whose outcome the chat already shows ("Approved land #3", "#1 may reach crates.io
// (owner)", the worker's "#3 · done").
const SAID_IN_CHAT = new Set(["approve", "deny", "allow"]);

// What a fixed action did, as a notice: wsx's own answer, unless the chat says it already.
export function announce(name, action, answer) {
  if (SAID_IN_CHAT.has(action) || (action === "land" && answer.state === "landed")) return;
  toast(`${name}: ${answer.message || "done"}`);
}

// An approved action that did not run is a failure the owner acts on: it stays until
// dismissed. Decisions seen before this page loaded say nothing.
const seenDecisions = new Map();
export function noteDecisions(overview) {
  const known = seenDecisions.get(overview.project);
  seenDecisions.set(overview.project, new Set((overview.decisions ?? []).map((decision) => decision.approval)));
  if (!known) return;
  for (const decision of overview.decisions ?? []) {
    if (known.has(decision.approval) || !decision.approved || decision.outcome?.startsWith("done")) continue;
    const what = [decision.action, decision.task != null && `#${decision.task}`].filter(Boolean).join(" ");
    toast(`${what.charAt(0).toUpperCase()}${what.slice(1)} did not run: ${decision.outcome}`, { failure: true });
  }
}

// One fixed action. A success says what wsx did and clears the draft; a failure keeps it.
async function act(project, item, action, body = {}) {
  const path = item.kind === "approval"
    ? `api/tasks/${project}/approvals/${item.approval}/${action}`
    : `api/tasks/${project}/t/${item.task}/${action}`;
  const answer = await api(path, { method: "POST", json: body });
  const name = item.kind === "approval" ? item.label : `#${item.task}`;
  announce(name, action, answer);
  await loadTasks(project);
  return answer;
}

// One open item in the reservoir: its chip (an approval names its task in its label), its
// title and what it waits for, with its one action. The title opens the task's sheet, where the
// other actions are. `onActing` says an action started, `onFailed` that it failed: the
// reservoir moves the focus on when the row leaves, before or after the action's answer.
function NeedRow({ project, item, index, onActing, onFailed, leaving }) {
  const key = `${project}/${itemKey(item)}`;
  const draft = actionOf(key);
  const [, renew] = useState(0);
  const field = useRef();
  const acting = can("task_act");
  const overview = useStore().tasks[project];
  // ^ This task's trial on the live site (docs/tasks.md "Hold and trial").
  const trial = overview?.trial?.task === item.task ? overview.trial : null;
  const refresh = () => renew((count) => count + 1);
  useEffect(() => { if (draft.field) field.current?.focus(); }, [draft.field]);
  const run = async (action, body) => {
    draft.busy = true;
    draft.error = "";
    refresh();
    onActing(itemKey(item), index);
    try {
      await act(project, item, action, body);
      draft.field = null;
      draft.text = "";
    } catch (failure) {
      onFailed(itemKey(item));
      draft.error = failure.message;
    }
    draft.busy = false;
    refresh();
  };
  const open = () => {
    draft.field = draft.field ? null : "answer";
    draft.error = "";
    refresh();
  };
  const submit = (event) => {
    event.preventDefault();
    const text = field.current.value.trim();
    if (text) run("add", { text });
  };
  // ^ One grant per ask; the host resumes the task with the last one.
  const asks = item.kind === "waiting" ? item.asks ?? [] : [];
  const grant = async () => {
    draft.busy = true;
    draft.error = "";
    refresh();
    onActing(itemKey(item), index);
    try {
      for (const ask of asks) await act(project, item, "allow", grantBody(ask, false));
    } catch (failure) {
      onFailed(itemKey(item));
      draft.error = failure.message;
    }
    draft.busy = false;
    refresh();
  };
  // ^ Icons, so the title and what the item waits for keep the row's width; the name is the
  // button's title and, with what it acts on, its accessible name.
  const button = (label, icon, tone, onClick, extra = {}) => html`
    <button class="button icon-button need-action" type="button" title=${label} data-tone=${tone} disabled=${draft.busy}
      onClick=${onClick} ...${extra}><${Icon} name=${icon} /></button>`;
  let actions = null;
  if (acting && asks.length) {
    actions = button("Allow", "pass", "accent", grant, { "aria-label": `Allow #${item.task} to ${askWords(asks)}` });
  } else if (acting && ["waiting", "failed"].includes(item.kind)) {
    actions = button("Answer", "reply", null, open, { "aria-expanded": draft.field === "answer", "aria-label": `Answer #${item.task}` });
  } else if (acting && item.kind === "approval" && draft.running) {
    // ^ The approved action runs until its decision settles the row; it may take minutes.
    actions = html`<span class="row-context" role="status">Running…</span>`;
  } else if (acting && item.kind === "approval") {
    const approve = async () => {
      await run("approve");
      if (!draft.error) { draft.running = true; refresh(); }
    };
    actions = html`${button("Deny", "fail", "danger", () => run("deny"), { "aria-label": `Deny: ${item.label}` })}${button("Approve", "pass", "accent", approve, { "aria-label": item.label })}`;
  } else if (acting && item.kind === "land" && trial?.stage === "live") {
    actions = html`${button("Fail: tell the worker", "reply", "danger", open, { "aria-expanded": draft.field === "answer", "aria-label": `Fail #${item.task}: tell the worker what is wrong` })}${button("Pass and land", "pass", "accent", () => run("pass"), { "aria-label": `Pass #${item.task} and land it` })}`;
  } else if (acting && item.kind === "land" && trial) {
    actions = html`<span class="row-context" role="status">Going live…</span>`;
  } else if (acting && item.kind === "land" && item.hold && overview?.try_live && !overview.trial) {
    actions = html`${button("Try live", "play", null, () => run("trial"), { "aria-label": `Try #${item.task} on the live site` })}${button("Land", "land", "accent", () => run("land"), { "aria-label": `Land #${item.task}` })}`;
  } else if (acting && item.kind === "land") {
    actions = button("Land", "land", "accent", () => run("land"), { "aria-label": `Land #${item.task}` });
  }
  // ^ An approval reads as the action it asks for, never as its id (docs/web-client-tasks.md).
  const context = item.kind === "approval" ? item.title ?? item.commit ?? ""
    : item.kind === "waiting" ? waitWords(item)
    : item.kind === "land" ? (trial?.stage === "live" ? "on the live site: try it" : item.reason) : "failed";
  const label = item.kind === "approval" ? item.label : item.title;
  const body = html`
    <span class="row-label">${label}</span>
    ${context && html`<span class="row-context">${context}</span>`}`;
  return html`
    <li class="need" data-key=${itemKey(item)} data-leaving=${leaving ? "" : null} inert=${Boolean(leaving)}>
      <div class="row-link">
        ${item.task != null && item.kind !== "approval" ? html`<${TaskChip} project=${project} id=${item.task} />`
          : html`<span class="mark" data-tone=${KIND_TONE[item.kind]} aria-hidden="true"></span>`}
        ${item.task != null
          ? html`<a class="row-main row-open" href=${taskLink(project, item.task)}>${body}</a>`
          : html`<span class="row-main">${body}</span>`}
        ${actions && html`<span class="row-actions">${actions}</span>`}
      </div>
      ${draft.field && html`
        <form class="need-form" onSubmit=${submit}>
          <input class="input" ref=${field} type="text" autocomplete="off" value=${draft.text}
            aria-label=${`Answer to #${item.task}`} placeholder="Answer"
            onInput=${(event) => { draft.text = event.target.value; }} disabled=${draft.busy} />
          <button class="button" type="submit" disabled=${draft.busy}>Send</button>
        </form>`}
      ${draft.error && html`<p class="error need-error" role="alert">${draft.error}</p>`}
    </li>`;
}

// What a working task does when it is more than work: a conflict, or the owner's hands.
const workingWords = (task) => task.conflict ? "resolving a conflict" : task.waiting?.on === "hand" ? "by hand" : "";

// A working task in the reservoir: its chip and title; the chip pulses.
const RunningRow = ({ project, task, leaving }) => html`
  <li class="need" data-leaving=${leaving ? "" : null} inert=${Boolean(leaving)}>
    <div class="row-link">
      <${TaskChip} project=${project} id=${task.id} />
      <a class="row-main row-open" href=${taskLink(project, task.id)}><span class="row-label">${task.title}</span>
        ${workingWords(task) && html`<span class="row-context">${workingWords(task)}</span>`}</a>
    </div>
  </li>`;

// The To check list in one row: its count, opening the list's sheet.
const CheckingRow = ({ project, count }) => html`
  <li class="need">
    <div class="row-link">
      <span class="mark" data-tone="attention" aria-hidden="true"></span>
      <a class="row-main row-open" href=${`#/p/${project}/checks`}><span class="row-label">${count} to check</span>
        <span class="row-context">for a person, before a release</span></a>
    </div>
  </li>`;

// The decision list, pinned over the composer (docs/web-client-tasks.md "Project screen"). Closed it
// is one line: "2 need you · #6 <title> · 5 to check", the running slot turning through the
// running tasks. Open, the line gives way to the chevron alone, in the same place, and every
// item that waits for the owner shows in the server's order, each with its one action, then
// the tasks that run and the To check row. Done tasks are not in it, and with nothing in it
// there is no list. A cleared row leaves and the focus moves to the next row's action, or to
// the composer after the last one. It never moves the chat.
function Reservoir({ project, items, tasks, checking }) {
  const [open, setOpen] = useState(false);
  const [closing, setClosing] = useState(false);
  const [paused, setPaused] = useState(false);
  const [turn, setTurn] = useState(0);
  const node = useRef();
  // ^ The row whose action runs: a `tasks` event may remove it long before the answer comes.
  const cleared = useRef(null);
  const working = tasks.filter(running);
  const keys = items.map(itemKey);
  const needRows = useLeaving(items, itemKey);
  const runRows = useLeaving(working, (task) => `run-${task.id}`);
  const expanded = open && !closing;
  // ^ With more than one task running, the closed line turns through them every TICK_MS; it
  // waits while the pointer or the focus is on it and while the page is hidden, and under
  // reduced motion it keeps the newest and counts the rest.
  const rotates = working.length > 1 && !expanded && !calm();
  useEffect(() => {
    if (!rotates || paused) return undefined;
    const timer = setInterval(() => { if (document.visibilityState === "visible") setTurn((count) => count + 1); }, TICK_MS);
    return () => clearInterval(timer);
  }, [rotates, paused]);
  useLayoutEffect(() => {
    const done = cleared.current;
    if (!done || keys.includes(done.key)) return;
    cleared.current = null;
    // ^ Only when the focus is still in the list, or left with the removed button.
    const focus = document.activeElement;
    if (focus && focus !== document.body && !node.current?.contains(focus)) return;
    const next = keys[Math.min(done.index, keys.length - 1)];
    const row = next && node.current?.querySelector(`.need[data-key="${CSS.escape(next)}"]`);
    // ^ The row's action first; its title only when the site may not act.
    const target = row?.querySelector(".need-action") ?? row?.querySelector(".row-open") ?? node.current?.querySelector(".reservoir-head");
    target?.focus({ preventScroll: true });
  }, [keys.join()]);
  // ^ The reservoir goes with the focus in it (its last row, or the closed line while that
  // task still ran): the focus moves to the composer, not to the page body.
  const shown = Boolean(items.length || working.length || checking);
  const wasShown = useRef(shown);
  useLayoutEffect(() => {
    const focus = document.activeElement;
    if (wasShown.current && !shown && (!focus || focus === document.body)) {
      document.querySelector(".chat .composer-field")?.focus({ preventScroll: true });
    }
    wasShown.current = shown;
  }, [shown]);
  if (!shown) return null;
  const onActing = (key, index) => { cleared.current = { key, index }; };
  const onFailed = (key) => { if (cleared.current?.key === key) cleared.current = null; };
  // ^ The newest running task, the one with the highest number, comes first and stays while it works.
  const lineup = [...working].sort((left, right) => right.id - left.id);
  const current = lineup[rotates ? turn % lineup.length : 0];
  const need = items.length ? html`<${Tick} value=${items.length} /> ${items.length === 1 ? "needs" : "need"} you` : "";
  // ^ Closing plays the body's exit, then unmounts it; under reduced motion it is at once.
  const toggle = () => {
    if (closing) setClosing(false);
    else if (open && !calm()) setClosing(true);
    else setOpen(!open);
  };
  const revealed = (event) => {
    if (closing && event.target === event.currentTarget) { setClosing(false); setOpen(false); }
  };
  return html`
    <section class="reservoir" ref=${node} aria-label="Needs you and running" data-open=${expanded ? "" : null}>
      <button class="reservoir-head" type="button" aria-expanded=${expanded} aria-controls="reservoir-list" aria-label=${expanded ? "Hide the list" : null} onClick=${toggle}
        onPointerEnter=${() => setPaused(true)} onPointerLeave=${() => setPaused(false)} onFocus=${() => setPaused(true)} onBlur=${() => setPaused(false)}>
        ${!expanded && html`
          ${items.length || checking ? html`<span class="mark" data-tone="attention" aria-hidden="true"></span>` : null}
          <span class="reservoir-words">
            ${need}
            ${current && html`
              ${need && html`${" "}<span class="reservoir-sep" aria-hidden="true">·</span>${" "}`}
              <span class="ticker-item" key=${current.id}>
                <span class="task-chip"><span class="mark" data-tone="working" data-running="" aria-hidden="true"></span>#${current.id}</span>${" "}
                <span class="reservoir-title">${current.title}</span>
              </span>`}
          </span>
          ${working.length > 1 && html`${" "}<span class="reservoir-more">${rotates ? html`${turn % lineup.length + 1}/${lineup.length}` : `+${working.length - 1} more`}</span>`}
          ${checking ? html`${need || current ? html`${" "}<span class="reservoir-sep" aria-hidden="true">·</span>${" "}` : null}<span class="reservoir-checks"><${Tick} value=${checking} /> to check</span>` : null}`}
        <${Icon} name="chevron" />
      </button>
      ${open && (items.length || working.length || checking) ? html`
        <div class="reservoir-reveal" data-closing=${closing ? "" : null} onAnimationEnd=${revealed}>
          <div class="reservoir-body" id="reservoir-list">
            ${items.length || needRows.length ? html`
              <ul class="reservoir-list" aria-label="Needs you">
                ${needRows.map(({ item, key, leaving }) => html`<${NeedRow} key=${key} project=${project} item=${item} index=${items.indexOf(item)} onActing=${onActing} onFailed=${onFailed} leaving=${leaving} />`)}
              </ul>` : null}
            ${working.length || runRows.length ? html`
              <h2 class="label reservoir-label">Running</h2>
              <ul class="reservoir-list reservoir-running" aria-label="Running">
                ${runRows.map(({ item, key, leaving }) => html`<${RunningRow} key=${key} project=${project} task=${item} leaving=${leaving} />`)}
              </ul>` : null}
            ${checking ? html`<ul class="reservoir-list" aria-label="To check"><${CheckingRow} project=${project} count=${checking} /></ul>` : null}
          </div>
        </div>` : null}
    </section>`;
}

// Unsent manager messages by project, kept on this device.
const DRAFTS_KEY = "wsx-manager-drafts";
const readDrafts = () => readMap(DRAFTS_KEY);
function keepDraft(project, text) {
  const all = readDrafts();
  if (text) all[project] = text;
  else delete all[project];
  writeMap(DRAFTS_KEY, all);
}

// While the word at the caret starts with `#` (a task), `@` (another project) or, as the whole
// text so far, `/` (one of the owner's skills): what it names so far.
function wordQuery(text, caret) {
  const before = text.slice(0, caret);
  const typed = /(?:^|\s)([#@])([\w.-]*)$/.exec(before) ?? /^(\/)([\w:.-]*)$/.exec(before);
  return typed ? { mark: typed[1], query: typed[2].toLowerCase(), start: caret - typed[2].length - 1 } : null;
}
const within = (names, query, pick) => {
  const starts = names.filter((item) => pick(item).toLowerCase().startsWith(query));
  return [...starts, ...names.filter((item) => !starts.includes(item) && pick(item).toLowerCase().includes(query))].slice(0, 50);
};

// The owner's skills and the other projects, read once per project while the page is open.
const mentionLists = new Map();
function loadMentions(project) {
  if (!mentionLists.has(project)) {
    mentionLists.set(project, api(`api/tasks/${project}/mentions`).catch((failure) => {
      mentionLists.delete(project);
      throw failure;
    }));
  }
  return mentionLists.get(project);
}

const LIST_LABELS = { "#": "Tasks", "@": "Projects", "/": "Skills" };
const emptyNote = (mark, tasks, mentions) => mark === "#" ? (tasks.length ? "No task matches that." : "This project has no task yet.")
  : !mentions ? "Loading…"
  : mark === "@" ? (mentions.projects.length ? "No project matches that." : "There is no other project.")
  : (mentions.skills.length ? "No skill matches that." : "You have no skills to lend the manager.");

function ManagerComposer({ project, busy: thinking, onSent, field }) {
  const store = useStore();
  const [text, setText] = useState(() => readDrafts()[project] ?? "");
  const [caret, setCaret] = useState(0);
  const [active, setActive] = useState(0);
  const [closed, setClosed] = useState(null);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  // ^ Files to send with the next message; they are lost on leaving the project.
  const files = useRef({ attachments: [], uploads: [] }).current;
  const [, renew] = useState(0);
  const filesChanged = () => renew((count) => count + 1);
  const tasks = store.tasks[project]?.tasks ?? [];
  const typed = closed === text ? null : wordQuery(text, caret);
  const [mentions, setMentions] = useState(null);
  const mark = typed?.mark;
  useEffect(() => {
    if (mark !== "/" && mark !== "@") return undefined;
    let current = true;
    loadMentions(project).then((value) => { if (current) setMentions(value); }, () => {});
    return () => { current = false; };
  }, [project, mark === "/" || mark === "@"]);
  // ^ `#` lists tasks, `@` the other projects, `/` the owner's skills the manager can use.
  const options = !typed ? null
    : mark === "#" ? tasks.filter((task) => String(task.id).startsWith(typed.query) || task.title.toLowerCase().includes(typed.query)).slice(0, 50).map((task) => ({ task }))
    : mark === "@" ? within(mentions?.projects ?? [], typed.query, (other) => other.name).map((other) => ({ other }))
    : within(mentions?.skills ?? [], typed.query, (skill) => skill.name).map((skill) => ({ skill }));
  const listOpen = Boolean(options);
  useLayoutEffect(() => { field.current.value = text; grow(field.current); }, [project]);
  useLayoutEffect(() => { if (listOpen) document.getElementById(`slash-${active}`)?.scrollIntoView({ block: "nearest" }); }, [active, listOpen]);
  useEffect(() => release, []);
  const changed = () => {
    const node = field.current;
    setText(node.value);
    setCaret(node.selectionStart);
    setActive(0);
    keepDraft(project, node.value);
    grow(node);
  };
  const pick = (option) => {
    const node = field.current;
    // ^ A skill is asked for in words: the manager is headless and takes no slash command.
    const insert = option.task ? `#${option.task.id} ` : option.other ? `@${option.other.name} ` : `Use owner:${option.skill.name}: `;
    node.value = text.slice(0, typed.start) + insert + text.slice(caret);
    const at = typed.start + insert.length;
    node.focus();
    node.setSelectionRange(at, at);
    changed();
  };
  const send = async () => {
    const body = field.current.value.trim();
    if (busy || files.uploads.length || (!body && !files.attachments.length)) return;
    setBusy(true);
    setError("");
    try {
      const attachments = files.attachments.map((item) => item.hash);
      const answer = await api(`api/tasks/${project}/say`, { method: "POST", json: { text: body, attachments } });
      field.current.value = "";
      files.attachments = [];
      changed();
      onSent({ id: answer.id, text: body });
    } catch (failure) {
      // The draft stays in the field; nothing was sent.
      setError(failure.message);
    }
    setBusy(false);
  };
  const keydown = (event) => {
    if (event.isComposing || event.keyCode === 229) return;
    if (listOpen) {
      const move = { ArrowDown: 1, ArrowUp: -1 }[event.key];
      if (move && options.length) {
        event.preventDefault();
        setActive((active + move + options.length) % options.length);
        return;
      }
      if ((event.key === "Tab" || event.key === "Enter") && !event.shiftKey && options[active]) {
        event.preventDefault();
        pick(options[active]);
        return;
      }
      if (event.key === "Escape") {
        event.preventDefault();
        setClosed(text);
        return;
      }
    }
    if (event.key !== "Enter" || event.shiftKey || touch()) return;
    event.preventDefault();
    send();
  };
  const upload = (picked) => { setError(""); uploadFiles(picked, files, filesChanged, setError); };
  const focusField = (event) => {
    if (event.target.closest("button, textarea, input")) return;
    event.preventDefault();
    field.current.focus();
  };
  return html`
    <div class="composer">
      <${FileChips} files=${files} onRemove=${(item) => { files.attachments = files.attachments.filter((other) => other !== item); filesChanged(); }} />
      ${error && html`<p class="error" role="alert">${error}</p>`}
      ${listOpen && html`<${Suggestions} label=${LIST_LABELS[mark]} active=${active} onHover=${setActive} onPick=${pick}
        options=${options.map((option) => option.task ? { key: option.task.id, name: `#${option.task.id}`, description: option.task.title,
          source: taskStage(option.task)[0], tone: taskStage(option.task)[1], ...option }
          : option.other ? { key: option.other.id, name: `@${option.other.name}`, description: option.other.manager ?? "", source: "project", ...option }
          : { key: option.skill.name, name: `/${option.skill.name}`, description: option.skill.description, source: "skill", ...option })}
        notes=${[!options.length && emptyNote(mark, tasks, mentions)]} />`}
      <div class="composer-row" onPointerDown=${focusField}>
        <${AttachButton} onFiles=${upload} />
        <div class="composer-input">
          <textarea class="composer-field" ref=${field} rows="1" aria-label="Message to the manager"
            placeholder="Message · # task @ project / skill" role="combobox" aria-autocomplete="list" aria-expanded=${listOpen}
            aria-controls=${listOpen ? "slash-list" : null} aria-activedescendant=${listOpen && options.length ? `slash-${active}` : null}
            onInput=${changed} onKeyDown=${keydown} onPaste=${pasteFiles(upload)} onKeyUp=${() => setCaret(field.current.selectionStart)}
            onClick=${() => setCaret(field.current.selectionStart)} onFocus=${isolate} onBlur=${release}></textarea>
        </div>
        <button class="button button-primary composer-send" type="button" aria-label=${thinking ? "Queue" : "Send"}
          disabled=${busy || files.uploads.length > 0} onClick=${send}>
          <span class="send-mark" aria-hidden="true"><${Icon} name="send" /></span><span class="send-text">${thinking ? "Queue" : "Send"}</span>
        </button>
      </div>
    </div>`;
}

// When this device last left each project's screen, for "since you left".
const LEFT_KEY = "wsx-project-left";
const readLeft = () => readMap(LEFT_KEY);
function noteLeft(project) {
  const all = readLeft();
  all[project] = Date.now();
  writeMap(LEFT_KEY, all);
}
// Shorter absences are a glance away, not an absence.
const AWAY_MS = 60_000;
const SINCE_WORDS = [["landed", "landed"], ["blocked", "blocked"], ["conflict", "conflicted"], ["revised", "got a new revision"], ["started", "started"]];

// "Since you left": what the task logs say happened while this device was away from the
// project, on opening it or on coming back to it. Nothing runs while away. It goes by itself
// once the newest reply that came while away has been on screen, or the owner sent a message.
// `reply` is the time of the newest message that is not the owner's.
function SinceLine({ project, reply, sent }) {
  const [since, setSince] = useState(null);
  useEffect(() => {
    const ask = () => {
      const left = readLeft()[project];
      if (left && Date.now() - left > AWAY_MS) {
        api(`api/tasks/${project}/since`, { query: { after: left } }).then((data) => setSince({ ...data, left }), () => {});
      }
    };
    const changed = () => (document.visibilityState === "hidden" ? noteLeft(project) : ask());
    ask();
    document.addEventListener("visibilitychange", changed);
    return () => {
      document.removeEventListener("visibilitychange", changed);
      noteLeft(project);
    };
  }, [project]);
  useEffect(() => { if (sent) setSince(null); }, [sent]);
  const left = since?.left;
  useEffect(() => {
    if (!left || !reply || reply <= left) return undefined;
    // ^ Bubbles of one entry share a time: the newest is the last of them.
    const bubble = [...document.querySelectorAll(`.log .bubble[data-side="in"][data-at="${reply}"]`)].at(-1);
    if (!bubble) return undefined;
    const watch = new IntersectionObserver((seen) => { if (seen.some((one) => one.isIntersecting)) setSince(null); }, { root: bubble.closest(".log") });
    watch.observe(bubble);
    return () => watch.disconnect();
  }, [left, reply]);
  if (!since) return null;
  const parts = SINCE_WORDS.filter(([key]) => since.tasks[key]?.length).map(([key, words]) => html`
    <span key=${key}>${since.tasks[key].map((id) => html`<a href=${taskLink(project, id)}>#${id}</a> `)}${words}</span>`);
  if (since.replies) parts.push(html`<span key="replies">${since.replies} ${since.replies === 1 ? "reply" : "replies"}</span>`);
  if (!parts.length) return null;
  return html`
    <div class="since" role="status">
      <p>Since you left: ${parts.map((part, index) => html`${index ? " · " : ""}${part}`)}</p>
      <button class="icon-button" type="button" title="Dismiss" aria-label="Dismiss" onClick=${() => setSince(null)}><${Icon} name="fail" /></button>
    </div>`;
}

// The top row's ⋯: the project page, sessions and files.
function More({ project }) {
  const [open, setOpen] = useState(false);
  const menu = useRef();
  useEffect(() => {
    if (!open) return undefined;
    const outside = (event) => { if (!menu.current?.contains(event.target)) setOpen(false); };
    const escape = (event) => { if (event.key === "Escape") { setOpen(false); menu.current?.querySelector("button")?.focus(); } };
    document.addEventListener("pointerdown", outside);
    document.addEventListener("keydown", escape);
    return () => { document.removeEventListener("pointerdown", outside); document.removeEventListener("keydown", escape); };
  }, [open]);
  const files = shows("files") && can("files_read");
  return html`
    <div class="more" ref=${menu}>
      <button class="icon-button more-button" type="button" aria-label="More" aria-haspopup="menu" aria-expanded=${open}
        onClick=${() => setOpen(!open)}><span aria-hidden="true">⋯</span></button>
      ${open && html`
        <div class="more-menu" role="menu">
          <a role="menuitem" href=${`#/p/${project.id}/page`} onClick=${() => setOpen(false)}>Page</a>
          <a role="menuitem" href=${`#/p/${project.id}/sessions`} onClick=${() => setOpen(false)}>Sessions</a>
          ${files && html`<a role="menuitem" href=${`#/p/${project.id}/files`} onClick=${() => setOpen(false)}>Files</a>`}
        </div>`}
    </div>`;
}

// A task or the project page over the chat, about 85% of the height (a `fit` sheet only as
// high as its content): Back, Escape, a tap
// beside it or a swipe down on its top closes it, and the chat under it keeps its scroll and
// its draft.
function Sheet({ label, fit = false, onClose, children }) {
  const dialog = useRef();
  const drag = useRef(null);
  useEffect(() => {
    const node = dialog.current;
    if (!node.open) node.showModal();
    return () => { if (node.open) node.close(); };
  }, []);
  const cancel = (event) => { event.preventDefault(); onClose(); };
  const backdrop = (event) => { if (event.target === dialog.current) onClose(); };
  const down = (event) => {
    if (event.target.closest("button, a, input, textarea")) return;
    drag.current = { y: event.clientY, id: event.pointerId };
    event.currentTarget.setPointerCapture(event.pointerId);
  };
  const move = (event) => {
    if (drag.current?.id !== event.pointerId) return;
    dialog.current.style.translate = `0 ${Math.max(0, event.clientY - drag.current.y)}px`;
  };
  const up = (event) => {
    if (drag.current?.id !== event.pointerId) return;
    const moved = event.clientY - drag.current.y;
    drag.current = null;
    dialog.current.style.translate = "";
    if (moved > 80) onClose();
  };
  return html`
    <dialog class="task-sheet" data-fit=${fit || undefined} ref=${dialog} aria-label=${label} onCancel=${cancel} onClick=${backdrop}>
      <div class="task-sheet-top" onPointerDown=${down} onPointerMove=${move} onPointerUp=${up} onPointerCancel=${up}>
        <span class="sheet-grip" aria-hidden="true"></span>
      </div>
      ${children({ down, move, up })}
    </dialog>`;
}

// The project screen with task_view: the manager chat as messages, what is open in the
// reservoir, and with a task or the page in the route its sheet over the chat.
export function Manager({ project: id, task, page, checks }) {
  const store = useStore();
  const project = store.projects.find((candidate) => candidate.id === id);
  const overview = store.tasks[id];
  const [chat, setChat] = useState(null);
  const [failure, setFailure] = useState("");
  const [sent, setSent] = useState(null);
  const { log, away, scrolled, toEnd } = useFollowEnd();
  const field = useRef();
  const loading = useRef(false);
  const again = useRef(false);
  const shown = useRef([]);
  const cut = useRef(false);

  // Reads after the last entry it has; one read at a time, one more after a change during it.
  const load = async () => {
    if (loading.current) {
      again.current = true;
      return;
    }
    loading.current = true;
    try {
      do {
        again.current = false;
        const last = shown.current.findLast((entry) => entry.id != null)?.id;
        const next = await api(`api/tasks/${id}/chat`, { query: last != null ? { after: last } : undefined });
        // ^ A rotation or a stop has no id, so every read after the last id returns it again
        // until the next message: it joins once.
        const known = (entry) => entry.id == null
          && shown.current.some((seen) => seen.id == null && seen.kind === entry.kind && seen.unix_ms === entry.unix_ms);
        const all = [...shown.current, ...next.entries.filter((entry) => !known(entry))];
        if (next.skipped || all.length > 400) cut.current = true;
        shown.current = all.slice(-400);
        setChat({ ...next, entries: shown.current });
        setFailure("");
      } while (again.current);
    } catch (error) {
      setFailure(error.message);
    } finally {
      loading.current = false;
    }
  };
  useEffect(() => {
    if (!overview) loadTasks(id, { missing: true });
    load();
    const changed = (event) => { if (event.detail.project === id) load(); };
    events.addEventListener("manager", changed);
    return () => events.removeEventListener("manager", changed);
  }, [id]);
  // Closing the sheet gives the focus back to the composer.
  const sheet = task ?? (page || checks || null);
  const sheetWas = useRef(sheet);
  useEffect(() => {
    if (sheetWas.current && !sheet) field.current?.focus({ preventScroll: true });
    sheetWas.current = sheet;
  }, [sheet]);

  if (!project) {
    return html`
      <div class="page">
        <header class="bar"><${Back} fallback="#/" /></header>
        <p class="empty">${!store.daemon ? "wsx is not running on the host." : store.loaded ? "This project is not available." : "Loading…"}</p>
      </div>`;
  }
  const entries = chat?.entries ?? [];
  const items = overview?.needs ?? [];
  const words = withChips(id);
  const rows = chat ? conversation(entries, overview?.resolutions ?? []) : [];
  // The sent message shows until the log has it.
  if (sent && !entries.some((entry) => entry.kind === "message" && entry.id === sent.id)) {
    const last = rows.at(-1);
    const grouped = last?.kind === "bubble" && last.author === "owner";
    if (grouped) last.last = false;
    rows.push({ kind: "bubble", key: "sending", author: "owner", at: Date.now(), first: !grouped, last: true, pending: true, entry: { text: sent.text } });
  }
  const typing = Boolean(chat?.busy);
  // ^ The typing bubble fades out as the reply replaces it.
  const typingShown = useLingering(typing);
  // ^ Rows that arrive after the first read slide in; the ones of the first read, and the
  // message this device just sent, do not.
  const known = useRef(null);
  const fresh = useRef(new Set());
  if (chat) {
    if (!known.current) known.current = new Set(rows.map((row) => row.key));
    for (const row of rows) {
      if (known.current.has(row.key)) continue;
      known.current.add(row.key);
      if (!(sent && row.entry?.id === sent.id) && row.key !== "sending") fresh.current.add(row.key);
    }
  }
  const names = { manager: chat?.manager.name, peers: chat?.peers };
  const stopped = chat && !chat.running && entries.at(-1)?.kind === "stopped";
  // ^ The manager's state only when it matters: typing or stopped (docs/web-client-tasks.md "Project screen").
  const managerWord = typing ? "typing" : stopped ? "stopped" : "";
  return html`
    <div class="page manager">
      <header class="bar">
        <${Back} fallback="#/" />
        <div class="bar-title">
          <span class="bar-label">${project.name}</span>
          ${managerWord && html`<span class="bar-state" key=${managerWord} data-tone=${typing ? "working" : "danger"}>${typing
            ? html`<span class="typing-dots"><span class="visually-hidden">typing</span><span class="typing-dot" aria-hidden="true"></span><span class="typing-dot" aria-hidden="true"></span><span class="typing-dot" aria-hidden="true"></span></span>`
            : managerWord}</span>`}
        </div>
        <div class="bar-tools"><${More} project=${project} /></div>
      </header>
      <div class="chat">
        <${SinceLine} project=${id} reply=${rows.findLast((row) => row.kind === "bubble" && row.author !== "owner")?.at} sent=${sent} />
        <div class="log-wrap">
          <div class="log" ref=${log} onScroll=${scrolled} role="log" aria-label="Conversation with the manager" tabindex="0">
            ${cut.current ? html`<p class="note">Older messages are not shown.</p>` : null}
            ${chat && !rows.length ? html`<p class="empty">No messages yet. Ask the manager for a task or a status; it answers from the ledger.</p>` : null}
            ${rows.map((row) => row.kind === "bubble" ? html`<${Bubble} key=${row.key} project=${id} row=${row} words=${words} names=${names} fresh=${fresh.current.has(row.key)} />`
              : row.kind === "line" ? html`<${StatusLine} key=${row.key} parts=${row.parts} words=${words} fresh=${fresh.current.has(row.key)} />`
              : html`<${Note} key=${row.key} entry=${row.entry} />`)}
            ${typingShown && html`<${Typing} name=${names.manager} leaving=${!typing} />`}
          </div>
          ${away && html`<button class="jump" type="button" onClick=${toEnd}><${Icon} name="down" />Jump to latest</button>`}
        </div>
        ${failure && html`<p class="error chat-error" role="alert">${failure}</p>`}
        <${Reservoir} project=${id} items=${items} tasks=${overview?.tasks ?? []} checking=${overview?.to_check ?? 0} />
        ${can("task_act") && html`<${ManagerComposer} project=${id} busy=${typing} field=${field}
          onSent=${(message) => { toEnd(); setSent(message); load(); }} />`}
      </div>
      ${task != null && html`
        <${Sheet} key=${`t${task}`} label=${`Task #${task}`} onClose=${() => leaveFor(`#/p/${id}`)}>
          ${(grip) => html`<${TaskPage} key=${task} project=${id} task=${task} onClose=${() => leaveFor(`#/p/${id}`)} grip=${grip} />`}
        </${Sheet}>`}
      ${page && html`
        <${Sheet} key="page" label="Project page" onClose=${() => leaveFor(`#/p/${id}`)}>
          ${(grip) => html`<${ProjectPage} project=${id} onClose=${() => leaveFor(`#/p/${id}`)} grip=${grip} />`}
        </${Sheet}>`}
      ${checks && html`
        <${Sheet} key="checks" label="To check" fit onClose=${() => leaveFor(`#/p/${id}`)}>
          ${(grip) => html`<${ChecksPage} project=${id} onClose=${() => leaveFor(`#/p/${id}`)} grip=${grip} />`}
        </${Sheet}>`}
    </div>`;
}

// A rotation or a stop of the manager, said once.
const Note = ({ entry }) => entry.kind === "rotated"
  ? html`<p class="note">${entry.reason ? `New conversation: ${entry.reason}` : `New conversation after ${entry.tokens.toLocaleString()} tokens`}; the ledger carries the state.</p>`
  : html`<p class="note" data-tone="danger">The manager stopped: ${entry.reason}</p>`;
