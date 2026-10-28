// Manager mode on a project: the manager chat as messages with short task chips, what needs
// the owner and what runs in one reservoir over the composer, the `#` task picker, and a
// task's or the project page's sheet over the chat (docs/web-client-tasks.md). Fixed actions
// go straight to wsx.
import { api } from "./api.js";
import { html, Icon, keepIn, readMap, useCoalescedLoad, useEffect, useFollowEnd, useLayoutEffect, useRef, useState } from "./lib.js";
import { can, leaveFor, shows, state, touch, update, useEventLoad, useStore } from "./store.js";
import { Back, MoreMenu, toast } from "./ui.js";
import { AttachButton, FileChips, grow, isolate, JumpButton, pasteFiles, release, Suggestions, uploadFiles, useRecent, withNewLine } from "./chat.js";
import { Bubble, CheckBubble, conversation, Dots, Fold, QuietReply, StatusLine, Typing } from "./bubbles.js";
import { QuoteChip, useReply, withQuote } from "./reply.js";
import { readTaskAhead, TaskPage } from "./task.js";
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

// True for `ms` after `flag` stops holding, so a thing can play its exit.
function useAfter(flag, ms) {
  const [after, setAfter] = useState(false);
  const was = useRef(flag);
  useEffect(() => {
    if (was.current && !flag && !calm()) {
      setAfter(true);
      setTimeout(() => setAfter(false), ms);
    }
    was.current = flag;
  }, [flag]);
  return after;
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
const taskLink = (project, task) => `#/p/${project}/t/${task}`;

// A task the agents work on: its chip and its reservoir row pulse.
export const running = (task) => task?.stage === "working";

// `#12` in any text: the number and a state dot, the state in its accessible name and the
// title on hover. The title shows in the sheet and the reservoir, not in the text.
export function TaskChip({ project, id }) {
  const store = useStore();
  const task = store.tasks[project]?.tasks.find((candidate) => candidate.id === id);
  if (!task) return html`<a class="task-chip" href=${taskLink(project, id)}>#${id}</a>`;
  return html`
    <a class="task-chip" href=${taskLink(project, id)} title=${task.title} aria-label=${`#${id}, ${taskStage(task)[0]}`}>
      <${StateMark} task=${task} />#${id}
    </a>`;
}

// A task's state dot. A task that stops running settles its dot once instead of snapping from the pulse.
function StateMark({ task }) {
  const live = running(task);
  const settle = useAfter(live, LEAVE_MS * 3);
  return html`<span class="mark" data-tone=${taskStage(task)[1]} data-running=${live ? "" : null} data-settle=${settle ? "" : null} aria-hidden="true"></span>`;
}

// A task in a list: its title first, then its number, small and muted, never cut. `state` is
// for a screen reader, where the row's dot shows it.
export const TaskTitle = ({ title, id, state }) => html`
  <span class="row-title"><span class="row-label">${title}</span><span class="task-number">#${id}</span>${state && html`<span class="visually-hidden">, ${state}</span>`}</span>`;

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

// What a waiting task waits for, in words: "waits for disk
// space", "waits for the session limit", "stopped", "needs an answer" (docs/tasks.md "Store").
const waitWords = (item) => item.waiting?.on === "disk" ? "waits for disk space"
  : item.waiting?.on === "limit" ? "waits for the session limit"
  : item.waiting?.reason === "stopped by request" ? "stopped" : "needs an answer";
const KIND_TONE = { waiting: "attention", failed: "danger", approval: "attention", land: "accent" };
// One open item in words, as the owner would read it: "#4 needs an answer", "Approve land #3".
export const needWords = (item) => item.kind === "approval" ? item.label
  : item.kind === "waiting" ? `#${item.task} ${waitWords(item)}`
  : `#${item.task} ${item.kind === "land" ? "is ready to land" : "failed"}`;
export const needTone = (item) => KIND_TONE[item.kind];
const itemKey = (item) => item.kind === "approval" ? item.approval : `${item.kind}-${item.task}`;

// Drafts and outcomes of actions, by `<project>/<item>`, kept across the cards' re-renders.
const pending = new Map();
const actionOf = (key) => {
  if (!pending.has(key)) pending.set(key, { field: null, text: "", busy: false, error: "" });
  return pending.get(key);
};

// Actions whose outcome the chat already shows ("Approved land #3", the worker's "#3 · done").
const SAID_IN_CHAT = new Set(["approve", "deny"]);

// What a fixed action did, as a notice: wsx's own answer, unless the chat says it already.
export function announce(name, action, answer) {
  if (SAID_IN_CHAT.has(action) || (action === "land" && answer.state === "landed")) return;
  toast(`${name}: ${answer.message || "done"}`);
}

// An approved action that did not run is a failure the owner acts on: it stays until
// dismissed. Decisions seen before this page loaded say nothing, and neither does one the
// owner gave in the chat: the manager runs that action itself and replies with its result.
const seenDecisions = new Map();
export function noteDecisions(overview) {
  const known = seenDecisions.get(overview.project);
  seenDecisions.set(overview.project, new Set((overview.decisions ?? []).map((decision) => decision.approval)));
  if (!known) return;
  for (const decision of overview.decisions ?? []) {
    if (known.has(decision.approval) || !decision.approved || decision.owner_said != null
      || decision.outcome?.startsWith("done")) continue;
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

// The approvals a manager reply asked for, with Deny and Approve in the bubble: one tap. The
// decision settles the request, which leaves the bubble and the reservoir.
export function ChatApprovals({ project, items }) {
  const [busy, setBusy] = useState(null);
  if (!can("task_act")) return null;
  const run = async (item, action) => {
    setBusy(item.approval);
    try {
      await act(project, item, action);
    } catch (failure) {
      toast(`${item.label}: ${failure.message}`, { failure: true });
    }
    setBusy(null);
  };
  return items.map((item) => html`
    <div class="bubble-approval" key=${item.approval} role="group" aria-label=${item.label}>
      <span>${item.label}</span>
      <button class="button need-action" type="button" data-tone="danger" disabled=${busy === item.approval}
        onClick=${() => run(item, "deny")} aria-label=${`Deny: ${item.label}`}>Deny</button>
      <button class="button need-action" type="button" data-tone="accent" disabled=${busy === item.approval}
        onClick=${() => run(item, "approve")} aria-label=${item.label}>${busy === item.approval ? "Running…" : "Approve"}</button>
    </div>`);
}

// One open item in the reservoir: its state dot, its title and number (an approval names its
// task in its label) and what it waits for, with its one action. The title opens the task's sheet, where the
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
  // ^ Icons, so the title and what the item waits for keep the row's width; the name is the
  // button's title and, with what it acts on, its accessible name.
  const button = (label, icon, tone, onClick, extra = {}) => html`
    <button class="button icon-button need-action" type="button" title=${label} data-tone=${tone} disabled=${draft.busy}
      onClick=${onClick} ...${extra}><${Icon} name=${icon} /></button>`;
  let actions = null;
  if (acting && ["waiting", "failed"].includes(item.kind)) {
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
  } else if (acting && item.kind === "land") {
    actions = button("Land", "land", "accent", () => run("land"), { "aria-label": `Land #${item.task}` });
  }
  // ^ An approval reads as the action it asks for, never as its id (docs/web-client-tasks.md).
  const context = item.kind === "approval" ? item.title ?? item.commit ?? ""
    : item.kind === "waiting" ? waitWords(item)
    : item.kind === "land" ? (trial?.stage === "live" ? "on the live site: try it" : item.reason) : "failed";
  // ^ An approval names its task in its label ("Approve land #3").
  const task = item.kind === "approval" ? null : overview?.tasks.find((candidate) => candidate.id === item.task);
  const body = html`
    ${item.kind === "approval" ? html`<span class="row-label">${item.label}</span>` : html`<${TaskTitle} title=${item.title} id=${item.task} state=${task && taskStage(task)[0]} />`}
    ${context && html`<span class="row-context">${context}</span>`}`;
  return html`
    <li class="need" data-key=${itemKey(item)} data-leaving=${leaving ? "" : null} inert=${Boolean(leaving)}>
      <div class="row-link">
        ${task ? html`<${StateMark} task=${task} />` : html`<span class="mark" data-tone=${KIND_TONE[item.kind]} aria-hidden="true"></span>`}
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

// What a working task does, in one line: a conflict, the owner's hands, the checks, or the
// workflow step it is on (docs/tasks.md "Workflow").
const workingWords = (task) => task.conflict ? "resolving a conflict" : task.waiting?.on === "hand" ? "by hand"
  : task.waiting?.on === "check_slot" ? "waiting to run checks" : task.state === "checking" ? "running checks" : task.step ?? "";

// A working task in the reservoir: its pulsing dot, title and number.
const RunningRow = ({ project, task, leaving }) => html`
  <li class="need" data-leaving=${leaving ? "" : null} inert=${Boolean(leaving)}>
    <div class="row-link">
      <${StateMark} task=${task} />
      <a class="row-main row-open" href=${taskLink(project, task.id)}><${TaskTitle} title=${task.title} id=${task.id} state=${taskStage(task)[0]} />
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
// running tasks. Open, the line gives way to the chevron alone, at the right end of the list's
// first row (app.css), and every item that waits for the owner shows in the server's order, each with its one action, then
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
  // ^ Open, each item's task is read ahead, so its sheet shows at once.
  useEffect(() => {
    if (open) for (const item of items) if (item.task != null) readTaskAhead(project, item.task);
  }, [open, keys.join()]);
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
            ${working.length || runRows.length ? html`
              <div class="reservoir-fixed">
                <h2 class="label reservoir-label">Running</h2>
                <ul class="reservoir-list reservoir-running" aria-label="Running">
                  ${runRows.map(({ item, key, leaving }) => html`<${RunningRow} key=${key} project=${project} task=${item} leaving=${leaving} />`)}
                </ul>
              </div>` : null}
            <div class="reservoir-scroll">
              ${items.length || needRows.length ? html`
                <ul class="reservoir-list" aria-label="Needs you">
                  ${needRows.map(({ item, key, leaving }) => html`<${NeedRow} key=${key} project=${project} item=${item} index=${items.indexOf(item)} onActing=${onActing} onFailed=${onFailed} leaving=${leaving} />`)}
                </ul>` : null}
              ${checking ? html`<ul class="reservoir-list" aria-label="To check"><${CheckingRow} project=${project} count=${checking} /></ul>` : null}
            </div>
          </div>
        </div>` : null}
    </section>`;
}

// Unsent manager messages by project, kept on this device.
const DRAFTS_KEY = "wsx-manager-drafts";

// While the word at the caret starts with `#` (a task), `@` (another project) or, as the whole
// text so far, `/` (one of the owner's skills): what it names so far.
function wordQuery(text, caret) {
  const before = text.slice(0, caret);
  const typed = /(?:^|\s)([#@])([\w.-]*)$/.exec(before) ?? /(?:^|\s)(\/)([\w:.-]*)$/.exec(before);
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
const emptyNote = (mark, tasks, mentions, failed, retry) => mark === "#" ? (tasks.length ? "No task matches that." : "This project has no task yet.")
  : failed ? html`<span class="slash-failed" role="alert">${failed}</span>
    <button class="link-action" type="button" onPointerDown=${(event) => event.preventDefault()} onClick=${retry}>Retry</button>`
  : !mentions ? "Loading…"
  : mark === "@" ? (mentions.projects.length ? "No project matches that." : "There is no other project.")
  : (mentions.skills.length ? "No skill matches that." : "You have no skills to lend the manager.");

// The `#` picker's tasks: the title first, the number after it. A task done or closed more than
// a day ago folds away until the text after `#` names it (docs/web-client-tasks.md "Project screen").
const DAY_MS = 86_400_000;
const settled = (task) => ["done", "closed"].includes(task.stage) && Date.now() - task.updated_unix_ms > DAY_MS;
const taskOptions = (tasks, query) => tasks
  .filter((task) => (query || !settled(task)) && (String(task.id).startsWith(query) || task.title.toLowerCase().includes(query))).slice(0, 50)
  .map((task) => ({ key: task.id, name: task.title, number: `#${task.id}`, source: taskStage(task)[0], tone: taskStage(task)[1], insert: `#${task.id} ` }));
const foldedNote = (tasks, query) => {
  const folded = query ? 0 : tasks.filter(settled).length;
  return folded ? `${folded} done more than a day ago: type a number or a word to find ${folded === 1 ? "it" : "them"}.` : null;
};

function ManagerComposer({ project, role, busy: thinking, reply, onReply, onSent, field }) {
  const store = useStore();
  // ^ A role's draft is its own.
  const draft = role ? `${project}/${role}` : project;
  const [text, setText] = useState(() => readMap(DRAFTS_KEY)[draft] ?? "");
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
  // ^ A failed read shows its error and a Retry in the list, not "Loading…" for ever.
  const [failed, setFailed] = useState("");
  const [attempt, setAttempt] = useState(0);
  const mark = typed?.mark;
  useEffect(() => {
    if (mark !== "/" && mark !== "@") return undefined;
    let current = true;
    setFailed("");
    loadMentions(project).then((value) => { if (current) setMentions(value); }, (failure) => { if (current) setFailed(failure.message); });
    return () => { current = false; };
  }, [project, mark === "/" || mark === "@", attempt]);
  // ^ `#` lists tasks, `@` the other projects, `/` the owner's skills the manager can use. A
  // skill is asked for in words: the manager is headless and takes no slash command.
  const options = !typed ? null
    : mark === "#" ? taskOptions(tasks, typed.query)
    : mark === "@" ? within(mentions?.projects ?? [], typed.query, (other) => other.name)
      .map((other) => ({ key: other.id, name: `@${other.name}`, description: other.manager ?? "", source: "project", insert: `@${other.name} ` }))
    : within(mentions?.skills ?? [], typed.query, (skill) => skill.name)
      .map((skill) => ({ key: skill.name, name: `/${skill.name}`, description: skill.description, source: "skill", insert: typed.start ? `owner:${skill.name} ` : `Use owner:${skill.name}: ` }));
  const listOpen = Boolean(options);
  useLayoutEffect(() => { field.current.value = text; grow(field.current); }, [project]);
  useLayoutEffect(() => { if (listOpen) document.getElementById(`slash-${active}`)?.scrollIntoView({ block: "nearest" }); }, [active, listOpen]);
  useEffect(() => release, []);
  const changed = () => {
    const node = field.current;
    setText(node.value);
    setCaret(node.selectionStart);
    setActive(0);
    keepIn(DRAFTS_KEY, draft, node.value);
    grow(node);
  };
  const pick = ({ insert }) => {
    const node = field.current;
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
      const text = withQuote(reply, body);
      // ^ A say may start the manager first: up to about 40 s (docs/tasks.md "Manager").
      const answer = await api(`api/tasks/${project}/say`, { method: "POST", json: { text, attachments, ...(role && { role }) }, timeout: 60_000 });
      field.current.value = "";
      files.attachments = [];
      changed();
      onReply(null);
      onSent({ id: answer.id, text });
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
      <${QuoteChip} quote=${reply} onRemove=${() => onReply(null)} />
      ${listOpen && html`<${Suggestions} label=${LIST_LABELS[mark]} active=${active} onHover=${setActive} onPick=${pick}
        options=${options}
        notes=${[!options.length && emptyNote(mark, tasks, mentions, failed, () => setAttempt((count) => count + 1)), mark === "#" && foldedNote(tasks, typed.query)]} />`}
      <div class="composer-row" onPointerDown=${focusField}>
        <${AttachButton} onFiles=${upload} />
        <div class="composer-input">
          <textarea class="composer-field" ref=${field} rows="1" aria-label=${`Message to ${role ?? "the manager"}`}
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
const noteLeft = (project) => keepIn(LEFT_KEY, project, Date.now());
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
// When this device last read each role's chat, by `<project>/<role>`: a contact with something
// newer is unread.
const ROLES_SEEN_KEY = "wsx-roles-seen";

function More({ project, roles }) {
  const files = shows("files") && can("files_read");
  const seen = readMap(ROLES_SEEN_KEY);
  return html`
    <${MoreMenu}>
      <a role="menuitem" href=${`#/p/${project.id}/page`}>Page</a>
      <a role="menuitem" href=${`#/p/${project.id}/sessions`}>Sessions</a>
      ${files && html`<a role="menuitem" href=${`#/p/${project.id}/files`}>Files</a>`}
      ${roles?.length > 0 && html`
        <p class="more-heading" aria-hidden="true">Roles</p>
        ${roles.map((role) => {
          const unread = role.last_unix_ms > (seen[`${project.id}/${role.name}`] ?? 0);
          return html`<a role="menuitem" class="role-contact" key=${role.name} href=${`#/p/${project.id}/r/${encodeURIComponent(role.name)}`}
            aria-label=${`Role ${role.name}${unread ? ", unread" : ""}`}>
            <span class="role-name">${role.name}${unread && html`<span class="news-mark" aria-hidden="true"></span>`}</span>
            <span class="role-line">${role.last_reply ?? role.brief}</span>
          </a>`;
        })}`}
    </${MoreMenu}>`;
}

// A task or the project page over the chat, about 85% of the height (a `fit` sheet only as
// high as its content): Back, Escape, a tap
// beside it or a swipe down on its top closes it, and the chat under it keeps its scroll and
// its draft. A fit sheet's top follows its height, so it opens once its content calls `ready`:
// rows that came after the slide would lift it again. Its first read gives up after a while
// (api.js), so `ready` always comes.
function Sheet({ label, fit = false, onClose, children }) {
  const dialog = useRef();
  const drag = useRef(null);
  const [ready, setReady] = useState(!fit);
  useEffect(() => {
    if (ready && !dialog.current.open) dialog.current.showModal();
  }, [ready]);
  useEffect(() => {
    const node = dialog.current;
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
      ${children({ down, move, up, ready: () => setReady(true) })}
    </dialog>`;
}

// The project screen with task_view: the manager chat as messages, what is open in the
// reservoir, and with a task or the page in the route its sheet over the chat. With `role`, a
// role's chat (#/p/<project>/r/<role>): the same bubbles without the manager's reservoir,
// approvals and To check items (docs/web-client-tasks.md "Roles").
export function Manager({ project: id, task, page, checks, role = null }) {
  const store = useStore();
  const project = store.projects.find((candidate) => candidate.id === id);
  const overview = store.tasks[id];
  const [chat, setChat] = useState(null);
  // ^ The To check items, each a bubble at the time it was added; read again when the open
  // count changes (a new item, a pass or a fail).
  const [checkItems, setCheckItems] = useState(role ? [] : null);
  const toCheck = overview?.to_check;
  useEffect(() => {
    if (role) return undefined;
    let live = true;
    api(`api/tasks/${id}/checks`).then((value) => { if (live) setCheckItems(value.items); }, () => { if (live) setCheckItems((now) => now ?? []); });
    return () => { live = false; };
  }, [id, toCheck]);
  const ready = Boolean(chat) && checkItems !== null;
  const [failure, setFailure] = useState("");
  const [sent, setSent] = useState(null);
  const place = role ? `project-${id}-r-${role}` : `project-${id}`;
  const { log, away, unread, since, scrolled, toEnd } = useFollowEnd(place, ready);
  const field = useRef();
  const [reply, setReply] = useState(null);
  const [gestures, menu] = useReply((line) => { setReply(line); field.current?.focus(); });
  const shown = useRef([]);
  const cut = useRef(false);

  // Reads after the last entry it has; one read at a time, one more after a change during it.
  const load = useCoalescedLoad(async (current) => {
    const last = shown.current.findLast((entry) => entry.id != null)?.id;
    const next = await api(`api/tasks/${id}/chat`, { query: { ...(last != null && { after: last }), ...(role && { role }) } });
    if (!current()) return;
    if (role) keepIn(ROLES_SEEN_KEY, `${id}/${role}`, Date.now());
    // ^ A rotation or a stop has no id, so every read after the last id returns it again
    // until the next message: it joins once.
    const known = (entry) => entry.id == null
      && shown.current.some((seen) => seen.id == null && seen.kind === entry.kind && seen.unix_ms === entry.unix_ms);
    const all = [...shown.current, ...next.entries.filter((entry) => !known(entry))];
    if (next.skipped || all.length > 400) cut.current = true;
    shown.current = all.slice(-400);
    setChat({ ...next, entries: shown.current });
    setFailure("");
  }, (error) => setFailure(error.message), [id]);
  useEffect(() => { if (!overview) loadTasks(id, { missing: true }); }, [id]);
  useEventLoad("manager", load, (detail) => detail.project == null
    || (detail.project === id && (!role || detail.role === role)), [id]);
  // Closing the sheet gives the focus back to the composer, in the same commit: after a paint
  // it took the focus from a button tapped in the meantime (a reservoir row's Land).
  const sheet = task ?? (page || checks || null);
  const sheetWas = useRef(sheet);
  useLayoutEffect(() => {
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
  const items = role ? [] : overview?.needs ?? [];
  const words = withChips(id);
  const rows = ready ? conversation(entries, overview?.resolutions ?? [], items.filter((item) => item.kind === "approval"), checkItems) : [];
  // The sent message shows until the log has it.
  if (sent && !entries.some((entry) => entry.kind === "message" && entry.id === sent.id)) {
    const last = rows.at(-1);
    const grouped = last?.kind === "bubble" && last.author === "owner";
    if (grouped) last.last = false;
    rows.push({ kind: "bubble", key: "sending", author: "owner", at: Date.now(), first: !grouped, last: true, pending: true, entry: { text: sent.text } });
  }
  const firstNew = since == null ? null : rows.find((row) => row.kind === "bubble" && row.author !== "owner" && !row.pending && row.at > since);
  const [recent, earlier] = useRecent(place, rows, firstNew);
  const typing = Boolean(chat?.busy);
  // ^ The typing bubble fades out as the reply replaces it.
  const typingShown = useAfter(typing, LEAVE_MS) || typing;
  // ^ Rows that arrive after the first read slide in; the ones of the first read, and the
  // message this device just sent, do not.
  const known = useRef(null);
  const fresh = useRef(new Set());
  if (ready) {
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
  const show = (row) => row.kind === "bubble" ? html`<${Bubble} key=${row.key} project=${id} row=${row} words=${words} names=${names} fresh=${fresh.current.has(row.key)} />`
    : row.kind === "line" ? html`<${StatusLine} key=${row.key} project=${id} text=${row.text} fresh=${fresh.current.has(row.key)} />`
    : row.kind === "quiet" ? html`<${QuietReply} key=${row.key} row=${row} name=${names.manager} fresh=${fresh.current.has(row.key)} />`
    : row.kind === "fold" ? html`<${Fold} key=${row.key} row=${row} render=${show} />`
    : row.kind === "check" ? html`<${CheckBubble} key=${row.key} project=${id} row=${row} names=${names} fresh=${fresh.current.has(row.key)} />`
    : html`<${Note} key=${row.key} entry=${row.entry} />`;
  return html`
    <div class="page manager">
      <header class="bar">
        <${Back} fallback=${role ? `#/p/${id}` : "#/"} />
        <div class="bar-title">
          <span class="bar-label">${role ?? project.name}</span>
          ${managerWord && html`<span class="bar-state" key=${managerWord} data-tone=${typing ? "working" : "danger"}>${typing
            ? html`<span class="typing-dots"><span class="visually-hidden">typing</span><${Dots} /></span>`
            : managerWord}</span>`}
        </div>
        ${!role && html`<div class="bar-tools"><${More} project=${project} roles=${chat?.roles} /></div>`}
      </header>
      <div class="chat">
        ${!role && html`<${SinceLine} project=${id} reply=${rows.findLast((row) => row.kind === "bubble" && row.author !== "owner")?.at} sent=${sent} />`}
        <div class="log-wrap">
          <div class="log" ref=${log} onScroll=${scrolled} role="log" aria-label=${`Conversation with ${role ?? "the manager"}`} tabindex="0" ...${gestures}>
            ${cut.current ? html`<p class="note">Older messages are not shown.</p>` : null}
            ${ready && !rows.length ? html`<p class="empty">${role
              ? `No messages yet. Ask ${role} about its work; it answers from the ledger.`
              : "No messages yet. Ask the manager for a task or a status; it answers from the ledger."}</p>` : null}
            ${earlier}
            ${withNewLine(recent, firstNew, show)}
            ${typingShown && html`<${Typing} name=${names.manager} leaving=${!typing} />`}
          </div>
          ${away && html`<${JumpButton} unread=${unread} onClick=${toEnd} />`}
        </div>
        ${menu}
        ${failure && html`<p class="error chat-error" role="alert">${failure}</p>`}
        ${!role && html`<${Reservoir} project=${id} items=${items} tasks=${overview?.tasks ?? []} checking=${overview?.to_check ?? 0} />`}
        ${can("task_act") && html`<${ManagerComposer} project=${id} role=${role} busy=${typing} field=${field} reply=${reply} onReply=${setReply}
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
