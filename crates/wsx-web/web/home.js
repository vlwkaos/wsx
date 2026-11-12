// Home with task_view: one row per granted project, what needs the owner first
// (docs/web-client-tasks.md "Home"). A row opens the project's manager chat.
import { ago, html, Icon, Loading, READ_EVENT, unreadNews, useEffect, useFlip, useNow, useRef, useState } from "./lib.js";
import { api } from "./api.js";
import { can, isAgent, listedSessions, stateOf, useStore } from "./store.js";
import { needTone, needWords } from "./task-words.js";
import { Dots } from "./bubbles.js";
import { slid } from "./ui.js";

// The order of need kinds, most urgent first, as the server ranks them.
const URGENCY = { waiting: 0, failed: 1, approval: 2, land: 3 };

// Projects with needs first, by their most urgent kind and then their oldest need of that
// kind; then those with a message this device has not read, newest message first; then the
// rest by their last owner-facing activity, newest first (docs/web-client-tasks.md "Home").
// `viewing` is the project whose chat this device shows: read at once.
function homeOrder(projects, tasks, sessions, viewing) {
  const rows = projects.map((project) => {
    const overview = tasks[project.id];
    const needs = overview?.needs ?? [];
    const rank = needs.length ? Math.min(...needs.map((item) => URGENCY[item.kind] ?? 5)) : null;
    const first = needs.filter((item) => (URGENCY[item.kind] ?? 5) === rank)
      .reduce((oldest, item) => (oldest && oldest.unix_ms <= item.unix_ms ? oldest : item), null);
    const agents = sessions.filter((session) => session.project.id === project.id);
    const last = Math.max(overview?.last_unix_ms ?? 0, ...agents.map((session) => session.state_since_unix_ms ?? 0)) || null;
    const news = project.id === viewing ? null : newsOf(project.id, overview);
    return { project, overview, needs, rank, first, agents, last, news };
  });
  return rows.sort((left, right) => {
    if ((left.rank === null) !== (right.rank === null)) return left.rank === null ? 1 : -1;
    if (left.rank !== null) return left.rank - right.rank || left.first.unix_ms - right.first.unix_ms;
    if ((left.news === null) !== (right.news === null)) return left.news === null ? 1 : -1;
    if (left.news) return right.overview.news.unix_ms - left.overview.news.unix_ms;
    return (right.last ?? 0) - (left.last ?? 0);
  });
}

// The project's agent sessions in words, most urgent group first: "claude in main exited",
// "2 working", "claude in main done 5m ago". Null without one.
const SESSION_GROUPS = ["blocked", "error", "exited", "working", "done", "idle", "unknown"];
// Session states that need the owner, and working: their words take the state's tone.
const SESSION_ATTENTION = ["blocked", "error", "exited"];
const SESSION_TONED = [...SESSION_ATTENTION, "working"];
function sessionWords(agents, now) {
  for (const key of SESSION_GROUPS) {
    const group = agents.filter((session) => stateOf(session).key === key)
      .sort((left, right) => (right.state_since_unix_ms ?? 0) - (left.state_since_unix_ms ?? 0));
    if (!group.length) continue;
    const { word, tone } = stateOf(group[0]);
    const done = key === "done" && group[0].state_since_unix_ms ? ago(now - group[0].state_since_unix_ms) : null;
    const since = done ? ` ${done === "now" ? "just now" : done}` : "";
    const who = group.length === 1 ? `${group[0].agent.provider} in ${group[0].branch}` : `${group.length}`;
    return { text: `${who} ${word}${since}`, tone: SESSION_TONED.includes(key) ? tone : null, key };
  }
  return null;
}

// The row's one status line: the most urgent need, else a session that needs attention or
// works, else the manager's last reply, else the other sessions, else whether there is any task.
function statusLine({ overview, first, agents }, now) {
  if (first) return { text: needWords(first), tone: needTone(first) };
  const sessions = sessionWords(agents, now);
  if (sessions && (SESSION_ATTENTION.includes(sessions.key) || sessions.key === "working")) return sessions;
  // ^ What only the owner can check waits for them, before a release (docs/tasks.md "To check").
  if (overview?.to_check) return { text: `${overview.to_check} to check` };
  if (overview?.last_reply) return { text: overview.last_reply };
  if (sessions) return sessions;
  // ^ Until its tasks are read, a project's row claims nothing about them.
  if (!overview) return { text: "Loading…", loading: true };
  return { text: overview.tasks.length ? "Nothing needs you" : "No tasks yet" };
}

