// Manager mode on a project: the manager chat as messages with short task chips, what needs
// the owner and what runs in one reservoir over the composer, the `#` task picker, and a
// task's or the project page's sheet over the chat (docs/web-client.md "Tasks"). Fixed actions
// go straight to wsx.
import { api } from "./api.js";
import { html, Icon, useEffect, useFollowEnd, useLayoutEffect, useRef, useState } from "./lib.js";
import { can, events, leaveFor, shows, state, touch, update, useStore } from "./store.js";
import { Back, toast } from "./ui.js";
import { grow, isolate, release, Suggestions } from "./chat.js";
import { Bubble, conversation, StatusLine, Typing } from "./bubbles.js";
import { Adopted } from "./adopted.js";
import { TaskPage } from "./task.js";
import { ProjectPage } from "./page.js";

// Every granted project's overview, or one project's.
export async function loadTasks(project) {
  try {
    if (project == null) {
      const all = await api("api/tasks");
      all.forEach(noteDecisions);
      update({ tasks: Object.fromEntries(all.map((overview) => [overview.project, overview])) });
    } else {
      const overview = await api(`api/tasks/${project}`);
      noteDecisions(overview);
      update({ tasks: { ...state.tasks, [project]: overview } });
    }
  } catch { /* the next tasks event brings it */ }
}

// A task state in words, and the tone that repeats it.
const STATES = {
  starting: ["starting", "working"], working: ["working", "working"], checking: ["checking", "working"], briefed: ["brief ready", "attention"],
  blocked: ["blocked", "attention"], ready: ["ready", "accent"], failed: ["failed", "danger"],
  conflict: ["conflict", "danger"], stopped: ["stopped", "muted"], in_review: ["in review", "muted"],
  landed: ["landed", "muted"], closed: ["closed", "muted"],
};
export const taskState = (key) => STATES[key] ?? [key, "muted"];
export const taskLink = (project, task) => `#/p/${project}/t/${task}`;

// A task that runs now: its chip and its reservoir row pulse.
const RUNNING = new Set(["starting", "working", "checking"]);
export const running = (task) => RUNNING.has(task?.state);

// `#12` in any text: the number and a state dot, the state in its accessible name and the
// title on hover. The title shows in the sheet and the reservoir, not in the text.
export function TaskChip({ project, id }) {
  const store = useStore();
  const task = store.tasks[project]?.tasks.find((candidate) => candidate.id === id);
  if (!task) return html`<a class="task-chip" href=${taskLink(project, id)}>#${id}</a>`;
  const [word, tone] = taskState(task.state);
  return html`
    <a class="task-chip" href=${taskLink(project, id)} title=${task.title} aria-label=${`#${id}, ${word}`}>
      <span class="mark" data-tone=${tone} data-running=${running(task) ? "" : null} aria-hidden="true"></span>#${id}
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

// What each Needs-you item is, in words.
const KIND_WORDS = {
  blocked: "needs an answer", conflict: "conflict", failed: "failed", verify: "ready to verify",
  land: "ready to land", review: "in review", brief: "brief ready",
};
const KIND_TONE = { blocked: "attention", conflict: "danger", failed: "danger", approval: "attention", brief: "attention", verify: "accent", land: "accent", review: "muted" };
// One open item in words, as the owner would read it: "#4 needs an answer", "Approve land #3",
// "Brief ready #2".
const NEED_WORDS = {
  blocked: "needs an answer", conflict: "has a conflict", failed: "failed", verify: "is ready to verify",
  land: "is ready to land", review: "is in review",
};
export const needWords = (item) => item.kind === "approval" ? item.label
  : item.kind === "brief" ? `Brief ready #${item.task}`
  : item.kind === "blocked" && item.asks?.length ? `#${item.task} needs to ${askWords(item.asks)}`
  : `#${item.task} ${NEED_WORDS[item.kind]}`;
// What a blocked task asks for, never a command: "reach crates.io and write ~/.cargo"
// (docs/web-client.md "Tasks"). A grant body names one ask.
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

// Actions whose outcome the chat already shows as a status line ("Approved land #3",
// "#1 may reach crates.io (owner)", "#2 passed verification of r1", "#3 landed").
const SAID_IN_CHAT = new Set(["approve", "deny", "allow", "verify"]);

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
  // ^ Awaited, so the row has left the list when the reservoir moves the focus on.
  await loadTasks(project);
  return answer;
}

