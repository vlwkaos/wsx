// Manager mode on a project: the manager chat with live task chips, what needs the owner as
// cards in it and in the tray over the composer, the `#` task picker and a task's sheet over the
// chat (docs/web-client.md "Tasks"). Fixed actions go straight to wsx.
import { api } from "./api.js";
import { html, Icon, useEffect, useFollowEnd, useLayoutEffect, useRef, useState } from "./lib.js";
import { can, events, leaveFor, shows, state, touch, update, useStore } from "./store.js";
import { Back, toast } from "./ui.js";
import { Markdown } from "./markdown.js";
import { grow, isolate, release, Suggestions } from "./chat.js";
import { ProjectBrief } from "./story.js";
import { TaskPage } from "./task.js";

// Every granted project's overview, or one project's.
export async function loadTasks(project) {
  try {
    if (project == null) {
      const all = await api("api/tasks");
      update({ tasks: Object.fromEntries(all.map((overview) => [overview.project, overview])) });
    } else {
      const overview = await api(`api/tasks/${project}`);
      update({ tasks: { ...state.tasks, [project]: overview } });
    }
  } catch { /* the next tasks event brings it */ }
}

// A task state in words, and the tone that repeats it.
const STATES = {
  starting: ["starting", "accent"], working: ["working", "accent"], checking: ["checking", "accent"], briefed: ["brief ready", "attention"],
  blocked: ["blocked", "attention"], ready: ["ready", "accent"], failed: ["failed", "danger"],
  conflict: ["conflict", "danger"], stopped: ["stopped", "muted"], in_review: ["in review", "muted"],
  landed: ["landed", "muted"], closed: ["closed", "muted"],
};
export const taskState = (key) => STATES[key] ?? [key, "muted"];
export const taskLink = (project, task) => `#/p/${project}/t/${task}`;

// `#12` in a reply, an event or a sent message: a link with the task's current state and title.
function TaskChip({ project, id }) {
  const store = useStore();
  const task = store.tasks[project]?.tasks.find((candidate) => candidate.id === id);
  if (!task) return html`<a class="task-chip" href=${taskLink(project, id)}>#${id}</a>`;
  const [word, tone] = taskState(task.state);
  return html`
    <a class="task-chip" href=${taskLink(project, id)} title=${task.title}>
      <span class="task-chip-id">#${id}</span>${" "}<span class="task-chip-state" data-tone=${tone}>${word}</span>${" "}<span class="task-chip-title">${task.title}</span>
    </a>`;
}

