// Home with task_view: one row per chat, as a messenger lists them (docs/web-client-tasks.md
// "Home"). A row opens the chat. The projects are not rows: the Projects page lists them.
import { ago, html, Icon, Loading, READ_EVENT, unreadNews, useEffect, useFlip, useNow, useRef, useState } from "./lib.js";
import { api } from "./api.js";
import { can, chatHash, parseRoute, useStore } from "./store.js";
import { loadTasks } from "./task-api.js";
import { agentWords, needTone, needWords } from "./task-words.js";
import { shownTrays, TrayIcons, trayCounts, trayWords } from "./trays.js";
import { Dots } from "./bubbles.js";
import { toast } from "./ui.js";
import { slide } from "./swipe.js";
import { AddProject } from "./projects.js";

// The listed chats (`listed`: wsx-core `chats_home::listed`, the owner's Dismiss and a new
// message decide it) with needs first, by their most urgent kind and then their oldest need of
// that kind (a project's home chat carries the project's `needs`, each with its kind's `rank`;
// another chat with only a count of needs comes after those); then those with a message this
// device has not read, newest first; then the rest by their newest message (`last_unix_ms`).
// `viewing` is the chat this device shows, `{project, room}`: read at once.
function homeOrder(projects, tasks, viewing) {
  const rows = [];
  for (const project of projects) {
    const overview = tasks[project.id];
    for (const chat of overview?.chats ?? []) {
      // ^ A chat that runs in several projects is one row, under its own (the first of `projects`).
      if (chat.listed === false || (chat.name !== overview.home_chat && chat.projects?.[0] !== project.name)) continue;
      const home = chat.name === overview.home_chat;
      const watched = viewing?.project === project.id && (viewing.room ?? overview.home_chat) === chat.name;
      const news = watched ? null : unreadNews(home ? `project-${project.id}` : `project-${project.id}-c-${chat.name}`, home ? overview.news : chat.news);
      const first = home ? overview.needs?.[0] ?? null : null;
      const rank = first ? first.rank : chat.counts?.needs > 0 ? Infinity : null;
      rows.push({ project, overview, chat, home, first, rank, news, last: chat.last_unix_ms || 0 });
    }
  }
  const apart = (left, right) => (left > right) - (left < right);
  return rows.sort((left, right) => {
    if ((left.rank === null) !== (right.rank === null)) return left.rank === null ? 1 : -1;
    if (left.rank !== null) return apart(left.rank, right.rank) || (left.first?.unix_ms ?? 0) - (right.first?.unix_ms ?? 0) || right.last - left.last;
    if ((left.news === null) !== (right.news === null)) return left.news === null ? 1 : -1;
    if (left.news) return right.news.unix_ms - left.news.unix_ms;
    return right.last - left.last;
  });
}

// The row's one status line: the most urgent need, else what only the owner can check, else the
// manager's last reply, else whether there is any task. Agent sessions are not the project's
// news: their states show in the Sessions list.
function statusLine({ overview, first }) {
  if (first) return { text: needWords(first), tone: needTone(first) };
  // ^ What only the owner can check waits for them, before a release (docs/tasks.md "To check").
  if (overview?.to_check) return { text: `${overview.to_check} to check` };
  if (overview?.last_reply) return { text: overview.last_reply };
  // ^ Until its tasks are read, a project's row claims nothing about them.
  if (!overview) return { text: "Loading…", loading: true };
  return { text: overview.tasks.length ? "Nothing needs you" : "No tasks yet" };
}

// What a Home row says: line 1 the name, the tray icons (trays.js: Running, Needs you, To check,
// Routines, each with its count, none at 0) and the time; a chat that is not a project's first has
// its projects on a line of their own (`projects`, muted); then the status line, or, while the chat
// has a message this device has not read, that message in bold (docs/web-client-tasks.md "Home").
// `typing`: the manager's name while its turn runs.
function RowText({ name, projects, asks, counts, last, now, line, news, typing }) {
  return html`
    <span class="row-main">
      <span class="row-head">
        <span class="row-work">
          ${news ? html`<span class="news-mark" role="img" aria-label="New message"></span>` : null}<span class="row-label" data-tone=${asks ? "accent" : null}>${name}${asks ? html`<span class="visually-hidden">, Asks you</span>` : null}</span>
        </span>
        <${TrayIcons} counts=${counts} />
        ${last ? html`<span class="time">${ago(now - last)}</span>` : null}
      </span>
      ${projects ? html`<span class="row-projects">${projects}</span>` : null}
      <span class="row-context" key=${line.text} data-tone=${line.tone ?? null} data-loading=${line.loading ? "" : null} data-news=${news ? "" : null}>
        ${typing ? html`<span class="row-typing" role="img" aria-label=${`${typing} is typing`}><${Dots} /></span>` : null}${line.text}
      </span>
    </span>`;
}

