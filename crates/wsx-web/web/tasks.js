// Manager mode on a project: the "Needs you" strip, the manager chat with live task chips,
// and the `#` task picker (docs/web-client.md "Tasks"). Fixed actions go straight to wsx.
import { api } from "./api.js";
import { html, Icon, useEffect, useLayoutEffect, useRef, useState } from "./lib.js";
import { can, events, state, taskSeen, touch, update, useStore } from "./store.js";
import { Back, toast } from "./ui.js";
import { Markdown } from "./markdown.js";
import { grow, isolate, release, Suggestions } from "./chat.js";
import { filesLink } from "./projects.js";

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
  starting: ["starting", "accent"], working: ["working", "accent"], checking: ["checking", "accent"],
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
  land: "ready to land", review: "in review", done: "done",
};
const KIND_TONE = { blocked: "attention", conflict: "danger", failed: "danger", approval: "attention", verify: "accent", land: "accent", review: "muted", done: "muted" };
const itemKey = (item) => item.kind === "approval" ? item.approval : `${item.kind}-${item.task}`;

// The items that still need this device: a done task drops out once its page was opened here.
export const needing = (project, overview) =>
  (overview?.needs ?? []).filter((item) => item.kind !== "done" || !taskSeen(project, item.task, item.unix_ms));

// Drafts and outcomes of actions, by `<project>/<item>`, kept across the strip's re-renders.
const pending = new Map();
const actionOf = (key) => {
  if (!pending.has(key)) pending.set(key, { field: null, text: "", busy: false, error: "" });
  return pending.get(key);
};

// What a fixed action did, as a notice: wsx's own answer, or the decision.
export function announce(name, action, answer) {
  toast(action === "approve" ? `${name} approved; it runs now.` : action === "deny" ? `${name} denied.` : `${name}: ${answer.message || "done"}`);
}

// One fixed action. A success says what wsx did and clears the draft; a failure keeps it.
async function act(project, item, action, body = {}) {
  const path = item.kind === "approval"
    ? `api/tasks/${project}/approvals/${item.approval}/${action}`
    : `api/tasks/${project}/t/${item.task}/${action}`;
  const answer = await api(path, { method: "POST", json: body });
  const name = item.kind === "approval" ? item.approval : `#${item.task}`;
  announce(name, action, answer);
  loadTasks(project);
  return answer;
}

function NeedRow({ project, item }) {
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
    if (!text) return;
    if (draft.field === "answer") run("add", { text });
    else if (draft.field === "allow") run("allow", { tool: text });
  };
  const button = (label, onClick, extra = {}) => html`
    <button class="button need-action" type="button" disabled=${draft.busy} onClick=${onClick} ...${extra}>${label}</button>`;
  let actions = null;
  if (acting && item.kind === "blocked") {
    actions = html`
      ${item.denials?.length ? button("Allow", () => open("allow", item.denials[0]), { "aria-expanded": draft.field === "allow" }) : null}
      ${button("Answer", () => open("answer"), { "aria-expanded": draft.field === "answer" })}`;
  } else if (acting && item.kind === "approval") {
    actions = html`${button("Deny", () => run("deny"))}${button("Approve", () => run("approve"))}`;
  } else if (acting && (item.kind === "land" || item.kind === "review")) {
    actions = button(item.kind === "land" ? "Land" : "Check", () => run("land"), { "aria-label": `${item.kind === "land" ? "Land" : "Check the pull request of"} #${item.task}` });
  } else if (item.kind === "verify" && item.task != null) {
    actions = html`<a class="button need-action" href=${taskLink(project, item.task)}>Verify</a>`;
  }
  const context = item.kind === "approval"
    ? `approve ${item.action}${item.task != null ? ` #${item.task}${item.revision != null ? ` r${item.revision}` : ""}` : ""}`
    : [KIND_WORDS[item.kind], item.summary].filter(Boolean).join(" · ");
  const label = item.kind === "approval" ? item.title ?? item.commit ?? item.action : item.title;
  const name = item.kind === "approval" ? item.approval : `#${item.task}`;
  const body = html`
    <span class="row-work"><span class="need-id">${name}</span><span class="row-label">${label}</span></span>
    <span class="row-context">${context}</span>`;
  return html`
    <li class="row need">
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
            aria-label=${draft.field === "answer" ? `Answer to #${item.task}` : `Tool to allow for #${item.task}`}
            placeholder=${draft.field === "answer" ? "Answer" : "Tool, such as Bash(make:*)"}
            onInput=${(event) => { draft.text = event.target.value; }} disabled=${draft.busy} />
          <button class="button" type="submit" disabled=${draft.busy}>${draft.field === "answer" ? "Send" : "Allow"}</button>
        </form>`}
      ${draft.error && html`<p class="error need-error" role="alert">${draft.error}</p>`}
    </li>`;
}