// One text-to-nodes function per project, so the markdown of a message is parsed once.
const chipWords = new Map();
export function withChips(project) {
  if (!chipWords.has(project)) {
    chipWords.set(project, (text) => text.split(/(#\d+)\b/).map((part) =>
      /^#\d+$/.test(part) ? html`<${TaskChip} project=${project} id=${Number(part.slice(1))} />` : part));
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
const needId = (item) => `need-${itemKey(item)}`;

// Drafts and outcomes of actions, by `<project>/<item>`, kept across the cards' re-renders.
const pending = new Map();
const actionOf = (key) => {
  if (!pending.has(key)) pending.set(key, { field: null, text: "", busy: false, error: "" });
  return pending.get(key);
};

// What a fixed action did, as a notice: wsx's own answer, or the decision.
export function announce(name, action, answer) {
  // ^ An approval is named by its label, "Approve land #3": the notice says what was decided.
  const decided = (word) => name.startsWith("Approve ") ? name.replace(/^Approve /, `${word} `) : `${name} ${word.toLowerCase()}`;
  toast(action === "approve" ? `${decided("Approved")}; it runs now.`
    : action === "deny" ? `${decided("Denied")}.` : `${name}: ${answer.message || "done"}`);
}

// One fixed action. A success says what wsx did and clears the draft; a failure keeps it.
async function act(project, item, action, body = {}) {
  const path = item.kind === "approval"
    ? `api/tasks/${project}/approvals/${item.approval}/${action}`
    : `api/tasks/${project}/t/${item.task}/${action}`;
  const answer = await api(path, { method: "POST", json: body });
  const name = item.kind === "approval" ? item.label : `#${item.task}`;
  announce(name, action, answer);
  loadTasks(project);
  return answer;
}

// One item that needs the owner, with its fixed actions: a card in the conversation (`card`),
// or a row opened from the tray. Its text opens the task's sheet.
function NeedRow({ project, item, card, flash }) {
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
    } catch (failure) {
      draft.error = failure.message;
    }
    draft.busy = false;
    refresh();
  };
  const open = (kind, text = "") => {
    draft.field = draft.field === kind ? null : kind;
    if (draft.field && !draft.text) draft.text = text;
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
  const grant = async (project_wide) => {
    draft.busy = true;
    draft.error = "";
    refresh();
    try {
      for (const ask of asks) await act(project, item, "allow", grantBody(ask, project_wide));
    } catch (failure) {
      draft.error = failure.message;
    }
    draft.busy = false;
    refresh();
  };
  const button = (label, onClick, extra = {}) => html`
    <button class="button need-action" type="button" disabled=${draft.busy} onClick=${onClick} ...${extra}>${label}</button>`;
  let actions = null;
  if (acting && item.kind === "blocked") {
    actions = html`
      ${asks.length ? html`
        ${button("Allow", () => grant(false), { "aria-label": `Allow #${item.task} to ${askWords(asks)}` })}
        ${button("Allow for project", () => grant(true), { "aria-label": `Allow every task of this project to ${askWords(asks)}` })}` : null}
      ${button("Answer", () => open("answer"), { "aria-expanded": draft.field === "answer" })}`;
  } else if (acting && item.kind === "approval") {
    actions = html`${button("Deny", () => run("deny"))}${button("Approve", () => run("approve"))}`;
  } else if (acting && item.kind === "brief") {
    // ^ A reply is a talk-through turn (an add); Start builds the brief's rewritten prompt.
    actions = html`
      ${button("Reply", () => open("answer"), { "aria-expanded": draft.field === "answer", "aria-label": `Reply to the brief of #${item.task}` })}
      ${button("Start", () => run("start"), { "aria-label": `Start the build of #${item.task}` })}`;
  } else if (acting && (item.kind === "land" || item.kind === "review")) {
    actions = button(item.kind === "land" ? "Land" : "Check", () => run("land"), { "aria-label": `${item.kind === "land" ? "Land" : "Check the pull request of"} #${item.task}` });
  } else if (item.kind === "verify" && item.task != null) {
    actions = html`<a class="button need-action" href=${taskLink(project, item.task)}>Verify</a>`;
  }
  // ^ An approval reads as the action it asks for, never as its id (docs/web-client.md "Tasks").
  const context = item.kind === "approval"
    ? [item.revision != null && `r${item.revision}`, item.title ?? item.commit].filter(Boolean).join(" · ")
    : asks.length ? `needs to ${askWords(asks)}`
    : [KIND_WORDS[item.kind], item.summary].filter(Boolean).join(" · ");
  const label = item.kind === "approval" ? item.label : item.title;
  const name = item.kind === "approval" ? null : `#${item.task}`;
  const body = html`
    <span class="row-work">${name && html`<span class="need-id">${name}</span>`}<span class="row-label">${label}</span></span>
    <span class="row-context">${context}</span>`;
  return html`
    <div class=${card ? "need need-card" : "need"} id=${card ? needId(item) : null} data-flash=${flash ? "" : null}>
      <div class="row-link">
        <span class="mark" data-tone=${KIND_TONE[item.kind]} aria-hidden="true"></span>
        ${item.task != null
          ? html`<a class="row-main row-open" href=${taskLink(project, item.task)}>${body}</a>`
          : html`<span class="row-main">${body}</span>`}
        ${actions && html`<span class="row-actions">${actions}</span>`}
      </div>
      ${draft.field && html`
        <form class="need-form" onSubmit=${submit}>
          <input class="input" ref=${field} type="text" autocomplete="off" value=${draft.text}
            aria-label=${`${item.kind === "brief" ? "Reply to the brief of" : "Answer to"} #${item.task}`}
            placeholder=${item.kind === "brief" ? "Reply to the brief" : "Answer"}
            onInput=${(event) => { draft.text = event.target.value; }} disabled=${draft.busy} />
          <button class="button" type="submit" disabled=${draft.busy}>Send</button>
        </form>`}
      ${draft.error && html`<p class="error need-error" role="alert">${draft.error}</p>`}
    </div>`;
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

// Plain text with task chips, for owner messages and event lines.
const Chipped = ({ project, text }) => withChips(project)(text);
// A resolved line stays one muted line: `#n` links to the task without the chip's state and title.
const Linked = ({ project, text }) => text.split(/(#\d+)/).map((part) => /^#\d+$/.test(part)
  ? html`<a class="resolved-id" href=${taskLink(project, Number(part.slice(1)))}>${part}</a>` : part);

function Entry({ project, entry }) {
  if (entry.kind === "message" && entry.from === "owner") {
    return html`<article class="message message-user"><p class="prose"><${Chipped} project=${project} text=${entry.text} /></p></article>`;
  }
  if (entry.kind === "message") {
    return html`
      <div class="events" aria-label="Task changes">
        ${entry.text.split("\n").filter(Boolean).map((line, index) => html`
          <p class="event-line" key=${index}><${Chipped} project=${project} text=${line} /></p>`)}
      </div>`;
  }
  if (entry.kind === "reply") {
    return html`<article class="message"><${Markdown} text=${entry.text} words=${withChips(project)} /></article>`;
  }
  if (entry.kind === "rotated") return html`<p class="note">New conversation after ${entry.tokens.toLocaleString()} tokens; the ledger carries the state.</p>`;
  if (entry.kind === "stopped") return html`<p class="note" data-tone="danger">The manager stopped: ${entry.reason}</p>`;
  return null;
}

// The conversation in time order: chat entries, a card for each open item at the time it arose,
// and a muted line for each settled one. On this page a card that gets settled is replaced by
// its line in place (`placed` remembers where cards were). Items older than the loaded history
// are not placed; the tray opens them inline.
function conversation(entries, items, resolutions, cut, placed) {
  const start = entries[0]?.unix_ms ?? Infinity;
  const loaded = (at) => !cut || at >= start;
  const rows = entries.map((entry, index) => ({ at: entry.unix_ms ?? 0, order: 0, key: entry.id != null ? `${entry.kind}-${entry.id}` : `n${index}`, entry }));
  const open = new Set(items.map(itemKey));
  for (const item of items) {
    if (!placed.has(itemKey(item))) placed.set(itemKey(item), { at: item.unix_ms, task: item.task ?? null });
    if (loaded(item.unix_ms)) rows.push({ at: item.unix_ms, order: 1, key: `card-${itemKey(item)}`, item });
  }
  const used = new Set();
  for (const [key, card] of placed) {
    if (open.has(key)) continue;
    const line = resolutions.find((candidate, index) => !used.has(index) && (candidate.task ?? null) === card.task && candidate.unix_ms >= card.at);
    if (!line) continue;
    used.add(resolutions.indexOf(line));
    if (loaded(card.at)) rows.push({ at: card.at, order: 1, key: `line-${key}`, line });
  }
  resolutions.forEach((line, index) => {
    if (!used.has(index) && loaded(line.unix_ms)) rows.push({ at: line.unix_ms, order: 2, key: `line-${index}-${line.unix_ms}`, line });
  });
  rows.sort((left, right) => left.at - right.at || left.order - right.order);
  return { rows, outside: items.filter((item) => !loaded(item.unix_ms)).map(itemKey) };
}

// The open items over the composer, one line each: two, then "+N" (one with the keyboard up).
// A line scrolls to its card, or opens the item here when its card is not in the loaded history.
function Tray({ project, items, outside, onShow }) {
  const [expanded, setExpanded] = useState(false);
  const [inline, setInline] = useState(null);
  if (!items.length) return null;
  const opened = items.find((item) => itemKey(item) === inline);
  return html`
    <section class="tray" aria-label="Needs you" data-expanded=${expanded ? "" : null}>
      <ul class="tray-lines">
        ${items.map((item) => html`
          <li key=${itemKey(item)}>
            <button class="tray-line" type="button" aria-expanded=${outside.includes(itemKey(item)) ? inline === itemKey(item) : null}
              onClick=${() => (outside.includes(itemKey(item)) ? setInline(inline === itemKey(item) ? null : itemKey(item)) : onShow(item))}>
              <span class="mark" data-tone=${KIND_TONE[item.kind]} aria-hidden="true"></span>
              <span class="tray-words">${needWords(item)}</span>
              ${item.kind !== "approval" && html`<span class="tray-title">${item.title}</span>`}
            </button>
          </li>`)}
      </ul>
      ${items.length > 1 && html`
        <button class="link-action tray-more" type="button" aria-expanded=${expanded} onClick=${() => setExpanded(!expanded)}>
          ${expanded ? "Fewer" : html`<span class="tray-more-one">+${items.length - 1}</span>${items.length > 2 && html`<span class="tray-more-two">+${items.length - 2}</span>`}`}
        </button>`}
      ${opened && html`<${NeedRow} project=${project} item=${opened} />`}
    </section>`;
}

// The top row's ⋯: the project's sessions and files.
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
          <a role="menuitem" href=${`#/p/${project.id}/sessions`} onClick=${() => setOpen(false)}>Sessions</a>
          ${files && html`<a role="menuitem" href=${`#/p/${project.id}/files`} onClick=${() => setOpen(false)}>Files</a>`}
        </div>`}
    </div>`;
}

// A task over the chat, about 85% of the height: Back, Escape, a tap beside it or a swipe down
// on its top closes it, and the chat under it keeps its scroll and its draft.
function TaskSheet({ project, task, onClose }) {
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
    <dialog class="task-sheet" ref=${dialog} aria-label=${`Task #${task}`} onCancel=${cancel} onClick=${backdrop}>
      <div class="task-sheet-top" onPointerDown=${down} onPointerMove=${move} onPointerUp=${up} onPointerCancel=${up}>
        <span class="sheet-grip" aria-hidden="true"></span>
      </div>
      <${TaskPage} key=${task} project=${project} task=${task} onClose=${onClose} grip=${{ down, move, up }} />
    </dialog>`;
}

// The project screen with task_view: the manager chat, what needs the owner as cards in it and
// in the tray, and with a task in the route its sheet over the chat.
export function Manager({ project: id, task }) {
  const store = useStore();
  const project = store.projects.find((candidate) => candidate.id === id);
  const overview = store.tasks[id];
  const [chat, setChat] = useState(null);
  const [failure, setFailure] = useState("");
  const [sent, setSent] = useState(null);
  const [flash, setFlash] = useState(null);
  const { log, atEnd, away, scrolled, toEnd } = useFollowEnd();
  const field = useRef();
  const loading = useRef(false);
  const again = useRef(false);
  const shown = useRef([]);
  const cut = useRef(false);
  const placed = useRef(new Map());

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
  const sheetWas = useRef(task);
  useEffect(() => {
    if (sheetWas.current != null && task == null) field.current?.focus({ preventScroll: true });
    sheetWas.current = task;
  }, [task]);
  const show = (item) => {
    // ^ Off the end first: the flash renders again, and a render at the end scrolls back to it.
    atEnd.current = false;
    document.getElementById(needId(item))?.scrollIntoView({ block: "center", behavior: matchMedia("(prefers-reduced-motion: reduce)").matches ? "auto" : "smooth" });
    setFlash(itemKey(item));
    setTimeout(() => setFlash((current) => (current === itemKey(item) ? null : current)), 1500);
  };

  if (!project) {
    return html`
      <div class="page">
        <header class="bar"><${Back} fallback="#/" /></header>
        <p class="empty">${!store.daemon ? "wsx is not running on the host." : store.loaded ? "This project is not available." : "Loading…"}</p>
      </div>`;
  }
  const entries = chat?.entries ?? [];
  const items = overview?.needs ?? [];
  const { rows, outside } = chat ? conversation(entries, items, overview?.resolutions ?? [], cut.current, placed.current) : { rows: [], outside: [] };
  // The sent message shows until the log has it.
  const waiting = sent && !entries.some((entry) => entry.kind === "message" && entry.id === sent.id) ? sent : null;
  const thinking = Boolean(chat?.busy);
  const managerWord = !chat ? "" : thinking ? "manager thinking" : chat.running ? "manager idle" : "manager not started";
  return html`
    <div class="page manager">
      <header class="bar">
        <${Back} fallback="#/" />
        <div class="bar-title">
          <span class="bar-label">${project.name}</span>
          <span class="bar-state" data-tone=${thinking ? "accent" : null}>${managerWord}</span>
        </div>
        <div class="bar-tools"><${More} project=${project} /></div>
      </header>
      <div class="chat">
        <${ProjectBrief} project=${id} overview=${overview} onExplained=${() => { atEnd.current = true; load(); }} />
        <${SinceLine} project=${id} />
        <div class="log-wrap">
          <div class="log" ref=${log} onScroll=${scrolled} role="log" aria-label="Conversation with the manager" tabindex="0">
            ${cut.current ? html`<p class="note">Older messages are not shown.</p>` : null}
            ${chat && !rows.length && !waiting ? html`<p class="empty">No messages yet. Ask the manager for a task or a status; it answers from the ledger.</p>` : null}
            ${rows.map((row) => row.entry ? html`<${Entry} key=${row.key} project=${id} entry=${row.entry} />`
              : row.item ? html`<${NeedRow} key=${row.key} project=${id} item=${row.item} card flash=${flash === itemKey(row.item)} />`
              : html`<p class="resolved" key=${row.key}><${Linked} project=${id} text=${row.line.text} /></p>`)}
            ${waiting && html`<article class="message message-user" data-pending><p class="prose">${waiting.text}</p></article>`}
            ${thinking && html`
              <div class="working" role="status">
                <span class="pulse" aria-hidden="true"></span>
                <span class="working-activity">${chat.activity || "Thinking"}</span>
              </div>`}
          </div>
          ${away && html`<button class="jump" type="button" onClick=${toEnd}><${Icon} name="down" />Jump to latest</button>`}
        </div>
        ${failure && html`<p class="error chat-error" role="alert">${failure}</p>`}
        <${Tray} project=${id} items=${items} outside=${outside} onShow=${show} />
        ${can("task_act") && html`<${ManagerComposer} project=${id} busy=${thinking} field=${field}
          onSent=${(message) => { atEnd.current = true; setSent(message); load(); }} />`}
      </div>
      ${task != null && html`<${TaskSheet} key=${task} project=${id} task=${task} onClose=${() => leaveFor(`#/p/${id}`)} />`}
    </div>`;
}