// A chat's row: two lines for the project's home chat (`.home-row`, keyed by its project: its name is
// the project's, then the project's statusLine), three for any other (`.chat-row`): its shown name, the
// projects it runs in, then its unread reply, else its last reply, else its goal. A slide to the left shows Dismiss.
function ChatRow({ row, current, now, onDismiss }) {
  const { project, overview, chat, home, news } = row;
  const counts = trayCounts(home ? overview : chat);
  const projects = chat.projects.join(", ");
  const text = news?.text.split("\n")[0] ?? (home ? null : chat.last_reply || chat.goal || "No messages yet");
  const line = home && !news ? statusLine(row) : { text };
  const slidable = can("task_act");
  return html`
    <li class=${slidable ? "row row-slide" : "row"} ...${slidable ? slide : null} data-key=${home ? project.id : `${project.id}/c/${chat.name}`} data-news=${news ? "" : null}>
      <a class=${home ? "row-link home-row" : "row-link chat-row"} href=${chatHash(chat.name)} aria-current=${current ? "page" : null}
        aria-label=${home ? null : [`Chat ${chat.shown} of ${projects}`, news && "unread", ...shownTrays(counts).map((tray) => trayWords(tray, counts))].filter(Boolean).join(", ")}>
        <${RowText} name=${home ? project.name : chat.shown} projects=${home ? null : projects} asks=${home && Boolean(overview.asks?.length)} counts=${counts}
          last=${row.last} now=${now} line=${line} news=${Boolean(news)} typing=${home && overview.busy ? overview.manager : null} />
      </a>
      ${slidable && html`<button class="row-remove" data-quiet type="button" onClick=${() => onDismiss(row)}
        aria-label=${`Dismiss chat ${chat.shown}`} title="Dismiss"><${Icon} name="fail" /></button>`}
    </li>`;
}

// ^ A chat's name as the store keeps it (wsx-core `chats::checked_name`): a lowercase letter, then
// letters, digits or -, at most 24. The sheet refuses a bad or an open name before it asks.
const NAME_RULE = /^[a-z][a-z0-9-]*$/;
const NAME_HINT = "a-z, 0-9 and -, up to 24";
function nameProblem(name, taken) {
  if (!name) return "";
  if (name === "manager") return "manager: the project's first chat";
  if (!NAME_RULE.test(name)) return `${name}: ${NAME_HINT}, from a letter`;
  if (taken.includes(name)) return `${name}: already open`;
  return "";
}

// ^ The + of Home: a new chat of a project, with an optional goal and an optional agent (an
// `[[agents]]` entry; Default is the manager's). The project's first chat is its row. The project
// list ends with "Add project…" where the site manages projects, which hands over to AddProject
// (docs/web-client-tasks.md "Home").
function NewChat({ projects, tasks, project, manage, onAdd, onClose }) {
  const dialog = useRef();
  const field = useRef();
  const [chosen, setChosen] = useState(project);
  const [name, setName] = useState("");
  const [asked, setAsked] = useState(false);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const [agents, setAgents] = useState([]);
  useEffect(() => {
    dialog.current.showModal();
    field.current.focus();
    api("api/tasks/agents").then((answer) => setAgents(answer.agents ?? []), () => {});
  }, []);
  // ^ The names in use are read again when the sheet opens and with each project: a chat made a moment ago is in the list.
  useEffect(() => { loadTasks(chosen); }, [chosen]);
  const taken = (tasks[chosen]?.chats ?? []).map((chat) => chat.name);
  const problem = nameProblem(name, taken) || (asked && !name ? "Name: required" : "");
  const submit = async (event) => {
    event.preventDefault();
    setAsked(true);
    if (!name || nameProblem(name, taken)) {
      field.current.focus();
      return;
    }
    const form = new FormData(event.target);
    setBusy(true);
    setError("");
    try {
      await api(`api/tasks/${chosen}/chats`, { method: "POST", json: { name, goal: form.get("goal"), agent: form.get("agent") } });
    } catch (failure) {
      setError(failure.message);
      setBusy(false);
      return;
    }
    dialog.current.close();
    location.hash = chatHash(name);
  };
  const standard = agentWords(tasks[chosen]?.default_agent);
  return html`
    <dialog class="sheet" ref=${dialog} aria-labelledby="new-chat-title" onClose=${onClose}>
      <form onSubmit=${submit} noValidate>
        <h2 class="sheet-title" id="new-chat-title">New chat</h2>
        <label class="field">Project
          <select class="input" name="project" value=${chosen} onChange=${(event) => (event.target.value === "add" ? onAdd() : setChosen(Number(event.target.value)))}>
            ${projects.map((each) => html`<option key=${each.id} value=${each.id}>${each.name}</option>`)}
            ${manage && html`<option value="add">Add project…</option>`}
          </select>
        </label>
        <label class="field">Name
          <input class="input" type="text" name="name" ref=${field} value=${name} placeholder="research" maxlength="24"
            aria-invalid=${problem ? "true" : null} aria-describedby="new-chat-name-note"
            onInput=${(event) => setName(event.target.value.toLowerCase().replace(/\s+/g, "-"))}
            autocomplete="off" autocapitalize="none" autocorrect="off" spellcheck=${false} enterkeyhint="go" />
          <span class="field-note" id="new-chat-name-note" data-tone=${problem ? "danger" : null} role=${problem ? "alert" : null}>${problem || NAME_HINT}</span>
        </label>
        <label class="field">Goal (optional)
          <textarea class="input" name="goal" rows="3" placeholder="What the chat is for"
            onKeyDown=${(event) => { if (event.key === "Enter" && (event.metaKey || event.ctrlKey)) event.target.form.requestSubmit(); }}></textarea>
        </label>
        ${agents.length > 0 && html`
          <label class="field">Agent
            <select class="input" name="agent">
              <option value="">${standard ? `Default (${standard})` : "Default"}</option>
              ${agents.map((agent) => html`<option key=${agent.name} value=${agent.name}>${agent.name} · ${agent.model}</option>`)}
            </select>
          </label>`}
        <p class="error" role="alert">${error}</p>
        <div class="actions sheet-actions">
          <button class="button" type="button" onClick=${() => dialog.current.close()}>Cancel</button>
          <button class="button button-primary" type="submit" disabled=${busy}>Create</button>
        </div>
      </form>
    </dialog>`;
}