// "Needs you": what waits for the owner, most urgent first. Two items, then the rest on request;
// one line when nothing waits.
const SHOWN = 2;
export function Needs({ project, overview, all }) {
  const [open, setOpen] = useState(false);
  const items = needing(project, overview);
  if (!overview) return null;
  if (!items.length) return html`<p class="needs-none">Nothing needs you.</p>`;
  const shown = open || all ? items : items.slice(0, SHOWN);
  // ^ With the keyboard up or on a short screen the rows give way to one line, so the
  // conversation stays in view (CSS: body[data-short], body[data-keyboard]).
  const names = items.map((item) => item.kind === "approval" ? item.approval : `#${item.task}`);
  return html`
    <section class="needs" aria-label="Needs you">
      <p class="needs-line">Needs you: ${names.join(", ")}</p>
      <ul class="rows">${shown.map((item) => html`<${NeedRow} key=${itemKey(item)} project=${project} item=${item} />`)}</ul>
      ${!all && items.length > SHOWN && html`
        <button class="link-action needs-more" type="button" aria-expanded=${open} onClick=${() => setOpen(!open)}>
          ${open ? "Show fewer" : `Show ${items.length - SHOWN} more`}</button>`}
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

function ManagerComposer({ project, busy: thinking, onSent }) {
  const store = useStore();
  const field = useRef();
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

// The project screen with task_view: one manager chat under the Needs-you strip.
export function Manager({ project: id }) {
  const store = useStore();
  const project = store.projects.find((candidate) => candidate.id === id);
  const overview = store.tasks[id];
  const [chat, setChat] = useState(null);
  const [failure, setFailure] = useState("");
  const [sent, setSent] = useState(null);
  const [away, setAway] = useState(false);
  const log = useRef();
  const atEnd = useRef(true);
  const loading = useRef(false);
  const again = useRef(false);
  const shown = useRef([]);

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
        shown.current = [...shown.current, ...next.entries].slice(-400);
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
  const scrolled = () => {
    const node = log.current;
    if (!node) return;
    atEnd.current = node.scrollHeight - node.scrollTop - node.clientHeight < 80;
    setAway(!atEnd.current);
  };
  const toEnd = () => { log.current.scrollTop = log.current.scrollHeight; atEnd.current = true; setAway(false); };
  useLayoutEffect(() => { if (atEnd.current && log.current) log.current.scrollTop = log.current.scrollHeight; });

  if (!project) {
    return html`
      <div class="page">
        <header class="bar"><${Back} fallback="#/projects" /></header>
        <p class="empty">${!store.daemon ? "wsx is not running on the host." : store.loaded ? "This project is not available." : "Loading…"}</p>
      </div>`;
  }
  const entries = chat?.entries ?? [];
  // The sent message shows until the log has it.
  const waiting = sent && !entries.some((entry) => entry.kind === "message" && entry.id === sent.id) ? sent : null;
  const thinking = Boolean(chat?.busy);
  const managerWord = !chat ? "" : thinking ? "manager thinking" : chat.running ? "manager idle" : "manager not started";
  const counts = [overview?.working && `${overview.working} working`, overview?.stopped && `${overview.stopped} stopped`].filter(Boolean);
  return html`
    <div class="page manager">
      <header class="bar">
        <${Back} fallback="#/projects" />
        <div class="bar-title">
          <span class="bar-label">${project.name}</span>
          <span class="bar-state" data-tone=${thinking ? "accent" : null}>${managerWord}</span>
        </div>
        <div class="bar-tools">
          <a class="link-action bar-link" href=${`#/p/${id}/sessions`}>Sessions</a>
          ${filesLink(project)}
        </div>
      </header>
      <div class="chat">
        <${SinceLine} project=${id} />
        <${Needs} project=${id} overview=${overview} />
        ${counts.length ? html`<p class="tasks-working">${counts.join(" · ")}</p>` : null}
        <div class="log-wrap">
          <div class="log" ref=${log} onScroll=${scrolled} role="log" aria-label="Conversation with the manager" tabindex="0">
            ${chat?.skipped ? html`<p class="note">Older messages are not shown.</p>` : null}
            ${chat && !entries.length && !waiting ? html`<p class="empty">No messages yet. Ask the manager for a task or a status; it answers from the ledger.</p>` : null}
            ${entries.map((entry, index) => html`<${Entry} key=${entry.id != null ? `${entry.kind}-${entry.id}` : `n${index}`} project=${id} entry=${entry} />`)}
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
        ${can("task_act") && html`<${ManagerComposer} project=${id} busy=${thinking}
          onSent=${(message) => { atEnd.current = true; setSent(message); load(); }} />`}
      </div>
    </div>`;
}
