// Projects: every project this site may reach. A project opens its worktrees and
// their sessions (with task_view too, not its chat); Files and New session stay on each row.
import { html, Icon, Loading, plural, useEffect, useNow, useRef, useState } from "./lib.js";
import { api } from "./api.js";
import { can, listedSessions, shows, stateOf, useStore } from "./store.js";
import { NewSession, SessionRow } from "./inbox.js";
import { Back } from "./ui.js";
import { slide } from "./swipe.js";
import { PanelsButton } from "./panels.js";
import { Suggestions, useListKeys } from "./chat.js";

const counted = (count) => plural(count, "session");
// How many task items wait for the owner in a project, after its session count.
const waiting = (store, project) => {
  const count = can("task_view") ? store.tasks[project.id]?.needs.length ?? 0 : 0;
  return count ? ` · ${count} ${count === 1 ? "needs" : "need"} you` : "";
};
const filesLink = (project) => shows("files") && can("files_read") && html`
  <a class="link-action" href=${`#/p/${project.id}/files`} aria-label=${`Files of ${project.name}`}>Files</a>`;

// ^ Add project (docs/web-client.md "Projects"): one field that lists what the TUI's `p` lists, in the
// composer's list (`Suggestions`): the git repositories wsx found under the host's home, newest
// first, or, once the text starts with / or ~, the folders of that path. Tab, Enter or a tap puts
// a row in the field and lists its folders. When the field names a folder, a line under it says
// what it is and where it will show, or why Add is off. The server's refusal stays in the sheet;
// the row comes with the next workspace event. `onAdded(name)` hears of the new project.
export function AddProject({ onClose, onAdded }) {
  const dialog = useRef();
  const field = useRef();
  const [text, setText] = useState("");
  // ^ `q` is the text the answer is for: a folder named by an older text is never added.
  const [found, setFound] = useState({ rows: [], more: 0, scanning: true, folder: null, q: null });
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const { active, setActive, shut, keys } = useListKeys(text);
  useEffect(() => {
    dialog.current.showModal();
    field.current.focus();
  }, []);
  // ^ Read on each change of the text, the newest answer wins; while the scan runs, again each second.
  useEffect(() => {
    let live = true;
    const read = (first) => api(`api/projects/suggest?q=${encodeURIComponent(text)}`).then((answer) => {
      if (!live) return;
      setFound({ ...answer, q: text });
      setError("");
      if (first) setActive(0);
      if (answer.scanning && !text) timer = setTimeout(read, 1000, false);
    }, (failure) => { if (live) setError(failure.message); });
    let timer = setTimeout(read, text ? 120 : 0, true);
    return () => { live = false; clearTimeout(timer); };
  }, [text]);
  const options = found.rows.map((row) => ({ key: row.path, name: row.name, description: row.path,
    source: row.added ? "added" : row.git ? "git" : "folder", tone: row.added ? "faint" : null, row }));
  const pick = (option) => {
    setText(option.row.path);
    field.current.focus();
  };
  const folder = found.q === text ? found.folder : null;
  const ready = folder?.git && !folder.added;
  const submit = async (event) => {
    event.preventDefault();
    if (!ready) return;
    setBusy(true);
    setError("");
    try {
      const answer = await api("api/projects", { method: "POST", json: { path: folder.path } });
      onAdded?.(answer.added);
      dialog.current.close();
    } catch (failure) {
      setError(failure.message);
      setBusy(false);
      field.current.focus();
    }
  };
  // ^ Enter adds a folder that is ready; otherwise it picks the active row, as Tab does.
  const listed = !shut && options.length > 0;
  const facts = folder && [folder.git ? "git" : "folder", folder.branch, folder.git && plural(folder.worktrees, "worktree")].filter(Boolean).join(" · ");
  return html`
    <dialog class="sheet add-project" ref=${dialog} aria-labelledby="add-project-title" onClose=${onClose}>
      <form onSubmit=${submit}>
        <h2 class="sheet-title" id="add-project-title">Add project</h2>
        <label class="field">Name or path
          <input class="input" type="text" name="path" ref=${field} value=${text} placeholder="Name or ~/path" autocomplete="off"
            role="combobox" aria-expanded=${listed} aria-controls="slash-list" aria-activedescendant=${listed ? `slash-${active}` : null}
            autocapitalize="none" autocorrect="off" spellcheck=${false} enterkeyhint="go"
            onInput=${(event) => setText(event.target.value)}
            onKeyDown=${(event) => { if (listed && !(event.key === "Enter" && ready)) keys(event, options, pick); }} />
        </label>
        ${listed && html`<${Suggestions} inline label="Repositories and folders" options=${options} active=${active} onHover=${setActive} onPick=${pick}
          notes=${[found.more > 0 && `${found.more} more: type a name, or a path from / or ~`, found.scanning && !text && "Still looking under your home…"]} />`}
        ${!listed && !folder && text && html`<p class="note">${found.scanning && !/^[/~]/.test(text) ? "Still looking under your home…" : "Nothing matches. Type a path from / or ~."}</p>`}
        ${folder && html`
          <div class="folder-card" data-tone=${ready ? null : "danger"} role="status">
            ${ready ? html`
              <p class="folder-what"><${Icon} name="pass" /><strong>${folder.name}</strong><span class="muted">${facts}</span></p>
              <p class="muted">Shows in Sessions with its worktrees, and on Home as its lead chat.</p>`
            : html`<p class="folder-what"><${Icon} name="fail" />${folder.added ? `${folder.path} is already the project ${folder.added}.`
                : `${folder.path} is not a git repository: pick one, or run git init there.`}</p>`}
          </div>`}
        <p class="error" role="alert">${error}</p>
        <div class="actions sheet-actions">
          <button class="button" type="button" onClick=${() => dialog.current.close()}>Cancel</button>
          <button class="button button-primary" type="submit" disabled=${busy || !ready}>Add</button>
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

export function Projects({ rail }) {
  const store = useStore();
  const count = (project) => listedSessions(store.sessions).filter((session) => session.project.id === project.id).length;
  const [, , open] = location.hash.split("/");
  // ^ Where the site manages projects, the + adds one and a slide to the left removes one: the projects are no Home rows.
  const manage = store.site.manage_projects;
  const [sheet, setSheet] = useState(null);
  const [removing, setRemoving] = useState(null);
  // ^ The project just added: its row is marked (the found tint) when the workspace lists it.
  const [fresh, setFresh] = useState(null);
  useEffect(() => {
    if (!fresh || !store.projects.some((project) => project.name === fresh)) return undefined;
    const timer = setTimeout(() => setFresh(null), 2500);
    return () => clearTimeout(timer);
  }, [fresh, store.projects.some((project) => project.name === fresh)]);
  let body;
  if (!store.daemon) body = html`<p class="empty">wsx is not running on the host.</p>`;
  else if (!store.loaded) body = html`<${Loading} />`;
  else if (!store.projects.length) body = html`<p class="empty">wsx manages no project that this site may reach.</p>`;
  else {
    body = html`
      <ul class="rows">
        ${store.projects.map((project) => html`
          <li class=${manage ? "row row-slide" : "row"} ...${manage ? slide : null} key=${project.id} data-found=${project.name === fresh ? "" : null}>
            <div class="row-link" aria-current=${rail && Number(open) === project.id ? "page" : null}>
              <a class="row-main row-open" href=${can("task_view") ? `#/p/${project.id}/sessions` : `#/p/${project.id}`}>
                <span class="row-label">${project.name}</span>
                <span class="row-context">${counted(count(project))}${waiting(store, project)}</span>
              </a>
              <span class="row-actions">
                ${filesLink(project)}
                <${NewSession} project=${project} />
              </span>
            </div>
            ${manage && html`<button class="row-remove" type="button" onClick=${() => setRemoving(project)}
              aria-label=${`Remove project ${project.name}`} title="Remove"><${Icon} name="fail" /></button>`}
          </li>`)}
      </ul>`;
  }
  return html`
    <div class="page">
      ${(!rail || manage) && html`
        <div class="home-top">
          ${!rail && html`<h1 class="title">Projects</h1>`}
          ${manage && html`<button class="icon-button new-chat" type="button" onClick=${() => setSheet("project")} aria-label="Add project" title="Add project"><${Icon} name="plus" /></button>`}
        </div>`}
      ${sheet === "project" && html`<${AddProject} onAdded=${setFresh} onClose=${() => setSheet(null)} />`}
      ${removing && html`<${RemoveProject} project=${removing} onClose=${() => setRemoving(null)} />`}
      <div class="scroll">${body}</div>
    </div>`;
}

// A project's sessions, from the manager's top row (#/p/<id>/sessions).
export const ProjectSessions = ({ id }) => html`<${Project} id=${id} back=${`#/p/${id}`} />`;

// ^ The confirmation of a worktree's delete (docs/tasks.md "Stale worktrees"): it asks the server
// what the delete closes and loses, and names it. A worktree with nothing to lose is deleted
// by the plain button; one with uncommitted work or commits neither merged nor pushed needs
// the button that names the loss. A failure stays in the sheet.
function DeleteWorktree({ worktree, onClose }) {
  const dialog = useRef();
  const [facts, setFacts] = useState(null);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  useEffect(() => {
    dialog.current.showModal();
    api(`api/worktrees/${worktree.id}`).then(setFacts, (failure) => setError(failure.message));
  }, []);
  const lost = facts?.lost;
  const submit = async (event) => {
    event.preventDefault();
    setBusy(true);
    setError("");
    try {
      await api(`api/worktrees/${worktree.id}${lost ? "?confirm=true" : ""}`, { method: "DELETE" });
      dialog.current.close();
    } catch (failure) {
      setError(failure.message);
      setBusy(false);
    }
  };
  return html`
    <dialog class="sheet" ref=${dialog} aria-labelledby="delete-worktree-title" onClose=${onClose}>
      <form onSubmit=${submit}>
        <h2 class="sheet-title" id="delete-worktree-title">Delete ${worktree.branch || "detached"}?</h2>
        ${facts && html`<p class="muted">${counted(facts.sessions)} close. ${lost ? html`<strong>It loses ${lost}.</strong>` : "Nothing is lost: it is clean, and merged or pushed."}</p>`}
        <p class="error" role="alert">${error}</p>
        <div class="actions sheet-actions">
          <button class="button" type="button" onClick=${() => dialog.current.close()}>Cancel</button>
          <button class="button button-danger" type="submit" disabled=${busy || !facts}>${lost ? "Delete and lose it" : "Delete"}</button>
        </div>
      </form>
    </dialog>`;
}

// One project: its worktrees, main checkout first, each with its sessions.
export function Project({ id, back = "#/projects" }) {
  const store = useStore();
  const project = store.projects.find((candidate) => candidate.id === id);
  const sessions = project ? listedSessions(store.sessions).filter((session) => session.project.id === id) : [];
  const now = useNow(sessions.some((session) => stateOf(session).key === "working"));
  const [deleting, setDeleting] = useState(null);
  if (!project) {
    return html`
      <div class="page">
        <header class="bar"><${Back} fallback=${back} /></header>
        ${store.daemon && !store.loaded ? html`<${Loading} />` : html`<p class="empty">${!store.daemon ? "wsx is not running on the host." : "This project is not available."}</p>`}
      </div>`;
  }
  const worktrees = [...project.worktrees].sort((left, right) => Number(right.main) - Number(left.main));
  const deletable = can("session_start") && can("files_write");
  return html`
    <div class="page">
      <header class="bar">
        <${Back} fallback=${back} />
        <div class="bar-title">
          <span class="bar-label">${project.name}</span>
          <span class="bar-state">${counted(sessions.length)}</span>
        </div>
        <div class="bar-tools">${filesLink(project)}<${PanelsButton} project=${id} /></div>
      </header>
      <div class="scroll">
        ${worktrees.map((worktree) => {
          const own = sessions.filter((session) => session.worktree === worktree.id);
          return html`
            <section class="group worktree" key=${worktree.id} aria-label=${worktree.branch || "detached"}>
              <div class="group-head">
                <h2 class="group-name"><span class="group-branch">${worktree.branch || "detached"}</span>${worktree.main ? " · main checkout" : null}</h2>
                ${deletable && !worktree.main && html`<button class="link-action delete-worktree" type="button" aria-label=${`Delete worktree ${worktree.branch || "detached"}`}
                  onClick=${() => setDeleting(worktree)}>Delete</button>`}
                <${NewSession} project=${project} worktree=${worktree} />
              </div>
              ${own.length
                ? html`<ul class="rows">${own.map((session) => html`
                    <${SessionRow} key=${session.id} session=${session} now=${now} place="worktree" />`)}</ul>`
                : html`<p class="note">No session.</p>`}
            </section>`;
        })}
      </div>
      ${deleting && html`<${DeleteWorktree} worktree=${deleting} onClose=${() => setDeleting(null)} />`}
    </div>`;
}