// ^ The project's newest message its chat counts as new (a reply that is not quiet, a To check
// item), while this device has not read the chat that far: the same mark as the chat's own
// "↓ N new" (lib.js `useFollowEnd`). A chat never opened on this device has nothing new.
function newsOf(project, overview) {
  return unreadNews(`project-${project}`, overview?.news)?.text.split("\n")[0] ?? null;
}

// ^ A sheet with one path field registers a git repository of the host, as the TUI's `p` does
// (docs/web-client.md "Projects"). The server's refusal stays in the sheet; the row comes with
// the next workspace event.
function AddProject({ onClose }) {
  const dialog = useRef();
  const field = useRef();
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  useEffect(() => {
    dialog.current.showModal();
    field.current.focus();
  }, []);
  const submit = async (event) => {
    event.preventDefault();
    setBusy(true);
    setError("");
    try {
      await api("api/projects", { method: "POST", json: { path: field.current.value } });
      dialog.current.close();
    } catch (failure) {
      setError(failure.message);
      setBusy(false);
      field.current.focus();
    }
  };
  return html`
    <dialog class="sheet" ref=${dialog} aria-labelledby="add-project-title" onClose=${onClose}>
      <form onSubmit=${submit}>
        <h2 class="sheet-title" id="add-project-title">Add project</h2>
        <label class="field">Path of a git repository
          <input class="input" type="text" name="path" ref=${field} placeholder="~/code/app" required autocomplete="off"
            autocapitalize="none" autocorrect="off" spellcheck=${false} enterkeyhint="go" />
        </label>
        <p class="error" role="alert">${error}</p>
        <div class="actions sheet-actions">
          <button class="button" type="button" onClick=${() => dialog.current.close()}>Cancel</button>
          <button class="button button-primary" type="submit" disabled=${busy}>Add</button>
        </div>
      </form>
    </dialog>`;
}

// ^ The confirmation of Remove: it unregisters the project from wsx only (its folder, worktrees,
// branches and running sessions stay), or, if ticked, also halts all its sessions, the hidden
// task ones too, as the TUI's `d` asks (docs/web-client.md "Projects"). A failure stays in the sheet.
function RemoveProject({ project, onClose }) {
  const dialog = useRef();
  const [halt, setHalt] = useState(false);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  useEffect(() => dialog.current.showModal(), []);
  const submit = async (event) => {
    event.preventDefault();
    setBusy(true);
    setError("");
    try {
      await api(`api/projects/${project.id}${halt ? "?halt=true" : ""}`, { method: "DELETE" });
      dialog.current.close();
    } catch (failure) {
      setError(failure.message);
      setBusy(false);
    }
  };
  return html`
    <dialog class="sheet" ref=${dialog} aria-labelledby="remove-project-title" onClose=${onClose}>
      <form onSubmit=${submit}>
        <h2 class="sheet-title" id="remove-project-title">Remove ${project.name}?</h2>
        <p class="muted">The folder, worktrees, branches and tasks stay.</p>
        <label class="option">
          <input type="checkbox" name="halt" checked=${halt} onChange=${(event) => setHalt(event.target.checked)} />
          Also halt all its sessions (agents, shells and task sessions)
        </label>
        <p class="error" role="alert">${error}</p>
        <div class="actions sheet-actions">
          <button class="button" type="button" onClick=${() => dialog.current.close()}>Cancel</button>
          <button class="button button-primary" type="submit" disabled=${busy}>Remove</button>
        </div>
      </form>
    </dialog>`;
}