export function Home({ rail }) {
  const store = useStore();
  // ^ The chat keeps what it read as it goes, and as it closes, which is after this list drew.
  const [, renew] = useState(0);
  useEffect(() => {
    const changed = () => renew((count) => count + 1);
    addEventListener(READ_EVENT, changed);
    changed();
    return () => removeEventListener(READ_EVENT, changed);
  }, []);
  // ^ Only a working task shows an age that moves by the second; any other row's age (`ago`, in
  // minutes) is renewed each minute.
  const live = Object.values(store.tasks).some((overview) => overview?.working > 0);
  const now = useNow(true, live ? 1000 : 60_000);
  // ^ The project the open screen belongs to, and its chat when that is not the home chat.
  const route = parseRoute(location.hash);
  const at = route.screen === "chat" ? route : location.hash.startsWith("#/p/") ? route : null;
  const open = at?.id ?? null;
  const inChat = at?.room ?? null;
  const list = useRef();
  useFlip(list);
  const [sheet, setSheet] = useState(null);
  const manage = store.site.manage_projects;
  // ^ A chat is the owner's to open where the site acts on tasks; the first chat of a project always exists.
  // ^ A site that manages projects but may not act on tasks has nothing to chat in: its + adds a project.
  const plus = can("task_act") && store.projects.length > 0 ? "chat" : manage ? "project" : null;
  // ^ The row goes at once; the chat, its project and its tasks go on, and a new message brings it back.
  const dismiss = async ({ project, chat }) => {
    try {
      await api(`api/tasks/${project.id}/chats/${encodeURIComponent(chat.name)}/dismiss`, { method: "POST" });
      await loadTasks(project.id);
    } catch (failure) {
      toast(failure.message, { failure: true });
    }
  };
  let body;
  if (!store.daemon) body = html`<p class="empty">wsx is not running on the host.</p>`;
  else if (!store.loaded) body = html`<${Loading} />`;
  else if (!store.projects.length) body = html`<p class="empty">wsx manages no project that this site may reach.</p>`;
  else {
    const viewing = rail && open != null && document.visibilityState === "visible" ? { project: open, room: inChat } : null;
    const rows = homeOrder(store.projects, store.tasks, viewing);
    if (rows.length) {
      body = html`
        <ul class="rows home-rows" aria-label="Chats" ref=${list}>
          ${rows.map((row) => html`<${ChatRow} key=${`${row.project.id}/${row.chat.name}`} row=${row} now=${now} onDismiss=${dismiss}
            current=${rail && open === row.project.id && (inChat ?? row.overview.home_chat) === row.chat.name} />`)}
        </ul>`;
    } else if (store.projects.some((project) => !store.tasks[project.id])) body = html`<${Loading} />`;
    else body = html`<p class="empty">No chat on Home. A new message or a new chat shows here.</p>`;
  }
  return html`
    <div class="page">
      ${(!rail || plus) && html`
        <div class="home-top">
          ${!rail && html`<h1 class="title">Home</h1>`}
          ${plus && html`<button class="icon-button new-chat" type="button" onClick=${() => setSheet(plus)} aria-label=${plus === "chat" ? "New chat" : "Add project"} title=${plus === "chat" ? "New chat" : "Add project"}><${Icon} name="plus" /></button>`}
        </div>`}
      ${sheet === "chat" && html`<${NewChat} projects=${store.projects} tasks=${store.tasks} project=${store.projects.some((each) => each.id === open) ? open : store.projects[0].id}
        manage=${manage} onAdd=${() => setSheet("project")} onClose=${() => setSheet(null)} />`}
      ${sheet === "project" && html`<${AddProject} onClose=${() => setSheet(null)} />`}
      <div class="scroll">
        ${body}
      </div>
    </div>`;
}