// One open item in the reservoir: its chip (an approval names its task in its label), its
// title and what it waits for, with its one action. The title opens the task's sheet, where the
// other actions are. `onDone` hears of a success, so the focus moves to the next row.
function NeedRow({ project, item, index, onDone }) {
  const key = `${project}/${itemKey(item)}`;
  const draft = actionOf(key);
  const [, renew] = useState(0);
  const field = useRef();
  const acting = can("task_act");
  const refresh = () => renew((count) => count + 1);
  useEffect(() => { if (draft.field) field.current?.focus(); }, [draft.field]);
  const run = async (action, body) => {
    draft.busy = true;
    draft.error = "";
    refresh();
    try {
      await act(project, item, action, body);
      draft.field = null;
      draft.text = "";
      onDone(itemKey(item), index);
    } catch (failure) {
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
  const asks = item.kind === "blocked" ? item.asks ?? [] : [];
  const grant = async () => {
    draft.busy = true;
    draft.error = "";
    refresh();
    try {
      for (const ask of asks) await act(project, item, "allow", grantBody(ask, false));
      onDone(itemKey(item), index);
    } catch (failure) {
      draft.error = failure.message;
    }
    draft.busy = false;
    refresh();
  };
  const button = (label, onClick, extra = {}) => html`
    <button class="button need-action" type="button" disabled=${draft.busy} onClick=${onClick} ...${extra}>${label}</button>`;
  let actions = null;
  if (acting && item.kind === "blocked" && asks.length) {
    actions = button("Allow", grant, { "aria-label": `Allow #${item.task} to ${askWords(asks)}` });
  } else if (acting && ["blocked", "failed", "conflict"].includes(item.kind)) {
    actions = button("Answer", open, { "aria-expanded": draft.field === "answer", "aria-label": `Answer #${item.task}` });
  } else if (acting && item.kind === "approval") {
    actions = html`${button("Deny", () => run("deny"), { "aria-label": `Deny: ${item.label}` })}${button("Approve", () => run("approve"), { "aria-label": item.label })}`;
  } else if (acting && item.kind === "brief") {
    actions = button("Start", () => run("start"), { "aria-label": `Start the build of #${item.task}` });
  } else if (acting && (item.kind === "land" || item.kind === "review")) {
    actions = button(item.kind === "land" ? "Land" : "Check", () => run("land"), { "aria-label": `${item.kind === "land" ? "Land" : "Check the pull request of"} #${item.task}` });
  } else if (item.kind === "verify" && item.task != null) {
    actions = html`<a class="button need-action" href=${taskLink(project, item.task)}>Verify</a>`;
  }
  // ^ An approval reads as the action it asks for, never as its id (docs/web-client.md "Tasks").
  const context = item.kind === "approval" ? [item.revision != null && `r${item.revision}`, item.title ?? item.commit].filter(Boolean).join(" · ")
    : asks.length ? `needs to ${askWords(asks)}` : KIND_WORDS[item.kind];
  const label = item.kind === "approval" ? item.label : item.title;
  const body = html`
    <span class="row-label">${label}</span>
    ${context && html`<span class="row-context">${context}</span>`}`;
  return html`
    <li class="need" data-key=${itemKey(item)}>
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

// A task that runs, in the reservoir: its chip and title; the chip pulses.
const RunningRow = ({ project, task }) => html`
  <li class="need">
    <div class="row-link">
      <${TaskChip} project=${project} id=${task.id} />
      <a class="row-main row-open" href=${taskLink(project, task.id)}><span class="row-label">${task.title}</span><span class="row-context">${taskState(task.state)[0]}</span></a>
    </div>
  </li>`;

// The decision list, pinned over the composer (docs/web-client.md "Project screen"). Closed it
// is one line: "2 need you · #6 <title> +1 more". Open, every item that waits for the owner in
// the server's order, each with its one action, then the tasks that run. A cleared row leaves
// and the focus moves to the next row's action. It never moves the chat.
function Reservoir({ project, items, tasks }) {
  const [open, setOpen] = useState(false);
  const node = useRef();
  const cleared = useRef(null);
  // ^ The reload may remove the row before its action returns: `onDone` runs the effect again.
  const [heard, hear] = useState(0);
  const working = tasks.filter(running);
  const keys = items.map(itemKey);
  useLayoutEffect(() => {
    const done = cleared.current;
    if (!done || keys.includes(done.key)) return;
    cleared.current = null;
    // ^ The row may leave on a later `tasks` event (a start reaches the host after its answer):
    // the focus moves then too, but only when it is still in the list or left with the button.
    const focus = document.activeElement;
    if (focus && focus !== document.body && !node.current?.contains(focus)) return;
    const next = keys[Math.min(done.index, keys.length - 1)];
    const row = next && node.current?.querySelector(`.need[data-key="${CSS.escape(next)}"]`);
    // ^ The row's action first; its title only when the site may not act.
    const target = row?.querySelector(".need-action") ?? row?.querySelector(".row-open") ?? node.current?.querySelector(".reservoir-head");
    target?.focus({ preventScroll: true });
  }, [keys.join(), heard]);
  if (!items.length && !working.length && !tasks.length) return null;
  const onDone = (key, index) => {
    cleared.current = { key, index };
    hear((count) => count + 1);
  };
  // ^ The newest running task is the one with the highest number: stable while it works.
  const newest = working.reduce((last, task) => (!last || task.id > last.id ? task : last), null);
  const need = items.length ? `${items.length} ${items.length === 1 ? "needs" : "need"} you` : "";
  return html`
    <section class="reservoir" ref=${node} aria-label="Needs you and running" data-open=${open ? "" : null}>
      <button class="reservoir-head" type="button" aria-expanded=${open} aria-controls="reservoir-list" onClick=${() => setOpen(!open)}>
        ${items.length ? html`<span class="mark" data-tone="attention" aria-hidden="true"></span>` : null}
        <span class="reservoir-words">
          ${need || (newest ? "" : "Nothing needs you")}
          ${newest && html`
            ${need && html`${" "}<span class="reservoir-sep" aria-hidden="true">·</span>${" "}`}
            <span class="task-chip"><span class="mark" data-tone="working" data-running="" aria-hidden="true"></span>#${newest.id}</span>${" "}
            <span class="reservoir-title">${newest.title}</span>`}
        </span>
        ${working.length > 1 && html`${" "}<span class="reservoir-more">+${working.length - 1} more</span>`}
        <${Icon} name="chevron" />
      </button>
      ${open && (items.length || working.length) ? html`
        <div class="reservoir-body" id="reservoir-list">
          ${items.length ? html`
            <ul class="reservoir-list" aria-label="Needs you">
              ${items.map((item, index) => html`<${NeedRow} key=${itemKey(item)} project=${project} item=${item} index=${index} onDone=${onDone} />`)}
            </ul>` : null}
          ${working.length ? html`
            <h2 class="label reservoir-label">Running</h2>
            <ul class="reservoir-list" aria-label="Running">
              ${working.map((task) => html`<${RunningRow} key=${`run-${task.id}`} project=${project} task=${task} />`)}
            </ul>` : null}
        </div>` : null}
    </section>`;
}

// Unsent manager messages by project, kept on this device.
const DRAFTS_KEY = "wsx-manager-drafts";
const readDrafts = () => { try { return JSON.parse(localStorage.getItem(DRAFTS_KEY)) ?? {}; } catch { return {}; } };
function keepDraft(project, text) {
  const all = readDrafts();
  if (text) all[project] = text;
  else delete all[project];
  try { localStorage.setItem(DRAFTS_KEY, JSON.stringify(all)); } catch { /* kept in the field only */ }
}

// While the word at the caret is `#` and digits or letters: the tasks that match it.
function taskQuery(text, caret) {
  const typed = /(^|\s)#(\w*)$/.exec(text.slice(0, caret));
  return typed ? { query: typed[2].toLowerCase(), start: caret - typed[2].length - 1 } : null;
}
function matchingTasks(tasks, query) {
  const hits = tasks.filter((task) => String(task.id).startsWith(query) || task.title.toLowerCase().includes(query));
  return hits.slice(0, 50);
}

function ManagerComposer({ project, busy: thinking, onSent, field }) {
  const store = useStore();
  const [text, setText] = useState(() => readDrafts()[project] ?? "");
  const [caret, setCaret] = useState(0);
  const [active, setActive] = useState(0);
  const [closed, setClosed] = useState(null);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const tasks = store.tasks[project]?.tasks ?? [];
  const typed = closed === text ? null : taskQuery(text, caret);
  const options = typed ? matchingTasks(tasks, typed.query) : null;
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
    const insert = `#${option.task.id} `;
    node.value = text.slice(0, typed.start) + insert + text.slice(caret);
    const at = typed.start + insert.length;
    node.focus();
    node.setSelectionRange(at, at);
    changed();
  };
  const send = async () => {
    const body = field.current.value.trim();
    if (busy || !body) return;
    setBusy(true);
    setError("");
    try {
      const answer = await api(`api/tasks/${project}/say`, { method: "POST", json: { text: body } });
      field.current.value = "";
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
        pick({ task: options[active] });
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
  const focusField = (event) => {
    if (event.target.closest("button, textarea, input")) return;
    event.preventDefault();
    field.current.focus();
  };
  return html`
    <div class="composer">
      ${error && html`<p class="error" role="alert">${error}</p>`}
      ${listOpen && html`<${Suggestions} label="Tasks" active=${active} onHover=${setActive} onPick=${pick}
        options=${options.map((task) => ({ key: task.id, name: `#${task.id}`, description: task.title,
          source: taskState(task.state)[0], tone: taskState(task.state)[1], task }))}
        notes=${[!options.length && (tasks.length ? "No task matches that." : "This project has no task yet.")]} />`}
      <div class="composer-row" onPointerDown=${focusField}>
        <div class="composer-input">
          <textarea class="composer-field" ref=${field} rows="1" aria-label="Message to the manager"
            placeholder="Message the manager · # for a task" role="combobox" aria-autocomplete="list" aria-expanded=${listOpen}
            aria-controls=${listOpen ? "slash-list" : null} aria-activedescendant=${listOpen && options.length ? `slash-${active}` : null}
            onInput=${changed} onKeyDown=${keydown} onKeyUp=${() => setCaret(field.current.selectionStart)}
            onClick=${() => setCaret(field.current.selectionStart)} onFocus=${isolate} onBlur=${release}></textarea>
        </div>
        <button class="button button-primary composer-send" type="button" aria-label=${thinking ? "Queue" : "Send"}
          disabled=${busy} onClick=${send}>
          <span class="send-mark" aria-hidden="true"><${Icon} name="send" /></span><span class="send-text">${thinking ? "Queue" : "Send"}</span>
        </button>
      </div>
    </div>`;
}

// When this device last left each project's screen, for "since you left".
const LEFT_KEY = "wsx-project-left";
const readLeft = () => { try { return JSON.parse(localStorage.getItem(LEFT_KEY)) ?? {}; } catch { return {}; } };
function noteLeft(project) {
  const all = readLeft();
  all[project] = Date.now();
  try { localStorage.setItem(LEFT_KEY, JSON.stringify(all)); } catch { /* no line next time */ }
}
// Shorter absences are a glance away, not an absence.
const AWAY_MS = 60_000;
const SINCE_WORDS = [["landed", "landed"], ["blocked", "blocked"], ["conflict", "conflicted"], ["revised", "got a new revision"], ["started", "started"]];

// "Since you left": what the task logs say happened while this device was away from the
// project, on opening it or on coming back to it. Nothing runs while away.
function SinceLine({ project }) {
  const [since, setSince] = useState(null);
  useEffect(() => {
    const ask = () => {
      const left = readLeft()[project];
      if (left && Date.now() - left > AWAY_MS) {
        api(`api/tasks/${project}/since`, { query: { after: left } }).then(setSince, () => {});
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
  if (!since) return null;
  const parts = SINCE_WORDS.filter(([key]) => since.tasks[key]?.length).map(([key, words]) => html`
    <span key=${key}>${since.tasks[key].map((id) => html`<a href=${taskLink(project, id)}>#${id}</a> `)}${words}</span>`);
  if (since.replies) parts.push(html`<span key="replies">${since.replies} ${since.replies === 1 ? "reply" : "replies"}</span>`);
  if (!parts.length) return null;
  return html`
    <div class="since" role="status">
      <p>Since you left: ${parts.map((part, index) => html`${index ? " · " : ""}${part}`)}</p>
      <button class="link-action" type="button" onClick=${() => setSince(null)}>Dismiss</button>
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

// A task or the project page over the chat, about 85% of the height: Back, Escape, a tap
// beside it or a swipe down on its top closes it, and the chat under it keeps its scroll and
// its draft.
function Sheet({ label, onClose, children }) {
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
    <dialog class="task-sheet" ref=${dialog} aria-label=${label} onCancel=${cancel} onClick=${backdrop}>
      <div class="task-sheet-top" onPointerDown=${down} onPointerMove=${move} onPointerUp=${up} onPointerCancel=${up}>
        <span class="sheet-grip" aria-hidden="true"></span>
      </div>
      ${children({ down, move, up })}
    </dialog>`;
}

// The project screen with task_view: the manager chat as messages, what is open in the
// reservoir, and with a task or the page in the route its sheet over the chat.
export function Manager({ project: id, task, page }) {
  const store = useStore();
  const project = store.projects.find((candidate) => candidate.id === id);
  const overview = store.tasks[id];
  const [chat, setChat] = useState(null);
  const [failure, setFailure] = useState("");
  const [sent, setSent] = useState(null);
  const { log, atEnd, away, scrolled, toEnd } = useFollowEnd();
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
        const all = [...shown.current, ...next.entries];
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
    if (!overview) loadTasks(id);
    load();
    const changed = (event) => { if (event.detail.project === id) load(); };
    events.addEventListener("manager", changed);
    return () => events.removeEventListener("manager", changed);
  }, [id]);
  // Closing the sheet gives the focus back to the composer.
  const sheetWas = useRef(task ?? page);
  useEffect(() => {
    if (sheetWas.current && !(task ?? page)) field.current?.focus({ preventScroll: true });
    sheetWas.current = task ?? page;
  }, [task, page]);

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
  const stopped = chat && !chat.running && entries.at(-1)?.kind === "stopped";
  // ^ The manager's state only when it matters: typing or stopped (docs/web-client.md "Project screen").
  const managerWord = typing ? "typing" : stopped ? "stopped" : "";
  return html`
    <div class="page manager">
      <header class="bar">
        <${Back} fallback="#/" />
        <div class="bar-title">
          <span class="bar-label">${project.name}</span>
          ${managerWord && html`<span class="bar-state" data-tone=${typing ? "working" : "danger"}>${managerWord}</span>`}
        </div>
        <div class="bar-tools"><${More} project=${project} /></div>
      </header>
      <div class="chat">
        <${Adopted} project=${project} strip />
        <${SinceLine} project=${id} />
        <div class="log-wrap">
          <div class="log" ref=${log} onScroll=${scrolled} role="log" aria-label="Conversation with the manager" tabindex="0">
            ${cut.current ? html`<p class="note">Older messages are not shown.</p>` : null}
            ${chat && !rows.length ? html`<p class="empty">No messages yet. Ask the manager for a task or a status; it answers from the ledger.</p>` : null}
            ${rows.map((row) => row.kind === "bubble" ? html`<${Bubble} key=${row.key} project=${id} row=${row} words=${words} />`
              : row.kind === "line" ? html`<${StatusLine} key=${row.key} parts=${row.parts} words=${words} />`
              : html`<${Note} key=${row.key} entry=${row.entry} />`)}
            ${typing && html`<${Typing} />`}
          </div>
          ${away && html`<button class="jump" type="button" onClick=${toEnd}><${Icon} name="down" />Jump to latest</button>`}
        </div>
        ${failure && html`<p class="error chat-error" role="alert">${failure}</p>`}
        <${Reservoir} project=${id} items=${items} tasks=${overview?.tasks ?? []} />
        ${can("task_act") && html`<${ManagerComposer} project=${id} busy=${typing} field=${field}
          onSent=${(message) => { atEnd.current = true; setSent(message); load(); }} />`}
      </div>
      ${task != null && html`
        <${Sheet} key=${`t${task}`} label=${`Task #${task}`} onClose=${() => leaveFor(`#/p/${id}`)}>
          ${(grip) => html`<${TaskPage} key=${task} project=${id} task=${task} onClose=${() => leaveFor(`#/p/${id}`)} grip=${grip} />`}
        </${Sheet}>`}
      ${page && html`
        <${Sheet} key="page" label="Project page" onClose=${() => leaveFor(`#/p/${id}`)}>
          ${(grip) => html`<${ProjectPage} project=${id} onClose=${() => leaveFor(`#/p/${id}`)} grip=${grip} />`}
        </${Sheet}>`}
    </div>`;
}

// A rotation or a stop of the manager, said once.
const Note = ({ entry }) => entry.kind === "rotated"
  ? html`<p class="note">${entry.reason ? `New conversation: ${entry.reason}` : `New conversation after ${entry.tokens.toLocaleString()} tokens`}; the ledger carries the state.</p>`
  : html`<p class="note" data-tone="danger">The manager stopped: ${entry.reason}</p>`;