// An open chat besides the first: its own row under the project's, the chat's name with the
// project under it, a count of its pending approvals, and while the chat has a reply this device
// has not read, a dot and that reply's first line (docs/web-client-tasks.md "Home").
function ChatRow({ project, chat, current }) {
  const news = unreadNews(`project-${project.id}-c-${chat.name}`, chat.news)?.text.split("\n")[0] ?? null;
  const count = chat.approvals;
  return html`
    <li class="row row-chat" data-key=${`${project.id}/c/${chat.name}`}>
      <a class="row-link chat-row" href=${`#/p/${project.id}/c/${encodeURIComponent(chat.name)}`} aria-current=${current ? "page" : null}
        aria-label=${`Chat ${chat.name} of ${project.name}${news ? ", unread" : ""}`}>
        <span class="row-main">
          <span class="row-work">
            ${news && html`<span class="news-mark" aria-hidden="true"></span>`}<span class="row-label">${chat.name}</span>
            ${count ? html`<span class="count" key=${count} data-tone="attention" aria-label=${`${count} ${count === 1 ? "needs" : "need"} you`}>${count}</span>` : null}
          </span>
          <span class="row-context">${project.name}</span>
          ${news && html`<span class="row-news">${news}</span>`}
        </span>
      </a>
    </li>`;
}

// ^ The + of Home: a chat of a project. The project list ends with "Add project…" where the site
// manages projects, which hands over to AddProject. With no name the sheet opens the project's
// first chat, which always exists; a name makes a chat with an optional goal and an optional
// agent (an `[[agents]]` entry; docs/web-client-tasks.md "Home").
function NewChat({ projects, project, manage, onAdd, onClose }) {
  const dialog = useRef();
  const [chosen, setChosen] = useState(project);
  const [name, setName] = useState("");
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const [agents, setAgents] = useState([]);
  useEffect(() => {
    dialog.current.showModal();
    api("api/tasks/agents").then((answer) => setAgents(answer.agents ?? []), () => {});
  }, []);
  const submit = async (event) => {
    event.preventDefault();
    const form = new FormData(event.target);
    const id = form.get("project");
    const chat = name.trim();
    if (chat) {
      setBusy(true);
      setError("");
      try {
        await api(`api/tasks/${id}/chats`, { method: "POST", json: { name: chat, goal: form.get("goal"), agent: form.get("agent") } });
      } catch (failure) {
        setError(failure.message);
        setBusy(false);
        return;
      }
    }
    dialog.current.close();
    location.hash = chat ? `#/p/${id}/c/${encodeURIComponent(chat)}` : `#/p/${id}`;
  };
  return html`
    <dialog class="sheet" ref=${dialog} aria-labelledby="new-chat-title" onClose=${onClose}>
      <form onSubmit=${submit}>
        <h2 class="sheet-title" id="new-chat-title">New chat</h2>
        <label class="field">Project
          <select class="input" name="project" value=${chosen} onChange=${(event) => (event.target.value === "add" ? onAdd() : setChosen(Number(event.target.value)))}>
            ${projects.map((each) => html`<option key=${each.id} value=${each.id}>${each.name}</option>`)}
            ${manage && html`<option value="add">Add project…</option>`}
          </select>
        </label>
        <label class="field">Name
          <input class="input" type="text" name="name" value=${name} onInput=${(event) => setName(event.target.value)} maxlength="24"
            autocomplete="off" autocapitalize="none" autocorrect="off" spellcheck=${false} enterkeyhint="go" />
        </label>
        <label class="field">Goal
          <textarea class="input" name="goal" rows="3" disabled=${!name.trim()}></textarea>
        </label>
        ${agents.length > 0 && html`
          <label class="field">Agent
            <select class="input" name="agent" disabled=${!name.trim()}>
              <option value="">Default</option>
              ${agents.map((agent) => html`<option key=${agent.name} value=${agent.name}>${agent.name} · ${agent.model}</option>`)}
            </select>
          </label>`}
        <p class="error" role="alert">${error}</p>
        <div class="actions sheet-actions">
          <button class="button" type="button" onClick=${() => dialog.current.close()}>Cancel</button>
          <button class="button button-primary" type="submit" disabled=${busy}>${name.trim() ? "Create" : "Open"}</button>
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
  const sessions = listedSessions(store.sessions).filter(isAgent);
  // ^ Only a working task or session shows an age that moves by the second; any other row's age
  // (`ago`, in minutes) is renewed each minute.
  const live = Object.values(store.tasks).some((overview) => overview?.working > 0)
    || sessions.some((session) => stateOf(session).key === "working");
  const now = useNow(true, live ? 1000 : 60_000);
  const [, kind, open, place, chatName] = location.hash.split("/");
  const inChat = place === "c" ? decodeURIComponent(chatName ?? "") : null;
  const list = useRef();
  useFlip(list);
  const [sheet, setSheet] = useState(null);
  const [removing, setRemoving] = useState(null);
  const manage = store.site.manage_projects;
  // ^ A chat is the owner's to open where the site acts on tasks; the first chat of a project always exists.
  const canChat = can("task_act") && store.projects.length > 0;
  let body;
  if (!store.daemon) body = html`<p class="empty">wsx is not running on the host.</p>`;
  else if (!store.loaded) body = html`<${Loading} />`;
  else if (!store.projects.length) body = html`<p class="empty">wsx manages no project that this site may reach.</p>`;
  else {
    body = html`
      <ul class="rows home-rows" aria-label="Projects" ref=${list}>
        ${homeOrder(store.projects, store.tasks, sessions, rail && kind === "p" && document.visibilityState === "visible" ? Number(open) : null).map((row) => {
          const status = statusLine(row, now);
          const count = row.needs.length;
          const working = row.overview?.working ?? 0;
          const last = row.last;
          const news = row.news;
          return [html`
            <li class=${manage ? "row row-slide" : "row"} onScroll=${manage ? slid : null} key=${row.project.id} data-key=${row.project.id} data-news=${news ? "" : null}>
              <a class="row-link home-row" href=${`#/p/${row.project.id}`} aria-current=${rail && Number(open) === row.project.id && inChat === null ? "page" : null}>
                <span class="row-main">
                  <span class="row-work">
                    ${news ? html`<span class="news-mark" role="img" aria-label="New message"></span>` : null}<span class="row-label" data-tone=${row.overview?.asks?.length ? "accent" : null}>${row.project.name}${row.overview?.asks?.length ? html`<span class="visually-hidden">, Asks you</span>` : null}</span>
                    ${count ? html`<span class="count" key=${count} data-tone="attention" aria-label=${`${count} ${count === 1 ? "needs" : "need"} you`}>${count}</span>` : null}
                  </span>
                  <span class="row-context" key=${status.text} data-tone=${status.tone ?? null} data-loading=${status.loading ? "" : null}>
                    ${row.overview?.busy ? html`<span class="row-typing" role="img" aria-label=${`${row.overview.manager} is typing`}><${Dots} /></span>` : null}${news && status.text === row.overview?.last_reply ? null : status.text}
                  </span>
                  ${news ? html`<span class="row-news">${news}</span>` : null}
                </span>
                <span class="row-state">
                  ${working || status.key === "working" ? html`<span class="pulse" aria-hidden="true"></span>` : null}
                  ${working ? html`<span data-tone="working" class="working-count">${working} working</span>` : null}
                  ${last ? html`<span class="time">${ago(now - last)}</span>` : null}
                </span>
              </a>
              ${manage && html`<button class="row-remove" type="button" onClick=${() => setRemoving(row.project)}
                aria-label=${`Remove project ${row.project.name}`} title="Remove"><${Icon} name="fail" /></button>`}
            </li>`, ...(row.overview?.chats ?? []).map((chat) => html`<${ChatRow} key=${`${row.project.id}/c/${chat.name}`} project=${row.project} chat=${chat}
              current=${rail && Number(open) === row.project.id && inChat === chat.name} />`)];
        })}
      </ul>`;
  }
  return html`
    <div class="page">
      ${(!rail || canChat) && html`
        <div class="home-top">
          ${!rail && html`<h1 class="title">Home</h1>`}
          ${canChat && html`<button class="icon-button new-chat" type="button" onClick=${() => setSheet("chat")} aria-label="New chat" title="New chat"><${Icon} name="plus" /></button>`}
        </div>`}
      ${sheet === "chat" && html`<${NewChat} projects=${store.projects} project=${kind === "p" && store.projects.some((each) => each.id === Number(open)) ? Number(open) : store.projects[0].id}
        manage=${manage} onAdd=${() => setSheet("project")} onClose=${() => setSheet(null)} />`}
      ${sheet === "project" && html`<${AddProject} onClose=${() => setSheet(null)} />`}
      ${removing && html`<${RemoveProject} project=${removing} onClose=${() => setRemoving(null)} />`}
      <div class="scroll">
        ${body}
      </div>
    </div>`;
}
