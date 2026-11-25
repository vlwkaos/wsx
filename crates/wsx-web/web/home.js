// Home with task_view: one row per chat, as a messenger lists them (docs/web-client-tasks.md
// "Home"). A row opens the chat. The projects are not rows: the Projects page lists them.
import { ago, html, Icon, keepIn, Loading, READ_EVENT, readMap, unreadNews, useEffect, useFlip, useNow, useRef, useState } from "./lib.js";
import { api } from "./api.js";
import { can, chatHash, parseRoute, useStore } from "./store.js";
import { loadTasks, useForYou } from "./task-api.js";
import { agentWords, needTone, needWords } from "./task-words.js";
import { forYouWords, shownTrays, TrayIcons, trayCounts, trayWords } from "./trays.js";
import { Dots } from "./bubbles.js";
import { toast } from "./ui.js";
import { slide } from "./swipe.js";
import { AddProject } from "./projects.js";

// The listed chats (`listed`: wsx-core `chats_home::listed`, the owner's Dismiss and a new
// message decide it) with needs first, by their most urgent kind and then their oldest need of
// that kind (a chat's `first` is its most urgent need, with its kind's `rank`; a chat with only a
// count of needs, a question, comes after those); then those with a message this
// device has not read, newest first; then the rest by their newest message (`last_unix_ms`).
// `viewing` is the chat this device shows, `{project, room}`: read at once.
function homeOrder(projects, tasks, viewing) {
  const rows = [];
  for (const project of projects) {
    const overview = tasks[project.id];
    for (const chat of overview?.chats ?? []) {
      // ^ A chat that runs in several projects is one row, under its own (the first of `projects`).
      if (chat.listed === false || (!chat.main && chat.projects?.[0] !== project.name)) continue;
      const main = chat.main === true;
      const watched = viewing?.project === project.id && (viewing.room ?? overview.main_chat) === chat.name;
      // ^ One rule for every chat: its own row says what is new, what it needs and whether it works.
      const news = watched ? null : unreadNews(main ? `project-${project.id}` : `project-${project.id}-c-${chat.name}`, chat.news);
      const first = chat.first ?? null;
      const rank = first ? first.rank : chat.counts?.needs > 0 ? Infinity : null;
      rows.push({ project, overview, chat, main, first, rank, news, last: chat.last_unix_ms || 0 });
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

// Home's folders (`chat.folder`, wsx-core `chats_home::sections`, the owner's, set in the ⋯ menu): the
// rows of `homeOrder` split by folder, a folder where its first row is, so folders go by their newest
// row and each keeps its rows' order; the rows of no folder come last (a null name, headed "No folder" once a folder exists).
function folderSections(rows) {
  const sections = [];
  for (const row of rows) {
    const name = row.chat.folder ?? null;
    const found = sections.find((section) => section.name === name);
    if (found) found.rows.push(row);
    else sections.push({ name, rows: [row] });
  }
  return [...sections.filter((section) => section.name), ...sections.filter((section) => !section.name)];
}

// The row's one status line, the same for every chat: the unread message, else the most urgent
// need, else what only the owner can check, else the last reply, else the goal, else whether the
// chat has any task. Agent sessions are not the chat's news: their states show in the Sessions list.
function statusLine({ chat, news }) {
  if (news) return { text: news.text.split("\n")[0] };
  if (chat.first) return { text: needWords(chat.first), tone: needTone(chat.first) };
  // ^ What only the owner can check waits for them, before a release (docs/tasks.md "To check").
  if (chat.counts?.check) return { text: `${chat.counts.check} to check` };
  return { text: chat.last_reply || chat.goal || (chat.counts?.open ? "Nothing needs you" : "No tasks yet") };
}

// What a Home row says: line 1 the name, the tray icons (trays.js: Running, Needs you, To check,
// Routines, each with its count, none at 0) and the time; a chat that is not a project's first has
// its projects on a line of their own (`projects`, muted); then the status line, or, while the chat
// has a message this device has not read, that message in bold (docs/web-client-tasks.md "Home").
// `typing`: the manager's name while its turn runs.
// ^ `lead`: the project's main chat with no goal, its lead; the name reads "<project> lead" as the lead's replies do.
function RowText({ name, lead, projects, asks, counts, last, now, line, news, typing }) {
  return html`
    <span class="row-main">
      <span class="row-head">
        <span class="row-work">
          ${news ? html`<span class="news-mark" role="img" aria-label="New message"></span>` : null}<span class="row-label" data-tone=${asks ? "accent" : null} data-role=${lead ? "lead" : null}>${name}${asks ? html`<span class="visually-hidden">, Asks you</span>` : null}</span>
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

// A chat's row, built from the chat's own row of the server (`overview.chats`), the same for every
// chat: its state (typing, unread, asks, counts) and its status line. Only the name differs: the
// project's main chat has two lines (`.home-row`, keyed by its project, named by it); any other has
// three (`.chat-row`): its shown name, the projects it runs in, then the status line. A slide to the left shows Dismiss;
// the ⋯ and a long press on a phone (`PRESS_MS`) open the folder sheet.
const FOLDED_KEY = "wsx-home-folded";
const PRESS_MS = 500;
const PRESS_SLOP_PX = 10;
function ChatRow({ row, current, now, onDismiss, onFolder }) {
  const { project, overview, chat, main, news } = row;
  // ^ The routines are the project's, so only its main chat's row shows their tray.
  const counts = trayCounts(chat, main ? overview.routines : undefined);
  const projects = chat.projects.join(", ");
  const line = statusLine(row);
  const slidable = can("task_act");
  // ^ A touch held on the row opens the sheet; a move past the slop is a scroll or a slide, and the click that ends the press is not a tap.
  const press = useRef({ timer: 0, at: null, done: false, touch: false });
  const release = () => { clearTimeout(press.current.timer); press.current.timer = 0; };
  const down = (event) => {
    press.current.touch = event.pointerType === "touch";
    if (!press.current.touch || !slidable) return;
    press.current.done = false;
    press.current.at = [event.clientX, event.clientY];
    press.current.timer = setTimeout(() => { press.current.done = true; onFolder(row); }, PRESS_MS);
  };
  const move = (event) => {
    const [x, y] = press.current.at ?? [event.clientX, event.clientY];
    if (Math.hypot(event.clientX - x, event.clientY - y) > PRESS_SLOP_PX) release();
  };
  const click = (event) => {
    if (!press.current.done) return;
    press.current.done = false;
    event.preventDefault();
  };
  return html`
    <li class=${slidable ? "row row-slide" : "row"} ...${slidable ? slide : null} data-key=${main ? project.id : `${project.id}/c/${chat.name}`} data-news=${news ? "" : null}
      onPointerDown=${down} onPointerMove=${move} onPointerUp=${release} onPointerCancel=${release} onClickCapture=${click}
      onContextMenu=${(event) => { if (press.current.touch) event.preventDefault(); }}>
      <a class=${main ? "row-link list-row home-row" : "row-link list-row chat-row"} href=${chatHash(chat.name)} aria-current=${current ? "page" : null}
        aria-label=${main ? null : [`Chat ${chat.shown} of ${projects}`, news && "unread", ...shownTrays(counts).map((tray) => trayWords(tray, counts))].filter(Boolean).join(", ")}>
        <${RowText} name=${main ? project.name : chat.shown} lead=${main && !chat.goal} projects=${main ? null : projects} asks=${chat.asks > 0} counts=${counts}
          last=${row.last} now=${now} line=${line} news=${Boolean(news)} typing=${chat.busy ? chat.manager : null} />
      </a>
      ${slidable && html`<button class="row-more" type="button" aria-label=${`Folder of chat ${chat.shown}`} aria-haspopup="dialog" onClick=${() => onFolder(row)}><span aria-hidden="true">⋯</span></button>`}
      ${slidable && html`<button class="row-remove" data-quiet type="button" onClick=${() => onDismiss(row)}
        aria-label=${`Dismiss chat ${chat.shown}`} title="Dismiss"><${Icon} name="fail" /></button>`}
    </li>`;
}

// A folder's heading: a fold button. A folded folder shows a dot while one of its chats has an unread
// message or something for the owner (a need or a To check item), as the For you row counts them.
function FolderHead({ id, name, folded, dot, onFold }) {
  return html`
    <li class="row folder-head" data-key=${`folder/${id}`}>
      <button class="folder-toggle" type="button" aria-expanded=${!folded} onClick=${() => onFold(id)}>
        <${Icon} name="chevron" /><span class="folder-name">${name}</span>
        ${folded && dot ? html`<span class="news-mark" role="img" aria-label="Unread or waiting for you"></span>` : null}
      </button>
    </li>`;
}

// ^ Where a chat goes: one of the folders in use, a new one, or none. A folder is only its chats'
// `folder`: a new name makes it, and the last chat to leave removes it (wsx-core `chats_home`).
function FolderSheet({ row, folders, onPick, onClose }) {
  const dialog = useRef();
  const [name, setName] = useState("");
  // ^ The sheet takes the focus itself, not its first option: a ring shows only once Tab moves into the list.
  useEffect(() => { dialog.current.showModal(); dialog.current.focus(); }, []);
  const pick = (folder) => { dialog.current.close(); onPick(row, folder); };
  // ^ A click outside the sheet closes it with no change, as Escape does (tasks.js `backdrop`); the
  // sheet's own padding is the dialog element too, so the click's place decides. Only a press that
  // started out there counts: the click that ends the long press which opened the sheet lands on
  // the backdrop too, and must leave it open.
  const pressedOut = useRef(false);
  const outside = (event) => {
    if (event.target !== dialog.current) return false;
    const box = dialog.current.getBoundingClientRect();
    return event.clientX < box.left || event.clientX > box.right || event.clientY < box.top || event.clientY > box.bottom;
  };
  const backdrop = (event) => {
    if (pressedOut.current && outside(event)) dialog.current.close();
    pressedOut.current = false;
  };
  const current = row.chat.folder ?? null;
  const option = (folder, text) => html`
    <li key=${folder ?? ""}><button class="folder-option" type="button" role="menuitemradio" aria-checked=${current === folder} onClick=${() => pick(folder)}>
      <span class="folder-tick">${current === folder ? html`<${Icon} name="pass" />` : null}</span>${text}</button></li>`;
  return html`
    <dialog class="sheet" tabindex="-1" ref=${dialog} aria-labelledby="folder-title" onClose=${onClose} onPointerDown=${(event) => { pressedOut.current = outside(event); }} onClick=${backdrop}>
      <h2 class="sheet-title" id="folder-title">${row.main ? row.project.name : row.chat.shown}</h2>
      <ul class="folder-options" role="menu" aria-label="Folder">
        ${folders.map((folder) => option(folder, folder))}
        ${option(null, "No folder")}
      </ul>
      <form class="folder-new" onSubmit=${(event) => { event.preventDefault(); if (name.trim()) pick(name.trim()); }}>
        <input class="input" type="text" value=${name} placeholder="New folder" maxlength="24" aria-label="New folder"
          onInput=${(event) => setName(event.target.value)} autocomplete="off" enterkeyhint="done" />
        <button class="button button-primary" type="submit" disabled=${!name.trim()}>Add</button>
      </form>
    </dialog>`;
}

// The row on top of Home: everything that waits for the owner in every chat, as one count and the
// words of its kinds; it opens the For you list (foryou.js). Not shown at 0 or before the list is read.
function ForYouRow({ chats }) {
  const { total, words } = forYouWords(chats);
  if (!total) return null;
  const counts = trayCounts({ counts: { running: 0, open: 0, needs: total, check: 0 } });
  return html`
    <li class="row" data-key="for-you">
      <a class="row-link list-row chat-row" href="#/for-you" aria-label=${`For you, ${words}`}>
        <span class="row-main">
          <span class="row-head"><span class="row-work"><span class="row-label">For you</span></span><${TrayIcons} counts=${counts} /></span>
          <span class="row-context">${words}</span>
        </span>
      </a>
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
function NewChat({ projects, tasks, project, manage, added, onAdd, onClose }) {
  const dialog = useRef();
  const field = useRef();
  const [chosen, setChosen] = useState(project);
  // ^ Back from Add project: the new project is chosen once the workspace lists it.
  const fresh = added && projects.find((each) => each.name === added);
  useEffect(() => { if (fresh) setChosen(fresh.id); }, [fresh?.id]);
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
          ${fresh && chosen === fresh.id && html`<span class="field-note">Added just now</span>`}
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
  const forYou = useForYou();
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
  // ^ The project the open screen belongs to, and its chat when that is not the main chat.
  const route = parseRoute(location.hash);
  const at = route.screen === "chat" ? route : location.hash.startsWith("#/p/") ? route : null;
  const open = at?.id ?? null;
  const inChat = at?.room ?? null;
  const list = useRef();
  useFlip(list);
  const [sheet, setSheet] = useState(null);
  // ^ The project Add project made last, which New chat chooses when it comes back.
  const [added, setAdded] = useState(null);
  // ^ The folders folded on this device (name -> 1), and the row whose folder sheet is open.
  const [folded, setFolded] = useState(() => readMap(FOLDED_KEY));
  const [moving, setMoving] = useState(null);
  const fold = (name) => { keepIn(FOLDED_KEY, name, folded[name] ? "" : "1"); setFolded(readMap(FOLDED_KEY)); };
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
  // ^ The folder is the owner's alone: the row's own chat takes it (`POST .../folder`), null is "No folder".
  const moveTo = async ({ project, chat }, folder) => {
    try {
      await api(`api/tasks/${project.id}/chats/${encodeURIComponent(chat.name)}/folder`, { method: "POST", json: { folder } });
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
    if (rows.length || forYou?.length) {
      const waiting = (row) => row.news || row.rank !== null || row.chat.counts?.check > 0;
      const chatRow = (row) => html`<${ChatRow} key=${`${row.project.id}/${row.chat.name}`} row=${row} now=${now} onDismiss=${dismiss} onFolder=${setMoving}
        current=${rail && open === row.project.id && (inChat ?? row.overview.main_chat) === row.chat.name} />`;
      const sections = folderSections(rows);
      // ^ The chats of no folder get a heading of their own once a folder exists, folded like any folder (its id is "").
      const headed = sections.some((section) => section.name);
      body = html`
        <ul class="rows home-rows list" aria-label="Chats" ref=${list}>
          ${forYou && html`<${ForYouRow} chats=${forYou} />`}
          ${sections.map(({ name, rows: inside }) => (headed
            ? [html`<${FolderHead} key=${`folder/${name ?? ""}`} id=${name ?? ""} name=${name ?? "No folder"} folded=${Boolean(folded[name ?? ""])} dot=${inside.some(waiting)} onFold=${fold} />`,
              ...(folded[name ?? ""] ? [] : inside.map(chatRow))]
            : inside.map(chatRow)))}
        </ul>
        ${moving && html`<${FolderSheet} row=${moving} folders=${sections.filter((section) => section.name).map((section) => section.name)} onPick=${moveTo} onClose=${() => setMoving(null)} />`}`;
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
      ${(sheet === "chat" || sheet === "project-for-chat") && html`<${NewChat} projects=${store.projects} tasks=${store.tasks} project=${store.projects.some((each) => each.id === open) ? open : store.projects[0].id}
        manage=${manage} added=${added} onAdd=${() => setSheet("project-for-chat")} onClose=${() => { setSheet(null); setAdded(null); }} />`}
      ${/* ^ From New chat, Add project opens over it: the typed name stays. */ ""}
      ${sheet?.startsWith("project") && html`<${AddProject} onAdded=${setAdded}
        onClose=${() => setSheet(sheet === "project-for-chat" ? "chat" : null)} />`}
      <div class="scroll">
        ${body}
      </div>
    </div>`;
}
